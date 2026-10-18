/*
File: crates/ms-region-postprocess/src/lib.rs

Purpose:
Crate root of `ms-region-postprocess`: the one owner of the FLUX klein region post-processing
(settings, mask dilation, colour match, inward feather, composite) and of its settings-section
body UI.

Exports:
- `PostprocessSettings` / `EffectiveSettings` and the range/default constants (`settings`).
- `RegionInputs`, `ColourReference`, `RegionPostprocessor`, `postprocess_region` (`pipeline`).
- `dilate_ellipse`, `MAX_DILATE_RADIUS_PX`: cv2-exact elliptical mask dilation (also the FLUX.2
  latent mask) and its radius cap.
- `draw_postprocess_body`, `section_title` (`ui`).
- `PostprocessError`: the typed error.

Notes:
The core is GUI-free, does no I/O and no logging; callers own threads, logs and textures. The
local CIELAB match lives in `local_match/` behind one crate-private function.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]

// The `ms-i18n` UI-string macros (`t!`), mounted crate-wide: the section body's labels and hints
// use the bare macro names the key-validation test scans for.
#[macro_use]
extern crate ms_i18n;

mod composite;
mod edge_band;
mod feather;
mod global_match;
mod local_match;
mod mask;
mod pipeline;
mod settings;
mod ui;

#[cfg(test)]
mod band_mode_tests;
#[cfg(test)]
mod fixture_tests;

pub use mask::{MAX_DILATE_RADIUS_PX, dilate_ellipse};
pub use pipeline::{ColourReference, RegionInputs, RegionPostprocessor, postprocess_region};
pub use settings::{
    DEFAULT_FEATHER_PX, DEFAULT_LOCAL_PCT, DEFAULT_SCALE_PX, EffectiveSettings, FEATHER_MAX_PX, LOCAL_PCT_MAX, PostprocessSettings, SCALE_MAX_PX, SCALE_MIN_PX,
};
pub use ui::{draw_postprocess_body, section_title};

/// Errors of the region post-processing API.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PostprocessError {
    /// The region has a zero width or height.
    #[error("the region is empty (zero width or height)")]
    EmptyRegion,
    /// `width * height` (or its RGB length) overflows `usize`.
    #[error("region size {width}x{height} overflows the address space")]
    SizeOverflow {
        /// Region width, px.
        width: usize,
        /// Region height, px.
        height: usize,
    },
    /// A buffer's length does not match the region size.
    #[error("buffer `{what}` has length {actual}, expected {expected}")]
    ShapeMismatch {
        /// Which buffer (`base`, `reference`, `generated`, `mask`, `sample_mask`).
        what: &'static str,
        /// Expected length in bytes.
        expected: usize,
        /// Actual length in bytes.
        actual: usize,
    },
    /// A mask dilation radius above `MAX_DILATE_RADIUS_PX`.
    #[error("mask dilation radius {radius} px exceeds the supported {max} px")]
    RadiusTooLarge {
        /// The requested radius, px.
        radius: u32,
        /// The largest accepted radius, px.
        max: u32,
    },
    /// The local match is on with a zero scale.
    #[error("local colour-match scale {0} px is invalid (must be at least 1)")]
    InvalidScale(u32),
    /// The local match strength is not finite or outside `0.0..=1.0`.
    #[error("local colour-match strength {0} is invalid (must be within 0..=1)")]
    InvalidStrength(f32),
    /// The local CIELAB match refused its input.
    #[error("local colour match failed: {reason}")]
    LocalMatch {
        /// The local stage's own error text.
        reason: String,
    },
}

/// `width * height`, or `SizeOverflow`.
fn checked_area(width: usize, height: usize) -> Result<usize, PostprocessError> {
    width.checked_mul(height).ok_or(PostprocessError::SizeOverflow { width, height })
}

/// `Ok` when `actual == expected`, else `ShapeMismatch` naming `what`.
fn check_len(what: &'static str, expected: usize, actual: usize) -> Result<(), PostprocessError> {
    if actual == expected { Ok(()) } else { Err(PostprocessError::ShapeMismatch { what, expected, actual }) }
}
