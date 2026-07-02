/*
File: src/tabs/typing/render_next/wrap/shape.rs

Purpose:
Shape-aware horizontal layout поверх общего wrap-ядра нового рендера typing.

Main responsibilities:
- строить width profile для `Rectangle`, `Oval`, `Hexagon`;
- выполнять iterative reshape текста под форму без участия raster-слоя;
- возвращать предупреждения о приблизительном fallback отдельно от обычного wrap.

Source:
- `reshape_text_for_shape`
- `rectangle_target_units`
- `compute_shape_line_widths`
- `approximate_shape_warning`
из старого `src/tabs/typing/render.rs`
*/

use super::horizontal::{
    ShapeMonotonicPhase, WrapScoringContext, WrapSettings, wrap_text_with_targets_scored,
};
use super::{HyphenationDictionaries, WordBreakPolicy};
use ms_text_util::segmentation::count_layout_units;
use crate::types::{TextShape, TextWrapMode};
use cosmic_text::{Attrs, FontSystem};

#[derive(Debug, Clone)]
pub(crate) struct LayoutTextResult {
    pub(crate) text: String,
    pub(crate) warnings: Vec<String>,
}

pub(crate) struct ShapeWrapRequest<'a> {
    pub(crate) text: &'a str,
    pub(crate) font_system: &'a mut FontSystem,
    pub(crate) attrs: &'a Attrs<'a>,
    pub(crate) font_size_px: f32,
    pub(crate) line_height_px: f32,
    pub(crate) base_width_px: f32,
    pub(crate) wrap_mode: TextWrapMode,
    pub(crate) hyphen_dicts: Option<&'a HyphenationDictionaries>,
    pub(crate) word_break_policy: Option<WordBreakPolicy>,
    pub(crate) shape: TextShape,
    pub(crate) min_width_percent: f32,
    pub(crate) shape_variant: u8,
    pub(crate) allow_moderate_trees: bool,
    pub(crate) hanging_punctuation: bool,
    pub(crate) preserve_edge_spaces: bool,
}

pub(crate) fn reshape_text_for_shape(request: ShapeWrapRequest<'_>) -> LayoutTextResult {
    let base_width_px = request.base_width_px.max(1.0);
    if request.wrap_mode == TextWrapMode::None {
        return LayoutTextResult {
            text: request.text.to_string(),
            warnings: Vec::new(),
        };
    }

    let mut scoring = WrapScoringContext::new(
        request.font_system,
        request.attrs,
        request.font_size_px,
        request.line_height_px,
    );
    let base_units = scoring.estimate_base_units(
        request.text,
        request.attrs,
        base_width_px,
        request.hanging_punctuation,
    );
    let wrap_settings = WrapSettings {
        base_units,
        line_unit_targets: None,
        line_width_targets_px: None,
        line_order_phases: None,
        strict_line_order: false,
        allow_moderate_trees: request.allow_moderate_trees,
        hanging_punctuation: request.hanging_punctuation,
        hyphen_dicts: request.hyphen_dicts,
        word_break_policy: request.word_break_policy,
        preserve_edge_spaces: request.preserve_edge_spaces,
    };

    if request.shape == TextShape::Rectangle {
        let base_lines = wrap_text_with_targets_scored(request.text, wrap_settings, &mut scoring);
        let target_units = rectangle_target_units(
            base_lines.lines.as_slice(),
            base_units,
            request.hanging_punctuation,
        );
        let profile = vec![target_units; base_lines.lines.len().max(1)];
        let width_profile = vec![base_width_px; profile.len()];
        let balanced = wrap_text_with_targets_scored(
            request.text,
            WrapSettings {
                line_unit_targets: Some(profile.as_slice()),
                line_width_targets_px: Some(width_profile.as_slice()),
                ..wrap_settings
            },
            &mut scoring,
        );
        let warnings = (base_lines.used_approximate_shape_fallback
            || balanced.used_approximate_shape_fallback)
            .then(|| approximate_shape_warning(request.word_break_policy))
            .into_iter()
            .collect();
        return LayoutTextResult {
            text: balanced.lines.join("\n"),
            warnings,
        };
    }

    if request.shape == TextShape::SoftPeak {
        let soft_wrap_settings = WrapSettings {
            hanging_punctuation: true,
            ..wrap_settings
        };
        let base_lines =
            wrap_text_with_targets_scored(request.text, soft_wrap_settings, &mut scoring);
        let phases = soft_peak_order_phases(base_lines.lines.len(), request.shape_variant);
        let targets = soft_peak_line_targets(
            base_lines.lines.as_slice(),
            base_units,
            true,
            request.shape_variant,
        );
        let balanced = wrap_text_with_targets_scored(
            request.text,
            WrapSettings {
                line_unit_targets: Some(targets.as_slice()),
                line_order_phases: Some(phases.as_slice()),
                strict_line_order: true,
                ..soft_wrap_settings
            },
            &mut scoring,
        );
        let warnings = (base_lines.used_approximate_shape_fallback
            || balanced.used_approximate_shape_fallback)
            .then(|| approximate_shape_warning(request.word_break_policy))
            .into_iter()
            .collect();
        return LayoutTextResult {
            text: balanced.lines.join("\n"),
            warnings,
        };
    }

    let mut prev_profile: Option<Vec<usize>> = None;
    let mut prev_width_profile: Option<Vec<u32>> = None;
    let mut lines_result = wrap_text_with_targets_scored(request.text, wrap_settings, &mut scoring);
    let mut used_approximate_shape_fallback = lines_result.used_approximate_shape_fallback;

    const MAX_PASSES: usize = 3;
    let min_ratio = (request.min_width_percent / 100.0).clamp(0.01, 1.0);
    for _ in 0..MAX_PASSES {
        let profile = compute_shape_line_widths(
            lines_result.lines.len(),
            base_units as f32,
            request.shape,
            min_ratio,
        )
        .into_iter()
        .map(|value| value.round().max(1.0) as usize)
        .collect::<Vec<_>>();
        let width_profile = compute_shape_line_widths(
            lines_result.lines.len(),
            base_width_px,
            request.shape,
            min_ratio,
        );
        let rounded_width_profile = width_profile
            .iter()
            .map(|value| value.round().max(1.0) as u32)
            .collect::<Vec<_>>();
        if prev_profile.as_ref() == Some(&profile)
            && prev_width_profile.as_ref() == Some(&rounded_width_profile)
        {
            break;
        }
        prev_profile = Some(profile.clone());
        prev_width_profile = Some(rounded_width_profile);
        lines_result = wrap_text_with_targets_scored(
            request.text,
            WrapSettings {
                line_unit_targets: Some(profile.as_slice()),
                line_width_targets_px: Some(width_profile.as_slice()),
                ..wrap_settings
            },
            &mut scoring,
        );
        used_approximate_shape_fallback |= lines_result.used_approximate_shape_fallback;
    }

    let warnings = used_approximate_shape_fallback
        .then(|| approximate_shape_warning(request.word_break_policy))
        .into_iter()
        .collect();
    LayoutTextResult {
        text: lines_result.lines.join("\n"),
        warnings,
    }
}

fn soft_peak_order_phases(line_count: usize, _variant: u8) -> Vec<ShapeMonotonicPhase> {
    if line_count == 0 {
        return Vec::new();
    }

    let center_left = (line_count - 1) / 2;
    let center_right = line_count / 2;

    (0..line_count)
        .map(|idx| {
            if idx < center_left {
                ShapeMonotonicPhase::Expanding
            } else if idx <= center_right {
                ShapeMonotonicPhase::None
            } else {
                ShapeMonotonicPhase::Contracting
            }
        })
        .collect()
}

fn soft_peak_line_targets(
    lines: &[String],
    base_units: usize,
    hanging_punctuation: bool,
    variant: u8,
) -> Vec<usize> {
    let line_count = lines.len().max(1);
    if line_count == 1 {
        return vec![base_units.max(1)];
    }

    let tension = (f32::from(variant.clamp(1, 9)) - 1.0) / 8.0;
    let half = (line_count - 1) as f32 / 2.0;
    lines
        .iter()
        .enumerate()
        .map(|(idx, line)| {
            let current_units = count_layout_units(line.trim(), hanging_punctuation).max(1) as f32;
            let distance = if half > 0.0 {
                ((idx as f32 - half).abs() / half).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let peak_bias = 1.0 - distance;
            let target = current_units + (base_units as f32 - current_units) * peak_bias * tension;
            target.round().max(1.0) as usize
        })
        .collect()
}

fn rectangle_target_units(lines: &[String], base_units: usize, hanging_punctuation: bool) -> usize {
    let mut total = 0usize;
    let mut count = 0usize;
    for line in lines {
        let sample = line.trim();
        if sample.is_empty() {
            continue;
        }
        total = total.saturating_add(count_layout_units(sample, hanging_punctuation));
        count += 1;
    }

    if count <= 1 {
        return base_units.max(1);
    }

    let avg_units = ((total as f32) / (count as f32)).ceil() as usize;
    avg_units.clamp((base_units / 2).max(1), base_units.max(1))
}

pub(crate) fn compute_shape_line_widths(
    line_count: usize,
    base_width_px: f32,
    shape: TextShape,
    min_ratio: f32,
) -> Vec<f32> {
    if line_count == 0 {
        return Vec::new();
    }
    if line_count == 1 {
        return vec![base_width_px.max(1.0)];
    }
    if !matches!(shape, TextShape::Oval | TextShape::Hexagon) {
        return vec![base_width_px.max(1.0); line_count];
    }

    let half = (line_count - 1) as f32 / 2.0;
    let mut widths = Vec::with_capacity(line_count);
    for i in 0..line_count {
        let u = if half > 0.0 {
            ((i as f32 - half).abs() / half).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let ratio = match shape {
            TextShape::Hexagon => 1.0 - (1.0 - min_ratio) * u,
            TextShape::Oval => min_ratio + (1.0 - min_ratio) * (1.0 - u * u).sqrt(),
            TextShape::Free | TextShape::Rectangle | TextShape::SoftPeak => 1.0,
        };
        widths.push((base_width_px * ratio).max(1.0));
    }
    widths
}

fn approximate_shape_warning(word_break_policy: Option<WordBreakPolicy>) -> String {
    match word_break_policy {
        Some(WordBreakPolicy::Aggressive) => {
            "Текст не удалось точно уложить в форму; использована приблизительная форма."
                .to_string()
        }
        Some(WordBreakPolicy::Moderate) => {
            "Текст не удалось точно уложить в форму без умеренного переноса слов; использована приблизительная форма.".to_string()
        }
        Some(WordBreakPolicy::Minimal) => {
            "Текст не удалось точно уложить в форму без активного переноса слов; использована приблизительная форма.".to_string()
        }
        None => {
            "Текст не удалось точно уложить в форму без переноса слов; использована приблизительная форма.".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{compute_shape_line_widths, soft_peak_line_targets, soft_peak_order_phases};
    use crate::types::TextShape;
    use crate::wrap::horizontal::ShapeMonotonicPhase;

    #[test]
    fn oval_profile_expands_to_middle() {
        let profile = compute_shape_line_widths(5, 100.0, TextShape::Oval, 0.4);
        assert!(profile[1] >= profile[0], "{profile:?}");
        assert!(profile[2] >= profile[1], "{profile:?}");
        assert!(profile[2] >= profile[3], "{profile:?}");
        assert!(profile[3] >= profile[4], "{profile:?}");
    }

    #[test]
    fn rectangle_profile_stays_flat() {
        let profile = compute_shape_line_widths(4, 80.0, TextShape::Rectangle, 0.5);
        assert_eq!(profile, vec![80.0, 80.0, 80.0, 80.0]);
    }

    #[test]
    fn soft_peak_phases_expand_then_contract() {
        let phases = soft_peak_order_phases(5, 9);
        assert_eq!(
            phases,
            vec![
                ShapeMonotonicPhase::Expanding,
                ShapeMonotonicPhase::Expanding,
                ShapeMonotonicPhase::None,
                ShapeMonotonicPhase::Contracting,
                ShapeMonotonicPhase::Contracting,
            ]
        );
    }

    #[test]
    fn soft_peak_variant_changes_target_bias_without_min_width_slider() {
        let lines = vec!["a".to_string(), "bb".to_string(), "c".to_string()];
        let low = soft_peak_line_targets(lines.as_slice(), 10, true, 1);
        let high = soft_peak_line_targets(lines.as_slice(), 10, true, 9);

        assert_eq!(low[1], 2);
        assert!(high[1] > low[1], "{low:?} {high:?}");
    }
}
