/*
File: local_match/tests.rs

Purpose:
Contract and parity tests of the local CIELAB colour match: the stage parity against the golden
PATCHED fixtures (`fixtures/cases/<case>/patched/local.png` from `patched/global.png`), the drift
against the UNPATCHED reference (printed, not asserted), the splitmix jitter known answers,
identity rules, refusals, the Lab round trip and an ignored release benchmark.

Notes:
Fixtures are tracked files of this crate (`fixtures/README.md`), read-only; nothing is written.
Run the benchmark with
`cargo test --release -p ms-region-postprocess local_match::tests::bench -- --ignored --nocapture`.
*/

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::grid::{self, Field, index_f64, reflect101, to_f32};
use super::jitter::{jittered_sample_indices, splitmix64};
use super::lab::LabTables;
use super::{LocalMatchError, MAX_SCALE_PX, match_color_local};

type TestResult = Result<(), Box<dyn Error>>;

/// Root of the crate's golden fixtures.
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

/// RGB8 bytes of a fixture PNG.
fn rgb(path: &Path) -> Result<Vec<u8>, Box<dyn Error>> {
    Ok(image::open(path)?.into_rgb8().into_raw())
}

/// L8 bytes of a fixture PNG.
fn luma(path: &Path) -> Result<Vec<u8>, Box<dyn Error>> {
    Ok(image::open(path)?.into_luma8().into_raw())
}

/// A `usize` field of a JSON object.
fn json_usize(value: &serde_json::Value, key: &str) -> Result<usize, Box<dyn Error>> {
    let raw = value.get(key).and_then(serde_json::Value::as_u64).ok_or_else(|| format!("missing integer `{key}`"))?;
    Ok(usize::try_from(raw)?)
}

/// Difference statistics of `actual` against `expected` (RGB8): max |Δ| over all values and the
/// mean |Δ| over the values of pixels inside `mask`, plus whether every pixel outside `mask`
/// is byte-identical.
#[derive(Debug)]
struct Diff {
    max_abs: u8,
    mean_inside: f64,
    differing: usize,
    outside_identical: bool,
}

fn diff(actual: &[u8], expected: &[u8], mask: &[u8]) -> Diff {
    let mut max_abs = 0_u8;
    let mut sum_inside = 0_u64;
    let mut inside_values = 0_usize;
    let mut differing = 0_usize;
    let mut outside_identical = true;
    for ((a, e), &m) in actual.chunks_exact(3).zip(expected.chunks_exact(3)).zip(mask) {
        for (&x, &y) in a.iter().zip(e) {
            let delta = x.abs_diff(y);
            max_abs = max_abs.max(delta);
            differing += usize::from(delta != 0);
            if m > 0 {
                sum_inside += u64::from(delta);
                inside_values += 1;
            } else if delta != 0 {
                outside_identical = false;
            }
        }
    }
    #[expect(clippy::cast_precision_loss, reason = "test statistic over a few hundred thousand values")]
    let mean_inside = if inside_values == 0 { 0.0 } else { sum_inside as f64 / inside_values as f64 };
    Diff { max_abs, mean_inside, differing, outside_identical }
}

#[test]
fn splitmix_and_jitter_grids_match_the_generator() -> TestResult {
    assert_eq!(splitmix64(0), 0xE220_A839_7B1D_CDAF);
    let text = std::fs::read_to_string(fixtures().join("jitter.json"))?;
    let json: serde_json::Value = serde_json::from_str(&text)?;
    let vectors = json["splitmix64"].as_array().ok_or("splitmix64 is not an array")?;
    assert!(!vectors.is_empty());
    for vector in vectors {
        let key = vector["key"].as_u64().ok_or("key")?;
        let hash = vector["hash"].as_u64().ok_or("hash")?;
        assert_eq!(splitmix64(key), hash, "splitmix64({key})");
    }
    let grids = json["grids"].as_array().ok_or("grids is not an array")?;
    assert_eq!(grids.len(), 2);
    for entry in grids {
        let height = json_usize(entry, "padded_h")?;
        let width = json_usize(entry, "padded_w")?;
        let level = u32::try_from(json_usize(entry, "level")?)?;
        let samples = jittered_sample_indices(height, width, 1 << level, level);
        let flatten = |key: &str| -> Result<Vec<usize>, Box<dyn Error>> {
            let mut out = Vec::new();
            for row in entry[key].as_array().ok_or("rows")? {
                for value in row.as_array().ok_or("row")? {
                    out.push(usize::try_from(value.as_u64().ok_or("index")?)?);
                }
            }
            Ok(out)
        };
        assert_eq!(samples.row_index, flatten("row_index")?, "row_index {height}x{width} level {level}");
        assert_eq!(samples.col_index, flatten("col_index")?, "col_index {height}x{width} level {level}");
        assert_eq!(samples.rows * samples.cols, samples.row_index.len());
    }
    Ok(())
}

#[test]
fn local_stage_matches_patched_fixtures() -> TestResult {
    let mut cases = Vec::new();
    for entry in std::fs::read_dir(fixtures().join("cases"))? {
        cases.push(entry?.path());
    }
    cases.sort();
    assert!(cases.len() >= 8, "fixture cases missing");
    let mut applied = 0;
    for case in &cases {
        let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(case.join("case.json"))?)?;
        let name = meta["name"].as_str().ok_or("name")?;
        let width = json_usize(&meta, "width")?;
        let height = json_usize(&meta, "height")?;
        let effective = &meta["effective"];
        let pct = u32::try_from(json_usize(effective, "local_strength_pct")?)?;
        let scale = u32::try_from(json_usize(effective, "local_scale_px")?)?;
        let reference_name = if meta["reference_distinct"].as_bool().ok_or("reference_distinct")? { "reference.png" } else { "base.png" };
        let reference = rgb(&case.join(reference_name))?;
        let current = rgb(&case.join("patched/global.png"))?;
        let sample_mask = luma(&case.join("sample_mask.png"))?;
        let expected = rgb(&case.join("patched/local.png"))?;
        let strength = to_f32(f64::from(pct) / 100.0);
        let actual = match_color_local(&reference, &current, &sample_mask, width, height, strength, scale)?;
        let patched = diff(&actual, &expected, &sample_mask);
        let unpatched = diff(&actual, &rgb(&case.join("unpatched/local.png"))?, &sample_mask);
        let untouched = diff(&actual, &current, &sample_mask);
        println!(
            "{name}: path {}, patched max {} mean-in-mask {:.4} differing {}; unpatched max {} mean-in-mask {:.4} differing {}",
            meta["local_path"], patched.max_abs, patched.mean_inside, patched.differing, unpatched.max_abs, unpatched.mean_inside, unpatched.differing
        );
        assert!(untouched.outside_identical, "{name}: a pixel outside the sample mask changed");
        match meta["local_path"].as_str().ok_or("local_path")? {
            "applied" => {
                applied += 1;
                assert!(patched.max_abs <= 2, "{name}: max |d| {}", patched.max_abs);
                assert!(patched.mean_inside <= 0.25, "{name}: mean |d| inside the mask {}", patched.mean_inside);
            }
            "skipped" | "off" => assert_eq!(actual, current, "{name}: a skipped / off local match must return its input"),
            other => return Err(format!("{name}: unknown local_path {other}").into()),
        }
    }
    assert!(applied >= 5, "only {applied} applied cases");
    Ok(())
}

/// A deterministic page-like RGB8 test region with a blob mask (`width x height`).
fn synthetic(width: usize, height: usize, noise_only: bool) -> (Vec<u8>, Vec<u8>) {
    let mut state = 0x1234_5678_u64;
    let mut rgb = Vec::with_capacity(width * height * 3);
    let mut mask = Vec::with_capacity(width * height);
    let (cx, cy) = (index_f64(width) / 2.0, index_f64(height) / 2.0);
    for row in 0..height {
        for col in 0..width {
            for channel in 0..3_u32 {
                state = splitmix64(state);
                let noise = i32::try_from(state % 7).unwrap_or(3) - 3;
                let base = if noise_only {
                    140 + 20 * i32::try_from(channel).unwrap_or(0)
                } else {
                    let ramp = i32::try_from((row + col * 2) % 160).unwrap_or(0);
                    60 + ramp + 15 * i32::try_from(channel).unwrap_or(0) + if (row / 9 + col / 13) % 5 == 0 { -40 } else { 0 }
                };
                rgb.push(u8::try_from((base + noise).clamp(0, 255)).unwrap_or(0));
            }
            let (dx, dy) = ((index_f64(col) - cx) / (cx * 0.6), (index_f64(row) - cy) / (cy * 0.5));
            mask.push(if dx * dx + dy * dy <= 1.0 { 255 } else { 0 });
        }
    }
    (rgb, mask)
}

#[test]
fn strength_zero_and_empty_mask_are_identity() -> TestResult {
    let (width, height) = (64, 48);
    let (reference, mask) = synthetic(width, height, false);
    let (current, _) = synthetic(width, height, true);
    assert_eq!(match_color_local(&reference, &current, &mask, width, height, 0.0, 24)?, current);
    assert_eq!(match_color_local(&reference, &current, &mask, width, height, -3.0, 24)?, current);
    assert_eq!(match_color_local(&reference, &current, &vec![0; width * height], width, height, 1.0, 24)?, current);
    // Fewer than 256 valid pixels around the margin: skipped.
    assert_eq!(match_color_local(&reference, &current, &vec![255; width * height], width, height, 1.0, 24)?, current);
    // An over-1 strength clamps to 1 and changes only masked pixels.
    let out = match_color_local(&reference, &current, &mask, width, height, 7.0, 8)?;
    assert_eq!(out, match_color_local(&reference, &current, &mask, width, height, 1.0, 8)?);
    assert!(diff(&out, &current, &mask).outside_identical);
    Ok(())
}

#[test]
fn flat_noise_is_nearly_identity() -> TestResult {
    let (width, height) = (96, 80);
    let (image, mask) = synthetic(width, height, true);
    // The settings' scale range (8..=96 px). Far below it (a 1 px sigma) the "low frequencies"
    // are the noise itself, so the identity is not expected there.
    for scale in [8, 24, 96] {
        let out = match_color_local(&image, &image, &mask, width, height, 1.0, scale)?;
        let stats = diff(&out, &image, &mask);
        assert!(stats.outside_identical);
        println!("flat noise, scale {scale}: max |d| {} mean |d| inside {:.4}", stats.max_abs, stats.mean_inside);
        assert!(stats.max_abs <= 1, "scale {scale}: max |d| {}", stats.max_abs);
        assert!(stats.mean_inside <= 0.05, "scale {scale}: mean |d| {}", stats.mean_inside);
    }
    Ok(())
}

#[test]
fn result_is_deterministic() -> TestResult {
    let (width, height) = (120, 90);
    let (reference, mask) = synthetic(width, height, false);
    let (current, _) = synthetic(width, height, true);
    let first = match_color_local(&reference, &current, &mask, width, height, 0.6, 24)?;
    assert_eq!(first, match_color_local(&reference, &current, &mask, width, height, 0.6, 24)?);
    Ok(())
}

#[test]
fn invalid_inputs_are_refused() {
    let (width, height) = (8, 4);
    let rgb = vec![0_u8; width * height * 3];
    let mask = vec![0_u8; width * height];
    assert!(matches!(match_color_local(&rgb[1..], &rgb, &mask, width, height, 0.5, 8), Err(LocalMatchError::ShapeMismatch { what: "reference", expected: 96, actual: 95 })));
    assert!(matches!(match_color_local(&rgb, &rgb[1..], &mask, width, height, 0.5, 8), Err(LocalMatchError::ShapeMismatch { what: "current", .. })));
    assert!(matches!(match_color_local(&rgb, &rgb, &mask[1..], width, height, 0.5, 8), Err(LocalMatchError::ShapeMismatch { what: "sample_mask", .. })));
    assert!(matches!(match_color_local(&rgb, &rgb, &mask, usize::MAX, 2, 0.5, 8), Err(LocalMatchError::SizeOverflow { .. })));
    assert!(matches!(match_color_local(&rgb, &rgb, &mask, width, height, 0.5, 0), Err(LocalMatchError::InvalidScale(0))));
    assert!(matches!(match_color_local(&rgb, &rgb, &mask, width, height, 0.5, MAX_SCALE_PX + 1), Err(LocalMatchError::ScaleTooLarge { .. })));
    assert!(matches!(match_color_local(&rgb, &rgb, &mask, width, height, f32::NAN, 8), Err(LocalMatchError::NonFiniteStrength(_))));
    assert!(matches!(match_color_local(&rgb, &rgb, &mask, width, height, f32::INFINITY, 8), Err(LocalMatchError::NonFiniteStrength(_))));
}

#[test]
fn lab_round_trip_of_a_16_level_cube_is_within_one_level() {
    let tables = LabTables::new();
    let levels: Vec<u8> = (0..16_u8).map(|level| level * 17).collect();
    let mut worst = 0_u8;
    for &r in &levels {
        for &g in &levels {
            for &b in &levels {
                let back = tables.to_srgb(tables.to_lab([r, g, b]));
                for (x, y) in back.iter().zip([r, g, b]) {
                    worst = worst.max(x.abs_diff(y));
                }
            }
        }
    }
    assert!(worst <= 1, "worst round-trip error {worst}");
}

#[test]
fn reflect101_folds_like_opencv() {
    // BORDER_REFLECT_101 over "abcd": ... c b | a b c d | c b a b ...
    let folded: Vec<usize> = (-4..9).map(|index| reflect101(index, 4)).collect();
    assert_eq!(folded, [2, 3, 2, 1, 0, 1, 2, 3, 2, 1, 0, 1, 2]);
    assert_eq!(reflect101(-7, 1), 0);
}

#[test]
fn low_pass_helpers_keep_a_constant_field() -> TestResult {
    let field = Field::from_vec(37, 29, 2, vec![3.5; 37 * 29 * 2])?;
    for sigma in [0.7, 3.0, 9.5] {
        let smoothed = grid::smooth(&field, sigma)?;
        assert_eq!((smoothed.height, smoothed.width, smoothed.channels), (37, 29, 2));
        assert!(smoothed.data.iter().all(|&value| (value - 3.5).abs() < 1e-4), "sigma {sigma}");
    }
    let down = grid::box_downsample(&field, 4)?;
    assert_eq!((down.height, down.width), (10, 8));
    assert!(down.data.iter().all(|&value| (value - 3.5).abs() < 1e-6));
    let up = grid::upsample_linear(&down, &(0..2), 4, 37, 29)?;
    assert!(up.data.iter().all(|&value| (value - 3.5).abs() < 1e-6));
    Ok(())
}

#[test]
#[ignore = "release benchmark; run with --release -- --ignored --nocapture"]
fn bench_2048_square() -> TestResult {
    let side = 2048;
    let (reference, mask) = synthetic(side, side, false);
    let (current, _) = synthetic(side, side, true);
    for scale in [24, 8] {
        let started = Instant::now();
        let out = match_color_local(&reference, &current, &mask, side, side, 0.6, scale)?;
        println!("local match {side}x{side}, scale {scale}: {:.1} ms", started.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(out.len(), current.len());
    }
    Ok(())
}

/// A deterministic pseudo-random field with some exact zeros (the zero-weight skip paths).
fn noise_field(height: usize, width: usize, channels: usize, seed: u64, zero_every: u64) -> Result<Field, LocalMatchError> {
    let mut state = seed;
    let data = (0..height * width * channels).map(|_| {
        state = splitmix64(state);
        if zero_every > 0 && state.is_multiple_of(zero_every) { 0.0 } else { to_f32(index_f64(usize::try_from(state % 20_000).unwrap_or(0)) / 100.0 - 60.0) }
    }).collect();
    Field::from_vec(height, width, channels, data)
}

#[test]
fn fused_moment_grid_equals_the_generic_block_means() -> TestResult {
    // (height, width, factor): ragged edges, factor 1, an 8+ lane pairwise and a one-column grid.
    for (height, width, factor) in [(23, 31, 3), (16, 16, 1), (50, 41, 8), (75, 70, 24), (40, 9, 10)] {
        let lab = noise_field(height, width, 3, 7, 0)?;
        let mut robust = noise_field(height, width, 1, 9, 3)?;
        for value in &mut robust.data {
            *value = value.abs();
        }
        let (grid_height, grid_width) = (height.div_ceil(factor), width.div_ceil(factor));
        let mut fused = Field::zeros(grid_height, grid_width, 6)?;
        grid::box_moments_into(&lab, &robust, factor, &mut fused);
        let mut generic = Field::zeros(grid_height, grid_width, 6)?;
        grid::box_downsample_into(height, width, 1, factor, |row, col, _| robust.at(row, col, 0), &mut generic, 1);
        grid::box_downsample_into(height, width, 3, factor, |row, col, channel| lab.at(row, col, channel) * robust.at(row, col, 0), &mut generic, 2);
        grid::box_downsample_into(height, width, 1, factor, |row, col, _| lab.at(row, col, 0) * (lab.at(row, col, 0) * robust.at(row, col, 0)), &mut generic, 5);
        let bits = |field: &Field| field.data.iter().map(|value| value.to_bits()).collect::<Vec<_>>();
        assert_eq!(bits(&fused), bits(&generic), "{height}x{width} factor {factor}");
    }
    Ok(())
}

#[test]
fn gaussian_blur_equals_a_direct_per_element_sum() -> TestResult {
    for (height, width, sigma) in [(19, 23, 0.8), (12, 5, 2.5), (30, 40, 3.99)] {
        let field = noise_field(height, width, 2, 11, 0)?;
        let blurred = grid::gaussian_blur(&field, sigma)?;
        // The reference loop: kernel as `gaussian_blur` builds it, rows then columns, each output
        // the f32 sum of its taps in order.
        let taps = grid::integral_to_usize((8.0 * sigma + 1.0).round_ties_even(), width, height)? | 1;
        let radius = taps / 2;
        let raw: Vec<f64> = (0..taps).map(|tap| {
            let offset = index_f64(tap) - index_f64(radius);
            (-(offset * offset) / (2.0 * sigma * sigma)).exp()
        }).collect();
        let total: f64 = grid::pairwise_sum(&|index: usize| raw[index], 0, taps);
        let kernel: Vec<f32> = raw.iter().map(|&weight| to_f32(weight / total)).collect();
        let shifted = |index: usize, tap: usize, length: usize| reflect101(index.cast_signed() + tap.cast_signed() - radius.cast_signed(), length);
        let mut vertical = vec![0.0_f32; field.data.len()];
        for row in 0..height {
            for col in 0..width {
                for channel in 0..2 {
                    let mut sum = 0.0_f32;
                    for (tap, &weight) in kernel.iter().enumerate() {
                        sum += weight * field.at(shifted(row, tap, height), col, channel);
                    }
                    vertical[(row * width + col) * 2 + channel] = sum;
                }
            }
        }
        for row in 0..height {
            for col in 0..width {
                for channel in 0..2 {
                    let mut sum = 0.0_f32;
                    for (tap, &weight) in kernel.iter().enumerate() {
                        sum += weight * vertical[(row * width + shifted(col, tap, width)) * 2 + channel];
                    }
                    assert_eq!(blurred.at(row, col, channel).to_bits(), sum.to_bits(), "{height}x{width} sigma {sigma} at {row},{col},{channel}");
                }
            }
        }
    }
    Ok(())
}

#[test]
fn reflect_fill_equals_reflect101_indexing() -> TestResult {
    let (height, width, top, left, padded_height, padded_width) = (5, 4, 7, 3, 21, 14);
    let region = noise_field(height, width, 2, 5, 0)?;
    let mut padded = Field::zeros(padded_height, padded_width, 2)?;
    for row in 0..height {
        for col in 0..width {
            let start = ((top + row) * padded_width + left + col) * 2;
            padded.data[start..start + 2].copy_from_slice(region.pixel(row, col));
        }
    }
    grid::reflect_fill(&mut padded, top, left, height, width);
    for row in 0..padded_height {
        for col in 0..padded_width {
            let source = region.pixel(reflect101(row.cast_signed() - top.cast_signed(), height), reflect101(col.cast_signed() - left.cast_signed(), width));
            assert_eq!(padded.pixel(row, col), source, "at {row},{col}");
        }
    }
    Ok(())
}
