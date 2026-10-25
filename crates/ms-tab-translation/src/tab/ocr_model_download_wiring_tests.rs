//! Tests of the OCR / detector model-download wiring of the tab state (`tab/ocr_routing.rs`) over
//! fake download jobs (no filesystem, no network).

use super::{OcrEngine, OcrLoadState, OcrModelDownloadController, TranslationTabState};
use crate::panels::ocr_model_download::ModelDownloadAction;
use ms_sysprobe::ai_models::external_catalog::PaddleVlVariant;
use crate::ocr_model_download::fakes::{intact_verify, missing_probe, waits_for_cancel};
use crate::ocr_model_download::{DownloadNotice, Jobs, OcrModelDownloadState};
use ms_sysprobe::ai_models::external_catalog::BABERU_OCR;
use std::time::{Duration, Instant};

/// A tab whose download controller runs fakes (no filesystem, no network).
fn tab_with_fake_downloads() -> TranslationTabState {
    let mut tab = TranslationTabState::default();
    tab.ocr_model_downloads = OcrModelDownloadController::with_jobs(Jobs { status: missing_probe, download: waits_for_cancel, verify: intact_verify });
    tab
}

fn settle(tab: &mut TranslationTabState) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        tab.ocr_model_downloads.poll();
        if !tab.ocr_model_downloads.is_active() {
            break;
        }
        assert!(Instant::now() < deadline, "download controller did not settle");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A PaddleOCR-VL variant switch unloads the engine (the loaded weights are the old
/// variant's) and probes the new variant; an unchanged selection probes nothing.
#[test]
fn a_variant_switch_unloads_paddle_vl_and_probes_the_new_variant() {
    let mut tab = tab_with_fake_downloads();
    tab.ocr_panel_options.engine = OcrEngine::PaddleVl;
    tab.ocr_panel_options.paddle_vl_model = PaddleVlVariant::MangaJa;
    tab.set_ocr_state_for_engine(OcrEngine::PaddleVl, OcrLoadState::Ready);
    let manga_ja = tab.selected_ocr_model_specs();
    tab.apply_ocr_model_panel_changes(&manga_ja, PaddleVlVariant::MangaJa, ModelDownloadAction::None);
    assert_eq!(tab.ocr_state_for_engine(OcrEngine::PaddleVl), OcrLoadState::Ready);
    assert_eq!(tab.ocr_model_downloads.state_for(PaddleVlVariant::MangaJa.spec().id), OcrModelDownloadState::Unknown);

    let official = [PaddleVlVariant::Official16.spec()];
    tab.apply_ocr_model_panel_changes(&official, PaddleVlVariant::Official16, ModelDownloadAction::None);
    assert_eq!(tab.ocr_state_for_engine(OcrEngine::PaddleVl), OcrLoadState::NotLoaded);
    assert_eq!(tab.ocr_model_downloads.state_for(PaddleVlVariant::MangaJa.spec().id), OcrModelDownloadState::Checking);
    settle(&mut tab);
    assert_eq!(tab.ocr_model_downloads.state_for(PaddleVlVariant::MangaJa.spec().id), OcrModelDownloadState::Missing);

    // An engine switch to Baberu probes Baberu and leaves PaddleOCR-VL's load state.
    tab.ocr_panel_options.engine = OcrEngine::Baberu;
    tab.apply_ocr_model_panel_changes(&manga_ja, PaddleVlVariant::MangaJa, ModelDownloadAction::None);
    assert_eq!(tab.ocr_model_downloads.state_for(BABERU_OCR.id), OcrModelDownloadState::Checking);
    settle(&mut tab);
}

/// An EasyOCR language change that switches the script probes the new script's model.
#[test]
fn an_easyocr_script_switch_probes_the_new_model() {
    use ms_sysprobe::ai_models::easyocr::{EasyOcrScript, easyocr_spec};
    let mut tab = tab_with_fake_downloads();
    tab.ocr_panel_options.engine = OcrEngine::EasyOcr;
    tab.ocr_panel_options.easy_langs = "ko".to_string();
    let korean = tab.selected_ocr_model_specs();
    assert_eq!(korean.iter().map(|spec| spec.id).collect::<Vec<_>>(), [easyocr_spec(EasyOcrScript::Korean).id]);
    tab.ocr_panel_options.easy_langs = "ja, en".to_string();
    tab.apply_ocr_model_panel_changes(&korean, PaddleVlVariant::Official16, ModelDownloadAction::None);
    assert_eq!(tab.ocr_model_downloads.state_for(easyocr_spec(EasyOcrScript::Japanese).id), OcrModelDownloadState::Checking);
    assert_eq!(tab.ocr_model_downloads.state_for(easyocr_spec(EasyOcrScript::Korean).id), OcrModelDownloadState::Unknown);
    settle(&mut tab);
}

/// Download and Cancel go to the selected engine's bundle (Surya: both checkpoints); an
/// engine without an external model ignores them.
#[test]
fn download_and_cancel_route_to_the_selected_spec() {
    let mut tab = tab_with_fake_downloads();
    tab.ocr_panel_options.engine = OcrEngine::MangaOcr;
    tab.apply_ocr_model_panel_changes(&[], PaddleVlVariant::Official16, ModelDownloadAction::Download);
    assert!(!tab.ocr_model_downloads.is_active());

    tab.ocr_panel_options.engine = OcrEngine::Baberu;
    let baberu = tab.selected_ocr_model_specs();
    tab.apply_ocr_model_panel_changes(&baberu, PaddleVlVariant::Official16, ModelDownloadAction::Download);
    assert!(matches!(tab.ocr_model_downloads.state_for(BABERU_OCR.id), OcrModelDownloadState::Downloading(_)));
    tab.apply_ocr_model_panel_changes(&baberu, PaddleVlVariant::Official16, ModelDownloadAction::Cancel);
    settle(&mut tab);
    let snapshot = tab.ocr_model_downloads.panel_snapshot(&baberu);
    assert_eq!(snapshot.notice, Some(DownloadNotice::Cancelled));
    assert_eq!(snapshot.state, OcrModelDownloadState::Missing);

    tab.ocr_panel_options.engine = OcrEngine::Surya;
    let surya = tab.selected_ocr_model_specs();
    assert_eq!(surya.len(), 2);
    tab.apply_ocr_model_panel_changes(&surya, PaddleVlVariant::Official16, ModelDownloadAction::Download);
    for spec in &surya {
        assert!(matches!(tab.ocr_model_downloads.state_for(spec.id), OcrModelDownloadState::Downloading(_)), "{}", spec.id);
    }
    // The detector panel's Surya bundle shares the detection spec: it sees the running
    // download as another bundle's and cannot start its own.
    let detector = tab.ocr_model_downloads.panel_snapshot(&crate::text_detector::SURYA_DETECTOR_SPECS);
    assert!(detector.other_download.is_some());
    tab.apply_detector_model_panel_action(ModelDownloadAction::Cancel);
    settle(&mut tab);
    assert_eq!(tab.ocr_model_downloads.panel_snapshot(&surya).notice, Some(DownloadNotice::Cancelled));
}
