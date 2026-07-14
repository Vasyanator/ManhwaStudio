# Module: src/page_ops

## Purpose
GUI-free engine for STRUCTURAL page operations on a loaded chapter: move a
page, insert pages (from image files or a generated blank), delete pages. An
operation is executed on disk as a journaled, crash-safe transaction that keeps
every page-keyed artifact consistent in BOTH trees — the committed chapter dir
and the sibling `{chapter}_unsaved` staging mirror (the save flow copy-merges
unsaved over committed without deleting, so a tree remapped on only one side
would resurrect stale files on save).

## Architecture
```text
execute_page_op(paths, pages, op)             recover_pending_page_op(project_dir)
        |                                                   |
   fs_exec::execute                                  fs_exec::recover
        |  scan_chapter  -> plan::ChapterSnapshot           |
        |  plan::build_plan (pure) <- json_remap (pure)     |
        |                                                   |
        v                                                   v
  journal A -> phase A + per-rename dir fsync -> journal B -> phase B -> slots deleted
  (write plan)          (reversible)            (commit pt)  (idempotent)
```
- `plan.rs` is pure: permutation math + a journal-serializable action plan
  built from a snapshot struct. `json_remap.rs` is pure: `Value`-level rewrites
  of the page-keyed JSON documents. All filesystem work lives in `fs_exec.rs`.
- Phase A stages created files and renames every affected file to a unique
  temp (`__ms_pageop_{id}_{n}.mstmp`) in its own directory — fully reversible.
- The journal uses two durable slots. Phase A is stored in
  `{chapter}/page_ops_journal.json`; after all staged files and every phase-A
  rename have been file/directory-fsynced, phase B is created separately as
  `page_ops_journal.b.json`. Only then is A removed. Thus Windows replacement
  never needs a remove-then-rename gap, and at least one complete plan exists
  throughout the commit transition. Recovery trusts B when both slots exist,
  rolls BACK A, and rolls FORWARD B. It validates all journal paths and
  conflicts before touching the filesystem, fails closed when a required
  transactional artifact is missing, and never re-reads an external insert
  source. A failed rollback retains A for the next recovery attempt.
- Rewritten JSON bodies are computed at plan time and stored IN the journal,
  so roll-forward never re-reads half-moved inputs.
- Deleted page artifacts are moved (never destroyed) into
  `{chapter}/.pageop_trash/{unix-millis}/` preserving their title-relative
  structure; removed JSON entries are archived next to them
  (`deleted_bubbles.json`, `deleted_text_info.json`,
  `deleted_layers_pages.json`). Trash folders are never garbage-collected by
  the engine — cleanup is a manual/user action.

## Files and submodules
- `mod.rs`: pinned public surface — `PageOpKind`, `PageOpOutcome`,
  `PageOpError`, `execute_page_op`, `recover_pending_page_op`.
- `plan.rs`: permutation math, canonical page-keyed file-name helpers (with
  citations to the owning modules), snapshot types, plan types, `build_plan`.
- `json_remap.rs`: bubbles / layers-manifest / text_info / detection-blocks
  rewrites over `serde_json::Value` (unknown fields survive).
- `fs_exec.rs`: chapter scanning, journal I/O, phase A/B execution, recovery,
  durability helpers, integration + crash-recovery tests.

## What is remapped (and what deliberately is not)
Remapped, in committed AND unsaved trees unless noted:
- `src/{stem}.{ext}` — renamed onto the canonical stems of the NEW order
  (`{idx:03}`, same format as `project::normalize_page_filenames`, so the next
  load's normalize pass is a no-op); extension preserved. Committed only
  (unsaved has no `src/`).
- `clean_layers/{stem}.png` — follows its page's stem.
- `layers/*.png` (`ps_p{page:04}_...`) — renamed to the new page prefix. The
  prefix is load-bearing: `layer_model/persist.rs::prune_orphan_pngs` prunes
  by it, so a stale prefix would let a save of the page now holding the old
  index delete another page's PNGs.
- `layers/layers.json` — `PageLayers.img_idx` remapped, `base_file` /
  `rendered_file` references rewritten by each NAME's embedded index, pages
  kept sorted by `img_idx`.
- `translation_bubbles.json` — `img_idx` remapped; bubbles of deleted pages
  removed (archived); page-crop `crop_page_idx` remapped, and when the crop
  TARGET page is deleted the `crop_page_idx`/`crop_rect` keys are removed so
  the bubble degrades to a plain image bubble instead of cropping a wrong page.
- `text_info.json` (legacy typing metadata; checked in `layers/` and
  `text_images/` of each tree) — `img_idx` remapped; deleted pages' entries
  removed (archived) and their referenced overlay PNGs (+ `*_layout.png`
  companions) moved to the trash.
- `text_images/mask_page_{idx}.png` — renamed by index.
- `text_detection/{idx:05}_blocks.json` + `{idx:05}_mask.png` — committed only
  (`text_detection/` has no unsaved mirror in `ProjectPaths`); the `mask_file`
  field inside a parsed blocks file is rewritten when it names the per-page
  default mask. An unparseable blocks file is renamed opaquely (a dangling
  custom `mask_file` degrades gracefully on load: missing file -> empty mask).

Deliberately NOT touched (each with the reason):
- `alt_vers/` — alternate-version images pair with pages by SORTED POSITION
  inside each `alt_vers/<name>/` subfolder (`cleaning/tools/stamp.rs::
  source_path_for_page` indexes the sorted list), and their file names are
  arbitrary source names. There is no reliable per-file page key to remap, and
  renaming files would not change positional pairing. After a structural
  operation the stamp tool's alt-version alignment may shift — a known,
  documented limitation instead of a silent wrong guess.
- typing overlay PNG names in `text_images/` (`typing_overlay_p{page:04}_...`)
  — the page token is a creation-time uniqueness hint only; loading goes
  through the JSON `file` reference and the stable overlay uid is derived FROM
  the file name (`text_payload::stable_overlay_uid`), so renaming would sever
  `layers.json` references. Page association lives in `img_idx`, which IS
  remapped.
- `image_bubbles/` — media files are keyed by bubble id, not page. Files of
  bubbles removed with a deleted page remain as orphans; the archived
  `deleted_bubbles.json` keeps their `image_path` for manual recovery.
- `cleaned/` and `saved/` — legacy folders migrated/consumed at load before
  any page op can run (`reconcile_clean_layers_dir`,
  `ensure_clean_layers_dir` bootstrap); remapping inert legacy data risks more
  than it fixes. Known edge: if `clean_layers/` is later emptied by hand, a
  bootstrap re-copy from a stale `cleaned/` would restore pre-operation order.

## Contracts and invariants
- Worker-thread only: synchronous disk I/O and fsync — never call from the GUI
  thread. Callers must quiesce all chapter writers (layer saver, bubble flush,
  overlay autosave) before `execute_page_op` and reload the project after.
- Operations are applied to BOTH trees immediately; they are not staged and are
  not undone by discarding unsaved changes.
- `recover_pending_page_op` must run at the very start of project load
  (`ProjectData::load_internal`), before any reconcile/normalize pass reads
  chapter files. A failed recovery aborts the load; the journal is left in
  place for inspection/retry.
- Legacy un-migrated documents are rejected, not guessed: bubbles or text_info
  entries in the absolute-ribbon-coordinate format (no `img_idx`, numeric
  `x`/`y`) are keyed by ribbon position — which any page op changes — so
  `execute_page_op` fails with `InvalidOp` until a normal load has migrated
  them.
- Uses `std::fs` directly, not the `crate::storage` seam: the transaction
  needs fsync and same-volume rename semantics the seam does not model. The
  feature is native-desktop; on wasm the journal never exists and recovery is
  an inert no-op. A web port of page ops requires extending the seam first.
- Windows: phase-B rename targets are guaranteed free (phase A vacated them),
  and A -> B uses distinct journal names, so no rename-over-existing is relied
  upon. Directory fsync is Unix-only best-effort (same policy as
  `tabs/settings`); each phase-A rename and staged create requests it before B.
- Same-volume assumption: temps live in their file's own directory and the
  trash lives inside the chapter dir, so every rename stays on one filesystem
  (`_unsaved` is a sibling of the chapter under the same title dir).

## Editing map
- New op kind or index-math change: `plan.rs` (`permutation_for_op`) +
  `mod.rs` (`PageOpKind`).
- New page-keyed artifact category: add it to `TreeSnapshot`/`ChapterSnapshot`
  + a `plan_*` function in `plan.rs`, scanning in `fs_exec::scan_tree`, and a
  rewrite in `json_remap.rs` if it is a JSON document.
- Journal format / crash-safety behavior: `fs_exec.rs` (bump
  `JOURNAL_SCHEMA_VERSION` on incompatible plan changes).
- Canonical file-name formats: they are OWNED by other modules (see the cited
  helpers in `plan.rs`); change them there first, then mirror here.
