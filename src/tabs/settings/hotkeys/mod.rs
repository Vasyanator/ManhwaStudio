/*
File: src/tabs/settings/hotkeys/mod.rs

Purpose:
The Settings › Hotkeys pane over the unified input model (`ms_input`): every declared action
grouped by scope, each with any number of binding alternatives (rebind / remove / add / reset),
binding capture, axis and gesture editors, conflict and shadow display, a search field, and
off-thread persistence of every change into `user_config` -> `Hotkeys`.

Key structures:
- `HotkeysPane`: the pane state owned by `SettingsTabState` (search text, capture chip choice,
  error lines, jump / highlight target, the override writer).
- `Intent`: what a click in the read-only drawing pass asks for; applied afterwards with the
  router borrowed mutably.

Key functions:
- `HotkeysPane::new`, `HotkeysPane::draw`, `HotkeysPane::cancel_capture`.
- `SettingsTabState::draw_hotkeys`: the settings dispatch entry (draws the `hotkeys` field).

Submodules:
- `model`: pure grouping / ordering / chip / conflict-row / search rules (tested in `tests`).
- `rows`: the grouped action list and the per-alternative editors.
- `capture`: the capture panel (live chord, mouse zone, modifier chips, Done / Cancel).
- `conflicts`: the "All conflicts" summary.
- `writer`: the ordered persistence worker.

Notes:
- The router owns the registry; every edit goes through `registry_mut()` (copy-on-write, takes
  effect from the next frame) and its `OverrideEntry` is queued on the writer. No file I/O on
  the GUI thread.
- A capture suppresses every binding and eats keys until it ends; the pane cancels a capture
  whose panel was not drawn on the previous frame (the pane was hidden in between).
*/

mod capture;
mod conflicts;
mod model;
mod rows;
mod writer;

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::path::PathBuf;

use ms_input::binding::MAX_ALTERNATIVES;
use ms_input::{ActionId, Binding, BindingError, CaptureError, CaptureSlot, InputRouter};
use web_time::Instant;

use crate::runtime_log;
use writer::{OverrideWriter, WriteFailure, WriteFailureKind};

/// At most this many error lines are kept; older ones are dropped first.
const MAX_ERROR_LINES: usize = 8;

/// The "use as modifier" choice of the running capture.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChipState {
    action: ActionId,
    slot: CaptureSlot,
    selected: BTreeSet<ActionId>,
}

/// A change requested while drawing (the drawing pass only reads the registry).
#[derive(Debug, Clone, PartialEq)]
enum Intent {
    BeginCapture { action: ActionId, slot: CaptureSlot },
    Remove { action: ActionId, index: usize },
    Reset { action: ActionId },
    Replace { action: ActionId, index: usize, binding: Binding },
    Append { action: ActionId, binding: Binding },
    Jump { action: ActionId },
}

/// State of the Settings › Hotkeys pane.
#[derive(Debug)]
pub(super) struct HotkeysPane {
    writer: OverrideWriter,
    search: String,
    chips: Option<ChipState>,
    errors: Vec<String>,
    /// Action to scroll to on the next draw (set by the conflict summary).
    jump: Option<ActionId>,
    /// Action framed after a jump, until the instant.
    highlight: Option<(ActionId, Instant)>,
    /// `cumulative_frame_nr` of the last draw, to notice a pane that was hidden.
    last_drawn_frame: Option<u64>,
}

impl HotkeysPane {
    /// A pane persisting into the `user_config` document at `user_settings_file`.
    pub(super) fn new(user_settings_file: PathBuf) -> Self {
        Self { writer: OverrideWriter::new(user_settings_file), search: String::new(), chips: None, errors: Vec::new(), jump: None, highlight: None, last_drawn_frame: None }
    }

    /// Ends a running capture without changing anything. The owner calls it when the pane stops
    /// being drawn, so a forgotten capture does not keep every binding suppressed.
    pub(super) fn cancel_capture(&mut self, router: &mut InputRouter) {
        if router.capture_status().is_some() {
            router.capture_cancel();
        }
        self.chips = None;
    }

    /// Draws the pane for this frame and applies what the user did.
    pub(super) fn draw(&mut self, ui: &mut egui::Ui, router: &mut InputRouter) {
        let ctx = ui.ctx().clone();
        let frame_nr = ctx.cumulative_frame_nr();
        if self.last_drawn_frame.is_some_and(|last| frame_nr > last.saturating_add(1)) && router.capture_status().is_some() {
            runtime_log::log_info("[settings::hotkeys] binding capture cancelled: the pane was hidden while recording");
            self.cancel_capture(router);
        }
        self.last_drawn_frame = Some(frame_nr);
        for failure in self.writer.drain_failures() {
            let text = write_failure_text(&router.registry().title(failure.action), failure);
            self.push_error(text);
        }

        ui.heading(t!("settings.hotkeys.heading"));
        self.draw_errors(ui);
        capture::draw_capture_panel(self, ui, router);
        ui.add(egui::TextEdit::singleline(&mut self.search).id_salt("settings.hotkeys.search").hint_text(t!("settings.hotkeys.search_hint")).desired_width(ui.available_width().min(420.0)));
        ui.add_space(4.0);

        let intents = {
            let registry = router.registry();
            let capturing = router.capture_status().map(|status| status.action);
            let mut intents = Vec::new();
            conflicts::draw_summary(ui, registry, &mut intents);
            ui.add_space(4.0);
            egui::ScrollArea::vertical().id_salt("settings.hotkeys.scroll").auto_shrink([false, false]).show(ui, |ui| {
                rows::draw_groups(self, ui, registry, capturing, &mut intents);
            });
            intents
        };
        for intent in intents {
            self.apply(&ctx, router, intent);
        }
    }

    fn draw_errors(&mut self, ui: &mut egui::Ui) {
        if self.errors.is_empty() {
            return;
        }
        for line in &self.errors {
            ui.colored_label(ms_theme::status::ERROR, line);
        }
        if ui.small_button(t!("settings.hotkeys.dismiss_button")).clicked() {
            self.errors.clear();
        }
        ui.add_space(4.0);
    }

    /// Adds a user-facing error line (the caller has logged the technical detail).
    fn push_error(&mut self, line: String) {
        self.errors.push(line);
        let excess = self.errors.len().saturating_sub(MAX_ERROR_LINES);
        self.errors.drain(..excess);
    }

    fn apply(&mut self, ctx: &egui::Context, router: &mut InputRouter, intent: Intent) {
        match intent {
            Intent::BeginCapture { action, slot } => {
                let selected = model::template_modes(router.registry(), action, slot);
                self.chips = Some(ChipState { action, slot, selected });
                router.capture_begin(action, slot);
                ctx.request_repaint();
            }
            Intent::Remove { action, index } => {
                let mut alternatives = router.registry().bindings(action).to_vec();
                if index < alternatives.len() {
                    alternatives.remove(index);
                    self.commit(ctx, router, action, alternatives);
                }
            }
            Intent::Reset { action } => match router.registry_mut().reset(action) {
                Ok(entry) => self.writer.submit(ctx, entry),
                Err(err) => self.binding_failed(action, &err),
            },
            Intent::Replace { action, index, binding } => {
                let mut alternatives = router.registry().bindings(action).to_vec();
                if let Some(slot) = alternatives.get_mut(index) {
                    *slot = binding;
                    self.commit(ctx, router, action, alternatives);
                }
            }
            Intent::Append { action, binding } => {
                let mut alternatives = router.registry().bindings(action).to_vec();
                alternatives.push(binding);
                self.commit(ctx, router, action, alternatives);
            }
            Intent::Jump { action } => {
                // A jump target hidden by the search would never be drawn: clear the search.
                self.search.clear();
                self.jump = Some(action);
                ctx.request_repaint();
            }
        }
    }

    /// Applies `alternatives` to `action` and queues the persisted consequence.
    fn commit(&mut self, ctx: &egui::Context, router: &mut InputRouter, action: ActionId, alternatives: Vec<Binding>) {
        match router.registry_mut().set_bindings(action, alternatives) {
            Ok(entry) => self.writer.submit(ctx, entry),
            Err(err) => self.binding_failed(action, &err),
        }
    }

    fn binding_failed(&mut self, action: ActionId, err: &BindingError) {
        runtime_log::log_warn(format!("[settings::hotkeys] binding change rejected; action={}; cause={err}", action.0));
        self.push_error(binding_error_text(err));
    }

    fn capture_failed(&mut self, action: ActionId, err: &CaptureError) {
        runtime_log::log_warn(format!("[settings::hotkeys] captured binding not applied; action={}; cause={err}", action.0));
        self.push_error(capture_error_text(err));
    }
}

impl super::SettingsTabState {
    /// Draws the Hotkeys section (the settings tab's dispatch entry for it).
    pub(super) fn draw_hotkeys(&mut self, ui: &mut egui::Ui, router: &mut InputRouter) {
        self.hotkeys.draw(ui, router);
    }
}

/// The user-facing text of a rejected binding change.
fn binding_error_text(err: &BindingError) -> String {
    match err {
        BindingError::TooManyAlternatives { .. } => tf!("settings.hotkeys.too_many_alternatives_error", max = MAX_ALTERNATIVES),
        BindingError::TooManyCombinations { .. } => t!("settings.hotkeys.too_many_combinations_error").to_owned(),
        BindingError::TooManyModeRefs { .. } => t!("settings.hotkeys.too_many_modifiers_error").to_owned(),
        BindingError::EmptySequence { .. } | BindingError::EmptyChord { .. } => t!("settings.hotkeys.empty_binding_error").to_owned(),
        BindingError::InvalidGestureButton { .. } => t!("settings.hotkeys.gesture_button_error").to_owned(),
        BindingError::UnknownAction(_)
        | BindingError::FixedAction(_)
        | BindingError::KindMismatch { .. }
        | BindingError::UnknownModeRef { .. }
        | BindingError::RefNotAMode { .. }
        | BindingError::InvalidSensitivity { .. } => t!("settings.hotkeys.apply_failed_error").to_owned(),
    }
}

/// The user-facing text of a capture that could not become a binding.
fn capture_error_text(err: &CaptureError) -> String {
    match err {
        CaptureError::Empty(_) => t!("settings.hotkeys.capture_empty_error").to_owned(),
        CaptureError::SequenceNotAllowed(_) => t!("settings.hotkeys.sequence_not_allowed_error").to_owned(),
        CaptureError::PointerButtonCount { .. } => t!("settings.hotkeys.gesture_button_error").to_owned(),
        CaptureError::NotCapturing | CaptureError::UnknownAction(_) | CaptureError::FixedAction(_) | CaptureError::SlotOutOfRange { .. } | CaptureError::NoTemplate(_) => {
            t!("settings.hotkeys.apply_failed_error").to_owned()
        }
    }
}

/// The user-facing text of a failed write of `action`'s override.
fn write_failure_text(action_title: &str, failure: WriteFailure) -> String {
    match failure.kind {
        WriteFailureKind::NewerFormat => tf!("settings.hotkeys.save_newer_format_error", action = action_title),
        WriteFailureKind::Io => tf!("settings.hotkeys.save_failed_error", action = action_title),
        WriteFailureKind::Spawn => tf!("settings.hotkeys.writer_spawn_error", action = action_title),
    }
}
