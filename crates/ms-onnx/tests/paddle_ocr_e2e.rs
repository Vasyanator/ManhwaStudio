/*
File: crates/ms-onnx/tests/paddle_ocr_e2e.rs

Purpose:
End-to-end integration test for the native PaddleOCR detection + recognition
engine. It exercises the WHOLE native pipeline against real ONNX models and a real
onnxruntime shared library. It is `#[ignore]` by default because it requires
external artifacts that are not part of the repository.

Required artifacts (provided via environment variables):
- `MS_ONNX_DYLIB`   : path to a real onnxruntime shared library (>= 1.18.x).
- `MS_ONNX_DET`     : path to the detection model (`det.onnx`).
- `MS_ONNX_REC`     : path to a recognition model (`rec.onnx`).
- `MS_ONNX_DICT`    : path to the recognizer's `dict.txt`.
- `MS_ONNX_IMAGE`   : path to an input page/line image.
- `MS_ONNX_EXPECT`  : (optional) expected joined text; if set, the test asserts
  equality — DO NOT hard-code a fabricated value here.

Run manually with, e.g.:
  MS_ONNX_DYLIB=/path/libonnxruntime.so \
  MS_ONNX_DET=.../PaddleOCR/detection/v5/det.onnx \
  MS_ONNX_REC=.../PaddleOCR/languages/english/rec.onnx \
  MS_ONNX_DICT=.../PaddleOCR/languages/english/dict.txt \
  MS_ONNX_IMAGE=/path/page.png \
  cargo test -p ms-onnx --test paddle_ocr_e2e -- --ignored --nocapture

This test asserts only non-panic and self-consistent invariants (e.g. the reported
source size equals the image size). It never fabricates expected OCR values.
*/

use std::path::{Path, PathBuf};

use ms_onnx::{ExecutionProvider, OrtRuntime, PaddleDetector, PaddleOcrEngine};

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
#[ignore = "requires a real onnxruntime dylib + PaddleOCR models (see file header)"]
fn paddle_ocr_detects_and_recognizes_real_image() {
    let Some(dylib) = required_env("MS_ONNX_DYLIB") else {
        return;
    };
    let Some(det) = required_env("MS_ONNX_DET") else {
        return;
    };
    let Some(rec) = required_env("MS_ONNX_REC") else {
        return;
    };
    let Some(dict) = required_env("MS_ONNX_DICT") else {
        return;
    };
    let Some(image_path) = required_env("MS_ONNX_IMAGE") else {
        return;
    };

    let runtime = OrtRuntime::load(Path::new(&dylib), ExecutionProvider::Cpu, None)
        .expect("onnxruntime dylib must load");
    runtime.warmup().expect("warmup must succeed");

    let image = image::open(&image_path)
        .expect("test image must decode")
        .to_rgba8();
    let (img_w, img_h) = image.dimensions();
    eprintln!("image: {img_w}x{img_h} ({image_path})");

    // --- Detection stage (textdetector.paddle) ---
    let mut detector = PaddleDetector::load(&runtime, &PathBuf::from(&det))
        .expect("detector must load");
    let detection = detector.detect(&image).expect("detection must run");
    assert_eq!(detection.source_size, (img_w, img_h), "source size must match input");
    let mask_set = detection.glyph_mask.pixels().filter(|p| p.0[0] != 0).count();
    eprintln!(
        "detection: quads={} blocks={} glyph_mask_set_px={}",
        detection.quads.len(),
        detection.blocks.len(),
        mask_set
    );
    for (i, block) in detection.blocks.iter().take(10).enumerate() {
        eprintln!("  block[{i}] = {block:?}");
    }

    // --- Full engine (ocr.paddle) ---
    let mut engine =
        PaddleOcrEngine::load(&runtime, &PathBuf::from(&det), &PathBuf::from(&rec), &PathBuf::from(&dict))
            .expect("engine must load");
    let lines = engine.recognize(&image).expect("recognition must run");
    eprintln!("recognized {} non-empty lines:", lines.len());
    for (i, line) in lines.iter().enumerate() {
        eprintln!("  [{i}] {line}");
    }

    if let Ok(expected) = std::env::var("MS_ONNX_EXPECT")
        && !expected.is_empty()
    {
        assert_eq!(lines.join("\n"), expected, "recognized text must match MS_ONNX_EXPECT");
    }
}
