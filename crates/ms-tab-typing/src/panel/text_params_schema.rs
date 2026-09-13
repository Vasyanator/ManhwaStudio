/*
File: panel/text_params_schema.rs

Purpose:
The SINGLE owner of the persisted `render_data.text_params` schema: its version, its
frozen per-version default set, the writer that omits defaults, and the reader that
materializes them for the version the document itself declares.

Main responsibilities:
- own `TEXT_PARAMS_SCHEMA_VERSION` and the `"schema"` key;
- own the FROZEN schema-2 default set (a value equal to its default is not written);
- `write_text_params`: strip defaults / dead keys / legacy font keys, stamp `"schema"`;
- `read_text_params`: fill the defaults of the document's OWN schema, leaving a
  schema-less (v1) document to the legacy readers untouched;
- own the SCHEMA-1 legacy font-key read ORDER (`legacy_font_name_candidates`), so the
  codec's conversion and the PSD export cannot disagree about which key names the font;
- own the read of any key whose TYPE changed across builds, so every reader resolves the
  old and the new shape identically, and the matching WRITER that picks the shape an
  older build can still read (`hanging_punctuation_weight` / `hanging_punctuation_value`).

Key functions:
- text_params_schema_version()
- frozen_v2_defaults()
- write_text_params()
- read_text_params()
- legacy_font_path()
- legacy_font_name_candidates()
- hanging_punctuation_weight()
- hanging_punctuation_value()

Notes:
The frozen default set is a CONTRACT, not a mirror of the panel's current defaults: a
schema-2 document that omits a key means exactly the value frozen here, forever. A
later change to a panel default therefore must NOT be applied here — it only makes
that key start being written explicitly. Changing a frozen value silently
reinterprets already-written documents and is allowed only together with a bump of
`TEXT_PARAMS_SCHEMA_VERSION` plus a read branch for the older version. `defaults_are_frozen`
pins every value so such a change cannot pass unnoticed.
*/

use serde_json::{Map, Value, json};
use std::borrow::Cow;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Current `text_params` schema version, written into every payload this build emits.
pub(crate) const TEXT_PARAMS_SCHEMA_VERSION: u32 = 2;

/// Key carrying the schema version inside a `text_params` object. A document WITHOUT
/// it is schema 1 (everything written before the version existed).
pub(crate) const TEXT_PARAMS_SCHEMA_KEY: &str = "schema";

/// Keys written unconditionally, even when equal to their frozen default.
///
/// `font` has no default at all (it is the overlay's font identity). `text` and
/// `width_px` are per-overlay essentials that several small readers pick out of the
/// object directly (layer-row previews, `overlay_render_data_width_hint`) without going
/// through [`read_text_params`]; keeping them present makes those readers correct by
/// construction for a few bytes.
const ALWAYS_WRITTEN_KEYS: [&str; 3] = ["font", "text", "width_px"];

/// Keys that no longer have any reader in `src/` and are dropped on write/conversion.
///
/// `aggressive_word_breaks` still feeds ONE legacy-only path
/// (`codec::normalize_text_wrap_mode_legacy`, which resolves the ancient `"smart"` wrap
/// token); that path runs on the schema-1 document BEFORE conversion and materializes
/// its result into `text_wrap_mode`, so dropping the key from the schema-2 payload
/// cannot change how anything renders.
pub(crate) const DEAD_TEXT_PARAM_KEYS: [&str; 2] =
    ["strict_shape_fit", "aggressive_word_breaks"];

/// Schema-1 font keys. Schema 2 carries the font identity in `font` and nothing else;
/// these are read (never written) by the legacy path and dropped on conversion — but
/// ONLY once the identity has actually been resolved (see `codec::upgrade_text_params_to_v2`).
pub(crate) const LEGACY_FONT_KEYS: [&str; 4] = [
    "font_path",
    "font_label",
    "font_original_name",
    "font_family",
];

/// Set once a document declaring a NEWER schema has been reported, so the warning is
/// written once per process instead of once per overlay per load.
static FUTURE_SCHEMA_REPORTED: AtomicBool = AtomicBool::new(false);

/// Normalizes a stored `hanging_punctuation` value — in EITHER of its two historical
/// representations — into the renderer's `0.0..=1.0` strength.
///
/// The key was a `bool` until the parameter became a strength slider, and documents
/// written by every older build are still on disk, so both shapes must stay readable
/// forever:
/// * a JSON number is the strength itself, clamped into `0.0..=1.0`;
/// * a JSON `true` is the fully-hanging `1.0` it used to mean, `false` is `0.0`.
///
/// Returns `None` for any other JSON kind (string, array, object, null) so the caller
/// keeps its own default instead of silently reading a garbage value as `0.0`. This is
/// the ONE reader of the key: both `create_apply` (panel state) and `codec`
/// (`TextRenderParams`) go through it so they cannot disagree about a legacy document.
#[must_use]
pub(crate) fn hanging_punctuation_weight(value: &Value) -> Option<f32> {
    if let Some(number) = value.as_f64() {
        // `as f32` saturates an out-of-range magnitude to an infinity; `clamp_hanging_weight`
        // maps both infinities into the range and rejects NaN, so no unchecked value escapes.
        return Some(ms_text_util::text_punctuation::clamp_hanging_weight(number as f32));
    }
    value.as_bool().map(|on| if on { 1.0 } else { 0.0 })
}

/// Serializes a `0.0..=1.0` hanging-punctuation strength into the shape a stored
/// `text_params` carries. The WRITE half of [`hanging_punctuation_weight`]; the two
/// live together so the dual representation has one definition.
///
/// The two exact endpoints are written as the LEGACY BOOL — `1.0` as `true`, `0.0` as
/// `false` — and only a strictly fractional value becomes a number. That is deliberate,
/// for three reasons:
/// * an OLDER build reads this key with `Value::as_bool` and falls back to "off" on a
///   number. The endpoints are exactly the values such a build can express, so keeping
///   their old shape means a project saved here still renders correctly after a rollback
///   instead of silently losing its hanging punctuation;
/// * `true` is `Value`-equal to the FROZEN schema-2 default, so a document left at the
///   default omits the key entirely again ([`write_text_params`]) — no churn in existing
///   project files;
/// * `false` is `Value`-equal to the schema-1 absent-meaning materialized by the codec's
///   conversion, so an off value round-trips through it unchanged.
///
/// A FRACTIONAL value read by an old build still degrades to "off". That is unavoidable
/// and accepted: the value could not exist in that build at all.
#[must_use]
pub(crate) fn hanging_punctuation_value(weight: f32) -> Value {
    let weight = ms_text_util::text_punctuation::clamp_hanging_weight(weight);
    // Compared with `<=` / `>=` rather than `==` so `-0.0` and any value the clamp
    // pinned to an endpoint take the bool branch too.
    if weight <= 0.0 {
        json!(false)
    } else if weight >= 1.0 {
        json!(true)
    } else {
        json!(weight)
    }
}

/// The `font_path` a SCHEMA-1 `text_params` object carries, trimmed; `None` when absent or
/// empty. Never written any more — this is the read side of the legacy contract, and the
/// path is a HINT about where bytes once came from, never evidence of identity.
#[must_use]
pub(crate) fn legacy_font_path(obj: &Map<String, Value>) -> Option<&str> {
    obj.get("font_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
}

/// EVERY font NAME a SCHEMA-1 `text_params` object carries, in the historical read order
/// `font_original_name` (family) → `font_label` (the identity on late v1 data, a stem or
/// label on old data) → `font_family` → `font` → file stem of `font_path`. Trimmed, empty
/// entries skipped, duplicates removed keeping the first position.
///
/// This ORDER is part of the persisted-schema contract and lives here, with the schema, so
/// that the codec (which resolves and converts) and the PSD export (which names the font
/// for Photoshop) cannot drift apart. Every form is a READ-ONLY alias of `TabFontProvider`
/// and the panel's `find_font_idx_by_name_forms`.
///
/// The result is a list of CANDIDATES, not one best guess: a real v1 document routinely
/// carries a family name that is no longer installed next to a label that still is. A
/// caller that RESOLVES must walk the list in order and take the first match; a caller
/// that merely needs a name takes the first entry. Empty when no font is named at all.
#[must_use]
pub(crate) fn legacy_font_name_candidates(obj: &Map<String, Value>) -> Vec<String> {
    let read_name = |key: &str| {
        obj.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    };
    let stem = legacy_font_path(obj).and_then(|path| {
        std::path::Path::new(path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(ToOwned::to_owned)
    });
    let mut out: Vec<String> = Vec::new();
    for candidate in [
        read_name("font_original_name"),
        read_name("font_label"),
        read_name("font_family"),
        read_name("font"),
        stem,
    ]
    .into_iter()
    .flatten()
    {
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// Schema version a stored `text_params` object declares.
///
/// Returns 1 for a document with no `"schema"` key (everything written before the
/// version existed) and for a malformed/negative value, which is the safest reading:
/// schema 1 is interpreted by the legacy rules, which never assume a default.
#[must_use]
pub(crate) fn text_params_schema_version(obj: &Map<String, Value>) -> u32 {
    obj.get(TEXT_PARAMS_SCHEMA_KEY)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(1)
}

/// The FROZEN schema-2 default set: the meaning of every key a schema-2 payload omits.
///
/// Built once per process. See the file header for why these values must not follow a
/// later change of the panel's own defaults.
#[must_use]
pub(crate) fn frozen_v2_defaults() -> &'static Map<String, Value> {
    static DEFAULTS: OnceLock<Map<String, Value>> = OnceLock::new();
    DEFAULTS.get_or_init(|| {
        // Assembled entry by entry rather than as one `json!` literal: the whole set in
        // a single literal blows serde_json's macro recursion limit.
        let entries: [(&str, Value); 45] = [
            ("text", json!("")),
            ("text_color", json!([0, 0, 0, 255])),
            ("font_size_px", json!(24.0)),
            ("line_spacing", json!("0.00%")),
            ("kerning_mode", json!("auto")),
            ("kerning", json!("0.00%")),
            ("glyph_height", json!("100.00%")),
            ("glyph_width", json!("100.00%")),
            ("width_px", json!(300)),
            ("align", json!("center")),
            ("align_bias", json!(0.0)),
            ("global_rotation_deg", json!(0.0)),
            ("line_placement_percent", json!(0.0)),
            ("line_placement_reference", json!("line_box")),
            ("text_line_mode", json!("horizontal")),
            ("vertical_line_direction", json!("right_to_left")),
            ("text_layout_mode", json!("normal")),
            ("formula_layout", default_formula_layout()),
            ("shape_layout", default_shape_layout()),
            ("drawn_lines_layout", default_drawn_lines_layout()),
            ("vector_lines_layout", default_vector_lines_layout()),
            ("selected_face_index", json!(0)),
            ("force_bold", json!(false)),
            ("force_italic", json!(false)),
            ("faux_bold", json!(false)),
            ("faux_bold_thicken_percent", json!(3.0)),
            ("faux_bold_expand_percent", json!(0.0)),
            ("faux_bold_sharp_corners", json!(true)),
            ("faux_bold_outward_only", json!(true)),
            ("faux_italic", json!(false)),
            ("faux_italic_slant_deg", json!(14.0)),
            ("uppercase_text", json!(false)),
            ("trim_extra_spaces", json!(true)),
            ("replace_ellipsis_with_dots", json!(true)),
            ("force_remove_ellipsis_glyph", json!(false)),
            // DELIBERATELY still the boolean `true`, although the parameter is an
            // `f32` strength now: the frozen VALUE is what an omitting document means,
            // and `true` still means "fully hanging" (`hanging_punctuation_weight`
            // reads it as `1.0`). Nothing already written changes meaning, so
            // `TEXT_PARAMS_SCHEMA_VERSION` must NOT be bumped — and the writer keeps
            // this default reachable: `hanging_punctuation_value` emits the two exact
            // endpoints as the legacy bool, so `1.0` is `Value`-equal to this entry and
            // a document at the default still omits the key. Only a strictly fractional
            // strength is written, as a number.
            ("hanging_punctuation", json!(true)),
            ("new_line_after_sentence", json!(false)),
            ("enable_inline_style_tags", json!(false)),
            ("text_wrap_mode", json!("aggressive")),
            ("anti_aliasing", json!("strong")),
            ("allow_moderate_trees", json!(false)),
            ("text_shape", json!("free")),
            ("shape_min_width_percent", json!(50.0)),
            ("shape_variant", json!(5)),
            ("formed_text", json!("")),
        ];
        let mut defaults = Map::new();
        for (key, value) in entries {
            defaults.insert(key.to_string(), value);
        }
        defaults
    })
}

/// Frozen schema-2 default of `formula_layout` — the serialization
/// `presets_io::text_formula_layout_to_value` produces for default parameters.
fn default_formula_layout() -> Value {
    json!({
        "x_expr": "t * w",
        "y_expr": "0",
        "rotation_expr": "0",
        "use_tangent_rotation": false,
        "t_start": 0.0,
        "t_end": 1.0,
        "offset_x_px": 0.0,
        "offset_y_px": 0.0,
        "scale_x": 1.0,
        "scale_y": 1.0,
        "normal_offset_px": 0.0,
        "letter_spacing_mul": 1.0,
        "letter_spacing_px": 0.0,
        "vars": [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    })
}

/// Frozen schema-2 default of `shape_layout`: the arc shape with default parameters,
/// as `TypingCreatePanelState::shape_layout_to_value` writes it.
fn default_shape_layout() -> Value {
    json!({
        "kind": "arc",
        "length_px": 320.0,
        "amplitude_px": 80.0,
        "width_px": 320.0,
        "height_px": 80.0,
        "frequency": 1.0,
        "orientation": "horizontal",
    })
}

/// Frozen schema-2 default of `drawn_lines_layout`.
fn default_drawn_lines_layout() -> Value {
    json!({
        "use_tangent_rotation": true,
        "static_rotation_rad": 0.0,
        "normal_offset_px": 0.0,
        "letter_spacing_mul": 1.0,
        "letter_spacing_px": 0.0,
        "color_tolerance": 16,
        "continuation_alpha": 64,
        "start_alpha": 192,
    })
}

/// Frozen schema-2 default of `vector_lines_layout`.
fn default_vector_lines_layout() -> Value {
    json!({
        "width_px": 1,
        "height_px": 1,
        "use_tangent_rotation": true,
        "static_rotation_rad": 0.0,
        "normal_offset_px": 0.0,
        "letter_spacing_mul": 1.0,
        "letter_spacing_px": 0.0,
        "lines": [],
    })
}

/// Serializes a FULL `text_params` map into the current schema.
///
/// The caller builds the object exactly as the panel state describes it (every key
/// present, no legacy font keys); this drops what does not need storing — dead keys,
/// any leftover legacy font key, `null` values, and every entry equal to its frozen
/// default — and stamps `"schema"`. Keys in [`ALWAYS_WRITTEN_KEYS`] survive the default
/// strip.
///
/// Takes the map BY VALUE: the caller has just built it and the result is the same
/// allocation with entries removed.
#[must_use]
pub(crate) fn write_text_params(mut params: Map<String, Value>) -> Value {
    for key in DEAD_TEXT_PARAM_KEYS {
        params.remove(key);
    }
    for key in LEGACY_FONT_KEYS {
        params.remove(key);
    }
    let defaults = frozen_v2_defaults();
    params.retain(|key, value| {
        if value.is_null() {
            return false;
        }
        if ALWAYS_WRITTEN_KEYS.contains(&key.as_str()) {
            return true;
        }
        defaults.get(key.as_str()) != Some(&*value)
    });
    params.insert(
        TEXT_PARAMS_SCHEMA_KEY.to_string(),
        Value::from(TEXT_PARAMS_SCHEMA_VERSION),
    );
    Value::Object(params)
}

/// Materializes the defaults of the schema the document ITSELF declares.
///
/// - Schema 1 (no `"schema"` key) is returned BORROWED and untouched: its absent keys
///   mean whatever the legacy readers have always taken them to mean, and this module
///   must not retro-fit today's defaults onto a document written before they existed.
/// - Schema 2 (and, best-effort, anything newer) is returned OWNED with every missing
///   key filled from [`frozen_v2_defaults`], so every downstream reader sees the same
///   fully-populated object a schema-1 document would have carried explicitly.
///
/// A newer-than-known schema is read with the schema-2 defaults and reported ONCE per
/// process — the alternative (refusing to read) would make the overlay unrenderable.
#[must_use]
pub(crate) fn read_text_params(
    obj: &Map<String, Value>,
) -> Cow<'_, Map<String, Value>> {
    let version = text_params_schema_version(obj);
    if version < 2 {
        return Cow::Borrowed(obj);
    }
    if version > TEXT_PARAMS_SCHEMA_VERSION && !FUTURE_SCHEMA_REPORTED.swap(true, Ordering::Relaxed)
    {
        ms_log::runtime_log::log_warn(format!(
            "text_params: document declares schema {version}, this build knows \
             {TEXT_PARAMS_SCHEMA_VERSION}; unknown keys are kept verbatim and missing ones are \
             filled with the schema-{TEXT_PARAMS_SCHEMA_VERSION} defaults. Reported once per \
             process."
        ));
    }
    let mut filled = obj.clone();
    for (key, default) in frozen_v2_defaults() {
        if !filled.contains_key(key) {
            filled.insert(key.clone(), default.clone());
        }
    }
    Cow::Owned(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both historical representations of `hanging_punctuation` must read back as the
    /// same `0.0..=1.0` strength, out-of-range values must be clamped, and anything that
    /// is not a number or a bool must be rejected so the caller keeps its own default.
    #[test]
    fn hanging_punctuation_accepts_both_representations() {
        // Legacy bool: `true` was fully hanging, `false` was off.
        assert_eq!(hanging_punctuation_weight(&json!(true)), Some(1.0));
        assert_eq!(hanging_punctuation_weight(&json!(false)), Some(0.0));
        // Current numeric representation, including a fraction and a JSON INTEGER `1`
        // (which `serde_json` keeps as an integer, not a float).
        assert_eq!(hanging_punctuation_weight(&json!(1.0)), Some(1.0));
        assert_eq!(hanging_punctuation_weight(&json!(0.0)), Some(0.0));
        assert_eq!(hanging_punctuation_weight(&json!(0.75)), Some(0.75));
        assert_eq!(hanging_punctuation_weight(&json!(1)), Some(1.0));
        // Out of range in both directions is clamped, never rejected: a value outside
        // `0.0..=1.0` still expresses an intent ("off" / "fully hanging").
        assert_eq!(hanging_punctuation_weight(&json!(5.0)), Some(1.0));
        assert_eq!(hanging_punctuation_weight(&json!(-3.0)), Some(0.0));
        // Anything else is NOT a strength; the caller must fall back to its own default.
        assert_eq!(hanging_punctuation_weight(&json!("nan")), None);
        assert_eq!(hanging_punctuation_weight(&json!(null)), None);
        assert_eq!(hanging_punctuation_weight(&json!([1.0])), None);
        // An absent key never reaches the helper — the `.get(..).and_then(..)` chain of
        // both readers short-circuits — which is what makes `None` mean "keep default".
        let obj = json!({ "text": "x" });
        assert_eq!(
            obj.get("hanging_punctuation").and_then(hanging_punctuation_weight),
            None
        );
    }

    /// The WRITER must emit the two exact endpoints as the LEGACY BOOL, so a project
    /// saved by this build still renders correctly in an older one (which reads the key
    /// with `Value::as_bool`) and a value left at the frozen default is still omitted.
    /// Only a strictly fractional strength becomes a number, and every shape must survive
    /// the write -> read round trip as the identical `f32`.
    #[test]
    fn hanging_punctuation_writes_the_endpoints_as_the_legacy_bool() {
        assert_eq!(hanging_punctuation_value(1.0), json!(true));
        assert_eq!(hanging_punctuation_value(0.0), json!(false));
        // `-0.0` and out-of-range values are clamped onto an endpoint, so they take the
        // bool branch too rather than leaking a stray number.
        assert_eq!(hanging_punctuation_value(-0.0), json!(false));
        assert_eq!(hanging_punctuation_value(-3.0), json!(false));
        assert_eq!(hanging_punctuation_value(5.0), json!(true));
        // NaN is "off" (`clamp_hanging_weight` rejects it), never a JSON null.
        assert_eq!(hanging_punctuation_value(f32::NAN), json!(false));
        // A strictly fractional strength is the only case that needs a number.
        assert!(hanging_punctuation_value(0.4).is_number());
        assert!(hanging_punctuation_value(0.75).is_number());

        // Round trip through the real writer, not a hand-written literal.
        for weight in [0.0_f32, 1.0, 0.4, 0.75, 0.05] {
            assert_eq!(
                hanging_punctuation_weight(&hanging_punctuation_value(weight)),
                Some(weight),
                "write -> read must be lossless for {weight}"
            );
        }

        // THE POINT OF THE BOOL WRITER: a document at the frozen default omits the key.
        let mut at_default = Map::new();
        at_default.insert("text".to_string(), json!("x"));
        at_default.insert(
            "hanging_punctuation".to_string(),
            hanging_punctuation_value(1.0),
        );
        let written = write_text_params(at_default);
        assert_eq!(
            written.get("hanging_punctuation"),
            None,
            "a fully-hanging value equals the frozen default and must not be written"
        );
    }

    /// Pins EVERY frozen schema-2 default. A default may not change without bumping
    /// `TEXT_PARAMS_SCHEMA_VERSION` and adding a read branch for the old version —
    /// otherwise every already-written document that OMITS the key silently changes
    /// meaning. If this test fails, that is the decision being made.
    #[test]
    fn defaults_are_frozen() {
        let defaults = frozen_v2_defaults();
        let expected: [(&str, Value); 45] = [
            ("text", json!("")),
            ("text_color", json!([0, 0, 0, 255])),
            ("font_size_px", json!(24.0)),
            ("line_spacing", json!("0.00%")),
            ("kerning_mode", json!("auto")),
            ("kerning", json!("0.00%")),
            ("glyph_height", json!("100.00%")),
            ("glyph_width", json!("100.00%")),
            ("width_px", json!(300)),
            ("align", json!("center")),
            ("align_bias", json!(0.0)),
            ("global_rotation_deg", json!(0.0)),
            ("line_placement_percent", json!(0.0)),
            ("line_placement_reference", json!("line_box")),
            ("text_line_mode", json!("horizontal")),
            ("vertical_line_direction", json!("right_to_left")),
            ("text_layout_mode", json!("normal")),
            (
                "formula_layout",
                json!({
                    "x_expr": "t * w",
                    "y_expr": "0",
                    "rotation_expr": "0",
                    "use_tangent_rotation": false,
                    "t_start": 0.0,
                    "t_end": 1.0,
                    "offset_x_px": 0.0,
                    "offset_y_px": 0.0,
                    "scale_x": 1.0,
                    "scale_y": 1.0,
                    "normal_offset_px": 0.0,
                    "letter_spacing_mul": 1.0,
                    "letter_spacing_px": 0.0,
                    "vars": [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
                }),
            ),
            (
                "shape_layout",
                json!({
                    "kind": "arc",
                    "length_px": 320.0,
                    "amplitude_px": 80.0,
                    "width_px": 320.0,
                    "height_px": 80.0,
                    "frequency": 1.0,
                    "orientation": "horizontal",
                }),
            ),
            (
                "drawn_lines_layout",
                json!({
                    "use_tangent_rotation": true,
                    "static_rotation_rad": 0.0,
                    "normal_offset_px": 0.0,
                    "letter_spacing_mul": 1.0,
                    "letter_spacing_px": 0.0,
                    "color_tolerance": 16,
                    "continuation_alpha": 64,
                    "start_alpha": 192,
                }),
            ),
            (
                "vector_lines_layout",
                json!({
                    "width_px": 1,
                    "height_px": 1,
                    "use_tangent_rotation": true,
                    "static_rotation_rad": 0.0,
                    "normal_offset_px": 0.0,
                    "letter_spacing_mul": 1.0,
                    "letter_spacing_px": 0.0,
                    "lines": [],
                }),
            ),
            ("selected_face_index", json!(0)),
            ("force_bold", json!(false)),
            ("force_italic", json!(false)),
            ("faux_bold", json!(false)),
            ("faux_bold_thicken_percent", json!(3.0)),
            ("faux_bold_expand_percent", json!(0.0)),
            ("faux_bold_sharp_corners", json!(true)),
            ("faux_bold_outward_only", json!(true)),
            ("faux_italic", json!(false)),
            ("faux_italic_slant_deg", json!(14.0)),
            ("uppercase_text", json!(false)),
            ("trim_extra_spaces", json!(true)),
            ("replace_ellipsis_with_dots", json!(true)),
            ("force_remove_ellipsis_glyph", json!(false)),
            // Still a bool although the panel field is an `f32` strength — see the
            // matching comment in `frozen_v2_defaults`: the frozen value is preserved
            // so no already-written document changes meaning, and the writer emits the
            // endpoints in this same shape so the default stays omissible.
            ("hanging_punctuation", json!(true)),
            ("new_line_after_sentence", json!(false)),
            ("enable_inline_style_tags", json!(false)),
            ("text_wrap_mode", json!("aggressive")),
            ("anti_aliasing", json!("strong")),
            ("allow_moderate_trees", json!(false)),
            ("text_shape", json!("free")),
            ("shape_min_width_percent", json!(50.0)),
            ("shape_variant", json!(5)),
            ("formed_text", json!("")),
        ];
        for (key, value) in &expected {
            assert_eq!(
                defaults.get(*key),
                Some(value),
                "frozen schema-2 default for '{key}' changed"
            );
        }
        assert_eq!(
            defaults.len(),
            expected.len(),
            "the frozen default set gained or lost a key; pin it here as well"
        );
    }

    /// `font` has NO default: an overlay's font identity is never implied.
    #[test]
    fn font_identity_has_no_default() {
        assert!(frozen_v2_defaults().get("font").is_none());
    }

    #[test]
    fn version_defaults_to_one_without_the_key() {
        let obj = json!({ "text": "x" });
        let obj = obj.as_object().expect("object literal");
        assert_eq!(text_params_schema_version(obj), 1);
        let v2 = json!({ "schema": 2 });
        assert_eq!(
            text_params_schema_version(v2.as_object().expect("object literal")),
            2
        );
    }

    /// Building a map that is all defaults writes only the essentials plus the version.
    #[test]
    fn write_skips_defaults_and_stamps_the_version() {
        let mut params = frozen_v2_defaults().clone();
        params.insert("font".to_string(), json!("CCWildWords-Regular"));
        params.insert("text".to_string(), json!("Привет"));
        let written = write_text_params(params);
        let obj = written.as_object().expect("written object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["font", "schema", "text", "width_px"]);
        assert_eq!(obj.get("schema"), Some(&json!(2)));
    }

    #[test]
    fn write_drops_dead_and_legacy_font_keys_and_nulls() {
        let mut params = Map::new();
        params.insert("font".to_string(), json!("Some-Font"));
        params.insert("text".to_string(), json!("t"));
        params.insert("width_px".to_string(), json!(300));
        params.insert("strict_shape_fit".to_string(), json!(true));
        params.insert("aggressive_word_breaks".to_string(), json!(true));
        params.insert("font_path".to_string(), json!("/tmp/font.ttf"));
        params.insert("font_label".to_string(), json!("font"));
        params.insert("font_original_name".to_string(), json!("Font Family"));
        params.insert("font_family".to_string(), json!("Font Family"));
        params.insert("raster_transform".to_string(), Value::Null);
        let written = write_text_params(params);
        let obj = written.as_object().expect("written object");
        for gone in [
            "strict_shape_fit",
            "aggressive_word_breaks",
            "font_path",
            "font_label",
            "font_original_name",
            "font_family",
            "raster_transform",
        ] {
            assert!(!obj.contains_key(gone), "'{gone}' must not be written");
        }
    }

    /// A non-default value survives the strip, and reading fills the rest back in.
    #[test]
    fn write_then_read_round_trips_every_field() {
        let mut params = frozen_v2_defaults().clone();
        params.insert("font".to_string(), json!("Some-Font"));
        // One changed value per JSON kind: string, bool, float, int, array, object.
        params.insert("text".to_string(), json!("hello"));
        params.insert("uppercase_text".to_string(), json!(true));
        params.insert("font_size_px".to_string(), json!(42.5));
        params.insert("shape_variant".to_string(), json!(7));
        params.insert("text_color".to_string(), json!([1, 2, 3, 4]));
        params.insert("line_placement_reference".to_string(), json!("glyph_height"));
        let mut expected = params.clone();
        expected.insert("schema".to_string(), json!(2));

        let written = write_text_params(params);
        let obj = written.as_object().expect("written object");
        assert!(obj.len() < expected.len(), "defaults must actually be skipped");
        let filled = read_text_params(obj);
        assert_eq!(*filled, expected, "every field must survive write -> read");
    }

    /// A schema-1 document is handed to the legacy readers UNCHANGED: filling today's
    /// defaults into it would silently restyle text written before those defaults
    /// existed (e.g. `line_placement_reference`, whose legacy meaning is `glyph_height`).
    #[test]
    fn read_leaves_a_schema_one_document_untouched() {
        let doc = json!({ "text": "x", "font_label": "Some-Font" });
        let obj = doc.as_object().expect("object literal");
        let read = read_text_params(obj);
        assert!(matches!(read, Cow::Borrowed(_)));
        assert_eq!(*read, *obj);
    }

    /// Writing is idempotent: re-writing an already-written payload changes no byte.
    #[test]
    fn write_is_idempotent() {
        let mut params = frozen_v2_defaults().clone();
        params.insert("font".to_string(), json!("Some-Font"));
        params.insert("text".to_string(), json!("hello"));
        let once = write_text_params(params);
        let obj = once.as_object().expect("written object").clone();
        let twice = write_text_params(obj);
        assert_eq!(
            serde_json::to_string(&once).ok(),
            serde_json::to_string(&twice).ok()
        );
    }
}
