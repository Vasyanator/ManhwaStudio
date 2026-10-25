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
- OwnedDocFormat / PlannedWriteFormat: the on-disk format (`.json` / `.db`) of
  the two OWNED chapter documents (layers manifest, bubbles) as found by the
  scan, and how phase B writes a journaled body (plain file vs. `ms_docstore`).

- PlacementMap: the affine of ONE page onto ONE output canvas — a rotation
  (`crop_geometry::RotatedPage`), then a crop, then a uniform scale and a
  translation — shared by the stitch, the split and the crop; the first two
  place with the identity rotation, the crop is the one rotating placement.
- StitchGeometry / SplitGeometry / CropGeometry / PageGeometry: the resolved
  pixel-identity request every remap of an affected page is routed through.
  `PageGeometry` has no per-variant accessor on purpose — see its doc.
- SplitTreeRouting: per-tree layer uid -> part and layer PNG -> new page index
  tables of a split (the 1 -> N fan-out `old_to_new` cannot express), plus the
  PNGs whose only claimants sit on deleted parts.
- ComposeSource / ComposeRotation / NewPageContent::ComposedPng: the recipe
  `fs_exec` executes to build a stitched, cut or rotated+cropped raster during
  phase A.

Key functions (this file declares the types; behaviour lives in `plan/`):
- permutation_for_op() (`plan/permutation.rs`): op -> permutation +
  validation (pure).
- build_plan() (`plan/builder.rs`, re-exported here): snapshot + op ->
  PageOpPlan (pure; no filesystem access).
- build_stitch_geometry() (`plan/geometry.rs`): stitch request + snapshot ->
  validated affines.
- build_split_geometry() (`plan/geometry.rs`): split request + snapshot ->
  validated part affines plus the per-tree layer routing.
- build_crop_geometry() (`plan/geometry.rs`): crop request + snapshot -> the
  validated rotating affine plus the per-tree set of layers the new frame
  removes.
- validate_split_routing() (`plan/split.rs`): the single legality rule of a
  split's `order` / `deleted` arrays, shared by the index-math and the
  geometry validator.
- canonical page-keyed file-name helpers shared with the scanner (here).

Child modules (`plan/MODULE_README.md`): `placement` (PlacementMap,
StitchGeometry impls), `split`, `crop`, `permutation`, `builder`
(PlanBuilder impl, build_plan), `geometry` (snapshot-dependent geometry and
raster compose planners), `artifacts` (per-artifact planners), `detection`,
`tests`.

Notes:
All plan paths are strings relative to the TITLE directory (the parent of the
chapter dir) using '/' separators, because the transaction spans both the
committed chapter tree and its sibling `{chapter}_unsaved` staging tree.
No function in this file touches the filesystem.
*/

use super::crop_geometry::RotatedPage;
use super::SplitAxis;
use ms_config as config;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;

mod artifacts;
mod builder;
mod crop;
mod detection;
mod geometry;
mod permutation;
mod placement;
mod split;
#[cfg(test)]
mod tests;

pub(crate) use builder::build_plan;

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
pub const STITCH_MAX_SIDE_PX: u32 = 40_000;
/// Inclusive bound on the stitched canvas area, in pixels. Caps the RGBA
/// staging buffer of the compose step at ~800 MB. Re-exported by `page_ops`
/// for the stitch UI's own pre-validation.
pub const STITCH_MAX_TOTAL_PX: u64 = 200_000_000;
/// Inclusive upper bound of a placement's uniform scale factor. Re-exported by
/// `page_ops` for the stitch UI's own pre-validation.
pub const STITCH_MAX_SCALE: f32 = 16.0;
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
/// The affine is a ROTATION of the source page, then a crop, then a uniform
/// scale and a translation:
///
/// ```text
/// (rx, ry)        = rotation(x, y)                 // page px -> rotated canvas px
/// map_point(x, y) = ((rx - crop.x) * scale + dx, (ry - crop.y) * scale + dy)
/// ```
///
/// Both existing operations place with [`PageRotation::IDENTITY`](crate::crop_geometry::PageRotation::IDENTITY), for which
/// the rotation step returns its input bit-for-bit and the whole map collapses
/// to the separable crop/scale/translate it has always been. A ROTATING
/// placement reads its `crop` in the ROTATED CANVAS' pixels (see
/// [`RotatedPage`]) rather than in the source page's own, which is what keeps
/// [`PlacementMap::crop_rect`] an exact axis-aligned rectangle instead of a
/// bounding-box approximation.
///
/// Built (and fully validated) by [`PlacementMap::new`] /
/// [`PlacementMap::with_rotation`]; all remaps of that page's artifacts go
/// through it, so the three on-disk coordinate spaces stay distinguishable:
/// page-normalized uv uses [`PlacementMap::map_uv`], absolute page pixels use
/// [`PlacementMap::map_point`], and layer-image-local pixels are never mapped
/// at all (the layer PNGs are not resampled). A stored MAGNITUDE (a length, a
/// half-extent, a page-size multiplier) is not a coordinate and uses
/// [`PlacementMap::map_extent`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PlacementMap {
    /// Source page plus the rotation applied BEFORE the crop. The crop fields
    /// below live in this rotated canvas' pixel space, which equals the source
    /// page's own space whenever the rotation is the identity.
    source: RotatedPage,
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
    /// Geometric part each layer node of the split page belongs to, by uid,
    /// DELETED parts included — the consumer drops those records, and must be
    /// able to tell them from records it could not route at all.
    node_part: std::collections::HashMap<String, usize>,
    /// New page index each layer PNG of the split page moves to, by file name.
    file_new_idx: std::collections::HashMap<String, usize>,
    /// Layer PNGs of the split page referenced ONLY by records on deleted
    /// parts. They are trashed rather than renamed: no surviving manifest
    /// entry references them any more. Disjoint from `file_new_idx` — a claim
    /// from a surviving record always wins, because that record still needs
    /// the pixels.
    deleted_files: std::collections::HashSet<String>,
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
    /// Index of each geometric part in the NEW page order, `None` for a part
    /// the request DELETED instead of turning into a page. The `Some` values
    /// are the consecutive run `source_old_idx ..` in the survivors' own order.
    part_new_idx: Vec<Option<usize>>,
    /// Geometric part that keeps the source page's index — the survivor the
    /// user ordered first. Always a part with a `Some` `part_new_idx`, because
    /// deleting every part is refused.
    first_kept_part: usize,
    /// Layer routing per tree, keyed by `TreeSnapshot::tree_rel`.
    trees: std::collections::BTreeMap<String, SplitTreeRouting>,
    /// Plan-time diagnostics collected while resolving the routing (probe
    /// failures, orphan PNGs); drained into the plan's warnings.
    warnings: Vec<String>,
}

// ---------------------------------------------------------------------------
// Crop geometry.
//
// A crop ROTATES one page and keeps a rectangular region of the rotated
// canvas. Like a split part it is one `PlacementMap` with `scale = 1` and
// `dx = dy = 0` — the difference is that its `PageRotation` is not the
// identity, so the map's `crop` is read in the ROTATED CANVAS' pixels and the
// pixel executor must rotate before it crops.
//
// What is specific to a crop is not routing (the page count does not change and
// there is only one destination) but SURVIVAL: an entry whose footprint still
// overlaps the kept region moves with the page and may hang off its edge, while
// an entry lying entirely outside it is archived exactly as a page delete would
// archive it.
// ---------------------------------------------------------------------------

/// Per-tree record of what the crop DROPS: the layer nodes of the cropped page
/// that fall entirely outside the kept region, and the layer PNGs only they
/// reference.
///
/// Keyed by data unique inside one tree (a layer uid, a layer PNG file name),
/// exactly like [`SplitTreeRouting`], and for the same reason: the decision
/// needs the manifest and the probed PNG sizes, which only the plan-time
/// snapshot carries, while the consumers (`plan_layer_pngs`,
/// `json_remap::remap_layers_manifest`) see one document at a time.
#[derive(Debug, Clone, Default)]
pub(crate) struct CropTreeRouting {
    /// Uids of the cropped page's layer records that survive nowhere.
    dropped_nodes: std::collections::HashSet<String>,
    /// Layer PNGs of the cropped page referenced ONLY by dropped records. A
    /// claim from a SURVIVING record always wins — that record still needs the
    /// pixels — so this set is disjoint from the surviving records' files.
    dropped_files: std::collections::HashSet<String>,
}

/// Everything the plan and the JSON remaps need to know about a crop: which
/// page is rotated, by how much, which region of the rotated canvas survives,
/// the one affine every artifact of that page is mapped through, and which of
/// that page's layers do not survive it.
#[derive(Debug, Clone)]
pub(crate) struct CropGeometry {
    /// Index of the cropped page in the CURRENT page order. Unchanged by the
    /// operation: a crop's `old_to_new` is the identity.
    source_old_idx: usize,
    /// Pixel size `[w, h]` of the source page, the space bubbles' normalized uv
    /// and layers' absolute px are expressed in.
    page_size: [u32; 2],
    /// Kept region `[x, y, w, h]` of the rotated canvas — the new page size.
    rect: [u32; 4],
    /// The affine: source page pixels -> new page pixels.
    placement: PlacementMap,
    /// Dropped layers per tree, keyed by `TreeSnapshot::tree_rel`.
    trees: std::collections::BTreeMap<String, CropTreeRouting>,
    /// Plan-time diagnostics collected while resolving what survives; drained
    /// into the plan's warnings by `build_plan`.
    warnings: Vec<String>,
}

/// Geometry attached to an operation that changes a page's PIXEL identity.
///
/// Ordinary operations (move / insert / delete) only re-key files: every page
/// keeps its own coordinate space, which is [`PageGeometry::None`]. A stitch
/// maps N pages onto one canvas, a split maps one page onto N canvases, a crop
/// rotates one page and keeps a region of it; in every case the affected
/// documents must additionally be routed and mapped, and the cases are mutually
/// exclusive.
///
/// This enum deliberately exposes NO `stitch()` / `split()` / `crop()`
/// accessor. Such an accessor is exhaustive in exactly one place — its own body
/// — so a new variant added there returns `None` everywhere else and is
/// SILENTLY IGNORED by every planner and remap, which is a data-loss bug the
/// compiler would never point at. Every consumer must therefore destructure
/// this enum with its own exhaustive `match`, so that adding a variant fails to
/// compile at every site that has to reconsider it (CLAUDE.md §17).
#[derive(Debug, Clone, Copy)]
pub(crate) enum PageGeometry<'a> {
    /// No pixel remapping: pages keep their own coordinate spaces.
    None,
    /// Several pages merged onto one canvas.
    Stitch(&'a StitchGeometry),
    /// One page cut into several parts.
    Split(&'a SplitGeometry),
    /// One page rotated and cropped in place.
    Crop(&'a CropGeometry),
}

// ---------------------------------------------------------------------------
// Permutation math.
// ---------------------------------------------------------------------------

/// Rotation the executor applies to a compose source BEFORE cropping it.
///
/// Mirrors the [`PageRotation`](crate::crop_geometry::PageRotation) the placement was built with, in the raw form a
/// journal can carry (the geometry types are `pub(crate)` and not
/// serializable). The executor re-validates it through
/// [`PageRotation::new`](crate::crop_geometry::PageRotation::new) / [`RotatedPage::new`], so a corrupt journal cannot
/// reach the pixel loop with an out-of-range angle.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct ComposeRotation {
    /// Clockwise 90-degree steps, `0..=3`.
    pub quarter_turns: u8,
    /// Fine straightening angle in degrees, clockwise-positive, in `(-45, 45)`.
    pub angle_deg: f64,
}

/// One source image of a composed page: which file, how it is rotated, which
/// part of it, and where it lands. Paths are title-relative and point at the
/// file's ORIGINAL location, because composing happens in phase A, before any
/// rename.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ComposeSource {
    /// Title-relative path of the image to read.
    pub path: String,
    /// Pixel size the rotation is applied to (the source PAGE's size). A
    /// decoded image of a different size is resized to it first — page-sized
    /// overlays may have been attached with a same-aspect resize.
    pub page_size: [u32; 2],
    /// Rotation applied to the `page_size` image before cropping, or `None`
    /// when the source is used upright. `crop` is then read in the ROTATED
    /// CANVAS' pixels rather than in `page_size`'s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<ComposeRotation>,
    /// Resample this source with NEAREST NEIGHBOUR instead of a smooth filter,
    /// at EVERY step that can resample it: the page-size pre-resize, the
    /// rotation, and the crop-to-destination resize.
    ///
    /// TRUE FOR EVERY MASK, ON EVERY OPERATION — a stitch, a split and a crop
    /// alike. Both mask families this engine moves are strictly binary by
    /// construction: the typing mask is painted by
    /// `MaskBrush::paint_binary_mask_segment` and re-thresholded at luma 128 on
    /// load (`tabs/typing/mask.rs`), and a detection mask is normalized to
    /// `0`/`255` by its one owner, `ms_text_detect::mask` (`normalize_binary_alpha` /
    /// `binary_alpha_from_gray`), which promotes ANY non-zero byte to fully
    /// masked. A smooth filter therefore does not merely blur such a raster, it
    /// changes what it MEANS: every interpolated pixel of a detection mask reads
    /// as masked, so the mask silently GROWS outwards, and a typing mask's edge
    /// shifts to wherever the interpolation crosses 128.
    ///
    /// The flag lives here rather than on [`ComposeRotation`] because the
    /// property belongs to the raster, not to one step. That is what lets one
    /// answer cover all three: a stitch and a split never rotate, but they still
    /// reach the page-size pre-resize whenever a mask file is not page-sized,
    /// and that step must not smooth it either.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nearest: bool,
    /// `[x, y, w, h]` region of the source image, in `page_size` pixels — or in
    /// the rotated canvas' pixels when `rotation` is `Some`.
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
    /// Parsed `layers/layers` manifest (either format), when present.
    pub layers_manifest: Option<Value>,
    /// The file format `layers_manifest` was read from (meaningful only when
    /// it is `Some`); its phase-B rewrite targets that same file.
    pub layers_manifest_format: OwnedDocFormat,
    /// Every file name in `text_images/`.
    pub text_images_files: BTreeSet<String>,
    /// Parsed `text_info.json` files found in this tree.
    pub text_info: Vec<TextInfoFile>,
    /// Parsed bubbles document entries (either format), when it exists.
    pub bubbles: Option<Vec<Value>>,
    /// The file format `bubbles` was read from (meaningful only when it is
    /// `Some`); its phase-B rewrite targets that same file.
    pub bubbles_format: OwnedDocFormat,
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

/// On-disk format of an OWNED chapter document (`layers/layers`, the bubbles
/// document) as the scan found it. A serializable mirror of
/// `ms_docstore::DocFormat`, which carries no serde and is not a journal type.
/// The default (`Json`) is what a test-built snapshot without a stored document
/// means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) enum OwnedDocFormat {
    /// `<stem>.json`.
    #[default]
    #[serde(rename = "json")]
    Json,
    /// `<stem>.db` (`SQLite` fragment store of `ms_docstore`).
    #[serde(rename = "db")]
    Db,
}

impl OwnedDocFormat {
    /// The scan-side conversion from the store's format.
    #[must_use]
    pub(crate) fn from_docstore(format: ms_docstore::DocFormat) -> Self {
        match format {
            ms_docstore::DocFormat::Json => Self::Json,
            ms_docstore::DocFormat::Db => Self::Db,
        }
    }

    /// File extension without the dot (`json` / `db`).
    #[must_use]
    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Db => "db",
        }
    }
}

/// Which owned chapter document a `.db` write materializes (recorded as the
/// `.db` file's `meta.doc_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum OwnedDocKind {
    /// `layers/layers.db`.
    #[serde(rename = "layers")]
    Layers,
    /// The bubbles document (`translation_bubbles.db`).
    #[serde(rename = "bubbles")]
    Bubbles,
}

impl OwnedDocKind {
    /// The store's kind of this document.
    #[must_use]
    pub(crate) fn to_docstore(self) -> ms_docstore::DocKind {
        match self {
            Self::Layers => ms_docstore::DocKind::Layers,
            Self::Bubbles => ms_docstore::DocKind::Bubbles,
        }
    }

    /// Extension-less file name of the document inside its directory
    /// (`layers` inside `layers/`, `translation_bubbles` inside the tree).
    #[must_use]
    pub(crate) fn file_stem(self) -> &'static str {
        match self {
            Self::Layers => LAYERS_MANIFEST_STEM,
            Self::Bubbles => config::BUBBLES_FILE.strip_suffix(".json").unwrap_or(config::BUBBLES_FILE),
        }
    }
}

/// Extension-less file name of the layer manifest inside `layers/`.
const LAYERS_MANIFEST_STEM: &str = "layers";

/// How phase B materializes a journaled document body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PlannedWriteFormat {
    /// `content` IS the file text; `fs_exec::atomic_write` writes it verbatim.
    /// Every non-owned document (`text_info.json`, detection blocks) and an
    /// owned document the scan found as `.json`.
    #[serde(rename = "json")]
    Json,
    /// `content` is the logical JSON body of an owned document the scan found
    /// as `.db`; `target` is that `.db` file and phase B rebuilds it whole
    /// through `ms_docstore::write_whole_atomic`.
    #[serde(rename = "db")]
    Db {
        /// The document's kind (its `meta.doc_kind`).
        kind: OwnedDocKind,
    },
}

/// One JSON document rewritten by the transaction. `content` is the complete
/// new LOGICAL body (pretty JSON text), computed at plan time and journaled so
/// recovery can re-apply it without re-reading (possibly already-moved)
/// inputs; `format` says whether `target` is a plain JSON file or an owned
/// `.db` document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlannedJsonWrite {
    pub target: String,
    pub format: PlannedWriteFormat,
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
