/*
File: crates/ms-onnx/src/manga_ocr/beam.rs

Purpose:
Faithful port of the MangaOCR Python beam search (`_generate_token_ids`) used to
turn decoder logits into a token-id sequence. Pure logic: the caller supplies a
per-step "next-token logits" closure, so this module never touches ONNX Runtime
and reduces cleanly to greedy decoding at `num_beams == 1`.

Key structures:
- BeamConfig    : generation hyper-parameters (matches `generation_config.json`).
- BeamCandidate : a partial hypothesis (token ids + summed log-prob + finished flag).

Key functions:
- beam_search              : the generation loop, parameterized by a logits closure.
- log_softmax              : numerically-stable log-softmax over a logits slice.
- no_repeat_ngram_banned_tokens : tokens that would complete a repeated n-gram.
- top_k_indices            : indices of the `k` largest values, descending.
- normalized_score         : length-penalized hypothesis score.

Notes:
Algorithm mirrors the reference exactly: at each step expand every beam by its top
`num_beams * 2` non-banned tokens, route EOS-completing candidates to a completed
pool, keep the top `num_beams` unfinished candidates by RAW summed log-prob, stop
early once `>= num_beams` hypotheses have completed, and finally pick the best by
length-penalized score. Floating-point summation differs marginally from NumPy's
pairwise reduction; this only matters on exact score ties, which are effectively
impossible on real model outputs (documented parity caveat).
*/

use std::cmp::Ordering;
use std::collections::HashSet;

use crate::OrtError;

/// Generation hyper-parameters for MangaOCR beam search.
///
/// Field meanings match Hugging Face `GenerationConfig`; the MangaOCR `base` and
/// `2025` exports share the same values (start=2, eos=3, max_length=300,
/// num_beams=4, no_repeat_ngram_size=3, length_penalty=2.0, early_stopping=true).
#[derive(Debug, Clone, Copy)]
pub struct BeamConfig {
    /// Seed token id for the decoder sequence (`[CLS]` = 2).
    pub decoder_start_token_id: i64,
    /// End-of-sequence token id (`[SEP]` = 3); completing a hypothesis.
    pub eos_token_id: i64,
    /// Hard cap on generated sequence length (including the start token).
    pub max_length: usize,
    /// Beam width; `1` reduces the search to greedy decoding.
    pub num_beams: usize,
    /// Ban tokens that would complete an already-seen n-gram of this size (`0` = off).
    pub no_repeat_ngram_size: usize,
    /// Length-penalty exponent for final hypothesis scoring.
    pub length_penalty: f64,
    /// Stop as soon as `>= num_beams` hypotheses have completed.
    pub early_stopping: bool,
}

/// A partial (or completed) decoding hypothesis.
#[derive(Debug, Clone)]
pub struct BeamCandidate {
    /// Token ids so far, starting with the decoder start token.
    pub token_ids: Vec<i64>,
    /// Sum of per-step log-probabilities for `token_ids` (excluding the start).
    pub sum_logprob: f64,
    /// Whether the last appended token was EOS.
    pub finished: bool,
}

impl BeamCandidate {
    /// Length-penalized score used to select the final hypothesis.
    #[must_use]
    pub fn normalized_score(&self, length_penalty: f64) -> f64 {
        normalized_score(self.token_ids.len(), self.sum_logprob, length_penalty)
    }
}

/// Length-penalized hypothesis score: `sum_logprob / max(len-1, 1)^length_penalty`.
///
/// `len` is the number of tokens in the hypothesis (including the start token), so
/// `len - 1` is the number of generated tokens. Mirrors the Python
/// `_BeamCandidate.normalized_score`.
#[must_use]
pub fn normalized_score(len: usize, sum_logprob: f64, length_penalty: f64) -> f64 {
    let effective_len = len.saturating_sub(1).max(1);
    // effective_len is a token count (<= max_length), so it always fits in u32.
    let effective = f64::from(u32::try_from(effective_len).unwrap_or(u32::MAX));
    sum_logprob / effective.powf(length_penalty)
}

/// Numerically-stable log-softmax over `logits`.
///
/// Returns per-element `log(softmax(logits))`. If the exponent sum is non-positive
/// or non-finite (degenerate input), returns all `-inf`, matching the reference.
#[must_use]
pub fn log_softmax(logits: &[f32]) -> Vec<f32> {
    let max_logit = logits
        .iter()
        .copied()
        .fold(f32::NEG_INFINITY, f32::max);
    if !max_logit.is_finite() {
        return vec![f32::NEG_INFINITY; logits.len()];
    }
    // Accumulate the exponent sum in f64 for stability; the reference sums in f32
    // (NumPy pairwise), a marginal difference that cannot flip argmax/top-k on real
    // outputs.
    let exp_sum: f64 = logits
        .iter()
        .map(|&x| f64::from(x - max_logit).exp())
        .sum();
    if exp_sum <= 0.0 || !exp_sum.is_finite() {
        return vec![f32::NEG_INFINITY; logits.len()];
    }
    let log_sum = exp_sum.ln();
    logits
        .iter()
        .map(|&x| {
            // Intended f64 -> f32 narrowing: the function returns f32 log-probs, and
            // computing the subtraction in f64 then rounding to f32 matches NumPy's
            // float32 output (float32 array minus float64 scalar). Not a data-loss bug.
            #[allow(clippy::cast_possible_truncation)]
            let logprob = (f64::from(x - max_logit) - log_sum) as f32;
            logprob
        })
        .collect()
}

/// Tokens that would complete an n-gram already present in `token_ids`.
///
/// With `ngram_size == n`, bans any token `t` such that the last `n-1` tokens
/// followed by `t` reproduce an n-gram seen earlier in the sequence. `ngram_size`
/// of `0` disables banning; `1` bans every token already present. Mirrors the
/// Python `_no_repeat_ngram_banned_tokens`.
#[must_use]
pub fn no_repeat_ngram_banned_tokens(token_ids: &[i64], ngram_size: usize) -> HashSet<i64> {
    let mut banned = HashSet::new();
    if ngram_size == 0 {
        return banned;
    }
    // Reference guard `len(token_ids) < ngram_size - 1`, written to avoid underflow.
    if token_ids.len() + 1 < ngram_size {
        return banned;
    }
    if token_ids.len() < ngram_size {
        // No complete n-gram to scan yet (prefix present but nothing to ban).
        return banned;
    }
    // The suffix that a completing token must extend: the last `ngram_size - 1` ids.
    let prefix = &token_ids[token_ids.len() - (ngram_size - 1)..];
    // Scan every full n-gram; if its head equals the current suffix, ban its tail.
    for start in 0..=(token_ids.len() - ngram_size) {
        let ngram = &token_ids[start..start + ngram_size];
        let (head, tail) = ngram.split_at(ngram_size - 1);
        if head == prefix {
            banned.insert(tail[0]);
        }
    }
    banned
}

/// Indices of the `limit` largest values in `values`, in descending value order.
///
/// Ties are broken by ascending index for determinism. Returns fewer than `limit`
/// indices if `values` is shorter; an empty vector when `limit == 0`.
#[must_use]
pub fn top_k_indices(values: &[f32], limit: usize) -> Vec<usize> {
    if limit == 0 {
        return Vec::new();
    }
    let mut indices: Vec<usize> = (0..values.len()).collect();
    indices.sort_by(|&a, &b| {
        // Descending by value; NaN sorts to the bottom; ties break by index asc.
        match values[b].partial_cmp(&values[a]) {
            Some(Ordering::Equal) | None => a.cmp(&b),
            Some(order) => order,
        }
    });
    indices.truncate(limit);
    indices
}

/// Runs MangaOCR beam search, returning the best token-id sequence.
///
/// `next_token_logits(token_ids)` must return the next-token logits vector (length
/// = vocabulary size) for the given prefix, i.e. `logits[0, last_position, :]` from
/// the decoder. The returned sequence starts with `decoder_start_token_id`.
///
/// # Errors
/// Propagates any [`OrtError`] returned by `next_token_logits`, and returns
/// [`OrtError::TensorShape`] if a token index cannot be represented.
pub fn beam_search<F>(config: &BeamConfig, mut next_token_logits: F) -> Result<Vec<i64>, OrtError>
where
    F: FnMut(&[i64]) -> Result<Vec<f32>, OrtError>,
{
    let start = config.decoder_start_token_id;
    let mut beams = vec![BeamCandidate {
        token_ids: vec![start],
        sum_logprob: 0.0,
        finished: false,
    }];
    let mut completed: Vec<BeamCandidate> = Vec::new();

    // Reference loops `range(max_length - 1)`: one token appended per iteration.
    let iterations = config.max_length.saturating_sub(1);
    for _ in 0..iterations {
        let mut candidates: Vec<BeamCandidate> = Vec::new();

        for beam in &beams {
            let logits = next_token_logits(&beam.token_ids)?;
            let mut logprobs = log_softmax(&logits);
            let banned = no_repeat_ngram_banned_tokens(&beam.token_ids, config.no_repeat_ngram_size);
            for &token in &banned {
                if let Ok(idx) = usize::try_from(token)
                    && idx < logprobs.len()
                {
                    logprobs[idx] = f32::NEG_INFINITY;
                }
            }

            for idx in top_k_indices(&logprobs, config.num_beams * 2) {
                let token_logprob = logprobs[idx];
                if !token_logprob.is_finite() {
                    continue;
                }
                let token_id = i64::try_from(idx).map_err(|_| OrtError::TensorShape {
                    detail: format!("индекс токена {idx} не помещается в i64"),
                })?;
                let mut next_ids = beam.token_ids.clone();
                next_ids.push(token_id);
                let candidate = BeamCandidate {
                    token_ids: next_ids,
                    sum_logprob: beam.sum_logprob + f64::from(token_logprob),
                    finished: token_id == config.eos_token_id,
                };
                if candidate.finished {
                    completed.push(candidate);
                } else {
                    candidates.push(candidate);
                }
            }
        }

        if candidates.is_empty() {
            break;
        }

        // Keep the top `num_beams` unfinished hypotheses by RAW summed log-prob.
        // `sort_by` is stable, so exact ties preserve insertion order (as Python's
        // stable `sorted`).
        candidates.sort_by(|a, b| {
            b.sum_logprob
                .partial_cmp(&a.sum_logprob)
                .unwrap_or(Ordering::Equal)
        });
        candidates.truncate(config.num_beams);
        beams = candidates;

        if config.early_stopping && completed.len() >= config.num_beams {
            break;
        }
    }

    let pool: &[BeamCandidate] = if completed.is_empty() { &beams } else { &completed };
    // `max` by length-penalized score; on ties keep the earliest (matches Python
    // `max`, which returns the first maximal element). `reduce` avoids `unwrap`.
    let best = pool.iter().cloned().reduce(|a, b| {
        if b.normalized_score(config.length_penalty) > a.normalized_score(config.length_penalty) {
            b
        } else {
            a
        }
    });
    match best {
        Some(candidate) => Ok(candidate.token_ids),
        None => Ok(vec![start]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> BeamConfig {
        BeamConfig {
            decoder_start_token_id: 2,
            eos_token_id: 3,
            max_length: 300,
            num_beams: 4,
            no_repeat_ngram_size: 3,
            length_penalty: 2.0,
            early_stopping: true,
        }
    }

    #[test]
    fn log_softmax_uniform_logits() {
        let lp = log_softmax(&[0.0, 0.0, 0.0, 0.0]);
        let expected = -(4.0_f32).ln();
        for value in lp {
            assert!((value - expected).abs() < 1e-6);
        }
    }

    #[test]
    fn log_softmax_sums_to_one_in_prob_space() {
        let lp = log_softmax(&[1.0, 2.0, 3.0]);
        let prob_sum: f64 = lp.iter().map(|&x| f64::from(x).exp()).sum();
        assert!((prob_sum - 1.0).abs() < 1e-6);
    }

    #[test]
    fn top_k_picks_largest_descending() {
        let values = [0.1, 0.9, 0.5, 0.9, -1.0];
        // Two 0.9 values at idx 1 and 3: descending value, ties by index asc.
        assert_eq!(top_k_indices(&values, 3), vec![1, 3, 2]);
        assert_eq!(top_k_indices(&values, 0), Vec::<usize>::new());
    }

    #[test]
    fn no_repeat_ngram_bans_completion_of_repeated_trigram() {
        // History: [10, 20, 30, 10, 20]. With ngram_size=3 the suffix is (10, 20);
        // the earlier trigram (10, 20, 30) means completing (10, 20) with 30 is banned.
        let banned = no_repeat_ngram_banned_tokens(&[10, 20, 30, 10, 20], 3);
        assert!(banned.contains(&30));
        assert_eq!(banned.len(), 1);
    }

    #[test]
    fn no_repeat_ngram_empty_when_no_repeat() {
        let banned = no_repeat_ngram_banned_tokens(&[10, 20, 30, 40], 3);
        assert!(banned.is_empty());
    }

    #[test]
    fn no_repeat_ngram_disabled_returns_empty() {
        assert!(no_repeat_ngram_banned_tokens(&[10, 20, 10], 0).is_empty());
    }

    #[test]
    fn normalized_score_formula() {
        // len=4 -> effective=3; 3^2 = 9; -0.9 / 9 = -0.1.
        let score = normalized_score(4, -0.9, 2.0);
        assert!((score - (-0.1)).abs() < 1e-12);
        // len<=1 -> effective clamped to 1.
        assert!((normalized_score(1, -0.5, 2.0) - (-0.5)).abs() < 1e-12);
    }

    #[test]
    fn greedy_num_beams_one_picks_argmax() {
        // Greedy: one decode step (max_length=2), no n-gram ban, argmax at index 42.
        let mut config = base_config();
        config.num_beams = 1;
        config.no_repeat_ngram_size = 0;
        config.max_length = 2;
        let vocab = 100;
        let result = beam_search(&config, |_ids| {
            let mut logits = vec![0.0_f32; vocab];
            logits[42] = 5.0; // clear maximum, not EOS (3)
            Ok(logits)
        })
        .expect("greedy beam search must not fail on a synthetic decoder");
        assert_eq!(result, vec![2, 42]);
    }

    #[test]
    fn generation_stops_on_eos_and_returns_completed() {
        // Step 1 argmax = token 7; step 2 argmax = EOS (3). Expect [start, 7, eos].
        let config = base_config();
        let vocab = 50;
        let result = beam_search(&config, move |ids| {
            let mut logits = vec![0.0_f32; vocab];
            if ids.len() == 1 {
                logits[7] = 10.0;
            } else {
                logits[3] = 10.0; // eos_token_id = 3
            }
            Ok(logits)
        })
        .expect("beam search must not fail on a synthetic decoder");
        assert_eq!(result.first(), Some(&2));
        assert_eq!(result.last(), Some(&3));
        assert!(result.contains(&7));
    }

    #[test]
    fn beam_search_propagates_decoder_error() {
        let config = base_config();
        let result = beam_search(&config, |_ids| {
            Err(OrtError::Inference {
                stage: "decoder",
                reason: "synthetic".to_owned(),
            })
        });
        assert!(matches!(result, Err(OrtError::Inference { .. })));
    }
}
