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
  a versioned document. `fonts_data.rs` owns its serde schema; `load_outcome` returns a typed
  `LoadOutcome` (`Missing` / `Loaded { data, fingerprint }` / `Invalid`) so a corrupt file is
  NEVER silently degraded to empty (which the next mutation would then overwrite, destroying
  imported fonts + overrides); a NEWER version still parses best-effort as `Loaded`.
  `save_checked` is atomic AND crash-durable through the SHARED recipe `doc_store::write_atomic`
  (temp sibling written via explicit `File` + `write_all` + `sync_all`, handle CLOSED, then
  rename), asking for `Durability::Contents` — no directory fsync, because nothing deletes a
  data source after this write and it happens on every debounced profile edit. `doc_store` also
  owns `DocumentFingerprint` / `SaveBaseline`, which `fonts_data` re-exports under their
  historical names and `presets_store` uses for the same guard.
- **THE VERSION IS DECIDED BY CONTENT WHEN THE FIELD IS ABSENT.** A document carrying
  `system_fonts`/`fonts` but no `version` is v2. Serde's `0` default made it "≤ 1" i.e. legacy,
  and the legacy decoder read only v1 keys — so such a document came back EMPTY and the next
  save wrote that emptiness over everything in it. Both payload shapes are decoded and UNIONED
  (v2 wins a key clash), so a half-migrated or hand-edited file loses neither half.
- **THREE WRITE GUARDS** (`fonts_data::guard_existing_document`, driven by
  `font_settings_store::save_snapshot_now`), each of which used to be a silent loss:
  - a document from a NEWER schema is never overwritten (`SaveError::NewerVersion`). Refusing
    beats preserving unknown fields through a flatten bag: stamping `"version": 2` onto a
    half-v99 payload produces a document that is neither, and unknown fields may reference the
    very keys the migration re-keys. The refusal is reported as an error, not swallowed.
  - a document that changed since the caller's `SaveBaseline` fingerprint (a SECOND running app
    instance wrote it) is reported as `SaveError::Conflict` WITH the parsed on-disk document;
    the store merges it in (`merge_disk_into_state`: additive — theirs is added, ours is kept)
    and retries once. The accepted asymmetry is that a deletion by the other instance can come
    back, which is the same "never destroy the last clue" bias as everywhere else here.
  - a corrupt document that could NOT be quarantined disables persistence for the session.
    `quarantine_bad_file` tries `rename` → `copy` → `QuarantineOutcome::Failed`; only on
    `Failed` is the original still the sole copy, and then nothing may rename over it.
- **SCHEMA 2 — the font is named by its IDENTITY, never by a path.**
  ```jsonc
  { "version": 2,
    "system_fonts": [ { "font": "Roboto-Medium", "last_path": "/home/…/Roboto-Medium.ttf" } ],
    "fonts": { "CCWildWordsLower-Regular": { "display_name": "Разговор", "profile": { … } } },
    "virtual_groups": [ { "name": "Возлюбленная",
                          "members": [ { "font": "kCCAskForMercy-Regular", "alias": "Основа" } ] } ] }
  ```
  `fonts` keys and `members[].font` are `FontEntry::render_identity_name()` values, so MOVING OR
  RENAMING a font file no longer drops its display name, its profile or its group membership —
  only editing the font's own PostScript name does. Unset fields (`display_name`, `profile`,
  `alias`, `last_path`) and empty collections are OMITTED; a per-font record left with nothing is
  dropped rather than written.
- `system_fonts[].font` is the imported font's UNSUFFIXED identity (its PostScript name): that
  entry names a FILE's face, while the `%hash` contest suffix is a property of one panel LIST.
  **THE NAME LOCATES THE FONT; `last_path` IS ONLY A HINT.**
  `fonts::load_imported_system_font_rows` resolves one entry in three steps:
  1. the recorded `last_path` still exists AND its PostScript name still matches → use it, with
     ONE file read and no system scan (a file REPLACED by a different font does not match and is
     never silently substituted);
  2. otherwise look the name up in the process-global system-font name index (below); on a hit
     the entry is loaded from there and the hint is rewritten
     (`font_settings_store::set_system_font_path`), so moving, renaming, repackaging or updating
     a system font re-links it automatically and the NEXT launch resolves at step 1 again;
  3. otherwise the entry stays in the document and the row carries the typed reason the HINT
     failed — the store entry is never pruned for being unavailable.

  A hint whose name is not recorded yet (a legacy entry) is accepted at step 1 and the name is
  LEARNED (`learn_system_font_identity`); such an entry cannot reach step 2, because there is no
  name to look up.
- **THE SYSTEM-FONT NAME INDEX** (`fonts::SystemFontNameIndex` / `system_font_name_index`).
  `fontdb` has no PostScript-name query (only `Family::Name`), so the index is our own linear
  pass over `fontdb::Database::load_system_fonts()`'s faces — the same pass the import picker's
  catalog already runs. It maps normalized name → candidate FILES (a list: on a typical desktop
  a handful of names are claimed by two files, e.g. the variable and the static cut of one
  family), skipping any face whose declared name is not spec-valid, since such a name counts as
  absent everywhere else too.
  - BUILT LAZILY, CACHED FOR THE PROCESS, and only ever off the GUI thread (its two callers are
    the panel's font-reload worker and the settings pane's off-thread list load). Step 1 above
    is what makes laziness matter: a document whose hints all resolve never scans at all.
    Measured on the maintainer's machine: ~90 ms for 2276 faces / 2260 distinct names.
  - CONCURRENT first callers do not each scan — the build is serialized and the cache re-checked
    inside the lock, so several worker threads share one snapshot.
  - REFRESHED by `fonts::load_system_fonts`, which publishes the catalog it just enumerated:
    the import picker is opened exactly when the user has been installing or removing fonts, so
    that load doubles as the explicit rebuild and costs nothing extra.
  - COLLISION RULE (`locate_system_font_by_identity`): every candidate is READ and confirmed to
    still claim the name, and the winner is the LOWEST CONTENT HASH, ties broken by the
    lexicographically first path. Same rule as the identity contract's "a bare contested name
    resolves to the lowest-hash claimant", so the located file is the file that name means
    everywhere else; and being a function of the candidates' BYTES rather than of enumeration
    order (which follows directory iteration), it picks the same file on every run.
    `locate_system_font_file_by_identity` is the `pub(in crate::tabs::typing)` wrapper the
    `font_admin` facade calls; it keeps only the CONFIRMED identity + path, because the parsed
    font data the private lookup returns must not leave this module.
  - In TEST builds the enumerator is stubbed: it returns only what a test installed
    (`test_install_system_faces`), never the machine's real fonts, so no unit test depends on
    what happens to be installed. `test_system_font_index_builds` is what pins "step 1 does not
    scan".
- **EVERY stored imported font produces a ROW** (`ImportedSystemFontRow`: stored identity, path
  hint, the loaded entry or a typed `ImportedFontUnavailable`). A row is what carries the remove
  action, so an import whose file went missing, became unparsable, or was replaced by a different
  font is finally visible AND removable in the settings list. Skipping it — the previous
  behavior — left a document entry nothing could ever prune. The row's `stored_identity` is the
  DOCUMENT key (`remove_imported_system_font` matches it); the loaded entry's
  `render_identity_name()` may carry a `%hash` suffix and would match nothing there.
- **ONE COMBINED LIST FOR BOTH CONSUMERS.** `fonts::build_combined_font_list(fonts_dir, refs)`
  builds folder + imported, merges byte-identical copies across the two sources, sorts, assigns
  the collision-aware identity, runs the deferred migration and applies display-name overrides —
  once. `load_fonts` prepends the bundled entry on top of it for the PANEL;
  `font_admin::load_font_lists` splits it back into categories for the SETTINGS pane (folder =
  representative path under the fonts dir). Building the settings categories independently hid
  every cross-source name collision from them: the folder-only pass showed the bare identity
  where the panel had assigned a suffixed one, so a group membership or display-name override
  written in settings matched no panel entry and silently did nothing.
- **DEFERRED v1 MIGRATION.** A `version: 1` document keys everything by FILE PATH. Re-keying it
  needs a `path → identity` map, which does not exist until fonts have been parsed — long after
  the store is seeded at startup. So: `fonts_data` decodes v1 VERBATIM with
  `FontsData.pending_migration`, and `fonts::run_pending_fonts_data_migration` calls
  `font_settings_store::migrate_legacy_font_keys` at the END of the COMBINED font-list build,
  where every entry carries its final identity.
  - **ONLY THE COMBINED PASS MAY RUN IT.** `fonts::folder_font_entries` — the folder-only
    subset — deliberately runs neither the migration nor the display-name overrides. Its
    identities are PRE-COLLISION: a folder font contested by an IMPORTED system font looks
    uncontested there, so the migration re-keyed its legacy key to the BARE name; the
    combined pass then suffixed both claimants, and the re-key — one-way, with the bare name
    already counted as an "already migrated" identity — was never redone. The user's
    display-name override or virtual-group membership then hung on an identity no final entry
    carries. `fonts::legacy_font_settings_key` is the read-only remains of
  the v1 keying rule and exists only to build that map.
  - **COMPLETION RULE: only when EVERY legacy reference resolved.** "The list looked complete" is
    not a licence to finish (it used to be): a font that merely happened to be unreadable during
    one launch would have its settings frozen — or, worse, re-keyed to a stem-derived guess —
    permanently. An EMPTY resolution (no fonts loaded at all) never finishes it either.
  - **PLACEHOLDER ENTRIES RESOLVE NOTHING.** A file that could not be read or parsed this run is
    still LISTED, but its "identity" is the family-or-file-stem FALLBACK. It is excluded from
    `LegacyKeyResolution` entirely, so `groups/ВВД/Основа.ttf` (really `CCWildWordsLower-Regular`)
    can never be re-keyed to `Основа` because it was briefly unreadable.
  - **THE PENDING FLAG IS PERSISTED** (`fonts_data`'s v2 `pending_migration`, written only when
    true). The migration rewrites the document in the CURRENT schema, so without the flag the
    next launch would read a v2 document, never retry, and freeze the unresolved keys while the
    log promised they "will apply again".
  - A key that already IS a loaded font's identity needs no translation, so a second pass
    (the combined list re-running after a folder-only one) does not report what the first pass
    just converted as lost — `LegacyKeyResolution.identities` is what tells the two apart.
  - A key that resolves to nothing is KEPT VERBATIM and logged — it is the only remaining clue
    about the font it meant, and it may resolve after the user reinstalls that font. Two legacy
    keys collapsing onto one identity MERGE FIELD-WISE (`merge_settings_record`: the two keys of
    a merged duplicate can each hold half the settings — one the display name, the other the
    profile), and a collapsing group member keeps the first non-empty ALIAS
    (`merge_group_member`). Only a field both records set and that actually differs is dropped,
    and that loss is warned about individually — logging is not saving.
  - The rewrite goes through the store's normal off-thread atomic save and does NOT bump the
    revision (it is a re-encoding, not a user-visible change; a bump would force a redundant font
    reload right after startup). If the app closes before the pass runs, nothing is lost: the file
    is still v1 (or v2 with the pending flag) and the next launch retries.
- Virtual font groups (`VirtualFontGroup` / `VirtualFontGroupMember`) are user-defined named,
  ordered sets of REAL fonts referenced by IDENTITY, each with an optional per-group display alias.
  `fonts_data.rs` sanitizes them on BOTH decode and encode (`sanitize_virtual_groups`): blank
  names/keys dropped, blank aliases -> `None`, duplicate members-by-key and case-insensitive-
  duplicate group names collapsed (first wins), user order preserved. Round-trip is lossless for
  sane data.
- `font_settings_store.rs` is the single process-global runtime store backed by that file:
  imported system fonts + per-font records + virtual groups + the pending-migration flag behind
  one `RwLock`, sharing ONE revision counter. Any user-visible mutation bumps the revision (so
  settings lists and typing panels reload) and persists the whole snapshot off the GUI thread.
  BATCH mutators exist for the bulk paths (`add_imported_system_fonts`,
  `add_virtual_group_members`): one write-lock section, ONE bump and ONE persist per batch —
  a per-entry loop would send every open panel through a font reload per added entry. They skip
  what already exists (including a duplicate inside the batch itself), keep an existing member's
  alias, refuse a blank identity with a log warning, and bump/persist nothing when nothing was
  added.
  Persistence is SERIALIZED via a process-global `save_lock` and the writer thread snapshots the
  store AFRESH inside that lock, so concurrent mutations coalesce to the newest state and never
  race on the shared per-process temp file. Startup seeding uses `load_outcome`: `Loaded` uses the
  file; `Missing` runs the one-time legacy `TextTab.imported_system_fonts` migration; `Invalid`
  quarantines the file then runs the migration (the key is never written again, and is deleted
  only by `presets_store::drop_migrated_user_config_keys`, see the create-preset section). The
  store CANNOT see folder groups (filesystem), so a virtual name colliding with a real
  folder-group name is validated at the UI level, not here.
- **The DEFAULT per-font profile** lives at `fonts.<identity>.profile` — the parameters the panel
  restores the next time that font is selected, in this session or a later run ("variant A" of the
  identity plan: presets keep their OWN per-font overrides in `fonts/presets.json`). It is
  written through `set_font_profile`, which uses a DEBOUNCED writer (`PROFILE_SAVE_DEBOUNCE`) and
  does NOT bump the revision: a profile is rewritten on every parameter edit, so an immediate
  atomic save per edit would be pure write amplification and a revision bump would force a font
  reload per keystroke. The accepted cost is a bounded loss window on a CRASH only — a normal
  exit flushes it: `app::on_exit` calls `font_admin::flush_pending_saves()`, which writes the
  pending snapshot synchronously (no GUI frame follows, and the detached debounce thread dies
  with the process, so closing the app inside the window used to lose the edit outright).
- Per-font records are found and mutated CASE-INSENSITIVELY (`find_record_key`), matching the
  identity contract. An exact map lookup let a differently-cased caller create a SECOND record
  for one font, of which only one was ever active.
  The panel side is `panel::FontProfileMemory` (the `font_profiles_by_identity` field): the
  session map answers first, a miss falls back to the persisted default and CACHES it, and a
  store writes the session map ALWAYS but the persisted default only when the caller passes
  `DefaultProfileWrite::UpdateFontDefault` — i.e. only while NO preset is applied (see the
  create-preset section: an edit made under an applied preset belongs to that preset). Applying
  a PRESET replaces the session map only — a preset must not rewrite what every font remembers
  on disk — and saving a preset captures the session map only, so a preset stays small.
  TEST COVERAGE NOTE: the two-layer rule is unit-tested through the injected
  `get_with`/`insert_with` hooks and the WHICH-LAYER rule through the per-thread journal
  `panel::take_persisted_default_writes` (`persist_profile` records instead of storing under
  `#[cfg(test)]`); the persistence itself is covered by `font_settings_store`'s own serialized
  tests, because the store is PROCESS-GLOBAL and a panel unit test reading it would see profiles
  written by any other test in the binary (same precedent as `persist_off_thread`).
- Display-name overrides are DISPLAY ONLY: `FontEntry.display_name` (populated by
  `fonts::apply_display_name_overrides`, which MUST run after `assign_font_identity_names` because
  the identity is the key) feeds `FontEntry::display_label()` used at presentation sites
  (`create_state::font_display_label`, and the settings font-settings rows). It never
  reaches persistence or the renderer.
- Virtual groups are injected into the panel font list by `fonts::apply_virtual_groups`,
  called at EVERY panel load site (`create_state::new` on the folder-only list, and the
  `spawn_font_reload` worker on the combined list) AFTER
  merge/disambiguation/identity-assignment. It matches each member by IDENTITY
  (case-insensitively, via `normalize_font_identity`), appends the membership into the font's
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

## Create-preset persistence (`presets_store.rs` + `create_presets.rs`)
- Create presets live in `fonts/presets.json` — **VERSION 1**, owned by `presets_store.rs`, and
  NOT in `user_config.json` any more (phase 5 of `dev-docs/font_identity_postscript_plan.md`).
  ```jsonc
  { "version": 1,
    "presets": { "ВВД": { "font": "d_CCShoutOut",
                          "profiles": { "d_CCShoutOut": { "schema": 2, … } } } } }
  ```
  `primary_font_key` + `primary_font_path` + `primary_font_label` collapsed into ONE `font` key
  (the identity). Unset fields and empty maps are omitted; preset names and profile keys are
  `BTreeMap`s so the file is byte-stable across saves.
- PRESET NAMES ARE USER DATA, stored VERBATIM. Nothing trims or folds them, so `" Рао-кун "`
  and `"Рао-кун"` are two presets; trimming collapsed them in the file's `BTreeMap` and one of
  them disappeared without a word. Only a completely empty name is dropped (it can address no
  combo row). Where a REAL clash is unavoidable — a MIGRATED preset whose name a saved preset
  already holds — both are kept, the migrated one under `"{name} (N)"`, with a warning.
- OWNERSHIP SPLIT (variant A), AND WHICH LAYER AN EDIT WRITES.
  `fonts_data.fonts.<identity>.profile` is the font's DEFAULT profile; `presets.<name>.profiles`
  are that preset's OWN overrides. The rule that keeps them apart lives at ONE place,
  `create_render_data::store_current_font_profile_by_idx`, which passes a
  `panel::DefaultProfileWrite` to `FontProfileMemory::insert`: **while a preset is applied
  (`selected_preset_name.is_some()`) an edit updates the SESSION map only** — that map is the
  preset's working set, and it reaches disk when the user saves the preset. **Without a preset
  the edit also updates the font's persisted default**, which is what makes parameters come
  back in a later session. Every store used to write both layers, so the first parameter edit
  after applying preset A silently made A's parameters the font's default and every fresh,
  preset-less panel opened with them. Deselecting the preset ("Без пресета") ends the preset
  context: the parameters on screen then belong to the font again, by the same rule.
- SAVING is atomic and CRASH-DURABLE through the shared `doc_store::write_atomic` with
  `Durability::ContentsAndDirectory`: the containing DIRECTORY is fsynced before `save` returns,
  because the caller deletes `TextTab.create_presets` from `user_config.json` right afterwards —
  without the directory flush a power loss in that window could leave the presets in NEITHER
  document. It reports a TYPED `PresetsStoreError`; the failure is logged AND pushed as a
  `PresetStoreEvent::SaveFailed` to the GUI thread, which shows it in the panel status line —
  the two `let _ = save_text_tab_create_presets(..)` calls this replaced dropped a lost preset
  without a word. A corrupt document reports `Invalid` and is quarantined to `presets.json.bad`,
  never silently read as empty — `rename`, else `copy`, else `QuarantineOutcome::Failed`, which
  DISABLES saving this document for the session (`PresetsStoreError::PersistenceDisabled`).
  Only on `Failed` is the corrupt file still the sole copy of the user's presets, and the
  atomic write ends in a `rename` over it; the same rule `fonts_data.json` follows.
- TWO RUNNING APP INSTANCES cannot clobber each other: the save is guarded by the SAME
  optimistic concurrency `fonts_data.json` uses (`doc_store::DocumentFingerprint` /
  `SaveBaseline`, one baseline per target path). A document from a NEWER schema is refused; a
  document that changed since this process read it is parsed, MERGED into the snapshot
  (additive — theirs is added, ours is kept) and the write is retried ONCE. What was merged in
  comes back as `PresetStoreEvent::MergedFromDisk` and is adopted by the panel, or its next
  snapshot would drop those presets again. A second conflict in a row is reported, not fought.
- SAVING CAPTURES THE SESSION MEMORY ONLY. `save_current_preset` no longer copies the CURRENT
  font's profile into every other loaded font's key — the fan-out that turned 67 real profiles
  into 162 stored ones (87 % of `user_config.json`) and made a preset claim parameters for
  fonts it was never configured for.
- NOTHING IS READ ON THE GUI THREAD. `create_presets::spawn_presets_seed` (called from
  `create_state::new`) starts a worker running `read_presets_seed`; the panel begins with NO
  presets and installs them from `PresetStoreEvent::Seeded` in the per-frame drain. Reading
  `presets.json` — and, behind a pending migration, the up-to-half-a-megabyte
  `user_config.json` — inside the constructor was file I/O on the GUI thread (CLAUDE.md §5).
  Until the seed lands the writer's baseline is `SaveBaseline::Absent`, so a save issued in that
  window cannot blindly overwrite the document it has not read: the conflict path merges the
  file in. A preset the user saved before the seed arrived wins over its stored namesake.
- ONE-SHOT MIGRATION, DEFERRED like `fonts_data`'s, AND GATED ON THE AUTHORITATIVE FONT
  LIST. When the document is missing (or was quarantined) the same worker also reads the
  legacy `user_config.TextTab.create_presets` payload, and `finish_legacy_presets_migration`
  completes the job on the GUI thread, where the font list exists. It may run ONLY once
  `TypingCreatePanelState.font_list_is_authoritative` — i.e. once the panel has installed a
  list built by the COMBINED loader; until then the payload is parked in
  `pending_legacy_presets_migration` and drained by `poll_font_reload_results`. The preset
  read and the font load are two independent background jobs and the reader easily wins
  (one small file against a whole font-directory scan), so "the fonts are usually there by
  now" is a race: a migration run too early resolves no IMPORTED system font, keeps those
  references verbatim, deletes the legacy `user_config` key and never retries, leaving
  `presets.json` and `fonts_data.json` naming the same font differently forever. A font
  reload that FAILS leaves the payload parked and logs it — the legacy keys stay, and the
  next launch retries the whole seed. What the migration then does: primary references collapse by NAME (a path-only match is refused, see the
  legacy-door rule), profile keys go through `font_profiles_keyed_by_identity`, and every
  profile body is upgraded by the ONE owner of that rule,
  `tab/codec::upgrade_text_params_to_v2`. Anything unresolvable is KEPT VERBATIM and logged. A
  migrated preset whose name is already taken is kept under `"{name} (N)"` rather than dropped.
- AFTER a successful write, `presets_store::drop_migrated_user_config_keys` deletes
  `TextTab.create_presets`, the dead `TextTab.use_system_fonts`, and — only once
  `fonts_data.json` DEMONSTRABLY CONTAINS the legacy list — `TextTab.imported_system_fonts`
  (which also ends the resurrection of a deleted imported font). The EXISTENCE of
  `fonts_data.json` is not that proof and must never be used as it: a valid but EMPTY document
  makes `font_settings_store::seed_from_fonts_dir` take its `Loaded` branch, so the legacy list
  is never consumed by `migrate_legacy_imported_fonts` — deleting the key then wipes the user's
  imported fonts from BOTH stores at once. The evidence is CONTENT: every legacy path must be
  the `last_path` of some `system_fonts` entry (`legacy_imports_are_taken_over`). The accepted
  false negative is a user who removed all imported fonts again: the key is then kept forever,
  which is harmless while `fonts_data.json` exists. `imported_system_fonts` was removed from
  `config::user_config_defaults()` for the same reason: a default would rewrite the key on every
  launch. Nothing is rewritten when no key is present, so a migrated config is never touched
  again. Measured on the real user config: 524 KB → 10 KB, with `fonts/presets.json` at 127 KB
  (its residue is the profile bodies whose fonts are no longer installed and therefore stay
  schema 1 verbatim).

## Built-in interface font (the bundled `fonts/ui` stack as a selectable font)
- The panel font list carries ONE synthetic entry (`FontEntry.kind =
  FontEntryKind::BundledUiStack`, built by `fonts::bundled_ui_font_entry`) that offers the
  bundled `fonts/ui` stack as a normal selectable font
  (`dev-docs/unicode_base_font_plan.md`, phase 5). It points at the FIRST `core` file of
  `ms_fonts::stack()` — a real file — so the own-typeface combo preview, the advanced-form
  width metric and PSD export need no special case; the REST of the stack follows for free
  because the renderer's `MsFallback::common_fallback` is that same core chain. There is no
  "font chain" type and must not be one: `FontContent` carries the bytes of exactly one file.
- IDENTITY: `fonts::BUNDLED_UI_FONT_IDENTITY` (`"ManhwaStudio-UI"`) is used as
  `identity_name` AND `label`, while `original_name` holds the PREVIOUS spelling
  `fonts::BUNDLED_UI_FONT_LEGACY_IDENTITY` (`"ManhwaStudio UI"`). Both are persisted-grade,
  non-localizable strings (`dev-docs/i18n_exclusions.md` §A7). `original_name` is
  deliberately NOT the core font's real family ("Noto Sans"): otherwise a build without this
  entry would silently resolve an overlay to a user's own Noto Sans instead of degrading to
  `missing_font`. Parking the legacy spelling there specifically is what keeps projects saved
  before the rename selecting the built-in font in the PANEL (which matches `original_name`
  as the family alias) and not only in the renderer, where `TabFontProvider` registers the
  legacy spelling explicitly. The legacy spelling is READ-ONLY: nothing writes it any more,
  and a stored document that still carries it is converted on load (`codec::
  upgrade_text_params_to_v2`).
- DISPLAY: `FontEntry::display_label` returns `t!("typing.fonts.bundled_ui_font_label")` for
  this entry only — the one entry whose shown name is localized while its stored name is not.
- COLLISIONS: the entry is inserted at index 0 by `fonts::prepend_bundled_ui_font` AFTER
  sorting and AFTER `assign_font_identity_names`. A user font claiming EITHER reserved
  spelling is given a `%hash`-suffixed identity by `assign_font_identity_names` and is warned
  about, so the reserved names cannot resolve to a user font even in a build where the
  bundled stack is unavailable and nothing would shadow it; on top of that the panel's
  ordered name lookup (`find_font_idx_by_name_forms`) and `TabFontProvider::from_fonts` are
  both FIRST-wins over the SAME list, so the entry also wins positionally in both places
  (never in only one). `assign_font_identity_names` skips the entry entirely, both as a
  recipient and as a claimant, so its presence cannot change any user font's identity. The
  entry claims no file-stem alias in the provider.
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
  settings font-administration list (`font_admin::load_font_lists`) does NOT get it —
  there is nothing to import, rename or group about it. It is also not the DEFAULT font of a
  fresh panel: the initial selection is made in `poll_font_reload_results` when the first
  list lands (the constructor has no list to choose from), and it picks the first of the
  user's OWN fonts, falling back to the built-in entry only when the user has none.

## Font file parsing (one pass per file)
- `fonts::read_font_file` is the ONLY place a font file is read for the panel lists: one
  `fs::read` + one `fontdb` parse produce `FontFileData` (faces, representative family
  name, coverage, content hash), with the bytes handed to `fontdb` through the same `Arc`
  the hash and the coverage classifier read. Loaders must go through it instead of
  re-parsing bytes; a test journal (`FONT_FILE_PARSE_JOURNAL`, `#[cfg(test)]`) pins the
  one-parse-per-file contract. The journal records, per read, HOW MANY `fontdb` databases
  that read built (a thread-local counter incremented where the database is created), not
  just that a read happened: counting calls alone would not notice a regression that
  re-introduces a second throwaway database inside one read, which is what phase 0 removed.
- Each `FontFaceEntry` carries its own `post_script_name` (`name` id 6) and `FontEntry`
  carries the representative face's, so the PostScript name is available STRUCTURALLY and
  nothing needs to split the decorated face `label` (`#i Family | Style | wNNN | PSName`)
  to recover it. The label stays a DISPLAY string. Empty PostScript name means "no usable
  face name": an unreadable/unparsable file, a name the spec forbids (below), or the
  synthetic bundled-stack entry, which stands for a chain of files rather than one face.
- NAME VALIDATION: a face's PostScript name is stored only when
  `fonts::is_valid_post_script_name` holds — after trimming, 1..=63 printable-ASCII
  characters with none of `[](){}<>/%`. An invalid name (interior space, non-ASCII,
  control character, delimiter, over-long) counts as ABSENT everywhere, so the identity
  falls back to family-then-label. The face `label` still shows the raw string, so the
  malformed font stays diagnosable, and `read_font_file` warns once per parse. The
  system-font PICKER catalog validates silently (it walks thousands of installed faces).
- DUPLICATE MERGE: `fonts::merge_duplicate_fonts` clusters raw files by
  `(valid PostScript name, content hash)`, so byte-identical copies of one font merge even
  when their FILE NAMES differ; the path-sorted first copy is the representative, the rest
  go to `alt_paths`, and their folder groups are unioned. A file that claims NO identity —
  no valid PostScript name, or no computed content hash (the `0` sentinel of an unreadable
  file) — NEVER merges: it is keyed by its position in the input. The historical stem key
  silently folded two different broken files that happened to share a stem
  (`groups/a/Broken.ttf` + `groups/b/broken.ttf`) into one entry, hiding one of them.
- COMBINED-LIST MERGE: the folder pass cannot see imported system fonts, so
  `fonts::load_fonts` runs `fonts::merge_duplicate_font_entries` over folder + imported
  entries with the SAME key. Without it a folder font and a byte-identical imported copy
  are two entries carrying ONE identity, and the provider (first-wins on the identity key)
  hides one of them while it still occupies a combo row. Rules: the first entry in list
  order wins (folder before imported), keeping its label, folder groups and disambiguator;
  the folded entry contributes only its paths (into `alt_paths`) and its display-name
  override when the representative has none. `groups` are NOT unioned — an imported file
  lives outside the fonts dir and its `groups = [None]` is a placeholder, not membership
  of the fonts-dir root. Must run BEFORE `assign_font_identity_names`.
  The `"{stem} [system]"` labels of the imported entries are then RENUMBERED over the
  survivors (`fonts::renumber_imported_system_font_labels`): their ` (N)` suffixes were
  handed out before the fold, so a folded copy could leave `"X [system] (2)"` with no
  unsuffixed sibling anywhere in the list. Display only — the label is not a key.
- DISPLAY-NAME OVERRIDES OF A MERGED ENTRY: overrides are keyed by IDENTITY, and a cluster of
  byte-identical copies carries exactly ONE, so the override is a single lookup and cannot depend
  on which copy became the representative. (The former per-path `first_display_name_override`
  scan was deleted with the path key in phase 4 of `dev-docs/font_identity_postscript_plan.md`.)
- CONTENT HASH: `fonts::font_content_hash` is the first 8 bytes of the file's SHA-256
  digest, big-endian. The algorithm is frozen: the hash is spelled into PERSISTED
  identities, and `std::collections::hash_map::DefaultHasher` (used before) carries no
  cross-release guarantee, so a toolchain upgrade would have re-suffixed every contested
  font. `0` is the "not computed" sentinel and disables merging for that entry.

## Font render IDENTITY (the representative face's PostScript name)
- The canonical name persisted in `render_data.text_params` / `TextRenderParams.font_name`
  and emitted in inline `<font=...>` tags is the font's identity
  (`FontEntry::render_identity_name()` = `identity_name`), computed for the FINALIZED panel
  list by `fonts::assign_font_identity_names`: the representative face's POSTSCRIPT NAME
  (`name` id 6), stored with its original casing and compared/keyed case-insensitively
  (`fonts::normalize_font_identity` — the one normalization, mirrored by the provider and
  the renderer). A file with no parsed face, or with a PostScript name the spec forbids,
  falls back to `base_font_identity_str`'s rule (family name, else file-stem `label`),
  which is the ONLY fallback and is documented as such. Call the pass wherever a panel list is finalized: `fonts::load_fonts` /
  `fonts::build_combined_font_list` (the AUTHORITATIVE list) and the folder-only subset
  `fonts::folder_font_entries`; non-panel lists (system-font picker) keep the per-entry
  `base_font_identity_name` default. Only the COMBINED list's identities are final — the
  folder-only ones are pre-collision, which is why nothing store-facing may be written
  from that pass (see the deferred-migration section). The file-stem
  `label` and any user display-name are for SHOWING the font (combos, lists) ONLY. Write
  sites — all of them write the identity and nothing else:
  `create_render_data::build_render_data_json_with_font` (the schema-2 `font` key),
  `create_apply::build_render_params_for` (`TextRenderParams.font_name`),
  `create_state::font_identity_name_by_idx` (inline `<font=...>` tags), `create_presets`
  (`presets.json`'s `font` + profile keys) and `font_settings_store` (`fonts_data.json`'s
  `fonts` keys, `virtual_groups[].members[].font`, `system_fonts[].font`).
- COLLISION POLICY. Two files claiming one PostScript name with IDENTICAL bytes are one
  font and merge (above). With DIFFERENT bytes they stay separate and EACH gets
  `"{ps_name}%{16 hex digits of its own content hash}"`
  (`fonts::suffixed_font_identity_name`, fed by `FontEntry.content_hash`). The suffix comes
  from the entry's own bytes, never from a list position, so it does not shift when another
  claimant appears or disappears — an ordinal suffix would renumber the survivors and
  invalidate everything persisted before. The WHOLE 64-bit hash is spelled out because
  contest detection compares whole hashes: a truncated suffix would hand two files that
  ARE recognized as different one identity. The bare name stays a resolution alias for the
  LOWEST-hash claimant, so documents written before the contest keep rendering. One warning
  per contested name, listing every file.
- THE SUFFIXED NAMESPACE IS DISJOINT FROM REAL NAMES, BY CONSTRUCTION. The separator
  `fonts::IDENTITY_HASH_SEPARATOR` (`%`) is a character the PostScript spec FORBIDS, and
  an invalid name counts as absent (above), so no font's real name can be a suffixed
  identity. The one remaining door is the family/label FALLBACK, which is unconstrained:
  a base identity that itself contains the separator is therefore suffixed
  UNCONDITIONALLY by `assign_font_identity_names` (alongside the reserved-name rule), so
  its identity carries one separator more than the form it would otherwise imitate.
  `/` would have served as the separator equally well; `%` was chosen because identities
  are shown next to file paths, where `/` reads as a path separator.
- Resolution accepts the identity AND legacy forms with the SAME precedence on both sides.
  `TabFontProvider` keys `identity_name` PRIMARY, then READ-ONLY aliases: the bundled legacy
  spelling, each font's own `%hash` form, the bare contested name (lowest content hash wins),
  then family-name / label / stem (first-wins; a display-name override is never a key).
  `create_state::find_font_idx_by_name_forms` runs the SAME seven ordered whole-list passes,
  so any name resolves to the same font in the panel combo and in the renderer — including
  the two forms the provider synthesizes from content hashes, which phase 1 left without a
  panel counterpart (a document naming a contested font used to render correctly while
  selecting nothing in the combo). A pinning test asserts panel/provider agreement per form.
  Legacy READ sites, all of them gated on the DOCUMENT'S schema (a schema-2 payload names the
  font once, under `font`, and no legacy key may override it):
  `codec::text_render_params_from_render_data` and `create_apply::
  apply_render_data_json_with_options` walk the schema-1 chain `font_original_name →
  font_label → font_family → font → file stem of font_path` — owned by
  `text_params_schema::legacy_font_name_candidates`, shared with `psd_export` — and
  `codec::normalize_text_params_object` carries all four legacy keys through VERBATIM, since
  only the conversion may remove them, and only once the identity is known.

## The panel speaks IDENTITIES
- **THE FONT LIST IS LOADED IN THE BACKGROUND, ALWAYS.** `create_state::new` touches the
  font directory for NOTHING: it starts with an EMPTY list (plus the synthetic bundled
  entry, whose bytes are `'static`), an empty group list and the "loading" status line.
  Scanning the directory and reading, hashing and parsing every font file is exactly the
  work CLAUDE.md §5 forbids on the GUI thread, and the constructor used to do it once PER
  PANEL — twice per session, with the folder-only result thrown away moments later by the
  reload that merges the imported system fonts in. `TypingTopPanelState::default` now starts
  ONE load for both panels (`create_state::spawn_shared_font_reload`, one worker, one
  `FontReloadResult` cloned under each panel's own token, so a later per-panel reload still
  supersedes it for that panel alone). Consequences that are contract, not accident:
  - the EMPTY list is a supported state everywhere (it already was: an empty fonts dir
    produces it), and `poll_font_reload_results` performs the INITIAL selection — the
    historical "first of the user's own fonts" — because `active_font_identity` is `None`
    until then. A `None` previous identity is "nothing to restore", NOT a missing font.
  - the panel's FIRST list only SEEDS the profile memory (`sync_current_font_profile_memory`),
    exactly as the constructor did. It deliberately does not take the "apply the font's
    persisted default profile" branch: opening a panel is not the user re-selecting a font.
  - under `#[cfg(test)]` `spawn_font_reload` / `spawn_shared_font_reload` arm nothing and
    spawn nothing, and the panel's fonts dir is INJECTED (`create_state::set_test_fonts_dir`,
    per-thread), defaulting to a path that does not exist. Dozens of panel unit tests build a
    panel; neither their results nor their runtime may depend on the font bundle sitting next
    to the developer's checkout. Tests that want real font files copy fixtures into a temp
    dir and inject it (`constructing_a_panel_reads_no_font_file` pins that the constructor
    reads none, through the parse journal).
- SELECTION KEY. `create_state::current_font_identity` / `font_identity_name_by_idx` return
  the font's identity; `TypingCreatePanelState.active_font_identity` stores it, and
  `find_font_idx_by_identity` (identity ONLY, case-insensitive) resolves it back. After a
  background reload (`poll_font_reload_results`) the selection is restored BY IDENTITY; when
  that identity is gone the panel enters `missing_font` instead of the old positional
  `min(idx, len - 1)` guess, which silently selected a different font and re-rendered with it.
- A FAILED RESTORE KEEPS LOOKING. `active_font_identity` is only re-anchored when the reload
  actually FOUND the identity; on a miss the SOUGHT identity stays, so the next reload — the
  one where the user has put the font back — restores that font and not the neighbour the
  clamped index landed on. For the same reason the per-font profile memory
  (`apply_render_data_json_with_options` / `sync_current_font_profile_memory`) is touched only
  on a successful restore: applying the neighbour's profile, or storing the missing font's
  parameters under the neighbour's identity, is exactly the substitution `missing_font`
  exists to prevent. A successful restore CLEARS `missing_font`.
- ONE LEGACY DOOR, AND A NAME ALWAYS BEATS A PATH.
  `create_state::match_font_by_legacy_reference(path, names)` is the ONLY routine a file path
  may enter (`ui_helpers::font_matches_path` is reachable from nowhere else). It tries every
  stored NAME first, in order, and reports WHICH evidence matched: `LegacyFontMatch::ByName`
  or `PathOnly`.
  - SELECTION accepts `ByName` only. `create_apply::select_font_by_legacy_reference` (a
    schema-1 blob, whose names come from `text_params_schema::legacy_font_name_candidates`, so
    panel and codec resolve identically) and `create_presets::apply_preset_by_name` (a leftover
    legacy value in `TypingCreatePreset.font`) turn a path-only match into `missing_font` plus a
    warning naming the font that now occupies that path. Until phase 5 a supplied path won
    OUTRIGHT: replacing a font file under its old name silently re-pointed every layer that
    remembered the path, and the next edit re-rendered it in the new typeface. This is the
    SELECTION half of the codec's safety rule D, which already refused to CONVERT such a layer.
  - RE-KEYING may accept `PathOnly`, ranked last: a legacy profile key WAS a
    `path.to_string_lossy()` and has no name to compete with, so refusing it would strand every
    stored profile, and the cost is bounded to remembered parameters.
  A preset's profile map is re-keyed to identities IN MEMORY on load; an unresolvable key is
  kept verbatim rather than dropped.
- A PRESET NAMING AN UNAVAILABLE FONT is NOT applied. `apply_preset_by_name` sets
  `missing_font` and leaves both the selection and every parameter alone, mirroring an
  overlay load; it used to re-anchor `active_font_identity` to the CURRENT font and apply the
  preset's parameters to it, which showed a preset "applied" to a font it was never saved
  for. A preset that names no font at all (an empty `font`) keeps the current selection and is
  not a missing font.
- PROFILE-KEY CONVERSION IS DETERMINISTIC. Several stored keys can name one font
  (`/old/fonts/Regular.ttf` and `Regular`); the winner is fixed by key FORM — exact identity,
  then identity-up-to-case, then a legacy NAME, then a legacy PATH — with a lexicographic
  tie-break, and the displaced profile is logged. The previous rule was `HashMap` iteration
  order, i.e. randomized per process. Every key that resolves is rewritten to the identity's
  own spelling (so a case-differing key stops hiding its profile from every lookup);
  a key that resolves to nothing is kept verbatim and cannot collide with a converted one.
- CACHES KEYED BY IDENTITY + CONTENT, never by path: the shared `widgets::font_preview`
  family name (`combo_font_family_name(identity, content_hash, face)`; the path is only the
  byte source for the one-time registration), `TypingTextOverlayLayer::editor_font_cache` (keyed by the
  content id the `FontProvider` reported for the bytes it actually served) +
  `TypingEditorFontSpec` (whose bytes now come from the panel's `FontProvider`, i.e. the
  renderer's own resolution), `char_table::CoverageJob`'s fingerprint, and
  `AdvancedFormMetricSignature` — whose `bundled_ui_stack: bool` field is GONE, because it
  existed only to separate two entries that share `core[0]`'s FILE (the bundled entry and a
  user import of it) and whose identities differ anyway.
  The CONTENT discriminant is not decoration: an uncontested font keeps its PostScript name
  when its file is replaced (only a contest adds the `%hash` suffix), egui never re-reads a
  registered font, and the provider's byte cache is per-provider — so an identity-only key
  meant the UI kept drawing a typeface the renderer no longer used. `content_hash == 0`
  means "content unknown" (bundled stack; a file unreadable at load time; an uncontested
  system-font picker row) and degenerates to the old key.
- THE PICKER CATALOG HASHES ONLY WHAT IT MUST. `fonts::resolve_contested_catalog_content_hashes`
  (inside `load_system_fonts`, off the GUI thread) resolves a REAL `content_hash` for exactly
  those identities that two or more of its files contest; uncontested rows keep the `0`
  sentinel, since their identity already names one row. Without it two installed files sharing
  a PostScript name shared ONE preview registration and the second row showed the first one's
  typeface. Hashing the whole catalog was measured and rejected: 2126 files / 543 MiB / ~3.4 s
  warm, against ~12 files / 4.6 MiB / ~19 ms for the contested names alone.
- THE PROVIDER'S BYTE CACHE EXPIRES, and is keyed by `FontByteSource` (`File` vs `Bundled` —
  one path can be both). A file replaced IN PLACE, with no font-list reload to rebuild the
  provider, used to render from the first resolve's bytes for the rest of the session. The
  cache now re-checks size + mtime at most once per `CACHE_REVALIDATION_INTERVAL` per font
  (one `fs::metadata`, outside the lock), never per resolve — `resolve` runs per text image and
  per inline `<font=…>` span on render threads. The stamp is taken BEFORE the read, or old
  bytes would pair with a new stamp and the change would never be noticed. Unreadable metadata
  keeps serving the last known good bytes and is reported once. Bundled entries hold `'static`
  bytes and are never revalidated.
- THE FORMS METRIC TAKES BYTES FROM THE PROVIDER, never from a path on the GUI thread:
  `create_advanced::poll_advanced_form_font` resolves in the background and caches the bytes in
  `AdvancedFormFont`; `AdvancedFormMetricSpec` is the snapshot the search worker builds the
  metric from, so nothing but tests ever measures on the GUI thread. THE FIRST SEARCH WAITS
  FOR THE BYTES (`schedule_advanced_form_search` returns while a resolve is in flight): their
  arrival changes `AdvancedFormMetricSignature::font_content_id`, i.e. the search key, so
  enumerating before they land means enumerating the same text twice — once per character and
  once per glyph. `CharWidthMetric` therefore remains the fallback for a font that resolves to
  NOTHING, not a transient first state.
- THE ON-CANVAS EDITOR FONT IS RESOLVED OFF-THREAD. `create_upload::request_editor_font`
  spawns the `FontProvider::resolve` (a cache miss is an `fs::read`, forbidden on the GUI
  thread) and `poll_editor_font_request` — called once per frame, BEFORE the "no editor
  open" early return — registers the bytes with egui, which must happen on the GUI thread.
  Until then the field draws in the default UI font; the poll requests a repaint while a
  request is in flight, since the worker schedules no frame of its own.
- DISPLAY IS UNCHANGED: the combo, the character table and the settings lists still show the
  display label (override → virtual-group alias → file stem). The one place the identity is
  now SHOWN is the settings font-properties window, next to the family name and the file.
- A FAILING resolve is distinguishable in the log from an unknown name: `TabFontProvider`
  reports an unreadable font file with its path and the OS reason (once per path — the
  renderer retries the resolve for every text image) and reports a poisoned cache mutex
  once, recovering the map rather than silently re-reading the file on every resolve. An
  unknown name stays a silent `None`; the renderer turns it into the missing-font error.
  Editing legacy text: `create_edit::normalize_desired_inline_tag_style` compares RESOLVED font
  identity (not raw strings) so a legacy `<font=stem>` on the base font is stripped, not duplicated.

## Persisted `text_params` schema (`text_params_schema.rs`)
- ONE OWNER of the persisted parameter format: `TEXT_PARAMS_SCHEMA_VERSION`, the FROZEN
  per-version default set, `write_text_params` (drops defaults / dead keys / legacy font keys
  and stamps `schema`) and `read_text_params` (fills the defaults of the schema the DOCUMENT
  declares). No other module may decide what an absent key means.
- WRITE (schema 2, `create_render_data::build_render_data_json_with_font`): the font is named
  ONCE, by IDENTITY, under `font`. `font_path` / `font_label` / `font_original_name` /
  `font_family` are never written again. Every value equal to its frozen default is omitted
  (`font`, `text` and `width_px` always survive), an empty `effects` array is omitted, and the
  dead keys `strict_shape_fit` / `aggressive_word_breaks` are dropped. ~1600 B → ~350 B on real
  data.
- READ (`create_apply::apply_render_data_json_with_options`): `read_text_params` FIRST, then the
  fields. Schema 2 selects the font by identity alone (`select_font_by_identity`); a schema-1
  blob keeps the legacy door (`select_font_by_legacy_reference`, order
  `font_original_name → font_label → font_family → font`, plus `font_path`).
- THE FROZEN DEFAULTS ARE A CONTRACT, not a mirror of the panel's current defaults. Changing a
  panel default is fine (that key simply starts being written); changing a FROZEN value
  reinterprets every already-written document and requires bumping the version and adding a read
  branch. `defaults_are_frozen` pins every value so this cannot happen by accident.
- SUB-PARAMETER `force_remove_ellipsis_glyph` (text-processing section) is a modifier of
  `replace_ellipsis_with_dots`, not an independent step. It is stored on its own (frozen
  default `false`, so no existing document changes meaning) but ANDed with its parent at every
  site that builds a `TextRenderParams` — `create_apply::build_render_params_for` and
  `tab/codec.rs` — so a stored `true` under a disabled parent can never reach the renderer. The
  panel draws it indented under its parent and only while the parent is on
  (`create_main_text::draw_text_processing_section`, mirrored in `create_edit.rs`), and it is
  deliberately EXCLUDED from that section's «включено N» summary, which counts independent
  processing steps.
- CONVERSION of schema-1 documents lives on the TAB side (`tab/codec.rs`), not here; the panel
  only supplies `create_render_data::resolve_legacy_font_identity` (the legacy door) and
  `font_post_script_names` (the `identity → PostScript name per face` snapshot the PSD export
  carries in its job instead of re-opening font files).

## Color presets (`color_presets_store.rs` + `ColorPresetsBinding`)
- The 20 preset cells offered under the palette by EVERY color picker of the «Текст» tab —
  the text color of both panels and every effect-card color — are ONE set per tab, owned by
  `TypingTopPanelState::color_presets`. It sits above `create_panel`/`edit_panel` on purpose:
  a per-panel set would let «Создание» and «Редактирование» drift apart and would read and
  write one title document twice.
- The set is TITLE-scoped: `{title_dir}/color_presets.json`
  (`ProjectPaths::color_presets_file`, `config::COLOR_PRESETS_FILE`), version 1, shape
  `{ "version": 1, "colors": [[r,g,b,a], …] }`. The bytes are PREMULTIPLIED sRGBA — the
  representation `Color32`/`ColorPresets::to_stored` round-trips losslessly — NOT the
  unmultiplied `[u8; 4]` of `settings.json`'s `bubble_status` rules.
- Persistence discipline mirrors `char_table/favorites.rs` and is documented there: typed
  `Missing`/`Loaded`/`NewerVersion`/`Invalid`/`Unreadable` load outcome, quarantine of a
  MALFORMED document to the first free `color_presets.json.bad*` name, temp+rename,
  everything through `crate::storage::storage()` (never `std::fs`), background load +
  per-frame `poll`, and the SHARED `char_table::SnapshotWriter` for writes. Exactly one
  document state — `Ready` — permits a write; `Loading`, `Quarantining`, `NewerVersion`,
  `Unreadable` and `QuarantineFailed` REFUSE the save and log why: replacing a document this
  process has not read (or has no right to rewrite) would overwrite the 19 cells it never saw
  with in-memory defaults.
- **A document of a NEWER version is read but NEVER written** — the same contract as the
  self-versioned `"PanelLayout"` section of `user_config.json` (`README_AGENT.md`). Its known
  cells are shown so the user still sees their colors, `ColorPresetsDocumentState::NewerVersion`
  refuses every save with a WARN naming the path, and the refusal is permanent for that
  document rather than transient. An OLDER (or absent) version is read best-effort and the
  next change rewrites it as the current version — that is the migration path.
- **No filesystem call may happen inside a frame** (CLAUDE.md §5). `save` only records
  intent: on `Invalid` it stores a quarantine request and switches to `Quarantining`; the
  free-`.bad`-name probe (up to 100 `exists`) and the `rename` run in the
  `typing-quarantine-color-presets` worker started by `poll`, and the pending change is
  handed to the writer only after the rename is CONFIRMED. Rebinding the store to another
  title drops a request that has not started yet, and a verdict that arrives for an unbound
  target is discarded. `favorites.rs` is NOT the reference here: its `toggle` still
  quarantines synchronously on the GUI thread. The `*.bad` naming race the two quarantines
  share is listed under "Contracts and invariants".
- **Durability is temp+rename only, and is documented as such.** A reader never sees a
  half-written document and a dying process leaves the previous set intact, but nothing is
  fsynced (`Storage` exposes no such primitive), so a power loss can still cost the last
  write — one preset edit, which the user redoes. The durable recipe of `doc_store.rs`
  (`write_all` + `sync_all` + rename + directory fsync) is deliberately not used: it is built
  on `std::fs` and would take this document off the wasm virtual store.
- A missing document is not an error and is NOT written: the set starts from the built-in
  palette (`PresetDefaults::Palette`), and the file appears only when the user confirms a
  cell.
- The path is pushed once per frame from `TypingTabState::draw` through
  `facade::set_color_presets_path`, next to `set_project_favorites_path`; the same call
  drives `poll`, which is the ONLY place the store starts or finishes disk work. A repeated
  identical path is a no-op, so it costs one read per TITLE change.
- The set reaches the drawing code as `ColorPresetsBinding` (`panel.rs`), a per-pass
  `Option<&mut ColorPresets>` PLUS the collected "a cell was overwritten" verdict. Every
  selector is drawn through `ColorPresetsBinding::draw_selector`, which is the single place
  that records the verdict — a bare `Option<&mut ColorPresets>` would force ~15 call sites to
  propagate a second return value each, and one forgotten site would silently lose a save.
  The two dock-tab bodies (`facade::draw_params_tab_body` / `draw_effects_tab_body`) create
  the binding and, after the pass, call `ColorPresetsStore::save` exactly once.
- **`None` is a legitimate mode, not a gap.** `effect_defaults::EffectDefaultsEditorState::ui`
  is drawn from the SETTINGS pane, where no title is open and there is therefore no document
  to read or write; it passes `ColorPresetsBinding::none()` and gets the stock egui color
  button, which is what `ViewportColorSelector::draw` has always shown.

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
  (`combo_font_family_name` / `is_font_family_bound` / `request_font_family`), shared by
  `create_presets::draw_font_combo_option` and the settings font UI. It is keyed by
  `(font identity, content hash, face index)` — see the CACHES bullet above for why the
  content discriminant is load-bearing; `FontEntry::render_identity_name` and
  `FontEntry::content_hash` are `pub(crate)` (via the `font_admin` re-export) precisely so the
  settings side can supply that key.
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
  `TypingTopPanelState::begin_frame` (`facade.rs`) stores the language the cache was
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
  inline tags use the renderer's `<b=...>` / `<i=...>` grammar. The geometry those
  parameters describe belongs to `crates/ms-text-render/src/MODULE_README.md` (faux
  bold/italic contract) — do not restate it here.
  - «Утолщение» (`faux_bold_thicken_percent`) is SIGNED: its range is the renderer's own
    `FAUX_THICKEN_PERCENT_MIN..=FAUX_THICKEN_PERCENT_MAX` (`-5..=25`), where a negative
    value THINS the glyphs. Both the `WheelSlider` range and the wheel step in
    `ui_helpers::draw_faux_style_controls` take those constants, and every app-side clamp
    (`create_apply`, `codec` decode + legacy normalizer, `inline_tags`) uses the same pair.
    Widening or narrowing one of them alone makes a value ping-pong between paths.
  - NEW overlays default `faux_bold_outward_only` to `false` (`create_state.rs`), the
    uniform-weight mode. The FROZEN schema-2 default in `text_params_schema.rs` stays
    `true` and must not follow it: it is what an already-saved document that omits the key
    decodes to, and flipping it would silently re-render every existing overlay. The
    per-field fallbacks in `create_apply`/`codec` are the same contract and stay `true`.
    The only consequence is that new overlays now write the key explicitly.
    ONE EXCEPTION to "saved documents are unaffected": an inline `<b=...>` tag lives in
    the TEXT and carries no key, so an omitted counter token means whatever
    `FauxBoldParams::default()` means TODAY — a hand-typed `<b=8>` / `<b=default>` saved
    before the flip now renders `both` instead of `out`. `inline_tags.rs` always emits the
    token explicitly, so only hand-typed tags are exposed. Documented, not shimmed: a tag
    has no version to key a shim off.
  - `inline_tags::parse_faux_bold_value` MIRRORS the renderer's `<b=...>` payload parser
    (`ms_text_render::inline_styles::parse_faux_bold_value`, a private module — the app
    cannot call it). It must build `FauxBoldParams` by spreading `..FauxBoldParams::default()`
    so an omitted token can never mean a different value here than in the renderer; pinned
    by `panel_faux_bold_mirror_matches_the_renderer_defaults`.
- The advanced-form width metric (`create_advanced::build_advanced_form_glyph_widths_from_spec`)
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
  `ScriptGroup::script_name_key` / `TextLanguage::name_key`, resolved through
  `i18n_resolve::resolve_key` — the crate is GUI-free and hands out catalog keys), so the
  wording
  is correct for any typesetting language, not hardcoded to Russian.
- No catch-all `match` arms over `TextLanguage`/`ScriptGroup` in `font_coverage.rs`.
- Accepted limitation of BOTH quarantines (`color_presets_store.rs`, `char_table/favorites.rs`):
  the free `*.bad` name is chosen with an `exists` probe and a separate `rename`, so two app
  instances quarantining one title's document in the same instant can pick one name twice and
  the second replaces the first copy — closing it needs a rename-without-replace primitive
  `Storage` does not expose, and the loss is one copy of an already-corrupt file.

## Advanced-form search: the knobs and the presentation order
(`advanced_form_params.rs` + `text_forms::order_advanced_forms`; spec:
`dev-docs/text_forms_ranking_plan.md` §2.3/§3b/§3c)
- **ONE OWNER OF THE EIGHT KNOBS.** `AdvancedFormParams` (`Copy`) holds `evenness`,
  `aspect_max`, `hyphen_ratio`, `hyphen_relax_slack`, `quality_floor`, `per_bucket`,
  `narrow_slots`, `filters_prune`, and the module exports every knob's `*_MIN` / `*_MAX` /
  `*_DEFAULT` next to its field doc. **The window's controls bind THOSE constants** — a
  control must never be able to offer a value the search refuses. `clamp_to_supported_range`
  is applied at every entry (setter, config decode) and once more inside `to_search_params`,
  so a hand-edited `user_config.json` cannot poison the search; `NaN` falls back to the
  field's default, since it has no side to clamp to.
- **RUNTIME VALUE**: a process-global `advanced_form_params()` / `set_advanced_form_params()`
  pair, mirroring `tabs::typing::rotation_ctrl_wheel`. Eight fields do not fit an atomic, so
  the store is `OnceLock<RwLock<AdvancedFormParams>>` (the `aspect_max` default is a division,
  not a `const` expression); read once per frame while the window is open, written only by the
  parameter section and the startup seed. A POISONED lock degrades to the compiled-in defaults
  and logs once — the form window may not abort the GUI thread.
- **PERSISTENCE** is `user_config.json` → `TextTab.advanced_form_search`, ONE JSON object
  (`config::TEXT_TAB_ADVANCED_FORM_SEARCH_KEY`), following the `rotation_ctrl_wheel_mode`
  recipe: seeded at startup by `main.rs::seed_advanced_form_search_from_config`, written by
  `tabs::settings::save_advanced_form_search_params` on a named thread. The
  GUI thread never reads the config file. The write goes through
  `config::update_user_config_file`, NOT the raw `fs::` read-modify-write its settings-tab
  siblings still use: that helper takes `config::lock_user_config_write()` itself (so the
  saver must not), REPORTS a malformed `user_config.json` instead of replacing it with an
  empty object — the sibling recipe silently destroys every unrelated setting when the file
  fails to parse — and goes through the `storage()` abstraction, so it is wasm-portable.
  Rewriting the siblings the same way is a separate decision. **The field NAMES belong to
  this module**
  (`to_config_value` / `from_config_value`) so the writer and the reader cannot drift, and a
  PARTIAL object is a supported input: every missing or invalid field keeps its compiled-in
  default. The key is deliberately absent from `config::user_config_defaults()` for the same
  reason — materializing it would add eight keys to every config for nothing.
- **`to_search_params(line_height_units, line_range, width_range)`** maps the knobs onto the
  engine's `forms::FormSearchParams` (layers A/B). `evenness` (`k`) rescales EVERY level of
  `forms::default_corridor_ladder()` by ONE law — all four bounds contract towards the ideal
  width `T_L` at `k < 1` and spread at `k > 1`: `interior_lo → 1 − (1 − lo)·k`,
  `interior_hi → 1 + (hi − 1)·k`, `head_lo → 1 − (1 − head_lo)·k`, `tail_lo` likewise — the
  measured mapping of plan §3b, reproduced BIT-EXACTLY at `k = 1.0` (`1.0 - (1.0 - x)` is not
  the identity in f32, so the default short-circuits instead of round-tripping). The
  multiplicative edge mapping (`head_lo·k`) was measured and REJECTED: it loosened the edge
  floors exactly when the user asked for more evenness, and the card count stopped responding
  monotonically. `hyphen_ratio` is `HyphenBudget::ratio_strict`; `hyphen_relax_slack` is its
  `slack_hi` end, raised to `slack_lo` when it would fall below it (the engine answers
  `slack_hi <= slack_lo` with "no relaxation at all"). The window's line/width ranges are
  passed as SEARCH ranges only while `filters_prune` is on — otherwise they stay a display
  filter. Everything else (quality weights, node budgets) comes from
  `FormSearchParams::default()` and is not user-visible.
- **`line_height_units` IS THE CALLER'S JOB, and the panel computes it in two halves.**
  `create_advanced::advanced_form_line_height_em` mirrors `ms_text_render::pipeline`
  (`pipeline.rs:432-437`) plus its `pub(crate)` `effective_spacing_percent`
  (`pipeline.rs:2628-2630`), reproduced here because the crate does not export it:
  `spacing% = clamp(line_spacing_percent + (glyph_height_percent − 100), ±300)`,
  `line_height_px = max(font_size_px + line_spacing_px + font_size_px·spacing%/100, 1)`,
  `em = line_height_px / font_size_px / (glyph_width_percent/100)`. The HORIZONTAL glyph
  scale must be in the divisor — widths are measured without it, so leaving it out silently
  detunes the aspect cap. The second half is the metric's own em scale, which only the WORKER
  knows because it depends on which metric it managed to build: `GLYPH_METRIC_UNITS_PER_EM`
  (1000, `GlyphWidths` measures in 1/1000 em) or `CHAR_METRIC_UNITS_PER_EM` (~2 characters
  per em, `CharWidthMetric`). The em figure is guaranteed finite and positive because it
  enters the search key, where a `NaN` would make the key unequal to itself and loop the
  restart forever. Pinned by
  `advanced_form_line_height_folds_spacing_and_glyph_width_into_metric_units`.
- **LAYER C — `text_forms::order_advanced_forms(forms, params)`** turns the engine's
  bucketed output into the card order and guarantees:
  1. **quality floor** — a form worse than `best + params.quality_floor_milli()` is dropped,
     which removes whole junk buckets rather than trimming good ones;
  2. **card #1 is the global `quality_milli` minimum**;
  3. **round-robin over line-count buckets, SPLIT INTO SUB-ROUNDS** — a bucket with `slots`
     cards per round places its card of rank `i` at `round = i / slots`, `sub = i % slots`,
     and the output is sorted by `(round, sub, quality_milli)`. Sub-round 0 of a round
     therefore holds EXACTLY ONE card per non-empty bucket, so no height repeats before
     every height has appeared;
  4. **narrow lean** — a bucket whose best form's `aspect_milli` is at or below the LOWER
     MEDIAN of all buckets' best aspects gets `params.narrow_slots` slots per round, every
     other bucket gets one. Relative to this text, never an absolute aspect threshold: for a
     large text every form is tall.
  **THE SUB-ROUND IS WHAT MAKES 3 AND 4 COMPATIBLE.** Emitted as one flat batch per round,
  a narrow bucket's two round-0 cards repeat its own height INSIDE round 0, i.e. guarantee 4
  breaks guarantee 3 at the shipped default `narrow_slots = 2` — the contradiction the
  ordering tests used to dodge by disabling the lean. With sub-rounds the second narrow card
  lands in sub-round 1, immediately after the complete one-card-per-height sub-round: the
  lean stays EARLY and nothing repeats too soon. Both are asserted at the DEFAULT
  `narrow_slots`.
  Forms carrying `forms::UNSCORED_QUALITY_MILLI` (a path that does not score, i.e.
  `forms::enumerate_forms`, which the window no longer uses) are ranked as a SEPARATE group
  appended at the END — mixed in, their "worst possible" score would either be dropped whole
  by the floor or would push an unscored form ahead of real cards in the next round.

## The advanced-form window: what it does per frame
(`create_advanced.rs`; window state lives in `panel.rs`)
- **THE SEARCH IS NOT ON THE GUI THREAD.** It used to be: `rebuild_advanced_form_cache_if_needed`
  ran `forms::enumerate_forms(..., usize::MAX, ..)` synchronously inside
  `draw_advanced_form_window`, on every frame the window was open, re-triggered by every
  keystroke — the plain violation of CLAUDE.md §5 the plan's work item B exists to remove.
- **THE INPUT IS A KEY, AND THE KEY IS THE INVALIDATION.** `AdvancedFormSearchKey` = an
  `AdvancedFormSearchBase` (prepared source text, preset, `AdvancedFormMetricSignature`, the
  five knobs that change WHICH forms exist — `filters_prune` is not one of them, see below —
  and the line height in em) plus the two range filters. A cache whose key
  differs from the current key is stale; nothing else invalidates it, which is why changing
  the preset or reopening the window no longer throws the previous result away.
- **Frame order, exactly:** `poll_advanced_form_font` → `poll_advanced_form_search` (accept a
  finished result) → `schedule_advanced_form_search` (reset ranges on a base change, debounce,
  spawn) → `reorder_advanced_form_cache_if_needed` → `poll_advanced_form_params_save` → draw
  the LAST known result. The window never draws an empty grid while it is recomputing; it
  draws the previous cards plus `typing.advanced.form_recomputing_status`.
- **DEBOUNCE `ADVANCED_FORM_SEARCH_DEBOUNCE` (200 ms).** The key changes on every keystroke
  and on every slider step, so without it a burst starts (and cancels) one search per frame.
  While the timer runs the window asks for `request_repaint_after`, since no input event will
  wake it otherwise.
- **CANCEL-ON-SUPERSEDE, WITHOUT A CALL SITE.** `AdvancedFormSearchJob` owns an
  `Arc<AtomicBool>` shared with the worker and sets it in `Drop`, so ASSIGNING the field
  cancels the previous job — the same shape as `tab::TypingShapeVariantPreviewState`. The
  worker checks the flag before starting and before sending. `forms::search_forms` itself
  takes no cancel token (it is bounded by node budgets instead), so cancellation stops the
  DELIVERY and the wasted metric build, not a running enumeration.
- **THE WORKER BUILDS THE METRIC TOO.** `AdvancedFormMetricSpec` is the whole snapshot
  (resolved bytes, face index, bundled-chain flag, the four bold/italic flags, hanging
  punctuation); `build_advanced_form_glyph_widths_from_spec` runs on the worker. Bytes are
  never read from disk there — they were resolved by `poll_advanced_form_font` — but the
  fontdb build and the alphabet shaping are still worker work.
- **RANGE FILTERS ARE SEARCH INPUTS while `filters_prune` is on**, which is what makes a
  deliberately narrow request cheap. Consequences that are contract:
  - the panel fields are `Option`: `None` means "not narrowed", and only a value strictly
    different from the bounds is written back, so a full range can never re-enter the search
    as a self-imposed constraint;
  - a cache built from a CONSTRAINED run CARRIES the previous bounds forward (unioned with
    what it observed) instead of adopting its own observations — otherwise the first
    narrowing would collapse the spin-box bounds onto itself and the user could never widen
    back;
  - a BASE change (text, font, preset, SEARCH knob) resets both ranges to `None` before the
    key is built. The comparison is against the base of the SHOWN cache, so the reset simply
    repeats every frame while a new set is being computed, which is the wanted state;
  - **`filters_prune` IS NOT PART OF THE BASE**, deliberately. It changes no form by itself —
    it only decides whether the ranges enter `AdvancedFormSearchKey` — so as a base field it
    made every toggle a "base change" that wiped BOTH ranges: a display-only range could not
    be promoted to a search constraint, and a search constraint could not be demoted back.
    The ranges now survive the toggle in both directions (`toggling_filters_prune_keeps_both_range_filters`),
    and the carry rule above is unaffected because it keys on the KEY's ranges, not on the knob.
- **THE QUALITY FLOOR AND THE NARROW LEAN NEVER RE-SEARCH.** They form `AdvancedFormOrderKey`;
  `reorder_advanced_form_cache_if_needed` re-runs `order_advanced_forms` over the retained
  `AdvancedFormCache::searched_forms`. The raw set is kept precisely because the floor DROPS
  forms and loosening it must bring them back without a new enumeration.
- **THE «Параметры поиска» SECTION LIVES IN THE WINDOW**, not in the settings pane: the knobs
  are meaningless without the result grid in view, and the window has no launcher counterpart,
  so the double-interface pattern of `egui-docs/04-widgets.md` §7 does not apply. It is a
  `collapsing_param_section`, closed by default, and every control binds the `*_MIN`/`*_MAX`
  constants of `advanced_form_params.rs`. An edit is applied to the process-global value
  IMMEDIATELY (the next frame's key change schedules the search) and written to
  `user_config.json` after `ADVANCED_FORM_PARAMS_SAVE_DEBOUNCE` (600 ms) on the named thread
  `typing-form-search-params-save`, which computes `config::user_config_path()` itself so the
  GUI thread touches no filesystem. Closing the window flushes a pending write; the accepted
  loss window is a crash (or an app exit that does not close the window) inside those 600 ms,
  with the value still in effect for the session.
- **THE WRITES ARE ORDER-SAFE, NOT ONLY SERIALIZED.** Each save is its own detached thread;
  `config::lock_user_config_write()` orders the WRITES but not the SPAWNS, so two saves could
  take the lock in the reverse order and leave the OLDER snapshot on disk — the knob would
  silently revert at the next launch. `create_advanced::AdvancedFormParamsSaveGate` stamps a
  monotonic generation at spawn time and admits only the newest one, checking it under the
  gate's own mutex so a stale writer cannot slip between a newer writer's check and its write
  (the config lock is taken INSIDE the gate, never the other way round). A superseded save
  abandons its write silently — it is not an error. A save whose THREAD failed to start
  RELEASES its generation, or a stillborn claim would declare itself newest forever and the
  save already in flight would write nothing at all.

## Editing map
- To change the create-preset FILE (a key, the version, the save guard), see
  `presets_store.rs`; to change the WRITE RECIPE or the concurrency vocabulary of EITHER panel
  document, see `doc_store.rs` — and only there; to change what a preset CAPTURES, how it is
  seeded off-thread or how a legacy one is converted, see `create_presets.rs`
  (`save_current_preset`, `read_presets_seed`, `migrate_legacy_presets`).
- To change WHICH profile layer a parameter edit writes (font default vs. applied preset), see
  `create_render_data::store_current_font_profile_by_idx` and `panel::DefaultProfileWrite`.
- To change the persisted `text_params` FORMAT (a new key, a default, the version), see
  `text_params_schema.rs` — and only there; the writer is
  `create_render_data::build_render_data_json_with_font`, the reader is
  `create_apply::apply_render_data_json_with_options`, and the legacy conversion is
  `tab/codec.rs::upgrade_text_params_to_v2`.
- To change an advanced-form search KNOB (its range, default, persisted name or how it maps
  onto the engine), see `advanced_form_params.rs` — and only there; to change its CONTROL, see
  `create_advanced::draw_advanced_form_search_params_section`; to change the CARD ORDER, see
  `text_forms::order_advanced_forms`; to change where the values are written, see
  `tabs::settings::save_advanced_form_search_params` and
  `main.rs::seed_advanced_form_search_from_config`.
- To change WHEN the form search re-runs, see `panel::AdvancedFormSearchBase` (the struct) /
  `create_advanced::advanced_form_search_base` (what fills it) /
  `schedule_advanced_form_search`; to change what it measures with, see
  `advanced_form_metric_spec` + `build_advanced_form_glyph_widths_from_spec`; to change the
  aspect-cap unit conversion, see `advanced_form_line_height_em`.
- To change where the color presets live, their schema or their failure handling, see
  `color_presets_store.rs`; to change WHICH pickers offer them, see `ColorPresetsBinding` in
  `panel.rs` and its construction in `facade.rs`; to change the cells or the popup itself, see
  `crate::widgets::color_preset_picker`.
- To change what a language requires, see `font_coverage.rs`
  (`script_chars_for_group`, `extra_chars_for_language`, the `*_EXTRA_CHARS` sets).
- To change when coverage is recomputed, see `facade.rs::begin_frame` (language-change
  detection) and `create_state.rs::spawn_font_reload`.
- To change the highlight colors / tooltip, see `create_presets.rs`
  (`draw_font_combo_option`, `font_coverage_tooltip`).
- To change the per-render fallback rows (wording, colors, truncation), see
  `create_presets.rs` (`font_fallback_status_lines`, `truncated_char_list`); to
  change where they are drawn, see `create_sections.rs::draw_preview_section`; to
  change WHAT the renderer reports, see `crates/ms-text-render/src/fallback_diag.rs`.
