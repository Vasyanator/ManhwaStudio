/*
File: crates/ms-text-util/src/segmentation/rules.rs

Purpose:
Group-dispatched break-boundary rules consumed by the renderer's runtime
horizontal/vertical wrap. Previously the wrap crate imported Cyrillic-specific
helpers directly (`contains_cyrillic`, `count_vowels_visible`,
`is_safe_hyphen_boundary_at`, `should_avoid_emergency_split`); those are now
behind this façade, which dispatches on the process-global typesetting language's
`ScriptGroup`. This keeps runtime wrap language-agnostic.

The public functions read `language::text_language()` once and dispatch. They are
called per candidate break in wrap's hot loop; each is a small pure computation.
The default language is `Ru`, so with no seeding these behave exactly like the
former Cyrillic helpers (Russian output unchanged).

Public functions:
- `dictionary_split_is_valid(word, boundary)` — accept a dictionary break point.
- `emergency_boundary_is_safe(text, boundary)` — accept an emergency break point.
- `avoid_emergency_split(text)` — reject emergency-splitting a whole block.
*/

use super::{cyrillic_slavic, latin_common};
use crate::language::{ScriptGroup, TextLanguage, text_language};

/// Whether a dictionary break of `word` at byte offset `boundary` is valid under
/// the process-global language's rules.
#[must_use]
pub fn dictionary_split_is_valid(word: &str, boundary: usize) -> bool {
    dictionary_split_is_valid_for(text_language(), word, boundary)
}

/// Whether an emergency (non-dictionary) break right before byte `boundary` in
/// `text` is allowed under the process-global language's rules.
#[must_use]
pub fn emergency_boundary_is_safe(text: &str, boundary: usize) -> bool {
    emergency_boundary_is_safe_for(text_language(), text, boundary)
}

/// Whether `text` (one wrap block) must never be emergency-hyphenated under the
/// process-global language's rules.
#[must_use]
pub fn avoid_emergency_split(text: &str) -> bool {
    avoid_emergency_split_for(text_language(), text)
}

// --- Explicit-language variants (dispatch core; also used by tests) ----------

pub(crate) fn dictionary_split_is_valid_for(
    language: TextLanguage,
    word: &str,
    boundary: usize,
) -> bool {
    match language.group() {
        ScriptGroup::CyrillicSlavic => cyrillic_slavic::dictionary_split_is_valid(word, boundary),
        ScriptGroup::LatinSlavic | ScriptGroup::Romance | ScriptGroup::English => {
            latin_common::dictionary_split_is_valid(word, boundary)
        }
    }
}

pub(crate) fn emergency_boundary_is_safe_for(
    language: TextLanguage,
    text: &str,
    boundary: usize,
) -> bool {
    match language.group() {
        ScriptGroup::CyrillicSlavic => cyrillic_slavic::emergency_boundary_is_safe(text, boundary),
        ScriptGroup::LatinSlavic | ScriptGroup::Romance | ScriptGroup::English => {
            latin_common::emergency_boundary_is_safe(text, boundary)
        }
    }
}

pub(crate) fn avoid_emergency_split_for(language: TextLanguage, text: &str) -> bool {
    match language.group() {
        ScriptGroup::CyrillicSlavic => cyrillic_slavic::avoid_emergency_split(text),
        ScriptGroup::LatinSlavic | ScriptGroup::Romance | ScriptGroup::English => {
            latin_common::avoid_emergency_split(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cyrillic_dispatch_matches_group_rules() {
        // Russian ъ line-start rule reachable through the façade.
        let word = "подъезд";
        let before_hard = word.find('ъ').unwrap_or(0);
        assert!(!emergency_boundary_is_safe_for(TextLanguage::Ru, word, before_hard));
    }

    #[test]
    fn latin_dispatch_rejects_apostrophe_boundary() {
        let word = "l'homme";
        let apostrophe = word.find('\'').unwrap_or(0);
        assert!(!emergency_boundary_is_safe_for(TextLanguage::Fr, word, apostrophe));
        assert!(!emergency_boundary_is_safe_for(TextLanguage::En, word, apostrophe));
    }
}
