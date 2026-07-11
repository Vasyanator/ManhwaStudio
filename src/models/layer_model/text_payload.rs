/*
File: models/layer_model/text_payload.rs

Purpose:
The single decode boundary for a text overlay's placement as stored in `text_info.json`. On disk an
overlay records its position (`img_x_px`/`img_y_px`), rotation in DEGREES (`rotation_deg`), uniform
`scale`, and an optional `deform_mesh`. In memory every layer uses the canonical center-anchored
`TransformRec` (rotation in RADIANS) and `DeformRec`. This module is the ONE place that converts
between the two, so the deg↔rad boundary, the center fallback, and the mesh validation rules live in
exactly one spot — shared by the PS editor (`tabs/ps_editor/text_layers.rs`) and, once text overlays
become unified layer nodes, the typing tab.
*/

use super::manifest::{DeformRec, TransformRec};
use serde_json::{Map, Value};
use std::path::Path;

/// The on-disk overlay store: an ordered JSON array of overlay objects keyed by `uid`.
const TEXT_INFO_FILE: &str = "text_info.json";

/// Fixed namespace for deterministic legacy-overlay uids. Do NOT change once shipped: it defines the
/// stable identity of pre-uid legacy overlays across the typing loader and the shared-doc decoder.
/// The 128-bit value is the ASCII `"ManhwaStudioOver"` (the first 16 bytes of `ManhwaStudioOverlay`,
/// truncated to fit `u128` — the full 19-byte string does not fit); it is an arbitrary fixed seed.
const OVERLAY_UID_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x4d616e68_77615374_7564696f_4f766572);

/// Deterministic uid for a legacy overlay that has no persisted `uid`, derived (UUIDv5) from its rendered
/// PNG file NAME. The typing tab's loader and the shared-doc `decode_page_payload` both call this, so the
/// same overlay resolves to the SAME uid in both — preventing a duplicate text node. `file` may be a path;
/// only its final component seeds the uid (so a bare name and a `dir/name` agree).
#[must_use]
pub fn stable_overlay_uid(file: &str) -> String {
    let name = std::path::Path::new(file)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(file);
    uuid::Uuid::new_v5(&OVERLAY_UID_NAMESPACE, name.as_bytes()).to_string()
}

/// Reads the ordered `text_info.json` overlay array from the first of `dirs` that has a readable,
/// parseable array. Returns an empty vec if none exists or parsing fails — mirrors the private
/// `read_text_info_array` in `tabs/ps_editor/text_layers.rs`, but exposed for the shared `LayerDoc`.
#[must_use]
pub fn read_overlay_entries(dirs: &[&Path]) -> Vec<Value> {
    for dir in dirs {
        let path = dir.join(TEXT_INFO_FILE);
        if let Ok(raw) = crate::storage::storage().read_to_string(path.to_string_lossy().as_ref())
            && let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&raw)
        {
            return items;
        }
    }
    Vec::new()
}

/// Canonical placement decoded from a `text_info.json` overlay entry.
pub struct OverlayPlacement {
    pub transform: TransformRec,
    pub deform: Option<DeformRec>,
}

fn value_f32(v: &Value) -> Option<f32> {
    v.as_f64().map(|f| f as f32)
}

// --- Legacy overlay coordinate vocabulary (the SINGLE per-entry decoder) -------------------------
//
// `text_info.json` accumulated several coordinate vocabularies over time. Both the typing tab and the
// shared `LayerDoc` decode them through THIS module so an old chapter resolves identically in both
// (and so migrating it to the inline v3 payload preserves the original geometry instead of snapping
// everything to page-center). The families this per-entry decoder handles:
//   position: `img_x_px`/`img_y_px` (page px, modern) → else `img_u`/`img_v` or bare `u`/`v`
//             (normalized CENTER-anchor uv, page-relative) → else page center (0.5, 0.5).
//   rotation: `rotation_deg` (modern) or its `angle` alias, in DEGREES → radians.
//   scale:    `scale` (modern) or its `user_scale` alias; floored to a small positive minimum.
//   deform:   `deform_mesh` (`points_px` page-px or `points_uv` page-relative) → else a `transform_uv`
//             quad expanded to a `DEFORM_SURFACE_COLS`×`ROWS` projective mesh.
//
// (Cross-entry legacy families — absolute ribbon `x`/`y`+`region_w`/`region_h`, and top-left-anchored
// bare `u`/`v` that needs the PNG footprint — are normalized to `img_u`/`img_v` UPSTREAM by the typing
// tab's `migrate_legacy_text_overlays` before this decoder runs; the doc loader runs the same step.)

/// Out-of-page slack allowed on normalized uv coordinates (overlays may sit partly off the page).
const MAX_OUT_OF_BOUNDS_UV: f32 = 0.90;
/// Control-point grid a `transform_uv` quad expands into (matches the typing render surface).
const DEFORM_SURFACE_COLS: usize = 13;
const DEFORM_SURFACE_ROWS: usize = 13;

fn uv_min() -> f32 {
    -MAX_OUT_OF_BOUNDS_UV
}
fn uv_max() -> f32 {
    1.0 + MAX_OUT_OF_BOUNDS_UV
}
fn clamp_uv_coord(value: f32) -> f32 {
    value.clamp(uv_min(), uv_max())
}
fn clamp_uv_point(point: [f32; 2]) -> [f32; 2] {
    [clamp_uv_coord(point[0]), clamp_uv_coord(point[1])]
}
fn clamp_page_coord(value: f32, side_px: usize) -> f32 {
    let side_px = side_px.max(1) as f32;
    value.clamp(uv_min() * side_px, uv_max() * side_px)
}
fn clamp_page_point(point: [f32; 2], page_size: [usize; 2]) -> [f32; 2] {
    [
        clamp_page_coord(point[0], page_size[0]),
        clamp_page_coord(point[1], page_size[1]),
    ]
}
fn uv_to_page_px(uv: [f32; 2], page_size: [usize; 2]) -> [f32; 2] {
    [
        clamp_uv_coord(uv[0]) * page_size[0].max(1) as f32,
        clamp_uv_coord(uv[1]) * page_size[1].max(1) as f32,
    ]
}

/// Center of an overlay in page pixels, from the per-entry position vocabulary (see module note).
fn overlay_center_page_px(obj: &Map<String, Value>, page_size: [usize; 2]) -> [f32; 2] {
    if let (Some(x_px), Some(y_px)) = (
        obj.get("img_x_px").and_then(value_f32),
        obj.get("img_y_px").and_then(value_f32),
    ) {
        return clamp_page_point([x_px, y_px], page_size);
    }
    let u = obj
        .get("img_u")
        .or_else(|| obj.get("u"))
        .and_then(value_f32)
        .unwrap_or(0.5)
        .clamp(uv_min(), uv_max());
    let v = obj
        .get("img_v")
        .or_else(|| obj.get("v"))
        .and_then(value_f32)
        .unwrap_or(0.5)
        .clamp(uv_min(), uv_max());
    uv_to_page_px([u, v], page_size)
}

/// Decodes a `text_info.json` overlay object into a canonical placement. `page_size` (page pixels)
/// drives normalized-uv → page-px conversion for legacy coordinates; an entry with no position falls
/// back to the page center. Rotation comes from `rotation_deg` (or its `angle` alias) in DEGREES and is
/// converted to radians; `scale` (or its `user_scale` alias) is floored to a small positive minimum.
/// This is the SINGLE source of truth for reading overlay geometry — both tabs and the doc call it.
#[must_use]
pub fn decode_overlay_placement(obj: &Map<String, Value>, page_size: [usize; 2]) -> OverlayPlacement {
    let [cx, cy] = overlay_center_page_px(obj, page_size);
    let rotation_deg = obj
        .get("rotation_deg")
        .or_else(|| obj.get("angle"))
        .and_then(value_f32)
        .unwrap_or(0.0);
    let scale = obj
        .get("scale")
        .or_else(|| obj.get("user_scale"))
        .and_then(value_f32)
        .unwrap_or(1.0)
        .max(0.01);
    // Deform: an explicit `deform_mesh` wins; else a `transform_uv` quad expands to a projective grid.
    let deform = decode_deform_mesh(obj.get("deform_mesh"), page_size)
        .or_else(|| decode_transform_uv(obj, page_size));
    OverlayPlacement {
        transform: TransformRec {
            cx,
            cy,
            rotation: rotation_deg.to_radians(),
            scale,
        },
        deform,
    }
}

/// Parses a `deform_mesh` storage object into a canonical `DeformRec` (page-pixel control points,
/// row-major). Accepts both `points_px` (absolute page pixels) and the legacy `points_uv` (normalized,
/// page-relative — converted via `page_size`). Returns `None` for a missing/degenerate grid (fewer
/// than 2×2 or a point-count mismatch), so a deformed overlay falls back to its affine transform.
/// `page_size` is only consulted for the `points_uv` form and for clamping.
#[must_use]
pub fn decode_deform_mesh(value: Option<&Value>, page_size: [usize; 2]) -> Option<DeformRec> {
    let obj = value?.as_object()?;
    let cols = obj.get("cols").and_then(Value::as_u64)? as usize;
    let rows = obj.get("rows").and_then(Value::as_u64)? as usize;
    let use_page_px = obj.contains_key("points_px");
    let raw = obj
        .get("points_px")
        .or_else(|| obj.get("points_uv"))
        .and_then(Value::as_array)?;
    if cols < 2 || rows < 2 || raw.len() != cols.saturating_mul(rows) {
        return None;
    }
    let mut points_px: Vec<[f32; 2]> = Vec::with_capacity(raw.len());
    for p in raw {
        let a = p.as_array()?;
        let x = value_f32(a.first()?)?;
        let y = value_f32(a.get(1)?)?;
        points_px.push(if use_page_px {
            clamp_page_point([x, y], page_size)
        } else {
            uv_to_page_px(clamp_uv_point([x, y]), page_size)
        });
    }
    if points_px.len() != cols * rows {
        return None;
    }
    Some(DeformRec {
        cols,
        rows,
        points_px,
    })
}


/// Decodes a legacy `transform_uv` quad (4 normalized corner points, TL→TR→BR→BL) into a
/// `DEFORM_SURFACE_COLS`×`ROWS` projective deform mesh in page pixels — the same expansion the typing
/// tab's `deform_mesh_from_quad` performs. Returns `None` when `transform_uv` is absent or malformed.
fn decode_transform_uv(obj: &Map<String, Value>, page_size: [usize; 2]) -> Option<DeformRec> {
    let raw_quad = obj.get("transform_uv")?.as_array()?;
    if raw_quad.len() != 4 {
        return None;
    }
    let mut quad = [[0.0f32; 2]; 4];
    for (idx, point) in raw_quad.iter().enumerate() {
        let coords = point.as_array()?;
        if coords.len() != 2 {
            return None;
        }
        quad[idx] = clamp_uv_point([value_f32(coords.first()?)?, value_f32(coords.get(1)?)?]);
    }
    let (cols, rows) = (DEFORM_SURFACE_COLS, DEFORM_SURFACE_ROWS);
    let mut points_px = Vec::with_capacity(cols * rows);
    for row in 0..rows {
        let tv = row as f32 / (rows - 1) as f32;
        for col in 0..cols {
            let tu = col as f32 / (cols - 1) as f32;
            points_px.push(uv_to_page_px(projective_quad_uv(quad, tu, tv), page_size));
        }
    }
    Some(DeformRec {
        cols,
        rows,
        points_px,
    })
}

/// Bilinear interpolation across a quad's 4 corners (fallback for a degenerate/near-affine quad).
fn bilinear_quad_uv(quad_uv: [[f32; 2]; 4], tu: f32, tv: f32) -> [f32; 2] {
    let t = tu.clamp(0.0, 1.0);
    let v = tv.clamp(0.0, 1.0);
    let top_u = quad_uv[0][0] + (quad_uv[1][0] - quad_uv[0][0]) * t;
    let top_v = quad_uv[0][1] + (quad_uv[1][1] - quad_uv[0][1]) * t;
    let bot_u = quad_uv[3][0] + (quad_uv[2][0] - quad_uv[3][0]) * t;
    let bot_v = quad_uv[3][1] + (quad_uv[2][1] - quad_uv[3][1]) * t;
    [top_u + (bot_u - top_u) * v, top_v + (bot_v - top_v) * v]
}

/// Projective (perspective-correct) interpolation of a `(tu, tv)` parameter across a quad's corners,
/// falling back to bilinear when the quad is affine/degenerate. Mirrors the typing tab's
/// `projective_quad_uv` exactly so a `transform_uv` overlay expands identically in both tabs.
fn projective_quad_uv(quad_uv: [[f32; 2]; 4], tu: f32, tv: f32) -> [f32; 2] {
    let p0 = quad_uv[0];
    let p1 = quad_uv[1];
    let p2 = quad_uv[2];
    let p3 = quad_uv[3];

    let a1 = p2[0] - p1[0];
    let b1 = p2[0] - p3[0];
    let c1 = p1[0] + p3[0] - p0[0] - p2[0];
    let a2 = p2[1] - p1[1];
    let b2 = p2[1] - p3[1];
    let c2 = p1[1] + p3[1] - p0[1] - p2[1];
    let det = a1 * b2 - a2 * b1;

    if det.abs() <= 1e-6 {
        return bilinear_quad_uv(quad_uv, tu, tv);
    }

    let g = (c1 * b2 - c2 * b1) / det;
    let h = (a1 * c2 - a2 * c1) / det;

    let a = p1[0] * (g + 1.0) - p0[0];
    let b = p3[0] * (h + 1.0) - p0[0];
    let c = p0[0];
    let d = p1[1] * (g + 1.0) - p0[1];
    let e = p3[1] * (h + 1.0) - p0[1];
    let f = p0[1];

    let u = tu.clamp(0.0, 1.0);
    let v = tv.clamp(0.0, 1.0);
    let denom = g * u + h * v + 1.0;
    if denom.abs() <= 1e-6 {
        return bilinear_quad_uv(quad_uv, u, v);
    }
    [(a * u + b * v + c) / denom, (d * u + e * v + f) / denom]
}

// --- Encode side (the SINGLE place geometry is serialized to the on-disk vocabulary) -------------

/// Encodes a canonical `TransformRec` (center-anchored, rotation in RADIANS) to the on-disk overlay
/// fields `img_x_px`/`img_y_px` (page px) and `rotation_deg` (DEGREES) + `scale`, writing them into
/// `obj`. The single place rad→deg happens on write. Inline v3 nodes keep radians in `TransformRec`
/// directly (no conversion); this encoder is for any disk-vocabulary serialization that still needs
/// the legacy degree fields, keeping the deg boundary in one module.
pub fn encode_transform_fields(transform: &TransformRec, obj: &mut Map<String, Value>) {
    obj.insert("img_x_px".into(), json_f32(transform.cx));
    obj.insert("img_y_px".into(), json_f32(transform.cy));
    obj.insert(
        "rotation_deg".into(),
        json_f32(transform.rotation.to_degrees()),
    );
    obj.insert("scale".into(), json_f32(transform.scale));
}

/// Encodes a `DeformRec` to the on-disk `deform_mesh` object (`{cols, rows, points_px:[[x,y],…]}`,
/// absolute page pixels) — the inverse of [`decode_deform_mesh`]'s `points_px` form.
#[must_use]
pub fn encode_deform_mesh(deform: &DeformRec) -> Value {
    let points: Vec<Value> = deform
        .points_px
        .iter()
        .map(|[x, y]| Value::Array(vec![json_f32(*x), json_f32(*y)]))
        .collect();
    serde_json::json!({ "cols": deform.cols, "rows": deform.rows, "points_px": points })
}

fn json_f32(v: f32) -> Value {
    serde_json::Number::from_f64(f64::from(v))
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

// --- Cross-entry legacy migration (ribbon x/y + top-left u/v) -------------------------------------
//
// The oldest overlay families need information spanning MULTIPLE entries (the chapter's shared ribbon
// scale) or the overlay PNG footprint, so they cannot be resolved by the per-entry `decode_*` above.
// This step normalizes them to the modern center-anchored `img_u`/`img_v` so the per-entry decoder
// then resolves them correctly. Shared by the typing tab's loader and the doc loader so an old chapter
// decodes identically in both (preventing the "everything snaps to page-center" corruption when the
// doc, now authoritative, migrates a chapter to the inline v3 payload).

/// True when the entry already uses a modern center-anchored placement (`img_x_px`/`img_y_px` or
/// `img_u`/`img_v`) and needs no cross-entry migration.
#[must_use]
pub fn overlay_entry_is_modern(obj: &Map<String, Value>) -> bool {
    obj.contains_key("img_x_px")
        || obj.contains_key("img_y_px")
        || obj.contains_key("img_u")
        || obj.contains_key("img_v")
}

/// Parses the 1-based page number from a legacy overlay `page` string such as `"1_1"` or `"1_19"`
/// (the trailing underscore-separated group is the page number).
fn legacy_overlay_page_number(page: &str) -> Option<usize> {
    let last = page.split('_').next_back()?;
    if last.is_empty() || !last.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    last.parse::<usize>().ok().filter(|&n| n >= 1)
}

/// Resolves the page index of a legacy overlay entry. An explicit `img_idx` is authoritative and
/// returned as-is (NOT clamped — the caller may know only a subset of pages, e.g. the doc loading one
/// page at a time; clamping an explicit index would misplace another page's overlay onto this one).
/// Only a `page`-string-derived index is clamped into `0..page_count` (its `<group>_<page>` numbering
/// can exceed the real page count for malformed data).
fn legacy_overlay_page_idx(obj: &Map<String, Value>, page_count: usize) -> Option<usize> {
    if let Some(idx) = obj
        .get("img_idx")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
    {
        return Some(idx);
    }
    let page = obj.get("page").and_then(Value::as_str)?;
    let number = legacy_overlay_page_number(page)?;
    Some((number - 1).min(page_count.saturating_sub(1)))
}

/// Migrates legacy text-overlay entries to the modern center-anchored `img_u`/`img_v` placement so the
/// per-entry [`decode_overlay_placement`] resolves them correctly. Modern entries pass through
/// unchanged. Two cross-entry families are handled:
/// - absolute ribbon `x`/`y` (+ optional `region_w`/`region_h`) with no `img_idx`/`u`/`v`: the
///   chapter's continuous-ribbon scale is recovered via [`LegacyRibbonGeometry`] from all such entries,
///   then each region center maps to normalized `img_u`/`img_v`;
/// - normalized top-left `u`/`v`: converted to a CENTER anchor by adding half the overlay PNG footprint
///   (`png_size * scale`), matching the modern center convention.
///
/// `png_size(obj)` supplies the overlay PNG `(width, height)` in pixels for the top-left case (the
/// caller owns image IO; return `(0.0, 0.0)` when unknown). `page_sizes[idx] = [w, h]` are page pixels.
#[must_use]
pub fn migrate_overlay_entries<F>(
    items: &[Value],
    page_sizes: &std::collections::HashMap<usize, [usize; 2]>,
    mut png_size: F,
) -> Vec<Value>
where
    F: FnMut(&Map<String, Value>) -> (f32, f32),
{
    let page_count = page_sizes.keys().copied().max().map_or(0, |m| m + 1);
    if page_count == 0 {
        return items.to_vec();
    }

    // Page aspect ratios (height / width) for ribbon scale recovery.
    let mut page_aspect = vec![1.0_f64; page_count];
    for (idx, size) in page_sizes {
        if *idx < page_aspect.len() {
            page_aspect[*idx] = (size[1].max(1) as f64) / (size[0].max(1) as f64);
        }
    }

    // Recover the shared ribbon scale from Family-A entries (absolute x/y, no u/v).
    let mut ribbon_points: Vec<(usize, f64, f64)> = Vec::new();
    for obj in items.iter().filter_map(Value::as_object) {
        if overlay_entry_is_modern(obj) || obj.contains_key("u") || obj.contains_key("v") {
            continue;
        }
        let (Some(x), Some(y)) = (
            obj.get("x").and_then(Value::as_f64),
            obj.get("y").and_then(Value::as_f64),
        ) else {
            continue;
        };
        let Some(idx) = legacy_overlay_page_idx(obj, page_count) else {
            continue;
        };
        let rw = obj.get("region_w").and_then(Value::as_f64).unwrap_or(0.0);
        let rh = obj.get("region_h").and_then(Value::as_f64).unwrap_or(0.0);
        ribbon_points.push((idx, x + rw / 2.0, y + rh / 2.0));
    }
    let ribbon = (!ribbon_points.is_empty()).then(|| {
        crate::project::LegacyRibbonGeometry::from_legacy_points(page_aspect, &ribbon_points)
    });

    items
        .iter()
        .map(|item| {
            let Some(obj) = item.as_object() else {
                return item.clone();
            };
            if overlay_entry_is_modern(obj) {
                return item.clone();
            }
            let Some(idx) = legacy_overlay_page_idx(obj, page_count) else {
                return item.clone();
            };
            let page_size = page_sizes.get(&idx).copied().unwrap_or([1, 1]);

            let center_uv = if let (Some(u), Some(v)) = (
                obj.get("u").and_then(value_f32),
                obj.get("v").and_then(value_f32),
            ) {
                // Legacy normalized top-left anchor -> center: add half the PNG footprint.
                let scale = obj
                    .get("scale")
                    .or_else(|| obj.get("user_scale"))
                    .and_then(value_f32)
                    .unwrap_or(1.0)
                    .max(0.01);
                let (pw, ph) = png_size(obj);
                let top_left = uv_to_page_px([u, v], page_size);
                let center_px = [top_left[0] + pw * scale * 0.5, top_left[1] + ph * scale * 0.5];
                Some(page_px_to_uv(center_px, page_size))
            } else if let (Some(x), Some(y), Some(geom)) = (
                obj.get("x").and_then(Value::as_f64),
                obj.get("y").and_then(Value::as_f64),
                ribbon.as_ref(),
            ) {
                // Legacy absolute ribbon coordinates (region top-left) -> normalized center.
                let rw = obj.get("region_w").and_then(Value::as_f64).unwrap_or(0.0);
                let rh = obj.get("region_h").and_then(Value::as_f64).unwrap_or(0.0);
                let (cu, cv) = geom.to_uv(idx, x + rw / 2.0, y + rh / 2.0);
                Some([cu as f32, cv as f32])
            } else {
                None
            };

            let mut out = obj.clone();
            out.insert(
                "img_idx".to_string(),
                Value::from(u64::try_from(idx).unwrap_or(0)),
            );
            if let Some([img_u, img_v]) = center_uv {
                out.insert("img_u".to_string(), Value::from(img_u));
                out.insert("img_v".to_string(), Value::from(img_v));
                out.remove("u");
                out.remove("v");
                out.remove("x");
                out.remove("y");
            }
            Value::Object(out)
        })
        .collect()
}

/// Page pixels → normalized page-relative uv (inverse of `uv_to_page_px`, with the same clamping).
fn page_px_to_uv(page_px: [f32; 2], page_size: [usize; 2]) -> [f32; 2] {
    let clamped = clamp_page_point(page_px, page_size);
    [
        clamped[0] / page_size[0].max(1) as f32,
        clamped[1] / page_size[1].max(1) as f32,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn decodes_placement_with_deg_to_rad() {
        let o = obj(serde_json::json!({
            "img_x_px": 100.0, "img_y_px": 50.0, "rotation_deg": 90.0, "scale": 2.0
        }));
        let p = decode_overlay_placement(&o, [1000, 1000]);
        assert!((p.transform.cx - 100.0).abs() < 1e-6);
        assert!((p.transform.cy - 50.0).abs() < 1e-6);
        assert!((p.transform.rotation - std::f32::consts::FRAC_PI_2).abs() < 1e-5, "90° → π/2 rad");
        assert!((p.transform.scale - 2.0).abs() < 1e-6);
        assert!(p.deform.is_none());
    }

    #[test]
    fn falls_back_to_page_center_and_clamps_scale() {
        // No position vocabulary → page center (0.5, 0.5) in page px. scale 0 → floored.
        let o = obj(serde_json::json!({ "scale": 0.0 }));
        let p = decode_overlay_placement(&o, [8, 6]);
        assert!((p.transform.cx - 4.0).abs() < 1e-6, "center x = page_w/2");
        assert!((p.transform.cy - 3.0).abs() < 1e-6, "center y = page_h/2");
        assert!(p.transform.scale >= 0.01, "scale floored");
        assert!((p.transform.rotation - 0.0).abs() < 1e-6);
    }

    #[test]
    fn decodes_legacy_per_entry_vocabulary() {
        // Reviewer regression probe: a raw legacy entry {u, v, angle, user_scale, transform_uv} must
        // decode to real geometry — NOT snap to center/0/1. u/v are CENTER-anchor normalized (page px),
        // `angle` aliases rotation_deg, `user_scale` aliases scale, `transform_uv` → a deform mesh.
        let page = [200, 100];
        let o = obj(serde_json::json!({
            "u": 0.25, "v": 0.75, "angle": 30.0, "user_scale": 1.5,
            "transform_uv": [[0.1, 0.1], [0.9, 0.1], [0.9, 0.9], [0.1, 0.9]]
        }));
        let p = decode_overlay_placement(&o, page);
        assert!((p.transform.cx - 50.0).abs() < 1e-3, "u 0.25 * 200 = 50 px (not centered)");
        assert!((p.transform.cy - 75.0).abs() < 1e-3, "v 0.75 * 100 = 75 px");
        assert!((p.transform.rotation - 30.0_f32.to_radians()).abs() < 1e-5, "`angle` → rad");
        assert!((p.transform.scale - 1.5).abs() < 1e-6, "`user_scale` alias");
        let d = p.deform.expect("transform_uv expands to a deform mesh");
        assert_eq!((d.cols, d.rows), (DEFORM_SURFACE_COLS, DEFORM_SURFACE_ROWS));
        // Corner (0,0) of the surface is the quad's TL corner (0.1, 0.1) in page px.
        assert!((d.points_px[0][0] - 20.0).abs() < 1e-2, "TL u 0.1 * 200 = 20 px");
        assert!((d.points_px[0][1] - 10.0).abs() < 1e-2, "TL v 0.1 * 100 = 10 px");
    }

    #[test]
    fn modern_format_decode_unchanged() {
        // Modern img_x_px/rotation_deg/scale/deform_mesh decode unaffected by the legacy additions.
        let page = [500, 500];
        let o = obj(serde_json::json!({
            "img_x_px": 123.0, "img_y_px": 45.0, "rotation_deg": 10.0, "scale": 2.0,
            "deform_mesh": { "cols": 2, "rows": 2, "points_px": [[1.0,2.0],[3.0,4.0],[5.0,6.0],[7.0,8.0]] }
        }));
        let p = decode_overlay_placement(&o, page);
        assert!((p.transform.cx - 123.0).abs() < 1e-6);
        assert!((p.transform.cy - 45.0).abs() < 1e-6);
        assert!((p.transform.rotation - 10.0_f32.to_radians()).abs() < 1e-5);
        assert!((p.transform.scale - 2.0).abs() < 1e-6);
        let d = p.deform.expect("explicit deform_mesh wins");
        assert_eq!((d.cols, d.rows), (2, 2));
        assert_eq!(d.points_px[3], [7.0, 8.0]);
    }

    #[test]
    fn legacy_overlay_page_number_parses_trailing_group() {
        assert_eq!(legacy_overlay_page_number("1_1"), Some(1));
        assert_eq!(legacy_overlay_page_number("1_19"), Some(19));
        assert_eq!(legacy_overlay_page_number("01"), Some(1));
        assert_eq!(legacy_overlay_page_number("1_"), None);
        assert_eq!(legacy_overlay_page_number("abc"), None);
    }

    #[test]
    fn legacy_overlay_page_idx_prefers_img_idx_then_page() {
        let mut with_idx = Map::new();
        with_idx.insert("img_idx".to_string(), Value::from(3u64));
        assert_eq!(legacy_overlay_page_idx(&with_idx, 10), Some(3));

        let mut with_page = Map::new();
        with_page.insert("page".to_string(), Value::from("1_5"));
        assert_eq!(legacy_overlay_page_idx(&with_page, 10), Some(4));

        // Explicit img_idx is authoritative and NOT clamped (the doc may know only a subset of pages).
        assert_eq!(legacy_overlay_page_idx(&with_idx, 2), Some(3));
        // A `page`-string index IS clamped into range.
        let mut big_page = Map::new();
        big_page.insert("page".to_string(), Value::from("1_99"));
        assert_eq!(legacy_overlay_page_idx(&big_page, 3), Some(2), "page-string clamped");
        assert_eq!(legacy_overlay_page_idx(&Map::new(), 10), None);
    }

    #[test]
    fn migrate_family_a_absolute_ribbon_to_center_uv() {
        // Two stacked pages (width 100, heights 200 and 300). Absolute ribbon x/y entries normalize to
        // center img_u/img_v; a modern entry passes through unchanged.
        let mut page_sizes: std::collections::HashMap<usize, [usize; 2]> =
            std::collections::HashMap::new();
        page_sizes.insert(0, [100, 200]);
        page_sizes.insert(1, [100, 300]);

        let items = vec![
            serde_json::json!({"page":"1_1","x":10.0,"y":2.0,"region_w":20.0,"region_h":4.0,"file":"a.png"}),
            serde_json::json!({"page":"1_1","x":10.0,"y":190.0,"region_w":20.0,"region_h":4.0,"file":"b.png"}),
            serde_json::json!({"page":"1_2","x":10.0,"y":210.0,"region_w":20.0,"region_h":4.0,"file":"c.png"}),
            serde_json::json!({"page":"1_2","x":10.0,"y":490.0,"region_w":20.0,"region_h":4.0,"file":"d.png"}),
            serde_json::json!({"img_idx":0,"img_x_px":50.0,"img_y_px":60.0,"file":"e.png","overlay_type":"text"}),
        ];

        let out = migrate_overlay_entries(&items, &page_sizes, |_| (0.0, 0.0));
        assert_eq!(out.len(), items.len());
        for entry in out.iter().take(4) {
            let obj = entry.as_object().expect("stays an object");
            assert!(obj.contains_key("img_u") && obj.contains_key("img_v"));
            assert!(!obj.contains_key("x") && !obj.contains_key("y"));
        }
        assert_eq!(out[0].get("img_idx").and_then(Value::as_u64), Some(0));
        assert_eq!(out[2].get("img_idx").and_then(Value::as_u64), Some(1));
        let v_top = out[0].get("img_v").and_then(value_f32).unwrap_or(1.0);
        let v_bot = out[1].get("img_v").and_then(value_f32).unwrap_or(0.0);
        assert!(v_top < v_bot, "vertical order preserved");
        assert_eq!(out[4], items[4], "modern entry unchanged");
    }

    #[test]
    fn ribbon_migration_needs_all_page_aspects_not_just_the_loaded_page() {
        // Regression (reviewer HIGH): the absolute-ribbon family recovers a CHAPTER-WIDE scale from
        // every page's aspect ratio. Passing only the loaded page's size makes every other page's
        // aspect default to a square 1.0 → wrong ribbon scale → wrong `img_u`/`img_v`. Non-uniform
        // aspects (page0 = 100×250 → 2.5, page1 = 100×300 → 3.0) expose the divergence.
        let full: std::collections::HashMap<usize, [usize; 2]> =
            [(0, [100, 250]), (1, [100, 300])].into_iter().collect();
        // What the BUGGY doc loader used to build: only the page being loaded (page 1).
        let single_page1: std::collections::HashMap<usize, [usize; 2]> =
            [(1, [100, 300])].into_iter().collect();

        // A page-0 ribbon entry constrains the ribbon scale; a page-1 ribbon entry is what we compare.
        let items = vec![
            serde_json::json!({"page":"1_1","x":10.0,"y":10.0,"region_w":20.0,"region_h":4.0,"file":"a.png"}),
            serde_json::json!({"page":"1_2","x":10.0,"y":150.0,"region_w":20.0,"region_h":4.0,"file":"b.png"}),
        ];

        let out_full = migrate_overlay_entries(&items, &full, |_| (0.0, 0.0));
        let out_single = migrate_overlay_entries(&items, &single_page1, |_| (0.0, 0.0));

        let v = |out: &[Value], i: usize| out[i].as_object().unwrap().get("img_v").and_then(value_f32);
        let v1_full = v(&out_full, 1).expect("page-1 img_v (full map)");
        let v1_single = v(&out_single, 1).expect("page-1 img_v (single-page map)");

        // The fix: with the FULL map the page-1 overlay decodes to a real, in-range center, and it
        // DIFFERS from the single-page (square-aspect-placeholder) result — proving the bug is gone.
        assert!((0.0..=1.0).contains(&v1_full), "full-map img_v is a valid page coordinate");
        assert!(
            (v1_full - v1_single).abs() > 1e-3,
            "single-page migration diverges from the correct full-map result \
             (was the silent corruption): full={v1_full} single={v1_single}"
        );

        // And the full-map doc migration equals the full-page typing migration — they share this exact
        // call with the same full map, so any later page yields identical geometry in both tabs.
        let out_typing = migrate_overlay_entries(&items, &full, |_| (0.0, 0.0));
        assert_eq!(out_full, out_typing, "doc and typing migrate identically with the full page map");
    }

    #[test]
    fn migrate_top_left_uv_to_center_via_png_footprint() {
        // Legacy top-left `u`/`v` → center by adding half the PNG footprint (png_size * scale).
        let mut page_sizes: std::collections::HashMap<usize, [usize; 2]> =
            std::collections::HashMap::new();
        page_sizes.insert(0, [100, 100]);
        let items = vec![serde_json::json!({
            "img_idx": 0, "u": 0.0, "v": 0.0, "scale": 1.0, "file": "a.png"
        })];
        // PNG is 20x40 px → center offset (10, 20) px → uv (0.1, 0.2) on a 100×100 page.
        let out = migrate_overlay_entries(&items, &page_sizes, |_| (20.0, 40.0));
        let obj = out[0].as_object().unwrap();
        let u = obj.get("img_u").and_then(value_f32).unwrap();
        let v = obj.get("img_v").and_then(value_f32).unwrap();
        assert!((u - 0.1).abs() < 1e-4, "top-left u 0 + 10px/100 = 0.1");
        assert!((v - 0.2).abs() < 1e-4, "top-left v 0 + 20px/100 = 0.2");
    }

    #[test]
    fn read_overlay_entries_reads_array_or_empty() {
        let dir = std::env::temp_dir().join(format!("tp_oe_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Missing file → empty.
        assert!(read_overlay_entries(&[dir.as_path()]).is_empty());

        // A small text_info.json with one overlay object reads back in order.
        let arr = serde_json::json!([
            { "uid": "a", "overlay_type": "text", "file": "a.png" },
            { "uid": "b", "overlay_type": "image", "file": "b.png" }
        ]);
        std::fs::write(dir.join("text_info.json"), serde_json::to_string(&arr).unwrap()).unwrap();
        let entries = read_overlay_entries(&[dir.as_path()]);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["uid"], "a");
        assert_eq!(entries[1]["uid"], "b");

        // Falls through to the second dir when the first lacks the file.
        let empty = std::env::temp_dir().join(format!("tp_oe_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        let entries = read_overlay_entries(&[empty.as_path(), dir.as_path()]);
        assert_eq!(entries.len(), 2, "falls through to the dir that has the file");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn decodes_valid_deform_and_rejects_degenerate() {
        let page = [100, 100];
        let good = serde_json::json!({
            "cols": 2, "rows": 2,
            "points_px": [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]]
        });
        let d = decode_deform_mesh(Some(&good), page).expect("valid 2×2 grid");
        assert_eq!((d.cols, d.rows), (2, 2));
        assert_eq!(d.points_px.len(), 4);
        assert_eq!(d.points_px[3], [1.0, 1.0]);

        // 1-D grid and point-count mismatch are rejected.
        assert!(decode_deform_mesh(Some(&serde_json::json!({
            "cols": 1, "rows": 2, "points_px": [[0.0, 0.0], [0.0, 1.0]]
        })), page).is_none());
        assert!(decode_deform_mesh(Some(&serde_json::json!({
            "cols": 2, "rows": 2, "points_px": [[0.0, 0.0]]
        })), page).is_none());
        assert!(decode_deform_mesh(None, page).is_none());
    }

    #[test]
    fn decodes_points_uv_deform_to_page_px() {
        // The legacy `points_uv` form is normalized; it converts to page px via page_size.
        let page = [200, 100];
        let mesh = serde_json::json!({
            "cols": 2, "rows": 2,
            "points_uv": [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]]
        });
        let d = decode_deform_mesh(Some(&mesh), page).expect("valid uv grid");
        assert_eq!(d.points_px[0], [0.0, 0.0]);
        assert_eq!(d.points_px[3], [200.0, 100.0], "uv (1,1) → page bottom-right px");
    }

    #[test]
    fn stable_overlay_uid_is_deterministic_and_basename_stable() {
        // Same input → identical uid (no randomness): the typing loader and the shared-doc decoder
        // MUST agree on the uid of a uid-less legacy overlay or the typing tab double-renders it.
        let a = stable_overlay_uid("typing_overlay_p0001_1700000000.png");
        let b = stable_overlay_uid("typing_overlay_p0001_1700000000.png");
        assert_eq!(a, b, "deterministic: same name → same uid");

        // Only the final path component seeds the uid, so a bare name and a `dir/name` agree (the
        // decoder passes the bare `file`, callers may pass a joined path).
        let bare = stable_overlay_uid("x.png");
        let nested = stable_overlay_uid("a/b/x.png");
        assert_eq!(bare, nested, "basename-stable: dir prefix is ignored");

        // Distinct names must not collide.
        assert_ne!(
            stable_overlay_uid("x.png"),
            stable_overlay_uid("y.png"),
            "different names → different uids"
        );

        // The output is a canonical UUID string.
        assert!(
            uuid::Uuid::parse_str(&a).is_ok(),
            "stable_overlay_uid returns a parseable UUID"
        );
    }
}
