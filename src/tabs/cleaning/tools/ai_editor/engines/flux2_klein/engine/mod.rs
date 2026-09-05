/*
File: cleaning/tools/ai_editor/engines/flux2_klein/engine/mod.rs

Purpose:
The FLUX.2 klein engine itself: the state it owns between frames, its `AiEngine` contract
with the host, and the polling that keeps that state current. This file is the root of
the `engine` module; the long-operation plumbing it starts and drains lives in
`actions.rs` beside it.

Main responsibilities:
- own `Flux2KleinEngine` — the settings and their save timer, the cached `.status`
  catalog, the forecast, the progress, the session and every worker channel;
- implement `AiEngine`: the sections the host draws, the mask layer it declares, the
  frame constraints it validates against, the run it starts and the poll it answers with;
- keep the derived state honest between frames: reload the settings once, save them when
  they settle, re-query the catalog when the paths change, re-arm the forecast when the
  region SIZE settles, and drop the undo history when the region moves;
- answer the questions the panel gates on — `pipeline_busy`, `model_readiness`,
  `status_for_current_paths`, `prompt_cache_state`, `download_check_current`;
- name the work that makes leaving this engine unsafe (`switch_block_reason`), which is
  what closes the host's engine picker while a model download is in flight.

Key structures:
- `Flux2KleinEngine`

Key functions:
- `AiEngine::start()`, `AiEngine::poll()`, `AiEngine::draw_section()`
- `same_region()`, `same_region_size()`

Submodules:
- `actions.rs`: the start/poll pairs of the long operations — component actions, the HF
  token, the download, the translator, the file pickers and the prompt-cache jobs.

Notes:
Nothing here blocks the GUI thread: every start hands its work to an `ms_thread` worker
and every poll only drains a channel. The frame rectangle, the painted mask and the
pending result belong to the HOST and reach this type only through `AiEngine`.
*/

use super::*;

// The start/poll pairs of every long operation this engine runs.
mod actions;

// ---------------------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------------------

/// The FLUX.2 klein engine of the «ИИ-редактор области» tool.
///
/// Everything here is engine-owned state; the frame rectangle, the painted mask and the
/// pending result belong to the host and reach this type only through `AiEngine`.
pub struct Flux2KleinEngine {
    /// Which checkpoint this instance is. It is fixed at construction and never changes:
    /// the two variants are separate engines in the picker, each with its own settings
    /// file, its own model directory and its own `.download.*` requests.
    pub(super) variant: Flux2Variant,
    pub(super) session: Flux2SessionState,
    /// The frame rectangle the forecast and the status line describe, `None` while the tool
    /// has no frame. Published by the host through [`AiEngine::set_region`].
    pub(super) region: Option<OverlayRectPx>,
    /// A region SIZE change is waiting for the frame's geometry to settle before it arms the
    /// forecast. It cannot be re-derived by comparing rectangles on the settling frame: the
    /// rectangle usually does not change on the frame the pointer is released, so the size
    /// change would be forgotten and the forecast would never be re-armed.
    pub(super) region_resize_pending: bool,
    /// User-facing line about the last run, shown in this engine's own panel under the
    /// progress bar. `None` while a run is in flight and before the first one.
    ///
    /// It carries only what NO OTHER SURFACE says: finished, recovered from an
    /// out-of-memory failure, failed, cancelled. "The run has started" is deliberately not
    /// among them — the progress bar directly above, the host's panel status line and the
    /// frame's chrome all report a running job already, and a fourth copy of that sentence
    /// only cost vertical space. A starting run therefore CLEARS this slot rather than
    /// writing to it, so the previous run's outcome cannot linger over a new one.
    pub(super) run_status: Option<String>,
    pub(super) settings: Flux2KleinSettings,
    pub(super) settings_rx: Option<Receiver<Flux2KleinSettings>>,
    pub(super) settings_loaded: bool,
    pub(super) dirty: bool,
    pub(super) save_rx: Option<Receiver<()>>,
    /// Component catalog; `None` until the first `.status` answer.
    pub(super) status: Option<Flux2Status>,
    pub(super) status_rx: Option<Receiver<Result<Flux2Status, String>>>,
    pub(super) status_error: Option<String>,
    /// Arms exactly ONE `.status` query, so a failing query cannot spawn a thread per
    /// frame. Re-armed whenever the PROMPT changes too: the answer's `prompt_cached`
    /// is about the prompt that travelled with the question.
    pub(super) status_wanted: bool,
    /// The trimmed prompt the query currently in flight was asked about, moved into
    /// `status_prompt` when its answer lands.
    pub(super) status_query_prompt: Option<String>,
    /// The trimmed prompt `status` describes. The cache line is shown only while it
    /// still equals the prompt in the field — otherwise the answer is about a prompt
    /// the user has already edited away from, and the honest state is "not known yet".
    pub(super) status_prompt: Option<String>,
    /// The effective paths the query currently in flight was asked about, moved into
    /// `status_paths` when its answer lands. The exact counterpart of
    /// `status_query_prompt`: the catalog is an answer about THREE PATHS as much as it is
    /// one about a prompt.
    pub(super) status_query_paths: Option<Flux2EffectivePaths>,
    /// The effective paths `status` describes. The presence half of the catalog counts
    /// only while they still equal [`Flux2KleinSettings::effective_paths`] — otherwise the
    /// answer is about files the user has already typed away from, and the honest verdict
    /// is `Unknown` ([`flux2_status_for_paths`]).
    pub(super) status_paths: Option<Flux2EffectivePaths>,
    /// The one prompt-cache operation (build / save / load / export / import) that may
    /// be in flight. All five share the channel because only one can run at a time.
    pub(super) prompt_cache_rx: Option<Receiver<Result<Flux2PromptCacheOutcome, String>>>,
    /// User-facing outcome of the last prompt-cache operation, shown under the buttons.
    pub(super) prompt_cache_status: Option<String>,
    /// A warning about the LAST prompt-cache operation, shown beside its status line:
    /// "the imported entry belongs to another encoder family", or "the encoder fingerprint
    /// of the loaded entry was not compared". Cleared when the next operation starts, so it
    /// is a remark about a result and never a standing banner.
    pub(super) prompt_cache_warning: Option<String>,
    /// The library listing of the current encoder family; `None` until `.list` answers.
    pub(super) prompt_cache_library: Option<Flux2PromptCacheList>,
    pub(super) prompt_cache_list_rx: Option<Receiver<Result<Flux2PromptCacheList, String>>>,
    pub(super) prompt_cache_list_error: Option<String>,
    /// Same one-shot arming as `status_wanted`; re-armed after a save or an import and
    /// whenever the encoder path changes, since the family — and with it the whole
    /// listing — follows that path.
    pub(super) prompt_cache_list_wanted: bool,
    /// Name of the library entry the combo points at. Kept as a NAME rather than an
    /// index so a refreshed listing cannot silently move the selection to another entry.
    pub(super) prompt_cache_selected: Option<String>,
    /// Buffer of the "save under this name" field.
    pub(super) prompt_cache_name_input: String,
    /// Entry an export dialog was opened for, captured when the dialog started so a
    /// selection changed while it was open cannot export the wrong entry.
    pub(super) prompt_cache_export_name: Option<String>,
    pub(super) estimate: Option<Flux2Estimate>,
    pub(super) estimate_rx: Option<Receiver<Result<Flux2Estimate, String>>>,
    pub(super) estimate_error: Option<String>,
    /// Same one-shot arming as `status_wanted`; re-armed whenever a parameter changed and,
    /// for the geometry, only by a SETTLED region SIZE change ([`AiEngine::set_region`]), so
    /// the forecast follows the controls without flooding the IPC from scrolling or dragging.
    pub(super) estimate_wanted: bool,
    pub(super) unload_rx: Option<Receiver<Result<(), String>>>,
    pub(super) unload_status: Option<String>,
    /// The per-component action (load / unload / move / warm up) that may be in flight.
    /// One channel and one operation, like the prompt cache: it holds the backend's one
    /// pipeline and claims the shared progress bar.
    pub(super) component_action_rx: Option<Receiver<Result<Flux2ComponentSnapshot, String>>>,
    /// What the action in flight is, kept so its outcome can name the component the user
    /// pressed and the failure log can carry the wire pair. Taken when the answer lands.
    pub(super) component_action_pending: Option<(Flux2ComponentId, Flux2ComponentAction)>,
    /// User-facing line about the last component action, shown under the residency block.
    pub(super) component_action_status: Option<String>,
    /// Buffer of the Hugging Face token field. NEVER persisted and never sent anywhere on
    /// its own: it is only what «Сохранить» hands to the OS secret store, and it is
    /// cleared as soon as the store accepts it, so the value does not linger on screen.
    pub(super) hf_token_input: String,
    /// A secret-store save or delete in flight. `Ok` carries the localized outcome line;
    /// neither branch ever carries the token.
    pub(super) hf_token_rx: Option<Receiver<Result<String, String>>>,
    /// User-facing line about the last token operation, shown beside the badge.
    pub(super) hf_token_status: Option<String>,
    /// The last `.download.check` answer; `None` until one lands.
    pub(super) download_check: Option<Flux2DownloadCheck>,
    pub(super) download_check_rx: Option<Receiver<Result<Flux2DownloadCheck, String>>>,
    pub(super) download_check_error: Option<String>,
    /// The streaming `.download.start` in flight. It claims the shared progress bar, which
    /// is why it counts towards [`flux2_pipeline_busy`].
    pub(super) download_rx: Option<Receiver<Result<Flux2DownloadOutcome, String>>>,
    /// User-facing line about the last download, shown under the download controls.
    pub(super) download_status: Option<String>,
    pub(super) translate_rx: Option<Receiver<Result<String, String>>>,
    pub(super) translate_status: Option<String>,
    pub(super) picker_rx: Option<Receiver<Option<PathBuf>>>,
    pub(super) picker: Option<Flux2PickerPurpose>,
    pub(super) progress: Arc<Mutex<Flux2Progress>>,
    pub(super) ai_backend_available: bool,
    /// Whether the prompt-cache LIBRARY is unfolded under the prompt block.
    ///
    /// SESSION state and deliberately not a persisted setting: the library is a workbench
    /// the user opens to save, load or carry a `.msprompt` and closes again, so remembering
    /// it across launches would re-expand an expert surface on every start. What the library
    /// EXPLAINS — whether the current prompt is cached — is drawn outside it either way,
    /// because that one line is what predicts a ~106 s encoder read.
    pub(super) prompt_library_open: bool,
    /// Whether the FIRST definite model verdict has already chosen the initial state of
    /// «Установка модели».
    ///
    /// Session state, and it exists because the verdict is not known on the frame the
    /// panel first draws: `.status` has not answered yet, so a `default_open` read from
    /// the verdict would always see `Unknown` and open the section on every launch,
    /// including on a machine where everything is installed. The section therefore starts
    /// folded and is opened ONCE, on the first `Missing` that lands; a `Ready` seeds it
    /// without opening anything. After that the user's own clicks are the only thing that
    /// moves it — a section forced open per frame cannot be closed.
    pub(super) install_section_seeded: bool,
    /// The open state «Установка модели» was LEFT IN by the previous drawn frame; `None`
    /// before the first one.
    ///
    /// Session state, and it exists to tell the user's clicks apart from this file's own
    /// `set_open`: a state that differs from what was recorded can only have been moved by
    /// the header (or Escape), and that retires the one-shot seeding. Without it a user who
    /// opens and closes the section while the verdict is still `Unknown` — the whole window
    /// a slow `.status` leaves open — gets it forced open again by the first `Missing`.
    pub(super) install_section_open_prev: Option<bool>,
}

/// The 9B engine, i.e. [`Flux2KleinEngine::new`] with [`Flux2Variant::Klein9B`].
///
/// It exists because that variant is the historic one and every `..Default::default()` in
/// this module's tests means it. The picker builds BOTH engines through `new` and never
/// through this, so a second variant can never be reached by forgetting to pass one.
impl Default for Flux2KleinEngine {
    fn default() -> Self {
        Self::new(Flux2Variant::Klein9B)
    }
}

impl Flux2KleinEngine {
    /// Builds the engine for one checkpoint and starts reading THAT variant's settings
    /// file on a worker thread.
    ///
    /// The variant is the only difference between the two instances the picker offers, and
    /// it is stamped into the in-memory settings straight away so that everything derived
    /// from them — the three model paths, the settings file a save lands in, the
    /// `.download.*` requests — describes this checkpoint even before the load returns.
    /// Visibility: the ENGINE CATALOG and nothing wider. `pub(super)` — the module's
    /// declared ceiling — would stop at `flux2_klein` and reject `all_engines`, which lives
    /// one level further out in `engines`; naming that module keeps the constructor as
    /// narrow as it can be while still reachable from the one place that builds an engine.
    #[must_use]
    pub(in crate::tabs::cleaning::tools::ai_editor::engines) fn new(variant: Flux2Variant) -> Self {
        let mut engine = Self::at_rest();
        engine.variant = variant;
        engine.settings.variant = variant.wire().to_string();
        engine.request_settings_load();
        engine
    }

    /// The engine's fields before anything is started: no channels, no cached answers,
    /// the 9B variant and default settings.
    ///
    /// Split out of [`Self::new`] so `Default` and `new` cannot drift, and so a test can
    /// build an engine without a settings-load worker behind it.
    fn at_rest() -> Self {
        Self {
            variant: Flux2Variant::Klein9B,
            session: Flux2SessionState::default(),
            region: None,
            region_resize_pending: false,
            run_status: None,
            settings: Flux2KleinSettings::default(),
            settings_rx: None,
            settings_loaded: false,
            dirty: false,
            save_rx: None,
            status: None,
            status_rx: None,
            status_error: None,
            status_wanted: true,
            status_query_prompt: None,
            status_prompt: None,
            status_query_paths: None,
            status_paths: None,
            prompt_cache_rx: None,
            prompt_cache_status: None,
            prompt_cache_warning: None,
            prompt_cache_library: None,
            prompt_cache_list_rx: None,
            prompt_cache_list_error: None,
            prompt_cache_list_wanted: true,
            prompt_cache_selected: None,
            prompt_cache_name_input: String::new(),
            prompt_cache_export_name: None,
            estimate: None,
            estimate_rx: None,
            estimate_error: None,
            estimate_wanted: false,
            unload_rx: None,
            unload_status: None,
            component_action_rx: None,
            component_action_pending: None,
            component_action_status: None,
            hf_token_input: String::new(),
            hf_token_rx: None,
            hf_token_status: None,
            download_check: None,
            download_check_rx: None,
            download_check_error: None,
            download_rx: None,
            download_status: None,
            translate_rx: None,
            translate_status: None,
            picker_rx: None,
            picker: None,
            progress: Arc::new(Mutex::new(Flux2Progress::default())),
            ai_backend_available: false,
            prompt_library_open: false,
            install_section_seeded: false,
            install_section_open_prev: None,
        }
    }

    /// Reads THIS variant's settings file on a worker thread (never on the GUI thread).
    pub(super) fn request_settings_load(&mut self) {
        let variant = self.variant;
        let (tx, rx) = mpsc::channel();
        self.settings_rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(load_flux2_settings(variant));
        });
    }

    /// Applies a finished settings load. A disconnected channel keeps the in-memory
    /// defaults and unblocks saving.
    pub(super) fn poll_settings_load(&mut self) {
        let Some(rx) = self.settings_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(settings) => {
                self.settings = settings;
                self.settings_loaded = true;
                self.settings_rx = None;
                self.estimate_wanted = true;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.settings_loaded = true;
                self.settings_rx = None;
            }
        }
    }

    /// Writes dirty settings on a worker thread, at most one save in flight, and never
    /// before the initial load finished (which would clobber the file).
    ///
    /// It is driven from [`AiEngine::poll`] and from nowhere else, which is why the host must
    /// poll the selected engine EVERY frame, panel visible or not: without that call nothing
    /// ever writes the settings file and every model path, memory preset and prompt is lost on
    /// exit, with no error anywhere.
    pub(super) fn poll_and_maybe_save(&mut self) {
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
        let settings = self.settings.clone();
        let (tx, rx) = mpsc::channel();
        self.save_rx = Some(rx);
        thread::spawn(move || {
            if let Err(err) = save_flux2_settings(&settings) {
                crate::runtime_log::log_warn(format!(
                    "[cleaning] failed to save FLUX.2 klein settings: {err}"
                ));
            }
            let _ = tx.send(());
        });
    }

    /// Polls the `.status` query and arms a new one when it is wanted, the backend is
    /// reachable and nothing else is running.
    pub(super) fn poll_and_maybe_query_status(&mut self) {
        if let Some(rx) = self.status_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(status)) => {
                    self.status = Some(status);
                    self.status_error = None;
                    self.status_rx = None;
                    // The answer describes the prompt AND the paths that travelled with the
                    // question, either of which may already be several keystrokes behind
                    // the fields.
                    self.status_prompt = self.status_query_prompt.take();
                    self.status_paths = self.status_query_paths.take();
                }
                Ok(Err(err)) => {
                    self.status_error = Some(err);
                    self.status_rx = None;
                    self.status_query_prompt = None;
                    self.status_query_paths = None;
                }
                Err(TryRecvError::Disconnected) => {
                    self.status_rx = None;
                    self.status_query_prompt = None;
                    self.status_query_paths = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.status_wanted
            && self.ai_backend_available
            && self.status_rx.is_none()
            // A prompt-cache build holds the backend for ~106 s and a component action for
            // as long; asking about the catalog meanwhile only queues a call behind it.
            // `.status` itself takes the lock a generation holds for its whole run, which
            // is also why the residency block reports `components_busy` instead of waiting.
            && !self.pipeline_busy()
        {
            self.status_wanted = false;
            // The catalog is a question ABOUT the paths in the settings, so they go
            // with the query; without them the backend answers about whatever it used
            // last, i.e. about nothing until a generation has succeeded. The PROMPT
            // travels for the same reason: `prompt_cached` is an answer about one
            // specific prompt, not about the tool in general. The mode goes as `false`:
            // there is no mask behind a catalog query, and the backend's own `.status`
            // probe forces the same value for the same reason.
            let normalized = self.settings.normalized();
            let params = normalized.to_params(false);
            self.status_query_prompt = Some(self.settings.prompt.trim().to_string());
            // Taken from the SAME value that built the params, so what is recorded is
            // literally what went on the wire and the guard below cannot compare the
            // answer against paths it was never asked about.
            self.status_query_paths = Some(normalized.effective_paths());
            let (tx, rx) = mpsc::channel();
            self.status_rx = Some(rx);
            thread::spawn(move || {
                let _ = tx.send(fetch_flux2_status(&params));
            });
        }
    }

    /// Whether the backend holds the embeddings of the prompt CURRENTLY in the field.
    ///
    /// `None` means "not known yet": no `.status` answer, an answer about a different
    /// prompt (the user has typed since), or a backend that does not report the field.
    /// A neutral line is shown for all three — reporting "not cached" for a question
    /// nobody has answered yet would be a guess, and the wrong one most of the time.
    pub(super) fn prompt_cache_state(&self) -> Option<bool> {
        prompt_cache_state_for(
            self.status.as_ref(),
            self.status_prompt.as_deref(),
            &self.settings.prompt,
        )
    }

    /// The `.status` catalog, but only while it still describes the paths a run would use.
    ///
    /// THE accessor every presence judgement goes through, exactly as
    /// [`Self::prompt_cache_state`] is the one for the cache half of the same answer.
    /// The raw `status` field stays available for what is genuinely about the machine
    /// rather than about the paths — the residency block, the device line, the totals.
    pub(super) fn status_for_current_paths(&self) -> Option<&Flux2Status> {
        flux2_status_for_paths(
            self.status.as_ref(),
            self.status_paths.as_ref(),
            &self.settings.effective_paths(),
        )
    }

    /// Whether the last `.download.check` answer still describes what the panel is asking
    /// about — the same checkpoint and the same encoder toggle.
    ///
    /// THE accessor the panel goes through, for the same reason
    /// [`Self::status_for_current_paths`] is: the comparison must use the EFFECTIVE toggle
    /// ([`Flux2KleinSettings::uncensored_encoder_active`]), because that is the value
    /// [`Self::start_download_check`] stamps into the answer. Comparing against the raw
    /// persisted flag instead rejects every answer forever on a 4B document that carries
    /// `uncensored_text_encoder: true` — a value that checkpoint cannot honour and whose
    /// control its panel does not draw, so the user could never clear it.
    pub(super) fn download_check_current(&self) -> bool {
        download_check_matches_selection(
            self.download_check.as_ref(),
            self.settings.uncensored_encoder_active(),
            self.variant,
        )
    }

    /// THE verdict on whether the model is installed, guarded against a stale catalog.
    ///
    /// Both consumers — the panel's readiness line and the run gate — read this rather
    /// than calling [`flux2_model_readiness`] with the raw `status`, so an answer about
    /// paths the user has already edited away from can never refuse a run.
    pub(super) fn model_readiness(&self) -> Flux2ModelReadiness {
        flux2_model_readiness(
            &self.settings,
            self.status_for_current_paths(),
            self.prompt_cache_state(),
        )
    }

    /// Records a parameter change and re-arms the background answers it invalidated.
    ///
    /// The memory forecast is re-armed as a whole, because any parameter can move it. The
    /// `.status` catalog is re-asked only when the change moved the EFFECTIVE PATHS: the
    /// catalog is an answer about exactly those three paths, so a change that leaves them
    /// alone cannot make it stale — and arming unconditionally would fire an IPC per frame
    /// of a dragged wheel. A typed or pasted path correction reaches the backend through
    /// here; only the folder picker arms the query on its own.
    pub(super) fn note_settings_changed(&mut self) {
        self.dirty = true;
        self.estimate_wanted = true;
        if flux2_status_paths_stale(
            &self.settings.effective_paths(),
            self.status_paths.as_ref(),
            self.status_query_paths.as_ref(),
        ) {
            self.status_wanted = true;
        }
    }

    /// Polls the `.estimate` query and sends a new one when something armed
    /// `estimate_wanted`. `region` is the frame rectangle's size; without a frame there is
    /// nothing to forecast and the arming is kept for the next frame that has one.
    pub(super) fn poll_and_maybe_query_estimate(&mut self, region: Option<[usize; 2]>) {
        if let Some(rx) = self.estimate_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(estimate)) => {
                    self.estimate = Some(estimate);
                    self.estimate_error = None;
                    self.estimate_rx = None;
                }
                Ok(Err(err)) => {
                    self.estimate_error = Some(err);
                    self.estimate_rx = None;
                }
                Err(TryRecvError::Disconnected) => self.estimate_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        let Some([width, height]) = region else {
            return;
        };
        if !self.estimate_wanted
            || !self.ai_backend_available
            || self.estimate_rx.is_some()
            || self.pipeline_busy()
        {
            return;
        }
        self.estimate_wanted = false;
        // The forecast is about memory, which the working mode does not move: the peak is
        // set by the weights and the region size, not by which pixels the mask permits.
        let params = self.settings.normalized().to_params(false);
        let (tx, rx) = mpsc::channel();
        self.estimate_rx = Some(rx);
        thread::spawn(move || {
            let _ = tx.send(fetch_flux2_estimate(&params, width, height));
        });
    }

    /// Drains the unload channel into the status line under the parameter section.
    pub(super) fn poll_unload(&mut self) {
        let Some(rx) = self.unload_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(())) => {
                self.unload_rx = None;
                self.unload_status =
                    Some(t!("cleaning.tools.flux2_klein.unload_done_status").to_string());
                // The pipeline is gone, so the resident/device fields of the catalog
                // are stale.
                self.status_wanted = true;
            }
            Ok(Err(err)) => {
                self.unload_rx = None;
                self.unload_status = Some(tf!("cleaning.inpaint.unload_error", err = err));
            }
            Err(TryRecvError::Disconnected) => self.unload_rx = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Whether one of the three long operations holds the backend's pipeline and the
    /// shared progress bar. See [`flux2_pipeline_busy`] for why all three count.
    pub(super) fn pipeline_busy(&self) -> bool {
        flux2_pipeline_busy(
            self.session.run_rx.is_some(),
            self.prompt_cache_rx.is_some(),
            self.component_action_rx.is_some(),
            self.download_rx.is_some(),
        )
    }

    /// The same rule with the GENERATION taken out: true while an operation OTHER than a
    /// run holds the pipeline.
    ///
    /// This is what the run gate reads. It cannot read [`Self::pipeline_busy`], which is
    /// true during a run and would make the run button explain itself away mid-run; and it
    /// must not read nothing, because a generation started under a `.prompt_cache.build`,
    /// a `.component_action` or a multi-hour model download would queue behind it and
    /// steal its progress bar.
    pub(super) fn non_run_pipeline_busy(&self) -> bool {
        flux2_pipeline_busy(
            false,
            self.prompt_cache_rx.is_some(),
            self.component_action_rx.is_some(),
            self.download_rx.is_some(),
        )
    }

    /// Size requirements this engine imposes on the frame rectangle, in source page pixels.
    ///
    /// Not a `const`: `max_area` is a `u64` and the limit is declared in `usize`, and a
    /// saturating widening is not available in a constant. Saturating rather than panicking
    /// keeps the comparison monotonic on a target where the two widths differ.
    pub(super) fn frame_constraints() -> FrameConstraints {
        FrameConstraints {
            multiple: FLUX2_SELECTION_MULTIPLE,
            min_side: FLUX2_MIN_SELECTION_PX,
            max_area: Some(u64::try_from(FLUX2_MAX_SELECTION_AREA_PX2).unwrap_or(u64::MAX)),
            max_aspect: Some(FLUX2_MAX_SELECTION_ASPECT),
        }
    }

    /// Region size the forecast and the status line describe, `None` without a frame.
    pub(super) fn region_size(&self) -> Option<[usize; 2]> {
        self.region.map(|rect| [rect.w, rect.h])
    }
}

/// Whether two frame rectangles describe the same region.
///
/// `OverlayRectPx` derives no `PartialEq`, and adding one to the canvas types for this
/// engine alone is not this module's call.
pub(super) fn same_region(a: Option<&OverlayRectPx>, b: Option<&OverlayRectPx>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.x == b.x && a.y == b.y && a.w == b.w && a.h == b.h,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

/// Whether two frame rectangles describe the same region SIZE.
///
/// Separate from [`same_region`] because the memory forecast is a question about the SIZE
/// alone: a rectangle that only moved forecasts identically, and asking the backend again
/// would be a round trip per scrolled pixel.
pub(super) fn same_region_size(a: Option<&OverlayRectPx>, b: Option<&OverlayRectPx>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.w == b.w && a.h == b.h,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

impl AiEngine for Flux2KleinEngine {
    fn id(&self) -> &'static str {
        self.variant.engine_id()
    }

    fn title(&self) -> String {
        self.variant.title().to_string()
    }

    fn section(&self) -> EngineSection {
        EngineSection::WithPrompt
    }

    fn requires_torch(&self) -> bool {
        true
    }

    fn constraints(&self) -> FrameConstraints {
        Self::frame_constraints()
    }

    /// ONE layer: the area the model is allowed to change. A second layer would have no
    /// meaning on the wire — the request carries exactly one mask.
    fn mask_layers(&self) -> Vec<MaskLayerSpec> {
        vec![MaskLayerSpec {
            tint: FLUX2_MASK_TINT,
            label_key: "cleaning.tools.flux2_klein.mask_heading",
        }]
    }

    /// Unconditionally `true`: an empty mask is not a missing input here, it is the
    /// whole-region MODE ([`mask_for_run`] turns it into a solid mask at run time). There
    /// is no parameter left that could make it `false`, so the host's per-frame re-read
    /// costs nothing and simply keeps agreeing with this.
    fn allows_empty_mask(&self) -> bool {
        true
    }

    fn draw_parameters(&mut self, ui: &mut egui::Ui) {
        let mut settings_changed = false;
        let mut want_status = false;
        let mut want_estimate = false;
        let mut unload_requested = false;
        let mut translate_requested = false;
        let mut prompt_cache_action: Option<Flux2PromptCacheAction> = None;
        let mut component_action: Option<(Flux2ComponentId, Flux2ComponentAction)> = None;
        let mut picker_requested: Option<Flux2PickerPurpose> = None;
        let mut hf_token_action: Option<Flux2HfTokenAction> = None;
        let mut download_action: Option<Flux2DownloadAction> = None;
        // Read before the destructure: it is a process-wide global, not a field the
        // context can borrow.
        let hf_token_state = crate::hf_token::hf_token_state();
        // Same reason: it compares a field the destructure hands out with one it does not.
        let download_check_current = self.download_check_current();
        // Copied out before the destructure borrows `self` apart: the panel needs it in
        // several blocks and it is `Copy`.
        let variant = self.variant;
        // Read before the destructure below borrows `settings` apart: the cache state is
        // derived from `status` AND `settings`, so it cannot be computed while both are
        // held by the context.
        let prompt_cache_state = self.prompt_cache_state();
        // Same reason, and the staleness guard it applies reads `status_paths`, which the
        // destructure below does not hand out: the panel gets the finished verdict.
        let readiness = self.model_readiness();
        // Same reason, and it reads three receivers the destructure hands out separately.
        let pipeline_busy = self.pipeline_busy();
        let region = self.region_size();
        {
            let Self {
                settings,
                status,
                status_error,
                estimate,
                estimate_error,
                unload_status,
                component_action_status,
                translate_status,
                translate_rx,
                estimate_rx,
                prompt_cache_rx,
                prompt_cache_status,
                prompt_cache_warning,
                prompt_cache_library,
                prompt_cache_list_rx,
                prompt_cache_list_error,
                prompt_cache_selected,
                prompt_cache_name_input,
                prompt_library_open,
                install_section_seeded,
                install_section_open_prev,
                hf_token_input,
                hf_token_rx,
                hf_token_status,
                download_check,
                download_check_rx,
                download_check_error,
                download_rx,
                download_status,
                progress,
                ai_backend_available,
                run_status,
                ..
            } = self;
            let mut panel = Flux2PanelCtx {
                variant,
                settings,
                status: status.as_ref(),
                status_error: status_error.as_deref(),
                estimate: estimate.as_ref(),
                estimate_error: estimate_error.as_deref(),
                unload_status,
                component_action_status: component_action_status.as_deref(),
                pipeline_busy,
                translate_status: translate_status.as_deref(),
                translate_busy: translate_rx.is_some(),
                estimate_busy: estimate_rx.is_some(),
                prompt_cache_state,
                prompt_cache_status: prompt_cache_status.as_deref(),
                prompt_cache_warning: prompt_cache_warning.as_deref(),
                prompt_cache_library: prompt_cache_library.as_ref(),
                prompt_cache_list_error: prompt_cache_list_error.as_deref(),
                prompt_cache_list_busy: prompt_cache_list_rx.is_some(),
                prompt_cache_busy: prompt_cache_rx.is_some(),
                prompt_cache_selected,
                prompt_cache_name_input,
                prompt_library_open,
                install_section_seeded,
                install_section_open_prev,
                readiness,
                hf_token_state,
                hf_token_input,
                hf_token_status: hf_token_status.as_deref(),
                hf_token_busy: hf_token_rx.is_some(),
                download_check: download_check.as_ref().filter(|_| download_check_current),
                download_check_error: download_check_error.as_deref(),
                download_check_busy: download_check_rx.is_some(),
                download_busy: download_rx.is_some(),
                download_status: download_status.as_deref(),
                progress,
                run_status: run_status.as_deref(),
                ai_backend_available: *ai_backend_available,
                settings_changed: &mut settings_changed,
                want_status: &mut want_status,
                want_estimate: &mut want_estimate,
                unload_requested: &mut unload_requested,
                translate_requested: &mut translate_requested,
                prompt_cache_action: &mut prompt_cache_action,
                component_action: &mut component_action,
                picker_requested: &mut picker_requested,
                hf_token_action: &mut hf_token_action,
                download_action: &mut download_action,
            };
            // Both variants draw the SAME body, so every widget id in it would be shared
            // between the two engine instances — the same fold, the same text cursor, the
            // same combo popup state. They are never drawn in the same frame, so this is
            // not an id CLASH, but it would carry one engine's UI state into the other's
            // panel. One salt over the whole body namespaces all of them at once
            // (`egui-docs/05-ids-and-i18n.md` §2).
            ui.push_id(variant.wire(), |ui| panel.draw(ui, region));
        }

        if settings_changed {
            self.note_settings_changed();
        }
        if want_status {
            self.status_wanted = true;
        }
        if want_estimate {
            self.estimate_wanted = true;
        }
        if translate_requested {
            self.start_translate();
        }
        match prompt_cache_action {
            Some(Flux2PromptCacheAction::Build) => self.start_prompt_cache_build(),
            Some(Flux2PromptCacheAction::Save) => self.start_prompt_cache_save(),
            Some(Flux2PromptCacheAction::Load) => self.start_prompt_cache_load(),
            // The dialog is opened here and the entry is captured with it, so a selection
            // changed while it is open cannot redirect the export to another entry.
            Some(Flux2PromptCacheAction::Export) => {
                self.prompt_cache_export_name = self.prompt_cache_selected.clone();
                self.start_picker(Flux2PickerPurpose::PromptCacheExport);
            }
            Some(Flux2PromptCacheAction::Import) => {
                self.start_picker(Flux2PickerPurpose::PromptCacheImport);
            }
            None => {}
        }
        if let Some((id, action)) = component_action {
            self.start_component_action(id, action);
        }
        match hf_token_action {
            Some(Flux2HfTokenAction::Save) => self.start_store_hf_token(),
            Some(Flux2HfTokenAction::Delete) => self.start_clear_hf_token(),
            None => {}
        }
        match download_action {
            Some(Flux2DownloadAction::Check) => self.start_download_check(),
            Some(Flux2DownloadAction::Start) => self.start_download(),
            Some(Flux2DownloadAction::Cancel) => self.cancel_download(),
            None => {}
        }
        if let Some(purpose) = picker_requested {
            self.start_picker(purpose);
        }
        if unload_requested && self.unload_rx.is_none() {
            let (tx, rx) = mpsc::channel();
            self.unload_rx = Some(rx);
            thread::spawn(move || {
                let _ = tx.send(unload_flux2_klein());
            });
            self.unload_status =
                Some(t!("cleaning.tools.flux2_klein.unload_requested_status").to_string());
        }
    }

    /// Only the engine's own half of the gate — the shared pipeline, the model paths and
    /// the prompt. The rectangle is already validated against [`Self::constraints`] by the
    /// frame, and the non-empty-mask rule is [`Self::allows_empty_mask`].
    ///
    /// The pipeline check comes first and reads [`Self::non_run_pipeline_busy`], not
    /// [`Self::pipeline_busy`]: a generation must not report itself as the reason it
    /// cannot start, while a `.prompt_cache.build`, a `.component_action` or a
    /// multi-hour model download genuinely blocks the next one — it holds the backend's
    /// one pipeline and the single progress bar a starting run would steal.
    fn run_block_reason(&self) -> Option<String> {
        if self.non_run_pipeline_busy() {
            return Some(t!("cleaning.tools.flux2_klein.pipeline_busy_error").to_string());
        }
        // The GUARDED catalog: an answer about paths the user has already corrected must
        // not refuse the run its correction just made possible.
        flux2_run_block_reason(&self.settings, self.status_for_current_paths(), self.prompt_cache_state())
    }

    /// Refuses the switch while THIS engine owns a model download or the access check that
    /// prices one.
    ///
    /// The download is the reason the hook exists: it is a multi-gigabyte transfer with its
    /// own free-space budget and no frame state at all, so without this a user could start
    /// the 9B download, switch, and start the 4B one against the same disk. The access
    /// check is included because it is the download's own pre-flight — leaving mid-check
    /// abandons an answer the panel would otherwise apply to the wrong checkpoint.
    ///
    /// A generation, a `.prompt_cache.build` and a `.component_action` are NOT here: the
    /// first locks the frame, which is the picker's other gate, and the other two are the
    /// backend's one pipeline, which the newly selected engine would queue behind rather
    /// than duplicate.
    fn switch_block_reason(&self) -> Option<String> {
        if self.download_rx.is_some() {
            return Some(t!("cleaning.tools.flux2_klein.download.switch_blocked_download").to_string());
        }
        if self.download_check_rx.is_some() {
            return Some(t!("cleaning.tools.flux2_klein.download.switch_blocked_check").to_string());
        }
        None
    }

    /// Validates the host's request and starts the run.
    ///
    /// The size guarantees of `EngineRunRequest` are CHECKED rather than trusted: a request
    /// whose region or mask does not match `rect_px` would otherwise be encoded onto the
    /// wire and rejected by the backend with a message about the protocol instead of about
    /// the region. The user-facing refusal names the region; the numbers go to the log.
    ///
    /// # Errors
    /// Returns a localized message when a run is already in flight, when the region or the
    /// masks do not match `rect_px`, or when the region size breaks a model constraint.
    fn start(&mut self, request: EngineRunRequest) -> Result<(), String> {
        let EngineRunRequest {
            page_idx,
            rect_px,
            region,
            masks,
        } = request;
        // Checked before anything is encoded: the backend has ONE pipeline, so a run
        // started under a prompt-cache build, a component action or a model download would
        // queue behind it for as long as it lasts AND steal its progress bar.
        if self.non_run_pipeline_busy() {
            return Err(t!("cleaning.tools.flux2_klein.pipeline_busy_error").to_string());
        }
        let size = [rect_px.w, rect_px.h];
        let expected_bytes = rect_px.w.saturating_mul(rect_px.h);
        let mask_ok = masks.len() == 1 && masks[0].len() == expected_bytes;
        if region.size != size || !mask_ok {
            crate::runtime_log::log_warn(format!(
                "[cleaning] FLUX.2 klein run request does not match the frame: page {page_idx}, rect {}x{} at ({}, {}), region {}x{}, {} mask layer(s) of {:?} bytes, expected {expected_bytes}",
                rect_px.w,
                rect_px.h,
                rect_px.x,
                rect_px.y,
                region.size[0],
                region.size[1],
                masks.len(),
                masks.iter().map(Vec::len).collect::<Vec<_>>()
            ));
            return Err(t!("cleaning.region.invalid_selection_size_error").to_string());
        }
        // Re-validated here as well as on the worker: the frame snaps to the same
        // constraints, but a host that hands over another size must be told which rule it
        // broke rather than have the backend refuse the blob.
        if let Some(reason) = region_block_reason(size) {
            return Err(reason);
        }
        // The mode is decided HERE, from the mask the host handed over, and nowhere else:
        // an empty layer becomes the solid buffer the whole-region mode requires, while a
        // painted one travels verbatim. The host's layer is never overwritten either way.
        let (mask, whole_region) = mask_for_run(&masks[0]);
        self.session.start_run(
            region,
            mask,
            whole_region,
            size,
            &self.settings,
            &self.progress,
        )?;
        // A run may have to load weights it did not have; the catalog and the forecast are
        // stale afterwards.
        self.status_wanted = true;
        self.estimate_wanted = true;
        // CLEARED, not set to «идёт обработка»: that sentence is already on screen three
        // times over — the progress bar this line sits under, the host's panel status line
        // and the frame's own chrome — and a fourth copy only pushed the panel down. What
        // the slot must not do is keep showing the PREVIOUS run's outcome while a new one
        // is in flight, which is what setting it to `None` here prevents.
        self.run_status = None;
        Ok(())
    }

    /// Drains every channel the engine owns and reports the run.
    ///
    /// Called whether the parameter panel is drawn or not, which is why all the polling
    /// lives here: a finished run must land even while the panel is closed.
    fn poll(&mut self, ctx: &egui::Context) -> EnginePoll {
        self.poll_settings_load();
        self.poll_and_maybe_query_status();
        self.poll_unload();
        self.poll_component_action();
        self.poll_translate();
        self.poll_prompt_cache();
        self.poll_and_maybe_query_prompt_cache_list();
        self.poll_hf_token();
        self.poll_download_check();
        self.poll_download();
        self.poll_picker();

        let mut settings_changed = false;
        let run = self.session.poll_run(
            &mut self.settings,
            &mut settings_changed,
            &mut self.run_status,
        );
        if settings_changed {
            self.note_settings_changed();
        }

        let region = self.region_size();
        self.poll_and_maybe_query_estimate(region);
        self.poll_and_maybe_save();

        match run {
            Flux2RunPoll::Idle => EnginePoll::Idle,
            Flux2RunPoll::Running => {
                // The progress bar moves on backend frames, not on user input, so the GUI
                // has to be woken even when nothing is touched.
                ctx.request_repaint();
                EnginePoll::Running
            }
            Flux2RunPoll::Done(image) => EnginePoll::Done(image),
            Flux2RunPoll::Failed(err) => EnginePoll::Failed(err),
        }
    }

    fn cancel(&mut self) {
        if self.session.cancel_run(&self.progress) {
            self.run_status =
                Some(t!("cleaning.mask_editor.processing_cancelled_status").to_string());
        }
    }

    fn set_backend_available(&mut self, available: bool) {
        self.ai_backend_available = available;
    }

    /// Deliberately empty: the run button is the host's `AiButton` with
    /// `AiRequirement::Torch`, which resolves the runtime's presence itself. Storing a
    /// second copy of that answer here could only ever disagree with it.
    fn set_torch_available(&mut self, _available: bool) {}

    /// Publishes the frame rectangle, and arms the memory forecast only on a SETTLED SIZE
    /// change.
    ///
    /// The rectangle itself is stored unconditionally, so the status line keeps printing the
    /// live size while the user drags. The `.estimate` round trip is what is gated, on two
    /// rules that are one bug each if dropped:
    /// - the forecast depends on the SIZE alone, so a pure position change arms nothing —
    ///   the host pushes this every frame and the frame's keep-in-view clamp moves the
    ///   rectangle whenever the canvas is scrolled, which used to re-arm the forecast
    ///   continuously;
    /// - a resize drag publishes a new size every rendered frame, so the change is held in
    ///   `region_resize_pending` until `geometry_settled` — one request when the handle is
    ///   released, no matter how many sizes the drag passed through.
    ///
    /// The cancel-and-clear half stays keyed to the rectangle's IDENTITY: a rectangle that
    /// moved describes another part of the page, so a run in flight would answer about the
    /// previous one and the pre-run images kept for chaining runs stop describing anything.
    /// In practice it fires only on a deliberate move — a frame holding a run or a result is
    /// locked and neither the user nor the keep-in-view clamp moves it — but the engine
    /// cannot see the frame's lock and must not depend on it.
    fn set_region(&mut self, region: Option<OverlayRectPx>, geometry_settled: bool) {
        if !same_region_size(self.region.as_ref(), region.as_ref()) {
            self.region_resize_pending = true;
        }
        if !same_region(self.region.as_ref(), region.as_ref()) {
            self.region = region;
            if self.session.cancel_run(&self.progress) {
                self.run_status =
                    Some(t!("cleaning.mask_editor.processing_cancelled_status").to_string());
            }
            self.session.clear();
        }
        if self.region_resize_pending && geometry_settled {
            self.region_resize_pending = false;
            self.estimate_wanted = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The undo entries describe the rectangle they were captured from, so a frame that
    /// moves or resizes invalidates them. `same_region` is what decides that, because
    /// `OverlayRectPx` derives no `PartialEq` to lean on.
    #[test]
    fn a_moved_frame_is_a_new_region_and_drops_the_undo_history() {
        let rect = |x, y, w, h| OverlayRectPx { x, y, w, h };
        assert!(same_region(None, None));
        assert!(same_region(Some(&rect(1, 2, 64, 64)), Some(&rect(1, 2, 64, 64))));
        assert!(
            !same_region(Some(&rect(1, 2, 64, 64)), Some(&rect(1, 3, 64, 64))),
            "a move is a new region even at the same size"
        );
        assert!(
            !same_region(Some(&rect(1, 2, 64, 64)), Some(&rect(1, 2, 64, 80))),
            "a resize is a new region"
        );
        assert!(!same_region(Some(&rect(1, 2, 64, 64)), None));

        let mut session = Flux2SessionState::default();
        session.push_undo(egui::ColorImage::filled([2, 2], Color32::WHITE));
        session.clear();
        assert!(session.undo_stack.is_empty());
    }

    /// A 4B settings document that carries `uncensored_text_encoder: true` — hand-edited, or
    /// copied from the 9B file, which is the shape of the historic bug — must still accept
    /// its OWN check answer.
    ///
    /// The flag is inert on that checkpoint and its panel draws no control to clear it, so
    /// comparing the answer against the RAW flag rejected every check forever: «Скачать»
    /// stayed dead with nothing on screen to say why. The comparison uses the value the
    /// request is stamped from, `uncensored_encoder_active()`.
    #[test]
    fn a_4b_document_carrying_the_raw_uncensored_flag_still_accepts_its_own_check() {
        let mut engine = Flux2KleinEngine {
            variant: Flux2Variant::Klein4B,
            ..Default::default()
        };
        engine.settings.variant = Flux2Variant::Klein4B.wire().to_string();
        engine.settings.uncensored_text_encoder = true;
        assert!(
            !engine.settings.uncensored_encoder_active(),
            "the 4B checkpoint has no uncensored encoder, so the flag is inert"
        );

        // Exactly what `start_download_check` stamps: the EFFECTIVE toggle and the echoed
        // checkpoint.
        engine.download_check = Some(Flux2DownloadCheck {
            uncensored: engine.settings.uncensored_encoder_active(),
            variant: Flux2Variant::Klein4B,
            ..Default::default()
        });
        assert!(
            engine.download_check_current(),
            "the panel must show the answer its own request asked for"
        );
        assert!(
            !download_check_matches_selection(
                engine.download_check.as_ref(),
                engine.settings.uncensored_text_encoder,
                engine.variant,
            ),
            "the raw flag is what used to reject it; this pins the difference"
        );
    }

    /// The 9B checkpoint keeps the toggle MEANINGFUL: there the flag and the effective value
    /// agree, so an answer about the other toggle is still correctly refused.
    #[test]
    fn on_9b_an_answer_about_the_other_toggle_is_still_refused() {
        let mut engine = Flux2KleinEngine::default();
        engine.settings.uncensored_text_encoder = true;
        assert!(engine.settings.uncensored_encoder_active());
        engine.download_check = Some(Flux2DownloadCheck {
            uncensored: false,
            variant: Flux2Variant::Klein9B,
            ..Default::default()
        });
        assert!(!engine.download_check_current());
    }

    /// Two engines with two independent free-space budgets must not download at once, so the
    /// engine closes the picker while it owns a transfer or the check that prices one.
    #[test]
    fn a_download_and_its_pre_flight_check_both_close_the_engine_picker() {
        let engine = Flux2KleinEngine::default();
        assert!(engine.switch_block_reason().is_none(), "an idle engine blocks nothing");

        let (_download_tx, download_rx) = mpsc::channel();
        let engine = Flux2KleinEngine { download_rx: Some(download_rx), ..Default::default() };
        assert_eq!(
            engine.switch_block_reason().as_deref(),
            Some(t!("cleaning.tools.flux2_klein.download.switch_blocked_download"))
        );

        let (_check_tx, check_rx) = mpsc::channel();
        let engine = Flux2KleinEngine { download_check_rx: Some(check_rx), ..Default::default() };
        assert_eq!(
            engine.switch_block_reason().as_deref(),
            Some(t!("cleaning.tools.flux2_klein.download.switch_blocked_check"))
        );

        // A generation is NOT one of them: the frame lock is the picker's gate for that, and
        // restating it here would close the picker twice for one reason.
        let (_run_tx, run_rx) = mpsc::channel();
        let mut engine = Flux2KleinEngine::default();
        engine.session.run_rx = Some(run_rx);
        assert!(engine.switch_block_reason().is_none());
    }

    /// The engine keeps a change pending rather than dropping it while a save cannot run yet,
    /// so nothing is lost between the parameter edit and the initial load landing.
    #[test]
    fn a_change_made_before_the_initial_load_is_kept_pending() {
        let mut engine = Flux2KleinEngine { settings_loaded: false, dirty: true, ..Default::default() };
        engine.poll_and_maybe_save();
        assert!(engine.dirty, "the change must survive until it can be written");
        assert!(engine.save_rx.is_none(), "and no writer may have been started");
    }

    /// The host pushes the rectangle EVERY frame, and the frame's keep-in-view clamp moves it
    /// whenever the canvas is scrolled — so a position change must cost nothing. It used to
    /// arm one `.estimate` round trip per scrolled frame.
    #[test]
    fn a_position_only_change_does_not_arm_the_forecast() {
        let mut engine = engine_with_settled_region();
        engine.set_region(region_rect(64, 32, 128, 128), true);
        assert!(!engine.estimate_wanted, "the forecast depends on the size alone");
        assert!(
            engine.region.is_some_and(|rect| rect.x == 64 && rect.y == 32),
            "the rectangle itself must still follow the frame, so the status line stays live"
        );
    }

    /// A resize drag publishes a new size on every rendered frame; querying the backend on
    /// each of them would be one IPC round trip per frame.
    #[test]
    fn a_size_change_while_the_geometry_is_unsettled_does_not_arm_the_forecast() {
        let mut engine = engine_with_settled_region();
        engine.set_region(region_rect(0, 0, 192, 128), false);
        assert!(!engine.estimate_wanted, "nothing may be queried mid-gesture");
        assert!(
            engine.region.is_some_and(|rect| rect.w == 192),
            "the dragged size must still be displayed"
        );
    }

    /// The rule the user asked for: one forecast per finished resize.
    #[test]
    fn a_settled_size_change_arms_the_forecast_exactly_once() {
        let mut engine = engine_with_settled_region();
        engine.set_region(region_rect(0, 0, 192, 128), true);
        assert!(engine.estimate_wanted, "a settled resize must re-arm the forecast");
        engine.estimate_wanted = false;
        // The host keeps pushing the same rectangle every frame afterwards.
        engine.set_region(region_rect(0, 0, 192, 128), true);
        assert!(!engine.estimate_wanted, "the arming must not repeat on the next frames");
    }

    /// The sizes a drag passes through are one change, not many: the pending flag survives
    /// the release frame, on which the rectangle usually no longer moves at all.
    #[test]
    fn the_sizes_of_one_drag_arm_the_forecast_once_on_release() {
        let mut engine = engine_with_settled_region();
        engine.set_region(region_rect(0, 0, 160, 128), false);
        engine.set_region(region_rect(0, 0, 192, 128), false);
        assert!(!engine.estimate_wanted, "still mid-drag");
        // The release frame carries the same rectangle as the last dragged one.
        engine.set_region(region_rect(0, 0, 192, 128), true);
        assert!(engine.estimate_wanted, "the finished resize must arm the forecast");
        engine.estimate_wanted = false;
        engine.set_region(region_rect(0, 0, 192, 128), true);
        assert!(!engine.estimate_wanted, "and exactly once");
    }

    /// The setter runs every frame with an unchanged rectangle for as long as the user does
    /// nothing at all.
    #[test]
    fn an_unchanged_rectangle_arms_nothing() {
        let mut engine = engine_with_settled_region();
        for _ in 0..3 {
            engine.set_region(region_rect(0, 0, 128, 128), true);
        }
        assert!(!engine.estimate_wanted, "an idle frame must be silent");
    }

    /// Regression guard for the fix above: gating the GEOMETRY must not disarm the forecast
    /// as a whole. Everything that is not the rectangle still arms it — here the settings
    /// load, which carries the memory preset, the placement and the model paths.
    #[test]
    fn a_settings_change_still_arms_the_forecast() {
        let (tx, rx) = mpsc::channel();
        assert!(tx.send(runnable_settings()).is_ok(), "the test channel must accept the settings");
        let mut engine = Flux2KleinEngine {
            settings_rx: Some(rx),
            estimate_wanted: false,
            ..Flux2KleinEngine::default()
        };
        engine.poll_settings_load();
        assert!(engine.settings_loaded, "the load must have landed");
        assert!(engine.estimate_wanted, "loaded settings describe another forecast");
    }

    /// The host promises that the region and every mask buffer are exactly `rect_px`.
    /// The engine CHECKS that promise: a mismatch would otherwise be encoded and refused
    /// by the backend with a message about the protocol instead of about the region.
    #[test]
    fn a_run_request_that_does_not_match_the_frame_is_refused() {
        let mut engine = Flux2KleinEngine {
            settings: runnable_settings(),
            ..Flux2KleinEngine::default()
        };
        let rect = OverlayRectPx {
            x: 0,
            y: 0,
            w: 128,
            h: 128,
        };
        let region = || egui::ColorImage::filled([128, 128], Color32::WHITE);
        let mask = || vec![255u8; 128 * 128];
        let request = |rect_px, region, masks| EngineRunRequest {
            page_idx: 3,
            rect_px,
            region,
            masks,
        };

        assert!(
            engine
                .start(request(
                    rect,
                    egui::ColorImage::filled([64, 128], Color32::WHITE),
                    vec![mask()]
                ))
                .is_err(),
            "a region of another size"
        );
        assert!(
            engine.start(request(rect, region(), Vec::new())).is_err(),
            "no mask layer at all"
        );
        assert!(
            engine
                .start(request(rect, region(), vec![mask(), mask()]))
                .is_err(),
            "more layers than the one this engine declares"
        );
        assert!(
            engine
                .start(request(rect, region(), vec![vec![255u8; 128 * 64]]))
                .is_err(),
            "a mask of the wrong length"
        );
        // A rectangle that breaks a model constraint is refused too, even when the region
        // and the mask agree with it: `region_block_reason` is the wire's last guard.
        let steep = OverlayRectPx {
            x: 0,
            y: 0,
            w: 120,
            h: 128,
        };
        assert!(
            engine
                .start(request(
                    steep,
                    egui::ColorImage::filled([120, 128], Color32::WHITE),
                    vec![vec![255u8; 120 * 128]]
                ))
                .is_err(),
            "120 is not a multiple of 16"
        );
    }

    /// The empty-mask rule LEFT the engine's gate in the port: the host's frame enforces
    /// it and asks the engine only whether an empty mask is meaningful at all. It always
    /// is here, and no setting may make it otherwise. Everything else the gate used to
    /// refuse is refused exactly as before.
    #[test]
    fn an_empty_mask_is_always_a_legal_run_for_this_engine() {
        let mut engine = Flux2KleinEngine {
            settings: runnable_settings(),
            ..Flux2KleinEngine::default()
        };
        assert!(
            engine.allows_empty_mask(),
            "nothing painted is the whole-region mode, not a missing input"
        );
        assert!(
            engine.run_block_reason().is_none(),
            "with paths and a prompt the engine's own half of the gate is clear"
        );
        // Every parameter the panel offers, at a value away from its default: none of
        // them may turn the empty-mask answer back into a refusal.
        engine.settings.mask_dilate_px = 0;
        engine.settings.mask_feather_px = 0;
        engine.settings.color_match = false;
        MemoryPreset::MinRam.apply(&mut engine.settings);
        assert!(engine.allows_empty_mask(), "no parameter may gate the empty-mask rule");

        // Every other gate still applies.
        engine.settings.prompt = "   ".to_string();
        assert!(engine.run_block_reason().is_some(), "a blank prompt still blocks");
        engine.settings = Flux2KleinSettings {
            vae_path: String::new(),
            ..runnable_settings()
        };
        assert!(engine.run_block_reason().is_some(), "a missing path still blocks");
        assert!(engine.allows_empty_mask(), "a blocked run is still a maskless one");
    }

    /// The other half of the same fix: the guard makes a stale answer harmless, and this
    /// makes a fresh one actually arrive. Only a change of the EFFECTIVE PATHS re-asks —
    /// the catalog says nothing about the other parameters, and arming on every edit would
    /// put an IPC on every frame of a dragged wheel.
    #[test]
    fn a_path_change_arms_a_fresh_catalog_query() {
        let asked_about = runnable_settings().effective_paths();
        let engine_at_rest = || Flux2KleinEngine {
            settings: runnable_settings(),
            settings_loaded: true,
            status: Some(status_with_present(&FLUX2_ALL_COMPONENTS)),
            status_paths: Some(asked_about.clone()),
            status_wanted: false,
            estimate_wanted: false,
            dirty: false,
            ..Flux2KleinEngine::default()
        };

        // A typed path correction: the answer in hand is about the old file.
        let mut typed = engine_at_rest();
        typed.settings.transformer_path = "/models/flux2-corrected.safetensors".to_string();
        typed.note_settings_changed();
        assert!(typed.dirty && typed.estimate_wanted);
        assert!(
            typed.status_wanted,
            "a typed path must re-ask the catalog — only the picker armed it before"
        );

        // A parameter the catalog knows nothing about: the forecast moves, the catalog
        // does not.
        let mut steps = engine_at_rest();
        steps.settings.steps += 1;
        steps.note_settings_changed();
        assert!(steps.dirty && steps.estimate_wanted);
        assert!(
            !steps.status_wanted,
            "a parameter that leaves the paths alone must not re-ask the catalog"
        );

        // A query for exactly these paths is already on the wire: arming again would only
        // send a duplicate behind it.
        let mut in_flight = engine_at_rest();
        in_flight.status_paths = None;
        in_flight.settings.transformer_path = "/models/flux2-corrected.safetensors".to_string();
        in_flight.status_query_paths = Some(in_flight.settings.effective_paths());
        in_flight.note_settings_changed();
        assert!(!in_flight.status_wanted);

        // The switch to download mode changes all three derived paths at once.
        let mut mode = engine_at_rest();
        mode.settings.source_mode = Flux2SourceMode::Download.wire().to_string();
        mode.note_settings_changed();
        assert!(mode.status_wanted);
    }

    #[test]
    fn a_generation_closes_the_prompt_cache_controls_just_as_a_cache_job_does() {
        // The gate has always documented that `busy` covers a generation too; the panel
        // used to pass only the prompt-cache receiver, which left «Кэшировать» clickable
        // in the middle of a run — and it would have stolen the run's progress bar.
        let mut engine = Flux2KleinEngine::default();
        assert!(!engine.pipeline_busy());
        let open = flux2_prompt_cache_gates(
            &cacheable_settings(),
            Some(true),
            Some(true),
            "entry",
            true,
            true,
            engine.pipeline_busy(),
        );
        assert!(open.build && open.save && open.load && open.export && open.import);

        // Each of the three long operations closes them, and each on its own.
        let (_run_tx, run_rx) = mpsc::channel();
        engine.session.run_rx = Some(run_rx);
        assert!(engine.pipeline_busy(), "a generation holds the pipeline");
        let during_run = flux2_prompt_cache_gates(
            &cacheable_settings(),
            Some(true),
            Some(true),
            "entry",
            true,
            true,
            engine.pipeline_busy(),
        );
        assert_eq!(
            during_run,
            Flux2PromptCacheGates {
                build: false,
                save: false,
                load: false,
                export: false,
                import: false
            }
        );
        engine.session.run_rx = None;

        let (_action_tx, action_rx) = mpsc::channel();
        engine.component_action_rx = Some(action_rx);
        assert!(
            engine.pipeline_busy(),
            "a component action holds it for as long as a cache build does"
        );
        engine.component_action_rx = None;

        let (_cache_tx, cache_rx) = mpsc::channel();
        engine.prompt_cache_rx = Some(cache_rx);
        assert!(engine.pipeline_busy());
    }

    /// The download claims the shared bar, so it must count towards the same busy rule the
    /// other three long operations do — and it must ALSO block a generation, which is what
    /// `non_run_pipeline_busy` exists for.
    #[test]
    fn a_download_holds_the_pipeline_exactly_as_the_other_long_operations_do() {
        // The pure rule first, one flag at a time.
        assert!(!flux2_pipeline_busy(false, false, false, false));
        assert!(flux2_pipeline_busy(false, false, false, true));

        let mut engine = Flux2KleinEngine {
            settings: runnable_settings(),
            settings_loaded: true,
            ..Flux2KleinEngine::default()
        };
        assert!(!engine.pipeline_busy());
        assert!(!engine.non_run_pipeline_busy());
        assert!(
            engine.run_block_reason().is_none(),
            "a configured engine with nothing running must be runnable"
        );

        let (_download_tx, download_rx) = mpsc::channel();
        engine.download_rx = Some(download_rx);
        assert!(engine.pipeline_busy(), "a download holds the pipeline");
        assert!(
            engine.non_run_pipeline_busy(),
            "and it is not a generation, so it blocks one"
        );
        assert_eq!(
            engine.run_block_reason(),
            Some(t!("cleaning.tools.flux2_klein.pipeline_busy_error").to_string()),
            "«Обработать» must say the pipeline is busy, not that a path is missing"
        );
        // The prompt-cache controls close on the same rule, on a real receiver.
        let gates = flux2_prompt_cache_gates(
            &cacheable_settings(),
            Some(true),
            Some(true),
            "entry",
            true,
            true,
            engine.pipeline_busy(),
        );
        assert_eq!(
            gates,
            Flux2PromptCacheGates {
                build: false,
                save: false,
                load: false,
                export: false,
                import: false
            }
        );
        engine.download_rx = None;

        // A GENERATION is the one holder that must not report itself as the reason it
        // cannot start: the frame owns that state.
        let (_run_tx, run_rx) = mpsc::channel();
        engine.session.run_rx = Some(run_rx);
        assert!(engine.pipeline_busy());
        assert!(!engine.non_run_pipeline_busy());
        assert!(engine.run_block_reason().is_none());
    }

}
