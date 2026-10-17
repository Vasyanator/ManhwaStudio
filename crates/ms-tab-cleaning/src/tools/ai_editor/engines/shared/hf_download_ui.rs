/*
File: cleaning/tools/ai_editor/engines/shared/hf_download_ui.rs

Purpose:
The drawing half of a Hugging Face model download block, shared by the large-model engines:
the token row, one repository's verdict row with the page it links to, the access-check /
plan / «Скачать» / «Отмена» controls, and the gated action button every engine panel uses.

Key structures:
- `HfTokenView`, `HfDownloadControlsView`

Key functions:
- `draw_hf_token_row()`, `draw_hf_repo_row()`, `draw_hf_download_controls()`, `gated_button()`

Notes:
Drawing only: a pressed control leaves as an `HfTokenAction` / `HfDownloadAction` the engine
folds back after the frame; nothing here starts work. The token never reaches this file's own
state: `HfTokenView::input` is the entry field's buffer and no branch prints, logs or formats a
token value. Every text is a `cleaning.tools.hf_download.*` key; what is model-specific (the
block heading, the repository line, the download button's hover) is drawn or handed in by the
engine.
*/

use super::hf_download::{HfDownloadAction, HfDownloadCheck, HfDownloadReadiness, HfDownloadRepo, HfTokenAction, hf_download_readiness};
use eframe::egui;
use ms_local_ai::large_models::format_gib;
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

/// Draws one repository's verdict row: its id, the localized state, and the link that
/// makes the state actionable.
///
/// A state literal this build does not know degrades to "not known" with the literal on
/// hover and offers NO link — the same rule the residency block follows, and for the same
/// reason: guessing which of the six a new literal means would send the user to the wrong
/// page.
pub(in crate::tools::ai_editor::engines) fn draw_hf_repo_row(ui: &mut egui::Ui, repo: &HfDownloadRepo) {
    ui.horizontal_wrapped(|ui| {
        // The repository id is a backend identifier, so it stays literal.
        ui.small(repo.repo.clone());
        match repo.state {
            Some(state) => {
                let response = match state.color() {
                    Some(color) => ui.colored_label(color, state.message()),
                    None => ui.small(state.message()),
                };
                // The backend's own wording is the TECHNICAL half and lives on the hover:
                // `network_error` has no localizable content, so without it the row would
                // say only "the repository could not be reached" and nothing about why.
                if !repo.message.is_empty() {
                    response.on_hover_text(repo.message.clone());
                }
            }
            None => {
                let response = ui.small(t!("cleaning.tools.hf_download.state_unknown"));
                if !repo.state_wire.is_empty() {
                    response.on_hover_text(tf!("cleaning.tools.hf_download.state_unrecognized_hint", value = repo.state_wire));
                }
            }
        }
    });
    if let Some((caption, url)) = repo.link() {
        ui.hyperlink_to(caption, url);
    }
}

/// What the access-check and download controls read for one frame.
pub(in crate::tools::ai_editor::engines) struct HfDownloadControlsView<'a, Q> {
    /// The last access check, ALREADY filtered to what the panel asks about right now (the
    /// engine's staleness rule); `None` shows no rows and keeps «Скачать» closed.
    pub(in crate::tools::ai_editor::engines) check: Option<&'a HfDownloadCheck<Q>>,
    pub(in crate::tools::ai_editor::engines) check_error: Option<&'a str>,
    pub(in crate::tools::ai_editor::engines) check_busy: bool,
    /// A `.download.start` is in flight: it decides whether «Отмена» is open.
    pub(in crate::tools::ai_editor::engines) download_busy: bool,
    pub(in crate::tools::ai_editor::engines) download_status: Option<&'a str>,
    pub(in crate::tools::ai_editor::engines) ai_backend_available: bool,
    /// One of the engine's long operations holds the backend; check and download wait.
    pub(in crate::tools::ai_editor::engines) pipeline_busy: bool,
    /// The «Скачать» hover: what a finished download means is the engine's to say.
    pub(in crate::tools::ai_editor::engines) download_tooltip: &'a str,
}

/// Draws «Проверить доступ» with the per-repository verdicts, the plan line, «Скачать» /
/// «Отмена», the completion line and the last download's status.
///
/// A size is drawn ONLY from a plan that was actually computed ([`hf_download_readiness`]):
/// an unpriced download gets the bare button caption and no size line, never "0.0 GiB".
pub(in crate::tools::ai_editor::engines) fn draw_hf_download_controls<Q>(
    ui: &mut egui::Ui,
    view: &HfDownloadControlsView<'_, Q>,
    action: &mut Option<HfDownloadAction>,
) {
    // --- the access check ----------------------------------------------------------
    ui.horizontal_wrapped(|ui| {
        if gated_button(
            ui,
            view.ai_backend_available && !view.check_busy && !view.pipeline_busy,
            t!("cleaning.tools.hf_download.check_button"),
            t!("cleaning.tools.hf_download.check_tooltip"),
            t!("cleaning.tools.hf_download.check_disabled_tooltip"),
        ) {
            *action = Some(HfDownloadAction::Check);
        }
        if view.check_busy {
            ui.small(t!("cleaning.tools.hf_download.checking_status"));
        }
    });
    if let Some(error) = view.check_error {
        ui.colored_label(ms_theme::status::ERROR, tf!("cleaning.tools.hf_download.check_error", err = error));
    }
    if let Some(check) = view.check {
        for repo in &check.repos {
            draw_hf_repo_row(ui, repo);
        }
    }

    // --- the download --------------------------------------------------------------
    let readiness = hf_download_readiness(view.check);
    // A size line is drawn ONLY from a plan that was actually computed. Without one there
    // is no number to show and none to invent: a zero would read as "nothing left to
    // download" and is the exact lie the nullable plan exists to prevent.
    if let Some(plan) = view.check.and_then(|check| check.plan) {
        ui.small(tf!(
            "cleaning.tools.hf_download.plan_status",
            missing = format_gib(plan.missing_bytes),
            files = plan.missing_files,
            total = format_gib(plan.total_bytes)
        ));
    }
    // The unpriced case gets a line only when the backend said WHY. A `plan_error` is sent
    // exactly when access succeeded and the listing failed — the one situation the
    // repository rows cannot explain, because every one of them reads `ok`. When it is
    // empty the rows already say what is wrong (no token, an inaccessible repository) and a
    // second line would only restate them.
    if let Some(check) = view.check
        && check.plan.is_none()
        && !check.plan_error.is_empty()
    {
        // The localized sentence carries the meaning and the backend's own wording rides on
        // the hover, exactly as a repository `message` does.
        ui.colored_label(ms_theme::status::WARNING, t!("cleaning.tools.hf_download.plan_unknown_status"))
            .on_hover_text(check.plan_error.clone());
    }
    ui.horizontal_wrapped(|ui| {
        // The size goes on the button only when it is a size the user can act on. An
        // unpriced download gets the bare caption — never "Download (0.0 GiB)", which is
        // the same lie as the completion line.
        let caption = match readiness {
            HfDownloadReadiness::Priced { missing_bytes } => {
                tf!("cleaning.tools.hf_download.download_sized_button", size = format_gib(missing_bytes))
            }
            HfDownloadReadiness::Blocked | HfDownloadReadiness::SizeUnknown | HfDownloadReadiness::Complete => {
                t!("cleaning.tools.hf_download.download_button").to_string()
            }
        };
        if gated_button(
            ui,
            view.ai_backend_available && !view.pipeline_busy && readiness.startable(),
            &caption,
            view.download_tooltip,
            t!("cleaning.tools.hf_download.download_disabled_tooltip"),
        ) {
            *action = Some(HfDownloadAction::Start);
        }
        if gated_button(
            ui,
            view.download_busy,
            t!("cleaning.tools.hf_download.cancel_button"),
            t!("cleaning.tools.hf_download.cancel_tooltip"),
            t!("cleaning.tools.hf_download.cancel_disabled_tooltip"),
        ) {
            *action = Some(HfDownloadAction::Cancel);
        }
    });
    // Everything is already on disk: say so, rather than leaving a disabled button with
    // no explanation of why it is disabled. Reached ONLY through a plan that was actually
    // computed — never through a missing one.
    if readiness == HfDownloadReadiness::Complete {
        ui.colored_label(ms_theme::status::SUCCESS, t!("cleaning.tools.hf_download.complete_status"));
    }
    if let Some(status) = view.download_status {
        ui.small(status);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    /// Every key this block and `hf_download.rs` spell is in every shipped catalog, with the
    /// placeholders the code interpolates — which a `t!` call cannot check, because a unit
    /// test runs with no catalog loaded.
    #[test]
    fn every_catalog_carries_the_download_block_strings() {
        let interpolated = [
            ("cleaning.tools.hf_download.state_unrecognized_hint", vec!["{value}"]),
            ("cleaning.tools.hf_download.check_error", vec!["{err}"]),
            ("cleaning.tools.hf_download.plan_status", vec!["{missing}", "{files}", "{total}"]),
            ("cleaning.tools.hf_download.download_sized_button", vec!["{size}"]),
        ];
        let plain = [
            "cleaning.tools.hf_download.no_token_hint",
            "cleaning.tools.hf_download.token_settings_link",
            "cleaning.tools.hf_download.repo_page_link",
            "cleaning.tools.hf_download.check_button",
            "cleaning.tools.hf_download.checking_status",
            "cleaning.tools.hf_download.download_button",
            "cleaning.tools.hf_download.cancel_button",
            "cleaning.tools.hf_download.complete_status",
            "cleaning.tools.hf_download.state_unknown",
            "cleaning.tools.hf_download.plan_unknown_status",
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
            for (key, placeholders) in &interpolated {
                let text = entry(key);
                for placeholder in placeholders {
                    assert!(text.contains(placeholder), "locale `{tag}`: `{key}` must carry `{placeholder}`, `{text}` does not");
                }
            }
            // Six verdicts drawn with the same words would make the block unreadable, and
            // three of them differ only in what the user is supposed to do next.
            let mut wordings = Vec::new();
            for suffix in ["ok", "no_token", "invalid_token", "not_accepted", "not_found", "network_error"] {
                let text = entry(&format!("cleaning.tools.hf_download.state_{suffix}"));
                assert!(!wordings.contains(&text), "locale `{tag}`: two download states share the wording `{text}`");
                wordings.push(text);
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
                "check_tooltip",
                "check_disabled_tooltip",
                "download_disabled_tooltip",
                "cancel_tooltip",
                "cancel_disabled_tooltip",
            ] {
                let key = format!("cleaning.tools.hf_download.{key}");
                assert!(!entry(&key).trim().is_empty(), "locale `{tag}`: `{key}` is empty");
            }
        }
    }
}
