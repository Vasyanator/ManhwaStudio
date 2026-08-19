# Module: src/tabs/typing/panel/char_table

## Purpose
State and persistence of the typing tab's character-table window ("Таблица символов"):
the curated non-language symbol set, the two favorite lists, and the background job that
answers "which loaded typing font can draw this symbol?".

Spec: `dev-docs/char_table_plan.md`. Everything except `window.rs` is the DATA layer — no
egui code; `window.rs` is the only file that draws.

## Architecture
`CharTableState` (in `mod.rs`) is one field of `TypingCreatePanelState` and owns
everything: the open flag, the selected tab, the cell size, the expanded character, the
star-popup target, both favorite stores, and the coverage job. It performs NO I/O until
the window is first opened (`ensure_loaded`), so a user who never opens it pays nothing.

Data flow per frame (once the window is wired up):
`ensure_coverage(&panel.fonts)` → spawns a worker only when the font-list fingerprint
changed → `poll()` picks up the finished `char -> Vec<font index>` map → the window reads
it through `fonts_for_char`. Every WRITE (favorites, cell size, last tab) goes through the
per-store COALESCING writer (`mod.rs::SnapshotWriter`, reached via `persist_user_config`
and the favorites stores), never on the GUI thread.

## Files and submodules
- `mod.rs`: `CharTableState` + the shared `SnapshotWriter` (and `persist_user_config`, the
  config-side snapshot) + the two `TextTab`
  settings (`char_table_font_size`, `char_table_last_group`). Edit here for window state.
- `window.rs`: the `egui::Window` — size control, tab strip, wrapping grid, expanded
  variants block, favorites star + popup. The only egui code in this directory.
- `charset.rs`: **GENERATED** by `tools/gen_char_table.py`, checked in. One
  `&'static [char]` per group plus the `CharGroup` table. Never edit by hand: change the
  ranges in the generator and re-run it.
- `favorites.rs`: both favorite stores (`GlobalFavorites`, `ProjectFavorites`), their
  documents, and their persistence.
- `coverage.rs`: the background glyph-coverage job (`CoverageJob`) and its pure mapping
  core (`compute_coverage`).

## Contracts and invariants

### The character set (`charset.rs`)
- Regenerate with `python3 tools/gen_char_table.py` (needs `fontTools`; the script STOPS
  rather than guessing coverage when no font parser is available).
- Group `key`s (`"arrows"`, `"lines"`, `"shapes"`, `"math"`, `"typography"`, `"currency"`,
  `"music"`, `"technical"`, `"game"`, `"stars_weather"`, `"emoji"`) are STABLE persisted
  identities (`TextTab.char_table_last_group`) and the i18n key suffix
  (`typing.char_table.group.<key>_label`). Never localize or rename one without a
  migration. "Избранное" is NOT a group here: it has no fixed character list, so it is a
  UI tab (`FAVORITES_TAB_KEY`) backed by `favorites.rs`.
- Three mandatory generator filters, current drop counts (1697 characters kept):
  1. **unassigned** (`unicodedata.name` raises) — dropped **6** (`U+2072`, `U+2073`,
     `U+208F`, `U+209D`, `U+209E`, `U+209F`, all reserved holes in the
     superscripts/subscripts block).
  2. **invisible/combining** (`Cc Cf Zs Zl Zp Mn Me`) — dropped **0**. Zero is expected,
     not a bug: the blocks that carry spacing/format/combining characters (General
     Punctuation, the Musical Symbols combining runs) are taken as *curated picks*, not as
     whole ranges. The filter stays as the guard for the next range widening, and
     `charset.rs`'s unit test asserts the property independently of the generator.
  3. **undrawable by any bundled `fonts/ui` font** (core + bold + ext, 49 files) — dropped
     **7** (`U+23B7`..`U+23BD`: the tall radical and horizontal-scan-line pieces of
     Miscellaneous Technical). Rationale: the table is a picker, and an entry nothing on
     this machine can render is a tofu box that also cannot be inserted usefully.
  A character claimed by two groups stays in the FIRST group that claims it, so the groups
  are disjoint by construction (⭐ `U+2B50` belongs to `stars_weather`, not to `emoji`).

### Favorites (`favorites.rs`)
- **Project list is TITLE-scoped, not chapter-scoped**: `{title_dir}/char_favorites.json`
  (`ProjectPaths::char_favorites_file`, computed in `project.rs::load_internal` next to
  `notes_file`). Every chapter of one manga therefore sees the same list — a fixed user
  decision, see `dev-docs/char_table_plan.md` §2.
- **All filesystem access to the project document goes through `crate::storage::storage()`,
  never `std::fs`.** `src/project.rs` and everything below it must keep working on the wasm
  virtual store. (`coverage.rs` is the deliberate exception: it reads APP font files, like
  `fonts.rs`, not project files.)
- A project document that cannot be used is NEVER silently replaced, and "corrupt" and
  "could not be read" are DIFFERENT: `load_project_document` returns `Missing` / `Loaded` /
  `Invalid` (malformed content) / `Unreadable` (read failed, content unknown), and the store
  mirrors them as `ProjectDocumentState`. Both failing states degrade to an empty in-memory
  list WITHOUT touching the file and refuse saves; the empty list means "unknown", never
  "the user has no favorites".
  **Only `Invalid` may be quarantined**, on the user's next explicit toggle. A transient read
  failure, a loader spawn failure and a lost load result all yield `Unreadable`, which never
  renames anything — quarantining there would move a perfectly good file out of the way.
  The destination is the first FREE `char_favorites.json.bad`, `…bad.1`, `…bad.2`, … name:
  the underlying rename replaces an existing destination (`std::fs::rename`), so reusing one
  name would destroy the earlier quarantined copy.
  **A FAILED quarantine blocks the save.** If no free destination exists or the rename fails,
  the state becomes `QuarantineFailed`, no write is scheduled, and the window says so.
  Overwriting anyway would destroy the only copy of a corrupt-but-recoverable document — the
  exact defect this state exists to prevent.
- Writes are atomic (temp sibling + rename), so a crash mid-write cannot truncate a list.
  The temp name carries the PID, which protects against another PROCESS; protection against
  this process's own concurrent writers comes from the single-writer rule below, not from
  the file name.
- A favorite is a CHARACTER ONLY, never a character+font pair. Ordering is user insertion
  order, preserved on round trip; duplicates collapse (first occurrence wins). The
  `characters` array is decoded element-wise: one junk element is skipped, it does not
  condemn the document.
- The global list lives in `user_config.json` (`TextTab.char_table_global_favorites`), read
  through `config::JsonConfig` with an EMPTY default tree (so a read can never rewrite the
  file) and written through `config::update_user_config_file`.
- **Every save goes through ONE coalescing writer per store, never a thread per change**
  (`mod.rs::SnapshotWriter`, shared infrastructure of the whole `panel` module — the typing
  tab's color presets use it too, which is why its log messages name the writer's thread
  instead of this window). A change replaces the pending COMPLETE snapshot OF ITS TARGET
  and spawns a writer only if none is live; the writer drains one target's newest snapshot
  after each write and exits when nothing is pending. This is the contract, not an
  optimization — three defects follow directly from spawning a thread per change, and all
  three are fixed by it: two toggles racing on one temp file, an older snapshot committing
  after a newer one, and one drag of the size slider queueing dozens of writes.
  Last-write-wins is correct here ONLY because every snapshot is a complete list, never a
  delta — and only WITHIN one target, which is why the pending slot is keyed by
  `SnapshotTarget::target()` (a map, not one slot): the project document follows the open
  title, so a snapshot for the previous title must never be replaced by one for the new one.
  The orderly exit clears `running` under the same lock that found the slot empty, so "no
  work" and "no writer" are one atomic fact; an UNWINDING save clears it from an RAII guard
  instead, because a stuck flag would silently stop every later write of that store.
  The writer early-returns under `cfg!(test)` so unit tests never touch disk (same guard as
  `font_settings_store::persist_off_thread`). Consequence for tests: nothing a toggle
  enqueues ever reaches disk, so the save functions (`save_project_snapshot`,
  `save_user_config_snapshot`) are covered by calling them DIRECTLY.
- `user_config.json` is read ONCE PER PROCESS (`ensure_loaded`'s flag is never reset, so it
  is the first window open that pays for it, not every open) and all three settings (cell
  size, selected tab, global favorites) are pulled from that single parse, and written back
  in ONE transaction. That read is deliberately synchronous: it is user-initiated, bounded,
  and hits a small local file. The PROJECT document is not — it lives in the user's project
  tree, which may be network-backed, so it loads in the background and arrives through
  `poll()`.
- **A failed settings read must never turn into an empty favorites list on disk.** The read
  is not retried (that would mean I/O every frame); instead the snapshot's favorites member
  is an `Option` and stays `None` for the rest of the session, which tells
  `save_user_config_snapshot` to leave `TextTab.char_table_global_favorites` exactly as it
  is. The cell size and selected tab keep persisting normally. Without this, one drag of the
  size slider after a transient `EACCES`/`EMFILE` would serialize the empty in-memory list
  over the user's real one.
- Errors: the store logs a structured message (path + OS reason) and returns a typed
  `FavoritesError`; the WINDOW owns the localized user-facing wording (no literal UI strings
  exist in this module).

### Coverage (`coverage.rs`)
- Runs on a spawned thread (it `fs::read`s every font file) and is delivered over an
  `mpsc` channel the GUI polls, mirroring `create_state::spawn_font_reload` /
  `FontReloadResult`. A stale token is discarded.
- Per font the swash charmap is built ONCE and every character is tested against it
  (`charmap.map(ch) != 0`), exactly like `font_coverage::classify_font_bytes_for`.
- Recomputed only when the font-list FINGERPRINT changes, not on every window open. The
  fingerprint is length + per entry the render identity, the CONTENT HASH and the
  representative face index. The content hash and the face index are load-bearing: an
  UNCONTESTED font keeps its PostScript name when its file is replaced by another build
  (the identity carries a `%hash` suffix only while two files contest one name), and a
  different face of one `.ttc` has a different cmap — with identity alone the cached map
  would silently keep offering a character the font no longer draws. The FILE PATH is
  deliberately NOT part of it: moving a font must not throw the whole map away and re-read
  every font.
- **The `FontEntryKind::BundledUiStack` entry is excluded from the map.** It stands for the
  whole bundled `fonts/ui` fallback chain (core + bold + ~44 `ext` files), not for the one
  file it points at, so a per-file cmap test would understate it — the same reason its
  language coverage is reported as `Full` without classification (`panel/MODULE_README.md`).
  The window instead offers it unconditionally as the FIRST variant.
- An unreadable or unparseable font file is logged and contributes nothing: claiming
  coverage it cannot deliver would offer a variant that renders tofu.

### The window (`window.rs`)
- **`draw_char_table_window` is a FREE FUNCTION taking disjoint borrows**
  (`&mut CharTableState`, `&egui::Context`, `&[FontEntry]`, `Option<&str>` base font
  identity), not a `&mut TypingCreatePanelState` method. It must read the panel font list,
  mutate this state, and cause an edit of the panel's TEXT buffer, which no single
  `&mut self` borrow can express. It therefore performs no edit: it RETURNS a
  `CharTableAction::Insert(String)` that the caller
  (`create_edit::drive_char_table_window`) applies through `insert_text_at_caret`.
- Only the EDIT panel hosts it (the button sits on the "Изначальный текст" accordion
  header), so only `edit_panel.char_table` is ever loaded or bound to a project.
- The window stays OPEN after an insertion (unlike the advanced-form window): inserting
  several symbols in a row is the normal case. Closing it collapses the expanded row and
  clears the star popup.
- **Every `ProjectDocumentState` explains itself in its own words**
  (`project_favorite_disabled_reason` / `project_favorites_status`): "no project is open" is
  the wording of `Unbound` ALONE. `Loading`, `Unreadable` and `QuarantineFailed` all occur
  with a project open, so a shared caption would state something false. `Ready` and `Invalid`
  leave the button enabled — the toggle repairs `Invalid` by quarantining first.
- `create_edit::drive_char_table_window` runs BEFORE `draw_text_accordion` sets
  `inline_text_target`, so for exactly one frame after an accordion pane switch an insertion
  would target the previously active buffer. Not reachable by a human (it needs a pane switch
  and a symbol click inside one frame) and deliberately not worked around; recorded so it is
  not rediscovered as a mystery.
- Everything the table costs — the settings/favorites reads and the coverage worker — is
  driven ONLY while the window is open (`create_edit::drive_char_table_window` gates
  `ensure_loaded`/`ensure_coverage`/`poll` on `is_open`), which is what keeps the "a user
  who never opens it pays nothing" contract true from the UI side too. The one exception
  is `set_project_favorites_path`, called per frame from `TypingTabState::draw` (mirroring
  `set_export_default_dir`); the store ignores a repeated identical path, so it costs one
  small read per TITLE change.
- **`ui_fonts::ensure_covers` is called once per frame** with the VISIBLE tab's characters
  (plus the two star glyphs, which are painted on every tab) BEFORE they are painted. The
  egui UI chain carries only `fonts/ui/core` until something asks for more, so without
  this a tab of rare symbols paints tofu. Consequence to accept: opening a tab whose
  symbols need the `ext` tier triggers the ONE-TIME ~80 MB extended-tier load. That is
  what the tier exists for, and the load runs on a worker thread — but the CALL must be on
  the GUI thread inside a frame (it panics before the first frame).
- **Registration throttle: `MAX_FONT_REGISTRATIONS_PER_FRAME = 2`.**
  `widgets::font_preview::request_font_family` makes egui rebuild its glyph atlas, and
  egui's `add_font` never evicts — a symbol covered by forty loaded fonts would otherwise
  mean forty queued file reads plus forty atlas rebuilds around ONE frame. `is_font_family_bound` decides without paying for a
  registration; cells not yet bound draw the glyph in the UI font meanwhile and the window
  calls `ctx.request_repaint()` while any registration is outstanding. A slot buys a QUEUED
  read plus a deferred `add_font`, never a blocking read — `request_font_family` hands the
  `fs::read` to a background loader and only the registration happens on the GUI thread.
  Remembering a font that cannot be used is `widgets::font_preview`'s job, not this
  window's: it keeps an egui refusal per `Context` and a read error process-wide, and logs
  each once, so an unreadable file is neither re-read nor re-logged per frame.
- **Tag emission**: a variant click inserts `<font={identity}>{ch}</font>` ONLY when
  `identity` differs from the font in effect at the caret, else the bare character. The
  identity comes from `FontEntry::render_identity_name()` (or
  `fonts::BUNDLED_UI_FONT_IDENTITY` for the always-first built-in cell), NEVER from
  `label`/`display_label`, and the comparison is case-insensitive like
  `create_edit::normalize_desired_inline_tag_style`.
  **LIMITATION:** "the font in effect at the caret" is approximated by the panel's SELECTED
  font. No helper resolves an effective inline style for a BARE CARET —
  `inline_selection_context` requires a NON-EMPTY selection — so inserting inside a
  `<font=Other>` span emits a redundant (but correct) tag instead of none. Lifting this
  needs a caret-position style resolver next to `inline_selection_context`.
- The star's hover state comes from `Response::hovered()`/`contains_pointer()`, never from
  a raw pointer read (`egui-docs/06-overlays.md` §5), and its hitbox exists only while the
  star is visible, so an unhovered cell has no invisible corner that swallows its click.
  The popup uses `PopupKind::Tooltip`, not `Menu`: a `Menu` popup lives on
  `Order::Foreground`, i.e. UNDER this window, which sits on `Order::Tooltip` (it has to,
  to float above the typing panels).

## Editing map
- To change WHICH characters the table offers, edit `GROUP_SPECS` in
  `tools/gen_char_table.py` and re-run it; update the drop counts above.
- To change how favorites are stored or recovered, see `favorites.rs`.
- To change when/what coverage is computed, see `coverage.rs` (`CoverageJob::ensure`,
  `compute_coverage`).
- To change window state or the persisted settings, see `mod.rs`; to change what the
  window LOOKS like or how it behaves, see `window.rs`; to change where it is drawn from
  or what an insertion does to the text, see `create_edit.rs`
  (`drive_char_table_window` / `insert_text_at_caret`).
- To change where the project document lives, see `project.rs::load_internal` and
  `config::CHAR_FAVORITES_FILE`.
