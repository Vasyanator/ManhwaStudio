/*
File: crates/ms-region-postprocess/src/settings.rs

Purpose:
The one owner of the region post-processing settings: the persisted struct, its defaults and
ranges, and the whole-region rule that turns stored values into what a run applies.

Key items:
- `PostprocessSettings`: the stored values (field names are persisted keys of the engines'
  settings, embedded flattened — never rename one).
- `EffectiveSettings`: what `RegionPostprocessor::render` applies.
- The `*_MAX` / `*_MIN` / `DEFAULT_*` constants: ranges and defaults, used by the UI dials too.

Notes:
Pure data, no egui, no I/O.
*/

use serde::{Deserialize, Serialize};

/// Largest inward feather (ramp width), px.
pub const FEATHER_MAX_PX: u32 = 32;
/// Default inward feather, px.
pub const DEFAULT_FEATHER_PX: u32 = 12;
/// Largest local colour-match strength, percent.
pub const LOCAL_PCT_MAX: u32 = 100;
/// Default local colour-match strength, percent.
pub const DEFAULT_LOCAL_PCT: u32 = 60;
/// Smallest local colour-match scale (Gaussian sigma of the finest level), px.
pub const SCALE_MIN_PX: u32 = 8;
/// Largest local colour-match scale, px.
pub const SCALE_MAX_PX: u32 = 96;
/// Default local colour-match scale, px.
pub const DEFAULT_SCALE_PX: u32 = 24;

/// Stored post-processing settings of a region editor engine.
///
/// The field names ARE the persisted keys (the engines embed this struct with
/// `#[serde(flatten)]`), so they must never be renamed. An absent field reads as its default; an
/// out-of-range stored value is kept as stored and clamped by [`PostprocessSettings::normalized`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostprocessSettings {
    /// Inward feather ramp width, px (`0..=FEATHER_MAX_PX`, 0 = hard edge).
    pub mask_feather_px: u32,
    /// Global colour match against the ring outside the mask (the local match refines it).
    pub color_match: bool,
    /// Local CIELAB match strength, percent (`0..=LOCAL_PCT_MAX`, 0 = global only).
    pub color_match_local_pct: u32,
    /// Local CIELAB match scale, px (`SCALE_MIN_PX..=SCALE_MAX_PX`).
    pub color_match_scale_px: u32,
}

impl Default for PostprocessSettings {
    fn default() -> Self {
        Self {
            mask_feather_px: DEFAULT_FEATHER_PX,
            color_match: true,
            color_match_local_pct: DEFAULT_LOCAL_PCT,
            color_match_scale_px: DEFAULT_SCALE_PX,
        }
    }
}

impl PostprocessSettings {
    /// Returns the settings with every numeric field clamped into its range.
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            mask_feather_px: self.mask_feather_px.min(FEATHER_MAX_PX),
            color_match: self.color_match,
            color_match_local_pct: self.color_match_local_pct.min(LOCAL_PCT_MAX),
            color_match_scale_px: self.color_match_scale_px.clamp(SCALE_MIN_PX, SCALE_MAX_PX),
        }
    }

    /// What a run applies, after [`Self::normalized`].
    ///
    /// The whole-region rule: when the mask covers the whole region (`whole_region`) there is
    /// no ring outside it to take colour statistics from, so both colour matches are off (a fit
    /// on the changed pixels would undo the edit); the feather stays, it joins the region to the
    /// page. The local match only refines the global one, so it is off whenever the global one is.
    #[must_use]
    pub fn effective(self, whole_region: bool) -> EffectiveSettings {
        let s = self.normalized();
        let global_match = s.color_match && !whole_region;
        let local_strength = if global_match { pct_to_strength(s.color_match_local_pct) } else { 0.0 };
        EffectiveSettings { feather_px: s.mask_feather_px, global_match, local_strength, local_scale_px: s.color_match_scale_px }
    }
}

/// `pct / 100` in `f32` (the reference's `pct / 100` in f64 cast to f32 gives the same value:
/// both are the correctly rounded quotient of exact operands).
fn pct_to_strength(pct: u32) -> f32 {
    // `pct <= LOCAL_PCT_MAX` (normalized), so it always fits u16; the fallback is unreachable.
    let pct = u16::try_from(pct).unwrap_or(u16::MAX);
    f32::from(pct) / 100.0
}

/// The post-processing a render applies (see [`PostprocessSettings::effective`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectiveSettings {
    /// Inward feather ramp width, px; 0 = hard edge.
    pub feather_px: u32,
    /// Global per-channel mean/std match.
    pub global_match: bool,
    /// Local CIELAB match strength in `0.0..=1.0`; 0 = off.
    pub local_strength: f32,
    /// Local CIELAB match scale, px (>= 1 when `local_strength > 0`).
    pub local_scale_px: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_values() {
        let d = PostprocessSettings::default();
        assert_eq!((d.mask_feather_px, d.color_match, d.color_match_local_pct, d.color_match_scale_px), (12, true, 60, 24));
        assert_eq!(d, d.normalized(), "defaults lie inside the ranges");
    }

    #[test]
    fn normalized_clamps_every_field() {
        let s = PostprocessSettings { mask_feather_px: 99, color_match: false, color_match_local_pct: 250, color_match_scale_px: 1 }.normalized();
        assert_eq!(s, PostprocessSettings { mask_feather_px: 32, color_match: false, color_match_local_pct: 100, color_match_scale_px: 8 });
        let s = PostprocessSettings { color_match_scale_px: 500, ..PostprocessSettings::default() }.normalized();
        assert_eq!(s.color_match_scale_px, 96);
    }

    #[test]
    fn effective_truth_table() {
        let on = PostprocessSettings::default();
        let off = PostprocessSettings { color_match: false, ..on };
        let e = on.effective(false);
        assert!(e.global_match);
        assert!((e.local_strength - 0.6).abs() < 1e-7);
        assert_eq!((e.feather_px, e.local_scale_px), (12, 24));
        for (s, whole) in [(on, true), (off, false), (off, true)] {
            let e = s.effective(whole);
            assert!(!e.global_match, "{s:?} whole={whole}");
            assert!(e.local_strength.to_bits() == 0, "local is off whenever global is");
            assert_eq!(e.feather_px, 12, "the feather survives the whole-region rule");
        }
        let zero_local = PostprocessSettings { color_match_local_pct: 0, ..on }.effective(false);
        assert!(zero_local.global_match && zero_local.local_strength.to_bits() == 0);
        let clamped = PostprocessSettings { color_match_local_pct: 400, mask_feather_px: 40, ..on }.effective(false);
        assert!((clamped.local_strength - 1.0).abs() < f32::EPSILON);
        assert_eq!(clamped.feather_px, 32);
    }

    #[test]
    fn serde_round_trip_and_absent_fields_default() -> Result<(), serde_json::Error> {
        let s = PostprocessSettings { mask_feather_px: 3, color_match: false, color_match_local_pct: 17, color_match_scale_px: 40 };
        let json = serde_json::to_string(&s)?;
        assert_eq!(json, r#"{"mask_feather_px":3,"color_match":false,"color_match_local_pct":17,"color_match_scale_px":40}"#);
        assert_eq!(serde_json::from_str::<PostprocessSettings>(&json)?, s);
        let partial: PostprocessSettings = serde_json::from_str(r#"{"color_match":false,"unknown_key":1}"#)?;
        assert_eq!(partial, PostprocessSettings { color_match: false, ..PostprocessSettings::default() });
        Ok(())
    }
}
