/*
File: page_ops/plan/permutation.rs

Purpose:
Permutation math: an operation request -> the old-order -> new-order index map
plus the pages it creates, with the request's self-consistency validated
(index ranges, extensions, blank sizes, "at least one page must remain").
Pure; geometry that needs page pixel sizes is validated later by `geometry.rs`.

Key functions:
- permutation_for_op(): the single entry point, dispatching per operation.

Notes:
A child of `plan.rs`; pure like its parent (no filesystem access).
*/

use super::split::validate_split_routing;
use super::{BLANK_MAX_SIDE_PX, BLANK_MIN_SIDE_PX, NewPage, NewPageContent, Permutation, SPLIT_MAX_PARTS, STITCH_MAX_SIDE_PX, STITCH_MAX_TOTAL_PX};
use crate::crop_geometry::PageRotation;
use crate::{PageOpError, PageOpKind, StitchPlacement};
use std::collections::BTreeSet;

/// Computes the old->new permutation for `op` over `old_page_count` pages and
/// validates the request (index ranges, non-empty inputs, supported insert
/// extensions, blank dimensions, "at least one page must remain").
///
/// Pure: never touches the filesystem — file readability is validated by the
/// executor before planning.
///
/// # Errors
/// Returns [`PageOpError::InvalidOp`] when the request does not apply to a
/// chapter with `old_page_count` pages.
pub(crate) fn permutation_for_op(
    op: &PageOpKind,
    old_page_count: usize,
) -> Result<Permutation, PageOpError> {
    match op {
        PageOpKind::Move { from, to } => {
            if *from >= old_page_count || *to >= old_page_count {
                return Err(PageOpError::InvalidOp(format!(
                    "move {from} -> {to} is out of range for {old_page_count} page(s)"
                )));
            }
            // New order = old order with `from` removed and re-inserted at `to`.
            let mut order: Vec<usize> = (0..old_page_count).filter(|i| i != from).collect();
            order.insert(*to, *from);
            let mut old_to_new = vec![None; old_page_count];
            for (new_idx, old_idx) in order.iter().enumerate() {
                old_to_new[*old_idx] = Some(new_idx);
            }
            Ok(Permutation {
                old_to_new,
                new_page_count: old_page_count,
                new_pages: Vec::new(),
            })
        }
        PageOpKind::InsertFiles { at, files } => {
            if files.is_empty() {
                return Err(PageOpError::InvalidOp(
                    "insert requested with an empty file list".to_string(),
                ));
            }
            if *at > old_page_count {
                return Err(PageOpError::InvalidOp(format!(
                    "insert position {at} is out of range for {old_page_count} page(s)"
                )));
            }
            let mut new_pages = Vec::with_capacity(files.len());
            for (offset, file) in files.iter().enumerate() {
                let extension = file
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(str::to_ascii_lowercase)
                    .unwrap_or_default();
                // Must match the extension filter of `project::collect_images`,
                // otherwise the inserted page would be invisible on next load.
                if !matches!(extension.as_str(), "png" | "jpg" | "jpeg") {
                    return Err(PageOpError::InvalidOp(format!(
                        "unsupported page image extension '{extension}' for '{}' \
                         (supported: png, jpg, jpeg)",
                        file.display()
                    )));
                }
                new_pages.push(NewPage {
                    new_idx: at + offset,
                    extension,
                    content: NewPageContent::CopyFile {
                        source: file.clone(),
                    },
                });
            }
            Ok(insert_permutation(old_page_count, *at, new_pages))
        }
        PageOpKind::CreateBlank {
            at,
            width,
            height,
            rgba,
        } => {
            if *at > old_page_count {
                return Err(PageOpError::InvalidOp(format!(
                    "insert position {at} is out of range for {old_page_count} page(s)"
                )));
            }
            let side_ok = |v: u32| (BLANK_MIN_SIDE_PX..=BLANK_MAX_SIDE_PX).contains(&v);
            if !side_ok(*width) || !side_ok(*height) {
                return Err(PageOpError::InvalidOp(format!(
                    "blank page dimensions {width}x{height} are outside \
                     [{BLANK_MIN_SIDE_PX}, {BLANK_MAX_SIDE_PX}]"
                )));
            }
            let new_pages = vec![NewPage {
                new_idx: *at,
                extension: "png".to_string(),
                content: NewPageContent::BlankPng {
                    width: *width,
                    height: *height,
                    rgba: *rgba,
                },
            }];
            Ok(insert_permutation(old_page_count, *at, new_pages))
        }
        PageOpKind::Delete { indices } => {
            if indices.is_empty() {
                return Err(PageOpError::InvalidOp(
                    "delete requested with an empty index list".to_string(),
                ));
            }
            let deleted: BTreeSet<usize> = indices.iter().copied().collect();
            if let Some(max) = deleted.iter().next_back()
                && *max >= old_page_count
            {
                return Err(PageOpError::InvalidOp(format!(
                    "delete index {max} is out of range for {old_page_count} page(s)"
                )));
            }
            if deleted.len() >= old_page_count {
                return Err(PageOpError::InvalidOp(
                    "cannot delete every page: a chapter must keep at least one page".to_string(),
                ));
            }
            let mut old_to_new = Vec::with_capacity(old_page_count);
            let mut kept = 0usize;
            for i in 0..old_page_count {
                if deleted.contains(&i) {
                    old_to_new.push(None);
                } else {
                    old_to_new.push(Some(kept));
                    kept += 1;
                }
            }
            Ok(Permutation {
                old_to_new,
                new_page_count: kept,
                new_pages: Vec::new(),
            })
        }
        PageOpKind::Stitch {
            placements,
            width,
            height,
            background: _,
        } => stitch_permutation(placements, *width, *height, old_page_count),
        PageOpKind::Split {
            page_idx,
            axis: _,
            cuts,
            order,
            deleted,
        } => split_permutation(*page_idx, cuts, order, deleted, old_page_count),
        PageOpKind::Crop {
            page_idx,
            quarter_turns,
            angle_deg,
            rect,
        } => crop_permutation(*page_idx, *quarter_turns, *angle_deg, *rect, old_page_count),
    }
}

/// Index math + request validation of a crop.
///
/// A crop replaces one page with a rotated, cropped version of itself: the page
/// COUNT is unchanged and `old_to_new` is the IDENTITY, so no other page is
/// touched by the permutation. The new page is not listed in `new_pages`: its
/// content is cut out of the chapter snapshot by `plan_src_pages`, exactly as a
/// split part's is, not derivable from the request alone.
///
/// Only the request's self-consistency is checked here — the rotation ranges and
/// a non-empty rectangle. Whether the rectangle fits the ROTATED CANVAS needs
/// the page's pixel size and is checked by [`build_crop_geometry`](super::geometry::build_crop_geometry).
///
/// # Errors
/// [`PageOpError::InvalidOp`] for an out-of-range `page_idx`, `quarter_turns`
/// outside `0..=3`, a fine angle outside `(-45, 45)`, or a zero-sized `rect`.
fn crop_permutation(
    page_idx: usize,
    quarter_turns: u8,
    angle_deg: f64,
    rect: [u32; 4],
    old_page_count: usize,
) -> Result<Permutation, PageOpError> {
    if page_idx >= old_page_count {
        return Err(PageOpError::InvalidOp(format!(
            "crop page {page_idx} is out of range for {old_page_count} page(s)"
        )));
    }
    // Validated here as well as in the geometry so a bad request is refused
    // even by a caller that only asks for the permutation (the page manager
    // pre-validates that way).
    PageRotation::new(quarter_turns, angle_deg)?;
    if rect[2] == 0 || rect[3] == 0 {
        return Err(PageOpError::InvalidOp(format!(
            "crop [{}, {}, {}, {}] of page {page_idx} is empty",
            rect[0], rect[1], rect[2], rect[3]
        )));
    }
    Ok(Permutation {
        old_to_new: (0..old_page_count).map(Some).collect(),
        new_page_count: old_page_count,
        new_pages: Vec::new(),
    })
}

/// Index math + cut/order validation of a split.
///
/// The `kept` surviving parts (`kept = cuts.len() + 1 - deleted parts`) occupy
/// the contiguous run `page_idx ..= page_idx + kept - 1` of the new order,
/// ranked by their `order` value among themselves; every page after the split
/// page shifts up by `kept - 1` (which is 0 when a single part is kept — a
/// crop, and a legal request). `old_to_new[page_idx]` is `page_idx` itself —
/// the one representative the permutation type can carry, which by
/// construction is the surviving part the user ordered FIRST. The created
/// pages are NOT listed in `new_pages`: their content is cut out of the
/// chapter snapshot by `plan_src_pages`, not derivable from the request alone.
///
/// The cut positions are checked only for internal consistency here (strictly
/// increasing, non-empty); whether they fall inside the page needs the page's
/// pixel size and is checked by [`build_split_geometry`](super::geometry::build_split_geometry).
///
/// # Errors
/// [`PageOpError::InvalidOp`] for an out-of-range `page_idx`, an empty cut
/// list, cuts that are not strictly increasing, more than [`SPLIT_MAX_PARTS`]
/// parts, or a routing rejected by [`validate_split_routing`] (bad `order`
/// permutation, `deleted` of the wrong length, every part deleted).
pub(super) fn split_permutation(
    page_idx: usize,
    cuts: &[u32],
    order: &[usize],
    deleted: &[bool],
    old_page_count: usize,
) -> Result<Permutation, PageOpError> {
    if page_idx >= old_page_count {
        return Err(PageOpError::InvalidOp(format!(
            "split page {page_idx} is out of range for {old_page_count} page(s)"
        )));
    }
    if cuts.is_empty() {
        return Err(PageOpError::InvalidOp(
            "split requested with no cut lines".to_string(),
        ));
    }
    if cuts.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(PageOpError::InvalidOp(format!(
            "split cuts {cuts:?} are not strictly increasing"
        )));
    }
    let part_count = cuts.len() + 1;
    if part_count > SPLIT_MAX_PARTS {
        return Err(PageOpError::InvalidOp(format!(
            "split into {part_count} parts exceeds the supported maximum of \
             {SPLIT_MAX_PARTS}"
        )));
    }
    let kept = validate_split_routing(part_count, order, deleted)?;

    // `kept >= 1` is guaranteed above, so this cannot underflow; it is 0 when
    // exactly one part survives, i.e. the split degenerates into a crop and no
    // page index moves.
    let added = kept - 1;
    let old_to_new = (0..old_page_count)
        .map(|i| {
            Some(match i.cmp(&page_idx) {
                std::cmp::Ordering::Less | std::cmp::Ordering::Equal => i,
                std::cmp::Ordering::Greater => i + added,
            })
        })
        .collect();
    Ok(Permutation {
        old_to_new,
        new_page_count: old_page_count + added,
        new_pages: Vec::new(),
    })
}

/// Index math + canvas/selection validation of a stitch.
///
/// Every source page maps to the new index of `primary = min(page_idx)`; pages
/// after a source shift down by the number of sources before them. The created
/// page is NOT listed in `new_pages`: its content is built from the chapter
/// snapshot by `build_stitch_plan`, not from the request alone.
///
/// # Errors
/// [`PageOpError::InvalidOp`] for fewer than two placements, a duplicate or
/// out-of-range `page_idx`, or a canvas outside the supported size bounds.
/// Per-placement geometry is validated later, against the page sizes.
fn stitch_permutation(
    placements: &[StitchPlacement],
    width: u32,
    height: u32,
    old_page_count: usize,
) -> Result<Permutation, PageOpError> {
    if placements.len() < 2 {
        return Err(PageOpError::InvalidOp(format!(
            "stitch needs at least 2 pages, got {}",
            placements.len()
        )));
    }
    let mut sources = BTreeSet::new();
    for placement in placements {
        if placement.page_idx >= old_page_count {
            return Err(PageOpError::InvalidOp(format!(
                "stitch page {} is out of range for {old_page_count} page(s)",
                placement.page_idx
            )));
        }
        if !sources.insert(placement.page_idx) {
            return Err(PageOpError::InvalidOp(format!(
                "stitch lists page {} more than once",
                placement.page_idx
            )));
        }
    }
    if width == 0 || height == 0 || width > STITCH_MAX_SIDE_PX || height > STITCH_MAX_SIDE_PX {
        return Err(PageOpError::InvalidOp(format!(
            "stitched canvas {width}x{height} is outside [1, {STITCH_MAX_SIDE_PX}] per side"
        )));
    }
    if u64::from(width) * u64::from(height) > STITCH_MAX_TOTAL_PX {
        return Err(PageOpError::InvalidOp(format!(
            "stitched canvas {width}x{height} exceeds {STITCH_MAX_TOTAL_PX} pixels"
        )));
    }

    let mut old_to_new = Vec::with_capacity(old_page_count);
    let mut next_new = 0usize;
    let mut merged_new: Option<usize> = None;
    for old_idx in 0..old_page_count {
        if sources.contains(&old_idx) {
            // The first (lowest) source claims the merged page's slot; the rest
            // fold onto it without consuming an index.
            let slot = *merged_new.get_or_insert_with(|| {
                let slot = next_new;
                next_new += 1;
                slot
            });
            old_to_new.push(Some(slot));
        } else {
            old_to_new.push(Some(next_new));
            next_new += 1;
        }
    }
    Ok(Permutation {
        old_to_new,
        new_page_count: next_new,
        new_pages: Vec::new(),
    })
}

/// Shared shift math for `InsertFiles` / `CreateBlank`: old pages before `at`
/// keep their index, pages at or after `at` shift up by the insertion count.
fn insert_permutation(old_page_count: usize, at: usize, new_pages: Vec<NewPage>) -> Permutation {
    let inserted = new_pages.len();
    let old_to_new = (0..old_page_count)
        .map(|i| Some(if i < at { i } else { i + inserted }))
        .collect();
    Permutation {
        old_to_new,
        new_page_count: old_page_count + inserted,
        new_pages,
    }
}
