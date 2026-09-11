/*
File: crates/ms-config/src/bubble_status.rs

Purpose:
GUI-free half of the bubble status rule model: the rule/condition/style types, the
default preset, JSON conversion, normalization and evaluation. It lives here because
`user_config_defaults()` embeds the default preset under `Canvas.bubble_status_rules`,
so the model has to sit at or below the config layer.

Main responsibilities:
- describe configurable bubble status border rules and logical expressions;
- provide the default preset matching the legacy status behavior;
- evaluate ordered rules against bubble-derived boolean facts.

Key structures:
- BubbleBorderKind
- BubbleBorderStyle
- BubbleStatusCondition
- BubbleStatusRule
- BubbleStatusContext

Key functions:
- default_bubble_status_rules()
- default_bubble_status_rules_value()
- evaluate_bubble_status_rules()
- bubble_status_rules_to_value()
- bubble_status_rules_from_value()

Notes:
- Colors are stored as `[u8; 4]` for stable JSON persistence independent of egui internals.
- Rule order matters: the first matching rule wins.
- Border PAINTING stays in the binary (`crates/ms-widgets/src/bubble_status.rs`), which needs `egui::Painter`;
  that module re-exports everything declared here, so `crate::bubble_status::...` call sites
  see one module as before.
*/

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};


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
            Self::Solid => t!("bubble_status.border_solid"),
            Self::Dashed => t!("bubble_status.border_dashed"),
            Self::Dotted => t!("bubble_status.border_dotted"),
            Self::Wavy => t!("bubble_status.border_wavy"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub struct BubbleBorderStyle {
    pub kind: BubbleBorderKind,
    pub color: [u8; 4],
}

impl BubbleBorderStyle {
    /// Builds a style from a non-premultiplied `[r, g, b, a]` quadruple.
    ///
    /// The raw quadruple - not an egui colour - is the storage form, because this
    /// model is persisted in `user_config.json` and must stay GUI-free. The binary's
    /// paint layer adds the egui flavour through the `BubbleBorderPaintColor`
    /// extension trait in `crates/ms-widgets/src/bubble_status.rs`.
    #[must_use]
    pub fn new(kind: BubbleBorderKind, color: [u8; 4]) -> Self {
        Self { kind, color }
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
            Self::TranslationFilled => t!("bubble_status.field_translation_filled"),
            Self::OriginalFilled => t!("bubble_status.field_original_filled"),
            Self::CharacterFilled => t!("bubble_status.field_character_filled"),
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
            Self::Empty => t!("bubble_status.summary_empty").to_string(),
            Self::Field(field) => field.label().to_string(),
            Self::All(items) => join_condition_summary(items, t!("bubble_status.summary_and")),
            Self::Any(items) => join_condition_summary(items, t!("bubble_status.summary_or")),
            Self::Not(item) => tf!("bubble_status.summary_not", item = item.summary()),
        }
    }
}

fn join_condition_summary(items: &[BubbleStatusCondition], delimiter: &str) -> String {
    if items.is_empty() {
        return t!("bubble_status.summary_empty_list").to_string();
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
            border: BubbleBorderStyle::new(BubbleBorderKind::Solid, [210, 72, 72, 255]),
        },
        BubbleStatusRule {
            id: 2,
            condition: BubbleStatusCondition::All(vec![
                BubbleStatusCondition::Field(BubbleStatusField::TranslationFilled),
                BubbleStatusCondition::Field(BubbleStatusField::CharacterFilled),
            ]),
            border: BubbleBorderStyle::new(BubbleBorderKind::Solid, [82, 196, 104, 255]),
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

