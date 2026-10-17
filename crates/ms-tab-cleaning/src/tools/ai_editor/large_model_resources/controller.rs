/*
File: ai_editor/large_model_resources/controller.rs

Purpose:
The engine-side state behind the «Память и скорость» widget of a large-model engine: the
`large_models.profiles` forecast query and its one-shot arming, the «Пределы» caps buffer and
its debounced writer, and the learned-floor reset worker — plus the fold of the widget's
`ResourcesAction`s back into that state and into the engine's step pair. Every large-model
engine owns one `ResourcesController` (FLUX.2 klein, FLUX.3 klein).

Key structures:
- `ResourcesController`

Key functions:
- `ResourcesController::{arm, invalidate, poll_side_jobs, poll_query, view, fold,
  start_learned_reset, wants_repaint}`

Notes:
Nothing here blocks the GUI thread: the query, the cap write and the reset each run on their
own worker (`ProfilesQuery`, `LimitsWriter`, `ms_thread`), and every method only drains a
channel or arms a flag. The forecast QUESTION is the engine's (`poll_query` takes a builder),
so no model name or parameter appears here except the adapter name the reset falls back to.
*/

use super::{ResourcesAction, ResourcesView, cap_changes_for};
use ms_local_ai::large_models::{self as large_models, LargeModelsError, Limits, LimitsWriter, ProfilesQuery, ProfilesRequest, StepChoice};
use ms_thread as thread;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use web_time::Instant;

/// The forecast, caps and learned-floor state of one large-model engine.
pub(in crate::tools::ai_editor) struct ResourcesController {
    /// The `large_models.profiles` forecast: at most one request in flight, latest wins.
    pub(in crate::tools::ai_editor) profiles: ProfilesQuery,
    /// Arms exactly ONE forecast question for the next poll that may ask it: re-armed
    /// whenever a parameter changed and, for the geometry, only by a SETTLED region size
    /// change, so the forecast follows the controls without flooding the IPC.
    pub(in crate::tools::ai_editor) wanted: bool,
    /// The two pool caps as the «Пределы» editor shows them: seeded from the last forecast's
    /// `limits` while no edit is pending, written through `limits_writer`.
    pub(in crate::tools::ai_editor) limits_edit: Limits,
    /// Debounced latest-wins `large_models.limits.set`.
    pub(in crate::tools::ai_editor) limits_writer: LimitsWriter,
    /// The last cap write's failure, shown under the editor.
    pub(in crate::tools::ai_editor) limits_error: Option<String>,
    /// A `large_models.learned.reset` of this engine's floor in flight.
    pub(in crate::tools::ai_editor) learned_reset_rx: Option<Receiver<Result<u32, LargeModelsError>>>,
    /// The last reset's failure, shown beside the floor line.
    pub(in crate::tools::ai_editor) learned_reset_error: Option<String>,
    /// The adapter name (`large_models.profiles` `model`) a reset falls back to when the
    /// held answer names no floor.
    model: &'static str,
    /// The engine's name in log lines (`"FLUX.2 klein"`).
    log_label: &'static str,
}

impl ResourcesController {
    /// A controller with nothing asked, nothing armed and default caps.
    pub(in crate::tools::ai_editor) fn new(model: &'static str, log_label: &'static str) -> Self {
        Self {
            profiles: ProfilesQuery::new(),
            wanted: false,
            limits_edit: Limits::default(),
            limits_writer: LimitsWriter::new(),
            limits_error: None,
            learned_reset_rx: None,
            learned_reset_error: None,
            model,
            log_label,
        }
    }

    /// Arms one forecast question: the question changed (a parameter, a settled resize).
    pub(in crate::tools::ai_editor) fn arm(&mut self) {
        self.wanted = true;
    }

    /// Marks the held forecast stale and re-arms it: the MACHINE changed under it (a run
    /// finished, weights moved, the caps or the learned floor changed, the backend came back).
    pub(in crate::tools::ai_editor) fn invalidate(&mut self) {
        self.profiles.invalidate();
        self.wanted = true;
    }

    /// Drains the learned-floor reset and the cap writer. A landed reset or cap write
    /// invalidates the forecast (every step's fit depends on both); a failure is logged and
    /// shown beside its control. A landed write re-seeds the editor unless another edit is
    /// already pending.
    pub(in crate::tools::ai_editor) fn poll_side_jobs(&mut self, now: Instant) {
        self.poll_learned_reset();
        match self.limits_writer.poll(now) {
            Some(Ok(limits)) => {
                self.limits_error = None;
                if !self.limits_writer.is_busy() {
                    self.limits_edit = limits;
                }
                self.invalidate();
            }
            Some(Err(err)) => {
                ms_log::runtime_log::log_warn(format!("[cleaning] {}: large-model limits not saved: {err}", self.log_label));
                self.limits_error = Some(tf!("large_models.limits.save_error", err = err.user_message()));
            }
            None => {}
        }
    }

    /// Drains the forecast query and asks the next question when one is armed.
    ///
    /// `region` is the frame rectangle's size; without a frame there is nothing to forecast
    /// and the arming is kept for the next frame that has one. `gate_open` is the engine's
    /// own condition (the backend is reachable and no long operation holds it); while it is
    /// closed the arming is kept too. `build` turns the region size into the engine's
    /// question. A landed answer also seeds the «Пределы» editor with the caps in force while
    /// no edit of the user's is pending or being written.
    pub(in crate::tools::ai_editor) fn poll_query(
        &mut self,
        region: Option<[usize; 2]>,
        gate_open: bool,
        build: impl FnOnce([usize; 2]) -> ProfilesRequest,
    ) {
        if self.profiles.poll()
            && !self.limits_writer.is_busy()
            && let Some(profiles) = self.profiles.latest()
        {
            self.limits_edit = profiles.limits.caps;
        }
        let Some(size) = region else {
            return;
        };
        if !self.wanted || !gate_open {
            return;
        }
        self.wanted = false;
        self.profiles.request(build(size));
    }

    /// Whether a worker of this controller is waiting with no input event to wake the GUI
    /// (a cap edit's quiet time, a query or a reset in flight).
    pub(in crate::tools::ai_editor) fn wants_repaint(&self) -> bool {
        self.limits_writer.is_busy() || self.profiles.is_busy() || self.learned_reset_rx.is_some()
    }

    /// What the widget reads for one frame; `connected` is whether the AI backend is
    /// reachable (the caps can be edited only then).
    pub(in crate::tools::ai_editor) fn view(&mut self, connected: bool) -> ResourcesView<'_> {
        ResourcesView {
            profiles: self.profiles.latest(),
            stale: self.profiles.is_stale(),
            busy: self.profiles.is_busy(),
            error: self.profiles.error(),
            connected,
            limits_edit: &mut self.limits_edit,
            limits_error: self.limits_error.as_deref(),
            learned_reset_busy: self.learned_reset_rx.is_some(),
            learned_reset_error: self.learned_reset_error.as_deref(),
        }
    }

    /// Folds the widget's actions of one frame back. Returns `true` when the engine's
    /// SETTINGS changed (a save is owed and the forecast must be re-armed by the caller's
    /// usual "settings changed" path).
    ///
    /// `choice` is the engine's persisted step pair (a suggestion is written into it); `format`
    /// is its weight-format choice, `None` for a model with one format — a format action then
    /// is logged and ignored rather than dropped silently. A cap edit is sent for ITS axis
    /// only ([`cap_changes_for`]); «Забыть» starts the reset.
    pub(in crate::tools::ai_editor) fn fold(
        &mut self,
        actions: Vec<ResourcesAction>,
        choice: &mut StepChoice,
        mut format: Option<&mut String>,
    ) -> bool {
        let mut settings_changed = false;
        for action in actions {
            match action {
                // The widget already wrote the pair (or the flag) into what it was handed.
                ResourcesAction::StepsChanged => settings_changed = true,
                ResourcesAction::ApplySuggestion(suggested) => {
                    if *choice != suggested {
                        *choice = suggested;
                        settings_changed = true;
                    }
                }
                ResourcesAction::ApplyFormat(picked) => match format.as_deref_mut() {
                    Some(format) => {
                        if *format != picked {
                            *format = picked;
                        }
                        settings_changed = true;
                    }
                    None => ms_log::runtime_log::log_warn(format!(
                        "[cleaning] {}: ignored a weight-format choice ({picked}); this engine has one format",
                        self.log_label
                    )),
                },
                ResourcesAction::LimitsEdited(axis) => {
                    let (vram, ram) = cap_changes_for(axis, self.limits_edit);
                    self.limits_writer.edit(vram, ram, Instant::now());
                }
                ResourcesAction::ResetLearned => self.start_learned_reset(),
                // The widget already wrote the flag into the engine's settings.
                ResourcesAction::EncoderOnCpuChanged => settings_changed = true,
            }
        }
        settings_changed
    }

    /// Starts forgetting this model's learned floor on a worker, at most one at a time. The
    /// floor's own model name (as the held answer reports it) wins over the adapter name.
    pub(in crate::tools::ai_editor) fn start_learned_reset(&mut self) {
        if self.learned_reset_rx.is_some() {
            return;
        }
        let model = self
            .profiles
            .latest()
            .and_then(|profiles| profiles.learned_floor.as_ref().map(|floor| floor.model.clone()))
            .unwrap_or_else(|| self.model.to_string());
        let (tx, rx) = mpsc::channel();
        self.learned_reset_rx = Some(rx);
        self.learned_reset_error = None;
        thread::spawn(move || {
            // A failed send means the engine was dropped meanwhile: nobody waits.
            let _ = tx.send(large_models::reset_learned(Some(&model)));
        });
    }

    /// Drains the learned-floor reset: success invalidates the forecast (the floor is gone).
    fn poll_learned_reset(&mut self) {
        let Some(rx) = self.learned_reset_rx.as_ref() else {
            return;
        };
        let outcome = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(LargeModelsError::WorkerLost),
        };
        self.learned_reset_rx = None;
        match outcome {
            Ok(_) => self.invalidate(),
            Err(err) => {
                ms_log::runtime_log::log_warn(format!("[cleaning] {}: learned limits not reset: {err}", self.log_label));
                self.learned_reset_error = Some(tf!("ai_backend.large_models.action_error", err = err.user_message()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ms_local_ai::large_models::{Axis, RamStep, VramStep};

    /// A controller whose cap writer never reaches a backend: a pending edit is SENT when the
    /// writer is dropped, and a test must not write the caps of whatever backend is running.
    fn controller() -> ResourcesController {
        let mut ctl = ResourcesController::new("test_model", "test");
        ctl.limits_writer = LimitsWriter::with_setter(std::sync::Arc::new(|_, _| Ok(Limits::default())));
        ctl
    }

    fn request(size: [usize; 2]) -> ProfilesRequest {
        ProfilesRequest {
            model: "test_model".to_string(),
            params: serde_json::json!({}),
            region: [u32::try_from(size[0]).unwrap_or(u32::MAX), u32::try_from(size[1]).unwrap_or(u32::MAX)],
            steps: None,
            format: None,
            choice: StepChoice::default(),
            references: Vec::new(),
        }
    }

    /// An armed question waits for a frame and for the engine's gate, and is asked once.
    #[test]
    fn an_armed_question_is_kept_until_a_frame_and_an_open_gate_then_asked_once() {
        let mut ctl = controller();
        ctl.arm();
        let mut built = 0;
        ctl.poll_query(None, true, |size| {
            built += 1;
            request(size)
        });
        assert!(ctl.wanted && built == 0, "no frame: kept");
        ctl.poll_query(Some([64, 64]), false, |size| {
            built += 1;
            request(size)
        });
        assert!(ctl.wanted && built == 0, "a closed gate: kept");
        ctl.wanted = false;
        ctl.poll_query(Some([64, 64]), true, |size| {
            built += 1;
            request(size)
        });
        assert_eq!(built, 0, "nothing armed: nothing asked");
    }

    /// `invalidate` re-arms (the machine changed); `arm` does too (the question changed).
    #[test]
    fn invalidating_rearms_the_question() {
        let mut ctl = controller();
        assert!(!ctl.wanted);
        ctl.invalidate();
        assert!(ctl.wanted);
    }

    /// The fold: a step change and a suggestion owe a save; an equal suggestion does not; a
    /// format is written where the engine has formats and ignored where it has one.
    #[test]
    fn the_fold_writes_the_choice_and_reports_a_settings_change() {
        let mut ctl = controller();
        let mut choice = StepChoice::default();
        assert!(ctl.fold(vec![ResourcesAction::StepsChanged], &mut choice, None));
        let half = StepChoice { vram: VramStep::Half, ram: RamStep::Run };
        assert!(ctl.fold(vec![ResourcesAction::ApplySuggestion(half)], &mut choice, None));
        assert_eq!(choice, half);
        assert!(!ctl.fold(vec![ResourcesAction::ApplySuggestion(half)], &mut choice, None), "the same pair owes nothing");
        assert!(!ctl.fold(vec![ResourcesAction::ApplyFormat("fp8r".to_string())], &mut choice, None), "one format: ignored");
        let mut format = "bf16".to_string();
        assert!(ctl.fold(vec![ResourcesAction::ApplyFormat("fp8r".to_string())], &mut choice, Some(&mut format)));
        assert_eq!(format, "fp8r");
        assert!(ctl.fold(vec![ResourcesAction::EncoderOnCpuChanged], &mut choice, None), "the encoder flag is a setting");
        assert!(!ctl.fold(vec![ResourcesAction::LimitsEdited(Axis::Vram)], &mut choice, None), "a cap is not a setting");
        assert!(ctl.limits_writer.is_busy(), "the cap edit waits for its quiet time");
    }
}
