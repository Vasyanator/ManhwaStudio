/*
File: cleaning/tools/ai_editor/engines/lama/engine.rs

Purpose:
`LamaEngine` itself: the state it keeps, its `AiEngine` contract with the «ИИ-редактор
области» host, its parameter panel, and the polling that drains every worker channel it
owns.

Main responsibilities:
- hold the settings, the model-scan state, the run channel and the unload channel;
- implement `AiEngine` — id, caption, section, constraints, mask layer, the run gate, the
  run itself and the per-frame `poll`;
- draw the «Редактор области» panel body: the mask note, the model picker with its presence
  status, the parameters of the SELECTED model's method, and the unload button.

Key structures:
- `LamaEngine`

Notes:
Every long operation lives on a worker: the settings load and save, the presence scan, the
unload call and the run. `poll` only drains channels, and it is where the settings saver
lives — the host polls the selected engine EVERY frame, panel visible or not, so a finished
run lands and a changed parameter is written even with the panel closed. The saver has no
time debounce: it writes on the first poll `settings_save_due` answers yes on.

The mask is MANDATORY (`allows_empty_mask` is unconditionally `false`): the mask means
"remove what is under it", so with nothing painted there is nothing to remove. The host
disables «Обработать» on an empty mask and draws no «работает без маски» hint.

Parameters follow the SELECTED model, not the user: a LaMa-v2 entry shows refine and its
three refine parameters, the MPE entry shows `inpaint_size`, and neither is offered for a
method that would ignore it. Refine itself is closed — with a tooltip saying why — on an
entry whose `supports_refine` is `false`.
*/

use super::*;

/// The «Lama» engine: four checkpoints behind two backend methods, one run button.
///
/// Constructed once per session by `engines::all_engines`, which is why the constructor does
/// no GUI-thread I/O: the settings load and the first model scan are both armed onto
/// workers instead.
pub struct LamaEngine {
    /// The selected model and every parameter. Replaced wholesale by the initial load, and
    /// only while `dirty` is clear — an edit the user has already made outranks the file.
    settings: LamaSettings,
    /// Channel of the initial settings load, `None` once it has landed.
    settings_rx: Option<Receiver<LamaSettings>>,
    /// Whether the initial load has landed. Gates saving: writing before it would overwrite
    /// the user's file with the in-memory defaults.
    settings_loaded: bool,
    /// Raised by every parameter change, cleared when a save is started. It is also what
    /// makes a user edit outrank a settings load that lands afterwards
    /// ([`Self::poll_settings_load`]).
    dirty: bool,
    /// Channel of the save in flight, which keeps at most one writer on the file.
    save_rx: Option<Receiver<()>>,
    /// Lifecycle of the background presence scan of both model directories.
    model_list: LamaModelListState,
    /// Channel of the run in flight, `None` when idle.
    run_rx: Option<Receiver<Result<egui::ColorImage, String>>>,
    /// The engine's own status line under the parameters.
    run_status: Option<String>,
    /// Channel of the unload call in flight, paired with the METHOD it was sent for.
    ///
    /// The method travels with the channel rather than being re-read from the selection when
    /// the answer lands: the user may have switched model in between, and the confirmation
    /// would then name the family that was NOT unloaded.
    unload_rx: Option<(LamaMethod, Receiver<Result<(), String>>)>,
    /// What the last unload attempt reported.
    unload_status: Option<String>,
}

impl Default for LamaEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl LamaEngine {
    /// Builds the engine and arms its settings load; the presence scan arms itself on the
    /// first `poll`, which is also what re-arms it after «Проверить модели».
    #[must_use]
    pub fn new() -> Self {
        let mut engine = Self {
            settings: LamaSettings::default(),
            settings_rx: None,
            settings_loaded: false,
            dirty: false,
            save_rx: None,
            model_list: LamaModelListState::Idle,
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
            let _send_result = tx.send(load_lama_settings());
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
    /// The cost of choosing the edit is that the DISCARDED file is discarded whole — the
    /// fields the user did not touch keep their defaults and the save that follows writes
    /// them back. That is the accepted price of never losing a visible edit; per-field
    /// merging would need per-field dirt, which nothing else here has a use for.
    ///
    /// A disconnected channel keeps the in-memory settings and unblocks saving, so a crashed
    /// loader cannot freeze persistence forever.
    fn poll_settings_load(&mut self) {
        let Some(rx) = self.settings_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(settings) => {
                if !self.dirty {
                    self.settings = settings;
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

    /// Writes dirty settings on a worker thread, at most one save in flight, never before
    /// the initial load landed.
    ///
    /// Driven from [`AiEngine::poll`] and nowhere else, which is why the host must poll the
    /// selected engine every frame: without that call nothing ever writes the file and the
    /// selected model and parameters are lost on exit, with no error anywhere.
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
        let settings = self.settings.normalized();
        let (tx, rx) = mpsc::channel();
        self.save_rx = Some(rx);
        thread::spawn(move || {
            if let Err(err) = save_lama_settings(&settings) {
                ms_log::runtime_log::log_warn(format!(
                    "[cleaning] failed to save the Lama engine settings: {err}"
                ));
            }
            let _send_result = tx.send(());
        });
    }

    /// Starts the presence scan of both model directories on a worker thread.
    fn request_model_scan(&mut self) {
        if matches!(self.model_list, LamaModelListState::Loading(_)) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.model_list = LamaModelListState::Loading(rx);
        thread::spawn(move || {
            let _send_result = tx.send(scan_lama_models());
        });
    }

    /// Drains the presence scan, and re-arms it whenever the state is `Idle` — which is what
    /// the refresh button sets and what the engine starts in.
    fn poll_model_scan(&mut self) {
        if matches!(self.model_list, LamaModelListState::Idle) {
            self.request_model_scan();
            return;
        }
        let landed = match &self.model_list {
            LamaModelListState::Loading(rx) => match rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    t!("cleaning.tools.lama.model_scan_aborted_status").to_string(),
                )),
            },
            LamaModelListState::Idle
            | LamaModelListState::Ready(_)
            | LamaModelListState::Error(_) => None,
        };
        if let Some(result) = landed {
            self.model_list = match result {
                Ok(scan) => LamaModelListState::Ready(scan),
                Err(err) => LamaModelListState::Error(err),
            };
        }
    }

    /// Drains the unload call and writes its outcome into the panel's status slot.
    fn poll_unload(&mut self) {
        let Some((method, rx)) = self.unload_rx.as_ref() else {
            return;
        };
        let confirmation = method.unload_requested_status();
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.unload_status = Some(confirmation.to_string());
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

    /// Asks the backend to drop the model of the SELECTED entry's method, on a worker.
    fn request_unload(&mut self) {
        if self.unload_rx.is_some() {
            return;
        }
        let method = self.settings.spec().method;
        let (tx, rx) = mpsc::channel();
        self.unload_rx = Some((method, rx));
        thread::spawn(move || {
            let _send_result = tx.send(unload_lama(method));
        });
    }

    /// Marks the settings dirty so the next `poll` starts a save.
    fn note_settings_changed(&mut self) {
        self.dirty = true;
    }

    /// Draws the model picker row and its presence status; reports whether the selection
    /// changed and whether a rescan was asked for.
    fn draw_model_picker(&mut self, ui: &mut egui::Ui) -> (bool, bool) {
        let mut changed = false;
        let mut refresh_requested = false;
        // The caption is resolved from the CURRENT selection before the closure below
        // borrows `self.settings` mutably.
        let selected_text = self.settings.spec().display_name();
        let Self { settings, model_list, .. } = self;
        ui.horizontal(|ui| {
            ui.label(t!("cleaning.tools.lama.model_label"));
            // A pinned `id_salt`: the visible caption is localized, so an id derived from
            // it would change with the UI language and drop the popup's stored state.
            WheelComboBox::from_id_salt("cleaning_lama_engine_model_picker")
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    for entry in lama_model_catalog() {
                        changed |= ui
                            .selectable_value(
                                &mut settings.model,
                                entry.file_name.to_string(),
                                entry.display_name(),
                            )
                            .changed();
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.small(lama_model_status_text(&settings.model, model_list));
            if ui
                .small_button(t!("cleaning.tools.lama.refresh_models_button"))
                .clicked()
            {
                refresh_requested = true;
            }
        });
        if let LamaModelListState::Error(err) = model_list {
            ui.colored_label(LAMA_STATUS_ERROR_COLOR, err.as_str());
        }
        (changed, refresh_requested)
    }

    /// Draws the parameters of the SELECTED entry's method and reports whether one changed.
    ///
    /// The parameter NAMES stay literal: `refine`, `n_iters`, `max_scales`, `px_budget` and
    /// `inpaint_size` are the backend's own wire field names, and a translated spelling
    /// would stop naming the field a user reading the log or the backend docs sees.
    fn draw_method_parameters(&mut self, ui: &mut egui::Ui, spec: &LamaModelSpec) -> bool {
        let mut changed = false;
        let settings = &mut self.settings;
        match spec.method {
            LamaMethod::V2 => {
                let refine_response = ui.add_enabled(
                    spec.supports_refine,
                    egui::Checkbox::new(&mut settings.refine, "refine"),
                );
                changed |= refine_response.changed();
                // Says WHY the box is closed. Without it the control is merely grey, which
                // reads as a bug on a checkpoint the backend simply cannot refine.
                refine_response
                    .on_disabled_hover_text(t!("cleaning.tools.lama.refine_unsupported_hint"));
                let refine_active = spec.supports_refine && settings.refine;
                ui.add_enabled_ui(refine_active, |ui| {
                    changed |= ui
                        .add(
                            WheelSlider::new(
                                &mut settings.n_iters,
                                LAMA_N_ITERS_MIN..=LAMA_N_ITERS_MAX,
                            )
                            .text("n_iters"),
                        )
                        .changed();
                    changed |= ui
                        .add(
                            WheelSlider::new(
                                &mut settings.max_scales,
                                LAMA_MAX_SCALES_MIN..=LAMA_MAX_SCALES_MAX,
                            )
                            .text("max_scales"),
                        )
                        .changed();
                    changed |= ui
                        .add(
                            WheelSlider::new(
                                &mut settings.px_budget,
                                LAMA_PX_BUDGET_MIN..=LAMA_PX_BUDGET_MAX,
                            )
                            .text("px_budget"),
                        )
                        .changed();
                });
            }
            LamaMethod::Mpe => {
                changed |= ui
                    .add(
                        WheelSlider::new(
                            &mut settings.inpaint_size,
                            LAMA_INPAINT_SIZE_MIN..=LAMA_INPAINT_SIZE_MAX,
                        )
                        .text("inpaint_size"),
                    )
                    .changed();
            }
        }
        changed
    }
}

impl AiEngine for LamaEngine {
    fn id(&self) -> &'static str {
        "lama"
    }

    fn title(&self) -> String {
        t!("cleaning.tools.lama.engine_title").to_string()
    }

    /// «Без промпта»: LaMa takes an image and a mask and nothing else.
    fn section(&self) -> EngineSection {
        EngineSection::WithoutPrompt
    }

    fn requires_torch(&self) -> bool {
        true
    }

    /// Multiple of 8, shortest side 8 px, no area or aspect cap.
    ///
    /// The frame snaps and validates a rectangle against this, and [`AiEngine::start`]
    /// re-checks the size it is actually handed against the same values.
    fn constraints(&self) -> FrameConstraints {
        FrameConstraints {
            multiple: LAMA_SELECTION_MULTIPLE,
            min_side: LAMA_MIN_SELECTION_PX,
            max_area: None,
            max_aspect: None,
        }
    }

    /// ONE layer: the area to REMOVE. A second layer would have no meaning on the wire —
    /// each request carries exactly one mask.
    fn mask_layers(&self) -> Vec<MaskLayerSpec> {
        vec![MaskLayerSpec {
            tint: LAMA_MASK_TINT,
            label_key: "cleaning.tools.lama.mask_heading",
        }]
    }

    /// Unconditionally `false`: the mask says WHAT TO REMOVE, so an empty one describes no
    /// work at all. No parameter can change that, which is what makes the host's per-frame
    /// re-read free here.
    fn allows_empty_mask(&self) -> bool {
        false
    }

    fn draw_parameters(&mut self, ui: &mut egui::Ui) {
        // The mask MEANING, stated in the engine's own body: it is the inverse of FLUX.2
        // klein's, and the two engines share one frame and one brush.
        ui.small(t!("cleaning.tools.lama.mask_meaning_hint"));
        ui.separator();

        let (mut changed, refresh_requested) = self.draw_model_picker(ui);
        // Re-read AFTER the picker: the selection may have changed one line above, and the
        // parameters shown must be the new model's.
        let spec = self.settings.spec();
        egui::CollapsingHeader::new(t!("cleaning.tools.lama.params_heading"))
            .id_salt("cleaning_lama_engine_params")
            .default_open(true)
            .show(ui, |ui| {
                changed |= self.draw_method_parameters(ui, spec);
            });

        ui.separator();
        let unload_busy = self.unload_rx.is_some();
        let unload_clicked = ui
            .add_enabled(
                !unload_busy,
                egui::Button::new(spec.method.unload_button_label()).small(),
            )
            .clicked();
        if let Some(status) = self.unload_status.as_ref() {
            ui.small(status);
        }
        if let Some(status) = self.run_status.as_ref() {
            ui.small(status);
        }

        if refresh_requested {
            self.model_list = LamaModelListState::Idle;
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
    /// Only what no frame state describes: a run already in flight. The rectangle is
    /// validated against `constraints()` by the frame, the non-empty-mask rule is
    /// `allows_empty_mask`, and the PyTorch requirement is the host's `AiButton`.
    fn run_block_reason(&self) -> Option<String> {
        if self.run_rx.is_some() {
            return Some(t!("cleaning.mask_editor.processing_already_running_status").to_string());
        }
        None
    }

    /// Starts one pass on a worker thread.
    ///
    /// The worker also performs the ensure-before-run step, which may download the selected
    /// checkpoint — which is exactly why it may not happen on the GUI thread.
    ///
    /// # Errors
    /// Returns a localized message when a run is already in flight, when the region is
    /// empty, when the mask does not match the region the host declared, or when the region
    /// violates the engine's own `constraints()`.
    fn start(&mut self, request: EngineRunRequest) -> Result<(), String> {
        if self.run_rx.is_some() {
            return Err(t!("cleaning.mask_editor.processing_already_running_status").to_string());
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

        let settings = self.settings.normalized();
        let spec = settings.spec();
        let mask = mask.clone();
        let region = request.region;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _send_result = tx.send(run_lama(&region, &mask, spec, &settings));
        });
        self.run_rx = Some(rx);
        // Cleared, not set to «идёт обработка»: the host's own panel line and the frame
        // chrome already say a run is in flight. What this must not do is keep showing the
        // PREVIOUS run's outcome underneath a new one.
        self.run_status = None;
        Ok(())
    }

    fn poll(&mut self, ctx: &egui::Context) -> EnginePoll {
        self.poll_settings_load();
        self.poll_model_scan();
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
                // The scan is stale: the run may have downloaded the checkpoint it needed.
                self.model_list = LamaModelListState::Idle;
                EnginePoll::Done(image)
            }
            Ok(Err(err)) => {
                self.run_rx = None;
                let message = tf!("cleaning.mask_editor.processing_error", err = err);
                self.run_status = Some(message.clone());
                EnginePoll::Failed(message)
            }
            Err(TryRecvError::Empty) => {
                // The answer arrives on a worker, not on user input, so the GUI has to be
                // woken or a finished run would sit in the channel until the next click.
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

    /// Detaches the run in flight: its answer is dropped and the next poll answers `Idle`.
    ///
    /// The backend keeps working to the end of the pass. `inpaint.lama_v2` /
    /// `inpaint.lama_mpe` are plain request/response calls, and the plain `call` never hands
    /// out the request id `Client::cancel` would need, so a backend-side stop is not
    /// available to this engine at all.
    fn cancel(&mut self) {
        if self.run_rx.take().is_some() {
            self.run_status =
                Some(t!("cleaning.mask_editor.processing_cancelled_status").to_string());
        }
    }

    /// Deliberately empty: nothing here is gated on the backend's presence. A call made
    /// while the process is down fails with the unified offline message, which is a better
    /// answer than a control greyed out by a second copy of that fact.
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

    /// A fresh engine names itself, sits in the no-prompt section, and REQUIRES a mask —
    /// the rule that makes the host disable «Обработать» on an empty one.
    #[test]
    fn the_engine_requires_a_mask_and_lives_in_the_no_prompt_section() {
        let engine = LamaEngine::new();
        assert_eq!(engine.id(), "lama");
        assert!(!engine.title().is_empty());
        assert_eq!(engine.section(), EngineSection::WithoutPrompt);
        assert!(engine.requires_torch());
        assert!(
            !engine.allows_empty_mask(),
            "the mask says what to REMOVE: an empty one describes no work"
        );
        let layers = engine.mask_layers();
        assert_eq!(layers.len(), 1, "one mask travels per request");
        assert_eq!(layers[0].tint, LAMA_MASK_TINT);
        assert!(embedded_en_catalog_has(layers[0].label_key));
    }

    /// The frame constraints are a grid of 8 and nothing more: neither backend method caps
    /// the area or the aspect ratio, so neither may appear here.
    #[test]
    fn the_constraints_are_a_grid_of_8_with_no_area_or_aspect_cap() {
        let constraints = LamaEngine::new().constraints();
        assert_eq!(constraints.multiple, 8);
        assert_eq!(constraints.min_side, 8);
        assert!(constraints.max_area.is_none(), "neither method caps the region's area");
        assert!(constraints.max_aspect.is_none(), "nor its aspect ratio");
    }

    /// A malformed request is refused by `start` instead of being encoded onto the wire,
    /// and a refusal leaves the engine idle rather than half-started.
    #[test]
    fn start_refuses_a_request_whose_sizes_do_not_agree() {
        let rect = OverlayRectPx { x: 0, y: 0, w: 8, h: 8 };
        let mut engine = LamaEngine::new();
        let short_mask = EngineRunRequest {
            page_idx: 0,
            rect_px: rect,
            region: egui::ColorImage::new([8, 8], vec![egui::Color32::BLACK; 64]),
            masks: vec![vec![0u8; 8]],
        };
        assert!(engine.start(short_mask).is_err());
        assert!(engine.run_rx.is_none(), "a refused start must leave no run behind");

        let wrong_layer_count = EngineRunRequest {
            page_idx: 0,
            rect_px: rect,
            region: egui::ColorImage::new([8, 8], vec![egui::Color32::BLACK; 64]),
            masks: vec![vec![0u8; 64], vec![0u8; 64]],
        };
        assert!(engine.start(wrong_layer_count).is_err());

        let wrong_region = EngineRunRequest {
            page_idx: 0,
            rect_px: rect,
            region: egui::ColorImage::new([4, 4], vec![egui::Color32::BLACK; 16]),
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
    }

    /// The run gate closes only on a run already in flight; everything else the host owns.
    #[test]
    fn the_run_gate_reports_only_a_run_in_flight() {
        let mut engine = LamaEngine::new();
        assert!(engine.run_block_reason().is_none());
        let (_tx, rx) = mpsc::channel();
        engine.run_rx = Some(rx);
        assert!(engine.run_block_reason().is_some());
        engine.cancel();
        assert!(engine.run_block_reason().is_none(), "cancel releases the gate");
        assert!(engine.run_status.is_some(), "cancel says so in the status slot");
    }

    /// A region that breaks the DECLARED constraints is refused by `start`, not encoded onto
    /// the wire — the frame and the region it hands over can disagree.
    #[test]
    fn start_refuses_a_region_that_violates_the_declared_constraints() {
        for (w, h) in [(7usize, 8usize), (9, 16)] {
            let mut engine = LamaEngine::new();
            let pixels = w * h;
            let request = EngineRunRequest {
                page_idx: 0,
                rect_px: OverlayRectPx { x: 0, y: 0, w, h },
                region: egui::ColorImage::new([w, h], vec![egui::Color32::BLACK; pixels]),
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
        let mut engine = LamaEngine::new();
        // A load that has NOT landed yet, whose payload differs from what the user picks.
        let (tx, rx) = mpsc::channel();
        engine.settings_rx = Some(rx);
        engine.settings_loaded = false;
        engine.dirty = false;

        let from_file =
            LamaSettings { inpaint_size: LAMA_INPAINT_SIZE_MAX, ..LamaSettings::default() };

        // The user edits in the same frame, BEFORE the poll that drains the channel.
        engine.settings.inpaint_size = LAMA_INPAINT_SIZE_MIN;
        engine.note_settings_changed();

        let _send_result = tx.send(from_file);
        engine.poll_settings_load();

        assert_eq!(
            engine.settings.inpaint_size, LAMA_INPAINT_SIZE_MIN,
            "the load must not overwrite an edit the user already made"
        );
        assert!(engine.settings_loaded, "the load is complete either way: saving is unblocked");
        assert!(engine.dirty, "the edit is still pending a write");
        // The save GATE is asserted rather than driven: `poll_and_maybe_save` would write
        // the real settings file of whoever runs the test suite.
        assert!(
            settings_save_due(engine.dirty, engine.settings_loaded, engine.save_rx.is_some()),
            "the surviving edit is what gets written next"
        );
    }

    /// With nothing edited, the load applies as it always did.
    #[test]
    fn a_load_applies_when_the_user_has_changed_nothing() {
        let mut engine = LamaEngine::new();
        let (tx, rx) = mpsc::channel();
        engine.settings_rx = Some(rx);
        engine.settings_loaded = false;
        engine.dirty = false;

        let from_file =
            LamaSettings { inpaint_size: LAMA_INPAINT_SIZE_MAX, ..LamaSettings::default() };
        let _send_result = tx.send(from_file);
        engine.poll_settings_load();

        assert_eq!(engine.settings.inpaint_size, LAMA_INPAINT_SIZE_MAX);
        assert!(engine.settings_loaded);
    }

    /// Nothing is written before the initial load has landed, and a parameter change after
    /// it arms exactly one save.
    #[test]
    fn settings_are_never_saved_before_the_load_lands() {
        let mut engine = LamaEngine::new();
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
