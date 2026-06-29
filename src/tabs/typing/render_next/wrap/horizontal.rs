/*
File: src/tabs/typing/render_next/wrap/horizontal.rs

Purpose:
DP-ядро horizontal wrap нового рендера typing.

Main responsibilities:
- подбирать line breaks через scoring + dictionary/emergency fallback;
- отдавать стабильный horizontal wrap для free и shape профилей.

Разбивка абзаца на блоки (`Block`/`Joint`) и языковые правила связывания/переноса
живут в `crate::tabs::typing::segmentation` (см. `Segmenter::segment`); здесь
остаётся только DP-подбор переносов поверх готовых блоков.

Source:
- `collect_line_break_candidates`
- `wrap_text_with_targets*`
- `violates_tree_width_rule`
- связанные wrap/scoring helper-ы из старого `src/tabs/typing/render.rs`
*/

use super::hyphenation::{
    append_wrapped_hyphen, find_dictionary_split_index, find_emergency_split_index,
};
use super::{
    CONSERVATIVE_DICTIONARY_BREAK_PENALTY, EMERGENCY_BREAK_PENALTY,
    MODERATE_TREE_CONTRACTING_RATIO, MODERATE_TREE_EXPANDING_RATIO, SHORT_HYPHEN_TAIL_PENALTY,
    SOFT_HYPHEN, SOFT_WRAP_WIDTH_TOLERANCE, WordBreakPolicy, is_hanging_punctuation,
};
use crate::tabs::typing::segmentation::{
    BindingMode, Block, HyphenationDictionaries, SegmentOptions, build_line_text_and_units,
    count_layout_units, with_default_segmenter,
};
use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping};
use std::collections::HashMap;

#[derive(Debug, Clone)]
struct LineBreakCandidate {
    consumed_blocks: usize,
    split_remainder: Option<Block>,
    line_text: String,
    line_units: usize,
    break_penalty: f32,
}

#[derive(Debug, Clone)]
struct WrapParagraphSolution {
    lines: Vec<String>,
    score: f32,
}

#[derive(Debug, Clone)]
pub(super) struct WrapTextResult {
    pub(super) lines: Vec<String>,
    pub(super) used_approximate_shape_fallback: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum ShapeMonotonicPhase {
    None,
    Expanding,
    Contracting,
}

pub(super) struct WrapScoringContext<'font, 'attrs> {
    font_system: Option<&'font mut FontSystem>,
    attrs: Option<&'attrs Attrs<'attrs>>,
    font_size_px: f32,
    line_height_px: f32,
    width_cache_px: HashMap<String, f32>,
}

impl<'font, 'attrs> WrapScoringContext<'font, 'attrs> {
    #[cfg(test)]
    fn fallback() -> Self {
        Self {
            font_system: None,
            attrs: None,
            font_size_px: 1.0,
            line_height_px: 1.0,
            width_cache_px: HashMap::new(),
        }
    }

    pub(super) fn new(
        font_system: &'font mut FontSystem,
        attrs: &'attrs Attrs<'attrs>,
        font_size_px: f32,
        line_height_px: f32,
    ) -> Self {
        Self {
            font_system: Some(font_system),
            attrs: Some(attrs),
            font_size_px,
            line_height_px,
            width_cache_px: HashMap::new(),
        }
    }

    pub(super) fn measure_line_width_px(&mut self, line_text: &str, fallback_units: usize) -> f32 {
        if let Some(width_px) = self.width_cache_px.get(line_text).copied() {
            return width_px;
        }

        let width_px = match (self.font_system.as_deref_mut(), self.attrs) {
            (Some(font_system), Some(attrs)) => measure_word_width_px(
                line_text,
                font_system,
                attrs,
                self.font_size_px,
                self.line_height_px,
            ),
            _ => fallback_units as f32,
        };
        self.width_cache_px.insert(line_text.to_string(), width_px);
        width_px
    }

    pub(super) fn estimate_base_units(
        &mut self,
        text: &str,
        attrs: &Attrs<'_>,
        base_width_px: f32,
        hanging_punctuation: bool,
    ) -> usize {
        if let Some(font_system) = self.font_system.as_deref_mut() {
            estimate_line_capacity_units(
                text,
                font_system,
                attrs,
                self.font_size_px,
                self.line_height_px,
                base_width_px,
                hanging_punctuation,
            )
        } else {
            1
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WrapMemoKey {
    remaining_blocks: Vec<Block>,
    line_idx: usize,
    prev_line_units: Option<usize>,
    prev_line_width_px: Option<u32>,
    must_not_expand: bool,
}

#[derive(Clone, Copy)]
pub(super) struct WrapSettings<'a> {
    pub(super) base_units: usize,
    pub(super) line_unit_targets: Option<&'a [usize]>,
    pub(super) line_width_targets_px: Option<&'a [f32]>,
    pub(super) line_order_phases: Option<&'a [ShapeMonotonicPhase]>,
    pub(super) strict_line_order: bool,
    pub(super) allow_moderate_trees: bool,
    pub(super) hanging_punctuation: bool,
    pub(super) hyphen_dicts: Option<&'a HyphenationDictionaries>,
    pub(super) word_break_policy: Option<WordBreakPolicy>,
    pub(super) preserve_edge_spaces: bool,
}

impl WrapSettings<'_> {
    fn line_target_units(self, line_idx: usize) -> usize {
        match self.line_unit_targets {
            Some(values) if !values.is_empty() => values
                .get(line_idx)
                .copied()
                .or_else(|| values.last().copied())
                .unwrap_or(self.base_units)
                .max(1),
            _ => self.base_units.max(1),
        }
    }

    fn line_target_width_px(self, line_idx: usize) -> f32 {
        match self.line_width_targets_px {
            Some(widths) if !widths.is_empty() => widths
                .get(line_idx)
                .copied()
                .or_else(|| widths.last().copied())
                .unwrap_or(1.0)
                .max(1.0),
            _ => self.line_target_units(line_idx).max(1) as f32,
        }
    }
}

#[derive(Debug, Clone)]
struct SolveState {
    remaining_blocks: Vec<Block>,
    line_idx: usize,
    prev_line_units: Option<usize>,
    prev_line_width_px: Option<u32>,
    must_not_expand: bool,
}

pub(super) fn wrap_text_with_targets_scored(
    text: &str,
    settings: WrapSettings<'_>,
    scoring: &mut WrapScoringContext<'_, '_>,
) -> WrapTextResult {
    let mut out = Vec::<String>::new();
    let mut global_line_idx = 0usize;
    let mut used_approximate_shape_fallback = false;

    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            out.push(String::new());
            global_line_idx += 1;
            continue;
        }

        let mut wrapped =
            wrap_paragraph_with_targets_scored(paragraph, settings, &mut global_line_idx, scoring);
        used_approximate_shape_fallback |= wrapped.used_approximate_shape_fallback;
        out.append(&mut wrapped.lines);
    }

    if out.is_empty() {
        out.push(String::new());
    }
    WrapTextResult {
        lines: out,
        used_approximate_shape_fallback,
    }
}

pub(super) fn estimate_line_capacity_units(
    text: &str,
    font_system: &mut FontSystem,
    attrs: &Attrs<'_>,
    font_size_px: f32,
    line_height_px: f32,
    base_width_px: f32,
    hanging_punctuation: bool,
) -> usize {
    let sample_text = text
        .chars()
        .filter(|&ch| {
            ch != SOFT_HYPHEN
                && ch != '\n'
                && ch != '\r'
                && (!hanging_punctuation || !is_hanging_punctuation(ch))
        })
        .collect::<String>();
    let sample_units = sample_text.chars().count();
    if sample_units == 0 {
        return 1;
    }
    let sample_width_px = measure_word_width_px(
        sample_text.as_str(),
        font_system,
        attrs,
        font_size_px,
        line_height_px,
    );
    let avg_char_width_px = (sample_width_px / sample_units as f32).max(1.0);
    ((base_width_px / avg_char_width_px).floor() as usize).max(1)
}

fn wrap_paragraph_with_targets_scored(
    paragraph: &str,
    settings: WrapSettings<'_>,
    global_line_idx: &mut usize,
    scoring: &mut WrapScoringContext<'_, '_>,
) -> WrapTextResult {
    let blocks = with_default_segmenter(|seg| {
        seg.segment(
            paragraph,
            SegmentOptions {
                hanging_punctuation: settings.hanging_punctuation,
                preserve_edge_spaces: settings.preserve_edge_spaces,
                allow_hard_hyphen_breaks: settings.word_break_policy.is_some(),
                // Врапер склеивает служебные слова: гарантия против сиротливых предлогов.
                binding: BindingMode::Glue,
            },
        )
    });
    let start_line_idx = *global_line_idx;
    let mut memo = HashMap::<WrapMemoKey, Option<WrapParagraphSolution>>::new();
    let best = solve_wrap_paragraph_dp(
        SolveState {
            remaining_blocks: blocks.clone(),
            line_idx: start_line_idx,
            prev_line_units: None,
            prev_line_width_px: None,
            must_not_expand: false,
        },
        settings,
        scoring,
        &mut memo,
    );
    let mut out = best.map(|state| state.lines).unwrap_or_default();
    let mut used_approximate_shape_fallback = false;

    if out.is_empty() {
        let approximate = approximate_wrap_paragraph_to_shape(
            blocks.as_slice(),
            start_line_idx,
            settings,
            scoring,
        );
        if approximate.is_empty() {
            out.push(paragraph.trim_end().to_string());
        } else {
            out = approximate;
            used_approximate_shape_fallback = true;
        }
    }
    *global_line_idx = (*global_line_idx).saturating_add(out.len());
    WrapTextResult {
        lines: out,
        used_approximate_shape_fallback,
    }
}

fn solve_wrap_paragraph_dp(
    state: SolveState,
    settings: WrapSettings<'_>,
    scoring: &mut WrapScoringContext<'_, '_>,
    memo: &mut HashMap<WrapMemoKey, Option<WrapParagraphSolution>>,
) -> Option<WrapParagraphSolution> {
    let SolveState {
        remaining_blocks,
        line_idx,
        prev_line_units,
        prev_line_width_px,
        must_not_expand,
    } = state;
    if remaining_blocks.is_empty() {
        return Some(WrapParagraphSolution {
            lines: Vec::new(),
            score: 0.0,
        });
    }

    let memo_key = WrapMemoKey {
        remaining_blocks: remaining_blocks.clone(),
        line_idx,
        prev_line_units,
        prev_line_width_px,
        must_not_expand,
    };
    if let Some(cached) = memo.get(&memo_key) {
        return cached.clone();
    }

    let mut best: Option<WrapParagraphSolution> = None;
    let max_units = settings.line_target_units(line_idx);
    let target_width_px = settings.line_target_width_px(line_idx);
    let mut candidate_sets = Vec::<Vec<LineBreakCandidate>>::new();
    let preferred = collect_line_break_candidates(remaining_blocks.as_slice(), max_units);
    match settings.word_break_policy {
        Some(WordBreakPolicy::Aggressive) => {
            if !preferred.is_empty() {
                candidate_sets.push(preferred.clone());
            }
            if let Some(dicts) = settings.hyphen_dicts
                && let Some(fallback) = build_dictionary_break_candidate(
                    remaining_blocks.as_slice(),
                    max_units,
                    target_width_px,
                    settings.hanging_punctuation,
                    dicts,
                    scoring,
                )
            {
                let mut candidates = preferred.clone();
                push_unique_line_break_candidate(&mut candidates, fallback);
                candidate_sets.push(candidates);
            }
            if candidate_sets.is_empty()
                && let Some(fallback) = build_emergency_break_candidate(
                    remaining_blocks.as_slice(),
                    max_units,
                    settings.hanging_punctuation,
                )
            {
                candidate_sets.push(vec![fallback]);
            }
        }
        Some(WordBreakPolicy::Minimal | WordBreakPolicy::Moderate) => {
            let mut candidates = preferred;
            if let Some(dicts) = settings.hyphen_dicts
                && let Some(fallback) = build_dictionary_break_candidate(
                    remaining_blocks.as_slice(),
                    max_units,
                    target_width_px,
                    settings.hanging_punctuation,
                    dicts,
                    scoring,
                )
            {
                push_unique_line_break_candidate(&mut candidates, fallback);
            }
            if let Some(fallback) = build_emergency_break_candidate(
                remaining_blocks.as_slice(),
                max_units,
                settings.hanging_punctuation,
            ) {
                push_unique_line_break_candidate(&mut candidates, fallback);
            }
            if !candidates.is_empty() {
                candidate_sets.push(candidates);
            }
        }
        None => {
            if !preferred.is_empty() {
                candidate_sets.push(preferred);
            }
        }
    }

    'candidate_sets: for candidates in candidate_sets {
        for candidate in candidates {
            let candidate_width_px =
                scoring.measure_line_width_px(candidate.line_text.as_str(), candidate.line_units);
            if let Some(prev_units) = prev_line_units {
                let phase = shape_monotonic_phase(settings, line_idx);
                let disallowed_contraction = settings.strict_line_order
                    && matches!(phase, ShapeMonotonicPhase::Expanding)
                    && candidate.line_units < prev_units;
                let disallowed_expansion = matches!(phase, ShapeMonotonicPhase::Contracting)
                    || (matches!(phase, ShapeMonotonicPhase::None) && must_not_expand);
                if disallowed_contraction
                    || (disallowed_expansion && candidate.line_units > prev_units)
                {
                    continue;
                }
            }
            if violates_tree_width_rule(
                line_idx,
                prev_line_width_px,
                candidate_width_px,
                must_not_expand,
                settings,
            ) {
                continue;
            }

            let remaining = apply_line_break_candidate(remaining_blocks.as_slice(), &candidate);
            let candidate_width_bucket = candidate_width_px.round().max(0.0) as u32;
            let next_must_not_expand = must_not_expand
                || shape_monotonic_phase(settings, line_idx) == ShapeMonotonicPhase::Contracting
                || prev_line_units.is_some_and(|prev_units| candidate.line_units < prev_units);
            let Some(mut tail_solution) = solve_wrap_paragraph_dp(
                SolveState {
                    remaining_blocks: remaining,
                    line_idx: line_idx.saturating_add(1),
                    prev_line_units: Some(candidate.line_units),
                    prev_line_width_px: Some(candidate_width_bucket),
                    must_not_expand: next_must_not_expand,
                },
                settings,
                scoring,
                memo,
            ) else {
                continue;
            };

            let score = compute_line_fit_penalty(
                line_idx,
                prev_line_units,
                must_not_expand,
                candidate.line_units,
                candidate_width_px,
                settings,
            ) + candidate.break_penalty
                + tail_solution.score;

            let mut lines = Vec::with_capacity(tail_solution.lines.len().saturating_add(1));
            lines.push(candidate.line_text);
            lines.append(&mut tail_solution.lines);
            let candidate_solution = WrapParagraphSolution { lines, score };

            if best
                .as_ref()
                .is_none_or(|current| candidate_solution.score < current.score)
            {
                best = Some(candidate_solution);
            }
        }
        if best.is_some() {
            break 'candidate_sets;
        }
    }

    memo.insert(memo_key, best.clone());
    best
}

fn approximate_wrap_paragraph_to_shape(
    blocks: &[Block],
    start_line_idx: usize,
    settings: WrapSettings<'_>,
    scoring: &mut WrapScoringContext<'_, '_>,
) -> Vec<String> {
    let mut remaining = blocks.to_vec();
    let mut lines = Vec::new();
    let mut line_idx = start_line_idx;
    let mut prev_line_width_px: Option<u32> = None;

    while !remaining.is_empty() {
        let max_units = settings.line_target_units(line_idx);
        let candidate = select_approximate_line_break_candidate(
            remaining.as_slice(),
            max_units,
            line_idx,
            prev_line_width_px,
            settings,
            scoring,
        );
        let Some(candidate) = candidate else {
            let Some(forced) =
                build_overflow_block_candidate(remaining.as_slice(), settings.hanging_punctuation)
            else {
                break;
            };
            let forced_width =
                scoring.measure_line_width_px(forced.line_text.as_str(), forced.line_units);
            remaining = apply_line_break_candidate(remaining.as_slice(), &forced);
            lines.push(forced.line_text);
            prev_line_width_px = Some(forced_width.round().max(0.0) as u32);
            line_idx = line_idx.saturating_add(1);
            continue;
        };
        let candidate_width_px =
            scoring.measure_line_width_px(candidate.line_text.as_str(), candidate.line_units);
        remaining = apply_line_break_candidate(remaining.as_slice(), &candidate);
        lines.push(candidate.line_text);
        prev_line_width_px = Some(candidate_width_px.round().max(0.0) as u32);
        line_idx = line_idx.saturating_add(1);
    }

    lines
}

fn select_approximate_line_break_candidate(
    blocks: &[Block],
    max_units: usize,
    line_idx: usize,
    prev_line_width_px: Option<u32>,
    settings: WrapSettings<'_>,
    scoring: &mut WrapScoringContext<'_, '_>,
) -> Option<LineBreakCandidate> {
    let mut candidates = collect_line_break_candidates(blocks, max_units);
    if candidates.is_empty()
        && let Some(first_block) =
            build_overflow_block_candidate(blocks, settings.hanging_punctuation)
    {
        candidates.push(first_block);
    }

    let filtered_candidates = candidates
        .into_iter()
        .filter_map(|candidate| {
            let candidate_width =
                scoring.measure_line_width_px(candidate.line_text.as_str(), candidate.line_units);
            (!violates_tree_width_rule(
                line_idx,
                prev_line_width_px,
                candidate_width,
                false,
                settings,
            ))
            .then_some(candidate)
        })
        .collect::<Vec<_>>();

    filtered_candidates.into_iter().min_by(|left, right| {
        let left_width = scoring.measure_line_width_px(left.line_text.as_str(), left.line_units);
        let right_width = scoring.measure_line_width_px(right.line_text.as_str(), right.line_units);
        let left_penalty =
            compute_line_fit_penalty(line_idx, None, false, left.line_units, left_width, settings);
        let right_penalty = compute_line_fit_penalty(
            line_idx,
            None,
            false,
            right.line_units,
            right_width,
            settings,
        );
        left_penalty.total_cmp(&right_penalty)
    })
}

fn build_overflow_block_candidate(
    blocks: &[Block],
    hanging_punctuation: bool,
) -> Option<LineBreakCandidate> {
    let block = blocks.first()?;
    let wraps_at_soft_hyphen = blocks.len() > 1 && block.joint.is_soft_hyphen();
    let line_text = build_line_text_and_units(&blocks[..1], wraps_at_soft_hyphen).0;
    Some(LineBreakCandidate {
        consumed_blocks: 1,
        split_remainder: None,
        line_text,
        line_units: count_layout_units(block.text.as_str(), hanging_punctuation),
        break_penalty: 0.0,
    })
}

fn collect_line_break_candidates(
    blocks: &[Block],
    max_units: usize,
) -> Vec<LineBreakCandidate> {
    let mut out = Vec::<LineBreakCandidate>::new();
    for end in 1..=blocks.len() {
        let wraps_here = end < blocks.len();
        let (line_text, line_units) = build_line_text_and_units(&blocks[..end], wraps_here);
        if line_text.is_empty() {
            continue;
        }
        if line_units > max_units {
            break;
        }
        out.push(LineBreakCandidate {
            consumed_blocks: end,
            split_remainder: None,
            line_text,
            line_units,
            break_penalty: 0.0,
        });
    }
    out
}

fn build_emergency_break_candidate(
    blocks: &[Block],
    max_units: usize,
    hanging_punctuation: bool,
) -> Option<LineBreakCandidate> {
    let block = blocks.first()?;
    if block.text.is_empty() || !is_hyphenatable_wrap_block(block) {
        return None;
    }
    let split_at =
        find_emergency_split_index(block.text.as_str(), max_units.max(1), hanging_punctuation)?;
    let head = block.text[..split_at].to_string();
    let tail = block.text[split_at..].to_string();
    let line_units = count_layout_units(head.as_str(), hanging_punctuation);
    Some(LineBreakCandidate {
        consumed_blocks: 1,
        split_remainder: Some(Block {
            text: tail,
            joint: block.joint.clone(),
            unit_count: count_layout_units(&block.text[split_at..], hanging_punctuation),
        }),
        line_text: append_wrapped_hyphen(head.as_str()),
        line_units,
        break_penalty: EMERGENCY_BREAK_PENALTY + hyphen_tail_penalty(block.text[split_at..].trim()),
    })
}

fn build_dictionary_break_candidate(
    blocks: &[Block],
    max_units: usize,
    target_width_px: f32,
    hanging_punctuation: bool,
    dicts: &HyphenationDictionaries,
    scoring: &mut WrapScoringContext<'_, '_>,
) -> Option<LineBreakCandidate> {
    let block = blocks.first()?;
    if block.text.is_empty() || !is_hyphenatable_wrap_block(block) {
        return None;
    }
    let split_at = find_dictionary_split_index(
        block.text.as_str(),
        max_units.max(1),
        target_width_px,
        hanging_punctuation,
        dicts,
        scoring,
    )?;
    let head = block.text[..split_at].to_string();
    let tail = block.text[split_at..].to_string();
    let line_text = append_wrapped_hyphen(head.as_str());
    let line_units = count_layout_units(head.as_str(), hanging_punctuation);
    Some(LineBreakCandidate {
        consumed_blocks: 1,
        split_remainder: Some(Block {
            text: tail,
            joint: block.joint.clone(),
            unit_count: count_layout_units(&block.text[split_at..], hanging_punctuation),
        }),
        line_text,
        line_units,
        break_penalty: CONSERVATIVE_DICTIONARY_BREAK_PENALTY
            + hyphen_tail_penalty(block.text[split_at..].trim()),
    })
}

fn push_unique_line_break_candidate(
    candidates: &mut Vec<LineBreakCandidate>,
    candidate: LineBreakCandidate,
) {
    let already_exists = candidates.iter().any(|existing| {
        existing.consumed_blocks == candidate.consumed_blocks
            && existing.line_text == candidate.line_text
            && existing.line_units == candidate.line_units
            && existing.split_remainder == candidate.split_remainder
    });
    if !already_exists {
        candidates.push(candidate);
    }
}

fn hyphen_tail_penalty(tail: &str) -> f32 {
    let tail_alpha = tail
        .chars()
        .filter(|ch| ch.is_alphabetic() && *ch != SOFT_HYPHEN)
        .count();
    if tail_alpha <= 2 {
        SHORT_HYPHEN_TAIL_PENALTY
    } else if tail_alpha == 3 {
        SHORT_HYPHEN_TAIL_PENALTY * 0.5
    } else {
        0.0
    }
}

fn is_hyphenatable_wrap_block(block: &Block) -> bool {
    !block.text.chars().any(char::is_whitespace)
}

fn apply_line_break_candidate(
    blocks: &[Block],
    candidate: &LineBreakCandidate,
) -> Vec<Block> {
    let mut remaining = blocks[candidate.consumed_blocks..].to_vec();
    if let Some(remainder) = candidate.split_remainder.as_ref() {
        remaining.insert(0, remainder.clone());
    }
    remaining
}

fn compute_line_fit_penalty(
    line_idx: usize,
    prev_line_units: Option<usize>,
    must_not_expand: bool,
    candidate_units: usize,
    candidate_width_px: f32,
    settings: WrapSettings<'_>,
) -> f32 {
    let target_units = settings.line_target_units(line_idx);
    let target_width_px = settings.line_target_width_px(line_idx);
    let slack_units = target_units.saturating_sub(candidate_units) as f32;
    let overflow_units = candidate_units.saturating_sub(target_units) as f32;
    let slack_width_px = (target_width_px - candidate_width_px).max(0.0);
    let overflow_width_px = (candidate_width_px - target_width_px).max(0.0);
    let width_scale = target_width_px.max(1.0);

    let mut penalty = slack_units * slack_units
        + overflow_units * overflow_units * 12.0
        + (slack_width_px / width_scale).powi(2) * 900.0
        + (overflow_width_px / width_scale).powi(2) * 3600.0;
    let soft_overflow_width_px =
        (candidate_width_px - target_width_px * SOFT_WRAP_WIDTH_TOLERANCE).max(0.0);
    penalty += (soft_overflow_width_px / width_scale).powi(2) * 7200.0;

    if let Some(prev_units) = prev_line_units {
        let phase = shape_monotonic_phase(settings, line_idx);
        let monotonic_violation = match phase {
            ShapeMonotonicPhase::Expanding => prev_units.saturating_sub(candidate_units),
            ShapeMonotonicPhase::Contracting => candidate_units.saturating_sub(prev_units),
            ShapeMonotonicPhase::None if must_not_expand => {
                candidate_units.saturating_sub(prev_units)
            }
            ShapeMonotonicPhase::None => 0,
        } as f32;
        penalty += monotonic_violation.powi(2) * 5000.0;
    }

    penalty
}

fn violates_tree_width_rule(
    line_idx: usize,
    prev_line_width_px: Option<u32>,
    candidate_width_px: f32,
    must_not_expand: bool,
    settings: WrapSettings<'_>,
) -> bool {
    if !matches!(
        settings.word_break_policy,
        Some(WordBreakPolicy::Minimal | WordBreakPolicy::Moderate)
    ) {
        return false;
    }
    let Some(prev_width_px) = prev_line_width_px.map(|value| value as f32) else {
        return false;
    };

    let phase = shape_monotonic_phase(settings, line_idx);
    match phase {
        ShapeMonotonicPhase::Expanding => {
            let min_ratio = if settings.allow_moderate_trees {
                MODERATE_TREE_EXPANDING_RATIO
            } else {
                1.0
            };
            candidate_width_px + 0.5 < prev_width_px * min_ratio
        }
        ShapeMonotonicPhase::Contracting => {
            let max_ratio = if settings.allow_moderate_trees {
                MODERATE_TREE_CONTRACTING_RATIO
            } else {
                1.0
            };
            candidate_width_px > prev_width_px * max_ratio + 0.5
        }
        ShapeMonotonicPhase::None if must_not_expand => {
            let max_ratio = if settings.allow_moderate_trees {
                MODERATE_TREE_CONTRACTING_RATIO
            } else {
                1.0
            };
            candidate_width_px > prev_width_px * max_ratio + 0.5
        }
        ShapeMonotonicPhase::None => false,
    }
}

fn shape_monotonic_phase(settings: WrapSettings<'_>, line_idx: usize) -> ShapeMonotonicPhase {
    if line_idx == 0 {
        return ShapeMonotonicPhase::None;
    }

    if let Some(phases) = settings.line_order_phases
        && let Some(phase) = phases.get(line_idx).copied()
    {
        return phase;
    }

    let previous_target = settings.line_target_units(line_idx - 1);
    let current_target = settings.line_target_units(line_idx);
    if current_target > previous_target {
        ShapeMonotonicPhase::Expanding
    } else if current_target < previous_target {
        ShapeMonotonicPhase::Contracting
    } else {
        ShapeMonotonicPhase::None
    }
}

fn measure_word_width_px(
    word: &str,
    font_system: &mut FontSystem,
    attrs: &Attrs<'_>,
    font_size_px: f32,
    line_height_px: f32,
) -> f32 {
    let mut measure = Buffer::new(
        font_system,
        Metrics::new(font_size_px.max(1.0), line_height_px.max(1.0)),
    );
    measure.set_size(font_system, None, None);
    measure.set_text(font_system, word, attrs, Shaping::Advanced);
    measure.shape_until_scroll(font_system, false);
    measure
        .layout_runs()
        .fold(0.0f32, |max_w, run| max_w.max(run.line_w))
}

#[cfg(test)]
mod tests {
    use super::{
        WrapScoringContext, WrapSettings, violates_tree_width_rule, wrap_text_with_targets_scored,
    };
    use crate::tabs::typing::render_next::wrap::WordBreakPolicy;
    use crate::tabs::typing::segmentation::{
        BindingMode, count_layout_units, with_default_segmenter,
    };

    /// Тестовый шорткат: сегменты абзаца дефолтным (русским) сегментатором.
    fn build_wrap_segments(text: &str, preserve_edge_spaces: bool) -> Vec<String> {
        with_default_segmenter(|seg| {
            seg.build_segments(text, preserve_edge_spaces, BindingMode::Glue)
        })
    }

    fn wrap_text_with_targets(
        text: &str,
        base_units: usize,
        line_unit_targets: Option<&[usize]>,
        allow_moderate_trees: bool,
        hanging_punctuation: bool,
    ) -> Vec<String> {
        let mut scoring = WrapScoringContext::fallback();
        wrap_text_with_targets_scored(
            text,
            WrapSettings {
                base_units,
                line_unit_targets,
                line_width_targets_px: None,
                line_order_phases: None,
                strict_line_order: false,
                allow_moderate_trees,
                hanging_punctuation,
                hyphen_dicts: None,
                word_break_policy: Some(WordBreakPolicy::Minimal),
                preserve_edge_spaces: false,
            },
            &mut scoring,
        )
        .lines
    }

    fn compute_shape_line_widths_for_test(
        line_count: usize,
        base_width_px: f32,
        oval: bool,
        min_ratio: f32,
    ) -> Vec<usize> {
        let shape = if oval {
            crate::tabs::typing::render_next::types::TextShape::Oval
        } else {
            crate::tabs::typing::render_next::types::TextShape::Hexagon
        };
        crate::tabs::typing::render_next::wrap::shape::compute_shape_line_widths(
            line_count,
            base_width_px,
            shape,
            min_ratio,
        )
        .into_iter()
        .map(|value| value.round().max(1.0) as usize)
        .collect()
    }

    #[test]
    fn wrap_segments_keep_negation_with_following_word() {
        let texts = build_wrap_segments("не знаю что", false);
        assert_eq!(texts, vec!["не знаю ".to_string(), "что".to_string()]);
    }

    #[test]
    fn wrap_segments_keep_particle_with_previous_word() {
        let texts = build_wrap_segments("сделай же это", false);
        assert_eq!(texts, vec!["сделай же ".to_string(), "это".to_string()]);
    }

    #[test]
    fn wrap_segments_keep_russian_abbreviation_with_following_word() {
        let texts = build_wrap_segments("ул. Ленина рядом", false);
        assert_eq!(texts, vec!["ул. Ленина ".to_string(), "рядом".to_string()]);
    }

    #[test]
    fn wrap_segments_keep_standalone_dash_with_previous_word() {
        let texts = build_wrap_segments("слово — слово", false);
        assert_eq!(texts, vec!["слово — ".to_string(), "слово".to_string()]);

        let hyphen_texts = build_wrap_segments("word - word", false);
        assert_eq!(
            hyphen_texts,
            vec!["word - ".to_string(), "word".to_string()]
        );
    }

    #[test]
    fn minimal_word_wrap_keeps_standalone_dash_at_line_end() {
        let mut scoring = WrapScoringContext::fallback();
        let wrapped = wrap_text_with_targets_scored(
            "слово — слово",
            WrapSettings {
                base_units: 5,
                line_unit_targets: Some(&[5, 5, 5]),
                line_width_targets_px: None,
                line_order_phases: None,
                strict_line_order: false,
                allow_moderate_trees: false,
                hanging_punctuation: false,
                hyphen_dicts: None,
                word_break_policy: Some(WordBreakPolicy::Minimal),
                preserve_edge_spaces: false,
            },
            &mut scoring,
        );

        assert_eq!(wrapped.lines[0], "слово —");
        assert_eq!(wrapped.lines[1], "слово");
    }

    #[test]
    fn minimal_word_wrap_can_break_after_inline_em_dash() {
        let mut scoring = WrapScoringContext::fallback();
        let wrapped = wrap_text_with_targets_scored(
            "слово—слово",
            WrapSettings {
                base_units: 6,
                line_unit_targets: Some(&[6, 5]),
                line_width_targets_px: None,
                line_order_phases: None,
                strict_line_order: false,
                allow_moderate_trees: false,
                hanging_punctuation: false,
                hyphen_dicts: None,
                word_break_policy: Some(WordBreakPolicy::Minimal),
                preserve_edge_spaces: false,
            },
            &mut scoring,
        );

        assert_eq!(wrapped.lines[0], "слово—");
        assert_eq!(wrapped.lines[1], "слово");
    }

    #[test]
    fn minimal_word_wrap_can_break_after_existing_hyphen() {
        let mut scoring = WrapScoringContext::fallback();
        let wrapped = wrap_text_with_targets_scored(
            "Рао-кун рядом",
            WrapSettings {
                base_units: 4,
                line_unit_targets: Some(&[4, 4, 6]),
                line_width_targets_px: None,
                line_order_phases: None,
                strict_line_order: false,
                allow_moderate_trees: false,
                hanging_punctuation: false,
                hyphen_dicts: None,
                word_break_policy: Some(WordBreakPolicy::Minimal),
                preserve_edge_spaces: false,
            },
            &mut scoring,
        );

        assert_eq!(wrapped.lines[0], "Рао-");
        assert_eq!(wrapped.lines[1], "кун");
    }

    #[test]
    fn whole_word_wrap_keeps_existing_hyphenated_word_together() {
        let mut scoring = WrapScoringContext::fallback();
        let wrapped = wrap_text_with_targets_scored(
            "Рао-кун рядом",
            WrapSettings {
                base_units: 4,
                line_unit_targets: Some(&[4, 4, 6]),
                line_width_targets_px: None,
                line_order_phases: None,
                strict_line_order: false,
                allow_moderate_trees: false,
                hanging_punctuation: false,
                hyphen_dicts: None,
                word_break_policy: None,
                preserve_edge_spaces: false,
            },
            &mut scoring,
        );

        assert!(wrapped.used_approximate_shape_fallback);
        assert_eq!(wrapped.lines[0], "Рао-кун");
    }

    #[test]
    fn existing_hyphen_wrap_does_not_split_urls() {
        let mut scoring = WrapScoringContext::fallback();
        let wrapped = wrap_text_with_targets_scored(
            "site-a://host рядом",
            WrapSettings {
                base_units: 6,
                line_unit_targets: Some(&[6, 12]),
                line_width_targets_px: None,
                line_order_phases: None,
                strict_line_order: false,
                allow_moderate_trees: false,
                hanging_punctuation: false,
                hyphen_dicts: None,
                word_break_policy: Some(WordBreakPolicy::Minimal),
                preserve_edge_spaces: false,
            },
            &mut scoring,
        );

        assert_eq!(wrapped.lines[0], "site-a://host");
    }

    #[test]
    fn wrap_segments_keep_number_with_measurement() {
        let texts = build_wrap_segments("5 кг муки", false);
        assert_eq!(texts, vec!["5 кг ".to_string(), "муки".to_string()]);
    }

    #[test]
    fn wrap_segments_preserve_leading_spaces_only_when_requested() {
        let preserved = build_wrap_segments("  текст", true);
        let trimmed = build_wrap_segments("  текст", false);

        assert_eq!(preserved, vec!["  ".to_string(), "текст".to_string()]);
        assert_eq!(trimmed, vec!["текст".to_string()]);
    }

    #[test]
    fn oval_wrap_keeps_middle_lines_at_least_as_wide_as_outer_lines() {
        let targets = compute_shape_line_widths_for_test(4, 12.0, true, 0.45);
        let lines = wrap_text_with_targets(
            "mmmm mmmm mmmm mmmm mmmm mmmm",
            12,
            Some(targets.as_slice()),
            false,
            false,
        );

        assert_eq!(lines.len(), 4);
        let widths = lines
            .iter()
            .map(|line| line.chars().filter(|&ch| ch != '-').count())
            .collect::<Vec<_>>();

        assert!(widths[1] >= widths[0], "{widths:?}");
        assert!(widths[2] >= widths[3], "{widths:?}");
        assert!(widths[1] >= widths[3], "{widths:?}");
    }

    #[test]
    fn conservative_wrap_blocks_local_tree_when_moderate_trees_disabled() {
        let targets = [8usize, 10usize, 12usize, 10usize, 8usize];

        assert!(violates_tree_width_rule(
            2,
            Some(120),
            112.0,
            false,
            WrapSettings {
                base_units: 12,
                line_unit_targets: Some(&targets),
                line_width_targets_px: None,
                line_order_phases: None,
                strict_line_order: false,
                allow_moderate_trees: false,
                hanging_punctuation: false,
                hyphen_dicts: None,
                word_break_policy: Some(WordBreakPolicy::Minimal),
                preserve_edge_spaces: false,
            },
        ));
        assert!(!violates_tree_width_rule(
            2,
            Some(120),
            112.0,
            false,
            WrapSettings {
                base_units: 12,
                line_unit_targets: Some(&targets),
                line_width_targets_px: None,
                line_order_phases: None,
                strict_line_order: false,
                allow_moderate_trees: true,
                hanging_punctuation: false,
                hyphen_dicts: None,
                word_break_policy: Some(WordBreakPolicy::Minimal),
                preserve_edge_spaces: false,
            },
        ));
    }

    #[test]
    fn shape_wrap_uses_approximate_fallback_instead_of_single_line_paragraph() {
        let mut scoring = WrapScoringContext::fallback();
        let wrapped = wrap_text_with_targets_scored(
            "монолит коротко еще",
            WrapSettings {
                base_units: 8,
                line_unit_targets: Some(&[3, 8, 8]),
                line_width_targets_px: None,
                line_order_phases: None,
                strict_line_order: false,
                allow_moderate_trees: false,
                hanging_punctuation: false,
                hyphen_dicts: None,
                word_break_policy: Some(WordBreakPolicy::Minimal),
                preserve_edge_spaces: false,
            },
            &mut scoring,
        );

        assert!(wrapped.used_approximate_shape_fallback);
        assert!(wrapped.lines.len() >= 2, "{:?}", wrapped.lines);
        assert_eq!(wrapped.lines[0], "монолит");
    }

    #[test]
    fn approximate_fallback_keeps_visible_hyphen_for_soft_hyphen_split() {
        let mut scoring = WrapScoringContext::fallback();
        let wrapped = wrap_text_with_targets_scored(
            "мо\u{00AD}нолит коротко",
            WrapSettings {
                base_units: 8,
                line_unit_targets: Some(&[1, 8, 8]),
                line_width_targets_px: None,
                line_order_phases: None,
                strict_line_order: false,
                allow_moderate_trees: false,
                hanging_punctuation: false,
                hyphen_dicts: None,
                word_break_policy: Some(WordBreakPolicy::Minimal),
                preserve_edge_spaces: false,
            },
            &mut scoring,
        );

        assert!(wrapped.used_approximate_shape_fallback);
        assert_eq!(wrapped.lines[0], "мо-");
        assert_eq!(wrapped.lines[1], "нолит");
    }

    #[test]
    fn shape_wrap_does_not_create_middle_valley_with_equal_peak() {
        let targets = compute_shape_line_widths_for_test(6, 16.0, false, 0.5);
        let lines = wrap_text_with_targets(
            "господин рейн если вы сегодня не дадите мне разумного объяснения",
            16,
            Some(targets.as_slice()),
            true,
            false,
        );

        let widths = lines
            .iter()
            .map(|line| count_layout_units(line, true))
            .collect::<Vec<_>>();

        for idx in 1..widths.len() {
            let previous_target = targets.get(idx - 1).copied().unwrap_or(16);
            let current_target = targets.get(idx).copied().unwrap_or(16);
            if current_target > previous_target {
                assert!(widths[idx] >= widths[idx - 1], "{widths:?} vs {targets:?}");
            }
            if current_target < previous_target {
                assert!(widths[idx] <= widths[idx - 1], "{widths:?} vs {targets:?}");
            }
        }
    }
}
