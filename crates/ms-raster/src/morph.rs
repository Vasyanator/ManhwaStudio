/*
File: crates/ms-raster/src/morph.rs

Purpose:
The one square (Chebyshev) binary dilation of the project.

Key functions:
- `dilate_square()`: dilates a nonzero-is-set `u8` mask by independent x/y radii.

Notes:
A square structuring element is separable: dilating by a (2rx+1) x (2ry+1) box equals a horizontal
dilation by rx followed by a vertical dilation by ry. Each pass keeps a sliding count of set pixels
in its window, so the cost is O(width * height) whatever the radii. Out-of-bounds neighbours are
simply absent (the window is clipped to the buffer), which is exactly what every pre-existing copy
did: the clipped-window forms, the iterated 3x3 forms (r passes of radius 1 compose to radius r)
and the replicate-border form (a replicated border pixel is an in-bounds pixel already counted).
Output formats (0/1, 0/255, in-place keeping original values, `bool`) are caller adapters.
*/

use crate::RasterError;

/// Dilates `src` (`width * height`, row-major, nonzero = set) by a square window of half-width
/// `rx` and half-height `ry`.
///
/// Output pixel `(x, y)` is `on` when any source pixel within `|dx| <= rx`, `|dy| <= ry` (clipped
/// to the buffer) is nonzero, else 0. Radii 0 normalize the mask to `0`/`on` without growing it;
/// radii larger than the buffer are allowed. `on == 0` yields an all-zero buffer. An empty
/// buffer with a zero-area size returns an empty `Vec`. Allocates the output plus one `u8`
/// scratch buffer of the same size and one `usize` per column.
///
/// # Errors
/// `RasterError::LengthMismatch` when `src.len() != width * height` (including when the product
/// overflows `usize`).
pub fn dilate_square(src: &[u8], width: usize, height: usize, rx: usize, ry: usize, on: u8) -> Result<Vec<u8>, RasterError> {
    let mismatch = RasterError::LengthMismatch { width, height, len: src.len() };
    let area = width.checked_mul(height).ok_or_else(|| mismatch.clone())?;
    if area != src.len() {
        return Err(mismatch);
    }
    if area == 0 {
        return Ok(Vec::new());
    }

    // Pass 1: horizontal. `rows[i]` is 1 when any pixel within `rx` columns of `i` is set.
    let mut rows = vec![0u8; area];
    for (src_row, dst_row) in src.chunks_exact(width).zip(rows.chunks_exact_mut(width)) {
        // `count` = set pixels in the clipped window [x - rx, x + rx] of the current `x`.
        let mut count: usize = src_row[..=rx.min(width - 1)].iter().filter(|&&px| px != 0).count();
        for x in 0..width {
            dst_row[x] = u8::from(count > 0);
            // Slide to x + 1: the column x + 1 + rx enters (if inside), the column x - rx leaves
            // (if it was inside). `saturating_add`: x + 1 + rx >= width for any huge radius.
            let entering = x.saturating_add(1).saturating_add(rx);
            if entering < width && src_row[entering] != 0 {
                count += 1;
            }
            if x >= rx && src_row[x - rx] != 0 {
                count -= 1;
            }
        }
    }

    // Pass 2: vertical, row by row with one running count per column, so every access is a
    // contiguous row slice (cache-friendly) instead of a strided column walk.
    let mut counts = vec![0usize; width];
    for row in rows.chunks_exact(width).take(ry.saturating_add(1)) {
        for (count, &px) in counts.iter_mut().zip(row) {
            *count += usize::from(px);
        }
    }
    let mut out = vec![0u8; area];
    for (y, dst_row) in out.chunks_exact_mut(width).enumerate() {
        for (dst, &count) in dst_row.iter_mut().zip(&counts) {
            *dst = if count > 0 { on } else { 0 };
        }
        // Slide to y + 1: row y + 1 + ry enters (if inside), row y - ry leaves (if it was inside).
        let entering = y.saturating_add(1).saturating_add(ry);
        if entering < height {
            let row = &rows[entering * width..(entering + 1) * width];
            for (count, &px) in counts.iter_mut().zip(row) {
                *count += usize::from(px);
            }
        }
        if y >= ry {
            let leaving = y - ry;
            let row = &rows[leaving * width..(leaving + 1) * width];
            for (count, &px) in counts.iter_mut().zip(row) {
                *count -= usize::from(px);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FNV-1a 64 over `bytes`: the fingerprint the WP1.0 characterization tests pinned.
    fn fnv1a64(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    /// Deterministic 64-bit LCG (Knuth MMIX constants); returns the high 31 bits. Same
    /// generator as the WP1.0 characterization tests.
    fn lcg_next(state: &mut u64) -> u32 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        // `>> 33` leaves 31 significant bits, so the conversion cannot fail.
        u32::try_from(*state >> 33).unwrap_or(0)
    }

    /// Random mask with ~`percent` % set pixels using the mixed values 1/7/255 — the exact
    /// generator of the WP1.0 characterization masks (`random_mask` in the translation tests,
    /// `characterization_mask` in the cleaning tests).
    fn random_mask(width: usize, height: usize, seed: u64, percent: u32) -> Vec<u8> {
        let mut state = seed;
        (0..width * height)
            .map(|_| {
                let roll = lcg_next(&mut state);
                if roll % 100 < percent {
                    [1u8, 7, 255][usize::try_from(roll % 3).unwrap_or(0)]
                } else {
                    0
                }
            })
            .collect()
    }

    /// Brute-force reference: the clipped-window scan every pre-refactor copy used.
    fn reference(src: &[u8], width: usize, height: usize, rx: usize, ry: usize, on: u8) -> Vec<u8> {
        let mut out = vec![0u8; src.len()];
        for y in 0..height {
            for x in 0..width {
                let hit = (y.saturating_sub(ry)..=(y + ry).min(height - 1))
                    .any(|yy| (x.saturating_sub(rx)..=(x + rx).min(width - 1)).any(|xx| src[yy * width + xx] != 0));
                out[y * width + x] = if hit { on } else { 0 };
            }
        }
        out
    }

    /// Iterated 3x3 reference (the cleaning `dilate_gray_inplace` / gradient `dilate` shape):
    /// `passes` rounds of radius-1 dilation.
    fn iterated_3x3(src: &[u8], width: usize, height: usize, passes: usize) -> Vec<u8> {
        let mut cur: Vec<u8> = src.iter().map(|&px| u8::from(px != 0)).collect();
        for _ in 0..passes {
            cur = reference(&cur, width, height, 1, 1, 1);
        }
        cur
    }

    #[test]
    fn matches_brute_force_for_random_masks_and_radii() {
        let sizes = [(1usize, 1usize), (1, 9), (9, 1), (2, 3), (7, 5), (16, 11), (37, 23)];
        let mut seed = 0x5eed_0001_u64;
        for &(width, height) in &sizes {
            for percent in [0u32, 3, 15, 60] {
                seed += 1;
                let src = random_mask(width, height, seed, percent);
                for rx in 0..=5 {
                    for ry in 0..=5 {
                        for on in [1u8, 255] {
                            let got = dilate_square(&src, width, height, rx, ry, on).expect("length matches");
                            let want = reference(&src, width, height, rx, ry, on);
                            assert_eq!(got, want, "{width}x{height} pct={percent} rx={rx} ry={ry} on={on}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn equals_iterated_3x3_passes() {
        let src = random_mask(37, 23, 0xd11a_7e01, 8);
        for r in 0..=5 {
            let got = dilate_square(&src, 37, 23, r, r, 1).expect("length matches");
            assert_eq!(got, iterated_3x3(&src, 37, 23, r), "r={r}");
        }
    }

    /// The WP1.0 characterization vectors of the pre-refactor copies, reproduced through this
    /// owner (the copies are private to their crates). Mask: 37x23, seed `0xd11a_7e01`, ~8 %.
    #[test]
    fn reproduces_the_characterization_vectors_of_the_old_copies() {
        let src = random_mask(37, 23, 0xd11a_7e01, 8);
        // Translation `dilate_binary` (0/1 output), and gradient `dilate` (bool as 0/1 bytes)
        // for the (r, r) cases: (rx, ry, set count, FNV).
        let observed = [(0usize, 0usize), (1, 1), (3, 3), (2, 1)]
            .into_iter()
            .map(|(rx, ry)| {
                let out = dilate_square(&src, 37, 23, rx, ry, 1).expect("length matches");
                (rx, ry, out.iter().filter(|&&px| px != 0).count(), fnv1a64(&out))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            observed,
            vec![
                (0, 0, 64, 9_260_574_274_878_741_243),
                (1, 1, 407, 12_066_787_267_297_046_334),
                (3, 3, 834, 13_298_246_393_450_976_053),
                (2, 1, 581, 2_458_741_469_921_375_552),
            ],
            "observed={observed:?}"
        );
        // Cleaning `dilate_binary_mask` (0/255 output) for r = 1, 3. Its r = 0 case returns the
        // input unchanged (mixed values), which is that caller's adapter, not this rule.
        let observed = [1usize, 3]
            .into_iter()
            .map(|r| {
                let out = dilate_square(&src, 37, 23, r, r, 255).expect("length matches");
                (r, out.iter().filter(|&&px| px != 0).count(), fnv1a64(&out))
            })
            .collect::<Vec<_>>();
        assert_eq!(observed, vec![(1, 407, 9_423_328_760_394_973_520), (3, 834, 11_412_883_831_778_999_225)], "observed={observed:?}");

        // Translation `dilate_mask_alpha` (0/255 in, 0/255 out): 41x29, seed 0xd11a_7e02, ~5 %;
        // dilate sizes 1, 3 and 31 (clamped to 30 by that caller).
        let alpha = random_mask(41, 29, 0xd11a_7e02, 5).into_iter().map(|px| if px == 0 { 0 } else { 255 }).collect::<Vec<_>>();
        let observed = [1usize, 3, 30]
            .into_iter()
            .map(|r| {
                let out = dilate_square(&alpha, 41, 29, r, r, 255).expect("length matches");
                (r, out.iter().filter(|&&px| px != 0).count(), fnv1a64(&out))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            observed,
            vec![(1, 500, 7_778_443_962_078_020_797), (3, 1146, 6_986_607_969_027_450_885), (30, 1189, 10_146_146_705_198_328_074)],
            "observed={observed:?}"
        );
    }

    #[test]
    fn rejects_length_mismatch_and_overflow() {
        assert_eq!(
            dilate_square(&[0; 5], 2, 3, 1, 1, 1),
            Err(RasterError::LengthMismatch { width: 2, height: 3, len: 5 })
        );
        assert_eq!(
            dilate_square(&[0; 4], usize::MAX, 2, 1, 1, 1),
            Err(RasterError::LengthMismatch { width: usize::MAX, height: 2, len: 4 })
        );
        assert_eq!(dilate_square(&[1], 0, 3, 1, 1, 1), Err(RasterError::LengthMismatch { width: 0, height: 3, len: 1 }));
    }

    #[test]
    fn empty_and_huge_radius_edge_cases() {
        assert_eq!(dilate_square(&[], 0, 0, 3, 3, 1), Ok(Vec::new()));
        assert_eq!(dilate_square(&[], 5, 0, 3, 3, 1), Ok(Vec::new()));
        // A single set pixel with an unbounded radius fills the whole buffer.
        let mut src = vec![0u8; 12];
        src[7] = 9;
        assert_eq!(dilate_square(&src, 4, 3, usize::MAX, usize::MAX, 200), Ok(vec![200u8; 12]));
        // `on == 0` is allowed and yields zeros.
        assert_eq!(dilate_square(&src, 4, 3, 1, 1, 0), Ok(vec![0u8; 12]));
    }
}
