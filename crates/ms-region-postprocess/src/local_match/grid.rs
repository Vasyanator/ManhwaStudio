/*
File: local_match/grid.rs

Purpose:
The float32 field type and the low-pass helpers of the local colour match: block-mean
downscale, bilinear upscale, separable Gaussian blur, the sigma-adaptive smoothing that combines
them, and reflect-101 padding. Each helper is a literal port of the numpy FALLBACK path of the
reference (`region_imaging.py` at `c11c4eb`: `_box_downsample`, `_upsample_linear`,
`_gaussian_blur`, `_smooth`, `_reflect_pad`, `_reflect101_indices`), including numpy's float32
summation order, so the parity fixtures hold to rounding noise.

Key structures:
- `Field`: row-major, channel-interleaved `height x width x channels` f32 buffer.
- `AxisWeights`: the per-axis sample positions of the bilinear upscale.

Key functions:
- `box_downsample_into()`, `box_moments_into()`, `upsample_linear()`, `upsample_row()` /
  `blend_rows()` / `blend_col()`, `gaussian_blur()`, `smooth()`, `reflect_fill()`,
  `reflect101()`, `pairwise_sum()`, `for_each_row()`.

Notes:
numpy order facts (probed on numpy 2.4.4): a block mean over a ONE-channel field adds each block
row as numpy's pairwise sum of that row (the reduce inner loop), a multi-channel one adds every
element sequentially in row-major block order. Every helper is row-separable; `for_each_row` is
the single place that decides how rows are scheduled.
*/

use std::borrow::Cow;
use std::ops::{Add, Range};

use rayon::prelude::*;

use super::LocalMatchError;

/// Row-major, channel-interleaved `height x width x channels` float32 field.
///
/// Invariant: `data.len() == height * width * channels` (all constructors check the product).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Field {
    pub(super) height: usize,
    pub(super) width: usize,
    pub(super) channels: usize,
    pub(super) data: Vec<f32>,
}

impl Field {
    /// A zero-filled field.
    ///
    /// # Errors
    /// `SizeOverflow` when `height * width * channels` overflows `usize`.
    pub(super) fn zeros(height: usize, width: usize, channels: usize) -> Result<Self, LocalMatchError> {
        let len = checked_len(height, width, channels)?;
        Ok(Self { height, width, channels, data: vec![0.0; len] })
    }

    /// Wraps `data` as a field.
    ///
    /// # Errors
    /// `SizeOverflow` when the product overflows, `ShapeMismatch` when `data` has another length.
    pub(super) fn from_vec(height: usize, width: usize, channels: usize, data: Vec<f32>) -> Result<Self, LocalMatchError> {
        let len = checked_len(height, width, channels)?;
        if data.len() != len {
            return Err(LocalMatchError::ShapeMismatch { what: "field", expected: len, actual: data.len() });
        }
        Ok(Self { height, width, channels, data })
    }

    /// Number of f32 values in one row.
    pub(super) fn row_len(&self) -> usize {
        self.width * self.channels
    }

    /// The `channels` values of pixel (`row`, `col`).
    pub(super) fn pixel(&self, row: usize, col: usize) -> &[f32] {
        let start = (row * self.width + col) * self.channels;
        &self.data[start..start + self.channels]
    }

    /// One value: pixel (`row`, `col`), channel `channel`.
    pub(super) fn at(&self, row: usize, col: usize, channel: usize) -> f32 {
        self.data[(row * self.width + col) * self.channels + channel]
    }
}

/// `height * width * channels` with overflow reported as `SizeOverflow`.
fn checked_len(height: usize, width: usize, channels: usize) -> Result<usize, LocalMatchError> {
    height.checked_mul(width).and_then(|pixels| pixels.checked_mul(channels)).ok_or(LocalMatchError::SizeOverflow { width, height })
}

/// numpy `astype(float32)` of a float64: round to nearest.
#[expect(clippy::cast_possible_truncation, reason = "f64 -> f32 rounding to nearest IS the numpy float32 cast this port reproduces")]
pub(super) fn to_f32(value: f64) -> f32 {
    value as f32
}

/// An index or count as float32 (numpy `arange(..., dtype=float32)`, an integer divisor).
#[expect(clippy::cast_precision_loss, reason = "field sizes stay far below 2^24, where every integer is exact in f32")]
pub(super) fn index_f32(value: usize) -> f32 {
    value as f32
}

/// An index or count as float64 (Python int -> float).
#[expect(clippy::cast_precision_loss, reason = "field sizes and scales stay far below 2^53, where every integer is exact in f64")]
pub(super) fn index_f64(value: usize) -> f64 {
    value as f64
}

/// A non-negative, finite, already integral f64 (a `floor` / `ceil` / `round` result) as `usize`.
///
/// # Errors
/// `SizeOverflow` (reported for `width` x `height`) when `value` is negative, not finite or
/// beyond `usize`; callers bound their inputs, so this only guards the conversion itself.
pub(super) fn integral_to_usize(value: f64, width: usize, height: usize) -> Result<usize, LocalMatchError> {
    // 2^53: every integral f64 below it converts exactly; the bound is far beyond any field size.
    const EXACT_LIMIT: f64 = 9_007_199_254_740_992.0;
    if !(value.is_finite() && (0.0..EXACT_LIMIT).contains(&value)) {
        return Err(LocalMatchError::SizeOverflow { width, height });
    }
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "checked above: integral, non-negative and below 2^53")]
    let converted = value as u64;
    usize::try_from(converted).map_err(|_| LocalMatchError::SizeOverflow { width, height })
}

/// Runs `op(row_index, row_values)` on every row of a buffer of `row_len` values per row.
///
/// The one scheduling point of the row-separable passes: every row is independent and writes
/// only its own output, so the result does not depend on how rows are scheduled. Rows run on
/// the rayon pool (`par_chunks_mut`) and yield the same bytes as a sequential pass.
pub(super) fn for_each_row<T: Send>(data: &mut [T], row_len: usize, op: impl Fn(usize, &mut [T]) + Sync + Send) {
    if row_len == 0 {
        return;
    }
    data.par_chunks_mut(row_len).enumerate().for_each(|(row, values)| op(row, values));
}

/// numpy's pairwise summation (`pairwise_sum` in `loops_utils.h`) of `count` values `get(start..)`.
///
/// Below 8 values a plain left-to-right sum from zero; up to 128 eight interleaved partial sums
/// folded as `((r0+r1)+(r2+r3))+((r4+r5)+(r6+r7))` plus the tail; above that, two halves split
/// at a multiple of 8.
pub(super) fn pairwise_sum<T, G>(get: &G, start: usize, count: usize) -> T
where
    T: Copy + Default + Add<Output = T>,
    G: Fn(usize) -> T,
{
    if count < 8 {
        let mut sum = T::default();
        for index in start..start + count {
            sum = sum + get(index);
        }
        sum
    } else if count <= 128 {
        let mut lanes = [T::default(); 8];
        for (lane, value) in lanes.iter_mut().enumerate() {
            *value = get(start + lane);
        }
        let unrolled = count - count % 8;
        let mut offset = 8;
        while offset < unrolled {
            for (lane, value) in lanes.iter_mut().enumerate() {
                *value = *value + get(start + offset + lane);
            }
            offset += 8;
        }
        let mut sum = ((lanes[0] + lanes[1]) + (lanes[2] + lanes[3])) + ((lanes[4] + lanes[5]) + (lanes[6] + lanes[7]));
        for index in start + unrolled..start + count {
            sum = sum + get(index);
        }
        sum
    } else {
        let mut half = count / 2;
        half -= half % 8;
        pairwise_sum(get, start, half) + pairwise_sum(get, start + half, count - half)
    }
}

/// Block mean of a virtual `height x width x channels` source (both sizes >= 1) by `factor`,
/// written into channels `dst_channel..dst_channel + channels` of `dst`.
///
/// `get(row, col, channel)` reads the source; the ragged bottom/right edge is replicated
/// (`np.pad(mode="edge")`). `dst` must be `ceil(height / factor) x ceil(width / factor)`.
/// Summation follows numpy's order for `blocks.mean(axis=(1, 3), dtype=float32)` (file notes);
/// `factor <= 1` copies the source unchanged, as `_box_downsample` returns it.
pub(super) fn box_downsample_into<G>(height: usize, width: usize, channels: usize, factor: usize, get: G, dst: &mut Field, dst_channel: usize)
where
    G: Fn(usize, usize, usize) -> f32 + Sync + Send,
{
    let factor = factor.max(1);
    debug_assert!(height > 0 && width > 0);
    debug_assert_eq!((dst.height, dst.width), (height.div_ceil(factor), width.div_ceil(factor)));
    let dst_channels = dst.channels;
    if factor == 1 {
        for_each_row(&mut dst.data, width * dst_channels, |row, out| {
            for col in 0..width {
                for channel in 0..channels {
                    out[col * dst_channels + dst_channel + channel] = get(row, col, channel);
                }
            }
        });
        return;
    }
    let count = index_f32(factor * factor);
    let coarse_width = dst.width;
    let source = |row: usize, col: usize, channel: usize| get(row.min(height - 1), col.min(width - 1), channel);
    for_each_row(&mut dst.data, coarse_width * dst_channels, |coarse_row, out| {
        let top = coarse_row * factor;
        for coarse_col in 0..coarse_width {
            let left = coarse_col * factor;
            for channel in 0..channels {
                let mut sum = 0.0_f32;
                if channels == 1 && coarse_width == 1 {
                    // numpy coalesces the two reduced axes of a one-column grid into one run.
                    let flat = |index: usize| source(top + index / factor, left + index % factor, 0);
                    sum += pairwise_sum(&flat, 0, factor * factor);
                } else if channels == 1 {
                    for block_row in 0..factor {
                        let along = |col: usize| source(top + block_row, col, 0);
                        sum += pairwise_sum(&along, left, factor);
                    }
                } else {
                    for block_row in 0..factor {
                        for block_col in 0..factor {
                            sum += source(top + block_row, left + block_col, channel);
                        }
                    }
                }
                out[coarse_col * dst_channels + dst_channel + channel] = sum / count;
            }
        }
    });
}

/// The five moment channels of one robust estimate, block-averaged by `factor` into channels
/// 1..=5 of the 6-channel `grid` (`ceil(h / factor) x ceil(w / factor)`): `robust`,
/// `robust·L`, `robust·a`, `robust·b`, `L·(L·robust)`. Channel 0 is left untouched.
///
/// One fused pass over the samples, numerically identical to five `box_downsample_into` calls
/// (the one-channel sums per block row in pairwise order, the three-channel sums sequential).
/// A sample with zero weight contributes `±0` to every channel, which leaves each of these sums
/// (all start at `+0`) unchanged, so it is skipped in the sequential channels.
pub(super) fn box_moments_into(lab: &Field, robust: &Field, factor: usize, grid: &mut Field) {
    let (height, width) = (lab.height, lab.width);
    let factor = factor.max(1);
    let coarse_width = grid.width;
    if factor == 1 || coarse_width == 1 {
        // No block mean / numpy's coalesced one-column reduction: the generic path owns both.
        box_downsample_into(height, width, 1, factor, |row, col, _| robust.at(row, col, 0), grid, 1);
        box_downsample_into(height, width, 3, factor, |row, col, channel| lab.at(row, col, channel) * robust.at(row, col, 0), grid, 2);
        box_downsample_into(height, width, 1, factor, |row, col, _| {
            let lightness = lab.at(row, col, 0);
            lightness * (lightness * robust.at(row, col, 0))
        }, grid, 5);
        return;
    }
    let count = index_f32(factor * factor);
    for_each_row(&mut grid.data, coarse_width * 6, |coarse_row, out| {
        let mut weights = vec![0.0_f32; factor];
        let mut squares = vec![0.0_f32; factor];
        for block_row in 0..factor {
            let row = (coarse_row * factor + block_row).min(height - 1);
            let lab_row = &lab.data[row * width * 3..(row + 1) * width * 3];
            let robust_row = &robust.data[row * width..(row + 1) * width];
            for (coarse_col, cell) in out.chunks_exact_mut(6).enumerate() {
                for (block_col, (weight_slot, square_slot)) in weights.iter_mut().zip(squares.iter_mut()).enumerate() {
                    let col = (coarse_col * factor + block_col).min(width - 1);
                    let weight = robust_row[col];
                    if weight == 0.0 {
                        *weight_slot = 0.0;
                        *square_slot = 0.0;
                        continue;
                    }
                    let sample = &lab_row[col * 3..col * 3 + 3];
                    let weighted = [sample[0] * weight, sample[1] * weight, sample[2] * weight];
                    cell[2] += weighted[0];
                    cell[3] += weighted[1];
                    cell[4] += weighted[2];
                    *weight_slot = weight;
                    *square_slot = sample[0] * weighted[0];
                }
                cell[1] += pairwise_sum(&|index: usize| weights[index], 0, factor);
                cell[5] += pairwise_sum(&|index: usize| squares[index], 0, factor);
            }
        }
        for cell in out.chunks_exact_mut(6) {
            for value in &mut cell[1..] {
                *value /= count;
            }
        }
    });
}

/// Block mean of a whole field (`_box_downsample`); `factor <= 1` borrows the field unchanged.
///
/// # Errors
/// `SizeOverflow` when the output size overflows.
pub(super) fn box_downsample(field: &Field, factor: usize) -> Result<Cow<'_, Field>, LocalMatchError> {
    if factor <= 1 || field.height == 0 || field.width == 0 {
        return Ok(Cow::Borrowed(field));
    }
    let mut out = Field::zeros(field.height.div_ceil(factor), field.width.div_ceil(factor), field.channels)?;
    box_downsample_into(field.height, field.width, field.channels, factor, |row, col, channel| field.at(row, col, channel), &mut out, 0);
    Ok(Cow::Owned(out))
}

/// Per-axis sample positions of `_upsample_linear`'s numpy fallback for one axis.
#[derive(Debug)]
pub(super) struct AxisWeights {
    low: Vec<usize>,
    high: Vec<usize>,
    t: Vec<f32>,
}

impl AxisWeights {
    /// Positions of the fine output samples `fine` over `coarse` cells (`coarse >= 1`,
    /// `factor >= 1`): cell `i` is centred on fine coordinate `i·factor + (factor - 1) / 2`,
    /// clamped at the outer centres. All in f32, as numpy computes it. A sub-range gives the
    /// samples of a cropped output (the crop of a full upscale, computed directly).
    pub(super) fn new(fine: Range<usize>, coarse: usize, factor: usize) -> Self {
        let last = coarse.saturating_sub(1);
        let last_f = index_f32(last);
        let factor_f = index_f32(factor);
        let mut low = Vec::with_capacity(fine.len());
        let mut high = Vec::with_capacity(fine.len());
        let mut t = Vec::with_capacity(fine.len());
        for index in fine {
            let position = ((index_f32(index) + 0.5) / factor_f - 0.5).clamp(0.0, last_f);
            // `position` lies in 0..=last (< 2^24), so its floor is an exact small integer.
            #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "clamped to 0..=coarse-1 just above")]
            let floor = position.floor() as usize;
            low.push(floor);
            high.push((floor + 1).min(last));
            t.push(position - index_f32(floor));
        }
        Self { low, high, t }
    }

    /// The (low, high, t) sample of output index `index` (relative to the range start).
    pub(super) fn at(&self, index: usize) -> (usize, usize, f32) {
        (self.low[index], self.high[index], self.t[index])
    }
}

/// First half of one output row of the bilinear upscale: the coarse row blended vertically.
///
/// `row_low` / `row_high` / `row_t` are this output row's vertical sample (`AxisWeights::at`);
/// `scratch` receives `field.width x channels.len()` values. Mirrors numpy: rows first.
pub(super) fn blend_rows(field: &Field, channels: &Range<usize>, (row_low, row_high, row_t): (usize, usize, f32), scratch: &mut Vec<f32>) {
    let inverse_t = 1.0 - row_t;
    scratch.clear();
    for col in 0..field.width {
        let upper = field.pixel(row_low, col);
        let lower = field.pixel(row_high, col);
        for channel in channels.clone() {
            scratch.push(upper[channel] * inverse_t + lower[channel] * row_t);
        }
    }
}

/// Second half: output column `index` of `cols` from a `blend_rows` scratch of `k` channels,
/// written into `out[..k]`.
pub(super) fn blend_col(scratch: &[f32], k: usize, cols: &AxisWeights, index: usize, out: &mut [f32]) {
    let (low, high, t) = cols.at(index);
    let inverse = 1.0 - t;
    for (channel, value) in out[..k].iter_mut().enumerate() {
        *value = scratch[low * k + channel] * inverse + scratch[high * k + channel] * t;
    }
}

/// One output row of the bilinear upscale of channels `channels` of `field` (`blend_rows`
/// then `blend_col` for every column); `out` receives `cols.len() x channels.len()` values.
pub(super) fn upsample_row(field: &Field, channels: &Range<usize>, row_weights: (usize, usize, f32), cols: &AxisWeights, scratch: &mut Vec<f32>, out: &mut [f32]) {
    let k = channels.len();
    blend_rows(field, channels, row_weights, scratch);
    for (index, values) in out.chunks_exact_mut(k).enumerate() {
        blend_col(scratch, k, cols, index, values);
    }
}

/// Bilinear inverse of `box_downsample` (`_upsample_linear`): channels `channels` of the grid
/// `field` back to `height x width`; `factor <= 1` crops instead.
///
/// # Errors
/// `SizeOverflow` when the output size overflows.
pub(super) fn upsample_linear(field: &Field, channels: &Range<usize>, factor: usize, height: usize, width: usize) -> Result<Field, LocalMatchError> {
    let k = channels.len();
    let mut out = Field::zeros(height, width, k)?;
    if factor <= 1 {
        for_each_row(&mut out.data, width * k, |row, values| {
            for col in 0..width {
                let pixel = field.pixel(row, col);
                values[col * k..(col + 1) * k].copy_from_slice(&pixel[channels.clone()]);
            }
        });
        return Ok(out);
    }
    let rows = AxisWeights::new(0..height, field.height, factor);
    let cols = AxisWeights::new(0..width, field.width, factor);
    for_each_row(&mut out.data, width * k, |row, values| {
        let mut scratch = Vec::with_capacity(field.width * k);
        upsample_row(field, channels, rows.at(row), &cols, &mut scratch, values);
    });
    Ok(out)
}

/// Maps any integer index onto `0..length` by reflect-101 (`cv2.BORDER_REFLECT_101`), at any
/// distance (`_reflect101_indices`). `length` must be at least 1.
pub(super) fn reflect101(index: isize, length: usize) -> usize {
    if length <= 1 {
        return 0;
    }
    let period = 2 * (length - 1);
    let folded = index.unsigned_abs() % period;
    if folded >= length { period - folded } else { folded }
}

/// Fills the border of a padded field in place by reflect-101 (`_reflect_pad`, at any width)
/// from its region, which occupies rows `top..top + height` and columns `left..left + width`
/// (both sizes >= 1) and must already be written.
pub(super) fn reflect_fill(field: &mut Field, top: usize, left: usize, height: usize, width: usize) {
    let k = field.channels;
    let row_len = field.row_len();
    let padded_width = field.width;
    // Region rows: their left and right borders, from their own region columns.
    for_each_row(&mut field.data, row_len, |row, values| {
        if !(top..top + height).contains(&row) {
            return;
        }
        for col in (0..left).chain(left + width..padded_width) {
            let source = left + reflect101(col.cast_signed() - left.cast_signed(), width);
            values.copy_within(source * k..(source + 1) * k, col * k);
        }
    });
    // Border rows: whole (already horizontally filled) region rows.
    for row in (0..top).chain(top + height..field.height) {
        let source = top + reflect101(row.cast_signed() - top.cast_signed(), height);
        field.data.copy_within(source * row_len..(source + 1) * row_len, row * row_len);
    }
}

/// Separable Gaussian blur, reflect-101 border (`_gaussian_blur`'s numpy fallback).
///
/// The kernel is cv2's for a float image with `ksize=(0, 0)`: `round_half_even(8σ + 1) | 1`
/// taps, computed in f64, normalized, then cast to f32; each output is the f32 sum of
/// `weight · sample` in tap order, rows first. `sigma <= 0` returns the field unchanged.
///
/// # Errors
/// `SizeOverflow` when the tap count cannot be represented (an unbounded sigma).
pub(super) fn gaussian_blur(field: &Field, sigma: f64) -> Result<Field, LocalMatchError> {
    if sigma <= 0.0 || field.height == 0 || field.width == 0 {
        return Ok(field.clone());
    }
    let taps = integral_to_usize((8.0 * sigma + 1.0).round_ties_even(), field.width, field.height)? | 1;
    let radius = taps / 2;
    let radius_i = isize::try_from(radius).map_err(|_| LocalMatchError::SizeOverflow { width: field.width, height: field.height })?;
    let denominator = 2.0 * sigma * sigma;
    let raw: Vec<f64> = (0..taps).map(|tap| {
        let offset = index_f64(tap) - index_f64(radius);
        (-(offset * offset) / denominator).exp()
    }).collect();
    let total: f64 = pairwise_sum(&|index: usize| raw[index], 0, taps);
    let kernel: Vec<f32> = raw.iter().map(|&weight| to_f32(weight / total)).collect();

    let k = field.channels;
    let row_len = field.row_len();
    // Axis 0: every output row is the weighted sum of reflected input rows.
    let mut vertical = Field::zeros(field.height, field.width, k)?;
    for_each_row(&mut vertical.data, row_len, |row, values| {
        for (tap, &weight) in kernel.iter().enumerate() {
            let source = reflect101(row.cast_signed() - radius_i + tap.cast_signed(), field.height);
            let input = &field.data[source * row_len..(source + 1) * row_len];
            for (value, &sample) in values.iter_mut().zip(input) {
                *value += weight * sample;
            }
        }
    });
    // Axis 1: along each row of the vertical result. Tap-major, so the interior span of each
    // tap is one contiguous (vectorizable) slice; every output still sums its taps in order.
    let width = field.width;
    let mut out = Field::zeros(field.height, width, k)?;
    for_each_row(&mut out.data, row_len, |row, values| {
        let input = &vertical.data[row * row_len..(row + 1) * row_len];
        for (tap, &weight) in kernel.iter().enumerate() {
            let shift = tap.cast_signed() - radius_i;
            // Output columns whose source `col + shift` lies inside the row.
            let low = shift.min(0).unsigned_abs().min(width);
            let high = width.saturating_sub(shift.max(0).unsigned_abs()).max(low);
            for col in (0..low).chain(high..width) {
                let source = reflect101(col.cast_signed() + shift, width);
                for channel in 0..k {
                    values[col * k + channel] += weight * input[source * k + channel];
                }
            }
            if low < high {
                // `low + shift >= 0` whenever the interior span is non-empty.
                let source_start = (low.cast_signed() + shift).unsigned_abs() * k;
                for (value, &sample) in values[low * k..high * k].iter_mut().zip(&input[source_start..]) {
                    *value += weight * sample;
                }
            }
        }
    });
    Ok(out)
}

/// Gaussian low-pass with standard deviation `sigma` cells (`_smooth`).
///
/// A sigma of at least `2 · LOCAL_MATCH_GRID_SIGMA` is applied on a grid
/// `floor(sigma / LOCAL_MATCH_GRID_SIGMA)` times coarser (box down, blur, bilinear up), so the
/// cost does not grow with sigma. `sigma <= 0` returns the field unchanged.
///
/// # Errors
/// `SizeOverflow` from the helpers.
pub(super) fn smooth(field: &Field, sigma: f64) -> Result<Field, LocalMatchError> {
    if sigma <= 0.0 {
        return Ok(field.clone());
    }
    let factor = integral_to_usize((sigma / super::LOCAL_MATCH_GRID_SIGMA).floor(), field.width, field.height)?;
    if factor < 2 {
        return gaussian_blur(field, sigma);
    }
    let coarse = box_downsample(field, factor)?;
    let factor_f = index_f64(factor);
    let coarse_sigma = (sigma * sigma - (factor_f * factor_f - 1.0) / 12.0).max(0.0).sqrt() / factor_f;
    let blurred = gaussian_blur(&coarse, coarse_sigma)?;
    upsample_linear(&blurred, &(0..field.channels), factor, field.height, field.width)
}
