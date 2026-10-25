//! Tests of the OCR model settings keys (`tab/settings_io.rs`): PaddleOCR-VL variant, Baberu engine
//! key aliases and the per-engine "create mask" parameter, read and written.

use super::{CompositionPanelOptions, Map, MtPanelOptions, OcrEngine, OcrPanelOptions, TextDetectorPanelOptions, Value};
use super::settings_io::{
    apply_translation_settings_sections, detect_mask_param, ocr_engine_to_project_key, paddle_vl_model_from_settings,
    parse_ocr_engine_key,
};
use ms_sysprobe::ai_models::external_catalog::PaddleVlVariant;
use serde_json::json;

/// Writes `ocr` through the real settings writer and returns the `OCR` section.
fn written_ocr_section(ocr: &OcrPanelOptions, seed: Value) -> Map<String, Value> {
    let mut root = seed.as_object().cloned().unwrap_or_default();
    apply_translation_settings_sections(
        &mut root,
        ocr,
        &MtPanelOptions::default(),
        &CompositionPanelOptions::default(),
        &TextDetectorPanelOptions::default(),
        &crate::neurolate::NeurolateSettings::default(),
    );
    root.get("OCR").and_then(Value::as_object).cloned().unwrap_or_default()
}

/// Every variant survives a write + read, keeps the sibling `script` key, and is stored
/// as its stable key.
#[test]
fn paddle_vl_model_round_trips_for_every_variant() {
    for variant in PaddleVlVariant::ALL {
        let options = OcrPanelOptions { paddle_vl_model: variant, ..Default::default() };
        let section = written_ocr_section(&options, json!({}));
        assert_eq!(section["params"]["paddle_vl"]["model"], json!(variant.key()));
        assert_eq!(section["params"]["paddle_vl"]["script"], json!("auto"));
        assert_eq!(paddle_vl_model_from_settings(Some(&section)), variant);
    }
}

/// A project saved before the option existed, an unknown key or a non-string value all
/// load as the default variant (normalized to its key on the next save).
#[test]
fn missing_or_unknown_paddle_vl_model_loads_as_default() {
    assert_eq!(paddle_vl_model_from_settings(None), PaddleVlVariant::Official16);
    for section in [
        json!({}),
        json!({ "params": {} }),
        json!({ "params": { "paddle_vl": { "script": "korean" } } }),
        json!({ "params": { "paddle_vl": { "model": "official_9_9" } } }),
        json!({ "params": { "paddle_vl": { "model": 3 } } }),
    ] {
        let map = section.as_object().cloned().unwrap_or_default();
        assert_eq!(paddle_vl_model_from_settings(Some(&map)), PaddleVlVariant::Official16, "{section}");
    }
    // The legacy section alias and key normalization of `from_key` are honoured.
    let legacy = json!({ "params": { "paddleocrvl": { "model": " MANGA_JA " } } });
    assert_eq!(paddle_vl_model_from_settings(legacy.as_object()), PaddleVlVariant::MangaJa);
}

#[test]
fn baberu_engine_key_round_trips_and_accepts_aliases() {
    for alias in ["baberu", "baberu_ocr", "BaberuOCR", " baberuocr "] {
        assert_eq!(parse_ocr_engine_key(alias), OcrEngine::Baberu, "{alias}");
    }
    assert_eq!(parse_ocr_engine_key(ocr_engine_to_project_key(OcrEngine::Baberu)), OcrEngine::Baberu);
    let options = OcrPanelOptions { engine: OcrEngine::Baberu, ..Default::default() };
    assert_eq!(written_ocr_section(&options, json!({}))["engine"], json!("baberu"));
}

/// The "create mask and text blocks" parameters are per engine: written under their own
/// engine's params section, never as a top-level `OCR` key; other keys of those sections
/// survive.
#[test]
fn detect_mask_parameters_are_written_per_engine() {
    let options = OcrPanelOptions { paddle_detect_mask: true, surya_detect_mask: false, ..Default::default() };
    let written = written_ocr_section(&options, json!({ "OCR": { "params": { "surya": { "custom": 1 } } } }));
    assert_eq!(written["params"]["paddle"]["detect_mask"], json!(true));
    assert_eq!(written["params"]["surya"]["detect_mask"], json!(false));
    assert_eq!(written["params"]["surya"]["custom"], json!(1));
    assert!(written.get("detect_mask").is_none());
}

/// What is written is what the loader reads back per engine; an absent or non-boolean
/// value keeps the default.
#[test]
fn detect_mask_parameters_are_read_per_engine() {
    let options = OcrPanelOptions { paddle_detect_mask: false, surya_detect_mask: true, ..Default::default() };
    let written = written_ocr_section(&options, json!({}));
    let params = written.get("params").and_then(Value::as_object).cloned().unwrap_or_default();
    let section = |engine: &str| params.get(engine).and_then(Value::as_object).cloned().unwrap_or_default();
    assert_eq!(detect_mask_param(&section("paddle")), Some(false));
    assert_eq!(detect_mask_param(&section("surya")), Some(true));
    assert_eq!(detect_mask_param(&Map::new()), None);
    let not_bool = json!({ "detect_mask": "yes" });
    assert_eq!(detect_mask_param(not_bool.as_object().unwrap_or(&Map::new())), None);
}
