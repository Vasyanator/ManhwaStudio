/*
FILE OVERVIEW: crates/ms-tab-translation/src/tab/neurolate.rs
The Translation tab's NeuroLate wiring: the tab's ONE key panel (`neurolate_key`, shared by the
detector panel and the OCR panel) and the request parameters resolved from the tab's ONE
NeuroLate settings store (`neurolate_settings`).

Main items:
- `pump_neurolate_key`: runs the key panel's background step with the actions of the panel
  drawn this frame (the detector panel or the OCR panel, each with NeuroLate selected) and turns
  its notices into toasts.
- `poll_neurolate_key`: the per-frame step while a key operation or limit check is in flight
  (so an answer lands even when the panel is closed).
- `neurolate_params`: the `OcrParams` of every NeuroLate request of the tab; `neurolate_detect`
  adds the detector mode's "recognize text right away".

Tests here also pin the NeuroLate keys of the tab's persistence and block OCR's retry policy
(`block_ocr_retry_after_failure`, defined in `tab.rs`).

Notes:
The key panel is pumped only where NeuroLate UI is drawn, or while its own work is in flight:
its first pump owes a credential-store check, and pumping it unconditionally would read the OS
keyring (possibly prompting to unlock it) at every studio open for users who never use
NeuroLate. GUI thread; never blocks (the panel runs its I/O on workers).
*/

use eframe::egui;
use ms_ai_api::neurolate::{NeurolateKeyActions, OcrParams};
use ms_project::ProjectData;
use web_time::Duration;

use super::TranslationTabState;
use crate::neurolate::request_params;
use crate::text_detector::NeurolateDetect;

/// How often a frame is requested while the key panel waits for its worker.
const KEY_PANEL_POLL: Duration = Duration::from_millis(100);

impl TranslationTabState {
    /// One step of the NeuroLate key panel with `actions` from the panel drawn this frame:
    /// starts what was requested, applies finished work, shows its notices as toasts, and keeps
    /// frames coming while work is in flight. Never blocks.
    pub(super) fn pump_neurolate_key(&mut self, ctx: &egui::Context, actions: NeurolateKeyActions) {
        for notice in self.neurolate_key.pump(actions) {
            self.push_toast(ctx, notice.text, notice.severity, notice.duration_s);
        }
        if self.neurolate_key_in_flight() {
            ctx.request_repaint_after(KEY_PANEL_POLL);
        }
    }

    /// The per-frame step of the key panel while its work is in flight (none otherwise, see the
    /// file header).
    pub(super) fn poll_neurolate_key(&mut self, ctx: &egui::Context) {
        if self.neurolate_key_in_flight() {
            self.pump_neurolate_key(ctx, NeurolateKeyActions::default());
        }
    }

    /// Whether a key operation or a limit check runs or is owed.
    fn neurolate_key_in_flight(&self) -> bool {
        self.neurolate_key.key_state().is_busy() || self.neurolate_key.is_checking_limit()
    }

    /// The parameters of every NeuroLate request of this tab: the tab's NeuroLate settings with
    /// "Auto" resolved from the MT source language and `project`'s comic type.
    pub(super) fn neurolate_params(&self, project: &ProjectData) -> OcrParams {
        request_params(&self.neurolate_settings, &self.mt_panel_options.source_lang, project.comic_type)
    }

    /// The NeuroLate page detection of this tab: [`Self::neurolate_params`] plus the detector
    /// mode's "recognize text right away" (`MASK_AND_TEXT` and block texts when on).
    pub(super) fn neurolate_detect(&self, project: &ProjectData) -> NeurolateDetect {
        NeurolateDetect { params: self.neurolate_params(project), with_text: self.neurolate_settings.detect_with_text }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        CompositionPanelOptions, MtPanelOptions, OcrPanelOptions, TextDetectorOcrTask, TextDetectorPanelOptions, Value,
        block_ocr_retry_after_failure,
    };
    use super::super::settings_io::{apply_translation_settings_sections, ocr_engine_to_project_key, parse_ocr_engine_key, parse_text_detector_algorithm_key};
    use crate::ocr::OcrEngine;
    use crate::neurolate::{NeurolateLanguageChoice, NeurolateSettings, NeurolateTypeChoice, settings_from_json};
    use crate::panels::text_detector::TextDetectorAlgorithm;
    use ms_ai_api::neurolate::NeurolateLanguage;
    use serde_json::json;

    /// The settings written by the real settings writer under `OCR.params.neurolate` read back
    /// equal; sibling keys of the section survive.
    #[test]
    fn neurolate_settings_round_trip_through_the_settings_writer() {
        let settings = NeurolateSettings { language: NeurolateLanguageChoice::Fixed(NeurolateLanguage::Ja), comic_type: NeurolateTypeChoice::Manga, detect_mask: true, detect_with_text: true };
        let mut root = json!({ "OCR": { "params": { "neurolate": { "custom": 1 } } } }).as_object().cloned().unwrap_or_default();
        apply_translation_settings_sections(
            &mut root,
            &OcrPanelOptions::default(),
            &MtPanelOptions::default(),
            &CompositionPanelOptions::default(),
            &TextDetectorPanelOptions::default(),
            &settings,
        );
        let params = root.get("OCR").and_then(|ocr| ocr.get("params")).and_then(Value::as_object);
        assert_eq!(settings_from_json(params), settings);
        assert_eq!(params.and_then(|params| params.get("neurolate")).and_then(|section| section.get("custom")), Some(&json!(1)));
    }

    #[test]
    fn neurolate_ocr_engine_key_round_trips() {
        assert_eq!(ocr_engine_to_project_key(OcrEngine::NeuroLate), "neurolate");
        assert_eq!(parse_ocr_engine_key(ocr_engine_to_project_key(OcrEngine::NeuroLate)), OcrEngine::NeuroLate);
        assert_eq!(parse_ocr_engine_key(" NeuroLate "), OcrEngine::NeuroLate);
    }

    /// Block OCR retries a failed first attempt once, except for NeuroLate (a paid request is
    /// never sent twice automatically) and an unknown engine; a retried task is not retried again.
    #[test]
    fn block_ocr_never_retries_a_failed_neurolate_request() {
        let task = TextDetectorOcrTask { page_idx: 2, uv_rect: [0.1, 0.2, 0.3, 0.4], retry_attempt: 0 };
        let retried = block_ocr_retry_after_failure(task, Some(OcrEngine::PaddleOcr)).unwrap_or_else(|| panic!("first PaddleOCR failure is retried"));
        assert_eq!((retried.page_idx, retried.uv_rect, retried.retry_attempt), (2, [0.1, 0.2, 0.3, 0.4], 1));
        assert!(block_ocr_retry_after_failure(retried, Some(OcrEngine::PaddleOcr)).is_none(), "only once");
        assert!(block_ocr_retry_after_failure(task, Some(OcrEngine::AiApi)).is_some());
        assert!(block_ocr_retry_after_failure(task, Some(OcrEngine::NeuroLate)).is_none(), "paid request");
        assert!(block_ocr_retry_after_failure(task, None).is_none());
    }

    #[test]
    fn neurolate_detector_key_round_trips() {
        assert_eq!(parse_text_detector_algorithm_key(TextDetectorAlgorithm::NeuroLate.key()), TextDetectorAlgorithm::NeuroLate);
        assert_eq!(parse_text_detector_algorithm_key(" NeuroLate "), TextDetectorAlgorithm::NeuroLate);
    }
}
