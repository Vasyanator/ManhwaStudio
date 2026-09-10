/*
File: cleaning/tools/ai_editor/engines/sdxl/engine.rs

Purpose:
`SdxlEngine` itself: the state it keeps, its `AiEngine` contract with the «ИИ-редактор
области» host, its parameter panel, and the polling that drains every worker channel it owns.

Main responsibilities:
- hold the two per-mode parameter sets, their persistence, the shared run progress, the
  latent-preview texture, the run channel and the unload channel;
- implement `AiEngine` — id, caption, section, constraints, mask layer, the run gate, the run
  itself and the per-frame `poll`;
- draw the «Редактор области» panel body: the mask note, the progress bar with the live
  latent preview, the collapsible generation parameters and the unload button.

Key structures:
- `SdxlEngine`

Notes:
Every long operation lives on a worker: the settings load and save, the unload call and the
run. `poll` only drains channels, and it is where the settings saver lives — the host polls
the selected engine EVERY frame, panel visible or not, so a finished run lands and a changed
parameter is written even with the panel closed. The saver has no time debounce: it writes on
the first poll `settings_save_due` answers yes on.

The mask is MANDATORY (`allows_empty_mask` is unconditionally `false`): SDXL inpainting has
no whole-region mode, so an empty mask describes no work at all. The host disables
«Обработать» on an empty mask and draws no «работает без маски» hint.

Parameters follow the SELECTED MODE, not the user: each mode owns a complete parameter set,
and the LaMa prefill picker is offered only in the 4-channel mode — the one that has a
prefill step. Switching mode therefore switches the whole parameter set and never merges the
two.
*/

use super::*;

/// The «SDXL Inpaint» engine: two channel modes behind one backend method and one run button.
///
/// Constructed once per session by `engines::all_engines`, which is why the constructor does
/// no GUI-thread I/O: the settings load is armed onto a worker instead.
pub struct SdxlEngine {
    /// The selected channel mode. Persisted, so it is restored with the parameters.
    mode: SdxlMode,
    /// Parameters of the 9-channel mode. Kept even while the 4-channel mode is selected.
    nine_channel: SdxlSettings,
    /// Parameters of the 4-channel mode. Kept even while the 9-channel mode is selected.
    four_channel: SdxlSettings,
    /// Channel of the initial settings load, `None` once it has landed.
    settings_rx: Option<Receiver<SdxlPersisted>>,
    /// Whether the initial load has landed. Gates saving: writing before it would overwrite
    /// the user's file with the in-memory defaults.
    settings_loaded: bool,
    /// Raised by every parameter change, cleared when a save is started. It is also what
    /// makes a user edit outrank a settings load that lands afterwards
    /// ([`Self::poll_settings_load`]).
    dirty: bool,
    /// Channel of the save in flight, which keeps at most one writer on the file.
    save_rx: Option<Receiver<()>>,
    /// Progress of the run in flight, written by the worker and read by the panel.
    progress: Arc<Mutex<SdxlSharedProgress>>,
    /// The uploaded latent preview, `None` while none has been produced.
    preview_texture: Option<egui::TextureHandle>,
    /// `SdxlSharedProgress::preview_seq` of the preview currently uploaded, so an unchanged
    /// preview is never re-uploaded to the GPU.
    preview_uploaded_seq: u64,
    /// Channel of the run in flight, `None` when idle.
    run_rx: Option<Receiver<Result<egui::ColorImage, String>>>,
    /// The engine's own status line under the parameters.
    run_status: Option<String>,
    /// Channel of the unload call in flight.
    unload_rx: Option<Receiver<Result<(), String>>>,
    /// What the last unload attempt reported.
    unload_status: Option<String>,
}

impl Default for SdxlEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SdxlEngine {
    /// Builds the engine and arms its settings load.
    #[must_use]
    pub fn new() -> Self {
        let mut engine = Self {
            mode: SdxlMode::NineChannel,
            nine_channel: SdxlSettings::for_mode(SdxlMode::NineChannel),
            four_channel: SdxlSettings::for_mode(SdxlMode::FourChannel),
            settings_rx: None,
            settings_loaded: false,
            dirty: false,
            save_rx: None,
            progress: Arc::new(Mutex::new(SdxlSharedProgress::default())),
            preview_texture: None,
            preview_uploaded_seq: 0,
            run_rx: None,
            run_status: None,
            unload_rx: None,
            unload_status: None,
        };
        engine.request_settings_load();
        engine
    }

    /// Reads the settings file on a worker thread (never on the GUI thread).
    fn request_settings_load(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.settings_rx = Some(rx);
        thread::spawn(move || {
            let _send_result = tx.send(load_sdxl_settings());
        });
    }

    /// Applies a finished settings load — UNLESS the user has already edited something, in
    /// which case the edit wins and the load is discarded.
    ///
    /// THE USER ALWAYS WINS, and this is the one place that decides it. The host draws an
    /// engine's panel body earlier in the frame than it polls that engine, so a value changed
    /// in the frames between construction and the load landing would otherwise be replaced by
    /// the file, silently. `dirty` is an exact witness of that case: nothing can clear it
    /// before the load lands, because [`Self::poll_and_maybe_save`] refuses to write until
    /// `settings_loaded` is set.
    ///
    /// The cost of choosing the edit is that the DISCARDED file is discarded whole — both
    /// parameter sets and the mode keep their defaults where the user did not touch them, and
    /// the save that follows writes them back. That is the accepted price of never losing a
    /// visible edit; per-field merging would need per-field dirt, which nothing else here has
    /// a use for.
    ///
    /// A disconnected channel keeps the in-memory settings and unblocks saving, so a crashed
    /// loader cannot freeze persistence forever.
    fn poll_settings_load(&mut self) {
        let Some(rx) = self.settings_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(persisted) => {
                if !self.dirty {
                    self.mode = SdxlMode::from_wire(&persisted.mode);
                    self.nine_channel = persisted.nine_channel;
                    self.four_channel = persisted.four_channel;
                }
                self.settings_loaded = true;
                self.settings_rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.settings_loaded = true;
                self.settings_rx = None;
            }
        }
    }

    /// Writes dirty settings on a worker thread, at most one save in flight, never before the
    /// initial load landed.
    ///
    /// Driven from [`AiEngine::poll`] and nowhere else, which is why the host must poll the
    /// selected engine every frame: without that call nothing ever writes the file and the
    /// mode and parameters are lost on exit, with no error anywhere.
    fn poll_and_maybe_save(&mut self) {
        if let Some(rx) = self.save_rx.as_ref() {
            match rx.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => self.save_rx = None,
                Err(TryRecvError::Empty) => return,
            }
        }
        if !settings_save_due(self.dirty, self.settings_loaded, self.save_rx.is_some()) {
            return;
        }
        self.dirty = false;
        let persisted = SdxlPersisted {
            mode: self.mode.wire().to_string(),
            nine_channel: self.nine_channel.clone(),
            four_channel: self.four_channel.clone(),
        };
        let (tx, rx) = mpsc::channel();
        self.save_rx = Some(rx);
        thread::spawn(move || {
            if let Err(err) = save_sdxl_settings(&persisted) {
                crate::runtime_log::log_warn(format!(
                    "[cleaning] failed to save the SDXL engine settings: {err}"
                ));
            }
            let _send_result = tx.send(());
        });
    }

    /// Drains the unload call and writes its outcome into the panel's status slot.
    fn poll_unload(&mut self) {
        let Some(rx) = self.unload_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.unload_status =
                    Some(t!("cleaning.tools.sdxl.unload_requested_status").to_string());
                self.unload_rx = None;
            }
            Ok(Err(err)) => {
                self.unload_status = Some(tf!("cleaning.inpaint.unload_error", err = err));
                self.unload_rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.unload_rx = None,
        }
    }

    /// Asks the backend to drop the resident SDXL pipeline, on a worker.
    fn request_unload(&mut self) {
        if self.unload_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.unload_rx = Some(rx);
        thread::spawn(move || {
            let _send_result = tx.send(unload_sdxl());
        });
    }

    /// Marks the settings dirty so the next `poll` starts a save.
    fn note_settings_changed(&mut self) {
        self.dirty = true;
    }

    /// The parameter set of the SELECTED mode.
    #[must_use]
    fn active_settings(&self) -> &SdxlSettings {
        match self.mode {
            SdxlMode::NineChannel => &self.nine_channel,
            SdxlMode::FourChannel => &self.four_channel,
        }
    }

    /// Draws the channel-mode picker and reports whether the selection changed.
    fn draw_mode_picker(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        let selected_text = self.mode.display_name();
        let mode = &mut self.mode;
        ui.horizontal(|ui| {
            ui.label(t!("cleaning.tools.sdxl.model_mode_label"));
            // A pinned `id_salt`: the visible caption is localized, so an id derived from it
            // would change with the UI language and drop the popup's stored state.
            WheelComboBox::from_id_salt("cleaning_sdxl_engine_mode_picker")
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    for entry in [SdxlMode::NineChannel, SdxlMode::FourChannel] {
                        changed |= ui
                            .selectable_value(mode, entry, entry.display_name())
                            .changed();
                    }
                });
        });
        ui.small(t!("cleaning.tools.sdxl.mode_description_hint"));
        changed
    }

    /// Draws the generation parameters of the SELECTED mode and reports whether one changed.
    ///
    /// The parameter NAMES «CFG» and «Denoise» stay literal: they are the backend's own
    /// `cfg_scale` / `denoise_strength` fields under the spelling every diffusion UI uses,
    /// and a translated caption would stop naming what a user reading the log sees.
    fn draw_mode_parameters(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        let is_four_channel = matches!(self.mode, SdxlMode::FourChannel);
        let settings = match self.mode {
            SdxlMode::NineChannel => &mut self.nine_channel,
            SdxlMode::FourChannel => &mut self.four_channel,
        };

        ui.label(t!("cleaning.tools.sdxl.weights_path_label"));
        changed |= ui.text_edit_singleline(&mut settings.model_path).changed();

        ui.label(t!("cleaning.tools.sdxl.positive_prompt_label"));
        changed |= ui
            .add(egui::TextEdit::multiline(&mut settings.positive_prompt).desired_rows(2))
            .changed();
        ui.label(t!("cleaning.tools.sdxl.negative_prompt_label"));
        changed |= ui
            .add(egui::TextEdit::multiline(&mut settings.negative_prompt).desired_rows(2))
            .changed();

        ui.horizontal(|ui| {
            ui.label(t!("cleaning.tools.sdxl.sampler_label"));
            WheelComboBox::from_id_salt("cleaning_sdxl_engine_sampler_picker")
                .selected_text(settings.sampler.clone())
                .show_ui(ui, |ui| {
                    for sampler in SDXL_SAMPLERS {
                        changed |= ui
                            .selectable_value(&mut settings.sampler, sampler.to_string(), sampler)
                            .changed();
                    }
                });
        });
        changed |= ui
            .add(
                WheelSlider::new(&mut settings.steps, SDXL_STEPS_MIN..=SDXL_STEPS_MAX)
                    .text(t!("cleaning.common.steps_label")),
            )
            .changed();
        changed |= ui
            .add(WheelSlider::new(&mut settings.cfg_scale, SDXL_CFG_MIN..=SDXL_CFG_MAX).text("CFG"))
            .changed();
        changed |= ui
            .add(
                WheelSlider::new(&mut settings.denoise_strength, SDXL_DENOISE_MIN..=SDXL_DENOISE_MAX)
                    .text("Denoise"),
            )
            .changed();
        changed |= ui
            .add(
                WheelSlider::new(
                    &mut settings.mask_dilation,
                    SDXL_MASK_DILATION_MIN..=SDXL_MASK_DILATION_MAX,
                )
                .text(t!("cleaning.common.mask_expand_label")),
            )
            .changed();
        changed |= ui
            .add(
                WheelSlider::new(&mut settings.mask_blur, SDXL_MASK_BLUR_MIN..=SDXL_MASK_BLUR_MAX)
                    .text(t!("cleaning.tools.sdxl.mask_blur_label")),
            )
            .changed();

        ui.horizontal(|ui| {
            ui.label(t!("cleaning.common.seed_label"));
            // `WheelSpinBox` rather than a raw `DragValue`: the panel lives inside a
            // `ScrollArea`, and only the `Wheel*` family consumes the wheel locally instead
            // of scrolling the panel out from under the cursor.
            changed |= ui
                .add(WheelSpinBox::new(&mut settings.seed).range(SDXL_RANDOM_SEED..=i64::MAX))
                .changed();
            // Restores the sentinel, which is what makes the backend draw a fresh seed for
            // every run — not one random number frozen into the settings file.
            if ui.small_button("🎲").clicked() {
                settings.seed = SDXL_RANDOM_SEED;
                changed = true;
            }
        });

        if is_four_channel {
            ui.separator();
            ui.label(t!("cleaning.tools.sdxl.lama_prefill_model_label"));
            let selected_label = lama_v2_model_catalog()
                .find(|spec| spec.file_name == settings.lama_model)
                .map_or_else(
                    || t!("cleaning.common.select_model_placeholder"),
                    LamaModelSpec::display_name,
                );
            WheelComboBox::from_id_salt("cleaning_sdxl_engine_lama_picker")
                .selected_text(selected_label)
                .show_ui(ui, |ui| {
                    for spec in lama_v2_model_catalog() {
                        changed |= ui
                            .selectable_value(
                                &mut settings.lama_model,
                                spec.file_name.to_string(),
                                spec.display_name(),
                            )
                            .changed();
                    }
                });
            ui.small(t!("cleaning.tools.sdxl.lama_prefill_hint"));
        }

        changed
    }
}

impl AiEngine for SdxlEngine {
    fn id(&self) -> &'static str {
        "sdxl"
    }

    fn title(&self) -> String {
        t!("cleaning.tools.sdxl.engine_title").to_string()
    }

    /// «С промптом»: every run is driven by the positive and negative prompt.
    fn section(&self) -> EngineSection {
        EngineSection::WithPrompt
    }

    fn requires_torch(&self) -> bool {
        true
    }

    /// Multiple of 8, shortest side 8 px, no area or aspect cap.
    ///
    /// The 8 is the SDXL VAE's downscale factor, so it is a model requirement rather than a
    /// UI preference. The frame snaps and validates a rectangle against this, and
    /// [`AiEngine::start`] re-checks the size it is actually handed against the same values.
    fn constraints(&self) -> FrameConstraints {
        FrameConstraints {
            multiple: SDXL_SELECTION_MULTIPLE,
            min_side: SDXL_MIN_SELECTION_PX,
            max_area: None,
            max_aspect: None,
        }
    }

    /// ONE layer: the area to REGENERATE. A second layer would have no meaning on the wire —
    /// each request carries exactly one mask.
    fn mask_layers(&self) -> Vec<MaskLayerSpec> {
        vec![MaskLayerSpec {
            tint: SDXL_MASK_TINT,
            label_key: "cleaning.tools.sdxl.mask_heading",
        }]
    }

    /// Unconditionally `false`: the mask is the inpainting hole, so an empty one describes no
    /// work at all and a request built from it could only answer with its own input. No
    /// parameter can change that, which is what makes the host's per-frame re-read free here.
    fn allows_empty_mask(&self) -> bool {
        false
    }

    fn draw_parameters(&mut self, ui: &mut egui::Ui) {
        // The mask MEANING, stated in the engine's own body: it is the inverse of FLUX.2
        // klein's, and the engines share one frame and one brush.
        ui.small(t!("cleaning.tools.sdxl.mask_meaning_hint"));
        ui.separator();

        {
            let Self { progress, preview_texture, preview_uploaded_seq, .. } = self;
            draw_sdxl_progress_ui(ui, progress, preview_texture, preview_uploaded_seq);
        }

        let mut changed = false;
        let mut unload_clicked = false;
        let unload_busy = self.unload_rx.is_some();
        // The fold stays CLOSED by default: the parameters are a long column and the
        // progress bar above them is what a run needs on screen.
        let params_open = egui::CollapsingHeader::new(t!("cleaning.tools.sdxl.params_heading"))
            .id_salt("cleaning_sdxl_engine_params")
            .default_open(false);
        params_open.show(ui, |ui| {
            changed |= self.draw_mode_picker(ui);
            ui.separator();
            changed |= self.draw_mode_parameters(ui);
            ui.separator();
            unload_clicked = ui
                .add_enabled(
                    !unload_busy,
                    egui::Button::new(t!("cleaning.tools.sdxl.unload_button")).small(),
                )
                .clicked();
            if let Some(status) = self.unload_status.as_ref() {
                ui.small(status);
            }
        });

        if let Some(status) = self.run_status.as_ref() {
            ui.small(status);
        }
        if unload_clicked {
            self.request_unload();
        }
        if changed {
            self.note_settings_changed();
        }
    }

    /// The engine's own half of the run gate.
    ///
    /// Two reasons, both of which no frame state describes: a run already in flight, and a
    /// missing weights path — SDXL has no default checkpoint to fall back on, so a run
    /// without one could only fail in the backend.
    fn run_block_reason(&self) -> Option<String> {
        if self.run_rx.is_some() {
            return Some(t!("cleaning.mask_editor.processing_already_running_status").to_string());
        }
        if self.active_settings().model_path.trim().is_empty() {
            return Some(t!("cleaning.tools.sdxl.weights_path_required_error").to_string());
        }
        None
    }

    /// Starts one pass on a worker thread.
    ///
    /// The worker also performs the ensure-before-run step of the 4-channel prefill
    /// checkpoint, which may download it — which is exactly why it may not happen on the GUI
    /// thread.
    ///
    /// # Errors
    /// Returns a localized message when a run is already in flight, when the region is empty,
    /// when the mask does not match the region the host declared, when the region violates
    /// the engine's own `constraints()`, or when no weights path is set.
    fn start(&mut self, request: EngineRunRequest) -> Result<(), String> {
        if let Some(reason) = self.run_block_reason() {
            return Err(reason);
        }
        let (width, height) = (request.rect_px.w, request.rect_px.h);
        if width == 0 || height == 0 {
            return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
        }
        // Re-validated rather than trusted: the host guarantees these sizes, and a request
        // that fails the check is refused here instead of being encoded onto the wire.
        if request.region.size != [width, height] {
            return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
        }
        let [mask] = request.masks.as_slice() else {
            return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
        };
        if mask.len() != width.saturating_mul(height) {
            return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
        }
        // The DECLARED constraints, re-checked against the size actually handed over. The
        // frame snaps its rectangle to the very same rule, so this can only fire when the
        // two disagree — and then the user is told which rule broke instead of the backend
        // refusing the blob.
        if let Some(refusal) = region_size_refusal(width, height, &self.constraints()) {
            return Err(refusal);
        }

        let cfg = SdxlRunConfig {
            mode: self.mode,
            settings: self.active_settings().clone(),
        };
        // Claimed on the GUI thread rather than on the worker: the claim then happens in the
        // order the user pressed the button, whatever order the threads start in.
        let generation = begin_progress_generation(&self.progress, cfg.settings.steps);
        let progress = Arc::clone(&self.progress);
        let mask = mask.clone();
        let region = request.region;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _send_result = tx.send(run_sdxl(&region, &mask, &cfg, &progress, generation));
        });
        self.run_rx = Some(rx);
        // Cleared, not set to «идёт обработка»: the progress bar above it, the host's own
        // panel line and the frame chrome already say a run is in flight. What this must not
        // do is keep showing the PREVIOUS run's outcome underneath a new one.
        self.run_status = None;
        Ok(())
    }

    fn poll(&mut self, ctx: &egui::Context) -> EnginePoll {
        self.poll_settings_load();
        self.poll_unload();
        self.poll_and_maybe_save();

        let Some(rx) = self.run_rx.as_ref() else {
            return EnginePoll::Idle;
        };
        match rx.try_recv() {
            Ok(Ok(image)) => {
                self.run_rx = None;
                self.run_status =
                    Some(t!("cleaning.mask_editor.processing_done_status").to_string());
                EnginePoll::Done(image)
            }
            Ok(Err(err)) => {
                self.run_rx = None;
                let message = tf!("cleaning.mask_editor.processing_error", err = err);
                self.run_status = Some(message.clone());
                EnginePoll::Failed(message)
            }
            Err(TryRecvError::Empty) => {
                // The progress frames arrive on a worker, not on user input, so the GUI has
                // to be woken or the bar would freeze until the next click.
                ctx.request_repaint();
                EnginePoll::Running
            }
            Err(TryRecvError::Disconnected) => {
                self.run_rx = None;
                let message =
                    t!("cleaning.mask_editor.processing_thread_crashed_error").to_string();
                self.run_status = Some(message.clone());
                EnginePoll::Failed(message)
            }
        }
    }

    /// Detaches the run in flight: its answer is dropped, its progress generation is retired
    /// so the abandoned worker can neither move nor stop the bar of whatever runs next, and
    /// the next poll answers `Idle`.
    ///
    /// The backend keeps working to the end of the pass: `call_streaming` never hands out the
    /// request id `Client::cancel` would need, so a backend-side stop is not available to
    /// this engine at all.
    fn cancel(&mut self) {
        if self.run_rx.take().is_some() {
            retire_progress_generation(&self.progress);
            self.run_status =
                Some(t!("cleaning.mask_editor.processing_cancelled_status").to_string());
        }
    }

    /// Deliberately empty: nothing here is gated on the backend's presence. A call made while
    /// the process is down fails with the unified offline message, which is a better answer
    /// than a control greyed out by a second copy of that fact.
    fn set_backend_available(&mut self, _available: bool) {}

    /// Deliberately empty: the run button is the host's `AiButton` with
    /// `AiRequirement::Torch`, which resolves the runtime's presence itself.
    fn set_torch_available(&mut self, _available: bool) {}

    /// Deliberately empty: nothing in this engine's panel depends on the rectangle before a
    /// run — there is no forecast to arm and no size to print.
    fn set_region(&mut self, _region: Option<OverlayRectPx>, _geometry_settled: bool) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `key` exists in the EMBEDDED English catalog, read straight from the tracked
    /// JSON.
    ///
    /// NOT `ms_i18n::lookup`: that answers from the process-global ACTIVE catalog, which
    /// nothing in this test binary installs, so an assertion on it would pass or fail
    /// depending on whether some other test happened to set a locale first.
    fn embedded_en_catalog_has(key: &str) -> bool {
        ms_i18n::catalog::embedded_locales()
            .iter()
            .find(|(tag, _)| *tag == "en")
            .and_then(|(_, source)| serde_json::from_str::<Value>(source).ok())
            .is_some_and(|value| value.get(key).is_some())
    }

    /// A fresh engine names itself, sits in the PROMPT section, and REQUIRES a mask — the
    /// rule that makes the host disable «Обработать» on an empty one.
    #[test]
    fn the_engine_requires_a_mask_and_lives_in_the_prompt_section() {
        let engine = SdxlEngine::new();
        assert_eq!(engine.id(), "sdxl");
        assert!(!engine.title().is_empty());
        assert_eq!(engine.section(), EngineSection::WithPrompt);
        assert!(engine.requires_torch());
        assert!(
            !engine.allows_empty_mask(),
            "SDXL inpainting has no whole-region mode: an empty mask describes no work"
        );
        let layers = engine.mask_layers();
        assert_eq!(layers.len(), 1, "one mask travels per request");
        assert_eq!(layers[0].tint, SDXL_MASK_TINT);
        assert!(embedded_en_catalog_has(layers[0].label_key));
        assert!(embedded_en_catalog_has("cleaning.tools.sdxl.engine_title"));
        assert!(embedded_en_catalog_has("cleaning.tools.sdxl.mask_meaning_hint"));
    }

    /// The frame constraints are the SDXL VAE's grid of 8 and nothing more: the pipeline caps
    /// neither the area nor the aspect ratio, so neither may appear here.
    #[test]
    fn the_constraints_are_a_grid_of_8_with_no_area_or_aspect_cap() {
        let constraints = SdxlEngine::new().constraints();
        assert_eq!(constraints.multiple, 8);
        assert_eq!(constraints.min_side, 8);
        assert!(constraints.max_area.is_none(), "the pipeline caps no region area");
        assert!(constraints.max_aspect.is_none(), "nor the aspect ratio");
    }

    /// Each mode owns a COMPLETE parameter set: selecting a mode selects its set whole, and
    /// editing one mode never reaches into the other.
    #[test]
    fn the_active_parameter_set_follows_the_selected_mode() {
        let mut engine = SdxlEngine::new();
        engine.nine_channel.model_path = "/nine.safetensors".to_string();
        engine.four_channel.model_path = "/four.safetensors".to_string();

        engine.mode = SdxlMode::NineChannel;
        assert_eq!(engine.active_settings().model_path, "/nine.safetensors");
        assert!((engine.active_settings().denoise_strength - 1.0).abs() < f32::EPSILON);

        engine.mode = SdxlMode::FourChannel;
        assert_eq!(engine.active_settings().model_path, "/four.safetensors");
        assert!(
            engine.active_settings().denoise_strength < 1.0,
            "the 4-channel default denoises over the LaMa prefill"
        );
        assert_eq!(
            engine.nine_channel.model_path, "/nine.safetensors",
            "switching mode must not reach into the other mode's parameters"
        );
    }

    /// The run gate closes on a run in flight and on a missing weights path — the two things
    /// no frame state describes. It reads the SELECTED mode's path, not the other one's.
    #[test]
    fn the_run_gate_reports_a_run_in_flight_and_a_missing_weights_path() {
        let mut engine = SdxlEngine::new();
        assert!(
            engine.run_block_reason().is_some(),
            "there is no default checkpoint, so a fresh engine cannot run"
        );

        engine.nine_channel.model_path = "/nine.safetensors".to_string();
        engine.mode = SdxlMode::NineChannel;
        assert!(engine.run_block_reason().is_none());

        engine.mode = SdxlMode::FourChannel;
        assert!(
            engine.run_block_reason().is_some(),
            "the gate must read the SELECTED mode's path"
        );
        engine.four_channel.model_path = "   ".to_string();
        assert!(engine.run_block_reason().is_some(), "whitespace is not a path");
        engine.four_channel.model_path = "/four.safetensors".to_string();
        assert!(engine.run_block_reason().is_none());

        let (_tx, rx) = mpsc::channel();
        engine.run_rx = Some(rx);
        assert!(engine.run_block_reason().is_some());
        engine.cancel();
        assert!(engine.run_block_reason().is_none(), "cancel releases the gate");
        assert!(engine.run_status.is_some(), "cancel says so in the status slot");
    }

    /// A cancelled run also retires its progress generation, so the detached worker's frames
    /// stop reaching the bar the moment the user cancels.
    #[test]
    fn cancelling_retires_the_progress_generation() {
        let mut engine = SdxlEngine::new();
        let generation = begin_progress_generation(&engine.progress, 30);
        publish_progress_frame(&engine.progress, generation, 5, 30, None);
        let (_tx, rx) = mpsc::channel();
        engine.run_rx = Some(rx);

        engine.cancel();
        publish_progress_frame(&engine.progress, generation, 6, 30, None);
        let guard = lock_progress(&engine.progress);
        assert!(!guard.active, "the bar is released at once");
        assert_eq!(guard.step, 0, "a frame from the abandoned run is dropped");
    }

    /// A malformed request is refused by `start` instead of being encoded onto the wire, and
    /// a refusal leaves the engine idle rather than half-started.
    #[test]
    fn start_refuses_a_request_whose_sizes_do_not_agree() {
        let rect = OverlayRectPx { x: 0, y: 0, w: 8, h: 8 };
        let mut engine = SdxlEngine::new();
        // Past the weights-path half of the gate, so the size checks are what is exercised.
        engine.nine_channel.model_path = "/nine.safetensors".to_string();

        let short_mask = EngineRunRequest {
            page_idx: 0,
            rect_px: rect,
            region: egui::ColorImage::new([8, 8], vec![Color32::BLACK; 64]),
            masks: vec![vec![0u8; 8]],
        };
        assert!(engine.start(short_mask).is_err());
        assert!(engine.run_rx.is_none(), "a refused start must leave no run behind");

        let wrong_layer_count = EngineRunRequest {
            page_idx: 0,
            rect_px: rect,
            region: egui::ColorImage::new([8, 8], vec![Color32::BLACK; 64]),
            masks: vec![vec![0u8; 64], vec![0u8; 64]],
        };
        assert!(engine.start(wrong_layer_count).is_err());

        let wrong_region = EngineRunRequest {
            page_idx: 0,
            rect_px: rect,
            region: egui::ColorImage::new([4, 4], vec![Color32::BLACK; 16]),
            masks: vec![vec![0u8; 64]],
        };
        assert!(engine.start(wrong_region).is_err());

        let empty_rect = EngineRunRequest {
            page_idx: 0,
            rect_px: OverlayRectPx { x: 0, y: 0, w: 0, h: 0 },
            region: egui::ColorImage::new([0, 0], Vec::new()),
            masks: vec![Vec::new()],
        };
        assert!(engine.start(empty_rect).is_err());
        assert!(engine.run_rx.is_none());
    }

    /// A region that breaks the DECLARED constraints is refused by `start`, not encoded onto
    /// the wire — the frame and the region it hands over can disagree.
    #[test]
    fn start_refuses_a_region_that_violates_the_declared_constraints() {
        for (w, h) in [(7usize, 8usize), (9, 16)] {
            let mut engine = SdxlEngine::new();
            // Past the weights-path half of the gate, so the size rule is what is exercised.
            engine.nine_channel.model_path = "/nine.safetensors".to_string();
            engine.mode = SdxlMode::NineChannel;
            let pixels = w * h;
            let request = EngineRunRequest {
                page_idx: 0,
                rect_px: OverlayRectPx { x: 0, y: 0, w, h },
                region: egui::ColorImage::new([w, h], vec![Color32::BLACK; pixels]),
                masks: vec![vec![255u8; pixels]],
            };
            assert!(
                engine.start(request).is_err(),
                "{w}x{h} is off the grid of 8 the engine declares"
            );
            assert!(engine.run_rx.is_none(), "a refused start must leave no run behind");
        }
    }

    /// An edit made before the load lands survives it: the file is discarded, the edit stays,
    /// and persistence is unblocked so the edit is what gets written.
    #[test]
    fn an_edit_made_before_the_load_lands_survives_it() {
        let mut engine = SdxlEngine::new();
        // A load that has NOT landed yet, whose payload differs from what the user picks.
        let (tx, rx) = mpsc::channel();
        engine.settings_rx = Some(rx);
        engine.settings_loaded = false;
        engine.dirty = false;

        let from_file = SdxlPersisted {
            mode: SdxlMode::FourChannel.wire().to_string(),
            nine_channel: SdxlSettings {
                model_path: "/from-file.safetensors".to_string(),
                ..SdxlSettings::for_mode(SdxlMode::NineChannel)
            },
            four_channel: SdxlSettings::for_mode(SdxlMode::FourChannel),
        };

        // The user edits in the same frame, BEFORE the poll that drains the channel.
        engine.nine_channel.model_path = "/typed-by-the-user.safetensors".to_string();
        engine.note_settings_changed();

        let _send_result = tx.send(from_file);
        engine.poll_settings_load();

        assert_eq!(
            engine.nine_channel.model_path, "/typed-by-the-user.safetensors",
            "the load must not overwrite an edit the user already made"
        );
        assert_eq!(engine.mode, SdxlMode::NineChannel, "nor the mode that edit was made in");
        assert!(engine.settings_loaded, "the load is complete either way: saving is unblocked");
        assert!(engine.dirty, "the edit is still pending a write");
        // The save GATE is asserted rather than driven: `poll_and_maybe_save` would write the
        // real settings file of whoever runs the test suite.
        assert!(
            settings_save_due(engine.dirty, engine.settings_loaded, engine.save_rx.is_some()),
            "the surviving edit is what gets written next"
        );
    }

    /// With nothing edited, the load applies as it always did.
    #[test]
    fn a_load_applies_when_the_user_has_changed_nothing() {
        let mut engine = SdxlEngine::new();
        let (tx, rx) = mpsc::channel();
        engine.settings_rx = Some(rx);
        engine.settings_loaded = false;
        engine.dirty = false;

        let from_file = SdxlPersisted {
            mode: SdxlMode::FourChannel.wire().to_string(),
            nine_channel: SdxlSettings {
                model_path: "/from-file.safetensors".to_string(),
                ..SdxlSettings::for_mode(SdxlMode::NineChannel)
            },
            four_channel: SdxlSettings::for_mode(SdxlMode::FourChannel),
        };
        let _send_result = tx.send(from_file);
        engine.poll_settings_load();

        assert_eq!(engine.mode, SdxlMode::FourChannel);
        assert_eq!(engine.nine_channel.model_path, "/from-file.safetensors");
        assert!(engine.settings_loaded);
    }

    /// Nothing is written before the initial load has landed, and a parameter change after it
    /// arms exactly one save.
    #[test]
    fn settings_are_never_saved_before_the_load_lands() {
        let mut engine = SdxlEngine::new();
        engine.settings_loaded = false;
        engine.note_settings_changed();
        engine.poll_and_maybe_save();
        assert!(
            engine.save_rx.is_none(),
            "saving the defaults before the load would clobber the user's file"
        );
        assert!(engine.dirty, "the change is still pending, not lost");
    }
}
