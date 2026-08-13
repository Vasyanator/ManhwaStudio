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
- `ocr.rs`: OCR engine/language/model controls, AI API provider/key/model controls, behavior
  toggles, load action, selection-mode hints, and last result/error preview. The "Заменять
  символы" master toggle expands an inline editor of post-OCR substitution rules (per-row enable,
  quoted comma-separated targets, replacement, delete); `runtime_char_replacements` parses the
  enabled rows into `CharReplacementRule`s carried by `OcrRecognizeRequest`, and the OCR worker
  applies them to the recognized result. The "Исправление КАПСЛОКА" toggle
  (`fix_caps_lock`, ON by default) sits with the other behavior toggles and asks the OCR
  worker to lower an entirely uppercase Latin/Cyrillic result to sentence case
  (`translation::ocr_case_fix`); it carries no UI logic of its own. Wider engines
  (PaddleOCR-VL) live on a second engine row to keep the side panel from widening; PaddleOCR-VL
  shows no language/model controls (only an optional writing-system restriction: auto / korean /
  chinese / japanese). The five runtime engine-selection buttons are `AiButton`s gated on a
  per-engine `AiRequirement` (`engine_button_requirement`, permissive on an unknown capability so a
  not-yet-probed native ONNX runtime does not lock selection out); each shows a runtime marker badge
  (`engine_marker`: Torch / ONNX / Torch/ONNX). AiApi is network-only and stays a plain ungated
  `selectable_value` (still disabled under `--no-ai` by the outer `add_enabled_ui` in `tab.rs`).
  Cascade: `selected_mode_requirement` (model-aware; MangaOCR `base_torch` needs Torch, its ONNX
  exports need onnxruntime) gates both the selected engine's options interface and the load button —
  when the requirement is known-unavailable, both are disabled and the requirement's
  `disabled_reason` is shown. The panel reads capabilities from the process-global `AiCaps::current`,
  not from parameters.
- `ocr_langs.rs`: static EasyOCR and PaddleOCR language catalogs used by the OCR panel.
  Each entry is `(wire_code, display_key)`: the wire code is the persisted identity
  sent to the backend, the display key is an i18n catalog key resolved to a localized
  label at render time via `lang_label`. Only the wire code is identity; labels localize.
- `text_detector.rs`: detector algorithm/options UI, status/progress display, run/OCR/save/clear
  actions, and line/mask edit mode toggles.
- `machine_translation.rs`: tabbed MT UI with legacy provider/source/target controls and AI API
  provider/key/model/prompt/batching/context controls, multimodal ImageBubble inclusion and image
  visual-detail controls, plus start/cancel actions. On the AI API tab the start buttons also expose
  a right-click "Отобразить полный запрос" debug action (`MtPanelActions::preview_request_page` /
  `preview_request_all`) that asks `tab.rs` to assemble and display the first request without
  sending it.
- `bubbles.rs`: searchable bubble cards, debounced original/translation text syncing, footer field
  editing, character filters, and card context actions. The class selector converts a bubble
  between all three `BubbleClass` values; `BubbleCardBody::for_class` is the one exhaustive
  class→card-shape decision (single-line text vs translation+original, image controls, character
  controls, «Перевести» action), so a new class must state what its card shows instead of
  inheriting the text-bubble layout.
- `composition.rs`: composed text generation from project bubbles, plain/MiniJinja formatting
  options, and TXT/DOCX export helpers. ImageBubbles are gated by the `include_image_bubbles`
  option: when enabled, each text area contributes one line `{translation}` (plus ` - {description}`
  when `use_character_names` is on and the description is non-empty); area 0 reads the legacy
  fields, later areas read `extra["text_areas"]`. The MiniJinja path simply includes/excludes image
  bubbles by the same option (their serialized `extra` already exposes `text_areas`).
  Hint bubbles are gated by `include_hint_bubbles` and formatted as
  `{hint_extra_sep}{hint_wrap}{line}{hint_wrap}{hint_extra_sep}` from `Bubble.text`, bypassing the
  source mode, `ignore_translated_lines`, the replica prefix and character names entirely.
  The plain path runs in two passes: a formatting pass turning each bubble into a `ComposedItem`,
  then the pure `emit_composition_items` applying the limit, character merging and the hint
  attachment rule (below). Keeping emission pure is what makes those rules unit-testable.

## Contracts and invariants
- Panels must not start long-running work directly. They return typed actions for `tab.rs` to
  handle.
- Panels must not own AI backend health, controller workers, text detector storage jobs, or canvas
  state machines.
- Option structs are the settings boundary between panel UI, project settings persistence, and
  controller request construction. Keep parser/writer mappings in `tab.rs` synchronized when
  adding fields. API keys edited in the OCR panel are transient UI input and must be saved only via
  controller actions to the OS credential store, not serialized into project settings.
- `bubbles.rs` must write bubble text through `CanvasView` APIs and footer fields through the
  parent tab patch queue; it must not mutate `ProjectData` directly.
- A class switch in `bubbles.rs` seeds the target class's own `extra` keys through the footer patch
  queue, because `CanvasView::set_bubble_class_for_bid` only swaps the class (and pins the aside
  display type). `Image` seeds `image_source_type`/`description`; `Hint` seeds
  `hint_show_outside_translation` from `canvas.state.hint_show_outside_default`, the same
  user-level default the `H` creation path (`promote_bubble_to_hint`) uses, so a hint made here and
  a hint made on the canvas start identically. A hint card shows ONE line (`Bubble.text`,
  `original_text` stays empty), keeps the replica-order spin box, and offers neither the character
  controls nor «Перевести» — a hint is an author note, never sent to a translator.
- Hint attachment in `composition.rs` is a contract, not a formatting detail. It rests on the
  **barrier rule**: an ordinary text bubble that emits nothing — filtered in pass 1 as
  already-translated or source-less, or normalizing to an empty line — is NOT discarded. It stays
  in reading order as a `ComposedItem::DroppedReplica`, which emits nothing and never disturbs a
  merge group, but stops a preceding hint's lookahead. Image bubbles and hints excluded by their
  own option leave no barrier: a hint legitimately binds across them. The "no replicas" early
  return is keyed on the count of genuinely emitting entries, not on the stream length.
  `classify_hint_bindings` then resolves each hint against the next replica-or-barrier:
  an emitting replica → the hint is forward-bound and emitted immediately before it; a barrier →
  the commented bubble is not inserted, so the hint is dropped and must never re-target the next
  surviving replica; neither → the hint is trailing.
  A forward-bound hint and its target replica are admitted to the character limit atomically, so a
  hint can never outlive the replica it comments on; when the limit stops composition the queued
  hints are dropped. The "the very first entry is always admitted" privilege belongs to that whole
  atomic bundle, not to its first element — otherwise a leading hint would take it and starve the
  replica it annotates. Trailing hints are never queued: they bind backward to the previous entry
  and are force-emitted at the very end past the limit, whether or not the loop stopped on the
  limit elsewhere. With `merge_same_character` on, a hint acts as a group boundary (flush, reset
  the character, emit, start a fresh group) and the atomic check covers only the target replica's
  own line, not the group it later joins. A hint is separated like any other entry — by the global
  `sep_between`; `hint_extra_sep` is extra padding inside the entry and counts toward the limit.
  Composition with no hints at all must stay byte-identical to the pre-hint composer; that property
  is what the emission tests around merging, image bubbles and the limit guard.
- Composition export may perform file writes from panel helpers because it is an explicit user
  action; errors must be returned and shown rather than ignored.
- Language catalogs in `ocr_langs.rs` are data only. Runtime model availability and downloads are
  handled outside this directory.
- UI strings localize through the `ms-i18n` `t!`/`tf!` macros; user-facing text must not be a
  Cyrillic literal. Display labels stored in `const` tables (`ocr_langs.rs` tuples,
  `MtLanguage.title` in `machine_translation.rs`) hold a catalog key, not the text, and resolve at
  render time (`t!` is not `const`). Wire codes/tokens sent to the backend stay literal identities.

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
