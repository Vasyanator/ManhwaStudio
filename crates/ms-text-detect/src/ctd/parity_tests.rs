/*
File: crates/ms-text-detect/src/ctd/parity_tests.rs

Purpose:
Golden-fixture parity of the CTD postprocess against the Python reference
(`fixtures/ctd/<case>/`, see `fixtures/README.md`), plus one end-to-end run through
`pipeline::run_detection` with a fake runner.

Notes:
Maps are fed as stored (level `k` == Python's `float32(k) / 255`). Tolerances (plan WP2.3a):
block count equal; every expected block matched with IoU >= 0.9 and each edge within 2 px; mask
IoU >= 0.97 and XOR <= 3 % of the union. The achieved values are printed (`--nocapture`). The
expected mask is the refined mask `> 30` WITHOUT dilation (the fixtures record dilation 0).
*/

use std::num::NonZeroUsize;
use std::path::PathBuf;

use image::{GrayImage, Rgb, RgbImage};

use super::refine::{enlarge_window, window_trace};
use super::resize::resize_linear_u8;
use super::{postprocess_maps, refine};
use crate::blocks::DetectRect;
use crate::plan::{DetectParams, EngineKind, plan_detection};
use crate::runner::{ProbMap, ProbMapRunner, RunnerError, TileMaps};
use crate::pipeline::run_detection;

/// One loaded CTD fixture case.
struct Case {
    name: &'static str,
    json: serde_json::Value,
    page: RgbImage,
    seg: ProbMap,
    shrink: ProbMap,
    seg_source: GrayImage,
    expected_mask: GrayImage,
    expected_blocks: Vec<[f32; 4]>,
}

const CASES: [&str; 3] = ["dark_on_light", "light_on_dark", "color_bg"];

fn gray(dir: &std::path::Path, file: &str) -> GrayImage {
    image::open(dir.join(file)).unwrap_or_else(|err| panic!("{file}: {err}")).to_luma8()
}

fn load(name: &'static str) -> Case {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/ctd").join(name);
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("case.json")).unwrap_or_else(|err| panic!("{name}: {err}"))).unwrap_or_else(|err| panic!("{name}: {err}"));
    let page = image::open(dir.join("input.png")).unwrap_or_else(|err| panic!("{name}: {err}")).to_rgb8();
    let map = |file: &str| {
        let g = gray(&dir, file);
        ProbMap::new(g.width(), g.height(), g.into_raw()).unwrap_or_else(|err| panic!("{file}: {err}"))
    };
    let num = |v: &serde_json::Value| v.as_f64().unwrap_or_else(|| panic!("{name}: not a number: {v}"));
    let expected_blocks = json["expected"]["blocks"]
        .as_array()
        .unwrap_or_else(|| panic!("{name}: no blocks"))
        .iter()
        .map(|b| [num(&b["x1"]), num(&b["y1"]), num(&b["x2"]), num(&b["y2"])].map(f64_to_f32))
        .collect();
    Case { name, page, seg: map("seg.png"), shrink: map("shrink.png"), seg_source: gray(&dir, "seg_source.png"), expected_mask: gray(&dir, "expected_mask.png"), expected_blocks, json }
}

fn f64_to_f32(v: f64) -> f32 {
    #[expect(clippy::cast_possible_truncation, reason = "fixture coordinates are small integers")]
    let out = v as f32;
    out
}

fn usize_f64(v: usize) -> f64 {
    #[expect(clippy::cast_precision_loss, reason = "pixel counts of tiny fixtures")]
    let out = v as f64;
    out
}

fn iou(a: [f32; 4], b: [f32; 4]) -> f32 {
    let iw = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let ih = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let inter = iw * ih;
    let union = (a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1]) - inter;
    if union > 0.0 { inter / union } else { 0.0 }
}

/// Mask agreement: (`IoU`, `XOR` / union, `XOR` pixel count).
fn mask_agreement(got: &[u8], expected: &[u8]) -> (f64, f64, usize) {
    let (mut inter, mut union, mut xor) = (0_usize, 0_usize, 0_usize);
    for (&g, &e) in got.iter().zip(expected) {
        let (g, e) = (g != 0, e != 0);
        inter += usize::from(g && e);
        union += usize::from(g || e);
        xor += usize::from(g != e);
    }
    if union == 0 {
        return (1.0, 0.0, xor);
    }
    (usize_f64(inter) / usize_f64(union), usize_f64(xor) / usize_f64(union), xor)
}

/// Asserts the block tolerances; returns (min `IoU`, max edge error).
fn check_blocks(case: &Case, got: &[DetectRect]) -> (f32, f32) {
    assert_eq!(got.len(), case.expected_blocks.len(), "{}: blocks {got:?} vs {:?}", case.name, case.expected_blocks);
    let (mut min_iou, mut max_edge) = (1.0_f32, 0.0_f32);
    for want in &case.expected_blocks {
        let best = got.iter().map(|r| [r.x1, r.y1, r.x2, r.y2]).max_by(|a, b| iou(*a, *want).total_cmp(&iou(*b, *want))).unwrap_or_else(|| panic!("{}: no blocks", case.name));
        let edge = best.iter().zip(want).map(|(g, w)| (g - w).abs()).fold(0.0_f32, f32::max);
        let value = iou(best, *want);
        assert!(value >= 0.9 && edge <= 2.0, "{}: block {want:?} matched {best:?} (IoU {value}, edge {edge})", case.name);
        min_iou = min_iou.min(value);
        max_edge = max_edge.max(edge);
    }
    (min_iou, max_edge)
}

#[test]
fn ctd_postprocess_matches_the_python_fixtures() {
    for name in CASES {
        let case = load(name);
        let (blocks, mask) = postprocess_maps(&case.page, &case.seg, &case.shrink).unwrap_or_else(|err| panic!("{name}: {err}"));
        let (min_iou, max_edge) = check_blocks(&case, &blocks);
        assert_eq!(mask.size, [case.page.width(), case.page.height()]);
        let (mask_iou, xor_ratio, xor) = mask_agreement(&mask.alpha, case.expected_mask.as_raw());
        eprintln!("ctd parity {name}: blocks {} min IoU {min_iou:.4} max edge {max_edge} px; mask IoU {mask_iou:.5} xor/union {xor_ratio:.5} ({xor} px)", blocks.len());
        assert!(mask_iou >= 0.97 && xor_ratio <= 0.03, "{name}: mask IoU {mask_iou} xor/union {xor_ratio}");
    }
}

/// Stronger than the plan tolerances: today the port reproduces every fixture exactly (blocks and
/// mask). A deliberate behaviour change may relax this test; the tolerance test above stays.
#[test]
fn ctd_postprocess_is_exact_on_the_fixtures() {
    for name in CASES {
        let case = load(name);
        let (mut blocks, mask) = postprocess_maps(&case.page, &case.seg, &case.shrink).unwrap_or_else(|err| panic!("{name}: {err}"));
        crate::blocks::finalize_blocks(&mut blocks);
        let got: Vec<[f32; 4]> = blocks.iter().map(|r| [r.x1, r.y1, r.x2, r.y2]).collect();
        assert_eq!(got, case.expected_blocks, "{name}");
        assert_eq!(mask.alpha.as_slice(), case.expected_mask.as_raw().as_slice(), "{name}");
    }
}

#[test]
fn ctd_seg_resize_matches_opencv_inter_linear() {
    for name in CASES {
        let case = load(name);
        let [w, h] = case.seg.size();
        let got = resize_linear_u8(case.seg.data(), w, h, case.page.width(), case.page.height());
        let want = case.seg_source.as_raw();
        let diff = got.iter().zip(want).filter(|(g, w)| g != w).count();
        let max = got.iter().zip(want).map(|(&g, &w)| g.abs_diff(w)).max().unwrap_or(0);
        eprintln!("ctd seg resize {name}: {diff} of {} levels differ, max diff {max}", want.len());
        assert_eq!(got.len(), want.len());
        assert_eq!(diff, 0, "{name}: {diff} levels differ (max {max})");
    }
}

#[test]
fn ctd_refine_intermediates_match_the_python_fixtures() {
    for name in CASES {
        let case = load(name);
        let (w, h) = case.page.dimensions();
        let refine_blocks = case.json["intermediate"]["refine_blocks"].as_array().unwrap_or_else(|| panic!("{name}: no refine_blocks"));
        let to_u32 = |v: &serde_json::Value| u32::try_from(v.as_u64().unwrap_or_else(|| panic!("{name}: {v}"))).unwrap_or_else(|err| panic!("{name}: {err}"));
        let mut blocks = Vec::new();
        for rb in refine_blocks {
            let block: [u32; 4] = std::array::from_fn(|i| to_u32(&rb["block_xyxy"][i]));
            let window: [u32; 4] = std::array::from_fn(|i| to_u32(&rb["window_xyxy"][i]));
            assert_eq!(enlarge_window(block, w, h), Some(window), "{name}: window of {block:?}");
            let (colors, otsu, sums) = window_trace(&case.page, case.seg_source.as_raw(), window);
            let mut want_colors: Vec<f64> = rb["topk"]["colors"].as_array().unwrap_or_else(|| panic!("{name}: colors")).iter().map(|v| v.as_f64().unwrap_or(f64::NAN)).collect();
            // Upstream's unstable argsort may order tied colours differently; the generator
            // guarantees the SET matches a stable sort, so compare sets when it flagged that.
            let mut colors = colors;
            if rb["topk"]["stable_sort_order_differs"].as_bool().unwrap_or(false) {
                colors.sort_by(f64::total_cmp);
                want_colors.sort_by(f64::total_cmp);
            }
            assert_eq!(colors.len(), want_colors.len(), "{name} {block:?}: colors {colors:?} vs {want_colors:?}");
            assert!(colors.iter().zip(&want_colors).all(|(a, b)| (a - b).abs() < 1e-9), "{name} {block:?}: colors {colors:?} vs {want_colors:?}");
            let want_otsu: Vec<u8> = rb["otsu"]["threshold_per_channel_bgr"].as_array().unwrap_or_else(|| panic!("{name}: otsu")).iter().map(level).collect();
            assert_eq!(otsu.to_vec(), want_otsu, "{name} {block:?}: otsu");
            let want_sums: Vec<u64> = rb["candidates_in_merge_order"].as_array().unwrap_or_else(|| panic!("{name}: sums")).iter().map(|v| v.as_u64().unwrap_or(0)).collect();
            assert_eq!(sums, want_sums, "{name} {block:?}: candidate xor sums");
            blocks.push(block);
        }
        // Refinement alone, from the reference seg_source and the reference blocks: exact.
        let refined = refine::refine_mask(&case.page, case.seg_source.as_raw(), &blocks);
        let thresholded: Vec<u8> = refined.iter().map(|&v| if v > super::MASK_THRESHOLD { 255 } else { 0 }).collect();
        let (mask_iou, _, xor) = mask_agreement(&thresholded, case.expected_mask.as_raw());
        eprintln!("ctd refine-only {name}: mask IoU {mask_iou:.5} ({xor} px differ)");
        assert_eq!(xor, 0, "{name}: refine-only mask differs in {xor} px");
    }
}

/// A fixture level stored as a float (`129.0`) as `u8`.
fn level(v: &serde_json::Value) -> u8 {
    let f = v.as_f64().unwrap_or_else(|| panic!("not a number: {v}"));
    assert!((0.0..=255.0).contains(&f) && f.fract() == 0.0, "not a level: {f}");
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "asserted integral 0..=255")]
    let out = f as u8;
    out
}

/// Fake CTD model: seg and shrink both mark dark page pixels (padding is black but marked 0).
struct DarkCtdRunner;

impl ProbMapRunner for DarkCtdRunner {
    fn max_batch(&self, _tile_input: [u32; 2]) -> NonZeroUsize {
        NonZeroUsize::MIN
    }

    fn forward(&mut self, tiles: &[RgbImage]) -> Result<Vec<TileMaps>, RunnerError> {
        tiles
            .iter()
            .map(|t| {
                let data: Vec<u8> = t.pixels().map(|px| if px.0[0] < 128 && px.0 != [0, 0, 0] { 255 } else { 0 }).collect();
                let map = ProbMap::new(t.width(), t.height(), data).map_err(|err| RunnerError { message: err.to_string() })?;
                Ok(TileMaps { maps: vec![map.clone(), map] })
            })
            .collect()
    }
}

#[test]
fn ctd_runs_end_to_end_through_the_pipeline() {
    // A white page with one dark text bar: the plan upscales it (letterbox parity), the block
    // comes back in source pixels and the mask covers the bar.
    let (w, h) = (500_u32, 700_u32);
    let page = RgbImage::from_fn(w, h, |x, y| if (100..300).contains(&x) && (400..430).contains(&y) { Rgb([10, 10, 10]) } else { Rgb([250, 250, 250]) });
    let plan = plan_detection(EngineKind::Ctd, [w, h], &DetectParams::default()).unwrap_or_else(|err| panic!("{err}"));
    let detection = run_detection(&plan, &page, &mut DarkCtdRunner).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(detection.blocks.len(), 1, "{:?}", detection.blocks);
    let b = detection.blocks[0];
    assert!(b.x1 <= 100.0 && b.x2 >= 299.0 && b.y1 <= 400.0 && b.y2 >= 429.0, "{b:?}");
    assert!(b.x1 > 70.0 && b.x2 < 330.0 && b.y1 > 370.0 && b.y2 < 460.0, "{b:?}");
    assert_eq!(detection.mask.size, [w, h]);
    let at = |x: u32, y: u32| detection.mask.alpha[crate::num::idx(y * w + x)];
    assert_eq!(at(200, 415), 255);
    assert_eq!(at(200, 100), 0);
}

#[test]
fn ctd_postprocess_validates_its_maps() {
    let plan = plan_detection(EngineKind::Ctd, [100, 80], &DetectParams::default()).unwrap_or_else(|err| panic!("{err}"));
    let [sw, sh] = plan.scaled_size();
    let map = |w: u32, h: u32| ProbMap::new(w, h, vec![0; crate::num::idx(w * h)]).unwrap_or_else(|err| panic!("{err}"));
    let page = RgbImage::new(100, 80);
    let one = [map(sw, sh)];
    assert!(matches!(super::postprocess(&plan, &page, &one), Err(crate::pipeline::DetectError::ChannelCount { expected: 2, got: 1, .. })));
    let wrong = [map(sw, sh), map(sw, sh + 1)];
    assert!(matches!(super::postprocess(&plan, &page, &wrong), Err(crate::pipeline::DetectError::MapShape { channel: 1, .. })));
    let ok = [map(sw, sh), map(sw, sh)];
    let (blocks, mask) = super::postprocess(&plan, &page, &ok).unwrap_or_else(|err| panic!("{err}"));
    assert!(blocks.is_empty());
    assert!(mask.alpha.iter().all(|&v| v == 0));
    assert!(matches!(super::postprocess(&plan, &RgbImage::new(80, 100), &ok), Err(crate::pipeline::DetectError::PageSize { .. })));
}
