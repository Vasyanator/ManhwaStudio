/*
File: tabs/mod.rs

Purpose:
Root of the project-editor tab modules and the shared `AppTab` selector enum.

Key structures:
- AppTab: the set of editor tabs, with a stable persistence `key()` and a
  localized display `title()`.

Notes:
`key()` and `title()` are deliberately split (see `docs/i18n_exclusions.md` B1):
`key()` is the byte-stable English identifier used for persistence and must never
change with the UI language, while `title()` is a localized label for display only.
*/

pub mod characters;
pub mod cleaning;
pub mod notes;
pub mod ps_editor;
pub mod settings;
pub mod terms;
pub mod translation;
pub mod typing;
pub mod wiki;

/// The editor tabs. `key()` is the persistence identifier; `title()` is the
/// localized display label. These two are intentionally distinct (see the note
/// on `key()`).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AppTab {
    Translation,
    Cleaning,
    Typing,
    PsEditor,
    Characters,
    Terms,
    Notes,
    Settings,
    Wiki,
}

impl AppTab {
    pub const ALL: [AppTab; 9] = [
        AppTab::Translation,
        AppTab::Cleaning,
        AppTab::Typing,
        AppTab::PsEditor,
        AppTab::Characters,
        AppTab::Terms,
        AppTab::Notes,
        AppTab::Settings,
        AppTab::Wiki,
    ];

    /// Stable English identifier for this tab and the PERSISTENCE contract.
    ///
    /// These ids are the object keys of `General.enabled_tabs` in
    /// `user_config.json` and must stay byte-stable across releases AND across UI
    /// languages. Unlike `title()` (a localized display label), `key()` never
    /// changes with the interface language, so persisted data never depends on
    /// the chrome language. The `match` is exhaustive with no catch-all: adding a
    /// variant forces this contract to be reconsidered.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            AppTab::Translation => "translation",
            AppTab::Cleaning => "cleaning",
            AppTab::Typing => "typing",
            AppTab::PsEditor => "ps_editor",
            AppTab::Characters => "characters",
            AppTab::Terms => "terms",
            AppTab::Notes => "notes",
            AppTab::Settings => "settings",
            AppTab::Wiki => "wiki",
        }
    }

    /// Localized display label for this tab (interface language).
    ///
    /// Display only — never persisted or compared. Use `key()` for anything that
    /// must round-trip to disk or stay language-independent.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            AppTab::Translation => t!("app.tab.translation"),
            AppTab::Cleaning => t!("app.tab.cleaning"),
            AppTab::Typing => t!("app.tab.typing"),
            AppTab::PsEditor => t!("app.tab.ps_editor"),
            AppTab::Characters => t!("app.tab.characters"),
            AppTab::Terms => t!("app.tab.terms"),
            AppTab::Notes => t!("app.tab.notes"),
            AppTab::Settings => t!("app.tab.settings"),
            AppTab::Wiki => t!("app.tab.wiki"),
        }
    }
}
