/*
File: crates/ms-tab-ps-editor/src/raster_ops.rs

Purpose:
Pure raster and record helpers of the tab: brush key parsing, layer sampling, clip/clear on
selections, transform record conversion, region blending and the overlay pointer gate.

Key functions:
- `collect_brush_key_actions()`, `sample_layer_world()`, `raster_below_by_band_z()`,
  `page_footprint_rect()`.
- `clip_into_new_layer()`, `clear_selected_pixels()`, `layer_touches_selection()`.
- `transform_to_rec()`, `rec_to_transform()`, `layer_to_raster_node()`, `pixel_region_edit_op()`.
- `blend_premul_region()`, `lerp_premul()`, `page_pixel_index()`, `overlay_pointer()`,
  `non_empty_selection()`.

Notes:
Free functions only; no egui context, no I/O.
A child of the crate root (`lib.rs`); reaches the root's types, constants and imports through
`use super::*`.
*/

use super::*;

/// Decodes this frame's key events into brush parameter changes, in event order.
///
/// The frame's EVENTS are scanned rather than `InputState::key_pressed`, because the brush needs
/// `[` and `Shift+[` to mean different things: `key_pressed` ignores modifiers entirely, and
/// `consume_key`/`consume_shortcut` match with `matches_logically`, which ignores an extra Shift.
/// Only `Modifiers::matches_exact` separates the two. Key repeats are accepted, so holding `]`
/// keeps growing the brush.
pub(crate) fn collect_brush_key_actions(events: &[egui::Event]) -> Vec<BrushKeyAction> {
    let mut out = Vec::new();
    for event in events {
        let egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } = event
        else {
            continue;
        };
        let plain = modifiers.matches_exact(egui::Modifiers::NONE);
        let shifted = modifiers.matches_exact(egui::Modifiers::SHIFT);
        if !plain && !shifted {
            continue;
        }
        let digit = match key {
            egui::Key::Num1 => Some(0.1),
            egui::Key::Num2 => Some(0.2),
            egui::Key::Num3 => Some(0.3),
            egui::Key::Num4 => Some(0.4),
            egui::Key::Num5 => Some(0.5),
            egui::Key::Num6 => Some(0.6),
            egui::Key::Num7 => Some(0.7),
            egui::Key::Num8 => Some(0.8),
            egui::Key::Num9 => Some(0.9),
            egui::Key::Num0 => Some(1.0),
            _ => None,
        };
        if let Some(fraction) = digit {
            out.push(if shifted {
                BrushKeyAction::Flow(fraction)
            } else {
                BrushKeyAction::Opacity(fraction)
            });
            continue;
        }
        match key {
            egui::Key::OpenBracket if plain => out.push(BrushKeyAction::DiameterStep { up: false }),
            egui::Key::CloseBracket if plain => out.push(BrushKeyAction::DiameterStep { up: true }),
            egui::Key::OpenBracket => out.push(BrushKeyAction::HardnessStep { up: false }),
            egui::Key::CloseBracket => out.push(BrushKeyAction::HardnessStep { up: true }),
            egui::Key::Minus if plain => out.push(BrushKeyAction::DiameterScale { down: true }),
            egui::Key::Equals if plain => out.push(BrushKeyAction::DiameterScale { down: false }),
            // `+` is Shift+`=` on most layouts, so it must be accepted with or without Shift.
            egui::Key::Plus => out.push(BrushKeyAction::DiameterScale { down: false }),
            _ => {}
        }
    }
    out
}

/// Nearest-neighbor sample of `layer` at page pixel `(wx, wy)` through its transform, or
/// transparent when the page point falls outside the layer image.
pub(crate) fn sample_layer_world(layer: &Layer, wx: usize, wy: usize) -> Color32 {
    let local = layer.world_to_local(Vec2::new(wx as f32 + 0.5, wy as f32 + 0.5));
    let lx = local.x.floor();
    let ly = local.y.floor();
    if lx < 0.0 || ly < 0.0 {
        return Color32::TRANSPARENT;
    }
    let (lx, ly) = (lx as usize, ly as usize);
    let [w, h] = layer.image.size;
    if lx >= w || ly >= h {
        return Color32::TRANSPARENT;
    }
    layer.image.pixels[ly * w + lx]
}

/// Selects the raster directly BENEATH `target_uid` on the unified band-Z axis (the layer the user
/// sees beneath it in the composite), for "merge down". This is the band-Z order — NOT the layer
/// stack index — so after a manual reorder the correct visually-below raster is chosen.
///
/// `rasters` lists every merge candidate as `(uid, band_z)` — see `merge_candidates_by_band_z`,
/// which supplies each raster's shifted `Band::Raster` Z and reserves Z 0 for `Клин`. `Исходник` is
/// excluded by the caller so it can never be a merge target. Among the candidates strictly below
/// the target's Z, the nearest one (greatest Z) wins; ties break toward the earlier list position
/// (stable, matching `draw_composite`'s constant raster tiebreak). Returns `None` when the target is
/// not found or is already the bottom-most candidate.
pub(crate) fn raster_below_by_band_z(rasters: &[(String, u32)], target_uid: &str) -> Option<String> {
    let target_z = rasters
        .iter()
        .find(|(uid, _)| uid == target_uid)
        .map(|(_, z)| *z)?;
    rasters
        .iter()
        .enumerate()
        .filter(|(_, (uid, z))| uid != target_uid && *z < target_z)
        // Nearest below = greatest Z; on a Z tie keep the earlier list position (lower stack index).
        .max_by(|(ia, (_, za)), (ib, (_, zb))| za.cmp(zb).then(ib.cmp(ia)))
        .map(|(_, (uid, _))| uid.clone())
}

/// Page-space pixel rect `(x, y, w, h)` bounding every pixel [`composite_to_page`] can take from
/// `layer`, clamped to a `size` page. `None` when the layer's footprint misses the page entirely.
///
/// This is a strict SUPERSET of the pixels the layer can alter, which is what makes it safe to
/// bound a merge write-back with: `composite_to_page` samples through [`sample_layer_world`], which
/// maps a page point back with the AFFINE `LayerTransform` only — `Layer::deform` is not consulted
/// there — so the four affine `world_corners()` bound every sample that can return a non-transparent
/// pixel, deformed layer or not. Should `sample_layer_world` ever become deform-aware, this bound
/// must grow with it or a merge will silently lose pixels.
///
/// The box is rounded OUTWARD by a whole pixel on each side: a page pixel contributes when its
/// CENTRE falls inside the quad, and the margin also absorbs the `f32` rounding of the corner math.
/// A non-finite corner (a degenerate transform) yields the whole page rather than a guess.
pub(crate) fn page_footprint_rect(layer: &Layer, size: [usize; 2]) -> Option<(usize, usize, usize, usize)> {
    let [pw, ph] = size;
    if pw == 0 || ph == 0 {
        return None;
    }
    let whole_page = Some((0, 0, pw, ph));
    let corners = layer.world_corners();
    if corners.iter().any(|c| !c.x.is_finite() || !c.y.is_finite()) {
        return whole_page;
    }
    let (page_w, page_h) = (pw as f32, ph as f32);
    let mut min = Vec2::new(f32::INFINITY, f32::INFINITY);
    let mut max = Vec2::new(f32::NEG_INFINITY, f32::NEG_INFINITY);
    for c in corners {
        min.x = min.x.min(c.x);
        min.y = min.y.min(c.y);
        max.x = max.x.max(c.x);
        max.y = max.y.max(c.y);
    }
    // Clamp into the page in float space first, so the casts below are on finite, non-negative
    // values no larger than the page dimensions — no truncation or sign loss is possible.
    let x0 = (min.x - 1.0).floor().clamp(0.0, page_w) as usize;
    let y0 = (min.y - 1.0).floor().clamp(0.0, page_h) as usize;
    let x1 = (max.x + 1.0).ceil().clamp(0.0, page_w) as usize;
    let y1 = (max.y + 1.0).ceil().clamp(0.0, page_h) as usize;
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    Some((x0, y0, x1 - x0, y1 - y0))
}

/// Composites `layer_ids` (bottom-to-top, src-over) within the selection into a new raster layer
/// **cropped to the selection bounds** and placed in page space, then pushed on top of `stack`.
/// For [`ClipMode::Cut`] the selected pixels are cleared from every chosen layer except the
/// immutable source. Layers are sampled through their transforms, so partial/rotated/scaled source
/// layers contribute correctly. Returns the selection bounds, or `None` when there is nothing to do.
pub(crate) fn clip_into_new_layer(
    stack: &mut LayerStack,
    selection: &Selection,
    mode: ClipMode,
    layer_ids: &[LayerId],
) -> Option<SelectionBounds> {
    let bounds = selection.bounds()?;
    if layer_ids.is_empty() {
        return None;
    }
    let crop_rect = OverlayRectPx {
        x: bounds.min_x,
        y: bounds.min_y,
        w: bounds.max_x - bounds.min_x + 1,
        h: bounds.max_y - bounds.min_y + 1,
    };

    // Composite into a buffer cropped to the selection bounds (an "incomplete" layer), through the
    // shared ROI composite so the clip and the patch tool cannot drift apart. A clip takes the
    // layers the USER picked at their own pixels — visibility and opacity are deliberately not
    // applied here, which is why the opacities are all 1.0 rather than `layer_opacity`.
    let clip_layers: Vec<CompositeLayer<'_>> = layer_ids
        .iter()
        .filter_map(|&id| stack.layer(id))
        .map(|layer| (layer, 1.0))
        .collect();
    let clip_image = composite_rect(&clip_layers, crop_rect, Some(selection));
    drop(clip_layers);

    if mode == ClipMode::Cut {
        for &id in layer_ids {
            let Some(layer) = stack.layer_mut(id) else {
                continue;
            };
            // The source layer is immutable: it can be copied from but never cut.
            if layer.kind == LayerKind::Source {
                continue;
            }
            // A cut removes base pixels: refuse on a raster still showing a non-destructive effects
            // chain (bake it first). Copying from it is fine; only the destructive clear is blocked.
            if layer.kind == LayerKind::Raster && !layer.can_edit_pixels() {
                ms_log::runtime_log::log_warn("[ps_editor] Сначала запеките слой");
                continue;
            }
            clear_selected_pixels(layer, selection, bounds);
            layer.pixels_dirty = true; // pixels removed → bake effects, rewrite base on save
        }
    }

    let name = match mode {
        // Persisted raster-layer names (round-trip to `layers.json`); stable literals,
        // not localized. See dev-docs/i18n_exclusions.md §A (persisted layer/group names).
        ClipMode::Copy => "Копия".to_string(),
        ClipMode::Cut => "Вырезка".to_string(),
    };
    // Place the cropped layer so it sits exactly where it was lifted from.
    let center = Vec2::new(
        (bounds.min_x + bounds.max_x + 1) as f32 * 0.5,
        (bounds.min_y + bounds.max_y + 1) as f32 * 0.5,
    );
    let transform = layers::LayerTransform {
        center,
        rotation: 0.0,
        scale: 1.0,
    };
    let new_id = stack.add_raster_layer_image(name, clip_image, transform);
    if let Some(layer) = stack.layer_mut(new_id) {
        layer.pixels_dirty = true; // freshly-composited PS pixels
    }
    Some(bounds)
}

/// Clears every pixel of `layer` whose page position falls inside the selection.
///
/// An identity (axis-aligned, page-placed) layer is cleared directly over the bounds; a
/// transformed layer is scanned in its own pixel space, each pixel mapped to page space.
pub(crate) fn clear_selected_pixels(layer: &mut Layer, selection: &Selection, bounds: SelectionBounds) {
    let [w, h] = layer.image.size;
    if layer.transform.is_identity_for(layer.image.size) {
        for y in bounds.min_y..=bounds.max_y.min(h.saturating_sub(1)) {
            let row = y * w;
            for x in bounds.min_x..=bounds.max_x.min(w.saturating_sub(1)) {
                if selection.contains(x, y) {
                    layer.image.pixels[row + x] = Color32::TRANSPARENT;
                }
            }
        }
        return;
    }
    let transform = layer.transform;
    let local_center = layer.image_size() * 0.5;
    for ly in 0..h {
        let row = ly * w;
        for lx in 0..w {
            let local = Vec2::new(lx as f32 + 0.5, ly as f32 + 0.5);
            let world = transform.center
                + rotate_vec((local - local_center) * transform.scale, transform.rotation);
            if world.x < 0.0 || world.y < 0.0 {
                continue;
            }
            if selection.contains(world.x as usize, world.y as usize) {
                layer.image.pixels[row + lx] = Color32::TRANSPARENT;
            }
        }
    }
}

/// Rotates `v` by `angle` radians (clockwise in image space, +y down).
pub(crate) fn rotate_vec(v: Vec2, angle: f32) -> Vec2 {
    let (s, c) = angle.sin_cos();
    Vec2::new(v.x * c - v.y * s, v.x * s + v.y * c)
}

/// Whether any opaque pixel of `layer` lies inside the selection within `bounds` (transform-aware).
pub(crate) fn layer_touches_selection(layer: &Layer, selection: &Selection, bounds: SelectionBounds) -> bool {
    for y in bounds.min_y..=bounds.max_y {
        for x in bounds.min_x..=bounds.max_x {
            if selection.contains(x, y) && sample_layer_world(layer, x, y).a() > 0 {
                return true;
            }
        }
    }
    false
}

/// Converts an in-memory layer transform to its on-disk record (center-anchored, page pixels).
pub(crate) fn transform_to_rec(t: LayerTransform) -> TransformRec {
    TransformRec {
        cx: t.center.x,
        cy: t.center.y,
        rotation: t.rotation,
        scale: t.scale,
    }
}

/// Converts a `[usize; 2]` pixel pair into `[u32; 2]`, or `None` if either component exceeds `u32`
/// (unreachable for realistic image dimensions). Keeps the raster-diff conversions free of lossy
/// `as` casts.
pub(crate) fn usize_pair_to_u32(pair: [usize; 2]) -> Option<[u32; 2]> {
    Some([u32::try_from(pair[0]).ok()?, u32::try_from(pair[1]).ok()?])
}

/// Structural equality of two optional deform meshes (`DeformRec` has no `PartialEq`): both `None`,
/// or both `Some` with equal grid dimensions and identical control points. Used to detect whether a
/// deform gesture actually changed the mesh before recording an undo step.
pub(crate) fn deform_eq(
    a: &Option<ms_models::layer_model::manifest::DeformRec>,
    b: &Option<ms_models::layer_model::manifest::DeformRec>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.cols == b.cols && a.rows == b.rows && a.points_px == b.points_px,
        (None, Some(_)) | (Some(_), None) => false,
    }
}

/// Inverse of [`transform_to_rec`].
pub(crate) fn rec_to_transform(r: TransformRec) -> LayerTransform {
    LayerTransform {
        center: Vec2::new(r.cx, r.cy),
        rotation: r.rotation,
        scale: r.scale,
    }
}

/// Builds a `LayerDoc` Raster `LayerNode` from a stack `Layer`, for adding/mirroring it into the doc
/// (its `z` is reassigned by `add_node`). Carries the layer's uid, name, visibility, opacity,
/// transform, group uid (resolved by the caller's stack), base/display pixels, and effects chain.
pub(crate) fn layer_to_raster_node(layer: &Layer) -> ms_models::layer_model::layer_doc::LayerNode {
    use ms_models::layer_model::layer_doc::{LayerNode, NodeBody, NodeKind};
    LayerNode {
        uid: layer.uid.to_string(),
        name: layer.name.clone(),
        kind: NodeKind::Raster,
        z: 0,
        visible: layer.visible,
        opacity: layer.opacity,
        group_uid: None,
        text_layer_idx: None,
        text_pinned: false,
        text_pinned_by_group: false,
        transform: transform_to_rec(layer.transform),
        deform: None,
        generation: 0,
        pixels_dirty: true,
        body: NodeBody::Raster {
            base_image: layer.base_image.clone(),
            display_image: layer.image.clone(),
            effects: layer.effects.clone(),
            base_file: format!("{}.png", layer.uid),
            // A PS-created raster (e.g. rasterize) defaults to no mask-clip.
            mask_clip: None,
        },
    }
}

/// Wraps a freshly built mask in the page-selection invariant: `Some` only when it selects at
/// least one pixel, `None` otherwise.
///
/// The pure decision behind `PsEditorTabState::set_selection`, and the tab-side twin of
/// `tools::normalize_selection_slot`. A `Some(all-zero)` selection is the phantom the whole
/// invariant exists to prevent: invisible (no marquee) but still clipping every brush stroke.
#[must_use]
pub(crate) fn non_empty_selection(selection: Selection) -> Option<Selection> {
    selection.any().then_some(selection)
}

/// The undo op that records a pixel-region edit of a layer of `kind`, or `None` when that kind
/// accepts no pixel edit at all.
///
/// The pure decision behind `PsEditorTabState::record_pixel_region`, split out so the variant
/// choice — which also NAMES the write-back route the commit takes — can be exercised without a
/// tab state, a stack or a history. The two are one decision and must not drift:
/// * [`LayerKind::Raster`] ⇒ [`PsEditOp::RasterPixels`], keyed on the layer's stable doc uid,
///   because a user raster is a doc node and its commit goes to the shared `LayerDoc`;
/// * [`LayerKind::Clean`] ⇒ [`PsEditOp::CleanPixels`], carrying NO uid, because base-layer uids are
///   regenerated on every page load and the commit goes to `CleanOverlaysModel` instead;
/// * [`LayerKind::Source`] ⇒ `None`: it is immutable (`Layer::can_edit_pixels`), so it is
///   unreachable here, but a real `match` (no `_ =>`) forces a new paintable kind to be handled.
///
/// `dir` is always `Forward`: recording is observer-style, the forward edit having already been
/// applied live.
#[must_use]
pub(crate) fn pixel_region_edit_op(
    kind: LayerKind,
    page_idx: usize,
    layer_uid: String,
    diff: Arc<RasterDiff>,
    label: String,
) -> Option<PsEditOp> {
    match kind {
        LayerKind::Raster => Some(PsEditOp::RasterPixels {
            page_idx,
            layer_uid,
            diff,
            dir: ApplyDirection::Forward,
            label,
        }),
        LayerKind::Clean => Some(PsEditOp::CleanPixels {
            page_idx,
            diff,
            dir: ApplyDirection::Forward,
            label,
        }),
        LayerKind::Source => None,
    }
}

/// Whether the ACTIVE TOOL's own dock tab is drawn this frame.
///
/// The pure decision behind the seventh tab's `visible` flag, split out so it can be exercised
/// without a dock. It is asked of the tool and of nothing else: the panel belongs to whichever tool
/// is active, so it appears and disappears with the tool selection rather than with a stored user
/// flag. An out-of-range index answers `false` instead of panicking.
#[must_use]
pub(crate) fn tool_panel_visible(tools: &[Box<dyn PsTool>], active_tool_idx: usize) -> bool {
    tools
        .get(active_tool_idx)
        .is_some_and(|tool| tool.wants_main_panel())
}

/// Blends a premultiplied RGBA source rect into a premultiplied destination image at `origin`,
/// returning the rect actually written in the destination's own pixels.
///
/// The pure core of the [`tools::PsToolAction::WriteRegion`] apply path, split out so the geometry
/// and the blend can be exercised without a tab state. Contract:
/// * `origin` is in DESTINATION pixels; the source rect is clipped to the destination and an empty
///   result yields `None`, so a rect that hangs off the page is a no-op rather than a panic;
/// * `coverage`, when present, must be exactly `src.size[0] * src.size[1]` bytes — a wrongly-sized
///   buffer is REFUSED (`None`), never padded, because a silent fallback would land a half-written
///   patch (`CLAUDE.md` §11);
/// * the blend is `dst += (src - dst) * coverage / 255` per premultiplied channel. Linear
///   interpolation of PREMULTIPLIED RGBA is exactly the premultiplied form of the interpolated
///   image, so this is a correct cross-fade and not an approximation of one. `None` coverage means
///   "fully opaque" and copies the source outright, which is what a coverage of 255 also does.
#[must_use]
pub(crate) fn blend_premul_region(
    dst: &mut ColorImage,
    origin: [usize; 2],
    src: &ColorImage,
    coverage: Option<&[u8]>,
) -> Option<tools::DirtyRect> {
    let [dw, dh] = dst.size;
    let [sw, sh] = src.size;
    if let Some(cov) = coverage
        && cov.len() != sw.saturating_mul(sh)
    {
        return None;
    }
    let x0 = origin[0].min(dw);
    let y0 = origin[1].min(dh);
    let x1 = origin[0].saturating_add(sw).min(dw);
    let y1 = origin[1].saturating_add(sh).min(dh);
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    for y in y0..y1 {
        let sy = y - origin[1];
        for x in x0..x1 {
            let sx = x - origin[0];
            let s_idx = sy * sw + sx;
            let d_idx = y * dw + x;
            let (Some(&src_px), Some(dst_px)) =
                (src.pixels.get(s_idx), dst.pixels.get_mut(d_idx))
            else {
                // Both indices are inside their buffers by the clamps above; bail out rather than
                // panic if that ever stops holding.
                continue;
            };
            let weight = coverage.map_or(255, |cov| cov.get(s_idx).copied().unwrap_or(0));
            *dst_px = match weight {
                0 => *dst_px,
                255 => src_px,
                w => lerp_premul(*dst_px, src_px, w),
            };
        }
    }
    Some(tools::DirtyRect {
        min_x: x0,
        min_y: y0,
        max_x: x1 - 1,
        max_y: y1 - 1,
    })
}

/// Linear interpolation between two PREMULTIPLIED colours by `weight / 255`.
///
/// Rounds to nearest so a weight of 255 reproduces `to` exactly and a weight of 0 reproduces
/// `from`; both are short-circuited by the caller, which is what keeps a full-coverage write
/// bit-exact.
#[must_use]
pub(crate) fn lerp_premul(from: Color32, to: Color32, weight: u8) -> Color32 {
    let mix = |a: u8, b: u8| -> u8 {
        let a = u32::from(a);
        let b = u32::from(b);
        let w = u32::from(weight);
        // (a * (255 - w) + b * w + 127) / 255, all terms below 255*255 + 127, so no overflow.
        let v = (a * (255 - w) + b * w + 127) / 255;
        u8::try_from(v).unwrap_or(255)
    };
    Color32::from_rgba_premultiplied(
        mix(from.r(), to.r()),
        mix(from.g(), to.g()),
        mix(from.b(), to.b()),
        mix(from.a(), to.a()),
    )
}

/// Screen position the active tool's `draw_overlay` may follow this frame, or `None` to hide it.
///
/// The pure decision behind the overlay call in `draw_canvas`, split out so it can be exercised
/// without a GUI. Three terms:
/// * `hover_pos` outside `rect` — the pointer left the program-tab area entirely;
/// * `pointer_occluded` — a dock panel, popup or window covers the pointer, the same gate the
///   input path uses (`PsEditorTabState::canvas_pointer_occluded`);
/// * `gesture_in_flight` — the escape hatch that keeps a preview alive once its gesture legitimately
///   started on bare canvas and was then dragged over a panel, mirroring the panning rule.
///
/// The in-flight term is asked of the TOOL and must not be replaced by "the primary button is
/// down": a press that BEGINS on a panel also holds the button down, and the tools correctly refuse
/// to start a gesture there — so `primary_down` would keep painting the preview under the panel in
/// exactly the case this function exists to fix.
/// Converts a page/world coordinate in px to a pixel index, or `None` when it addresses no pixel.
///
/// Rejects negatives, NaN, infinities and anything past `u32::MAX` instead of letting an `as usize`
/// truncate or saturate them into a valid-looking index (§17 of `CLAUDE.md`): a saturating cast
/// turns "the pointer is nowhere near the page" into "the pointer is on the last pixel", which
/// silently samples or clips the wrong pixel. The upper bound is 2^24, the largest integer an `f32`
/// still represents exactly and far above any page dimension, so everything it excludes is out of
/// the page anyway.
///
/// The caller still has to range-check the result against the actual width or height.
#[must_use]
pub(crate) fn page_pixel_index(v: f32) -> Option<usize> {
    /// Largest coordinate accepted: 2^24, the exact-integer limit of an `f32`.
    const MAX_EXACT: f32 = 16_777_216.0;
    if !(0.0..=MAX_EXACT).contains(&v) {
        return None;
    }
    // The range test above proves the truncation is exact and that it fits a `usize`.
    Some(v as usize)
}

#[must_use]
pub(crate) fn overlay_pointer(
    hover_pos: Option<Pos2>,
    rect: Rect,
    pointer_occluded: bool,
    gesture_in_flight: bool,
) -> Option<Pos2> {
    hover_pos
        .filter(|p| rect.contains(*p))
        .filter(|_| !pointer_occluded || gesture_in_flight)
}
