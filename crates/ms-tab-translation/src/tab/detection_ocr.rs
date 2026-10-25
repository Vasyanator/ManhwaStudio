/*
File: crates/ms-tab-translation/src/tab/detection_ocr.rs

Purpose:
Detector runs and the detector-block OCR batch: which pages have detected blocks, the detector
run mode and its status text, starting detection for the current page or all pages, and the
sequential per-block OCR scheduler with its delayed retry, finish and abort.

Key functions:
- TranslationTabState::has_detected_blocks_on_page / has_detected_blocks_any /
  detected_page_indices / collect_detected_block_rects_px / textdetector_ocr_is_running.
- text_detector_run_mode / text_detector_running_status.
- start_text_detection_for_current_page / start_text_detection_for_all_pages.
- start_text_detector_ocr_for_indices / maybe_dispatch_next_textdetector_ocr_request /
  finish_textdetector_ocr_if_done / abort_textdetector_ocr.
*/

use ms_canvas::CanvasView;
use ms_project::ProjectData;
use crate::ocr::{OcrLoadState, OcrRecognizeRequest};
use crate::panels::text_detector::TextDetectorAlgorithm;
use crate::text_detector::{TextDetectorRect, TextDetectorRunMode};
use eframe::egui;
use ms_theme::Severity;
use std::collections::VecDeque;
use super::{OcrPendingBubbleInsert, TEXT_DETECTOR_OCR_RETRY_DELAY_SECS, TextDetectorOcrRetryState, TextDetectorOcrTask, TranslationTabState};
use super::ocr_flow::build_ocr_runtime_options;
use super::detection_mask::detector_blocks_with_options;
use super::block_bubbles;

impl TranslationTabState {
    pub(super) fn has_detected_blocks_on_page(&self, page_idx: usize) -> bool {
        !self.collect_detected_block_rects_px(page_idx).is_empty()
    }

    pub(super) fn has_detected_blocks_any(&self) -> bool {
        self.text_detector_results
            .keys()
            .copied()
            .any(|idx| self.has_detected_blocks_on_page(idx))
    }

    pub(super) fn detected_page_indices(&self) -> Vec<usize> {
        let mut out = self
            .text_detector_results
            .keys()
            .copied()
            .filter(|idx| self.has_detected_blocks_on_page(*idx))
            .collect::<Vec<_>>();
        out.sort_unstable();
        out
    }

    fn collect_detected_block_rects_px(&self, page_idx: usize) -> Vec<TextDetectorRect> {
        let Some(result) = self.text_detector_results.get(&page_idx) else {
            return Vec::new();
        };
        detector_blocks_with_options(result, &self.text_detector_options)
    }

    pub(super) fn textdetector_ocr_is_running(&self) -> bool {
        self.textdetector_ocr_active_request_id.is_some()
            || self.textdetector_ocr_retry_state.is_some()
            || !self.pending_textdetector_ocr_tasks.is_empty()
            || (self.textdetector_ocr_total > 0
                && self.textdetector_ocr_done < self.textdetector_ocr_total)
    }

    /// Builds the run mode for the selected algorithm (`TextDetectorPanelOptions::run_mode`,
    /// the same mode the panel's plan notice uses), or the localized refusal text when
    /// [`TextDetectorAlgorithm::availability`] says it cannot run now.
    fn text_detector_run_mode(&self, project: &ProjectData) -> Result<TextDetectorRunMode, String> {
        let algorithm = self.text_detector_options.algorithm;
        self.text_detector_availability().map_err(|reason| reason.status_text(algorithm))?;
        Ok(self.text_detector_options.run_mode(self.neurolate_detect(project)))
    }

    pub(super) fn text_detector_running_status(&self) -> &'static str {
        match self.text_detector_options.algorithm {
            TextDetectorAlgorithm::Classic => t!("translation.tab.detecting_status"),
            TextDetectorAlgorithm::PaddleOcr => t!("translation.tab.detecting_paddle_status"),
            TextDetectorAlgorithm::Ai => t!("translation.tab.detecting_ai_status"),
            TextDetectorAlgorithm::Surya => t!("translation.tab.detecting_surya_status"),
            TextDetectorAlgorithm::NeuroLate => t!("translation.tab.detecting_neurolate_status"),
        }
    }

    pub(super) fn start_text_detection_for_current_page(
        &mut self,
        project: &ProjectData,
        canvas: &CanvasView,
    ) {
        if self.detector_busy_for_batches() {
            return;
        }
        let page_idx = canvas.current_page_idx();
        let Some(page) = project.pages.iter().find(|page| page.idx == page_idx) else {
            self.set_text_detector_status(t!("translation.tab.current_page_not_found_status"), Severity::Error);
            return;
        };
        let mode = match self.text_detector_run_mode(project) {
            Ok(mode) => mode,
            Err(error) => {
                self.set_text_detector_status(error, Severity::Error);
                return;
            }
        };
        match self.text_detector_controller.start_detection(
            vec![(page.idx, page.path.clone())],
            false,
            mode,
            self.text_detector_options.mask_dilate_size,
        ) {
            Ok(()) => {
                self.text_detector_progress = Some((0, 1));
                self.set_text_detector_status(
                    self.text_detector_running_status(),
                    Severity::Info,
                );
            }
            Err(error) => {
                self.set_text_detector_status(
                    tf!("translation.tab.detector_start_error", error = error),
                    Severity::Error,
                );
            }
        }
    }

    pub(super) fn start_text_detection_for_all_pages(&mut self, project: &ProjectData) {
        if self.detector_busy_for_batches() {
            return;
        }
        let pages = project
            .pages
            .iter()
            .map(|page| (page.idx, page.path.clone()))
            .collect::<Vec<_>>();
        let mode = match self.text_detector_run_mode(project) {
            Ok(mode) => mode,
            Err(error) => {
                self.set_text_detector_status(error, Severity::Error);
                return;
            }
        };
        let total = pages.len();
        match self.text_detector_controller.start_detection(
            pages,
            true,
            mode,
            self.text_detector_options.mask_dilate_size,
        ) {
            Ok(()) => {
                self.text_detector_progress = Some((0, total));
                self.set_text_detector_status(
                    self.text_detector_running_status(),
                    Severity::Info,
                );
            }
            Err(error) => {
                self.set_text_detector_status(
                    tf!("translation.tab.detector_start_error", error = error),
                    Severity::Error,
                );
            }
        }
    }

    pub(super) fn start_text_detector_ocr_for_indices(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        mut indices: Vec<usize>,
    ) {
        if let Err(off) = self.current_ocr_route_availability() {
            self.push_toast(ctx, off.user_message(), Severity::Warning, 2.6);
            return;
        }
        if let Some(error) = self.current_ocr_torch_requirement_error() {
            self.push_toast(ctx, error, Severity::Error, 2.6);
            return;
        }
        if self.ocr_controller.state() != OcrLoadState::Ready {
            self.set_text_detector_status(
                t!("translation.tab.engine_not_loaded_status"),
                Severity::Error,
            );
            return;
        }
        if self.textdetector_ocr_is_running() {
            return;
        }

        indices.sort_unstable();
        indices.dedup();

        let mut tasks = VecDeque::new();
        for page_idx in indices {
            let Some(result) = self.text_detector_results.get(&page_idx) else {
                continue;
            };
            let blocks = detector_blocks_with_options(result, &self.text_detector_options);
            for rect in blocks {
                let Some(uv_rect) = block_bubbles::block_uv_rect(&rect, result.source_size) else {
                    continue;
                };
                tasks.push_back(TextDetectorOcrTask { page_idx, uv_rect, retry_attempt: 0 });
            }
        }

        if tasks.is_empty() {
            self.set_text_detector_status(t!("translation.tab.no_blocks_status"), Severity::Error);
            self.text_detector_progress = None;
            return;
        }

        self.pending_textdetector_ocr_tasks = tasks;
        self.textdetector_ocr_active_request_id = None;
        self.textdetector_ocr_active_task = None;
        self.textdetector_ocr_active_engine = None;
        self.textdetector_ocr_retry_state = None;
        self.textdetector_ocr_total = self.pending_textdetector_ocr_tasks.len();
        self.textdetector_ocr_done = 0;
        self.textdetector_ocr_recognized = 0;
        self.text_detector_progress = Some((0, self.textdetector_ocr_total));
        self.set_text_detector_status(t!("translation.tab.block_recognition_status"), Severity::Info);
        self.maybe_dispatch_next_textdetector_ocr_request(ctx, project);
    }

    pub(super) fn maybe_dispatch_next_textdetector_ocr_request(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
    ) {
        if self.textdetector_ocr_total == 0 {
            return;
        }

        if self.textdetector_ocr_active_request_id.is_some() {
            self.text_detector_progress = Some((
                self.textdetector_ocr_done,
                self.textdetector_ocr_total.max(1),
            ));
            return;
        }

        if let Some(retry_state) = self.textdetector_ocr_retry_state {
            let now_s = ctx.input(|i| i.time);
            if now_s < retry_state.retry_at_s {
                self.text_detector_progress = Some((
                    self.textdetector_ocr_done,
                    self.textdetector_ocr_total.max(1),
                ));
                return;
            }
            self.textdetector_ocr_retry_state = None;
            self.pending_textdetector_ocr_tasks
                .push_front(retry_state.task);
            self.set_text_detector_status(t!("translation.tab.block_recognition_status"), Severity::Info);
        }

        if self.ocr_controller.state().is_busy() {
            self.text_detector_progress = Some((
                self.textdetector_ocr_done,
                self.textdetector_ocr_total.max(1),
            ));
            return;
        }

        while let Some(task) = self.pending_textdetector_ocr_tasks.pop_front() {
            let Some(page) = project.pages.iter().find(|page| page.idx == task.page_idx) else {
                self.textdetector_ocr_done = self.textdetector_ocr_done.saturating_add(1);
                continue;
            };

            let request_id = self.next_ocr_request_id;
            self.next_ocr_request_id = self.next_ocr_request_id.saturating_add(1);
            let request = OcrRecognizeRequest {
                request_id,
                engine: self.ocr_panel_options.engine,
                options: build_ocr_runtime_options(&self.ocr_panel_options, &self.mt_panel_options.source_lang, &self.neurolate_settings, project.comic_type),
                page_path: page.path.clone(),
                uv_rect: task.uv_rect,
                image_override_png: None,
                join_newlines: self.ocr_panel_options.join_newlines,
                reflect_strings: self.ocr_panel_options.reflect_strings,
                char_replacements: self.ocr_panel_options.runtime_char_replacements(),
                fix_caps_lock: self.ocr_panel_options.fix_caps_lock,
                // Block OCR reads text only (NeuroLate: `TEXT` mode), never a region.
                region: None,
            };
            if self.ocr_panel_options.create_bubble {
                self.pending_bubble_inserts.insert(
                    request_id,
                    OcrPendingBubbleInsert {
                        page_idx: task.page_idx,
                        uv_rect: task.uv_rect,
                        join_newlines: self.ocr_panel_options.join_newlines,
                        recent_character_rank: None,
                    },
                );
            }

            self.ocr_controller.request_recognize(request);
            if self.ocr_controller.state().is_busy() {
                self.mark_ocr_load_requested_for_engine(self.ocr_panel_options.engine);
            }
            if self.ocr_controller.state() == OcrLoadState::Error {
                self.pending_bubble_inserts.remove(&request_id);
                if task.retry_attempt == 0 {
                    let now_s = ctx.input(|i| i.time);
                    self.textdetector_ocr_retry_state = Some(TextDetectorOcrRetryState {
                        task: TextDetectorOcrTask {
                            retry_attempt: 1,
                            ..task
                        },
                        retry_at_s: now_s + TEXT_DETECTOR_OCR_RETRY_DELAY_SECS,
                    });
                    self.set_text_detector_status(
                        tf!(
                            "translation.tab.recognition_unavailable_retry_status",
                            sec = format!("{:.0}", TEXT_DETECTOR_OCR_RETRY_DELAY_SECS)
                        ),
                        Severity::Warning,
                    );
                    break;
                }
                self.abort_textdetector_ocr(
                    ctx,
                    t!("translation.tab.recognition_start_stopped_status").to_string(),
                    Severity::Error,
                );
                break;
            }
            self.textdetector_ocr_active_request_id = Some(request_id);
            self.textdetector_ocr_active_task = Some(task);
            self.textdetector_ocr_active_engine = Some(self.ocr_panel_options.engine);
            break;
        }

        self.text_detector_progress = Some((
            self.textdetector_ocr_done,
            self.textdetector_ocr_total.max(1),
        ));
        self.finish_textdetector_ocr_if_done(ctx, false);
    }

    fn finish_textdetector_ocr_if_done(&mut self, ctx: &egui::Context, force_error: bool) {
        if self.textdetector_ocr_total == 0
            || self.textdetector_ocr_active_request_id.is_some()
            || self.textdetector_ocr_retry_state.is_some()
            || !self.pending_textdetector_ocr_tasks.is_empty()
            || (!force_error && self.textdetector_ocr_done < self.textdetector_ocr_total)
        {
            return;
        }

        let total = self.textdetector_ocr_total;
        let recognized = self.textdetector_ocr_recognized;
        self.textdetector_ocr_total = 0;
        self.textdetector_ocr_done = 0;
        self.textdetector_ocr_recognized = 0;
        self.textdetector_ocr_active_request_id = None;
        self.textdetector_ocr_active_task = None;
        self.textdetector_ocr_active_engine = None;
        self.textdetector_ocr_retry_state = None;
        self.pending_textdetector_ocr_tasks.clear();
        self.text_detector_progress = None;

        if force_error {
            self.set_text_detector_status(
                t!("translation.tab.engine_not_loaded_status"),
                Severity::Error,
            );
            return;
        }

        if recognized > 0 {
            self.set_text_detector_status(
                tf!("translation.tab.recognition_done_blocks_status", recognized = recognized, total = total),
                Severity::Success,
            );
            self.push_toast(
                ctx,
                tf!("translation.tab.detector_recognition_progress_status", recognized = recognized, total = total),
                Severity::Success,
                2.4,
            );
        } else {
            self.set_text_detector_status(
                t!("translation.tab.recognition_done_no_text_status"),
                Severity::Warning,
            );
        }
    }

    /// Hard-resets the detector block-recognition batch and shows `message` with `severity`
    /// (`Error` after a fatal condition, `Info` for a user stop).
    pub(super) fn abort_textdetector_ocr(&mut self, ctx: &egui::Context, message: String, severity: Severity) {
        self.textdetector_ocr_total = 0;
        self.textdetector_ocr_done = 0;
        self.textdetector_ocr_recognized = 0;
        self.textdetector_ocr_active_request_id = None;
        self.textdetector_ocr_active_task = None;
        self.textdetector_ocr_active_engine = None;
        self.textdetector_ocr_retry_state = None;
        self.pending_textdetector_ocr_tasks.clear();
        self.text_detector_progress = None;
        self.set_text_detector_status(message, severity);
        ctx.request_repaint();
    }
}
