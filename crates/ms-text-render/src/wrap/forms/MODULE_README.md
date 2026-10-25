# Module: crates/ms-text-render/src/wrap/forms

## Purpose
Discrete text "forms": breaking a text into lines whose width sequence matches a preset
shape (free, lens, widen, narrow, …). Public as `ms_text_render::forms` (`pub use
wrap::forms` in `lib.rs`): the typing panel's "Продвинутая форма текста" window calls
`search_forms`, the renderer calls `choose_form`. The design contracts (raw text in,
tags back out; the two search entry points; pen spacing) are in the header of `mod.rs`
and in `../MODULE_README.md` ("Form search", "Inline tags").

## Architecture
- Input text is RAW (inline tags included). `inline_tags` strips them for the search
  (`<no-break>` runs become protected blocks) and `reapply_inline_tags_to_form_text`
  puts them back on the chosen form.
- Widths come from a `LineWidthMetric`: `CharWidthMetric` (no font) or `GlyphWidths`
  (`width_metrics`), which spaces pairs exactly as the renderer's pen does.
- Two entry points into the break tree:
  - `enumerate::enumerate_forms` — exhaustive DFS, bounded by `max_forms`, free memory
    (`current_available_memory` in `mod.rs`) and a node ceiling; the `choose_form` path.
  - `search::search_forms` — ranked bounded search per form height inside a width
    corridor (`params`), scored by `quality::form_quality`.
- Both prune with the shape predicates and the per-preset phase machine (`shape_match`).

## Files and submodules
- `mod.rs`: public types (`TextFormPreset`, `TextForm`, `LineWidthMetric`,
  `CharWidthMetric`, `GlyphWidths`, `MetricKerning`, `InlineTagScope`,
  `TagReapplyError`, `FormEnumeration`, `CorridorLevel`, `HyphenBudget`,
  `QualityWeights`, `FormSearchParams`), constants, the memory-probe source, internal
  search types, and the `pub use` re-exports of the public functions.
- `width_metrics.rs`: metric impls, `GlyphWidths::build` (a CUSTOM KERNING site),
  pair-unit measurement, `split_hanging_edges`.
- `inline_tags.rs`: tag stripping / no-break preparation, `scopes_strip_alike`,
  `reapply_inline_tags_to_form_text` and its whitespace / anchor walk.
- `shape_match.rs`: `is_mountain`, `is_christmas_tree`, `is_lens`, `sequence_matches`,
  `advance_step`.
- `enumerate.rs`: form-text preparation, block segmentation with protected junctions,
  `enumerate_forms` and its DFS.
- `params.rs`: corridor ladder, hyphen budget, quality weights, `FormSearchParams`
  defaults and sanitizing.
- `quality.rs`: median / unevenness / roughness statistics and `form_quality`.
- `search.rs`: width memo, bucket plan, `search_forms`, `choose_form`.
- `tests.rs`: unit tests of the directory; owns the test memory source
  (`with_memory_source`, `test_available_memory`) that `mod.rs` reads under `cfg(test)`.

## Contracts and invariants
- Public paths are `forms::<item>`; a function moved into a child is re-exported with
  `pub use` in `mod.rs`. Never rename a public item without updating `ms-tab-typing`.
- Types stay in `mod.rs`; a child item used outside its file is `pub(super)`.
- The memory-probe twins (`current_available_memory`, test and non-test) stay in `mod.rs`
  so the test substitution path `tests::test_available_memory` stays relative.
- Tag meaning comes only from `crate::inline_styles::classify_inline_tag_body`; never add
  a second tag table here.
- GUI-free and wasm-buildable: no clock deadlines, only deterministic node budgets.

## Editing map
- Line width measurement or pen-faithful kerning: `width_metrics.rs`.
- Tag stripping / restoration: `inline_tags.rs`.
- A new preset shape: `TextFormPreset` in `mod.rs` + `shape_match.rs`.
- Exhaustive enumeration and memory guard: `enumerate.rs` (+ constants in `mod.rs`).
- Search numbers: `params.rs`; scoring: `quality.rs`; the ranked search: `search.rs`.
