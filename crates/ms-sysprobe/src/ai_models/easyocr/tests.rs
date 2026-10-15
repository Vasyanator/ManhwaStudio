/*
File: crates/ms-sysprobe/src/ai_models/easyocr/tests.rs

Purpose:
Tests of the EasyOCR rule owner: the Rust tables equal the library mirror
`crates/ms-sysprobe/fixtures/easyocr_scripts.json` (scripts, order, file names, archives,
language lists, compatibility lists), the recognizer choice reproduces every fixture case
(verified against the real easyocr 1.7.2 `Reader` when the fixture was generated), language
normalization, and the shared-directory spec layout.
*/

use serde_json::Value;

use super::*;

/// The tracked library mirror, embedded at compile time.
const FIXTURE: &str = include_str!("../../../fixtures/easyocr_scripts.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).unwrap_or_else(|err| panic!("fixture is not JSON: {err}"))
}

fn strings(value: &Value) -> Vec<String> {
    value.as_array().unwrap_or_else(|| panic!("not an array: {value}")).iter().map(|item| item.as_str().unwrap_or_default().to_owned()).collect()
}

fn archive_of(file: &ExternalFile) -> &'static str {
    match file.origin {
        FileOrigin::ZipMember { archive, member, .. } => {
            assert_eq!(member, file.path, "member name equals the weight file name");
            archive
        }
        FileOrigin::Direct => panic!("{} is not a zip member", file.path),
    }
}

#[test]
fn the_rust_table_equals_the_library_fixture() {
    let fixture = fixture();
    assert_eq!(fixture["easyocr_version"], "1.7.2");
    assert_eq!(fixture["release_base_url"], RELEASE_BASE_URL);
    assert_eq!(fixture["detector"]["filename"], EASYOCR_DETECTOR_FILE);
    assert_eq!(fixture["detector"]["archive"], archive_of(&CRAFT));
    let scripts = fixture["scripts"].as_array().unwrap_or_else(|| panic!("no scripts"));
    assert_eq!(scripts.len(), EasyOcrScript::ALL.len());
    for (entry, script) in scripts.iter().zip(EasyOcrScript::ALL) {
        assert_eq!(entry["script"], script.key(), "selection order");
        assert_eq!(entry["filename"], script.recognizer_file());
        let spec = easyocr_spec(script);
        assert_eq!(entry["archive"], archive_of(&spec.files[1]));
        let compatible = script.compatible().map(|codes| codes.iter().map(|code| (*code).to_owned()).collect::<Vec<_>>());
        let expected = if entry["compatible"].is_null() { None } else { Some(strings(&entry["compatible"])) };
        assert_eq!(compatible, expected, "{}", script.key());
    }
    let lists = &fixture["lang_lists"];
    for (name, table) in [("latin", LATIN), ("arabic", ARABIC), ("bengali", BENGALI), ("cyrillic", CYRILLIC), ("devanagari", DEVANAGARI), ("other", OTHER)] {
        assert_eq!(strings(&lists[name]), table.to_vec(), "{name}");
    }
    assert_eq!(strings(&fixture["unusable_langs"]), UNUSABLE.to_vec());
}

#[test]
fn the_choice_reproduces_every_fixture_case() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().unwrap_or_else(|| panic!("no cases"));
    assert!(cases.len() >= 20);
    for case in cases {
        let langs = strings(&case["langs"]);
        let got = match easyocr_script_for(&langs) {
            Ok(script) => script.key().to_owned(),
            Err(EasyOcrLangError::Unsupported { .. }) => "error:unsupported".to_owned(),
            Err(EasyOcrLangError::Incompatible { .. }) => "error:incompatible".to_owned(),
            Err(EasyOcrLangError::Empty) => "error:empty".to_owned(),
        };
        assert_eq!(case["expect"], got.as_str(), "{langs:?}");
    }
}

#[test]
fn errors_name_what_is_wrong() {
    assert_eq!(easyocr_script_for::<&str>(&[]), Err(EasyOcrLangError::Empty));
    assert_eq!(easyocr_script_for(&["ko", "zz", "mni"]), Err(EasyOcrLangError::Unsupported { codes: vec!["zz".to_owned(), "mni".to_owned()] }));
    assert!(matches!(easyocr_script_for(&["ja", "ko"]), Err(EasyOcrLangError::Incompatible { script: EasyOcrScript::Japanese, .. })));
    // Duplicates are ignored, so `en, en` is still the English-only branch.
    assert_eq!(easyocr_script_for(&["en", "en"]), Ok(EasyOcrScript::English));
}

#[test]
fn normalization_maps_aliases_dedups_and_defaults_to_korean() {
    assert_eq!(normalize_easyocr_langs(""), vec!["ko"]);
    assert_eq!(normalize_easyocr_langs(" , ,"), vec!["ko"]);
    assert_eq!(normalize_easyocr_langs(" Korean , EN,kor"), vec!["ko", "en"]);
    assert_eq!(normalize_easyocr_langs("jp,ja,JAPANESE"), vec!["ja"]);
    assert_eq!(normalize_easyocr_langs("zh-CN, zh-Hant"), vec!["ch_sim", "ch_tra"]);
    assert_eq!(normalize_easyocr_langs("RS_Latin"), vec!["rs_latin"]);
    assert_eq!(easyocr_script_for(&normalize_easyocr_langs("Chinese, en")), Ok(EasyOcrScript::ChineseSim));
}

#[test]
fn every_script_shares_one_directory_and_one_detector_pin() {
    for script in EasyOcrScript::ALL {
        let spec = easyocr_spec(script);
        assert_eq!(spec.id, format!("easyocr_{}", script.key()));
        assert_eq!((spec.dir, spec.layout), (EASYOCR_DIR, DirLayout::Shared));
        assert_eq!(spec.source, ExternalSource::Url { base_url: RELEASE_BASE_URL });
        assert_eq!(spec.files.len(), 2);
        assert_eq!(spec.files[0], CRAFT);
        assert_eq!(spec.files[0].path, EASYOCR_DETECTOR_FILE);
    }
}
