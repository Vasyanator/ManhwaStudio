# Module: crates/ms-text-util/src

## Purpose
Config-free text utilities shared between the ManhwaStudio binary and the text
renderer (`ms-text-render`): the typesetting-language model, the app-wide hanging
punctuation set and its strength contract, Hangul syllable arithmetic, and the
language-aware segmenter that wrapping is built on.

## Architecture
The crate is a LEAF: it depends on no application crate and reads no config file.
The two pieces of process-global state it owns (`language`, `text_punctuation`)
initialize to values reproducing the historical behavior and are seeded by the
binary at startup. Everything else is pure functions over `&str`.

Consumers:
- `ms-text-render` — wrapping, layout and form search (`segmentation`,
  `text_punctuation`);
- the binary — startup seeding, the settings tab, Hangul captions;
- `ms-config` — the hanging-punctuation default only.

## Files and submodules
- `lib.rs`: crate root, module wiring, the config-free contract.
- `language.rs`: `TextLanguage` / `ScriptGroup` and the process-global selected
  typesetting language. Edit it to add a language; every dispatch site matches
  exhaustively on purpose.
- `text_punctuation.rs`: the editable hanging-punctuation CHARACTER SET (global,
  generation-counted, with a per-thread snapshot for the hot `is_hanging_punctuation`
  path) and `clamp_hanging_weight`, the single normalizer of the hanging STRENGTH.
  The two are independent: the set says WHICH characters hang, the strength says how
  MUCH they hang.
- `hangul.rs`: modern-Hangul compose/decompose and compatibility-jamo caption
  tables. Independent of the language model.
- `segmentation/`: language-aware block/junction segmentation, TeX hyphenation
  dictionaries, and the layout-unit counting the renderer's wrap consumes. See its
  own `MODULE_README.md`.

## Contracts and invariants
- **Config-free.** No module here reads `user_config.json` or any path. Global state
  is seeded by the app (`text_punctuation::set_hanging_punctuation`,
  `language::set_text_language`); the defaults must stand on their own.
- **The hanging set and the hanging strength are separate features that share a
  name.** Changing the set changes which characters hang everywhere at once;
  changing the strength (`clamp_hanging_weight`, `0.0..=1.0`, `NaN` = `0.0`) changes
  how much of a hanging character's width is dropped. Both ends of the strength range
  must reproduce the historical off/on behaviors exactly — `ms-text-render` is
  written against that.
- **GUI-free.** UI strings are returned as catalog KEYS (see `Conservatism::label_key`);
  the crate must not depend on the string catalog.
- Global-state mutation is rare and coarse: a set/language change bumps a generation
  counter and invalidates thread-local snapshots. Never take a lock on a per-character
  path.

## Editing map
- To change which characters hang, or the strength contract, see `text_punctuation.rs`.
- To add or change a typesetting language, see `language.rs` and `segmentation/`.
- To change how a line's layout units are counted, see
  `segmentation/base.rs::count_layout_units`.
