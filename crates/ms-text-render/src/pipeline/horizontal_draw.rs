/*
File: crates/ms-text-render/src/pipeline/horizontal_draw.rs

Purpose:
The normal (unrotated) horizontal glyph pass: collect each glyph ONCE into a
`HorizontalGlyphPlacement` (bitmap placement box, fill outline or RGBA fallback) and
draw it, so the bounds box and the draw pass share one layout pass.

Key functions:
- build_horizontal_placement() / draw_horizontal_placement()
- horizontal_placement_extra_samples(): the extra-info placement-box samples

Notes:
`HorizontalGlyphPlacement` itself is declared in `pipeline/mod.rs`. Monochrome glyphs
are rasterized from outlines through the shared `glyph_blit` pivot; colour glyphs
keep the `raster.rs` bitmap blit.
*/

use crate::glyph_blit::{
    GlyphBox, glyph_needs_bitmap_fallback, glyph_outline_transform, glyph_subpixel_offset,
    resolve_outline_for_glyph,
};
use crate::raster::{
    RgbaCanvasView, RgbaView, SwashGlyphView, build_glyph_rgba_buffer, draw_scaled_glyph_rgba,
    rasterize_unscaled_glyph,
};
use crate::vector::{MeshWarpContext, OutlinePaint, RasterScratch, rasterize_outline_into};
use cosmic_text::{FontSystem, LayoutGlyph};
use super::{GlyphCaches, GlyphStyle, HorizontalGlyphPlacement};
use super::faux::faux_bounds_pads;

impl HorizontalGlyphPlacement {
    /// Whether this placement puts any ink on the canvas.
    ///
    /// `false` only for a glyph whose outline was CONSUMED by a faux thinning
    /// offset: it has no outline AND deliberately no bitmap fallback, yet keeps
    /// a non-zero box. Such a glyph must not vote on the extra-info ink centers
    /// (`ExtraInfoAccumulator`), which report where the ink IS.
    #[must_use]
    pub(super) fn draws_ink(&self) -> bool {
        self.outline.is_some() || self.fallback.is_some()
    }

    /// The run's pen baseline in content space — the anchor of the height scale.
    ///
    /// `src_top_i` is `physical.y - placement.top`, so adding `placement_top`
    /// recovers exactly the integer pen `physical.y` the glyph was shaped at.
    #[must_use]
    fn baseline_y(&self) -> f32 {
        self.src_top_i as f32 + self.placement_top
    }

    /// The scaled glyph rect, height-anchored at the baseline and widened by the
    /// faux bounds pads (exactly the plain baseline-anchored rect when faux is
    /// off — the pads are hard zeros).
    ///
    /// The pads are symmetric, so they do not move the rect's centre: bounds,
    /// extra-info samples and the draw pivot stay on one box.
    #[must_use]
    pub(super) fn padded_scaled_rect(&self) -> (f32, f32, f32, f32) {
        self.scale.scaled_rect_about_baseline(
            self.src_left_i as f32 - self.bounds_pad[0],
            self.src_top_i as f32 - self.bounds_pad[1],
            self.glyph_w as f32 + 2.0 * self.bounds_pad[0],
            self.glyph_h as f32 + 2.0 * self.bounds_pad[1],
            self.baseline_y(),
        )
    }
}

/// Collect one glyph placement for the normal horizontal path.
///
/// Calls `get_image` ONCE to read the bitmap placement box (bounds/pivot source)
/// and resolve the fill outline; returns `None` for glyphs `get_image` cannot
/// produce or for zero-size (space) glyphs, which never contribute to bounds or
/// pixels. `pos` is the glyph pen position in layout pixels (already carrying the
/// inline glyph offset); `style` is its inline scale and faux style.
pub(super) fn build_horizontal_placement(
    font_system: &mut FontSystem,
    caches: &mut GlyphCaches,
    glyph: &LayoutGlyph,
    pos: [f32; 2],
    style: GlyphStyle,
    text_color: [u8; 4],
) -> Option<HorizontalGlyphPlacement> {
    let [pos_x, pos_y] = pos;
    let GlyphStyle { scale, faux } = style;
    let physical = glyph.physical((pos_x, pos_y), 1.0);
    let image = caches.swash.get_image(font_system, physical.cache_key).as_ref()?;
    let glyph_w = image.placement.width as usize;
    let glyph_h = image.placement.height as usize;
    if glyph_w == 0 || glyph_h == 0 {
        return None;
    }
    let placement_left = image.placement.left as f32;
    let placement_top = image.placement.top as f32;
    // Integer content-space top-left, identical to the historical bounds/draw math.
    let src_left_i = physical.x + image.placement.left;
    let src_top_i = physical.y - image.placement.top;
    let subpixel = glyph_subpixel_offset(physical.cache_key);
    let outline = resolve_outline_for_glyph(font_system, &mut caches.outlines, glyph, faux.bold);
    // Capture the swash bitmap only for genuinely outline-less glyphs (real
    // color/emoji or a monochrome embedded-bitmap glyph); a glyph whose outline
    // was CONSUMED by a faux thinning offset must draw nothing rather than pop
    // back to its full-weight bitmap.
    let fallback = if outline.is_none()
        && glyph_needs_bitmap_fallback(font_system, &mut caches.outlines, glyph, faux.bold)
    {
        Some((image.content, image.data.clone()))
    } else {
        None
    };
    let bounds_pad = faux_bounds_pads(
        faux,
        placement_top,
        glyph_h as f32,
        scale.width_mul,
        scale.height_mul,
    );
    Some(HorizontalGlyphPlacement {
        outline,
        fallback,
        glyph_w,
        glyph_h,
        src_left_i,
        src_top_i,
        placement_left,
        placement_top,
        scale,
        text_color,
        subpixel,
        faux,
        bounds_pad,
    })
}

/// Draw one collected horizontal (unrotated) glyph placement into the canvas.
///
/// Rasterizes the glyph's true font outline at exactly the pixels the bitmap blit
/// used: scaled about the bitmap center (`dst_center`), no rotation, world mapped
/// to the canvas by the `canvas_offset` (`[x, y]`) derived from the bounds box.
/// Color/emoji glyphs (captured `fallback`) keep the bitmap blit (identity or
/// center-scaled). `aa_lut` is the coverage->alpha transfer table applied only on
/// the outline path; the bitmap fallback is unaffected. `scratch` supplies the
/// reused per-glyph rasterizer buffers. `warp` is the optional vector mesh warp,
/// applied only on the outline path (the bitmap fallback is not warped in
/// Phase 1); `None` keeps the byte-identical fast path.
pub(super) fn draw_horizontal_placement(
    scratch: &mut RasterScratch,
    canvas: &mut RgbaCanvasView<'_>,
    placement: &HorizontalGlyphPlacement,
    canvas_offset: [i32; 2],
    aa_lut: &[u8; 256],
    warp: Option<&MeshWarpContext>,
) {
    let [x_offset, y_offset] = canvas_offset;
    let glyph_w = placement.glyph_w;
    let glyph_h = placement.glyph_h;
    // Content-space top-left of the (unscaled) glyph bitmap.
    let src_left = placement.src_left_i as f32;
    let src_top = placement.src_top_i as f32;

    // Prefer the true font outline: rasterize it at the exact world placement the
    // bitmap blit would have used (scale about the bitmap center, no rotation).
    if let Some(outline) = placement.outline.as_ref() {
        // Height scale anchored at the pen baseline, not at the glyph's own box
        // centre: a box-centre pivot lifts a shrunk glyph off the baseline by an
        // amount that differs per glyph (its own ink box), which is what made a
        // partially height-scaled word float at mid-height.
        let (dst_center_x, dst_center_y) = placement.scale.scaled_center_about_baseline(
            src_left,
            src_top,
            glyph_w as f32,
            glyph_h as f32,
            placement.baseline_y(),
        );
        // Re-add the subpixel fraction cosmic-text baked into the bitmap coverage
        // (physical.x/y carried only the integer pen), so the outline matches it.
        let transform = glyph_outline_transform(
            GlyphBox {
                placement_left: placement.placement_left,
                placement_top: placement.placement_top,
                glyph_w: glyph_w as f32,
                glyph_h: glyph_h as f32,
                scale: placement.scale,
                subpixel: placement.subpixel,
                shear_x: placement.faux.shear_x,
            },
            [dst_center_x, dst_center_y],
            0.0,
        );
        rasterize_outline_into(
            scratch,
            canvas,
            [-(x_offset as f32), -(y_offset as f32)],
            outline,
            &transform,
            OutlinePaint {
                color: placement.text_color,
                aa_lut,
                warp,
            },
        );
        return;
    }

    // No fillable outline (real color glyph, or a monochrome embedded-bitmap /
    // sbix / CBDT-mono glyph): blit the captured non-empty bitmap. Spaces were
    // already filtered by the zero-size check during collection. Dropping this on
    // a non-color glyph would silently lose embedded-bitmap-only glyphs.
    let Some((content, data)) = placement.fallback.as_ref() else {
        return;
    };
    let draw_x = placement.src_left_i + x_offset;
    let draw_y = placement.src_top_i + y_offset;
    if placement.scale.is_identity() {
        rasterize_unscaled_glyph(
            canvas,
            SwashGlyphView {
                content: *content,
                data: data.as_slice(),
                width: glyph_w,
                height: glyph_h,
            },
            [draw_x, draw_y],
            placement.text_color,
        );
    } else {
        let glyph_rgba =
            build_glyph_rgba_buffer(content, data.as_slice(), glyph_w, glyph_h, placement.text_color);
        // Same baseline anchor as the outline path above, shifted into canvas px
        // by the bounds offsets — the bitmap fallback must land in the same box
        // the bounds pass reserved for it.
        let dst_rect = placement.scale.scaled_rect_about_baseline(
            draw_x as f32,
            draw_y as f32,
            glyph_w as f32,
            glyph_h as f32,
            placement.baseline_y() + y_offset as f32,
        );
        draw_scaled_glyph_rgba(
            canvas,
            RgbaView {
                rgba: glyph_rgba.as_slice(),
                width: glyph_w,
                height: glyph_h,
            },
            dst_rect,
            placement.scale,
        );
    }
}

/// Content-space placement-box `(corners, center)` of a normal (unrotated)
/// horizontal glyph, matching the scaled draw box (per-glyph width/height scale,
/// no rotation). Corners are counter-clockwise starting at the top-left.
#[must_use]
pub(super) fn horizontal_placement_extra_samples(
    placement: &HorizontalGlyphPlacement,
) -> ([[f32; 2]; 4], [f32; 2]) {
    let (left, top, width, height) = placement.scale.scaled_rect_about_baseline(
        placement.src_left_i as f32,
        placement.src_top_i as f32,
        placement.glyph_w as f32,
        placement.glyph_h as f32,
        placement.baseline_y(),
    );
    let corners = [
        [left, top],
        [left + width, top],
        [left + width, top + height],
        [left, top + height],
    ];
    let center = [left + width * 0.5, top + height * 0.5];
    (corners, center)
}
