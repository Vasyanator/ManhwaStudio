# Module: crates/ms-tab-cleaning/src/tools/watermark_removal

## Purpose
The standalone «Удаление водяных знаков» cleaning tool (`WatermarkRemovalTool`, re-exported by
`tools/mod.rs`). Three modes in one region-editor window: `MaskOnly` and `Clean` stream the
Python backend's `watermark.detect` / `watermark.remove`; `Chapter` («По главе (точное
вычитание)») runs the GUI-free engine in `crate::watermark_chapter` with no backend, plus the
on-disk mark library (`../watermark_library.rs`, `../watermark_entry.rs`,
`../watermark_library_window.rs`). The full contract list (GUI thread never blocks, blob length
checks, honest reporting, single-image sessions have no chapter mode) is the header of `mod.rs`;
the tool's place among the other tools is in `../MODULE_README.md`.

## Architecture
One directory module. `mod.rs` holds the header, the imports, the constants and EVERY type of
the tool — `WatermarkMode`, `WatermarkNetworkMode`, `WatermarkRemovalSettings`, the session
state, the chapter catalog / state / request / event / report types, `GrownCrop`,
`WatermarkEditorCtx` and `WatermarkRemovalTool` with its `Default` — so private fields stay
visible to every child. The children hold behaviour only:

- GUI thread: `cleaning_tool.rs` (the `CleaningTool` hooks) drives `tool_ops.rs` (inherent tool
  methods: settings load/save, status, library panel, selection diversion, job start / poll /
  apply) and `editor_ui.rs` (every `draw_*` through `WatermarkEditorCtx`).
- Workers: `backend.rs` (neural pass over IPC, settings file), `chapter_jobs.rs` (chapter
  sample / scan / apply / match / library load), `library_capture.rs` (reference crop capture and
  scratch crops). They are spawned through `ms_thread` and report over channels.
- Shared helpers: `chapter_state.rs` (state methods, page decode, geometry, previews),
  `settings.rs` (mode / settings / session-state methods), `describe.rs` (user-facing verdict
  wording).

## Files and submodules
- `mod.rs`: header and contracts, constants, all type declarations, child `mod`s, the
  `describe_conditioning` re-export.
- `settings.rs`: `impl WatermarkMode` / `WatermarkNetworkMode` / `WatermarkRemovalSettings` /
  `WatermarkSessionState`, `effective_watermark_mode`, `offered_watermark_modes`.
- `chapter_state.rs`: `impl ChapterMark` / `ChapterCatalog` / `ChapterState`, page decode,
  overlay <-> page rect mapping, crops, histogram bins, kind refit, previews, `next_library_arm`.
- `library_capture.rs`: `run_library_capture`, `grow_page_rect`, scratch-crop path / staleness /
  sweep.
- `chapter_jobs.rs`: the `run_chapter_*` workers, kind rebuild, library adopt / release / match,
  `build_library_request`, `chapter_source_ref`.
- `describe.rs`: verdict, conditioning, alpha-uncertainty, residual, quality and histogram
  wording.
- `tool_ops.rs`: `impl WatermarkRemovalTool`.
- `cleaning_tool.rs`: `impl CleaningTool for WatermarkRemovalTool`.
- `editor_ui.rs`: `draw_mode_entry` and `impl WatermarkEditorCtx`.
- `backend.rs`: `run_watermark`, request header, streamed call, blob split / decode, model
  unload, settings load / save.
- `tests.rs`: the tool's unit tests (scratch data in the system temp directory only).

## Contracts and invariants
- Visibility: a helper shared between children is `pub(super)` (visible in this module only).
  `describe_conditioning` is `pub(in crate::tools)` and re-exported by `mod.rs`, because
  `../watermark_library_window.rs`'s wording test checks it against `entry_verdict_line`; keep
  that path (`watermark_removal::describe_conditioning`) stable.
- No child does blocking work on the GUI thread; every IPC call, page decode, fit and library
  write runs in `backend.rs`, `chapter_jobs.rs` or `library_capture.rs` on a worker.
- Chapter mode owns no maths: everything numeric is `crate::watermark_chapter`.
- Apply goes through `CanvasView`; the tool never writes `CleanOverlaysModel` storage itself.

## Editing map
- Modes, persisted parameters, their clamping: `settings.rs` (+ the types in `mod.rs`).
- Neural run / IPC / blob format / settings file: `backend.rs`.
- Region-editor and chapter-panel UI: `editor_ui.rs`; the tab-facing hooks: `cleaning_tool.rs`.
- Library panel requests, selection diversion, chapter job dispatch and polling: `tool_ops.rs`.
- Chapter worker passes: `chapter_jobs.rs`; capture of reference crops: `library_capture.rs`.
- Wording of verdicts: `describe.rs` (wording is load-bearing, see `describe_conditioning`).
