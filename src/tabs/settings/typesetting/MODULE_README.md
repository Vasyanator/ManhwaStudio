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
  three font categories (folder / imported system / custom) AND the real folder-group names
  off the GUI thread via `font_admin::{load_font_lists, list_folder_group_names}`, reloading
  live when `font_admin::fonts_revision()` advances. `load_font_lists` is ONE combined pass on
  purpose: the folder and imported categories must carry the identities the typing panel
  resolves, and only a merged list can assign them (two independent passes hide every
  cross-source name collision, so settings showed and wrote a bare identity where the panel had
  assigned a `%hash`-suffixed one, and group membership / display-name overrides written here
  silently did nothing). The imported category is rendered from `ImportedFontRow`s, one per
  DOCUMENT entry: a font whose file is missing, unparsable, replaced, or path-less is shown
  greyed with a localized reason and keeps its remove button — that entry is otherwise
  impossible to get rid of, since nothing else prunes `fonts_data.json`. An unavailable row now
  also means the font is not installed under that PostScript name EITHER: one that merely MOVED
  is located by name and comes back as a normal row (the localized reason still describes what
  happened at the recorded path, which is the half the user can act on). The picker's
  system-font catalog load doubles as the refresh of that by-name index — see
  `src/tabs/typing/panel/MODULE_README.md`. The remove button
  passes `row.stored_identity` (the document key), NOT the loaded font's render identity, which
  may carry a collision suffix. Draws
  each font's name in its own typeface (`crate::widgets::request_font_family`, keyed by the
  font's IDENTITY — `FontEntry::render_identity_name` — PLUS its `content_hash`, with the
  path as the byte source only; the hash is what retires a binding whose file was replaced,
  and is `0`/"unknown" for the picker catalog, which never reads whole files);
  and hosts a
  searchable, row-virtualized picker over `font_admin::load_system_catalog()` to import a system
  font (`font_admin::add_imported_font` / `remove_imported_font` / `is_font_imported`).
  Folder/imported rows are BUTTONS that open the per-font properties window. Owns the
  `font_groups::FontGroupsEditorState` rendered as the fourth "Группы" category. Pure helpers
  `font_row_matches` (`pub(super)`, reused by `font_groups`) / `clean_font_display_name` are
  unit-tested.
- `font_groups.rs`: `FontGroupsEditorState`, the "Группы" section — create/list/rename/delete
  VIRTUAL font groups and edit their members. Reaches the model ONLY through the virtual-group
  facade (`font_admin::{list_virtual_groups, create/delete/rename_virtual_group,
  add/remove_virtual_group_member, set_virtual_group_member_alias}`), caching the snapshot and
  refreshing on `fonts_revision()`. Members are addressed by font IDENTITY throughout — the
  member-row resolver, the alias buffers and the add-picker selection are all identity-keyed. BOTH create AND rename validation reject a blank name or a
  case-insensitive collision with an existing virtual group OR a real folder-group name (the
  folder names are passed in from `font_settings.rs`'s off-thread pass, and threaded into the
  editor window for rename); each surfaces a red error near its row. Deletion uses an inline
  two-step confirm (NOT a child-viewport modal — those have an input-routing bug here); the
  confirm is guarded against a physical double-click and auto-disarms when the pointer leaves the
  armed button. Owns the
  group-editor `egui::Window` (pinned `Id`, follows the group name on rename) with a virtualized
  member list (per-member alias edit + remove; a member identity that matches no loaded font is
  shown greyed — the stored identity is the clue the user needs — and is never auto-removed) and an inline add-member picker mirroring the import-picker body. The member
  names and the picker candidates render in their OWN typeface via the shared
  `crate::widgets::font_preview` helpers, VISIBLE rows only, bounded by the shared
  `PICKER_PREVIEW_FONT_CAP` (reused from `font_settings.rs`).
- `font_properties_window.rs`: the per-font PROPERTIES window (`FontPropertiesState`, one open
  at a time on `FontSettingsEditorState`). Identity header — family name, the render IDENTITY
  (the PostScript name the project persists) and the file/face — an editable display-name override
  (wired to `font_admin::set_display_name_override(identity, ..)`; the window's `path` is only the
  byte source of the analysis and the preview),
  a live own-typeface preview, a virtualized glyph grid, and a collapsible kerning-pair list.
  The glyph inventory + kerning are extracted OFF the GUI thread via `ttf-parser` (cmap
  codepoints confirmed by `glyph_index`; `kern` Format 0 + GPOS `PairAdjustment` Format 1/2 over
  a capped glyph probe set), delivered over an `mpsc` channel the window polls. Pure extraction
  helpers are unit-tested; the end-to-end `analyze_font_bytes` has no fixture-font test (a
  permissively-licensed test font with known `kern`/GPOS pairs was out of scope — add a golden
  test when one is available).

## Font-groups deep-link reveal
- `draw_typesetting` wraps its whole body in a `ScrollArea::vertical` (id_salt
  `settings.typesetting.scroll`) — required so `scroll_to_me` reveal targets have an ancestor
  ScrollArea to consume; the pane had none before, so long content was cut off.
- While `SettingsTabState::pending_reveal == Some(TypesettingFontGroups)` (set by `navigate_to`),
  `draw_typesetting` threads a `force_reveal` flag down `FontSettingsEditorState::ui` →
  `draw_categories` → `FontGroupsEditorState::ui`: BOTH `CollapsingHeader`s ("Настройки шрифтов",
  "Группы") get `.open(Some(true))` and the groups header calls `scroll_to_me(Align::TOP)`. The
  groups block rect (header+body union) bubbles back up the call chain. The pending flag is
  consumed on the FIRST frame that rect actually comes back — the font categories load
  asynchronously, so on a first visit the nested groups header may not exist for a few frames and
  consuming earlier would silently lose the reveal. A bounded wait (`REVEAL_PENDING_TIMEOUT`, via
  `pending_reveal_expires`) abandons the reveal if the load never completes, so the force-open can
  never stick. Once consumed, `.open(None)` leaves the persisted collapsed state alone (the user
  can re-collapse).
- `reveal_highlight_until` (a `web_time::Instant` on `SettingsTabState`, NOT egui memory) arms a
  ~2 s outline painted by `paint_reveal_highlight` around the groups rect on an `Order::Tooltip`
  layer painter (pure paint, no hitbox), clipped to the pane ScrollArea's viewport (`inner_rect`)
  so a partially-scrolled-out block never paints over the tab bar, fading over the final
  `REVEAL_HIGHLIGHT_FADE_SECS` and requesting repaints until expiry.

## Contracts and invariants
- UI ONLY: no font-model logic lives here. The single sanctioned entry point to typing's font
  administration is `crate::tabs::typing::font_admin`; do not add or reach for any other typing
  internal. egui font-preview registration uses `crate::widgets::font_preview`.
- Own-typeface rule: wherever a font's name is displayed and/or the font is selectable, render
  the name in that font itself when available (see the contract in
  `src/tabs/typing/panel/MODULE_README.md`, "Font model exposure").
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
- To change the per-font properties window (rename editor, glyph grid, kerning viewer, the
  per-font "Группы" membership section), edit `font_properties_window.rs`.
- To change the "Группы" section or the group-editor window, edit `font_groups.rs`.
- To reach a NEW piece of font-model state, add a wrapper to `crate::tabs::typing::font_admin`
  first, then call it here — never widen a typing internal to `pub(crate)`.
