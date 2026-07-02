/*
File: src/tabs/typing/render_next/glyph_blit.rs

Purpose:
Shared outline-blit helpers for the vector-glyph render paths. Both the
horizontal renderer (`pipeline.rs`) and the on-path / formula / custom-line
composite pass (`formula/render.rs`) resolve a glyph's true font outline and
place it with the same pen-relative pivot, so a glyph rasterized from its
outline lands on exactly the pixels the old bitmap blit would have drawn.

Key functions:
- hash_font_id: stable `u64` identity for a `fontdb::ID` (slotmap key with no
  public numeric accessor) to key the `OutlineCache`.
- resolve_outline_for_glyph: per-render cached outline lookup for a laid-out
  glyph at its shaped em size.
- nominal_glyph_advance_px: font `hmtx` (un-kerned) advance of a glyph, the base
  step for `KerningMode::Fixed` (cosmic-text bakes pair kerning into
  `LayoutGlyph.w`, so the shaped advance cannot express "no pair kerning").
- glyph_outline_transform: local->world `GlyphTransform` reproducing the bitmap
  blit's scale+rotation about the bitmap center (the single source of truth for
  the outline->world pivot).

Notes:
These helpers were lifted out of `formula/render.rs` so the horizontal path can
share them instead of duplicating the pivot math. The extraction em is the
glyph's shaped `font_size` (the exact ppem the bitmap path feeds to swash);
per-glyph scale is applied later as a transform, never baked into the outline.
*/

use super::vector::{GlyphTransform, Outline, OutlineCache, OutlineKey};
use cosmic_text::{CacheKey, FontSystem, LayoutGlyph};
use std::sync::Arc;

/// The per-glyph subpixel fraction (`[x_bin, y_bin]` in device px) that the
/// swash bitmap coverage was baked with, for a physical glyph placement.
///
/// cosmic-text quantizes each glyph's fractional pen position into a 4-way
/// `SubpixelBin` (`{0, 0.25, 0.5, 0.75}` px) and renders the bitmap pre-shifted
/// by it (`Render::offset`), while `physical.x/y` keep only the integer pen. The
/// outline draw sites feed this to [`glyph_outline_transform`] so the vector ink
/// lands on the same pixels as the pre-shifted bitmap coverage. The color-glyph
/// bitmap fallback must NOT use it (the fraction is already in that coverage).
#[must_use]
pub(crate) fn glyph_subpixel_offset(cache_key: CacheKey) -> [f32; 2] {
    [cache_key.x_bin.as_float(), cache_key.y_bin.as_float()]
}

/// Stable `u64` identity for a font id, used to key the [`OutlineCache`].
///
/// The concrete `fontdb::ID` is a slotmap key with no public numeric accessor,
/// so it is hashed to a `u64`. The value is stable within a process run, which
/// is all the per-render cache needs; distinct fonts collide only with
/// negligible probability and would also need the same glyph id and em to alias.
#[must_use]
pub(crate) fn hash_font_id(font_id: impl std::hash::Hash) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    font_id.hash(&mut hasher);
    hasher.finish()
}

/// Resolve a laid-out glyph's true font outline through the per-render cache.
///
/// The extraction em is `glyph.font_size` (the exact ppem the bitmap path feeds
/// to `physical`/`get_image`); the per-glyph scale is applied later as a
/// transform, never baked into the outline. Returns `None` when the font is
/// missing or the glyph has no fillable monochrome outline (space or COLR/bitmap
/// color glyph); callers then fall back to the bitmap blit. Never panics.
#[must_use]
pub(crate) fn resolve_outline_for_glyph(
    font_system: &mut FontSystem,
    outline_cache: &mut OutlineCache,
    glyph: &LayoutGlyph,
) -> Option<Arc<Outline>> {
    let font_id = glyph.font_id;
    let glyph_id = glyph.glyph_id;
    let em_px = glyph.font_size;
    let font = font_system.get_font(font_id)?;
    let key = OutlineKey::new(hash_font_id(font_id), glyph_id, em_px);
    outline_cache.get_or_extract(key, &font.as_swash(), glyph_id, em_px)
}

/// Nominal (un-kerned) horizontal advance of a laid-out glyph in layout px.
///
/// Reads the font's `hmtx` advance for `glyph.glyph_id` scaled to
/// `glyph.font_size`, WITHOUT any GPOS/`kern` pair adjustment. This is the lever
/// `KerningMode::Fixed` needs: cosmic-text bakes pair kerning into
/// `LayoutGlyph.w` (`w == next.x - this.x` by construction), so the shaped
/// advance cannot express "no pair kerning" and the raw metrics table must be
/// consulted instead. Returns `None` when the font is unavailable or the metric
/// is not finite, letting the caller fall back to the shaped advance. Never
/// panics.
#[must_use]
pub(crate) fn nominal_glyph_advance_px(
    font_system: &mut FontSystem,
    glyph: &LayoutGlyph,
) -> Option<f32> {
    let font = font_system.get_font(glyph.font_id)?;
    let advance = font
        .as_swash()
        .glyph_metrics(&[])
        .scale(glyph.font_size)
        .advance_width(glyph.glyph_id);
    advance.is_finite().then_some(advance)
}

/// Build the local->world [`GlyphTransform`] that places a glyph outline on
/// exactly the pixels the bitmap blit drew.
///
/// The blit maps a world pixel to the glyph bitmap by inverse scale+rotation
/// around the bitmap center, which sits at world `dst_center`. In the outline's
/// pen-relative frame (`O = content - pen`, y-down px) that bitmap center is at
/// `O_center = (placement_left + glyph_w/2, glyph_h/2 - placement_top)`, so the
/// transform that sends `O_center` to `dst_center` under `scale` then `rotation`
/// has `pos = dst_center + Rot(rotation) * (-scale * O_center)`. This is the
/// single source of truth shared by the outline rasterizer and the ink contour
/// placement, guaranteeing zero positional shift versus the old bitmap blit.
///
/// `subpixel` is the per-glyph subpixel fraction (`[x_bin, y_bin]` in device px,
/// each in `{0, 0.25, 0.5, 0.75}`) that cosmic-text bakes into the swash BITMAP
/// coverage via `Render::offset` while `physical.x/y` carry only the integer pen.
/// Because the outline is placed from that integer pen, the baked-in shift is
/// re-added here as a box-local translation — scaled with the glyph, then rotated
/// with it — so the rasterized outline lands on exactly the pixels the pre-shifted
/// bitmap coverage occupies. The color-glyph bitmap fallback must pass
/// `subpixel = [0.0, 0.0]`: `get_image` already baked the fraction into that
/// coverage, so re-adding it would double-apply.
// The placement is an irreducible list of independent scalars (bitmap center,
// rotation, per-axis scale, bitmap placement/size, subpixel fraction); bundling
// them into a one-off struct would not add clarity.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub(crate) fn glyph_outline_transform(
    dst_center_x: f32,
    dst_center_y: f32,
    rotation_rad: f32,
    placement_left: f32,
    placement_top: f32,
    glyph_w: f32,
    glyph_h: f32,
    width_mul: f32,
    height_mul: f32,
    subpixel: [f32; 2],
) -> GlyphTransform {
    let (sin_a, cos_a) = rotation_rad.sin_cos();
    // pivot = -scale * O_center (bitmap center in the outline's pen-relative frame).
    let pivot_x = -(placement_left + glyph_w * 0.5) * width_mul;
    let pivot_y = (placement_top - glyph_h * 0.5) * height_mul;
    // Subpixel fraction lives in the same box-local frame as the (scaled) outline
    // points, so scale it with the glyph before it is rotated into world space.
    let offset_x = pivot_x + subpixel[0] * width_mul;
    let offset_y = pivot_y + subpixel[1] * height_mul;
    GlyphTransform {
        pos: [
            dst_center_x + offset_x * cos_a - offset_y * sin_a,
            dst_center_y + offset_x * sin_a + offset_y * cos_a,
        ],
        rot: rotation_rad,
        scale: [width_mul, height_mul],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zero subpixel offset must reproduce the exact pen pivot (no shift versus
    /// the pre-fix placement), and a non-zero offset at identity rotation/scale
    /// must translate `pos` by exactly that offset.
    #[test]
    fn subpixel_translates_pos_at_identity() {
        let base = glyph_outline_transform(100.0, 50.0, 0.0, 2.0, 3.0, 10.0, 12.0, 1.0, 1.0, [0.0, 0.0]);
        let shifted =
            glyph_outline_transform(100.0, 50.0, 0.0, 2.0, 3.0, 10.0, 12.0, 1.0, 1.0, [0.5, 0.75]);
        assert!((shifted.pos[0] - base.pos[0] - 0.5).abs() < 1e-5);
        assert!((shifted.pos[1] - base.pos[1] - 0.75).abs() < 1e-5);
    }

    /// Under non-unit scale the subpixel fraction scales with the glyph (matching
    /// the bitmap path, which scales the whole pre-shifted bitmap).
    #[test]
    fn subpixel_scales_with_glyph() {
        let base =
            glyph_outline_transform(0.0, 0.0, 0.0, 0.0, 0.0, 8.0, 8.0, 2.0, 3.0, [0.0, 0.0]);
        let shifted =
            glyph_outline_transform(0.0, 0.0, 0.0, 0.0, 0.0, 8.0, 8.0, 2.0, 3.0, [0.5, 0.5]);
        assert!((shifted.pos[0] - base.pos[0] - 0.5 * 2.0).abs() < 1e-5);
        assert!((shifted.pos[1] - base.pos[1] - 0.5 * 3.0).abs() < 1e-5);
    }

    /// Under rotation the subpixel translation is rotated with the glyph, so a
    /// pure x fraction at 90 degrees moves `pos` along +y.
    #[test]
    fn subpixel_rotates_with_glyph() {
        let rot = std::f32::consts::FRAC_PI_2;
        let base = glyph_outline_transform(0.0, 0.0, rot, 0.0, 0.0, 8.0, 8.0, 1.0, 1.0, [0.0, 0.0]);
        let shifted =
            glyph_outline_transform(0.0, 0.0, rot, 0.0, 0.0, 8.0, 8.0, 1.0, 1.0, [1.0, 0.0]);
        assert!((shifted.pos[0] - base.pos[0]).abs() < 1e-5, "x ~ unchanged at 90deg");
        assert!((shifted.pos[1] - base.pos[1] - 1.0).abs() < 1e-5, "x fraction maps to +y");
    }
}
