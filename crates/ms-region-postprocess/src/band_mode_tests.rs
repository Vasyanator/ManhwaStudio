/*
File: crates/ms-region-postprocess/src/band_mode_tests.rs

Purpose:
Property tests of the edge-band reference mode (`ColourReference::EdgeBand`, a whole-region
run). The Python reference has no such mode, so these are Rust-only contracts on synthetic
inputs: what lies outside the mask, that a VAE-like tone shift and tint shrink at the band and
inside, the small-region cap, strength 0 = the global stage alone, determinism across thread
counts, plus the ignored release benchmark of a cold 2048² render.

Notes:
`cfg(test)` only; nothing is written anywhere. Run with `--nocapture` to see the measured
residuals.
*/

use crate::edge_band::EdgeBand;
use crate::global_match::{GlobalPath, match_color_band};
use crate::{ColourReference, EffectiveSettings, PostprocessError, RegionInputs, RegionPostprocessor, postprocess_region};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Default settings as a run applies them (feather 12, global on, local 60 % at 24 px).
const ON: EffectiveSettings = EffectiveSettings { feather_px: 12, global_match: true, local_strength: 0.6, local_scale_px: 24 };
/// The same with the colour match off (today's whole-region behaviour before the band mode).
const OFF: EffectiveSettings = EffectiveSettings { global_match: false, local_strength: 0.0, ..ON };

/// A synthetic whole-region result: `ideal` is the page with a centre edit (what the user
/// wants), `generated` is `ideal` after a VAE-like round trip (per-channel gain + offset and a
/// low-frequency tint growing from the left edge to the right one), `base` the page itself.
struct Synthetic {
    inputs: RegionInputs,
    ideal: Vec<u8>,
}

/// xorshift32 noise in `0..modulo`.
fn noise(state: &mut u32, modulo: u32) -> u8 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    u8::try_from(*state % modulo).unwrap_or(0)
}

/// `value` clamped to a u8 level.
fn level(value: i32) -> u8 {
    u8::try_from(value.clamp(0, 255)).unwrap_or(0)
}

/// See [`Synthetic`]; `tint` is the right-edge red tint in levels (0 = none).
fn synthetic(w: usize, h: usize, tint: i32) -> Synthetic {
    let mut state = 0x1234_5679_u32;
    let mut base = Vec::with_capacity(w * h * 3);
    let mut ideal = Vec::with_capacity(w * h * 3);
    let mut generated = Vec::with_capacity(w * h * 3);
    let (cx, cy, radius) = (w / 2, h / 2, w.min(h) / 4);
    for y in 0..h {
        for x in 0..w {
            let ramp = i32::try_from((x * 90 / w.max(1)) + (y * 50 / h.max(1))).unwrap_or(0);
            let grain = i32::from(noise(&mut state, 17));
            let page = [level(110 + ramp + grain), level(100 + ramp / 2 + grain), level(80 + ramp / 3 + grain)];
            base.extend_from_slice(&page);
            let (dx, dy) = (x.abs_diff(cx), y.abs_diff(cy));
            let edited = if dx * dx + dy * dy <= radius * radius { [level(40 + grain), level(160 + grain), level(200 + grain)] } else { page };
            ideal.extend_from_slice(&edited);
            let drift = tint * i32::try_from(x).unwrap_or(0) / i32::try_from(w.max(1)).unwrap_or(1);
            let [red, green, blue] = edited.map(i32::from);
            generated.extend_from_slice(&[level(red * 92 / 100 + 14 + drift), level(green * 95 / 100 + 6), level(blue * 105 / 100 - 9)]);
        }
    }
    let inputs = RegionInputs { width: w, height: h, base, reference: None, generated, mask: vec![255; w * h], sample_mask: None, colour_reference: ColourReference::EdgeBand };
    Synthetic { inputs, ideal }
}

/// Mean |a - b| over the pixels `select(index)` keeps.
fn mean_abs(a: &[u8], b: &[u8], select: impl Fn(usize) -> bool) -> f64 {
    let (mut sum, mut count) = (0_u64, 0_u32);
    for (index, (pa, pb)) in a.as_chunks::<3>().0.iter().zip(b.as_chunks::<3>().0).enumerate() {
        if select(index) {
            sum += pa.iter().zip(pb).map(|(x, y)| u64::from(x.abs_diff(*y))).sum::<u64>();
            count += 3;
        }
    }
    #[expect(clippy::cast_precision_loss, reason = "test statistics; sums far below 2^52")]
    let mean = if count == 0 { 0.0 } else { sum as f64 / f64::from(count) };
    mean
}

/// Outside the mask the result is the base's bytes even in band mode, and inside a solid
/// mask every pixel may change.
#[test]
fn outside_the_mask_nothing_changes() -> Result<(), PostprocessError> {
    let mut case = synthetic(96, 80, 10);
    // Unmask one column: band mode ignores the sample mask, the composite still honours `mask`.
    for row in 0..80 {
        case.inputs.mask[row * 96 + 40] = 0;
    }
    let base = case.inputs.base.clone();
    let mask = case.inputs.mask.clone();
    let out = postprocess_region(case.inputs, &ON)?;
    for (index, (o, b)) in out.as_chunks::<3>().0.iter().zip(base.as_chunks::<3>().0).enumerate() {
        if mask[index] == 0 {
            assert_eq!(o, b, "pixel {index} outside the mask");
        }
    }
    Ok(())
}

/// A VAE-like tone shift plus a tint growing towards one edge: the band mode shrinks the
/// mismatch at the band AND inside against the colour match off, and keeps the edit.
#[test]
fn band_colour_mismatch_shrinks_against_colour_off() -> Result<(), PostprocessError> {
    for (w, h, tint) in [(160, 128, 0), (160, 128, 12), (256, 256, 16), (64, 64, 8)] {
        let case = synthetic(w, h, tint);
        let band = EdgeBand::new(w, h);
        let (cx, cy, radius) = (w / 2, h / 2, w.min(h) / 4);
        let in_edit = |index: usize| {
            let (dx, dy) = ((index % w).abs_diff(cx), (index / w).abs_diff(cy));
            dx * dx + dy * dy <= radius * radius
        };
        let off = postprocess_region(case.inputs.clone(), &OFF)?;
        let on = postprocess_region(case.inputs.clone(), &ON)?;
        let full = postprocess_region(case.inputs.clone(), &EffectiveSettings { local_strength: 1.0, ..ON })?;
        let global = postprocess_region(case.inputs.clone(), &EffectiveSettings { local_strength: 0.0, ..ON })?;
        let report = |name: &str, out: &[u8]| {
            let at_band = mean_abs(out, &case.ideal, |i| band.contains_index(i));
            let inside = mean_abs(out, &case.ideal, |i| !band.contains_index(i));
            let edit = mean_abs(out, &case.ideal, in_edit);
            println!("{w}x{h} tint {tint} {name}: band {at_band:.2} interior {inside:.2} edit {edit:.2}");
            (at_band, inside, edit)
        };
        let off = report("off", &off);
        let global = report("global", &global);
        let on = report("on 60%", &on);
        let full = report("on 100%", &full);
        for (name, got) in [("global", global), ("on 60%", on), ("on 100%", full)] {
            assert!(got.0 < off.0 * 0.6, "{w}x{h} tint {tint} {name}: band {got:?} vs off {off:?}");
            assert!(got.1 < off.1, "{w}x{h} tint {tint} {name}: interior {got:?} vs off {off:?}");
            assert!(got.2 < off.2, "{w}x{h} tint {tint} {name}: the edit itself {got:?} vs off {off:?}");
        }
    }
    Ok(())
}

/// A region too small for 256 band samples is left alone by both stages (colour on = off); a
/// small region over the threshold is matched, its band capped at a quarter of the side.
#[test]
fn a_small_region_caps_its_band() -> Result<(), PostprocessError> {
    // 16x16: depth 4, 192 band pixels < 256.
    let tiny = synthetic(16, 16, 10);
    assert_eq!(EdgeBand::new(16, 16).count(), 192);
    assert_eq!(postprocess_region(tiny.inputs.clone(), &ON)?, postprocess_region(tiny.inputs, &OFF)?);
    // 40x24: depth 6 (24 / 4), 960 - 28·12 = 624 band pixels.
    let small = synthetic(40, 24, 10);
    let band = EdgeBand::new(40, 24);
    assert_eq!((band.depth(), band.count()), (6, 624));
    let (_, path) = match_color_band(&small.inputs.base, &small.inputs.generated, &band);
    assert_eq!(path, GlobalPath::Band);
    assert_ne!(postprocess_region(small.inputs.clone(), &ON)?, postprocess_region(small.inputs, &OFF)?);
    Ok(())
}

/// Local strength 0 is the global band stage alone: with a hard edge the result is exactly
/// `match_color_band`'s output.
#[test]
fn strength_zero_is_the_global_stage_alone() -> Result<(), PostprocessError> {
    let case = synthetic(120, 90, 12);
    let (global, path) = match_color_band(&case.inputs.base, &case.inputs.generated, &EdgeBand::new(120, 90));
    assert_eq!(path, GlobalPath::Band);
    let hard = EffectiveSettings { feather_px: 0, local_strength: 0.0, ..ON };
    assert_eq!(postprocess_region(case.inputs.clone(), &hard)?, global);
    assert_ne!(postprocess_region(case.inputs, &EffectiveSettings { feather_px: 0, ..ON })?, global, "60 % adds the offset field");
    Ok(())
}

/// Equal inputs give equal bytes, whatever the thread count and the processor's history.
#[test]
fn band_mode_is_deterministic() -> TestResult {
    let case = synthetic(150, 110, 14);
    let wide = postprocess_region(case.inputs.clone(), &ON)?;
    let single = rayon::ThreadPoolBuilder::new().num_threads(1).build()?;
    let narrow = single.install(|| postprocess_region(case.inputs.clone(), &ON))?;
    assert_eq!(wide, narrow);
    let mut cached = RegionPostprocessor::new(case.inputs.clone())?;
    cached.render(&EffectiveSettings { local_scale_px: 48, ..ON })?;
    cached.render(&EffectiveSettings { local_strength: 1.0, ..ON })?;
    assert_eq!(cached.render(&ON)?, wide, "a re-render equals a fresh one");
    Ok(())
}

/// The cost of a cold band-mode render (and of a strength-only re-blend) at 2048².
#[test]
#[ignore = "release benchmark; run with --release -- --ignored --nocapture"]
fn bench_band_mode_2048_square() -> TestResult {
    let case = synthetic(2048, 2048, 12);
    let started = std::time::Instant::now();
    postprocess_region(case.inputs.clone(), &OFF)?;
    println!("2048x2048 colour off (feather + composite only): {:.1} ms", started.elapsed().as_secs_f64() * 1000.0);
    for scale in [24, 8] {
        let mut processor = RegionPostprocessor::new(case.inputs.clone())?;
        let started = std::time::Instant::now();
        let out = processor.render(&EffectiveSettings { local_scale_px: scale, ..ON })?;
        let cold = started.elapsed().as_secs_f64() * 1000.0;
        let started = std::time::Instant::now();
        processor.render(&EffectiveSettings { local_scale_px: scale, local_strength: 0.3, ..ON })?;
        let reblend = started.elapsed().as_secs_f64() * 1000.0;
        println!("band mode 2048x2048, scale {scale}: cold {cold:.1} ms, strength-only re-blend {reblend:.1} ms");
        assert_eq!(out.len(), case.inputs.base.len());
    }
    Ok(())
}
