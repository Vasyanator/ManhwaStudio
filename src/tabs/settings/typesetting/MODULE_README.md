# Module: src/tabs/settings/typesetting

## Purpose
The "Тайп" settings pane: text-typesetting options plus the app's font-administration UI
(font categories, system-font import, per-font display-name/glyph/kerning inspection). This
is a UI-only module — the font MODEL lives in `crate::tabs::typing`.

## Architecture
`mod.rs` is the pane orchestrator: it renders the Ctrl+wheel rotation chooser, the shared
typesetting-language selector, the hanging-punctuation editor, and two collapsed blocks — the
effect-defaults editor (`crate::tabs::typing::EffectDefaultsEditorState`, a typing-panel widget)
and the font-settings block (`FontSettingsEditorState`, owned here). The methods hang off
`SettingsTabState` (declared in the parent `settings` module).

The font UI reaches the font model through ONE narrow facade,
`crate::tabs::typing::font_admin`, and the shared egui font-registration helpers in
`crate::widgets::font_preview`. It imports NOTHING else from the typing internals: the
loaders, the imported-fonts store, the `fonts_data.json` schema, and the display-name keying
scheme all stay `pub(in crate::tabs::typing)`; `FontEntry` is exposed only as an opaque
re-export whose fields/constructors stay private (external reads go through its `pub(crate)`
accessors). All heavy font enumeration runs on worker threads; the GUI only polls.

## Files and submodules
- `mod.rs`: pane orchestrator (`SettingsTabState::draw_typesetting` +
  `draw_rotation_ctrl_wheel_setting`) and the `font_settings` / `font_properties_window`
  submodule declarations. Re-exports `FontSettingsEditorState` to the parent `settings` module.
- `font_settings.rs`: `FontSettingsEditorState`, the "Настройки шрифтов" widget. Loads the
  three font categories (folder / imported system / custom) off the GUI thread via
  `font_admin::{load_folder_fonts, load_imported_fonts}`, reloading live when
  `font_admin::fonts_revision()` advances; draws each font's name in its own typeface
  (`crate::widgets::ensure_font_family`); and hosts a searchable, row-virtualized picker over
  `font_admin::load_system_catalog()` to import a system font
  (`font_admin::add_imported_font` / `remove_imported_font` / `is_font_imported`). Folder/imported
  rows are BUTTONS that open the per-font properties window. Pure helpers `font_row_matches` /
  `clean_font_display_name` are unit-tested.
- `font_properties_window.rs`: the per-font PROPERTIES window (`FontPropertiesState`, one open
  at a time on `FontSettingsEditorState`). Identity header, an editable display-name override
  (wired to `font_admin::set_display_name_override(&Path, ..)` — the facade computes the key),
  a live own-typeface preview, a virtualized glyph grid, and a collapsible kerning-pair list.
  The glyph inventory + kerning are extracted OFF the GUI thread via `ttf-parser` (cmap
  codepoints confirmed by `glyph_index`; `kern` Format 0 + GPOS `PairAdjustment` Format 1/2 over
  a capped glyph probe set), delivered over an `mpsc` channel the window polls. Pure extraction
  helpers are unit-tested; the end-to-end `analyze_font_bytes` has no fixture-font test (a
  permissively-licensed test font with known `kern`/GPOS pairs was out of scope — add a golden
  test when one is available).

## Contracts and invariants
- UI ONLY: no font-model logic lives here. The single sanctioned entry point to typing's font
  administration is `crate::tabs::typing::font_admin`; do not add or reach for any other typing
  internal. egui font-preview registration uses `crate::widgets::font_preview`.
- Do not block the GUI thread: font enumeration and font-file analysis run on worker threads,
  results polled over `mpsc`.
- i18n: the font-settings strings keep the HISTORICAL `typing.font_settings.*` key namespace
  (the widget used to live under the typing panel). The namespace was intentionally NOT renamed
  on the move — renaming ~21 keys would churn every locale for no user benefit. UI-visible
  strings go through `t!`/`tf!`; any localized `CollapsingHeader`/`Window` carries a stable
  `id_salt`/`Id` so widget/window ids do not follow the translated text.

## Editing map
- To change the pane layout or the non-font blocks, edit `mod.rs`.
- To change the font list, categories, or import picker, edit `font_settings.rs`.
- To change the per-font properties window (rename editor, glyph grid, kerning viewer), edit
  `font_properties_window.rs`.
- To reach a NEW piece of font-model state, add a wrapper to `crate::tabs::typing::font_admin`
  first, then call it here — never widen a typing internal to `pub(crate)`.
