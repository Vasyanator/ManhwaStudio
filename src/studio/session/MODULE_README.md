# Module: src/studio/session

## Purpose
The studio session strategy: what `StudioApp` does differently for a project chapter and for an
image set (one image or an ordered set of images opened without a project). The kind is decided
ONCE, in `StudioApp::new`, from the opened `ProjectData` (`SessionMode::from_project`); after that
every per-kind behaviour is a dispatch call on `SessionMode`, and no other code in `src/studio` asks
which kind is open.

## Architecture
```text
StudioApp.session: SessionMode
  Project(ProjectSession)          project/    save-to-project, page ops, DISCARD, unsaved cache
  ImageSet(Box<ImageSetSession>)   image_set/  «Сохранить» / «Сохранить как», its dialogs and rules
StudioApp::session_parts() -> (&mut SessionMode, SessionParts)
  SessionParts = project + StagingWriters + CloseFlow + typing / PS editor / translation canvas /
                 cleaning / page manager tab states, borrowed for one call;
                 a successful merge only LATCHES "committed tree changed"
                 (take_committed_tree_changed): the root tells the tabs, the flow never does;
                 flush_canvas_bubble_edits() = the all-canvas bubble flush of every persistence point
```
Where the two layers of the session come from (`ARCHITECTURE.md` §4): the open worker decides the kind once; the shell keeps its half (`StudioSession`: scratch
lifetime, reload policy, `src/shell/studio/session.rs`); `ProjectData.session` (`ms-project`)
carries the data facts; this module is the app half, built from those facts.

Dispatch methods on `SessionMode` and their `StudioApp` call sites:
- `has_unsaved_changes`, `tab_visible`, `input_declarations` (`new`, `build_input_registry`),
  `capabilities` (`new`: the ONE push of `ms_models::SessionCapabilities` into the translation,
  cleaning, typing and Settings tabs, once per `StudioApp` — the kind never changes; tab crates
  read those named features and never ask `ProjectData::is_single_image`);
- `tick` (once per frame in `ui`, before the unsaved-cache-dependent steps), `draw_top_bar`
  (right of «to launcher»), `draw_exit_dialog`, `draw_dialogs` (right after the received-images
  confirmation), `run_fired_commands` (right after `InputRouter::begin_frame`), `start_page_op` (Page Manager action), `before_shutdown`
  (first step of `shutdown_blocking`);
- project-state queries that answer "nothing" for an image set: `busy_jobs` (`studio/busy.rs`),
  `exit_cleanup_action` (`studio/incoming_open.rs`), `take_reload_request` (`take_outputs`),
  `take_committed_tree_changed` (`ui`, right after `tick`: a merge succeeded, so every tab gets
  `StudioTab::on_committed_tree_changed` before any tab draws), `note_unsaved` (bubbles revision); `os_close_policy` (`request_os_close`); `take_title`
  (`take_outputs`; `None` for a project).

## Files and submodules
- `mod.rs`: `SessionMode`, `SessionParts`, `DeferredTabEdits` (the one owner of "the tabs hold
  edits not yet handed to a staging writer", read by both kinds' dirty checks; a new tab-held
  deferred edit becomes a field here), the dispatch methods, and the image-set close helpers
  (`close_image_set_discarding`, `apply_image_set_outcome`); tests that build both kinds from real
  fixtures (a temp chapter, an opened image set) and check their dispatch answers.
- `project/`: `ProjectSession`, the project persistence state and flows. See its `MODULE_README.md`.
- `image_set/`: `ImageSetSession` (the save controller), its pure save machine, dialogs and rules
  (`tab_visible`, `image_window_title`, `input_declarations`). See its `MODULE_README.md`.

## Contracts and invariants
- One decision: `SessionMode::from_project` is the only constructor; the kind is read from
  `project.session()` by `ImageSetSession::from_project` (always `None` on wasm). Never infer the
  kind from paths or from other state.
- A session flow reaches the app ONLY through `SessionParts` (never `StudioApp` fields) and keeps
  every ordered sequence (save, page op, discard, failed-discard restart, save-then-close) inside
  ONE method in its fixed order ("Save / page-op / discard / exit choreography",
  `src/studio/MODULE_README.md`).
- `tick` runs every frame whatever tab is active (the image set's is the one `poll_flatten_to_file`
  caller the typing tab requires; the project's drives a save deferred behind the layer preload).
- An image set never merges or deletes staging: every close is `StagingWriters::quiesce_discarding`
  (dialog «Не сохранять», a finished save-then-close, `before_shutdown`), never a flush.
- Project-only state does not exist in an image set: the busy jobs are all clear, no reload is
  requested, no page op can start (Page Manager hidden; `start_page_op` refuses with a log).
- A second OS close while the exit dialog is open follows `os_close_policy`: a project exits
  without asking again (its staging survives); an image set keeps the dialog and logs, because
  every image-set close discards the scratch and would lose the unsaved edits.
- The only item of this module used outside `src/studio` is `image_set::image_window_title`
  (`src/shell/target.rs`).

## Editing map
- A new per-kind behaviour: add a dispatch method on `SessionMode` (both arms), call it from the
  one `StudioApp` site; never add a `match` on the kind elsewhere.
- Project save / page op / discard / exit dialog: `project/` (`save.rs`, `page_op.rs`,
  `discard.rs`, `exit_dialog.rs`); per-frame order: `ProjectSession::tick`.
- Image-set save rules and UI: `image_set/`; its close and outcome handling: `mod.rs`.
- Which tabs a flow may touch: `SessionParts` (and `StudioApp::session_parts`).
