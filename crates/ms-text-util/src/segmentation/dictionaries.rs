/*
File: crates/ms-text-util/src/segmentation/dictionaries.rs

Purpose:
Per-language TeX hyphenation dictionary bundle used by the segmenters and by the
renderer's runtime wrap. Loading a `Standard` dictionary from embedded patterns
is expensive, so `for_language` caches one bundle per `TextLanguage` in a
thread-local map and hands out cheap `Rc` clones. All the language TeX patterns
are already compiled into the `hyphenation` crate via its `embed_all` feature —
no dictionary is downloaded or added at runtime.

Key type:
- `HyphenationDictionaries` — the primary dictionary for a language plus one
  opposite-script fallback (English for Cyrillic-script languages, Russian for
  Latin-script languages), so mixed-script words still hyphenate.

Key function:
- `for_language(TextLanguage) -> Rc<HyphenationDictionaries>` (thread-local cache).

Contract:
`breaks_for_word` returns dictionary break byte-offsets already sanitized by the
language's group rules (see `cyrillic_slavic`/`romance`/`latin_common`). The
dictionary matching a word's own script is tried first; on an empty (sanitized)
result the opposite-script dictionary is tried. For Russian this reproduces the
pre-refactor `russian`-then-`EnglishUS` order byte-for-byte.
*/

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use hyphenation::{Hyphenator, Language, Load, Standard};

use super::{cyrillic_slavic, latin_common};
use crate::language::{ScriptGroup, TextLanguage};

/// Loaded hyphenation dictionaries for one typesetting language.
#[derive(Debug)]
pub struct HyphenationDictionaries {
    language: TextLanguage,
    /// Dictionary for `language` itself.
    primary: Option<Standard>,
    /// Opposite-script fallback: `EnglishUS` for Cyrillic-script languages,
    /// `Russian` for Latin-script languages. Used for words whose script does
    /// not match the selected language.
    other_script: Option<Standard>,
}

thread_local! {
    /// Per-language dictionary cache. Keyed by `TextLanguage`; each entry owns
    /// its loaded `Standard` patterns and is shared via `Rc`.
    static DICT_CACHE: RefCell<HashMap<TextLanguage, Rc<HyphenationDictionaries>>> =
        RefCell::new(HashMap::new());
}

impl HyphenationDictionaries {
    /// Returns the cached dictionary bundle for `language`, loading it on first
    /// use. Cheap on the hot path: a thread-local map lookup and an `Rc` clone.
    #[must_use]
    pub fn for_language(language: TextLanguage) -> Rc<HyphenationDictionaries> {
        DICT_CACHE.with(|cache| {
            if let Some(existing) = cache.borrow().get(&language) {
                return Rc::clone(existing);
            }
            let dicts = Rc::new(HyphenationDictionaries::load(language));
            cache.borrow_mut().insert(language, Rc::clone(&dicts));
            dicts
        })
    }

    fn load(language: TextLanguage) -> Self {
        let primary = Standard::from_embedded(embedded_language(language)).ok();
        let other_script = match language.group() {
            ScriptGroup::CyrillicSlavic => Standard::from_embedded(Language::EnglishUS).ok(),
            ScriptGroup::LatinSlavic | ScriptGroup::Romance | ScriptGroup::English => {
                Standard::from_embedded(Language::Russian).ok()
            }
        };
        Self {
            language,
            primary,
            other_script,
        }
    }

    /// The language this bundle was built for.
    #[must_use]
    pub fn language(&self) -> TextLanguage {
        self.language
    }

    /// Sanitized dictionary break byte-offsets for `word`. The dictionary that
    /// matches the word's own script is tried first; if its sanitized result is
    /// empty, the opposite-script dictionary is tried. Empty when the word must
    /// not break.
    #[must_use]
    pub fn breaks_for_word(&self, word: &str) -> Vec<usize> {
        let word_cyrillic = cyrillic_slavic::contains_cyrillic(word);
        let language_cyrillic = matches!(self.language.group(), ScriptGroup::CyrillicSlavic);
        // Dictionary matching the word's own script goes first.
        let (first, second) = if word_cyrillic == language_cyrillic {
            (self.primary.as_ref(), self.other_script.as_ref())
        } else {
            (self.other_script.as_ref(), self.primary.as_ref())
        };
        for dic in [first, second].into_iter().flatten() {
            let sanitized = self.sanitize(word, dic.hyphenate(word).breaks);
            if !sanitized.is_empty() {
                return sanitized;
            }
        }
        Vec::new()
    }

    /// Applies the language group's break-sanitizing rules to raw dictionary
    /// offsets.
    fn sanitize(&self, word: &str, raw: Vec<usize>) -> Vec<usize> {
        match self.language.group() {
            ScriptGroup::CyrillicSlavic => cyrillic_slavic::sanitize_breaks(word, raw),
            ScriptGroup::LatinSlavic | ScriptGroup::English | ScriptGroup::Romance => {
                latin_common::sanitize_breaks(word, raw)
            }
        }
    }
}

/// Maps a `TextLanguage` to the embedded `hyphenation` language whose TeX
/// patterns back it.
fn embedded_language(language: TextLanguage) -> Language {
    match language {
        TextLanguage::Ru => Language::Russian,
        TextLanguage::Uk => Language::Ukrainian,
        TextLanguage::Be => Language::Belarusian,
        TextLanguage::Sr => Language::SerbianCyrillic,
        TextLanguage::Pl => Language::Polish,
        TextLanguage::Cs => Language::Czech,
        TextLanguage::Sk => Language::Slovak,
        TextLanguage::Sl => Language::Slovenian,
        TextLanguage::Hr => Language::Croatian,
        TextLanguage::Es => Language::Spanish,
        TextLanguage::Fr => Language::French,
        TextLanguage::Pt => Language::Portuguese,
        TextLanguage::En => Language::EnglishUS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_loads_a_primary_dictionary() {
        for language in TextLanguage::all() {
            let dicts = HyphenationDictionaries::for_language(language);
            assert!(
                dicts.primary.is_some(),
                "missing embedded dictionary for {}",
                language.tag()
            );
        }
    }

    #[test]
    fn for_language_returns_cached_instance() {
        let a = HyphenationDictionaries::for_language(TextLanguage::Ru);
        let b = HyphenationDictionaries::for_language(TextLanguage::Ru);
        assert!(Rc::ptr_eq(&a, &b), "for_language must reuse the cached bundle");
    }

    #[test]
    fn romance_words_hyphenate_at_dictionary_points() {
        // Spanish / Portuguese / English words break at real dictionary points.
        let es = HyphenationDictionaries::for_language(TextLanguage::Es);
        assert!(!es.breaks_for_word("palabra").is_empty(), "Spanish 'palabra'");
        let pt = HyphenationDictionaries::for_language(TextLanguage::Pt);
        assert!(!pt.breaks_for_word("palavra").is_empty(), "Portuguese 'palavra'");
        let en = HyphenationDictionaries::for_language(TextLanguage::En);
        assert!(!en.breaks_for_word("hyphenation").is_empty(), "English 'hyphenation'");
    }

    #[test]
    fn french_word_does_not_break_adjacent_to_apostrophe() {
        let fr = HyphenationDictionaries::for_language(TextLanguage::Fr);
        let word = "l'homme";
        let apostrophe = word.find('\'').unwrap_or(0);
        let after_apostrophe = apostrophe + '\''.len_utf8();
        let breaks = fr.breaks_for_word(word);
        assert!(
            !breaks.contains(&apostrophe) && !breaks.contains(&after_apostrophe),
            "French must not break at an apostrophe boundary, got {breaks:?}"
        );
    }
}
