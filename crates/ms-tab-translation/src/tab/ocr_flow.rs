/*
File: crates/ms-tab-translation/src/tab/ocr_flow.rs

Purpose:
OCR on canvas selections: the OCR controller's event drain (toasts, deferred bubble inserts,
detector-batch retries), the image-bubble page crop, the advanced-recognition window and manual
OCR dispatch, and the builders that turn panel state into OCR worker requests.

Key functions:
- TranslationTabState::poll_ocr_events / stop_ocr_generation: OCR controller events and the
  panel's generation Stop.
- apply_image_crop_selection: a finished image-crop selection writes crop metadata (no OCR).
- open_advanced_recognition_for_scene_rect / handle_advanced_recognition_window /
  dispatch_manual_ocr_request / start_ocr_for_scene_rect: the quick and advanced OCR paths
  (the region detection they may start lives in `tab/detector_region.rs`).
- build_ocr_runtime_options / build_ocr_request / parse_char_replacement_rules /
  build_bubble_original_text: pure request and text builders (also used by `tab/tool_panels.rs`,
  `tab/block_bubbles.rs` and `tab/detection_ocr.rs`).
*/

use ms_canvas::{BubbleClass, CanvasView};
use ms_project::{ComicType, Page, ProjectData};
use crate::adv_rec::{AdvancedRecognitionAction, AdvancedRecognitionSelection};
use crate::ocr::{OcrControllerEvent, OcrEngine, OcrLoadState, OcrRecognizeRequest, OcrRuntimeOptions, external_model_specs};
use ms_ai_api::GenerationSnapshot;
use crate::panels::ocr::{CharReplacementRuleUi, OcrPanelOptions};
use crate::neurolate::NeurolateSettings;
use eframe::egui;
use egui::Rect;
use ms_theme::Severity;
use serde_json::{Map, Value};
use super::{BuiltOcrRequest, ManualOcrResultTarget, OcrPendingBubbleInsert, TEXT_DETECTOR_OCR_RETRY_DELAY_SECS, TextDetectorOcrRetryState, TranslationTabState, block_ocr_retry_after_failure};
use super::canvas_hooks::{build_image_crop_selection, rect_coords_value};
use super::detector_region;

impl TranslationTabState {
    pub(super) fn poll_ocr_events(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        for event in self.ocr_controller.poll_events() {
            match event {
                OcrControllerEvent::ExternalModelMissing(specs) => {
                    // The gate's error names the button of the disk state it just probed;
                    // re-probe so the panel draws that same button (it may still show a
                    // stale `Installed` or an older state).
                    self.ocr_model_downloads.request_bundle_status(&specs);
                }
                OcrControllerEvent::StateChanged(state) => {
                    // Any other state ends the command's download (finished, cancelled, failed).
                    if state != OcrLoadState::DownloadingModel {
                        self.ocr_model_download.clear();
                    }
                    match state {
                        OcrLoadState::DownloadingModel => {
                            let engine = self
                                .ocr_loading_engine
                                .unwrap_or(self.ocr_panel_options.engine);
                            self.mark_ocr_model_download_started_for_engine(engine);
                            self.push_toast(
                                ctx,
                                t!("translation.common.downloading_model_status").to_string(),
                                Severity::Info,
                                2.2,
                            );
                        }
                        OcrLoadState::Loading => {
                            let engine = self
                                .ocr_loading_engine
                                .unwrap_or(self.ocr_panel_options.engine);
                            self.mark_ocr_load_requested_for_engine(engine);
                            self.push_toast(
                                ctx,
                                t!("translation.tab.engine_loading_status").to_string(),
                                Severity::Info,
                                2.2,
                            );
                        }
                        OcrLoadState::Error => {
                            let engine = self
                                .ocr_loading_engine
                                .take()
                                .unwrap_or(self.ocr_panel_options.engine);
                            self.set_ocr_state_for_engine(engine, OcrLoadState::Error);
                            // A load error of an external-model engine may come from its files
                            // (deleted, damaged): refresh the panel's install state too.
                            let options = &self.ocr_panel_options;
                            let specs = external_model_specs(engine, options.paddle_vl_model, &options.easy_langs);
                            self.ocr_model_downloads.request_bundle_status(&specs);
                            self.push_toast(
                                ctx,
                                t!("translation.tab.engine_load_error").to_string(),
                                Severity::Error,
                                3.0,
                            );
                        }
                        OcrLoadState::Ready => {
                            let engine = self
                                .ocr_loading_engine
                                .take()
                                .unwrap_or(self.ocr_panel_options.engine);
                            self.set_ocr_state_for_engine(engine, OcrLoadState::Ready);
                        }
                        // A load whose model download was cancelled: the engine is not loaded
                        // (the next request loads it again and resumes the staged bytes).
                        OcrLoadState::NotLoaded => {
                            if let Some(engine) = self.ocr_loading_engine.take() {
                                self.set_ocr_state_for_engine(engine, OcrLoadState::NotLoaded);
                            }
                            self.push_toast(
                                ctx,
                                t!("translation.tab.model_download_cancelled_status").to_string(),
                                Severity::Info,
                                2.2,
                            );
                        }
                    }
                }
                // The state already reads `DownloadingModel`; the panel draws the latest event.
                OcrControllerEvent::ModelDownloadProgress(event) => {
                    self.ocr_model_download.record(event);
                    ctx.request_repaint();
                }
                OcrControllerEvent::Recognized { request_id, result, region } => {
                    self.receive_ocr_region(ctx, request_id, region);
                    let manual_target = if self.manual_ocr_active_request_id == Some(request_id) {
                        self.manual_ocr_active_request_id = None;
                        self.manual_ocr_result_target.take()
                    } else {
                        None
                    };
                    let quick_manual =
                        manual_target == Some(ManualOcrResultTarget::ToastAndClipboard);
                    let manual_text =
                        build_bubble_original_text(&result, self.ocr_panel_options.join_newlines);
                    let adv_rec_applied = match manual_target {
                        Some(ManualOcrResultTarget::ToastAndClipboard) => {
                            self.advanced_recognition.apply_quick_recognition_result(
                                request_id,
                                manual_text.clone(),
                                t!("translation.tab.selection_recognition_done_status").to_string(),
                            )
                        }
                        _ => self
                            .advanced_recognition
                            .apply_recognition_result(request_id, manual_text.clone()),
                    };
                    if !quick_manual
                        && self.ocr_panel_options.copy_to_clipboard
                        && !result.text.trim().is_empty()
                    {
                        ctx.copy_text(result.text.clone());
                    }
                    if adv_rec_applied
                        && !quick_manual
                        && self.ocr_panel_options.copy_to_clipboard
                        && !manual_text.trim().is_empty()
                    {
                        ctx.copy_text(manual_text.clone());
                    }
                    if manual_target == Some(ManualOcrResultTarget::ToastAndClipboard) {
                        if !manual_text.trim().is_empty() {
                            ctx.copy_text(manual_text.clone());
                        }
                        let quick_msg = if manual_text.trim().is_empty() {
                            t!("translation.tab.quick_ocr_no_text_status").to_string()
                        } else {
                            tf!("translation.tab.quick_ocr_result_status", manual_text = manual_text.trim())
                        };
                        self.push_toast(ctx, quick_msg, Severity::Success, 3.0);
                    }
                    if let Some(pending_insert) = self.pending_bubble_inserts.remove(&request_id) {
                        self.insert_recognized_bubble(canvas, project, &pending_insert, &result);
                    }
                    if self.textdetector_ocr_active_request_id == Some(request_id) {
                        self.textdetector_ocr_active_request_id = None;
                        self.textdetector_ocr_active_task = None;
                        self.textdetector_ocr_active_engine = None;
                        self.textdetector_ocr_retry_state = None;
                        self.textdetector_ocr_done = self.textdetector_ocr_done.saturating_add(1);
                        if !result.text.trim().is_empty() {
                            self.textdetector_ocr_recognized =
                                self.textdetector_ocr_recognized.saturating_add(1);
                        }
                        self.set_text_detector_status(
                            t!("translation.tab.block_recognition_status"),
                            Severity::Info,
                        );
                    }
                    let msg = if result.lines.is_empty() {
                        t!("translation.tab.recognition_no_text_status").to_string()
                    } else {
                        tf!("translation.tab.recognition_lines_status", lines = result.lines.len())
                    };
                    self.push_toast(ctx, msg, Severity::Success, 2.2);
                }
                OcrControllerEvent::RecognizeFailed { request_id, error } => {
                    self.drop_ocr_region(request_id, tf!("translation.tab.region_detect_error", error = error), Severity::Error);
                    let manual_target = if self.manual_ocr_active_request_id == Some(request_id) {
                        self.manual_ocr_active_request_id = None;
                        self.manual_ocr_result_target.take()
                    } else {
                        None
                    };
                    self.pending_bubble_inserts.remove(&request_id);
                    let adv_rec_error_applied = self
                        .advanced_recognition
                        .apply_recognition_error(request_id, error.clone());
                    if self.textdetector_ocr_active_request_id == Some(request_id) {
                        self.textdetector_ocr_active_request_id = None;
                        let engine = self.textdetector_ocr_active_engine.take();
                        if let Some(task) = self.textdetector_ocr_active_task.take().and_then(|task| block_ocr_retry_after_failure(task, engine)) {
                            let now_s = ctx.input(|i| i.time);
                            self.textdetector_ocr_retry_state = Some(TextDetectorOcrRetryState {
                                task,
                                retry_at_s: now_s + TEXT_DETECTOR_OCR_RETRY_DELAY_SECS,
                            });
                            self.set_text_detector_status(
                                tf!(
                                    "translation.tab.recognition_error_retry_status",
                                    sec = format!("{:.0}", TEXT_DETECTOR_OCR_RETRY_DELAY_SECS)
                                ),
                                Severity::Warning,
                            );
                            self.push_toast(
                                ctx,
                                t!("translation.tab.recognition_retry_status").to_string(),
                                Severity::Warning,
                                2.8,
                            );
                            continue;
                        }
                        self.abort_textdetector_ocr(
                            ctx,
                            tf!("translation.tab.recognition_stopped_error", error = error),
                            Severity::Error,
                        );
                    }
                    if manual_target == Some(ManualOcrResultTarget::ToastAndClipboard) {
                        self.push_toast(ctx, tf!("translation.tab.quick_ocr_error", error = error), Severity::Error, 3.0);
                    } else if !adv_rec_error_applied {
                        self.push_toast(
                            ctx,
                            tf!("translation.tab.recognition_error", error = error),
                            Severity::Error,
                            3.0,
                        );
                    }
                }
                // A user stop: drop the request's pending targets like a failure, but report it
                // neutrally (no error toast, no detector retry).
                OcrControllerEvent::RecognizeCancelled { request_id } => {
                    self.drop_ocr_region(request_id, t!("translation.tab.recognition_cancelled_status").to_string(), Severity::Info);
                    if self.manual_ocr_active_request_id == Some(request_id) {
                        self.manual_ocr_active_request_id = None;
                        self.manual_ocr_result_target = None;
                    }
                    self.pending_bubble_inserts.remove(&request_id);
                    let stopped = t!("translation.tab.recognition_cancelled_status").to_string();
                    self.advanced_recognition.apply_recognition_error(request_id, stopped.clone());
                    if self.textdetector_ocr_active_request_id == Some(request_id) {
                        self.abort_textdetector_ocr(ctx, stopped.clone(), Severity::Info);
                    }
                    self.push_toast(ctx, stopped, Severity::Info, 2.2);
                }
            }
        }
    }

    /// The OCR panel's generation "Stop" on the generation `shown`: aborts it if still in
    /// flight and, when its request (`shown.request_tag` = OCR request id) is the detector
    /// block batch's active one, stops the whole batch (neutral status).
    pub(super) fn stop_ocr_generation(&mut self, ctx: &egui::Context, shown: GenerationSnapshot) {
        self.ocr_controller.cancel_generation(shown.run_id);
        if self.textdetector_ocr_active_request_id == Some(shown.request_tag) {
            self.abort_textdetector_ocr(ctx, t!("translation.tab.recognition_cancelled_status").to_string(), Severity::Info);
        }
    }
}

impl TranslationTabState {
    pub(super) fn apply_image_crop_selection(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
        selection_rect: Rect,
    ) {
        let Some((page_idx, crop_rect)) =
            build_image_crop_selection(canvas, project, selection_rect)
        else {
            return;
        };
        let live_bubbles = canvas.hook_bubbles_snapshot(project);
        let selected_image_id = canvas.selected_bubble_id().and_then(|selected_id| {
            live_bubbles
                .iter()
                .find(|bubble| {
                    bubble.id == selected_id
                        && bubble.bubble_class.as_deref().map(BubbleClass::from_str)
                            == Some(BubbleClass::Image)
                })
                .map(|bubble| bubble.id)
        });
        let bubble_id = if let Some(bubble_id) = selected_image_id {
            bubble_id
        } else {
            let Some(page_rect) = canvas.page_scene_rect(page_idx) else {
                return;
            };
            let center = egui::pos2(
                page_rect.left() + page_rect.width() * ((crop_rect[0] + crop_rect[2]) * 0.5),
                page_rect.top() + page_rect.height() * ((crop_rect[1] + crop_rect[3]) * 0.5),
            );
            let Some(new_id) = canvas.create_image_bubble_at_scene_pos(ctx, center) else {
                return;
            };
            new_id
        };

        let _ = canvas.set_bubble_class_for_bid(bubble_id, BubbleClass::Image);
        let mut patch = Map::new();
        patch.insert(
            "image_source_type".to_string(),
            Value::String("page_crop".to_string()),
        );
        patch.insert(
            "crop_page_idx".to_string(),
            Value::Number(u64::try_from(page_idx).unwrap_or(u64::MAX).into()),
        );
        patch.insert(
            "crop_rect".to_string(),
            Value::Array(
                crop_rect
                    .iter()
                    .map(|value| Value::from(f64::from(*value)))
                    .collect(),
            ),
        );
        // The image area rect (red) is owned by the canvas; keep `rect_coords` equal to the crop
        // region so the canvas red rect matches the selected crop and clamps text areas to it.
        patch.insert("rect_coords".to_string(), rect_coords_value(crop_rect));
        if canvas.patch_bubble_extra_fields(project, bubble_id, &patch) {
            canvas.flush_pending_bubble_upserts_now(project);
        }
    }

    pub(super) fn open_advanced_recognition_for_scene_rect(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
        scene_rect: Rect,
    ) {
        let request_id = self.next_ocr_request_id;
        let Some(built_request) = build_ocr_request(
            canvas,
            project,
            scene_rect,
            request_id,
            &self.ocr_panel_options,
            &self.mt_panel_options.source_lang,
            &self.neurolate_settings,
        ) else {
            self.push_toast(
                ctx,
                t!("translation.tab.selection_no_pages_status").to_string(),
                Severity::Warning,
                2.6,
            );
            return;
        };

        let selection = AdvancedRecognitionSelection {
            page_idx: built_request.page_idx,
            uv_rect: built_request.request.uv_rect,
        };
        match self
            .advanced_recognition
            .open_selection(selection, built_request.request.page_path.clone())
        {
            Ok(()) => {
                self.advanced_recognition_request = Some(built_request);
            }
            Err(error) => {
                self.advanced_recognition_request = None;
                self.push_toast(ctx, error, Severity::Error, 3.0);
            }
        }
    }

    fn dispatch_manual_ocr_request(
        &mut self,
        ctx: &egui::Context,
        built_request: BuiltOcrRequest,
        pending_insert: Option<OcrPendingBubbleInsert>,
        result_target: ManualOcrResultTarget,
    ) {
        let request_id = built_request.request.request_id;
        if let Some(insert) = pending_insert {
            self.pending_bubble_inserts.insert(request_id, insert);
        }
        self.manual_ocr_active_request_id = Some(request_id);
        self.manual_ocr_result_target = Some(result_target);

        let was_ready = self.ocr_controller.state() == OcrLoadState::Ready;
        if let Some(error) = self.current_ocr_torch_requirement_error() {
            self.manual_ocr_active_request_id = None;
            self.manual_ocr_result_target = None;
            self.pending_bubble_inserts.remove(&request_id);
            self.push_toast(ctx, error, Severity::Error, 2.6);
            return;
        }
        self.ocr_controller.request_recognize(built_request.request);
        if self.ocr_controller.state().is_busy() {
            self.mark_ocr_load_requested_for_engine(self.ocr_panel_options.engine);
        }
        if self.ocr_controller.state() == OcrLoadState::Error {
            self.manual_ocr_active_request_id = None;
            self.manual_ocr_result_target = None;
            self.pending_bubble_inserts.remove(&request_id);
        }
        if !was_ready {
            self.push_toast(ctx, t!("translation.tab.engine_loading_status").to_string(), Severity::Info, 2.2);
        }
    }

    pub(super) fn handle_advanced_recognition_window(
        &mut self,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        let Some(action) = self.advanced_recognition.draw(ctx, self.input_frame.as_ref(), &self.input_labels) else {
            return;
        };
        match action {
            AdvancedRecognitionAction::Recognize { image_override_png } => {
                if let Some(refusal) = self.ocr_request_refusal() {
                    self.push_toast(ctx, refusal, Severity::Error, 2.6);
                    return;
                }
                if let Some(error) = self.current_ocr_torch_requirement_error() {
                    self.push_toast(ctx, error, Severity::Error, 2.6);
                    return;
                }
                if self.manual_ocr_active_request_id.is_some() {
                    return;
                }
                let Some(mut built_request) = self.advanced_recognition_request.clone() else {
                    self.push_toast(
                        ctx,
                        t!("translation.tab.no_selection_status").to_string(),
                        Severity::Warning,
                        2.6,
                    );
                    return;
                };
                let request_id = self.next_ocr_request_id;
                self.next_ocr_request_id = self.next_ocr_request_id.saturating_add(1);
                built_request.request.request_id = request_id;
                built_request.request.image_override_png = image_override_png;
                self.advanced_recognition.set_request_running(request_id);
                self.dispatch_manual_ocr_request(
                    ctx,
                    built_request,
                    None,
                    ManualOcrResultTarget::AdvancedWindowText,
                );
                if self.manual_ocr_active_request_id != Some(request_id) {
                    let error = self
                        .ocr_controller
                        .last_error()
                        .unwrap_or(t!("translation.tab.start_recognition_error"))
                        .to_string();
                    let _ = self
                        .advanced_recognition
                        .apply_recognition_error(request_id, error);
                }
            }
            AdvancedRecognitionAction::QuickRecognizeSelection { image_override_png } => {
                if let Some(refusal) = self.ocr_request_refusal() {
                    self.push_toast(ctx, refusal, Severity::Error, 2.6);
                    return;
                }
                if let Some(error) = self.current_ocr_torch_requirement_error() {
                    self.push_toast(ctx, error, Severity::Error, 2.6);
                    return;
                }
                if self.manual_ocr_active_request_id.is_some() {
                    return;
                }
                let Some(mut built_request) = self.advanced_recognition_request.clone() else {
                    self.push_toast(
                        ctx,
                        t!("translation.tab.no_selection_status").to_string(),
                        Severity::Warning,
                        2.6,
                    );
                    return;
                };
                let request_id = self.next_ocr_request_id;
                self.next_ocr_request_id = self.next_ocr_request_id.saturating_add(1);
                built_request.request.request_id = request_id;
                built_request.request.image_override_png = Some(image_override_png);
                self.advanced_recognition.set_request_running(request_id);
                self.dispatch_manual_ocr_request(
                    ctx,
                    built_request,
                    None,
                    ManualOcrResultTarget::ToastAndClipboard,
                );
                if self.manual_ocr_active_request_id != Some(request_id) {
                    let error = self
                        .ocr_controller
                        .last_error()
                        .unwrap_or(t!("translation.tab.start_recognition_error"))
                        .to_string();
                    let _ = self
                        .advanced_recognition
                        .apply_recognition_error(request_id, error.clone());
                    self.push_toast(ctx, tf!("translation.tab.quick_ocr_error", error = error), Severity::Error, 3.0);
                }
            }
            AdvancedRecognitionAction::CreateBubble {
                page_idx,
                uv_rect,
                text,
            } => {
                if !text.trim().is_empty()
                    && canvas
                        .create_bubble_with_original_text_at_page_uv_rect(page_idx, uv_rect, text)
                {
                    canvas.flush_pending_bubble_upserts_now(project);
                }
                self.advanced_recognition_request = None;
                // The window's ORIGINAL page selection (never its edited / rotated preview),
                // with the engine parameters as they are now.
                if let Some(page) = project.pages.iter().find(|page| page.idx == page_idx) {
                    let options = build_ocr_runtime_options(&self.ocr_panel_options, &self.mt_panel_options.source_lang, &self.neurolate_settings, project.comic_type);
                    let engine = self.ocr_panel_options.engine;
                    let selection = detector_region::OcrSelection { page_idx, page_path: page.path.clone(), uv_rect };
                    // Never carried by an OCR answer: NeuroLate sends a separate `MASK` request.
                    self.start_selection_region_detection(ctx, project, engine, &options, selection, None);
                }
            }
            AdvancedRecognitionAction::Close => {
                self.advanced_recognition_request = None;
            }
        }
    }

    pub(super) fn start_ocr_for_scene_rect(
        &mut self,
        ctx: &egui::Context,
        canvas: &CanvasView,
        project: &ProjectData,
        scene_rect: Rect,
        recent_character_rank: Option<usize>,
    ) {
        if let Some(refusal) = self.ocr_request_refusal() {
            self.push_toast(ctx, refusal, Severity::Error, 2.6);
            return;
        }
        if let Some(error) = self.current_ocr_torch_requirement_error() {
            self.push_toast(ctx, error, Severity::Error, 2.6);
            return;
        }
        let request_id = self.next_ocr_request_id;
        self.next_ocr_request_id = self.next_ocr_request_id.saturating_add(1);

        let Some(mut built_request) = build_ocr_request(
            canvas,
            project,
            scene_rect,
            request_id,
            &self.ocr_panel_options,
            &self.mt_panel_options.source_lang,
            &self.neurolate_settings,
        ) else {
            self.push_toast(
                ctx,
                t!("translation.tab.selection_no_pages_status").to_string(),
                Severity::Warning,
                2.6,
            );
            return;
        };

        let pending_insert = if self.ocr_panel_options.create_bubble {
            Some(OcrPendingBubbleInsert {
                page_idx: built_request.page_idx,
                uv_rect: built_request.request.uv_rect,
                join_newlines: built_request.request.join_newlines,
                recent_character_rank,
            })
        } else {
            None
        };

        if self.manual_ocr_active_request_id.is_some() {
            self.push_toast(
                ctx,
                t!("translation.tab.ocr_already_running_status").to_string(),
                Severity::Warning,
                2.0,
            );
            return;
        }

        let region_engine = built_request.request.engine;
        let region_options = built_request.request.options.clone();
        let region_selection = detector_region::OcrSelection {
            page_idx: built_request.page_idx,
            page_path: built_request.request.page_path.clone(),
            uv_rect: built_request.request.uv_rect,
        };
        // NeuroLate: the same request also returns the region (none while the detector is busy).
        built_request.request.region = self.ocr_region_want_for(region_engine, &region_options, built_request.page_idx);
        let carried_by = built_request.request.region.is_some().then_some(request_id);
        self.dispatch_manual_ocr_request(
            ctx,
            built_request,
            pending_insert,
            ManualOcrResultTarget::AdvancedWindowText,
        );
        // The detector runs on the same selection only when the OCR itself was dispatched.
        if self.manual_ocr_active_request_id == Some(request_id) {
            self.start_selection_region_detection(ctx, project, region_engine, &region_options, region_selection, carried_by);
        }
    }
}

/// Snapshots the OCR panel state into worker options. `ocr_source_lang` is the OCR input
/// language (the MT panel's source-language code) the AI API default instruction is worded for;
/// with `neurolate` (the tab's NeuroLate settings) and `project_type` it also resolves the
/// NeuroLate request parameters (`crate::neurolate::request_params`).
pub(super) fn build_ocr_runtime_options(ocr_options: &OcrPanelOptions, ocr_source_lang: &str, neurolate: &NeurolateSettings, project_type: Option<ComicType>) -> OcrRuntimeOptions {
    let surya_is_active = ocr_options.engine == OcrEngine::Surya;
    OcrRuntimeOptions {
        manga_model: ocr_options.manga_model.clone(),
        paddle_lang: ocr_options.paddle_lang.clone(),
        paddle_vl_script: ocr_options.paddle_vl_script.clone(),
        paddle_vl_model: ocr_options.paddle_vl_model,
        easy_langs: ocr_options.easy_langs.clone(),
        surya_task_name: if surya_is_active {
            "ocr_without_boxes".to_string()
        } else {
            ocr_options.surya_task_name.clone()
        },
        surya_recognize_math: if surya_is_active {
            false
        } else {
            ocr_options.surya_recognize_math
        },
        surya_sort_lines: if surya_is_active {
            false
        } else {
            ocr_options.surya_sort_lines
        },
        surya_drop_repeated_text: if surya_is_active {
            false
        } else {
            ocr_options.surya_drop_repeated_text
        },
        surya_max_sliding_window: if surya_is_active {
            0
        } else {
            ocr_options.surya_max_sliding_window
        },
        surya_max_tokens: if surya_is_active {
            0
        } else {
            ocr_options.surya_max_tokens
        },
        paddle_detect_mask: ocr_options.paddle_detect_mask,
        surya_detect_mask: ocr_options.surya_detect_mask,
        ai_api_service: ocr_options.ai_api.service,
        ai_api_base_url: ocr_options.ai_api.base_url.clone(),
        ai_api_model: ocr_options.ai_api.model.clone(),
        ai_api_system_instruction: ocr_options.ai_api.system_instruction.clone(),
        ai_api_source_lang: ocr_source_lang.to_string(),
        neurolate: crate::neurolate::request_params(neurolate, ocr_source_lang, project_type),
        neurolate_detect_mask: neurolate.detect_mask,
    }
}

/// Builds the recognize request for a canvas selection on the page it overlaps most (`None`
/// when it overlaps no page). `ocr_source_lang` and `neurolate` as in
/// `build_ocr_runtime_options`. The request carries no region (the quick-selection trigger
/// sets it).
fn build_ocr_request(
    canvas: &CanvasView,
    project: &ProjectData,
    selection_rect: Rect,
    request_id: u64,
    ocr_options: &OcrPanelOptions,
    ocr_source_lang: &str,
    neurolate: &NeurolateSettings,
) -> Option<BuiltOcrRequest> {
    let mut best_area = 0.0_f32;
    let mut best_page: Option<(&Page, Rect)> = None;

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
            best_page = Some((page, page_rect));
        }
    }

    let (page, page_rect) = best_page?;
    let crop_scene = page_rect.intersect(selection_rect);
    if !crop_scene.is_positive() {
        return None;
    }

    let page_w = page_rect.width().max(1.0);
    let page_h = page_rect.height().max(1.0);
    let u1 = ((crop_scene.left() - page_rect.left()) / page_w).clamp(0.0, 1.0);
    let v1 = ((crop_scene.top() - page_rect.top()) / page_h).clamp(0.0, 1.0);
    let u2 = ((crop_scene.right() - page_rect.left()) / page_w).clamp(0.0, 1.0);
    let v2 = ((crop_scene.bottom() - page_rect.top()) / page_h).clamp(0.0, 1.0);

    Some(BuiltOcrRequest {
        request: OcrRecognizeRequest {
            request_id,
            engine: ocr_options.engine,
            options: build_ocr_runtime_options(ocr_options, ocr_source_lang, neurolate, project.comic_type),
            page_path: page.path.clone(),
            uv_rect: [u1, v1, u2, v2],
            image_override_png: None,
            join_newlines: ocr_options.join_newlines,
            reflect_strings: ocr_options.reflect_strings,
            char_replacements: ocr_options.runtime_char_replacements(),
            fix_caps_lock: ocr_options.fix_caps_lock,
            region: None,
        },
        page_idx: page.idx,
    })
}

/// Parses the persisted `char_replacements` array into editable UI rules.
///
/// Each element must be an object with `enabled` (bool, default `true`),
/// `targets` (string), and `replacement` (string). Non-object entries are
/// skipped so a malformed settings file cannot abort project loading.
pub(super) fn parse_char_replacement_rules(rules: &[Value]) -> Vec<CharReplacementRuleUi> {
    rules
        .iter()
        .filter_map(Value::as_object)
        .map(|rule| CharReplacementRuleUi {
            enabled: rule.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            targets_raw: rule
                .get("targets")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            replacement: rule
                .get("replacement")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
        .collect()
}

pub(super) fn build_bubble_original_text(
    result: &crate::ocr::OcrRecognizeResult,
    join_newlines: bool,
) -> String {
    let lines = result
        .lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return result.text.trim().to_string();
    }
    let separator = if join_newlines { "\n" } else { " " };
    lines.join(separator)
}
