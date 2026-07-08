# Module: crates/ms-onnx/src/paddle_ocr

## Purpose
Native PaddleOCR (PP-OCRv5) text **detection** and **recognition** over ONNX
Runtime, in pure Rust — no OpenCV, no Clipper, no C++. Faithful port of the Python
reference `modules/ai_backend/paddle_onnx_runtime.py` (DBPostProcess, CTC decode,
pre/post-processing) and `modules/ai_backend/paddle_text_detector_service.py`
(glyph mask). The crate resolves nothing: callers pass model + dict paths and a
committed `OrtRuntime`.

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

- Detection: preprocess → session → DB probability map → `boxes_from_bitmap`
  (binarize → contours → min-area-rect → box-score gate → unclip → rescale).
- Recognition: crops grouped by dynamic width → batched session run →
  softmax-if-needed → CTC greedy decode → per-crop `(text, confidence)`.

Sessions are built via `OrtRuntime::build_session`, so the committed execution
provider (CPU/DirectML/CoreML/CUDA) is applied uniformly. Model input/output names
are discovered positionally (input[0]/output[0]).

## Files and submodules
- `mod.rs`: public API (`PaddleDetector`, `PaddleRecognizer`, `PaddleOcrEngine`,
  `PaddleDetection`, `PaddleLine`, `Quad`), the free `paddle_recognize` pipeline
  function, session I/O, and the crate-internal numeric conversion helpers
  (`u32_to_f32`, `nonneg_f32_to_u32`, `f32_to_i32_trunc`).
- `preprocess.rs`: detection resize/normalize (ImageNet, stride-32, ≤960) and
  recognition resize/normalize/pad (H=48, dynamic width [320, 3200]).
- `db_postprocess.rs`: DB probability-map → text quads; `box_score`, `unclip_quad`.
- `crop.rs`: `rotate_crop` (perspective warp), `sort_quad_indices`, crop dims.
- `ctc.rs`: `needs_softmax`, `softmax_rows`, `decode_greedy`.
- `dict.rs`: `CharacterTable` construction (`["blank"] + lines + (space?)`).
- `glyph_mask.rs`: self-contained Otsu + 3x3-cross morphology + per-ROI text mask.

## Contracts and invariants
- **Pure crate.** No OpenCV/Clipper/egui/config/download. Everything geometric is
  via `imageproc` (contours, min-area-rect, perspective warp) or local code.
- **Parity, not bit-equality.** `imageproc` contours / min-area-rect / bicubic warp
  and the Triangle (bilinear) resize kernel are close but not bit-identical to
  cv2; these feed robust CNNs. Unit tests use synthetic inputs / tolerances and
  never fabricate expected values. The `#[ignore]` e2e test needs real artifacts.
- **Unclip = rotated-rect expansion.** Instead of pyclipper, each quad corner is
  moved outward along the rectangle's two edge normals by
  `distance = area * 2.0 / perimeter` (growing width/height each by `2*distance`) —
  equivalent to pyclipper-offset-then-refit for a convex quad.
- **DB constants** (from the Python config): thresh 0.3, box_thresh 0.6,
  unclip_ratio 2.0, max_candidates 1000, min_size 3 (→ 5 after unclip).
- **Character table:** index 0 = CTC blank; dict lines follow; a trailing space is
  appended only if absent. `num_classes == dict_lines + 2` (no pre-existing space).
- **No panics on bad input.** Empty images, degenerate quads, and shape mismatches
  return typed `OrtError` (`ImagePreprocess`/`TensorShape`/`Inference`/`PaddleDictLoad`).
- **`&mut self` on run paths** because `ort::session::Session::run` requires it.
- **Single pipeline source of truth.** The end-to-end detect→crop→recognize
  pipeline lives in the free `paddle_recognize(&mut PaddleDetector,
  &mut PaddleRecognizer, &RgbaImage)`. `PaddleOcrEngine::recognize` delegates to it;
  callers that own a detector separately can share ONE `PaddleDetector` across many
  `PaddleRecognizer`s by calling the free function directly.

## Editing map
- To change detection numbers/geometry, edit `db_postprocess.rs` (+`preprocess.rs`).
- To change recognition decode/normalization, edit `ctc.rs` / `preprocess.rs`.
- To change the detector glyph mask, edit `glyph_mask.rs`.
- To change the public surface or session wiring, edit `mod.rs` (and re-export from
  `../lib.rs`). Execution-provider selection lives in `OrtRuntime::build_session`.
