//! Tests of the title settings writer and composition loader (`tab/settings_io.rs`): defaults for
//! missing keys, other writers' keys kept, retired keys stripped, malformed files never
//! overwritten (desktop-only).

use super::{CompositionPanelOptions, MtPanelOptions, OcrPanelOptions, TextDetectorPanelOptions, Value};
use super::settings_io::{composition_options_from_settings, save_translation_settings_to_project_file};

/// A title's composition object that lacks keys yields their defaults: the loader never
/// carries a value over from the title loaded before (it starts from the defaults).
#[test]
fn composition_keys_missing_from_a_title_take_their_defaults() {
    let mut first = serde_json::Map::new();
    first.insert("ignore_translated_lines".to_string(), Value::Bool(false));
    first.insert("replica_prefix".to_string(), Value::String("- ".to_string()));
    let loaded = composition_options_from_settings(Some(&first));
    assert!(!loaded.ignore_translated_lines);
    assert_eq!(loaded.replica_prefix, "- ");

    let mut second = serde_json::Map::new();
    second.insert("replica_prefix".to_string(), Value::String("* ".to_string()));
    let loaded = composition_options_from_settings(Some(&second));
    assert_eq!(loaded.ignore_translated_lines, CompositionPanelOptions::default().ignore_translated_lines);
    assert_eq!(loaded.replica_prefix, "* ");
    assert_eq!(composition_options_from_settings(None), CompositionPanelOptions::default());
}

fn save_defaults(path: &std::path::Path) -> Result<(), String> {
    save_translation_settings_to_project_file(
        path,
        &OcrPanelOptions::default(),
        &MtPanelOptions::default(),
        &CompositionPanelOptions::default(),
        None,
        &TextDetectorPanelOptions::default(),
        &crate::neurolate::NeurolateSettings::default(),
    )
}

/// The translation writer owns only its four sections; `canvas` and top-level keys of
/// other writers survive.
#[test]
fn save_keeps_keys_of_other_writers() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
    let path = dir.path().join("settings.json");
    std::fs::write(&path, r#"{"canvas":{"bubble_type":"aside"},"comic_type":"ribbon"}"#).map_err(|err| err.to_string())?;
    save_defaults(&path)?;
    let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).map_err(|err| err.to_string())?).map_err(|err| err.to_string())?;
    assert_eq!(root["canvas"]["bubble_type"], "aside");
    assert_eq!(root["comic_type"], "ribbon");
    for section in ["OCR", "machine_translation", "composition", "text_detector"] {
        assert!(root[section].is_object(), "{section} written");
    }
    Ok(())
}

/// Declared behavior change: a corrupt `settings.json` is no longer replaced by `{}` plus
/// the four sections; the save fails and the file stays byte-identical.
#[test]
fn save_never_overwrites_malformed_file() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
    let path = dir.path().join("settings.json");
    let seed = "{\"canvas\": {";
    std::fs::write(&path, seed).map_err(|err| err.to_string())?;
    assert!(save_defaults(&path).is_err());
    assert_eq!(std::fs::read_to_string(&path).map_err(|err| err.to_string())?, seed);
    Ok(())
}

/// The retired CTD params of older builds are stripped on save; the kept params and
/// unknown keys of the same object survive.
#[test]
fn save_strips_retired_text_detector_params() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
    let path = dir.path().join("settings.json");
    let seed = r#"{"text_detector":{"params":{"detect_size":1024,"det_rearrange_max_batches":4,"font size multiplier":1.0,"font size max":-1.0,"font size min":-1.0,"custom":7}}}"#;
    std::fs::write(&path, seed).map_err(|err| err.to_string())?;
    save_defaults(&path)?;
    let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).map_err(|err| err.to_string())?).map_err(|err| err.to_string())?;
    let params = root["text_detector"]["params"].as_object().ok_or("params is an object")?;
    for stale in ["det_rearrange_max_batches", "font size multiplier", "font size max", "font size min"] {
        assert!(!params.contains_key(stale), "{stale} stripped: {params:?}");
    }
    assert_eq!(params["detect_size"], 1280, "the current options are written");
    assert_eq!(params["custom"], 7, "unknown keys survive");
    Ok(())
}

/// A parseable document whose root is not an object is an error too, and stays
/// byte-identical (consistent with the other `settings.json` writers).
#[test]
fn save_never_overwrites_a_non_object_root() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
    let path = dir.path().join("settings.json");
    let seed = "[1, 2]";
    std::fs::write(&path, seed).map_err(|err| err.to_string())?;
    assert!(save_defaults(&path).is_err());
    assert_eq!(std::fs::read_to_string(&path).map_err(|err| err.to_string())?, seed);
    Ok(())
}
