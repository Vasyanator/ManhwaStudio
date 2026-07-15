# Module: src/tabs/typing/panel

## Purpose
The typing tab's top-panel state and UI: mode/layout management, the create/edit
parameter and effects panels, font discovery/loading, font-coverage
classification, presets, and the create-preview panel. `TypingTopPanelState`
(declared in the parent `panel.rs`) is the facade; the `impl` blocks and helpers
live in this directory's submodules.

## Architecture
`TypingTopPanelState` owns two `TypingCreatePanelState` instances (`create_panel`,
`edit_panel`), each with its own font list, selected font/face, and preview
pipeline. Font loading and coverage classification run off the GUI thread; the UI
reads cached results. The per-file catalog is maintained in the parent
`src/tabs/typing/MODULE_README.md` ("panel submodules" list) — this document only
records the directory role and the coverage/cache contract, to avoid duplication.

## Files and submodules
See the parent `MODULE_README.md` for the full per-file catalog
(`facade.rs`, `create_state.rs`, `create_*`, `fonts.rs`, `font_provider.rs`,
`font_coverage.rs`, `presets_io.rs`, `font_settings*.rs`, ...). Edit here for
panel state/UI, font loading, and coverage; edit `render_next/` for the renderer.

## Font-coverage contract (`font_coverage.rs`)
- Coverage follows the selected TYPESETTING language
  (`ms_text_util::language::text_language()`), which is independent of the UI
  language. It is pure logic (no egui): `Full` / `Partial` / `Unsupported`.
- `script_chars` come from the language's `ScriptGroup` (one Cyrillic base, one
  Latin base); `extra_chars` come from the concrete `TextLanguage` (its own
  letters plus its typography). A font covering < 50% of the script base is
  `Unsupported`; script present but some required chars missing is `Partial`.
- The Cyrillic base keeps the Russian-flavored `ъ`/`ы`/`э` so Russian's result is
  byte-identical to the historical behavior (Russian extras are frozen to
  `Ё ё « » — – … №`). Trade-off: Ukrainian/Belarusian/Serbian fonts lacking those
  letters are reported `Partial` even though those languages do not use them —
  over-strict but never wrong about the writing system.
- The `match` on `TextLanguage` (`extra_chars_for_language`) and on `ScriptGroup`
  (`script_chars_for_group`) are exhaustive with no catch-all arm: a new language
  or group must be wired here explicitly (enforced by the compiler).

## Coverage cache invalidation
- `FontEntry.coverage` is computed ONCE per font at LOAD time (in `fonts.rs`,
  off the GUI thread) against the then-current typesetting language; the dropdown
  never recomputes it.
- Because the language can change at runtime, that cache can go stale.
  `TypingTopPanelState::draw` (`facade.rs`) stores the language the cache was
  built against in `coverage_language` and, when `text_language()` differs, calls
  `spawn_font_reload` on both panels to reload the font lists and recompute
  coverage off-thread. This is self-healing: any caller of
  `ms_text_util::language::set_text_language` (including the "Тайп" settings
  typesetting-language selector) is picked up automatically on the next frame the
  typing panel draws — no explicit invalidation call from the settings UI is required.

## Contracts and invariants
- The "Параметры" sub-tab is grouped into collapsible sections via
  `create_main_text::collapsing_param_section` (six param sections + presets +
  the edit-only "Слой" transform group). Each section renders as a "header bar +
  left guide rule": a faint full-width bar (`Visuals::faint_bg_color`) behind the
  toggle/title/weak-summary header row, and an indented body with a thin faint
  vertical guide line (`Visuals::weak_text_color`) down its left. The bar uses
  the reserve-`Shape::Noop`-then-`painter().set()` trick (a `Frame` can't wrap
  `show_header` because `HeaderResponse::body` re-borrows the same `ui`); egui's
  built-in indent vline is suppressed for the body so it doesn't double the
  guide. A uniform `PARAM_SECTION_GAP_PX` trailing space keeps open/collapsed
  rhythm even. There is no floating panel heading above the sections anymore
  (the image-only edit panel, which is NOT sectioned, keeps its heading in
  `facade.rs`). Section open/closed state persists per
  `egui::Id::new((id_salt, preview_enabled))` so the create and edit panels are
  independent and state survives a UI-language switch. The `id_salt`s are literal
  persistence keys (i18n exclusions); titles/summaries are localized. The
  non-stacked ("wide") layout path is dead code (both call sites pass
  `stacked_columns = true`) kept only so the file compiles.
- Bold/italic controls preserve legacy real-face behavior by default. Faux controls
  serialize their seven `text_params` keys on every render-data rebuild; parameterized
  inline tags use the renderer's `<b=...>` / `<i=...>` grammar.
- The built-in formula-preset NAMES in `presets_io.rs::default_text_tab_formula_presets`
  (all eleven: `"Дуга (мягкая)"`, `"Наклонная линия"`, `"Волна"`, `"Спираль"`,
  `"Экспонента"`, `"Парабола"`, `"Пульс"`, `"Лемниската"`, `"Сердце"`, `"Капля"`,
  `"Вертикальная волна"`) are persisted `TextTab.formula_presets` map keys, NOT UI labels.
  They stay byte-identical Russian literals and are never localized (`docs/i18n_exclusions.md`
  §A1); translating one would double every user's built-in presets via `merge_missing`.
- Font loading and coverage classification must stay off the GUI thread
  (`spawn_font_reload` worker); `draw` only detects the change and dispatches it.
- Coverage classification is UI-free; only `create_presets.rs` maps a result to
  colors/tooltip. `font_coverage_tooltip` derives the writing-system name and
  language name from the selected `TextLanguage` (`text_language()` +
  `ScriptGroup::script_display_name` / `TextLanguage::display_name`), so the wording
  is correct for any typesetting language, not hardcoded to Russian.
- No catch-all `match` arms over `TextLanguage`/`ScriptGroup` in `font_coverage.rs`.

## Editing map
- To change what a language requires, see `font_coverage.rs`
  (`script_chars_for_group`, `extra_chars_for_language`, the `*_EXTRA_CHARS` sets).
- To change when coverage is recomputed, see `facade.rs::draw` (language-change
  detection) and `create_state.rs::spawn_font_reload`.
- To change the highlight colors / tooltip, see `create_presets.rs`
  (`draw_font_combo_option`, `font_coverage_tooltip`).
