/*
File: crates/ms-custom-fonts/src/recipe.rs

Purpose:
The custom font RECIPE model — the only definition of a custom font (the compiled face is a
disposable derivative) — with its canonical JSON form and its validation.

Key structures:
- `CustomFontRecipe` / `CustomFontKind`: kind-tagged recipe (`Hybrid` today; a per-character
  `Combined` kind is planned and must arrive as a new variant, never as a flag on `Hybrid`).
- `HybridParams` / `HybridSource` / `MissingGlyphPolicy`: the hybrid kind's parameters.
- `RecipeError`: typed parse/validation failure; `UnknownKind` is distinct so stores can keep a
  recipe of a kind this build does not know verbatim.

Key functions:
- `CustomFontRecipe::from_json` / `to_json`: canonical JSON
  `{"kind":"hybrid","sources":[{"identity":..,"weight":..,"missing":null|"disable"|"no_influence"}],
  "missing_default":"disable"|"no_influence"}`.
- `CustomFontRecipe::validate`: 2..=4 sources, finite weight > 0, non-empty identities, no
  duplicate identity under the CALLER's normalization rule.

Notes:
Hand-written over `serde_json::Value` (no serde derive): the error text and the canonical
output (field order, shortest round-trip weight decimal) are part of the contract. Unknown
extra fields are ignored on read (forward compatibility) and never written.
*/

use std::collections::HashSet;

use serde_json::{Map, Number, Value};

/// Fewest sources a hybrid recipe may have (decision D3).
pub const MIN_HYBRID_SOURCES: usize = 2;
/// Most sources a hybrid recipe may have (decision D3).
pub const MAX_HYBRID_SOURCES: usize = 4;

/// JSON tag of the hybrid kind.
const KIND_HYBRID: &str = "hybrid";

/// What happens to a character that a source lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MissingGlyphPolicy {
    /// The character is left out of the custom font entirely (absent from its cmap).
    Disable,
    /// The source lacking the character has no influence on it: the character is blended from
    /// the sources that have it, with their weights renormalized.
    NoInfluence,
}

impl MissingGlyphPolicy {
    /// Canonical JSON spelling (`"disable"` / `"no_influence"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::NoInfluence => "no_influence",
        }
    }

    /// Parses the canonical JSON spelling; `None` for anything else.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "disable" => Some(Self::Disable),
            "no_influence" => Some(Self::NoInfluence),
            _ => None,
        }
    }
}

/// One source font of a hybrid: a REFERENCE to a real font by identity, never its bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct HybridSource {
    /// The typing model's font identity (PostScript-name based); compared by the caller's
    /// normalization rule in `validate`, matched exactly against `SourceFont::identity`.
    pub identity: String,
    /// Relative weight (any positive finite scale; weights are normalized by their sum).
    pub weight: f32,
    /// This source's own missing-glyph policy, or `None` to follow the hybrid's default.
    pub missing_override: Option<MissingGlyphPolicy>,
}

impl HybridSource {
    /// The policy that applies to a character this source lacks.
    #[must_use]
    pub fn effective_missing(&self, default: MissingGlyphPolicy) -> MissingGlyphPolicy {
        self.missing_override.unwrap_or(default)
    }
}

/// Parameters of the hybrid kind: weighted sources plus the default missing-glyph policy.
#[derive(Debug, Clone, PartialEq)]
pub struct HybridParams {
    /// Sources in the user's order (the order is part of the recipe key).
    pub sources: Vec<HybridSource>,
    /// Policy of every source without its own override.
    pub missing_default: MissingGlyphPolicy,
}

/// The kind of a custom font and its kind-specific parameters.
#[derive(Debug, Clone, PartialEq)]
pub enum CustomFontKind {
    /// Weighted outline average of 2..=4 real fonts.
    Hybrid(HybridParams),
}

impl CustomFontKind {
    /// JSON `kind` tag of this variant.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Hybrid(_) => KIND_HYBRID,
        }
    }
}

/// A custom font definition: references to real fonts plus the kind's parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomFontRecipe {
    /// The kind and its parameters.
    pub kind: CustomFontKind,
}

/// Why a recipe could not be read or is not valid.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RecipeError {
    /// The JSON does not have the recipe shape (missing/mistyped field); the text names it.
    #[error("malformed custom font recipe: {0}")]
    Malformed(String),
    /// The `kind` tag is not one this build implements; the recipe should be kept verbatim.
    #[error("unknown custom font kind '{0}'")]
    UnknownKind(String),
    /// Fewer sources than `MIN_HYBRID_SOURCES`.
    #[error("a hybrid font needs at least {min} sources, the recipe has {count}")]
    TooFewSources {
        /// Sources in the recipe.
        count: usize,
        /// Required minimum.
        min: usize,
    },
    /// More sources than `MAX_HYBRID_SOURCES`.
    #[error("a hybrid font takes at most {max} sources, the recipe has {count}")]
    TooManySources {
        /// Sources in the recipe.
        count: usize,
        /// Allowed maximum.
        max: usize,
    },
    /// A source has an empty identity.
    #[error("source #{index} of the recipe has an empty font identity")]
    EmptyIdentity {
        /// Zero-based position of the source.
        index: usize,
    },
    /// A weight is not finite or not greater than zero.
    #[error("source '{identity}' has an invalid weight {weight} (must be finite and > 0)")]
    InvalidWeight {
        /// The source's identity.
        identity: String,
        /// The offending weight.
        weight: f32,
    },
    /// Two sources name the same font under the caller's identity normalization.
    #[error("font '{identity}' is used more than once in the recipe")]
    DuplicateSource {
        /// The identity of the second occurrence, as written.
        identity: String,
    },
}

impl CustomFontRecipe {
    /// Reads a recipe from its JSON form.
    ///
    /// Only the shape is checked here (field presence and types); bounds and duplicates are
    /// `validate`'s job, so a stored recipe that became invalid can still be loaded and shown.
    ///
    /// # Errors
    /// `RecipeError::UnknownKind` when `kind` is a string this build does not implement (keep
    /// the JSON verbatim), `RecipeError::Malformed` for any other shape problem.
    pub fn from_json(value: &Value) -> Result<Self, RecipeError> {
        let object = value
            .as_object()
            .ok_or_else(|| RecipeError::Malformed("the recipe is not a JSON object".to_owned()))?;
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| RecipeError::Malformed("field 'kind' is missing or not a string".to_owned()))?;
        match kind {
            KIND_HYBRID => parse_hybrid(object).map(|params| Self { kind: CustomFontKind::Hybrid(params) }),
            other => Err(RecipeError::UnknownKind(other.to_owned())),
        }
    }

    /// The canonical JSON form: fixed field order, weights written as the shortest decimal
    /// that reads back to the same `f32`, `missing` always present (`null` = follow default).
    ///
    /// A non-finite weight (only constructible in code, never read from JSON) is written as
    /// `null`, which `from_json` rejects — `validate` reports it first.
    #[must_use]
    pub fn to_json(&self) -> Value {
        match &self.kind {
            CustomFontKind::Hybrid(params) => {
                let sources: Vec<Value> = params
                    .sources
                    .iter()
                    .map(|source| {
                        let mut entry = Map::new();
                        entry.insert("identity".to_owned(), Value::String(source.identity.clone()));
                        entry.insert("weight".to_owned(), weight_to_json(source.weight));
                        entry.insert(
                            "missing".to_owned(),
                            source
                                .missing_override
                                .map_or(Value::Null, |policy| Value::String(policy.as_str().to_owned())),
                        );
                        Value::Object(entry)
                    })
                    .collect();
                let mut root = Map::new();
                root.insert("kind".to_owned(), Value::String(KIND_HYBRID.to_owned()));
                root.insert("sources".to_owned(), Value::Array(sources));
                root.insert(
                    "missing_default".to_owned(),
                    Value::String(params.missing_default.as_str().to_owned()),
                );
                Value::Object(root)
            }
        }
    }

    /// Checks the recipe's invariants.
    ///
    /// `normalize_identity` is the caller's identity normalization (the typing model's rule);
    /// two sources whose normalized identities are equal are duplicates.
    ///
    /// # Errors
    /// `TooFewSources` / `TooManySources` (outside `2..=4`), `EmptyIdentity`, `InvalidWeight`
    /// (not finite or `<= 0`), `DuplicateSource`. The first violation in source order wins.
    pub fn validate(&self, normalize_identity: &dyn Fn(&str) -> String) -> Result<(), RecipeError> {
        match &self.kind {
            CustomFontKind::Hybrid(params) => validate_hybrid(params, normalize_identity),
        }
    }

    /// Identities of every font the recipe references, in recipe order.
    #[must_use]
    pub fn source_identities(&self) -> Vec<&str> {
        match &self.kind {
            CustomFontKind::Hybrid(params) => params.sources.iter().map(|source| source.identity.as_str()).collect(),
        }
    }
}

/// Validates the hybrid kind (see `CustomFontRecipe::validate`).
fn validate_hybrid(params: &HybridParams, normalize_identity: &dyn Fn(&str) -> String) -> Result<(), RecipeError> {
    let count = params.sources.len();
    if count < MIN_HYBRID_SOURCES {
        return Err(RecipeError::TooFewSources { count, min: MIN_HYBRID_SOURCES });
    }
    if count > MAX_HYBRID_SOURCES {
        return Err(RecipeError::TooManySources { count, max: MAX_HYBRID_SOURCES });
    }
    let mut seen = HashSet::with_capacity(count);
    for (index, source) in params.sources.iter().enumerate() {
        if source.identity.trim().is_empty() {
            return Err(RecipeError::EmptyIdentity { index });
        }
        if !source.weight.is_finite() || source.weight <= 0.0 {
            return Err(RecipeError::InvalidWeight { identity: source.identity.clone(), weight: source.weight });
        }
        if !seen.insert(normalize_identity(&source.identity)) {
            return Err(RecipeError::DuplicateSource { identity: source.identity.clone() });
        }
    }
    Ok(())
}

/// Reads the hybrid kind's fields from the recipe object.
fn parse_hybrid(object: &Map<String, Value>) -> Result<HybridParams, RecipeError> {
    let raw_sources = object
        .get("sources")
        .and_then(Value::as_array)
        .ok_or_else(|| RecipeError::Malformed("field 'sources' is missing or not an array".to_owned()))?;
    let sources = raw_sources
        .iter()
        .enumerate()
        .map(|(index, raw)| parse_source(index, raw))
        .collect::<Result<Vec<_>, _>>()?;
    let missing_default = object
        .get("missing_default")
        .and_then(Value::as_str)
        .and_then(MissingGlyphPolicy::parse)
        .ok_or_else(|| {
            RecipeError::Malformed("field 'missing_default' is missing or not 'disable' / 'no_influence'".to_owned())
        })?;
    Ok(HybridParams { sources, missing_default })
}

/// Reads one `sources[index]` entry.
fn parse_source(index: usize, raw: &Value) -> Result<HybridSource, RecipeError> {
    let object = raw
        .as_object()
        .ok_or_else(|| RecipeError::Malformed(format!("sources[{index}] is not an object")))?;
    let identity = object
        .get("identity")
        .and_then(Value::as_str)
        .ok_or_else(|| RecipeError::Malformed(format!("sources[{index}].identity is missing or not a string")))?;
    let weight = object
        .get("weight")
        .and_then(Value::as_f64)
        .ok_or_else(|| RecipeError::Malformed(format!("sources[{index}].weight is missing or not a number")))?;
    let missing_override = match object.get("missing") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw_policy)) => Some(MissingGlyphPolicy::parse(raw_policy).ok_or_else(|| {
            RecipeError::Malformed(format!("sources[{index}].missing has an unknown value '{raw_policy}'"))
        })?),
        Some(_) => return Err(RecipeError::Malformed(format!("sources[{index}].missing is not a string or null"))),
    };
    Ok(HybridSource { identity: identity.to_owned(), weight: weight_from_json(weight), missing_override })
}

/// Converts a JSON number to the model's `f32` weight.
#[expect(
    clippy::cast_possible_truncation,
    reason = "weights are f32 by contract; nearest-f32 rounding is the intended conversion and an out-of-range value becomes infinite, which `validate` rejects"
)]
fn weight_from_json(weight: f64) -> f32 {
    weight as f32
}

/// Writes a weight as the shortest decimal that reads back to the same `f32` (so `0.6f32` is
/// stored as `0.6`, not `0.6000000238418579`). Non-finite weights become `null`.
fn weight_to_json(weight: f32) -> Value {
    // `f32`'s `Display` is the shortest round-trip representation; parsing it as `f64` keeps
    // that decimal, and reading it back with `weight_from_json` yields the same `f32`.
    format!("{weight}")
        .parse::<f64>()
        .ok()
        .and_then(Number::from_f64)
        .map_or(Value::Null, Value::Number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn exact(identity: &str) -> String {
        identity.to_owned()
    }

    fn sample() -> CustomFontRecipe {
        CustomFontRecipe {
            kind: CustomFontKind::Hybrid(HybridParams {
                sources: vec![
                    HybridSource { identity: "FontA".to_owned(), weight: 0.6, missing_override: None },
                    HybridSource {
                        identity: "FontB".to_owned(),
                        weight: 0.4,
                        missing_override: Some(MissingGlyphPolicy::Disable),
                    },
                ],
                missing_default: MissingGlyphPolicy::NoInfluence,
            }),
        }
    }

    #[test]
    fn json_round_trip_is_canonical() {
        let recipe = sample();
        let value = recipe.to_json();
        assert_eq!(
            value,
            json!({
                "kind": "hybrid",
                "sources": [
                    {"identity": "FontA", "weight": 0.6, "missing": null},
                    {"identity": "FontB", "weight": 0.4, "missing": "disable"}
                ],
                "missing_default": "no_influence"
            })
        );
        let back = CustomFontRecipe::from_json(&value).expect("canonical JSON reads back");
        assert_eq!(back, recipe);
        assert_eq!(back.to_json(), value);
    }

    #[test]
    fn absent_missing_reads_as_follow_default_and_extra_fields_are_ignored() {
        let value = json!({
            "kind": "hybrid",
            "sources": [{"identity": "A", "weight": 1}, {"identity": "B", "weight": 2, "future": true}],
            "missing_default": "disable",
            "future_field": 1
        });
        let recipe = CustomFontRecipe::from_json(&value).expect("readable");
        let CustomFontKind::Hybrid(params) = &recipe.kind;
        assert!(params.sources.iter().all(|source| source.missing_override.is_none()));
        assert_eq!(recipe.source_identities(), vec!["A", "B"]);
    }

    #[test]
    fn unknown_kind_is_distinct_from_malformed() {
        let unknown = CustomFontRecipe::from_json(&json!({"kind": "combined", "anything": 1}));
        assert_eq!(unknown, Err(RecipeError::UnknownKind("combined".to_owned())));
        assert!(matches!(CustomFontRecipe::from_json(&json!({"sources": []})), Err(RecipeError::Malformed(_))));
        assert!(matches!(CustomFontRecipe::from_json(&json!([1, 2])), Err(RecipeError::Malformed(_))));
        let bad_policy = json!({
            "kind": "hybrid",
            "sources": [{"identity": "A", "weight": 1, "missing": "sometimes"}],
            "missing_default": "disable"
        });
        assert!(matches!(CustomFontRecipe::from_json(&bad_policy), Err(RecipeError::Malformed(_))));
        let bad_weight = json!({"kind": "hybrid", "sources": [{"identity": "A", "weight": "1"}], "missing_default": "disable"});
        assert!(matches!(CustomFontRecipe::from_json(&bad_weight), Err(RecipeError::Malformed(_))));
    }

    #[test]
    fn validate_enforces_bounds_weights_and_duplicates() {
        assert_eq!(sample().validate(&exact), Ok(()));

        let with_sources = |sources: Vec<HybridSource>| CustomFontRecipe {
            kind: CustomFontKind::Hybrid(HybridParams { sources, missing_default: MissingGlyphPolicy::Disable }),
        };
        let source = |identity: &str, weight: f32| HybridSource {
            identity: identity.to_owned(),
            weight,
            missing_override: None,
        };

        assert_eq!(
            with_sources(vec![source("A", 1.0)]).validate(&exact),
            Err(RecipeError::TooFewSources { count: 1, min: 2 })
        );
        let five = (0..5).map(|index| source(&format!("F{index}"), 1.0)).collect();
        assert_eq!(with_sources(five).validate(&exact), Err(RecipeError::TooManySources { count: 5, max: 4 }));
        let four = (0..4).map(|index| source(&format!("F{index}"), 1.0)).collect();
        assert_eq!(with_sources(four).validate(&exact), Ok(()));

        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(matches!(
                with_sources(vec![source("A", 1.0), source("B", bad)]).validate(&exact),
                Err(RecipeError::InvalidWeight { .. })
            ));
        }
        assert_eq!(
            with_sources(vec![source("A", 1.0), source("  ", 1.0)]).validate(&exact),
            Err(RecipeError::EmptyIdentity { index: 1 })
        );
        // Duplicates are judged by the CALLER's normalization, not by exact spelling.
        let pair = with_sources(vec![source("Font-A", 1.0), source("font-a", 1.0)]);
        assert_eq!(pair.validate(&exact), Ok(()));
        assert_eq!(
            pair.validate(&|identity: &str| identity.to_lowercase()),
            Err(RecipeError::DuplicateSource { identity: "font-a".to_owned() })
        );
    }
}
