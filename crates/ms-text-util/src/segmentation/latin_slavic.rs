/*
File: crates/ms-text-util/src/segmentation/latin_slavic.rs

Purpose:
Latin-script Slavic implementation of the text segmenter
(`ScriptGroup::LatinSlavic`: Polish, Czech, Slovak, Slovenian, Croatian). Uses
the embedded TeX patterns for the selected language and the shared Latin
boundary rules from `latin_common`.

Known limitation — Polish/Czech repeated hyphen (NOT implemented, not faked):
Polish and Czech orthography require the hyphen to be REPEATED at the START of
the next line after a hyphenated break (the head line ends with "-" and the tail
line begins with "-"). The current `Joint` model can only append to the head
line (`wrap_suffix`); it has no field for a prefix prepended to the tail line, so
this cannot be expressed. Per project rules we do not fake it: Latin-Slavic
languages hyphenate like the other Latin groups (hyphen at the line end only).
Removal condition: add a `wrap_prefix: Cow<'static, str>` field to
`base::Joint`, thread it through `base::build_line_text_and_units` and every wrap
consumer (`ms-text-render` horizontal/vertical/forms line assembly), then set it
to "-" for Polish/Czech soft-hyphen joints here.

Key type:
- `LatinSlavicSegmenter` — `impl Segmenter` for pl/cs/sk/sl/hr.
*/

use std::rc::Rc;

use super::base::{Conservatism, Segmenter};
use super::dictionaries::HyphenationDictionaries;
use super::latin_common;
use crate::language::TextLanguage;

/// Latin-Slavic segmenter for one of pl/cs/sk/sl/hr. Holds the thread-local-
/// cached dictionaries for its `language`.
#[derive(Debug)]
pub struct LatinSlavicSegmenter {
    language: TextLanguage,
    dicts: Rc<HyphenationDictionaries>,
}

impl LatinSlavicSegmenter {
    /// Builds a segmenter for `language` (must belong to
    /// `ScriptGroup::LatinSlavic`).
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

impl Segmenter for LatinSlavicSegmenter {
    /// No service-word gluing in this engine; every junction is a free break.
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
    fn polish_and_czech_words_hyphenate() {
        let pl = LatinSlavicSegmenter::new(TextLanguage::Pl);
        assert!(pl.hyphenate_word("czytanie").is_some(), "Polish 'czytanie'");
        let cs = LatinSlavicSegmenter::new(TextLanguage::Cs);
        assert!(cs.hyphenate_word("rozhodnutí").is_some(), "Czech 'rozhodnutí'");
    }
}
