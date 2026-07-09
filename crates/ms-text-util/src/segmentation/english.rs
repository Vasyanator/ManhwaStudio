/*
File: crates/ms-text-util/src/segmentation/english.rs

Purpose:
English implementation of the text segmenter (`ScriptGroup::English`). English
has no preposition/particle gluing in this engine, so every word junction is a
free break point: `binding_conservatism` is `Safe` everywhere. Dictionary soft
hyphenation and boundary rules come from `latin_common`.

Key type:
- `EnglishSegmenter` — `impl Segmenter` for English.
*/

use std::rc::Rc;

use super::base::{Conservatism, Segmenter};
use super::dictionaries::HyphenationDictionaries;
use super::latin_common;
use crate::language::TextLanguage;

/// English segmenter. Holds the thread-local-cached English dictionaries.
#[derive(Debug)]
pub struct EnglishSegmenter {
    dicts: Rc<HyphenationDictionaries>,
}

impl EnglishSegmenter {
    /// Builds the English segmenter (fixed to `TextLanguage::En`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            dicts: HyphenationDictionaries::for_language(TextLanguage::En),
        }
    }
}

impl Default for EnglishSegmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Segmenter for EnglishSegmenter {
    /// English never glues service words: every junction is a free break.
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
    fn english_words_hyphenate_at_dictionary_points() {
        let seg = EnglishSegmenter::new();
        let hyphenated = seg
            .hyphenate_word("hyphenation")
            .expect("English 'hyphenation' hyphenates");
        assert!(hyphenated.contains('\u{00AD}'));
        assert_eq!(hyphenated.replace('\u{00AD}', ""), "hyphenation");
    }

    #[test]
    fn binding_is_always_safe() {
        let seg = EnglishSegmenter::new();
        assert_eq!(seg.binding_conservatism("in", "the"), Conservatism::Safe);
        assert_eq!(seg.binding_conservatism("a", "cat"), Conservatism::Safe);
    }
}
