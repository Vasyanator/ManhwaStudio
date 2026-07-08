/*
File: crates/ms-onnx/src/paddle_ocr/ctc.rs

Purpose:
CTC greedy decoding of the PaddleOCR recognizer output. Faithful port of
`CTCLabelDecoder.decode_batch` / `_decode_logits` in
`modules/ai_backend/paddle_onnx_runtime.py`.

Key functions:
- needs_softmax   : detect whether raw logits must be softmax-normalized first.
- softmax_rows    : in-place per-timestep softmax over the class axis.
- decode_greedy   : collapse one sample's [T, C] probabilities into (text, conf).

Notes:
Decode contract (matches Python exactly): per timestep take the argmax class and
its probability; skip the token when the class index is 0 (blank at the FRONT of
the table) OR equal to the previous kept-or-skipped index (repeat collapse); map
each surviving index to its character via the `CharacterTable`. Confidence is the
mean of the kept tokens' probabilities (0.0 when nothing survives). PaddleOCR rec
exports already emit softmax probabilities, so `needs_softmax` is normally false;
it mirrors Python's guard that softmaxes only when values fall outside `[0, 1]`.
*/

use super::dict::CharacterTable;

/// Whether the raw recognizer output must be softmax-normalized before decoding.
///
/// Returns `true` when any value lies outside `[0, 1]` (i.e. the export emitted raw
/// logits rather than probabilities), matching Python's
/// `max(logits) > 1.0 or min(logits) < 0.0` check.
#[must_use]
pub fn needs_softmax(data: &[f32]) -> bool {
    let mut any_above = false;
    let mut any_below = false;
    for &v in data {
        if v > 1.0 {
            any_above = true;
        }
        if v < 0.0 {
            any_below = true;
        }
    }
    any_above || any_below
}

/// Applies a numerically stable softmax over the class axis, in place.
///
/// `data` is a flat `rows * num_classes` buffer laid out row-major (one row per
/// timestep); each `num_classes`-length row is softmaxed independently. A trailing
/// partial row (when `data.len()` is not a multiple of `num_classes`) is left
/// untouched. `num_classes` of 0 is a no-op.
pub fn softmax_rows(data: &mut [f32], num_classes: usize) {
    if num_classes == 0 {
        return;
    }
    for row in data.chunks_mut(num_classes) {
        if row.len() != num_classes {
            break;
        }
        let max = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        if !max.is_finite() {
            continue;
        }
        let mut sum = 0.0_f32;
        for value in row.iter_mut() {
            let e = (*value - max).exp();
            *value = e;
            sum += e;
        }
        if sum > 0.0 {
            for value in row.iter_mut() {
                *value /= sum;
            }
        }
    }
}

/// Greedily decodes one sample's `[T, C]` probability rows into `(text, confidence)`.
///
/// `sample` is a flat `time_steps * num_classes` row-major buffer of per-class
/// probabilities. Applies the CTC collapse rule (skip blank index 0 and repeated
/// indices), maps surviving indices through `table`, and returns the joined text
/// and the mean probability of the kept tokens (0.0 when none survive).
///
/// A trailing partial row is ignored. Indices at or beyond `table.len()` are
/// dropped (mirrors Python's `idx_i < len(self.character)` guard).
#[must_use]
pub fn decode_greedy(
    sample: &[f32],
    time_steps: usize,
    num_classes: usize,
    table: &CharacterTable,
) -> (String, f32) {
    if num_classes == 0 || time_steps == 0 {
        return (String::new(), 0.0);
    }

    let mut text = String::new();
    let mut conf_sum = 0.0_f32;
    let mut conf_count = 0_usize;
    // Sentinel that never equals a valid class index, so the first token is kept.
    let mut prev_idx: i64 = -1;

    for step in 0..time_steps {
        let start = step * num_classes;
        let Some(row) = sample.get(start..start + num_classes) else {
            break;
        };

        // argmax over the class axis; `prob` is the winning class probability.
        let mut best_idx = 0_usize;
        let mut best_prob = row[0];
        for (idx, &value) in row.iter().enumerate() {
            if value > best_prob {
                best_prob = value;
                best_idx = idx;
            }
        }

        let best_idx_i64 = i64::try_from(best_idx).unwrap_or(i64::MAX);
        // Collapse: drop blank (index 0) and repeats of the previous index. Python
        // updates `prev_idx` even for skipped tokens, so a run of the same class
        // yields at most one emission.
        if best_idx == 0 || best_idx_i64 == prev_idx {
            prev_idx = best_idx_i64;
            continue;
        }

        if let Some(ch) = table.get(best_idx) {
            text.push_str(ch);
            conf_sum += best_prob;
            conf_count += 1;
        }
        prev_idx = best_idx_i64;
    }

    let confidence = if conf_count == 0 {
        0.0
    } else {
        conf_sum / conf_count_as_f32(conf_count)
    };
    (text, confidence)
}

/// Converts a small positive count to `f32` for averaging, without a lossy `as`.
///
/// Kept-token counts are bounded by the timestep count (a few thousand at most), so
/// `u32` always holds them and `f32::from` is exact and lossless.
fn conf_count_as_f32(count: usize) -> f32 {
    let count_u32 = u32::try_from(count).unwrap_or(u32::MAX);
    // f32 cannot represent every u32 exactly, but recognizer sequence lengths are
    // far below 2^24 (the exact-integer limit), so this stays exact in practice.
    #[allow(clippy::cast_precision_loss)]
    let out = count_u32 as f32;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_table() -> CharacterTable {
        // ["blank", "a", "b", "c"] (from_lines appends a trailing space -> index 4).
        CharacterTable::from_lines(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()])
    }

    #[test]
    fn needs_softmax_flags_out_of_range_only() {
        assert!(!needs_softmax(&[0.0, 0.5, 1.0]));
        assert!(needs_softmax(&[0.0, 2.0]));
        assert!(needs_softmax(&[-0.1, 0.5]));
    }

    #[test]
    fn softmax_rows_normalizes_each_row() {
        let mut data = vec![1.0, 1.0, 1.0, 1.0];
        softmax_rows(&mut data, 2);
        // Two rows of [1,1] -> [0.5, 0.5] each.
        for &v in &data {
            assert!((v - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn decode_collapses_blanks_and_repeats() {
        let table = tiny_table();
        let num_classes = table.len(); // 5
        // Timesteps (argmax in brackets): a a blank a b  -> "aab"
        //   t0: class 1 (a), t1: class 1 (a, repeat -> dropped),
        //   t2: class 0 (blank -> dropped), t3: class 1 (a, kept again),
        //   t4: class 2 (b).
        let mut sample = vec![0.0_f32; 5 * num_classes];
        let set = |s: &mut [f32], t: usize, cls: usize, p: f32| {
            s[t * num_classes + cls] = p;
        };
        set(&mut sample, 0, 1, 0.9);
        set(&mut sample, 1, 1, 0.8);
        set(&mut sample, 2, 0, 0.7);
        set(&mut sample, 3, 1, 0.6);
        set(&mut sample, 4, 2, 0.5);

        let (text, conf) = decode_greedy(&sample, 5, num_classes, &table);
        assert_eq!(text, "aab");
        // Kept tokens: t0=0.9, t3=0.6, t4=0.5 -> mean 2.0/3.
        assert!((conf - (2.0 / 3.0)).abs() < 1e-6);
    }

    #[test]
    fn decode_all_blank_is_empty_zero_conf() {
        let table = tiny_table();
        let num_classes = table.len();
        let mut sample = vec![0.0_f32; 3 * num_classes];
        for t in 0..3 {
            sample[t * num_classes] = 1.0; // class 0 (blank) wins every step
        }
        let (text, conf) = decode_greedy(&sample, 3, num_classes, &table);
        assert!(text.is_empty());
        assert!((conf - 0.0).abs() < 1e-9);
    }
}
