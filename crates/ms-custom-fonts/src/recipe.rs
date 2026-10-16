/*
File: crates/ms-custom-fonts/src/recipe.rs

Purpose:
The custom font RECIPE model — the only definition of a custom font (the compiled face is a
disposable derivative) — with its canonical JSON form and its validation.

Key structures:
- `CustomFontRecipe` / `CustomFontKind`: kind-tagged recipe (`Hybrid` today; a per-character
  `Combined` kind is planned and must arrive as a new variant, never as a flag on `Hybrid`).
- `HybridParams` / `HybridSource` / `MissingGlyphPolicy`: the hybrid kind's parameters.
- `BlendAlgorithm`: how the hybrid's glyph outlines are produced (automatic path choice,
  forced distance field, no blending, stroke skeleton, parameter transfer onto a base font).
- `PropertyWeights` / `EffectiveWeights`: optional separate per-source weights for glyph
  shapes, spacing and kerning, and the one owner of "which weight applies where".
- `RecipeError`: typed parse/validation failure; `UnknownKind` and `UnknownAlgorithm` are
  distinct so stores can keep a recipe this build does not understand verbatim.

Key functions:
- `CustomFontRecipe::from_json` / `to_json`: canonical JSON. A BASELINE hybrid (algorithm
  `Auto`, no property weights) is written exactly as before the algorithm choice existed:
  `{"kind":"hybrid","sources":[{"identity":..,"weight":..,"missing":null|"disable"|"no_influence"}],
  "missing_default":"disable"|"no_influence"}`. Every other hybrid is written under the kind tag
  `hybrid2` with `"algorithm"`, optional per-source `"shape"`/`"spacing"`/`"kerning"` and, for
  transfer only, `"base_source"`.
- `CustomFontRecipe::validate`: 2..=4 sources, finite weight > 0, non-empty identities, no
  duplicate identity under the CALLER's normalization rule, property weights (one entry per
  source, each finite > 0), transfer base index in range.
- `HybridParams::is_baseline` (picks the kind tag), `HybridParams::effective_weights`.

Notes:
Hand-written over `serde_json::Value` (no serde derive): the error text and the canonical
output (field order, shortest round-trip weight decimal) are part of the contract. Unknown
extra fields are ignored on read (forward compatibility) and never written.
The KIND TAG is the compatibility gate with older builds: a build that predates `hybrid2`
reports it as an unknown kind and keeps it verbatim (unavailable, never compiled as a plain
average), whereas an extra field under `hybrid` would be silently ignored. Hence every
recipe whose output differs from the plain average MUST be written as `hybrid2`, and a
`hybrid2` recipe this build cannot read (an unknown algorithm) is `UnknownAlgorithm`, never
read as `Auto`.
*/

use std::collections::HashSet;

use serde_json::{Map, Number, Value};

/// Fewest sources a hybrid recipe may have (decision D3).
pub const MIN_HYBRID_SOURCES: usize = 2;
/// Most sources a hybrid recipe may have (decision D3).
pub const MAX_HYBRID_SOURCES: usize = 4;

/// JSON tag of a baseline hybrid (algorithm `Auto`, no property weights): readable by every
/// build that knows hybrids.
pub const KIND_HYBRID: &str = "hybrid";
/// JSON tag of every other hybrid (a chosen algorithm or separate property weights). Builds
/// that predate it treat it as an unknown kind, so they never compile such a recipe as a plain
/// average.
pub const KIND_HYBRID_V2: &str = "hybrid2";

/// JSON key (and `InvalidPropertyWeight::property` name) of `PropertyWeights::shape`.
const PROPERTY_SHAPE: &str = "shape";
/// JSON key (and `InvalidPropertyWeight::property` name) of `PropertyWeights::spacing`.
const PROPERTY_SPACING: &str = "spacing";
/// JSON key (and `InvalidPropertyWeight::property` name) of `PropertyWeights::kerning`.
const PROPERTY_KERNING: &str = "kerning";

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

/// Separate weights of one source for the three properties a hybrid blends (any positive
/// finite scale each; every property is normalized over its own participants).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PropertyWeights {
    /// Weight of the source's glyph SHAPES (outline blend, `NoBlend` winner, matched /
    /// skeleton reference, transfer target parameters).
    pub shape: f32,
    /// Weight of the source's SPACING: per-glyph advances and font-wide vertical metrics.
    pub spacing: f32,
    /// Weight of the source's inherited KERNING pairs.
    pub kerning: f32,
}

/// How a hybrid's glyph outlines are produced from two or more inked source glyphs. Advances,
/// metrics and kerning are blended the same way under every algorithm. Glyphs with ink in at
/// most one source are copied under every algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BlendAlgorithm {
    /// Best available path per glyph: exact point interpolation, else contour correspondence,
    /// else the distance-field blend. The only algorithm of baseline (`hybrid`) recipes.
    #[default]
    Auto,
    /// Always the distance-field blend.
    Sdf,
    /// No outline blending: each glyph is copied from the inked source with the highest shape
    /// weight (ties: the earliest source in recipe order).
    NoBlend,
    /// Exact interpolation when the point structures match, else stroke-skeleton matching,
    /// else (per glyph) the distance-field blend.
    Skeleton,
    /// The base font's glyph shapes with the sources' averaged font parameters (stem
    /// thickness, width, x-height, cap height, slant) transferred onto them.
    Transfer {
        /// Recipe index of the base font whose shapes are kept.
        base_source: usize,
    },
}

impl BlendAlgorithm {
    /// Canonical JSON spelling (`"auto"`, `"sdf"`, `"no_blend"`, `"skeleton"`, `"transfer"`);
    /// the transfer base index is a separate field.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Sdf => "sdf",
            Self::NoBlend => "no_blend",
            Self::Skeleton => "skeleton",
            Self::Transfer { .. } => "transfer",
        }
    }

    /// Parses the canonical JSON spelling; `None` for anything else. `"transfer"` yields
    /// `Transfer { base_source: 0 }`: the base index is read from its own field by the caller.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "auto" => Some(Self::Auto),
            "sdf" => Some(Self::Sdf),
            "no_blend" => Some(Self::NoBlend),
            "skeleton" => Some(Self::Skeleton),
            "transfer" => Some(Self::Transfer { base_source: 0 }),
            _ => None,
        }
    }
}

/// Raw (not normalized) per-source weights of each blended property, in recipe order; the
/// output of `HybridParams::effective_weights`.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveWeights {
    /// Glyph-shape weights.
    pub shape: Vec<f64>,
    /// Advance and vertical-metric weights.
    pub spacing: Vec<f64>,
    /// Inherited-kerning weights.
    pub kerning: Vec<f64>,
}

/// Parameters of the hybrid kind: weighted sources, the default missing-glyph policy, the
/// outline algorithm and optional separate property weights.
#[derive(Debug, Clone, PartialEq)]
pub struct HybridParams {
    /// Sources in the user's order (the order is part of the recipe key).
    pub sources: Vec<HybridSource>,
    /// Policy of every source without its own override.
    pub missing_default: MissingGlyphPolicy,
    /// How glyph outlines are produced.
    pub algorithm: BlendAlgorithm,
    /// Separate shape / spacing / kerning weights, one entry per source in recipe order, or
    /// `None` to use each source's `weight` for all three.
    pub property_weights: Option<Vec<PropertyWeights>>,
}

impl HybridParams {
    /// Whether the recipe is a BASELINE hybrid (algorithm `Auto`, no property weights): the
    /// form older builds read, written under `KIND_HYBRID` with byte-identical JSON and key.
    #[must_use]
    pub fn is_baseline(&self) -> bool {
        self.algorithm == BlendAlgorithm::Auto && self.property_weights.is_none()
    }

    /// Per-source weight of each property, raw (callers normalize over the participants of
    /// each use). The one owner of "which weight applies where": shapes take
    /// `PropertyWeights::shape`, advances and metrics `spacing`, kerning `kerning`; without
    /// property weights all three are the source's `weight`. A source without a property
    /// entry (only in a recipe `validate` rejects) also uses its `weight`.
    #[must_use]
    pub fn effective_weights(&self) -> EffectiveWeights {
        let pick = |property: fn(&PropertyWeights) -> f32| -> Vec<f64> {
            self.sources
                .iter()
                .enumerate()
                .map(|(index, source)| {
                    let split = self.property_weights.as_ref().and_then(|weights| weights.get(index));
                    f64::from(split.map_or(source.weight, property))
                })
                .collect()
        };
        EffectiveWeights {
            shape: pick(|weights| weights.shape),
            spacing: pick(|weights| weights.spacing),
            kerning: pick(|weights| weights.kerning),
        }
    }
}

/// The kind of a custom font and its kind-specific parameters.
#[derive(Debug, Clone, PartialEq)]
pub enum CustomFontKind {
    /// Weighted outline average of 2..=4 real fonts.
    Hybrid(HybridParams),
}

impl CustomFontKind {
    /// JSON `kind` tag of this recipe: for a hybrid, `KIND_HYBRID` when it is a baseline
    /// (`HybridParams::is_baseline`), else `KIND_HYBRID_V2`.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Hybrid(params) if params.is_baseline() => KIND_HYBRID,
            Self::Hybrid(_) => KIND_HYBRID_V2,
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
    /// A `hybrid2` recipe names an algorithm this build does not implement (a newer build wrote
    /// it); the recipe should be kept verbatim and never compiled with another algorithm.
    #[error("unknown hybrid blend algorithm '{0}'")]
    UnknownAlgorithm(String),
    /// The property weights do not have exactly one entry per source.
    #[error("the recipe has {found} property weight entries for {expected} sources")]
    PropertyWeightsCount {
        /// Sources in the recipe.
        expected: usize,
        /// Property weight entries.
        found: usize,
    },
    /// A property weight is not finite or not greater than zero.
    #[error("source '{identity}' has an invalid {property} weight {value} (must be finite and > 0)")]
    InvalidPropertyWeight {
        /// The source's identity.
        identity: String,
        /// The property: `"shape"`, `"spacing"` or `"kerning"`.
        property: &'static str,
        /// The offending weight.
        value: f32,
    },
    /// The transfer algorithm's base index does not name a source.
    #[error("the transfer base font #{index} does not exist, the recipe has {count} sources")]
    BaseSourceOutOfRange {
        /// The base index.
        index: usize,
        /// Sources in the recipe.
        count: usize,
    },
}

impl CustomFontRecipe {
    /// Reads a recipe from its JSON form.
    ///
    /// Only the shape is checked here (field presence and types); bounds and duplicates are
    /// `validate`'s job, so a stored recipe that became invalid can still be loaded and shown.
    ///
    /// `hybrid` reads the baseline fields only (algorithm `Auto`, no property weights; extra
    /// fields such as an `algorithm` are ignored, as every earlier build ignores them).
    /// `hybrid2` additionally requires `algorithm`, takes the per-source `shape` / `spacing` /
    /// `kerning` keys on every source or on none, and `base_source` exactly when the
    /// algorithm is `transfer`.
    ///
    /// # Errors
    /// `RecipeError::UnknownKind` when `kind` is a string this build does not implement (keep
    /// the JSON verbatim), `RecipeError::UnknownAlgorithm` when a `hybrid2` names an algorithm
    /// this build does not implement (checked before every other `hybrid2` field, so a newer
    /// recipe is always reported as such), `RecipeError::Malformed` for any other shape
    /// problem.
    pub fn from_json(value: &Value) -> Result<Self, RecipeError> {
        let object = value
            .as_object()
            .ok_or_else(|| RecipeError::Malformed("the recipe is not a JSON object".to_owned()))?;
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| RecipeError::Malformed("field 'kind' is missing or not a string".to_owned()))?;
        match kind {
            KIND_HYBRID => parse_hybrid(object, false).map(|params| Self { kind: CustomFontKind::Hybrid(params) }),
            KIND_HYBRID_V2 => parse_hybrid(object, true).map(|params| Self { kind: CustomFontKind::Hybrid(params) }),
            other => Err(RecipeError::UnknownKind(other.to_owned())),
        }
    }

    /// The canonical JSON form: fixed field order, weights written as the shortest decimal
    /// that reads back to the same `f32`, `missing` always present (`null` = follow default).
    ///
    /// The kind tag is `CustomFontKind::tag`: a baseline hybrid is written exactly in the
    /// pre-algorithm form under `hybrid` (so older builds keep reading it); any other hybrid is
    /// written under `hybrid2` with `algorithm`, `base_source` (transfer only) and, with
    /// property weights, `shape` / `spacing` / `kerning` on every source.
    ///
    /// A non-finite weight (only constructible in code, never read from JSON) is written as
    /// `null`, which `from_json` rejects — `validate` reports it first; so does a property
    /// weight list whose length differs from the sources (its missing entries are not written).
    #[must_use]
    pub fn to_json(&self) -> Value {
        match &self.kind {
            CustomFontKind::Hybrid(params) => {
                let split = params.property_weights.as_deref();
                let sources: Vec<Value> = params
                    .sources
                    .iter()
                    .enumerate()
                    .map(|(index, source)| {
                        let mut entry = Map::new();
                        entry.insert("identity".to_owned(), Value::String(source.identity.clone()));
                        entry.insert("weight".to_owned(), weight_to_json(source.weight));
                        entry.insert(
                            "missing".to_owned(),
                            source
                                .missing_override
                                .map_or(Value::Null, |policy| Value::String(policy.as_str().to_owned())),
                        );
                        if let Some(weights) = split.and_then(|split| split.get(index)) {
                            entry.insert(PROPERTY_SHAPE.to_owned(), weight_to_json(weights.shape));
                            entry.insert(PROPERTY_SPACING.to_owned(), weight_to_json(weights.spacing));
                            entry.insert(PROPERTY_KERNING.to_owned(), weight_to_json(weights.kerning));
                        }
                        Value::Object(entry)
                    })
                    .collect();
                let mut root = Map::new();
                root.insert("kind".to_owned(), Value::String(self.kind.tag().to_owned()));
                root.insert("sources".to_owned(), Value::Array(sources));
                root.insert(
                    "missing_default".to_owned(),
                    Value::String(params.missing_default.as_str().to_owned()),
                );
                if !params.is_baseline() {
                    root.insert("algorithm".to_owned(), Value::String(params.algorithm.as_str().to_owned()));
                    if let BlendAlgorithm::Transfer { base_source } = params.algorithm {
                        // `usize` is at most 64 bits on every supported target.
                        root.insert("base_source".to_owned(), Value::from(u64::try_from(base_source).unwrap_or(u64::MAX)));
                    }
                }
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
    /// `TooFewSources` / `TooManySources` (outside `2..=4`), `PropertyWeightsCount` (property
    /// weights present but not one per source), then per source in order `EmptyIdentity`,
    /// `InvalidWeight` (not finite or `<= 0`), `InvalidPropertyWeight` (shape, spacing,
    /// kerning), `DuplicateSource`, and finally `BaseSourceOutOfRange` (transfer base index
    /// not a source). The first violation in that order wins.
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
    if let Some(split) = &params.property_weights
        && split.len() != count
    {
        return Err(RecipeError::PropertyWeightsCount { expected: count, found: split.len() });
    }
    let valid = |value: f32| value.is_finite() && value > 0.0;
    let mut seen = HashSet::with_capacity(count);
    for (index, source) in params.sources.iter().enumerate() {
        if source.identity.trim().is_empty() {
            return Err(RecipeError::EmptyIdentity { index });
        }
        if !valid(source.weight) {
            return Err(RecipeError::InvalidWeight { identity: source.identity.clone(), weight: source.weight });
        }
        if let Some(weights) = params.property_weights.as_ref().and_then(|split| split.get(index)) {
            let properties = [(PROPERTY_SHAPE, weights.shape), (PROPERTY_SPACING, weights.spacing), (PROPERTY_KERNING, weights.kerning)];
            if let Some(&(property, value)) = properties.iter().find(|(_, value)| !valid(*value)) {
                return Err(RecipeError::InvalidPropertyWeight { identity: source.identity.clone(), property, value });
            }
        }
        if !seen.insert(normalize_identity(&source.identity)) {
            return Err(RecipeError::DuplicateSource { identity: source.identity.clone() });
        }
    }
    if let BlendAlgorithm::Transfer { base_source } = params.algorithm
        && base_source >= count
    {
        return Err(RecipeError::BaseSourceOutOfRange { index: base_source, count });
    }
    Ok(())
}

/// Reads the hybrid kind's fields from the recipe object; `v2` = the `hybrid2` tag (algorithm,
/// property weights and transfer base are read), else only the baseline fields are.
fn parse_hybrid(object: &Map<String, Value>, v2: bool) -> Result<HybridParams, RecipeError> {
    // The algorithm comes first so that a recipe of a newer build is reported as
    // `UnknownAlgorithm` even when its other fields have a shape this build does not expect.
    let algorithm = if v2 { parse_algorithm(object)? } else { BlendAlgorithm::Auto };
    let raw_sources = object
        .get("sources")
        .and_then(Value::as_array)
        .ok_or_else(|| RecipeError::Malformed("field 'sources' is missing or not an array".to_owned()))?;
    let parsed = raw_sources
        .iter()
        .enumerate()
        .map(|(index, raw)| parse_source(index, raw, v2))
        .collect::<Result<Vec<_>, _>>()?;
    let split_count = parsed.iter().filter(|(_, weights)| weights.is_some()).count();
    if split_count != 0 && split_count != parsed.len() {
        return Err(RecipeError::Malformed(
            "the property weights 'shape' / 'spacing' / 'kerning' must be on every source or on none".to_owned(),
        ));
    }
    let (sources, split): (Vec<HybridSource>, Vec<Option<PropertyWeights>>) = parsed.into_iter().unzip();
    let property_weights = if split_count == 0 { None } else { split.into_iter().collect::<Option<Vec<_>>>() };
    let missing_default = object
        .get("missing_default")
        .and_then(Value::as_str)
        .and_then(MissingGlyphPolicy::parse)
        .ok_or_else(|| {
            RecipeError::Malformed("field 'missing_default' is missing or not 'disable' / 'no_influence'".to_owned())
        })?;
    Ok(HybridParams { sources, missing_default, algorithm, property_weights })
}

/// Reads a `hybrid2` recipe's `algorithm` and, for `transfer`, its `base_source` (present
/// exactly for transfer; a non-negative integer).
fn parse_algorithm(object: &Map<String, Value>) -> Result<BlendAlgorithm, RecipeError> {
    let raw = object
        .get("algorithm")
        .and_then(Value::as_str)
        .ok_or_else(|| RecipeError::Malformed("field 'algorithm' is missing or not a string".to_owned()))?;
    let algorithm = BlendAlgorithm::parse(raw).ok_or_else(|| RecipeError::UnknownAlgorithm(raw.to_owned()))?;
    match (algorithm, object.get("base_source")) {
        (BlendAlgorithm::Transfer { .. }, Some(raw_base)) => {
            let base_source = raw_base
                .as_u64()
                .and_then(|base| usize::try_from(base).ok())
                .ok_or_else(|| RecipeError::Malformed("field 'base_source' is not a non-negative integer".to_owned()))?;
            Ok(BlendAlgorithm::Transfer { base_source })
        }
        (BlendAlgorithm::Transfer { .. }, None) => {
            Err(RecipeError::Malformed("field 'base_source' is required by the 'transfer' algorithm".to_owned()))
        }
        (BlendAlgorithm::Auto | BlendAlgorithm::Sdf | BlendAlgorithm::NoBlend | BlendAlgorithm::Skeleton, Some(_)) => {
            Err(RecipeError::Malformed(format!("field 'base_source' is only allowed with the 'transfer' algorithm, not '{raw}'")))
        }
        (BlendAlgorithm::Auto | BlendAlgorithm::Sdf | BlendAlgorithm::NoBlend | BlendAlgorithm::Skeleton, None) => Ok(algorithm),
    }
}

/// Reads one `sources[index]` entry; with `v2`, also its property weights (all three keys or
/// none, `None` when absent).
fn parse_source(index: usize, raw: &Value, v2: bool) -> Result<(HybridSource, Option<PropertyWeights>), RecipeError> {
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
    let property_weights = if v2 { parse_property_weights(index, object)? } else { None };
    Ok((HybridSource { identity: identity.to_owned(), weight: weight_from_json(weight), missing_override }, property_weights))
}

/// Reads the `shape` / `spacing` / `kerning` keys of `sources[index]`: all three numbers, or
/// none of them (`None`).
fn parse_property_weights(index: usize, object: &Map<String, Value>) -> Result<Option<PropertyWeights>, RecipeError> {
    let keys = [PROPERTY_SHAPE, PROPERTY_SPACING, PROPERTY_KERNING];
    let present = keys.iter().filter(|key| object.contains_key(**key)).count();
    if present == 0 {
        return Ok(None);
    }
    if present != keys.len() {
        return Err(RecipeError::Malformed(format!(
            "sources[{index}] must have all of 'shape' / 'spacing' / 'kerning' or none of them"
        )));
    }
    let read = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_f64)
            .map(weight_from_json)
            .ok_or_else(|| RecipeError::Malformed(format!("sources[{index}].{key} is not a number")))
    };
    Ok(Some(PropertyWeights { shape: read(PROPERTY_SHAPE)?, spacing: read(PROPERTY_SPACING)?, kerning: read(PROPERTY_KERNING)? }))
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
                algorithm: BlendAlgorithm::Auto,
                property_weights: None,
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
        // The exact bytes the build before algorithms wrote for this recipe (serialized with
        // sorted keys): a baseline recipe must stay byte-identical for older builds.
        assert_eq!(
            value.to_string(),
            r#"{"kind":"hybrid","missing_default":"no_influence","sources":[{"identity":"FontA","missing":null,"weight":0.6},{"identity":"FontB","missing":"disable","weight":0.4}]}"#
        );
    }

    /// `sample()` with another algorithm and property weights.
    fn sample_with(algorithm: BlendAlgorithm, property_weights: Option<Vec<PropertyWeights>>) -> CustomFontRecipe {
        let mut recipe = sample();
        let CustomFontKind::Hybrid(params) = &mut recipe.kind;
        params.algorithm = algorithm;
        params.property_weights = property_weights;
        recipe
    }

    fn split() -> Vec<PropertyWeights> {
        vec![
            PropertyWeights { shape: 0.6, spacing: 0.25, kerning: 1.0 },
            PropertyWeights { shape: 0.4, spacing: 0.75, kerning: 2.0 },
        ]
    }

    #[test]
    fn hybrid2_round_trips_every_algorithm_and_property_weights() {
        let algorithms = [
            BlendAlgorithm::Auto,
            BlendAlgorithm::Sdf,
            BlendAlgorithm::NoBlend,
            BlendAlgorithm::Skeleton,
            BlendAlgorithm::Transfer { base_source: 1 },
        ];
        for algorithm in algorithms {
            for weights in [None, Some(split())] {
                let recipe = sample_with(algorithm, weights.clone());
                let value = recipe.to_json();
                let expected_tag = if recipe.kind.tag() == KIND_HYBRID { "hybrid" } else { "hybrid2" };
                assert_eq!(value["kind"], expected_tag);
                let back = CustomFontRecipe::from_json(&value).expect("canonical JSON reads back");
                assert_eq!(back, recipe, "{algorithm:?} {weights:?}");
                assert_eq!(back.to_json(), value);
            }
        }
        assert_eq!(
            sample_with(BlendAlgorithm::Transfer { base_source: 1 }, Some(split())).to_json(),
            json!({
                "kind": "hybrid2",
                "sources": [
                    {"identity": "FontA", "weight": 0.6, "missing": null, "shape": 0.6, "spacing": 0.25, "kerning": 1.0},
                    {"identity": "FontB", "weight": 0.4, "missing": "disable", "shape": 0.4, "spacing": 0.75, "kerning": 2.0}
                ],
                "missing_default": "no_influence",
                "algorithm": "transfer",
                "base_source": 1
            })
        );
        let sdf = sample_with(BlendAlgorithm::Sdf, None).to_json();
        assert_eq!(sdf["algorithm"], "sdf");
        assert!(sdf.get("base_source").is_none() && sdf["sources"][0].get("shape").is_none());
    }

    #[test]
    fn is_baseline_picks_the_kind_tag() {
        assert_eq!(sample().kind.tag(), KIND_HYBRID);
        let CustomFontKind::Hybrid(params) = &sample().kind;
        assert!(params.is_baseline());
        for (algorithm, weights) in [(BlendAlgorithm::Sdf, None), (BlendAlgorithm::Auto, Some(split()))] {
            let recipe = sample_with(algorithm, weights);
            let CustomFontKind::Hybrid(params) = &recipe.kind;
            assert!(!params.is_baseline());
            assert_eq!(recipe.kind.tag(), KIND_HYBRID_V2);
        }
        // A `hybrid2` that is in fact a baseline reads as one and is written back as `hybrid`,
        // so switching an editor back to Auto with one weight per source makes the font
        // readable by older builds again.
        let v2_baseline = json!({
            "kind": "hybrid2",
            "sources": [{"identity": "FontA", "weight": 0.6, "missing": null}, {"identity": "FontB", "weight": 0.4, "missing": "disable"}],
            "missing_default": "no_influence",
            "algorithm": "auto"
        });
        let recipe = CustomFontRecipe::from_json(&v2_baseline).expect("readable");
        assert_eq!(recipe, sample());
        assert_eq!(recipe.to_json()["kind"], "hybrid");
    }

    #[test]
    fn hybrid_ignores_algorithm_and_property_fields() {
        // Older builds ignore these under `hybrid`; so does this one, so a `hybrid` recipe
        // means exactly the plain average everywhere.
        let value = json!({
            "kind": "hybrid",
            "sources": [
                {"identity": "A", "weight": 1, "shape": 2, "spacing": 3, "kerning": 4},
                {"identity": "B", "weight": 2}
            ],
            "missing_default": "disable",
            "algorithm": "skeleton",
            "base_source": 7
        });
        let recipe = CustomFontRecipe::from_json(&value).expect("readable");
        let CustomFontKind::Hybrid(params) = &recipe.kind;
        assert_eq!(params.algorithm, BlendAlgorithm::Auto);
        assert_eq!(params.property_weights, None);
        assert_eq!(recipe.kind.tag(), KIND_HYBRID);
    }

    #[test]
    fn hybrid2_shape_rules() {
        let with = |sources: serde_json::Value, extra: &[(&str, serde_json::Value)]| {
            let mut value = json!({"kind": "hybrid2", "sources": sources, "missing_default": "disable", "algorithm": "sdf"});
            for (key, field) in extra {
                value[*key] = field.clone();
            }
            CustomFontRecipe::from_json(&value)
        };
        let plain = json!([{"identity": "A", "weight": 1}, {"identity": "B", "weight": 1}]);
        assert!(with(plain.clone(), &[]).is_ok());
        // Property keys: all three on a source or none, and on every source or on none.
        let partial = json!([{"identity": "A", "weight": 1, "shape": 1, "spacing": 1}, {"identity": "B", "weight": 1}]);
        assert!(matches!(with(partial, &[]), Err(RecipeError::Malformed(_))));
        let one_split = json!([{"identity": "A", "weight": 1, "shape": 1, "spacing": 1, "kerning": 1}, {"identity": "B", "weight": 1}]);
        assert!(matches!(with(one_split, &[]), Err(RecipeError::Malformed(_))));
        let not_number = json!([
            {"identity": "A", "weight": 1, "shape": "1", "spacing": 1, "kerning": 1},
            {"identity": "B", "weight": 1, "shape": 1, "spacing": 1, "kerning": 1}
        ]);
        assert!(matches!(with(not_number, &[]), Err(RecipeError::Malformed(_))));
        // `algorithm` is required; `base_source` exactly with transfer, a non-negative integer.
        let mut no_algorithm = json!({"kind": "hybrid2", "sources": plain.clone(), "missing_default": "disable"});
        assert!(matches!(CustomFontRecipe::from_json(&no_algorithm), Err(RecipeError::Malformed(_))));
        no_algorithm["algorithm"] = json!(3);
        assert!(matches!(CustomFontRecipe::from_json(&no_algorithm), Err(RecipeError::Malformed(_))));
        assert!(matches!(with(plain.clone(), &[("base_source", json!(0))]), Err(RecipeError::Malformed(_))));
        assert!(matches!(with(plain.clone(), &[("algorithm", json!("transfer"))]), Err(RecipeError::Malformed(_))));
        for bad_base in [json!(-1), json!(0.5), json!("0")] {
            assert!(matches!(
                with(plain.clone(), &[("algorithm", json!("transfer")), ("base_source", bad_base)]),
                Err(RecipeError::Malformed(_))
            ));
        }
        let transfer = with(plain.clone(), &[("algorithm", json!("transfer")), ("base_source", json!(1))]).expect("readable");
        let CustomFontKind::Hybrid(params) = &transfer.kind;
        assert_eq!(params.algorithm, BlendAlgorithm::Transfer { base_source: 1 });
    }

    #[test]
    fn unknown_algorithm_wins_over_every_other_field() {
        // A newer build's algorithm must read as "newer", never as malformed or as `Auto`,
        // whatever else that build put into the recipe.
        let value = json!({"kind": "hybrid2", "sources": "a newer shape", "algorithm": "glyph_dreams"});
        assert_eq!(CustomFontRecipe::from_json(&value), Err(RecipeError::UnknownAlgorithm("glyph_dreams".to_owned())));
        for raw in ["auto", "sdf", "no_blend", "skeleton", "transfer"] {
            let parsed = BlendAlgorithm::parse(raw).expect("known spelling");
            assert_eq!(parsed.as_str(), raw);
        }
        assert_eq!(BlendAlgorithm::parse("Auto"), None);
    }

    #[test]
    fn validate_checks_property_weights_and_the_transfer_base() {
        assert_eq!(sample_with(BlendAlgorithm::Transfer { base_source: 1 }, Some(split())).validate(&exact), Ok(()));
        assert_eq!(
            sample_with(BlendAlgorithm::Transfer { base_source: 2 }, Some(split())).validate(&exact),
            Err(RecipeError::BaseSourceOutOfRange { index: 2, count: 2 })
        );
        let short = Some(vec![PropertyWeights { shape: 1.0, spacing: 1.0, kerning: 1.0 }]);
        assert_eq!(
            sample_with(BlendAlgorithm::Auto, short).validate(&exact),
            Err(RecipeError::PropertyWeightsCount { expected: 2, found: 1 })
        );
        for (index, property) in ["shape", "spacing", "kerning"].into_iter().enumerate() {
            for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
                let mut values = [1.0_f32; 3];
                values[index] = bad;
                let weights = Some(vec![
                    PropertyWeights { shape: 1.0, spacing: 1.0, kerning: 1.0 },
                    PropertyWeights { shape: values[0], spacing: values[1], kerning: values[2] },
                ]);
                let result = sample_with(BlendAlgorithm::Auto, weights).validate(&exact);
                assert!(
                    matches!(&result, Err(RecipeError::InvalidPropertyWeight { identity, property: found, .. }) if identity == "FontB" && *found == property),
                    "{property} = {bad}: {result:?}"
                );
            }
        }
        // First violation in order: a bad plain weight of an earlier source wins over a later
        // source's property weight, and per-source checks win over the base index.
        let mut recipe = sample_with(BlendAlgorithm::Transfer { base_source: 9 }, Some(split()));
        let CustomFontKind::Hybrid(params) = &mut recipe.kind;
        params.sources[0].weight = 0.0;
        assert!(matches!(recipe.validate(&exact), Err(RecipeError::InvalidWeight { .. })));
    }

    #[test]
    fn effective_weights_route_each_property() {
        let CustomFontKind::Hybrid(plain) = sample().kind;
        let weights = plain.effective_weights();
        let expected = vec![f64::from(0.6_f32), f64::from(0.4_f32)];
        assert_eq!(weights, EffectiveWeights { shape: expected.clone(), spacing: expected.clone(), kerning: expected });
        let CustomFontKind::Hybrid(split_params) = sample_with(BlendAlgorithm::Auto, Some(split())).kind;
        let weights = split_params.effective_weights();
        assert_eq!(weights.shape, vec![f64::from(0.6_f32), f64::from(0.4_f32)]);
        assert_eq!(weights.spacing, vec![0.25, 0.75]);
        assert_eq!(weights.kerning, vec![1.0, 2.0]);
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
            kind: CustomFontKind::Hybrid(HybridParams {
                sources,
                missing_default: MissingGlyphPolicy::Disable,
                algorithm: BlendAlgorithm::Auto,
                property_weights: None,
            }),
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
