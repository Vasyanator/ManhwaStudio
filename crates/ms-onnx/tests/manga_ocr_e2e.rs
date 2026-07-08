/*
File: crates/ms-onnx/tests/manga_ocr_e2e.rs

Purpose:
End-to-end integration test for the native MangaOCR engine. It validates TRUE
parity by running real inference against the downloaded ONNX models and a real
onnxruntime shared library. It is `#[ignore]` by default because it requires
external artifacts that are not part of the repository.

Required artifacts (provided via environment variables):
- `MS_ONNX_DYLIB`      : path to a real onnxruntime shared library (>= 1.18.x).
- `MS_ONNX_MODEL_DIR`  : a MangaOCR export directory containing
  `encoder_model.onnx`, `decoder_model.onnx`, and `vocab.txt`
  (e.g. `ManhwaStudio_AI_Models/ONNX/MangaOCR/base` or `.../2025`).
- `MS_ONNX_IMAGE`      : path to an input image (a tight crop of one text line).
- `MS_ONNX_EXPECT`     : (optional) expected recognized string; if set, the test
  asserts equality — DO NOT hard-code a fabricated value here.

Run manually with, e.g.:
  MS_ONNX_DYLIB=/path/libonnxruntime.so \
  MS_ONNX_MODEL_DIR=.../MangaOCR/base \
  MS_ONNX_IMAGE=/path/line.png \
  cargo test -p ms-onnx --test manga_ocr_e2e -- --ignored --nocapture
*/

use std::path::PathBuf;

use ms_onnx::{ExecutionProvider, MangaOcrEngine, NativeDeviceSelection, OrtRuntime};

/// Reads a required environment variable or skips the test with a clear message.
fn required_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => {
            eprintln!("skipping: environment variable {key} is not set");
            None
        }
    }
}

#[test]
#[ignore = "requires a real onnxruntime dylib + MangaOCR models (see file header)"]
fn manga_ocr_recognizes_real_image() {
    let Some(dylib) = required_env("MS_ONNX_DYLIB") else {
        return;
    };
    let Some(model_dir) = required_env("MS_ONNX_MODEL_DIR") else {
        return;
    };
    let Some(image_path) = required_env("MS_ONNX_IMAGE") else {
        return;
    };

    let model_dir = PathBuf::from(model_dir);
    let encoder = model_dir.join("encoder_model.onnx");
    let decoder = model_dir.join("decoder_model.onnx");
    let vocab = model_dir.join("vocab.txt");

    let runtime = OrtRuntime::load(
        std::path::Path::new(&dylib),
        ExecutionProvider::Cpu,
        NativeDeviceSelection::Default,
    )
        .expect("onnxruntime dylib must load");
    runtime.warmup().expect("ort environment must initialize");

    let mut engine =
        MangaOcrEngine::load(&runtime, &encoder, &decoder, &vocab).expect("engine must load");

    let image = image::open(&image_path)
        .expect("input image must decode")
        .to_rgba8();
    let text = engine.recognize(&image).expect("recognition must succeed");

    println!("MangaOCR recognized: {text:?}");
    assert!(!text.is_empty(), "recognized text should not be empty");

    if let Ok(expected) = std::env::var("MS_ONNX_EXPECT") {
        assert_eq!(text, expected, "recognized text must match the reference");
    }
}
