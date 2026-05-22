# Module: src/tabs/translation/panels

## Purpose
Side-panel UI modules for the Translation tab. Panels render controls and return typed actions or
option changes to `TranslationTabState`.

## Architecture
Panels are UI boundaries, not worker owners. They keep editable option structs, lightweight
per-panel UI cache, and helper functions for formatting controls. The parent tab owns controller
lifecycles, canvas mutation, storage, backend health commands, settings persistence, and shared
model access.

The typical flow is:

```text
TranslationTabState::draw_active_panel
    -> draw_*_panel(options, status snapshots, capabilities)
    -> panel action struct / changed option flag
    -> tab.rs dispatches controller, canvas, storage, or settings work
```

`bubbles.rs` is the exception with more local runtime state: it mirrors editable bubble card text
and footer fields, then flushes text changes back through `CanvasView` after a debounce.

## Files and submodules
- `mod.rs`: panel module declarations.
- `ocr.rs`: OCR engine/language/model controls, behavior toggles, load action, selection-mode
  hints, and last result/error preview.
- `ocr_langs.rs`: static EasyOCR and PaddleOCR language catalogs used by the OCR panel.
- `text_detector.rs`: detector algorithm/options UI, status/progress display, run/OCR/save/clear
  actions, and line/mask edit mode toggles.
- `machine_translation.rs`: MT provider and source/target language controls plus start/cancel
  actions.
- `bubbles.rs`: searchable bubble cards, debounced original/translation text syncing, footer field
  editing, character filters, and card context actions.
- `composition.rs`: composed text generation from project bubbles, plain/MiniJinja formatting
  options, and TXT/DOCX export helpers.

## Contracts and invariants
- Panels must not start long-running work directly. They return typed actions for `tab.rs` to
  handle.
- Panels must not own AI backend health, controller workers, text detector storage jobs, or canvas
  state machines.
- Option structs are the settings boundary between panel UI, project settings persistence, and
  controller request construction. Keep parser/writer mappings in `tab.rs` synchronized when
  adding fields.
- `bubbles.rs` must write bubble text through `CanvasView` APIs and footer fields through the
  parent tab patch queue; it must not mutate `ProjectData` directly.
- Composition export may perform file writes from panel helpers because it is an explicit user
  action; errors must be returned and shown rather than ignored.
- Language catalogs in `ocr_langs.rs` are data only. Runtime model availability and downloads are
  handled outside this directory.

## Editing map
- To add a new translation side panel, add the module here, route it in `TranslationPanel` and
  `draw_active_panel` in `tab.rs`, and define an option/action boundary.
- To change OCR UI fields or language lists, edit `ocr.rs` and `ocr_langs.rs`; update settings
  parsing and request construction in `tab.rs`.
- To change detector UI controls or edit-mode buttons, edit `text_detector.rs`; update controller
  option conversion in `tab.rs` and `text_detector.rs` if semantics change.
- To change MT provider/language UI, edit `machine_translation.rs` and coordinate with
  `translation/machine_translation.rs`.
- To change bubble card editing or footer metadata UI, edit `bubbles.rs` and related footer sync
  code in `tab.rs`.
- To change prompt composition, MiniJinja variables, sort/merge rules, or export formats, edit
  `composition.rs`.
