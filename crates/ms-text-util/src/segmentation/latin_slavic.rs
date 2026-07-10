/*
File: crates/ms-text-util/src/segmentation/latin_slavic.rs

Purpose:
Latin-script Slavic implementation of the text segmenter
(`ScriptGroup::LatinSlavic`: Polish, Czech, Slovak, Slovenian, Croatian). Uses
the embedded TeX patterns for the selected language and the shared Latin
boundary rules from `latin_common`.

Binding: unlike the other Latin groups, this engine marks two risky junctions as
`Conservatism::Reckless` (see `binding_conservatism`): a one-letter preposition/
conjunction orphaned at a line end (Polish/Czech/Slovak/Slovenian/Croatian
typography forbids it) and a "number + unit" pair. Both rules are language-agnostic
and reuse the script-neutral primitives from `base` (`normalize_binding_token`,
`is_single_letter_binding`, `is_numeric_measure_pair`); no per-language one-letter
list is hand-authored.

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

use super::base::{
    Conservatism, Segmenter, is_numeric_measure_pair, is_single_letter_binding,
    normalize_binding_token,
};
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
    /// Marks the risky junctions of the Latin-Slavic languages (see the free
    /// [`binding_conservatism`]). The rule is language-agnostic, so the segmenter's
    /// own `language` is not consulted.
    fn binding_conservatism(&self, left_token: &str, right_token: &str) -> Conservatism {
        binding_conservatism(left_token, right_token)
    }

    fn hyphenate_word(&self, word: &str) -> Option<String> {
        latin_common::maybe_soft_hyphenate_word(word, &self.dicts)
    }

    fn hyphen_cost(&self, head_word: &str, tail_word: &str) -> u32 {
        latin_common::hyphen_cost(head_word, tail_word)
    }
}

/// Conservatism of a break between two Latin-Slavic tokens. Two language-agnostic
/// rules apply, both the riskiest class (`Conservatism::Reckless`):
/// - a one-letter preposition/conjunction left orphaned at a line end. Polish,
///   Czech, Slovak, Slovenian and Croatian typography all forbid this (Polish `w`,
///   `z`, `i`, `a`, `o`, `u`; Czech/Slovak `k`, `s`, `v`, ...). The single-alphabetic
///   -character test covers every such word without a per-language list.
/// - a "number + unit" pair ("5 kg").
///
/// Everything else is `Safe`; this engine does not glue multi-letter service words.
fn binding_conservatism(left_token: &str, right_token: &str) -> Conservatism {
    // Number+unit ("5 kg") judged on RAW tokens (normalization strips the digits).
    if is_numeric_measure_pair(left_token, right_token) {
        return Conservatism::Reckless;
    }
    let left = normalize_binding_token(left_token);
    // A one-letter preposition/conjunction must not be orphaned at a line end.
    if is_single_letter_binding(left.as_str()) {
        return Conservatism::Reckless;
    }
    Conservatism::Safe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_letter_preposition_is_reckless() {
        // Polish one-letter prepositions/conjunctions must not be orphaned.
        assert_eq!(binding_conservatism("w", "domu"), Conservatism::Reckless);
        assert_eq!(binding_conservatism("z", "nami"), Conservatism::Reckless);
        // A normal word pair breaks freely.
        assert_eq!(binding_conservatism("kot", "śpi"), Conservatism::Safe);
    }

    #[test]
    fn number_and_unit_is_reckless() {
        assert_eq!(binding_conservatism("5", "kg"), Conservatism::Reckless);
    }

    #[test]
    fn binding_via_trait_matches_free_function() {
        let pl = LatinSlavicSegmenter::new(TextLanguage::Pl);
        assert_eq!(pl.binding_conservatism("w", "domu"), Conservatism::Reckless);
        assert_eq!(pl.binding_conservatism("kot", "śpi"), Conservatism::Safe);
    }

    #[test]
    fn polish_and_czech_words_hyphenate() {
        let pl = LatinSlavicSegmenter::new(TextLanguage::Pl);
        assert!(pl.hyphenate_word("czytanie").is_some(), "Polish 'czytanie'");
        let cs = LatinSlavicSegmenter::new(TextLanguage::Cs);
        assert!(cs.hyphenate_word("rozhodnutí").is_some(), "Czech 'rozhodnutí'");
    }
}
