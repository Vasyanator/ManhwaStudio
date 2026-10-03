/*
File: crates/ms-raster/src/otsu.rs

Purpose:
The one Otsu threshold of the project, over a 256-bin histogram of `u8` samples.

Key functions:
- `otsu_threshold()`: returns the threshold, or `None` when the samples admit no split.

Notes:
The arithmetic reproduces the two pre-existing copies (classic detector and Paddle glyph mask)
bit for bit: f64 class weights and sums accumulated bin by bin, between-class variance
`w_b * w_f * (m_b - m_f)^2`, strict `>` so ties keep the FIRST maximum. Those copies differed only
when no split exists (one returned 127, the other 0); that default now belongs to each caller
(`otsu_threshold(..).unwrap_or(default)`).
*/

/// Computes the Otsu threshold `t` of `values`; classification is `value > t` (foreground).
///
/// Returns the intensity that maximizes the between-class variance, keeping the lowest such
/// intensity on ties. Returns `None` when no threshold splits the samples into two non-empty
/// classes: an empty input, or an input with a single distinct value. Pure, O(n + 256).
#[must_use]
pub fn otsu_threshold(values: &[u8]) -> Option<u8> {
    let mut hist = [0u64; 256];
    for &v in values {
        hist[usize::from(v)] += 1;
    }
    let total = count_to_f64(u64::try_from(values.len()).unwrap_or(u64::MAX));

    // Sum of intensity * count over all bins, for the running foreground mean.
    let mut sum_total = 0.0_f64;
    for (intensity, &count) in (0u8..=255).zip(hist.iter()) {
        sum_total += f64::from(intensity) * count_to_f64(count);
    }

    let mut bg_weight = 0.0_f64;
    let mut bg_sum = 0.0_f64;
    let mut best_variance = -1.0_f64;
    let mut best: Option<u8> = None;

    for (intensity, &count) in (0u8..=255).zip(hist.iter()) {
        bg_weight += count_to_f64(count);
        // Counts are integers, so an exact zero comparison is the "class empty" test.
        if bg_weight == 0.0 {
            continue;
        }
        let fg_weight = total - bg_weight;
        if fg_weight == 0.0 {
            break;
        }
        bg_sum += f64::from(intensity) * count_to_f64(count);
        let bg_mean = bg_sum / bg_weight;
        let fg_mean = (sum_total - bg_sum) / fg_weight;
        let diff = bg_mean - fg_mean;
        let variance = bg_weight * fg_weight * diff * diff;
        // Any evaluated split has variance >= 0 > the -1 seed, so the first evaluated
        // intensity always records; strict `>` keeps the first maximum on ties.
        if variance > best_variance {
            best_variance = variance;
            best = Some(intensity);
        }
    }
    best
}

/// Converts a histogram count to `f64`.
fn count_to_f64(count: u64) -> f64 {
    // Sample counts are buffer lengths, far below f64's 2^53 exact-integer limit, so the
    // conversion is exact in practice.
    #[expect(clippy::cast_precision_loss, reason = "sample counts stay far below 2^53, where u64 -> f64 is exact")]
    let out = count as f64;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic 64-bit LCG (Knuth MMIX constants); returns the high 31 bits. Same
    /// generator as the WP1.0 characterization tests of the pre-refactor copies.
    fn lcg_next(state: &mut u64) -> u32 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        // `>> 33` leaves 31 significant bits, so the conversion cannot fail.
        u32::try_from(*state >> 33).unwrap_or(0)
    }

    /// The characterization vectors pinned against BOTH pre-refactor copies
    /// (`ms-tab-translation` classic `otsu_threshold`, `ms-onnx` glyph-mask `otsu_threshold`).
    /// Where they agreed this owner must agree; where they differed (`empty`,
    /// `single_value`: 127 vs 0) this owner reports `None`.
    #[test]
    fn matches_the_characterization_vectors_of_both_old_copies() {
        let mut state = 0x0750_u64;
        let random: Vec<u8> = (0..1000).map(|_| u8::try_from(lcg_next(&mut state) % 256).unwrap_or(0)).collect();
        let mut bimodal = vec![20u8; 50];
        bimodal.extend(std::iter::repeat_n(200u8, 50));
        let mut three_level = vec![10u8; 30];
        three_level.extend(std::iter::repeat_n(128u8, 40));
        three_level.extend(std::iter::repeat_n(240u8, 30));
        let vectors: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("single_value", vec![77u8; 10]),
            ("two_adjacent", vec![100, 101, 100, 101]),
            ("bimodal", bimodal),
            ("three_level", three_level),
            ("extremes", vec![0, 255]),
            ("random_1000", random),
        ];
        let observed: Vec<(&str, Option<u8>)> = vectors.iter().map(|(name, values)| (*name, otsu_threshold(values))).collect();
        assert_eq!(
            observed,
            vec![
                ("empty", None),
                ("single_value", None),
                ("two_adjacent", Some(100)),
                ("bimodal", Some(20)),
                ("three_level", Some(10)),
                ("extremes", Some(0)),
                ("random_1000", Some(129)),
            ],
            "observed={observed:?}"
        );
        // The callers' defaults reproduce the old copies exactly.
        assert_eq!(otsu_threshold(&[]).unwrap_or(127), 127);
        assert_eq!(otsu_threshold(&[77; 10]).unwrap_or(0), 0);
    }

    #[test]
    fn two_distinct_values_always_split_at_the_lower_one() {
        for (lo, hi) in [(0u8, 1u8), (3, 250), (254, 255)] {
            assert_eq!(otsu_threshold(&[lo, hi, hi, lo, hi]), Some(lo), "lo={lo} hi={hi}");
        }
    }
}
