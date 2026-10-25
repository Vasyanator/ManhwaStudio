/*
File: crates/ms-widgets/src/armed_delete_button.rs

Purpose:
The two-click "armed" delete button: the first plain click ARMS it (the caption switches to a
confirm wording and the fill turns strongly red), a second plain click on the armed button
CONFIRMS. A physical double click never confirms, and an armed button the pointer leaves
disarms itself.

Key structures:
- `ArmedDeleteButton`: the per-frame builder (both captions, an optional fill at rest, enabled).
- `ArmedDeleteStep`: this frame's transition, which the caller applies to the arm it owns.

Key functions:
- `ArmedDeleteButton::show()`: draws the button and decides the step.
- `armed_delete_step()`: the pure decision behind it.

Notes:
- DRAW ONLY, stateless: the caller owns WHICH thing is armed (one flag, or the key of one row
  among many) and applies the returned step; the widget never deletes anything.
- Call sites: `PresetPicker`'s «Delete», Settings › Тайп font groups (`font_groups.rs`) and
  custom fonts (`hybrid_fonts.rs`).
*/

use eframe::egui;
use egui::{Color32, Response, Ui, WidgetText};

/// One frame's transition of an [`ArmedDeleteButton`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmedDeleteStep {
    /// The armed button got a plain click: delete now (the caller also disarms).
    Confirm,
    /// The idle button got a plain click: arm it.
    Arm,
    /// The armed button lost the pointer: disarm, so a later unrelated click cannot delete.
    Disarm,
    /// Nothing changes.
    Keep,
}

/// Decides [`ArmedDeleteStep`] from the effective arm and this frame's response flags. A click
/// that is also a double click never counts: a double-click delivers a press on two consecutive
/// frames and must not arm-and-confirm in one gesture.
#[must_use]
pub fn armed_delete_step(armed: bool, clicked: bool, double_clicked: bool, hovered: bool) -> ArmedDeleteStep {
    if clicked && !double_clicked {
        if armed { ArmedDeleteStep::Confirm } else { ArmedDeleteStep::Arm }
    } else if armed && !hovered {
        ArmedDeleteStep::Disarm
    } else {
        ArmedDeleteStep::Keep
    }
}

/// The two-click delete button, built per frame. See the file header.
#[must_use = "an ArmedDeleteButton draws nothing until `show` is called"]
pub struct ArmedDeleteButton {
    idle_caption: WidgetText,
    armed_caption: WidgetText,
    idle_fill: Option<Color32>,
    enabled: bool,
}

impl ArmedDeleteButton {
    /// A button reading `idle_caption` at rest and `armed_caption` once armed (both already
    /// localized). The armed state is filled with `ms_theme::status::DESTRUCTIVE_ARMED_FILL`.
    pub fn new(idle_caption: impl Into<WidgetText>, armed_caption: impl Into<WidgetText>) -> Self {
        Self { idle_caption: idle_caption.into(), armed_caption: armed_caption.into(), idle_fill: None, enabled: true }
    }

    /// Fill of the button at REST (default: the theme's normal button fill). The preset picker
    /// passes `ms_theme::status::DESTRUCTIVE_IDLE_FILL` so its delete reads as destructive before
    /// it is armed.
    #[inline]
    pub fn idle_fill(mut self, fill: Color32) -> Self {
        self.idle_fill = Some(fill);
        self
    }

    /// Whether the button takes input (default `true`). A disabled button reports neither clicks
    /// nor hovers, so it is drawn and decided as NOT armed whatever the caller's arm says.
    #[inline]
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Draws the button with the caller's `armed` flag and returns its response plus the step
    /// the caller must apply to that flag.
    pub fn show(self, ui: &mut Ui, armed: bool) -> (Response, ArmedDeleteStep) {
        // A disabled button can never be hovered, so an arm left behind would otherwise never
        // disarm through the pointer rule.
        let armed = armed && self.enabled;
        let button = if armed {
            egui::Button::new(self.armed_caption).fill(ms_theme::status::DESTRUCTIVE_ARMED_FILL)
        } else {
            let button = egui::Button::new(self.idle_caption);
            match self.idle_fill {
                Some(fill) => button.fill(fill),
                None => button,
            }
        };
        let response = ui.add_enabled(self.enabled, button);
        let step = armed_delete_step(armed, response.clicked(), response.double_clicked(), response.hovered());
        (response, step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_needs_an_armed_plain_click_and_disarms_off_hover() {
        assert_eq!(armed_delete_step(false, true, false, true), ArmedDeleteStep::Arm);
        assert_eq!(armed_delete_step(true, true, false, true), ArmedDeleteStep::Confirm);
        // A double click neither arms nor confirms.
        assert_eq!(armed_delete_step(false, true, true, true), ArmedDeleteStep::Keep);
        assert_eq!(armed_delete_step(true, true, true, true), ArmedDeleteStep::Keep);
        // An armed button that lost the pointer disarms; a hovered one stays armed.
        assert_eq!(armed_delete_step(true, false, false, false), ArmedDeleteStep::Disarm);
        assert_eq!(armed_delete_step(true, false, false, true), ArmedDeleteStep::Keep);
        // An unarmed button the pointer is not on does nothing.
        assert_eq!(armed_delete_step(false, false, false, false), ArmedDeleteStep::Keep);
    }

    #[test]
    fn a_disabled_button_is_never_armed() {
        let ctx = egui::Context::default();
        let mut outcome = None;
        ctx.run_ui(egui::RawInput::default(), |ui| {
            let (response, step) = ArmedDeleteButton::new("Delete", "Delete?").enabled(false).show(ui, true);
            outcome = Some((response.enabled(), step));
        })
        .drop_without_applying_deltas();
        // Not hovered, but the effective arm is off, so nothing is reported to disarm either.
        assert_eq!(outcome, Some((false, ArmedDeleteStep::Keep)));
    }
}
