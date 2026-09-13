/*
File: src/tabs/typing/render_next/wrap/mod.rs

Purpose:
Каркас подсистемы переноса строк нового рендера typing.

Main responsibilities:
- отделить word wrapping и hyphenation от layout и raster;
- стать корневым модулем для horizontal/vertical/shape wrap логики.

Разбивка текста на блоки и языковые правила переноса вынесены в
`ms_text_util::segmentation` (см. `Segmenter`); здесь wrap-ядро лишь
подбирает переносы поверх готовых блоков.

Public surface:
- `HyphenationDictionaries` (реэкспорт из `segmentation`) обслуживает runtime
  словарный/аварийный перенос;
- `reshape_text_for_shape` собирает normal/free/shape horizontal wrap без участия raster-слоя;
- `WordBreakPolicy` и helper-функции скрывают mapping от `TextWrapMode` к деталям wrap-ядра.
*/

pub mod forms;
mod horizontal;
mod hyphenation;
mod shape;
mod vertical;

use super::types::TextWrapMode;

pub(crate) use ms_text_util::segmentation::HyphenationDictionaries;
pub(crate) use shape::{LayoutTextResult, ShapeWrapRequest, reshape_text_for_shape};
pub(crate) use vertical::{VerticalWrapRequest, build_vertical_layout_text};

const SOFT_HYPHEN: char = '\u{00AD}';
const SOFT_WRAP_WIDTH_TOLERANCE: f32 = 1.04;
const CONSERVATIVE_DICTIONARY_BREAK_PENALTY: f32 = 120.0;
const EMERGENCY_BREAK_PENALTY: f32 = 900.0;
const SHORT_HYPHEN_TAIL_PENALTY: f32 = 220.0;
const MODERATE_TREE_EXPANDING_RATIO: f32 = 0.94;
const MODERATE_TREE_CONTRACTING_RATIO: f32 = 1.06;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WordBreakPolicy {
    Minimal,
    Moderate,
    Aggressive,
}

#[must_use]
pub(crate) fn needs_hyphenation_dicts(wrap_mode: TextWrapMode) -> bool {
    matches!(
        wrap_mode,
        TextWrapMode::Minimal | TextWrapMode::Moderate | TextWrapMode::Aggressive
    )
}

#[must_use]
pub(crate) fn word_break_policy(wrap_mode: TextWrapMode) -> Option<WordBreakPolicy> {
    match wrap_mode {
        TextWrapMode::None | TextWrapMode::WholeWords => None,
        TextWrapMode::Minimal => Some(WordBreakPolicy::Minimal),
        TextWrapMode::Moderate => Some(WordBreakPolicy::Moderate),
        TextWrapMode::Aggressive => Some(WordBreakPolicy::Aggressive),
    }
}

#[must_use]
pub(crate) fn should_prehyphenate_overlong(wrap_mode: TextWrapMode) -> bool {
    matches!(wrap_mode, TextWrapMode::Moderate | TextWrapMode::Aggressive)
}

/// Является ли символ висящей пунктуацией. Список общий для всего приложения и
/// редактируется в настройках — см. [`ms_text_util::text_punctuation`].
#[must_use]
pub(crate) fn is_hanging_punctuation(ch: char) -> bool {
    ms_text_util::text_punctuation::is_hanging_punctuation(ch)
}

/// Whether a face the shaper can pick for `attrs` covers BOTH characters of a
/// kerning pair — the width metrics' stand-in for the pen loop's same-face guard.
///
/// The pen loop applies a user-authored pair only when both glyphs came from the
/// SAME face and that face carries the table (`pipeline::custom_pair_delta_px`). A
/// width measurement has no per-glyph font ids to reproduce that with, but it has
/// the cause: a character the selected font does not cover is exactly the character
/// that falls through to a FALLBACK font at draw time, where no override applies. So
/// gating the measurement-side override on coverage makes the two sides agree for
/// every pair whose characters the selected font can actually draw.
///
/// Face selection reproduces what the shaper will do with these `attrs`: the
/// requested family resolved through `fontdb::Database::family_name` (which turns a
/// generic family into the concrete one the render set as its default) intersected
/// with `Attrs::matches` for style/stretch. Note that `Attrs::matches` alone checks
/// ONLY style and stretch — matching on it without the family name would accept
/// coverage from any face in the database, which for this renderer means the whole
/// bundled `fonts/ui` stack, and the gate would never say no.
///
/// Coverage itself is the cmap probe the rest of the project uses for this question
/// (`panel::font_coverage::classify_font_bytes_for`): glyph id `0` is `.notdef`,
/// i.e. not covered. Callers memoize per pair — the face scan is not free.
///
/// Returns `false` when no face matches `attrs` at all (nothing will draw the pair
/// in the selected font, so no correction may be claimed for it).
///
/// RESIDUAL GAP, stated rather than hidden: this proves the selected font CAN draw
/// both characters, not that the shaper did — a pair split across two runs, or one
/// whose cluster is not a single `char` (a ligature), is still corrected here while
/// the pen loop rejects it. Both are bounded by the pair's own authored magnitude on
/// a single line; the unbounded fallback drift this guard removes was not.
#[must_use]
pub(crate) fn selected_face_covers_pair(
    font_system: &mut cosmic_text::FontSystem,
    attrs: &cosmic_text::Attrs<'_>,
    left: char,
    right: char,
) -> bool {
    let family = font_system
        .db()
        .family_name(&attrs.family)
        .to_lowercase();
    let matching: Vec<cosmic_text::fontdb::ID> = font_system
        .db()
        .faces()
        .filter(|face| attrs.matches(face))
        .filter(|face| {
            face.families
                .iter()
                .any(|(name, _language)| name.to_lowercase() == family)
        })
        .map(|face| face.id)
        .collect();
    // `get_font` needs the system mutably, so the ids are collected first.
    matching.into_iter().any(|id| {
        font_system.get_font(id).is_some_and(|font| {
            let charmap = font.as_swash().charmap();
            charmap.map(left) != 0 && charmap.map(right) != 0
        })
    })
}


#[cfg(test)]
mod tests {
    use cosmic_text::{Attrs, Family, FontSystem, fontdb};

    /// Fixture font for the coverage query: the same Liberation Sans the pipeline and
    /// form-metric tests use. It covers Latin and Hebrew but NOT Arabic, which is what
    /// makes a "not covered" case reachable without shipping a second fixture.
    fn coverage_fixture_system() -> (FontSystem, String) {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../test/PanelCleaner/pcleaner/data/LiberationSans-Regular.ttf");
        let bytes = std::fs::read(path).expect("fixture font bytes");
        let mut db = fontdb::Database::new();
        db.load_font_data(bytes);
        let family = db
            .faces()
            .next()
            .and_then(|face| face.families.first().cloned())
            .map(|(name, _language)| name)
            .expect("fixture family name");
        (
            FontSystem::new_with_locale_and_db("en-US".to_string(), db),
            family,
        )
    }

    /// The width metrics' stand-in for the pen loop's same-face guard: a pair is
    /// overridable only when the SELECTED font draws both of its characters. Without
    /// this the measurement corrects a pair that draw time leaves alone, and the two
    /// disagree by the pair's whole authored magnitude — which moves line BREAKS.
    #[test]
    fn coverage_gates_a_pair_on_the_selected_font() {
        let (mut font_system, family) = coverage_fixture_system();
        let attrs = Attrs::new().family(Family::Name(family.as_str()));

        assert!(
            super::selected_face_covers_pair(&mut font_system, &attrs, 'A', 'V'),
            "the fixture must cover a Latin pair"
        );
        // Arabic: absent from Liberation Sans, so at draw time both characters fall
        // through to a fallback face that carries no override table.
        assert!(
            !super::selected_face_covers_pair(&mut font_system, &attrs, '\u{0639}', '\u{0628}'),
            "an uncovered pair must not be overridable"
        );
        // One side covered is still not a pair the selected font can kern.
        assert!(
            !super::selected_face_covers_pair(&mut font_system, &attrs, 'A', '\u{0628}'),
            "a half-covered pair must not be overridable either"
        );
        // A repeated character is counted per occurrence, not per distinct char.
        assert!(
            super::selected_face_covers_pair(&mut font_system, &attrs, 'A', 'A'),
            "a covered character paired with itself must stay overridable"
        );
    }

    /// No face matches the attrs at all: nothing in the selected font will draw the
    /// pair, so no correction may be claimed for it.
    #[test]
    fn coverage_is_false_when_no_face_matches_the_attrs() {
        let (mut font_system, _family) = coverage_fixture_system();
        let attrs = Attrs::new().family(Family::Name("Ms Nonexistent Coverage Family"));
        assert!(!super::selected_face_covers_pair(
            &mut font_system,
            &attrs,
            'A',
            'V'
        ));
    }
}
