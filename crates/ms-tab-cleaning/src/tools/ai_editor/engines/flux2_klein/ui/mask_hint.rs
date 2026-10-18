/*
File: cleaning/tools/ai_editor/engines/flux2_klein/ui/mask_hint.rs

Purpose:
The hint that explains what the mask MEANS for this engine (the end of the scrolled body,
just above the bottom-docked «Постобработка» section).
The progress bar every long operation of the engine shares is the shared
`draw_stream_progress` (`../../shared/stream_progress_ui.rs`), called from
`draw_flux2_progress_section` in `mod.rs`.

Main responsibilities:
- state the mask rule and what an empty mask does (`draw_flux2_mask_hint`).

Key functions:
- `draw_flux2_mask_hint()`

Notes:
The mask hint reads no state at all — the working mode is derived from
the mask itself, and the brush belongs to the host's «Выбранный инструмент» panel.
*/

use super::*;

/// Explains what the mask MEANS for this engine, and what an empty one means.
///
/// A free function and not a [`Flux2PanelCtx`] method because it reads no state at all:
/// there is no control here and nothing to change. The working mode is derived from the
/// mask itself ([`mask_for_run`]), and the brush — radius, paint/erase, clear and fill —
/// belongs to the host's «Выбранный инструмент» panel, which drives the frame's one
/// `MaskBrush` for every engine. Both halves of the rule are stated at once because the
/// user picks between them by painting or not painting rather than by reading a state
/// back; the host adds its own green line under «Обработать» while the empty half is what
/// a click would actually do.
pub(super) fn draw_flux2_mask_hint(ui: &mut egui::Ui) {
    ui.separator();
    ui.label(t!("cleaning.tools.flux2_klein.mask_heading"));
    // What the mask MEANS only: the paint / erase gestures are named, from the current
    // bindings, by the host's brush controls.
    ui.small(t!("cleaning.tools.flux2_klein.mask_meaning_hint"));
}
