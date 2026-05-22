#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

/*
FILE OVERVIEW: src/bin/test_text_shape.rs
Изолированный egui-тестер алгоритма переноса текста под shape-профиль.

Main responsibilities:
- держит отдельное окно для ввода текста и просмотра shape-раскладки;
- копирует алгоритм разбиения текста на блоки и DP-подбора переносов из typing/render;
- позволяет крутить форму (`квадратная`/`округлая`/`шестигранная`), максимальную ширину
  в символах и минимальную ширину shape-профиля в процентах.

Key structures:
- `ShapePreviewApp` — состояние UI и параметров теста.
- `TextShape` — тип shape-профиля для раскладки.
- `WrapBlock`, `LineBreakCandidate`, `WrapParagraphSolution`, `WrapMemoKey` — служебные сущности DP.

Key functions:
- `reshape_text_for_shape` — строит итоговый перенос текста под выбранную форму.
- `solve_wrap_paragraph_dp` — DP-решатель переносов с монотонным shape-профилем.
- `build_wrap_segments` / `build_wrap_blocks` — связывают короткие частицы и готовят блоки переноса.

Notes:
- Этот бинарник намеренно работает только в символьных единицах ширины, без измерения шрифта.
- Используется для отдельной доработки shape-алгоритма, не для финального рендера glyph-ов.
*/

use eframe::egui;
use egui::{TextEdit, TextStyle};
use hyphenation::{Hyphenator, Language, Load, Standard};
use std::collections::{HashMap, VecDeque};

const APP_TITLE: &str = "test_text_shape";
const DEFAULT_TEXT: &str = "ГОСПОДИН РЕЙН, ЕСЛИ ВЫ СЕГОДНЯ НЕ ДАДИТЕ МНЕ РАЗУМНОГО ОБЪЯСНЕНИЯ,";
const SOFT_HYPHEN: char = '\u{00AD}';

fn main() {
    let run_result = eframe::run_native(
        APP_TITLE,
        eframe::NativeOptions::default(),
        Box::new(|cc| Ok(Box::new(ShapePreviewApp::new(cc)))),
    );

    if let Err(err) = run_result {
        eprintln!("[{APP_TITLE}] failed to start: {err}");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextShape {
    Rectangle,
    Oval,
    Hexagon,
}

impl TextShape {
    fn label(self) -> &'static str {
        match self {
            Self::Rectangle => "Квадратная",
            Self::Oval => "Округлая",
            Self::Hexagon => "Шестигранная",
        }
    }
}

#[derive(Debug)]
struct ShapePreviewApp {
    source_text: String,
    shape: TextShape,
    max_width_chars: usize,
    min_width_percent: f32,
    hyphenation_dicts: HyphenationDictionaries,
}

impl ShapePreviewApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            source_text: DEFAULT_TEXT.to_string(),
            shape: TextShape::Oval,
            max_width_chars: 16,
            min_width_percent: 45.0,
            hyphenation_dicts: HyphenationDictionaries::new(),
        }
    }
}

impl eframe::App for ShapePreviewApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let preview_text = reshape_text_for_shape(
            self.source_text.as_str(),
            self.max_width_chars,
            self.shape,
            self.min_width_percent,
            &self.hyphenation_dicts,
        );
        let centered_preview = center_preview_lines(preview_text.as_str(), self.max_width_chars);

        egui::TopBottomPanel::bottom("controls_panel").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                egui::ComboBox::from_label("Форма")
                    .selected_text(self.shape.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.shape,
                            TextShape::Rectangle,
                            TextShape::Rectangle.label(),
                        );
                        ui.selectable_value(
                            &mut self.shape,
                            TextShape::Oval,
                            TextShape::Oval.label(),
                        );
                        ui.selectable_value(
                            &mut self.shape,
                            TextShape::Hexagon,
                            TextShape::Hexagon.label(),
                        );
                    });

                ui.label("Макс. ширина");
                ui.add(egui::DragValue::new(&mut self.max_width_chars).range(4..=80));

                ui.label("Мин. ширина %");
                ui.add(
                    egui::Slider::new(&mut self.min_width_percent, 10.0..=100.0)
                        .clamping(egui::SliderClamping::Always),
                );
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.columns(2, |columns| {
                columns[0].heading("Исходный текст");
                columns[0].add(
                    TextEdit::multiline(&mut self.source_text)
                        .desired_width(f32::INFINITY)
                        .desired_rows(26),
                );

                columns[1].heading("Shape-перенос");
                let mut preview_buffer = centered_preview;
                columns[1].add(
                    TextEdit::multiline(&mut preview_buffer)
                        .font(TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .desired_rows(26)
                        .interactive(false),
                );
            });
        });
    }
}

#[derive(Debug, Clone)]
struct WrapSegment {
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum WrapBreakKind {
    None,
    Space(String),
    Hyphen,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WrapBlock {
    text: String,
    break_kind: WrapBreakKind,
    unit_count: usize,
}

#[derive(Debug, Clone)]
struct LineBreakCandidate {
    consumed_blocks: usize,
    split_remainder: Option<WrapBlock>,
    line_text: String,
    line_units: usize,
}

#[derive(Debug, Clone)]
struct WrapParagraphSolution {
    lines: Vec<String>,
    score: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ShapeMonotonicPhase {
    None,
    Expanding,
    Contracting,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WrapMemoKey {
    remaining_blocks: Vec<WrapBlock>,
    line_idx: usize,
    prev_line_units: Option<usize>,
    must_not_expand: bool,
}

#[derive(Debug)]
struct HyphenationDictionaries {
    russian: Option<Standard>,
    english_us: Option<Standard>,
}

impl HyphenationDictionaries {
    fn new() -> Self {
        Self {
            russian: Standard::from_embedded(Language::Russian).ok(),
            english_us: Standard::from_embedded(Language::EnglishUS).ok(),
        }
    }

    fn breaks_for_word(&self, word: &str) -> Vec<usize> {
        let has_cyrillic = contains_cyrillic(word);
        let mut out = Vec::<usize>::new();

        if has_cyrillic {
            if let Some(dic) = self.russian.as_ref() {
                out = sanitize_breaks(word, dic.hyphenate(word).breaks);
            }
            if out.is_empty()
                && let Some(dic) = self.english_us.as_ref()
            {
                out = sanitize_breaks(word, dic.hyphenate(word).breaks);
            }
        } else {
            if let Some(dic) = self.english_us.as_ref() {
                out = sanitize_breaks(word, dic.hyphenate(word).breaks);
            }
            if out.is_empty()
                && let Some(dic) = self.russian.as_ref()
            {
                out = sanitize_breaks(word, dic.hyphenate(word).breaks);
            }
        }

        out
    }
}

fn reshape_text_for_shape(
    text: &str,
    base_width_chars: usize,
    shape: TextShape,
    min_width_percent: f32,
    dicts: &HyphenationDictionaries,
) -> String {
    let base_width_chars = base_width_chars.max(1);
    let min_ratio = (min_width_percent / 100.0).clamp(0.01, 1.0);
    let hyphenated_text = soft_hyphenate_overlong(text, dicts);

    if shape == TextShape::Rectangle {
        let base_lines = wrap_text_with_targets(hyphenated_text.as_str(), base_width_chars, None);
        let target_units = rectangle_target_units(base_lines.as_slice(), base_width_chars);
        let profile = vec![target_units; base_lines.len().max(1)];
        return wrap_text_with_targets(
            hyphenated_text.as_str(),
            base_width_chars,
            Some(profile.as_slice()),
        )
        .join("\n");
    }

    let mut prev_profile: Option<Vec<usize>> = None;
    let mut lines = wrap_text_with_targets(hyphenated_text.as_str(), base_width_chars, None);
    const MAX_PASSES: usize = 4;

    for _ in 0..MAX_PASSES {
        let profile =
            compute_shape_line_widths(lines.len(), base_width_chars as f32, shape, min_ratio)
                .into_iter()
                .map(|value| value.round().max(1.0) as usize)
                .collect::<Vec<_>>();
        if prev_profile.as_ref() == Some(&profile) {
            break;
        }
        prev_profile = Some(profile.clone());
        lines = wrap_text_with_targets(
            hyphenated_text.as_str(),
            base_width_chars,
            Some(profile.as_slice()),
        );
    }

    lines.join("\n")
}

fn wrap_text_with_targets(
    text: &str,
    base_units: usize,
    line_unit_targets: Option<&[usize]>,
) -> Vec<String> {
    let mut out = Vec::<String>::new();
    let mut global_line_idx = 0usize;

    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            out.push(String::new());
            global_line_idx += 1;
            continue;
        }

        let blocks = build_wrap_blocks(paragraph);
        let mut memo = HashMap::<WrapMemoKey, Option<WrapParagraphSolution>>::new();
        let best = solve_wrap_paragraph_dp(
            blocks,
            global_line_idx,
            None,
            false,
            base_units,
            line_unit_targets,
            &mut memo,
        );

        let mut lines = best.map(|solution| solution.lines).unwrap_or_default();
        if lines.is_empty() {
            lines.push(String::new());
        }
        global_line_idx = global_line_idx.saturating_add(lines.len());
        out.append(&mut lines);
    }

    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn solve_wrap_paragraph_dp(
    remaining_blocks: Vec<WrapBlock>,
    line_idx: usize,
    prev_line_units: Option<usize>,
    must_not_expand: bool,
    base_units: usize,
    line_unit_targets: Option<&[usize]>,
    memo: &mut HashMap<WrapMemoKey, Option<WrapParagraphSolution>>,
) -> Option<WrapParagraphSolution> {
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
        must_not_expand,
    };
    if let Some(cached) = memo.get(&memo_key) {
        return cached.clone();
    }

    let max_units = line_target_units(line_unit_targets, base_units, line_idx);
    let mut candidates = collect_line_break_candidates(remaining_blocks.as_slice(), max_units);
    if candidates.is_empty()
        && let Some(fallback) =
            build_emergency_break_candidate(remaining_blocks.as_slice(), max_units)
    {
        candidates.push(fallback);
    }

    let mut best: Option<WrapParagraphSolution> = None;
    for candidate in candidates {
        if let Some(prev_units) = prev_line_units {
            let phase = shape_monotonic_phase(line_unit_targets, base_units, line_idx);
            let disallowed_expansion = matches!(phase, ShapeMonotonicPhase::Contracting)
                || (matches!(phase, ShapeMonotonicPhase::None) && must_not_expand);
            if disallowed_expansion && candidate.line_units > prev_units {
                continue;
            }
        }

        let remaining = apply_line_break_candidate(remaining_blocks.as_slice(), &candidate);
        let next_must_not_expand = must_not_expand
            || shape_monotonic_phase(line_unit_targets, base_units, line_idx)
                == ShapeMonotonicPhase::Contracting
            || prev_line_units.is_some_and(|prev_units| candidate.line_units < prev_units);
        let Some(mut tail_solution) = solve_wrap_paragraph_dp(
            remaining,
            line_idx.saturating_add(1),
            Some(candidate.line_units),
            next_must_not_expand,
            base_units,
            line_unit_targets,
            memo,
        ) else {
            continue;
        };

        let score = compute_line_fit_penalty(
            line_unit_targets,
            base_units,
            line_idx,
            prev_line_units,
            must_not_expand,
            candidate.line_units,
        ) + tail_solution.score;

        let mut lines = Vec::with_capacity(tail_solution.lines.len().saturating_add(1));
        lines.push(candidate.line_text);
        lines.append(&mut tail_solution.lines);
        let solution = WrapParagraphSolution { lines, score };
        if best
            .as_ref()
            .is_none_or(|current| solution.score < current.score)
        {
            best = Some(solution);
        }
    }

    memo.insert(memo_key, best.clone());
    best
}

fn collect_line_break_candidates(
    blocks: &[WrapBlock],
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
        });
    }
    out
}

fn build_emergency_break_candidate(
    blocks: &[WrapBlock],
    max_units: usize,
) -> Option<LineBreakCandidate> {
    let block = blocks.first()?;
    let split_at = find_emergency_split_index(block.text.as_str(), max_units.max(1))?;
    let head = block.text[..split_at].to_string();
    let tail = block.text[split_at..].to_string();
    let line_units = count_layout_units(head.as_str());
    Some(LineBreakCandidate {
        consumed_blocks: 1,
        split_remainder: Some(WrapBlock {
            text: tail,
            break_kind: block.break_kind.clone(),
            unit_count: count_layout_units(&block.text[split_at..]),
        }),
        line_text: format!("{head}-"),
        line_units,
    })
}

fn apply_line_break_candidate(
    blocks: &[WrapBlock],
    candidate: &LineBreakCandidate,
) -> Vec<WrapBlock> {
    let mut remaining = blocks[candidate.consumed_blocks..].to_vec();
    if let Some(remainder) = candidate.split_remainder.as_ref() {
        remaining.insert(0, remainder.clone());
    }
    remaining
}

#[allow(clippy::too_many_arguments)]
fn compute_line_fit_penalty(
    line_unit_targets: Option<&[usize]>,
    base_units: usize,
    line_idx: usize,
    prev_line_units: Option<usize>,
    must_not_expand: bool,
    candidate_units: usize,
) -> f32 {
    let target_units = line_target_units(line_unit_targets, base_units, line_idx);
    let slack_units = target_units.saturating_sub(candidate_units) as f32;
    let overflow_units = candidate_units.saturating_sub(target_units) as f32;
    let mut penalty = slack_units * slack_units + overflow_units * overflow_units * 12.0;

    if let Some(prev_units) = prev_line_units {
        let phase = shape_monotonic_phase(line_unit_targets, base_units, line_idx);
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

fn shape_monotonic_phase(
    line_unit_targets: Option<&[usize]>,
    base_units: usize,
    line_idx: usize,
) -> ShapeMonotonicPhase {
    if line_idx == 0 {
        return ShapeMonotonicPhase::None;
    }

    let previous_target = line_target_units(line_unit_targets, base_units, line_idx - 1);
    let current_target = line_target_units(line_unit_targets, base_units, line_idx);
    if current_target > previous_target {
        ShapeMonotonicPhase::Expanding
    } else if current_target < previous_target {
        ShapeMonotonicPhase::Contracting
    } else {
        ShapeMonotonicPhase::None
    }
}

fn line_target_units(targets: Option<&[usize]>, base_units: usize, line_idx: usize) -> usize {
    match targets {
        Some(values) if !values.is_empty() => values
            .get(line_idx)
            .copied()
            .or_else(|| values.last().copied())
            .unwrap_or(base_units)
            .max(1),
        _ => base_units.max(1),
    }
}

fn compute_shape_line_widths(
    line_count: usize,
    base_width: f32,
    shape: TextShape,
    min_ratio: f32,
) -> Vec<f32> {
    if line_count == 0 {
        return Vec::new();
    }
    if line_count == 1 {
        return vec![base_width.max(1.0)];
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
            TextShape::Rectangle => 1.0,
            TextShape::Hexagon => 1.0 - (1.0 - min_ratio) * u,
            TextShape::Oval => min_ratio + (1.0 - min_ratio) * (1.0 - u * u).sqrt(),
        };
        widths.push((base_width * ratio).max(1.0));
    }
    widths
}

fn rectangle_target_units(lines: &[String], base_units: usize) -> usize {
    let mut total = 0usize;
    let mut count = 0usize;
    for line in lines {
        let sample = line.trim();
        if sample.is_empty() {
            continue;
        }
        total = total.saturating_add(count_layout_units(sample));
        count += 1;
    }

    if count <= 1 {
        return base_units.max(1);
    }

    let avg_units = ((total as f32) / (count as f32)).ceil() as usize;
    avg_units.clamp((base_units / 2).max(1), base_units.max(1))
}

fn build_wrap_segments(paragraph: &str) -> VecDeque<WrapSegment> {
    let tokens = tokenize_paragraph(paragraph);
    let mut out = VecDeque::<WrapSegment>::new();
    let mut idx = 0usize;

    while idx < tokens.len() && tokens[idx].chars().all(|ch| ch.is_whitespace()) {
        idx += 1;
    }

    let mut current_text = String::new();
    while idx < tokens.len() {
        let token = tokens[idx].as_str();
        if token.chars().all(|ch| ch.is_whitespace()) {
            idx += 1;
            continue;
        }

        current_text.push_str(token);
        let Some(space) = tokens.get(idx + 1) else {
            break;
        };
        if !space.chars().all(|ch| ch.is_whitespace()) {
            idx += 1;
            continue;
        }

        let Some(next_word) = tokens.get(idx + 2) else {
            current_text.push_str(space.as_str());
            break;
        };

        current_text.push_str(space.as_str());
        if should_keep_words_together(token, next_word.as_str()) {
            idx += 2;
            continue;
        }

        out.push_back(WrapSegment {
            text: std::mem::take(&mut current_text),
        });
        idx += 2;
    }

    if !current_text.is_empty() {
        out.push_back(WrapSegment { text: current_text });
    }

    out
}

fn build_wrap_blocks(paragraph: &str) -> Vec<WrapBlock> {
    let segments = build_wrap_segments(paragraph);
    let mut out = Vec::<WrapBlock>::new();

    for segment in segments {
        let trimmed_text = segment.text.trim_end_matches(char::is_whitespace);
        if trimmed_text.is_empty() {
            continue;
        }

        let trailing_ws = segment
            .text
            .chars()
            .rev()
            .take_while(|ch| ch.is_whitespace())
            .count();
        let separator = if trailing_ws == 0 {
            None
        } else {
            Some(" ".repeat(trailing_ws))
        };

        let parts = trimmed_text
            .split(SOFT_HYPHEN)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        for (idx, part) in parts.iter().enumerate() {
            let break_kind = if idx + 1 < parts.len() {
                WrapBreakKind::Hyphen
            } else {
                separator.as_ref().map_or(WrapBreakKind::None, |space| {
                    WrapBreakKind::Space(space.clone())
                })
            };
            out.push(WrapBlock {
                text: (*part).to_string(),
                break_kind,
                unit_count: count_layout_units(part),
            });
        }
    }

    out
}

fn build_line_text_and_units(blocks: &[WrapBlock], wraps_here: bool) -> (String, usize) {
    let mut line = String::new();
    let mut units = 0usize;

    for (idx, block) in blocks.iter().enumerate() {
        line.push_str(block.text.as_str());
        units = units.saturating_add(block.unit_count);
        let is_last = idx + 1 == blocks.len();
        if !is_last {
            if let WrapBreakKind::Space(space) = &block.break_kind {
                line.push_str(space.as_str());
                units = units.saturating_add(space.chars().count());
            }
        } else if wraps_here && matches!(block.break_kind, WrapBreakKind::Hyphen) {
            line.push('-');
        }
    }

    (line, units)
}

fn count_layout_units(text: &str) -> usize {
    text.chars()
        .filter(|&ch| ch != '\n' && ch != '\r' && ch != SOFT_HYPHEN)
        .count()
}

fn tokenize_paragraph(paragraph: &str) -> VecDeque<String> {
    let mut tokens = VecDeque::<String>::new();
    let mut start = 0usize;
    let mut mode_ws: Option<bool> = None;

    for (idx, ch) in paragraph.char_indices() {
        let is_ws = ch.is_whitespace();
        match mode_ws {
            None => mode_ws = Some(is_ws),
            Some(prev) if prev != is_ws => {
                tokens.push_back(paragraph[start..idx].to_string());
                start = idx;
                mode_ws = Some(is_ws);
            }
            _ => {}
        }
    }

    if start < paragraph.len() {
        tokens.push_back(paragraph[start..].to_string());
    }

    tokens
}

fn should_keep_words_together(left_token: &str, right_token: &str) -> bool {
    let left = normalize_binding_token(left_token);
    let right = normalize_binding_token(right_token);
    if left.is_empty() || right.is_empty() {
        return false;
    }

    is_nonbreaking_prefix_word(left.as_str())
        || is_nonbreaking_suffix_particle(right.as_str())
        || (left.chars().count() == 1 && left.chars().all(char::is_alphabetic))
}

fn normalize_binding_token(token: &str) -> String {
    token
        .trim_matches(|ch: char| !ch.is_alphabetic())
        .to_lowercase()
}

fn is_nonbreaking_prefix_word(word: &str) -> bool {
    matches!(
        word,
        "не" | "ни"
            | "без"
            | "для"
            | "при"
            | "про"
            | "через"
            | "перед"
            | "пред"
            | "в"
            | "во"
            | "к"
            | "ко"
            | "с"
            | "со"
            | "у"
            | "о"
            | "об"
            | "обо"
            | "от"
            | "до"
            | "по"
            | "за"
            | "подо"
            | "из"
            | "изо"
            | "на"
            | "над"
            | "под"
    )
}

fn is_nonbreaking_suffix_particle(word: &str) -> bool {
    matches!(word, "же" | "ли" | "ль" | "бы" | "б" | "ка" | "де" | "то")
}

fn find_emergency_split_index(text: &str, max_units: usize) -> Option<usize> {
    let mut units = 0usize;
    let mut split_at = None;
    for (idx, ch) in text.char_indices() {
        if ch != SOFT_HYPHEN {
            units = units.saturating_add(1);
        }
        if units > max_units {
            break;
        }

        let next_idx = idx + ch.len_utf8();
        if next_idx >= text.len() {
            break;
        }

        let left = text[..next_idx].chars().next_back();
        let right = text[next_idx..].chars().next();
        if matches!((left, right), (Some(l), Some(r)) if l.is_alphabetic() && r.is_alphabetic())
            && count_alpha_chars(&text[..next_idx]) >= 2
            && count_alpha_chars(&text[next_idx..]) >= 2
        {
            split_at = Some(next_idx);
        }
    }
    split_at
}

fn count_alpha_chars(text: &str) -> usize {
    text.chars()
        .filter(|ch| ch.is_alphabetic() && *ch != SOFT_HYPHEN)
        .count()
}

fn center_preview_lines(text: &str, width: usize) -> String {
    text.lines()
        .map(|line| {
            let line_width = count_layout_units(line);
            let pad = width.saturating_sub(line_width) / 2;
            format!("{}{}", " ".repeat(pad), line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn soft_hyphenate_overlong(text: &str, dicts: &HyphenationDictionaries) -> String {
    let ranges = find_word_ranges(text);
    if ranges.is_empty() {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len() + text.len() / 8);
    let mut tail_start = 0usize;
    for (start, end) in ranges {
        out.push_str(&text[tail_start..start]);
        let word = &text[start..end];
        let replacement =
            maybe_soft_hyphenate_word(word, dicts).unwrap_or_else(|| word.to_string());
        out.push_str(replacement.as_str());
        tail_start = end;
    }
    out.push_str(&text[tail_start..]);
    out
}

fn maybe_soft_hyphenate_word(word: &str, dicts: &HyphenationDictionaries) -> Option<String> {
    if word.chars().count() < 4 {
        return None;
    }
    if word.contains("://")
        || word.contains('@')
        || word.contains('-')
        || word.contains(SOFT_HYPHEN)
    {
        return None;
    }

    let breaks = dicts.breaks_for_word(word);
    if breaks.is_empty() {
        return None;
    }

    Some(insert_soft_hyphens(word, breaks.as_slice()))
}

fn find_word_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::<(usize, usize)>::new();
    let mut run_start: Option<usize> = None;
    let mut run_len_chars = 0usize;

    for (idx, ch) in text.char_indices() {
        if is_word_char(ch) {
            if run_start.is_none() {
                run_start = Some(idx);
                run_len_chars = 0;
            }
            run_len_chars += 1;
            continue;
        }

        if let Some(start) = run_start.take()
            && run_len_chars >= 4
        {
            ranges.push((start, idx));
        }
        run_len_chars = 0;
    }

    if let Some(start) = run_start
        && run_len_chars >= 4
    {
        ranges.push((start, text.len()));
    }

    ranges
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn contains_cyrillic(word: &str) -> bool {
    word.chars().any(|ch| {
        let cp = ch as u32;
        matches!(cp, 0x0400..=0x052F | 0x2DE0..=0x2DFF | 0xA640..=0xA69F)
    })
}

fn sanitize_breaks(word: &str, mut breaks: Vec<usize>) -> Vec<usize> {
    breaks.retain(|&idx| {
        idx > 0
            && idx < word.len()
            && word.is_char_boundary(idx)
            && is_safe_boundary_for_dictionary_at(word, idx)
    });
    breaks.sort_unstable();
    breaks.dedup();
    breaks
}

fn is_safe_boundary_for_dictionary_at(word: &str, idx: usize) -> bool {
    if idx == 0 || idx >= word.len() || !word.is_char_boundary(idx) {
        return false;
    }
    let left = word[..idx].chars().next_back();
    let right = word[idx..].chars().next();
    is_safe_boundary_for_dictionary(left, right)
}

fn is_safe_boundary_for_dictionary(left: Option<char>, right: Option<char>) -> bool {
    let (Some(left), Some(right)) = (left, right) else {
        return false;
    };
    if matches!(left, 'ь' | 'Ь' | 'ъ' | 'Ъ') || matches!(right, 'ь' | 'Ь' | 'ъ' | 'Ъ') {
        return false;
    }
    true
}

fn insert_soft_hyphens(word: &str, breaks: &[usize]) -> String {
    let mut out = String::with_capacity(word.len() + breaks.len() * SOFT_HYPHEN.len_utf8());
    let mut tail_start = 0usize;
    for &idx in breaks {
        if idx <= tail_start || idx >= word.len() || !word.is_char_boundary(idx) {
            continue;
        }
        out.push_str(&word[tail_start..idx]);
        out.push(SOFT_HYPHEN);
        tail_start = idx;
    }
    out.push_str(&word[tail_start..]);
    out
}
