/*
File: crates/ms-tab-translation/src/tab/mt.rs

Purpose:
Machine translation in the Translation tab: the MT and AI API event drains, the deferred MT
actions and scopes (bubble, page, all, AI image bubbles), the MT run start, the AI MT options
snapshot and the AI MT request-preview worker and window.

Key functions:
- TranslationTabState::poll_mt_events / poll_ai_api_events / mt_has_active_or_pending /
  cancel_active_mt / handle_pending_mt_actions.
- start_mt_for_scope / start_mt_for_ids / start_mt_with_items (+ the AI image-bubble collectors).
- current_ai_mt_options / start_ai_mt_request_preview / poll_ai_mt_request_preview /
  draw_ai_mt_request_preview_window.
- mt_image_input_for_bubble / mt_image_areas_for_bubble: an image bubble's MT image input.
- selectable_monospace / format_context_chars: preview window and progress text helpers.
*/

use ms_canvas::CanvasView;
use ms_project::{Bubble, ProjectData};
use crate::machine_translation::{AiMtImageMode, AiMtOptions, MtControllerEvent, MtImageArea, MtImageInput, MtImageSource, MtRequestPreviewPart, MtTranslateItem, MtTranslateRequest, bubble_order_for_sort, build_ai_mt_request_preview, character_for_bubble, whole_minutes_ceil};
use ms_ai_api::{ImageInputSupport, image_input_support, is_probable_quota_or_limit_error};
use crate::panels::bubbles::bubble_extra_string;
use crate::panels::machine_translation::{MtPanelProgress, MtPanelTab, MtStopNotice};
use eframe::egui;
use ms_theme::Severity;
use serde_json::Value;
use std::sync::mpsc;
use ms_thread as thread;
use super::{MtRequestPreviewWindow, TranslationTabState};
use super::canvas_hooks::{is_hint_bubble_record, is_image_bubble_record, normalize_uv_rect};
use super::settings_io::normalized_lang_input;

impl TranslationTabState {
    pub(super) fn poll_mt_events(&mut self, ctx: &egui::Context, canvas: &mut CanvasView) {
        for event in self.mt_controller.poll_events() {
            match event {
                MtControllerEvent::RunStarted { total } => {
                    // A fresh run clears any stale credit/limit notice from a previous run.
                    self.mt_stop_notice = None;
                    self.mt_progress = Some(MtPanelProgress {
                        total,
                        ..MtPanelProgress::default()
                    });
                }
                MtControllerEvent::ItemTranslated {
                    bubble_id,
                    translated_text,
                    original_text,
                } => {
                    let applied = if let Some(original_text) = original_text {
                        canvas.apply_machine_translation_result_with_original(
                            bubble_id,
                            original_text,
                            translated_text,
                        )
                    } else {
                        canvas.apply_machine_translation_result(bubble_id, translated_text)
                    };
                    if !applied {
                        self.push_toast(
                            ctx,
                            tf!("translation.tab.apply_translation_error", bubble_id = bubble_id),
                            Severity::Error,
                            2.4,
                        );
                    }
                }
                MtControllerEvent::ItemAreasTranslated { bubble_id, areas } => {
                    if !canvas.apply_machine_translation_areas(bubble_id, areas) {
                        self.push_toast(
                            ctx,
                            tf!("translation.tab.apply_area_translation_error", bubble_id = bubble_id),
                            Severity::Error,
                            2.4,
                        );
                    }
                }
                MtControllerEvent::RunFinished { translated, errors } => {
                    self.mt_progress = None;
                    let severity = if errors == 0 {
                        Severity::Success
                    } else {
                        Severity::Warning
                    };
                    self.push_toast(
                        ctx,
                        tf!("translation.tab.mt_progress_status", translated = translated, translated_2 = translated + errors, errors = errors),
                        severity,
                        2.8,
                    );
                }
                MtControllerEvent::RunCancelled { translated, errors } => {
                    self.mt_progress = None;
                    self.push_toast(
                        ctx,
                        tf!("translation.tab.mt_cancelled_status", translated = translated, errors = errors),
                        Severity::Warning,
                        2.8,
                    );
                }
                MtControllerEvent::RunFailed { error } => {
                    self.mt_progress = None;
                    eprintln!("[MT][RunFailed] {}", error.replace('\n', " "));
                    if is_probable_quota_or_limit_error(&error) {
                        // Stop quietly: replace the red error toast with a sticky yellow notice that
                        // keeps the full provider error available behind a toggle.
                        self.mt_stop_notice = Some(MtStopNotice {
                            full_error: error,
                            expanded: false,
                        });
                    } else {
                        self.push_toast(
                            ctx,
                            tf!("translation.tab.mt_error_status", error = error),
                            Severity::Error,
                            3.2,
                        );
                    }
                }
                // Arrives before the run's first request, so it shows until the first progress
                // toast replaces it; the worker already logged it with the path.
                MtControllerEvent::ContextNotice { notice } => {
                    self.push_toast(ctx, notice, Severity::Warning, 6.0);
                }
                MtControllerEvent::Progress {
                    translated,
                    errors,
                    total,
                    context_used_chars,
                    context_budget_chars,
                    pruned_replicas,
                } => {
                    // Replaces the whole progress, which also clears a finished limiter wait.
                    self.mt_progress = Some(MtPanelProgress {
                        translated,
                        errors,
                        total,
                        context_used_chars,
                        context_budget_chars,
                        pruned_replicas,
                        rate_limit_wait: None,
                    });
                    self.push_toast(
                        ctx,
                        tf!("translation.tab.ai_mt_progress_status", translated = translated, total = total, errors = errors, used = format_context_chars(context_used_chars), budget = format_context_chars(context_budget_chars), pruned_replicas = pruned_replicas),
                        Severity::Info,
                        2.4,
                    );
                }
                MtControllerEvent::RateLimitWait { wait } => {
                    self.mt_progress.get_or_insert_default().rate_limit_wait = wait;
                }
                MtControllerEvent::RunRateLimited { service, translated, errors, retry_in_secs } => {
                    self.mt_progress = None;
                    self.push_toast(
                        ctx,
                        tf!("translation.tab.mt_rate_limited_stop_status", service = service.title(), minutes = whole_minutes_ceil(retry_in_secs), translated = translated, errors = errors),
                        Severity::Warning,
                        6.0,
                    );
                }
            }
        }
    }

    /// Applies finished AI API connection requests (key save / delete, metadata refresh) of the
    /// OCR and MT panels: a replaced model marks that panel's settings dirty, every notice
    /// becomes a toast. Non-blocking.
    pub(super) fn poll_ai_api_events(&mut self, ctx: &egui::Context) {
        let ocr = self.ocr_ai_api_tasks.poll_and_apply(&mut self.ocr_panel_options.ai_api);
        self.ocr_settings_dirty |= ocr.model_changed;
        let mt = self.mt_ai_api_tasks.poll_and_apply(&mut self.mt_panel_options.ai_api);
        self.mt_settings_dirty |= mt.model_changed;
        for notice in ocr.notices.into_iter().chain(mt.notices) {
            self.push_toast(ctx, notice.text, notice.severity, notice.duration_s);
        }
    }

    pub(super) fn mt_has_active_or_pending(&self) -> bool {
        self.mt_controller.is_busy()
            || self.pending_mt_start_all
            || self.pending_mt_start_page
            || !self.pending_translate_actions.is_empty()
    }

    pub(super) fn cancel_active_mt(&mut self, ctx: &egui::Context) {
        let had_pending = self.pending_mt_start_all
            || self.pending_mt_start_page
            || !self.pending_translate_actions.is_empty();
        self.pending_mt_start_all = false;
        self.pending_mt_start_page = false;
        self.pending_translate_actions.clear();

        if self.mt_controller.request_cancel() {
            self.push_toast(
                ctx,
                t!("translation.tab.mt_cancelled_notice").to_string(),
                Severity::Warning,
                2.2,
            );
        } else if had_pending {
            self.push_toast(
                ctx,
                t!("translation.tab.mt_deferred_cancelled_status").to_string(),
                Severity::Warning,
                2.2,
            );
        }
    }

    pub(super) fn handle_pending_mt_actions(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
    ) {
        if self.pending_mt_start_all {
            self.pending_mt_start_all = false;
            self.start_mt_for_scope(ctx, canvas, project, false);
        }
        if self.pending_mt_start_page {
            self.pending_mt_start_page = false;
            self.start_mt_for_scope(ctx, canvas, project, true);
        }
        if self.mt_controller.is_busy() || self.pending_translate_actions.is_empty() {
            return;
        }
        let bubble_ids = std::mem::take(&mut self.pending_translate_actions);
        self.start_mt_for_ids(ctx, canvas, project, bubble_ids);
    }

    fn start_mt_for_scope(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
        current_page_only: bool,
    ) {
        let items = self.collect_ai_mt_scope_items(canvas, project, current_page_only);
        if !items.iter().any(|item| item.needs_translation) {
            self.push_toast(
                ctx,
                t!("translation.tab.no_bubbles_for_mt_status").to_string(),
                Severity::Warning,
                2.3,
            );
            return;
        }
        self.start_mt_with_items(ctx, project, items);
    }

    /// Collects the MT items for a scope translation (whole project or current page) using the same
    /// rules as a real run: image-bubble inclusion, multimodal gating, and AI context mode that adds
    /// already-translated replicas as ordered read-only context. Shared by the translate action and
    /// the request preview so both see identical input.
    fn collect_ai_mt_scope_items(
        &self,
        canvas: &CanvasView,
        project: &ProjectData,
        current_page_only: bool,
    ) -> Vec<MtTranslateItem> {
        if self.ai_mt_imagebubble_mode_active() {
            return self.collect_ai_mt_imagebubble_items(canvas, project, current_page_only);
        }
        let current_page = canvas.current_page_idx();
        let mut items = Vec::new();
        let include_image_bubbles = self.ai_mt_can_include_image_bubbles();
        // When AI context mode is on, already-translated replicas are kept as ordered read-only
        // context so the model sees the correct reading order around the untranslated replicas.
        let context_mode = self.ai_mt_context_includes_translated();
        for bubble in project.bubbles.iter() {
            if current_page_only && bubble.img_idx != current_page {
                continue;
            }
            // A hint is an author note to the translator, never a replica: it must not be sent
            // for translation, nor offered as context.
            if is_hint_bubble_record(bubble) {
                continue;
            }
            if is_image_bubble_record(bubble) && !include_image_bubbles {
                continue;
            }
            let image_input = if include_image_bubbles {
                mt_image_input_for_bubble(bubble)
            } else {
                None
            };
            let has_translation = !bubble.text.trim().is_empty();
            // An image bubble is translatable whenever it has no translation yet (the model infers
            // its source text); a text bubble also needs a non-empty source.
            let needs_translation = if image_input.is_some() {
                !has_translation
            } else {
                !has_translation && !bubble.original_text.trim().is_empty()
            };
            let is_context = !needs_translation && context_mode && has_translation;
            if !needs_translation && !is_context {
                continue;
            }
            items.push(MtTranslateItem {
                bubble_id: bubble.id,
                page_idx: bubble.img_idx,
                img_v: bubble.img_v,
                order: bubble_order_for_sort(bubble),
                character: character_for_bubble(
                    bubble,
                    self.mt_panel_options.ai_use_character_names,
                ),
                text: bubble.original_text.clone(),
                existing_translation: bubble.text.clone(),
                // Context replicas never carry an image binary; they exist only for ordering.
                image: if needs_translation { image_input } else { None },
                needs_translation,
            });
        }
        items
    }

    /// True when the AI per-ImageBubble mode is selected and usable (AI tab + a model not known
    /// to be text-only).
    fn ai_mt_imagebubble_mode_active(&self) -> bool {
        self.mt_panel_options.active_tab == MtPanelTab::AiApi
            && self.mt_panel_options.ai_image_mode == AiMtImageMode::ImagesOnly
            && image_input_support(&self.mt_panel_options.ai_api.model) != ImageInputSupport::NotSupported
    }

    /// Collects items for the per-ImageBubble mode: every chapter bubble in reading order is included
    /// as ordered context (text only, no binary), and `needs_translation` is set only on the in-scope
    /// ImageBubbles that still lack a translation. The context spans the full chapter regardless of
    /// the page scope, so each translated ImageBubble sees everything before it; the page scope only
    /// restricts which ImageBubbles are actually translated.
    fn collect_ai_mt_imagebubble_items(
        &self,
        canvas: &CanvasView,
        project: &ProjectData,
        current_page_only: bool,
    ) -> Vec<MtTranslateItem> {
        let current_page = canvas.current_page_idx();
        let mut items = Vec::new();
        for bubble in project.bubbles.iter() {
            // A hint is an author note to the translator, never a replica: it is neither
            // translated nor used as ordered context.
            if is_hint_bubble_record(bubble) {
                continue;
            }
            let is_image = is_image_bubble_record(bubble);
            let has_translation = !bubble.text.trim().is_empty();
            let in_target_scope = !current_page_only || bubble.img_idx == current_page;
            // Only still-untranslated ImageBubbles in scope are translated; everything else (text
            // bubbles, already-translated or out-of-scope images) is read-only ordered context.
            let needs_translation = is_image && !has_translation && in_target_scope;
            let image_input = if needs_translation {
                mt_image_input_for_bubble(bubble)
            } else {
                None
            };
            if needs_translation && image_input.is_none() {
                // An ImageBubble we cannot load a binary for falls back to plain context.
                continue;
            }
            items.push(MtTranslateItem {
                bubble_id: bubble.id,
                page_idx: bubble.img_idx,
                img_v: bubble.img_v,
                order: bubble_order_for_sort(bubble),
                character: character_for_bubble(
                    bubble,
                    self.mt_panel_options.ai_use_character_names,
                ),
                text: bubble.original_text.clone(),
                existing_translation: bubble.text.clone(),
                image: image_input,
                needs_translation,
            });
        }
        items
    }

    fn start_mt_for_ids(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
        bubble_ids: Vec<i64>,
    ) {
        let mut ids = bubble_ids;
        ids.sort_unstable();
        ids.dedup();
        let mut items = Vec::new();
        let include_image_bubbles = self.ai_mt_can_include_image_bubbles();
        for bubble_id in ids {
            let project_bubble = project.bubbles.iter().find(|bubble| bubble.id == bubble_id);
            let source_text = canvas
                .bubble_original_text(bubble_id)
                .or_else(|| project_bubble.map(|bubble| bubble.original_text.clone()))
                .unwrap_or_default();
            let Some(bubble) = project_bubble else {
                continue;
            };
            // A hint is an author note to the translator, never a replica: an explicit per-id
            // request must not translate it either.
            if is_hint_bubble_record(bubble) {
                continue;
            }
            if is_image_bubble_record(bubble) && !include_image_bubbles {
                continue;
            }
            let image_input = if include_image_bubbles {
                mt_image_input_for_bubble(bubble)
            } else {
                None
            };
            if source_text.trim().is_empty() && image_input.is_none() {
                continue;
            }
            items.push(MtTranslateItem {
                bubble_id,
                page_idx: bubble.img_idx,
                img_v: bubble.img_v,
                order: bubble_order_for_sort(bubble),
                character: character_for_bubble(
                    bubble,
                    self.mt_panel_options.ai_use_character_names,
                ),
                text: source_text,
                existing_translation: bubble.text.clone(),
                image: image_input,
                // Explicit per-id requests always translate the selected replicas.
                needs_translation: true,
            });
        }
        if items.is_empty() {
            return;
        }
        self.start_mt_with_items(ctx, project, items);
    }

    fn start_mt_with_items(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        items: Vec<MtTranslateItem>,
    ) {
        if self.mt_controller.is_busy() {
            self.push_toast(
                ctx,
                t!("translation.mt.already_running_status").to_string(),
                Severity::Warning,
                2.2,
            );
            return;
        }

        let request = MtTranslateRequest {
            service: self.mt_panel_options.service,
            source_lang: normalized_lang_input(&self.mt_panel_options.source_lang, "auto"),
            target_lang: normalized_lang_input(&self.mt_panel_options.target_lang, "ru"),
            items,
            ai_api: self
                .mt_panel_options
                .active_tab
                .eq(&MtPanelTab::AiApi)
                .then(|| self.current_ai_mt_options(project)),
        };

        if let Err(err) = self.mt_controller.start_translation(request) {
            eprintln!("[MT][StartFailed] {}", err.replace('\n', " "));
            self.push_toast(ctx, tf!("translation.tab.mt_start_error", err = err), Severity::Error, 3.0);
        } else {
            // Drop any previous credit/limit notice as soon as a new run is accepted.
            self.mt_stop_notice = None;
        }
    }
}

impl TranslationTabState {
    /// Builds the AI MT options from the current panel state. `project` is cloned into the options
    /// because the worker thread needs an owned snapshot for image loading. Only meaningful when the
    /// AI API tab is active.
    fn current_ai_mt_options(&self, project: &ProjectData) -> AiMtOptions {
        AiMtOptions {
            service: self.mt_panel_options.ai_api.service,
            base_url: self.mt_panel_options.ai_api.base_url.clone(),
            model: self.mt_panel_options.ai_api.model.clone(),
            system_instruction: self.mt_panel_options.ai_api.system_instruction.clone(),
            sort_mode: self.mt_panel_options.ai_sort_mode,
            use_character_names: self.mt_panel_options.ai_use_character_names,
            use_notes_prompt: self.mt_panel_options.ai_use_notes_prompt,
            include_characters: self.mt_panel_options.ai_include_characters,
            include_terms: self.mt_panel_options.ai_include_terms,
            batch_size: self.mt_panel_options.ai_batch_size,
            reasoning: self.mt_panel_options.ai_reasoning,
            context_limit_percent: self.mt_panel_options.ai_context_limit_percent,
            include_existing_translation: self.mt_panel_options.ai_include_existing_translation,
            image_detail: self.mt_panel_options.ai_image_detail,
            image_mode: self.mt_panel_options.ai_image_mode,
            image_context_source: self.mt_panel_options.ai_image_context_source,
            strict_json: self.mt_panel_options.ai_strict_json,
            session_caps: self.session_caps,
            project: project.clone(),
        }
    }

    /// Debug entry point for "Отобразить полный запрос". Collects the same items a real scope
    /// translation would use and builds the first AI request on a background thread (image loading
    /// must not block the GUI). The result is consumed by `poll_ai_mt_request_preview`.
    pub(super) fn start_ai_mt_request_preview(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
        current_page_only: bool,
    ) {
        // Preview is meaningful only for the AI API path; plain translators have no request context.
        if self.mt_panel_options.active_tab != MtPanelTab::AiApi {
            return;
        }
        let items = self.collect_ai_mt_scope_items(canvas, project, current_page_only);
        if !items.iter().any(|item| item.needs_translation) {
            self.push_toast(
                ctx,
                t!("translation.tab.no_bubbles_preview_status").to_string(),
                Severity::Warning,
                2.3,
            );
            return;
        }
        let options = self.current_ai_mt_options(project);
        let source_lang = normalized_lang_input(&self.mt_panel_options.source_lang, "auto");
        let target_lang = normalized_lang_input(&self.mt_panel_options.target_lang, "ru");
        let scope_label = if current_page_only {
            t!("translation.tab.scope_current_page")
        } else {
            t!("translation.tab.scope_whole_project")
        }
        .to_string();

        let (tx, rx) = mpsc::channel();
        self.mt_request_preview_rx = Some(rx);
        self.mt_request_preview = None;
        let ctx_clone = ctx.clone();
        thread::spawn(move || {
            let result = build_ai_mt_request_preview(&source_lang, &target_lang, items, &options)
                .map(|preview| (preview, scope_label));
            let _ = tx.send(result);
            // Wake the UI so the pending result is picked up even if the pointer is idle.
            ctx_clone.request_repaint();
        });
        self.push_toast(
            ctx,
            t!("translation.tab.preparing_request_status").to_string(),
            Severity::Info,
            1.6,
        );
    }

    /// Consumes a finished background request-preview build, opening the debug window on success or
    /// surfacing the failure as a toast.
    pub(super) fn poll_ai_mt_request_preview(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.mt_request_preview_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok((preview, scope_label))) => {
                self.mt_request_preview_rx = None;
                let image_count = preview
                    .parts
                    .iter()
                    .filter(|part| matches!(part, MtRequestPreviewPart::Image(_)))
                    .count();
                self.mt_request_preview = Some(MtRequestPreviewWindow {
                    preview,
                    scope_label,
                    open: true,
                    image_textures: vec![None; image_count],
                });
            }
            Ok(Err(error)) => {
                self.mt_request_preview_rx = None;
                self.push_toast(
                    ctx,
                    tf!("translation.tab.build_request_error", error = error),
                    Severity::Error,
                    3.5,
                );
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.mt_request_preview_rx = None;
            }
        }
    }

    /// Draws the debug "Полный запрос ИИ перевода" window: system prompt, then the first batch user
    /// message with image binaries rendered inline at their exact positions. Images are uploaded to
    /// GPU textures lazily on first display.
    pub(super) fn draw_ai_mt_request_preview_window(&mut self, ctx: &egui::Context) {
        let Some(window) = self.mt_request_preview.as_mut() else {
            return;
        };
        let mut open = window.open;
        // Disjoint borrows of the window fields so the texture cache can be filled while reading the
        // immutable preview content during the same frame.
        let MtRequestPreviewWindow {
            preview,
            scope_label,
            image_textures,
            ..
        } = window;
        egui::Window::new(t!("translation.tab.ai_request_window_title")).id(egui::Id::new("translation.tab.ai_request_window_title"))
            .open(&mut open)
            .resizable(true)
            .default_size([720.0, 640.0])
            .show(ctx, |ui| {
                ui.label(tf!("translation.tab.request_preview_summary", scope_label = scope_label, batch_total = preview.batch_total, translate = preview.translate_count, context = preview.context_count, items_total = preview.total_item_count, images = preview.image_count, kib = preview.image_bytes / 1024));
                if preview.batch_total > 1 {
                    ui.colored_label(
                        ms_theme::status::WARNING,
                        t!("translation.tab.request_preview_context_hint"),
                    );
                }
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.heading(t!("translation.tab.system_prompt_label"));
                        selectable_monospace(ui, &preview.system_prompt);
                        ui.add_space(10.0);
                        ui.separator();
                        ui.heading(t!("translation.tab.user_message_label"));
                        let mut image_idx = 0usize;
                        for part in &preview.parts {
                            match part {
                                MtRequestPreviewPart::Text(text) => {
                                    selectable_monospace(ui, text);
                                    ui.add_space(6.0);
                                }
                                MtRequestPreviewPart::Image(image) => {
                                    ui.label(tf!("translation.tab.bubble_image_summary", image = image.bubble_id, image_2 = image.width, image_3 = image.height, image_4 = image.png_byte_len / 1024));
                                    if let Some(slot) = image_textures.get_mut(image_idx) {
                                        let texture = slot.get_or_insert_with(|| {
                                            let dims = [
                                                usize::try_from(image.width).unwrap_or(0),
                                                usize::try_from(image.height).unwrap_or(0),
                                            ];
                                            let color = egui::ColorImage::from_rgba_unmultiplied(
                                                dims, &image.rgba,
                                            );
                                            ui.ctx().load_texture(
                                                format!(
                                                    "mt-request-preview-{}-{image_idx}",
                                                    image.bubble_id
                                                ),
                                                color,
                                                egui::TextureOptions::LINEAR,
                                            )
                                        });
                                        // Fit the long edge to a readable width, never upscaling.
                                        let max_w = ui.available_width().min(360.0);
                                        let w = image.width.max(1) as f32;
                                        let h = image.height.max(1) as f32;
                                        let scale = (max_w / w).min(1.0);
                                        let size = egui::vec2(w * scale, h * scale);
                                        ui.add(egui::Image::new((texture.id(), size)));
                                    }
                                    ui.add_space(8.0);
                                    image_idx += 1;
                                }
                            }
                        }
                    });
            });
        window.open = open;
        if !window.open {
            self.mt_request_preview = None;
        }
    }

    fn ai_mt_can_include_image_bubbles(&self) -> bool {
        self.mt_panel_options.active_tab == MtPanelTab::AiApi
            && self.mt_panel_options.ai_include_image_bubbles
            && image_input_support(&self.mt_panel_options.ai_api.model) != ImageInputSupport::NotSupported
    }

    /// True when a scope translation should also send already-translated replicas as ordered
    /// read-only context. Only meaningful for the AI API path with "existing translation in
    /// context" enabled; the plain translators have no batch context.
    fn ai_mt_context_includes_translated(&self) -> bool {
        self.mt_panel_options.active_tab == MtPanelTab::AiApi
            && self.mt_panel_options.ai_include_existing_translation
    }
}

fn mt_image_input_for_bubble(bubble: &Bubble) -> Option<MtImageInput> {
    if !is_image_bubble_record(bubble) {
        return None;
    }
    let source_type = bubble_extra_string(&bubble.extra, "image_source_type");
    let description = bubble_extra_string(&bubble.extra, "description");
    if source_type == "page_crop" {
        let page_idx = bubble
            .extra
            .get("crop_page_idx")
            .and_then(Value::as_u64)
            .and_then(|raw| usize::try_from(raw).ok())
            .unwrap_or(bubble.img_idx);
        let crop_rect = bubble
            .extra
            .get("crop_rect")
            .and_then(Value::as_array)
            .and_then(|items| {
                if items.len() != 4 {
                    return None;
                }
                let mut rect = [0.0; 4];
                for (idx, item) in items.iter().enumerate() {
                    rect[idx] = item.as_f64()? as f32;
                }
                Some(normalize_uv_rect(rect))
            })
            .unwrap_or([
                bubble.img_u - 0.05,
                bubble.img_v - 0.05,
                bubble.img_u + 0.05,
                bubble.img_v + 0.05,
            ]);
        let crop_rect = normalize_uv_rect(crop_rect);
        return Some(MtImageInput {
            description,
            source: MtImageSource::PageCrop {
                page_idx,
                crop_rect,
            },
            areas: mt_image_areas_for_bubble(bubble, Some(crop_rect)),
        });
    }

    let image_path = bubble_extra_string(&bubble.extra, "image_path");
    (!image_path.trim().is_empty()).then(|| MtImageInput {
        description,
        source: MtImageSource::ExternalPath(image_path),
        areas: mt_image_areas_for_bubble(bubble, None),
    })
}

/// Builds the ordered text areas of an image bubble for AI translation.
///
/// Area 0's text is read from the legacy fields (`original_text` + `extra.description`); later
/// areas come from `extra["text_areas"]`. `crop` is the page-crop region (the sent image) used to
/// express each area's bounding box relative to that image; `None` (external images) omits the
/// positional hint.
fn mt_image_areas_for_bubble(bubble: &Bubble, crop: Option<[f32; 4]>) -> Vec<MtImageArea> {
    let description0 = bubble_extra_string(&bubble.extra, "description");
    let rel = |area_rect: [f32; 4]| -> Option<[f32; 4]> {
        let c = crop?;
        let cw = (c[2] - c[0]).max(1e-6);
        let ch = (c[3] - c[1]).max(1e-6);
        Some([
            ((area_rect[0] - c[0]) / cw).clamp(0.0, 1.0),
            ((area_rect[1] - c[1]) / ch).clamp(0.0, 1.0),
            ((area_rect[2] - c[0]) / cw).clamp(0.0, 1.0),
            ((area_rect[3] - c[1]) / ch).clamp(0.0, 1.0),
        ])
    };
    let read_rect = |entry: &Value| -> Option<[f32; 4]> {
        let arr = entry.get("rect").and_then(Value::as_array)?;
        if arr.len() != 4 {
            return None;
        }
        let mut rect = [0.0f32; 4];
        for (idx, item) in arr.iter().enumerate() {
            rect[idx] = item.as_f64()? as f32;
        }
        Some(rect)
    };
    let fallback_rect = crop.unwrap_or([0.0, 0.0, 1.0, 1.0]);
    match bubble.extra.get("text_areas").and_then(Value::as_array) {
        Some(arr) if !arr.is_empty() => arr
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let area_rect = read_rect(entry).unwrap_or(fallback_rect);
                let (description, original) = if idx == 0 {
                    (description0.clone(), bubble.original_text.clone())
                } else {
                    (
                        entry
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        entry
                            .get("original")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    )
                };
                MtImageArea {
                    description,
                    original,
                    rel_bbox: rel(area_rect),
                }
            })
            .collect(),
        _ => vec![MtImageArea {
            description: description0,
            original: bubble.original_text.clone(),
            rel_bbox: None,
        }],
    }
}

/// Renders read-only monospace text that wraps to the panel width. egui labels support text
/// selection, so the exact prompt text stays copyable in the AI request-preview window.
fn selectable_monospace(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).monospace().small()).wrap());
}

fn format_context_chars(chars: usize) -> String {
    if chars >= 1_000 {
        format!("{}.{:01}k", chars / 1_000, (chars % 1_000) / 100)
    } else {
        chars.to_string()
    }
}
