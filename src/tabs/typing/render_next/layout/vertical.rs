/*
File: src/tabs/typing/render_next/layout/vertical.rs

Purpose:
Vertical raster/layout path staged рендера typing.

Main responsibilities:
- превратить vertical `layout_text` и shaped glyph runs в колонки/cells;
- посчитать column positions и vertical optical spacing без участия старого `render.rs`;
- собрать итоговый `RenderedTextImage`, переиспользуя общий raster-слой.

Source:
- `render_vertical_text`
- `collect_vertical_render_columns`
- `compute_vertical_column_positions`
- `compute_vertical_cell_tops`
- `optical_vertical_pair_adjustment`
из старого `src/tabs/typing/render.rs`
*/

use crate::tabs::typing::render_next::inline_styles::InlineStyleSpan;
use crate::tabs::typing::render_next::pipeline::{
    GlyphScaleSettings, KerningSettings, inline_glyph_offset_for_glyph,
    inline_glyph_scale_for_glyph, inline_kerning_for_glyph, inline_text_color_for_glyph,
};
use crate::tabs::typing::render_next::raster::{
    GlyphRgbaView, PixelBounds, RgbaCanvasView, build_glyph_rgba_buffer, draw_scaled_glyph_rgba,
    include_scaled_rect_bounds, rasterize_unscaled_glyph, sample_swash_alpha,
};
use crate::tabs::typing::render_next::types::{
    RenderedTextImage, TextRenderParams, VerticalLineDirection,
};
use cosmic_text::{Buffer, FontSystem, LayoutGlyph, SwashCache};

const OPTICAL_ALPHA_THRESHOLD: u8 = 24;
const VERTICAL_HALF_SPACE: char = '\u{200A}';

pub(crate) struct VerticalRasterRequest<'a> {
    pub(crate) params: &'a TextRenderParams,
    pub(crate) font_system: &'a mut FontSystem,
    pub(crate) buffer: &'a mut Buffer,
    pub(crate) layout_text: &'a str,
    pub(crate) inline_style_spans: Option<&'a [InlineStyleSpan]>,
    pub(crate) layout_line_offsets: &'a [usize],
    pub(crate) font_size_px: f32,
    pub(crate) base_line_height_px: f32,
    pub(crate) line_extra_spacing_table: &'a [f32],
    pub(crate) direction: VerticalLineDirection,
}

#[derive(Clone)]
enum VerticalRenderCell {
    Glyph {
        glyph: LayoutGlyph,
        text_color: [u8; 4],
        glyph_scale: GlyphScaleSettings,
        kerning: KerningSettings,
        glyph_offset_px: [f32; 2],
    },
    Blank(f32),
}

#[derive(Clone)]
struct VerticalRenderColumn {
    cells: Vec<VerticalRenderCell>,
    visual_width_px: f32,
}

#[derive(Debug, Clone, Copy)]
struct GlyphInkProfile {
    top_px: f32,
    bottom_px: f32,
}

impl GlyphInkProfile {
    #[must_use]
    fn fallback(height_px: f32) -> Self {
        Self {
            top_px: 0.0,
            bottom_px: height_px.max(1.0),
        }
    }

    #[must_use]
    fn height_px(self) -> f32 {
        (self.bottom_px - self.top_px).max(1.0)
    }
}

pub(crate) fn render_vertical_text(
    request: VerticalRasterRequest<'_>,
) -> Result<RenderedTextImage, String> {
    let VerticalRasterRequest {
        params,
        font_system,
        buffer,
        layout_text,
        inline_style_spans,
        layout_line_offsets,
        font_size_px,
        base_line_height_px,
        line_extra_spacing_table,
        direction,
    } = request;
    let width_px = layout_text.lines().count().max(1);
    let mut cache = SwashCache::new();
    let columns = collect_vertical_render_columns(
        params,
        buffer,
        font_system,
        &mut cache,
        layout_text,
        inline_style_spans,
        layout_line_offsets,
        font_size_px,
        params.text_color,
    );
    if columns.is_empty() {
        return Ok(RenderedTextImage::transparent(
            u32::try_from(width_px).unwrap_or(1),
            base_line_height_px.ceil().max(1.0) as u32,
        ));
    }

    let column_positions = compute_vertical_column_positions(
        columns.as_slice(),
        line_extra_spacing_table,
        line_extra_spacing_table.first().copied().unwrap_or(0.0),
        direction,
    );
    let mut bounds = PixelBounds::empty();
    for (column_idx, column) in columns.iter().enumerate() {
        let Some(column_x) = column_positions.get(column_idx).copied() else {
            continue;
        };
        let cell_baselines =
            compute_vertical_cell_baselines(column, font_system, &mut cache, font_size_px);
        for (cell_idx, cell) in column.cells.iter().enumerate() {
            let VerticalRenderCell::Glyph {
                glyph,
                glyph_scale,
                glyph_offset_px,
                ..
            } = cell
            else {
                continue;
            };
            let baseline_y = cell_baselines.get(cell_idx).copied().unwrap_or(font_size_px)
                + glyph_offset_px[1];
            let origin_x = column_x + ((column.visual_width_px - glyph.w).max(0.0) * 0.5) - glyph.x
                + glyph_offset_px[0];
            let physical = glyph.physical((origin_x, baseline_y), 1.0);
            let Some(image) = cache.get_image(font_system, physical.cache_key) else {
                continue;
            };
            let x = physical.x + image.placement.left;
            let y = physical.y - image.placement.top;
            include_scaled_rect_bounds(
                &mut bounds,
                x as f32,
                y as f32,
                image.placement.width as f32,
                image.placement.height as f32,
                *glyph_scale,
            );
        }
    }

    if !bounds.initialized {
        return Ok(RenderedTextImage::transparent(
            u32::try_from(width_px).unwrap_or(1),
            base_line_height_px.ceil().max(1.0) as u32,
        ));
    }

    let horizontal_pad = 2u32;
    let vertical_pad = 2u32;
    let safety_pad = (font_size_px * 0.5).ceil().max(0.0) as u32;
    let content_width = (bounds.max_x - bounds.min_x).max(1) as u32;
    let content_height = (bounds.max_y - bounds.min_y).max(1) as u32;
    let out_width = content_width
        .saturating_add(horizontal_pad * 2)
        .saturating_add(safety_pad * 2);
    let out_height = content_height
        .max(base_line_height_px.ceil().max(1.0) as u32)
        .saturating_add(vertical_pad * 2)
        .saturating_add(safety_pad * 2);
    let x_offset = -bounds.min_x + horizontal_pad as i32 + safety_pad as i32;
    let y_offset = -bounds.min_y + vertical_pad as i32 + safety_pad as i32;
    let mut rgba = vec![0u8; out_width as usize * out_height as usize * 4];

    for (column_idx, column) in columns.iter().enumerate() {
        let Some(column_x) = column_positions.get(column_idx).copied() else {
            continue;
        };
        let cell_baselines =
            compute_vertical_cell_baselines(column, font_system, &mut cache, font_size_px);
        for (cell_idx, cell) in column.cells.iter().enumerate() {
            let VerticalRenderCell::Glyph {
                glyph,
                text_color,
                glyph_scale,
                glyph_offset_px,
                ..
            } = cell
            else {
                continue;
            };
            let baseline_y = cell_baselines.get(cell_idx).copied().unwrap_or(font_size_px)
                + glyph_offset_px[1];
            let origin_x = column_x + ((column.visual_width_px - glyph.w).max(0.0) * 0.5) - glyph.x
                + glyph_offset_px[0];
            let physical = glyph.physical((origin_x, baseline_y), 1.0);
            let Some(image) = cache.get_image(font_system, physical.cache_key) else {
                continue;
            };
            let draw_x = physical.x + image.placement.left + x_offset;
            let draw_y = physical.y - image.placement.top + y_offset;
            let glyph_w = image.placement.width as usize;
            let glyph_h = image.placement.height as usize;
            if glyph_w == 0 || glyph_h == 0 {
                continue;
            }

            if glyph_scale.is_identity() {
                rasterize_unscaled_glyph(
                    rgba.as_mut_slice(),
                    out_width,
                    out_height,
                    image.content,
                    image.data.as_slice(),
                    glyph_w,
                    glyph_h,
                    draw_x,
                    draw_y,
                    *text_color,
                );
                continue;
            }

            let glyph_rgba = build_glyph_rgba_buffer(
                &image.content,
                image.data.as_slice(),
                glyph_w,
                glyph_h,
                *text_color,
            );
            let mut canvas = RgbaCanvasView {
                rgba: rgba.as_mut_slice(),
                width: out_width as usize,
                height: out_height as usize,
            };
            draw_scaled_glyph_rgba(
                &mut canvas,
                GlyphRgbaView {
                    rgba: glyph_rgba.as_slice(),
                    width: glyph_w,
                    height: glyph_h,
                },
                draw_x as f32,
                draw_y as f32,
                *glyph_scale,
            );
        }
    }

    Ok(RenderedTextImage {
        width: out_width,
        height: out_height,
        rgba,
        warnings: Vec::new(),
        content_origin_x: 0,
        content_origin_y: 0,
    })
}

// The collector must see layout, spans and per-glyph defaults together to preserve vertical
// glyph order and inline styling in one pass without allocating an intermediate model.
#[allow(clippy::too_many_arguments)]
fn collect_vertical_render_columns(
    params: &TextRenderParams,
    buffer: &mut Buffer,
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    layout_text: &str,
    inline_style_spans: Option<&[InlineStyleSpan]>,
    layout_line_offsets: &[usize],
    font_size_px: f32,
    default_text_color: [u8; 4],
) -> Vec<VerticalRenderColumn> {
    let mut columns = Vec::<VerticalRenderColumn>::new();
    let source_columns = layout_text.split('\n').collect::<Vec<_>>();

    for (run_idx, run) in buffer.layout_runs().enumerate() {
        let mut cells = Vec::<VerticalRenderCell>::new();
        let mut visual_width_px = 0.0f32;
        let mut glyph_iter = run.glyphs.iter();
        for ch in source_columns
            .get(run_idx)
            .copied()
            .unwrap_or_default()
            .chars()
        {
            if ch == VERTICAL_HALF_SPACE {
                cells.push(VerticalRenderCell::Blank(0.5));
                continue;
            }
            if ch.is_whitespace() {
                cells.push(VerticalRenderCell::Blank(1.0));
                continue;
            }
            let Some(glyph) = glyph_iter.next() else {
                continue;
            };
            let glyph_scale = inline_glyph_scale_for_glyph(
                params,
                inline_style_spans,
                layout_line_offsets,
                run.line_i,
                glyph,
            );
            visual_width_px = visual_width_px.max(measure_vertical_glyph_visual_width(
                font_system,
                cache,
                glyph,
                font_size_px,
                glyph_scale,
            ));
            cells.push(VerticalRenderCell::Glyph {
                glyph: glyph.clone(),
                text_color: inline_text_color_for_glyph(
                    default_text_color,
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                glyph_scale,
                kerning: inline_kerning_for_glyph(
                    params,
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                glyph_offset_px: inline_glyph_offset_for_glyph(
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
            });
        }
        for glyph in glyph_iter {
            let glyph_scale = inline_glyph_scale_for_glyph(
                params,
                inline_style_spans,
                layout_line_offsets,
                run.line_i,
                glyph,
            );
            visual_width_px = visual_width_px.max(measure_vertical_glyph_visual_width(
                font_system,
                cache,
                glyph,
                font_size_px,
                glyph_scale,
            ));
            cells.push(VerticalRenderCell::Glyph {
                glyph: glyph.clone(),
                text_color: inline_text_color_for_glyph(
                    default_text_color,
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                glyph_scale,
                kerning: inline_kerning_for_glyph(
                    params,
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
                glyph_offset_px: inline_glyph_offset_for_glyph(
                    inline_style_spans,
                    layout_line_offsets,
                    run.line_i,
                    glyph,
                ),
            });
        }
        if !cells.is_empty() {
            columns.push(VerticalRenderColumn {
                cells,
                visual_width_px: visual_width_px.max(font_size_px * 0.5).ceil(),
            });
        }
    }

    columns
}

fn measure_vertical_glyph_visual_width(
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    glyph: &LayoutGlyph,
    font_size_px: f32,
    glyph_scale: GlyphScaleSettings,
) -> f32 {
    let physical = glyph.physical((-glyph.x, font_size_px), 1.0);
    if let Some(image) = cache.get_image(font_system, physical.cache_key) {
        let (scaled_width, scaled_height) =
            glyph_scale.scaled_size(image.placement.width as f32, image.placement.height as f32);
        scaled_width
            .max(scaled_height)
            .max(glyph.w.max(1.0) * glyph_scale.width_mul)
    } else {
        glyph.w.max(font_size_px * 0.5) * glyph_scale.width_mul
    }
}

fn compute_vertical_column_positions(
    columns: &[VerticalRenderColumn],
    line_extra_spacing_table: &[f32],
    default_extra_line_spacing_px: f32,
    direction: VerticalLineDirection,
) -> Vec<f32> {
    if columns.is_empty() {
        return Vec::new();
    }

    let mut positions = vec![0.0f32; columns.len()];
    match direction {
        VerticalLineDirection::LeftToRight => {
            let mut x = 0.0f32;
            for (idx, column) in columns.iter().enumerate() {
                positions[idx] = x;
                x += column.visual_width_px
                    + line_extra_spacing_table
                        .get(idx)
                        .copied()
                        .unwrap_or(default_extra_line_spacing_px);
            }
        }
        VerticalLineDirection::RightToLeft => {
            let total_width = columns
                .iter()
                .enumerate()
                .fold(0.0f32, |acc, (idx, column)| {
                    acc + column.visual_width_px
                        + if idx + 1 < columns.len() {
                            line_extra_spacing_table
                                .get(idx)
                                .copied()
                                .unwrap_or(default_extra_line_spacing_px)
                        } else {
                            0.0
                        }
                });
            let mut x = total_width;
            for (idx, column) in columns.iter().enumerate() {
                x -= column.visual_width_px;
                positions[idx] = x;
                if idx + 1 < columns.len() {
                    x -= line_extra_spacing_table
                        .get(idx)
                        .copied()
                        .unwrap_or(default_extra_line_spacing_px);
                }
            }
        }
    }
    positions
}

/// Доля кегля для базового зазора между «чернильными» (ink) боксами глифов столбца.
const VERTICAL_INK_GAP_FRACTION: f32 = 0.1;

/// Базовые линии глифов в столбце.
///
/// Каждый глиф ставится так, чтобы верх его ink-бокса встал на текущую позицию, а
/// шаг до следующего глифа равнялся ink-высоте текущего глифа плюс равномерный зазор
/// (базовый + кернинг). То есть вертикальный шаг определяется реальной высотой глифа,
/// а не полным em, поэтому при нулевом кернинге символы стоят плотно. Кернинг (в % от
/// кегля) и пробелы (em-доли) по-прежнему регулируют расстояние.
fn compute_vertical_cell_baselines(
    column: &VerticalRenderColumn,
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    font_size_px: f32,
) -> Vec<f32> {
    let base_gap = font_size_px * VERTICAL_INK_GAP_FRACTION;
    let profiles = column
        .cells
        .iter()
        .map(|cell| match cell {
            VerticalRenderCell::Glyph { glyph, .. } => {
                Some(glyph_ink_profile(font_system, cache, glyph, font_size_px))
            }
            VerticalRenderCell::Blank(_) => None,
        })
        .collect::<Vec<_>>();

    let mut baselines = vec![font_size_px; column.cells.len()];
    let mut current_top = 0.0f32;
    for (idx, cell) in column.cells.iter().enumerate() {
        match cell {
            VerticalRenderCell::Glyph { kerning, .. } => {
                let profile =
                    profiles[idx].unwrap_or_else(|| GlyphInkProfile::fallback(font_size_px));
                // Базовая линия так, чтобы верх ink-бокса оказался на `current_top`.
                baselines[idx] = current_top + font_size_px - profile.top_px;
                current_top += profile.height_px();
                // Зазор добавляется только между двумя глифами одной строки.
                if matches!(
                    column.cells.get(idx + 1),
                    Some(VerticalRenderCell::Glyph { .. })
                ) {
                    current_top += base_gap + kerning.extra_spacing_px(font_size_px);
                }
            }
            VerticalRenderCell::Blank(height_mul) => {
                current_top += font_size_px * height_mul;
            }
        }
    }
    baselines
}

fn glyph_ink_profile(
    font_system: &mut FontSystem,
    cache: &mut SwashCache,
    glyph: &LayoutGlyph,
    font_size_px: f32,
) -> GlyphInkProfile {
    let physical = glyph.physical((-glyph.x, font_size_px), 1.0);
    let Some(image) = cache.get_image(font_system, physical.cache_key) else {
        return GlyphInkProfile::fallback(font_size_px);
    };
    glyph_ink_profile_from_image(
        (physical.y - image.placement.top) as f32,
        &image.content,
        image.data.as_slice(),
        [
            image.placement.width as usize,
            image.placement.height as usize,
        ],
        font_size_px,
    )
}

fn glyph_ink_profile_from_image(
    draw_top_px: f32,
    content: &cosmic_text::SwashContent,
    data: &[u8],
    glyph_size: [usize; 2],
    fallback_height_px: f32,
) -> GlyphInkProfile {
    let [glyph_w, glyph_h] = glyph_size;
    if glyph_w == 0 || glyph_h == 0 {
        return GlyphInkProfile::fallback(fallback_height_px);
    }

    let mut min_y = glyph_h;
    let mut max_y = 0usize;
    let mut has_alpha = false;

    for gy in 0..glyph_h {
        for gx in 0..glyph_w {
            if sample_swash_alpha(content, data, glyph_w, gx, gy) < OPTICAL_ALPHA_THRESHOLD {
                continue;
            }
            min_y = min_y.min(gy);
            max_y = max_y.max(gy);
            has_alpha = true;
        }
    }

    if !has_alpha {
        return GlyphInkProfile::fallback(fallback_height_px);
    }

    GlyphInkProfile {
        top_px: draw_top_px + min_y as f32,
        bottom_px: draw_top_px + max_y as f32 + 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::compute_vertical_column_positions;
    use crate::tabs::typing::render_next::types::VerticalLineDirection;

    #[test]
    fn right_to_left_columns_shift_from_total_width() {
        let columns = vec![
            super::VerticalRenderColumn {
                cells: Vec::new(),
                visual_width_px: 10.0,
            },
            super::VerticalRenderColumn {
                cells: Vec::new(),
                visual_width_px: 12.0,
            },
        ];

        let positions = compute_vertical_column_positions(
            columns.as_slice(),
            &[4.0, 4.0],
            4.0,
            VerticalLineDirection::RightToLeft,
        );

        assert_eq!(positions, vec![16.0, 0.0]);
    }

}
