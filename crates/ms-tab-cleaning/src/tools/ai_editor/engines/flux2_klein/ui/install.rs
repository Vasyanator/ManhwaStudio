/*
File: cleaning/tools/ai_editor/engines/flux2_klein/ui/install.rs

Purpose:
The «Установка модели» half of the panel: where the model comes from, where its three
paths point, and the catalog download that fills them in.

Main responsibilities:
- draw the source-mode switch, the one control that decides whether the paths are typed
  or derived (`draw_source_mode_switch`);
- draw an editable model-path row with its browse buttons (`Flux2PathRow`,
  `draw_path_row`) and the read-only derived paths of the download mode
  (`draw_derived_paths`);
- draw the CATALOG download block of download mode, both variants
  (`Flux2CatalogDownloadView` / `Flux2CatalogDownloadOut`, `draw_catalog_download_block`): the
  «Расцензуренный энкодер» toggle where the variant offers it with the line a format
  adjustment leaves (`draw_encoder_source_toggle`), the two weight-format pickers
  (`draw_weight_format_combo`), the derived paths, the Hugging Face token row while a gated
  spec is missing (the shared `draw_hf_token_row`), and one pinned bundle download over the
  shared `draw_ocr_model_download` (which also draws the per-repository access lines).

Key structures:
- `Flux2CatalogDownloadView`, `Flux2CatalogDownloadOut`, `Flux2PathRow`

Key functions:
- `draw_source_mode_switch()`, `draw_path_row()`, `draw_derived_paths()`
- `draw_catalog_download_block()`, `draw_encoder_source_toggle()`, `draw_weight_format_combo()`

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
/// Read-only because nothing here is a choice: the paths follow the models directory, the
/// weight formats and the encoder toggle. They are shown anyway because the user still has to
/// see where the files went and whether the install is complete, and making him switch modes
/// to find that out would be the same as not showing it.
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

/// Everything the CATALOG download block reads (download mode, either variant) and the token
/// field it edits.
pub(super) struct Flux2CatalogDownloadView<'a> {
    /// The controller's view of the bundle the selection needed when the frame began; `None`
    /// without a controller (a test build). Drawn only while its `bundle_key` still matches.
    pub(super) snapshot: Option<&'a OcrModelPanelSnapshot>,
    /// The last `.status` answer, for the presence marks beside the derived paths.
    pub(super) status: Option<&'a Flux2Status>,
    /// The gated repositories of the bundle's specs not known to be installed
    /// ([`catalog_gated_repos_missing`]): the token row is drawn while this is non-empty.
    pub(super) gated_repos: &'a [&'static str],
    pub(super) token_state: ms_sysprobe::hf_token::HfTokenState,
    /// The masked field's buffer. Never persisted and never logged.
    pub(super) token_input: &'a mut String,
    pub(super) token_status: Option<&'a str>,
    pub(super) token_busy: bool,
}

/// What the catalog block's controls did this frame, for the engine to fold back.
pub(super) struct Flux2CatalogDownloadOut<'a> {
    /// A persisted value changed (a format, the encoder source): saved and re-armed.
    pub(super) changed: &'a mut bool,
    /// The bundle block's click (download / verify / cancel).
    pub(super) action: &'a mut ModelDownloadAction,
    /// The token row's click (save / delete).
    pub(super) hf_token_action: &'a mut Option<HfTokenAction>,
    /// The line under the encoder toggle: set when a source switch changed the stored encoder
    /// format, cleared by a switch that kept it.
    pub(super) encoder_format_notice: &'a mut Option<String>,
}

/// Draws the catalog download block of download mode, both variants: the «Расцензуренный
/// энкодер» toggle where the variant offers that source, «Веса трансформера» and «Веса
/// текстового энкодера» (each a `WheelComboBox` of the offered formats with size and installed
/// mark; the encoder's list is the selected source's), the derived paths they lead to, the
/// Hugging Face token row while a gated spec of the bundle is not installed, and ONE pinned
/// download block for the whole bundle (`draw_ocr_model_download`, the SDXL pattern: download
/// / resume / verify / cancel, the per-repository access lines of its gated sources, and the
/// automatic adoption of an unmarked existing download shown as its check).
///
/// The app fetches the pinned files itself; no backend method is involved. A changed format or
/// source raises `out.changed` (the engine saves it and re-arms the status, the forecast, the
/// installed formats and the bundle probe). `out.action` receives the block's click, applied by
/// the engine to the CURRENT bundle; the block is skipped for the one frame in which the
/// controls have just moved the bundle away from `view.snapshot`.
///
/// The token never reaches this function's own state: it lives in the process-wide `hf_token`
/// module, `token_input` is only the buffer of the entry field, and no branch here prints,
/// logs or formats a token value.
pub(super) fn draw_catalog_download_block(ui: &mut egui::Ui, settings: &mut Flux2KleinSettings, view: Flux2CatalogDownloadView<'_>, out: Flux2CatalogDownloadOut<'_>) {
    ui.separator();
    ui.label(t!("cleaning.tools.flux2_klein.download.heading"));
    draw_encoder_source_toggle(ui, settings, out.changed, out.encoder_format_notice);
    for axis in Flux2WeightAxis::all() {
        draw_weight_format_combo(ui, settings, axis, out.changed);
    }
    // After the combos: the transformer and encoder paths follow the formats.
    draw_derived_paths(ui, settings, view.status);
    // The token is needed only to fetch from a gated repository; once those specs are
    // installed (or for 4B, whose repositories are open) the row would ask for nothing. The
    // per-repository access verdicts and the repository links are the bundle block's own
    // lines below, so no second list is drawn here.
    if !view.gated_repos.is_empty() {
        draw_hf_token_row(
            ui,
            HfTokenView { state: view.token_state, input: view.token_input, status: view.token_status, busy: view.token_busy },
            "cleaning_flux2_klein_hf_token",
            None,
            out.hf_token_action,
        );
    }
    let current = bundle_key(&flux2_catalog_bundle(settings));
    if let Some(snapshot) = view.snapshot.filter(|snapshot| snapshot.bundle_key == current) {
        let clicked = draw_ocr_model_download(ui, snapshot);
        if clicked != ModelDownloadAction::None {
            *out.action = clicked;
        }
    }
}

/// Draws «Расцензуренный энкодер» where the variant offers more than one encoder source
/// (`Flux2EncoderSource::offered_for`; 4B has none, so nothing is drawn there), and under it
/// the line a format adjustment left. A switch stores the source and maps the encoder format
/// through `closest_offered` (`Flux2KleinSettings::set_encoder_source`); when that changed the
/// format the line names both, otherwise the line is cleared.
///
/// A plain checkbox: the toggle's `Id` does not hold state, so its localized caption needs no
/// salt (`egui-docs/05-ids-and-i18n.md` §2).
fn draw_encoder_source_toggle(ui: &mut egui::Ui, settings: &mut Flux2KleinSettings, changed: &mut bool, notice: &mut Option<String>) {
    if Flux2EncoderSource::offered_for(settings.variant()).len() > 1 {
        let mut uncensored = settings.encoder_source() == Flux2EncoderSource::Uncensored;
        let toggled = ui
            .checkbox(&mut uncensored, t!("cleaning.tools.flux2_klein.download.uncensored_label"))
            .on_hover_text(t!("cleaning.tools.flux2_klein.download.uncensored_encoder_hint"))
            .changed();
        if toggled {
            let previous = settings.encoder_source();
            let source = if uncensored { Flux2EncoderSource::Uncensored } else { Flux2EncoderSource::Stock };
            *notice = settings.set_encoder_source(source).map(|(from, to)| {
                tf!(
                    "cleaning.tools.flux2_klein.text_encoder_format_adjusted_status",
                    from = flux2_encoder_format_token(previous, from),
                    to = flux2_encoder_format_token(source, to)
                )
            });
            *changed = true;
        }
    }
    if let Some(line) = notice.as_deref() {
        ui.colored_label(ms_theme::status::WARNING, line);
    }
}

/// Draws one weight-format picker: its caption (hover: what the axis choice means) and a
/// `WheelComboBox` of the formats the variant offers on `axis` for the selected encoder source,
/// each captioned by [`flux2_weight_format_label`] and hovered by [`flux2_weight_format_hint`].
/// A pick is written into the axis's persisted wire id and raises `changed`.
///
/// The combo's `Id` comes from a fixed `id_salt` per axis, never from the localized caption
/// (`egui-docs/05-ids-and-i18n.md` §2); a `WheelComboBox` rather than `egui::ComboBox`
/// (`egui-docs/04-widgets.md` §0.2).
fn draw_weight_format_combo(ui: &mut egui::Ui, settings: &mut Flux2KleinSettings, axis: Flux2WeightAxis, changed: &mut bool) {
    let variant = settings.variant();
    let source = settings.encoder_source();
    let selected = settings.weight_format(axis);
    let (label, hint, id_salt) = match axis {
        Flux2WeightAxis::Transformer => (
            t!("cleaning.tools.flux2_klein.transformer_format_label"),
            t!("cleaning.tools.flux2_klein.transformer_format_hint"),
            "cleaning_flux2_klein_transformer_format",
        ),
        Flux2WeightAxis::TextEncoder => (
            t!("cleaning.tools.flux2_klein.text_encoder_format_label"),
            t!("cleaning.tools.flux2_klein.text_encoder_format_hint"),
            "cleaning_flux2_klein_text_encoder_format",
        ),
    };
    let installed = &settings.installed_formats;
    let mut picked = selected;
    ui.horizontal_wrapped(|ui| {
        ui.label(label).on_hover_text(hint);
        WheelComboBox::from_id_salt(id_salt)
            .selected_text(flux2_weight_format_label(variant, axis, source, selected, installed))
            .show_ui(ui, |ui| {
                for &format in Flux2WeightFormat::offered_for(variant, axis, source) {
                    ui.selectable_value(&mut picked, format, flux2_weight_format_label(variant, axis, source, format, installed))
                        .on_hover_text(flux2_weight_format_hint(axis, format));
                }
            })
            .response
            .on_hover_text(hint);
    });
    if picked != selected {
        let stored = match axis {
            Flux2WeightAxis::Transformer => &mut settings.transformer_format,
            Flux2WeightAxis::TextEncoder => &mut settings.text_encoder_format,
        };
        *stored = picked.wire().to_string();
        *changed = true;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads one embedded catalog as a JSON object.
    fn catalog(tag: &str, source: &str) -> Value {
        serde_json::from_str(source).unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"))
    }

    /// The FLUX.2 keys of this block (the shared ones are pinned in
    /// `../../shared/hf_download_ui.rs`, `stream_progress_ui.rs` and `catalog_download.rs`) that every shipped locale
    /// carries.
    #[test]
    fn every_catalog_carries_the_download_block_strings() {
        let plain = [
            "cleaning.tools.flux2_klein.pipeline_busy_error",
            "cleaning.tools.flux2_klein.download.heading",
            "cleaning.tools.flux2_klein.download.uncensored_label",
            "cleaning.tools.flux2_klein.variant_residency_hint",
            "cleaning.tools.flux2_klein.title",
            "cleaning.tools.flux2_klein.title_4b",
            "cleaning.tools.flux2_klein.download.derived_paths_label",
            "cleaning.tools.flux2_klein.source_mode_label",
            "cleaning.tools.flux2_klein.source_mode_manual",
            "cleaning.tools.flux2_klein.source_mode_download",
            "cleaning.tools.flux2_klein.source_mode_manual_hint",
            "cleaning.tools.flux2_klein.source_mode_download_hint",
        ];
        for (tag, source) in ms_i18n::embedded_locales() {
            let catalog = catalog(tag, source);
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

    /// The catalog half of the block: the two pickers, the row captions and hovers, the line a
    /// source switch leaves, the readiness line of an incomplete bundle and the fp8 checkbox a
    /// GGUF encoder closes. The two catalogs maintained by hand (`en`, the reference, and
    /// `ru`) must carry them; `es` / `fr` / `pt` fall back to `en`. The retired backend-download
    /// keys and the two switch-block keys moved to the shared catalog block are gone from both.
    #[test]
    fn the_reference_catalogs_carry_the_weight_format_keys() {
        let plain = [
            "cleaning.tools.flux2_klein.transformer_format_label",
            "cleaning.tools.flux2_klein.transformer_format_hint",
            "cleaning.tools.flux2_klein.text_encoder_format_label",
            "cleaning.tools.flux2_klein.text_encoder_format_hint",
            "cleaning.tools.flux2_klein.format_bf16_hint",
            "cleaning.tools.flux2_klein.format_gguf_transformer_hint",
            "cleaning.tools.flux2_klein.format_gguf_encoder_hint",
            "cleaning.tools.flux2_klein.text_encoder_fp8_gguf_disabled_hint",
            "cleaning.tools.flux2_klein.bundle_missing_status",
            // Reworded under a new id (was `download.uncensored_hint`, retired below); es / fr
            // / pt fall back to en.
            "cleaning.tools.flux2_klein.download.uncensored_encoder_hint",
        ];
        // A format row is only a choice while it names the format and its size; the switch
        // line names both formats.
        let interpolated: [(&str, &[&str]); 3] = [
            ("cleaning.tools.flux2_klein.format_unquantized_label", &["{dtype}", "{size}"]),
            ("cleaning.tools.flux2_klein.format_gguf_label", &["{quant}", "{size}"]),
            ("cleaning.tools.flux2_klein.text_encoder_format_adjusted_status", &["{from}", "{to}"]),
        ];
        let retired = [
            "cleaning.tools.flux2_klein.format_bf16_label",
            "cleaning.tools.flux2_klein.download.hint",
            "cleaning.tools.flux2_klein.download.repo_status",
            "cleaning.tools.flux2_klein.download.token_not_needed_hint",
            "cleaning.tools.flux2_klein.download.token_unexpected_hint",
            "cleaning.tools.flux2_klein.download.variant_mismatch_error",
            "cleaning.tools.flux2_klein.download.switch_blocked_check",
            // Moved to the shared `cleaning.tools.catalog_download.*` (pinned in
            // `../../shared/catalog_download.rs`).
            "cleaning.tools.flux2_klein.download.switch_blocked_download",
            "cleaning.tools.flux2_klein.download.switch_blocked_verify",
            "cleaning.tools.flux2_klein.download.running_status",
            "cleaning.tools.flux2_klein.download.cancelled_status",
            "cleaning.tools.flux2_klein.download.done_status",
            "cleaning.tools.flux2_klein.download.error",
            "cleaning.tools.flux2_klein.download.download_tooltip",
            "cleaning.tools.flux2_klein.download.uncensored_hint",
        ];
        for (tag, source) in ms_i18n::embedded_locales().iter().filter(|(tag, _)| matches!(*tag, "en" | "ru")) {
            let catalog = catalog(tag, source);
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
            for (key, placeholders) in interpolated {
                let text = entry(key);
                for placeholder in placeholders {
                    assert!(text.contains(placeholder), "locale `{tag}`: `{key}` must carry `{placeholder}`, `{text}` does not");
                }
            }
            for key in retired {
                assert!(catalog.get(key).is_none(), "locale `{tag}`: the retired `{key}` is still there");
            }
            // The two pickers sit one under the other: identical captions would hide which
            // component a choice is for.
            assert_ne!(
                entry("cleaning.tools.flux2_klein.transformer_format_label"),
                entry("cleaning.tools.flux2_klein.text_encoder_format_label"),
                "locale `{tag}`: the two weight-format pickers share a caption"
            );
        }
    }
}
