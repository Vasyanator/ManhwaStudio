/*
File: tools/watermark_removal/settings.rs

Purpose:
Behaviour of the tool's mode and settings types and of the per-session state: mode
availability per session, the network mode's backend ids, settings clamping and the engine
parameters derived from them, and the per-region-editor session state (run channel, undo stack
of the last runs, mask preview texture).

Key items:
- `impl WatermarkMode`, `effective_watermark_mode()`, `offered_watermark_modes()`
- `impl WatermarkNetworkMode`, `impl WatermarkRemovalSettings`
- `impl WatermarkSessionState`

Notes:
The types are declared in `mod.rs`.
*/
use crate::tools::base::{RegionEditorSession, build_tinted_mask_preview};
use crate::tools::mask_generation::{
    DEFAULT_WATERMARK_MODEL, WatermarkProgress, watermark_model_spec,
};
use ms_backend_ipc as backend_ipc;
use crate::watermark_chapter::{DetectionParams, SampleParams};
use eframe::egui;
use egui::TextureOptions;
use ms_thread as thread;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use super::{
    WATERMARK_DILATE_MAX, WATERMARK_MASK_PREVIEW_RGB, WATERMARK_OVERLAP_MAX,
    WATERMARK_THRESHOLD_MAX, WATERMARK_THRESHOLD_MIN, WATERMARK_TILE_MAX, WATERMARK_TILE_MIN,
    WATERMARK_TILE_MULTIPLE, WatermarkJobResult, WatermarkMode, WatermarkNetworkMode,
    WatermarkRemovalSettings, WatermarkSessionState,
};
use super::backend::run_watermark;


impl WatermarkMode {
    /// Persisted/wire value of the mode.
    pub(super) fn wire(self) -> &'static str {
        match self {
            Self::MaskOnly => "mask_only",
            Self::Clean => "clean",
            Self::Chapter => "chapter",
        }
    }

    /// Parses a persisted value, falling back to the safe mask-only default for
    /// anything unknown (a settings file from a newer build must not break).
    pub(super) fn from_wire(value: &str) -> Self {
        match value.trim() {
            "clean" => Self::Clean,
            "chapter" => Self::Chapter,
            _ => Self::MaskOnly,
        }
    }

    /// Localized name of the mode as shown in the mode picker.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::MaskOnly => t!("cleaning.tools.watermark.mode_mask_only"),
            Self::Clean => t!("cleaning.tools.watermark.mode_clean"),
            Self::Chapter => t!("cleaning.tools.watermark.chapter.mode_chapter"),
        }
    }

    /// The backend-facing mode, or `None` for the local chapter engine.
    ///
    /// Every path that talks to the Python backend goes through this, so "which IPC
    /// method does the chapter mode call" cannot be asked: the type says it calls none.
    pub(super) fn network(self) -> Option<WatermarkNetworkMode> {
        match self {
            Self::MaskOnly => Some(WatermarkNetworkMode::MaskOnly),
            Self::Clean => Some(WatermarkNetworkMode::Clean),
            Self::Chapter => None,
        }
    }

    /// Whether selecting this mode requires a working Torch runtime.
    ///
    /// The chapter mode is deliberately AI-free. This gates the two NETWORK entries of the
    /// mode picker and the run button INSIDE the tool; it deliberately does NOT reach
    /// `CleaningTool::pytorch_required`, which is a constant `false` so the picker button —
    /// and `ensure_active_tool_available` behind it — can never make the chapter mode
    /// unreachable on a machine without Torch.
    pub(super) fn requires_torch(self) -> bool {
        self.network().is_some()
    }
}

/// The mode the tool runs in this session: the persisted one, except that a session without the
/// series watermark library (`chapter_mode_offered` false: a single-image session) never runs
/// `Chapter`.
///
/// The chapter mode keys the GLOBAL watermark library by the series folder
/// (`chapter_source_ref`: the chapter directory's parent), which in a single-image session is
/// the throwaway scratch root — every run would file its measurements under a meaningless,
/// per-session source key. It falls back to the safe default `MaskOnly` for the session only;
/// the persisted value is never rewritten by this (`draw_mode_picker` writes only a real pick).
pub(super) fn effective_watermark_mode(persisted: &str, chapter_mode_offered: bool) -> WatermarkMode {
    match WatermarkMode::from_wire(persisted) {
        WatermarkMode::Chapter if !chapter_mode_offered => WatermarkMode::MaskOnly,
        mode => mode,
    }
}

/// The modes the picker offers: all three in a project, the two network modes in a
/// single-image session (see `effective_watermark_mode`).
pub(super) fn offered_watermark_modes(chapter_mode_offered: bool) -> &'static [WatermarkMode] {
    if chapter_mode_offered {
        &[WatermarkMode::MaskOnly, WatermarkMode::Clean, WatermarkMode::Chapter]
    } else {
        &[WatermarkMode::MaskOnly, WatermarkMode::Clean]
    }
}

impl WatermarkNetworkMode {
    /// Localized caption of the run button, which states what the run will do.
    pub(super) fn run_button_label(self) -> &'static str {
        match self {
            Self::MaskOnly => t!("cleaning.tools.watermark.detect_button"),
            Self::Clean => t!("cleaning.tools.watermark.clean_button"),
        }
    }

    /// IPC method the mode calls.
    pub(super) fn ipc_method(self) -> &'static str {
        match self {
            Self::MaskOnly => backend_ipc::protocol::METHOD_WATERMARK_DETECT,
            Self::Clean => backend_ipc::protocol::METHOD_WATERMARK_REMOVE,
        }
    }
}

impl Default for WatermarkRemovalSettings {
    fn default() -> Self {
        let detection = DetectionParams::default();
        let sample = SampleParams::default();
        Self {
            mode: WatermarkMode::MaskOnly.wire().to_string(),
            model: DEFAULT_WATERMARK_MODEL.to_string(),
            tile: 512,
            overlap: 64,
            threshold: 0.5,
            dilate_px: 4,
            show_mask_preview: true,
            // The engine's own measured defaults, so the settings file starts out
            // agreeing with the constants the plan justifies.
            chapter_anchor_tolerance_px: detection.anchor_tolerance,
            chapter_background_blur_px: detection.background_blur_radius,
            chapter_ring_width_px: sample.ring_width,
        }
    }
}

impl WatermarkRemovalSettings {
    /// Returns a copy with every field forced into its supported range: the mode
    /// and model fall back to the defaults when unknown, the tile is snapped down
    /// to a multiple of 16 inside `[256, 1024]`, the overlap is capped at half the
    /// tile, a non-finite threshold falls back to the default, and the dilation is
    /// capped. This is the ONLY value ever put on the wire.
    #[must_use]
    pub(super) fn normalized(&self) -> Self {
        let tile = (self.tile.clamp(WATERMARK_TILE_MIN, WATERMARK_TILE_MAX)
            / WATERMARK_TILE_MULTIPLE)
            * WATERMARK_TILE_MULTIPLE;
        let tile = tile.max(WATERMARK_TILE_MIN);
        let threshold = if self.threshold.is_finite() {
            self.threshold
                .clamp(WATERMARK_THRESHOLD_MIN, WATERMARK_THRESHOLD_MAX)
        } else {
            Self::default().threshold
        };
        // The chapter parameters are normalized by the engine itself, which is where the
        // measured hard bounds live; running them through it here keeps the settings file
        // and the values actually used identical.
        let detection = DetectionParams {
            anchor_tolerance: self.chapter_anchor_tolerance_px,
            background_blur_radius: self.chapter_background_blur_px,
            ..DetectionParams::default()
        }
        .normalized();
        let sample = SampleParams {
            ring_width: self.chapter_ring_width_px,
            ..SampleParams::default()
        }
        .normalized();
        Self {
            mode: WatermarkMode::from_wire(&self.mode).wire().to_string(),
            model: watermark_model_spec(&self.model).id.to_string(),
            tile,
            overlap: self.overlap.min(WATERMARK_OVERLAP_MAX).min(tile / 2),
            threshold,
            dilate_px: self.dilate_px.min(WATERMARK_DILATE_MAX),
            show_mask_preview: self.show_mask_preview,
            chapter_anchor_tolerance_px: detection.anchor_tolerance,
            chapter_background_blur_px: detection.background_blur_radius,
            chapter_ring_width_px: sample.ring_width,
        }
    }

    /// Detection parameters of the chapter engine, already normalized.
    pub(super) fn chapter_detection_params(&self) -> DetectionParams {
        DetectionParams {
            anchor_tolerance: self.chapter_anchor_tolerance_px,
            background_blur_radius: self.chapter_background_blur_px,
            ..DetectionParams::default()
        }
        .normalized()
    }

    /// Calibration-sample parameters of the chapter engine, already normalized.
    pub(super) fn chapter_sample_params(&self) -> SampleParams {
        SampleParams {
            ring_width: self.chapter_ring_width_px,
            ..SampleParams::default()
        }
        .normalized()
    }
}

impl WatermarkSessionState {
    /// Drops everything tied to a previous region when the editor opened a new one.
    pub(super) fn sync_session(&mut self, scroll_id: u64) {
        if self.scroll_id == Some(scroll_id) {
            return;
        }
        self.scroll_id = Some(scroll_id);
        self.undo_stack.clear();
        self.mask_preview = None;
        self.mask_texture = None;
        // Dropping the receiver detaches the in-flight worker: its result is
        // discarded instead of landing on the new region.
        self.run_rx = None;
    }

    /// Clears the whole session (Escape, tool deactivation, editor closed).
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Starts a run on a worker thread. A second run is refused while one is in
    /// flight; the status line explains that.
    pub(super) fn start_run(
        &mut self,
        editor: &mut RegionEditorSession,
        settings: &WatermarkRemovalSettings,
        progress: &Arc<Mutex<WatermarkProgress>>,
        mode: WatermarkNetworkMode,
    ) {
        if self.run_rx.is_some() {
            editor.status =
                Some(t!("cleaning.mask_editor.processing_already_running_status").to_string());
            return;
        }
        let image = editor.image.clone();
        let settings = settings.normalized();
        let progress = Arc::clone(progress);
        let (tx, rx) = mpsc::channel::<WatermarkJobResult>();
        thread::spawn(move || {
            let result = run_watermark(&image, &settings, &progress, mode);
            let _ = tx.send(WatermarkJobResult {
                source: image,
                result,
            });
        });
        self.run_rx = Some(rx);
        editor.status = Some(t!("cleaning.mask_editor.processing_background_status").to_string());
    }

    /// Polls the run channel and applies a finished run. Returns `true` while a
    /// run is still in flight.
    pub(super) fn poll_run(&mut self, editor: &mut RegionEditorSession) -> bool {
        let Some(rx) = self.run_rx.as_ref() else {
            return false;
        };
        match rx.try_recv() {
            Ok(job) => {
                self.run_rx = None;
                match job.result {
                    Ok(outcome) => {
                        if let Some(image) = outcome.image {
                            // Only a run that actually replaced pixels becomes an
                            // undo entry; a mask-only run has nothing to revert.
                            self.undo_stack.push(job.source);
                            editor.image = image;
                            editor.texture_dirty = true;
                            editor.status =
                                Some(t!("cleaning.mask_editor.processing_done_status").to_string());
                        } else {
                            editor.status = Some(
                                t!("cleaning.tools.watermark.mask_ready_status").to_string(),
                            );
                        }
                        self.set_mask_preview(&outcome.mask);
                    }
                    Err(err) => {
                        editor.status =
                            Some(tf!("cleaning.mask_editor.processing_error", err = err));
                    }
                }
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => {
                self.run_rx = None;
                editor.status =
                    Some(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string());
                false
            }
        }
    }

    /// Restores the region image from before the last applied run.
    pub(super) fn undo_last_run(&mut self, editor: &mut RegionEditorSession) {
        let Some(image) = self.undo_stack.pop() else {
            editor.status = Some(t!("cleaning.mask_editor.no_state_for_undo_status").to_string());
            return;
        };
        editor.image = image;
        editor.texture_dirty = true;
        editor.status = Some(t!("cleaning.mask_editor.reverted_status").to_string());
    }

    /// Replaces the mask overlay with a tinted copy of `mask` and invalidates its
    /// texture (a new upload is cheaper and simpler than patching in place).
    fn set_mask_preview(&mut self, mask: &egui::ColorImage) {
        self.mask_preview = Some(build_tinted_mask_preview(mask, WATERMARK_MASK_PREVIEW_RGB));
        self.mask_texture = None;
    }

    /// Uploads the mask overlay texture if one is missing.
    pub(super) fn ensure_mask_texture(&mut self, ctx: &egui::Context, scroll_id: u64) {
        let Some(preview) = self.mask_preview.as_ref() else {
            self.mask_texture = None;
            return;
        };
        if self.mask_texture.is_some() {
            return;
        }
        self.mask_texture = Some(ctx.load_texture(
            format!("cleaning-watermark-mask-{scroll_id}"),
            preview.clone(),
            TextureOptions::LINEAR,
        ));
    }
}
