/*
File: crates/ms-onnx/src/manga_ocr/mod.rs

Purpose:
Native MangaOCR inference engine over ONNX Runtime: owns the encoder and decoder
`ort` sessions plus the WordPiece vocabulary, and turns an input image into
recognized (post-processed) Japanese text. Faithful port of the Python reference
`_OnnxMangaOcrRuntime` in `modules/ai_backend/manga_ocr_service.py`.

Key structures:
- MangaOcrEngine : the public entry point (load + recognize).
- EncoderStates  : encoder output (hidden states) fed to the decoder each step.

Key functions:
- MangaOcrEngine::load      : build both sessions and load the vocabulary.
- MangaOcrEngine::recognize : image -> encoder -> beam search -> decode -> post-process.

Submodules:
- preprocess  : image -> NCHW f32 tensor data (ViTImageProcessor parity).
- tokenizer   : `vocab.txt` load + decode-only WordPiece.
- beam        : beam search / greedy generation (pure logic).
- postprocess : `_post_process` + `jaconv.h2z` parity.

Notes:
The encoder hidden dimension is inferred at runtime from the encoder output's last
shape dim, so a single code path serves both the `base` (hidden=768) and `2025`
(hidden=192) exports. The decoder runs WITHOUT a KV cache: every step feeds the
entire `input_ids` so far plus the unchanged `encoder_hidden_states`, and the
next-token logits are `logits[0, last_position, :]`.

`recognize` takes `&mut self` because `ort::session::Session::run` requires
`&mut self`. Model input/output names are discovered from the sessions rather than
hard-coded, so any valid export naming works.
*/

pub mod beam;
pub mod postprocess;
pub mod preprocess;
pub mod tokenizer;

use std::path::Path;

use ms_log::trace::cat;
use ort::session::Session;
use ort::value::{Shape, Tensor};

use crate::{OrtError, OrtRuntime};
use beam::BeamConfig;
use tokenizer::Vocab;

// MangaOCR generation contract. Both the `base` and `2025` `generation_config.json`
// exports carry identical values; they are fixed here as the crate's inference
// contract (see manga_ocr_service.py generation config usage).
/// Decoder start token id (`[CLS]`); seeds the generated sequence.
const DECODER_START_TOKEN_ID: i64 = 2;
/// End-of-sequence token id (`[SEP]`).
const EOS_TOKEN_ID: i64 = 3;
/// Maximum generated sequence length (including the start token).
const MAX_LENGTH: usize = 300;
/// Beam width.
const NUM_BEAMS: usize = 4;
/// No-repeat n-gram size.
const NO_REPEAT_NGRAM_SIZE: usize = 3;
/// Length-penalty exponent.
const LENGTH_PENALTY: f64 = 2.0;
/// Whether to stop once `>= num_beams` hypotheses complete.
const EARLY_STOPPING: bool = true;

/// Encoder hidden states passed to the decoder on every generation step.
///
/// `data` is the flat row-major tensor content and `shape` its dims
/// (`[1, enc_seq, hidden]`). The hidden dimension is never hard-coded: `shape` is
/// forwarded verbatim to the decoder, so the same code path serves both the `base`
/// (hidden=768) and `2025` (hidden=192) exports.
#[derive(Debug, Clone)]
struct EncoderStates {
    /// Flat encoder output tensor data (`encoder_hidden_states`).
    data: Vec<f32>,
    /// Tensor shape `[1, enc_seq, hidden]`.
    shape: Vec<i64>,
}

/// Native MangaOCR inference engine: encoder + decoder sessions and vocabulary.
///
/// Construct with [`MangaOcrEngine::load`], then call [`MangaOcrEngine::recognize`].
/// The engine owns its two `ort` sessions and runs entirely on the CPU execution
/// provider (Phase 1). It never downloads anything and never reads app config: the
/// caller supplies the model and vocabulary paths and a committed [`OrtRuntime`].
#[derive(Debug)]
pub struct MangaOcrEngine {
    /// Vision encoder session: `pixel_values` -> `last_hidden_state`.
    encoder_session: Session,
    /// Text decoder session: `input_ids` + `encoder_hidden_states` -> `logits`.
    decoder_session: Session,
    /// Decode-only WordPiece vocabulary.
    vocab: Vocab,
    /// Discovered encoder input name (index 0).
    encoder_input_name: String,
    /// Discovered encoder output name (index 0).
    encoder_output_name: String,
    /// Discovered decoder `input_ids` input name (index 0).
    decoder_input_ids_name: String,
    /// Discovered decoder `encoder_hidden_states` input name (index 1).
    decoder_encoder_states_name: String,
    /// Discovered decoder `logits` output name (index 0).
    decoder_output_name: String,
}

impl MangaOcrEngine {
    /// Loads the MangaOCR encoder/decoder sessions and vocabulary.
    ///
    /// `runtime` must be a committed [`OrtRuntime`]: taking it by reference enforces
    /// (at the type level) that the process-global ort environment was loaded before
    /// any session is built, so callers cannot construct an engine against an
    /// uninitialized runtime. Its provider is used for diagnostic logging.
    ///
    /// `encoder_path` / `decoder_path` are the two ONNX model files
    /// (`encoder_model.onnx` / `decoder_model.onnx`); `vocab_path` is the WordPiece
    /// `vocab.txt`. Sessions are built with all graph optimizations enabled, matching
    /// the Python reference (`ORT_ENABLE_ALL`).
    ///
    /// # Errors
    /// - [`OrtError::SessionBuild`] if either model file is missing/unreadable or a
    ///   session cannot be built.
    /// - [`OrtError::VocabLoad`] if the vocabulary cannot be read or is empty.
    /// - [`OrtError::TensorShape`] if a session exposes fewer inputs/outputs than the
    ///   MangaOCR contract requires.
    pub fn load(
        runtime: &OrtRuntime,
        encoder_path: &Path,
        decoder_path: &Path,
        vocab_path: &Path,
    ) -> Result<Self, OrtError> {
        ms_log::trace_log!(
            cat::STARTUP,
            "MangaOcrEngine load provider={} encoder={} decoder={} vocab={}",
            runtime.provider().id(),
            encoder_path.display(),
            decoder_path.display(),
            vocab_path.display()
        );

        // Sessions are built through the runtime so the committed execution provider
        // is applied (CPU registers nothing — byte-identical to the previous
        // CPU-only builder); the provider selection lives in `OrtRuntime`.
        let encoder_session = runtime.build_session(encoder_path)?;
        let decoder_session = runtime.build_session(decoder_path)?;
        let vocab = Vocab::load(vocab_path)?;

        // Discover input/output names positionally, per the MangaOCR contract:
        // encoder input[0]=pixel_values -> output[0]=last_hidden_state;
        // decoder input[0]=input_ids, input[1]=encoder_hidden_states -> output[0]=logits.
        let encoder_input_name = nth_input_name(&encoder_session, 0, "encoder", "pixel_values")?;
        let encoder_output_name = nth_output_name(&encoder_session, 0, "encoder", "last_hidden_state")?;
        let decoder_input_ids_name = nth_input_name(&decoder_session, 0, "decoder", "input_ids")?;
        let decoder_encoder_states_name =
            nth_input_name(&decoder_session, 1, "decoder", "encoder_hidden_states")?;
        let decoder_output_name = nth_output_name(&decoder_session, 0, "decoder", "logits")?;

        ms_log::trace_log!(
            cat::STARTUP,
            "MangaOcrEngine ready vocab_len={} enc_in={} enc_out={} dec_ids={} dec_enc={} dec_out={}",
            vocab.len(),
            encoder_input_name,
            encoder_output_name,
            decoder_input_ids_name,
            decoder_encoder_states_name,
            decoder_output_name
        );

        Ok(Self {
            encoder_session,
            decoder_session,
            vocab,
            encoder_input_name,
            encoder_output_name,
            decoder_input_ids_name,
            decoder_encoder_states_name,
            decoder_output_name,
        })
    }

    /// Recognizes text in `image`, returning the final post-processed string.
    ///
    /// Runs the full pipeline: preprocess -> encoder -> beam search (decoder, no KV
    /// cache) -> WordPiece decode -> `_post_process`. Takes `&mut self` because the
    /// underlying `ort` sessions run with `&mut self`.
    ///
    /// # Errors
    /// - [`OrtError::ImagePreprocess`] if the image cannot be prepared.
    /// - [`OrtError::Inference`] if the encoder or decoder run fails.
    /// - [`OrtError::TensorShape`] on an unexpected output shape/dtype.
    pub fn recognize(&mut self, image: &image::RgbaImage) -> Result<String, OrtError> {
        let pixel_values = preprocess::preprocess(image)?;
        let encoder_states = self.run_encoder(&pixel_values)?;
        let token_ids = self.run_generation(&encoder_states)?;
        let decoded = self.vocab.decode(&token_ids);
        Ok(postprocess::post_process(&decoded))
    }

    /// Runs the vision encoder and extracts the hidden states.
    fn run_encoder(&mut self, pixel_values: &[f32]) -> Result<EncoderStates, OrtError> {
        let side = i64::from(preprocess::OCR_IMAGE_SIDE);
        let channels = i64::try_from(preprocess::OCR_CHANNELS).map_err(|_| OrtError::TensorShape {
            detail: "число каналов не помещается в i64".to_owned(),
        })?;
        let shape = vec![1_i64, channels, side, side];
        let tensor =
            Tensor::<f32>::from_array((shape, pixel_values.to_vec())).map_err(|e| OrtError::TensorShape {
                detail: format!("не удалось создать тензор pixel_values: {e}"),
            })?;

        let input_name = self.encoder_input_name.clone();
        let output_name = self.encoder_output_name.clone();
        let outputs = self
            .encoder_session
            .run(ort::inputs![input_name.as_str() => tensor])
            .map_err(|e| OrtError::Inference {
                stage: "encoder",
                reason: e.to_string(),
            })?;

        let value = outputs.get(output_name.as_str()).ok_or_else(|| OrtError::TensorShape {
            detail: format!("выход энкодера «{output_name}» отсутствует"),
        })?;
        let (shape, data): (&Shape, &[f32]) =
            value.try_extract_tensor::<f32>().map_err(|e| OrtError::TensorShape {
                detail: format!("не удалось извлечь last_hidden_state: {e}"),
            })?;
        let dims: &[i64] = shape;
        if dims.len() != 3 {
            return Err(OrtError::TensorShape {
                detail: format!("энкодер: ожидалась форма [1, seq, hidden], получено {dims:?}"),
            });
        }
        // Log the runtime-inferred hidden dim (768 for base, 192 for 2025); the shape
        // itself is forwarded to the decoder unchanged.
        ms_log::trace_log!(
            cat::STARTUP,
            "MangaOcrEngine encoder out seq={} hidden={}",
            dims[1],
            dims[2]
        );

        Ok(EncoderStates {
            data: data.to_vec(),
            shape: dims.to_vec(),
        })
    }

    /// Runs beam-search generation, driving the decoder session per step.
    fn run_generation(&mut self, encoder_states: &EncoderStates) -> Result<Vec<i64>, OrtError> {
        let config = BeamConfig {
            decoder_start_token_id: DECODER_START_TOKEN_ID,
            eos_token_id: EOS_TOKEN_ID,
            max_length: MAX_LENGTH,
            num_beams: NUM_BEAMS,
            no_repeat_ngram_size: NO_REPEAT_NGRAM_SIZE,
            length_penalty: LENGTH_PENALTY,
            early_stopping: EARLY_STOPPING,
        };

        let ids_name = self.decoder_input_ids_name.clone();
        let enc_name = self.decoder_encoder_states_name.clone();
        let out_name = self.decoder_output_name.clone();
        let vocab_len = self.vocab.len();
        let decoder = &mut self.decoder_session;

        beam::beam_search(&config, |token_ids| {
            let seq_len = i64::try_from(token_ids.len()).map_err(|_| OrtError::TensorShape {
                detail: "длина последовательности не помещается в i64".to_owned(),
            })?;
            let ids_tensor = Tensor::<i64>::from_array((vec![1_i64, seq_len], token_ids.to_vec()))
                .map_err(|e| OrtError::TensorShape {
                    detail: format!("не удалось создать тензор input_ids: {e}"),
                })?;
            // Encoder states are unchanged across steps; the tensor owns its data, so
            // it is re-cloned each step (Phase 1 favors correctness; a borrowed view
            // is a possible later optimization).
            let enc_tensor =
                Tensor::<f32>::from_array((encoder_states.shape.clone(), encoder_states.data.clone()))
                    .map_err(|e| OrtError::TensorShape {
                        detail: format!("не удалось создать тензор encoder_hidden_states: {e}"),
                    })?;

            let outputs = decoder
                .run(ort::inputs![
                    ids_name.as_str() => ids_tensor,
                    enc_name.as_str() => enc_tensor,
                ])
                .map_err(|e| OrtError::Inference {
                    stage: "decoder",
                    reason: e.to_string(),
                })?;

            let value = outputs.get(out_name.as_str()).ok_or_else(|| OrtError::TensorShape {
                detail: format!("выход декодера «{out_name}» отсутствует"),
            })?;
            let (shape, data): (&Shape, &[f32]) =
                value.try_extract_tensor::<f32>().map_err(|e| OrtError::TensorShape {
                    detail: format!("не удалось извлечь logits: {e}"),
                })?;
            let dims: &[i64] = shape;
            if dims.len() != 3 {
                return Err(OrtError::TensorShape {
                    detail: format!("декодер: ожидалась форма [1, len, vocab], получено {dims:?}"),
                });
            }
            let len = usize::try_from(dims[1]).map_err(|_| OrtError::TensorShape {
                detail: format!("декодер: некорректная длина {}", dims[1]),
            })?;
            let vocab = usize::try_from(dims[2]).map_err(|_| OrtError::TensorShape {
                detail: format!("декодер: некорректная ширина словаря {}", dims[2]),
            })?;
            if vocab != vocab_len {
                return Err(OrtError::TensorShape {
                    detail: format!(
                        "декодер: ширина логитов {vocab} не совпадает со словарём {vocab_len}"
                    ),
                });
            }
            if len == 0 {
                return Err(OrtError::TensorShape {
                    detail: "декодер: пустая размерность длины".to_owned(),
                });
            }
            // Next-token logits = logits[0, len - 1, :].
            let offset = (len - 1) * vocab;
            let slice = data.get(offset..offset + vocab).ok_or_else(|| OrtError::TensorShape {
                detail: "декодер: срез логитов вне диапазона".to_owned(),
            })?;
            Ok(slice.to_vec())
        })
    }
}

/// Returns the name of the session's `index`-th input, or a typed shape error.
fn nth_input_name(
    session: &Session,
    index: usize,
    stage: &str,
    expected: &str,
) -> Result<String, OrtError> {
    session
        .inputs()
        .get(index)
        .map(|outlet| outlet.name().to_owned())
        .ok_or_else(|| OrtError::TensorShape {
            detail: format!("{stage}: отсутствует вход #{index} ({expected})"),
        })
}

/// Returns the name of the session's `index`-th output, or a typed shape error.
fn nth_output_name(
    session: &Session,
    index: usize,
    stage: &str,
    expected: &str,
) -> Result<String, OrtError> {
    session
        .outputs()
        .get(index)
        .map(|outlet| outlet.name().to_owned())
        .ok_or_else(|| OrtError::TensorShape {
            detail: format!("{stage}: отсутствует выход #{index} ({expected})"),
        })
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pure sub-modules live in each sub-module (`preprocess`,
    //! `tokenizer`, `beam`, `postprocess`). Full end-to-end inference is an
    //! integration test that requires the real ONNX models and an onnxruntime dylib;
    //! see `tests/manga_ocr_e2e.rs` (marked `#[ignore]`).
}
