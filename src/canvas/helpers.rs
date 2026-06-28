/*
File: src/canvas/helpers.rs

Purpose:
Чистые helper-функции для canvas-модуля: геометрия, сериализация rect coords,
per-tile overlay RGBA extraction, текстовые оценки и хеширование bubble-снимков.

Main responsibilities:
- инкапсулировать stateless helper-логику;
- уменьшить размер `src/canvas/mod.rs`;
- сохранить переиспользуемые pure helpers вне основного runtime-фасада.

Key functions:
- rgba_from_overlay_tile
- rect_coords_from_bubble
- upsert_rect_coords_into_extra
- bubbles_stamp
- page_info_content_size

Notes:
- Функции не держат runtime state `CanvasView`.
- GUI-зависимая helper-логика ограничена измерением текста через `egui::Ui`.
- The text-measurement cache lives in egui temp data as a cheap-to-clone shared handle
  (`Arc<Mutex<..>>`) so per-frame lookups/stores no longer clone the whole map; the map is
  bounded in size and cleared on overflow (see `TEXT_MEASURE_CACHE_MAX_ENTRIES`).
*/

use super::types::{BubbleClass, ImageTextArea, OverlayRectPx, RectCoords};
use crate::app::PageImageInfo;
use crate::project::{Bubble, Side};
use eframe::egui;
use egui::{Color32, FontFamily, FontId, Pos2, Rect, Stroke, Vec2};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

pub(crate) const BUBBLE_TEXT_FONT_FAMILY_NAME: &str = "canvas-bubble-unicode";

const TEXT_MEASURE_CACHE_ID: &str = "canvas_text_measure_cache";

/// Maximum number of entries kept in the text-measurement cache.
///
/// The cache key includes the available text width in points, which scales with canvas zoom, so
/// each zoom level produces fresh entries per (text, width, font). Without a bound the map would
/// grow without limit across a zoom session. On overflow the whole map is cleared (clear-on-overflow
/// rather than LRU): measurements are cheap to recompute and clearing keeps the store O(1) and
/// simple. 8192 entries is well above the live bubble count for a single page yet small enough to
/// keep the map cheap.
const TEXT_MEASURE_CACHE_MAX_ENTRIES: usize = 8192;

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

/// Cheap-to-clone shared handle to the bounded text-measurement map stored in egui temp data.
///
/// Cloning the handle is O(1) (an `Arc` bump), unlike cloning the owned `HashMap`. The `Mutex`
/// keeps it `Send + Sync` as required by `egui::Memory::insert_temp`.
type TextMeasureCache = Arc<Mutex<HashMap<TextMeasureKey, f32>>>;

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

/// Returns the cheap-to-clone shared cache handle from egui temp data, creating an empty one on
/// first use.
///
/// Only the `Arc` is cloned out of the temp store (O(1)); the underlying map is never cloned.
fn text_measure_cache_handle(ui: &egui::Ui) -> TextMeasureCache {
    let cache_id = egui::Id::new(TEXT_MEASURE_CACHE_ID);
    ui.ctx().data_mut(|data| {
        if let Some(handle) = data.get_temp::<TextMeasureCache>(cache_id) {
            return handle;
        }
        let handle: TextMeasureCache = Arc::new(Mutex::new(HashMap::new()));
        data.insert_temp(cache_id, handle.clone());
        handle
    })
}

/// Reads a cached measurement for `key`, or `None` on a miss.
///
/// Clones only the shared handle (O(1)) and briefly locks the map for the read. A poisoned lock is
/// treated as a miss rather than panicking.
fn text_measure_cache_lookup(ui: &egui::Ui, key: &TextMeasureKey) -> Option<f32> {
    let handle = text_measure_cache_handle(ui);
    let guard = handle.lock().ok()?;
    guard.get(key).copied()
}

/// Stores `value` for `key` in the bounded shared cache.
///
/// Clones only the shared handle (O(1)) and briefly locks the map for the insert. Size is bounded
/// to `TEXT_MEASURE_CACHE_MAX_ENTRIES`; on overflow the map is cleared before inserting
/// (clear-on-overflow, not LRU). A poisoned lock is treated as a no-op rather than panicking.
fn text_measure_cache_store(ui: &egui::Ui, key: TextMeasureKey, value: f32) {
    let handle = text_measure_cache_handle(ui);
    if let Ok(mut guard) = handle.lock() {
        insert_bounded(&mut guard, key, value, TEXT_MEASURE_CACHE_MAX_ENTRIES);
    }
}

/// Inserts `key`/`value` into `map`, clearing it first when it is already at `cap` and `key` is new.
///
/// `cap` of 0 means the map is kept empty. Clear-on-overflow keeps the operation O(1) amortized and
/// avoids unbounded growth; updating an existing key never triggers a clear.
fn insert_bounded(
    map: &mut HashMap<TextMeasureKey, f32>,
    key: TextMeasureKey,
    value: f32,
    cap: usize,
) {
    if map.len() >= cap && !map.contains_key(&key) {
        map.clear();
    }
    if cap == 0 {
        return;
    }
    map.insert(key, value);
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
    // Multi-area image bubbles store geometry/text of extra areas here; hash it so remote edits
    // (e.g. cross-tab autosync) re-sync into the runtime even though the legacy fields are equal.
    if let Some(areas) = bubble.extra.get("text_areas") {
        areas.to_string().hash(hasher);
    }
    if let Some(description) = bubble.extra.get("description").and_then(Value::as_str) {
        description.hash(hasher);
    }
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

/// Returns the image-area rect (the single red rectangle) for an image bubble.
///
/// For a page-crop image bubble the image area is the crop region (`extra["crop_rect"]`), which is
/// the authoritative rect that sizes the image on the ribbon; `rect_coords` is kept in sync with
/// it. For external images and as a fallback it returns `rect_coords`. Returns `None` when neither
/// is present.
pub(crate) fn image_area_rect_from_bubble(bubble: &Bubble) -> Option<RectCoords> {
    let is_image =
        bubble.bubble_class.as_deref().map(BubbleClass::from_str) == Some(BubbleClass::Image);
    if is_image
        && bubble
            .extra
            .get("image_source_type")
            .and_then(Value::as_str)
            .unwrap_or("external")
            == "page_crop"
        && let Some(arr) = bubble.extra.get("crop_rect").and_then(Value::as_array)
        && arr.len() == 4
    {
        let coord = |idx: usize| arr[idx].as_f64().map(|n| (n as f32).clamp(0.0, 1.0));
        if let (Some(x1), Some(y1), Some(x2), Some(y2)) = (coord(0), coord(1), coord(2), coord(3)) {
            return Some(
                RectCoords {
                    p1: egui::pos2(x1, y1),
                    p2: egui::pos2(x2, y2),
                }
                .normalized(),
            );
        }
    }
    rect_coords_from_bubble(bubble)
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

/// Parses the text areas of a multi-area `ImageBubble` from its record.
///
/// Returns an empty vector for non-image bubbles. For image bubbles it always returns at least one
/// area: when `extra["text_areas"]` is missing or empty a single area is synthesized from the
/// legacy fields (`rect_coords` as the area rect, `img_u/img_v` as the anchor). Area 0's text is
/// always taken from the legacy `text` / `original_text` / `extra.description` fields, which remain
/// the source of truth for the primary area; later areas carry their own text. Every area is
/// normalized so its rect sits inside the red `rect_coords` and its anchor sits inside its rect.
#[must_use]
pub(crate) fn parse_image_text_areas(bubble: &Bubble) -> Vec<ImageTextArea> {
    let is_image = bubble
        .bubble_class
        .as_deref()
        .map(BubbleClass::from_str)
        .unwrap_or(BubbleClass::Text)
        == BubbleClass::Image;
    if !is_image {
        return Vec::new();
    }

    let red_rect = image_area_rect_from_bubble(bubble)
        .unwrap_or_else(|| default_rect_coords(bubble.img_u, bubble.img_v, 0.05, 0.05));
    let legacy_description = bubble
        .extra
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut areas: Vec<ImageTextArea> = bubble
        .extra
        .get("text_areas")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(image_text_area_from_value)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if areas.is_empty() {
        // Legacy single-area image bubble: synthesize one small box around the stored anchor (not
        // covering the whole image area, so it does not look like a duplicate of the red rect).
        let anchor = egui::pos2(bubble.img_u.clamp(0.0, 1.0), bubble.img_v.clamp(0.0, 1.0));
        areas.push(ImageTextArea {
            area_rect: default_text_area_box(red_rect, anchor),
            anchor,
            original: String::new(),
            description: String::new(),
            translation: String::new(),
        });
    }

    // Migrate areas that cover (nearly) the entire image area back to a small box around their
    // anchor; older builds stored area 0 as the full red rect, which duplicated the image area.
    for area in &mut areas {
        if covers_full_red_rect(area.area_rect, red_rect) {
            area.area_rect = default_text_area_box(red_rect, area.anchor);
        }
    }

    // Area 0 text is mirrored from the legacy fields so OCR/MT/status keep one canonical primary.
    if let Some(first) = areas.first_mut() {
        first.original = bubble.original_text.clone();
        first.translation = bubble.text.clone();
        first.description = legacy_description;
    }

    normalize_image_text_areas(&mut areas, red_rect);
    areas
}

/// Reads one `ImageTextArea` from a JSON object `{rect:[x1,y1,x2,y2], anchor:[u,v], original,
/// description, translation}`. Returns `None` when the rect/anchor arrays are malformed.
fn image_text_area_from_value(raw: &Value) -> Option<ImageTextArea> {
    let obj = raw.as_object()?;
    let rect = obj.get("rect")?.as_array()?;
    if rect.len() != 4 {
        return None;
    }
    let coord = |idx: usize| -> f32 {
        rect.get(idx)
            .and_then(Value::as_f64)
            .map(|n| (n as f32).clamp(0.0, 1.0))
            .unwrap_or(0.0)
    };
    let area_rect = RectCoords {
        p1: egui::pos2(coord(0), coord(1)),
        p2: egui::pos2(coord(2), coord(3)),
    }
    .normalized();
    let anchor_arr = obj.get("anchor").and_then(Value::as_array);
    let anchor = match anchor_arr {
        Some(values) if values.len() == 2 => egui::pos2(
            values[0]
                .as_f64()
                .map(|n| (n as f32).clamp(0.0, 1.0))
                .unwrap_or_else(|| area_rect.center_uv().x),
            values[1]
                .as_f64()
                .map(|n| (n as f32).clamp(0.0, 1.0))
                .unwrap_or_else(|| area_rect.center_uv().y),
        ),
        _ => area_rect.center_uv(),
    };
    let text = |key: &str| {
        obj.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Some(ImageTextArea {
        area_rect,
        anchor,
        original: text("original"),
        description: text("description"),
        translation: text("translation"),
    })
}

/// Serializes image text areas into the JSON array stored under `extra["text_areas"]`.
///
/// Geometry is written for every area; area 0's text is intentionally omitted because the legacy
/// fields remain its source of truth, so it is never persisted in two places that could drift.
#[must_use]
pub(crate) fn serialize_image_text_areas(areas: &[ImageTextArea]) -> Value {
    let items: Vec<Value> = areas
        .iter()
        .enumerate()
        .map(|(idx, area)| {
            let rect = area.area_rect.normalized();
            let mut obj = Map::new();
            obj.insert(
                "rect".to_string(),
                Value::Array(vec![
                    Value::from(f64::from(rect.p1.x)),
                    Value::from(f64::from(rect.p1.y)),
                    Value::from(f64::from(rect.p2.x)),
                    Value::from(f64::from(rect.p2.y)),
                ]),
            );
            obj.insert(
                "anchor".to_string(),
                Value::Array(vec![
                    Value::from(f64::from(area.anchor.x)),
                    Value::from(f64::from(area.anchor.y)),
                ]),
            );
            if idx != 0 {
                obj.insert("original".to_string(), Value::String(area.original.clone()));
                obj.insert(
                    "description".to_string(),
                    Value::String(area.description.clone()),
                );
                obj.insert(
                    "translation".to_string(),
                    Value::String(area.translation.clone()),
                );
            }
            Value::Object(obj)
        })
        .collect();
    Value::Array(items)
}

/// Clamps every text area (including area 0) so its rect lies fully inside the red image area and
/// its anchor lies inside its own rect.
///
/// All areas are independent sub-rects of the red `rect_coords`. Areas wider/taller than the red
/// rect are shrunk to fit; rects are translated (not just clipped) to stay fully inside so dragging
/// never silently resizes them.
pub(crate) fn normalize_image_text_areas(areas: &mut [ImageTextArea], red_rect: RectCoords) {
    let red = red_rect.normalized();
    for area in areas.iter_mut() {
        area.area_rect = clamp_rect_inside(area.area_rect.normalized(), red);
        area.anchor = clamp_pos_inside(area.anchor, area.area_rect);
    }
}

/// Builds a default text-area box centered on `anchor`, sized to a fraction of the red rect and
/// clamped to stay inside it. Used so area 0 (and freshly added areas) start as a small sub-box
/// rather than covering the whole image area.
pub(crate) fn default_text_area_box(red: RectCoords, anchor: Pos2) -> RectCoords {
    let red = red.normalized();
    let w = (red.p2.x - red.p1.x).max(0.001);
    let h = (red.p2.y - red.p1.y).max(0.001);
    let half_w = (w * 0.25).max(0.0);
    let half_h = (h * 0.2).max(0.0);
    let cx = anchor.x.clamp(red.p1.x + half_w, red.p2.x - half_w);
    let cy = anchor.y.clamp(red.p1.y + half_h, red.p2.y - half_h);
    RectCoords {
        p1: egui::pos2(cx - half_w, cy - half_h),
        p2: egui::pos2(cx + half_w, cy + half_h),
    }
    .normalized()
}

/// Clamps `inner` to lie fully inside `outer`, shrinking it only when it is larger than `outer`.
fn clamp_rect_inside(inner: RectCoords, outer: RectCoords) -> RectCoords {
    let clamp_axis = |mut lo: f32, mut hi: f32, omin: f32, omax: f32| -> (f32, f32) {
        let span = (hi - lo).min(omax - omin).max(0.0);
        if lo < omin {
            lo = omin;
            hi = omin + span;
        }
        if hi > omax {
            hi = omax;
            lo = omax - span;
        }
        (lo.max(omin), hi.min(omax))
    };
    let (x1, x2) = clamp_axis(inner.p1.x, inner.p2.x, outer.p1.x, outer.p2.x);
    let (y1, y2) = clamp_axis(inner.p1.y, inner.p2.y, outer.p1.y, outer.p2.y);
    RectCoords {
        p1: egui::pos2(x1, y1),
        p2: egui::pos2(x2, y2),
    }
}

/// True when `area` covers essentially the entire `red` rect (within a small tolerance), i.e. it is
/// a leftover full-image-area box that should be shrunk to a normal sub-box.
fn covers_full_red_rect(area: RectCoords, red: RectCoords) -> bool {
    let area = area.normalized();
    let red = red.normalized();
    const EPS: f32 = 0.01;
    area.p1.x <= red.p1.x + EPS
        && area.p1.y <= red.p1.y + EPS
        && area.p2.x >= red.p2.x - EPS
        && area.p2.y >= red.p2.y - EPS
}

/// Clamps a normalized point to lie inside `rect`.
fn clamp_pos_inside(pos: Pos2, rect: RectCoords) -> Pos2 {
    let rect = rect.normalized();
    egui::pos2(
        pos.x.clamp(rect.p1.x, rect.p2.x),
        pos.y.clamp(rect.p1.y, rect.p2.y),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::types::{ImageTextArea, image_bubble_side_from_areas};
    use serde_json::json;

    fn image_bubble(extra: Map<String, Value>) -> Bubble {
        Bubble {
            id: 1,
            img_idx: 0,
            img_u: 0.5,
            img_v: 0.5,
            side: Some("right".to_string()),
            bubble_class: Some("image".to_string()),
            bubble_type: Some("aside".to_string()),
            text: "tr0".to_string(),
            original_text: "orig0".to_string(),
            extra,
        }
    }

    fn measure_key(text: &str) -> TextMeasureKey {
        TextMeasureKey {
            kind: TextMeasureKind::Height,
            text: text.to_owned(),
            width_bits: 0,
            font_size_bits: 0,
        }
    }

    #[test]
    fn insert_bounded_normal_insert_and_update() {
        let mut map: HashMap<TextMeasureKey, f32> = HashMap::new();
        insert_bounded(&mut map, measure_key("a"), 1.0, 8);
        insert_bounded(&mut map, measure_key("b"), 2.0, 8);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&measure_key("a")).copied(), Some(1.0));
        // Updating an existing key does not grow the map and does not clear it.
        insert_bounded(&mut map, measure_key("a"), 9.0, 8);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&measure_key("a")).copied(), Some(9.0));
    }

    #[test]
    fn insert_bounded_clears_on_overflow_then_inserts() {
        const CAP: usize = 4;
        let mut map: HashMap<TextMeasureKey, f32> = HashMap::new();
        for i in 0..CAP {
            insert_bounded(&mut map, measure_key(&i.to_string()), i as f32, CAP);
        }
        assert_eq!(map.len(), CAP);
        // A new key at capacity clears the whole map first, then inserts the single new entry.
        insert_bounded(&mut map, measure_key("overflow"), 42.0, CAP);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&measure_key("overflow")).copied(), Some(42.0));
    }

    #[test]
    fn insert_bounded_updating_existing_key_at_capacity_does_not_clear() {
        const CAP: usize = 3;
        let mut map: HashMap<TextMeasureKey, f32> = HashMap::new();
        for i in 0..CAP {
            insert_bounded(&mut map, measure_key(&i.to_string()), i as f32, CAP);
        }
        // Re-inserting an existing key while full must not wipe the cache.
        insert_bounded(&mut map, measure_key("0"), 100.0, CAP);
        assert_eq!(map.len(), CAP);
        assert_eq!(map.get(&measure_key("0")).copied(), Some(100.0));
    }

    #[test]
    fn insert_bounded_zero_cap_keeps_map_empty() {
        let mut map: HashMap<TextMeasureKey, f32> = HashMap::new();
        insert_bounded(&mut map, measure_key("a"), 1.0, 0);
        assert!(map.is_empty());
    }

    #[test]
    fn parse_synthesizes_single_area_for_legacy_image_bubble() {
        let mut extra = Map::new();
        extra.insert(
            "description".to_string(),
            Value::String("desc0".to_string()),
        );
        let areas = parse_image_text_areas(&image_bubble(extra));
        assert_eq!(areas.len(), 1);
        // Area 0 text is always mirrored from the legacy fields.
        assert_eq!(areas[0].original, "orig0");
        assert_eq!(areas[0].translation, "tr0");
        assert_eq!(areas[0].description, "desc0");
    }

    #[test]
    fn image_area_rect_prefers_crop_rect_for_page_crop() {
        let mut extra = Map::new();
        extra.insert(
            "image_source_type".to_string(),
            Value::String("page_crop".to_string()),
        );
        extra.insert(
            "rect_coords".to_string(),
            json!({"p1": {"img_u": 0.4, "img_v": 0.4}, "p2": {"img_u": 0.5, "img_v": 0.5}}),
        );
        extra.insert("crop_rect".to_string(), json!([0.1, 0.1, 0.9, 0.9]));
        let rect = image_area_rect_from_bubble(&image_bubble(extra)).unwrap();
        // The crop region wins over the (smaller) rect_coords for page-crop image bubbles.
        assert!((rect.p1.x - 0.1).abs() < 1e-4 && (rect.p2.x - 0.9).abs() < 1e-4);

        // External image bubble falls back to rect_coords.
        let mut ext = Map::new();
        ext.insert(
            "image_source_type".to_string(),
            Value::String("external".to_string()),
        );
        ext.insert(
            "rect_coords".to_string(),
            json!({"p1": {"img_u": 0.4, "img_v": 0.4}, "p2": {"img_u": 0.5, "img_v": 0.5}}),
        );
        ext.insert("crop_rect".to_string(), json!([0.1, 0.1, 0.9, 0.9]));
        let rect = image_area_rect_from_bubble(&image_bubble(ext)).unwrap();
        assert!((rect.p1.x - 0.4).abs() < 1e-4 && (rect.p2.x - 0.5).abs() < 1e-4);
    }

    #[test]
    fn parse_returns_empty_for_text_bubble() {
        let mut bubble = image_bubble(Map::new());
        bubble.bubble_class = Some("text".to_string());
        assert!(parse_image_text_areas(&bubble).is_empty());
    }

    #[test]
    fn serialize_then_parse_round_trips_extra_areas() {
        let mut extra = Map::new();
        extra.insert(
            "rect_coords".to_string(),
            json!({"p1": {"img_u": 0.0, "img_v": 0.0}, "p2": {"img_u": 1.0, "img_v": 1.0}}),
        );
        extra.insert(
            "description".to_string(),
            Value::String("desc0".to_string()),
        );
        extra.insert(
            "text_areas".to_string(),
            json!([
                {"rect": [0.0, 0.0, 0.4, 0.4], "anchor": [0.2, 0.2]},
                {"rect": [0.5, 0.5, 0.9, 0.9], "anchor": [0.7, 0.7],
                 "original": "o1", "description": "d1", "translation": "t1"}
            ]),
        );
        let areas = parse_image_text_areas(&image_bubble(extra));
        assert_eq!(areas.len(), 2);
        assert_eq!(areas[1].original, "o1");
        assert_eq!(areas[1].translation, "t1");

        // Re-serialize and parse again; geometry of area 1 must survive (red rect spans the page).
        let serialized = serialize_image_text_areas(&areas);
        let mut extra2 = Map::new();
        extra2.insert(
            "rect_coords".to_string(),
            json!({"p1": {"img_u": 0.0, "img_v": 0.0}, "p2": {"img_u": 1.0, "img_v": 1.0}}),
        );
        extra2.insert("text_areas".to_string(), serialized);
        let reparsed = parse_image_text_areas(&image_bubble(extra2));
        assert_eq!(reparsed.len(), 2);
        assert!((reparsed[1].anchor.x - 0.7).abs() < 1e-4);
        assert_eq!(reparsed[1].translation, "t1");
    }

    #[test]
    fn side_weight_lets_one_far_left_anchor_outweigh_two_slightly_right() {
        let area = |u: f32| ImageTextArea {
            area_rect: RectCoords {
                p1: egui::pos2(0.0, 0.0),
                p2: egui::pos2(1.0, 1.0),
            },
            anchor: egui::pos2(u, 0.5),
            original: String::new(),
            description: String::new(),
            translation: String::new(),
        };
        // One strongly-left anchor plus two slightly-right anchors → Left overall.
        let areas = vec![area(0.05), area(0.55), area(0.55)];
        assert_eq!(image_bubble_side_from_areas(&areas), Side::Left);
        // All right → Right.
        let right = vec![area(0.8), area(0.6)];
        assert_eq!(image_bubble_side_from_areas(&right), Side::Right);
    }

    #[test]
    fn normalize_keeps_area_inside_red_rect_and_anchor_inside_area() {
        let red = RectCoords {
            p1: egui::pos2(0.2, 0.2),
            p2: egui::pos2(0.8, 0.8),
        };
        let area = |x1: f32, y1: f32, x2: f32, y2: f32, ax: f32, ay: f32| ImageTextArea {
            area_rect: RectCoords {
                p1: egui::pos2(x1, y1),
                p2: egui::pos2(x2, y2),
            },
            anchor: egui::pos2(ax, ay),
            original: String::new(),
            description: String::new(),
            translation: String::new(),
        };
        let mut areas = vec![
            // Already inside red: stays put (area 0 is a normal sub-box, not pinned to red).
            area(0.4, 0.4, 0.5, 0.5, 0.45, 0.45),
            // Partly outside red with anchor outside the sub-area: clamped fully inside red.
            area(0.7, 0.7, 1.2, 1.2, 0.95, 0.95),
        ];
        normalize_image_text_areas(&mut areas, red);
        for area in &areas {
            let rect = area.area_rect;
            assert!(rect.p1.x >= 0.2 - 1e-4 && rect.p2.x <= 0.8 + 1e-4);
            assert!(rect.p1.y >= 0.2 - 1e-4 && rect.p2.y <= 0.8 + 1e-4);
            assert!(area.anchor.x >= rect.p1.x - 1e-4 && area.anchor.x <= rect.p2.x + 1e-4);
            assert!(area.anchor.y >= rect.p1.y - 1e-4 && area.anchor.y <= rect.p2.y + 1e-4);
        }
        // Area 0 stayed where it was (not resized to fill the red rect).
        assert!((areas[0].area_rect.p1.x - 0.4).abs() < 1e-4);
    }
}
