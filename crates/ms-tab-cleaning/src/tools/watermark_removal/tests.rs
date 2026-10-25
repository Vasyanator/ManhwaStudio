/*
File: tools/watermark_removal/tests.rs

Purpose:
Unit tests of the watermark removal tool (settings, chapter jobs, capture, wording, blob codec).

Key items:
- the tool's test cases

Notes:
Moved out of `mod.rs` unchanged; test scratch goes to the system temp directory.
*/
use super::*;
use crate::tools::base::CleaningTool;
use crate::tools::mask_generation::{DEFAULT_WATERMARK_MODEL, WATERMARK_DETECT_DOWNSCALE_TO};
use crate::tools::watermark_entry::{CropMargins, engine_background, reference_crop_margin_px};
use crate::tools::watermark_library::{EntrySummary, LoadedEntry, SaveEntryRequest, StoredSourceRef};
use crate::tools::watermark_library_window::{LibraryPanelRequest, remove_scratch_crop};
use crate::input::CleaningLabels;
use ms_backend_ipc as backend_ipc;
use ms_canvas::{CanvasView, OverlayRectPx};
use ms_models::SessionCapabilities;
use crate::watermark_chapter::{
    AlphaSource, AlphaUncertainty, CalibrationSample, DetectionParams, MarkTemplate,
    ModelConditioning, PixelRect, RemovalResidual, SampleParams, WatermarkKind,
    alpha_blend_operator,
};
use image::RgbaImage;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use web_time::Duration;
use super::backend::{split_watermark_remove_blob, watermark_request_header};
use super::chapter_jobs::{
    build_library_request, match_catalog_against_library, run_chapter_sample, run_chapter_use_match,
};
use super::chapter_state::{
    decode_chapter_page, next_library_arm, overlay_rect_to_page, page_rect_to_overlay,
    refit_chapter_kind,
};
use super::describe::describe_residual;
use super::library_capture::{grow_page_rect, run_library_capture, scratch_crop_is_stale};
use super::settings::{effective_watermark_mode, offered_watermark_modes};
// Only the tests build library records by hand; the product code reaches them through
// `watermark_entry`, which is why this is not in the module's own import list.
use super::super::watermark_library::StoredSignature;
// The intake itself is driven only here, to prove the CAPTURE lane's own output is
// acceptable to it; the product code runs it from the library panel's worker.
use super::super::watermark_entry::{ReferenceIntakeRequest, run_reference_intake};

#[test]
fn mode_wire_roundtrip() {
    assert_eq!(WatermarkMode::from_wire("clean"), WatermarkMode::Clean);
    assert_eq!(
        WatermarkMode::from_wire("mask_only"),
        WatermarkMode::MaskOnly
    );
    // An unknown persisted value must fall back to the SAFE mode, never to
    // the experimental one.
    assert_eq!(WatermarkMode::from_wire("bogus"), WatermarkMode::MaskOnly);
    assert_eq!(WatermarkMode::from_wire("chapter"), WatermarkMode::Chapter);
    assert_eq!(WatermarkMode::MaskOnly.wire(), "mask_only");
    assert_eq!(WatermarkMode::Clean.wire(), "clean");
    assert_eq!(WatermarkMode::Chapter.wire(), "chapter");
    assert_eq!(
        WatermarkNetworkMode::MaskOnly.ipc_method(),
        backend_ipc::protocol::METHOD_WATERMARK_DETECT
    );
    assert_eq!(
        WatermarkNetworkMode::Clean.ipc_method(),
        backend_ipc::protocol::METHOD_WATERMARK_REMOVE
    );
    // The chapter mode is local: it has no backend method and needs no Torch. This
    // mapping now gates the mode picker entries and the run button INSIDE the tool,
    // not the tool's selectability — see `the_tool_is_never_torch_gated`.
    assert_eq!(WatermarkMode::Chapter.network(), None);
    assert!(!WatermarkMode::Chapter.requires_torch());
    assert!(WatermarkMode::MaskOnly.requires_torch());
    assert!(WatermarkMode::Clean.requires_torch());
}

/// The picker BUTTON must never be Torch-gated, whatever mode is persisted.
///
/// `pytorch_required` feeds both the button's `AiRequirement::Torch` badge and
/// `CleaningTabState::ensure_active_tool_available`, which runs every frame. While it
/// followed the mode, the persisted default (`mask_only`) disabled the button on a
/// Torch-less machine — hiding the one mode that needs nothing — and selecting a network
/// mode force-deselected the tool on the next frame. The gate lives inside the tool now
/// (`draw_mode_picker`, `draw_actions`), so this answer is a constant.
#[test]
fn the_tool_is_never_torch_gated() {
    let mut tool = WatermarkRemovalTool::default();
    for mode in [
        WatermarkMode::MaskOnly,
        WatermarkMode::Clean,
        WatermarkMode::Chapter,
    ] {
        tool.settings.mode = mode.wire().to_string();
        assert!(
            !tool.pytorch_required(),
            "{mode:?} must not make the tool button Torch-gated"
        );
    }
    // Including an unknown persisted value, which falls back to `mask_only`.
    tool.settings.mode = "bogus".to_string();
    assert!(!tool.pytorch_required());
}

/// A single-image session never runs the chapter mode — it would key the global library by
/// the scratch directory — and never offers it, while a project keeps every mode.
#[test]
fn a_single_image_session_never_runs_or_offers_the_chapter_mode() {
    assert_eq!(effective_watermark_mode("chapter", false), WatermarkMode::MaskOnly);
    assert_eq!(effective_watermark_mode("chapter", true), WatermarkMode::Chapter);
    assert_eq!(effective_watermark_mode("clean", false), WatermarkMode::Clean);
    assert_eq!(effective_watermark_mode("mask_only", false), WatermarkMode::MaskOnly);
    assert!(!offered_watermark_modes(false).contains(&WatermarkMode::Chapter));
    assert!(offered_watermark_modes(true).contains(&WatermarkMode::Chapter));
}

/// The session fallback leaves the persisted choice alone: the tool reads «По главе» as
/// «Только маска» in a single-image session without rewriting `settings.mode`, and the
/// chapter library panel stays hidden even when its toggle was on.
#[test]
fn a_single_image_session_keeps_the_persisted_chapter_mode() {
    let mut tool = WatermarkRemovalTool::default();
    tool.settings.mode = WatermarkMode::Chapter.wire().to_string();
    tool.library_window.toggle();
    tool.set_session_capabilities(SessionCapabilities::IMAGE_SET);
    assert_eq!(tool.effective_mode(), WatermarkMode::MaskOnly);
    assert_eq!(tool.settings.mode, WatermarkMode::Chapter.wire());
    assert!(!tool.wants_library_panel());
    tool.set_session_capabilities(SessionCapabilities::PROJECT);
    assert_eq!(tool.effective_mode(), WatermarkMode::Chapter);
    assert!(tool.wants_library_panel());
}

/// The «Библиотека знаков» dock tab follows the library state and nothing else, and it
/// starts hidden: a dock tab has no close affordance, so the two «Библиотека знаков…»
/// buttons toggling this ONE flag are the whole story.
#[test]
fn the_library_panel_follows_the_library_toggle() {
    let mut tool = WatermarkRemovalTool::default();
    assert!(!tool.wants_library_panel());
    tool.library_window.toggle();
    assert!(tool.wants_library_panel());
    tool.library_window.toggle();
    assert!(!tool.wants_library_panel());
}

/// The library panel is a DOCK panel: the router already treats it as occluding the canvas,
/// and it must not publish the editor-window context, which would switch off the
/// clean-overlay undo for as long as the panel stays open.
#[test]
fn the_library_panel_keeps_the_canvas_selection_and_the_undo() {
    let mut tool = WatermarkRemovalTool::default();
    tool.library_window.toggle();
    assert!(tool.wants_library_panel());
    assert_eq!(tool.input_contexts(), vec![crate::input::REGION_SELECT]);
}

#[test]
fn defaults_are_mask_only_slbr() {
    let settings = WatermarkRemovalSettings::default();
    assert_eq!(settings.mode, "mask_only");
    assert_eq!(settings.model, DEFAULT_WATERMARK_MODEL);
    assert!(settings.show_mask_preview);
    assert_eq!(settings, settings.normalized());
}

#[test]
fn settings_roundtrip_through_json() {
    let settings = WatermarkRemovalSettings {
        mode: WatermarkMode::Clean.wire().to_string(),
        model: "splitnet".to_string(),
        tile: 256,
        overlap: 32,
        threshold: 0.35,
        dilate_px: 9,
        show_mask_preview: false,
        chapter_anchor_tolerance_px: 4,
        chapter_background_blur_px: 8,
        chapter_ring_width_px: 5,
    };
    let raw = serde_json::to_string(&settings).expect("serialize settings");
    let parsed: WatermarkRemovalSettings =
        serde_json::from_str(&raw).expect("deserialize settings");
    assert_eq!(parsed, settings);
}

#[test]
fn partial_json_uses_defaults() {
    let empty: WatermarkRemovalSettings =
        serde_json::from_str("{}").expect("deserialize empty object");
    assert_eq!(empty, WatermarkRemovalSettings::default());

    let partial: WatermarkRemovalSettings =
        serde_json::from_str(r#"{"tile": 256, "mode": "clean"}"#).expect("deserialize partial");
    assert_eq!(partial.tile, 256);
    assert_eq!(partial.mode, "clean");
    assert_eq!(partial.model, WatermarkRemovalSettings::default().model);
    assert_eq!(
        partial.dilate_px,
        WatermarkRemovalSettings::default().dilate_px
    );
}

#[test]
fn normalize_clamps_every_parameter() {
    let wild = WatermarkRemovalSettings {
        mode: "nonsense".to_string(),
        model: "not-a-model".to_string(),
        tile: 9999,
        overlap: 9999,
        threshold: 12.0,
        dilate_px: 999,
        show_mask_preview: true,
        chapter_anchor_tolerance_px: 9999,
        chapter_background_blur_px: 9999,
        chapter_ring_width_px: 9999,
    };
    let normalized = wild.normalized();
    assert_eq!(normalized.mode, WatermarkMode::MaskOnly.wire());
    assert_eq!(normalized.model, DEFAULT_WATERMARK_MODEL);
    assert_eq!(normalized.tile, WATERMARK_TILE_MAX);
    assert_eq!(normalized.tile % WATERMARK_TILE_MULTIPLE, 0);
    assert_eq!(normalized.overlap, WATERMARK_OVERLAP_MAX);
    assert!((normalized.threshold - WATERMARK_THRESHOLD_MAX).abs() < f32::EPSILON);
    assert_eq!(normalized.dilate_px, WATERMARK_DILATE_MAX);
    // The chapter parameters are clamped by the ENGINE's own measured bounds, so a
    // hand-edited settings file cannot widen the anchor band into "anywhere".
    let engine_detection = DetectionParams {
        anchor_tolerance: u32::MAX,
        background_blur_radius: u32::MAX,
        ..DetectionParams::default()
    }
    .normalized();
    assert_eq!(
        normalized.chapter_anchor_tolerance_px,
        engine_detection.anchor_tolerance
    );
    assert_eq!(
        normalized.chapter_background_blur_px,
        engine_detection.background_blur_radius
    );
    assert_eq!(
        normalized.chapter_ring_width_px,
        SampleParams {
            ring_width: u32::MAX,
            ..SampleParams::default()
        }
        .normalized()
        .ring_width
    );

    // With a small tile it is half the tile, not the slider maximum, that caps
    // the overlap — otherwise neighbouring tiles would swallow each other.
    let narrow = WatermarkRemovalSettings {
        tile: WATERMARK_TILE_MIN,
        overlap: WATERMARK_OVERLAP_MAX,
        ..WatermarkRemovalSettings::default()
    };
    assert_eq!(narrow.normalized().overlap, WATERMARK_TILE_MIN / 2);

    let tiny = WatermarkRemovalSettings {
        tile: 10,
        overlap: 4,
        threshold: f32::NAN,
        dilate_px: 0,
        ..WatermarkRemovalSettings::default()
    };
    let normalized = tiny.normalized();
    assert_eq!(normalized.tile, WATERMARK_TILE_MIN);
    assert_eq!(normalized.overlap, 4);
    assert!(
        (normalized.threshold - WatermarkRemovalSettings::default().threshold).abs()
            < f32::EPSILON
    );

    // A tile that is not a multiple of 16 is snapped DOWN, never up: the
    // networks reject any other size.
    let odd = WatermarkRemovalSettings {
        tile: 519,
        ..WatermarkRemovalSettings::default()
    };
    assert_eq!(odd.normalized().tile, 512);
}

#[test]
fn blob_split_accepts_exact_lengths() {
    let blob = b"CLEANMASK".to_vec();
    let header = json!({ "image_len": 5, "mask_len": 4 });
    let (clean, mask) =
        split_watermark_remove_blob(&header, &blob).expect("exact lengths must split");
    assert_eq!(clean, b"CLEAN");
    assert_eq!(mask, b"MASK");
}

#[test]
fn blob_split_rejects_short_padded_and_malformed() {
    let blob = b"CLEANMASK".to_vec();

    // Declared more than the blob carries.
    let short = json!({ "image_len": 5, "mask_len": 5 });
    assert!(split_watermark_remove_blob(&short, &blob).is_err());

    // Declared less than the blob carries (trailing padding).
    let padded = json!({ "image_len": 5, "mask_len": 3 });
    assert!(split_watermark_remove_blob(&padded, &blob).is_err());

    // Missing / non-integer / negative fields.
    assert!(split_watermark_remove_blob(&json!({ "mask_len": 4 }), &blob).is_err());
    assert!(
        split_watermark_remove_blob(&json!({ "image_len": "5", "mask_len": 4 }), &blob).is_err()
    );
    assert!(
        split_watermark_remove_blob(&json!({ "image_len": -5, "mask_len": 4 }), &blob).is_err()
    );

    // A zero-length part is an empty result, not a valid split.
    assert!(
        split_watermark_remove_blob(&json!({ "image_len": 9, "mask_len": 0 }), &blob).is_err()
    );
    assert!(
        split_watermark_remove_blob(&json!({ "image_len": 0, "mask_len": 9 }), &blob).is_err()
    );
}

#[test]
fn request_header_carries_wire_params_per_mode() {
    let settings = WatermarkRemovalSettings {
        mode: WatermarkMode::Clean.wire().to_string(),
        model: "wdnet".to_string(),
        tile: 256,
        overlap: 64,
        threshold: 0.4,
        dilate_px: 6,
        show_mask_preview: true,
        ..WatermarkRemovalSettings::default()
    }
    .normalized();

    let clean = watermark_request_header(WatermarkNetworkMode::Clean, &settings);
    let params = clean.get("params").expect("clean header has params");
    assert_eq!(params.get("model").and_then(Value::as_str), Some("wdnet"));
    assert_eq!(params.get("tile").and_then(Value::as_u64), Some(256));
    assert_eq!(params.get("overlap").and_then(Value::as_u64), Some(64));
    assert_eq!(params.get("dilate_px").and_then(Value::as_u64), Some(6));
    assert!(params.get("downscale_to").is_none());

    let detect = watermark_request_header(WatermarkNetworkMode::MaskOnly, &settings);
    let params = detect.get("params").expect("detect header has params");
    assert_eq!(
        params.get("downscale_to").and_then(Value::as_u64),
        Some(u64::from(WATERMARK_DETECT_DOWNSCALE_TO))
    );
    // The detect pass is not tiled, so the tile geometry must not leak into it.
    assert!(params.get("tile").is_none());
    assert!(params.get("overlap").is_none());
}

// -----------------------------------------------------------------------------------
// Chapter mode
// -----------------------------------------------------------------------------------

/// Synthetic mark: a cross at opacity 0.4 inside a border at 0.2, colour (30, 200, 30).
///
/// Every product below is an exact byte at both background levels used here, so the
/// test measures the pipeline rather than rounding.
const SYNTHETIC_MARK_SIDE: u32 = 16;
const SYNTHETIC_MARK_COLOUR: [f32; 3] = [30.0, 200.0, 30.0];

fn synthetic_alpha(x: u32, y: u32) -> f32 {
    let inside_cross = (6..10).contains(&x) || (6..10).contains(&y);
    let on_border = x == 0 || y == 0 || x + 1 == SYNTHETIC_MARK_SIDE || y + 1 == SYNTHETIC_MARK_SIDE;
    if inside_cross {
        0.4
    } else if on_border {
        0.2
    } else {
        0.0
    }
}

/// Stamps the synthetic mark over a uniform background at `(ox, oy)`.
fn stamp_synthetic_mark(page: &mut RgbaImage, ox: u32, oy: u32) {
    for y in 0..SYNTHETIC_MARK_SIDE {
        for x in 0..SYNTHETIC_MARK_SIDE {
            let alpha = synthetic_alpha(x, y);
            let pixel = page.get_pixel_mut(ox + x, oy + y);
            for channel in 0..3 {
                let background = f32::from(pixel[channel]);
                let value = alpha * SYNTHETIC_MARK_COLOUR[channel] + (1.0 - alpha) * background;
                // Cast justification: every product is exact for the alphas and levels
                // used here, and the value is clamped into 0..=255 anyway.
                pixel[channel] = value.clamp(0.0, 255.0).round() as u8;
            }
        }
    }
}

/// Writes a 200x600 page holding the mark twice on white and once on black, and
/// returns its path plus the three occurrence rects.
fn write_synthetic_page(name: &str) -> (PathBuf, [OverlayRectPx; 3]) {
    let dir = std::env::temp_dir().join(format!("manhwastudio-wm-chapter-test-{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    let mut page = RgbaImage::from_pixel(200, 600, image::Rgba([255, 255, 255, 255]));
    for y in 350..500 {
        for x in 0..200 {
            page.put_pixel(x, y, image::Rgba([0, 0, 0, 255]));
        }
    }
    let rects = [
        OverlayRectPx {
            x: 100,
            y: 100,
            w: SYNTHETIC_MARK_SIDE as usize,
            h: SYNTHETIC_MARK_SIDE as usize,
        },
        OverlayRectPx {
            x: 100,
            y: 200,
            w: SYNTHETIC_MARK_SIDE as usize,
            h: SYNTHETIC_MARK_SIDE as usize,
        },
        OverlayRectPx {
            x: 100,
            y: 400,
            w: SYNTHETIC_MARK_SIDE as usize,
            h: SYNTHETIC_MARK_SIDE as usize,
        },
    ];
    for rect in &rects {
        // Cast justification: the rects above are literals far below u32::MAX.
        stamp_synthetic_mark(&mut page, rect.x as u32, rect.y as u32);
    }
    let path = dir.join("page.png");
    page.save(&path).expect("write synthetic page");
    (path, rects)
}

/// A catalog holding one mark LOADED from a library entry and one the chapter discovered
/// on its own, with a kind id that would fool a plain `index_of` lookup.
fn catalog_with_library_and_discovered(entry_id: &str, discovered_id: &str) -> ChapterCatalog {
    let mut catalog = ChapterCatalog::default();
    for (id, library_entry) in [
        (entry_id, Some(entry_id.to_string())),
        (discovered_id, None),
    ] {
        let mut crop = RgbaImage::from_pixel(16, 16, image::Rgba([255, 255, 255, 255]));
        stamp_synthetic_mark(&mut crop, 0, 0);
        let template = MarkTemplate::from_page(&crop, PixelRect::new(0, 0, 16, 16))
            .expect("template from the synthetic crop");
        let kind = WatermarkKind::new(id.to_string(), template, alpha_blend_operator());
        let mut mark = ChapterMark::new(id.to_string(), crop, 200);
        mark.library_entry = library_entry;
        catalog.push(kind, mark);
    }
    catalog
}

/// The card's «Выбрать»/«Убрать» reads chapter MEMBERSHIP, and «Убрать» drops only the
/// mark that came from that entry: a mark the chapter discovered for itself cost a scan
/// to collect and must survive a click on a library card that happens to sit next to it.
#[test]
fn the_card_toggle_reflects_catalog_membership_and_spares_discovered_marks() {
    let mut tool = WatermarkRemovalTool::default();
    tool.chapter.catalog = catalog_with_library_and_discovered("entry-a", "mark-1");
    assert_eq!(tool.chapter_library_selection(), vec!["entry-a".to_string()]);

    tool.unload_library_entry("mark-1");
    assert_eq!(
        tool.chapter.catalog.len(),
        2,
        "a chapter-discovered mark is not the library card's to remove"
    );

    tool.unload_library_entry("entry-a");
    assert_eq!(tool.chapter.catalog.len(), 1);
    assert!(tool.chapter_library_selection().is_empty());
    assert!(
        tool.chapter.catalog.index_of("mark-1").is_some(),
        "the discovered mark is still the chapter's"
    );
}

/// An entry a chapter-DISCOVERED mark was saved to is already in the chapter under a
/// `mark-{n}` kind id. It is therefore not selectable — a second load would give the
/// catalog two kinds for one physical mark — and not unselectable either, because the
/// mark it names cost a scan to measure.
#[test]
fn a_mark_saved_to_the_library_is_already_in_the_chapter() {
    let mut tool = WatermarkRemovalTool::default();
    tool.chapter.catalog = catalog_with_library_and_discovered("entry-a", "mark-1");
    // «В библиотеку» on the discovered mark: the kind id stays `mark-1`.
    tool.chapter.catalog.marks[1].library_entry = Some("entry-b".to_string());

    assert_eq!(tool.chapter_library_selection(), vec!["entry-a".to_string()]);
    assert_eq!(tool.chapter_saved_entries(), vec!["entry-b".to_string()]);
    assert_eq!(
        tool.chapter_entry_index("entry-b"),
        Some(1),
        "the entry is already represented, so a load must be refused"
    );
    assert_eq!(tool.chapter_entry_index("entry-a"), Some(0));
    assert_eq!(tool.chapter_entry_index("entry-c"), None);

    // And the mark itself is not the library card's to remove.
    tool.unload_library_entry("entry-b");
    assert_eq!(
        tool.chapter.catalog.len(),
        2,
        "a mark the chapter discovered survives a click on the card it was saved to"
    );
}

/// Exactly one canvas reservation is live at a time, so «+ новый» and «+ Выделить новый»
/// are mutually exclusive without either knowing the other exists — and cancelling clears
/// it, while choosing which entry the chapter hunts leaves it alone.
#[test]
fn exactly_one_canvas_reservation_is_live() {
    let mut arm = next_library_arm(None, &LibraryPanelRequest::Arm("entry-b".to_string()));
    assert_eq!(arm, Some("entry-b".to_string()));

    arm = next_library_arm(
        arm,
        &LibraryPanelRequest::Arm("entry-a".to_string()),
    );
    assert_eq!(
        arm,
        Some("entry-a".to_string()),
        "arming a second purpose replaces the first instead of adding to it"
    );

    arm = next_library_arm(
        arm,
        &LibraryPanelRequest::SelectEntry("entry-b".to_string()),
    );
    assert_eq!(arm, Some("entry-a".to_string()));

    arm = next_library_arm(arm, &LibraryPanelRequest::Disarm);
    assert_eq!(arm, None);
    assert_eq!(next_library_arm(None, &LibraryPanelRequest::Disarm), None);
}

/// A reservation and the region base's DIVERSION are one fact: while a crop is reserved,
/// the next completed drag belongs to the library and must not run the region editor.
#[test]
fn a_reservation_diverts_the_next_selection_and_letting_it_go_undoes_that() {
    let mut tool = WatermarkRemovalTool {
        library_arm: Some("entry-b".to_string()),
        ..Default::default()
    };
    tool.sync_selection_diversion();
    tool.region_base.park_diverted_selection(
        3,
        OverlayRectPx {
            x: 8,
            y: 9,
            w: 40,
            h: 50,
        },
    );
    let (page_idx, rect) = tool
        .region_base
        .take_diverted_selection()
        .expect("an armed drag must be parked for the tool, not loaded into the editor");
    assert_eq!(page_idx, 3);
    assert_eq!((rect.x, rect.y, rect.w, rect.h), (8, 9, 40, 50));
    assert!(
        !tool.region_base.has_open_editor(),
        "an armed drag must not open the region editor"
    );

    tool.library_arm = None;
    tool.sync_selection_diversion();
    tool.region_base.set_selection_diverted(true);
    tool.region_base.park_diverted_selection(
        3,
        OverlayRectPx {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
        },
    );
    tool.sync_selection_diversion();
    assert!(
        tool.region_base.take_diverted_selection().is_none(),
        "with no reservation live the diversion is off and any parked rect is dropped"
    );
}

/// THE FIRST HALF OF THE BUG: while a reservation is armed the tool must accept a PLAIN
/// drag, and the hint the user is reading must say so.
///
/// The selection-mode-only gate made an armed drag a complete no-op — no rubber band, no pan
/// (the canvas pans only with Space held), no message — and the one string that named the
/// gesture was painted in a different dock tab from the library panel the user was reading.
#[test]
fn an_armed_tool_accepts_a_plain_drag_and_says_which_gesture_it_wants() {
    let _guard = locale_guard();
    let labels = Arc::new(CleaningLabels::from_registry(&crate::input::test_support::registry()));
    let mut tool = WatermarkRemovalTool {
        library_arm: Some("entry-b".to_string()),
        ..Default::default()
    };
    tool.set_input_labels(&labels);
    tool.sync_selection_diversion();
    assert!(
        tool.input_contexts().contains(&crate::input::REGION_ARMED),
        "the button the user just pressed IS the statement of intent"
    );
    assert_eq!(
        tool.region_base.selection_hint(),
        tf!("cleaning.region.select_armed_gesture_hint", armed = labels.select_rect_armed, gesture = labels.select_rect),
        "the tool's own hint must name the gesture accepted in this state"
    );
    let gesture = tf!("cleaning.tools.watermark.chapter.library_arm_select_hint", armed = labels.select_rect_armed, select_mode = labels.select_mode);
    assert!(gesture.contains("LMB drag") && gesture.contains("Shift"), "{gesture}");

    tool.library_arm = None;
    tool.sync_selection_diversion();
    assert!(
        !tool.input_contexts().contains(&crate::input::REGION_ARMED),
        "with nothing armed the selection-mode gate stands, as it does for every other tool"
    );
    assert_eq!(tool.region_base.selection_hint(), tf!("cleaning.region.select_gesture_hint", gesture = labels.select_rect));
}

/// A parked rect that reaches the tool with NO reservation must be REPORTED, not dropped.
///
/// Defensive: every path that clears `library_arm` also calls `sync_selection_diversion`,
/// which drops the parked rect with it, so this state means one of them stopped keeping
/// that promise. Dropping the user's gesture without a word looks exactly like a drag that
/// did nothing at all — which is the whole class of defect this change closes.
#[test]
fn a_parked_rect_with_no_reservation_is_reported_instead_of_dropped() {
    let _guard = locale_guard();
    let mut tool = WatermarkRemovalTool {
        library_arm: Some("entry-a".to_string()),
        ..Default::default()
    };
    tool.sync_selection_diversion();
    let rect = OverlayRectPx {
        x: 5,
        y: 6,
        w: 32,
        h: 24,
    };
    tool.region_base.park_diverted_selection(2, rect);
    // The reservation dies without the diversion being re-synced — the state this branch
    // exists to catch.
    tool.library_arm = None;

    assert!(
        tool.take_reservation_for_diverted(2, rect).is_none(),
        "there is no reservation to take"
    );
    assert!(
        tool.library_window.status().is_some(),
        "an orphan rect must reach the user, not the void"
    );
}

/// A reservation whose screen stopped being drawn is dropped — and SAYS SO.
///
/// The red «Отменить выделение» is the only way to call a reservation off, so one drawn on
/// no screen is one the user cannot cancel. Dropping it silently left the next drag opening
/// the ordinary region editor with nothing having explained why.
#[test]
fn a_reservation_whose_screen_is_gone_is_dropped_with_a_reason() {
    let _guard = locale_guard();
    let mut tool = WatermarkRemovalTool {
        library_arm: Some("entry-a".to_string()),
        ..Default::default()
    };
    tool.sync_selection_diversion();
    // The panel is closed, so `arm_survives` is false: no screen draws the cancel button.
    assert!(!tool.library_window.is_open());

    tool.drop_reservation_without_a_screen();

    assert!(tool.library_arm.is_none());
    assert!(
        tool.library_window.status().is_some(),
        "a reservation that dies for a reason the user did not choose must be reported"
    );
}

/// The ORDERING TRAP: `CleaningTabState::activate_tool` calls `finish_stroke()` before
/// `deactivate()`, so switching tools in the middle of an ARMED drag completes a diverted
/// selection one statement before the reservation dies. Neither the rect nor a capture may
/// survive that — a rect kept here would be cut out the next time the tool is armed.
#[test]
fn a_tool_switch_mid_armed_drag_strands_neither_the_rect_nor_a_capture() {
    let mut tool = WatermarkRemovalTool {
        library_arm: Some("entry-a".to_string()),
        ..Default::default()
    };
    let mut canvas = CanvasView::default();
    tool.sync_selection_diversion();
    // `finish_stroke` -> `stroke_end` -> the base parks the rect, all BEFORE `deactivate`.
    tool.region_base.park_diverted_selection(
        1,
        OverlayRectPx {
            x: 2,
            y: 2,
            w: 16,
            h: 16,
        },
    );

    tool.deactivate(&mut canvas);

    assert!(tool.library_arm.is_none());
    assert!(
        tool.region_base.take_diverted_selection().is_none(),
        "the rect must die with the reservation that armed it"
    );
    assert!(
        tool.library_capture_rx.is_none() && tool.library_capture_arm.is_none(),
        "no capture may be left in flight by a tool switch"
    );
}

/// A selection in the middle of the page gets its full ring on all four sides, and the
/// margins it reports say so — that is the case every existing entry was cut under, and it
/// must not move because the edge case was added.
#[test]
fn a_capture_away_from_the_edge_keeps_its_full_ring() {
    assert_eq!(
        grow_page_rect(PixelRect::new(10, 10, 20, 20), 4, 100, 100),
        Some(GrownCrop {
            outer: PixelRect::new(6, 6, 28, 28),
            margins: CropMargins::uniform(4),
        })
    );
}

/// A selection with less than a full ring available takes what the page can give instead
/// of being refused. This is the user's real case: a mark stamped flush against the image
/// border has no background on that side and never will, so a symmetric demand made such a
/// mark permanently unsamplable. The per-side margins come back with the crop so the intake
/// derives the footprint that was cut.
#[test]
fn a_capture_at_the_page_edge_takes_the_ring_the_page_can_give() {
    // Tight on the left: 1 px available of the 4 asked for.
    assert_eq!(
        grow_page_rect(PixelRect::new(1, 10, 20, 20), 4, 100, 100),
        Some(GrownCrop {
            outer: PixelRect::new(0, 6, 25, 28),
            margins: CropMargins {
                left: 1,
                top: 4,
                right: 4,
                bottom: 4,
            },
        })
    );
    // The user's own geometry: footprint 319 wide at x=480 on an 800-px page leaves one
    // pixel on the right. It used to be refused outright.
    assert_eq!(
        grow_page_rect(PixelRect::new(480, 200, 319, 236), 4, 800, 1200),
        Some(GrownCrop {
            outer: PixelRect::new(476, 196, 324, 244),
            margins: CropMargins {
                left: 4,
                top: 4,
                right: 1,
                bottom: 4,
            },
        })
    );
    // Flush against the top: nothing above it at all, and that is still a crop.
    assert_eq!(
        grow_page_rect(PixelRect::new(10, 0, 20, 20), 4, 100, 100),
        Some(GrownCrop {
            outer: PixelRect::new(6, 0, 28, 24),
            margins: CropMargins {
                left: 4,
                top: 0,
                right: 4,
                bottom: 4,
            },
        })
    );
}

/// A rect that is not inside the page, or is empty, is still refused: that is a bad
/// selection rather than a tight one, and no amount of clipping makes it a crop.
#[test]
fn a_capture_outside_the_page_is_still_refused() {
    assert_eq!(grow_page_rect(PixelRect::new(90, 10, 20, 20), 4, 100, 100), None);
    assert_eq!(grow_page_rect(PixelRect::new(10, 90, 20, 20), 4, 100, 100), None);
    assert_eq!(grow_page_rect(PixelRect::new(10, 10, 0, 20), 4, 100, 100), None);
    assert_eq!(grow_page_rect(PixelRect::new(10, 10, 20, 0), 4, 100, 100), None);
}

/// A crop whose ring the engine could not MEASURE AT ALL is refused at capture time.
///
/// Stating a level cannot license it — `run_reference_intake` returns `reference_ring_error`
/// whatever is asserted — so offering it the colour control would walk the user through
/// stating a background, committing, and losing the crop. The refusal also happens before
/// the scratch PNG is written, so nothing is left behind to leak.
#[test]
fn a_crop_with_no_measurable_ring_is_refused_instead_of_offered_a_colour() {
    let _guard = locale_guard();
    let (path, rects) = write_synthetic_page("capture-ring");
    let scratch_before = count_scratch_crops();
    let request = LibraryCaptureRequest {
        page: ChapterPageTask {
            page_idx: 0,
            path: path.clone(),
            overlay_size: Some([200, 600]),
        },
        rect: rects[0],
        // A ring requirement no crop of this page can meet: the measurement then reports
        // `RingTooSmall`, which is the verdict under test.
        sample_params: SampleParams {
            min_ring_pixels: usize::MAX,
            ..SampleParams::default()
        },
        max_side: CHAPTER_MAX_TEMPLATE_SIDE,
    };
    let error = run_library_capture(request).expect_err("an unmeasurable ring is refused");
    assert!(
        error.message.contains(&usize::MAX.to_string()),
        "the message must name what the ring needed: {}",
        error.message
    );
    assert_eq!(
        count_scratch_crops(),
        scratch_before,
        "a refused capture writes no scratch file"
    );

    // The same selection with the ordinary tunables still measures, so the refusal is
    // about the ring and not about the selection.
    let ok = run_library_capture(LibraryCaptureRequest {
        page: ChapterPageTask {
            page_idx: 0,
            path: path.clone(),
            overlay_size: Some([200, 600]),
        },
        rect: rects[0],
        sample_params: SampleParams::default(),
        max_side: CHAPTER_MAX_TEMPLATE_SIDE,
    })
    .expect("the synthetic mark sits on flat white");
    assert!(ok.measured.is_some());
    remove_scratch_crop(&ok.path);
    let _ = fs::remove_dir_all(path.parent().expect("temp dir"));
}

/// THE USER'S OWN ENTRY, end to end through the capture lane.
///
/// `wm-1786790637-268d778f`: footprint 319x236, anchor x=480, page width 800. The mark's
/// right edge therefore lands on 799 of an 800-px page and one pixel of background is left
/// where the ring wants four. Every attempt to add a sample to it died here — the capture
/// refused before a crop existed, so the panel had no row to put the reason on and the lane
/// wrote nothing to the log.
///
/// It must now produce a crop whose level is measured, whose margins name the truncated
/// side, and which the intake then accepts.
#[test]
fn the_users_edge_hugging_entry_can_finally_take_a_sample() {
    let _guard = locale_guard();
    let dir = std::env::temp_dir().join("manhwastudio-wm-edge-entry-test");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    // An 800x1200 page whose background is flat and whose mark sits flush against the
    // right border, exactly as `entry.json` records it.
    let footprint = PixelRect::new(480, 200, 319, 236);
    let mut page = RgbaImage::from_pixel(800, 1200, image::Rgba([240, 240, 240, 255]));
    for y in footprint.y..footprint.y + footprint.height {
        for x in footprint.x..footprint.x + footprint.width {
            // Structured, so the ring could never be mistaken for the mark.
            // Cast justification: the product is taken modulo 200, far inside u8.
            let shade = ((x * 7 + y * 13) % 200) as u8;
            page.put_pixel(x, y, image::Rgba([shade, shade, shade, 255]));
        }
    }
    let path = dir.join("page.png");
    page.save(&path).expect("write the page");

    let captured = run_library_capture(LibraryCaptureRequest {
        page: ChapterPageTask {
            page_idx: 0,
            path: path.clone(),
            overlay_size: Some([800, 1200]),
        },
        rect: OverlayRectPx {
            x: footprint.x as usize,
            y: footprint.y as usize,
            w: footprint.width as usize,
            h: footprint.height as usize,
        },
        sample_params: SampleParams::default(),
        max_side: CHAPTER_MAX_TEMPLATE_SIDE,
    })
    .expect("a mark against the page border must be capturable");

    let level = captured.measured.expect("the surviving ring measures the background");
    assert!(
        (level[0] - 240.0).abs() < 0.5,
        "the measured level is the page's own background: {level:?}"
    );
    assert_eq!(
        captured.margins,
        CropMargins {
            left: 4,
            top: 4,
            right: 1,
            bottom: 4,
        },
        "and the crop states the background it actually carries on each side"
    );
    // The crop the intake will read is the footprint plus exactly those margins.
    let crop = image::open(&captured.path).expect("the scratch crop was written").to_rgba8();
    assert_eq!(crop.dimensions(), (319 + 4 + 1, 236 + 4 + 4));

    remove_scratch_crop(&captured.path);
    let _ = fs::remove_dir_all(&dir);
}

/// THE ACCEPTANCE TEST FOR THE CANVAS LANE: the user's own two marks build through
/// `run_library_capture` -> `run_reference_intake`, in either order, on real pixels.
///
/// The file-picker lane already built them; this one did not, and it is the one the user
/// works in. Its input differs in exactly one way — it STATES its `CropMargins` — and that
/// statement used to veto the joint footprint rule outright, so the entry fell back on a
/// footprint shaped like whatever rectangle had been dragged. Two hand-cut crops of one mark
/// are almost never the same rectangle, so the second was refused as too small.
///
/// `test/` is a working directory, not a published fixture path: the test is a no-op when the
/// files are absent and must never fail for a checkout that does not carry them.
#[test]
fn the_users_real_crops_build_both_marks_through_the_canvas_lane() {
    let _guard = locale_guard();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test/водяные метки");
    for (names, level) in [
        (["1_w.png", "1_b.png"], [255u8, 0u8]),
        (["2_w.png", "2_b.png"], [255, 0]),
    ] {
        let paths = names.map(|name| fixtures.join(name));
        if !paths.iter().all(|path| path.is_file()) {
            continue;
        }
        let dir = std::env::temp_dir().join(format!("manhwastudio-wm-canvas-{}", names[0]));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");

        // Each crop pasted into a page of its OWN background, so the drag has real page
        // around it exactly as the user's does. The selection is the crop's own rectangle:
        // the capture lane then grows it by the ring margin and states what it took.
        let mut captured = Vec::new();
        for (index, path) in paths.iter().enumerate() {
            let crop = image::open(path)
                .unwrap_or_else(|err| panic!("{} decodes: {err}", path.display()))
                .to_rgba8();
            let (cw, ch) = crop.dimensions();
            let shade = level[index];
            let mut page = RgbaImage::from_pixel(cw + 200, ch + 200, image::Rgba([shade, shade, shade, 255]));
            for y in 0..ch {
                for x in 0..cw {
                    page.put_pixel(100 + x, 100 + y, *crop.get_pixel(x, y));
                }
            }
            let page_path = dir.join(format!("page-{index}.png"));
            page.save(&page_path).expect("write the page");
            let shot = run_library_capture(LibraryCaptureRequest {
                page: ChapterPageTask {
                    page_idx: 0,
                    path: page_path,
                    overlay_size: Some([(cw + 200) as usize, (ch + 200) as usize]),
                },
                rect: OverlayRectPx {
                    x: 100,
                    y: 100,
                    w: cw as usize,
                    h: ch as usize,
                },
                sample_params: SampleParams::default(),
                max_side: CHAPTER_MAX_TEMPLATE_SIDE,
            })
            .unwrap_or_else(|err| panic!("{} must be capturable: {}", names[index], err.message));
            assert_eq!(
                shot.margins,
                CropMargins::uniform(reference_crop_margin_px(&SampleParams::default())),
                "the selection sits well inside the page, so nothing was clipped"
            );
            captured.push(shot);
        }

        // Both orders, because the whole defect was that the first crop decided the geometry.
        let mut ran = 0usize;
        for order in [[0usize, 1usize], [1, 0]] {
            let files: Vec<PathBuf> = order.iter().map(|&i| captured[i].path.clone()).collect();
            let margins: Vec<Option<CropMargins>> =
                order.iter().map(|&i| Some(captured[i].margins)).collect();
            let label = format!("{:?} via the canvas", order.map(|i| names[i]));
            let outcome = run_reference_intake(ReferenceIntakeRequest {
                files,
                manual_levels: Vec::new(),
                crop_margins: margins,
                sample_params: SampleParams::default(),
                base: None,
                name: "Знак".to_string(),
                max_side: CHAPTER_MAX_TEMPLATE_SIDE,
            })
            .unwrap_or_else(|err| panic!("{label} must build: {}", err.message));
            assert!(
                outcome.conditioning.is_separable(),
                "{label}: white and black separate the model: {:?}",
                outcome.conditioning
            );
            assert_eq!(outcome.request.samples.len(), 2);
            // The footprint is the MARK's, so it is strictly smaller than the SMALLEST of the
            // two selections in both axes — which is what no rule shaped like "cover the first
            // crop" can ever produce for a pair where neither crop covers the other.
            let smallest = paths
                .iter()
                .map(|path| {
                    image::image_dimensions(path).unwrap_or_else(|err| panic!("{} reads: {err}", path.display()))
                })
                .fold((u32::MAX, u32::MAX), |acc, size| (acc.0.min(size.0), acc.1.min(size.1)));
            assert!(
                outcome.request.width < smallest.0 && outcome.request.height < smallest.1,
                "{label}: the footprint {}x{} must be the mark's, not a drag's (smallest crop {smallest:?})",
                outcome.request.width,
                outcome.request.height
            );
            ran += 1;
        }

        assert_eq!(ran, 2, "both orders must have been exercised, not skipped");
        for shot in &captured {
            remove_scratch_crop(&shot.path);
        }
        let _ = fs::remove_dir_all(&dir);
    }
}

/// A chapter mark saved to the library with NO calibration crops is a deliberate write, not
/// an omission: the entry keeps its template and its footprint, carries no model, and is
/// exactly the state the user then adds samples to from the panel.
///
/// The user's own entry was created this way, silently. It stays possible — refusing it
/// would throw away a discovered mark's template — but it is now stated, and logged.
#[test]
fn a_chapter_save_with_no_crops_writes_a_deliberate_empty_sample_list() {
    let _guard = locale_guard();
    // A mark discovered by correlation alone: it has its template crop and nothing else,
    // which is precisely what «В библиотеку» hands over in this case.
    let mut template_crop =
        RgbaImage::from_pixel(SYNTHETIC_MARK_SIDE, SYNTHETIC_MARK_SIDE, image::Rgba([255, 255, 255, 255]));
    stamp_synthetic_mark(&mut template_crop, 0, 0);
    let rect = PixelRect::new(0, 0, SYNTHETIC_MARK_SIDE, SYNTHETIC_MARK_SIDE);
    let kind = WatermarkKind::new(
        "mark-1",
        MarkTemplate::from_page(&template_crop, rect).expect("template"),
        alpha_blend_operator(),
    );
    let mark = ChapterMark::new("Знак".to_string(), template_crop, 200);
    assert!(
        mark.crops.is_empty(),
        "this fixture is the no-calibration case under test"
    );
    let request = build_library_request(&mark, &kind, None);
    assert!(
        request.samples.is_empty(),
        "no crop is invented to fill the list"
    );
    assert!(
        request.template.is_some(),
        "but the discovered template IS kept: it is the mark's identity"
    );
    assert!(
        request.planes.is_none(),
        "and no model is claimed from measurements that were never made"
    );
    assert_eq!(
        request.calibration.verdict, "not_enough_samples",
        "the verdict says what the entry is: {:?}",
        request.calibration.verdict
    );
}

/// Scratch crops of a run that was killed are swept; the ones this process may still be
/// holding never are, and neither is anything that is not ours.
#[test]
fn the_scratch_sweep_takes_only_what_no_run_can_still_be_holding() {
    let pid = 4242;
    let day = Duration::from_secs(24 * 60 * 60);
    assert!(scratch_crop_is_stale(
        "manhwastudio-wm-capture-99-1.png",
        day,
        pid
    ));
    assert!(
        !scratch_crop_is_stale("manhwastudio-wm-capture-99-1.png", Duration::ZERO, pid),
        "another instance may still be holding a fresh crop"
    );
    assert!(
        !scratch_crop_is_stale("manhwastudio-wm-capture-4242-1.png", day * 30, pid),
        "this process's own crops are still reachable from the panel"
    );
    assert!(
        !scratch_crop_is_stale("holiday-photo.png", day * 30, pid),
        "the sweep touches nothing but its own files"
    );
    assert!(
        !scratch_crop_is_stale("manhwastudio-wm-capture-99-1.txt", day * 30, pid),
        "only the crops themselves are swept"
    );
}

/// Scratch crops currently in the system temp directory, whoever wrote them.
fn count_scratch_crops() -> usize {
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(SCRATCH_CROP_PREFIX))
        })
        .count()
}

fn sample_request(
    catalog: ChapterCatalog,
    path: &Path,
    rect: OverlayRectPx,
    target: Option<usize>,
) -> ChapterSampleRequest {
    ChapterSampleRequest {
        catalog,
        target,
        page: ChapterPageTask {
            page_idx: 0,
            path: path.to_path_buf(),
            overlay_size: Some([200, 600]),
        },
        rect,
        settings: WatermarkRemovalSettings::default().normalized(),
        next_number: 1,
    }
}

/// The catalog resolves identity through the engine's signature: a second occurrence of
/// the SAME mark joins the existing entry instead of becoming a second one.
#[test]
fn catalog_matches_a_second_sample_of_the_same_mark() {
    let (path, rects) = write_synthetic_page("match");
    let first = run_chapter_sample(sample_request(
        ChapterCatalog::default(),
        &path,
        rects[0],
        None,
    ))
    .expect("first sample");
    assert_eq!(first.catalog.len(), 1);
    assert_eq!(first.catalog.kinds[0].samples().len(), 1);
    assert_eq!(first.catalog.marks[0].crops.len(), 1);
    // One flat white level: the imprint is exact, the alpha scale is assumed.
    assert!(matches!(
        first.catalog.kinds[0].conditioning(),
        ModelConditioning::DepositExact { .. }
    ));

    let second = run_chapter_sample(sample_request(first.catalog, &path, rects[1], None))
        .expect("second sample");
    assert_eq!(
        second.catalog.len(),
        1,
        "an identical mark must not become a second catalog entry"
    );
    assert_eq!(second.catalog.kinds[0].samples().len(), 2);
    assert_eq!(second.catalog.marks[0].crops.len(), 2);

    // The dark occurrence is added to the SAME mark explicitly, which is what the
    // graded verdict asks for — and it makes the fit separable.
    let third = run_chapter_sample(sample_request(second.catalog, &path, rects[2], Some(0)))
        .expect("third sample");
    assert_eq!(third.catalog.len(), 1);
    assert_eq!(third.catalog.kinds[0].samples().len(), 3);
    assert!(matches!(
        third.catalog.kinds[0].conditioning(),
        ModelConditioning::Separable { .. }
    ));

    // With both levels measured the model is the truth: removal recovers the page.
    let model = third.catalog.kinds[0].model().expect("fitted model");
    let page = decode_chapter_page(&path).expect("decode page");
    for (rect, expected) in [(rects[0], 255u8), (rects[2], 0u8)] {
        let patch = crate::watermark_chapter::remove_occurrence(
            &page,
            PixelRect::new(rect.x as u32, rect.y as u32, SYNTHETIC_MARK_SIDE, SYNTHETIC_MARK_SIDE),
            model,
            crate::watermark_chapter::SubpixelShift::NONE,
        )
        .expect("removal");
        for chunk in patch.pixels.as_chunks::<4>().0 {
            for &value in &chunk[..3] {
                assert!(
                    value.abs_diff(expected) <= 1,
                    "recovered {value} instead of {expected}"
                );
            }
        }
    }

    let _ = fs::remove_dir_all(path.parent().expect("temp dir"));
}

/// A selection whose ring is not uniform is refused as a CALIBRATION target and kept as
/// a detection template — the plan's explicit requirement.
#[test]
fn a_structured_ring_is_refused_as_calibration() {
    let (path, _) = write_synthetic_page("ring");
    // This rect straddles the white/black boundary, so its ring is not uniform.
    let straddling = OverlayRectPx {
        x: 100,
        y: 340,
        w: SYNTHETIC_MARK_SIDE as usize,
        h: SYNTHETIC_MARK_SIDE as usize,
    };
    let outcome = run_chapter_sample(sample_request(
        ChapterCatalog::default(),
        &path,
        straddling,
        None,
    ))
    .expect("template-only sample");
    assert_eq!(outcome.catalog.len(), 1);
    assert_eq!(
        outcome.catalog.kinds[0].samples().len(),
        0,
        "a structured ring must not reach the estimator"
    );
    assert_eq!(outcome.catalog.marks[0].template_only, 1);
    let verdict = outcome.catalog.marks[0]
        .last_verdict
        .as_ref()
        .expect("verdict");
    assert!(!verdict.is_calibration());
    assert!(verdict.usable_as_template());
    let _ = fs::remove_dir_all(path.parent().expect("temp dir"));
}

/// A selection larger than the chapter template limit is refused with the limit named.
#[test]
fn an_oversized_selection_is_refused() {
    let _guard = locale_guard();
    let (path, _) = write_synthetic_page("oversized");
    let huge = OverlayRectPx {
        x: 0,
        y: 0,
        w: 200,
        h: 600,
    };
    let error = run_chapter_sample(sample_request(ChapterCatalog::default(), &path, huge, None))
        .expect_err("an oversized selection must be refused");
    assert!(
        error.contains(&CHAPTER_MAX_TEMPLATE_SIDE.to_string()),
        "the message must name the limit: {error}"
    );
    let _ = fs::remove_dir_all(path.parent().expect("temp dir"));
}

/// The graded verdict renders as four lines: what was measured, the levels, what the
/// alpha SCALE is worth, and the concrete sample that would collapse it.
#[test]
fn graded_verdict_renders_levels_and_the_fix() {
    let _guard = locale_guard();
    let graded = ModelConditioning::DepositExact {
        levels: vec![255.0],
        spread: 0.0,
        samples: 3,
        alpha: AlphaUncertainty::from_percent(AlphaSource::Assumed, 30.0),
    };
    let lines = describe_conditioning(&graded, false);
    assert_eq!(lines.len(), 4);
    // The imprint is what is measured exactly — never «c».
    assert_eq!(
        lines[0],
        t!("cleaning.tools.watermark.chapter.verdict_deposit_exact")
    );
    assert!(lines[1].contains("255"), "the measured level must be shown: {}", lines[1]);
    assert!(lines[2].contains("30"), "the alpha scale bound must be shown: {}", lines[2]);
    // Samples on white: the fix is a DARKER one, and it is named.
    assert!(
        lines[3].contains("191"),
        "the suggested background must be named: {}",
        lines[3]
    );

    // A refusal produces no alpha line, but still names the fix.
    let refused = ModelConditioning::NotEnoughSamples { have: 0, need: 2 };
    let lines = describe_conditioning(&refused, false);
    assert_eq!(lines.len(), 3);
    assert_eq!(
        lines[1],
        t!("cleaning.tools.watermark.chapter.levels_none")
    );
}

/// The quantization report must never read as a quality score, and must carry the two
/// numbers that are genuinely informative.
#[test]
fn residual_is_reported_as_quantization_and_clipping() {
    let _guard = locale_guard();
    let mut residual = RemovalResidual::default();
    residual.mark_pixels = 200;
    residual.exact_pixels = 150;
    residual.clipped_pixels = 10;
    residual.max_uncertainty_lsb = 0.61;
    let line = describe_residual(&residual);
    assert!(line.contains("75.0"), "exact share missing: {line}");
    assert!(line.contains("5.0"), "clipped share missing: {line}");
    assert!(line.contains("0.61"), "uncertainty missing: {line}");
}

/// Installs the embedded English catalog for a test that asserts on rendered text.
///
/// Without an installed catalog `t!`/`tf!` return the key verbatim, so a message test
/// would pass on a template that never interpolates. The returned guard is held for the
/// test's lifetime because the active locale is process-global.
fn locale_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = ms_config::locale_store::GLOBAL_LOCALE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
    ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");
    guard
}

/// `OverlayRectPx` carries no `PartialEq`, so its fields are compared explicitly.
fn assert_overlay_rect(actual: OverlayRectPx, expected: [usize; 4]) {
    assert_eq!(
        [actual.x, actual.y, actual.w, actual.h],
        expected,
        "unexpected overlay rect"
    );
}

/// Patch geometry: overlay and page grids agree when they are the same size, scale
/// proportionally when they are not, and degenerate rects are refused instead of being
/// clamped into a one-pixel write.
#[test]
fn patch_geometry_maps_between_overlay_and_page() {
    let identity = overlay_rect_to_page(
        OverlayRectPx {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        },
        [200, 600],
        200,
        600,
    )
    .expect("identity mapping");
    assert_eq!(identity, PixelRect::new(10, 20, 30, 40));
    assert_overlay_rect(
        page_rect_to_overlay(identity, [200, 600], [200, 600]).expect("back"),
        [10, 20, 30, 40],
    );

    // Half-size overlay: the rect doubles on the page and halves on the way back.
    let scaled = overlay_rect_to_page(
        OverlayRectPx {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        },
        [100, 300],
        200,
        600,
    )
    .expect("scaled mapping");
    assert_eq!(scaled, PixelRect::new(20, 40, 60, 80));
    assert_overlay_rect(
        page_rect_to_overlay(scaled, [200, 600], [100, 300]).expect("back"),
        [10, 20, 30, 40],
    );

    // Degenerate and out-of-page inputs yield nothing at all.
    assert!(
        overlay_rect_to_page(
            OverlayRectPx {
                x: 10,
                y: 20,
                w: 0,
                h: 40
            },
            [200, 600],
            200,
            600
        )
        .is_none()
    );
    assert!(
        overlay_rect_to_page(
            OverlayRectPx {
                x: 10,
                y: 20,
                w: 30,
                h: 40
            },
            [0, 600],
            200,
            600
        )
        .is_none()
    );
    assert!(page_rect_to_overlay(PixelRect::new(0, 0, 1, 1), [200, 600], [0, 0]).is_none());
}

/// The library request carries the calibration verdict, the search metadata and the
/// crops — and the crops alone reconstruct the same model, which is the contract that
/// makes a stored entry usable without any calibration.
#[test]
fn library_request_reconstructs_the_same_model() {
    let (path, rects) = write_synthetic_page("library");
    let mut catalog = ChapterCatalog::default();
    for (index, rect) in rects.iter().enumerate() {
        let target = (index > 0).then_some(0);
        catalog = run_chapter_sample(sample_request(catalog, &path, *rect, target))
            .expect("sample")
            .catalog;
    }
    let mark = &catalog.marks[0];
    let kind = &catalog.kinds[0];
    let request = build_library_request(
        mark,
        kind,
        Some(StoredSourceRef {
            source_key: "series".to_string(),
            page_width: 200,
            anchor_key: kind.template().anchor_key(),
            variant_id: kind.id().to_string(),
            chapter: Some("ch01".to_string()),
        }),
    );
    assert_eq!(request.calibration.verdict, "separable");
    assert_eq!(
        request.calibration.fit_method.as_deref(),
        Some("closed_form_flat")
    );
    assert_eq!(request.calibration.levels.len(), 2);
    assert_eq!(request.samples.len(), 3);
    let request_template = request.template.clone().expect("a chapter mark always has one");
    assert_eq!(
        request_template.dimensions(),
        (SYNTHETIC_MARK_SIDE, SYNTHETIC_MARK_SIDE)
    );
    let planes = request.planes.as_ref().expect("a fitted model has planes");

    // Rebuild the kind the way `run_chapter_library_load` does — from the crops only.
    let rect = PixelRect::new(0, 0, request.width, request.height);
    let template = MarkTemplate::from_page(&request_template, rect).expect("template");
    let mut restored = WatermarkKind::new("restored", template, alpha_blend_operator());
    for sample in &request.samples {
        let sample_rect = PixelRect::new(0, 0, sample.image.width(), sample.image.height());
        restored
            .add_sample(
                CalibrationSample::from_page(
                    &sample.image,
                    0,
                    sample_rect,
                    engine_background(sample.background),
                )
                .expect("calibration sample"),
            )
            .expect("add sample");
    }
    refit_chapter_kind(&mut restored).expect("refit");
    let restored_model = restored.model().expect("restored model");
    for (a, b) in restored_model.c().iter().zip(planes.c.iter()) {
        assert!((a - b).abs() <= 0.01, "c drifted: {a} vs {b}");
    }
    for (a, b) in restored_model.s().iter().zip(planes.s.iter()) {
        assert!((a - b).abs() <= 0.0001, "s drifted: {a} vs {b}");
    }
    let _ = fs::remove_dir_all(path.parent().expect("temp dir"));
}

/// Turns a save request into the entry a loader would hand back, without touching the
/// installation's own library.
fn loaded_entry(id: &str, request: &SaveEntryRequest) -> LoadedEntry {
    LoadedEntry {
        meta: super::super::watermark_library::EntryFile {
            format: super::super::watermark_library::WATERMARK_LIBRARY_FORMAT,
            id: id.to_string(),
            name: request.name.clone(),
            created_unix: 1,
            updated_unix: 2,
            operator: request.operator.clone(),
            width: request.width,
            height: request.height,
            anchors: request.anchors.clone(),
            anchor_key: request.anchor_key.clone(),
            alpha_assumption: request.alpha_assumption,
            signature: request.signature,
            calibration: request.calibration.clone(),
            sources: Vec::new(),
            samples: Vec::new(),
            template: Some("template.png".to_string()),
            planes: None,
        },
        template: request.template.clone(),
        samples: request.samples.clone(),
    }
}

/// The listing row of an entry built from `request`, optionally with its deposit scaled
/// so it describes a DIFFERENT mark that happens to share the artwork.
fn library_row(id: &str, request: &SaveEntryRequest, deposit_scale: f32) -> EntrySummary {
    let signature = request.signature.map(|signature| StoredSignature {
        mean_deposit: signature.mean_deposit * deposit_scale,
        ..signature
    });
    EntrySummary {
        has_template: true,
        partial_rings: 0,
        id: id.to_string(),
        name: id.to_string(),
        width: request.width,
        height: request.height,
        anchor_key: request.anchor_key.clone(),
        verdict: request.calibration.verdict.clone(),
        levels: request.calibration.levels.clone(),
        spread: request.calibration.spread,
        samples: request.samples.len(),
        alpha: request.calibration.alpha.clone(),
        fit_method: request.calibration.fit_method.clone(),
        signature,
        sources: Vec::new(),
        updated_unix: 2,
        format: 1,
        clamped_pixels: request.calibration.clamped_pixels,
        manual_backgrounds: Vec::new(),
    }
}

/// Auto-match end to end: a chapter that measured only ONE background level adopts the
/// library entry that carries the same mark and becomes exact, while an entry whose
/// artwork is identical but whose deposit is not never even enters the candidate list.
/// The override then hands the chapter its own measurements back.
#[test]
fn auto_match_adopts_the_entry_that_carries_this_mark() {
    let (path, rects) = write_synthetic_page("automatch");

    // The library entry: all three occurrences, so the fit separated `c` from `s`.
    let mut exact = ChapterCatalog::default();
    for (index, rect) in rects.iter().enumerate() {
        let target = (index > 0).then_some(0);
        exact = run_chapter_sample(sample_request(exact, &path, *rect, target))
            .expect("sample")
            .catalog;
    }
    let entry_request = build_library_request(&exact.marks[0], &exact.kinds[0], None);
    assert_eq!(entry_request.calibration.verdict, "separable");
    let entry = loaded_entry("wm-exact", &entry_request);

    // The open chapter: one white occurrence only, so its own fit is the graded one.
    let graded = run_chapter_sample(sample_request(
        ChapterCatalog::default(),
        &path,
        rects[0],
        None,
    ))
    .expect("sample")
    .catalog;
    let mut catalog = graded;
    assert!(matches!(
        catalog.kinds[0].conditioning(),
        ModelConditioning::DepositExact { .. }
    ));
    assert_eq!(catalog.marks[0].crops.len(), 1);

    // A decoy sharing the footprint and the artwork but depositing half as much: the
    // measured colour/greyscale twin of the second chapter. It must NOT match.
    let library = vec![
        library_row("wm-twin", &entry_request, 0.5),
        library_row("wm-exact", &entry_request, 1.0),
    ];
    let matched = match_catalog_against_library(&mut catalog, &library, |id| {
        assert_eq!(id, "wm-exact", "the twin must never be loaded");
        Ok(entry.clone())
    });

    assert_eq!(matched, 1);
    assert_eq!(
        catalog.marks[0].matches.len(),
        1,
        "the artwork-identical twin must be filtered out by its deposit"
    );
    assert_eq!(catalog.marks[0].matches[0].entry_id, "wm-exact");
    assert_eq!(catalog.marks[0].adopted_entry.as_deref(), Some("wm-exact"));
    assert!(
        matches!(
            catalog.kinds[0].conditioning(),
            ModelConditioning::Separable { .. }
        ),
        "adopting the entry must make the chapter's mark exact"
    );
    assert_eq!(catalog.marks[0].crops.len(), entry_request.samples.len());

    // The override: back to the chapter's own single measurement, not to nothing.
    let released = run_chapter_use_match(ChapterMatchRequest {
        catalog,
        index: 0,
        entry: None,
    })
    .expect("release");
    assert!(released.catalog.marks[0].adopted_entry.is_none());
    assert_eq!(released.catalog.marks[0].crops.len(), 1);
    assert!(matches!(
        released.catalog.kinds[0].conditioning(),
        ModelConditioning::DepositExact { .. }
    ));

    let _ = fs::remove_dir_all(path.parent().expect("temp dir"));
}
