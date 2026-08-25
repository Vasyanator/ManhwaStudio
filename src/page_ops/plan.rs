/*
File: page_ops/plan.rs

Purpose:
Pure planning layer for structural page operations: turns a `PageOpKind` plus a
snapshot of the chapter's page-keyed artifacts into (a) the full old-order ->
new-order permutation and (b) a journal-serializable action plan (renames,
JSON rewrites, new-file creations, trash moves) that `fs_exec` executes as a
crash-safe transaction.

Key structures:
- Permutation / NewPage: pure index math result for an operation.
- ChapterSnapshot / TreeSnapshot / DetectionFiles: input describing what exists
  on disk (built by `fs_exec::scan_chapter`; tests build it directly).
- PageOpPlan / PlannedMove / PlannedCreate / PlannedJsonWrite /
  PlannedTrashWrite: the action plan persisted verbatim into the journal.

- PlacementMap: the affine of ONE page onto ONE output canvas, shared by the
  stitch and the split.
- StitchGeometry / SplitGeometry / PageGeometry: the resolved pixel-identity
  request every remap of an affected page is routed through.
- SplitTreeRouting: per-tree layer uid -> part and layer PNG -> new page index
  tables of a split (the 1 -> N fan-out `old_to_new` cannot express).
- ComposeSource / NewPageContent::ComposedPng: the recipe `fs_exec` executes to
  build a stitched or cropped raster during phase A.

Key functions:
- permutation_for_op(): op -> permutation + validation (pure).
- build_plan(): snapshot + op -> PageOpPlan (pure; no filesystem access).
- build_stitch_geometry(): stitch request + snapshot -> validated affines.
- build_split_geometry(): split request + snapshot -> validated part affines
  plus the per-tree layer routing.
- canonical page-keyed file-name helpers shared with the scanner.

Notes:
All plan paths are strings relative to the TITLE directory (the parent of the
chapter dir) using '/' separators, because the transaction spans both the
committed chapter tree and its sibling `{chapter}_unsaved` staging tree.
No function in this file touches the filesystem.
*/

use super::{PageOpError, PageOpKind, SplitAxis, StitchPlacement};
use crate::config;
use crate::page_ops::json_remap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

/// Journal file written into the chapter dir for the duration of a transaction.
pub(crate) const JOURNAL_FILE_NAME: &str = "page_ops_journal.json";
/// Phase-B journal slot. It is created durably before the phase-A slot is
/// removed, so the commit marker never requires replacing an existing file.
pub(crate) const JOURNAL_B_FILE_NAME: &str = "page_ops_journal.b.json";
/// Chapter-local trash directory; each transaction uses one `{id}` subfolder.
pub(crate) const TRASH_DIR_NAME: &str = ".pageop_trash";
/// Prefix of every transaction temp file (phase A renames / staged creations).
pub(crate) const TEMP_PREFIX: &str = "__ms_pageop_";
/// Temp suffix chosen so temps never match the image-extension filters of
/// `project::collect_images` or the overlay/mask loaders mid-transaction.
const TEMP_SUFFIX: &str = ".mstmp";
/// Copy of the bubbles removed together with deleted pages (per tree, in trash).
pub(crate) const DELETED_BUBBLES_FILE: &str = "deleted_bubbles.json";
/// Copy of `text_info.json` entries removed together with deleted pages.
pub(crate) const DELETED_TEXT_INFO_FILE: &str = "deleted_text_info.json";
/// Copy of `layers.json` page entries removed together with deleted pages.
pub(crate) const DELETED_LAYERS_PAGES_FILE: &str = "deleted_layers_pages.json";

/// Inclusive bounds for `CreateBlank` dimensions, in pixels.
const BLANK_MIN_SIDE_PX: u32 = 1;
const BLANK_MAX_SIDE_PX: u32 = 20_000;

// ---------------------------------------------------------------------------
// Canonical page-keyed file names.
//
// These formats are the on-disk contract of other modules; each helper cites
// the authoritative definition. They are duplicated here (rather than imported)
// because the originals are private functions of tab modules this engine must
// not depend on.
// ---------------------------------------------------------------------------

/// Canonical zero-based page stem, `000`, `001`, ... — must stay byte-identical
/// to `project::normalize_page_filenames` (`format!("{:03}", page.idx)`) so a
/// reopened chapter needs no renames.
#[must_use]
pub(crate) fn canonical_page_stem(idx: usize) -> String {
    format!("{idx:03}")
}

/// Per-page layer-PNG prefix `ps_p{page:04}_` — mirrors
/// `models/layer_model/persist.rs::page_file_prefix`.
#[must_use]
pub(crate) fn layers_png_prefix(idx: usize) -> String {
    format!("ps_p{idx:04}_")
}

/// Typing-tab page mask `mask_page_{idx}.png` (no zero padding) — mirrors
/// `tabs/typing/mask.rs::mask_file_name_for_page`.
#[must_use]
pub(crate) fn typing_mask_file_name(idx: usize) -> String {
    format!("mask_page_{idx}.png")
}

/// Text-detector blocks file `{idx:05}_blocks.json` — mirrors
/// `tabs/translation/tab.rs::text_detection_blocks_file_path`.
#[must_use]
pub(crate) fn detection_blocks_file_name(idx: usize) -> String {
    format!("{idx:05}_blocks.json")
}

/// Text-detector mask file `{idx:05}_mask.png` — mirrors
/// `tabs/translation/tab.rs::text_detection_mask_file_name` (the cleaning tab
/// uses the same format).
#[must_use]
pub(crate) fn detection_mask_file_name(idx: usize) -> String {
    format!("{idx:05}_mask.png")
}

/// Parses the page index out of a layer PNG name (`ps_p{page:04}_...`).
/// Returns `None` for names that do not match the pattern.
#[must_use]
pub(crate) fn parse_layers_png_page_idx(name: &str) -> Option<usize> {
    let rest = name.strip_prefix("ps_p")?;
    let digits_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    // The canonical form pads to 4 digits; wider indices keep growing digits.
    if digits_len < 4 {
        return None;
    }
    let (digits, tail) = rest.split_at(digits_len);
    if !tail.starts_with('_') {
        return None;
    }
    digits.parse::<usize>().ok()
}

/// Parses the page index out of `mask_page_{idx}.png`.
#[must_use]
pub(crate) fn parse_typing_mask_page_idx(name: &str) -> Option<usize> {
    let stem = name.strip_prefix("mask_page_")?.strip_suffix(".png")?;
    if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse::<usize>().ok()
}

/// Parses the page index out of `{idx}_blocks.json` (5+ digit zero padding).
#[must_use]
pub(crate) fn parse_detection_blocks_page_idx(name: &str) -> Option<usize> {
    parse_detection_page_idx(name, "_blocks.json")
}

/// Parses the page index out of `{idx}_mask.png` (5+ digit zero padding).
#[must_use]
pub(crate) fn parse_detection_mask_page_idx(name: &str) -> Option<usize> {
    parse_detection_page_idx(name, "_mask.png")
}

fn parse_detection_page_idx(name: &str, suffix: &str) -> Option<usize> {
    let digits = name.strip_suffix(suffix)?;
    // The canonical writer pads to 5 digits; accept 5 or more so indices past
    // 99999 (which widen naturally) still parse.
    if digits.len() < 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<usize>().ok()
}

// ---------------------------------------------------------------------------
// Stitch geometry.
//
// A stitch places N source pages on one canvas. Every page-keyed artifact of a
// stitched page — normalized bubble uv, absolute page-px layer transforms,
// detector blocks, page-sized rasters — is remapped through ONE affine per
// source page, `PlacementMap`. No caller re-derives the formula.
// ---------------------------------------------------------------------------

/// Inclusive bound on either side of a stitched canvas, in pixels. Larger than
/// `BLANK_MAX_SIDE_PX` on purpose: a stitched ribbon legitimately exceeds the
/// size a user would ever type into the blank-page dialog.
///
/// Re-exported by `page_ops` so the stitch UI validates against the very value
/// the engine enforces instead of a copy that can silently drift.
pub(crate) const STITCH_MAX_SIDE_PX: u32 = 40_000;
/// Inclusive bound on the stitched canvas area, in pixels. Caps the RGBA
/// staging buffer of the compose step at ~800 MB. Re-exported by `page_ops`
/// for the stitch UI's own pre-validation.
pub(crate) const STITCH_MAX_TOTAL_PX: u64 = 200_000_000;
/// Inclusive upper bound of a placement's uniform scale factor. Re-exported by
/// `page_ops` for the stitch UI's own pre-validation.
pub(crate) const STITCH_MAX_SCALE: f32 = 16.0;
/// Bound on `|dx|` / `|dy|`. Keeps every placement offset inside `i32`, so the
/// affine can be built without a lossy integer -> float cast.
const STITCH_MAX_OFFSET_PX: i64 = 1_000_000;

/// The affine map from one source page's own pixels to an output canvas.
///
/// Shared by both pixel-identity operations: a STITCH gives every merged page
/// one map onto the shared canvas, and a SPLIT gives every part one map from
/// the source page onto that part's own canvas (`crop = the part rect`,
/// `scale = 1`, `dx = dy = 0`).
///
/// Built (and fully validated) by [`PlacementMap::new`]; all remaps of that
/// page's artifacts go through it, so the three on-disk coordinate spaces stay
/// distinguishable: page-normalized uv uses [`PlacementMap::map_u`] /
/// [`PlacementMap::map_v`], absolute page pixels use [`PlacementMap::map_x`] /
/// [`PlacementMap::map_y`], and layer-image-local pixels are never mapped at
/// all (the layer PNGs are not resampled).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PlacementMap {
    page_w: f64,
    page_h: f64,
    crop_x: f64,
    crop_y: f64,
    crop_w: f64,
    crop_h: f64,
    scale: f64,
    dx: f64,
    dy: f64,
    canvas_w: f64,
    canvas_h: f64,
    /// Placed rectangle in canvas pixels, `[x, y, w, h]` (rounded once here so
    /// every consumer agrees on the pixel extent).
    placed: [u32; 4],
}

impl PlacementMap {
    /// Validates one placement against its page and the canvas and returns the
    /// affine.
    ///
    /// `page_size` is the source page's own pixel size, `canvas` the output
    /// page's size. Validation is total: a returned map is guaranteed to place
    /// a non-empty rectangle fully inside the canvas from a non-empty crop
    /// fully inside the page.
    ///
    /// # Errors
    /// [`PageOpError::InvalidOp`] for a zero-sized page or crop, a crop leaving
    /// the page, a non-finite / out-of-range `scale`, an offset beyond
    /// `STITCH_MAX_OFFSET_PX`, or a placed rectangle leaving the canvas.
    pub(crate) fn new(
        placement: &StitchPlacement,
        page_size: [u32; 2],
        canvas: [u32; 2],
    ) -> Result<Self, PageOpError> {
        let idx = placement.page_idx;
        if page_size[0] == 0 || page_size[1] == 0 {
            return Err(PageOpError::InvalidOp(format!(
                "source page {idx} has a zero pixel size {}x{}",
                page_size[0], page_size[1]
            )));
        }
        let [cx, cy, cw, ch] = placement.crop;
        if cw == 0 || ch == 0 {
            return Err(PageOpError::InvalidOp(format!(
                "crop of page {idx} is empty ({cw}x{ch})"
            )));
        }
        let right = cx.checked_add(cw);
        let bottom = cy.checked_add(ch);
        if right.is_none_or(|r| r > page_size[0]) || bottom.is_none_or(|b| b > page_size[1]) {
            return Err(PageOpError::InvalidOp(format!(
                "crop [{cx}, {cy}, {cw}, {ch}] of page {idx} leaves its \
                 {}x{} image",
                page_size[0], page_size[1]
            )));
        }
        if !placement.scale.is_finite()
            || placement.scale <= 0.0
            || placement.scale > STITCH_MAX_SCALE
        {
            return Err(PageOpError::InvalidOp(format!(
                "placement scale {} of page {idx} is outside (0, {STITCH_MAX_SCALE}]",
                placement.scale
            )));
        }
        // Range test rather than `abs()`: `i64::MIN.abs()` panics, and this
        // value comes straight from the UI.
        let offset_ok = -STITCH_MAX_OFFSET_PX..=STITCH_MAX_OFFSET_PX;
        if !offset_ok.contains(&placement.dx) || !offset_ok.contains(&placement.dy) {
            return Err(PageOpError::InvalidOp(format!(
                "placement offset ({}, {}) of page {idx} exceeds +-{STITCH_MAX_OFFSET_PX} px",
                placement.dx, placement.dy
            )));
        }
        // The bound above guarantees both offsets fit `i32`, so the conversion
        // to f64 is exact and needs no lossy `as`.
        let (Ok(dx_i32), Ok(dy_i32)) = (
            i32::try_from(placement.dx),
            i32::try_from(placement.dy),
        ) else {
            return Err(PageOpError::InvalidOp(format!(
                "placement offset ({}, {}) of page {idx} is not representable",
                placement.dx, placement.dy
            )));
        };
        let scale = f64::from(placement.scale);
        let dx = f64::from(dx_i32);
        let dy = f64::from(dy_i32);
        let placed_w = round_to_u32(f64::from(cw) * scale);
        let placed_h = round_to_u32(f64::from(ch) * scale);
        if placed_w == 0 || placed_h == 0 {
            return Err(PageOpError::InvalidOp(format!(
                "placement of page {idx} rounds to an empty rectangle"
            )));
        }
        if dx < 0.0
            || dy < 0.0
            || dx + f64::from(placed_w) > f64::from(canvas[0])
            || dy + f64::from(placed_h) > f64::from(canvas[1])
        {
            return Err(PageOpError::InvalidOp(format!(
                "placement of page {idx} ({placed_w}x{placed_h} at \
                 ({}, {})) leaves the {}x{} canvas",
                placement.dx, placement.dy, canvas[0], canvas[1]
            )));
        }
        // `dx`/`dy` are >= 0 and bounded by the canvas here, so the placed
        // origin is a valid u32.
        let placed_x = round_to_u32(dx);
        let placed_y = round_to_u32(dy);
        Ok(Self {
            page_w: f64::from(page_size[0]),
            page_h: f64::from(page_size[1]),
            crop_x: f64::from(cx),
            crop_y: f64::from(cy),
            crop_w: f64::from(cw),
            crop_h: f64::from(ch),
            scale,
            dx,
            dy,
            canvas_w: f64::from(canvas[0]),
            canvas_h: f64::from(canvas[1]),
            placed: [placed_x, placed_y, placed_w, placed_h],
        })
    }

    /// Maps an absolute X in the source page's pixels to canvas pixels.
    #[must_use]
    pub(crate) fn map_x(&self, x: f64) -> f64 {
        (x - self.crop_x) * self.scale + self.dx
    }

    /// Maps an absolute Y in the source page's pixels to canvas pixels.
    #[must_use]
    pub(crate) fn map_y(&self, y: f64) -> f64 {
        (y - self.crop_y) * self.scale + self.dy
    }

    /// Maps a page-pixel LENGTH (no origin shift) to canvas pixels. Also the
    /// right operation for a stored page-size MULTIPLIER (a layer transform's
    /// `scale`), which is a length in disguise: the layer image it sizes is not
    /// resampled, so its on-page extent must follow the placement.
    #[must_use]
    pub(crate) fn map_len(&self, length: f64) -> f64 {
        length * self.scale
    }

    /// Maps a page-normalized U (0..1 over the source page's width) to a
    /// canvas-normalized U.
    #[must_use]
    pub(crate) fn map_u(&self, u: f64) -> f64 {
        self.map_x(u * self.page_w) / self.canvas_w
    }

    /// Maps a page-normalized V (0..1 over the source page's height) to a
    /// canvas-normalized V.
    #[must_use]
    pub(crate) fn map_v(&self, v: f64) -> f64 {
        self.map_y(v * self.page_h) / self.canvas_h
    }

    /// The placed rectangle in canvas pixels, `[x, y, w, h]`.
    #[must_use]
    pub(crate) fn placed_rect(&self) -> [u32; 4] {
        self.placed
    }

    /// The crop rectangle in the source page's own pixels, `[x, y, w, h]`.
    #[must_use]
    pub(crate) fn crop_rect(&self) -> [u32; 4] {
        [
            round_to_u32(self.crop_x),
            round_to_u32(self.crop_y),
            round_to_u32(self.crop_w),
            round_to_u32(self.crop_h),
        ]
    }
}

/// Rounds a non-negative, finite f64 to `u32`, saturating at both ends.
///
/// The `as` conversion is reached only for a value already proven finite and
/// inside `0 ..= u32::MAX`, where it is exact.
#[must_use]
fn round_to_u32(value: f64) -> u32 {
    let rounded = value.round();
    if !rounded.is_finite() || rounded <= 0.0 {
        return 0;
    }
    if rounded >= f64::from(u32::MAX) {
        return u32::MAX;
    }
    rounded as u32
}

/// Everything the JSON remaps need to know about a stitch: which pages are
/// merged, where each lands, and how their per-page index axes are re-based.
#[derive(Debug, Clone)]
pub(crate) struct StitchGeometry {
    /// Affine per SOURCE page index (current order), ascending.
    placements: std::collections::BTreeMap<usize, PlacementMap>,
    /// Offset added to every `layer_idx` of that source page so the typing
    /// tab's «Группа текста N» axes of the merged pages stay distinct.
    /// Computed chapter-wide (over both trees, `layers.json` AND every
    /// `text_info.json`) so all documents agree on the same re-basing.
    layer_idx_offsets: std::collections::BTreeMap<usize, u32>,
    /// New index of the merged page.
    pub primary_new: usize,
    /// Canvas size of the stitched page, in pixels.
    pub canvas: [u32; 2],
    /// Straight-RGBA fill of the stitched PAGE canvas where no source covers
    /// it. Page-sized overlays and masks always compose over their own neutral
    /// background (transparent / black) instead, never over this one.
    pub background: [u8; 4],
}

impl StitchGeometry {
    /// The affine of `old_idx`, or `None` when that page is not stitched.
    #[must_use]
    pub(crate) fn placement(&self, old_idx: usize) -> Option<&PlacementMap> {
        self.placements.get(&old_idx)
    }

    /// Builds a geometry directly, for tests of the JSON remaps (the normal
    /// path goes through `build_stitch_geometry`, which needs a full snapshot).
    #[cfg(test)]
    pub(crate) fn for_tests(
        placements: Vec<(usize, PlacementMap)>,
        layer_idx_offsets: Vec<(usize, u32)>,
        primary_new: usize,
        canvas: [u32; 2],
    ) -> Self {
        Self {
            placements: placements.into_iter().collect(),
            layer_idx_offsets: layer_idx_offsets.into_iter().collect(),
            primary_new,
            canvas,
            background: [0, 0, 0, 0],
        }
    }

    /// Number of merged source pages.
    #[must_use]
    pub(crate) fn source_count(&self) -> usize {
        self.placements.len()
    }

    /// `layer_idx` offset of a source page; 0 for any other page.
    #[must_use]
    pub(crate) fn layer_idx_offset(&self, old_idx: usize) -> u32 {
        self.layer_idx_offsets.get(&old_idx).copied().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Split geometry.
//
// A split cuts ONE page along parallel lines into N parts, each of which
// becomes a page of its own. Every part reuses the same `PlacementMap`
// primitive as a stitch (`crop` = the part rectangle, `scale` = 1,
// `dx` = `dy` = 0, canvas = the part size), so all geometry helpers are shared
// verbatim. What is specific to a split is ROUTING: because one page becomes
// many, every page-keyed entry must be assigned to exactly ONE part before its
// geometry can be mapped.
// ---------------------------------------------------------------------------

/// Inclusive upper bound on the number of parts one split may produce.
///
/// The cut list alone already bounds the count (cuts are strictly increasing
/// pixel positions inside the page), but that bound is the page's pixel extent
/// — tens of thousands for a ribbon. A request in that range is a malformed
/// request, not a user intent, and each part costs its own page image, clean
/// overlay, typing mask and detection document.
const SPLIT_MAX_PARTS: usize = 256;

/// Per-tree routing of the split page's LAYER records and layer PNG files.
///
/// Both maps are keyed by data that is unique inside one tree: a layer uid and
/// a layer PNG file name. They exist because a split fans ONE page's layers
/// out onto SEVERAL new pages, which the index-keyed `old_to_new` map cannot
/// express — it can only name the representative part.
#[derive(Debug, Clone, Default)]
pub(crate) struct SplitTreeRouting {
    /// Geometric part each layer node of the split page belongs to, by uid.
    node_part: std::collections::HashMap<String, usize>,
    /// New page index each layer PNG of the split page moves to, by file name.
    file_new_idx: std::collections::HashMap<String, usize>,
}

impl SplitTreeRouting {
    /// Builds a routing from the two resolved tables (see
    /// `json_remap::split_layer_routing`, the only production constructor).
    #[must_use]
    pub(crate) fn new(
        node_part: std::collections::HashMap<String, usize>,
        file_new_idx: std::collections::HashMap<String, usize>,
    ) -> Self {
        Self {
            node_part,
            file_new_idx,
        }
    }

    /// Geometric part of the layer node `uid`, or `None` when the uid does not
    /// belong to the split page.
    #[must_use]
    pub(crate) fn node_part(&self, uid: &str) -> Option<usize> {
        self.node_part.get(uid).copied()
    }

    /// New page index of the layer PNG `file`, or `None` when the file is not
    /// owned by a node of the split page.
    #[must_use]
    pub(crate) fn file_new_idx(&self, file: &str) -> Option<usize> {
        self.file_new_idx.get(file).copied()
    }
}

/// Everything the plan and the JSON remaps need to know about a split: where
/// the cuts are, what each part looks like, where each part lands in the new
/// page order, and which layer of which tree belongs to which part.
#[derive(Debug, Clone)]
pub(crate) struct SplitGeometry {
    /// Index of the cut page in the CURRENT page order.
    source_old_idx: usize,
    /// Pixel size `[w, h]` of the cut page, needed to turn the page-normalized
    /// uv of bubbles and typing overlays into the page pixels the cuts live in.
    page_size: [u32; 2],
    /// Orientation of the cut lines.
    axis: SplitAxis,
    /// Part boundaries along the cut axis in SOURCE pixels: `part_count + 1`
    /// strictly increasing values starting at 0 and ending at the page extent,
    /// so geometric part `k` spans `bounds[k] .. bounds[k + 1]`.
    bounds: Vec<f64>,
    /// Affine of each geometric part (`0` = topmost / leftmost).
    parts: Vec<PlacementMap>,
    /// Pixel size `[w, h]` of each geometric part.
    part_sizes: Vec<[u32; 2]>,
    /// Index of each geometric part in the NEW page order.
    part_new_idx: Vec<usize>,
    /// Layer routing per tree, keyed by `TreeSnapshot::tree_rel`.
    trees: std::collections::BTreeMap<String, SplitTreeRouting>,
    /// Plan-time diagnostics collected while resolving the routing (probe
    /// failures, orphan PNGs); drained into the plan's warnings.
    warnings: Vec<String>,
}

impl SplitGeometry {
    /// Index of the page being cut, in the current order.
    #[must_use]
    pub(crate) fn source_old_idx(&self) -> usize {
        self.source_old_idx
    }

    /// Pixel size `[w, h]` of the page being cut.
    #[must_use]
    pub(crate) fn page_size(&self) -> [f64; 2] {
        [f64::from(self.page_size[0]), f64::from(self.page_size[1])]
    }

    /// Geometric part containing a page-normalized `(u, v)` anchor.
    #[must_use]
    pub(crate) fn part_for_uv_point(&self, u: f64, v: f64) -> usize {
        let [w, h] = self.page_size();
        self.part_for_point(u * w, v * h)
    }

    /// Geometric part holding the largest share of a page-normalized
    /// `[u1, v1, u2, v2]` rectangle.
    #[must_use]
    pub(crate) fn part_for_uv_rect(&self, rect: [f64; 4]) -> usize {
        let [w, h] = self.page_size();
        self.part_for_page_rect([rect[0] * w, rect[1] * h, rect[2] * w, rect[3] * h])
    }

    /// Number of geometric parts (`cuts.len() + 1`).
    #[must_use]
    pub(crate) fn part_count(&self) -> usize {
        self.parts.len()
    }

    /// Affine of geometric part `part`.
    #[must_use]
    pub(crate) fn placement(&self, part: usize) -> Option<&PlacementMap> {
        self.parts.get(part)
    }

    /// Pixel size `[w, h]` of geometric part `part`.
    #[must_use]
    pub(crate) fn part_size(&self, part: usize) -> Option<[u32; 2]> {
        self.part_sizes.get(part).copied()
    }

    /// New-order index of geometric part `part`.
    #[must_use]
    pub(crate) fn part_new_idx(&self, part: usize) -> Option<usize> {
        self.part_new_idx.get(part).copied()
    }

    /// Layer routing of the tree rooted at `tree_rel`.
    #[must_use]
    pub(crate) fn routing(&self, tree_rel: &str) -> Option<&SplitTreeRouting> {
        self.trees.get(tree_rel)
    }

    /// Geometric part containing the page-pixel point `(x, y)`.
    ///
    /// Containment follows the part CROP rectangles exactly: a point sitting
    /// on a cut line belongs to the part that owns that pixel row/column, i.e.
    /// the lower / right one. Points outside the page clamp to the first or
    /// last part.
    #[must_use]
    pub(crate) fn part_for_point(&self, x: f64, y: f64) -> usize {
        let coord = match self.axis {
            SplitAxis::Horizontal => y,
            SplitAxis::Vertical => x,
        };
        // `bounds` has part_count + 1 entries; the last one is the page extent.
        for part in 0..self.parts.len() {
            if coord < self.bounds[part + 1] {
                return part;
            }
        }
        self.parts.len().saturating_sub(1)
    }

    /// Geometric part holding the LARGEST share of a polygon's area, in page
    /// pixels, or `None` when the polygon encloses no area.
    ///
    /// This is the exact form of the "the part holding 51% of it" rule: the
    /// polygon is clipped against each part's slab and compared by shoelace
    /// area, so a rotated or deformed layer is judged by what it really covers
    /// rather than by its bounding box. An exact tie resolves to the SMALLER
    /// part index — the top part for horizontal cuts, the left part for
    /// vertical ones — which is geometric position, not user order.
    ///
    /// The caller must pass a SIMPLE (non-self-intersecting) ring here; a
    /// footprint that can fold — a deform mesh — must go through
    /// [`Self::part_for_polygon_group`] instead, one piece per grid cell.
    #[must_use]
    pub(crate) fn part_for_polygon(&self, points: &[[f64; 2]]) -> Option<usize> {
        self.part_for_polygon_group(&[points])
    }

    /// Geometric part holding the largest share of a footprint made of SEVERAL
    /// pieces, whose per-part clipped areas are summed in ABSOLUTE value.
    ///
    /// This is the fold-correct form of [`Self::part_for_polygon`]. A signed
    /// shoelace sum over one outer ring cancels the lobes of a self-intersecting
    /// (folded) footprint, so the clipped area is not the filled area and the
    /// layer can route to the wrong part. A deform mesh is a regular grid of
    /// quads and a user CAN fold it in the typing tab, so its area is measured
    /// cell by cell: each cell is clipped against the part slab and its absolute
    /// area added, which stays correct however the cells overlap.
    ///
    /// Pieces with fewer than three points contribute nothing. Ties resolve
    /// exactly as in [`Self::part_for_polygon`].
    #[must_use]
    pub(crate) fn part_for_polygon_group(&self, pieces: &[&[[f64; 2]]]) -> Option<usize> {
        let axis_index = match self.axis {
            SplitAxis::Horizontal => 1,
            SplitAxis::Vertical => 0,
        };
        let mut areas = vec![0.0_f64; self.parts.len()];
        for points in pieces {
            if points.len() < 3 {
                continue;
            }
            for (part, area) in areas.iter_mut().enumerate() {
                let lower = clip_half_plane(points, axis_index, self.bounds[part], true);
                let slab = clip_half_plane(&lower, axis_index, self.bounds[part + 1], false);
                *area += polygon_area(&slab);
            }
        }
        let mut best: Option<(usize, f64)> = None;
        for (part, area) in areas.into_iter().enumerate() {
            // Strictly greater keeps the FIRST (topmost/leftmost) part on a tie.
            if best.is_none_or(|(_, best_area)| area > best_area) {
                best = Some((part, area));
            }
        }
        best.filter(|(_, area)| *area > 0.0).map(|(part, _)| part)
    }

    /// Geometric part holding the largest share of an axis-aligned page-pixel
    /// rectangle `[x1, y1, x2, y2]`; falls back to the rectangle's centre point
    /// when the rectangle is degenerate.
    #[must_use]
    pub(crate) fn part_for_page_rect(&self, rect: [f64; 4]) -> usize {
        let [x1, y1, x2, y2] = rect;
        let (left, right) = (x1.min(x2), x1.max(x2));
        let (top, bottom) = (y1.min(y2), y1.max(y2));
        let quad = [
            [left, top],
            [right, top],
            [right, bottom],
            [left, bottom],
        ];
        self.part_for_polygon(&quad).unwrap_or_else(|| {
            self.part_for_point((left + right) * 0.5, (top + bottom) * 0.5)
        })
    }

    /// Drains the diagnostics collected while resolving the routing.
    fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// Attaches a per-tree layer routing to a test geometry.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_routing(mut self, tree_rel: &str, routing: SplitTreeRouting) -> Self {
        self.trees.insert(tree_rel.to_string(), routing);
        self
    }

    /// Builds a geometry directly, for tests of the JSON remaps (the normal
    /// path goes through `build_split_geometry`, which needs a full snapshot).
    #[cfg(test)]
    pub(crate) fn for_tests(
        source_old_idx: usize,
        axis: SplitAxis,
        page_size: [u32; 2],
        cuts: &[u32],
        order: &[usize],
    ) -> Self {
        resolve_split_parts(source_old_idx, axis, page_size, cuts, order)
            .expect("valid test split geometry")
    }
}

/// Resolves the pure part geometry of a split (no tree routing yet).
///
/// `order[k]` is the position of geometric part `k` in the new page order,
/// relative to `source_old_idx` (see [`super::PageOpKind::Split`]).
///
/// # Errors
/// [`PageOpError::InvalidOp`] for a zero-sized page, a cut list that is not
/// strictly increasing strictly inside the page, an `order` that is not a
/// permutation of the parts, or a part that fails [`PlacementMap::new`].
fn resolve_split_parts(
    source_old_idx: usize,
    axis: SplitAxis,
    page_size: [u32; 2],
    cuts: &[u32],
    order: &[usize],
) -> Result<SplitGeometry, PageOpError> {
    if page_size[0] == 0 || page_size[1] == 0 {
        return Err(PageOpError::InvalidOp(format!(
            "split page {source_old_idx} has a zero pixel size {}x{}",
            page_size[0], page_size[1]
        )));
    }
    let extent = match axis {
        SplitAxis::Horizontal => page_size[1],
        SplitAxis::Vertical => page_size[0],
    };
    let mut previous = 0u32;
    for cut in cuts {
        // Strictly increasing and strictly inside the page is exactly the
        // condition "every part is at least 1 px".
        if *cut <= previous || *cut >= extent {
            return Err(PageOpError::InvalidOp(format!(
                "split cuts {cuts:?} of page {source_old_idx} are not strictly increasing \
                 inside (0, {extent}) along the cut axis"
            )));
        }
        previous = *cut;
    }
    let part_count = cuts.len() + 1;
    if order.len() != part_count {
        return Err(PageOpError::InvalidOp(format!(
            "split order has {} entr(ies) for {part_count} part(s)",
            order.len()
        )));
    }
    let mut seen = vec![false; part_count];
    for position in order {
        let slot = seen.get_mut(*position).ok_or_else(|| {
            PageOpError::InvalidOp(format!(
                "split order position {position} is out of range for {part_count} part(s)"
            ))
        })?;
        if *slot {
            return Err(PageOpError::InvalidOp(format!(
                "split order {order:?} lists position {position} more than once"
            )));
        }
        *slot = true;
    }

    let mut bounds = Vec::with_capacity(part_count + 1);
    bounds.push(0u32);
    bounds.extend_from_slice(cuts);
    bounds.push(extent);

    let mut parts = Vec::with_capacity(part_count);
    let mut part_sizes = Vec::with_capacity(part_count);
    let mut part_new_idx = Vec::with_capacity(part_count);
    for part in 0..part_count {
        let start = bounds[part];
        let length = bounds[part + 1] - start;
        let (crop, canvas) = match axis {
            SplitAxis::Horizontal => (
                [0, start, page_size[0], length],
                [page_size[0], length],
            ),
            SplitAxis::Vertical => (
                [start, 0, length, page_size[1]],
                [length, page_size[1]],
            ),
        };
        let placement = StitchPlacement {
            page_idx: source_old_idx,
            crop,
            scale: 1.0,
            dx: 0,
            dy: 0,
        };
        parts.push(PlacementMap::new(&placement, page_size, canvas)?);
        part_sizes.push(canvas);
        // D1: the part the user ordered FIRST keeps the source page's index,
        // so the parts occupy the contiguous run `source_old_idx ..`.
        part_new_idx.push(source_old_idx + order[part]);
    }

    Ok(SplitGeometry {
        source_old_idx,
        page_size,
        axis,
        bounds: bounds.into_iter().map(f64::from).collect(),
        parts,
        part_sizes,
        part_new_idx,
        trees: std::collections::BTreeMap::new(),
        warnings: Vec::new(),
    })
}

/// Shoelace area of a simple polygon, in the units of its coordinates. An
/// empty or degenerate ring yields 0.
#[must_use]
fn polygon_area(points: &[[f64; 2]]) -> f64 {
    if points.len() < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for (index, point) in points.iter().enumerate() {
        let next = points[(index + 1) % points.len()];
        sum += point[0] * next[1] - next[0] * point[1];
    }
    (sum * 0.5).abs()
}

/// Sutherland-Hodgman clip of a polygon against ONE axis-aligned half-plane.
///
/// `axis_index` selects the coordinate (0 = x, 1 = y); `keep_greater` keeps the
/// side `coord >= limit`, otherwise `coord <= limit`. The clip region is convex
/// (a half-plane), so the signed area of the result is exact even for a concave
/// subject polygon such as a deform mesh's boundary ring — the degenerate edges
/// the algorithm may introduce there contribute zero area.
#[must_use]
fn clip_half_plane(
    points: &[[f64; 2]],
    axis_index: usize,
    limit: f64,
    keep_greater: bool,
) -> Vec<[f64; 2]> {
    if points.is_empty() {
        return Vec::new();
    }
    let inside = |point: &[f64; 2]| {
        if keep_greater {
            point[axis_index] >= limit
        } else {
            point[axis_index] <= limit
        }
    };
    let intersect = |a: &[f64; 2], b: &[f64; 2]| {
        let delta = b[axis_index] - a[axis_index];
        // Both endpoints on the boundary line: no unique crossing, take `b`.
        if delta.abs() <= f64::EPSILON {
            return *b;
        }
        let t = (limit - a[axis_index]) / delta;
        [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
    };
    let mut out = Vec::with_capacity(points.len() + 4);
    for (index, current) in points.iter().enumerate() {
        let previous = &points[(index + points.len() - 1) % points.len()];
        let (current_in, previous_in) = (inside(current), inside(previous));
        if current_in {
            if !previous_in {
                out.push(intersect(previous, current));
            }
            out.push(*current);
        } else if previous_in {
            out.push(intersect(previous, current));
        }
    }
    out
}

/// Geometry attached to an operation that changes a page's PIXEL identity.
///
/// Ordinary operations (move / insert / delete) only re-key files: every page
/// keeps its own coordinate space, which is [`PageGeometry::None`]. A stitch
/// maps N pages onto one canvas, a split maps one page onto N canvases; in
/// both cases the affected documents must additionally be routed and mapped,
/// and the two cases are mutually exclusive.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PageGeometry<'a> {
    /// No pixel remapping: pages keep their own coordinate spaces.
    None,
    /// Several pages merged onto one canvas.
    Stitch(&'a StitchGeometry),
    /// One page cut into several parts.
    Split(&'a SplitGeometry),
}

impl<'a> PageGeometry<'a> {
    /// The stitch geometry, or `None` for any other operation.
    #[must_use]
    pub(crate) fn stitch(self) -> Option<&'a StitchGeometry> {
        match self {
            Self::Stitch(geo) => Some(geo),
            Self::None | Self::Split(_) => None,
        }
    }

    /// The split geometry, or `None` for any other operation.
    #[must_use]
    pub(crate) fn split(self) -> Option<&'a SplitGeometry> {
        match self {
            Self::Split(geo) => Some(geo),
            Self::None | Self::Stitch(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Permutation math.
// ---------------------------------------------------------------------------

/// One source image of a composed page: which file, which part of it, and
/// where it lands. Paths are title-relative and point at the file's ORIGINAL
/// location, because composing happens in phase A, before any rename.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ComposeSource {
    /// Title-relative path of the image to read.
    pub path: String,
    /// Pixel size the crop/offset are expressed in (the source PAGE's size). A
    /// decoded image of a different size is resized to it first — page-sized
    /// overlays may have been attached with a same-aspect resize.
    pub page_size: [u32; 2],
    /// `[x, y, w, h]` region of the source image, in `page_size` pixels.
    pub crop: [u32; 4],
    /// `[x, y, w, h]` destination rectangle in the composed canvas.
    pub dest: [u32; 4],
}

/// Content of a page created by the operation (journal-serializable so
/// recovery can re-stage it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) enum NewPageContent {
    /// Copy this absolute source file into `src/`.
    CopyFile { source: PathBuf },
    /// Encode a solid-fill PNG (straight, non-premultiplied RGBA).
    BlankPng {
        width: u32,
        height: u32,
        rgba: [u8; 4],
    },
    /// Compose several cropped/scaled chapter images onto one background,
    /// painted in list order, and encode the result as a straight-RGBA PNG.
    /// Every listed source must exist at execution time: the executor never
    /// invents pixels (a missing source fails the transaction closed, exactly
    /// like a missing `CopyFile` source).
    ComposedPng {
        width: u32,
        height: u32,
        /// Straight (non-premultiplied) RGBA fill of the uncovered canvas.
        background: [u8; 4],
        sources: Vec<ComposeSource>,
    },
}

/// One page the operation adds, keyed by its index in the NEW order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NewPage {
    pub new_idx: usize,
    /// Lower-cased extension of the created file (`png`/`jpg`/`jpeg`).
    pub extension: String,
    pub content: NewPageContent,
}

/// Full permutation produced by an operation over `old_page_count` pages.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Permutation {
    /// `old_to_new[i]` is the new index of old page `i`; `None` = deleted.
    pub old_to_new: Vec<Option<usize>>,
    pub new_page_count: usize,
    /// Pages created by the operation, ordered by `new_idx`.
    pub new_pages: Vec<NewPage>,
}

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
        } => split_permutation(*page_idx, cuts, order, old_page_count),
    }
}

/// Index math + cut/order validation of a split.
///
/// The `cuts.len() + 1` parts occupy the contiguous run
/// `page_idx ..= page_idx + cuts.len()` of the new order, geometric part `k`
/// landing at `page_idx + order[k]`; every page after the split page shifts up
/// by `cuts.len()`. `old_to_new[page_idx]` is `page_idx` itself — the one
/// representative the permutation type can carry, which by construction is the
/// part the user ordered FIRST. The created pages are NOT listed in
/// `new_pages`: their content is cut out of the chapter snapshot by
/// `plan_src_pages`, not derivable from the request alone.
///
/// The cut positions are checked only for internal consistency here (strictly
/// increasing, non-empty); whether they fall inside the page needs the page's
/// pixel size and is checked by [`build_split_geometry`].
///
/// # Errors
/// [`PageOpError::InvalidOp`] for an out-of-range `page_idx`, an empty cut
/// list, cuts that are not strictly increasing, more than [`SPLIT_MAX_PARTS`]
/// parts, or an `order` that is not a permutation of `0..cuts.len() + 1`.
fn split_permutation(
    page_idx: usize,
    cuts: &[u32],
    order: &[usize],
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
    if order.len() != part_count {
        return Err(PageOpError::InvalidOp(format!(
            "split order has {} entr(ies) for {part_count} part(s)",
            order.len()
        )));
    }
    let mut seen = vec![false; part_count];
    for position in order {
        let slot = seen.get_mut(*position).ok_or_else(|| {
            PageOpError::InvalidOp(format!(
                "split order position {position} is out of range for {part_count} part(s)"
            ))
        })?;
        if *slot {
            return Err(PageOpError::InvalidOp(format!(
                "split order {order:?} lists position {position} more than once"
            )));
        }
        *slot = true;
    }

    let added = part_count - 1;
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

// ---------------------------------------------------------------------------
// Chapter snapshot (plan input).
// ---------------------------------------------------------------------------

/// Which directory a `text_info.json` file was found in. Modern chapters keep
/// it in `layers/`; legacy chapters in `text_images/` (see
/// `tabs/typing/tab/render_jobs.rs` read order). Both are remapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextInfoLocation {
    LayersDir,
    TextImagesDir,
}

/// One parsed `text_info.json` (top-level array entries).
#[derive(Debug, Clone)]
pub(crate) struct TextInfoFile {
    pub location: TextInfoLocation,
    pub entries: Vec<Value>,
}

/// Page-keyed artifacts of ONE tree (committed chapter dir or `_unsaved`).
/// Only files that actually exist are listed; the plan never renames a file
/// that is not in the snapshot.
#[derive(Debug, Clone, Default)]
pub(crate) struct TreeSnapshot {
    /// Tree root relative to the title dir (`{chapter}` or `{chapter}_unsaved`).
    pub tree_rel: String,
    /// Stems of `clean_layers/*.png` files.
    pub clean_overlay_stems: BTreeSet<String>,
    /// Every file name in `layers/`.
    pub layers_files: BTreeSet<String>,
    /// Pixel size of the layer PNGs of the page a SPLIT cuts, by file name.
    /// Filled only for `PageOpKind::Split` (an image-header probe per file of
    /// that one page), because a TEXT layer record stores no `image_size` and
    /// the split's area rule cannot be evaluated without it. Empty otherwise.
    pub layer_png_sizes: std::collections::BTreeMap<String, [u32; 2]>,
    /// Parsed `layers/layers.json`, when present.
    pub layers_manifest: Option<Value>,
    /// Every file name in `text_images/`.
    pub text_images_files: BTreeSet<String>,
    /// Parsed `text_info.json` files found in this tree.
    pub text_info: Vec<TextInfoFile>,
    /// Parsed `translation_bubbles.json` entries, when the file exists.
    pub bubbles: Option<Vec<Value>>,
}

/// Parsed state of a text-detection blocks file.
#[derive(Debug, Clone)]
pub(crate) enum DetectionBlocks {
    /// Valid JSON: content is rewritten (`mask_file`) during remap.
    Parsed(Value),
    /// Unparseable JSON: renamed as an opaque file (its optional `mask_file`
    /// reference resolves gracefully to the per-page default on load).
    Opaque,
}

/// Text-detection artifacts for one page index (committed tree only — the
/// `text_detection/` dir has no unsaved mirror, see `ProjectPaths`).
#[derive(Debug, Clone)]
pub(crate) struct DetectionFiles {
    pub page_idx: usize,
    pub blocks: Option<DetectionBlocks>,
    pub has_mask: bool,
}

/// Everything the planner needs to know about the chapter on disk.
#[derive(Debug, Clone)]
pub(crate) struct ChapterSnapshot {
    /// Chapter dir relative to the title dir (its file name).
    pub chapter_rel: String,
    /// File name of each current page in `src/`, index-aligned with the
    /// current page order.
    pub page_file_names: Vec<String>,
    /// Pixel size of each current page, index-aligned with `page_file_names`.
    /// Filled only for operations that need page geometry (`Stitch`), because
    /// probing it costs one image-header read per page; empty otherwise.
    pub page_sizes: Vec<[u32; 2]>,
    /// True when `alt_vers/` holds anything. The engine never remaps it (its
    /// files pair with pages by SORTED POSITION, not by name), so an operation
    /// that changes the page count warns about the resulting misalignment.
    pub has_alt_vers: bool,
    pub committed: TreeSnapshot,
    pub unsaved: TreeSnapshot,
    pub detection: Vec<DetectionFiles>,
}

// ---------------------------------------------------------------------------
// Action plan (journal payload).
// ---------------------------------------------------------------------------

/// Where a phase-A temp ends up in phase B.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum MoveDest {
    /// Renamed to this final path (a page-keyed file surviving under a new key).
    Final { path: String },
    /// Moved into the transaction trash (deleted page artifacts).
    Trash { path: String },
    /// Deleted at commit (original of a JSON document that phase B rewrites;
    /// its content survives remapped in the corresponding `PlannedJsonWrite`).
    Discard,
}

/// One two-phase file move: phase A renames `from` -> `temp` (reversible),
/// phase B resolves `temp` per `dest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlannedMove {
    pub from: String,
    pub temp: String,
    pub dest: MoveDest,
}

/// One file the operation creates: staged at `temp` during phase A, renamed to
/// `target` in phase B.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlannedCreate {
    pub temp: String,
    pub target: String,
    pub content: NewPageContent,
}

/// One JSON document rewritten by the transaction. `content` is the complete
/// new file body, computed at plan time and journaled so recovery can re-apply
/// it without re-reading (possibly already-moved) inputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlannedJsonWrite {
    pub target: String,
    pub content: String,
}

/// An extra file written into the trash (copies of deleted JSON entries).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlannedTrashWrite {
    pub target: String,
    pub content: String,
}

/// Complete journaled plan of one page operation. All paths are relative to
/// the title dir with '/' separators.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PageOpPlan {
    pub old_to_new: Vec<Option<usize>>,
    pub new_page_count: usize,
    /// Trash root for this transaction, `{chapter}/.pageop_trash/{id}`.
    pub trash_root: String,
    pub moves: Vec<PlannedMove>,
    pub creates: Vec<PlannedCreate>,
    pub json_writes: Vec<PlannedJsonWrite>,
    pub trash_writes: Vec<PlannedTrashWrite>,
    /// Plan-time diagnostics (stale indices, opaque files). Logged by the
    /// executor; not part of the journal.
    #[serde(skip)]
    pub warnings: Vec<String>,
}

impl PageOpPlan {
    /// True when the operation changes nothing on disk (identity permutation
    /// over already-canonical names with no content changes).
    #[must_use]
    pub(crate) fn is_noop(&self) -> bool {
        self.moves.is_empty()
            && self.creates.is_empty()
            && self.json_writes.is_empty()
            && self.trash_writes.is_empty()
    }
}

/// Splits `file.ext` into (`file`, `Some("ext")`); no dot yields (`name`, `None`).
fn split_name(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    }
}

/// Internal accumulator that owns temp-name numbering and `from`-path dedup.
struct PlanBuilder {
    trash_root: String,
    temp_id: u128,
    temp_counter: usize,
    planned_from: HashSet<String>,
    moves: Vec<PlannedMove>,
    creates: Vec<PlannedCreate>,
    json_writes: Vec<PlannedJsonWrite>,
    trash_writes: Vec<PlannedTrashWrite>,
    warnings: Vec<String>,
}

impl PlanBuilder {
    fn new(trash_root: String, temp_id: u128) -> Self {
        Self {
            trash_root,
            temp_id,
            temp_counter: 0,
            planned_from: HashSet::new(),
            moves: Vec::new(),
            creates: Vec::new(),
            json_writes: Vec::new(),
            trash_writes: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Unique temp path in the same directory as `sibling_rel` (same-volume
    /// rename) that no chapter loader recognizes as an image/JSON artifact.
    fn next_temp(&mut self, sibling_rel: &str) -> String {
        let n = self.temp_counter;
        self.temp_counter += 1;
        let dir = match sibling_rel.rsplit_once('/') {
            Some((dir, _)) => dir,
            None => "",
        };
        let name = format!("{TEMP_PREFIX}{}_{n}{TEMP_SUFFIX}", self.temp_id);
        if dir.is_empty() {
            name
        } else {
            format!("{dir}/{name}")
        }
    }

    /// Plans `from` -> `to`, skipping identity renames and duplicate sources.
    fn rename(&mut self, from: String, to: String) {
        if from == to || !self.planned_from.insert(from.clone()) {
            return;
        }
        let temp = self.next_temp(&from);
        self.moves.push(PlannedMove {
            from,
            temp,
            dest: MoveDest::Final { path: to },
        });
    }

    /// Plans moving `from` into the trash, preserving its title-relative path.
    fn trash(&mut self, from: String) {
        if !self.planned_from.insert(from.clone()) {
            return;
        }
        let temp = self.next_temp(&from);
        let path = format!("{}/{from}", self.trash_root);
        self.moves.push(PlannedMove {
            from,
            temp,
            dest: MoveDest::Trash { path },
        });
    }

    /// Plans discarding the file at `from` at commit time (phase A still moves
    /// it to a temp first, so the step stays reversible until commit).
    fn discard(&mut self, from: String) {
        if !self.planned_from.insert(from.clone()) {
            return;
        }
        let temp = self.next_temp(&from);
        self.moves.push(PlannedMove {
            from,
            temp,
            dest: MoveDest::Discard,
        });
    }

    /// Plans rewriting the JSON document at `target` with `content`; when the
    /// file currently exists (`had_original`) its original is discarded at
    /// commit (the remapped content supersedes it).
    fn rewrite_json(&mut self, target: String, content: String, had_original: bool) {
        if had_original {
            self.discard(target.clone());
        }
        self.json_writes.push(PlannedJsonWrite { target, content });
    }

    /// Plans creating a new file at `target` from `content` (staged in phase A
    /// beside its destination, committed in phase B).
    fn create(&mut self, target: String, content: NewPageContent) {
        let temp = self.next_temp(&target);
        self.creates.push(PlannedCreate {
            temp,
            target,
            content,
        });
    }

    fn write_trash_extra(&mut self, rel_inside_trash: String, content: String) {
        let target = format!("{}/{rel_inside_trash}", self.trash_root);
        self.trash_writes.push(PlannedTrashWrite { target, content });
    }

    fn warn(&mut self, message: String) {
        self.warnings.push(message);
    }
}

/// Serializes a JSON value the way the app writes its project documents
/// (pretty, matching `ProjectData::autosave_bubbles` / layer-manifest writes).
fn to_pretty(value: &Value) -> Result<String, PageOpError> {
    serde_json::to_string_pretty(value)
        .map_err(|err| PageOpError::Json(format!("serialize remapped document: {err}")))
}

/// Builds the full action plan for `op` from the chapter snapshot.
///
/// `trash_id` names both the trash subfolder and the temp-file namespace of
/// this transaction (the executor derives it from `SystemTime`).
///
/// # Errors
/// - [`PageOpError::InvalidOp`] for requests that do not apply to the snapshot
///   (bad indices, unsupported extensions, un-remappable legacy documents).
/// - [`PageOpError::Json`] when a page-keyed document cannot be re-serialized.
pub(crate) fn build_plan(
    snapshot: &ChapterSnapshot,
    op: &PageOpKind,
    trash_id: u128,
) -> Result<PageOpPlan, PageOpError> {
    let old_page_count = snapshot.page_file_names.len();
    let permutation = permutation_for_op(op, old_page_count)?;
    let map = &permutation.old_to_new;

    let trash_root = format!("{}/{TRASH_DIR_NAME}/{trash_id}", snapshot.chapter_rel);
    let mut b = PlanBuilder::new(trash_root.clone(), trash_id);

    // Stitch and split are the two operations that change a page's PIXEL
    // identity instead of merely re-keying files: they resolve their request
    // into affines first, and every planner below then treats an affected page
    // as "merged into" / "cut into" rather than "renamed to a new key".
    let stitch;
    let split;
    let geometry = match op {
        PageOpKind::Stitch {
            placements,
            width,
            height,
            background,
        } => {
            stitch = build_stitch_geometry(
                snapshot,
                placements,
                [*width, *height],
                *background,
                map,
            )?;
            PageGeometry::Stitch(&stitch)
        }
        PageOpKind::Split {
            page_idx,
            axis,
            cuts,
            order,
        } => {
            let mut resolved = build_split_geometry(snapshot, *page_idx, *axis, cuts, order)?;
            for warning in resolved.take_warnings() {
                b.warn(warning);
            }
            split = resolved;
            PageGeometry::Split(&split)
        }
        PageOpKind::Move { .. }
        | PageOpKind::InsertFiles { .. }
        | PageOpKind::CreateBlank { .. }
        | PageOpKind::Delete { .. } => PageGeometry::None,
    };

    if snapshot.has_alt_vers {
        // `alt_vers/` pairs with pages by sorted position and has no per-file
        // page key (see this module's MODULE_README), so any change to the page
        // COUNT shifts that pairing and there is nothing to rename.
        let shift = match geometry {
            PageGeometry::Stitch(geo) => Some(format!(
                "merging {} page(s) into one",
                geo.source_count()
            )),
            PageGeometry::Split(geo) => Some(format!(
                "cutting one page into {} part(s)",
                geo.part_count()
            )),
            PageGeometry::None => None,
        };
        if let Some(shift) = shift {
            b.warn(format!(
                "{}/{}: alternate versions are position-matched and are NOT remapped; \
                 {shift} shifts their alignment with the pages",
                snapshot.chapter_rel,
                config::ALT_VERS_DIR,
            ));
        }
    }

    plan_src_pages(&mut b, snapshot, map, geometry)?;
    for tree in [&snapshot.committed, &snapshot.unsaved] {
        plan_clean_overlays(&mut b, snapshot, tree, map, geometry);
        plan_layer_pngs(&mut b, tree, map, geometry)?;
        plan_layers_manifest(&mut b, tree, map, geometry)?;
        plan_text_info(&mut b, tree, map, geometry)?;
        plan_typing_masks(&mut b, snapshot, tree, map, geometry);
        plan_bubbles(&mut b, tree, map, geometry)?;
    }
    plan_detection(&mut b, snapshot, map, geometry)?;
    plan_creates(&mut b, snapshot, &permutation);

    Ok(PageOpPlan {
        old_to_new: permutation.old_to_new,
        new_page_count: permutation.new_page_count,
        trash_root,
        moves: b.moves,
        creates: b.creates,
        json_writes: b.json_writes,
        trash_writes: b.trash_writes,
        warnings: b.warnings,
    })
}

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
fn build_stitch_geometry(
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
/// cuts are not strictly increasing strictly inside the page, the `order` is
/// not a permutation of the parts, a part's placement is invalid, or one layer
/// PNG of the cut page is claimed by records routed to different parts (see
/// [`json_remap::split_layer_routing`]).
fn build_split_geometry(
    snapshot: &ChapterSnapshot,
    page_idx: usize,
    axis: SplitAxis,
    cuts: &[u32],
    order: &[usize],
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
    let mut geometry = resolve_split_parts(page_idx, axis, page_size, cuts, order)?;

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

/// Stages one cropped PNG per split part from ONE page-sized raster of the cut
/// page.
///
/// `from` is the raster's title-relative path at its ORIGINAL location (phase A
/// composes before any rename), `page_size` the pixel size its crop is
/// expressed in, and `target_for` maps a part's NEW page index to the file the
/// part's raster must end up at. Every part's crop equals its destination, so
/// `encode_composed_png` copies the pixels bit-exactly instead of resampling,
/// and `background` is only ever visible if the source raster decodes smaller
/// than its page (it is then resized to the page size first).
fn plan_split_raster_parts(
    b: &mut PlanBuilder,
    geo: &SplitGeometry,
    from: &str,
    page_size: [u32; 2],
    background: [u8; 4],
    target_for: impl Fn(usize) -> String,
) {
    for part in 0..geo.part_count() {
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
                    crop: placement.crop_rect(),
                    dest: placement.placed_rect(),
                }],
            },
        );
    }
}

/// Source page files: rename surviving pages onto the canonical stem of their
/// NEW index (extension preserved), move deleted pages to the trash.
///
/// Under a stitch every source page is trashed instead (its pixels survive in
/// the composed page) and the merged page is staged as a new PNG, regardless of
/// the source pages' extensions. Under a split the cut page is trashed and each
/// part is staged as its own new PNG, likewise regardless of the source
/// extension.
fn plan_src_pages(
    b: &mut PlanBuilder,
    snapshot: &ChapterSnapshot,
    map: &[Option<usize>],
    geometry: PageGeometry<'_>,
) -> Result<(), PageOpError> {
    let stitch = geometry.stitch();
    let mut composed: Vec<ComposeSource> = Vec::new();
    for (old_idx, name) in snapshot.page_file_names.iter().enumerate() {
        let from = format!("{}/{}/{name}", snapshot.chapter_rel, config::SRC_DIR);
        if let Some(geo) = geometry.split()
            && geo.source_old_idx() == old_idx
        {
            let page_size = *snapshot.page_sizes.get(old_idx).ok_or_else(|| {
                PageOpError::InvalidOp(format!("page {old_idx} has no known pixel size"))
            })?;
            plan_split_raster_parts(b, geo, &from, page_size, [0, 0, 0, 0], |new_idx| {
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

/// Clean overlays are keyed by the PAGE'S CURRENT STEM (`{stem}.png`), in both
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
fn plan_clean_overlays(
    b: &mut PlanBuilder,
    snapshot: &ChapterSnapshot,
    tree: &TreeSnapshot,
    map: &[Option<usize>],
    geometry: PageGeometry<'_>,
) {
    let stitch = geometry.stitch();
    let mut composed: Vec<ComposeSource> = Vec::new();
    for (old_idx, name) in snapshot.page_file_names.iter().enumerate() {
        let (stem, _) = split_name(name);
        let exists = tree.clean_overlay_stems.contains(stem);
        let from = format!("{}/{}/{stem}.png", tree.tree_rel, config::CLEAN_LAYERS_DIR);
        if let Some(geo) = geometry.split()
            && geo.source_old_idx() == old_idx
        {
            if exists && let Some(page_size) = snapshot.page_sizes.get(old_idx).copied() {
                plan_split_raster_parts(b, geo, &from, page_size, [0, 0, 0, 0], |new_idx| {
                    format!(
                        "{}/{}/{}.png",
                        tree.tree_rel,
                        config::CLEAN_LAYERS_DIR,
                        canonical_page_stem(new_idx)
                    )
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
                let target = format!(
                    "{}/{}/{}.png",
                    tree.tree_rel,
                    config::CLEAN_LAYERS_DIR,
                    canonical_page_stem(new_idx)
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
                "{}/{}/{}.png",
                tree.tree_rel,
                config::CLEAN_LAYERS_DIR,
                canonical_page_stem(geo.primary_new)
            ),
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
/// from the split's per-tree [`SplitTreeRouting`], which resolved each layer
/// NODE to a part; a PNG that no record of the manifest claims has no part and
/// falls back to the representative part with a warning (it is an orphan that
/// `prune_orphan_pngs` will collect anyway).
///
/// # Errors
/// [`PageOpError::InvalidOp`] when two merged pages would produce the same
/// layer PNG name.
fn plan_layer_pngs(
    b: &mut PlanBuilder,
    tree: &TreeSnapshot,
    map: &[Option<usize>],
    geometry: PageGeometry<'_>,
) -> Result<(), PageOpError> {
    let stitch = geometry.stitch();
    let split = geometry.split();
    let routing = split.and_then(|geo| geo.routing(&tree.tree_rel));
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
        // A split routes the cut page's PNGs by NODE, not by the page index in
        // the name: the parts land on different prefixes.
        if let Some(geo) = split
            && geo.source_old_idx() == old_idx
        {
            let new_idx = routing.and_then(|r| r.file_new_idx(name)).unwrap_or_else(|| {
                b.warn(format!(
                    "layer PNG '{from}' of the split page is not referenced by any layer \
                     record; it follows the part that keeps the page's index"
                ));
                geo.source_old_idx()
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
/// references; page entries of deleted pages are removed and archived in the
/// trash as `deleted_layers_pages.json`. A stitch folds the merged pages'
/// entries into one; a split partitions the cut page's entry into one entry per
/// part that holds at least one layer.
fn plan_layers_manifest(
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
        let target = format!("{}/{}/layers.json", tree.tree_rel, config::LAYERS_DIR);
        b.rewrite_json(target, to_pretty(&remap.manifest)?, true);
    }
    Ok(())
}

/// `text_info.json` (legacy typing metadata, possibly present in both the
/// `layers/` and `text_images/` dirs of each tree): remap `img_idx`, drop
/// entries of deleted pages (archived as `deleted_text_info.json`), and move
/// the overlay PNGs referenced only by dropped entries to the trash.
fn plan_text_info(
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
fn plan_typing_masks(
    b: &mut PlanBuilder,
    snapshot: &ChapterSnapshot,
    tree: &TreeSnapshot,
    map: &[Option<usize>],
    geometry: PageGeometry<'_>,
) {
    let stitch = geometry.stitch();
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
        if let Some(geo) = geometry.split()
            && geo.source_old_idx() == old_idx
        {
            if let Some(page_size) = snapshot.page_sizes.get(old_idx).copied() {
                // Opaque black = "no mask here" (the loader thresholds luma at
                // 128), matching how the typing tab writes its masks.
                plan_split_raster_parts(b, geo, &from, page_size, [0, 0, 0, 255], |new_idx| {
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
fn plan_bubbles(
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
        let target = format!("{}/{}", tree.tree_rel, config::BUBBLES_FILE);
        b.rewrite_json(target, to_pretty(&Value::Array(remap.kept))?, true);
    }
    Ok(())
}

/// `text_detection/` (committed tree only): `{idx:05}_blocks.json` +
/// `{idx:05}_mask.png`, with the `mask_file` reference inside a parsed blocks
/// file rewritten to the new default mask name.
fn plan_detection(
    b: &mut PlanBuilder,
    snapshot: &ChapterSnapshot,
    map: &[Option<usize>],
    geometry: PageGeometry<'_>,
) -> Result<(), PageOpError> {
    let stitch = geometry.stitch();
    if let Some(geo) = stitch {
        plan_stitch_detection(b, snapshot, geo)?;
    }
    if let Some(geo) = geometry.split() {
        plan_split_detection(b, snapshot, geo)?;
    }
    for det in &snapshot.detection {
        let old_idx = det.page_idx;
        // Stitched pages were handled as one merged group above.
        if stitch.is_some_and(|geo| geo.placement(old_idx).is_some()) {
            continue;
        }
        // The split page was cut into one document per part above.
        if geometry
            .split()
            .is_some_and(|geo| geo.source_old_idx() == old_idx)
        {
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
                            b.json_writes.push(PlannedJsonWrite {
                                target: blocks_target,
                                content,
                            });
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
    b.json_writes.push(PlannedJsonWrite {
        target: format!("{dir}/{}", detection_blocks_file_name(geo.primary_new)),
        content: to_pretty(&merged)?,
    });
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
            .or_else(|| json_remap::detection_split_blocker(value, old_idx));
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
        plan_split_raster_parts(b, geo, &mask_from, page_size, [0, 0, 0, 255], |new_idx| {
            format!("{dir}/{}", detection_mask_file_name(new_idx))
        });
    }
    for part in 0..geo.part_count() {
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
        b.json_writes.push(PlannedJsonWrite {
            target: format!("{dir}/{}", detection_blocks_file_name(new_idx)),
            content: to_pretty(&cut)?,
        });
    }
    Ok(())
}

/// New pages (insert / blank) staged into `src/` under their canonical stems.
fn plan_creates(b: &mut PlanBuilder, snapshot: &ChapterSnapshot, permutation: &Permutation) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn map_of(perm: &Permutation) -> Vec<Option<usize>> {
        perm.old_to_new.clone()
    }

    #[test]
    fn move_forward_and_backward_permutations() {
        // Move page 0 to the end of a 4-page chapter.
        let perm = permutation_for_op(&PageOpKind::Move { from: 0, to: 3 }, 4).expect("valid");
        assert_eq!(map_of(&perm), vec![Some(3), Some(0), Some(1), Some(2)]);
        assert_eq!(perm.new_page_count, 4);

        // Move page 3 to the front.
        let perm = permutation_for_op(&PageOpKind::Move { from: 3, to: 0 }, 4).expect("valid");
        assert_eq!(map_of(&perm), vec![Some(1), Some(2), Some(3), Some(0)]);

        // Move to the same position is the identity.
        let perm = permutation_for_op(&PageOpKind::Move { from: 2, to: 2 }, 4).expect("valid");
        assert_eq!(map_of(&perm), vec![Some(0), Some(1), Some(2), Some(3)]);

        // Adjacent swap forward: `to` is an index into the order WITHOUT `from`.
        let perm = permutation_for_op(&PageOpKind::Move { from: 1, to: 2 }, 4).expect("valid");
        assert_eq!(map_of(&perm), vec![Some(0), Some(2), Some(1), Some(3)]);
    }

    #[test]
    fn move_rejects_out_of_range() {
        assert!(matches!(
            permutation_for_op(&PageOpKind::Move { from: 4, to: 0 }, 4),
            Err(PageOpError::InvalidOp(_))
        ));
        assert!(matches!(
            permutation_for_op(&PageOpKind::Move { from: 0, to: 4 }, 4),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn insert_at_start_and_end() {
        let files = vec![PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b.JPG")];
        // At the start: every old page shifts by 2.
        let perm = permutation_for_op(
            &PageOpKind::InsertFiles {
                at: 0,
                files: files.clone(),
            },
            3,
        )
        .expect("valid");
        assert_eq!(map_of(&perm), vec![Some(2), Some(3), Some(4)]);
        assert_eq!(perm.new_page_count, 5);
        assert_eq!(perm.new_pages[0].new_idx, 0);
        assert_eq!(perm.new_pages[1].new_idx, 1);
        // Extensions are lower-cased.
        assert_eq!(perm.new_pages[1].extension, "jpg");

        // At the end: old pages keep their indices.
        let perm = permutation_for_op(&PageOpKind::InsertFiles { at: 3, files }, 3).expect("valid");
        assert_eq!(map_of(&perm), vec![Some(0), Some(1), Some(2)]);
        assert_eq!(perm.new_pages[0].new_idx, 3);
        assert_eq!(perm.new_pages[1].new_idx, 4);
    }

    #[test]
    fn insert_rejects_empty_list_bad_position_and_bad_extension() {
        assert!(matches!(
            permutation_for_op(&PageOpKind::InsertFiles { at: 0, files: vec![] }, 3),
            Err(PageOpError::InvalidOp(_))
        ));
        assert!(matches!(
            permutation_for_op(
                &PageOpKind::InsertFiles {
                    at: 4,
                    files: vec![PathBuf::from("/tmp/a.png")]
                },
                3
            ),
            Err(PageOpError::InvalidOp(_))
        ));
        assert!(matches!(
            permutation_for_op(
                &PageOpKind::InsertFiles {
                    at: 0,
                    files: vec![PathBuf::from("/tmp/a.webp")]
                },
                3
            ),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn create_blank_validates_dimensions() {
        let ok = permutation_for_op(
            &PageOpKind::CreateBlank {
                at: 1,
                width: 800,
                height: 1200,
                rgba: [255, 255, 255, 255],
            },
            2,
        )
        .expect("valid");
        assert_eq!(map_of(&ok), vec![Some(0), Some(2)]);
        assert_eq!(ok.new_pages[0].extension, "png");

        for (w, h) in [(0, 100), (100, 0), (20_001, 100), (100, 20_001)] {
            assert!(matches!(
                permutation_for_op(
                    &PageOpKind::CreateBlank {
                        at: 0,
                        width: w,
                        height: h,
                        rgba: [0, 0, 0, 255],
                    },
                    2
                ),
                Err(PageOpError::InvalidOp(_))
            ));
        }
    }

    #[test]
    fn delete_multiple_pages() {
        let perm = permutation_for_op(
            &PageOpKind::Delete {
                // Unsorted with a duplicate: the engine sorts and dedups.
                indices: vec![3, 1, 1],
            },
            5,
        )
        .expect("valid");
        assert_eq!(
            map_of(&perm),
            vec![Some(0), None, Some(1), None, Some(2)]
        );
        assert_eq!(perm.new_page_count, 3);
    }

    #[test]
    fn delete_rejects_empty_all_and_out_of_range() {
        assert!(matches!(
            permutation_for_op(&PageOpKind::Delete { indices: vec![] }, 3),
            Err(PageOpError::InvalidOp(_))
        ));
        assert!(matches!(
            permutation_for_op(
                &PageOpKind::Delete {
                    indices: vec![0, 1, 2]
                },
                3
            ),
            Err(PageOpError::InvalidOp(_))
        ));
        assert!(matches!(
            permutation_for_op(&PageOpKind::Delete { indices: vec![3] }, 3),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn page_keyed_name_parsers_roundtrip() {
        assert_eq!(parse_layers_png_page_idx("ps_p0007_ab12.png"), Some(7));
        assert_eq!(
            parse_layers_png_page_idx("ps_p0007_ab12_text.png"),
            Some(7)
        );
        assert_eq!(parse_layers_png_page_idx("ps_p12_x.png"), None);
        assert_eq!(parse_layers_png_page_idx("other.png"), None);

        assert_eq!(parse_typing_mask_page_idx("mask_page_12.png"), Some(12));
        assert_eq!(parse_typing_mask_page_idx("mask_page_.png"), None);
        assert_eq!(parse_typing_mask_page_idx("mask_page_1.jpg"), None);

        assert_eq!(parse_detection_blocks_page_idx("00012_blocks.json"), Some(12));
        assert_eq!(parse_detection_mask_page_idx("00012_mask.png"), Some(12));
        assert_eq!(parse_detection_blocks_page_idx("012_blocks.json"), None);
    }

    fn snapshot_for_plan() -> ChapterSnapshot {
        let committed = TreeSnapshot {
            tree_rel: "ch1".to_string(),
            clean_overlay_stems: ["000", "002"].iter().map(ToString::to_string).collect(),
            layers_files: ["layers.json", "ps_p0000_u1.png", "ps_p0002_u2_text.png"]
                .iter()
                .map(ToString::to_string)
                .collect(),
            layer_png_sizes: std::collections::BTreeMap::new(),
            layers_manifest: Some(serde_json::json!({
                "schema_version": 3,
                "pages": [
                    {"img_idx": 0, "tree": [
                        {"uid": "u1", "name": "L", "z": 0, "visible": true,
                         "opacity": 1.0, "base_file": "ps_p0000_u1.png"}
                    ]},
                    {"img_idx": 2, "tree": [
                        {"uid": "u2", "name": "T", "z": 0, "visible": true,
                         "opacity": 1.0, "rendered_file": "ps_p0002_u2_text.png"}
                    ]}
                ]
            })),
            text_images_files: ["mask_page_1.png", "typing_overlay_p0001_1.png"]
                .iter()
                .map(ToString::to_string)
                .collect(),
            text_info: vec![TextInfoFile {
                location: TextInfoLocation::TextImagesDir,
                entries: vec![serde_json::json!({
                    "img_idx": 1, "file": "typing_overlay_p0001_1.png"
                })],
            }],
            bubbles: Some(vec![serde_json::json!({
                "id": 1, "img_idx": 3, "img_u": 0.5, "img_v": 0.5,
                "side": "left", "text": "t", "original_text": "o"
            })]),
        };
        let unsaved = TreeSnapshot {
            tree_rel: "ch1_unsaved".to_string(),
            ..TreeSnapshot::default()
        };
        ChapterSnapshot {
            chapter_rel: "ch1".to_string(),
            page_file_names: vec![
                "000.png".to_string(),
                "001.png".to_string(),
                "002.jpg".to_string(),
                "003.png".to_string(),
            ],
            // Distinct per-page sizes: a mixed-up coordinate space shows up as a
            // wrong number instead of an accidental identity.
            page_sizes: vec![[100, 200], [50, 400], [80, 80], [60, 120]],
            has_alt_vers: false,
            committed,
            unsaved,
            detection: vec![DetectionFiles {
                page_idx: 1,
                blocks: Some(DetectionBlocks::Parsed(serde_json::json!({
                    "source_size": [100, 200],
                    "blocks": [],
                    "mask_file": "00001_mask.png"
                }))),
                has_mask: true,
            }],
        }
    }

    #[test]
    fn build_plan_move_covers_every_category_with_canonical_names() {
        let snapshot = snapshot_for_plan();
        // Move page 0 to the end: 0->3, 1->0, 2->1, 3->2.
        let plan = build_plan(&snapshot, &PageOpKind::Move { from: 0, to: 3 }, 42)
            .expect("plan builds");
        assert_eq!(
            plan.old_to_new,
            vec![Some(3), Some(0), Some(1), Some(2)]
        );

        let final_targets: Vec<(&str, &str)> = plan
            .moves
            .iter()
            .filter_map(|m| match &m.dest {
                MoveDest::Final { path } => Some((m.from.as_str(), path.as_str())),
                MoveDest::Trash { .. } | MoveDest::Discard => None,
            })
            .collect();
        // Source pages keep their extension under the new canonical stem.
        assert!(final_targets.contains(&("ch1/src/000.png", "ch1/src/003.png")));
        assert!(final_targets.contains(&("ch1/src/002.jpg", "ch1/src/001.jpg")));
        // Clean overlays follow the page stem.
        assert!(final_targets.contains(&(
            "ch1/clean_layers/000.png",
            "ch1/clean_layers/003.png"
        )));
        assert!(final_targets.contains(&(
            "ch1/clean_layers/002.png",
            "ch1/clean_layers/001.png"
        )));
        // Layer PNGs get the new `ps_p{page:04}_` prefix.
        assert!(final_targets.contains(&(
            "ch1/layers/ps_p0000_u1.png",
            "ch1/layers/ps_p0003_u1.png"
        )));
        assert!(final_targets.contains(&(
            "ch1/layers/ps_p0002_u2_text.png",
            "ch1/layers/ps_p0001_u2_text.png"
        )));
        // Typing mask follows the raw index format.
        assert!(final_targets.contains(&(
            "ch1/text_images/mask_page_1.png",
            "ch1/text_images/mask_page_0.png"
        )));
        // Detection mask follows the 5-digit format.
        assert!(final_targets.contains(&(
            "ch1/text_detection/00001_mask.png",
            "ch1/text_detection/00000_mask.png"
        )));

        // JSON rewrites: bubbles + layers manifest + text_info + blocks.
        let json_targets: Vec<&str> =
            plan.json_writes.iter().map(|w| w.target.as_str()).collect();
        assert!(json_targets.contains(&"ch1/translation_bubbles.json"));
        assert!(json_targets.contains(&"ch1/layers/layers.json"));
        assert!(json_targets.contains(&"ch1/text_images/text_info.json"));
        assert!(json_targets.contains(&"ch1/text_detection/00000_blocks.json"));

        // The rewritten blocks file references the NEW mask name.
        let blocks = plan
            .json_writes
            .iter()
            .find(|w| w.target == "ch1/text_detection/00000_blocks.json")
            .expect("blocks rewrite present");
        assert!(blocks.content.contains("00000_mask.png"));

        // No trash content for a pure move.
        assert!(plan.trash_writes.is_empty());
        assert!(
            plan.moves
                .iter()
                .all(|m| !matches!(m.dest, MoveDest::Trash { .. }))
        );
        // Every temp lives in the same directory as its source.
        for m in &plan.moves {
            let from_dir = m.from.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            let temp_dir = m.temp.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            assert_eq!(from_dir, temp_dir, "temp of {} in same dir", m.from);
        }
    }

    #[test]
    fn build_plan_delete_moves_artifacts_to_trash() {
        let snapshot = snapshot_for_plan();
        let plan = build_plan(&snapshot, &PageOpKind::Delete { indices: vec![1] }, 7)
            .expect("plan builds");
        assert_eq!(plan.trash_root, "ch1/.pageop_trash/7");
        let trash_targets: Vec<&str> = plan
            .moves
            .iter()
            .filter_map(|m| match &m.dest {
                MoveDest::Trash { path } => Some(path.as_str()),
                MoveDest::Final { .. } | MoveDest::Discard => None,
            })
            .collect();
        assert!(trash_targets.contains(&"ch1/.pageop_trash/7/ch1/src/001.png"));
        assert!(
            trash_targets.contains(&"ch1/.pageop_trash/7/ch1/text_images/mask_page_1.png")
        );
        assert!(trash_targets
            .contains(&"ch1/.pageop_trash/7/ch1/text_detection/00001_blocks.json"));
        assert!(trash_targets
            .contains(&"ch1/.pageop_trash/7/ch1/text_detection/00001_mask.png"));
        // The deleted page's overlay PNG (referenced by its text_info entry)
        // is archived too.
        assert!(trash_targets
            .contains(&"ch1/.pageop_trash/7/ch1/text_images/typing_overlay_p0001_1.png"));
        // Deleted text_info entries are archived.
        assert!(plan.trash_writes.iter().any(|w| w.target
            == "ch1/.pageop_trash/7/ch1/text_images/deleted_text_info.json"));
    }

    #[test]
    fn delete_keeps_text_overlay_referenced_by_surviving_entry() {
        let mut snapshot = snapshot_for_plan();
        snapshot.committed.text_info[0].entries = vec![
            serde_json::json!({"img_idx": 1, "file": "shared.png"}),
            serde_json::json!({"img_idx": 2, "file": "shared.png"}),
        ];
        snapshot.committed.text_images_files.insert("shared.png".to_string());
        let plan = build_plan(&snapshot, &PageOpKind::Delete { indices: vec![1] }, 8)
            .expect("plan builds");
        assert!(!plan.moves.iter().any(|planned| {
            planned.from == "ch1/text_images/shared.png"
                && matches!(planned.dest, MoveDest::Trash { .. })
        }));
    }

    /// A placement with no crop and no scale, for permutation-only tests.
    fn whole_page(page_idx: usize, dx: i64, dy: i64, size: [u32; 2]) -> StitchPlacement {
        StitchPlacement {
            page_idx,
            crop: [0, 0, size[0], size[1]],
            scale: 1.0,
            dx,
            dy,
        }
    }

    fn stitch_op(placements: Vec<StitchPlacement>, width: u32, height: u32) -> PageOpKind {
        PageOpKind::Stitch {
            placements,
            width,
            height,
            background: [0, 0, 0, 0],
        }
    }

    #[test]
    fn stitch_folds_sources_onto_the_lowest_index() {
        // Pages 1 and 3 of 5 merge: 0 stays, 1+3 -> 1, 2 -> 2, 4 -> 3.
        let perm = permutation_for_op(
            &stitch_op(
                vec![
                    whole_page(3, 0, 0, [50, 400]),
                    whole_page(1, 0, 0, [50, 400]),
                ],
                100,
                400,
            ),
            5,
        )
        .expect("valid");
        assert_eq!(
            map_of(&perm),
            vec![Some(0), Some(1), Some(2), Some(1), Some(3)]
        );
        assert_eq!(perm.new_page_count, 4);
        // The merged page is not an "inserted" page: its content comes from the
        // chapter snapshot, not from the request.
        assert!(perm.new_pages.is_empty());

        // Merging the first two pages of 3 leaves 2 pages, primary at index 0.
        let perm = permutation_for_op(
            &stitch_op(
                vec![whole_page(0, 0, 0, [10, 10]), whole_page(1, 0, 0, [10, 10])],
                20,
                10,
            ),
            3,
        )
        .expect("valid");
        assert_eq!(map_of(&perm), vec![Some(0), Some(0), Some(1)]);
        assert_eq!(perm.new_page_count, 2);
    }

    #[test]
    fn stitch_rejects_bad_selections_and_canvases() {
        // Fewer than two pages.
        assert!(matches!(
            permutation_for_op(&stitch_op(vec![whole_page(0, 0, 0, [10, 10])], 10, 10), 3),
            Err(PageOpError::InvalidOp(_))
        ));
        // Duplicate page.
        assert!(matches!(
            permutation_for_op(
                &stitch_op(
                    vec![whole_page(1, 0, 0, [10, 10]), whole_page(1, 0, 0, [10, 10])],
                    20,
                    10
                ),
                3
            ),
            Err(PageOpError::InvalidOp(_))
        ));
        // Out of range.
        assert!(matches!(
            permutation_for_op(
                &stitch_op(
                    vec![whole_page(0, 0, 0, [10, 10]), whole_page(3, 0, 0, [10, 10])],
                    20,
                    10
                ),
                3
            ),
            Err(PageOpError::InvalidOp(_))
        ));
        // Canvas out of bounds (zero, too wide, too many pixels).
        for (w, h) in [(0, 10), (10, 0), (STITCH_MAX_SIDE_PX + 1, 10), (39_000, 39_000)] {
            assert!(
                matches!(
                    permutation_for_op(
                        &stitch_op(
                            vec![whole_page(0, 0, 0, [10, 10]), whole_page(1, 0, 0, [10, 10])],
                            w,
                            h
                        ),
                        3
                    ),
                    Err(PageOpError::InvalidOp(_))
                ),
                "canvas {w}x{h} must be rejected"
            );
        }
    }

    #[test]
    fn placement_map_maps_every_coordinate_space() {
        // A 100x200 page, cropped to its lower-right 60x150 quadrant, doubled,
        // and placed at (40, 10) of a 400x400 canvas.
        let placement = StitchPlacement {
            page_idx: 0,
            crop: [20, 40, 60, 150],
            scale: 2.0,
            dx: 40,
            dy: 10,
        };
        let map = PlacementMap::new(&placement, [100, 200], [400, 400]).expect("valid");
        assert_eq!(map.placed_rect(), [40, 10, 120, 300]);
        assert_eq!(map.crop_rect(), [20, 40, 60, 150]);
        // Absolute page px -> canvas px.
        assert!((map.map_x(20.0) - 40.0).abs() < 1e-9, "crop origin maps to dx");
        assert!((map.map_x(70.0) - 140.0).abs() < 1e-9);
        assert!((map.map_y(40.0) - 10.0).abs() < 1e-9);
        assert!((map.map_y(140.0) - 210.0).abs() < 1e-9);
        // Lengths carry the scale but not the origin.
        assert!((map.map_len(3.0) - 6.0).abs() < 1e-9);
        // Page-normalized uv -> canvas-normalized uv: u=0.2 is page px 20,
        // which is the crop origin, i.e. canvas px 40 = 0.1 of a 400 canvas.
        assert!((map.map_u(0.2) - 0.1).abs() < 1e-9);
        // v=0.2 is page px 40 -> canvas px 10 -> 0.025.
        assert!((map.map_v(0.2) - 0.025).abs() < 1e-9);
    }

    #[test]
    fn placement_map_rejects_geometry_that_does_not_fit() {
        let base = StitchPlacement {
            page_idx: 2,
            crop: [0, 0, 100, 200],
            scale: 1.0,
            dx: 0,
            dy: 0,
        };
        // Crop leaving the page.
        let mut bad = base;
        bad.crop = [50, 0, 100, 200];
        assert!(matches!(
            PlacementMap::new(&bad, [100, 200], [400, 400]),
            Err(PageOpError::InvalidOp(_))
        ));
        // Empty crop.
        let mut bad = base;
        bad.crop = [0, 0, 0, 200];
        assert!(matches!(
            PlacementMap::new(&bad, [100, 200], [400, 400]),
            Err(PageOpError::InvalidOp(_))
        ));
        // Scale out of range, non-finite, or zero.
        for scale in [0.0, -1.0, f32::NAN, STITCH_MAX_SCALE + 0.5] {
            let mut bad = base;
            bad.scale = scale;
            assert!(
                matches!(
                    PlacementMap::new(&bad, [100, 200], [400, 400]),
                    Err(PageOpError::InvalidOp(_))
                ),
                "scale {scale} must be rejected"
            );
        }
        // Placed rect leaving the canvas (negative and overflowing).
        let mut bad = base;
        bad.dx = -1;
        assert!(matches!(
            PlacementMap::new(&bad, [100, 200], [400, 400]),
            Err(PageOpError::InvalidOp(_))
        ));
        let mut bad = base;
        bad.dy = 201;
        assert!(matches!(
            PlacementMap::new(&bad, [100, 200], [400, 400]),
            Err(PageOpError::InvalidOp(_))
        ));
        // A zero-sized page has no usable geometry.
        assert!(matches!(
            PlacementMap::new(&base, [0, 200], [400, 400]),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn build_plan_stitch_composes_rasters_and_merges_documents() {
        let snapshot = snapshot_for_plan();
        // Merge pages 0 (100x200) and 2 (80x80) onto a 180x200 canvas.
        let op = stitch_op(
            vec![
                whole_page(0, 0, 0, [100, 200]),
                whole_page(2, 100, 0, [80, 80]),
            ],
            180,
            200,
        );
        let plan = build_plan(&snapshot, &op, 55).expect("plan builds");
        assert_eq!(plan.old_to_new, vec![Some(0), Some(1), Some(0), Some(2)]);
        assert_eq!(plan.new_page_count, 3);

        // Both source pages go to the trash; nothing is destroyed.
        let trashed: Vec<&str> = plan
            .moves
            .iter()
            .filter_map(|m| match &m.dest {
                MoveDest::Trash { path } => Some(path.as_str()),
                MoveDest::Final { .. } | MoveDest::Discard => None,
            })
            .collect();
        assert!(trashed.contains(&"ch1/.pageop_trash/55/ch1/src/000.png"));
        assert!(trashed.contains(&"ch1/.pageop_trash/55/ch1/src/002.jpg"));
        assert!(trashed.contains(&"ch1/.pageop_trash/55/ch1/clean_layers/000.png"));
        assert!(trashed.contains(&"ch1/.pageop_trash/55/ch1/clean_layers/002.png"));

        // The merged page is a composed PNG of both sources, in page order.
        let create = plan
            .creates
            .iter()
            .find(|c| c.target == "ch1/src/000.png")
            .expect("composed page staged");
        let NewPageContent::ComposedPng {
            width,
            height,
            sources,
            ..
        } = &create.content
        else {
            panic!("stitched page must be a composed PNG");
        };
        assert_eq!((*width, *height), (180, 200));
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].path, "ch1/src/000.png");
        assert_eq!(sources[0].dest, [0, 0, 100, 200]);
        assert_eq!(sources[1].path, "ch1/src/002.jpg");
        assert_eq!(sources[1].dest, [100, 0, 80, 80]);
        // Clean overlays of both sources compose onto ONE transparent overlay.
        let clean = plan
            .creates
            .iter()
            .find(|c| c.target == "ch1/clean_layers/000.png")
            .expect("composed clean overlay staged");
        let NewPageContent::ComposedPng {
            background,
            sources,
            ..
        } = &clean.content
        else {
            panic!("composed overlay expected");
        };
        assert_eq!(*background, [0, 0, 0, 0]);
        assert_eq!(sources.len(), 2);

        // Surviving pages compact around the merged one.
        let finals: Vec<(&str, &str)> = plan
            .moves
            .iter()
            .filter_map(|m| match &m.dest {
                MoveDest::Final { path } => Some((m.from.as_str(), path.as_str())),
                MoveDest::Trash { .. } | MoveDest::Discard => None,
            })
            .collect();
        // Page 1 keeps index 1: an identity rename is never planned.
        assert!(!finals.iter().any(|(from, _)| *from == "ch1/src/001.png"));
        assert!(finals.contains(&("ch1/src/003.png", "ch1/src/002.png")));
        // Layer PNGs of both merged pages now share the primary's prefix.
        assert!(finals.contains(&(
            "ch1/layers/ps_p0002_u2_text.png",
            "ch1/layers/ps_p0000_u2_text.png"
        )));

        // The two manifest pages became one entry at the merged index.
        let manifest = plan
            .json_writes
            .iter()
            .find(|w| w.target == "ch1/layers/layers.json")
            .expect("manifest rewritten");
        let manifest: Value = serde_json::from_str(&manifest.content).expect("valid json");
        let pages = manifest["pages"].as_array().expect("pages");
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0]["img_idx"], serde_json::json!(0));
        assert_eq!(pages[0]["tree"].as_array().expect("tree").len(), 2);
    }

    #[test]
    fn build_plan_stitch_warns_about_position_matched_alt_versions() {
        let mut snapshot = snapshot_for_plan();
        snapshot.has_alt_vers = true;
        let op = stitch_op(
            vec![
                whole_page(0, 0, 0, [100, 200]),
                whole_page(2, 100, 0, [80, 80]),
            ],
            180,
            200,
        );
        let plan = build_plan(&snapshot, &op, 58).expect("plan builds");
        assert!(
            plan.warnings.iter().any(|w| w.contains("alt_vers")),
            "merging pages must warn about the alt-version misalignment: {:?}",
            plan.warnings
        );
        // A chapter without alternate versions stays silent.
        snapshot.has_alt_vers = false;
        let plan = build_plan(&snapshot, &op, 59).expect("plan builds");
        assert!(!plan.warnings.iter().any(|w| w.contains("alt_vers")));
    }

    #[test]
    fn build_plan_stitch_refuses_pages_sharing_a_layer_uid() {
        let mut snapshot = snapshot_for_plan();
        // Page 2 carries the same uid as page 0: both PNGs would become
        // `ps_p0000_u1.png`.
        snapshot
            .committed
            .layers_files
            .insert("ps_p0002_u1.png".to_string());
        let op = stitch_op(
            vec![
                whole_page(0, 0, 0, [100, 200]),
                whole_page(2, 100, 0, [80, 80]),
            ],
            180,
            200,
        );
        let err = build_plan(&snapshot, &op, 56).expect_err("uid collision must be refused");
        assert!(matches!(err, PageOpError::InvalidOp(_)), "got: {err}");
    }

    #[test]
    fn build_plan_stitch_needs_page_sizes() {
        let mut snapshot = snapshot_for_plan();
        snapshot.page_sizes.clear();
        let op = stitch_op(
            vec![
                whole_page(0, 0, 0, [100, 200]),
                whole_page(2, 100, 0, [80, 80]),
            ],
            180,
            200,
        );
        assert!(matches!(
            build_plan(&snapshot, &op, 57),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    // -----------------------------------------------------------------------
    // Split.
    // -----------------------------------------------------------------------

    fn split_op(page_idx: usize, axis: SplitAxis, cuts: &[u32], order: &[usize]) -> PageOpKind {
        PageOpKind::Split {
            page_idx,
            axis,
            cuts: cuts.to_vec(),
            order: order.to_vec(),
        }
    }

    #[test]
    fn split_fans_one_page_into_a_contiguous_run_of_parts() {
        // Page 1 of 4 cut into 3 parts: pages 2 and 3 shift up by 2.
        let perm = permutation_for_op(
            &split_op(1, SplitAxis::Horizontal, &[10, 20], &[0, 1, 2]),
            4,
        )
        .expect("valid");
        assert_eq!(map_of(&perm), vec![Some(0), Some(1), Some(4), Some(5)]);
        assert_eq!(perm.new_page_count, 6);
        // The parts are not "inserted" pages: their pixels are cut out of the
        // chapter snapshot, not derivable from the request.
        assert!(perm.new_pages.is_empty());

        // D1: the representative index is the SOURCE index whatever order the
        // user chose — the part ordered first is the one that lands there.
        let perm =
            permutation_for_op(&split_op(2, SplitAxis::Vertical, &[10], &[1, 0]), 4)
                .expect("valid");
        assert_eq!(map_of(&perm), vec![Some(0), Some(1), Some(2), Some(4)]);
        assert_eq!(perm.new_page_count, 5);
    }

    #[test]
    fn split_rejects_bad_pages_cuts_and_orders() {
        for (case, op) in [
            ("page out of range", split_op(4, SplitAxis::Horizontal, &[10], &[0, 1])),
            ("no cuts", split_op(1, SplitAxis::Horizontal, &[], &[0])),
            (
                "cuts not strictly increasing",
                split_op(1, SplitAxis::Horizontal, &[10, 10], &[0, 1, 2]),
            ),
            ("order too short", split_op(1, SplitAxis::Horizontal, &[10], &[0])),
            (
                "duplicated order position",
                split_op(1, SplitAxis::Horizontal, &[10], &[0, 0]),
            ),
            (
                "order position out of range",
                split_op(1, SplitAxis::Horizontal, &[10], &[0, 2]),
            ),
        ] {
            assert!(
                matches!(
                    permutation_for_op(&op, 4),
                    Err(PageOpError::InvalidOp(_))
                ),
                "{case} must be rejected"
            );
        }
        // Too many parts.
        let cuts: Vec<u32> = (1..=u32::try_from(SPLIT_MAX_PARTS).expect("fits")).collect();
        let order: Vec<usize> = (0..cuts.len() + 1).collect();
        assert!(matches!(
            permutation_for_op(&split_op(1, SplitAxis::Horizontal, &cuts, &order), 4),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn split_parts_map_every_coordinate_space_and_reject_empty_parts() {
        // A 50x400 page cut horizontally at 100 and 250, parts kept in order.
        let geo = resolve_split_parts(1, SplitAxis::Horizontal, [50, 400], &[100, 250], &[0, 1, 2])
            .expect("valid");
        assert_eq!(geo.part_count(), 3);
        assert_eq!(geo.part_size(0), Some([50, 100]));
        assert_eq!(geo.part_size(1), Some([50, 150]));
        assert_eq!(geo.part_size(2), Some([50, 150]));
        assert_eq!(geo.part_new_idx(0), Some(1));
        assert_eq!(geo.part_new_idx(2), Some(3));
        let middle = geo.placement(1).expect("middle part");
        assert_eq!(middle.crop_rect(), [0, 100, 50, 150]);
        assert_eq!(middle.placed_rect(), [0, 0, 50, 150]);
        // Absolute page px: the first cut is the top of the middle part.
        assert!(middle.map_y(100.0).abs() < 1e-9);
        assert!((middle.map_y(175.0) - 75.0).abs() < 1e-9);
        // Page-normalized v renormalizes onto the part; u is untouched by a
        // horizontal cut (the part is as wide as the page).
        assert!(middle.map_v(0.25).abs() < 1e-9);
        assert!((middle.map_u(0.5) - 0.5).abs() < 1e-9);
        // A split never resamples, so page-px lengths pass through.
        assert!((middle.map_len(7.0) - 7.0).abs() < 1e-9);

        // The transpose: a vertical cut renormalizes u and leaves v alone.
        let geo = resolve_split_parts(0, SplitAxis::Vertical, [50, 400], &[20], &[1, 0])
            .expect("valid");
        assert_eq!(geo.part_size(0), Some([20, 400]));
        assert_eq!(geo.part_size(1), Some([30, 400]));
        // Reversed order: the LEFT part goes second, the right one first.
        assert_eq!(geo.part_new_idx(0), Some(1));
        assert_eq!(geo.part_new_idx(1), Some(0));
        let right = geo.placement(1).expect("right part");
        assert!(right.map_x(20.0).abs() < 1e-9);
        assert!((right.map_v(0.25) - 0.25).abs() < 1e-9);

        // Cuts on or past the borders would produce a zero-sized part.
        for cut in [0u32, 400] {
            assert!(
                matches!(
                    resolve_split_parts(1, SplitAxis::Horizontal, [50, 400], &[cut], &[0, 1]),
                    Err(PageOpError::InvalidOp(_))
                ),
                "cut at {cut} must be rejected"
            );
        }
        assert!(matches!(
            resolve_split_parts(1, SplitAxis::Horizontal, [50, 400], &[100, 100], &[0, 1, 2]),
            Err(PageOpError::InvalidOp(_))
        ));
        // A page with no pixels has no geometry at all.
        assert!(matches!(
            resolve_split_parts(1, SplitAxis::Horizontal, [50, 0], &[10], &[0, 1]),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn split_assignment_uses_exact_area_with_a_top_left_tie_break() {
        // A 100x400 page cut in half horizontally.
        let geo = resolve_split_parts(0, SplitAxis::Horizontal, [100, 400], &[200], &[0, 1])
            .expect("valid");
        let rect = |top: f64, bottom: f64| {
            [[10.0, top], [90.0, top], [90.0, bottom], [10.0, bottom]]
        };
        assert_eq!(geo.part_for_polygon(&rect(220.0, 300.0)), Some(1));
        // 75% above the cut.
        assert_eq!(geo.part_for_polygon(&rect(140.0, 220.0)), Some(0));
        // Exactly half on each side: the TOP part wins (geometric position,
        // not user order).
        assert_eq!(geo.part_for_polygon(&rect(150.0, 250.0)), Some(0));
        // A right triangle that is WIDE AT THE BOTTOM: its bounding box spans
        // the whole page (a tie a bbox rule would resolve to the top part),
        // but three quarters of its real area sit below the cut.
        assert_eq!(
            geo.part_for_polygon(&[[0.0, 0.0], [0.0, 400.0], [100.0, 400.0]]),
            Some(1)
        );
        // A polygon enclosing no area has no majority at all.
        assert_eq!(
            geo.part_for_polygon(&[[10.0, 10.0], [90.0, 10.0], [50.0, 10.0]]),
            None
        );
        // Points follow the part CROP rectangles: a point on the cut line
        // belongs to the part that owns that pixel row.
        assert_eq!(geo.part_for_point(50.0, 199.9), 0);
        assert_eq!(geo.part_for_point(50.0, 200.0), 1);
        // Outside the page, the extreme parts claim the point.
        assert_eq!(geo.part_for_point(50.0, -20.0), 0);
        assert_eq!(geo.part_for_point(50.0, 900.0), 1);

        // The transpose: an exact tie across a vertical cut goes LEFT.
        let geo = resolve_split_parts(0, SplitAxis::Vertical, [400, 100], &[200], &[0, 1])
            .expect("valid");
        assert_eq!(
            geo.part_for_polygon(&[
                [150.0, 10.0],
                [250.0, 10.0],
                [250.0, 90.0],
                [150.0, 90.0]
            ]),
            Some(0)
        );
    }

    /// The plan snapshot with page 1 (50x400) carrying a full layer stack, a
    /// clean overlay, a typing mask and detection data, so a split of that page
    /// has something to partition in every category.
    fn snapshot_for_split() -> ChapterSnapshot {
        let mut snapshot = snapshot_for_plan();
        snapshot.committed.clean_overlay_stems.insert("001".to_string());
        for name in ["ps_p0001_top.png", "ps_p0001_bottom.png", "ps_p0001_text.png"] {
            snapshot.committed.layers_files.insert(name.to_string());
        }
        // Only the TEXT node needs a probe: a raster stores its `image_size`.
        snapshot
            .committed
            .layer_png_sizes
            .insert("ps_p0001_text.png".to_string(), [20, 20]);
        snapshot.committed.layers_manifest = Some(serde_json::json!({
            "schema_version": 4,
            "pages": [
                {"img_idx": 1,
                 "groups": [{"uid": "g1", "name": "G", "visible": true, "opacity": 1.0}],
                 "text_groups": [{"layer_idx": 0, "z": 1, "name": "TG"}],
                 "tree": [
                    {"uid": "top", "name": "Top", "kind": "raster", "z": 0,
                     "visible": true, "opacity": 1.0, "group_uid": "g1",
                     "base_file": "ps_p0001_top.png", "image_size": [40, 40],
                     "transform": {"cx": 25.0, "cy": 50.0, "rotation": 0.0, "scale": 1.0}},
                    {"uid": "text", "name": "T", "kind": "text", "z": 1, "layer_idx": 0,
                     "visible": true, "opacity": 1.0,
                     "rendered_file": "ps_p0001_text.png",
                     "transform": {"cx": 25.0, "cy": 300.0, "rotation": 0.0, "scale": 1.0}},
                    {"uid": "bottom", "name": "Bottom", "kind": "raster", "z": 2,
                     "visible": true, "opacity": 1.0, "group_uid": "g1",
                     "base_file": "ps_p0001_bottom.png", "image_size": [40, 40],
                     "transform": {"cx": 25.0, "cy": 350.0, "rotation": 0.0, "scale": 1.0}}
                 ]},
                {"img_idx": 2, "tree": [
                    {"uid": "u2", "name": "T", "z": 0, "visible": true,
                     "opacity": 1.0, "rendered_file": "ps_p0002_u2_text.png"}
                ]}
            ]
        }));
        snapshot.detection = vec![DetectionFiles {
            page_idx: 1,
            blocks: Some(DetectionBlocks::Parsed(serde_json::json!({
                "page_idx": 1,
                "source_size": [50, 400],
                "mask_size": [50, 400],
                "blocks": [
                    {"x1": 1.0, "y1": 2.0, "x2": 3.0, "y2": 40.0},
                    {"x1": 5.0, "y1": 300.0, "x2": 9.0, "y2": 380.0}
                ],
                "mask_file": "00001_mask.png"
            }))),
            has_mask: true,
        }];
        snapshot
    }

    #[test]
    fn build_plan_split_cuts_rasters_and_partitions_documents() {
        let snapshot = snapshot_for_split();
        // Cut page 1 (50x400) in half; the top part keeps index 1.
        let plan = build_plan(
            &snapshot,
            &split_op(1, SplitAxis::Horizontal, &[200], &[0, 1]),
            77,
        )
        .expect("plan builds");
        assert_eq!(plan.old_to_new, vec![Some(0), Some(1), Some(3), Some(4)]);
        assert_eq!(plan.new_page_count, 5);

        // Both parts are staged as PNGs cut out of the source page.
        let cut_source = |target: &str, crop: [u32; 4], size: (u32, u32)| {
            let create = plan
                .creates
                .iter()
                .find(|c| c.target == target)
                .unwrap_or_else(|| panic!("{target} staged"));
            let NewPageContent::ComposedPng {
                width,
                height,
                sources,
                ..
            } = &create.content
            else {
                panic!("{target} must be a composed PNG");
            };
            assert_eq!((*width, *height), size);
            assert_eq!(sources.len(), 1, "a split part has exactly one source");
            assert_eq!(sources[0].crop, crop);
            // Crop == destination, so `encode_composed_png` copies the pixels
            // instead of resampling.
            assert_eq!(sources[0].dest, [0, 0, crop[2], crop[3]]);
            sources[0].path.clone()
        };
        assert_eq!(
            cut_source("ch1/src/001.png", [0, 0, 50, 200], (50, 200)),
            "ch1/src/001.png"
        );
        assert_eq!(
            cut_source("ch1/src/002.png", [0, 200, 50, 200], (50, 200)),
            "ch1/src/001.png"
        );
        cut_source("ch1/clean_layers/001.png", [0, 0, 50, 200], (50, 200));
        cut_source("ch1/clean_layers/002.png", [0, 200, 50, 200], (50, 200));
        cut_source("ch1/text_images/mask_page_1.png", [0, 0, 50, 200], (50, 200));
        cut_source("ch1/text_images/mask_page_2.png", [0, 200, 50, 200], (50, 200));
        cut_source("ch1/text_detection/00001_mask.png", [0, 0, 50, 200], (50, 200));
        cut_source("ch1/text_detection/00002_mask.png", [0, 200, 50, 200], (50, 200));

        // The originals are recoverable, never destroyed.
        let trashed: Vec<&str> = plan
            .moves
            .iter()
            .filter_map(|m| match &m.dest {
                MoveDest::Trash { path } => Some(path.as_str()),
                MoveDest::Final { .. } | MoveDest::Discard => None,
            })
            .collect();
        assert!(trashed.contains(&"ch1/.pageop_trash/77/ch1/src/001.png"));
        assert!(trashed.contains(&"ch1/.pageop_trash/77/ch1/clean_layers/001.png"));
        assert!(trashed.contains(&"ch1/.pageop_trash/77/ch1/text_images/mask_page_1.png"));

        // Layer PNGs of ONE page fan out onto DIFFERENT prefixes.
        let finals: Vec<(&str, &str)> = plan
            .moves
            .iter()
            .filter_map(|m| match &m.dest {
                MoveDest::Final { path } => Some((m.from.as_str(), path.as_str())),
                MoveDest::Trash { .. } | MoveDest::Discard => None,
            })
            .collect();
        // `top` stays on part 0, which keeps index 1: an identity rename is
        // never planned.
        assert!(!finals.iter().any(|(from, _)| *from == "ch1/layers/ps_p0001_top.png"));
        assert!(finals.contains(&(
            "ch1/layers/ps_p0001_bottom.png",
            "ch1/layers/ps_p0002_bottom.png"
        )));
        assert!(finals.contains(&(
            "ch1/layers/ps_p0001_text.png",
            "ch1/layers/ps_p0002_text.png"
        )));
        // The untouched pages compact around the parts.
        assert!(finals.contains(&("ch1/src/002.jpg", "ch1/src/003.jpg")));
        assert!(finals.contains(&(
            "ch1/layers/ps_p0002_u2_text.png",
            "ch1/layers/ps_p0003_u2_text.png"
        )));

        // The manifest page entry became TWO entries, one per part.
        let manifest = plan
            .json_writes
            .iter()
            .find(|w| w.target == "ch1/layers/layers.json")
            .expect("manifest rewritten");
        let manifest: Value = serde_json::from_str(&manifest.content).expect("valid json");
        let pages = manifest["pages"].as_array().expect("pages");
        let by_idx = |idx: u64| {
            pages
                .iter()
                .find(|page| page["img_idx"] == serde_json::json!(idx))
                .unwrap_or_else(|| panic!("page {idx} present"))
        };
        let top = by_idx(1);
        assert_eq!(top["tree"].as_array().expect("tree").len(), 1);
        assert_eq!(top["tree"][0]["uid"], serde_json::json!("top"));
        // Geometry mapped into the part: cy 50 stays 50 in the top part.
        assert_eq!(top["tree"][0]["transform"]["cy"], serde_json::json!(50.0));
        // The PS group is duplicated into every part holding a member.
        assert_eq!(top["groups"].as_array().expect("groups").len(), 1);
        // No unpinned text node here, so the text-group band is dropped.
        assert!(top.get("text_groups").is_none());

        let bottom = by_idx(2);
        assert_eq!(bottom["tree"].as_array().expect("tree").len(), 2);
        // cy 300 of the source page is cy 100 of the bottom part.
        assert_eq!(bottom["tree"][0]["transform"]["cy"], serde_json::json!(100.0));
        assert_eq!(bottom["tree"][1]["transform"]["cy"], serde_json::json!(150.0));
        // z is re-ranked densely PER PART: the source bands 1 and 2 become 0
        // and 1, and the text group keeps sharing the text node's band.
        assert_eq!(bottom["tree"][0]["z"], serde_json::json!(0));
        assert_eq!(bottom["tree"][1]["z"], serde_json::json!(1));
        assert_eq!(bottom["text_groups"][0]["z"], serde_json::json!(0));
        assert_eq!(bottom["groups"].as_array().expect("groups").len(), 1);
        // Page 2's untouched entry followed the ordinary index shift.
        assert_eq!(
            by_idx(3)["tree"][0]["rendered_file"],
            serde_json::json!("ps_p0003_u2_text.png")
        );

        // Detection: one document per part, blocks routed by area and mapped.
        let blocks_of = |target: &str| -> Value {
            let write = plan
                .json_writes
                .iter()
                .find(|w| w.target == target)
                .unwrap_or_else(|| panic!("{target} written"));
            serde_json::from_str(&write.content).expect("valid json")
        };
        let first = blocks_of("ch1/text_detection/00001_blocks.json");
        assert_eq!(first["source_size"], serde_json::json!([50, 200]));
        assert_eq!(first["mask_size"], serde_json::json!([50, 200]));
        assert_eq!(first["mask_file"], serde_json::json!("00001_mask.png"));
        assert_eq!(first["blocks"].as_array().expect("blocks").len(), 1);
        assert_eq!(first["blocks"][0]["y2"], serde_json::json!(40.0));
        let second = blocks_of("ch1/text_detection/00002_blocks.json");
        assert_eq!(second["blocks"].as_array().expect("blocks").len(), 1);
        // Absolute page px 300 is px 100 of the bottom part.
        assert_eq!(second["blocks"][0]["y1"], serde_json::json!(100.0));
        assert_eq!(second["mask_file"], serde_json::json!("00002_mask.png"));
    }

    #[test]
    fn build_plan_split_falls_back_to_the_centre_when_a_text_render_cannot_be_probed() {
        let mut snapshot = snapshot_for_split();
        // The text node's PNG is unreadable, so its `image_size` stays unknown.
        snapshot.committed.layer_png_sizes.clear();
        let plan = build_plan(
            &snapshot,
            &split_op(1, SplitAxis::Horizontal, &[200], &[0, 1]),
            78,
        )
        .expect("plan builds");
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("'text'") && w.contains("centre point")),
            "the degradation must be reported: {:?}",
            plan.warnings
        );
        // It still lands on the part its centre (cy 300) sits in, so the layer
        // is never lost — only the exactness of the rule degrades.
        let manifest = plan
            .json_writes
            .iter()
            .find(|w| w.target == "ch1/layers/layers.json")
            .expect("manifest rewritten");
        let manifest: Value = serde_json::from_str(&manifest.content).expect("valid json");
        let bottom = manifest["pages"]
            .as_array()
            .expect("pages")
            .iter()
            .find(|page| page["img_idx"] == serde_json::json!(2))
            .expect("bottom part present");
        assert!(
            bottom["tree"]
                .as_array()
                .expect("tree")
                .iter()
                .any(|rec| rec["uid"] == serde_json::json!("text"))
        );
    }

    #[test]
    fn build_plan_split_needs_page_sizes_and_warns_about_alt_versions() {
        let mut snapshot = snapshot_for_split();
        snapshot.has_alt_vers = true;
        let op = split_op(1, SplitAxis::Horizontal, &[200], &[0, 1]);
        let plan = build_plan(&snapshot, &op, 79).expect("plan builds");
        assert!(
            plan.warnings.iter().any(|w| w.contains("alt_vers")),
            "changing the page count must warn: {:?}",
            plan.warnings
        );
        snapshot.page_sizes.clear();
        assert!(matches!(
            build_plan(&snapshot, &op, 80),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn build_plan_split_trashes_detection_it_cannot_cut() {
        let mut snapshot = snapshot_for_split();
        // A mask smaller than its page cannot be cut without inventing a scale.
        snapshot.detection[0].blocks = Some(DetectionBlocks::Parsed(serde_json::json!({
            "source_size": [50, 400],
            "mask_size": [25, 200],
            "blocks": [],
            "mask_file": "00001_mask.png"
        })));
        let plan = build_plan(
            &snapshot,
            &split_op(1, SplitAxis::Horizontal, &[200], &[0, 1]),
            81,
        )
        .expect("plan builds");
        assert!(
            plan.warnings.iter().any(|w| w.contains("text detection")),
            "the degradation must be reported: {:?}",
            plan.warnings
        );
        assert!(
            !plan
                .json_writes
                .iter()
                .any(|w| w.target.contains("text_detection")),
            "no detection document may be written"
        );
        assert!(
            !plan
                .creates
                .iter()
                .any(|c| c.target.contains("text_detection")),
            "no detection mask may be cut"
        );
    }

    #[test]
    fn build_plan_identity_move_is_noop() {
        // A snapshot whose names are already canonical and an identity move.
        let mut snapshot = snapshot_for_plan();
        snapshot.detection.clear();
        let plan = build_plan(&snapshot, &PageOpKind::Move { from: 1, to: 1 }, 9)
            .expect("plan builds");
        assert!(plan.is_noop(), "identity op should plan no actions");
    }
}
