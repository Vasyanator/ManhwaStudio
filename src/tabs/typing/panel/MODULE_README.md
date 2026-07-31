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
`font_coverage.rs`, `presets_io.rs`, `font_settings_store.rs`, `fonts_data.rs`, ...). Edit
here for panel state/UI, font loading, and coverage; edit `render_next/` for the renderer.

## Per-font settings persistence (`fonts_data.rs` + `font_settings_store.rs`)
- App-level per-font settings live in `fonts/fonts_data.json` (`resolve_fonts_dir()`),
  a versioned document (`version: 1`) holding the user-imported system font FILE paths
  and per-font settings (currently a `display_name` override). `fonts_data.rs` owns its
  serde schema; `load_outcome` returns a typed `LoadOutcome` (`Missing` / `Loaded` /
  `Invalid`) so a corrupt file is NEVER silently degraded to empty (which the next mutation
  would then overwrite, destroying imported fonts + overrides); an unknown version still
  parses best-effort as `Loaded`. `save` is atomic AND crash-durable (temp sibling written
  via explicit `File` + `write_all` + `sync_all`, then rename; no directory fsync — the
  file is not crash-critical). `quarantine_bad_file` moves a corrupt document to
  `fonts_data.json.bad`. The stable per-font KEY (`font_settings_key`) is the fonts-dir-
  relative forward-slash path when under the fonts dir, else the absolute path.
- Virtual font groups (`VirtualFontGroup` / `VirtualFontGroupMember`) are user-defined named,
  ordered sets of REAL fonts referenced by `font_settings_key`, each with an optional per-group
  display alias. They live in the SAME document under `virtual_groups` (additive field, schema
  stays `version: 1`; an older binary that saves the file silently drops them). `fonts_data.rs`
  sanitizes them on BOTH decode and encode (`sanitize_virtual_groups`): blank names/keys dropped,
  blank aliases -> `None`, duplicate members-by-key and case-insensitive-duplicate group names
  collapsed (first wins), user order preserved. Round-trip is lossless for sane data.
- `font_settings_store.rs` is the single process-global runtime store backed by that file:
  imported paths + display-name overrides + virtual groups behind one `RwLock`, sharing ONE
  revision counter. Any mutation bumps the revision (so settings lists and typing panels reload)
  and persists the whole snapshot off the GUI thread. Persistence is SERIALIZED via a
  process-global `save_lock` and the writer thread snapshots the store AFRESH inside that
  lock, so concurrent mutations coalesce to the newest state and never race on the shared
  per-process temp file (previously ~21% of saves were lost / a stale snapshot could win).
  Startup seeding uses `load_outcome`: `Loaded` uses the file; `Missing` runs the one-time
  legacy `TextTab.imported_system_fonts` migration; `Invalid` quarantines the file then runs
  the migration (legacy key left in place, no longer written). The virtual-group mutators
  (`create_/delete_/rename_virtual_group`, `add_/remove_virtual_group_member`,
  `set_virtual_group_member_alias`) each return whether they actually changed state and follow
  the same "bump-revision-and-persist only on real change" contract. The store CANNOT see folder
  groups (filesystem), so a virtual name colliding with a real folder-group name is validated at
  the UI level, not here.
- Display-name overrides are DISPLAY ONLY: `FontEntry.display_name` (populated by the
  `fonts.rs` loaders) feeds `FontEntry::display_label()` used at presentation sites
  (`create_state::font_display_label`, and the settings font-settings rows). It never
  reaches persistence or the renderer.
- Virtual groups are injected into the panel font list by `fonts::apply_virtual_groups`,
  called at EVERY panel load site (`create_state::new` on the folder-only list, and the
  `spawn_font_reload` worker on the combined list) AFTER
  merge/disambiguation/identity-assignment. It appends each membership into the font's
  `groups` (so `font_in_group`/`filtered_font_indices` and the a95f082 ambiguous-label
  precedence govern virtual members automatically) and stores each optional per-group
  alias in `FontEntry.virtual_group_aliases`, returning the merged (real folder + virtual)
  combobox group list, case-insensitively sorted. A virtual name colliding
  case-insensitively with a real folder group is skipped with a warning. Members with no
  loaded font are silently skipped (a virtual group may have zero loaded members; the
  combo/selection code already tolerates an empty filtered list). `virtual_group_aliases`
  is DISPLAY ONLY — surfaced by `FontEntry::display_label_in_group(active_group)` and used
  only by the font-selection combo via `create_state::font_display_label`; it is never a
  resolution key, never persisted, and never sent to the renderer.

## Built-in interface font (the bundled `fonts/ui` stack as a selectable font)
- The panel font list carries ONE synthetic entry (`FontEntry.kind =
  FontEntryKind::BundledUiStack`, built by `fonts::bundled_ui_font_entry`) that offers the
  bundled `fonts/ui` stack as a normal selectable font
  (`dev-docs/unicode_base_font_plan.md`, phase 5). It points at the FIRST `core` file of
  `ms_fonts::stack()` — a real file — so the own-typeface combo preview, the advanced-form
  width metric and PSD export need no special case; the REST of the stack follows for free
  because the renderer's `MsFallback::common_fallback` is that same core chain. There is no
  "font chain" type and must not be one: `FontContent` carries the bytes of exactly one file.
- IDENTITY: `fonts::BUNDLED_UI_FONT_IDENTITY` (`"ManhwaStudio UI"`) is used as
  `identity_name`, `original_name` AND `label`. It is persisted and non-localizable
  (`dev-docs/i18n_exclusions.md` §A7). `original_name` is deliberately NOT the core font's
  real family ("Noto Sans"): otherwise a build without this entry would silently resolve an
  overlay to a user's own Noto Sans instead of degrading to `missing_font`.
- DISPLAY: `FontEntry::display_label` returns `t!("typing.fonts.bundled_ui_font_label")` for
  this entry only — the one entry whose shown name is localized while its stored name is not.
- COLLISIONS: the entry is inserted at index 0 by `fonts::prepend_bundled_ui_font` AFTER
  sorting and AFTER `assign_font_identity_names`. The panel's ordered name lookup
  (`find_font_idx_by_label_norm`) and `TabFontProvider::from_fonts` are both FIRST-wins over
  the SAME list, so a user font claiming the reserved name loses in both places (never in
  only one) and is warned about. `assign_font_identity_names` skips the entry entirely, both
  as a recipient and as a family-collision COUNT, so its presence cannot flip a user's own
  copy of a bundled family from a family-name identity to a file-stem one. The entry claims
  no file-stem alias in the provider.
- COVERAGE is reported as `Full` (never classified): the classifier can only measure the one
  file an entry points at, while this entry stands for the whole chain (core + bold + ~44
  `ext` fonts) the renderer reaches — classifying the core file alone would paint the option
  as `Partial` for languages the chain does serve.
- The ADVANCED-FORM WIDTH METRIC honours the chain for this entry:
  `create_advanced::register_bundled_core_fallback` adds the remaining `core` files to the
  metric's throwaway `fontdb` (as `Source::Binary` over the `'static` buffers `ms_fonts`
  already holds, so no I/O), and cosmic-text's last fallback stage reaches them the way the
  renderer's `MsFallback::common_fallback` does. It runs AFTER
  `metric_real_face_availability`, which must keep seeing ONLY the selected file — a chain
  face must never make an unsatisfiable Bold/Italic request look satisfiable. `ext` is
  deliberately NOT registered: the database is rebuilt on every metric-cache rebuild, on the
  GUI thread, and ~44 files would be opened and mapped each time; a script only `ext` covers
  is therefore still measured as `.notdef`.
- BYTES: `TabFontProvider` serves this entry from `ms_fonts::bytes` (the `'static` buffer
  already shared with the egui UI and the renderer's font base), so the file is not read
  twice AND `ms_text_render::font_base::resident_face_ids` recognizes the buffer as already
  registered — no duplicate face lands in any pooled `FontSystem`.
- SCOPE: PANEL lists only. `fonts::load_fonts` and `create_state::new` prepend it; the
  settings font-administration list (`font_admin::load_folder_fonts`) does NOT get it —
  there is nothing to import, rename or group about it. It is also not the DEFAULT font of a
  fresh panel: `create_state::new` selects the first of the user's OWN fonts and only falls
  back to the built-in entry when the user has none.

## Font render IDENTITY (collision-aware: family name when unique, else file-stem label)
- The canonical name persisted in `render_data.text_params` / `TextRenderParams.font_name`
  and emitted in inline `<font=...>` tags is the font's COLLISION-AWARE identity
  (`FontEntry::render_identity_name()` = `identity_name`). `fonts::assign_font_identity_names`
  computes it for the FINALIZED panel list: the ORIGINAL FAMILY NAME when that family is
  unique in the list, else the (unique-ish) file-stem `label` when two loaded FILES share a
  family (a Regular + Bold pair shipped as separate files) — so each file keeps a distinct
  persisted identity and neither renders as the other. A residual label collision keeps the
  label and logs a warning (no synthetic suffix — persisted identity must be stable). Call it
  wherever a combined panel list is finalized: `fonts::load_fonts` (combined) and
  `load_fonts_from_dir` (folder-only initial list); non-panel lists (system-font picker) keep
  the per-entry `default_font_identity_name` fallback. The file-stem `label` and any user
  display-name are for SHOWING the font (combos, lists) ONLY. Write sites: `create_render_data`
  (JSON `font_label`+`font_original_name`), `create_apply::build_render_params_for` (`font_name`),
  `create_state::font_identity_name_by_idx` (inline tags + preset `primary_font_label`).
- Resolution accepts the identity AND legacy forms with the SAME precedence on both sides:
  `TabFontProvider` keys `identity_name` PRIMARY, then family-name / label / stem aliases
  (first-wins; display-name is never a key); `create_state::find_font_idx_by_label_norm` runs the
  matching ordered whole-list passes (identity → family → label → stem) so a name that is one
  font's identity and another's alias resolves to the SAME font in the panel and the provider;
  `codec::text_render_params_from_render_data` reads `font_original_name` → `font_label` →
  `font_family` → `font` → path stem; `create_apply` panel-restore also falls back to
  `font_original_name`; `normalize_text_params_object` preserves `font_original_name`;
  `ui_helpers::font_matches_label` is the form-agnostic union (identity/family/label/stem).
- A FAILING resolve is distinguishable in the log from an unknown name: `TabFontProvider`
  reports an unreadable font file with its path and the OS reason (once per path — the
  renderer retries the resolve for every text image) and reports a poisoned cache mutex
  once, recovering the map rather than silently re-reading the file on every resolve. An
  unknown name stays a silent `None`; the renderer turns it into the missing-font error.
  Editing legacy text: `create_edit::normalize_desired_inline_tag_style` compares RESOLVED font
  identity (not raw strings) so a legacy `<font=stem>` on the base font is stripped, not duplicated.

## Character table (`char_table/`)
- A panel-owned symbol picker ("Таблица символов"): a floating window of curated
  non-language symbols that INSERTS the picked character into the panel's active text
  buffer. Full contract in `char_table/MODULE_README.md`; spec in
  `dev-docs/char_table_plan.md`.
- It belongs to the EDIT panel only: the opening button sits on the "Изначальный текст"
  header row of `create_advanced::draw_text_accordion`, and the window is pumped once per
  frame by `create_edit::drive_char_table_window` next to `draw_advanced_form_window`.
  The create panel's `char_table` field therefore stays unloaded and unbound.
- The window is a FREE FN returning a `CharTableAction`, not a `&mut self` method: it
  needs the font list, the table state and a text-buffer edit at once. The edit lands in
  `create_edit::insert_text_at_caret`, which writes the buffer named by
  `inline_text_target` at `text_selection_char_range` — the SAME notions the inline-tag
  styling uses, never a second "active field". `sync_text_selection_from_text_edit` now
  records a COLLAPSED caret while the field has focus (it used to drop it), which is what
  gives the insertion a position; an empty range is still "no selection" to every other
  reader.
- The inline `<font=...>` tag is emitted only when the picked font's
  `render_identity_name()` differs from the panel's selected font — see the LIMITATION
  note in `char_table/MODULE_README.md` (there is no caret-position style resolver, so the
  comparison is against the BASE font, not the enclosing span).

## Font model exposure (`crate::tabs::typing::font_admin`)
- The font ADMINISTRATION UI (categories, system-font import, per-font properties window)
  moved OUT of this directory to `src/tabs/settings/typesetting/`. The MODEL stays here:
  `fonts.rs` loaders, `font_settings_store.rs`, `fonts_data.rs`, `FontEntry`/`FontFaceEntry`.
- Non-typing code reaches that model ONLY through the sibling `font_admin` facade
  (`src/tabs/typing/font_admin.rs`), which wraps the loaders + store + display-name keying and
  re-exports `FontEntry` as an opaque type (fields/constructors private; `pub(crate)`
  accessors). Everything the facade wraps stays `pub(in crate::tabs::typing)`; do not widen a
  loader/store item to `pub(crate)` — add a facade wrapper instead.
- egui own-typeface registration for font previews lives in `crate::widgets::font_preview`
  (`combo_font_family_name` / `is_font_family_bound` / `ensure_font_family`), shared by
  `create_presets::ensure_combo_font_family` and the settings font UI.
- **Own-typeface rule (UI contract):** EVERYWHERE a font's name is displayed and/or the font
  is selectable (combo boxes, font lists, group members, settings rows, properties windows),
  the name must be rendered IN THAT FONT when the font is available, via the
  `crate::widgets::font_preview` helpers. Fall back to the default UI font only when the font
  cannot be registered (missing file, unreadable face). New font-name UI must follow this rule.

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

## Two font diagnostics, two questions (do not merge them)
- STATIC coverage (`font_coverage.rs`, above): "could this FONT serve the selected
  typesetting LANGUAGE at all?" — computed per font at load time, off the GUI thread,
  before any text exists. It is what ranks the options in the font combo (colors +
  `font_coverage_tooltip`) so the user can pick well BEFORE typing.
- FACTUAL fallback report (`ms_text_render::types::FontFallbackReport`, returned in
  `RenderedTextImage.font_fallbacks`): "what happened to THIS text in THIS render?" —
  which characters the renderer's deterministic fallback chain drew instead of the
  selected font and in which font, and which characters came out as tofu. It exists
  because after `dev-docs/unicode_base_font_plan.md` phase 4 "the character will not
  render" is almost never true any more, while "the character was drawn by a font you
  did not choose" became the meaningful statement.
- The panel keeps the report of the LAST COMPLETED preview render in
  `TypingCreatePanelState::preview_font_fallbacks`, replaced with the preview texture
  on success and cleared on a render error, so the diagnostic can never outlive the
  pixels it explains.
- It is DRAWN in `create_sections::draw_preview_section`, under the preview status
  row, not on the font combo: it is a property of one render of one text, whereas the
  combo's coloring is a per-font, per-language classification shared by the whole
  list. Only the create panel has a preview render (`preview_enabled`), so only it
  shows the rows.
- `create_presets.rs` maps BOTH diagnostics to colors/wording and is the only place
  that may (`font_coverage_tooltip`, `font_fallback_status_lines`, the shared
  `FONT_DIAGNOSTIC_WARNING_COLOR`/`FONT_DIAGNOSTIC_ERROR_COLOR` and
  `MAX_SHOWN_CHARS`). Falling back is INFORMATION and uses the warning color; a tofu
  character uses the error color.

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

## Settings deep link (font-group "?" help icon)
- The font-group combo (`create_main_text::draw_font_section`) has an inline `crate::widgets::HelpHint`
  "?" icon whose tooltip carries a "Перейти" action button. A click sets
  `TypingCreatePanelState::pending_settings_link_request` (a `crate::settings_shared::SettingsDeepLink`).
- Drain chain (mirrors the font-group request): `create_state::take_settings_link_request` →
  `facade::draw` drains BOTH sub-panels into `TypingTopPanelState::pending_settings_link` →
  `facade::take_settings_link` → crate-public `TypingTabState::take_settings_navigation_request`.
  `app.rs` polls it right after `typing.draw`, calls `SettingsTabState::navigate_to(link)`, and
  switches `active_tab` to `Settings`. The link is a pure payload; the typing side never touches
  settings state directly.

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
- The advanced-form width metric (`create_advanced::build_advanced_form_glyph_widths`)
  must measure the SAME face the renderer draws, so its real-Bold/Italic face request
  (`apply_metric_real_bold_italic`) MIRRORS `ms_text_render::pipeline::
  base_attrs_real_bold_italic`: real face only when `force_*` is set WITHOUT its faux
  companion; `force_* && faux_*` keeps the selected face (the renderer synthesizes the
  style geometrically). Changing one side without the other silently sizes the enumerated
  forms against a face that is never rendered. That request is ADDITIONALLY gated by
  availability (`metric_real_face_availability`): this path's `FontSystem` is a throwaway
  with an EMPTY fontdb holding only the selected font FILE, so it probes that db (the exact
  set cosmic-text can match) and SKIPS an override the file cannot satisfy, keeping the
  selected face and logging a `runtime_log` warning naming the font and the requested
  style. The metric therefore never requests a face the loaded file does not provide and
  CANNOT panic — cosmic-text 0.14.2 treats weight as a ranking key but STYLE as an exact
  `Attrs::matches` filter, so an unsatisfiable Italic request used to leave the fallback
  iterator empty and abort the GUI thread (`shape.rs` `expect("no default font found")`).
  The probe mirrors `Attrs::matches` (style + stretch, emoji faces always admitted) and
  runs the weight check under the style that will actually be requested, since cosmic-text
  filters by style before ranking by weight. REMAINING divergence from the renderer: the
  renderer's pooled `FontSystem` carries the whole system font database and can match a
  same-family Bold/Italic face living in ANOTHER file, which this empty-db metric cannot
  see; likewise a file whose heaviest face is Semibold reports no Bold. In those cases the
  metric measures the selected face, so its widths are a lower-fidelity approximation for
  real-face bold/italic (never a crash, and faux styles stay exact).
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
- To change the per-render fallback rows (wording, colors, truncation), see
  `create_presets.rs` (`font_fallback_status_lines`, `truncated_char_list`); to
  change where they are drawn, see `create_sections.rs::draw_preview_section`; to
  change WHAT the renderer reports, see `crates/ms-text-render/src/fallback_diag.rs`.
