/*
File: crates/ms-onnx/src/baberu_ocr/decode.rs

Purpose:
The pure Baberu OCR greedy decoding loop, parameterized by a "next logits" closure so it
is unit-testable on scripted fake logits without any ONNX session.

Key structures:
- GreedyConfig : decode settings (BABERU = the published upstream defaults).

Key functions:
- greedy_decode : prefill logits -> generated token ids (without BOS/EOS).

Notes:
Exact port of the loop in upstream `onnx_infer.py` (`BaberuOnnxOCR.__call__`):
1. logits are widened to f64; every id seen so far INCLUDING BOS gets the HF repetition
   penalty (`s / p` when `s >= 0`, else `s * p`), once per step;
2. the run cap: when the trailing run of the last token reaches 12 (content token) or
   16 (any other token > 3), that token's logit becomes -inf;
3. argmax picks the FIRST maximum; NaN logits are an error (NumPy would pick the NaN);
4. EOS stops; otherwise the token is kept, and generation stops at `max_new_tokens`;
5. the next step runs with `position = first_position + k` (`vision_len + 1 + k`).
*/

use std::collections::BTreeSet;

use crate::OrtError;

/// Greedy decode settings.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GreedyConfig {
    /// Maximum number of generated tokens (EOS excluded).
    pub max_new_tokens: usize,
    /// HF-style repetition penalty applied to every seen id (1.0 disables it).
    pub repetition_penalty: f64,
    /// Run cap for a repeated content (letter/number) token.
    pub max_content_run: usize,
    /// Run cap for a repeated non-content token with id > 3.
    pub max_symbol_run: usize,
    /// Beginning-of-sequence id fed to the prefill; counts as "seen".
    pub bos: u32,
    /// End-of-sequence id; stops generation and is not returned.
    pub eos: u32,
}

impl GreedyConfig {
    /// The published Baberu decode settings (`onnx_infer.py` defaults).
    pub(crate) const BABERU: Self = Self {
        max_new_tokens: 256,
        repetition_penalty: 1.2,
        max_content_run: 12,
        max_symbol_run: 16,
        bos: 1,
        eos: 2,
    };
}

/// Ids at or below this value never get a symbol run cap (`last > 3` in the reference):
/// they are the special tokens.
const LAST_SPECIAL_ID: u32 = 3;

/// Runs the greedy loop and returns the generated ids (no BOS, no EOS).
///
/// `first_logits` is the last prefill row; `first_position` the `position_ids` value of
/// the first step call (`vision_len + 1`). `step(token, position)` runs the decoder for
/// one token and returns its logits row, which must have the same width as
/// `first_logits`. `is_content(id)` is the vocabulary's content classification.
///
/// # Errors
/// [`OrtError::BaberuDecode`] for an empty logits row, a width change between steps, a
/// NaN logit, a BOS outside the row, or a position overflow; any error of `step` is
/// returned unchanged.
pub(crate) fn greedy_decode(
    cfg: &GreedyConfig,
    is_content: &dyn Fn(u32) -> bool,
    first_logits: &[f32],
    first_position: i64,
    step: &mut dyn FnMut(u32, i64) -> Result<Vec<f32>, OrtError>,
) -> Result<Vec<u32>, OrtError> {
    let width = first_logits.len();
    if width == 0 {
        return Err(decode_error("пустая строка логитов префилла".to_owned()));
    }
    let mut logits: Vec<f64> = first_logits.iter().copied().map(f64::from).collect();
    let mut seen: BTreeSet<u32> = BTreeSet::from([cfg.bos]);
    let mut tokens: Vec<u32> = Vec::new();
    let mut position = first_position;

    for _ in 0..cfg.max_new_tokens {
        // The reference skips this when the penalty is 1.0; applying it anyway is
        // identical (`x * 1.0` and `x / 1.0` are exact in IEEE 754, -0.0 and NaN included).
        for &id in &seen {
            let slot = logit_slot(&mut logits, id)?;
            *slot = if *slot < 0.0 {
                *slot * cfg.repetition_penalty
            } else {
                *slot / cfg.repetition_penalty
            };
        }

        // The reference uses `last = 0` before the first token, which caps nothing.
        let last = tokens.last().copied().unwrap_or(0);
        let cap = if is_content(last) {
            Some(cfg.max_content_run)
        } else if last > LAST_SPECIAL_ID {
            Some(cfg.max_symbol_run)
        } else {
            None
        };
        if let Some(cap) = cap {
            let run = tokens.iter().rev().take_while(|&&t| t == last).count();
            if run >= cap {
                *logit_slot(&mut logits, last)? = f64::NEG_INFINITY;
            }
        }

        let next = argmax_first(&logits)?;
        if next == cfg.eos {
            break;
        }
        tokens.push(next);
        seen.insert(next);
        if tokens.len() >= cfg.max_new_tokens {
            break;
        }

        let row = step(next, position)?;
        if row.len() != width {
            return Err(decode_error(format!(
                "ширина логитов шага {} не совпадает с префиллом {width}",
                row.len()
            )));
        }
        logits.clear();
        logits.extend(row.iter().copied().map(f64::from));
        position = position
            .checked_add(1)
            .ok_or_else(|| decode_error("переполнение position_ids".to_owned()))?;
    }
    Ok(tokens)
}

/// The mutable logit of `id`, or a typed error when the id is outside the row.
fn logit_slot(logits: &mut [f64], id: u32) -> Result<&mut f64, OrtError> {
    let len = logits.len();
    usize::try_from(id)
        .ok()
        .and_then(|i| logits.get_mut(i))
        .ok_or_else(|| decode_error(format!("id {id} вне строки логитов длины {len}")))
}

/// Index of the FIRST maximum (NumPy `argmax` tie rule); NaN is an error.
fn argmax_first(logits: &[f64]) -> Result<u32, OrtError> {
    let mut best: Option<(usize, f64)> = None;
    for (i, &value) in logits.iter().enumerate() {
        if value.is_nan() {
            return Err(decode_error(format!("NaN в логитах (id {i})")));
        }
        // Strictly greater keeps the first index on ties.
        if best.is_none_or(|(_, b)| value > b) {
            best = Some((i, value));
        }
    }
    let (index, _) = best.ok_or_else(|| decode_error("пустая строка логитов".to_owned()))?;
    u32::try_from(index).map_err(|_| decode_error(format!("индекс токена {index} не помещается в u32")))
}

fn decode_error(detail: String) -> OrtError {
    OrtError::BaberuDecode { detail }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const WIDTH: usize = 12;

    /// A logits row with `value` at `id` and 0 elsewhere.
    fn row_with(pairs: &[(usize, f32)]) -> Vec<f32> {
        let mut row = vec![0.0; WIDTH];
        for &(id, v) in pairs {
            row[id] = v;
        }
        row
    }

    /// Ids 4..=7 are content (letters), 8..=11 symbols.
    fn content(id: u32) -> bool {
        (4..=7).contains(&id)
    }

    /// The decode result plus every `(token, position)` the step closure received.
    type Run = (Result<Vec<u32>, OrtError>, Vec<(u32, i64)>);

    /// Runs the decoder with `first` and then the scripted rows (then EOS forever),
    /// recording step calls.
    fn run(cfg: &GreedyConfig, first: &[f32], rows: Vec<Vec<f32>>) -> Run {
        let calls = RefCell::new(Vec::new());
        let mut rows = rows.into_iter();
        let mut step = |tok: u32, pos: i64| {
            calls.borrow_mut().push((tok, pos));
            Ok(rows.next().unwrap_or_else(|| row_with(&[(2, 100.0)])))
        };
        let out = greedy_decode(cfg, &content, first, 257, &mut step);
        (out, calls.into_inner())
    }

    #[test]
    fn stops_on_eos_and_feeds_positions_from_vision_len_plus_one() {
        let (out, calls) = run(
            &GreedyConfig::BABERU,
            &row_with(&[(5, 10.0)]),
            vec![row_with(&[(6, 10.0)]), row_with(&[(2, 10.0)])],
        );
        assert_eq!(out.ok(), Some(vec![5, 6]));
        assert_eq!(calls, vec![(5, 257), (6, 258)]);
    }

    #[test]
    fn penalty_covers_bos_and_seen_ids_with_sign_handling() {
        // BOS (1) logit 1.1 vs id 4 logit 1.0: after 1.1/1.2 BOS loses -> 4 wins.
        let (out, _) = run(&GreedyConfig::BABERU, &row_with(&[(1, 1.1), (4, 1.0)]), vec![]);
        assert_eq!(out.ok(), Some(vec![4]));

        // Seen id 4 at 1.1 vs fresh id 5 at 1.0: 4 is penalized to 0.9166 -> 5 wins.
        let (out, _) = run(
            &GreedyConfig::BABERU,
            &row_with(&[(4, 5.0)]),
            vec![row_with(&[(4, 1.1), (5, 1.0)])],
        );
        assert_eq!(out.ok(), Some(vec![4, 5]));

        // Negative logits are MULTIPLIED: seen id 4 at -1.0 becomes -1.2, below fresh
        // id 5 at -1.1 (every other id is -5 so they cannot win).
        let mut neg = vec![-5.0_f32; WIDTH];
        neg[4] = -1.0;
        neg[5] = -1.1;
        let (out, _) = run(&GreedyConfig::BABERU, &row_with(&[(4, 5.0)]), vec![neg]);
        assert_eq!(out.ok(), Some(vec![4, 5]));
    }

    #[test]
    fn content_run_is_capped_at_twelve_and_symbol_run_at_sixteen() {
        let cfg = GreedyConfig { repetition_penalty: 1.0, ..GreedyConfig::BABERU };
        // Content id 4 always wins (second best 9): exactly 12 in a row, then 9.
        let rows = vec![row_with(&[(4, 10.0), (9, 5.0)]); 40];
        let (out, _) = run(&cfg, &row_with(&[(4, 10.0), (9, 5.0)]), rows);
        let Ok(out) = out else {
            panic!("decode failed");
        };
        assert_eq!(out.iter().take_while(|&&t| t == 4).count(), 12);
        assert_eq!(out[12], 9);

        // Symbol id 9 always wins (second best 4): exactly 16 in a row, then 4.
        let rows = vec![row_with(&[(9, 10.0), (4, 5.0)]); 40];
        let (out, _) = run(&cfg, &row_with(&[(9, 10.0), (4, 5.0)]), rows);
        let Ok(out) = out else {
            panic!("decode failed");
        };
        assert_eq!(out.iter().take_while(|&&t| t == 9).count(), 16);
        assert_eq!(out[16], 4);
    }

    #[test]
    fn special_ids_get_no_run_cap() {
        // Token 3 (<unk>) is neither content nor > 3, so 20 of them stay uncapped.
        let cfg = GreedyConfig { repetition_penalty: 1.0, max_new_tokens: 20, ..GreedyConfig::BABERU };
        let rows = vec![row_with(&[(3, 10.0)]); 30];
        let (out, _) = run(&cfg, &row_with(&[(3, 10.0)]), rows);
        assert_eq!(out.ok(), Some(vec![3; 20]));
    }

    #[test]
    fn stops_at_max_new_tokens_without_an_extra_step() {
        let cfg = GreedyConfig { repetition_penalty: 1.0, max_content_run: 1000, ..GreedyConfig::BABERU };
        let rows = vec![row_with(&[(4, 10.0)]); 300];
        let (out, calls) = run(&cfg, &row_with(&[(4, 10.0)]), rows);
        assert_eq!(out.map(|t| t.len()).ok(), Some(256));
        // 256 tokens need 255 step calls: the last token is never fed back.
        assert_eq!(calls.len(), 255);
        assert_eq!(calls.last().map(|c| c.1), Some(257 + 254));
    }

    #[test]
    fn ties_pick_the_first_index() {
        let (out, _) = run(&GreedyConfig::BABERU, &row_with(&[(6, 3.0), (7, 3.0)]), vec![]);
        assert_eq!(out.ok(), Some(vec![6]));
    }

    #[test]
    fn invalid_logits_are_typed_errors() {
        let mut nan = row_with(&[(4, 1.0)]);
        nan[8] = f32::NAN;
        let (out, _) = run(&GreedyConfig::BABERU, &nan, vec![]);
        assert!(matches!(out, Err(OrtError::BaberuDecode { .. })));

        let (out, _) = run(&GreedyConfig::BABERU, &[], vec![]);
        assert!(matches!(out, Err(OrtError::BaberuDecode { .. })));

        let (out, _) = run(&GreedyConfig::BABERU, &row_with(&[(4, 1.0)]), vec![vec![0.0; 3]]);
        assert!(matches!(out, Err(OrtError::BaberuDecode { .. })));

        // BOS outside a too-short row.
        let (out, _) = run(&GreedyConfig::BABERU, &[0.5], vec![]);
        assert!(matches!(out, Err(OrtError::BaberuDecode { .. })));
    }

    #[test]
    fn step_errors_propagate() {
        let mut step = |_: u32, _: i64| -> Result<Vec<f32>, OrtError> {
            Err(OrtError::Inference { stage: "decoder_step", reason: "boom".to_owned() })
        };
        let out = greedy_decode(&GreedyConfig::BABERU, &content, &row_with(&[(4, 1.0)]), 257, &mut step);
        assert!(matches!(out, Err(OrtError::Inference { stage: "decoder_step", .. })));
    }
}
