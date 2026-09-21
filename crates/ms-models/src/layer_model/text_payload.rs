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
        if let Ok(raw) = ms_storage::global::storage().read_to_string(path.to_string_lossy().as_ref())
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
// (Cross-entry legacy families — absolute ribbon `x`/`y`+`region_w`/`region_h`, and the two OLDEST
// top-left-anchored bare `u`/`v` generations that need the overlay's displayed footprint — are
// normalized to `img_u`/`img_v` UPSTREAM by [`migrate_overlay_entries`], the single shared cross-entry
// step that the typing loader, the doc loader and the chapter migration all run before this decoder.
// Bare `u`/`v` from every LATER legacy generation is already a centre and passes through that step
// verbatim — see [`legacy_uv_anchor`].)

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

// --- Cross-entry legacy migration (ribbon x/y + top-left-anchored u/v) ---------------------------
//
// The oldest overlay families need information spanning MULTIPLE entries (the chapter's shared ribbon
// scale) or the overlay's displayed footprint, so they cannot be resolved by the per-entry `decode_*`
// above. This step normalizes them to the modern center-anchored `img_u`/`img_v` so the per-entry
// decoder then resolves them correctly. Shared by the typing tab's loader and the doc loader so an old
// chapter decodes identically in both (preventing the "everything snaps to page-center" corruption when
// the doc, now authoritative, migrates a chapter to the inline v3 payload).
//
// Bare `u`/`v` is a CENTRE anchor by default and is shifted only for the two oldest generations, which
// [`legacy_uv_anchor`] separates (they need DIFFERENT shifts — see [`LegacyUvAnchor`]); treating every
// `u`/`v` entry as a top-left anchor displaced the whole Qt-2.X majority down-right by half its own
// footprint, and treating both top-left generations alike mis-shifted each by a `user_scale` factor.

/// True when the entry already uses a modern center-anchored placement (`img_x_px`/`img_y_px` or
/// `img_u`/`img_v`) and needs no cross-entry migration.
#[must_use]
pub fn overlay_entry_is_modern(obj: &Map<String, Value>) -> bool {
    obj.contains_key("img_x_px")
        || obj.contains_key("img_y_px")
        || obj.contains_key("img_u")
        || obj.contains_key("img_v")
}

/// Anchor convention of a legacy overlay's bare `u`/`v`, one variant per legacy writer generation.
///
/// `text_info.json` was written by four generations of the legacy Python app, and the meaning of
/// `u`/`v` changed twice. `W` is the page width in px, `png_w`/`png_h` the overlay PNG's own pixels
/// and `us` the entry's `user_scale` (default `1.0`):
///
/// | Generation | Marker keys | `u`/`v` is… | Displayed size |
/// |---|---|---|---|
/// | 1. Tkinter | `region_w`/`region_h`, else `page` with no Qt key | top-left of the **scaled** box | `w_frac*W*us` × aspect |
/// | 2. Qt pre-`align` | `text`, no `align`, no `style` | top-left of the **unscaled** box | `w_frac*W*us` × aspect |
/// | 3a. Qt with `align` | `text` + top-level `align` | the CENTRE | native PNG × `us` |
/// | 3b. Qt 2.X | nested `style` object | the CENTRE | native PNG × `us` |
///
/// Generation 1 (`old_or_test/ui/tabs/text_tab_new/text_ops.py:245-254` writes it; `.../overlays.py:92`
/// draws it with `canvas.create_image(..., anchor='nw')` and `.../overlays.py:270-280` keeps that
/// top-left pinned across a rescale) anchors the box it actually DISPLAYS, so the shift to a centre is
/// half of `w_frac*W*us` — the size the save path `.../saving.py:56,80` baked into the archived pages.
/// (`overlays.py:82` dropping `us` on the initial load is a live-preview-only legacy bug and is
/// deliberately NOT reproduced.)
///
/// Generation 2 has no surviving writer tree of its own, but BOTH Qt overlay items that do survive pin
/// the transform origin to the box centre before scaling or rotating —
/// `old_or_test/text_tab_old/text_overlay_item.py:118`
/// (`setTransformOriginPoint(self.boundingRect().center())`) and
/// `old_or_test/2.X/ui_new/tabs/text_tab/text_overlay_item.py:222`
/// (`setTransformOriginPoint(self._rect.center())`). Scaling is therefore centre-preserving: `user_scale`
/// moves neither corner of the *unscaled* box the position refers to, so the shift to a centre is half of
/// `w_frac*W` and `us` does not enter the position at all. Validated on all 186 entries of the one
/// chapter that uses this generation: median error 0.5 px, p90 2.4 px, against 111 px median for the
/// centre reading.
///
/// Generations 3a/3b already store the true visual centroid (`setPos(centre - pixmap*user_scale/2)`),
/// so their `u`/`v` is copied verbatim.
///
/// Classification order matters and is asymmetric in cost: a false top-left re-introduces the "every
/// overlay displaced down-right by half its footprint" bug across a whole chapter of the legacy
/// majority, while a false centre only under-shifts an older, rarer chapter. Hence the Tkinter markers
/// are tested FIRST — `region_w`/`region_h` is emitted by every Tkinter entry and by no Qt writer, so
/// a Tkinter entry that also carries `text` can never fall through into the Qt clause. `align` is the
/// generation-3a marker because it appeared in one writer step together with nine siblings
/// (`line_spacing`, `extra_vpadding`, `stroke_width`, `grad_angle_deg`, …); `stroke_color_rgba`,
/// `glow_color_rgba` and `reflect` are written only when non-null and therefore must NEVER be used as
/// markers. `user_scale` cannot discriminate either — `overlays.py` writes it back into a Tkinter
/// entry when the user key-scales an overlay, which is why it may only ever VETO the weaker `page`
/// clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyUvAnchor {
    /// The modern reading: `u`/`v` is already the overlay's centre (Qt with `align`, and Qt 2.X).
    Centre,
    /// Tkinter: `u`/`v` is the top-left of the SCALED box (`w_frac * page_w * user_scale`).
    TkinterScaledTopLeft,
    /// Qt pre-`align`: `u`/`v` is the top-left of the UNSCALED box (`w_frac * page_w`); `user_scale`
    /// scales about the box centre and so does not shift the position.
    QtUnscaledTopLeft,
}

/// Classifies the legacy writer generation an entry's bare `u`/`v` belongs to. See [`LegacyUvAnchor`]
/// for the generation table, the markers and why the clause order is what it is.
#[must_use]
pub fn legacy_uv_anchor(obj: &Map<String, Value>) -> LegacyUvAnchor {
    // Tkinter first: `region_w`/`region_h` is exclusive to that writer, so an entry carrying it is
    // Tkinter even when it also has `text`, and can never reach the Qt clause below.
    if obj.contains_key("region_w") || obj.contains_key("region_h") {
        return LegacyUvAnchor::TkinterScaledTopLeft;
    }
    // Pre-`region_w` Tkinter entries that `overlays.py` migrated in place: `page`, and none of the
    // keys only a Qt writer emits.
    if obj.contains_key("page")
        && !obj.contains_key("text")
        && !obj.contains_key("style")
        && !obj.contains_key("user_scale")
    {
        return LegacyUvAnchor::TkinterScaledTopLeft;
    }
    // Qt before the `align` writer step: a text entry with neither `align` nor a nested `style`.
    if obj.contains_key("text") && !obj.contains_key("align") && !obj.contains_key("style") {
        return LegacyUvAnchor::QtUnscaledTopLeft;
    }
    LegacyUvAnchor::Centre
}

/// Page-pixel geometry a top-left-anchored legacy entry must be migrated with.
#[derive(Debug, Clone, Copy)]
struct LegacyTopLeftGeometry {
    /// Half-extent to ADD to the top-left `u`/`v` (in page px) to obtain the centre. `[0.0, 0.0]`
    /// when nothing is known about the overlay's size, so the entry is left unshifted.
    shift_px: [f32; 2],
    /// Explicit uniform `scale` the migrated entry must carry so the modern decoder reproduces the
    /// legacy DISPLAYED width from the PNG's own pixels. `None` when `w_frac` or the PNG size is
    /// unknown — the entry then keeps whatever scale it already had.
    scale: Option<f32>,
}

/// Displayed/anchor geometry, in PAGE pixels, of a top-left-anchored overlay whose PNG measures
/// `png_w`×`png_h` px.
///
/// Neither legacy loader drew the strip at its native size: both resized it to `w_frac * page_width`
/// and took the height from the PNG aspect (Tkinter `overlays.py`: `target_w = int(w_frac * disp_w)`,
/// `scale = target_w / base.width`, `target_h = int(base.height * scale)`). The two generations differ
/// only in what `user_scale` does to the ANCHOR:
/// - [`LegacyUvAnchor::TkinterScaledTopLeft`]: the anchor is the corner of the scaled box, so the
///   half-extent uses `w_frac * page_w * user_scale`;
/// - [`LegacyUvAnchor::QtUnscaledTopLeft`]: scaling is centre-preserving, so the half-extent uses the
///   UNSCALED `w_frac * page_w` and `user_scale` is ignored here.
///
/// Both generations displayed the strip at `w_frac * page_w * user_scale` wide, which is what
/// [`LegacyTopLeftGeometry::scale`] reports. Degradation, in order:
/// - `w_frac` usable AND the PNG size known: the full result above, with an explicit `scale`;
/// - `w_frac` usable but the PNG size unknown (the caller's lookup returns `(0.0, 0.0)` for a missing
///   file): the HORIZONTAL half-shift is still fully determined by `w_frac * page_w * anchor_scale`, so
///   it is applied; the vertical one needs the PNG aspect and stays `0.0`, and no `scale` is emitted
///   (it divides by `png_w`);
/// - `w_frac` absent, non-finite or `<= 0`: the half-extent degrades to the native PNG footprint —
///   times `user_scale` for the Tkinter family, unscaled for the Qt one, mirroring the rule above —
///   which is `(0.0, 0.0)`, i.e. no shift, when the PNG size is unknown too, and no `scale` is emitted.
///
/// Never panics and never divides by a zero width.
///
/// Approximation for a ROTATED Tkinter entry. The half-extent computed here is that of the UNROTATED
/// box, but the Tkinter loader and its save path rotate with `expand=True` BEFORE measuring
/// (`old_or_test/ui/tabs/text_tab_new/overlays.py:76`, `.../saving.py:50`), so the footprint they
/// actually anchored is the larger EXPANDED bounding box — a rotated entry's migrated centre is
/// therefore approximate. The Tkinter writer does persist a rotation (`.../overlays.py:393` rotates and
/// `:421` writes the resulting `angle` back into the entry), so such entries are possible in principle;
/// a scan of every archived `text_info.json` in the corpus this migration was measured against found
/// none — 0 rotated among 1381 Tkinter entries — so the approximation is currently unexercised. It is
/// not corrected because the caller supplies only the UNROTATED PNG size, from which the expanded
/// rotated extent cannot be recovered.
fn legacy_top_left_geometry(
    anchor: LegacyUvAnchor,
    obj: &Map<String, Value>,
    page_size: [usize; 2],
    png_w: f32,
    png_h: f32,
) -> LegacyTopLeftGeometry {
    // `user_scale` (or its modern `scale` spelling); the same floor the per-entry decoder applies.
    let user_scale = obj
        .get("scale")
        .or_else(|| obj.get("user_scale"))
        .and_then(value_f32)
        .filter(|s| s.is_finite())
        .unwrap_or(1.0)
        .max(0.01);
    // The Tkinter anchor moves with `user_scale`; the Qt pre-`align` one does not (centre-preserving
    // scaling), so only the former folds it into the anchor box.
    let anchor_scale = match anchor {
        LegacyUvAnchor::TkinterScaledTopLeft => user_scale,
        LegacyUvAnchor::QtUnscaledTopLeft | LegacyUvAnchor::Centre => 1.0,
    };
    let w_frac = obj
        .get("w_frac")
        .and_then(value_f32)
        .filter(|f| f.is_finite() && *f > 0.0);
    if let Some(w_frac) = w_frac {
        let page_w = page_size[0].max(1) as f32;
        let anchor_w = w_frac * page_w * anchor_scale;
        if png_w > 0.0 && png_h > 0.0 {
            let displayed_w = w_frac * page_w * user_scale;
            return LegacyTopLeftGeometry {
                shift_px: [anchor_w * 0.5, anchor_w * (png_h / png_w) * 0.5],
                scale: Some(displayed_w / png_w),
            };
        }
        // PNG size unknown, `w_frac` known. The anchor box's WIDTH does not depend on the PNG at all,
        // so the horizontal half-shift stays exact and is applied; only the height came from the PNG
        // aspect, so the vertical half-shift degrades to 0 rather than to a guess. `scale` is withheld
        // because it is `displayed_w / png_w`. This path is close to dead in practice: the typing
        // decoder `decode_overlay_from_storage_entry` (`crates/ms-tab-typing/src/tab/codec.rs`,
        // `image::open(&image_path).ok()?`) DROPS an overlay whose PNG will not load, so an entry that
        // reaches rendering with an unreadable PNG normally does not exist — but the dimension lookup
        // and that decode consult different directory sets, so the case is not structurally impossible.
        return LegacyTopLeftGeometry { shift_px: [anchor_w * 0.5, 0.0], scale: None };
    }
    LegacyTopLeftGeometry {
        shift_px: [png_w * anchor_scale * 0.5, png_h * anchor_scale * 0.5],
        scale: None,
    }
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
/// - normalized `u`/`v`: already a CENTER anchor for the two newest legacy generations, and copied to
///   `img_u`/`img_v` verbatim. The two top-left generations [`legacy_uv_anchor`] recognizes are
///   shifted by half their anchor box, which differs per generation (see [`LegacyUvAnchor`] and
///   [`legacy_top_left_geometry`]).
///
/// Deliberate migration-time NORMALIZATION of the displayed size, for the two top-left generations
/// only: both drew the strip at `w_frac * page_w * user_scale`, while the modern decoder sizes an
/// overlay from its PNG's own pixels times the placement scale — and the two have drifted apart (a
/// median of 14 px, up to ~6 %, in the measured chapters). So when both `w_frac` and the PNG size are
/// known, this step writes an explicit `scale = w_frac * page_w * user_scale / png_w` into the entry.
/// [`decode_overlay_placement`] prefers `scale` over the `user_scale` alias, so this cleanly overrides
/// it without having to delete the legacy key. Centre-anchored entries are left alone: their PNG is
/// already native and their `user_scale` is already the right factor.
///
/// `png_size(obj)` supplies the overlay PNG `(width, height)` in pixels for the top-left cases (the
/// caller owns image IO; return `(0.0, 0.0)` when unknown — the entry then keeps whatever half-shift
/// `w_frac` alone determines, and gets no injected `scale`; see [`legacy_top_left_geometry`]).
/// `page_sizes[idx] = [w, h]` are page pixels. An entry whose resolved page index is ABSENT from
/// `page_sizes` has no known page geometry: a top-left `u`/`v` is then copied verbatim rather than
/// shifted by a half-extent derived from a placeholder page.
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
        ms_project::LegacyRibbonGeometry::from_legacy_points(page_aspect, &ribbon_points)
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
            // `None` = this entry's page is not in the map at all (an explicit `img_idx` is not
            // clamped, and the map may be sparse). The `[1, 1]` placeholder keeps the ribbon and
            // `Centre` paths working unchanged; the top-left path checks `known_page_size` instead.
            let known_page_size = page_sizes.get(&idx).copied();
            let page_size = known_page_size.unwrap_or([1, 1]);

            // Explicit `scale` a top-left family needs so the migrated box keeps its legacy WIDTH.
            let mut normalized_scale: Option<f32> = None;
            let center_uv = if let (Some(u), Some(v)) = (
                obj.get("u").and_then(value_f32),
                obj.get("v").and_then(value_f32),
            ) {
                let anchor = legacy_uv_anchor(obj);
                match anchor {
                    LegacyUvAnchor::TkinterScaledTopLeft | LegacyUvAnchor::QtUnscaledTopLeft
                        if known_page_size.is_none() =>
                    {
                        // The page this entry belongs to is not in `page_sizes`, so the `[1, 1]`
                        // placeholder above is NOT the page. Both the half-shift and the normalized
                        // `scale` are expressed in page pixels (`w_frac * page_w`), and the uv→px→uv
                        // round-trip through a 1×1 "page" is meaningless; the vertical half-extent
                        // would additionally be wrong by the true `page_w / page_h`. Leaving `u`/`v`
                        // unshifted is off by at most half a footprint, whereas shifting by a
                        // placeholder-derived extent is off by an unbounded, unknowable amount — so
                        // copy the corner verbatim and inject no `scale`.
                        Some([u, v])
                    }
                    LegacyUvAnchor::TkinterScaledTopLeft | LegacyUvAnchor::QtUnscaledTopLeft => {
                        // `u`/`v` is the overlay's TOP-LEFT corner: shift by half the anchor box the
                        // generation actually used (scaled for Tkinter, unscaled for Qt pre-`align`).
                        let (pw, ph) = png_size(obj);
                        let geometry = legacy_top_left_geometry(anchor, obj, page_size, pw, ph);
                        normalized_scale = geometry.scale;
                        let top_left = uv_to_page_px([u, v], page_size);
                        Some(page_px_to_uv(
                            [
                                top_left[0] + geometry.shift_px[0],
                                top_left[1] + geometry.shift_px[1],
                            ],
                            page_size,
                        ))
                    }
                    // Every later generation already stores the CENTRE; only the key is renamed.
                    LegacyUvAnchor::Centre => Some([u, v]),
                }
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
            // Size normalization: the legacy displayed width expressed as a modern `scale` over the
            // PNG's own pixels. `scale` wins over the `user_scale` alias in the per-entry decoder.
            if let Some(scale) = normalized_scale.filter(|s| s.is_finite() && *s > 0.0) {
                out.insert("scale".to_string(), Value::from(scale));
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
    fn migrate_tkinter_top_left_uv_uses_displayed_w_frac_footprint() {
        // Generation 1 (Tkinter, `old_or_test/ui/tabs/text_tab_new/`): a realistic entry — `page` +
        // `region_w`/`region_h`, no `text`/`style`/`user_scale`. Its `u`/`v` is the TOP-LEFT corner
        // (`overlays.py` draws it with `anchor='nw'`), and the strip was displayed at
        // `w_frac * page_width`, NOT at the PNG's native size.
        let mut page_sizes: std::collections::HashMap<usize, [usize; 2]> =
            std::collections::HashMap::new();
        page_sizes.insert(0, [100, 100]);
        let items = vec![serde_json::json!({
            "file": "a.png", "page": "1_1", "img_idx": 0,
            "u": 0.0, "v": 0.0, "w_frac": 0.4, "angle": 0.0,
            "region_w": 40, "region_h": 80
        })];
        // PNG 20×40 px (aspect 2.0); w_frac 0.4 × page 100 → displayed 40×80 px → half = (20, 40) px
        // → uv (0.2, 0.4). The NATIVE-size reading would have given (0.1, 0.2) — that is the bug.
        let out = migrate_overlay_entries(&items, &page_sizes, |_| (20.0, 40.0));
        let obj = out[0].as_object().unwrap();
        let u = obj.get("img_u").and_then(value_f32).unwrap();
        let v = obj.get("img_v").and_then(value_f32).unwrap();
        assert!((u - 0.2).abs() < 1e-4, "displayed width 0.4*100=40 px → half 20 px → u 0.2, got {u}");
        assert!((v - 0.4).abs() < 1e-4, "displayed height 40*2=80 px → half 40 px → v 0.4, got {v}");
        // Size normalization: the strip was displayed 40 px wide but its PNG is 20 px → scale 2.0.
        let scale = obj.get("scale").and_then(value_f32).unwrap();
        assert!((scale - 2.0).abs() < 1e-4, "scale = w_frac*W*us/png_w = 40/20 = 2.0, got {scale}");

        // With no `w_frac` the footprint degrades to the native PNG size times `user_scale`, and no
        // `scale` is invented (the legacy displayed width is simply unknown).
        let no_frac = vec![serde_json::json!({
            "file": "a.png", "page": "1_1", "img_idx": 0, "u": 0.0, "v": 0.0,
            "region_w": 40, "region_h": 80
        })];
        let out = migrate_overlay_entries(&no_frac, &page_sizes, |_| (20.0, 40.0));
        let obj = out[0].as_object().unwrap();
        assert!((obj.get("img_u").and_then(value_f32).unwrap() - 0.1).abs() < 1e-4);
        assert!((obj.get("img_v").and_then(value_f32).unwrap() - 0.2).abs() < 1e-4);
        assert!(!obj.contains_key("scale"), "no `w_frac` → no size normalization");

        // Unknown PNG size (the lookup returns (0,0)) WITH a usable `w_frac`: the horizontal
        // half-shift is `w_frac * page_w * us / 2 = 0.4*100/2 = 20 px` → u 0.2 and does not involve the
        // PNG at all, so it is still applied. Only the vertical half-extent came from the PNG aspect,
        // so it degrades to 0. No `scale` is invented (it divides by `png_w`) and nothing divides by
        // zero.
        let out = migrate_overlay_entries(&items, &page_sizes, |_| (0.0, 0.0));
        let obj = out[0].as_object().unwrap();
        assert!(
            (obj.get("img_u").and_then(value_f32).unwrap() - 0.2).abs() < 1e-4,
            "the `w_frac`-only horizontal half-shift survives a missing PNG"
        );
        assert!((obj.get("img_v").and_then(value_f32).unwrap() - 0.0).abs() < 1e-6);
        assert!(!obj.contains_key("scale"), "unknown PNG size → no size normalization");
    }

    #[test]
    fn migrate_tkinter_top_left_uv_folds_user_scale_into_shift_and_scale() {
        // Generation 1 WITH `user_scale`. `overlays.py:270-280` keeps the top-left pinned when the
        // user key-scales an overlay and writes `user_scale` back into the entry, and the save path
        // (`.../text_tab_new/saving.py:56,80`) bakes the strip in at `w_frac*W*user_scale` — which is
        // what the archived rendered pages contain. 220 real entries across the user's chapters
        // `ch17`/`ch18`/`ch19` carry `user_scale` between 0.185 and 1.1, so dropping the factor here
        // mis-shifts every one of them. (`overlays.py:82` ignoring `user_scale` on the INITIAL load is
        // a live-preview-only legacy bug and is deliberately not reproduced.)
        let page_sizes: std::collections::HashMap<usize, [usize; 2]> =
            [(0, [1000, 2000])].into_iter().collect();
        let items = vec![serde_json::json!({
            "file": "t_3.png", "page": "1_1", "img_idx": 0,
            "u": 0.1, "v": 0.2, "w_frac": 0.5, "angle": 0.0, "user_scale": 0.8,
            "region_w": 500, "region_h": 250
        })];
        // PNG 250×125 (aspect 0.5). Displayed 0.5*1000*0.8 = 400 px wide, 200 px tall → half
        // (200, 100) px → centre (0.1*1000+200, 0.2*2000+100) = (300, 500) px → uv (0.3, 0.25).
        let out = migrate_overlay_entries(&items, &page_sizes, |_| (250.0, 125.0));
        let obj = out[0].as_object().unwrap();
        let u = obj.get("img_u").and_then(value_f32).unwrap();
        let v = obj.get("img_v").and_then(value_f32).unwrap();
        assert!((u - 0.3).abs() < 1e-4, "half of w_frac*W*us = 200 px → u 0.3, got {u}");
        assert!((v - 0.25).abs() < 1e-4, "half of the aspect height = 100 px → v 0.25, got {v}");
        // Without the `user_scale` factor the shift would have been half of 500 px → u 0.35.
        assert!((u - 0.35).abs() > 1e-3, "the unscaled reading (u 0.35) is the defect being fixed");

        // And the displayed width is re-expressed over the PNG's own pixels: 400/250 = 1.6.
        let scale = obj.get("scale").and_then(value_f32).unwrap();
        assert!((scale - 1.6).abs() < 1e-4, "scale = w_frac*W*us/png_w = 400/250 = 1.6, got {scale}");
        // `scale` beats the `user_scale` alias in the per-entry decoder, so THIS is the factor used.
        let placement = decode_overlay_placement(obj, [1000, 2000]);
        assert!((placement.transform.scale - 1.6).abs() < 1e-4, "`scale` overrides `user_scale`");
    }

    #[test]
    fn migrate_qt_pre_align_uv_is_an_unscaled_top_left() {
        // Generation 2 (Qt before the `align` writer step). The ONE chapter on disk that uses it
        // (`ch20`, 186 entries) has exactly this key set:
        // {angle, color, file, font, img_idx, size, text, u, user_scale, v, w_frac} — `text` but
        // neither a top-level `align` nor a nested `style`.
        //
        // Measured evidence: every surviving Qt overlay item pins the transform origin to the box
        // centre — `old_or_test/text_tab_old/text_overlay_item.py:118`
        // (`setTransformOriginPoint(self.boundingRect().center())`) and
        // `old_or_test/2.X/ui_new/tabs/text_tab/text_overlay_item.py:222`
        // (`setTransformOriginPoint(self._rect.center())`) — so `user_scale` scales about the box
        // CENTRE and cannot move the UNSCALED box the stored corner refers to. Reading `u`/`v` as the
        // unscaled top-left reproduced all 186 entries with a median error of 0.5 px (p90 2.4 px);
        // reading them as a centre was off by 111 px median.
        let page_sizes: std::collections::HashMap<usize, [usize; 2]> =
            [(0, [1000, 4000])].into_iter().collect();
        let entry = |user_scale: f64| {
            serde_json::json!({
                "angle": 0.0, "color": "#000000", "file": "t_0.png", "font": "Anime Ace",
                "img_idx": 0, "size": 22, "text": "привет",
                "u": 0.2, "user_scale": user_scale, "v": 0.25, "w_frac": 0.4
            })
        };
        // PNG 500×250 (aspect 0.5). Unscaled box 0.4*1000 = 400 px wide, 200 px tall → half
        // (200, 100) px → centre (0.2*1000+200, 0.25*4000+100) = (400, 1100) px → uv (0.4, 0.275).
        let items = vec![entry(0.826), entry(1.1)];
        let out = migrate_overlay_entries(&items, &page_sizes, |_| (500.0, 250.0));

        let first = out[0].as_object().unwrap();
        assert_eq!(
            legacy_uv_anchor(items[0].as_object().unwrap()),
            LegacyUvAnchor::QtUnscaledTopLeft,
            "`text` without `align`/`style` is the pre-`align` Qt generation"
        );
        let u = first.get("img_u").and_then(value_f32).unwrap();
        let v = first.get("img_v").and_then(value_f32).unwrap();
        assert!((u - 0.4).abs() < 1e-4, "half of the UNSCALED w_frac*W = 200 px → u 0.4, got {u}");
        assert!((v - 0.275).abs() < 1e-4, "half of the aspect height = 100 px → v 0.275, got {v}");
        assert!(!first.contains_key("u") && !first.contains_key("v"), "legacy keys retired");

        // The centre must NOT depend on `user_scale`: the two entries differ only in it.
        let second = out[1].as_object().unwrap();
        assert_eq!(
            first.get("img_u"),
            second.get("img_u"),
            "centre is independent of `user_scale` (centre-preserving scaling)"
        );
        assert_eq!(first.get("img_v"), second.get("img_v"), "same for the v coordinate");

        // The displayed SIZE does depend on it: w_frac*W*us/png_w.
        let s0 = first.get("scale").and_then(value_f32).unwrap();
        let s1 = second.get("scale").and_then(value_f32).unwrap();
        assert!((s0 - 0.4 * 1000.0 * 0.826 / 500.0).abs() < 1e-4, "scale 0.6608, got {s0}");
        assert!((s1 - 0.4 * 1000.0 * 1.1 / 500.0).abs() < 1e-4, "scale 0.88, got {s1}");
    }

    #[test]
    fn migrate_qt_2x_uv_is_a_centre_and_is_never_shifted() {
        // REGRESSION. Generation 3 (Qt 2.X, `old_or_test/2.X/ui_new/tabs/text_tab/text_view.py`) is
        // the legacy majority and stores the overlay's true visual CENTRE in `u`/`v`
        // (`_on_item_changed` writes `_uv_from_scene(idx, centroid)`; the loader does
        // `setPos(centre - pixmap*user_scale/2)`). The key shape below is the real one from the
        // user's chapter `ch41` (134 entries, 690×16308 pages).
        //
        // Measured evidence that this is a centre: every rendered strip was located inside the
        // 2.x-rendered `saved/*.png` pages at `(u*W − sw/2, v*H − sh/2)` (median mean-abs-diff
        // 5.88/255, max 12.9), while the top-left reading `(u*W, v*H)` matched nothing (median 224)
        // and put 42 of 134 strips outside the page. Shifting these by half a footprint displaced
        // every overlay down-right by a median of (129, 40) px.
        let mut page_sizes: std::collections::HashMap<usize, [usize; 2]> =
            std::collections::HashMap::new();
        page_sizes.insert(0, [690, 16308]);
        let items = vec![serde_json::json!({
            "img_idx": 0, "u": 0.3742, "v": 0.5118, "w_frac": 0.37,
            "user_scale": 1.0, "angle": 0.0, "file": "t_0.png",
            "text": "привет", "cut_enabled": true,
            "style": { "font_family": "Anime Ace", "font_size": 22, "align": "center" }
        })];
        // A large PNG footprint would be very visible if it were (wrongly) applied.
        let out = migrate_overlay_entries(&items, &page_sizes, |_| (256.0, 128.0));
        let obj = out[0].as_object().unwrap();
        assert_eq!(
            legacy_uv_anchor(items[0].as_object().unwrap()),
            LegacyUvAnchor::Centre,
            "a nested `style` object is the Qt 2.X marker"
        );
        let u = obj.get("img_u").and_then(value_f32).unwrap();
        let v = obj.get("img_v").and_then(value_f32).unwrap();
        assert!((u - 0.3742).abs() < 1e-6, "Qt 2.X `u` is a centre and is copied verbatim, got {u}");
        assert!((v - 0.5118).abs() < 1e-6, "Qt 2.X `v` is a centre and is copied verbatim, got {v}");
        assert_eq!(obj.get("img_idx").and_then(Value::as_u64), Some(0));
        assert!(!obj.contains_key("u") && !obj.contains_key("v"), "legacy keys retired");
        // Its PNG is already native and `user_scale` is already the right factor: no normalization.
        assert!(!obj.contains_key("scale"), "centre families keep their own `user_scale`");
    }

    #[test]
    fn migrate_qt_with_align_flat_style_uv_is_a_centre() {
        // Generation 3a (`old_or_test/text_tab_old/text_view.py`): `text` + FLAT style keys including
        // the top-level `align`, no nested `style` object, no `page`/`region_*`. Its loader places the
        // item at `centre - local_centre * user_scale`, so `u`/`v` is a CENTRE — no shift. `align` is
        // the sound marker for this writer step because it landed together with nine siblings
        // (`line_spacing`, `extra_vpadding`, `stroke_width`, `grad_angle_deg`, …); keys such as
        // `stroke_color_rgba` are written only when non-null and come and go per file.
        let mut page_sizes: std::collections::HashMap<usize, [usize; 2]> =
            std::collections::HashMap::new();
        page_sizes.insert(0, [400, 800]);
        let items = vec![serde_json::json!({
            "img_idx": 0, "u": 0.25, "v": 0.6, "w_frac": 0.3, "user_scale": 1.25,
            "angle": 5.0, "file": "o_0.png", "text": "hi",
            "font": "Arial", "size": 20, "color": [0, 0, 0, 255], "align": "center",
            "line_spacing": 1.1, "extra_vpadding": 0, "stroke_width": 0
        })];
        let out = migrate_overlay_entries(&items, &page_sizes, |_| (120.0, 60.0));
        let obj = out[0].as_object().unwrap();
        assert_eq!(
            legacy_uv_anchor(items[0].as_object().unwrap()),
            LegacyUvAnchor::Centre,
            "a top-level `align` marks the post-`align` Qt writer, whose `u`/`v` is a centre"
        );
        assert!((obj.get("img_u").and_then(value_f32).unwrap() - 0.25).abs() < 1e-6);
        assert!((obj.get("img_v").and_then(value_f32).unwrap() - 0.6).abs() < 1e-6);
        assert!(!obj.contains_key("scale"), "centre families keep their own `user_scale`");
    }

    #[test]
    fn legacy_uv_anchor_discriminates_the_generations() {
        use LegacyUvAnchor::{Centre, QtUnscaledTopLeft, TkinterScaledTopLeft};

        // `region_w`/`region_h` is the exclusive Tkinter marker and stands alone — even when the
        // entry also carries `user_scale`, which `overlays.py` writes back on a key-scale.
        assert_eq!(
            legacy_uv_anchor(&obj(serde_json::json!({
                "page": "1_1", "u": 0.0, "v": 0.0, "region_w": 10, "region_h": 20, "user_scale": 1.4
            }))),
            TkinterScaledTopLeft
        );
        // Ordering case: a Tkinter entry that ALSO carries `text` must not fall into the Qt clause —
        // the `region_*` test runs first, so it stays Tkinter.
        assert_eq!(
            legacy_uv_anchor(&obj(serde_json::json!({
                "page": "1_1", "u": 0.0, "v": 0.0, "region_w": 10, "region_h": 20, "text": "x"
            }))),
            TkinterScaledTopLeft,
            "`region_w` outranks the `text` clause"
        );
        // Pre-`region_w` Tkinter entry: `page` and none of the Qt-only keys.
        assert_eq!(
            legacy_uv_anchor(&obj(serde_json::json!({
                "page": "1_2", "u": 0.1, "v": 0.2, "w_frac": 0.25, "file": "a.png"
            }))),
            TkinterScaledTopLeft
        );
        // `style`/`user_scale` veto the weaker `page` clause and, with no `text`, leave a centre.
        for veto in ["style", "user_scale"] {
            let mut o = obj(serde_json::json!({ "page": "1_2", "u": 0.1, "v": 0.2 }));
            o.insert(veto.to_string(), Value::from(1.0));
            assert_eq!(legacy_uv_anchor(&o), Centre, "`{veto}` must veto the `page` clause");
        }
        // `text` also vetoes the `page` clause, but a bare `text` entry is the pre-`align` Qt
        // generation, which is top-left too — just with the UNSCALED box. (No Qt writer emits `page`,
        // so this combination does not occur on disk; `text` is nonetheless a Qt-only key.)
        let mut with_text = obj(serde_json::json!({ "page": "1_2", "u": 0.1, "v": 0.2 }));
        with_text.insert("text".to_string(), Value::from("x"));
        assert_eq!(legacy_uv_anchor(&with_text), QtUnscaledTopLeft);

        // Generation 2: `text`, no `align`, no `style` (the real `ch20` key set).
        assert_eq!(
            legacy_uv_anchor(&obj(serde_json::json!({
                "angle": 0.0, "color": "#000000", "file": "t_0.png", "font": "Anime Ace",
                "img_idx": 0, "size": 22, "text": "x", "u": 0.2, "user_scale": 0.9,
                "v": 0.25, "w_frac": 0.4
            }))),
            QtUnscaledTopLeft
        );
        // Generation 3a: the same shape plus a top-level `align`.
        assert_eq!(
            legacy_uv_anchor(&obj(serde_json::json!({
                "img_idx": 0, "u": 0.4, "v": 0.5, "user_scale": 1.0, "text": "x", "align": "center"
            }))),
            Centre
        );
        // Generation 3b: a nested `style` object.
        assert_eq!(
            legacy_uv_anchor(&obj(serde_json::json!({
                "img_idx": 0, "u": 0.4, "v": 0.5, "user_scale": 1.0, "text": "x", "style": {}
            }))),
            Centre
        );
        // Nothing recognizable → the safe default (a false top-left is the costly direction).
        assert_eq!(legacy_uv_anchor(&Map::new()), Centre);
    }

    #[test]
    fn migrate_overlay_entries_is_idempotent() {
        // Feeding the migration its OWN output back must be a no-op: every migrated entry now carries
        // `img_u`/`img_v`, so `overlay_entry_is_modern` short-circuits it — in particular the injected
        // `scale` must NOT be recomputed over the already-normalized entry and compound. A realistic
        // mix: Tkinter top-left, Qt pre-`align` top-left, Qt 2.X centre, absolute ribbon, and an
        // already-modern entry.
        let page_sizes: std::collections::HashMap<usize, [usize; 2]> =
            [(0, [1000, 2000]), (1, [1000, 2400])].into_iter().collect();
        let items = vec![
            serde_json::json!({
                "file": "t_0.png", "page": "1_1", "img_idx": 0, "u": 0.1, "v": 0.2,
                "w_frac": 0.5, "angle": 0.0, "user_scale": 0.8, "region_w": 500, "region_h": 250
            }),
            serde_json::json!({
                "angle": 0.0, "color": "#000000", "file": "t_1.png", "font": "Anime Ace",
                "img_idx": 1, "size": 22, "text": "привет", "u": 0.2, "user_scale": 1.1,
                "v": 0.25, "w_frac": 0.4
            }),
            serde_json::json!({
                "img_idx": 0, "u": 0.3742, "v": 0.5118, "w_frac": 0.37, "user_scale": 1.0,
                "angle": 0.0, "file": "t_2.png", "text": "привет",
                "style": { "font_family": "Anime Ace", "font_size": 22, "align": "center" }
            }),
            serde_json::json!({
                "page": "1_2", "x": 10.0, "y": 2400.0, "region_w": 200.0, "region_h": 40.0,
                "file": "t_3.png"
            }),
            serde_json::json!({
                "img_idx": 0, "img_x_px": 50.0, "img_y_px": 60.0, "file": "t_4.png",
                "overlay_type": "text"
            }),
        ];

        let png = |_: &Map<String, Value>| (250.0_f32, 125.0_f32);
        let once = migrate_overlay_entries(&items, &page_sizes, png);
        let twice = migrate_overlay_entries(&once, &page_sizes, png);
        assert_eq!(once, twice, "migration is idempotent on its own output");

        // The injected `scale` in particular must be identical, not squared.
        let scale_of = |out: &[Value], i: usize| out[i].as_object().unwrap().get("scale").cloned();
        assert_eq!(scale_of(&once, 0), scale_of(&twice, 0), "Tkinter `scale` does not compound");
        assert_eq!(scale_of(&once, 1), scale_of(&twice, 1), "Qt pre-`align` `scale` does not compound");
        // And a third pass changes nothing either.
        let thrice = migrate_overlay_entries(&twice, &page_sizes, png);
        assert_eq!(twice, thrice, "still a fixed point after a third pass");
    }

    #[test]
    fn migrate_tkinter_entry_carrying_both_x_and_u_uses_the_uv_path() {
        // The one hybrid shape that exists on disk: `Сегодня я буду _/ch17` has an entry keyed
        // {angle, file, img_idx, page, region_h, region_w, u, v, w_frac, x, y} — Tkinter `region_*`
        // AND leftover absolute `x`/`y`. It must (a) classify as Tkinter, (b) take the `u`/`v` path,
        // and (c) be EXCLUDED from the cross-entry ribbon solve, which skips any entry carrying
        // `u`/`v` — otherwise its stale `x`/`y` would drag the chapter-wide ribbon scale.
        let page_sizes: std::collections::HashMap<usize, [usize; 2]> =
            [(0, [1000, 2000]), (1, [1000, 2000])].into_iter().collect();
        let hybrid = serde_json::json!({
            "angle": 0.0, "file": "t_7.png", "img_idx": 0, "page": "1_1",
            "region_h": 250, "region_w": 500, "u": 0.1, "v": 0.2, "w_frac": 0.5,
            "x": 999.0, "y": 999.0
        });
        assert_eq!(
            legacy_uv_anchor(hybrid.as_object().unwrap()),
            LegacyUvAnchor::TkinterScaledTopLeft,
            "`region_w`/`region_h` classifies it as Tkinter even with `x`/`y` present"
        );

        // (a)+(b): the centre comes from `u`/`v` + the Tkinter half-shift, NOT from `x`/`y`.
        // PNG 250×125 (aspect 0.5). Displayed 0.5*1000*1.0 = 500 px wide, 250 px tall → half
        // (250, 125) px → centre (0.1*1000+250, 0.2*2000+125) = (350, 525) px → uv (0.35, 0.2625).
        let out = migrate_overlay_entries(&[hybrid.clone()], &page_sizes, |_| (250.0, 125.0));
        let obj = out[0].as_object().unwrap();
        let u = obj.get("img_u").and_then(value_f32).unwrap();
        let v = obj.get("img_v").and_then(value_f32).unwrap();
        assert!((u - 0.35).abs() < 1e-4, "u comes from the `u`/`v` path, got {u}");
        assert!((v - 0.2625).abs() < 1e-4, "v comes from the `u`/`v` path, got {v}");
        assert!(!obj.contains_key("x") && !obj.contains_key("y"), "stale absolute keys retired");

        // (c): adding it to a chapter of real ribbon entries must not move their solved geometry.
        let ribbon = vec![
            serde_json::json!({"page":"1_1","x":10.0,"y":10.0,"region_w":20.0,"region_h":4.0,"file":"r0.png"}),
            serde_json::json!({"page":"1_2","x":10.0,"y":2100.0,"region_w":20.0,"region_h":4.0,"file":"r1.png"}),
        ];
        let without = migrate_overlay_entries(&ribbon, &page_sizes, |_| (0.0, 0.0));
        let mut with = ribbon.clone();
        with.push(hybrid);
        let with = migrate_overlay_entries(&with, &page_sizes, |_| (250.0, 125.0));
        assert_eq!(
            without[..],
            with[..without.len()],
            "the hybrid entry is excluded from the ribbon solve (it carries `u`/`v`)"
        );
    }

    #[test]
    fn migrate_top_left_uv_is_unshifted_when_the_page_size_is_unknown() {
        // An explicit `img_idx` is authoritative and NOT clamped, so an entry can resolve to a page
        // that is absent from `page_sizes` (the doc may hold a subset). The `[1, 1]` placeholder the
        // migration then uses is not the page: a half-shift derived from it would be wrong by an
        // unbounded factor, and the vertical one additionally by `page_w / page_h`. Both top-left
        // generations must therefore copy `u`/`v` verbatim and inject no `scale`.
        let page_sizes: std::collections::HashMap<usize, [usize; 2]> =
            [(0, [1000, 2000])].into_iter().collect();
        let tkinter = serde_json::json!({
            "file": "t_0.png", "img_idx": 7, "u": 0.1, "v": 0.2, "w_frac": 0.5,
            "angle": 0.0, "user_scale": 0.8, "region_w": 500, "region_h": 250
        });
        let qt = serde_json::json!({
            "angle": 0.0, "file": "t_1.png", "font": "Anime Ace", "img_idx": 7, "size": 22,
            "text": "x", "u": 0.2, "user_scale": 1.1, "v": 0.25, "w_frac": 0.4
        });
        let items = vec![tkinter, qt];
        let out = migrate_overlay_entries(&items, &page_sizes, |_| (250.0, 125.0));

        let first = out[0].as_object().unwrap();
        assert!((first.get("img_u").and_then(value_f32).unwrap() - 0.1).abs() < 1e-6, "Tkinter u kept");
        assert!((first.get("img_v").and_then(value_f32).unwrap() - 0.2).abs() < 1e-6, "Tkinter v kept");
        assert!(!first.contains_key("scale"), "no `scale` from a placeholder page width");
        assert_eq!(first.get("img_idx").and_then(Value::as_u64), Some(7), "the index is preserved");

        let second = out[1].as_object().unwrap();
        assert!((second.get("img_u").and_then(value_f32).unwrap() - 0.2).abs() < 1e-6, "Qt u kept");
        assert!((second.get("img_v").and_then(value_f32).unwrap() - 0.25).abs() < 1e-6, "Qt v kept");
        assert!(!second.contains_key("scale"), "no `scale` from a placeholder page width");

        // Control: the SAME entries, with page 7 present in the map, ARE shifted — so the guard is
        // what makes the difference above, not some unrelated early return.
        let known: std::collections::HashMap<usize, [usize; 2]> =
            [(7, [1000, 2000])].into_iter().collect();
        let shifted = migrate_overlay_entries(&items, &known, |_| (250.0, 125.0));
        let first = shifted[0].as_object().unwrap();
        // Tkinter: anchor 0.5*1000*0.8 = 400 px wide → half 200 px → u = (100 + 200)/1000 = 0.3.
        let u = first.get("img_u").and_then(value_f32).unwrap();
        assert!((u - 0.3).abs() < 1e-4, "a KNOWN page size does shift the Tkinter corner, got {u}");
        assert!(first.contains_key("scale"), "and does inject the normalized `scale`");
    }

    #[test]
    fn legacy_top_left_geometry_degrades_on_bad_w_frac_and_unknown_png() {
        // Degenerate `w_frac` must never panic and never divide by zero. Note that JSON cannot carry a
        // NaN literal, so the `is_finite()` guard is reached through an f64 that OVERFLOWS f32
        // (`value_f32` casts f64→f32, and `1e300 as f32` is +inf) rather than through a NaN.
        let page = [1000, 2000];
        let anchor = LegacyUvAnchor::TkinterScaledTopLeft;
        let bad_fracs = [
            serde_json::json!(0.0),
            serde_json::json!(-0.5),
            serde_json::json!(1e300),
            Value::Null,
            serde_json::json!("0.5"),
        ];
        for frac in bad_fracs {
            let mut o = obj(serde_json::json!({ "u": 0.1, "v": 0.2, "user_scale": 2.0 }));
            o.insert("w_frac".to_string(), frac.clone());

            // Known PNG: the half-extent degrades to the NATIVE footprint × `user_scale`.
            let g = legacy_top_left_geometry(anchor, &o, page, 250.0, 125.0);
            assert!(
                (g.shift_px[0] - 250.0).abs() < 1e-3 && (g.shift_px[1] - 125.0).abs() < 1e-3,
                "native footprint × user_scale 2.0 → half (250, 125) px for w_frac {frac}, got {:?}",
                g.shift_px
            );
            assert!(g.scale.is_none(), "no legacy displayed width is known for w_frac {frac}");

            // Unknown PNG on top of that: no shift at all, and still no panic.
            let g = legacy_top_left_geometry(anchor, &o, page, 0.0, 0.0);
            assert_eq!(g.shift_px, [0.0, 0.0], "fully unknown footprint → no shift for w_frac {frac}");
            assert!(g.scale.is_none());
        }

        // A usable `w_frac` with an unknown PNG keeps the horizontal half-shift and drops only the
        // vertical one (finding: the known half of the shift must not be thrown away).
        let o = obj(serde_json::json!({ "u": 0.1, "v": 0.2, "w_frac": 0.5, "user_scale": 0.8 }));
        let g = legacy_top_left_geometry(anchor, &o, page, 0.0, 0.0);
        assert!((g.shift_px[0] - 200.0).abs() < 1e-3, "0.5*1000*0.8/2 = 200 px, got {:?}", g.shift_px);
        assert_eq!(g.shift_px[1], 0.0, "the vertical half-extent needs the PNG aspect");
        assert!(g.scale.is_none(), "`scale` divides by png_w and is withheld");

        // The Qt pre-`align` generation ignores `user_scale` in the anchor box, same degradation.
        let g = legacy_top_left_geometry(LegacyUvAnchor::QtUnscaledTopLeft, &o, page, 0.0, 0.0);
        assert!((g.shift_px[0] - 250.0).abs() < 1e-3, "unscaled 0.5*1000/2 = 250 px, got {:?}", g.shift_px);
        assert_eq!(g.shift_px[1], 0.0);
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
