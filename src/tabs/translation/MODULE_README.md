# Module: src/tabs/translation

## Purpose
Translation tab runtime for OCR, text detection, machine translation, translation-focused bubble
editing, composed text export, and translation-specific canvas hooks.

## Architecture
`TranslationTabState` in `tab.rs` is the tab orchestrator and `CanvasHooks` implementation. It owns
panel state, controller instances, footer metadata caches, text-detection storage, OCR selection
state, and debounced settings persistence.

Long work is delegated to focused controllers:

- `ocr.rs` owns the OCR worker, backend HTTP transport, page crop/cache handling, and per-engine
  load state.
- `text_detector.rs` owns the text detector worker and returns page boxes plus editable binary
  masks.
- `machine_translation.rs` owns MT run threads, cancellation, and stale-event filtering.
- `backend_health.rs` owns the shared Python backend probe snapshot used by Translation and
  Settings.

UI panels under `panels/` expose typed options/actions and avoid owning worker lifecycles. The tab
converts panel actions into controller requests, canvas changes, persistent settings, or storage
jobs.

Text-detector mask GPU textures are a reconstructable display cache. The tab exposes memory
snapshots and coarse eviction methods for those textures, while detector result masks and stored
text-mask data remain intact for redraw, editing, and disk persistence.

Python-backed OCR/detector routes call the backend service on `127.0.0.1` using the runtime port
published by `backend_health.rs`. The default port is `8765`, but the Settings backend process
manager may switch to a free port when the default is occupied and launch `ai_backend.py --port`.
App-managed model files must be resolved through `src/ai_models.rs` before backend initialization;
EasyOCR and Surya library caches remain backend/library-managed. PaddleOCR detector-only downloads
only detection files, while full PaddleOCR also downloads the selected recognition language.

## Files and submodules
- `mod.rs`: module wiring and public re-exports for `TranslationTabState`, hotkey IDs, and hotkey
  hint structs.
- `tab.rs`: main tab state, canvas hooks, panel routing, OCR region selection, advanced-recognition
  integration, footer metadata sync, text-detection mask/line editing, detector result storage,
  MT dispatch, and coalesced translation settings persistence.
- `ocr.rs`: `TranslationOcrController`, OCR load/recognize worker, persistent backend HTTP client,
  crop encoding, page image LRU cache, and model-download/load-state events.
- `text_detector.rs`: `TranslationTextDetectorController`, local classic detector, backend
  PaddleOCR/CTD/Surya detector calls, mask decoding/building, detector batch progress, and helper
  routes reused by cleaning tools for region masks.
- `machine_translation.rs`: `TranslationMtController`, `MtService`, batch item/request types,
  per-run worker thread lifecycle, cancellation, and backend dispatch.
- `machine_translators/`: UI-agnostic Google, Yandex, and DeepL MT provider implementations behind
  `MachineTranslatorBackend`.
- `panels/`: side-panel UI modules for OCR, bubble cards/footer fields, machine translation,
  text detector controls, composed text, and OCR language catalogs.
- `adv_rec.rs`: floating advanced-recognition crop preview/editor with async crop preparation,
  brush overlay, rotation, zoom, and local quick-selection OCR actions.
- `backend_health.rs`: backend `/health`, backend version snapshot, `/device`, ONNX provider,
  max-loaded-models, and
  diagnostics probe helpers.

## Contracts and invariants
- OCR, detector, MT, storage load/save, crop preparation, and backend health work must not block
  the GUI thread.
- Canvas integration must stay behind `CanvasHooks` or typed `CanvasView` APIs; do not duplicate
  canvas bubble/viewport state machines in translation code.
- Text detector page results carry source size, block rectangles in source pixels, mask size, and
  mask alpha bytes. Mask buffers must match `width * height` and invalid geometry must be rejected.
- Text-detector mask GPU eviction must drop only tiled `TextureHandle` pages and preserve detector
  result data, text-mask model data, and pending storage jobs.
- Persisted detector results live under `ProjectPaths::text_detection_dir`; storage jobs must load
  and save real block/mask data rather than synthesizing success.
- Footer metadata lives in `Bubble.extra` fields and is synchronized through debounced
  `CanvasView::patch_bubble_extra_fields` calls.
- App-managed OCR/detector model downloads must go through `src/ai_models.rs`. Missing models,
  backend failures, and unsupported engines must surface clear errors.
- MT worker events are accepted only for the active run id; cancelled/detached run output must not
  mutate canvas state.
- Shared model locks must be short-lived and released before image decoding, HTTP calls,
  composition/export, storage I/O, or callbacks.

## Editing map
- To change top-level translation UI routing, canvas overlays/hooks, OCR selection behavior,
  detector storage, footer sync, or settings persistence, edit `tab.rs`.
- To change OCR engine options, loading, recognition requests, backend endpoints, crop handling, or
  page image caching, edit `ocr.rs` and the OCR panel in `panels/ocr.rs`.
- To change text detector algorithms, masks, backend detector endpoints, or cleaning-tool detector
  helpers, edit `text_detector.rs` and `panels/text_detector.rs`.
- To change MT run lifecycle, provider selection, cancellation, or batch dispatch, edit
  `machine_translation.rs`, `machine_translators/`, and `panels/machine_translation.rs`.
- To change bubble list editing, footer fields, search/filter behavior, or text write-through, edit
  `panels/bubbles.rs` and footer sync helpers in `tab.rs`.
- To change composed text generation or export, edit `panels/composition.rs`.
- To change advanced manual OCR crop behavior, edit `adv_rec.rs`; keep OCR dispatch in `tab.rs`.
