/*
File: crates/ms-text-util/src/segmentation/mod.rs

Purpose:
Text-tab segmenter: splits a paragraph into blocks (`Block`) and describes the
junction (`Joint`) between adjacent blocks — how to join them on one line and how
to hyphenate/wrap at a break. The language-neutral core lives in `base`; each
typesetting-language group has its own module implementing `Segmenter`.

Groups (one `impl Segmenter` per `ScriptGroup`):
- `cyrillic_slavic` — Russian and relatives; owns the historical Russian rules.
- `latin_slavic`    — Polish/Czech/Slovak/Slovenian/Croatian.
- `romance`         — Spanish/French/Portuguese.
- `english`         — English.

Shared infrastructure:
- `dictionaries` — per-language TeX hyphenation bundle (`HyphenationDictionaries`,
  thread-local cached via `for_language`).
- `latin_common` — break helpers shared by the Latin-script groups.
- `rules`        — group-dispatched break-boundary façade for the renderer's wrap.

Language selection:
`with_default_segmenter` dispatches on the process-global typesetting language
(`crate::language::text_language`, seeded by the app). The per-language segmenter
is cached in a thread-local so it is not rebuilt per call.
*/

pub mod base;
pub mod cyrillic_slavic;
pub mod dictionaries;
pub mod english;
pub mod latin_common;
pub mod latin_slavic;
pub mod romance;
pub mod rules;

use std::cell::RefCell;
use std::collections::HashMap;

pub use base::{
    BindingMode, Block, Conservatism, NON_BREAKING_SPACE, SOFT_HYPHEN, SegmentOptions, Segmenter,
    build_line_text_and_units, count_layout_units,
};
pub use cyrillic_slavic::CyrillicSlavicSegmenter;
pub use dictionaries::HyphenationDictionaries;
pub use english::EnglishSegmenter;
pub use latin_slavic::LatinSlavicSegmenter;
pub use romance::RomanceSegmenter;

use crate::language::{ScriptGroup, TextLanguage, text_language};

thread_local! {
    /// Per-language segmenter cache. Building a segmenter clones a cheap `Rc` to
    /// the (also cached) dictionaries, but caching avoids rebuilding the struct
    /// per wrap call.
    static SEGMENTER_CACHE: RefCell<HashMap<TextLanguage, Box<dyn Segmenter>>> =
        RefCell::new(HashMap::new());
}

/// Builds the segmenter for `language` by dispatching on its script group.
fn build_segmenter(language: TextLanguage) -> Box<dyn Segmenter> {
    match language.group() {
        ScriptGroup::CyrillicSlavic => Box::new(CyrillicSlavicSegmenter::new(language)),
        ScriptGroup::LatinSlavic => Box::new(LatinSlavicSegmenter::new(language)),
        ScriptGroup::Romance => Box::new(RomanceSegmenter::new(language)),
        ScriptGroup::English => Box::new(EnglishSegmenter::new()),
    }
}

/// Runs `f` with the segmenter for the process-global typesetting language.
///
/// The segmenter is cached per language in a thread-local, so repeated calls do
/// not rebuild it. `f` must not itself call `with_default_segmenter` (the
/// thread-local cache is borrowed for the duration); no current caller does.
pub fn with_default_segmenter<R>(f: impl FnOnce(&dyn Segmenter) -> R) -> R {
    let language = text_language();
    SEGMENTER_CACHE.with(|cache| {
        let mut map = cache.borrow_mut();
        let segmenter = map
            .entry(language)
            .or_insert_with(|| build_segmenter(language));
        f(segmenter.as_ref())
    })
}

/// Test-only serialization lock for the process-global typesetting language, so
/// tests that set it do not race the default-`Ru` readers. Held via a mutex
/// guard for the duration of the test body.
#[cfg(test)]
pub(crate) fn test_language_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::set_text_language;

    #[test]
    fn dispatch_selects_group_segmenter_for_global_language() {
        let _guard = test_language_lock();
        let previous = text_language();

        // Russian: dictionary soft hyphenation with the ь rule.
        set_text_language(TextLanguage::Ru);
        let ru = with_default_segmenter(|seg| seg.soft_hyphenate_overlong("сильнее"));
        assert_eq!(ru.replace(SOFT_HYPHEN, "·"), "силь·нее");

        // English: dictionary soft hyphenation via the English engine.
        set_text_language(TextLanguage::En);
        let en = with_default_segmenter(|seg| seg.soft_hyphenate_overlong("hyphenation"));
        assert!(en.contains(SOFT_HYPHEN));

        set_text_language(previous);
    }
}
