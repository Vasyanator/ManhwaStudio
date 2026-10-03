# Module: crates/ms-onnx/src (crate `ms-onnx`)

## Purpose
Native ONNX Runtime inference over the `ort` bindings in load-dynamic mode: the onnxruntime
shared library is resolved at RUNTIME from a caller-supplied path, never linked or downloaded at
build time. It runs the model forward passes of the native OCR / detection engines (MangaOCR,
PaddleOCR) and their engine-specific pre/post-processing.

## Architecture
Level 1 (see `ARCHITECTURE.md`). Depends on `ort`, `image`, `imageproc`, `ms-log` (trace log)
and `ms-text-detect` (engine-neutral detection domain). Callers: `ms-native-runtime` (process-
global session manager), `ms-onnx-runtime` (library resolver), `ms-tab-translation`,
`ms-settings-ui`.

- `OrtRuntime` (`lib.rs`) is the app-side handle of the committed, dylib-backed ort environment;
  every session is built through `OrtRuntime::build_session` so the committed execution provider
  and device are applied uniformly.
- Engines own their sessions and take `&mut self` on run paths (`Session::run` requires it).
- The detection DOMAIN (DB postprocess, glyph mask, `Quad`, `ProbMap`) is NOT here: `paddle_ocr`
  runs the forward pass and calls `ms_text_detect::{db, glyph_mask}` on the probability map
  (`detect`), or returns quantized `ProbMap`s for prepared tiles (`forward_prob_maps`) so that
  `ms-text-detect` stitches and postprocesses them.

## Files and submodules
- `lib.rs`: crate root; `ExecutionProvider`, `NativeDeviceSelection`, `OrtError`, `OrtRuntime`
  (load, warmup, session build); re-exports the engine types.
- `manga_ocr/`: native MangaOCR encoder + decoder (beam search, tokenizer, postprocess). See its
  `MODULE_README.md`.
- `paddle_ocr/`: native PaddleOCR detector + recognizer (preprocess, crop, CTC, dict). See its
  `MODULE_README.md`.

## Contracts and invariants
- Pure inference: no egui, no application config, no paths, no downloads. The caller supplies
  the dylib path, provider, and model / dictionary paths.
- Native-only: `ort` loads a native shared library, so this crate is never built for wasm32.
  Shared detection rules that must also build on wasm belong in `ms-text-detect`.
- No panics on bad input: shape and preprocessing failures return typed `OrtError` variants.
- Execution-provider ids (`ExecutionProvider::id`) are persisted by higher layers and must stay
  stable.

## Editing map
- To change provider / device selection or session options, edit `lib.rs`
  (`OrtRuntime::build_session`).
- To change MangaOCR inference, see `manga_ocr/`; PaddleOCR inference, see `paddle_ocr/`.
- To change DB box extraction or the Paddle glyph mask, edit `ms-text-detect` (`db.rs`,
  `glyph_mask.rs`), not this crate.
