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

`paddle_forward_prob_maps_matches_detect_real_image` needs only `MS_ONNX_DYLIB`,
`MS_ONNX_DET` and `MS_ONNX_IMAGE`. It checks the forward-only entry
(`PaddleDetector::forward_prob_maps`) against `detect` on the same model input: the
map has the tile's size, a batch of two equals two single runs (within one level),
and the DB postprocess on the quantized map finds about as many quads as `detect`.

Every artifact is optional for the suite: an unset variable or a missing file logs one
`skipping:` line and the test returns without failing.

These tests assert only non-panic and self-consistent invariants (e.g. the reported
source size equals the image size). They never fabricate expected OCR values.
*/

use std::path::{Path, PathBuf};

use image::imageops;
use ms_onnx::paddle_ocr::preprocess::resize_dims_for_det;
use ms_onnx::{
    ExecutionProvider, NativeDeviceSelection, OrtRuntime, PaddleDetector, PaddleOcrEngine,
};
use ms_text_detect::db::boxes_from_bitmap;

/// Reads a required artifact path from the environment, or logs one line and returns `None`
/// (the caller skips) when the variable is unset/empty or names a file that does not exist.
fn required_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => {
            if Path::new(&value).exists() {
                Some(value)
            } else {
                eprintln!("skipping: {key} names a file that does not exist: {value}");
                None
            }
        }
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

    let runtime =
        OrtRuntime::load(Path::new(&dylib), ExecutionProvider::Cpu, NativeDeviceSelection::Default)
        .expect("onnxruntime dylib must load");
    runtime.warmup().expect("warmup must succeed");

    let image = image::open(&image_path)
        .expect("test image must decode")
        .to_rgba8();
    let (img_w, img_h) = image.dimensions();
    eprintln!("image: {img_w}x{img_h} ({image_path})");

    // --- Detection stage (native whole-page `PaddleDetector::detect`) ---
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

#[test]
#[ignore = "requires a real onnxruntime dylib + the PaddleOCR detection model (see file header)"]
fn paddle_forward_prob_maps_matches_detect_real_image() {
    let Some(dylib) = required_env("MS_ONNX_DYLIB") else {
        return;
    };
    let Some(det) = required_env("MS_ONNX_DET") else {
        return;
    };
    let Some(image_path) = required_env("MS_ONNX_IMAGE") else {
        return;
    };

    let runtime =
        OrtRuntime::load(Path::new(&dylib), ExecutionProvider::Cpu, NativeDeviceSelection::Default)
        .expect("onnxruntime dylib must load");
    runtime.warmup().expect("warmup must succeed");
    let mut detector = PaddleDetector::load(&runtime, &PathBuf::from(&det))
        .expect("detector must load");

    let page = image::open(&image_path).expect("test image must decode").to_rgba8();
    let (src_w, src_h) = page.dimensions();
    // The exact model input `detect` builds: the same dims and the same Triangle resize.
    let (model_w, model_h) = resize_dims_for_det(src_w, src_h);
    let tile = image::DynamicImage::ImageRgba8(imageops::resize(&page, model_w, model_h, imageops::FilterType::Triangle)).to_rgb8();

    let single = detector.forward_prob_maps(std::slice::from_ref(&tile)).expect("forward must run");
    assert_eq!(single.len(), 1);
    assert_eq!(single[0].size(), [model_w, model_h], "map must have the tile size");

    let batched = detector.forward_prob_maps(&[tile.clone(), tile]).expect("batched forward must run");
    assert_eq!(batched.len(), 2);
    for map in &batched {
        let max_diff = map.data().iter().zip(single[0].data()).map(|(&a, &b)| a.abs_diff(b)).max().unwrap_or(0);
        assert!(max_diff <= 1, "batched map differs from the single run by {max_diff} levels");
    }

    // Two DIFFERENT full-resolution, 32-aligned crops (top and bottom of the page) in one
    // batch must equal two single runs: the batch axis must not mix tiles.
    let crop_w = (src_w / 32) * 32;
    let crop_h = (src_h / 32).min(30) * 32;
    if crop_w > 0 && crop_h > 0 {
        let rgb = image::DynamicImage::ImageRgba8(page.clone()).to_rgb8();
        let top = imageops::crop_imm(&rgb, 0, 0, crop_w, crop_h).to_image();
        let bottom = imageops::crop_imm(&rgb, 0, src_h - crop_h, crop_w, crop_h).to_image();
        let pair = detector.forward_prob_maps(&[top.clone(), bottom.clone()]).expect("crop batch must run");
        for (map, tile) in pair.iter().zip([top, bottom]) {
            let alone = detector.forward_prob_maps(std::slice::from_ref(&tile)).expect("crop must run");
            assert_eq!(map.size(), [crop_w, crop_h]);
            let max_diff = map.data().iter().zip(alone[0].data()).map(|(&a, &b)| a.abs_diff(b)).max().unwrap_or(0);
            assert!(max_diff <= 1, "batched crop map differs from its single run by {max_diff} levels");
        }
        eprintln!("crop batch: 2 x {crop_w}x{crop_h} ok");
    }

    let reference = detector.detect(&page).expect("detection must run");
    let map_w = usize::try_from(model_w).expect("width fits usize");
    let map_h = usize::try_from(model_h).expect("height fits usize");
    let quads = boxes_from_bitmap(single[0].data(), map_w, map_h, src_w, src_h);
    let set = single[0].data().iter().filter(|&&v| v >= 77).count();
    eprintln!(
        "forward: tile={model_w}x{model_h} px>=0.3: {set}; quads u8 path={} detect={}",
        quads.len(),
        reference.quads.len()
    );
    // Quantization can only move borderline candidates across the box-score gate.
    let tolerance = (reference.quads.len() / 20).max(2);
    assert!(
        quads.len().abs_diff(reference.quads.len()) <= tolerance,
        "u8-map postprocess found {} quads, detect found {}",
        quads.len(),
        reference.quads.len()
    );
}
