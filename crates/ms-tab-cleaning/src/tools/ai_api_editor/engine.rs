/*
File: ai_api_editor/engine.rs

Purpose:
`CloudEditEngine`, the one engine of «ИИ редактирование (API)»: hosted image-edit models
reached through `ms_ai_api::image_edit`. It owns the provider/model selection and its key block,
the prompt, the mask blend, the settings file, the run worker and its progress line; the host
owns the frame, the mask, the pending result and the apply path.

Main responsibilities:
- implement `AiEngine` — id, caption, section, the selected offer's size rule as the frame's
  constraints, the single "may change" mask layer, the marks forms the selected model takes,
  the run gate, the run and the per-frame poll;
- draw the «Редактор области» panel body: the mask meaning, the service picker
  (`draw_image_edit_picker`), the prompt, the blend, the size line, the billing and privacy
  notes, and the run's progress and outcome;
- drive the key block's single-flight worker (`ImageEditKeyRunner::pump`) and the settings
  load/save workers from `poll`.

Key structures:
- `CloudEditEngine`, `RunInFlight`

Notes:
Nothing here blocks the GUI thread: the settings IO, every credential-store operation and the
run live on workers; `poll` only drains channels. `constraints()` follows the selected model and
the host re-reads it every frame, so switching model re-validates the frame (red, never
resized). `marks_support()` follows the selected model the same way: a model that takes a
reference image besides the edited one (`ModelOffer::accepts_references`) gets the marks as a
separate reference (preferred), a transparent layer, or drawn onto the region; any other model
only the last. A run carrying separate marks for a model that takes none (the host's mode
switch lags one frame behind a model switch) is refused with a localized message, never sent
without them. The mask means "the model MAY change this"; an empty mask means the whole region.
A cancelled run is detached and its HTTP call may still complete and be billed (the panel says
so); dropping the engine cancels its run the same way (`Drop`). A failure is logged with the
run's own provider and model snapshot (`RunInFlight`), not the picker's current selection. Errors reach the user as the localized `ImageEditError` text and the log as one line with
the provider, model and error — never the key, never the prompt.
*/

use super::constraints::frame_constraints;
use super::decisions::{RunGate, run_block_reason};
use super::settings::{ApiEditSettings, BLEND_RADIUS_MAX_PX, load_api_edit_settings, save_api_edit_settings};
use super::worker::{RunJob, WorkerEvent, spawn_run};
use crate::tools::region_edit_v2::engine::{AiEngine, EnginePoll, EngineRunRequest, EngineSection, MarksMode, MarksSupport, MaskLayerSpec, RunMarks, region_size_refusal};
use crate::tools::region_edit_v2::engine_settings::settings_save_due;
use crate::tools::region_edit_v2::geometry::{FrameConstraints, upscale_factor_for};
use ms_ai_api::image_edit::{CancelFlag, ImageEditKeyRunner, ImageEditKeySlot, ImageEditKeyState, ImageEditProvider, ImageEditSelection, ImageEditStage, MaskBlend, draw_image_edit_picker, key_slot};
use ms_ai_api::{AiApiNotice, KeyBlockActions};
use ms_canvas::OverlayRectPx;
use ms_thread as thread;
use eframe::egui;
use ms_widgets::WheelSlider;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

/// Log prefix of this engine (the host's tag for this tool).
const LOG_TAG: &str = "[cleaning/ai_api_editor]";

/// Id prefix of the service picker's widgets (`draw_image_edit_picker` derives every id from
/// it). Frozen: it keys the picker's stored widget state.
const PICKER_ID_SALT: &str = "cleaning_ai_api_editor_picker";
/// `id_salt` of the prompt field.
const PROMPT_ID_SALT: &str = "cleaning_ai_api_editor_prompt";
/// `id_salt` of the collapsible blend section (its caption is localized).
const BLEND_SECTION_ID_SALT: &str = "cleaning_ai_api_editor_blend";

/// Rows of the prompt field.
const PROMPT_ROWS: usize = 3;

/// How often the panel repaints while a worker of this engine is busy: the spinner and the
/// stage line move, and a finished worker is noticed within this delay without input.
const BUSY_REPAINT: Duration = Duration::from_millis(100);

/// Preview tint of the "may change here" mask layer: the same blue as FLUX.2 klein's
/// edit-permission layer (`ai_editor/engines/flux2_klein`, `FLUX2_MASK_TINT`), so one meaning
/// reads as one colour across the two tools — NOT the yellow removal tint of the inpainting
/// engines, whose mask means the opposite. Opaque on purpose: `MaskStack` scales the alpha.
const MASK_TINT: egui::Color32 = egui::Color32::from_rgb(80, 200, 255);

/// The run in flight.
#[derive(Debug)]
struct RunInFlight {
    /// The worker's events.
    events: Receiver<WorkerEvent>,
    /// Shared with the worker; raised by `cancel` and when the engine is dropped.
    cancel: CancelFlag,
    /// The last stage the worker reported.
    stage: ImageEditStage,
    /// The provider the run was started with (the picker stays live during a run, so the
    /// current selection may name another one by the time the run ends).
    provider: ImageEditProvider,
    /// The model id the run was started with, for the same reason.
    model_id: String,
}

/// The engine's last outcome line under the parameters.
#[derive(Debug, Clone)]
struct StatusLine {
    text: String,
    error: bool,
}

/// The «Облачные модели» engine. Constructed once per session by the tool's catalog; the
/// constructor does no I/O (the settings load runs on a worker, the key check on the first
/// `poll`).
pub(super) struct CloudEditEngine {
    /// The provider and every visited provider's model id and endpoint.
    selection: ImageEditSelection,
    /// The edit instruction.
    prompt: String,
    /// How the edit is blended back inside the mask.
    blend: MaskBlend,
    /// Channel of the initial settings load, `None` once it landed.
    settings_rx: Option<Receiver<ApiEditSettings>>,
    /// Whether the initial load landed; gates saving (see `settings_save_due`).
    settings_loaded: bool,
    /// Raised by every persisted edit; a user edit also outranks a load that lands later.
    dirty: bool,
    /// Channel of the save in flight (at most one writer).
    save_rx: Option<Receiver<()>>,
    /// The key block's state (presence, password buffer, status).
    key_state: ImageEditKeyState<ImageEditKeySlot>,
    /// Runs the key block's one credential-store operation at a time.
    key_runner: ImageEditKeyRunner<ImageEditKeySlot>,
    /// Key-block buttons clicked since the last `poll`, handed to the runner there: the panel
    /// may not be drawn every frame, the poll always runs.
    key_actions: KeyBlockActions,
    /// The selection's key slot must be resolved again (provider, region or address changed).
    key_slot_stale: bool,
    /// The last toast of the key block, drawn in the panel.
    key_notice: Option<AiApiNotice>,
    /// The run in flight.
    run: Option<RunInFlight>,
    /// The last run's outcome.
    status: Option<StatusLine>,
    /// The frame's current rectangle, for the size line and the run gate.
    region: Option<OverlayRectPx>,
}

impl CloudEditEngine {
    /// A fresh engine with default settings and the settings load armed on a worker.
    #[must_use]
    pub(super) fn new() -> Self {
        let defaults = ApiEditSettings::default();
        let mut engine = Self {
            selection: defaults.selection(),
            prompt: defaults.prompt.clone(),
            blend: defaults.blend(),
            settings_rx: None,
            settings_loaded: false,
            dirty: false,
            save_rx: None,
            key_state: ImageEditKeyState::default(),
            key_runner: ImageEditKeyRunner::default(),
            key_actions: KeyBlockActions::default(),
            key_slot_stale: true,
            key_notice: None,
            run: None,
            status: None,
            region: None,
        };
        engine.request_settings_load();
        engine
    }

    /// Reads the settings file on a worker thread.
    fn request_settings_load(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.settings_rx = Some(rx);
        thread::spawn(move || {
            // A send fails only when the engine is gone; the load then has no reader.
            let _detached = tx.send(load_api_edit_settings());
        });
    }

    /// Applies the landed settings load unless the user already edited something (the edit
    /// wins and is what the next save writes). A lost loader unblocks saving with the
    /// in-memory state.
    fn poll_settings_load(&mut self) {
        let Some(rx) = self.settings_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(settings) => {
                if !self.dirty {
                    self.apply_settings(&settings);
                }
                self.settings_loaded = true;
                self.settings_rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                ms_log::runtime_log::log_warn(format!("{LOG_TAG} the settings loader stopped without answering; keeping the defaults"));
                self.settings_loaded = true;
                self.settings_rx = None;
            }
        }
    }

    /// Replaces the live state with `settings` (the selection's key slot is then stale).
    fn apply_settings(&mut self, settings: &ApiEditSettings) {
        self.selection = settings.selection();
        self.prompt.clone_from(&settings.prompt);
        self.blend = settings.blend();
        self.key_slot_stale = true;
    }

    /// Starts a save on a worker when one is due (`settings_save_due`).
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
        let settings = ApiEditSettings::capture(&self.selection, &self.prompt, self.blend);
        let (tx, rx) = mpsc::channel();
        self.save_rx = Some(rx);
        thread::spawn(move || {
            if let Err(error) = save_api_edit_settings(&settings) {
                ms_log::runtime_log::log_warn(format!("{LOG_TAG} failed to save the tool settings: {error}"));
            }
            // A send fails only when the engine is gone; nobody waits for the save then.
            let _detached = tx.send(());
        });
    }

    /// Re-resolves the key slot when it is stale, then runs one step of the key block's
    /// background work with the buttons clicked since the last poll.
    fn poll_key_block(&mut self) {
        if self.key_slot_stale {
            self.key_slot_stale = false;
            // The last toast was about the previous slot's key; the new slot gets its own.
            self.key_notice = None;
            let slot = self.selection.endpoint_choice().and_then(|endpoint| key_slot(self.selection.provider, &endpoint));
            self.key_state.select_slot(slot);
        }
        let actions = std::mem::take(&mut self.key_actions);
        if let Some(notice) = self.key_runner.pump(&mut self.key_state, actions).pop() {
            self.key_notice = Some(notice);
        }
    }

    /// Whether the selected model takes a reference image besides the edited one.
    fn accepts_references(&self) -> bool {
        self.selection.offer().is_ok_and(|offer| offer.accepts_references())
    }

    /// The run gate's snapshot of this engine.
    fn gate(&self) -> RunGate<'_> {
        RunGate {
            run_in_flight: self.run.is_some(),
            settings_loaded: self.settings_loaded,
            web_build: cfg!(target_arch = "wasm32"),
            offer: self.selection.offer(),
            endpoint: self.selection.endpoint_choice(),
            key_required: self.selection.provider.requires_key(),
            key_configured: self.key_state.key_configured(),
            prompt: &self.prompt,
            region: self.region.map(|rect| (rect.w, rect.h)),
            constraints: self.constraints(),
        }
    }

    /// Drains the run's events; returns the poll answer.
    fn poll_run(&mut self, ctx: &egui::Context) -> EnginePoll {
        let Some(run) = self.run.as_mut() else {
            return EnginePoll::Idle;
        };
        loop {
            match run.events.try_recv() {
                Ok(WorkerEvent::Stage(stage)) => run.stage = stage,
                Ok(WorkerEvent::Finished(Ok(image))) => {
                    self.run = None;
                    self.status = Some(StatusLine { text: t!("cleaning.mask_editor.processing_done_status").to_string(), error: false });
                    return EnginePoll::Done(image);
                }
                Ok(WorkerEvent::Finished(Err(error))) => {
                    ms_log::runtime_log::log_warn(format!("{LOG_TAG} cloud edit failed: provider={} model={} error={error:?}", run.provider.key(), run.model_id));
                    self.run = None;
                    let message = tf!("cleaning.mask_editor.processing_error", err = error);
                    self.status = Some(StatusLine { text: message.clone(), error: true });
                    return EnginePoll::Failed(message);
                }
                Err(TryRecvError::Empty) => {
                    ctx.request_repaint_after(BUSY_REPAINT);
                    return EnginePoll::Running;
                }
                Err(TryRecvError::Disconnected) => {
                    self.run = None;
                    ms_log::runtime_log::log_error(format!("{LOG_TAG} the cloud edit worker stopped without answering"));
                    let message = t!("cleaning.mask_editor.processing_thread_crashed_error").to_string();
                    self.status = Some(StatusLine { text: message.clone(), error: true });
                    return EnginePoll::Failed(message);
                }
            }
        }
    }

    /// The prompt field; reports whether it changed.
    fn draw_prompt(&mut self, ui: &mut egui::Ui) -> bool {
        ui.label(t!("cleaning.tools.ai_api_editor.prompt_label"));
        ui.add(
            egui::TextEdit::multiline(&mut self.prompt)
                .id_salt(PROMPT_ID_SALT)
                .desired_rows(PROMPT_ROWS)
                .desired_width(ui.available_width())
                .hint_text(t!("cleaning.tools.ai_api_editor.prompt_hint")),
        )
        .changed()
    }

    /// The collapsible blend section; reports whether a value changed.
    fn draw_blend(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        let blend = &mut self.blend;
        egui::CollapsingHeader::new(t!("cleaning.tools.ai_api_editor.blend_heading"))
            .id_salt(BLEND_SECTION_ID_SALT)
            .default_open(false)
            .show(ui, |ui| {
                ui.small(t!("cleaning.tools.ai_api_editor.blend_hint"));
                changed |= ui.add(WheelSlider::new(&mut blend.dilate_px, 0..=BLEND_RADIUS_MAX_PX).text(t!("cleaning.tools.ai_api_editor.dilate_label"))).changed();
                changed |= ui.add(WheelSlider::new(&mut blend.feather_px, 0..=BLEND_RADIUS_MAX_PX).text(t!("cleaning.tools.ai_api_editor.feather_label"))).changed();
            });
        changed
    }

    /// The size the selected model will receive for the current frame, when it can take it.
    fn draw_size_line(&self, ui: &mut egui::Ui) {
        let Some(rect) = self.region else {
            return;
        };
        if self.selection.offer().is_err() {
            return;
        }
        // `None` = the frame is invalid for this model; the host's own panel says why.
        let Some(k) = upscale_factor_for(rect.w, rect.h, &self.constraints()) else {
            return;
        };
        let line = if k == 1 {
            tf!("cleaning.tools.ai_api_editor.size_direct_label", width = rect.w, height = rect.h)
        } else {
            let factor = usize::from(k);
            tf!(
                "cleaning.tools.ai_api_editor.size_upscaled_label",
                sent_width = rect.w.saturating_mul(factor),
                sent_height = rect.h.saturating_mul(factor),
                factor = k,
                width = rect.w,
                height = rect.h
            )
        };
        ui.small(line);
    }

    /// The run's spinner and stage, or the last outcome.
    fn draw_run_state(&self, ui: &mut egui::Ui) {
        if let Some(run) = self.run.as_ref() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.small(stage_text(run.stage));
            });
            ui.small(t!("cleaning.tools.ai_api_editor.cancel_billing_hint"));
            return;
        }
        if let Some(status) = self.status.as_ref() {
            if status.error {
                ui.colored_label(ms_theme::status::ERROR, &status.text);
            } else {
                ui.small(&status.text);
            }
        }
    }
}

/// Dropping the engine (the cleaning tab torn down without `deactivate`: an app rebuild after a
/// page op, a reload, a project switch) cancels the run in flight, so its worker stops polling,
/// the executor sends the provider's cancel request for a queued job, and nothing is
/// downloaded for a reader that is gone. No status line: nobody will draw it.
impl Drop for CloudEditEngine {
    fn drop(&mut self) {
        if let Some(run) = self.run.take() {
            run.cancel.cancel();
            ms_log::runtime_log::log_info(format!("{LOG_TAG} cloud edit cancelled: the tool was dropped mid-run (provider={} model={})", run.provider.key(), run.model_id));
        }
    }
}

/// The log name of a run's marks form (technical, not localized).
fn marks_form(marks: &RunMarks) -> &'static str {
    match marks {
        RunMarks::None => "none",
        RunMarks::Reference(_) => "reference",
        RunMarks::Layer(_) => "layer",
    }
}

/// The localized text of a pipeline stage.
fn stage_text(stage: ImageEditStage) -> String {
    match stage {
        ImageEditStage::Preparing => t!("cleaning.tools.ai_api_editor.stage_preparing_status").to_string(),
        ImageEditStage::Sending => t!("cleaning.tools.ai_api_editor.stage_sending_status").to_string(),
        ImageEditStage::Waiting { polls } => tf!("cleaning.tools.ai_api_editor.stage_waiting_status", polls = polls),
        ImageEditStage::Downloading => t!("cleaning.tools.ai_api_editor.stage_downloading_status").to_string(),
        ImageEditStage::Compositing => t!("cleaning.tools.ai_api_editor.stage_compositing_status").to_string(),
    }
}

impl AiEngine for CloudEditEngine {
    fn id(&self) -> &'static str {
        "cloud_edit"
    }

    fn title(&self) -> String {
        t!("cleaning.tools.ai_api_editor.engine_title").to_string()
    }

    /// «С промптом»: every hosted model is driven by an instruction.
    fn section(&self) -> EngineSection {
        EngineSection::WithPrompt
    }

    /// The run needs the network and a key, never the PyTorch runtime.
    fn requires_torch(&self) -> bool {
        false
    }

    /// The selected offer's size rule; nothing while no usable model is selected (the run is
    /// then refused by the gate, not by the frame). Re-read by the host every frame.
    fn constraints(&self) -> FrameConstraints {
        self.selection.offer().map_or(FrameConstraints::UNCONSTRAINED, |offer| frame_constraints(offer.rule))
    }

    /// ONE layer: where the model may change the page.
    fn mask_layers(&self) -> Vec<MaskLayerSpec> {
        vec![MaskLayerSpec { tint: MASK_TINT, label_key: "cleaning.tools.ai_api_editor.mask_layer" }]
    }

    /// Always `true`: an empty mask means "the whole region may change".
    fn allows_empty_mask(&self) -> bool {
        true
    }

    /// A model that takes a reference image besides the edited one gets the marks as that
    /// reference (preferred), as a transparent layer in the same slot, or drawn onto the region;
    /// any other model (or no model) only drawn onto the region. Re-read by the host every frame.
    fn marks_support(&self) -> MarksSupport {
        if self.accepts_references() {
            MarksSupport::new(MarksMode::SeparateReference).with(MarksMode::OverlayOnRegion).with(MarksMode::TransparentLayer)
        } else {
            MarksSupport::OVERLAY_ONLY
        }
    }

    fn draw_parameters(&mut self, ui: &mut egui::Ui) {
        ui.small(t!("cleaning.tools.ai_api_editor.mask_meaning_hint"));
        ui.separator();

        let width = ui.available_width();
        let actions = draw_image_edit_picker(ui, PICKER_ID_SALT, width, &mut self.selection, &mut self.key_state);
        let mut changed = actions.selection_changed;
        self.key_slot_stale |= actions.key_slot_changed;
        self.key_actions.refresh |= actions.key.refresh;
        self.key_actions.save_key |= actions.key.save_key;
        self.key_actions.clear_key |= actions.key.clear_key;
        if let Some(notice) = self.key_notice.as_ref() {
            ui.colored_label(notice.severity.color(), &notice.text);
        }
        if self.selection.offer().is_ok() && !self.accepts_references() {
            ui.small(t!("cleaning.tools.ai_api_editor.marks_overlay_only_hint"));
        }
        ui.separator();

        changed |= self.draw_prompt(ui);
        changed |= self.draw_blend(ui);
        self.draw_size_line(ui);
        ui.small(t!("cleaning.tools.ai_api_editor.billing_hint"));
        ui.small(t!("cleaning.tools.ai_api_editor.privacy_hint"));
        self.draw_run_state(ui);

        if changed {
            self.dirty = true;
        }
    }

    fn run_block_reason(&self) -> Option<String> {
        run_block_reason(&self.gate())
    }

    /// Starts one run on a worker thread.
    ///
    /// # Errors
    /// A localized message when a run is in flight, the request's sizes disagree (the marks'
    /// included), the gate is closed, the region fits the selected model at no allowed upscale,
    /// or separate marks arrive for a model that takes no reference (logged).
    fn start(&mut self, request: EngineRunRequest) -> Result<(), String> {
        if let Some(reason) = run_block_reason(&RunGate { region: None, ..self.gate() }) {
            return Err(reason);
        }
        let (width, height) = (request.rect_px.w, request.rect_px.h);
        let pixel_count = width.checked_mul(height).filter(|count| *count > 0);
        let [mask] = request.masks.as_slice() else {
            return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
        };
        if pixel_count.is_none() || request.region.size != [width, height] || Some(mask.len()) != pixel_count {
            return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
        }
        let marks_fit = match &request.marks {
            RunMarks::None => true,
            RunMarks::Reference(image) => image.size == [width, height],
            RunMarks::Layer(layer) => usize::try_from(layer.width()).is_ok_and(|w| w == width) && usize::try_from(layer.height()).is_ok_and(|h| h == height),
        };
        if !marks_fit {
            return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
        }
        let marks_form = marks_form(&request.marks);
        if !matches!(request.marks, RunMarks::None) && !self.accepts_references() {
            // The host switches the mode one frame after a model switch; a run started in
            // between must not silently lose the marks.
            ms_log::runtime_log::log_warn(format!("{LOG_TAG} cloud edit refused: marks as {marks_form} but provider={} model={} takes no reference image", self.selection.provider.key(), self.selection.model_id().trim()));
            return Err(tf!("cleaning.tools.ai_api_editor.marks_reference_unsupported_error", button = t!("cleaning.tools.area_editor.marks_mode_overlay_button")));
        }
        let constraints = self.constraints();
        let Some(upscale) = upscale_factor_for(width, height, &constraints) else {
            return Err(region_size_refusal(width, height, &constraints).unwrap_or_else(|| t!("cleaning.inpaint.size_mismatch_error").to_string()));
        };
        let endpoint = self.selection.endpoint_choice().map_err(|error| error.to_string())?;
        // An all-zero mask and no mask mean the same (the whole region); `None` says it plainly.
        let mask = mask.iter().any(|value| *value != 0).then(|| mask.clone());
        let job = RunJob {
            provider: self.selection.provider,
            model_id: self.selection.model_id().trim().to_string(),
            endpoint,
            prompt: self.prompt.clone(),
            region: request.region,
            mask,
            marks: request.marks,
            blend: self.blend,
            upscale,
        };
        ms_log::runtime_log::log_info(format!(
            "{LOG_TAG} cloud edit started: page={} region={}x{} provider={} model={} k={upscale} marks={marks_form}",
            request.page_idx,
            width,
            height,
            job.provider.key(),
            job.model_id
        ));
        let (provider, model_id) = (job.provider, job.model_id.clone());
        let cancel = CancelFlag::new();
        let events = spawn_run(job, cancel.clone());
        self.run = Some(RunInFlight { events, cancel, stage: ImageEditStage::Preparing, provider, model_id });
        self.status = None;
        Ok(())
    }

    fn poll(&mut self, ctx: &egui::Context) -> EnginePoll {
        self.poll_settings_load();
        self.poll_key_block();
        if self.key_runner.is_running() {
            ctx.request_repaint_after(BUSY_REPAINT);
        }
        self.poll_and_maybe_save();
        self.poll_run(ctx)
    }

    /// Raises the run's cancel flag and detaches it: the pipeline stops between steps, and an
    /// HTTP call already in flight completes on its own (and may be billed) with its answer
    /// dropped.
    fn cancel(&mut self) {
        if let Some(run) = self.run.take() {
            run.cancel.cancel();
            ms_log::runtime_log::log_info(format!("{LOG_TAG} cloud edit cancelled by the user"));
            self.status = Some(StatusLine { text: t!("cleaning.mask_editor.processing_cancelled_status").to_string(), error: false });
        }
    }

    /// Deliberately empty: the run does not use the AI backend.
    fn set_backend_available(&mut self, _available: bool) {}

    /// Deliberately empty: the run does not use PyTorch.
    fn set_torch_available(&mut self, _available: bool) {}

    /// Kept for the size line and the run gate; nothing is started off it.
    fn set_region(&mut self, region: Option<OverlayRectPx>, _geometry_settled: bool) {
        self.region = region;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::region_edit_v2::engine::RunMarks;

    /// A fresh engine: the default provider's first model is selected, so its rule is the
    /// frame's, and a run is refused until the settings have loaded.
    #[test]
    fn a_fresh_engine_follows_the_selected_model() {
        let engine = CloudEditEngine::new();
        let offer = engine.selection.offer().expect("the default provider has a default model");
        let c = engine.constraints();
        assert_eq!(c.multiple, frame_constraints(offer.rule).multiple);
        assert_eq!(c.max_upscale, offer.rule.max_upscale);
        assert!(engine.run_block_reason().is_some(), "settings not loaded yet");
    }

    /// Switching model re-derives the constraints (the host pushes them every frame); no model
    /// imposes nothing on the frame and blocks the run instead.
    #[test]
    fn the_constraints_follow_a_model_switch() {
        let mut engine = CloudEditEngine::new();
        engine.selection = ImageEditSelection::new(ImageEditProvider::Ideogram);
        assert_eq!(engine.constraints().max_upscale, 1, "Ideogram keeps the exact input size, no upscale");
        engine.selection = ImageEditSelection::new(ImageEditProvider::OpenAi);
        if let Some(choice) = engine.selection.choices.get_mut(&ImageEditProvider::OpenAi) {
            choice.model_id = "gpt-image-2".to_string();
        }
        assert_eq!(engine.constraints().min_area, Some(655_360));
        if let Some(choice) = engine.selection.choices.get_mut(&ImageEditProvider::OpenAi) {
            choice.model_id.clear();
        }
        assert_eq!(engine.constraints().min_area, None, "no model: unconstrained frame");
        engine.settings_loaded = true;
        assert!(engine.run_block_reason().is_some(), "no model: the run is refused");
    }

    /// A request whose sizes disagree is refused before any worker starts.
    #[test]
    fn start_refuses_a_malformed_request() {
        let mut engine = CloudEditEngine::new();
        engine.settings_loaded = true;
        engine.prompt = "remove the text".to_string();
        let rect = OverlayRectPx { x: 0, y: 0, w: 1024, h: 1024 };
        let short_mask = EngineRunRequest { page_idx: 0, rect_px: rect, region: egui::ColorImage::filled([1024, 1024], egui::Color32::WHITE), masks: vec![vec![0u8; 8]], marks: RunMarks::None };
        assert!(engine.start(short_mask).is_err());
        let two_layers = EngineRunRequest { page_idx: 0, rect_px: rect, region: egui::ColorImage::filled([1024, 1024], egui::Color32::WHITE), masks: vec![Vec::new(), Vec::new()], marks: RunMarks::None };
        assert!(engine.start(two_layers).is_err());
        assert!(engine.run.is_none(), "a refused start leaves no run behind");
    }

    /// The engine for `provider`'s `model_id`, loaded and with a prompt, so only the request
    /// decides whether `start` refuses.
    fn engine_for(provider: ImageEditProvider, model_id: &str) -> CloudEditEngine {
        let mut engine = CloudEditEngine::new();
        engine.settings_loaded = true;
        engine.prompt = "remove the text".to_string();
        engine.selection = ImageEditSelection::new(provider);
        if let Some(choice) = engine.selection.choices.get_mut(&provider) {
            choice.model_id = model_id.to_string();
        }
        engine
    }

    /// The marks forms follow the selected model: references -> reference preferred plus the
    /// other two; no references (or no model) -> drawn onto the region only.
    #[test]
    fn marks_support_follows_the_selected_model() {
        let with_references = engine_for(ImageEditProvider::OpenAi, "gpt-image-2").marks_support();
        assert_eq!(with_references.preferred(), MarksMode::SeparateReference);
        assert!(MarksMode::ALL.iter().all(|mode| with_references.supports(*mode)));
        let single_image = engine_for(ImageEditProvider::Xai, "grok-imagine-image-2.0").marks_support();
        assert_eq!(single_image, MarksSupport::OVERLAY_ONLY);
        let no_model = engine_for(ImageEditProvider::OpenAi, "").marks_support();
        assert_eq!(no_model, MarksSupport::OVERLAY_ONLY);
    }

    /// Separate marks for a model without references (the host's mode switch lags a model
    /// switch by a frame) and marks of another size are refused before any worker starts.
    #[test]
    fn start_refuses_marks_the_model_cannot_take() {
        let rect = OverlayRectPx { x: 0, y: 0, w: 64, h: 32 };
        let request = |marks: RunMarks| EngineRunRequest { page_idx: 0, rect_px: rect, region: egui::ColorImage::filled([64, 32], egui::Color32::WHITE), masks: vec![vec![0u8; 64 * 32]], marks };
        let mut engine = engine_for(ImageEditProvider::Xai, "grok-imagine-image-2.0");
        let refusal = tf!("cleaning.tools.ai_api_editor.marks_reference_unsupported_error", button = t!("cleaning.tools.area_editor.marks_mode_overlay_button"));
        assert_eq!(engine.start(request(RunMarks::Reference(egui::ColorImage::filled([64, 32], egui::Color32::RED)))), Err(refusal.clone()));
        assert_eq!(engine.start(request(RunMarks::Layer(image::RgbaImage::new(64, 32)))), Err(refusal));
        let mut engine = engine_for(ImageEditProvider::OpenAi, "gpt-image-2");
        assert!(engine.start(request(RunMarks::Layer(image::RgbaImage::new(32, 32)))).is_err(), "marks of another size");
        assert!(engine.start(request(RunMarks::Reference(egui::ColorImage::filled([64, 31], egui::Color32::RED)))).is_err(), "marks of another size");
        assert!(engine.run.is_none(), "a refused start leaves no run behind");
    }

    /// An edit made before the settings load lands wins over the file, and is what gets saved.
    #[test]
    fn an_edit_made_before_the_load_lands_survives_it() {
        let mut engine = CloudEditEngine::new();
        let (tx, rx) = mpsc::channel();
        engine.settings_rx = Some(rx);
        engine.settings_loaded = false;
        engine.prompt = "typed by the user".to_string();
        engine.dirty = true;
        let from_file = ApiEditSettings { prompt: "from the file".to_string(), ..ApiEditSettings::default() };
        tx.send(from_file).expect("the receiver is alive");
        engine.poll_settings_load();
        assert_eq!(engine.prompt, "typed by the user");
        assert!(engine.settings_loaded);
        // Asserted, not driven: driving the saver would spawn a worker (which writes nothing
        // in a test build, but the gate is the contract under test).
        assert!(settings_save_due(engine.dirty, engine.settings_loaded, engine.save_rx.is_some()));
    }

    /// With nothing edited the load applies, and the key slot must be resolved again.
    #[test]
    fn a_load_applies_when_nothing_was_edited() {
        let mut engine = CloudEditEngine::new();
        let (tx, rx) = mpsc::channel();
        engine.settings_rx = Some(rx);
        engine.key_slot_stale = false;
        let from_file = ApiEditSettings { provider: ImageEditProvider::Fal.key().to_string(), prompt: "from the file".to_string(), ..ApiEditSettings::default() };
        tx.send(from_file).expect("the receiver is alive");
        engine.poll_settings_load();
        assert_eq!(engine.prompt, "from the file");
        assert_eq!(engine.selection.provider, ImageEditProvider::Fal);
        assert!(engine.key_slot_stale);
    }

    /// Cancel detaches the run, raises the flag the worker checks, and says so.
    #[test]
    fn cancel_raises_the_flag_and_detaches_the_run() {
        let mut engine = CloudEditEngine::new();
        let (_tx, events) = mpsc::channel();
        let cancel = CancelFlag::new();
        engine.run = Some(RunInFlight { events, cancel: cancel.clone(), stage: ImageEditStage::Sending, provider: ImageEditProvider::Fal, model_id: "m".to_string() });
        engine.cancel();
        assert!(cancel.is_cancelled());
        assert!(engine.run.is_none());
        assert!(engine.status.is_some());
    }

    /// Dropping the engine mid-run (no `deactivate`) raises the flag the worker and the
    /// executor check, so the run stops and the provider job is cancelled.
    #[test]
    fn dropping_the_engine_cancels_the_run_in_flight() {
        let mut engine = CloudEditEngine::new();
        let (_tx, events) = mpsc::channel();
        let cancel = CancelFlag::new();
        engine.run = Some(RunInFlight { events, cancel: cancel.clone(), stage: ImageEditStage::Waiting { polls: 2 }, provider: ImageEditProvider::Fal, model_id: "m".to_string() });
        drop(engine);
        assert!(cancel.is_cancelled());
    }

    /// A failure is logged and reported for the run's own provider and model, whatever the
    /// picker shows by then; the run is cleared.
    #[test]
    fn a_failed_run_is_attributed_to_its_own_snapshot() {
        let mut engine = CloudEditEngine::new();
        let (tx, events) = mpsc::channel();
        engine.run = Some(RunInFlight { events, cancel: CancelFlag::new(), stage: ImageEditStage::Sending, provider: ImageEditProvider::Fal, model_id: "fal-model".to_string() });
        engine.selection = ImageEditSelection::new(ImageEditProvider::OpenAi);
        tx.send(WorkerEvent::Finished(Err(ms_ai_api::image_edit::ImageEditError::Cancelled))).expect("the receiver is alive");
        assert!(matches!(engine.poll_run(&egui::Context::default()), EnginePoll::Failed(_)));
        assert!(engine.run.is_none());
    }

    /// Every stage has its own line.
    #[test]
    fn every_stage_has_a_line() {
        let _locale_guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().expect("locale lock");
        let en = ms_i18n::LocaleTag::parse("en").expect("en tag is valid");
        ms_i18n::set_locale(&en).expect("en catalog installs");
        let lines = [
            stage_text(ImageEditStage::Preparing),
            stage_text(ImageEditStage::Sending),
            stage_text(ImageEditStage::Waiting { polls: 3 }),
            stage_text(ImageEditStage::Downloading),
            stage_text(ImageEditStage::Compositing),
        ];
        for (idx, line) in lines.iter().enumerate() {
            assert!(!line.starts_with("cleaning."), "{line} has no catalog entry");
            assert!(lines[idx + 1..].iter().all(|other| other != line), "{line} is used twice");
        }
        assert!(lines[2].contains('3'));
    }
}
