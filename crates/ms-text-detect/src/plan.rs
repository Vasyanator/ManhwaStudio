/*
File: crates/ms-text-detect/src/plan.rs

Purpose:
The single owner of the detection scale and tiling decision per engine: scaled size, tile
input size, tile grid with guaranteed overlap, padding and resize filter. Both the panel notice
and the detection worker call `plan_detection`, so what the panel announces is what runs.

Key items:
- `EngineKind`, `DetectParams` (+ the CTD detect-size constants and `effective_ctd_detect_size`).
- `plan_detection()` -> `DetectionPlan` (private fields, accessors) or `PlanError`.
- `TileRect`: the valid (unpadded) area of one tile in SCALED pixels.
- `PlanNotice` / `PlanMode`: everything the panel shows (mode, scale %, grid, tile count, tile
  and CTD sizes) without recomputing anything.

Engine table (`S` model tile side, `O` minimum overlap in scaled px, `align` input multiple):
| engine | S                                | align | stride/ch | single pass                          | tiled scale          | O              | pad   | filter   |
| Classic| no tiling, long side <= 1600     | 1     | - / -     | always; `min(1, 1600/long)` in f32   | -                    | 0              | black | Triangle |
| Ctd    | snap64(clamp(size, 896, 2048))   | 64    | 1 / 2     | `r = min(S/W, S/H) >= 0.5` (upscale) | `clamp(S/W, 0.5, 1)` | max(S/6, 128)  | black | Triangle |
| Paddle | 960                              | 32    | 1 / 1     | `r = min(1, 960/long) >= 0.4`        | `clamp(960/W, 0.4,1)`| 160            | black | Triangle |
| Surya  | 1200                             | 4     | 4 / 1     | `H <= 1400`: 1200x1200               | `1200/W` x 1 (rows)  | 200            | white | Lanczos3 |

Notes:
Tiles cover the scaled page completely; adjacent tiles overlap by at least `O` (see
`axis_positions`). A single tile on an axis whose scaled length is below the tile side gets an
input of `align_up(length)` and is padded; all tiles of a plan share one input size. Plans of
pages above `MAX_MASK_PIXELS` (source or scaled) are refused.
*/

use crate::mask::{MAX_MASK_PIXELS, exceeds_pixel_limit};
use crate::num::{f32_to_i32_trunc, f64_round_to_u32, u32_to_f32};

/// Smallest CTD detection size (model tile side) the plan accepts; smaller requests are raised.
pub const CTD_DETECT_SIZE_MIN: u32 = 896;
/// Largest CTD detection size; larger requests are lowered.
pub const CTD_DETECT_SIZE_MAX: u32 = 2048;
/// Default CTD detection size of a new project.
pub const CTD_DETECT_SIZE_DEFAULT: u32 = 1280;
/// The CTD network needs inputs that are multiples of this on both axes.
const CTD_ALIGN: u32 = 64;

/// Longest working side of the classic detector (mirrors the translation tab's classic path).
const CLASSIC_MAX_DIM: u32 = 1600;
/// Paddle detection tile side and the long side of its single pass.
const PADDLE_TILE: u32 = 960;
/// Smallest single-pass scale Paddle accepts before tiling instead.
const PADDLE_MIN_SINGLE_SCALE: f64 = 0.4;
/// Smallest single-pass scale CTD accepts before tiling instead.
const CTD_MIN_SINGLE_SCALE: f64 = 0.5;
/// Surya model side (the library resizes every chunk to this square).
const SURYA_TILE: u32 = 1200;
/// Tallest page Surya still processes as one squeezed 1200x1200 pass (library parity).
const SURYA_SINGLE_MAX_HEIGHT: u32 = 1400;

/// The detection engines that share the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    /// The classic (non-AI) detector: only downscales, never tiles, has no runner.
    Classic,
    /// comic-text-detector: two channels (`seg`, `shrink`).
    Ctd,
    /// Paddle (PP-OCR) DB detection: one probability channel.
    Paddle,
    /// Surya detection: one heatmap channel at a quarter of the input resolution.
    Surya,
}

/// The user-facing detection parameters that influence the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectParams {
    /// Requested CTD detection size (model tile side). Clamped to
    /// [`CTD_DETECT_SIZE_MIN`]`..=`[`CTD_DETECT_SIZE_MAX`] and snapped down to a multiple of 64
    /// inside the plan; [`effective_ctd_detect_size`] tells what will be used.
    pub ctd_detect_size: u32,
}

impl Default for DetectParams {
    fn default() -> Self {
        Self { ctd_detect_size: CTD_DETECT_SIZE_DEFAULT }
    }
}

/// The CTD tile side actually used for a requested detection size: clamped to
/// `896..=2048`, then snapped DOWN to a multiple of 64 (both bounds are multiples of 64, so the
/// result stays inside the range).
#[must_use]
pub fn effective_ctd_detect_size(requested: u32) -> u32 {
    let clamped = requested.clamp(CTD_DETECT_SIZE_MIN, CTD_DETECT_SIZE_MAX);
    clamped - clamped % CTD_ALIGN
}

/// Resampling filter the pipeline uses to bring the page to the scaled size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeFilter {
    /// Bilinear (`image` `Triangle`), the cv2 `INTER_LINEAR` stand-in.
    Triangle,
    /// Lanczos with a = 3, the PIL `LANCZOS` stand-in (Surya).
    Lanczos3,
}

/// The valid (unpadded) area of one tile, in SCALED pixels. The tile input image is this area
/// copied to its top-left corner and padded to the plan's `tile_input`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRect {
    /// Left edge in scaled pixels.
    pub x: u32,
    /// Top edge in scaled pixels.
    pub y: u32,
    /// Width in scaled pixels (`<= tile_input[0]`).
    pub w: u32,
    /// Height in scaled pixels (`<= tile_input[1]`).
    pub h: u32,
}

/// What the plan does to the page, as the panel shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanMode {
    /// One pass at the source resolution.
    FullResolution,
    /// One pass at a different resolution (a downscale, or the CTD letterbox upscale).
    Resized,
    /// Several tiles at the source resolution.
    Tiled,
    /// Several tiles at a different resolution.
    ResizedAndTiled,
}

/// Everything the detector panel announces for a page, precomputed by the plan so the panel
/// only formats it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanNotice {
    /// Resize / tiling summary.
    pub mode: PlanMode,
    /// Effective scale per axis `[x, y]` in percent, rounded (`scaled / source * 100`). The two
    /// differ only for Surya, which squeezes pages anisotropically like its library.
    pub scale_percent: [u32; 2],
    /// Tile columns.
    pub cols: u32,
    /// Tile rows.
    pub rows: u32,
    /// Total tiles (`cols * rows`).
    pub tiles: u32,
    /// Model input size of every tile `[w, h]` in scaled pixels (padding included).
    pub tile_size: [u32; 2],
    /// Page size after scaling `[w, h]`.
    pub scaled_size: [u32; 2],
    /// The effective CTD detection size (snapped tile side); `None` for other engines.
    pub ctd_detect_size: Option<u32>,
}

/// Why a plan could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// The page has zero width or height.
    #[error("detection page is empty ({width}x{height})")]
    EmptyImage {
        /// Page width in pixels.
        width: u32,
        /// Page height in pixels.
        height: u32,
    },
    /// The source or the scaled page exceeds the pixel limit.
    #[error("detection page {width}x{height} exceeds the {max}-pixel limit")]
    TooLarge {
        /// Width of the offending image (source or scaled) in pixels.
        width: u32,
        /// Height of the offending image in pixels.
        height: u32,
        /// The limit, [`MAX_MASK_PIXELS`].
        max: usize,
    },
}

/// A complete detection plan for one engine and page size. Built only by [`plan_detection`],
/// so its invariants hold: tiles lie inside `scaled_size`, cover it completely, overlap their
/// neighbours by at least `overlap_min`, and `w <= tile_input[0]`, `h <= tile_input[1]`.
#[derive(Debug, Clone, PartialEq)]
pub struct DetectionPlan {
    engine: EngineKind,
    source_size: [u32; 2],
    scale: [f64; 2],
    scaled_size: [u32; 2],
    tile_input: [u32; 2],
    grid: [u32; 2],
    overlap_min: u32,
    tiles: Vec<TileRect>,
    pad_rgb: [u8; 3],
    filter: ResizeFilter,
    notice: PlanNotice,
}

impl DetectionPlan {
    /// The engine this plan is for.
    #[must_use]
    pub fn engine(&self) -> EngineKind {
        self.engine
    }

    /// Source page size `[w, h]` in pixels.
    #[must_use]
    pub fn source_size(&self) -> [u32; 2] {
        self.source_size
    }

    /// Effective scale per axis `[x, y]` = `scaled_size / source_size`.
    #[must_use]
    pub fn scale(&self) -> [f64; 2] {
        self.scale
    }

    /// Page size after the one resize `[w, h]`; tiles and stitched maps live in this space.
    #[must_use]
    pub fn scaled_size(&self) -> [u32; 2] {
        self.scaled_size
    }

    /// Model input size of every tile `[w, h]` (a multiple of the engine alignment).
    #[must_use]
    pub fn tile_input(&self) -> [u32; 2] {
        self.tile_input
    }

    /// Tile grid `[cols, rows]`.
    #[must_use]
    pub fn grid(&self) -> [u32; 2] {
        self.grid
    }

    /// Guaranteed minimum overlap between adjacent tiles, in scaled pixels (0 for one tile).
    #[must_use]
    pub fn overlap_min(&self) -> u32 {
        self.overlap_min
    }

    /// The tiles, row-major (`rows` outer, `cols` inner).
    #[must_use]
    pub fn tiles(&self) -> &[TileRect] {
        &self.tiles
    }

    /// Padding colour of the tile area outside the page.
    #[must_use]
    pub fn pad_rgb(&self) -> [u8; 3] {
        self.pad_rgb
    }

    /// Resize filter of the one page resize.
    #[must_use]
    pub fn filter(&self) -> ResizeFilter {
        self.filter
    }

    /// The panel summary of this plan.
    #[must_use]
    pub fn notice(&self) -> PlanNotice {
        self.notice
    }

    /// The tile's valid area mapped back to source pixels `[x1, y1, x2, y2]` (unclamped f64).
    #[must_use]
    pub fn source_rect(&self, tile: &TileRect) -> [f64; 4] {
        let [sx, sy] = self.scale;
        [
            f64::from(tile.x) / sx,
            f64::from(tile.y) / sy,
            f64::from(tile.x) / sx + f64::from(tile.w) / sx,
            f64::from(tile.y) / sy + f64::from(tile.h) / sy,
        ]
    }

    /// Ratio between a tile input and the maps the model returns for it (Surya: 4, else 1).
    #[must_use]
    pub fn map_stride(&self) -> u32 {
        match self.engine {
            EngineKind::Classic | EngineKind::Ctd | EngineKind::Paddle => 1,
            EngineKind::Surya => 4,
        }
    }

    /// Probability channels a runner returns per tile (Classic has no runner: 0).
    #[must_use]
    pub fn channels(&self) -> usize {
        match self.engine {
            EngineKind::Classic => 0,
            EngineKind::Paddle | EngineKind::Surya => 1,
            EngineKind::Ctd => 2,
        }
    }

    /// Expected size `[w, h]` of every map a runner returns (`tile_input / map_stride`; exact,
    /// because the alignment is a multiple of the stride).
    #[must_use]
    pub fn map_size(&self) -> [u32; 2] {
        let stride = self.map_stride();
        [self.tile_input[0] / stride, self.tile_input[1] / stride]
    }
}

/// Per-engine constants of the tiled path.
struct TileSpec {
    /// Model tile side `S` (a multiple of `align`).
    tile: u32,
    /// Input alignment.
    align: u32,
    /// Minimum overlap `O` (`< tile`).
    overlap: u32,
    /// Padding colour.
    pad: [u8; 3],
    /// Resize filter.
    filter: ResizeFilter,
}

/// Plans detection of a `source_size` page (`[w, h]`, pixels) with `engine`.
///
/// # Errors
/// [`PlanError::EmptyImage`] for a zero-area page; [`PlanError::TooLarge`] when the source or
/// the scaled page has more than [`MAX_MASK_PIXELS`] pixels.
pub fn plan_detection(engine: EngineKind, source_size: [u32; 2], params: &DetectParams) -> Result<DetectionPlan, PlanError> {
    let [width, height] = source_size;
    if width == 0 || height == 0 {
        return Err(PlanError::EmptyImage { width, height });
    }
    guard_pixels(width, height)?;
    match engine {
        EngineKind::Classic => plan_classic(source_size),
        EngineKind::Ctd => {
            let side = effective_ctd_detect_size(params.ctd_detect_size);
            let spec = TileSpec { tile: side, align: CTD_ALIGN, overlap: (side / 6).max(128), pad: [0, 0, 0], filter: ResizeFilter::Triangle };
            let w = f64::from(width);
            let h = f64::from(height);
            let fit = (f64::from(side) / w).min(f64::from(side) / h);
            let scaled = if fit >= CTD_MIN_SINGLE_SCALE {
                // Single pass, today's letterbox scale (upscale included); clamp guards a
                // rounding overshoot of the long side.
                scaled_size(source_size, fit)?.map(|v| v.min(side))
            } else {
                let s = (f64::from(side) / w).clamp(CTD_MIN_SINGLE_SCALE, 1.0);
                scaled_size(source_size, s)?
            };
            plan_tiled(engine, source_size, scaled, &spec, Some(side))
        }
        EngineKind::Paddle => {
            let spec = TileSpec { tile: PADDLE_TILE, align: 32, overlap: 160, pad: [0, 0, 0], filter: ResizeFilter::Triangle };
            let w = f64::from(width);
            let fit = (f64::from(PADDLE_TILE) / f64::from(width.max(height))).min(1.0);
            let scaled = if fit >= PADDLE_MIN_SINGLE_SCALE {
                scaled_size(source_size, fit)?.map(|v| v.min(PADDLE_TILE))
            } else {
                let s = (f64::from(PADDLE_TILE) / w).clamp(PADDLE_MIN_SINGLE_SCALE, 1.0);
                scaled_size(source_size, s)?
            };
            plan_tiled(engine, source_size, scaled, &spec, None)
        }
        EngineKind::Surya => {
            let spec = TileSpec { tile: SURYA_TILE, align: 4, overlap: 200, pad: [255, 255, 255], filter: ResizeFilter::Lanczos3 };
            // Library parity: the width always becomes 1200; a page up to 1400 px tall is squeezed
            // into one 1200x1200 pass, a taller one keeps its height and is cut into rows.
            let scaled_h = if height <= SURYA_SINGLE_MAX_HEIGHT { SURYA_TILE } else { height };
            plan_tiled(engine, source_size, [SURYA_TILE, scaled_h], &spec, None)
        }
    }
}

/// The classic detector's plan: one pass, downscaled so the long side is at most 1600. The
/// arithmetic is f32 exactly like the translation tab's classic path, so the sizes match it.
fn plan_classic(source_size: [u32; 2]) -> Result<DetectionPlan, PlanError> {
    let [width, height] = source_size;
    let long = width.max(height);
    let scaled_size = if long > CLASSIC_MAX_DIM {
        let scale = u32_to_f32(CLASSIC_MAX_DIM) / u32_to_f32(long);
        let dim = |v: u32| u32::try_from(f32_to_i32_trunc((u32_to_f32(v) * scale).round())).unwrap_or(0).max(1);
        [dim(width), dim(height)]
    } else {
        source_size
    };
    let tile = TileRect { x: 0, y: 0, w: scaled_size[0], h: scaled_size[1] };
    let scale = axis_scales(source_size, scaled_size);
    let notice = make_notice(source_size, scaled_size, [1, 1], scaled_size, None)?;
    Ok(DetectionPlan {
        engine: EngineKind::Classic,
        source_size,
        scale,
        scaled_size,
        tile_input: scaled_size,
        grid: [1, 1],
        overlap_min: 0,
        tiles: vec![tile],
        pad_rgb: [0, 0, 0],
        filter: ResizeFilter::Triangle,
        notice,
    })
}

/// Builds the grid for a scaled page with the engine's tile spec.
fn plan_tiled(engine: EngineKind, source_size: [u32; 2], scaled_size: [u32; 2], spec: &TileSpec, ctd_detect_size: Option<u32>) -> Result<DetectionPlan, PlanError> {
    guard_pixels(scaled_size[0], scaled_size[1])?;
    let xs = axis_positions(scaled_size[0], spec.tile, spec.overlap);
    let ys = axis_positions(scaled_size[1], spec.tile, spec.overlap);
    let extent = [scaled_size[0].min(spec.tile), scaled_size[1].min(spec.tile)];
    // An axis with one tile pads its (shorter) length up to the alignment; a tiled axis uses the
    // full tile side, which the engine table keeps aligned.
    let tile_input = [align_up(extent[0], spec.align), align_up(extent[1], spec.align)];
    let tiles: Vec<TileRect> = ys.iter().flat_map(|&y| xs.iter().map(move |&x| TileRect { x, y, w: extent[0], h: extent[1] })).collect();
    let cols = len_u32(xs.len());
    let rows = len_u32(ys.len());
    let overlap_min = if cols > 1 || rows > 1 { spec.overlap } else { 0 };
    let notice = make_notice(source_size, scaled_size, [cols, rows], tile_input, ctd_detect_size)?;
    Ok(DetectionPlan {
        engine,
        source_size,
        scale: axis_scales(source_size, scaled_size),
        scaled_size,
        tile_input,
        grid: [cols, rows],
        overlap_min,
        tiles,
        pad_rgb: spec.pad,
        filter: spec.filter,
        notice,
    })
}

/// Start positions of the tiles along one axis of length `len` (`>= 1`), tile side `tile` and
/// minimum overlap `overlap < tile`.
///
/// `n = 1` when `len <= tile`, else `n = ceil((len - O) / (tile - O))`; `p_i = floor(i * (len -
/// tile) / (n - 1))`, so `p_0 = 0` and the last tile ends flush at `len`. Overlap proof: from the
/// choice of `n`, `n - 1 >= (len - tile) / (tile - O)`, so every step `p_{i+1} - p_i <=
/// ceil((len - tile) / (n - 1)) <= tile - O` (the right side is an integer), i.e. adjacent tiles
/// share at least `O` pixels and together cover `0..len`.
pub(crate) fn axis_positions(len: u32, tile: u32, overlap: u32) -> Vec<u32> {
    if len <= tile {
        return vec![0];
    }
    let step = tile - overlap;
    let n = (len - overlap).div_ceil(step);
    let span = u64::from(len - tile);
    let last = u64::from(n - 1);
    (0..n)
        // `i * span / last <= span = len - tile`, which fits u32.
        .map(|i| u32::try_from(u64::from(i) * span / last).unwrap_or(len - tile))
        .collect()
}

/// `value` rounded up to a multiple of `align` (`>= 1`). Inputs are tile extents bounded by the
/// tile side, so the result never exceeds the (aligned) tile side and cannot overflow.
fn align_up(value: u32, align: u32) -> u32 {
    value.div_ceil(align) * align
}

/// `round(side * scale).max(1)` per axis of `source_size` as a pixel size.
///
/// # Errors
/// [`PlanError::TooLarge`] naming the source size when a scaled side does not fit `u32`.
fn scaled_size(source_size: [u32; 2], scale: f64) -> Result<[u32; 2], PlanError> {
    let [width, height] = source_size;
    let dim = |len: u32| f64_round_to_u32(f64::from(len) * scale).map(|v| v.max(1)).ok_or(PlanError::TooLarge { width, height, max: MAX_MASK_PIXELS });
    Ok([dim(width)?, dim(height)?])
}

/// Refuses an image above the pixel limit.
fn guard_pixels(width: u32, height: u32) -> Result<(), PlanError> {
    if exceeds_pixel_limit(width, height) {
        return Err(PlanError::TooLarge { width, height, max: MAX_MASK_PIXELS });
    }
    Ok(())
}

/// Effective per-axis scale `scaled / source` (both sizes are non-zero here).
fn axis_scales(source_size: [u32; 2], scaled_size: [u32; 2]) -> [f64; 2] {
    [f64::from(scaled_size[0]) / f64::from(source_size[0]), f64::from(scaled_size[1]) / f64::from(source_size[1])]
}

/// A grid dimension as `u32`. Counts are bounded by the page side (`<= u32::MAX`).
fn len_u32(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// Builds the panel notice of a plan.
fn make_notice(source_size: [u32; 2], scaled_size: [u32; 2], grid: [u32; 2], tile_size: [u32; 2], ctd_detect_size: Option<u32>) -> Result<PlanNotice, PlanError> {
    let [sx, sy] = axis_scales(source_size, scaled_size);
    let percent = |scale: f64| f64_round_to_u32(scale * 100.0).ok_or(PlanError::TooLarge { width: scaled_size[0], height: scaled_size[1], max: MAX_MASK_PIXELS });
    let resized = scaled_size != source_size;
    let tiles = grid[0].saturating_mul(grid[1]);
    let mode = match (resized, tiles > 1) {
        (false, false) => PlanMode::FullResolution,
        (true, false) => PlanMode::Resized,
        (false, true) => PlanMode::Tiled,
        (true, true) => PlanMode::ResizedAndTiled,
    };
    Ok(PlanNotice { mode, scale_percent: [percent(sx)?, percent(sy)?], cols: grid[0], rows: grid[1], tiles, tile_size, scaled_size, ctd_detect_size })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(engine: EngineKind, w: u32, h: u32) -> DetectionPlan {
        plan_detection(engine, [w, h], &DetectParams::default()).unwrap_or_else(|err| panic!("{engine:?} {w}x{h}: {err}"))
    }

    /// The old classic formula, verbatim from the translation tab (f32, `as` casts).
    // The casts ARE the reference being pinned; inputs stay far below 2^24.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn classic_reference(w: u32, h: u32) -> [u32; 2] {
        if w.max(h) > 1600 {
            let scale = 1600_f32 / w.max(h) as f32;
            [((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1)]
        } else {
            [w, h]
        }
    }

    const SIZES: [u32; 16] = [1, 7, 63, 300, 700, 800, 959, 960, 961, 1100, 1280, 1401, 2048, 3000, 7777, 20_000];

    /// Checks every structural invariant of a tiled plan along one axis.
    fn check_axis(len: u32, positions: &[u32], extent: u32, input: u32, overlap: u32, axis: &str) {
        assert!(extent <= input, "{axis}: extent {extent} > input {input}");
        assert_eq!(positions[0], 0, "{axis}: first tile at 0");
        let last = positions[positions.len() - 1];
        assert_eq!(last + extent, len, "{axis}: last tile flush to {len}");
        for pair in positions.windows(2) {
            assert!(pair[1] > pair[0], "{axis}: strictly increasing {positions:?}");
            assert!(pair[0] + extent >= pair[1] + overlap, "{axis}: overlap < {overlap} in {positions:?} (extent {extent})");
        }
    }

    #[test]
    fn plans_cover_overlap_align_and_flush_over_a_size_grid() {
        for engine in [EngineKind::Ctd, EngineKind::Paddle, EngineKind::Surya] {
            for &w in &SIZES {
                for &h in &SIZES {
                    if exceeds_pixel_limit(w, h) {
                        continue;
                    }
                    let p = plan(engine, w, h);
                    let [cols, rows] = p.grid();
                    assert_eq!(p.tiles().len(), usize::try_from(cols * rows).unwrap_or(0));
                    let align = match engine {
                        EngineKind::Ctd => 64,
                        EngineKind::Paddle => 32,
                        EngineKind::Surya => 4,
                        EngineKind::Classic => 1,
                    };
                    let side = match engine {
                        EngineKind::Ctd => effective_ctd_detect_size(CTD_DETECT_SIZE_DEFAULT),
                        EngineKind::Paddle => PADDLE_TILE,
                        EngineKind::Surya | EngineKind::Classic => SURYA_TILE,
                    };
                    let input = p.tile_input();
                    assert!(input[0].is_multiple_of(align) && input[1].is_multiple_of(align), "{engine:?} {w}x{h}: input {input:?}");
                    assert!(input[0] <= side && input[1] <= side, "{engine:?} {w}x{h}: input {input:?} > {side}");
                    let xs: Vec<u32> = p.tiles().iter().take(usize::try_from(cols).unwrap_or(0)).map(|t| t.x).collect();
                    let ys: Vec<u32> = p.tiles().iter().step_by(usize::try_from(cols).unwrap_or(1)).map(|t| t.y).collect();
                    let first = p.tiles()[0];
                    let [sw, sh] = p.scaled_size();
                    check_axis(sw, &xs, first.w, input[0], p.overlap_min(), "x");
                    check_axis(sh, &ys, first.h, input[1], p.overlap_min(), "y");
                    for t in p.tiles() {
                        assert!(t.x + t.w <= sw && t.y + t.h <= sh, "{engine:?} {w}x{h}: {t:?} outside {sw}x{sh}");
                    }
                    assert_eq!(p.notice().tiles, cols * rows);
                    assert_eq!(p.map_size(), [input[0] / p.map_stride(), input[1] / p.map_stride()]);
                }
            }
        }
    }

    #[test]
    fn classic_matches_the_translation_tab_formula_and_never_tiles() {
        for &w in &SIZES {
            for &h in &SIZES {
                if exceeds_pixel_limit(w, h) {
                    continue;
                }
                let p = plan(EngineKind::Classic, w, h);
                assert_eq!(p.scaled_size(), classic_reference(w, h), "{w}x{h}");
                assert_eq!(p.grid(), [1, 1]);
                assert_eq!(p.tile_input(), p.scaled_size());
                assert_eq!(p.channels(), 0);
            }
        }
    }

    #[test]
    fn ctd_detect_size_is_clamped_then_snapped_down_to_64() {
        assert_eq!(effective_ctd_detect_size(100), 896);
        assert_eq!(effective_ctd_detect_size(1000), 960);
        assert_eq!(effective_ctd_detect_size(1280), 1280);
        assert_eq!(effective_ctd_detect_size(1343), 1280);
        assert_eq!(effective_ctd_detect_size(9000), 2048);
        let p = plan_detection(EngineKind::Ctd, [800, 800], &DetectParams { ctd_detect_size: 1000 }).unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(p.notice().ctd_detect_size, Some(960));
        assert_eq!(p.tile_input(), [960, 960]);
    }

    #[test]
    fn ctd_single_pass_keeps_the_letterbox_scale_including_upscale() {
        // 800x1000 at S = 1280: r = 1.28 -> one upscaled pass, padded to a multiple of 64.
        let p = plan(EngineKind::Ctd, 800, 1000);
        assert_eq!(p.scaled_size(), [1024, 1280]);
        assert_eq!(p.grid(), [1, 1]);
        assert_eq!(p.tile_input(), [1024, 1280]);
        let n = p.notice();
        assert_eq!(n.mode, PlanMode::Resized);
        assert_eq!(n.scale_percent, [128, 128]);
    }

    #[test]
    fn ctd_webtoon_strip_tiles_at_full_resolution() {
        // 800x12000 at S = 1280: r = 0.107 < 0.5 -> tiled at clamp(1.6, 0.5, 1) = 1.
        let p = plan(EngineKind::Ctd, 800, 12_000);
        assert_eq!(p.scaled_size(), [800, 12_000]);
        assert_eq!(p.tile_input(), [832, 1280]);
        assert_eq!(p.overlap_min(), 213);
        // n = ceil((12000 - 213) / (1280 - 213)) = 12.
        assert_eq!(p.grid(), [1, 12]);
        let n = p.notice();
        assert_eq!(n.mode, PlanMode::Tiled);
        assert_eq!((n.cols, n.rows, n.tiles), (1, 12, 12));
        assert_eq!(n.scale_percent, [100, 100]);
    }

    #[test]
    fn paddle_downscales_a_page_and_tiles_a_strip() {
        let p = plan(EngineKind::Paddle, 1500, 2000);
        assert_eq!(p.scaled_size(), [720, 960]);
        assert_eq!(p.tile_input(), [736, 960]);
        assert_eq!(p.notice().mode, PlanMode::Resized);
        assert_eq!(p.notice().scale_percent, [48, 48]);

        let p = plan(EngineKind::Paddle, 2000, 9000);
        // fit = 960 / 9000 < 0.4 -> tiled at clamp(0.48, 0.4, 1) = 0.48.
        assert_eq!(p.scaled_size(), [960, 4320]);
        assert_eq!(p.notice().mode, PlanMode::ResizedAndTiled);
        assert_eq!(p.grid(), [1, 6]);

        let p = plan(EngineKind::Paddle, 640, 480);
        assert_eq!(p.notice().mode, PlanMode::FullResolution);
        assert_eq!(p.tile_input(), [640, 480]);
    }

    #[test]
    fn surya_squeezes_short_pages_and_cuts_tall_pages_into_rows() {
        let p = plan(EngineKind::Surya, 800, 1300);
        assert_eq!(p.scaled_size(), [1200, 1200]);
        assert_eq!(p.grid(), [1, 1]);
        assert_eq!(p.notice().scale_percent, [150, 92]);
        assert_eq!(p.map_size(), [300, 300]);
        assert_eq!(p.pad_rgb(), [255, 255, 255]);
        assert_eq!(p.filter(), ResizeFilter::Lanczos3);

        let p = plan(EngineKind::Surya, 800, 5000);
        assert_eq!(p.scaled_size(), [1200, 5000]);
        // n = ceil((5000 - 200) / 1000) = 5.
        assert_eq!(p.grid(), [1, 5]);
        assert_eq!(p.notice().scale_percent, [150, 100]);
        assert_eq!(p.notice().mode, PlanMode::ResizedAndTiled);
    }

    #[test]
    fn source_rect_maps_tiles_back_to_source_pixels() {
        let p = plan(EngineKind::Paddle, 1920, 1080);
        let [x1, y1, x2, y2] = p.source_rect(&p.tiles()[0]);
        assert!(x1.abs() < 1e-9 && y1.abs() < 1e-9);
        assert!((x2 - 1920.0).abs() < 1e-9 && (y2 - 1080.0).abs() < 1e-9, "{x2} {y2}");
    }

    #[test]
    fn empty_and_oversized_pages_are_refused() {
        assert_eq!(plan_detection(EngineKind::Ctd, [0, 10], &DetectParams::default()), Err(PlanError::EmptyImage { width: 0, height: 10 }));
        assert_eq!(
            plan_detection(EngineKind::Paddle, [10_001, 10_000], &DetectParams::default()),
            Err(PlanError::TooLarge { width: 10_001, height: 10_000, max: MAX_MASK_PIXELS })
        );
        // A small source whose CTD single pass would upscale stays legal.
        assert!(plan_detection(EngineKind::Ctd, [1, 1], &DetectParams::default()).is_ok());
    }

    #[test]
    fn axis_positions_handles_three_way_overlaps() {
        // len = 2T - O + 1 forces three tiles whose step is below T / 2.
        let positions = axis_positions(2 * 1000 - 200 + 1, 1000, 200);
        assert_eq!(positions, vec![0, 400, 801]);
    }
}
