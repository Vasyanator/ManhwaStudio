/*
File: crates/ms-region-postprocess/src/fixture_tests.rs

Purpose:
Golden parity tests against `fixtures/` (generated from the Python reference by
`tools/make_region_postprocess_fixtures.py`, see `fixtures/README.md`): the ellipse kernels and
dilations (exact), the global / feather / composite stages vs the PATCHED set, the end-to-end
result vs the PATCHED set, and the drift vs the UNPATCHED set (today's shipped output).

Notes:
Crate-private so the stage functions can be tested directly. Read-only: nothing is written.
Run with `--nocapture` to see the per-case statistics.
*/

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::feather::{feather_alpha, inside_distance};
use crate::global_match::{GlobalPath, match_color_global};
use crate::mask::{dilate_ellipse, ellipse_half_widths};
use crate::{EffectiveSettings, PostprocessSettings, RegionInputs, composite, postprocess_region};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn read_json(path: &Path) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

fn read_l8(path: &Path) -> Result<(Vec<u8>, usize, usize), Box<dyn std::error::Error>> {
    let img = image::open(path)?.into_luma8();
    let (w, h) = img.dimensions();
    Ok((img.into_raw(), usize::try_from(w)?, usize::try_from(h)?))
}

fn read_rgb(path: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    Ok(image::open(path)?.into_rgb8().into_raw())
}

fn as_u64(v: &Value, key: &str) -> Result<u64, Box<dyn std::error::Error>> {
    v.get(key).and_then(Value::as_u64).ok_or_else(|| format!("missing integer `{key}`").into())
}

fn as_u32(v: &Value, key: &str) -> Result<u32, Box<dyn std::error::Error>> {
    Ok(u32::try_from(as_u64(v, key)?)?)
}

fn as_bool(v: &Value, key: &str) -> Result<bool, Box<dyn std::error::Error>> {
    v.get(key).and_then(Value::as_bool).ok_or_else(|| format!("missing bool `{key}`").into())
}

fn as_str<'a>(v: &'a Value, key: &str) -> Result<&'a str, Box<dyn std::error::Error>> {
    v.get(key).and_then(Value::as_str).ok_or_else(|| format!("missing string `{key}`").into())
}

/// One fixture case: inputs, settings and its directory.
struct Case {
    name: String,
    dir: PathBuf,
    meta: Value,
    width: usize,
    height: usize,
    base: Vec<u8>,
    reference: Option<Vec<u8>>,
    generated: Vec<u8>,
    mask: Vec<u8>,
    sample_mask: Vec<u8>,
    stored: PostprocessSettings,
    whole_region: bool,
}

impl Case {
    fn reference(&self) -> &[u8] {
        self.reference.as_deref().unwrap_or(&self.base)
    }

    fn effective(&self) -> Result<EffectiveSettings, Box<dyn std::error::Error>> {
        let e = self.meta.get("effective").ok_or("missing `effective`")?;
        let pct = u16::try_from(as_u64(e, "local_strength_pct")?)?;
        Ok(EffectiveSettings {
            feather_px: as_u32(e, "feather_px")?,
            global_match: as_bool(e, "global_match")?,
            local_strength: f32::from(pct) / 100.0,
            local_scale_px: as_u32(e, "local_scale_px")?,
        })
    }

    fn inputs(&self) -> RegionInputs {
        RegionInputs {
            width: self.width,
            height: self.height,
            base: self.base.clone(),
            reference: self.reference.clone(),
            generated: self.generated.clone(),
            mask: self.mask.clone(),
            sample_mask: Some(self.sample_mask.clone()),
        }
    }
}

fn load_cases() -> Result<Vec<Case>, Box<dyn std::error::Error>> {
    let manifest = read_json(&fixtures().join("manifest.json"))?;
    let names = manifest.get("cases").and_then(Value::as_array).ok_or("manifest without cases")?;
    let mut cases = Vec::new();
    for name in names {
        let name = name.as_str().ok_or("case name is not a string")?.to_owned();
        let dir = fixtures().join("cases").join(&name);
        let meta = read_json(&dir.join("case.json"))?;
        let (mask, width, height) = read_l8(&dir.join("mask.png"))?;
        let (sample_mask, sw, sh) = read_l8(&dir.join("sample_mask.png"))?;
        assert_eq!((sw, sh, width, height), (width, height, usize::try_from(as_u64(&meta, "width")?)?, usize::try_from(as_u64(&meta, "height")?)?), "{name}: sizes");
        let reference = if as_bool(&meta, "reference_distinct")? { Some(read_rgb(&dir.join("reference.png"))?) } else { None };
        let s = meta.get("settings").ok_or("missing `settings`")?;
        let stored = PostprocessSettings {
            mask_feather_px: as_u32(s, "mask_feather_px")?,
            color_match: as_bool(s, "color_match")?,
            color_match_local_pct: as_u32(s, "color_match_local_pct")?,
            color_match_scale_px: as_u32(s, "color_match_scale_px")?,
        };
        cases.push(Case {
            base: read_rgb(&dir.join("base.png"))?,
            generated: read_rgb(&dir.join("generated.png"))?,
            whole_region: as_bool(&meta, "whole_region")?,
            name,
            dir,
            meta,
            width,
            height,
            reference,
            mask,
            sample_mask,
            stored,
        });
    }
    assert!(cases.len() >= 8, "every fixture case is loaded");
    Ok(cases)
}

/// Absolute-difference statistics of two equal-length byte buffers, optionally restricted to
/// the pixels where `select` (one entry per pixel, `channels` values each) is nonzero.
#[derive(Debug, Clone, Copy)]
struct Diff {
    max: u8,
    p99: u8,
    differing: usize,
    count: usize,
    mean: f64,
}

fn diff(a: &[u8], b: &[u8], channels: usize, select: Option<&[u8]>) -> Diff {
    assert_eq!(a.len(), b.len());
    let mut values: Vec<u8> = Vec::with_capacity(a.len());
    for (i, (pa, pb)) in a.chunks_exact(channels).zip(b.chunks_exact(channels)).enumerate() {
        if select.is_none_or(|s| s[i] != 0) {
            values.extend(pa.iter().zip(pb).map(|(x, y)| x.abs_diff(*y)));
        }
    }
    values.sort_unstable();
    let count = values.len();
    let differing = values.iter().filter(|v| **v != 0).count();
    let max = values.last().copied().unwrap_or(0);
    let p99 = if count == 0 { 0 } else { values[(count * 99).div_ceil(100).saturating_sub(1)] };
    let sum: u64 = values.iter().map(|v| u64::from(*v)).sum();
    #[expect(clippy::cast_precision_loss, reason = "test statistics; counts far below 2^52")]
    let mean = if count == 0 { 0.0 } else { sum as f64 / count as f64 };
    Diff { max, p99, differing, count, mean }
}

/// `differing <= 0.2 %` of the compared values.
fn within_share(d: &Diff) -> bool {
    d.differing * 1000 <= d.count * 2
}

#[test]
fn ellipse_kernels_equal_opencv() -> TestResult {
    let kernels = read_json(&fixtures().join("kernels.json"))?;
    let list = kernels.get("kernels").and_then(Value::as_array).ok_or("kernels.json without kernels")?;
    assert_eq!(list.len(), 64);
    for k in list {
        let radius = as_u32(k, "radius")?;
        let expected: Vec<usize> = k
            .get("half_widths")
            .and_then(Value::as_array)
            .ok_or("missing half_widths")?
            .iter()
            .map(|v| v.as_u64().and_then(|v| usize::try_from(v).ok()).ok_or("bad half width"))
            .collect::<Result<_, _>>()?;
        assert_eq!(ellipse_half_widths(radius), expected, "radius {radius}");
    }
    Ok(())
}

#[test]
fn dilation_equals_cv2_exactly() -> TestResult {
    let dir = fixtures().join("dilation");
    let (input, w, h) = read_l8(&dir.join("input.png"))?;
    for radius in [3u32, 6, 16] {
        let (expected, ew, eh) = read_l8(&dir.join(format!("dilate_r{radius}.png")))?;
        assert_eq!((ew, eh), (w, h));
        assert_eq!(dilate_ellipse(&input, w, h, radius)?, expected, "radius {radius}");
    }
    Ok(())
}

#[test]
fn effective_settings_match_the_cases() -> TestResult {
    for case in load_cases()? {
        assert_eq!(case.stored.effective(case.whole_region), case.effective()?, "{}", case.name);
    }
    Ok(())
}

#[test]
fn global_stage_matches_patched() -> TestResult {
    for case in load_cases()? {
        let path = as_str(&case.meta, "global_path")?;
        let expected = read_rgb(&case.dir.join("patched/global.png"))?;
        let (got, got_path) = if path == "off" {
            (case.generated.clone(), None)
        } else {
            let (g, p) = match_color_global(case.reference(), &case.generated, &case.sample_mask, case.width, case.height)?;
            (g, Some(p))
        };
        let want_path = match path {
            "margin" => Some(GlobalPath::Margin),
            "unmargined" => Some(GlobalPath::Unmargined),
            "skipped" => Some(GlobalPath::Skipped),
            _ => None,
        };
        assert_eq!(got_path, want_path, "{}: branch", case.name);
        let d = diff(&got, &expected, 3, None);
        println!("{} global vs patched: {d:?}", case.name);
        // The plan's bar is `max <= 1` on <= 0.2 % of values; the port is bit-exact (numpy's
        // float32 statistics are mirrored), so any difference is a regression.
        assert!(d.max <= 1 && within_share(&d), "{}: {d:?}", case.name);
        assert_eq!(d.differing, 0, "{}: global stage is bit-exact", case.name);
    }
    Ok(())
}

#[test]
fn feather_and_composite_stages_match_patched() -> TestResult {
    for case in load_cases()? {
        let feather = case.effective()?.feather_px;
        let distance = inside_distance(&case.mask, case.width, case.height)?;
        let alpha = feather_alpha(&case.mask, &distance, feather);
        let (expected_alpha, _, _) = read_l8(&case.dir.join("patched/alpha.png"))?;
        let d = diff(&alpha, &expected_alpha, 1, None);
        println!("{} alpha vs patched: {d:?}", case.name);
        assert!(d.max <= 1 && within_share(&d), "{}: alpha {d:?}", case.name);
        // Bit-exact today (exact EDT on both sides); any difference is a regression.
        assert_eq!(d.differing, 0, "{}: alpha is bit-exact", case.name);
        // The composite alone, fed the reference's own stage outputs, must be exact.
        let local = read_rgb(&case.dir.join("patched/local.png"))?;
        let expected = read_rgb(&case.dir.join("patched/expected.png"))?;
        assert_eq!(composite::composite_over_base(&case.base, &local, &case.mask, &expected_alpha), expected, "{}: composite", case.name);
    }
    Ok(())
}

#[test]
fn end_to_end_matches_patched_and_keeps_outside_bytes() -> TestResult {
    for case in load_cases()? {
        let settings = case.effective()?;
        let got = postprocess_region(case.inputs(), &settings)?;
        let expected = read_rgb(&case.dir.join("patched/expected.png"))?;
        let all = diff(&got, &expected, 3, None);
        let inside = diff(&got, &expected, 3, Some(&case.mask));
        println!("{} expected vs patched: all {all:?}; inside mean {:.4}", case.name, inside.mean);
        assert!(all.max <= 2, "{}: {all:?}", case.name);
        assert!(inside.mean <= 0.3, "{}: inside {inside:?}", case.name);
        for (i, (g, b)) in got.chunks_exact(3).zip(case.base.chunks_exact(3)).enumerate() {
            if case.mask[i] == 0 {
                assert_eq!(g, b, "{}: pixel {i} outside the mask must be the base's bytes", case.name);
            }
        }
    }
    Ok(())
}

#[test]
fn drift_against_unpatched_is_reported() -> TestResult {
    for case in load_cases()? {
        let got = postprocess_region(case.inputs(), &case.effective()?)?;
        let unpatched = read_rgb(&case.dir.join("unpatched/expected.png"))?;
        let d = diff(&got, &unpatched, 3, None);
        let (unpatched_alpha, _, _) = read_l8(&case.dir.join("unpatched/alpha.png"))?;
        let distance = inside_distance(&case.mask, case.width, case.height)?;
        let alpha = feather_alpha(&case.mask, &distance, case.effective()?.feather_px);
        let da = diff(&alpha, &unpatched_alpha, 1, None);
        println!("{} UNPATCHED drift: expected max {} p99 {} differing {}/{}; alpha max {} p99 {}", case.name, d.max, d.p99, d.differing, d.count, da.max, da.p99);
        assert!(d.p99 <= 6, "{}: {d:?}", case.name);
    }
    Ok(())
}

#[test]
fn rerender_with_changed_settings_equals_a_fresh_render() -> TestResult {
    let cases = load_cases()?;
    let case = cases.iter().find(|c| c.name.starts_with("01")).ok_or("case 01 missing")?;
    let base = case.effective()?;
    let variants = [
        base,
        EffectiveSettings { feather_px: 3, ..base },
        EffectiveSettings { local_strength: 0.25, ..base },
        EffectiveSettings { global_match: false, local_strength: 0.0, ..base },
        EffectiveSettings { feather_px: 0, ..base },
        base,
    ];
    let mut cached = crate::RegionPostprocessor::new(case.inputs())?;
    for s in variants {
        assert_eq!(cached.render(&s)?, postprocess_region(case.inputs(), &s)?, "{s:?}");
    }
    Ok(())
}
