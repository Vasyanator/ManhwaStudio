# Module: crates/ms-text-util/src/segmentation

## Purpose
Text-tab segmenter: cuts a paragraph into blocks (`Block`) and describes the
junction (`Joint`) between adjacent blocks — how to join them on one line and how
to hyphenate/wrap at a break. The language-neutral core is separated from the
per-language rules so a new typesetting language is a self-contained submodule.

## Architecture
The engine is selected by the process-global typesetting language
(`crate::language::text_language`, seeded by the app). Each `ScriptGroup` has one
`impl Segmenter`:

- `ScriptGroup::CyrillicSlavic` → `cyrillic_slavic` (ru/uk/be/sr)
- `ScriptGroup::LatinSlavic`    → `latin_slavic` (pl/cs/sk/sl/hr)
- `ScriptGroup::Romance`        → `romance` (es/fr/pt)
- `ScriptGroup::English`        → `english` (en)

`with_default_segmenter` dispatches on the global language and caches the built
segmenter per language in a thread-local. Dictionaries are cached separately
(`HyphenationDictionaries::for_language`), so a segmenter build is a cheap `Rc`
clone and switching languages does not reload TeX patterns.

## Files and submodules
- `base.rs`: language-neutral API — `Block`, `Joint` (junction groups: `space`,
  `soft_hyphen`, `hard_hyphen`, `glue` + `with_conservatism`), `Conservatism`
  (`Safe` < `Relaxed` < `Bold` < `Reckless`), `BindingMode` (`Glue`/`Annotate`),
  the `Segmenter` trait (hooks `binding_conservatism`, `hyphenate_word`,
  `hyphen_cost`, `is_hard_hyphen_boundary`; default `segment`/`build_segments`/
  `soft_hyphenate_overlong`/`split_segment_into_parts`), and the shared
  `count_layout_units` / `build_line_text_and_units`. Also hosts the
  **script-neutral binding primitives** (`normalize_binding_token`,
  `is_single_letter_binding`, `is_numeric_measure_pair`) shared by the
  Cyrillic-Slavic and Latin-Slavic `binding_conservatism`. They live here rather
  than in `latin_common` so the Cyrillic engine does not depend on a Latin-named
  module. The number+unit list is a script-agnostic Cyrillic+Latin superset. That
  WIDENS the Russian engine on purpose: a Latin unit inside Russian text ("5 kg")
  used to be a free break and now binds. This is the only intended Russian behavior
  change from the extraction, and it is pinned by
  `cyrillic_slavic::tests::russian_binds_a_latin_unit_after_the_shared_extraction`.
- `dictionaries.rs`: `HyphenationDictionaries` — per-language TeX dictionary
  bundle (primary + one opposite-script fallback). `for_language(TextLanguage)`
  returns a thread-local-cached `Rc`. `breaks_for_word` returns group-sanitized
  break offsets; for Russian it reproduces the old `russian`→`EnglishUS` order.
- `cyrillic_slavic.rs`: `CyrillicSlavicSegmenter`. Owns the historical Russian
  rules (ь/ъ/й line-start rule, one-letter/syllable rules, preposition/particle/
  abbreviation binding, `sanitize_breaks`, safe boundaries). Russian output is
  byte-identical to the pre-refactor renderer (golden regression tests), with the
  single documented exception of the widened number+unit list above. The
  preposition/particle binding lists are **dispatched per language** (Ru/Uk/Be/Sr)
  via an exhaustive `TextLanguage` match; non-Cyrillic languages (never built here)
  map to the empty list, never a panic. The Uk/Be/Sr lists are **best-effort,
  pending native-speaker review** — they only change break COST, never correctness.
  Number+unit and the single-letter orphan rule come from the shared `base` helpers.
- `latin_common.rs`: shared Latin-script break helpers (sanitize, split validity,
  emergency boundary, `avoid_emergency_split`, `maybe_soft_hyphenate_word`,
  `hyphen_cost`). The "break only strictly between two letters" rule is what keeps
  a break away from an apostrophe (French `l'homme`) or opening punctuation
  (Spanish `¿ ¡`).
- `romance.rs` / `english.rs`: thin group segmenters over `latin_common`. Binding
  is `Safe` at every junction — a deliberate choice (see `romance.rs` header); the
  `english.rs` `binding_is_always_safe` test pins it.
- `latin_slavic.rs`: thin group segmenter over `latin_common`, but its
  `binding_conservatism` marks two junctions `Reckless`: a one-letter preposition/
  conjunction orphaned at a line end (Polish/Czech/Slovak/Slovenian/Croatian
  typography) and a number+unit pair. Both are language-agnostic and reuse the
  shared `base` primitives — no hand-authored per-language one-letter list.
- `rules.rs`: group-dispatched break-boundary façade consumed by the renderer's
  runtime wrap (`dictionary_split_is_valid`, `emergency_boundary_is_safe`,
  `avoid_emergency_split`). Dispatches on the process-global language's group.
- `mod.rs`: module wiring, re-exports, `with_default_segmenter` + per-language
  cache.

## Contracts and invariants
- Config-free: no submodule reads config. The app seeds the selected language via
  `crate::language::set_text_language` at startup (default `Ru`).
- Russian is a hard bit-identical contract (golden tests in `cyrillic_slavic`).
- `rules::*` read the process-global language; the renderer builds
  `HyphenationDictionaries::for_language(text_language())` so the dictionaries and
  the boundary rules always agree on the language during a render.
- **Known limitation — Polish/Czech repeated hyphen (not implemented, not faked):**
  Polish/Czech require the hyphen to be repeated at the START of the next line
  after a hyphenated break. `Joint` can only append to the head line
  (`wrap_suffix`); there is no tail-line prefix. Latin-Slavic therefore hyphenates
  like the other Latin groups (hyphen at line end only). Removal condition: add a
  `wrap_prefix` field to `base::Joint`, thread it through
  `base::build_line_text_and_units` and every `ms-text-render` wrap consumer, then
  set it to `"-"` for pl/cs soft-hyphen joints in `latin_slavic`.

## Editing map
- To change Russian/Cyrillic-Slavic behavior, see `cyrillic_slavic.rs` (guard the
  golden tests).
- To change shared Latin break behavior, see `latin_common.rs`.
- To add a typesetting language: add the variant + group + tag in
  `crate::language`, add its embedded dictionary in `dictionaries.rs`
  (`embedded_language`), and route it in `mod::build_segmenter` and `rules`. If it
  needs a new group, add the `ScriptGroup` arm at every dispatch site (no
  catch-all arms).
- To change what the renderer's wrap treats as a valid break, see `rules.rs`.
