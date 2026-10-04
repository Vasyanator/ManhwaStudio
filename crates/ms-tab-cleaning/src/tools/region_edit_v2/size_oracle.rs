/*
File: region_edit_v2/size_oracle.rs

Purpose:
TEST-ONLY legacy-equivalence guard for the frame's size rules. It holds a frozen copy of the
size maths as it stood before `FrameConstraints` grew the max-side, min-area, aspect-pair,
size-table and upscale rules: `check_size`, `nearest_valid_size` and the page fit
`fit_side_into_page` (then in `input.rs`). The live rules in `geometry.rs` must answer exactly
like this copy for every constraint set that uses only the four original fields, which is what
keeps the existing engines' frames behaving as they always did.

Key structures:
- `LegacyConstraints`: the four original fields

Key functions:
- `legacy_of()`: maps a live `FrameConstraints` onto the legacy shape, refusing a set that uses
  a rule the legacy code never had
- `legacy_check_size()`, `legacy_nearest_valid_size()`, `legacy_fit_side_into_page()`: the
  frozen copies
- `assert_equivalent_to_legacy()`: the size sweep that compares the live rules with the copy

Notes:
Permanent, and deliberately NOT refactored alongside `geometry.rs`: a copy that followed the
live code would stop guarding anything. The bodies are verbatim apart from the names; the
comments are trimmed to what the copy itself needs.
*/

use super::geometry::{self, FrameConstraints};

/// The original four-field size contract.
#[derive(Debug, Clone, Copy)]
pub(in crate::tools) struct LegacyConstraints {
    pub multiple: usize,
    pub min_side: usize,
    pub max_area: Option<u64>,
    pub max_aspect: Option<f32>,
}

/// The legacy shape of `c`.
///
/// Panics (test-only code) when `c` uses a rule the legacy code never had, or an asymmetric
/// aspect pair: such a set has no legacy answer to compare with, so a sweep over it would be
/// meaningless rather than green.
#[must_use]
pub(in crate::tools) fn legacy_of(c: &FrameConstraints) -> LegacyConstraints {
    assert!(c.max_side.is_none(), "max_side has no legacy equivalent: {c:?}");
    assert!(c.min_area.is_none(), "min_area has no legacy equivalent: {c:?}");
    assert!(c.sizes.is_empty(), "a size table has no legacy equivalent: {c:?}");
    assert!(c.max_upscale <= 1, "an upscale allowance has no legacy equivalent: {c:?}");
    let max_aspect = c.aspect.map(|a| {
        assert_eq!(a.max_w_over_h.to_bits(), a.max_h_over_w.to_bits(), "an asymmetric aspect has no legacy equivalent: {c:?}");
        a.max_w_over_h
    });
    LegacyConstraints { multiple: c.multiple, min_side: c.min_side, max_area: c.max_area, max_aspect }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tools) enum LegacyViolation {
    NotMultiple,
    TooSmall,
    AreaTooLarge,
    AspectTooSteep,
}

impl LegacyViolation {
    /// The live variant this legacy one corresponds to.
    #[must_use]
    pub(in crate::tools) fn live(self) -> geometry::SizeViolation {
        match self {
            Self::NotMultiple => geometry::SizeViolation::NotMultiple,
            Self::TooSmall => geometry::SizeViolation::TooSmall,
            Self::AreaTooLarge => geometry::SizeViolation::AreaTooLarge,
            Self::AspectTooSteep => geometry::SizeViolation::AspectTooSteep,
        }
    }
}

fn grid_step(c: &LegacyConstraints) -> usize {
    c.multiple.max(1)
}

fn min_side(c: &LegacyConstraints) -> usize {
    c.min_side.max(1)
}

fn max_aspect(c: &LegacyConstraints) -> Option<f32> {
    c.max_aspect.filter(|r| r.is_finite()).map(|r| r.max(1.0))
}

fn as_u64(v: usize) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}

fn as_usize(v: u64) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

fn px_f64(v: usize) -> f64 {
    v as f64
}

/// Frozen copy of the legacy `geometry::check_size`.
#[must_use]
pub(in crate::tools) fn legacy_check_size(w: usize, h: usize, c: &LegacyConstraints) -> Option<LegacyViolation> {
    let step = grid_step(c);
    if !w.is_multiple_of(step) || !h.is_multiple_of(step) {
        return Some(LegacyViolation::NotMultiple);
    }
    let min = min_side(c);
    if w < min || h < min {
        return Some(LegacyViolation::TooSmall);
    }
    if let Some(max_area) = c.max_area
        && as_u64(w).saturating_mul(as_u64(h)) > max_area
    {
        return Some(LegacyViolation::AreaTooLarge);
    }
    if let Some(ratio) = max_aspect(c) {
        let (short, long) = if w <= h { (w, h) } else { (h, w) };
        if px_f64(long) > px_f64(short) * f64::from(ratio) {
            return Some(LegacyViolation::AspectTooSteep);
        }
    }
    None
}

/// Frozen copy of the legacy `geometry::nearest_valid_size`.
#[must_use]
pub(in crate::tools) fn legacy_nearest_valid_size(w: usize, h: usize, c: &LegacyConstraints) -> (usize, usize) {
    let step = grid_step(c);
    let min_units = min_side(c).div_ceil(step).max(1);
    let mut w_u = round_units(w, step);
    let mut h_u = round_units(h, step);
    w_u = w_u.max(min_units);
    h_u = h_u.max(min_units);
    if let Some(max_area) = c.max_area {
        let (nw, nh) = shrink_to_area(w_u, h_u, min_units, step, max_area);
        w_u = nw;
        h_u = nh;
    }
    if let Some(ratio) = max_aspect(c) {
        let (nw, nh) = shrink_to_aspect(w_u, h_u, ratio);
        w_u = nw;
        h_u = nh;
    }
    (w_u.saturating_mul(step), h_u.saturating_mul(step))
}

fn round_units(v: usize, step: usize) -> usize {
    v.saturating_add(step / 2) / step
}

fn shrink_to_area(w_u: usize, h_u: usize, min_units: usize, step: usize, max_area: u64) -> (usize, usize) {
    let cell = as_u64(step).saturating_mul(as_u64(step)).max(1);
    let budget = max_area / cell;
    let current = as_u64(w_u).saturating_mul(as_u64(h_u));
    if current <= budget {
        return (w_u, h_u);
    }
    let scale = (px_f64(as_usize(budget)) / px_f64(as_usize(current))).sqrt();
    let scaled_w = float_floor_units(px_f64(w_u) * scale).max(min_units);
    let fitted_h = as_usize(budget / as_u64(scaled_w).max(1)).min(h_u).max(min_units);
    let fitted_w = as_usize(budget / as_u64(fitted_h).max(1)).min(w_u).max(min_units);
    (fitted_w, fitted_h)
}

fn float_floor_units(v: f64) -> usize {
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    v.floor() as usize
}

fn shrink_to_aspect(w_u: usize, h_u: usize, ratio: f32) -> (usize, usize) {
    let (short, long) = if w_u <= h_u { (w_u, h_u) } else { (h_u, w_u) };
    let allowed = px_f64(short) * f64::from(ratio);
    if px_f64(long) <= allowed {
        return (w_u, h_u);
    }
    let long_new = float_floor_units(allowed).max(short);
    if w_u <= h_u { (w_u, long_new) } else { (long_new, h_u) }
}

/// Frozen copy of the legacy `input::fit_side_into_page`.
#[must_use]
pub(in crate::tools) fn legacy_fit_side_into_page(side: usize, page: usize, step: usize) -> usize {
    if side <= page {
        return side;
    }
    let step = step.max(1);
    let fitted = (page / step).saturating_mul(step);
    if fitted == 0 { page.max(1) } else { fitted }
}

/// The legacy resize snap: `nearest_valid_size`, then each side fitted into the page.
#[must_use]
pub(in crate::tools) fn legacy_snap(w: usize, h: usize, page_w: usize, page_h: usize, c: &LegacyConstraints) -> (usize, usize) {
    let (sw, sh) = legacy_nearest_valid_size(w, h, c);
    (legacy_fit_side_into_page(sw, page_w, c.multiple), legacy_fit_side_into_page(sh, page_h, c.multiple))
}

/// Page sizes the sweep fits into: narrower than most snaps, a typical page, and a huge one.
const SWEEP_PAGES: [usize; 3] = [300, 1024, 5000];

/// Sizes the sweep visits on each axis: every 7th size up to 600 (the plan's fine sweep), then
/// a coarse walk to past every area and aspect limit an engine declares.
fn sweep_sides() -> impl Iterator<Item = usize> {
    (1..=600).step_by(7).chain((0..=6000).step_by(97)).chain([0, 8, 16, 128, 1024, 1025, 1040])
}

/// Asserts that the live `check_size` and `snap_size` answer exactly like the legacy copies
/// for `c` over the whole sweep. `label` names the set in a failure.
pub(in crate::tools) fn assert_equivalent_to_legacy(c: &FrameConstraints, label: &str) {
    let legacy = legacy_of(c);
    for w in sweep_sides() {
        for h in sweep_sides() {
            assert_eq!(
                geometry::check_size(w, h, c),
                legacy_check_size(w, h, &legacy).map(LegacyViolation::live),
                "{label}: check_size({w}x{h})"
            );
            for page_w in SWEEP_PAGES {
                for page_h in SWEEP_PAGES {
                    assert_eq!(
                        geometry::snap_size(w, h, page_w, page_h, c),
                        legacy_snap(w, h, page_w, page_h, &legacy),
                        "{label}: snap_size({w}x{h}) on a {page_w}x{page_h} page"
                    );
                }
            }
        }
    }
}
