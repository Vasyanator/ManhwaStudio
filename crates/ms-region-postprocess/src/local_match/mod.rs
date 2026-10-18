/*
File: local_match/mod.rs

Purpose:
The local CIELAB colour match of a generated region: after the global mean/std match fixed the
VAE's tone shift, it pulls the LOW frequencies inside the sample mask towards the surroundings
(lightness mean + contrast, a/b mean shift), per pixel, at a user scale. A literal port of
`match_color_local` and `_robust_local_moments` of the reference Python
(`modules/ai_backend/inpaint/region_imaging.py` at `c11c4eb`), numpy-fallback paths included.

Key structures:
- `LocalMatchError`: typed refusals (shape, size overflow, scale, strength, margin dilation).
- `LocalSampling`: the sample-mask half, built once per region (margin, sample sets).
- `LocalFields`: the correction fields of one current image at one scale; `apply()` is the only
  step `strength` enters.
- `Geometry`: grid factor, reflect padding and cascade sigmas shared by both sides.
- `ScaleEstimate`: one cascade level's robust moments, share and confidence on the base grid.

Key functions:
- `LocalSampling::new()` -> `LocalSampling::fields()` -> `LocalFields::apply()`: the pipeline's
  three cached steps; `match_color_local()` (tests) is their one-shot composition.
- `robust_local_moments()`, `robust_scale()`, `estimate()`, `similarity_weights()`.

Algorithm (contract of the reference, see `match_color_local`'s doc comment):
- target statistics: the reference image's dominant local colour over pixels outside the sample
  mask grown by `LOCAL_MATCH_VALID_MARGIN_PX`, carried into the mask by a cascade of scales
  `scale·2^i` (`LOCAL_MATCH_LEVELS`) and finally the global robust moments;
- current statistics: the current image's dominant local colour over pixels INSIDE the mask,
  finest scale only;
- both sides are robust modes (`LOCAL_MATCH_ROBUST_ITERATIONS` reweightings by
  `exp(-ΔE² / (2·LOCAL_MATCH_SIMILARITY_DE²))`), evaluated on a grid `floor(scale / 4)` times
  coarser over a reflect-101 padded copy; coarse levels use jittered point samples (`jitter.rs`);
- the correction fades to the identity where either side's similar-pixel share is below
  `LOCAL_MATCH_MIN_SHARE`, and is blended by `strength`.

Notes:
Deliberate deviation: the coarse-level jitter uses splitmix64 instead of numpy's PCG64 (see
`jitter.rs`); the parity fixtures are generated with the same rule. Float work is f32 with
numpy 2's weak-scalar semantics; statistics are f64 sums in a fixed sequential order; parallelism
is only over rows with per-row outputs (`grid::for_each_row`) and between the two sides (which
share only read-only inputs), so the result is deterministic. GUI-free, no I/O, no logging: the
caller logs and times the run.
*/

mod grid;
mod jitter;
mod lab;
#[cfg(test)]
mod tests;

use grid::{AxisWeights, Field, box_moments_into, for_each_row, index_f64, integral_to_usize, reflect_fill, smooth, to_f32, blend_col, blend_rows, upsample_linear};
use lab::LabTables;

/// Margin, in pixels, between the sample mask and the pixels the target statistics come from:
/// a text outline the mask does not quite cover sits at its edge (reference
/// `LOCAL_MATCH_VALID_MARGIN_PX`).
pub(crate) const LOCAL_MATCH_VALID_MARGIN_PX: u32 = 3;

/// Minimum number of valid (outside-margin) pixels; below it the local match is skipped
/// (the reference's `MIN_COLOR_MATCH_SAMPLES`, owned by the global match).
const MIN_VALID_SAMPLES: usize = crate::global_match::MIN_SAMPLES;

/// Number of scales of the target's cascade fill (`s, 2s, 4s, 8s, 16s`).
const LOCAL_MATCH_LEVELS: usize = 5;

/// Valid-weight fraction at which one cascade scale is fully trusted.
const LOCAL_MATCH_CONFIDENCE: f64 = 0.25;

/// Range of the local lightness contrast gain (target std / current std).
const LOCAL_MATCH_GAIN_RANGE: (f64, f64) = (0.8, 1.4);

/// Variance floor of the local statistics, so a flat channel has a finite std.
const LOCAL_MATCH_VARIANCE_FLOOR: f64 = 1e-3;

/// Weight below which a cell counts as having no support (its ratio would be rounding noise).
const LOCAL_MATCH_MIN_WEIGHT: f64 = 1e-6;

/// Similarity scale tau (ΔE, CIE76) of the robust estimate: weight `exp(-ΔE² / (2·tau²))`.
const LOCAL_MATCH_SIMILARITY_DE: f64 = 12.0;

/// Reweighting passes of the robust estimate.
const LOCAL_MATCH_ROBUST_ITERATIONS: usize = 2;

/// Similar-pixel share below which a side's estimate is a mixture; the correction fades below it.
const LOCAL_MATCH_MIN_SHARE: f64 = 0.25;

/// Gaussian sigma, in grid cells, at which the low-pass works (grid factor `floor(scale / this)`).
const LOCAL_MATCH_GRID_SIGMA: f64 = 4.0;

/// Largest accepted `scale_px`. Not in the reference: the reflect padding grows by about
/// `4·scale` pixels per side, so an unbounded scale would exhaust memory instead of returning an
/// error. Ten times the settings' maximum (96 px).
pub(crate) const MAX_SCALE_PX: u32 = 1024;

/// Why the local colour match refused its inputs.
#[derive(Debug, thiserror::Error)]
pub enum LocalMatchError {
    /// A buffer's length does not match the region (`what` names it; RGB buffers hold 3 bytes
    /// per pixel, the mask one).
    #[error("local colour match: `{what}` holds {actual} values, expected {expected}")]
    ShapeMismatch { what: &'static str, expected: usize, actual: usize },
    /// The region (or its padded working copy) is too large to address.
    #[error("local colour match: a {width}x{height} region is too large to process")]
    SizeOverflow { width: usize, height: usize },
    /// `scale_px` is below 1.
    #[error("local colour match: the scale must be at least 1 px, got {0}")]
    InvalidScale(u32),
    /// `scale_px` exceeds `MAX_SCALE_PX`.
    #[error("local colour match: the scale {scale_px} px exceeds the supported {max_px} px")]
    ScaleTooLarge { scale_px: u32, max_px: u32 },
    /// `strength` is NaN or infinite.
    #[error("local colour match: the strength must be a finite number, got {0}")]
    NonFiniteStrength(f32),
    /// The valid-margin dilation of the sample mask failed (boxed: the crate error may wrap this one).
    #[error("local colour match: dilating the sample mask failed")]
    Margin(#[source] Box<crate::PostprocessError>),
}

/// Pulls the low frequencies of `current` inside `sample_mask` towards `reference`'s
/// surroundings, in CIELAB, and returns the new RGB8 region: the one-shot composition of
/// [`LocalSampling::new`], [`LocalSampling::fields`] and [`LocalFields::apply`] the pipeline
/// caches separately, kept as the parity tests' entry point.
///
/// `reference`, `current`: row-major interleaved RGB8, `width * height * 3` bytes;
/// `sample_mask`: `width * height` bytes, nonzero = may change and is sampled as "current",
/// zero beyond a `LOCAL_MATCH_VALID_MARGIN_PX` margin = sampled as "target". `strength` is
/// clamped to 0..=1 (0 returns `current` unchanged); `scale_px` is the Gaussian sigma of the
/// finest level, in pixels. Only pixels inside `sample_mask` are rewritten; every other byte is
/// `current`'s. Returns `current` unchanged when the mask is empty or fewer than 256 pixels are
/// valid. Deterministic for equal inputs; rounds half to even.
///
/// # Errors
/// `ShapeMismatch` / `SizeOverflow` for buffers that do not fit `width x height`,
/// `InvalidScale` for `scale_px < 1`, `ScaleTooLarge` above `MAX_SCALE_PX`,
/// `NonFiniteStrength` for a NaN or infinite `strength`, `Margin` if the mask dilation fails.
#[cfg(test)]
pub(crate) fn match_color_local(reference: &[u8], current: &[u8], sample_mask: &[u8], width: usize, height: usize, strength: f32, scale_px: u32) -> Result<Vec<u8>, LocalMatchError> {
    let pixels = width.checked_mul(height).ok_or(LocalMatchError::SizeOverflow { width, height })?;
    check_rgb("reference", reference, pixels, width, height)?;
    check_rgb("current", current, pixels, width, height)?;
    check_mask(sample_mask, pixels)?;
    check_scale(scale_px)?;
    let strength = clamp_strength(strength)?;
    if strength <= 0.0 {
        return Ok(current.to_vec());
    }
    let sampling = LocalSampling::new(reference, sample_mask, width, height)?;
    match sampling.fields(reference, current, scale_px)? {
        Some(fields) => fields.apply(current, sample_mask, strength),
        None => Ok(current.to_vec()),
    }
}

/// `ShapeMismatch` unless `buffer` holds `pixels * 3` bytes (`SizeOverflow` if that overflows).
fn check_rgb(what: &'static str, buffer: &[u8], pixels: usize, width: usize, height: usize) -> Result<(), LocalMatchError> {
    let expected = pixels.checked_mul(3).ok_or(LocalMatchError::SizeOverflow { width, height })?;
    if buffer.len() == expected { Ok(()) } else { Err(LocalMatchError::ShapeMismatch { what, expected, actual: buffer.len() }) }
}

/// `ShapeMismatch` unless the sample mask holds `pixels` bytes.
fn check_mask(sample_mask: &[u8], pixels: usize) -> Result<(), LocalMatchError> {
    if sample_mask.len() == pixels { Ok(()) } else { Err(LocalMatchError::ShapeMismatch { what: "sample_mask", expected: pixels, actual: sample_mask.len() }) }
}

/// `InvalidScale` below 1, `ScaleTooLarge` above `MAX_SCALE_PX`.
fn check_scale(scale_px: u32) -> Result<(), LocalMatchError> {
    if scale_px < 1 {
        return Err(LocalMatchError::InvalidScale(scale_px));
    }
    if scale_px > MAX_SCALE_PX {
        return Err(LocalMatchError::ScaleTooLarge { scale_px, max_px: MAX_SCALE_PX });
    }
    Ok(())
}

/// The strength clamped to 0..=1, or `NonFiniteStrength`.
fn clamp_strength(strength: f32) -> Result<f32, LocalMatchError> {
    if strength.is_finite() { Ok(strength.clamp(0.0, 1.0)) } else { Err(LocalMatchError::NonFiniteStrength(strength)) }
}

/// The half of the local match that depends only on the sample mask — the valid-margin
/// dilation and both sides' sample sets — built once per region and shared by every current
/// image, scale and strength.
#[derive(Debug)]
pub(crate) struct LocalSampling {
    width: usize,
    height: usize,
    tables: LabTables,
    /// `None`: the match is the identity whatever the current image, scale or strength (an
    /// empty sample mask, or fewer than `MIN_VALID_SAMPLES` pixels outside the margin).
    sides: Option<Sides>,
}

/// The two sides' sample sets of a non-identity match.
#[derive(Debug)]
struct Sides {
    /// Target samples: outside the sample mask grown by `LOCAL_MATCH_VALID_MARGIN_PX`.
    valid: Vec<bool>,
    /// Current samples: inside the sample mask.
    inside: Vec<bool>,
}

impl LocalSampling {
    /// Validates the shapes and builds the sample sets (the 3 px margin dilation, row-parallel).
    /// `reference` is only shape-checked here; `fields` converts it per scale (caching its Lab
    /// would cost 12 B/px for a few ms per scale change).
    ///
    /// # Errors
    /// `SizeOverflow` / `ShapeMismatch` for buffers that do not fit `width x height`, `Margin`
    /// if the mask dilation fails.
    pub(crate) fn new(reference: &[u8], sample_mask: &[u8], width: usize, height: usize) -> Result<Self, LocalMatchError> {
        let pixels = width.checked_mul(height).ok_or(LocalMatchError::SizeOverflow { width, height })?;
        check_rgb("reference", reference, pixels, width, height)?;
        check_mask(sample_mask, pixels)?;
        let tables = LabTables::new();
        if !sample_mask.iter().any(|&value| value > 0) {
            return Ok(Self { width, height, tables, sides: None });
        }
        let margin = crate::mask::dilate_ellipse(sample_mask, width, height, LOCAL_MATCH_VALID_MARGIN_PX).map_err(|error| LocalMatchError::Margin(Box::new(error)))?;
        if margin.iter().map(|&value| usize::from(value == 0)).sum::<usize>() < MIN_VALID_SAMPLES {
            return Ok(Self { width, height, tables, sides: None });
        }
        let valid: Vec<bool> = margin.iter().map(|&value| value == 0).collect();
        let inside: Vec<bool> = sample_mask.iter().map(|&value| value > 0).collect();
        Ok(Self { width, height, tables, sides: Some(Sides { valid, inside }) })
    }

    /// The strength-independent correction fields of `current` at `scale_px` (`reference` must be
    /// the image this sampling was built from): the target's
    /// cascade and the current side's moments (run concurrently) and the per-cell correction.
    /// `None` when the match is the identity for this region (see `sides`).
    ///
    /// # Errors
    /// `ShapeMismatch` / `SizeOverflow` for a `reference` / `current` that does not fit the region or a padded
    /// size that overflows, `InvalidScale` / `ScaleTooLarge` for a scale outside
    /// `1..=MAX_SCALE_PX`.
    pub(crate) fn fields(&self, reference: &[u8], current: &[u8], scale_px: u32) -> Result<Option<LocalFields>, LocalMatchError> {
        let (width, height) = (self.width, self.height);
        check_rgb("reference", reference, width * height, width, height)?;
        check_rgb("current", current, width * height, width, height)?;
        check_scale(scale_px)?;
        let Some(sides) = &self.sides else {
            return Ok(None);
        };
        let center = strided_center(&self.tables, current, width, height);
        let geometry = Geometry::new(scale_px, width, height)?;

        // Target: the reference's dominant local colour outside the margin, carried into the mask
        // by the cascade. Current: the current image's dominant local colour INSIDE the mask
        // only. The two sides share nothing but read-only inputs, so they run concurrently.
        let (target, local) = rayon::join(
            || -> Result<(Field, Field), LocalMatchError> {
                let lab = geometry.padded_centred_lab(&self.tables, reference, &sides.valid, center)?;
                let weight = geometry.padded_weight(&sides.valid)?;
                robust_local_moments(&lab, &weight, &geometry, true)
            },
            || -> Result<(Field, Field), LocalMatchError> {
                let lab = geometry.padded_centred_lab(&self.tables, current, &sides.inside, center)?;
                let weight = geometry.padded_weight(&sides.inside)?;
                robust_local_moments(&lab, &weight, &geometry, false)
            },
        );
        let ((target, target_share), (local, local_share)) = (target?, local?);
        let fields = correction_fields(&target, &target_share, &local, &local_share, center[0])?;
        Ok(Some(LocalFields { fields, geometry }))
    }
}

/// The per-cell correction of one current image at one scale: everything of the local match
/// except the final per-pixel remap, which is the only step `strength` enters.
#[derive(Debug)]
pub(crate) struct LocalFields {
    fields: Field,
    geometry: Geometry,
}

impl LocalFields {
    /// Heap bytes of the fields (the pipeline's cache budget).
    pub(crate) fn byte_size(&self) -> usize {
        self.fields.data.len() * std::mem::size_of::<f32>()
    }

    /// Remaps every pixel of `current` inside `sample_mask` by the fields, blended by `strength`
    /// (clamped to 0..=1; 0 returns `current`); every other byte is `current`'s. `current` must
    /// be the image the fields were computed for.
    ///
    /// # Errors
    /// `ShapeMismatch` for buffers that do not fit the fields' region, `NonFiniteStrength` for a
    /// NaN or infinite `strength`.
    pub(crate) fn apply(&self, current: &[u8], sample_mask: &[u8], strength: f32) -> Result<Vec<u8>, LocalMatchError> {
        let (width, height) = (self.geometry.width, self.geometry.height);
        check_rgb("current", current, width * height, width, height)?;
        check_mask(sample_mask, width * height)?;
        let strength = clamp_strength(strength)?;
        if strength <= 0.0 {
            return Ok(current.to_vec());
        }
        Ok(apply_fields(&LabTables::new(), current, sample_mask, &self.fields, &self.geometry, strength))
    }
}

/// Grid and padding geometry shared by both sides of the match.
#[derive(Debug)]
struct Geometry {
    /// Fine pixels per grid cell: `max(1, floor(scale / LOCAL_MATCH_GRID_SIGMA))`.
    factor: usize,
    /// Reflect padding above / left of the region (a whole number of cells).
    pad: usize,
    /// Reflect padding below the region (`pad` plus the remainder to a multiple of `factor`).
    pad_bottom: usize,
    /// Reflect padding right of the region (likewise).
    pad_right: usize,
    /// Region size.
    height: usize,
    width: usize,
    /// The cascade's Gaussian sigmas in fine pixels, finest first.
    sigmas_px: [f64; LOCAL_MATCH_LEVELS],
}

impl Geometry {
    /// The reference's geometry for `scale_px` (1..=`MAX_SCALE_PX`) over a `width x height` region.
    ///
    /// A grid's own reflect-101 border would mirror about the centre of its first CELL, which
    /// biases the statistics along a border the mask touches, so the fine fields are padded
    /// first, by whole cells and by the finest level's kernel reach.
    fn new(scale_px: u32, width: usize, height: usize) -> Result<Self, LocalMatchError> {
        let overflow = || LocalMatchError::SizeOverflow { width, height };
        let scale = f64::from(scale_px);
        let factor = usize::try_from(scale_px / 4).map_err(|_| overflow())?.max(1);
        let factor_f = index_f64(factor);
        let box_variance = (factor_f * factor_f - 1.0) / 12.0;
        let grid_sigma = (scale * scale - box_variance).max(0.0).sqrt() / factor_f;
        let pad = if factor == 1 { 0 } else { factor.checked_mul(integral_to_usize((4.0 * grid_sigma).ceil(), width, height)? + 1).ok_or_else(overflow)? };
        let twice_pad = pad.checked_mul(2).ok_or_else(overflow)?;
        let remainder = |length: usize| -> Result<usize, LocalMatchError> {
            let padded = length.checked_add(twice_pad).ok_or_else(overflow)?;
            Ok((factor - padded % factor) % factor)
        };
        let pad_bottom = pad + remainder(height)?;
        let pad_right = pad + remainder(width)?;
        let mut sigmas_px = [0.0; LOCAL_MATCH_LEVELS];
        let mut level_scale = scale;
        for sigma in &mut sigmas_px {
            *sigma = level_scale;
            level_scale *= 2.0;
        }
        let geometry = Self { factor, pad, pad_bottom, pad_right, height, width, sigmas_px };
        // The padded working size must be addressable (3 channels plus the 6-channel grids).
        geometry.padded_height().ok_or_else(overflow)?.checked_mul(geometry.padded_width().ok_or_else(overflow)?).and_then(|cells| cells.checked_mul(6)).ok_or_else(overflow)?;
        Ok(geometry)
    }

    /// Padded field height, `None` on overflow.
    fn padded_height(&self) -> Option<usize> {
        self.height.checked_add(self.pad)?.checked_add(self.pad_bottom)
    }

    /// Padded field width, `None` on overflow.
    fn padded_width(&self) -> Option<usize> {
        self.width.checked_add(self.pad)?.checked_add(self.pad_right)
    }

    /// A padded `channels`-channel working field whose region rows are written by
    /// `fill(row, values)` (`values` = the region's part of padded row `pad + row`) and whose
    /// border is then reflect-101 filled (`_reflect_pad` of the region).
    fn padded_field(&self, channels: usize, fill: impl Fn(usize, &mut [f32]) + Sync + Send) -> Result<Field, LocalMatchError> {
        let overflow = || LocalMatchError::SizeOverflow { width: self.width, height: self.height };
        let mut field = Field::zeros(self.padded_height().ok_or_else(overflow)?, self.padded_width().ok_or_else(overflow)?, channels)?;
        let row_len = field.row_len();
        let (start, end) = (self.pad * channels, (self.pad + self.width) * channels);
        for_each_row(&mut field.data, row_len, |padded_row, values| {
            if let Some(row) = padded_row.checked_sub(self.pad).filter(|&row| row < self.height) {
                fill(row, &mut values[start..end]);
            }
        });
        reflect_fill(&mut field, self.pad, self.pad, self.height, self.width);
        Ok(field)
    }

    /// The padded 0/1 sample weight of one side.
    fn padded_weight(&self, weight: &[bool]) -> Result<Field, LocalMatchError> {
        self.padded_field(1, |row, values| {
            for (value, &on) in values.iter_mut().zip(&weight[row * self.width..(row + 1) * self.width]) {
                *value = if on { 1.0 } else { 0.0 };
            }
        })
    }

    /// The padded `lab(rgb) - center` field of one side (f32).
    ///
    /// Only pixels with a nonzero `weight` are converted; the others hold 0. They are never
    /// read with a nonzero weight (every use is `lab · weight` or behind a zero weight), and a
    /// `±0` addend leaves every sum that starts at `+0` unchanged, so the result equals
    /// converting the whole region while skipping most `cbrt` calls.
    fn padded_centred_lab(&self, tables: &LabTables, rgb: &[u8], weight: &[bool], center: [f32; 3]) -> Result<Field, LocalMatchError> {
        self.padded_field(3, |row, values| {
            let start = row * self.width;
            for (col, out) in values.chunks_exact_mut(3).enumerate() {
                if weight[start + col] {
                    let lab = tables.to_lab(rgb_at(rgb, start + col));
                    for ((value, &channel), &c) in out.iter_mut().zip(&lab).zip(&center) {
                        *value = channel - c;
                    }
                }
            }
        })
    }
}

/// Pixel `index` of an interleaved RGB8 buffer.
fn rgb_at(rgb: &[u8], index: usize) -> [u8; 3] {
    [rgb[index * 3], rgb[index * 3 + 1], rgb[index * 3 + 2]]
}

/// The shared moment centre: the f64 mean of the Lab of every 4th row and column
/// (`lab[::4, ::4]`), cast to f32. Centring both sides on one value near their mean keeps
/// `E[x²] - E[x]²` from cancelling catastrophically in f32 (L² reaches 1e4).
fn strided_center(tables: &LabTables, rgb: &[u8], width: usize, height: usize) -> [f32; 3] {
    let mut sums = [0.0_f64; 3];
    let mut count = 0_usize;
    for row in (0..height).step_by(4) {
        for col in (0..width).step_by(4) {
            for (sum, value) in sums.iter_mut().zip(tables.to_lab(rgb_at(rgb, row * width + col))) {
                *sum += f64::from(value);
            }
            count += 1;
        }
    }
    let count = index_f64(count.max(1));
    sums.map(|sum| to_f32(sum / count))
}

/// One side's robust local moments on the base grid.
#[derive(Debug)]
struct ScaleEstimate {
    /// Centred `[L, a, b, E[L²]]` per cell (4 channels).
    value: Field,
    /// Similarity-weighted share of the sampled pixels per cell (1 channel).
    share: Field,
    /// Spatial confidence `clip(base / LOCAL_MATCH_CONFIDENCE, 0, 1)` per cell (1 channel).
    confidence: Field,
    /// The robust global moments of this level (the fallback where no sample is in reach).
    totals: [f32; 4],
    /// The robust global share of this level.
    share_total: f32,
}

/// Dominant local colour `[L, a, b, E[L²]]` (centred) on the base grid, and its share
/// (`_robust_local_moments`).
///
/// `centred`: padded fine Lab minus the shared centre; `base_weight`: padded 0/1 weight of the
/// pixels this side may sample. `cascade = true` (the target) fills the field through every
/// scale and ends with the last scale's robust global moments; `false` (the current side) uses
/// the finest scale only, whose unsupported cells already read its global moments.
fn robust_local_moments(centred: &Field, base_weight: &Field, geometry: &Geometry, cascade: bool) -> Result<(Field, Field), LocalMatchError> {
    let first = robust_scale(centred, base_weight, geometry, 0)?;
    if !cascade {
        return Ok((first.value, first.share));
    }
    let ScaleEstimate { value: mut moments, mut share, mut confidence, mut totals, mut share_total } = first;
    for level in 1..LOCAL_MATCH_LEVELS {
        let next = robust_scale(centred, base_weight, geometry, level)?;
        for (cell, c) in confidence.data.iter_mut().enumerate() {
            let keep = *c;
            for (value, &level_value) in moments.data[cell * 4..cell * 4 + 4].iter_mut().zip(&next.value.data[cell * 4..cell * 4 + 4]) {
                *value = keep * *value + (1.0 - keep) * level_value;
            }
            share.data[cell] = keep * share.data[cell] + (1.0 - keep) * next.share.data[cell];
            *c = keep.max(next.confidence.data[cell]);
        }
        totals = next.totals;
        share_total = next.share_total;
    }
    for (cell, &keep) in confidence.data.iter().enumerate() {
        for (value, &total) in moments.data[cell * 4..cell * 4 + 4].iter_mut().zip(&totals) {
            *value = keep * *value + (1.0 - keep) * total;
        }
        share.data[cell] = keep * share.data[cell] + (1.0 - keep) * share_total;
    }
    Ok((moments, share))
}

/// One cascade level: stride `2^level` jittered point samples (all pixels at level 0), its
/// own grid `factor·2^level` px a cell, the initial weighted estimate plus
/// `LOCAL_MATCH_ROBUST_ITERATIONS` reweightings, upsampled back to the base grid.
fn robust_scale(centred: &Field, base_weight: &Field, geometry: &Geometry, level: usize) -> Result<ScaleEstimate, LocalMatchError> {
    let overflow = || LocalMatchError::SizeOverflow { width: geometry.width, height: geometry.height };
    let stride = 1_usize.checked_shl(u32::try_from(level).map_err(|_| overflow())?).ok_or_else(overflow)?;
    let sampled;
    let (lab, base) = if stride == 1 {
        (centred, base_weight)
    } else {
        sampled = sample_level(centred, base_weight, stride, level)?;
        (&sampled.0, &sampled.1)
    };
    let factor = geometry.factor;
    let cell_px = factor * stride;
    let cell_f = index_f64(cell_px);
    let sigma_px = geometry.sigmas_px[level];
    let sigma = (sigma_px * sigma_px - (cell_f * cell_f - 1.0) / 12.0).max(0.0).sqrt() / cell_f;
    // The first estimate's robust weight IS the base, so its channel 1 is the base grid.
    let first_grid = moment_grid(lab, base, None, factor)?;
    let base_grid = Field::from_vec(first_grid.height, first_grid.width, 1, first_grid.data.chunks_exact(6).map(|cell| cell[0]).collect())?;
    // The region's own cells (a ragged edge cell holds a few reflected pixels, negligible for a
    // global fallback); clamped like a Python slice.
    let top = geometry.pad;
    let bottom = geometry.pad + geometry.height;
    let left = geometry.pad;
    let right = geometry.pad + geometry.width;
    let cells = Cells {
        rows: (top / cell_px).min(base_grid.height)..bottom.div_ceil(cell_px).min(base_grid.height),
        cols: (left / cell_px).min(base_grid.width)..right.div_ceil(cell_px).min(base_grid.width),
    };
    let base_sum = cells.sum_channel(&base_grid, 0);

    let mut current = estimate(&first_grid, &cells, base_sum, sigma)?;
    for _ in 0..LOCAL_MATCH_ROBUST_ITERATIONS {
        let robust = similarity_weights(lab, base, &current.value, factor)?;
        current = estimate(&moment_grid(lab, &robust, Some(&base_grid), factor)?, &cells, base_sum, sigma)?;
    }
    if stride == 1 {
        return Ok(current);
    }
    // This scale's cells span exactly `stride x stride` base-grid cells.
    let grid_height = base_weight.height / factor;
    let grid_width = base_weight.width / factor;
    let cell_count = current.value.height * current.value.width;
    let mut stacked = Vec::with_capacity(cell_count * 6);
    for cell in 0..cell_count {
        stacked.extend_from_slice(&current.value.data[cell * 4..cell * 4 + 4]);
        stacked.push(current.share.data[cell]);
        stacked.push(current.confidence.data[cell]);
    }
    let stacked = Field::from_vec(current.value.height, current.value.width, 6, stacked)?;
    let up = upsample_linear(&stacked, &(0..6), stride, grid_height, grid_width)?;
    let mut value = Vec::with_capacity(grid_height * grid_width * 4);
    let mut share = Vec::with_capacity(grid_height * grid_width);
    let mut confidence = Vec::with_capacity(grid_height * grid_width);
    for cell in up.data.chunks_exact(6) {
        value.extend_from_slice(&cell[..4]);
        share.push(cell[4]);
        confidence.push(cell[5]);
    }
    Ok(ScaleEstimate {
        value: Field::from_vec(grid_height, grid_width, 4, value)?,
        share: Field::from_vec(grid_height, grid_width, 1, share)?,
        confidence: Field::from_vec(grid_height, grid_width, 1, confidence)?,
        totals: current.totals,
        share_total: current.share_total,
    })
}

/// The jittered point samples of one level: `(lab, base)` fields of `ceil(H / stride) x ceil(W / stride)`.
fn sample_level(centred: &Field, base_weight: &Field, stride: usize, level: usize) -> Result<(Field, Field), LocalMatchError> {
    let level_u32 = u32::try_from(level).map_err(|_| LocalMatchError::SizeOverflow { width: centred.width, height: centred.height })?;
    let samples = jitter::jittered_sample_indices(base_weight.height, base_weight.width, stride, level_u32);
    let mut lab = Field::zeros(samples.rows, samples.cols, 3)?;
    let mut base = Field::zeros(samples.rows, samples.cols, 1)?;
    for (index, (&row, &col)) in samples.row_index.iter().zip(&samples.col_index).enumerate() {
        lab.data[index * 3..index * 3 + 3].copy_from_slice(centred.pixel(row, col));
        base.data[index] = base_weight.at(row, col, 0);
    }
    Ok((lab, base))
}

/// A rectangle of grid cells (the region's own cells of one level).
#[derive(Debug)]
struct Cells {
    rows: std::ops::Range<usize>,
    cols: std::ops::Range<usize>,
}

impl Cells {
    /// f64 sum of channel `channel` of `grid` over these cells, row-major.
    fn sum_channel(&self, grid: &Field, channel: usize) -> f64 {
        let mut sum = 0.0;
        for row in self.rows.clone() {
            for col in self.cols.clone() {
                sum += f64::from(grid.at(row, col, channel));
            }
        }
        sum
    }
}

/// The 6-channel moment grid of one estimate, in the reference's concatenation order:
/// base, robust, robust·Lab (3), L·(L·robust). `base_grid` `None` means `robust` is the base
/// itself (the first estimate of a level), whose block mean is then channel 1.
fn moment_grid(lab: &Field, robust: &Field, base_grid: Option<&Field>, factor: usize) -> Result<Field, LocalMatchError> {
    let factor = factor.max(1);
    let mut grid = Field::zeros(lab.height.div_ceil(factor), lab.width.div_ceil(factor), 6)?;
    box_moments_into(lab, robust, factor, &mut grid);
    match base_grid {
        Some(base) => {
            for (cell, &value) in grid.data.chunks_exact_mut(6).zip(&base.data) {
                cell[0] = value;
            }
        }
        None => {
            for cell in grid.data.chunks_exact_mut(6) {
                cell[0] = cell[1];
            }
        }
    }
    Ok(grid)
}

/// Normalized convolution of `[robust·Lab, robust·L²]` by `robust` over a moment grid: value,
/// share and spatial confidence per cell of this level's grid; a cell without (similar) samples
/// reads as the robust global estimate.
fn estimate(grid: &Field, cells: &Cells, base_sum: f64, sigma: f64) -> Result<ScaleEstimate, LocalMatchError> {
    let robust_sum = cells.sum_channel(grid, 1);
    let totals = if robust_sum > 0.0 { [2, 3, 4, 5].map(|channel| to_f32(cells.sum_channel(grid, channel) / robust_sum)) } else { [0.0; 4] };
    let share_total = if base_sum > 0.0 { to_f32(robust_sum / base_sum) } else { 0.0 };
    let blurred = smooth(grid, sigma)?;

    let min_weight = to_f32(LOCAL_MATCH_MIN_WEIGHT);
    let confidence_full = to_f32(LOCAL_MATCH_CONFIDENCE);
    let cell_count = grid.height * grid.width;
    let mut value = Vec::with_capacity(cell_count * 4);
    let mut share = Vec::with_capacity(cell_count);
    let mut confidence = Vec::with_capacity(cell_count);
    for cell in blurred.data.chunks_exact(6) {
        let (cell_base, cell_weight) = (cell[0], cell[1]);
        for (channel, &total) in totals.iter().enumerate() {
            value.push(if cell_weight > min_weight { cell[2 + channel] / cell_weight.max(min_weight) } else { total });
        }
        share.push(if cell_base > min_weight { cell_weight / cell_base.max(min_weight) } else { share_total });
        confidence.push((cell_base / confidence_full).clamp(0.0, 1.0));
    }
    Ok(ScaleEstimate {
        value: Field::from_vec(grid.height, grid.width, 4, value)?,
        share: Field::from_vec(grid.height, grid.width, 1, share)?,
        confidence: Field::from_vec(grid.height, grid.width, 1, confidence)?,
        totals,
        share_total,
    })
}

/// The next robust weights: `base · exp(-|lab - value(sample)|² / (2·tau²))`, with `value`'s
/// Lab channels bilinearly upsampled to every sample position (mean shift towards the local mode).
fn similarity_weights(lab: &Field, base: &Field, value: &Field, factor: usize) -> Result<Field, LocalMatchError> {
    let coefficient = to_f32(-0.5 / (LOCAL_MATCH_SIMILARITY_DE * LOCAL_MATCH_SIMILARITY_DE));
    let rows = AxisWeights::new(0..lab.height, value.height, factor);
    let cols = AxisWeights::new(0..lab.width, value.width, factor);
    let mut out = Field::zeros(lab.height, lab.width, 1)?;
    for_each_row(&mut out.data, lab.width, |row, weights| {
        // `base · exp(..)` is `+0` wherever the base is 0 (the output's initial value), so rows
        // and samples without base weight skip the upsample and the `exp`.
        let base_row = &base.data[row * lab.width..(row + 1) * lab.width];
        if base_row.iter().all(|&weight| weight == 0.0) {
            return;
        }
        let mut scratch = Vec::new();
        if factor > 1 {
            blend_rows(value, &(0..3), rows.at(row), &mut scratch);
        }
        let mut mean = [0.0_f32; 3];
        for (col, (weight, &base_weight)) in weights.iter_mut().zip(base_row).enumerate() {
            if base_weight == 0.0 {
                continue;
            }
            if factor > 1 {
                blend_col(&scratch, 3, &cols, col, &mut mean);
            } else {
                mean.copy_from_slice(&value.pixel(row, col)[..3]);
            }
            let sample = lab.pixel(row, col);
            let mut distance = 0.0_f32;
            for (&x, &m) in sample.iter().zip(&mean) {
                let d = x - m;
                distance += d * d;
            }
            *weight = base_weight * (distance * coefficient).exp();
        }
    });
    Ok(out)
}

/// The four per-cell correction fields `[gain, offset_L, shift_a, shift_b]` on absolute Lab
/// values: `L' = L·gain + offset_L`, `a' = a + shift_a`, `b' = b + shift_b`. Where either side's
/// share is below `LOCAL_MATCH_MIN_SHARE` the correction fades to the identity.
fn correction_fields(target: &Field, target_share: &Field, local: &Field, local_share: &Field, center_l: f32) -> Result<Field, LocalMatchError> {
    let floor = to_f32(LOCAL_MATCH_VARIANCE_FLOOR);
    let min_share = to_f32(LOCAL_MATCH_MIN_SHARE);
    let (gain_low, gain_high) = (to_f32(LOCAL_MATCH_GAIN_RANGE.0), to_f32(LOCAL_MATCH_GAIN_RANGE.1));
    let mut fields = Vec::with_capacity(target.data.len());
    for (((t, &t_share), l), &l_share) in target.data.chunks_exact(4).zip(&target_share.data).zip(local.data.chunks_exact(4)).zip(&local_share.data) {
        let target_std = (t[3] - t[0] * t[0]).max(floor).sqrt();
        let local_std = (l[3] - l[0] * l[0]).max(floor).sqrt();
        let trust = (t_share.min(l_share) / min_share).clamp(0.0, 1.0);
        let gain = 1.0 + ((target_std / local_std).clamp(gain_low, gain_high) - 1.0) * trust;
        let offset_l = (t[0] + center_l) * trust - (l[0] + center_l) * (gain - 1.0 + trust);
        fields.extend_from_slice(&[gain, offset_l, (t[1] - l[1]) * trust, (t[2] - l[2]) * trust]);
    }
    Field::from_vec(target.height, target.width, 4, fields)
}

/// Applies the fields (upsampled to the padded fine grid and cropped to the region) to every
/// pixel inside `sample_mask`, blended by `strength`; every other pixel keeps `current`'s bytes.
fn apply_fields(tables: &LabTables, current: &[u8], sample_mask: &[u8], fields: &Field, geometry: &Geometry, strength: f32) -> Vec<u8> {
    let (width, height, factor) = (geometry.width, geometry.height, geometry.factor);
    let rows = AxisWeights::new(geometry.pad..geometry.pad + height, fields.height, factor);
    let cols = AxisWeights::new(geometry.pad..geometry.pad + width, fields.width, factor);
    let mut out = current.to_vec();
    for_each_row(&mut out, width * 3, |row, rgb| {
        let mask_row = &sample_mask[row * width..(row + 1) * width];
        if !mask_row.iter().any(|&value| value > 0) {
            return;
        }
        let mut scratch = Vec::new();
        if factor > 1 {
            blend_rows(fields, &(0..4), rows.at(row), &mut scratch);
        }
        let mut field = [0.0_f32; 4];
        for (col, &mask) in mask_row.iter().enumerate() {
            if mask == 0 {
                continue;
            }
            if factor > 1 {
                blend_col(&scratch, 4, &cols, col, &mut field);
            } else {
                field.copy_from_slice(fields.pixel(row, col));
            }
            // The same conversion the current side used, so the same values.
            let lab = tables.to_lab(rgb_at(current, row * width + col));
            let remapped = [lab[0] * field[0] + field[1], lab[1] + field[2], lab[2] + field[3]];
            let blended = [0, 1, 2].map(|channel| lab[channel] + (remapped[channel] - lab[channel]) * strength);
            rgb[col * 3..col * 3 + 3].copy_from_slice(&tables.to_srgb(blended));
        }
    });
    out
}
