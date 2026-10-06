# Module: crates/ms-onnx/src (crate `ms-onnx`)

## Purpose
Native ONNX Runtime inference over the `ort` bindings in load-dynamic mode: the onnxruntime
shared library is resolved at RUNTIME from a caller-supplied path, never linked or downloaded at
build time. It runs the model forward passes of the native OCR / detection engines (MangaOCR,
PaddleOCR, Baberu OCR) and their engine-specific pre/post-processing.

## Architecture
Level 1 (see `ARCHITECTURE.md`). Depends on `ort`, `image`, `imageproc`, `serde_json` and
`unicode-general-category` (Baberu vocab), `ms-log` (trace log) and `ms-text-detect`
(engine-neutral detection domain). Callers: `ms-native-runtime` (process-
global session manager), `ms-onnx-runtime` (library resolver), `ms-tab-translation`,
`ms-settings-ui`.

- `OrtRuntime` (`lib.rs`) is the app-side handle of the committed, dylib-backed ort environment;
  every session is built through it: `build_session` applies the committed execution provider
  and device uniformly, `build_session_cpu_only` (same options, no EP) serves graphs that are
  CPU-oriented on every build (Baberu's int8 decoders). Both share one private builder.
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
- `baberu_ocr/`: native Baberu OCR (Pillow-exact preprocess, vision + KV-cached decoder,
  greedy decode with run caps); vision on the selected EP, decoders on CPU. See its
  `MODULE_README.md`.
- `paddle_ocr/`: native PaddleOCR detector + recognizer (preprocess, crop, CTC, dict). See its
  `MODULE_README.md`.

## Contracts and invariants
- Pure inference: no egui, no application config, no paths, no downloads. The caller supplies
  the dylib path, provider, and model / dictionary paths.
- Built in the wasm32 graph (through `ms-settings-ui`, which depends on it unconditionally),
  but the `ort` calls fail to compile there (STATE S-003: `ort::init_from`, `Session::run`,
  `commit_from_file`), so the crate cannot run on wasm. New code stays pure Rust without
  threads, so its only wasm errors are `ort` calls of that same S-003 class (the Baberu engine
  adds three `Session::run` errors in `baberu_ocr/mod.rs`). Shared detection rules that must also build on
  wasm belong in `ms-text-detect`.
- No panics on bad input: shape and preprocessing failures return typed `OrtError` variants.
- Execution-provider ids (`ExecutionProvider::id`) are persisted by higher layers and must stay
  stable.

## Editing map
- To change provider / device selection or session options, edit `lib.rs`
  (`OrtRuntime::build_session`).
- To change MangaOCR inference, see `manga_ocr/`; PaddleOCR inference, see `paddle_ocr/`;
  Baberu OCR, see `baberu_ocr/`.
- To change DB box extraction or the Paddle glyph mask, edit `ms-text-detect` (`db.rs`,
  `glyph_mask.rs`), not this crate.
