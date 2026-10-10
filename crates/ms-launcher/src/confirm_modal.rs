/*
File: src/confirm_modal.rs

Purpose:
In-app Yes/No confirmation drawn as an `egui::Modal`. It replaces the native blocking
message boxes the launcher used for overwrite questions, so a confirmation never freezes the
GUI thread and works on every target (desktop, Android, web) without a native dialog.

Key structures:
- ConfirmModal: the question (stable id salt, localized title and text).
- ConfirmAnswer: what the user did this frame (`Pending`, `Yes`, `No`).

Notes:
The modal is immediate-mode: the caller stores the `ConfirmModal` (plus whatever the "Yes"
continuation needs) in its state, calls `show` every frame while it is stored, and drops it on
`Yes` / `No`. The backdrop blocks input to everything under it.
*/

use egui::{Align, Layout, RichText};

/// Width of the modal's text column, in points.
const MODAL_WIDTH: f32 = 420.0;

/// A Yes/No question shown as an in-app modal over the current viewport.
#[derive(Debug, Clone)]
pub(crate) struct ConfirmModal {
    id_salt: &'static str,
    title: String,
    text: String,
}

/// The user's answer to a [`ConfirmModal`] in the current frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmAnswer {
    /// Still open: keep the modal and show it again next frame.
    Pending,
    /// "Yes" was clicked.
    Yes,
    /// "No" was clicked, Escape was pressed, or the backdrop was clicked.
    No,
}

impl ConfirmModal {
    /// A question with a stable `id_salt` (an i18n key or another fixed string, so the modal's
    /// id never changes with the UI language) and already-localized `title` and `text`.
    pub(crate) fn new(id_salt: &'static str, title: impl Into<String>, text: impl Into<String>) -> Self {
        Self { id_salt, title: title.into(), text: text.into() }
    }

    /// Draws the modal for this frame and reports the answer. Call it every frame while the
    /// question is pending; `ctx` is the context of the viewport the modal belongs to.
    pub(crate) fn show(&self, ctx: &egui::Context) -> ConfirmAnswer {
        let modal = egui::Modal::new(egui::Id::new(self.id_salt)).show(ctx, |ui| {
            ui.set_max_width(MODAL_WIDTH);
            ui.label(RichText::new(&self.title).size(18.0).strong());
            ui.add_space(8.0);
            ui.label(&self.text);
            ui.add_space(12.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // Right-to-left: "No" is laid out first so it ends up rightmost, the safe
                // choice under the pointer's usual resting place for a destructive question.
                if ui.button(t!("launcher.common.no_button")).clicked() {
                    return ConfirmAnswer::No;
                }
                if ui.button(t!("launcher.common.yes_button")).clicked() {
                    return ConfirmAnswer::Yes;
                }
                ConfirmAnswer::Pending
            })
            .inner
        });
        // Escape and a click on the backdrop dismiss the question: both read as "No", the
        // non-destructive answer.
        if modal.inner == ConfirmAnswer::Pending && modal.should_close() {
            return ConfirmAnswer::No;
        }
        modal.inner
    }
}
