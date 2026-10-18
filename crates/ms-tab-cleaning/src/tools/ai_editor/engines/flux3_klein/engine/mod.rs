/*
File: cleaning/tools/ai_editor/engines/flux3_klein/engine/mod.rs

Purpose:
The FLUX.3 klein engine itself: the state it keeps between frames, its `AiEngine` contract with
the host, and the polling that keeps that state current. The start/poll pairs of its long
operations live in `actions.rs` beside it.

Main responsibilities:
- own `Flux3KleinEngine` — the settings and their saver, the seed field, the `.status` answer,
  the forecast (`ResourcesController`), the extra references, the token field, the download,
  the run channel and the progress bar;
- implement `AiEngine`: one mask layer, the FLUX.3 frame constraints, marks as a separate
  reference while the reference list has room (else composited onto the region), the run it
  starts, mid-run cancel, the poll it answers with (`EnginePoll::DoneRaw`: the raw region, its
  mask and mode) and the live «Постобработка» settings the host finishes it with
  (`postprocess_settings`);
- validate a run request and turn it into the run's pixels (`run_input`);
- keep the derived state honest: load the settings once, save them when they settle, re-ask
  `.status` when the package root changes, re-arm the forecast on a settled region SIZE.

Key structures:
- `Flux3KleinEngine`

Key functions:
- `AiEngine::{start, poll, cancel, draw_parameters, marks_support, postprocess_settings}`,
  `Flux3KleinEngine::{run_input, frame_constraints, model_readiness, profiles_request}`

Notes:
Nothing here blocks the GUI thread: every start hands its work to an `ms_thread` worker and every
poll only drains a channel. The frame, the painted mask and the pending result are the HOST's.
*/

use super::*;

// The start/poll pairs of the long operations: token, download, pickers, references, unload.
mod actions;

/// The file dialog a running picker serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux3PickerPurpose {
    /// The package folder of manual mode (a directory path used in place).
    ModelRoot,
    /// One or more extra reference images.
    References,
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
    /// The run in flight.
    pub(super) run_rx: Option<Receiver<Result<Flux3RunOutcome, String>>>,
    /// Cancelled runs and downloads whose workers have not ended; while one is running nothing
    /// new starts (`pipeline_busy`), so quick start/cancel cycles cannot stack backend requests.
    pub(super) abandoned: AbandonedJobs,
    pub(super) settings: Flux3KleinSettings,
    pub(super) settings_rx: Option<Receiver<Flux3SettingsLoad>>,
    pub(super) settings_problem: Option<SettingsProblem>,
    pub(super) settings_save_blocked: bool,
    pub(super) settings_loaded: bool,
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
    /// The forecast, the caps and the learned-floor reset.
    pub(super) resources: ResourcesController,
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
    /// The Hugging Face token field and its secret-store worker (never persisted here).
    pub(super) hf_token: HfTokenControl,
    pub(super) download_check: Option<Flux3DownloadCheck>,
    pub(super) download_check_rx: Option<Receiver<Result<Flux3DownloadCheck, String>>>,
    pub(super) download_check_error: Option<String>,
    /// The streaming `.download.start`; it claims the progress bar.
    pub(super) download_rx: Option<Receiver<Result<Flux3DownloadOutcome, String>>>,
    pub(super) download_status: Option<String>,
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
            abandoned: AbandonedJobs::default(),
            settings: Flux3KleinSettings::default(),
            settings_rx: None,
            settings_problem: None,
            settings_save_blocked: false,
            settings_loaded: false,
            dirty: false,
            saver: SettingsSaver::new(save_flux3_settings, FLUX3_LOG_LABEL),
            seed_input: String::new(),
            status: None,
            status_root: None,
            status_rx: None,
            status_query_root: None,
            status_error: None,
            status_wanted: true,
            resources: ResourcesController::new(FLUX3_LARGE_MODEL, FLUX3_LOG_LABEL),
            references: Flux3References::default(),
            marks_reference: false,
            references_rx: None,
            references_errors: Vec::new(),
            unload_rx: None,
            unload_status: None,
            hf_token: HfTokenControl::default(),
            download_check: None,
            download_check_rx: None,
            download_check_error: None,
            download_rx: None,
            download_status: None,
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
    /// disconnected loader unblocks saving with the in-memory settings.
    pub(super) fn poll_settings_load(&mut self) {
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
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.settings_loaded = true;
                self.settings_rx = None;
            }
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

    /// Drains the `.status` query and asks a new one when it is wanted, the backend is up and
    /// nothing holds it. The answer is recorded with the root it was asked about.
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
                    self.status_rx = None;
                }
                Err(TryRecvError::Disconnected) => {
                    self.status_query_root = None;
                    self.status_rx = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        if self.status_wanted && self.ai_backend_available && self.status_rx.is_none() && !self.pipeline_busy() {
            self.status_wanted = false;
            let normalized = self.settings.normalized();
            // Taken from the value that builds the params, so what is recorded is what travelled.
            self.status_query_root = Some(normalized.effective_root());
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

    /// THE model verdict, guarded against a stale answer; read by the readiness line and the gate.
    pub(super) fn model_readiness(&self) -> Flux3ModelReadiness {
        flux3_model_readiness(&self.settings, self.status_for_current_root())
    }

    /// Whether the held check answer still describes the selected format.
    pub(super) fn download_check_current(&self) -> bool {
        download_check_matches_format(self.download_check.as_ref(), self.settings.format())
    }

    /// Records a parameter change: a save is owed, the forecast is re-armed, a "moved aside"
    /// notice retires, and `.status` is re-asked when the package root moved.
    pub(super) fn note_settings_changed(&mut self) {
        self.note_postprocess_changed();
        self.resources.arm();
        let root = self.settings.effective_root();
        let asked = self.status_query_root.as_deref().or(self.status_root.as_deref());
        if asked != Some(root.as_str()) {
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

    /// Drains the forecast and asks the next question when one is armed. The gate also needs a
    /// package root: the backend refuses a forecast without `source_root`.
    pub(super) fn poll_and_maybe_query_profiles(&mut self) {
        let region = self.region.map(|rect| [rect.w, rect.h]);
        let gate_open = self.ai_backend_available && !self.pipeline_busy() && !self.settings.effective_root().is_empty();
        let Self { resources, settings, references, marks_reference, .. } = self;
        resources.poll_query(region, gate_open, |size| Self::profiles_request(settings, references, *marks_reference, size));
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
            format: Some(normalized.format().wire().to_string()),
            choice: normalized.step_choice(),
            references: std::iter::once(region).chain(references.sizes()).chain(marks_reference.then_some(region)).collect(),
        }
    }

    /// Whether an operation holds the backend and the shared progress bar (a run or a download).
    pub(super) fn pipeline_busy(&self) -> bool {
        self.run_rx.is_some() || self.non_run_pipeline_busy()
    }

    /// The same rule without the run: what the RUN gate reads, so a run never reports itself
    /// as the reason it cannot start. A cancelled run or download still ending counts.
    pub(super) fn non_run_pipeline_busy(&self) -> bool {
        self.download_rx.is_some() || self.abandoned.busy()
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

    /// Drains the run channel: a finished run records its seed and invalidates the forecast.
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
    /// worker is kept in `abandoned` until it ends; no new run starts meanwhile. Returns whether
    /// there was a run.
    pub(super) fn cancel_run(&mut self) -> bool {
        let Some(rx) = self.run_rx.take() else {
            return false;
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
    fn id(&self) -> &'static str {
        FLUX3_ENGINE_ID
    }

    fn title(&self) -> String {
        t!("cleaning.tools.flux3_klein.title").to_string()
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
        let download_check_current = self.download_check_current();
        let pipeline_busy = self.pipeline_busy();
        let hf_token_state = ms_sysprobe::hf_token::hf_token_state();
        let hf_token_busy = self.hf_token.busy();
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
                download_check,
                download_check_rx,
                download_check_error,
                download_rx,
                download_status,
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
                download_check: download_check.as_ref().filter(|_| download_check_current),
                download_check_error: download_check_error.as_deref(),
                download_check_busy: download_check_rx.is_some(),
                download_busy: download_rx.is_some(),
                download_status: download_status.as_deref(),
                ai_backend_available: *ai_backend_available,
                pipeline_busy,
                install_section_seeded,
                install_section_open_prev,
                intents: &mut intents,
            };
            // One namespace over the whole body, so no widget state is shared with FLUX.2's.
            ui.push_id(FLUX3_ENGINE_ID, |ui| panel.draw(ui));
        }
        self.apply_intents(intents);
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
        if self.non_run_pipeline_busy() {
            return Some(t!("cleaning.tools.flux3_klein.pipeline_busy_error").to_string());
        }
        if self.references_rx.is_some() {
            return Some(t!("cleaning.tools.flux3_klein.references_loading_error").to_string());
        }
        flux3_run_block_reason(&self.settings, &self.model_readiness(), parse_seed_input(&self.seed_input))
    }

    /// Refuses the switch while the model download or its access check is in flight: a
    /// multi-gigabyte transfer no frame state describes.
    fn switch_block_reason(&self) -> Option<String> {
        if self.download_rx.is_some() {
            return Some(t!("cleaning.tools.flux3_klein.switch_blocked_download").to_string());
        }
        if self.download_check_rx.is_some() {
            return Some(t!("cleaning.tools.flux3_klein.switch_blocked_check").to_string());
        }
        None
    }

    /// Validates the request and starts the run on a worker.
    ///
    /// # Errors
    /// A localized message when a run or a download is in flight, the references are still
    /// loading, the seed field is invalid, or [`Flux3KleinEngine::run_input`] refuses it.
    fn start(&mut self, request: EngineRunRequest) -> Result<(), String> {
        if self.run_rx.is_some() {
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
        self.poll_unload();
        self.poll_hf_token();
        self.poll_download_check();
        self.poll_download();
        self.poll_picker();
        self.poll_references();
        self.resources.poll_side_jobs(Instant::now());
        let run = self.poll_run();
        self.poll_and_maybe_query_profiles();
        let now = Instant::now();
        self.poll_and_maybe_save(now);
        if self.resources.wants_repaint() || self.references_rx.is_some() {
            ctx.request_repaint_after(LIMITS_EDIT_DEBOUNCE);
        }
        // The settings save debounce, a failed save's retry and a write in flight: the saver
        // says exactly when it needs the next poll.
        if let Some(after) = self.saver.wake_after(now) {
            ctx.request_repaint_after(after);
        }
        if matches!(run, EnginePoll::Running) {
            // The bar moves on backend frames, not on input.
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

    /// Empty: the host's `AiButton` resolves the Torch requirement itself.
    fn set_torch_available(&mut self, _available: bool) {}

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

    /// The run gate: a download blocks with its own line, an invalid seed field blocks, a fresh
    /// engine with no answer yet does not.
    #[test]
    fn the_run_gate_reads_the_engine_state() {
        let mut engine = Flux3KleinEngine::at_rest();
        assert!(engine.run_block_reason().is_none(), "no answer yet never blocks");
        engine.seed_input = "not a number".to_string();
        assert!(engine.run_block_reason().is_some());
        engine.seed_input.clear();
        let (_tx, rx) = mpsc::channel();
        engine.download_rx = Some(rx);
        assert!(engine.run_block_reason().is_some());
        assert!(engine.switch_block_reason().is_some(), "the picker is closed during a download");
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
        engine.status = Some(status_with(&[Flux3Format::Fp8r], true, true));
        engine.status_root = Some(engine.settings.effective_root());
        engine.status_wanted = false;
        assert_eq!(engine.model_readiness(), Flux3ModelReadiness::Ready);
        engine.settings.source_mode = "manual".to_string();
        engine.settings.model_root = "/home/u/flux.3-klein".to_string();
        engine.note_settings_changed();
        assert!(engine.status_wanted);
        assert_eq!(engine.model_readiness(), Flux3ModelReadiness::Unknown, "an answer about another root is not used");
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

    /// A cancelled run is waited for: no new run (nor download) starts until its worker ended,
    /// and the gate says why; then it reopens by itself.
    #[test]
    fn a_cancelled_run_blocks_the_next_one_until_its_worker_ends() {
        let mut engine = Flux3KleinEngine::at_rest();
        let (tx, rx) = mpsc::channel();
        engine.run_rx = Some(rx);
        engine.progress = Arc::new(Mutex::new(EngineProgress { canceller: Some(Arc::new(|_, _| {})), ..EngineProgress::default() }));
        begin_progress_generation(&engine.progress);
        assert!(engine.cancel_run());
        assert!(engine.run_rx.is_none());
        assert!(engine.pipeline_busy() && engine.non_run_pipeline_busy());
        let _locale = english();
        assert_eq!(engine.run_block_reason().as_deref(), Some(t!("cleaning.tools.flux3_klein.cancel_pending_error")));
        assert!(engine.start(request(128, 128, true, RunMarks::None)).is_err(), "start refuses as well");
        tx.send(Err("interrupted".to_string())).expect("the worker answers");
        assert!(!engine.abandoned.poll());
        assert!(engine.run_block_reason().is_none(), "the gate reopens once the worker ended");
    }
}
