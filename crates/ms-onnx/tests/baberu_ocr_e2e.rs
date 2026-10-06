/*
File: crates/ms-onnx/tests/baberu_ocr_e2e.rs

Purpose:
Opt-in real-weights parity test of the native Baberu OCR engine: it decodes the
committed sample crops in `fixtures/baberu/` and compares the text with what the
upstream Python reference loop produced on the CPU execution provider (recorded in
`fixtures/baberu/e2e_cases.json` by `tools/make_baberu_fixtures.py`).

Required environment (the test skips with one line when either is unset):
- `MS_BABERU_MODEL_DIR` : a Baberu model directory with `onnx/vision_fp16.onnx`,
  `onnx/decoder_prefill_int8.onnx`, `onnx/decoder_step_int8.onnx` and
  `tokenizer/vocab.json` (the app keeps it in
  `ManhwaStudio_AI_Models/side_models/BaberuOCR/`).
- `MS_ORT_DYLIB` : an onnxruntime shared library (>= 1.18; the fixtures were recorded
  with onnxruntime 1.27.0 CPU, which is the exact-parity configuration).
Optional:
- `MS_BABERU_E2E_CASES` : another `e2e_cases.json` (same format, image paths relative
  to it), e.g. a larger sweep written by `tools/make_baberu_fixtures.py --out <tmp>`
  with more samples; replaces the committed cases.
- `MS_BABERU_E2E_PROVIDER` : an `ExecutionProvider::id` (`cpu` default, e.g. `webgpu`,
  `cuda`, `directml`) the runtime is committed with, so the vision graph runs on that EP
  (decoders stay on the CPU). The dylib must be a build that carries the EP. The test
  then also requires the vision session to have built on it (no CPU fallback). Only the
  4 committed fixtures are expected EP-stable; a sweep may differ on fragile crops.

Run:
  MS_BABERU_MODEL_DIR=<dir> MS_ORT_DYLIB=<libonnxruntime> \
  cargo test -p ms-onnx --test baberu_ocr_e2e -- --nocapture
*/

use std::path::{Path, PathBuf};

use ms_onnx::{BaberuOcrEngine, BaberuVisionPlacement, ExecutionProvider, NativeDeviceSelection, OrtRuntime};

/// The value of `key`, or `None` after logging the skip.
fn required_env(key: &str) -> Option<PathBuf> {
    match std::env::var_os(key) {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => {
            eprintln!("skipping baberu_ocr_e2e: environment variable {key} is not set");
            None
        }
    }
}

/// The provider named by `MS_BABERU_E2E_PROVIDER` (default CPU); panics on an unknown id.
fn requested_provider() -> ExecutionProvider {
    let Some(raw) = std::env::var("MS_BABERU_E2E_PROVIDER").ok().filter(|value| !value.is_empty()) else {
        return ExecutionProvider::Cpu;
    };
    let all = [
        ExecutionProvider::Cpu,
        ExecutionProvider::DirectMl,
        ExecutionProvider::CoreMl,
        ExecutionProvider::Cuda,
        ExecutionProvider::WebGpu,
        ExecutionProvider::OpenVino,
        ExecutionProvider::TensorRt,
    ];
    let found = all.into_iter().find(|provider| provider.id() == raw);
    let Some(provider) = found else {
        panic!("MS_BABERU_E2E_PROVIDER={raw:?} is not an ExecutionProvider id");
    };
    provider
}

#[test]
fn baberu_matches_python_reference_on_committed_samples() {
    let (Some(model_dir), Some(dylib)) = (required_env("MS_BABERU_MODEL_DIR"), required_env("MS_ORT_DYLIB"))
    else {
        return;
    };
    let committed = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures").join("baberu").join("e2e_cases.json");
    let cases_path = std::env::var_os("MS_BABERU_E2E_CASES").map_or(committed, PathBuf::from);
    let fixtures = cases_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let cases_text = std::fs::read_to_string(&cases_path);
    let Ok(cases_text) = cases_text else {
        panic!("cannot read {}: {cases_text:?}", cases_path.display());
    };
    let cases: serde_json::Value = serde_json::from_str(&cases_text).unwrap_or(serde_json::Value::Null);
    let cases = cases["cases"].as_array().cloned().unwrap_or_default();
    assert!(!cases.is_empty(), "e2e_cases.json lists no cases");

    // The CPU provider is what the fixture was recorded with (exact parity). On an
    // accelerator build only the vision graph would move to the EP (decoders stay on the
    // CPU); the 4 committed fixtures were measured EP-stable, a larger sweep is not.
    let provider = requested_provider();
    let runtime = OrtRuntime::load(&dylib, provider, NativeDeviceSelection::Default);
    let Ok(runtime) = runtime else {
        panic!("onnxruntime load failed: {runtime:?}");
    };
    let onnx = model_dir.join("onnx");
    let engine = BaberuOcrEngine::load(
        &runtime,
        &onnx.join("vision_fp16.onnx"),
        &onnx.join("decoder_prefill_int8.onnx"),
        &onnx.join("decoder_step_int8.onnx"),
        &model_dir.join("tokenizer").join("vocab.json"),
    );
    let Ok(mut engine) = engine else {
        panic!("engine load failed: {engine:?}");
    };
    assert_eq!(engine.vision_placement(), &BaberuVisionPlacement::Selected(provider));
    eprintln!("vision on {}, decoders on cpu", provider.id());

    for case in cases {
        let image_name = case["image"].as_str().unwrap_or_default();
        let expected = case["text"].as_str().unwrap_or_default();
        let image = image::open(fixtures.join(image_name));
        let Ok(image) = image else {
            panic!("{image_name}: cannot open sample: {image:?}");
        };
        let started = std::time::Instant::now();
        let text = engine.recognize(&image.to_rgba8());
        let Ok(text) = text else {
            panic!("{image_name}: recognize failed: {text:?}");
        };
        eprintln!("{image_name}: {text:?} in {:?}", started.elapsed());
        assert_eq!(text, expected, "{image_name}: differs from the Python reference");
    }
}
