/*
File: tab/mesh_geometry.rs

Purpose:
Geometry, hit-testing, and deform-mesh math for the typing tab's on-canvas
overlay editing (selection/handle drawing, quad/mesh sampling, pointer target
resolution, page/scene/UV conversions and clamping). No `self`, no state.

Main responsibilities:
- draw selection paths, transform/bend/frame/grid handles, and textured deform
  mesh wireframes;
- hit-test transform handles and classify pointer targets across text/raster
  overlays;
- sample and deform quad/mesh control points, and convert between page, scene,
  and UV coordinate spaces with clamping.

Key enums:
- SampledHandleMode
- TypingPointerTarget
- TypingLayerRow

Notes:
Extracted verbatim from `tab.rs`. Free fns and enums are `pub(super)` so `tab.rs`
and sibling submodules of `tab` can use them. `use super::*;` pulls in the parent
module's types and imports.
*/

use super::*;

pub(super) fn draw_dashed_selection_path(painter: &egui::Painter, path: &[Pos2]) {
    if path.len() < 2 {
        return;
    }
    let dash_length = 8.0;
    let gap_length = 6.0;
    let white_offset = dash_length;
    let mut shapes = Vec::new();
    for segment in path.windows(2) {
        egui::Shape::dashed_line_many(
            segment,
            Stroke::new(2.0, Color32::BLACK),
            dash_length,
            gap_length,
            &mut shapes,
        );
        egui::Shape::dashed_line_many_with_offset(
            segment,
            Stroke::new(2.0, Color32::WHITE),
            &[dash_length],
            &[gap_length],
            white_offset,
            &mut shapes,
        );
    }
    painter.extend(shapes);
}

/// Draws the dashed "specified width" guide line + "N px" label above a selected text overlay.
///
/// `render_width_px` is the layer's CONFIGURED width (source px, from `text_params.width_px`) and is
/// what the label prints. The line length is that width scaled to screen by
/// `display_scale_px_per_source_px` (= `zoom * user_scale`). The scale is deliberately taken from the
/// overlay's placement transform, NOT from its on-screen bounding box: a raster-rotated overlay's AABB
/// inflates with the rotation, which would stretch the guide and make it disagree with its own label.
/// The guide therefore always reflects the specified width regardless of raster rotation.
pub(super) fn draw_text_overlay_width_guide(
    painter: &egui::Painter,
    selection_bounds_rect: Rect,
    render_width_px: u32,
    display_scale_px_per_source_px: f32,
) {
    let guide_width = render_width_px.max(1) as f32 * display_scale_px_per_source_px.max(0.0);
    let half_width = guide_width.max(1.0) * 0.5;
    let center_x = selection_bounds_rect.center().x;
    let line_y = selection_bounds_rect.top() - TEXT_OVERLAY_WIDTH_GUIDE_GAP_PX;
    let left = Pos2::new(center_x - half_width, line_y);
    let right = Pos2::new(center_x + half_width, line_y);
    let tick_top_y = line_y - TEXT_OVERLAY_WIDTH_GUIDE_TICK_HALF_PX;
    let tick_bottom_y = line_y + TEXT_OVERLAY_WIDTH_GUIDE_TICK_HALF_PX;

    draw_dashed_selection_path(
        painter,
        &[
            Pos2::new(left.x, tick_top_y),
            Pos2::new(left.x, tick_bottom_y),
        ],
    );
    draw_dashed_selection_path(painter, &[left, right]);
    draw_dashed_selection_path(
        painter,
        &[
            Pos2::new(right.x, tick_top_y),
            Pos2::new(right.x, tick_bottom_y),
        ],
    );

    let label = format!("{} px", render_width_px.max(1));
    let label_pos = Pos2::new(center_x, tick_top_y - TEXT_OVERLAY_WIDTH_GUIDE_LABEL_GAP_PX);
    let font_id = egui::FontId::proportional(13.0);
    painter.text(
        label_pos + Vec2::new(1.0, 1.0),
        egui::Align2::CENTER_BOTTOM,
        label.as_str(),
        font_id.clone(),
        Color32::BLACK,
    );
    painter.text(
        label_pos,
        egui::Align2::CENTER_BOTTOM,
        label,
        font_id,
        Color32::WHITE,
    );
}

/// Computes the new configured text width (source px) for a width-guide tick drag.
///
/// The guide is CENTERED on the overlay, so a single tick's screen-x offset (`pointer_dx_screen`,
/// current pointer x minus the drag-start x) changes the TOTAL width by twice its source-px
/// equivalent. `display_scale_px_per_source_px` (= `zoom * user_scale`) converts screen px to source
/// px. `right` is the dragged tick: the right tick widens on a positive (rightward) offset, the left
/// tick is mirrored. The result is clamped to
/// [`TEXT_OVERLAY_WIDTH_MIN_PX`, `TEXT_OVERLAY_WIDTH_MAX_PX`].
pub(super) fn width_from_guide_drag(
    start_width_px: u32,
    right: bool,
    pointer_dx_screen: f32,
    display_scale_px_per_source_px: f32,
) -> u32 {
    let sign = if right { 1.0 } else { -1.0 };
    let delta_source_px = pointer_dx_screen / display_scale_px_per_source_px.max(f32::EPSILON);
    (start_width_px as f32 + 2.0 * sign * delta_source_px)
        .round()
        .clamp(
            TEXT_OVERLAY_WIDTH_MIN_PX as f32,
            TEXT_OVERLAY_WIDTH_MAX_PX as f32,
        ) as u32
}

pub(super) fn mesh_boundary_path(mesh_scene: &[Pos2], cols: usize, rows: usize) -> Vec<Pos2> {
    if cols < 2 || rows < 2 || mesh_scene.len() != cols.saturating_mul(rows) {
        return Vec::new();
    }

    let idx = |col: usize, row: usize| -> usize { row * cols + col };
    let mut path = Vec::with_capacity(cols.saturating_mul(2) + rows.saturating_mul(2) + 1);

    for col in 0..cols {
        path.push(mesh_scene[idx(col, 0)]);
    }
    for row in 1..rows {
        path.push(mesh_scene[idx(cols - 1, row)]);
    }
    if rows > 1 {
        for col in (0..(cols - 1)).rev() {
            path.push(mesh_scene[idx(col, rows - 1)]);
        }
    }
    if cols > 1 {
        for row in (1..(rows - 1)).rev() {
            path.push(mesh_scene[idx(0, row)]);
        }
    }
    if let Some(first) = path.first().copied() {
        path.push(first);
    }
    path
}

pub(super) fn expand_selection_mesh_to_min_screen_side(
    mesh_scene: &[Pos2],
    cols: usize,
    rows: usize,
) -> Vec<Pos2> {
    if cols < 2 || rows < 2 || mesh_scene.len() != cols.saturating_mul(rows) {
        return mesh_scene.to_vec();
    }

    if cols == 2 && rows == 2 {
        return expand_quad_selection_mesh_to_min_screen_side(mesh_scene);
    }

    expand_axis_aligned_selection_mesh_to_min_screen_side(mesh_scene)
}

pub(super) fn expand_quad_selection_mesh_to_min_screen_side(mesh_scene: &[Pos2]) -> Vec<Pos2> {
    let quad = [mesh_scene[0], mesh_scene[1], mesh_scene[3], mesh_scene[2]];
    let width = ((quad[0].distance(quad[1]) + quad[3].distance(quad[2])) * 0.5).max(f32::EPSILON);
    let height = ((quad[0].distance(quad[3]) + quad[1].distance(quad[2])) * 0.5).max(f32::EPSILON);
    if width >= TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX
        && height >= TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX
    {
        return mesh_scene.to_vec();
    }

    let scale_x = (TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX / width).max(1.0);
    let scale_y = (TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX / height).max(1.0);
    let center = quad_center_scene(&quad);
    let top_axis = normalized_or_none(quad[1] - quad[0]);
    let left_axis = normalized_or_none(quad[3] - quad[0]);
    let (Some(x_axis), Some(y_axis)) = (top_axis, left_axis) else {
        return expand_axis_aligned_selection_mesh_to_min_screen_side(mesh_scene);
    };

    mesh_scene
        .iter()
        .map(|point| {
            let delta = *point - center;
            center + x_axis * delta.dot(x_axis) * scale_x + y_axis * delta.dot(y_axis) * scale_y
        })
        .collect()
}

pub(super) fn expand_axis_aligned_selection_mesh_to_min_screen_side(mesh_scene: &[Pos2]) -> Vec<Pos2> {
    let bounds = deform_mesh_bounds(mesh_scene);
    if !bounds.is_positive() {
        return mesh_scene.to_vec();
    }
    let width = bounds.width().max(f32::EPSILON);
    let height = bounds.height().max(f32::EPSILON);
    if width >= TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX
        && height >= TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX
    {
        return mesh_scene.to_vec();
    }

    let center = bounds.center();
    let scale_x = (TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX / width).max(1.0);
    let scale_y = (TEXT_OVERLAY_MIN_SELECTION_SIDE_SCREEN_PX / height).max(1.0);
    mesh_scene
        .iter()
        .map(|point| {
            Pos2::new(
                center.x + (point.x - center.x) * scale_x,
                center.y + (point.y - center.y) * scale_y,
            )
        })
        .collect()
}

pub(super) fn normalized_or_none(vector: Vec2) -> Option<Vec2> {
    let len = vector.length();
    if len <= f32::EPSILON {
        None
    } else {
        Some(vector / len)
    }
}

pub(super) fn draw_perspective_handles(painter: &egui::Painter, quad: &[Pos2; 4]) {
    for corner in quad {
        painter.circle_filled(
            *corner,
            TEXT_OVERLAY_TRANSFORM_HANDLE_RADIUS_PX,
            Color32::from_rgba_unmultiplied(255, 80, 80, 230),
        );
        painter.circle_stroke(
            *corner,
            TEXT_OVERLAY_TRANSFORM_HANDLE_RADIUS_PX,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 200)),
        );
    }
}

pub(super) fn draw_bend_handles(painter: &egui::Painter, mesh_scene: &[Pos2], cols: usize, rows: usize) {
    for handle_idx in 0..bend_handle_count() {
        let Some((surface_col, surface_row)) = bend_handle_surface_coord(handle_idx, cols, rows)
        else {
            continue;
        };
        let point = mesh_scene[surface_row * cols + surface_col];
        painter.circle_filled(
            point,
            TEXT_OVERLAY_BEND_HANDLE_RADIUS_PX,
            Color32::from_rgba_unmultiplied(255, 110, 110, 215),
        );
        painter.circle_stroke(
            point,
            TEXT_OVERLAY_BEND_HANDLE_RADIUS_PX,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 180)),
        );
    }
}

pub(super) fn draw_frame_handles(
    painter: &egui::Painter,
    mesh_scene: &[Pos2],
    cols: usize,
    rows: usize,
    side_points: usize,
) {
    for handle_idx in 0..frame_handle_count(side_points) {
        let Some((surface_col, surface_row)) =
            frame_handle_surface_coord(handle_idx, side_points, cols, rows)
        else {
            continue;
        };
        let point = mesh_scene[surface_row * cols + surface_col];
        painter.circle_filled(
            point,
            TEXT_OVERLAY_FRAME_HANDLE_RADIUS_PX,
            Color32::from_rgba_unmultiplied(255, 140, 110, 220),
        );
        painter.circle_stroke(
            point,
            TEXT_OVERLAY_FRAME_HANDLE_RADIUS_PX,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 180)),
        );
    }
}

pub(super) fn draw_grid_handles(
    painter: &egui::Painter,
    mesh_scene: &[Pos2],
    cols: usize,
    rows: usize,
    side_points: usize,
) {
    for handle_idx in 0..grid_handle_count(side_points) {
        let Some((surface_col, surface_row)) =
            grid_handle_surface_coord(handle_idx, side_points, cols, rows)
        else {
            continue;
        };
        let point = mesh_scene[surface_row * cols + surface_col];
        painter.circle_filled(
            point,
            TEXT_OVERLAY_FRAME_HANDLE_RADIUS_PX,
            Color32::from_rgba_unmultiplied(255, 180, 110, 225),
        );
        painter.circle_stroke(
            point,
            TEXT_OVERLAY_FRAME_HANDLE_RADIUS_PX,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 180)),
        );
    }
}

/// The four scene-space corners of a raster layer's image quad (top-left, top-right, bottom-right,
/// bottom-left), from its `TransformRec` (center page px, uniform scale, rotation radians). Mirrors
/// the corner math in `draw_one_raster_layer`.
pub(super) fn raster_quad_scene(
    transform: &crate::models::layer_model::manifest::TransformRec,
    size: [usize; 2],
    image_rect: Rect,
    zoom: f32,
) -> [Pos2; 4] {
    let (sin_a, cos_a) = transform.rotation.sin_cos();
    let hw = size[0] as f32 * 0.5 * transform.scale;
    let hh = size[1] as f32 * 0.5 * transform.scale;
    let corners = [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)];
    let mut quad = [Pos2::ZERO; 4];
    for (i, (dx, dy)) in corners.iter().enumerate() {
        let rx = dx * cos_a - dy * sin_a;
        let ry = dx * sin_a + dy * cos_a;
        quad[i] = scene_from_page_px(image_rect, zoom, [transform.cx + rx, transform.cy + ry]);
    }
    quad
}

/// The 4 corner scene points of a deform mesh grid (TL, TR, BR, BL), for perspective-handle drag.
pub(super) fn deform_mesh_corners_scene(
    deform: &crate::models::layer_model::manifest::DeformRec,
    image_rect: Rect,
    zoom: f32,
) -> Option<[Pos2; 4]> {
    let (c, r) = (deform.cols, deform.rows);
    if c < 2 || r < 2 || deform.points_px.len() != c * r {
        return None;
    }
    let at = |col: usize, row: usize| {
        scene_from_page_px(image_rect, zoom, deform.points_px[row * c + col])
    };
    Some([at(0, 0), at(c - 1, 0), at(c - 1, r - 1), at(0, r - 1)])
}

/// All scene points of a deform mesh grid (row-major), for drawing the wireframe.
pub(super) fn deform_mesh_scene_points(
    deform: &crate::models::layer_model::manifest::DeformRec,
    image_rect: Rect,
    zoom: f32,
) -> Vec<Pos2> {
    deform
        .points_px
        .iter()
        .map(|p| scene_from_page_px(image_rect, zoom, *p))
        .collect()
}

/// Draws a deform mesh's grid lines (row + column segments) — the wireframe shown while a raster is in
/// perspective transform mode.
pub(super) fn draw_textured_deform_mesh_wire(painter: &egui::Painter, mesh_scene: &[Pos2], cols: usize, rows: usize) {
    if cols < 2 || rows < 2 || mesh_scene.len() != cols * rows {
        return;
    }
    let stroke = Stroke::new(1.0, Color32::from_rgba_unmultiplied(90, 185, 255, 170));
    let at = |c: usize, r: usize| mesh_scene[r * cols + c];
    for r in 0..rows {
        for c in 0..cols {
            if c + 1 < cols {
                painter.line_segment([at(c, r), at(c + 1, r)], stroke);
            }
            if r + 1 < rows {
                painter.line_segment([at(c, r), at(c, r + 1)], stroke);
            }
        }
    }
}

pub(super) fn draw_rotation_handle(painter: &egui::Painter, quad: &[Pos2; 4], image_rect: Rect) {
    let (corner, handle) = rotation_handle_scene_with_corner(quad, image_rect);
    painter.line_segment(
        [corner, handle],
        Stroke::new(2.0, Color32::from_rgba_unmultiplied(0, 0, 0, 180)),
    );
    painter.circle_filled(
        handle,
        TEXT_OVERLAY_ROTATE_HANDLE_RADIUS_PX,
        Color32::from_rgba_unmultiplied(90, 185, 255, 235),
    );
    painter.circle_stroke(
        handle,
        TEXT_OVERLAY_ROTATE_HANDLE_RADIUS_PX,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 210)),
    );
}

pub(super) fn draw_brush_preview(painter: &egui::Painter, center: Pos2, radius_px: f32) {
    painter.circle_stroke(
        center,
        radius_px.max(2.0),
        Stroke::new(1.5, Color32::from_rgba_unmultiplied(255, 215, 120, 220)),
    );
    painter.circle_stroke(
        center,
        3.0,
        Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 245, 210, 180)),
    );
}

pub(super) fn hit_test_transform_handle(pointer_scene: Pos2, quad_scene: &[Pos2; 4]) -> Option<usize> {
    for (idx, corner) in quad_scene.iter().enumerate() {
        if pointer_scene.distance(*corner) <= TEXT_OVERLAY_TRANSFORM_HANDLE_RADIUS_PX * 2.0 {
            return Some(idx);
        }
    }
    None
}

pub(super) fn hit_test_bend_handle(
    pointer_scene: Pos2,
    mesh_scene: &[Pos2],
    cols: usize,
    rows: usize,
) -> Option<usize> {
    for handle_idx in 0..bend_handle_count() {
        let Some((surface_col, surface_row)) = bend_handle_surface_coord(handle_idx, cols, rows)
        else {
            continue;
        };
        let point_idx = surface_row * cols + surface_col;
        if pointer_scene.distance(mesh_scene[point_idx]) <= TEXT_OVERLAY_BEND_HANDLE_RADIUS_PX * 2.0
        {
            return Some(handle_idx);
        }
    }
    None
}

pub(super) fn hit_test_frame_handle(
    pointer_scene: Pos2,
    mesh_scene: &[Pos2],
    cols: usize,
    rows: usize,
    side_points: usize,
) -> Option<usize> {
    for handle_idx in 0..frame_handle_count(side_points) {
        let Some((surface_col, surface_row)) =
            frame_handle_surface_coord(handle_idx, side_points, cols, rows)
        else {
            continue;
        };
        let point_idx = surface_row * cols + surface_col;
        if pointer_scene.distance(mesh_scene[point_idx])
            <= TEXT_OVERLAY_FRAME_HANDLE_RADIUS_PX * 2.0
        {
            return Some(handle_idx);
        }
    }
    None
}

pub(super) fn hit_test_grid_handle(
    pointer_scene: Pos2,
    mesh_scene: &[Pos2],
    cols: usize,
    rows: usize,
    side_points: usize,
) -> Option<usize> {
    for handle_idx in 0..grid_handle_count(side_points) {
        let Some((surface_col, surface_row)) =
            grid_handle_surface_coord(handle_idx, side_points, cols, rows)
        else {
            continue;
        };
        let point_idx = surface_row * cols + surface_col;
        if pointer_scene.distance(mesh_scene[point_idx])
            <= TEXT_OVERLAY_FRAME_HANDLE_RADIUS_PX * 2.0
        {
            return Some(handle_idx);
        }
    }
    None
}

pub(super) fn bend_handle_count() -> usize {
    TEXT_OVERLAY_BEND_HANDLE_COLS
        .saturating_sub(2)
        .saturating_mul(TEXT_OVERLAY_BEND_HANDLE_ROWS.saturating_sub(2))
}

pub(super) fn frame_handle_count(side_points: usize) -> usize {
    if side_points < 3 {
        0
    } else {
        side_points.saturating_sub(1).saturating_mul(4)
    }
}

pub(super) fn grid_handle_count(side_points: usize) -> usize {
    if side_points < 2 {
        0
    } else {
        side_points.saturating_mul(side_points)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum SampledHandleMode {
    Frame,
    Grid,
}

pub(super) fn bend_handle_surface_coord(
    handle_idx: usize,
    surface_cols: usize,
    surface_rows: usize,
) -> Option<(usize, usize)> {
    if surface_cols < 3
        || surface_rows < 3
        || TEXT_OVERLAY_BEND_HANDLE_COLS < 3
        || TEXT_OVERLAY_BEND_HANDLE_ROWS < 3
    {
        return None;
    }
    let handle_cols = TEXT_OVERLAY_BEND_HANDLE_COLS - 2;
    let handle_rows = TEXT_OVERLAY_BEND_HANDLE_ROWS - 2;
    if handle_idx >= handle_cols.saturating_mul(handle_rows) {
        return None;
    }
    let handle_row = handle_idx / handle_cols + 1;
    let handle_col = handle_idx % handle_cols + 1;
    Some((
        sample_control_axis_to_surface(handle_col, TEXT_OVERLAY_BEND_HANDLE_COLS, surface_cols),
        sample_control_axis_to_surface(handle_row, TEXT_OVERLAY_BEND_HANDLE_ROWS, surface_rows),
    ))
}

pub(super) fn frame_handle_surface_coord(
    handle_idx: usize,
    side_points: usize,
    surface_cols: usize,
    surface_rows: usize,
) -> Option<(usize, usize)> {
    if side_points < 3 || surface_cols < 2 || surface_rows < 2 {
        return None;
    }

    let side_points = side_points.min(surface_cols.min(surface_rows));
    let top_count = side_points;
    let right_count = side_points - 1;
    let bottom_count = side_points - 1;
    let left_count = side_points - 2;
    let total = top_count + right_count + bottom_count + left_count;
    if handle_idx >= total {
        return None;
    }

    if handle_idx < top_count {
        return Some((
            sample_control_axis_to_surface(handle_idx, side_points, surface_cols),
            0,
        ));
    }
    let idx = handle_idx - top_count;
    if idx < right_count {
        return Some((
            surface_cols - 1,
            sample_control_axis_to_surface(idx + 1, side_points, surface_rows),
        ));
    }
    let idx = idx - right_count;
    if idx < bottom_count {
        return Some((
            sample_control_axis_to_surface(side_points - 2 - idx, side_points, surface_cols),
            surface_rows - 1,
        ));
    }
    let idx = idx - bottom_count;
    if idx < left_count {
        return Some((
            0,
            sample_control_axis_to_surface(side_points - 2 - idx, side_points, surface_rows),
        ));
    }
    None
}

pub(super) fn grid_handle_surface_coord(
    handle_idx: usize,
    side_points: usize,
    surface_cols: usize,
    surface_rows: usize,
) -> Option<(usize, usize)> {
    if side_points < 2 || surface_cols < 2 || surface_rows < 2 {
        return None;
    }
    let side_points = side_points.min(surface_cols.min(surface_rows));
    let total = side_points.saturating_mul(side_points);
    if handle_idx >= total {
        return None;
    }
    let row = handle_idx / side_points;
    let col = handle_idx % side_points;
    Some((
        sample_control_axis_to_surface(col, side_points, surface_cols),
        sample_control_axis_to_surface(row, side_points, surface_rows),
    ))
}

pub(super) fn is_frame_handle_surface_point(
    col: usize,
    row: usize,
    side_points: usize,
    surface_cols: usize,
    surface_rows: usize,
) -> bool {
    (0..frame_handle_count(side_points)).any(|handle_idx| {
        frame_handle_surface_coord(handle_idx, side_points, surface_cols, surface_rows)
            .is_some_and(|coord| coord == (col, row))
    })
}

pub(super) fn is_grid_handle_surface_point(
    col: usize,
    row: usize,
    side_points: usize,
    surface_cols: usize,
    surface_rows: usize,
) -> bool {
    (0..grid_handle_count(side_points)).any(|handle_idx| {
        grid_handle_surface_coord(handle_idx, side_points, surface_cols, surface_rows)
            .is_some_and(|coord| coord == (col, row))
    })
}

pub(super) fn sampled_handle_surface_coord(
    mode: SampledHandleMode,
    handle_idx: usize,
    side_points: usize,
    surface_cols: usize,
    surface_rows: usize,
) -> Option<(usize, usize)> {
    match mode {
        SampledHandleMode::Frame => {
            frame_handle_surface_coord(handle_idx, side_points, surface_cols, surface_rows)
        }
        SampledHandleMode::Grid => {
            grid_handle_surface_coord(handle_idx, side_points, surface_cols, surface_rows)
        }
    }
}

pub(super) fn is_sampled_handle_surface_point(
    mode: SampledHandleMode,
    col: usize,
    row: usize,
    side_points: usize,
    surface_cols: usize,
    surface_rows: usize,
) -> bool {
    match mode {
        SampledHandleMode::Frame => {
            is_frame_handle_surface_point(col, row, side_points, surface_cols, surface_rows)
        }
        SampledHandleMode::Grid => {
            is_grid_handle_surface_point(col, row, side_points, surface_cols, surface_rows)
        }
    }
}

pub(super) fn sample_control_axis_to_surface(
    control_idx: usize,
    control_count: usize,
    surface_count: usize,
) -> usize {
    if control_count <= 1 || surface_count <= 1 {
        return 0;
    }
    (((surface_count - 1) as f32 * control_idx as f32) / (control_count - 1) as f32)
        .round()
        .clamp(0.0, (surface_count - 1) as f32) as usize
}

/// Paints a `cols`x`rows` textured deform-mesh quad grid into `painter`.
///
/// `mesh_scene` holds the scene-space vertex positions in row-major order and
/// must have exactly `cols * rows` entries; UVs are assigned uniformly across
/// `[0,1]`. `tint` is a PREMULTIPLIED per-vertex color multiplied into the
/// sampled texel: pass `Color32::WHITE` for unchanged opaque rendering, or a
/// premultiplied white with reduced alpha (`Color32::from_white_alpha`) to fade
/// the whole quad. Does nothing when the grid is degenerate (`cols < 2`,
/// `rows < 2`, or the vertex count mismatches).
pub(super) fn draw_textured_deform_mesh(
    painter: &egui::Painter,
    texture_id: egui::TextureId,
    mesh_scene: &[Pos2],
    cols: usize,
    rows: usize,
    tint: Color32,
) {
    if let Some(mesh) = build_textured_deform_mesh(texture_id, mesh_scene, cols, rows, tint) {
        painter.add(egui::Shape::mesh(mesh));
    }
}

/// Builds the row-major `cols`x`rows` textured deform-mesh quad grid.
///
/// See `draw_textured_deform_mesh` for the parameter contract. Returns `None`
/// when the grid is degenerate (`cols < 2`, `rows < 2`, or `mesh_scene.len()`
/// does not equal `cols * rows`); otherwise every emitted vertex carries `tint`.
fn build_textured_deform_mesh(
    texture_id: egui::TextureId,
    mesh_scene: &[Pos2],
    cols: usize,
    rows: usize,
    tint: Color32,
) -> Option<Mesh> {
    if cols < 2 || rows < 2 || mesh_scene.len() != cols.saturating_mul(rows) {
        return None;
    }

    let mut mesh = Mesh::with_texture(texture_id);
    mesh.reserve_vertices(mesh_scene.len());
    mesh.reserve_triangles((cols - 1) * (rows - 1) * 2);

    for row in 0..rows {
        let t = row as f32 / (rows - 1) as f32;
        for col in 0..cols {
            let s = col as f32 / (cols - 1) as f32;
            mesh.vertices.push(egui::epaint::Vertex {
                pos: mesh_scene[row * cols + col],
                uv: Pos2::new(s, t),
                color: tint,
            });
        }
    }

    for row in 0..(rows - 1) {
        for col in 0..(cols - 1) {
            let i0 = (row * cols + col) as u32;
            let i1 = i0 + 1;
            let i2 = ((row + 1) * cols + col) as u32;
            let i3 = i2 + 1;
            mesh.add_triangle(i0, i1, i2);
            mesh.add_triangle(i2, i1, i3);
        }
    }

    Some(mesh)
}

#[cfg(test)]
mod mesh_tint_tests {
    use super::*;

    #[test]
    fn build_textured_deform_mesh_applies_tint_to_every_vertex() {
        let scene = [
            Pos2::new(0.0, 0.0),
            Pos2::new(1.0, 0.0),
            Pos2::new(0.0, 1.0),
            Pos2::new(1.0, 1.0),
        ];
        let tint = Color32::from_white_alpha(128);
        let mesh = build_textured_deform_mesh(egui::TextureId::default(), &scene, 2, 2, tint)
            .expect("2x2 grid is non-degenerate");
        assert_eq!(mesh.vertices.len(), 4);
        assert!(mesh.vertices.iter().all(|v| v.color == tint));
    }

    #[test]
    fn build_textured_deform_mesh_rejects_degenerate_grid() {
        let scene = [Pos2::new(0.0, 0.0)];
        assert!(
            build_textured_deform_mesh(egui::TextureId::default(), &scene, 1, 1, Color32::WHITE)
                .is_none()
        );
    }
}

pub(super) fn bilinear_quad_point(quad: [Pos2; 4], s: f32, t: f32) -> Pos2 {
    let top = quad[0].lerp(quad[1], s);
    let bottom = quad[3].lerp(quad[2], s);
    top.lerp(bottom, t)
}

pub(super) fn point_in_quad(point: Pos2, quad: &[Pos2; 4]) -> bool {
    point_in_triangle(point, quad[0], quad[1], quad[2])
        || point_in_triangle(point, quad[0], quad[2], quad[3])
}

pub(super) fn point_in_triangle(point: Pos2, a: Pos2, b: Pos2, c: Pos2) -> bool {
    fn edge_sign(p: Pos2, p1: Pos2, p2: Pos2) -> f32 {
        (p.x - p2.x) * (p1.y - p2.y) - (p1.x - p2.x) * (p.y - p2.y)
    }

    let d1 = edge_sign(point, a, b);
    let d2 = edge_sign(point, b, c);
    let d3 = edge_sign(point, c, a);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
}

pub(super) fn segment_intersects_quad(start: Pos2, end: Pos2, quad: &[Pos2; 4]) -> bool {
    if point_in_quad(start, quad) || point_in_quad(end, quad) {
        return true;
    }
    for edge_idx in 0..4 {
        let edge_start = quad[edge_idx];
        let edge_end = quad[(edge_idx + 1) % 4];
        if line_segments_intersect(start, end, edge_start, edge_end) {
            return true;
        }
    }
    false
}

pub(super) fn quads_intersect(a: &[Pos2; 4], b: &[Pos2; 4]) -> bool {
    if !quad_bounds(a).intersects(quad_bounds(b)) {
        return false;
    }
    if a.iter().any(|point| point_in_quad(*point, b))
        || b.iter().any(|point| point_in_quad(*point, a))
    {
        return true;
    }
    for a_idx in 0..4 {
        let a_start = a[a_idx];
        let a_end = a[(a_idx + 1) % 4];
        for b_idx in 0..4 {
            let b_start = b[b_idx];
            let b_end = b[(b_idx + 1) % 4];
            if line_segments_intersect(a_start, a_end, b_start, b_end) {
                return true;
            }
        }
    }
    false
}

pub(super) fn line_segments_intersect(a1: Pos2, a2: Pos2, b1: Pos2, b2: Pos2) -> bool {
    const EPS: f32 = 0.001;

    fn cross(origin: Pos2, a: Pos2, b: Pos2) -> f32 {
        (a.x - origin.x) * (b.y - origin.y) - (a.y - origin.y) * (b.x - origin.x)
    }

    fn on_segment(a: Pos2, p: Pos2, b: Pos2) -> bool {
        p.x >= a.x.min(b.x) - EPS
            && p.x <= a.x.max(b.x) + EPS
            && p.y >= a.y.min(b.y) - EPS
            && p.y <= a.y.max(b.y) + EPS
    }

    let d1 = cross(a1, a2, b1);
    let d2 = cross(a1, a2, b2);
    let d3 = cross(b1, b2, a1);
    let d4 = cross(b1, b2, a2);

    if ((d1 > EPS && d2 < -EPS) || (d1 < -EPS && d2 > EPS))
        && ((d3 > EPS && d4 < -EPS) || (d3 < -EPS && d4 > EPS))
    {
        return true;
    }

    (d1.abs() <= EPS && on_segment(a1, b1, a2))
        || (d2.abs() <= EPS && on_segment(a1, b2, a2))
        || (d3.abs() <= EPS && on_segment(b1, a1, b2))
        || (d4.abs() <= EPS && on_segment(b1, a2, b2))
}

pub(super) fn quad_bounds(quad: &[Pos2; 4]) -> Rect {
    let mut min_x = quad[0].x;
    let mut min_y = quad[0].y;
    let mut max_x = quad[0].x;
    let mut max_y = quad[0].y;
    for point in quad.iter().skip(1) {
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    Rect::from_min_max(Pos2::new(min_x, min_y), Pos2::new(max_x, max_y))
}

pub(super) fn quad_center_scene(quad: &[Pos2; 4]) -> Pos2 {
    let (sum_x, sum_y) = quad.iter().fold((0.0f32, 0.0f32), |(acc_x, acc_y), p| {
        (acc_x + p.x, acc_y + p.y)
    });
    Pos2::new(sum_x / 4.0, sum_y / 4.0)
}

pub(super) fn rotation_handle_scene(quad: &[Pos2; 4], image_rect: Rect) -> Pos2 {
    rotation_handle_scene_with_corner(quad, image_rect).1
}

pub(super) fn rotation_handle_scene_with_corner(quad: &[Pos2; 4], image_rect: Rect) -> (Pos2, Pos2) {
    let corner_idx = select_rotation_handle_corner(quad, image_rect);
    let corner = quad[corner_idx];
    let center = quad_center_scene(quad);
    let dir = corner - center;
    let len_sq = dir.length_sq();
    if len_sq <= f32::EPSILON {
        return (
            corner,
            corner + Vec2::new(TEXT_OVERLAY_ROTATE_HANDLE_OFFSET_PX, 0.0),
        );
    }
    (
        corner,
        corner + dir / len_sq.sqrt() * TEXT_OVERLAY_ROTATE_HANDLE_OFFSET_PX,
    )
}

/// Finds the TOPMOST raster (last in `entries`, which are bottom-to-top) under `pointer`, SKIPPING the
/// currently-selected idx so the normal-mode interaction never creates a second response for the
/// selected raster (egui duplicate-Id). A raster is "under" the pointer if the point is inside its quad
/// OR within the rotate-handle radius. Returns `(idx, quad, center, on_rotate)`. Pure (geometry only),
/// so it is unit-testable. `excluded` (the selected idx) is skipped; pass `None` to consider every entry.
pub(super) fn topmost_raster_target(
    entries: &[(usize, [Pos2; 4], Pos2)],
    pointer: Option<Pos2>,
    image_rect: Rect,
    excluded: Option<usize>,
) -> Option<(usize, [Pos2; 4], Pos2, bool)> {
    let p = pointer?;
    entries.iter().rev().find_map(|(idx, quad, center)| {
        if excluded == Some(*idx) {
            return None;
        }
        let (_, handle) = rotation_handle_scene_with_corner(quad, image_rect);
        let on_rotate = p.distance(handle) <= TEXT_OVERLAY_ROTATE_HANDLE_RADIUS_PX * 2.0;
        if point_in_quad(p, quad) || on_rotate {
            Some((*idx, *quad, *center, on_rotate))
        } else {
            None
        }
    })
}

/// Which kind of layer the pointer should interact with when a text overlay and a raster overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TypingPointerTarget {
    Overlay,
    Raster,
    None,
}

/// Picks the TOPMOST item (text overlay vs raster) under the pointer by UNIFIED band-Z, so the click
/// goes to whatever is drawn on top — matching the canvas draw order exactly. `overlay_z` / `raster_z`
/// are the topmost overlay's / raster's band-Z *if one is under the pointer* (else `None`). Ties go to
/// the OVERLAY (text draws above a raster at the same band-Z, mirroring `merged_fills`' `(z, kind)`
/// tiebreak where raster=0 < overlay=1). Pure, so it is unit-testable.
pub(super) fn unified_topmost_pointer_target(
    overlay_z: Option<u32>,
    raster_z: Option<u32>,
) -> TypingPointerTarget {
    match (overlay_z, raster_z) {
        (Some(oz), Some(rz)) => {
            // Equal band-Z → overlay wins (overlay draws above raster at the same band).
            if oz >= rz {
                TypingPointerTarget::Overlay
            } else {
                TypingPointerTarget::Raster
            }
        }
        (Some(_), None) => TypingPointerTarget::Overlay,
        (None, Some(_)) => TypingPointerTarget::Raster,
        (None, None) => TypingPointerTarget::None,
    }
}

/// How many text-preview characters fit in a text row's available label width, with a floor of
/// `LAYERS_PANEL_MIN_PREVIEW_CHARS`. `available_px` is the row width left for the preview text (panel
/// content width minus the fixed row overhead — buttons, `Текст (…)` wrapper, spacing); `char_px` is a
/// representative glyph width. Wider panel → more chars before the dots; never below the min. Pure.
pub(super) fn preview_char_budget(available_px: f32, char_px: f32) -> usize {
    if char_px <= 0.0 || !available_px.is_finite() {
        return LAYERS_PANEL_MIN_PREVIEW_CHARS;
    }
    let fits = (available_px / char_px).floor();
    let fits = if fits.is_finite() && fits > 0.0 { fits as usize } else { 0 };
    fits.max(LAYERS_PANEL_MIN_PREVIEW_CHARS)
}

/// Builds the `{preview}` shown inside a text row's `Текст ({preview})` label.
///
/// - Takes the first `max_chars` CHARACTERS (Unicode `chars()`, NOT bytes — text is Cyrillic) of `text`
///   after trimming leading whitespace. `max_chars` grows with the panel width (min 5).
/// - Ensures the run of trailing "dot-equivalents" is AT LEAST 3, accounting for dots already present:
///   a regular dot `.` counts 1, the single ellipsis char `…` (U+2026) counts 3. Trailing dots are
///   counted from the end of the prefix until the first non-dot char; then `max(0, 3 - count)` regular
///   dots are appended.
/// - Empty (after trim) → `""` (the caller then shows just `Текст`, no parentheses).
///
/// Crate-visible so other tabs (e.g. the PS editor layers panel) reuse the SAME preview logic.
pub(crate) fn text_preview_label(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut prefix: String = trimmed.chars().take(max_chars).collect();
    // Count trailing dot-equivalents (regular dot = 1, ellipsis = 3), stopping at the first non-dot.
    let mut existing = 0u32;
    for ch in prefix.chars().rev() {
        match ch {
            '.' => existing += 1,
            '…' => existing += 3,
            _ => break,
        }
    }
    let needed = 3u32.saturating_sub(existing);
    for _ in 0..needed {
        prefix.push('.');
    }
    prefix
}

/// One row in the unified "Слои страницы" list: a text/image overlay (index into `self.overlays`) or a
/// raster (index into `raster_layers_by_page[page]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TypingLayerRow {
    Overlay(usize),
    Raster(usize),
}

/// Orders the page's layer rows for the panel: by unified band-Z DESCENDING (top of the stack first),
/// interleaving overlays and rasters. Tie-break at equal Z: OVERLAY above RASTER (matches the canvas
/// draw/hit-test tie-break where raster=0 < overlay=1). The input is `(row, band_z, raster_below_overlay)`
/// where the bool is `true` for a raster (sorts below an overlay at the same Z). Pure → unit-testable.
pub(super) fn order_unified_layer_rows(mut rows: Vec<(TypingLayerRow, u32, bool)>) -> Vec<TypingLayerRow> {
    // Sort TOP-first: higher Z first; at equal Z, overlay (raster_below=false) before raster (true).
    rows.sort_by(|a, b| {
        b.1.cmp(&a.1) // band-Z descending
            .then_with(|| a.2.cmp(&b.2)) // false (overlay) before true (raster) at equal Z
    });
    rows.into_iter().map(|(row, _, _)| row).collect()
}

pub(super) fn select_rotation_handle_corner(quad: &[Pos2; 4], image_rect: Rect) -> usize {
    const ROTATION_HANDLE_CORNER_ORDER: [usize; 4] = [1, 0, 3, 2];

    for corner_idx in ROTATION_HANDLE_CORNER_ORDER {
        let handle = rotation_handle_scene_for_corner(quad, corner_idx);
        let handle_rect = Rect::from_center_size(
            handle,
            Vec2::splat(TEXT_OVERLAY_ROTATE_HANDLE_RADIUS_PX * 2.0),
        );
        if image_rect.contains_rect(handle_rect) {
            return corner_idx;
        }
    }

    1
}

pub(super) fn rotation_handle_scene_for_corner(quad: &[Pos2; 4], corner_idx: usize) -> Pos2 {
    let corner = quad[corner_idx];
    let center = quad_center_scene(quad);
    let dir = corner - center;
    let len_sq = dir.length_sq();
    if len_sq <= f32::EPSILON {
        return corner + Vec2::new(TEXT_OVERLAY_ROTATE_HANDLE_OFFSET_PX, 0.0);
    }
    corner + dir / len_sq.sqrt() * TEXT_OVERLAY_ROTATE_HANDLE_OFFSET_PX
}

pub(super) fn pointer_angle_rad(center: Pos2, pointer: Pos2) -> f32 {
    (pointer.y - center.y).atan2(pointer.x - center.x)
}

pub(super) fn overlay_quad_scene(overlay: &TypingOverlayRuntime, image_rect: Rect, zoom: f32) -> [Pos2; 4] {
    if overlay.deform_mesh.is_none() {
        return default_overlay_quad_scene(overlay, image_rect, zoom);
    }
    let mesh = overlay_deform_mesh(overlay, image_rect, zoom);
    [
        scene_from_page_px(image_rect, zoom, mesh.point(0, 0)),
        scene_from_page_px(image_rect, zoom, mesh.point(mesh.cols - 1, 0)),
        scene_from_page_px(image_rect, zoom, mesh.point(mesh.cols - 1, mesh.rows - 1)),
        scene_from_page_px(image_rect, zoom, mesh.point(0, mesh.rows - 1)),
    ]
}

pub(super) fn overlay_scene_geometry(
    overlay: &TypingOverlayRuntime,
    image_rect: Rect,
    zoom: f32,
) -> TypingOverlaySceneGeometry {
    if overlay.deform_mesh.is_none() {
        let quad_scene = default_overlay_quad_scene(overlay, image_rect, zoom);
        return TypingOverlaySceneGeometry {
            quad_scene,
            mesh_scene: vec![quad_scene[0], quad_scene[1], quad_scene[3], quad_scene[2]],
            mesh_cols: 2,
            mesh_rows: 2,
            bounds_rect: quad_bounds(&quad_scene),
        };
    }

    let deform_mesh = overlay_deform_mesh(overlay, image_rect, zoom);
    let quad_scene = [
        scene_from_page_px(image_rect, zoom, deform_mesh.point(0, 0)),
        scene_from_page_px(image_rect, zoom, deform_mesh.point(deform_mesh.cols - 1, 0)),
        scene_from_page_px(
            image_rect,
            zoom,
            deform_mesh.point(deform_mesh.cols - 1, deform_mesh.rows - 1),
        ),
        scene_from_page_px(image_rect, zoom, deform_mesh.point(0, deform_mesh.rows - 1)),
    ];
    let mesh_scene = scene_mesh_points(&deform_mesh, image_rect, zoom);
    let bounds_rect = deform_mesh_bounds(&mesh_scene);
    TypingOverlaySceneGeometry {
        quad_scene,
        mesh_scene,
        mesh_cols: deform_mesh.cols,
        mesh_rows: deform_mesh.rows,
        bounds_rect,
    }
}

pub(super) fn shift_index_after_remove(index: &mut Option<usize>, removed_idx: usize) {
    if let Some(current_idx) = *index {
        *index = if current_idx == removed_idx {
            None
        } else if current_idx > removed_idx {
            Some(current_idx - 1)
        } else {
            Some(current_idx)
        };
    }
}

pub(super) fn default_overlay_quad_scene(
    overlay: &TypingOverlayRuntime,
    image_rect: Rect,
    zoom: f32,
) -> [Pos2; 4] {
    let center_page_px = clamp_page_point(
        overlay.center_page_px,
        page_size_from_image_rect(image_rect, zoom),
    );
    let scale = overlay.user_scale.max(0.01);
    let center = scene_from_page_px(image_rect, zoom, center_page_px);
    let size = Vec2::new(
        overlay.size_px[0] as f32 * zoom * scale,
        overlay.size_px[1] as f32 * zoom * scale,
    );
    let rect = Rect::from_center_size(center, size);
    let mut quad = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    ];
    if overlay.angle_deg.abs() > f32::EPSILON {
        let radians = overlay.angle_deg.to_radians();
        let (sin_a, cos_a) = radians.sin_cos();
        for point in &mut quad {
            let dx = point.x - center.x;
            let dy = point.y - center.y;
            point.x = center.x + dx * cos_a - dy * sin_a;
            point.y = center.y + dx * sin_a + dy * cos_a;
        }
    }
    quad
}

pub(super) fn default_overlay_quad_uv(
    overlay: &TypingOverlayRuntime,
    image_rect: Rect,
    zoom: f32,
) -> [[f32; 2]; 4] {
    default_overlay_quad_scene(overlay, image_rect, zoom).map(|point| {
        page_px_to_uv(
            page_px_from_scene(image_rect, zoom, point),
            page_size_from_image_rect(image_rect, zoom),
        )
    })
}

pub(super) fn default_overlay_quad_mesh(
    overlay: &TypingOverlayRuntime,
    image_rect: Rect,
    zoom: f32,
) -> TypingOverlayDeformMesh {
    let quad_uv = default_overlay_quad_uv(overlay, image_rect, zoom);
    let page_size = page_size_from_image_rect(image_rect, zoom);
    let quad_px = quad_uv.map(|point| uv_to_page_px(point, page_size));
    TypingOverlayDeformMesh::new(
        2,
        2,
        vec![quad_px[0], quad_px[1], quad_px[3], quad_px[2]],
        page_size,
    )
    .unwrap_or_else(|| {
        default_deform_mesh_for_page(overlay.center_page_px, [1, 1], 1.0, 0.0, [1, 1])
    })
}

pub(super) fn overlay_deform_mesh(
    overlay: &TypingOverlayRuntime,
    image_rect: Rect,
    zoom: f32,
) -> Cow<'_, TypingOverlayDeformMesh> {
    overlay.deform_mesh.as_ref().map_or_else(
        || Cow::Owned(default_overlay_deform_mesh(overlay, image_rect, zoom)),
        Cow::Borrowed,
    )
}

pub(super) fn overlay_deform_mesh_for_page(
    overlay: &TypingOverlayRuntime,
    page_size: [usize; 2],
) -> Cow<'_, TypingOverlayDeformMesh> {
    overlay.deform_mesh.as_ref().map_or_else(
        || {
            Cow::Owned(default_deform_mesh_for_page(
                overlay.center_page_px,
                overlay.size_px,
                overlay.user_scale,
                overlay.angle_deg,
                page_size,
            ))
        },
        Cow::Borrowed,
    )
}

pub(super) fn page_size_from_image_rect(image_rect: Rect, zoom: f32) -> [usize; 2] {
    let zoom = zoom.max(f32::EPSILON);
    [
        (image_rect.width() / zoom).round().max(1.0) as usize,
        (image_rect.height() / zoom).round().max(1.0) as usize,
    ]
}

pub(super) fn scene_from_page_px(image_rect: Rect, zoom: f32, page_px: [f32; 2]) -> Pos2 {
    let page_size = page_size_from_image_rect(image_rect, zoom);
    let clamped = clamp_page_point(page_px, page_size);
    Pos2::new(
        image_rect.left() + clamped[0] * zoom,
        image_rect.top() + clamped[1] * zoom,
    )
}

pub(super) fn page_px_from_scene(image_rect: Rect, zoom: f32, point: Pos2) -> [f32; 2] {
    let zoom = zoom.max(f32::EPSILON);
    [
        (point.x - image_rect.left()) / zoom,
        (point.y - image_rect.top()) / zoom,
    ]
}

pub(super) fn scene_from_uv(image_rect: Rect, u: f32, v: f32) -> Pos2 {
    Pos2::new(
        image_rect.left() + u * image_rect.width(),
        image_rect.top() + v * image_rect.height(),
    )
}

pub(super) fn uv_from_scene(image_rect: Rect, point: Pos2) -> [f32; 2] {
    let w = image_rect.width().max(1.0);
    let h = image_rect.height().max(1.0);
    [
        (point.x - image_rect.left()) / w,
        (point.y - image_rect.top()) / h,
    ]
}

pub(super) fn sync_overlay_center_from_deform_mesh(overlay: &mut TypingOverlayRuntime, page_size: [usize; 2]) {
    let Some(mesh) = overlay.deform_mesh.as_ref() else {
        return;
    };
    let (sum_x, sum_y) = mesh
        .points_px
        .iter()
        .fold((0.0f32, 0.0f32), |(acc_x, acc_y), p| {
            (acc_x + p[0], acc_y + p[1])
        });
    let count = mesh.points_px.len().max(1) as f32;
    overlay.center_page_px = clamp_page_point([sum_x / count, sum_y / count], page_size);
}

pub(super) fn snap_overlay_center_to_pixels_if_enabled(
    overlay: &mut TypingOverlayRuntime,
    strict_pixel_movement: bool,
    page_size: [usize; 2],
) {
    if !strict_pixel_movement {
        return;
    }
    let snapped_center = [
        overlay.center_page_px[0].round(),
        overlay.center_page_px[1].round(),
    ];
    if let Some(mesh) = overlay.deform_mesh.as_mut() {
        let dx_px = snapped_center[0] - overlay.center_page_px[0];
        let dy_px = snapped_center[1] - overlay.center_page_px[1];
        if dx_px.abs() > f32::EPSILON || dy_px.abs() > f32::EPSILON {
            mesh.translate(dx_px, dy_px, page_size);
            sync_overlay_center_from_deform_mesh(overlay, page_size);
        }
    } else {
        overlay.center_page_px = clamp_page_point(snapped_center, page_size);
    }
}

pub(super) fn quantize_drag_page_delta(delta_page_px: [f32; 2], strict_pixel_movement: bool) -> [f32; 2] {
    if !strict_pixel_movement {
        return delta_page_px;
    }
    [
        quantize_drag_page_delta_axis(delta_page_px[0]),
        quantize_drag_page_delta_axis(delta_page_px[1]),
    ]
}

pub(super) fn quantize_drag_page_delta_axis(delta_page_px: f32) -> f32 {
    if delta_page_px.is_sign_negative() {
        delta_page_px.ceil()
    } else {
        delta_page_px.floor()
    }
}

pub(super) fn default_overlay_deform_mesh(
    overlay: &TypingOverlayRuntime,
    image_rect: Rect,
    zoom: f32,
) -> TypingOverlayDeformMesh {
    deform_mesh_from_quad(
        default_overlay_quad_uv(overlay, image_rect, zoom),
        TEXT_OVERLAY_DEFORM_SURFACE_COLS,
        TEXT_OVERLAY_DEFORM_SURFACE_ROWS,
        page_size_from_image_rect(image_rect, zoom),
    )
}

pub(super) fn default_deform_mesh_for_page(
    center_page_px: [f32; 2],
    overlay_size_px: [usize; 2],
    user_scale: f32,
    angle_deg: f32,
    page_size: [usize; 2],
) -> TypingOverlayDeformMesh {
    deform_mesh_from_quad(
        default_quad_uv_for_page(
            center_page_px,
            overlay_size_px,
            user_scale,
            angle_deg,
            page_size,
        ),
        TEXT_OVERLAY_DEFORM_SURFACE_COLS,
        TEXT_OVERLAY_DEFORM_SURFACE_ROWS,
        page_size,
    )
}

pub(super) fn deform_mesh_from_quad(
    quad_uv: [[f32; 2]; 4],
    cols: usize,
    rows: usize,
    page_size: [usize; 2],
) -> TypingOverlayDeformMesh {
    let mut points_px = Vec::with_capacity(cols.saturating_mul(rows));
    for row in 0..rows {
        let tv = row as f32 / (rows - 1) as f32;
        for col in 0..cols {
            let tu = col as f32 / (cols - 1) as f32;
            points_px.push(uv_to_page_px(
                projective_quad_uv(quad_uv, tu, tv),
                page_size,
            ));
        }
    }
    TypingOverlayDeformMesh::new(cols, rows, points_px, page_size).unwrap_or_else(|| {
        TypingOverlayDeformMesh {
            cols: 2,
            rows: 2,
            points_px: quad_uv
                .into_iter()
                .map(|point| uv_to_page_px(point, page_size))
                .collect(),
        }
    })
}

pub(super) fn normalize_deform_mesh_resolution(
    mesh: &TypingOverlayDeformMesh,
    page_size: [usize; 2],
) -> TypingOverlayDeformMesh {
    if mesh.cols == TEXT_OVERLAY_DEFORM_SURFACE_COLS
        && mesh.rows == TEXT_OVERLAY_DEFORM_SURFACE_ROWS
    {
        return mesh.clone();
    }

    let mut points_px = Vec::with_capacity(
        TEXT_OVERLAY_DEFORM_SURFACE_COLS.saturating_mul(TEXT_OVERLAY_DEFORM_SURFACE_ROWS),
    );
    for row in 0..TEXT_OVERLAY_DEFORM_SURFACE_ROWS {
        let tv = row as f32 / (TEXT_OVERLAY_DEFORM_SURFACE_ROWS - 1) as f32;
        for col in 0..TEXT_OVERLAY_DEFORM_SURFACE_COLS {
            let tu = col as f32 / (TEXT_OVERLAY_DEFORM_SURFACE_COLS - 1) as f32;
            points_px.push(sample_deform_mesh_page_px_for_size(mesh, tu, tv, page_size));
        }
    }

    TypingOverlayDeformMesh::new(
        TEXT_OVERLAY_DEFORM_SURFACE_COLS,
        TEXT_OVERLAY_DEFORM_SURFACE_ROWS,
        points_px,
        page_size,
    )
    .unwrap_or_else(|| default_deform_mesh_for_page([0.5, 0.5], [1, 1], 1.0, 0.0, [1, 1]))
}

pub(super) fn scene_mesh_points(mesh: &TypingOverlayDeformMesh, image_rect: Rect, zoom: f32) -> Vec<Pos2> {
    mesh.points_px
        .iter()
        .map(|&point| scene_from_page_px(image_rect, zoom, point))
        .collect()
}

pub(super) fn mesh_page_size_hint(mesh: &TypingOverlayDeformMesh) -> [usize; 2] {
    let bounds = deform_mesh_bounds_px(mesh);
    [
        bounds.max.x.ceil().max(1.0) as usize,
        bounds.max.y.ceil().max(1.0) as usize,
    ]
}

pub(super) fn deform_mesh_bounds_px(mesh: &TypingOverlayDeformMesh) -> Rect {
    let Some(first) = mesh.points_px.first().copied() else {
        return Rect::NOTHING;
    };
    let mut min_x = first[0];
    let mut max_x = first[0];
    let mut min_y = first[1];
    let mut max_y = first[1];
    for point in mesh.points_px.iter().skip(1) {
        min_x = min_x.min(point[0]);
        max_x = max_x.max(point[0]);
        min_y = min_y.min(point[1]);
        max_y = max_y.max(point[1]);
    }
    Rect::from_min_max(Pos2::new(min_x, min_y), Pos2::new(max_x, max_y))
}

pub(super) fn uv_to_page_px(uv: [f32; 2], page_size: [usize; 2]) -> [f32; 2] {
    [
        clamp_overlay_uv_coord(uv[0]) * page_size[0].max(1) as f32,
        clamp_overlay_uv_coord(uv[1]) * page_size[1].max(1) as f32,
    ]
}

pub(super) fn page_px_to_uv(page_px: [f32; 2], page_size: [usize; 2]) -> [f32; 2] {
    let clamped = clamp_page_point(page_px, page_size);
    [
        clamped[0] / page_size[0].max(1) as f32,
        clamped[1] / page_size[1].max(1) as f32,
    ]
}

pub(super) fn clamp_page_point(point: [f32; 2], page_size: [usize; 2]) -> [f32; 2] {
    [
        clamp_overlay_page_coord(point[0], page_size[0]),
        clamp_overlay_page_coord(point[1], page_size[1]),
    ]
}

pub(super) fn clamp_quad_uv(quad: [[f32; 2]; 4]) -> [[f32; 2]; 4] {
    quad.map(clamp_uv_point)
}

pub(super) fn clamp_uv_point(point: [f32; 2]) -> [f32; 2] {
    [
        clamp_overlay_uv_coord(point[0]),
        clamp_overlay_uv_coord(point[1]),
    ]
}

pub(super) fn deform_mesh_bounds_uv(mesh: &TypingOverlayDeformMesh, page_size: [usize; 2]) -> Rect {
    let Some(first) = mesh.points_px.first().copied() else {
        return Rect::NOTHING;
    };
    let first_uv = page_px_to_uv(first, page_size);
    let mut min_u = first_uv[0];
    let mut max_u = first_uv[0];
    let mut min_v = first_uv[1];
    let mut max_v = first_uv[1];
    for point in mesh.points_px.iter().skip(1) {
        let uv = page_px_to_uv(*point, page_size);
        min_u = min_u.min(uv[0]);
        max_u = max_u.max(uv[0]);
        min_v = min_v.min(uv[1]);
        max_v = max_v.max(uv[1]);
    }
    Rect::from_min_max(Pos2::new(min_u, min_v), Pos2::new(max_u, max_v))
}

pub(super) fn mesh_cell_quad_scene(mesh_scene: &[Pos2], cols: usize, col: usize, row: usize) -> [Pos2; 4] {
    let idx = |c: usize, r: usize| -> usize { r * cols + c };
    [
        mesh_scene[idx(col, row)],
        mesh_scene[idx(col + 1, row)],
        mesh_scene[idx(col + 1, row + 1)],
        mesh_scene[idx(col, row + 1)],
    ]
}

pub(super) fn build_mesh_occluder_quads(mesh_scene: &[Pos2], cols: usize, rows: usize) -> Vec<[Pos2; 4]> {
    if cols < 2 || rows < 2 {
        return Vec::new();
    }
    let mut quads = Vec::with_capacity(
        cols.saturating_sub(1)
            .saturating_mul(rows.saturating_sub(1)),
    );
    for row in 0..(rows - 1) {
        for col in 0..(cols - 1) {
            quads.push(mesh_cell_quad_scene(mesh_scene, cols, col, row));
        }
    }
    quads
}

pub(super) fn deform_mesh_contains_point(mesh_scene: &[Pos2], cols: usize, rows: usize, point: Pos2) -> bool {
    if cols < 2 || rows < 2 {
        return false;
    }
    if !deform_mesh_bounds(mesh_scene).contains(point) {
        return false;
    }
    for row in 0..(rows - 1) {
        for col in 0..(cols - 1) {
            if point_in_quad(point, &mesh_cell_quad_scene(mesh_scene, cols, col, row)) {
                return true;
            }
        }
    }
    false
}

pub(super) fn sample_deform_mesh_page_px(mesh: &TypingOverlayDeformMesh, tu: f32, tv: f32) -> [f32; 2] {
    sample_deform_mesh_page_px_for_size(mesh, tu, tv, mesh_page_size_hint(mesh))
}

pub(super) fn sample_deform_mesh_page_px_for_size(
    mesh: &TypingOverlayDeformMesh,
    tu: f32,
    tv: f32,
    page_size: [usize; 2],
) -> [f32; 2] {
    if mesh.cols < 2 || mesh.rows < 2 {
        return [0.5, 0.5];
    }
    let u = tu.clamp(0.0, 1.0) * (mesh.cols - 1) as f32;
    let v = tv.clamp(0.0, 1.0) * (mesh.rows - 1) as f32;
    let col0 = u.floor().clamp(0.0, (mesh.cols - 2) as f32) as usize;
    let row0 = v.floor().clamp(0.0, (mesh.rows - 2) as f32) as usize;
    let col1 = (col0 + 1).min(mesh.cols - 1);
    let row1 = (row0 + 1).min(mesh.rows - 1);
    let local_u = u - col0 as f32;
    let local_v = v - row0 as f32;
    let quad = [
        mesh.point(col0, row0),
        mesh.point(col1, row0),
        mesh.point(col1, row1),
        mesh.point(col0, row1),
    ];
    clamp_page_point(bilinear_quad_page_px(quad, local_u, local_v), page_size)
}

pub(super) fn sample_deform_mesh_uv(
    mesh: &TypingOverlayDeformMesh,
    tu: f32,
    tv: f32,
    page_size: [usize; 2],
) -> [f32; 2] {
    page_px_to_uv(
        sample_deform_mesh_page_px_for_size(mesh, tu, tv, page_size),
        page_size,
    )
}

pub(super) fn mesh_grid_tuv(mesh: &TypingOverlayDeformMesh, col: usize, row: usize) -> [f32; 2] {
    let tu = if mesh.cols <= 1 {
        0.0
    } else {
        col as f32 / (mesh.cols - 1) as f32
    };
    let tv = if mesh.rows <= 1 {
        0.0
    } else {
        row as f32 / (mesh.rows - 1) as f32
    };
    [tu, tv]
}

pub(super) fn apply_bend_handle_drag(
    mesh: &TypingOverlayDeformMesh,
    handle_idx: usize,
    delta_page_px: [f32; 2],
    page_size: [usize; 2],
) -> TypingOverlayDeformMesh {
    let Some((handle_col, handle_row)) =
        bend_handle_surface_coord(handle_idx, mesh.cols, mesh.rows)
    else {
        return mesh.clone();
    };

    let center_tuv = mesh_grid_tuv(mesh, handle_col, handle_row);
    let radius_u = 1.35 / (TEXT_OVERLAY_BEND_HANDLE_COLS.saturating_sub(1)).max(1) as f32;
    let radius_v = 1.35 / (TEXT_OVERLAY_BEND_HANDLE_ROWS.saturating_sub(1)).max(1) as f32;
    let mut next_points = mesh.points_px.clone();

    for row in 0..mesh.rows {
        for col in 0..mesh.cols {
            let [tu, tv] = mesh_grid_tuv(mesh, col, row);
            let du = (tu - center_tuv[0]) / radius_u.max(1e-4);
            let dv = (tv - center_tuv[1]) / radius_v.max(1e-4);
            let dist = (du * du + dv * dv).sqrt();
            if dist >= 1.0 {
                continue;
            }
            let influence = 1.0 - dist;
            let weight = influence * influence * (3.0 - 2.0 * influence);
            let point_idx = row * mesh.cols + col;
            next_points[point_idx] = clamp_page_point(
                [
                    next_points[point_idx][0] + delta_page_px[0] * weight,
                    next_points[point_idx][1] + delta_page_px[1] * weight,
                ],
                page_size,
            );
        }
    }

    TypingOverlayDeformMesh::new(mesh.cols, mesh.rows, next_points, page_size)
        .unwrap_or_else(|| mesh.clone())
}

pub(super) fn apply_sampled_handle_drag(
    mesh: &TypingOverlayDeformMesh,
    mode: SampledHandleMode,
    side_points: usize,
    handle_idx: usize,
    pull_neighbor_handles: bool,
    delta_page_px: [f32; 2],
    page_size: [usize; 2],
) -> TypingOverlayDeformMesh {
    let Some((handle_col, handle_row)) =
        sampled_handle_surface_coord(mode, handle_idx, side_points, mesh.cols, mesh.rows)
    else {
        return mesh.clone();
    };

    let center_tuv = mesh_grid_tuv(mesh, handle_col, handle_row);
    let spacing = 1.0 / (side_points.saturating_sub(1)).max(1) as f32;
    let radius_u = (spacing * 1.75).max(1e-4);
    let radius_v = (spacing * 1.75).max(1e-4);
    let mut next_points = mesh.points_px.clone();

    for row in 0..mesh.rows {
        for col in 0..mesh.cols {
            if !pull_neighbor_handles
                && (col != handle_col || row != handle_row)
                && is_sampled_handle_surface_point(
                    mode,
                    col,
                    row,
                    side_points,
                    mesh.cols,
                    mesh.rows,
                )
            {
                continue;
            }
            let [tu, tv] = mesh_grid_tuv(mesh, col, row);
            let du = (tu - center_tuv[0]) / radius_u;
            let dv = (tv - center_tuv[1]) / radius_v;
            let dist = (du * du + dv * dv).sqrt();
            if dist >= 1.0 {
                continue;
            }
            let influence = 1.0 - dist;
            let weight = influence * influence * (3.0 - 2.0 * influence);
            let point_idx = row * mesh.cols + col;
            next_points[point_idx] = clamp_page_point(
                [
                    next_points[point_idx][0] + delta_page_px[0] * weight,
                    next_points[point_idx][1] + delta_page_px[1] * weight,
                ],
                page_size,
            );
        }
    }

    TypingOverlayDeformMesh::new(mesh.cols, mesh.rows, next_points, page_size)
        .unwrap_or_else(|| mesh.clone())
}

pub(super) fn apply_perspective_corner_drag(
    mesh: &TypingOverlayDeformMesh,
    handle_idx: usize,
    delta_page_px: [f32; 2],
    page_size: [usize; 2],
) -> TypingOverlayDeformMesh {
    if handle_idx >= 4 || mesh.cols < 2 || mesh.rows < 2 {
        return mesh.clone();
    }

    let mut next_points = Vec::with_capacity(mesh.points_px.len());
    for row in 0..mesh.rows {
        for col in 0..mesh.cols {
            let [tu, tv] = mesh_grid_tuv(mesh, col, row);
            let weights = [
                (1.0 - tu) * (1.0 - tv),
                tu * (1.0 - tv),
                tu * tv,
                (1.0 - tu) * tv,
            ];
            let influence = weights[handle_idx];
            next_points.push(clamp_page_point(
                [
                    mesh.point(col, row)[0] + delta_page_px[0] * influence,
                    mesh.point(col, row)[1] + delta_page_px[1] * influence,
                ],
                page_size,
            ));
        }
    }

    TypingOverlayDeformMesh::new(mesh.cols, mesh.rows, next_points, page_size)
        .unwrap_or_else(|| mesh.clone())
}

// Brush deformation depends on distinct input spaces (scene pointer, mesh state, page rect, zoom, tool settings).
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_brush_deform_drag(
    mode: TypingDeformMode,
    mesh: &TypingOverlayDeformMesh,
    default_mesh: &TypingOverlayDeformMesh,
    brush_center_scene: Pos2,
    pointer_scene: Pos2,
    image_rect: Rect,
    zoom: f32,
    settings: &TypingDeformToolSettings,
) -> TypingOverlayDeformMesh {
    if !mode.is_brush_mode() || mesh.cols < 2 || mesh.rows < 2 {
        return mesh.clone();
    }

    let page_size = page_size_from_image_rect(image_rect, zoom);
    let delta_page_px = [
        pointer_scene.x - brush_center_scene.x,
        pointer_scene.y - brush_center_scene.y,
    ];
    let delta_scene = pointer_scene - brush_center_scene;
    let radius_px = settings.brush_radius_px.max(4.0);
    let strength = settings.brush_strength.max(0.01);
    let center_page_px = page_px_from_scene(image_rect, zoom, brush_center_scene);
    let radial_drag = (delta_scene.length() / radius_px).min(1.0);
    let mut next_points = mesh.points_px.clone();

    for row in 0..mesh.rows {
        for col in 0..mesh.cols {
            let idx = row * mesh.cols + col;
            let point_page_px = mesh.point(col, row);
            let point_scene = scene_from_page_px(image_rect, zoom, point_page_px);
            let to_center = point_scene - brush_center_scene;
            let dist_px = to_center.length();
            if dist_px > radius_px {
                continue;
            }
            let influence = 1.0 - dist_px / radius_px;
            let weight = influence * influence * (3.0 - 2.0 * influence) * strength;
            let next_page_px = match mode {
                TypingDeformMode::Bulge => {
                    let dir = normalize_or_zero_page([
                        point_page_px[0] - center_page_px[0],
                        point_page_px[1] - center_page_px[1],
                    ]);
                    let amount = TEXT_OVERLAY_BULGE_PINCH_BRUSH_SCALE
                        * weight
                        * radial_drag
                        * page_size[0].max(page_size[1]).max(1) as f32;
                    [
                        point_page_px[0] + dir[0] * amount,
                        point_page_px[1] + dir[1] * amount,
                    ]
                }
                TypingDeformMode::Pinch => {
                    let dir = normalize_or_zero_page([
                        center_page_px[0] - point_page_px[0],
                        center_page_px[1] - point_page_px[1],
                    ]);
                    let amount = TEXT_OVERLAY_BULGE_PINCH_BRUSH_SCALE
                        * weight
                        * radial_drag
                        * page_size[0].max(page_size[1]).max(1) as f32;
                    [
                        point_page_px[0] + dir[0] * amount,
                        point_page_px[1] + dir[1] * amount,
                    ]
                }
                TypingDeformMode::Push => [
                    point_page_px[0] + delta_page_px[0] * weight,
                    point_page_px[1] + delta_page_px[1] * weight,
                ],
                TypingDeformMode::Twirl => {
                    let angle = delta_scene.x / radius_px * 1.6 * weight;
                    rotate_page_around_center(point_page_px, center_page_px, angle)
                }
                TypingDeformMode::Restore => {
                    let target = sample_deform_mesh_page_px(
                        default_mesh,
                        mesh_grid_tuv(mesh, col, row)[0],
                        mesh_grid_tuv(mesh, col, row)[1],
                    );
                    [
                        lerp(point_page_px[0], target[0], weight.min(1.0)),
                        lerp(point_page_px[1], target[1], weight.min(1.0)),
                    ]
                }
                TypingDeformMode::Smooth => {
                    let target = smooth_mesh_point(mesh, default_mesh, col, row);
                    [
                        lerp(point_page_px[0], target[0], (weight * 0.85).min(1.0)),
                        lerp(point_page_px[1], target[1], (weight * 0.85).min(1.0)),
                    ]
                }
                TypingDeformMode::Stretch => {
                    let dir = normalize_or_zero_scene(delta_scene);
                    let stretch = (delta_scene.length() / radius_px).min(1.0) * 0.08 * weight;
                    let offset = [
                        (point_page_px[0] - center_page_px[0])
                            * dir.x.abs()
                            * stretch
                            * delta_scene.x.signum(),
                        (point_page_px[1] - center_page_px[1])
                            * dir.y.abs()
                            * stretch
                            * delta_scene.y.signum(),
                    ];
                    [point_page_px[0] + offset[0], point_page_px[1] + offset[1]]
                }
                TypingDeformMode::Fold => {
                    let axis = normalize_or_zero_scene(delta_scene);
                    let signed_side = if dist_px <= f32::EPSILON {
                        0.0
                    } else {
                        (to_center.x * axis.y - to_center.y * axis.x).signum()
                    };
                    let fold_dir = egui::vec2(-axis.y, axis.x) * signed_side;
                    [
                        point_page_px[0] + fold_dir.x * 0.06 * weight,
                        point_page_px[1] + fold_dir.y * 0.06 * weight,
                    ]
                }
                _ => point_page_px,
            };
            next_points[idx] = clamp_page_point(next_page_px, page_size);
        }
    }

    TypingOverlayDeformMesh::new(mesh.cols, mesh.rows, next_points, page_size)
        .unwrap_or_else(|| mesh.clone())
}

pub(super) fn smooth_mesh_point(
    mesh: &TypingOverlayDeformMesh,
    default_mesh: &TypingOverlayDeformMesh,
    col: usize,
    row: usize,
) -> [f32; 2] {
    let mut sum = [0.0f32; 2];
    let mut count = 0.0f32;
    let row_start = row.saturating_sub(1);
    let row_end = (row + 1).min(mesh.rows - 1);
    let col_start = col.saturating_sub(1);
    let col_end = (col + 1).min(mesh.cols - 1);
    for rr in row_start..=row_end {
        for cc in col_start..=col_end {
            let point = mesh.point(cc, rr);
            sum[0] += point[0];
            sum[1] += point[1];
            count += 1.0;
        }
    }
    if count <= 0.0 {
        return mesh.point(col, row);
    }
    let avg = [sum[0] / count, sum[1] / count];
    let default_point = sample_deform_mesh_page_px(
        default_mesh,
        mesh_grid_tuv(mesh, col, row)[0],
        mesh_grid_tuv(mesh, col, row)[1],
    );
    [
        lerp(avg[0], default_point[0], 0.15),
        lerp(avg[1], default_point[1], 0.15),
    ]
}

pub(super) fn rotate_page_around_center(
    point_page_px: [f32; 2],
    center_page_px: [f32; 2],
    angle_rad: f32,
) -> [f32; 2] {
    let dx = point_page_px[0] - center_page_px[0];
    let dy = point_page_px[1] - center_page_px[1];
    let (sin_a, cos_a) = angle_rad.sin_cos();
    [
        center_page_px[0] + dx * cos_a - dy * sin_a,
        center_page_px[1] + dx * sin_a + dy * cos_a,
    ]
}

pub(super) fn normalize_or_zero_page(v: [f32; 2]) -> [f32; 2] {
    let len = (v[0] * v[0] + v[1] * v[1]).sqrt();
    if len <= 1e-6 {
        [0.0, 0.0]
    } else {
        [v[0] / len, v[1] / len]
    }
}

pub(super) fn normalize_or_zero_scene(v: Vec2) -> Vec2 {
    let len = v.length();
    if len <= 1e-6 { Vec2::ZERO } else { v / len }
}


pub(super) fn projective_quad_uv(quad_uv: [[f32; 2]; 4], tu: f32, tv: f32) -> [f32; 2] {
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
        return export_bilinear_quad_uv(quad_uv, tu, tv);
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
        return export_bilinear_quad_uv(quad_uv, u, v);
    }
    [(a * u + b * v + c) / denom, (d * u + e * v + f) / denom]
}

pub(super) fn deform_mesh_bounds(mesh_scene: &[Pos2]) -> Rect {
    let Some(first) = mesh_scene.first().copied() else {
        return Rect::NOTHING;
    };
    let mut min_x = first.x;
    let mut min_y = first.y;
    let mut max_x = first.x;
    let mut max_y = first.y;
    for point in mesh_scene.iter().skip(1) {
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    Rect::from_min_max(Pos2::new(min_x, min_y), Pos2::new(max_x, max_y))
}

pub(super) fn deform_mesh_center_scene(mesh_scene: &[Pos2]) -> Pos2 {
    let (sum_x, sum_y) = mesh_scene
        .iter()
        .fold((0.0f32, 0.0f32), |(acc_x, acc_y), p| {
            (acc_x + p.x, acc_y + p.y)
        });
    let count = mesh_scene.len().max(1) as f32;
    Pos2::new(sum_x / count, sum_y / count)
}

pub(super) fn rotate_mesh_scene(mesh_scene: &[Pos2], center: Pos2, angle_rad: f32) -> Vec<Pos2> {
    let (sin_a, cos_a) = angle_rad.sin_cos();
    mesh_scene
        .iter()
        .map(|point| {
            let dx = point.x - center.x;
            let dy = point.y - center.y;
            Pos2::new(
                center.x + dx * cos_a - dy * sin_a,
                center.y + dx * sin_a + dy * cos_a,
            )
        })
        .collect()
}

pub(super) fn overlay_uv_min() -> f32 {
    -TEXT_OVERLAY_MAX_OUT_OF_BOUNDS_UV
}

pub(super) fn overlay_uv_max() -> f32 {
    1.0 + TEXT_OVERLAY_MAX_OUT_OF_BOUNDS_UV
}

pub(super) fn clamp_overlay_uv_coord(value: f32) -> f32 {
    value.clamp(overlay_uv_min(), overlay_uv_max())
}

pub(super) fn clamp_overlay_page_coord(value: f32, side_px: usize) -> f32 {
    let side_px = side_px.max(1) as f32;
    value.clamp(overlay_uv_min() * side_px, overlay_uv_max() * side_px)
}

// ---------------------------------------------------------------------------
// Vector transform (Phase 3a) — pure page-px <-> normalized conversions.
//
// A text overlay's vector warp is authored over an ORIENTED source rectangle
// centered at `center_page_px` (page px), with content size `(src_w, src_h)`
// (content px) scaled by `scale` and rotated by `angle_deg`. A lattice node's
// normalized position `(u, v)` in `[0, 1]` maps to the page point
//   page = center + Rot(angle) * ((u - 0.5) * src_w * scale, (v - 0.5) * src_h * scale)
// and back. These match the `raster_transform` renderer contract (points_norm are
// normalized over the source rect; Design B normalizes the renderer box over the
// SAME src dims), so a page-px<->normalized round-trip of the identity grid yields
// the identity normalized grid (a renderer no-op).
// ---------------------------------------------------------------------------

/// Read a text overlay's `text_layout_mode` out of its `render_data` JSON (defaulting to
/// `Normal` when absent/unparseable). Pure.
pub(super) fn overlay_text_layout_mode(render_data: Option<&Value>) -> TextLayoutMode {
    render_data
        .and_then(|rd| rd.get("text_params"))
        .and_then(|tp| tp.get("text_layout_mode"))
        .and_then(Value::as_str)
        .and_then(parse_text_layout_mode_config_str)
        .unwrap_or(TextLayoutMode::Normal)
}

/// Whether the on-canvas VECTOR transform mode is allowed for a text overlay's layout mode.
///
/// Enabled for `Normal`, `Shape`, and `CustomVectorLines`; disabled for `Formula` and
/// `CustomRasterLines` (their layouts do not compose with the post-layout vector warp yet). Pure.
pub(super) fn vector_transform_allowed_for_layout_mode(mode: TextLayoutMode) -> bool {
    match mode {
        TextLayoutMode::Normal | TextLayoutMode::Shape | TextLayoutMode::CustomVectorLines => true,
        TextLayoutMode::Formula | TextLayoutMode::CustomRasterLines => false,
    }
}

/// Map a normalized `(u, v)` (source-rect space, `[0, 1]`) to a page-px point over the oriented
/// footprint centered at `center_page_px`, size `(src_w, src_h)` content px scaled by `scale` and
/// rotated by `angle_deg`. Pure.
pub(super) fn vector_footprint_page_point(
    center_page_px: [f32; 2],
    src_w: f32,
    src_h: f32,
    scale: f32,
    angle_deg: f32,
    u: f32,
    v: f32,
) -> [f32; 2] {
    let span_w = src_w * scale;
    let span_h = src_h * scale;
    let local_x = (u - 0.5) * span_w;
    let local_y = (v - 0.5) * span_h;
    let (sin_a, cos_a) = angle_deg.to_radians().sin_cos();
    [
        center_page_px[0] + local_x * cos_a - local_y * sin_a,
        center_page_px[1] + local_x * sin_a + local_y * cos_a,
    ]
}

/// Inverse of [`vector_footprint_page_point`]: map a page-px point to normalized `(u, v)` over the
/// oriented footprint. `src_w * scale` / `src_h * scale` are guarded against zero. Pure.
pub(super) fn vector_footprint_local_uv(
    center_page_px: [f32; 2],
    src_w: f32,
    src_h: f32,
    scale: f32,
    angle_deg: f32,
    page_point: [f32; 2],
) -> [f32; 2] {
    let dx = page_point[0] - center_page_px[0];
    let dy = page_point[1] - center_page_px[1];
    // Rotate by -angle to reach footprint-local space.
    let (sin_a, cos_a) = angle_deg.to_radians().sin_cos();
    let local_x = dx * cos_a + dy * sin_a;
    let local_y = -dx * sin_a + dy * cos_a;
    let span_x = (src_w * scale).abs().max(f32::EPSILON);
    let span_y = (src_h * scale).abs().max(f32::EPSILON);
    [local_x / span_x + 0.5, local_y / span_y + 0.5]
}

/// Bilinearly sample a stored `points_norm` grid (row-major, `cols*rows`) at identity coords
/// `(u, v)` in `[0, 1]`, returning the WARPED normalized position at that identity location. Used to
/// resample a stored warp of arbitrary resolution onto the fixed 13x13 working lattice. Clamps
/// out-of-range and returns `(u, v)` unchanged for a degenerate grid. Pure.
pub(super) fn sample_points_norm_bilinear(
    points_norm: &[[f32; 2]],
    cols: usize,
    rows: usize,
    u: f32,
    v: f32,
) -> [f32; 2] {
    if cols < 2 || rows < 2 || points_norm.len() != cols.saturating_mul(rows) {
        return [u, v];
    }
    let gx = (u.clamp(0.0, 1.0) * (cols - 1) as f32).clamp(0.0, (cols - 1) as f32);
    let gy = (v.clamp(0.0, 1.0) * (rows - 1) as f32).clamp(0.0, (rows - 1) as f32);
    let j0 = (gx.floor() as usize).min(cols - 2);
    let i0 = (gy.floor() as usize).min(rows - 2);
    let fx = (gx - j0 as f32).clamp(0.0, 1.0);
    let fy = (gy - i0 as f32).clamp(0.0, 1.0);
    let p00 = points_norm[i0 * cols + j0];
    let p10 = points_norm[i0 * cols + j0 + 1];
    let p01 = points_norm[(i0 + 1) * cols + j0];
    let p11 = points_norm[(i0 + 1) * cols + j0 + 1];
    [
        lerp(lerp(p00[0], p10[0], fx), lerp(p01[0], p11[0], fx), fy),
        lerp(lerp(p00[1], p10[1], fx), lerp(p01[1], p11[1], fx), fy),
    ]
}
