/*
File: tabs/page_manager/stitch_layout.rs

Purpose:
GUI-free layout core of the "stitch pages" feature: where each selected source
page sits on the new canvas, how a dragged page snaps to the others, the quick
row/column arrangements, and the fit modes that resolve a cross-axis size
mismatch. Contains no egui widget code and performs no I/O, so every rule here
is unit-testable.

Key structures:
- EditPlacement: editing-side placement of one source page (mirrors the engine's
  `page_ops::StitchPlacement` plus the source page's own pixel size).
- PlacedRect / WorldRect / BoundingBox / CanvasSize: integer canvas geometry and
  the float rect used for drag math.
- SnapResult / SnapGuide: the snapped offset plus the guides that fired.
- CrossAlign / FitMode / LayoutKind: arrangement and fit vocabulary.

Key functions:
- normalize(): shifts the layout to a (0,0) origin and returns the canvas size.
- snap_drag(): adjacency + alignment snapping of a dragged rect, per axis.
- arrange_row() / arrange_column(): quick arrangements in page-index order.
- layout_kind() / apply_fit(): fit-mode availability gate and application.

Notes:
The coordinate contract is the affine of `dev-docs/stitch_pages_plan.md`:
`map_point(x, y) = ((x - cx0) * s + dx, (y - cy0) * s + dy)`. Scaling and
cropping are expressed ONLY through the `scale` / `crop` fields — this module
never resamples anything and never decides how pixels are composed; the engine
applies the same affine to pixels and to page-keyed geometry.
*/

// The three bounds below are the ENGINE's, taken from `page_ops` rather than
// restated: a layout this module accepts must be one `PageOpKind::Stitch`
// validation also accepts, and two independent copies of the numbers would let
// the dialog enable a confirm the engine then refuses.
/// Maximum side of the stitched canvas, in pixels.
pub(super) const MAX_CANVAS_SIDE_PX: u32 = crate::page_ops::STITCH_MAX_SIDE_PX;
/// Maximum stitched canvas area, in pixels.
pub(super) const MAX_CANVAS_PIXELS: u64 = crate::page_ops::STITCH_MAX_TOTAL_PX;
/// Upper bound of a placement's uniform scale (the engine accepts `(0, 16]`).
pub(super) const MAX_PLACEMENT_SCALE: f32 = crate::page_ops::STITCH_MAX_SCALE;

/// Why a layout operation could not produce a valid result.
///
/// Every variant is a refusal, never a silently corrected value: the UI turns it
/// into a disabled confirm button or a localized message, and the engine would
/// reject the same layout with its own validation.
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub(super) enum StitchLayoutError {
    /// The placement list is empty (a stitch needs at least two pages).
    #[error("the stitch layout has no placements")]
    Empty,
    /// The bounding box does not fit the engine's canvas budget.
    #[error(
        "the stitched canvas is {width}x{height} px, over the {MAX_CANVAS_SIDE_PX} px side or {MAX_CANVAS_PIXELS} px area budget"
    )]
    CanvasTooLarge { width: i64, height: i64 },
    /// A non-`Fill` fit mode was requested for a layout that is neither a pure
    /// row nor a pure column.
    #[error("this fit mode is only available for a pure row or a pure column layout")]
    FitNotAvailable,
    /// The fit would need a scale outside the engine's `(0, MAX_PLACEMENT_SCALE]` range.
    #[error("page {page_idx} would need scale {scale}, outside (0, {MAX_PLACEMENT_SCALE}]")]
    ScaleOutOfRange { page_idx: usize, scale: f32 },
    /// The page has a zero-sized source image or crop, or its scale rounds the
    /// placed size to zero on one axis (which the engine also refuses).
    #[error("page {page_idx} has a zero-sized source image, crop, or placed size")]
    DegeneratePage { page_idx: usize },
}

/// Engine-shaped placement fields: `(page_idx, crop, scale, dx, dy)`.
///
/// The tuple is deliberately structural instead of an import of
/// `page_ops::StitchPlacement`: the layout core owns no engine type (only its
/// numeric bounds, see `MAX_CANVAS_SIDE_PX`), and the UI maps this tuple into
/// the engine struct at request time.
pub(super) type EnginePlacementFields = (usize, [u32; 4], f32, i64, i64);

/// Editing-side placement of one source page on the stitch canvas.
///
/// Carries the same data as the engine's `page_ops::StitchPlacement` plus
/// `page_size`, the source page's own pixel dimensions, which the editor needs
/// to reset crops and to reason about cross-axis extents.
///
/// Invariants maintained by this module: `crop` stays inside `page_size`,
/// `crop` width/height stay non-zero, and `scale` stays in `(0, MAX_PLACEMENT_SCALE]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct EditPlacement {
    /// Index of the source page in the CURRENT chapter order.
    pub page_idx: usize,
    /// The source page's own pixel size `[w, h]`.
    pub page_size: [u32; 2],
    /// Rect inside the source page's own image: `[x, y, w, h]` in that page's px.
    pub crop: [u32; 4],
    /// Uniform scale applied to the cropped rect; `1.0` leaves pixels untouched.
    pub scale: f32,
    /// X of the placed image's top-left corner, in new-canvas px.
    pub dx: i64,
    /// Y of the placed image's top-left corner, in new-canvas px.
    pub dy: i64,
}

impl EditPlacement {
    /// Creates an untouched placement of `page_idx`: full crop, scale `1.0`, origin `(0, 0)`.
    #[must_use]
    pub fn new(page_idx: usize, page_size: [u32; 2]) -> Self {
        Self {
            page_idx,
            page_size,
            crop: [0, 0, page_size[0], page_size[1]],
            scale: 1.0,
            dx: 0,
            dy: 0,
        }
    }

    /// Returns the engine-shaped field tuple for this placement.
    #[must_use]
    pub fn engine_fields(&self) -> EnginePlacementFields {
        (self.page_idx, self.crop, self.scale, self.dx, self.dy)
    }

    /// Resets the pixel-selecting fields: full crop, scale `1.0`. Position is kept.
    pub fn reset_pixels(&mut self) {
        self.crop = [0, 0, self.page_size[0], self.page_size[1]];
        self.scale = 1.0;
    }

    /// Size of the placed image on the canvas, `[w, h]` in new-canvas px.
    ///
    /// Follows the plan's affine: `round(crop_w * scale)`. A product beyond
    /// `u32::MAX` saturates — such a placement always fails [`normalize`]'s
    /// canvas budget, so saturation fails closed instead of wrapping.
    #[must_use]
    pub fn placed_size(&self) -> [u32; 2] {
        [
            scaled_len(self.crop[2], self.scale),
            scaled_len(self.crop[3], self.scale),
        ]
    }

    /// The placed image's rect on the canvas.
    #[must_use]
    pub fn rect(&self) -> PlacedRect {
        let [w, h] = self.placed_size();
        PlacedRect {
            x: self.dx,
            y: self.dy,
            w,
            h,
        }
    }
}

/// Scales a pixel length by `scale` and rounds to the nearest pixel.
///
/// Non-finite, negative, or overflowing results saturate into `[0, u32::MAX]`;
/// callers treat a saturated length as an invalid layout, never as a valid size.
fn scaled_len(len: u32, scale: f32) -> u32 {
    let value = (f64::from(len) * f64::from(scale)).round();
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    if value >= f64::from(u32::MAX) {
        return u32::MAX;
    }
    // Guarded above: `value` is finite and inside `[0, u32::MAX)`, so the cast is exact.
    value as u32
}

/// Axis-aligned rect of a placed page, in whole canvas pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PlacedRect {
    pub x: i64,
    pub y: i64,
    pub w: u32,
    pub h: u32,
}

impl PlacedRect {
    /// X of the right edge (exclusive).
    #[must_use]
    pub fn right(&self) -> i64 {
        self.x + i64::from(self.w)
    }

    /// Y of the bottom edge (exclusive).
    #[must_use]
    pub fn bottom(&self) -> i64 {
        self.y + i64::from(self.h)
    }

    /// Converts to the float rect used by drag/snap math.
    ///
    /// The conversion is exact: canvas coordinates stay far inside the ±2^53
    /// range where `f64` represents every integer exactly.
    #[must_use]
    pub fn to_world(self) -> WorldRect {
        WorldRect {
            min_x: self.x as f64,
            min_y: self.y as f64,
            max_x: self.right() as f64,
            max_y: self.bottom() as f64,
        }
    }
}

/// Axis-aligned rect in canvas (world) pixels, as a float so a drag in progress
/// can be expressed before it is committed to whole pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct WorldRect {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl WorldRect {
    /// Builds a rect from its top-left corner and its size.
    #[must_use]
    pub fn from_min_size(min_x: f64, min_y: f64, width: f64, height: f64) -> Self {
        Self {
            min_x,
            min_y,
            max_x: min_x + width,
            max_y: min_y + height,
        }
    }

    /// Width of the rect, in world px.
    #[must_use]
    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    /// Height of the rect, in world px.
    #[must_use]
    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    /// X of the rect's centre.
    #[must_use]
    pub fn center_x(&self) -> f64 {
        f64::midpoint(self.min_x, self.max_x)
    }

    /// Y of the rect's centre.
    #[must_use]
    pub fn center_y(&self) -> f64 {
        f64::midpoint(self.min_y, self.max_y)
    }

    /// Returns the rect moved by `offset` (`[dx, dy]`, world px).
    #[must_use]
    pub fn translated(&self, offset: [f64; 2]) -> Self {
        Self {
            min_x: self.min_x + offset[0],
            min_y: self.min_y + offset[1],
            max_x: self.max_x + offset[0],
            max_y: self.max_y + offset[1],
        }
    }
}

/// Bounding box of a whole layout, in canvas px. `max_*` are exclusive edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BoundingBox {
    pub min_x: i64,
    pub min_y: i64,
    pub max_x: i64,
    pub max_y: i64,
}

/// Pixel size of the stitched canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CanvasSize {
    pub width: u32,
    pub height: u32,
}

/// Bounding box of every placed rect, or `None` for an empty layout.
#[must_use]
pub(super) fn bounding_box(placements: &[EditPlacement]) -> Option<BoundingBox> {
    let mut result: Option<BoundingBox> = None;
    for placement in placements {
        let rect = placement.rect();
        result = Some(match result {
            None => BoundingBox {
                min_x: rect.x,
                min_y: rect.y,
                max_x: rect.right(),
                max_y: rect.bottom(),
            },
            Some(bbox) => BoundingBox {
                min_x: bbox.min_x.min(rect.x),
                min_y: bbox.min_y.min(rect.y),
                max_x: bbox.max_x.max(rect.right()),
                max_y: bbox.max_y.max(rect.bottom()),
            },
        });
    }
    result
}

/// Shifts the layout so its bounding box starts at `(0, 0)` and returns the
/// resulting canvas size.
///
/// Post-condition on success: every placed rect lies inside
/// `[0, width] x [0, height]`.
///
/// # Errors
/// [`StitchLayoutError::Empty`] when there are no placements,
/// [`StitchLayoutError::DegeneratePage`] when a placement's placed size rounds
/// to zero on either axis (the engine refuses such a placement, so the dialog
/// must refuse it first), and [`StitchLayoutError::CanvasTooLarge`] when the
/// bounding box exceeds the engine's side/area budget.
pub(super) fn normalize(placements: &mut [EditPlacement]) -> Result<CanvasSize, StitchLayoutError> {
    // Checked before anything is shifted, so a refused layout is returned
    // untouched rather than half-normalized.
    for placement in placements.iter() {
        let [w, h] = placement.placed_size();
        if w == 0 || h == 0 {
            return Err(StitchLayoutError::DegeneratePage {
                page_idx: placement.page_idx,
            });
        }
    }
    let bbox = bounding_box(placements).ok_or(StitchLayoutError::Empty)?;
    for placement in placements.iter_mut() {
        placement.dx -= bbox.min_x;
        placement.dy -= bbox.min_y;
    }
    let width = bbox.max_x - bbox.min_x;
    let height = bbox.max_y - bbox.min_y;
    let too_large = |side: i64| side <= 0 || side > i64::from(MAX_CANVAS_SIDE_PX);
    if too_large(width) || too_large(height) {
        return Err(StitchLayoutError::CanvasTooLarge { width, height });
    }
    // Both sides are positive and bounded by MAX_CANVAS_SIDE_PX here, so neither the
    // conversion nor the area product can overflow.
    let (width, height) = (
        u32::try_from(width).map_err(|_| StitchLayoutError::CanvasTooLarge { width, height })?,
        u32::try_from(height).map_err(|_| StitchLayoutError::CanvasTooLarge { width, height })?,
    );
    if u64::from(width) * u64::from(height) > MAX_CANVAS_PIXELS {
        return Err(StitchLayoutError::CanvasTooLarge {
            width: i64::from(width),
            height: i64::from(height),
        });
    }
    Ok(CanvasSize { width, height })
}

/// Which axis a snap guide constrains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SnapAxis {
    X,
    Y,
}

/// What made a guide fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SnapKind {
    /// Edge-to-edge contact (right→left, left→right, bottom→top, top→bottom):
    /// the placement that makes two pages read as one seamless image.
    Adjacency,
    /// Shared min / centre / max line (left-left, centre-centre, …).
    Alignment,
}

/// One guide that fired during a drag, so the window can draw it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SnapGuide {
    pub axis: SnapAxis,
    pub kind: SnapKind,
    /// Index into the `others` slice passed to [`snap_drag`].
    pub other: usize,
    /// World coordinate of the guide line (an X for [`SnapAxis::X`], a Y otherwise).
    pub position: f64,
}

/// Outcome of snapping a dragged rect: the offset to apply plus the guides.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SnapResult {
    /// `[dx, dy]` to add to the dragged rect, world px. Zero on an axis that did not snap.
    pub offset: [f64; 2],
    /// The guide that fired on the X axis, if any.
    pub x: Option<SnapGuide>,
    /// The guide that fired on the Y axis, if any.
    pub y: Option<SnapGuide>,
}

impl SnapResult {
    /// Applies [`Self::offset`] to `candidate`.
    #[must_use]
    pub fn snapped_rect(&self, candidate: WorldRect) -> WorldRect {
        candidate.translated(self.offset)
    }
}

/// Snaps a dragged rect to the other pages' edges and alignment lines.
///
/// `candidate` is the dragged page's rect at the raw pointer position, `others`
/// the rects of every OTHER page (the dragged one must be excluded, or it would
/// snap to itself), and `threshold` the snap radius in WORLD px — the caller
/// divides its screen-px radius by the current zoom so the feel stays constant.
///
/// Both families are considered: adjacency (the dragged edge meets an opposite
/// edge) and alignment (min / centre / max lines). The two axes are snapped
/// independently; on each axis the smallest |delta| within `threshold` wins and
/// ties resolve deterministically towards the earlier candidate in the fixed
/// order (per `others` entry: adjacency first, then min / centre / max).
///
/// A non-finite or non-positive `threshold` disables snapping.
#[must_use]
pub(super) fn snap_drag(
    candidate: WorldRect,
    others: &[WorldRect],
    threshold: f64,
) -> SnapResult {
    if !threshold.is_finite() || threshold <= 0.0 {
        return SnapResult {
            offset: [0.0, 0.0],
            x: None,
            y: None,
        };
    }
    let x = best_guide(candidate, others, threshold, SnapAxis::X);
    let y = best_guide(candidate, others, threshold, SnapAxis::Y);
    SnapResult {
        offset: [
            x.map_or(0.0, |(_, delta)| delta),
            y.map_or(0.0, |(_, delta)| delta),
        ],
        x: x.map(|(guide, _)| guide),
        y: y.map(|(guide, _)| guide),
    }
}

/// Returns the winning guide of one axis together with the offset it implies.
///
/// Candidates are enumerated in a fixed order and compared with a strict `<`, so
/// an exact tie keeps the earlier candidate (adjacency before alignment, and
/// `others` in caller order) — snapping is therefore reproducible frame to frame.
fn best_guide(
    candidate: WorldRect,
    others: &[WorldRect],
    threshold: f64,
    axis: SnapAxis,
) -> Option<(SnapGuide, f64)> {
    let (cand_min, cand_max, cand_center) = match axis {
        SnapAxis::X => (candidate.min_x, candidate.max_x, candidate.center_x()),
        SnapAxis::Y => (candidate.min_y, candidate.max_y, candidate.center_y()),
    };
    let mut best: Option<(SnapGuide, f64)> = None;
    for (index, other) in others.iter().enumerate() {
        let (other_min, other_max, other_center) = match axis {
            SnapAxis::X => (other.min_x, other.max_x, other.center_x()),
            SnapAxis::Y => (other.min_y, other.max_y, other.center_y()),
        };
        // Fixed candidate order: the two adjacency contacts, then the three alignment lines.
        let candidates = [
            (SnapKind::Adjacency, other_max, other_max - cand_min),
            (SnapKind::Adjacency, other_min, other_min - cand_max),
            (SnapKind::Alignment, other_min, other_min - cand_min),
            (SnapKind::Alignment, other_center, other_center - cand_center),
            (SnapKind::Alignment, other_max, other_max - cand_max),
        ];
        for (kind, position, delta) in candidates {
            if !delta.is_finite() || delta.abs() > threshold {
                continue;
            }
            let better = match best {
                None => true,
                Some((_, best_delta)) => delta.abs() < best_delta.abs(),
            };
            if better {
                best = Some((
                    SnapGuide {
                        axis,
                        kind,
                        other: index,
                        position,
                    },
                    delta,
                ));
            }
        }
    }
    best
}

/// Cross-axis alignment of a quick arrangement (the axis the pages are NOT
/// stacked along): top/left, centre, or bottom/right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CrossAlign {
    Start,
    Center,
    End,
}

/// The axis pages are stacked along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainAxis {
    /// Left to right — a row.
    Horizontal,
    /// Top to bottom — a column.
    Vertical,
}

/// Packs the pages into one contiguous row, in ascending page-index order.
///
/// The result is already normalized: the bounding box starts at `(0, 0)` and the
/// pages touch edge to edge, `align` deciding where a shorter page sits inside
/// the row's height. Crops and scales are left untouched — use [`apply_fit`] to
/// resolve a height mismatch.
pub(super) fn arrange_row(placements: &mut [EditPlacement], align: CrossAlign) {
    let order = order_by_page_idx(placements);
    pack(placements, &order, MainAxis::Horizontal, align);
}

/// Packs the pages into one contiguous column, in ascending page-index order.
///
/// Mirror of [`arrange_row`]: `align` decides where a narrower page sits inside
/// the column's width.
pub(super) fn arrange_column(placements: &mut [EditPlacement], align: CrossAlign) {
    let order = order_by_page_idx(placements);
    pack(placements, &order, MainAxis::Vertical, align);
}

/// Indices into `placements`, sorted by ascending source page index.
fn order_by_page_idx(placements: &[EditPlacement]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..placements.len()).collect();
    order.sort_by_key(|&index| placements[index].page_idx);
    order
}

/// Indices into `placements`, sorted by their current position along `axis`
/// (page index breaking ties), i.e. the order the user currently SEES.
fn order_by_position(placements: &[EditPlacement], axis: MainAxis) -> Vec<usize> {
    let mut order: Vec<usize> = (0..placements.len()).collect();
    order.sort_by_key(|&index| {
        let placement = &placements[index];
        let main = match axis {
            MainAxis::Horizontal => placement.dx,
            MainAxis::Vertical => placement.dy,
        };
        (main, placement.page_idx)
    });
    order
}

/// Lays the pages out contiguously along `axis` in the given `order`, aligning
/// the cross axis inside the largest cross extent. Positions only — crops and
/// scales are not touched. The bounding box ends up at `(0, 0)`.
fn pack(
    placements: &mut [EditPlacement],
    order: &[usize],
    axis: MainAxis,
    align: CrossAlign,
) {
    let cross_max = order
        .iter()
        .map(|&index| {
            let [w, h] = placements[index].placed_size();
            match axis {
                MainAxis::Horizontal => h,
                MainAxis::Vertical => w,
            }
        })
        .max()
        .unwrap_or(0);
    let mut cursor: i64 = 0;
    for &index in order {
        let [w, h] = placements[index].placed_size();
        let (main_len, cross_len) = match axis {
            MainAxis::Horizontal => (w, h),
            MainAxis::Vertical => (h, w),
        };
        let free = i64::from(cross_max) - i64::from(cross_len);
        let cross = match align {
            CrossAlign::Start => 0,
            // `free` is non-negative (cross_max is the maximum), so the truncating
            // division is a floor and keeps the page inside the band.
            CrossAlign::Center => free / 2,
            CrossAlign::End => free,
        };
        match axis {
            MainAxis::Horizontal => {
                placements[index].dx = cursor;
                placements[index].dy = cross;
            }
            MainAxis::Vertical => {
                placements[index].dx = cross;
                placements[index].dy = cursor;
            }
        }
        cursor += i64::from(main_len);
    }
}

/// Shape of the current layout, which decides whether the non-`Fill` fit modes
/// are offered at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LayoutKind {
    /// Pages are side by side: no horizontal overlap and a shared horizontal band.
    PureRow,
    /// Pages are stacked: no vertical overlap and a shared vertical band.
    PureColumn,
    /// Anything else (an L shape, overlapping pages, a single page).
    Free,
}

/// Classifies the layout.
///
/// A pure row means the placed rects do not overlap horizontally (touching is
/// allowed) and every rect shares a common horizontal band; a pure column is the
/// mirror image. The two are mutually exclusive, and fewer than two placements
/// are always [`LayoutKind::Free`] — there is no mismatch to fit.
#[must_use]
pub(super) fn layout_kind(placements: &[EditPlacement]) -> LayoutKind {
    if placements.len() < 2 {
        return LayoutKind::Free;
    }
    let rects: Vec<PlacedRect> = placements.iter().map(EditPlacement::rect).collect();
    if rects.iter().any(|rect| rect.w == 0 || rect.h == 0) {
        return LayoutKind::Free;
    }
    if disjoint_along(&rects, MainAxis::Horizontal) && shares_band(&rects, MainAxis::Vertical) {
        return LayoutKind::PureRow;
    }
    if disjoint_along(&rects, MainAxis::Vertical) && shares_band(&rects, MainAxis::Horizontal) {
        return LayoutKind::PureColumn;
    }
    LayoutKind::Free
}

/// Whether the rects' intervals along `axis` never overlap (touching allowed).
fn disjoint_along(rects: &[PlacedRect], axis: MainAxis) -> bool {
    let mut spans: Vec<(i64, i64)> = rects
        .iter()
        .map(|rect| match axis {
            MainAxis::Horizontal => (rect.x, rect.right()),
            MainAxis::Vertical => (rect.y, rect.bottom()),
        })
        .collect();
    spans.sort_unstable();
    spans
        .windows(2)
        .all(|pair| matches!(pair, [(_, prev_end), (next_start, _)] if prev_end <= next_start))
}

/// Whether every rect's interval along `axis` overlaps a common band.
fn shares_band(rects: &[PlacedRect], axis: MainAxis) -> bool {
    let mut band_start = i64::MIN;
    let mut band_end = i64::MAX;
    for rect in rects {
        let (start, end) = match axis {
            MainAxis::Horizontal => (rect.x, rect.right()),
            MainAxis::Vertical => (rect.y, rect.bottom()),
        };
        band_start = band_start.max(start);
        band_end = band_end.min(end);
    }
    band_start < band_end
}

/// How a cross-axis size mismatch is resolved when the pages are joined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FitMode {
    /// Nothing is resampled or cut; the leftover space stays background. Default,
    /// and the only mode offered for a [`LayoutKind::Free`] layout.
    Fill,
    /// Every page is scaled down to the smallest cross extent.
    ScaleToSmaller,
    /// Every page is scaled up to the largest cross extent.
    ScaleToLarger,
    /// Every page is cropped down to the smallest cross extent.
    Crop,
}

/// Applies a fit mode to the layout.
///
/// The pixel-selecting fields are recomputed from scratch (full crop, scale
/// `1.0`) before the mode is applied, so switching between modes never compounds
/// earlier scales or crops. For a pure row/column the pages are then re-packed
/// contiguously in their CURRENT visual order (not page-index order — the user's
/// arrangement is preserved), with `align` deciding the cross-axis position;
/// [`FitMode::Crop`] keeps the band that `align` points at, so the kept strip is
/// the one the user already sees. A [`LayoutKind::Free`] layout keeps its
/// positions and accepts [`FitMode::Fill`] only.
///
/// Scaling and cropping are expressed ONLY through `scale` / `crop`: this
/// function never resamples anything.
///
/// # Errors
/// [`StitchLayoutError::FitNotAvailable`] for a non-`Fill` mode on a free
/// layout, [`StitchLayoutError::DegeneratePage`] for a zero-sized page or for a
/// fit that would round a placed size to zero, and
/// [`StitchLayoutError::ScaleOutOfRange`] when the needed scale leaves the
/// engine's `(0, MAX_PLACEMENT_SCALE]` range. On error the placements are left
/// with their crops and scales reset and their positions unchanged.
pub(super) fn apply_fit(
    placements: &mut [EditPlacement],
    mode: FitMode,
    align: CrossAlign,
) -> Result<(), StitchLayoutError> {
    let kind = layout_kind(placements);
    for placement in placements.iter_mut() {
        if placement.page_size[0] == 0 || placement.page_size[1] == 0 {
            return Err(StitchLayoutError::DegeneratePage {
                page_idx: placement.page_idx,
            });
        }
        placement.reset_pixels();
    }
    let axis = match kind {
        LayoutKind::PureRow => MainAxis::Horizontal,
        LayoutKind::PureColumn => MainAxis::Vertical,
        LayoutKind::Free => {
            return match mode {
                // Free + Fill is the documented fallback: pixels untouched, positions kept.
                FitMode::Fill => Ok(()),
                FitMode::ScaleToSmaller | FitMode::ScaleToLarger | FitMode::Crop => {
                    Err(StitchLayoutError::FitNotAvailable)
                }
            };
        }
    };
    // Cross extent of a page after the reset above: its own height in a row, width in a column.
    let cross_of = |placement: &EditPlacement| match axis {
        MainAxis::Horizontal => placement.page_size[1],
        MainAxis::Vertical => placement.page_size[0],
    };
    let extents: Vec<u32> = placements.iter().map(cross_of).collect();
    let smallest = extents.iter().copied().min().unwrap_or(0);
    let largest = extents.iter().copied().max().unwrap_or(0);
    match mode {
        FitMode::Fill => {}
        FitMode::ScaleToSmaller => scale_cross_to(placements, axis, smallest)?,
        FitMode::ScaleToLarger => scale_cross_to(placements, axis, largest)?,
        FitMode::Crop => crop_cross_to(placements, axis, smallest, align)?,
    }
    let order = order_by_position(placements, axis);
    pack(placements, &order, axis, align);
    Ok(())
}

/// Sets every placement's `scale` so its cross extent becomes `target` px.
///
/// # Errors
/// [`StitchLayoutError::DegeneratePage`] for a zero cross extent, or when the
/// resulting scale rounds the placed size to zero on either axis (a page far
/// narrower than the target band); [`StitchLayoutError::ScaleOutOfRange`] when
/// the ratio leaves `(0, MAX_PLACEMENT_SCALE]`.
fn scale_cross_to(
    placements: &mut [EditPlacement],
    axis: MainAxis,
    target: u32,
) -> Result<(), StitchLayoutError> {
    for placement in placements.iter_mut() {
        let cross = match axis {
            MainAxis::Horizontal => placement.crop[3],
            MainAxis::Vertical => placement.crop[2],
        };
        if cross == 0 || target == 0 {
            return Err(StitchLayoutError::DegeneratePage {
                page_idx: placement.page_idx,
            });
        }
        // f32 matches the engine's field type; both operands are exact small integers.
        let scale = (f64::from(target) / f64::from(cross)) as f32;
        if !scale.is_finite() || scale <= 0.0 || scale > MAX_PLACEMENT_SCALE {
            return Err(StitchLayoutError::ScaleOutOfRange {
                page_idx: placement.page_idx,
                scale,
            });
        }
        placement.scale = scale;
        // A valid scale can still round the OTHER axis away entirely (a 4 px wide
        // page scaled by 0.05). The engine rejects such a placement, so refusing
        // here is what keeps the dialog's confirm honest.
        let [placed_w, placed_h] = placement.placed_size();
        if placed_w == 0 || placed_h == 0 {
            return Err(StitchLayoutError::DegeneratePage {
                page_idx: placement.page_idx,
            });
        }
    }
    Ok(())
}

/// Trims every placement's crop along the cross axis to `target` px, keeping the
/// band `align` points at (start / centre / end of the page).
///
/// # Errors
/// [`StitchLayoutError::DegeneratePage`] when `target` is zero or larger than a
/// page's cross extent (which cannot happen for the minimum extent, but is
/// rejected rather than clamped).
fn crop_cross_to(
    placements: &mut [EditPlacement],
    axis: MainAxis,
    target: u32,
    align: CrossAlign,
) -> Result<(), StitchLayoutError> {
    for placement in placements.iter_mut() {
        let cross = match axis {
            MainAxis::Horizontal => placement.crop[3],
            MainAxis::Vertical => placement.crop[2],
        };
        if target == 0 || target > cross {
            return Err(StitchLayoutError::DegeneratePage {
                page_idx: placement.page_idx,
            });
        }
        let free = cross - target;
        let offset = match align {
            CrossAlign::Start => 0,
            CrossAlign::Center => free / 2,
            CrossAlign::End => free,
        };
        match axis {
            MainAxis::Horizontal => {
                placement.crop[1] = offset;
                placement.crop[3] = target;
            }
            MainAxis::Vertical => {
                placement.crop[0] = offset;
                placement.crop[2] = target;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a placement of `page_idx` sized `w x h`, positioned at `(dx, dy)`.
    fn placement(page_idx: usize, w: u32, h: u32, dx: i64, dy: i64) -> EditPlacement {
        let mut placement = EditPlacement::new(page_idx, [w, h]);
        placement.dx = dx;
        placement.dy = dy;
        placement
    }

    fn rects(placements: &[EditPlacement]) -> Vec<(i64, i64, u32, u32)> {
        placements
            .iter()
            .map(|placement| {
                let rect = placement.rect();
                (rect.x, rect.y, rect.w, rect.h)
            })
            .collect()
    }

    #[test]
    fn engine_fields_mirror_the_placement() {
        let mut item = placement(3, 100, 200, -5, 7);
        item.crop = [1, 2, 90, 190];
        item.scale = 0.5;
        assert_eq!(item.engine_fields(), (3, [1, 2, 90, 190], 0.5, -5, 7));
        assert_eq!(item.placed_size(), [45, 95]);
    }

    #[test]
    fn bounding_box_spans_every_placed_rect() {
        let items = [placement(0, 100, 50, -20, 10), placement(1, 40, 80, 30, -5)];
        let bbox = bounding_box(&items).expect("two placements have a bounding box");
        assert_eq!(
            bbox,
            BoundingBox {
                min_x: -20,
                min_y: -5,
                max_x: 80,
                max_y: 75,
            }
        );
        assert!(bounding_box(&[]).is_none());
    }

    #[test]
    fn normalize_moves_the_layout_into_the_canvas() {
        let mut items = [placement(0, 100, 50, -20, 10), placement(1, 40, 80, 30, -5)];
        let canvas = normalize(&mut items).expect("layout fits the canvas budget");
        assert_eq!(
            canvas,
            CanvasSize {
                width: 100,
                height: 80
            }
        );
        assert_eq!(rects(&items), vec![(0, 15, 100, 50), (50, 0, 40, 80)]);
        for item in &items {
            let rect = item.rect();
            assert!(rect.x >= 0 && rect.y >= 0);
            assert!(rect.right() <= i64::from(canvas.width));
            assert!(rect.bottom() <= i64::from(canvas.height));
        }
    }

    #[test]
    fn normalize_rejects_an_empty_layout_and_an_oversized_canvas() {
        assert_eq!(normalize(&mut []), Err(StitchLayoutError::Empty));
        let mut items = [
            placement(0, MAX_CANVAS_SIDE_PX, 10, 0, 0),
            placement(1, 10, 10, i64::from(MAX_CANVAS_SIDE_PX), 0),
        ];
        assert!(matches!(
            normalize(&mut items),
            Err(StitchLayoutError::CanvasTooLarge { .. })
        ));
        // 20 000 x 20 000 = 400 MPx: inside the side budget, over the area budget.
        let mut area = [
            placement(0, 20_000, 20_000, 0, 0),
            placement(1, 10, 10, 0, 0),
        ];
        assert!(matches!(
            normalize(&mut area),
            Err(StitchLayoutError::CanvasTooLarge { .. })
        ));
    }

    #[test]
    fn snap_adjacency_joins_the_dragged_left_edge_to_a_right_edge() {
        let other = WorldRect::from_min_size(0.0, 0.0, 100.0, 200.0);
        // Dragged page hovering 3 px right of the seam and 2 px below the top line.
        let candidate = WorldRect::from_min_size(103.0, 2.0, 50.0, 200.0);
        let result = snap_drag(candidate, &[other], 5.0);
        assert!((result.offset[0] - (-3.0)).abs() < 1e-9);
        assert!((result.offset[1] - (-2.0)).abs() < 1e-9);
        let guide = result.x.expect("the x axis snapped");
        assert_eq!(guide.kind, SnapKind::Adjacency);
        assert_eq!(guide.axis, SnapAxis::X);
        assert_eq!(guide.other, 0);
        assert!((guide.position - 100.0).abs() < 1e-9);
        let snapped = result.snapped_rect(candidate);
        assert!((snapped.min_x - 100.0).abs() < 1e-9);
        assert!((snapped.min_y - 0.0).abs() < 1e-9);
    }

    #[test]
    fn snap_adjacency_joins_the_dragged_top_edge_to_a_bottom_edge() {
        let other = WorldRect::from_min_size(0.0, 0.0, 100.0, 200.0);
        let candidate = WorldRect::from_min_size(0.0, 196.0, 100.0, 60.0);
        let result = snap_drag(candidate, &[other], 5.0);
        let guide = result.y.expect("the y axis snapped");
        assert_eq!(guide.kind, SnapKind::Adjacency);
        assert_eq!(guide.axis, SnapAxis::Y);
        assert!((guide.position - 200.0).abs() < 1e-9);
        assert!((result.offset[1] - 4.0).abs() < 1e-9);
    }

    #[test]
    fn snap_alignment_catches_the_centre_line() {
        let other = WorldRect::from_min_size(0.0, 0.0, 100.0, 200.0);
        // Far away on x (no snap), centre-aligned within 2 px on y.
        let candidate = WorldRect::from_min_size(400.0, 48.0, 60.0, 100.0);
        let result = snap_drag(candidate, &[other], 4.0);
        assert!(result.x.is_none());
        assert!((result.offset[0] - 0.0).abs() < 1e-9);
        let guide = result.y.expect("the centre line snapped");
        assert_eq!(guide.kind, SnapKind::Alignment);
        assert!((guide.position - 100.0).abs() < 1e-9);
        assert!((result.offset[1] - 2.0).abs() < 1e-9);
    }

    #[test]
    fn snap_prefers_the_closest_candidate_and_ignores_far_ones() {
        let others = [
            WorldRect::from_min_size(0.0, 0.0, 100.0, 100.0),
            WorldRect::from_min_size(300.0, 0.0, 100.0, 100.0),
        ];
        // 4 px past the first page's right edge, 6 px before the second page's left edge.
        // Far below both on y, so only the x family can fire.
        let candidate = WorldRect::from_min_size(104.0, 500.0, 50.0, 100.0);
        let result = snap_drag(candidate, &others, 8.0);
        let guide = result.x.expect("the nearer seam wins");
        assert_eq!(guide.other, 0);
        assert!((result.offset[0] - (-4.0)).abs() < 1e-9);
        // Out of range: nothing snaps and the offset stays zero.
        let far = snap_drag(candidate, &others, 1.0);
        assert_eq!(far.offset, [0.0, 0.0]);
        assert!(far.x.is_none() && far.y.is_none());
    }

    #[test]
    fn arrange_row_packs_in_page_order_with_each_alignment() {
        // Deliberately out of page order in the slice: page 1 comes first visually.
        let base = [placement(1, 40, 60, 500, 500), placement(0, 30, 100, 0, 0)];
        let mut start = base;
        arrange_row(&mut start, CrossAlign::Start);
        assert_eq!(rects(&start), vec![(30, 0, 40, 60), (0, 0, 30, 100)]);
        let mut center = base;
        arrange_row(&mut center, CrossAlign::Center);
        assert_eq!(rects(&center), vec![(30, 20, 40, 60), (0, 0, 30, 100)]);
        let mut end = base;
        arrange_row(&mut end, CrossAlign::End);
        assert_eq!(rects(&end), vec![(30, 40, 40, 60), (0, 0, 30, 100)]);
        assert_eq!(layout_kind(&center), LayoutKind::PureRow);
    }

    #[test]
    fn arrange_column_packs_in_page_order_with_each_alignment() {
        let base = [placement(1, 60, 40, -300, 900), placement(0, 100, 30, 0, 0)];
        let mut start = base;
        arrange_column(&mut start, CrossAlign::Start);
        assert_eq!(rects(&start), vec![(0, 30, 60, 40), (0, 0, 100, 30)]);
        let mut center = base;
        arrange_column(&mut center, CrossAlign::Center);
        assert_eq!(rects(&center), vec![(20, 30, 60, 40), (0, 0, 100, 30)]);
        let mut end = base;
        arrange_column(&mut end, CrossAlign::End);
        assert_eq!(rects(&end), vec![(40, 30, 60, 40), (0, 0, 100, 30)]);
        assert_eq!(layout_kind(&end), LayoutKind::PureColumn);
    }

    #[test]
    fn layout_kind_classifies_row_column_and_free() {
        let row = [placement(0, 100, 200, 0, 0), placement(1, 100, 150, 100, 25)];
        assert_eq!(layout_kind(&row), LayoutKind::PureRow);
        let column = [placement(0, 100, 200, 0, 0), placement(1, 80, 150, 10, 200)];
        assert_eq!(layout_kind(&column), LayoutKind::PureColumn);
        // L shape: page 2 sits beside page 0, page 1 sits above it.
        let l_shape = [
            placement(0, 100, 100, 0, 100),
            placement(1, 100, 100, 0, 0),
            placement(2, 100, 100, 100, 100),
        ];
        assert_eq!(layout_kind(&l_shape), LayoutKind::Free);
        // Overlapping pages are neither.
        let overlap = [placement(0, 100, 100, 0, 0), placement(1, 100, 100, 50, 50)];
        assert_eq!(layout_kind(&overlap), LayoutKind::Free);
        // A single page has no mismatch to fit.
        assert_eq!(layout_kind(&[placement(0, 10, 10, 0, 0)]), LayoutKind::Free);
    }

    #[test]
    fn fit_fill_leaves_pixels_untouched_and_repacks_the_row() {
        let mut items = [placement(0, 100, 200, 0, 0), placement(1, 100, 100, 140, 30)];
        apply_fit(&mut items, FitMode::Fill, CrossAlign::Center)
            .expect("fill is always available for a row");
        assert!(items.iter().all(|item| item.scale == 1.0));
        assert_eq!(items[0].crop, [0, 0, 100, 200]);
        assert_eq!(items[1].crop, [0, 0, 100, 100]);
        assert_eq!(rects(&items), vec![(0, 0, 100, 200), (100, 50, 100, 100)]);
    }

    #[test]
    fn fit_scale_modes_equalize_the_cross_axis_of_a_row() {
        let mut smaller = [placement(0, 100, 200, 0, 0), placement(1, 100, 100, 140, 0)];
        apply_fit(&mut smaller, FitMode::ScaleToSmaller, CrossAlign::Start)
            .expect("a row accepts scale fits");
        assert!((smaller[0].scale - 0.5).abs() < 1e-6);
        assert!((smaller[1].scale - 1.0).abs() < 1e-6);
        assert_eq!(rects(&smaller), vec![(0, 0, 50, 100), (50, 0, 100, 100)]);

        let mut larger = [placement(0, 100, 200, 0, 0), placement(1, 100, 100, 140, 0)];
        apply_fit(&mut larger, FitMode::ScaleToLarger, CrossAlign::Start)
            .expect("a row accepts scale fits");
        assert!((larger[0].scale - 1.0).abs() < 1e-6);
        assert!((larger[1].scale - 2.0).abs() < 1e-6);
        assert_eq!(rects(&larger), vec![(0, 0, 100, 200), (100, 0, 200, 200)]);
    }

    #[test]
    fn fit_crop_keeps_the_band_the_alignment_points_at() {
        let base = [placement(0, 100, 200, 0, 0), placement(1, 100, 100, 140, 0)];
        let mut start = base;
        apply_fit(&mut start, FitMode::Crop, CrossAlign::Start).expect("a row accepts a crop fit");
        assert_eq!(start[0].crop, [0, 0, 100, 100]);
        assert_eq!(start[1].crop, [0, 0, 100, 100]);
        let mut center = base;
        apply_fit(&mut center, FitMode::Crop, CrossAlign::Center).expect("a row accepts a crop fit");
        assert_eq!(center[0].crop, [0, 50, 100, 100]);
        let mut end = base;
        apply_fit(&mut end, FitMode::Crop, CrossAlign::End).expect("a row accepts a crop fit");
        assert_eq!(end[0].crop, [0, 100, 100, 100]);
        assert_eq!(rects(&end), vec![(0, 0, 100, 100), (100, 0, 100, 100)]);
        assert!(end.iter().all(|item| item.scale == 1.0));
    }

    #[test]
    fn fit_crop_trims_the_width_of_a_column() {
        let mut items = [placement(0, 200, 100, 0, 0), placement(1, 100, 100, 0, 140)];
        apply_fit(&mut items, FitMode::Crop, CrossAlign::Center)
            .expect("a column accepts a crop fit");
        assert_eq!(items[0].crop, [50, 0, 100, 100]);
        assert_eq!(items[1].crop, [0, 0, 100, 100]);
        assert_eq!(rects(&items), vec![(0, 0, 100, 100), (0, 100, 100, 100)]);
    }

    #[test]
    fn a_free_layout_offers_only_fill() {
        let l_shape = [
            placement(0, 100, 100, 0, 100),
            placement(1, 100, 100, 0, 0),
            placement(2, 100, 100, 100, 100),
        ];
        for mode in [FitMode::ScaleToSmaller, FitMode::ScaleToLarger, FitMode::Crop] {
            let mut items = l_shape;
            assert_eq!(
                apply_fit(&mut items, mode, CrossAlign::Center),
                Err(StitchLayoutError::FitNotAvailable)
            );
        }
        let mut items = l_shape;
        apply_fit(&mut items, FitMode::Fill, CrossAlign::Center)
            .expect("fill is available everywhere");
        // Positions of a free layout are kept as the user arranged them.
        assert_eq!(rects(&items), rects(&l_shape));
    }

    #[test]
    fn fit_rejects_a_scale_beyond_the_engine_range() {
        // 1 px against 100 px would need scale 100, far over MAX_PLACEMENT_SCALE.
        let mut items = [placement(0, 10, 1, 0, 0), placement(1, 10, 100, 20, 0)];
        assert!(matches!(
            apply_fit(&mut items, FitMode::ScaleToLarger, CrossAlign::Start),
            Err(StitchLayoutError::ScaleOutOfRange { page_idx: 0, .. })
        ));
    }

    #[test]
    fn fit_rejects_a_zero_sized_page() {
        let mut items = [placement(0, 0, 0, 0, 0), placement(1, 10, 10, 20, 0)];
        assert_eq!(
            apply_fit(&mut items, FitMode::Fill, CrossAlign::Start),
            Err(StitchLayoutError::DegeneratePage { page_idx: 0 })
        );
    }

    #[test]
    fn fit_rejects_a_scale_that_rounds_a_placed_size_to_zero() {
        // A 4x2000 sliver next to a 300x100 page: fitting to the smaller cross
        // extent needs scale 100/2000 = 0.05, and round(4 * 0.05) = 0 px wide.
        // The engine refuses such a placement, so the layout core must too —
        // otherwise the confirm stays enabled and the op fails after the dialog
        // has already closed.
        let mut items = [placement(0, 4, 2000, 0, 0), placement(1, 300, 100, 10, 0)];
        assert_eq!(
            apply_fit(&mut items, FitMode::ScaleToSmaller, CrossAlign::Start),
            Err(StitchLayoutError::DegeneratePage { page_idx: 0 })
        );
    }

    #[test]
    fn normalize_rejects_a_placement_that_rounds_to_nothing() {
        let mut items = [placement(7, 4, 2000, 0, 0), placement(1, 300, 100, 10, 0)];
        items[0].scale = 0.05;
        assert_eq!(
            normalize(&mut items),
            Err(StitchLayoutError::DegeneratePage { page_idx: 7 })
        );
        // Refused before anything moved: the caller's layout is left untouched.
        assert_eq!((items[1].dx, items[1].dy), (10, 0));
    }
}
