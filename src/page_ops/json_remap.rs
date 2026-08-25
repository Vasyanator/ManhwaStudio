/*
File: page_ops/json_remap.rs

Purpose:
Pure JSON rewrites for the page-keyed documents a structural page operation
must keep consistent: `translation_bubbles.json` (bubble `img_idx` +
ImageBubble `crop_page_idx`/`crop_rect`), `layers/layers.json` (page `img_idx`
plus the `ps_p{page:04}_...` file references inside layer records),
`text_info.json` (legacy typing overlay `img_idx`) and text-detection
`{idx:05}_blocks.json` (`mask_file` reference).

Key functions:
- remap_bubbles(): bubbles kept/remapped/deleted per the permutation.
- remap_text_info(): typing overlay entries kept/remapped/deleted.
- remap_layers_manifest(): layer-manifest pages remapped, deleted pages split
  off, stitched pages merged into one entry.
- remap_detection_blocks(): `mask_file` default-name rewrite for one page.
- merge_detection_blocks(): one detection document for a stitched page.
- remap_layers_png_name(): `ps_p{old:04}_` -> `ps_p{new:04}_` file-name rewrite.

Notes:
Everything operates on `serde_json::Value` so unknown/extra fields survive the
rewrite byte-for-byte at the value level (object key ORDER may change, matching
how the app itself re-serializes these documents). No filesystem access.

Geometry: the remaps take an optional `StitchGeometry`. Without it they are
purely index-keyed — a page keeps its own coordinate space. With it, a page
merged into a stitched canvas ALSO has its geometry mapped through that page's
`PlacementMap`; this file is the only place that affine touches JSON. Three
on-disk coordinate spaces must never be confused:
- page-normalized uv (bubble `img_u`/`img_v`, `rect_coords`, `text_areas`,
  `crop_rect`, legacy `u`/`v`, `points_uv`, `transform_uv`) -> `map_u`/`map_v`;
- absolute page pixels (`transform.cx/cy`, `deform.points_px`,
  `centering_frame.cx/cy`, detector `blocks`, `img_x_px`/`img_y_px`) ->
  `map_x`/`map_y`;
- layer-image-local pixels (`image_size`, `text_centers`, `render_data` text
  params, `raster_transform`) -> UNTOUCHED, because the layer PNGs are never
  resampled. Scalar page-pixel LENGTHS (`transform.scale`, a centering frame's
  half-extents, a legacy overlay's `scale`) multiply by the placement scale.
*/

use super::PageOpError;
use super::plan::{PlacementMap, StitchGeometry, layers_png_prefix};
use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// Shared numeric helpers.
//
// Stored geometry is written as JSON numbers, but a few legacy documents carry
// the same values as strings (`rect_coords` readers accept both), so reads
// tolerate strings and writes always normalize to numbers.
// ---------------------------------------------------------------------------

/// Reads a coordinate that may be stored as a number or a numeric string.
fn read_f64(value: &Value) -> Option<f64> {
    if let Some(number) = value.as_f64() {
        return Some(number);
    }
    value.as_str()?.trim().parse::<f64>().ok()
}

/// Reads `key` of `obj` as a coordinate.
fn get_f64(obj: &Map<String, Value>, key: &str) -> Option<f64> {
    read_f64(obj.get(key)?)
}

/// Writes a finite `value` back under `key`; a non-finite result is dropped
/// rather than written, so a corrupt input cannot produce a `null` field.
fn put_f64(obj: &mut Map<String, Value>, key: &str, value: f64) {
    if let Some(number) = serde_json::Number::from_f64(value) {
        obj.insert(key.to_string(), Value::Number(number));
    }
}

/// Builds a JSON number from a finite `f64`, or `Value::Null` when it is not
/// representable (callers only pass mapped, finite coordinates).
fn number(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

/// Reads a `[w, h]` size pair.
fn read_u32_pair(value: Option<&Value>) -> Option<[u32; 2]> {
    let array = value?.as_array()?;
    let w = u32::try_from(array.first()?.as_u64()?).ok()?;
    let h = u32::try_from(array.get(1)?.as_u64()?).ok()?;
    Some([w, h])
}

// ---------------------------------------------------------------------------
// Stitch geometry application.
// ---------------------------------------------------------------------------

/// Maps a `{u_key, v_key}` page-normalized pair in place. Returns true when a
/// value was rewritten.
fn map_uv_keys(
    obj: &mut Map<String, Value>,
    u_key: &str,
    v_key: &str,
    placement: &PlacementMap,
) -> bool {
    let mut changed = false;
    if let Some(u) = get_f64(obj, u_key) {
        put_f64(obj, u_key, placement.map_u(u));
        changed = true;
    }
    if let Some(v) = get_f64(obj, v_key) {
        put_f64(obj, v_key, placement.map_v(v));
        changed = true;
    }
    changed
}

/// Maps a `[u, v]` page-normalized point array in place.
fn map_uv_point(value: &mut Value, placement: &PlacementMap) -> bool {
    let Some(array) = value.as_array_mut() else {
        return false;
    };
    let (Some(u), Some(v)) = (
        array.first().and_then(read_f64),
        array.get(1).and_then(read_f64),
    ) else {
        return false;
    };
    array[0] = number(placement.map_u(u));
    array[1] = number(placement.map_v(v));
    true
}

/// Maps an `[x1, y1, x2, y2]` page-normalized rect array in place.
fn map_uv_rect(value: &mut Value, placement: &PlacementMap) -> bool {
    let Some(array) = value.as_array_mut() else {
        return false;
    };
    if array.len() < 4 {
        return false;
    }
    let coords: Option<Vec<f64>> = array.iter().take(4).map(read_f64).collect();
    let Some(coords) = coords else {
        return false;
    };
    array[0] = number(placement.map_u(coords[0]));
    array[1] = number(placement.map_v(coords[1]));
    array[2] = number(placement.map_u(coords[2]));
    array[3] = number(placement.map_v(coords[3]));
    true
}

/// Maps an array of `[x, y]` ABSOLUTE page-pixel points in place.
fn map_px_points(value: &mut Value, placement: &PlacementMap) -> bool {
    let Some(points) = value.as_array_mut() else {
        return false;
    };
    let mut changed = false;
    for point in points.iter_mut() {
        let Some(pair) = point.as_array_mut() else {
            continue;
        };
        let (Some(x), Some(y)) = (
            pair.first().and_then(read_f64),
            pair.get(1).and_then(read_f64),
        ) else {
            continue;
        };
        pair[0] = number(placement.map_x(x));
        pair[1] = number(placement.map_y(y));
        changed = true;
    }
    changed
}

/// Maps an array of `[u, v]` page-normalized points in place.
fn map_uv_points(value: &mut Value, placement: &PlacementMap) -> bool {
    let Some(points) = value.as_array_mut() else {
        return false;
    };
    let mut changed = false;
    for point in points.iter_mut() {
        changed |= map_uv_point(point, placement);
    }
    changed
}

/// Scales a stored page-pixel LENGTH (or a page-size multiplier such as a
/// layer transform's `scale`) with the placement.
fn scale_key(obj: &mut Map<String, Value>, key: &str, placement: &PlacementMap) {
    if let Some(value) = get_f64(obj, key) {
        put_f64(obj, key, placement.map_len(value));
    }
}

/// Result of remapping the bubbles array.
#[derive(Debug)]
pub(crate) struct BubblesRemap {
    /// Surviving bubbles with `img_idx` (and crop fields) remapped.
    pub kept: Vec<Value>,
    /// Bubbles of deleted pages, verbatim (archived in the trash).
    pub deleted: Vec<Value>,
    /// True when `kept` differs from the input or anything was deleted.
    pub changed: bool,
    pub warnings: Vec<String>,
}

/// Remaps every bubble's page association per `old_to_new`.
///
/// Rules:
/// - `img_idx` is rewritten to the page's new index; bubbles whose page was
///   deleted move to `deleted` (the caller archives them as
///   `deleted_bubbles.json` in the trash).
/// - a `crop_page_idx` (page-crop ImageBubble) is rewritten the same way; when
///   the crop TARGET page was deleted, `crop_page_idx` and `crop_rect` are
///   removed so the bubble degrades to a plain external-image bubble instead
///   of cropping a wrong page.
/// - an `img_idx`/`crop_page_idx` beyond the current page count is left
///   untouched with a warning (already-dangling data is not made worse).
/// - with a `stitch` geometry, a bubble sitting on a merged page also has its
///   normalized placement (`img_u`/`img_v`, `rect_coords`, `text_areas`) mapped
///   into the new canvas and its `side` recomputed by bubble class; a
///   `crop_rect` is mapped through the CROP page's placement, which is a
///   different page from the bubble's own whenever the two indices differ.
///
/// # Errors
/// [`PageOpError::InvalidOp`] when an entry is not an object, still uses the
/// legacy absolute-coordinate format (no `img_u`, numeric `x`/`y` — those are
/// keyed by ribbon position, not page, and must be migrated by a normal
/// project load first), or has no `img_idx` at all.
pub(crate) fn remap_bubbles(
    entries: &[Value],
    old_to_new: &[Option<usize>],
    stitch: Option<&StitchGeometry>,
) -> Result<BubblesRemap, PageOpError> {
    let mut kept = Vec::with_capacity(entries.len());
    let mut deleted = Vec::new();
    let mut changed = false;
    let mut warnings = Vec::new();

    for (pos, entry) in entries.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            return Err(PageOpError::InvalidOp(format!(
                "bubble entry #{pos} is not a JSON object"
            )));
        };
        if obj.get("img_u").is_none()
            && obj.get("x").and_then(Value::as_f64).is_some()
            && obj.get("y").and_then(Value::as_f64).is_some()
        {
            return Err(PageOpError::InvalidOp(format!(
                "bubble entry #{pos} uses the legacy absolute-coordinate format; \
                 open the chapter once so the load migration rewrites it before \
                 running page operations"
            )));
        }
        let Some(old_idx) = obj.get("img_idx").and_then(Value::as_u64) else {
            return Err(PageOpError::InvalidOp(format!(
                "bubble entry #{pos} has no numeric img_idx"
            )));
        };
        let Ok(old_idx) = usize::try_from(old_idx) else {
            return Err(PageOpError::InvalidOp(format!(
                "bubble entry #{pos} img_idx {old_idx} does not fit usize"
            )));
        };
        if old_idx >= old_to_new.len() {
            warnings.push(format!(
                "bubble entry #{pos} references page {old_idx} beyond the current \
                 {} page(s); left untouched",
                old_to_new.len()
            ));
            kept.push(entry.clone());
            continue;
        }
        match old_to_new[old_idx] {
            None => {
                deleted.push(entry.clone());
                changed = true;
            }
            Some(new_idx) => {
                let mut new_obj = obj.clone();
                if new_idx != old_idx {
                    new_obj.insert("img_idx".to_string(), Value::from(new_idx));
                    changed = true;
                }
                if let Some(placement) = stitch.and_then(|geo| geo.placement(old_idx))
                    && apply_bubble_geometry(&mut new_obj, placement)
                {
                    changed = true;
                }
                let (crop_changed, crop_warning) =
                    remap_crop_fields(&mut new_obj, old_to_new, old_idx, pos, stitch);
                changed |= crop_changed;
                if let Some(warning) = crop_warning {
                    warnings.push(warning);
                }
                kept.push(Value::Object(new_obj));
            }
        }
    }

    Ok(BubblesRemap {
        kept,
        deleted,
        changed,
        warnings,
    })
}

/// Maps the page-normalized placement of one bubble that sits on a stitched
/// page into the merged canvas. Returns true when anything was rewritten.
///
/// `crop_rect` is deliberately NOT handled here: it is normalized against the
/// CROP page, which may be a different page with a different placement, and is
/// mapped by [`remap_crop_fields`] instead.
fn apply_bubble_geometry(obj: &mut Map<String, Value>, placement: &PlacementMap) -> bool {
    let mut changed = map_uv_keys(obj, "img_u", "img_v", placement);
    if let Some(rect_coords) = obj.get_mut("rect_coords").and_then(Value::as_object_mut) {
        for corner in ["p1", "p2"] {
            if let Some(point) = rect_coords.get_mut(corner).and_then(Value::as_object_mut) {
                changed |= map_uv_keys(point, "img_u", "img_v", placement);
            }
        }
    }
    if let Some(areas) = obj.get_mut("text_areas").and_then(Value::as_array_mut) {
        for area in areas.iter_mut() {
            let Some(area) = area.as_object_mut() else {
                continue;
            };
            if let Some(rect) = area.get_mut("rect") {
                changed |= map_uv_rect(rect, placement);
            }
            if let Some(anchor) = area.get_mut("anchor") {
                changed |= map_uv_point(anchor, placement);
            }
        }
    }
    // `side` is persisted and trusted at load. Image bubbles derive it from
    // every area anchor; all other/unknown classes use the bubble anchor.
    // Keep an explicit null (unplaced bubble) intact.
    if obj.get("side").and_then(Value::as_str).is_some() {
        let side = if obj.get("bubble_class").and_then(Value::as_str) == Some("image") {
            let weight: f64 = obj
                .get("text_areas")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|area| area.get("anchor")?.as_array()?.first().and_then(read_f64))
                .map(|anchor_u| anchor_u - 0.5)
                .sum();
            Some(if weight < 0.0 { "left" } else { "right" })
        } else {
            get_f64(obj, "img_u").map(|u| if u < 0.5 { "left" } else { "right" })
        };
        if let Some(side) = side {
            obj.insert("side".to_string(), Value::String(side.to_string()));
            changed = true;
        }
    }
    changed
}

/// Rewrites `crop_page_idx` in one bubble object; removes `crop_page_idx` +
/// `crop_rect` when the crop target page was deleted; maps `crop_rect` through
/// the CROP page's placement when that page is stitched. Returns
/// `(changed, warning)`.
fn remap_crop_fields(
    obj: &mut Map<String, Value>,
    old_to_new: &[Option<usize>],
    bubble_idx: usize,
    entry_pos: usize,
    stitch: Option<&StitchGeometry>,
) -> (bool, Option<String>) {
    let Some(crop_idx) = obj.get("crop_page_idx").and_then(Value::as_u64) else {
        return (false, None);
    };
    let Ok(crop_idx) = usize::try_from(crop_idx) else {
        return (
            false,
            Some(format!(
                "bubble entry #{entry_pos} crop_page_idx {crop_idx} does not fit usize; \
                 left untouched"
            )),
        );
    };
    if crop_idx >= old_to_new.len() {
        return (
            false,
            Some(format!(
                "bubble entry #{entry_pos} crop_page_idx {crop_idx} is beyond the current \
                 {} page(s); left untouched",
                old_to_new.len()
            )),
        );
    }
    match old_to_new[crop_idx] {
        Some(new_idx) => {
            let warning = if crop_idx != bubble_idx
                && stitch.is_some_and(|geo| {
                    geo.placement(bubble_idx).is_some() && geo.placement(crop_idx).is_some()
                })
            {
                Some(format!(
                    "stitched bubble entry #{entry_pos} uses crop page {crop_idx}, different from \
                     its own page {bubble_idx}; crop_rect follows the crop page while bubble \
                     geometry follows its own page"
                ))
            } else {
                None
            };
            let mut changed = false;
            if new_idx != crop_idx {
                obj.insert("crop_page_idx".to_string(), Value::from(new_idx));
                changed = true;
            }
            // The crop rect is normalized against the CROPPED page, so it
            // follows that page's placement — not the bubble's own.
            if let Some(placement) = stitch.and_then(|geo| geo.placement(crop_idx))
                && let Some(rect) = obj.get_mut("crop_rect")
            {
                changed |= map_uv_rect(rect, placement);
            }
            (changed, warning)
        }
        None => {
            // The page this bubble cropped from is gone: drop the crop link so
            // the bubble cannot show a crop of an unrelated page.
            obj.remove("crop_page_idx");
            obj.remove("crop_rect");
            (true, None)
        }
    }
}

/// Result of remapping one `text_info.json` array.
#[derive(Debug)]
pub(crate) struct TextInfoRemap {
    pub kept: Vec<Value>,
    /// Entries of deleted pages, verbatim (archived in the trash).
    pub deleted: Vec<Value>,
    /// `file` names referenced by the deleted entries (overlay PNGs the caller
    /// moves to the trash).
    pub deleted_files: Vec<String>,
    pub changed: bool,
    pub warnings: Vec<String>,
}

/// Remaps legacy typing-overlay entries (`text_info.json`) per `old_to_new`.
///
/// Only `img_idx` is rewritten: the overlay PNG names (`file`) are NOT
/// page-keyed for loading (their `p{page:04}` token is a creation-time
/// uniqueness hint and the stable overlay uid is derived from the file name),
/// so renaming them would break `layers.json` references.
///
/// An entry with no `img_idx` and no legacy `x`/`y` is treated as page 0,
/// mirroring the typing loader (`helpers.rs` defaults a missing `img_idx`
/// to 0), and gets an explicit remapped `img_idx`.
///
/// With a `stitch` geometry, an entry on a merged page additionally has its
/// placement mapped into the new canvas (`img_x_px`/`img_y_px`, `img_u`/`u`,
/// `img_v`/`v`, `deform_mesh`, `transform_uv`, `scale`) and its `layer_idx`
/// re-based, keeping the typing tab's text groups of different source pages
/// distinct.
///
/// # Errors
/// [`PageOpError::InvalidOp`] for the legacy absolute-coordinate placement
/// family (numeric `x`/`y`, no modern or bare normalized coordinates): those
/// entries are keyed by continuous-ribbon position — which any page operation
/// changes — and must be migrated by opening the typing tab first.
pub(crate) fn remap_text_info(
    entries: &[Value],
    old_to_new: &[Option<usize>],
    stitch: Option<&StitchGeometry>,
) -> Result<TextInfoRemap, PageOpError> {
    let mut kept = Vec::with_capacity(entries.len());
    let mut deleted = Vec::new();
    let mut deleted_files = Vec::new();
    let mut changed = false;
    let mut warnings = Vec::new();

    for (pos, entry) in entries.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            return Err(PageOpError::InvalidOp(format!(
                "text_info entry #{pos} is not a JSON object"
            )));
        };
        let has_img_idx = obj.get("img_idx").and_then(Value::as_u64).is_some();
        if obj.get("img_x_px").is_none()
            && obj.get("img_y_px").is_none()
            && obj.get("img_u").is_none()
            && obj.get("img_v").is_none()
            && obj.get("u").is_none()
            && obj.get("v").is_none()
            && obj.get("x").and_then(Value::as_f64).is_some()
            && obj.get("y").and_then(Value::as_f64).is_some()
        {
            return Err(PageOpError::InvalidOp(format!(
                "text_info entry #{pos} uses the legacy absolute-coordinate placement; \
                 open the chapter (typing tab) once so the load migration rewrites it \
                 before running page operations"
            )));
        }
        // Mirror of the typing loader: a missing img_idx reads as page 0.
        let old_idx = obj
            .get("img_idx")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(0);
        if !has_img_idx {
            warnings.push(format!(
                "text_info entry #{pos} has no img_idx; treated as page 0 \
                 (matching the typing loader)"
            ));
        }
        if old_idx >= old_to_new.len() {
            warnings.push(format!(
                "text_info entry #{pos} references page {old_idx} beyond the current \
                 {} page(s); left untouched",
                old_to_new.len()
            ));
            kept.push(entry.clone());
            continue;
        }
        match old_to_new[old_idx] {
            None => {
                if let Some(file) = obj.get("file").and_then(Value::as_str) {
                    let trimmed = file.trim();
                    if !trimmed.is_empty() {
                        deleted_files.push(trimmed.to_string());
                    }
                }
                deleted.push(entry.clone());
                changed = true;
            }
            Some(new_idx) => {
                let placement = stitch.and_then(|geo| geo.placement(old_idx));
                if new_idx != old_idx || !has_img_idx || placement.is_some() {
                    let mut new_obj = obj.clone();
                    new_obj.insert("img_idx".to_string(), Value::from(new_idx));
                    if let (Some(geo), Some(placement)) = (stitch, placement) {
                        apply_text_info_geometry(&mut new_obj, placement);
                        offset_layer_idx(&mut new_obj, geo.layer_idx_offset(old_idx));
                    }
                    kept.push(Value::Object(new_obj));
                    changed = true;
                } else {
                    kept.push(entry.clone());
                }
            }
        }
    }

    Ok(TextInfoRemap {
        kept,
        deleted,
        deleted_files,
        changed,
        warnings,
    })
}

/// Maps one legacy `text_info.json` overlay entry onto a stitched canvas.
///
/// Mirrors the position vocabulary of `text_payload::decode_overlay_placement`:
/// absolute `img_x_px`/`img_y_px` win over normalized `img_u`/`u` +
/// `img_v`/`v`, a `deform_mesh` may store either `points_px` or the legacy
/// `points_uv`, and `transform_uv` is a quad of normalized corners. Rotation is
/// left alone (a uniform scale preserves angles) while `scale`/`user_scale` —
/// a page-pixel size factor of a NON-resampled overlay PNG — follows the
/// placement scale, exactly like `TransformRec::scale` in `layers.json`.
fn apply_text_info_geometry(obj: &mut Map<String, Value>, placement: &PlacementMap) {
    if let Some(x) = get_f64(obj, "img_x_px") {
        put_f64(obj, "img_x_px", placement.map_x(x));
    }
    if let Some(y) = get_f64(obj, "img_y_px") {
        put_f64(obj, "img_y_px", placement.map_y(y));
    }
    map_uv_keys(obj, "img_u", "img_v", placement);
    map_uv_keys(obj, "u", "v", placement);
    if let Some(mesh) = obj.get_mut("deform_mesh").and_then(Value::as_object_mut) {
        if let Some(points) = mesh.get_mut("points_px") {
            map_px_points(points, placement);
        }
        if let Some(points) = mesh.get_mut("points_uv") {
            map_uv_points(points, placement);
        }
    }
    if let Some(quad) = obj.get_mut("transform_uv") {
        map_uv_points(quad, placement);
    }
    scale_key(obj, "scale", placement);
    scale_key(obj, "user_scale", placement);
}

/// Adds `offset` to an entry's `layer_idx` (the typing tab's «Группа текста N»
/// axis), saturating at `u32::MAX`. A missing or non-numeric field is left
/// alone.
fn offset_layer_idx(obj: &mut Map<String, Value>, offset: u32) {
    if offset == 0 {
        return;
    }
    let Some(current) = obj
        .get("layer_idx")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
    else {
        return;
    };
    obj.insert(
        "layer_idx".to_string(),
        Value::from(current.saturating_add(offset)),
    );
}

/// Result of remapping `layers.json`.
#[derive(Debug)]
pub(crate) struct LayersRemap {
    /// The full manifest with surviving pages remapped and re-sorted.
    pub manifest: Value,
    /// Page entries of deleted pages, verbatim (archived in the trash).
    pub deleted_pages: Vec<Value>,
    pub changed: bool,
    pub warnings: Vec<String>,
}

/// Remaps the layer manifest (`layers/layers.json`) per `old_to_new`.
///
/// For every surviving page entry: `img_idx` is rewritten and every
/// `base_file` / `rendered_file` in its `tree` gets its `ps_p{page:04}_`
/// prefix rewritten to the file's new page index (the prefix is load-bearing:
/// `persist.rs::prune_orphan_pngs` prunes by it, so a stale prefix would let a
/// later save of the page now holding the OLD index delete the moved page's
/// PNGs). Page entries kept sorted by `img_idx` (a `LayersManifest` invariant,
/// see `manifest.rs::upsert_page`). Entries of deleted pages are split off.
///
/// With a `stitch` geometry the page entries of the merged pages are folded
/// into ONE entry at the new index (see [`merge_stitched_pages`]) with their
/// layer geometry mapped into the stitched canvas.
///
/// # Errors
/// - [`PageOpError::Json`] when the manifest root is not an object.
/// - [`PageOpError::InvalidOp`] when two merged pages share a layer/group uid.
pub(crate) fn remap_layers_manifest(
    manifest: &Value,
    old_to_new: &[Option<usize>],
    stitch: Option<&StitchGeometry>,
) -> Result<LayersRemap, PageOpError> {
    let Some(root) = manifest.as_object() else {
        return Err(PageOpError::Json(
            "layers.json root is not a JSON object".to_string(),
        ));
    };
    let mut warnings = Vec::new();
    let mut deleted_pages = Vec::new();
    let mut changed = false;

    let Some(pages) = root.get("pages").and_then(Value::as_array) else {
        // No pages array: nothing page-keyed to rewrite.
        return Ok(LayersRemap {
            manifest: manifest.clone(),
            deleted_pages,
            changed: false,
            warnings,
        });
    };

    let mut kept: Vec<Value> = Vec::with_capacity(pages.len());
    // Page entries of the stitched pages, in merge order (source page index ->
    // its remapped entry); folded into a single entry after the loop.
    let mut merged: std::collections::BTreeMap<usize, Map<String, Value>> =
        std::collections::BTreeMap::new();
    for (pos, page) in pages.iter().enumerate() {
        let Some(page_obj) = page.as_object() else {
            return Err(PageOpError::Json(format!(
                "layers.json pages[{pos}] is not a JSON object"
            )));
        };
        let Some(old_idx) = page_obj
            .get("img_idx")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
        else {
            return Err(PageOpError::Json(format!(
                "layers.json pages[{pos}] has no numeric img_idx"
            )));
        };
        if old_idx >= old_to_new.len() {
            warnings.push(format!(
                "layers.json pages[{pos}] references page {old_idx} beyond the current \
                 {} page(s); left untouched",
                old_to_new.len()
            ));
            kept.push(page.clone());
            continue;
        }
        match old_to_new[old_idx] {
            None => {
                deleted_pages.push(page.clone());
                changed = true;
            }
            Some(new_idx) => {
                let mut new_page = page_obj.clone();
                new_page.insert("img_idx".to_string(), Value::from(new_idx));
                // File references are remapped even on a page that keeps its
                // index: each name's EMBEDDED index is remapped independently,
                // so a cross-page PNG reference stays aligned with the
                // file-rename pass.
                if let Some(tree) = new_page.get_mut("tree").and_then(Value::as_array_mut) {
                    for rec in tree.iter_mut() {
                        if let Some(rec_obj) = rec.as_object_mut() {
                            remap_layer_rec_files(rec_obj, old_to_new);
                        }
                    }
                }
                if let Some(placement) = stitch.and_then(|geo| geo.placement(old_idx)) {
                    let offset = stitch.map_or(0, |geo| geo.layer_idx_offset(old_idx));
                    apply_page_layers_geometry(&mut new_page, placement, offset);
                    if merged.insert(old_idx, new_page).is_some() {
                        return Err(PageOpError::Json(format!(
                            "layers.json has two page entries for stitched page {old_idx}"
                        )));
                    }
                    changed = true;
                    continue;
                }
                if new_page == *page_obj {
                    kept.push(page.clone());
                } else {
                    kept.push(Value::Object(new_page));
                    changed = true;
                }
            }
        }
    }

    if !merged.is_empty() {
        kept.push(Value::Object(merge_stitched_pages(merged)?));
    }

    // Preserve the manifest's sorted-by-img_idx invariant after remapping.
    kept.sort_by_key(|page| {
        page.as_object()
            .and_then(|o| o.get("img_idx"))
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX)
    });

    let mut new_root = root.clone();
    new_root.insert("pages".to_string(), Value::Array(kept));
    Ok(LayersRemap {
        manifest: Value::Object(new_root),
        deleted_pages,
        changed,
        warnings,
    })
}

/// Maps one manifest page entry onto a stitched canvas and re-bases its
/// text-group axis.
///
/// Per layer record: `transform.cx/cy` and `deform.points_px` are absolute page
/// pixels and move with the placement; `transform.scale` and a centering
/// frame's `half_w`/`half_h` are page-pixel magnitudes and follow the scale;
/// `transform.rotation` is unchanged (a uniform scale preserves angles); and
/// `image_size`, `text_centers`, `render_data` are layer-image-local, so they
/// are left exactly as they are — the layer PNG is not resampled.
fn apply_page_layers_geometry(
    page: &mut Map<String, Value>,
    placement: &PlacementMap,
    layer_idx_offset: u32,
) {
    if let Some(tree) = page.get_mut("tree").and_then(Value::as_array_mut) {
        for rec in tree.iter_mut() {
            let Some(rec) = rec.as_object_mut() else {
                continue;
            };
            if let Some(transform) = rec.get_mut("transform").and_then(Value::as_object_mut) {
                if let Some(cx) = get_f64(transform, "cx") {
                    put_f64(transform, "cx", placement.map_x(cx));
                }
                if let Some(cy) = get_f64(transform, "cy") {
                    put_f64(transform, "cy", placement.map_y(cy));
                }
                scale_key(transform, "scale", placement);
            }
            if let Some(points) = rec
                .get_mut("deform")
                .and_then(Value::as_object_mut)
                .and_then(|deform| deform.get_mut("points_px"))
            {
                map_px_points(points, placement);
            }
            if let Some(frame) = rec
                .get_mut("centering_frame")
                .and_then(Value::as_object_mut)
            {
                if let Some(cx) = get_f64(frame, "cx") {
                    put_f64(frame, "cx", placement.map_x(cx));
                }
                if let Some(cy) = get_f64(frame, "cy") {
                    put_f64(frame, "cy", placement.map_y(cy));
                }
                scale_key(frame, "half_w", placement);
                scale_key(frame, "half_h", placement);
            }
            offset_layer_idx(rec, layer_idx_offset);
        }
    }
    if let Some(groups) = page.get_mut("text_groups").and_then(Value::as_array_mut) {
        for group in groups.iter_mut() {
            if let Some(group) = group.as_object_mut() {
                offset_layer_idx(group, layer_idx_offset);
            }
        }
    }
}

/// Folds the already-remapped page entries of a stitch into ONE entry.
///
/// The entries arrive keyed by their source page index, so iteration order is
/// ascending — the first (lowest) page's layers end up at the BOTTOM of the
/// merged stack, which is the reading order of the stitched canvas. `tree`,
/// `groups` and `text_groups` are concatenated in that order; every other key
/// (`img_idx` included, already set to the merged index) is inherited from the
/// first entry, so unknown//future fields survive.
///
/// `z` is a per-page band axis shared by raster nodes, pinned text nodes and
/// text-group records (`manifest.rs`), so the merged entry re-ranks it densely
/// over `(page order, z)`: relative order inside each page is preserved, equal
/// bands stay equal, and no two pages can claim the same band by accident.
///
/// # Errors
/// [`PageOpError::InvalidOp`] when two merged pages share a layer or group uid
/// — the merged tree would then hold two nodes with one identity and their PNGs
/// would collide on one file name.
fn merge_stitched_pages(
    pages: std::collections::BTreeMap<usize, Map<String, Value>>,
) -> Result<Map<String, Value>, PageOpError> {
    // Dense re-rank of the shared Z axis across the merged pages.
    let mut bands: std::collections::BTreeSet<(usize, u64)> = std::collections::BTreeSet::new();
    for (order, (_, page)) in pages.iter().enumerate() {
        for key in ["tree", "text_groups"] {
            for rec in page.get(key).and_then(Value::as_array).into_iter().flatten() {
                if let Some(z) = rec.get("z").and_then(Value::as_u64) {
                    bands.insert((order, z));
                }
            }
        }
    }
    let ranks: std::collections::HashMap<(usize, u64), u64> = bands
        .iter()
        .enumerate()
        .map(|(rank, key)| {
            // `rank` is bounded by the number of distinct bands in the merged
            // document, far below u64::MAX.
            let rank = u64::try_from(rank).unwrap_or(u64::MAX);
            (*key, rank)
        })
        .collect();

    let mut merged: Option<Map<String, Value>> = None;
    let mut tree: Vec<Value> = Vec::new();
    let mut groups: Vec<Value> = Vec::new();
    let mut text_groups: Vec<Value> = Vec::new();
    let mut seen_uids: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for (order, (old_idx, page)) in pages.into_iter().enumerate() {
        let mut page = page;
        for (key, sink) in [
            ("tree", &mut tree),
            ("groups", &mut groups),
            ("text_groups", &mut text_groups),
        ] {
            let Some(items) = page.remove(key) else {
                continue;
            };
            let Value::Array(items) = items else {
                continue;
            };
            for item in items {
                let mut item = item;
                if let Some(obj) = item.as_object_mut() {
                    if let Some(z) = obj.get("z").and_then(Value::as_u64)
                        && let Some(rank) = ranks.get(&(order, z))
                    {
                        obj.insert("z".to_string(), Value::from(*rank));
                    }
                    // Groups and layers share one uid namespace in the merged
                    // tree; a collision would also collide their PNG names.
                    if key != "text_groups"
                        && let Some(uid) = obj.get("uid").and_then(Value::as_str)
                        && let Some(previous) = seen_uids.insert(uid.to_string(), old_idx)
                        && previous != old_idx
                    {
                        return Err(PageOpError::InvalidOp(format!(
                            "stitch cannot merge pages {previous} and {old_idx}: both \
                             carry a layer with uid '{uid}'"
                        )));
                    }
                }
                sink.push(item);
            }
        }
        if merged.is_none() {
            merged = Some(page);
        }
    }

    let mut merged = merged.ok_or_else(|| {
        PageOpError::Json("no layer-manifest page entries to merge".to_string())
    })?;
    // Match the manifest writer, which omits these keys when empty.
    for (key, items) in [
        ("groups", groups),
        ("text_groups", text_groups),
        ("tree", tree),
    ] {
        if items.is_empty() && key != "tree" {
            merged.remove(key);
        } else {
            merged.insert(key.to_string(), Value::Array(items));
        }
    }
    Ok(merged)
}

/// Rewrites the `ps_p{page:04}_` prefix of `base_file` / `rendered_file` in
/// one layer record. The embedded index of EACH FILE NAME is remapped
/// independently (rather than assuming the page's own index) so a record
/// referencing a PNG with a different page prefix stays consistent with the
/// file-rename pass, which is also keyed by the name's embedded index.
fn remap_layer_rec_files(rec: &mut Map<String, Value>, old_to_new: &[Option<usize>]) {
    for key in ["base_file", "rendered_file"] {
        let Some(name) = rec.get(key).and_then(Value::as_str) else {
            continue;
        };
        let Some(file_idx) = super::plan::parse_layers_png_page_idx(name) else {
            continue;
        };
        if file_idx >= old_to_new.len() {
            continue;
        }
        if let Some(new_idx) = old_to_new[file_idx]
            && let Some(new_name) = remap_layers_png_name(name, file_idx, new_idx)
        {
            rec.insert(key.to_string(), Value::String(new_name));
        }
    }
}

/// Rewrites a layer PNG name from the `old_idx` prefix to the `new_idx`
/// prefix; returns `None` when the name does not carry the `old_idx` prefix
/// or already has the target name.
#[must_use]
pub(crate) fn remap_layers_png_name(name: &str, old_idx: usize, new_idx: usize) -> Option<String> {
    if old_idx == new_idx {
        return None;
    }
    let rest = name.strip_prefix(&layers_png_prefix(old_idx))?;
    Some(format!("{}{rest}", layers_png_prefix(new_idx)))
}

/// Rewrites the `mask_file` reference inside a text-detection blocks document
/// when it points at the page's DEFAULT mask name (`{old:05}_mask.png`), which
/// the transaction renames. A custom `mask_file` value is left untouched (the
/// engine does not rename such files). Returns the (possibly new) document and
/// whether it changed.
#[must_use]
pub(crate) fn remap_detection_blocks(
    blocks: &Value,
    old_idx: usize,
    new_idx: usize,
) -> (Value, bool) {
    let Some(obj) = blocks.as_object() else {
        return (blocks.clone(), false);
    };
    let old_default = super::plan::detection_mask_file_name(old_idx);
    let matches_default = obj
        .get("mask_file")
        .and_then(Value::as_str)
        .is_some_and(|name| name.trim() == old_default);
    if !matches_default || old_idx == new_idx {
        return (blocks.clone(), false);
    }
    let mut new_obj = obj.clone();
    new_obj.insert(
        "mask_file".to_string(),
        Value::String(super::plan::detection_mask_file_name(new_idx)),
    );
    (Value::Object(new_obj), true)
}

/// Why a stitched page's detection document cannot be merged, or `None` when
/// it can be.
///
/// The detector's blocks are ABSOLUTE pixels of the source page, so merging is
/// only sound when the document really describes this page at its current size
/// (`source_size` == the page image) and, when a mask exists, when that mask is
/// not a downscaled copy (`mask_size` == `source_size`). Anything else would
/// require inventing a scale factor; the caller trashes the group instead.
#[must_use]
pub(crate) fn detection_merge_blocker(
    blocks: &Value,
    has_mask: bool,
    page_size: [u32; 2],
    page_idx: usize,
) -> Option<String> {
    let Some(obj) = blocks.as_object() else {
        return Some(format!("page {page_idx}: blocks file is not a JSON object"));
    };
    let Some(source_size) = read_u32_pair(obj.get("source_size")) else {
        return Some(format!("page {page_idx}: blocks file has no source_size"));
    };
    if source_size != page_size {
        return Some(format!(
            "page {page_idx}: blocks source_size {}x{} differs from the page image {}x{}",
            source_size[0], source_size[1], page_size[0], page_size[1]
        ));
    }
    if has_mask {
        let mask_size = read_u32_pair(obj.get("mask_size")).unwrap_or(source_size);
        if mask_size != source_size {
            return Some(format!(
                "page {page_idx}: detection mask is {}x{} for a {}x{} page (downscaled)",
                mask_size[0], mask_size[1], source_size[0], source_size[1]
            ));
        }
    }
    None
}

/// Builds the ONE text-detection document of a stitched page from the
/// documents of its source pages.
///
/// Every source's blocks are mapped into the canvas through that page's
/// placement (absolute page pixels, see `models/text_mask_model.rs`), and the
/// merged document declares the canvas as both `source_size` and `mask_size`.
/// `mask_file` names the composed mask, or is empty when no source page had
/// one — matching what `save_text_detection_page` writes. Non-geometry keys are
/// inherited from the first source document so unknown fields survive.
///
/// `pages` must be ordered by source page index and every entry must have a
/// placement in `geometry`.
///
/// # Errors
/// [`PageOpError::Json`] when a listed page has no placement or the input list
/// is empty.
pub(crate) fn merge_detection_blocks(
    pages: &[(usize, &Value)],
    geometry: &StitchGeometry,
    mask_file: Option<&str>,
) -> Result<Value, PageOpError> {
    let Some((_, first)) = pages.first() else {
        return Err(PageOpError::Json(
            "no text-detection documents to merge".to_string(),
        ));
    };
    let mut root = first.as_object().cloned().unwrap_or_default();
    let mut blocks_out: Vec<Value> = Vec::new();
    for (page_idx, document) in pages {
        let placement = geometry.placement(*page_idx).ok_or_else(|| {
            PageOpError::Json(format!(
                "text-detection page {page_idx} has no stitch placement"
            ))
        })?;
        for block in document
            .get("blocks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(obj) = block.as_object() else {
                continue;
            };
            let mut mapped = obj.clone();
            for (key, is_x) in [("x1", true), ("x2", true), ("y1", false), ("y2", false)] {
                if let Some(value) = get_f64(&mapped, key) {
                    let mapped_value = if is_x {
                        placement.map_x(value)
                    } else {
                        placement.map_y(value)
                    };
                    put_f64(&mut mapped, key, mapped_value);
                }
            }
            blocks_out.push(Value::Object(mapped));
        }
    }
    let canvas = Value::Array(vec![
        Value::from(geometry.canvas[0]),
        Value::from(geometry.canvas[1]),
    ]);
    root.insert("page_idx".to_string(), Value::from(geometry.primary_new));
    root.insert("source_size".to_string(), canvas.clone());
    root.insert("mask_size".to_string(), canvas);
    root.insert("blocks".to_string(), Value::Array(blocks_out));
    root.insert(
        "mask_file".to_string(),
        Value::String(mask_file.unwrap_or_default().to_string()),
    );
    Ok(Value::Object(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Move page 0 to the end of 4 pages: 0->3, 1->0, 2->1, 3->2.
    fn move_map() -> Vec<Option<usize>> {
        vec![Some(3), Some(0), Some(1), Some(2)]
    }

    /// Delete page 1 of 3: 0->0, 1->gone, 2->1.
    fn delete_map() -> Vec<Option<usize>> {
        vec![Some(0), None, Some(1)]
    }

    #[test]
    fn bubbles_img_idx_and_crop_are_remapped() {
        let entries = vec![
            json!({"id": 1, "img_idx": 0, "img_u": 0.5, "img_v": 0.5, "text": "a",
                   "custom_field": "kept"}),
            json!({"id": 2, "img_idx": 3, "img_u": 0.1, "img_v": 0.2,
                   "crop_page_idx": 1, "crop_rect": [0, 0, 10, 10],
                   "image_source_type": "page_crop"}),
        ];
        let out = remap_bubbles(&entries, &move_map(), None).expect("remaps");
        assert!(out.changed);
        assert!(out.deleted.is_empty());
        assert_eq!(out.kept[0]["img_idx"], json!(3));
        // Unknown fields survive.
        assert_eq!(out.kept[0]["custom_field"], json!("kept"));
        assert_eq!(out.kept[1]["img_idx"], json!(2));
        // crop target page 1 moved to index 0.
        assert_eq!(out.kept[1]["crop_page_idx"], json!(0));
        assert_eq!(out.kept[1]["crop_rect"], json!([0, 0, 10, 10]));
    }

    #[test]
    fn bubbles_of_deleted_pages_are_split_off_and_crop_links_dropped() {
        let entries = vec![
            json!({"id": 1, "img_idx": 1, "img_u": 0.5, "img_v": 0.5}),
            json!({"id": 2, "img_idx": 2, "img_u": 0.5, "img_v": 0.5,
                   "crop_page_idx": 1, "crop_rect": [1, 2, 3, 4]}),
        ];
        let out = remap_bubbles(&entries, &delete_map(), None).expect("remaps");
        assert!(out.changed);
        // Bubble on the deleted page is archived verbatim.
        assert_eq!(out.deleted.len(), 1);
        assert_eq!(out.deleted[0]["id"], json!(1));
        // Survivor: img_idx 2 -> 1, crop link to the deleted page removed.
        assert_eq!(out.kept.len(), 1);
        assert_eq!(out.kept[0]["img_idx"], json!(1));
        assert!(out.kept[0].get("crop_page_idx").is_none());
        assert!(out.kept[0].get("crop_rect").is_none());
    }

    #[test]
    fn bubbles_reject_legacy_absolute_coordinates() {
        let entries = vec![json!({"id": 1, "x": 10.0, "y": 20.0, "text": "legacy"})];
        assert!(matches!(
            remap_bubbles(&entries, &move_map(), None),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn bubbles_out_of_range_img_idx_is_left_with_warning() {
        let entries = vec![json!({"id": 1, "img_idx": 99, "img_u": 0.5, "img_v": 0.5})];
        let out = remap_bubbles(&entries, &move_map(), None).expect("remaps");
        assert!(!out.changed);
        assert_eq!(out.kept[0]["img_idx"], json!(99));
        assert_eq!(out.warnings.len(), 1);
    }

    #[test]
    fn text_info_entries_are_remapped_and_deleted_files_collected() {
        let entries = vec![
            json!({"img_idx": 1, "file": "typing_overlay_p0001_1.png", "u": 0.5}),
            json!({"img_idx": 0, "file": "typing_overlay_p0000_2.png"}),
        ];
        let out = remap_text_info(&entries, &delete_map(), None).expect("remaps");
        assert!(out.changed);
        assert_eq!(out.deleted.len(), 1);
        assert_eq!(out.deleted_files, vec!["typing_overlay_p0001_1.png"]);
        assert_eq!(out.kept.len(), 1);
        // Page 0 keeps index 0 and its file name is NOT rewritten.
        assert_eq!(out.kept[0]["img_idx"], json!(0));
        assert_eq!(out.kept[0]["file"], json!("typing_overlay_p0000_2.png"));
    }

    #[test]
    fn text_info_rejects_legacy_ribbon_placement() {
        let entries = vec![json!({"x": 100.0, "y": 2000.0, "file": "t.png"})];
        assert!(matches!(
            remap_text_info(&entries, &move_map(), None),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn text_info_rejects_legacy_ribbon_placement_with_img_idx() {
        let entries = vec![json!({
            "img_idx": 1, "x": 100.0, "y": 2000.0, "file": "t.png"
        })];
        assert!(matches!(
            remap_text_info(&entries, &move_map(), None),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn layers_manifest_remaps_img_idx_and_png_references() {
        let manifest = json!({
            "schema_version": 3,
            "pages": [
                {"img_idx": 0, "tree": [
                    {"uid": "u1", "base_file": "ps_p0000_u1.png",
                     "rendered_file": "ps_p0000_u1_fx.png", "z": 0}
                ]},
                {"img_idx": 2, "tree": [
                    {"uid": "u2", "rendered_file": "ps_p0002_u2_text.png", "z": 0}
                ]}
            ]
        });
        let out = remap_layers_manifest(&manifest, &move_map(), None).expect("remaps");
        assert!(out.changed);
        assert!(out.deleted_pages.is_empty());
        let pages = out.manifest["pages"].as_array().expect("pages array");
        // Sorted by the NEW img_idx: page 2 -> 1 first, page 0 -> 3 second.
        assert_eq!(pages[0]["img_idx"], json!(1));
        assert_eq!(
            pages[0]["tree"][0]["rendered_file"],
            json!("ps_p0001_u2_text.png")
        );
        assert_eq!(pages[1]["img_idx"], json!(3));
        assert_eq!(pages[1]["tree"][0]["base_file"], json!("ps_p0003_u1.png"));
        assert_eq!(
            pages[1]["tree"][0]["rendered_file"],
            json!("ps_p0003_u1_fx.png")
        );
        // schema_version survives untouched.
        assert_eq!(out.manifest["schema_version"], json!(3));
    }

    #[test]
    fn layers_manifest_splits_off_deleted_pages() {
        let manifest = json!({
            "schema_version": 3,
            "pages": [
                {"img_idx": 1, "tree": [{"uid": "gone", "base_file": "ps_p0001_g.png", "z": 0}]},
                {"img_idx": 2, "tree": []}
            ]
        });
        let out = remap_layers_manifest(&manifest, &delete_map(), None).expect("remaps");
        assert!(out.changed);
        assert_eq!(out.deleted_pages.len(), 1);
        assert_eq!(out.deleted_pages[0]["img_idx"], json!(1));
        let pages = out.manifest["pages"].as_array().expect("pages array");
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0]["img_idx"], json!(1));
    }

    #[test]
    fn layers_manifest_rejects_page_without_numeric_img_idx() {
        for invalid in [json!({"tree": []}), json!({"img_idx": "1", "tree": []}), json!(7)] {
            let manifest = json!({"schema_version": 3, "pages": [invalid]});
            assert!(matches!(
                remap_layers_manifest(&manifest, &move_map(), None),
                Err(PageOpError::Json(_))
            ));
        }
    }

    #[test]
    fn detection_blocks_mask_file_default_is_rewritten() {
        let blocks = json!({
            "source_size": [10, 20],
            "blocks": [],
            "mask_file": "00001_mask.png"
        });
        let (out, changed) = remap_detection_blocks(&blocks, 1, 0);
        assert!(changed);
        assert_eq!(out["mask_file"], json!("00000_mask.png"));

        // A custom mask_file name is left untouched.
        let custom = json!({"mask_file": "custom.png"});
        let (out, changed) = remap_detection_blocks(&custom, 1, 0);
        assert!(!changed);
        assert_eq!(out["mask_file"], json!("custom.png"));
    }

    // -----------------------------------------------------------------------
    // Stitch: two 100x200 pages side by side on a 200x200 canvas.
    // Page 0 keeps its coordinates (dx = 0); page 1 shifts to the right half,
    // so `u' = 0.5 + u / 2` and `v' = v`.
    // -----------------------------------------------------------------------

    fn placement(page_idx: usize, dx: i64, scale: f32, canvas: [u32; 2]) -> PlacementMap {
        PlacementMap::new(
            &crate::page_ops::StitchPlacement {
                page_idx,
                crop: [0, 0, 100, 200],
                scale,
                dx,
                dy: 0,
            },
            [100, 200],
            canvas,
        )
        .expect("valid placement")
    }

    /// Pages 0 and 1 merged into new index 0; page 1's text groups start at 1.
    fn side_by_side() -> StitchGeometry {
        StitchGeometry::for_tests(
            vec![
                (0, placement(0, 0, 1.0, [200, 200])),
                (1, placement(1, 100, 1.0, [200, 200])),
            ],
            vec![(0, 0), (1, 1)],
            0,
            [200, 200],
        )
    }

    /// Merging pages 0 and 1 of 3: both -> 0, page 2 -> 1.
    fn stitch_map() -> Vec<Option<usize>> {
        vec![Some(0), Some(0), Some(1)]
    }

    #[test]
    fn stitch_renormalizes_bubble_geometry_and_side() {
        let entries = vec![json!({
            "id": 1, "img_idx": 1, "img_u": 0.5, "img_v": 0.25, "side": "left",
            "rect_coords": {"p1": {"img_u": 0.0, "img_v": 0.0},
                            "p2": {"img_u": 1.0, "img_v": 1.0}},
            "text_areas": [{"rect": [0.0, 0.0, 1.0, 1.0], "anchor": [0.5, 0.5]}],
            "custom": "kept"
        })];
        let geometry = side_by_side();
        let out = remap_bubbles(&entries, &stitch_map(), Some(&geometry)).expect("remaps");
        assert!(out.changed);
        assert!(out.deleted.is_empty(), "a stitch never drops a bubble");
        let bubble = &out.kept[0];
        assert_eq!(bubble["img_idx"], json!(0));
        assert_eq!(bubble["img_u"], json!(0.75));
        assert_eq!(bubble["img_v"], json!(0.25));
        // The anchor moved into the right half of the canvas.
        assert_eq!(bubble["side"], json!("right"));
        assert_eq!(bubble["rect_coords"]["p1"]["img_u"], json!(0.5));
        assert_eq!(bubble["rect_coords"]["p1"]["img_v"], json!(0.0));
        assert_eq!(bubble["rect_coords"]["p2"]["img_u"], json!(1.0));
        assert_eq!(bubble["text_areas"][0]["rect"], json!([0.5, 0.0, 1.0, 1.0]));
        assert_eq!(bubble["text_areas"][0]["anchor"], json!([0.75, 0.5]));
        assert_eq!(bubble["custom"], json!("kept"));
    }

    #[test]
    fn stitch_derives_image_bubble_side_from_all_area_anchors() {
        let entries = vec![json!({
            "id": 2, "img_idx": 1, "img_u": 0.7, "img_v": 0.25, "side": "left",
            "bubble_class": "image",
            "text_areas": [
                {"anchor": [0.7, 0.5]},
                {"anchor": [0.1, 0.5]}
            ]
        })];
        // Center the 100px page in a 200px canvas: the bubble anchor maps to
        // the right half, while the signed sum of mapped area anchors is left.
        let geometry = StitchGeometry::for_tests(
            vec![(1, placement(1, 50, 1.0, [200, 200]))],
            vec![(1, 0)],
            0,
            [200, 200],
        );
        let out = remap_bubbles(&entries, &stitch_map(), Some(&geometry)).expect("remaps");
        let bubble = &out.kept[0];
        assert!(bubble["img_u"].as_f64().is_some_and(|u| u > 0.5));
        assert_eq!(bubble["side"], json!("left"));
    }

    #[test]
    fn stitch_preserves_explicit_null_bubble_side() {
        let entries = vec![json!({
            "id": 3, "img_idx": 1, "img_u": 0.5, "img_v": 0.25, "side": null
        })];
        let geometry = side_by_side();
        let out = remap_bubbles(&entries, &stitch_map(), Some(&geometry)).expect("remaps");
        assert!(out.kept[0]["side"].is_null());
    }

    #[test]
    fn stitch_maps_crop_rect_through_the_cropped_page_not_the_bubble_page() {
        // The bubble lives on page 0 (identity placement) but crops page 1,
        // which moves to the right half: using the bubble's own placement here
        // would leave the crop rect untouched and silently show the wrong area.
        let entries = vec![json!({
            "id": 7, "img_idx": 0, "img_u": 0.1, "img_v": 0.1,
            "bubble_class": "image", "image_source_type": "page_crop",
            "crop_page_idx": 1, "crop_rect": [0.0, 0.0, 1.0, 1.0]
        })];
        let geometry = side_by_side();
        let out = remap_bubbles(&entries, &stitch_map(), Some(&geometry)).expect("remaps");
        let bubble = &out.kept[0];
        assert_eq!(bubble["crop_page_idx"], json!(0));
        assert_eq!(bubble["crop_rect"], json!([0.5, 0.0, 1.0, 1.0]));
        // The bubble's own anchor followed page 0's identity placement.
        assert_eq!(bubble["img_u"], json!(0.05));
        assert_eq!(out.warnings.len(), 1);
        assert!(out.warnings[0].contains("crop page 1"));
    }

    #[test]
    fn stitch_merges_layer_pages_with_dense_z_and_rebased_text_groups() {
        let manifest = json!({
            "schema_version": 4,
            "pages": [
                {"img_idx": 0,
                 "text_groups": [{"layer_idx": 0, "z": 2, "name": "G0"}],
                 "groups": [{"uid": "g0", "name": "Folder", "visible": true, "opacity": 1.0}],
                 "tree": [
                    {"uid": "a", "z": 0, "base_file": "ps_p0000_a.png"},
                    {"uid": "b", "z": 5, "layer_idx": 0, "kind": "text"}
                 ]},
                {"img_idx": 1,
                 "text_groups": [{"layer_idx": 0, "z": 1, "name": "G1"}],
                 "tree": [
                    {"uid": "c", "z": 0, "layer_idx": 0, "kind": "text"},
                    {"uid": "d", "z": 4, "base_file": "ps_p0001_d.png",
                     "transform": {"cx": 10.0, "cy": 20.0, "rotation": 0.5, "scale": 1.0},
                     "image_size": [7, 9]}
                 ]}
            ]
        });
        let geometry = side_by_side();
        let out = remap_layers_manifest(&manifest, &stitch_map(), Some(&geometry))
            .expect("merges");
        assert!(out.changed);
        assert!(out.deleted_pages.is_empty());
        let pages = out.manifest["pages"].as_array().expect("pages");
        assert_eq!(pages.len(), 1, "the merged pages become one entry");
        let page = &pages[0];
        assert_eq!(page["img_idx"], json!(0));

        // Bottom-to-top: page 0's layers first, then page 1's.
        let tree = page["tree"].as_array().expect("tree");
        let uids: Vec<&str> = tree
            .iter()
            .filter_map(|rec| rec["uid"].as_str())
            .collect();
        assert_eq!(uids, vec!["a", "b", "c", "d"]);
        // Dense Z re-rank over (page, z): page 0 uses 0/2/5 -> 0/1/2,
        // page 1 uses 0/1/4 -> 3/4/5.
        assert_eq!(tree[0]["z"], json!(0));
        assert_eq!(tree[1]["z"], json!(2));
        assert_eq!(tree[2]["z"], json!(3));
        assert_eq!(tree[3]["z"], json!(5));
        let text_groups = page["text_groups"].as_array().expect("text_groups");
        assert_eq!(text_groups[0]["z"], json!(1));
        assert_eq!(text_groups[1]["z"], json!(4));
        // Text-group axes stay distinct: page 1 is re-based past page 0.
        assert_eq!(text_groups[0]["layer_idx"], json!(0));
        assert_eq!(text_groups[1]["layer_idx"], json!(1));
        assert_eq!(tree[1]["layer_idx"], json!(0));
        assert_eq!(tree[2]["layer_idx"], json!(1));
        // Groups are concatenated and the file references follow the prefix.
        assert_eq!(page["groups"].as_array().expect("groups").len(), 1);
        assert_eq!(tree[3]["base_file"], json!("ps_p0000_d.png"));
        // Page-px placement moved; the layer-local size did not.
        assert_eq!(tree[3]["transform"]["cx"], json!(110.0));
        assert_eq!(tree[3]["transform"]["cy"], json!(20.0));
        assert_eq!(tree[3]["transform"]["rotation"], json!(0.5));
        assert_eq!(tree[3]["image_size"], json!([7, 9]));
    }

    #[test]
    fn stitch_scale_follows_page_px_magnitudes_only() {
        // Page 1 doubled and placed at the origin of a 200x400 canvas.
        let geometry = StitchGeometry::for_tests(
            vec![(1, placement(1, 0, 2.0, [200, 400]))],
            vec![(1, 0)],
            0,
            [200, 400],
        );
        let manifest = json!({
            "pages": [{"img_idx": 1, "tree": [
                {"uid": "d", "z": 0,
                 "transform": {"cx": 10.0, "cy": 20.0, "rotation": 0.25, "scale": 1.5},
                 "deform": {"cols": 2, "rows": 2,
                            "points_px": [[0.0, 0.0], [10.0, 0.0], [0.0, 10.0], [10.0, 10.0]]},
                 "centering_frame": {"cx": 10.0, "cy": 20.0, "half_w": 5.0, "half_h": 7.0},
                 "image_size": [7, 9],
                 "text_centers": {"mean": [3.0, 4.0]}}
            ]}]
        });
        let out = remap_layers_manifest(&manifest, &[Some(0), Some(0)], Some(&geometry))
            .expect("merges");
        let rec = &out.manifest["pages"][0]["tree"][0];
        // Page pixels scale AND shift; the scale factor multiplies.
        assert_eq!(rec["transform"]["cx"], json!(20.0));
        assert_eq!(rec["transform"]["cy"], json!(40.0));
        assert_eq!(rec["transform"]["scale"], json!(3.0));
        assert_eq!(rec["transform"]["rotation"], json!(0.25));
        assert_eq!(rec["deform"]["points_px"][3], json!([20.0, 20.0]));
        assert_eq!(rec["centering_frame"]["half_w"], json!(10.0));
        assert_eq!(rec["centering_frame"]["half_h"], json!(14.0));
        // Layer-image-local values must NOT move with the page.
        assert_eq!(rec["image_size"], json!([7, 9]));
        assert_eq!(rec["text_centers"]["mean"], json!([3.0, 4.0]));
    }

    #[test]
    fn stitch_refuses_pages_sharing_a_layer_uid() {
        let manifest = json!({
            "pages": [
                {"img_idx": 0, "tree": [{"uid": "same", "z": 0}]},
                {"img_idx": 1, "tree": [{"uid": "same", "z": 0}]}
            ]
        });
        let geometry = side_by_side();
        let err = remap_layers_manifest(&manifest, &stitch_map(), Some(&geometry))
            .expect_err("uid collision must be refused");
        assert!(matches!(err, PageOpError::InvalidOp(_)), "got: {err}");
    }

    #[test]
    fn stitch_maps_legacy_text_info_placement_and_rebases_layer_idx() {
        let entries = vec![
            json!({"img_idx": 1, "file": "ov.png", "img_x_px": 10.0, "img_y_px": 20.0,
                   "layer_idx": 3, "rotation_deg": 15.0, "scale": 2.0,
                   "deform_mesh": {"cols": 2, "rows": 2,
                                   "points_px": [[0.0, 0.0], [10.0, 0.0],
                                                 [0.0, 10.0], [10.0, 10.0]]}}),
            json!({"img_idx": 1, "file": "ov2.png", "u": 0.5, "v": 0.5,
                   "transform_uv": [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]}),
            json!({"img_idx": 2, "file": "other.png", "img_u": 0.5, "img_v": 0.5}),
        ];
        let geometry = side_by_side();
        let out = remap_text_info(&entries, &stitch_map(), Some(&geometry)).expect("remaps");
        assert!(out.changed);
        assert!(out.deleted.is_empty());
        let first = &out.kept[0];
        assert_eq!(first["img_idx"], json!(0));
        assert_eq!(first["img_x_px"], json!(110.0));
        assert_eq!(first["img_y_px"], json!(20.0));
        // Rotation is angle-preserving under a uniform scale; scale is 1 here.
        assert_eq!(first["rotation_deg"], json!(15.0));
        assert_eq!(first["scale"], json!(2.0));
        assert_eq!(first["layer_idx"], json!(4));
        assert_eq!(first["deform_mesh"]["points_px"][0], json!([100.0, 0.0]));
        let second = &out.kept[1];
        assert_eq!(second["u"], json!(0.75));
        assert_eq!(second["v"], json!(0.5));
        assert_eq!(second["transform_uv"][0], json!([0.5, 0.0]));
        assert_eq!(second["transform_uv"][2], json!([1.0, 1.0]));
        // A page that is not stitched only shifts its index.
        let third = &out.kept[2];
        assert_eq!(third["img_idx"], json!(1));
        assert_eq!(third["img_u"], json!(0.5));
    }

    #[test]
    fn detection_merge_is_refused_for_downscaled_or_stale_documents() {
        let good = json!({"source_size": [100, 200], "mask_size": [100, 200], "blocks": []});
        assert!(detection_merge_blocker(&good, true, [100, 200], 1).is_none());
        // A mask smaller than the page cannot be remapped without inventing a
        // scale factor.
        let downscaled = json!({"source_size": [100, 200], "mask_size": [50, 100], "blocks": []});
        assert!(detection_merge_blocker(&downscaled, true, [100, 200], 1).is_some());
        // Without a mask file the mask_size field is irrelevant.
        assert!(detection_merge_blocker(&downscaled, false, [100, 200], 1).is_none());
        // A document describing a differently-sized page is stale.
        assert!(detection_merge_blocker(&good, true, [90, 200], 1).is_some());
        assert!(detection_merge_blocker(&json!({"blocks": []}), false, [1, 1], 1).is_some());
        assert!(detection_merge_blocker(&json!([]), false, [1, 1], 1).is_some());
    }

    #[test]
    fn detection_blocks_merge_into_one_canvas_document() {
        let page0 = json!({
            "page_idx": 0, "source_size": [100, 200], "mask_size": [100, 200],
            "mask_file": "00000_mask.png",
            "blocks": [{"x1": 1.0, "y1": 2.0, "x2": 3.0, "y2": 4.0}]
        });
        let page1 = json!({
            "page_idx": 1, "source_size": [100, 200], "mask_size": [100, 200],
            "mask_file": "00001_mask.png",
            "blocks": [{"x1": 1.0, "y1": 2.0, "x2": 3.0, "y2": 4.0}]
        });
        let geometry = side_by_side();
        let merged = merge_detection_blocks(
            &[(0, &page0), (1, &page1)],
            &geometry,
            Some("00000_mask.png"),
        )
        .expect("merges");
        assert_eq!(merged["page_idx"], json!(0));
        assert_eq!(merged["source_size"], json!([200, 200]));
        assert_eq!(merged["mask_size"], json!([200, 200]));
        assert_eq!(merged["mask_file"], json!("00000_mask.png"));
        let blocks = merged["blocks"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 2);
        // Page 0 is placed at the origin, page 1 shifted by 100 px in x only.
        assert_eq!(blocks[0], json!({"x1": 1.0, "y1": 2.0, "x2": 3.0, "y2": 4.0}));
        assert_eq!(
            blocks[1],
            json!({"x1": 101.0, "y1": 2.0, "x2": 103.0, "y2": 4.0})
        );
        // With no composed mask the reference is empty, as the writer does.
        let no_mask = merge_detection_blocks(&[(0, &page0)], &geometry, None).expect("merges");
        assert_eq!(no_mask["mask_file"], json!(""));
    }

    #[test]
    fn layers_png_name_rewrite() {
        assert_eq!(
            remap_layers_png_name("ps_p0002_u2_text.png", 2, 5),
            Some("ps_p0005_u2_text.png".to_string())
        );
        assert_eq!(remap_layers_png_name("ps_p0002_u2.png", 2, 2), None);
        assert_eq!(remap_layers_png_name("other.png", 2, 5), None);
    }
}
