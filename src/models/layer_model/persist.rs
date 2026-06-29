/*
File: models/layer_model/persist.rs

Purpose:
Disk save/load for the layer model. Phase 1 persists the PS editor's normal (raster) layers only:
each layer's pre-effects base pixels go to a PNG in the layers dir, and the per-page tree is
recorded in `layers.json` (see `manifest.rs`). Text / group / effects nodes are part of the schema
but not produced yet; the loader skips any node it cannot yet materialize so a future writer cannot
break an older reader.

Staging:
Saves are written to the chapter's `*_unsaved/layers/` dir; the existing project-merge step copies
it into the main `layers/` on "save to project". Loads read the unsaved dir first and fall back to
the main dir (both for the manifest and for each PNG), mirroring how `text_images/` is loaded.
*/

use super::manifest::{
    DeformRec, GroupRec, LayerKindRec, LayerRec, LayersManifest, PageLayers, PayloadRef,
    TextGroupRec, TransformRec,
};
use crate::trace::cat;
use eframe::egui::ColorImage;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MANIFEST_FILE: &str = "layers.json";

/// Logical store name used in a text node's `payload_ref`: the overlay payload (render params +
/// geometry) lives in `text_info.json`, in the same `layers/` folder.
pub const TEXT_PAYLOAD_STORE: &str = "text_info";

/// Serializes every `layers.json` read-modify-write. The manifest is shared by two independent
/// subsystems — the PS editor writes raster nodes, the typing tab writes text nodes — and the
/// typing save runs on a worker thread, so concurrent RMW must not interleave.
static MANIFEST_LOCK: Mutex<()> = Mutex::new(());

/// A raster layer handed in for saving (pixels borrowed from the live stack).
pub struct RasterLayerOut<'a> {
    pub uid: String,
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    pub transform: TransformRec,
    /// Optional mesh-deform grid (cols×rows control points, absolute page px). When present the
    /// layer is positioned by this mesh rather than its affine `transform` (mirrors text deform).
    pub deform: Option<DeformRec>,
    /// Stable uid of the group this layer belongs to, if any.
    pub group_uid: Option<String>,
    /// The display image. Written as the base PNG only when `pixels_dirty` (the layer's *base* pixels
    /// actually changed); otherwise the on-disk base PNG, `rendered_file` and `effects` chain are
    /// preserved from the manifest so a non-destructive effects chain set by another tab survives a
    /// whole-page save.
    pub image: &'a ColorImage,
    /// True when this layer's *base* pixels were edited (paint/cut/merge/effects-bake), so the base
    /// PNG must be rewritten and any non-destructive effects chain dropped (baked in).
    pub pixels_dirty: bool,
    /// Optional mask-clip flag (typing tab): whether the raster is clipped to the page mask. `None`/
    /// `Some(false)` ⇒ no clip (rasters default OFF). Round-trips through `LayerRec.mask_clip`.
    pub mask_clip: Option<bool>,
}

/// A raster layer decoded back from disk.
pub struct RasterLayerIn {
    pub uid: String,
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    pub transform: TransformRec,
    /// Optional mesh-deform grid decoded from disk (absolute page px). `None` for affine rasters.
    pub deform: Option<DeformRec>,
    pub group_uid: Option<String>,
    /// Optional mask-clip flag decoded from disk. `None`/`Some(false)` ⇒ no clip (rasters default OFF).
    pub mask_clip: Option<bool>,
    /// The display image: the post-effects render (`rendered_file`) when effects are present,
    /// otherwise the base PNG.
    pub image: ColorImage,
    /// The pre-effects (base) pixels, decoded from `base_file`. Equals `image` when no effects are
    /// present; with effects it is the original so the chain stays reversible. Falls back to a clone
    /// of `image` if the base PNG cannot be decoded.
    pub base_image: ColorImage,
    /// The base (pre-effects) PNG name, so callers can re-render the effects chain from the original.
    pub base_file: String,
    /// Post-effects chain (non-destructive). Empty = no effects.
    pub effects: Vec<serde_json::Value>,
}

/// A layer group's metadata (single-level groups; `parent_uid` is reserved for future nesting).
#[derive(Clone)]
pub struct GroupMeta {
    pub uid: String,
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    /// Panel-only collapse state (children hidden in the PS layers panel).
    pub collapsed: bool,
}

/// Everything decoded for a page: its groups plus its raster layers (bottom-to-top).
pub struct PageRasters {
    pub groups: Vec<GroupMeta>,
    pub layers: Vec<RasterLayerIn>,
}

/// Per-page base-PNG filename: `ps_p{page:04}_{uid}.png`.
fn base_file_name(page_idx: usize, uid: &str) -> String {
    format!("ps_p{page_idx:04}_{uid}.png")
}

/// Per-page post-effects (rendered) PNG filename for a raster: `ps_p{page:04}_{uid}_fx.png`.
#[must_use]
pub fn rendered_file_name(page_idx: usize, uid: &str) -> String {
    format!("ps_p{page_idx:04}_{uid}_fx.png")
}

/// Prefix shared by every base PNG of a page, used to detect orphans to prune.
fn page_file_prefix(page_idx: usize) -> String {
    format!("ps_p{page_idx:04}_")
}

/// Per-page rendered-text PNG filename for a TEXT node: `ps_p{page:04}_{uid}_text.png`. Keyed by uid
/// so a text re-render overwrites in place; distinct from the raster `_fx` suffix.
#[must_use]
pub fn text_image_file_name(page_idx: usize, uid: &str) -> String {
    format!("ps_p{page_idx:04}_{uid}_text.png")
}

/// Writes (or clears) the persisted raster layers for one page in `layers_dir`.
///
/// The page entry in `layers.json` is rewritten from `layers` (ordered bottom-to-top). The caller
/// (the PS editor) is authoritative only over the rasters it actually loaded into its stack: a
/// raster node already in the manifest whose uid is **not** in `layers` is preserved verbatim
/// (effects + PNGs intact) — it belongs to another tab (e.g. the typing tab added/effected it while
/// the PS stack was stale or had never loaded it). Only uids listed in `removed_uids` (rasters the
/// PS editor explicitly deleted/merged away this session) are dropped from the manifest and pruned.
/// Any base PNG for this page that is no longer referenced is deleted. When nothing is written and
/// the page had nothing persisted, this is a no-op (it does not create an empty manifest or directory).
pub fn save_page_rasters(
    layers_dir: &Path,
    page_idx: usize,
    layers: &[RasterLayerOut],
    groups: &[GroupMeta],
    removed_uids: &[String],
) -> Result<(), String> {
    let _span = crate::trace_scope!(
        cat::PERSIST,
        "save_page_rasters page={} layers={} groups={} removed={}",
        page_idx,
        layers.len(),
        groups.len(),
        removed_uids.len()
    );
    let manifest_path = layers_dir.join(MANIFEST_FILE);
    let _guard = MANIFEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut manifest = read_manifest(&manifest_path)?.unwrap_or_else(LayersManifest::empty);

    // Text nodes and their group bands (written by the typing tab) must survive a raster rewrite.
    let preserved: Vec<LayerRec> = manifest
        .page(page_idx)
        .map(|p| {
            p.tree
                .iter()
                .filter(|r| r.kind != LayerKindRec::Raster)
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    // Raster nodes the caller does not own — present in the manifest but absent from `layers` and
    // not explicitly removed — belong to another tab (it added/effected them while this caller's
    // stack was stale). Preserve them verbatim so a whole-page raster save never clobbers another
    // tab's rasters or their non-destructive effects.
    let out_uids: HashSet<&str> = layers.iter().map(|l| l.uid.as_str()).collect();
    let removed_set: HashSet<&str> = removed_uids.iter().map(|s| s.as_str()).collect();
    let preserved_rasters: Vec<LayerRec> = manifest
        .page(page_idx)
        .map(|p| {
            p.tree
                .iter()
                .filter(|r| {
                    r.kind == LayerKindRec::Raster
                        && !out_uids.contains(r.uid.as_str())
                        && !removed_set.contains(r.uid.as_str())
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let preserved_text_groups = manifest
        .page(page_idx)
        .map(|p| p.text_groups.clone())
        .unwrap_or_default();
    // Raster band Z is stable: an existing layer keeps its Z (set by content saves or PS band
    // reordering); a brand-new layer is placed on top of every current band.
    let existing_raster_z: HashMap<String, u32> = manifest
        .page(page_idx)
        .map(|p| {
            p.tree
                .iter()
                .filter(|r| r.kind == LayerKindRec::Raster)
                .map(|r| (r.uid.clone(), r.z))
                .collect()
        })
        .unwrap_or_default();
    // Non-destructive effects: when a raster's base pixels were NOT edited, preserve its on-disk
    // base PNG + rendered PNG + effects chain + base `image_size` (another tab may own the effects).
    type ExistingRaster = (Option<String>, Option<String>, Vec<serde_json::Value>, Option<[usize; 2]>);
    let existing_rasters: HashMap<String, ExistingRaster> = manifest
        .page(page_idx)
        .map(|p| {
            p.tree
                .iter()
                .filter(|r| r.kind == LayerKindRec::Raster)
                .map(|r| {
                    (
                        r.uid.clone(),
                        (
                            r.base_file.clone(),
                            r.rendered_file.clone(),
                            r.effects.clone(),
                            r.image_size,
                        ),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    // Mask-clip: a writer that does not own the flag (e.g. the PS editor, whose `Layer` has no
    // mask-clip) passes `None`; preserve the existing on-disk value in that case so a typing-set clip
    // is never clobbered by a PS whole-page save. `Some(_)` from the writer (the doc flush) wins.
    let existing_mask_clip: HashMap<String, Option<bool>> = manifest
        .page(page_idx)
        .map(|p| {
            p.tree
                .iter()
                .filter(|r| r.kind == LayerKindRec::Raster)
                .map(|r| (r.uid.clone(), r.mask_clip))
                .collect()
        })
        .unwrap_or_default();
    let mut next_top_z = manifest
        .page(page_idx)
        .map(|p| {
            p.tree
                .iter()
                .map(|r| r.z)
                .chain(p.text_groups.iter().map(|g| g.z))
                .max()
                .map_or(0, |m| m + 1)
        })
        .unwrap_or(0);

    // Nothing to write and nothing recorded before: avoid creating empty artifacts on bare page
    // visits.
    if layers.is_empty()
        && groups.is_empty()
        && manifest.page(page_idx).is_none()
        && !layers_dir.exists()
    {
        return Ok(());
    }

    fs::create_dir_all(layers_dir)
        .map_err(|e| format!("create {}: {e}", layers_dir.display()))?;

    let mut keep: Vec<String> = Vec::with_capacity(layers.len());
    let mut recs: Vec<LayerRec> = preserved;
    // Keep the PNGs of preserved (other-tab) rasters alive through pruning, then carry their nodes.
    for r in &preserved_rasters {
        if let Some(b) = &r.base_file {
            keep.push(b.clone());
        }
        if let Some(rf) = &r.rendered_file {
            keep.push(rf.clone());
        }
    }
    recs.extend(preserved_rasters);
    for layer in layers.iter() {
        let z = existing_raster_z.get(&layer.uid).copied().unwrap_or_else(|| {
            let z = next_top_z;
            next_top_z += 1;
            z
        });
        let existing = existing_rasters.get(&layer.uid);
        // Preserve the original base PNG + effects when the base pixels were not edited, so a
        // non-destructive effects chain (set by the typing tab) survives a PS whole-page save.
        let (base_file, rendered_file, effects, image_size) = match existing {
            Some((Some(base), rendered, eff, size)) if !layer.pixels_dirty => {
                keep.push(base.clone());
                if let Some(r) = rendered {
                    keep.push(r.clone());
                }
                (
                    base.clone(),
                    rendered.clone(),
                    eff.clone(),
                    size.unwrap_or(layer.image.size),
                )
            }
            _ => {
                // Dirty or new: (re)write the base PNG from the (display==base) image; effects bake in.
                let file = base_file_name(page_idx, &layer.uid);
                write_png(&layers_dir.join(&file), layer.image)?;
                keep.push(file.clone());
                (file, None, Vec::new(), layer.image.size)
            }
        };
        recs.push(LayerRec {
            uid: layer.uid.clone(),
            name: layer.name.clone(),
            kind: LayerKindRec::Raster,
            z,
            layer_idx: None,
            pinned: false,
            pinned_by_group: false,
            group_uid: layer.group_uid.clone(),
            visible: layer.visible,
            opacity: layer.opacity,
            transform: Some(layer.transform),
            deform: layer.deform.clone(),
            base_file: Some(base_file),
            rendered_file,
            image_size: Some(image_size),
            effects,
            payload_ref: None,
            render_data: None,
            // The owning writer's value wins; a non-owning writer (PS, `None`) preserves the on-disk one.
            mask_clip: layer
                .mask_clip
                .or_else(|| existing_mask_clip.get(&layer.uid).copied().flatten()),
        });
    }

    prune_orphan_pngs(layers_dir, page_idx, &keep);

    let group_recs: Vec<GroupRec> = groups
        .iter()
        .map(|g| GroupRec {
            uid: g.uid.clone(),
            name: g.name.clone(),
            visible: g.visible,
            opacity: g.opacity,
            collapsed: g.collapsed,
            parent_uid: None,
        })
        .collect();

    if recs.is_empty() && group_recs.is_empty() {
        manifest.remove_page(page_idx);
        crate::trace_log!(
            cat::PERSIST,
            "save_page_rasters page={} removed empty page from manifest",
            page_idx
        );
    } else {
        crate::trace_log!(
            cat::PERSIST,
            "save_page_rasters page={} recs={} groups={} pngs_kept={}",
            page_idx,
            recs.len(),
            group_recs.len(),
            keep.len()
        );
        manifest.upsert_page(PageLayers {
            img_idx: page_idx,
            groups: group_recs,
            text_groups: preserved_text_groups,
            tree: recs,
        });
    }
    crate::trace_log!(
        cat::PERSIST,
        "save_page_rasters writing manifest {}",
        manifest_path.display()
    );
    write_manifest(&manifest_path, &manifest)
}

/// Loads the persisted raster layers for one page, ordered bottom-to-top.
///
/// `primary_dir` (the unsaved staging dir) is consulted first for both the manifest and each PNG,
/// falling back to `fallback_dir` (the main layers dir). Nodes that are not yet materializable
/// (text / group) or whose PNG is missing are skipped with a warning. Missing dirs yield an empty
/// list rather than an error.
pub fn load_page_rasters(
    primary_dir: &Path,
    fallback_dir: Option<&Path>,
    page_idx: usize,
) -> Result<PageRasters, String> {
    let _span = crate::trace_scope!(cat::PERSIST, "load_page_rasters page={}", page_idx);
    let empty = || PageRasters {
        groups: Vec::new(),
        layers: Vec::new(),
    };
    // PER-PAGE fallback: a committed-only page (absent from the staging manifest) must still load its
    // committed rasters/groups (the PNGs are then resolved from either dir below).
    let Some(page) = read_page_with_fallback(primary_dir, fallback_dir, page_idx)? else {
        crate::trace_log!(
            cat::PERSIST,
            "load_page_rasters page={} -> no manifest page (empty)",
            page_idx
        );
        return Ok(empty());
    };

    let groups: Vec<GroupMeta> = page
        .groups
        .iter()
        .map(|g| GroupMeta {
            uid: g.uid.clone(),
            name: g.name.clone(),
            visible: g.visible,
            opacity: g.opacity,
            collapsed: g.collapsed,
        })
        .collect();

    let mut nodes: Vec<&LayerRec> = page.tree.iter().collect();
    nodes.sort_by_key(|rec| rec.z);

    let mut out = Vec::new();
    for rec in nodes {
        if rec.kind != LayerKindRec::Raster {
            // Text / group nodes are not materializable into a raster layer in this phase.
            continue;
        }
        let Some(base_file) = rec.base_file.clone() else {
            continue;
        };
        let Some(transform) = rec.transform else {
            crate::runtime_log::log_warn(format!(
                "[layers] raster {base_file} for page {page_idx} has no transform, skipping"
            ));
            continue;
        };
        // Non-destructive effects: display the rendered (post-effects) PNG when present, else the
        // base. Only the base's size is recorded in `image_size`, so the size check applies only to
        // the base load.
        let has_effects = !rec.effects.is_empty();
        let display_file = match (has_effects, rec.rendered_file.as_deref()) {
            (true, Some(rendered)) => rendered,
            _ => base_file.as_str(),
        };
        let image = match read_png_with_fallback(primary_dir, fallback_dir, display_file) {
            Some(img) => img,
            None => {
                crate::runtime_log::log_warn(format!(
                    "[layers] missing image {display_file} for page {page_idx}, skipping layer"
                ));
                continue;
            }
        };
        if display_file == base_file.as_str()
            && let Some(expected) = rec.image_size
            && image.size != expected
        {
            crate::runtime_log::log_warn(format!(
                "[layers] {base_file}: size {:?} != recorded {:?}, skipping",
                image.size, expected
            ));
            continue;
        }
        // The pre-effects base pixels (so effects stay reversible). Reuse `image` when it already is
        // the base; otherwise decode the base PNG, falling back to the display image on failure.
        let base_image = if display_file == base_file.as_str() {
            image.clone()
        } else {
            read_png_with_fallback(primary_dir, fallback_dir, base_file.as_str())
                .unwrap_or_else(|| image.clone())
        };
        out.push(RasterLayerIn {
            uid: rec.uid.clone(),
            name: rec.name.clone(),
            visible: rec.visible,
            opacity: rec.opacity,
            transform,
            deform: rec.deform.clone(),
            group_uid: rec.group_uid.clone(),
            mask_clip: rec.mask_clip,
            image,
            base_image,
            base_file,
            effects: rec.effects.clone(),
        });
    }
    crate::trace_log!(
        cat::PERSIST,
        "load_page_rasters page={} -> rasters={} groups={}",
        page_idx,
        out.len(),
        groups.len()
    );
    Ok(PageRasters {
        groups,
        layers: out,
    })
}

/// Appends a single new raster node to `page_idx` on top of the current band stack and writes its
/// PNG (`ps_p{page:04}_{uid}.png`), preserving every other node / group / text-group. Returns the
/// PNG file name. Used by tabs (e.g. the typing tab adding an external image as a raster) that need
/// to add one raster without rewriting the whole page like `save_page_rasters` does.
#[allow(clippy::too_many_arguments)]
pub fn add_page_raster(
    layers_dir: &Path,
    fallback_dir: Option<&Path>,
    page_idx: usize,
    uid: &str,
    name: &str,
    visible: bool,
    opacity: f32,
    transform: TransformRec,
    image: &ColorImage,
) -> Result<String, String> {
    let _span = crate::trace_scope!(
        cat::PERSIST,
        "add_page_raster page={} uid={} size={}x{}",
        page_idx,
        uid,
        image.size[0],
        image.size[1]
    );
    let manifest_path = layers_dir.join(MANIFEST_FILE);
    let _guard = MANIFEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut manifest = read_manifest(&manifest_path)?.unwrap_or_else(LayersManifest::empty);
    fs::create_dir_all(layers_dir)
        .map_err(|e| format!("create {}: {e}", layers_dir.display()))?;

    let file = base_file_name(page_idx, uid);
    write_png(&layers_dir.join(&file), image)?;

    // DATA-SAFETY: seed the committed page (its TEXT + rasters + groups) into the staging manifest when
    // this page is not yet staged. Creating a bare `tree: Vec::new()` page (the old behaviour) staged a
    // TEXT-LESS page on top of a typeset committed page; the doc reload then saw zero text and the
    // save-to-project merge dropped the committed text. Seeding mirrors `read_page_with_fallback`.
    ensure_page_staged(&mut manifest, page_idx, fallback_dir)?;

    // Reuse-or-create the page entry; the new raster gets a Z above every existing band.
    let page = match manifest.pages.iter_mut().find(|p| p.img_idx == page_idx) {
        Some(p) => p,
        None => {
            manifest.upsert_page(PageLayers {
                img_idx: page_idx,
                groups: Vec::new(),
                text_groups: Vec::new(),
                tree: Vec::new(),
            });
            manifest
                .pages
                .iter_mut()
                .find(|p| p.img_idx == page_idx)
                .expect("just inserted")
        }
    };
    let top_z = page
        .tree
        .iter()
        .map(|r| r.z)
        .chain(page.text_groups.iter().map(|g| g.z))
        .max()
        .map_or(0, |m| m + 1);
    page.tree.push(LayerRec {
        uid: uid.to_string(),
        name: name.to_string(),
        kind: LayerKindRec::Raster,
        z: top_z,
        layer_idx: None,
        pinned: false,
        pinned_by_group: false,
        group_uid: None,
        visible,
        opacity,
        transform: Some(transform),
        deform: None,
        base_file: Some(file.clone()),
        rendered_file: None,
        image_size: Some(image.size),
        effects: Vec::new(),
        payload_ref: None,
        render_data: None,
        mask_clip: None,
    });
    write_manifest(&manifest_path, &manifest)?;
    Ok(file)
}

/// Updates only the transform of an existing raster node (by uid) on `page_idx`. No PNG IO. A no-op
/// if the page or node is absent.
/// Ensures `page_idx` exists in `manifest`, seeding it from the `fallback_dir` manifest when absent.
/// This lets a targeted single-node edit (transform/effects) persist even for a page that currently
/// lives only in the committed (fallback) manifest — e.g. right after "save to project" cleared the
/// unsaved staging dir. Without it the edit would silently no-op (the unsaved manifest has no such
/// page yet), so a scale/effects change made in the typing tab would never reach PS or export.
///
/// Load-bearing: the DIRECT callers of `update_raster_transform` / `update_raster_effects` (typing
/// tab's drag-persist + effects-apply, PS editor's effects-apply) target a single node and bypass
/// `LayerDoc::flush_page` (which stages the whole page via `save_page_rasters`), so without this they
/// would silently no-op on a committed-only page. Kept for them.
fn ensure_page_staged(
    manifest: &mut LayersManifest,
    page_idx: usize,
    fallback_dir: Option<&Path>,
) -> Result<(), String> {
    if manifest.page(page_idx).is_some() {
        return Ok(());
    }
    let Some(fb) = fallback_dir else {
        return Ok(());
    };
    if let Some(fb_manifest) = read_manifest(&fb.join(MANIFEST_FILE))?
        && let Some(fb_page) = fb_manifest.page(page_idx)
    {
        manifest.upsert_page(fb_page.clone());
    }
    Ok(())
}

pub fn update_raster_transform(
    layers_dir: &Path,
    page_idx: usize,
    uid: &str,
    transform: TransformRec,
    fallback_dir: Option<&Path>,
) -> Result<(), String> {
    let _span = crate::trace_scope!(
        cat::PERSIST,
        "update_raster_transform page={} uid={}",
        page_idx,
        uid
    );
    let manifest_path = layers_dir.join(MANIFEST_FILE);
    let _guard = MANIFEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut manifest = read_manifest(&manifest_path)?.unwrap_or_else(LayersManifest::empty);
    ensure_page_staged(&mut manifest, page_idx, fallback_dir)?;
    let Some(page) = manifest.pages.iter_mut().find(|p| p.img_idx == page_idx) else {
        return Ok(());
    };
    if let Some(node) = page
        .tree
        .iter_mut()
        .find(|r| r.kind == LayerKindRec::Raster && r.uid == uid)
    {
        node.transform = Some(transform);
        // The unsaved staging dir may not exist yet (e.g. right after "save to project" removed it),
        // so create it before writing — otherwise the edit would be silently lost.
        fs::create_dir_all(layers_dir)
            .map_err(|e| format!("create {}: {e}", layers_dir.display()))?;
        write_manifest(&manifest_path, &manifest)?;
    }
    Ok(())
}

/// Updates a single raster node's transform AND deform mesh (no PNG IO) — used by the typing tab's
/// raster perspective transform mode, where a mesh-handle drag changes both the deform grid and the
/// affine transform (kept in sync). Mirrors [`update_raster_transform`]'s single-node, staging-aware
/// write. A no-op if the page or node is absent.
pub fn update_raster_geometry(
    layers_dir: &Path,
    page_idx: usize,
    uid: &str,
    transform: TransformRec,
    deform: Option<DeformRec>,
    fallback_dir: Option<&Path>,
) -> Result<(), String> {
    let _span = crate::trace_scope!(
        cat::PERSIST,
        "update_raster_geometry page={} uid={} has_deform={}",
        page_idx,
        uid,
        deform.is_some()
    );
    let manifest_path = layers_dir.join(MANIFEST_FILE);
    let _guard = MANIFEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut manifest = read_manifest(&manifest_path)?.unwrap_or_else(LayersManifest::empty);
    ensure_page_staged(&mut manifest, page_idx, fallback_dir)?;
    let Some(page) = manifest.pages.iter_mut().find(|p| p.img_idx == page_idx) else {
        return Ok(());
    };
    if let Some(node) = page
        .tree
        .iter_mut()
        .find(|r| r.kind == LayerKindRec::Raster && r.uid == uid)
    {
        node.transform = Some(transform);
        node.deform = deform;
        fs::create_dir_all(layers_dir)
            .map_err(|e| format!("create {}: {e}", layers_dir.display()))?;
        write_manifest(&manifest_path, &manifest)?;
    }
    Ok(())
}

/// Sets a raster's non-destructive effects chain. With `rendered` (the post-effects pixels), writes
/// the rendered PNG (`rendered_file_name`) and records `rendered_file`; without it (empty chain),
/// clears `rendered_file` and deletes any stale rendered PNG. The base PNG is never touched, so the
/// effects stay fully reversible across restarts. A no-op if the page or node is absent.
pub fn update_raster_effects(
    layers_dir: &Path,
    page_idx: usize,
    uid: &str,
    effects: &[serde_json::Value],
    rendered: Option<&ColorImage>,
    fallback_dir: Option<&Path>,
) -> Result<(), String> {
    let _span = crate::trace_scope!(
        cat::PERSIST,
        "update_raster_effects page={} uid={} effects_len={} has_rendered={}",
        page_idx,
        uid,
        effects.len(),
        rendered.is_some()
    );
    let manifest_path = layers_dir.join(MANIFEST_FILE);
    let _guard = MANIFEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut manifest = read_manifest(&manifest_path)?.unwrap_or_else(LayersManifest::empty);
    ensure_page_staged(&mut manifest, page_idx, fallback_dir)?;
    let Some(page) = manifest.pages.iter_mut().find(|p| p.img_idx == page_idx) else {
        return Ok(());
    };
    let Some(node) = page
        .tree
        .iter_mut()
        .find(|r| r.kind == LayerKindRec::Raster && r.uid == uid)
    else {
        return Ok(());
    };
    fs::create_dir_all(layers_dir)
        .map_err(|e| format!("create {}: {e}", layers_dir.display()))?;
    let old_rendered = node.rendered_file.clone();
    match (effects.is_empty(), rendered) {
        (false, Some(image)) => {
            let fx = rendered_file_name(page_idx, uid);
            write_png(&layers_dir.join(&fx), image)?;
            node.rendered_file = Some(fx);
            node.effects = effects.to_vec();
        }
        _ => {
            // No effects: drop the chain and remove the stale rendered PNG.
            node.rendered_file = None;
            node.effects = Vec::new();
        }
    }
    // Remove a now-unreferenced old rendered PNG.
    if let Some(old) = old_rendered
        && node.rendered_file.as_deref() != Some(old.as_str())
    {
        let _ = fs::remove_file(layers_dir.join(&old));
    }
    write_manifest(&manifest_path, &manifest)
}

/// The PS-owned order/identity fields of a text node — the subset that `write_page_text_payload`
/// merges (preserving PS pin / Z / group across a typing-side rewrite). Internal helper for the merge;
/// the full inline payload travels in [`TextPayloadOut`].
#[derive(Clone)]
struct TextIdent {
    uid: String,
    /// Band Z when `pinned`; ignored otherwise (the text group's band Z is used).
    z: u32,
    /// Text group (Группа текста N) this overlay belongs to.
    layer_idx: u32,
    /// True when the overlay was given an explicit Z in PS (no auto page-Y ordering).
    pinned: bool,
    group_uid: Option<String>,
    /// `pinned` was set implicitly by PS grouping (vs. an explicit user pin). Preserved across a
    /// typing rewrite so ungrouping can restore page-Y order.
    pinned_by_group: bool,
}

/// A TEXT node with its full inline payload, handed in for the schema-v3 whole-page text flush
/// (`write_page_text_payload`). Unlike [`TextNodeOut`] (identity/order only, payload referenced into
/// `text_info.json`), this carries the render params, geometry, the rendered PNG name, and mask-clip
/// inline so `layers.json` is self-sufficient for text.
#[derive(Clone)]
pub struct TextPayloadOut {
    pub uid: String,
    pub name: String,
    pub z: u32,
    pub layer_idx: u32,
    pub pinned: bool,
    pub visible: bool,
    pub opacity: f32,
    pub group_uid: Option<String>,
    pub pinned_by_group: bool,
    /// Identity of the overlay payload this node renders from (kept in `payload_ref` for legacy
    /// readers). Usually equals `uid`.
    pub payload_uid: String,
    /// Opaque text render payload (was `text_info.json`'s `render_data`).
    pub render_data: serde_json::Value,
    /// Canonical geometry (center-anchored, rotation in radians); encoded to disk degrees on write.
    pub transform: TransformRec,
    pub deform: Option<DeformRec>,
    /// Rendered text PNG filename (already on disk from the typing render). `None` ⇒ no image yet.
    pub rendered_file: Option<String>,
    pub mask_clip: Option<bool>,
}

/// A text node decoded back from `layers.json`, consumed by both tabs (the PS editor's text-layer
/// metadata and the doc's text-node loader). Order (`z`) is NOT carried — the unified band Z is
/// derived from the page's bands, not the node — so it is intentionally omitted.
pub struct TextNodeIn {
    pub uid: String,
    pub name: String,
    pub layer_idx: u32,
    pub pinned: bool,
    pub visible: bool,
    pub opacity: f32,
    pub group_uid: Option<String>,
    pub pinned_by_group: bool,
    pub payload_uid: String,
    /// Schema v3 inline payload, present when the node carries its full text payload in `layers.json`
    /// (so the loader can build the text node without `text_info.json`). `None` for a legacy (v2) node.
    pub inline: Option<TextInlineIn>,
}

/// The inline TEXT payload decoded from a schema-v3 `layers.json` node: render params, geometry, the
/// rendered PNG name, and the mask-clip flag. Present only when the node is self-sufficient.
#[derive(Clone)]
pub struct TextInlineIn {
    pub render_data: serde_json::Value,
    pub transform: Option<TransformRec>,
    pub deform: Option<DeformRec>,
    /// Rendered text PNG filename in the layers dir (the node's displayed image).
    pub rendered_file: Option<String>,
    pub mask_clip: Option<bool>,
}

/// Writes a page's TEXT nodes with their FULL inline payload (schema v3) into `layers.json`,
/// preserving raster nodes (kind-filter), the page's PS groups, and rebuilding the text-group bands,
/// so it never clobbers rasters or `text_groups`. Each node carries `render_data` + geometry + the
/// rendered PNG name + `mask_clip` inline, making `layers.json` self-sufficient for text. This is the
/// single text writer (the former reference-only `text_info.json` writers were removed in A4–D).
///
/// PS-owned fields (`pinned` / `pinned_by_group` / `z` / `group_uid`) on an existing text node are
/// carried onto the incoming node by [`merge_preserved_text_fields`] (same as the typing rewrite), so
/// a doc flush does not clobber a pin / Z / PS group set in the PS editor. Does NO PNG IO: the
/// rendered text PNG is written by the typing render and named in `rendered_file`; this only writes the
/// manifest. Runs under `MANIFEST_LOCK`.
pub fn write_page_text_payload(
    layers_dir: &Path,
    fallback_dir: Option<&Path>,
    page_idx: usize,
    nodes: &[TextPayloadOut],
) -> Result<(), String> {
    let _span = crate::trace_scope!(
        cat::PERSIST,
        "write_page_text_payload page={} text_nodes={}",
        page_idx,
        nodes.len()
    );
    let manifest_path = layers_dir.join(MANIFEST_FILE);
    let _guard = MANIFEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut manifest = read_manifest(&manifest_path)?.unwrap_or_else(LayersManifest::empty);

    // Whether this page was already known on disk (primary staging OR the committed fallback). A page
    // that EXISTED but is now emptied (last text deleted) must stay PRESENT-but-EMPTY in the manifest —
    // NOT be removed — so the delete is honored across the triangle: the per-page loader sees the
    // primary page (no committed fallback → no resurrected text), and the owned-page merge processes the
    // page (present in unsaved) and replaces committed text with the empty set. Removing it (or never
    // writing it) makes the page ABSENT, which the loader/merge both treat as "fall back to committed" →
    // the deleted text RESURRECTS. Only a page that never existed ANYWHERE is omitted.
    let page_existed = manifest.page(page_idx).is_some()
        || fallback_dir.is_some_and(|d| {
            read_manifest(&d.join(MANIFEST_FILE))
                .ok()
                .flatten()
                .is_some_and(|m| m.page(page_idx).is_some())
        });

    // Nothing to write only when the page never existed anywhere AND there is no staging dir yet — i.e.
    // there is genuinely no page record to preserve. A previously-existing page emptied to nothing still
    // falls through so it is recorded PRESENT-but-EMPTY (deletion durability), even if the staging dir
    // must be created for it.
    if nodes.is_empty() && !page_existed && !layers_dir.exists() {
        return Ok(());
    }

    fs::create_dir_all(layers_dir)
        .map_err(|e| format!("create {}: {e}", layers_dir.display()))?;

    // Keep every non-text node (rasters) plus the page's PS groups; replace only the text nodes.
    // Text groups (`text_groups`) are intentionally NOT carried: text is fully-manual pinned-with-Z now,
    // so any legacy group band is dropped (its members become pinned bands at their own Z).
    #[allow(clippy::type_complexity)]
    let (mut tree, groups, existing_text): (Vec<LayerRec>, Vec<GroupRec>, Vec<LayerRec>) = manifest
        .page(page_idx)
        .map(|p| {
            (
                p.tree
                    .iter()
                    .filter(|r| r.kind != LayerKindRec::Text)
                    .cloned()
                    .collect(),
                p.groups.clone(),
                p.tree
                    .iter()
                    .filter(|r| r.kind == LayerKindRec::Text)
                    .cloned()
                    .collect(),
            )
        })
        .unwrap_or_default();

    // The doc carries the text-group axis (`layer_idx`) on each text node (typing sets it on create,
    // load projects it from disk), so the INCOMING value is authoritative — the doc is the source of
    // truth. `merge_preserved_text_fields` separately carries the PS-owned pin / z / group fields.
    let ident: Vec<TextIdent> = nodes
        .iter()
        .map(|n| TextIdent {
            uid: n.uid.clone(),
            z: n.z,
            layer_idx: n.layer_idx,
            pinned: n.pinned,
            group_uid: n.group_uid.clone(),
            pinned_by_group: n.pinned_by_group,
        })
        .collect();
    let mut merged_ident = merge_preserved_text_fields(&existing_text, &ident);

    // FULLY-MANUAL UNIFIED Z: text is always pinned-with-explicit-Z now (no auto-Y text groups). Force
    // every text pinned so it owns its own band at its Z (the doc-assigned unique Z, or a preserved
    // PS/typing-reordered Z from `merge_preserved_text_fields`). `rebuild_text_groups` is no longer
    // called — NO new `TextGroup` bands are created, and any legacy ones are dropped here (their members
    // are now pinned bands carrying their own Z, so the visual order is unchanged). `layer_idx` is kept
    // on each node purely as the historical group label; it no longer drives ordering.
    for ident in &mut merged_ident {
        ident.pinned = true;
    }
    let text_groups: Vec<TextGroupRec> = Vec::new();

    for (payload, ident) in nodes.iter().zip(merged_ident.iter()) {
        tree.push(text_payload_rec(payload, ident));
    }

    if tree.is_empty() && groups.is_empty() && !page_existed {
        // Never-existed page emptied to nothing → omit it entirely (don't create a stray record).
        manifest.remove_page(page_idx);
    } else {
        // A previously-existing page stays present even when emptied (empty tree + empty groups), so the
        // deletion is durable. An empty PageLayers is benign for every reader: `page_bands` yields no
        // bands, the doc loads zero nodes, ordering has nothing to place.
        manifest.upsert_page(PageLayers {
            img_idx: page_idx,
            groups,
            text_groups,
            tree,
        });
    }
    write_manifest(&manifest_path, &manifest)
}

/// Builds the schema-v3 `layers.json` record for a TEXT node with its full inline payload. `ident`
/// supplies the PS-owned order/identity fields after [`merge_preserved_text_fields`] (pin / z /
/// group); `payload` supplies the inline geometry + render params + rendered PNG name + mask-clip.
/// Geometry reuses the canonical `transform`/`deform` fields (same encoding as a raster: rotation in
/// radians); the rendered PNG name reuses `rendered_file` (a text node has no `base_file`/effects, so
/// the field unambiguously names the text render). A `payload_ref` into `text_info.json` is kept for
/// legacy-reader compatibility, but the inline payload is authoritative.
fn text_payload_rec(payload: &TextPayloadOut, ident: &TextIdent) -> LayerRec {
    LayerRec {
        uid: payload.uid.clone(),
        name: payload.name.clone(),
        kind: LayerKindRec::Text,
        z: ident.z,
        layer_idx: Some(ident.layer_idx),
        pinned: ident.pinned,
        pinned_by_group: ident.pinned_by_group,
        group_uid: ident.group_uid.clone(),
        visible: payload.visible,
        opacity: payload.opacity,
        transform: Some(payload.transform),
        deform: payload.deform.clone(),
        base_file: None,
        rendered_file: payload.rendered_file.clone(),
        image_size: None,
        effects: Vec::new(),
        payload_ref: Some(PayloadRef {
            store: TEXT_PAYLOAD_STORE.to_string(),
            uid: payload.payload_uid.clone(),
        }),
        render_data: Some(payload.render_data.clone()),
        mask_clip: payload.mask_clip,
    }
}

/// One band in a desired unified order (bottom-to-top), as edited in the PS unified layer panel.
#[derive(Clone, PartialEq, Eq)]
pub enum BandRef {
    /// A raster layer, by node uid.
    Raster(String),
    /// A text group, by `layer_idx`.
    TextGroup(u32),
    /// A pinned text overlay, by node uid.
    PinnedText(String),
}

/// Loads the page's unified bands (bottom-to-top), reading the manifest from `primary_dir` then
/// `fallback_dir`. Both tabs use this to render in the same order.
pub fn load_page_bands(
    primary_dir: &Path,
    fallback_dir: Option<&Path>,
    page_idx: usize,
) -> Vec<super::ordering::Band> {
    // PER-PAGE fallback: a committed-only page (absent from the staging manifest) must still load its
    // committed band order so PS and typing render it in the same unified order.
    let bands = read_page_with_fallback(primary_dir, fallback_dir, page_idx)
        .ok()
        .flatten()
        .as_ref()
        .map(super::ordering::page_bands)
        .unwrap_or_default();
    crate::trace_log!(
        cat::PERSIST,
        "load_page_bands page={} bands={}",
        page_idx,
        bands.len()
    );
    bands
}

/// Rewrites a page's unified band Z from `order` (bottom-to-top): each raster node's Z, each text
/// group's band Z, and each pinned text's Z become the band index. Texts in a `TextGroup` band are
/// marked unpinned; a `PinnedText` band marks that text pinned at its Z (its `layer_idx` is kept, so
/// unpinning later returns it to its group). PS owns this ordering.
pub fn save_page_band_order(
    layers_dir: &Path,
    page_idx: usize,
    order: &[BandRef],
) -> Result<(), String> {
    let _span = crate::trace_scope!(
        cat::PERSIST,
        "save_page_band_order page={} bands={}",
        page_idx,
        order.len()
    );
    let manifest_path = layers_dir.join(MANIFEST_FILE);
    let _guard = MANIFEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut manifest = read_manifest(&manifest_path)?.unwrap_or_else(LayersManifest::empty);
    let Some(page) = manifest.pages.iter_mut().find(|p| p.img_idx == page_idx) else {
        return Ok(());
    };
    apply_band_order(page, order);
    write_manifest(&manifest_path, &manifest)
}

/// Reassigns every band's Z to its index in `order` (bottom-to-top) and (un)pins text nodes: every
/// text starts unpinned, then each `PinnedText` band re-pins its node at its Z. Shared by
/// [`save_page_band_order`] and [`save_page_grouping`].
fn apply_band_order(page: &mut PageLayers, order: &[BandRef]) {
    // Start by unpinning every text node; `PinnedText` bands below re-pin the ones that stay pinned.
    for node in page
        .tree
        .iter_mut()
        .filter(|r| r.kind == LayerKindRec::Text)
    {
        node.pinned = false;
    }
    for (z, band) in order.iter().enumerate() {
        let z = z as u32;
        match band {
            BandRef::Raster(uid) => {
                if let Some(node) = page
                    .tree
                    .iter_mut()
                    .find(|r| r.kind == LayerKindRec::Raster && &r.uid == uid)
                {
                    node.z = z;
                }
            }
            BandRef::TextGroup(layer_idx) => {
                if let Some(group) = page.text_groups.iter_mut().find(|g| g.layer_idx == *layer_idx) {
                    group.z = z;
                }
            }
            BandRef::PinnedText(uid) => {
                if let Some(node) = page
                    .tree
                    .iter_mut()
                    .find(|r| r.kind == LayerKindRec::Text && &r.uid == uid)
                {
                    node.pinned = true;
                    node.z = z;
                }
            }
        }
    }
}

// Per-text reorder note: the typing tab's ⬆/⬇ arrows route a single overlay's band-Z move through the
// shared doc and `save_page_band_order` (see `TypingTextOverlayLayer::move_overlay_in_unified_z`),
// flattening the target's text group into per-overlay pinned bands in the tab from its live `Band`
// list. The former standalone `flatten_unified_order` / `move_node_one_step` (a separate disk RMW that
// would clobber the doc) were removed — the doc is the single source of truth.

/// A batched group-structure edit for the unified PS layer tree, applied in one locked manifest RMW
/// (no PNG IO). It changes membership (`group_uid` on raster AND text nodes), creates/removes
/// `GroupRec`s, toggles collapse, sets the unified band order, and records which texts were pinned
/// *implicitly* by grouping (so ungrouping can restore page-Y order). Fields are applied in this
/// order: remove groups → create groups → membership → collapse → band order → pin flags.
#[derive(Default)]
pub struct GroupingEdit {
    /// Groups to create (members reference these by `uid`).
    pub new_groups: Vec<GroupMeta>,
    /// Group uids to delete; any node in one of them is ungrouped.
    pub remove_groups: Vec<String>,
    /// Node uid (raster or text) → new `group_uid` (`None` ungroups). Matched across the whole tree.
    pub set_membership: Vec<(String, Option<String>)>,
    /// Group uid → collapsed state.
    pub set_collapsed: Vec<(String, bool)>,
    /// Group uid → visibility.
    pub set_group_visible: Vec<(String, bool)>,
    /// Group uid → opacity.
    pub set_group_opacity: Vec<(String, f32)>,
    /// Complete unified order (bottom-to-top); each band's Z becomes its index, pins applied.
    pub order: Vec<BandRef>,
    /// Text uids whose pin is owned by grouping: sets `pinned_by_group = true`.
    pub pin_for_group: Vec<String>,
    /// Text uids leaving a group: clears `pinned_by_group` (the `pinned` flag itself follows `order`).
    pub unpin_for_group: Vec<String>,
}

/// Applies a [`GroupingEdit`] to one page's manifest in a single locked read-modify-write. Does not
/// touch PNGs; the PS editor mirrors the same membership/group changes into its in-memory stack so a
/// later `save_page_rasters` stays consistent. Returns `Ok(())` (a no-op) if the page is absent.
pub fn save_page_grouping(
    layers_dir: &Path,
    page_idx: usize,
    edit: &GroupingEdit,
) -> Result<(), String> {
    let _span = crate::trace_scope!(
        cat::PERSIST,
        "save_page_grouping page={} new_groups={} remove_groups={}",
        page_idx,
        edit.new_groups.len(),
        edit.remove_groups.len()
    );
    let manifest_path = layers_dir.join(MANIFEST_FILE);
    let _guard = MANIFEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut manifest = read_manifest(&manifest_path)?.unwrap_or_else(LayersManifest::empty);
    let Some(page) = manifest.pages.iter_mut().find(|p| p.img_idx == page_idx) else {
        return Ok(());
    };

    // Remove groups, ungrouping their members.
    if !edit.remove_groups.is_empty() {
        page.groups.retain(|g| !edit.remove_groups.contains(&g.uid));
        for node in &mut page.tree {
            if let Some(g) = &node.group_uid
                && edit.remove_groups.contains(g)
            {
                node.group_uid = None;
            }
        }
    }
    // Create groups.
    for g in &edit.new_groups {
        if !page.groups.iter().any(|e| e.uid == g.uid) {
            page.groups.push(GroupRec {
                uid: g.uid.clone(),
                name: g.name.clone(),
                visible: g.visible,
                opacity: g.opacity,
                collapsed: g.collapsed,
                parent_uid: None,
            });
        }
    }
    // Membership (raster + text nodes, matched by uid).
    for (node_uid, group_uid) in &edit.set_membership {
        if let Some(node) = page.tree.iter_mut().find(|r| &r.uid == node_uid) {
            node.group_uid = group_uid.clone();
        }
    }
    // Collapse / visibility / opacity.
    for (group_uid, collapsed) in &edit.set_collapsed {
        if let Some(g) = page.groups.iter_mut().find(|g| &g.uid == group_uid) {
            g.collapsed = *collapsed;
        }
    }
    for (group_uid, visible) in &edit.set_group_visible {
        if let Some(g) = page.groups.iter_mut().find(|g| &g.uid == group_uid) {
            g.visible = *visible;
        }
    }
    for (group_uid, opacity) in &edit.set_group_opacity {
        if let Some(g) = page.groups.iter_mut().find(|g| &g.uid == group_uid) {
            g.opacity = *opacity;
        }
    }
    // Unified order (Z + pins).
    if !edit.order.is_empty() {
        apply_band_order(page, &edit.order);
    }
    // Pin ownership flags (after `order`, which set the `pinned` bit itself).
    for uid in &edit.pin_for_group {
        if let Some(node) = page
            .tree
            .iter_mut()
            .find(|r| r.kind == LayerKindRec::Text && &r.uid == uid)
        {
            node.pinned_by_group = true;
        }
    }
    for uid in &edit.unpin_for_group {
        if let Some(node) = page
            .tree
            .iter_mut()
            .find(|r| r.kind == LayerKindRec::Text && &r.uid == uid)
        {
            node.pinned_by_group = false;
        }
    }

    // Drop text-group bands that no longer have any unpinned member (e.g. their only text was just
    // pinned into a PS group), so an empty band doesn't linger in the unified Z order.
    let live_text_groups: std::collections::BTreeSet<u32> = page
        .tree
        .iter()
        .filter(|r| r.kind == LayerKindRec::Text && !r.pinned)
        .filter_map(|r| r.layer_idx)
        .collect();
    page.text_groups
        .retain(|g| live_text_groups.contains(&g.layer_idx));

    write_manifest(&manifest_path, &manifest)
}

/// Carries each existing text node's PS-owned fields (`pinned`, `pinned_by_group`, `z`, and the
/// unified-tree `group_uid`) onto the matching incoming node, so a typing-side rewrite (which only
/// knows overlay properties and always sends `group_uid: None`) does not clobber a pin / Z / PS
/// group membership set in the PS editor. Without preserving `group_uid` here, every typing autosave
/// would silently ungroup texts that the PS tree put into a group.
fn merge_preserved_text_fields(
    existing_text: &[LayerRec],
    nodes: &[TextIdent],
) -> Vec<TextIdent> {
    let existing: HashMap<&str, &LayerRec> = existing_text
        .iter()
        .map(|r| (r.uid.as_str(), r))
        .collect();
    nodes
        .iter()
        .map(|n| {
            let mut node = n.clone();
            if let Some(rec) = existing.get(n.uid.as_str()) {
                // Carry the PS-owned `group_uid` (PS-tree membership) regardless.
                node.group_uid = rec.group_uid.clone();
                node.pinned_by_group = rec.pinned_by_group;
                // Preserve the explicit band Z ONLY from an already-PINNED disk node (a PS/typing reorder
                // authority). A legacy UNPINNED (text-group) disk node must NOT clobber the incoming
                // (doc) Z — that doc Z is the per-text flattened order computed on read, and clobbering it
                // with the group's shared Z would collapse every group member back onto one band, losing
                // the manual order. Text is always pinned-with-explicit-Z now (see `write_page_text_payload`).
                if rec.pinned {
                    node.pinned = true;
                    node.z = rec.z;
                }
            }
            node
        })
        .collect()
}

// `rebuild_text_groups` was retired: text is fully-manual pinned-with-explicit-Z now, so
// `write_page_text_payload` never creates `TextGroup` bands. Legacy groups are flattened into per-text
// pinned bands on read (`layer_doc::ensure_page_loaded`) and dropped on the next write.

/// Loads the text-overlay nodes for one page (ordered by `z`), resolving the manifest from
/// `primary_dir` then `fallback_dir`. Nodes without a `text_info` payload reference are skipped.
pub fn load_page_text_nodes(
    primary_dir: &Path,
    fallback_dir: Option<&Path>,
    page_idx: usize,
) -> Result<Vec<TextNodeIn>, String> {
    // PER-PAGE fallback: a committed-only page (absent from the staging manifest) must still load its
    // committed text — otherwise a save would replace it with an empty staging page (data loss).
    let Some(page) = read_page_with_fallback(primary_dir, fallback_dir, page_idx)? else {
        return Ok(Vec::new());
    };

    let mut nodes: Vec<&LayerRec> = page
        .tree
        .iter()
        .filter(|r| r.kind == LayerKindRec::Text)
        .collect();
    nodes.sort_by_key(|r| r.z);

    let out: Vec<TextNodeIn> = nodes
        .into_iter()
        .filter_map(|r| {
            // A v3 inline node carries its full payload in `render_data` and may have no `payload_ref`;
            // a legacy (v2) node has only a `payload_ref` into `text_info.json`. Keep either.
            let payload_uid = r
                .payload_ref
                .as_ref()
                .filter(|p| p.store == TEXT_PAYLOAD_STORE)
                .map(|p| p.uid.clone());
            let inline = r.render_data.as_ref().map(|rd| TextInlineIn {
                render_data: rd.clone(),
                transform: r.transform,
                deform: r.deform.clone(),
                rendered_file: r.rendered_file.clone(),
                mask_clip: r.mask_clip,
            });
            if payload_uid.is_none() && inline.is_none() {
                // Neither a legacy reference nor an inline payload — nothing to materialize.
                return None;
            }
            // For an inline node without a payload_ref, fall back to the node's own uid as the payload
            // identity (the rendered PNG is keyed by uid).
            let payload_uid = payload_uid.unwrap_or_else(|| r.uid.clone());
            Some(TextNodeIn {
                uid: r.uid.clone(),
                name: r.name.clone(),
                layer_idx: r.layer_idx.unwrap_or(0),
                pinned: r.pinned,
                visible: r.visible,
                opacity: r.opacity,
                group_uid: r.group_uid.clone(),
                pinned_by_group: r.pinned_by_group,
                payload_uid,
                inline,
            })
        })
        .collect();
    crate::trace_log!(
        cat::PERSIST,
        "load_page_text_nodes page={} text_nodes={}",
        page_idx,
        out.len()
    );
    Ok(out)
}

/// Reads `layers.json`, migrating any older on-disk format up to the current canonical shape. All
/// version/compat handling lives in `compat`; every reader here sees only a current-version manifest.
fn read_manifest(path: &Path) -> Result<Option<LayersManifest>, String> {
    super::compat::read_manifest(path)
}

/// Resolves one page with a PER-PAGE fallback: returns the PRIMARY (unsaved/staging) manifest's page
/// when present, else the COMMITTED (`fallback_dir`) manifest's page. This is load-bearing: a session
/// that has staged ANY page makes the primary `layers.json` EXIST, but a committed-only page X is NOT in
/// it (`primary.page(X) == None`). A manifest-level fallback ("only when the primary file is absent")
/// then returns empty for page X — so opening it loads ZERO text/rasters into the doc, and a later save
/// REPLACES the committed page with the empty staging one (DATA LOSS). Per-page fallback loads page X's
/// real committed content into the doc, so the owned/whole-page-replace merge stays faithful. READ-only.
fn read_page_with_fallback(
    primary_dir: &Path,
    fallback_dir: Option<&Path>,
    page_idx: usize,
) -> Result<Option<PageLayers>, String> {
    if let Some(m) = read_manifest(&primary_dir.join(MANIFEST_FILE))?
        && let Some(page) = m.page(page_idx)
    {
        return Ok(Some(page.clone()));
    }
    if let Some(dir) = fallback_dir
        && let Some(m) = read_manifest(&dir.join(MANIFEST_FILE))?
        && let Some(page) = m.page(page_idx)
    {
        return Ok(Some(page.clone()));
    }
    Ok(None)
}

/// Merges the UNSAVED staging `layers.json` (`unsaved_layers_dir`) INTO the committed one
/// (`committed_layers_dir`) PER PAGE, and writes the merged result to the committed location. This is
/// the "save to project" step for `layers.json` — it must NOT be a file-level overwrite, because the
/// doc session's unsaved manifest only holds the pages the user actually visited, while the committed
/// manifest may carry more pages (e.g. all 23 written by the eager migration).
///
/// OWNERSHIP, two axes:
/// - A page present ONLY in the committed manifest (not in unsaved at all) is PRESERVED entirely (the
///   ВВД/13 truncation fix — never drop a committed page the session never touched).
/// - A page present in BOTH: for RASTERS/groups the unsaved page wins (the PS/typing session is
///   authoritative). For TEXT, the unsaved page wins ONLY when `owned_text_pages` contains it — i.e.
///   the session actually LOADED that page's text into the doc, so the unsaved text (including
///   DELETIONS) is authoritative. When the page is NOT owned (e.g. a PS raster-only edit on a page
///   whose text was never loaded), the committed page's TEXT nodes + text-group bands are PRESERVED —
///   otherwise the unsaved page's missing text would silently DROP the committed text (HIGH data loss),
///   while a naive "preserve absent text" would RESURRECT a legitimately-deleted text. Honoring
///   ownership avoids both.
///
/// `schema_version` is taken as the max of the two (never downgrade). No-op when there is no unsaved
/// manifest. Returns whether anything was written.
pub fn merge_unsaved_layers_into_committed(
    committed_layers_dir: &Path,
    unsaved_layers_dir: &Path,
    owned_text_pages: &HashSet<usize>,
) -> Result<bool, String> {
    let _span = crate::trace_scope!(
        cat::PERSIST,
        "merge_unsaved_layers_into_committed owned_text_pages={}",
        owned_text_pages.len()
    );
    let unsaved_path = unsaved_layers_dir.join(MANIFEST_FILE);
    let _guard = MANIFEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(unsaved) = read_manifest(&unsaved_path)? else {
        crate::trace_log!(
            cat::PERSIST,
            "merge_unsaved_layers_into_committed -> no unsaved manifest (no-op)"
        );
        return Ok(false); // nothing staged — committed stays as-is
    };
    let committed_path = committed_layers_dir.join(MANIFEST_FILE);
    let mut merged = read_manifest(&committed_path)?.unwrap_or_else(LayersManifest::empty);

    for mut page in unsaved.pages {
        let page_idx = page.img_idx;
        // For a page the session did NOT own text on, carry the COMMITTED text nodes + text-group bands
        // onto the unsaved (raster) page, so committed text is not dropped by the whole-page replace.
        if !owned_text_pages.contains(&page_idx)
            && let Some(committed_page) = merged.page(page_idx)
        {
            let committed_text: Vec<LayerRec> = committed_page
                .tree
                .iter()
                .filter(|r| r.kind == LayerKindRec::Text)
                .cloned()
                .collect();
            if !committed_text.is_empty() {
                // Replace any (unowned, hence stale) text the unsaved page might carry with the
                // committed text, and restore the committed text-group bands.
                page.tree.retain(|r| r.kind != LayerKindRec::Text);
                page.tree.extend(committed_text);
                if page.text_groups.is_empty() {
                    page.text_groups = committed_page.text_groups.clone();
                }
            }
        }
        merged.upsert_page(page);
    }
    merged.schema_version = merged.schema_version.max(unsaved.schema_version);
    crate::trace_log!(
        cat::PERSIST,
        "merge_unsaved_layers_into_committed merged_pages={} -> {}",
        merged.pages.len(),
        committed_path.display()
    );
    fs::create_dir_all(committed_layers_dir)
        .map_err(|e| format!("create {}: {e}", committed_layers_dir.display()))?;
    write_manifest(&committed_path, &merged)?;
    Ok(true)
}

fn write_manifest(path: &Path, manifest: &LayersManifest) -> Result<(), String> {
    let text = serde_json::to_string_pretty(manifest)
        .map_err(|e| format!("serialize manifest: {e}"))?;
    fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Removes base PNGs for `page_idx` that are not in `keep`.
fn prune_orphan_pngs(layers_dir: &Path, page_idx: usize, keep: &[String]) {
    let prefix = page_file_prefix(page_idx);
    let Ok(entries) = fs::read_dir(layers_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(&prefix) && name.ends_with(".png") && !keep.iter().any(|k| k == name) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn read_png_with_fallback(
    primary_dir: &Path,
    fallback_dir: Option<&Path>,
    file: &str,
) -> Option<ColorImage> {
    let mut candidates: Vec<PathBuf> = vec![primary_dir.join(file)];
    if let Some(dir) = fallback_dir {
        candidates.push(dir.join(file));
    }
    candidates.into_iter().find_map(|p| read_png(&p))
}

fn read_png(path: &Path) -> Option<ColorImage> {
    if !path.is_file() {
        return None;
    }
    let rgba = image::open(path).ok()?.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// Writes a TEXT node's rendered image to `ps_p{page:04}_{uid}_text.png` in `layers_dir` and returns
/// the filename. Used by the doc's text flush when the in-memory render is dirty (the typing tab
/// otherwise already wrote the PNG; flush need not re-encode an unchanged image). Creates the dir.
pub fn write_text_image(
    layers_dir: &Path,
    page_idx: usize,
    uid: &str,
    image: &ColorImage,
) -> Result<String, String> {
    fs::create_dir_all(layers_dir)
        .map_err(|e| format!("create {}: {e}", layers_dir.display()))?;
    let file = text_image_file_name(page_idx, uid);
    crate::trace_log!(
        cat::PERSIST,
        "write_text_image page={} uid={} file={} size={}x{}",
        page_idx,
        uid,
        file,
        image.size[0],
        image.size[1]
    );
    write_png(&layers_dir.join(&file), image)?;
    Ok(file)
}

fn write_png(path: &Path, image: &ColorImage) -> Result<(), String> {
    let [w, h] = image.size;
    let mut raw = Vec::with_capacity(w * h * 4);
    for px in &image.pixels {
        raw.extend_from_slice(&px.to_srgba_unmultiplied());
    }
    let buf = image::RgbaImage::from_raw(w as u32, h as u32, raw)
        .ok_or_else(|| format!("layer image {}x{} buffer mismatch", w, h))?;
    buf.save(path)
        .map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::layer_model::manifest::LAYERS_SCHEMA_VERSION;
    use eframe::egui::Color32;

    fn img(size: [usize; 2], c: Color32) -> ColorImage {
        ColorImage::filled(size, c)
    }

    /// Minimal `TextPayloadOut` for test setup (the production text writer is `write_page_text_payload`).
    fn text_payload_out(uid: &str, z: u32, layer_idx: u32, pinned: bool) -> TextPayloadOut {
        TextPayloadOut {
            uid: uid.into(),
            name: uid.into(),
            z,
            layer_idx,
            pinned,
            visible: true,
            opacity: 1.0,
            group_uid: None,
            pinned_by_group: false,
            payload_uid: uid.into(),
            render_data: serde_json::Value::Null,
            transform: TransformRec { cx: 0.0, cy: 0.0, rotation: 0.0, scale: 1.0 },
            deform: None,
            rendered_file: None,
            mask_clip: None,
        }
    }

    #[test]
    fn round_trips_raster_layers() {
        let dir = std::env::temp_dir().join(format!("ml_layers_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let red = img([4, 3], Color32::from_rgba_unmultiplied(200, 10, 10, 255));
        let blue = img([2, 2], Color32::from_rgba_unmultiplied(0, 0, 255, 128));
        let outs = vec![
            RasterLayerOut {
                uid: "uid-a".into(),
                name: "Слой 1".into(),
                visible: true,
                opacity: 1.0,
                transform: TransformRec { cx: 2.0, cy: 1.5, rotation: 0.0, scale: 1.0 },
                deform: None,
                group_uid: Some("grp-1".into()),
                image: &red,
                pixels_dirty: true,
                mask_clip: None,
            },
            RasterLayerOut {
                uid: "uid-b".into(),
                name: "Слой 2".into(),
                visible: false,
                opacity: 0.5,
                transform: TransformRec { cx: 10.0, cy: 20.0, rotation: 0.25, scale: 2.0 },
                deform: None,
                group_uid: None,
                image: &blue,
                pixels_dirty: true,
                mask_clip: None,
            },
        ];
        let groups = vec![GroupMeta {
            uid: "grp-1".into(),
            name: "Группа".into(),
            visible: false,
            opacity: 0.25,
            collapsed: false,
        }];
        save_page_rasters(&dir, 0, &outs, &groups, &[]).unwrap();

        let loaded = load_page_rasters(&dir, None, 0).unwrap();
        assert_eq!(loaded.layers.len(), 2);
        assert_eq!(loaded.layers[0].uid, "uid-a");
        assert_eq!(loaded.layers[0].group_uid.as_deref(), Some("grp-1"));
        assert_eq!(loaded.layers[0].image.size, [4, 3]);
        assert_eq!(loaded.layers[1].uid, "uid-b");
        assert!(loaded.layers[1].group_uid.is_none());
        assert!(!loaded.layers[1].visible);
        assert!((loaded.layers[1].opacity - 0.5).abs() < 1e-6);
        assert!((loaded.layers[1].transform.scale - 2.0).abs() < 1e-6);
        assert_eq!(loaded.groups.len(), 1);
        assert_eq!(loaded.groups[0].uid, "grp-1");
        assert!(!loaded.groups[0].visible);
        assert!((loaded.groups[0].opacity - 0.25).abs() < 1e-6);

        // Explicitly removing all layers and groups prunes the page and its PNGs. (An empty `layers`
        // with no `removed_uids` would PRESERVE the rasters as unowned — deletions must be explicit.)
        save_page_rasters(&dir, 0, &[], &[], &["uid-a".to_string(), "uid-b".to_string()]).unwrap();
        assert!(load_page_rasters(&dir, None, 0).unwrap().layers.is_empty());
        let pngs = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".png"))
            .count();
        assert_eq!(pngs, 0, "orphan PNGs should be pruned");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trips_raster_deform_mesh() {
        let dir = std::env::temp_dir().join(format!("ml_deform_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let red = img([4, 3], Color32::from_rgba_unmultiplied(200, 10, 10, 255));
        // A 2x2 mesh skewed off the affine quad: the loader must preserve these exact points.
        let mesh = DeformRec {
            cols: 2,
            rows: 2,
            points_px: vec![[1.0, 2.0], [30.0, 3.0], [2.0, 40.0], [33.0, 44.0]],
        };
        let outs = vec![RasterLayerOut {
            uid: "uid-deform".into(),
            name: "Деформ".into(),
            visible: true,
            opacity: 1.0,
            transform: TransformRec { cx: 2.0, cy: 1.5, rotation: 0.0, scale: 1.0 },
            deform: Some(mesh.clone()),
            group_uid: None,
            image: &red,
            pixels_dirty: true,
            mask_clip: None,
        }];
        save_page_rasters(&dir, 0, &outs, &[], &[]).unwrap();

        let loaded = load_page_rasters(&dir, None, 0).unwrap();
        assert_eq!(loaded.layers.len(), 1);
        let got = loaded.layers[0].deform.as_ref().expect("deform round-trips");
        assert_eq!(got.cols, 2);
        assert_eq!(got.rows, 2);
        assert_eq!(got.points_px, mesh.points_px);

        // A raster with no deform must still load as None (no spurious mesh).
        let blue = img([2, 2], Color32::from_rgba_unmultiplied(0, 0, 255, 255));
        save_page_rasters(
            &dir,
            0,
            &[RasterLayerOut {
                uid: "uid-deform".into(),
                name: "Деформ".into(),
                visible: true,
                opacity: 1.0,
                transform: TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
                deform: None,
                group_uid: None,
                image: &blue,
                pixels_dirty: true,
                mask_clip: None,
            }],
            &[],
            &[],
        )
        .unwrap();
        let loaded = load_page_rasters(&dir, None, 0).unwrap();
        assert!(loaded.layers[0].deform.is_none(), "cleared deform stays None");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn raster_mask_clip_round_trips_and_is_preserved_for_non_owning_writers() {
        let dir = std::env::temp_dir().join(format!("ml_raster_clip_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let pic = img([2, 2], Color32::WHITE);

        // Save a raster with mask_clip = Some(true): it must round-trip.
        save_page_rasters(
            &dir,
            0,
            &[RasterLayerOut {
                uid: "r".into(),
                name: "R".into(),
                visible: true,
                opacity: 1.0,
                transform: TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
                deform: None,
                group_uid: None,
                image: &pic,
                pixels_dirty: true,
                mask_clip: Some(true),
            }],
            &[],
            &[],
        )
        .unwrap();
        let loaded = load_page_rasters(&dir, None, 0).unwrap();
        assert_eq!(loaded.layers[0].mask_clip, Some(true), "mask_clip round-trips");

        // A NON-owning writer (mask_clip: None, e.g. PS) must PRESERVE the on-disk Some(true).
        save_page_rasters(
            &dir,
            0,
            &[RasterLayerOut {
                uid: "r".into(),
                name: "R".into(),
                visible: true,
                opacity: 1.0,
                transform: TransformRec { cx: 9.0, cy: 9.0, rotation: 0.0, scale: 1.0 },
                deform: None,
                group_uid: None,
                image: &pic,
                pixels_dirty: false,
                mask_clip: None,
            }],
            &[],
            &[],
        )
        .unwrap();
        let loaded = load_page_rasters(&dir, None, 0).unwrap();
        assert_eq!(loaded.layers[0].mask_clip, Some(true), "None-writer preserved the on-disk clip");

        // An OWNING writer can clear it to Some(false).
        save_page_rasters(
            &dir,
            0,
            &[RasterLayerOut {
                uid: "r".into(),
                name: "R".into(),
                visible: true,
                opacity: 1.0,
                transform: TransformRec { cx: 9.0, cy: 9.0, rotation: 0.0, scale: 1.0 },
                deform: None,
                group_uid: None,
                image: &pic,
                pixels_dirty: false,
                mask_clip: Some(false),
            }],
            &[],
            &[],
        )
        .unwrap();
        let loaded = load_page_rasters(&dir, None, 0).unwrap();
        assert_eq!(loaded.layers[0].mask_clip, Some(false), "owning writer set it off");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn raster_and_text_nodes_coexist_without_clobbering() {
        let dir = std::env::temp_dir().join(format!("ml_coexist_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let img = img([3, 3], Color32::WHITE);
        let raster = || RasterLayerOut {
            uid: "r1".into(),
            name: "R".into(),
            visible: true,
            opacity: 1.0,
            transform: TransformRec { cx: 1.5, cy: 1.5, rotation: 0.0, scale: 1.0 },
            deform: None,
            group_uid: None,
            image: &img,
            pixels_dirty: true,
            mask_clip: None,
        };
        save_page_rasters(&dir, 0, &[raster()], &[], &[]).unwrap();

        // Writing text nodes (the production inline writer) preserves the raster node.
        let mut t1 = text_payload_out("t1", 5, 0, false);
        t1.payload_uid = "ov-1".into();
        write_page_text_payload(&dir, None, 0, &[t1]).unwrap();
        assert_eq!(load_page_rasters(&dir, None, 0).unwrap().layers.len(), 1);
        let texts = load_page_text_nodes(&dir, None, 0).unwrap();
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].payload_uid, "ov-1");

        // Re-saving rasters preserves the text node...
        save_page_rasters(&dir, 0, &[raster()], &[], &[]).unwrap();
        assert_eq!(load_page_text_nodes(&dir, None, 0).unwrap().len(), 1);

        // ...and clearing text nodes preserves the raster.
        write_page_text_payload(&dir, None, 0, &[]).unwrap();
        assert_eq!(load_page_text_nodes(&dir, None, 0).unwrap().len(), 0);
        assert_eq!(load_page_rasters(&dir, None, 0).unwrap().layers.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_page_text_payload_round_trips_inline_and_preserves_raster() {
        let dir = std::env::temp_dir().join(format!("ml_textpayload_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        // Seed a raster on the page; the text payload write must not drop it.
        let pic = img([3, 3], Color32::WHITE);
        save_page_rasters(
            &dir,
            0,
            &[RasterLayerOut {
                uid: "r1".into(),
                name: "R".into(),
                visible: true,
                opacity: 1.0,
                transform: TransformRec { cx: 1.5, cy: 1.5, rotation: 0.0, scale: 1.0 },
                deform: None,
                group_uid: None,
                image: &pic,
                pixels_dirty: true,
                mask_clip: None,
            }],
            &[],
            &[],
        )
        .unwrap();

        let mesh = DeformRec {
            cols: 2,
            rows: 2,
            points_px: vec![[0.0, 0.0], [5.0, 1.0], [1.0, 6.0], [7.0, 8.0]],
        };
        write_page_text_payload(
            &dir,
            None,
            0,
            &[TextPayloadOut {
                uid: "t1".into(),
                name: "T".into(),
                z: 9,
                layer_idx: 2,
                pinned: true,
                visible: false,
                opacity: 0.6,
                group_uid: None,
                pinned_by_group: false,
                payload_uid: "t1".into(),
                render_data: serde_json::json!({ "text": "Hi" }),
                transform: TransformRec { cx: 12.0, cy: 34.0, rotation: 0.4, scale: 2.0 },
                deform: Some(mesh.clone()),
                rendered_file: Some("ps_p0000_t1_text.png".into()),
                mask_clip: Some(true),
            }],
        )
        .unwrap();

        // Raster survives.
        assert_eq!(load_page_rasters(&dir, None, 0).unwrap().layers.len(), 1, "raster preserved");

        // Text node reloads with full inline payload.
        let texts = load_page_text_nodes(&dir, None, 0).unwrap();
        assert_eq!(texts.len(), 1);
        let t = &texts[0];
        assert_eq!(t.uid, "t1");
        assert!(t.pinned, "pin persisted");
        assert!(!t.visible);
        assert!((t.opacity - 0.6).abs() < 1e-6);
        let inline = t.inline.as_ref().expect("inline payload present");
        assert_eq!(inline.render_data["text"], "Hi");
        assert_eq!(inline.rendered_file.as_deref(), Some("ps_p0000_t1_text.png"));
        assert_eq!(inline.mask_clip, Some(true));
        let tr = inline.transform.expect("inline transform");
        assert!((tr.cx - 12.0).abs() < 1e-4);
        assert!((tr.rotation - 0.4).abs() < 1e-5, "rotation stored as radians, no deg conversion");
        assert_eq!(inline.deform.as_ref().unwrap().points_px, mesh.points_px);

        // A second write that drops the text node preserves the raster (kind-filter both ways).
        write_page_text_payload(&dir, None, 0, &[]).unwrap();
        assert_eq!(load_page_text_nodes(&dir, None, 0).unwrap().len(), 0, "text node cleared");
        assert_eq!(load_page_rasters(&dir, None, 0).unwrap().layers.len(), 1, "raster still preserved");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn band_order_reassigns_z_and_pins_text() {
        use crate::models::layer_model::ordering::Band;
        let dir = std::env::temp_dir().join(format!("ml_band_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let img = img([2, 2], Color32::WHITE);
        save_page_rasters(
            &dir,
            0,
            &[RasterLayerOut {
                uid: "r1".into(),
                name: "R".into(),
                visible: true,
                opacity: 1.0,
                transform: TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
                deform: None,
                group_uid: None,
                image: &img,
                pixels_dirty: true,
                mask_clip: None,
            }],
            &[],
            &[],
        )
        .unwrap();
        // FULLY-MANUAL UNIFIED Z: a written text is ALWAYS pinned-with-explicit-Z (no auto-Y TextGroup),
        // so `page_bands` emits a `PinnedText` band — interleaved with the raster like any other band.
        write_page_text_payload(&dir, None, 0, &[text_payload_out("t1", 0, 0, false)]).unwrap();
        let texts = load_page_text_nodes(&dir, None, 0).unwrap();
        assert!(texts[0].pinned, "text is pinned-with-explicit-Z (no text group)");
        let bands = load_page_bands(&dir, None, 0);
        assert_eq!(bands.len(), 2, "raster + pinned-text bands, no TextGroup band");
        assert!(
            !bands.iter().any(|b| matches!(b, Band::TextGroup { .. })),
            "no TextGroup band is ever produced"
        );

        // Put the text BELOW the raster.
        save_page_band_order(
            &dir,
            0,
            &[BandRef::PinnedText("t1".into()), BandRef::Raster("r1".into())],
        )
        .unwrap();
        let bands = load_page_bands(&dir, None, 0);
        assert!(matches!(&bands[0], Band::PinnedText { uid, .. } if uid == "t1"), "text below raster");
        assert!(matches!(&bands[1], Band::Raster { uid, .. } if uid == "r1"));

        // Move the text back ABOVE the raster.
        save_page_band_order(
            &dir,
            0,
            &[BandRef::Raster("r1".into()), BandRef::PinnedText("t1".into())],
        )
        .unwrap();
        let texts = load_page_text_nodes(&dir, None, 0).unwrap();
        assert_eq!(texts.len(), 1);
        assert!(texts[0].pinned, "text stays pinned");
        let bands = load_page_bands(&dir, None, 0);
        assert!(matches!(bands.first(), Some(Band::Raster { uid, .. }) if uid == "r1"));
        assert!(matches!(bands.last(), Some(Band::PinnedText { uid, .. }) if uid == "t1"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_text_move_in_unified_z_round_trips() {
        // PART 1/2: moving ONE text up/down one step in the unified band-Z round-trips on disk, and the
        // order both tabs read back (`load_page_bands`) reflects exactly the move — text and raster share
        // one Z axis, every text is its own band.
        use crate::models::layer_model::ordering::Band;
        let dir = std::env::temp_dir().join(format!("ml_textmove_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let pic = img([2, 2], Color32::WHITE);
        save_page_rasters(
            &dir,
            0,
            &[RasterLayerOut {
                uid: "r0".into(),
                name: "R".into(),
                visible: true,
                opacity: 1.0,
                transform: TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
                deform: None,
                group_uid: None,
                image: &pic,
                pixels_dirty: true,
                mask_clip: None,
            }],
            &[],
            &[],
        )
        .unwrap();
        // Two texts written (both pinned now); seed the order r0, t0, t1 (bottom-to-top).
        write_page_text_payload(&dir, None, 0, &[text_payload_out("t0", 0, 0, false), text_payload_out("t1", 0, 0, false)]).unwrap();
        save_page_band_order(
            &dir,
            0,
            &[BandRef::Raster("r0".into()), BandRef::PinnedText("t0".into()), BandRef::PinnedText("t1".into())],
        )
        .unwrap();
        let band_uids = |bands: &[Band]| -> Vec<String> {
            bands
                .iter()
                .map(|b| match b {
                    Band::Raster { uid, .. } | Band::PinnedText { uid, .. } => uid.clone(),
                    Band::TextGroup { layer_idx, .. } => format!("group{layer_idx}"),
                })
                .collect()
        };
        assert_eq!(band_uids(&load_page_bands(&dir, None, 0)), vec!["r0", "t0", "t1"]);

        // Move t0 UP one step (swap with t1): order becomes r0, t1, t0.
        save_page_band_order(
            &dir,
            0,
            &[BandRef::Raster("r0".into()), BandRef::PinnedText("t1".into()), BandRef::PinnedText("t0".into())],
        )
        .unwrap();
        assert_eq!(band_uids(&load_page_bands(&dir, None, 0)), vec!["r0", "t1", "t0"], "single text moved up");

        // A subsequent text flush PRESERVES the reordered Z (merge_preserved_text_fields keeps pinned Z).
        write_page_text_payload(&dir, None, 0, &[text_payload_out("t0", 0, 0, false), text_payload_out("t1", 0, 0, false)]).unwrap();
        assert_eq!(band_uids(&load_page_bands(&dir, None, 0)), vec!["r0", "t1", "t0"], "reorder survives a text flush");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_unsaved_layers_preserves_committed_only_pages() {
        // Bug B (ВВД/13 truncation) regression: the committed `layers.json` has MORE pages than the
        // unsaved staging one (the eager migration wrote all pages to committed; the doc session only
        // touched some). The save-to-project merge must KEEP every committed-only page and apply the
        // unsaved edits — NOT file-overwrite committed with the smaller unsaved manifest.
        let root = std::env::temp_dir().join(format!("ml_merge_layers_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let committed = root.join("layers");
        let unsaved = root.join("layers_unsaved");
        let pic = img([2, 2], Color32::WHITE);
        let raster = |uid: &str| RasterLayerOut {
            uid: uid.into(),
            name: uid.into(),
            visible: true,
            opacity: 1.0,
            transform: TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
            deform: None,
            group_uid: None,
            image: &pic,
            pixels_dirty: true,
            mask_clip: None,
        };
        // COMMITTED: rasters on pages 0..5 (like the migration writing every page).
        for p in 0..5 {
            save_page_rasters(&committed, p, &[raster(&format!("c{p}"))], &[], &[]).unwrap();
        }
        // UNSAVED: only page 0 visited/edited (a NEW raster).
        save_page_rasters(&unsaved, 0, &[raster("edited0")], &[], &[]).unwrap();

        let owned: HashSet<usize> = [0].into_iter().collect();
        let wrote = merge_unsaved_layers_into_committed(&committed, &unsaved, &owned).unwrap();
        assert!(wrote);

        let m = read_manifest(&committed.join("layers.json")).unwrap().unwrap();
        let mut pages: Vec<usize> = m.pages.iter().map(|p| p.img_idx).collect();
        pages.sort_unstable();
        assert_eq!(pages, vec![0, 1, 2, 3, 4], "all committed pages survive (no truncation)");
        // Page 0 reflects the unsaved edit (its raster set replaced the committed page).
        let p0 = m.page(0).unwrap();
        assert!(p0.tree.iter().any(|r| r.uid == "edited0"), "page 0 got the unsaved edit");
        assert!(!p0.tree.iter().any(|r| r.uid == "c0"), "page 0 = the unsaved version (committed replaced)");
        // Pages 1..5 keep their committed-only rasters.
        for p in 1..5 {
            assert!(
                m.page(p).unwrap().tree.iter().any(|r| r.uid == format!("c{p}")),
                "committed-only page {p} preserved"
            );
        }

        // No unsaved manifest → no-op (committed untouched).
        let empty_unsaved = root.join("empty");
        assert!(!merge_unsaved_layers_into_committed(&committed, &empty_unsaved, &owned).unwrap());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn merge_preserves_committed_text_on_unowned_raster_only_edit() {
        // DROP TRAP (HIGH data loss): a committed page has TEXT + a raster. The session edits only the
        // RASTER on it (e.g. a PS band reorder) WITHOUT ever loading that page's text into the doc, so
        // staging has the raster but NO text. With ownership, the merge must PRESERVE the committed text
        // (the page is NOT in `owned_text_pages`) — not drop it.
        let root = std::env::temp_dir().join(format!("ml_merge_droptrap_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let committed = root.join("layers");
        let unsaved = root.join("layers_unsaved");
        let pic = img([2, 2], Color32::WHITE);
        let raster = |uid: &str| RasterLayerOut {
            uid: uid.into(),
            name: uid.into(),
            visible: true,
            opacity: 1.0,
            transform: TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
            deform: None,
            group_uid: None,
            image: &pic,
            pixels_dirty: true,
            mask_clip: None,
        };
        // COMMITTED page 0: a raster + a text overlay.
        save_page_rasters(&committed, 0, &[raster("r0")], &[], &[]).unwrap();
        write_page_text_payload(&committed, None, 0, &[text_payload_out("t0", 0, 0, false)]).unwrap();
        assert_eq!(load_page_text_nodes(&committed, None, 0).unwrap().len(), 1);

        // UNSAVED page 0: only a raster edit (no text — the page's text was never loaded into the doc).
        save_page_rasters(&unsaved, 0, &[raster("r0_moved")], &[], &[]).unwrap();

        // Page 0 is NOT owned (text not loaded this session).
        let owned: HashSet<usize> = HashSet::new();
        merge_unsaved_layers_into_committed(&committed, &unsaved, &owned).unwrap();

        // The committed text SURVIVES; the raster edit applied.
        let texts = load_page_text_nodes(&committed, None, 0).unwrap();
        assert_eq!(texts.len(), 1, "committed text preserved on an unowned raster-only edit");
        assert_eq!(texts[0].uid, "t0");
        let m = read_manifest(&committed.join("layers.json")).unwrap().unwrap();
        assert!(m.page(0).unwrap().tree.iter().any(|r| r.uid == "r0_moved"), "raster edit applied");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn merge_does_not_resurrect_deleted_text_on_owned_page() {
        // RESURRECTION TRAP: the session LOADED page 0's text (owned), the user DELETED a text overlay
        // (staging page 0 has the raster but NOT that text). The merge must NOT bring it back — for an
        // OWNED page the unsaved text (incl. deletions) is authoritative.
        let root = std::env::temp_dir().join(format!("ml_merge_resurrect_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let committed = root.join("layers");
        let unsaved = root.join("layers_unsaved");
        let pic = img([2, 2], Color32::WHITE);
        let raster = |uid: &str| RasterLayerOut {
            uid: uid.into(),
            name: uid.into(),
            visible: true,
            opacity: 1.0,
            transform: TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
            deform: None,
            group_uid: None,
            image: &pic,
            pixels_dirty: true,
            mask_clip: None,
        };
        // COMMITTED page 0: a raster + TWO text overlays.
        save_page_rasters(&committed, 0, &[raster("r0")], &[], &[]).unwrap();
        write_page_text_payload(
            &committed,
            None,
            0,
            &[text_payload_out("keep", 0, 0, false), text_payload_out("deleted", 1, 0, false)],
        )
        .unwrap();
        assert_eq!(load_page_text_nodes(&committed, None, 0).unwrap().len(), 2);

        // UNSAVED page 0: the session kept "keep" but DELETED "deleted" (only "keep" flushed).
        save_page_rasters(&unsaved, 0, &[raster("r0")], &[], &[]).unwrap();
        write_page_text_payload(&unsaved, None, 0, &[text_payload_out("keep", 0, 0, false)]).unwrap();

        // Page 0 IS owned (its text was loaded this session).
        let owned: HashSet<usize> = [0].into_iter().collect();
        merge_unsaved_layers_into_committed(&committed, &unsaved, &owned).unwrap();

        // The deleted text stays DELETED; "keep" survives.
        let texts = load_page_text_nodes(&committed, None, 0).unwrap();
        let uids: std::collections::HashSet<&str> = texts.iter().map(|n| n.uid.as_str()).collect();
        assert!(uids.contains("keep"), "kept text survives");
        assert!(!uids.contains("deleted"), "deleted text NOT resurrected on an owned page");
        assert_eq!(texts.len(), 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn real_pipeline_committed_only_page_text_survives_after_staging_another_page() {
        // FIX 1 end-to-end (HIGH data loss): drive the REAL doc pipeline, not a hand-built owned set.
        //
        // 1. COMMITTED has text on pages 0 AND 8. UNSAVED stages page 0 only → the staging `layers.json`
        //    EXISTS but has NO page 8. Pre-fix, `load_page_text_nodes` fell back to committed ONLY when the
        //    staging file was absent (file-level), so opening page 8 loaded ZERO text into the doc.
        // 2. `ensure_page_loaded(8)` (primary=unsaved, fallback=committed) MUST load page 8's committed
        //    text (per-page fallback). Pre-fix this returned empty → the gap.
        // 3. A PS raster edit on page 8 → `flush_page` + `flush_page_text` stage page 8 (now owned).
        // 4. The save merge with the REAL computed owned set whole-page-replaces page 8 — and because the
        //    doc actually loaded the committed text, the staged page 8 still carries it → it SURVIVES.
        use crate::models::layer_model::layer_doc::LayerDoc;

        let root = std::env::temp_dir().join(format!("ml_realpipe_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let committed = root.join("layers");
        let unsaved = root.join("layers_unsaved");
        let pic = img([2, 2], Color32::WHITE);
        let raster = |uid: &str| RasterLayerOut {
            uid: uid.into(),
            name: uid.into(),
            visible: true,
            opacity: 1.0,
            transform: TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
            deform: None,
            group_uid: None,
            image: &pic,
            pixels_dirty: true,
            mask_clip: None,
        };

        // A text payload whose rendered PNG actually exists on disk, so `ensure_page_loaded` builds the
        // inline text node (the loader skips a text node with no rendered image).
        let text_with_png = |dir: &Path, uid: &str, page: usize| -> TextPayloadOut {
            let file = rendered_file_name(page, uid);
            write_png(&dir.join(&file), &img([2, 2], Color32::GREEN)).unwrap();
            let mut out = text_payload_out(uid, 1, 0, false);
            out.rendered_file = Some(file);
            out.render_data = serde_json::json!({ "text": uid });
            out
        };

        // COMMITTED: raster + text on page 0 and page 8.
        save_page_rasters(&committed, 0, &[raster("c0")], &[], &[]).unwrap();
        let t0 = text_with_png(&committed, "t0", 0);
        write_page_text_payload(&committed, None, 0, &[t0]).unwrap();
        save_page_rasters(&committed, 8, &[raster("c8")], &[], &[]).unwrap();
        let t8 = text_with_png(&committed, "t8", 8);
        write_page_text_payload(&committed, None, 8, &[t8]).unwrap();

        // Sanity: committed page 8 has a self-sufficient inline text node with a rendered PNG.
        {
            let seed = load_page_text_nodes(&committed, None, 8).unwrap();
            assert_eq!(seed.len(), 1, "seed: committed page 8 has one text node");
            assert!(
                seed[0].inline.as_ref().and_then(|i| i.rendered_file.as_ref()).is_some(),
                "seed: inline rendered_file present"
            );
        }

        // STAGE page 0 this session → the unsaved manifest now EXISTS but contains only page 0.
        save_page_rasters(&unsaved, 0, &[raster("c0")], &[], &[]).unwrap();
        assert!(
            read_manifest(&unsaved.join("layers.json")).unwrap().unwrap().page(8).is_none(),
            "precondition: staging manifest has no page 8"
        );

        // Page sizes for the chapter (full map required by ensure_page_loaded's ribbon migration).
        let mut page_sizes: HashMap<usize, [usize; 2]> = HashMap::new();
        for p in 0..=8 {
            page_sizes.insert(p, [2, 2]);
        }

        // OPEN committed-only page 8 through the real loader: primary=unsaved, fallback=committed.
        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(8, &unsaved, Some(committed.as_path()), &page_sizes).unwrap();
        let loaded_text = doc
            .page(8)
            .expect("page 8 resident")
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, crate::models::layer_model::layer_doc::NodeKind::Text))
            .count();
        assert_eq!(loaded_text, 1, "per-page fallback loaded page 8's COMMITTED text into the doc");

        // PS raster edit on page 8, then stage it through the real flush path (rasters + text).
        doc.set_raster_pixels(8, "c8", img([2, 2], Color32::BLACK), img([2, 2], Color32::BLACK), Vec::new(), true);
        doc.flush_page(8, &unsaved, Some(committed.as_path())).unwrap();
        doc.flush_page_text(8, &unsaved, Some(committed.as_path())).unwrap();

        // The REAL owned set: every resident page that flushed text Ok (page 8 was loaded + flushed).
        let mut owned: HashSet<usize> = HashSet::new();
        for p in doc.resident_pages() {
            if doc.flush_page_text(p, &unsaved, Some(committed.as_path())).is_ok() {
                owned.insert(p);
            }
        }
        assert!(owned.contains(&8), "page 8 is owned (its text was loaded + flushed this session)");

        // Save-to-project merge with the REAL owned set.
        merge_unsaved_layers_into_committed(&committed, &unsaved, &owned).unwrap();

        // Committed page 8's text SURVIVES (no data loss); page 0's text is intact too.
        let final_t8 = load_page_text_nodes(&committed, None, 8).unwrap();
        assert_eq!(final_t8.len(), 1, "committed page 8 text survives the save (FIX 1)");
        assert_eq!(final_t8[0].uid, "t8");
        assert_eq!(load_page_text_nodes(&committed, None, 0).unwrap().len(), 1, "page 0 text intact");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn add_page_raster_on_typeset_page_preserves_committed_text_through_save() {
        // ITEM A (data-loss): creating an image/raster on a page that already has committed TEXT must NOT
        // drop that text on save-to-project. The create worker stages the raster via `add_page_raster`
        // on the (previously absent-from-staging) page; pre-fix it staged a TEXT-LESS page, so the doc
        // reload saw text=0 and the owned-page merge dropped the committed text. The fallback-seed keeps
        // the committed text on the staged page. Drives the REAL pipeline (stage → doc reload → flush →
        // merge with the real owned set).
        use crate::models::layer_model::layer_doc::LayerDoc;

        let root = std::env::temp_dir().join(format!("ml_addraster_text_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let committed = root.join("layers");
        let unsaved = root.join("layers_unsaved");

        let text_with_png = |dir: &Path, uid: &str, page: usize| -> TextPayloadOut {
            fs::create_dir_all(dir).unwrap();
            let file = rendered_file_name(page, uid);
            write_png(&dir.join(&file), &img([2, 2], Color32::GREEN)).unwrap();
            let mut out = text_payload_out(uid, 1, 0, false);
            out.rendered_file = Some(file);
            out.render_data = serde_json::json!({ "text": uid });
            out
        };

        // COMMITTED page 3: a TYPESET page (one text), NO staging yet.
        let t3 = text_with_png(&committed, "t3", 3);
        write_page_text_payload(&committed, None, 3, &[t3]).unwrap();
        assert_eq!(load_page_text_nodes(&committed, None, 3).unwrap().len(), 1, "seed: committed text");

        // CREATE an image on page 3 → stage a raster into the (absent) unsaved page, seeding from
        // committed (the production create path: `add_page_raster(unsaved, Some(committed), ...)`).
        add_page_raster(
            &unsaved,
            Some(committed.as_path()),
            3,
            "img1",
            "Картинка",
            true,
            1.0,
            TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
            &img([2, 2], Color32::BLUE),
        )
        .unwrap();
        // The staged page now carries BOTH the new raster AND the committed text.
        assert!(
            read_manifest(&unsaved.join("layers.json")).unwrap().unwrap().page(3).is_some(),
            "page 3 staged"
        );
        assert_eq!(
            load_page_text_nodes(&unsaved, None, 3).unwrap().len(),
            1,
            "staged page keeps the committed text (fallback seed)"
        );

        // Reload page 3 through the doc (primary=unsaved, fallback=committed) and flush — the real path
        // `invalidate_raster_cache_for_page` + reload + save trigger.
        let mut page_sizes: HashMap<usize, [usize; 2]> = HashMap::new();
        for p in 0..=3 {
            page_sizes.insert(p, [2, 2]);
        }
        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(3, &unsaved, Some(committed.as_path()), &page_sizes).unwrap();
        assert_eq!(
            doc.page(3).unwrap().nodes.iter().filter(|n| n.is_text()).count(),
            1,
            "doc reload sees the committed text (NOT zero)"
        );
        let mut owned: HashSet<usize> = HashSet::new();
        for p in doc.resident_pages() {
            if doc.flush_page_text(p, &unsaved, Some(committed.as_path())).is_ok() {
                owned.insert(p);
            }
        }

        // Save-to-project merge: committed text SURVIVES.
        merge_unsaved_layers_into_committed(&committed, &unsaved, &owned).unwrap();
        assert_eq!(
            load_page_text_nodes(&committed, None, 3).unwrap().len(),
            1,
            "committed text survives creating an image on a typeset page (ITEM A)"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn add_page_raster_does_not_resurrect_a_doc_deleted_last_text() {
        // ITEM A follow-up (HIGH resurrection): the drop-fix seeds an unstaged page from COMMITTED, which
        // is STALE w.r.t. an in-session deletion of the page's LAST text (that empty page was never
        // staged). Creating an image then re-seeded the deleted text → resurrection. The fix flushes the
        // target page's CURRENT doc text to staging (PRESENT-but-EMPTY for a deleted-last-text page)
        // BEFORE `add_page_raster`, so `ensure_page_staged` sees the page present and does NOT seed stale
        // committed text. Drives the real pipeline: commit text → load → delete → pre-flush → create
        // raster → merge → text stays deleted.
        use crate::models::layer_model::layer_doc::LayerDoc;

        let root = std::env::temp_dir().join(format!("ml_addraster_resurrect_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let committed = root.join("layers");
        let unsaved = root.join("layers_unsaved");

        let text_with_png = |dir: &Path, uid: &str, page: usize| -> TextPayloadOut {
            fs::create_dir_all(dir).unwrap();
            let file = rendered_file_name(page, uid);
            write_png(&dir.join(&file), &img([2, 2], Color32::GREEN)).unwrap();
            let mut out = text_payload_out(uid, 1, 0, false);
            out.rendered_file = Some(file);
            out.render_data = serde_json::json!({ "text": uid });
            out
        };

        // COMMITTED page 3: a TEXT-ONLY page (one text), NO staging yet.
        let t3 = text_with_png(&committed, "t3", 3);
        write_page_text_payload(&committed, None, 3, &[t3]).unwrap();
        assert_eq!(load_page_text_nodes(&committed, None, 3).unwrap().len(), 1, "seed: committed text");

        let mut page_sizes: HashMap<usize, [usize; 2]> = HashMap::new();
        for p in 0..=3 {
            page_sizes.insert(p, [2, 2]);
        }

        // Session: load page 3 into the doc, then DELETE its last text (doc only — the empty page is not
        // staged by the placement-save, mirroring `spawn_overlay_placement_save`'s `pages_with_text`).
        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(3, &unsaved, Some(committed.as_path()), &page_sizes).unwrap();
        assert!(doc.remove_node(3, "t3"));
        assert_eq!(doc.page(3).unwrap().nodes.iter().filter(|n| n.is_text()).count(), 0, "text deleted in doc");
        assert!(
            read_manifest(&unsaved.join("layers.json")).ok().flatten().is_none(),
            "precondition: nothing staged yet (the empty page was skipped by the placement-save)"
        );

        // THE FIX: flush the target page's CURRENT doc text to staging before creating the raster. For a
        // deleted-last-text page this writes it PRESENT-but-EMPTY (`flush_target_page_text_to_staging`).
        doc.flush_page_text(3, &unsaved, Some(committed.as_path())).unwrap();
        assert!(
            read_manifest(&unsaved.join("layers.json")).unwrap().unwrap().page(3).is_some(),
            "page 3 now staged present-but-empty (deletion durable)"
        );
        assert_eq!(load_page_text_nodes(&unsaved, None, 3).unwrap().len(), 0, "staged page has no text");

        // CREATE an image on page 3 → `add_page_raster` seeds from committed only if the page is ABSENT;
        // it is now PRESENT (empty) → no stale seed → no resurrection.
        add_page_raster(
            &unsaved,
            Some(committed.as_path()),
            3,
            "img1",
            "Картинка",
            true,
            1.0,
            TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
            &img([2, 2], Color32::BLUE),
        )
        .unwrap();
        assert_eq!(
            load_page_text_nodes(&unsaved, None, 3).unwrap().len(),
            0,
            "staged page still has NO text after add_page_raster (deletion not resurrected)"
        );

        // Reload + flush + save-to-project merge with the real owned set.
        doc.evict_page(3);
        doc.ensure_page_loaded(3, &unsaved, Some(committed.as_path()), &page_sizes).unwrap();
        let mut owned: HashSet<usize> = HashSet::new();
        for p in doc.resident_pages() {
            if doc.flush_page_text(p, &unsaved, Some(committed.as_path())).is_ok() {
                owned.insert(p);
            }
        }
        merge_unsaved_layers_into_committed(&committed, &unsaved, &owned).unwrap();
        assert_eq!(
            load_page_text_nodes(&committed, None, 3).unwrap().len(),
            0,
            "deleted text stays deleted through an image-create + save (NO resurrection)"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn real_pipeline_text_only_page_delete_is_not_resurrected() {
        // FIX 3 end-to-end (RESURRECTION, the symmetric bug the per-page fallback could re-introduce):
        // a TEXT-ONLY committed page (no rasters, no groups) has its last text deleted and saved. The
        // delete must STICK — not resurrect.
        //
        // Before this fix: `flush_page_text` → `write_page_text_payload([])` hit the `remove_page` branch
        // (empty tree + empty groups), so the page became ABSENT from the unsaved manifest. Then the
        // per-page loader fell back to COMMITTED (returning the deleted text) AND the owned-merge skipped
        // the absent page (committed preserved whole) → the deletion was undone.
        //
        // With the WRITE-keep-present fix: an emptied-but-previously-existing page stays PRESENT-but-EMPTY
        // in unsaved, so the owned-merge processes it and replaces committed text with the empty set.
        use crate::models::layer_model::layer_doc::LayerDoc;

        let root = std::env::temp_dir().join(format!("ml_textonly_del_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let committed = root.join("layers");
        let unsaved = root.join("layers_unsaved");

        // A text payload whose rendered PNG actually exists, so `ensure_page_loaded` builds the node.
        let text_with_png = |dir: &Path, uid: &str, page: usize| -> TextPayloadOut {
            fs::create_dir_all(dir).unwrap();
            let file = rendered_file_name(page, uid);
            write_png(&dir.join(&file), &img([2, 2], Color32::GREEN)).unwrap();
            let mut out = text_payload_out(uid, 1, 0, false);
            out.rendered_file = Some(file);
            out.render_data = serde_json::json!({ "text": uid });
            out
        };

        // COMMITTED page 3: TEXT-ONLY (one text, NO raster, NO groups).
        let t3 = text_with_png(&committed, "t3", 3);
        write_page_text_payload(&committed, None, 3, &[t3]).unwrap();
        assert_eq!(load_page_text_nodes(&committed, None, 3).unwrap().len(), 1, "seed: committed text");

        // Full chapter page-size map (ensure_page_loaded's ribbon migration needs it).
        let mut page_sizes: HashMap<usize, [usize; 2]> = HashMap::new();
        for p in 0..=3 {
            page_sizes.insert(p, [2, 2]);
        }

        // OPEN page 3 (primary=unsaved which has NOTHING yet, fallback=committed): loads the text.
        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(3, &unsaved, Some(committed.as_path()), &page_sizes).unwrap();
        assert_eq!(
            doc.page(3).unwrap().nodes.iter().filter(|n| n.is_text()).count(),
            1,
            "doc loaded the committed text"
        );

        // DELETE the text in the doc, then flush — exactly the real save path.
        assert!(doc.remove_node(3, "t3"));
        // Build the owned set the way `flush_text_layers` does: every resident page flushed Ok is owned.
        let mut owned: HashSet<usize> = HashSet::new();
        for p in doc.resident_pages() {
            if doc.flush_page_text(p, &unsaved, Some(committed.as_path())).is_ok() {
                owned.insert(p);
            }
        }
        assert!(owned.contains(&3), "page 3 owned (text loaded + flushed this session)");

        // The emptied page must remain PRESENT in the unsaved manifest (write-keep-present), so the merge
        // can honor the deletion — NOT absent (which would resurrect committed text).
        let staged = read_manifest(&unsaved.join("layers.json")).unwrap().unwrap();
        let staged_page = staged.page(3).expect("emptied page stays present in unsaved (not removed)");
        assert!(
            staged_page.tree.iter().all(|r| r.kind != LayerKindRec::Text),
            "staged page 3 carries no text (deletion honored)"
        );

        // Save-to-project merge with the REAL owned set.
        merge_unsaved_layers_into_committed(&committed, &unsaved, &owned).unwrap();

        // The deletion STICKS: committed page 3 now has zero text (NOT resurrected).
        assert_eq!(
            load_page_text_nodes(&committed, None, 3).unwrap().len(),
            0,
            "deleted text on a text-only page is NOT resurrected (FIX 3)"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn raster_unified_z_reorder_swaps_band_order() {
        // The typing raster ⬆/⬇ core: flatten bands → swap the target raster one step → save band order.
        use crate::models::layer_model::ordering::Band;
        let dir = std::env::temp_dir().join(format!("ml_raster_reorder_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let pic = img([2, 2], Color32::WHITE);
        let raster = |uid: &str| RasterLayerOut {
            uid: uid.into(),
            name: uid.into(),
            visible: true,
            opacity: 1.0,
            transform: TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
            deform: None,
            group_uid: None,
            image: &pic,
            pixels_dirty: true,
            mask_clip: None,
        };
        // Two rasters: r0 (bottom, z=0), r1 (top, z=1) by insertion.
        save_page_rasters(&dir, 0, &[raster("r0"), raster("r1")], &[], &[]).unwrap();
        let band_uids = |dir: &std::path::Path| -> Vec<String> {
            load_page_bands(dir, None, 0)
                .into_iter()
                .filter_map(|b| match b {
                    Band::Raster { uid, .. } => Some(uid),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(band_uids(&dir), vec!["r0", "r1"], "initial bottom-to-top order");

        // Move r0 UP one step (swap with r1): [r1, r0]. This is the order `move_node_in_unified_z`
        // computes and passes to `save_page_band_order`.
        save_page_band_order(
            &dir,
            0,
            &[BandRef::Raster("r1".into()), BandRef::Raster("r0".into())],
        )
        .unwrap();
        assert_eq!(band_uids(&dir), vec!["r1", "r0"], "r0 moved up past r1");

        // Move r0 back DOWN one step: [r0, r1].
        save_page_band_order(
            &dir,
            0,
            &[BandRef::Raster("r0".into()), BandRef::Raster("r1".into())],
        )
        .unwrap();
        assert_eq!(band_uids(&dir), vec!["r0", "r1"], "r0 moved back down");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_text_reorder_via_band_order_survives_inline_text_flush() {
        // The typing per-text ⬆/⬇ reorder writes a flattened band order (pin + Z) via
        // `save_page_band_order`; a SUBSEQUENT inline text flush (`write_page_text_payload`, the doc's
        // text writer) must PRESERVE that pin + Z — i.e. not clobber the reorder (`merge_preserved_text_fields`).
        use crate::models::layer_model::ordering::Band;
        let dir = std::env::temp_dir().join(format!("ml_pertext_reorder_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        // Two unpinned texts in the same group 0 (no raster needed).
        write_page_text_payload(
            &dir,
            None,
            0,
            &[
                text_payload_out("t_lo", 0, 0, false),
                text_payload_out("t_hi", 0, 0, false),
            ],
        )
        .unwrap();

        // Flatten the group into per-overlay pinned bands and put t_hi BELOW t_lo (reorder result).
        save_page_band_order(
            &dir,
            0,
            &[BandRef::PinnedText("t_hi".into()), BandRef::PinnedText("t_lo".into())],
        )
        .unwrap();

        // Both texts are now pinned; t_hi is the bottom band, t_lo the top.
        let bands = load_page_bands(&dir, None, 0);
        assert!(matches!(bands.first(), Some(Band::PinnedText { uid, .. }) if uid == "t_hi"));
        assert!(matches!(bands.last(), Some(Band::PinnedText { uid, .. }) if uid == "t_lo"));
        let texts = load_page_text_nodes(&dir, None, 0).unwrap();
        assert!(texts.iter().all(|t| t.pinned), "reorder pinned every text");

        // The typing tab now flushes the page's text inline (group_uid: None, the doc never carries the
        // disk pin). This MUST NOT clobber the pin/Z the reorder just wrote.
        let with_render = |uid: &str| {
            let mut n = text_payload_out(uid, 0, 0, false);
            n.render_data = serde_json::json!({ "text": uid });
            n
        };
        write_page_text_payload(&dir, None, 0, &[with_render("t_lo"), with_render("t_hi")]).unwrap();

        // Reorder preserved: still pinned, same band order, AND now self-sufficient inline.
        let bands = load_page_bands(&dir, None, 0);
        assert!(
            matches!(bands.first(), Some(Band::PinnedText { uid, .. }) if uid == "t_hi"),
            "t_hi still the bottom band after the inline flush (not clobbered)"
        );
        assert!(matches!(bands.last(), Some(Band::PinnedText { uid, .. }) if uid == "t_lo"));
        let texts = load_page_text_nodes(&dir, None, 0).unwrap();
        assert!(texts.iter().all(|t| t.pinned), "pin survived the inline flush");
        // No text-group band lingers (all texts pinned out of the group).
        assert!(
            !bands.iter().any(|b| matches!(b, Band::TextGroup { .. })),
            "the flattened group leaves no TextGroup band"
        );
        // The flush made the nodes self-sufficient (inline payload present).
        assert!(texts.iter().all(|t| t.inline.is_some()), "inline payload written by the flush");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grouping_edit_moves_raster_and_text_into_one_group() {
        use crate::models::layer_model::ordering::Band;
        let dir = std::env::temp_dir().join(format!("ml_group_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let img = img([2, 2], Color32::WHITE);
        save_page_rasters(
            &dir,
            0,
            &[RasterLayerOut {
                uid: "r1".into(),
                name: "R".into(),
                visible: true,
                opacity: 1.0,
                transform: TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
                deform: None,
                group_uid: None,
                image: &img,
                pixels_dirty: true,
                mask_clip: None,
            }],
            &[],
            &[],
        )
        .unwrap();
        write_page_text_payload(&dir, None, 0, &[text_payload_out("t1", 0, 0, false)]).unwrap();

        // Put both the raster and the text into one mixed group, contiguous, text pinned above.
        save_page_grouping(
            &dir,
            0,
            &GroupingEdit {
                new_groups: vec![GroupMeta {
                    uid: "g1".into(),
                    name: "Mixed".into(),
                    visible: true,
                    opacity: 1.0,
                    collapsed: false,
                }],
                set_membership: vec![
                    ("r1".into(), Some("g1".into())),
                    ("t1".into(), Some("g1".into())),
                ],
                order: vec![BandRef::Raster("r1".into()), BandRef::PinnedText("t1".into())],
                pin_for_group: vec!["t1".into()],
                ..Default::default()
            },
        )
        .unwrap();

        let rasters = load_page_rasters(&dir, None, 0).unwrap();
        assert_eq!(rasters.groups.len(), 1, "group created");
        assert_eq!(rasters.layers[0].group_uid.as_deref(), Some("g1"));
        let texts = load_page_text_nodes(&dir, None, 0).unwrap();
        assert_eq!(texts[0].group_uid.as_deref(), Some("g1"), "text in same group");
        assert!(texts[0].pinned && texts[0].pinned_by_group, "grouped text auto-pinned");
        let bands = load_page_bands(&dir, None, 0);
        assert!(matches!(bands.first(), Some(Band::Raster { uid, .. }) if uid == "r1"));
        assert!(matches!(bands.last(), Some(Band::PinnedText { uid, .. }) if uid == "t1"));

        // A typing-side rewrite (the inline text flush, group_uid: None) must NOT drop PS membership/pin.
        write_page_text_payload(&dir, None, 0, &[text_payload_out("t1", 0, 0, false)]).unwrap();
        let texts = load_page_text_nodes(&dir, None, 0).unwrap();
        assert_eq!(texts[0].group_uid.as_deref(), Some("g1"), "membership survived typing save");
        assert!(texts[0].pinned && texts[0].pinned_by_group, "pin survived typing save");

        // Ungroup the text: clears membership + group-owned pin.
        save_page_grouping(
            &dir,
            0,
            &GroupingEdit {
                set_membership: vec![("t1".into(), None)],
                unpin_for_group: vec!["t1".into()],
                order: vec![BandRef::Raster("r1".into()), BandRef::TextGroup(0)],
                ..Default::default()
            },
        )
        .unwrap();
        let texts = load_page_text_nodes(&dir, None, 0).unwrap();
        assert!(texts[0].group_uid.is_none(), "ungrouped");
        assert!(!texts[0].pinned && !texts[0].pinned_by_group, "unpinned back to page-Y order");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn targeted_raster_add_and_update_transform() {
        let dir = std::env::temp_dir().join(format!("ml_targeted_raster_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        // A pre-existing raster (e.g. from the PS editor) that must survive a targeted add.
        save_page_rasters(
            &dir,
            0,
            &[RasterLayerOut {
                uid: "ps".into(),
                name: "PS".into(),
                visible: true,
                opacity: 1.0,
                transform: TransformRec { cx: 0.0, cy: 0.0, rotation: 0.0, scale: 1.0 },
                deform: None,
                group_uid: None,
                image: &img([2, 2], Color32::RED),
                pixels_dirty: true,
                mask_clip: None,
            }],
            &[],
            &[],
        )
        .unwrap();

        // Add a new raster (as the typing tab would for an external image).
        let t0 = TransformRec { cx: 5.0, cy: 6.0, rotation: 0.0, scale: 1.0 };
        let file = add_page_raster(&dir, None, 0, "img", "Картинка", true, 1.0, t0, &img([3, 4], Color32::BLUE)).unwrap();
        assert!(file.contains("img"));
        let loaded = load_page_rasters(&dir, None, 0).unwrap();
        assert_eq!(loaded.layers.len(), 2, "both rasters present");
        let added = loaded.layers.iter().find(|l| l.uid == "img").unwrap();
        assert_eq!(added.image.size, [3, 4]);
        assert!((added.transform.cx - 5.0).abs() < 1e-6);
        // New raster is on top (higher z than the existing one) — it loads last.
        assert_eq!(loaded.layers.last().unwrap().uid, "img");

        // Move it (transform only, no PNG change).
        let t1 = TransformRec { cx: 50.0, cy: 60.0, rotation: 0.5, scale: 2.0 };
        update_raster_transform(&dir, 0, "img", t1, None).unwrap();
        let loaded = load_page_rasters(&dir, None, 0).unwrap();
        let added = loaded.layers.iter().find(|l| l.uid == "img").unwrap();
        assert!((added.transform.cx - 50.0).abs() < 1e-6 && (added.transform.scale - 2.0).abs() < 1e-6);
        assert_eq!(added.image.size, [3, 4], "transform update must not touch pixels");

        // The PS raster is still intact through all of it.
        assert!(loaded.layers.iter().any(|l| l.uid == "ps"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_destructive_raster_effects_round_trip_and_preserve() {
        let dir = std::env::temp_dir().join(format!("ml_raster_fx_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        // A raster created with a base image (no effects yet).
        add_page_raster(
            &dir,
            None,
            0,
            "r",
            "Pic",
            true,
            1.0,
            TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
            &img([2, 2], Color32::RED),
        )
        .unwrap();

        // Apply effects: write a rendered (larger) image; base is untouched.
        let effects = vec![serde_json::json!({"type":"shadow"})];
        update_raster_effects(&dir, 0, "r", &effects, Some(&img([4, 4], Color32::BLUE)), None).unwrap();
        let loaded = load_page_rasters(&dir, None, 0).unwrap();
        let r = &loaded.layers[0];
        assert_eq!(r.image.size, [4, 4], "display is the rendered image");
        assert_eq!(r.base_file, "ps_p0000_r.png", "base file is the original");
        assert_eq!(r.effects.len(), 1, "effects chain stored");
        // The decoded base pixels are the pre-effects original, distinct from the rendered display,
        // so the effects stay reversible after a reload.
        assert_eq!(r.base_image.size, [2, 2], "base_image is the pre-effects original");
        assert_ne!(
            r.base_image.size, r.image.size,
            "base_image is distinct from the rendered display when effects are present"
        );

        // A PS-style whole-page save of a NON-dirty raster must PRESERVE the effects + rendered file.
        save_page_rasters(
            &dir,
            0,
            &[RasterLayerOut {
                uid: "r".into(),
                name: "Pic".into(),
                visible: true,
                opacity: 1.0,
                transform: TransformRec { cx: 9.0, cy: 9.0, rotation: 0.0, scale: 1.0 },
                deform: None,
                group_uid: None,
                image: &img([4, 4], Color32::BLUE), // PS holds the display image
                pixels_dirty: false,
                mask_clip: None,
            }],
            &[],
            &[],
        )
        .unwrap();
        let loaded = load_page_rasters(&dir, None, 0).unwrap();
        let r = &loaded.layers[0];
        assert_eq!(r.effects.len(), 1, "effects survived a non-dirty PS save");
        assert_eq!(r.image.size, [4, 4], "still showing rendered");
        assert!((r.transform.cx - 9.0).abs() < 1e-6, "transform update applied");

        // Remove effects: rendered file dropped, display falls back to the base.
        update_raster_effects(&dir, 0, "r", &[], None, None).unwrap();
        let loaded = load_page_rasters(&dir, None, 0).unwrap();
        let r = &loaded.layers[0];
        assert!(r.effects.is_empty(), "effects cleared");
        assert_eq!(r.image.size, [2, 2], "display back to the original base");
        assert_eq!(
            r.base_image.size, r.image.size,
            "with no effects, base_image equals the display"
        );

        // A pixels-dirty PS save bakes: writes a new base from the display, drops effects.
        update_raster_effects(&dir, 0, "r", &effects, Some(&img([4, 4], Color32::BLUE)), None).unwrap();
        save_page_rasters(
            &dir,
            0,
            &[RasterLayerOut {
                uid: "r".into(),
                name: "Pic".into(),
                visible: true,
                opacity: 1.0,
                transform: TransformRec { cx: 9.0, cy: 9.0, rotation: 0.0, scale: 1.0 },
                deform: None,
                group_uid: None,
                image: &img([4, 4], Color32::GREEN),
                pixels_dirty: true,
                mask_clip: None,
            }],
            &[],
            &[],
        )
        .unwrap();
        let loaded = load_page_rasters(&dir, None, 0).unwrap();
        let r = &loaded.layers[0];
        assert!(r.effects.is_empty(), "dirty save baked effects away");
        assert_eq!(r.image.size, [4, 4], "base is now the baked image");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ps_split_then_typing_effects_survive_a_clean_resave() {
        // Reproduces the reported bug end-to-end at the persist layer: paste an image (typing) → PS
        // cuts it into TWO rasters → return to typing and apply effects to both → save. The split
        // rasters are `pixels_dirty` from the cut, so PS writes their bases on the page-leave flush.
        // Once persisted, PS clears the dirty flag (`LayerStack::mark_rasters_persisted`), so the
        // SAVE-time flush re-saves them as CLEAN and must preserve the typing tab's effects + `_fx`
        // PNGs rather than rewriting the bases and dropping the chain.
        let dir = std::env::temp_dir().join(format!("ml_split_fx_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let tf = |cx: f32| TransformRec { cx, cy: 1.0, rotation: 0.0, scale: 1.0 };
        let base_a = img([2, 2], Color32::RED);
        let base_b = img([2, 2], Color32::BLUE);

        // PS page-leave flush: both halves are freshly cut, so both arrive `pixels_dirty` and their
        // bases are written. `pixels_dirty` toggles between the two flushes; everything else matches.
        let outs = |dirty: bool| {
            [
                RasterLayerOut {
                    uid: "a".into(),
                    name: "a".into(),
                    visible: true,
                    opacity: 1.0,
                    transform: tf(1.0),
                    deform: None,
                    group_uid: None,
                    image: &base_a,
                    pixels_dirty: dirty,
                    mask_clip: None,
                },
                RasterLayerOut {
                    uid: "b".into(),
                    name: "b".into(),
                    visible: true,
                    opacity: 1.0,
                    transform: tf(5.0),
                    deform: None,
                    group_uid: None,
                    image: &base_b,
                    pixels_dirty: dirty,
                    mask_clip: None,
                },
            ]
        };
        save_page_rasters(&dir, 0, &outs(true), &[], &[]).unwrap();

        // Typing applies (slightly different) effects to each half — non-destructive `_fx` writes.
        let fx_a = vec![serde_json::json!({"type":"shadow"})];
        let fx_b = vec![serde_json::json!({"type":"glow"})];
        update_raster_effects(&dir, 0, "a", &fx_a, Some(&img([4, 4], Color32::RED)), None).unwrap();
        update_raster_effects(&dir, 0, "b", &fx_b, Some(&img([4, 4], Color32::BLUE)), None).unwrap();

        // SAVE-time PS flush. The dirty flag was cleared after the first flush, so both halves are
        // CLEAN here (PS still holds their original cut pixels, not the `_fx` image).
        save_page_rasters(&dir, 0, &outs(false), &[], &[]).unwrap();

        // Both halves keep their effects, show the rendered image, and their `_fx` PNGs survive.
        let loaded = load_page_rasters(&dir, None, 0).unwrap();
        assert_eq!(loaded.layers.len(), 2, "both split halves persisted");
        for uid in ["a", "b"] {
            let r = loaded.layers.iter().find(|l| l.uid == uid).unwrap();
            assert_eq!(r.effects.len(), 1, "{uid}: effects survived the save-time flush");
            assert_eq!(r.image.size, [4, 4], "{uid}: display is the rendered (effected) image");
            assert!(
                dir.join(rendered_file_name(0, uid)).is_file(),
                "{uid}: rendered _fx PNG kept through prune"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn whole_page_save_preserves_unowned_rasters_and_drops_only_removed() {
        // Reproduces the "effects vanished on save to project" bug: the typing tab adds an effected
        // raster, then a PS-style whole-page save runs with a STALE/empty stack (the raster is not in
        // `layers`). The unowned raster — and its non-destructive effects — must survive; only a raster
        // listed in `removed_uids` is dropped.
        let dir = std::env::temp_dir().join(format!("ml_unowned_raster_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        // Typing adds an external image as a raster and applies effects (rendered PNG written).
        add_page_raster(
            &dir,
            None,
            0,
            "typing",
            "Картинка",
            true,
            1.0,
            TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
            &img([2, 2], Color32::RED),
        )
        .unwrap();
        let effects = vec![serde_json::json!({"type":"shadow"})];
        update_raster_effects(&dir, 0, "typing", &effects, Some(&img([4, 4], Color32::BLUE)), None).unwrap();

        // PS flush on "save to project" with a stale stack that knows nothing about the raster.
        save_page_rasters(&dir, 0, &[], &[], &[]).unwrap();
        let loaded = load_page_rasters(&dir, None, 0).unwrap();
        assert_eq!(loaded.layers.len(), 1, "unowned raster survived the whole-page save");
        let r = &loaded.layers[0];
        assert_eq!(r.uid, "typing");
        assert_eq!(r.effects.len(), 1, "effects preserved");
        assert_eq!(r.image.size, [4, 4], "still showing the rendered (effected) image");

        // The rendered PNG must also survive pruning so the display loads after a restart.
        assert!(
            dir.join(rendered_file_name(0, "typing")).is_file(),
            "rendered _fx PNG kept through prune"
        );

        // Now an explicit removal (PS deleted it) drops the node on the next save.
        save_page_rasters(&dir, 0, &[], &[], &["typing".to_string()]).unwrap();
        let loaded = load_page_rasters(&dir, None, 0).unwrap();
        assert!(loaded.layers.is_empty(), "explicitly removed raster dropped");
        assert!(
            !dir.join(rendered_file_name(0, "typing")).is_file(),
            "rendered PNG pruned after removal"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_round_trips_at_current_version_and_newer_reads_best_effort() {
        // Anchors the contract the future `compat` layer must preserve: a manifest we write carries
        // the current schema_version, reads back through `read_manifest` unchanged (an identity pass
        // at the current version), and a manifest tagged with a NEWER schema_version still parses
        // best-effort rather than erroring. This is the behavior Phase 1 moves into `compat.rs`.
        let dir = std::env::temp_dir().join(format!("ml_schema_ver_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let red = img([2, 2], Color32::RED);
        save_page_rasters(
            &dir,
            0,
            &[RasterLayerOut {
                uid: "r".into(),
                name: "R".into(),
                visible: true,
                opacity: 1.0,
                transform: TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
                deform: None,
                group_uid: None,
                image: &red,
                pixels_dirty: true,
                mask_clip: None,
            }],
            &[],
            &[],
        )
        .unwrap();

        let path = dir.join(MANIFEST_FILE);

        // Written at the current version.
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["schema_version"].as_u64().unwrap() as u32, LAYERS_SCHEMA_VERSION);

        // `read_manifest` returns it unchanged at the current version (identity pass).
        let m = read_manifest(&path).unwrap().unwrap();
        assert_eq!(m.schema_version, LAYERS_SCHEMA_VERSION);
        assert_eq!(m.pages.len(), 1);
        assert_eq!(m.pages[0].tree.len(), 1);
        assert_eq!(m.pages[0].tree[0].uid, "r");

        // A manifest tagged with a NEWER schema_version still parses best-effort (no error).
        let mut bumped = m;
        bumped.schema_version = LAYERS_SCHEMA_VERSION + 1;
        write_manifest(&path, &bumped).unwrap();
        let reread = read_manifest(&path).unwrap().unwrap();
        assert_eq!(reread.schema_version, LAYERS_SCHEMA_VERSION + 1);
        assert_eq!(reread.pages[0].tree[0].uid, "r", "newer file read best-effort");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn targeted_update_seeds_committed_page_so_edit_persists_to_unsaved() {
        // Repro of "scale/effects edited in typing not seen by PS/export": after "save to project"
        // the unsaved dir is gone and the raster lives ONLY in committed. A targeted update on the
        // unsaved dir would find no page and silently no-op — so the edit never reaches disk. With the
        // committed dir as `fallback_dir`, the page is seeded into the unsaved manifest and the edit
        // persists.
        let pid = std::process::id();
        let committed = std::env::temp_dir().join(format!("ml_seed_committed_{pid}"));
        let unsaved = std::env::temp_dir().join(format!("ml_seed_unsaved_{pid}"));
        let unsaved_nofb = std::env::temp_dir().join(format!("ml_seed_unsaved_nofb_{pid}"));
        for d in [&committed, &unsaved, &unsaved_nofb] {
            let _ = fs::remove_dir_all(d);
        }

        // A raster that exists only in the committed manifest, at scale 1.0.
        add_page_raster(
            &committed,
            None,
            0,
            "r",
            "R",
            true,
            1.0,
            TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.0 },
            &img([2, 2], Color32::RED),
        )
        .unwrap();

        // Typing edits scale → 1.5 via a targeted update on the (empty) unsaved dir, committed as fallback.
        update_raster_transform(
            &unsaved,
            0,
            "r",
            TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.5 },
            Some(committed.as_path()),
        )
        .unwrap();
        let loaded = load_page_rasters(&unsaved, Some(&committed), 0).unwrap();
        assert_eq!(loaded.layers.len(), 1);
        assert!(
            (loaded.layers[0].transform.scale - 1.5).abs() < 1e-6,
            "scale edit persisted to unsaved via fallback seeding"
        );

        // Without a fallback the edit can't reach the committed-only page (the bug): the reader still
        // sees the original scale 1.0.
        update_raster_transform(
            &unsaved_nofb,
            0,
            "r",
            TransformRec { cx: 1.0, cy: 1.0, rotation: 0.0, scale: 1.5 },
            None,
        )
        .unwrap();
        let loaded = load_page_rasters(&unsaved_nofb, Some(&committed), 0).unwrap();
        assert!(
            loaded
                .layers
                .iter()
                .all(|l| (l.transform.scale - 1.0).abs() < 1e-6),
            "no fallback → edit does not reach the committed raster"
        );

        for d in [&committed, &unsaved, &unsaved_nofb] {
            let _ = fs::remove_dir_all(d);
        }
    }
}
