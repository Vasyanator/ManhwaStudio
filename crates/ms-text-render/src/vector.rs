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
- GlyphTransform: local->world affine (scale, faux-italic baseline shear,
  rotation, translation), same convention as `PlacedContour`.
- OutlineKey / OutlineCache: resolution-independent outline cache keyed per
  faux-bold variant (`faux_bits`, 0 = plain); `OutlineCache` also owns the
  reusable swash `ScaleContext` used on a cache miss.
- FauxOutlineParams: quantized (1/64 px) faux-bold offset parameters shared by
  the outline cache key and the offset geometry itself.
- RasterScratch: reusable per-render rasterizer buffers (subpaths, zeno commands,
  coverage mask) so `rasterize_outline_into` allocates nothing per glyph.

Key functions:
- extract_glyph_outline: swash outline -> flattened `Outline` (via a reused
  `ScaleContext` passed by the caller).
- offset_outline: polyline offset (faux bold embolden) on the flattened
  outline — outward for outer contours, optional hole shrink, miter/round
  joins; self-intersections are resolved by the NonZero fill.
- flatten_quad / flatten_cubic: adaptive bezier flattening.
- rasterize_outline_into: the single vector rasterizer (zeno + tint + over-blend);
  takes a `&mut RasterScratch` and resets it per glyph for byte-identical reuse.
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
use super::types::{AntiAliasingMode, VectorMeshWarp};
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
/// `world = Rot(rot) * (Shear(shear_x) * Scale(sx, sy) * local) + pos`, where
/// `Rot([x, y]) = [x*cos - y*sin, x*sin + y*cos]` and
/// `Shear([x, y]) = [x - shear_x * y, y]`. The shear sits BETWEEN scale and
/// rotation and operates in the scaled glyph-local frame, whose baseline is the
/// `y = 0` line (outline extraction keeps the pen/baseline at the local
/// origin, ascenders at negative y in the y-down frame). Because `pos` places
/// the local origin (the pen), the baseline is invariant under the shear and a
/// POSITIVE `shear_x` leans glyph tops to the right (faux italic,
/// `shear_x = tan(slant)`). `shear_x = 0.0` is bit-exact to the pre-shear
/// transform. This is the SAME convention as `glyph_contour::PlacedContour`,
/// so a `GlyphTransform` can place both the rasterized outline and the
/// measured contour identically.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GlyphTransform {
    /// World-space translation applied after scale and rotation.
    pub(crate) pos: [f32; 2],
    /// Rotation in radians.
    pub(crate) rot: f32,
    /// Per-axis scale `[sx, sy]` applied before rotation.
    pub(crate) scale: [f32; 2],
    /// Baseline shear applied between scale and rotation
    /// (`x' = x - shear_x * y` in the scaled local frame); `0.0` = none.
    pub(crate) shear_x: f32,
}

impl GlyphTransform {
    /// Identity transform (no translation/rotation/shear, unit scale).
    #[must_use]
    pub(crate) fn identity() -> Self {
        Self {
            pos: [0.0, 0.0],
            rot: 0.0,
            scale: [1.0, 1.0],
            shear_x: 0.0,
        }
    }

    /// Transform one glyph-local point to world space (scale, shear, rotate,
    /// translate — see the struct doc for the exact composition).
    #[must_use]
    fn apply(&self, local: [f32; 2], cos: f32, sin: f32) -> [f32; 2] {
        let sy = local[1] * self.scale[1];
        // Baseline shear in the scaled local frame; `shear_x == 0.0` yields
        // `sx - 0.0`, which is bit-exact to the unsheared value.
        let sx = local[0] * self.scale[0] - self.shear_x * sy;
        [
            sx * cos - sy * sin + self.pos[0],
            sx * sin + sy * cos + self.pos[1],
        ]
    }

    /// Place a glyph-local `GlyphContour` into world space with this transform.
    ///
    /// Reuses `GlyphContour::placed` (including the baseline shear) so on-path
    /// measurement and rasterization share one placement convention.
    #[must_use]
    pub(crate) fn place_contour(&self, contour: &GlyphContour) -> PlacedContour {
        let (cos, sin) = (self.rot.cos(), self.rot.sin());
        contour.placed_sheared(
            cos,
            sin,
            self.scale[0],
            self.scale[1],
            self.shear_x,
            self.pos[0],
            self.pos[1],
        )
    }
}

/// Offset distances below this are treated as "no faux bold" (return the plain
/// outline unchanged). Half of the 1/64 px quantization step.
const FAUX_MIN_OFFSET_PX: f32 = 1.0 / 128.0;

/// Quantized faux-bold parameters for outline offsetting and cache keying.
///
/// The offset distance is stored in 1/64 px fixed point so two near-identical
/// `f32` distances share one cache entry AND one geometry (the offset itself is
/// computed from the quantized value — key and geometry can never disagree).
/// The step is far below visual resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FauxOutlineParams {
    /// Offset distance in 1/64 px units, `> 0` (zero-distance params are
    /// rejected by [`FauxOutlineParams::new`]). Capped at `2^30 - 1` so the two
    /// flag bits of [`FauxOutlineParams::key_bits`] never collide.
    d_q64: u32,
    /// Miter (`true`) vs round (`false`) joins at offset vertices.
    sharp_corners: bool,
    /// Preserve counters/holes (`true`) vs shrink them by `d` (`false`).
    outward_only: bool,
}

impl FauxOutlineParams {
    /// Quantize `d_px` (outline offset distance in glyph-local px) to 1/64 px.
    ///
    /// Returns `None` for a non-finite, non-positive, or sub-quantum distance —
    /// callers then take the plain (non-faux) outline path, which keeps a
    /// zero-strength faux bold byte-identical to no faux bold.
    #[must_use]
    pub(crate) fn new(d_px: f32, sharp_corners: bool, outward_only: bool) -> Option<Self> {
        if !d_px.is_finite() || d_px < FAUX_MIN_OFFSET_PX {
            return None;
        }
        // Round to the fixed-point grid; cap so the flag bits stay free.
        let d_q64 = (d_px * 64.0).round().min(((1u32 << 30) - 1) as f32) as u32;
        if d_q64 == 0 {
            return None;
        }
        Some(Self {
            d_q64,
            sharp_corners,
            outward_only,
        })
    }

    /// The quantized offset distance in px (always `> 0`).
    #[must_use]
    pub(crate) fn d_px(self) -> f32 {
        self.d_q64 as f32 / 64.0
    }

    /// Worst-case distance the offset boundary can stray from the source ink:
    /// `d` for round joins (and bevel fallbacks), up to the miter limit `4*d`
    /// at acute sharp corners. Bounds/canvas padding must use this, not `d`.
    #[must_use]
    pub(crate) fn max_overhang_px(self) -> f32 {
        self.d_px() * if self.sharp_corners { 4.0 } else { 1.0 }
    }

    /// Pack into the non-zero `u32` used by [`OutlineKey`] and the optical
    /// contour-cache keys: bits 0..=29 = `d_q64`, bit 30 = sharp, bit 31 = out.
    #[must_use]
    pub(crate) fn key_bits(self) -> u32 {
        self.d_q64
            | (u32::from(self.sharp_corners) << 30)
            | (u32::from(self.outward_only) << 31)
    }
}

/// Non-zero faux key bits, or `0` for the plain (non-faux) variant.
///
/// Shared helper for every cache keyed per faux-bold variant (`OutlineKey`,
/// `OpticalContourCache`, the formula ink cache).
#[must_use]
pub(crate) fn faux_key_bits(faux: Option<FauxOutlineParams>) -> u32 {
    faux.map_or(0, FauxOutlineParams::key_bits)
}

/// Cache key for an extracted outline.
///
/// Vectors are resolution independent, so no subpixel bin is needed; the em
/// size is keyed by its raw `f32` bit pattern to make identical sizes hit.
/// `faux_bits` distinguishes faux-bold offset variants of the same glyph
/// (`0` = the plain extracted outline, so plain-variant keys — and therefore
/// the cached plain geometry — are unaffected by the faux feature).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct OutlineKey {
    /// Stable per-font identifier supplied by the caller.
    font_id: u64,
    /// Glyph id within the font.
    glyph_id: u16,
    /// `em_px.to_bits()` — exact-match keying for the extraction ppem.
    em_bits: u32,
    /// Packed faux-bold variant bits (`FauxOutlineParams::key_bits`), `0` = plain.
    faux_bits: u32,
}

impl OutlineKey {
    /// Build a PLAIN (non-faux) key; `em_px` is keyed by its exact bit pattern.
    #[must_use]
    pub(crate) fn new(font_id: u64, glyph_id: u16, em_px: f32) -> Self {
        Self {
            font_id,
            glyph_id,
            em_bits: em_px.to_bits(),
            faux_bits: 0,
        }
    }

    /// This key's faux-bold variant: same glyph/em, faux bits from `faux`.
    #[must_use]
    fn with_faux(self, faux: FauxOutlineParams) -> Self {
        Self {
            faux_bits: faux.key_bits(),
            ..self
        }
    }
}

/// Extracted-outline cache.
///
/// Stores `Option<Arc<Outline>>` so a glyph known to have no fillable outline
/// (space/empty) is cached as a negative result and not re-extracted. Owns the
/// reusable `swash::scale::ScaleContext` so a cache MISS extracts through one
/// shared scaler context instead of building a fresh `ScaleContext` (with its
/// internal caches) per glyph. `ScaleContext` is not `Debug`, so `Debug` is
/// implemented manually and only reports the entry count.
pub(crate) struct OutlineCache {
    map: HashMap<OutlineKey, Option<Arc<Outline>>>,
    /// Reused swash scaler context; passed by `&mut` into `extract_glyph_outline`
    /// on every cache miss so extraction does not allocate a new context.
    context: swash::scale::ScaleContext,
}

impl std::fmt::Debug for OutlineCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // ScaleContext is not Debug; report only the observable cache size.
        f.debug_struct("OutlineCache")
            .field("entries", &self.map.len())
            .finish_non_exhaustive()
    }
}

impl Default for OutlineCache {
    fn default() -> Self {
        Self::new()
    }
}

impl OutlineCache {
    /// Empty cache with a fresh reusable scaler context.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            map: HashMap::new(),
            context: swash::scale::ScaleContext::new(),
        }
    }

    /// Return the cached outline for `key`, extracting it once on a miss.
    ///
    /// `font`/`glyph_id`/`em_px` must correspond to `key`. Returns `None` (and
    /// caches it) when the glyph has no fillable outline. The miss path extracts
    /// through the cache-owned reusable `ScaleContext`. Never panics.
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
        let extracted = extract_glyph_outline(&mut self.context, font, glyph_id, em_px).map(Arc::new);
        self.map.insert(key, extracted.clone());
        extracted
    }

    /// Faux-bold-aware outline lookup: the plain outline for `faux == None`,
    /// or the offset (emboldened) variant cached under the faux key otherwise.
    ///
    /// `key` must be the PLAIN key for `font`/`glyph_id`/`em_px`. On a faux
    /// miss the plain outline is resolved first (through this same cache, so it
    /// is extracted at most once and stays byte-identical to the non-faux
    /// path), then offset via [`offset_outline`] with the QUANTIZED distance
    /// and cached under the faux variant key. A glyph with no fillable outline
    /// is `None` for every variant. Never panics.
    pub(crate) fn get_or_extract_with_faux(
        &mut self,
        key: OutlineKey,
        font: &swash::FontRef,
        glyph_id: u16,
        em_px: f32,
        faux: Option<FauxOutlineParams>,
    ) -> Option<Arc<Outline>> {
        let Some(faux) = faux else {
            return self.get_or_extract(key, font, glyph_id, em_px);
        };
        let faux_key = key.with_faux(faux);
        if let Some(cached) = self.map.get(&faux_key) {
            return cached.clone();
        }
        let offset = self
            .get_or_extract(key, font, glyph_id, em_px)
            .map(|plain| {
                Arc::new(offset_outline(
                    &plain,
                    faux.d_px(),
                    faux.sharp_corners,
                    faux.outward_only,
                ))
            });
        self.map.insert(faux_key, offset.clone());
        offset
    }

    /// Number of cached entries (including negative results).
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

/// Reusable per-render scratch buffers for [`rasterize_outline_into`].
///
/// The rasterizer needs three working buffers per glyph: the canvas-space
/// subpath polylines, the zeno path commands, and the 8-bit coverage mask.
/// Allocating them fresh per glyph is real allocator traffic across hundreds of
/// glyphs per render (thousands across a preview grid). Threading one
/// `RasterScratch` through every rasterize call on a render path reuses those
/// allocations.
///
/// Reset contract (BYTE-IDENTICAL to a fresh allocation): each glyph clears
/// every buffer WITHOUT freeing capacity and re-zeroes the coverage mask to the
/// new window size. Inner subpath Vecs are pooled (`subpath_pool`) so a glyph
/// with N subpaths does not re-allocate N inner Vecs. No stale point, command,
/// or coverage byte may survive between glyphs — `rasterize_outline_into`
/// depends on a fully zeroed coverage mask because `Mask::render_into` only
/// writes covered cells.
#[derive(Debug, Default)]
pub(crate) struct RasterScratch {
    /// Canvas-space subpath polylines for the current glyph. The outer Vec is
    /// reused; inner Vecs come from and return to `subpath_pool`.
    canvas_subpaths: Vec<Vec<[f32; 2]>>,
    /// Free list of inner subpath Vecs (each already cleared) reclaimed from
    /// `canvas_subpaths` so their capacity survives between glyphs.
    subpath_pool: Vec<Vec<[f32; 2]>>,
    /// zeno path commands for the current glyph.
    commands: Vec<Command>,
    /// 8-bit coverage mask sized to the current glyph's raster window.
    coverage: Vec<u8>,
}

impl RasterScratch {
    /// Empty scratch; buffers grow to fit on first use.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Reclaim the previous glyph's inner subpath Vecs into the pool (clearing
    /// each) and clear the command buffer, retaining all capacity. Call once at
    /// the start of each glyph before rebuilding. Coverage is (re)sized and
    /// zeroed separately once the raster window is known.
    fn begin_glyph(&mut self) {
        for mut subpath in self.canvas_subpaths.drain(..) {
            subpath.clear();
            self.subpath_pool.push(subpath);
        }
        self.commands.clear();
    }

    /// Take an already-cleared inner subpath Vec from the pool, or a fresh empty
    /// one if the pool is empty. Its capacity is preserved for reuse.
    #[must_use]
    fn take_subpath(&mut self) -> Vec<[f32; 2]> {
        self.subpath_pool.pop().unwrap_or_default()
    }
}

/// Extract and flatten a glyph outline at `em_px` pixels-per-em.
///
/// Uses `swash::scale::Scaler::scale_outline`; swash reports points y-up, and
/// this function negates y so the result is y-down glyph-local pixels
/// consistent with `raster.rs`/`glyph_contour.rs`. Quadratics and cubics are
/// flattened to `DEFAULT_FLATTEN_TOLERANCE_PX`. `context` is a reusable swash
/// scaler context (owned by `OutlineCache`) so callers do not build a fresh
/// `ScaleContext` per extraction.
///
/// Returns `None` for a glyph with no fillable outline (space/empty), a
/// non-finite or non-positive `em_px`, or a color glyph (handled elsewhere).
/// Never panics on a missing/invalid glyph id.
#[must_use]
pub(crate) fn extract_glyph_outline(
    context: &mut swash::scale::ScaleContext,
    font: &swash::FontRef,
    glyph_id: u16,
    em_px: f32,
) -> Option<Outline> {
    if !em_px.is_finite() || em_px <= 0.0 {
        return None;
    }
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

/// Contours with |signed area| below this (px^2) are dropped by
/// [`offset_outline`] as degenerate (hairline slivers cannot be offset
/// meaningfully and contribute no visible fill).
const FAUX_MIN_CONTOUR_AREA_PX2: f32 = 1e-2;

/// Miter-limit fallback threshold: bevel when `1 + n0·n1 < 0.125`, i.e. when
/// the miter length `d * sqrt(2 / (1 + dot))` would exceed `4 * d`.
const FAUX_MITER_LIMIT_DOT: f32 = 0.125;

/// Chord tolerance (px) for round-join circular-arc approximation; matches the
/// bezier flattening tolerance so joins are as smooth as the flattened curves.
const FAUX_ARC_TOLERANCE_PX: f32 = 0.2;

/// Hard cap on interior points per round join (safety bound for huge `d`).
const FAUX_MAX_ARC_STEPS: usize = 64;

/// Shoelace signed area of a closed ring (closing edge implicit).
///
/// Orientation convention (this crate's y-DOWN glyph-local frame, see the file
/// header): a POSITIVE area means the ring runs clockwise on screen. The sign
/// gives each ring's own orientation (used for its outward normal and for the
/// inversion check after offsetting); outer-vs-hole classification does NOT
/// rely on any absolute winding convention — [`offset_outline`] evaluates the
/// actual NonZero winding of the whole outline at a point inside each ring.
#[must_use]
fn ring_signed_area(ring: &[[f32; 2]]) -> f32 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut sum = 0.0f32;
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        sum += a[0] * b[1] - b[0] * a[1];
    }
    sum * 0.5
}

/// Wrap an angle difference into `(-PI, PI]`.
#[must_use]
fn wrap_angle(mut angle: f32) -> f32 {
    while angle > std::f32::consts::PI {
        angle -= std::f32::consts::TAU;
    }
    while angle <= -std::f32::consts::PI {
        angle += std::f32::consts::TAU;
    }
    angle
}

/// Offset (embolden) a flattened glyph outline by `d_px` on the crate's own
/// polyline representation, preserving vertex order (and therefore winding).
///
/// Semantics (faux bold):
/// - Outer-vs-hole is decided by the actual NonZero fill semantics, not by a
///   winding convention: for each ring the total winding of ALL
///   (non-degenerate) rings is evaluated at a point strictly inside it
///   (`ring_interior_is_inked`). Ink inside (`winding != 0`) = an OUTER/ink
///   boundary, moved outward by `d_px`; empty inside (`winding == 0`) = a
///   counter (HOLE), moved INTO the hole by `d_px` (shrinking it) when
///   `outward_only == false` and copied unchanged when `outward_only == true`.
///   This matches what the NonZero rasterizer will fill for both TrueType and
///   CFF conventions and for same-wound nested rings (which are ink, not
///   holes). Assumes simple (non-self-intersecting) source contours — the norm
///   for font outlines.
/// - Joins at vertices: `sharp_corners == true` uses miter joins with limit ~4
///   (falling back to bevel beyond the limit); `false` approximates a circular
///   arc with a segment count proportional to `d_px`.
/// - Degenerate contours (fewer than 3 distinct points or near-zero area) are
///   dropped. An offset ring whose orientation FLIPPED or whose area collapsed
///   below the epsilon (a counter narrower than `2*d`) is dropped entirely —
///   an inverted ring would contribute filled ink under NonZero instead of a
///   collapsed counter.
/// - `d_px <= 0` (or below the quantization epsilon) returns a clone.
///
/// Self-intersections in the offset polygons are ACCEPTABLE by contract: the
/// rasterizer fills with the NonZero rule (`rasterize_outline_into`), which
/// resolves both local join overlaps and an expanded outer contour crossing
/// its (preserved) hole. The result recomputes the outline AABB via
/// `Outline::from_subpaths`. Never panics.
#[must_use]
pub(crate) fn offset_outline(
    outline: &Outline,
    d_px: f32,
    sharp_corners: bool,
    outward_only: bool,
) -> Outline {
    if !d_px.is_finite() || d_px < FAUX_MIN_OFFSET_PX {
        return outline.clone();
    }

    // Per-ring signed areas; `retained` marks the non-degenerate rings that
    // both survive into the output and participate in the winding evaluation.
    let areas: Vec<f32> = outline
        .subpaths
        .iter()
        .map(|ring| ring_signed_area(ring))
        .collect();
    let retained: Vec<bool> = outline
        .subpaths
        .iter()
        .zip(areas.iter())
        .map(|(ring, &area)| ring.len() >= 3 && area.abs() >= FAUX_MIN_CONTOUR_AREA_PX2)
        .collect();
    if !retained.iter().any(|&keep| keep) {
        // No non-degenerate contour: nothing to offset.
        return outline.clone();
    }

    let mut out_subpaths: Vec<Vec<[f32; 2]>> = Vec::with_capacity(outline.subpaths.len());
    for (i, (ring, &area)) in outline.subpaths.iter().zip(areas.iter()).enumerate() {
        if !retained[i] {
            continue;
        }
        let is_outer = ring_interior_is_inked(&outline.subpaths, &retained, i);
        if !is_outer && outward_only {
            // Counters preserved: hole geometry copied verbatim.
            out_subpaths.push(ring.clone());
            continue;
        }
        // Outer rings expand (offset outward relative to the ring); holes
        // shrink, i.e. their boundary moves AGAINST the ring's outward normal
        // (into the hole interior), which is a negative signed offset.
        let delta = if is_outer { d_px } else { -d_px };
        let offset_ring = offset_closed_ring(ring, area.signum(), delta, sharp_corners);
        if offset_ring.len() < 3 {
            continue;
        }
        // Inversion/collapse guard: shrinking a counter narrower than `2*d`
        // turns the ring inside out (flipped signed area) — under NonZero an
        // inverted "hole" would ADD ink instead of vanishing. A collapsed
        // counter is simply removed (solid ink), which is the correct
        // embolden limit. Outward offsets cannot flip; the check is uniform
        // for robustness.
        let offset_area = ring_signed_area(&offset_ring);
        if offset_area.abs() < FAUX_MIN_CONTOUR_AREA_PX2 || (offset_area > 0.0) != (area > 0.0) {
            continue;
        }
        out_subpaths.push(offset_ring);
    }

    // `from_subpaths` recomputes the AABB. An outline that lost every contour
    // (e.g. a lone counter that collapsed) falls back to a clone — safe.
    Outline::from_subpaths(out_subpaths, outline.winding).unwrap_or_else(|| outline.clone())
}

/// Whether the region JUST INSIDE ring `index` (immediately right of its
/// leftmost boundary on a scanline) is INK under the NonZero rule: the total
/// winding of all `retained` rings there is non-zero.
///
/// Ink inside means the ring is an outer/ink boundary (faux bold offsets it
/// outward); an empty interior means it bounds a counter (hole). Evaluating
/// the real winding — instead of comparing orientations — is convention-free
/// (TrueType and CFF wind oppositely) and treats same-wound nested rings as
/// the ink they render as. The sample interval runs from the ring's leftmost
/// scanline crossing to the NEAREST boundary of any retained ring, so nested
/// children (e.g. a counter inside this outer ring) can never capture the
/// sample point — a naive interior midpoint could land inside the counter and
/// misclassify the outer ring. Falls back to `true` (outer) if no valid sample
/// interval is found, which only happens for near-degenerate rings.
#[must_use]
fn ring_interior_is_inked(subpaths: &[Vec<[f32; 2]>], retained: &[bool], index: usize) -> bool {
    let ring = &subpaths[index];
    if ring.len() < 3 {
        return true;
    }
    let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
    for v in ring {
        min_y = min_y.min(v[1]);
        max_y = max_y.max(v[1]);
    }
    if !(max_y - min_y).is_finite() || max_y - min_y <= 1e-4 {
        return true;
    }
    // Odd fractions of the height avoid vertex-aligned scanlines on typical
    // axis-aligned geometry; several candidates make the search robust.
    let mut crossings: Vec<f32> = Vec::new();
    for k in [0.5f32, 0.37, 0.61, 0.23, 0.79] {
        let y = min_y + (max_y - min_y) * k;
        // Leftmost crossing of THIS ring = entry into its interior.
        crossings.clear();
        ring_crossings_at_y(ring, y, &mut crossings);
        let Some(x_enter) = crossings
            .iter()
            .copied()
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        else {
            continue;
        };
        // Nearest boundary of ANY retained ring strictly right of the entry:
        // the open interval between them lies inside this ring and crosses no
        // other boundary, so its winding is uniform and describes the region
        // adjacent to this ring's inner side.
        let mut x_next = f32::INFINITY;
        for (other, &keep) in subpaths.iter().zip(retained.iter()) {
            if !keep {
                continue;
            }
            crossings.clear();
            ring_crossings_at_y(other, y, &mut crossings);
            for &x in &crossings {
                if x > x_enter + 1e-4 {
                    x_next = x_next.min(x);
                }
            }
        }
        if !x_next.is_finite() {
            continue;
        }
        let sample = [(x_enter + x_next) * 0.5, y];
        let mut winding = 0i32;
        for (other, &keep) in subpaths.iter().zip(retained.iter()) {
            if keep {
                winding += ring_winding_at(other, sample);
            }
        }
        return winding != 0;
    }
    true
}

/// Append the x coordinates where the closed ring's edges cross the horizontal
/// scanline `y` (half-open `<=` rule, so vertex-aligned scanlines count each
/// crossing once).
fn ring_crossings_at_y(ring: &[[f32; 2]], y: f32, out: &mut Vec<f32>) {
    let n = ring.len();
    if n < 2 {
        return;
    }
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        if (a[1] <= y) != (b[1] <= y) {
            let t = (y - a[1]) / (b[1] - a[1]);
            out.push(a[0] + t * (b[0] - a[0]));
        }
    }
}

/// NonZero winding number of one closed ring at `point` (`0` when outside; the
/// sign reflects the ring's orientation in the y-down frame).
///
/// Standard signed crossing count over the rightward ray from `point`; the
/// half-open `<=` test makes shared vertices count exactly once. `point` must
/// not lie exactly ON the ring (callers sample strictly interior points).
#[must_use]
fn ring_winding_at(ring: &[[f32; 2]], point: [f32; 2]) -> i32 {
    let n = ring.len();
    if n < 3 {
        return 0;
    }
    let mut winding = 0i32;
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        if (a[1] <= point[1]) != (b[1] <= point[1]) {
            // The divisor is non-zero exactly because the endpoints straddle.
            let t = (point[1] - a[1]) / (b[1] - a[1]);
            let x = a[0] + t * (b[0] - a[0]);
            if x > point[0] {
                winding += if b[1] > a[1] { 1 } else { -1 };
            }
        }
    }
    winding
}

/// Offset one closed ring by the signed distance `delta` along its outward
/// normal, joining edges per `sharp_corners`.
///
/// `orientation_sign` is the sign of the ring's shoelace area in the y-down
/// frame; the outward unit normal of an edge with direction `t = (dx, dy)` is
/// `sign * (dy, -dx) / |t|` (verified by the axis-aligned-square unit tests).
/// A join gap opens at a vertex exactly when `sign * cross(t0, t1) * delta > 0`
/// (loop-convex corners for a positive offset, loop-concave for a negative
/// one); the opposite side uses the natural line intersection (miter apex),
/// avoiding backward micro-loops that could flip NonZero winding locally.
/// Returns an empty vec for rings with fewer than 3 distinct points.
fn offset_closed_ring(
    ring: &[[f32; 2]],
    orientation_sign: f32,
    delta: f32,
    sharp_corners: bool,
) -> Vec<[f32; 2]> {
    // Deduplicate consecutive coincident points (keeping order/orientation).
    let mut pts: Vec<[f32; 2]> = Vec::with_capacity(ring.len());
    for &p in ring {
        if pts.last().is_none_or(|last| !points_close(*last, p)) {
            pts.push(p);
        }
    }
    if pts.len() >= 2 && points_close(pts[0], pts[pts.len() - 1]) {
        pts.pop();
    }
    let n = pts.len();
    if n < 3 {
        return Vec::new();
    }

    // Per-edge outward unit normals (edge i runs pts[i] -> pts[(i+1) % n]).
    let mut normals: Vec<[f32; 2]> = Vec::with_capacity(n);
    for i in 0..n {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let len = (dx * dx + dy * dy).sqrt();
        if len <= f32::EPSILON {
            // Coincident points were deduped, so this is unreachable in
            // practice; a zero normal degrades to a plain translation-free
            // join rather than NaN.
            normals.push([0.0, 0.0]);
            continue;
        }
        normals.push([
            orientation_sign * dy / len,
            -orientation_sign * dx / len,
        ]);
    }

    let mut out: Vec<[f32; 2]> = Vec::with_capacity(n * 2);
    for i in 0..n {
        // Vertex pts[i] joins incoming edge (i-1) and outgoing edge i.
        let p = pts[i];
        let n0 = normals[(i + n - 1) % n];
        let n1 = normals[i];
        let e0 = [p[0] + delta * n0[0], p[1] + delta * n0[1]];
        let e1 = [p[0] + delta * n1[0], p[1] + delta * n1[1]];
        let dot = n0[0] * n1[0] + n0[1] * n1[1];
        let cross = n0[0] * n1[1] - n0[1] * n1[0];

        if dot >= 1.0 - 1e-6 {
            // Collinear edges: one shared offset point.
            out.push(e1);
            continue;
        }
        // Gap opens on the offset side at loop-convex corners for delta > 0
        // (and loop-concave for delta < 0). cross(n0, n1) == cross(t0, t1)
        // because both normals are the same fixed rotation of the tangents.
        let opens = orientation_sign * cross * delta > 0.0;
        if !opens {
            // Closing side: the natural join is the offset-line intersection
            // (same formula as the miter apex); it is bounded here because the
            // edges converge. Near-antiparallel edges fall back to both
            // endpoints (tiny backward loop, absorbed by the NonZero fill).
            if 1.0 + dot > FAUX_MITER_LIMIT_DOT {
                out.push(miter_apex(p, n0, n1, delta, dot));
            } else {
                out.push(e0);
                out.push(e1);
            }
            continue;
        }
        if sharp_corners {
            // Miter within limit ~4, bevel beyond it.
            if 1.0 + dot >= FAUX_MITER_LIMIT_DOT {
                out.push(miter_apex(p, n0, n1, delta, dot));
            } else {
                out.push(e0);
                out.push(e1);
            }
        } else {
            // Round join: circular arc of radius |delta| about the vertex from
            // e0 to e1, swept the short way (the offset-normal rotation).
            out.push(e0);
            let radius = delta.abs();
            let a0 = (e0[1] - p[1]).atan2(e0[0] - p[0]);
            let a1 = (e1[1] - p[1]).atan2(e1[0] - p[0]);
            let sweep = wrap_angle(a1 - a0);
            // Segment count proportional to d: chord error <= FAUX_ARC_TOLERANCE_PX.
            let max_step = if radius > FAUX_ARC_TOLERANCE_PX {
                2.0 * (1.0 - FAUX_ARC_TOLERANCE_PX / radius).acos()
            } else {
                std::f32::consts::FRAC_PI_2
            };
            let steps = ((sweep.abs() / max_step.max(1e-3)).ceil() as usize)
                .clamp(1, FAUX_MAX_ARC_STEPS);
            for step in 1..steps {
                let angle = a0 + sweep * (step as f32 / steps as f32);
                out.push([p[0] + radius * angle.cos(), p[1] + radius * angle.sin()]);
            }
            out.push(e1);
        }
    }
    out
}

/// Intersection of the two offset lines at a vertex (the miter apex):
/// `p + delta * (n0 + n1) / (1 + n0·n1)`. Caller guarantees `1 + dot` is
/// bounded away from zero (miter-limit check), so the division is safe.
#[must_use]
fn miter_apex(p: [f32; 2], n0: [f32; 2], n1: [f32; 2], delta: f32, dot: f32) -> [f32; 2] {
    let scale = delta / (1.0 + dot);
    [p[0] + scale * (n0[0] + n1[0]), p[1] + scale * (n0[1] + n1[1])]
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

/// Absolute epsilon under which a mesh node is considered coincident with its
/// identity position. A mesh whose every node is within this of identity is
/// treated as a no-op so it renders byte-identically to `None`.
const MESH_IDENTITY_EPS: f32 = 1e-6;

/// Prepared per-render context for applying a [`VectorMeshWarp`] at the
/// rasterizer vertex seam.
///
/// Owns a copy of the mesh nodes plus the normalization box (pre-warp,
/// pre-global-rotation layout AABB) and the global rotation to peel/reapply.
/// Build it ONCE per render via [`MeshWarpContext::new`]; it is `None` (so the
/// caller uses the byte-identical fast path) for an invalid mesh, a degenerate
/// box, or an identity mesh.
///
/// Order at the seam: the incoming `world` point already carries per-glyph
/// placement AND the global rotation. `warp_world` PEELS the global rotation
/// (`R^-1` about the centroid) to reach pre-global-rotation layout space, warps
/// there over `box`, then RE-APPLIES the rotation (`R` about the centroid). When
/// there is no global rotation the peel/reapply are skipped entirely.
#[derive(Debug, Clone)]
pub(crate) struct MeshWarpContext {
    cols: usize,
    rows: usize,
    /// Warped normalized node positions, row-major, `len == cols * rows`.
    points_norm: Vec<[f32; 2]>,
    /// Minimum corner of the normalization box in layout space.
    box_min: [f32; 2],
    /// Size of the normalization box `[w, h]`; both strictly positive.
    box_size: [f32; 2],
    /// Global-rotation pivot (centroid) in layout space.
    centroid: [f32; 2],
    /// `cos`/`sin` of the POSITIVE global rotation angle (peel negates `sin`).
    cos: f32,
    sin: f32,
    /// Whether a non-zero global rotation must be peeled/reapplied.
    has_rotation: bool,
}

impl MeshWarpContext {
    /// Build a warp context, or `None` when the warp must be a no-op.
    ///
    /// `box_min`/`box_size` are the pre-warp, pre-global-rotation layout AABB
    /// (`box_size` components must be finite and `> 0`). `global_rotation_rad`
    /// and `centroid` describe the global rotation `R` that the placement pass
    /// has already baked into each glyph's world transform, so the seam can peel
    /// it, warp in layout space, and reapply it. Returns `None` for an invalid
    /// mesh (`cols < 2`, `rows < 2`, or `points_norm.len() != cols*rows`), a
    /// degenerate/non-finite box, a mesh with any non-finite node, or a mesh that
    /// equals identity within [`MESH_IDENTITY_EPS`] (all render byte-identically
    /// to `None`).
    #[must_use]
    pub(crate) fn new(
        warp: &VectorMeshWarp,
        box_min: [f32; 2],
        box_size: [f32; 2],
        global_rotation_rad: f32,
        centroid: [f32; 2],
    ) -> Option<Self> {
        let (cols, rows) = (warp.cols, warp.rows);
        if cols < 2 || rows < 2 || warp.points_norm.len() != cols.checked_mul(rows)? {
            return None;
        }
        // Design B: honor stored source dims as the normalization-box SIZE when both are valid
        // (finite and > 0), keeping the passed pre-warp content-bounds ORIGIN (`box_min`). This
        // makes the canvas authoring UI (which normalizes handle positions against the stored src
        // dims) and the renderer agree. Absent/0 -> keep the live pre-warp-bounds `box_size`.
        let box_size = if warp.src_width_px.is_finite()
            && warp.src_height_px.is_finite()
            && warp.src_width_px > 0.0
            && warp.src_height_px > 0.0
        {
            [warp.src_width_px, warp.src_height_px]
        } else {
            box_size
        };
        if !box_size[0].is_finite()
            || !box_size[1].is_finite()
            || box_size[0] <= 0.0
            || box_size[1] <= 0.0
            || !box_min[0].is_finite()
            || !box_min[1].is_finite()
        {
            return None;
        }
        // A non-finite (NaN/inf) node would denormalize into a garbage coverage sample for its cell at
        // the rasterizer seam, so reject the whole warp and fall back to the byte-identical no-warp path.
        if warp
            .points_norm
            .iter()
            .any(|p| !p[0].is_finite() || !p[1].is_finite())
        {
            return None;
        }
        if mesh_is_identity(warp) {
            return None;
        }
        let (sin, cos) = global_rotation_rad.sin_cos();
        Some(Self {
            cols,
            rows,
            points_norm: warp.points_norm.clone(),
            box_min,
            box_size,
            centroid,
            cos,
            sin,
            has_rotation: global_rotation_rad != 0.0,
        })
    }

    /// Warp one layout-space `world` point: peel global rotation, warp over the
    /// box, reapply global rotation. Never panics (indices are clamped).
    #[must_use]
    fn warp_world(&self, world: [f32; 2]) -> [f32; 2] {
        // Peel R^-1 (rotate by -angle about the centroid) to pre-rotation space.
        let pre = if self.has_rotation {
            rotate_about(world, self.centroid, self.cos, -self.sin)
        } else {
            world
        };
        let warped = self.warp_in_box(pre);
        // Reapply R (rotate by +angle about the centroid).
        if self.has_rotation {
            rotate_about(warped, self.centroid, self.cos, self.sin)
        } else {
            warped
        }
    }

    /// Bilinearly warp a point already in pre-global-rotation layout space.
    ///
    /// Normalizes over the box, clamps lattice coordinates into the grid (points
    /// outside `[0, 1]` clamp to the edge cell), and denormalizes the
    /// interpolated normalized node value back to layout space.
    #[must_use]
    fn warp_in_box(&self, p: [f32; 2]) -> [f32; 2] {
        let nx = (p[0] - self.box_min[0]) / self.box_size[0];
        let ny = (p[1] - self.box_min[1]) / self.box_size[1];
        // Lattice coords, clamped so a point outside the box maps to the edge cell.
        let gx = (nx * (self.cols - 1) as f32).clamp(0.0, (self.cols - 1) as f32);
        let gy = (ny * (self.rows - 1) as f32).clamp(0.0, (self.rows - 1) as f32);
        // Cell base index clamped to the last full cell so the +1 neighbor exists.
        let j0 = (gx.floor() as usize).min(self.cols - 2);
        let i0 = (gy.floor() as usize).min(self.rows - 2);
        let fx = (gx - j0 as f32).clamp(0.0, 1.0);
        let fy = (gy - i0 as f32).clamp(0.0, 1.0);
        let p00 = self.points_norm[i0 * self.cols + j0];
        let p10 = self.points_norm[i0 * self.cols + j0 + 1];
        let p01 = self.points_norm[(i0 + 1) * self.cols + j0];
        let p11 = self.points_norm[(i0 + 1) * self.cols + j0 + 1];
        let wu = lerp(lerp(p00[0], p10[0], fx), lerp(p01[0], p11[0], fx), fy);
        let wv = lerp(lerp(p00[1], p10[1], fx), lerp(p01[1], p11[1], fx), fy);
        [
            self.box_min[0] + wu * self.box_size[0],
            self.box_min[1] + wv * self.box_size[1],
        ]
    }

    /// Invoke `f(x, y)` for the warped+rotated world position of every lattice
    /// node. Because bilinear interpolation of a cell stays within the AABB of
    /// its four warped node values (each output coordinate is a convex
    /// combination of the corner values), the AABB of all these points bounds
    /// the warped image of the whole box — hence of all glyph ink inside it. The
    /// caller unions them to grow the canvas so a strong outward warp never
    /// clips.
    pub(crate) fn for_each_warped_bound_point(&self, mut f: impl FnMut(f32, f32)) {
        for node in &self.points_norm {
            let pre = [
                self.box_min[0] + node[0] * self.box_size[0],
                self.box_min[1] + node[1] * self.box_size[1],
            ];
            let world = if self.has_rotation {
                rotate_about(pre, self.centroid, self.cos, self.sin)
            } else {
                pre
            };
            f(world[0], world[1]);
        }
    }
}

/// Linear interpolation `a + (b - a) * t`.
#[must_use]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Rotate `pt` about `center` by the angle whose cosine/sine are `cos`/`sin`.
///
/// Standard screen (y-down) matrix `[cos -sin; sin cos]`. Pass a negated `sin`
/// to rotate by the opposite angle (used to peel the global rotation).
#[must_use]
fn rotate_about(pt: [f32; 2], center: [f32; 2], cos: f32, sin: f32) -> [f32; 2] {
    let dx = pt[0] - center[0];
    let dy = pt[1] - center[1];
    [
        center[0] + dx * cos - dy * sin,
        center[1] + dx * sin + dy * cos,
    ]
}

/// Whether every mesh node coincides with its identity normalized position
/// within [`MESH_IDENTITY_EPS`], making the warp a no-op.
///
/// Node `(i, j)` has identity position `(j/(cols-1), i/(rows-1))`. Assumes
/// `cols >= 2`, `rows >= 2`, and `points_norm.len() == cols*rows` (checked by
/// [`MeshWarpContext::new`] before this is called).
#[must_use]
fn mesh_is_identity(warp: &VectorMeshWarp) -> bool {
    let inv_cols = 1.0 / (warp.cols - 1) as f32;
    let inv_rows = 1.0 / (warp.rows - 1) as f32;
    for i in 0..warp.rows {
        for j in 0..warp.cols {
            let node = warp.points_norm[i * warp.cols + j];
            let id_x = j as f32 * inv_cols;
            let id_y = i as f32 * inv_rows;
            if (node[0] - id_x).abs() > MESH_IDENTITY_EPS
                || (node[1] - id_y).abs() > MESH_IDENTITY_EPS
            {
                return false;
            }
        }
    }
    true
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
///
/// `scratch` supplies the reused per-glyph working buffers (subpaths, zeno
/// commands, coverage mask). It is reset per call (see [`RasterScratch`]) so the
/// result is byte-identical to a freshly allocated render regardless of what the
/// previous glyph left in it.
///
/// `warp` optionally deforms each vertex AFTER `transform` (which already
/// carries per-glyph placement and any global rotation) but BEFORE the mapping
/// to canvas pixels: `world` is peeled of the global rotation, warped in
/// pre-global-rotation layout space, and re-rotated (see [`MeshWarpContext`]).
/// `None` keeps the vertex path byte-for-byte identical to the un-warped
/// renderer (no extra float work runs).
// The rasterizer call site naturally carries the scratch, canvas target, origin,
// outline, transform, tint, the AA transfer table and the optional mesh warp;
// splitting them would obscure the mapping.
#[allow(clippy::too_many_arguments)]
pub(crate) fn rasterize_outline_into(
    scratch: &mut RasterScratch,
    canvas: &mut [u8],
    canvas_w: usize,
    canvas_h: usize,
    origin_x: f32,
    origin_y: f32,
    outline: &Outline,
    transform: &GlyphTransform,
    color: [u8; 4],
    aa_lut: &[u8; 256],
    warp: Option<&MeshWarpContext>,
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

    // Reset the scratch for this glyph: reclaim the previous glyph's inner Vecs
    // and clear the command buffer, all without freeing capacity.
    scratch.begin_glyph();

    // Transform every subpath point to canvas space (world - origin) and track
    // the path bounding box in canvas coordinates. Inner point Vecs are pulled
    // from the scratch pool so no per-glyph allocation happens on reuse.
    let mut min = [f32::INFINITY, f32::INFINITY];
    let mut max = [f32::NEG_INFINITY, f32::NEG_INFINITY];
    for subpath in &outline.subpaths {
        let mut world_pts = scratch.take_subpath();
        for &local in subpath {
            let placed = transform.apply(local, cos, sin);
            // Optional mesh warp deforms the placed vertex in layout space
            // (peel global rotation -> warp -> reapply); `None` is a no-op that
            // keeps this the byte-identical fast path.
            let world = match warp {
                Some(ctx) => ctx.warp_world(placed),
                None => placed,
            };
            let cx = world[0] - origin_x;
            let cy = world[1] - origin_y;
            min[0] = min[0].min(cx);
            min[1] = min[1].min(cy);
            max[0] = max[0].max(cx);
            max[1] = max[1].max(cy);
            world_pts.push([cx, cy]);
        }
        if world_pts.is_empty() {
            // Return the unused Vec to the pool so its capacity is not lost.
            scratch.subpath_pool.push(world_pts);
        } else {
            scratch.canvas_subpaths.push(world_pts);
        }
    }
    if scratch.canvas_subpaths.is_empty() || !min[0].is_finite() {
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
    // `commands` was already cleared by `begin_glyph`; rebuild disjoint from the
    // subpath buffer (both are distinct fields of `scratch`).
    for subpath in &scratch.canvas_subpaths {
        for (i, p) in subpath.iter().enumerate() {
            let pt = Vector::new(p[0] - shift_x, p[1] - shift_y);
            if i == 0 {
                scratch.commands.push(Command::MoveTo(pt));
            } else {
                scratch.commands.push(Command::LineTo(pt));
            }
        }
        scratch.commands.push(Command::Close);
    }

    let (Ok(mask_w_u32), Ok(mask_h_u32)) = (u32::try_from(mask_w), u32::try_from(mask_h)) else {
        return;
    };
    // Resize and fully zero the coverage mask: `clear` + `resize(_, 0)` leaves
    // every cell 0 (fresh-allocation semantics) while retaining capacity, which
    // `render_into` requires because it only writes covered cells.
    let coverage_len = mask_w.saturating_mul(mask_h);
    scratch.coverage.clear();
    scratch.coverage.resize(coverage_len, 0);
    Mask::new(&scratch.commands[..])
        .style(outline.winding.to_zeno())
        .format(Format::Alpha)
        .size(mask_w_u32, mask_h_u32)
        .render_into(&mut scratch.coverage, None);

    // Monochrome tint contract (doc 4.1): RGB replaced by color, alpha scaled by
    // coverage and the color's alpha. Identical math to raster::sample_swash_pixel.
    let tint_alpha = f32::from(color[3]) / 255.0;
    for my in 0..mask_h {
        let canvas_y = win_min_y + my;
        for mx in 0..mask_w {
            // Map raw coverage through the AA transfer table before tinting.
            let cov = aa_lut[usize::from(scratch.coverage[my * mask_w + mx])];
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

    /// Extract an outline through a throwaway scaler context. Tests do not need
    /// to reuse the context across calls, unlike `OutlineCache`.
    fn extract_outline(font: &swash::FontRef, glyph_id: u16, em_px: f32) -> Option<Outline> {
        let mut ctx = swash::scale::ScaleContext::new();
        extract_glyph_outline(&mut ctx, font, glyph_id, em_px)
    }

    /// Load the shared Latin+Cyrillic test face bytes.
    fn load_test_font_bytes() -> Vec<u8> {
        // Fixture lives at the workspace root; this crate sits two levels down
        // (crates/ms-text-render), so anchor CARGO_MANIFEST_DIR up two dirs.
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../test/PanelCleaner/pcleaner/data/LiberationSans-Regular.ttf");
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
        let outline = extract_outline(&font, gid, em).expect("H has an outline");
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
            extract_outline(&font, gid, 64.0).is_none(),
            "space glyph has no fillable outline"
        );
    }

    #[test]
    fn extract_rejects_bad_em() {
        let data = load_test_font_bytes();
        let font = swash::FontRef::from_index(&data, 0).expect("valid font");
        let gid = glyph_for_char(&font, 'H');
        assert!(extract_outline(&font, gid, 0.0).is_none());
        assert!(extract_outline(&font, gid, -5.0).is_none());
        assert!(extract_outline(&font, gid, f32::NAN).is_none());
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
        let outline = extract_outline(&font, gid, em).expect("R outline");
        let mut canvas = vec![0u8; ref_w * ref_h * 4];
        let mut scratch = RasterScratch::new();
        rasterize_outline_into(
            &mut scratch,
            &mut canvas,
            ref_w,
            ref_h,
            placement.left as f32,
            -(placement.top as f32),
            &outline,
            &GlyphTransform::identity(),
            [255, 255, 255, 255],
            &identity_lut(),
            None,
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
        let outline = extract_outline(&font, gid, em).expect("H outline");
        let (min, max) = outline.local_bbox();
        let w = (max[0] - min[0]).ceil() as usize + 4;
        let h = (max[1] - min[1]).ceil() as usize + 4;

        // Opaque red: covered pixels are exactly the tint RGB. One scratch is
        // reused for both renders below to also exercise scratch reuse.
        let mut scratch = RasterScratch::new();
        let mut canvas_full = vec![0u8; w * h * 4];
        rasterize_outline_into(
            &mut scratch,
            &mut canvas_full,
            w,
            h,
            min[0] - 2.0,
            min[1] - 2.0,
            &outline,
            &GlyphTransform::identity(),
            [200, 30, 30, 255],
            &identity_lut(),
            None,
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
            &mut scratch,
            &mut canvas_half,
            w,
            h,
            min[0] - 2.0,
            min[1] - 2.0,
            &outline,
            &GlyphTransform::identity(),
            [200, 30, 30, 128],
            &identity_lut(),
            None,
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
    fn shared_scratch_matches_fresh_scratch() {
        // Reusing one `RasterScratch` must leave no stale state: rendering glyph
        // B through a scratch already dirtied by glyph A must be byte-identical to
        // rendering B through a brand-new scratch.
        let data = load_test_font_bytes();
        let font = swash::FontRef::from_index(&data, 0).expect("valid font");
        let em = 64.0;
        let outline_a = extract_outline(&font, glyph_for_char(&font, 'A'), em).expect("A outline");
        let outline_b = extract_outline(&font, glyph_for_char(&font, 'B'), em).expect("B outline");
        let (min, max) = outline_b.local_bbox();
        let w = (max[0] - min[0]).ceil() as usize + 6;
        let h = (max[1] - min[1]).ceil() as usize + 6;
        let origin_x = min[0] - 3.0;
        let origin_y = min[1] - 3.0;
        let lut = identity_lut();

        // Reference: render B into a fresh scratch.
        let mut fresh_scratch = RasterScratch::new();
        let mut fresh_canvas = vec![0u8; w * h * 4];
        rasterize_outline_into(
            &mut fresh_scratch,
            &mut fresh_canvas,
            w,
            h,
            origin_x,
            origin_y,
            &outline_b,
            &GlyphTransform::identity(),
            [255, 255, 255, 255],
            &lut,
            None,
        );

        // Shared scratch: dirty it with A (different bbox/window), then render B.
        let mut shared_scratch = RasterScratch::new();
        let mut dirty_canvas = vec![0u8; w * h * 4];
        rasterize_outline_into(
            &mut shared_scratch,
            &mut dirty_canvas,
            w,
            h,
            origin_x,
            origin_y,
            &outline_a,
            &GlyphTransform::identity(),
            [255, 255, 255, 255],
            &lut,
            None,
        );
        let mut reused_canvas = vec![0u8; w * h * 4];
        rasterize_outline_into(
            &mut shared_scratch,
            &mut reused_canvas,
            w,
            h,
            origin_x,
            origin_y,
            &outline_b,
            &GlyphTransform::identity(),
            [255, 255, 255, 255],
            &lut,
            None,
        );

        assert_eq!(
            fresh_canvas, reused_canvas,
            "scratch reuse must be byte-identical to a fresh-scratch render"
        );
    }

    #[test]
    fn rotation_swaps_bbox_and_offcanvas_is_safe() {
        let data = load_test_font_bytes();
        let font = swash::FontRef::from_index(&data, 0).expect("valid font");
        let gid = glyph_for_char(&font, 'L');
        let em = 64.0;
        let outline = extract_outline(&font, gid, em).expect("L outline");
        let (min, max) = outline.local_bbox();
        let ow = max[0] - min[0];
        let oh = max[1] - min[1];

        // Identity render: measure the alpha bbox. One scratch is reused across
        // all three renders in this test to exercise reuse.
        let mut scratch = RasterScratch::new();
        let big = 200usize;
        let mut c_id = vec![0u8; big * big * 4];
        rasterize_outline_into(
            &mut scratch,
            &mut c_id,
            big,
            big,
            min[0] - 20.0,
            min[1] - 20.0,
            &outline,
            &GlyphTransform::identity(),
            [255, 255, 255, 255],
            &identity_lut(),
            None,
        );
        let (idw, idh) = alpha_bbox_dims(&c_id, big, big);

        // 90-degree rotation about the origin: width/height of the ink swap.
        let rot = GlyphTransform {
            pos: [0.0, 0.0],
            rot: std::f32::consts::FRAC_PI_2,
            scale: [1.0, 1.0],
            shear_x: 0.0,
        };
        // After rot, local x-extent maps to y and vice versa; choose an origin
        // that keeps the rotated ink on-canvas.
        let mut c_rot = vec![0u8; big * big * 4];
        rasterize_outline_into(
            &mut scratch,
            &mut c_rot,
            big,
            big,
            -(max[1]) - 20.0,
            min[0] - 20.0,
            &outline,
            &rot,
            [255, 255, 255, 255],
            &identity_lut(),
            None,
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
            shear_x: 0.0,
        };
        rasterize_outline_into(
            &mut scratch,
            &mut c_off,
            big,
            big,
            0.0,
            0.0,
            &outline,
            &far,
            [255, 255, 255, 255],
            &identity_lut(),
            None,
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
        let outline = extract_outline(&font, gid, em).expect("H outline");
        let (min, max) = outline.local_bbox();
        // Pad so the shifted ink never clips the canvas edge on any axis.
        let w = (max[0] - min[0]).ceil() as usize + 8;
        let h = (max[1] - min[1]).ceil() as usize + 8;
        let origin_x = min[0] - 4.0;
        let origin_y = min[1] - 4.0;

        let render_at = |pos: [f32; 2]| -> (f32, f32) {
            let mut canvas = vec![0u8; w * h * 4];
            let mut scratch = RasterScratch::new();
            rasterize_outline_into(
                &mut scratch,
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
                    shear_x: 0.0,
                },
                [255, 255, 255, 255],
                &identity_lut(),
                None,
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
        let o_outline = extract_outline(&font, glyph_for_char(&font, 'O'), em)
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
        let colon_outline = extract_outline(&font, glyph_for_char(&font, ':'), em)
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

    /// Build a `cols x rows` identity mesh (every node at its identity position).
    ///
    /// `src_*` are left at `0.0` so these helpers exercise the LIVE pre-warp-bounds
    /// normalization path (the passed `box_size`); the explicit-src-dims Design B
    /// path is covered separately by `mesh_context_honors_explicit_src_dims`.
    fn identity_mesh(cols: usize, rows: usize) -> VectorMeshWarp {
        let mut points_norm = Vec::with_capacity(cols * rows);
        for i in 0..rows {
            for j in 0..cols {
                points_norm.push([j as f32 / (cols - 1) as f32, i as f32 / (rows - 1) as f32]);
            }
        }
        VectorMeshWarp {
            cols,
            rows,
            src_width_px: 0.0,
            src_height_px: 0.0,
            points_norm,
        }
    }

    /// Build a mesh whose every node is shifted from identity by a constant
    /// normalized offset `dn` (a pure translation-equivalent mesh).
    fn translation_mesh(cols: usize, rows: usize, dn: [f32; 2]) -> VectorMeshWarp {
        let mut warp = identity_mesh(cols, rows);
        for node in &mut warp.points_norm {
            node[0] += dn[0];
            node[1] += dn[1];
        }
        warp
    }

    #[test]
    fn mesh_context_is_none_for_identity_and_invalid() {
        // Identity mesh -> no-op (None) so it renders byte-identically to no warp.
        let identity = identity_mesh(13, 13);
        assert!(
            MeshWarpContext::new(&identity, [0.0, 0.0], [100.0, 80.0], 0.0, [0.0, 0.0]).is_none(),
            "identity mesh must produce a None (no-op) context"
        );
        // Degenerate box -> None.
        let shifted = translation_mesh(13, 13, [0.1, 0.0]);
        assert!(
            MeshWarpContext::new(&shifted, [0.0, 0.0], [0.0, 80.0], 0.0, [0.0, 0.0]).is_none(),
            "zero-width box must produce None"
        );
        // Wrong point count / too few columns -> None.
        let bad_len = VectorMeshWarp {
            cols: 3,
            rows: 3,
            src_width_px: 10.0,
            src_height_px: 10.0,
            points_norm: vec![[0.0, 0.0]; 8],
        };
        assert!(MeshWarpContext::new(&bad_len, [0.0, 0.0], [10.0, 10.0], 0.0, [0.0, 0.0]).is_none());
    }

    #[test]
    fn mesh_context_is_none_for_non_finite_node() {
        // A NaN node would inject garbage coverage into its cell -> reject the whole warp.
        let mut nan_node = translation_mesh(13, 13, [0.1, 0.0]);
        nan_node.points_norm[42][0] = f32::NAN;
        assert!(
            MeshWarpContext::new(&nan_node, [0.0, 0.0], [100.0, 80.0], 0.0, [0.0, 0.0]).is_none(),
            "a NaN node must produce a None (no-op) context"
        );
        // An infinite node in the other axis is rejected the same way.
        let mut inf_node = translation_mesh(13, 13, [0.0, 0.1]);
        inf_node.points_norm[7][1] = f32::INFINITY;
        assert!(
            MeshWarpContext::new(&inf_node, [0.0, 0.0], [100.0, 80.0], 0.0, [0.0, 0.0]).is_none(),
            "an infinite node must produce a None (no-op) context"
        );
    }

    #[test]
    fn warp_translation_mesh_shifts_points_uniformly() {
        // A pure translation mesh must map every interior point by a constant
        // world offset `dn * box_size` (bilinear reproduces the linear coordinate
        // plus the constant node offset), leaving relative geometry unchanged.
        let dn = [0.1, -0.2];
        let warp = translation_mesh(5, 5, dn);
        let box_min = [5.0, 7.0];
        let box_size = [40.0, 20.0];
        let ctx = MeshWarpContext::new(&warp, box_min, box_size, 0.0, [0.0, 0.0])
            .expect("translation mesh is active (non-identity)");
        let expected = [dn[0] * box_size[0], dn[1] * box_size[1]];
        for p in [[15.0, 12.0], [40.0, 20.0], [6.0, 26.0]] {
            let w = ctx.warp_world(p);
            assert!(
                (w[0] - (p[0] + expected[0])).abs() < 1e-3,
                "x shift mismatch at {p:?}: got {w:?}"
            );
            assert!(
                (w[1] - (p[1] + expected[1])).abs() < 1e-3,
                "y shift mismatch at {p:?}: got {w:?}"
            );
        }
    }

    #[test]
    fn warp_clamps_points_outside_the_box_to_the_edge() {
        // Points far outside the box clamp to the edge cell (no extrapolation), so
        // two different far-left points map to the same warped x.
        let warp = translation_mesh(4, 4, [0.15, 0.05]);
        let box_min = [0.0, 0.0];
        let box_size = [100.0, 100.0];
        let ctx = MeshWarpContext::new(&warp, box_min, box_size, 0.0, [0.0, 0.0]).expect("active");
        let a = ctx.warp_world([-500.0, 50.0]);
        let b = ctx.warp_world([-50.0, 50.0]);
        assert!(
            (a[0] - b[0]).abs() < 1e-3,
            "far-left points must clamp to the same edge x: {a:?} vs {b:?}"
        );
    }

    #[test]
    fn mesh_context_honors_explicit_src_dims() {
        // Design B: a mesh carrying valid src_width/height_px normalizes over THOSE dims (as the box
        // SIZE), keeping the passed content-bounds origin — regardless of the live `box_size` passed
        // by the caller. A pure translation mesh then shifts every point by `dn * src_dims`.
        let dn = [0.1, -0.2];
        let mut warp = translation_mesh(5, 5, dn);
        warp.src_width_px = 100.0;
        warp.src_height_px = 80.0;
        let box_min = [5.0, 7.0];
        // Deliberately different from the src dims: the override must win.
        let live_box = [40.0, 20.0];
        let ctx = MeshWarpContext::new(&warp, box_min, live_box, 0.0, [0.0, 0.0])
            .expect("translation mesh is active (non-identity)");
        assert_eq!(
            ctx.box_size,
            [100.0, 80.0],
            "explicit src dims must override the live box size"
        );
        assert_eq!(ctx.box_min, box_min, "origin must stay the passed content-bounds origin");
        let expected = [dn[0] * 100.0, dn[1] * 80.0];
        let p = [20.0, 15.0];
        let w = ctx.warp_world(p);
        assert!(
            (w[0] - (p[0] + expected[0])).abs() < 1e-3,
            "x shift must use src dims: got {w:?}"
        );
        assert!(
            (w[1] - (p[1] + expected[1])).abs() < 1e-3,
            "y shift must use src dims: got {w:?}"
        );
    }

    /// Build an outline directly from rings (test-only; NonZero fill).
    fn outline_from_rings(rings: Vec<Vec<[f32; 2]>>) -> Outline {
        Outline::from_subpaths(rings, FillRule::NonZero).expect("non-empty outline")
    }

    /// Screen-clockwise (positive shoelace area in the y-down frame) square.
    fn cw_square(x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<[f32; 2]> {
        vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]]
    }

    /// Opposite-orientation (negative-area) square, used as a hole ring.
    fn ccw_square(x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<[f32; 2]> {
        vec![[x0, y0], [x0, y1], [x1, y1], [x1, y0]]
    }

    fn assert_has_vertex_near(ring: &[[f32; 2]], expected: [f32; 2], tol: f32) {
        assert!(
            ring.iter().any(|v| (v[0] - expected[0]).abs() <= tol
                && (v[1] - expected[1]).abs() <= tol),
            "expected a vertex near {expected:?} in {ring:?}"
        );
    }

    #[test]
    fn offset_square_miter_corners_are_exact() {
        let outline = outline_from_rings(vec![cw_square(0.0, 0.0, 10.0, 10.0)]);
        let d = 2.0;
        let offset = offset_outline(&outline, d, true, true);
        assert_eq!(offset.subpaths().len(), 1);
        let ring = &offset.subpaths()[0];
        // Right-angle miter apexes sit exactly d beyond each corner diagonal.
        for corner in [[-2.0, -2.0], [12.0, -2.0], [12.0, 12.0], [-2.0, 12.0]] {
            assert_has_vertex_near(ring, corner, 1e-3);
        }
        let (min, max) = offset.local_bbox();
        assert!((min[0] + 2.0).abs() < 1e-3 && (min[1] + 2.0).abs() < 1e-3);
        assert!((max[0] - 12.0).abs() < 1e-3 && (max[1] - 12.0).abs() < 1e-3);
    }

    #[test]
    fn offset_square_with_hole_respects_outward_only() {
        let outer = cw_square(0.0, 0.0, 20.0, 20.0);
        let hole = ccw_square(5.0, 5.0, 15.0, 15.0);
        let outline = outline_from_rings(vec![outer, hole.clone()]);
        let d = 2.0;

        // outward_only = true: the hole ring is copied VERBATIM (counters kept).
        let kept = offset_outline(&outline, d, true, true);
        assert_eq!(kept.subpaths().len(), 2);
        assert_eq!(kept.subpaths()[1], hole, "hole must be untouched");
        // The outer ring still expanded.
        assert_has_vertex_near(&kept.subpaths()[0], [-2.0, -2.0], 1e-3);

        // outward_only = false: the hole shrinks by d on each side.
        let shrunk = offset_outline(&outline, d, true, false);
        assert_eq!(shrunk.subpaths().len(), 2);
        let hole_ring = &shrunk.subpaths()[1];
        for corner in [[7.0, 7.0], [7.0, 13.0], [13.0, 13.0], [13.0, 7.0]] {
            assert_has_vertex_near(hole_ring, corner, 1e-3);
        }
        // Orientation must be preserved (NonZero hole subtraction relies on it).
        assert!(
            ring_signed_area(hole_ring) < 0.0,
            "hole ring must keep its (negative) orientation"
        );
    }

    #[test]
    fn offset_round_joins_add_arc_vertices_at_convex_corners() {
        let outline = outline_from_rings(vec![cw_square(0.0, 0.0, 10.0, 10.0)]);
        let sharp = offset_outline(&outline, 2.0, true, true);
        let round = offset_outline(&outline, 2.0, false, true);
        assert!(
            round.subpaths()[0].len() > sharp.subpaths()[0].len(),
            "round joins must add arc vertices over miter joins ({} vs {})",
            round.subpaths()[0].len(),
            sharp.subpaths()[0].len()
        );
        // Segment count grows with d (arc step is proportional to the radius).
        let round_small = offset_outline(&outline, 0.1, false, true);
        let round_big = offset_outline(&outline, 8.0, false, true);
        assert!(
            round_big.subpaths()[0].len() > round_small.subpaths()[0].len(),
            "arc segment count must grow with d ({} vs {})",
            round_big.subpaths()[0].len(),
            round_small.subpaths()[0].len()
        );
        // No round-join vertex strays farther than d (+ arc chord tolerance)
        // from the source square boundary.
        let square = cw_square(0.0, 0.0, 10.0, 10.0);
        let mut closed = square.clone();
        closed.push(square[0]);
        for v in &round.subpaths()[0] {
            let dist = dist_to_polyline(*v, &closed);
            assert!(
                dist <= 2.0 + 0.25,
                "round-join vertex {v:?} strays {dist} > d from the boundary"
            );
        }
    }

    #[test]
    fn offset_collapses_counter_narrower_than_two_d() {
        // A 2px-wide slot counter inside solid ink, shrunk by d = 2 per side:
        // the offset ring inverts (flipped signed area). It must be DROPPED
        // (collapsed counter = solid ink), never emitted inverted — an
        // inverted "hole" would ADD ink under the NonZero fill.
        let outline = outline_from_rings(vec![
            cw_square(0.0, 0.0, 20.0, 20.0),
            ccw_square(9.0, 4.0, 11.0, 16.0),
        ]);
        for sharp in [true, false] {
            let offset = offset_outline(&outline, 2.0, sharp, false);
            assert_eq!(
                offset.subpaths().len(),
                1,
                "collapsed counter must be dropped entirely (sharp={sharp})"
            );
            assert!(
                ring_signed_area(&offset.subpaths()[0]) > 0.0,
                "remaining outer ring keeps its orientation (sharp={sharp})"
            );
        }
        // A wide-enough counter survives the same shrink with its orientation
        // intact (round joins exercised on the shrinking side too).
        let survivable = outline_from_rings(vec![
            cw_square(0.0, 0.0, 20.0, 20.0),
            ccw_square(5.0, 5.0, 15.0, 15.0),
        ]);
        let offset = offset_outline(&survivable, 2.0, false, false);
        assert_eq!(offset.subpaths().len(), 2);
        assert!(
            ring_signed_area(&offset.subpaths()[1]) < 0.0,
            "surviving hole keeps its (negative) orientation under round joins"
        );
    }

    #[test]
    fn offset_round_joins_arc_concave_hole_corners_when_shrinking() {
        // An L-shaped counter has one loop-concave corner; shrinking it opens
        // a join gap there, so the round mode must insert arc vertices that
        // the sharp mode replaces with a single miter apex.
        let l_hole = vec![
            [4.0, 4.0],
            [4.0, 16.0],
            [16.0, 16.0],
            [16.0, 12.0],
            [8.0, 12.0],
            [8.0, 4.0],
        ];
        let outline =
            outline_from_rings(vec![cw_square(0.0, 0.0, 20.0, 20.0), l_hole]);
        let sharp = offset_outline(&outline, 1.5, true, false);
        let round = offset_outline(&outline, 1.5, false, false);
        assert_eq!(sharp.subpaths().len(), 2);
        assert_eq!(round.subpaths().len(), 2);
        assert!(
            round.subpaths()[1].len() > sharp.subpaths()[1].len(),
            "round joins must arc the concave hole corner ({} vs {})",
            round.subpaths()[1].len(),
            sharp.subpaths()[1].len()
        );
        // Both keep the hole orientation (no inversion at this size).
        assert!(ring_signed_area(&sharp.subpaths()[1]) < 0.0);
        assert!(ring_signed_area(&round.subpaths()[1]) < 0.0);
    }

    #[test]
    fn winding_classification_is_orientation_convention_free() {
        // Two DISJOINT blobs wound oppositely (larger one "CCW"): with an
        // orientation-relative heuristic the small blob would be misread as a
        // hole of the dominant ring. The NonZero-winding classification sees
        // ink inside BOTH, so both expand outward.
        let outline = outline_from_rings(vec![
            cw_square(0.0, 0.0, 5.0, 5.0),
            ccw_square(10.0, 0.0, 30.0, 20.0),
        ]);
        let offset = offset_outline(&outline, 1.0, true, true);
        assert_eq!(offset.subpaths().len(), 2);
        assert_has_vertex_near(&offset.subpaths()[0], [-1.0, -1.0], 1e-3);
        assert_has_vertex_near(&offset.subpaths()[0], [6.0, 6.0], 1e-3);
        assert_has_vertex_near(&offset.subpaths()[1], [9.0, -1.0], 1e-3);
        assert_has_vertex_near(&offset.subpaths()[1], [31.0, 21.0], 1e-3);
    }

    #[test]
    fn winding_classification_treats_same_wound_nested_ring_as_ink() {
        // A nested ring with the SAME orientation is NOT a counter under
        // NonZero (its interior winds to 2, i.e. it renders as ink), so it
        // must be offset outward even in `outward_only == false` mode.
        let outline = outline_from_rings(vec![
            cw_square(0.0, 0.0, 20.0, 20.0),
            cw_square(5.0, 5.0, 15.0, 15.0),
        ]);
        let offset = offset_outline(&outline, 1.0, true, false);
        assert_eq!(offset.subpaths().len(), 2);
        assert_has_vertex_near(&offset.subpaths()[1], [4.0, 4.0], 1e-3);
        assert_has_vertex_near(&offset.subpaths()[1], [16.0, 16.0], 1e-3);
    }

    #[test]
    fn offset_handles_duplicate_and_collinear_points() {
        // A square ring polluted with duplicated vertices and long collinear
        // runs on every edge must offset to the same miter corners as the
        // clean square, with no NaN/degenerate output.
        let noisy = vec![
            [0.0, 0.0],
            [0.0, 0.0],
            [3.0, 0.0],
            [6.0, 0.0],
            [10.0, 0.0],
            [10.0, 0.0],
            [10.0, 5.0],
            [10.0, 10.0],
            [5.0, 10.0],
            [5.0, 10.0],
            [0.0, 10.0],
            [0.0, 5.0],
        ];
        let offset = offset_outline(&outline_from_rings(vec![noisy]), 2.0, true, true);
        assert_eq!(offset.subpaths().len(), 1);
        for v in &offset.subpaths()[0] {
            assert!(v[0].is_finite() && v[1].is_finite(), "no NaN vertices");
        }
        for corner in [[-2.0, -2.0], [12.0, -2.0], [12.0, 12.0], [-2.0, 12.0]] {
            assert_has_vertex_near(&offset.subpaths()[0], corner, 1e-3);
        }
        let (min, max) = offset.local_bbox();
        assert!((min[0] + 2.0).abs() < 1e-3 && (max[0] - 12.0).abs() < 1e-3);
        assert!((min[1] + 2.0).abs() < 1e-3 && (max[1] - 12.0).abs() < 1e-3);
    }

    #[test]
    fn offset_drops_degenerate_contours() {
        // A tiny sliver (area far below the epsilon) and a 2-point path ride
        // along with a real square: both must be dropped from the offset result.
        let outline = outline_from_rings(vec![
            cw_square(0.0, 0.0, 10.0, 10.0),
            vec![[30.0, 30.0], [30.001, 30.0], [30.001, 30.001]],
            vec![[40.0, 40.0], [41.0, 40.0]],
        ]);
        let offset = offset_outline(&outline, 2.0, true, true);
        assert_eq!(
            offset.subpaths().len(),
            1,
            "degenerate contours must be dropped"
        );
    }

    #[test]
    fn offset_zero_distance_returns_identical_outline() {
        let rings = vec![cw_square(0.0, 0.0, 10.0, 10.0), ccw_square(2.0, 2.0, 8.0, 8.0)];
        let outline = outline_from_rings(rings.clone());
        for d in [0.0, -1.0, 1.0 / 256.0, f32::NAN] {
            let offset = offset_outline(&outline, d, true, false);
            assert_eq!(offset.subpaths(), outline.subpaths(), "d={d} must be a clone");
            assert_eq!(offset.local_bbox(), outline.local_bbox());
        }
    }

    #[test]
    fn outline_cache_keys_faux_variants_separately() {
        let data = load_test_font_bytes();
        let font = swash::FontRef::from_index(&data, 0).expect("valid font");
        let mut cache = OutlineCache::new();
        let gid = glyph_for_char(&font, 'H');
        let key = OutlineKey::new(1, gid, 64.0);

        let plain = cache
            .get_or_extract_with_faux(key, &font, gid, 64.0, None)
            .expect("plain outline");
        let faux = FauxOutlineParams::new(2.0, true, true).expect("valid faux params");
        let bold = cache
            .get_or_extract_with_faux(key, &font, gid, 64.0, Some(faux))
            .expect("faux outline");
        // Distinct entries: the plain variant is untouched, the faux one is
        // wider by ~d on each side.
        assert_eq!(cache.len(), 2, "plain + faux variants must both be cached");
        let (pmin, pmax) = plain.local_bbox();
        let (bmin, bmax) = bold.local_bbox();
        assert!((pmin[0] - bmin[0] - 2.0).abs() < 0.75, "left edge moved by ~d");
        assert!((bmax[0] - pmax[0] - 2.0).abs() < 0.75, "right edge moved by ~d");
        // Repeat lookups hit the same Arcs (per-variant caching).
        let plain_again = cache
            .get_or_extract_with_faux(key, &font, gid, 64.0, None)
            .expect("plain outline");
        let bold_again = cache
            .get_or_extract_with_faux(key, &font, gid, 64.0, Some(faux))
            .expect("faux outline");
        assert!(Arc::ptr_eq(&plain, &plain_again));
        assert!(Arc::ptr_eq(&bold, &bold_again));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn glyph_transform_shear_leans_tops_and_fixes_baseline() {
        // A square above the baseline (y in [-10, 0], y-down local frame).
        let contour = GlyphContour {
            components: vec![vec![[0.0, -10.0], [10.0, -10.0], [10.0, 0.0], [0.0, 0.0]]],
        };
        let transform = GlyphTransform {
            pos: [0.0, 0.0],
            rot: 0.0,
            scale: [1.0, 1.0],
            shear_x: 0.5,
        };
        let placed = transform.place_contour(&contour);
        let ring = &placed.components[0];
        // Baseline vertices (y = 0) are fixed; top vertices (y = -10) lean
        // right by shear_x * 10 = 5 (positive shear leans tops right).
        assert!((ring[3][0] - 0.0).abs() < 1e-5 && (ring[3][1] - 0.0).abs() < 1e-5);
        assert!((ring[0][0] - 5.0).abs() < 1e-5 && (ring[0][1] + 10.0).abs() < 1e-5);
        assert!((ring[1][0] - 15.0).abs() < 1e-5);
    }

    #[test]
    fn warp_bound_points_grow_with_outward_corner() {
        // Pushing the bottom-right node outward must move at least one warped
        // bound point beyond the box's far corner, so the canvas-growth pass sees
        // a larger extent than the identity box.
        let mut warp = identity_mesh(3, 3);
        let last = warp.points_norm.len() - 1;
        warp.points_norm[last] = [1.4, 1.4];
        let box_min = [0.0, 0.0];
        let box_size = [50.0, 50.0];
        let ctx = MeshWarpContext::new(&warp, box_min, box_size, 0.0, [0.0, 0.0]).expect("active");
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        ctx.for_each_warped_bound_point(|x, y| {
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        });
        assert!(max_x > 50.0, "warped extent must exceed the box width, got {max_x}");
        assert!(max_y > 50.0, "warped extent must exceed the box height, got {max_y}");
    }
}
