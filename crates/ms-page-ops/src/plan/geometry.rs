/*
File: page_ops/plan/geometry.rs

Purpose:
Snapshot-dependent geometry resolution and raster compose planning: turns a
stitch / split / crop request plus the chapter snapshot (page pixel sizes,
layer manifests) into the validated `StitchGeometry` / `SplitGeometry` /
`CropGeometry`, and stages the composed / cut / rotated rasters those
operations need.

Key functions:
- build_stitch_geometry() / build_split_geometry() / build_crop_geometry().
- plan_crop_raster() / plan_split_raster_parts(): the shared raster planners
  used for page images, clean overlays, typing masks and detection masks.

Notes:
A child of `plan.rs`; pure like its parent (no filesystem access).
*/

use super::crop::resolve_crop;
use super::split::resolve_split_parts;
use super::{ChapterSnapshot, ComposeRotation, ComposeSource, CropGeometry, NewPageContent, PlacementMap, PlanBuilder, SplitGeometry, StitchGeometry};
use crate::json_remap;
use crate::{PageOpError, SplitAxis, StitchPlacement};
use serde_json::Value;

/// Resolves a stitch request against the chapter snapshot.
///
/// Validates every placement against its page's real pixel size and the canvas
/// (via [`PlacementMap::new`]) and computes the chapter-wide `layer_idx`
/// re-basing: page k's text-group axis is shifted past every axis merged before
/// it, using the maximum seen across BOTH trees' `layers.json` and every
/// `text_info.json`, so all those documents agree on one re-basing.
///
/// # Errors
/// [`PageOpError::InvalidOp`] when the snapshot carries no page sizes, a
/// placement is geometrically invalid, or the re-based axis would overflow.
pub(super) fn build_stitch_geometry(
    snapshot: &ChapterSnapshot,
    placements: &[StitchPlacement],
    canvas: [u32; 2],
    background: [u8; 4],
    map: &[Option<usize>],
) -> Result<StitchGeometry, PageOpError> {
    if snapshot.page_sizes.len() != snapshot.page_file_names.len() {
        return Err(PageOpError::InvalidOp(
            "stitch requires the pixel size of every page, which this chapter \
             snapshot does not carry"
                .to_string(),
        ));
    }
    let mut resolved = std::collections::BTreeMap::new();
    for placement in placements {
        let size = *snapshot.page_sizes.get(placement.page_idx).ok_or_else(|| {
            PageOpError::InvalidOp(format!(
                "stitch page {} has no known pixel size",
                placement.page_idx
            ))
        })?;
        resolved.insert(
            placement.page_idx,
            PlacementMap::new(placement, size, canvas)?,
        );
    }
    let primary_old = *resolved.keys().next().ok_or_else(|| {
        PageOpError::InvalidOp("stitch has no source pages".to_string())
    })?;
    let primary_new = map
        .get(primary_old)
        .copied()
        .flatten()
        .ok_or_else(|| {
            PageOpError::InvalidOp(format!(
                "stitch primary page {primary_old} has no index in the new order"
            ))
        })?;

    // `layer_idx` re-basing: each merged page starts past the highest axis of
    // the pages merged before it, so «Группа текста N» of different pages stay
    // distinct instead of silently fusing.
    let mut layer_idx_offsets = std::collections::BTreeMap::new();
    let mut running: u32 = 0;
    for old_idx in resolved.keys().copied() {
        layer_idx_offsets.insert(old_idx, running);
        if let Some(max_idx) = max_layer_idx_for_page(snapshot, old_idx) {
            running = running
                .checked_add(max_idx)
                .and_then(|v| v.checked_add(1))
                .ok_or_else(|| {
                    PageOpError::InvalidOp(
                        "stitch would overflow the text-group index axis".to_string(),
                    )
                })?;
        }
    }

    Ok(StitchGeometry {
        placements: resolved,
        layer_idx_offsets,
        primary_new,
        canvas,
        background,
    })
}

/// Highest `layer_idx` used by page `old_idx` anywhere in the chapter, or
/// `None` when the page uses no text-group axis at all. Scans both trees'
/// layer manifests (node and text-group records) and every `text_info.json`.
fn max_layer_idx_for_page(snapshot: &ChapterSnapshot, old_idx: usize) -> Option<u32> {
    let mut max: Option<u32> = None;
    let mut observe = |value: Option<&Value>| {
        if let Some(idx) = value
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
        {
            max = Some(max.map_or(idx, |current: u32| current.max(idx)));
        }
    };
    for tree in [&snapshot.committed, &snapshot.unsaved] {
        if let Some(pages) = tree
            .layers_manifest
            .as_ref()
            .and_then(|m| m.get("pages"))
            .and_then(Value::as_array)
        {
            for page in pages {
                if page
                    .get("img_idx")
                    .and_then(Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    != Some(old_idx)
                {
                    continue;
                }
                for rec in page.get("tree").and_then(Value::as_array).into_iter().flatten() {
                    observe(rec.get("layer_idx"));
                }
                for group in page
                    .get("text_groups")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    observe(group.get("layer_idx"));
                }
            }
        }
        for file in &tree.text_info {
            for entry in &file.entries {
                // A missing img_idx reads as page 0, mirroring the typing loader.
                let entry_idx = entry
                    .get("img_idx")
                    .and_then(Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .unwrap_or(0);
                if entry_idx == old_idx {
                    observe(entry.get("layer_idx"));
                }
            }
        }
    }
    max
}

/// Resolves a split request against the chapter snapshot.
///
/// Validates the cut list against the page's real pixel size, builds one
/// [`PlacementMap`] per part, and resolves the per-tree layer routing (which
/// layer node and which layer PNG belongs to which part) by the exact-area
/// rule. Diagnostics produced while routing (an unprobeable text render, a
/// layer PNG no record claims) are collected into the geometry and drained by
/// the caller into the plan's warnings.
///
/// A malformed layer manifest does not fail here: the routing degrades to
/// empty and [`json_remap::remap_layers_manifest`] reports the structural
/// problem with full context when it rewrites the same document.
///
/// # Errors
/// [`PageOpError::InvalidOp`] when the snapshot carries no page sizes, the
/// cuts are not strictly increasing strictly inside the page, the routing
/// arrays are rejected by [`validate_split_routing`](super::split::validate_split_routing), a part's placement is
/// invalid, or one layer PNG of the cut page is claimed by records routed to
/// different SURVIVING parts (see [`json_remap::split_layer_routing`]).
pub(super) fn build_split_geometry(
    snapshot: &ChapterSnapshot,
    page_idx: usize,
    axis: SplitAxis,
    cuts: &[u32],
    order: &[usize],
    deleted: &[bool],
) -> Result<SplitGeometry, PageOpError> {
    if snapshot.page_sizes.len() != snapshot.page_file_names.len() {
        return Err(PageOpError::InvalidOp(
            "split requires the pixel size of every page, which this chapter \
             snapshot does not carry"
                .to_string(),
        ));
    }
    let page_size = *snapshot.page_sizes.get(page_idx).ok_or_else(|| {
        PageOpError::InvalidOp(format!("split page {page_idx} has no known pixel size"))
    })?;
    let mut geometry = resolve_split_parts(page_idx, axis, page_size, cuts, order, deleted)?;

    // Deleting user content must never be silent: state how many parts were
    // dropped and where their artifacts went, before any per-document warning.
    let dropped = geometry.part_count() - geometry.kept_count();
    if dropped > 0 {
        let parts: Vec<usize> = (0..geometry.part_count())
            .filter(|part| geometry.is_deleted_part(*part))
            .collect();
        geometry.warnings.push(format!(
            "split of page {page_idx}: {dropped} of {} part(s) deleted (geometric part(s) \
             {parts:?}); their pixels stay recoverable in the chapter trash, and every \
             bubble, layer, layer PNG, detection block and text overlay routed to them is \
             archived or trashed instead of being moved to a surviving part",
            geometry.part_count()
        ));
    }

    // The routing is per TREE: the committed and the staging manifest are
    // independent documents that may describe different layers for the page.
    for tree in [&snapshot.committed, &snapshot.unsaved] {
        let (routing, warnings) = json_remap::split_layer_routing(
            tree.layers_manifest.as_ref(),
            &tree.layer_png_sizes,
            &geometry,
        )?;
        for warning in warnings {
            geometry
                .warnings
                .push(format!("{}/layers: {warning}", tree.tree_rel));
        }
        geometry.trees.insert(tree.tree_rel.clone(), routing);
    }
    Ok(geometry)
}

/// Resolves a crop request against the chapter snapshot.
///
/// Validates the rotation and the crop rectangle against the page's real pixel
/// size (via [`resolve_crop`]) and resolves, per tree, which layer records of
/// the page fall entirely outside the kept region and which layer PNGs only
/// those records reference. Diagnostics produced while deciding (an unprobeable
/// text render, a record with no placement at all) are collected into the
/// geometry and drained by the caller into the plan's warnings.
///
/// A malformed layer manifest does not fail here: the routing degrades to empty
/// — nothing is dropped, which is the safe direction — and
/// [`json_remap::remap_layers_manifest`] reports the structural problem with
/// full context when it rewrites the same document.
///
/// # Errors
/// [`PageOpError::InvalidOp`] when the snapshot carries no page sizes, the page
/// index has no size, or the request is rejected by [`resolve_crop`].
pub(super) fn build_crop_geometry(
    snapshot: &ChapterSnapshot,
    page_idx: usize,
    quarter_turns: u8,
    angle_deg: f64,
    rect: [u32; 4],
) -> Result<CropGeometry, PageOpError> {
    if snapshot.page_sizes.len() != snapshot.page_file_names.len() {
        return Err(PageOpError::InvalidOp(
            "crop requires the pixel size of every page, which this chapter \
             snapshot does not carry"
                .to_string(),
        ));
    }
    let page_size = *snapshot.page_sizes.get(page_idx).ok_or_else(|| {
        PageOpError::InvalidOp(format!("crop page {page_idx} has no known pixel size"))
    })?;
    let mut geometry = resolve_crop(page_idx, page_size, quarter_turns, angle_deg, rect)?;

    // Per TREE: the committed and the staging manifest are independent
    // documents that may describe different layers for the same page.
    for tree in [&snapshot.committed, &snapshot.unsaved] {
        let (routing, warnings) = json_remap::crop_layer_routing(
            tree.layers_manifest.as_ref(),
            &tree.layer_png_sizes,
            &geometry,
        );
        for warning in warnings {
            geometry
                .warnings
                .push(format!("{}/layers: {warning}", tree.tree_rel));
        }
        geometry.trees.insert(tree.tree_rel.clone(), routing);
    }
    Ok(geometry)
}

/// Stages the ONE rotated + cropped PNG a crop produces from a page-sized
/// raster of the cropped page.
///
/// `from` is the raster's title-relative path at its ORIGINAL location (phase A
/// composes before any rename) and `page_size` the pixel size the rotation is
/// applied to. `nearest` selects nearest-neighbour sampling for a raster whose
/// values are categorical (a typing or detection MASK) instead of continuous.
///
/// Every raster of the page goes through the SAME rotation and the SAME crop
/// rectangle, which is what keeps them all exactly the new page's size.
pub(super) fn plan_crop_raster(
    b: &mut PlanBuilder,
    geo: &CropGeometry,
    from: &str,
    page_size: [u32; 2],
    background: [u8; 4],
    nearest: bool,
    target: String,
) {
    let [width, height] = geo.new_size();
    b.create(
        target,
        NewPageContent::ComposedPng {
            width,
            height,
            background,
            sources: vec![crop_compose_source(geo, from, page_size, nearest)],
        },
    );
}

/// The single [`ComposeSource`] of a crop: the whole page, rotated, cropped to
/// the kept region and placed at the origin of the new page.
fn crop_compose_source(
    geo: &CropGeometry,
    from: &str,
    page_size: [u32; 2],
    nearest: bool,
) -> ComposeSource {
    let rotation = geo.rotation();
    ComposeSource {
        path: from.to_string(),
        page_size,
        // Always recorded, even for a zero rotation: the executor's short
        // circuit (an identity rotation copies the buffer) keeps that case
        // bit-exact, and an explicit recipe is easier to audit in a journal
        // than an implicit one.
        rotation: Some(ComposeRotation {
            quarter_turns: rotation.quarter().steps(),
            angle_deg: rotation.angle_deg(),
        }),
        nearest,
        crop: geo.placement().crop_rect(),
        dest: geo.placement().placed_rect(),
    }
}

/// Stages one cropped PNG per split part from ONE page-sized raster of the cut
/// page.
///
/// `from` is the raster's title-relative path at its ORIGINAL location (phase A
/// composes before any rename), `page_size` the pixel size its crop is
/// expressed in, and `target_for` maps a part's NEW page index to the file the
/// part's raster must end up at. Every part's crop equals its destination, so
/// `encode_composed_png` copies the pixels bit-exactly instead of resampling.
///
/// `nearest` is the raster's sampling policy (see [`ComposeSource::nearest`]).
/// A split never rotates and never scales, so the ONLY step it can reach is the
/// page-size pre-resize — which happens exactly when the source raster is not
/// page-sized, and which must not smooth a MASK.
pub(super) fn plan_split_raster_parts(
    b: &mut PlanBuilder,
    geo: &SplitGeometry,
    from: &str,
    page_size: [u32; 2],
    background: [u8; 4],
    nearest: bool,
    target_for: impl Fn(usize) -> String,
) {
    for part in 0..geo.part_count() {
        // A part with no new index was DELETED by the request: skipping it is
        // the deletion itself — nothing is staged, so the part's pixels are
        // never written and survive only in the trashed source page.
        let (Some(placement), Some(size), Some(new_idx)) = (
            geo.placement(part),
            geo.part_size(part),
            geo.part_new_idx(part),
        ) else {
            continue;
        };
        b.create(
            target_for(new_idx),
            NewPageContent::ComposedPng {
                width: size[0],
                height: size[1],
                background,
                sources: vec![ComposeSource {
                    path: from.to_string(),
                    page_size,
                    // A stitch and a split never rotate: the crop indexes the page image
                    // directly (`PlacementMap::rotates()` is false for both).
                    rotation: None,
                    nearest,
                    crop: placement.crop_rect(),
                    dest: placement.placed_rect(),
                }],
            },
        );
    }
}
