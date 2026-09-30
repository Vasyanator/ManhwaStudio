# Module: crates/ms-project/src

## Purpose
Crate root of `ms-project`: the project/chapter domain model of ManhwaStudio and the
load-time passes that bring a chapter folder into a consistent, current shape before
any tab or worker reads it. Re-exported by the binary as `crate::project`
(`src/main.rs`), so every `crate::project::…` call site keeps working unchanged.

GUI-free: it produces data for the editor, never widgets. All of its work is blocking
disk I/O, so it runs on the background load thread (`src/studio_bootstrap.rs`), never
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
   ProjectData { pages, bubbles, paths, comic_type, canvas_settings, settings_data }
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
- `storage_mode.rs`: the Dev/Prod storage-mode conversion driver (`convert_globals`):
  (a) persists `General.storage_mode` in user_config's current format and switches the
  docstore default, (b) converts `fonts_data`/`presets`, (c) the five title-level documents of
  every `list_titles` title, (d) user_config LAST and only after a clean (b)+(c) — user_config
  is the startup sentinel. Idempotent, per-document failures collected; chapters are never
  touched. Blocking; the only caller is `ms-settings-ui`'s process-wide job (worker thread).
- `lib.rs`: the rest of the crate. `Bubble`, `CanvasSettings`, `ComicType`, `Side`,
  `ProjectData` and its load/reconcile/normalize passes, `LegacyRibbonGeometry`
  (migration of the very old absolute-coordinate Tkinter ribbon bubbles), the
  JPEG->PNG magic-byte conversion, and the filesystem helpers the load uses.

## Contracts and invariants
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
- **Canvas defaults are mirrored in three places** (`CanvasSettings::default` here,
  `SharedCanvasSettings::default` and `CanvasState::default` in the binary) plus the
  JSON copies in `ms_config`. The test that guards their agreement lives in
  `crates/ms-models/src/bubbles_model.rs` because two of the mirrors are binary-only types.
- **Owned documents.** `settings.json` (`DocKind::ProjectSettings`) and the bubbles document
  (`DocKind::Bubbles`) are read/written through `ms_docstore`, never the seam directly:
  `load_bubbles` reads, `persist_migrated_bubbles` writes (its `*_legacy_xy.json` backup is a
  plain seam file, not an owned document: a byte copy of a `.json` source, the pretty JSON of
  a `.db` source's value; backup + rewrite run in one `ms_docstore::with_lock` section), `save_comic_type_to_project_file` is ONE
  `ms_docstore::update` that edits only `comic_type` and returns an error — leaving the file
  untouched — when `settings.json` is malformed.
- Legacy formats are read FOREVER. A reconcile or migration pass may only act on an
  unambiguous match, and `persist_migrated_bubbles` backs the original up to
  `*_legacy_xy.json` before rewriting.
- The clean-overlay reconcile / legacy / normalize passes name their targets with
  `ms_page_ops::clean_binding::clean_overlay_file_name` and rename a source only when
  `classify_clean_fit` says `Matches` (`clean_dimensions_match_page`); which files are
  CANDIDATES for renaming (case-insensitive `.png`, legacy numbering, fuzzy keys) is migration
  policy owned here, the binding rule is not.

## Editing map
- To change the chapter file layout, edit `ProjectPaths` in
  `crates/ms-page-ops/src/lib.rs` and the constants in `ms-config` — not here.
- To change what a load reconciles or normalizes, see `reconcile_clean_layers_dir`,
  `reconcile_legacy_cleaned_names`, `normalize_page_filenames`,
  `overlays_already_canonical` and `convert_jpegs_to_png` in `lib.rs`.
- To change the bubble document shape or its legacy migration, see `Bubble`,
  `load_bubbles` and `LegacyRibbonGeometry`.
- To change which global/title documents a storage-mode switch converts, or its order, see
  `storage_mode.rs` (keep user_config last: it is the sentinel `ms-config`'s startup probe reads).
- To change canvas presets or defaults, see `CanvasSettings` / `ComicType` here AND
  the two binary mirrors named above — the test will fail if they drift.
