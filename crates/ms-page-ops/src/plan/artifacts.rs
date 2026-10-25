/*
File: page_ops/plan/artifacts.rs

Purpose:
Per-artifact planners for the page-keyed files of both trees: source pages,
clean overlays, layer PNGs, the layers manifest, legacy `text_info.json`,
typing masks, the bubbles document, and the creation of inserted / blank pages.
Each one renames, rewrites, trashes or stages through `PlanBuilder`.

Key functions:
- plan_src_pages(), plan_clean_overlays(), plan_layer_pngs(),
  plan_layers_manifest(), plan_text_info(), plan_typing_masks(),
  plan_bubbles(), plan_creates().

Notes:
A child of `plan.rs`; pure like its parent (no filesystem access).
*/

use super::builder::to_pretty;
use super::geometry::{plan_crop_raster, plan_split_raster_parts};
use super::{ChapterSnapshot, ComposeSource, DELETED_BUBBLES_FILE, DELETED_LAYERS_PAGES_FILE, DELETED_TEXT_INFO_FILE, NewPageContent, OwnedDocKind, PageGeometry, Permutation, PlanBuilder, PlannedCreate, TextInfoLocation, TreeSnapshot, canonical_page_stem, parse_layers_png_page_idx, parse_typing_mask_page_idx, split_name, typing_mask_file_name};
use crate::PageOpError;
use crate::json_remap;
use ms_config as config;
use serde_json::Value;
use std::collections::HashSet;

/// Source page files: rename surviving pages onto the canonical stem of their
/// NEW index (extension preserved), move deleted pages to the trash.
///
/// Under a stitch every source page is trashed instead (its pixels survive in
/// the composed page) and the merged page is staged as a new PNG, regardless of
/// the source pages' extensions. Under a split the cut page is trashed and each
/// part is staged as its own new PNG, likewise regardless of the source
/// extension. Under a crop the cropped page is trashed and ONE rotated+cropped
/// PNG is staged at the same index (the page count does not change).
pub(super) fn plan_src_pages(
    b: &mut PlanBuilder,
    snapshot: &ChapterSnapshot,
    map: &[Option<usize>],
    geometry: PageGeometry<'_>,
) -> Result<(), PageOpError> {
    // Exhaustive by design (see `PageGeometry`): a new pixel-identity operation
    // must fail to compile HERE, so that whoever adds it has to decide what
    // this planner does with it instead of silently falling through to the
    // plain-rename path.
    let (stitch, split, crop) = match geometry {
        PageGeometry::None => (None, None, None),
        PageGeometry::Stitch(geo) => (Some(geo), None, None),
        PageGeometry::Split(geo) => (None, Some(geo), None),
        PageGeometry::Crop(geo) => (None, None, Some(geo)),
    };
    let mut composed: Vec<ComposeSource> = Vec::new();
    for (old_idx, name) in snapshot.page_file_names.iter().enumerate() {
        let from = format!("{}/{}/{name}", snapshot.chapter_rel, config::SRC_DIR);
        if let Some(geo) = crop
            && geo.source_old_idx() == old_idx
        {
            let page_size = *snapshot.page_sizes.get(old_idx).ok_or_else(|| {
                PageOpError::InvalidOp(format!("page {old_idx} has no known pixel size"))
            })?;
            plan_crop_raster(
                b,
                geo,
                &from,
                page_size,
                // Straight alpha: whatever the rotation leaves uncovered at the
                // corners of the new page must be transparent, not painted.
                [0, 0, 0, 0],
                false,
                format!(
                    "{}/{}/{}.png",
                    snapshot.chapter_rel,
                    config::SRC_DIR,
                    canonical_page_stem(geo.source_old_idx())
                ),
            );
            b.trash(from);
            continue;
        }
        if let Some(geo) = split
            && geo.source_old_idx() == old_idx
        {
            let page_size = *snapshot.page_sizes.get(old_idx).ok_or_else(|| {
                PageOpError::InvalidOp(format!("page {old_idx} has no known pixel size"))
            })?;
            plan_split_raster_parts(b, geo, &from, page_size, [0, 0, 0, 0], false, |new_idx| {
                format!(
                    "{}/{}/{}.png",
                    snapshot.chapter_rel,
                    config::SRC_DIR,
                    canonical_page_stem(new_idx)
                )
            });
            b.trash(from);
            continue;
        }
        if let Some(geo) = stitch
            && let Some(placement) = geo.placement(old_idx)
        {
            let page_size = *snapshot.page_sizes.get(old_idx).ok_or_else(|| {
                PageOpError::InvalidOp(format!("page {old_idx} has no known pixel size"))
            })?;
            composed.push(ComposeSource {
                path: from.clone(),
                page_size,
                // A stitch and a split never rotate: the crop indexes the page image
                // directly (`PlacementMap::rotates()` is false for both).
                rotation: None,
                nearest: false,
                crop: placement.crop_rect(),
                dest: placement.placed_rect(),
            });
            b.trash(from);
            continue;
        }
        match map[old_idx] {
            Some(new_idx) => {
                let (_, ext) = split_name(name);
                let ext = ext.ok_or_else(|| {
                    PageOpError::InvalidOp(format!("page file '{name}' has no extension"))
                })?;
                let target = format!(
                    "{}/{}/{}.{ext}",
                    snapshot.chapter_rel,
                    config::SRC_DIR,
                    canonical_page_stem(new_idx)
                );
                b.rename(from, target);
            }
            None => b.trash(from),
        }
    }
    if let Some(geo) = stitch {
        b.create(
            format!(
                "{}/{}/{}.png",
                snapshot.chapter_rel,
                config::SRC_DIR,
                canonical_page_stem(geo.primary_new)
            ),
            NewPageContent::ComposedPng {
                width: geo.canvas[0],
                height: geo.canvas[1],
                background: geo.background,
                sources: composed,
            },
        );
    }
    Ok(())
}

/// Tree-relative path of the clean overlay keyed by `page_stem` inside the tree rooted at
/// `tree_rel`: `<tree_rel>/clean_layers/<canonical clean file name>`. The file name comes from
/// the binding owner, [`crate::clean_binding::clean_overlay_file_name`].
fn clean_overlay_rel_path(tree_rel: &str, page_stem: &str) -> String {
    format!(
        "{tree_rel}/{}/{}",
        config::CLEAN_LAYERS_DIR,
        crate::clean_binding::clean_overlay_file_name(page_stem)
    )
}

/// Clean overlays are keyed by the PAGE'S CURRENT STEM (`clean_binding::clean_overlay_file_name`), in both
/// the committed and unsaved `clean_layers/` dirs.
///
/// Under a stitch they are page-sized rasters and therefore COMPOSED, not
/// renamed: the merged page gets one overlay built on a fully transparent
/// canvas, so a source page without an overlay contributes a transparent hole
/// showing the composed page pixels underneath. When no source page has an
/// overlay, none is created.
///
/// Under a split they are CUT the same way the page image is: each part gets
/// the corresponding crop of the cut page's overlay, and nothing is created
/// when the cut page had no overlay.
///
/// Under a crop they are ROTATED AND CROPPED with the very same transform as
/// the page image, which is what keeps them exactly the new page's size.
pub(super) fn plan_clean_overlays(
    b: &mut PlanBuilder,
    snapshot: &ChapterSnapshot,
    tree: &TreeSnapshot,
    map: &[Option<usize>],
    geometry: PageGeometry<'_>,
) {
    // Exhaustive by design; see `plan_src_pages` and `PageGeometry`.
    let (stitch, split, crop) = match geometry {
        PageGeometry::None => (None, None, None),
        PageGeometry::Stitch(geo) => (Some(geo), None, None),
        PageGeometry::Split(geo) => (None, Some(geo), None),
        PageGeometry::Crop(geo) => (None, None, Some(geo)),
    };
    let mut composed: Vec<ComposeSource> = Vec::new();
    for (old_idx, name) in snapshot.page_file_names.iter().enumerate() {
        let (stem, _) = split_name(name);
        let exists = tree.clean_overlay_stems.contains(stem);
        let from = clean_overlay_rel_path(&tree.tree_rel, stem);
        if let Some(geo) = crop
            && geo.source_old_idx() == old_idx
        {
            if exists && let Some(page_size) = snapshot.page_sizes.get(old_idx).copied() {
                plan_crop_raster(
                    b,
                    geo,
                    &from,
                    page_size,
                    [0, 0, 0, 0],
                    false,
                    clean_overlay_rel_path(&tree.tree_rel, &canonical_page_stem(geo.source_old_idx())),
                );
                b.trash(from);
            }
            continue;
        }
        if let Some(geo) = split
            && geo.source_old_idx() == old_idx
        {
            if exists && let Some(page_size) = snapshot.page_sizes.get(old_idx).copied() {
                plan_split_raster_parts(b, geo, &from, page_size, [0, 0, 0, 0], false, |new_idx| {
                    clean_overlay_rel_path(&tree.tree_rel, &canonical_page_stem(new_idx))
                });
                b.trash(from);
            }
            continue;
        }
        if let Some(geo) = stitch
            && let Some(placement) = geo.placement(old_idx)
        {
            if exists && let Some(page_size) = snapshot.page_sizes.get(old_idx).copied() {
                composed.push(ComposeSource {
                    path: from.clone(),
                    page_size,
                    // A stitch and a split never rotate: the crop indexes the page image
                    // directly (`PlacementMap::rotates()` is false for both).
                    rotation: None,
                    nearest: false,
                    crop: placement.crop_rect(),
                    dest: placement.placed_rect(),
                });
                b.trash(from);
            }
            continue;
        }
        if !exists {
            continue;
        }
        match map[old_idx] {
            Some(new_idx) => {
                let target = clean_overlay_rel_path(&tree.tree_rel, &canonical_page_stem(new_idx));
                b.rename(from, target);
            }
            None => b.trash(from),
        }
    }
    if let Some(geo) = stitch
        && !composed.is_empty()
    {
        b.create(
            clean_overlay_rel_path(&tree.tree_rel, &canonical_page_stem(geo.primary_new)),
            NewPageContent::ComposedPng {
                width: geo.canvas[0],
                height: geo.canvas[1],
                // Straight alpha over the page: uncovered area must stay fully
                // transparent, not painted.
                background: [0, 0, 0, 0],
                sources: composed,
            },
        );
    }
}

/// Layer PNGs are keyed by the page index embedded in their name
/// (`ps_p{page:04}_...`); the index prefix is load-bearing because
/// `persist.rs::prune_orphan_pngs` deletes by that prefix.
///
/// A stitch renames them exactly the same way — the pixels are layer-local and
/// the placement lives in the manifest — but N pages now share one prefix, so
/// two merged pages carrying the same layer uid would collide on one file name.
/// Layer uids are UUIDs (`tabs/ps_editor/layers.rs`, `text_payload.rs`
/// `stable_overlay_uid`) so this cannot normally happen; it is detected and
/// refused rather than silently overwriting a PNG.
///
/// A split is the opposite fan-out: ONE page's PNGs land on SEVERAL prefixes,
/// which the name's embedded index cannot express. Their destination comes
/// from the split's per-tree [`SplitTreeRouting`](super::SplitTreeRouting), which resolved each layer
/// NODE to a part; a PNG that no record of the manifest claims has no part and
/// falls back to the representative part with a warning (it is an orphan that
/// `prune_orphan_pngs` will collect anyway).
///
/// A crop keeps every prefix (the page index does not change), so a surviving
/// layer's PNG is simply left alone. The PNGs referenced ONLY by records that
/// fall entirely outside the kept region are trashed with those records, exactly
/// as a deleted page's are.
///
/// # Errors
/// [`PageOpError::InvalidOp`] when two merged pages would produce the same
/// layer PNG name.
pub(super) fn plan_layer_pngs(
    b: &mut PlanBuilder,
    tree: &TreeSnapshot,
    map: &[Option<usize>],
    geometry: PageGeometry<'_>,
) -> Result<(), PageOpError> {
    // Exhaustive by design; see `plan_src_pages` and `PageGeometry`.
    let (stitch, split, crop) = match geometry {
        PageGeometry::None => (None, None, None),
        PageGeometry::Stitch(geo) => (Some(geo), None, None),
        PageGeometry::Split(geo) => (None, Some(geo), None),
        PageGeometry::Crop(geo) => (None, None, Some(geo)),
    };
    let routing = split.and_then(|geo| geo.routing(&tree.tree_rel));
    let crop_routing = crop.and_then(|geo| geo.routing(&tree.tree_rel));
    let mut merged_targets: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for name in &tree.layers_files {
        let Some(old_idx) = parse_layers_png_page_idx(name) else {
            continue;
        };
        if !name.ends_with(".png") {
            continue;
        }
        let from = format!("{}/{}/{name}", tree.tree_rel, config::LAYERS_DIR);
        if old_idx >= map.len() {
            b.warn(format!(
                "layer PNG '{}' references page {old_idx} beyond the current \
                 {} page(s); left untouched",
                from,
                map.len()
            ));
            continue;
        }
        // A crop keeps the page's index, so a surviving PNG needs no rename at
        // all; only the ones whose last claimant fell outside the frame move —
        // into the trash, never destroyed.
        if let Some(geo) = crop
            && geo.source_old_idx() == old_idx
        {
            if crop_routing.is_some_and(|r| r.is_dropped_file(name)) {
                b.trash(from);
            }
            continue;
        }
        // A split routes the cut page's PNGs by NODE, not by the page index in
        // the name: the parts land on different prefixes.
        if let Some(geo) = split
            && geo.source_old_idx() == old_idx
        {
            // A PNG claimed ONLY by records on deleted parts goes to the trash
            // with those records: it must never be renamed onto a surviving
            // part, whose manifest no longer references it.
            if routing.is_some_and(|r| r.is_deleted_file(name)) {
                b.trash(from);
                continue;
            }
            let new_idx = routing.and_then(|r| r.file_new_idx(name)).unwrap_or_else(|| {
                b.warn(format!(
                    "layer PNG '{from}' of the split page is not referenced by any layer \
                     record; it follows the surviving part that keeps the page's index"
                ));
                // Not `source_old_idx()` by accident: that index IS the first
                // surviving part's new index, and an orphan must land on a part
                // that still exists.
                geo.first_kept_new_idx()
            });
            if let Some(new_name) = json_remap::remap_layers_png_name(name, old_idx, new_idx) {
                let target = format!("{}/{}/{new_name}", tree.tree_rel, config::LAYERS_DIR);
                b.rename(from, target);
            }
            continue;
        }
        match map[old_idx] {
            Some(new_idx) => {
                let new_name = json_remap::remap_layers_png_name(name, old_idx, new_idx)
                    .unwrap_or_else(|| name.clone());
                if stitch.is_some_and(|geo| geo.placement(old_idx).is_some())
                    && let Some(previous) = merged_targets.insert(new_name.clone(), from.clone())
                {
                    return Err(PageOpError::InvalidOp(format!(
                        "stitch would merge pages whose layer PNGs collide: '{previous}' \
                         and '{from}' both become '{new_name}' (duplicate layer uid)"
                    )));
                }
                if new_name != *name {
                    let target =
                        format!("{}/{}/{new_name}", tree.tree_rel, config::LAYERS_DIR);
                    b.rename(from, target);
                }
            }
            None => b.trash(from),
        }
    }
    Ok(())
}

/// `layers/layers.json`: remap `img_idx` and the embedded `ps_p...` file
/// references; everything the operation REMOVES is archived in the trash as
/// `deleted_layers_pages.json` — a page deleted whole contributes its entry, and
/// a page that survives but loses records (a split's deleted part, a crop's
/// removed frame) contributes an entry holding just those records. A stitch
/// folds the merged pages' entries into one; a split partitions the cut page's
/// entry into one entry per part that holds at least one layer; a crop keeps one
/// entry and drops the records the new frame removed.
pub(super) fn plan_layers_manifest(
    b: &mut PlanBuilder,
    tree: &TreeSnapshot,
    map: &[Option<usize>],
    geometry: PageGeometry<'_>,
) -> Result<(), PageOpError> {
    let Some(manifest) = &tree.layers_manifest else {
        return Ok(());
    };
    let remap = json_remap::remap_layers_manifest(manifest, map, geometry, &tree.tree_rel)?;
    for warning in remap.warnings {
        b.warn(format!("{}/layers/layers.json: {warning}", tree.tree_rel));
    }
    if !remap.deleted_pages.is_empty() {
        b.write_trash_extra(
            format!(
                "{}/{}/{DELETED_LAYERS_PAGES_FILE}",
                tree.tree_rel,
                config::LAYERS_DIR
            ),
            to_pretty(&Value::Array(remap.deleted_pages))?,
        );
    }
    if remap.changed {
        let dir = format!("{}/{}", tree.tree_rel, config::LAYERS_DIR);
        b.rewrite_owned_doc(&dir, OwnedDocKind::Layers, tree.layers_manifest_format, to_pretty(&remap.manifest)?);
    }
    Ok(())
}

/// `text_info.json` (legacy typing metadata, possibly present in both the
/// `layers/` and `text_images/` dirs of each tree): remap `img_idx`, drop
/// entries of deleted pages (archived as `deleted_text_info.json`), and move
/// the overlay PNGs referenced only by dropped entries to the trash.
pub(super) fn plan_text_info(
    b: &mut PlanBuilder,
    tree: &TreeSnapshot,
    map: &[Option<usize>],
    geometry: PageGeometry<'_>,
) -> Result<(), PageOpError> {
    for file in &tree.text_info {
        let (dir_name, dir_files) = match file.location {
            TextInfoLocation::LayersDir => (config::LAYERS_DIR, &tree.layers_files),
            TextInfoLocation::TextImagesDir => {
                (config::TEXT_IMAGES_DIR, &tree.text_images_files)
            }
        };
        let remap = json_remap::remap_text_info(&file.entries, map, geometry)?;
        let surviving_files: HashSet<&str> = remap
            .kept
            .iter()
            .filter_map(|entry| entry.get("file").and_then(Value::as_str))
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect();
        for warning in remap.warnings {
            b.warn(format!(
                "{}/{dir_name}/text_info.json: {warning}",
                tree.tree_rel
            ));
        }
        if !remap.deleted.is_empty() {
            b.write_trash_extra(
                format!("{}/{dir_name}/{DELETED_TEXT_INFO_FILE}", tree.tree_rel),
                to_pretty(&Value::Array(remap.deleted))?,
            );
        }
        // Overlay PNGs referenced by dropped entries (plus their optional
        // `*_layout.png` companion) become unreferenced: keep them recoverable
        // in the trash instead of leaving orphans behind.
        for file_name in remap.deleted_files {
            let mut candidates = vec![file_name.clone()];
            if let Some(stem) = file_name.strip_suffix(".png") {
                candidates.push(format!("{stem}_layout.png"));
            }
            for candidate in candidates {
                if dir_files.contains(&candidate) && !surviving_files.contains(candidate.as_str()) {
                    b.trash(format!("{}/{dir_name}/{candidate}", tree.tree_rel));
                }
            }
        }
        if remap.changed {
            let target = format!("{}/{dir_name}/text_info.json", tree.tree_rel);
            b.rewrite_json(target, to_pretty(&Value::Array(remap.kept))?, true);
        }
    }
    Ok(())
}

/// Typing-tab page masks `text_images/mask_page_{idx}.png`.
///
/// Under a stitch these are page-sized rasters and are COMPOSED like the clean
/// overlays, over a black (inactive) background — the loader thresholds the
/// decoded luma at 128, so uncovered canvas reads as "not masked". Under a
/// split they are CUT into one mask per part, the same way the page image is.
///
/// Under a crop they are rotated and cropped with the page's transform, but with
/// NEAREST-NEIGHBOUR sampling: the loader thresholds the mask, so interpolating
/// it would turn a hard edge into a band of half-masked pixels.
pub(super) fn plan_typing_masks(
    b: &mut PlanBuilder,
    snapshot: &ChapterSnapshot,
    tree: &TreeSnapshot,
    map: &[Option<usize>],
    geometry: PageGeometry<'_>,
) {
    // Exhaustive by design; see `plan_src_pages` and `PageGeometry`.
    let (stitch, split, crop) = match geometry {
        PageGeometry::None => (None, None, None),
        PageGeometry::Stitch(geo) => (Some(geo), None, None),
        PageGeometry::Split(geo) => (None, Some(geo), None),
        PageGeometry::Crop(geo) => (None, None, Some(geo)),
    };
    // Page order, not file-name order: `mask_page_10` sorts before
    // `mask_page_2` as a string, and the compose order must be by page.
    let mut by_page: std::collections::BTreeMap<usize, String> =
        std::collections::BTreeMap::new();
    for name in &tree.text_images_files {
        if let Some(old_idx) = parse_typing_mask_page_idx(name) {
            by_page.insert(old_idx, name.clone());
        }
    }
    let mut composed: Vec<ComposeSource> = Vec::new();
    for (old_idx, name) in &by_page {
        let old_idx = *old_idx;
        let from = format!("{}/{}/{name}", tree.tree_rel, config::TEXT_IMAGES_DIR);
        if old_idx >= map.len() {
            b.warn(format!(
                "typing mask '{}' references page {old_idx} beyond the current \
                 {} page(s); left untouched",
                from,
                map.len()
            ));
            continue;
        }
        if let Some(geo) = crop
            && geo.source_old_idx() == old_idx
        {
            if let Some(page_size) = snapshot.page_sizes.get(old_idx).copied() {
                plan_crop_raster(
                    b,
                    geo,
                    &from,
                    page_size,
                    // Opaque black = "no mask here" (the loader thresholds luma
                    // at 128), so the corners the rotation leaves uncovered read
                    // as unmasked.
                    [0, 0, 0, 255],
                    // Categorical values: nearest neighbour keeps the mask
                    // binary instead of fringing its edges with grey.
                    true,
                    format!(
                        "{}/{}/{}",
                        tree.tree_rel,
                        config::TEXT_IMAGES_DIR,
                        typing_mask_file_name(geo.source_old_idx())
                    ),
                );
                b.trash(from);
            }
            continue;
        }
        if let Some(geo) = split
            && geo.source_old_idx() == old_idx
        {
            if let Some(page_size) = snapshot.page_sizes.get(old_idx).copied() {
                // Opaque black = "no mask here" (the loader thresholds luma at
                // 128), matching how the typing tab writes its masks.
                plan_split_raster_parts(b, geo, &from, page_size, [0, 0, 0, 255], true, |new_idx| {
                    format!(
                        "{}/{}/{}",
                        tree.tree_rel,
                        config::TEXT_IMAGES_DIR,
                        typing_mask_file_name(new_idx)
                    )
                });
                b.trash(from);
            }
            continue;
        }
        if let Some(geo) = stitch
            && let Some(placement) = geo.placement(old_idx)
        {
            if let Some(page_size) = snapshot.page_sizes.get(old_idx).copied() {
                composed.push(ComposeSource {
                    path: from.clone(),
                    page_size,
                    // A stitch and a split never rotate: the crop indexes the page image
                    // directly (`PlacementMap::rotates()` is false for both).
                    rotation: None,
                    // A typing mask is CATEGORICAL: its loader thresholds it, so a
                    // smooth pre-resize of an off-size mask would shift its edge.
                    nearest: true,
                    crop: placement.crop_rect(),
                    dest: placement.placed_rect(),
                });
                b.trash(from);
            }
            continue;
        }
        match map[old_idx] {
            Some(new_idx) => {
                let target = format!(
                    "{}/{}/{}",
                    tree.tree_rel,
                    config::TEXT_IMAGES_DIR,
                    typing_mask_file_name(new_idx)
                );
                b.rename(from, target);
            }
            None => b.trash(from),
        }
    }
    if let Some(geo) = stitch
        && !composed.is_empty()
    {
        b.create(
            format!(
                "{}/{}/{}",
                tree.tree_rel,
                config::TEXT_IMAGES_DIR,
                typing_mask_file_name(geo.primary_new)
            ),
            NewPageContent::ComposedPng {
                width: geo.canvas[0],
                height: geo.canvas[1],
                // Opaque black = "no mask here", matching how the typing tab
                // writes its masks (v, v, v, 255).
                background: [0, 0, 0, 255],
                sources: composed,
            },
        );
    }
}

/// `translation_bubbles.json`: remap `img_idx` + `crop_page_idx`; bubbles of
/// deleted pages are removed and archived in the trash as
/// `deleted_bubbles.json`.
pub(super) fn plan_bubbles(
    b: &mut PlanBuilder,
    tree: &TreeSnapshot,
    map: &[Option<usize>],
    geometry: PageGeometry<'_>,
) -> Result<(), PageOpError> {
    let Some(entries) = &tree.bubbles else {
        return Ok(());
    };
    let remap = json_remap::remap_bubbles(entries, map, geometry)?;
    for warning in remap.warnings {
        b.warn(format!(
            "{}/{}: {warning}",
            tree.tree_rel,
            config::BUBBLES_FILE
        ));
    }
    if !remap.deleted.is_empty() {
        b.write_trash_extra(
            format!("{}/{DELETED_BUBBLES_FILE}", tree.tree_rel),
            to_pretty(&Value::Array(remap.deleted))?,
        );
    }
    if remap.changed {
        b.rewrite_owned_doc(&tree.tree_rel, OwnedDocKind::Bubbles, tree.bubbles_format, to_pretty(&Value::Array(remap.kept))?);
    }
    Ok(())
}

/// New pages (insert / blank) staged into `src/` under their canonical stems.
pub(super) fn plan_creates(b: &mut PlanBuilder, snapshot: &ChapterSnapshot, permutation: &Permutation) {
    for page in &permutation.new_pages {
        let target = format!(
            "{}/{}/{}.{}",
            snapshot.chapter_rel,
            config::SRC_DIR,
            canonical_page_stem(page.new_idx),
            page.extension
        );
        let temp = b.next_temp(&target);
        b.creates.push(PlannedCreate {
            temp,
            target,
            content: page.content.clone(),
        });
    }
}
