/*
File: crates/ms-raster/src/distance.rs

Purpose:
The one exact Euclidean distance transform of the project (Felzenszwalb-Huttenlocher squared
distance, evaluated in `f32`).

Key items:
- `EDT_COST_INF`: the non-seed / unreachable sentinel of the cost field and of the output.
- `EDT_MAX_DIMENSION`: the largest accepted width or height (the exact-`f32` coordinate domain).
- `squared_distance_with_costs()`: squared distance over an arbitrary per-pixel seed-cost field
  (sub-pixel seeds included).
- `squared_distance_to_mask()`: squared distance to the nearest nonzero pixel of a `u8` mask.

Notes:
Two separable passes of the 1-D lower-envelope algorithm (columns, then rows), O(width * height).
The arithmetic is the text renderer's historical implementation moved verbatim, so the glow and
dry-media effects keep their output bit for bit (`ms-text-render` pins it with a characterization
digest). Invalid shapes are typed errors; the renderer's old "all-INF / empty" fallback lives in
its own adapters.
*/

use crate::RasterError;

/// Sentinel cost marking a non-seed pixel, and the output value of an unreachable pixel.
///
/// A valid seed cost lies in `[0.0, EDT_COST_INF)`. Any other input — `>= EDT_COST_INF`,
/// negative, infinite, or NaN — is treated as "no seed here"; pixels with no reachable seed keep
/// this value in the output.
pub const EDT_COST_INF: f32 = 1.0e15;

/// Maps a caller-supplied cost onto the valid seed range.
///
/// Values in `[0.0, EDT_COST_INF)` pass through unchanged; anything else (negative,
/// `>= EDT_COST_INF`, infinite, or NaN — the range check rejects NaN) becomes the non-seed
/// sentinel `EDT_COST_INF`.
fn normalize_edt_cost(value: f32) -> f32 {
    if (0.0..EDT_COST_INF).contains(&value) {
        value
    } else {
        EDT_COST_INF
    }
}

/// Largest width or height the transform accepts: `2^24`. The envelope works on `f32`
/// coordinates, and every index of a scanline of this length (`0..2^24`) is an exact, distinct
/// `f32`; one more and two adjacent indices round to the same value, which makes an envelope
/// intersection divide by zero and the distances wrong.
pub const EDT_MAX_DIMENSION: usize = 1 << 24;

/// Validates both dimensions against `EDT_MAX_DIMENSION` (first, so an oversized empty raster
/// is refused too), then `len == width * height`, and returns that area.
fn checked_area(width: usize, height: usize, len: usize) -> Result<usize, RasterError> {
    if width > EDT_MAX_DIMENSION || height > EDT_MAX_DIMENSION {
        return Err(RasterError::DimensionTooLarge { width, height, max: EDT_MAX_DIMENSION });
    }
    let mismatch = RasterError::LengthMismatch { width, height, len };
    let area = width.checked_mul(height).ok_or_else(|| mismatch.clone())?;
    if area == len { Ok(area) } else { Err(mismatch) }
}

/// Squared Euclidean distance (px²) from every pixel of a `width * height` row-major `u8` mask to
/// the nearest nonzero ("covered") pixel.
///
/// Covered pixels are seeds at `0.0`; pixels with no reachable seed (an all-zero mask) keep
/// `EDT_COST_INF`. Exactly `squared_distance_with_costs` over `{0.0, EDT_COST_INF}` seeding.
/// Allocates the cost field, the output and one transposed scratch buffer of the same size.
///
/// # Errors
/// `RasterError::DimensionTooLarge` when `width` or `height` exceeds `EDT_MAX_DIMENSION`, and
/// `RasterError::LengthMismatch` when `mask.len() != width * height` (including when the product
/// overflows `usize`).
pub fn squared_distance_to_mask(mask: &[u8], width: usize, height: usize) -> Result<Vec<f32>, RasterError> {
    checked_area(width, height, mask.len())?;
    let costs: Vec<f32> = mask
        .iter()
        .map(|&m| if m > 0 { 0.0 } else { EDT_COST_INF })
        .collect();
    squared_distance_with_costs(&costs, width, height)
}

/// Felzenszwalb-Huttenlocher squared-distance transform, evaluated in `f32`, over an arbitrary
/// squared-distance cost field.
///
/// `costs` is row-major, length `width * height`, in px² units: `0.0` is a fully covered seed, a
/// finite value in `(0.0, EDT_COST_INF)` is a sub-pixel seed whose squared edge distance is
/// already known (e.g. `d0*d0` for a partially covered pixel), and any other value —
/// `>= EDT_COST_INF`, negative, infinite, or NaN — is normalized to the non-seed sentinel.
/// Returns, for each pixel, `min_p (dx*dx + dy*dy + costs[p])` — the squared Euclidean distance
/// to the nearest seed, biased by that seed's own initial cost. Pixels with no reachable seed
/// keep `EDT_COST_INF`. Runs in `O(width * height)`.
///
/// The lower-envelope algorithm is exact in real arithmetic and supports an arbitrary finite
/// initial cost naturally (the second pass already consumes finite squared distances from the
/// first). Evaluating it in `f32` can shift envelope intersections by rounding at large
/// magnitudes; at px²-scale costs the error is negligible. Coordinates are converted to `f32`,
/// which is why a dimension above `EDT_MAX_DIMENSION` (2^24) is refused.
///
/// # Errors
/// `RasterError::DimensionTooLarge` when `width` or `height` exceeds `EDT_MAX_DIMENSION`, and
/// `RasterError::LengthMismatch` when `costs.len() != width * height` (including when the
/// product overflows `usize`).
pub fn squared_distance_with_costs(costs: &[f32], width: usize, height: usize) -> Result<Vec<f32>, RasterError> {
    const INF: f32 = EDT_COST_INF;

    let expected_len = checked_area(width, height, costs.len())?;

    let mut tmp = vec![INF; expected_len];
    let mut dist2 = vec![INF; expected_len];
    let mut f = vec![0.0f32; width.max(height)];
    let mut d = vec![0.0f32; width.max(height)];

    // Pass 1: every column. Index math cannot overflow: `y < height`, `x < width` and
    // `width * height` was checked above.
    for x in 0..width {
        for (y, fi) in f[..height].iter_mut().enumerate() {
            *fi = normalize_edt_cost(costs[y * width + x]);
        }
        edt_1d(f[..height].as_ref(), d[..height].as_mut(), INF);
        for y in 0..height {
            tmp[y * width + x] = d[y];
        }
    }

    // Pass 2: every row over the column result.
    for y in 0..height {
        let row = y * width;
        f[..width].copy_from_slice(&tmp[row..(width + row)]);
        edt_1d(f[..width].as_ref(), d[..width].as_mut(), INF);
        dist2[row..(width + row)].copy_from_slice(&d[..width]);
    }

    Ok(dist2)
}

/// 1-D lower envelope of the parabolas `(q - p)^2 + f[p]`, written into `out` (same length as
/// `f`); a scanline without any seed (`f[p] < inf` for no `p`) is filled with `inf`.
#[expect(clippy::cast_precision_loss, reason = "scanline indices stay below EDT_MAX_DIMENSION (2^24, checked by checked_area), where usize -> f32 is exact")]
#[expect(clippy::many_single_char_names, reason = "the names follow the Felzenszwalb-Huttenlocher paper (f, v, z, k, q, p, s)")]
fn edt_1d(f: &[f32], out: &mut [f32], inf: f32) {
    let n = f.len();
    if n == 0 {
        return;
    }
    // Inputs are normalized upstream to [0, inf) for seeds and exactly `inf` for non-seeds
    // (first-pass outputs are either `inf` exactly or a finite seed-derived value), so a
    // strict `< inf` comparison identifies "this scanline has at least one seed" precisely.
    if !f.iter().any(|&value| value < inf) {
        out.fill(inf);
        return;
    }

    let mut v = vec![0usize; n];
    let mut z = vec![0.0f32; n + 1];
    let mut k = 0usize;

    v[0] = 0;
    z[0] = f32::NEG_INFINITY;
    z[1] = f32::INFINITY;

    for q in 1..n {
        let mut s = edt_intersection(f, q, v[k]);
        while s <= z[k] {
            if k == 0 {
                break;
            }
            k -= 1;
            s = edt_intersection(f, q, v[k]);
        }

        if s <= z[k] && k == 0 {
            v[0] = q;
            z[0] = f32::NEG_INFINITY;
            z[1] = f32::INFINITY;
            k = 0;
            continue;
        }

        k += 1;
        v[k] = q;
        z[k] = s;
        z[k + 1] = f32::INFINITY;
    }

    let mut kk = 0usize;
    for (q, out_q) in out[..n].iter_mut().enumerate() {
        while z[kk + 1] < q as f32 {
            kk += 1;
        }
        let p = v[kk];
        let dq = q as f32 - p as f32;
        *out_q = dq * dq + f[p];
    }
}

/// Abscissa where the parabolas rooted at `q` and `p` (`q != p`) intersect.
#[expect(clippy::cast_precision_loss, reason = "scanline indices stay below EDT_MAX_DIMENSION (2^24, checked by checked_area), where usize -> f32 is exact")]
fn edt_intersection(f: &[f32], q: usize, p: usize) -> f32 {
    let qf = q as f32;
    let pf = p as f32;
    ((f[q] + qf * qf) - (f[p] + pf * pf)) / (2.0 * (qf - pf))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Grid coordinate as `f32`; test grids are tiny, so the conversion is exact.
    fn coord(v: usize) -> f32 {
        f32::from(u16::try_from(v).unwrap_or(u16::MAX))
    }

    /// FNV-1a 64 over the little-endian bit patterns of an `f32` buffer, length included. Same
    /// digest as `ms-text-render`'s EDT characterization test.
    fn f32_bits_digest(values: &[f32]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let len_bytes = u64::try_from(values.len()).unwrap_or(u64::MAX).to_le_bytes();
        for byte in len_bytes.into_iter().chain(values.iter().flat_map(|v| v.to_bits().to_le_bytes())) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// Two of the characterization fields pinned against the pre-move text-renderer copy (the
    /// digests are the ones `ms-text-render`'s `edt_characterization_is_bit_exact` records).
    #[test]
    fn matches_pre_move_characterization_bits() -> Result<(), RasterError> {
        let (bw, bh) = (19usize, 11usize);
        let mut border_mask = vec![0u8; bw * bh];
        for y in 0..bh {
            border_mask[y * bw] = 1;
        }
        border_mask[bw - 1] = 1;
        border_mask[(bh - 1) * bw + bw - 1] = 1;
        for x in 5..9 {
            border_mask[(bh - 1) * bw + x] = 1;
        }
        let border = squared_distance_to_mask(&border_mask, bw, bh)?;
        assert_eq!(f32_bits_digest(&border), 0xffda_6e7b_e3b3_86c0);

        let (cw, ch) = (16usize, 9usize);
        let mut costs = vec![EDT_COST_INF; cw * ch];
        costs[0] = 0.0;
        costs[2 * cw + 7] = 0.25;
        costs[4 * cw + 3] = 0.16;
        costs[5 * cw + 12] = 0.49;
        costs[8 * cw + 15] = 0.09;
        costs[3 * cw + 10] = f32::NAN;
        costs[6 * cw + 1] = -2.0;
        costs[7 * cw + 8] = 0.333;
        let sub_pixel = squared_distance_with_costs(&costs, cw, ch)?;
        assert_eq!(f32_bits_digest(&sub_pixel), 0x397e_db1e_8a9b_6a46);
        Ok(())
    }

    /// Verifies the cost-seeded EDT against a brute-force `O(n^2 m^2)` reference on a small grid
    /// mixing exact seeds (`0.0`), fractional sub-pixel seeds, and non-seeds (`INF`). The
    /// tolerance absorbs the f32 rounding of envelope intersections.
    #[test]
    fn with_costs_matches_bruteforce() -> Result<(), RasterError> {
        let width = 7usize;
        let height = 5usize;
        let inf = EDT_COST_INF;

        let mut costs = vec![inf; width * height];
        costs[width + 1] = 0.0;
        costs[3 * width + 5] = 0.25;
        costs[4 * width + 2] = 0.16;
        costs[6] = 0.09;

        let got = squared_distance_with_costs(&costs, width, height)?;

        for y in 0..height {
            for x in 0..width {
                let mut best = inf;
                for sy in 0..height {
                    for sx in 0..width {
                        let c = costs[sy * width + sx];
                        if c >= inf {
                            continue;
                        }
                        let dx = coord(x) - coord(sx);
                        let dy = coord(y) - coord(sy);
                        let candidate = dx * dx + dy * dy + c;
                        if candidate < best {
                            best = candidate;
                        }
                    }
                }
                let g = got[y * width + x];
                if best >= inf {
                    assert!(g >= inf * 0.5, "pixel ({x},{y}) should be unreachable but got {g}");
                } else {
                    assert!((g - best).abs() <= 1e-2, "pixel ({x},{y}): EDT={g}, brute-force={best}");
                }
            }
        }
        Ok(())
    }

    /// The mask form equals a brute-force nearest-covered-pixel search on a pseudo-random mask.
    #[test]
    fn to_mask_matches_bruteforce() -> Result<(), RasterError> {
        let (width, height) = (13usize, 9usize);
        let mut state = 0x1234_5678u64;
        let mask: Vec<u8> = (0..width * height)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                u8::from((state >> 60) == 0)
            })
            .collect();
        assert!(mask.iter().any(|&m| m > 0), "fixture needs at least one seed");

        let got = squared_distance_to_mask(&mask, width, height)?;
        for y in 0..height {
            for x in 0..width {
                let mut best = f32::INFINITY;
                for sy in 0..height {
                    for sx in 0..width {
                        if mask[sy * width + sx] == 0 {
                            continue;
                        }
                        let dx = coord(x) - coord(sx);
                        let dy = coord(y) - coord(sy);
                        best = best.min(dx * dx + dy * dy);
                    }
                }
                let g = got[y * width + x];
                assert!((g - best).abs() <= 1e-3, "pixel ({x},{y}): EDT={g}, brute-force={best}");
            }
        }
        Ok(())
    }

    /// A single zero-cost seed produces the plain squared Euclidean distance everywhere.
    #[test]
    fn single_zero_seed_is_squared_distance() -> Result<(), RasterError> {
        let width = 6usize;
        let height = 5usize;
        let (sx, sy) = (2usize, 1usize);
        let mut costs = vec![EDT_COST_INF; width * height];
        costs[sy * width + sx] = 0.0;

        let got = squared_distance_with_costs(&costs, width, height)?;

        for y in 0..height {
            for x in 0..width {
                let dx = coord(x) - coord(sx);
                let dy = coord(y) - coord(sy);
                let expected = dx * dx + dy * dy;
                let g = got[y * width + x];
                assert!((g - expected).abs() <= 1e-3, "pixel ({x},{y}): EDT={g}, expected={expected}");
            }
        }
        Ok(())
    }

    /// A field with no seeds keeps every pixel at the unreachable sentinel, in both forms.
    #[test]
    fn all_inf_stays_unreachable() -> Result<(), RasterError> {
        let width = 5usize;
        let height = 4usize;
        let costs = vec![EDT_COST_INF; width * height];
        let got = squared_distance_with_costs(&costs, width, height)?;
        assert!(got.iter().all(|&v| v >= EDT_COST_INF), "expected all-INF, got {got:?}");

        let empty_mask = squared_distance_to_mask(&vec![0u8; width * height], width, height)?;
        assert!(empty_mask.iter().all(|&v| v.to_bits() == EDT_COST_INF.to_bits()));
        Ok(())
    }

    /// A cost just below the sentinel is still a valid seed: the whole grid resolves as reachable
    /// with roughly that cost. Guards the seed-detection threshold (a `< INF * 0.5` style check
    /// would misclassify this field as all-INF).
    #[test]
    fn accepts_cost_just_below_sentinel() -> Result<(), RasterError> {
        let width = 4usize;
        let height = 3usize;
        let near_inf_cost = EDT_COST_INF * 0.999;
        let mut costs = vec![EDT_COST_INF; width * height];
        costs[width + 1] = near_inf_cost;

        let got = squared_distance_with_costs(&costs, width, height)?;

        for (idx, &v) in got.iter().enumerate() {
            assert!(v < EDT_COST_INF, "pixel {idx} treated as unreachable despite a valid near-sentinel seed");
            // The px^2-scale grid offsets vanish in f32 next to the huge seed cost.
            assert!((v - near_inf_cost).abs() <= near_inf_cost * 1e-3, "pixel {idx}: got {v}, expected ~{near_inf_cost}");
        }
        Ok(())
    }

    /// NaN and negative costs are normalized to non-seeds: the output is driven solely by the
    /// remaining valid seed and contains no NaN.
    #[test]
    fn ignores_nan_and_negative_costs() -> Result<(), RasterError> {
        let width = 5usize;
        let height = 4usize;
        let (sx, sy) = (4usize, 3usize);
        let mut costs = vec![EDT_COST_INF; width * height];
        costs[width + 1] = f32::NAN;
        costs[2 * width + 3] = -5.0;
        costs[sy * width + sx] = 0.0;

        let got = squared_distance_with_costs(&costs, width, height)?;

        for y in 0..height {
            for x in 0..width {
                let dx = coord(x) - coord(sx);
                let dy = coord(y) - coord(sy);
                let expected = dx * dx + dy * dy;
                let g = got[y * width + x];
                assert!(!g.is_nan(), "pixel ({x},{y}) is NaN");
                assert!((g - expected).abs() <= 1e-3, "pixel ({x},{y}): EDT={g}, expected={expected} (only the 0.0 seed counts)");
            }
        }
        Ok(())
    }

    /// A zero-area raster is valid and yields an empty output.
    #[test]
    fn zero_area_is_empty() -> Result<(), RasterError> {
        assert!(squared_distance_with_costs(&[], 0, 7)?.is_empty());
        assert!(squared_distance_to_mask(&[], 5, 0)?.is_empty());
        Ok(())
    }

    /// A dimension of exactly `EDT_MAX_DIMENSION` is accepted, one more is refused before the
    /// length check — on empty rasters, so nothing large is allocated.
    #[test]
    fn rejects_dimensions_beyond_the_exact_f32_domain() -> Result<(), RasterError> {
        assert!(squared_distance_with_costs(&[], EDT_MAX_DIMENSION, 0)?.is_empty());
        assert!(squared_distance_to_mask(&[], 0, EDT_MAX_DIMENSION)?.is_empty());
        let over = EDT_MAX_DIMENSION + 1;
        assert_eq!(squared_distance_with_costs(&[], over, 0), Err(RasterError::DimensionTooLarge { width: over, height: 0, max: EDT_MAX_DIMENSION }));
        assert_eq!(squared_distance_to_mask(&[], 0, over), Err(RasterError::DimensionTooLarge { width: 0, height: over, max: EDT_MAX_DIMENSION }));
        assert_eq!(squared_distance_to_mask(&[1], over, 1), Err(RasterError::DimensionTooLarge { width: over, height: 1, max: EDT_MAX_DIMENSION }));
        Ok(())
    }

    /// Mismatched lengths and an overflowing `width * height` are typed errors, never panics.
    #[test]
    fn rejects_invalid_shapes() {
        assert_eq!(
            squared_distance_with_costs(&[0.0f32; 7], 4, 3),
            Err(RasterError::LengthMismatch { width: 4, height: 3, len: 7 })
        );
        assert_eq!(
            squared_distance_to_mask(&[1u8; 5], 2, 2),
            Err(RasterError::LengthMismatch { width: 2, height: 2, len: 5 })
        );
        // Within the dimension cap the product cannot overflow a 64-bit `usize`; a huge width is
        // refused by the cap before the product is formed.
        assert_eq!(
            squared_distance_with_costs(&[], usize::MAX, 2),
            Err(RasterError::DimensionTooLarge { width: usize::MAX, height: 2, max: EDT_MAX_DIMENSION })
        );
        assert_eq!(
            squared_distance_to_mask(&[], usize::MAX, 2),
            Err(RasterError::DimensionTooLarge { width: usize::MAX, height: 2, max: EDT_MAX_DIMENSION })
        );
    }
}
