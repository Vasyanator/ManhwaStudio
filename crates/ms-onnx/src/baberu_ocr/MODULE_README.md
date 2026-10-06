# Module: crates/ms-onnx/src/baberu_ocr

## Purpose
Native Baberu OCR inference (`genshiai-daichi/baberu-ocr`, Apache-2.0): one speech-bubble
crop (Japanese, Chinese or English; it does NOT read Korean or Russian) to text, without
Python. A faithful port of the upstream reference `onnx_infer.py`, which is the source of
truth for every rule below. Pure inference: the caller supplies the committed `OrtRuntime`
and the four file paths; nothing here downloads, reads app config, or knows the model
directory layout.

## Architecture
`BaberuOcrEngine` (`mod.rs`) owns three sessions and the vocabulary:

```
RgbaImage -> preprocess (drop alpha, Pillow BICUBIC to 224x224, ImageNet norm, CHW f32)
          -> vision_fp16.onnx         pixel_values -> vision_embeds [1, L=256, 512]
          -> decoder_prefill_int8     vision_embeds + BOS -> logits row + 12 presents
          -> greedy_decode (decode.rs) driving decoder_step_int8 per token:
             input_ids, position_ids = L + 1 + k, past_k0..5 / past_v0..5 = presents
          -> BaberuVocab::decode -> String (newlines kept)
```

- Graph inputs/outputs are addressed BY NAME (`pixel_values`, `vision_embeds`,
  `input_ids`, `position_ids`, `logits`, `present_{k,v}{0..5}` -> `past_{k,v}{0..5}`);
  `load` rejects a graph that lacks one (`OrtError::TensorShape`).
- The 12 presents are moved out of each run with `SessionOutputs::remove` (a
  reference-counted handle, no copy) and fed into the next step.
- **Session placement: vision on the selected EP, decoders on CPU** (why: int8 decoder
  ops fall back to CPU on GPU EPs; benchmark 2026-10-06). One rule, `graph_placement`:
  prefill and step always use `OrtRuntime::build_session_cpu_only` (their
  `DynamicQuantizeLinear` / `MatMulInteger` and KV-cache ops fall back node by node to
  the CPU on CUDA/TensorRT/DirectML/WebGPU and run 2-4x slower per token with the copies);
  vision uses `OrtRuntime::build_session_on` with the committed EP + device (on a GTX
  1660 Ti 216 ms CPU -> 84-98 ms CUDA/DirectML), except under TensorRT, where it uses the
  CUDA EP of the same build: no TensorRT engine cache is configured, so each session
  build would compile for minutes. `build_vision_session` retries a vision build that
  failed on its EP on the CPU; `BaberuOcrEngine::vision_placement`
  (`BaberuVisionPlacement`: `Selected` / `Substituted` / `CpuFallback`) reports the
  outcome and the caller logs it. Accelerator EPs register with
  `error_on_failure()`, so a session that built has its EP active; per-node fallback
  inside a registered EP is not observable through ort at `api-18`.
- Output is not EP-invariant on fragile crops (GPU vision differed from CPU on 2-5 of 83
  sweep crops; identical on the 4 committed fixtures). The decoders on CPU keep the
  decode reference-identical; the e2e parity test runs on the CPU provider.

## Files and submodules
- `mod.rs`: `BaberuOcrEngine` (`load` / `recognize` / `vision_placement`), the session
  placement rule (`graph_placement`, `build_vision_session`), graph I/O contract, KV glue.
- `preprocess.rs`: `preprocess`, `rgba_to_rgb`, `resize_bicubic_pillow` (integer port of
  Pillow `Resample.c`), `pixel_values`.
- `vocab.rs`: `BaberuVocab` (`from_json`, `is_content`, `decode`, `vocab_size`).
- `decode.rs`: `GreedyConfig` (`BABERU` = published defaults) and the pure
  `greedy_decode` over a "next logits" closure.

## Contracts and invariants
- **Preprocess parity is bit-exact with Pillow 12** `convert("RGB").resize((224, 224),
  BICUBIC)`: f64 coefficients (`a = -0.5`, support `2 * max(scale, 1)`, normalized by
  their sum), 22-bit fixed point with C `(int)(k +- 0.5)` rounding, accumulator from
  `1 << 21`, u8 clamp between passes, a pass skipped when its size is unchanged.
  Horizontal pass first, EXCEPT Pillow's `Image.resize` special case: vertical first
  when `h > w * 100` and the height shrinks. The `image` crate's CatmullRom is NOT
  equivalent (up to 22/255 per pixel, and it flips characters); never substitute it.
- Normalization is f32 throughout: `(f32(px) / 255 - mean) / std`.
- **Decode** (`decode.rs` header has the exact steps): logits widened to f64; repetition
  penalty 1.2 over every seen id INCLUDING BOS (divide when `>= 0`, multiply when `< 0`);
  run cap 12 for a content token, 16 for any other id > 3 (logit -> -inf); argmax takes
  the FIRST maximum; EOS (2) stops; at most 256 tokens; the last token is never fed back.
- **Content** = Unicode general category L* or N* (`unicode-general-category`) minus
  `ーｰ〜~`, identical to Python's `unicodedata.category(ch)[0] in "LN"` on the real vocab.
  `char::is_alphanumeric` is a different set.
- Logits width must equal `vocab_size()` (vocab + 4 specials); ids 0..3 decode to nothing.
- No panics on bad input: empty image -> `BaberuPreprocess`, bad vocab ->
  `BaberuVocabLoad`, NaN / inconsistent logits -> `BaberuDecode`, graph failures ->
  `Inference { stage: "vision" | "decoder_prefill" | "decoder_step" }`.
- Pure Rust and no threads: adds nothing to the wasm32 build beyond the crate's existing
  `ort` failures (S-003).

## Tests and fixtures
- Unit tests next to each file: decoder on scripted fake logits (penalty incl. BOS and
  sign, both caps, special ids uncapped, EOS, the 256 limit, first-max tie, positions from
  `L + 1`, NaN/width errors), vocab parsing and content rule, resize goldens and edge sizes.
- `../../fixtures/baberu/` (generator: `tools/make_baberu_fixtures.py`, its docstring
  documents the formats and the xorshift32 PRNG the Rust test reimplements):
  `resize_cases.json` (Pillow BICUBIC sha256 goldens, always run),
  `noncontent_ids.json` and `e2e_cases.json` + `sample_*.png` (opt-in).
- Opt-in real weights: `MS_BABERU_MODEL_DIR` (both the vocab content-set test here and
  `tests/baberu_ocr_e2e.rs`) plus `MS_ORT_DYLIB` (e2e); each skips with one line when unset.

## Editing map
- Preprocessing or a Pillow-version change: `preprocess.rs`, then regenerate the fixtures.
- Decode settings or loop rules: `decode.rs` (keep the tests in step with `onnx_infer.py`).
- A new Baberu graph variant (e.g. `vision_int4.onnx`): the caller passes its path; check
  the I/O names in `mod.rs::load` and re-record `e2e_cases.json`.
- Which EP a graph runs on (e.g. real TensorRT for vision once an engine cache exists):
  `mod.rs::graph_placement` and its tests; keep the backend fallback
  (`modules/ai_backend/ocr/baberu.py`) on the same rule.
