/*
File: crates/ms-text-render/src/formula/render/seed_ink.rs

Purpose:
Glyph ink on the path for the MinimumPreviousDistance mode: the cached outline contour
of a seed, its placement for a transform (same `glyph_outline_transform` pivot as the
rasterizer), the on-path / straight-reference transforms, and the pair measure on the
chord between two inked glyphs plus the omnidirectional clearance floor.

Key functions:
- seed_ink_geometry(), place_seed_ink_for_transform() / _at() / _straight()
- on_path_transform_from_sample(), straight_reference_transform(), drawn_line_transform_at()
- pair_chord_axis(), on_path_pair_gap(), clears_floor(), pair_clearance_floor(),
  placed_contour_for_transform(), `OnPathInkCaches` (impl)
*/

use crate::drawn_lines::DrawnLinePath;
use crate::glyph_blit::{GlyphBox, glyph_outline_transform, glyph_subpixel_offset};
use crate::glyph_contour::{GlyphContour, PlacedContour, min_placed_distance, placed_aabb_gap};
use crate::vector::{OutlineCache, glyph_contour_from_outline};
use crate::pair_gap::{GapAxis, directional_pair_gap};
use crate::types::TextRenderParams;
use cosmic_text::{FontSystem, SwashCache};
use std::collections::HashMap;
use super::{
    CONTOUR_SIMPLIFY_TOLERANCE_PX, CachedGlyphInk, CustomLineLayoutSettings, DrawnLineTransform,
    FormulaGlyphSeed, LinePlacementBand, ON_PATH_INK_CLEARANCE_FLOOR_PX, OnPathInkCaches,
    PAIR_CHORD_MIN_LEN_PX, PlacedGlyphInk, SeedInkGeometry, drawn_line_glyph_destination_center_raw,
    resolve_glyph_outline,
};
use super::path_geometry::{sample_drawn_line_path_for_direction, vector_line_flip_rotation};

impl OnPathInkCaches {
    /// Three empty caches.
    pub(super) fn new() -> Self {
        Self {
            swash: SwashCache::new(),
            ink: HashMap::new(),
            outlines: OutlineCache::new(),
        }
    }
}

/// Build (or reuse) the ink geometry for a seed's glyph.
///
/// The ink contour is derived from the glyph's true font outline (cached in
/// `caches.ink` on a miss). Nothing about the ink is measured from the
/// rasterized bitmap: the bitmap is consulted ONLY for its placement box
/// (`placement_left`/`placement_top`, size), which pins the contour to the exact
/// pixels the outline rasterizer draws on. Returns `None` for glyphs with no
/// bitmap (e.g. spaces), zero-size placement, or no monochrome outline (color
/// glyphs), which callers treat as "no ink" and handle with the arc-length
/// fallback.
pub(super) fn seed_ink_geometry(
    seed: &FormulaGlyphSeed,
    font_system: &mut FontSystem,
    caches: &mut OnPathInkCaches,
) -> Option<SeedInkGeometry> {
    let physical = seed.glyph.physical(
        (
            seed.origin_x + seed.glyph_offset_px[0],
            seed.origin_y + seed.glyph_offset_px[1],
        ),
        1.0,
    );
    let key = physical.cache_key;
    // The ink cache is per faux-bold variant (same bits as `OutlineKey`), so a
    // faux span's offset contour never aliases the plain one within a render.
    let ink_key = (key, seed.faux.bold_key_bits());

    // Bitmap placement, copied out before the swash image borrow ends.
    // Only the PLACEMENT is read here: the ink itself comes from the outline
    // below, and the spacing target is measured on that contour, so no
    // bitmap-derived ink extent is needed.
    let placement_left;
    let placement_top;
    let glyph_w;
    let glyph_h;
    {
        let Some(image) = caches.swash.get_image(font_system, key) else {
            return None;
        };
        let gw = image.placement.width as usize;
        let gh = image.placement.height as usize;
        if gw == 0 || gh == 0 {
            return None;
        }
        placement_left = image.placement.left as f32;
        placement_top = image.placement.top as f32;
        glyph_w = gw as f32;
        glyph_h = gh as f32;
    }

    // Trace the ink contour from the true outline once per distinct glyph key
    // (and faux-bold variant). A glyph with no fillable monochrome outline is
    // treated as "no ink".
    if let std::collections::hash_map::Entry::Vacant(entry) = caches.ink.entry(ink_key) {
        let outline = resolve_glyph_outline(seed, font_system, &mut caches.outlines)?;
        let contour = glyph_contour_from_outline(&outline, CONTOUR_SIMPLIFY_TOLERANCE_PX);
        entry.insert(CachedGlyphInk {
            contour,
            glyph_w,
            glyph_h,
            placement_left,
            placement_top,
        });
    }

    let cached = caches.ink.get(&ink_key)?;
    let src_left = physical.x as f32 + placement_left;
    let src_top = physical.y as f32 - placement_top;
    let (_scaled_left, _scaled_top, _scaled_width, scaled_height) =
        seed.glyph_scale
            .scaled_rect(src_left, src_top, glyph_w, glyph_h);
    Some(SeedInkGeometry {
        contour: cached.contour.clone(),
        glyph_w: cached.glyph_w,
        glyph_h: cached.glyph_h,
        placement_left: cached.placement_left,
        placement_top: cached.placement_top,
        scaled_height,
        subpixel: glyph_subpixel_offset(key),
    })
}

/// On-path transform (position + rotation) for a candidate arc-length position.
///
/// Samples the path, then hands the point and tangent to
/// [`on_path_transform_from_sample`], which owns the placement expression itself
/// (normal offset, tangent/static rotation, flip, per-glyph rotation and the
/// rotated glyph offset) and is shared with the straight reference layout. Every
/// blit transform and every trial placement of the ink search comes from here,
/// so a measured contour can never sit anywhere else than the drawn one.
/// Returns `None` if the path cannot be sampled.
pub(super) fn drawn_line_transform_at(
    params: &TextRenderParams,
    seed: &FormulaGlyphSeed,
    path: &DrawnLinePath,
    center_s: f32,
    layout: &CustomLineLayoutSettings,
) -> Option<DrawnLineTransform> {
    let (center_x, center_y, tangent_x, tangent_y) =
        sample_drawn_line_path_for_direction(path, center_s)?;
    Some(on_path_transform_from_sample(
        params, seed, layout, center_x, center_y, tangent_x, tangent_y,
    ))
}

/// The on-path transform for an ALREADY SAMPLED path point and tangent.
///
/// Split out of [`drawn_line_transform_at`] so the straight reference layout
/// ([`straight_reference_transform`]) composes the normal offset, rotation,
/// flip and glyph offset through the very same expression instead of a second
/// copy of it. The tangent need not be unit length; it is normalized here.
fn on_path_transform_from_sample(
    params: &TextRenderParams,
    seed: &FormulaGlyphSeed,
    layout: &CustomLineLayoutSettings,
    center_x: f32,
    center_y: f32,
    tangent_x: f32,
    tangent_y: f32,
) -> DrawnLineTransform {
    let tangent_len = (tangent_x * tangent_x + tangent_y * tangent_y)
        .sqrt()
        .max(1e-6);
    let tangent_x = tangent_x / tangent_len;
    let tangent_y = tangent_y / tangent_len;
    let normal_offset = layout.normal_offset_px;
    let center_x = center_x - tangent_y * normal_offset;
    let center_y = center_y + tangent_x * normal_offset;
    let rotation_rad = (if layout.use_tangent_rotation {
        tangent_y.atan2(tangent_x)
    } else {
        layout.static_rotation_rad
    }) + vector_line_flip_rotation(params, seed.line_idx)
        + seed.extended_offset.glyph_rotation_rad;
    let (sin_a, cos_a) = rotation_rad.sin_cos();
    let center_x = center_x + seed.glyph_offset_px[0] * cos_a - seed.glyph_offset_px[1] * sin_a;
    let center_y = center_y + seed.glyph_offset_px[0] * sin_a + seed.glyph_offset_px[1] * cos_a;
    DrawnLineTransform {
        center_x,
        center_y,
        rotation_rad,
    }
}

/// The transform this glyph would get on a STRAIGHT line along +x, at distance
/// `x` from that line's origin.
///
/// This is the reference layout `MinimumPreviousDistance` measures its target
/// against (see [`drawn_line_seed_transform`]). It runs through the same
/// [`on_path_transform_from_sample`] as the real placement, with the path point
/// `(x, 0)` and the constant tangent `(1, 0)`, so every other setting — normal
/// offset, flip, static vs. tangent rotation, per-glyph rotation and offset —
/// enters the reference exactly as it enters the real placement. On a straight
/// path the reference therefore differs from the real placement by a rigid
/// transform only, so every gap measured in the reference is exactly the gap the
/// arc-length seed would draw there — which is what makes the reference the
/// honest place to read the line's typography off, NOT a claim that the mode is
/// a no-op on a straight line. It is not: the pairs are still normalized onto
/// the line's ONE median gap (see the file header).
fn straight_reference_transform(
    params: &TextRenderParams,
    seed: &FormulaGlyphSeed,
    layout: &CustomLineLayoutSettings,
    x: f32,
) -> DrawnLineTransform {
    on_path_transform_from_sample(params, seed, layout, x, 0.0, 1.0, 0.0)
}

/// Place a seed's cached ink for an already-resolved on-path transform.
///
/// The one place that turns (`SeedInkGeometry`, transform) into a
/// [`PlacedGlyphInk`]: the candidate placements of the search, the straight
/// reference placements and the final stored placement all go through it, so
/// they cannot drift apart.
pub(super) fn place_seed_ink_for_transform(
    seed: &FormulaGlyphSeed,
    geom: &SeedInkGeometry,
    layout: &CustomLineLayoutSettings,
    transform: &DrawnLineTransform,
) -> PlacedGlyphInk {
    let (center_x, center_y) = drawn_line_glyph_destination_center_raw(
        transform,
        geom.scaled_height,
        geom.placement_top * seed.glyph_scale.height_mul,
        layout.line_placement_frac,
        layout.line_placement_reference,
        layout.ascent_scaled,
    );
    let contour = placed_contour_for_transform(
        &geom.contour,
        GlyphBox {
            placement_left: geom.placement_left,
            placement_top: geom.placement_top,
            glyph_w: geom.glyph_w,
            glyph_h: geom.glyph_h,
            scale: seed.glyph_scale,
            subpixel: geom.subpixel,
            shear_x: seed.faux.shear_x,
        },
        geom.scaled_height,
        layout.placement_band(),
        transform,
    );
    PlacedGlyphInk {
        contour,
        center: [center_x, center_y],
    }
}

/// Place a glyph's cached ink into world space for a candidate arc-length
/// position.
///
/// Combines [`drawn_line_transform_at`] with [`place_seed_ink_for_transform`]
/// so a search closure produces the exact ink the blit would draw at
/// `center_s`. Returns `None` when the path cannot be sampled.
pub(super) fn place_seed_ink_at(
    params: &TextRenderParams,
    seed: &FormulaGlyphSeed,
    geom: &SeedInkGeometry,
    path: &DrawnLinePath,
    center_s: f32,
    layout: &CustomLineLayoutSettings,
) -> Option<PlacedGlyphInk> {
    let transform = drawn_line_transform_at(params, seed, path, center_s, layout)?;
    Some(place_seed_ink_for_transform(seed, geom, layout, &transform))
}

/// Place a glyph's cached ink in the STRAIGHT REFERENCE layout at reference
/// coordinate `x` (see [`straight_reference_transform`]).
///
/// Infallible: the reference has no path to sample.
pub(super) fn place_seed_ink_straight(
    params: &TextRenderParams,
    seed: &FormulaGlyphSeed,
    geom: &SeedInkGeometry,
    layout: &CustomLineLayoutSettings,
    x: f32,
) -> PlacedGlyphInk {
    let transform = straight_reference_transform(params, seed, layout, x);
    place_seed_ink_for_transform(seed, geom, layout, &transform)
}

/// Measuring frame of a pair of placed glyphs: the CHORD between their ink
/// placement centers, with `forward` pointing from `prev` to `cur`.
///
/// The chord — not either glyph's path tangent — because the measure must be
/// SYMMETRIC under swapping the pair: measuring "prev against cur" and "cur
/// against prev" has to give one number, or the walk order would change the
/// spacing. On a circular arc the chord is exactly the bisector of the two
/// tangents, and unlike the bisector it does not degenerate as the turn angle
/// grows.
///
/// Returns `None` when the two centers coincide (a path that doubles back
/// exactly onto itself): the pair then has no advance direction at all, and the
/// caller treats that as "unmeasurably close" rather than dividing by zero.
pub(super) fn pair_chord_axis(prev_center: [f32; 2], cur_center: [f32; 2]) -> Option<GapAxis> {
    let dx = cur_center[0] - prev_center[0];
    let dy = cur_center[1] - prev_center[1];
    let len = (dx * dx + dy * dy).sqrt();
    if !len.is_finite() || len < PAIR_CHORD_MIN_LEN_PX {
        return None;
    }
    let forward = [dx / len, dy / len];
    // Perpendicular of `forward`, unit length by construction.
    Some(GapAxis::new(forward, [-forward[1], forward[0]]))
}

/// Directional ink whitespace of a placed pair, measured on its own chord.
///
/// Delegates to `pair_gap::directional_pair_gap`, the crate's single owner of
/// the pair-gap measurement (the third consumer, next to horizontal and
/// vertical optical kerning). Returns `None` only for the degenerate chord of
/// [`pair_chord_axis`]; the metric's own `f32::INFINITY` ("the pair does not
/// face itself at all") is passed through as `Some(INFINITY)`.
pub(super) fn on_path_pair_gap(prev: &PlacedGlyphInk, cur: &PlacedGlyphInk) -> Option<f32> {
    let axis = pair_chord_axis(prev.center, cur.center)?;
    Some(directional_pair_gap(&prev.contour, &cur.contour, axis))
}

/// Whether `cur` keeps at least `floor` omnidirectional clearance from `prev`.
///
/// `floor <= 0.0` is always satisfied (an authored overlap is respected, not
/// undone). The AABB lower bound short-circuits the common case.
pub(super) fn clears_floor(prev: &PlacedContour, cur: &PlacedContour, floor: f32) -> bool {
    if floor <= 0.0 {
        return true;
    }
    if placed_aabb_gap(prev, cur) >= floor {
        return true;
    }
    min_placed_distance(prev, cur) >= floor
}

/// Omnidirectional clearance a pair must keep on the curve, derived from the
/// straight reference layout.
///
/// [`ON_PATH_INK_CLEARANCE_FLOOR_PX`], except that a pair the straight
/// reference already draws TIGHTER than the floor keeps its own straight-line
/// clearance instead. Without that cap the floor would push apart pairs the
/// font (or a user-authored negative kerning pair) deliberately draws touching,
/// which is exactly what the mode must not do.
pub(super) fn pair_clearance_floor(straight_prev: &PlacedContour, straight_cur: &PlacedContour) -> f32 {
    if placed_aabb_gap(straight_prev, straight_cur) >= ON_PATH_INK_CLEARANCE_FLOOR_PX {
        return ON_PATH_INK_CLEARANCE_FLOOR_PX;
    }
    ON_PATH_INK_CLEARANCE_FLOOR_PX.min(min_placed_distance(straight_prev, straight_cur))
}

/// Map an outline-frame contour into world space using the blit's exact
/// geometry.
///
/// The contour lives in the outline's pen-relative y-down pixel frame (the same
/// frame the outline rasterizer consumes), so this resolves the glyph's world
/// destination center from the on-path transform and reuses
/// [`glyph_outline_transform`] — the single source of truth for the pivot — so
/// the measured contour lands on the exact pixels the outline is rasterized to.
pub(super) fn placed_contour_for_transform(
    contour: &GlyphContour,
    glyph_box: GlyphBox,
    scaled_height: f32,
    band: LinePlacementBand,
    transform: &DrawnLineTransform,
) -> PlacedContour {
    // Same per-glyph scaled top bearing the placement uses to convert baseline->center.
    let placement_top_scaled = glyph_box.placement_top * glyph_box.scale.height_mul;
    let (dst_center_x, dst_center_y) = drawn_line_glyph_destination_center_raw(
        transform,
        scaled_height,
        placement_top_scaled,
        band.frac,
        band.reference,
        band.ascent_scaled,
    );
    // Same subpixel-corrected pivot (and faux-italic shear) the outline
    // rasterizer uses, so the measured contour matches the drawn ink exactly.
    let glyph_transform =
        glyph_outline_transform(glyph_box, [dst_center_x, dst_center_y], transform.rotation_rad);
    glyph_transform.place_contour(contour)
}
