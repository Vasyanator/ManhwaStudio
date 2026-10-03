/*
FILE OVERVIEW: crates/ms-tab-translation/src/text_detector/tests.rs
Unit and characterization tests of the detector module (all submodules): routing and the
backend gate, the IPC runner's batch sizing and its request/response handling over a fake
transport, the native runner's error mapping, the native -> backend fallback decision, error
texts, the region helpers' conversion / size / dilation contract, the classic pipeline and the
Otsu/dilation adapters.

Notes:
The `characterization_*` expected values were OBSERVED from the code before the detector
Phase 1 refactor and must not be edited to make a change pass. No test needs a backend
process or model files.
*/

use super::*;
use super::backend::{
    DetectorTransport, IPC_MAX_TILES_PER_REQUEST, IpcRunner, ensure_v2_backend_ready, ipc_frame_budget_tiles,
    ipc_max_batch,
};
use super::classic::{classic_otsu_threshold, detect_classic_from_gray, dilate_binary};
#[cfg(not(target_arch = "wasm32"))]
use super::pipeline::detector_native_route;
use super::pipeline::{backend_fallback_applies, ctd_detect_params, detect_error_message, native_applies};
use super::region::{color_image_to_rgb, region_mask};
use crate::backend_health::ai_backend_offline_error;
use eframe::egui;
use image::RgbImage;
use ms_backend_ipc::textdetector::{ForwardEngine, ForwardWireError, decode_forward_response};
use ms_text_detect::mask::{BinaryMask, MaskError};
use ms_text_detect::{
    DetectError, Detection, DetectionStats, PlanError, ProbMapError, ProbMapRunner, RunnerError, plan_detection, run_detection,
};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::num::NonZeroUsize;

// -------------------------------------------------------------------------
// ensure_v2_backend_ready error message
// -------------------------------------------------------------------------

#[test]
fn backend_ready_error_uses_offline_message() {
    // shared_client() will fail (no backend running in tests).
    let result = ensure_v2_backend_ready();
    if let Err(msg) = result {
        assert_eq!(
            msg, ai_backend_offline_error(),
            "offline error must use the canonical constant"
        );
    }
    // If shared_client() somehow succeeds (live backend in CI), that's fine too.
}

// -------------------------------------------------------------------------
// Characterization tests (detector Phase 1 refactor). Every expected value
// below was OBSERVED from the pre-refactor code, never derived by hand: they
// pin today's behavior so the move of sort/truncate/Otsu/dilation/mask
// normalization onto single owners can prove it changed nothing.
// -------------------------------------------------------------------------

/// FNV-1a 64 over `bytes`: a compact, dependency-free fingerprint for pinned buffers.
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Deterministic 64-bit LCG (Knuth MMIX constants); returns the high 31 bits.
fn lcg_next(state: &mut u64) -> u32 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    // `>> 33` leaves 31 significant bits, so the conversion cannot fail.
    u32::try_from(*state >> 33).unwrap_or(0)
}

/// Fingerprint of a rect list: FNV over the little-endian bits of every coordinate.
fn rects_fingerprint(rects: &[TextDetectorRect]) -> u64 {
    let bytes = rects
        .iter()
        .flat_map(|r| [r.x1, r.y1, r.x2, r.y2])
        .flat_map(f32::to_le_bytes)
        .collect::<Vec<_>>();
    fnv1a64(&bytes)
}

fn rects_as_arrays(rects: &[TextDetectorRect]) -> Vec<[f32; 4]> {
    rects.iter().map(|r| [r.x1, r.y1, r.x2, r.y2]).collect()
}

/// A random mask of `width*height` bytes in which about `percent`% of the
/// pixels are set; set pixels use the mixed values 1/7/255 so the tests pin
/// the "any nonzero counts as set" input rule.
fn random_mask(width: usize, height: usize, seed: u64, percent: u32) -> Vec<u8> {
    let mut state = seed;
    (0..width * height)
        .map(|_| {
            let roll = lcg_next(&mut state);
            if roll % 100 < percent {
                [1u8, 7, 255][usize::try_from(roll % 3).unwrap_or(0)]
            } else {
                0
            }
        })
        .collect()
}

/// Synthetic grayscale "page": a noisy light background (225..=255) with
/// three lines of hollow dark glyphs (2*unit stroke), one solid dark block
/// (rejected by the density rule) and one speck (rejected by the area rule).
/// `unit` scales every geometric feature so a large page keeps the same layout.
fn synthetic_text_page(width: u32, height: u32, unit: u32, seed: u64) -> image::GrayImage {
    let mut state = seed;
    let mut img = image::GrayImage::from_fn(width, height, |_, _| {
        image::Luma([225 + u8::try_from(lcg_next(&mut state) % 31).unwrap_or(0)])
    });
    let mut dark = |img: &mut image::GrayImage, x0: u32, y0: u32, x1: u32, y1: u32| {
        for y in y0..y1.min(height) {
            for x in x0..x1.min(width) {
                img.put_pixel(x, y, image::Luma([u8::try_from(lcg_next(&mut state) % 41).unwrap_or(0)]));
            }
        }
    };
    let (glyph_w, glyph_h, stroke, pitch) = (10 * unit, 14 * unit, 2 * unit, 13 * unit);
    for (line, glyphs) in [(0u32, 12u32), (1, 9), (2, 15)] {
        let y0 = 30 * unit + line * 45 * unit;
        for glyph in 0..glyphs {
            let x0 = 20 * unit + glyph * pitch;
            dark(&mut img, x0, y0, x0 + glyph_w, y0 + stroke);
            dark(&mut img, x0, y0 + glyph_h - stroke, x0 + glyph_w, y0 + glyph_h);
            dark(&mut img, x0, y0, x0 + stroke, y0 + glyph_h);
            dark(&mut img, x0 + glyph_w - stroke, y0, x0 + glyph_w, y0 + glyph_h);
        }
    }
    dark(&mut img, 260 * unit, 200 * unit, 330 * unit, 260 * unit);
    dark(&mut img, 40 * unit, 250 * unit, 42 * unit, 252 * unit);
    img
}

/// Prints the observed values of a classic result in the form the
/// characterization assertions use (shown only when an assertion fails).
fn describe_classic(result: &TextDetectorPageResult) -> String {
    format!(
        "source_size={:?} mask_size={:?} mask_len={} mask_set={} mask_fnv={:#018x} blocks={:?}",
        result.source_size,
        result.mask_size,
        result.mask_alpha.len(),
        result.mask_alpha.iter().filter(|&&px| px == 255).count(),
        fnv1a64(&result.mask_alpha),
        rects_as_arrays(&result.blocks),
    )
}

/// characterization: classic detection on a 400x300 text-like page (no
/// downscale): exact block rects and the mask fingerprint.
#[test]
fn characterization_classic_text_page_rects_and_mask() {
    let result = detect_classic_from_gray(synthetic_text_page(400, 300, 1, 0x5eed_0001));
    let observed = describe_classic(&result);
    assert_eq!(result.source_size, [400, 300], "{observed}");
    assert_eq!(result.mask_size, [400, 300], "{observed}");
    // At this size the (2, 1) dilation merges each glyph line into one block.
    assert_eq!(
        rects_as_arrays(&result.blocks),
        vec![[18.0, 29.0, 175.0, 45.0], [18.0, 74.0, 136.0, 90.0], [18.0, 119.0, 214.0, 135.0]],
        "{observed}"
    );
    assert_eq!(result.mask_alpha.iter().filter(|&&px| px == 255).count(), 6960, "{observed}");
    assert_eq!(fnv1a64(&result.mask_alpha), 0x19ef_945a_7765_03c5, "{observed}");
}

/// characterization: classic detection on a 2000x1800 page, which exercises
/// the 1600 downscale (Triangle), the scale-back of rects and the nearest
/// upscale of the mask to source size.
#[test]
fn characterization_classic_downscaled_page_rects_and_mask() {
    let result = detect_classic_from_gray(synthetic_text_page(2000, 1800, 5, 0x5eed_0002));
    let observed = describe_classic(&result);
    assert_eq!(result.source_size, [2000, 1800], "{observed}");
    assert_eq!(result.mask_size, [2000, 1800], "{observed}");
    // At the 1600x1440 working size the glyph gaps survive dilation, so each
    // hollow glyph is its own block (12 + 9 + 15), scaled back by 1.25.
    let blocks = rects_as_arrays(&result.blocks);
    assert_eq!(blocks.len(), 36, "{observed}");
    assert_eq!(blocks[0], [97.5, 148.75, 152.5, 221.25], "{observed}");
    assert_eq!(blocks[12], [97.5, 373.75, 152.5, 446.25], "{observed}");
    assert_eq!(blocks[35], [1007.5, 598.75, 1062.5, 671.25], "{observed}");
    assert_eq!(rects_fingerprint(&result.blocks), 12_158_377_757_990_131_416, "{observed}");
    assert_eq!(result.mask_alpha.iter().filter(|&&px| px == 255).count(), 99_359, "{observed}");
    assert_eq!(fnv1a64(&result.mask_alpha), 0xf046_7c19_cf01_8f8a, "{observed}");
}

/// characterization: classic detection on a blank page finds nothing and
/// returns an all-zero mask at source size.
#[test]
fn characterization_classic_empty_page() {
    let result = detect_classic_from_gray(image::GrayImage::from_pixel(64, 48, image::Luma([255])));
    let observed = describe_classic(&result);
    assert_eq!(result.source_size, [64, 48], "{observed}");
    assert_eq!(result.mask_size, [64, 48], "{observed}");
    assert!(result.blocks.is_empty(), "{observed}");
    assert_eq!(result.mask_alpha, vec![0u8; 64 * 48], "{observed}");
}

/// Fixed Otsu input vectors shared by the classic and glyph-mask Otsu
/// characterization tests (the glyph-mask caller lives in `ms-text-detect`; its
/// test rebuilds the same vectors).
fn otsu_vectors() -> Vec<(&'static str, Vec<u8>)> {
    let mut state = 0x0750_u64;
    let random = (0..1000).map(|_| u8::try_from(lcg_next(&mut state) % 256).unwrap_or(0)).collect();
    let mut bimodal = vec![20u8; 50];
    bimodal.extend(std::iter::repeat_n(200u8, 50));
    let mut three_level = vec![10u8; 30];
    three_level.extend(std::iter::repeat_n(128u8, 40));
    three_level.extend(std::iter::repeat_n(240u8, 30));
    vec![
        ("empty", Vec::new()),
        ("single_value", vec![77u8; 10]),
        ("two_adjacent", vec![100, 101, 100, 101]),
        ("bimodal", bimodal),
        ("three_level", three_level),
        ("extremes", vec![0, 255]),
        ("random_1000", random),
    ]
}

/// characterization: the classic Otsu adapter on fixed histograms. When no
/// split exists ("empty" and "single_value") it returns its default 127, while the
/// glyph-mask caller defaults to 0; every other vector agrees between the callers.
#[test]
fn characterization_classic_otsu_fixed_histograms() {
    let observed = otsu_vectors()
        .into_iter()
        .map(|(name, values)| (name, classic_otsu_threshold(&values)))
        .collect::<Vec<_>>();
    assert_eq!(
        observed,
        vec![
            ("empty", 127),
            ("single_value", 127),
            ("two_adjacent", 100),
            ("bimodal", 20),
            ("three_level", 10),
            ("extremes", 0),
            ("random_1000", 129),
        ],
        "observed={observed:?}"
    );
}

/// characterization: `dilate_binary` (output 0/1) at r = 0, 1, 3 and the
/// classic anisotropic (2, 1), on a random mixed-value mask.
#[test]
fn characterization_dilate_binary_random_masks() {
    let (width, height) = (37usize, 23usize);
    let src = random_mask(width, height, 0xd11a_7e01, 8);
    let observed = [(0usize, 0usize), (1, 1), (3, 3), (2, 1)]
        .into_iter()
        .map(|(rx, ry)| {
            let out = dilate_binary(&src, width, height, rx, ry).expect("mask length matches its size");
            (rx, ry, out.iter().filter(|&&px| px == 1).count(), fnv1a64(&out))
        })
        .collect::<Vec<_>>();
    // (rx, ry, set count, FNV of the 0/1 output)
    assert_eq!(
        observed,
        vec![
            (0, 0, 64, 9_260_574_274_878_741_243),
            (1, 1, 407, 12_066_787_267_297_046_334),
            (3, 3, 834, 13_298_246_393_450_976_053),
            (2, 1, 581, 2_458_741_469_921_375_552),
        ],
        "observed={observed:?}"
    );
    assert!(dilate_binary(&[], 0, 0, 1, 1).expect("empty mask").is_empty());
}

/// characterization: `dilate_mask_alpha` (the 0/255 wrapper used for every
/// detector mode) at sizes 0, 1, 3 and the clamp at 31 -> 30.
#[test]
fn characterization_dilate_mask_alpha_random_mask() {
    let size = [41u32, 29u32];
    let src = random_mask(41, 29, 0xd11a_7e02, 5)
        .into_iter()
        .map(|px| if px == 0 { 0 } else { 255 })
        .collect::<Vec<_>>();
    let observed = [0i32, 1, 3, 31, -4]
        .into_iter()
        .map(|dilate_size| {
            let mut alpha = src.clone();
            dilate_mask_alpha(&mut alpha, size, dilate_size);
            (dilate_size, alpha.iter().filter(|&&px| px == 255).count(), fnv1a64(&alpha))
        })
        .collect::<Vec<_>>();
    // (dilate_size, set count, FNV of the 0/255 output); -4 clamps to 0 (no-op).
    assert_eq!(
        observed,
        vec![
            (0, 68, 6_807_247_926_453_094_363),
            (1, 500, 7_778_443_962_078_020_797),
            (3, 1146, 6_986_607_969_027_450_885),
            (31, 1189, 10_146_146_705_198_328_074),
            (-4, 68, 6_807_247_926_453_094_363),
        ],
        "observed={observed:?}"
    );
}

// -------------------------------------------------------------------------
// Routing and the backend gate
// -------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn detector_route_native_only_for_native_runtime_and_safe_guard() {
    use ms_config::{AiRuntime, OrtLoadDecision};
    assert_eq!(detector_native_route(AiRuntime::Native, OrtLoadDecision::Safe), DetectorRoute::Native);
    assert_eq!(detector_native_route(AiRuntime::Native, OrtLoadDecision::Suspect), DetectorRoute::Backend);
    assert_eq!(detector_native_route(AiRuntime::Backend, OrtLoadDecision::Safe), DetectorRoute::Backend);
    assert_eq!(detector_native_route(AiRuntime::Backend, OrtLoadDecision::Suspect), DetectorRoute::Backend);
}

/// Only PaddleOCR under the native route tries the in-process runner.
#[test]
fn native_runner_only_for_paddle_on_the_native_route() {
    assert!(native_applies(EngineKind::Paddle, DetectorRoute::Native));
    assert!(!native_applies(EngineKind::Paddle, DetectorRoute::Backend));
    for engine in [EngineKind::Ctd, EngineKind::Surya, EngineKind::Classic] {
        assert!(!native_applies(engine, DetectorRoute::Native), "{engine:?}");
    }
}

/// The batch gate needs the backend for CTD and Surya always, for PaddleOCR only off the
/// native route, and never for Classic.
#[test]
fn backend_gate_follows_mode_and_paddle_route() {
    let paddle = TextDetectorRunMode::PaddleOcr(TextDetectorPaddleOcrOptions::default());
    assert!(!detector_mode_needs_backend(&paddle, DetectorRoute::Native));
    assert!(detector_mode_needs_backend(&paddle, DetectorRoute::Backend));
    for route in [DetectorRoute::Native, DetectorRoute::Backend] {
        assert!(!detector_mode_needs_backend(&TextDetectorRunMode::Classic, route));
        assert!(detector_mode_needs_backend(&TextDetectorRunMode::AiCtd(TextDetectorAiCtdOptions::default()), route));
        assert!(detector_mode_needs_backend(&TextDetectorRunMode::Surya(TextDetectorSuryaOptions), route));
    }
}

/// The CTD detection size reaches the plan; a negative size becomes 0 (clamped by the plan).
#[test]
fn ctd_params_carry_the_detect_size() {
    let mut options = TextDetectorAiCtdOptions::default();
    assert_eq!(ctd_detect_params(&options).ctd_detect_size, 1280);
    options.detect_size = -5;
    assert_eq!(ctd_detect_params(&options).ctd_detect_size, 0);
}

/// Every run mode maps to its engine; only CTD carries non-default params (its detect size).
#[test]
fn run_modes_map_to_their_plan_inputs() {
    assert_eq!(TextDetectorRunMode::Classic.plan_inputs(), (EngineKind::Classic, DetectParams::default()));
    let paddle = TextDetectorRunMode::PaddleOcr(TextDetectorPaddleOcrOptions::default());
    assert_eq!(paddle.plan_inputs(), (EngineKind::Paddle, DetectParams::default()));
    let surya = TextDetectorRunMode::Surya(TextDetectorSuryaOptions);
    assert_eq!(surya.plan_inputs(), (EngineKind::Surya, DetectParams::default()));
    let ctd = TextDetectorRunMode::AiCtd(TextDetectorAiCtdOptions { detect_size: 1000, mask_dilate_size: 0 });
    assert_eq!(ctd.plan_inputs(), (EngineKind::Ctd, DetectParams { ctd_detect_size: 1000 }));
}

// -------------------------------------------------------------------------
// Native -> backend fallback decision and error texts
// -------------------------------------------------------------------------

/// Native-specific failures re-run the page on the backend; failures decided by the shared plan
/// or postprocess do not (the backend would fail the same way).
#[test]
fn backend_fallback_only_for_native_specific_failures() {
    let runner = DetectError::Runner(RunnerError { message: "dylib".to_string() });
    let rerun = [
        runner,
        DetectError::MapCount { expected: 2, got: 1 },
        DetectError::ChannelCount { tile: 0, expected: 1, got: 2 },
        DetectError::MapShape { tile: 0, channel: 0, expected: [32, 32], got: [16, 16] },
        DetectError::Map(ProbMapError::Empty { width: 0, height: 1 }),
    ];
    for err in &rerun {
        assert!(backend_fallback_applies(err), "{err:?}");
    }
    let final_errors = [
        DetectError::Plan(PlanError::EmptyImage { width: 0, height: 3 }),
        DetectError::PageSize { expected: [1, 1], got: [2, 2] },
        DetectError::Unsupported { engine: EngineKind::Classic },
        DetectError::Mask(MaskError::TooLarge { width: 20_000, height: 20_000 }),
    ];
    for err in &final_errors {
        assert!(!backend_fallback_applies(err), "{err:?}");
    }
}

/// Runner messages pass through unchanged; plan and mask limits get their own keys; every
/// technical variant is wrapped with the engine name.
#[test]
fn detect_errors_map_to_localized_texts() {
    let runner = DetectError::Runner(RunnerError { message: "backend said no".to_string() });
    assert_eq!(detect_error_message(EngineKind::Ctd, &runner), "backend said no");
    assert_eq!(
        detect_error_message(EngineKind::Paddle, &DetectError::Plan(PlanError::EmptyImage { width: 0, height: 3 })),
        tf!("translation.text_detector.page_empty_error", w = 0, h = 3)
    );
    let too_large = PlanError::TooLarge { width: 20_000, height: 20_000, max: 100_000_000 };
    assert_eq!(
        detect_error_message(EngineKind::Paddle, &DetectError::Plan(too_large)),
        tf!("translation.text_detector.page_too_large_error", w = 20_000, h = 20_000, max = 100_000_000)
    );
    let mask = DetectError::Mask(MaskError::TooLarge { width: 7, height: 9 });
    assert_eq!(
        detect_error_message(EngineKind::Surya, &mask),
        tf!("translation.text_detector.result_mask_too_large_error", w = 7, h = 9)
    );
    let count = DetectError::MapCount { expected: 2, got: 1 };
    assert_eq!(
        detect_error_message(EngineKind::Surya, &count),
        tf!("translation.text_detector.pipeline_error", engine = "Surya", err = count)
    );
}

/// A native runtime failure reaches the user as its localized `Display`.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn native_runtime_error_becomes_its_display_text() {
    let err = ms_native_runtime::NativeRuntimeError::ModelEnsure("det.onnx missing".to_string());
    assert_eq!(super::native::runner_error_from_native(&err), RunnerError { message: err.to_string() });
}

// -------------------------------------------------------------------------
// IPC runner: batch sizing and the forward exchange over a fake transport
// -------------------------------------------------------------------------

/// Frame budget = 90 % of `MAX_BLOB_BYTES` over the larger direction; the batch is capped at
/// `IPC_MAX_TILES_PER_REQUEST` (4) and never drops below 1.
#[test]
fn ipc_batch_sizing_math() {
    let budget = |engine, side| ipc_frame_budget_tiles(engine, [side, side]).expect("aligned tile");
    // CTD 1280^2: request 4.9 MB (RGB) > response 3.3 MB (2 maps) -> 6 fit, capped to 4.
    assert_eq!(budget(ForwardEngine::Ctd, 1280), 6);
    assert_eq!(ipc_max_batch(ForwardEngine::Ctd, [1280, 1280]).get(), IPC_MAX_TILES_PER_REQUEST);
    // CTD 2048^2: 12.6 MB per tile -> 2, below the cap.
    assert_eq!(budget(ForwardEngine::Ctd, 2048), 2);
    assert_eq!(ipc_max_batch(ForwardEngine::Ctd, [2048, 2048]).get(), 2);
    // Surya 1200^2: 4.3 MB request, quarter-size response -> 6, capped.
    assert_eq!(budget(ForwardEngine::Surya, 1200), 6);
    assert_eq!(ipc_max_batch(ForwardEngine::Surya, [1200, 1200]).get(), 4);
    // Paddle 960^2: 2.8 MB -> 10, capped.
    assert_eq!(budget(ForwardEngine::Paddle, 960), 10);
    assert_eq!(ipc_max_batch(ForwardEngine::Paddle, [960, 960]).get(), 4);
    // A tile the wire refuses (misaligned) yields 1 so the request builder reports it.
    assert!(ipc_frame_budget_tiles(ForwardEngine::Ctd, [100, 64]).is_err());
    assert_eq!(ipc_max_batch(ForwardEngine::Ctd, [100, 64]), NonZeroUsize::MIN);
    // A tile above the whole budget still sends one at a time.
    assert_eq!(ipc_frame_budget_tiles(ForwardEngine::Ctd, [4096, 4096]).expect("aligned tile"), 0);
    assert_eq!(ipc_max_batch(ForwardEngine::Ctd, [4096, 4096]), NonZeroUsize::MIN);
}

/// One recorded request: method, header fields and blob length.
type RecordedCall = (String, Value, usize);

/// The response a fake backend produces for a request.
type FakeResponse = Box<dyn Fn(&Value, &[u8]) -> Result<(Value, Vec<u8>), String>>;

/// In-process stand-in for the backend: records each request and answers with `respond`.
struct FakeTransport {
    calls: RefCell<Vec<RecordedCall>>,
    respond: FakeResponse,
}

impl FakeTransport {
    fn new(respond: impl Fn(&Value, &[u8]) -> Result<(Value, Vec<u8>), String> + 'static) -> Self {
        Self { calls: RefCell::new(Vec::new()), respond: Box::new(respond) }
    }
}

impl DetectorTransport for &FakeTransport {
    fn call(&self, method: &str, header: Value, blob: &[u8]) -> Result<(Value, Vec<u8>), String> {
        self.calls.borrow_mut().push((method.to_string(), header.clone(), blob.len()));
        (self.respond)(&header, blob)
    }
}

/// Reads `n`, `width`, `height` back from a request header built by the runner.
fn request_dims(header: &Value) -> (usize, u32, u32) {
    let field = |key: &str| header.get(key).and_then(Value::as_u64).expect("request header field");
    let n = usize::try_from(field("n")).expect("n fits usize");
    let width = u32::try_from(field("width")).expect("width fits u32");
    let height = u32::try_from(field("height")).expect("height fits u32");
    (n, width, height)
}

/// A well-formed fake Paddle forward: one `prob` map per tile, 255 where the tile pixel is
/// pure red (the test's "text"), 0 elsewhere (padding is black, so it stays 0).
fn fake_paddle_forward(header: &Value, blob: &[u8]) -> Result<(Value, Vec<u8>), String> {
    let (n, width, height) = request_dims(header);
    let maps: Vec<u8> = blob.chunks_exact(3).map(|px| if px[0] > 200 && px[1] < 50 { 255 } else { 0 }).collect();
    let response = json!({"engine": "paddle", "n": n, "map_width": width, "map_height": height, "channels": ["prob"]});
    Ok((response, maps))
}

/// White region with one red "text" bar.
fn red_bar_region(width: u32, height: u32) -> RgbImage {
    RgbImage::from_fn(width, height, |x, y| {
        if (20..70).contains(&x) && (20..32).contains(&y) { image::Rgb([255, 0, 0]) } else { image::Rgb([255, 255, 255]) }
    })
}

/// The runner packs the tiles into one `{n, width, height}` request on the engine's method and
/// splits a valid reply into per-tile channel maps.
#[test]
fn ipc_runner_round_trips_a_valid_forward() {
    let transport = FakeTransport::new(|header, _blob| {
        let (n, width, height) = request_dims(header);
        let plane = usize::try_from(width * height).expect("plane fits usize");
        // Tile t, channel c is filled with the value 10 * t + c.
        let blob = (0..n).flat_map(|tile| (0..2).flat_map(move |channel| std::iter::repeat_n(u8::try_from(10 * tile + channel).unwrap_or(0), plane))).collect();
        Ok((json!({"engine": "ctd", "n": n, "map_width": width, "map_height": height, "channels": ["seg", "shrink"]}), blob))
    });
    let mut runner = IpcRunner::new(ForwardEngine::Ctd, &transport);
    let tiles = vec![RgbImage::new(64, 128); 2];
    let maps = runner.forward(&tiles).expect("valid forward");
    assert_eq!(maps.len(), 2);
    for (tile, tile_maps) in maps.iter().enumerate() {
        assert_eq!(tile_maps.maps.len(), 2);
        for (channel, map) in tile_maps.maps.iter().enumerate() {
            assert_eq!(map.size(), [64, 128]);
            assert!(map.data().iter().all(|&v| usize::from(v) == 10 * tile + channel));
        }
    }
    let calls = transport.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, ms_backend_ipc::protocol::METHOD_TEXTDETECTOR_CTD_FORWARD);
    assert_eq!(calls[0].1, json!({"n": 2, "width": 64, "height": 128}));
    assert_eq!(calls[0].2, 2 * 64 * 128 * 3);
}

/// Every response-contract violation becomes the localized `forward_response_error` carrying the
/// wire error; a transport failure passes through unchanged.
#[test]
fn ipc_runner_rejects_malformed_responses() {
    let tiles = vec![RgbImage::new(32, 32)];
    let good_header = json!({"engine": "paddle", "n": 1, "map_width": 32, "map_height": 32, "channels": ["prob"]});
    let cases = [
        (json!({"engine": "paddle", "n": 2, "map_width": 32, "map_height": 32, "channels": ["prob"]}), vec![0u8; 32 * 32]),
        (json!({"engine": "ctd", "n": 1, "map_width": 32, "map_height": 32, "channels": ["prob"]}), vec![0u8; 32 * 32]),
        (good_header.clone(), vec![0u8; 32 * 32 - 1]),
    ];
    for (header, blob) in cases {
        let expected_wire = decode_forward_response(ForwardEngine::Paddle, 1, 32, 32, &header, blob.clone()).expect_err("malformed");
        let transport = FakeTransport::new(move |_, _| Ok((header.clone(), blob.clone())));
        let err = IpcRunner::new(ForwardEngine::Paddle, &transport).forward(&tiles).expect_err("malformed reply");
        let expected = tf!("translation.text_detector.forward_response_error", engine = "paddle", err = expected_wire);
        assert_eq!(err, RunnerError { message: expected });
    }

    let transport = FakeTransport::new(|_, _| Err("backend exploded".to_string()));
    let err = IpcRunner::new(ForwardEngine::Paddle, &transport).forward(&tiles).expect_err("transport error");
    assert_eq!(err.message, "backend exploded");

    // A request the wire refuses never reaches the transport.
    let transport = FakeTransport::new(|_, _| Err("must not be called".to_string()));
    let err = IpcRunner::new(ForwardEngine::Paddle, &transport).forward(&[RgbImage::new(30, 32)]).expect_err("misaligned tile");
    let wire = ForwardWireError::Misaligned { engine: "paddle", width: 30, height: 32, align: 32 };
    assert_eq!(err.message, tf!("translation.text_detector.forward_response_error", engine = "paddle", err = wire));
    assert!(transport.calls.borrow().is_empty());
}

/// The full pipeline over a fake Paddle backend: blocks in source pixels and a mask exactly the
/// size of the region, both through the region helper (which also dilates).
#[test]
fn region_mask_over_a_fake_backend_has_the_region_size() {
    let transport = FakeTransport::new(fake_paddle_forward);
    let (width, height) = (100_u32, 60_u32);
    let region = egui::ColorImage::from_rgb(
        [100, 60],
        red_bar_region(width, height).as_raw(),
    );
    let mut detection_blocks = Vec::new();
    let (size, alpha) = region_mask(&region, 2, |page| {
        let plan = plan_detection(EngineKind::Paddle, [page.width(), page.height()], &DetectParams::default()).map_err(|err| err.to_string())?;
        let mut runner = IpcRunner::new(ForwardEngine::Paddle, &transport);
        let detection = run_detection(&plan, page, &mut runner).map_err(|err| err.to_string())?;
        detection_blocks.clone_from(&detection.blocks);
        Ok(detection)
    })
    .expect("fake backend detection");
    assert_eq!(size, [width, height]);
    assert_eq!(alpha.len(), 100 * 60);
    assert!(alpha.iter().all(|&px| px == 0 || px == 255));
    assert_eq!(detection_blocks.len(), 1, "{detection_blocks:?}");
    let block = detection_blocks[0];
    assert!(block.x1 < 25.0 && block.x2 > 65.0 && block.y1 < 25.0 && block.y2 > 28.0, "{block:?}");
    // One 100x60 region pads to one 128x64 tile: a single request.
    let calls = transport.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, json!({"n": 1, "width": 128, "height": 64}));
}

// -------------------------------------------------------------------------
// Region helpers: conversion, empty region, dilation
// -------------------------------------------------------------------------

/// The region reaches the detector as its UNMULTIPLIED colour with alpha dropped.
#[test]
fn region_conversion_uses_unmultiplied_colour() {
    let rgba = [200u8, 100, 50, 128, 10, 20, 30, 255];
    let image = egui::ColorImage::from_rgba_unmultiplied([2, 1], &rgba);
    let rgb = color_image_to_rgb(&image).expect("convert");
    assert_eq!(rgb.dimensions(), (2, 1));
    let expected: Vec<u8> = image.pixels.iter().flat_map(|px| {
        let [r, g, b, _] = px.to_srgba_unmultiplied();
        [r, g, b]
    }).collect();
    assert_eq!(rgb.as_raw(), &expected);
    assert_eq!(&rgb.as_raw()[3..], &[10, 20, 30]);

    let mut broken = image.clone();
    broken.pixels.push(egui::Color32::BLACK);
    assert_eq!(
        color_image_to_rgb(&broken),
        Err(tf!("translation.text_detector.region_image_size_error", w = 2, h = 1, len = 3))
    );
}

/// An empty region is the empty mask and never reaches the detector; otherwise the detector's
/// mask is dilated in Rust by the requested radius (clamped 0..=30).
#[test]
fn region_mask_skips_empty_regions_and_dilates() {
    let empty = egui::ColorImage::filled([0, 4], egui::Color32::WHITE);
    let result = region_mask(&empty, 3, |_| Err("detector must not run".to_string()));
    assert_eq!(result, Ok(([0, 0], Vec::new())));

    let region = egui::ColorImage::filled([5, 5], egui::Color32::WHITE);
    let single_pixel = |_page: &RgbImage| {
        let mut alpha = vec![0u8; 25];
        alpha[12] = 255;
        Ok(Detection {
            source_size: [5, 5],
            blocks: Vec::new(),
            mask: BinaryMask { size: [5, 5], alpha },
            stats: DetectionStats { tiles: 1, batches: 1 },
        })
    };
    let (size, alpha) = region_mask(&region, 1, single_pixel).expect("dilated");
    assert_eq!(size, [5, 5]);
    assert_eq!(alpha.iter().filter(|&&px| px == 255).count(), 9);
    let (_, undilated) = region_mask(&region, 0, single_pixel).expect("undilated");
    assert_eq!(undilated.iter().filter(|&&px| px == 255).count(), 1);
    let detector_error = region_mask(&region, 1, |_| Err("offline".to_string()));
    assert_eq!(detector_error, Err("offline".to_string()));
}
