/*
File: crates/ms-custom-fonts/src/hybrid/metrics.rs

Purpose:
Font-wide metrics of a hybrid: weighted averages of the sources' normalized vertical metrics
and style facts, quantized for the writer.

Key functions:
- `blend_metrics`: recipe weights (normalized over all sources) -> `FontMetrics`.

Notes:
An optional metric (x-height, cap height, underline) is averaged over the sources that have
it, weights renormalized; when none has it a conventional fallback is written (x-height 500,
cap height 700, underline -100/50) because the tables need a value. `usWeightClass` is the
weighted average rounded to a multiple of 100; italic when sources holding at least half the
weight are italic. Per-glyph advances are blended per glyph (`hybrid::blend_glyph`).
*/

use crate::sfnt::FontMetrics;
use crate::source::SourceMetrics;

/// Blends font-wide metrics; `weights` are normalized (sum 1), one per source.
pub(crate) fn blend_metrics(metrics: &[SourceMetrics], weights: &[f64]) -> FontMetrics {
    let average = |pick: &dyn Fn(&SourceMetrics) -> f64| -> f64 {
        metrics.iter().zip(weights).map(|(source, weight)| weight * pick(source)).sum()
    };
    let average_optional = |pick: &dyn Fn(&SourceMetrics) -> Option<f64>, fallback: f64| -> f64 {
        let (sum, total) = metrics.iter().zip(weights).fold((0.0, 0.0), |(sum, total), (source, weight)| match pick(source) {
            Some(value) => (sum + weight * value, total + weight),
            None => (sum, total),
        });
        if total > 0.0 { sum / total } else { fallback }
    };
    let italic_weight: f64 = metrics.iter().zip(weights).filter(|(source, _)| source.italic).map(|(_, weight)| weight).sum();
    let weight_class = (average(&|source| source.weight_class) / 100.0).round() * 100.0;
    FontMetrics {
        ascender: units(average(&|source| source.ascender)),
        descender: units(average(&|source| source.descender)),
        line_gap: units(average(&|source| source.line_gap).max(0.0)),
        x_height: units(average_optional(&|source| source.x_height, 500.0)),
        cap_height: units(average_optional(&|source| source.cap_height, 700.0)),
        underline_position: units(average_optional(&|source| source.underline_position, -100.0)),
        underline_thickness: units(average_optional(&|source| source.underline_thickness, 50.0)),
        weight_class: u16::try_from(units(weight_class.clamp(100.0, 900.0))).unwrap_or(400),
        italic: italic_weight >= 0.5,
        italic_angle: average(&|source| source.italic_angle),
    }
}

/// Rounds to integer font units, saturating at the `i16` range.
pub(crate) fn units(value: f64) -> i16 {
    let rounded = value.round();
    if !rounded.is_finite() {
        return 0;
    }
    let clamped = rounded.clamp(f64::from(i16::MIN), f64::from(i16::MAX));
    #[expect(clippy::cast_possible_truncation, reason = "clamped to the i16 range just above")]
    let value = clamped as i16;
    value
}
