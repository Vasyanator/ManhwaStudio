/*
File: cleaning/tools/ai_editor/engines/flux2_klein/ui/install.rs

Purpose:
The «Установка модели» half of the panel: where the model comes from, where its three
paths point, and the Hugging Face download that fills them in.

Main responsibilities:
- draw the source-mode switch, the one control that decides whether the paths are typed
  or derived (`draw_source_mode_switch`);
- draw an editable model-path row with its browse buttons (`Flux2PathRow`,
  `draw_path_row`) and the read-only derived paths of the download mode
  (`draw_derived_paths`);
- draw the download block — the repository line, the token row (`draw_hf_token_row`,
  unconditional for a gated repository and reopened on an ungated one whose check blamed
  the token), the encoder toggle, the per-repository access verdicts with the link each one
  prescribes, and the controls (`Flux2DownloadView`, `draw_download_block`,
  `draw_download_repo_row`);
- draw one gated action button with the disabled tooltip that explains it
  (`flux2_gated_button`), which the component list and the prompt-cache controls share.

Key structures:
- `Flux2DownloadView`, `Flux2PathRow`

Key functions:
- `draw_source_mode_switch()`, `draw_path_row()`, `draw_derived_paths()`
- `draw_download_block()`, `draw_hf_token_row()`, `draw_download_repo_row()`,
  `flux2_gated_button()`

Notes:
The HF token never reaches this file's own state: it lives in the process-wide `hf_token`
module, the view carries only the buffer of the entry field, and no branch here prints,
logs or formats a token value. Presence marks beside a derived path come from the last
`.status` answer, never from a filesystem probe — this all runs on the GUI thread.
*/

use super::*;

/// Draws the source-mode switch: two toggle buttons, exactly one of them selected.
///
/// Two mutually exclusive positions with short captions, so this is the toggle-button
/// idiom the engine picker already uses (`../../mod.rs::draw_engine_picker`) —
/// `egui::Button::selected` in a `horizontal_wrapped`, with `wrap_mode = Extend` so a long
/// caption moves its button to the next ROW instead of breaking over two lines
/// (`egui-docs/04-widgets.md` §5). A `WheelComboBox` would hide one of two choices behind a
/// popup for no benefit, and `egui::ComboBox` is forbidden outright (§0.2). The buttons take
/// their `Id` from their localized captions, so each carries a stable `id_salt`
/// (`egui-docs/05-ids-and-i18n.md` §2) — without it, switching UI language would reset which
/// button egui thinks was clicked.
pub(super) fn draw_source_mode_switch(
    ui: &mut egui::Ui,
    settings: &mut Flux2KleinSettings,
    changed: &mut bool,
) {
    let active = settings.source_mode();
    let mut picked: Option<Flux2SourceMode> = None;
    ui.label(t!("cleaning.tools.flux2_klein.source_mode_label"));
    ui.horizontal_wrapped(|ui| {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        for mode in Flux2SourceMode::all() {
            // `Button` has no `id_salt` in egui 0.35 (it is not in
            // `egui-docs/api/symbols.txt`), so the salt is pushed onto a child `Ui` —
            // the mechanism `05-ids-and-i18n.md` §2 prescribes for exactly this case.
            // The salt is the MODE's stable token, not its localized caption, so switching
            // UI language cannot change which button egui thinks it is looking at.
            let response = ui
                .push_id(mode.wire(), |ui| {
                    ui.add(egui::Button::new(mode.label()).selected(mode == active))
                })
                .inner;
            if response.on_hover_text(mode.hint()).clicked() {
                picked = Some(mode);
            }
        }
    });
    if let Some(mode) = picked
        && mode != active
    {
        settings.source_mode = mode.wire().to_string();
        *changed = true;
    }
}

/// Draws the three DERIVED destination paths of the download mode, read-only, with what
/// the backend reports about each one's presence.
///
/// Read-only because nothing here is a choice: the paths follow the models directory and
/// the encoder toggle. They are shown anyway because the user still has to see where his
/// 35 GB went and whether the install is complete, and making him switch modes to find that
/// out would be the same as not showing it.
///
/// Presence comes from the last `.status` answer, never from a filesystem probe: this runs
/// on the GUI thread, which must not touch the disk. No answer yet means the mark is
/// omitted rather than guessed — an absent status is "not known", not "missing".
pub(super) fn draw_derived_paths(ui: &mut egui::Ui, settings: &Flux2KleinSettings, status: Option<&Flux2Status>) {
    let paths = settings.effective_paths();
    ui.label(t!("cleaning.tools.flux2_klein.download.derived_paths_label"));
    for (label, path, component) in [
        (
            t!("cleaning.tools.flux2_klein.text_encoder_path_label"),
            &paths.text_encoder,
            status.map(|status| &status.text_encoder),
        ),
        (
            t!("cleaning.tools.flux2_klein.transformer_path_label"),
            &paths.transformer,
            status.map(|status| &status.transformer),
        ),
        (
            t!("cleaning.tools.flux2_klein.vae_path_label"),
            &paths.vae,
            status.map(|status| &status.vae),
        ),
    ] {
        ui.horizontal_wrapped(|ui| {
            // Presence is only claimed when the backend actually answered.
            if let Some(component) = component {
                let mark = if component.present { "✓" } else { "✗" };
                let color = if component.present {
                    FLUX2_STATUS_OK_COLOR
                } else {
                    FLUX2_STATUS_WARN_COLOR
                };
                ui.colored_label(color, mark);
            }
            ui.small(label);
        });
        // Selectable so the user can copy the path out; a path is data, not prose, so it
        // stays literal.
        ui.add(egui::Label::new(egui::RichText::new(path).small()).selectable(true));
    }
}

/// Everything the model-download block READS, grouped so the block stays one function
/// with a reviewable signature instead of eleven positional parameters.
pub(super) struct Flux2DownloadView<'a> {
    /// The checkpoint this block installs. It decides whether a Hugging Face token row is
    /// drawn at all and whether the uncensored-encoder toggle exists, and it names the
    /// repository the block reports on.
    pub(super) variant: Flux2Variant,
    pub(super) token_state: ms_sysprobe::hf_token::HfTokenState,
    /// The masked field's buffer. Never persisted and never logged.
    pub(super) token_input: &'a mut String,
    pub(super) token_status: Option<&'a str>,
    pub(super) token_busy: bool,
    /// The last access check, ALREADY filtered to the checkpoint and the encoder toggle
    /// currently selected ([`Flux2KleinEngine::download_check_current`]).
    pub(super) check: Option<&'a Flux2DownloadCheck>,
    pub(super) check_error: Option<&'a str>,
    pub(super) check_busy: bool,
    pub(super) download_busy: bool,
    pub(super) download_status: Option<&'a str>,
    pub(super) ai_backend_available: bool,
    pub(super) pipeline_busy: bool,
    /// The last `.status` answer, for the presence marks beside the derived paths.
    pub(super) status: Option<&'a Flux2Status>,
}

/// Draws the Hugging Face model-download block: the repository it fetches, the token row,
/// the encoder toggle, the per-repository access verdicts with the link each one
/// prescribes, and the download controls.
///
/// TWO of those are per-variant:
/// - the TOKEN row is unconditional only for a gated repository (`requires_hf_token`); the
///   4B checkpoint is apache-2.0, so it gets one line saying no token is needed — UNLESS
///   the check itself blamed the token, in which case the row appears with an explanation,
///   because that credential has no other UI anywhere in the application;
/// - the UNCENSORED-encoder toggle is drawn only where such an encoder is published
///   (`supports_uncensored_encoder`); the backend refuses the other pairing, so offering
///   the control could only manufacture a refusal.
///
/// Everything else — the derived paths, the access check, the plan, the buttons and the
/// progress — is identical for both, because it is keyed by the three component paths.
///
/// The token never reaches this function's own state: it lives in the process-wide
/// `hf_token` module, `token_input` is only the buffer of the entry field, and no branch
/// here prints, logs or formats a token value.
pub(super) fn draw_download_block(
    ui: &mut egui::Ui,
    settings: &mut Flux2KleinSettings,
    mut view: Flux2DownloadView<'_>,
    changed: &mut bool,
    hf_token_action: &mut Option<Flux2HfTokenAction>,
    download_action: &mut Option<Flux2DownloadAction>,
) {
    ui.separator();
    ui.label(t!("cleaning.tools.flux2_klein.download.heading"));
    // This hint is entirely about GATING — a token and accepted conditions — so it is
    // drawn only where that is true. For an ungated variant it would be a false warning,
    // and `token_not_needed_hint` below says the opposite in one line.
    if view.variant.requires_hf_token() {
        ui.small(t!("cleaning.tools.flux2_klein.download.hint"));
    }
    // The repository the model itself comes from, named BEFORE any check has run: the
    // per-repository rows below only exist once the backend has answered, and until then
    // the block says nothing about which of the two checkpoints it would fetch. The id is
    // a backend identifier and stays literal, exactly as it does on those rows.
    ui.small(tf!(
        "cleaning.tools.flux2_klein.download.repo_status",
        repo = view.variant.model_repo()
    ));

    // --- the token row -------------------------------------------------------------
    // Always drawn for a variant whose repository is gated. For an ungated one it appears
    // only when the last check actually BLAMED the token: the 4B checkpoint is apache-2.0,
    // the request carries an empty token legally, and an unconditional field there would
    // ask the user to solve a problem they do not have. A verdict of `no_token` or
    // `invalid_token` means they DO have it — and `hf_token` has exactly one UI surface in
    // the whole application, this block, so leaving the row out there strands them with a
    // dead «Скачать» and a credential they cannot reach.
    if view.variant.requires_hf_token() {
        draw_hf_token_row(ui, &mut view, hf_token_action);
    } else if download_check_blames_token(view.check) {
        // Only reachable from a backend that authenticates against this repository even
        // though it is open — an outdated one. Both halves are said: what the answer means,
        // and the controls that make it actionable.
        ui.colored_label(
            FLUX2_STATUS_WARN_COLOR,
            t!("cleaning.tools.flux2_klein.download.token_unexpected_hint"),
        );
        draw_hf_token_row(ui, &mut view, hf_token_action);
    } else {
        // Said once, positively: the absence of a token row is otherwise indistinguishable
        // from a missing feature, and a user who set one up for the 9B model would wonder
        // why it is gone here.
        ui.small(t!("cleaning.tools.flux2_klein.download.token_not_needed_hint"));
    }

    // --- the encoder toggle --------------------------------------------------------
    // Only for a variant that HAS an uncensored encoder published for it. The backend
    // refuses the other pairing outright, so offering the control would only manufacture a
    // refusal; `normalized()` keeps the persisted flag inert there as well.
    if view.variant.supports_uncensored_encoder() {
        let toggled = ui
            .checkbox(
                &mut settings.uncensored_text_encoder,
                t!("cleaning.tools.flux2_klein.download.uncensored_label"),
            )
            .on_hover_text(t!("cleaning.tools.flux2_klein.download.uncensored_hint"))
            .changed();
        if toggled {
            *changed = true;
            // The toggle selects the encoder AND the directory the tool reads it from. Only
            // a path this block manages is repointed; a hand-picked one survives the flip.
            if let Some(path) = flux2_text_encoder_path_after_toggle(
                &settings.text_encoder_path,
                settings.uncensored_text_encoder,
                view.variant,
            ) {
                settings.text_encoder_path = path;
            }
        }
    }

    // Where the files go, and whether they are there. Drawn after the toggle because the
    // encoder path follows it.
    draw_derived_paths(ui, settings, view.status);

    // --- the access check ----------------------------------------------------------
    ui.horizontal_wrapped(|ui| {
        if flux2_gated_button(
            ui,
            view.ai_backend_available && !view.check_busy && !view.pipeline_busy,
            t!("cleaning.tools.flux2_klein.download.check_button"),
            t!("cleaning.tools.flux2_klein.download.check_tooltip"),
            t!("cleaning.tools.flux2_klein.download.check_disabled_tooltip"),
        ) {
            *download_action = Some(Flux2DownloadAction::Check);
        }
        if view.check_busy {
            ui.small(t!("cleaning.tools.flux2_klein.download.checking_status"));
        }
    });
    if let Some(error) = view.check_error {
        ui.colored_label(
            FLUX2_STATUS_ERROR_COLOR,
            tf!("cleaning.tools.flux2_klein.download.check_error", err = error),
        );
    }
    if let Some(check) = view.check {
        for repo in &check.repos {
            draw_download_repo_row(ui, repo);
        }
    }

    // --- the download --------------------------------------------------------------
    let readiness = flux2_download_readiness(view.check);
    // A size line is drawn ONLY from a plan that was actually computed. Without one there
    // is no number to show and none to invent: a zero would read as "nothing left to
    // download" and is the exact lie the nullable plan exists to prevent.
    if let Some(plan) = view.check.and_then(|check| check.plan) {
        ui.small(tf!(
            "cleaning.tools.flux2_klein.download.plan_status",
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
        ui.colored_label(
            FLUX2_STATUS_WARN_COLOR,
            t!("cleaning.tools.flux2_klein.download.plan_unknown_status"),
        )
        .on_hover_text(check.plan_error.clone());
    }
    ui.horizontal_wrapped(|ui| {
        // The size goes on the button only when it is a size the user can act on. An
        // unpriced download gets the bare caption — never "Download (0.0 GiB)", which is
        // the same lie as the completion line.
        let caption = match readiness {
            Flux2DownloadReadiness::Priced { missing_bytes } => tf!(
                "cleaning.tools.flux2_klein.download.download_sized_button",
                size = format_gib(missing_bytes)
            ),
            Flux2DownloadReadiness::Blocked
            | Flux2DownloadReadiness::SizeUnknown
            | Flux2DownloadReadiness::Complete => {
                t!("cleaning.tools.flux2_klein.download.download_button").to_string()
            }
        };
        // `SizeUnknown` keeps the button OPEN: pressing it re-lists, and either succeeds or
        // reports the same failure honestly. Leaving it closed would strand a user whose
        // listing failed once with no way to retry.
        let startable = match readiness {
            Flux2DownloadReadiness::Priced { .. } | Flux2DownloadReadiness::SizeUnknown => true,
            Flux2DownloadReadiness::Blocked | Flux2DownloadReadiness::Complete => false,
        };
        if flux2_gated_button(
            ui,
            view.ai_backend_available && !view.pipeline_busy && startable,
            &caption,
            t!("cleaning.tools.flux2_klein.download.download_tooltip"),
            t!("cleaning.tools.flux2_klein.download.download_disabled_tooltip"),
        ) {
            *download_action = Some(Flux2DownloadAction::Start);
        }
        if flux2_gated_button(
            ui,
            view.download_busy,
            t!("cleaning.tools.flux2_klein.download.cancel_button"),
            t!("cleaning.tools.flux2_klein.download.cancel_tooltip"),
            t!("cleaning.tools.flux2_klein.download.cancel_disabled_tooltip"),
        ) {
            *download_action = Some(Flux2DownloadAction::Cancel);
        }
    });
    // Everything is already on disk: say so, rather than leaving a disabled button with
    // no explanation of why it is disabled. Reached ONLY through a plan that was actually
    // computed — never through a missing one.
    if readiness == Flux2DownloadReadiness::Complete {
        ui.colored_label(
            FLUX2_STATUS_OK_COLOR,
            t!("cleaning.tools.flux2_klein.download.complete_status"),
        );
    }
    if let Some(status) = view.download_status {
        ui.small(status);
    }
}

/// Draws the Hugging Face token controls: the saved-token badge, the masked field and the
/// save/delete pair, plus the "where a token comes from" link while none is saved.
///
/// Split out because it has TWO callers with different conditions — a gated repository,
/// which always needs it, and an ungated one whose check nevertheless blamed the token —
/// and duplicating it would let the two drift into offering different controls for the
/// same credential.
///
/// The token never reaches this function's own state: `view.token_input` is the buffer of
/// the entry field, and no branch here prints, logs or formats a token value.
fn draw_hf_token_row(
    ui: &mut egui::Ui,
    view: &mut Flux2DownloadView<'_>,
    hf_token_action: &mut Option<Flux2HfTokenAction>,
) {
    use ms_sysprobe::hf_token::HfTokenState;

    ui.small(match view.token_state {
        // "Not read yet" is its own line: telling the user no token is saved while the
        // secret store has not answered would send them to create a second one.
        HfTokenState::Unknown => t!("cleaning.tools.flux2_klein.download.token_unknown_status"),
        HfTokenState::Missing => t!("cleaning.tools.flux2_klein.download.token_missing_status"),
        HfTokenState::Stored => t!("cleaning.tools.flux2_klein.download.token_stored_status"),
    });
    ui.add(
        egui::TextEdit::singleline(&mut *view.token_input)
            .password(true)
            .id_salt("cleaning_flux2_klein_hf_token")
            .hint_text(t!("cleaning.tools.flux2_klein.download.token_hint_text")),
    );
    ui.horizontal_wrapped(|ui| {
        if flux2_gated_button(
            ui,
            !view.token_busy && !view.token_input.trim().is_empty(),
            t!("cleaning.tools.flux2_klein.download.token_save_button"),
            t!("cleaning.tools.flux2_klein.download.token_save_tooltip"),
            t!("cleaning.tools.flux2_klein.download.token_save_disabled_tooltip"),
        ) {
            *hf_token_action = Some(Flux2HfTokenAction::Save);
        }
        if flux2_gated_button(
            ui,
            !view.token_busy && view.token_state == HfTokenState::Stored,
            t!("cleaning.tools.flux2_klein.download.token_delete_button"),
            t!("cleaning.tools.flux2_klein.download.token_delete_tooltip"),
            t!("cleaning.tools.flux2_klein.download.token_delete_disabled_tooltip"),
        ) {
            *hf_token_action = Some(Flux2HfTokenAction::Delete);
        }
    });
    if let Some(status) = view.token_status {
        ui.small(status);
    }
    if view.token_state == HfTokenState::Missing {
        // Whoever reaches this row needs a token — a gated repository always does, an
        // ungated one because the backend just asked for one — so without a saved token
        // there is nothing to check and nothing to download: the only useful thing this
        // block can do is say where a token comes from.
        ui.small(t!("cleaning.tools.flux2_klein.download.no_token_hint"));
        ui.hyperlink_to(
            t!("cleaning.tools.flux2_klein.download.token_settings_link"),
            FLUX2_HF_TOKEN_SETTINGS_URL,
        );
    }
}

/// Draws one repository's verdict row: its id, the localized state, and the link that
/// makes the state actionable.
///
/// A state literal this build does not know degrades to "not known" with the literal on
/// hover and offers NO link — the same rule the residency block follows, and for the same
/// reason: guessing which of the six a new literal means would send the user to the wrong
/// page.
pub(super) fn draw_download_repo_row(ui: &mut egui::Ui, repo: &Flux2DownloadRepo) {
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
                let response = ui.small(t!("cleaning.tools.flux2_klein.download.state_unknown"));
                if !repo.state_wire.is_empty() {
                    response.on_hover_text(tf!(
                        "cleaning.tools.flux2_klein.download.state_unrecognized_hint",
                        value = repo.state_wire
                    ));
                }
            }
        }
    });
    if let Some((caption, url)) = repo.link() {
        ui.hyperlink_to(caption, url);
    }
}

// ---------------------------------------------------------------------------------------
// UI helpers
// ---------------------------------------------------------------------------------------

/// The static description of one model-path row, bundled so the drawing function keeps a
/// short signature: what the path is called, what belongs in it, the persistent id of its
/// field, and the browse buttons beside it.
///
/// `hint` explains what belongs in THIS path — a folder or a file, and which one. The
/// per-button `tooltip` describes the PICKER a glyph opens and cannot stand in for it.
pub(super) struct Flux2PathRow<'a> {
    pub(super) label: &'a str,
    pub(super) hint: &'a str,
    pub(super) id_salt: &'static str,
    /// One `(glyph, tooltip, purpose)` entry per browse button.
    pub(super) buttons: &'a [(&'static str, &'static str, Flux2PickerPurpose)],
}

/// Draws one editable model-path row: a label, a text field, and its browse buttons.
/// Returns nothing; `changed` and `picker_requested` carry the outcome.
pub(super) fn draw_path_row(
    ui: &mut egui::Ui,
    row: &Flux2PathRow<'_>,
    value: &mut String,
    changed: &mut bool,
    picker_requested: &mut Option<Flux2PickerPurpose>,
) {
    // The hint sits on the label AND on the field: the label is what a user reads first,
    // the field is what he hovers while wondering what to paste into it. Only the glyph
    // buttons used to say anything, and they explain the PICKER, not the path.
    ui.label(row.label).on_hover_text(row.hint);
    ui.horizontal(|ui| {
        let edit = ui.add(
            egui::TextEdit::singleline(value)
                .id_salt(row.id_salt)
                .desired_width(ui.available_width() - 64.0),
        );
        *changed |= edit.changed();
        edit.on_hover_text(row.hint);
        for (glyph, tooltip, purpose) in row.buttons {
            // Glyph buttons: an icon is not prose, so the caption stays literal and the
            // localized text lives in the tooltip.
            if ui.small_button(*glyph).on_hover_text(*tooltip).clicked() {
                *picker_requested = Some(*purpose);
            }
        }
    });
}

/// Draws one gated action button of this engine and reports whether it was clicked.
///
/// Shared by the prompt-cache controls and the per-component actions. Every one of them is
/// disabled for a reason the user cannot see from the button itself (no cache yet, no
/// selection, an unreachable backend, an operation already running), so a disabled tooltip
/// is not optional here — it is the only place that reason is stated.
pub(super) fn flux2_gated_button(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalog_carries_the_download_block_strings() {
        let interpolated = [
            (
                "cleaning.tools.flux2_klein.download.state_unrecognized_hint",
                vec!["{value}"],
            ),
            (
                "cleaning.tools.flux2_klein.download.check_error",
                vec!["{err}"],
            ),
            ("cleaning.tools.flux2_klein.download.error", vec!["{err}"]),
            (
                "cleaning.tools.flux2_klein.download.done_status",
                vec!["{size}", "{skipped}"],
            ),
            (
                "cleaning.tools.flux2_klein.download.plan_status",
                vec!["{missing}", "{files}", "{total}"],
            ),
            (
                "cleaning.tools.flux2_klein.download.download_sized_button",
                vec!["{size}"],
            ),
            (
                "cleaning.tools.flux2_klein.download.overall_progress_status",
                vec!["{done}", "{total}"],
            ),
            (
                "cleaning.tools.flux2_klein.download.speed_status",
                vec!["{speed}"],
            ),
            (
                "cleaning.tools.flux2_klein.download.speed_and_eta_status",
                vec!["{speed}", "{eta}"],
            ),
            (
                "cleaning.tools.flux2_klein.download.eta_hours",
                vec!["{hours}", "{minutes}"],
            ),
            (
                "cleaning.tools.flux2_klein.download.eta_minutes",
                vec!["{minutes}"],
            ),
            (
                "cleaning.tools.flux2_klein.download.eta_seconds",
                vec!["{seconds}"],
            ),
            (
                "cleaning.tools.flux2_klein.download.file_progress_status",
                vec!["{label}", "{done}", "{total}"],
            ),
            // Without `{repo}` the line would claim to name a repository and name none,
            // which is worse than not drawing it: the two variants differ by exactly that
            // id before any check has run.
            (
                "cleaning.tools.flux2_klein.download.repo_status",
                vec!["{repo}"],
            ),
            // Both checkpoints are named, or the sentence cannot say what went wrong.
            (
                "cleaning.tools.flux2_klein.download.variant_mismatch_error",
                vec!["{expected}", "{actual}"],
            ),
        ];
        let plain = [
            "cleaning.tools.flux2_klein.pipeline_busy_error",
            "cleaning.tools.flux2_klein.download.heading",
            "cleaning.tools.flux2_klein.download.hint",
            "cleaning.tools.flux2_klein.download.no_token_hint",
            "cleaning.tools.flux2_klein.download.token_settings_link",
            "cleaning.tools.flux2_klein.download.repo_page_link",
            "cleaning.tools.flux2_klein.download.uncensored_label",
            "cleaning.tools.flux2_klein.download.uncensored_hint",
            // The 4B half of the block: the line that replaces the token row, and the one
            // that explains why the other engine's model disappears from memory.
            "cleaning.tools.flux2_klein.download.token_not_needed_hint",
            // …and the line that replaces it when the backend nevertheless blames the token,
            // which is the only thing that reopens the token row on an ungated checkpoint.
            "cleaning.tools.flux2_klein.download.token_unexpected_hint",
            // The picker's disabled tooltips: a closed control that says nothing is a defect.
            "cleaning.tools.flux2_klein.download.switch_blocked_download",
            "cleaning.tools.flux2_klein.download.switch_blocked_check",
            "cleaning.tools.flux2_klein.variant_residency_hint",
            "cleaning.tools.flux2_klein.title",
            "cleaning.tools.flux2_klein.title_4b",
            "cleaning.tools.flux2_klein.download.check_button",
            "cleaning.tools.flux2_klein.download.checking_status",
            "cleaning.tools.flux2_klein.download.download_button",
            "cleaning.tools.flux2_klein.download.cancel_button",
            "cleaning.tools.flux2_klein.download.complete_status",
            "cleaning.tools.flux2_klein.download.running_status",
            "cleaning.tools.flux2_klein.download.cancelled_status",
            "cleaning.tools.flux2_klein.download.state_unknown",
            "cleaning.tools.flux2_klein.download.plan_unknown_status",
            "cleaning.tools.flux2_klein.download.derived_paths_label",
            "cleaning.tools.flux2_klein.source_mode_label",
            "cleaning.tools.flux2_klein.source_mode_manual",
            "cleaning.tools.flux2_klein.source_mode_download",
            "cleaning.tools.flux2_klein.source_mode_manual_hint",
            "cleaning.tools.flux2_klein.source_mode_download_hint",
            "hf_token.empty_error",
        ];
        for (tag, source) in ms_i18n::embedded_locales() {
            let catalog: Value = serde_json::from_str(source)
                .unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            let entry = |key: &str| -> String {
                catalog
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("locale `{tag}` lacks the key `{key}`"))
                    .to_owned()
            };
            for key in plain {
                assert!(
                    !entry(key).trim().is_empty(),
                    "locale `{tag}`: `{key}` is empty"
                );
            }
            for (key, placeholders) in &interpolated {
                let text = entry(key);
                for placeholder in placeholders {
                    assert!(
                        text.contains(placeholder),
                        "locale `{tag}`: `{key}` must carry `{placeholder}`, `{text}` does not"
                    );
                }
            }
            // The two modes are a switch: identical captions would make it unusable.
            assert_ne!(
                entry("cleaning.tools.flux2_klein.source_mode_manual"),
                entry("cleaning.tools.flux2_klein.source_mode_download"),
                "locale `{tag}`: the two source modes share a caption"
            );
            // The two engines sit side by side in the SAME picker row, so identical
            // captions would leave the user unable to tell which checkpoint he picked.
            assert_ne!(
                entry("cleaning.tools.flux2_klein.title"),
                entry("cleaning.tools.flux2_klein.title_4b"),
                "locale `{tag}`: the two FLUX.2 klein engines share a picker caption"
            );
            // Six verdicts drawn with the same words would make the block unreadable, and
            // three of them differ only in what the user is supposed to do next.
            let mut wordings = Vec::new();
            for suffix in [
                "ok",
                "no_token",
                "invalid_token",
                "not_accepted",
                "not_found",
                "network_error",
            ] {
                let text = entry(&format!("cleaning.tools.flux2_klein.download.state_{suffix}"));
                assert!(
                    !wordings.contains(&text),
                    "locale `{tag}`: two download states share the wording `{text}`"
                );
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
                "download_tooltip",
                "download_disabled_tooltip",
                "cancel_tooltip",
                "cancel_disabled_tooltip",
            ] {
                let key = format!("cleaning.tools.flux2_klein.download.{key}");
                assert!(
                    !entry(&key).trim().is_empty(),
                    "locale `{tag}`: `{key}` is empty"
                );
            }
        }
    }
}
