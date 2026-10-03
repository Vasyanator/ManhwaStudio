/*
File: crates/ms-text-detect/src/surya/tests.rs

Purpose:
Tests of the Surya postprocess: parity against the golden fixtures of the removed Python
postprocess (`fixtures/surya/<case>`, see `fixtures/README.md`), plus unit tests of the
replicated quirks (float32 threshold edge, even-kernel dilation anchor, cv2 nearest resize,
`clean_boxes` duplicates) and the pipeline arm.

Notes:
The parity test prints the achieved numbers (threshold error, max box edge difference, mask
IoU) per case; run with `--nocapture` to see them.
*/

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use image::{Rgb, RgbImage};

use super::*;
use crate::pipeline::run_detection;
use crate::plan::{DetectParams, EngineKind, plan_detection};
use crate::runner::{ProbMapRunner, RunnerError, TileMaps};

const CASES: [&str; 3] = ["stretch_both", "width_only", "low_contrast"];

fn case_dir(case: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/surya").join(case)
}

fn load_json(dir: &Path) -> serde_json::Value {
    let bytes = std::fs::read(dir.join("case.json")).unwrap_or_else(|err| panic!("{}: {err}", dir.display()));
    serde_json::from_slice(&bytes).unwrap_or_else(|err| panic!("{}: {err}", dir.display()))
}

fn load_gray(path: &Path) -> GrayImage {
    image::open(path).unwrap_or_else(|err| panic!("{}: {err}", path.display())).to_luma8()
}

fn json_f64(value: &serde_json::Value) -> f64 {
    value.as_f64().unwrap_or_else(|| panic!("not a number: {value}"))
}

fn json_u32(value: &serde_json::Value) -> u32 {
    value.as_u64().and_then(|v| u32::try_from(v).ok()).unwrap_or_else(|| panic!("not a u32: {value}"))
}

fn heat_map(dir: &Path) -> ProbMap {
    let heat = load_gray(&dir.join("heat.png"));
    let (w, h) = heat.dimensions();
    ProbMap::new(w, h, heat.into_raw()).unwrap_or_else(|err| panic!("{err}"))
}

fn iou(a: &[u8], b: &[u8]) -> f64 {
    let inter = a.iter().zip(b).filter(|(x, y)| **x != 0 && **y != 0).count();
    let union = a.iter().zip(b).filter(|(x, y)| **x != 0 || **y != 0).count();
    if union == 0 { 1.0 } else { f64::from(u32::try_from(inter).unwrap_or(0)) / f64::from(u32::try_from(union).unwrap_or(1)) }
}

#[test]
fn golden_fixture_parity() {
    for case in CASES {
        let dir = case_dir(case);
        let json = load_json(&dir);
        let heat = heat_map(&dir);
        let source_size = [json_u32(&json["source_size"][0]), json_u32(&json["source_size"][1])];
        let inter = &json["intermediate"];

        // Thresholds (float32, within 1e-6).
        let t = dynamic_thresholds(heat.data()).unwrap_or_else(|| panic!("{case}: empty map"));
        let text_err = (f64::from(t.text) - json_f64(&inter["dynamic_text_threshold_f32"])).abs();
        let low_err = (f64::from(t.low) - json_f64(&inter["dynamic_low_text_f32"])).abs();
        assert!(text_err <= 1e-6 && low_err <= 1e-6, "{case}: thresholds {t:?}, errors {text_err} {low_err}");

        // Components: every 4-connected component with its stats and outcome, in label order.
        let raw = extract_boxes(&heat);
        let expected_components = inter["components"].as_array().unwrap_or_else(|| panic!("{case}: components"));
        assert_eq!(raw.components.len(), expected_components.len(), "{case}: component count");
        assert_eq!(json_u32(&inter["label_count"]), u32::try_from(raw.components.len()).unwrap_or(0));
        for ((c, outcome), e) in raw.components.iter().zip(expected_components) {
            let bbox = [c.min_x, c.min_y, c.max_x - c.min_x + 1, c.max_y - c.min_y + 1];
            let e_bbox: Vec<u32> = (0..4).map(|i| json_u32(&e["bbox_xywh"][i])).collect();
            assert_eq!(bbox.as_slice(), e_bbox.as_slice(), "{case}: label {}", e["label"]);
            assert_eq!(c.area, json_u32(&e["area"]), "{case}: label {}", e["label"]);
            assert!((f64::from(level_value(c.max_level)) - json_f64(&e["max"])).abs() < 1e-7, "{case}: label {}", e["label"]);
            let e_outcome = match e["outcome"].as_str() {
                Some("area_lt_10") => Outcome::AreaTooSmall,
                Some("max_lt_text_threshold") => Outcome::BelowTextThreshold,
                Some("kept") => Outcome::Kept,
                other => panic!("{case}: unknown outcome {other:?}"),
            };
            assert_eq!(*outcome, e_outcome, "{case}: label {}", e["label"]);
        }

        // Processor-space boxes (after the roll, before rescale).
        let e_boxes = inter["proc_boxes"].as_array().unwrap_or_else(|| panic!("{case}: proc_boxes"));
        assert_eq!(raw.boxes.len(), e_boxes.len(), "{case}: proc box count");
        let mut proc_err = 0.0_f64;
        for (got, e) in raw.boxes.iter().zip(e_boxes) {
            for (corner, e_corner) in got.iter().zip(e.as_array().unwrap_or_else(|| panic!("{case}: box"))) {
                for axis in 0..2 {
                    proc_err = proc_err.max((f64::from(corner[axis]) - json_f64(&e_corner[axis])).abs());
                }
            }
        }
        assert!(proc_err <= 0.05, "{case}: processor corners differ by {proc_err}");

        // Processor mask: exact.
        let proc_mask = load_gray(&dir.join("proc_mask.png"));
        let proc_iou = iou(&raw.proc_mask, proc_mask.as_raw());

        // Final blocks and mask.
        let (blocks, mask) = postprocess_heatmap(&heat, source_size).unwrap_or_else(|err| panic!("{case}: {err}"));
        let e_blocks = json["expected"]["blocks"].as_array().unwrap_or_else(|| panic!("{case}: blocks"));
        assert_eq!(blocks.len(), e_blocks.len(), "{case}: {blocks:?}");
        let mut edge_err = 0.0_f64;
        for (b, e) in blocks.iter().zip(e_blocks) {
            for (got, key) in [(b.x1, "x1"), (b.y1, "y1"), (b.x2, "x2"), (b.y2, "y2")] {
                edge_err = edge_err.max((f64::from(got) - json_f64(&e[key])).abs());
            }
        }
        let expected_mask = load_gray(&dir.join("expected_mask.png"));
        assert_eq!(mask.size, [expected_mask.width(), expected_mask.height()], "{case}: mask size");
        let mask_iou = iou(&mask.alpha, expected_mask.as_raw());
        let mask_xor = mask.alpha.iter().zip(expected_mask.as_raw()).filter(|(a, b)| (**a != 0) != (**b != 0)).count();
        eprintln!("surya parity {case}: threshold err text {text_err:.2e} low {low_err:.2e}; proc corner err {proc_err:.2e}; block edge err {edge_err}; proc mask IoU {proc_iou}; mask IoU {mask_iou} (xor {mask_xor} px)");
        // The package tolerance is +-2 px per edge and mask IoU >= 0.97; the port is exact on
        // every fixture, so any drift is a regression worth looking at.
        assert!(edge_err == 0.0, "{case}: block edges differ by {edge_err}");
        assert!(mask_xor == 0 && raw.proc_mask == *proc_mask.as_raw(), "{case}: mask IoU {mask_iou}, proc mask IoU {proc_iou}");
    }
}

#[test]
fn a_component_whose_max_is_level_153_meets_the_saturated_threshold() {
    // A saturated map (top 10 % mean above 0.7) keeps text = 0.6f32 == level 153 / 255.
    let (w, h) = (40_u32, 20_u32);
    let data: Vec<u8> = (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| if (2..12).contains(&x) && (2..6).contains(&y) { 153 } else if y >= 15 { 255 } else { 0 }).collect();
    let heat = ProbMap::new(w, h, data).unwrap_or_else(|err| panic!("{err}"));
    let t = dynamic_thresholds(heat.data()).unwrap_or_else(|| panic!("non-empty"));
    assert_eq!(t.text.to_bits(), 0.6_f32.to_bits());
    assert_eq!(level_value(153).to_bits(), t.text.to_bits());
    let raw = extract_boxes(&heat);
    assert_eq!(raw.components[0].1, Outcome::Kept);
}

#[test]
fn even_kernel_dilation_matches_the_cv2_anchor() {
    // Brute-force cv2 dilate with a k x k rect and anchor k/2, clipped to the window.
    let (w, h) = (9_usize, 7_usize);
    let mut src = vec![0_u8; w * h];
    src[3 * w + 4] = 1;
    src[0] = 1;
    for k in 2..=5_usize {
        let reference: Vec<u8> = (0..h * w)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                let hit = (0..k).any(|dy| (0..k).any(|dx| {
                    let (sx, sy) = ((x + dx).checked_sub(k / 2), (y + dy).checked_sub(k / 2));
                    matches!((sx, sy), (Some(sx), Some(sy)) if sx < w && sy < h && src[sy * w + sx] != 0)
                }));
                u8::from(hit)
            })
            .collect();
        let radius = if k % 2 == 1 { (k - 1) / 2 } else { k / 2 - 1 };
        let mut got = ms_raster::dilate_square(&src, w, h, radius, radius, 1).unwrap_or_else(|err| panic!("{err}"));
        if k % 2 == 0 {
            got = extend_right_down(&got, w);
        }
        assert_eq!(got, reference, "k = {k}");
    }
}

#[test]
fn nearest_resize_uses_the_cv2_source_index() {
    // 4 -> 6 columns: cv2 picks floor(d * 4/6) = 0, 0, 1, 2, 2, 3.
    let proc = vec![10, 0, 30, 40];
    let mask = resize_mask_nearest(&proc, [4, 1], [6, 1]);
    assert_eq!(mask.alpha, vec![255, 255, 0, 255, 255, 255]);
    let empty = resize_mask_nearest(&[], [0, 0], [3, 2]);
    assert_eq!(empty, BinaryMask { size: [3, 2], alpha: vec![0; 6] });
}

#[test]
fn clean_boxes_keeps_duplicates_and_drops_contained_and_flat_boxes() {
    let rect = |x1: u32, y1: u32, x2: u32, y2: u32| -> Polygon { [[x1, y1], [x2, y1], [x2, y2], [x1, y2]] };
    let outer = rect(0, 0, 10, 10);
    let inner = rect(0, 2, 10, 5); // touches the outer edges: inclusive containment
    let flat = rect(20, 0, 20, 5);
    let kept = clean_boxes(&[outer, inner, outer, flat]);
    assert_eq!(kept, vec![outer, outer]);
}

#[test]
fn rescale_and_y_expand_truncate_and_fit() {
    // 2.9 * 2 = 5.8 -> 5; -0.25 * 2 -> int 0; 10.5 * 2 = 21 -> fit to 20.
    let polygon = rescale_and_fit(&[[2.9, 1.0], [10.5, 1.0], [10.5, 16.0], [-0.25, 16.0]], [10, 16], [20, 32]);
    assert_eq!(polygon, [[5, 2], [20, 2], [20, 32], [0, 32]]);
    // height 30 -> margin 1.5: 1 - 1.5 = -0.5 -> int 0; 31 + 1.5 = 32.5 -> 32, fit to 32.
    let out = expand_y_and_fit(&[[1, 1], [9, 1], [9, 31], [1, 31]], [10, 32]);
    assert_eq!(out, [[1, 0], [9, 0], [9, 32], [1, 32]]);
}

#[test]
fn empty_and_flat_heatmaps_yield_no_blocks() {
    assert_eq!(dynamic_thresholds(&[]), None);
    // An all-zero map: nothing exceeds `low`, so no component and an all-zero source mask.
    let heat = ProbMap::new(5, 4, vec![0; 20]).unwrap_or_else(|err| panic!("{err}"));
    let (blocks, mask) = postprocess_heatmap(&heat, [4, 3]).unwrap_or_else(|err| panic!("{err}"));
    assert!(blocks.is_empty());
    assert_eq!(mask, BinaryMask { size: [4, 3], alpha: vec![0; 12] });
    assert_eq!(postprocess_heatmap(&heat, [10_001, 10_000]), Err(MaskError::TooLarge { width: 10_001, height: 10_000 }));
}

/// Fake Surya model: a quarter-resolution heatmap that is 1 on dark page pixels.
struct DarkQuarterRunner;

impl ProbMapRunner for DarkQuarterRunner {
    fn max_batch(&self, _tile_input: [u32; 2]) -> NonZeroUsize {
        NonZeroUsize::MIN
    }

    fn forward(&mut self, tiles: &[RgbImage]) -> Result<Vec<TileMaps>, RunnerError> {
        tiles
            .iter()
            .map(|t| {
                let (w, h) = (t.width() / 4, t.height() / 4);
                let data = (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| if t.get_pixel(x * 4 + 2, y * 4 + 2).0[0] < 128 { 255 } else { 0 }).collect();
                let map = ProbMap::new(w, h, data).map_err(|err| RunnerError { message: err.to_string() })?;
                Ok(TileMaps { maps: vec![map] })
            })
            .collect()
    }
}

#[test]
fn pipeline_runs_surya_end_to_end() {
    let (w, h) = (600_u32, 800_u32);
    let page = RgbImage::from_fn(w, h, |x, y| if (100..500).contains(&x) && (300..340).contains(&y) { Rgb([0, 0, 0]) } else { Rgb([255, 255, 255]) });
    let plan = plan_detection(EngineKind::Surya, [w, h], &DetectParams::default()).unwrap_or_else(|err| panic!("{err}"));
    let detection = run_detection(&plan, &page, &mut DarkQuarterRunner).unwrap_or_else(|err| panic!("{err}"));
    assert_eq!(detection.blocks.len(), 1, "{:?}", detection.blocks);
    let b = detection.blocks[0];
    assert!(b.x1 <= 100.0 && b.x2 >= 499.0 && b.y1 <= 300.0 && b.y2 >= 339.0, "{b:?}");
    assert!(b.x1 > 80.0 && b.x2 < 520.0 && b.y1 > 280.0 && b.y2 < 360.0, "{b:?}");
    assert_eq!(detection.mask.size, [w, h]);
    assert_eq!(detection.mask.alpha[idx(320 * w + 300)], 255);
    assert_eq!(detection.mask.alpha[idx(100 * w + 300)], 0);
}
