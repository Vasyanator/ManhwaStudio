/*
File: crates/ms-onnx/src/baberu_ocr/mod.rs

Purpose:
Native Baberu OCR inference engine (genshiai-daichi/baberu-ocr, ONNX "lossless" tier):
a DINOv2 vision encoder plus a 6-layer character-level decoder with a KV cache, run as
three ONNX graphs. Turns one speech-bubble crop into text. Faithful port of the
upstream reference `onnx_infer.py` (Apache-2.0).

Key structures:
- BaberuOcrEngine       : the public entry point (load + recognize); owns three sessions
  and the vocabulary.
- BaberuVisionPlacement : where the vision session ended up (selected EP, substitute, or CPU).

Key functions:
- BaberuOcrEngine::load      : build the three sessions (vision on the selected EP, decoders
  on CPU) and parse the vocab.
- BaberuOcrEngine::recognize : crop -> vision -> prefill -> greedy KV-cached decode -> text.
- graph_placement / build_vision_session : the pure session-placement rule and the vision
  build with its CPU fallback.

Submodules:
- preprocess : RGBA crop -> CHW f32 `pixel_values` (Pillow BICUBIC parity).
- vocab      : `vocab.json` + the content classification of the run caps.
- decode     : the pure greedy loop over a "next logits" closure.

Notes:
Graph contract (`vision_fp16.onnx`, `decoder_prefill_int8.onnx`,
`decoder_step_int8.onnx`), addressed BY NAME:
- vision : `pixel_values` f32 [1,3,224,224] -> `vision_embeds` f32 [1,256,512];
- prefill: `vision_embeds` + `input_ids` i64 [1,1] (BOS) -> `logits` [1,257,V] and
  `present_k0..5` / `present_v0..5` [1,2,257,64];
- step   : `input_ids` [1,1], `position_ids` [1,1] (starting at vision_len + 1) and
  `past_k0..5` / `past_v0..5` -> `logits` [1,1,V] and the grown presents.
The presents are moved out of each run's outputs (`SessionOutputs::remove` hands back a
reference-counted handle, no copy) and fed to the next step.
Placement: vision on the selected EP, decoders on CPU (why: int8 decoder ops fall back to
CPU on GPU EPs; benchmark 2026-10-06). The fp16 vision graph runs wholly on CUDA/DirectML
(216 ms CPU -> 84-98 ms on a GTX 1660 Ti); the decoders' `DynamicQuantizeLinear` /
`MatMulInteger` and KV-cache ops fall back node by node to the CPU on every GPU EP and
pay copies per token (2-4x slower than a plain CPU session). Under TensorRT vision runs
on the CUDA EP of the same build: no TensorRT engine cache is configured, so every session
build would compile for minutes. A vision build that fails on its EP is retried CPU-only
(recorded in the placement, logged by the caller), because CPU is a correct vision backend. Output is not EP-invariant on
fragile crops; CPU decoders keep the decode itself reference-identical.
`recognize` takes `&mut self` because `Session::run` requires it.
*/

pub(crate) mod decode;
pub(crate) mod preprocess;
pub(crate) mod vocab;

use std::path::Path;

use ms_log::trace::cat;
use ort::session::{Session, SessionInputValue, SessionOutputs};
use ort::value::{DynValue, Shape, Tensor};

use crate::{ExecutionProvider, OrtError, OrtRuntime};
use decode::GreedyConfig;
use preprocess::BABERU_IMAGE_SIDE;
use vocab::BaberuVocab;

/// Vision input name.
const PIXEL_VALUES: &str = "pixel_values";
/// Vision output / prefill input name.
const VISION_EMBEDS: &str = "vision_embeds";
/// Token input name of both decoder graphs.
const INPUT_IDS: &str = "input_ids";
/// Position input name of the step graph.
const POSITION_IDS: &str = "position_ids";
/// Logits output name of both decoder graphs.
const LOGITS: &str = "logits";
/// Decoder layers, each with one K and one V cache tensor.
const LAYERS: usize = 6;
/// Number of KV tensors exchanged per step (`LAYERS` K + `LAYERS` V).
const KV_TENSORS: usize = 2 * LAYERS;

/// `(present output name, past input name)` pairs of the KV cache, K layers then V
/// layers — the order the reference zips them in (by name here, so order is cosmetic).
fn kv_names() -> [(String, String); KV_TENSORS] {
    std::array::from_fn(|i| {
        let (kind, layer) = if i < LAYERS { ('k', i) } else { ('v', i - LAYERS) };
        (format!("present_{kind}{layer}"), format!("past_{kind}{layer}"))
    })
}

/// One of the three Baberu ONNX graphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaberuGraph {
    /// `vision_fp16.onnx`.
    Vision,
    /// `decoder_prefill_int8.onnx`.
    Prefill,
    /// `decoder_step_int8.onnx`.
    Step,
}

/// Which execution provider a graph is built on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionPlacement {
    /// [`OrtRuntime::build_session_on`] with this provider and the runtime's device
    /// ([`ExecutionProvider::Cpu`] registers no EP).
    On(ExecutionProvider),
    /// [`OrtRuntime::build_session_cpu_only`]: ONNX Runtime's built-in CPU backend.
    CpuOnly,
}

/// The session-placement rule (the one owner of "which graph runs where") for a runtime
/// committed with `provider`.
///
/// Both int8 decoders are always [`SessionPlacement::CpuOnly`]: their quantized matmuls
/// and KV-cache ops fall back to the CPU on every GPU EP and run slower there. Vision
/// follows the selected provider, except TensorRT, which is replaced by the CUDA EP of
/// the same build: without a persistent engine cache every session build compiles a
/// TensorRT engine for minutes (benchmark 2026-10-06: 374 s for the vision graph).
fn graph_placement(graph: BaberuGraph, provider: ExecutionProvider) -> SessionPlacement {
    match graph {
        BaberuGraph::Prefill | BaberuGraph::Step => SessionPlacement::CpuOnly,
        BaberuGraph::Vision => match provider {
            ExecutionProvider::TensorRt => SessionPlacement::On(ExecutionProvider::Cuda),
            ExecutionProvider::Cpu
            | ExecutionProvider::DirectMl
            | ExecutionProvider::CoreMl
            | ExecutionProvider::Cuda
            | ExecutionProvider::WebGpu
            | ExecutionProvider::OpenVino => SessionPlacement::On(provider),
        },
    }
}

/// Where the Baberu vision session runs, decided once in [`BaberuOcrEngine::load`].
///
/// The decoders always run on the CPU, so this is the only placement worth reporting.
/// The effective provider is exact as far as ONNX Runtime registration goes: every
/// accelerator EP is registered with `error_on_failure()`, so a session that built on
/// it has that EP active. Per-node CPU fallback inside a registered EP is not
/// observable through the ort C API at `api-18`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaberuVisionPlacement {
    /// On the runtime's selected provider ([`ExecutionProvider::Cpu`] included).
    Selected(ExecutionProvider),
    /// On `used` instead of `requested` by rule (TensorRT -> CUDA of the same build).
    Substituted {
        /// The provider the runtime was committed with.
        requested: ExecutionProvider,
        /// The provider vision runs on.
        used: ExecutionProvider,
    },
    /// On the CPU because the session could not be built on `attempted`.
    CpuFallback {
        /// The provider the runtime was committed with.
        requested: ExecutionProvider,
        /// The provider the failed build used (`requested`, or its substitute).
        attempted: ExecutionProvider,
        /// Display text of the failed build's `OrtError`.
        error: String,
    },
}

impl BaberuVisionPlacement {
    /// The provider the vision session effectively runs on.
    #[must_use]
    pub fn effective(&self) -> ExecutionProvider {
        match self {
            Self::Selected(provider) | Self::Substituted { used: provider, .. } => *provider,
            Self::CpuFallback { .. } => ExecutionProvider::Cpu,
        }
    }
}

/// Builds the vision session per [`graph_placement`] and reports where it runs.
///
/// `build_on(ep)` builds the session on `ep`, `build_cpu` on the CPU backend (closures
/// so the decision is testable without onnxruntime). A failure on a non-CPU provider is
/// retried on the CPU and recorded as [`BaberuVisionPlacement::CpuFallback`].
///
/// # Errors
/// The CPU build's error when the CPU retry runs and fails (a bad model file fails the
/// same way on both), or the build's own error on the CPU provider.
fn build_vision_session<S>(
    provider: ExecutionProvider,
    build_on: impl FnOnce(ExecutionProvider) -> Result<S, OrtError>,
    build_cpu: impl FnOnce() -> Result<S, OrtError>,
) -> Result<(S, BaberuVisionPlacement), OrtError> {
    let used = match graph_placement(BaberuGraph::Vision, provider) {
        SessionPlacement::On(used) => used,
        SessionPlacement::CpuOnly => ExecutionProvider::Cpu,
    };
    match build_on(used) {
        Ok(session) if used == provider => Ok((session, BaberuVisionPlacement::Selected(provider))),
        Ok(session) => Ok((session, BaberuVisionPlacement::Substituted { requested: provider, used })),
        Err(error) if used != ExecutionProvider::Cpu => {
            let session = build_cpu()?;
            let error = error.to_string();
            Ok((session, BaberuVisionPlacement::CpuFallback { requested: provider, attempted: used, error }))
        }
        Err(error) => Err(error),
    }
}

/// Native Baberu OCR engine: vision, prefill and step sessions plus the vocabulary.
///
/// Construct with [`BaberuOcrEngine::load`], then call [`BaberuOcrEngine::recognize`]
/// once per bubble crop. Vision runs on the runtime's selected execution provider
/// (see [`BaberuOcrEngine::vision_placement`]), both decoders on ONNX Runtime's CPU
/// backend. It never downloads anything and never reads app config: the caller
/// supplies the file paths.
#[derive(Debug)]
pub struct BaberuOcrEngine {
    /// `vision_fp16.onnx`: `pixel_values` -> `vision_embeds`.
    vision: Session,
    /// Where `vision` runs (selected provider, substitute, or CPU after a failed build).
    vision_placement: BaberuVisionPlacement,
    /// `decoder_prefill_int8.onnx`: `vision_embeds` + BOS -> logits + presents.
    prefill: Session,
    /// `decoder_step_int8.onnx`: one token + past KV -> logits + presents.
    step: Session,
    /// Character vocabulary and content classification.
    vocab: BaberuVocab,
}

impl BaberuOcrEngine {
    /// Loads the three Baberu graphs and the vocabulary.
    ///
    /// `runtime` must be a committed [`OrtRuntime`] (taking it by reference guarantees
    /// the ort environment was loaded first). `vision` / `prefill` / `step` are
    /// `onnx/vision_fp16.onnx`, `onnx/decoder_prefill_int8.onnx` and
    /// `onnx/decoder_step_int8.onnx`; `vocab` is `tokenizer/vocab.json`. Every session
    /// gets all graph optimizations (the reference's `ORT_ENABLE_ALL`). Vision is built
    /// on the runtime's provider (CUDA for TensorRT, and the CPU when that build fails;
    /// [`BaberuOcrEngine::vision_placement`] reports which), the two
    /// decoders always on the CPU (the reference's `CPUExecutionProvider`).
    ///
    /// # Errors
    /// - [`OrtError::SessionBuild`] if a model file is missing/unreadable (or a decoder
    ///   session cannot be built).
    /// - [`OrtError::BaberuVocabLoad`] if the vocabulary cannot be read or parsed.
    /// - [`OrtError::TensorShape`] if a graph lacks an input/output of the contract.
    pub fn load(
        runtime: &OrtRuntime,
        vision: &Path,
        prefill: &Path,
        step: &Path,
        vocab: &Path,
    ) -> Result<Self, OrtError> {
        ms_log::trace_log!(
            cat::STARTUP,
            "BaberuOcrEngine load (committed provider={}) vision={} prefill={} step={} vocab={}",
            runtime.provider().id(),
            vision.display(),
            prefill.display(),
            step.display(),
            vocab.display()
        );
        let bytes = std::fs::read(vocab).map_err(|e| OrtError::BaberuVocabLoad {
            path: vocab.to_path_buf(),
            detail: e.to_string(),
        })?;
        let vocab = BaberuVocab::from_json(&bytes, vocab)?;

        let provider = runtime.provider();
        let (vision, vision_placement) = build_vision_session(
            provider,
            |used| runtime.build_session_on(used, vision),
            || runtime.build_session_cpu_only(vision),
        )?;
        let build_decoder = |graph: BaberuGraph, path: &Path| match graph_placement(graph, provider) {
            SessionPlacement::On(used) => runtime.build_session_on(used, path),
            SessionPlacement::CpuOnly => runtime.build_session_cpu_only(path),
        };
        let prefill = build_decoder(BaberuGraph::Prefill, prefill)?;
        let step = build_decoder(BaberuGraph::Step, step)?;

        let names = kv_names();
        require_io(&vision, "vision", &[PIXEL_VALUES], &[VISION_EMBEDS])?;
        let prefill_outputs: Vec<&str> =
            std::iter::once(LOGITS).chain(names.iter().map(|(present, _)| present.as_str())).collect();
        require_io(&prefill, "decoder_prefill", &[VISION_EMBEDS, INPUT_IDS], &prefill_outputs)?;
        let step_inputs: Vec<&str> = [INPUT_IDS, POSITION_IDS]
            .into_iter()
            .chain(names.iter().map(|(_, past)| past.as_str()))
            .collect();
        require_io(&step, "decoder_step", &step_inputs, &prefill_outputs)?;

        ms_log::trace_log!(
            cat::STARTUP,
            "BaberuOcrEngine ready vocab_size={} vision_provider={}",
            vocab.vocab_size(),
            vision_placement.effective().id()
        );
        Ok(Self { vision, vision_placement, prefill, step, vocab })
    }

    /// Where the vision session runs; the decoders always run on the CPU.
    #[must_use]
    pub fn vision_placement(&self) -> &BaberuVisionPlacement {
        &self.vision_placement
    }

    /// Recognizes the text of one speech-bubble crop.
    ///
    /// Runs preprocess -> vision -> prefill (BOS) -> greedy decode with the KV cache ->
    /// vocabulary decode. Newlines the model emits are kept. Takes `&mut self` because
    /// the `ort` sessions run with `&mut self`.
    ///
    /// # Errors
    /// - [`OrtError::BaberuPreprocess`] if the image is empty.
    /// - [`OrtError::Inference`] if a graph run fails (`stage` names the graph).
    /// - [`OrtError::TensorShape`] on a missing output or an unexpected shape, including a
    ///   logits width that does not match the vocabulary.
    /// - [`OrtError::BaberuDecode`] on NaN or otherwise unusable logits.
    pub fn recognize(&mut self, image: &image::RgbaImage) -> Result<String, OrtError> {
        let pixel_values = preprocess::preprocess(image)?;
        let vision_embeds = self.run_vision(pixel_values)?;
        let vision_len = vision_seq_len(&vision_embeds)?;
        let (first_logits, mut presents) = self.run_prefill(vision_embeds)?;

        let first_position = vision_len
            .checked_add(1)
            .ok_or_else(|| shape_error("vision: длина последовательности переполняет i64".to_owned()))?;
        let vocab_size = self.vocab.vocab_size();
        let names = kv_names();
        let Self { step, vocab, .. } = self;
        let is_content = |id: u32| vocab.is_content(id);
        let mut step_fn = |token: u32, position: i64| -> Result<Vec<f32>, OrtError> {
            let mut inputs: Vec<(&str, SessionInputValue<'_>)> = Vec::with_capacity(KV_TENSORS + 2);
            inputs.push((INPUT_IDS, scalar_i64(i64::from(token), INPUT_IDS)?.into()));
            inputs.push((POSITION_IDS, scalar_i64(position, POSITION_IDS)?.into()));
            for ((_, past), value) in names.iter().zip(presents.drain(..)) {
                inputs.push((past.as_str(), value.into()));
            }
            let mut outputs = step.run(inputs).map_err(|e| OrtError::Inference {
                stage: "decoder_step",
                reason: e.to_string(),
            })?;
            let logits = last_logits_row(&outputs, "decoder_step", vocab_size)?;
            presents = take_presents(&mut outputs, &names, "decoder_step")?;
            Ok(logits)
        };
        let tokens = decode::greedy_decode(
            &GreedyConfig::BABERU,
            &is_content,
            &first_logits,
            first_position,
            &mut step_fn,
        )?;
        Ok(vocab.decode(&tokens))
    }

    /// Runs the vision graph and returns the `vision_embeds` value (moved, not copied).
    fn run_vision(&mut self, pixel_values: Vec<f32>) -> Result<DynValue, OrtError> {
        let side = i64::from(BABERU_IMAGE_SIDE);
        let tensor = Tensor::<f32>::from_array((vec![1_i64, 3, side, side], pixel_values))
            .map_err(|e| shape_error(format!("не удалось создать тензор pixel_values: {e}")))?;
        let mut outputs = self
            .vision
            .run(ort::inputs![PIXEL_VALUES => tensor])
            .map_err(|e| OrtError::Inference { stage: "vision", reason: e.to_string() })?;
        outputs
            .remove(VISION_EMBEDS)
            .ok_or_else(|| shape_error(format!("vision: отсутствует выход «{VISION_EMBEDS}»")))
    }

    /// Runs the prefill graph on `vision_embeds` + BOS and returns the last logits row
    /// and the 12 present KV tensors.
    fn run_prefill(&mut self, vision_embeds: DynValue) -> Result<(Vec<f32>, Vec<DynValue>), OrtError> {
        let bos = scalar_i64(i64::from(GreedyConfig::BABERU.bos), INPUT_IDS)?;
        let vocab_size = self.vocab.vocab_size();
        let inputs: Vec<(&str, SessionInputValue<'_>)> =
            vec![(VISION_EMBEDS, vision_embeds.into()), (INPUT_IDS, bos.into())];
        let mut outputs = self.prefill.run(inputs).map_err(|e| OrtError::Inference {
            stage: "decoder_prefill",
            reason: e.to_string(),
        })?;
        let logits = last_logits_row(&outputs, "decoder_prefill", vocab_size)?;
        let presents = take_presents(&mut outputs, &kv_names(), "decoder_prefill")?;
        Ok((logits, presents))
    }
}

/// Fails with [`OrtError::TensorShape`] unless `session` has every named input/output.
fn require_io(session: &Session, stage: &str, inputs: &[&str], outputs: &[&str]) -> Result<(), OrtError> {
    for &name in inputs {
        if !session.inputs().iter().any(|outlet| outlet.name() == name) {
            return Err(shape_error(format!("{stage}: отсутствует вход «{name}»")));
        }
    }
    for &name in outputs {
        if !session.outputs().iter().any(|outlet| outlet.name() == name) {
            return Err(shape_error(format!("{stage}: отсутствует выход «{name}»")));
        }
    }
    Ok(())
}

/// A `[1, 1]` i64 tensor holding `value`.
fn scalar_i64(value: i64, name: &str) -> Result<Tensor<i64>, OrtError> {
    Tensor::<i64>::from_array((vec![1_i64, 1], vec![value]))
        .map_err(|e| shape_error(format!("не удалось создать тензор {name}: {e}")))
}

/// The sequence length `L` of `vision_embeds` `[1, L, hidden]`.
fn vision_seq_len(vision_embeds: &DynValue) -> Result<i64, OrtError> {
    let (shape, _): (&Shape, &[f32]) = vision_embeds
        .try_extract_tensor::<f32>()
        .map_err(|e| shape_error(format!("vision: не удалось извлечь {VISION_EMBEDS}: {e}")))?;
    let dims: &[i64] = shape;
    match dims {
        [1, len, _hidden] if *len > 0 => Ok(*len),
        other => Err(shape_error(format!("vision: ожидалась форма [1, L, hidden], получено {other:?}"))),
    }
}

/// The last row of `logits` `[1, len, vocab_size]`, checked against the vocabulary.
fn last_logits_row(outputs: &SessionOutputs<'_>, stage: &str, vocab_size: usize) -> Result<Vec<f32>, OrtError> {
    let value = outputs
        .get(LOGITS)
        .ok_or_else(|| shape_error(format!("{stage}: отсутствует выход «{LOGITS}»")))?;
    let (shape, data): (&Shape, &[f32]) = value
        .try_extract_tensor::<f32>()
        .map_err(|e| shape_error(format!("{stage}: не удалось извлечь logits: {e}")))?;
    let dims: &[i64] = shape;
    let [1, len, width] = dims else {
        return Err(shape_error(format!("{stage}: ожидалась форма [1, len, vocab], получено {dims:?}")));
    };
    let len = usize::try_from(*len).map_err(|_| shape_error(format!("{stage}: некорректная длина {len}")))?;
    let width = usize::try_from(*width).map_err(|_| shape_error(format!("{stage}: некорректная ширина {width}")))?;
    if width != vocab_size {
        return Err(shape_error(format!(
            "{stage}: ширина логитов {width} не совпадает со словарём {vocab_size}"
        )));
    }
    let start = len
        .checked_sub(1)
        .and_then(|last| last.checked_mul(width))
        .ok_or_else(|| shape_error(format!("{stage}: пустая размерность длины")))?;
    data.get(start..start + width)
        .map(<[f32]>::to_vec)
        .ok_or_else(|| shape_error(format!("{stage}: срез логитов вне диапазона")))
}

/// Moves the 12 present KV tensors out of `outputs`, in [`kv_names`] order.
fn take_presents(
    outputs: &mut SessionOutputs<'_>,
    names: &[(String, String); KV_TENSORS],
    stage: &str,
) -> Result<Vec<DynValue>, OrtError> {
    names
        .iter()
        .map(|(present, _)| {
            outputs
                .remove(present.as_str())
                .ok_or_else(|| shape_error(format!("{stage}: отсутствует выход «{present}»")))
        })
        .collect()
}

fn shape_error(detail: String) -> OrtError {
    OrtError::TensorShape { detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_names_pair_presents_with_pasts_k_then_v() {
        let names = kv_names();
        assert_eq!(names[0], ("present_k0".to_owned(), "past_k0".to_owned()));
        assert_eq!(names[5], ("present_k5".to_owned(), "past_k5".to_owned()));
        assert_eq!(names[6], ("present_v0".to_owned(), "past_v0".to_owned()));
        assert_eq!(names[11], ("present_v5".to_owned(), "past_v5".to_owned()));
    }

    const ALL_PROVIDERS: [ExecutionProvider; 7] = [
        ExecutionProvider::Cpu,
        ExecutionProvider::DirectMl,
        ExecutionProvider::CoreMl,
        ExecutionProvider::Cuda,
        ExecutionProvider::WebGpu,
        ExecutionProvider::OpenVino,
        ExecutionProvider::TensorRt,
    ];

    #[test]
    fn decoders_are_cpu_only_on_every_provider() {
        for provider in ALL_PROVIDERS {
            assert_eq!(graph_placement(BaberuGraph::Prefill, provider), SessionPlacement::CpuOnly, "{provider:?}");
            assert_eq!(graph_placement(BaberuGraph::Step, provider), SessionPlacement::CpuOnly, "{provider:?}");
        }
    }

    #[test]
    fn vision_follows_the_selected_provider_and_tensorrt_uses_cuda() {
        for provider in ALL_PROVIDERS {
            let expected = if provider == ExecutionProvider::TensorRt { ExecutionProvider::Cuda } else { provider };
            assert_eq!(graph_placement(BaberuGraph::Vision, provider), SessionPlacement::On(expected), "{provider:?}");
        }
    }

    /// Fake session builder: records which provider it ran on, fails on demand.
    fn fake_build(
        calls: &std::cell::RefCell<Vec<ExecutionProvider>>,
        provider: ExecutionProvider,
        fail: bool,
    ) -> Result<ExecutionProvider, OrtError> {
        calls.borrow_mut().push(provider);
        if fail {
            Err(OrtError::SessionBuild {
                path: std::path::PathBuf::from("vision.onnx"),
                reason: format!("{} failed", provider.id()),
            })
        } else {
            Ok(provider)
        }
    }

    /// `build_vision_session`'s result on fakes (the "session" is the provider it was
    /// built on) plus the providers the builders were called with, in order.
    type VisionRun = (Result<(ExecutionProvider, BaberuVisionPlacement), OrtError>, Vec<ExecutionProvider>);

    /// Runs `build_vision_session` on fakes; `fail_on` lists providers whose build fails.
    fn run_vision(provider: ExecutionProvider, fail_on: &[ExecutionProvider]) -> VisionRun {
        let calls = std::cell::RefCell::new(Vec::new());
        let result = build_vision_session(
            provider,
            |used| fake_build(&calls, used, fail_on.contains(&used)),
            || fake_build(&calls, ExecutionProvider::Cpu, fail_on.contains(&ExecutionProvider::Cpu)),
        );
        (result, calls.into_inner())
    }

    #[test]
    fn vision_on_an_accelerator_uses_it() {
        let (result, calls) = run_vision(ExecutionProvider::Cuda, &[]);
        let Ok((session, placement)) = result else { panic!("unexpected {result:?}") };
        assert_eq!(session, ExecutionProvider::Cuda);
        assert_eq!(placement, BaberuVisionPlacement::Selected(ExecutionProvider::Cuda));
        assert_eq!(placement.effective(), ExecutionProvider::Cuda);
        assert_eq!(calls, [ExecutionProvider::Cuda]);
    }

    #[test]
    fn vision_falls_back_to_cpu_when_the_provider_build_fails() {
        let (result, calls) = run_vision(ExecutionProvider::DirectMl, &[ExecutionProvider::DirectMl]);
        let Ok((session, placement)) = result else { panic!("unexpected {result:?}") };
        assert_eq!(session, ExecutionProvider::Cpu);
        assert_eq!(placement.effective(), ExecutionProvider::Cpu);
        let BaberuVisionPlacement::CpuFallback { requested, attempted, error } = placement else {
            panic!("unexpected placement {placement:?}")
        };
        assert_eq!((requested, attempted), (ExecutionProvider::DirectMl, ExecutionProvider::DirectMl));
        assert!(error.contains("directml failed"), "{error}");
        assert_eq!(calls, [ExecutionProvider::DirectMl, ExecutionProvider::Cpu]);
    }

    #[test]
    fn vision_build_failing_on_both_returns_the_cpu_error() {
        let (result, _) = run_vision(ExecutionProvider::WebGpu, &[ExecutionProvider::WebGpu, ExecutionProvider::Cpu]);
        let Err(OrtError::SessionBuild { reason, .. }) = result else { panic!("unexpected {result:?}") };
        assert_eq!(reason, "cpu failed");
    }

    #[test]
    fn vision_on_the_cpu_provider_never_retries() {
        let (result, calls) = run_vision(ExecutionProvider::Cpu, &[ExecutionProvider::Cpu]);
        assert!(matches!(result, Err(OrtError::SessionBuild { .. })), "{result:?}");
        assert_eq!(calls, [ExecutionProvider::Cpu]);
        let (result, _) = run_vision(ExecutionProvider::Cpu, &[]);
        let Ok((_, placement)) = result else { panic!("unexpected {result:?}") };
        assert_eq!(placement, BaberuVisionPlacement::Selected(ExecutionProvider::Cpu));
    }

    #[test]
    fn tensorrt_runs_vision_on_cuda() {
        let (result, calls) = run_vision(ExecutionProvider::TensorRt, &[]);
        let Ok((session, placement)) = result else { panic!("unexpected {result:?}") };
        assert_eq!(session, ExecutionProvider::Cuda);
        assert_eq!(
            placement,
            BaberuVisionPlacement::Substituted { requested: ExecutionProvider::TensorRt, used: ExecutionProvider::Cuda }
        );
        assert_eq!(placement.effective(), ExecutionProvider::Cuda);
        assert_eq!(calls, [ExecutionProvider::Cuda]);
    }

    #[test]
    fn tensorrt_falls_back_to_cpu_when_cuda_fails() {
        let (result, calls) = run_vision(ExecutionProvider::TensorRt, &[ExecutionProvider::Cuda]);
        let Ok((_, placement)) = result else { panic!("unexpected {result:?}") };
        let BaberuVisionPlacement::CpuFallback { requested, attempted, .. } = placement else {
            panic!("unexpected placement {placement:?}")
        };
        assert_eq!((requested, attempted), (ExecutionProvider::TensorRt, ExecutionProvider::Cuda));
        assert_eq!(calls, [ExecutionProvider::Cuda, ExecutionProvider::Cpu]);
    }
}
