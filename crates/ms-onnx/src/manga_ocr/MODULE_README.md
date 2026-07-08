# Module: crates/ms-onnx/src/manga_ocr

## Purpose
Native MangaOCR inference engine: turns an input image into recognized,
post-processed Japanese text using ONNX Runtime. This is a faithful, token-for-token
port of the Python reference `_OnnxMangaOcrRuntime` in
`modules/ai_backend/manga_ocr_service.py` (the source of truth). The module is pure
inference: the caller supplies model/vocabulary paths and an already-committed
`OrtRuntime`; nothing here downloads, reads app config, or touches the GUI.

## Architecture
The public type is `MangaOcrEngine` (`mod.rs`), which owns two `ort` sessions
(encoder + decoder) and the WordPiece vocabulary. The pipeline is:

```
image → preprocess (NCHW f32) → encoder session → encoder_hidden_states
      → beam search (drives the decoder per step, NO KV cache)
      → WordPiece decode (id → text) → post_process → String
```

- The **encoder hidden dim is inferred at runtime** from the encoder output's last
  shape dim, so one code path serves both `base` (hidden=768) and `2025` (hidden=192).
- The **decoder has no KV cache**: every step feeds the entire `input_ids` so far
  plus the unchanged `encoder_hidden_states`; the next-token logits are
  `logits[0, last_position, :]`.
- Model input/output **names are discovered positionally** from the sessions, not
  hard-coded (encoder in[0]/out[0]; decoder in[0]=input_ids, in[1]=encoder_hidden_states,
  out[0]=logits).

## Files and submodules
- `mod.rs`: `MangaOcrEngine` (`load` / `recognize`), session building, name
  discovery, and the per-step decoder closure. Edit here for session/generation glue.
- `preprocess.rs`: image → `pixel_values` NCHW f32. PIL "L" luma (exact integer
  coefficients), sub-2×2 nearest upscale, unconditional 224×224 resize, rescale +
  normalize to [-1, 1].
- `tokenizer.rs`: `Vocab` — loads `vocab.txt` (id = line index) and decodes ids →
  text (skips specials 0..=4, strips leading `##`, joins with no separators).
- `beam.rs`: pure beam search parameterized by a "next-token logits" closure
  (`beam_search`), plus `log_softmax`, `no_repeat_ngram_banned_tokens`,
  `top_k_indices`, `normalized_score`. Reduces to greedy at `num_beams == 1`.
- `postprocess.rs`: `_post_process` port — whitespace strip, `…`→`...`, `[・.]{2,}`
  dot-run collapse, then `jaconv.h2z(ascii=True, digit=True)` (kana included). The
  h2z tables were extracted verbatim from the installed `jaconv` package.

## Contracts and invariants
- **Generation constants** (both exports identical): start=2, eos=3, max_length=300,
  num_beams=4, no_repeat_ngram_size=3, length_penalty=2.0, early_stopping=true.
- **No panics on bad input / no fake behavior.** Missing outputs, wrong shapes, and
  bad indices map to `OrtError::TensorShape`; run failures to `OrtError::Inference`;
  session-build failures to `OrtError::SessionBuild`; vocab problems to
  `OrtError::VocabLoad`; empty images to `OrtError::ImagePreprocess`.
- **`recognize` takes `&mut self`** because `ort::session::Session::run` requires
  `&mut self`; the engine owns its sessions.
- **CPU-only (Phase 1):** sessions register no execution provider (default CPU) and
  use `GraphOptimizationLevel::All` to match the reference's `ORT_ENABLE_ALL`.
- **Post-process order is load-bearing** and must match Python exactly.

## Parity caveats
- Resize kernel: PIL uses BILINEAR (`resample=2`); we use `image` crate
  `FilterType::Triangle` (closest bilinear). Sub-pixel rounding may differ slightly.
- Log-softmax sum accumulates in f64 vs NumPy's f32 pairwise reduction; this can only
  matter on exact score ties, which do not occur on real model outputs.

## Editing map
- To change preprocessing, see `preprocess.rs`.
- To change generation/beam behavior, see `beam.rs` (pure) and the closure in
  `mod.rs::run_generation`.
- To change decode or post-processing, see `tokenizer.rs` / `postprocess.rs`.
- Full end-to-end parity is validated by `tests/manga_ocr_e2e.rs` (`#[ignore]`,
  needs real models + an onnxruntime dylib).
