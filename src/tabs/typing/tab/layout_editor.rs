/*
File: tab/layout_editor.rs

Purpose:
Free-function helpers for the typing tab's layout editor: drawing the vector
lines list UI, editing/hit-testing vector line points on the canvas, the movable
layout frame, and converting between the in-memory editor state and the stored
vector-lines layout parameters / render data.

Main responsibilities:
- render the layout editor lines side panel and frame/point canvas interaction;
- add/remove editor lines and keep the active line valid;
- convert between `TypingLayoutEditorState` and `TextVectorLinesLayoutParams`;
- serialize the vector layout into render-data JSON and parse enum config values;
- geometry helpers for the layout frame and smoothed line rendering.

Notes:
Extracted verbatim from `tab.rs`. Free fns are `pub(super)` so `tab.rs` and
sibling submodules of `tab` can use them. `use super::*;` pulls in the parent
module's types and imports.
*/

use super::*;

pub(super) fn draw_layout_editor_vector_lines_tab(ui: &mut egui::Ui, editor: &mut TypingLayoutEditorState) {
    ensure_layout_editor_has_line(editor);
    ui.label(egui::RichText::new("Строки").strong());
    ui.add_space(6.0);
    egui::ScrollArea::vertical()
        .id_salt("typing_layout_editor_vector_lines_scroll")
        .show(ui, |ui| {
            let mut remove_idx: Option<usize> = None;
            for idx in 0..editor.lines.len() {
                let selected = editor.active_line_idx == idx;
                let frame = if selected {
                    egui::Frame::default()
                        .fill(Color32::from_rgb(45, 72, 98))
                        .stroke(Stroke::new(1.4, Color32::from_rgb(120, 210, 255)))
                } else {
                    egui::Frame::default()
                        .fill(Color32::from_rgb(38, 40, 44))
                        .stroke(Stroke::new(1.0, Color32::from_rgb(86, 90, 98)))
                };
                frame
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let label = editor
                                .lines
                                .get(idx)
                                .map(|line| line.label.as_str())
                                .unwrap_or("Строка");
                            if ui.selectable_label(selected, label).clicked() {
                                editor.active_line_idx = idx;
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if selected && ui.small_button("×").clicked() {
                                        remove_idx = Some(idx);
                                    }
                                },
                            );
                        });
                    });
                ui.add_space(5.0);
            }
            if let Some(idx) = remove_idx {
                remove_layout_editor_line(editor, idx);
            }
            let plus_response = egui::Frame::default()
                .fill(Color32::from_rgb(34, 35, 38))
                .stroke(Stroke::new(1.0, Color32::from_rgb(92, 96, 105)))
                .inner_margin(egui::Margin::symmetric(8, 8))
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        if ui.button("+").clicked() {
                            let next_idx = editor.lines.len() + 1;
                            editor.lines.push(TypingLayoutEditorLine {
                                label: format!("Строка {next_idx}"),
                                points: Vec::new(),
                                corner_smoothing_px: 0.0,
                                text_direction: TextVectorLineTextDirection::LeftToRight,
                                distance_mode: TextVectorLineDistanceMode::ByLineLength,
                                flip_text: false,
                            });
                            editor.active_line_idx = editor.lines.len().saturating_sub(1);
                        }
                    });
                });
            if plus_response.response.clicked() {
                let next_idx = editor.lines.len() + 1;
                editor.lines.push(TypingLayoutEditorLine {
                    label: format!("Строка {next_idx}"),
                    points: Vec::new(),
                    corner_smoothing_px: 0.0,
                    text_direction: TextVectorLineTextDirection::LeftToRight,
                    distance_mode: TextVectorLineDistanceMode::ByLineLength,
                    flip_text: false,
                });
                editor.active_line_idx = editor.lines.len().saturating_sub(1);
            }
        });
    ui.separator();
    ui.label(egui::RichText::new("Параметры строки").strong());
    if let Some(line) = editor.lines.get_mut(editor.active_line_idx) {
        ui.add(WheelSlider::new(&mut line.corner_smoothing_px, 0.0..=256.0).text("Сглаживание"));
        egui::ComboBox::from_label("Направление текста")
            .selected_text(vector_line_text_direction_label(line.text_direction))
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut line.text_direction,
                    TextVectorLineTextDirection::LeftToRight,
                    vector_line_text_direction_label(TextVectorLineTextDirection::LeftToRight),
                );
                ui.selectable_value(
                    &mut line.text_direction,
                    TextVectorLineTextDirection::RightToLeft,
                    vector_line_text_direction_label(TextVectorLineTextDirection::RightToLeft),
                );
            });
        egui::ComboBox::from_label("Режим расстояния")
            .selected_text(vector_line_distance_mode_label(line.distance_mode))
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut line.distance_mode,
                    TextVectorLineDistanceMode::ByLineLength,
                    vector_line_distance_mode_label(TextVectorLineDistanceMode::ByLineLength),
                );
                ui.selectable_value(
                    &mut line.distance_mode,
                    TextVectorLineDistanceMode::MinimumPreviousDistance,
                    vector_line_distance_mode_label(
                        TextVectorLineDistanceMode::MinimumPreviousDistance,
                    ),
                );
            });
        ui.checkbox(&mut line.flip_text, "Перевернуть текст");
    }
}

pub(super) fn vector_line_text_direction_label(direction: TextVectorLineTextDirection) -> &'static str {
    match direction {
        TextVectorLineTextDirection::LeftToRight => "Слева направо",
        TextVectorLineTextDirection::RightToLeft => "Справа налево",
    }
}

pub(super) fn vector_line_distance_mode_label(mode: TextVectorLineDistanceMode) -> &'static str {
    match mode {
        TextVectorLineDistanceMode::ByLineLength => "По длине линии",
        TextVectorLineDistanceMode::MinimumPreviousDistance => "Мин. расстояние до символа",
    }
}

pub(super) fn ensure_layout_editor_has_line(editor: &mut TypingLayoutEditorState) {
    if editor.lines.is_empty() {
        editor.lines.push(TypingLayoutEditorLine {
            label: "Строка 1".to_string(),
            points: Vec::new(),
            corner_smoothing_px: 0.0,
            text_direction: TextVectorLineTextDirection::LeftToRight,
            distance_mode: TextVectorLineDistanceMode::ByLineLength,
            flip_text: false,
        });
    }
    editor.active_line_idx = editor
        .active_line_idx
        .min(editor.lines.len().saturating_sub(1));
}

pub(super) fn remove_layout_editor_line(editor: &mut TypingLayoutEditorState, idx: usize) {
    if editor.lines.len() <= 1 {
        if let Some(line) = editor.lines.first_mut() {
            line.points.clear();
            line.corner_smoothing_px = 0.0;
            line.text_direction = TextVectorLineTextDirection::LeftToRight;
            line.distance_mode = TextVectorLineDistanceMode::ByLineLength;
            line.flip_text = false;
        }
        editor.active_line_idx = 0;
        return;
    }
    if idx < editor.lines.len() {
        editor.lines.remove(idx);
    }
    for (line_idx, line) in editor.lines.iter_mut().enumerate() {
        line.label = format!("Строка {}", line_idx + 1);
    }
    editor.active_line_idx = editor
        .active_line_idx
        .min(editor.lines.len().saturating_sub(1));
}

pub(super) fn layout_editor_lines_from_vector_layout(
    layout: TextVectorLinesLayoutParams,
) -> Vec<TypingLayoutEditorLine> {
    layout
        .lines
        .into_iter()
        .enumerate()
        .map(|(idx, line)| TypingLayoutEditorLine {
            label: format!("Строка {}", idx + 1),
            points: line
                .points
                .into_iter()
                .map(|point| egui::pos2(point.x, point.y))
                .collect(),
            corner_smoothing_px: line.corner_smoothing_px.clamp(0.0, 256.0),
            text_direction: line.text_direction,
            distance_mode: line.distance_mode,
            flip_text: line.flip_text,
        })
        .collect()
}

pub(super) fn vector_lines_layout_from_editor(
    editor: &TypingLayoutEditorState,
) -> TextVectorLinesLayoutParams {
    let width_px = rounded_positive_f32_to_u32(editor.frame_page_rect.width());
    let height_px = rounded_positive_f32_to_u32(editor.frame_page_rect.height());
    let max_x = width_px as f32;
    let max_y = height_px as f32;
    let lines = editor
        .lines
        .iter()
        .map(|line| TextVectorLine {
            points: line
                .points
                .iter()
                .map(|point| TextVectorPoint {
                    x: point.x.clamp(0.0, max_x),
                    y: point.y.clamp(0.0, max_y),
                })
                .collect(),
            corner_smoothing_px: line.corner_smoothing_px.clamp(0.0, 256.0),
            text_direction: line.text_direction,
            distance_mode: line.distance_mode,
            flip_text: line.flip_text,
        })
        .collect();
    TextVectorLinesLayoutParams {
        width_px,
        height_px,
        lines,
        ..TextVectorLinesLayoutParams::default()
    }
}

pub(super) fn render_data_with_vector_layout(
    render_data: &Value,
    layout: &TextVectorLinesLayoutParams,
) -> Option<Value> {
    let mut updated = render_data.clone();
    let obj = updated.as_object_mut()?;
    let text_params = obj.get_mut("text_params")?.as_object_mut()?;
    text_params.insert(
        "text_layout_mode".to_string(),
        Value::from("custom_vector_lines"),
    );
    text_params.insert("text_line_mode".to_string(), Value::from("horizontal"));
    text_params.insert("width_px".to_string(), Value::from(layout.width_px.max(1)));
    text_params.insert(
        "vector_lines_layout".to_string(),
        vector_lines_layout_to_value_for_render_data(layout),
    );
    Some(updated)
}

pub(super) fn vector_lines_layout_to_value_for_render_data(layout: &TextVectorLinesLayoutParams) -> Value {
    let lines = layout
        .lines
        .iter()
        .map(|line| {
            let points = line
                .points
                .iter()
                .map(|point| json!({ "x": point.x, "y": point.y }))
                .collect::<Vec<_>>();
            json!({
                "points": points,
                "corner_smoothing_px": line.corner_smoothing_px,
                "text_direction": vector_line_text_direction_to_str(line.text_direction),
                "distance_mode": vector_line_distance_mode_to_str(line.distance_mode),
                "flip_text": line.flip_text,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "width_px": layout.width_px.max(1),
        "height_px": layout.height_px.max(1),
        "use_tangent_rotation": layout.use_tangent_rotation,
        "static_rotation_rad": layout.static_rotation_rad,
        "normal_offset_px": layout.normal_offset_px,
        "letter_spacing_mul": layout.letter_spacing_mul,
        "letter_spacing_px": layout.letter_spacing_px,
        "lines": lines,
    })
}

pub(super) fn vector_line_text_direction_to_str(direction: TextVectorLineTextDirection) -> &'static str {
    match direction {
        TextVectorLineTextDirection::LeftToRight => "left_to_right",
        TextVectorLineTextDirection::RightToLeft => "right_to_left",
    }
}

pub(super) fn vector_line_text_direction_from_value(value: Option<&Value>) -> TextVectorLineTextDirection {
    match value.and_then(Value::as_str).unwrap_or("left_to_right") {
        "right_to_left" | "rtl" => TextVectorLineTextDirection::RightToLeft,
        "left_to_right" | "ltr" => TextVectorLineTextDirection::LeftToRight,
        _ => TextVectorLineTextDirection::LeftToRight,
    }
}

pub(super) fn vector_line_distance_mode_to_str(mode: TextVectorLineDistanceMode) -> &'static str {
    match mode {
        TextVectorLineDistanceMode::ByLineLength => "by_line_length",
        TextVectorLineDistanceMode::MinimumPreviousDistance => "minimum_previous_distance",
    }
}

pub(super) fn vector_line_distance_mode_from_value(value: Option<&Value>) -> TextVectorLineDistanceMode {
    match value.and_then(Value::as_str).unwrap_or("by_line_length") {
        "minimum_previous_distance" | "min_previous_distance" | "minimum_distance" => {
            TextVectorLineDistanceMode::MinimumPreviousDistance
        }
        "by_line_length" | "line_length" => TextVectorLineDistanceMode::ByLineLength,
        _ => TextVectorLineDistanceMode::ByLineLength,
    }
}

pub(super) fn rounded_positive_f32_to_u32(value: f32) -> u32 {
    let rounded = value.round().clamp(1.0, u32::MAX as f32);
    rounded as u32
}

pub(super) fn frame_rect_from_center_and_size(center: Pos2, size: Vec2, page_size: [usize; 2]) -> Rect {
    let page_w = page_size[0].max(1) as f32;
    let page_h = page_size[1].max(1) as f32;
    let width = size.x.clamp(1.0, page_w);
    let height = size.y.clamp(1.0, page_h);
    let min_x = (center.x - width * 0.5).clamp(0.0, (page_w - width).max(0.0));
    let min_y = (center.y - height * 0.5).clamp(0.0, (page_h - height).max(0.0));
    Rect::from_min_size(Pos2::new(min_x, min_y), Vec2::new(width, height))
}

pub(super) fn layout_editor_frame_scene_rect(frame_page_rect: Rect, image_rect: Rect, zoom: f32) -> Rect {
    Rect::from_min_max(
        scene_from_page_px(
            image_rect,
            zoom,
            [frame_page_rect.min.x, frame_page_rect.min.y],
        ),
        scene_from_page_px(
            image_rect,
            zoom,
            [frame_page_rect.max.x, frame_page_rect.max.y],
        ),
    )
}

pub(super) fn layout_frame_handle_points(rect: Rect) -> [(TypingLayoutFrameHandle, Pos2); 8] {
    [
        (TypingLayoutFrameHandle::TopLeft, rect.left_top()),
        (
            TypingLayoutFrameHandle::Top,
            egui::pos2(rect.center().x, rect.top()),
        ),
        (TypingLayoutFrameHandle::TopRight, rect.right_top()),
        (
            TypingLayoutFrameHandle::Right,
            egui::pos2(rect.right(), rect.center().y),
        ),
        (TypingLayoutFrameHandle::BottomRight, rect.right_bottom()),
        (
            TypingLayoutFrameHandle::Bottom,
            egui::pos2(rect.center().x, rect.bottom()),
        ),
        (TypingLayoutFrameHandle::BottomLeft, rect.left_bottom()),
        (
            TypingLayoutFrameHandle::Left,
            egui::pos2(rect.left(), rect.center().y),
        ),
    ]
}

pub(super) fn apply_layout_frame_drag(
    start_rect: Rect,
    handle: TypingLayoutFrameHandle,
    delta: Vec2,
    page_size: [usize; 2],
) -> Rect {
    let mut min = start_rect.min;
    let mut max = start_rect.max;
    match handle {
        TypingLayoutFrameHandle::TopLeft => {
            min += delta;
        }
        TypingLayoutFrameHandle::Top => {
            min.y += delta.y;
        }
        TypingLayoutFrameHandle::TopRight => {
            max.x += delta.x;
            min.y += delta.y;
        }
        TypingLayoutFrameHandle::Right => {
            max.x += delta.x;
        }
        TypingLayoutFrameHandle::BottomRight => {
            max += delta;
        }
        TypingLayoutFrameHandle::Bottom => {
            max.y += delta.y;
        }
        TypingLayoutFrameHandle::BottomLeft => {
            min.x += delta.x;
            max.y += delta.y;
        }
        TypingLayoutFrameHandle::Left => {
            min.x += delta.x;
        }
    }
    let page_w = page_size[0].max(1) as f32;
    let page_h = page_size[1].max(1) as f32;
    min.x = min.x.clamp(0.0, page_w);
    max.x = max.x.clamp(0.0, page_w);
    min.y = min.y.clamp(0.0, page_h);
    max.y = max.y.clamp(0.0, page_h);
    if max.x - min.x < TEXT_LAYOUT_EDITOR_FRAME_MIN_SIDE_PX {
        match handle {
            TypingLayoutFrameHandle::TopLeft
            | TypingLayoutFrameHandle::Left
            | TypingLayoutFrameHandle::BottomLeft => {
                min.x = (max.x - TEXT_LAYOUT_EDITOR_FRAME_MIN_SIDE_PX).max(0.0);
            }
            TypingLayoutFrameHandle::TopRight
            | TypingLayoutFrameHandle::Right
            | TypingLayoutFrameHandle::BottomRight => {
                max.x = (min.x + TEXT_LAYOUT_EDITOR_FRAME_MIN_SIDE_PX).min(page_w);
            }
            TypingLayoutFrameHandle::Top | TypingLayoutFrameHandle::Bottom => {}
        }
    }
    if max.y - min.y < TEXT_LAYOUT_EDITOR_FRAME_MIN_SIDE_PX {
        match handle {
            TypingLayoutFrameHandle::TopLeft
            | TypingLayoutFrameHandle::Top
            | TypingLayoutFrameHandle::TopRight => {
                min.y = (max.y - TEXT_LAYOUT_EDITOR_FRAME_MIN_SIDE_PX).max(0.0);
            }
            TypingLayoutFrameHandle::BottomLeft
            | TypingLayoutFrameHandle::Bottom
            | TypingLayoutFrameHandle::BottomRight => {
                max.y = (min.y + TEXT_LAYOUT_EDITOR_FRAME_MIN_SIDE_PX).min(page_h);
            }
            TypingLayoutFrameHandle::Left | TypingLayoutFrameHandle::Right => {}
        }
    }
    Rect::from_min_max(min, max)
}

pub(super) fn handle_layout_editor_vector_canvas_input(
    editor: &mut TypingLayoutEditorState,
    line_idx: usize,
    frame_scene: Rect,
    image_rect: Rect,
    zoom: f32,
    response: &egui::Response,
    ctx: &egui::Context,
) {
    if ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Delete))
        && let Some(line) = editor.lines.get_mut(line_idx)
    {
        let _ = line.points.pop();
        ctx.request_repaint();
    }

    let Some(pointer_scene) = response.interact_pointer_pos() else {
        return;
    };
    let pointer_page = page_px_from_scene(image_rect, zoom, pointer_scene);
    let local = egui::pos2(
        (pointer_page[0] - editor.frame_page_rect.left())
            .clamp(0.0, editor.frame_page_rect.width().max(1.0)),
        (pointer_page[1] - editor.frame_page_rect.top())
            .clamp(0.0, editor.frame_page_rect.height().max(1.0)),
    );
    if response.clicked()
        && frame_scene.contains(pointer_scene)
        && let Some(line) = editor.lines.get_mut(line_idx)
    {
        let shift_creates_next = ctx.input(|input| input.modifiers.shift)
            && hit_test_layout_editor_line_point(line, frame_scene, zoom, pointer_scene)
                == line.points.len().checked_sub(1);
        if line.points.is_empty() || shift_creates_next {
            line.points.push(local);
            ctx.request_repaint();
        }
    }
    if response.drag_started()
        && let Some(line) = editor.lines.get_mut(line_idx)
    {
        let hit_point_idx =
            hit_test_layout_editor_line_point(line, frame_scene, zoom, pointer_scene);
        let shift_pressed = ctx.input(|input| input.modifiers.shift);
        let last_point_idx = line.points.len().checked_sub(1);
        if shift_pressed && hit_point_idx.is_some() && hit_point_idx == last_point_idx {
            line.points.push(local);
            editor.line_drag = Some(TypingLayoutLineDragState {
                line_idx,
                point_idx: line.points.len().saturating_sub(1),
            });
            ctx.request_repaint();
        } else if let Some(point_idx) = hit_point_idx {
            editor.line_drag = Some(TypingLayoutLineDragState {
                line_idx,
                point_idx,
            });
            ctx.request_repaint();
        }
    }
    if response.dragged()
        && let Some(drag) = editor.line_drag
        && let Some(line) = editor.lines.get_mut(drag.line_idx)
        && let Some(point) = line.points.get_mut(drag.point_idx)
    {
        *point = local;
        ctx.request_repaint();
    }
    if response.drag_stopped() {
        editor.line_drag = None;
    }
}

pub(super) fn clamp_layout_editor_points_to_frame(editor: &mut TypingLayoutEditorState) {
    let max_x = editor.frame_page_rect.width().max(1.0);
    let max_y = editor.frame_page_rect.height().max(1.0);
    for line in &mut editor.lines {
        for point in &mut line.points {
            point.x = point.x.clamp(0.0, max_x);
            point.y = point.y.clamp(0.0, max_y);
        }
    }
}

pub(super) fn hit_test_layout_editor_line_point(
    line: &TypingLayoutEditorLine,
    frame_scene: Rect,
    zoom: f32,
    pointer_scene: Pos2,
) -> Option<usize> {
    line.points
        .iter()
        .enumerate()
        .rev()
        .find(|(_, point)| {
            layout_line_point_scene(frame_scene, **point, zoom).distance(pointer_scene)
                <= TEXT_LAYOUT_EDITOR_POINT_RADIUS_PX * 2.2
        })
        .map(|(point_idx, _)| point_idx)
}

pub(super) fn layout_line_point_scene(frame_scene: Rect, point: Pos2, zoom: f32) -> Pos2 {
    egui::pos2(
        frame_scene.left() + point.x * zoom,
        frame_scene.top() + point.y * zoom,
    )
}

pub(super) fn draw_layout_editor_frame(painter: &egui::Painter, rect: Rect) {
    painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(20, 32, 46, 36));
    painter.rect_stroke(
        rect,
        0.0,
        Stroke::new(2.0, Color32::from_rgb(92, 210, 255)),
        egui::StrokeKind::Outside,
    );
    for (handle, pos) in layout_frame_handle_points(rect) {
        let is_corner = matches!(
            handle,
            TypingLayoutFrameHandle::TopLeft
                | TypingLayoutFrameHandle::TopRight
                | TypingLayoutFrameHandle::BottomRight
                | TypingLayoutFrameHandle::BottomLeft
        );
        let color = if is_corner {
            Color32::from_rgb(255, 220, 90)
        } else {
            Color32::from_rgb(118, 225, 255)
        };
        painter.rect_filled(Rect::from_center_size(pos, Vec2::splat(10.0)), 1.5, color);
        painter.rect_stroke(
            Rect::from_center_size(pos, Vec2::splat(10.0)),
            1.5,
            Stroke::new(1.0, Color32::from_rgb(12, 20, 28)),
            egui::StrokeKind::Outside,
        );
    }
}

pub(super) fn draw_layout_editor_vector_lines(
    painter: &egui::Painter,
    frame_scene: Rect,
    zoom: f32,
    editor: &TypingLayoutEditorState,
) {
    for (line_idx, line) in editor.lines.iter().enumerate() {
        let active = line_idx == editor.active_line_idx;
        let line_color = if active {
            layout_editor_active_line_color(line_idx)
        } else {
            Color32::from_rgba_unmultiplied(165, 170, 178, 145)
        };
        let point_color = if active {
            Color32::from_rgb(255, 245, 110)
        } else {
            Color32::from_rgba_unmultiplied(178, 182, 188, 150)
        };
        let raw_line_color = if active {
            Color32::from_rgba_unmultiplied(line_color.r(), line_color.g(), line_color.b(), 110)
        } else {
            Color32::from_rgba_unmultiplied(140, 145, 152, 85)
        };
        for pair in line.points.windows(2) {
            painter.line_segment(
                [
                    layout_line_point_scene(frame_scene, pair[0], zoom),
                    layout_line_point_scene(frame_scene, pair[1], zoom),
                ],
                Stroke::new(if active { 1.2 } else { 0.9 }, raw_line_color),
            );
        }
        let smoothed_points = smoothed_layout_editor_line_points(line);
        for pair in smoothed_points.windows(2) {
            painter.line_segment(
                [
                    layout_line_point_scene(frame_scene, pair[0], zoom),
                    layout_line_point_scene(frame_scene, pair[1], zoom),
                ],
                Stroke::new(if active { 2.8 } else { 1.4 }, line_color),
            );
        }
        for (point_idx, point) in line.points.iter().enumerate() {
            let scene = layout_line_point_scene(frame_scene, *point, zoom);
            draw_layout_editor_line_point(
                painter,
                scene,
                point_color,
                point_idx,
                line.points.len(),
                active,
            );
        }
    }
}

pub(super) fn smoothed_layout_editor_line_points(line: &TypingLayoutEditorLine) -> Vec<Pos2> {
    let points = line
        .points
        .iter()
        .map(|point| TextVectorPoint {
            x: point.x,
            y: point.y,
        })
        .collect::<Vec<_>>();
    super::super::render_next::drawn_lines::smooth_vector_points(
        points.as_slice(),
        line.corner_smoothing_px,
    )
    .into_iter()
    .map(|point| Pos2::new(point.x, point.y))
    .collect()
}

pub(super) fn draw_layout_editor_line_point(
    painter: &egui::Painter,
    center: Pos2,
    color: Color32,
    point_idx: usize,
    point_count: usize,
    active: bool,
) {
    let radius = if active {
        TEXT_LAYOUT_EDITOR_POINT_RADIUS_PX
    } else {
        TEXT_LAYOUT_EDITOR_POINT_RADIUS_PX - 1.5
    };
    if point_idx == 0 && point_count > 1 {
        painter.circle_filled(center, radius + 2.0, Color32::from_rgb(20, 28, 38));
        painter.circle_stroke(center, radius + 2.0, Stroke::new(2.0, color));
        painter.circle_filled(center, radius - 2.0, color);
    } else if point_idx + 1 == point_count {
        painter.rect_filled(
            Rect::from_center_size(center, Vec2::splat(radius * 2.0)),
            1.5,
            color,
        );
        painter.rect_stroke(
            Rect::from_center_size(center, Vec2::splat(radius * 2.0)),
            1.5,
            Stroke::new(1.0, Color32::from_rgb(20, 28, 38)),
            egui::StrokeKind::Outside,
        );
    } else {
        painter.circle_filled(center, radius, color);
        painter.circle_stroke(
            center,
            radius,
            Stroke::new(1.0, Color32::from_rgb(20, 28, 38)),
        );
    }
}

pub(super) fn layout_editor_active_line_color(line_idx: usize) -> Color32 {
    const COLORS: [Color32; 12] = [
        Color32::from_rgb(255, 64, 64),
        Color32::from_rgb(255, 150, 40),
        Color32::from_rgb(240, 205, 70),
        Color32::from_rgb(74, 220, 96),
        Color32::from_rgb(35, 220, 190),
        Color32::from_rgb(70, 190, 255),
        Color32::from_rgb(80, 110, 255),
        Color32::from_rgb(170, 90, 255),
        Color32::from_rgb(255, 70, 170),
        Color32::from_rgb(180, 115, 60),
        Color32::from_rgb(190, 195, 205),
        Color32::from_rgb(170, 35, 70),
    ];
    COLORS[line_idx % COLORS.len()]
}
