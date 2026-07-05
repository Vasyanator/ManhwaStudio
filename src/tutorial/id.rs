/*
File: src/tutorial/id.rs

Purpose:
Central, stable identity for every tutorial in the app. `TutorialId` is the
persistence key (config `Tutorials.completed`), the replay-menu enumerator, and
the catalog key a `TutorialController` maps to a step script.

Notes:
`ALL` is exhaustive on purpose: adding a tutorial forces updating the array (and
thus the replay menu and any exhaustive `match`). `is_available` gates which ids
have an implemented script + wired surface today; the rest are reserved keys for
upcoming phases and are hidden from the replay UI so it never lists dead rows.
*/

/// Stable identity of a single tutorial. The `key` string is the on-disk
/// persistence key and must never change once shipped; the variant name may be
/// refactored freely.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TutorialId {
    LauncherMain,
    NewProject,
    StudioBase,
    StudioTranslation,
    StudioCleaning,
    StudioTyping,
    StudioPsEditor,
    StudioCharacters,
    StudioTerms,
    StudioNotes,
    StudioWiki,
}

impl TutorialId {
    /// Every tutorial id, in replay-menu order. Exhaustive: a new variant must be
    /// added here, which surfaces it in the replay menu and forces reconsidering
    /// exhaustive matches over the array.
    pub const ALL: [TutorialId; 11] = [
        TutorialId::LauncherMain,
        TutorialId::NewProject,
        TutorialId::StudioBase,
        TutorialId::StudioTranslation,
        TutorialId::StudioCleaning,
        TutorialId::StudioTyping,
        TutorialId::StudioPsEditor,
        TutorialId::StudioCharacters,
        TutorialId::StudioTerms,
        TutorialId::StudioNotes,
        TutorialId::StudioWiki,
    ];

    /// Stable persistence key stored in `Tutorials.completed`. Never change an
    /// existing value: it would orphan users' recorded completion.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            TutorialId::LauncherMain => "launcher_main",
            TutorialId::NewProject => "new_project",
            TutorialId::StudioBase => "studio_base",
            TutorialId::StudioTranslation => "studio_translation",
            TutorialId::StudioCleaning => "studio_cleaning",
            TutorialId::StudioTyping => "studio_typing",
            TutorialId::StudioPsEditor => "studio_ps_editor",
            TutorialId::StudioCharacters => "studio_characters",
            TutorialId::StudioTerms => "studio_terms",
            TutorialId::StudioNotes => "studio_notes",
            TutorialId::StudioWiki => "studio_wiki",
        }
    }

    /// Resolve an id from its persistence key, or `None` for an unknown key
    /// (e.g. a key written by a newer build; it is simply ignored on load).
    #[must_use]
    pub fn from_key(key: &str) -> Option<TutorialId> {
        TutorialId::ALL.into_iter().find(|id| id.key() == key)
    }

    /// Human-readable name shown in the replay menu (Russian UI language).
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            TutorialId::LauncherMain => "Главное меню лаунчера",
            TutorialId::NewProject => "Окно новой главы",
            TutorialId::StudioBase => "Обзор редактора",
            TutorialId::StudioTranslation => "Вкладка перевода",
            TutorialId::StudioCleaning => "Вкладка клининга",
            TutorialId::StudioTyping => "Вкладка тайпинга",
            TutorialId::StudioPsEditor => "PS-редактор",
            TutorialId::StudioCharacters => "Вкладка персонажей",
            TutorialId::StudioTerms => "Вкладка терминов",
            TutorialId::StudioNotes => "Вкладка заметок",
            TutorialId::StudioWiki => "Вкладка вики",
        }
    }

    /// Whether this tutorial has an implemented step script and a wired surface
    /// today. Only available tutorials appear in the replay pane; reserved keys
    /// stay hidden until their phase ships.
    #[must_use]
    pub fn is_available(self) -> bool {
        match self {
            TutorialId::LauncherMain => true,
            TutorialId::NewProject
            | TutorialId::StudioBase
            | TutorialId::StudioTranslation
            | TutorialId::StudioCleaning
            | TutorialId::StudioTyping
            | TutorialId::StudioPsEditor
            | TutorialId::StudioCharacters
            | TutorialId::StudioTerms
            | TutorialId::StudioNotes
            | TutorialId::StudioWiki => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TutorialId;

    #[test]
    fn keys_roundtrip_through_from_key() {
        for id in TutorialId::ALL {
            assert_eq!(TutorialId::from_key(id.key()), Some(id));
        }
    }

    #[test]
    fn keys_are_unique() {
        for (i, a) in TutorialId::ALL.iter().enumerate() {
            for b in &TutorialId::ALL[i + 1..] {
                assert_ne!(a.key(), b.key(), "duplicate persistence key");
            }
        }
    }

    #[test]
    fn unknown_key_is_ignored() {
        assert_eq!(TutorialId::from_key("does_not_exist"), None);
    }
}
