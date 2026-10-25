# Module: crates/ms-tab-translation/src/panels

## Purpose
Tool-panel UI modules for the Translation tab: each tool's panel is the body of its own dock tab.
Panels render controls and return typed actions or option changes to `TranslationTabState`.

## Architecture
Panels are UI boundaries, not worker owners. They keep editable option structs, lightweight
per-panel UI cache, and helper functions for formatting controls. The parent tab owns controller
lifecycles, canvas mutation, storage, backend health commands, settings persistence, and shared
model access.

The typical flow is:

```text
PanelDock::end (tool tab in front of a drawn, expanded panel)
    -> TranslationTabState::draw_tool_body(tool) -> draw_*_tool_body
    -> draw_*_panel(options, status snapshots, capabilities)
    -> panel action struct / changed option flag
    -> tab.rs dispatches controller, canvas, storage, or settings work
```

`bubbles.rs` is the exception with more local runtime state: it mirrors editable bubble card text
and footer fields, then flushes text changes back through `CanvasView` after a debounce.

## Files and submodules
- `mod.rs`: panel module declarations.
- `mod.rs`: module declarations and `section_header_button`, the one "▶ / ▼ title" header
  button of the hand-rolled collapsible sections (MT accordion, OCR AI API connection section).
- `ocr.rs`: OCR engine/language/model controls, the AI API engine options (a collapsible
  "service, key and instruction" section — `OcrPanelOptions::ai_api_connection_open`, collapsed by
  default and not persisted, like MT's — holding the shared `ms_ai_api::draw_connection` widget,
  id salt `translation_ocr_ai_api`, inside this panel's own height-capped `ScrollArea` and 300 px
  width), the shared `ms_ai_api::draw_generation_status` widget right before the "Статус:" block
  while an AI API request is in flight (its Stop -> `OcrPanelActions::stop_generation`, carrying the clicked snapshot so the tab stops
  exactly that generation), behavior
  toggles, load action, selection-mode hints (`OcrSelectionHints`: the bindings of the canvas OCR
  gestures, finger alternatives included, from the input registry), and last result/error preview. The "Заменять
  символы" master toggle expands an inline editor of post-OCR substitution rules (per-row enable,
  quoted comma-separated targets, replacement, delete); `runtime_char_replacements` parses the
  enabled rows into `CharReplacementRule`s carried by `OcrRecognizeRequest`, and the OCR worker
  applies them to the recognized result. The "Исправление КАПСЛОКА" toggle
  (`fix_caps_lock`, ON by default) sits with the other behavior toggles and asks the OCR
  worker to lower an entirely uppercase Latin/Cyrillic result to sentence case
  (`translation::ocr_case_fix`); it carries no UI logic of its own. Wider engines
  (PaddleOCR-VL) live on a second engine row to keep the tool panel from widening; PaddleOCR-VL
  shows no language controls, only its weight-variant combo (below) and an optional
  writing-system restriction: auto / korean / chinese / japanese. The six runtime engine-selection buttons are `AiButton`s gated on a
  per-engine `AiRequirement` (`engine_button_requirement`, permissive on an unknown capability so a
  not-yet-probed native ONNX runtime does not lock selection out); each shows a runtime marker badge
  (`engine_marker`: Torch / ONNX / Torch/ONNX). NeuroLate (cloud, paid per block) is a plain
  ungated selectable like AiApi; its body is the tab's ONE NeuroLate settings block
  (`crate::neurolate::draw_neurolate_settings` WITH the "create mask and text blocks" checkbox,
  salt `translation.ocr.neurolate`) plus the key + limit panel, both lent through
  `OcrPanelNeurolate` (settings edits fold into `options_changed`, key buttons come back as
  `OcrPanelActions::neurolate_key`); while a request of the selected NeuroLate engine is in flight
  a spinner, "Waiting for NeuroLate…" and a Stop (tooltip: the result is dropped, the request may
  still be charged) replace the generation widget and use the same `stop_generation`. AiApi is network-only and stays a plain ungated
  `selectable_value`: it works with every local AI runtime off (`--no-ai`, Android); `tab.rs` never
  disables the panel as a whole and names an unavailable local route's reason under it.
  Cascade: `selected_mode_requirement` (model-aware; MangaOCR `base_torch` needs Torch, its ONNX
  exports need onnxruntime) gates both the selected engine's options interface and the load button —
  when the requirement is known-unavailable, both are disabled and the requirement's
  `disabled_reason` is shown. The panel reads capabilities from the process-global `AiCaps::current`,
  not from parameters. `selected_mode_requirement` is `pub(crate)` and the single owner of "which
  runtime does the selected OCR mode need": the tab's Torch-unavailable status
  (`tab.rs::selected_ocr_mode_requires_torch`) only checks it for `Some(AiRequirement::Torch)`.
  External-model engines: Baberu OCR (second row, `AiRequirement::Onnx` / marker "ONNX", a
  "ja/zh/en only" hint and a warning when the MT source language is `ko`, lent as
  `OcrPanelModelView::korean_source`) and PaddleOCR-VL (weight-variant `WheelComboBox`
  `paddle_vl_model`, id salt `translation.ocr_panel.paddle_vl_model_label`, above the script
  restriction; the manga_ja choice shows its hint). EasyOCR (the model of the selected languages'
  script) and Surya (detection + recognition) have download blocks too. For all of them the
  download block is drawn outside the engine-options gate (`OcrPanelModelView::download`, the
  `OcrModelPanelSnapshot` of the bundle `ocr::external_model_specs(engine, variant, easy_langs)`
  taken at frame start; drawn only while its `bundle_key` matches the selection after this frame's
  widgets), its clicks return in `OcrPanelActions::model_download`, and the load button is enabled
  only when that state is `Installed` (`load_blocked_reason` gives the disabled hover: checking /
  downloading / not downloaded). An EasyOCR language list easyocr refuses shows its localized
  error in red and blocks the load button.
- `ocr_model_download.rs`: the shared external-model download block of one bundle (status line,
  the last cancel / failure / verify notice, Download / Resume button, "Verify files" once
  installed (`ModelDownloadAction::Verify`, the repair path for a same-size corruption), `ProgressBar` with file N of M and
  byte counters over the bundle or the automatic-retry countdown, Cancel; while ANOTHER bundle
  downloads, a line naming it with its progress over a disabled Download button). The pure
  `download_block_view` maps `OcrModelPanelSnapshot` -> what is shown; button captions come from
  `crate::ocr_model_download::offered_download_label` (shared with the "not downloaded" errors);
  `external_model_display_name` names every catalog spec. `pub` items (`draw_ocr_model_download`,
  `load_blocked_reason`, `ModelDownloadAction`): the OCR and detector panels here and the
  Cleaning tab's SDXL engine draw it. The detector panel draws it for
  `text_detector::SURYA_DETECTOR_SPECS` while Surya is selected
  (`TextDetectorPanelView::surya_model`, `TextDetectorPanelActions::surya_model_download`).
- `ocr_langs.rs`: static EasyOCR and PaddleOCR language catalogs used by the OCR panel.
  Each entry is `(wire_code, display_key)`: the wire code is the persisted identity
  sent to the backend, the display key is an i18n catalog key resolved to a localized
  label at render time via `lang_label`. Only the wire code is identity; labels localize.
- `text_detector.rs`: detector algorithm/options UI, status/progress display, run/OCR/save/clear
  actions, and line/mask edit mode toggles. `tab.rs` lends it a per-frame
  `TextDetectorPanelView` snapshot. It also owns `TextDetectorAlgorithm::availability`, the ONE
  "can this detector run" decision (the local-AI rule of `ms_local_ai::local_ai` for the algorithm's
  route — Paddle native or backend, CTD/Surya backend, NeuroLate cloud (always allowed) — then reported-missing PyTorch) shared by the panel's
  detect-button gate and hint and by `tab.rs`'s run-mode refusal; never re-derive it elsewhere.
  The hint under disabled detect buttons names the same reason a refused run reports.
  With NeuroLate selected it draws the shared NeuroLate settings block
  (`crate::neurolate::draw_neurolate_settings`, salt `translation.text_detector.neurolate`) and
  the key + limit panel over the tab's ONE store and key panel (`TextDetectorNeurolate`), and,
  the "recognize text right away" checkbox (`NeurolateSettings::detect_with_text`), and,
  instead of a plan notice, the line that the whole page goes to a paid service (worded for
  masks AND texts while that checkbox is on); settings edits
  come back as `neurolate_changed` (persisted with the OCR section) and the key buttons as
  `neurolate_key`.
  A batch's model download (`TextDetectorPanelView::model_download`) is drawn under the status
  with a Cancel (`TextDetectorPanelActions::cancel_detection`); otherwise, while a page batch
  runs (`TextDetectorPanelView::batch_running`), with a Stop
  (`TextDetectorPanelActions::stop_detection`, "after the current page").
- Both `ocr.rs` (engine load / recognize downloads, `OcrPanelModelView::engine_download`, Cancel
  -> `OcrPanelActions::cancel_model_download`) and `text_detector.rs` draw app-managed model
  downloads only through `crate::model_download_status`; never format that text here.
  `TextDetectorPanelOptions::run_mode` builds the run mode `tab.rs` starts (availability is
  checked separately). The per-page plan notice (full resolution / scaled with percent / tiled
  with tile count, grid and tile size / both, plus the effective CTD detection size) comes from
  `TextDetectorPlanNoticeCache`, which plans the CURRENT page with
  `run_mode().plan_inputs()` -> `ms_text_detect::plan_detection` — the same inputs the worker
  uses — and replans only when the engine, params or page size change. The tab owns the cache
  and lends it to `draw_text_detector_panel`, which queries it after the option widgets ran, so
  the frame of an option change already shows the new plan. The page size reaches the
  panel from `StudioApp::page_infos` through the host's `prepare_tool_frame`, which caches it on
  the tab state (`detector_page_size`: no notice while the page is loading, failed or 0x0). `plan_notice_text` / `plan_ctd_size_text` only
  format the notice.
- `machine_translation.rs`: tabbed MT UI with legacy provider/source/target controls and AI API
  controls: a "connection" accordion section (source/target languages, then the shared
  `ms_ai_api::draw_connection` widget, id salt `translation_mt_ai_api`) and the
  batching/context section, ImageBubble inclusion and image visual-detail controls (blocked only
  for a model `ms_ai_api::image_input_support` lists as text-only; `Unknown` models are allowed),
  the opt-in "strict JSON format" checkbox (`ai_strict_json`, explained by a `HelpHint`), plus
  start/cancel actions.
  The tab lends the run view as one `MtPanelRun` (busy, can-cancel, progress, generation
  snapshot, and the controller's `run_issues` slice, drawn on both tabs as a collapsed list
  `#id: reason` with a stable `id_salt`); on the AI API tab `ms_ai_api::draw_generation_status` sits under the start/cancel row,
  outside both sections, its Stop setting `MtPanelActions::cancel`, and the generic
  "Translating…" line is hidden while it shows (the counters stay); it stays shown in the gaps
  between two requests of a busy run, so it does not flicker between batches. On the AI API tab the start buttons also expose
  a right-click "Отобразить полный запрос" debug action (`MtPanelActions::preview_request_page` /
  `preview_request_all`) that asks `tab.rs` to assemble and display the first request without
  sending it.
  The machine tab's transport row (under the service combo: «Встроенные запросы» / «Через мой
  браузер» + a `HelpHint`) is drawn only when the tab lends an `MtTransportPanel` (the platform
  can host the extension); a click returns `MtPanelActions::transport` = (service, choice). In
  browser mode the extension block draws the lent `ExtensionPanelView` — the view types live in
  `tab/browser_extension.rs` (the one exception to "panels do not name the tab": the state is
  decided there, GUI-free) — and returns `install_extension` / `reveal_extension_folder`; the
  `chrome://extensions` copy button uses `Context::copy_text` directly. A run's wait line reads
  the browser-attention text for `MtRateLimitWaitReason::BrowserAttention`.
- `bubbles.rs`: searchable bubble cards, debounced original/translation text syncing, footer field
  editing, character filters, and card context actions (the card's `Ui` senses clicks behind its
  widgets; the menu opens on the `translation.bubble_card.menu` gesture of the input frame,
  `BubblesPanelContext::input`). The class selector converts a bubble
  between all three `BubbleClass` values; `BubbleCardBody::for_class` is the one exhaustive
  class→card-shape decision (single-line text vs translation+original, image controls, character
  controls, «Перевести» action), so a new class must state what its card shows instead of
  inheriting the text-bubble layout.
- `composition.rs`: composed text generation from project bubbles, plain/MiniJinja formatting
  options, and the TXT/DOCX export: a non-blocking `ms_platform` save dialog, then a worker that
  writes the whole file and commits the target (`write_export`); the export buttons stay disabled
  while one export is in flight. ImageBubbles are gated by the `include_image_bubbles`
  option: when enabled, each text area contributes one line `{translation}` (plus ` - {description}`
  when `use_character_names` is on and the description is non-empty); area 0 reads the legacy
  fields, later areas read `extra["text_areas"]`. The MiniJinja path simply includes/excludes image
  bubbles by the same option (their serialized `extra` already exposes `text_areas`).
  Hint bubbles are gated by `include_hint_bubbles` and formatted as
  `{hint_extra_sep}{hint_wrap}{line}{hint_wrap}{hint_extra_sep}` from `Bubble.text`, bypassing the
  source mode, `ignore_translated_lines`, the replica prefix and character names entirely.
  With `use_character_names` on, a replica (or a merged same-character group) carries its
  character name through ONE rule, `CharacterNameFormat::attach`: `Before` ->
  `{prefix}{name}{suffix}{text}`, `After` -> `{text}{prefix}{name}{suffix}`, nothing else added
  (the separator lives in the prefix / suffix, escapes decoded). The defaults (`After`, `" - "`,
  `""`) are the historical `"{line} - {name}"` and must stay byte-identical; they are mirrored in
  `ms_config::project_config_defaults` (guarded by a test). These options are plain-path only (the
  MiniJinja template sees raw bubble fields) and do not apply to an image bubble's description.
  The sub-options are drawn indented under the checkbox and only while it is on.
  The plain path runs in two passes: a formatting pass turning each bubble into a `ComposedItem`,
  then the pure `emit_composition_items` applying the limit, character merging and the hint
  attachment rule (below). Keeping emission pure is what makes those rules unit-testable.
  It also owns the ONE JSON mapping of `CompositionPanelOptions`
  (`write_composition_options_json` / `read_composition_options_json`, compared through
  `CompositionPanelOptions::stored_form`), used by `tab.rs` for the title section and by the
  presets.
- `composition_presets.rs`: app-wide named presets of every composition option (sorting
  included), drawn with `ms_widgets::PresetPicker` at the top of «Параметры». Two built-ins ship
  (`BuiltinCompositionPreset`: «Экспорт перевода» / «Перевод через ИИ»), identified by a stable
  `id()` with a localized `display_name()`; their values are `factory()`. Stored in `user_config`
  › `TranslarionTab.composition_presets` = `{version: 1, builtin: {<id>: {…}}, user: {<name>: {…}}}`
  (same keys as the title `composition` section; Python never reads it, listed in the v19
  `PROTOCOL_VERSION` note); a store without a numeric `version` is the earlier flat
  `{<name>: {…}}` map, read as user presets and moved under `user` by the first write; an unknown
  version is read-only. Seeded from the pre-read `user_settings`
  (`TranslationTabState::seed_composition_presets`; the store path is injected by the host's
  caller, `None` in every test); every create / save / rename / delete is its own
  read-modify-write on ONE ordered writer thread (`ms_config::preset_store::OrderedWriter`). An entry whose last write failed stays unsaved
  (and the write-error line stays) until a later write of that entry succeeds. A built-in edit is
  an override keyed by id (kept only while it differs from the factory values); deleting it
  restores the factory values, an unmodified built-in has no «Delete» and no built-in can be
  renamed. A user preset may share a built-in's display name (another language, or a copy made
  before the built-ins shipped) and is then listed next to it; creating or renaming one to a
  built-in's name in the current language is refused. The selection is per title
  (`composition.preset` in `settings.json`, `{builtin: <id>}` / `{user: <name>}`, a plain string
  read as a user name; written by the tab's settings saver); a missing user preset reads as «Нет».

## Contracts and invariants
- Panels must not start long-running work directly. They return typed actions for `tab.rs` to
  handle.
- Panels must not own AI backend health, controller workers, text detector storage jobs, or canvas
  state machines.
- Option structs are the settings boundary between panel UI, project settings persistence, and
  controller request construction. Keep parser/writer mappings in `tab.rs` synchronized when
  adding fields; composition is the exception — its mapping lives only in the `composition.rs`
  codec (exhaustive destructuring makes a new field fail to compile there), and a new
  composition option is preset material unless the preset value is split from the options. The AI API connection state (`ai_api: AiApiConnectionState`) persists only
  `service`, `base_url`, `model` and `system_instruction` (written by `tab.rs` as
  `OCR.params.ai_api.*` / `machine_translation.ai_api.*`; a missing `base_url` loads as empty); its key buffer is transient
  UI input stored only through `AiApiConnectionActions` -> `AiApiTaskRunner` in the OS credential
  store, never serialized into project settings. The connection id salts are persisted widget
  state: keep them stable.
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
- To add a new translation tool, add the module here, add a `TranslationTool` variant (tab id,
  extras flag key, captions, toggle hotkey) and its `draw_tool_body` arm in `tab/tool_panels.rs`,
  name its tab in `translation_default_dock_layout`, and define an option/action boundary.
- To change OCR UI fields or language lists, edit `ocr.rs` and `ocr_langs.rs`; update settings
  parsing and request construction in `tab.rs`.
- To change detector UI controls or edit-mode buttons, edit `text_detector.rs`; update
  `TextDetectorPanelOptions::run_mode`, the settings read/write in `tab.rs` and
  `../text_detector/` if semantics change.
- To change the plan notice wording, edit `plan_notice_text` and its
  `translation.text_detector_panel.plan_*` keys; to change what it reports, change the plan in
  `ms-text-detect` (never recompute scale or tiles in the panel).
- To change when a detector algorithm may run or the text explaining why not, edit
  `TextDetectorAlgorithm::availability` / `DetectorUnavailable` in `text_detector.rs` (its
  truth-table test pins the current rules).
- To change MT provider/language UI, edit `machine_translation.rs` and coordinate with
  `translation/machine_translation.rs`. The extension block's look: `machine_translation.rs`;
  its states and jobs: `../tab/browser_extension.rs`.
- To change bubble card editing or footer metadata UI, edit `bubbles.rs` and related footer sync
  code in `tab.rs`.
- To change prompt composition, MiniJinja variables, sort/merge rules, or export formats, edit
  `composition.rs`.
- To add a built-in composition preset, add a variant to `BuiltinCompositionPreset` in
  `composition_presets.rs` (its `id()` is a persistence key: never rename it; its name is an i18n
  key) and list it in `ALL`; preset storage and picker behaviour live in the same file.
