# Module: crates/ms-project/src

## Purpose
Crate root of `ms-project`: the project/chapter domain model of ManhwaStudio and the
load-time passes that bring a chapter folder into a consistent, current shape before
any tab or worker reads it. Re-exported by the binary as `crate::project`
(`src/lib.rs`).

GUI-free: it produces data for the editor, never widgets. All of its work is blocking
disk I/O, so it runs on the background load thread (`src/shell/studio/open.rs`), never
on the GUI thread.

## Architecture
```text
ProjectData::load / load_resume_unsaved
        |
        |  1. canonicalize project_dir (native only)
        |  2. ms_page_ops::recover_pending_page_op   <-- MUST be first
        |  3. reconcile passes  (cleaned -> clean_layers, legacy overlay names,
        |                        magic-byte JPEG -> PNG, filename normalization)
        |  4. load pages / bubbles / settings / canvas settings
        v
   ProjectData { project_dir, image_dir, pages, bubbles: Arc<Vec<Bubble>>, paths,
                 comic_type: Option<ComicType>, canvas_settings, settings_data }
```

Layer position: `ms-config` <- `ms-page-ops` <- **`ms-project`** <- the binary.

`Page` and `ProjectPaths` are DECLARED in `ms-page-ops` and re-exported from `lib.rs`.
The two crates would otherwise be circular: the load path must resolve a pending
page-op journal, and the page-op engine operates on the page list and the path set.
Only the type declarations could move without weakening a crash-safety invariant, and
they fit the lower layer anyway — every `ProjectPaths` field is an `ms_config` name
joined onto a chapter or title directory.

## Files and submodules
- `project_scan.rs`: the catalogue scan of the projects ROOT (as opposed to the chapter LOAD
  the rest of the crate performs): `list_titles`, `list_chapters`,
  `validate_project_dir_for_startup` (`ProjectValidationState`), `find_unsaved_chapter`, and the
  chapter storage-format probe/conversion `chapter_storage_report` / `convert_chapter_storage`
  (both trees; document names from `ms_page_ops::chapter_docs`, never re-derived), plus the
  unsaved-session parse probe `damaged_unsaved_documents` (staging tree only; `Malformed`
  counts as damage) used by the launcher banner and by `load_resume_unsaved`, which refuses a
  damaged session with `damaged_unsaved_session_message` instead of loading it. Plain
  filesystem I/O, no UI and no app state. Read by the launcher and by startup. NOTE: unlike the
  load path it uses `std::fs` DIRECTLY, not the `ms_storage` seam — it browses a real projects
  root on a native desktop, which has no web analogue.
- `save_merge.rs`: the "save to project" merge (`merge_unsaved_into_project`): byte-copies the
  `{chapter}_unsaved/` tree over the committed chapter EXCEPT the owned documents (skipped by
  stem: `.json`, `.db` and a `.db`'s `-journal`, which would be replayed into the committed
  database) and docstore temps, copies the staged bubbles (either format) through
  `ms_docstore::copy_document` INTO the committed document's format (a missing committed
  document takes the chapter's format, `ms_page_ops::chapter_docs`), so a committed chapter
  never ends with both `X.json` and `X.db`; runs the
  caller-injected per-page layer merge (`ms-models` sits above this crate, so the binary passes
  `persist::merge_unsaved_layers_into_committed` as a closure), then removes the staging dir.
  Native `std::fs`, blocking, worker thread only; errors are localized `app.merge.*` texts.
- `single_image.rs` (native only, tests in `single_image_tests.rs`): the storage side of the
  image-set (single-image) mode — one or several picture files opened, in the order received, as
  the pages of one hidden scratch chapter. `SingleImageScratch` reserves a session root under a
  base dir (`ms_config::single_image::scratch_base()` in production) as create_dir -> lock
  `.lock` -> write `.ms-single-image` marker; `sweep_stale_sessions` deletes only MARKED roots
  whose lock it can acquire. `estimate_image_set` (header-only RGBA8 size) +
  `needs_size_confirmation` (thresholds from `ms_config::single_image`) decide whether a large
  set asks first. `open_image_set` decodes each file in order (format from content, TGA by
  extension; EXIF orientation baked in; ICC kept; animated sources open their first frame) into
  `<root>/title/chapter/src/{i:03}.png`, records its `SourceImageFacts` with the `PixelDigest`
  of the written RGBA8, skips (and lists as `SkippedSource`) files that cannot be opened, seeds
  `comic_type = pages`, runs the regular `ProjectData::load` and sets the session. Uses
  `std::fs` directly like `project_scan.rs`. Errors are `SingleImageError` (`user_message()`
  localized `project.single_image.*`, `Display` technical).
- `pixel_digest.rs` (every target): `PixelDigest`, SHA-256 over `width_le ‖ height_le ‖ RGBA8`
  — the ONE "same pixels" test of the image-set mode (open records it per page, the typing
  flatten recomputes it so Save skips an unchanged page).
- `storage_mode.rs`: the Dev/Prod storage-mode conversion driver (`convert_globals`):
  (a) persists `General.storage_mode` in user_config's current format and switches the
  docstore default, (b) converts `fonts_data`/`presets`, (c) the five title-level documents of
  every `list_titles` title, (d) user_config LAST and only after a clean (b)+(c) — user_config
  is the startup sentinel. Idempotent, per-document failures collected; chapters are never
  touched. Blocking; the only caller is `ms-settings-ui`'s process-wide job (worker thread).
- `canvas_layout.rs`: the canvas LAYOUT presets — `CanvasLayout` (the 13 preset-owned fields of
  `CanvasSettings` in persisted vocabulary, `JSON_KEYS`, `of` / `apply_to` / `normalized`),
  `BuiltinLayoutPreset` (`Pages` / `Webtoon`: `factory()` is THE table, persistence `id`,
  `for_kind` / `fallback_for_kind`, `display_name`) and `BASE_BUILTIN`; the JSON codec
  (`from_json_object` / `write_json_object`), the per-title read `read_title_layout` with the
  legacy migration rule, `LayoutPresetSelection`, and two `settings.json` writers
  (`save_layout_preset_selection`, `seed_image_set_title`); the app-wide preset store
  `CanvasLayoutPresets` (`user_config` `Canvas.layout_presets`: built-in overrides + user presets,
  lenient decode, `effective_builtin` / `fallback_for_kind` / `effective`), its operations
  `LayoutPresetOp` and their writer (`apply_layout_preset_op_to_user_config` pure,
  `apply_layout_preset_op` one `ms_config::update_user_config_file` RMW). Re-exported from `lib.rs`.
- `canvas_settings_tests.rs`: tests of the canvas-settings reader (`canvas_settings_from_config`):
  characterization of the legacy per-field document precedence, root fallbacks and clamps, plus
  the per-title (marker) read and the kind-dependent migration parts.
- `lib.rs`: the rest of the crate. `Bubble`, `CanvasSettings`, `ComicType`, `Side`,
  `ProjectData` and its load/reconcile/normalize passes, `LegacyRibbonGeometry`
  (migration of the very old absolute-coordinate Tkinter ribbon bubbles), the
  JPEG->PNG magic-byte conversion, and the filesystem helpers the load uses.

## Contracts and invariants
- **Session kind is explicit.** `ProjectData::session` is `SessionKind::Project` after every
  load; only `single_image::open_image_set` sets `SingleImage`. Never infer it from paths.
- **Image-set page mapping.** A `SingleImageSession` has at least one source (its constructor
  refuses an empty list); `sources()[i]` backs scratch page `i` (`Page::idx == i`, file
  `{i:03}.png`), in the order the files were given, and `open_image_set` verifies the loaded
  page list against it. Structural page operations would break this mapping, which is why the
  mode hides the Page Manager. A file failing to READ or DECODE is skipped (listed in
  `skipped()`); a scratch WRITE failure aborts the whole set; nothing opened is
  `NothingOpened`, whose text for a single requested file is that file's own reason.
- **Single-image mode never writes the user's file or folder** here: everything goes into the
  scratch root; the source is only read. `SingleImageScratch`'s `Drop` does no I/O (a crash or
  plain drop leaves the root for the next sweep); `remove()` is a blocking recursive delete,
  never on the GUI thread. A sweep never touches an entry without the marker, and drops its
  probe lock handle before deleting (Windows). Both lock sites go through the one helper
  `try_lock_exclusive` (`File::try_lock`; a direct `flock` on Android, whose stable std
  returns `Unsupported`).
- **Journal first.** `load_internal` calls `ms_page_ops::recover_pending_page_op`
  before ANY reconcile or normalize pass. Until the journal is resolved the page-op
  transaction owns the page keying of every artifact, so a reconcile pass running
  first can mis-pair half-renamed pages with their overlays and bubbles. A failed
  recovery ABORTS the load; it never proceeds on a best-effort basis.
- **Storage seam.** Every read, write, rename, directory listing and existence check of the LOAD path
  goes through `ms_storage::global::storage()`, not `std::fs`, so the same pipeline
  serves the native filesystem and the in-memory web store. Images are decoded from
  in-memory bytes and encoded to a buffer before being handed to the seam; there is
  no `copy`, so a copy is read + write. The one native-specific call is
  `canonicalize()` on the incoming project dir, which has no seam analog and runs
  before the virtual layer applies.
- **Never on the GUI thread.** Every entry point here performs blocking I/O, and the
  JPEG conversion additionally fans out over the global rayon pool.
- **One layout table, stored per title.** The 13 preset-owned layout fields (`CanvasLayout`), the
  factory values of the built-in presets «Pages» / «Webtoon» (`BuiltinLayoutPreset::factory`), the
  layout's JSON codec, the per-title read with its migration rule (`read_title_layout`) and the
  preset selection (`LayoutPresetSelection`, `canvas.layout_preset`) are owned by
  `canvas_layout.rs`. The three canvas defaults (`CanvasSettings::default` here,
  `SharedCanvasSettings::default` in `ms-models`, `CanvasState::default` in `ms-canvas`) take their
  layout fields from `BASE_BUILTIN` and spell only the non-layout fields themselves; the
  `ms_config` default trees carry NO layout key (a backfill would write stale values into every
  title), guarded by `defaults_trees_carry_no_layout_key`. The agreement test
  (`canvas_defaults_agree_across_the_three_mirrors`) lives in `crates/ms-canvas/src/types.rs`, the
  only crate that sees all three.
- **Built-in overrides.** The user may edit «Pages» / «Webtoon»; the store keeps an override only
  while it differs from the factory row (saving the factory values or `ResetBuiltin` removes it).
  Every APPLY of a built-in — the reader's fallback for what a title does not store (so new titles
  and image sets follow an edited preset), the first-open prompt, the Settings picker — goes
  through `CanvasLayoutPresets::effective_builtin`; the factory row is used only by a revert and
  by the legacy compact-mode recovery of `read_title_layout` (exact old-build derivation). The
  store writer edits only the touched entries (malformed and unknown entries survive) and refuses
  every write to a store whose `version` it does not know (a newer build's), leaving it untouched.
- **Per-title layout and its migration.** A title whose `canvas.layout_per_title` is `true` reads
  its whole layout from its own `canvas` object (missing keys: its kind's preset, override-aware). A title without
  the marker was last saved by an older build that kept six fields in `user_config` `Canvas.*`: it
  reads what that build showed, except the compaction of a `pages` / `ribbon` title, which comes
  from its kind's factory preset (see `read_title_layout`). The project canvas writer
  (`ms-canvas`) sets the marker on every save, so the migration happens once, on the first draw.
  An older build re-saving a migrated title drops three of the keys again; the new build then
  fills them from the kind's preset.
- **`comic_type` is the KIND, never derived.** It is written only by the first-open prompt, the
  Settings «Тип тайтла» row (both through `save_comic_type_to_project_file`) and the image-set
  seed (`seed_image_set_title`, which also sets the layout marker, so an image set takes the
  «Pages» values). `Custom` is a legacy value: readable, never written.
- **Owned documents.** `settings.json` (`DocKind::ProjectSettings`) and the bubbles document
  (`DocKind::Bubbles`) are read/written through `ms_docstore`, never the seam directly:
  `load_bubbles` reads, `persist_migrated_bubbles` writes (its `*_legacy_xy.json` backup is a
  plain seam file, not an owned document: a byte copy of a `.json` source, the pretty JSON of
  a `.db` source's value; backup + rewrite run in one `ms_docstore::with_lock` section), `save_comic_type_to_project_file` is ONE
  `ms_docstore::update` that edits only `comic_type` and returns an error — leaving the file
  untouched — when `settings.json` is malformed; `save_layout_preset_selection` and
  `seed_image_set_title` follow the same rule.
- Legacy formats are read FOREVER. A reconcile or migration pass may only act on an
  unambiguous match, and `persist_migrated_bubbles` backs the original up to
  `*_legacy_xy.json` before rewriting.
- The clean-overlay reconcile / legacy / normalize passes name their targets with
  `ms_page_ops::clean_binding::clean_overlay_file_name` and rename a source only when
  `classify_clean_fit` says `Matches` (`clean_dimensions_match_page`); which files are
  CANDIDATES for renaming (case-insensitive `.png`, legacy numbering, fuzzy keys) is migration
  policy owned here, the binding rule is not.

## Editing map
- To change the scratch layout, accepted input formats, the per-file decode or the skip/fatal
  split of an image-set open, see `single_image.rs` (the readable-types table and the
  confirmation thresholds live in `ms_config::single_image`). To change what counts as "the
  same pixels", see `pixel_digest.rs` (the typing flatten uses the same type).
- To change the chapter file layout, edit `ProjectPaths` in
  `crates/ms-page-ops/src/lib.rs` and the constants in `ms-config` — not here.
- To change what a load reconciles or normalizes, see `reconcile_clean_layers_dir`,
  `reconcile_legacy_cleaned_names`, `normalize_page_filenames`,
  `overlays_already_canonical` and `convert_jpegs_to_png` in `lib.rs`.
- To change the bubble document shape or its legacy migration, see `Bubble`,
  `load_bubbles` and `LegacyRibbonGeometry`.
- To change which global/title documents a storage-mode switch converts, or its order, see
  `storage_mode.rs` (keep user_config last: it is the sentinel `ms-config`'s startup probe reads).
- To change a layout preset's values, the preset-owned field set, the per-title migration rule,
  the selection's JSON shape or the preset store's format and override rules, edit
  `canvas_layout.rs` only (a new layout field is a compile
  error at every mapping that destructures `CanvasLayout`); the reader that applies it is
  `canvas_settings_from_config` in `lib.rs`;
  non-layout canvas defaults are `CanvasSettings::default` here AND the two mirrors named above —
  the test will fail if they drift.
