# Module: src/single_image

## Purpose
App-shell half of the single-image mode (plan `dev-docs/single_image_mode_plan.md`, WP-2.4): the
session controller behind «Сохранить» / «Сохранить как», its two dialogs, and the small rules
`MangaApp` consults in that mode (visible tabs, window title, save hotkeys). The session itself (the
scratch chapter, `SessionKind::SingleImage`) is opened by `ms-project` + `studio_bootstrap.rs`; the
flatten into a file is `ms-tab-typing`'s `*_flatten_to_file` API. This module only sequences them.

It exists as a directory module because `src/app.rs` is over the 5000-line gate (plan D12):
`app.rs` keeps hooks only — the tab filter, the top-bar branch, the exit-dialog branch, the per-frame
`tick`, the hotkey arms and the discard-quiesce close.

## Architecture
```text
MangaApp.single_image: Option<SingleImageController>   (Some only for SessionKind::SingleImage)
  controller.rs  SingleImageController — side effects + UI
     └─ machine.rs  SaveMachine — pure state: target, dirty baseline, Phase, Status
  dialogs.rs     exit dialog, «Параметры JPEG» dialog (stateless draw fns)
  mod.rs         tab_visible, image_window_title, hotkey_specs, SaveParts, SaveOutcome, wasm stub
```
One save: `Idle -> [ChoosingFormat -> PickingPath] -> [JpegOptions] -> Preparing -> Writing -> Idle`.
- `ChoosingFormat`: «Сохранить как…» (button or Ctrl+Shift+S) toggles a small format panel under the
  button — one button per `ImageSaveFormat::save_formats()` (the one extension table; labels from
  `ImageSaveFormat::label`), plus «✕». «Сохранить» without an in-place target, including from the exit
  dialog, opens the same panel. «✕» / Escape / the toggle close it with no status; during a
  save-then-close that returns to the exit dialog (the controller defers that step to the next `tick`).
- `PickingPath { format }`: native `rfd` save dialog on a worker, filtered to the chosen format's
  extensions (plus uppercase copies for GTK), `<stem>.<ext>` preselected. The CHOSEN format is
  authoritative (`extension_enforced_path`): a name without one of its extensions gets the canonical
  one appended (no extension) or REPLACING the other one. When that changed file EXISTS the worker
  shows the dialog again with the full name (`changed_name_needs_confirmation`), so the native
  overwrite confirmation covers it — the dialog only ever checked the name the user typed.
- `JpegOptions`: once per session per target for «Сохранить»; always for «Сохранить как» to a JPEG.
  The confirmed quality is persisted to `user_config` (`SingleImage.jpeg_quality`) on a worker.
- `Preparing`: `TypingTabState::prepare_flatten_to_file` every frame until `Ready`; cancellable from
  the top bar (it may never become ready if a loader worker died).
- Dispatch (GUI thread, cheap): PS `flush_layers`, typing `flush_text_layers` (failure logged, the
  flatten composes from the live document), `baseline = edit_stamp`, `request_flatten_to_file`.
- `Writing`: `poll_flatten_to_file` every frame; success adopts the baseline and the target.

## Files and submodules
- `mod.rs`: shared types and pure rules; the wasm stand-in for the controller.
- `machine.rs`: the pure state machine and its unit tests. Edit it for any change of WHEN something
  is saved, asked or considered dirty.
- `controller.rs`: workers, channel polling, the dispatch sequence, top-bar UI and the format panel
  (`draw_format_panel`), status texts.
- `dialogs.rs`: the two `egui::Window` dialogs (stable `.id` = their i18n key).

## Contracts and invariants
- Dirty (plan D6) = `edit_stamp != saved_baseline || deferred_edits`, both defined in
  `controller.rs`: `edit_stamp` = `AutosaveGate::action_count()` + `TypingTabState::mask_edit_count()`
  (the clip mask is outside the gated writers; both counters only grow, so the sum moves iff either
  does); `deferred_edits` = `typing.has_pending_text_edits()` || `ps_editor.has_deferred_layer_edits()`
  (edits that changed the image but have not noted an action yet). Any new edit source that bypasses
  the gate must join one of the two. The baseline is captured AFTER the save's own flushes (they note
  actions and clear the deferred flags) and applied only on a successful write: an edit during the
  write stays dirty; a failure changes nothing but the status. A write in flight also counts as
  unsaved for the close prompt.
- In-place writability has one owner, `ImageSaveFormat::in_place_for` (ms-tab-typing): animated or
  non-writable sources make «Сохранить» act as «Сохранить как». The opened file is written only by an
  explicit save; a «Сохранить как» moves the target and retitles the window, the original is untouched.
- Encoding: PNG / WebP lossless with `AlphaPolicy::DropIfOpaque`, JPEG over white at the session
  quality, the source's ICC profile re-embedded.
- `tick` MUST run every frame whatever tab is active: it is the app's one `poll_flatten_to_file`
  caller (the typing tab defers project exports until a finished flatten is polled).
- Every close of a single-image session is a DISCARD quiesce in `app.rs`
  (`quiesce_writers_discarding`: no flush into the scratch, which `run_main` deletes after the window
  closed, plan D13). Save-then-close closes only when the write succeeded and the session is still
  clean; a cancelled picker / JPEG dialog returns to the exit dialog; a failure keeps the window open
  with the error status. While a write runs the exit dialog only offers «Cancel»; while the native
  «Сохранить как» dialog is open «Не сохранять» is disabled (that dialog cannot be closed from code and
  would be orphaned over the launcher).
- No file I/O on the GUI thread: picker, flatten and quality persistence run on `ms_thread` workers.
- The format panel is a transient `egui::Popup` (menu kind, `Order::Foreground`), not a dock panel;
  its open state is the machine's `Phase::ChoosingFormat`. It must keep blocking the canvas beneath
  it the way the exit dialog does: an interactable area sensing click AND drag (egui's hit-test then
  hands hover / press / drag over it to the panel, so canvas `Response::hovered()` stays false) and
  a popup registration (`Context::any_popup_open`, checked by the cleaning and PS-editor canvas
  gates). Outside clicks are ignored (`IgnoreClicks`), so one click never both closes it and reaches
  the canvas. Never make it `interactable(false)` or draw it with a bare painter.
- The save hotkeys (Ctrl+Shift+S before Ctrl+S, `Global` scope) are registered only in this mode;
  egui matches shortcuts logically, so the Shift variant must consume the event first.
- Native only: the web build never opens an image (plan D10); its controller is an uninhabited enum.

## Known limitations
- An in-place save replaces the file by an atomic rename: its permission bits (Windows: the read-only
  flag) are re-applied, but owner, ACLs and extended attributes are not, and a HARD-LINKED picture is
  detached from its other links (they keep the old bytes).
- Detached typing workers (mask save, placement save, create renders) are not joined before
  `run_main` deletes the scratch session; one finishing late can re-create a marker-less directory
  under the scratch base that the startup sweep never removes (`dev-docs/known_gaps.md` KG-031).

## Editing map
- Which tabs exist in the mode: `tab_visible` in `mod.rs` (and its test).
- Save / dirty / JPEG-options rules: `machine.rs`.
- What a save writes (format, alpha, ICC) or the dispatch order: `controller.rs::dispatch` / `tick`.
- Top-bar buttons, the format panel and status texts: `controller.rs` (`draw_top_bar`,
  `draw_format_panel`, `status_text`, `error_text`); which formats exist and their labels:
  `ImageSaveFormat` in `crates/ms-tab-typing/src/image_encode.rs`.
- Exit dialog wording or buttons: `dialogs.rs::draw_exit_dialog`; the close choreography itself:
  `src/app.rs` (`close_single_image_discarding`, `quiesce_writers_discarding`, `on_exit`).
