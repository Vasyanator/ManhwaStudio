# Module: crates/ms-onnx/src/paddle_ocr

## Purpose
Native PaddleOCR (PP-OCRv5) text **detection** and **recognition** over ONNX
Runtime, in pure Rust — no OpenCV, no Clipper, no C++. Faithful port of the Python
reference `modules/ai_backend/engines/paddle_onnx.py` (CTC decode,
pre/post-processing). The DB postprocess and the glyph mask are the engine-neutral
detection domain and live in `ms-text-detect` (`db`, `glyph_mask`); this module
runs the forward pass and calls them. The crate resolves nothing: callers pass
model + dict paths and a committed `OrtRuntime`.

## Architecture
Two independent stages plus a composing engine:

```
image ─► PaddleDetector.detect ─► PaddleDetection { quads, blocks, glyph_mask }
                                        │
                    sort_quad_indices (reading order)
                                        │
                    rotate_crop per quad (perspective warp)
                                        │
image ─► PaddleOcrEngine.recognize ─► PaddleRecognizer.recognize_crops ─► Vec<String>
```

- Forward-only detection (the tiled text-detector pipeline):
  `PaddleDetector::forward_prob_maps(&[RgbImage])` → `Vec<ms_text_detect::ProbMap>`.
  Tiles arrive already scaled and padded by the `ms-text-detect` plan; this module
  only validates them (equal size, sides multiple of `preprocess::DET_TILE_ALIGN` = 32),
  ImageNet-normalizes them with the SAME rule as `detect`, runs them as one batch
  (the model's batch axis is dynamic) and quantizes each `[1, H, W]` plane to `u8`.
  Plan, stitch and the DB postprocess live in `ms-text-detect`.
- Whole-image detection (`detect`, used by OCR): preprocess → session → DB probability map →
  `ms_text_detect::db::boxes_from_bitmap` (binarize → contours → min-area-rect →
  box-score gate → unclip → rescale) → `block_from_quad` per quad →
  `ms_text_detect::glyph_mask::build_glyph_mask`.
- Recognition: crops grouped by dynamic width → batched session run →
  softmax-if-needed → CTC greedy decode → per-crop `(text, confidence)`.

Sessions are built via `OrtRuntime::build_session`, so the committed execution
provider (whichever EP was committed, CPU included) is applied uniformly. Model input/output names
are discovered positionally (input[0]/output[0]).

## Files and submodules
- `mod.rs`: public API (`PaddleDetector`, `PaddleRecognizer`, `PaddleOcrEngine`,
  `PaddleDetection`, `PaddleLine`, and `Quad` re-exported from `ms-text-detect`),
  the free `paddle_recognize` pipeline function, session I/O, and the
  crate-internal numeric conversion helpers (`u32_to_f32`, `nonneg_f32_to_u32`).
- `preprocess.rs`: detection resize/normalize (ImageNet, stride-32, ≤960), the
  forward-only tile batch (`preprocess_det_tiles`, no resize) and
  recognition resize/normalize/pad (H=48, dynamic width [320, 3200]).
- `crop.rs`: `rotate_crop` (perspective warp), `sort_quad_indices`, crop dims.
- `ctc.rs`: `needs_softmax`, `softmax_rows`, `decode_greedy`.
- `dict.rs`: `CharacterTable` construction (`["blank"] + lines + (space?)`).

## Contracts and invariants
- **Pure crate.** No OpenCV/Clipper/egui/config/download. The perspective warp is
  `imageproc`; contours, min-area-rect and the glyph mask are in `ms-text-detect`.
- **One owner for the detection domain.** DB constants, unclip, box score, the
  glyph mask and `Quad` live only in `ms-text-detect`; never re-implement them here.
- **Parity, not bit-equality.** `imageproc` contours / min-area-rect / bicubic warp
  and the Triangle (bilinear) resize kernel are close but not bit-identical to
  cv2; these feed robust CNNs. Unit tests use synthetic inputs / tolerances and
  never fabricate expected values. The `#[ignore]` e2e test needs real artifacts.
- **Character table:** index 0 = CTC blank; dict lines follow; a trailing space is
  appended only if absent. `num_classes == dict_lines + 2` (no pre-existing space).
- **No panics on bad input.** Empty images, degenerate quads, and shape mismatches
  return typed `OrtError` (`ImagePreprocess`/`TensorShape`/`Inference`/`PaddleDictLoad`).
- **One normalization rule.** `detect` and `forward_prob_maps` share
  `preprocess::normalize_det_channel`; never inline a second copy.
- **Forward-only quantization** is `floor(clamp(p, 0, 1) * 255 + 0.5)` in `f32`,
  operation for operation the rule of the Python backend's
  `modules/ai_backend/detection/forward_maps.py::quantize_probability_maps`, so the
  native and backend routes give the stitcher identical bytes. A NaN / infinite
  output is `OrtError::TensorShape`, never a zero map. Change both sides together.
- **Known gap (KG-026 in `dev-docs/known_gaps.md`): OCR detection is still a single
  960-px pass.** `paddle_recognize` calls `detect`, which shrinks the longest side to
  960 px (an 800x12000 strip reaches the model at ~64x960), so native PaddleOCR OCR
  on a tall page misses small text that the tiled text-detector pipeline finds.
- **`&mut self` on run paths** because `ort::session::Session::run` requires it.
- **Single pipeline source of truth.** The end-to-end detect→crop→recognize
  pipeline lives in the free `paddle_recognize(&mut PaddleDetector,
  &mut PaddleRecognizer, &RgbaImage)`. `PaddleOcrEngine::recognize` delegates to it;
  callers that own a detector separately can share ONE `PaddleDetector` across many
  `PaddleRecognizer`s by calling the free function directly.

## Editing map
- To change the forward-only tile contract or quantization, edit
  `PaddleDetector::forward_prob_maps` / `quantize_prob_maps` in `mod.rs` and
  `preprocess_det_tiles` (and keep the Python `forward_maps.py` rule in step).
- To change detection input sizing, edit `preprocess.rs`; DB numbers/geometry live
  in `ms-text-detect/src/db.rs`.
- To change recognition decode/normalization, edit `ctc.rs` / `preprocess.rs`.
- To change the detector glyph mask, edit `ms-text-detect/src/glyph_mask.rs`.
- To change the public surface or session wiring, edit `mod.rs` (and re-export from
  `../lib.rs`). Execution-provider selection lives in `OrtRuntime::build_session`.
