/*
File: src/launcher/first_run_language.rs

Purpose:
First-run language-selection MODAL for the Rust launcher. It is shown once, on the
very first launch where the user never chose an interface language and/or a
typesetting ("тайп") language, and lets them pick both before anything else. The
system locale is detected and pre-selected; NOTHING is persisted until the user
presses the confirm button.

Why a modal here:
The launcher is the first screen a fresh install shows, so it is the natural place
to ask for the two independent languages the app uses — the UI language
(`General.ui_language`, `ms-i18n`) and the typesetting language
(`TextTab.text_language`, `ms_text_util::language`). Both persistence paths and the
locale option scan are REUSED from `general_settings_panel` so this surface and the
settings pane can never drift.

Gating:
The modal is constructed only when the persisted tri-state marker
`General.first_run_languages_confirmed` is present and `false`
(`config::user_settings_first_run_languages_pending`). It CANNOT gate on the language
keys being absent: the native startup flow persists the full defaults tree (via
`load_user_config`) before the launcher constructs, materializing
`General.ui_language` and `TextTab.text_language`, so by modal time both keys always
look present. The marker is instead written once, before any defaults-persisting call,
by `config::mark_first_run_languages_if_needed`. A missing marker means the feature
never triggered (existing/pre-feature install) and the modal never shows; `true` means
already confirmed. While the modal is present the launcher's main-menu tutorial autoplay
is suppressed (they are mutually exclusive), and the tutorial is handed off exactly once
on confirm.

Blocking:
Input is blocked by z-order occlusion (the tutorial-engine pattern, see
`egui-docs/06-overlays.md`): a full-viewport `Area` on `Order::Middle` paints a dim
scrim and allocates one `Sense::click_and_drag` rect to absorb everything beneath;
the interactive card sits on `Order::Foreground` above it.

Key items:
- `FirstRunLanguageState`: modal state (locale options, selected UI tag, selected
  typesetting language, transient persist-error line).
- `FirstRunLanguageState::from_startup`: need-detection + preselection + wiring.
- `FirstRunLanguageState::show`: renders the modal and returns whether it was
  confirmed (the caller then drops the state and hands off to the tutorial).
- `primary_subtag` / `preselect_ui_tag` / `preselect_text_language`: pure,
  OS-free selection cores (unit-tested without touching the locale catalog).
*/

use crate::config;
use crate::general_settings_panel::{self, LocaleOption};
use crate::i18n_resolve::resolve_key;
use crate::launcher::theme;
use crate::runtime_log;
use eframe::egui;
use ms_text_util::language::{ScriptGroup, TextLanguage, set_text_language};
use serde_json::Value;

/// Interface-language fallback tag when nothing else matches. English is the
/// project's reference / error-path locale (Russian is only the shipped config
/// default, not the detection fallback — see `locale_store.rs`).
const FALLBACK_UI_TAG: &str = "en";

/// Stable egui `Id` for the full-viewport input-blocking scrim area.
const BLOCKER_AREA_ID: &str = "launcher_first_run_language_blocker";
/// Stable egui `Id` for the centered interactive content area.
const CONTENT_AREA_ID: &str = "launcher_first_run_language_modal";
/// Stable `id_salt` for the content `ScrollArea` (localized labels inside must not
/// derive the scroll id, or its offset would reset on a language switch).
const CONTENT_SCROLL_ID: &str = "launcher_first_run_language_scroll";

/// Dim scrim painted over the whole viewport behind the modal card. Slightly
/// stronger than `theme::VEIL_TINT` so the card reads as a true modal, not a page.
const SCRIM_TINT: egui::Color32 = egui::Color32::from_rgba_premultiplied(0, 0, 0, 168);

/// Error-status colour for a failed confirm-time persist (matches the launcher's
/// settings error colour). A colour literal, not a user-visible string.
const ERROR_COLOR: egui::Color32 = egui::Color32::from_rgb(208, 84, 62);

/// Opaque card fill for the modal. `theme::CARD_FILL` is deliberately translucent
/// (the menu card floats over the wallpaper), but a modal must not let the page
/// beneath show through — same base colour, full alpha.
const MODAL_CARD_FILL: egui::Color32 = egui::Color32::from_rgb(24, 24, 28);

/// State of the launcher's first-run language modal.
///
/// Held as `Option<FirstRunLanguageState>` on `LauncherApp`: `Some` while the modal
/// must be shown, `None` once confirmed or when it was never needed. Selections are
/// scratch until the user confirms; only then are they persisted.
#[derive(Debug)]
pub struct FirstRunLanguageState {
    /// Interface-language options, scanned ONCE at construction from the on-disk
    /// `locale/` folder + embedded catalogs (reused from `general_settings_panel`).
    locale_options: Vec<LocaleOption>,
    /// Currently selected interface-language tag (an option's `tag`). Live-installed
    /// on change so the modal itself re-localizes; persisted only on confirm.
    selected_ui_tag: String,
    /// Currently selected typesetting language. Applied to the process-global and
    /// persisted only on confirm (no side effect while merely selected).
    selected_text_language: TextLanguage,
    /// Transient error line shown when a confirm-time persist failed; the modal
    /// stays open so the user can retry.
    error: Option<String>,
}

impl FirstRunLanguageState {
    /// Builds the modal state from the RAW (unmerged) startup settings, or returns
    /// `None` when it is not needed.
    ///
    /// Returns `None` unless the persisted first-run marker
    /// `General.first_run_languages_confirmed` is present and `false`
    /// (`config::user_settings_first_run_languages_pending`). The key-presence of the
    /// language values cannot be used here: the startup flow persists the defaults tree
    /// (materializing both language keys) before the launcher constructs, so by this
    /// point they always look present — see the file header's Gating section.
    ///
    /// When pending, scans the locale options, detects the system locale, preselects
    /// BOTH languages purely from that locale, and live-installs the preselected
    /// interface locale so the modal renders in the detected language (the startup path
    /// installed only the shipped default). Nothing is persisted here.
    ///
    /// `raw` must be the config from `config::load_raw_user_settings_for_startup`; the
    /// marker is absent from the defaults tree, so the merged startup settings carry
    /// the same value.
    #[must_use]
    pub fn from_startup(raw: &Value) -> Option<Self> {
        if !config::user_settings_first_run_languages_pending(raw) {
            return None;
        }

        let locale_options = general_settings_panel::scan_locale_options();
        let system = sys_locale::get_locale();
        let system_ref = system.as_deref();

        let selected_ui_tag = preselect_ui_tag(system_ref, &locale_options);
        let selected_text_language = preselect_text_language(system_ref);

        // The startup path installed only the shipped default locale before the launcher
        // constructed; live-install the detected locale so the modal renders in it.
        general_settings_panel::install_selected_ui_locale(&selected_ui_tag);

        Some(Self {
            locale_options,
            selected_ui_tag,
            selected_text_language,
            error: None,
        })
    }

    /// Renders the modal for one frame and returns whether the user confirmed.
    ///
    /// A `true` return means the selections were persisted successfully; the caller
    /// must drop this state (`None`) and hand off to the main-menu tutorial. A
    /// `false` return means the modal is still open (either not confirmed, or a
    /// confirm-time persist failed and the error line is now shown).
    ///
    /// Input beneath the modal is fully blocked (z-order occlusion). Selecting a
    /// different interface language live-installs it so the whole modal re-localizes
    /// next frame; the typesetting selection changes state only.
    #[must_use]
    pub fn show(&mut self, ctx: &egui::Context) -> bool {
        let screen = ctx.viewport_rect();

        // Full-viewport blocker on `Order::Middle`: one dim rect + one sensed rect
        // that covers the pointer's search area everywhere, so egui's hit-test drops
        // every lower-layer widget from both the click target and the hover set.
        egui::Area::new(egui::Id::new(BLOCKER_AREA_ID))
            .order(egui::Order::Middle)
            .fixed_pos(screen.min)
            .constrain(false)
            .movable(false)
            .interactable(true)
            .show(ctx, |ui| {
                ui.set_clip_rect(screen);
                ui.painter().rect_filled(screen, 0.0, SCRIM_TINT);
                ui.allocate_rect(screen, egui::Sense::click_and_drag());
            });

        let previous_ui_tag = self.selected_ui_tag.clone();
        let mut confirm_clicked = false;
        // Keep the sections from overflowing a short viewport; the rest of the card
        // (title, explanation, confirm button) stays fixed above/below.
        let scroll_max_height = (screen.height() * 0.58).max(160.0);

        // Interactive card on `Order::Foreground` — above the blocker, so its
        // widgets remain clickable while everything below is inert.
        egui::Area::new(egui::Id::new(CONTENT_AREA_ID))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .constrain(true)
            .movable(false)
            .show(ctx, |ui| {
                theme::card_frame().fill(MODAL_CARD_FILL).show(ui, |ui| {
                    // Fix the card width so wrapped radio rows lay out predictably.
                    ui.set_width(520.0);
                    ui.label(theme::hero_title(t!("launcher.first_run_language.title")));
                    ui.add_space(4.0);
                    ui.label(theme::status(
                        t!("launcher.first_run_language.explanation"),
                        theme::TEXT_MUTED,
                    ));
                    ui.add_space(12.0);
                    egui::ScrollArea::vertical()
                        .id_salt(CONTENT_SCROLL_ID)
                        .max_height(scroll_max_height)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            self.draw_sections(ui);
                        });
                    ui.add_space(12.0);
                    if let Some(error) = &self.error {
                        ui.colored_label(ERROR_COLOR, error);
                        ui.add_space(6.0);
                    }
                    let button_size = egui::vec2(ui.available_width(), 44.0);
                    // Stable salt for the confirm button: its caption is localized, so
                    // pin the id to a literal key rather than let it follow the label.
                    let confirm_response = ui
                        .push_id("first_run_confirm", |ui| {
                            theme::launcher_button(
                                ui,
                                t!("launcher.first_run_language.confirm_button"),
                                button_size,
                                true,
                            )
                        })
                        .inner;
                    if confirm_response.clicked() {
                        confirm_clicked = true;
                    }
                });
            });

        // A changed interface language is live-installed (not persisted) so the
        // whole modal re-localizes on the next frame.
        if self.selected_ui_tag != previous_ui_tag {
            general_settings_panel::install_selected_ui_locale(&self.selected_ui_tag);
            ctx.request_repaint();
        }

        if confirm_clicked {
            return self.confirm();
        }
        false
    }

    /// Draws the two language sections (interface + typesetting) into the scroll
    /// area, updating the selections in place. Selection uses radio toggles per the
    /// product requirement (never dropdowns).
    fn draw_sections(&mut self, ui: &mut egui::Ui) {
        // Interface-language section: one radio per locale option. The label is the
        // endonym (`_meta.name`), which intentionally does NOT go through `t!`.
        ui.label(egui::RichText::new(t!("launcher.first_run_language.interface_section_label")).strong());
        ui.add_space(4.0);
        let mut picked_ui: Option<String> = None;
        for option in &self.locale_options {
            let checked = self.selected_ui_tag == option.tag;
            // Salt each radio with its stable tag (not the endonym label) so the
            // widget id never depends on any translated/localized text — uniform with
            // the typesetting radios below and collision-free across languages.
            let clicked = ui
                .push_id(option.tag.as_str(), |ui| {
                    ui.radio(checked, option.display.as_str()).clicked()
                })
                .inner;
            if clicked {
                picked_ui = Some(option.tag.clone());
            }
        }
        if let Some(tag) = picked_ui {
            self.selected_ui_tag = tag;
        }

        ui.separator();

        // Typesetting-language section: the 13 languages grouped under their 4
        // script-group subheaders. Both label sets are runtime-resolved catalog keys
        // (`resolve_key`), enumerated via the enums' `all()`/`languages()` so a new
        // variant is picked up without a hand-written match.
        ui.label(egui::RichText::new(t!("launcher.first_run_language.typesetting_section_label")).strong());
        ui.add_space(4.0);
        let mut picked_text: Option<TextLanguage> = None;
        for group in ScriptGroup::all() {
            ui.label(egui::RichText::new(resolve_key(group.name_key())).color(theme::TEXT_MUTED));
            ui.horizontal_wrapped(|ui| {
                for language in group.languages() {
                    let checked = self.selected_text_language == *language;
                    // Localized label ⇒ pin a stable per-language salt (the tag), or the
                    // radio id would follow the translated caption and its state would
                    // collide/reset on a language switch (see egui-docs/05-ids-and-i18n).
                    let clicked = ui
                        .push_id(language.tag(), |ui| {
                            ui.radio(checked, resolve_key(language.name_key())).clicked()
                        })
                        .inner;
                    if clicked {
                        picked_text = Some(*language);
                    }
                }
            });
        }
        if let Some(language) = picked_text {
            self.selected_text_language = language;
        }
    }

    /// Persists both selections plus the confirm marker atomically and reports whether
    /// the modal may close.
    ///
    /// Persists `General.ui_language`, `TextTab.text_language` and
    /// `General.first_run_languages_confirmed = true` in ONE
    /// `general_settings_panel::persist_config_keys` locked read-modify-write —
    /// synchronous tiny disk I/O on an explicit user click, the documented precedented
    /// exception to the no-GUI-I/O rule (same justification as `persist_config_keys`).
    /// Writing the marker to `true` in the same transaction guarantees the modal never
    /// reappears once the languages are saved. On success it re-installs the interface
    /// locale (idempotent) and applies the typesetting language to the process-global,
    /// then returns `true`. On a persist failure it logs, sets the error line, and
    /// returns `false`: nothing is applied and the modal stays open so the user can
    /// retry.
    fn confirm(&mut self) -> bool {
        let ui_tag = self.selected_ui_tag.clone();
        let text_tag = self.selected_text_language.tag().to_string();
        if let Err(err) = general_settings_panel::persist_config_keys(&[
            (
                "General",
                config::GENERAL_UI_LANGUAGE_KEY,
                Value::String(ui_tag.clone()),
            ),
            (
                "TextTab",
                config::TEXT_TAB_TEXT_LANGUAGE_KEY,
                Value::String(text_tag.clone()),
            ),
            (
                "General",
                config::GENERAL_FIRST_RUN_LANGUAGES_CONFIRMED_KEY,
                Value::Bool(true),
            ),
        ]) {
            runtime_log::log_error(format!(
                "[first-run-language] failed to persist languages ui='{ui_tag}' text='{text_tag}'; error={err}"
            ));
            self.error = Some(tf!("launcher.first_run_language.save_error", err = err));
            return false;
        }
        // Persist succeeded: apply both live so the saved values are the active ones.
        // The locale may already be live-installed; re-installing the persisted tag is
        // idempotent and guarantees the saved value is the active one.
        general_settings_panel::install_selected_ui_locale(&ui_tag);
        set_text_language(self.selected_text_language);

        self.error = None;
        true
    }
}

/// Extracts the lowercase primary subtag of a system locale string.
///
/// Splits on the first `-`, `_`, `.` or `@` separator (handles `"ru-RU"`,
/// `"en_US.UTF-8"`, `"pt"`, `"de@euro"`). Returns `None` for an empty string or the
/// locale-agnostic `"C"` / `"POSIX"` values, which carry no language information.
#[must_use]
fn primary_subtag(locale: &str) -> Option<String> {
    // `split` always yields at least one element; `?` avoids the forbidden
    // unwrap-family construct while keeping the empty-string case handled below.
    let first = locale.split(['-', '_', '.', '@']).next()?;
    let primary = first.trim().to_ascii_lowercase();
    if primary.is_empty() || primary == "c" || primary == "posix" {
        None
    } else {
        Some(primary)
    }
}

/// Chooses the pre-selected interface-language tag from the system locale.
///
/// The system locale's primary subtag is matched against the available `options`.
/// Falls back to [`FALLBACK_UI_TAG`] (English) when it maps to no available option.
/// The modal always preselects from the locale: by the time it is shown, any stored
/// config value is an auto-persisted default, not a user choice (see the file header).
#[must_use]
fn preselect_ui_tag(system: Option<&str>, options: &[LocaleOption]) -> String {
    if let Some(primary) = system.and_then(primary_subtag)
        && options.iter().any(|option| option.tag == primary)
    {
        return primary;
    }
    FALLBACK_UI_TAG.to_string()
}

/// Chooses the pre-selected typesetting language from the system locale.
///
/// The system locale's primary subtag is parsed. Falls back to [`TextLanguage::En`]
/// when it maps to no known language.
#[must_use]
fn preselect_text_language(system: Option<&str>) -> TextLanguage {
    if let Some(primary) = system.and_then(primary_subtag)
        && let Some(language) = TextLanguage::from_tag(&primary)
    {
        return language;
    }
    TextLanguage::En
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal option list for the preselect tests.
    fn options() -> Vec<LocaleOption> {
        ["en", "ru", "es"]
            .into_iter()
            .map(|tag| LocaleOption {
                tag: tag.to_string(),
                display: tag.to_string(),
            })
            .collect()
    }

    #[test]
    fn primary_subtag_extracts_lowercase_primary() {
        assert_eq!(primary_subtag("ru-RU").as_deref(), Some("ru"));
        assert_eq!(primary_subtag("en_US.UTF-8").as_deref(), Some("en"));
        assert_eq!(primary_subtag("pt").as_deref(), Some("pt"));
        assert_eq!(primary_subtag("de@euro").as_deref(), Some("de"));
    }

    #[test]
    fn primary_subtag_rejects_locale_agnostic_and_empty() {
        assert_eq!(primary_subtag("C"), None);
        assert_eq!(primary_subtag("POSIX"), None);
        assert_eq!(primary_subtag(""), None);
        assert_eq!(primary_subtag("   "), None);
    }

    #[test]
    fn preselect_ui_tag_matches_system_locale() {
        assert_eq!(preselect_ui_tag(Some("ru-RU"), &options()), "ru");
        assert_eq!(preselect_ui_tag(Some("es_ES.UTF-8"), &options()), "es");
    }

    #[test]
    fn preselect_ui_tag_unmatched_falls_back_to_english() {
        // System locale with no matching option, and no system locale at all.
        assert_eq!(preselect_ui_tag(Some("ja-JP"), &options()), "en");
        assert_eq!(preselect_ui_tag(None, &options()), "en");
    }

    #[test]
    fn preselect_text_language_from_system() {
        assert_eq!(preselect_text_language(Some("ru-RU")), TextLanguage::Ru);
        assert_eq!(preselect_text_language(Some("pl_PL")), TextLanguage::Pl);
    }

    #[test]
    fn preselect_text_language_unknown_falls_back_to_english() {
        assert_eq!(preselect_text_language(Some("ja-JP")), TextLanguage::En);
        assert_eq!(preselect_text_language(None), TextLanguage::En);
    }
}
