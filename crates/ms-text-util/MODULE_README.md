# Module: crates/ms-text-util

## Purpose
Config-free text utilities shared by the app binary and the text renderer
(`ms-text-render`): the typesetting-language model, the global hanging-punctuation
set, and language-aware line segmentation. Extracted from `src/` so the renderer
can be its own crate without reaching back into the application crate.

## Architecture
Three independent modules. None reads config or touches the filesystem.

- `language`: `TextLanguage` (concrete typesetting language) and `ScriptGroup`
  (the segmentation engine family), plus the process-global selected language
  (`set_text_language` / `text_language`, backed by an `AtomicU8` with an explicit
  u8 encoding — no `as` casts). Default `Ru`. Shared with the app's font-coverage
  checker (a later task), which is why it lives here rather than in `segmentation`.
- `text_punctuation`: a process-global, generation-counted set of "hanging"
  punctuation characters with a thread-local snapshot cache for the hot path
  (`is_hanging_punctuation`, called per char in tight render/wrap loops).
- `segmentation`: splits text into layout units and offers hyphenation-aware
  segmentation (via the `hyphenation` crate, `embed_all`). One `impl Segmenter`
  per `ScriptGroup`; engine chosen by the global language. See
  `segmentation/MODULE_README.md`.

## Files and submodules
- `src/lib.rs`: crate root; re-exports the three modules.
- `src/language.rs`: `TextLanguage`, `ScriptGroup`, `set_text_language`,
  `text_language`. Also the UI-facing metadata used by the settings selector and the
  font-coverage tooltip: `TextLanguage::name_key`, `ScriptGroup::all` /
  `languages` / `first_language` / `name_key` / `script_name_key` (all
  exhaustive, no catch-all arms; the group→language partition is unit-tested). These
  `*_key` methods return CATALOG KEYS, not localized text — this crate is GUI-free and
  must not depend on `ms-i18n`; the binary resolves them via
  `ms_i18n::lookup(key).unwrap_or(key)` (see `docs/i18n_exclusions.md` §F).
- `src/text_punctuation.rs`: `is_hanging_punctuation`, `set_hanging_punctuation`,
  `hanging_punctuation_string`, `DEFAULT_HANGING_PUNCTUATION`.
- `src/segmentation/`: language-neutral core (`base`), per-language groups
  (`cyrillic_slavic`, `latin_slavic`, `romance`, `english`), the dictionary
  bundle (`dictionaries`), shared Latin helpers (`latin_common`), the wrap
  boundary façade (`rules`), and `mod` (dispatch + cache).

## Contracts and invariants
- No dependency on the app crate or `config`. Process-global state defaults to
  values that reproduce the historical behavior; the **app** seeds user values at
  startup: hanging punctuation via `set_hanging_punctuation`
  (`main.rs::seed_hanging_punctuation_from_config`) and the typesetting language
  via `set_text_language` (`main.rs::seed_text_language_from_config`, config key
  `TextTab.text_language`).
- Russian segmentation output is bit-identical to the pre-refactor renderer.
- `segmentation` depends on `text_punctuation` and `language` within this crate.

## Editing map
- To add/adjust a typesetting language, start in `language.rs`, then
  `segmentation/` (see its MODULE_README).
- To change which characters hang, edit `DEFAULT_HANGING_PUNCTUATION` (and the
  app's config default) — see `text_punctuation.rs`.
- To change segmentation/hyphenation behavior, see `segmentation/`.
