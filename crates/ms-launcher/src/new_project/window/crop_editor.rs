/*
File: crates/ms-launcher/src/new_project/window/crop_editor.rs

Purpose:
Crop editor window of the New Project window: opening it for one ribbon page, the window body,
the crop overlay and handles, and the handle-drag math.

Key items:
- `open_crop_editor`, `show_crop_editor_window`.
- `draw_crop_editor_canvas`, `crop_handle_rects`, `process_crop_drag_response`, `apply_crop_drag`.

Notes:
Crops are non-destructive `RibbonCrop` metadata on the page (`ribbon.rs`).
Part of the `window` module split: methods are `impl NewProjectWindowState` blocks over the
type declared in `mod.rs`; items are `pub(super)` and reach siblings through `mod.rs`.
*/

use super::*;

impl NewProjectWindowState {
    pub(super) fn open_crop_editor(&mut self, index: usize) {
        let Some(page) = self.ribbon.pages().get(index) else {
            return;
        };
        let source_image = page.source_image();
        let source_size = page.source_size();
        self.crop_editor = Some(CropEditorState {
            page_index: index,
            page_name: page.name.clone(),
            source_size,
            crop_rect: page.crop().unwrap_or(RibbonCrop {
                left: 0,
                top: 0,
                width: source_size[0].max(1),
                height: source_size[1].max(1),
            }),
            tiles: build_ribbon_tiles(source_image.as_ref()),
            drag_state: None,
            window_rect: None,
        });
    }

    pub(super) fn show_crop_editor_window(&mut self, ctx: &egui::Context) {
        let Some(editor) = self.crop_editor.as_mut() else {
            return;
        };
        let viewport = ctx.content_rect().shrink(16.0);
        let max_window_size = egui::vec2(viewport.width(), viewport.height() * 0.8);
        let default_size = egui::vec2(
            viewport.width().clamp(CROP_WINDOW_MIN_SIZE.x, 920.0),
            max_window_size.y.clamp(CROP_WINDOW_MIN_SIZE.y, 760.0),
        );
        let window_size = editor
            .window_rect
            .map(|rect| rect.size())
            .unwrap_or(default_size)
            .min(max_window_size);
        let fallback_pos = egui::pos2(
            viewport.center().x - window_size.x * 0.5,
            viewport.center().y - window_size.y * 0.5,
        );
        let window_pos = clamp_window_pos_to_viewport(
            editor
                .window_rect
                .map(|rect| rect.min)
                .unwrap_or(fallback_pos),
            window_size,
            viewport,
        );

        let mut keep_open = true;
        let mut request_apply = false;
        let shown = Window::new(tf!("launcher.new_project.crop_editor_title", editor = editor.page_name))
            .id(egui::Id::new((
                "launcher_new_project_crop",
                editor.page_index,
            )))
            .default_size(default_size)
            .current_pos(window_pos)
            .min_size(CROP_WINDOW_MIN_SIZE.min(max_window_size))
            .max_size(max_window_size)
            .collapsible(false)
            .resizable(true)
            .constrain_to(viewport)
            .open(&mut keep_open)
            .show(ctx, |ui| {
                ui.set_min_size(CROP_WINDOW_MIN_SIZE.min(max_window_size));
                ui.label(
                    RichText::new(
                        t!("launcher.new_project.crop_editor_hint"),
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(8.0);
                ScrollArea::both()
                    .id_salt(("launcher_new_project_crop_scroll", editor.page_index))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        draw_crop_editor_canvas(ui, editor);
                    });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(t!("launcher.new_project.reset_button")).clicked() {
                        editor.crop_rect = RibbonCrop {
                            left: 0,
                            top: 0,
                            width: editor.source_size[0].max(1),
                            height: editor.source_size[1].max(1),
                        };
                        editor.drag_state = None;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_sized(egui::vec2(132.0, 32.0), Button::new(t!("launcher.new_project.apply_button")))
                            .clicked()
                        {
                            request_apply = true;
                        }
                    });
                });
            });
        if let Some(shown) = shown {
            editor.window_rect = Some(shown.response.rect);
        }

        if request_apply {
            let page_index = editor.page_index;
            let page_name = editor.page_name.clone();
            let crop_rect = editor.crop_rect;
            if self.ribbon.apply_crop(page_index, crop_rect) {
                self.clamp_manual_cut_guides_to_current_pages();
                self.import_status = tf!("launcher.new_project.image_cropped_status", page_name = page_name);
                self.last_error = None;
            } else {
                self.last_error = Some(t!("launcher.new_project.apply_crop_error").to_string());
            }
            self.crop_editor = None;
            return;
        }

        if !keep_open {
            self.crop_editor = None;
        }
    }
}

pub(super) fn clamp_window_pos_to_viewport(
    desired_pos: egui::Pos2,
    window_size: egui::Vec2,
    viewport: egui::Rect,
) -> egui::Pos2 {
    egui::pos2(
        desired_pos.x.clamp(
            viewport.left(),
            (viewport.right() - window_size.x).max(viewport.left()),
        ),
        desired_pos.y.clamp(
            viewport.top(),
            (viewport.bottom() - window_size.y).max(viewport.top()),
        ),
    )
}

pub(super) fn draw_crop_editor_canvas(ui: &mut Ui, editor: &mut CropEditorState) {
    let image_size = egui::vec2(editor.source_size[0] as f32, editor.source_size[1] as f32);
    let (image_rect, _) = ui.allocate_exact_size(image_size, egui::Sense::hover());
    paint_ribbon_tiles(
        ui,
        image_rect,
        1.0,
        &mut editor.tiles,
        &format!("launcher-new-project-crop-{}", editor.page_index),
        &mut ribbon_upload_budget(),
    );

    let crop_rect_screen = crop_rect_to_screen(editor.crop_rect, image_rect);
    paint_crop_overlay(ui, image_rect, crop_rect_screen);
    paint_crop_handles(ui, crop_rect_screen);
    handle_crop_drag(ui, image_rect, editor, crop_rect_screen);
}

pub(super) fn crop_rect_to_screen(crop_rect: RibbonCrop, image_rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(
            image_rect.left() + crop_rect.left as f32,
            image_rect.top() + crop_rect.top as f32,
        ),
        egui::pos2(
            image_rect.left() + (crop_rect.left + crop_rect.width) as f32,
            image_rect.top() + (crop_rect.top + crop_rect.height) as f32,
        ),
    )
}

pub(super) fn paint_crop_overlay(ui: &Ui, image_rect: egui::Rect, crop_rect_screen: egui::Rect) {
    let painter = ui.painter();
    let shade = egui::Color32::from_rgba_premultiplied(0, 0, 0, 110);
    let top = egui::Rect::from_min_max(
        image_rect.min,
        egui::pos2(image_rect.right(), crop_rect_screen.top()),
    );
    let bottom = egui::Rect::from_min_max(
        egui::pos2(image_rect.left(), crop_rect_screen.bottom()),
        image_rect.max,
    );
    let left = egui::Rect::from_min_max(
        egui::pos2(image_rect.left(), crop_rect_screen.top()),
        egui::pos2(crop_rect_screen.left(), crop_rect_screen.bottom()),
    );
    let right = egui::Rect::from_min_max(
        egui::pos2(crop_rect_screen.right(), crop_rect_screen.top()),
        egui::pos2(image_rect.right(), crop_rect_screen.bottom()),
    );
    for rect in [top, bottom, left, right] {
        if rect.is_positive() {
            painter.rect_filled(rect, 0.0, shade);
        }
    }
    painter.rect_stroke(
        crop_rect_screen,
        8.0,
        Stroke::new(2.0, egui::Color32::from_rgb(255, 168, 61)),
        egui::StrokeKind::Inside,
    );
}

pub(super) fn paint_crop_handles(ui: &Ui, crop_rect_screen: egui::Rect) {
    let painter = ui.painter();
    for (_, handle_rect) in crop_handle_rects(crop_rect_screen) {
        painter.rect_filled(handle_rect, 4.0, egui::Color32::from_rgb(255, 168, 61));
        painter.rect_stroke(
            handle_rect,
            4.0,
            Stroke::new(1.0, egui::Color32::from_rgb(70, 37, 6)),
            egui::StrokeKind::Inside,
        );
    }
}

pub(super) fn handle_crop_drag(
    ui: &mut Ui,
    image_rect: egui::Rect,
    editor: &mut CropEditorState,
    crop_rect_screen: egui::Rect,
) {
    let handle_rects = crop_handle_rects(crop_rect_screen);
    for (handle, rect) in handle_rects {
        process_crop_drag_response(ui, image_rect, editor, rect, handle);
    }
    let move_rect = crop_rect_screen.shrink(CROP_HANDLE_SIZE * 0.75);
    if move_rect.is_positive() {
        process_crop_drag_response(ui, image_rect, editor, move_rect, CropHandle::Move);
    }
}

pub(super) fn crop_handle_rects(crop_rect_screen: egui::Rect) -> [(CropHandle, egui::Rect); 8] {
    let center = crop_rect_screen.center();
    [
        (
            CropHandle::TopLeft,
            egui::Rect::from_center_size(
                crop_rect_screen.left_top(),
                egui::vec2(CROP_HANDLE_SIZE, CROP_HANDLE_SIZE),
            ),
        ),
        (
            CropHandle::Top,
            egui::Rect::from_center_size(
                egui::pos2(center.x, crop_rect_screen.top()),
                egui::vec2(CROP_HANDLE_SIZE * 1.4, CROP_HANDLE_SIZE),
            ),
        ),
        (
            CropHandle::TopRight,
            egui::Rect::from_center_size(
                crop_rect_screen.right_top(),
                egui::vec2(CROP_HANDLE_SIZE, CROP_HANDLE_SIZE),
            ),
        ),
        (
            CropHandle::Right,
            egui::Rect::from_center_size(
                egui::pos2(crop_rect_screen.right(), center.y),
                egui::vec2(CROP_HANDLE_SIZE, CROP_HANDLE_SIZE * 1.4),
            ),
        ),
        (
            CropHandle::BottomRight,
            egui::Rect::from_center_size(
                crop_rect_screen.right_bottom(),
                egui::vec2(CROP_HANDLE_SIZE, CROP_HANDLE_SIZE),
            ),
        ),
        (
            CropHandle::Bottom,
            egui::Rect::from_center_size(
                egui::pos2(center.x, crop_rect_screen.bottom()),
                egui::vec2(CROP_HANDLE_SIZE * 1.4, CROP_HANDLE_SIZE),
            ),
        ),
        (
            CropHandle::BottomLeft,
            egui::Rect::from_center_size(
                crop_rect_screen.left_bottom(),
                egui::vec2(CROP_HANDLE_SIZE, CROP_HANDLE_SIZE),
            ),
        ),
        (
            CropHandle::Left,
            egui::Rect::from_center_size(
                egui::pos2(crop_rect_screen.left(), center.y),
                egui::vec2(CROP_HANDLE_SIZE, CROP_HANDLE_SIZE * 1.4),
            ),
        ),
    ]
}

pub(super) fn process_crop_drag_response(
    ui: &mut Ui,
    image_rect: egui::Rect,
    editor: &mut CropEditorState,
    drag_rect: egui::Rect,
    handle: CropHandle,
) {
    let response = ui.interact(
        drag_rect,
        ui.id()
            .with(("launcher_new_project_crop_drag", editor.page_index, handle)),
        egui::Sense::drag(),
    );
    if response.drag_started()
        && let Some(pointer_pos) = ui.ctx().pointer_interact_pos()
    {
        editor.drag_state = Some(CropDragState {
            handle,
            start_rect: editor.crop_rect,
            start_pointer_px: egui::pos2(
                pointer_pos.x - image_rect.left(),
                pointer_pos.y - image_rect.top(),
            ),
        });
    }
    if response.dragged()
        && let Some(drag_state) = editor.drag_state
        && drag_state.handle == handle
        && let Some(pointer_pos) = ui.ctx().pointer_interact_pos()
    {
        let current_pointer_px = egui::pos2(
            pointer_pos.x - image_rect.left(),
            pointer_pos.y - image_rect.top(),
        );
        editor.crop_rect = apply_crop_drag(
            drag_state.start_rect,
            drag_state.handle,
            drag_state.start_pointer_px,
            current_pointer_px,
            editor.source_size,
            CROP_MIN_SIDE,
        );
    }
    if response.drag_stopped()
        && editor
            .drag_state
            .is_some_and(|state| state.handle == handle)
    {
        editor.drag_state = None;
    }
}

pub(super) fn apply_crop_drag(
    start_rect: RibbonCrop,
    handle: CropHandle,
    start_pointer_px: egui::Pos2,
    current_pointer_px: egui::Pos2,
    source_size: [usize; 2],
    min_side_px: usize,
) -> RibbonCrop {
    let delta_x = (current_pointer_px.x - start_pointer_px.x).round() as i64;
    let delta_y = (current_pointer_px.y - start_pointer_px.y).round() as i64;
    let min_side = i64::try_from(
        min_side_px
            .min(source_size[0].max(1))
            .min(source_size[1].max(1)),
    )
    .unwrap_or(1);
    let mut left = i64::try_from(start_rect.left).unwrap_or(0);
    let mut top = i64::try_from(start_rect.top).unwrap_or(0);
    let mut right = i64::try_from(start_rect.left + start_rect.width).unwrap_or(i64::MAX);
    let mut bottom = i64::try_from(start_rect.top + start_rect.height).unwrap_or(i64::MAX);
    let max_width = i64::try_from(source_size[0].max(1)).unwrap_or(i64::MAX);
    let max_height = i64::try_from(source_size[1].max(1)).unwrap_or(i64::MAX);

    if handle == CropHandle::Move {
        let width = right - left;
        let height = bottom - top;
        left = (left + delta_x).clamp(0, max_width - width);
        top = (top + delta_y).clamp(0, max_height - height);
        right = left + width;
        bottom = top + height;
    } else {
        if matches!(
            handle,
            CropHandle::Left | CropHandle::TopLeft | CropHandle::BottomLeft
        ) {
            left = (left + delta_x).clamp(0, right - min_side);
        }
        if matches!(
            handle,
            CropHandle::Right | CropHandle::TopRight | CropHandle::BottomRight
        ) {
            right = (right + delta_x).clamp(left + min_side, max_width);
        }
        if matches!(
            handle,
            CropHandle::Top | CropHandle::TopLeft | CropHandle::TopRight
        ) {
            top = (top + delta_y).clamp(0, bottom - min_side);
        }
        if matches!(
            handle,
            CropHandle::Bottom | CropHandle::BottomLeft | CropHandle::BottomRight
        ) {
            bottom = (bottom + delta_y).clamp(top + min_side, max_height);
        }
    }

    RibbonCrop {
        left: usize::try_from(left).unwrap_or(0),
        top: usize::try_from(top).unwrap_or(0),
        width: usize::try_from((right - left).max(1)).unwrap_or(source_size[0].max(1)),
        height: usize::try_from((bottom - top).max(1)).unwrap_or(source_size[1].max(1)),
    }
}
