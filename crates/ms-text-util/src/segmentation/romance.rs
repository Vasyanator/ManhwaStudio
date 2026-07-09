/*
File: crates/ms-text-util/src/segmentation/romance.rs

Purpose:
Romance implementation of the text segmenter (`ScriptGroup::Romance`: Spanish,
French, Portuguese). Uses the embedded TeX patterns for the selected language
(via `HyphenationDictionaries`) and the shared Latin boundary rules from
`latin_common`.

Language-specific concerns and how they are satisfied:
- French `l'homme`: a break must never sit next to an apostrophe. The shared
  `latin_common` rule only allows a break strictly between two letters, so an
  apostrophe (a non-letter) is never a break side. No French-specific code is
  needed, and this is verified in `dictionaries`/`latin_common` tests.
- Spanish `¿` `¡` opening punctuation: these are non-letters, so the same
  letter-to-letter rule keeps them attached to the following word and never
  produces a break after them. Whether they hang off the line edge is governed
  by the app-global hanging-punctuation set, not by this module.

Binding: Romance service words (articles/prepositions) are not glued by this
engine; every junction is a free break (`Safe`). A future refinement could add
per-language article/preposition binding, mirroring the Cyrillic-Slavic lists.

Key type:
- `RomanceSegmenter` — `impl Segmenter` for es/fr/pt.
*/

use std::rc::Rc;

use super::base::{Conservatism, Segmenter};
use super::dictionaries::HyphenationDictionaries;
use super::latin_common;
use crate::language::TextLanguage;

/// Romance segmenter for one of es/fr/pt. Holds the thread-local-cached
/// dictionaries for its `language`.
#[derive(Debug)]
pub struct RomanceSegmenter {
    language: TextLanguage,
    dicts: Rc<HyphenationDictionaries>,
}

impl RomanceSegmenter {
    /// Builds a segmenter for `language` (must belong to `ScriptGroup::Romance`).
    #[must_use]
    pub fn new(language: TextLanguage) -> Self {
        Self {
            language,
            dicts: HyphenationDictionaries::for_language(language),
        }
    }

    /// The language this segmenter was built for.
    #[must_use]
    pub fn language(&self) -> TextLanguage {
        self.language
    }
}

impl Segmenter for RomanceSegmenter {
    /// Romance junctions are free breaks in this engine.
    fn binding_conservatism(&self, _left_token: &str, _right_token: &str) -> Conservatism {
        Conservatism::Safe
    }

    fn hyphenate_word(&self, word: &str) -> Option<String> {
        latin_common::maybe_soft_hyphenate_word(word, &self.dicts)
    }

    fn hyphen_cost(&self, head_word: &str, tail_word: &str) -> u32 {
        latin_common::hyphen_cost(head_word, tail_word)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spanish_and_portuguese_words_hyphenate() {
        let es = RomanceSegmenter::new(TextLanguage::Es);
        assert!(es.hyphenate_word("palabra").is_some(), "Spanish 'palabra'");
        let pt = RomanceSegmenter::new(TextLanguage::Pt);
        assert!(pt.hyphenate_word("palavra").is_some(), "Portuguese 'palavra'");
    }

    #[test]
    fn french_apostrophe_word_does_not_break_at_apostrophe() {
        let fr = RomanceSegmenter::new(TextLanguage::Fr);
        // Soft hyphenation walks word runs split by non-letters, so the "l" run
        // is too short to hyphenate and no soft hyphen lands on the apostrophe.
        let hyphenated = fr
            .hyphenate_word("l'homme")
            .unwrap_or_else(|| "l'homme".to_string());
        let apostrophe = hyphenated.find('\'').unwrap_or(0);
        let before = hyphenated[..apostrophe].chars().next_back();
        let after = hyphenated[apostrophe + '\''.len_utf8()..].chars().next();
        assert_ne!(before, Some('\u{00AD}'), "no soft hyphen before apostrophe");
        assert_ne!(after, Some('\u{00AD}'), "no soft hyphen after apostrophe");
    }
}
