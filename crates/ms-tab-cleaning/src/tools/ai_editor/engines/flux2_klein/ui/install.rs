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
- draw the download block — the heading and repository line, the token row
  (`draw_flux2_token_row` over the shared `draw_hf_token_row`, unconditional for a gated
  repository and reopened on an ungated one whose check blamed the token), the encoder
  toggle, the derived paths, and the shared access-check / plan / download controls
  (`draw_hf_download_controls`) (`Flux2DownloadView`, `draw_download_block`).

Key structures:
- `Flux2DownloadView`, `Flux2PathRow`

Key functions:
- `draw_source_mode_switch()`, `draw_path_row()`, `draw_derived_paths()`
- `draw_download_block()`, `draw_flux2_token_row()`

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
/// idiom the engine picker already uses (`region_edit_v2/host_panels.rs::draw_engine_picker`) —
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
                    ms_theme::status::SUCCESS
                } else {
                    ms_theme::status::WARNING
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
    hf_token_action: &mut Option<HfTokenAction>,
    download_action: &mut Option<HfDownloadAction>,
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
    // The access hint names the uncensored encoder's gated repository only while that toggle
    // is in effect, i.e. while the download would actually fetch from it.
    let uncensored = settings.uncensored_encoder_active();
    // Always drawn for a variant whose repository is gated. For an ungated one it appears
    // only when the last check actually BLAMED the token: the 4B checkpoint is apache-2.0,
    // the request carries an empty token legally, and an unconditional field there would
    // ask the user to solve a problem they do not have. A verdict of `no_token` or
    // `invalid_token` means they DO have it — and `hf_token` has exactly one UI surface in
    // the whole application, this block, so leaving the row out there strands them with a
    // dead «Скачать» and a credential they cannot reach.
    if view.variant.requires_hf_token() {
        draw_flux2_token_row(ui, &mut view, uncensored, hf_token_action);
    } else if download_check_blames_token(view.check) {
        // Only reachable from a backend that authenticates against this repository even
        // though it is open — an outdated one. Both halves are said: what the answer means,
        // and the controls that make it actionable.
        ui.colored_label(
            ms_theme::status::WARNING,
            t!("cleaning.tools.flux2_klein.download.token_unexpected_hint"),
        );
        draw_flux2_token_row(ui, &mut view, uncensored, hf_token_action);
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

    // --- the access check and the download (shared) ---------------------------------
    draw_hf_download_controls(
        ui,
        &HfDownloadControlsView {
            check: view.check,
            check_error: view.check_error,
            check_busy: view.check_busy,
            download_busy: view.download_busy,
            download_status: view.download_status,
            ai_backend_available: view.ai_backend_available,
            pipeline_busy: view.pipeline_busy,
            download_tooltip: t!("cleaning.tools.flux2_klein.download.download_tooltip"),
        },
        download_action,
    );
}

/// Draws the Hugging Face token row of this block through the shared `draw_hf_token_row`.
///
/// Split out because it has TWO callers with different conditions — a gated repository,
/// which always needs it, and an ungated one whose check nevertheless blamed the token —
/// and duplicating it would let the two drift into offering different controls for the
/// same credential. The field keeps its historic id (`cleaning_flux2_klein_hf_token`); the
/// per-variant `push_id` around the whole body keeps the two engines apart.
///
/// The row carries the step-by-step access hint (`ms_widgets::HfAccessHint`) for the
/// repositories this download needs: the variant's own model repository, plus the gated
/// uncensored encoder's while that toggle is in effect (`uncensored`).
fn draw_flux2_token_row(
    ui: &mut egui::Ui,
    view: &mut Flux2DownloadView<'_>,
    uncensored: bool,
    hf_token_action: &mut Option<HfTokenAction>,
) {
    let variant = view.variant;
    let mut hint = HfAccessHint::new("cleaning_flux2_klein_hf_access_hint", token_save_button_label())
        .repo(variant.title(), variant.model_repo());
    if uncensored && let Some(repo) = variant.uncensored_encoder_repo() {
        hint = hint.repo(t!("cleaning.tools.flux2_klein.download.uncensored_label"), repo);
    }
    draw_hf_token_row(
        ui,
        HfTokenView {
            state: view.token_state,
            input: &mut *view.token_input,
            status: view.token_status,
            busy: view.token_busy,
        },
        "cleaning_flux2_klein_hf_token",
        Some(hint),
        hf_token_action,
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The FLUX.2 keys of this block (the shared ones are pinned in
    /// `../../shared/hf_download_ui.rs` and `stream_progress_ui.rs`).
    #[test]
    fn every_catalog_carries_the_download_block_strings() {
        let interpolated = [
            ("cleaning.tools.flux2_klein.download.error", vec!["{err}"]),
            ("cleaning.tools.flux2_klein.download.done_status", vec!["{size}", "{skipped}"]),
            // Without `{repo}` the line would claim to name a repository and name none,
            // which is worse than not drawing it: the two variants differ by exactly that
            // id before any check has run.
            ("cleaning.tools.flux2_klein.download.repo_status", vec!["{repo}"]),
            // Both checkpoints are named, or the sentence cannot say what went wrong.
            ("cleaning.tools.flux2_klein.download.variant_mismatch_error", vec!["{expected}", "{actual}"]),
        ];
        let plain = [
            "cleaning.tools.flux2_klein.pipeline_busy_error",
            "cleaning.tools.flux2_klein.download.heading",
            "cleaning.tools.flux2_klein.download.hint",
            "cleaning.tools.flux2_klein.download.download_tooltip",
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
            "cleaning.tools.flux2_klein.download.running_status",
            "cleaning.tools.flux2_klein.download.cancelled_status",
            "cleaning.tools.flux2_klein.download.derived_paths_label",
            "cleaning.tools.flux2_klein.source_mode_label",
            "cleaning.tools.flux2_klein.source_mode_manual",
            "cleaning.tools.flux2_klein.source_mode_download",
            "cleaning.tools.flux2_klein.source_mode_manual_hint",
            "cleaning.tools.flux2_klein.source_mode_download_hint",
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
        }
    }
}
