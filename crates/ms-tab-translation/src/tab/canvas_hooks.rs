/*
File: crates/ms-tab-translation/src/tab/canvas_hooks.rs

Purpose:
The tab's `CanvasHooks` implementation (the per-frame overlay orchestrator, bubble footer,
status style, context menu, scrollbar marks), the image / hint halves of the bubble footer, the
canvas toasts and the image-bubble file helpers.

Key functions:
- impl CanvasHooks for TranslationTabState: draw_canvas_overlay_top_left (polling, flushes,
  pending Composition rebuild, then the panel dock), build_bubble_footer, bubble_status_style,
  on_bubble_action, draw_canvas_page_context_menu, canvas_scrollbar_marks, mask / page overlays.
- TranslationTabState::build_image_bubble_footer_controls / build_hint_bubble_footer_controls.
- TranslationTabState::push_toast / draw_toast / draw_startup_page_load_toast.
- is_image_bubble_record / is_hint_bubble_record (the image-bubble file paste / copy is
  `panels::bubbles::ImageBubbleSave`), project_relative_path, build_image_crop_selection /
  normalize_uv_rect / rect_coords_value (the default crop rect is
  `panels::bubbles::default_image_crop_rect`).
*/

use ms_widgets::bubble_status::{BubbleBorderStyle, BubbleStatusContext, evaluate_bubble_status_rules};
use ms_canvas::{BubbleAction, BubbleClass, CanvasHooks, CanvasScrollbarContext, CanvasUiStatus, CanvasView};
use ms_project::{Bubble, ProjectData};
use crate::panels::bubbles::{ImageBubbleFilePick, ImageBubbleSave, ImageBubbleSource, bubble_extra_bool, bubble_extra_string, default_image_crop_rect};
use ms_widgets::panel_dock::PanelDockState;
use ms_widgets::{AutocompleteLine, ScrollMark, WheelComboBox, WheelSlider, WheelSpinBox};
use eframe::egui;
use egui::{Color32, Pos2, Rect, Stroke};
use ms_theme::Severity;
use serde_json::Value;

use std::path::Path;
use super::{FOOTER_CHARACTER_AUTOCOMPLETE_MAX, HINT_SHOW_OUTSIDE_EXTRA_KEY, OcrToast, TranslationTabState, TranslationTool};
use super::characters::footer_autocomplete_candidates;
use super::scrollbar_marks;

impl TranslationTabState {
    /// Shows `text` as the canvas toast for `duration_s` seconds (at least 0.2 s), replacing any
    /// current toast. `severity` is chosen by the MEANING of the message (`Info` = in progress,
    /// `Success`, `Warning` = nothing done / partial, `Error` = failed); it selects the colour.
    pub(super) fn push_toast(&mut self, ctx: &egui::Context, text: String, severity: Severity, duration_s: f64) {
        let now = ctx.input(|i| i.time);
        self.ocr_toast = Some(OcrToast {
            text,
            severity,
            hide_at_s: now + duration_s.max(0.2),
        });
    }
}

impl TranslationTabState {
    fn draw_toast(&mut self, ctx: &egui::Context, canvas_rect: Rect, top_offset: f32) {
        if !canvas_rect.min.x.is_finite()
            || !canvas_rect.min.y.is_finite()
            || !canvas_rect.max.x.is_finite()
            || !canvas_rect.max.y.is_finite()
        {
            return;
        }
        let now = ctx.input(|i| i.time);
        let Some(toast) = self.ocr_toast.clone() else {
            return;
        };
        if now >= toast.hide_at_s {
            self.ocr_toast = None;
            return;
        }

        egui::Area::new("translation_ocr_toast".into())
            .order(egui::Order::Foreground)
            .fixed_pos(canvas_rect.center_top() + egui::vec2(-160.0, top_offset))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .stroke(Stroke::new(1.0, toast.severity.color()))
                    .show(ui, |ui| {
                        ui.colored_label(toast.severity.color(), toast.text);
                    });
            });
    }

    fn draw_startup_page_load_toast(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        status: CanvasUiStatus,
    ) {
        if !canvas_rect.min.x.is_finite()
            || !canvas_rect.min.y.is_finite()
            || !canvas_rect.max.x.is_finite()
            || !canvas_rect.max.y.is_finite()
        {
            return;
        }
        if status.total_pages == 0 || status.loaded_pages >= status.total_pages {
            return;
        }

        let progress = (status.loaded_pages as f32 / status.total_pages as f32).clamp(0.0, 1.0);
        let title = tf!("translation.tab.loading_images_status", loaded = status.loaded_pages, total = status.total_pages);
        let details = if status.load_errors_count > 0 {
            tf!("translation.tab.load_errors_status", errors = status.load_errors_count)
        } else {
            t!("translation.tab.preparing_pages_status").to_string()
        };

        egui::Area::new("translation_startup_page_load_toast".into())
            .order(egui::Order::Foreground)
            .fixed_pos(canvas_rect.center_top() + egui::vec2(-160.0, 16.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    // In-progress notice: same `Info` severity as the "loading" canvas toasts.
                    .stroke(Stroke::new(1.0, Severity::Info.color()))
                    .show(ui, |ui| {
                        ui.set_width(320.0);
                        ui.label(
                            egui::RichText::new(title)
                                .color(Color32::from_rgb(255, 222, 142))
                                .strong(),
                        );
                        ui.add_space(4.0);
                        let progress_width = (ui.available_width() - 12.0).max(120.0);
                        ui.add(
                            egui::ProgressBar::new(progress)
                                .desired_width(progress_width)
                                .show_percentage(),
                        );
                        ui.add_space(2.0);
                        ui.small(details);
                    });
            });
    }
}

impl TranslationTabState {
    fn build_image_bubble_footer_controls(
        &mut self,
        ui: &mut egui::Ui,
        project: &ProjectData,
        bubble: &Bubble,
        now_s: f64,
    ) {
        let bubble_id = bubble.id;
        let mut source_type = bubble_extra_string(&bubble.extra, "image_source_type");
        if source_type.is_empty() {
            source_type = "external".to_string();
        }
        let before_source_type = source_type.clone();
        WheelComboBox::from_id_salt(("translation_footer_image_source_type", bubble_id))
            .selected_text(if source_type == "page_crop" {
                t!("translation.tab.image_source_ribbon_crop")
            } else {
                t!("translation.common.image_source_external")
            })
            .width(170.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut source_type,
                    "page_crop".to_string(),
                    t!("translation.tab.image_source_ribbon_crop"),
                );
                ui.selectable_value(
                    &mut source_type,
                    "external".to_string(),
                    t!("translation.common.image_source_external"),
                );
            });
        if source_type != before_source_type {
            self.queue_footer_patch(
                bubble_id,
                "image_source_type",
                Value::String(source_type.clone()),
                now_s,
            );
            if source_type == "page_crop" && !bubble.extra.contains_key("crop_rect") {
                self.queue_footer_patch(
                    bubble_id,
                    "crop_rect",
                    Value::Array(default_image_crop_rect(
                        self.known_page_sizes.get(&bubble.img_idx).copied(),
                        bubble,
                    )),
                    now_s,
                );
                self.queue_footer_patch(
                    bubble_id,
                    "crop_page_idx",
                    Value::Number(u64::try_from(bubble.img_idx).unwrap_or(u64::MAX).into()),
                    now_s,
                );
            }
        }

        if source_type == "external" {
            // Disabled while a file dialog is open or a save runs: one at a time (it may belong
            // to another bubble). Resolved by `poll_image_bubble_pick` / `poll_image_bubble_save`.
            let busy = self.image_bubble_pick.is_some() || self.image_bubble_save.is_some();
            if ui
                .add_enabled(!busy, egui::Button::new(t!("translation.common.paste_image_button")).small())
                .clicked()
            {
                match ImageBubbleSave::start(ui.ctx(), project, bubble_id, ImageBubbleSource::Clipboard) {
                    Ok(save) => self.image_bubble_save = Some(save),
                    Err(err) => self.push_toast(ui.ctx(), err, Severity::Error, 3.0),
                }
            }
            if ui
                .add_enabled(!busy, egui::Button::new(t!("translation.common.choose_file_button")).small())
                .clicked()
            {
                self.image_bubble_pick = Some(ImageBubbleFilePick::request(ui.ctx(), bubble_id));
            }
        }
    }

    /// Draws the hint-bubble half of the canvas bubble footer: the single
    /// `canvas.bubble.hint_show_outside_label` checkbox, right after the shared replica-order
    /// spin box.
    ///
    /// The value is read from `extra["hint_show_outside_translation"]`, but a still-pending footer
    /// patch wins: writes are debounced (`FOOTER_PATCH_DEBOUNCE_SECS`), so reading the bubble alone
    /// would visibly bounce the checkbox back for a few frames after a click.
    fn build_hint_bubble_footer_controls(&mut self, ui: &mut egui::Ui, bubble: &Bubble, now_s: f64) {
        let bubble_id = bubble.id;
        let stored = bubble_extra_bool(&bubble.extra, HINT_SHOW_OUTSIDE_EXTRA_KEY, false);
        let mut show_outside = self
            .pending_footer_patches
            .get(&bubble_id)
            .and_then(|patch| patch.get(HINT_SHOW_OUTSIDE_EXTRA_KEY))
            .and_then(Value::as_bool)
            .unwrap_or(stored);
        if ui
            .checkbox(&mut show_outside, t!("canvas.bubble.hint_show_outside_label"))
            .on_hover_text(t!("canvas.bubble.hint_show_outside_tooltip"))
            .changed()
        {
            self.queue_footer_patch(
                bubble_id,
                HINT_SHOW_OUTSIDE_EXTRA_KEY,
                Value::Bool(show_outside),
                now_s,
            );
        }
    }
}

impl CanvasHooks for TranslationTabState {
    /// The create-bubble command's binding (`crate::input::CREATE_BUBBLE`), from this frame's
    /// labels; `None` while it is unbound.
    fn page_menu_create_bubble_shortcut(&self) -> Option<String> {
        self.input_labels().create_bubble_menu.clone()
    }

    /// Translation status per bubble (`tab/scrollbar_marks.rs`), cached across frames.
    fn canvas_scrollbar_marks(&mut self, ctx: &CanvasScrollbarContext<'_>) -> Vec<ScrollMark> {
        scrollbar_marks::translation_status_marks(&mut self.scrollbar_status_cache, ctx)
    }

    fn draw_canvas_mask_overlay_on_page(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        zoom: f32,
    ) {
        self.draw_text_detector_mask_layer_on_page(ui, ctx, page_idx, image_rect, zoom);
    }

    fn draw_canvas_overlay_on_page(
        &mut self,
        ui: &mut egui::Ui,
        _ctx: &egui::Context,
        page_idx: usize,
        image_rect: Rect,
        _zoom: f32,
    ) {
        self.draw_text_detector_additional_overlay_on_page(ui, page_idx, image_rect);
    }

    fn draw_canvas_overlay_top_left(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: egui::Rect,
        canvas: &mut CanvasView,
        project: &ProjectData,
        status: CanvasUiStatus,
        panel_dock: &mut PanelDockState,
    ) {
        let _ = (
            &project.image_dir,
            status.loaded_pages,
            status.load_errors_count,
        );
        let now_s = ctx.input(|i| i.time);
        self.ensure_character_names_loaded(project);
        self.maybe_refresh_character_names_by_watch(project, now_s);
        if self.pending_characters_refresh {
            self.reload_character_names(project);
            self.pending_characters_refresh = false;
        }
        self.sync_footer_tracking(canvas, project);
        self.ensure_ocr_settings_loaded(project);
        self.ensure_mt_settings_loaded(project);
        self.ensure_composition_settings_loaded(project);
        self.ensure_text_detector_settings_loaded(project);
        self.flush_settings_save_if_needed(project);

        // The detector overlay shown outside its panel (OCR engine's "create mask" parameter)
        // needs the stored detection too; the load runs on its worker.
        if self.detector_overlay_visible() {
            self.ensure_text_detection_storage_loaded(project);
        }
        self.poll_text_detector_events(ctx, canvas, project);
        self.poll_text_detection_storage_events(project);
        self.poll_ocr_events(ctx, canvas, project);
        self.poll_ocr_model_downloads(ctx);
        self.poll_mt_events(ctx, canvas);
        self.poll_ai_api_events(ctx);
        self.poll_browser_extension(ctx);
        self.poll_neurolate_key(ctx);
        // The canvas-wide rect selections (mouse and finger alike) come from the router's
        // claim; the canvas raises its own capture area while one is armed.
        self.drive_canvas_selection(ctx, canvas, project);
        self.flush_line_edits_to_text_mask_model();
        self.handle_advanced_recognition_window(ctx, canvas, project);
        self.maybe_dispatch_next_textdetector_ocr_request(ctx, project);
        self.handle_pending_mt_actions(ctx, canvas, project);
        self.poll_image_bubble_pick(ctx, project);
        self.poll_image_bubble_save(ctx, project, now_s);
        self.flush_footer_patches(canvas, project, now_s);
        self.bubbles_panel.flush_text_updates(canvas, now_s);
        // The MT request preview outlives its tool body: polled and drawn on every frame, so it
        // keeps working while the MT tab is behind a sibling or switched off.
        self.poll_ai_mt_request_preview(ctx);
        self.draw_ai_mt_request_preview_window(ctx);
        // A pending rebuild (tool just switched on, settings just loaded) is done here rather
        // than in the body: the body runs only while its tab is in front, and the dock below
        // must see the fresh text on this very frame.
        if self.tools.is_on(TranslationTool::Composition) && self.composition_rebuild_requested {
            self.rebuild_composition_text(project);
        }

        let show_startup_load_toast =
            status.total_pages > 0 && status.loaded_pages < status.total_pages;
        let toast_top_offset = if show_startup_load_toast { 96.0 } else { 16.0 };
        self.draw_startup_page_load_toast(ctx, canvas_rect, status);
        self.draw_toast(ctx, canvas_rect, toast_top_offset);
        if self.text_detector_edit_lines_mode {
            let panel_pos = self.text_detector_edit_panel_pos(canvas_rect);
            egui::Area::new("translation_text_detector_line_edit_panel".into())
                .fixed_pos(panel_pos)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .fill(Color32::from_rgb(26, 88, 40))
                        .stroke(Stroke::new(1.0, Color32::from_rgb(88, 180, 110)))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(t!("translation.tab.lines_edit_mode_label"))
                                    .color(Color32::from_rgb(210, 255, 220))
                                    .strong(),
                            );
                            if ui.button(t!("translation.tab.exit_button")).clicked() {
                                self.set_text_detector_edit_lines_mode(false);
                            }
                        });
                });
        }
        if self.text_detector_edit_mask_mode {
            let panel_pos = self.text_detector_edit_panel_pos(canvas_rect);
            egui::Area::new("translation_text_detector_mask_edit_panel".into())
                .fixed_pos(panel_pos)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .fill(Color32::from_rgb(104, 28, 28))
                        .stroke(Stroke::new(1.0, Color32::from_rgb(212, 72, 72)))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(t!("translation.tab.mask_edit_mode_label"))
                                    .color(Color32::from_rgb(255, 220, 220))
                                    .strong(),
                            );
                            let mut radius = self.text_detector_mask_brush.radius_px();
                            if ui
                                .add(WheelSlider::new(&mut radius, 1..=200).text(t!("translation.tab.brush_px_label")))
                                .changed()
                            {
                                self.text_detector_mask_brush.set_radius_px(radius);
                            }
                            ui.small(tf!("translation.tab.paint_binding_hint", binding = self.input_labels.mask_paint));
                            ui.small(tf!("translation.tab.erase_binding_hint", binding = self.input_labels.mask_erase));
                            if ui.button(t!("translation.tab.exit_button")).clicked() {
                                self.set_text_detector_edit_mask_mode(false);
                            }
                        });
                });
        }
        if self.ocr_quick_selection_mode_active()
            || self.ocr_advanced_selection_mode_active()
            || self.canvas_selection.is_some()
            || self.advanced_recognition.is_open()
            || self.ocr_toast.is_some()
            || self.ocr_controller.state().is_busy()
            || self.mt_controller.is_busy()
            || self.pending_mt_start_all
            || self.pending_mt_start_page
            || !self.pending_translate_actions.is_empty()
            || self.text_detector_controller.is_busy()
            || self.text_detection_storage_busy
            || self.textdetector_ocr_is_running()
            // Detector edit modes are NOT a repaint trigger on their own: an idle
            // edit mode with no active gesture has nothing to animate, and egui
            // already repaints on pointer movement (brush cursor / hover). Only a
            // live gesture needs forced frames — an in-progress mask brush stroke or
            // an active line drag — so painted pixels keep up between pointer events.
            || self.text_detector_mask_stroke_state.is_some()
            || self.text_detector_line_drag_state.is_some()
            || !self.pending_footer_patches.is_empty()
            || self.bubbles_panel.has_pending_text_updates()
        {
            ctx.request_repaint();
        }
        // The dock runs LAST in this hook, but NOT because that would win it the pointer: within
        // one `Order`, egui's layer list is persistent (`egui-0.36.2/src/memory/mod.rs:1236-1242`)
        // and re-sorted stably every pass (`:1371`), so where in the frame an `Area` is created is
        // irrelevant. What raises a layer inside its `Order` is `Area::begin`'s `move_to_top` for
        // an area that was not visible last frame (`egui-0.36.2/src/containers/area.rs:549-553`).
        // The canvas-wide selections (OCR, image crop) are therefore not a problem for the dock:
        // the canvas' own capture area lives on the LOWER `Order::Background`
        // (`ms_canvas` `CanvasView::draw_capture_area`), so it never rises above the dock's panels
        // and an armed selection takes no panel's clicks (`egui-docs/06-overlays.md` §1.1).
        //
        // It must run on EVERY frame this tab is active — the dock's detached sub-windows are
        // immediate viewports that only exist while `PanelDock::end` shows them
        // (`studio.rs::PanelDockPass`), so this hook has no early return before this point.
        self.draw_panel_dock(ctx, canvas_rect, canvas, project, status.total_pages, panel_dock);
    }

    fn build_bubble_header(&mut self, _ui: &mut egui::Ui, _bubble: &Bubble, _editable: bool) {}

    fn build_bubble_footer(
        &mut self,
        ui: &mut egui::Ui,
        project: &ProjectData,
        bubble: &Bubble,
        editable: bool,
    ) {
        if !editable {
            return;
        }
        let bubble_id = bubble.id;
        let now_s = ui.ctx().input(|i| i.time);
        let mut state = self.footer_state_for(bubble);

        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            let mut bubble_order = state.bubble_order;
            let order_resp = ui
                .add(
                    WheelSpinBox::new(&mut bubble_order)
                        .range(0..=100_000)
                        .speed(0.25),
                )
                .on_hover_text(t!("translation.tab.replica_order_tooltip"));
            if order_resp.changed() {
                state.bubble_order = bubble_order.clamp(0, 100_000);
                self.queue_footer_patch(
                    bubble_id,
                    "bubble_order",
                    Value::Number(state.bubble_order.into()),
                    now_s,
                );
            }

            // Class dispatch AFTER the order spin box: every class keeps the replica order, but
            // only a text bubble gets the character field below it.
            match bubble.bubble_class.as_deref().map(BubbleClass::from_str) {
                Some(BubbleClass::Image) => {
                    self.build_image_bubble_footer_controls(ui, project, bubble, now_s);
                    return;
                }
                Some(BubbleClass::Hint) => {
                    self.build_hint_bubble_footer_controls(ui, bubble, now_s);
                    return;
                }
                Some(BubbleClass::Text) | None => {}
            }

            // Borrow `character_names` as a slice instead of cloning the whole Vec
            // every frame for every edited bubble. `character_names` and
            // `footer_character_autocomplete` are disjoint fields, so the immutable
            // suggestions borrow coexists with the mutable autocomplete entry borrow;
            // it ends at the `draw` call, before the later `queue_footer_patch` writes.
            // That disjointness is also why the sentinel is trimmed by a FREE function over
            // the field rather than a `&self` method — a method would borrow the whole
            // `self` and collide with the `footer_character_autocomplete` entry below.
            let suggestions: &[String] = footer_autocomplete_candidates(&self.character_names);
            let autocomplete = self
                .footer_character_autocomplete
                .entry(bubble_id)
                .or_insert_with(|| {
                    AutocompleteLine::new(("translation_footer_character", bubble_id))
                });
            autocomplete.set_max_suggestions(FOOTER_CHARACTER_AUTOCOMPLETE_MAX);
            autocomplete.set_hint_text(t!("translation.tab.who_speaks_placeholder"));

            let field_resp = autocomplete.draw(ui, &mut state.character_name, suggestions);
            if field_resp.changed || field_resp.submitted {
                if !state.is_known_character {
                    state.is_known_character = true;
                    self.queue_footer_patch(
                        bubble_id,
                        "is_known_character",
                        Value::Bool(true),
                        now_s,
                    );
                }
                self.queue_footer_patch(
                    bubble_id,
                    "character_name",
                    Value::String(state.character_name.clone()),
                    now_s,
                );
                self.last_is_known_character = true;
                self.last_character_name = state.character_name.clone();
                self.last_clarification = state.clarification.clone();
            }
        });

        self.footer_overrides.insert(bubble_id, state);
    }

    fn bubble_status_style(
        &mut self,
        bubble: &Bubble,
        editable: bool,
        canvas: &CanvasView,
    ) -> Option<BubbleBorderStyle> {
        if !editable {
            return None;
        }
        // A hint is not a replica: it has no original, no translation and no character, so the
        // incomplete-replica rules would paint a warning border on every hint. Exempt the class.
        if bubble.bubble_class.as_deref().map(BubbleClass::from_str) == Some(BubbleClass::Hint) {
            return None;
        }
        evaluate_bubble_status_rules(
            &canvas.state.bubble_status_rules,
            BubbleStatusContext {
                translation_filled: !bubble.text.trim().is_empty(),
                original_filled: !bubble.original_text.trim().is_empty(),
                character_filled: self.bubble_has_character_for_status(bubble),
            },
        )
    }

    fn on_bubble_action(&mut self, action: BubbleAction, bubble_id: i64) {
        if action == BubbleAction::Translate {
            self.pending_translate_actions.push(bubble_id);
        }
    }

    fn draw_canvas_page_context_menu(
        &mut self,
        ui: &mut egui::Ui,
        _project: &ProjectData,
        page_idx: usize,
        page_uv: Pos2,
    ) -> bool {
        if self.text_detector_edit_mask_mode {
            return true;
        }
        if !self.text_detector_edit_lines_mode {
            return false;
        }
        if ui.button(t!("translation.tab.create_line_button")).clicked() {
            if let Err(error) = self.create_text_detector_line_at_uv(page_idx, page_uv) {
                ms_log::runtime_log::log_warn(format!("[translation] lines edit: could not add a line on page {page_idx}: {error}"));
                self.set_text_detector_status(error, Severity::Error);
            }
            ui.close();
        }
        true
    }

    fn suppress_canvas_page_context_menu(&self, _page_idx: usize) -> bool {
        self.text_detector_edit_mask_mode
    }
}

pub(super) fn is_image_bubble_record(bubble: &Bubble) -> bool {
    bubble.bubble_class.as_deref().map(BubbleClass::from_str) == Some(BubbleClass::Image)
}

/// True when the stored bubble record is a hint bubble (`bubble_class == "hint"`).
pub(super) fn is_hint_bubble_record(bubble: &Bubble) -> bool {
    bubble.bubble_class.as_deref().map(BubbleClass::from_str) == Some(BubbleClass::Hint)
}

pub(super) fn project_relative_path(project: &ProjectData, path: &Path) -> String {
    path.strip_prefix(&project.paths.unsaved_dir)
        .or_else(|_| path.strip_prefix(&project.project_dir))
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub(super) fn build_image_crop_selection(
    canvas: &CanvasView,
    project: &ProjectData,
    selection_rect: Rect,
) -> Option<(usize, [f32; 4])> {
    let mut best_area = 0.0_f32;
    let mut best_page: Option<(usize, Rect)> = None;
    for page in &project.pages {
        let Some(page_rect) = canvas.page_scene_rect(page.idx) else {
            continue;
        };
        let intersection = page_rect.intersect(selection_rect);
        if !intersection.is_positive() {
            continue;
        }
        let area = intersection.width() * intersection.height();
        if area > best_area {
            best_area = area;
            best_page = Some((page.idx, page_rect));
        }
    }

    let (page_idx, page_rect) = best_page?;
    let crop_scene = page_rect.intersect(selection_rect);
    if !crop_scene.is_positive() {
        return None;
    }
    let page_w = page_rect.width().max(1.0);
    let page_h = page_rect.height().max(1.0);
    Some((
        page_idx,
        normalize_uv_rect([
            ((crop_scene.left() - page_rect.left()) / page_w).clamp(0.0, 1.0),
            ((crop_scene.top() - page_rect.top()) / page_h).clamp(0.0, 1.0),
            ((crop_scene.right() - page_rect.left()) / page_w).clamp(0.0, 1.0),
            ((crop_scene.bottom() - page_rect.top()) / page_h).clamp(0.0, 1.0),
        ]),
    ))
}

pub(super) fn normalize_uv_rect(rect: [f32; 4]) -> [f32; 4] {
    [
        rect[0].min(rect[2]).clamp(0.0, 1.0),
        rect[1].min(rect[3]).clamp(0.0, 1.0),
        rect[0].max(rect[2]).clamp(0.0, 1.0),
        rect[1].max(rect[3]).clamp(0.0, 1.0),
    ]
}

/// Builds the canvas `rect_coords` extra value (`{p1:{img_u,img_v}, p2:{img_u,img_v}}`) from a
/// normalized `[x1,y1,x2,y2]` crop rect, so the canvas red image-area rect matches the crop region.
pub(super) fn rect_coords_value(rect: [f32; 4]) -> Value {
    let rect = normalize_uv_rect(rect);
    let point = |u: f32, v: f32| {
        Value::Object(
            [
                ("img_u".to_string(), Value::from(f64::from(u))),
                ("img_v".to_string(), Value::from(f64::from(v))),
            ]
            .into_iter()
            .collect(),
        )
    };
    Value::Object(
        [
            ("p1".to_string(), point(rect[0], rect[1])),
            ("p2".to_string(), point(rect[2], rect[3])),
        ]
        .into_iter()
        .collect(),
    )
}
