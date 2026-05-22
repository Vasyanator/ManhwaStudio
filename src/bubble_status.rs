/*
File: src/bubble_status.rs

Purpose:
Shared bubble status rule model, evaluation helpers, JSON conversion, and border painting.

Main responsibilities:
- describe configurable bubble status border rules and logical expressions;
- provide the default preset matching the legacy status behavior;
- evaluate ordered rules against bubble-derived boolean facts;
- paint solid/dashed/dotted/wavy borders for bubble widgets.

Key structures:
- BubbleBorderKind
- BubbleBorderStyle
- BubbleStatusCondition
- BubbleStatusRule
- BubbleStatusContext

Key functions:
- default_bubble_status_rules()
- evaluate_bubble_status_rules()
- bubble_status_rules_to_value()
- bubble_status_rules_from_value()
- paint_bubble_status_border()

Notes:
- Colors are stored as `[u8; 4]` for stable JSON persistence independent of egui internals.
- Rule order matters: the first matching rule wins.
*/

use egui::{Color32, CornerRadius, Painter, Pos2, Rect, Stroke};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const DEFAULT_STATUS_BORDER_WIDTH: f32 = 2.0;
const DASH_LENGTH_PX: f32 = 10.0;
const DASH_GAP_PX: f32 = 6.0;
const DOT_SPACING_PX: f32 = 10.0;
const DOT_RADIUS_PX: f32 = 1.8;
const WAVE_STEP_PX: f32 = 4.0;
const WAVE_LENGTH_PX: f32 = 18.0;
const WAVE_AMPLITUDE_PX: f32 = 2.5;

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BubbleBorderKind {
    Solid,
    Dashed,
    Dotted,
    Wavy,
}

impl BubbleBorderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Solid => "Сплошная",
            Self::Dashed => "Пунктир",
            Self::Dotted => "Точки",
            Self::Wavy => "Волнистая",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct BubbleBorderStyle {
    pub kind: BubbleBorderKind,
    pub color: [u8; 4],
}

impl BubbleBorderStyle {
    pub fn new(kind: BubbleBorderKind, color: Color32) -> Self {
        Self {
            kind,
            color: [color.r(), color.g(), color.b(), color.a()],
        }
    }

    pub fn color32(self) -> Color32 {
        Color32::from_rgba_unmultiplied(self.color[0], self.color[1], self.color[2], self.color[3])
    }

    pub fn set_color32(&mut self, color: Color32) {
        self.color = [color.r(), color.g(), color.b(), color.a()];
    }
}

// "Filled" suffix is semantically meaningful here — these represent specific "filled" conditions
// (e.g. TranslationFilled ≠ Translation). Renaming would also break stored JSON serialization.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BubbleStatusField {
    TranslationFilled,
    OriginalFilled,
    CharacterFilled,
}

impl BubbleStatusField {
    pub fn label(self) -> &'static str {
        match self {
            Self::TranslationFilled => "Перевод заполнен",
            Self::OriginalFilled => "Оригинал заполнен",
            Self::CharacterFilled => "Персонаж заполнен",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "items", rename_all = "snake_case")]
pub enum BubbleStatusCondition {
    Empty,
    Field(BubbleStatusField),
    All(Vec<BubbleStatusCondition>),
    Any(Vec<BubbleStatusCondition>),
    Not(Box<BubbleStatusCondition>),
}

impl BubbleStatusCondition {
    pub fn summary(&self) -> String {
        match self {
            Self::Empty => "Пусто".to_string(),
            Self::Field(field) => field.label().to_string(),
            Self::All(items) => join_condition_summary(items, " И "),
            Self::Any(items) => join_condition_summary(items, " ИЛИ "),
            Self::Not(item) => format!("НЕ ({})", item.summary()),
        }
    }
}

fn join_condition_summary(items: &[BubbleStatusCondition], delimiter: &str) -> String {
    if items.is_empty() {
        return "пусто".to_string();
    }
    items
        .iter()
        .map(BubbleStatusCondition::summary)
        .collect::<Vec<_>>()
        .join(delimiter)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BubbleStatusRule {
    #[serde(default)]
    pub id: u64,
    pub condition: BubbleStatusCondition,
    pub border: BubbleBorderStyle,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BubbleStatusContext {
    pub translation_filled: bool,
    pub original_filled: bool,
    pub character_filled: bool,
}

impl BubbleStatusContext {
    pub fn value_for(self, field: BubbleStatusField) -> bool {
        match field {
            BubbleStatusField::TranslationFilled => self.translation_filled,
            BubbleStatusField::OriginalFilled => self.original_filled,
            BubbleStatusField::CharacterFilled => self.character_filled,
        }
    }
}

pub fn default_bubble_status_rules() -> Vec<BubbleStatusRule> {
    vec![
        BubbleStatusRule {
            id: 1,
            condition: BubbleStatusCondition::Not(Box::new(BubbleStatusCondition::Field(
                BubbleStatusField::TranslationFilled,
            ))),
            border: BubbleBorderStyle::new(BubbleBorderKind::Solid, Color32::from_rgb(210, 72, 72)),
        },
        BubbleStatusRule {
            id: 2,
            condition: BubbleStatusCondition::All(vec![
                BubbleStatusCondition::Field(BubbleStatusField::TranslationFilled),
                BubbleStatusCondition::Field(BubbleStatusField::CharacterFilled),
            ]),
            border: BubbleBorderStyle::new(
                BubbleBorderKind::Solid,
                Color32::from_rgb(82, 196, 104),
            ),
        },
    ]
}

pub fn default_bubble_status_rules_value() -> Value {
    bubble_status_rules_to_value(&default_bubble_status_rules())
}

pub fn bubble_status_rules_from_value(value: &Value) -> Option<Vec<BubbleStatusRule>> {
    let mut rules = serde_json::from_value::<Vec<BubbleStatusRule>>(value.clone()).ok()?;
    normalize_bubble_status_rules(&mut rules);
    Some(rules)
}

pub fn bubble_status_rules_to_value(rules: &[BubbleStatusRule]) -> Value {
    match serde_json::to_value(rules) {
        Ok(value) => value,
        Err(_) => json!([]),
    }
}

pub fn normalize_bubble_status_rules(rules: &mut Vec<BubbleStatusRule>) {
    if rules.is_empty() {
        *rules = default_bubble_status_rules();
    }

    let mut next_id = rules.iter().map(|rule| rule.id).max().unwrap_or(0) + 1;
    let mut used_ids = std::collections::HashSet::new();
    for rule in rules.iter_mut() {
        if rule.id == 0 || !used_ids.insert(rule.id) {
            rule.id = next_id;
            next_id += 1;
            used_ids.insert(rule.id);
        }
        normalize_condition(&mut rule.condition);
    }
}

fn normalize_condition(condition: &mut BubbleStatusCondition) {
    match condition {
        BubbleStatusCondition::Empty => {}
        BubbleStatusCondition::Field(_) => {}
        BubbleStatusCondition::All(items) | BubbleStatusCondition::Any(items) => {
            if items.is_empty() {
                items.push(BubbleStatusCondition::Empty);
            }
            for item in items.iter_mut() {
                normalize_condition(item);
            }
        }
        BubbleStatusCondition::Not(item) => normalize_condition(item),
    }
}

pub fn evaluate_bubble_status_rules(
    rules: &[BubbleStatusRule],
    ctx: BubbleStatusContext,
) -> Option<BubbleBorderStyle> {
    rules
        .iter()
        .find(|rule| evaluate_condition(&rule.condition, ctx))
        .map(|rule| rule.border)
}

fn evaluate_condition(condition: &BubbleStatusCondition, ctx: BubbleStatusContext) -> bool {
    match condition {
        BubbleStatusCondition::Empty => false,
        BubbleStatusCondition::Field(field) => ctx.value_for(*field),
        BubbleStatusCondition::All(items) => items.iter().all(|item| evaluate_condition(item, ctx)),
        BubbleStatusCondition::Any(items) => items.iter().any(|item| evaluate_condition(item, ctx)),
        BubbleStatusCondition::Not(item) => {
            if matches!(item.as_ref(), BubbleStatusCondition::Empty) {
                false
            } else {
                !evaluate_condition(item, ctx)
            }
        }
    }
}

pub fn paint_bubble_status_border(
    painter: &Painter,
    rect: Rect,
    corner_radius: CornerRadius,
    style: BubbleBorderStyle,
) {
    let color = style.color32();
    match style.kind {
        BubbleBorderKind::Solid => {
            painter.rect_stroke(
                rect,
                corner_radius,
                Stroke::new(DEFAULT_STATUS_BORDER_WIDTH, color),
                egui::StrokeKind::Inside,
            );
        }
        BubbleBorderKind::Dashed => paint_dashed_rect(painter, rect, color),
        BubbleBorderKind::Dotted => paint_dotted_rect(painter, rect, color),
        BubbleBorderKind::Wavy => paint_wavy_rect(painter, rect, color),
    }
}

fn paint_dashed_rect(painter: &Painter, rect: Rect, color: Color32) {
    paint_dashed_segment(
        painter,
        Pos2::new(rect.left(), rect.top()),
        Pos2::new(rect.right(), rect.top()),
        color,
    );
    paint_dashed_segment(
        painter,
        Pos2::new(rect.right(), rect.top()),
        Pos2::new(rect.right(), rect.bottom()),
        color,
    );
    paint_dashed_segment(
        painter,
        Pos2::new(rect.right(), rect.bottom()),
        Pos2::new(rect.left(), rect.bottom()),
        color,
    );
    paint_dashed_segment(
        painter,
        Pos2::new(rect.left(), rect.bottom()),
        Pos2::new(rect.left(), rect.top()),
        color,
    );
}

fn paint_dashed_segment(painter: &Painter, start: Pos2, end: Pos2, color: Color32) {
    let delta = end - start;
    let length = delta.length();
    if length <= 0.0 {
        return;
    }
    let dir = delta / length;
    let mut cursor = 0.0;
    while cursor < length {
        let dash_end = (cursor + DASH_LENGTH_PX).min(length);
        let p1 = start + dir * cursor;
        let p2 = start + dir * dash_end;
        painter.line_segment([p1, p2], Stroke::new(DEFAULT_STATUS_BORDER_WIDTH, color));
        cursor += DASH_LENGTH_PX + DASH_GAP_PX;
    }
}

fn paint_dotted_rect(painter: &Painter, rect: Rect, color: Color32) {
    paint_dotted_segment(
        painter,
        Pos2::new(rect.left(), rect.top()),
        Pos2::new(rect.right(), rect.top()),
        color,
    );
    paint_dotted_segment(
        painter,
        Pos2::new(rect.right(), rect.top()),
        Pos2::new(rect.right(), rect.bottom()),
        color,
    );
    paint_dotted_segment(
        painter,
        Pos2::new(rect.right(), rect.bottom()),
        Pos2::new(rect.left(), rect.bottom()),
        color,
    );
    paint_dotted_segment(
        painter,
        Pos2::new(rect.left(), rect.bottom()),
        Pos2::new(rect.left(), rect.top()),
        color,
    );
}

fn paint_dotted_segment(painter: &Painter, start: Pos2, end: Pos2, color: Color32) {
    let delta = end - start;
    let length = delta.length();
    if length <= 0.0 {
        return;
    }
    let dir = delta / length;
    let mut cursor = 0.0;
    while cursor <= length {
        let center = start + dir * cursor;
        painter.circle_filled(center, DOT_RADIUS_PX, color);
        cursor += DOT_SPACING_PX;
    }
}

fn paint_wavy_rect(painter: &Painter, rect: Rect, color: Color32) {
    paint_wavy_edge(
        painter,
        Pos2::new(rect.left(), rect.top()),
        Pos2::new(rect.right(), rect.top()),
        egui::vec2(0.0, -1.0),
        color,
    );
    paint_wavy_edge(
        painter,
        Pos2::new(rect.right(), rect.top()),
        Pos2::new(rect.right(), rect.bottom()),
        egui::vec2(1.0, 0.0),
        color,
    );
    paint_wavy_edge(
        painter,
        Pos2::new(rect.right(), rect.bottom()),
        Pos2::new(rect.left(), rect.bottom()),
        egui::vec2(0.0, 1.0),
        color,
    );
    paint_wavy_edge(
        painter,
        Pos2::new(rect.left(), rect.bottom()),
        Pos2::new(rect.left(), rect.top()),
        egui::vec2(-1.0, 0.0),
        color,
    );
}

fn paint_wavy_edge(painter: &Painter, start: Pos2, end: Pos2, normal: egui::Vec2, color: Color32) {
    let delta = end - start;
    let length = delta.length();
    if length <= 0.0 {
        return;
    }
    let dir = delta / length;
    let steps = ((length / WAVE_STEP_PX).ceil() as usize).max(2);
    let mut points = Vec::with_capacity(steps + 1);
    for step in 0..=steps {
        let t = step as f32 / steps as f32;
        let dist = t * length;
        let phase = dist / WAVE_LENGTH_PX * std::f32::consts::TAU;
        let offset = normal * phase.sin() * WAVE_AMPLITUDE_PX;
        points.push(start + dir * dist + offset);
    }
    painter.add(egui::Shape::line(
        points,
        Stroke::new(DEFAULT_STATUS_BORDER_WIDTH, color),
    ));
}
