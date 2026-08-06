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
  carries the name-display switch (see below) — the widget `draw_name_mode_switch` and the
  name selectors live here and are reused by `font_groups.rs`;
  and hosts a
  searchable, row-virtualized picker over `font_admin::load_system_catalog()` to import a system
  font (`font_admin::add_imported_font` / `remove_imported_font` / `is_font_imported`).
  Folder/imported rows are BUTTONS that open the per-font properties window. Owns the
  `font_groups::FontGroupsEditorState` rendered as the fourth "Группы" category. Pure helpers
  `font_row_matches` (`pub(super)`, reused by `font_groups`) / `clean_font_display_name` /
  `font_row_name_for_mode` / `unavailable_row_name` are unit-tested.
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
  group-editor `egui::Window` (pinned `Id`, `min_width` sized around the member table) holding a
  rename field, a virtualized member TABLE, an inline add-member picker mirroring the
  import-picker body, and ONE "Применить" button at the bottom (see "Group-editor edit model"
  below). The MEMBER TABLE is an `egui::Grid` inside `ScrollArea::show_rows` (`Grid::start_row`
  keeps the grid's row bookkeeping on the absolute row index) with four columns: the font's
  user-facing name in its OWN typeface, its identity (PostScript name) in the interface font,
  the per-group alias field, and the remove button. Column widths are EXPLICIT (`MemberColumns`,
  each cell allocated through the `table_cell` helper that pins its own width) — the alias and
  remove columns are fixed, the two name columns split the rest evenly down to
  `MIN_NAME_COL_WIDTH`, and names longer than their column are truncated with the full text in
  the hover. Content-sized columns would have staggered (row 1 is drawn in a different typeface
  per row) and would have resized while the virtualized list scrolled; fixed widths also let the
  header row, which must live OUTSIDE the scroll area, share one geometry with the rows. A member
  identity that matches no loaded font is shown greyed in BOTH name columns (falling back to the
  stored identity, `Custom` preferring a surviving display-name override) and is never
  auto-removed. The member
  names and the picker candidates render in their OWN typeface via the shared
  `crate::widgets::font_preview` helpers, VISIBLE rows only, bounded by the shared
  `PICKER_PREVIEW_FONT_CAP` (reused from `font_settings.rs`). The loaded font data the section
  draws from travels as one `GroupEditorFonts<'_>` borrow (folder-group names, the two loaded
  categories, the snapshot revision) instead of four parallel parameters.
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

## Group-editor edit model
- The rename field and every per-member alias field are BUFFERS. Nothing they hold reaches the
  store until the window's SINGLE "Применить" button (or Enter in one of those fields) commits
  it; there are no per-row apply buttons and no apply button on the rename row. The button is
  `add_enabled(has_pending_changes)`, so "there is something unsaved" is readable from the
  button alone, and both states explain themselves on hover.
- `apply_changes` writes each CHANGED alias first and the rename last, on purpose: aliases are
  addressed by the group's CURRENT name, so a rejected rename (blank name, or a collision with
  a virtual or real folder group — validation and its red message are unchanged) still leaves
  the alias edits applied. A blank buffer clears the alias; buffers are compared trimmed, so
  whitespace alone never arms the button.
- REMOVING a member stays IMMEDIATE — it is a membership operation, not a text edit. Its alias
  buffer is dropped with it, and `sync_alias_bufs` (run every frame against the store's member
  list) both seeds missing buffers and prunes any whose member is gone, including one removed
  from another surface. Buffers are keyed by font IDENTITY, so a stale one could never be
  applied to a different row.
- Closing the window DISCARDS every uncommitted buffer along with the editor state. There is no
  confirm prompt: the window is reopened from a one-click "Изменить" button and every buffer is
  reseeded from the store, so the cost of losing an edit is retyping it.

## Font-name display switch
- Three independent switch surfaces (`FontListKind::{Folder, Imported, Group}` ×
  `FontNameDisplayMode::{Custom, Identity}`, both in `font_settings.rs`): the folder list and
  the imported-system list each carry their own above their rows, and the group editor carries
  one INSIDE its add-member picker — that picker is the only list there with a name to choose,
  since the member table shows the user-facing name and the identity in adjacent columns. `Custom` draws
  `FontEntry::display_label()` (user rename → file-stem label) — the historical behavior and
  the default; `Identity` draws `FontEntry::render_identity_name()`, i.e. the PostScript name
  every persisted document uses, `%hash` collision suffix included. The switch changes the TEXT
  only: the row is still painted in the font's own typeface, since the preview registration is
  keyed by identity + content hash regardless of what is written. A font whose file declares no
  spec-valid PostScript name has no identity of its own and shows the documented identity
  FALLBACK (family, else file stem) — that fallback IS the name the app uses for it, so the row
  stays truthful; the switch's hover text says so.
- The switch widget (`draw_name_mode_switch`) and both name selectors
  (`font_row_name_for_mode` / `unavailable_row_name`) live in `font_settings.rs` and are
  `pub(super)`; `font_groups.rs` reuses them rather than duplicating the choice, so all three
  surfaces cannot drift apart. Each surface's `switch_id_salt` keeps the two switches that can
  be on screen at once from sharing egui widget state.
- The group editor's switch selects the FONT NAME only, and only in the ADD picker. The member
  TABLE is mode-INDEPENDENT: it calls the same two selectors with a fixed mode per column
  (`Custom` for "Имя шрифта", `Identity` for "Название шрифта"), so the two surfaces still name
  a font through one implementation. A member's per-group ALIAS
  (`FontEntry::display_label_in_group`, what the typing panel shows while that group is active)
  is NOT a third name here: it stays the separate value edited by the field on the same row.
  Substituting it for the name would leave the user editing a field whose effect they cannot
  see beside it, and would hide which font the row actually is.
- A row with no loaded `FontEntry` — an UNAVAILABLE imported entry, or a group member whose
  font is not currently loaded — shows what the document records: the stored identity in both
  modes, except that `Custom` prefers the user's display-name override for that identity when
  one exists. An unavailable imported row whose stored identity is blank falls back to the
  recorded path; a blank member identity falls back to the unnamed-font placeholder. A member
  row whose shown name is not the identity puts the identity in its hover text, so the greyed
  row still carries the clue.
- PERSISTENCE lives in `user_config.json` (`TextTab.font_list_name_mode_folder` /
  `…_imported` / `…_group`), NOT in `fonts/fonts_data.json`: this is an interface preference of
  a list, not a property of a font, and `fonts_data.json` is keyed by font identity and
  revision-bumped (a write there would force every cached font list to reload).
  `SettingsTabState::new` reads all three modes once
  (`settings::load_font_name_display_modes`) and injects them plus the config path into
  `FontSettingsEditorState::new`, so the widget itself performs no I/O; a switch flip applies
  live and is written back by `settings::save_font_name_display_mode` on a worker thread
  (best-effort, logged on failure). `FontSettingsEditorState` owns ALL three modes: the group
  editor borrows its slot (`&mut FontNameDisplayMode`) for the frame — reaching its add-member
  picker, the only place it is now read — and the owner compares
  around the call to catch the flip — `font_groups.rs` holds no preference state and writes no
  files.
- SEARCH is mode-INDEPENDENT everywhere. The two category lists have no search box; the import
  picker and the group add-picker do, and their predicate `font_row_matches` ORs label /
  original name / display label / IDENTITY, so a row stays findable by every name it can be
  shown under. Narrowing the predicate with the mode would silently drop rows the user had
  already found without retyping the query.

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
