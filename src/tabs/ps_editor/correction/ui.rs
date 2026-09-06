/*
File: tabs/ps_editor/correction/ui.rs

Purpose:
The «Коррекция» dock-tab body and the reusable per-kind parameter CARD it renders.

Key functions:
- `correction_panel_body`: the whole panel — the «Настройка» section plus the unavailability notice.
- `correction_card_controls`: the card. A free function over `&mut Correction`, carrying NO
  host-specific state, so a second host can render the same controls unchanged.

Notes:
Mirrors the typing tab's card idiom (`tabs/typing/panel/effect_cards.rs::draw_effect_card_controls`)
deliberately rather than importing it: everything there is private to `typing/panel`, and the two
card sets have no shared vocabulary.
*/

use eframe::egui;

use super::model::{Correction, CorrectionKind, CorrectionState, PARAM_MAX, PARAM_MIN};
use crate::widgets::{WheelComboBox, WheelSlider};

/// `id_salt` of the kind combo. A literal, never the localized caption: egui derives a combo's `Id`
/// from its label text, so a language switch would otherwise reset the widget
/// (`egui-docs/05-ids-and-i18n.md` §2).
const KIND_COMBO_ID_SALT: &str = "ps_editor.correction.kind_combo";

/// Draws the «Коррекция» panel body and reports whether the user changed anything.
///
/// `state` is the tab's live correction; `filter_failed` says whether the GPU pass has given up, in
/// which case the panel says so instead of pretending the sliders do something.
///
/// The return value exists for the caller's repaint decision; the correction itself is read fresh
/// by the canvas every frame, so nothing has to be invalidated here.
pub(crate) fn correction_panel_body(ui: &mut egui::Ui, state: &mut CorrectionState, filter_failed: bool) -> bool {
    let mut changed = false;
    ui.label(t!("ps_editor.correction.setup_section"));
    if filter_failed {
        // The pass is dead for this session; say so where the user is looking, in the panel whose
        // controls would otherwise appear to do nothing.
        ui.colored_label(
            egui::Color32::from_rgb(220, 80, 80),
            t!("ps_editor.correction.gpu_unavailable_error"),
        );
    }
    ui.add_space(4.0);

    // `show_index` is the WheelComboBox entry point that already owns the whole index dance:
    // selectable rows, wheel cycling when the popup is closed, and a `Response` marked changed.
    let mut kind_idx = CorrectionKind::ALL
        .iter()
        .position(|kind| *kind == state.kind)
        .unwrap_or(0);
    let response = WheelComboBox::from_label(t!("ps_editor.correction.kind_combo_label"))
        .id_salt(KIND_COMBO_ID_SALT)
        .show_index(ui, &mut kind_idx, CorrectionKind::ALL.len(), |idx| {
            CorrectionKind::ALL[idx].title()
        });
    response.on_hover_text(t!("ps_editor.correction.kind_combo_tooltip"));
    if let Some(&selected) = CorrectionKind::ALL.get(kind_idx)
        && selected != state.kind
    {
        state.kind = selected;
        changed = true;
    }

    // «Нет» shows no card at all: it is the "correct nothing" choice, not a correction with neutral
    // parameters, and an empty card would invite the user to drag sliders that do nothing.
    match state.kind {
        CorrectionKind::None => {}
        CorrectionKind::BrightnessContrast => {
            ui.add_space(4.0);
            changed |= correction_card_controls(ui, &mut state.correction);
        }
    }
    changed
}

/// Draws the editable controls of ONE correction and reports whether anything changed.
///
/// The card takes NO host-specific state on purpose: its only argument besides the `Ui` is the
/// `&mut Correction` it edits. That is the contract that lets a second host — the «Пресеты» section
/// that comes later — render the same controls over a preset's own `Correction` value without this
/// function changing at all. Do not add host parameters to it; pass host-scoped extras through a
/// single binding argument if one ever becomes unavoidable, the way the typing tab does.
///
/// One `match` arm per [`CorrectionKind`] with a payload, exhaustive by construction, so a new
/// correction cannot be added without being given controls here.
pub(crate) fn correction_card_controls(ui: &mut egui::Ui, correction: &mut Correction) -> bool {
    let mut changed = false;
    match correction {
        Correction::BrightnessContrast { brightness, contrast } => {
            changed |= ui
                .add(
                    WheelSlider::new(brightness, PARAM_MIN..=PARAM_MAX)
                        .text(t!("ps_editor.correction.brightness_label")),
                )
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(contrast, PARAM_MIN..=PARAM_MAX)
                        .text(t!("ps_editor.correction.contrast_label")),
                )
                .changed();
        }
    }
    changed
}
