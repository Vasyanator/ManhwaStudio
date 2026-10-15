# Module: src/studio

## Purpose
The editor root `StudioApp` (`src/studio.rs`) and its parts. One `StudioApp` is one opened
session (a project chapter or an image set) inside the main window's studio screen: it creates the
shared chapter models, wires them into the tabs and canvases, drives the source-page residency,
routes the active tab, runs the input router and sequences persistence. It coordinates
subsystems; feature logic stays in the tab crates.

## Architecture
```text
shell/studio/ open worker: StudioBootstrap::build_blocking -> StudioApp::new(project, bootstrap, ai_backend)
StudioApp (studio.rs, eframe::App::ui driven by the shell's studio screen / the web entry)
  shared models   BubblesModel, CleanOverlaysModel, LayerDoc  (Arc<Mutex<_>>; TextMaskModel is
                  created here and held by the translation and cleaning tabs)
  residency       PageResidency (residency/)          decode pool, promotion, GPU upload, eviction
  writers         StagingWriters (staging_writers.rs) layer saver, bubbles saver, клин autosave, gate
  session         SessionMode (session/)              Project(ProjectSession) | ImageSet(ImageSetSession)
  tabs            Tabs (tabs.rs)                      ten program tabs (Translation = TranslationTab,
                                                      owning its canvas); StudioTab hook fan-outs
  view            ViewSync (view_sync.rs)             shared viewport / page across view tabs
  close           CloseFlow (close_flow.rs)           exit dialog, close request, incoming question
  panel dock      PanelDockState + PanelLayoutWriter  one per studio window
  input           InputRouter (ms-input)              registry (view set + tabs + session +
                                                      user overrides), begin_frame once per frame
  outbound        StudioOutput queue (studio_output.rs) -> take_outputs -> the window host
```
Root responsibilities that stay in `studio.rs`:
- **Construction.** `new` builds the models, the tabs, the input registry, the session's one
  `AutosaveGate` (shared by the three staging writers through `StagingWriters::spawn`) and the
  `PageResidency` pool, and hands the process's `LocalAiAvailability` (from the AI backend handle)
  to the Translation tab. The shared canvas settings are seeded once into `BubblesModel` by the
  pure `ms_canvas::shared_canvas_settings_from_project`; no canvas is seeded directly (each pulls
  them on its first draw). The input registry (`build_input_registry`: the shared `view.*` set,
  `Tabs::input_declarations`, `SessionMode::input_declarations`, then the bootstrap's overrides;
  invalid declarations fall back to an empty, logged registry) and the panel-dock restore are
  built from `Tabs` after every tab exists. It reads nothing from disk (see `bootstrap.rs`).
- **Input frame.** `InputRouter::begin_frame` runs ONCE per frame in `ui`, after the session tick,
  the worker polls and `sync_project_from_bubbles_model`, and BEFORE the top bar and the tab draw:
  matched keys are consumed before any widget sees them, and every consumer of the frame reads
  the same `InputFrame`. `FrameEnv` = the active tab, its `StudioTab::input_contexts`, and
  `modal_open` (the comic-type prompt, or the AI device prompt: its health refresh and
  `AiDevicePrompt::refresh` run right before the router and `is_open_for` decides this frame's
  state; the prompt is still drawn after the tab). Also right before the router,
  `cancel_capture_outside_settings` ends a binding capture whenever the active tab is not
  Settings: the one owner of "a capture never outlives the Settings tab" (a capture suppresses
  every binding).
  The session runs its own fired commands right after (`SessionMode::run_fired_commands`: the
  image set's save / save-as); the drawing tab gets the router through `TabFrame::input` (its
  frame and registry; the Settings pane edits the bindings, applied from the next frame).
  The registry also holds the canvas set (`ms_canvas::input::declarations`) and the shared brush
  set (`ms_tools::mask_brush::declarations`). Canvas gate: the view set's zoom commands and zoom
  axis are registered with `Condition::Context(ms_canvas::input::ZOOM_ACCEPTED)`
  (`canvas_gated_view_declarations`), which a canvas tab forwards from its `CanvasView` while the
  canvas executes them, so the keys and Ctrl+wheel stay with a tab's own readers while it blocks
  canvas zoom, and with the PS editor; removed by input-unification WP6-WP8.
- **Residency glue.** `studio.rs` supplies what residency must not know: the active tab's page
  window (`active_source_page_window`; for the page manager the one page its viewer draws), the
  pixel-inspection flag, and the eviction of the tabs' own GPU caches (`Tabs::evict_tab_gpu_caches`),
  each answered by the tabs' `ms_studio_tab::StudioTab` hooks.
  The canvas `cache_pages` setting is owned by `CleanOverlaysModel`; residency reads it there. Source textures are lent per frame to the canvas tabs and to
  the page manager's viewer.
- **Panel dock.** Exactly one `PanelDockState` per studio window, restored before the first frame
  from the bootstrap's `user_config` snapshot (`restore_panel_dock`, so a stored layout wins over a
  tab's default), lent to the drawing tab, polled for dirty layouts once per frame (persisted by
  the `PanelLayoutWriter` thread, flushed by `shutdown_blocking`). A tab hosts the dock iff its
  `StudioTab::default_dock_layout` is `Some` (today translation, cleaning, typing and the PS
  editor): that one hook feeds both the restore dictionary (`Tabs::dock_layout_defaults`) and the
  per-frame host verdict (`Tabs::hosts_panel_dock`). The canvas tabs run the dock inside
  `CanvasHooks::draw_canvas_overlay_top_left` (the state reaches them through
  `CanvasDrawParams::panel_dock`, which the canvas only carries); the PS editor runs it in its own
  draw. On a frame whose active tab is not
  a host, `PanelDockState::show_idle_sub_windows` keeps the detached OS windows alive (they are
  immediate viewports and exist only while shown). Persistence format: `ms-widgets`'
  `panel_dock/persist.rs`.
- **Window boundary.** `StudioApp` sends no viewport command (close, maximize, title), never
  styles the window and owns no `egui::Options`; it has no `on_exit`. A close it decides (exit dialog, save-then-close,
  launcher button, OS close via `request_os_close`, delivered images via
  `request_close_for_incoming`) leaves as `StudioOutput::Close(PendingCloseAction)`.

## Files and submodules
- `bootstrap.rs`: `StudioBootstrap` (`build_blocking(project)`), the pre-read inputs of
  `StudioApp::new`: the merged startup `user_config` root, the input `HotkeyOverrides` (read from
  that root; a version-1 section is migrated and written back once, on the same worker),
  `SettingsTabSeed`,
  `SpellcheckWordsSeed`, `TypingPanelSeed` and whether the `_unsaved` staging dir existed. Built
  on the shell's open worker (inline on the web); `new` performs no config or document read.
- `shutdown.rs`: `StudioApp::shutdown_blocking() -> ShutdownReport`, the studio's blocking
  teardown: its ORDER (the all-canvas bubble flush unless discarding, writer steps from
  `StagingWriters`, the decode-pool release between them),
  the tabs' exit-only settings (ONE `user_config` read-modify-write over
  `Tabs::write_exit_settings`), the tabs' debounced stores (`Tabs::flush_debounced_stores_blocking`)
  and the panel layouts, plus the permanent compile-time `StudioApp: Send` guard.
- `staging_writers.rs`: `StagingWriters`, the three `_unsaved` writers of the session as an owned
  struct (autosave gate, клин autosave thread + `OverlayAutosaveControl`, Arc clones of the layer
  document, bubbles and clean-overlay models, the staging клин dir, the DISCARD latch). One method
  per writer sequence: `spawn`, `quiesce_discarding(typing)`, `restart_after_failed_discard`,
  `quiesce_for_page_op` (returns the клин thread for the page-op worker), `take_overlay_thread`
  (the discard delete job), `stop_overlay_autosave_blocking` and `drain_for_shutdown(typing,
  report)` (teardown); `gate()` / `overlay_control()` and the model accessors (`layer_doc()`,
  `bubbles_model()`, `clean_overlays_model()`) for the project session's workers and the image
  set's dirty baseline.
- `close_flow.rs`: `CloseFlow`, the session's close intent as an owned struct: the exit dialog
  shown (`Option<PendingCloseAction>`: `Exit` = OS close, `ReturnToLauncher`, `OpenIncoming`), the
  outbound close request (`finalize`, `take_close_request`, drained by `take_outputs`), the received-images question
  (`IncomingOpenRequest`: count + which form is shown) and its decline latch; the pure
  `os_close_decision` and `CloseFlow::request_os_close` (a second OS close over the exit dialog
  follows the session's `OsClosePolicy`).
- `studio_output.rs`: `StudioOutput`, the ONE outbound queue to the window host (`Title`,
  `Close`, `IncomingDeclined`, `ReloadRequested`), and the pure `collect_outputs` (order and
  precedence of one drain). `StudioApp::take_outputs` fills it from the latches where each output
  is decided (`CloseFlow`, the project session's reload request, the image-set controller's
  pending title); `StudioApp` sends no root viewport command itself.
- `incoming_open.rs`: the session's answer to images another app delivered
  (`request_close_for_incoming`; the decline leaves as `StudioOutput::IncomingDeclined`): the unsaved-changes dialog's
  `OpenIncoming` variant (drawn by the session's exit dialog) or the one-line confirmation
  (`draw_incoming_confirm`), and the pure `incoming_decision` policy. The decisions stay on
  `StudioApp` (they need the dirty state and the running discard cleanup); the question's state
  lives in `CloseFlow`.
- `session/`: `SessionMode`, the session strategy chosen once in `StudioApp::new`
  (`Project(ProjectSession)` / `ImageSet(Box<ImageSetSession>)`), its dispatch methods and
  `SessionParts` (the app parts a flow borrows). `session/project/` owns the project persistence
  state and flows (save-to-project, page op, DISCARD, exit dialog, unsaved cache);
  `session/image_set/` the image-set save controller. See `session/MODULE_README.md`.

- `busy.rs`: `PersistenceActivity`, the `Copy` snapshot of the persistence busy flags (save,
  save preload, page op / its failure / pending reload, discard delete job — the session's
  `PersistenceJobs` — plus typing export and page manager clean op), built by
  `PersistenceActivity::of` (the one composition, used by `StudioApp::persistence_activity` and
  the project session's flows), and one named predicate per
  subset the gates read (`structural_op_active`, `save_busy`, `blocks_page_op`,
  `clean_op_in_flight`, `exit_cleanup_running`), plus `save_blocker` (`SaveBlocker`: the one
  cause a project save is refused for; saves read the strict `blocks_page_op` set plus the clean worker).
- `residency/`: `PageResidency`, the source-page residency pipeline (decode pool, in-order
  promotion, budgeted tiled GPU upload, overlay and page-cache loaders, page geometry and resident
  textures lent to the drawing tab, memory-pressure eviction). Unlike its siblings it does NOT
  reach `StudioApp`'s fields: it is an owned struct with a narrow API, and every tab-dependent input
  is passed in by `studio.rs`. See `residency/MODULE_README.md`.
- `prompts.rs`: the two modal prompts drawn over every tab, as owned structs with a narrow API
  (no `StudioApp` field access): `ComicTypePrompt` (`draw` returns the clicked `ComicType`; the
  caller applies it with `apply_comic_type_preset` and reports back through `resolve` / `fail`)
  and `AiDevicePrompt` (`refresh` opens it from the health snapshot, `draw` sends the
  `SetDevice` / `SetOnnxDevice` probe commands and returns whether it is still open).
- `tabs.rs`: `Tabs`, the ten program tabs (field `tabs`; the Translation tab is its host
  `ms_tab_translation::TranslationTab`, which owns the Translation canvas), the one `AppTab` ->
  tab mapping as `StudioTab` trait objects (`get` / `get_mut`) with the order-free fan-outs in
  `AppTab::ALL` order (`for_each` / `for_each_mut`, `input_declarations`, `dock_layout_defaults`,
  `hosts_panel_dock`, `evict_tab_gpu_caches`; the teardown's `write_exit_settings` and
  `flush_debounced_stores_blocking`), and `Tabs::draw(drawn, ui, TabFrame) -> Vec<AppRequest>`, the per-tab
  draw dispatch of `ui`. The
  frame context `TabFrame` is borrowed from `StudioApp` (project, page geometry + resident textures
  lent by `PageResidency`, panel dock, busy snapshot, canvas status, the input router); every tab ->
  app request is an `AppRequest` (`PageOp`, `OpenPage`, `NavigateSettings`, `CharactersChanged`,
  `OpenNotesForCharacter`, `TermsChanged`), applied by `StudioApp::apply_app_request`. Ordered
  app -> tab calls (setters, flushes, the §12
  sequences) stay method calls on `Tabs` fields; order-free ones go through the trait.
- `view_sync.rs`: `ViewSync`, the shared view state of the view tabs (the three canvas tabs and
  the PS editor): shared viewport snapshot and its owner tab, shared page and page-local center,
  previous view tab. `before_draw` (tab-leave flushes, then snapshot / page apply),
  `after_draw` (publish), `open_page` (explicit focus). Owned struct: the tab states it drives
  come in as a borrowed `ViewTabs` (`Tabs::view_tabs`). Also the ONE tab -> canvas mapping (`canvas_slot`, with
  `ViewTabs::canvas_mut` / `canvas_of`), used by every per-canvas fan-out in `studio.rs`.

## Contracts and invariants
- `StudioApp::new` reads nothing from disk: a new construction-time input is read in
  `StudioBootstrap::build_blocking` and passed in. Only reads whose result may be taken at open
  time belong there; anything that must observe state created during construction (globals set
  by a tab, model state, thread spawns) stays in `new`. Known remaining probe: `BubblesModel::new`
  (`ms-models`) checks its staging document.
- `shutdown_blocking` blocks: it runs from the window host's `on_exit`, before a structural
  reload, or on a worker that owns the moved `Box<StudioApp>` — never in a regular frame.
- Every failure it observes is logged with the `[app] on_exit:` prefix AND returned in
  `ShutdownReport`; it never claims a drain it did not observe.
- `StudioApp: Send` must hold on native targets: a non-`Send` field anywhere under `StudioApp`
  (including a `Box<dyn CleaningTool / AiEngine / PsTool>` implementor) is a build error here.

- `StagingWriters` keeps each writer sequence in ONE method in its fixed order (discard: latch ->
  drop typing's deferred edits -> layer `shutdown_saver_discarding` -> bubbles pause -> клин
  stop-now; restart: un-latch -> layer saver -> bubbles resume -> fresh клин control + thread, all
  on the same gate). A sequence that calls into a tab mid-way takes that tab as a parameter; the
  save / page-op / discard orchestration around the writers (tab flushes, workers, delete job)
  is `ProjectSession`'s (`session/project/`).
- `CloseFlow` holds no decision that needs the session's dirty state: `StudioApp` passes it in
  (`request_os_close(has_unsaved_changes)`). The OS close arms nothing; a project's
  save-then-close is armed only by its exit dialog's Save and reports its end through
  `CloseFlow::end_save_then_close` (a clean success finalizes; anything else closes the dialog).
- The session kind is decided once (`SessionMode::from_project`); every per-kind behaviour is a
  `SessionMode` method, and session flows reach the app only through `SessionParts`.
- `request_close_for_incoming` never touches the images (the window host keeps them) and never
  closes anything itself: it ends in `PendingCloseAction::OpenIncoming` through the regular close
  flow, or in a decline. A close decided or asked for another reason wins (refused, `false`). A
  clean confirmation re-checks unsaved changes on «Открыть» and switches to the unsaved dialog.

- Every "is persistence busy" decision in `studio.rs` goes through a `PersistenceActivity`
  predicate; no gate spells its own OR of fields. A predicate is a SUBSET of flags, shared by
  every consumer that reads exactly that subset; a consumer that needs a different subset gets a
  new predicate instead of editing a shared one. The snapshot is taken where the decision is made,
  before tab states are borrowed for drawing.

- `PageResidency` knows no tab type: the page window to keep and the pixel-inspection flag are
  parameters, the canvas `cache_pages` setting is read from `CleanOverlaysModel` (its owner; no
  code reads a canvas's `state.cache_pages` copy for it), and tab GPU caches are evicted by
  `studio.rs`.
  `shutdown_blocking` calls `PageResidency::shutdown` before the typing flush and layer barrier.

- While either prompt is open the input router is suppressed (`FrameEnv::modal_open`; nothing
  fires, no mode is active) and the studio repaints every frame (the `ui` call site in
  `studio.rs`). Applying a comic type stays in `studio.rs`: it writes the
  project (memory + settings file), then applies the preset to the `BubblesModel` canvas-settings
  snapshot (`SharedCanvasSettings::apply_comic_type_preset`, the one preset owner, also used by the
  Settings pane) and pushes it to `BubblesModel`, `CleanOverlaysModel` and the Settings tab; the
  canvases pull it by revision.

- `ViewSync::before_draw` runs right before the active tab draws and `after_draw` at the end of
  the frame, with the tab the frame ended on. Inside `before_draw` the PS-editor
  layer flush and the Typing deferred-text flush on tab leave, and the bubble flush of every canvas
  the active tab does not draw (each frame; a no-op once clean), run BEFORE the snapshot apply, so
  the destination tab reads the latest layer and bubble state and at most the active canvas holds
  unwritten bubble edits. Every tab switch (tab bar, page-manager open, settings deep link,
  input commands) is covered because the destination tab first draws after a `before_draw`. The PS editor never owns the scroll
  snapshot; it only feeds the shared page, zoom and center. Non-view tabs leave the state alone.
- Code in `src/studio` reaches a tab's canvas only through `canvas_slot` (all three canvas tabs
  expose theirs as `canvas()` / `canvas_mut()`; Translation's is `TranslationTab`'s) or, for all
  three at once, `for_each_canvas_mut`; no site spells its own Translation / Cleaning / Typing match.
- Per-tab ORDER-FREE questions go through `Tabs` and `ms_studio_tab::StudioTab`, never a per-tab
  `match` in `studio.rs`: input declarations (`Tabs::input_declarations`, `AppTab::ALL` order,
  after the view set and before the session's; pinned by `input_registry_golden`), the ACTIVE
  tab's `input_contexts` (every frame, before `begin_frame`), dock hosts, GPU eviction, the active tab's
  source-page window and NEAREST flag, the every-frame `poll_background` (after the residency's
  decoded-page poll, before any busy gate reads `clean_op_in_flight`) and
  `on_committed_tree_changed` (the session latch `take_committed_tree_changed`, drained right
  after `SessionMode::tick`). Drawing and every ordered sequence stay explicit calls.

- `Tabs::draw` never changes `StudioApp::active_tab` and never polls the panel dock: `ui` takes the
  panel-dock verdict (`PanelDockPass`) for the tab that draws, then after the draw polls the dirty
  layouts (only when that tab ran the dock, `PanelDockPass::dock_ran`), then applies the requests in
  the order the tab raised them — the tab switches (page open, deep link, «open notes») land there,
  before the idle sub-window pass, the trims, the prompts and `ViewSync::after_draw`. The frame
  context is built before any tab state is borrowed (the busy snapshot first).

## Save / page-op / discard / exit choreography
The project session (`session/project/`, `ProjectSession`) sequences the three staging writers
(layer saver, bubbles saver, клин autosave with its `OverlayAutosaveControl`) around every
destructive point; the teardown is `shutdown.rs`. The writers, their gate and the DISCARD latch
are `StagingWriters` (`staging_writers.rs`): every writer-side step below is one of its methods,
in the order given here; the tab flushes, workers and the delete job around them are
`ProjectSession` methods (`save.rs`, `page_op.rs`, `discard.rs`), each sequence in ONE method.
The writers' own contracts: `crates/ms-models/src/MODULE_README.md`,
`crates/ms-canvas/src/MODULE_README.md`; typing's deferred text edits:
`crates/ms-tab-typing/src/MODULE_README.md`.
- Save trigger (`request_save_to_project`): refused with the blocker's text while any
  `SaveBlocker` holds — the page-op set (`blocks_page_op`: page op / its failure / reload, a save
  under way, the DISCARD cleanup, a typing export) plus the page manager's clean worker; the exit
  dialog's Save is disabled during all of them except a save already under way.
- Canvas bubble edits: a canvas keeps debounced / focused bubble edits locally and writes whole
  records (last writer wins). `ViewSync::before_draw` therefore flushes every canvas the active
  tab does not draw, each frame (`CanvasView::flush_all_bubble_edits_now`, a no-op once clean), so
  at most the active canvas holds unwritten edits; every persistence point below flushes all three
  through `SessionParts::flush_canvas_bubble_edits`, the ACTIVE canvas last (its edit is newest).
- `start_save_to_project`: the bubble edits of ALL three canvases first (the 1 s text debounce and
  focused bubbles included; model only, no I/O), then PS `flush_layers`, then typing
  `flush_text_layers`. A flush `Err` or any `failed_pages` ABORTS the save (nothing merged, staging
  kept, status shown) — an `Err` is never degraded to an empty owned-page set. The worker then
  barriers the bubbles saver (holding it), the layer saver (a failed TEXT write aborts), pauses the
  клин autosave and takes the dirty клин snapshots (a failed write restores them via
  `restore_dirty_save_snapshots` and aborts), merges (`ms_project::save_merge`, with the per-page
  layer-manifest merge injected from `layer_model::persist`), resumes, and re-barriers; success
  calls `AutosaveGate::force_flush`.
- Save-then-close (an exit dialog's «Сохранить главу»): arms the dialog's close action, then
  requests the save; Cancel clears the arm. The arm ends with THAT save (`ProjectSession::tick`):
  a clean success closes the dialog and requests the close; a failed, refused or aborted save, or a
  success that left unsaved work, closes the dialog without a close (the status line keeps the
  save's own error). The OS close button never arms it.
- `start_page_op`: flushes the bubble edits of all three canvases (same flush as the save), PS
  layers and typing text; `page_op_text_quiesce` refuses the op when edits stay unwritten or the
  doc lock is poisoned (`NoLayersDir` / `NoLayerDoc` pass only when nothing was pending). Then
  `StagingWriters::quiesce_for_page_op`: `force_flush`, клин autosave `request_stop_now`, its
  thread handed over; the worker joins it, takes клин snapshots, pauses the bubbles saver,
  barriers the layer saver (failed pages are logged as a warning, not an abort), writes the
  snapshots to staging and dispatches the engine. Page ops are not staged; the shell rebuilds the
  app from disk (`StudioOutput::ReloadRequested`).
- Unsaved work held inside tabs: `session::DeferredTabEdits` is the ONE owner of which tabs hold
  edits not yet handed to a staging writer (typing's deferred text edits, the PS editor's deferred
  layer edits); the project's dirty check (latched in `refresh_unsaved_changes_cache`, and read live
  by `SessionMode::has_unsaved_changes` outside a discard) and the image set's both read it, so a
  close with only such an edit pending still opens the exit dialog.
- DISCARD (`start_exit_cleanup` = `StagingWriters::quiesce_discarding` + delete job): the quiesce
  latches the discard first, DROPS (never flushes) typing's deferred edits,
  `shutdown_saver_discarding` on the layer saver, pauses the bubbles saver, `request_stop_now` on
  the клин autosave; then staging is deleted on a job that joins that thread.
- Image-set session (`session/image_set/`): staging is a scratch that is never merged and that the
  shell's teardown worker deletes after the app was dropped, so EVERY close quiesces the discard
  way and nothing is flushed: «Не сохранять» and a successful save-then-close go through
  `close_image_set_discarding` in `session/mod.rs` (quiesce + `CloseFlow::finalize`, no delete
  job, клин thread joined in `shutdown_blocking`); `shutdown_blocking` itself quiesces first when
  no dialog did (`SessionMode::before_shutdown`). Dirty is the controller's (`AutosaveGate::
  action_count` vs the last-write baseline, plus the deferred tab edits, plus a write in flight),
  never the staging probe. The save-then-close keeps the window open on a failed write and
  re-opens the exit dialog when the user backed out of the picker / JPEG options. A second OS
  close while its exit dialog is open is ignored (`SessionMode::os_close_policy`): only the
  dialog's buttons close an image set; a project's second OS close still exits (staging survives).
- Failed discard (`abort_discard_after_failed_cleanup` ->
  `StagingWriters::restart_after_failed_discard`): releases the latch, restarts ALL three writers
  on the same `AutosaveGate` (`enable_background_saver`, `resume_saver_after_failed_discard`, a
  fresh `OverlayAutosaveControl` + autosave thread); the app then marks the session unsaved.
- `shutdown_blocking` (`shutdown.rs`, run by the shell's Closing-screen worker for every leave of
  the studio — exit, return to the launcher, reload — or inline from the shell's `on_exit` when
  eframe ended the window with the studio open): unless discarding, flush the bubble edits of all
  three canvases into the bubbles model (so the bubbles-saver shutdown below writes them) and the
  PS editor's deferred layer edits onto the layer saver (`flush_layers_if_dirty`, so the barrier
  below covers them; same bubbles -> PS -> typing order as the save and the page op), then
  flush-and-stop the клин autosave (stop-now when discarding) and join it
  (`StagingWriters::stop_overlay_autosave_blocking`), release the decode pool, then
  (`StagingWriters::drain_for_shutdown`) — unless discarding — enqueue typing's deferred edits
  INLINE, barrier the layer saver and report its failed pages, then shut down layer and bubbles
  savers. Only after that drain: every tab's exit-only `user_config` keys in ONE read-modify-write
  (`StudioTab::write_exit_settings`; a failure is logged, the file left as it was), then every
  tab's debounced user-global stores (`StudioTab::flush_debounced_stores_blocking`; typing: the
  per-font settings, then the default local-preset set), then the panel-layout poll and the
  `PanelLayoutWriter` flush-and-join.

## Editing map
- Add a construction-time read for the studio: a field of `StudioBootstrap` (`bootstrap.rs`),
  consumed in `StudioApp::new`.
- Add a tab -> app request, or change what a tab gets for its draw: `tabs.rs` (`AppRequest`,
  `TabFrame`), its application in `studio.rs` (`apply_app_request`). A new dock host, input action
  or input context,
  GPU cache, page window or background poll of a tab: that tab's `impl StudioTab` (in its crate;
  Settings: `src/tabs/settings/studio_tab.rs`); a new order-free hook: `ms-studio-tab` plus a
  fan-out in `tabs.rs`.
- Change the teardown order or what it reports: `shutdown.rs`, and the teardown bullet of the
  choreography above.
- Change how a running session answers delivered images: `incoming_open.rs` (the dialog body:
  `session/project/exit_dialog.rs` and `session/image_set/dialogs.rs`); its state:
  `close_flow.rs`.
- Change what differs between a project and an image set, or a project save / page op / discard:
  `session/` (its `MODULE_README.md`).
- Change what a writer does at a discard, a failed discard, a page op or teardown:
  `staging_writers.rs` (its real-model test), and the choreography above.
- Change the exit dialog / close request / OS close policy: `close_flow.rs` (its lifetime table
  test), the dialog drawing in `session/` (`SessionMode::draw_exit_dialog`).
- Change which operations block a save, a page op, the save button or the typing export:
  `busy.rs` (predicates and their table tests), then the call site in `studio.rs` or
  `session/project/`.
- Change page decode, promotion, upload budgets, the page cache loader or pressure eviction:
  `residency/`; which pages the active tab keeps: the tab's `StudioTab::source_page_window`.
- Change how the canvas tabs and the PS editor share zoom / scroll / page across tab switches, or
  which tab draws which canvas: `view_sync.rs` (its tests), call sites in `studio.rs` `ui` and
  `open_page_in_tab`.
- Change the comic-type or AI device prompt UI or its send rules: `prompts.rs` (pure decisions
  and their tests live there); what a chosen comic type changes: `apply_comic_type_preset` in
  `studio.rs`.
