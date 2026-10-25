# Module: crates/ms-launcher/src/new_project/window

## Purpose
`NewProjectWindowState`: the content of the launcher's "New Project" window — the left control
column (simple wizard and full panel), the ribbon viewer, crop and manual-cut editing, the
advanced downloader with its auto-review window, image processing (stitch, waifu2x, Reline), save,
and the wiring of every background controller of the parent `new_project` module.

## Architecture
One type, many `impl` blocks. `mod.rs` declares every type of the window (`NewProjectWindowState`
and its private state types), the layout constants and the entry points the launcher calls
(`new`, `poll`, `show_content`, `show_children`, `on_window_closed`, the takers,
`set_projects_root`) plus the `tutorial`-feature hooks. Each child file adds one area's methods to
`NewProjectWindowState` and holds that area's free helpers.

Visibility and imports:
- Children start with `use super::*;` and so see every import, constant and type of `mod.rs`,
  including private fields (children of a module may read its private items).
- Methods and free helpers in children are `pub(super)`: callable from `mod.rs` and every sibling,
  invisible outside `window`.
- `mod.rs` glob-imports the free helpers of the children that siblings call
  (`use self::<child>::*;`), so a sibling or the tests call them by their bare names. A child whose
  helpers are used only inside it (`capture.rs`, `options.rs`, `save.rs`) is not glob-imported;
  when a sibling starts calling one of its helpers, add the glob line (or import it by path).

Control flow per pass: `poll` (mod.rs) calls the `poll_*` methods — the controller polls in
`jobs.rs`, the clipboard/capture polls in `capture.rs`, the folder-dialog polls with their areas
(`ribbon_view.rs` cut-like folder, `save.rs` save folder); `show_content` draws `show_contents` -> `show_left_panel` (left_panel.rs) and
`show_viewer_panel` (ribbon_view.rs), then the dialogs (crop editor, auto review, screen-capture
overlay, overwrite confirm).

## Files and submodules
- `mod.rs`: types, constants, entry points, tutorial hooks.
- `left_panel.rs`: left column, mode tabs, the simple-mode step wizard and navigation, import
  section and quick downloader form.
- `ribbon_view.rs`: viewer panel, ribbon layout/resize, per-page controls and edits, manual cut
  guides, cut-like / stitch-split entry points, ribbon geometry helpers.
- `crop_editor.rs`: the per-page crop editor window and crop drag math.
- `capture.rs`: clipboard paste and screen-capture overlay (macOS-only bounds helper and its test
  module included).
- `advanced_ui.rs`: advanced browser downloader form, auto-review window, `start_advanced_*` /
  `finish_advanced_*` wrappers, URL-prefix save.
- `processing_ui.rs`: stitch, image-processing, waifu2x and Reline sections, Reline model catalog
  combo and the simplified Reline gallery.
- `options.rs`: ribbon-image snapshots for workers, option parsers, `can_start_*` predicates.
- `jobs.rs`: `start_*` / `poll_*` for every controller, `handle_window_closed`, the test-chapter
  site check (wasm/native twins), progress stage/operation titles, stitch status names.
- `save.rs`: save section, project-catalog selection helpers, the save launch flow and the
  overwrite check.
- `widgets.rs`: progress display, section/field/combo helpers, sized buttons,
  `close_batch_window`.
- `prefs.rs`: `user_config` preferences of the window (URL presets, Reline UI mode, ribbon width).
- `tests.rs`: unit tests (close paths, quick-download cancel, ribbon geometry, overwrite check).

## Contracts and invariants
- The GUI thread only polls: decode, I/O, downloads, processing and saving run in the parent
  module's controllers; methods here start them and apply their events.
- Every new type of the window goes into `mod.rs`; children hold methods and free helpers only.
- cfg twins stay together in one file: the macOS bounds helper and its test module in
  `capture.rs`, the wasm/native `check_test_chapter_site_availability` in `jobs.rs`.
- `feature = "tutorial"` code lives in `mod.rs` (hooks) and inline in a few section bodies;
  check the crate with and without `--features tutorial`.
- Ribbon and crop-editor images are painted only through `ribbon::paint_ribbon_tiles`.
- The batch child window has one close path, `close_batch_window` (`widgets.rs`).

## Editing map
- New window state or a new mode enum: `mod.rs`.
- Simple-mode wizard steps or the left column: `left_panel.rs`.
- Ribbon scaling, container width, page controls, manual cut: `ribbon_view.rs`
  (`show_ribbon`, `apply_ribbon_resize`, the geometry helpers).
- Crop editor: `crop_editor.rs`.
- Clipboard / screen capture: `capture.rs`.
- Advanced downloader or auto review: `advanced_ui.rs`.
- Stitch / waifu2x / Reline controls: `processing_ui.rs`; their option parsing: `options.rs`;
  starting and polling the runs: `jobs.rs`.
- Save form or save flow: `save.rs`.
- Persisted window preferences: `prefs.rs`.
