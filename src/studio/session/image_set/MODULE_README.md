# Module: src/studio/session/image_set

## Purpose
The image-set (single-image) strategy of a studio session: the session controller behind «Сохранить» / «Сохранить как» for
one page or an ordered set of pages, its dialogs (exit, JPEG options, non-empty folder, skipped
files), and the small rules the session consults in that mode (visible tabs, window title, save
hotkeys). The session itself (the scratch chapter, `SessionKind::SingleImage`) is opened by
`ms-project` + `src/shell/studio/`; the flatten into a file is `ms-tab-typing`'s `*_flatten_to_file`
API. This module only sequences them.

`StudioApp` never calls in here directly: `SessionMode::ImageSet` (`../mod.rs`) dispatches the tab
filter, the top bar, the exit dialog, the per-frame `tick`, the dialogs, the save hotkeys and the
discard-quiesce close to it.

## Architecture
```text
StudioApp.session = SessionMode::ImageSet(Box<ImageSetSession>)   (only for SessionKind::SingleImage)
  controller.rs  ImageSetSession — side effects + UI
     └─ machine.rs  SaveMachine — pure state: per-page target + written digest, dirty baseline,
                    Phase, the running save (Run: job queue, totals), Status
  dialogs.rs     exit dialogs, «Параметры JPEG», non-empty folder, skipped files (stateless draw fns)
  mod.rs         tab_visible, image_window_title, hotkey_specs, SaveParts, SaveOutcome, wasm stub
```
One page (one-image session): `Idle -> [ChoosingFormat -> PickingPath] -> [JpegOptions] -> Preparing -> Writing -> [Committing] -> Idle`.
An image set (2+ pages):
- «Сохранить»: `[JpegOptions] -> (Preparing -> Writing)×N` over the pages in order — a page with an
  in-place target is flattened into it with `unchanged_if = its written digest` (an unchanged page
  is neither encoded nor written), a page without one is composed only. Pages that changed but have
  no file (no target, or the in-place write was refused for permission / read-only) then go to ONE
  folder: `ChoosingFormat (header: N pages have no file) -> PickingFolder -> [ConfirmFolder] ->
  [JpegOptions] -> (Preparing -> Writing)×k -> Committing`.
- «Сохранить как»: the same folder flow over ALL pages, always written; names come from
  `folder_file_names` (source stems, case-insensitive collisions get ` (2)`, ` (3)`).
- A failing page is recorded and the pass goes on; «Стоп» ends it after the page in flight.
- `ChoosingFormat`: «Сохранить как…» (button or Ctrl+Shift+S) toggles a small format panel under the
  button — one button per `ImageSaveFormat::save_formats()` (the one extension table; labels from
  `ImageSaveFormat::label`), plus «✕». «Сохранить» without an in-place target, including from the exit
  dialog, opens the same panel. «✕» / Escape / the toggle close it with no status; during a
  save-then-close that returns to the exit dialog (the controller defers that step to the next `tick`).
- `PickingPath { format }`: `ms_platform::file_dialog` save dialog (`SaveFile`, `pick_blocking`) on
  a worker, filtered to the chosen format's extensions, `<stem>.<ext>` preselected; a dialog that
  cannot be shown ends the save as `PickerUnavailable` with the dialog layer's message. The answer
  is an `ms_platform` dialog target: the machine writes its write path and keeps the target
  (`pending_commit`) for `Committing`.
- `Committing`: after the flatten wrote a dialog target's file, the controller commits it on a
  worker (`take_commit` -> `spawn_commit` -> `commit_finished`); a commit error is a failed save.
  Only where platform paths are the user's own files (`writes_in_place`, from
  `PlatformFeature::PersistentDirectoryPicker`: desktop) are the opened file and the last
  «Сохранить как» target rewritten in place by «Сохранить»; elsewhere (Android SAF) there is no
  in-place target and every save goes through the dialog and its commit. The CHOSEN format is
  authoritative (`extension_enforced_path`): a name without one of its extensions gets the canonical
  one appended (no extension) or REPLACING the other one. When that changed file EXISTS the worker
  shows the dialog again with the full name (`changed_name_needs_confirmation`), so the native
  overwrite confirmation covers it — the dialog only ever checked the name the user typed.
- `JpegOptions`: once per session per target for «Сохранить» (an image set: before an in-place pass
  when any page's JPEG target is unconfirmed — the pass cannot know which pages changed before
  composing them); always for «Сохранить как» / a folder written as JPEG.
  The confirmed quality is persisted to `user_config` (`SingleImage.jpeg_quality`) on a worker.
- `Preparing`: `TypingTabState::prepare_flatten_to_file` every frame until `Ready`; cancellable from
  the top bar (it may never become ready if a loader worker died).
- Dispatch (GUI thread, cheap): PS `flush_layers`, typing `flush_text_layers` (failure logged, the
  flatten composes from the live document), `baseline = edit_stamp`, `request_flatten_to_file`.
- `Writing`: `poll_flatten_to_file` every frame; the result (`Written` with the digest /
  `Unchanged` / `Composed`, or an error) advances the pass. The run ends after its last stage; only a
  COMPLETE run (every page written or unchanged, nothing failed, stopped early or cancelled) adopts
  the baseline. Pages written in place adopt their digest at once; pages written through a dialog
  target adopt digest and (where paths are written in place) target only after the commit.
- The PS / typing flushes run once per stage (before its first dispatch); the baseline is the
  stamp of the run's FIRST dispatch.
- The skipped-files notice (`session.skipped()`) is shown once after the open; the window title
  is computed from the actually opened pages on the first `tick` (`image_window_title(first, N)`)
  and handed out by `take_title` (`SessionMode::take_title` -> `StudioOutput::Title`): the
  controller never sends a viewport command, the window host sets the title.

## Files and submodules
- `mod.rs`: shared types and pure rules; the wasm stand-in for the controller (uninhabited, so
  `SessionMode::ImageSet` can never be built on the web).
- `machine.rs`: the pure state machine and its unit tests. Edit it for any change of WHEN something
  is saved, asked or considered dirty.
- `controller.rs`: workers, channel polling, the dispatch sequence, top-bar UI and the format panel
  (`draw_format_panel`), status texts.
- `dialogs.rs`: the `egui::Window` dialogs (stable `.id` = their i18n key): exit (plus its
  open-received-images variant `draw_open_incoming_exit_dialog`), JPEG options, non-empty folder,
  skipped files.

## Contracts and invariants
- Dirty = `edit_stamp != saved_baseline || deferred_edits`, both defined in
  `controller.rs`: `edit_stamp` = `AutosaveGate::action_count()` + `TypingTabState::mask_edit_count()`
  (the clip mask is outside the gated writers; both counters only grow, so the sum moves iff either
  does); `deferred_edits` = `session::DeferredTabEdits::any` (typing's pending text edits, the PS
  editor's deferred layer edits: edits that changed the image but have not noted an action yet; the
  same owner answers the project's dirty check). Any new edit source that bypasses the gate must
  join `edit_stamp` or `DeferredTabEdits`. The baseline is captured AFTER the save's own flushes (they note
  actions and clear the deferred flags) and applied only when the whole save succeeded: an edit
  during the write stays dirty; a failed, stopped or cancelled page leaves the session dirty (the
  pages that were written stay written). A write in flight also counts as unsaved for the close
  prompt.
- "Changed" has one owner, `ms_project::PixelDigest`: the machine keeps per page the digest last
  written to its target (initially `SourceImageFacts::opened_digest`); the typing flatten compares.
- In-place writability has one owner, `ImageSaveFormat::in_place_for` (ms-tab-typing): animated or
  non-writable sources make «Сохранить» act as «Сохранить как». The opened file is written only by an
  explicit save; a «Сохранить как» moves the target(s) and retitles the window, the originals are
  untouched. A one-page session reports a refused in-place write as an error (no folder for one file).
- Encoding: PNG / WebP lossless with `AlphaPolicy::DropIfOpaque`, JPEG over white at the session
  quality, the source's ICC profile re-embedded.
- `tick` MUST run every frame whatever tab is active: it is the app's one `poll_flatten_to_file`
  caller (the typing tab defers project exports until a finished flatten is polled).
- Every close of a single-image session is a DISCARD quiesce (`StagingWriters::quiesce_discarding`,
  called by `SessionMode`'s `close_image_set_discarding` and `before_shutdown`: no flush into the
  scratch, which the shell's teardown worker deletes after the app was dropped). Save-then-close closes only when the session is still
  clean after a COMPLETE save; a cancelled format panel / picker / folder confirmation / JPEG dialog
  returns to the exit dialog; a failure or a stop keeps the window open with the status. While a write runs the exit dialog only offers «Cancel»; while the native
  «Сохранить как» dialog is open «Не сохранять» is disabled (that dialog cannot be closed from code and
  would be orphaned over the launcher).
- No file I/O on the GUI thread: picker, flatten, commit and quality persistence run on named
  `ms_thread` workers started through `spawn_worker` (`controller.rs`); a failed spawn surfaces as
  the worker-lost error.
- The format panel is a transient `egui::Popup` (menu kind, `Order::Foreground`), not a dock panel;
  its open state is the machine's `Phase::ChoosingFormat`. It must keep blocking the canvas beneath
  it the way the exit dialog does: an interactable area sensing click AND drag (egui's hit-test then
  hands hover / press / drag over it to the panel, so canvas `Response::hovered()` stays false) and
  a popup registration (`Context::any_popup_open`, checked by the cleaning and PS-editor canvas
  gates). Outside clicks are ignored (`IgnoreClicks`), so one click never both closes it and reaches
  the canvas. Never make it `interactable(false)` or draw it with a bare painter.
- The save hotkeys (Ctrl+Shift+S before Ctrl+S, `Global` scope) are registered only in this mode;
  egui matches shortcuts logically, so the Shift variant must consume the event first.
- Native only: the web build never opens an image; its controller is an uninhabited enum.

## Known limitations
- An in-place save replaces the file by an atomic rename: its permission bits (Windows: the read-only
  flag) are re-applied, but owner, ACLs and extended attributes are not, and a HARD-LINKED picture is
  detached from its other links (they keep the old bytes).
- Detached typing workers (mask save, placement save, create renders) are not joined before
  the shell's teardown worker deletes the scratch session; one finishing late can re-create a marker-less directory
  under the scratch base that the startup sweep never removes (`dev-docs/known_gaps.md` KG-031).

## Editing map
- Which tabs exist in the mode: `tab_visible` in `mod.rs` (and its test).
- Save / dirty / JPEG-options rules: `machine.rs`.
- What a save writes (format, alpha, ICC) or the dispatch order: `controller.rs::dispatch` / `tick`.
- Top-bar buttons, the format panel and status texts: `controller.rs` (`draw_top_bar`,
  `draw_format_panel`, `status_text`, `error_text`); which formats exist and their labels:
  `ImageSaveFormat` in `crates/ms-tab-typing/src/image_encode.rs`.
- Exit dialog wording or buttons: `dialogs.rs::draw_exit_dialog`; the close choreography itself:
  `../mod.rs` (`SessionMode::draw_exit_dialog`, `close_image_set_discarding`, `before_shutdown`),
  `src/studio/staging_writers.rs` (`quiesce_discarding`) and `src/studio/shutdown.rs`.
