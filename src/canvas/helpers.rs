/*
File: src/canvas/helpers.rs

Purpose:
Чистые helper-функции для canvas-модуля: геометрия, сериализация rect coords,
overlay tiling, текстовые оценки и хеширование bubble-снимков.

Main responsibilities:
- инкапсулировать stateless helper-логику;
- уменьшить размер `src/canvas/mod.rs`;
- сохранить переиспользуемые pure helpers вне основного runtime-фасада.

Key functions:
- build_overlay_prepared_tiles
- rect_coords_from_bubble
- upsert_rect_coords_into_extra
- bubbles_stamp
- page_info_content_size

Notes:
- Функции не держат runtime state `CanvasView`.
- GUI-зависимая helper-логика ограничена измерением текста через `egui::Ui`.
*/

use super::OVERLAY_TILE_SIDE;
use super::types::{OverlayPreparedTile, OverlayRectPx, RectCoords};
use crate::app::PageImageInfo;
use crate::project::{Bubble, Side};
use eframe::egui;
use egui::{Color32, FontFamily, FontId, Rect, Stroke, Vec2};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

pub(crate) const BUBBLE_TEXT_FONT_FAMILY_NAME: &str = "canvas-bubble-unicode";

const TEXT_MEASURE_CACHE_ID: &str = "canvas_text_measure_cache";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum TextMeasureKind {
    Height,
    CompactWidth,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TextMeasureKey {
    kind: TextMeasureKind,
    text: String,
    width_bits: u32,
    font_size_bits: u32,
}

#[derive(Clone, Debug, Default)]
struct TextMeasureCache {
    values: HashMap<TextMeasureKey, f32>,
}

pub(crate) fn bubble_text_font_family() -> FontFamily {
    FontFamily::Name(BUBBLE_TEXT_FONT_FAMILY_NAME.into())
}

fn bubble_text_font_bound(ui: &egui::Ui) -> bool {
    let bubble_family = bubble_text_font_family();
    ui.ctx()
        .fonts(|fonts| fonts.definitions().families.contains_key(&bubble_family))
}

pub(crate) fn bubble_text_font_id(ui: &egui::Ui) -> FontId {
    let default_font = egui::FontSelection::Default.resolve(ui.style());
    let bubble_family = bubble_text_font_family();
    if bubble_text_font_bound(ui) {
        FontId::new(default_font.size, bubble_family)
    } else {
        default_font
    }
}

fn text_measure_cache_lookup(ui: &egui::Ui, key: &TextMeasureKey) -> Option<f32> {
    let cache_id = egui::Id::new(TEXT_MEASURE_CACHE_ID);
    ui.ctx().data_mut(|data| {
        data.get_temp::<TextMeasureCache>(cache_id)
            .and_then(|cache| cache.values.get(key).copied())
    })
}

fn text_measure_cache_store(ui: &egui::Ui, key: TextMeasureKey, value: f32) {
    let cache_id = egui::Id::new(TEXT_MEASURE_CACHE_ID);
    ui.ctx().data_mut(|data| {
        let mut cache = data
            .get_temp::<TextMeasureCache>(cache_id)
            .unwrap_or_default();
        cache.values.insert(key, value);
        data.insert_temp(cache_id, cache);
    });
}

pub(crate) fn with_bubble_text_font<R>(
    ui: &mut egui::Ui,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let previous_override = ui.style().override_font_id.clone();
    ui.style_mut().override_font_id = Some(bubble_text_font_id(ui));
    let result = add_contents(ui);
    ui.style_mut().override_font_id = previous_override;
    result
}

pub(crate) fn sanitize_clipboard_text(raw: &str) -> String {
    let replaced = raw.replace('\u{2026}', "...");
    let body = replaced.trim().to_string();
    let n = body.chars().count();
    if n > 0 && n.is_multiple_of(2) {
        let half = n / 2;
        let left: String = body.chars().take(half).collect();
        let right: String = body.chars().skip(half).collect();
        if left == right {
            let lead = replaced
                .char_indices()
                .find(|(_, ch)| !ch.is_whitespace())
                .map(|(idx, _)| &replaced[..idx])
                .unwrap_or("");
            let tail = replaced
                .char_indices()
                .rev()
                .find(|(_, ch)| !ch.is_whitespace())
                .map(|(idx, ch)| &replaced[idx + ch.len_utf8()..])
                .unwrap_or("");
            return format!("{lead}{left}{tail}");
        }
    }
    replaced
}

pub(crate) fn blit_scaled_chunk(
    dst: &mut egui::ColorImage,
    target: OverlayRectPx,
    chunk: &egui::ColorImage,
) {
    if target.w == 0 || target.h == 0 || chunk.size[0] == 0 || chunk.size[1] == 0 {
        return;
    }
    let dst_w = dst.size[0];
    let dst_h = dst.size[1];
    for y in 0..target.h {
        let src_y = (y * chunk.size[1] / target.h).min(chunk.size[1] - 1);
        let dst_y = target.y + y;
        if dst_y >= dst_h {
            break;
        }
        for x in 0..target.w {
            let src_x = (x * chunk.size[0] / target.w).min(chunk.size[0] - 1);
            let dst_x = target.x + x;
            if dst_x >= dst_w {
                break;
            }
            let src_idx = src_y * chunk.size[0] + src_x;
            let dst_idx = dst_y * dst_w + dst_x;
            if let (Some(src), Some(dst_px)) = (
                chunk.pixels.get(src_idx).copied(),
                dst.pixels.get_mut(dst_idx),
            ) {
                *dst_px = src;
            }
        }
    }
}

pub(crate) fn build_overlay_prepared_tiles(image: &egui::ColorImage) -> Vec<OverlayPreparedTile> {
    let w = image.size[0];
    let h = image.size[1];
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let mut tiles = Vec::new();
    let mut tile_idx = 0usize;
    let mut y = 0usize;
    while y < h {
        let mut x = 0usize;
        while x < w {
            let tw = (w - x).min(OVERLAY_TILE_SIDE);
            let th = (h - y).min(OVERLAY_TILE_SIDE);
            tiles.push(OverlayPreparedTile {
                tile_idx,
                origin_px: [x, y],
                size_px: [tw, th],
                rgba: rgba_from_overlay_tile(image, x, y, tw, th),
            });
            tile_idx += 1;
            x += OVERLAY_TILE_SIDE;
        }
        y += OVERLAY_TILE_SIDE;
    }
    tiles
}

pub(crate) fn rgba_from_overlay_tile(
    image: &egui::ColorImage,
    origin_x: usize,
    origin_y: usize,
    tile_w: usize,
    tile_h: usize,
) -> Vec<u8> {
    let full_w = image.size[0];
    let mut raw = vec![0u8; tile_w * tile_h * 4];
    for ty in 0..tile_h {
        let sy = origin_y + ty;
        let row_off = sy * full_w;
        for tx in 0..tile_w {
            let sx = origin_x + tx;
            let src_idx = row_off + sx;
            let dst_idx = (ty * tile_w + tx) * 4;
            let px = image.pixels[src_idx];
            raw[dst_idx] = px.r();
            raw[dst_idx + 1] = px.g();
            raw[dst_idx + 2] = px.b();
            raw[dst_idx + 3] = px.a();
        }
    }
    raw
}

pub(crate) fn build_overlay_tile_image(
    image: &egui::ColorImage,
    origin_x: usize,
    origin_y: usize,
    tile_w: usize,
    tile_h: usize,
) -> egui::ColorImage {
    let raw = rgba_from_overlay_tile(image, origin_x, origin_y, tile_w, tile_h);
    egui::ColorImage::from_rgba_premultiplied([tile_w, tile_h], &raw)
}

pub(crate) fn bubble_side(bubble: &Bubble) -> Side {
    match bubble
        .side
        .as_deref()
        .unwrap_or("right")
        .to_ascii_lowercase()
        .as_str()
    {
        "left" => Side::Left,
        _ => Side::Right,
    }
}

pub(crate) fn side_to_string(side: Side) -> String {
    match side {
        Side::Left => "left".to_string(),
        Side::Right => "right".to_string(),
    }
}

pub(crate) fn bubbles_stamp(bubbles: &[Bubble]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bubbles.len().hash(&mut hasher);
    for bubble in bubbles {
        bubble_fingerprint_with_hasher(bubble, &mut hasher);
    }
    hasher.finish()
}

pub(crate) fn bubble_fingerprint(bubble: &Bubble) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bubble_fingerprint_with_hasher(bubble, &mut hasher);
    hasher.finish()
}

pub(crate) fn bubble_fingerprint_with_hasher(
    bubble: &Bubble,
    hasher: &mut std::collections::hash_map::DefaultHasher,
) {
    bubble.id.hash(hasher);
    bubble.img_idx.hash(hasher);
    bubble.img_u.to_bits().hash(hasher);
    bubble.img_v.to_bits().hash(hasher);
    bubble.side.hash(hasher);
    bubble.bubble_type.hash(hasher);
    bubble.text.hash(hasher);
    bubble.original_text.hash(hasher);
    if let Some(coords) = rect_coords_from_bubble(bubble) {
        coords.p1.x.to_bits().hash(hasher);
        coords.p1.y.to_bits().hash(hasher);
        coords.p2.x.to_bits().hash(hasher);
        coords.p2.y.to_bits().hash(hasher);
    }
}

pub(crate) fn bubbles_history_hash(bubbles: &[Bubble]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Ok(raw) = serde_json::to_vec(bubbles) {
        raw.hash(&mut hasher);
        return hasher.finish();
    }
    for bubble in bubbles {
        bubble_fingerprint_with_hasher(bubble, &mut hasher);
    }
    hasher.finish()
}

pub(crate) fn default_rect_coords(u: f32, v: f32, delta_u: f32, delta_v: f32) -> RectCoords {
    RectCoords {
        p1: egui::pos2((u - delta_u).clamp(0.0, 1.0), (v - delta_v).clamp(0.0, 1.0)),
        p2: egui::pos2((u + delta_u).clamp(0.0, 1.0), (v + delta_v).clamp(0.0, 1.0)),
    }
    .normalized()
}

pub(crate) fn default_rect_coords_from_source_px(
    u: f32,
    v: f32,
    source_w_px: f32,
    source_h_px: f32,
    rect_side_px: f32,
) -> RectCoords {
    let half_side_px = (rect_side_px * 0.5).max(0.0);
    let delta_u = (half_side_px / source_w_px.max(1.0)).clamp(0.0, 1.0);
    let delta_v = (half_side_px / source_h_px.max(1.0)).clamp(0.0, 1.0);
    default_rect_coords(u, v, delta_u, delta_v)
}

pub(crate) fn rect_coords_from_bubble(bubble: &Bubble) -> Option<RectCoords> {
    let raw = bubble.extra.get("rect_coords")?;
    rect_coords_from_value(raw)
}

pub(crate) fn rect_coords_from_value(raw: &Value) -> Option<RectCoords> {
    let obj = raw.as_object()?;
    let p1 = obj.get("p1")?.as_object()?;
    let p2 = obj.get("p2")?.as_object()?;
    let u1 = read_rect_coord_value(p1, "img_u")?;
    let v1 = read_rect_coord_value(p1, "img_v")?;
    let u2 = read_rect_coord_value(p2, "img_u")?;
    let v2 = read_rect_coord_value(p2, "img_v")?;
    Some(
        RectCoords {
            p1: egui::pos2(u1, v1),
            p2: egui::pos2(u2, v2),
        }
        .normalized(),
    )
}

pub(crate) fn read_rect_coord_value(obj: &Map<String, Value>, key: &str) -> Option<f32> {
    let value = obj.get(key)?;
    if let Some(n) = value.as_f64() {
        return Some((n as f32).clamp(0.0, 1.0));
    }
    if let Some(s) = value.as_str()
        && let Ok(parsed) = s.parse::<f32>()
    {
        return Some(parsed.clamp(0.0, 1.0));
    }
    None
}

pub(crate) fn upsert_rect_coords_into_extra(extra: &mut Map<String, Value>, coords: RectCoords) {
    let coords = coords.normalized();
    extra.insert(
        "rect_coords".to_string(),
        Value::Object(
            [
                (
                    "p1".to_string(),
                    Value::Object(
                        [
                            (
                                "img_u".to_string(),
                                Value::from(f64::from(coords.p1.x.clamp(0.0, 1.0))),
                            ),
                            (
                                "img_v".to_string(),
                                Value::from(f64::from(coords.p1.y.clamp(0.0, 1.0))),
                            ),
                        ]
                        .into_iter()
                        .collect(),
                    ),
                ),
                (
                    "p2".to_string(),
                    Value::Object(
                        [
                            (
                                "img_u".to_string(),
                                Value::from(f64::from(coords.p2.x.clamp(0.0, 1.0))),
                            ),
                            (
                                "img_v".to_string(),
                                Value::from(f64::from(coords.p2.y.clamp(0.0, 1.0))),
                            ),
                        ]
                        .into_iter()
                        .collect(),
                    ),
                ),
            ]
            .into_iter()
            .collect(),
        ),
    );
}

pub(crate) fn measure_text_widget_content_height(ui: &egui::Ui, text: &str, width_px: f32) -> f32 {
    const TEXT_EDIT_MIN_WIDTH_PX: f32 = 24.0;
    const TEXT_EDIT_MARGIN_X_PX: f32 = 8.0;
    const TEXT_EDIT_MARGIN_Y_PX: f32 = 4.0;

    let font_size_bits = bubble_text_font_id(ui).size.to_bits();
    let cache_key = TextMeasureKey {
        kind: TextMeasureKind::Height,
        text: text.to_owned(),
        width_bits: width_px.to_bits(),
        font_size_bits,
    };
    if let Some(cached) = text_measure_cache_lookup(ui, &cache_key) {
        return cached;
    }

    let wrap_width = (width_px - TEXT_EDIT_MARGIN_X_PX).max(TEXT_EDIT_MIN_WIDTH_PX);
    let font_id = bubble_text_font_id(ui);
    let text_color = ui.visuals().widgets.inactive.text_color();
    let layout_text = if text.is_empty() { " " } else { text };
    let (galley_height, row_height) = ui.fonts_mut(|fonts| {
        let galley = fonts.layout_job(egui::text::LayoutJob::simple(
            layout_text.to_owned(),
            font_id.clone(),
            text_color,
            wrap_width,
        ));
        (galley.size().y, fonts.row_height(&font_id))
    });

    let measured = (galley_height.max(row_height) + TEXT_EDIT_MARGIN_Y_PX).max(28.0);
    text_measure_cache_store(ui, cache_key, measured);
    measured
}

pub(crate) fn measure_text_widget_compact_width(
    ui: &egui::Ui,
    text: &str,
    max_width_px: f32,
) -> f32 {
    const TEXT_EDIT_MIN_WIDTH_PX: f32 = 24.0;
    const TEXT_EDIT_MARGIN_X_PX: f32 = 8.0;

    let cache_key = TextMeasureKey {
        kind: TextMeasureKind::CompactWidth,
        text: text.to_owned(),
        width_bits: max_width_px.to_bits(),
        font_size_bits: bubble_text_font_id(ui).size.to_bits(),
    };
    if let Some(cached) = text_measure_cache_lookup(ui, &cache_key) {
        return cached;
    }

    let max_wrap_width = (max_width_px - TEXT_EDIT_MARGIN_X_PX).max(TEXT_EDIT_MIN_WIDTH_PX);
    let target_height = measure_text_widget_content_height(ui, text, max_width_px);
    let mut low = TEXT_EDIT_MIN_WIDTH_PX;
    let mut high = max_wrap_width;
    let mut best = max_wrap_width;

    for _ in 0..12 {
        let mid = (low + high) * 0.5;
        let mid_total_width = mid + TEXT_EDIT_MARGIN_X_PX;
        let measured_height = measure_text_widget_content_height(ui, text, mid_total_width);
        if measured_height <= target_height + 0.5 {
            best = mid;
            high = mid;
        } else {
            low = mid;
        }
    }

    let measured = (best + TEXT_EDIT_MARGIN_X_PX)
        .clamp(TEXT_EDIT_MIN_WIDTH_PX + TEXT_EDIT_MARGIN_X_PX, max_width_px);
    text_measure_cache_store(ui, cache_key, measured);
    measured
}

pub(crate) fn draw_anchor_link(
    painter: &egui::Painter,
    image_rect: Rect,
    img_u: f32,
    img_v: f32,
    target_x: f32,
    target_y: f32,
    color: Color32,
) {
    let anchor = egui::pos2(
        image_rect.left() + image_rect.width() * img_u.clamp(0.0, 1.0),
        image_rect.top() + image_rect.height() * img_v.clamp(0.0, 1.0),
    );

    let target = egui::pos2(target_x, target_y);

    painter.line_segment([anchor, target], Stroke::new(1.0, color));
    painter.circle_filled(anchor, 3.0, color);
}

pub(crate) fn page_info_content_size(page_info: &PageImageInfo) -> Option<Vec2> {
    if page_info.width_px == 0 || page_info.height_px == 0 {
        return None;
    }
    Some(egui::vec2(
        page_info.width_px as f32,
        page_info.height_px as f32,
    ))
}
