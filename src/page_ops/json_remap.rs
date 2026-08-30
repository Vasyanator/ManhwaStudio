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
  off, stitched pages merged into one entry, a split page partitioned into one
  entry per part.
- split_layer_routing(): which layer node / layer PNG of the split page belongs
  to which part (the exact-area rule); refuses one PNG claimed by records of
  different surviving parts, and reports the PNGs claimed only by deleted ones.
- remap_detection_blocks(): `mask_file` default-name rewrite for one page.
- detection_merge_blocker() / detection_rect_blocker(): the all-or-nothing
  trust gates a page's detection document must pass before it is remapped.
- merge_detection_blocks() / split_detection_blocks(): the detection document
  of a stitched page / of one split part.
- remap_layers_png_name(): `ps_p{old:04}_` -> `ps_p{new:04}_` file-name rewrite.

Notes:
Everything operates on `serde_json::Value` so unknown/extra fields survive the
rewrite byte-for-byte at the value level (object key ORDER may change, matching
how the app itself re-serializes these documents). No filesystem access.

Geometry: the remaps take a `PageGeometry`. `PageGeometry::None` makes them
purely index-keyed — a page keeps its own coordinate space. A `Stitch` maps a
merged page's geometry through that page's `PlacementMap`; a `Split` first
ROUTES each entry to one part (bubbles by their anchor, layers by the exact
area of their footprint, detector blocks by the area of their rectangle) and
then maps it through that part's `PlacementMap`. An entry routed to a part the
request DELETED is archived exactly as an entry of a deleted PAGE would be,
never re-homed on a surviving part. This file is the only place
those affines touch JSON. Three on-disk coordinate spaces must never be
confused:
- page-normalized uv (bubble `img_u`/`img_v`, `rect_coords`, `text_areas`,
  `crop_rect`, legacy `u`/`v`, `points_uv`, `transform_uv`) -> `map_uv`;
- absolute page pixels (`transform.cx/cy`, `deform.points_px`,
  `centering_frame.cx/cy`, detector `blocks`, `img_x_px`/`img_y_px`) ->
  `map_point`;
- layer-image-local pixels (`image_size`, `text_centers`, `render_data` text
  params, `raster_transform`) -> UNTOUCHED, because the layer PNGs are never
  resampled. Scalar page-pixel MAGNITUDES (`transform.scale`, a centering
  frame's half-extents, a legacy overlay's `scale`) are not coordinates and use
  `map_extent`, which applies the scale but never the rotation.
Both coordinate mappings take a POINT, never one axis at a time, because a
placement may rotate. The two exceptions are documented where they occur: an
axis-aligned RECT (`rect_coords`, `text_areas`, detector blocks) degrades to
the bounding box of the mapped quad under a rotation, and a HALF-SPECIFIED
point is mapped only while the placement does not rotate.
*/

use super::PageOpError;
use super::plan::{
    CropGeometry, CropTreeRouting, PageGeometry, PlacementMap, SplitGeometry, SplitTreeRouting,
    StitchGeometry, layers_png_prefix,
};
use serde_json::{Map, Value};

/// The per-variant view of a [`PageGeometry`], destructured EXHAUSTIVELY.
///
/// `PageGeometry` deliberately exposes no accessor (see its doc): an accessor is
/// exhaustive in one place only, so a new variant would silently read as "no
/// geometry" in every remap. Each remap therefore destructures the enum itself,
/// which is why this pattern is repeated instead of factored out — the
/// repetition IS the compile-time reminder that a new pixel-identity operation
/// must be considered at every one of these sites.
type GeometryParts<'a> = (
    Option<&'a StitchGeometry>,
    Option<&'a SplitGeometry>,
    Option<&'a CropGeometry>,
);

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
///
/// An entry carrying only ONE of the two keys is not a point: a non-rotating
/// placement maps it exactly all the same (its axes are independent), while a
/// rotating one leaves it untouched, because the missing coordinate cannot be
/// guessed. A document that may hold such half-specified entries must therefore
/// be gated out before a rotating placement is applied to it.
fn map_uv_keys(
    obj: &mut Map<String, Value>,
    u_key: &str,
    v_key: &str,
    placement: &PlacementMap,
) -> bool {
    match (get_f64(obj, u_key), get_f64(obj, v_key)) {
        (Some(u), Some(v)) => {
            let (mapped_u, mapped_v) = placement.map_uv(u, v);
            put_f64(obj, u_key, mapped_u);
            put_f64(obj, v_key, mapped_v);
            true
        }
        (Some(u), None) => map_half_pair(obj, u_key, placement.map_u_without_v(u)),
        (None, Some(v)) => map_half_pair(obj, v_key, placement.map_v_without_u(v)),
        (None, None) => false,
    }
}

/// Maps an `{x_key, y_key}` ABSOLUTE page-pixel pair in place. Returns true
/// when a value was rewritten. Half-specified pairs behave as in
/// [`map_uv_keys`].
fn map_px_keys(
    obj: &mut Map<String, Value>,
    x_key: &str,
    y_key: &str,
    placement: &PlacementMap,
) -> bool {
    match (get_f64(obj, x_key), get_f64(obj, y_key)) {
        (Some(x), Some(y)) => {
            let (mapped_x, mapped_y) = placement.map_point(x, y);
            put_f64(obj, x_key, mapped_x);
            put_f64(obj, y_key, mapped_y);
            true
        }
        (Some(x), None) => map_half_pair(obj, x_key, placement.map_x_without_y(x)),
        (None, Some(y)) => map_half_pair(obj, y_key, placement.map_y_without_x(y)),
        (None, None) => false,
    }
}

/// Writes back the single mapped coordinate of a half-specified pair, or leaves
/// the entry alone when the placement could not map one axis on its own.
fn map_half_pair(obj: &mut Map<String, Value>, key: &str, mapped: Option<f64>) -> bool {
    let Some(value) = mapped else {
        return false;
    };
    put_f64(obj, key, value);
    true
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
    let (mapped_u, mapped_v) = placement.map_uv(u, v);
    array[0] = number(mapped_u);
    array[1] = number(mapped_v);
    true
}

/// Maps an `[x1, y1, x2, y2]` page-normalized rect array in place. The rect is
/// axis-aligned, so a ROTATING placement can only return its bounding box (see
/// `PlacementMap::map_uv_rect`).
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
    let mapped = placement.map_uv_rect([coords[0], coords[1], coords[2], coords[3]]);
    for (slot, value) in array.iter_mut().zip(mapped) {
        *slot = number(value);
    }
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
        let (mapped_x, mapped_y) = placement.map_point(x, y);
        pair[0] = number(mapped_x);
        pair[1] = number(mapped_y);
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

/// Scales a stored page-pixel MAGNITUDE (a length, a half-extent, or a
/// page-size multiplier such as a layer transform's `scale`) with the
/// placement. A magnitude has no origin and no direction, so a rotation never
/// applies to it — see `PlacementMap::map_extent`.
fn scale_key(obj: &mut Map<String, Value>, key: &str, placement: &PlacementMap) {
    if let Some(value) = get_f64(obj, key) {
        put_f64(obj, key, placement.map_extent(value));
    }
}

/// Adds the placement's page rotation to a STORED angle of a page-placed
/// object, in the unit `delta` is given in, and re-canonicalizes the result.
///
/// A stored angle describes an orientation RELATIVE TO THE PAGE, so when the
/// page itself turns the angle must turn with it or the object is drawn
/// unrotated on a rotated page. `keys` lists the accepted spellings of the field
/// in priority order (`text_info` accepts `rotation_deg` and its legacy `angle`
/// alias); the FIRST one present is rewritten. When none is present the first
/// key is inserted with the delta, because the readers default a missing angle
/// to zero and zero is no longer the right orientation.
///
/// `full_turn` is one whole turn in that unit (`360.0` for degrees, `TAU` for
/// radians) and the sum is wrapped into the half-open `[-full/2, +full/2)`,
/// mirroring `tabs/typing/tab/geometry.rs::normalize_angle_deg`. Wrapping
/// matters because these operations COMPOSE: four quarter-turn crops of one page
/// would otherwise store `2*PI` where `0` means the same thing, and the number
/// is surfaced to the user as a degree readout. A wrapped angle is
/// indistinguishable to every reader — they all go through `sin`/`cos` — so
/// re-canonicalizing here is safe even though the typing tab's interactive
/// wheel/drag paths deliberately let `transform.rotation` grow unbounded
/// between edits.
///
/// A zero `delta` writes nothing at all — not even a wrap of an already-stored
/// value: a non-rotating placement must leave the document byte-identical, which
/// is what keeps a stitch and a split pixel-identical operations.
fn add_stored_angle(
    obj: &mut Map<String, Value>,
    keys: &[&str],
    delta: f64,
    full_turn: f64,
) -> bool {
    if delta == 0.0 {
        return false;
    }
    let wrap = |angle: f64| -> f64 {
        if !angle.is_finite() || full_turn <= 0.0 {
            return angle;
        }
        let half = full_turn / 2.0;
        (angle + half).rem_euclid(full_turn) - half
    };
    for key in keys {
        if let Some(current) = get_f64(obj, key) {
            put_f64(obj, key, wrap(current + delta));
            return true;
        }
    }
    let Some(first) = keys.first() else {
        return false;
    };
    put_f64(obj, first, wrap(delta));
    true
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
/// - with a STITCH geometry, a bubble sitting on a merged page also has its
///   normalized placement (`img_u`/`img_v`, `rect_coords`, `text_areas`) mapped
///   into the new canvas and its `side` recomputed by bubble class; a
///   `crop_rect` is mapped through the CROP page's placement, which is a
///   different page from the bubble's own whenever the two indices differ.
/// - with a SPLIT geometry, a bubble sitting on the cut page goes to the part
///   containing its ANCHOR point — a user-fixed rule that holds for image
///   bubbles too, whose visible area may mostly lie in another part — and its
///   placement is then mapped into that part. A `crop_rect` on the cut page
///   follows the part holding the majority of the CROPPED AREA instead, and is
///   clamped back into `[0, 1]` after mapping.
/// - with a CROP geometry, a bubble on the cropped page KEEPS its page (the
///   index never changes) and has its placement mapped through the crop, so it
///   may hang off the new page's edge. A bubble whose whole footprint fell
///   outside the kept region is archived exactly like a bubble of a deleted
///   page. Survival is judged by the bubble's `rect_coords` box — not by its
///   anchor alone as the split does, because the question here is "does
///   anything of it remain visible", and an anchor-only test would archive a
///   bubble whose box still overlaps the frame; a bubble without a readable
///   box falls back to its anchor point.
///
/// # Errors
/// [`PageOpError::InvalidOp`] when an entry is not an object, still uses the
/// legacy absolute-coordinate format (no `img_u`, numeric `x`/`y` — those are
/// keyed by ribbon position, not page, and must be migrated by a normal
/// project load first), or has no `img_idx` at all.
pub(crate) fn remap_bubbles(
    entries: &[Value],
    old_to_new: &[Option<usize>],
    geometry: PageGeometry<'_>,
) -> Result<BubblesRemap, PageOpError> {
    // Exhaustive by design (see `PageGeometry` and `GeometryParts`).
    let (stitch, split, crop): GeometryParts<'_> = match geometry {
        PageGeometry::None => (None, None, None),
        PageGeometry::Stitch(geo) => (Some(geo), None, None),
        PageGeometry::Split(geo) => (None, Some(geo), None),
        PageGeometry::Crop(geo) => (None, None, Some(geo)),
    };
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
                // A bubble of the CROPPED page whose whole footprint fell
                // outside the kept region is archived exactly like a bubble of
                // a deleted page — never silently dropped, never relocated.
                if let Some(geo) = crop
                    && geo.source_old_idx() == old_idx
                    && !bubble_survives_crop(obj, geo)
                {
                    warnings.push(format!(
                        "bubble entry #{pos} lies entirely outside the kept region of the \
                         cropped page {old_idx}; archived in the trash"
                    ));
                    deleted.push(entry.clone());
                    changed = true;
                    continue;
                }
                // A split routes the bubble by its anchor point, so its target
                // page is a PART, not the index map's representative.
                let split_part = split.filter(|geo| geo.source_old_idx() == old_idx).map(
                    |geo| {
                        let u = get_f64(obj, "img_u").unwrap_or(0.5);
                        let v = get_f64(obj, "img_v").unwrap_or(0.5);
                        geo.part_for_uv_point(u, v)
                    },
                );
                // A bubble anchored in a DELETED part is archived exactly like a
                // bubble of a deleted page, not moved to a neighbouring part:
                // the user asked for that content to go away.
                if let (Some(geo), Some(part)) = (split, split_part)
                    && geo.is_deleted_part(part)
                {
                    warnings.push(format!(
                        "bubble entry #{pos} is anchored in deleted split part {part}; \
                         archived in the trash instead of being moved to another part"
                    ));
                    deleted.push(entry.clone());
                    changed = true;
                    continue;
                }
                let new_idx = match (split, split_part) {
                    (Some(geo), Some(part)) => geo.part_new_idx(part).ok_or_else(|| {
                        PageOpError::InvalidOp(format!(
                            "split part {part} has no index in the new order"
                        ))
                    })?,
                    _ => new_idx,
                };
                if new_idx != old_idx {
                    new_obj.insert("img_idx".to_string(), Value::from(new_idx));
                    changed = true;
                }
                let placement = match (stitch, split, split_part, crop) {
                    (Some(geo), _, _, _) => geo.placement(old_idx),
                    (_, Some(geo), Some(part), _) => geo.placement(part),
                    (_, _, _, Some(geo)) if geo.source_old_idx() == old_idx => {
                        Some(geo.placement())
                    }
                    _ => None,
                };
                if let Some(placement) = placement
                    && apply_bubble_geometry(&mut new_obj, placement)
                {
                    changed = true;
                }
                let (crop_changed, crop_warnings) =
                    remap_crop_fields(&mut new_obj, old_to_new, old_idx, pos, (stitch, split, crop));
                changed |= crop_changed;
                warnings.extend(crop_warnings);
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
/// the CROP page's placement when that page is stitched, through the winning
/// PART's placement when that page is split, or through the crop transform when
/// that page is the one being rotated and cropped. Returns
/// `(changed, warnings)`.
///
/// The split case never drops the crop link: the cropped page still exists, cut
/// into parts, so `crop_page_idx` is remapped to the part holding the majority
/// of `crop_rect` and the rect is renormalized into that part. A rect that
/// straddled a cut maps partly outside `[0, 1]`; it is clamped back (which is
/// also what the reader does, `canvas/mod.rs::image_bubble_crop_rect`) and the
/// trim is reported, so a page-crop bubble degrades to a smaller crop instead
/// of silently becoming a plain image bubble.
///
/// A page CROP keeps the target page's index, so only the rect moves: it is
/// mapped through the crop transform and clamped back into `[0, 1]`, and only a
/// rect with NOTHING left inside the kept region loses its link — the same rule
/// as a crop of a deleted split part, for the same reason (the link would
/// otherwise show an unrelated region).
fn remap_crop_fields(
    obj: &mut Map<String, Value>,
    old_to_new: &[Option<usize>],
    bubble_idx: usize,
    entry_pos: usize,
    geometry: (
        Option<&StitchGeometry>,
        Option<&SplitGeometry>,
        Option<&CropGeometry>,
    ),
) -> (bool, Vec<String>) {
    let (stitch, split, crop) = geometry;
    let Some(crop_idx) = obj.get("crop_page_idx").and_then(Value::as_u64) else {
        return (false, Vec::new());
    };
    let Ok(crop_idx) = usize::try_from(crop_idx) else {
        return (
            false,
            vec![format!(
                "bubble entry #{entry_pos} crop_page_idx {crop_idx} does not fit usize; \
                 left untouched"
            )],
        );
    };
    if crop_idx >= old_to_new.len() {
        return (
            false,
            vec![format!(
                "bubble entry #{entry_pos} crop_page_idx {crop_idx} is beyond the current \
                 {} page(s); left untouched",
                old_to_new.len()
            )],
        );
    }
    if let Some(geo) = split
        && geo.source_old_idx() == crop_idx
    {
        return remap_split_crop_fields(obj, geo, crop_idx, entry_pos);
    }
    if let Some(geo) = crop
        && geo.source_old_idx() == crop_idx
    {
        return remap_page_crop_crop_fields(obj, geo, crop_idx, entry_pos);
    }
    match old_to_new[crop_idx] {
        Some(new_idx) => {
            let mut warnings = Vec::new();
            if crop_idx != bubble_idx
                && stitch.is_some_and(|geo| {
                    geo.placement(bubble_idx).is_some() && geo.placement(crop_idx).is_some()
                })
            {
                warnings.push(format!(
                    "stitched bubble entry #{entry_pos} uses crop page {crop_idx}, different from \
                     its own page {bubble_idx}; crop_rect follows the crop page while bubble \
                     geometry follows its own page"
                ));
            }
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
            (changed, warnings)
        }
        None => {
            // The page this bubble cropped from is gone: drop the crop link so
            // the bubble cannot show a crop of an unrelated page.
            obj.remove("crop_page_idx");
            obj.remove("crop_rect");
            (true, Vec::new())
        }
    }
}

/// The `crop_page_idx == <split page>` case of [`remap_crop_fields`].
fn remap_split_crop_fields(
    obj: &mut Map<String, Value>,
    geo: &SplitGeometry,
    crop_idx: usize,
    entry_pos: usize,
) -> (bool, Vec<String>) {
    let mut warnings = Vec::new();
    // Mirror of the reader's effective crop: an absent `crop_rect` means a
    // small default box around the bubble's own anchor.
    let stored = read_uv_rect(obj.get("crop_rect"));
    let effective = stored.unwrap_or_else(|| {
        let u = get_f64(obj, "img_u").unwrap_or(0.5);
        let v = get_f64(obj, "img_v").unwrap_or(0.5);
        [u - 0.05, v - 0.05, u + 0.05, v + 0.05]
    });
    let part = geo.part_for_uv_rect(effective);
    if geo.is_deleted_part(part) {
        // The cropped region was deleted with its part. Drop the crop link,
        // exactly as `remap_crop_fields` does for a deleted page: keeping the
        // stale index would show a crop of an unrelated page.
        obj.remove("crop_page_idx");
        obj.remove("crop_rect");
        warnings.push(format!(
            "bubble entry #{entry_pos} cropped deleted split part {part} of page \
             {crop_idx}; its crop link was removed"
        ));
        return (true, warnings);
    }
    let (Some(placement), Some(new_idx)) = (geo.placement(part), geo.part_new_idx(part)) else {
        warnings.push(format!(
            "bubble entry #{entry_pos} crop page {crop_idx} has no split part {part}; \
             left untouched"
        ));
        return (false, warnings);
    };
    let mut changed = false;
    if new_idx != crop_idx {
        obj.insert("crop_page_idx".to_string(), Value::from(new_idx));
        changed = true;
    }
    if let Some(rect) = stored {
        let mapped = placement.map_uv_rect(rect);
        let clamped = mapped.map(|value| value.clamp(0.0, 1.0));
        if clamped != mapped {
            warnings.push(format!(
                "bubble entry #{entry_pos} crops the split page across a cut; its crop was \
                 trimmed to the part it mostly covers"
            ));
        }
        obj.insert(
            "crop_rect".to_string(),
            Value::Array(clamped.iter().map(|value| number(*value)).collect()),
        );
        changed = true;
    } else {
        warnings.push(format!(
            "bubble entry #{entry_pos} crops the split page but stores no crop_rect; only \
             its crop_page_idx was routed to a part"
        ));
    }
    (changed, warnings)
}

/// The `crop_page_idx == <rotated and cropped page>` case of
/// [`remap_crop_fields`].
///
/// The page index does not change, so only the rect moves: it is mapped through
/// the crop transform (a bounding box when the crop rotates by a fine angle,
/// exact otherwise) and clamped back into `[0, 1]`, which is also what the
/// reader does (`canvas/mod.rs::image_bubble_crop_rect`). A rect with NOTHING
/// left inside the kept region loses its link, exactly as a crop of a deleted
/// split part does — showing the same index would show an unrelated region.
fn remap_page_crop_crop_fields(
    obj: &mut Map<String, Value>,
    geo: &CropGeometry,
    crop_idx: usize,
    entry_pos: usize,
) -> (bool, Vec<String>) {
    let mut warnings = Vec::new();
    // Mirror of the reader's effective crop: an absent `crop_rect` means a
    // small default box around the bubble's own anchor.
    let stored = read_uv_rect(obj.get("crop_rect"));
    let effective = stored.unwrap_or_else(|| {
        let u = get_f64(obj, "img_u").unwrap_or(0.5);
        let v = get_f64(obj, "img_v").unwrap_or(0.5);
        [u - 0.05, v - 0.05, u + 0.05, v + 0.05]
    });
    if !geo.keeps_uv_rect(effective) {
        obj.remove("crop_page_idx");
        obj.remove("crop_rect");
        warnings.push(format!(
            "bubble entry #{entry_pos} cropped a region of page {crop_idx} that the crop \
             removed entirely; its crop link was removed"
        ));
        return (true, warnings);
    }
    let Some(rect) = stored else {
        warnings.push(format!(
            "bubble entry #{entry_pos} crops the cropped page but stores no crop_rect; its \
             crop follows the page's new frame unchanged"
        ));
        return (false, warnings);
    };
    let mapped = geo.placement().map_uv_rect(rect);
    let clamped = mapped.map(|value| value.clamp(0.0, 1.0));
    if clamped != mapped {
        warnings.push(format!(
            "bubble entry #{entry_pos} crops a region of page {crop_idx} that reaches past \
             the new frame; its crop was trimmed to what the crop kept"
        ));
    }
    obj.insert(
        "crop_rect".to_string(),
        Value::Array(clamped.iter().map(|value| number(*value)).collect()),
    );
    (true, warnings)
}

/// Whether a bubble still has any of its footprint inside the kept region of a
/// crop.
///
/// Evidence order: the bubble's own `rect_coords` box (the image area the user
/// sees), then — when that box cannot be read — its `img_u`/`img_v` anchor,
/// which defaults to the page centre exactly as the reader's does. The box is
/// preferred deliberately: the question a crop asks is "does anything of this
/// bubble remain visible", not the split's "which part owns it", and an
/// anchor-only test would archive a bubble whose box still overlaps the frame.
fn bubble_survives_crop(obj: &Map<String, Value>, geo: &CropGeometry) -> bool {
    if let Some(rect) = bubble_uv_rect(obj) {
        return geo.keeps_uv_rect(rect);
    }
    let u = get_f64(obj, "img_u").unwrap_or(0.5);
    let v = get_f64(obj, "img_v").unwrap_or(0.5);
    geo.keeps_uv_point(u, v)
}

/// Reads a bubble's `rect_coords` box as a page-normalized `[u1, v1, u2, v2]`,
/// or `None` when either corner is missing or unreadable.
fn bubble_uv_rect(obj: &Map<String, Value>) -> Option<[f64; 4]> {
    let rect = obj.get("rect_coords")?.as_object()?;
    let corner = |key: &str| -> Option<(f64, f64)> {
        let point = rect.get(key)?.as_object()?;
        Some((get_f64(point, "img_u")?, get_f64(point, "img_v")?))
    };
    let (u1, v1) = corner("p1")?;
    let (u2, v2) = corner("p2")?;
    Some([u1, v1, u2, v2])
}

/// Reads a `[u1, v1, u2, v2]` page-normalized rectangle.
fn read_uv_rect(value: Option<&Value>) -> Option<[f64; 4]> {
    let array = value?.as_array()?;
    if array.len() < 4 {
        return None;
    }
    Some([
        read_f64(array.first()?)?,
        read_f64(array.get(1)?)?,
        read_f64(array.get(2)?)?,
        read_f64(array.get(3)?)?,
    ])
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
/// With a STITCH geometry, an entry on a merged page additionally has its
/// placement mapped into the new canvas (`img_x_px`/`img_y_px`, `img_u`/`u`,
/// `img_v`/`v`, `deform_mesh`, `transform_uv`, `scale`) and its `layer_idx`
/// re-based, keeping the typing tab's text groups of different source pages
/// distinct.
///
/// With a SPLIT geometry, an entry on the cut page is routed to one part before
/// being mapped: by its deform mesh's area when it has one, otherwise by its
/// decoded centre point. Its extent is NOT recorded in this legacy document, so
/// an un-deformed overlay cannot be judged by area here — in a v3 chapter the
/// authoritative record of the same overlay is the `layers.json` node, which
/// IS judged by the exact-area rule. `layer_idx` is not re-based: parts are
/// different pages, so their text-group axes are distinct for free.
///
/// With a CROP geometry, an entry on the cropped page keeps its page and is
/// mapped through the crop transform, so it may hang off the new page's edge.
/// An entry whose whole footprint fell outside the kept region is archived with
/// its overlay PNG recorded for trashing, exactly like an entry of a deleted
/// page. Survival is judged by the same evidence the split routes by: the
/// deform mesh's cell areas when there is one, otherwise the decoded centre
/// point (this document does not record the overlay's extent).
///
/// # Errors
/// [`PageOpError::InvalidOp`] for the legacy absolute-coordinate placement
/// family (numeric `x`/`y`, no modern or bare normalized coordinates): those
/// entries are keyed by continuous-ribbon position — which any page operation
/// changes — and must be migrated by opening the typing tab first.
pub(crate) fn remap_text_info(
    entries: &[Value],
    old_to_new: &[Option<usize>],
    geometry: PageGeometry<'_>,
) -> Result<TextInfoRemap, PageOpError> {
    // Exhaustive by design (see `PageGeometry` and `GeometryParts`).
    let (stitch, split_geo, crop): GeometryParts<'_> = match geometry {
        PageGeometry::None => (None, None, None),
        PageGeometry::Stitch(geo) => (Some(geo), None, None),
        PageGeometry::Split(geo) => (None, Some(geo), None),
        PageGeometry::Crop(geo) => (None, None, Some(geo)),
    };
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
                // An entry of the CROPPED page whose whole footprint fell
                // outside the kept region follows the deleted-page arm above:
                // archived, with its PNG (and `*_layout.png` companion)
                // recorded for trashing.
                if let Some(geo) = crop
                    && geo.source_old_idx() == old_idx
                    && !text_info_survives_crop(obj, geo)
                {
                    if let Some(file) = obj.get("file").and_then(Value::as_str) {
                        let trimmed = file.trim();
                        if !trimmed.is_empty() {
                            deleted_files.push(trimmed.to_string());
                        }
                    }
                    warnings.push(format!(
                        "text_info entry #{pos} lies entirely outside the kept region of the \
                         cropped page {old_idx}; archived in the trash"
                    ));
                    deleted.push(entry.clone());
                    changed = true;
                    continue;
                }
                // A split routes the entry to a PART; the index map can only
                // name the representative one.
                let split = split_geo
                    .filter(|geo| geo.source_old_idx() == old_idx)
                    .map(|geo| (geo, text_info_part(obj, geo)));
                // An overlay on a DELETED part follows the deleted-page arm
                // above: archived, with its PNG (and `*_layout.png` companion)
                // recorded for trashing — never re-homed on a surviving part.
                if let Some((geo, part)) = split
                    && geo.is_deleted_part(part)
                {
                    if let Some(file) = obj.get("file").and_then(Value::as_str) {
                        let trimmed = file.trim();
                        if !trimmed.is_empty() {
                            deleted_files.push(trimmed.to_string());
                        }
                    }
                    warnings.push(format!(
                        "text_info entry #{pos} sits on deleted split part {part}; archived \
                         in the trash instead of being moved to another part"
                    ));
                    deleted.push(entry.clone());
                    changed = true;
                    continue;
                }
                let (new_idx, placement) = match split {
                    Some((geo, part)) => {
                        let target = geo.part_new_idx(part).ok_or_else(|| {
                            PageOpError::InvalidOp(format!(
                                "split part {part} has no index in the new order"
                            ))
                        })?;
                        (target, geo.placement(part))
                    }
                    None => {
                        let placement = stitch.and_then(|geo| geo.placement(old_idx)).or_else(
                            || {
                                crop.filter(|geo| geo.source_old_idx() == old_idx)
                                    .map(CropGeometry::placement)
                            },
                        );
                        (new_idx, placement)
                    }
                };
                if new_idx != old_idx || !has_img_idx || placement.is_some() {
                    let mut new_obj = obj.clone();
                    new_obj.insert("img_idx".to_string(), Value::from(new_idx));
                    if let Some(placement) = placement {
                        apply_text_info_geometry(&mut new_obj, placement);
                        // Only a stitch re-bases the text-group axis; a split
                        // separates the pages, so their axes cannot collide, and
                        // a crop keeps ONE page, so its axis is untouched.
                        if let Some(geo) = stitch {
                            offset_layer_idx(&mut new_obj, geo.layer_idx_offset(old_idx));
                        }
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

/// Maps one legacy `text_info.json` overlay entry into a placement's canvas.
///
/// Mirrors the position vocabulary of `text_payload::decode_overlay_placement`:
/// absolute `img_x_px`/`img_y_px` win over normalized `img_u`/`u` +
/// `img_v`/`v`, a `deform_mesh` may store either `points_px` or the legacy
/// `points_uv`, and `transform_uv` is a quad of normalized corners. The entry's
/// stored angle (`rotation_deg`, or its legacy `angle` alias, in DEGREES) gains
/// the placement's own page rotation: a uniform scale preserves angles, but a
/// ROTATING placement does not, and an overlay whose angle did not turn with its
/// page would be drawn unrotated on a rotated page. `scale`/`user_scale` —
/// a page-pixel size factor of a NON-resampled overlay PNG — follows the
/// placement scale, exactly like `TransformRec::scale` in `layers.json`.
fn apply_text_info_geometry(obj: &mut Map<String, Value>, placement: &PlacementMap) {
    map_px_keys(obj, "img_x_px", "img_y_px", placement);
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
    // Stored in DEGREES here (`text_payload::encode_overlay_transform` writes
    // `rotation_deg`; `angle` is the legacy alias the decoder still accepts).
    add_stored_angle(
        obj,
        &["rotation_deg", "angle"],
        placement.rotation_degrees(),
        360.0,
    );
}

/// Geometric part a legacy `text_info.json` overlay entry belongs to.
///
/// Mirrors the position vocabulary of `text_payload::decode_overlay_placement`:
/// absolute `img_x_px`/`img_y_px` win over normalized `img_u`/`u` +
/// `img_v`/`v`, and a missing position reads as the page centre. A
/// `deform_mesh` supplies a real polygon, so the exact-area rule applies to it
/// (summed over the mesh's grid CELLS, which stays correct for a folded mesh);
/// without one only the centre point is known, because this document does not
/// record the overlay's extent at all.
///
/// The centre-of-page default is a real POSITION, not an absence of evidence:
/// the loader draws such an entry at the page centre, so it belongs to the part
/// covering that point even when the request deletes it. This is deliberately
/// unlike `assign_layer_part`'s no-placement case, which has no position at all
/// and must fall back to a surviving part.
fn text_info_part(obj: &Map<String, Value>, geo: &SplitGeometry) -> usize {
    let [page_w, page_h] = geo.page_size();
    if let Some(mesh) = obj.get("deform_mesh")
        && let Some(part) = deform_mesh_part(mesh, Some([page_w, page_h]), geo)
    {
        return part;
    }
    let (x, y) = match (get_f64(obj, "img_x_px"), get_f64(obj, "img_y_px")) {
        (Some(x), Some(y)) => (x, y),
        _ => {
            let u = get_f64(obj, "img_u")
                .or_else(|| get_f64(obj, "u"))
                .unwrap_or(0.5);
            let v = get_f64(obj, "img_v")
                .or_else(|| get_f64(obj, "v"))
                .unwrap_or(0.5);
            (u * page_w, v * page_h)
        }
    };
    geo.part_for_point(x, y)
}

/// Whether a legacy `text_info.json` overlay entry keeps any of its footprint
/// inside the kept region of a crop.
///
/// The crop mirror of [`text_info_part`], with the same evidence order and the
/// same limitation: a `deform_mesh` supplies a real polygon and is judged by
/// area (summed over its grid CELLS, which stays correct for a folded mesh),
/// while without one only the decoded centre point is known — this document does
/// not record the overlay's extent. The centre-of-page default is a real
/// POSITION, not an absence of evidence, so an entry with no stored placement is
/// judged by whether the crop kept the page centre.
fn text_info_survives_crop(obj: &Map<String, Value>, geo: &CropGeometry) -> bool {
    let [page_w, page_h] = geo.page_size();
    if let Some(mesh) = obj.get("deform_mesh")
        && let Some(cells) = deform_mesh_cells(mesh, Some([page_w, page_h]))
    {
        let pieces: Vec<&[[f64; 2]]> = cells.iter().map(Vec::as_slice).collect();
        if let Some(keeps) = geo.keeps_polygon_group(&pieces) {
            return keeps;
        }
    }
    let (x, y) = match (get_f64(obj, "img_x_px"), get_f64(obj, "img_y_px")) {
        (Some(x), Some(y)) => (x, y),
        _ => {
            let u = get_f64(obj, "img_u")
                .or_else(|| get_f64(obj, "u"))
                .unwrap_or(0.5);
            let v = get_f64(obj, "img_v")
                .or_else(|| get_f64(obj, "v"))
                .unwrap_or(0.5);
            (u * page_w, v * page_h)
        }
    };
    geo.keeps_point(x, y)
}

/// The GRID CELL quads of a stored deform mesh, in absolute page pixels: one
/// four-point ring `[(r,c), (r,c+1), (r+1,c+1), (r+1,c)]` per cell, in row
/// order.
///
/// A mesh OVERRIDES its node's affine transform (`tabs/ps_editor/layers.rs`
/// documents the rule), so the mesh — not the transform quad — is the layer's
/// real footprint. The footprint is expressed CELL BY CELL rather than as the
/// mesh's outer boundary ring because a user can FOLD a mesh in the typing tab:
/// the lobes of a self-intersecting ring cancel in the signed shoelace sum, so
/// its area is not the filled area and the layer would route by a bogus
/// comparison. Summing the ABSOLUTE area of every cell stays correct however
/// the cells overlap (see `SplitGeometry::part_for_polygon_group`).
///
/// `points_px` is absolute page pixels; the legacy `points_uv` form is
/// converted with `page_size`, which must therefore be supplied wherever that
/// form can occur (`text_info.json`). Returns `None` for a missing, degenerate
/// or inconsistent grid, so the caller falls back.
fn deform_mesh_cells(mesh: &Value, page_size: Option<[f64; 2]>) -> Option<Vec<Vec<[f64; 2]>>> {
    let obj = mesh.as_object()?;
    let cols = usize::try_from(obj.get("cols")?.as_u64()?).ok()?;
    let rows = usize::try_from(obj.get("rows")?.as_u64()?).ok()?;
    if cols < 2 || rows < 2 {
        return None;
    }
    let (raw, scale) = match obj.get("points_px") {
        Some(points) => (points.as_array()?, [1.0, 1.0]),
        None => (obj.get("points_uv")?.as_array()?, page_size?),
    };
    if raw.len() != cols.checked_mul(rows)? {
        return None;
    }
    let point = |index: usize| -> Option<[f64; 2]> {
        let pair = raw.get(index)?.as_array()?;
        Some([
            read_f64(pair.first()?)? * scale[0],
            read_f64(pair.get(1)?)? * scale[1],
        ])
    };
    let mut cells = Vec::with_capacity((cols - 1) * (rows - 1));
    for row in 0..rows - 1 {
        for col in 0..cols - 1 {
            // Row-major grid: the cell's own corners, walked consistently.
            let top_left = row * cols + col;
            cells.push(vec![
                point(top_left)?,
                point(top_left + 1)?,
                point(top_left + cols + 1)?,
                point(top_left + cols)?,
            ]);
        }
    }
    Some(cells)
}

/// Geometric part a stored deform mesh belongs to, by the summed ABSOLUTE area
/// of its grid cells clipped against each part.
///
/// Returns `None` when the mesh is missing, degenerate or encloses no area at
/// all, so the caller falls back to the transform quad or the centre point.
/// `page_size` is required only for the legacy `points_uv` mesh form.
fn deform_mesh_part(
    mesh: &Value,
    page_size: Option<[f64; 2]>,
    geo: &SplitGeometry,
) -> Option<usize> {
    let cells = deform_mesh_cells(mesh, page_size)?;
    let pieces: Vec<&[[f64; 2]]> = cells.iter().map(Vec::as_slice).collect();
    geo.part_for_polygon_group(&pieces)
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
    /// Page entries the operation REMOVED, archived verbatim in the trash as
    /// `deleted_layers_pages.json`.
    ///
    /// One entry per page that lost records, in either of two shapes — both
    /// carry the page's ORIGINAL `img_idx`, so the archive reads uniformly:
    /// - a page deleted WHOLE contributes its entry exactly as it stood;
    /// - a page that SURVIVES but lost part of its tree (a split's deleted
    ///   part, a crop's removed frame) contributes an entry holding just the
    ///   dropped records plus the groups they belonged to (see
    ///   [`archived_layer_page`]).
    ///
    /// Never empty when records were dropped: only a layer's rendered PNG goes
    /// to the trash, so this document is the sole surviving copy of its
    /// transform, its group membership and a TEXT node's typed text.
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
/// With a STITCH geometry the page entries of the merged pages are folded
/// into ONE entry at the new index (see [`merge_stitched_pages`]) with their
/// layer geometry mapped into the stitched canvas. With a SPLIT geometry the
/// cut page's single entry is PARTITIONED into one entry per part that holds at
/// least one layer (see [`split_page_layers`]), each mapped into that part.
/// With a CROP geometry the cropped page keeps its single entry, the records
/// that fall entirely outside the kept region are dropped from it (see
/// [`crop_page_layers`]) and the rest are mapped through the crop.
///
/// `tree_rel` names the tree this manifest belongs to; a split's and a crop's
/// layer routing are resolved per tree, because the committed and the staging
/// manifest are independent documents.
///
/// # Errors
/// - [`PageOpError::Json`] when the manifest root is not an object.
/// - [`PageOpError::InvalidOp`] when two merged pages share a layer/group uid.
pub(crate) fn remap_layers_manifest(
    manifest: &Value,
    old_to_new: &[Option<usize>],
    geometry: PageGeometry<'_>,
    tree_rel: &str,
) -> Result<LayersRemap, PageOpError> {
    // Exhaustive by design (see `PageGeometry` and `GeometryParts`).
    let (stitch, split, crop): GeometryParts<'_> = match geometry {
        PageGeometry::None => (None, None, None),
        PageGeometry::Stitch(geo) => (Some(geo), None, None),
        PageGeometry::Split(geo) => (None, Some(geo), None),
        PageGeometry::Crop(geo) => (None, None, Some(geo)),
    };
    let split_routing = split.and_then(|geo| geo.routing(tree_rel));
    let crop_routing = crop.and_then(|geo| geo.routing(tree_rel));
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
                            remap_layer_rec_files(rec_obj, old_to_new, split_routing);
                        }
                    }
                }
                if let Some(geo) = split
                    && geo.source_old_idx() == old_idx
                {
                    kept.extend(split_page_layers(
                        &new_page,
                        geo,
                        split_routing,
                        &mut deleted_pages,
                        &mut warnings,
                    )?);
                    changed = true;
                    continue;
                }
                if let Some(geo) = crop
                    && geo.source_old_idx() == old_idx
                {
                    kept.push(Value::Object(crop_page_layers(
                        &new_page,
                        geo,
                        crop_routing,
                        &mut deleted_pages,
                        &mut warnings,
                    )));
                    changed = true;
                    continue;
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
/// `transform.rotation` (RADIANS) gains the placement's page rotation, which is
/// zero for a stitch and a split and non-zero only for a crop — a uniform scale
/// preserves angles, a rotation does not, and a layer whose angle did not turn
/// with its page would be drawn unrotated on a rotated page; and
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
                map_px_keys(transform, "cx", "cy", placement);
                scale_key(transform, "scale", placement);
                // `TransformRec::rotation` is stored in RADIANS
                // (`layer_model/persist.rs`), unlike `text_info`'s degrees.
                add_stored_angle(
                    transform,
                    &["rotation"],
                    placement.rotation_radians(),
                    std::f64::consts::TAU,
                );
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
                map_px_keys(frame, "cx", "cy", placement);
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

/// Resolves which geometric part each layer of the SPLIT page belongs to, for
/// ONE tree.
///
/// This is the table the index map cannot express: a split fans one page's
/// layers out onto several new pages, so both the manifest partition and the
/// layer-PNG rename need a per-node answer. Returns the routing plus the
/// diagnostics of every node whose footprint could not be measured exactly.
///
/// `layer_png_sizes` supplies the pixel size of the page's layer PNGs (probed
/// by `fs_exec::scan_chapter`), which is the ONLY way to size a TEXT node — its
/// record stores `image_size: None`.
///
/// A malformed manifest yields an empty routing rather than an error:
/// [`remap_layers_manifest`] runs later over the same document and reports the
/// structural problem with full context.
///
/// A record IS routed to a deleted part, so that [`split_page_layers`] can tell
/// it apart from an unroutable record and drop it with that part; the PNGs only
/// such records reference are reported as deleted files, for `plan_layer_pngs`
/// to trash.
///
/// # Errors
/// [`PageOpError::InvalidOp`] when ONE layer PNG of the cut page is referenced
/// by records that route to DIFFERENT SURVIVING parts. The file can only move
/// to one `ps_p{page:04}_` prefix, so any other answer would leave a record pointing
/// at a PNG owned by another page — and `persist.rs::prune_orphan_pngs` prunes
/// by that prefix, so a later save of the owning page would delete a PNG the
/// other page still references. There are no shared-file semantics to fall
/// back on, so the operation is refused (the same rule the stitch applies to a
/// duplicated layer uid).
pub(crate) fn split_layer_routing(
    manifest: Option<&Value>,
    layer_png_sizes: &std::collections::BTreeMap<String, [u32; 2]>,
    geo: &SplitGeometry,
) -> Result<(SplitTreeRouting, Vec<String>), PageOpError> {
    let mut node_part: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut file_new_idx: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    // PNGs whose only claimants sit on deleted parts; a later surviving claim
    // removes a name from here, because that record still needs the pixels.
    let mut deleted_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Which SURVIVING geometric part first claimed each file, so a second claim
    // from a different one can be refused instead of silently overwriting.
    let mut file_part: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut warnings = Vec::new();

    let page = manifest
        .and_then(Value::as_object)
        .and_then(|root| root.get("pages"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .find(|page| {
            page.get("img_idx")
                .and_then(Value::as_u64)
                .and_then(|idx| usize::try_from(idx).ok())
                == Some(geo.source_old_idx())
        });
    let Some(page) = page else {
        return Ok((
            SplitTreeRouting::new(node_part, file_new_idx, deleted_files),
            warnings,
        ));
    };

    for rec in page
        .get("tree")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
    {
        let part = assign_layer_part(rec, layer_png_sizes, geo, &mut warnings);
        // Routed FIRST, deleted parts included: a node must be distinguishable
        // from an unroutable one, which follows the first surviving part
        // instead of being dropped.
        if let Some(uid) = rec.get("uid").and_then(Value::as_str) {
            node_part.insert(uid.to_string(), part);
        } else {
            warnings.push(
                "a layer record of the split page has no uid; it follows the first \
                 surviving part"
                    .to_string(),
            );
        }
        let Some(new_idx) = geo.part_new_idx(part) else {
            // The part was DELETED: the record vanishes with it (its bucket is
            // discarded by `split_page_layers`) and the PNGs only it references
            // become deletion candidates. They are trashed rather than renamed
            // onto a surviving part, unless a surviving record claims the same
            // file below.
            for key in ["base_file", "rendered_file"] {
                let Some(name) = rec.get(key).and_then(Value::as_str) else {
                    continue;
                };
                if super::plan::parse_layers_png_page_idx(name) == Some(geo.source_old_idx())
                    && !file_new_idx.contains_key(name)
                {
                    deleted_files.insert(name.to_string());
                }
            }
            continue;
        };
        // Only the CUT page's own PNGs fan out; a cross-page reference keeps
        // following the ordinary index map.
        for key in ["base_file", "rendered_file"] {
            let Some(name) = rec.get(key).and_then(Value::as_str) else {
                continue;
            };
            if super::plan::parse_layers_png_page_idx(name) == Some(geo.source_old_idx()) {
                if let Some(previous) = file_part.insert(name.to_string(), part)
                    && previous != part
                {
                    let source = geo.source_old_idx();
                    return Err(PageOpError::InvalidOp(format!(
                        "split cannot cut page {source}: layer PNG '{name}' is referenced \
                         by layer records routed to different parts ({previous} and \
                         {part}), and one file can only move to one page prefix"
                    )));
                }
                file_new_idx.insert(name.to_string(), new_idx);
                // A surviving record needs the pixels, so it outranks any claim
                // an earlier deleted record made on the same file.
                deleted_files.remove(name);
            }
        }
    }
    Ok((
        SplitTreeRouting::new(node_part, file_new_idx, deleted_files),
        warnings,
    ))
}

/// Geometric part ONE layer record belongs to, by the exact-area rule.
///
/// Order of evidence, strongest first:
/// 1. a `deform` mesh — it OVERRIDES the affine transform, so the summed
///    absolute area of its grid CELLS is the layer's real footprint (a cell
///    sum, not an outer ring: a folded mesh's ring area cancels itself);
/// 2. the transform quad — the four `local_to_world` corners of the layer
///    image (`tabs/ps_editor/layers.rs::world_corners`), which needs the image
///    size: `image_size` for a raster, a probed `rendered_file` for a TEXT node
///    that stores none;
/// 3. the transform CENTRE point, when the size is unknown (an unreadable or
///    missing render). That is a documented degradation of the stated rule, so
///    it always pushes a warning.
///
/// A record with NO placement evidence at all (neither a mesh nor a transform)
/// cannot be routed geometrically, so it falls back to the first SURVIVING part
/// — never to the literal part 0, which the request may have deleted. Dropping
/// such a record would destroy user data as a side effect of a fallback, not
/// because the geometry said so.
fn assign_layer_part(
    rec: &Map<String, Value>,
    layer_png_sizes: &std::collections::BTreeMap<String, [u32; 2]>,
    geo: &SplitGeometry,
    warnings: &mut Vec<String>,
) -> usize {
    let name = rec
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or("<no uid>")
        .to_string();
    if let Some(mesh) = rec.get("deform")
        && let Some(part) = deform_mesh_part(mesh, None, geo)
    {
        return part;
    }
    let Some(transform) = rec.get("transform").and_then(Value::as_object) else {
        warnings.push(format!(
            "layer '{name}' of the split page has no transform and no deform mesh; it \
             follows the first surviving part"
        ));
        // Not a literal 0: part 0 may be DELETED, and a record the engine could
        // not route must never be destroyed by that fallback.
        return geo.first_kept_part();
    };
    let cx = get_f64(transform, "cx").unwrap_or(0.0);
    let cy = get_f64(transform, "cy").unwrap_or(0.0);
    match layer_image_size(rec, layer_png_sizes) {
        Some([width, height]) if width > 0.0 && height > 0.0 => {
            let rotation = get_f64(transform, "rotation").unwrap_or(0.0);
            let scale = get_f64(transform, "scale").unwrap_or(1.0);
            let quad = layer_world_quad(cx, cy, rotation, scale, width, height);
            if let Some(part) = geo.part_for_polygon(&quad) {
                return part;
            }
        }
        _ => warnings.push(format!(
            "layer '{name}' of the split page has no measurable image size (a text render \
             whose PNG could not be probed); it was assigned by its centre point instead of \
             by area"
        )),
    }
    geo.part_for_point(cx, cy)
}

/// Intrinsic pixel size of a layer record's image: the stored `image_size` of a
/// raster, or the PROBED size of its rendered/base PNG (a TEXT node stores
/// `image_size: None`, `models/layer_model/persist.rs::text_payload_rec`).
fn layer_image_size(
    rec: &Map<String, Value>,
    layer_png_sizes: &std::collections::BTreeMap<String, [u32; 2]>,
) -> Option<[f64; 2]> {
    if let Some(size) = read_u32_pair(rec.get("image_size")) {
        return Some([f64::from(size[0]), f64::from(size[1])]);
    }
    for key in ["rendered_file", "base_file"] {
        if let Some(name) = rec.get(key).and_then(Value::as_str)
            && let Some(size) = layer_png_sizes.get(name)
        {
            return Some([f64::from(size[0]), f64::from(size[1])]);
        }
    }
    None
}

/// The four page-pixel corners of a layer image placed by a center-anchored
/// affine, in `tl, tr, br, bl` order.
///
/// Mirrors `tabs/ps_editor/layers.rs::local_to_world`:
/// `corner = center + rotate((local - image_center) * scale, rotation)`, with
/// `rotation` in RADIANS and +y pointing down.
#[must_use]
fn layer_world_quad(
    cx: f64,
    cy: f64,
    rotation: f64,
    scale: f64,
    width: f64,
    height: f64,
) -> [[f64; 2]; 4] {
    let (sin, cos) = rotation.sin_cos();
    let half_w = width * 0.5 * scale;
    let half_h = height * 0.5 * scale;
    let place = |x: f64, y: f64| [cx + x * cos - y * sin, cy + x * sin + y * cos];
    [
        place(-half_w, -half_h),
        place(half_w, -half_h),
        place(half_w, half_h),
        place(-half_w, half_h),
    ]
}

/// The `groups` / `text_groups` records of `page` that `tree`'s records belong
/// to, in the manifest's own order.
///
/// A `GroupRec` is claimed by any record carrying its `uid` in `group_uid`; a
/// `TextGroupRec` band is keyed by the `layer_idx` of the UNPINNED text nodes of
/// the set (a pinned text owns its own band instead), which is exactly the set
/// the manifest contract says the bands describe. Group uids are page-scoped, so
/// the same group may legitimately be claimed by several of the sets one page
/// is broken into.
fn page_groups_for(page: &Map<String, Value>, tree: &[Value]) -> (Vec<Value>, Vec<Value>) {
    let members: std::collections::HashSet<&str> = tree
        .iter()
        .filter_map(|rec| rec.get("group_uid")?.as_str())
        .collect();
    let groups: Vec<Value> = page
        .get("groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|group| {
            group
                .get("uid")
                .and_then(Value::as_str)
                .is_some_and(|uid| members.contains(uid))
        })
        .cloned()
        .collect();
    let band_owners: std::collections::HashSet<u64> = tree
        .iter()
        .filter(|rec| {
            rec.get("kind").and_then(Value::as_str) == Some("text")
                && !rec.get("pinned").and_then(Value::as_bool).unwrap_or(false)
        })
        .filter_map(|rec| rec.get("layer_idx")?.as_u64())
        .collect();
    let text_groups: Vec<Value> = page
        .get("text_groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|group| {
            group
                .get("layer_idx")
                .and_then(Value::as_u64)
                .is_some_and(|idx| band_owners.contains(&idx))
        })
        .cloned()
        .collect();
    (groups, text_groups)
}

/// Builds the trash-archive entry for layer records dropped from a page that
/// itself SURVIVES the operation (a split's deleted part, a crop's removed
/// frame).
///
/// Shape: the SAME page-entry object the whole-page archive uses, so
/// `deleted_layers_pages.json` stays one uniform array of page entries whatever
/// removed the records, and a manual recovery reads it the same way in both
/// cases. It carries the page's ORIGINAL `img_idx` (like a page deleted whole),
/// the dropped records VERBATIM — their coordinates are in the page's own
/// pre-operation space, because the geometry mapping is applied per surviving
/// set and never touches these — and the group / text-group records those
/// layers belonged to, whose names and bands are not recoverable from a layer
/// record alone. Every other key of the page entry is inherited, so
/// unknown/future fields survive.
///
/// Archiving is NOT optional: only a dropped layer's rendered PNG goes to the
/// trash, while its transform, its group membership and — for a TEXT node — the
/// typed text itself live solely in this document.
fn archived_layer_page(
    page: &Map<String, Value>,
    old_idx: usize,
    dropped: Vec<Value>,
) -> Map<String, Value> {
    let (groups, text_groups) = page_groups_for(page, &dropped);
    let mut entry = page.clone();
    // The page's own index BEFORE the operation: the archive describes what the
    // chapter looked like when the records still existed.
    entry.insert("img_idx".to_string(), Value::from(old_idx));
    entry.insert("tree".to_string(), Value::Array(dropped));
    // Match the manifest writer, which omits these keys when empty.
    for (key, items) in [("groups", groups), ("text_groups", text_groups)] {
        if items.is_empty() {
            entry.remove(key);
        } else {
            entry.insert(key.to_string(), Value::Array(items));
        }
    }
    entry
}

/// Partitions the already-file-remapped page entry of the SPLIT page into one
/// entry per part that holds at least one layer.
///
/// Layers are never cut: each node moves WHOLE to the part `routing` assigned
/// it, and its geometry is then mapped through that part's placement — so a
/// node crossing a cut legitimately hangs off its new page's edge. Per part:
/// - `z` is re-ranked densely over that part's own bands (`tree` +
///   `text_groups`), because `z` is a PER-PAGE band axis;
/// - a `GroupRec` is DUPLICATED into every part holding one of its members and
///   omitted from the parts holding none; group uids are page-scoped (every
///   `LayerDoc` group operation takes a `page_idx`), so the duplication cannot
///   collide;
/// - a `TextGroupRec` band follows the same rule, keyed by the `layer_idx` of
///   the part's UNPINNED text nodes, which is exactly the set the manifest
///   contract says the bands describe;
/// - every other key of the source entry is inherited, so unknown/future fields
///   survive.
///
/// `layer_idx` is deliberately NOT re-based: the parts are different pages, so
/// their «Группа текста N» axes are distinct without any offset.
///
/// A part the request DELETED yields no entry at all: its records are dropped
/// with it and reported in `warnings`, never re-homed on a surviving part. They
/// are ARCHIVED into `archived` first (see [`archived_layer_page`]) — only their
/// PNGs go to the trash, so the records themselves are the sole copy of the
/// layer's transform, group membership and typed text.
///
/// # Errors
/// [`PageOpError::Json`] when a SURVIVING part has no placement or no index in
/// the new order (an internally inconsistent geometry).
fn split_page_layers(
    page: &Map<String, Value>,
    geo: &SplitGeometry,
    routing: Option<&SplitTreeRouting>,
    archived: &mut Vec<Value>,
    warnings: &mut Vec<String>,
) -> Result<Vec<Value>, PageOpError> {
    let mut per_part: Vec<Vec<Value>> = vec![Vec::new(); geo.part_count()];
    for rec in page
        .get("tree")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let part = rec
            .as_object()
            .and_then(|obj| obj.get("uid"))
            .and_then(Value::as_str)
            .and_then(|uid| routing.and_then(|routing| routing.node_part(uid)))
            // An unrouted node (no uid, or a manifest the routing pass could
            // not read) follows the first SURVIVING part; the routing pass
            // warned. It must not default to part 0, which may be deleted —
            // the record would then vanish without the user asking for it.
            .unwrap_or_else(|| geo.first_kept_part());
        if let Some(sink) = per_part.get_mut(part) {
            sink.push(rec.clone());
        }
    }

    let mut out = Vec::with_capacity(geo.part_count());
    // Records of every deleted part, in geometric part order, archived as ONE
    // entry: the archive describes what was removed from the PAGE, not from
    // each part.
    let mut dropped: Vec<Value> = Vec::new();
    for (part, tree) in per_part.into_iter().enumerate() {
        if tree.is_empty() {
            continue;
        }
        // A deleted part produces no manifest entry: its records are dropped
        // together with the page they would have described, never moved onto a
        // surviving part. Their PNGs are trashed by `plan_layer_pngs` via the
        // routing's deleted-file set, and the records themselves are archived
        // below — the PNG alone does not carry the layer's metadata.
        if geo.is_deleted_part(part) {
            warnings.push(format!(
                "{} layer record(s) of deleted split part {part} were discarded with it \
                 and archived in the trash",
                tree.len()
            ));
            dropped.extend(tree);
            continue;
        }
        let (Some(placement), Some(new_idx)) = (geo.placement(part), geo.part_new_idx(part))
        else {
            return Err(PageOpError::Json(format!(
                "split part {part} has no placement or no index in the new order"
            )));
        };

        // PS groups and text-group bands this part's nodes actually claim.
        let (groups, text_groups) = page_groups_for(page, &tree);

        let mut entry = page.clone();
        entry.insert("img_idx".to_string(), Value::from(new_idx));
        entry.insert("tree".to_string(), Value::Array(tree));
        // Match the manifest writer, which omits these keys when empty.
        for (key, items) in [("groups", groups), ("text_groups", text_groups)] {
            if items.is_empty() {
                entry.remove(key);
            } else {
                entry.insert(key.to_string(), Value::Array(items));
            }
        }
        rerank_page_bands(&mut entry);
        apply_page_layers_geometry(&mut entry, placement, 0);
        out.push(Value::Object(entry));
    }
    if !dropped.is_empty() {
        archived.push(Value::Object(archived_layer_page(
            page,
            geo.source_old_idx(),
            dropped,
        )));
    }
    Ok(out)
}

/// Resolves which layer records of the CROPPED page fall entirely outside the
/// kept region, and which layer PNGs only those records reference, for ONE tree.
///
/// The crop counterpart of [`split_layer_routing`]. It answers a keep/drop
/// question instead of a which-part question, so it cannot fail: a crop keeps
/// the page's index, so no PNG has to move to a new prefix and the "one file,
/// two destinations" refusal of a split has no analogue here.
///
/// `layer_png_sizes` supplies the pixel size of the page's layer PNGs (probed by
/// `fs_exec::scan_chapter`), which is the ONLY way to size a TEXT node — its
/// record stores `image_size: None`.
///
/// A malformed manifest yields an EMPTY routing, i.e. nothing is dropped, which
/// is the safe direction: [`remap_layers_manifest`] runs later over the same
/// document and reports the structural problem with full context.
#[must_use]
pub(crate) fn crop_layer_routing(
    manifest: Option<&Value>,
    layer_png_sizes: &std::collections::BTreeMap<String, [u32; 2]>,
    geo: &CropGeometry,
) -> (CropTreeRouting, Vec<String>) {
    let mut dropped_nodes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut dropped_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Files a SURVIVING record claims; such a claim always wins, because that
    // record still needs the pixels.
    let mut kept_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut warnings = Vec::new();

    let page = manifest
        .and_then(Value::as_object)
        .and_then(|root| root.get("pages"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .find(|page| {
            page.get("img_idx")
                .and_then(Value::as_u64)
                .and_then(|idx| usize::try_from(idx).ok())
                == Some(geo.source_old_idx())
        });
    let Some(page) = page else {
        return (
            CropTreeRouting::new(dropped_nodes, dropped_files),
            warnings,
        );
    };

    for rec in page
        .get("tree")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
    {
        let survives = layer_survives_crop(rec, layer_png_sizes, geo, &mut warnings);
        if let Some(uid) = rec.get("uid").and_then(Value::as_str) {
            if !survives {
                dropped_nodes.insert(uid.to_string());
            }
        } else if !survives {
            // Without a uid the manifest pass cannot recognize the record, so
            // it would be kept anyway; say so rather than dropping it blindly.
            warnings.push(
                "a layer record of the cropped page has no uid and could not be dropped even \
                 though it lies outside the kept region; it stays on the page"
                    .to_string(),
            );
            continue;
        }
        for key in ["base_file", "rendered_file"] {
            let Some(name) = rec.get(key).and_then(Value::as_str) else {
                continue;
            };
            // Only the cropped page's OWN PNGs can be orphaned by this
            // operation; a cross-page reference belongs to another page.
            if super::plan::parse_layers_png_page_idx(name) != Some(geo.source_old_idx()) {
                continue;
            }
            if survives {
                kept_files.insert(name.to_string());
                dropped_files.remove(name);
            } else if !kept_files.contains(name) {
                dropped_files.insert(name.to_string());
            }
        }
    }
    (
        CropTreeRouting::new(dropped_nodes, dropped_files),
        warnings,
    )
}

/// Whether ONE layer record keeps any of its footprint inside the kept region of
/// a crop.
///
/// The crop mirror of [`assign_layer_part`], with the same order of evidence,
/// strongest first:
/// 1. a `deform` mesh — it OVERRIDES the affine transform, so the summed
///    absolute area of its grid CELLS is the layer's real footprint;
/// 2. the transform quad — the four `local_to_world` corners of the layer image,
///    which needs the image size (`image_size` for a raster, a probed
///    `rendered_file` for a TEXT node that stores none);
/// 3. the transform CENTRE point, when the size is unknown. A documented
///    degradation of the stated rule, so it always warns.
///
/// A record with NO placement evidence at all (neither a mesh nor a transform)
/// is KEPT: dropping it would destroy user data as a side effect of a fallback,
/// not because the geometry said so. That mirrors `assign_layer_part`'s
/// first-surviving-part fallback, which likewise never destroys.
fn layer_survives_crop(
    rec: &Map<String, Value>,
    layer_png_sizes: &std::collections::BTreeMap<String, [u32; 2]>,
    geo: &CropGeometry,
    warnings: &mut Vec<String>,
) -> bool {
    let name = rec
        .get("uid")
        .and_then(Value::as_str)
        .unwrap_or("<no uid>")
        .to_string();
    if let Some(mesh) = rec.get("deform")
        && let Some(cells) = deform_mesh_cells(mesh, None)
    {
        let pieces: Vec<&[[f64; 2]]> = cells.iter().map(Vec::as_slice).collect();
        if let Some(keeps) = geo.keeps_polygon_group(&pieces) {
            return keeps;
        }
    }
    let Some(transform) = rec.get("transform").and_then(Value::as_object) else {
        warnings.push(format!(
            "layer '{name}' of the cropped page has no transform and no deform mesh; it is \
             kept on the page instead of being judged by area"
        ));
        return true;
    };
    let cx = get_f64(transform, "cx").unwrap_or(0.0);
    let cy = get_f64(transform, "cy").unwrap_or(0.0);
    match layer_image_size(rec, layer_png_sizes) {
        Some([width, height]) if width > 0.0 && height > 0.0 => {
            let rotation = get_f64(transform, "rotation").unwrap_or(0.0);
            let scale = get_f64(transform, "scale").unwrap_or(1.0);
            let quad = layer_world_quad(cx, cy, rotation, scale, width, height);
            if let Some(keeps) = geo.keeps_polygon(&quad) {
                return keeps;
            }
        }
        _ => warnings.push(format!(
            "layer '{name}' of the cropped page has no measurable image size (a text render \
             whose PNG could not be probed); it was judged by its centre point instead of by \
             area"
        )),
    }
    geo.keeps_point(cx, cy)
}

/// Rewrites the already-file-remapped page entry of the CROPPED page.
///
/// The page itself survives, so exactly ONE entry comes out — even when every
/// record was dropped, because the page still exists and its (now empty) entry
/// must stay. Layers are never cut: a record that keeps any of its footprint
/// inside the kept region moves whole and its geometry is mapped through the
/// crop, so it may legitimately hang off the new page's edge. A record that
/// survives nowhere is dropped with its exclusively-claimed PNGs (trashed by
/// `plan_layer_pngs`) and ARCHIVED into `archived` (see
/// [`archived_layer_page`]), exactly as a deleted split part's records are: the
/// PNG alone does not carry the layer's transform, its group membership or a
/// text node's typed text.
///
/// `z` is re-ranked densely afterwards because it is a per-page band axis and
/// dropping records leaves gaps in it; `GroupRec` / `TextGroupRec` entries whose
/// last member was dropped are removed, matching the manifest writer, which
/// omits those keys when empty.
fn crop_page_layers(
    page: &Map<String, Value>,
    geo: &CropGeometry,
    routing: Option<&CropTreeRouting>,
    archived: &mut Vec<Value>,
    warnings: &mut Vec<String>,
) -> Map<String, Value> {
    let mut dropped: Vec<Value> = Vec::new();
    let tree: Vec<Value> = page
        .get("tree")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|rec| {
            let outside = rec
                .as_object()
                .and_then(|obj| obj.get("uid"))
                .and_then(Value::as_str)
                .is_some_and(|uid| {
                    routing.is_some_and(|routing| routing.is_dropped_node(uid))
                });
            if outside {
                dropped.push((*rec).clone());
            }
            !outside
        })
        .cloned()
        .collect();
    if !dropped.is_empty() {
        warnings.push(format!(
            "{} layer record(s) lay entirely outside the kept region of the cropped \
             page and were dropped with it; the records are archived in the trash and the \
             PNGs only they referenced went there too",
            dropped.len()
        ));
        archived.push(Value::Object(archived_layer_page(
            page,
            geo.source_old_idx(),
            dropped,
        )));
    }

    // PS groups and text-group bands whose last member is gone: same membership
    // rules as `split_page_layers`, applied to the one surviving entry.
    let (groups, text_groups) = page_groups_for(page, &tree);

    let mut entry = page.clone();
    entry.insert("tree".to_string(), Value::Array(tree));
    for (key, items) in [("groups", groups), ("text_groups", text_groups)] {
        if items.is_empty() {
            entry.remove(key);
        } else {
            entry.insert(key.to_string(), Value::Array(items));
        }
    }
    rerank_page_bands(&mut entry);
    // `layer_idx` is NOT re-based: a crop keeps ONE page, so its text-group axis
    // cannot collide with another page's.
    apply_page_layers_geometry(&mut entry, geo.placement(), 0);
    entry
}

/// Re-ranks the shared per-page Z band axis of one manifest page entry densely
/// from 0, preserving the relative order of its `tree` and `text_groups`
/// records and keeping equal bands equal.
fn rerank_page_bands(page: &mut Map<String, Value>) {
    let mut bands: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    for key in ["tree", "text_groups"] {
        for rec in page.get(key).and_then(Value::as_array).into_iter().flatten() {
            if let Some(z) = rec.get("z").and_then(Value::as_u64) {
                bands.insert(z);
            }
        }
    }
    let ranks: std::collections::HashMap<u64, u64> = bands
        .iter()
        .enumerate()
        // `rank` counts the distinct bands of one page, far below u64::MAX.
        .map(|(rank, z)| (*z, u64::try_from(rank).unwrap_or(u64::MAX)))
        .collect();
    for key in ["tree", "text_groups"] {
        let Some(items) = page.get_mut(key).and_then(Value::as_array_mut) else {
            continue;
        };
        for item in items.iter_mut() {
            let Some(obj) = item.as_object_mut() else {
                continue;
            };
            if let Some(z) = obj.get("z").and_then(Value::as_u64)
                && let Some(rank) = ranks.get(&z)
            {
                obj.insert("z".to_string(), Value::from(*rank));
            }
        }
    }
}

/// Rewrites the `ps_p{page:04}_` prefix of `base_file` / `rendered_file` in
/// one layer record. The embedded index of EACH FILE NAME is remapped
/// independently (rather than assuming the page's own index) so a record
/// referencing a PNG with a different page prefix stays consistent with the
/// file-rename pass, which is also keyed by the name's embedded index.
fn remap_layer_rec_files(
    rec: &mut Map<String, Value>,
    old_to_new: &[Option<usize>],
    split: Option<&SplitTreeRouting>,
) {
    for key in ["base_file", "rendered_file"] {
        let Some(name) = rec.get(key).and_then(Value::as_str) else {
            continue;
        };
        let Some(file_idx) = super::plan::parse_layers_png_page_idx(name) else {
            continue;
        };
        // A split's PNGs of the cut page fan out onto SEVERAL prefixes, so the
        // routing (keyed by file name) wins over the index map, which can only
        // name the representative part. A PNG of a DELETED part has no routing
        // entry and falls back to the index map here, but the record carrying
        // it is dropped by `split_page_layers` right after, so that name never
        // reaches disk.
        let new_idx = split.and_then(|routing| routing.file_new_idx(name)).or_else(|| {
            if file_idx >= old_to_new.len() {
                return None;
            }
            old_to_new[file_idx]
        });
        if let Some(new_idx) = new_idx
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

/// Why a detection document's BLOCKS cannot be mapped as RECTANGLES, or `None`
/// when they can.
///
/// Complements [`detection_merge_blocker`], which only judges the document's
/// declared sizes. Two operations need a COMPLETE rectangle per block and are
/// gated by this:
/// - a SPLIT partitions the block list, so every block must land in exactly ONE
///   part's document; a block whose rectangle cannot be read is not routable and
///   would be skipped by every part, surviving nowhere;
/// - a CROP maps the blocks through a ROTATING placement, which cannot map one
///   axis without the other ([`PlacementMap::map_x_without_y`]), so a
///   half-specified block cannot be mapped at all.
///
/// A block is unreadable when it is not an object or is missing one of the
/// numeric `x1`/`y1`/`x2`/`y2` coordinates. Such an entry must never be dropped
/// silently, so it blocks the whole document and the caller trashes the page's
/// detection files with a warning — the same all-or-nothing degradation of
/// regenerable data the size checks already use.
///
/// Validating here, ONCE, is what keeps the decision in a single place:
/// [`split_detection_blocks`] and [`crop_detection_blocks`] then treat a
/// malformed block as a contract violation and fail closed instead of skipping
/// it.
#[must_use]
pub(crate) fn detection_rect_blocker(blocks: &Value, page_idx: usize) -> Option<String> {
    let items = blocks.as_object()?.get("blocks")?.as_array()?;
    for (index, block) in items.iter().enumerate() {
        let Some(obj) = block.as_object() else {
            return Some(format!(
                "page {page_idx}: blocks[{index}] is not a JSON object, so it cannot be \
                 routed to a part"
            ));
        };
        for key in ["x1", "y1", "x2", "y2"] {
            if get_f64(obj, key).is_none() {
                return Some(format!(
                    "page {page_idx}: blocks[{index}] has no numeric {key}, so it cannot \
                     be routed to a part"
                ));
            }
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
            // A complete rectangle is mapped as a rectangle; the merge gate
            // does not require one, so a block missing a coordinate still gets
            // every coordinate it does have mapped along its own axis. Only a
            // ROTATING placement cannot do that, and there the incomplete block
            // is a hard failure rather than a silently half-mapped entry.
            match (
                get_f64(&mapped, "x1"),
                get_f64(&mapped, "y1"),
                get_f64(&mapped, "x2"),
                get_f64(&mapped, "y2"),
            ) {
                (Some(x1), Some(y1), Some(x2), Some(y2)) => {
                    let [mx1, my1, mx2, my2] = placement.map_px_rect([x1, y1, x2, y2]);
                    put_f64(&mut mapped, "x1", mx1);
                    put_f64(&mut mapped, "y1", my1);
                    put_f64(&mut mapped, "x2", mx2);
                    put_f64(&mut mapped, "y2", my2);
                }
                _ => {
                    for (key, along_x) in [("x1", true), ("x2", true), ("y1", false), ("y2", false)]
                    {
                        let Some(value) = get_f64(&mapped, key) else {
                            continue;
                        };
                        let axis_mapped = if along_x {
                            placement.map_x_without_y(value)
                        } else {
                            placement.map_y_without_x(value)
                        };
                        let Some(axis_mapped) = axis_mapped else {
                            return Err(PageOpError::Json(format!(
                                "text-detection page {page_idx} has a block without a complete \
                                 x1/y1/x2/y2 rectangle, which a rotating placement cannot map"
                            )));
                        };
                        put_f64(&mut mapped, key, axis_mapped);
                    }
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

/// Builds the text-detection document of ONE split part from the cut page's
/// document.
///
/// The detector's blocks are ABSOLUTE page pixels, so each block is assigned to
/// the part holding the largest share of its rectangle and mapped through that
/// part's placement; blocks belonging elsewhere are simply absent from this
/// part's document (they appear in the document of the part that owns them).
/// `source_size` and `mask_size` become the part's own size, and `mask_file`
/// names the part's cropped mask, or is empty when the page had none — matching
/// what `save_text_detection_page` writes. Non-geometry keys are inherited so
/// unknown fields survive.
///
/// PRECONDITION: `document` has already passed [`detection_rect_blocker`], so
/// every block is routable. A malformed block is therefore a contract
/// violation and fails the operation instead of being skipped — skipping it in
/// every part would delete it from the chapter without a trace.
///
/// # Errors
/// [`PageOpError::Json`] when `part` has no placement or size in `geo`, or
/// when a block is not an object or lacks a numeric `x1`/`y1`/`x2`/`y2`.
pub(crate) fn split_detection_blocks(
    document: &Value,
    geo: &SplitGeometry,
    part: usize,
    new_idx: usize,
    mask_file: Option<&str>,
) -> Result<Value, PageOpError> {
    let (Some(placement), Some(size)) = (geo.placement(part), geo.part_size(part)) else {
        return Err(PageOpError::Json(format!(
            "text-detection split part {part} has no placement"
        )));
    };
    let mut root = document.as_object().cloned().unwrap_or_default();
    let mut blocks_out: Vec<Value> = Vec::new();
    for (index, block) in document
        .get("blocks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        // Fail closed: the gate above accepted this document, so anything
        // unroutable here is an internal inconsistency, never a reason to drop
        // the entry from every part's document.
        let Some(obj) = block.as_object() else {
            return Err(PageOpError::Json(format!(
                "text-detection blocks[{index}] of the split page is not a JSON object"
            )));
        };
        let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
            get_f64(obj, "x1"),
            get_f64(obj, "y1"),
            get_f64(obj, "x2"),
            get_f64(obj, "y2"),
        ) else {
            return Err(PageOpError::Json(format!(
                "text-detection blocks[{index}] of the split page has no numeric \
                 x1/y1/x2/y2 rectangle"
            )));
        };
        if geo.part_for_page_rect([x1, y1, x2, y2]) != part {
            continue;
        }
        let mut mapped = obj.clone();
        let [mx1, my1, mx2, my2] = placement.map_px_rect([x1, y1, x2, y2]);
        put_f64(&mut mapped, "x1", mx1);
        put_f64(&mut mapped, "x2", mx2);
        put_f64(&mut mapped, "y1", my1);
        put_f64(&mut mapped, "y2", my2);
        blocks_out.push(Value::Object(mapped));
    }
    let part_size = Value::Array(vec![Value::from(size[0]), Value::from(size[1])]);
    root.insert("page_idx".to_string(), Value::from(new_idx));
    root.insert("source_size".to_string(), part_size.clone());
    root.insert("mask_size".to_string(), part_size);
    root.insert("blocks".to_string(), Value::Array(blocks_out));
    root.insert(
        "mask_file".to_string(),
        Value::String(mask_file.unwrap_or_default().to_string()),
    );
    Ok(Value::Object(root))
}

/// Builds the text-detection document of the CROPPED page and reports how many
/// blocks the crop removed.
///
/// The detector's blocks are ABSOLUTE page pixels, so each is mapped through the
/// crop's placement; a block that keeps any of its rectangle inside the kept
/// region survives (and may reach past the new page's edge, exactly like a
/// layer), while one lying entirely outside is dropped and counted. Detection
/// output is regenerable, so a dropped block is reported as a warning count
/// rather than archived. `source_size` and `mask_size` become the new page size
/// and `mask_file` names the page's rotated mask, or is empty when the page had
/// none — matching what `save_text_detection_page` writes. Non-geometry keys are
/// inherited so unknown fields survive.
///
/// PRECONDITION: `document` has already passed [`detection_merge_blocker`] and
/// [`detection_rect_blocker`], and the crop does NOT rotate by a fine angle (the
/// caller trashes the document in that case, because an axis-aligned rectangle
/// cannot describe a freely rotated page). A malformed block is therefore a
/// contract violation and fails the operation instead of being skipped.
///
/// # Errors
/// [`PageOpError::Json`] when a block is not an object or lacks a numeric
/// `x1`/`y1`/`x2`/`y2` rectangle.
pub(crate) fn crop_detection_blocks(
    document: &Value,
    geo: &CropGeometry,
    new_idx: usize,
    mask_file: Option<&str>,
) -> Result<(Value, usize), PageOpError> {
    let placement = geo.placement();
    let mut root = document.as_object().cloned().unwrap_or_default();
    let mut blocks_out: Vec<Value> = Vec::new();
    let mut dropped = 0usize;
    for (index, block) in document
        .get("blocks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        // Fail closed: the gates above accepted this document, so anything
        // unreadable here is an internal inconsistency, never a reason to drop
        // the entry silently.
        let Some(obj) = block.as_object() else {
            return Err(PageOpError::Json(format!(
                "text-detection blocks[{index}] of the cropped page is not a JSON object"
            )));
        };
        let (Some(x1), Some(y1), Some(x2), Some(y2)) = (
            get_f64(obj, "x1"),
            get_f64(obj, "y1"),
            get_f64(obj, "x2"),
            get_f64(obj, "y2"),
        ) else {
            return Err(PageOpError::Json(format!(
                "text-detection blocks[{index}] of the cropped page has no numeric \
                 x1/y1/x2/y2 rectangle"
            )));
        };
        if !geo.keeps_page_rect([x1, y1, x2, y2]) {
            dropped += 1;
            continue;
        }
        let mut mapped = obj.clone();
        let [mx1, my1, mx2, my2] = placement.map_px_rect([x1, y1, x2, y2]);
        put_f64(&mut mapped, "x1", mx1);
        put_f64(&mut mapped, "y1", my1);
        put_f64(&mut mapped, "x2", mx2);
        put_f64(&mut mapped, "y2", my2);
        blocks_out.push(Value::Object(mapped));
    }
    let [width, height] = geo.new_size();
    let new_size = Value::Array(vec![Value::from(width), Value::from(height)]);
    root.insert("page_idx".to_string(), Value::from(new_idx));
    root.insert("source_size".to_string(), new_size.clone());
    root.insert("mask_size".to_string(), new_size);
    root.insert("blocks".to_string(), Value::Array(blocks_out));
    root.insert(
        "mask_file".to_string(),
        Value::String(mask_file.unwrap_or_default().to_string()),
    );
    Ok((Value::Object(root), dropped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_ops::SplitAxis;
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
        let out = remap_bubbles(&entries, &move_map(), PageGeometry::None).expect("remaps");
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
        let out = remap_bubbles(&entries, &delete_map(), PageGeometry::None).expect("remaps");
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
            remap_bubbles(&entries, &move_map(), PageGeometry::None),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn bubbles_out_of_range_img_idx_is_left_with_warning() {
        let entries = vec![json!({"id": 1, "img_idx": 99, "img_u": 0.5, "img_v": 0.5})];
        let out = remap_bubbles(&entries, &move_map(), PageGeometry::None).expect("remaps");
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
        let out = remap_text_info(&entries, &delete_map(), PageGeometry::None).expect("remaps");
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
            remap_text_info(&entries, &move_map(), PageGeometry::None),
            Err(PageOpError::InvalidOp(_))
        ));
    }

    #[test]
    fn text_info_rejects_legacy_ribbon_placement_with_img_idx() {
        let entries = vec![json!({
            "img_idx": 1, "x": 100.0, "y": 2000.0, "file": "t.png"
        })];
        assert!(matches!(
            remap_text_info(&entries, &move_map(), PageGeometry::None),
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
        let out = remap_layers_manifest(&manifest, &move_map(), PageGeometry::None, "ch1").expect("remaps");
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
        let out = remap_layers_manifest(&manifest, &delete_map(), PageGeometry::None, "ch1").expect("remaps");
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
                remap_layers_manifest(&manifest, &move_map(), PageGeometry::None, "ch1"),
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
        let out = remap_bubbles(&entries, &stitch_map(), PageGeometry::Stitch(&geometry)).expect("remaps");
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
        let out = remap_bubbles(&entries, &stitch_map(), PageGeometry::Stitch(&geometry)).expect("remaps");
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
        let out = remap_bubbles(&entries, &stitch_map(), PageGeometry::Stitch(&geometry)).expect("remaps");
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
        let out = remap_bubbles(&entries, &stitch_map(), PageGeometry::Stitch(&geometry)).expect("remaps");
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
        let out = remap_layers_manifest(&manifest, &stitch_map(), PageGeometry::Stitch(&geometry), "ch1")
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
        let out = remap_layers_manifest(&manifest, &[Some(0), Some(0)], PageGeometry::Stitch(&geometry), "ch1")
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
        let err = remap_layers_manifest(&manifest, &stitch_map(), PageGeometry::Stitch(&geometry), "ch1")
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
        let out = remap_text_info(&entries, &stitch_map(), PageGeometry::Stitch(&geometry)).expect("remaps");
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

    // -----------------------------------------------------------------------
    // Split.
    // -----------------------------------------------------------------------

    /// Page 1 of 3, a 100x400 page cut in half horizontally, parts in order:
    /// the top part keeps index 1, the bottom one takes index 2.
    fn split_in_half() -> SplitGeometry {
        SplitGeometry::for_tests(
            1,
            SplitAxis::Horizontal,
            [100, 400],
            &[200],
            &[0, 1],
            &[false, false],
        )
    }

    /// Old -> new index map of [`split_in_half`]: page 2 shifts up by one.
    fn split_map() -> Vec<Option<usize>> {
        vec![Some(0), Some(1), Some(3)]
    }

    /// [`split_in_half`] with the BOTTOM part DELETED: exactly one part
    /// survives, so the split degenerates into a crop of page 1.
    fn split_half_bottom_deleted() -> SplitGeometry {
        SplitGeometry::for_tests(
            1,
            SplitAxis::Horizontal,
            [100, 400],
            &[200],
            &[0, 1],
            &[false, true],
        )
    }

    /// Old -> new index map of [`split_half_bottom_deleted`]: nothing shifts.
    fn split_deleted_map() -> Vec<Option<usize>> {
        vec![Some(0), Some(1), Some(2)]
    }

    #[test]
    fn split_routes_a_bubble_by_its_anchor_and_maps_it_into_that_part() {
        let entries = vec![
            json!({
                "id": 1, "img_idx": 1, "img_u": 0.6, "img_v": 0.75, "side": "left",
                "text_areas": [{"rect": [0.0, 0.5, 1.0, 1.0], "anchor": [0.6, 0.75]}],
                "custom": "kept"
            }),
            // An IMAGE bubble whose visible area lies mostly in the TOP part
            // still follows its anchor — the user-fixed rule.
            json!({
                "id": 2, "img_idx": 1, "img_u": 0.2, "img_v": 0.55, "side": "right",
                "bubble_class": "image",
                "rect_coords": {"p1": {"img_u": 0.0, "img_v": 0.0},
                                "p2": {"img_u": 0.4, "img_v": 0.6}}
            }),
        ];
        let geometry = split_in_half();
        let out = remap_bubbles(&entries, &split_map(), PageGeometry::Split(&geometry))
            .expect("remaps");
        assert!(out.changed);
        assert!(
            out.deleted.is_empty(),
            "a split that deletes no part never drops a bubble"
        );

        let bottom = &out.kept[0];
        // Page px y = 0.75 * 400 = 300 -> the bottom part, at index 2.
        assert_eq!(bottom["img_idx"], json!(2));
        // v renormalizes onto the 200 px tall part; u is untouched.
        assert_eq!(bottom["img_v"], json!(0.5));
        assert_eq!(bottom["img_u"], json!(0.6));
        // `side` is re-derived from the mapped anchor.
        assert_eq!(bottom["side"], json!("right"));
        assert_eq!(bottom["text_areas"][0]["rect"], json!([0.0, 0.0, 1.0, 1.0]));
        assert_eq!(bottom["custom"], json!("kept"));

        let image = &out.kept[1];
        // Anchor at page px 220 -> the bottom part, even though its rect is
        // mostly above the cut.
        assert_eq!(image["img_idx"], json!(2));
        // f64 renormalization: the exact bit pattern is not part of the
        // contract, the value is.
        let mapped_v = image["img_v"].as_f64().expect("number");
        assert!((mapped_v - 0.1).abs() < 1e-9, "got {mapped_v}");
        // The body rect follows the bubble; the part above the cut maps to a
        // negative v, which is exactly the "hangs off the edge" case.
        assert_eq!(image["rect_coords"]["p1"]["img_v"], json!(-1.0));
    }

    #[test]
    fn split_archives_bubbles_of_a_deleted_part_instead_of_moving_them() {
        let entries = vec![
            // Anchored at page px y = 300 -> the DELETED bottom part.
            json!({"id": 1, "img_idx": 1, "img_u": 0.6, "img_v": 0.75, "side": "left"}),
            // Anchored at page px y = 100 -> the surviving top part.
            json!({"id": 2, "img_idx": 1, "img_u": 0.6, "img_v": 0.25, "side": "left"}),
            // A bubble on the untouched page 0 that crops the deleted part.
            json!({"id": 3, "img_idx": 0, "img_u": 0.5, "img_v": 0.5,
                   "bubble_class": "image", "image_source_type": "page_crop",
                   "crop_page_idx": 1, "crop_rect": [0.1, 0.6, 0.9, 0.9]}),
        ];
        let geometry = split_half_bottom_deleted();
        let out = remap_bubbles(&entries, &split_deleted_map(), PageGeometry::Split(&geometry))
            .expect("remaps");
        assert!(out.changed);
        // The bubble of the deleted part is ARCHIVED, never re-anchored onto
        // the surviving part.
        assert_eq!(out.deleted.len(), 1);
        assert_eq!(out.deleted[0]["id"], json!(1));
        assert!(out.kept.iter().all(|entry| entry["id"] != json!(1)));
        assert!(
            out.warnings.iter().any(|w| w.contains("deleted split part")),
            "the archived bubble must be reported: {:?}",
            out.warnings
        );

        let survivor = out
            .kept
            .iter()
            .find(|entry| entry["id"] == json!(2))
            .expect("the top part's bubble survives");
        assert_eq!(survivor["img_idx"], json!(1));
        assert_eq!(survivor["img_v"], json!(0.5));

        // The crop link into the deleted part is dropped, not left pointing at
        // a stale index.
        let cropper = out
            .kept
            .iter()
            .find(|entry| entry["id"] == json!(3))
            .expect("the cropping bubble survives on its own page");
        assert!(cropper.get("crop_page_idx").is_none());
        assert!(cropper.get("crop_rect").is_none());
    }

    #[test]
    fn split_remaps_a_page_crop_by_area_and_clamps_the_trim() {
        // A bubble on the untouched page 0 that crops the page being split.
        let straddling = vec![json!({
            "id": 1, "img_idx": 0, "img_u": 0.5, "img_v": 0.5,
            "bubble_class": "image", "image_source_type": "page_crop",
            "crop_page_idx": 1, "crop_rect": [0.1, 0.4, 0.9, 0.7]
        })];
        let geometry = split_in_half();
        let out = remap_bubbles(&straddling, &split_map(), PageGeometry::Split(&geometry))
            .expect("remaps");
        // Page px y 160..280: 40 px above the cut, 80 below -> the bottom part.
        assert_eq!(out.kept[0]["crop_page_idx"], json!(2));
        // 160 maps to -40 of the part and is clamped to its top edge; 280 maps
        // to 80, i.e. v = 0.4. Horizontal bounds are untouched.
        assert_eq!(out.kept[0]["crop_rect"], json!([0.1, 0.0, 0.9, 0.4]));
        assert!(
            out.warnings.iter().any(|w| w.contains("trimmed")),
            "a trimmed crop must be reported: {:?}",
            out.warnings
        );

        // A crop fully inside one part is remapped without any trim.
        let contained = vec![json!({
            "id": 1, "img_idx": 0, "img_u": 0.5, "img_v": 0.5,
            "crop_page_idx": 1, "crop_rect": [0.1, 0.1, 0.9, 0.4]
        })];
        let out = remap_bubbles(&contained, &split_map(), PageGeometry::Split(&geometry))
            .expect("remaps");
        // The top part keeps index 1 and covers page v 0..0.5.
        assert_eq!(out.kept[0]["crop_page_idx"], json!(1));
        assert_eq!(out.kept[0]["crop_rect"], json!([0.1, 0.2, 0.9, 0.8]));
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    }

    #[test]
    fn split_routes_text_info_by_its_mesh_then_by_its_centre() {
        let entries = vec![
            json!({"img_idx": 1, "file": "a.png", "img_x_px": 50.0, "img_y_px": 300.0,
                   "layer_idx": 3}),
            // A deform mesh outranks the stored centre, exactly as the loader's
            // placement decoder does.
            json!({"img_idx": 1, "file": "b.png", "img_x_px": 50.0, "img_y_px": 10.0,
                   "deform_mesh": {"cols": 2, "rows": 2,
                                   "points_px": [[10.0, 300.0], [90.0, 300.0],
                                                 [10.0, 380.0], [90.0, 380.0]]}}),
        ];
        let geometry = split_in_half();
        let out = remap_text_info(&entries, &split_map(), PageGeometry::Split(&geometry))
            .expect("remaps");
        assert_eq!(out.kept[0]["img_idx"], json!(2));
        assert_eq!(out.kept[0]["img_y_px"], json!(100.0));
        // A split never re-bases the text-group axis: the parts are different
        // pages, so their axes cannot collide.
        assert_eq!(out.kept[0]["layer_idx"], json!(3));
        assert_eq!(out.kept[1]["img_idx"], json!(2));
        assert_eq!(out.kept[1]["deform_mesh"]["points_px"][0], json!([10.0, 100.0]));
        // Its own centre moved with the part, into negative page space.
        assert_eq!(out.kept[1]["img_y_px"], json!(-190.0));
    }

    #[test]
    fn split_archives_text_info_of_a_deleted_part_with_its_overlay_file() {
        let entries = vec![
            json!({"img_idx": 1, "file": "gone.png", "img_x_px": 50.0, "img_y_px": 300.0}),
            json!({"img_idx": 1, "file": "kept.png", "img_x_px": 50.0, "img_y_px": 100.0}),
        ];
        let geometry = split_half_bottom_deleted();
        let out =
            remap_text_info(&entries, &split_deleted_map(), PageGeometry::Split(&geometry))
                .expect("remaps");
        assert!(out.changed);
        assert_eq!(out.kept.len(), 1);
        assert_eq!(out.kept[0]["file"], json!("kept.png"));
        assert_eq!(out.kept[0]["img_idx"], json!(1));
        // Archived like an entry of a deleted PAGE, and its overlay PNG is
        // recorded so the plan can trash it instead of orphaning it.
        assert_eq!(out.deleted.len(), 1);
        assert_eq!(out.deleted[0]["file"], json!("gone.png"));
        assert_eq!(out.deleted_files, vec!["gone.png".to_string()]);
        assert!(
            out.warnings.iter().any(|w| w.contains("deleted split part")),
            "the archived overlay must be reported: {:?}",
            out.warnings
        );
    }

    /// A page-1 manifest whose layers are spread across a 100x400 page.
    fn split_manifest() -> Value {
        json!({
            "schema_version": 4,
            "pages": [
                {"img_idx": 1,
                 "groups": [{"uid": "g1", "name": "G", "visible": true, "opacity": 1.0},
                            {"uid": "g2", "name": "H", "visible": true, "opacity": 1.0}],
                 "text_groups": [{"layer_idx": 0, "z": 1, "name": "TG"}],
                 "tree": [
                    {"uid": "top", "name": "Top", "kind": "raster", "z": 0,
                     "visible": true, "opacity": 1.0, "group_uid": "g1",
                     "base_file": "ps_p0001_top.png", "image_size": [40, 40],
                     "transform": {"cx": 50.0, "cy": 50.0, "rotation": 0.0, "scale": 1.0}},
                    {"uid": "text", "name": "T", "kind": "text", "z": 1, "layer_idx": 0,
                     "visible": true, "opacity": 1.0,
                     "rendered_file": "ps_p0001_text.png",
                     "transform": {"cx": 50.0, "cy": 300.0, "rotation": 0.0, "scale": 1.0}},
                    {"uid": "bottom", "name": "Bottom", "kind": "raster", "z": 2,
                     "visible": true, "opacity": 1.0, "group_uid": "g1",
                     "base_file": "ps_p0001_bottom.png", "image_size": [40, 40],
                     "transform": {"cx": 50.0, "cy": 350.0, "rotation": 0.0, "scale": 1.0}}
                 ]}
            ]
        })
    }

    fn text_png_sizes() -> std::collections::BTreeMap<String, [u32; 2]> {
        [("ps_p0001_text.png".to_string(), [20, 20])]
            .into_iter()
            .collect()
    }

    #[test]
    fn split_partitions_layer_pages_with_per_part_z_and_duplicated_groups() {
        let manifest = split_manifest();
        let geometry = split_in_half();
        let (routing, warnings) =
            split_layer_routing(Some(&manifest), &text_png_sizes(), &geometry)
                .expect("routes");
        assert!(warnings.is_empty(), "{warnings:?}");
        // The layer PNGs of ONE page fan out onto DIFFERENT new pages.
        assert_eq!(routing.node_part("top"), Some(0));
        assert_eq!(routing.node_part("bottom"), Some(1));
        assert_eq!(routing.node_part("text"), Some(1));
        assert_eq!(routing.file_new_idx("ps_p0001_top.png"), Some(1));
        assert_eq!(routing.file_new_idx("ps_p0001_bottom.png"), Some(2));
        assert_eq!(routing.file_new_idx("ps_p0001_text.png"), Some(2));

        let geometry = geometry.with_routing("ch1", routing);
        let out = remap_layers_manifest(
            &manifest,
            &split_map(),
            PageGeometry::Split(&geometry),
            "ch1",
        )
        .expect("remaps");
        assert!(out.changed);
        assert!(out.deleted_pages.is_empty(), "a split never drops a page entry");
        let pages = out.manifest["pages"].as_array().expect("pages");
        assert_eq!(pages.len(), 2, "one entry per part holding a layer");

        let top = &pages[0];
        assert_eq!(top["img_idx"], json!(1));
        assert_eq!(top["tree"].as_array().expect("tree").len(), 1);
        assert_eq!(top["tree"][0]["uid"], json!("top"));
        assert_eq!(top["tree"][0]["transform"]["cy"], json!(50.0));
        // The PNG reference follows the part, not the page index map.
        assert_eq!(top["tree"][0]["base_file"], json!("ps_p0001_top.png"));
        // Only the group this part's node belongs to survives here...
        let groups = top["groups"].as_array().expect("groups");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["uid"], json!("g1"));
        // ...and the empty text-group list is omitted, as the writer does.
        assert!(top.get("text_groups").is_none());

        let bottom = &pages[1];
        assert_eq!(bottom["img_idx"], json!(2));
        assert_eq!(bottom["tree"].as_array().expect("tree").len(), 2);
        // Geometry mapped into the part: page px 300 is part px 100.
        assert_eq!(bottom["tree"][0]["transform"]["cy"], json!(100.0));
        assert_eq!(bottom["tree"][1]["transform"]["cy"], json!(150.0));
        assert_eq!(bottom["tree"][1]["base_file"], json!("ps_p0002_bottom.png"));
        // `z` is a per-PAGE band axis, so each part is re-ranked densely from
        // 0; the text node and its band keep sharing a band.
        assert_eq!(bottom["tree"][0]["z"], json!(0));
        assert_eq!(bottom["tree"][1]["z"], json!(1));
        assert_eq!(bottom["text_groups"][0]["z"], json!(0));
        // `g1` is DUPLICATED into both parts (group uids are page-scoped);
        // `g2`, which no node belongs to, appears in neither.
        assert_eq!(bottom["groups"].as_array().expect("groups").len(), 1);
        assert_eq!(bottom["groups"][0]["uid"], json!("g1"));
    }

    #[test]
    fn split_drops_layer_records_of_a_deleted_part_and_marks_their_pngs() {
        let manifest = split_manifest();
        let geometry = split_half_bottom_deleted();
        let (routing, warnings) =
            split_layer_routing(Some(&manifest), &text_png_sizes(), &geometry)
                .expect("routes");
        assert!(warnings.is_empty(), "{warnings:?}");
        // The records are still ROUTED to the deleted part — that is what tells
        // them apart from records the pass could not route at all.
        assert_eq!(routing.node_part("bottom"), Some(1));
        assert_eq!(routing.node_part("text"), Some(1));
        // ...but they get no destination page, and their PNGs are marked for
        // the trash instead of being renamed onto the surviving part.
        assert_eq!(routing.file_new_idx("ps_p0001_bottom.png"), None);
        assert_eq!(routing.file_new_idx("ps_p0001_text.png"), None);
        assert!(routing.is_deleted_file("ps_p0001_bottom.png"));
        assert!(routing.is_deleted_file("ps_p0001_text.png"));
        assert_eq!(routing.file_new_idx("ps_p0001_top.png"), Some(1));
        assert!(!routing.is_deleted_file("ps_p0001_top.png"));

        let geometry = geometry.with_routing("ch1", routing);
        let out = remap_layers_manifest(
            &manifest,
            &split_deleted_map(),
            PageGeometry::Split(&geometry),
            "ch1",
        )
        .expect("remaps");
        assert!(out.changed);
        let pages = out.manifest["pages"].as_array().expect("pages");
        assert_eq!(pages.len(), 1, "only the surviving part keeps an entry");
        assert_eq!(pages[0]["img_idx"], json!(1));
        let tree = pages[0]["tree"].as_array().expect("tree");
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0]["uid"], json!("top"));
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("deleted split part 1")),
            "discarded layer records must be reported: {:?}",
            out.warnings
        );
        // The deleted part's records are ARCHIVED, not merely dropped: their
        // PNGs go to the trash, but the transform, the group membership and a
        // text node's typed text exist nowhere else.
        assert_eq!(out.deleted_pages.len(), 1, "{:?}", out.deleted_pages);
        let archived = &out.deleted_pages[0];
        // The page's ORIGINAL index, as for a page deleted whole.
        assert_eq!(archived["img_idx"], json!(1));
        let uids: Vec<&str> = archived["tree"]
            .as_array()
            .expect("tree")
            .iter()
            .filter_map(|rec| rec["uid"].as_str())
            .collect();
        assert_eq!(uids, vec!["text", "bottom"], "{archived:?}");
        // Verbatim, in the page's own pre-operation coordinate space.
        approx(&archived["tree"][1]["transform"]["cy"], 350.0);
        // Only the groups the dropped records CLAIM travel with them: "bottom"
        // carries `group_uid: g1`, while g2 is claimed by nobody and stays out.
        let groups: Vec<&str> = archived["groups"]
            .as_array()
            .expect("groups")
            .iter()
            .filter_map(|group| group["uid"].as_str())
            .collect();
        assert_eq!(groups, vec!["g1"], "{archived:?}");
        // The dropped TEXT node owns text band `layer_idx: 0`, whose name lives
        // only in the band record — so that record is archived with it.
        assert_eq!(archived["text_groups"][0]["name"], json!("TG"), "{archived:?}");
    }

    #[test]
    fn split_assigns_layers_by_exact_area_not_by_the_unrotated_footprint() {
        // Unequal parts: 0..300 and 300..400, so a rotation really changes the
        // answer instead of cancelling out.
        let geometry =
            SplitGeometry::for_tests(
                1,
                SplitAxis::Horizontal,
                [50, 400],
                &[300],
                &[0, 1],
                &[false, false],
            );
        let node = |rotation: f64| {
            json!({"pages": [{"img_idx": 1, "tree": [
                {"uid": "wide", "kind": "raster", "z": 0, "visible": true, "opacity": 1.0,
                 "image_size": [400, 20],
                 "transform": {"cx": 25.0, "cy": 310.0, "rotation": rotation, "scale": 1.0}}
            ]}]})
        };
        // Flat: a 400x20 band lying entirely inside the bottom part.
        let flat = node(0.0);
        let (routing, _) =
            split_layer_routing(Some(&flat), &Default::default(), &geometry).expect("routes");
        assert_eq!(routing.node_part("wide"), Some(1));
        // Upright: the same layer rotated a quarter turn now spans page px
        // 110..510, and most of the part of it that is ON the page lies above
        // the cut. A bounding box of the UNROTATED image would still say
        // "bottom".
        let upright = node(std::f64::consts::FRAC_PI_2);
        let (routing, _) = split_layer_routing(Some(&upright), &Default::default(), &geometry)
            .expect("routes");
        assert_eq!(routing.node_part("wide"), Some(0));
    }

    #[test]
    fn split_layer_assignment_honours_deform_ties_and_the_probe_fallback() {
        let geometry = split_in_half();
        let manifest = json!({"pages": [{"img_idx": 1, "tree": [
            // A deform mesh OVERRIDES the transform: the transform alone would
            // put this node in the top part.
            {"uid": "deformed", "kind": "raster", "z": 0, "visible": true, "opacity": 1.0,
             "image_size": [40, 40],
             "transform": {"cx": 50.0, "cy": 50.0, "rotation": 0.0, "scale": 1.0},
             "deform": {"cols": 2, "rows": 2,
                        "points_px": [[10.0, 320.0], [90.0, 320.0],
                                      [10.0, 380.0], [90.0, 380.0]]}},
            // Exactly half on each side of the cut: the TOP part wins.
            {"uid": "tied", "kind": "raster", "z": 1, "visible": true, "opacity": 1.0,
             "image_size": [40, 100],
             "transform": {"cx": 50.0, "cy": 200.0, "rotation": 0.0, "scale": 1.0}},
            // A text render whose PNG could not be probed: no size, so no area.
            {"uid": "unprobed", "kind": "text", "z": 2, "visible": true, "opacity": 1.0,
             "rendered_file": "ps_p0001_missing.png",
             "transform": {"cx": 50.0, "cy": 260.0, "rotation": 0.0, "scale": 1.0}},
            // No placement at all.
            {"uid": "placeless", "kind": "raster", "z": 3, "visible": true, "opacity": 1.0}
        ]}]});
        let (routing, warnings) =
            split_layer_routing(Some(&manifest), &Default::default(), &geometry)
                .expect("routes");
        assert_eq!(routing.node_part("deformed"), Some(1));
        assert_eq!(routing.node_part("tied"), Some(0));
        // Degraded to its centre point (page px 260 -> the bottom part).
        assert_eq!(routing.node_part("unprobed"), Some(1));
        assert_eq!(routing.node_part("placeless"), Some(0));
        assert!(
            warnings.iter().any(|w| w.contains("'unprobed'")),
            "the probe failure must be reported: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("'placeless'")),
            "a placeless layer must be reported: {warnings:?}"
        );
    }

    #[test]
    fn split_detection_blocks_go_to_the_part_that_holds_them() {
        let geometry = split_in_half();
        let document = json!({
            "page_idx": 1,
            "source_size": [100, 400],
            "mask_size": [100, 400],
            "blocks": [
                {"x1": 1.0, "y1": 2.0, "x2": 3.0, "y2": 40.0, "text": "a"},
                {"x1": 5.0, "y1": 300.0, "x2": 9.0, "y2": 380.0},
                // Straddles the cut, 3/4 below it.
                {"x1": 5.0, "y1": 190.0, "x2": 9.0, "y2": 220.0}
            ],
            "custom": "kept"
        });
        let top = split_detection_blocks(&document, &geometry, 0, 1, Some("00001_mask.png"))
            .expect("cuts");
        assert_eq!(top["page_idx"], json!(1));
        assert_eq!(top["source_size"], json!([100, 200]));
        assert_eq!(top["mask_size"], json!([100, 200]));
        assert_eq!(top["mask_file"], json!("00001_mask.png"));
        assert_eq!(top["custom"], json!("kept"));
        let blocks = top["blocks"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["text"], json!("a"));

        let bottom = split_detection_blocks(&document, &geometry, 1, 2, None).expect("cuts");
        let blocks = bottom["blocks"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 2);
        // Absolute page px translate into the part's own space.
        assert_eq!(blocks[0]["y1"], json!(100.0));
        assert_eq!(blocks[1]["y1"], json!(-10.0));
        // No mask for this part: the writer's empty reference, not a stale one.
        assert_eq!(bottom["mask_file"], json!(""));
    }

    /// A page-1 manifest whose two layers sit on OPPOSITE sides of the cut but
    /// name the SAME layer PNG in `key` (`base_file` or `rendered_file`).
    fn shared_layer_png_manifest(key: &str) -> Value {
        let mut manifest = json!({"pages": [{"img_idx": 1, "tree": [
            {"uid": "above", "kind": "raster", "z": 0, "visible": true, "opacity": 1.0,
             "image_size": [40, 40],
             "transform": {"cx": 50.0, "cy": 50.0, "rotation": 0.0, "scale": 1.0}},
            {"uid": "below", "kind": "raster", "z": 1, "visible": true, "opacity": 1.0,
             "image_size": [40, 40],
             "transform": {"cx": 50.0, "cy": 350.0, "rotation": 0.0, "scale": 1.0}}
        ]}]});
        for rec in 0..2 {
            manifest["pages"][0]["tree"][rec][key] = json!("ps_p0001_shared.png");
        }
        manifest
    }

    #[test]
    fn split_refuses_one_layer_png_claimed_by_records_of_different_parts() {
        let geometry = split_in_half();
        for key in ["base_file", "rendered_file"] {
            let manifest = shared_layer_png_manifest(key);
            let Err(PageOpError::InvalidOp(message)) =
                split_layer_routing(Some(&manifest), &Default::default(), &geometry)
            else {
                panic!("a {key} claimed by two parts must be refused, not silently routed");
            };
            assert!(message.contains("ps_p0001_shared.png"), "{message}");
            // The two conflicting parts are named, so the user can find them.
            assert!(message.contains('0') && message.contains('1'), "{message}");
        }
    }

    #[test]
    fn split_accepts_one_layer_png_shared_inside_a_single_part() {
        let geometry = split_in_half();
        let mut manifest = shared_layer_png_manifest("base_file");
        // Move the second record above the cut: one file, one destination.
        manifest["pages"][0]["tree"][1]["transform"]["cy"] = json!(60.0);
        let (routing, _) = split_layer_routing(Some(&manifest), &Default::default(), &geometry)
            .expect("one part claims the file");
        assert_eq!(routing.file_new_idx("ps_p0001_shared.png"), Some(1));
    }

    #[test]
    fn split_routes_a_folded_deform_mesh_by_its_cells_not_by_its_ring() {
        let geometry = split_in_half();
        // A 3x2 grid folded back on itself: the two cells cover the SAME
        // rectangle in the bottom part with opposite winding, so the outer
        // boundary ring encloses exactly zero signed area while the filled
        // area is 2 x 80x100 px, all of it below the cut.
        let manifest = json!({"pages": [{"img_idx": 1, "tree": [
            {"uid": "folded", "kind": "raster", "z": 0, "visible": true, "opacity": 1.0,
             "image_size": [40, 40],
             "transform": {"cx": 50.0, "cy": 20.0, "rotation": 0.0, "scale": 1.0},
             "deform": {"cols": 3, "rows": 2,
                        "points_px": [[0.0, 250.0], [100.0, 250.0], [0.0, 250.0],
                                      [0.0, 350.0], [100.0, 350.0], [0.0, 350.0]]}}
        ]}]});
        let (routing, warnings) =
            split_layer_routing(Some(&manifest), &Default::default(), &geometry)
                .expect("routes");
        // A ring-based area would cancel to 0 and fall back to the transform
        // quad, which sits at page px 0..40 — the TOP part. The cell sum keeps
        // the mesh's real footprint and answers with the bottom part.
        assert_eq!(routing.node_part("folded"), Some(1));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn split_detection_refuses_a_block_it_cannot_route() {
        let geometry = split_in_half();
        let document = |blocks: Value| {
            json!({"page_idx": 1, "source_size": [100, 400], "mask_size": [100, 400],
                   "blocks": blocks})
        };
        let good = json!({"x1": 1.0, "y1": 2.0, "x2": 3.0, "y2": 40.0});
        // A well-formed document passes the gate untouched.
        assert!(detection_rect_blocker(&document(json!([good.clone()])), 1).is_none());

        // A non-object element: every part would skip it, so it blocks the group.
        let non_object = document(json!([good.clone(), "legacy"]));
        let reason = detection_rect_blocker(&non_object, 1).expect("blocked");
        assert!(reason.contains("blocks[1]"), "{reason}");
        assert!(
            split_detection_blocks(&non_object, &geometry, 0, 1, None).is_err(),
            "a malformed block must fail the cut, never be dropped from every part"
        );

        // An object missing one coordinate: same all-or-nothing decision.
        let missing = document(json!([{"x1": 1.0, "y1": 2.0, "x2": 3.0}]));
        let reason = detection_rect_blocker(&missing, 1).expect("blocked");
        assert!(reason.contains("y2"), "{reason}");
        assert!(split_detection_blocks(&missing, &geometry, 1, 2, None).is_err());
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

    /// Asserts a mapped JSON number equals `expected` within float tolerance.
    fn approx(value: &Value, expected: f64) {
        let got = read_f64(value)
            .unwrap_or_else(|| panic!("expected a number, got {value}"));
        assert!(
            (got - expected).abs() < 1e-9,
            "expected {expected}, got {got}"
        );
    }

    // -----------------------------------------------------------------------
    // Crop: page 1 (100x400) turned 90 CW onto a 400x100 canvas, of which the
    // LEFT half is kept, i.e. a 200x100 page. Under that turn the page's
    // BOTTOM half (y 200..400) becomes the canvas' LEFT half and survives,
    // while its top half is cropped away.
    // -----------------------------------------------------------------------

    /// The crop described above.
    fn crop_cw90_left_half() -> CropGeometry {
        CropGeometry::for_tests(1, [100, 400], 1, 0.0, [0, 0, 200, 100])
    }

    /// A crop with no rotation at all: page 1's left column is kept.
    fn crop_left_column() -> CropGeometry {
        CropGeometry::for_tests(1, [100, 400], 0, 0.0, [0, 0, 40, 400])
    }

    /// Old -> new index map of every crop: the identity over 3 pages.
    fn crop_map() -> Vec<Option<usize>> {
        vec![Some(0), Some(1), Some(2)]
    }

    #[test]
    fn crop_keeps_an_overlapping_bubble_and_archives_one_outside_the_frame() {
        let geo = crop_left_column();
        let entries = vec![
            // Straddles the right edge of the kept column (u 0.4 == x 40).
            json!({"id": 1, "img_idx": 1, "img_u": 0.35, "img_v": 0.5, "side": "left",
                   "rect_coords": {"p1": {"img_u": 0.3, "img_v": 0.4},
                                   "p2": {"img_u": 0.5, "img_v": 0.6}}}),
            // Entirely to the right of the kept column.
            json!({"id": 2, "img_idx": 1, "img_u": 0.8, "img_v": 0.5, "side": "right",
                   "rect_coords": {"p1": {"img_u": 0.7, "img_v": 0.4},
                                   "p2": {"img_u": 0.9, "img_v": 0.6}}}),
            // Another page: untouched by a crop.
            json!({"id": 3, "img_idx": 0, "img_u": 0.9, "img_v": 0.9, "side": "right"}),
        ];
        let out = remap_bubbles(&entries, &crop_map(), PageGeometry::Crop(&geo))
            .expect("remaps");
        assert!(out.changed);
        assert_eq!(out.deleted.len(), 1, "{:?}", out.deleted);
        assert_eq!(out.deleted[0]["id"], json!(2));
        assert!(
            out.warnings.iter().any(|w| w.contains("entirely outside")),
            "archiving must be reported: {:?}",
            out.warnings
        );
        assert_eq!(out.kept.len(), 2);
        // The survivor keeps its page and is renormalized onto the 40x400 page:
        // u 0.35 of 100 px = 35 px = u 0.875 of 40 px.
        assert_eq!(out.kept[0]["img_idx"], json!(1));
        approx(&out.kept[0]["img_u"], 0.875);
        approx(&out.kept[0]["img_v"], 0.5);
        // Its box legitimately reaches past the new page's right edge.
        approx(&out.kept[0]["rect_coords"]["p2"]["img_u"], 1.25);
        // The other page's bubble is untouched.
        assert_eq!(out.kept[1]["img_idx"], json!(0));
        approx(&out.kept[1]["img_u"], 0.9);
    }

    #[test]
    fn crop_bubble_without_a_box_falls_back_to_its_anchor() {
        let geo = crop_left_column();
        let entries = vec![
            json!({"id": 1, "img_idx": 1, "img_u": 0.2, "img_v": 0.5}),
            json!({"id": 2, "img_idx": 1, "img_u": 0.9, "img_v": 0.5}),
        ];
        let out = remap_bubbles(&entries, &crop_map(), PageGeometry::Crop(&geo))
            .expect("remaps");
        assert_eq!(out.kept.len(), 1);
        assert_eq!(out.kept[0]["id"], json!(1));
        assert_eq!(out.deleted.len(), 1);
        assert_eq!(out.deleted[0]["id"], json!(2));
    }

    #[test]
    fn crop_maps_a_page_crop_bubbles_rect_and_drops_a_link_it_removed() {
        let geo = crop_left_column();
        let entries = vec![
            // Crops the left half of page 1: survives, trimmed to the new frame.
            json!({"id": 1, "img_idx": 0, "img_u": 0.5, "img_v": 0.5, "side": "left",
                   "bubble_class": "image", "image_source_type": "page_crop",
                   "crop_page_idx": 1, "crop_rect": [0.0, 0.25, 0.6, 0.75]}),
            // Crops a region the crop removed entirely.
            json!({"id": 2, "img_idx": 0, "img_u": 0.5, "img_v": 0.5, "side": "left",
                   "bubble_class": "image", "image_source_type": "page_crop",
                   "crop_page_idx": 1, "crop_rect": [0.7, 0.1, 0.9, 0.2]}),
        ];
        let out = remap_bubbles(&entries, &crop_map(), PageGeometry::Crop(&geo))
            .expect("remaps");
        // The page index never changes: a crop keeps the page it cropped.
        assert_eq!(out.kept[0]["crop_page_idx"], json!(1));
        let rect = out.kept[0]["crop_rect"].as_array().expect("rect");
        // u 0.0..0.6 of the 100 px page is 0..60 px, clamped to the 40 px page.
        approx(&rect[0], 0.0);
        approx(&rect[1], 0.25);
        approx(&rect[2], 1.0);
        approx(&rect[3], 0.75);
        assert!(
            out.warnings.iter().any(|w| w.contains("trimmed")),
            "the trim must be reported: {:?}",
            out.warnings
        );
        // Nothing of the second bubble's crop survived: the link is removed
        // rather than left pointing at an unrelated region.
        assert!(out.kept[1].get("crop_page_idx").is_none());
        assert!(out.kept[1].get("crop_rect").is_none());
    }

    #[test]
    fn crop_adds_the_page_angle_to_every_stored_angle() {
        let geo = crop_cw90_left_half();
        let manifest = json!({
            "schema_version": 4,
            "pages": [{"img_idx": 1, "tree": [
                {"uid": "a", "name": "A", "kind": "raster", "z": 0, "visible": true,
                 "opacity": 1.0, "image_size": [20, 20],
                 "transform": {"cx": 50.0, "cy": 300.0, "rotation": 0.25, "scale": 2.0}},
                {"uid": "b", "name": "B", "kind": "raster", "z": 1, "visible": true,
                 "opacity": 1.0, "image_size": [20, 20],
                 "transform": {"cx": 50.0, "cy": 320.0, "scale": 1.0}}
            ]}]
        });
        let out = remap_layers_manifest(&manifest, &crop_map(), PageGeometry::Crop(&geo), "ch1")
            .expect("remaps");
        let tree = out.manifest["pages"][0]["tree"].as_array().expect("tree");
        // A quarter turn is +pi/2 radians on top of whatever was stored.
        let quarter = std::f64::consts::FRAC_PI_2;
        approx(&tree[0]["transform"]["rotation"], 0.25 + quarter);
        // A record with NO stored angle gains one: zero is no longer correct.
        approx(&tree[1]["transform"]["rotation"], quarter);
        // A magnitude is not a coordinate and a rotation does not touch it.
        approx(&tree[0]["transform"]["scale"], 2.0);
        // Page (50, 300) -> canvas (400 - 300, 50) = (100, 50) -> new page
        // (100, 50): the crop keeps the canvas origin.
        approx(&tree[0]["transform"]["cx"], 100.0);
        approx(&tree[0]["transform"]["cy"], 50.0);

        // The same rule in DEGREES for the legacy typing document.
        let entries = vec![
            json!({"img_idx": 1, "file": "ov.png", "img_x_px": 50.0, "img_y_px": 300.0,
                   "rotation_deg": 10.0, "scale": 1.5}),
            json!({"img_idx": 1, "file": "ov2.png", "img_x_px": 50.0, "img_y_px": 320.0}),
        ];
        let out = remap_text_info(&entries, &crop_map(), PageGeometry::Crop(&geo))
            .expect("remaps");
        assert_eq!(out.kept.len(), 2, "{:?}", out.deleted);
        approx(&out.kept[0]["rotation_deg"], 100.0);
        approx(&out.kept[1]["rotation_deg"], 90.0);
        approx(&out.kept[0]["scale"], 1.5);
        approx(&out.kept[0]["img_x_px"], 100.0);
        approx(&out.kept[0]["img_y_px"], 50.0);
    }

    #[test]
    fn a_stored_angle_wraps_instead_of_accumulating_past_a_full_turn() {
        // These operations COMPOSE: without wrapping, four quarter-turn crops
        // would store a full turn where zero means the same thing, and the value
        // is surfaced to the user as a degree readout.
        let geo = crop_cw90_left_half();
        let manifest = json!({
            "pages": [{"img_idx": 1, "tree": [
                // 3.0 rad + pi/2 = 4.571, past pi: wraps into [-pi, pi).
                {"uid": "a", "name": "A", "kind": "raster", "z": 0, "visible": true,
                 "opacity": 1.0, "image_size": [20, 20],
                 "transform": {"cx": 50.0, "cy": 300.0, "rotation": 3.0, "scale": 1.0}}
            ]}]
        });
        let out = remap_layers_manifest(&manifest, &crop_map(), PageGeometry::Crop(&geo), "ch1")
            .expect("remaps");
        let stored = read_f64(&out.manifest["pages"][0]["tree"][0]["transform"]["rotation"])
            .expect("a number");
        approx(
            &out.manifest["pages"][0]["tree"][0]["transform"]["rotation"],
            3.0 + std::f64::consts::FRAC_PI_2 - std::f64::consts::TAU,
        );
        assert!(
            (-std::f64::consts::PI..std::f64::consts::PI).contains(&stored),
            "radians must land in [-pi, pi), got {stored}"
        );

        // Degrees follow the SAME convention as the typing tab's own
        // `normalize_angle_deg`: [-180, 180).
        let entries = vec![json!({
            "img_idx": 1, "file": "ov.png", "img_x_px": 50.0, "img_y_px": 300.0,
            "rotation_deg": 100.0
        })];
        let out = remap_text_info(&entries, &crop_map(), PageGeometry::Crop(&geo))
            .expect("remaps");
        // 100 + 90 = 190 -> -170, not 190.
        approx(&out.kept[0]["rotation_deg"], -170.0);
    }

    #[test]
    fn a_non_rotating_placement_still_leaves_every_stored_angle_alone() {
        // The pixel-identity guarantee of a stitch and a split: adding a zero
        // angle must not even rewrite the field.
        let geo = split_in_half();
        let manifest = json!({
            "schema_version": 4,
            "pages": [{"img_idx": 1, "tree": [
                {"uid": "a", "name": "A", "kind": "raster", "z": 0, "visible": true,
                 "opacity": 1.0, "image_size": [20, 20],
                 "transform": {"cx": 50.0, "cy": 50.0, "rotation": 0.25, "scale": 1.0}},
                {"uid": "b", "name": "B", "kind": "raster", "z": 1, "visible": true,
                 "opacity": 1.0, "image_size": [20, 20],
                 "transform": {"cx": 50.0, "cy": 60.0, "scale": 1.0}}
            ]}]
        });
        let out = remap_layers_manifest(&manifest, &split_map(), PageGeometry::Split(&geo), "ch1")
            .expect("remaps");
        let tree = out.manifest["pages"][0]["tree"].as_array().expect("tree");
        assert_eq!(tree[0]["transform"]["rotation"], json!(0.25));
        assert!(
            tree[1]["transform"].get("rotation").is_none(),
            "a zero page angle must not invent a stored angle"
        );
    }

    #[test]
    fn crop_drops_a_layer_outside_the_frame_and_orphans_only_its_own_png() {
        let geo = crop_left_column();
        let manifest = json!({
            "schema_version": 4,
            "pages": [{"img_idx": 1,
                "groups": [{"uid": "g1", "name": "G", "visible": true, "opacity": 1.0}],
                "tree": [
                    // Inside the kept 40 px column.
                    {"uid": "in", "name": "I", "kind": "raster", "z": 0, "visible": true,
                     "opacity": 1.0, "group_uid": "g1", "base_file": "ps_p0001_in.png",
                     "image_size": [20, 20],
                     "transform": {"cx": 20.0, "cy": 100.0, "rotation": 0.0, "scale": 1.0}},
                    // Entirely to the right of it.
                    {"uid": "out", "name": "O", "kind": "raster", "z": 1, "visible": true,
                     "opacity": 1.0, "base_file": "ps_p0001_out.png",
                     "image_size": [20, 20],
                     "transform": {"cx": 80.0, "cy": 100.0, "rotation": 0.0, "scale": 1.0}}
                ]}]
        });
        let sizes = std::collections::BTreeMap::new();
        let (routing, warnings) = crop_layer_routing(Some(&manifest), &sizes, &geo);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!routing.is_dropped_node("in"));
        assert!(routing.is_dropped_node("out"));
        assert!(routing.is_dropped_file("ps_p0001_out.png"));
        assert!(!routing.is_dropped_file("ps_p0001_in.png"));

        let geo = geo.with_routing("ch1", routing);
        let mut out =
            remap_layers_manifest(&manifest, &crop_map(), PageGeometry::Crop(&geo), "ch1")
                .expect("remaps");
        assert!(out.changed);
        let pages = out.manifest["pages"].as_array().expect("pages");
        // ONE entry: the page survives, only records were dropped.
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0]["img_idx"], json!(1));
        let tree = pages[0]["tree"].as_array().expect("tree");
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0]["uid"], json!("in"));
        // The band axis is re-ranked densely after the drop.
        assert_eq!(tree[0]["z"], json!(0));
        // The group keeps its only surviving member.
        assert_eq!(pages[0]["groups"][0]["uid"], json!("g1"));
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("entirely outside the kept region")),
            "the drop must be reported: {:?}",
            out.warnings
        );
        // Same archive as a deleted split part: the dropped record's metadata
        // survives in the trash, not only its PNG.
        assert_eq!(out.deleted_pages.len(), 1, "{:?}", out.deleted_pages);
        let archived = &out.deleted_pages[0];
        assert_eq!(archived["img_idx"], json!(1));
        let uids: Vec<&str> = archived["tree"]
            .as_array()
            .expect("tree")
            .iter()
            .filter_map(|rec| rec["uid"].as_str())
            .collect();
        assert_eq!(uids, vec!["out"], "{archived:?}");
        approx(&archived["tree"][0]["transform"]["cx"], 80.0);
        // The dropped record claimed no group, so none is carried over.
        assert!(archived.get("groups").is_none(), "{archived:?}");
        out.warnings.clear();
    }

    #[test]
    fn a_crop_that_removes_nothing_writes_no_layer_archive() {
        // The archive must stay empty when the frame kept every record, so a
        // manual recovery never has to guess whether a file means anything.
        let geo = crop_left_column();
        let manifest = json!({
            "pages": [{"img_idx": 1, "tree": [
                {"uid": "in", "name": "I", "kind": "raster", "z": 0, "visible": true,
                 "opacity": 1.0, "image_size": [20, 20],
                 "transform": {"cx": 20.0, "cy": 100.0, "rotation": 0.0, "scale": 1.0}}
            ]}]
        });
        let sizes = std::collections::BTreeMap::new();
        let (routing, _) = crop_layer_routing(Some(&manifest), &sizes, &geo);
        let geo = geo.with_routing("ch1", routing);
        let out = remap_layers_manifest(&manifest, &crop_map(), PageGeometry::Crop(&geo), "ch1")
            .expect("remaps");
        assert!(out.deleted_pages.is_empty(), "{:?}", out.deleted_pages);
    }

    #[test]
    fn crop_keeps_a_layer_it_cannot_measure_and_says_so() {
        let geo = crop_left_column();
        let manifest = json!({
            "pages": [{"img_idx": 1, "tree": [
                // No transform and no mesh: nothing to judge, so it stays.
                {"uid": "blind", "name": "B", "kind": "raster", "z": 0,
                 "visible": true, "opacity": 1.0},
                // A TEXT node whose render could not be probed: judged by its
                // centre point instead of by area, with a warning.
                {"uid": "text", "name": "T", "kind": "text", "z": 1,
                 "visible": true, "opacity": 1.0,
                 "rendered_file": "ps_p0001_text.png",
                 "transform": {"cx": 20.0, "cy": 100.0, "rotation": 0.0, "scale": 1.0}}
            ]}]
        });
        let sizes = std::collections::BTreeMap::new();
        let (routing, warnings) = crop_layer_routing(Some(&manifest), &sizes, &geo);
        assert!(!routing.is_dropped_node("blind"));
        assert!(!routing.is_dropped_node("text"));
        assert!(
            warnings.iter().any(|w| w.contains("no transform")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("centre point")),
            "{warnings:?}"
        );
    }

    #[test]
    fn crop_archives_a_text_info_entry_outside_the_frame_with_its_png() {
        let geo = crop_left_column();
        let entries = vec![
            json!({"img_idx": 1, "file": "in.png", "img_x_px": 20.0, "img_y_px": 100.0}),
            json!({"img_idx": 1, "file": "out.png", "img_x_px": 80.0, "img_y_px": 100.0}),
        ];
        let out = remap_text_info(&entries, &crop_map(), PageGeometry::Crop(&geo))
            .expect("remaps");
        assert_eq!(out.kept.len(), 1);
        assert_eq!(out.kept[0]["file"], json!("in.png"));
        assert_eq!(out.deleted.len(), 1);
        assert_eq!(out.deleted[0]["file"], json!("out.png"));
        assert_eq!(out.deleted_files, vec!["out.png".to_string()]);
        assert!(
            out.warnings.iter().any(|w| w.contains("entirely outside")),
            "{:?}",
            out.warnings
        );
    }

    #[test]
    fn crop_detection_maps_blocks_exactly_under_a_quarter_turn() {
        let geo = crop_cw90_left_half();
        let document = json!({
            "page_idx": 1,
            "source_size": [100, 400],
            "mask_size": [100, 400],
            // In the page's BOTTOM half, which the turn moves into the kept
            // LEFT half of the canvas.
            "blocks": [
                {"x1": 10.0, "y1": 300.0, "x2": 30.0, "y2": 340.0, "text": "kept"},
                {"x1": 10.0, "y1": 20.0, "x2": 30.0, "y2": 60.0, "text": "gone"}
            ],
            "mask_file": "00001_mask.png"
        });
        assert!(detection_rect_blocker(&document, 1).is_none());
        let (out, dropped) =
            crop_detection_blocks(&document, &geo, 1, Some("00001_mask.png")).expect("maps");
        assert_eq!(dropped, 1);
        let blocks = out["blocks"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["text"], json!("kept"));
        // (x, y) -> (400 - y, x): the rectangle stays an exact rectangle.
        approx(&blocks[0]["x1"], 60.0);
        approx(&blocks[0]["y1"], 10.0);
        approx(&blocks[0]["x2"], 100.0);
        approx(&blocks[0]["y2"], 30.0);
        assert_eq!(out["source_size"], json!([200, 100]));
        assert_eq!(out["mask_size"], json!([200, 100]));
        assert_eq!(out["page_idx"], json!(1));
    }
}
