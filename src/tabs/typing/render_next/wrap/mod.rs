/*
File: src/tabs/typing/render_next/wrap/mod.rs

Purpose:
Каркас подсистемы переноса строк нового рендера typing.

Main responsibilities:
- отделить word wrapping и hyphenation от layout и raster;
- стать корневым модулем для horizontal/vertical/shape wrap логики.

Public surface:
- `HyphenationDictionaries` и `soft_hyphenate_overlong` обслуживают staged hyphenation path;
- `reshape_text_for_shape` собирает normal/free/shape horizontal wrap без участия raster-слоя;
- `WordBreakPolicy` и helper-функции скрывают mapping от `TextWrapMode` к деталям wrap-ядра.
*/

mod horizontal;
mod hyphenation;
mod shape;
mod vertical;

use super::types::TextWrapMode;

pub(crate) use hyphenation::{HyphenationDictionaries, soft_hyphenate_overlong};
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

#[must_use]
pub(crate) fn is_hanging_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '.' | ','
            | '!'
            | '?'
            | ':'
            | ';'
            | '-'
            | '–'
            | '—'
            | '~'
            | '…'
            | '·'
            | '•'
            | '。'
            | '、'
            | '，'
            | '．'
            | '！'
            | '？'
            | '：'
            | '；'
            | '・'
            | '･'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '"'
            | '\''
            | '«'
            | '»'
            | '\u{201C}'
            | '\u{201D}'
            | '\u{2018}'
            | '\u{2019}'
            | '\u{2039}'
            | '\u{203A}'
            | '\u{201E}'
            | '\u{201F}'
            | '\u{201A}'
    )
}
