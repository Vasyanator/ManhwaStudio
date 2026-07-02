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
