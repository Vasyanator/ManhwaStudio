/*
File: crates/ms-text-render/src/wrap/forms/quality.rs

Purpose:
Width statistics and the quality score `Q` of a candidate form: median, unevenness,
roughness, hyphen-run share, and `form_quality` (lower is better).

Key functions:
- median_of_widths(), unevenness_of_widths(), unevenness_pct_of_widths()
- roughness_of_widths(), roughness_pct_of_widths(), hyphen_run_share()
- form_quality(), count_as_f64(), round_to_u32()
*/

use super::{QualityBreakdown, QualityWeights};

/// Медиана набора ширин (округление половины вверх для чётного числа строк).
#[must_use]
pub(super) fn median_of_widths(widths: &[u32]) -> u32 {
    if widths.is_empty() {
        return 0;
    }
    let mut sorted = widths.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]).div_ceil(2)
    }
}

/// «Неравномерность» в долях медианы: среднее абсолютное отклонение ширин от
/// медианы, делённое на медиану. Устойчива к одиночным выбросам (короткая
/// последняя строка почти не влияет), но штрафует общий разброс
/// («лесенку»/«воронку»). `0.0` для пустого набора и нулевой медианы.
#[must_use]
fn unevenness_of_widths(widths: &[u32], median: u32) -> f64 {
    if widths.is_empty() || median == 0 {
        return 0.0;
    }
    let mean_abs_dev = widths
        .iter()
        .map(|&w| (f64::from(w) - f64::from(median)).abs())
        .sum::<f64>()
        / count_as_f64(widths.len());
    mean_abs_dev / f64::from(median)
}

/// «Неравномерность» в % — то же, что [`unevenness_of_widths`], округлённое до
/// целых процентов (поле [`TextForm::unevenness_pct`]).
#[must_use]
pub(super) fn unevenness_pct_of_widths(widths: &[u32], median: u32) -> u32 {
    round_to_u32(unevenness_of_widths(widths, median) * 100.0)
}

/// Приводит счётчик к `f64` без потери точности на реальных размерах текста
/// (счётчики форм/строк/узлов заведомо меньше `u32::MAX`; больше — насыщение).
#[must_use]
pub(super) fn count_as_f64(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}

/// Округляет неотрицательное вещественное к `u32` с насыщением. `NaN`,
/// бесконечность и отрицательные значения дают `0`/`u32::MAX` соответственно.
#[must_use]
pub(super) fn round_to_u32(value: f64) -> u32 {
    if value.is_nan() || value <= 0.0 {
        return 0;
    }
    let rounded = value.round();
    if rounded >= f64::from(u32::MAX) {
        return u32::MAX;
    }
    // Проверки выше доказывают `0.0 < rounded < u32::MAX` — приведение точное.
    rounded as u32
}

/// «Шероховатость» профиля ширин (терм `rough` плана §2.2), в долях медианы.
///
/// Для каждого перехода между соседними строками берётся `|Δw| / медиана`;
/// первый и последний переходы умножаются на `weights.edge_transition`. Результат —
/// смесь `rough_max_mix × максимум + rough_mean_mix × среднее`. Одна строка (нет
/// переходов) и нулевая медиана дают `0.0`.
#[must_use]
fn roughness_of_widths(widths: &[u32], weights: &QualityWeights) -> f64 {
    let median = f64::from(median_of_widths(widths));
    if widths.len() < 2 || median <= 0.0 {
        return 0.0;
    }
    let transitions = widths.len() - 1;
    let edge = f64::from(weights.edge_transition);
    let mut max_jump = 0.0f64;
    let mut sum = 0.0f64;
    for (index, pair) in widths.windows(2).enumerate() {
        let jump = f64::from(pair[0].abs_diff(pair[1])) / median;
        // Краевые переходы уценены: короткая первая/последняя строка — приём
        // наборщика, а не «резкий скачок ширины» (план §2.2).
        let weighted = if index == 0 || index + 1 == transitions {
            jump * edge
        } else {
            jump
        };
        max_jump = max_jump.max(weighted);
        sum += weighted;
    }
    let mean = sum / count_as_f64(transitions);
    f64::from(weights.rough_max_mix) * max_jump + f64::from(weights.rough_mean_mix) * mean
}

/// «Шероховатость» в процентах для поля [`TextForm::roughness_pct`].
#[must_use]
pub(super) fn roughness_pct_of_widths(widths: &[u32], weights: &QualityWeights) -> u32 {
    round_to_u32(roughness_of_widths(widths, weights) * 100.0)
}

/// Доля строк, попавших в серию из `run_len` и более подряд идущих строк с
/// переносом (терм `runs` плана §2.2). `run_len == 0` трактуется как «серий нет».
#[must_use]
fn hyphen_run_share(hyphen_flags: &[bool], run_len: usize) -> f64 {
    if hyphen_flags.is_empty() || run_len == 0 {
        return 0.0;
    }
    let mut in_runs = 0usize;
    let mut current = 0usize;
    for &flag in hyphen_flags {
        if flag {
            current += 1;
        } else {
            if current >= run_len {
                in_runs += current;
            }
            current = 0;
        }
    }
    if current >= run_len {
        in_runs += current;
    }
    count_as_f64(in_runs) / count_as_f64(hyphen_flags.len())
}

/// Считает оценку качества `Q` формы (план §2.2, слой B).
///
/// `hyphen_flags[i]` — несёт ли строка `i` дефис переноса (у последней строки это
/// всегда `false`); длина совпадает с `widths`. `break_cost_total` — сумма
/// `Joint::break_cost` по фактическим разрывам, `allowed_hyphens` — бюджет
/// переносов слоя A в строках (может быть дробным). Оценка width-agnostic: все
/// термы нормированы медианной шириной самой формы.
#[must_use]
pub(super) fn form_quality(
    widths: &[u32],
    hyphen_flags: &[bool],
    break_cost_total: u32,
    allowed_hyphens: f64,
    weights: &QualityWeights,
) -> QualityBreakdown {
    let roughness = roughness_of_widths(widths, weights);
    let median = f64::from(median_of_widths(widths));
    let line_count = widths.len();
    if line_count == 0 {
        return QualityBreakdown {
            quality: 0.0,
            roughness,
        };
    }

    let uneven = unevenness_of_widths(widths, median_of_widths(widths));

    let hyphen_lines = count_as_f64(hyphen_flags.iter().filter(|&&flag| flag).count());
    // Бюджет исчерпан «в ноль» только у одностроч­ных форм, где переносов и быть
    // не может; ненулевой перенос при нулевом бюджете — максимальный штраф.
    let hyphen_term = if allowed_hyphens > 0.0 {
        hyphen_lines / allowed_hyphens
    } else if hyphen_lines > 0.0 {
        1.0
    } else {
        0.0
    };

    let breaks = line_count - 1;
    let norm = f64::from(weights.break_cost_norm);
    let break_term = if breaks == 0 || norm <= 0.0 {
        0.0
    } else {
        f64::from(break_cost_total) / count_as_f64(breaks) / norm
    };

    let edge_term = |width: u32, threshold: f32| -> f64 {
        let threshold = f64::from(threshold);
        if threshold <= 0.0 || median <= 0.0 {
            return 0.0;
        }
        let ratio = f64::from(width) / median;
        (threshold - ratio).max(0.0) / threshold
    };
    let head_term = edge_term(widths[0], weights.head_threshold);
    let tail_term = edge_term(widths[line_count - 1], weights.tail_threshold);
    let runs_term = hyphen_run_share(hyphen_flags, weights.hyphen_run_len);

    let quality = f64::from(weights.rough) * roughness
        + f64::from(weights.uneven) * uneven
        + f64::from(weights.hyphen) * hyphen_term
        + f64::from(weights.break_cost) * break_term
        + f64::from(weights.head) * head_term
        + f64::from(weights.tail) * tail_term
        + f64::from(weights.hyphen_runs) * runs_term;

    QualityBreakdown { quality, roughness }
}
