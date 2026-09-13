/*
File: crates/ms-text-util/src/lib.rs

Purpose:
Crate root of `ms-text-util` — config-free text utilities shared between the
ManhwaStudio binary and the text renderer (`ms-text-render`).

Modules:
- `hangul`: modern-Hangul syllable arithmetic (compose/decompose) and the
  compatibility-jamo caption tables. Independent of the language model.
- `language`: the typesetting-language model (`TextLanguage`/`ScriptGroup`) and
  the process-global selected language (seeded by the app).
- `text_punctuation`: the global hanging-punctuation SET (seeded by the app) and
  `clamp_hanging_weight`, the normalizer of the separate hanging STRENGTH
  (`0.0..=1.0`) that the renderer and the wrap weight a hanging character by.
- `segmentation`: language-aware line/unit segmentation used by wrapping.

Contract:
- No dependency on the application crate or its config. Process-global state
  (hanging punctuation, typesetting language) defaults to values that reproduce
  the historical behavior; the app seeds the user values at startup via
  `text_punctuation::set_hanging_punctuation` and `language::set_text_language`.
*/

#![warn(clippy::all)]

pub mod hangul;
pub mod language;
pub mod segmentation;
pub mod text_punctuation;
