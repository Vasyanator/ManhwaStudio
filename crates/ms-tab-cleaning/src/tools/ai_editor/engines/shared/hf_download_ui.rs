/*
File: cleaning/tools/ai_editor/engines/shared/hf_download_ui.rs

Purpose:
The drawing half of the Hugging Face token controls shared by the large-model engines (whose
gated downloads are the app's model catalog): the token row with its optional access hint, the
caption of its save button, and the gated action button every engine panel uses.

Key structures:
- `HfTokenView`

Key functions:
- `draw_hf_token_row()`, `token_save_button_label()`, `gated_button()`

Notes:
Drawing only: a pressed control leaves as an `HfTokenAction` the engine folds back after the
frame; nothing here starts work. The token never reaches this file's own state:
`HfTokenView::input` is the entry field's buffer and no branch prints, logs or formats a token
value. Every text is a `cleaning.tools.hf_download.*` key.
*/

use super::hf_download::HfTokenAction;
use eframe::egui;
use ms_sysprobe::hf_token::HfTokenState;
use ms_widgets::HfAccessHint;
use ms_widgets::hf_access_hint::HF_TOKEN_SETTINGS_URL;

/// Draws one gated action button and reports whether it was clicked.
///
/// Every gated control of these panels is disabled for a reason the user cannot see from the
/// button itself (no cache yet, no selection, an unreachable backend, an operation already
/// running), so a disabled tooltip is not optional here — it is the only place that reason is
/// stated.
pub(in crate::tools::ai_editor::engines) fn gated_button(
    ui: &mut egui::Ui,
    enabled: bool,
    caption: &str,
    tooltip: &str,
    disabled_tooltip: &str,
) -> bool {
    ui.add_enabled(enabled, egui::Button::new(caption))
        .on_hover_text(tooltip)
        .on_disabled_hover_text(disabled_tooltip)
        .clicked()
}

/// What the token row reads (and the one buffer it writes) for one frame.
pub(in crate::tools::ai_editor::engines) struct HfTokenView<'a> {
    /// Tri-state of the OS secret store: not read yet / no token / a token is stored.
    pub(in crate::tools::ai_editor::engines) state: HfTokenState,
    /// The masked field's buffer. Never persisted and never logged.
    pub(in crate::tools::ai_editor::engines) input: &'a mut String,
    pub(in crate::tools::ai_editor::engines) status: Option<&'a str>,
    /// A secret-store save or delete is in flight; both buttons are closed meanwhile.
    pub(in crate::tools::ai_editor::engines) busy: bool,
}

/// Draws the Hugging Face token controls: the saved-token badge, the step-by-step access hint
/// (when the caller hands one), the masked field and the save/delete pair, plus the "where a
/// token comes from" link while none is saved.
///
/// `id_salt` is the token field's stable id — the caller's, so two engines never share one
/// text cursor and an existing field keeps its id. `access_hint` names the gated repositories
/// the model needs (`HfAccessHint::repo`); its step 4 names this row's save button, so the
/// caller builds it with `token_save_button_label()`.
pub(in crate::tools::ai_editor::engines) fn draw_hf_token_row(
    ui: &mut egui::Ui,
    view: HfTokenView<'_>,
    id_salt: &str,
    access_hint: Option<HfAccessHint<'_>>,
    action: &mut Option<HfTokenAction>,
) {
    let HfTokenView { state, input, status, busy } = view;
    ui.horizontal_wrapped(|ui| {
        ui.small(match state {
            // "Not read yet" is its own line: telling the user no token is saved while the
            // secret store has not answered would send them to create a second one.
            HfTokenState::Unknown => t!("cleaning.tools.hf_download.token_unknown_status"),
            HfTokenState::Missing => t!("cleaning.tools.hf_download.token_missing_status"),
            HfTokenState::Stored => t!("cleaning.tools.hf_download.token_stored_status"),
        });
        // Beside the badge, above the field its step 4 points at ("under this hint").
        if let Some(hint) = access_hint {
            hint.show(ui);
        }
    });
    ui.add(
        egui::TextEdit::singleline(&mut *input)
            .password(true)
            .id_salt(id_salt)
            .hint_text(t!("cleaning.tools.hf_download.token_hint_text")),
    );
    ui.horizontal_wrapped(|ui| {
        if gated_button(
            ui,
            !busy && !input.trim().is_empty(),
            token_save_button_label(),
            t!("cleaning.tools.hf_download.token_save_tooltip"),
            t!("cleaning.tools.hf_download.token_save_disabled_tooltip"),
        ) {
            *action = Some(HfTokenAction::Save);
        }
        if gated_button(
            ui,
            !busy && state == HfTokenState::Stored,
            t!("cleaning.tools.hf_download.token_delete_button"),
            t!("cleaning.tools.hf_download.token_delete_tooltip"),
            t!("cleaning.tools.hf_download.token_delete_disabled_tooltip"),
        ) {
            *action = Some(HfTokenAction::Delete);
        }
    });
    if let Some(status) = status {
        ui.small(status);
    }
    if state == HfTokenState::Missing {
        // Whoever reaches this row needs a token — a gated repository always does, an
        // ungated one because the backend just asked for one — so without a saved token
        // there is nothing to check and nothing to download: the only useful thing this
        // block can do is say where a token comes from.
        ui.small(t!("cleaning.tools.hf_download.no_token_hint"));
        ui.hyperlink_to(
            t!("cleaning.tools.hf_download.token_settings_link"),
            HF_TOKEN_SETTINGS_URL,
        );
    }
}

/// The caption of the token row's «Сохранить» button — what the access hint's last step
/// tells the user to press, so the two can never name different buttons.
pub(in crate::tools::ai_editor::engines) fn token_save_button_label() -> &'static str {
    t!("cleaning.tools.hf_download.token_save_button")
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    /// The `cleaning.tools.hf_download.*` keys of the backend-download block (access check,
    /// verdict rows, plan, «Скачать» / «Отмена»), retired with the last engine that used it.
    const RETIRED_KEYS: [&str; 23] = [
        "state_ok", "state_no_token", "state_invalid_token", "state_not_accepted", "state_not_found", "state_network_error",
        "state_unknown", "state_unrecognized_hint", "repo_page_link", "check_button", "check_tooltip", "check_disabled_tooltip",
        "checking_status", "check_error", "plan_status", "plan_unknown_status", "download_button", "download_sized_button",
        "download_disabled_tooltip", "cancel_button", "cancel_tooltip", "cancel_disabled_tooltip", "complete_status",
    ];

    /// Every key the token row and `hf_download.rs` spell is in every shipped catalog — which a
    /// `t!` call cannot check, because a unit test runs with no catalog loaded — and the keys of
    /// the retired backend-download block are gone from all of them.
    #[test]
    fn every_catalog_carries_the_token_row_strings() {
        let plain = [
            "cleaning.tools.hf_download.no_token_hint",
            "cleaning.tools.hf_download.token_settings_link",
            "hf_token.empty_error",
        ];
        for (tag, source) in ms_i18n::embedded_locales() {
            let catalog: Value =
                serde_json::from_str(source).unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            let entry = |key: &str| -> String {
                catalog
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("locale `{tag}` lacks the key `{key}`"))
                    .to_owned()
            };
            for key in plain {
                assert!(!entry(key).trim().is_empty(), "locale `{tag}`: `{key}` is empty");
            }
            for key in [
                "token_save_button",
                "token_save_tooltip",
                "token_save_disabled_tooltip",
                "token_delete_button",
                "token_delete_tooltip",
                "token_delete_disabled_tooltip",
                "token_unknown_status",
                "token_missing_status",
                "token_stored_status",
                "token_hint_text",
                "token_saved_status",
                "token_deleted_status",
            ] {
                let key = format!("cleaning.tools.hf_download.{key}");
                assert!(!entry(&key).trim().is_empty(), "locale `{tag}`: `{key}` is empty");
            }
            for key in RETIRED_KEYS {
                let key = format!("cleaning.tools.hf_download.{key}");
                assert!(catalog.get(&key).is_none(), "locale `{tag}`: the retired `{key}` is still there");
            }
        }
    }
}
