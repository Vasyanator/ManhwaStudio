/*
File: crates/ms-tab-translation/src/tab/ocr_routing.rs

Purpose:
Per-engine OCR load states, the pushed AI-backend health copy and probe requests, the external
OCR / detector model downloads, and the route gates that decide whether an OCR engine or a
detector algorithm may run in this process (native, backend or cloud).

Key functions:
- TranslationTabState::ocr_state_for_engine / set_ocr_state_for_engine / mark_*: per-engine
  OCR status (`ocr_engine_states`), synced from the health snapshot by
  `sync_ocr_states_from_backend_health_snapshot`.
- apply_ocr_model_panel_changes / apply_detector_model_panel_action / poll_ocr_model_downloads:
  the external-model download controller shared by the OCR and detector panels.
- request_ai_backend_health_check / refresh_ai_backend_health_snapshot_cache /
  ai_backend_unavailable: throttled health probe and the per-frame health copy.
- selected_ocr_route_availability / current_ocr_route_availability / ocr_request_refusal /
  text_detector_availability / detector_algorithm_availability: route availability through
  `ms_local_ai::local_ai::AiRouteKind::availability` (the one owner of that rule).
- gate_over_ocr_route_inputs (native / wasm twins) and refresh_ocr_route_inputs_cache
  (desktop-only): the cached routing inputs of `tab/route_inputs.rs`.
- current_ocr_torch_requirement_error: the "PyTorch not installed" status of torch-only modes.

Notes:
- The two `gate_over_ocr_route_inputs` cfg twins stay together in this file.
*/

use ms_local_ai::backend_health::{AiBackendHealthSnapshot, AiBackendProbeCommand, paddle_vl_ready_for};
use ms_local_ai::local_ai::{AiRouteKind, LocalAiOff};
use crate::ocr::{OcrEngine, OcrLoadState, external_model_specs};
use crate::panels::ocr_model_download::ModelDownloadAction;
use ms_sysprobe::ai_models::external::ExternalModelSpec;
use ms_sysprobe::ai_models::external_catalog::PaddleVlVariant;
use crate::panels::ocr::selected_mode_requirement;
use crate::panels::text_detector::{DetectorUnavailable, TextDetectorAlgorithm};
use crate::text_detector::SURYA_DETECTOR_SPECS;
use ms_widgets::AiRequirement;
use eframe::egui;
use web_time::Duration;
use super::{OCR_HEALTH_CHECK_THROTTLE_SECS, TranslationTabState};
#[cfg(not(target_arch = "wasm32"))]
use super::OCR_ROUTE_INPUTS_CACHE_TTL;
#[cfg(not(target_arch = "wasm32"))]
use super::route_inputs;

impl TranslationTabState {
    fn ocr_engine_index(engine: OcrEngine) -> usize {
        match engine {
            OcrEngine::MangaOcr => 0,
            OcrEngine::EasyOcr => 1,
            OcrEngine::PaddleOcr => 2,
            OcrEngine::Surya => 3,
            OcrEngine::AiApi => 4,
            OcrEngine::PaddleVl => 5,
            OcrEngine::Baberu => 6,
            OcrEngine::NeuroLate => 7,
        }
    }

    pub(super) fn ocr_state_for_engine(&self, engine: OcrEngine) -> OcrLoadState {
        self.ocr_engine_states[Self::ocr_engine_index(engine)]
    }

    pub(super) fn set_ocr_state_for_engine(&mut self, engine: OcrEngine, state: OcrLoadState) {
        self.ocr_engine_states[Self::ocr_engine_index(engine)] = state;
    }

    pub(super) fn mark_ocr_load_requested_for_engine(&mut self, engine: OcrEngine) {
        self.set_ocr_state_for_engine(engine, OcrLoadState::Loading);
        self.ocr_loading_engine = Some(engine);
    }

    pub(super) fn mark_ocr_model_download_started_for_engine(&mut self, engine: OcrEngine) {
        self.set_ocr_state_for_engine(engine, OcrLoadState::DownloadingModel);
        self.ocr_loading_engine = Some(engine);
    }

    pub(super) fn sync_ocr_states_from_backend_health_snapshot(&mut self) {
        let snapshot = self.ai_backend_health_snapshot();
        if !snapshot.connected {
            return;
        }
        let manga_ready = snapshot.ocr_manga_ready;
        let easy_ready = snapshot.ocr_easy_ready;
        let paddle_ready = snapshot.ocr_paddle_ready;
        // Per variant: a resident engine of another PaddleOCR-VL variant is not "ready".
        let paddle_vl_ready = paddle_vl_ready_for(snapshot, self.ocr_panel_options.paddle_vl_model.key());
        let surya_ready = snapshot.ocr_surya_ready;
        let baberu_ready = snapshot.ocr_baberu_ready;
        let sync_engine = |this: &mut Self, engine: OcrEngine, ready: Option<bool>| {
            let Some(ready) = ready else {
                return;
            };
            if ready {
                this.set_ocr_state_for_engine(engine, OcrLoadState::Ready);
            } else if !this.ocr_state_for_engine(engine).is_busy() {
                this.set_ocr_state_for_engine(engine, OcrLoadState::NotLoaded);
            }
        };
        sync_engine(self, OcrEngine::MangaOcr, manga_ready);
        sync_engine(self, OcrEngine::EasyOcr, easy_ready);
        sync_engine(self, OcrEngine::PaddleOcr, paddle_ready);
        sync_engine(self, OcrEngine::PaddleVl, paddle_vl_ready);
        sync_engine(self, OcrEngine::Surya, surya_ready);
        sync_engine(self, OcrEngine::Baberu, baberu_ready);
    }

    /// The external model bundle of the OCR panel's current selection
    /// (`ocr::external_model_specs`); empty for engines without one.
    pub(super) fn selected_ocr_model_specs(&self) -> Vec<&'static ExternalModelSpec> {
        let options = &self.ocr_panel_options;
        external_model_specs(options.engine, options.paddle_vl_model, &options.easy_langs)
    }

    /// Applies the OCR panel's external-model outcome of one frame: a requested download /
    /// cancel, a fresh status probe when the selected bundle changed (engine, PaddleOCR-VL
    /// variant or the EasyOCR languages' script; an earlier "installed" may be stale), and
    /// `NotLoaded` for PaddleOCR-VL after a variant switch (the loaded weights are the old
    /// variant's). `specs_before` is the bundle selected at the start of the frame.
    pub(super) fn apply_ocr_model_panel_changes(
        &mut self,
        specs_before: &[&'static ExternalModelSpec],
        variant_before: PaddleVlVariant,
        action: ModelDownloadAction,
    ) {
        let specs = self.selected_ocr_model_specs();
        if self.ocr_panel_options.paddle_vl_model != variant_before {
            self.set_ocr_state_for_engine(OcrEngine::PaddleVl, OcrLoadState::NotLoaded);
        }
        if specs != specs_before {
            self.ocr_model_downloads.request_bundle_status(&specs);
        }
        match action {
            ModelDownloadAction::Download if !specs.is_empty() => self.ocr_model_downloads.start_download(&specs),
            ModelDownloadAction::Verify if !specs.is_empty() => self.ocr_model_downloads.start_verify(&specs),
            ModelDownloadAction::Cancel if !specs.is_empty() => self.ocr_model_downloads.cancel(),
            ModelDownloadAction::None | ModelDownloadAction::Download | ModelDownloadAction::Verify | ModelDownloadAction::Cancel => {}
        }
    }

    /// Applies the detector panel's Surya-checkpoint download / verify request of one frame.
    pub(super) fn apply_detector_model_panel_action(&mut self, action: ModelDownloadAction) {
        match action {
            ModelDownloadAction::Download => self.ocr_model_downloads.start_download(&SURYA_DETECTOR_SPECS),
            ModelDownloadAction::Verify => self.ocr_model_downloads.start_verify(&SURYA_DETECTOR_SPECS),
            ModelDownloadAction::Cancel => self.ocr_model_downloads.cancel(),
            ModelDownloadAction::None => {}
        }
    }

    /// Per-frame poll of the external OCR model controller; keeps repainting while a
    /// download or status probe is in flight so progress stays live.
    pub(super) fn poll_ocr_model_downloads(&mut self, ctx: &egui::Context) {
        self.ocr_model_downloads.poll();
        if self.ocr_model_downloads.is_active() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Asks the shared backend probe thread for a health check when the snapshot is stale (or
    /// `force`), at most once per `OCR_HEALTH_CHECK_THROTTLE_SECS`. Non-blocking: only a channel
    /// send; a dead probe thread is logged once and the tab stops asking.
    pub(super) fn request_ai_backend_health_check(&mut self, ctx: &egui::Context, force: bool) {
        if self.local_ai.backend.is_err() {
            return;
        }
        let snapshot = self.ai_backend_health_snapshot();
        let now_s = ctx.input(|i| i.time);
        let stale = snapshot
            .checked_at
            .map(|checked| checked.elapsed().as_secs_f64() >= OCR_HEALTH_CHECK_THROTTLE_SECS)
            .unwrap_or(true);
        if (force || stale)
            && now_s - self.ocr_last_health_check_request_s >= OCR_HEALTH_CHECK_THROTTLE_SECS
            && let Some(tx) = self.ai_backend_probe_tx.as_ref()
        {
            if let Err(err) = tx.send(AiBackendProbeCommand::CheckNow) {
                // The probe thread is gone (process shutdown or a crashed probe); a disconnected
                // channel never reconnects, so drop the sender to log this once, not every
                // throttle period. The shared health snapshot then simply stops refreshing.
                ms_log::runtime_log::log_warn(format!(
                    "[translation] backend health check not requested: the probe thread is gone ({err}); further checks from this tab are disabled"
                ));
                self.ai_backend_probe_tx = None;
            }
            self.ocr_last_health_check_request_s = now_s;
        }
    }
}

impl TranslationTabState {
    pub(super) fn ocr_quick_selection_mode_active(&self) -> bool {
        self.input_mode_active(crate::input::OCR_QUICK_MODE)
    }

    pub(super) fn ocr_advanced_selection_mode_active(&self) -> bool {
        self.input_mode_active(crate::input::OCR_ADVANCED_MODE)
    }

    /// Refreshes the per-frame health copy; clones only when the shared snapshot's revision
    /// changed, so calling it every frame is cheap.
    pub(super) fn refresh_ai_backend_health_snapshot_cache(&mut self) {
        ms_local_ai::backend_health::refresh_cached_snapshot(&self.ai_backend_health, &mut self.ai_backend_health_cached);
    }

    fn ai_backend_health_snapshot(&self) -> &AiBackendHealthSnapshot {
        &self.ai_backend_health_cached
    }

    /// Whether the backend is known unusable: off for the process (`local_ai.backend`), or
    /// checked and not connected. A not-yet-probed backend counts as usable (optimistic).
    pub(super) fn ai_backend_unavailable(&self) -> bool {
        if self.local_ai.backend.is_err() {
            return true;
        }
        let snapshot = self.ai_backend_health_snapshot();
        snapshot.checked_at.is_some() && !snapshot.connected
    }

    /// The routing inputs (effective `AiRuntime` + provider-scoped SIGILL load guard) the
    /// route gates use, evaluated through `gate`: against the background-refreshed snapshot
    /// (see `refresh_ocr_route_inputs_cache`), or — before the first read landed —
    /// optimistically over the possible inputs (`route_inputs::gate_over_inputs`), so nothing
    /// is refused that the real inputs might allow. Never reads disk.
    #[cfg(not(target_arch = "wasm32"))]
    fn gate_over_ocr_route_inputs<T, E>(
        &self,
        gate: impl Fn((ms_config::AiRuntime, ms_config::OrtLoadDecision)) -> Result<T, E>,
    ) -> Result<T, E> {
        route_inputs::gate_over_inputs(self.ocr_route_inputs.snapshot(), gate)
    }

    /// Web build: no native runtime, so every local route is a backend route.
    #[cfg(target_arch = "wasm32")]
    fn gate_over_ocr_route_inputs<T, E>(
        &self,
        gate: impl Fn((ms_config::AiRuntime, ms_config::OrtLoadDecision)) -> Result<T, E>,
    ) -> Result<T, E> {
        gate((ms_config::AiRuntime::Backend, ms_config::OrtLoadDecision::Safe))
    }

    /// Route-aware gate for the selected OCR engine/model: what it runs on, or why it cannot
    /// run in this process (`crate::ocr::ocr_route_availability`, the single owner). An
    /// `Ok(AiRouteKind::Backend)` additionally needs a connected backend
    /// ([`Self::ai_backend_unavailable`]); AI API is always `Ok(Cloud)`, even under `--no-ai`.
    /// Uses the cached route inputs (no disk I/O); `&mut self` callers refresh them first
    /// through [`Self::current_ocr_route_availability`].
    pub(super) fn selected_ocr_route_availability(&self) -> Result<AiRouteKind, LocalAiOff> {
        self.gate_over_ocr_route_inputs(|(runtime, guard)| {
            crate::ocr::ocr_route_availability(
                self.ocr_panel_options.engine,
                &self.ocr_panel_options.manga_model,
                runtime,
                guard,
                &self.local_ai,
            )
        })
    }

    /// [`Self::selected_ocr_route_availability`] after refreshing the throttled route-input
    /// cache, for the OCR triggers and the panel.
    pub(super) fn current_ocr_route_availability(&mut self) -> Result<AiRouteKind, LocalAiOff> {
        #[cfg(not(target_arch = "wasm32"))]
        self.refresh_ocr_route_inputs_cache();
        self.selected_ocr_route_availability()
    }

    /// Whether the selected OCR engine/model runs on the Python backend in this process.
    pub(super) fn ocr_requires_backend_runtime(&mut self) -> bool {
        matches!(self.current_ocr_route_availability(), Ok(AiRouteKind::Backend))
    }

    /// Why an OCR request for the selected engine/model must be refused now, or `None`: the
    /// route's local runtime is off (its reason), or the route needs the backend and the
    /// backend is unavailable. The OCR triggers toast it as an error. Before the first
    /// route-inputs read landed a native-capable selection is not refused for the backend
    /// (see [`Self::gate_over_ocr_route_inputs`]); the OCR worker routes with the real inputs.
    pub(super) fn ocr_request_refusal(&mut self) -> Option<String> {
        match self.current_ocr_route_availability() {
            Err(off) => Some(off.user_message()),
            Ok(AiRouteKind::Backend) if self.ai_backend_unavailable() => {
                Some(t!("translation.tab.backend_unavailable_status").to_string())
            }
            // No OCR route runs on sd.cpp (`ocr_route_kind`); the arm only keeps the match total.
            Ok(AiRouteKind::Backend | AiRouteKind::Native | AiRouteKind::Cloud | AiRouteKind::SdCpp) => None,
        }
    }

    /// The text detector's availability for the selected algorithm: the PaddleOCR route from
    /// the cached route inputs (`detector_native_route`, the rule the detector worker applies),
    /// checked against `local_ai` and the backend's PyTorch report.
    pub(super) fn text_detector_availability(&self) -> Result<(), DetectorUnavailable> {
        self.detector_algorithm_availability(self.text_detector_options.algorithm)
    }

    /// [`Self::text_detector_availability`] for any `algorithm` (an OCR selection's region
    /// detection runs the OCR engine's detector, not necessarily the panel's).
    pub(super) fn detector_algorithm_availability(&self, algorithm: TextDetectorAlgorithm) -> Result<(), DetectorUnavailable> {
        let torch_available = self.ai_backend_torch_available();
        self.gate_over_ocr_route_inputs(|(runtime, guard)| {
            algorithm.availability(
                crate::text_detector::detector_native_route(runtime, guard),
                &self.local_ai,
                torch_available,
            )
        })
    }

    /// Non-blocking refresh step of the cached native-runtime routing inputs: adopts a
    /// background read that landed and, at most once per `OCR_ROUTE_INPUTS_CACHE_TTL` with
    /// none in flight, starts the next one (`route_inputs::spawn_route_inputs_read`). Never
    /// touches the disk on the calling (GUI) thread, so the per-frame gate calls it freely.
    /// Each landed snapshot republishes the native-ORT capability slot.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn refresh_ocr_route_inputs_cache(&mut self) {
        let landed = self.ocr_route_inputs.poll(
            web_time::Instant::now(),
            OCR_ROUTE_INPUTS_CACHE_TTL,
            route_inputs::spawn_route_inputs_read,
        );
        if let Some((runtime, guard)) = landed {
            // Publish native onnxruntime availability for UI gating. Native ORT is
            // "armed" — considered available even before its dylib loads — when native ONNX
            // is allowed in this process (`local_ai.native_onnx`: not `--no-ai`, and the
            // platform has it), the effective runtime is Native and the SIGILL load guard is
            // Safe. Otherwise native is not the ORT path, so this slot is false and ORT
            // availability falls to the backend slot (backend connected + providers).
            let native_armed = self.local_ai.native_onnx.is_ok()
                && runtime == ms_config::AiRuntime::Native
                && guard == ms_config::OrtLoadDecision::Safe;
            ms_sysprobe::ai_backend_capabilities::set_native_ort_available(Some(native_armed));
        }
    }

    fn ai_backend_torch_available(&self) -> Option<bool> {
        self.ai_backend_health_snapshot()
            .is_torch_available
            .or_else(ms_sysprobe::ai_backend_capabilities::torch_available)
    }

    /// Whether the selected OCR engine+model needs PyTorch. Delegates to the single owner of the
    /// selected mode's runtime requirement, `panels::ocr::selected_mode_requirement`.
    fn selected_ocr_mode_requires_torch(&self) -> bool {
        matches!(selected_mode_requirement(&self.ocr_panel_options), Some(AiRequirement::Torch))
    }

    pub(super) fn current_ocr_torch_requirement_error(&self) -> Option<String> {
        if !self.selected_ocr_mode_requires_torch() {
            return None;
        }
        matches!(self.ai_backend_torch_available(), Some(false))
            .then(|| t!("translation.common.pytorch_not_installed_status").to_string())
    }
}
