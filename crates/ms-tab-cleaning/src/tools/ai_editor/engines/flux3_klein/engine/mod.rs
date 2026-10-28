/*
File: cleaning/tools/ai_editor/engines/flux3_klein/engine/mod.rs

Purpose:
The FLUX.3 klein engine itself: the state it keeps between frames, its `AiEngine` contract with
the host, and the polling that keeps that state current. The start/poll pairs of its long
operations live in `actions.rs` beside it, the Stable-Diffusion.cpp run and files check in
`sdcpp.rs`.

Main responsibilities:
- own `Flux3KleinEngine` — the settings and their saver, the seed field, the `.status` answer,
  the forecast (`ResourcesController`), the extra references, the token field, the catalog
  download controller (`downloads`), the prompt library (the shared `PromptCacheController`),
  the run channel and the progress bar;
- implement `AiEngine`: one mask layer, the FLUX.3 frame constraints, marks as a separate
  reference while the reference list has room (else composited onto the region), the run it
  starts ON THE SELECTED RUNTIME (PyTorch backend or Stable-Diffusion.cpp), mid-run cancel, the
  poll it answers with (`EnginePoll::DoneRaw`: the raw region, its mask and mode, either runtime)
  and the live «Постобработка» settings the host finishes it with (`postprocess_settings`); the
  PyTorch requirement follows the runtime (`requires_torch`, the engine's own
  `torch_missing_hint`), the picker gate covers both runtimes (`ai_requirement`), and selecting
  the engine without PyTorch moves it to sd.cpp (`on_selected`, `switch_runtime`);
- close the runtime switch while any job of either runtime is in flight
  (`runtime_switch_block_reason`);
- validate a run request and turn it into the run's pixels (`run_input`);
- keep the derived state honest: load the settings once, save them when they settle, re-ask
  `.status` when the package root or the cache question (prompt, encoder mode) changes, re-ask
  the library listing when the encoder or the root changes, re-arm the forecast on a settled
  region SIZE — the backend questions only on PyTorch; on sd.cpp the files worker instead;
- answer whether the typed prompt is served from the cache (`prompt_cache_state`, guarded to
  the prompt `.status` was asked about), which waives the text encoder in TEXT mode
  (`model_readiness`);
- drive the catalog download every frame (`poll_catalog_bundle`: probes, the automatic
  adoption, the end of a job re-asks `.status` and the forecast) and apply the block's click
  (`apply_catalog_action`).

Key structures:
- `Flux3KleinEngine`

Key functions:
- `AiEngine::{start, poll, cancel, draw_parameters, marks_support, postprocess_settings,
  switch_block_reason, requires_torch, ai_requirement, on_selected, run_block_reason}`,
  `Flux3KleinEngine::{run_input, frame_constraints, model_readiness, prompt_cache_state,
  prompt_cache_status_entry, profiles_request, catalog_snapshot, poll_catalog_bundle,
  apply_catalog_action, fold_panel_frame, runtime_switch_block_reason}`

Notes:
Nothing here blocks the GUI thread: every start hands its work to an `ms_thread` worker and every
poll only drains a channel. The frame, the painted mask and the pending result are the HOST's.
*/

use super::*;

// The start/poll pairs of the long operations: token, pickers, references, unload.
mod actions;
// The Stable-Diffusion.cpp run (prep worker + shared job) and the files worker.
mod sdcpp;
use sdcpp::*;

/// The file dialog a running picker serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux3PickerPurpose {
    /// The package folder of manual mode (a directory path used in place).
    ModelRoot,
    /// One or more extra reference images.
    References,
    /// Destination of `inpaint.flux3_klein.prompt_cache.export` (a save target).
    PromptCacheExport,
    /// Source of `inpaint.flux3_klein.prompt_cache.import`.
    PromptCacheImport,
}

/// The FLUX.3 klein engine of the «ИИ-редактор области» tool.
pub struct Flux3KleinEngine {
    /// The frame rectangle, `None` while the tool has no frame (`AiEngine::set_region`).
    pub(super) region: Option<OverlayRectPx>,
    /// A region SIZE change waits for the geometry to settle before it arms the forecast.
    pub(super) region_resize_pending: bool,
    /// The line about the last run (finished with its seed, failed, cancelled); cleared by a
    /// starting run.
    pub(super) run_status: Option<String>,
    /// The seed the last finished run used, offered as «Повторить этот сид».
    pub(super) last_seed: Option<u64>,
    /// The PyTorch run in flight.
    pub(super) run_rx: Option<Receiver<Result<Flux3RunOutcome, String>>>,
    /// The Stable-Diffusion.cpp run in flight (its request prep, then the shared job).
    pub(super) sdcpp_run: Option<Flux3SdcppRun>,
    /// The last answer of the sd.cpp files worker (counts only for the selection it names) and
    /// the saved sd.cpp device read with it.
    pub(super) sdcpp_files: Option<Flux3SdcppFilesAnswer>,
    pub(super) sdcpp_files_rx: Option<Receiver<Flux3SdcppFilesAnswer>>,
    /// Re-check the sd.cpp files even when the answer names the current selection («Обновить»,
    /// the end of a catalog job: the disk may have changed).
    pub(super) sdcpp_files_wanted: bool,
    /// The `ms_sdcpp` status revision the last files question was asked at (`None` = never):
    /// a refresh re-reads the saved device (and the files) with it.
    pub(super) sdcpp_status_revision: Option<u64>,
    /// Whether the PyTorch runtime is present, as the host last pushed it.
    pub(super) torch_available: bool,
    /// Cancelled runs whose workers have not ended; while one is running nothing new starts
    /// (`pipeline_busy`), so quick start/cancel cycles cannot stack backend requests.
    pub(super) abandoned: AbandonedJobs,
    pub(super) settings: Flux3KleinSettings,
    pub(super) settings_rx: Option<Receiver<Flux3SettingsLoad>>,
    pub(super) settings_problem: Option<SettingsProblem>,
    pub(super) settings_save_blocked: bool,
    pub(super) settings_loaded: bool,
    /// A selection event (`AiEngine::on_selected`) arrived while the settings load was in
    /// flight: decided right after the load is applied (`poll_settings_load_with`), on the
    /// FILE's runtime.
    pub(super) selection_pending: bool,
    /// Raised by every parameter change; also what makes an edit outrank a late settings load.
    pub(super) dirty: bool,
    pub(super) saver: SettingsSaver<Flux3KleinSettings>,
    /// The seed field's text: blank = random. Kept beside the setting because an INVALID text
    /// must stay on screen (and block the run) without being persisted.
    pub(super) seed_input: String,
    /// The last `.status` answer and the package root it describes.
    pub(super) status: Option<Flux3Status>,
    pub(super) status_root: Option<String>,
    pub(super) status_rx: Option<Receiver<Result<Flux3Status, String>>>,
    /// The root the query in flight asks about, moved into `status_root` when it lands.
    pub(super) status_query_root: Option<String>,
    pub(super) status_error: Option<String>,
    /// Arms exactly ONE `.status` query.
    pub(super) status_wanted: bool,
    /// The cache question (trimmed prompt, TEXT mode in effect) the last `.status` query
    /// carried, recorded when it was asked; `None` before the first and after a failed one. A
    /// settings change that moves it re-asks `.status` (`note_settings_changed`): `prompt_cached`
    /// is an answer about one prompt in one encoder mode.
    pub(super) status_cache_question: Option<(String, bool)>,
    /// The forecast, the caps and the learned-floor reset.
    pub(super) resources: ResourcesController,
    /// The model's prompt LIBRARY between frames (the shared controller): the one operation in
    /// flight (build / rename / export / import), the `.list` query with its one-shot arming
    /// (re-armed after a build or an import, after a catalog job and whenever the encoder or the
    /// package root changes), the preset-name field, the export file captured when its dialog
    /// opened, the outcome lines and the session-only fold flag.
    pub(super) prompt_cache: PromptCacheController,
    /// The session's extra reference images.
    pub(super) references: Flux3References,
    /// Whether the next run would carry a marks reference, as the host last said
    /// (`RunOptionsCtx::marks_reference`, pushed through `draw_run_options`). The forecast
    /// counts it as one more region-sized reference; a value gone stale while the panel was not
    /// drawn costs one forecast, never a wrong run, and `start` corrects it from the request.
    pub(super) marks_reference: bool,
    /// The worker reading picked reference files.
    pub(super) references_rx: Option<Receiver<(Vec<Flux3ReferenceLoad>, usize)>>,
    /// Why the last picked files were not all added (one line each).
    pub(super) references_errors: Vec<String>,
    pub(super) unload_rx: Option<Receiver<Result<bool, String>>>,
    pub(super) unload_status: Option<String>,
    /// The Hugging Face token field and its secret-store worker (never persisted here); the
    /// catalog download reads the stored token itself.
    pub(super) hf_token: HfTokenControl,
    /// The explicit catalog download of the selection's bundle: status probes, the download /
    /// verify job (with the gated-repository access check) and the automatic adoption of an
    /// existing unmarked tree, all on its own workers; polled every frame by
    /// [`Self::poll_catalog_bundle`].
    ///
    /// `Some` for the engine the product builds ([`Self::new`]); `None` in a TEST build, where a
    /// real controller could adopt — hash and mark — the developer's real model files.
    pub(super) downloads: Option<OcrModelDownloadController>,
    /// Whether a catalog job (download, verify or adoption, of any bundle) was running at the
    /// last poll; its falling edge is when files on disk may have changed.
    pub(super) catalog_job_running: bool,
    /// The open file dialog and what it was opened for; at most one.
    pub(super) picker: Option<(Flux3PickerPurpose, ms_platform::file_dialog::PendingDialog)>,
    pub(super) progress: Arc<Mutex<EngineProgress>>,
    pub(super) ai_backend_available: bool,
    /// Whether the first definite non-ready verdict has opened «Установка модели» already.
    pub(super) install_section_seeded: bool,
    /// The fold state «Установка модели» was left in last frame (tells the user's clicks
    /// apart from the seeding).
    pub(super) install_section_open_prev: Option<bool>,
}

impl Flux3KleinEngine {
    /// Builds the engine and starts reading its settings file on a worker thread.
    ///
    /// Visibility: the engine catalog (`all_engines`), one level above this module.
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn new() -> Self {
        let mut engine = Self::at_rest();
        // A test build gets no controller: its automatic adoption would hash and mark the
        // developer's real model files.
        if !cfg!(test) {
            engine.downloads = Some(OcrModelDownloadController::new());
        }
        engine.request_settings_load();
        engine
    }

    /// The fields before anything is started: no channels, defaults. Split out so a test can
    /// build an engine without a settings-load worker behind it.
    pub(super) fn at_rest() -> Self {
        Self {
            region: None,
            region_resize_pending: false,
            run_status: None,
            last_seed: None,
            run_rx: None,
            sdcpp_run: None,
            sdcpp_files: None,
            sdcpp_files_rx: None,
            sdcpp_files_wanted: false,
            sdcpp_status_revision: None,
            torch_available: false,
            abandoned: AbandonedJobs::default(),
            settings: Flux3KleinSettings::default(),
            settings_rx: None,
            settings_problem: None,
            settings_save_blocked: false,
            settings_loaded: false,
            selection_pending: false,
            dirty: false,
            saver: SettingsSaver::new(save_flux3_settings, FLUX3_LOG_LABEL),
            seed_input: String::new(),
            status: None,
            status_root: None,
            status_rx: None,
            status_query_root: None,
            status_error: None,
            status_wanted: true,
            status_cache_question: None,
            resources: ResourcesController::new(FLUX3_LARGE_MODEL, FLUX3_LOG_LABEL),
            prompt_cache: PromptCacheController::default(),
            references: Flux3References::default(),
            marks_reference: false,
            references_rx: None,
            references_errors: Vec::new(),
            unload_rx: None,
            unload_status: None,
            hf_token: HfTokenControl::default(),
            downloads: None,
            catalog_job_running: false,
            picker: None,
            progress: Arc::new(Mutex::new(EngineProgress::default())),
            ai_backend_available: false,
            install_section_seeded: false,
            install_section_open_prev: None,
        }
    }

    /// Reads the settings file on a worker thread.
    fn request_settings_load(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.settings_rx = Some(rx);
        thread::spawn(move || {
            // A failed send means the engine was dropped meanwhile: nobody waits.
            let _ = tx.send(load_flux3_settings());
        });
    }

    /// Applies a finished settings load — unless the user already edited something, in which
    /// case the edit wins and the file's document is discarded whole (FLUX.2 klein's rule). A
    /// disconnected loader unblocks saving with the in-memory settings. A selection event
    /// deferred while the load was in flight (`selection_pending`) is decided right after it.
    pub(super) fn poll_settings_load(&mut self) {
        self.poll_settings_load_with(AiCaps::current);
    }

    /// [`Self::poll_settings_load`] with the capability source of a deferred selection decision
    /// injected (tests pass a fixed snapshot; production reads `AiCaps::current`).
    pub(super) fn poll_settings_load_with(&mut self, caps: impl FnOnce() -> AiCaps) {
        let Some(rx) = self.settings_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(load) => {
                self.settings_save_blocked = load.save_blocked();
                self.settings_problem = load.problem.clone();
                if !self.dirty {
                    self.settings = load.settings;
                    self.seed_input = self.settings.seed.map(|seed| seed.to_string()).unwrap_or_default();
                    self.resources.arm();
                    self.status_wanted = true;
                }
                self.settings_loaded = true;
                self.settings_rx = None;
            }
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.settings_loaded = true;
                self.settings_rx = None;
                // The file was never read: the in-memory settings are DEFAULTS, and a switch
                // decided on them would be saved over that file. The deferred event is dropped;
                // the next selection decides on what the session then holds.
                self.selection_pending = false;
            }
        }
        if std::mem::take(&mut self.selection_pending) {
            self.decide_runtime_on_selection(&caps());
        }
    }

    /// The selection-time runtime decision on LOADED settings: leaves an out-of-reach PyTorch
    /// for sd.cpp (the shared `sdcpp_auto_switch` rule) through the path a click on the switch
    /// takes, and logs it.
    fn decide_runtime_on_selection(&mut self, caps: &AiCaps) {
        if sdcpp_auto_switch(self.settings.runtime(), caps, sdcpp_route_available(), self.runtime_switch_block_reason().is_some()) {
            log_sdcpp_auto_switch(self.id());
            self.switch_runtime(ImageEditRuntime::SdCpp);
        }
    }

    /// Writes dirty settings on a worker (the shared `SettingsSaver`): never before the load,
    /// never while the file is blocked, only once a change stayed unchanged for the shared
    /// `SETTINGS_SAVE_DEBOUNCE`, a failure retried with backoff; a value still owed when the
    /// engine is dropped is flushed by the saver on a worker. `now` is the poll's clock.
    pub(super) fn poll_and_maybe_save(&mut self, now: Instant) {
        self.saver.poll(
            now,
            SaveGate { dirty: &mut self.dirty, loaded: self.settings_loaded, blocked: self.settings_save_blocked },
            &self.settings,
            |err| tf!("cleaning.tools.flux3_klein.settings_save_error", err = err),
        );
    }

    /// Drains the `.status` query and asks a new one when it is wanted, PyTorch is the runtime
    /// (sd.cpp asks the backend nothing), the backend is up,
    /// nothing holds it and an ABSOLUTE package root exists (`has_usable_root`; otherwise the
    /// verdict is `NoRoot` / `RootNotAbsolute`, decided locally, and the GGUF candidate paths
    /// would not be absolute). The answer is recorded
    /// with the root it was asked about.
    pub(super) fn poll_and_maybe_query_status(&mut self) {
        if let Some(rx) = self.status_rx.as_ref() {
            match rx.try_recv() {
                Ok(Ok(status)) => {
                    self.status = Some(status);
                    self.status_error = None;
                    self.status_root = self.status_query_root.take();
                    self.status_rx = None;
                }
                Ok(Err(err)) => {
                    self.status_error = Some(err);
                    self.status_query_root = None;
                    self.status_cache_question = None;
                    self.status_rx = None;
                }
                Err(TryRecvError::Disconnected) => {
                    self.status_query_root = None;
                    self.status_cache_question = None;
                    self.status_rx = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        let pytorch = self.settings.runtime() == ImageEditRuntime::PyTorch;
        if pytorch && self.status_wanted && self.ai_backend_available && self.status_rx.is_none() && !self.pipeline_busy() && self.settings.has_usable_root() {
            self.status_wanted = false;
            let normalized = self.settings.normalized();
            // Taken from the value that builds the params, so what is recorded is what travelled.
            self.status_query_root = Some(normalized.effective_root());
            self.status_cache_question = Some(Self::cache_question(&normalized));
            let params = normalized.to_params(false);
            let (tx, rx) = mpsc::channel();
            self.status_rx = Some(rx);
            thread::spawn(move || {
                let _ = tx.send(fetch_flux3_status(params));
            });
        }
    }

    /// The `.status` answer, but only while it still describes the root the settings name.
    pub(super) fn status_for_current_root(&self) -> Option<&Flux3Status> {
        flux3_status_for_root(self.status.as_ref(), self.status_root.as_deref(), &self.settings.effective_root())
    }

    /// THE model verdict, guarded against a stale answer; read by the readiness line and the
    /// gate. A prompt served from the cache waives the text encoder in TEXT mode.
    pub(super) fn model_readiness(&self) -> Flux3ModelReadiness {
        flux3_model_readiness(&self.settings, self.status_for_current_root(), self.prompt_cache_state())
    }

    /// Whether the backend serves the prompt CURRENTLY in the field from the cache (memory or
    /// library). `None` = not known: no answer about the current root, an answer about a prompt
    /// the user has typed away from, or a backend without the field.
    pub(super) fn prompt_cache_state(&self) -> Option<bool> {
        let status = self.status_for_current_root()?;
        prompt_cache_state_for(status.prompt_cache_asked_about.as_deref(), status.prompt_cached, &self.settings.prompt)
    }

    /// The entry `.status` named for the prompt in the field (the one a run would use), under
    /// the same guard as [`Self::prompt_cache_state`].
    pub(super) fn prompt_cache_status_entry(&self) -> Option<&PromptLibraryEntry> {
        self.prompt_cache_state()?;
        self.status_for_current_root()?.prompt_cache_entry.as_ref()
    }

    /// The cache question a `.status` asked with `settings` carries: the trimmed prompt and
    /// whether TEXT mode is in effect (the cache serves that mode only).
    fn cache_question(settings: &Flux3KleinSettings) -> (String, bool) {
        (settings.prompt.trim().to_string(), settings.text_mode_in_effect())
    }

    /// Records a parameter change: a save is owed, the forecast is re-armed, a "moved aside"
    /// notice retires; `.status` is re-asked when the package root or the cache question
    /// (prompt, encoder mode) moved — a prompt edit, a library pick and the «Референсы и в
    /// текстовый энкодер» switch all land here — and a moved root also re-asks the library
    /// listing (every entry's `current_encoder` is an answer about the encoder under it).
    /// Re-arming the one-shot flags, never firing a query here, keeps a keystroke from becoming
    /// a request.
    pub(super) fn note_settings_changed(&mut self) {
        self.note_postprocess_changed();
        self.resources.arm();
        let root = self.settings.effective_root();
        let asked = self.status_query_root.as_deref().or(self.status_root.as_deref());
        if asked != Some(root.as_str()) {
            self.status_wanted = true;
            self.prompt_cache.arm_list();
        }
        if self.status_cache_question.as_ref() != Some(&Self::cache_question(&self.settings)) {
            self.status_wanted = true;
        }
    }

    /// Records a change of the «Постобработка» values ONLY: a save is owed and a "moved aside"
    /// notice retires, but neither the forecast nor `.status` is re-armed — post-processing
    /// runs in the host after the run and is no part of either question (`to_params` leaves
    /// those keys out), and a dragged dial changes it on every frame.
    pub(super) fn note_postprocess_changed(&mut self) {
        self.dirty = true;
        if matches!(self.settings_problem, Some(SettingsProblem::MovedAside(_))) {
            self.settings_problem = None;
        }
    }

    /// Drains the forecast and asks the next question when one is armed. The gate also needs an
    /// absolute package root (`has_usable_root`): the backend refuses a forecast without
    /// `source_root` or with relative candidate paths. PyTorch only: the forecast is the
    /// backend's large-model manager's, which an sd.cpp run never meets.
    pub(super) fn poll_and_maybe_query_profiles(&mut self) {
        let region = self.region.map(|rect| [rect.w, rect.h]);
        let pytorch = self.settings.runtime() == ImageEditRuntime::PyTorch;
        let gate_open = pytorch && self.ai_backend_available && !self.pipeline_busy() && self.settings.has_usable_root();
        let Self { resources, settings, references, marks_reference, .. } = self;
        resources.poll_query(region, gate_open, |size| Self::profiles_request(settings, references, *marks_reference, size));
    }

    /// Folds one drawn frame of the panel: its intents (`apply_intents`), then — when the
    /// weight-format selection or the runtime moved during the frame (the install pickers, the
    /// forecast's DiT or encoder rows, a format suggestion, the runtime switch) — re-probes the
    /// new catalog bundle and re-asks `.status` (asked only on PyTorch; the sd.cpp files worker
    /// follows the selection's key by itself). `selection_before` is [`Self::catalog_selection`]
    /// taken before the panel drew.
    pub(super) fn fold_panel_frame(&mut self, intents: Flux3PanelIntents, selection_before: (Flux3DitFormat, Flux3EncoderFormat, Flux3SourceMode, ImageEditRuntime)) {
        self.apply_intents(intents);
        let selection = self.catalog_selection();
        if selection != selection_before {
            self.request_catalog_status();
            // The per-format rows answer every candidate, but `text_encoder_present`,
            // `available` and `prompt_cached` describe the selection the question carried: ask
            // again.
            self.status_wanted = true;
            if selection.1 != selection_before.1 || selection.2 != selection_before.2 || selection.3 != selection_before.3 {
                // Every listed entry's `current_encoder` follows the encoder; back on PyTorch the
                // listing was not asked while sd.cpp ran.
                self.prompt_cache.arm_list();
            }
        }
    }

    /// The forecast question for the current settings, references and a `size` region.
    ///
    /// `shape.references` lists exactly what the run sends, in its order: the region itself
    /// (reference #0), each user reference, and — when `marks_reference` — the region-sized
    /// marks reference last (`run_flux3_klein_pass` appends it there).
    pub(super) fn profiles_request(settings: &Flux3KleinSettings, references: &Flux3References, marks_reference: bool, size: [usize; 2]) -> ProfilesRequest {
        let normalized = settings.normalized();
        let side = |px: usize| u32::try_from(px).unwrap_or(u32::MAX);
        let region = [side(size[0]), side(size[1])];
        ProfilesRequest {
            model: FLUX3_LARGE_MODEL.to_string(),
            params: normalized.to_params(false),
            region,
            steps: Some(normalized.steps),
            format: Some(normalized.dit_format().wire().to_string()),
            choice: normalized.step_choice(),
            references: std::iter::once(region).chain(references.sizes()).chain(marks_reference.then_some(region)).collect(),
        }
    }

    /// Whether an operation holds the backend or the sd.cpp worker and the shared progress bar:
    /// a run of either runtime, a cancelled run still ending (the run gate names the latter
    /// itself, `cancel_pending_error`), or a prompt-cache operation (a build streams on the same
    /// bar and reads the encoder). The catalog download is not one: it runs in the app, not in
    /// the backend, and has its own progress in its block.
    pub(super) fn pipeline_busy(&self) -> bool {
        self.run_rx.is_some() || self.sdcpp_run.is_some() || self.abandoned.busy() || self.prompt_cache.busy()
    }

    /// Why the PyTorch / Stable-Diffusion.cpp switch is closed right now: a run of either
    /// runtime (or its prep), a cancelled run still ending, a prompt-cache operation or a
    /// `.unload` in flight — each belongs to the runtime it started on — or a catalog job (the
    /// runtime picks the bundle, so a switch would move the bundle under the running job).
    /// `None` = it may switch.
    pub(super) fn runtime_switch_block_reason(&self) -> Option<String> {
        catalog_switch_block_reason(self.downloads.as_ref().and_then(OcrModelDownloadController::running_job))
            .or_else(|| (self.pipeline_busy() || self.unload_rx.is_some()).then(|| t!("cleaning.tools.flux3_klein.switch_busy_reason").to_string()))
    }

    /// The download block's view of the catalog bundle the current selection needs
    /// ([`flux3_catalog_bundle`]); `None` without a controller (a test build) or in manual mode.
    pub(super) fn catalog_snapshot(&self) -> Option<OcrModelPanelSnapshot> {
        let downloads = self.downloads.as_ref()?;
        let bundle = flux3_catalog_bundle(&self.settings);
        (!bundle.is_empty()).then(|| downloads.panel_snapshot(&bundle))
    }

    /// Drives the catalog download every frame, panel visible or not: probes the current
    /// bundle's never-probed specs (which also registers it for the automatic adoption),
    /// applies the controller's worker events and notices the end of a catalog job. Returns
    /// `true` while the GUI must keep repainting.
    ///
    /// A job's end (download, verify or adoption of ANY bundle) means files on disk may have
    /// changed: `.status` (the presence marks and the verdict) and the forecast are asked again.
    pub(super) fn poll_catalog_bundle(&mut self) -> bool {
        let bundle = flux3_catalog_bundle(&self.settings);
        let Some(downloads) = self.downloads.as_mut() else {
            return false;
        };
        downloads.probe_unknown(&bundle);
        let repaint = downloads.poll() || downloads.is_active();
        // An empty bundle (manual mode) still sees another bundle's job as `other_download`,
        // so a download left running across a mode switch is not missed.
        let running = catalog_job_running(&downloads.panel_snapshot(&bundle));
        if self.catalog_job_running && !running {
            self.status_wanted = true;
            self.sdcpp_files_wanted = true;
            self.resources.invalidate();
            // The encoder files may have changed: the listing's `current_encoder` follows them.
            self.prompt_cache.arm_list();
        }
        self.catalog_job_running = running;
        repaint
    }

    /// What the catalog bundle follows: the two weight formats, the source mode and the runtime
    /// (sd.cpp fetches another set).
    pub(super) fn catalog_selection(&self) -> (Flux3DitFormat, Flux3EncoderFormat, Flux3SourceMode, ImageEditRuntime) {
        (self.settings.dit_format(), self.settings.encoder_format(), self.settings.source_mode(), self.settings.runtime())
    }

    /// Moves the runtime to `runtime` the way a click on the panel's switch does: the runtime
    /// is written and folded as a frame whose only intent is a settings change (saved, and the
    /// selection change re-probes the bundle). Used by the automatic switch
    /// (`AiEngine::on_selected`).
    fn switch_runtime(&mut self, runtime: ImageEditRuntime) {
        let selection_before = self.catalog_selection();
        self.settings.runtime = runtime.wire().to_string();
        self.fold_panel_frame(Flux3PanelIntents { settings_changed: true, ..Flux3PanelIntents::default() }, selection_before);
    }

    /// Re-probes the bundle the CURRENT selection needs (even specs probed before: the user may
    /// have changed the disk). Called after the selection or the source mode changed in a
    /// frame (`draw_parameters`) and on «Обновить».
    pub(super) fn request_catalog_status(&mut self) {
        let bundle = flux3_catalog_bundle(&self.settings);
        if let Some(downloads) = self.downloads.as_mut()
            && !bundle.is_empty()
        {
            downloads.request_bundle_status(&bundle);
        }
    }

    /// Applies a click of the catalog download block to the bundle the current selection
    /// needs. The block is drawn only while its snapshot describes that very bundle, so the
    /// action can never land on another one.
    pub(super) fn apply_catalog_action(&mut self, action: ModelDownloadAction) {
        let bundle = flux3_catalog_bundle(&self.settings);
        let Some(downloads) = self.downloads.as_mut() else {
            return;
        };
        match action {
            ModelDownloadAction::Download => downloads.start_download(&bundle),
            ModelDownloadAction::Verify => downloads.start_verify(&bundle),
            ModelDownloadAction::Cancel => downloads.cancel(),
            ModelDownloadAction::None => {}
        }
    }

    /// The frame rule (multiple of 16, side >= 128, area <= 4 MP, aspect <= 4:1), decided by
    /// `region_edit_v2::geometry::check_size` — the one owner of validity — over these values.
    pub(super) fn frame_constraints() -> FrameConstraints {
        FrameConstraints {
            multiple: FLUX3_SELECTION_MULTIPLE,
            min_side: FLUX3_MIN_SELECTION_PX,
            max_area: Some(FLUX3_MAX_SELECTION_AREA_PX2),
            aspect: Some(AspectLimit::symmetric(FLUX3_MAX_SELECTION_ASPECT)),
            ..FrameConstraints::UNCONSTRAINED
        }
    }

    /// Turns the host's request into one run's pixels, or refuses it.
    ///
    /// The size guarantees of `EngineRunRequest` are CHECKED (region, mask, marks reference all
    /// `rect_px`), the region against the frame rule (`region_size_refusal`), and the reference
    /// count against the model's limit with the marks reference counted. A transparent marks
    /// layer cannot arrive by contract (`marks_support` does not offer it) and is refused and
    /// logged. The working mode is derived here by `mask_for_run`.
    ///
    /// # Errors
    /// A localized message for any of those refusals.
    pub(super) fn run_input(request: EngineRunRequest, references: &Flux3References) -> Result<Flux3RunInput, String> {
        let EngineRunRequest { page_idx, rect_px, region, masks, marks } = request;
        let marks_reference = match marks {
            RunMarks::None => None,
            RunMarks::Reference(reference) => Some(reference),
            RunMarks::Layer(layer) => {
                ms_log::runtime_log::log_error(format!(
                    "[cleaning] {FLUX3_LOG_LABEL} was handed the marks as a transparent {}x{} layer, which it does not support: page {page_idx}, rect {}x{} at ({}, {}); run refused",
                    layer.width(),
                    layer.height(),
                    rect_px.w,
                    rect_px.h,
                    rect_px.x,
                    rect_px.y
                ));
                return Err(t!("cleaning.tools.area_editor.marks_mode_unsupported_hint").to_string());
            }
        };
        let size = [rect_px.w, rect_px.h];
        let expected = rect_px.w.saturating_mul(rect_px.h);
        let mask_ok = masks.len() == 1 && masks[0].len() == expected;
        if region.size != size || !mask_ok || marks_reference.as_ref().is_some_and(|marks| marks.size != size) {
            ms_log::runtime_log::log_warn(format!(
                "[cleaning] {FLUX3_LOG_LABEL} run request does not match the frame: page {page_idx}, rect {}x{} at ({}, {}), region {}x{}, mask layer bytes {:?}, expected {expected}",
                rect_px.w,
                rect_px.h,
                rect_px.x,
                rect_px.y,
                region.size[0],
                region.size[1],
                masks.iter().map(Vec::len).collect::<Vec<_>>()
            ));
            return Err(t!("cleaning.region.invalid_selection_size_error").to_string());
        }
        if let Some(reason) = region_size_refusal(rect_px.w, rect_px.h, &Self::frame_constraints()) {
            return Err(reason);
        }
        if references.items.len() + usize::from(marks_reference.is_some()) > FLUX3_MAX_EXTRA_REFERENCES {
            return Err(tf!("cleaning.tools.flux3_klein.too_many_references_error", max = FLUX3_MAX_EXTRA_REFERENCES));
        }
        let (mask, whole_region) = mask_for_run(&masks[0]);
        Ok(Flux3RunInput {
            region,
            mask,
            whole_region,
            size,
            references: references.items.iter().map(|reference| Arc::clone(&reference.png)).collect(),
            marks_reference,
        })
    }

    /// Drains the PyTorch run channel: a finished run records its seed and invalidates the
    /// forecast.
    fn poll_run(&mut self) -> EnginePoll {
        let Some(rx) = self.run_rx.as_ref() else {
            return EnginePoll::Idle;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return EnginePoll::Running,
            Err(TryRecvError::Disconnected) => Err(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string()),
        };
        self.run_rx = None;
        // Weights moved, the backend may have degraded and learned a floor.
        self.resources.invalidate();
        self.status_wanted = true;
        match result {
            Ok(Flux3RunOutcome { answer, mask, whole_region }) => {
                ms_log::runtime_log::log_info(format!(
                    "[cleaning] {FLUX3_LOG_LABEL} run finished: seed {}, steps {}, format {}, compute {}, encoder {}, ae {}, whole region {whole_region}",
                    answer.seed, answer.steps, answer.applied.format, answer.applied.compute_path, answer.applied.encoder_mode, answer.applied.ae_dtype
                ));
                self.last_seed = Some(answer.seed);
                self.run_status = Some(flux3_run_done_status(answer.seed, answer.degraded.as_ref()));
                // The raw model output: the host composites it inside `mask` with the live
                // «Постобработка» settings. The colour match samples the same mask (FLUX.3 has
                // no separate latent mask).
                EnginePoll::DoneRaw(RawRegionResult { image: answer.image, mask, sample_mask: None, whole_region })
            }
            Err(err) => {
                let message = tf!("cleaning.mask_editor.processing_error", err = err);
                self.run_status = Some(message.clone());
                EnginePoll::Failed(message)
            }
        }
    }

    /// Abandons the run in flight: its answer is never read, its progress generation retired,
    /// and the backend told to stop (it checks at every stage and every denoise step) — sticky,
    /// so a cancel before the request id exists is sent the moment the worker publishes it. The
    /// worker is kept in `abandoned` until it ends; no new run starts meanwhile. An sd.cpp run is
    /// cancelled through `cancel_sdcpp_run`. Returns whether there was a run.
    pub(super) fn cancel_run(&mut self) -> bool {
        let Some(rx) = self.run_rx.take() else {
            return self.cancel_sdcpp_run();
        };
        self.abandoned.push(rx);
        retire_progress_generation(&self.progress, FLUX3_LOG_LABEL);
        self.run_status = Some(t!("cleaning.mask_editor.processing_cancelled_status").to_string());
        true
    }
}

/// Whether two frame rectangles describe the same region (`OverlayRectPx` has no `PartialEq`).
fn same_region(a: Option<&OverlayRectPx>, b: Option<&OverlayRectPx>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.x == b.x && a.y == b.y && a.w == b.w && a.h == b.h,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

/// Whether two frame rectangles have the same SIZE (the forecast's only geometric input).
fn same_region_size(a: Option<&OverlayRectPx>, b: Option<&OverlayRectPx>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.w == b.w && a.h == b.h,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

impl AiEngine for Flux3KleinEngine {
    /// The value the shared `SettingsSaver` still owes, written now on the teardown worker. A
    /// settings load that landed meanwhile is applied first, so its gate is the real one.
    fn flush_settings_blocking(&mut self) {
        self.poll_settings_load();
        self.saver.flush_blocking(
            SaveGate { dirty: &mut self.dirty, loaded: self.settings_loaded, blocked: self.settings_save_blocked },
            &self.settings,
        );
    }

    fn id(&self) -> &'static str {
        FLUX3_ENGINE_ID
    }

    fn title(&self) -> String {
        t!("cleaning.tools.flux3_klein.title").to_string()
    }

    fn section(&self) -> EngineSection {
        EngineSection::WithPrompt
    }

    /// Only the PyTorch runtime needs Torch.
    fn requires_torch(&self) -> bool {
        self.settings.runtime() == ImageEditRuntime::PyTorch
    }

    /// PyTorch or Stable-Diffusion.cpp, whichever the switch shows: the picker button is open
    /// while either runtime is available (`AiCaps` folds `--no-ai`, the platform and the sd.cpp
    /// install into the sd.cpp half).
    fn ai_requirement(&self) -> Option<AiRequirement> {
        Some(AiRequirement::TorchOrSdcpp)
    }

    /// Decides the runtime on the user's selection (`decide_runtime_on_selection`) — but NEVER
    /// on settings that have not been loaded: the defaults say PyTorch, and a change made now
    /// would make the late load discard the user's file. While the load is in flight the event
    /// is remembered and decided when it lands.
    fn on_selected(&mut self, caps: &AiCaps) {
        self.poll_settings_load();
        if self.settings_rx.is_some() {
            self.selection_pending = true;
            return;
        }
        self.decide_runtime_on_selection(caps);
    }

    fn constraints(&self) -> FrameConstraints {
        Self::frame_constraints()
    }

    /// ONE layer: the area the model's result may replace.
    fn mask_layers(&self) -> Vec<MaskLayerSpec> {
        vec![MaskLayerSpec { tint: FLUX3_MASK_TINT, label_key: "cleaning.tools.flux3_klein.mask_heading" }]
    }

    /// Unconditionally `true`: an empty mask is the whole-region mode (`mask_for_run`).
    fn allows_empty_mask(&self) -> bool {
        true
    }

    /// The marks preferably travel as a separate reference (a copy of the region with the marks
    /// on it, one more extra reference) — but only while the user's list leaves room for it;
    /// with nine user references only «На редактируемую картинку» remains. Never a transparent
    /// layer: the model reads RGB images.
    fn marks_support(&self) -> MarksSupport {
        if self.references.allows_marks_reference() {
            MarksSupport::new(MarksMode::SeparateReference).with(MarksMode::OverlayOnRegion)
        } else {
            MarksSupport::OVERLAY_ONLY
        }
    }

    /// Always `Some` (every run answers `DoneRaw`): the persisted «Постобработка» values,
    /// normalized. Read every frame by the host; a copy of four numbers.
    fn postprocess_settings(&self) -> Option<PostprocessSettings> {
        Some(self.settings.postprocess.normalized())
    }

    fn draw_parameters(&mut self, ui: &mut egui::Ui) {
        let readiness = self.model_readiness();
        // Whether the held answer describes the current root (computed before the destructure
        // below borrows the fields apart): the presence marks read it guarded.
        let status_current = self.status_for_current_root().is_some();
        let pipeline_busy = self.pipeline_busy();
        let hf_token_state = ms_sysprobe::hf_token::hf_token_state();
        let hf_token_busy = self.hf_token.busy();
        // The catalog block (download mode): its snapshot is taken here, before the format
        // pickers may move the bundle this frame; the block is drawn only while the snapshot
        // still describes the bundle the selection needs.
        let catalog_snapshot = self.catalog_snapshot();
        let bundle_missing = catalog_snapshot.as_ref().is_some_and(|snapshot| catalog_bundle_missing(&snapshot.state));
        // The gated repositories the token row is drawn for: those of the bundle's specs the
        // controller has not seen installed (its last probe; nothing here touches the disk).
        let gated_repos = catalog_gated_repos_missing(&flux3_catalog_bundle(&self.settings), |spec| {
            self.downloads.as_ref().is_some_and(|downloads| matches!(downloads.state_for(spec.id), OcrModelDownloadState::Installed))
        });
        // The selection the frame began with: the pickers, the memory widget's format rows and
        // a format suggestion may move it; a change re-probes the new bundle after the fold.
        let selection_before = self.catalog_selection();
        // The cache half of `.status`, guarded to the typed prompt (copied out before the
        // destructure below borrows the fields apart).
        let prompt_cache_state = self.prompt_cache_state();
        let prompt_cache_status_entry = self.prompt_cache_status_entry().cloned();
        // The Stable-Diffusion.cpp facts the panel reads: the process-global status (one short
        // lock), the `--no-ai` / platform gate, the saved device and the files state of the
        // selection; the switch's block reason and whether the switch is offered at all.
        let sdcpp_status = ms_sdcpp::snapshot();
        let sdcpp_local = sdcpp_route_gate();
        let sdcpp_device = self.sdcpp_files.as_ref().map(|answer| answer.device.clone());
        let sdcpp_files = self.sdcpp_files_state();
        let runtime_switch_block = self.runtime_switch_block_reason();
        let pytorch_unavailable = pytorch_unavailable_reason(&AiCaps::current());
        // Offered where the sd.cpp route is available, and always while it is selected (a way back).
        let show_runtime_switch = sdcpp_route_available() || self.settings.runtime() == ImageEditRuntime::SdCpp;
        let mut intents = Flux3PanelIntents::default();
        {
            let Self {
                settings,
                seed_input,
                last_seed,
                status,
                status_error,
                resources,
                references,
                references_rx,
                references_errors,
                settings_problem,
                saver,
                unload_status,
                hf_token,
                prompt_cache,
                ai_backend_available,
                install_section_seeded,
                install_section_open_prev,
                ..
            } = self;
            let mut panel = Flux3PanelCtx {
                settings,
                seed_input,
                last_seed: *last_seed,
                status: status.as_ref(),
                current_status: status.as_ref().filter(|_| status_current),
                status_error: status_error.as_deref(),
                readiness,
                resources: resources.view(*ai_backend_available),
                references,
                references_busy: references_rx.is_some(),
                references_errors,
                settings_problem: settings_problem.as_ref(),
                save_error: saver.error.as_deref(),
                unload_status: unload_status.as_deref(),
                hf_token_state,
                hf_token_input: &mut hf_token.input,
                hf_token_status: hf_token.status.as_deref(),
                hf_token_busy,
                gated_repos: &gated_repos,
                catalog_snapshot: catalog_snapshot.as_ref(),
                bundle_missing,
                ai_backend_available: *ai_backend_available,
                pipeline_busy,
                prompt_cache,
                prompt_cache_state,
                prompt_cache_status_entry: prompt_cache_status_entry.as_ref(),
                sdcpp_status: &sdcpp_status,
                sdcpp_local,
                sdcpp_device: sdcpp_device.as_ref(),
                sdcpp_files,
                runtime_switch_block: runtime_switch_block.as_deref(),
                pytorch_unavailable,
                show_runtime_switch,
                install_section_seeded,
                install_section_open_prev,
                intents: &mut intents,
            };
            // One namespace over the whole body, so no widget state is shared with FLUX.2's.
            ui.push_id(FLUX3_ENGINE_ID, |ui| panel.draw(ui));
        }
        self.fold_panel_frame(intents, selection_before);
    }

    /// Draws nothing: FLUX.3 has no per-run option. It only learns whether the next run would
    /// carry a marks reference, which the forecast counts, and re-asks the forecast on a change.
    fn draw_run_options(&mut self, _ui: &mut egui::Ui, ctx: RunOptionsCtx) {
        if self.marks_reference != ctx.marks_reference {
            self.marks_reference = ctx.marks_reference;
            self.resources.arm();
        }
    }

    /// The shared progress bar with the backend stages named in the UI language, and the last
    /// run's line under it.
    fn draw_progress(&mut self, ui: &mut egui::Ui) {
        draw_stream_progress_staged(ui, &self.progress, flux3_stage_label);
        if let Some(status) = self.run_status.as_deref() {
            ui.small(status);
        }
    }

    fn run_block_reason(&self) -> Option<String> {
        if self.abandoned.busy() {
            return Some(t!("cleaning.tools.flux3_klein.cancel_pending_error").to_string());
        }
        if self.references_rx.is_some() {
            return Some(t!("cleaning.tools.flux3_klein.references_loading_error").to_string());
        }
        // A prompt-cache operation holds the backend's pipeline and the one progress bar a
        // starting run would steal (FLUX.2 klein's rule).
        if self.prompt_cache.busy() {
            return Some(t!("cleaning.tools.flux3_klein.prompt_cache_busy_error").to_string());
        }
        let seed = parse_seed_input(&self.seed_input);
        match self.settings.runtime() {
            // The engine's half of the Torch rule wins over the host's generic refusal: it names
            // the way out (the sd.cpp runtime) where this platform has it.
            ImageEditRuntime::PyTorch => flux3_torch_block_reason(ImageEditRuntime::PyTorch, self.torch_available, sdcpp_route_available())
                .or_else(|| flux3_run_block_reason(&self.settings, &self.model_readiness(), seed)),
            ImageEditRuntime::SdCpp => flux3_sdcpp_run_block_reason(&self.settings, self.sdcpp_readiness().run_block_reason(), &self.sdcpp_files_state(), seed),
        }
    }

    /// Refuses the switch while a job of THIS engine's catalog controller runs (download,
    /// verify or automatic adoption, the shared [`catalog_switch_block_reason`]): a
    /// multi-gigabyte transfer no frame state describes, and the host polls only the selected
    /// engine, so a job left behind by a switch would run unseen. An sd.cpp run (its prep
    /// included) refuses it too — and so does a cancelled run still ending (`abandoned`): a
    /// cancelled sd.cpp job keeps the service's generation slot until its worker answered, so
    /// the next engine's sd.cpp run would be refused with a misleading text.
    fn switch_block_reason(&self) -> Option<String> {
        catalog_switch_block_reason(self.downloads.as_ref().and_then(OcrModelDownloadController::running_job))
            .or_else(|| (self.sdcpp_run.is_some() || self.abandoned.busy()).then(|| t!("cleaning.tools.flux3_klein.switch_busy_reason").to_string()))
    }

    /// Validates the request and starts the run on a worker, on the selected runtime.
    ///
    /// # Errors
    /// A localized message when a run is in flight or a cancelled one still ends, the
    /// references are still loading, a prompt-cache operation holds the pipeline, the seed
    /// field is invalid, the runtime's gate refuses (Torch missing; sd.cpp not ready or its
    /// files not validated), or [`Flux3KleinEngine::run_input`] refuses it.
    fn start(&mut self, request: EngineRunRequest) -> Result<(), String> {
        if self.run_rx.is_some() || self.sdcpp_run.is_some() {
            return Err(t!("cleaning.mask_editor.processing_already_running_status").to_string());
        }
        if let Some(reason) = self.run_block_reason() {
            return Err(reason);
        }
        let input = Self::run_input(request, &self.references)?;
        // The request is the truth about the marks: correct a stale forecast question.
        if self.marks_reference != input.marks_reference.is_some() {
            self.marks_reference = input.marks_reference.is_some();
            self.resources.arm();
        }
        if self.settings.runtime() == ImageEditRuntime::SdCpp {
            return self.start_sdcpp(input);
        }
        let settings = self.settings.normalized();
        let claim = begin_progress_generation(&self.progress);
        let progress = Arc::clone(&self.progress);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(run_flux3_klein(input, &settings, &progress, &claim));
        });
        self.run_rx = Some(rx);
        self.run_status = None;
        Ok(())
    }

    /// Drains every channel, keeps the derived state current, reports the run.
    fn poll(&mut self, ctx: &egui::Context) -> EnginePoll {
        // A cancelled worker still ending keeps the gate closed; wake the GUI until it ends.
        if self.abandoned.poll() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        self.poll_settings_load();
        self.poll_and_maybe_query_status();
        self.poll_and_maybe_resolve_sdcpp_files();
        self.poll_unload();
        self.poll_hf_token();
        self.poll_prompt_cache();
        self.poll_and_maybe_query_prompt_cache_list();
        let catalog_repaint = self.poll_catalog_bundle();
        self.poll_picker();
        self.poll_references();
        self.resources.poll_side_jobs(Instant::now());
        // At most one runtime has a run: the PyTorch one answers first, else the sd.cpp one.
        let run = match self.poll_run() {
            EnginePoll::Idle => self.poll_sdcpp_run(),
            answered @ (EnginePoll::Running | EnginePoll::Done(_) | EnginePoll::DoneRaw(_) | EnginePoll::Failed(_)) => answered,
        };
        self.poll_and_maybe_query_profiles();
        let now = Instant::now();
        self.poll_and_maybe_save(now);
        if self.resources.wants_repaint() || self.references_rx.is_some() || self.prompt_cache.list_busy() || self.sdcpp_files_rx.is_some() || catalog_repaint {
            ctx.request_repaint_after(LIMITS_EDIT_DEBOUNCE);
        }
        // The settings save debounce, a failed save's retry and a write in flight: the saver
        // says exactly when it needs the next poll.
        if let Some(after) = self.saver.wake_after(now) {
            ctx.request_repaint_after(after);
        }
        if matches!(run, EnginePoll::Running) || self.prompt_cache.busy() {
            // The bar moves on backend frames, not on input (a prompt-cache build drives it too).
            ctx.request_repaint();
        }
        run
    }

    fn cancel(&mut self) {
        self.cancel_run();
    }

    /// A backend that came (back) up is a fresh process: forecast and status are asked again.
    fn set_backend_available(&mut self, available: bool) {
        if available && !self.ai_backend_available {
            self.resources.invalidate();
            self.status_wanted = true;
        }
        self.ai_backend_available = available;
    }

    /// Stored for the engine's own half of the Torch rule (`flux3_torch_block_reason`: PyTorch
    /// selected without Torch names the sd.cpp runtime); the host's run gate still applies its
    /// generic requirement after it (`requires_torch`).
    fn set_torch_available(&mut self, available: bool) {
        self.torch_available = available;
    }

    /// Stores the rectangle every frame; arms the forecast only on a SETTLED size change, and
    /// cancels a run whose rectangle moved (its answer would describe another place).
    fn set_region(&mut self, region: Option<OverlayRectPx>, geometry_settled: bool) {
        if !same_region_size(self.region.as_ref(), region.as_ref()) {
            self.region_resize_pending = true;
        }
        if !same_region(self.region.as_ref(), region.as_ref()) {
            self.region = region;
            self.cancel_run();
        }
        if self.region_resize_pending && geometry_settled {
            self.region_resize_pending = false;
            self.resources.arm();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: usize, h: usize) -> OverlayRectPx {
        OverlayRectPx { x: 0, y: 0, w, h }
    }

    fn request(w: usize, h: usize, painted: bool, marks: RunMarks) -> EngineRunRequest {
        EngineRunRequest {
            page_idx: 0,
            rect_px: rect(w, h),
            region: egui::ColorImage::filled([w, h], Color32::WHITE),
            masks: vec![vec![if painted { 255 } else { 0 }; w * h]],
            marks,
        }
    }

    /// The frame rule is the backend's (`validate_region_size`) and the run path agrees with
    /// `check_size` on it: one authority, two readers.
    #[test]
    fn the_frame_rule_is_the_backends_region_rule() {
        let c = Flux3KleinEngine::frame_constraints();
        let valid = |w: usize, h: usize| crate::tools::region_edit_v2::geometry::check_size(w, h, &c).is_none();
        assert!(valid(128, 128));
        assert!(valid(2048, 2048), "exactly 4 MP");
        assert!(valid(1024, 4096), "4:1 at the 4096 side");
        assert!(valid(4096, 1024));
        assert!(!valid(2048, 2064), "above 4 MP");
        assert!(!valid(128, 528), "steeper than 4:1");
        assert!(!valid(112, 128), "below the minimum side");
        assert!(!valid(136, 130), "off the 16 grid");
        for (w, h) in [(128, 128), (2048, 2064), (128, 528), (120, 128), (1024, 4096)] {
            assert_eq!(region_size_refusal(w, h, &c).is_some(), !valid(w, h), "{w}x{h}");
        }
    }

    /// An empty mask is the whole-region mode, a painted one travels verbatim; references keep
    /// their order and the marks reference is appended after them.
    #[test]
    fn a_run_request_becomes_the_runs_pixels() {
        let mut references = Flux3References::default();
        references.push_loaded(vec![Ok(test_reference("a", 1)), Ok(test_reference("b", 2))], 0);
        let marks = egui::ColorImage::filled([128, 128], Color32::RED);
        let input = Flux3KleinEngine::run_input(request(128, 128, false, RunMarks::Reference(marks)), &references).expect("valid");
        assert!(input.whole_region);
        assert!(input.mask.iter().all(|value| *value == 255), "the solid mask of the whole-region mode");
        assert_eq!(input.references.iter().map(|png| png[0]).collect::<Vec<_>>(), vec![1, 2]);
        assert!(input.marks_reference.is_some());
        let painted = Flux3KleinEngine::run_input(request(128, 128, true, RunMarks::None), &references).expect("valid");
        assert!(!painted.whole_region);
        assert!(Flux3KleinEngine::run_input(request(120, 128, true, RunMarks::None), &references).is_err(), "off the grid");
        let layer = RunMarks::Layer(image::RgbaImage::new(128, 128));
        assert!(Flux3KleinEngine::run_input(request(128, 128, true, layer), &references).is_err(), "no transparent layer");
    }

    /// Nine user references plus a marks reference would be ten extras: the marks mode closes
    /// and a request that still carries one is refused.
    #[test]
    fn a_full_reference_list_closes_the_separate_marks_reference() {
        let mut engine = Flux3KleinEngine::at_rest();
        assert!(engine.marks_support().supports(MarksMode::SeparateReference));
        engine.references.push_loaded((0..9).map(|i| Ok(test_reference("r", i))).collect(), 0);
        assert_eq!(engine.marks_support(), MarksSupport::OVERLAY_ONLY);
        let marks = RunMarks::Reference(egui::ColorImage::filled([128, 128], Color32::RED));
        assert!(Flux3KleinEngine::run_input(request(128, 128, true, marks), &engine.references).is_err());
        assert!(Flux3KleinEngine::run_input(request(128, 128, true, RunMarks::None), &engine.references).is_ok());
    }

    /// The forecast question: the FLUX.3 adapter, the selected format, the encoder flag in the
    /// params, and the region first among the references.
    #[test]
    fn the_forecast_question_carries_the_format_and_the_references() {
        let mut references = Flux3References::default();
        references.push_loaded(vec![Ok(test_reference("a", 1))], 0);
        let settings = Flux3KleinSettings { format: "nvfp4".to_string(), ..Flux3KleinSettings::default() };
        let question = Flux3KleinEngine::profiles_request(&settings, &references, false, [512, 256]);
        assert_eq!(question.model, "flux3_klein");
        assert_eq!(question.format.as_deref(), Some("nvfp4"));
        assert_eq!(question.steps, Some(16));
        assert_eq!(question.references, vec![[512, 256], [64, 32]]);
        assert_eq!(question.params["encoder_on_cpu"], json!(true));
        assert_eq!(question.params["whole_region"], json!(false));
    }

    /// A settled resize arms the forecast once; a move arms nothing; a moved frame cancels a run.
    #[test]
    fn the_forecast_follows_a_settled_size_and_a_move_cancels_the_run() {
        let mut engine = Flux3KleinEngine::at_rest();
        engine.set_region(Some(rect(128, 128)), true);
        engine.resources.wanted = false;
        engine.set_region(Some(OverlayRectPx { x: 16, y: 0, w: 128, h: 128 }), true);
        assert!(!engine.resources.wanted, "a pure move costs nothing");
        engine.set_region(Some(OverlayRectPx { x: 16, y: 0, w: 256, h: 128 }), false);
        assert!(!engine.resources.wanted, "not while the drag is in flight");
        engine.set_region(Some(OverlayRectPx { x: 16, y: 0, w: 256, h: 128 }), true);
        assert!(engine.resources.wanted, "armed on release");
        let (_tx, rx) = mpsc::channel();
        engine.run_rx = Some(rx);
        engine.set_region(Some(rect(256, 128)), true);
        assert!(engine.run_rx.is_none(), "a moved frame cancels the run");
        assert!(engine.run_status.is_some());
    }

    /// The run gate: an invalid seed field blocks, a fresh engine with no answer yet does not;
    /// a test engine has no catalog controller, so nothing closes the engine picker.
    #[test]
    fn the_run_gate_reads_the_engine_state() {
        let mut engine = Flux3KleinEngine::at_rest();
        // The host pushes the Torch flag every frame; this test is about the PyTorch gate.
        engine.set_torch_available(true);
        assert!(engine.run_block_reason().is_none(), "no answer yet never blocks");
        engine.seed_input = "not a number".to_string();
        assert!(engine.run_block_reason().is_some());
        engine.seed_input.clear();
        assert!(engine.run_block_reason().is_none());
        assert!(engine.switch_block_reason().is_none(), "no catalog job, no block");
        assert!(engine.catalog_snapshot().is_none(), "a test build has no controller");
        assert!(!engine.poll_catalog_bundle());
    }

    /// No `.status` question is asked without a package root (manual mode before a pick): the
    /// verdict is `NoRoot` locally and the candidate paths would not be absolute.
    #[test]
    fn no_status_question_without_a_package_root() {
        let mut engine = Flux3KleinEngine::at_rest();
        engine.ai_backend_available = true;
        engine.settings.source_mode = "manual".to_string();
        engine.status_wanted = true;
        engine.poll_and_maybe_query_status();
        assert!(engine.status_rx.is_none(), "nothing asked");
        assert!(engine.status_wanted, "still wanted once a folder is picked");
        assert_eq!(engine.model_readiness(), Flux3ModelReadiness::NoRoot);
        // A relative folder asks nothing either and blocks the run with its own line.
        engine.settings.model_root = "models/flux3".to_string();
        engine.poll_and_maybe_query_status();
        assert!(engine.status_rx.is_none(), "nothing asked about a relative root");
        assert_eq!(engine.model_readiness(), Flux3ModelReadiness::RootNotAbsolute);
        engine.seed_input.clear();
        assert!(engine.run_block_reason().is_some());
    }

    /// A user edit outranks a settings load that lands after it; an untouched engine adopts the
    /// file and its pinned seed fills the field.
    #[test]
    fn a_late_settings_load_never_overwrites_an_edit() {
        let mut engine = Flux3KleinEngine::at_rest();
        let (tx, rx) = mpsc::channel();
        engine.settings_rx = Some(rx);
        let file = Flux3KleinSettings { seed: Some(77), steps: 30, ..Flux3KleinSettings::default() };
        tx.send(Flux3SettingsLoad { settings: file.clone(), problem: None }).expect("send");
        engine.poll_settings_load();
        assert_eq!(engine.settings, file);
        assert_eq!(engine.seed_input, "77");
        assert!(engine.settings_loaded);

        let mut edited = Flux3KleinEngine::at_rest();
        let (tx, rx) = mpsc::channel();
        edited.settings_rx = Some(rx);
        edited.settings.steps = 5;
        edited.note_settings_changed();
        tx.send(Flux3SettingsLoad { settings: file, problem: None }).expect("send");
        edited.poll_settings_load();
        assert_eq!(edited.settings.steps, 5, "the edit wins");
    }

    /// A status answer counts only for the root it was asked about; switching the source mode
    /// re-asks.
    #[test]
    fn a_root_change_re_asks_the_status() {
        let mut engine = Flux3KleinEngine::at_rest();
        engine.status = Some(status_with(&[Flux3DitFormat::Fp8r], true, true));
        engine.status_root = Some(engine.settings.effective_root());
        engine.status_wanted = false;
        assert_eq!(engine.model_readiness(), Flux3ModelReadiness::Ready);
        engine.settings.source_mode = "manual".to_string();
        engine.settings.model_root = "/home/u/flux.3-klein".to_string();
        engine.note_settings_changed();
        assert!(engine.status_wanted);
        assert_eq!(engine.model_readiness(), Flux3ModelReadiness::Unknown, "an answer about another root is not used");
    }

    /// A text-encoder row picked in «Память и скорость» is the panel's frame like any other
    /// edit: the setting is stored and owed a save, the forecast is re-armed, and the moved
    /// selection re-asks `.status` (and re-probes the catalog bundle, the same branch).
    #[test]
    fn a_picked_encoder_row_saves_and_re_arms_the_status_and_the_forecast() {
        let mut engine = Flux3KleinEngine::at_rest();
        engine.dirty = false;
        engine.status_wanted = false;
        engine.resources.wanted = false;
        let before = engine.catalog_selection();
        let mut intents = Flux3PanelIntents::default();
        intents.settings_changed |= flux3_apply_encoder_pick(&mut engine.settings, Flux3EncoderFormat::Q4KM.wire());
        engine.fold_panel_frame(intents, before);
        assert_eq!(engine.settings.encoder_format(), Flux3EncoderFormat::Q4KM);
        assert!(engine.dirty, "a save is owed");
        assert!(engine.resources.wanted, "the forecast is re-armed");
        assert!(engine.status_wanted, "the moved selection re-asks `.status`");
        // A frame that moved nothing asks nothing.
        engine.dirty = false;
        engine.status_wanted = false;
        engine.resources.wanted = false;
        let before = engine.catalog_selection();
        engine.fold_panel_frame(Flux3PanelIntents::default(), before);
        assert!(!engine.dirty && !engine.status_wanted && !engine.resources.wanted);
    }

    /// `prompt_cached` counts only for the prompt `.status` was asked about and for the current
    /// root; served in TEXT mode it waives a missing encoder in the run gate, and an edit of the
    /// prompt (or of the encoder mode) re-asks `.status`.
    #[test]
    fn a_cached_prompt_opens_the_run_gate_without_the_encoder() {
        let mut engine = Flux3KleinEngine::at_rest();
        engine.set_torch_available(true);
        let mut status = status_with(&[Flux3DitFormat::Fp8r], true, false);
        status.prompt_cached = Some(true);
        status.prompt_cache_asked_about = Some(engine.settings.prompt.trim().to_string());
        engine.status = Some(status);
        engine.status_root = Some(engine.settings.effective_root());
        engine.status_cache_question = Some((engine.settings.prompt.trim().to_string(), true));
        engine.status_wanted = false;
        assert_eq!(engine.prompt_cache_state(), Some(true));
        assert_eq!(engine.model_readiness(), Flux3ModelReadiness::Ready, "the encoder is waived");
        assert!(engine.run_block_reason().is_none());
        // Another prompt: the answer is about the old one, the encoder is needed again and
        // `.status` is re-asked.
        engine.settings.prompt.push_str(" now");
        engine.note_settings_changed();
        assert_eq!(engine.prompt_cache_state(), None);
        assert!(matches!(engine.model_readiness(), Flux3ModelReadiness::Missing(_)));
        assert!(engine.status_wanted, "a prompt edit re-asks `.status`");
        // The references in the encoder: the same answer waives nothing and `.status` is re-asked.
        engine.settings.prompt = FLUX3_DEFAULT_PROMPT.to_string();
        engine.status_wanted = false;
        engine.settings.vl_refs = true;
        engine.note_settings_changed();
        assert!(engine.status_wanted, "the encoder mode is part of the cache question");
        assert_eq!(engine.prompt_cache_state(), Some(true));
        assert!(matches!(engine.model_readiness(), Flux3ModelReadiness::Missing(_)), "VL_REFS always encodes");
    }

    /// A prompt-cache operation holds the pipeline: the run gate names it, the status and
    /// forecast questions wait, and a picked encoder re-asks the library listing.
    #[test]
    fn a_prompt_cache_operation_holds_the_pipeline() {
        let _locale = english();
        let mut engine = Flux3KleinEngine::at_rest();
        let (_tx, rx) = mpsc::channel();
        engine.prompt_cache.rx = Some(rx);
        assert!(engine.pipeline_busy());
        assert_eq!(engine.run_block_reason().as_deref(), Some(t!("cleaning.tools.flux3_klein.prompt_cache_busy_error")));
        assert!(engine.start(request(128, 128, true, RunMarks::None)).is_err());
        engine.prompt_cache.rx = None;
        engine.prompt_cache.list_wanted = false;
        let before = engine.catalog_selection();
        let mut intents = Flux3PanelIntents::default();
        intents.settings_changed |= flux3_apply_encoder_pick(&mut engine.settings, Flux3EncoderFormat::Q80.wire());
        engine.fold_panel_frame(intents, before);
        assert!(engine.prompt_cache.list_wanted, "every entry's `current_encoder` follows the encoder");
        // A finished build re-asks `.status` and the forecast.
        engine.status_wanted = false;
        engine.resources.wanted = false;
        let (_line, effects) = engine.prompt_cache.apply_outcome(PromptCacheOutcome::Built { written: Vec::new(), renamed: false });
        engine.apply_prompt_cache_effects(effects);
        assert!(engine.status_wanted && engine.resources.wanted);
    }

    /// The forecast question lists exactly the references the run sends, marked or not: the
    /// region, each user reference, and the region-sized marks reference last.
    #[test]
    fn the_forecast_references_equal_the_wire_references_marked_or_not() {
        let mut references = Flux3References::default();
        references.push_loaded(vec![Ok(test_reference("a", 1)), Ok(test_reference("b", 2))], 0);
        let settings = Flux3KleinSettings::default();
        for marked in [false, true] {
            let marks = if marked { RunMarks::Reference(egui::ColorImage::filled([256, 128], Color32::RED)) } else { RunMarks::None };
            let input = Flux3KleinEngine::run_input(request(256, 128, true, marks), &references).expect("valid");
            // The wire list: #0 the region, then what `run_flux3_klein_pass` appends in order.
            let mut wire: Vec<[u32; 2]> = vec![[256, 128]];
            wire.extend(references.sizes());
            if let Some(marks) = input.marks_reference.as_ref() {
                wire.push([u32::try_from(marks.size[0]).unwrap_or(0), u32::try_from(marks.size[1]).unwrap_or(0)]);
            }
            assert_eq!(input.references.len() + usize::from(input.marks_reference.is_some()) + 1, wire.len());
            let question = Flux3KleinEngine::profiles_request(&settings, &references, input.marks_reference.is_some(), [256, 128]);
            assert_eq!(question.references, wire, "marked = {marked}");
        }
    }

    /// The host's marks fact re-arms the forecast on a change only, and a request carrying a
    /// marks reference corrects a stale fact.
    #[test]
    fn the_marks_reference_fact_rearms_the_forecast() {
        let mut engine = Flux3KleinEngine::at_rest();
        let ctx = egui::Context::default();
        let draw = |engine: &mut Flux3KleinEngine, marks_reference: bool| {
            ctx.run_ui(egui::RawInput::default(), |ui| engine.draw_run_options(ui, RunOptionsCtx { mask_painted: true, marks_reference }))
                .drop_without_applying_deltas();
        };
        engine.resources.wanted = false;
        draw(&mut engine, false);
        assert!(!engine.resources.wanted, "no change, no question");
        draw(&mut engine, true);
        assert!(engine.marks_reference && engine.resources.wanted);
    }

    /// A cancelled run is waited for: no new run starts until its worker ended,
    /// and the gate says why; then it reopens by itself.
    #[test]
    fn a_cancelled_run_blocks_the_next_one_until_its_worker_ends() {
        let mut engine = Flux3KleinEngine::at_rest();
        engine.set_torch_available(true);
        let (tx, rx) = mpsc::channel();
        engine.run_rx = Some(rx);
        engine.progress = Arc::new(Mutex::new(EngineProgress { canceller: Some(Arc::new(|_, _| {})), ..EngineProgress::default() }));
        begin_progress_generation(&engine.progress);
        assert!(engine.cancel_run());
        assert!(engine.run_rx.is_none());
        assert!(engine.pipeline_busy());
        let _locale = english();
        assert_eq!(engine.run_block_reason().as_deref(), Some(t!("cleaning.tools.flux3_klein.cancel_pending_error")));
        assert!(engine.start(request(128, 128, true, RunMarks::None)).is_err(), "start refuses as well");
        tx.send(Err("interrupted".to_string())).expect("the worker answers");
        assert!(!engine.abandoned.poll());
        assert!(engine.run_block_reason().is_none(), "the gate reopens once the worker ended");
    }
}
