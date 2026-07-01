/*
File: src/tabs/typing/render_next/vector.rs

Purpose:
Foundational vector-glyph layer for the text-renderer refactor
(`VECTOR_ENGINE_REFACTOR.md`, sections 3.1/3.2/4.1). It extracts true font
outlines (swash), flattens their beziers to polylines, caches them, rasterizes a
transformed outline to a coverage mask (zeno) with the preserved monochrome tint
contract, and derives a `glyph_contour::GlyphContour` from an outline so on-path
distance measurement no longer needs a rasterize-and-retrace round trip.

This layer is consumed by the on-path / formula / custom-line composite pass
(`formula/render.rs`), which rasterizes each glyph's outline instead of blitting
its bitmap. The horizontal (`pipeline.rs`) and vertical (`layout/vertical.rs`)
paths still use the `raster.rs` bitmap blit and are unaffected.

Key structures:
- FillRule: glyph fill winding rule (TrueType/CFF use non-zero).
- Outline: flattened, y-down glyph-local closed subpaths + cached local bbox.
- GlyphTransform: local->world affine, same convention as `PlacedContour`.
- OutlineKey / OutlineCache: resolution-independent outline cache.

Key functions:
- extract_glyph_outline: swash outline -> flattened `Outline`.
- flatten_quad / flatten_cubic: adaptive bezier flattening.
- rasterize_outline_into: the single vector rasterizer (zeno + tint + over-blend).
- glyph_contour_from_outline: `Outline` -> `GlyphContour` for measurement.

Coordinate note (y-flip):
swash/skrifa report outline points in font design space scaled to the requested
ppem, y-UP (baseline at y=0, ascenders positive). `raster.rs` alpha bitmaps and
`glyph_contour.rs` vertices live in y-DOWN top-left pixel space. To keep every
downstream consumer in one frame, `extract_glyph_outline` NEGATES y at extraction
so `Outline`/`GlyphContour` are y-down glyph-local pixels. A global y mirror does
not change which regions a non-zero (or even-odd) fill selects, so winding stays
correct. Parity with swash's own bottom-left-origin bitmap is verified by
`rasterizer_matches_swash_reference` in the tests below.
*/

use super::glyph_contour::{GlyphContour, PlacedContour};
use super::raster::blend_pixel_over;
use super::types::AntiAliasingMode;
use std::collections::HashMap;
use std::sync::Arc;
use zeno::{Command, Fill, Format, Mask, Vector};

/// Default bezier flattening tolerance in glyph-local pixels.
///
/// Sub-pixel (0.2 px) so flattened polylines are visually indistinguishable
/// from the true curve at the extracted em size, keeping zeno-vs-swash AA close.
const DEFAULT_FLATTEN_TOLERANCE_PX: f32 = 0.2;

/// Hard cap on adaptive bezier subdivision depth.
///
/// Bounds recursion for pathological (near-degenerate) control polygons; at
/// depth 16 a single curve yields at most ~65k segments, far beyond any glyph.
const MAX_FLATTEN_DEPTH: u32 = 16;

/// Fill winding rule for glyph outlines.
///
/// TrueType and CFF fonts fill with the non-zero rule; `EvenOdd` is kept for
/// completeness and future non-font paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FillRule {
    /// Non-zero winding (the default for TrueType/CFF glyphs).
    NonZero,
    /// Even-odd winding.
    EvenOdd,
}

impl FillRule {
    /// Map to the zeno fill rule used by the rasterizer.
    #[must_use]
    fn to_zeno(self) -> Fill {
        match self {
            FillRule::NonZero => Fill::NonZero,
            FillRule::EvenOdd => Fill::EvenOdd,
        }
    }
}

/// Flattened glyph outline in glyph-local em-scaled pixels (y-down, origin at
/// the glyph pen/baseline).
///
/// Beziers are pre-flattened to line segments at build time; each subpath is a
/// closed polygon whose closing edge (last -> first vertex) is implicit (the
/// first vertex is not duplicated). `bbox_min`/`bbox_max` bound every vertex in
/// glyph-local space; both are `[0.0, 0.0]` when the outline has no vertices.
#[derive(Debug, Clone)]
pub(crate) struct Outline {
    /// One closed polyline (list of `[x, y]` vertices) per subpath.
    subpaths: Vec<Vec<[f32; 2]>>,
    /// Fill winding rule for all subpaths.
    winding: FillRule,
    /// Minimum corner of the glyph-local vertex AABB.
    bbox_min: [f32; 2],
    /// Maximum corner of the glyph-local vertex AABB.
    bbox_max: [f32; 2],
}

impl Outline {
    /// Build an `Outline` from already-flattened subpaths, computing the AABB.
    ///
    /// `subpaths` are in glyph-local y-down pixels. Empty subpaths are dropped.
    /// Returns `None` when no vertex remains (space/empty glyph).
    #[must_use]
    fn from_subpaths(subpaths: Vec<Vec<[f32; 2]>>, winding: FillRule) -> Option<Outline> {
        let subpaths: Vec<Vec<[f32; 2]>> =
            subpaths.into_iter().filter(|s| !s.is_empty()).collect();
        if subpaths.is_empty() {
            return None;
        }
        let mut min = [f32::INFINITY, f32::INFINITY];
        let mut max = [f32::NEG_INFINITY, f32::NEG_INFINITY];
        for subpath in &subpaths {
            for v in subpath {
                min[0] = min[0].min(v[0]);
                min[1] = min[1].min(v[1]);
                max[0] = max[0].max(v[0]);
                max[1] = max[1].max(v[1]);
            }
        }
        Some(Outline {
            subpaths,
            winding,
            bbox_min: min,
            bbox_max: max,
        })
    }

    /// Glyph-local vertex AABB as `(min, max)`.
    #[must_use]
    pub(crate) fn local_bbox(&self) -> ([f32; 2], [f32; 2]) {
        (self.bbox_min, self.bbox_max)
    }

    /// Closed subpaths (each a polyline; closing edge implicit).
    #[must_use]
    pub(crate) fn subpaths(&self) -> &[Vec<[f32; 2]>] {
        &self.subpaths
    }

    /// Fill winding rule.
    #[must_use]
    pub(crate) fn winding(&self) -> FillRule {
        self.winding
    }
}

/// Local->world affine placement for a glyph outline.
///
/// `world = Rot(rot) * (Scale(sx, sy) * local) + pos`, where
/// `Rot([x, y]) = [x*cos - y*sin, x*sin + y*cos]`. This is the SAME convention
/// as `glyph_contour::PlacedContour`, so a `GlyphTransform` can place both the
/// rasterized outline and the measured contour identically.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GlyphTransform {
    /// World-space translation applied after scale and rotation.
    pub(crate) pos: [f32; 2],
    /// Rotation in radians.
    pub(crate) rot: f32,
    /// Per-axis scale `[sx, sy]` applied before rotation.
    pub(crate) scale: [f32; 2],
}

impl GlyphTransform {
    /// Identity transform (no translation/rotation, unit scale).
    #[must_use]
    pub(crate) fn identity() -> Self {
        Self {
            pos: [0.0, 0.0],
            rot: 0.0,
            scale: [1.0, 1.0],
        }
    }

    /// Transform one glyph-local point to world space.
    #[must_use]
    fn apply(&self, local: [f32; 2], cos: f32, sin: f32) -> [f32; 2] {
        let sx = local[0] * self.scale[0];
        let sy = local[1] * self.scale[1];
        [
            sx * cos - sy * sin + self.pos[0],
            sx * sin + sy * cos + self.pos[1],
        ]
    }

    /// Place a glyph-local `GlyphContour` into world space with this transform.
    ///
    /// Reuses `GlyphContour::placed` so on-path measurement and rasterization
    /// share one placement convention.
    #[must_use]
    pub(crate) fn place_contour(&self, contour: &GlyphContour) -> PlacedContour {
        let (cos, sin) = (self.rot.cos(), self.rot.sin());
        contour.placed(cos, sin, self.scale[0], self.scale[1], self.pos[0], self.pos[1])
    }
}

/// Cache key for an extracted outline.
///
/// Vectors are resolution independent, so no subpixel bin is needed; the em
/// size is keyed by its raw `f32` bit pattern to make identical sizes hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct OutlineKey {
    /// Stable per-font identifier supplied by the caller.
    font_id: u64,
    /// Glyph id within the font.
    glyph_id: u16,
    /// `em_px.to_bits()` — exact-match keying for the extraction ppem.
    em_bits: u32,
}

impl OutlineKey {
    /// Build a key; `em_px` is keyed by its exact bit pattern.
    #[must_use]
    pub(crate) fn new(font_id: u64, glyph_id: u16, em_px: f32) -> Self {
        Self {
            font_id,
            glyph_id,
            em_bits: em_px.to_bits(),
        }
    }
}

/// Extracted-outline cache.
///
/// Stores `Option<Arc<Outline>>` so a glyph known to have no fillable outline
/// (space/empty) is cached as a negative result and not re-extracted.
#[derive(Debug, Default)]
pub(crate) struct OutlineCache {
    map: HashMap<OutlineKey, Option<Arc<Outline>>>,
}

impl OutlineCache {
    /// Empty cache.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// Return the cached outline for `key`, extracting it once on a miss.
    ///
    /// `font`/`glyph_id`/`em_px` must correspond to `key`. Returns `None` (and
    /// caches it) when the glyph has no fillable outline. Never panics.
    pub(crate) fn get_or_extract(
        &mut self,
        key: OutlineKey,
        font: &swash::FontRef,
        glyph_id: u16,
        em_px: f32,
    ) -> Option<Arc<Outline>> {
        if let Some(cached) = self.map.get(&key) {
            return cached.clone();
        }
        let extracted = extract_glyph_outline(font, glyph_id, em_px).map(Arc::new);
        self.map.insert(key, extracted.clone());
        extracted
    }

    /// Number of cached entries (including negative results).
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

/// Extract and flatten a glyph outline at `em_px` pixels-per-em.
///
/// Uses `swash::scale::Scaler::scale_outline`; swash reports points y-up, and
/// this function negates y so the result is y-down glyph-local pixels
/// consistent with `raster.rs`/`glyph_contour.rs`. Quadratics and cubics are
/// flattened to `DEFAULT_FLATTEN_TOLERANCE_PX`.
///
/// Returns `None` for a glyph with no fillable outline (space/empty), a
/// non-finite or non-positive `em_px`, or a color glyph (handled elsewhere).
/// Never panics on a missing/invalid glyph id.
#[must_use]
pub(crate) fn extract_glyph_outline(
    font: &swash::FontRef,
    glyph_id: u16,
    em_px: f32,
) -> Option<Outline> {
    if !em_px.is_finite() || em_px <= 0.0 {
        return None;
    }
    let mut context = swash::scale::ScaleContext::new();
    let mut scaler = context.builder(*font).size(em_px).hint(false).build();
    let outline = scaler.scale_outline(glyph_id)?;
    // Color glyphs have no monochrome fill contract; the bitmap path owns them.
    if outline.is_color() {
        return None;
    }

    let points = outline.points();
    let verbs = outline.verbs();
    let tol = DEFAULT_FLATTEN_TOLERANCE_PX;

    let mut subpaths: Vec<Vec<[f32; 2]>> = Vec::new();
    let mut current: Vec<[f32; 2]> = Vec::new();
    // `pen` is the last emitted point in y-down local space; curve flatteners
    // consume it as the start point and never re-emit it.
    let mut pen: [f32; 2] = [0.0, 0.0];
    let mut point_idx = 0usize;

    // Negate y once here (font y-up -> local y-down).
    let flip = |p: zeno::Point| -> [f32; 2] { [p.x, -p.y] };

    for verb in verbs {
        match verb {
            zeno::Verb::MoveTo => {
                let Some(p) = points.get(point_idx).copied() else {
                    break;
                };
                point_idx += 1;
                // Starting a new subpath: flush the previous one.
                if !current.is_empty() {
                    subpaths.push(std::mem::take(&mut current));
                }
                pen = flip(p);
                current.push(pen);
            }
            zeno::Verb::LineTo => {
                let Some(p) = points.get(point_idx).copied() else {
                    break;
                };
                point_idx += 1;
                pen = flip(p);
                current.push(pen);
            }
            zeno::Verb::QuadTo => {
                let (Some(c), Some(p)) =
                    (points.get(point_idx).copied(), points.get(point_idx + 1).copied())
                else {
                    break;
                };
                point_idx += 2;
                let c = flip(c);
                let end = flip(p);
                flatten_quad(pen, c, end, tol, &mut current);
                pen = end;
            }
            zeno::Verb::CurveTo => {
                let (Some(c0), Some(c1), Some(p)) = (
                    points.get(point_idx).copied(),
                    points.get(point_idx + 1).copied(),
                    points.get(point_idx + 2).copied(),
                ) else {
                    break;
                };
                point_idx += 3;
                let c0 = flip(c0);
                let c1 = flip(c1);
                let end = flip(p);
                flatten_cubic(pen, c0, c1, end, tol, &mut current);
                pen = end;
            }
            zeno::Verb::Close => {
                // Subpaths are closed implicitly; just finish the current one.
                if !current.is_empty() {
                    subpaths.push(std::mem::take(&mut current));
                }
            }
        }
    }
    if !current.is_empty() {
        subpaths.push(current);
    }

    // Drop the trailing vertex of any subpath that duplicates its start, so the
    // implicit closing edge is not a zero-length segment.
    for subpath in &mut subpaths {
        if subpath.len() >= 2 {
            let first = subpath[0];
            let last = subpath[subpath.len() - 1];
            if points_close(first, last) {
                subpath.pop();
            }
        }
    }

    // TrueType/CFF glyphs fill non-zero.
    Outline::from_subpaths(subpaths, FillRule::NonZero)
}

/// Whether two points coincide within a tight epsilon.
#[must_use]
fn points_close(a: [f32; 2], b: [f32; 2]) -> bool {
    (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4
}

/// Adaptively flatten a quadratic bezier into `out` (excluding the start point).
///
/// `p0` is the current pen (already in `out`); `p1` is the control point and
/// `p2` the end. Recursively subdivides until the control point's distance to
/// the `p0`-`p2` chord is within `tolerance`, then emits the end point. The
/// start point is never re-pushed, so consecutive segments share no duplicate.
pub(crate) fn flatten_quad(
    p0: [f32; 2],
    p1: [f32; 2],
    p2: [f32; 2],
    tolerance: f32,
    out: &mut Vec<[f32; 2]>,
) {
    flatten_quad_rec(p0, p1, p2, tolerance, 0, out);
    out.push(p2);
}

/// Recursive half of [`flatten_quad`]; does not emit the final end point.
fn flatten_quad_rec(
    p0: [f32; 2],
    p1: [f32; 2],
    p2: [f32; 2],
    tolerance: f32,
    depth: u32,
    out: &mut Vec<[f32; 2]>,
) {
    // Flatness: control-point deviation from the p0->p2 chord.
    if depth >= MAX_FLATTEN_DEPTH || point_line_distance(p1, p0, p2) <= tolerance {
        return;
    }
    let p01 = midpoint(p0, p1);
    let p12 = midpoint(p1, p2);
    let mid = midpoint(p01, p12);
    flatten_quad_rec(p0, p01, mid, tolerance, depth + 1, out);
    out.push(mid);
    flatten_quad_rec(mid, p12, p2, tolerance, depth + 1, out);
}

/// Adaptively flatten a cubic bezier into `out` (excluding the start point).
///
/// `p0` is the current pen (already in `out`); `p1`/`p2` are the control points
/// and `p3` the end. Recursively subdivides until both control points lie within
/// `tolerance` of the `p0`-`p3` chord, then emits the end point.
pub(crate) fn flatten_cubic(
    p0: [f32; 2],
    p1: [f32; 2],
    p2: [f32; 2],
    p3: [f32; 2],
    tolerance: f32,
    out: &mut Vec<[f32; 2]>,
) {
    flatten_cubic_rec(p0, p1, p2, p3, tolerance, 0, out);
    out.push(p3);
}

/// Recursive half of [`flatten_cubic`]; does not emit the final end point.
fn flatten_cubic_rec(
    p0: [f32; 2],
    p1: [f32; 2],
    p2: [f32; 2],
    p3: [f32; 2],
    tolerance: f32,
    depth: u32,
    out: &mut Vec<[f32; 2]>,
) {
    let d1 = point_line_distance(p1, p0, p3);
    let d2 = point_line_distance(p2, p0, p3);
    if depth >= MAX_FLATTEN_DEPTH || d1.max(d2) <= tolerance {
        return;
    }
    // De Casteljau subdivision at t = 0.5.
    let p01 = midpoint(p0, p1);
    let p12 = midpoint(p1, p2);
    let p23 = midpoint(p2, p3);
    let p012 = midpoint(p01, p12);
    let p123 = midpoint(p12, p23);
    let mid = midpoint(p012, p123);
    flatten_cubic_rec(p0, p01, p012, mid, tolerance, depth + 1, out);
    out.push(mid);
    flatten_cubic_rec(mid, p123, p23, p3, tolerance, depth + 1, out);
}

/// Midpoint of two points.
#[must_use]
fn midpoint(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5]
}

/// Perpendicular distance from `p` to the infinite line through `a` and `b`.
///
/// For a zero-length `a`-`b` this degenerates to the distance from `p` to `a`,
/// which is the correct flatness measure for a collapsed control polygon.
#[must_use]
fn point_line_distance(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let abx = b[0] - a[0];
    let aby = b[1] - a[1];
    let len = (abx * abx + aby * aby).sqrt();
    if len <= f32::EPSILON {
        let dx = p[0] - a[0];
        let dy = p[1] - a[1];
        return (dx * dx + dy * dy).sqrt();
    }
    // |cross((p - a), (b - a))| / |b - a|.
    ((p[0] - a[0]) * aby - (p[1] - a[1]) * abx).abs() / len
}

/// Contrast gain applied to coverage around the mid value for `Crisp`.
const AA_CRISP_GAIN: f32 = 1.6;
/// Contrast gain applied to coverage around the mid value for `Sharp`.
const AA_SHARP_GAIN: f32 = 2.6;
/// Contrast gain applied to coverage around the mid value for `Strong`.
const AA_STRONG_GAIN: f32 = 1.4;
/// Additive bias (in coverage fraction) applied by `Strong`; lifts mid values so
/// edges look denser without a hard threshold.
const AA_STRONG_BIAS: f32 = 0.12;

/// Apply a symmetric contrast transfer to a normalized coverage value.
///
/// `c` is coverage in `0.0..=1.0`. The curve pivots around 0.5:
/// `((c - 0.5) * gain + 0.5 + bias)` clamped back to `0.0..=1.0`. A `gain > 1`
/// steepens the edge; `bias > 0` lifts every value (denser ink).
#[must_use]
fn aa_contrast(c: f32, gain: f32, bias: f32) -> f32 {
    ((c - 0.5) * gain + 0.5 + bias).clamp(0.0, 1.0)
}

/// Build the 256-entry coverage->alpha transfer lookup table for an AA mode.
///
/// The input index is a raw zeno coverage byte; the output is the transferred
/// coverage byte fed to the tint multiply. `Smooth` is the exact identity table
/// (`lut[i] == i`), guaranteeing byte-identical output to the pre-AA renderer;
/// `None` is a hard threshold at coverage 0.5 (byte 128). Every table is
/// monotonic non-decreasing with `lut[0] == 0`.
#[must_use]
pub(crate) fn build_aa_lut(mode: AntiAliasingMode) -> [u8; 256] {
    let mut lut = [0u8; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        // `i` is 0..=255, so the truncating cast to u8 is exact.
        let c = i as f32 / 255.0;
        let transferred = match mode {
            // Identity: preserve the exact byte so Smooth is a regression anchor.
            AntiAliasingMode::Smooth => {
                *slot = i as u8;
                continue;
            }
            AntiAliasingMode::Crisp => aa_contrast(c, AA_CRISP_GAIN, 0.0),
            AntiAliasingMode::Sharp => aa_contrast(c, AA_SHARP_GAIN, 0.0),
            AntiAliasingMode::Strong => aa_contrast(c, AA_STRONG_GAIN, AA_STRONG_BIAS),
            AntiAliasingMode::None => {
                if c >= 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
        };
        // Round the transferred fraction back to a coverage byte.
        *slot = (transferred * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    lut
}

/// Rasterize a transformed glyph outline into a straight-alpha RGBA8 canvas.
///
/// Coordinate flow: each glyph-local subpath point is transformed to world
/// coords by `transform` (scale, then rotate, then translate by `pos`), then
/// mapped to a canvas pixel by subtracting `(origin_x, origin_y)` (so a world
/// point at the origin lands at pixel `(0, 0)`, matching how `formula/render.rs`
/// blits with an x/y offset). The transformed polygons are filled to an 8-bit
/// coverage mask with zeno over the integer bounding box of the path (clamped to
/// the canvas), preserving sub-pixel AA (path points are not rounded).
///
/// Anti-aliasing contract: each raw zeno coverage byte is first mapped through
/// `aa_lut` (a coverage->alpha transfer table from `build_aa_lut`) BEFORE the tint
/// multiply. Passing an identity table (`AntiAliasingMode::Smooth`) reproduces the
/// pre-AA coverage byte-for-byte. This applies only to monochrome outline glyphs;
/// the color-glyph bitmap fallback path does not go through this rasterizer.
///
/// Tint contract (doc 4.1, monochrome case): output RGB is replaced by
/// `color[0..3]`; output alpha is `transferred_coverage * color[3] / 255`. Each
/// covered pixel is composited with `raster::blend_pixel_over`.
///
/// `canvas` must be at least `canvas_w * canvas_h * 4` bytes; a shorter buffer
/// is a no-op. Never panics and never indexes out of range for transforms that
/// push the glyph partly or fully off-canvas (such pixels are clipped).
// The rasterizer call site naturally carries canvas target, origin, outline,
// transform, tint and the AA transfer table; splitting them would obscure the mapping.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rasterize_outline_into(
    canvas: &mut [u8],
    canvas_w: usize,
    canvas_h: usize,
    origin_x: f32,
    origin_y: f32,
    outline: &Outline,
    transform: &GlyphTransform,
    color: [u8; 4],
    aa_lut: &[u8; 256],
) {
    if canvas_w == 0 || canvas_h == 0 {
        return;
    }
    let Some(required) = canvas_w
        .checked_mul(canvas_h)
        .and_then(|px| px.checked_mul(4))
    else {
        return;
    };
    if canvas.len() < required {
        return;
    }

    let (cos, sin) = (transform.rot.cos(), transform.rot.sin());

    // Transform every subpath point to canvas space (world - origin) and track
    // the path bounding box in canvas coordinates.
    let mut canvas_subpaths: Vec<Vec<[f32; 2]>> = Vec::with_capacity(outline.subpaths.len());
    let mut min = [f32::INFINITY, f32::INFINITY];
    let mut max = [f32::NEG_INFINITY, f32::NEG_INFINITY];
    for subpath in &outline.subpaths {
        let mut world_pts: Vec<[f32; 2]> = Vec::with_capacity(subpath.len());
        for &local in subpath {
            let world = transform.apply(local, cos, sin);
            let cx = world[0] - origin_x;
            let cy = world[1] - origin_y;
            min[0] = min[0].min(cx);
            min[1] = min[1].min(cy);
            max[0] = max[0].max(cx);
            max[1] = max[1].max(cy);
            world_pts.push([cx, cy]);
        }
        if !world_pts.is_empty() {
            canvas_subpaths.push(world_pts);
        }
    }
    if canvas_subpaths.is_empty() || !min[0].is_finite() {
        return;
    }

    // Integer raster window, clamped to the canvas. Fully off-canvas -> no-op.
    let raw_min_x = min[0].floor();
    let raw_min_y = min[1].floor();
    let raw_max_x = max[0].ceil();
    let raw_max_y = max[1].ceil();
    let win_min_x = clamp_to_range(raw_min_x, 0, canvas_w);
    let win_min_y = clamp_to_range(raw_min_y, 0, canvas_h);
    let win_max_x = clamp_to_range(raw_max_x, 0, canvas_w);
    let win_max_y = clamp_to_range(raw_max_y, 0, canvas_h);
    if win_max_x <= win_min_x || win_max_y <= win_min_y {
        return;
    }
    let mask_w = win_max_x - win_min_x;
    let mask_h = win_max_y - win_min_y;

    // Shift the (full, unclipped) path into the mask-local frame so zeno keeps
    // correct winding/coverage at the window edges while writing only inside it.
    let shift_x = win_min_x as f32;
    let shift_y = win_min_y as f32;
    let mut commands: Vec<Command> = Vec::new();
    for subpath in &canvas_subpaths {
        for (i, p) in subpath.iter().enumerate() {
            let pt = Vector::new(p[0] - shift_x, p[1] - shift_y);
            if i == 0 {
                commands.push(Command::MoveTo(pt));
            } else {
                commands.push(Command::LineTo(pt));
            }
        }
        commands.push(Command::Close);
    }

    let (Ok(mask_w_u32), Ok(mask_h_u32)) = (u32::try_from(mask_w), u32::try_from(mask_h)) else {
        return;
    };
    let mut coverage = vec![0u8; mask_w.saturating_mul(mask_h)];
    Mask::new(&commands[..])
        .style(outline.winding.to_zeno())
        .format(Format::Alpha)
        .size(mask_w_u32, mask_h_u32)
        .render_into(&mut coverage, None);

    // Monochrome tint contract (doc 4.1): RGB replaced by color, alpha scaled by
    // coverage and the color's alpha. Identical math to raster::sample_swash_pixel.
    let tint_alpha = f32::from(color[3]) / 255.0;
    for my in 0..mask_h {
        let canvas_y = win_min_y + my;
        for mx in 0..mask_w {
            // Map raw coverage through the AA transfer table before tinting.
            let cov = aa_lut[usize::from(coverage[my * mask_w + mx])];
            if cov == 0 {
                continue;
            }
            let canvas_x = win_min_x + mx;
            let out_a = (f32::from(cov) * tint_alpha).round().clamp(0.0, 255.0) as u8;
            if out_a == 0 {
                continue;
            }
            let idx = (canvas_y * canvas_w + canvas_x) * 4;
            blend_pixel_over(&mut canvas[idx..idx + 4], color[0], color[1], color[2], out_a);
        }
    }
}

/// Clamp a float coordinate to the integer range `[0, upper]`.
///
/// Non-finite inputs clamp to `0`. Used to keep raster windows inside the canvas.
#[must_use]
fn clamp_to_range(value: f32, lower: usize, upper: usize) -> usize {
    if !value.is_finite() || value <= lower as f32 {
        return lower;
    }
    if value >= upper as f32 {
        return upper;
    }
    // In-range and finite: the truncating cast is exact for these small values.
    value as usize
}

/// Build a `GlyphContour` from an `Outline` for on-path distance measurement.
///
/// Each subpath becomes one contour component (vertices in the SAME y-down
/// glyph-local frame the outline uses, so `GlyphTransform::place_contour` /
/// `GlyphContour::placed` transform them directly). When `simplify_tolerance_px`
/// is positive each component is Douglas-Peucker simplified; components that
/// collapse below three vertices are dropped. This replaces the old
/// rasterize-and-`trace` path for min-distance spacing.
#[must_use]
pub(crate) fn glyph_contour_from_outline(
    outline: &Outline,
    simplify_tolerance_px: f32,
) -> GlyphContour {
    let mut components: Vec<Vec<[f32; 2]>> = Vec::new();
    for subpath in &outline.subpaths {
        let component = if simplify_tolerance_px > 0.0 {
            simplify_closed_ring(subpath, simplify_tolerance_px)
        } else {
            subpath.clone()
        };
        if component.len() >= 3 {
            components.push(component);
        }
    }
    GlyphContour { components }
}

/// Douglas-Peucker simplify a closed ring while keeping it closed.
///
/// Mirrors `glyph_contour::simplify_closed`: anchor at vertex 0 and the farthest
/// vertex, simplify the two open halves independently, and stitch them back.
fn simplify_closed_ring(ring: &[[f32; 2]], tolerance: f32) -> Vec<[f32; 2]> {
    let n = ring.len();
    if n <= 3 {
        return ring.to_vec();
    }
    let anchor = ring[0];
    let mut far = 0usize;
    let mut far_dist = -1.0f32;
    for (i, v) in ring.iter().enumerate() {
        let dx = v[0] - anchor[0];
        let dy = v[1] - anchor[1];
        let d = dx * dx + dy * dy;
        if d > far_dist {
            far_dist = d;
            far = i;
        }
    }
    if far == 0 {
        return ring.to_vec();
    }
    let first_line = &ring[0..=far];
    let mut second_line: Vec<[f32; 2]> = ring[far..n].to_vec();
    second_line.push(ring[0]);
    let s1 = douglas_peucker(first_line, tolerance);
    let s2 = douglas_peucker(&second_line, tolerance);
    let mut out: Vec<[f32; 2]> = Vec::with_capacity(s1.len() + s2.len());
    out.extend_from_slice(&s1[..s1.len() - 1]);
    out.extend_from_slice(&s2[..s2.len() - 1]);
    out
}

/// Iterative Douglas-Peucker on an open polyline (no recursion).
fn douglas_peucker(points: &[[f32; 2]], tolerance: f32) -> Vec<[f32; 2]> {
    let n = points.len();
    if n <= 2 {
        return points.to_vec();
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    let mut stack: Vec<(usize, usize)> = vec![(0, n - 1)];
    while let Some((first, last)) = stack.pop() {
        if last <= first + 1 {
            continue;
        }
        let mut max_dist = -1.0f32;
        let mut max_idx = first;
        for (offset, point) in points[first + 1..last].iter().enumerate() {
            let idx = first + 1 + offset;
            let d = point_line_distance(*point, points[first], points[last]);
            if d > max_dist {
                max_dist = d;
                max_idx = idx;
            }
        }
        if max_dist > tolerance {
            keep[max_idx] = true;
            stack.push((first, max_idx));
            stack.push((max_idx, last));
        }
    }
    points
        .iter()
        .zip(keep.iter())
        .filter_map(|(p, &k)| k.then_some(*p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Identity coverage table used by the geometry/parity tests so they keep
    /// asserting the raw (pre-AA) coverage the rasterizer produced before the
    /// anti-aliasing transfer was introduced.
    fn identity_lut() -> [u8; 256] {
        build_aa_lut(AntiAliasingMode::Smooth)
    }

    /// Load the shared Latin+Cyrillic test face bytes.
    fn load_test_font_bytes() -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test/PanelCleaner/pcleaner/data/LiberationSans-Regular.ttf");
        std::fs::read(&path)
            .unwrap_or_else(|e| panic!("failed to read test font {}: {e}", path.display()))
    }

    /// Resolve a glyph id for a `char` via the font charmap.
    fn glyph_for_char(font: &swash::FontRef, ch: char) -> u16 {
        let gid = font.charmap().map(ch);
        assert_ne!(gid, 0, "font is missing glyph for {ch:?}");
        gid
    }

    /// Sample a quadratic bezier at parameter `t`.
    fn quad_at(p0: [f32; 2], p1: [f32; 2], p2: [f32; 2], t: f32) -> [f32; 2] {
        let u = 1.0 - t;
        [
            u * u * p0[0] + 2.0 * u * t * p1[0] + t * t * p2[0],
            u * u * p0[1] + 2.0 * u * t * p1[1] + t * t * p2[1],
        ]
    }

    /// Sample a cubic bezier at parameter `t`.
    fn cubic_at(p0: [f32; 2], p1: [f32; 2], p2: [f32; 2], p3: [f32; 2], t: f32) -> [f32; 2] {
        let u = 1.0 - t;
        [
            u * u * u * p0[0]
                + 3.0 * u * u * t * p1[0]
                + 3.0 * u * t * t * p2[0]
                + t * t * t * p3[0],
            u * u * u * p0[1]
                + 3.0 * u * u * t * p1[1]
                + 3.0 * u * t * t * p2[1]
                + t * t * t * p3[1],
        ]
    }

    /// Minimum distance from `p` to a polyline (as line segments).
    fn dist_to_polyline(p: [f32; 2], poly: &[[f32; 2]]) -> f32 {
        let mut best = f32::INFINITY;
        for w in poly.windows(2) {
            best = best.min(point_line_segment_distance(p, w[0], w[1]));
        }
        best
    }

    /// Distance from point to a (finite) segment.
    fn point_line_segment_distance(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
        let abx = b[0] - a[0];
        let aby = b[1] - a[1];
        let len_sq = abx * abx + aby * aby;
        if len_sq <= f32::EPSILON {
            let dx = p[0] - a[0];
            let dy = p[1] - a[1];
            return (dx * dx + dy * dy).sqrt();
        }
        let t = (((p[0] - a[0]) * abx) + ((p[1] - a[1]) * aby)) / len_sq;
        let t = t.clamp(0.0, 1.0);
        let proj = [a[0] + t * abx, a[1] + t * aby];
        let dx = p[0] - proj[0];
        let dy = p[1] - proj[1];
        (dx * dx + dy * dy).sqrt()
    }

    #[test]
    fn extract_h_outline_has_plausible_bbox() {
        let data = load_test_font_bytes();
        let font = swash::FontRef::from_index(&data, 0).expect("valid font");
        let gid = glyph_for_char(&font, 'H');
        let em = 64.0;
        let outline = extract_glyph_outline(&font, gid, em).expect("H has an outline");
        assert!(!outline.subpaths().is_empty(), "H must have >= 1 subpath");
        let (min, max) = outline.local_bbox();
        let width = max[0] - min[0];
        let height = max[1] - min[1];
        assert!(width > 0.0, "positive width, got {width}");
        // Cap height for LiberationSans is ~0.72 em; allow a generous band.
        assert!(
            height > em * 0.5 && height < em * 0.9,
            "H height ~ cap height, got {height} at em {em}"
        );
    }

    #[test]
    fn extract_space_has_no_outline() {
        let data = load_test_font_bytes();
        let font = swash::FontRef::from_index(&data, 0).expect("valid font");
        let gid = glyph_for_char(&font, ' ');
        assert!(
            extract_glyph_outline(&font, gid, 64.0).is_none(),
            "space glyph has no fillable outline"
        );
    }

    #[test]
    fn extract_rejects_bad_em() {
        let data = load_test_font_bytes();
        let font = swash::FontRef::from_index(&data, 0).expect("valid font");
        let gid = glyph_for_char(&font, 'H');
        assert!(extract_glyph_outline(&font, gid, 0.0).is_none());
        assert!(extract_glyph_outline(&font, gid, -5.0).is_none());
        assert!(extract_glyph_outline(&font, gid, f32::NAN).is_none());
    }

    #[test]
    fn flatten_quad_within_tolerance() {
        let p0 = [0.0, 0.0];
        let p1 = [50.0, 100.0];
        let p2 = [100.0, 0.0];
        let tol = 0.2;
        let mut out = vec![p0];
        flatten_quad(p0, p1, p2, tol, &mut out);
        // Every sampled true-curve point is within tolerance of the polyline.
        let mut worst = 0.0f32;
        for i in 0..=200 {
            let t = i as f32 / 200.0;
            let pt = quad_at(p0, p1, p2, t);
            worst = worst.max(dist_to_polyline(pt, &out));
        }
        assert!(worst <= tol + 1e-3, "quad deviation {worst} > tol {tol}");
    }

    #[test]
    fn flatten_cubic_within_tolerance() {
        let p0 = [0.0, 0.0];
        let p1 = [30.0, 120.0];
        let p2 = [70.0, -40.0];
        let p3 = [100.0, 0.0];
        let tol = 0.15;
        let mut out = vec![p0];
        flatten_cubic(p0, p1, p2, p3, tol, &mut out);
        let mut worst = 0.0f32;
        for i in 0..=400 {
            let t = i as f32 / 400.0;
            let pt = cubic_at(p0, p1, p2, p3, t);
            worst = worst.max(dist_to_polyline(pt, &out));
        }
        assert!(worst <= tol + 1e-3, "cubic deviation {worst} > tol {tol}");
    }

    /// IoU (intersection over union) of two alpha masks thresholded at 128.
    fn mask_iou(a: &[u8], b: &[u8]) -> f32 {
        assert_eq!(a.len(), b.len());
        let mut inter = 0usize;
        let mut union = 0usize;
        for (&x, &y) in a.iter().zip(b.iter()) {
            let xi = x >= 128;
            let yi = y >= 128;
            if xi && yi {
                inter += 1;
            }
            if xi || yi {
                union += 1;
            }
        }
        if union == 0 {
            return 1.0;
        }
        inter as f32 / union as f32
    }

    #[test]
    fn rasterizer_matches_swash_reference() {
        let data = load_test_font_bytes();
        let font = swash::FontRef::from_index(&data, 0).expect("valid font");
        let gid = glyph_for_char(&font, 'R');
        let em = 64.0;

        // Reference: swash's own outline rasterizer (bottom-left origin). This is
        // the strongest parity check: same font outline, zeno fill on both sides;
        // only OUR bezier flattening differs from swash's internal flattening.
        let mut ctx = swash::scale::ScaleContext::new();
        let mut scaler = ctx.builder(font).size(em).hint(false).build();
        let reference = swash::scale::Render::new(&[swash::scale::Source::Outline])
            .format(zeno::Format::Alpha)
            .render(&mut scaler, gid)
            .expect("reference render");
        let placement = reference.placement;
        let ref_w = placement.width as usize;
        let ref_h = placement.height as usize;
        assert!(ref_w > 0 && ref_h > 0, "reference bitmap must be non-empty");

        // Our path: extract + rasterize at identity onto a canvas sized to the
        // reference placement. origin maps world -> canvas so ink lands on the
        // same pixels: origin_x = placement.left; origin_y = -placement.top
        // (our y-down outline vs the reference bottom-left origin, see header).
        let outline = extract_glyph_outline(&font, gid, em).expect("R outline");
        let mut canvas = vec![0u8; ref_w * ref_h * 4];
        rasterize_outline_into(
            &mut canvas,
            ref_w,
            ref_h,
            placement.left as f32,
            -(placement.top as f32),
            &outline,
            &GlyphTransform::identity(),
            [255, 255, 255, 255],
            &identity_lut(),
        );

        // Extract our alpha channel and compare to the reference alpha.
        let ours_alpha: Vec<u8> = canvas.chunks_exact(4).map(|px| px[3]).collect();
        let iou = mask_iou(&ours_alpha, &reference.data);
        assert!(iou >= 0.93, "zeno-vs-swash IoU too low: {iou}");

        // Mean absolute alpha difference over the whole (union) bbox.
        let sum: u32 = ours_alpha
            .iter()
            .zip(reference.data.iter())
            .map(|(&a, &b)| u32::from(a.abs_diff(b)))
            .sum();
        let mad = sum as f32 / ours_alpha.len() as f32;
        assert!(mad < 12.0, "mean abs alpha diff too high: {mad}");
    }

    #[test]
    fn tint_contract_replaces_rgb_and_scales_alpha() {
        let data = load_test_font_bytes();
        let font = swash::FontRef::from_index(&data, 0).expect("valid font");
        let gid = glyph_for_char(&font, 'H');
        let em = 64.0;
        let outline = extract_glyph_outline(&font, gid, em).expect("H outline");
        let (min, max) = outline.local_bbox();
        let w = (max[0] - min[0]).ceil() as usize + 4;
        let h = (max[1] - min[1]).ceil() as usize + 4;

        // Opaque red: covered pixels are exactly the tint RGB.
        let mut canvas_full = vec![0u8; w * h * 4];
        rasterize_outline_into(
            &mut canvas_full,
            w,
            h,
            min[0] - 2.0,
            min[1] - 2.0,
            &outline,
            &GlyphTransform::identity(),
            [200, 30, 30, 255],
            &identity_lut(),
        );
        let mut covered = 0usize;
        for px in canvas_full.chunks_exact(4) {
            if px[3] > 0 {
                covered += 1;
                assert_eq!([px[0], px[1], px[2]], [200, 30, 30], "RGB must be tint");
            }
        }
        assert!(covered > 0, "expected covered pixels");

        // Half alpha: coverage alpha is halved vs the opaque render (+/- 1).
        let mut canvas_half = vec![0u8; w * h * 4];
        rasterize_outline_into(
            &mut canvas_half,
            w,
            h,
            min[0] - 2.0,
            min[1] - 2.0,
            &outline,
            &GlyphTransform::identity(),
            [200, 30, 30, 128],
            &identity_lut(),
        );
        for (full, half) in canvas_full.chunks_exact(4).zip(canvas_half.chunks_exact(4)) {
            let expected = (f32::from(full[3]) * (128.0 / 255.0)).round() as i32;
            let got = i32::from(half[3]);
            assert!(
                (got - expected).abs() <= 1,
                "half-alpha mismatch: got {got}, expected {expected}"
            );
        }
    }

    #[test]
    fn rotation_swaps_bbox_and_offcanvas_is_safe() {
        let data = load_test_font_bytes();
        let font = swash::FontRef::from_index(&data, 0).expect("valid font");
        let gid = glyph_for_char(&font, 'L');
        let em = 64.0;
        let outline = extract_glyph_outline(&font, gid, em).expect("L outline");
        let (min, max) = outline.local_bbox();
        let ow = max[0] - min[0];
        let oh = max[1] - min[1];

        // Identity render: measure the alpha bbox.
        let big = 200usize;
        let mut c_id = vec![0u8; big * big * 4];
        rasterize_outline_into(
            &mut c_id,
            big,
            big,
            min[0] - 20.0,
            min[1] - 20.0,
            &outline,
            &GlyphTransform::identity(),
            [255, 255, 255, 255],
            &identity_lut(),
        );
        let (idw, idh) = alpha_bbox_dims(&c_id, big, big);

        // 90-degree rotation about the origin: width/height of the ink swap.
        let rot = GlyphTransform {
            pos: [0.0, 0.0],
            rot: std::f32::consts::FRAC_PI_2,
            scale: [1.0, 1.0],
        };
        // After rot, local x-extent maps to y and vice versa; choose an origin
        // that keeps the rotated ink on-canvas.
        let mut c_rot = vec![0u8; big * big * 4];
        rasterize_outline_into(
            &mut c_rot,
            big,
            big,
            -(max[1]) - 20.0,
            min[0] - 20.0,
            &outline,
            &rot,
            [255, 255, 255, 255],
            &identity_lut(),
        );
        let (rw, rh) = alpha_bbox_dims(&c_rot, big, big);

        // The rotated ink's width ~ original height and height ~ original width.
        assert!(
            (rw - idh).abs() <= (oh * 0.15).max(3.0),
            "rotated width {rw} should match original height {idh} (orig {oh})"
        );
        assert!(
            (rh - idw).abs() <= (ow * 0.15).max(3.0),
            "rotated height {rh} should match original width {idw} (orig {ow})"
        );

        // Fully off-canvas transform: must not panic and must write nothing.
        let mut c_off = vec![7u8; big * big * 4];
        let expected = c_off.clone();
        let far = GlyphTransform {
            pos: [100_000.0, 100_000.0],
            rot: 0.0,
            scale: [1.0, 1.0],
        };
        rasterize_outline_into(
            &mut c_off,
            big,
            big,
            0.0,
            0.0,
            &outline,
            &far,
            [255, 255, 255, 255],
            &identity_lut(),
        );
        assert_eq!(c_off, expected, "off-canvas render must leave the buffer intact");
    }

    /// Width/height (px) of the alpha>0 bounding box in an RGBA canvas.
    fn alpha_bbox_dims(canvas: &[u8], w: usize, h: usize) -> (f32, f32) {
        let mut min_x = w;
        let mut min_y = h;
        let mut max_x = 0usize;
        let mut max_y = 0usize;
        let mut found = false;
        for y in 0..h {
            for x in 0..w {
                if canvas[(y * w + x) * 4 + 3] > 0 {
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                    found = true;
                }
            }
        }
        if !found {
            return (0.0, 0.0);
        }
        ((max_x - min_x + 1) as f32, (max_y - min_y + 1) as f32)
    }

    /// Alpha-weighted centroid (x, y) of an RGBA canvas, or `None` if fully
    /// transparent. Used to detect a sub-pixel positional shift.
    fn alpha_centroid(canvas: &[u8], w: usize, h: usize) -> Option<(f32, f32)> {
        let mut sum_a = 0.0f64;
        let mut sum_x = 0.0f64;
        let mut sum_y = 0.0f64;
        for y in 0..h {
            for x in 0..w {
                let a = f64::from(canvas[(y * w + x) * 4 + 3]);
                sum_a += a;
                sum_x += a * x as f64;
                sum_y += a * y as f64;
            }
        }
        if sum_a <= 0.0 {
            return None;
        }
        Some(((sum_x / sum_a) as f32, (sum_y / sum_a) as f32))
    }

    #[test]
    fn subpixel_pos_shifts_centroid() {
        // The vector rasterizer fills at exact float coords, so a +0.5 px shift in
        // `GlyphTransform.pos` must move the alpha-weighted centroid by ~0.5 px on
        // that axis (and leave the other axis unchanged). This is the property the
        // outline subpixel restoration relies on: re-adding the baked x_bin/y_bin
        // to the placement moves the drawn ink by that fraction.
        let data = load_test_font_bytes();
        let font = swash::FontRef::from_index(&data, 0).expect("valid font");
        let gid = glyph_for_char(&font, 'H');
        let em = 64.0;
        let outline = extract_glyph_outline(&font, gid, em).expect("H outline");
        let (min, max) = outline.local_bbox();
        // Pad so the shifted ink never clips the canvas edge on any axis.
        let w = (max[0] - min[0]).ceil() as usize + 8;
        let h = (max[1] - min[1]).ceil() as usize + 8;
        let origin_x = min[0] - 4.0;
        let origin_y = min[1] - 4.0;

        let render_at = |pos: [f32; 2]| -> (f32, f32) {
            let mut canvas = vec![0u8; w * h * 4];
            rasterize_outline_into(
                &mut canvas,
                w,
                h,
                origin_x,
                origin_y,
                &outline,
                &GlyphTransform {
                    pos,
                    rot: 0.0,
                    scale: [1.0, 1.0],
                },
                [255, 255, 255, 255],
                &identity_lut(),
            );
            alpha_centroid(&canvas, w, h).expect("non-empty render")
        };

        let (base_cx, base_cy) = render_at([0.0, 0.0]);
        let (x_cx, x_cy) = render_at([0.5, 0.0]);
        let (y_cx, y_cy) = render_at([0.0, 0.5]);

        // A +0.5 px x shift moves the centroid ~0.5 px in x, ~0 in y.
        assert!(
            (x_cx - base_cx - 0.5).abs() < 0.05,
            "x centroid shift {} should be ~0.5",
            x_cx - base_cx
        );
        assert!((x_cy - base_cy).abs() < 0.05, "x shift must not move y centroid");
        // A +0.5 px y shift moves the centroid ~0.5 px in y, ~0 in x.
        assert!(
            (y_cy - base_cy - 0.5).abs() < 0.05,
            "y centroid shift {} should be ~0.5",
            y_cy - base_cy
        );
        assert!((y_cx - base_cx).abs() < 0.05, "y shift must not move x centroid");
    }

    #[test]
    fn from_outline_component_counts() {
        let data = load_test_font_bytes();
        let font = swash::FontRef::from_index(&data, 0).expect("valid font");
        let em = 64.0;

        // 'O' is a single ink blob (outer + hole) -> one closed outer component
        // per subpath; the glyph has an outer and an inner ring => 2 subpaths.
        // We assert the OUTER AABB matches the outline bbox and that at least the
        // outer contour is present.
        let o_outline = extract_glyph_outline(&font, glyph_for_char(&font, 'O'), em)
            .expect("O outline");
        let o_contour = glyph_contour_from_outline(&o_outline, 0.5);
        assert!(
            !o_contour.components.is_empty(),
            "O must yield at least one component"
        );
        // Outer AABB over all components must match the outline bbox closely.
        let (omin, omax) = o_outline.local_bbox();
        let placed = GlyphTransform::identity().place_contour(&o_contour);
        assert!((placed.aabb_min[0] - omin[0]).abs() <= 2.0);
        assert!((placed.aabb_min[1] - omin[1]).abs() <= 2.0);
        assert!((placed.aabb_max[0] - omax[0]).abs() <= 2.0);
        assert!((placed.aabb_max[1] - omax[1]).abs() <= 2.0);

        // ':' (colon) is two disjoint ink blobs -> two components.
        let colon_outline = extract_glyph_outline(&font, glyph_for_char(&font, ':'), em)
            .expect("colon outline");
        let colon_contour = glyph_contour_from_outline(&colon_outline, 0.5);
        assert_eq!(
            colon_contour.components.len(),
            2,
            "colon must yield two components"
        );
    }

    #[test]
    fn cache_returns_same_arc_and_caches_negative() {
        let data = load_test_font_bytes();
        let font = swash::FontRef::from_index(&data, 0).expect("valid font");
        let mut cache = OutlineCache::new();
        let gid = glyph_for_char(&font, 'A');
        let key = OutlineKey::new(1, gid, 64.0);
        let first = cache.get_or_extract(key, &font, gid, 64.0).expect("A outline");
        let second = cache.get_or_extract(key, &font, gid, 64.0).expect("A outline");
        assert!(Arc::ptr_eq(&first, &second), "cache must return the same Arc");
        assert_eq!(cache.len(), 1);

        // Negative result (space) is cached without re-extraction.
        let space = glyph_for_char(&font, ' ');
        let space_key = OutlineKey::new(1, space, 64.0);
        assert!(cache.get_or_extract(space_key, &font, space, 64.0).is_none());
        assert!(cache.get_or_extract(space_key, &font, space, 64.0).is_none());
        assert_eq!(cache.len(), 2);
    }

    /// Every AA table must start at 0 and be monotonic non-decreasing.
    fn assert_lut_monotonic(lut: &[u8; 256]) {
        assert_eq!(lut[0], 0, "lut[0] must be 0");
        for i in 1..256 {
            assert!(
                lut[i] >= lut[i - 1],
                "lut must be non-decreasing at {i}: {} < {}",
                lut[i],
                lut[i - 1]
            );
        }
    }

    #[test]
    fn aa_lut_smooth_is_identity() {
        let lut = build_aa_lut(AntiAliasingMode::Smooth);
        for (i, &v) in lut.iter().enumerate() {
            assert_eq!(usize::from(v), i, "Smooth must be identity at {i}");
        }
    }

    #[test]
    fn aa_lut_none_is_step_at_128() {
        let lut = build_aa_lut(AntiAliasingMode::None);
        // c = i/255 >= 0.5 <=> i >= 127.5 <=> i >= 128.
        for (i, &v) in lut.iter().enumerate() {
            let expected = if i >= 128 { 255 } else { 0 };
            assert_eq!(u32::from(v), expected, "None threshold wrong at {i}");
        }
    }

    #[test]
    fn aa_lut_sharpness_ordering_around_mid() {
        let smooth = build_aa_lut(AntiAliasingMode::Smooth);
        let crisp = build_aa_lut(AntiAliasingMode::Crisp);
        let sharp = build_aa_lut(AntiAliasingMode::Sharp);
        // Above the mid point a steeper curve lifts coverage higher: at index 160
        // Sharp > Crisp > Smooth.
        assert!(
            sharp[160] > crisp[160],
            "Sharp {} should exceed Crisp {} at 160",
            sharp[160],
            crisp[160]
        );
        assert!(
            crisp[160] > smooth[160],
            "Crisp {} should exceed Smooth {} at 160",
            crisp[160],
            smooth[160]
        );
        // Below the mid point the steeper curve is darker (pushes toward 0).
        assert!(sharp[96] < crisp[96], "Sharp should be darker below mid");
        assert!(crisp[96] < smooth[96], "Crisp should be darker below mid");
    }

    #[test]
    fn aa_lut_strong_lifts_mid_above_crisp() {
        let crisp = build_aa_lut(AntiAliasingMode::Crisp);
        let strong = build_aa_lut(AntiAliasingMode::Strong);
        // The additive bias makes Strong denser than Crisp at the exact mid byte.
        assert!(
            strong[128] > crisp[128],
            "Strong bias should lift mid: strong {} vs crisp {}",
            strong[128],
            crisp[128]
        );
    }

    #[test]
    fn aa_lut_all_monotonic_and_zero_origin() {
        for mode in [
            AntiAliasingMode::None,
            AntiAliasingMode::Sharp,
            AntiAliasingMode::Crisp,
            AntiAliasingMode::Strong,
            AntiAliasingMode::Smooth,
        ] {
            assert_lut_monotonic(&build_aa_lut(mode));
        }
    }
}
