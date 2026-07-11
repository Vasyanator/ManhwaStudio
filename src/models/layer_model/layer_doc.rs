/*
File: models/layer_model/layer_doc.rs

Purpose:
The foundational shared, in-memory, per-page layer document that both view tabs (the PS editor and
the typing tab) will eventually read and edit instead of their own separate caches. Both tabs always
display the SAME page (a shared page index), so this document is current-page-centric: pages are
loaded on demand and can be evicted.

A `LayerNode` is the unified notion of a layer — a raster node (pixels from any source) or a text
node (re-renderable from text params) — mirroring the on-disk `manifest::LayerKindRec` capability
model. This step (step 1) implements the types plus the RASTER node load / flush / in-memory ops,
backed by the existing `persist` functions. Text-node DISK loading is a follow-up (step 1b); the
`Text` node body type is defined now and exercised by a unit test so it is real, not dead code.

Persistence is via the existing `persist` API: `load_page_rasters` / `load_page_bands` on the way
in, `save_page_rasters` + `update_raster_effects` on the way out. The doc is authoritative over the
rasters it holds, so a flush writes the doc's effects chain (not merely whatever was on disk).

The doc exposes BOTH a synchronous flush (`flush_page` / `flush_page_text`, byte-stable, used by
tests and read-back paths) and an ADDITIVE async path: with an optional `LayerSaver` enabled
(`enable_background_saver`), `enqueue_page_save` / `enqueue_page_text_save` build an OWNED page job
(via `build_page_save_job`, cloning the same data the sync flush gathers) and hand it to the
coalescing background saver in `saver.rs`; without a saver they fall back to the synchronous flush.
The saver runs the identical `persist::*` write sequence off-thread, so on-disk bytes match the sync
flush. `Drop` shuts the saver down (flush queue + join).
*/

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use eframe::egui::ColorImage;
use serde_json::Value;

use super::manifest::{DeformRec, TransformRec};
use super::ordering::Band;
use super::persist::{self, GroupMeta, RasterLayerOut};
use super::saver::{
    EffectsSaveItem, LayerSaver, LayerSaverHandle, OwnedRasterLayer, OwnedTextNode, PageSaveJob,
    RasterSavePart, SaveAckMap, SaveKind, TextSavePart,
};
use super::text_payload;
use crate::trace::cat;

/// Decodes a PNG at `path` into an unmultiplied `ColorImage`, mirroring the raster load path's
/// decode. Returns `None` if the file is absent or undecodable.
fn read_png(path: &Path) -> Option<ColorImage> {
    if !path.is_file() {
        return None;
    }
    let rgba = image::open(path).ok()?.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// The kind of a unified layer node. Mirrors `manifest::LayerKindRec` (sans `Group`, which is a
/// container, not a node body here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Raster,
    Text,
}

/// The body of a layer node, carrying the kind-specific pixels and payload.
pub enum NodeBody {
    /// A raster node: pre-effects `base_image`, post-effects `display_image` (equal to the base when
    /// there are no effects), the opaque effects chain, the base PNG filename on disk, and the
    /// persisted mask-clip flag.
    Raster {
        base_image: ColorImage,
        display_image: ColorImage,
        effects: Vec<Value>,
        base_file: String,
        /// Persisted mask-clip flag (typing tab): whether the raster is clipped to the page mask.
        /// `None`/`Some(false)` ⇒ no clip (rasters default OFF). Round-trips through `LayerRec.mask_clip`.
        mask_clip: Option<bool>,
    },
    /// A text node: its render params (`render_data`, opaque here), the rendered `image`, the uid of
    /// the payload it references in the text store, and the persisted mask-clip flag. `is_image == true`
    /// marks a placed PNG image overlay: `render_data` is `Value::Null` and it is not re-renderable. The text flush
    /// (`LayerDoc::flush_page`) writes these inline into `layers.json` (schema v3); the load builds
    /// them from the inline node when present, else from the legacy `text_info.json` overlay entry.
    Text {
        render_data: Value,
        image: ColorImage,
        is_image: bool,
        /// The uid of the overlay payload this text node renders from (its `payload_ref` uid, or the
        /// node uid for a v3 inline node). Written back as the node's `payload_uid` on flush.
        payload_uid: String,
        /// Persisted `mask_clip_enabled` (typing tab): whether the text is clipped to its mask. `None`
        /// ⇒ default. Round-trips through the v3 inline payload.
        mask_clip: Option<bool>,
    },
}

/// One unified layer node, resident in memory for a loaded page.
pub struct LayerNode {
    pub uid: String,
    pub name: String,
    pub kind: NodeKind,
    /// Unified Z, bottom-to-top.
    pub z: u32,
    pub visible: bool,
    pub opacity: f32,
    pub group_uid: Option<String>,
    /// TEXT node only: the typing tab's «Группа текста N» grouping axis (`layer_idx`). Carried on the
    /// node so a doc text flush persists it for NEW overlays too (the persist layer separately preserves
    /// it for existing nodes). `None` for rasters and for text nodes whose grouping is unknown (flush
    /// then defaults to the persisted value, else 0). Orthogonal to the PS `group_uid`.
    pub text_layer_idx: Option<u32>,
    pub transform: TransformRec,
    pub deform: Option<DeformRec>,
    /// Bumped whenever pixels change, so the GPU texture cache can invalidate.
    pub generation: u64,
    /// The raster base pixels were edited this session (paint / cut / merge / bake), so a flush must
    /// rewrite the base PNG and drop any non-destructive effects chain.
    pub pixels_dirty: bool,
    pub body: NodeBody,
}

impl LayerNode {
    #[must_use]
    pub fn is_raster(&self) -> bool {
        matches!(self.kind, NodeKind::Raster)
    }

    // Test-only accessors: both tabs read `NodeBody` by pattern match rather than via these helpers,
    // so they exist only in test builds (avoiding a dead-code allow in the production build).
    #[cfg(test)]
    #[must_use]
    pub fn is_text(&self) -> bool {
        matches!(self.kind, NodeKind::Text)
    }

    /// The image a tab should display: the raster's post-effects render, or the text's rendered image.
    #[cfg(test)]
    #[must_use]
    pub fn display_image(&self) -> &ColorImage {
        match &self.body {
            NodeBody::Raster { display_image, .. } => display_image,
            NodeBody::Text { image, .. } => image,
        }
    }

    /// Bumps the GPU-cache invalidation counter (call after changing pixels / display image).
    pub fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        crate::trace_log!(
            cat::SYNC,
            "bump_generation uid={} generation={}",
            self.uid,
            self.generation
        );
    }
}

/// One resident page: its nodes (sorted bottom-to-top by `z`) and its raster-layer groups.
pub struct DocPage {
    pub nodes: Vec<LayerNode>,
    pub groups: Vec<GroupMeta>,
}

/// An OWNED, fully-decoded page payload built off any doc lock by [`LayerDoc::decode_page_payload`].
///
/// It is the exact set of values `ensure_page_loaded` produces BEFORE inserting them into
/// `LayerDoc::pages`: the page's raster + text `LayerNode`s (already sorted bottom-to-top and
/// re-ranked to unique contiguous `z`) and its raster-layer groups. All disk I/O and PNG decode that
/// produced it happened WITHOUT holding the doc lock; moving it into the doc is a cheap, lock-brief
/// insert (`insert_decoded_page`). `LayerNode`/`ColorImage`/`Value` are `Send`, so a payload can be
/// produced on a worker thread and inserted on the GUI thread.
#[must_use]
pub struct DecodedPagePayload {
    pub nodes: Vec<LayerNode>,
    pub groups: Vec<GroupMeta>,
}

// Compact, pixel-free Debug so a `LoadedPage`/`PageLoadResult` carrying a payload stays `Debug`
// (the page-loader worker structs derive it) without dumping multi-MB image buffers into logs.
impl std::fmt::Debug for DecodedPagePayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedPagePayload")
            .field("nodes", &self.nodes.len())
            .field("groups", &self.groups.len())
            .finish()
    }
}

/// The shared in-memory layer document: pages loaded on demand, keyed by page index.
pub struct LayerDoc {
    pages: HashMap<usize, DocPage>,
    /// Monotonic version, bumped by EVERY mutating operation (page load/evict, node/group adds and
    /// removes, transform/effects/pixels/text/visibility/opacity/group/reorder edits). Both view tabs
    /// store the last version they projected and re-project their current page whenever it changes;
    /// this is the in-memory cross-tab sync (replacing the old disk-revision bridge).
    version: u64,
    /// Optional background saver (additive). When present, `enqueue_page_save` /
    /// `enqueue_page_text_save` route page persistence off-thread (coalescing); when absent (the
    /// default — e.g. unit tests that construct a bare `LayerDoc`), those entry points fall back to the
    /// synchronous `flush_page` / `flush_page_text`. Created lazily via `enable_background_saver` so a
    /// constructed-and-dropped doc spawns no thread.
    saver: Option<LayerSaver>,
    save_epoch: u64,
    latest_epoch: HashMap<(usize, SaveKind), u64>,
    save_acks: Option<Arc<Mutex<SaveAckMap>>>,
}

impl Default for LayerDoc {
    fn default() -> Self {
        Self::new()
    }
}

impl LayerDoc {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pages: HashMap::new(),
            version: 0,
            saver: None,
            save_epoch: 0,
            latest_epoch: HashMap::new(),
            save_acks: None,
        }
    }

    /// The current document version. A tab compares this against its last-projected value each frame
    /// and re-projects the current page when it differs (cheap: a lock + a `u64` read).
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Bumps the document version. Called by every mutator so cross-tab listeners re-project.
    fn bump_version(&mut self) {
        self.version = self.version.wrapping_add(1);
        crate::trace_log!(cat::SYNC, "bump_version version={}", self.version);
    }

    /// Explicitly bumps the version. For callers that mutate node fields directly via `node_mut`
    /// (bypassing the version-bumping mutators) and still need cross-tab listeners to re-project.
    pub fn mark_changed(&mut self) {
        crate::trace_log!(cat::SYNC, "mark_changed version_before={}", self.version);
        self.bump_version();
    }

    /// Loads `page_idx` into memory if not already resident.
    ///
    /// Convenience composition of [`Self::decode_page_payload`] (disk I/O + PNG decode + legacy
    /// migration, lock-free) and [`Self::insert_decoded_page`] (the cheap memoized move).
    ///
    /// TEST-ONLY reference implementation: every production caller now runs the two steps explicitly so
    /// the multi-MB PNG decode happens off the shared doc lock (a worker in the PS page loader, the GUI
    /// thread before locking in the typing tab). This monolith is retained only as the differential
    /// oracle the split path is unit-tested against (see
    /// `decode_then_insert_matches_ensure_page_loaded_incl_legacy_ribbon`); keeping it out of production
    /// prevents a future caller from reintroducing a decode under the lock.
    ///
    /// See [`Self::decode_page_payload`] for the full decode contract (raster + text node build,
    /// unified-Z re-rank, the FULL-`page_sizes`-map ribbon-migration requirement).
    ///
    /// # Errors
    /// Propagates [`Self::decode_page_payload`]'s error string.
    #[cfg(test)]
    pub fn ensure_page_loaded(
        &mut self,
        page_idx: usize,
        primary_dir: &Path,
        fallback_dir: Option<&Path>,
        legacy_text_dir: Option<&Path>,
        page_sizes: &HashMap<usize, [usize; 2]>,
    ) -> Result<(), String> {
        if self.pages.contains_key(&page_idx) {
            return Ok(());
        }
        // Decode off the (here non-existent) lock, then move the payload in. Identical behavior to the
        // pre-split monolith; the split lets a worker run `decode_page_payload` lock-free and the GUI
        // thread run only the cheap `insert_decoded_page`.
        let payload =
            Self::decode_page_payload(page_idx, primary_dir, fallback_dir, legacy_text_dir, page_sizes)?;
        self.insert_decoded_page(page_idx, payload);
        Ok(())
    }

    /// Inserts an already-decoded page payload into the doc, taking effect only if `page_idx` is not
    /// already resident (memoization: a live page's in-memory edits are NEVER clobbered by a stale
    /// background decode). On a real insert the document `version` is bumped so both tabs re-project.
    ///
    /// This is the ONLY lock-held step of a page load: it moves an OWNED [`DecodedPagePayload`] (built
    /// lock-free by [`Self::decode_page_payload`]) into `self.pages`. No disk I/O or PNG decode happens
    /// here, so a shared `Arc<Mutex<LayerDoc>>` is held only for this cheap move.
    pub fn insert_decoded_page(&mut self, page_idx: usize, payload: DecodedPagePayload) {
        if self.pages.contains_key(&page_idx) {
            // Already loaded (e.g. a concurrent sync load, or the page was edited between the
            // background decode request and this insert): discard the freshly-decoded payload so live
            // in-memory edits survive. Mirrors `ensure_page_loaded`'s memoization no-op.
            crate::trace_log!(
                cat::PERSIST,
                "insert_decoded_page page={} already resident; discarding decoded payload",
                page_idx
            );
            return;
        }
        crate::trace_log!(
            cat::PERSIST,
            "insert_decoded_page page={} nodes={} groups={}",
            page_idx,
            payload.nodes.len(),
            payload.groups.len()
        );
        self.pages.insert(
            page_idx,
            DocPage {
                nodes: payload.nodes,
                groups: payload.groups,
            },
        );
        self.bump_version();
    }

    /// Decodes a page's full layer payload from disk WITHOUT touching `LayerDoc` state, so it can run
    /// off the GUI thread (the page loader worker) or under no lock. It performs ALL the disk I/O and
    /// PNG decode + legacy migration that `ensure_page_loaded` used to do inline, and returns the OWNED
    /// [`DecodedPagePayload`] to hand to [`Self::insert_decoded_page`].
    ///
    /// `legacy_text_dir` is the un-migrated legacy `text_images/` directory, appended as the LAST text
    /// source (after `primary_dir` and `fallback_dir`), or `None` once the chapter's inline manifest text
    /// has migrated. It feeds BOTH the `text_info.json` read and the overlay-PNG lookups, so a
    /// never-migrated legacy chapter's text becomes visible in the shared doc (hence the PS editor).
    /// A uid-less legacy entry receives a deterministic uid via [`text_payload::stable_overlay_uid`],
    /// matching the typing loader so the same overlay never double-renders.
    ///
    /// `page_sizes` MUST be the FULL chapter page-size map (`page_idx -> [w, h]`): the absolute-ribbon
    /// legacy migration recovers a chapter-wide ribbon scale from EVERY page's aspect ratio, so a
    /// partial map corrupts the geometry of legacy chapters. The loaded page's own size is
    /// `page_sizes[page_idx]` (defaulting to `[1, 1]` when absent).
    ///
    /// # Errors
    /// Returns the persist-layer error string if `load_page_rasters` or `load_page_text_nodes` fails.
    pub fn decode_page_payload(
        page_idx: usize,
        primary_dir: &Path,
        fallback_dir: Option<&Path>,
        legacy_text_dir: Option<&Path>,
        page_sizes: &HashMap<usize, [usize; 2]>,
    ) -> Result<DecodedPagePayload, String> {
        let _span = crate::trace_scope!(cat::PERSIST, "decode_page_payload page={}", page_idx);
        // Pixel size of the page being loaded (page-relative uv → page px). The FULL chapter map is
        // required for the cross-entry ribbon migration below — see the comment there.
        let page_size = page_sizes.get(&page_idx).copied().unwrap_or([1, 1]);

        let rasters = persist::load_page_rasters(primary_dir, fallback_dir, page_idx)?;
        let bands = persist::load_page_bands(primary_dir, fallback_dir, page_idx);
        crate::trace_log!(
            cat::PERSIST,
            "decode_page_payload page={} rasters={} groups={} bands={}",
            page_idx,
            rasters.layers.len(),
            rasters.groups.len(),
            bands.len()
        );

        // uid -> band z, for the raster bands.
        let mut z_by_uid: HashMap<String, u32> = HashMap::new();
        for band in &bands {
            if let Band::Raster { uid, z } = band {
                z_by_uid.insert(uid.clone(), *z);
            }
        }

        let mut nodes: Vec<LayerNode> = Vec::with_capacity(rasters.layers.len());
        for (idx, layer) in rasters.layers.into_iter().enumerate() {
            let z = z_by_uid.get(&layer.uid).copied().unwrap_or(idx as u32);
            nodes.push(LayerNode {
                uid: layer.uid,
                name: layer.name,
                kind: NodeKind::Raster,
                z,
                visible: layer.visible,
                opacity: layer.opacity,
                group_uid: layer.group_uid,
                text_layer_idx: None,
                transform: layer.transform,
                deform: layer.deform,
                generation: 0,
                pixels_dirty: false,
                body: NodeBody::Raster {
                    base_image: layer.base_image,
                    display_image: layer.image,
                    effects: layer.effects,
                    base_file: layer.base_file,
                    mask_clip: layer.mask_clip,
                },
            });
        }
        // Text nodes: metadata comes from `layers.json` text nodes; geometry/pixels/render params
        // come from the `text_info.json` overlay payload keyed by the same uid.
        let mut text_dirs: Vec<&Path> = vec![primary_dir];
        if let Some(fb) = fallback_dir {
            text_dirs.push(fb);
        }
        // The un-migrated legacy `text_images/` dir is the LAST text source: a migrated chapter whose
        // `text_info.json` lives in `layers/` still wins over any stale `text_images/text_info.json`,
        // while a never-migrated chapter (which only has `text_images/`) still resolves its overlays.
        if let Some(legacy) = legacy_text_dir {
            text_dirs.push(legacy);
        }
        // Normalize the cross-entry legacy families (ribbon x/y, top-left u/v) to modern
        // `img_u`/`img_v` BEFORE per-entry decode, identically to the typing tab's loader — so an old
        // chapter resolves the same in both tabs and migrates to the inline v3 payload without snapping
        // text to page-center. The absolute-ribbon family recovers the chapter's SHARED ribbon scale
        // from every page's aspect ratio, so the caller must pass the FULL chapter page-size map (not
        // just this page) — otherwise the missing pages default to a square aspect and the solve, hence
        // every page's resulting `img_u`/`img_v`, is wrong. The PNG footprint (top-left case) is
        // resolved from the text dirs.
        let raw_entries = text_payload::read_overlay_entries(&text_dirs);
        let overlay_entries =
            text_payload::migrate_overlay_entries(&raw_entries, page_sizes, |obj| {
                obj.get("file")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .and_then(|file| {
                        text_dirs.iter().find_map(|dir| {
                            image::image_dimensions(dir.join(file))
                                .ok()
                                .map(|(w, h)| (w as f32, h as f32))
                        })
                    })
                    .unwrap_or((0.0, 0.0))
            });

        // Pinned-text band Z by uid; text-group band Z by layer_idx — for the unified text node Z.
        let mut pinned_z_by_uid: HashMap<String, u32> = HashMap::new();
        let mut group_z_by_layer_idx: HashMap<u32, u32> = HashMap::new();
        for band in &bands {
            match band {
                Band::PinnedText { uid, z } => {
                    pinned_z_by_uid.insert(uid.clone(), *z);
                }
                Band::TextGroup { layer_idx, z, .. } => {
                    group_z_by_layer_idx.insert(*layer_idx, *z);
                }
                Band::Raster { .. } => {}
            }
        }
        let fallback_text_z = bands.len() as u32;

        // Overlay sources are unioned by uid: a schema-v3 `layers.json` text node carries its FULL
        // inline payload (render_data + geometry + rendered PNG + mask_clip) and is self-sufficient —
        // it builds the text node directly and WINS over any legacy entry of the same uid. A legacy
        // (v2) node carries only PS-owned overrides (visibility / opacity / group / pin-Z); its
        // authoritative payload still comes from the `text_info.json` overlay entry. So: build every
        // inline node first, then iterate the overlay entries skipping uids already built inline. This
        // keeps old chapters (no inline payload) working while letting v3 chapters list+build text
        // without `text_info.json`. (Iterating only the nodes would drop every plain legacy overlay.)
        let node_meta: HashMap<String, persist::TextNodeIn> =
            persist::load_page_text_nodes(primary_dir, fallback_dir, page_idx)?
                .into_iter()
                .map(|n| (n.uid.clone(), n))
                .collect();
        let mut texts: Vec<LayerNode> = Vec::new();
        let mut built_inline: HashMap<String, ()> = HashMap::new();
        let (mut text_n, mut image_n) = (0usize, 0usize);

        // 1) v3 inline text nodes: build straight from `layers.json` (no `text_info.json` needed).
        for meta in node_meta.values() {
            let Some(inline) = &meta.inline else {
                continue;
            };
            // The rendered PNG is named by `rendered_file`; decode it from the layers dir(s). A node
            // with no rendered image yet (None / missing PNG) is skipped with a warning, mirroring the
            // legacy path's "missing overlay image" handling.
            let image = inline
                .rendered_file
                .as_deref()
                .and_then(|f| text_dirs.iter().find_map(|dir| read_png(&dir.join(f))));
            let Some(image) = image else {
                crate::runtime_log::log_warn(format!(
                    "[layer_doc] inline text node '{}' page {page_idx} has no rendered image; skipping",
                    meta.uid
                ));
                continue;
            };
            let layer_idx = meta.layer_idx;
            // Round-trip the persisted node name (stable across reloads). Fall back to a generated
            // "Текст {n}" only when the stored name is empty (e.g. an older inline node).
            let name = if meta.name.trim().is_empty() {
                text_n += 1;
                format!("Текст {text_n}")
            } else {
                meta.name.clone()
            };
            let z = pinned_z_by_uid
                .get(&meta.uid)
                .copied()
                .or_else(|| group_z_by_layer_idx.get(&layer_idx).copied())
                .unwrap_or(fallback_text_z);
            // Geometry from the inline node; fall back to image-centered affine if absent.
            let transform = inline.transform.unwrap_or(TransformRec {
                cx: image.size[0] as f32 * 0.5,
                cy: image.size[1] as f32 * 0.5,
                rotation: 0.0,
                scale: 1.0,
            });
            texts.push(LayerNode {
                uid: meta.uid.clone(),
                name,
                kind: NodeKind::Text,
                z,
                visible: meta.visible,
                opacity: meta.opacity,
                group_uid: meta.group_uid.clone(),
                text_layer_idx: Some(layer_idx),
                transform,
                deform: inline.deform.clone(),
                generation: 0,
                pixels_dirty: false,
                body: NodeBody::Text {
                    render_data: inline.render_data.clone(),
                    image,
                    is_image: inline.is_image,
                    payload_uid: meta.payload_uid.clone(),
                    mask_clip: inline.mask_clip,
                },
            });
            built_inline.insert(meta.uid.clone(), ());
        }

        // A page is MIGRATED once it carries any inline text node: `write_page_text_payload` always
        // writes the page's FULL text set inline at once, so "any inline node" ⇒ "all text is inline".
        // For a migrated page the legacy `text_info.json` is ignored entirely — otherwise an overlay
        // that was DELETED (or rasterized) from the inline set would resurrect from the stale legacy
        // file. A page with no inline nodes is still pure-legacy and reads `text_info.json` below.
        let page_is_migrated = !built_inline.is_empty();
        let legacy_entries: &[Value] = if page_is_migrated {
            &[]
        } else {
            &overlay_entries
        };

        // 2) Legacy overlays from `text_info.json` (migration-on-read), skipping uids already inline.
        for entry in legacy_entries {
            let Some(obj) = entry.as_object() else {
                continue;
            };
            // Entries without `img_idx` default to page 0 (mirrors the legacy PS overlay loader).
            let entry_page = obj.get("img_idx").and_then(Value::as_u64).unwrap_or(0) as usize;
            if entry_page != page_idx {
                continue;
            }
            // `file` is read FIRST: it seeds the deterministic uid for uid-less legacy entries and is
            // reused below for the PNG read. Trimmed to match the typing codec's `str::trim`
            // (`normalize_overlay_storage_entry` / `decode_overlay_from_storage_entry`), so a name with
            // surrounding whitespace still resolves its PNG AND seeds the SAME uid on both paths.
            let Some(file) = obj
                .get("file")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            // Legacy `text_info.json` has no `uid` for pre-uid overlays; mint one deterministically from
            // the PNG file name (matching the typing loader) instead of dropping the entry.
            let uid = obj
                .get("uid")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| text_payload::stable_overlay_uid(file));
            // A v3 inline node of the same uid already won; skip the legacy entry.
            if built_inline.contains_key(&uid) {
                continue;
            }
            let Some(image) = text_dirs.iter().find_map(|dir| read_png(&dir.join(file))) else {
                crate::runtime_log::log_warn(format!(
                    "[layer_doc] text overlay image '{file}' not found for page {page_idx}; skipping"
                ));
                continue;
            };
            let layer_idx = obj.get("layer_idx").and_then(Value::as_u64).unwrap_or(0) as u32;
            let is_image = obj.get("overlay_type").and_then(Value::as_str) == Some("image");
            let meta = node_meta.get(&uid);
            // Prefer the persisted layers.json node name (so it round-trips, F4); fall back to a
            // generated "Текст/Картинка {n}" only when there is no node name. The counters advance
            // regardless so generated fallbacks stay unique.
            let generated = if is_image {
                image_n += 1;
                format!("Картинка {image_n}")
            } else {
                text_n += 1;
                format!("Текст {text_n}")
            };
            let name = meta
                .map(|m| m.name.clone())
                .filter(|n| !n.trim().is_empty())
                .unwrap_or(generated);
            // Mirror the typing tab's `overlay_band_z`: pinned-text band Z, else text-group band Z by
            // layer_idx, else the past-the-top fallback.
            let z = pinned_z_by_uid
                .get(&uid)
                .copied()
                .or_else(|| group_z_by_layer_idx.get(&layer_idx).copied())
                .unwrap_or(fallback_text_z);
            let visible = meta.is_none_or(|m| m.visible);
            let opacity = meta.map_or(1.0, |m| m.opacity);
            let group_uid = meta.and_then(|m| m.group_uid.clone());
            // Decode legacy geometry through the shared codec (page-relative uv → page px needs the
            // PAGE size, not the overlay PNG size). `_image` size is only used as the render image.
            let placement = text_payload::decode_overlay_placement(obj, page_size);
            let render_data = obj.get("render_data").cloned().unwrap_or(Value::Null);
            // Carry the legacy chapter's clip flag so it is not lost on migration-on-read; it then
            // persists into the inline payload on the next flush.
            let mask_clip = obj.get("mask_clip_enabled").and_then(Value::as_bool);

            texts.push(LayerNode {
                uid: uid.clone(),
                name,
                kind: NodeKind::Text,
                z,
                visible,
                opacity,
                group_uid,
                text_layer_idx: Some(layer_idx),
                transform: placement.transform,
                deform: placement.deform,
                generation: 0,
                pixels_dirty: false,
                body: NodeBody::Text {
                    render_data,
                    image,
                    is_image,
                    payload_uid: uid,
                    mask_clip,
                },
            });
        }

        // Rasters first, then texts, then a STABLE sort by z: this keeps a raster before a text at the
        // same z, matching the live render tiebreak.
        crate::trace_log!(
            cat::PERSIST,
            "decode_page_payload page={} text_nodes={} inline_built={} migrated={}",
            page_idx,
            texts.len(),
            built_inline.len(),
            page_is_migrated
        );
        nodes.extend(texts);

        // FULLY-MANUAL UNIFIED Z (retire auto-Y): every node — raster AND text — gets a UNIQUE explicit
        // band-Z. Legacy text-group members loaded above all share their group's band Z (a collision);
        // flatten them here, ON READ, into per-text bands in the SAME visual order the group produced —
        // ascending page-Y within the group (lower on the page = higher in the stack, matching the old
        // `overlay_stack_cmp` tiebreak). Order is PRESERVED exactly; each text just becomes individually
        // movable. The doc flush then persists each text pinned at its own Z (see `write_page_text` +
        // `write_page_text_payload`), so groups dissolve on the next save. Idempotent: once every node has
        // a distinct Z this is a stable no-op re-rank.
        nodes.sort_by(|a, b| {
            a.z.cmp(&b.z)
                // Same band Z: rasters below texts (matches the live render `(z, kind)` tiebreak)...
                .then_with(|| {
                    let rank = |n: &LayerNode| matches!(n.kind, NodeKind::Raster) as u8;
                    rank(a).cmp(&rank(b))
                })
                // ...then by text-group (`layer_idx`) so a degenerate equal-band-Z manifest with MULTIPLE
                // groups still reproduces the old `(layer_idx, page-Y)` sub-order exactly (insurance; not
                // user-reachable since distinct groups normally carry distinct band Z)...
                .then_with(|| a.text_layer_idx.cmp(&b.text_layer_idx))
                // ...and finally within a group (same Z, same layer_idx), order by page-Y (ascending →
                // lower-on-page higher in the stack, the legacy `overlay_stack_cmp` sub-order).
                .then_with(|| a.transform.cy.total_cmp(&b.transform.cy))
        });
        for (i, node) in nodes.iter_mut().enumerate() {
            node.z = i as u32;
        }

        crate::trace_log!(
            cat::PERSIST,
            "decode_page_payload page={} total_nodes={} (final z-order assigned)",
            page_idx,
            nodes.len()
        );
        Ok(DecodedPagePayload {
            nodes,
            groups: rasters.groups,
        })
    }

    #[must_use]
    pub fn page(&self, page_idx: usize) -> Option<&DocPage> {
        self.pages.get(&page_idx)
    }

    /// Returns whether `page_idx` has ANY persistence that is enqueued/edited but not yet
    /// acknowledged-durable — the correct "do not evict, an edit could be lost" signal.
    ///
    /// True iff `latest_epoch` holds an entry for `(page_idx, Raster)` or `(page_idx, Text)`, OR the
    /// resident page still has a `pixels_dirty` node. A `latest_epoch` entry is created the moment a
    /// page is touched — by a pixel edit (`bump_dirty_epoch`) OR by ANY persisting enqueue
    /// (`next_save_epoch`, which every whole-page/text/effects save reserves, so GEOMETRY-only edits
    /// like `set_transform`/`set_deform`/visibility/opacity/group that persist via a whole-page
    /// enqueue are covered too) — and is removed only by a matching successful ack in `poll_save_acks`
    /// or by `evict_page`. The `pixels_dirty` fallback is belt-and-suspenders for the brief window
    /// between a pixel edit and its enqueue. Lock-free and cheap: only `HashMap`/slice lookups.
    #[must_use]
    pub fn page_has_pending_save(&self, page_idx: usize) -> bool {
        if self
            .latest_epoch
            .contains_key(&(page_idx, SaveKind::Raster))
            || self.latest_epoch.contains_key(&(page_idx, SaveKind::Text))
        {
            return true;
        }
        self.pages
            .get(&page_idx)
            .is_some_and(|page| page.nodes.iter().any(|node| node.pixels_dirty))
    }

    /// Page indices currently resident (loaded) in the doc. A resident page had its text loaded by
    /// `ensure_page_loaded` (both tabs load text on page load), so the doc's view of that page's text
    /// is authoritative — including deletions. The save-to-project flush iterates these to make staging
    /// text-complete for OWNED pages, and the unsaved→committed merge treats them as owned (whole-page
    /// replace); a page NOT resident was never loaded this session, so the merge preserves its committed
    /// text instead of dropping it.
    #[must_use]
    pub fn resident_pages(&self) -> Vec<usize> {
        self.pages.keys().copied().collect()
    }

    /// Drops a page from memory (e.g. when the shared page index moves away).
    pub fn evict_page(&mut self, page_idx: usize) {
        let evicted = self.pages.remove(&page_idx).is_some();
        // Drop the page's pending save epochs together with its memory: an evicted page has no in-memory
        // dirty left to settle, and a page later re-inserted must NOT be settled by a pre-eviction ack (an
        // in-flight ack whose epoch still equaled a lingering `latest_epoch` would otherwise clear the
        // reloaded page's dirty). Removing the entries also stops the map growing unbounded across a long
        // session of page visits.
        self.latest_epoch.remove(&(page_idx, SaveKind::Raster));
        self.latest_epoch.remove(&(page_idx, SaveKind::Text));
        crate::trace_log!(
            cat::LAYER_MODEL,
            "evict_page page={} evicted={}",
            page_idx,
            evicted
        );
        if evicted {
            self.bump_version();
        }
    }

    // Test-only read accessor; production code reads nodes via `page().nodes`. Exists only in test
    // builds so it needs no dead-code allow in the production build.
    #[cfg(test)]
    #[must_use]
    pub fn node(&self, page_idx: usize, uid: &str) -> Option<&LayerNode> {
        self.pages
            .get(&page_idx)?
            .nodes
            .iter()
            .find(|n| n.uid == uid)
    }

    pub fn node_mut(&mut self, page_idx: usize, uid: &str) -> Option<&mut LayerNode> {
        self.pages
            .get_mut(&page_idx)?
            .nodes
            .iter_mut()
            .find(|n| n.uid == uid)
    }

    /// Clones the pre-effects base pixels of a resident raster node, if one exists at
    /// `(page_idx, uid)`. The effects worker prefers this in-memory source over the on-disk base PNG
    /// so a freshly created raster (not yet flushed to disk) can still be effected. `None` when the
    /// page is not resident, the uid is absent, or the node is not a raster.
    #[must_use]
    pub fn raster_base_image(&self, page_idx: usize, uid: &str) -> Option<ColorImage> {
        let node = self
            .pages
            .get(&page_idx)?
            .nodes
            .iter()
            .find(|n| n.uid == uid)?;
        match &node.body {
            NodeBody::Raster { base_image, .. } => Some(base_image.clone()),
            NodeBody::Text { .. } => None,
        }
    }

    /// Sets a node's affine placement. Geometry only — no generation bump (pixels are unchanged).
    pub fn set_transform(&mut self, page_idx: usize, uid: &str, transform: TransformRec) {
        crate::trace_log!(
            cat::LAYER_MODEL,
            "set_transform page={} uid={} cx={} cy={} rot={} scale={}",
            page_idx,
            uid,
            transform.cx,
            transform.cy,
            transform.rotation,
            transform.scale
        );
        if let Some(node) = self.node_mut(page_idx, uid) {
            node.transform = transform;
            self.bump_version();
        }
    }

    /// Sets (or clears) a node's mesh-deform grid. Geometry only — no generation bump (pixels are
    /// unchanged); bumps the version so cross-tab listeners re-project. Applies to raster and text
    /// nodes alike: when `Some`, the node is positioned by the mesh and its affine transform no
    /// longer applies (mirrors the deformed-text rendering rule).
    pub fn set_deform(&mut self, page_idx: usize, uid: &str, deform: Option<DeformRec>) {
        crate::trace_log!(
            cat::LAYER_MODEL,
            "set_deform page={} uid={} has_deform={}",
            page_idx,
            uid,
            deform.is_some()
        );
        if let Some(node) = self.node_mut(page_idx, uid) {
            node.deform = deform;
            self.bump_version();
        }
    }

    /// Sets a RASTER node's mask-clip flag and bumps its generation (so the projecting tab re-clips and
    /// re-uploads its texture). No-op if the page/node is absent or the node is not a raster. Persisted
    /// on `flush_page` via `LayerRec.mask_clip`.
    pub fn set_raster_mask_clip(&mut self, page_idx: usize, uid: &str, mask_clip: Option<bool>) {
        crate::trace_log!(
            cat::LAYER_MODEL,
            "set_raster_mask_clip page={} uid={} mask_clip={:?}",
            page_idx,
            uid,
            mask_clip
        );
        if let Some(node) = self.node_mut(page_idx, uid)
            && let NodeBody::Raster { mask_clip: m, .. } = &mut node.body
        {
            *m = mask_clip;
            node.bump_generation();
            self.bump_version();
        }
    }

    /// Replaces a raster node's effects chain and post-effects display image, bumping its generation.
    /// Pure in-memory; persistence happens on `flush_page`.
    pub fn set_effects(
        &mut self,
        page_idx: usize,
        uid: &str,
        effects: Vec<Value>,
        display_image: ColorImage,
    ) {
        crate::trace_log!(
            cat::LAYER_MODEL,
            "set_effects page={} uid={} effects_len={} img={}x{}",
            page_idx,
            uid,
            effects.len(),
            display_image.size[0],
            display_image.size[1]
        );
        let mut changed = false;
        if let Some(node) = self.node_mut(page_idx, uid)
            && let NodeBody::Raster {
                effects: e,
                display_image: d,
                ..
            } = &mut node.body
        {
            *e = effects;
            *d = display_image;
            node.bump_generation();
            self.bump_version();
            changed = true;
        }
        if changed {
            self.bump_dirty_epoch(page_idx, SaveKind::Raster);
        }
    }

    /// Adds a node to a resident page on top of the stack: its `z` becomes one above the current
    /// maximum (or 0 if the page is empty). Re-sorts `nodes` by `z`. Returns false if the page is not
    /// resident.
    pub fn add_node(&mut self, page_idx: usize, mut node: LayerNode) -> bool {
        let Some(page) = self.pages.get_mut(&page_idx) else {
            crate::trace_log!(
                cat::LAYER_MODEL,
                "add_node page={} uid={} result=false (page not resident)",
                page_idx,
                node.uid
            );
            return false;
        };
        let top_z = page.nodes.iter().map(|n| n.z).max();
        node.z = top_z.map_or(0, |z| z + 1);
        crate::trace_log!(
            cat::LAYER_MODEL,
            "add_node page={} uid={} kind={:?} z={} count={}",
            page_idx,
            node.uid,
            node.kind,
            node.z,
            page.nodes.len() + 1
        );
        // Capture the node's dirty kind BEFORE moving it into the page: a pushed node that carries
        // `pixels_dirty` (a fresh text overlay with no on-disk PNG, or a PS undo re-add of a raster whose
        // base PNG was pruned) must advance its `(page, kind)` dirty epoch, upholding the invariant that
        // every site making a doc node dirty bumps the epoch — so no stale-but-current-epoch ack settles it.
        let dirty_kind = node.pixels_dirty.then_some(node.kind);
        page.nodes.push(node);
        page.nodes.sort_by_key(|n| n.z);
        self.bump_version();
        if let Some(kind) = dirty_kind {
            let save_kind = match kind {
                NodeKind::Raster => SaveKind::Raster,
                NodeKind::Text => SaveKind::Text,
            };
            self.bump_dirty_epoch(page_idx, save_kind);
        }
        true
    }

    /// Adds a node to a resident page at an EXPLICIT `z`, re-sorting `nodes` bottom-to-top. Unlike
    /// [`Self::add_node`] (which always places the node on top), this restores a node at a precise Z —
    /// used by the PS-editor undo/redo to re-insert a previously deleted layer at its exact prior
    /// position. The caller is responsible for `z` not colliding with an existing node's (removal
    /// never renumbers, so an immediate undo re-inserts into a still-free slot). Returns false if the
    /// page is not resident.
    pub fn add_node_at_z(&mut self, page_idx: usize, mut node: LayerNode, z: u32) -> bool {
        let Some(page) = self.pages.get_mut(&page_idx) else {
            crate::trace_log!(
                cat::LAYER_MODEL,
                "add_node_at_z page={} uid={} result=false (page not resident)",
                page_idx,
                node.uid
            );
            return false;
        };
        node.z = z;
        crate::trace_log!(
            cat::LAYER_MODEL,
            "add_node_at_z page={} uid={} kind={:?} z={} count={}",
            page_idx,
            node.uid,
            node.kind,
            node.z,
            page.nodes.len() + 1
        );
        // Capture the node's dirty kind BEFORE moving it in (see `add_node`): a re-inserted dirty node
        // must advance its `(page, kind)` dirty epoch so a stale-but-current-epoch ack cannot settle it.
        let dirty_kind = node.pixels_dirty.then_some(node.kind);
        page.nodes.push(node);
        page.nodes.sort_by_key(|n| n.z);
        self.bump_version();
        if let Some(kind) = dirty_kind {
            let save_kind = match kind {
                NodeKind::Raster => SaveKind::Raster,
                NodeKind::Text => SaveKind::Text,
            };
            self.bump_dirty_epoch(page_idx, save_kind);
        }
        true
    }

    /// Removes the node with `uid` from a resident page. Returns whether a node was removed.
    pub fn remove_node(&mut self, page_idx: usize, uid: &str) -> bool {
        let Some(page) = self.pages.get_mut(&page_idx) else {
            return false;
        };
        let before = page.nodes.len();
        page.nodes.retain(|n| n.uid != uid);
        let removed = page.nodes.len() != before;
        crate::trace_log!(
            cat::LAYER_MODEL,
            "remove_node page={} uid={} removed={} count={}",
            page_idx,
            uid,
            removed,
            page.nodes.len()
        );
        if removed {
            self.bump_version();
        }
        removed
    }

    /// Sets a node's visibility. No-op if the page or node is absent.
    pub fn set_visibility(&mut self, page_idx: usize, uid: &str, visible: bool) {
        crate::trace_log!(
            cat::LAYER_MODEL,
            "set_visibility page={} uid={} visible={}",
            page_idx,
            uid,
            visible
        );
        if let Some(node) = self.node_mut(page_idx, uid) {
            node.visible = visible;
            self.bump_version();
        }
    }

    /// Sets a node's opacity, clamped to `0.0..=1.0`. No-op if the page or node is absent.
    pub fn set_opacity(&mut self, page_idx: usize, uid: &str, opacity: f32) {
        crate::trace_log!(
            cat::LAYER_MODEL,
            "set_opacity page={} uid={} opacity={}",
            page_idx,
            uid,
            opacity.clamp(0.0, 1.0)
        );
        if let Some(node) = self.node_mut(page_idx, uid) {
            node.opacity = opacity.clamp(0.0, 1.0);
            self.bump_version();
        }
    }

    /// Sets a node's group membership (`None` to ungroup). No-op if the page or node is absent.
    pub fn set_group(&mut self, page_idx: usize, uid: &str, group_uid: Option<String>) {
        crate::trace_log!(
            cat::LAYER_MODEL,
            "set_group page={} uid={} group_uid={:?}",
            page_idx,
            uid,
            group_uid
        );
        if let Some(node) = self.node_mut(page_idx, uid) {
            node.group_uid = group_uid;
            self.bump_version();
        }
    }

    /// Adds a `GroupMeta` to a resident page, ignoring it if a group with the same uid already exists.
    /// No-op if the page is absent.
    pub fn add_group(&mut self, page_idx: usize, group: GroupMeta) {
        if let Some(page) = self.pages.get_mut(&page_idx) {
            if page.groups.iter().any(|g| g.uid == group.uid) {
                crate::trace_log!(
                    cat::LAYER_MODEL,
                    "add_group page={} uid={} result=skip (exists)",
                    page_idx,
                    group.uid
                );
                return;
            }
            crate::trace_log!(
                cat::LAYER_MODEL,
                "add_group page={} uid={} groups={}",
                page_idx,
                group.uid,
                page.groups.len() + 1
            );
            page.groups.push(group);
            self.bump_version();
        }
    }

    /// Removes the `GroupMeta` with `group_uid` from a resident page and clears `group_uid` on every
    /// member node (ungroups them). No-op if the page is absent.
    pub fn remove_group(&mut self, page_idx: usize, group_uid: &str) {
        crate::trace_log!(
            cat::LAYER_MODEL,
            "remove_group page={} uid={}",
            page_idx,
            group_uid
        );
        let Some(page) = self.pages.get_mut(&page_idx) else {
            return;
        };
        page.groups.retain(|g| g.uid != group_uid);
        for node in &mut page.nodes {
            if node.group_uid.as_deref() == Some(group_uid) {
                node.group_uid = None;
            }
        }
        self.bump_version();
    }

    /// Replaces a RASTER node's base image, display image, and effects chain, sets `pixels_dirty`, and
    /// bumps its generation. No-op if the page/node is absent or the node is not a raster.
    pub fn set_raster_pixels(
        &mut self,
        page_idx: usize,
        uid: &str,
        base_image: ColorImage,
        display_image: ColorImage,
        effects: Vec<Value>,
        pixels_dirty: bool,
    ) {
        crate::trace_log!(
            cat::LAYER_MODEL,
            "set_raster_pixels page={} uid={} base={}x{} display={}x{} effects_len={} dirty={}",
            page_idx,
            uid,
            base_image.size[0],
            base_image.size[1],
            display_image.size[0],
            display_image.size[1],
            effects.len(),
            pixels_dirty
        );
        let mut changed = false;
        if let Some(node) = self.node_mut(page_idx, uid)
            && let NodeBody::Raster {
                base_image: b,
                display_image: d,
                effects: e,
                ..
            } = &mut node.body
        {
            *b = base_image;
            *d = display_image;
            *e = effects;
            node.pixels_dirty = pixels_dirty;
            node.bump_generation();
            self.bump_version();
            changed = pixels_dirty;
        }
        if changed {
            self.bump_dirty_epoch(page_idx, SaveKind::Raster);
        }
    }

    /// Replaces a TEXT node's render params and rendered image, sets `pixels_dirty` (so the next flush
    /// re-encodes the rendered PNG — mirrors `set_raster_pixels`), and bumps its generation. No-op if
    /// the page/node is absent or the node is not text. Persistence happens on `flush_page`.
    pub fn set_text_render(
        &mut self,
        page_idx: usize,
        uid: &str,
        render_data: Value,
        image: ColorImage,
    ) {
        crate::trace_log!(
            cat::LAYER_MODEL,
            "set_text_render page={} uid={} img={}x{}",
            page_idx,
            uid,
            image.size[0],
            image.size[1]
        );
        let mut changed = false;
        if let Some(node) = self.node_mut(page_idx, uid)
            && let NodeBody::Text {
                render_data: r,
                image: i,
                ..
            } = &mut node.body
        {
            *r = render_data;
            *i = image;
            node.pixels_dirty = true;
            node.bump_generation();
            self.bump_version();
            changed = true;
        }
        if changed {
            self.bump_dirty_epoch(page_idx, SaveKind::Text);
        }
    }

    /// Moves the node one step in `z` among the page's nodes by swapping its `z` with the neighbor in
    /// the current bottom-to-top order (`up == true` moves toward the top). Keeps `nodes` sorted by
    /// `z`. Pure in-memory. Returns true if the order changed.
    pub fn reorder_node_one(&mut self, page_idx: usize, uid: &str, up: bool) -> bool {
        let Some(page) = self.pages.get_mut(&page_idx) else {
            return false;
        };
        // `nodes` is maintained sorted by z, so positions are the bottom-to-top order.
        let Some(pos) = page.nodes.iter().position(|n| n.uid == uid) else {
            return false;
        };
        let other = if up {
            if pos + 1 >= page.nodes.len() {
                return false;
            }
            pos + 1
        } else {
            if pos == 0 {
                return false;
            }
            pos - 1
        };
        let z_a = page.nodes[pos].z;
        let z_b = page.nodes[other].z;
        page.nodes[pos].z = z_b;
        page.nodes[other].z = z_a;
        page.nodes.sort_by_key(|n| n.z);
        crate::trace_log!(
            cat::LAYER_MODEL,
            "reorder_node_one page={} uid={} dir={} z={}->{} result=true",
            page_idx,
            uid,
            if up { "up" } else { "down" },
            z_a,
            z_b
        );
        self.bump_version();
        true
    }

    /// Moves an entire group's block of nodes one step in `z`, swapping it past the neighbouring block
    /// (a group run or a single ungrouped node) while preserving the group's intra-order. Mirrors the
    /// PS panel's group-block move. `up == true` moves the block toward the top.
    ///
    /// The page's `nodes` are kept sorted by `z`; group members may be non-contiguous in `z` only if
    /// the disk band order was, but the PS editor anchors a group's bands together, so in practice a
    /// group occupies a contiguous run. This walks the current bottom-to-top order, finds the group's
    /// run, swaps it with the adjacent block, then reassigns contiguous `z` by the new position.
    /// Returns true if the order changed.
    pub fn reorder_group_block(&mut self, page_idx: usize, group_uid: &str, up: bool) -> bool {
        let Some(page) = self.pages.get_mut(&page_idx) else {
            return false;
        };
        // `nodes` is maintained sorted by z → positions are the bottom-to-top order. Segment the order
        // into maximal blocks: a run of consecutive nodes sharing the same (non-None) group_uid, or a
        // single ungrouped/other-group node.
        let n = page.nodes.len();
        let mut blocks: Vec<(usize, usize)> = Vec::new();
        for i in 0..n {
            let gi = page.nodes[i].group_uid.clone();
            if let Some((_, hi)) = blocks.last_mut()
                && gi.is_some()
                && page.nodes[*hi].group_uid == gi
            {
                *hi = i;
            } else {
                blocks.push((i, i));
            }
        }
        // Find the target group's block.
        let Some(bi) = blocks
            .iter()
            .position(|(lo, _)| page.nodes[*lo].group_uid.as_deref() == Some(group_uid))
        else {
            return false;
        };
        let target = if up { bi + 1 } else { bi.wrapping_sub(1) };
        if target >= blocks.len() {
            return false;
        }
        // Rebuild the node order with the two blocks swapped, then reassign contiguous z.
        let mut order_blocks: Vec<std::ops::RangeInclusive<usize>> =
            blocks.iter().map(|(lo, hi)| *lo..=*hi).collect();
        order_blocks.swap(bi, target);
        let new_index: Vec<usize> = order_blocks.into_iter().flatten().collect();
        // `new_index[new_pos] = old_pos`; reorder the Vec accordingly.
        let mut reordered: Vec<LayerNode> = Vec::with_capacity(n);
        // Drain in the new order by swapping out via indices: build by taking owned nodes.
        let mut slots: Vec<Option<LayerNode>> = page.nodes.drain(..).map(Some).collect();
        for (new_z, &old_pos) in new_index.iter().enumerate() {
            let mut node = slots[old_pos].take().expect("each old slot taken once");
            node.z = new_z as u32;
            reordered.push(node);
        }
        page.nodes = reordered;
        crate::trace_log!(
            cat::LAYER_MODEL,
            "reorder_group_block page={} group_uid={} dir={} result=true",
            page_idx,
            group_uid,
            if up { "up" } else { "down" }
        );
        self.bump_version();
        true
    }

    /// Reassigns every node's `z` to its position in `order` (a bottom-to-top list of node uids), then
    /// re-sorts. Nodes whose uid is absent from `order` keep their relative order after the listed
    /// ones (appended in their prior order). The PS structure ops compute the authoritative band order
    /// and persist it to disk; this applies that SAME order to the in-memory doc so cross-tab listeners
    /// re-project without a disk round-trip. No-op if the page is absent.
    pub fn set_z_order(&mut self, page_idx: usize, order: &[String]) {
        crate::trace_log!(
            cat::LAYER_MODEL,
            "set_z_order page={} order_len={}",
            page_idx,
            order.len()
        );
        let Some(page) = self.pages.get_mut(&page_idx) else {
            return;
        };
        let rank: HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(i, u)| (u.as_str(), i))
            .collect();
        // Listed nodes get their explicit rank; unlisted nodes sort after, keeping their prior z order.
        let unlisted_base = order.len();
        let mut next_unlisted = unlisted_base;
        // Assign z: listed by rank, unlisted by a running counter seeded from their current z order.
        // First snapshot current bottom-to-top order for stable unlisted placement.
        let prior: Vec<String> = {
            let mut v: Vec<&LayerNode> = page.nodes.iter().collect();
            v.sort_by_key(|n| n.z);
            v.into_iter().map(|n| n.uid.clone()).collect()
        };
        let mut unlisted_rank: HashMap<&str, usize> = HashMap::new();
        for uid in &prior {
            if !rank.contains_key(uid.as_str()) {
                unlisted_rank.insert(uid.as_str(), next_unlisted);
                next_unlisted += 1;
            }
        }
        for node in &mut page.nodes {
            node.z = rank
                .get(node.uid.as_str())
                .copied()
                .or_else(|| unlisted_rank.get(node.uid.as_str()).copied())
                .unwrap_or(unlisted_base) as u32;
        }
        page.nodes.sort_by_key(|n| n.z);
        self.bump_version();
    }

    /// Persists a page's RASTER nodes back to disk via `persist::save_page_rasters` (passing each
    /// node's base image + `pixels_dirty` and the page's groups, bottom-to-top by current `z`), then
    /// re-writes the effects chain + rendered PNG for any raster node with a non-empty chain via
    /// `persist::update_raster_effects` (the doc is authoritative over the rasters it holds). After a
    /// successful flush, every raster node's `pixels_dirty` is cleared.
    pub fn flush_page(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
    ) -> Result<(), String> {
        self.flush_page_inner(page_idx, layers_dir, fallback_dir, &[])
    }

    /// Like [`flush_page`], but also DROPS `removed_raster_uids` from the manifest (and prunes their
    /// PNGs). Without this, `save_page_rasters` PRESERVES a manifest raster not in the flushed set as
    /// "owned by another tab" — so a raster the typing tab deleted would resurrect on disk. The deleter
    /// passes the removed uid here. (The PS editor uses `record_raster_deletion` for the same effect.)
    pub fn flush_page_dropping_raster(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
        removed_uid: &str,
    ) -> Result<(), String> {
        self.flush_page_inner(
            page_idx,
            layers_dir,
            fallback_dir,
            &[removed_uid.to_string()],
        )
    }

    fn flush_page_inner(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
        removed_raster_uids: &[String],
    ) -> Result<(), String> {
        let _span = crate::trace_scope!(
            cat::PERSIST,
            "flush_page_inner page={} dir={} removed={}",
            page_idx,
            layers_dir.display(),
            removed_raster_uids.len()
        );
        let Some(page) = self.pages.get_mut(&page_idx) else {
            crate::trace_log!(
                cat::PERSIST,
                "flush_page_inner page={} skipped (not resident)",
                page_idx
            );
            return Ok(());
        };

        // Pass rasters bottom-to-top by current z (nodes are kept sorted, but be explicit).
        let mut raster_indices: Vec<usize> = page
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.is_raster())
            .map(|(i, _)| i)
            .collect();
        raster_indices.sort_by_key(|&i| page.nodes[i].z);

        let mut outs: Vec<RasterLayerOut> = Vec::with_capacity(raster_indices.len());
        for &i in &raster_indices {
            let node = &page.nodes[i];
            let NodeBody::Raster {
                base_image,
                mask_clip,
                ..
            } = &node.body
            else {
                continue;
            };
            outs.push(RasterLayerOut {
                uid: node.uid.clone(),
                name: node.name.clone(),
                visible: node.visible,
                opacity: node.opacity,
                transform: node.transform,
                deform: node.deform.clone(),
                group_uid: node.group_uid.clone(),
                image: base_image,
                pixels_dirty: node.pixels_dirty,
                mask_clip: *mask_clip,
            });
        }

        crate::trace_log!(
            cat::PERSIST,
            "flush_page_inner page={} rasters={} groups={}",
            page_idx,
            outs.len(),
            page.groups.len()
        );
        persist::save_page_rasters(
            layers_dir,
            page_idx,
            &outs,
            &page.groups,
            removed_raster_uids,
        )?;

        // Text flush (schema v3): write every TEXT node's inline payload (see `write_page_text`).
        Self::write_page_text(page, page_idx, layers_dir, fallback_dir)?;

        // Re-write the doc's effects chains so the on-disk chain + rendered PNG match the doc, which
        // is authoritative. A non-dirty `save_page_rasters` preserves the on-disk chain, but the doc
        // may have changed effects in memory, so always reconcile here.
        for &i in &raster_indices {
            let node = &page.nodes[i];
            if let NodeBody::Raster {
                effects,
                display_image,
                ..
            } = &node.body
                && !effects.is_empty()
            {
                persist::update_raster_effects(
                    layers_dir,
                    page_idx,
                    &node.uid,
                    effects,
                    Some(display_image),
                    fallback_dir,
                )?;
            }
        }

        for &i in &raster_indices {
            page.nodes[i].pixels_dirty = false;
        }
        Ok(())
    }

    /// Persists ONLY a page's TEXT nodes (their inline v3 payload + rendered PNGs), leaving raster
    /// nodes untouched on disk. The typing tab calls this on every text edit/placement save so the doc
    /// is the sole text writer without re-saving rasters each time; `flush_page` calls the same writer
    /// for whole-page (save-to-project) flushes. No-op if the page is not resident.
    pub fn flush_page_text(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
    ) -> Result<(), String> {
        let _span = crate::trace_scope!(
            cat::PERSIST,
            "flush_page_text page={} dir={}",
            page_idx,
            layers_dir.display()
        );
        let Some(page) = self.pages.get_mut(&page_idx) else {
            crate::trace_log!(
                cat::PERSIST,
                "flush_page_text page={} skipped (not resident)",
                page_idx
            );
            return Ok(());
        };
        Self::write_page_text(page, page_idx, layers_dir, fallback_dir)
    }

    /// Writes every TEXT node of `page` into `layers.json` with its FULL inline payload (render_data +
    /// geometry + rendered PNG name + mask_clip) via `persist::write_page_text_payload`, which
    /// preserves raster nodes (kind-filter) and rebuilds the text-group bands. The rendered text PNG is
    /// (re)written only when the in-memory image is dirty (a
    /// text re-render this session) OR the deterministic file is not yet on disk — an unchanged text
    /// PNG is never re-encoded (mirrors the raster `pixels_dirty` rule). After a successful write every
    /// text node's `pixels_dirty` is cleared. Nodes are emitted bottom-to-top by `z`.
    fn write_page_text(
        page: &mut DocPage,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
    ) -> Result<(), String> {
        let _span = crate::trace_scope!(cat::PERSIST, "write_page_text page={}", page_idx);
        let mut text_indices: Vec<usize> = page
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.kind, NodeKind::Text))
            .map(|(i, _)| i)
            .collect();
        text_indices.sort_by_key(|&i| page.nodes[i].z);

        let mut pngs_written = 0usize;
        let mut text_outs: Vec<persist::TextPayloadOut> = Vec::with_capacity(text_indices.len());
        for &i in &text_indices {
            let node = &page.nodes[i];
            let NodeBody::Text {
                render_data,
                image,
                is_image,
                payload_uid,
                mask_clip,
            } = &node.body
            else {
                continue;
            };
            // Determine the rendered PNG: rewrite when dirty or when the deterministic file is missing;
            // otherwise reuse the existing on-disk file without re-encoding.
            let file_name = persist::text_image_file_name(page_idx, &node.uid);
            let present = layers_dir.join(&file_name).is_file()
                || fallback_dir.is_some_and(|d| d.join(&file_name).is_file());
            let rendered_file = if node.pixels_dirty || !present {
                pngs_written += 1;
                crate::trace_log!(
                    cat::PERSIST,
                    "write_page_text page={} uid={} rewriting PNG (dirty={} present={})",
                    page_idx,
                    node.uid,
                    node.pixels_dirty,
                    present
                );
                Some(persist::write_text_image(
                    layers_dir, page_idx, &node.uid, image,
                )?)
            } else {
                Some(file_name)
            };
            text_outs.push(persist::TextPayloadOut {
                uid: node.uid.clone(),
                name: node.name.clone(),
                z: node.z,
                // The text-group axis (Группа текста N): the node's carried `text_layer_idx` (set by the
                // typing tab on create / projected from disk on load) is authoritative. Falls back to
                // group 0 when unknown.
                layer_idx: node.text_layer_idx.unwrap_or(0),
                pinned: false,
                visible: node.visible,
                opacity: node.opacity,
                group_uid: node.group_uid.clone(),
                pinned_by_group: false,
                payload_uid: payload_uid.clone(),
                render_data: render_data.clone(),
                is_image: *is_image,
                transform: node.transform,
                deform: node.deform.clone(),
                rendered_file,
                mask_clip: *mask_clip,
            });
        }
        crate::trace_log!(
            cat::PERSIST,
            "write_page_text page={} text_nodes={} pngs_written={}",
            page_idx,
            text_outs.len(),
            pngs_written
        );
        persist::write_page_text_payload(layers_dir, fallback_dir, page_idx, &text_outs)?;
        for &i in &text_indices {
            page.nodes[i].pixels_dirty = false;
        }
        Ok(())
    }

    // Async-save API (`enable_background_saver` … `clear_page_text_dirty`): wired into the GUI tabs
    // (PS per-edit/raster flushes, typing text flushes) and the save-to-project / app-close barriers.
    /// Enables the background (coalescing) saver on this doc, if not already enabled. After this,
    /// `enqueue_page_save` / `enqueue_page_text_save` route persistence off-thread. Idempotent:
    /// a second call is a no-op (the existing saver + its worker thread are kept).
    pub fn enable_background_saver(&mut self) {
        if self.saver.is_none() {
            let saver = LayerSaver::new();
            self.save_acks = Some(saver.ack_map());
            self.saver = Some(saver);
            crate::trace_log!(cat::PERSIST, "enable_background_saver: saver enabled");
        }
    }

    /// A cheap-clone handle to the background saver, if one is enabled, so a merge worker can enqueue
    /// jobs / run a barrier without locking the doc. `None` when no saver is enabled.
    #[must_use]
    pub fn saver_handle(&self) -> Option<LayerSaverHandle> {
        self.saver.as_ref().map(LayerSaver::handle)
    }

    /// Shuts the background saver down (sentinel + join), if one is enabled, flushing its queue first.
    /// Called by `Drop`; safe to call explicitly. A no-op when no saver is enabled.
    pub fn shutdown_saver(&mut self) {
        if let Some(saver) = self.saver.take() {
            saver.shutdown();
        }
        self.save_acks = None;
    }

    /// Reserves the current page-kind generation for a background job and returns its epoch.
    #[must_use]
    pub fn next_save_epoch(&mut self, page_idx: usize, kind: SaveKind) -> u64 {
        self.save_epoch = self.save_epoch.wrapping_add(1);
        self.latest_epoch.insert((page_idx, kind), self.save_epoch);
        self.save_epoch
    }

    /// Advances a dirty page-kind generation so older in-flight acknowledgements cannot settle it.
    fn bump_dirty_epoch(&mut self, page_idx: usize, kind: SaveKind) {
        let _epoch = self.next_save_epoch(page_idx, kind);
    }

    /// Applies completed worker acknowledgements without doing persistence or holding the ack lock
    /// while mutating resident document pages.
    pub fn poll_save_acks(&mut self) {
        let completions = self.save_acks.as_ref().and_then(|acks| match acks.lock() {
            Ok(mut ack) => Some(ack.take()),
            Err(_) => {
                crate::runtime_log::log_error("[layer_model] save acknowledgement map lock poisoned");
                None
            }
        }).unwrap_or_default();
        for (page_idx, kind, ack_epoch, ok) in completions {
            if ok && self.latest_epoch.get(&(page_idx, kind)) == Some(&ack_epoch) && self.pages.contains_key(&page_idx) {
                match kind {
                    SaveKind::Raster => self.clear_page_dirty(page_idx),
                    SaveKind::Text => self.clear_page_text_dirty(page_idx),
                }
                self.latest_epoch.remove(&(page_idx, kind));
            }
        }
    }

    /// ASYNC whole-page save: when the background saver is enabled, builds the page's owned save job
    /// and enqueues it (coalescing, off-thread); otherwise falls back to the synchronous [`flush_page`]
    /// so behavior is safe either way. No-op (returns `Ok`) if the page is not resident.
    ///
    /// Mirrors `flush_page`'s contract: callers that need the bytes ON DISK before reading them must
    /// `barrier_blocking` the saver handle (the enqueue is fire-and-forget).
    ///
    /// # Errors
    /// Only the synchronous fallback path can return an error (the async path enqueues and returns
    /// `Ok`); it matches [`flush_page`]'s error propagation.
    pub fn enqueue_page_save(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
    ) -> Result<(), String> {
        if self.saver.is_some() {
            if let Some(mut job) = self.build_page_save_job(page_idx, layers_dir, fallback_dir, &[]) {
                job.raster_epoch = Some(self.next_save_epoch(page_idx, SaveKind::Raster));
                job.text_epoch = Some(self.next_save_epoch(page_idx, SaveKind::Text));
                if let Some(saver) = &self.saver {
                    saver.enqueue(job);
                }
            }
            Ok(())
        } else {
            self.flush_page(page_idx, layers_dir, fallback_dir)
        }
    }

    /// ASYNC text-only save: when the background saver is enabled, builds a TEXT-ONLY owned save job
    /// and enqueues it; otherwise falls back to the synchronous [`flush_page_text`]. No-op (returns
    /// `Ok`) if the page is not resident.
    ///
    /// # Errors
    /// Only the synchronous fallback can error; the async path enqueues and returns `Ok`.
    pub fn enqueue_page_text_save(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
    ) -> Result<(), String> {
        if self.saver.is_some() {
            if let Some(text) = self.build_text_save_part(page_idx) {
                let text_epoch = self.next_save_epoch(page_idx, SaveKind::Text);
                let job = PageSaveJob {
                    page_idx,
                    layers_dir: layers_dir.to_path_buf(),
                    fallback_dir: fallback_dir.map(Path::to_path_buf),
                    raster: None,
                    raster_epoch: None,
                    text: Some(text),
                    text_epoch: Some(text_epoch),
                    effects: Vec::new(),
                };
                if let Some(saver) = &self.saver {
                    saver.enqueue(job);
                }
            }
            Ok(())
        } else {
            self.flush_page_text(page_idx, layers_dir, fallback_dir)
        }
    }

    /// ASYNC targeted effects update for a SINGLE raster: when the background saver is enabled,
    /// enqueues an effects-only job that reconciles only this raster's chain + rendered PNG (never a
    /// whole-page raster rewrite); otherwise falls back to the synchronous `persist::update_raster_effects`
    /// so behavior is safe either way. `display` is the post-effects render (`None` ⇒ CLEAR the chain
    /// and delete any rendered PNG). The bytes written are identical to the direct sync call; only the
    /// PNG encode moves off the GUI thread.
    ///
    /// Unlike a whole-page save this does NOT depend on the page being resident in the doc — it targets
    /// the on-disk node by `uid` (mirroring the callers' existing direct `update_raster_effects`).
    ///
    /// # Errors
    /// Only the synchronous fallback can error; the async path enqueues and returns `Ok`.
    pub fn enqueue_raster_effects(
        &mut self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
        uid: &str,
        effects: &[serde_json::Value],
        display: Option<&ColorImage>,
    ) -> Result<(), String> {
        if self.saver.is_some() {
            let raster_epoch = self.next_save_epoch(page_idx, SaveKind::Raster);
            let Some(saver) = &self.saver else {
                return Ok(());
            };
            saver.enqueue(PageSaveJob {
                page_idx,
                layers_dir: layers_dir.to_path_buf(),
                fallback_dir: fallback_dir.map(Path::to_path_buf),
                raster: None,
                raster_epoch: Some(raster_epoch),
                text: None,
                text_epoch: None,
                effects: vec![EffectsSaveItem {
                    uid: uid.to_string(),
                    effects: effects.to_vec(),
                    display_image: display.cloned(),
                }],
            });
            Ok(())
        } else {
            persist::update_raster_effects(
                layers_dir,
                page_idx,
                uid,
                effects,
                display,
                fallback_dir,
            )
        }
    }

    /// Builds the OWNED whole-page save job for the background saver, CLONING all data the persist call
    /// needs so the worker holds no borrow into the doc. The field selection mirrors `flush_page_inner`
    /// (rasters bottom-to-top by `z`, base image + `pixels_dirty` + effects/display) and `write_page_text`
    /// (text nodes bottom-to-top by `z`) EXACTLY, so the worker's `persist::*` calls produce the same
    /// bytes the synchronous flush would. `removed_raster_uids` matches `flush_page_dropping_raster`'s
    /// drop set. Returns `None` if the page is not resident.
    #[must_use]
    fn build_page_save_job(
        &self,
        page_idx: usize,
        layers_dir: &Path,
        fallback_dir: Option<&Path>,
        removed_raster_uids: &[String],
    ) -> Option<PageSaveJob> {
        let page = self.pages.get(&page_idx)?;
        let raster = Self::build_raster_save_part(page, removed_raster_uids);
        let text = Self::build_text_save_part_from(page);
        Some(PageSaveJob {
            page_idx,
            layers_dir: layers_dir.to_path_buf(),
            fallback_dir: fallback_dir.map(Path::to_path_buf),
            raster: Some(raster),
            raster_epoch: None,
            text: Some(text),
            text_epoch: None,
            effects: Vec::new(),
        })
    }

    /// Builds ONLY the owned text part for a resident page (for the text-only async path). `None` if
    /// the page is not resident.
    #[must_use]
    fn build_text_save_part(&self, page_idx: usize) -> Option<TextSavePart> {
        let page = self.pages.get(&page_idx)?;
        Some(Self::build_text_save_part_from(page))
    }

    /// Gathers the owned RASTER part of a page save, mirroring `flush_page_inner`'s `outs` build
    /// (bottom-to-top by `z`; base image + `pixels_dirty` + mask-clip; the effects chain + display
    /// image so the worker can reconcile effects exactly like the sync flush).
    #[must_use]
    fn build_raster_save_part(page: &DocPage, removed_raster_uids: &[String]) -> RasterSavePart {
        let mut raster_indices: Vec<usize> = page
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.is_raster())
            .map(|(i, _)| i)
            .collect();
        raster_indices.sort_by_key(|&i| page.nodes[i].z);

        let mut layers: Vec<OwnedRasterLayer> = Vec::with_capacity(raster_indices.len());
        for &i in &raster_indices {
            let node = &page.nodes[i];
            let NodeBody::Raster {
                base_image,
                display_image,
                effects,
                mask_clip,
                ..
            } = &node.body
            else {
                continue;
            };
            // The display image is only needed when there is a non-empty effects chain to reconcile
            // (the sync flush's `update_raster_effects` is gated on a non-empty chain). Cloning it
            // unconditionally would waste a copy on the common no-effects raster.
            let display = if effects.is_empty() {
                None
            } else {
                Some(display_image.clone())
            };
            layers.push(OwnedRasterLayer {
                uid: node.uid.clone(),
                name: node.name.clone(),
                visible: node.visible,
                opacity: node.opacity,
                transform: node.transform,
                deform: node.deform.clone(),
                group_uid: node.group_uid.clone(),
                base_image: base_image.clone(),
                pixels_dirty: node.pixels_dirty,
                mask_clip: *mask_clip,
                display_image: display,
                effects: effects.clone(),
            });
        }
        RasterSavePart {
            layers,
            groups: page.groups.clone(),
            removed_uids: removed_raster_uids.to_vec(),
        }
    }

    /// Gathers the owned TEXT part of a page save, mirroring `write_page_text`'s per-node out build
    /// (bottom-to-top by `z`; full inline payload + rendered image + `pixels_dirty`). The worker
    /// reproduces the "rewrite PNG iff dirty or missing" rule from the carried `pixels_dirty`.
    #[must_use]
    fn build_text_save_part_from(page: &DocPage) -> TextSavePart {
        let mut text_indices: Vec<usize> = page
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.kind, NodeKind::Text))
            .map(|(i, _)| i)
            .collect();
        text_indices.sort_by_key(|&i| page.nodes[i].z);

        let mut nodes: Vec<OwnedTextNode> = Vec::with_capacity(text_indices.len());
        for &i in &text_indices {
            let node = &page.nodes[i];
            let NodeBody::Text {
                render_data,
                image,
                is_image,
                payload_uid,
                mask_clip,
            } = &node.body
            else {
                continue;
            };
            nodes.push(OwnedTextNode {
                uid: node.uid.clone(),
                name: node.name.clone(),
                z: node.z,
                layer_idx: node.text_layer_idx.unwrap_or(0),
                visible: node.visible,
                opacity: node.opacity,
                group_uid: node.group_uid.clone(),
                payload_uid: payload_uid.clone(),
                render_data: render_data.clone(),
                is_image: *is_image,
                transform: node.transform,
                deform: node.deform.clone(),
                mask_clip: *mask_clip,
                image: image.clone(),
                pixels_dirty: node.pixels_dirty,
            });
        }
        TextSavePart { nodes }
    }

    /// Clears `pixels_dirty` on every RASTER node of a resident page after its current raster epoch
    /// is durably acknowledged, leaving text dirty flags untouched.
    fn clear_page_dirty(&mut self, page_idx: usize) {
        if let Some(page) = self.pages.get_mut(&page_idx) {
            for node in &mut page.nodes {
                if matches!(node.kind, NodeKind::Raster) {
                    node.pixels_dirty = false;
                }
            }
        }
    }

    /// Clears `pixels_dirty` on every TEXT node of a resident page after its current text epoch is
    /// durably acknowledged, leaving raster dirty flags untouched.
    fn clear_page_text_dirty(&mut self, page_idx: usize) {
        if let Some(page) = self.pages.get_mut(&page_idx) {
            for node in &mut page.nodes {
                if matches!(node.kind, NodeKind::Text) {
                    node.pixels_dirty = false;
                }
            }
        }
    }
}

impl Drop for LayerDoc {
    /// Shuts the background saver down (flush queue + join the worker) if one was enabled. A doc with
    /// no saver (the default — most unit tests) does nothing here, spawning/joining no thread.
    fn drop(&mut self) {
        self.shutdown_saver();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Color32;
    use std::fs;

    fn img(size: [usize; 2], c: Color32) -> ColorImage {
        ColorImage::filled(size, c)
    }

    fn tf(cx: f32, cy: f32, scale: f32) -> TransformRec {
        TransformRec {
            cx,
            cy,
            rotation: 0.0,
            scale,
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ld_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    /// A single-page page-size map for page 0, for `ensure_page_loaded` test calls.
    fn psz(size: [usize; 2]) -> HashMap<usize, [usize; 2]> {
        let mut m = HashMap::new();
        m.insert(0, size);
        m
    }

    #[test]
    fn ensure_page_loaded_reads_rasters_z_sorted() {
        let dir = temp_dir("load");

        // Seed two rasters on disk; add order is bottom-to-top, so r0 is below r1.
        persist::add_page_raster(
            &dir,
            None,
            0,
            "r0",
            "Bottom",
            true,
            1.0,
            tf(10.0, 20.0, 1.0),
            &img([2, 2], Color32::RED),
        )
        .unwrap();
        persist::add_page_raster(
            &dir,
            None,
            0,
            "r1",
            "Top",
            false,
            0.5,
            tf(30.0, 40.0, 2.0),
            &img([3, 3], Color32::BLUE),
        )
        .unwrap();

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();

        let page = doc.page(0).expect("page resident");
        assert_eq!(page.nodes.len(), 2, "two raster nodes loaded");
        // Sorted bottom-to-top by z.
        assert!(page.nodes[0].z < page.nodes[1].z, "z sorted bottom-to-top");
        assert_eq!(page.nodes[0].uid, "r0");
        assert_eq!(page.nodes[1].uid, "r1");

        let r0 = doc.node(0, "r0").unwrap();
        assert!(r0.is_raster());
        assert!(r0.visible);
        assert!((r0.transform.cx - 10.0).abs() < 1e-6);
        assert!((r0.transform.cy - 20.0).abs() < 1e-6);

        let r1 = doc.node(0, "r1").unwrap();
        assert!(!r1.visible);
        assert!((r1.opacity - 0.5).abs() < 1e-6);
        assert!((r1.transform.scale - 2.0).abs() < 1e-6);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_has_pending_save_tracks_all_pending_persistence() {
        let dir = temp_dir("pending_save");
        persist::add_page_raster(
            &dir,
            None,
            0,
            "r",
            "Raster",
            true,
            1.0,
            tf(0.0, 0.0, 1.0),
            &img([2, 2], Color32::RED),
        )
        .unwrap();

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        // A clean freshly-decoded page and a non-resident page have no pending save.
        assert!(
            !doc.page_has_pending_save(0),
            "freshly decoded page has no pending save"
        );
        assert!(
            !doc.page_has_pending_save(1),
            "non-resident page has no pending save"
        );

        // GEOMETRY-only edit: `set_transform` does NOT flag any node `pixels_dirty`; it is persisted
        // via a whole-page enqueue that reserves a `latest_epoch`. Driving that enqueue is the smallest
        // public API that exercises the `latest_epoch` gate (Gap-1 regression: the old pixel-only helper
        // classified this page "clean" and evicted it, losing the rotation).
        doc.enable_background_saver();
        doc.set_transform(0, "r", tf(5.0, 7.0, 1.0));
        doc.enqueue_page_save(0, &dir, None).unwrap();
        if let Some(handle) = doc.saver_handle() {
            // Let the job run so `shutdown_saver` joins cleanly; the ack is not polled, so
            // `latest_epoch` still holds the reserved epoch.
            let _ = handle.barrier_blocking();
        }
        assert!(
            doc.latest_epoch.contains_key(&(0, SaveKind::Raster))
                || doc.latest_epoch.contains_key(&(0, SaveKind::Text)),
            "the geometry enqueue reserved a latest_epoch"
        );
        assert!(
            !doc.pages.get(&0).unwrap().nodes.iter().any(|n| n.pixels_dirty),
            "a geometry edit does not flag any node pixels_dirty"
        );
        assert!(
            doc.page_has_pending_save(0),
            "a geometry edit awaiting its enqueued save is pending even with no pixels_dirty node"
        );

        // A PIXEL edit is likewise pending (via both `pixels_dirty` and the reserved epoch).
        doc.set_raster_pixels(
            0,
            "r",
            img([2, 2], Color32::BLUE),
            img([2, 2], Color32::BLUE),
            Vec::new(),
            true,
        );
        assert!(
            doc.page_has_pending_save(0),
            "pixel-edited resident page has a pending save"
        );

        doc.shutdown_saver();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_transform_round_trips_through_flush() {
        let dir = temp_dir("tf");

        persist::add_page_raster(
            &dir,
            None,
            0,
            "r",
            "Pic",
            true,
            1.0,
            tf(1.0, 1.0, 1.0),
            &img([2, 2], Color32::RED),
        )
        .unwrap();

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        doc.set_transform(0, "r", tf(123.0, 456.0, 3.5));
        // Geometry-only change must not bump the generation.
        assert_eq!(
            doc.node(0, "r").unwrap().generation,
            0,
            "set_transform does not bump generation"
        );
        doc.flush_page(0, &dir, None).unwrap();

        // A fresh doc reload sees the new transform, including scale.
        let mut doc2 = LayerDoc::new();
        doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        let r = doc2.node(0, "r").unwrap();
        assert!((r.transform.cx - 123.0).abs() < 1e-6, "cx round-trips");
        assert!((r.transform.cy - 456.0).abs() < 1e-6, "cy round-trips");
        assert!((r.transform.scale - 3.5).abs() < 1e-6, "scale round-trips");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_page_dropping_raster_removes_it_from_disk() {
        // The typing tab's raster delete: remove the doc node, then flush DROPPING its uid so it does
        // not resurrect (save_page_rasters otherwise preserves a manifest raster as another tab's).
        let dir = temp_dir("drop_raster");
        persist::add_page_raster(
            &dir,
            None,
            0,
            "r0",
            "A",
            true,
            1.0,
            tf(0.0, 0.0, 1.0),
            &img([2, 2], Color32::RED),
        )
        .unwrap();
        persist::add_page_raster(
            &dir,
            None,
            0,
            "r1",
            "B",
            true,
            1.0,
            tf(0.0, 0.0, 1.0),
            &img([2, 2], Color32::BLUE),
        )
        .unwrap();

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        assert_eq!(doc.page(0).unwrap().nodes.len(), 2);

        // Remove r0 from the doc, then flush dropping it.
        assert!(doc.remove_node(0, "r0"));
        doc.flush_page_dropping_raster(0, &dir, None, "r0").unwrap();

        // A fresh reload sees only r1 (r0 gone, not resurrected); r1 intact.
        let mut doc2 = LayerDoc::new();
        doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        assert!(
            doc2.node(0, "r0").is_none(),
            "deleted raster did not resurrect on disk"
        );
        assert!(doc2.node(0, "r1").is_some(), "the other raster survives");
        assert_eq!(doc2.page(0).unwrap().nodes.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_deform_bumps_version_and_round_trips_through_flush() {
        use super::super::manifest::DeformRec;
        let dir = temp_dir("dfm");

        persist::add_page_raster(
            &dir,
            None,
            0,
            "r",
            "Pic",
            true,
            1.0,
            tf(1.0, 1.0, 1.0),
            &img([2, 2], Color32::RED),
        )
        .unwrap();

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        assert!(
            doc.node(0, "r").unwrap().deform.is_none(),
            "raster loads affine (no deform)"
        );

        let ver_before = doc.version();
        let gen_before = doc.node(0, "r").unwrap().generation;
        let mesh = DeformRec {
            cols: 2,
            rows: 2,
            points_px: vec![[0.0, 0.0], [10.0, 1.0], [1.0, 12.0], [11.0, 13.0]],
        };
        doc.set_deform(0, "r", Some(mesh.clone()));
        assert_eq!(
            doc.version(),
            ver_before + 1,
            "set_deform bumps the version"
        );
        assert_eq!(
            doc.node(0, "r").unwrap().generation,
            gen_before,
            "set_deform is geometry-only: no generation bump"
        );

        doc.flush_page(0, &dir, None).unwrap();

        // A fresh doc reload sees the persisted mesh.
        let mut doc2 = LayerDoc::new();
        doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        let got = doc2
            .node(0, "r")
            .unwrap()
            .deform
            .as_ref()
            .expect("deform round-trips");
        assert_eq!(got.cols, 2);
        assert_eq!(got.rows, 2);
        assert_eq!(got.points_px, mesh.points_px);

        // Clearing the deform persists as None.
        doc2.set_deform(0, "r", None);
        doc2.flush_page(0, &dir, None).unwrap();
        let mut doc3 = LayerDoc::new();
        doc3.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        assert!(
            doc3.node(0, "r").unwrap().deform.is_none(),
            "cleared deform stays None"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_raster_mask_clip_bumps_generation_and_round_trips_through_flush() {
        let dir = temp_dir("rclip");
        persist::add_page_raster(
            &dir,
            None,
            0,
            "r",
            "Pic",
            true,
            1.0,
            tf(1.0, 1.0, 1.0),
            &img([2, 2], Color32::RED),
        )
        .unwrap();

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        // A freshly-loaded raster defaults OFF (no clip).
        if let NodeBody::Raster { mask_clip, .. } = &doc.node(0, "r").unwrap().body {
            assert_eq!(*mask_clip, None, "raster defaults to no mask-clip");
        } else {
            panic!("expected raster body");
        }

        let gen_before = doc.node(0, "r").unwrap().generation;
        doc.set_raster_mask_clip(0, "r", Some(true));
        assert!(
            doc.node(0, "r").unwrap().generation > gen_before,
            "set_raster_mask_clip bumps generation (so the tab re-clips + re-uploads)"
        );
        if let NodeBody::Raster { mask_clip, .. } = &doc.node(0, "r").unwrap().body {
            assert_eq!(*mask_clip, Some(true));
        } else {
            panic!("expected raster body");
        }

        doc.flush_page(0, &dir, None).unwrap();
        let mut doc2 = LayerDoc::new();
        doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        if let NodeBody::Raster { mask_clip, .. } = &doc2.node(0, "r").unwrap().body {
            assert_eq!(
                *mask_clip,
                Some(true),
                "mask_clip round-trips through the doc + flush"
            );
        } else {
            panic!("expected raster body");
        }

        // set_raster_mask_clip is a no-op on a text node.
        doc2.add_node(0, text_node("t", [2, 2], Color32::WHITE));
        let t_gen = doc2.node(0, "t").unwrap().generation;
        doc2.set_raster_mask_clip(0, "t", Some(true));
        assert_eq!(
            doc2.node(0, "t").unwrap().generation,
            t_gen,
            "no-op on a text node"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_effects_bumps_generation_and_persists() {
        let dir = temp_dir("fx");

        persist::add_page_raster(
            &dir,
            None,
            0,
            "r",
            "Pic",
            true,
            1.0,
            tf(1.0, 1.0, 1.0),
            &img([2, 2], Color32::RED),
        )
        .unwrap();

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        let gen_before = doc.node(0, "r").unwrap().generation;

        let effects = vec![serde_json::json!({"type": "shadow"})];
        // A distinct (larger, different-colored) display image.
        doc.set_effects(0, "r", effects, img([4, 4], Color32::BLUE));

        let node = doc.node(0, "r").unwrap();
        assert!(node.generation > gen_before, "set_effects bumps generation");
        // display_image now differs from the base.
        if let NodeBody::Raster {
            base_image,
            display_image,
            ..
        } = &node.body
        {
            assert_eq!(base_image.size, [2, 2]);
            assert_eq!(display_image.size, [4, 4], "display image replaced");
            assert_ne!(
                base_image.size, display_image.size,
                "display distinct from base"
            );
        } else {
            panic!("expected raster body");
        }

        doc.flush_page(0, &dir, None).unwrap();

        // Reload: effects present, display distinct from base.
        let mut doc2 = LayerDoc::new();
        doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        let r = doc2.node(0, "r").unwrap();
        if let NodeBody::Raster {
            base_image,
            display_image,
            effects,
            ..
        } = &r.body
        {
            assert_eq!(effects.len(), 1, "effects chain persisted");
            assert_eq!(display_image.size, [4, 4], "rendered display persisted");
            assert_eq!(base_image.size, [2, 2], "base preserved (non-destructive)");
            assert_ne!(
                base_image.size, display_image.size,
                "display still distinct from base"
            );
        } else {
            panic!("expected raster body");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reorder_node_one_swaps_z() {
        let dir = temp_dir("reorder");

        persist::add_page_raster(
            &dir,
            None,
            0,
            "r0",
            "A",
            true,
            1.0,
            tf(0.0, 0.0, 1.0),
            &img([2, 2], Color32::RED),
        )
        .unwrap();
        persist::add_page_raster(
            &dir,
            None,
            0,
            "r1",
            "B",
            true,
            1.0,
            tf(0.0, 0.0, 1.0),
            &img([2, 2], Color32::BLUE),
        )
        .unwrap();

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();

        let order_before: Vec<String> = doc
            .page(0)
            .unwrap()
            .nodes
            .iter()
            .map(|n| n.uid.clone())
            .collect();
        assert_eq!(order_before, vec!["r0".to_string(), "r1".to_string()]);

        // Move bottom node (r0) up one: it swaps z with r1, so order flips.
        let changed = doc.reorder_node_one(0, "r0", true);
        assert!(changed, "reorder reports a change");

        let order_after: Vec<String> = doc
            .page(0)
            .unwrap()
            .nodes
            .iter()
            .map(|n| n.uid.clone())
            .collect();
        assert_eq!(
            order_after,
            vec!["r1".to_string(), "r0".to_string()],
            "order flipped"
        );

        // Already at the top: moving up again is a no-op.
        assert!(
            !doc.reorder_node_one(0, "r0", true),
            "top node can't move up"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reorder_group_block_moves_whole_group_past_neighbour() {
        // Order (bottom-to-top): a (g0), b (g0), c (ungrouped). Moving g0 up should hop past c.
        let mut doc = doc_with_empty_page();
        let mut a = raster_node("a", [2, 2], Color32::RED);
        a.group_uid = Some("g0".into());
        let mut b = raster_node("b", [2, 2], Color32::GREEN);
        b.group_uid = Some("g0".into());
        let c = raster_node("c", [2, 2], Color32::BLUE);
        doc.add_node(0, a);
        doc.add_node(0, b);
        doc.add_node(0, c);
        doc.add_group(
            0,
            GroupMeta {
                uid: "g0".into(),
                name: "G".into(),
                visible: true,
                opacity: 1.0,
                collapsed: false,
            },
        );

        let order = |d: &LayerDoc| -> Vec<String> {
            d.page(0)
                .unwrap()
                .nodes
                .iter()
                .map(|n| n.uid.clone())
                .collect()
        };
        assert_eq!(order(&doc), vec!["a", "b", "c"]);

        // Move the g0 block up: it hops past the ungrouped c, preserving intra-group order a<b.
        assert!(doc.reorder_group_block(0, "g0", true), "group block moved");
        assert_eq!(
            order(&doc),
            vec!["c", "a", "b"],
            "group hopped past c, intra-order kept"
        );
        // z stays contiguous and sorted.
        let zs: Vec<u32> = doc.page(0).unwrap().nodes.iter().map(|n| n.z).collect();
        assert_eq!(zs, vec![0, 1, 2], "z reassigned contiguously");

        // Move it back down: g0 hops past c again, restoring the original order.
        assert!(
            doc.reorder_group_block(0, "g0", false),
            "group block moved back"
        );
        assert_eq!(
            order(&doc),
            vec!["a", "b", "c"],
            "moved back to original order"
        );

        // g0 is now the bottom block; moving it down further is a no-op.
        let before = doc.version();
        assert!(
            !doc.reorder_group_block(0, "g0", false),
            "g0 already at the bottom"
        );
        assert_eq!(doc.version(), before, "no-op did not bump version");

        // Unknown group → false.
        assert!(!doc.reorder_group_block(0, "nope", true));
    }

    #[test]
    fn set_z_order_reassigns_by_uid_order() {
        let mut doc = doc_with_empty_page();
        doc.add_node(0, raster_node("a", [2, 2], Color32::RED));
        doc.add_node(0, raster_node("b", [2, 2], Color32::GREEN));
        doc.add_node(0, raster_node("c", [2, 2], Color32::BLUE));

        let order = |d: &LayerDoc| -> Vec<String> {
            d.page(0)
                .unwrap()
                .nodes
                .iter()
                .map(|n| n.uid.clone())
                .collect()
        };
        assert_eq!(order(&doc), vec!["a", "b", "c"]);

        // Reverse the order via an explicit uid list.
        doc.set_z_order(0, &["c".into(), "b".into(), "a".into()]);
        assert_eq!(
            order(&doc),
            vec!["c", "b", "a"],
            "reordered to the listed order"
        );
        let zs: Vec<u32> = doc.page(0).unwrap().nodes.iter().map(|n| n.z).collect();
        assert_eq!(zs, vec![0, 1, 2], "z = position in the list");

        // A partial list places listed nodes first (in list order), unlisted after in prior order.
        doc.set_z_order(0, &["b".into()]);
        assert_eq!(order(&doc)[0], "b", "listed node placed first");
    }

    fn save_png(path: &std::path::Path, w: u32, h: u32, c: [u8; 4]) {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba(c));
        img.save(path).unwrap();
    }

    /// Appends a legacy REFERENCE-only text node (payload in `text_info.json` via `payload_ref`, no
    /// inline `render_data`) onto page 0 of an existing `layers.json`, for the legacy-load tests. The
    /// production text writer (`write_page_text_payload`) always writes the inline v3 payload, so it
    /// can't produce a reference-only node; this mutates the manifest JSON directly.
    #[allow(clippy::too_many_arguments)]
    fn add_legacy_text_ref_node(
        dir: &std::path::Path,
        uid: &str,
        z: u32,
        layer_idx: u32,
        pinned: bool,
        visible: bool,
        opacity: f32,
    ) {
        let path = dir.join("layers.json");
        let mut manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let pages = manifest["pages"].as_array_mut().unwrap();
        let page = pages
            .iter_mut()
            .find(|p| p["img_idx"] == 0)
            .expect("page 0 exists");
        page["tree"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "uid": uid, "name": uid, "kind": "text", "z": z,
                "layer_idx": layer_idx, "pinned": pinned,
                "visible": visible, "opacity": opacity,
                "payload_ref": { "store": "text_info", "uid": uid }
            }));
        fs::write(&path, serde_json::to_string(&manifest).unwrap()).unwrap();
    }

    /// Adds a `text_groups` band entry (layer_idx → z) to page 0's manifest.
    fn add_text_group_band(dir: &std::path::Path, layer_idx: u32, z: u32) {
        let path = dir.join("layers.json");
        let mut manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let page = manifest["pages"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|p| p["img_idx"] == 0)
            .expect("page 0 exists");
        page["text_groups"]
            .as_array_mut()
            .map(|a| a.push(serde_json::json!({ "layer_idx": layer_idx, "z": z, "name": "g" })))
            .unwrap_or_else(|| {
                page["text_groups"] =
                    serde_json::json!([{ "layer_idx": layer_idx, "z": z, "name": "g" }]);
            });
        fs::write(&path, serde_json::to_string(&manifest).unwrap()).unwrap();
    }

    #[test]
    fn degenerate_equal_band_z_two_groups_keep_layer_idx_then_y_order() {
        // ITEM F (insurance): a degenerate manifest where TWO text groups share the SAME band Z. The
        // re-rank's `layer_idx` tiebreak (before page-Y) must reproduce the old `(layer_idx, page-Y)`
        // sub-order exactly: group 0's members (by Y) then group 1's members (by Y).
        let dir = temp_dir("degenerate_two_groups");
        // A raster first, to create the page manifest (and sit at the bottom, z=0).
        persist::add_page_raster(
            &dir,
            None,
            0,
            "r0",
            "R",
            true,
            1.0,
            tf(50.0, 50.0, 1.0),
            &img([2, 2], Color32::RED),
        )
        .unwrap();
        // Both groups at the SAME band Z = 5 (degenerate; normally distinct).
        add_text_group_band(&dir, 0, 5);
        add_text_group_band(&dir, 1, 5);
        // group 0: a (Y=30), b (Y=10); group 1: c (Y=20), d (Y=5). All unpinned at node z=0.
        for uid in ["a", "b", "c", "d"] {
            add_legacy_text_ref_node(
                &dir,
                uid,
                0,
                if matches!(uid, "a" | "b") { 0 } else { 1 },
                false,
                true,
                1.0,
            );
        }
        let mk = |uid: &str, layer_idx: u32, y: f32| {
            save_png(&dir.join(format!("{uid}.png")), 4, 3, [0, 255, 0, 255]);
            serde_json::json!({
                "uid": uid, "img_idx": 0, "overlay_type": "text", "file": format!("{uid}.png"),
                "layer_idx": layer_idx, "img_x_px": 5.0, "img_y_px": y, "rotation_deg": 0.0, "scale": 1.0,
                "render_data": { "text": uid }
            })
        };
        let overlays = serde_json::json!([
            mk("a", 0, 30.0),
            mk("b", 0, 10.0),
            mk("c", 1, 20.0),
            mk("d", 1, 5.0)
        ]);
        fs::write(
            dir.join("text_info.json"),
            serde_json::to_string(&overlays).unwrap(),
        )
        .unwrap();

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([1000, 1000]))
            .unwrap();
        let order: Vec<&str> = doc
            .page(0)
            .unwrap()
            .nodes
            .iter()
            .map(|n| n.uid.as_str())
            .collect();
        // Raster at the bottom (z=0 < group z=5), then (layer_idx asc, then page-Y asc):
        // group0 = b(10), a(30); group1 = d(5), c(20).
        assert_eq!(
            order,
            vec!["r0", "b", "a", "d", "c"],
            "old (layer_idx, page-Y) sub-order reproduced"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_text_group_flattens_to_per_text_bands_preserving_visual_order() {
        // PART 1 (order preservation, the main risk): a legacy chapter with a raster + a TEXT GROUP of
        // 3 unpinned texts (sub-ordered by page-Y) must, after the on-read flatten, keep the SAME visual
        // order (group members ascending page-Y, interleaved with the raster by the group's band Z) while
        // giving every text its OWN unique Z (individually movable). No reshuffle.
        let dir = temp_dir("legacy_group_flatten");

        // Raster r0 at the BOTTOM (add_page_raster assigns z above existing bands → 0 here).
        persist::add_page_raster(
            &dir,
            None,
            0,
            "r0",
            "R",
            true,
            1.0,
            tf(50.0, 50.0, 1.0),
            &img([2, 2], Color32::RED),
        )
        .unwrap();
        // Text group 0 band ABOVE the raster (z = 5). Three unpinned members.
        add_text_group_band(&dir, 0, 5);
        for uid in ["tc", "ta", "tb"] {
            add_legacy_text_ref_node(&dir, uid, 0, 0, false, true, 1.0);
        }
        // text_info.json: members at distinct page-Y (ta=10 top, tb=20 mid, tc=30 bottom). The group
        // sub-order is ASCENDING page-Y, so the in-group bottom-to-top order is ta, tb, tc.
        let mk = |uid: &str, y: f32| {
            save_png(&dir.join(format!("{uid}.png")), 4, 3, [0, 255, 0, 255]);
            serde_json::json!({
                "uid": uid, "img_idx": 0, "overlay_type": "text", "file": format!("{uid}.png"),
                "img_x_px": 5.0, "img_y_px": y, "rotation_deg": 0.0, "scale": 1.0,
                "render_data": { "text": uid }
            })
        };
        let overlays = serde_json::json!([mk("ta", 10.0), mk("tb", 20.0), mk("tc", 30.0)]);
        fs::write(
            dir.join("text_info.json"),
            serde_json::to_string(&overlays).unwrap(),
        )
        .unwrap();

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([1000, 1000]))
            .unwrap();
        let page = doc.page(0).expect("resident");

        // Bottom-to-top: raster (group band was z=5 > raster z=0), then the group members by page-Y.
        let order: Vec<&str> = page.nodes.iter().map(|n| n.uid.as_str()).collect();
        assert_eq!(
            order,
            vec!["r0", "ta", "tb", "tc"],
            "visual order preserved, group flattened by page-Y"
        );
        // Every node now has a UNIQUE, contiguous Z (each text individually movable).
        let zs: Vec<u32> = page.nodes.iter().map(|n| n.z).collect();
        assert_eq!(zs, vec![0, 1, 2, 3], "each band has a unique sequential Z");

        // IDEMPOTENCY: flush to disk → reload → order unchanged (no drift across save/reload).
        doc.flush_page(0, &dir, None).unwrap();
        let mut doc2 = LayerDoc::new();
        doc2.ensure_page_loaded(0, &dir, None, None, &psz([1000, 1000]))
            .unwrap();
        let order2: Vec<String> = doc2
            .page(0)
            .unwrap()
            .nodes
            .iter()
            .map(|n| n.uid.clone())
            .collect();
        assert_eq!(
            order2,
            vec!["r0", "ta", "tb", "tc"],
            "order stable across a save/reload cycle"
        );
        // On disk every text is now pinned-with-explicit-Z; no TextGroup band remains.
        let bands = persist::load_page_bands(&dir, None, 0);
        assert!(
            !bands.iter().any(|b| matches!(
                b,
                crate::models::layer_model::ordering::Band::TextGroup { .. }
            )),
            "groups dissolved into per-text pinned bands after the flush"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_text_node_is_added_on_top() {
        // PART 1(d): a NEW text overlay goes to the TOP of the unified Z (max Z + 1), not auto-by-Y.
        let dir = temp_dir("new_text_top");
        persist::add_page_raster(
            &dir,
            None,
            0,
            "r0",
            "R",
            true,
            1.0,
            tf(50.0, 50.0, 1.0),
            &img([2, 2], Color32::RED),
        )
        .unwrap();
        add_legacy_text_ref_node(&dir, "t0", 5, 0, true, true, 1.0);
        save_png(&dir.join("t0.png"), 4, 3, [0, 255, 0, 255]);
        let overlays = serde_json::json!([{
            "uid": "t0", "img_idx": 0, "overlay_type": "text", "file": "t0.png",
            "img_x_px": 5.0, "img_y_px": 5.0, "rotation_deg": 0.0, "scale": 1.0,
            "render_data": { "text": "t0" }
        }]);
        fs::write(
            dir.join("text_info.json"),
            serde_json::to_string(&overlays).unwrap(),
        )
        .unwrap();

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([1000, 1000]))
            .unwrap();

        // Add a NEW text node (a freshly-created overlay routes through `add_node`).
        let new_node = LayerNode {
            uid: "tnew".into(),
            name: "New".into(),
            kind: NodeKind::Text,
            z: 0, // set on top by add_node
            visible: true,
            opacity: 1.0,
            group_uid: None,
            text_layer_idx: Some(0),
            transform: TransformRec {
                cx: 5.0,
                cy: 1.0,
                rotation: 0.0,
                scale: 1.0,
            }, // very high on page (low Y)
            deform: None,
            generation: 0,
            pixels_dirty: false,
            body: NodeBody::Text {
                render_data: serde_json::json!({ "text": "new" }),
                image: img([2, 2], Color32::GREEN),
                is_image: false,
                payload_uid: "tnew".into(),
                mask_clip: None,
            },
        };
        assert!(doc.add_node(0, new_node));

        // Despite a low page-Y (which auto-Y would have sunk), the new text is on TOP.
        let order: Vec<String> = doc
            .page(0)
            .unwrap()
            .nodes
            .iter()
            .map(|n| n.uid.clone())
            .collect();
        assert_eq!(
            order.last().map(String::as_str),
            Some("tnew"),
            "new text is on top (max Z + 1)"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn flattened_text_can_be_ordered_below_a_raster_and_back() {
        // A flattened text owns its own band, so it can move BELOW a raster (Part 1 unifies text+raster Z)
        // and back, via the doc Z order (the same set_z_order the ⬆/⬇ band move uses).
        let dir = temp_dir("text_below_raster");
        persist::add_page_raster(
            &dir,
            None,
            0,
            "r0",
            "R",
            true,
            1.0,
            tf(50.0, 50.0, 1.0),
            &img([2, 2], Color32::RED),
        )
        .unwrap();
        add_legacy_text_ref_node(&dir, "t0", 5, 0, true, true, 1.0);
        save_png(&dir.join("t0.png"), 4, 3, [0, 255, 0, 255]);
        let overlays = serde_json::json!([{
            "uid": "t0", "img_idx": 0, "overlay_type": "text", "file": "t0.png",
            "img_x_px": 5.0, "img_y_px": 5.0, "rotation_deg": 0.0, "scale": 1.0,
            "render_data": { "text": "t0" }
        }]);
        fs::write(
            dir.join("text_info.json"),
            serde_json::to_string(&overlays).unwrap(),
        )
        .unwrap();

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([1000, 1000]))
            .unwrap();
        // Initially text above raster.
        assert_eq!(
            doc.page(0)
                .unwrap()
                .nodes
                .iter()
                .map(|n| n.uid.clone())
                .collect::<Vec<_>>(),
            vec!["r0".to_string(), "t0".to_string()]
        );
        // Move text BELOW the raster.
        doc.set_z_order(0, &["t0".to_string(), "r0".to_string()]);
        assert_eq!(
            doc.page(0)
                .unwrap()
                .nodes
                .iter()
                .map(|n| n.uid.clone())
                .collect::<Vec<_>>(),
            vec!["t0".to_string(), "r0".to_string()],
            "text now below the raster"
        );
        // And back above.
        doc.set_z_order(0, &["r0".to_string(), "t0".to_string()]);
        assert_eq!(
            doc.page(0)
                .unwrap()
                .nodes
                .iter()
                .map(|n| n.uid.clone())
                .collect::<Vec<_>>(),
            vec!["r0".to_string(), "t0".to_string()]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_page_loaded_reads_text_node_interleaved_by_z() {
        let dir = temp_dir("text_load");

        // A raster at the bottom (z = 0). add_page_raster assigns Z above existing bands → 0 here.
        persist::add_page_raster(
            &dir,
            None,
            0,
            "r0",
            "Bottom",
            true,
            1.0,
            tf(10.0, 20.0, 1.0),
            &img([2, 2], Color32::RED),
        )
        .unwrap();

        // A pinned text node at z = 5 (above the raster), referencing an overlay payload by uid.
        add_legacy_text_ref_node(&dir, "t0", 5, 0, true, false, 0.75);

        // The text overlay PNG + a text_info.json entry keyed by the same uid.
        let png_name = "t0.png";
        save_png(&dir.join(png_name), 4, 3, [0, 255, 0, 255]);
        let overlays = serde_json::json!([
            {
                "uid": "t0",
                "img_idx": 0,
                "overlay_type": "text",
                "file": png_name,
                "img_x_px": 111.0,
                "img_y_px": 222.0,
                "rotation_deg": 0.0,
                "scale": 1.0,
                "render_data": { "text": "Hello", "size": 24 }
            }
        ]);
        fs::write(
            dir.join("text_info.json"),
            serde_json::to_string(&overlays).unwrap(),
        )
        .unwrap();

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([1000, 1000]))
            .unwrap();

        let page = doc.page(0).expect("page resident");
        assert_eq!(page.nodes.len(), 2, "raster + text loaded");
        // Interleaved bottom-to-top by z: raster (z=0) below text (z=5).
        assert_eq!(page.nodes[0].uid, "r0");
        assert!(page.nodes[0].is_raster());
        assert_eq!(page.nodes[1].uid, "t0");
        assert!(page.nodes[1].is_text());
        assert!(
            page.nodes[0].z < page.nodes[1].z,
            "text sorts above the raster"
        );

        let t = doc.node(0, "t0").unwrap();
        assert!(t.is_text());
        assert!(!t.visible, "visible carried from the text node");
        assert!(
            (t.opacity - 0.75).abs() < 1e-6,
            "opacity carried from the text node"
        );
        // Transform decoded from the overlay payload (img_x/y → center, deg→rad).
        assert!((t.transform.cx - 111.0).abs() < 1e-6);
        assert!((t.transform.cy - 222.0).abs() < 1e-6);
        assert!((t.transform.rotation - 0.0).abs() < 1e-6);
        assert!((t.transform.scale - 1.0).abs() < 1e-6);
        assert_eq!(
            t.display_image().size,
            [4, 3],
            "rendered overlay PNG loaded"
        );
        if let NodeBody::Text {
            render_data,
            payload_uid,
            ..
        } = &t.body
        {
            assert_eq!(payload_uid, "t0");
            assert_eq!(render_data["text"], "Hello");
        } else {
            panic!("expected text body");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_page_loaded_loads_overlays_without_layers_json_nodes() {
        // Regression: most overlays live ONLY in text_info.json — a layers.json text NODE exists only
        // for PS pin/group overrides. The doc must load EVERY page overlay from text_info.json, not
        // just the sparse nodes, otherwise PS (which projects its text layers from the doc) shows
        // almost none ("doesn't display text layers, except sometimes one").
        let dir = temp_dir("text_no_node");
        fs::create_dir_all(&dir).unwrap();

        // Two overlays for page 0 + one for page 1, all in text_info.json, with NO layers.json nodes.
        save_png(&dir.join("a.png"), 4, 3, [0, 255, 0, 255]);
        save_png(&dir.join("b.png"), 5, 2, [0, 0, 255, 255]);
        let overlays = serde_json::json!([
            { "uid": "a", "img_idx": 0, "overlay_type": "text", "file": "a.png",
              "img_x_px": 10.0, "img_y_px": 20.0, "rotation_deg": 0.0, "scale": 1.0,
              "render_data": { "text": "A" } },
            { "uid": "b", "img_idx": 0, "overlay_type": "image", "file": "b.png",
              "img_x_px": 30.0, "img_y_px": 40.0, "rotation_deg": 0.0, "scale": 1.0,
              "render_data": {} },
            { "uid": "other", "img_idx": 1, "overlay_type": "text", "file": "a.png" }
        ]);
        fs::write(
            dir.join("text_info.json"),
            serde_json::to_string(&overlays).unwrap(),
        )
        .unwrap();

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        let page = doc.page(0).expect("page resident");
        assert_eq!(
            page.nodes.len(),
            2,
            "both page-0 overlays loaded despite having no layers.json nodes"
        );
        assert!(
            doc.node(0, "a").is_some_and(|n| n.is_text() && n.visible),
            "default visible=true"
        );
        assert!(
            doc.node(0, "b")
                .is_some_and(|node| { matches!(node.body, NodeBody::Text { is_image: true, .. }) }),
            "legacy image overlay retains its image marker"
        );
        assert!(
            doc.node(0, "other").is_none(),
            "page-1 overlay not loaded onto page 0"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn decode_reads_legacy_text_images_dir_and_mints_stable_uid() {
        // A never-migrated legacy chapter keeps its overlays ONLY under `text_images/` (a MIDDLE-format
        // `text_info.json` with img_idx/img_x_px/render_data but NO uid) and has no `layers/layers.json`.
        // `decode_page_payload` must read that dir (when threaded as `legacy_text_dir`) and mint a
        // deterministic uid for the uid-less entry, so the PS editor's doc-fed text is non-empty.
        let root = temp_dir("legacy_text_images");
        let primary = root.join("layers");
        let legacy = root.join("text_images");
        fs::create_dir_all(&primary).unwrap();
        fs::create_dir_all(&legacy).unwrap();

        // MIDDLE-format overlay + its PNG live ONLY in `text_images/` — nothing in the layers dirs.
        let file_name = "typing_overlay_p0000_1700000000.png";
        save_png(&legacy.join(file_name), 7, 4, [255, 0, 0, 255]);
        let overlays = serde_json::json!([
            { "img_idx": 0, "overlay_type": "text", "file": file_name,
              "img_x_px": 12.0, "img_y_px": 34.0, "rotation_deg": 0.0, "scale": 1.0,
              "render_data": { "text": "Legacy" } }
        ]);
        fs::write(
            legacy.join("text_info.json"),
            serde_json::to_string(&overlays).unwrap(),
        )
        .unwrap();

        let expected_uid = text_payload::stable_overlay_uid(file_name);

        // WITH the legacy dir: the overlay resolves to a Text node keyed by the deterministic uid.
        let payload = LayerDoc::decode_page_payload(0, &primary, None, Some(&legacy), &psz([100, 100]))
            .expect("decode succeeds with legacy dir");
        assert_eq!(
            payload.nodes.len(),
            1,
            "the legacy text_images overlay loads when the legacy dir is threaded through"
        );
        let node = &payload.nodes[0];
        assert_eq!(node.uid, expected_uid, "uid-less legacy entry gets the stable uid");
        assert!(node.is_text(), "loaded as a text node");
        assert_eq!(node.display_image().size, [7, 4], "legacy PNG resolved from text_images/");

        // WITHOUT the legacy dir (gate closed / migrated chapter): NO text node is produced.
        let payload_none = LayerDoc::decode_page_payload(0, &primary, None, None, &psz([100, 100]))
            .expect("decode succeeds without legacy dir");
        assert!(
            payload_none.nodes.is_empty(),
            "no legacy text source ⇒ no text node (proves the gate/threading matters)"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ensure_page_loaded_skips_text_node_without_overlay() {
        let dir = temp_dir("text_no_overlay");

        persist::add_page_raster(
            &dir,
            None,
            0,
            "r0",
            "Bottom",
            true,
            1.0,
            tf(0.0, 0.0, 1.0),
            &img([2, 2], Color32::RED),
        )
        .unwrap();
        // A text node whose payload has no matching text_info.json entry → skipped (warned).
        add_legacy_text_ref_node(&dir, "ghost", 3, 0, true, true, 1.0);

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        let page = doc.page(0).unwrap();
        assert_eq!(
            page.nodes.len(),
            1,
            "text node without an overlay is skipped"
        );
        assert!(page.nodes[0].is_raster());

        let _ = fs::remove_dir_all(&dir);
    }

    /// Builds a text `LayerNode` carrying render_data + transform + deform + mask_clip, for flush tests.
    fn text_node_with_payload(uid: &str) -> LayerNode {
        LayerNode {
            uid: uid.into(),
            name: "Текст".into(),
            kind: NodeKind::Text,
            z: 0,
            visible: false,
            opacity: 0.5,
            group_uid: None,
            text_layer_idx: Some(0),
            transform: TransformRec {
                cx: 111.0,
                cy: 222.0,
                rotation: 0.7,
                scale: 1.5,
            },
            deform: Some(DeformRec {
                cols: 2,
                rows: 2,
                points_px: vec![[0.0, 0.0], [10.0, 1.0], [1.0, 12.0], [11.0, 13.0]],
            }),
            generation: 0,
            pixels_dirty: true, // dirty → flush writes the rendered PNG
            body: NodeBody::Text {
                render_data: serde_json::json!({"text": "Hello", "size": 24}),
                image: img([4, 3], Color32::GREEN),
                is_image: false,
                payload_uid: uid.into(),
                mask_clip: Some(true),
            },
        }
    }

    #[test]
    fn text_payload_round_trips_through_flush() {
        // A text node with full inline payload (render_data + transform + deform + mask_clip) flushes
        // into layers.json (no text_info.json) and reloads byte-for-byte identical.
        let dir = temp_dir("text_flush_rt");

        let mut doc = doc_with_empty_page();
        doc.add_node(0, text_node_with_payload("t0"));
        doc.flush_page(0, &dir, None).unwrap();
        // No text_info.json was written: the inline payload is self-sufficient.
        assert!(
            !dir.join("text_info.json").exists(),
            "flush writes no text_info.json"
        );

        let mut doc2 = LayerDoc::new();
        doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        let t = doc2
            .node(0, "t0")
            .expect("inline text node reloads from layers.json alone");
        assert!(t.is_text());
        assert!(!t.visible, "visible round-trips");
        assert!((t.opacity - 0.5).abs() < 1e-6, "opacity round-trips");
        assert!((t.transform.cx - 111.0).abs() < 1e-4, "cx round-trips");
        assert!((t.transform.cy - 222.0).abs() < 1e-4, "cy round-trips");
        assert!(
            (t.transform.rotation - 0.7).abs() < 1e-5,
            "rotation (radians) round-trips"
        );
        assert!((t.transform.scale - 1.5).abs() < 1e-6, "scale round-trips");
        let d = t.deform.as_ref().expect("deform round-trips");
        assert_eq!(
            d.points_px,
            vec![[0.0, 0.0], [10.0, 1.0], [1.0, 12.0], [11.0, 13.0]]
        );
        assert_eq!(t.display_image().size, [4, 3], "rendered PNG round-trips");
        if let NodeBody::Text {
            render_data,
            mask_clip,
            payload_uid,
            ..
        } = &t.body
        {
            assert_eq!(render_data["text"], "Hello");
            assert_eq!(render_data["size"], 24);
            assert_eq!(*mask_clip, Some(true), "mask_clip persisted");
            assert_eq!(payload_uid, "t0");
        } else {
            panic!("expected text body");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn text_flush_preserves_existing_raster_and_vice_versa() {
        // Flushing text payload must not drop a raster node on the same page, and a later raster flush
        // must not drop the text node.
        let dir = temp_dir("text_flush_preserve");

        // Seed a raster on disk, load it, add a text node, flush.
        persist::add_page_raster(
            &dir,
            None,
            0,
            "r0",
            "Pic",
            true,
            1.0,
            tf(5.0, 5.0, 1.0),
            &img([2, 2], Color32::RED),
        )
        .unwrap();
        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        doc.add_node(0, text_node_with_payload("t0"));
        doc.flush_page(0, &dir, None).unwrap();

        // Both kinds present on a fresh reload.
        let mut doc2 = LayerDoc::new();
        doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        assert!(
            doc2.node(0, "r0").is_some_and(LayerNode::is_raster),
            "raster preserved across text flush"
        );
        assert!(
            doc2.node(0, "t0").is_some_and(LayerNode::is_text),
            "text node persisted"
        );

        // The raster is still loadable from persist (kind-filter preserved it).
        let rasters = persist::load_page_rasters(&dir, None, 0).unwrap();
        assert_eq!(rasters.layers.len(), 1, "raster survives in layers.json");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_v2_text_node_migrates_on_read() {
        // A v2 layers.json (text node with payload_ref, NO inline render_data) plus a text_info.json
        // entry must load into a populated NodeBody::Text (migration-on-read).
        let dir = temp_dir("text_migrate_v2");
        fs::create_dir_all(&dir).unwrap();

        // v2-style manifest: a text node referencing text_info.json by payload_ref, no inline payload.
        let manifest = serde_json::json!({
            "schema_version": 2,
            "pages": [{
                "img_idx": 0,
                "tree": [{
                    "uid": "t0", "name": "T", "kind": "text", "z": 3,
                    "layer_idx": 0, "pinned": true,
                    "visible": true, "opacity": 0.8,
                    "payload_ref": { "store": "text_info", "uid": "t0" }
                }]
            }]
        });
        fs::write(
            dir.join("layers.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        // The legacy payload + PNG live in text_info.json.
        save_png(&dir.join("t0.png"), 6, 5, [0, 255, 0, 255]);
        let overlays = serde_json::json!([
            { "uid": "t0", "img_idx": 0, "overlay_type": "text", "file": "t0.png",
              "img_x_px": 50.0, "img_y_px": 60.0, "rotation_deg": 90.0, "scale": 2.0,
              "render_data": { "text": "Legacy" } }
        ]);
        fs::write(
            dir.join("text_info.json"),
            serde_json::to_string(&overlays).unwrap(),
        )
        .unwrap();

        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        let t = doc.node(0, "t0").expect("legacy text node loaded");
        assert!(t.is_text());
        assert!(
            (t.opacity - 0.8).abs() < 1e-6,
            "opacity from the layers.json override"
        );
        // Geometry decoded from the legacy entry (deg→rad).
        assert!((t.transform.cx - 50.0).abs() < 1e-4);
        assert!(
            (t.transform.rotation - std::f32::consts::FRAC_PI_2).abs() < 1e-4,
            "90° → π/2"
        );
        assert!((t.transform.scale - 2.0).abs() < 1e-6);
        assert_eq!(t.display_image().size, [6, 5], "legacy PNG loaded");
        if let NodeBody::Text { render_data, .. } = &t.body {
            assert_eq!(render_data["text"], "Legacy");
        } else {
            panic!("expected text body");
        }

        // Migration: flush writes the page inline; a fresh reload reads from the INLINE payload (name +
        // mask_clip preserved) and does NOT need text_info.json.
        doc.flush_page(0, &dir, None).unwrap();
        // Remove the legacy file: the migrated page must load without it.
        let _ = fs::remove_file(dir.join("text_info.json"));
        let mut doc2 = LayerDoc::new();
        doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        let t = doc2
            .node(0, "t0")
            .expect("migrated text node reloads from inline alone");
        assert_eq!(
            t.name, "T",
            "persisted node name round-trips (not regenerated)"
        );
        assert!(
            (t.transform.scale - 2.0).abs() < 1e-6,
            "geometry migrated to inline"
        );
        if let NodeBody::Text { render_data, .. } = &t.body {
            assert_eq!(
                render_data["text"], "Legacy",
                "render_data migrated to inline"
            );
        } else {
            panic!("expected text body");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn typing_style_edit_flush_reload_preserves_geometry_and_render_data() {
        // Mirrors the typing tab's flow: add a text node, edit geometry + render via the doc mutators,
        // flush text, reload — geometry + render_data + grouping must round-trip from layers.json alone.
        let dir = temp_dir("typing_flow");

        let mut doc = doc_with_empty_page();
        let mut node = text_node_with_payload("ov1");
        node.text_layer_idx = Some(3);
        doc.add_node(0, node);

        // Geometry edit (drag) + a re-render (text change) routed through the doc mutators.
        doc.set_transform(
            0,
            "ov1",
            TransformRec {
                cx: 77.0,
                cy: 88.0,
                rotation: 0.3,
                scale: 2.5,
            },
        );
        doc.set_text_render(
            0,
            "ov1",
            serde_json::json!({"text": "Edited", "size": 40}),
            img([6, 5], Color32::BLUE),
        );
        doc.flush_page_text(0, &dir, None).unwrap();

        let mut doc2 = LayerDoc::new();
        doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        let t = doc2.node(0, "ov1").expect("text node reloads");
        assert!(
            (t.transform.cx - 77.0).abs() < 1e-4,
            "edited cx round-trips"
        );
        assert!(
            (t.transform.scale - 2.5).abs() < 1e-6,
            "edited scale round-trips"
        );
        assert!(
            (t.transform.rotation - 0.3).abs() < 1e-5,
            "edited rotation round-trips"
        );
        assert_eq!(t.text_layer_idx, Some(3), "text-group axis round-trips");
        assert_eq!(
            t.display_image().size,
            [6, 5],
            "re-rendered image round-trips"
        );
        if let NodeBody::Text { render_data, .. } = &t.body {
            assert_eq!(render_data["text"], "Edited");
            assert_eq!(render_data["size"], 40);
        } else {
            panic!("expected text body");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_text_render_marks_dirty_and_flush_reencodes_png() {
        // F2: set_text_render must set pixels_dirty so the next flush re-encodes the rendered PNG with
        // the new pixels (otherwise a re-rendered text keeps a stale PNG on disk).
        let dir = temp_dir("text_reencode");

        let mut doc = doc_with_empty_page();
        doc.add_node(0, text_node_with_payload("t0"));
        // First flush writes the initial PNG (RED-ish from text_node_with_payload: GREEN actually).
        doc.flush_page_text(0, &dir, None).unwrap();
        let png_path = dir.join(persist::text_image_file_name(0, "t0"));
        let before = std::fs::read(&png_path).expect("initial png written");

        // Re-render with a DIFFERENT-sized, different-colored image; set_text_render sets pixels_dirty.
        doc.set_text_render(
            0,
            "t0",
            serde_json::json!({"text": "New"}),
            img([9, 7], Color32::from_rgb(1, 2, 3)),
        );
        assert!(
            doc.node(0, "t0").unwrap().pixels_dirty,
            "set_text_render marks pixels_dirty"
        );
        doc.flush_page_text(0, &dir, None).unwrap();
        let after = std::fs::read(&png_path).expect("png still present");
        assert_ne!(before, after, "flush re-encoded the changed text PNG");

        // Reload sees the new pixels.
        let mut doc2 = LayerDoc::new();
        doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        assert_eq!(
            doc2.node(0, "t0").unwrap().display_image().size,
            [9, 7],
            "new render size persisted"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrated_page_ignores_stale_legacy_text_info() {
        // After a page is migrated (inline node present), a DELETED overlay still lingering in the
        // legacy text_info.json must NOT resurrect on load (the migration gate ignores the legacy file).
        let dir = temp_dir("migrate_gate");

        let mut doc = doc_with_empty_page();
        doc.add_node(0, text_node_with_payload("keep"));
        doc.flush_page_text(0, &dir, None).unwrap();

        // Plant a stale legacy entry for a DIFFERENT uid that is NOT in the inline set.
        save_png(&dir.join("ghost.png"), 3, 3, [255, 0, 0, 255]);
        let overlays = serde_json::json!([
            { "uid": "ghost", "img_idx": 0, "overlay_type": "text", "file": "ghost.png",
              "img_x_px": 1.0, "img_y_px": 1.0, "rotation_deg": 0.0, "scale": 1.0, "render_data": {} }
        ]);
        fs::write(
            dir.join("text_info.json"),
            serde_json::to_string(&overlays).unwrap(),
        )
        .unwrap();

        let mut doc2 = LayerDoc::new();
        doc2.ensure_page_loaded(0, &dir, None, None, &psz([100, 100]))
            .unwrap();
        assert!(doc2.node(0, "keep").is_some(), "inline node loads");
        assert!(
            doc2.node(0, "ghost").is_none(),
            "stale legacy overlay does NOT resurrect on a migrated page"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_page_loaded_ribbon_uses_full_chapter_page_sizes() {
        // Regression (reviewer HIGH): loading a ribbon-format page through the doc must pass the FULL
        // chapter page-size map so the cross-page ribbon scale is solved against real aspects. With
        // non-uniform aspects (page0 100×250, page1 100×300), a single-page map would corrupt page-1
        // geometry. The doc result must equal the shared `migrate_overlay_entries`(full map) reference.
        let dir = temp_dir("ribbon_doc");
        fs::create_dir_all(&dir).unwrap();

        // Two ribbon overlays: one on page 0 (constrains the scale), one on page 1 (under test).
        save_png(&dir.join("a.png"), 4, 3, [0, 255, 0, 255]);
        save_png(&dir.join("b.png"), 4, 3, [0, 0, 255, 255]);
        let overlays = serde_json::json!([
            {"uid":"a","page":"1_1","x":10.0,"y":10.0,"region_w":20.0,"region_h":4.0,
             "file":"a.png","render_data":{"text":"A"}},
            {"uid":"b","page":"1_2","x":10.0,"y":150.0,"region_w":20.0,"region_h":4.0,
             "file":"b.png","render_data":{"text":"B"}},
        ]);
        fs::write(
            dir.join("text_info.json"),
            serde_json::to_string(&overlays).unwrap(),
        )
        .unwrap();

        let full: HashMap<usize, [usize; 2]> =
            [(0, [100, 250]), (1, [100, 300])].into_iter().collect();

        // Reference: the shared codec's full-map migration + per-entry decode for the page-1 entry.
        let raw = text_payload::read_overlay_entries(&[dir.as_path()]);
        let migrated = text_payload::migrate_overlay_entries(&raw, &full, |_| (0.0, 0.0));
        let ref_obj = migrated
            .iter()
            .find_map(|e| {
                e.as_object()
                    .filter(|o| o.get("uid").and_then(Value::as_str) == Some("b"))
            })
            .expect("page-1 entry migrated")
            .clone();
        let ref_placement = text_payload::decode_overlay_placement(&ref_obj, [100, 300]);

        // The doc loads page 1 with the FULL map and must produce the same transform.
        let mut doc = LayerDoc::new();
        doc.ensure_page_loaded(1, &dir, None, None, &full).unwrap();
        let t = doc.node(1, "b").expect("page-1 ribbon overlay loaded");
        assert!(
            (t.transform.cx - ref_placement.transform.cx).abs() < 1e-3,
            "cx matches full-map reference: doc={} ref={}",
            t.transform.cx,
            ref_placement.transform.cx
        );
        assert!(
            (t.transform.cy - ref_placement.transform.cy).abs() < 1e-3,
            "cy matches full-map reference: doc={} ref={}",
            t.transform.cy,
            ref_placement.transform.cy
        );
        // Sanity: a single-page map would have produced a different cy (the corruption case).
        let single: HashMap<usize, [usize; 2]> = [(1, [100, 300])].into_iter().collect();
        let migrated_bad = text_payload::migrate_overlay_entries(&raw, &single, |_| (0.0, 0.0));
        let bad_obj = migrated_bad
            .iter()
            .find_map(|e| {
                e.as_object()
                    .filter(|o| o.get("uid").and_then(Value::as_str) == Some("b"))
            })
            .unwrap()
            .clone();
        let bad_cy = text_payload::decode_overlay_placement(&bad_obj, [100, 300])
            .transform
            .cy;
        assert!(
            (t.transform.cy - bad_cy).abs() > 1e-2,
            "the full-map doc result differs from the buggy single-page result (corruption avoided)"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn text_node_body_helpers() {
        // Construct a Text node directly (disk loading is step 1b) and exercise the Text variant.
        let node = LayerNode {
            uid: "t".into(),
            name: "Hello".into(),
            kind: NodeKind::Text,
            z: 0,
            visible: true,
            opacity: 1.0,
            group_uid: None,
            text_layer_idx: Some(0),
            transform: tf(0.0, 0.0, 1.0),
            deform: None,
            generation: 0,
            pixels_dirty: false,
            body: NodeBody::Text {
                render_data: serde_json::json!({"text": "Hello", "size": 24}),
                image: img([5, 6], Color32::WHITE),
                is_image: false,
                payload_uid: "payload-123".into(),
                mask_clip: None,
            },
        };

        assert!(node.is_text());
        assert!(!node.is_raster());
        assert_eq!(
            node.display_image().size,
            [5, 6],
            "display_image returns the text image"
        );
        if let NodeBody::Text {
            payload_uid,
            render_data,
            ..
        } = &node.body
        {
            assert_eq!(payload_uid, "payload-123");
            assert_eq!(render_data["text"], "Hello");
        } else {
            panic!("expected text body");
        }
    }

    /// Builds a raster `LayerNode` in memory (no disk), for the in-memory mutator tests.
    fn raster_node(uid: &str, size: [usize; 2], c: Color32) -> LayerNode {
        LayerNode {
            uid: uid.into(),
            name: uid.into(),
            kind: NodeKind::Raster,
            z: 0,
            visible: true,
            opacity: 1.0,
            group_uid: None,
            text_layer_idx: None,
            transform: tf(0.0, 0.0, 1.0),
            deform: None,
            generation: 0,
            pixels_dirty: false,
            body: NodeBody::Raster {
                base_image: img(size, c),
                display_image: img(size, c),
                effects: Vec::new(),
                base_file: format!("{uid}.png"),
                mask_clip: None,
            },
        }
    }

    /// Builds a text `LayerNode` in memory (no disk), for the in-memory mutator tests.
    fn text_node(uid: &str, size: [usize; 2], c: Color32) -> LayerNode {
        LayerNode {
            uid: uid.into(),
            name: uid.into(),
            kind: NodeKind::Text,
            z: 0,
            visible: true,
            opacity: 1.0,
            group_uid: None,
            text_layer_idx: Some(0),
            transform: tf(0.0, 0.0, 1.0),
            deform: None,
            generation: 0,
            pixels_dirty: false,
            body: NodeBody::Text {
                render_data: serde_json::json!({"text": "x"}),
                image: img(size, c),
                is_image: false,
                payload_uid: uid.into(),
                mask_clip: None,
            },
        }
    }

    /// A resident, empty page for in-memory mutator tests (no disk involved).
    fn doc_with_empty_page() -> LayerDoc {
        let mut doc = LayerDoc::new();
        doc.pages.insert(
            0,
            DocPage {
                nodes: Vec::new(),
                groups: Vec::new(),
            },
        );
        doc
    }

    #[test]
    fn mutations_bump_the_document_version() {
        let mut doc = doc_with_empty_page();
        let mut last = doc.version();

        // Each mutator that changes something must advance the version.
        let mut assert_bumped = |doc: &LayerDoc, what: &str| {
            assert!(
                doc.version() > last,
                "{what} must bump the version (was {last}, now {})",
                doc.version()
            );
            last = doc.version();
        };

        assert!(doc.add_node(0, raster_node("a", [2, 2], Color32::RED)));
        assert_bumped(&doc, "add_node");
        assert!(doc.add_node(0, raster_node("b", [2, 2], Color32::BLUE)));
        assert_bumped(&doc, "add_node b");

        doc.set_transform(0, "a", tf(5.0, 5.0, 2.0));
        assert_bumped(&doc, "set_transform");
        doc.set_visibility(0, "a", false);
        assert_bumped(&doc, "set_visibility");
        doc.set_opacity(0, "a", 0.5);
        assert_bumped(&doc, "set_opacity");
        doc.set_effects(
            0,
            "a",
            vec![serde_json::json!({"x": 1})],
            img([3, 3], Color32::GREEN),
        );
        assert_bumped(&doc, "set_effects");
        doc.set_raster_pixels(
            0,
            "a",
            img([4, 4], Color32::RED),
            img([4, 4], Color32::RED),
            Vec::new(),
            true,
        );
        assert_bumped(&doc, "set_raster_pixels");
        assert!(doc.reorder_node_one(0, "a", true));
        assert_bumped(&doc, "reorder_node_one");

        let group = GroupMeta {
            uid: "g0".into(),
            name: "G".into(),
            visible: true,
            opacity: 1.0,
            collapsed: false,
        };
        doc.add_group(0, group);
        assert_bumped(&doc, "add_group");
        doc.set_group(0, "a", Some("g0".into()));
        assert_bumped(&doc, "set_group");
        doc.remove_group(0, "g0");
        assert_bumped(&doc, "remove_group");

        doc.add_node(0, text_node("t", [2, 2], Color32::WHITE));
        assert_bumped(&doc, "add_node t");
        doc.set_text_render(
            0,
            "t",
            serde_json::json!({"text": "y"}),
            img([2, 2], Color32::WHITE),
        );
        assert_bumped(&doc, "set_text_render");

        assert!(doc.remove_node(0, "a"));
        assert_bumped(&doc, "remove_node");

        // A no-op (missing node) must NOT bump the version.
        let before_noop = doc.version();
        doc.set_visibility(0, "missing", true);
        assert_eq!(
            doc.version(),
            before_noop,
            "no-op must not bump the version"
        );

        // Evicting a resident page bumps; evicting an absent page does not.
        doc.evict_page(0);
        assert!(doc.version() > before_noop, "evict_page bumps the version");
        let after_evict = doc.version();
        doc.evict_page(0);
        assert_eq!(
            doc.version(),
            after_evict,
            "evicting an absent page is a no-op"
        );
    }

    #[test]
    fn add_node_puts_new_node_on_top_and_keeps_sorted() {
        let mut doc = doc_with_empty_page();

        // First node into an empty page → z = 0.
        assert!(doc.add_node(0, raster_node("a", [2, 2], Color32::RED)));
        assert_eq!(doc.node(0, "a").unwrap().z, 0);

        // Second node → one above the current max.
        assert!(doc.add_node(0, raster_node("b", [2, 2], Color32::BLUE)));
        assert_eq!(doc.node(0, "b").unwrap().z, 1, "new node on top");

        let page = doc.page(0).unwrap();
        assert_eq!(page.nodes.len(), 2);
        // Sorted bottom-to-top by z.
        assert_eq!(page.nodes[0].uid, "a");
        assert_eq!(page.nodes[1].uid, "b");
        assert!(page.nodes[0].z < page.nodes[1].z, "kept sorted by z");

        // Absent page → false.
        assert!(!doc.add_node(99, raster_node("c", [2, 2], Color32::GREEN)));
    }

    #[test]
    fn remove_node_removes_by_uid_and_reports_missing() {
        let mut doc = doc_with_empty_page();
        doc.add_node(0, raster_node("a", [2, 2], Color32::RED));
        doc.add_node(0, raster_node("b", [2, 2], Color32::BLUE));

        assert!(doc.remove_node(0, "a"), "existing node removed");
        assert!(doc.node(0, "a").is_none());
        assert_eq!(doc.page(0).unwrap().nodes.len(), 1);

        assert!(!doc.remove_node(0, "missing"), "missing uid returns false");
        assert!(!doc.remove_node(99, "a"), "absent page returns false");
    }

    #[test]
    fn set_visibility_and_opacity_clamps() {
        let mut doc = doc_with_empty_page();
        doc.add_node(0, raster_node("a", [2, 2], Color32::RED));

        doc.set_visibility(0, "a", false);
        assert!(!doc.node(0, "a").unwrap().visible, "visibility toggled off");
        doc.set_visibility(0, "a", true);
        assert!(doc.node(0, "a").unwrap().visible, "visibility toggled on");

        doc.set_opacity(0, "a", 0.25);
        assert!((doc.node(0, "a").unwrap().opacity - 0.25).abs() < 1e-6);
        doc.set_opacity(0, "a", 5.0);
        assert!(
            (doc.node(0, "a").unwrap().opacity - 1.0).abs() < 1e-6,
            "opacity clamped high"
        );
        doc.set_opacity(0, "a", -3.0);
        assert!(
            (doc.node(0, "a").unwrap().opacity - 0.0).abs() < 1e-6,
            "opacity clamped low"
        );
    }

    #[test]
    fn set_group_then_remove_group_ungroups_members() {
        let mut doc = doc_with_empty_page();
        doc.add_node(0, raster_node("a", [2, 2], Color32::RED));
        doc.add_node(0, raster_node("b", [2, 2], Color32::BLUE));

        let group = GroupMeta {
            uid: "g0".into(),
            name: "Group".into(),
            visible: true,
            opacity: 1.0,
            collapsed: false,
        };
        doc.add_group(0, group.clone());
        assert_eq!(doc.page(0).unwrap().groups.len(), 1);
        // Duplicate uid ignored.
        doc.add_group(0, group);
        assert_eq!(
            doc.page(0).unwrap().groups.len(),
            1,
            "duplicate group uid ignored"
        );

        doc.set_group(0, "a", Some("g0".into()));
        doc.set_group(0, "b", Some("g0".into()));
        assert_eq!(doc.node(0, "a").unwrap().group_uid.as_deref(), Some("g0"));
        assert_eq!(doc.node(0, "b").unwrap().group_uid.as_deref(), Some("g0"));

        doc.remove_group(0, "g0");
        assert!(doc.page(0).unwrap().groups.is_empty(), "GroupMeta dropped");
        assert_eq!(
            doc.node(0, "a").unwrap().group_uid,
            None,
            "member a ungrouped"
        );
        assert_eq!(
            doc.node(0, "b").unwrap().group_uid,
            None,
            "member b ungrouped"
        );
    }

    #[test]
    fn set_raster_pixels_bumps_generation_and_swaps() {
        let mut doc = doc_with_empty_page();
        doc.add_node(0, raster_node("a", [2, 2], Color32::RED));
        let gen_before = doc.node(0, "a").unwrap().generation;

        let effects = vec![serde_json::json!({"type": "blur"})];
        doc.set_raster_pixels(
            0,
            "a",
            img([4, 4], Color32::GREEN),
            img([5, 5], Color32::BLUE),
            effects,
            true,
        );

        let node = doc.node(0, "a").unwrap();
        assert!(
            node.generation > gen_before,
            "set_raster_pixels bumps generation"
        );
        assert!(node.pixels_dirty, "pixels_dirty set");
        if let NodeBody::Raster {
            base_image,
            display_image,
            effects,
            ..
        } = &node.body
        {
            assert_eq!(base_image.size, [4, 4], "base swapped");
            assert_eq!(display_image.size, [5, 5], "display swapped");
            assert_eq!(effects.len(), 1, "effects swapped");
        } else {
            panic!("expected raster body");
        }

        // No-op on a text node.
        doc.add_node(0, text_node("t", [2, 2], Color32::WHITE));
        let t_gen = doc.node(0, "t").unwrap().generation;
        doc.set_raster_pixels(
            0,
            "t",
            img([9, 9], Color32::RED),
            img([9, 9], Color32::RED),
            Vec::new(),
            true,
        );
        assert_eq!(
            doc.node(0, "t").unwrap().generation,
            t_gen,
            "no-op on a text node"
        );
    }

    #[test]
    fn set_text_render_bumps_generation_and_swaps() {
        let mut doc = doc_with_empty_page();
        doc.add_node(0, text_node("t", [2, 2], Color32::WHITE));
        let gen_before = doc.node(0, "t").unwrap().generation;

        doc.set_text_render(
            0,
            "t",
            serde_json::json!({"text": "Hi", "size": 32}),
            img([7, 8], Color32::BLUE),
        );

        let node = doc.node(0, "t").unwrap();
        assert!(
            node.generation > gen_before,
            "set_text_render bumps generation"
        );
        if let NodeBody::Text {
            render_data, image, ..
        } = &node.body
        {
            assert_eq!(render_data["text"], "Hi", "render_data swapped");
            assert_eq!(render_data["size"], 32);
            assert_eq!(image.size, [7, 8], "image swapped");
        } else {
            panic!("expected text body");
        }

        // No-op on a raster node.
        doc.add_node(0, raster_node("r", [2, 2], Color32::RED));
        let r_gen = doc.node(0, "r").unwrap().generation;
        doc.set_text_render(
            0,
            "r",
            serde_json::json!({"x": 1}),
            img([9, 9], Color32::RED),
        );
        assert_eq!(
            doc.node(0, "r").unwrap().generation,
            r_gen,
            "no-op on a raster node"
        );
    }

    /// A compact, comparable fingerprint of a loaded page's nodes + groups, so the split path and the
    /// monolith can be asserted byte-for-byte equal without depending on `LayerNode`/`ColorImage`
    /// equality (which they do not implement). Captures every field that the split must preserve.
    fn page_fingerprint(page: &DocPage) -> String {
        let nodes: Vec<String> = page
            .nodes
            .iter()
            .map(|n| {
                let kind = match n.kind {
                    NodeKind::Raster => "R",
                    NodeKind::Text => "T",
                };
                let img = n.display_image();
                format!(
                    "{kind}|uid={}|name={}|z={}|vis={}|op={:.4}|grp={:?}|tli={:?}|cx={:.4}|cy={:.4}|rot={:.4}|sc={:.4}|deform={}|wh={}x{}",
                    n.uid,
                    n.name,
                    n.z,
                    n.visible,
                    n.opacity,
                    n.group_uid,
                    n.text_layer_idx,
                    n.transform.cx,
                    n.transform.cy,
                    n.transform.rotation,
                    n.transform.scale,
                    n.deform.is_some(),
                    img.size[0],
                    img.size[1],
                )
            })
            .collect();
        let groups: Vec<String> = page
            .groups
            .iter()
            .map(|g| {
                format!(
                    "G|uid={}|name={}|vis={}|op={:.4}|col={}",
                    g.uid, g.name, g.visible, g.opacity, g.collapsed
                )
            })
            .collect();
        format!(
            "nodes=[{}] groups=[{}]",
            nodes.join(", "),
            groups.join(", ")
        )
    }

    /// The off-thread split (`decode_page_payload` + `insert_decoded_page`) must produce a loaded page
    /// IDENTICAL to the legacy synchronous `ensure_page_loaded`, including the legacy `text_info.json`
    /// migration-on-read path (which the off-thread worker now runs lock-free). Also checks the insert
    /// memoization: a second insert onto an already-resident page is a no-op (live edits not clobbered).
    #[test]
    fn decode_then_insert_matches_ensure_page_loaded_incl_legacy_ribbon() {
        let dir = temp_dir("decode_split_eq");

        // A raster at the bottom, a PS group, and a LEGACY text group of three unpinned overlays whose
        // geometry is decoded through the shared codec with the FULL chapter page-size map — exercising
        // the same legacy migration path the worker must reproduce.
        persist::add_page_raster(
            &dir,
            None,
            0,
            "r0",
            "R",
            true,
            1.0,
            tf(50.0, 50.0, 1.0),
            &img([3, 3], Color32::RED),
        )
        .unwrap();
        add_text_group_band(&dir, 0, 5);
        for uid in ["tc", "ta", "tb"] {
            add_legacy_text_ref_node(&dir, uid, 0, 0, false, true, 1.0);
        }
        let mk = |uid: &str, y: f32| {
            save_png(&dir.join(format!("{uid}.png")), 4, 3, [0, 255, 0, 255]);
            serde_json::json!({
                "uid": uid, "img_idx": 0, "overlay_type": "text", "file": format!("{uid}.png"),
                "img_x_px": 5.0, "img_y_px": y, "rotation_deg": 0.0, "scale": 1.0,
                "render_data": { "text": uid }
            })
        };
        let overlays = serde_json::json!([mk("ta", 10.0), mk("tb", 20.0), mk("tc", 30.0)]);
        fs::write(
            dir.join("text_info.json"),
            serde_json::to_string(&overlays).unwrap(),
        )
        .unwrap();

        // FULL chapter page-size map (a second page is present so the ribbon solve is non-degenerate).
        let mut page_sizes = HashMap::new();
        page_sizes.insert(0usize, [1000usize, 1400usize]);
        page_sizes.insert(1usize, [1000usize, 1600usize]);

        // Reference: the synchronous monolith.
        let mut doc_ref = LayerDoc::new();
        doc_ref
            .ensure_page_loaded(0, &dir, None, None, &page_sizes)
            .unwrap();
        let ref_fp = page_fingerprint(doc_ref.page(0).expect("ref page resident"));

        // Split: decode lock-free, then insert.
        let payload =
            LayerDoc::decode_page_payload(0, &dir, None, None, &page_sizes).expect("decode succeeds");
        let mut doc_split = LayerDoc::new();
        let version_before = doc_split.version();
        doc_split.insert_decoded_page(0, payload);
        let split_fp = page_fingerprint(doc_split.page(0).expect("split page resident"));

        assert_eq!(
            ref_fp, split_fp,
            "split path matches ensure_page_loaded (incl. legacy migration)"
        );
        assert!(
            doc_split.version() > version_before,
            "a real insert bumps the doc version (cross-tab re-projection)"
        );

        // MEMOIZATION: a second insert of a freshly-decoded payload must NOT clobber the resident page.
        let payload2 =
            LayerDoc::decode_page_payload(0, &dir, None, None, &page_sizes).expect("decode succeeds");
        let version_after_first = doc_split.version();
        doc_split.insert_decoded_page(0, payload2);
        assert_eq!(
            doc_split.version(),
            version_after_first,
            "insert onto an already-resident page is a no-op (does not bump version)"
        );
        assert_eq!(
            page_fingerprint(doc_split.page(0).unwrap()),
            split_fp,
            "resident page unchanged by the discarded second payload"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_text_ack_cannot_clear_newer_dirty_generation() {
        let dir = temp_dir("text_ack_generation");
        let mut doc = LayerDoc::new();
        doc.pages.insert(0, DocPage {
            nodes: vec![text_node_with_payload("t")],
            groups: Vec::new(),
        });
        doc.enable_background_saver();
        doc.set_text_render(0, "t", serde_json::json!({"text": "first"}), img([2, 2], Color32::RED));
        doc.enqueue_page_text_save(0, &dir, None).unwrap();
        doc.set_text_render(0, "t", serde_json::json!({"text": "second"}), img([2, 2], Color32::BLUE));
        let handle = doc.saver_handle().expect("background saver enabled");
        assert!(handle.barrier_blocking().is_empty());
        doc.poll_save_acks();
        assert!(doc.node(0, "t").unwrap().pixels_dirty, "stale success cannot clear a newer edit");

        doc.enqueue_page_text_save(0, &dir, None).unwrap();
        assert!(handle.barrier_blocking().is_empty());
        doc.poll_save_acks();
        assert!(!doc.node(0, "t").unwrap().pixels_dirty, "current successful acknowledgement settles text dirty");
        doc.shutdown_saver();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_text_ack_leaves_text_dirty() {
        let invalid_parent = temp_dir("failed_text_ack_parent");
        fs::write(&invalid_parent, b"not a directory").unwrap();
        let mut doc = LayerDoc::new();
        doc.pages.insert(0, DocPage {
            nodes: vec![text_node_with_payload("t")],
            groups: Vec::new(),
        });
        doc.enable_background_saver();
        doc.set_text_render(0, "t", serde_json::json!({"text": "dirty"}), img([2, 2], Color32::RED));
        doc.enqueue_page_text_save(0, &invalid_parent.join("layers"), None).unwrap();
        let handle = doc.saver_handle().expect("background saver enabled");
        assert!(handle.barrier_blocking().contains(&0));
        doc.poll_save_acks();
        assert!(doc.node(0, "t").unwrap().pixels_dirty, "failed acknowledgement leaves text dirty");
        doc.shutdown_saver();
        let _ = fs::remove_file(&invalid_parent);
    }

    /// Evicting a page drops its pending save epoch, so a still-unconsumed pre-eviction ack cannot
    /// settle the page after it is re-inserted (and the epoch map does not leak the evicted key).
    #[test]
    fn evict_page_drops_pending_save_epochs() {
        let dir = temp_dir("evict_drops_epoch");
        let mut doc = LayerDoc::new();
        doc.pages.insert(0, DocPage {
            nodes: vec![text_node_with_payload("t")],
            groups: Vec::new(),
        });
        doc.enable_background_saver();
        // Reserve a text epoch and run the real save so an OK ack for it lands in the shared map.
        doc.enqueue_page_text_save(0, &dir, None).unwrap();
        let handle = doc.saver_handle().expect("background saver enabled");
        assert!(handle.barrier_blocking().is_empty());
        // Evict before polling: the pending epoch entry must be dropped with the page.
        doc.evict_page(0);
        assert!(
            !doc.latest_epoch.contains_key(&(0, SaveKind::Text)),
            "evict drops the page's latest_epoch entry"
        );
        // Re-insert a fresh, dirty page 0; the still-unconsumed pre-eviction ack must NOT settle it.
        doc.pages.insert(0, DocPage {
            nodes: vec![text_node_with_payload("t")],
            groups: Vec::new(),
        });
        doc.poll_save_acks();
        assert!(
            doc.node(0, "t").unwrap().pixels_dirty,
            "a pre-eviction ack cannot clear the reloaded page's dirty"
        );
        doc.shutdown_saver();
        let _ = fs::remove_dir_all(&dir);
    }

    /// Adding a dirty node advances its page-kind dirty epoch, so a stale ack for the prior epoch cannot
    /// clear the freshly added node's dirty.
    #[test]
    fn add_node_bumps_dirty_epoch_so_stale_ack_cannot_settle() {
        let dir = temp_dir("add_node_bumps_epoch");
        let mut doc = LayerDoc::new();
        doc.pages.insert(0, DocPage {
            nodes: vec![text_node_with_payload("t")],
            groups: Vec::new(),
        });
        doc.enable_background_saver();
        // Reserve e1 and run the real save for the existing text; its OK ack lands in the map.
        doc.enqueue_page_text_save(0, &dir, None).unwrap();
        let handle = doc.saver_handle().expect("background saver enabled");
        assert!(handle.barrier_blocking().is_empty());
        let e1 = doc.latest_epoch.get(&(0, SaveKind::Text)).copied().expect("text epoch reserved");
        // Add a fresh DIRTY text node: this must advance the (0, Text) dirty epoch past e1.
        assert!(doc.add_node(0, text_node_with_payload("t2")));
        let e2 = doc.latest_epoch.get(&(0, SaveKind::Text)).copied().expect("text epoch present after add");
        assert!(e2 > e1, "add_node of a dirty text node advances the dirty epoch");
        // The pre-add OK ack (epoch e1) must NOT settle the page now that latest_epoch = e2.
        doc.poll_save_acks();
        assert!(
            doc.node(0, "t2").unwrap().pixels_dirty,
            "a stale (e1) ack cannot clear the node added at e2"
        );
        doc.shutdown_saver();
        let _ = fs::remove_dir_all(&dir);
    }
}
