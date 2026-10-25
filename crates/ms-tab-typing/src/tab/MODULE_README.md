# Module: crates/ms-tab-typing/src/tab

## Purpose
The behaviour half of the «Текст» (typing) tab: every method group and free-function slab of
`TypingTabState` / `TypingTextOverlayLayer`. The data model, the public facade and the
`CanvasHooks` impl live in the module root `../tab.rs`; this directory holds what those types DO.
The tab's architecture, terminology, input set and its contracts (deferred-save policy, one move
primitive, composite order, residency, export pipelines) are owned by the crate readme
`../MODULE_README.md`; this file is the map of the directory only.

## Architecture
```text
tab.rs (structs/enums, TypingTabState facade, impl CanvasHooks, mod wiring)
  -> input.rs          declarations + per-frame dispatch (runs before canvas.draw)
  -> draw_page.rs      per-page draw + interaction  -> move_layer.rs / touch_layers.rs /
                       selection_rasters.rs / vector_transform.rs / layout_editor.rs
  -> doc_layers.rs + page_residency.rs   LayerDoc projection, band-Z, page loads
  -> render_jobs.rs -> render_store.rs (worker side)   create / edit / raster renders
  -> persist.rs        deferred text saves into LayerDoc / staging
  -> export.rs, flatten_to_file.rs      PNG / PSD / PDF export, image-set page flatten
  -> studio_tab.rs     ms_studio_tab::StudioTab answers
```
Every file is a DESCENDANT of module `tab`: it reads the model's private fields directly and
pulls the root's imports through `use super::*;`. Items are `pub(super)` (or `pub(crate)` when a
typing-level sibling such as `panel.rs` / `psd_export.rs` calls them).

## Files and submodules
- `input.rs`: the tab's `ms-input` declarations (ids are persistence keys), contexts, binding
  labels and THE dispatch of fired commands and axes.
- `studio_tab.rs`: `impl StudioTab for TypingTabState`, exit-only `user_config` keys, bottom hint.
- `draw_page.rs`: `draw_page_overlays` (one page's overlays and rasters), the pure
  `merged_fill_plan`, centering assist. Structural changes requested during the draw are applied
  as its last statement.
- `move_layer.rs`: THE layer-move session (pointer drag, arrow nudge, finger drag).
- `touch_layers.rs`: finger tap / drag on text layers, from typed canvas touch output only.
- `selection_rasters.rs`: selection switching, removal, duplication, wheel / key transforms,
  raster canvas interaction and its context menu.
- `vector_transform.rs`: vector deform mode of text overlays (baked into `raster_transform`).
- `layout_editor.rs`: vector-lines layout editor helpers and its stored-params codec.
- `panels.rs`: dock tab bodies (deformation, layers list, layout editor) and the editor lifecycle.
- `create_upload.rs`: create rectangle, on-canvas text editor, status toasts, texture uploads.
- `doc_layers.rs`: `LayerDoc` sync, band-Z, composite items shared by canvas and export, the
  whole-chapter page preloader.
- `page_residency.rs`: one page's layer load, split into a disk half and a GUI-thread apply half.
- `render_jobs.rs`: GUI-side job bookkeeping (create / edit / raster effects / shape variants,
  loader and migration workers); `render_store.rs`: their worker-side render bodies.
- `persist.rs`: deferred text placement saves, staging flushes, `flush_text_layers`.
- `codec.rs`: overlay JSON <-> typed render params, legacy normalizers, schema-1 -> 2 upgrade.
- `export.rs`: export routes (one-to-one / collecting), page flatten, export job polling.
- `flatten_to_file.rs` (native only): flatten ONE page to a file for image-set Save / Save As.
- `autotype.rs`: auto-typing (bubble detection -> alignment) and its debug overlay.
- `helpers.rs`: selection-to-page resolution, seed text, overlay runtime builders, legacy file IO.
- `geometry.rs` / `mesh_geometry.rs`: pure coordinate, hit-test and deform-mesh math (no state).
- `tests.rs`, `composite_characterization_tests.rs`: unit and characterization tests (test-only).

## Contracts and invariants
- The contracts named in Purpose are stated once, in `../MODULE_README.md` ("Contracts and
  invariants", "Input"); do not restate them here.
- No file here decides composite order or visibility: `ms_models::layer_model::ordering` does;
  `doc_layers.rs` is the one mapping onto it, shared by the canvas and the export flatten.
- The GUI thread does no file I/O beyond the residency / loader-start paths: decode, render,
  export and flatten run on workers (`render_store.rs`, `export.rs`, `flatten_to_file.rs`).
- Input is read only through the frame's `InputFrame` (`input.rs` and the gesture sites); raw
  key reads are forbidden by the crate's `clippy.toml`.

## Editing map
- New command, mode or gesture: `input.rs` (declaration + dispatch), then the widget site.
- Moving a layer in any way: `move_layer.rs` only.
- Layer order / visibility on canvas or in export: `ms_models::layer_model::ordering`, then the
  item mapping in `doc_layers.rs`.
- When an edit is written to disk: `persist.rs` (deferred-save policy in `../MODULE_README.md`).
- A new stored text parameter: `codec.rs` plus the schema owner `../panel/text_params_schema.rs`.
- Export format or route: `export.rs` (+ `../psd_export.rs`, `../pdf_export.rs`).
- A new per-tab studio hook: `studio_tab.rs`.
