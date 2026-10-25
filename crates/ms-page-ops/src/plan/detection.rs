/*
File: page_ops/plan/detection.rs

Purpose:
Planner for the committed tree's `text_detection/` files
(`{idx:05}_blocks.json` + `{idx:05}_mask.png`): re-keying under ordinary
operations and the stitch / split / crop variants, which remap the blocks and
compose the masks when the trust gates allow it and trash them otherwise.

Key functions:
- plan_detection(): entry point called by `build_plan`.
- plan_stitch_detection() / plan_split_detection() / plan_crop_detection().

Notes:
A child of `plan.rs`; pure like its parent (no filesystem access).
*/

use super::builder::to_pretty;
use super::geometry::{plan_crop_raster, plan_split_raster_parts};
use super::{ChapterSnapshot, ComposeSource, CropGeometry, DetectionBlocks, DetectionFiles, NewPageContent, PageGeometry, PlanBuilder, SplitGeometry, StitchGeometry, detection_blocks_file_name, detection_mask_file_name};
use crate::PageOpError;
use crate::json_remap;
use ms_config as config;
use serde_json::Value;

/// `text_detection/` (committed tree only): `{idx:05}_blocks.json` +
/// `{idx:05}_mask.png`, with the `mask_file` reference inside a parsed blocks
/// file rewritten to the new default mask name.
pub(super) fn plan_detection(
    b: &mut PlanBuilder,
    snapshot: &ChapterSnapshot,
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
    if let Some(geo) = stitch {
        plan_stitch_detection(b, snapshot, geo)?;
    }
    if let Some(geo) = split {
        plan_split_detection(b, snapshot, geo)?;
    }
    if let Some(geo) = crop {
        plan_crop_detection(b, snapshot, geo)?;
    }
    for det in &snapshot.detection {
        let old_idx = det.page_idx;
        // Stitched pages were handled as one merged group above.
        if stitch.is_some_and(|geo| geo.placement(old_idx).is_some()) {
            continue;
        }
        // The split page was cut into one document per part above.
        if split.is_some_and(|geo| geo.source_old_idx() == old_idx) {
            continue;
        }
        // The cropped page's document was rewritten (or trashed) above.
        if crop.is_some_and(|geo| geo.source_old_idx() == old_idx) {
            continue;
        }
        let dir = format!("{}/{}", snapshot.chapter_rel, config::TEXT_DETECTION_DIR);
        let blocks_from = format!("{dir}/{}", detection_blocks_file_name(old_idx));
        let mask_from = format!("{dir}/{}", detection_mask_file_name(old_idx));
        if old_idx >= map.len() {
            b.warn(format!(
                "text-detection files for page {old_idx} reference a page beyond the \
                 current {} page(s); left untouched",
                map.len()
            ));
            continue;
        }
        match map[old_idx] {
            Some(new_idx) => {
                if det.has_mask {
                    b.rename(
                        mask_from,
                        format!("{dir}/{}", detection_mask_file_name(new_idx)),
                    );
                }
                let blocks_target = format!("{dir}/{}", detection_blocks_file_name(new_idx));
                match &det.blocks {
                    Some(DetectionBlocks::Parsed(value)) => {
                        let (remapped, changed) =
                            json_remap::remap_detection_blocks(value, old_idx, new_idx);
                        if changed {
                            // Content changed (`mask_file` reference): journal
                            // the remapped body at the NEW path and discard the
                            // superseded original.
                            let content = to_pretty(&remapped)?;
                            b.discard(blocks_from);
                            b.write_json(blocks_target, content);
                        } else if blocks_from != blocks_target {
                            b.rename(blocks_from, blocks_target);
                        }
                    }
                    Some(DetectionBlocks::Opaque) if blocks_from != blocks_target => {
                        b.warn(format!(
                            "{blocks_from}: not valid JSON; renamed without rewriting \
                             its mask_file reference"
                        ));
                        b.rename(blocks_from, blocks_target);
                    }
                    Some(DetectionBlocks::Opaque) | None => {}
                }
            }
            None => {
                if det.blocks.is_some() {
                    b.trash(blocks_from);
                }
                if det.has_mask {
                    b.trash(mask_from);
                }
            }
        }
    }
    Ok(())
}

/// Text detection of the STITCHED pages, as one group.
///
/// The detector's blocks live in absolute source-page pixels, so they can be
/// merged — but only when every stitched page's document is trustworthy: valid
/// JSON, `source_size` equal to the page's real pixel size, and (if it has a
/// mask) `mask_size` equal to `source_size`. A downscaled or stale mask cannot
/// be remapped without inventing a scale factor, so the whole group is moved to
/// the trash with a warning instead — detection output is regenerable, and this
/// is a deliberate, documented degradation rather than a silent wrong remap.
///
/// Either way the stitched pages' own detection files are trashed: they are
/// keyed to page indices that no longer exist.
///
/// # Errors
/// [`PageOpError::Json`] when the merged document cannot be built or
/// re-serialized.
fn plan_stitch_detection(
    b: &mut PlanBuilder,
    snapshot: &ChapterSnapshot,
    geo: &StitchGeometry,
) -> Result<(), PageOpError> {
    let dir = format!("{}/{}", snapshot.chapter_rel, config::TEXT_DETECTION_DIR);
    let sources: Vec<&DetectionFiles> = snapshot
        .detection
        .iter()
        .filter(|det| geo.placement(det.page_idx).is_some())
        .collect();
    if sources.is_empty() {
        return Ok(());
    }

    let mut blockers: Vec<String> = Vec::new();
    let mut mergeable: Vec<(usize, &Value)> = Vec::new();
    for det in &sources {
        match &det.blocks {
            Some(DetectionBlocks::Parsed(value)) => {
                let page_size = snapshot
                    .page_sizes
                    .get(det.page_idx)
                    .copied()
                    .unwrap_or([0, 0]);
                match json_remap::detection_merge_blocker(
                    value,
                    det.has_mask,
                    page_size,
                    det.page_idx,
                ) {
                    Some(reason) => blockers.push(reason),
                    None => mergeable.push((det.page_idx, value)),
                }
            }
            Some(DetectionBlocks::Opaque) => blockers.push(format!(
                "page {}: blocks file is not valid JSON",
                det.page_idx
            )),
            // A mask with no blocks file is never loaded (the loader keys on the
            // blocks file); it is trashed below and needs no merge decision.
            None => {}
        }
    }

    for det in &sources {
        if det.blocks.is_some() {
            b.trash(format!("{dir}/{}", detection_blocks_file_name(det.page_idx)));
        }
        if det.has_mask {
            b.trash(format!("{dir}/{}", detection_mask_file_name(det.page_idx)));
        }
    }

    if !blockers.is_empty() {
        b.warn(format!(
            "text detection of the stitched pages was moved to the trash instead of \
             being remapped ({}); re-run detection on the merged page",
            blockers.join("; ")
        ));
        return Ok(());
    }
    if mergeable.is_empty() {
        return Ok(());
    }

    // Detection masks are page-sized here (verified above), so they compose
    // exactly like the typing masks, over a black "nothing detected" canvas.
    let mut composed: Vec<ComposeSource> = Vec::new();
    for det in &sources {
        // Only pages whose blocks document was verified above: an unverified
        // mask has no checked size, and a mask whose page has no blocks file is
        // never loaded anyway (the loader keys on the blocks file).
        if !det.has_mask || !mergeable.iter().any(|(idx, _)| *idx == det.page_idx) {
            continue;
        }
        let (Some(placement), Some(page_size)) = (
            geo.placement(det.page_idx),
            snapshot.page_sizes.get(det.page_idx).copied(),
        ) else {
            continue;
        };
        composed.push(ComposeSource {
            path: format!("{dir}/{}", detection_mask_file_name(det.page_idx)),
            page_size,
            // A stitch and a split never rotate: the crop indexes the page image
            // directly (`PlacementMap::rotates()` is false for both).
            rotation: None,
            // A detection mask is CATEGORICAL, and its loader promotes ANY
            // non-zero byte to fully masked, so smoothing it grows the mask.
            nearest: true,
            crop: placement.crop_rect(),
            dest: placement.placed_rect(),
        });
    }
    let mask_name = (!composed.is_empty()).then(|| detection_mask_file_name(geo.primary_new));
    if let Some(name) = &mask_name {
        b.create(
            format!("{dir}/{name}"),
            NewPageContent::ComposedPng {
                width: geo.canvas[0],
                height: geo.canvas[1],
                background: [0, 0, 0, 255],
                sources: composed,
            },
        );
    }
    let merged = json_remap::merge_detection_blocks(&mergeable, geo, mask_name.as_deref())?;
    b.write_json(format!("{dir}/{}", detection_blocks_file_name(geo.primary_new)), to_pretty(&merged)?);
    Ok(())
}

/// Text detection of the SPLIT page, cut into one document per part.
///
/// The detector's blocks live in absolute source-page pixels, so they can be
/// partitioned — but only when the page's document is trustworthy: valid JSON,
/// `source_size` equal to the page's real pixel size, (if it has a mask)
/// `mask_size` equal to `source_size`, and every block carrying a readable
/// `x1`/`y1`/`x2`/`y2` rectangle. Otherwise the page's detection files go
/// to the trash with a warning, exactly like the stitch's degradation, because
/// detection output is regenerable and neither a mask of an unknown scale nor
/// an unroutable block can be cut without inventing an answer.
///
/// Either way the source page's own detection files are trashed: they are keyed
/// to a page index that no longer exists.
///
/// # Errors
/// [`PageOpError::Json`] when a per-part document cannot be re-serialized.
fn plan_split_detection(
    b: &mut PlanBuilder,
    snapshot: &ChapterSnapshot,
    geo: &SplitGeometry,
) -> Result<(), PageOpError> {
    let old_idx = geo.source_old_idx();
    let Some(det) = snapshot
        .detection
        .iter()
        .find(|det| det.page_idx == old_idx)
    else {
        return Ok(());
    };
    let dir = format!("{}/{}", snapshot.chapter_rel, config::TEXT_DETECTION_DIR);
    let blocks_from = format!("{dir}/{}", detection_blocks_file_name(old_idx));
    let mask_from = format!("{dir}/{}", detection_mask_file_name(old_idx));

    let page_size = snapshot.page_sizes.get(old_idx).copied().unwrap_or([0, 0]);
    let document = match &det.blocks {
        Some(DetectionBlocks::Parsed(value)) => {
            // Two gates, one all-or-nothing decision: the document's declared
            // sizes must be trustworthy AND every block must be routable to a
            // part. A block that no part can claim would otherwise be skipped
            // by all of them and vanish from the chapter.
            let blocker = json_remap::detection_merge_blocker(
                value,
                det.has_mask,
                page_size,
                old_idx,
            )
            .or_else(|| json_remap::detection_rect_blocker(value, old_idx));
            match blocker {
                Some(reason) => {
                    b.warn(format!(
                        "text detection of the split page was moved to the trash instead of \
                         being remapped ({reason}); re-run detection on the parts"
                    ));
                    None
                }
                None => Some(value),
            }
        }
        Some(DetectionBlocks::Opaque) => {
            b.warn(format!(
                "text detection of the split page was moved to the trash instead of being \
                 remapped (page {old_idx}: blocks file is not valid JSON); re-run detection \
                 on the parts"
            ));
            None
        }
        // A mask with no blocks file is never loaded (the loader keys on the
        // blocks file); it is trashed below and needs no decision.
        None => None,
    };

    if det.blocks.is_some() {
        b.trash(blocks_from);
    }
    if det.has_mask {
        b.trash(mask_from.clone());
    }

    let Some(document) = document else {
        return Ok(());
    };
    if det.has_mask {
        // Verified page-sized above, so each part's mask is the same crop as
        // the part's page image.
        plan_split_raster_parts(b, geo, &mask_from, page_size, [0, 0, 0, 255], true, |new_idx| {
            format!("{dir}/{}", detection_mask_file_name(new_idx))
        });
    }
    for part in 0..geo.part_count() {
        // A deleted part gets no blocks document at all; `split_detection_blocks`
        // keeps only the blocks owned by the part it is called for, so the blocks
        // of a deleted part are dropped with it and never land on a neighbour.
        let Some(new_idx) = geo.part_new_idx(part) else {
            continue;
        };
        let mask_name = det.has_mask.then(|| detection_mask_file_name(new_idx));
        let cut = json_remap::split_detection_blocks(
            document,
            geo,
            part,
            new_idx,
            mask_name.as_deref(),
        )?;
        b.write_json(format!("{dir}/{}", detection_blocks_file_name(new_idx)), to_pretty(&cut)?);
    }
    Ok(())
}

/// Text detection of the CROPPED page.
///
/// Two outcomes, chosen by the rotation alone:
/// - a PURE QUARTER TURN (or no rotation at all) keeps every stored rectangle
///   axis-aligned, so the document is REMAPPED: blocks are mapped through the
///   crop's placement, blocks lying entirely outside the kept region are
///   dropped (with a count in a warning), `source_size`/`mask_size` become the
///   new page size, and the page's mask is rotated and cropped with
///   nearest-neighbour sampling so it stays binary.
/// - a FINE ANGLE cannot be expressed by an axis-aligned `x1/y1/x2/y2`
///   rectangle, so the page's detection files go to the TRASH with a warning
///   instead of being remapped wrongly — the same all-or-nothing degradation of
///   regenerable data the stitch and the split already apply to an untrustworthy
///   document.
///
/// The trust gates ([`json_remap::detection_merge_blocker`] for the declared
/// sizes and [`json_remap::detection_rect_blocker`] for complete block
/// rectangles) apply to the remapping branch exactly as they do to a split.
///
/// # Errors
/// [`PageOpError::Json`] when the remapped document cannot be built or
/// re-serialized.
fn plan_crop_detection(
    b: &mut PlanBuilder,
    snapshot: &ChapterSnapshot,
    geo: &CropGeometry,
) -> Result<(), PageOpError> {
    let old_idx = geo.source_old_idx();
    let Some(det) = snapshot
        .detection
        .iter()
        .find(|det| det.page_idx == old_idx)
    else {
        return Ok(());
    };
    let dir = format!("{}/{}", snapshot.chapter_rel, config::TEXT_DETECTION_DIR);
    let blocks_from = format!("{dir}/{}", detection_blocks_file_name(old_idx));
    let mask_from = format!("{dir}/{}", detection_mask_file_name(old_idx));
    let page_size = snapshot.page_sizes.get(old_idx).copied().unwrap_or([0, 0]);

    // A fine angle is decided BEFORE the document is even inspected: no
    // axis-aligned block list can describe a page rotated by a free angle, so
    // there is nothing to gate.
    let document = if geo.resamples() {
        b.warn(format!(
            "text detection of page {old_idx} was moved to the trash instead of being \
             remapped: its blocks are axis-aligned rectangles and cannot describe a page \
             rotated by {:.3} deg; re-run detection on the cropped page",
            geo.rotation().total_degrees()
        ));
        None
    } else {
        match &det.blocks {
            Some(DetectionBlocks::Parsed(value)) => {
                let blocker =
                    json_remap::detection_merge_blocker(value, det.has_mask, page_size, old_idx)
                        .or_else(|| json_remap::detection_rect_blocker(value, old_idx));
                match blocker {
                    Some(reason) => {
                        b.warn(format!(
                            "text detection of the cropped page was moved to the trash instead \
                             of being remapped ({reason}); re-run detection on the cropped page"
                        ));
                        None
                    }
                    None => Some(value),
                }
            }
            Some(DetectionBlocks::Opaque) => {
                b.warn(format!(
                    "text detection of the cropped page was moved to the trash instead of \
                     being remapped (page {old_idx}: blocks file is not valid JSON); re-run \
                     detection on the cropped page"
                ));
                None
            }
            // A mask with no blocks file is never loaded (the loader keys on
            // the blocks file); it is trashed below and needs no decision.
            None => None,
        }
    };

    let Some(document) = document else {
        // Degraded: the page's detection artifacts are keyed to a page whose
        // pixel space no longer exists, so they go to the trash rather than
        // staying as a wrong overlay.
        if det.blocks.is_some() {
            b.trash(blocks_from);
        }
        if det.has_mask {
            b.trash(mask_from);
        }
        return Ok(());
    };

    let mask_name = det.has_mask.then(|| detection_mask_file_name(old_idx));
    if det.has_mask {
        // Verified page-sized by the gate above, so the mask takes exactly the
        // page's transform and ends up exactly the new page's size.
        plan_crop_raster(
            b,
            geo,
            &mask_from,
            page_size,
            [0, 0, 0, 255],
            // Categorical values: a detection mask must stay binary.
            true,
            format!("{dir}/{}", detection_mask_file_name(old_idx)),
        );
        b.trash(mask_from);
    }
    let (remapped, dropped) = json_remap::crop_detection_blocks(
        document,
        geo,
        old_idx,
        mask_name.as_deref(),
    )?;
    if dropped > 0 {
        b.warn(format!(
            "crop of page {old_idx}: {dropped} text-detection block(s) lay entirely outside \
             the kept region and were dropped; detection output is regenerable"
        ));
    }
    // The rewritten body supersedes the original at the SAME path (a crop does
    // not change the page index), so the original is discarded rather than
    // renamed.
    b.discard(blocks_from);
    b.write_json(format!("{dir}/{}", detection_blocks_file_name(old_idx)), to_pretty(&remapped)?);
    Ok(())
}
