/*
FILE OVERVIEW: src/tabs/translation/adv_rec.rs
Floating advanced-recognition window for Translation tab.

Main responsibilities:
- prepare selected page crop in a background worker so GUI thread stays responsive;
- show crop preview in a centered floating window with region-editor-style sizing and zoom;
- maintain a small transparent paint overlay above the crop for OCR-guides drawn by user;
- hold editable recognition result text and emit UI actions back to Translation tab;
- support local quick-selection OCR inside the floating preview when brush mode is disabled.

Key types:
- `AdvancedRecognitionWindow`: runtime state of the floating window and crop loader thread.
- `AdvancedRecognitionAction`: one-shot UI actions (`Recognize`, `CreateBubble`).
- `AdvancedRecognitionSelection`: metadata of the current selected region.
- `AdvancedRecognitionQuickSelection`: transient in-window selection used for quick OCR crop.

Notes:
- The module intentionally owns only window/crop-preview state.
- OCR dispatch and bubble creation stay in `tab.rs`, which already owns OCR controller/canvas logic.
*/

use crate::tools::MaskBrush;
use crate::widgets::WheelSlider;
use eframe::egui;
use egui::color_picker::{self, Alpha};
use egui::{Color32, ColorImage, Pos2, Rect, TextureHandle, TextureOptions};
use image::{DynamicImage, GenericImageView, ImageFormat, RgbImage, RgbaImage};
use rayon::prelude::*;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use ms_thread::{self as thread, JoinHandle};

const ADV_REC_TEXTURE_OPTIONS: TextureOptions = TextureOptions::LINEAR;
const ADV_REC_MIN_ZOOM: f32 = 0.1;
const ADV_REC_MAX_ZOOM: f32 = 8.0;
const ADV_REC_ZOOM_STEP: f32 = 1.1;
const ADV_REC_WINDOW_UI_PAD_X: f32 = 24.0;
const ADV_REC_WINDOW_UI_PAD_Y: f32 = 290.0;
const ADV_REC_WINDOW_MIN_W: f32 = 340.0;
const ADV_REC_WINDOW_MIN_H: f32 = 320.0;
const ADV_REC_WINDOW_MAX_W: f32 = 1800.0;
const ADV_REC_WINDOW_MAX_H: f32 = 1400.0;
const ADV_REC_WINDOW_VIEWPORT_CAP_FRAC: f32 = 0.90;
const ADV_REC_LOADING_WINDOW_W: f32 = 340.0;
const ADV_REC_LOADING_WINDOW_H: f32 = 120.0;
const ADV_REC_ROTATION_EPSILON_DEG: f32 = 0.01;
const ADV_REC_ROTATION_PARALLEL_PIXELS_THRESHOLD: usize = 262_144;

#[derive(Debug, Clone, Copy)]
pub struct AdvancedRecognitionSelection {
    pub page_idx: usize,
    pub uv_rect: [f32; 4],
}

#[derive(Debug, Clone)]
pub enum AdvancedRecognitionAction {
    Recognize {
        image_override_png: Option<Vec<u8>>,
    },
    QuickRecognizeSelection {
        image_override_png: Vec<u8>,
    },
    CreateBubble {
        page_idx: usize,
        uv_rect: [f32; 4],
        text: String,
    },
    Close,
}

#[derive(Debug)]
struct AdvancedRecognitionLoadRequest {
    job_id: u64,
    selection: AdvancedRecognitionSelection,
    page_path: PathBuf,
}

#[derive(Debug)]
struct AdvancedRecognitionLoadResult {
    job_id: u64,
    selection: AdvancedRecognitionSelection,
    image: Result<ColorImage, String>,
}

struct AdvancedRecognitionSession {
    selection: AdvancedRecognitionSelection,
    working_base_image: ColorImage,
    working_overlay_image: ColorImage,
    preview_base_image: ColorImage,
    preview_overlay_image: ColorImage,
    base_texture: Option<TextureHandle>,
    base_texture_dirty: bool,
    overlay_texture: Option<TextureHandle>,
    overlay_texture_dirty: bool,
    text: String,
    last_quick_result: String,
    status: Option<String>,
    zoom: f32,
    scroll_id: u64,
    zoom_drag_active: bool,
    zoom_drag_last_x: f32,
    active_request_id: Option<u64>,
    brush_enabled: bool,
    brush: MaskBrush,
    brush_color: Color32,
    brush_hardness: f32,
    brush_last_drag_px: Option<(i32, i32)>,
    brush_last_drag_erase: bool,
    brush_stroke_base_image: Option<ColorImage>,
    brush_stroke_mask: Vec<f32>,
    brush_stroke_erase: bool,
    rotation_degrees: f32,
    quick_selection: Option<AdvancedRecognitionQuickSelection>,
}

#[derive(Debug, Clone, Copy)]
struct AdvancedRecognitionQuickSelection {
    start_px: (i32, i32),
    current_px: (i32, i32),
}

impl AdvancedRecognitionQuickSelection {
    fn pixel_rect(self, image_size: [usize; 2]) -> Option<(usize, usize, usize, usize)> {
        let width = image_size[0];
        let height = image_size[1];
        if width == 0 || height == 0 {
            return None;
        }
        let max_x = i32::try_from(width.saturating_sub(1)).ok()?;
        let max_y = i32::try_from(height.saturating_sub(1)).ok()?;
        let x0 = self
            .start_px
            .0
            .clamp(0, max_x)
            .min(self.current_px.0.clamp(0, max_x));
        let y0 = self
            .start_px
            .1
            .clamp(0, max_y)
            .min(self.current_px.1.clamp(0, max_y));
        let x1 = self
            .start_px
            .0
            .clamp(0, max_x)
            .max(self.current_px.0.clamp(0, max_x));
        let y1 = self
            .start_px
            .1
            .clamp(0, max_y)
            .max(self.current_px.1.clamp(0, max_y));
        Some((
            usize::try_from(x0).ok()?,
            usize::try_from(y0).ok()?,
            usize::try_from(x1).ok()?,
            usize::try_from(y1).ok()?,
        ))
    }

    fn scene_rect(self, image_rect: Rect, image_size: [usize; 2]) -> Option<Rect> {
        let (x0, y0, x1, y1) = self.pixel_rect(image_size)?;
        let image_w = image_size[0].max(1) as f32;
        let image_h = image_size[1].max(1) as f32;
        let left = image_rect.left() + image_rect.width() * (x0 as f32 / image_w);
        let top = image_rect.top() + image_rect.height() * (y0 as f32 / image_h);
        let right = image_rect.left() + image_rect.width() * ((x1 + 1) as f32 / image_w);
        let bottom = image_rect.top() + image_rect.height() * ((y1 + 1) as f32 / image_h);
        Some(Rect::from_min_max(
            Pos2::new(left, top),
            Pos2::new(right, bottom),
        ))
    }

    fn is_large_enough(self, image_size: [usize; 2]) -> bool {
        self.pixel_rect(image_size)
            .is_some_and(|(x0, y0, x1, y1)| x1 > x0 && y1 > y0)
    }
}

impl AdvancedRecognitionSession {
    fn set_zoom(&mut self, value: f32) -> bool {
        let clamped = value.clamp(ADV_REC_MIN_ZOOM, ADV_REC_MAX_ZOOM);
        if (clamped - self.zoom).abs() <= f32::EPSILON {
            return false;
        }
        self.zoom = clamped;
        true
    }

    fn scale_zoom(&mut self, factor: f32) -> bool {
        if factor <= 0.0 {
            return false;
        }
        self.set_zoom(self.zoom * factor)
    }

    fn zoomed_image_size(&self) -> egui::Vec2 {
        let w = self.preview_base_image.size[0].max(1) as f32;
        let h = self.preview_base_image.size[1].max(1) as f32;
        egui::vec2((w * self.zoom).max(1.0), (h * self.zoom).max(1.0))
    }

    fn refresh_preview(&mut self) {
        let config = self.rotation_config();
        let (preview_base_image, preview_overlay_image) = rayon::join(
            || rotate_color_image_with_config(&self.working_base_image, config, false),
            || rotate_color_image_with_config(&self.working_overlay_image, config, true),
        );
        self.preview_base_image = preview_base_image;
        self.preview_overlay_image = preview_overlay_image;
        self.base_texture_dirty = true;
        self.overlay_texture_dirty = true;
        self.brush_last_drag_px = None;
        self.brush_last_drag_erase = false;
        self.brush_stroke_base_image = None;
        self.brush_stroke_mask.clear();
        self.brush_stroke_erase = false;
    }

    fn refresh_preview_overlay(&mut self) {
        let config = self.rotation_config();
        self.preview_overlay_image =
            rotate_color_image_with_config(&self.working_overlay_image, config, true);
        self.overlay_texture_dirty = true;
    }

    fn rotation_config(&self) -> RotationRasterConfig {
        RotationRasterConfig::new(&self.working_base_image, self.rotation_degrees)
    }

    fn set_rotation_degrees(&mut self, value: f32) -> bool {
        let clamped = value.clamp(-180.0, 180.0);
        if (clamped - self.rotation_degrees).abs() <= ADV_REC_ROTATION_EPSILON_DEG {
            return false;
        }
        self.rotation_degrees = clamped;
        self.refresh_preview();
        true
    }
}

pub struct AdvancedRecognitionWindow {
    pending_job_id: Option<u64>,
    next_job_id: u64,
    session_id: u64,
    window_rect: Option<Rect>,
    force_center_after_load: bool,
    load_tx: Sender<Option<AdvancedRecognitionLoadRequest>>,
    load_rx: Receiver<AdvancedRecognitionLoadResult>,
    load_thread: Option<JoinHandle<()>>,
    session: Option<AdvancedRecognitionSession>,
    load_error: Option<String>,
}

impl Default for AdvancedRecognitionWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl AdvancedRecognitionWindow {
    pub fn new() -> Self {
        let (load_tx, load_rx, load_thread) = spawn_adv_rec_loader_thread();
        Self {
            pending_job_id: None,
            next_job_id: 1,
            session_id: 0,
            window_rect: None,
            force_center_after_load: false,
            load_tx,
            load_rx,
            load_thread: Some(load_thread),
            session: None,
            load_error: None,
        }
    }

    pub fn open_selection(
        &mut self,
        selection: AdvancedRecognitionSelection,
        page_path: PathBuf,
    ) -> Result<(), String> {
        let job_id = self.next_job_id;
        self.next_job_id = self.next_job_id.saturating_add(1);
        self.pending_job_id = Some(job_id);
        self.window_rect = None;
        self.force_center_after_load = false;
        self.session = None;
        self.load_error = None;
        self.load_tx
            .send(Some(AdvancedRecognitionLoadRequest {
                job_id,
                selection,
                page_path,
            }))
            .map_err(|_| "Не удалось запустить подготовку области распознавания.".to_string())
    }

    pub fn is_open(&self) -> bool {
        self.pending_job_id.is_some() || self.session.is_some()
    }

    pub fn set_request_running(&mut self, request_id: u64) {
        if let Some(session) = self.session.as_mut() {
            session.active_request_id = Some(request_id);
            session.status = Some("Распознавание выполняется...".to_string());
        }
    }

    pub fn apply_recognition_result(&mut self, request_id: u64, text: String) -> bool {
        let Some(session) = self.session.as_mut() else {
            return false;
        };
        if session.active_request_id != Some(request_id) {
            return false;
        }
        session.active_request_id = None;
        session.text = text;
        session.status = Some("Распознавание завершено.".to_string());
        true
    }

    pub fn apply_quick_recognition_result(
        &mut self,
        request_id: u64,
        text: String,
        status: String,
    ) -> bool {
        let Some(session) = self.session.as_mut() else {
            return false;
        };
        if session.active_request_id != Some(request_id) {
            return false;
        }
        session.active_request_id = None;
        session.last_quick_result = text;
        session.status = Some(status);
        true
    }

    pub fn apply_recognition_error(&mut self, request_id: u64, error: String) -> bool {
        let Some(session) = self.session.as_mut() else {
            return false;
        };
        if session.active_request_id != Some(request_id) {
            return false;
        }
        session.active_request_id = None;
        session.status = Some(error);
        true
    }

    pub fn draw(&mut self, ctx: &egui::Context) -> Option<AdvancedRecognitionAction> {
        self.poll_loaded_region();
        if self.pending_job_id.is_some() {
            ctx.request_repaint();
        }
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.active_request_id.is_some())
        {
            ctx.request_repaint();
        }

        let prev_window_rect = self.window_rect;
        self.window_rect = None;

        if let Some(session) = self.session.as_mut() {
            let viewport = ctx.content_rect();
            let viewport_cap = viewport.size() * ADV_REC_WINDOW_VIEWPORT_CAP_FRAC;
            let window_size = adv_rec_target_window_size(session.zoomed_image_size(), viewport_cap);
            let window_pos = adv_rec_window_pos(
                prev_window_rect,
                window_size,
                viewport,
                self.force_center_after_load,
            );
            self.force_center_after_load = false;

            let mut keep_open = true;
            let mut action = None;
            let mut request_close = false;
            let shown = egui::Window::new("Продвинутое распознавание")
                .id(egui::Id::new(("translation_adv_rec", self.session_id)))
                .fixed_size(window_size)
                .current_pos(window_pos)
                .resizable(false)
                .collapsible(false)
                .open(&mut keep_open)
                .show(ctx, |ui| {
                    action = draw_adv_rec_window_contents(ui, session, &mut request_close);
                });
            if let Some(resp) = shown {
                let window_rect = resp.response.rect;
                self.window_rect = Some(window_rect);
                handle_adv_rec_zoom_input(ctx, session, window_rect);
            }
            if request_close || !keep_open {
                self.close();
                if matches!(action, Some(AdvancedRecognitionAction::CreateBubble { .. })) {
                    return action;
                }
                return Some(AdvancedRecognitionAction::Close);
            }
            return action;
        }

        if let Some(job_id) = self.pending_job_id {
            let viewport = ctx.content_rect();
            let loading_size = egui::vec2(ADV_REC_LOADING_WINDOW_W, ADV_REC_LOADING_WINDOW_H);
            let window_pos = adv_rec_window_pos(prev_window_rect, loading_size, viewport, false);
            let mut keep_open = true;
            let mut close_clicked = false;
            let shown = egui::Window::new("Продвинутое распознавание")
                .id(egui::Id::new(("translation_adv_rec_loading", job_id)))
                .fixed_size(loading_size)
                .current_pos(window_pos)
                .resizable(false)
                .collapsible(false)
                .open(&mut keep_open)
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(4.0);
                        ui.spinner();
                        ui.add_space(6.0);
                        ui.label("Подготавливаю выделенную область...");
                        ui.add_space(6.0);
                        if ui.button("Отмена").clicked() {
                            close_clicked = true;
                        }
                    });
                });
            if let Some(resp) = shown {
                self.window_rect = Some(resp.response.rect);
            }
            if close_clicked || !keep_open {
                self.close();
            }
        }

        if let Some(error) = self.load_error.as_ref() {
            let viewport = ctx.content_rect();
            let error_size = egui::vec2(420.0, 160.0);
            let window_pos = adv_rec_window_pos(prev_window_rect, error_size, viewport, true);
            let mut keep_open = true;
            let mut close_clicked = false;
            let shown = egui::Window::new("Продвинутое распознавание")
                .id(egui::Id::new("translation_adv_rec_error"))
                .fixed_size(error_size)
                .current_pos(window_pos)
                .resizable(false)
                .collapsible(false)
                .open(&mut keep_open)
                .show(ctx, |ui| {
                    ui.label(error);
                    ui.add_space(8.0);
                    if ui.button("Закрыть").clicked() {
                        close_clicked = true;
                    }
                });
            if let Some(resp) = shown {
                self.window_rect = Some(resp.response.rect);
            }
            if close_clicked || !keep_open {
                self.load_error = None;
                return Some(AdvancedRecognitionAction::Close);
            }
        }

        None
    }

    fn poll_loaded_region(&mut self) {
        loop {
            match self.load_rx.try_recv() {
                Ok(result) => {
                    if self.pending_job_id != Some(result.job_id) {
                        continue;
                    }
                    self.pending_job_id = None;
                    self.window_rect = None;
                    self.force_center_after_load = false;
                    match result.image {
                        Ok(image) => {
                            self.session_id = self.session_id.saturating_add(1);
                            self.session = Some(AdvancedRecognitionSession {
                                selection: result.selection,
                                working_overlay_image: ColorImage::filled(
                                    image.size,
                                    Color32::TRANSPARENT,
                                ),
                                preview_overlay_image: ColorImage::filled(
                                    image.size,
                                    Color32::TRANSPARENT,
                                ),
                                working_base_image: image.clone(),
                                preview_base_image: image,
                                base_texture: None,
                                base_texture_dirty: true,
                                overlay_texture: None,
                                overlay_texture_dirty: true,
                                text: String::new(),
                                last_quick_result: String::new(),
                                status: None,
                                zoom: 1.0,
                                scroll_id: self.session_id,
                                zoom_drag_active: false,
                                zoom_drag_last_x: 0.0,
                                active_request_id: None,
                                brush_enabled: false,
                                brush: MaskBrush::default(),
                                brush_color: Color32::from_rgba_unmultiplied(255, 0, 0, 255),
                                brush_hardness: 1.0,
                                brush_last_drag_px: None,
                                brush_last_drag_erase: false,
                                brush_stroke_base_image: None,
                                brush_stroke_mask: Vec::new(),
                                brush_stroke_erase: false,
                                rotation_degrees: 0.0,
                                quick_selection: None,
                            });
                            self.force_center_after_load = true;
                            self.load_error = None;
                        }
                        Err(error) => {
                            self.session = None;
                            self.load_error = Some(error);
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.pending_job_id = None;
                    self.session = None;
                    self.load_error = Some(
                        "Поток подготовки окна распознавания завершился неожиданно.".to_string(),
                    );
                    break;
                }
            }
        }
    }

    fn close(&mut self) {
        self.pending_job_id = None;
        self.window_rect = None;
        self.force_center_after_load = false;
        self.session = None;
    }
}

impl Drop for AdvancedRecognitionWindow {
    fn drop(&mut self) {
        let _ = self.load_tx.send(None);
        if let Some(handle) = self.load_thread.take() {
            let _ = handle.join();
        }
    }
}

fn draw_adv_rec_window_contents(
    ui: &mut egui::Ui,
    session: &mut AdvancedRecognitionSession,
    request_close: &mut bool,
) -> Option<AdvancedRecognitionAction> {
    let mut action = None;

    ui.horizontal(|ui| {
        let mut rotation = session.rotation_degrees;
        if ui
            .add(
                WheelSlider::new(&mut rotation, -180.0..=180.0)
                    .step_by(1.0)
                    .wheel_step(1.0)
                    .fixed_decimals(0)
                    .text("Поворот"),
            )
            .changed()
        {
            session.set_rotation_degrees(rotation);
        }
        ui.label(format!("{:.0}°", session.rotation_degrees));
    });

    ui.horizontal(|ui| {
        if ui.small_button("-").clicked() {
            session.scale_zoom(1.0 / ADV_REC_ZOOM_STEP);
        }
        let mut zoom = session.zoom;
        if ui
            .add(
                WheelSlider::new(&mut zoom, ADV_REC_MIN_ZOOM..=ADV_REC_MAX_ZOOM)
                    .logarithmic(true)
                    .text("Зум"),
            )
            .changed()
        {
            session.set_zoom(zoom);
        }
        if ui.small_button("+").clicked() {
            session.scale_zoom(ADV_REC_ZOOM_STEP);
        }
        if ui.small_button("1:1").clicked() {
            session.set_zoom(1.0);
        }
        ui.label(format!("{:.0}%", session.zoom * 100.0));
    });
    let _ = session.brush.handle_size_shortcuts(ui.ctx());

    ui.separator();
    ensure_adv_rec_texture(session, ui.ctx());

    ui.horizontal(|ui| {
        let brush_button = ui.add(egui::Button::new("Кисть").selected(session.brush_enabled));
        if brush_button.clicked() {
            session.brush_enabled = !session.brush_enabled;
            reset_adv_rec_brush_stroke(session);
        }
        color_picker::color_edit_button_srgba(ui, &mut session.brush_color, Alpha::OnlyBlend);
        let mut radius = session.brush.radius_px();
        if ui
            .add(WheelSlider::new(&mut radius, 1..=200).text("Размер"))
            .changed()
        {
            session.brush.set_radius_px(radius);
        }
        ui.add(
            WheelSlider::new(&mut session.brush_hardness, 0.0..=1.0)
                .text("Жесткость")
                .custom_formatter(|value, _| format!("{:.0}%", value * 100.0)),
        );
    });
    ui.small(
        "ЛКМ: выделить область для быстрого OCR. Кисть: ЛКМ рисует, ПКМ или Shift+ЛКМ стирают.",
    );

    let preview_size = session.zoomed_image_size();
    egui::ScrollArea::both()
        .id_salt(("translation_adv_rec_scroll", session.scroll_id))
        .auto_shrink([false, false])
        .max_width(preview_size.x.max(1.0).min(ui.available_width().max(1.0)))
        .max_height((ui.available_height() - 210.0).max(120.0))
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                let Some(base_texture) = session.base_texture.as_ref() else {
                    return;
                };
                let response = ui.add(
                    egui::Image::new((base_texture.id(), preview_size))
                        .sense(egui::Sense::click_and_drag()),
                );
                if let Some(overlay_texture) = session.overlay_texture.as_ref() {
                    ui.painter().image(
                        overlay_texture.id(),
                        response.rect,
                        Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
                if let Some(image_action) = handle_adv_rec_image_input(ui, session, &response) {
                    action = Some(image_action);
                }
            });
        });

    ui.separator();
    let can_recognize = session.active_request_id.is_none();
    let recognize_label = if session.active_request_id.is_some() {
        "Распознаю..."
    } else {
        "Распознать"
    };
    if ui
        .add_enabled(can_recognize, egui::Button::new(recognize_label))
        .clicked()
    {
        match encode_adv_rec_image_override_png(session) {
            Ok(image_override_png) => {
                action = Some(AdvancedRecognitionAction::Recognize { image_override_png });
            }
            Err(error) => {
                session.status = Some(error);
            }
        }
    }

    if let Some(status) = session.status.as_ref() {
        ui.small(status);
    } else {
        ui.small(" ");
    }

    ui.label(format!(
        "Последнее распознавание: {}",
        if session.last_quick_result.trim().is_empty() {
            "нет".to_string()
        } else {
            session.last_quick_result.clone()
        }
    ));

    ui.add(
        egui::TextEdit::multiline(&mut session.text)
            .desired_rows(6)
            .hint_text("Результат распознавания появится здесь"),
    );

    ui.separator();
    ui.horizontal(|ui| {
        if ui.button("Отмена").clicked() {
            *request_close = true;
        }
        if ui.button("Создать пузырь").clicked() {
            *request_close = true;
            action = Some(AdvancedRecognitionAction::CreateBubble {
                page_idx: session.selection.page_idx,
                uv_rect: session.selection.uv_rect,
                text: session.text.clone(),
            });
        }
    });

    action
}

fn ensure_adv_rec_texture(session: &mut AdvancedRecognitionSession, ctx: &egui::Context) {
    if session.base_texture.is_none() {
        session.base_texture = Some(ctx.load_texture(
            format!("translation_adv_rec_{}", session.scroll_id),
            session.preview_base_image.clone(),
            ADV_REC_TEXTURE_OPTIONS,
        ));
        session.base_texture_dirty = false;
    } else if session.base_texture_dirty
        && let Some(texture) = session.base_texture.as_mut()
    {
        texture.set(session.preview_base_image.clone(), ADV_REC_TEXTURE_OPTIONS);
        session.base_texture_dirty = false;
    }

    if session.overlay_texture.is_none() {
        session.overlay_texture = Some(ctx.load_texture(
            format!("translation_adv_rec_overlay_{}", session.scroll_id),
            session.preview_overlay_image.clone(),
            ADV_REC_TEXTURE_OPTIONS,
        ));
        session.overlay_texture_dirty = false;
        return;
    }
    if !session.overlay_texture_dirty {
        return;
    }
    if let Some(texture) = session.overlay_texture.as_mut() {
        texture.set(
            session.preview_overlay_image.clone(),
            ADV_REC_TEXTURE_OPTIONS,
        );
        session.overlay_texture_dirty = false;
    }
}

fn handle_adv_rec_image_input(
    ui: &mut egui::Ui,
    session: &mut AdvancedRecognitionSession,
    response: &egui::Response,
) -> Option<AdvancedRecognitionAction> {
    let image_rect = response.rect;
    let (
        primary_down,
        secondary_down,
        hover_pos,
        interact_pos,
        mods,
        z_down,
        smooth_scroll,
        raw_scroll,
    ) = ui.ctx().input(|i| {
        (
            i.pointer.primary_down(),
            i.pointer.secondary_down(),
            i.pointer.hover_pos(),
            i.pointer.interact_pos(),
            i.modifiers,
            i.key_down(egui::Key::Z),
            i.smooth_scroll_delta,
            i.raw_scroll_delta,
        )
    });
    let zoom_modifier_down = mods.ctrl || mods.command || z_down;

    if !session.brush_enabled {
        if let Some(selection) = session.quick_selection
            && let Some(selection_rect) =
                selection.scene_rect(image_rect, session.preview_base_image.size)
        {
            ui.painter().rect_filled(
                selection_rect,
                0.0,
                Color32::from_rgba_unmultiplied(0, 160, 255, 60),
            );
            ui.painter().rect_stroke(
                selection_rect,
                0.0,
                egui::Stroke::new(2.0, Color32::from_rgb(0, 160, 255)),
                egui::StrokeKind::Outside,
            );
        }
    } else if let Some(pointer_pos) = hover_pos.filter(|p| image_rect.contains(*p)) {
        session.brush.draw_circle_cursor_on_image(
            ui,
            image_rect,
            session.preview_base_image.size,
            pointer_pos,
        );
    }

    if !session.brush_enabled && !zoom_modifier_down && !session.zoom_drag_active {
        if response.drag_started()
            && let Some(pointer_pos) = response
                .interact_pointer_pos()
                .filter(|p| image_rect.contains(*p))
        {
            let start_px = adv_rec_pointer_to_image_px(
                pointer_pos,
                image_rect,
                session.preview_base_image.size,
            );
            session.quick_selection = Some(AdvancedRecognitionQuickSelection {
                start_px,
                current_px: start_px,
            });
            ui.ctx().request_repaint();
        }
        if let Some(selection) = session.quick_selection.as_mut()
            && let Some(pointer_pos) = ui.ctx().input(|i| i.pointer.latest_pos())
            && image_rect.contains(pointer_pos)
        {
            selection.current_px = adv_rec_pointer_to_image_px(
                pointer_pos,
                image_rect,
                session.preview_base_image.size,
            );
        }
        let should_finish = session.quick_selection.is_some()
            && (response.drag_stopped()
                || (!primary_down && !ui.ctx().input(|i| i.pointer.any_down())));
        if should_finish && let Some(selection) = session.quick_selection.take() {
            if selection.is_large_enough(session.preview_base_image.size) {
                match encode_adv_rec_selection_png(session, selection) {
                    Ok(image_override_png) => {
                        return Some(AdvancedRecognitionAction::QuickRecognizeSelection {
                            image_override_png,
                        });
                    }
                    Err(error) => {
                        session.status = Some(error);
                    }
                }
            }
            ui.ctx().request_repaint();
        }
    } else if !primary_down {
        session.quick_selection = None;
    }

    if session.brush_enabled
        && let Some(pointer_pos) = hover_pos.filter(|p| image_rect.contains(*p))
    {
        session.brush.draw_circle_cursor_on_image(
            ui,
            image_rect,
            session.preview_base_image.size,
            pointer_pos,
        );
    }

    let image_hovered = hover_pos.is_some_and(|p| image_rect.contains(p));
    if image_hovered {
        let mut wheel_delta = smooth_scroll.y;
        if wheel_delta.abs() <= f32::EPSILON {
            wheel_delta = raw_scroll.y;
        }
        if wheel_delta.abs() <= f32::EPSILON {
            wheel_delta = smooth_scroll.x;
        }
        if wheel_delta.abs() <= f32::EPSILON {
            wheel_delta = raw_scroll.x;
        }
        if mods.shift && !zoom_modifier_down && session.brush.handle_wheel(wheel_delta, mods) {
            ui.ctx().input_mut(|input| {
                input.smooth_scroll_delta = egui::Vec2::ZERO;
                input.raw_scroll_delta = egui::Vec2::ZERO;
            });
            ui.ctx().request_repaint();
        }
    }

    if !session.brush_enabled || zoom_modifier_down || session.zoom_drag_active {
        if !(primary_down || secondary_down) {
            reset_adv_rec_brush_stroke(session);
        } else {
            session.brush_last_drag_px = None;
            session.brush_last_drag_erase = false;
        }
        return None;
    }

    let erase = secondary_down || (primary_down && mods.shift);
    let draw = primary_down && !mods.shift;
    let paint_active = erase || draw;

    if let Some(pointer_pos) = interact_pos
        && image_rect.contains(pointer_pos)
        && paint_active
    {
        let preview_px =
            adv_rec_pointer_to_image_px(pointer_pos, image_rect, session.preview_base_image.size);
        let Some((to_x, to_y)) = adv_rec_preview_px_to_working_px(session, preview_px) else {
            session.brush_last_drag_px = None;
            session.brush_last_drag_erase = erase;
            return None;
        };
        let (from_x, from_y) = match session.brush_last_drag_px {
            Some((from_x, from_y)) if session.brush_last_drag_erase == erase => (from_x, from_y),
            _ => (to_x, to_y),
        };
        begin_adv_rec_brush_stroke(session, erase);
        paint_line_mask_with_hardness(
            &mut session.brush_stroke_mask,
            session.working_overlay_image.size,
            from_x,
            from_y,
            to_x,
            to_y,
            session.brush.radius_px().max(1) as i32,
            session.brush_hardness,
        );
        apply_adv_rec_brush_stroke(session);
        session.refresh_preview_overlay();
        session.brush_last_drag_px = Some((to_x, to_y));
        session.brush_last_drag_erase = erase;
        ui.ctx().request_repaint();
        return None;
    }

    if !(primary_down || secondary_down) {
        reset_adv_rec_brush_stroke(session);
    }
    None
}

fn handle_adv_rec_zoom_input(
    ctx: &egui::Context,
    session: &mut AdvancedRecognitionSession,
    window_rect: Rect,
) {
    let (mods, hover_pos, interact_pos, z_down, primary_down, wheel_delta_y) = ctx.input(|i| {
        (
            i.modifiers,
            i.pointer.hover_pos(),
            i.pointer.interact_pos(),
            i.key_down(egui::Key::Z),
            i.pointer.primary_down(),
            i.smooth_scroll_delta.y + i.raw_scroll_delta.y,
        )
    });
    let zoom_modifier_down = mods.ctrl || mods.command || z_down;
    let pointer_pos = interact_pos.or(hover_pos);
    let inside_window = pointer_pos
        .map(|pointer| window_rect.contains(pointer))
        .unwrap_or(false);
    let mut changed = false;

    if inside_window && zoom_modifier_down && wheel_delta_y.abs() > f32::EPSILON {
        let factor = if wheel_delta_y > 0.0 {
            ADV_REC_ZOOM_STEP
        } else {
            1.0 / ADV_REC_ZOOM_STEP
        };
        changed |= session.scale_zoom(factor);
        ctx.input_mut(|input| {
            input.smooth_scroll_delta = egui::Vec2::ZERO;
            input.raw_scroll_delta = egui::Vec2::ZERO;
        });
    }

    if session.zoom_drag_active {
        if !zoom_modifier_down || !primary_down {
            session.zoom_drag_active = false;
        } else if let Some(pointer) = pointer_pos {
            let dx = pointer.x - session.zoom_drag_last_x;
            if dx.abs() > f32::EPSILON {
                let factor = ADV_REC_ZOOM_STEP.powf(dx / 80.0);
                changed |= session.scale_zoom(factor);
                session.zoom_drag_last_x = pointer.x;
            }
        }
    } else if inside_window
        && zoom_modifier_down
        && primary_down
        && let Some(pointer) = pointer_pos
    {
        session.zoom_drag_active = true;
        session.zoom_drag_last_x = pointer.x;
    }

    if inside_window {
        if ctx.input_mut(|input| input.consume_key(mods, egui::Key::Equals)) {
            changed |= session.scale_zoom(ADV_REC_ZOOM_STEP);
        }
        if ctx.input_mut(|input| input.consume_key(mods, egui::Key::Minus)) {
            changed |= session.scale_zoom(1.0 / ADV_REC_ZOOM_STEP);
        }
        if ctx.input_mut(|input| input.consume_key(mods, egui::Key::Num0)) {
            changed |= session.set_zoom(1.0);
        }
    }

    if changed {
        ctx.request_repaint();
    }
}

fn adv_rec_pointer_to_image_px(
    pointer_pos: Pos2,
    image_rect: Rect,
    image_size: [usize; 2],
) -> (i32, i32) {
    let img_w = image_size[0].max(1) as f32;
    let img_h = image_size[1].max(1) as f32;
    let rel_x = ((pointer_pos.x - image_rect.left()) / image_rect.width()).clamp(0.0, 1.0);
    let rel_y = ((pointer_pos.y - image_rect.top()) / image_rect.height()).clamp(0.0, 1.0);
    let x = (rel_x * (img_w - 1.0)).round() as i32;
    let y = (rel_y * (img_h - 1.0)).round() as i32;
    (x, y)
}

fn adv_rec_preview_px_to_working_px(
    session: &AdvancedRecognitionSession,
    preview_px: (i32, i32),
) -> Option<(i32, i32)> {
    let config = session.rotation_config();
    let preview_x = usize::try_from(preview_px.0).ok()?;
    let preview_y = usize::try_from(preview_px.1).ok()?;
    config.preview_px_to_source_px(preview_x, preview_y)
}

fn encode_adv_rec_image_override_png(
    session: &AdvancedRecognitionSession,
) -> Result<Option<Vec<u8>>, String> {
    let rotation_active = !rotation_is_effectively_zero(session.rotation_degrees);
    let overlay_has_pixels = if rotation_active {
        adv_rec_overlay_has_pixels(&session.preview_overlay_image)
    } else {
        adv_rec_overlay_has_pixels(&session.working_overlay_image)
    };
    if !rotation_active && !overlay_has_pixels {
        return Ok(None);
    }

    let image = if overlay_has_pixels {
        let (base, overlay) = if rotation_active {
            (&session.preview_base_image, &session.preview_overlay_image)
        } else {
            (&session.working_base_image, &session.working_overlay_image)
        };
        let composited = composite_adv_rec_image(base, overlay)?;
        DynamicImage::ImageRgb8(composited)
    } else {
        let base = if rotation_active {
            &session.preview_base_image
        } else {
            &session.working_base_image
        };
        let rgba = color_image_to_rgba_image(base)?;
        DynamicImage::ImageRgba8(rgba)
    };

    let mut cursor = Cursor::new(Vec::new());
    image
        .write_to(&mut cursor, ImageFormat::Png)
        .map_err(|err| format!("Не удалось сериализовать изображение OCR: {err}"))?;
    Ok(Some(cursor.into_inner()))
}

fn encode_adv_rec_selection_png(
    session: &AdvancedRecognitionSession,
    selection: AdvancedRecognitionQuickSelection,
) -> Result<Vec<u8>, String> {
    let (x0, y0, x1, y1) = selection
        .pixel_rect(session.preview_base_image.size)
        .ok_or_else(|| "Не удалось определить границы выделения.".to_string())?;
    if x1 <= x0 || y1 <= y0 {
        return Err("Слишком маленькое выделение для OCR.".to_string());
    }

    let crop_w = x1 - x0 + 1;
    let crop_h = y1 - y0 + 1;
    let overlay_has_pixels =
        adv_rec_overlay_crop_has_pixels(&session.preview_overlay_image, x0, y0, x1, y1);
    let image = if overlay_has_pixels {
        let composited = composite_adv_rec_image_crop(
            &session.preview_base_image,
            &session.preview_overlay_image,
            x0,
            y0,
            crop_w,
            crop_h,
        )?;
        DynamicImage::ImageRgb8(composited)
    } else {
        let rgba =
            color_image_crop_to_rgba_image(&session.preview_base_image, x0, y0, crop_w, crop_h)?;
        DynamicImage::ImageRgba8(rgba)
    };

    let mut cursor = Cursor::new(Vec::new());
    image
        .write_to(&mut cursor, ImageFormat::Png)
        .map_err(|err| format!("Не удалось сериализовать выделение OCR: {err}"))?;
    Ok(cursor.into_inner())
}

fn adv_rec_overlay_has_pixels(overlay: &ColorImage) -> bool {
    overlay.pixels.iter().any(|px| px.a() > 0)
}

fn adv_rec_overlay_crop_has_pixels(
    overlay: &ColorImage,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
) -> bool {
    let width = overlay.size[0];
    if width == 0 || overlay.size[1] == 0 {
        return false;
    }
    for y in y0..=y1 {
        let row_start = y * width;
        for x in x0..=x1 {
            if overlay.pixels[row_start + x].a() > 0 {
                return true;
            }
        }
    }
    false
}

fn composite_adv_rec_image(base: &ColorImage, overlay: &ColorImage) -> Result<RgbImage, String> {
    if base.size != overlay.size {
        return Err("Размер overlay не совпадает с изображением OCR.".to_string());
    }
    let width = u32::try_from(base.size[0])
        .map_err(|_| "Ширина изображения OCR слишком большая.".to_string())?;
    let height = u32::try_from(base.size[1])
        .map_err(|_| "Высота изображения OCR слишком большая.".to_string())?;
    let mut out = RgbImage::new(width, height);
    for (idx, pixel) in out.pixels_mut().enumerate() {
        let base_px = base.pixels[idx];
        let overlay_px = overlay.pixels[idx];
        let [base_r, base_g, base_b, _] = base_px.to_srgba_unmultiplied();
        let [overlay_r, overlay_g, overlay_b, overlay_a] = overlay_px.to_srgba_unmultiplied();
        let alpha = overlay_a as f32 / 255.0;
        let inv_alpha = 1.0 - alpha;
        let r = (base_r as f32 * inv_alpha + overlay_r as f32 * alpha)
            .round()
            .clamp(0.0, 255.0) as u8;
        let g = (base_g as f32 * inv_alpha + overlay_g as f32 * alpha)
            .round()
            .clamp(0.0, 255.0) as u8;
        let b = (base_b as f32 * inv_alpha + overlay_b as f32 * alpha)
            .round()
            .clamp(0.0, 255.0) as u8;
        *pixel = image::Rgb([r, g, b]);
    }
    Ok(out)
}

fn composite_adv_rec_image_crop(
    base: &ColorImage,
    overlay: &ColorImage,
    x0: usize,
    y0: usize,
    width: usize,
    height: usize,
) -> Result<RgbImage, String> {
    if base.size != overlay.size {
        return Err("Размер overlay не совпадает с изображением OCR.".to_string());
    }
    let out_w =
        u32::try_from(width).map_err(|_| "Ширина выделения OCR слишком большая.".to_string())?;
    let out_h =
        u32::try_from(height).map_err(|_| "Высота выделения OCR слишком большая.".to_string())?;
    let src_w = base.size[0];
    let mut out = RgbImage::new(out_w, out_h);
    for dy in 0..height {
        for dx in 0..width {
            let src_idx = (y0 + dy) * src_w + (x0 + dx);
            let base_px = base.pixels[src_idx];
            let overlay_px = overlay.pixels[src_idx];
            let [base_r, base_g, base_b, _] = base_px.to_srgba_unmultiplied();
            let [overlay_r, overlay_g, overlay_b, overlay_a] = overlay_px.to_srgba_unmultiplied();
            let alpha = overlay_a as f32 / 255.0;
            let inv_alpha = 1.0 - alpha;
            out.put_pixel(
                u32::try_from(dx)
                    .map_err(|_| "Координата выделения OCR слишком большая.".to_string())?,
                u32::try_from(dy)
                    .map_err(|_| "Координата выделения OCR слишком большая.".to_string())?,
                image::Rgb([
                    (base_r as f32 * inv_alpha + overlay_r as f32 * alpha)
                        .round()
                        .clamp(0.0, 255.0) as u8,
                    (base_g as f32 * inv_alpha + overlay_g as f32 * alpha)
                        .round()
                        .clamp(0.0, 255.0) as u8,
                    (base_b as f32 * inv_alpha + overlay_b as f32 * alpha)
                        .round()
                        .clamp(0.0, 255.0) as u8,
                ]),
            );
        }
    }
    Ok(out)
}

fn color_image_crop_to_rgba_image(
    image: &ColorImage,
    x0: usize,
    y0: usize,
    width: usize,
    height: usize,
) -> Result<RgbaImage, String> {
    let out_w =
        u32::try_from(width).map_err(|_| "Ширина выделения OCR слишком большая.".to_string())?;
    let out_h =
        u32::try_from(height).map_err(|_| "Высота выделения OCR слишком большая.".to_string())?;
    let src_w = image.size[0];
    let mut out = RgbaImage::new(out_w, out_h);
    for dy in 0..height {
        for dx in 0..width {
            let src_idx = (y0 + dy) * src_w + (x0 + dx);
            let [r, g, b, a] = image.pixels[src_idx].to_srgba_unmultiplied();
            out.put_pixel(
                u32::try_from(dx)
                    .map_err(|_| "Координата выделения OCR слишком большая.".to_string())?,
                u32::try_from(dy)
                    .map_err(|_| "Координата выделения OCR слишком большая.".to_string())?,
                image::Rgba([r, g, b, a]),
            );
        }
    }
    Ok(out)
}

fn begin_adv_rec_brush_stroke(session: &mut AdvancedRecognitionSession, erase: bool) {
    let pixel_count = session.working_overlay_image.pixels.len();
    let needs_reset = session
        .brush_stroke_base_image
        .as_ref()
        .is_none_or(|image| image.size != session.working_overlay_image.size)
        || session.brush_stroke_erase != erase;

    if !needs_reset {
        return;
    }

    session.brush_stroke_base_image = Some(session.working_overlay_image.clone());
    session.brush_stroke_mask.clear();
    session.brush_stroke_mask.resize(pixel_count, 0.0);
    session.brush_stroke_erase = erase;
}

fn reset_adv_rec_brush_stroke(session: &mut AdvancedRecognitionSession) {
    session.brush_last_drag_px = None;
    session.brush_last_drag_erase = false;
    session.brush_stroke_base_image = None;
    session.brush_stroke_mask.clear();
    session.brush_stroke_erase = false;
}

fn apply_adv_rec_brush_stroke(session: &mut AdvancedRecognitionSession) {
    let Some(base_image) = session.brush_stroke_base_image.as_ref() else {
        return;
    };
    if base_image.size != session.working_overlay_image.size
        || session.brush_stroke_mask.len() != session.working_overlay_image.pixels.len()
    {
        return;
    }

    for ((dst, base), strength) in session
        .working_overlay_image
        .pixels
        .iter_mut()
        .zip(base_image.pixels.iter().copied())
        .zip(session.brush_stroke_mask.iter().copied())
    {
        if strength <= f32::EPSILON {
            *dst = base;
            continue;
        }
        let mut pixel = base;
        paint_overlay_pixel(
            &mut pixel,
            session.brush_color,
            strength,
            session.brush_stroke_erase,
        );
        *dst = pixel;
    }
}

// All parameters are independent brush stroke properties; grouping would obscure painting intent.
#[allow(clippy::too_many_arguments)]
fn paint_line_mask_with_hardness(
    dst: &mut [f32],
    image_size: [usize; 2],
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    radius: i32,
    hardness: f32,
) {
    let r = radius.max(1);
    let dx = (x1 - x0) as f32;
    let dy = (y1 - y0) as f32;
    let distance = (dx * dx + dy * dy).sqrt();
    if distance <= f32::EPSILON {
        paint_circle_mask_with_hardness(dst, image_size, x0, y0, r, hardness);
        return;
    }
    let step = (r as f32 * 0.45).max(1.0);
    let stamps = (distance / step).ceil() as usize;
    let mut last = (i32::MIN, i32::MIN);
    for i in 0..=stamps {
        let t = i as f32 / stamps.max(1) as f32;
        let sx = (x0 as f32 + dx * t).round() as i32;
        let sy = (y0 as f32 + dy * t).round() as i32;
        if (sx, sy) == last {
            continue;
        }
        paint_circle_mask_with_hardness(dst, image_size, sx, sy, r, hardness);
        last = (sx, sy);
    }
}

fn paint_circle_mask_with_hardness(
    dst: &mut [f32],
    image_size: [usize; 2],
    cx: i32,
    cy: i32,
    radius: i32,
    hardness: f32,
) {
    let r = radius.max(1);
    let w_usize = image_size[0];
    let w = w_usize as i32;
    let h = image_size[1] as i32;
    let x0 = (cx - r).max(0);
    let x1 = (cx + r).min(w - 1);
    let y0 = (cy - r).max(0);
    let y1 = (cy + r).min(h - 1);
    let radius_f = r as f32;
    let hardness = hardness.clamp(0.0, 1.0);
    let hard_radius = (radius_f * hardness).clamp(0.0, radius_f);
    let soft_span = (radius_f - hard_radius).max(f32::EPSILON);

    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = x - cx;
            let dy = y - cy;
            let dist = ((dx * dx + dy * dy) as f32).sqrt();
            if dist > radius_f {
                continue;
            }
            let strength = if dist <= hard_radius || hardness >= 1.0 {
                1.0
            } else {
                let t = ((dist - hard_radius) / soft_span).clamp(0.0, 1.0);
                1.0 - smoothstep(t)
            };
            if strength <= f32::EPSILON {
                continue;
            }
            let idx = (y as usize).saturating_mul(w_usize) + x as usize;
            dst[idx] = dst[idx].max(strength);
        }
    }
}

fn paint_overlay_pixel(dst: &mut Color32, src: Color32, strength: f32, erase: bool) {
    let pressure = strength.clamp(0.0, 1.0);
    if pressure <= f32::EPSILON {
        return;
    }

    if erase {
        erase_overlay_pixel(dst, pressure);
        return;
    }

    let [src_r, src_g, src_b, src_a] = src.to_srgba_unmultiplied();
    let [dst_r, dst_g, dst_b, dst_a] = dst.to_srgba_unmultiplied();
    let brush_alpha = src_a as f32 / 255.0;
    let src_alpha = brush_alpha * pressure;
    if src_alpha <= f32::EPSILON {
        return;
    }
    let dst_alpha = dst_a as f32 / 255.0;
    let src_r = src_r as f32 / 255.0;
    let src_g = src_g as f32 / 255.0;
    let src_b = src_b as f32 / 255.0;
    let dst_r = dst_r as f32 / 255.0;
    let dst_g = dst_g as f32 / 255.0;
    let dst_b = dst_b as f32 / 255.0;

    let raw_alpha = src_alpha + dst_alpha * (1.0 - src_alpha);
    let mut premul_r = src_r * src_alpha + dst_r * dst_alpha * (1.0 - src_alpha);
    let mut premul_g = src_g * src_alpha + dst_g * dst_alpha * (1.0 - src_alpha);
    let mut premul_b = src_b * src_alpha + dst_b * dst_alpha * (1.0 - src_alpha);

    // Limit alpha growth so semi-transparent strokes stay semi-transparent,
    // while keeping the premultiplied color contribution from both layers.
    let capped_alpha = raw_alpha.min(dst_alpha.max(brush_alpha));
    if raw_alpha > f32::EPSILON && capped_alpha < raw_alpha {
        let scale = capped_alpha / raw_alpha;
        premul_r *= scale;
        premul_g *= scale;
        premul_b *= scale;
    }
    let out_alpha = capped_alpha;
    if out_alpha <= f32::EPSILON {
        *dst = Color32::TRANSPARENT;
        return;
    }

    let out_r = (premul_r / out_alpha).clamp(0.0, 1.0);
    let out_g = (premul_g / out_alpha).clamp(0.0, 1.0);
    let out_b = (premul_b / out_alpha).clamp(0.0, 1.0);
    *dst = Color32::from_rgba_unmultiplied(
        (out_r * 255.0).round().clamp(0.0, 255.0) as u8,
        (out_g * 255.0).round().clamp(0.0, 255.0) as u8,
        (out_b * 255.0).round().clamp(0.0, 255.0) as u8,
        (out_alpha * 255.0).round().clamp(0.0, 255.0) as u8,
    );
}

fn erase_overlay_pixel(dst: &mut Color32, pressure: f32) {
    let [dst_r, dst_g, dst_b, dst_a] = dst.to_srgba_unmultiplied();
    let dst_alpha = dst_a as f32 / 255.0;
    if dst_alpha <= f32::EPSILON {
        return;
    }
    let keep = 1.0 - pressure.clamp(0.0, 1.0);
    *dst = Color32::from_rgba_unmultiplied(
        dst_r,
        dst_g,
        dst_b,
        (dst_alpha * keep * 255.0).round().clamp(0.0, 255.0) as u8,
    );
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn adv_rec_target_window_size(content_size: egui::Vec2, max_size: egui::Vec2) -> egui::Vec2 {
    let content_w = content_size.x.max(1.0);
    let content_h = content_size.y.max(1.0);
    let mut out = egui::vec2(
        content_w + ADV_REC_WINDOW_UI_PAD_X,
        content_h + ADV_REC_WINDOW_UI_PAD_Y,
    );
    let hard_max_x = max_size.x.clamp(1.0, ADV_REC_WINDOW_MAX_W);
    let hard_max_y = max_size.y.clamp(1.0, ADV_REC_WINDOW_MAX_H);
    let hard_min_x = ADV_REC_WINDOW_MIN_W.min(hard_max_x);
    let hard_min_y = ADV_REC_WINDOW_MIN_H.min(hard_max_y);
    out.x = out.x.clamp(hard_min_x, hard_max_x);
    out.y = out.y.clamp(hard_min_y, hard_max_y);
    out
}

fn clamp_window_pos_to_viewport(pos: Pos2, size: egui::Vec2, viewport: Rect) -> Pos2 {
    let min_x = viewport.left();
    let min_y = viewport.top();
    let max_x = (viewport.right() - size.x).max(min_x);
    let max_y = (viewport.bottom() - size.y).max(min_y);
    egui::pos2(pos.x.clamp(min_x, max_x), pos.y.clamp(min_y, max_y))
}

fn adv_rec_window_pos(
    prev_window_rect: Option<Rect>,
    size: egui::Vec2,
    viewport: Rect,
    force_center: bool,
) -> Pos2 {
    if !force_center && let Some(prev_rect) = prev_window_rect {
        return clamp_window_pos_to_viewport(prev_rect.min, prev_rect.size(), viewport);
    }
    let centered = viewport.center() - size * 0.5;
    clamp_window_pos_to_viewport(centered, size, viewport)
}

fn spawn_adv_rec_loader_thread() -> (
    Sender<Option<AdvancedRecognitionLoadRequest>>,
    Receiver<AdvancedRecognitionLoadResult>,
    JoinHandle<()>,
) {
    let (request_tx, request_rx) = mpsc::channel::<Option<AdvancedRecognitionLoadRequest>>();
    let (result_tx, result_rx) = mpsc::channel::<AdvancedRecognitionLoadResult>();
    let handle = thread::spawn(move || {
        let mut cached_page: Option<(PathBuf, RgbaImage)> = None;
        while let Ok(message) = request_rx.recv() {
            let Some(request) = message else {
                break;
            };
            let image = if cached_page
                .as_ref()
                .is_some_and(|(path, _)| *path == request.page_path)
            {
                if let Some((_, rgba)) = cached_page.as_ref() {
                    crop_page_image_to_color_image(
                        &DynamicImage::ImageRgba8(rgba.clone()),
                        request.selection.uv_rect,
                        &request.page_path,
                    )
                } else {
                    Err("Внутренняя ошибка кеша области распознавания.".to_string())
                }
            } else {
                match image::open(&request.page_path) {
                    Ok(decoded) => {
                        cached_page = Some((request.page_path.clone(), decoded.to_rgba8()));
                        crop_page_image_to_color_image(
                            &decoded,
                            request.selection.uv_rect,
                            &request.page_path,
                        )
                    }
                    Err(err) => Err(format!(
                        "Не удалось открыть изображение для распознавания.\nПуть: {}\nОшибка: {err}",
                        request.page_path.display()
                    )),
                }
            };
            if result_tx
                .send(AdvancedRecognitionLoadResult {
                    job_id: request.job_id,
                    selection: request.selection,
                    image,
                })
                .is_err()
            {
                break;
            }
        }
    });
    (request_tx, result_rx, handle)
}

fn crop_page_image_to_color_image(
    source: &DynamicImage,
    uv_rect: [f32; 4],
    page_path: &Path,
) -> Result<ColorImage, String> {
    let (img_w, img_h) = source.dimensions();
    if img_w == 0 || img_h == 0 {
        return Err(format!(
            "Пустое изображение для распознавания: {}",
            page_path.display()
        ));
    }

    let [u1, v1, u2, v2] = normalized_uv(uv_rect);
    let x1 = ((u1 * img_w as f32).floor() as u32).min(img_w.saturating_sub(1));
    let y1 = ((v1 * img_h as f32).floor() as u32).min(img_h.saturating_sub(1));
    let x2 = ((u2 * img_w as f32).ceil() as u32).min(img_w);
    let y2 = ((v2 * img_h as f32).ceil() as u32).min(img_h);
    if x2 <= x1 || y2 <= y1 {
        return Err("Выделение слишком маленькое для подготовки окна распознавания.".to_string());
    }

    let crop = source.crop_imm(x1, y1, x2 - x1, y2 - y1).to_rgba8();
    Ok(ColorImage::from_rgba_unmultiplied(
        [crop.width() as usize, crop.height() as usize],
        crop.as_raw(),
    ))
}

fn normalized_uv(uv: [f32; 4]) -> [f32; 4] {
    let left = uv[0].min(uv[2]).clamp(0.0, 1.0);
    let right = uv[0].max(uv[2]).clamp(0.0, 1.0);
    let top = uv[1].min(uv[3]).clamp(0.0, 1.0);
    let bottom = uv[1].max(uv[3]).clamp(0.0, 1.0);
    [left, top, right, bottom]
}

fn rotation_is_effectively_zero(rotation_degrees: f32) -> bool {
    rotation_degrees.abs() <= ADV_REC_ROTATION_EPSILON_DEG
}

#[derive(Clone, Copy)]
struct RotationRasterConfig {
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    src_center: egui::Vec2,
    dst_center: egui::Vec2,
    cos_a: f32,
    sin_a: f32,
    identity: bool,
}

impl RotationRasterConfig {
    fn new(image: &ColorImage, rotation_degrees: f32) -> Self {
        let src_w = image.size[0];
        let src_h = image.size[1];
        if src_w == 0 || src_h == 0 || rotation_is_effectively_zero(rotation_degrees) {
            return Self {
                src_w,
                src_h,
                dst_w: src_w,
                dst_h: src_h,
                src_center: egui::Vec2::ZERO,
                dst_center: egui::Vec2::ZERO,
                cos_a: 1.0,
                sin_a: 0.0,
                identity: true,
            };
        }

        let angle = rotation_degrees.to_radians();
        let cos_a = angle.cos();
        let sin_a = angle.sin();
        let src_center = egui::vec2((src_w as f32 - 1.0) * 0.5, (src_h as f32 - 1.0) * 0.5);
        let corners = [
            egui::vec2(0.0, 0.0),
            egui::vec2(src_w as f32 - 1.0, 0.0),
            egui::vec2(0.0, src_h as f32 - 1.0),
            egui::vec2(src_w as f32 - 1.0, src_h as f32 - 1.0),
        ];
        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        for corner in corners {
            let rel_x = corner.x - src_center.x;
            let rel_y = corner.y - src_center.y;
            let rot_x = rel_x * cos_a - rel_y * sin_a;
            let rot_y = rel_x * sin_a + rel_y * cos_a;
            min_x = min_x.min(rot_x);
            min_y = min_y.min(rot_y);
            max_x = max_x.max(rot_x);
            max_y = max_y.max(rot_y);
        }
        let dst_w = (max_x - min_x).ceil().max(0.0) as usize + 1;
        let dst_h = (max_y - min_y).ceil().max(0.0) as usize + 1;
        let dst_center = egui::vec2((dst_w as f32 - 1.0) * 0.5, (dst_h as f32 - 1.0) * 0.5);

        Self {
            src_w,
            src_h,
            dst_w,
            dst_h,
            src_center,
            dst_center,
            cos_a,
            sin_a,
            identity: false,
        }
    }

    fn preview_px_to_source_px(self, preview_x: usize, preview_y: usize) -> Option<(i32, i32)> {
        if self.dst_w == 0 || self.dst_h == 0 {
            return None;
        }
        let x = preview_x.min(self.dst_w.saturating_sub(1));
        let y = preview_y.min(self.dst_h.saturating_sub(1));
        let rel_x = x as f32 - self.dst_center.x;
        let rel_y = y as f32 - self.dst_center.y;
        let src_rel_x = rel_x * self.cos_a + rel_y * self.sin_a;
        let src_rel_y = -rel_x * self.sin_a + rel_y * self.cos_a;
        let src_x = src_rel_x + self.src_center.x;
        let src_y = src_rel_y + self.src_center.y;
        let rounded_x = src_x.round() as i32;
        let rounded_y = src_y.round() as i32;
        if rounded_x < 0
            || rounded_y < 0
            || rounded_x >= self.src_w as i32
            || rounded_y >= self.src_h as i32
        {
            return None;
        }
        Some((rounded_x, rounded_y))
    }
}

fn rotate_color_image_with_config(
    image: &ColorImage,
    config: RotationRasterConfig,
    transparent_outside: bool,
) -> ColorImage {
    if config.identity {
        return image.clone();
    }

    let mut out = ColorImage::filled(
        [config.dst_w.max(1), config.dst_h.max(1)],
        if transparent_outside {
            Color32::TRANSPARENT
        } else {
            image.pixels[0]
        },
    );

    if out.pixels.len() >= ADV_REC_ROTATION_PARALLEL_PIXELS_THRESHOLD {
        out.pixels
            .par_chunks_mut(out.size[0])
            .enumerate()
            .for_each(|(y, row)| fill_rotated_row(row, image, y, config, transparent_outside));
    } else {
        for (y, row) in out.pixels.chunks_mut(out.size[0]).enumerate() {
            fill_rotated_row(row, image, y, config, transparent_outside);
        }
    }
    out
}

fn fill_rotated_row(
    row: &mut [Color32],
    image: &ColorImage,
    y: usize,
    config: RotationRasterConfig,
    transparent_outside: bool,
) {
    let rel_y = y as f32 - config.dst_center.y;
    for (x, pixel) in row.iter_mut().enumerate() {
        let rel_x = x as f32 - config.dst_center.x;
        let src_rel_x = rel_x * config.cos_a + rel_y * config.sin_a;
        let src_rel_y = -rel_x * config.sin_a + rel_y * config.cos_a;
        let src_x = src_rel_x + config.src_center.x;
        let src_y = src_rel_y + config.src_center.y;
        *pixel = sample_rotated_color_pixel(image, src_x, src_y, transparent_outside);
    }
}

fn sample_rotated_color_pixel(
    image: &ColorImage,
    src_x: f32,
    src_y: f32,
    transparent_outside: bool,
) -> Color32 {
    let src_w = image.size[0] as i32;
    let src_h = image.size[1] as i32;
    let x = src_x.round() as i32;
    let y = src_y.round() as i32;
    if transparent_outside && (x < 0 || y < 0 || x >= src_w || y >= src_h) {
        return Color32::TRANSPARENT;
    }
    let clamped_x = x.clamp(0, src_w.saturating_sub(1)) as usize;
    let clamped_y = y.clamp(0, src_h.saturating_sub(1)) as usize;
    image.pixels[clamped_y.saturating_mul(image.size[0]) + clamped_x]
}

fn color_image_to_rgba_image(image: &ColorImage) -> Result<RgbaImage, String> {
    let width = u32::try_from(image.size[0])
        .map_err(|_| "Ширина изображения OCR слишком большая.".to_string())?;
    let height = u32::try_from(image.size[1])
        .map_err(|_| "Высота изображения OCR слишком большая.".to_string())?;
    let mut raw = Vec::with_capacity(image.pixels.len().saturating_mul(4));
    for pixel in &image.pixels {
        raw.extend_from_slice(&pixel.to_srgba_unmultiplied());
    }
    RgbaImage::from_raw(width, height, raw)
        .ok_or_else(|| "Не удалось собрать RGBA-изображение для OCR.".to_string())
}
