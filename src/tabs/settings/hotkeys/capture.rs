/*
File: src/tabs/settings/hotkeys/capture.rs

Purpose:
The capture panel of the hotkeys pane (plan P6): shown above the action list while the router
records a binding. It displays the recorded chords and the chord being held, marks the mouse
capture zone, offers the "use as modifier" chips and ends the capture with Done / Cancel.

Key functions:
- `draw_capture_panel`: draws the panel and finishes / cancels the capture.

Notes:
- Mouse buttons are recorded only inside the zone rect handed to `capture_set_mouse_zone`, so
  the clicks on Done / Cancel (outside it) are never recorded. Esc is recorded like any key;
  the capture is cancelled only by the Cancel button.
- A command capture can hold several chords (a key sequence) and ends with Done; a mode, axis or
  gesture capture takes one chord and is applied as soon as it is recorded.
*/

use std::collections::BTreeSet;

use ms_input::{ActionId, ActionKind, Binding, CaptureSlot, Chord, InputRegistry, InputRouter};

use super::{ChipState, HotkeysPane, model};

/// Height of the mouse capture zone, in points.
const MOUSE_ZONE_HEIGHT: f32 = 44.0;
/// Maximum width of the mouse capture zone, in points.
const MOUSE_ZONE_MAX_WIDTH: f32 = 420.0;

/// Draws the capture panel when a capture runs; applies or cancels it on the user's request.
pub(super) fn draw_capture_panel(pane: &mut HotkeysPane, ui: &mut egui::Ui, router: &mut InputRouter) {
    let Some(status) = router.capture_status().cloned() else {
        pane.chips = None;
        return;
    };
    let action = status.action;
    if pane.chips.as_ref().is_none_or(|chips| chips.action != action || chips.slot != status.slot) {
        // The capture was started elsewhere (or restarted): seed the chips from its template.
        pane.chips = Some(ChipState { action, slot: status.slot, selected: model::template_modes(router.registry(), action, status.slot) });
    }
    let is_command = router.registry().kind(action.0) == Some(ActionKind::Command);
    if !is_command && !status.chords.is_empty() {
        finish(pane, ui.ctx(), router, action, status.slot);
        return;
    }

    let mut done = false;
    let mut cancel = false;
    let mut zone = None;
    {
        let registry = router.registry();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.strong(tf!("settings.hotkeys.capture_title", action = registry.title(action)));
            if is_command {
                ui.label(tf!("settings.hotkeys.capture_hint_command", button = t!("settings.hotkeys.done_button")));
            } else {
                ui.label(t!("settings.hotkeys.capture_hint_single"));
            }
            if status.chords.is_empty() {
                ui.weak(t!("settings.hotkeys.capture_waiting_status"));
            } else {
                let recorded = Binding::sequence(status.chords.iter().map(|buttons| Chord::new(buttons.iter().cloned())).collect());
                ui.monospace(tf!("settings.hotkeys.capture_recorded_status", binding = registry.alternative_label(&recorded)));
            }
            if !status.pending.is_empty() {
                ui.monospace(tf!("settings.hotkeys.capture_forming_status", buttons = registry.chord_label(&Chord::new(status.pending.iter().cloned()))));
            }
            zone = Some(draw_mouse_zone(ui));
            if let Some(chips) = &mut pane.chips {
                draw_chips(ui, registry, chips);
            }
            ui.horizontal(|ui| {
                if is_command && ui.add_enabled(!status.chords.is_empty(), egui::Button::new(t!("settings.hotkeys.done_button"))).clicked() {
                    done = true;
                }
                if ui.button(t!("settings.hotkeys.cancel_button")).clicked() {
                    cancel = true;
                }
            });
        });
    }
    router.capture_set_mouse_zone(zone);
    if done {
        finish(pane, ui.ctx(), router, action, status.slot);
    } else if cancel {
        pane.cancel_capture(router);
    }
    ui.add_space(6.0);
}

/// Allocates and paints the framed area in which mouse buttons are recorded; returns its rect.
fn draw_mouse_zone(ui: &mut egui::Ui) -> egui::Rect {
    let width = ui.available_width().min(MOUSE_ZONE_MAX_WIDTH);
    // `click` sense: a press inside the zone must not fall through to a widget below.
    let (rect, _response) = ui.allocate_exact_size(egui::vec2(width, MOUSE_ZONE_HEIGHT), egui::Sense::click());
    let visuals = ui.visuals();
    let stroke = visuals.widgets.active.bg_stroke;
    let text_color = visuals.weak_text_color();
    let painter = ui.painter();
    painter.rect_stroke(rect, 4.0, stroke, egui::StrokeKind::Inside);
    painter.text(rect.center(), egui::Align2::CENTER_CENTER, t!("settings.hotkeys.capture_mouse_zone_hint"), egui::TextStyle::Small.resolve(ui.style()), text_color);
    rect
}

/// The "use as modifier" chips: the offered modes plus whatever is selected, in declaration
/// order of the offer, selected-but-not-offered last.
fn draw_chips(ui: &mut egui::Ui, registry: &InputRegistry, chips: &mut ChipState) {
    let mut modes = model::offered_modes(registry, chips.action);
    modes.extend(chips.selected.iter().copied().filter(|mode| !modes.contains(mode)).collect::<Vec<_>>());
    if modes.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(t!("settings.hotkeys.capture_modifiers_label"));
        for mode in modes {
            let text = tf!("settings.hotkeys.capture_modifier_chip", mode = registry.title(mode), binding = registry.label(mode));
            if ui.selectable_label(chips.selected.contains(&mode), text).on_hover_text(t!("settings.hotkeys.capture_modifier_tooltip")).clicked() && !chips.selected.remove(&mode) {
                chips.selected.insert(mode);
            }
        }
    });
}

/// Ends the capture and applies its alternative list with the chip choice.
fn finish(pane: &mut HotkeysPane, ctx: &egui::Context, router: &mut InputRouter, action: ActionId, slot: CaptureSlot) {
    let selected: BTreeSet<ActionId> = pane.chips.take().filter(|chips| chips.action == action).map(|chips| chips.selected).unwrap_or_default();
    match router.capture_finish() {
        Ok(mut alternatives) => {
            let index = match slot {
                CaptureSlot::Replace(index) => Some(index),
                CaptureSlot::Append => alternatives.len().checked_sub(1),
            };
            if let Some(binding) = index.and_then(|index| alternatives.get_mut(index)) {
                let registry = router.registry();
                model::apply_modifier_choice(binding, &selected, &|mode| registry.mode_button_sets(mode));
            }
            pane.commit(ctx, router, action, alternatives);
        }
        Err(err) => pane.capture_failed(action, &err),
    }
    ctx.request_repaint();
}
