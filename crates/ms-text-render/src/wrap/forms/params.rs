/*
File: crates/ms-text-render/src/wrap/forms/params.rs

Purpose:
Numeric decisions of the ranked search: the corridor ladder, the hyphen budget, the
quality weights and `FormSearchParams` sanitizing.

Key items:
- default_corridor_ladder(), `CorridorLevel` / `HyphenBudget` / `QualityWeights` /
  `FormSearchParams` impls (defaults included)
- sane_non_negative() / sane_positive()
*/

use super::{
    CorridorLevel, DEFAULT_LINE_HEIGHT_UNITS, FormSearchParams, HyphenBudget, QualityWeights,
};

impl CorridorLevel {
    /// Строгая (первая) ступень лестницы: `[0.72, 1.32]`, голова `0.34`, хвост `0.30`.
    pub const STRICT: Self = Self {
        interior_lo: 0.72,
        interior_hi: 1.32,
        head_lo: 0.34,
        tail_lo: 0.30,
    };
    /// Первое послабление: `[0.60, 1.45]`, голова `0.24`, хвост `0.20`.
    pub const RELAXED: Self = Self {
        interior_lo: 0.60,
        interior_hi: 1.45,
        head_lo: 0.24,
        tail_lo: 0.20,
    };
    /// Последнее послабление: `[0.45, 1.60]`, голова `0.12`, хвост `0.10`.
    pub const LOOSE: Self = Self {
        interior_lo: 0.45,
        interior_hi: 1.60,
        head_lo: 0.12,
        tail_lo: 0.10,
    };
}

impl Default for CorridorLevel {
    fn default() -> Self {
        Self::STRICT
    }
}

/// Лестница коридоров по умолчанию: строгая ступень, затем два послабления
/// (план §2.4, шаг 5). Порядок значим — берётся первая ступень, давшая формы.
#[must_use]
pub fn default_corridor_ladder() -> Vec<CorridorLevel> {
    vec![
        CorridorLevel::STRICT,
        CorridorLevel::RELAXED,
        CorridorLevel::LOOSE,
    ]
}

impl HyphenBudget {
    /// Значения плана §2.1: 100 % при люфте ≤ 1.25, 50 % при люфте ≥ 2.0.
    pub const DEFAULT: Self = Self {
        ratio_strict: 0.50,
        ratio_relaxed: 1.00,
        slack_lo: 1.25,
        slack_hi: 2.00,
    };

    /// Допустимая доля строк с переносом при люфте `slack`
    /// (`max_width / min_possible_width`).
    ///
    /// Ниже `slack_lo` — `ratio_relaxed`, выше `slack_hi` — `ratio_strict`, между
    /// ними линейная интерполяция. Нечисловой (`NaN`/бесконечный) люфт трактуется
    /// как «места вдоволь» → `ratio_strict`. Вырожденная настройка
    /// `slack_hi <= slack_lo` также даёт `ratio_strict`.
    #[must_use]
    pub fn allowed_ratio(self, slack: f64) -> f64 {
        let lo = f64::from(self.slack_lo);
        let hi = f64::from(self.slack_hi);
        let strict = f64::from(self.ratio_strict);
        let relaxed = f64::from(self.ratio_relaxed);
        if !slack.is_finite() || slack >= hi || hi <= lo {
            return strict;
        }
        if slack <= lo {
            return relaxed;
        }
        relaxed + (strict - relaxed) * (slack - lo) / (hi - lo)
    }
}

impl Default for HyphenBudget {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl QualityWeights {
    /// Веса плана §2.2 (настроены на реальном корпусе реплик).
    pub const DEFAULT: Self = Self {
        rough: 1.00,
        uneven: 0.70,
        hyphen: 0.55,
        break_cost: 0.25,
        head: 0.45,
        tail: 0.35,
        hyphen_runs: 0.30,
        edge_transition: 0.45,
        rough_max_mix: 0.6,
        rough_mean_mix: 0.4,
        head_threshold: 0.55,
        tail_threshold: 0.45,
        hyphen_run_len: 3,
        break_cost_norm: 4.0,
    };
}

impl Default for QualityWeights {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl Default for FormSearchParams {
    fn default() -> Self {
        Self {
            aspect_max: 21.0 / 9.0,
            line_height_units: DEFAULT_LINE_HEIGHT_UNITS,
            corridor_levels: default_corridor_ladder(),
            hyphen: HyphenBudget::DEFAULT,
            quality: QualityWeights::DEFAULT,
            per_bucket: 14,
            node_budget_per_bucket: 300_000,
            form_cap_per_bucket: 3_000,
            node_budget_total: 5_000_000,
            line_range: None,
            width_range: None,
        }
    }
}

/// Конечное неотрицательное значение или `fallback` (`NaN`, бесконечность и
/// отрицательные — вне области для долей коридора, весов и порогов).
#[must_use]
fn sane_non_negative(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        fallback
    }
}

/// Конечное СТРОГО положительное значение или `fallback` (там, где ноль обнулил
/// бы знаменатель — например высота строки в пропорции формы).
#[must_use]
fn sane_positive(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}

impl FormSearchParams {
    /// Копия настроек, пригодная для арифметики поиска: каждое вещественное поле
    /// вне своей области значений заменено значением по умолчанию (см. раздел
    /// «Санация входа» в описании типа).
    ///
    /// Область значений по полям: `aspect_max` — строго положительное, при этом
    /// `f32::INFINITY` разрешена и означает «потолка нет»; `line_height_units` —
    /// конечное строго положительное; доли коридора, доли/люфты бюджета переносов
    /// и все веса качества — конечные неотрицательные. Целочисленные поля,
    /// `line_range` и `width_range` копируются как есть.
    #[must_use]
    pub fn sanitized(&self) -> Self {
        let defaults = Self::default();
        // Потолок пропорции: бесконечность осмысленна («без потолка»), а `NaN` и
        // неположительное — нет.
        let aspect_max = if self.aspect_max.is_nan() || self.aspect_max <= 0.0 {
            defaults.aspect_max
        } else {
            self.aspect_max
        };
        let strict = CorridorLevel::STRICT;
        let corridor_levels = self
            .corridor_levels
            .iter()
            .map(|level| CorridorLevel {
                interior_lo: sane_non_negative(level.interior_lo, strict.interior_lo),
                interior_hi: sane_non_negative(level.interior_hi, strict.interior_hi),
                head_lo: sane_non_negative(level.head_lo, strict.head_lo),
                tail_lo: sane_non_negative(level.tail_lo, strict.tail_lo),
            })
            .collect();
        let hyphen_default = HyphenBudget::DEFAULT;
        let hyphen = HyphenBudget {
            ratio_strict: sane_non_negative(self.hyphen.ratio_strict, hyphen_default.ratio_strict),
            ratio_relaxed: sane_non_negative(
                self.hyphen.ratio_relaxed,
                hyphen_default.ratio_relaxed,
            ),
            slack_lo: sane_non_negative(self.hyphen.slack_lo, hyphen_default.slack_lo),
            slack_hi: sane_non_negative(self.hyphen.slack_hi, hyphen_default.slack_hi),
        };
        let weights_default = QualityWeights::DEFAULT;
        let quality = QualityWeights {
            rough: sane_non_negative(self.quality.rough, weights_default.rough),
            uneven: sane_non_negative(self.quality.uneven, weights_default.uneven),
            hyphen: sane_non_negative(self.quality.hyphen, weights_default.hyphen),
            break_cost: sane_non_negative(self.quality.break_cost, weights_default.break_cost),
            head: sane_non_negative(self.quality.head, weights_default.head),
            tail: sane_non_negative(self.quality.tail, weights_default.tail),
            hyphen_runs: sane_non_negative(self.quality.hyphen_runs, weights_default.hyphen_runs),
            edge_transition: sane_non_negative(
                self.quality.edge_transition,
                weights_default.edge_transition,
            ),
            rough_max_mix: sane_non_negative(
                self.quality.rough_max_mix,
                weights_default.rough_max_mix,
            ),
            rough_mean_mix: sane_non_negative(
                self.quality.rough_mean_mix,
                weights_default.rough_mean_mix,
            ),
            head_threshold: sane_non_negative(
                self.quality.head_threshold,
                weights_default.head_threshold,
            ),
            tail_threshold: sane_non_negative(
                self.quality.tail_threshold,
                weights_default.tail_threshold,
            ),
            hyphen_run_len: self.quality.hyphen_run_len,
            break_cost_norm: sane_non_negative(
                self.quality.break_cost_norm,
                weights_default.break_cost_norm,
            ),
        };
        Self {
            aspect_max,
            line_height_units: sane_positive(self.line_height_units, defaults.line_height_units),
            corridor_levels,
            hyphen,
            quality,
            per_bucket: self.per_bucket,
            node_budget_per_bucket: self.node_budget_per_bucket,
            form_cap_per_bucket: self.form_cap_per_bucket,
            node_budget_total: self.node_budget_total,
            line_range: self.line_range,
            width_range: self.width_range,
        }
    }
}
