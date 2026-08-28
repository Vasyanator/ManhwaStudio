/*
File: tab/codec.rs

Purpose:
Serialization/parsing helpers for the typing tab: converting between stored
overlay JSON (render data / render params) and the in-memory typed parameter
structs, plus the legacy-format normalization and storage-entry
encode/decode/normalize routines.

Main responsibilities:
- parse `render_data` / render-param JSON into `TextRenderParams` and the
  per-layout parameter structs, through the schema owner
  (`panel/text_params_schema.rs`) so the document's OWN schema decides its defaults;
- parse config-string enums (shape, wrap mode, anti-aliasing, line mode, etc.);
- build, normalize, and decode overlay storage entries, including legacy formats;
- convert a SCHEMA-1 `text_params` payload to the current schema
  (`upgrade_text_params_to_v2`, pure) and apply that conversion to the resident
  overlays (`TypingTextOverlayLayer::convert_legacy_text_params_to_v2`).

Notes:
Extracted verbatim from `tab.rs`. Free fns are `pub(super)` so `tab.rs` and
sibling submodules of `tab` can use them. `use super::*;` pulls in the parent
module's types and imports.

The legacy READ chain (`font_original_name -> font_label -> font_family -> font ->
file stem of font_path`, and every other schema-1 fallback here) is kept FOREVER:
a project written years ago must still open. Only the WRITE side is allowed to
forget a format. The chain's ORDER belongs to the schema owner
(`text_params_schema::legacy_font_name_candidates`), shared with the PSD export.

Two rules govern the conversion, both about not destroying user data:
- an unresolvable font leaves the payload completely untouched;
- resolution goes BY NAME, walking the whole chain; a stored path is a weak hint
  and never proof of identity (safety rule D, `upgrade_text_params_to_v2`).
And one rule governs the legacy NORMALIZER: it rebuilds `text_params` from a
whitelist, so every key of the frozen schema-2 default set must appear in it or the
stored value dies on load.
*/

use super::*;
use crate::tabs::typing::panel::text_params_schema;
use serde_json::Map;

/// Decodes a stored render-data object — `{"text_params": {…}, "effects": […]}` — into the
/// renderer's `TextRenderParams`, folding the `effects` array into `effects_json` so the
/// whole effect chain travels with the parameters.
///
/// `text_params` is read through the schema owner, so a schema-2 payload gets the frozen
/// defaults for its omitted keys and a schema-1 payload keeps the legacy per-field
/// fallbacks (including the historical font-name chain). `formed_text`, when non-empty,
/// replaces `text` and forces `TextWrapMode::None`, matching what the panel renders.
///
/// Returns `None` when the value is not an object, carries no `text_params` object, or
/// names no font at all — the ONE thing that cannot be defaulted.
///
/// `pub(in crate::tabs::typing)` rather than `pub(super)`: besides `tab` and its submodules
/// (the overlay render pipeline and the shape-variant preview grid), the typing panel's
/// local-preset preview cache (`panel/local_preset_preview.rs`) decodes preset profiles
/// through it, so the preview is rendered by exactly the same parameter path as the canvas.
#[must_use]
pub(in crate::tabs::typing) fn text_render_params_from_render_data(
    render_data: &Value,
) -> Option<TextRenderParams> {
    let render_obj = render_data.as_object()?;
    let stored_params = render_obj.get("text_params")?.as_object()?;
    // Read every parameter through the schema owner: a schema-2 payload omits values
    // equal to their FROZEN defaults, which are materialized here; a schema-1 payload is
    // passed through untouched so the legacy per-field defaults below keep applying.
    let filled_params = text_params_schema::read_text_params(stored_params);
    let text_params = &*filled_params;
    let read_name = |key: &str| {
        text_params
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    };
    // Schema 2 names the font ONCE, by identity, in `font`. Schema 1 is resolved through
    // the historical chain — `font_original_name` (family), `font_label` (the identity on
    // late v1 data, a stem/label on old data), `font_family`, `font`, then the `font_path`
    // file stem — every form of which `TabFontProvider` still keeps as a READ-ONLY alias.
    // Bail only when NONE of them yields a non-empty name.
    let font_name = if text_params_schema::text_params_schema_version(text_params)
        >= text_params_schema::TEXT_PARAMS_SCHEMA_VERSION
    {
        read_name("font")?
    } else {
        legacy_font_name_from_text_params(text_params)?
    };
    let effects_json = render_obj
        .get("effects")
        .and_then(Value::as_array)
        .map(|effects| Value::Array(effects.clone()))
        .and_then(|effects| serde_json::to_string(&effects).ok())
        .unwrap_or_default();

    // Сформированный текст (если задан) идёт в рендер вместо исходного, без
    // повторного авто-переноса.
    let formed_text = text_params
        .get("formed_text")
        .and_then(Value::as_str)
        .filter(|formed| !formed.trim().is_empty());
    let source_text = text_params
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let uses_formed = formed_text.is_some();
    let render_text = formed_text.unwrap_or(source_text).to_string();

    let font_size_px = text_params
        .get("font_size_px")
        .and_then(value_as_f32)
        .unwrap_or(24.0)
        .max(1.0);
    // Единое представление `px-или-%`: новый строковый ключ либо устаревшая пара.
    let line_spacing = read_render_param_px_or_percent(
        text_params,
        "line_spacing",
        "line_spacing_px",
        "line_spacing_percent",
        PxOrPercent::percent(50.0),
    );
    let kerning = read_render_param_px_or_percent(
        text_params,
        "kerning",
        "kerning_px",
        "kerning_percent",
        PxOrPercent::percent(0.0),
    );
    let glyph_height = read_render_param_px_or_percent(
        text_params,
        "glyph_height",
        "",
        "glyph_height_percent",
        PxOrPercent::percent(100.0),
    );
    let glyph_width = read_render_param_px_or_percent(
        text_params,
        "glyph_width",
        "",
        "glyph_width_percent",
        PxOrPercent::percent(100.0),
    );

    Some(TextRenderParams {
        text: render_text,
        text_color: text_params
            .get("text_color")
            .and_then(parse_rgba_value)
            .unwrap_or([0, 0, 0, 255]),
        font_name,
        font_size_px,
        line_spacing_px: line_spacing.as_px_percent().0,
        line_spacing_percent: line_spacing.as_px_percent().1,
        kerning_mode: text_params
            .get("kerning_mode")
            .and_then(Value::as_str)
            .and_then(parse_kerning_mode_config_str)
            .unwrap_or(KerningMode::Auto),
        kerning_px: kerning.as_px_percent().0,
        kerning_percent: kerning.as_px_percent().1,
        glyph_height_percent: glyph_height.as_percent_of(font_size_px),
        glyph_width_percent: glyph_width.as_percent_of(font_size_px),
        width_px: text_params
            .get("width_px")
            .and_then(value_as_f32)
            .map(|value| value.round().max(1.0) as u32)
            .unwrap_or(TEXT_RENDER_DATA_FALLBACK_WIDTH_PX),
        align: HorizontalAlign::from_config(
            text_params.get("align").and_then(Value::as_str),
            text_params.get("align_bias").and_then(value_as_f32),
        ),
        // Global vector rotation of the whole block; absent in old projects -> 0.
        global_rotation_deg: text_params
            .get("global_rotation_deg")
            .and_then(value_as_f32)
            .unwrap_or(0.0),
        // Perpendicular line placement; absent in projects saved before it -> 0.
        line_placement_percent: text_params
            .get("line_placement_percent")
            .and_then(value_as_f32)
            .unwrap_or(0.0),
        // Reference band the line placement snaps to. Absent in projects saved before
        // the option -> legacy per-glyph anchoring (GlyphHeight).
        line_placement_reference: text_params
            .get("line_placement_reference")
            .and_then(|value| value.as_str())
            .map_or(LinePlacementReference::GlyphHeight, |token| {
                LinePlacementReference::from_json_str(token)
            }),
        // Vector mesh warp authored on the canvas (Phase 3); carried verbatim
        // through render_data. Absent/invalid -> None (identity / no warp).
        raster_transform: text_params
            .get("raster_transform")
            .and_then(decode_vector_mesh_warp),
        selected_face_index: text_params
            .get("selected_face_index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(0),
        force_bold: text_params
            .get("force_bold")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        force_italic: text_params
            .get("force_italic")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        faux_bold: (text_params.get("force_bold").and_then(Value::as_bool).unwrap_or(false)
            && text_params.get("faux_bold").and_then(Value::as_bool).unwrap_or(false))
            .then(|| crate::tabs::typing::render_next::types::FauxBoldParams {
                // SIGNED: a negative stored value is a legitimate THINNING request.
                thicken_percent: text_params.get("faux_bold_thicken_percent").and_then(value_as_f32).unwrap_or(3.0).clamp(FAUX_THICKEN_PERCENT_MIN, FAUX_THICKEN_PERCENT_MAX),
                expand_percent: text_params.get("faux_bold_expand_percent").and_then(value_as_f32).unwrap_or(0.0).clamp(0.0, 50.0),
                sharp_corners: text_params.get("faux_bold_sharp_corners").and_then(Value::as_bool).unwrap_or(true),
                outward_only: text_params.get("faux_bold_outward_only").and_then(Value::as_bool).unwrap_or(true),
            }),
        faux_italic_slant_deg: (text_params.get("force_italic").and_then(Value::as_bool).unwrap_or(false)
            && text_params.get("faux_italic").and_then(Value::as_bool).unwrap_or(false))
            .then_some(text_params.get("faux_italic_slant_deg").and_then(value_as_f32).unwrap_or(14.0).clamp(-45.0, 45.0)),
        uppercase_text: text_params
            .get("uppercase_text")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        trim_extra_spaces: text_params
            .get("trim_extra_spaces")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        // Off when the key is absent, like every other processing flag here: a card
        // saved before this option existed must keep rendering its `…` untouched.
        // New cards always carry the key (the panel writes it), so the panel-side
        // default of `on` is unaffected.
        replace_ellipsis_with_dots: text_params
            .get("replace_ellipsis_with_dots")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        // Sub-parameter of the flag above, and the AND is what the renderer's contract
        // asks for: the GSUB patch only makes sense on text the substitution already
        // rewrote. Absent = off, which matches its frozen schema-2 default, so no card
        // written before this option existed changes.
        force_remove_ellipsis_glyph: text_params
            .get("replace_ellipsis_with_dots")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && text_params
                .get("force_remove_ellipsis_glyph")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        hanging_punctuation: text_params
            .get("hanging_punctuation")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        new_line_after_sentence: text_params
            .get("new_line_after_sentence")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        enable_inline_style_tags: text_params
            .get("enable_inline_style_tags")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        text_wrap_mode: if uses_formed {
            TextWrapMode::None
        } else {
            text_params
                .get("text_wrap_mode")
                .and_then(Value::as_str)
                .and_then(parse_text_wrap_mode_config_str)
                .unwrap_or(TextWrapMode::Aggressive)
        },
        text_shape: text_params
            .get("text_shape")
            .and_then(Value::as_str)
            .and_then(parse_text_shape_config_str)
            .unwrap_or(TextShape::Rectangle),
        shape_min_width_percent: text_params
            .get("shape_min_width_percent")
            .and_then(value_as_f32)
            .unwrap_or(50.0),
        shape_variant: text_params
            .get("shape_variant")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(5)
            .clamp(1, 9),
        compare_shape_with: None,
        allow_moderate_trees: text_params
            .get("allow_moderate_trees")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        text_line_mode: text_params
            .get("text_line_mode")
            .and_then(Value::as_str)
            .and_then(parse_text_line_mode_config_str)
            .unwrap_or(TextLineMode::Horizontal),
        vertical_line_direction: text_params
            .get("vertical_line_direction")
            .and_then(Value::as_str)
            .and_then(parse_vertical_line_direction_config_str)
            .unwrap_or(VerticalLineDirection::RightToLeft),
        text_layout_mode: text_params
            .get("text_layout_mode")
            .and_then(Value::as_str)
            .and_then(parse_text_layout_mode_config_str)
            .unwrap_or(TextLayoutMode::Normal),
        formula_layout: text_formula_layout_params_from_value(text_params.get("formula_layout")),
        drawn_lines_layout: text_drawn_lines_layout_params_from_value(
            text_params.get("drawn_lines_layout"),
        ),
        vector_lines_layout: text_vector_lines_layout_params_from_value(
            text_params.get("vector_lines_layout"),
        ),
        effects_json,
        anti_aliasing: text_params
            .get("anti_aliasing")
            .and_then(Value::as_str)
            .and_then(parse_anti_aliasing_config_str)
            .unwrap_or(AntiAliasingMode::Strong),
        // Extra render info (mean/median centers) is a per-render compute request,
        // not persisted state; decoded params always start with nothing requested.
        extra_info: crate::tabs::typing::render_next::types::RenderExtraInfoRequest::default(),
    })
}

/// The `font_path` a SCHEMA-1 `text_params` object carries; see
/// [`text_params_schema::legacy_font_path`], which owns the legacy read contract.
#[must_use]
pub(super) fn legacy_font_path_from_text_params(obj: &Map<String, Value>) -> Option<&str> {
    text_params_schema::legacy_font_path(obj)
}

/// The FIRST name of [`text_params_schema::legacy_font_name_candidates`], i.e. the
/// historical "best" name of a SCHEMA-1 payload. Used where a single name is all the
/// contract allows (the decoded `TextRenderParams.font_name`, the missing-font UI text);
/// anything that RESOLVES must walk the full candidate list instead, because the first
/// non-empty name is routinely a family name the machine no longer has. `None` when no
/// font is named.
#[must_use]
pub(super) fn legacy_font_name_from_text_params(obj: &Map<String, Value>) -> Option<String> {
    text_params_schema::legacy_font_name_candidates(obj)
        .into_iter()
        .next()
}

/// Resolves a font reference persisted by an OLDER build — `(font_path, font_name)`, each
/// optional — to the current render IDENTITY, or `None` when no installed font matches.
///
/// Implemented by the typing panel (`resolve_legacy_font_identity`), which owns the font
/// list; the codec takes it as a parameter so the conversion stays a pure function.
///
/// The conversion never supplies both at once — see [`upgrade_text_params_to_v2`], safety
/// rule D: each stored NAME is offered alone, and the path only afterwards, so the answer
/// always says which kind of evidence matched. (The panel's own resolver ranks names above
/// the path too since phase 5, but this contract does not depend on that.)
pub(in crate::tabs::typing) type LegacyFontIdentityResolver<'a> =
    &'a dyn Fn(Option<&str>, Option<&str>) -> Option<String>;

/// Outcome of upgrading one stored `text_params` object to the current schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tabs::typing) enum TextParamsUpgrade {
    /// The payload already declares the current schema — nothing to do, nothing to save.
    AlreadyCurrent,
    /// Converted. The caller stores this object and marks the overlay changed so the
    /// normal (deferred) save path writes it; nothing is written here.
    Converted(Value),
    /// A schema-1 payload whose font is NOT installed. NOTHING is converted: the legacy
    /// keys are the only surviving record of which font the text was set in, and the
    /// conversion must never destroy them. `legacy_name` is the best name for the
    /// missing-font UI (`None` when the payload names no font at all).
    UnresolvedFont { legacy_name: Option<String> },
    /// A schema-1 payload none of whose stored NAMES resolves, while its stored
    /// `font_path` still points at an installed font — a DIFFERENT one than the names
    /// describe, or the same font under a name this build no longer knows. Nothing is
    /// converted (safety rule D, see [`upgrade_text_params_to_v2`]): a file that happens
    /// to sit at a remembered path is not proof of identity, and rewriting the layer to
    /// `path_identity` would erase the only record of the font it was really set in.
    ///
    /// `legacy_name` is the layer's best stored name (`None` when it names none) and
    /// `path_identity` the identity of whatever now lives at that path — both are for the
    /// user-facing "needs attention" report, not for storing.
    PathOnlyFont {
        legacy_name: Option<String>,
        path_identity: String,
    },
}

/// Converts a SCHEMA-1 `text_params` object to schema 2, resolving its legacy font
/// reference to a font IDENTITY through `resolve_identity`.
///
/// `resolve_identity(font_path, font_name)` is the panel's legacy resolver
/// (`resolve_legacy_font_identity`).
///
/// **Safety rule D — the conversion resolves by NAME; a path is only a weak hint.** Every
/// stored name is tried IN ORDER (`legacy_font_name_candidates_from_text_params`), each on
/// its own, and the first that resolves supplies the identity. The stored `font_path` is
/// never allowed to decide the outcome, because a file that still sits at a remembered
/// path is not proof that it is the same font: replacing `dialogue.ttf` with another face
/// would make the conversion write the NEW font's identity and drop the legacy keys, which
/// destroys the only surviving record of the real one. It is also not a loss to ignore:
/// rendering resolves a v1 layer by NAME too (`text_render_params_from_render_data`), so a
/// payload whose names do not resolve does not render either way and has nothing to gain
/// from being converted. A path that DOES resolve while no name does is reported as
/// [`TextParamsUpgrade::PathOnlyFont`] — not converted, flagged for the user.
///
/// When nothing resolves at all the payload is left completely untouched — see
/// [`TextParamsUpgrade::UnresolvedFont`].
///
/// The conversion is meaning-preserving, not merely mechanical:
/// - the legacy `*_px`/`*_percent` pairs are folded into their single token key WITHOUT
///   losing precision ([`PxOrPercent::to_token_lossless`]), so dropping them cannot change
///   spacing;
/// - every key whose SCHEMA-1 absent-meaning differs from the frozen schema-2 default is
///   materialized first (`line_placement_reference`, `trim_extra_spaces`,
///   `replace_ellipsis_with_dots`, `hanging_punctuation`, `text_shape`, `width_px`), so
///   an old payload that omitted it keeps rendering exactly as before;
/// - the ancient `"smart"` wrap token is resolved through
///   [`normalize_text_wrap_mode_legacy`] BEFORE its input `aggressive_word_breaks` is
///   dropped as a dead key.
///
/// Idempotent: a payload it produced reports [`TextParamsUpgrade::AlreadyCurrent`].
#[must_use]
pub(in crate::tabs::typing) fn upgrade_text_params_to_v2(
    text_params: &Map<String, Value>,
    resolve_identity: LegacyFontIdentityResolver<'_>,
) -> TextParamsUpgrade {
    if text_params_schema::text_params_schema_version(text_params)
        >= text_params_schema::TEXT_PARAMS_SCHEMA_VERSION
    {
        return TextParamsUpgrade::AlreadyCurrent;
    }
    // Safety rule D: names decide, the path never does. Each candidate is offered ALONE
    // (path = `None`), because the panel's resolver gives a supplied path absolute
    // priority and would otherwise answer with whatever file now sits there.
    let candidates = text_params_schema::legacy_font_name_candidates(text_params);
    let legacy_name = candidates.first().cloned();
    let Some(identity) = candidates
        .iter()
        .find_map(|name| resolve_identity(None, Some(name.as_str())))
    else {
        // Nothing resolved by name. A path that still resolves means the file is there but
        // is not evidence of identity: report it so the user can repair the layer, and keep
        // every legacy key verbatim.
        return match legacy_font_path_from_text_params(text_params)
            .and_then(|path| resolve_identity(Some(path), None))
        {
            Some(path_identity) => TextParamsUpgrade::PathOnlyFont {
                legacy_name,
                path_identity,
            },
            None => TextParamsUpgrade::UnresolvedFont { legacy_name },
        };
    };

    let mut params = text_params.clone();
    // 1. Fold the legacy px/percent PAIRS into the single token key, then drop them:
    //    the pair carries real values that the schema-2 reader would otherwise lose.
    for (token_key, legacy_px_key, legacy_percent_key, default) in [
        (
            "line_spacing",
            "line_spacing_px",
            "line_spacing_percent",
            PxOrPercent::percent(50.0),
        ),
        ("kerning", "kerning_px", "kerning_percent", PxOrPercent::percent(0.0)),
        ("glyph_height", "", "glyph_height_percent", PxOrPercent::percent(100.0)),
        ("glyph_width", "", "glyph_width_percent", PxOrPercent::percent(100.0)),
    ] {
        let value = read_render_param_px_or_percent(
            text_params,
            token_key,
            legacy_px_key,
            legacy_percent_key,
            default,
        );
        // LOSSLESS token: the canonical two-decimal `to_token` would silently round a
        // stored `line_spacing_px: 1.2345` to `"1.23"`, and the legacy pair it replaces is
        // then dropped — an irreversible geometry change. `to_token_lossless` keeps the
        // canonical spelling whenever it round-trips and only widens when it would not.
        params.insert(token_key.to_string(), Value::String(value.to_token_lossless()));
        params.remove(legacy_px_key);
        params.remove(legacy_percent_key);
    }
    // 2. Resolve the ancient `"smart"` wrap token while its inputs are still here.
    if params
        .get("text_wrap_mode")
        .and_then(Value::as_str)
        .map(str::trim)
        .is_some_and(|token| token.eq_ignore_ascii_case("smart"))
    {
        let resolved = normalize_text_wrap_mode_legacy(
            Some("smart"),
            text_params.get("aggressive_word_breaks").and_then(Value::as_bool),
            text_params.get("allow_moderate_trees").and_then(Value::as_bool),
        );
        params.insert(
            "text_wrap_mode".to_string(),
            Value::String(resolved.to_string()),
        );
    }
    // 3. Materialize the keys whose schema-1 absent-meaning is NOT the frozen schema-2
    //    default, so omitting them in schema 2 cannot change what the overlay looks like.
    for (key, legacy_absent_meaning) in [
        ("line_placement_reference", json!("glyph_height")),
        ("trim_extra_spaces", json!(false)),
        ("replace_ellipsis_with_dots", json!(false)),
        ("hanging_punctuation", json!(false)),
        ("text_shape", json!("rectangle")),
        ("width_px", json!(TEXT_RENDER_DATA_FALLBACK_WIDTH_PX)),
    ] {
        params
            .entry(key.to_string())
            .or_insert(legacy_absent_meaning);
    }
    // 4. The single font key of schema 2. `write_text_params` drops the legacy ones.
    params.insert("font".to_string(), Value::String(identity));
    TextParamsUpgrade::Converted(text_params_schema::write_text_params(params))
}

/// Why one overlay was left unconverted, for the once-per-overlay report.
///
/// Kept apart from [`TextParamsUpgrade`] so the reporting loop borrows nothing from the
/// overlay list it walked.
#[derive(Debug, Clone)]
enum UnconvertedTextParams {
    /// No stored name resolves and no stored path resolves either — the font is simply
    /// not installed. `legacy_name` is the best stored name, `None` when none is stored.
    NoFontInstalled { legacy_name: Option<String> },
    /// No stored NAME resolves, but the stored path does — to `path_identity`. Safety
    /// rule D: not converted, and the user is told, because the file at that path may be
    /// a different font entirely.
    PathOnlyMatch {
        legacy_name: Option<String>,
        path_identity: String,
    },
}

impl TypingTextOverlayLayer {
    /// Converts every resident TEXT overlay still carrying a SCHEMA-1 `text_params`
    /// payload to schema 2, IN MEMORY, and marks the layer dirty so the normal deferred
    /// save writes it. Writes nothing itself.
    ///
    /// `resolve_identity` is the panel's legacy font resolver (path and/or any historical
    /// name form → the current identity). An overlay whose font is not installed is left
    /// COMPLETELY untouched — its legacy keys are the only record of which font it used —
    /// and reported once per overlay. So is an overlay that only matches through its
    /// stored PATH (safety rule D in [`upgrade_text_params_to_v2`]): it is reported as
    /// needing the user's attention, naming the font that now occupies that path.
    ///
    /// Called once per frame from the tab: overlays materialize lazily (a doc page is
    /// projected the first time it is visited), so a single pass at load time would miss
    /// most of them. The pass is idempotent and cheap — one map lookup per overlay once
    /// the payloads are current — and a document that is ALREADY schema 2 is never
    /// touched, so opening a converted project writes nothing.
    pub(in crate::tabs::typing) fn convert_legacy_text_params_to_v2(
        &mut self,
        resolve_identity: LegacyFontIdentityResolver<'_>,
    ) {
        let mut converted: Vec<(usize, Value)> = Vec::new();
        let mut unresolved: Vec<(String, UnconvertedTextParams)> = Vec::new();
        for (idx, overlay) in self.overlays.iter().enumerate() {
            if overlay.kind != TypingOverlayKind::Text {
                continue;
            }
            let Some(render_data) = overlay.render_data_json.as_ref() else {
                continue;
            };
            let Some(text_params) = render_data.get("text_params").and_then(Value::as_object) else {
                continue;
            };
            match upgrade_text_params_to_v2(text_params, resolve_identity) {
                TextParamsUpgrade::AlreadyCurrent => {}
                TextParamsUpgrade::Converted(params) => {
                    let mut updated = render_data.clone();
                    let Some(obj) = updated.as_object_mut() else {
                        continue;
                    };
                    obj.insert("text_params".to_string(), params);
                    converted.push((idx, updated));
                }
                TextParamsUpgrade::UnresolvedFont { legacy_name } => {
                    if !self.legacy_text_params_unresolved.contains(&overlay.uid) {
                        unresolved.push((
                            overlay.uid.clone(),
                            UnconvertedTextParams::NoFontInstalled { legacy_name },
                        ));
                    }
                }
                TextParamsUpgrade::PathOnlyFont {
                    legacy_name,
                    path_identity,
                } => {
                    if !self.legacy_text_params_unresolved.contains(&overlay.uid) {
                        unresolved.push((
                            overlay.uid.clone(),
                            UnconvertedTextParams::PathOnlyMatch {
                                legacy_name,
                                path_identity,
                            },
                        ));
                    }
                }
            }
        }
        for (uid, reason) in unresolved {
            match reason {
                UnconvertedTextParams::NoFontInstalled {
                    legacy_name: Some(name),
                } => crate::runtime_log::log_warn(format!(
                    "[typing] text layer '{uid}': the font it was set in ('{name}') is not \
                     installed, so its legacy font keys are kept verbatim and it is NOT converted \
                     to the current text_params schema. Install the font (or re-pick one for the \
                     layer) to convert it."
                )),
                // A payload that names no font at all is not a user-visible problem (it
                // cannot render either way) — keep it out of the runtime log.
                UnconvertedTextParams::NoFontInstalled { legacy_name: None } => crate::trace_log!(
                    cat::PERSIST,
                    "text_params not upgraded uid={} reason=no_font_named",
                    uid
                ),
                UnconvertedTextParams::PathOnlyMatch {
                    legacy_name,
                    path_identity,
                } => crate::runtime_log::log_warn(format!(
                    "[typing] text layer '{uid}': none of the font names it stores ({}) is \
                     installed; only its remembered file path still resolves, and to a font named \
                     '{path_identity}'. A file at a remembered path is not proof that it is the \
                     same font, so the layer is NOT converted and keeps its legacy font keys. \
                     Check it and re-pick its font.",
                    legacy_name.as_deref().unwrap_or("none")
                )),
            }
            self.legacy_text_params_unresolved.insert(uid);
        }
        if converted.is_empty() {
            return;
        }
        // Grouped per page: `route_to_doc` locks the doc and RE-PROJECTS the whole page, so
        // one call per overlay would re-project a legacy page once per text layer on the
        // frame it is opened.
        let mut by_page: std::collections::BTreeMap<usize, Vec<(String, Value)>> =
            std::collections::BTreeMap::new();
        for (idx, updated) in converted {
            let Some(overlay) = self.overlays.get_mut(idx) else {
                continue;
            };
            let page_idx = overlay.page_idx;
            let uid = overlay.uid.clone();
            // The runtime is updated FIRST so the conversion sticks even when the page is
            // not resident in the doc (a legacy chapter with no doc wired): the pass then
            // sees schema 2 next frame and does not retry.
            overlay.render_data_json = Some(updated.clone());
            self.legacy_text_params_unresolved.remove(&uid);
            crate::trace_log!(
                cat::PERSIST,
                "text_params upgraded to schema 2 page={} uid={}",
                page_idx,
                uid
            );
            by_page.entry(page_idx).or_default().push((uid, updated));
        }
        let mut wrote_to_doc = false;
        for (page_idx, payloads) in by_page {
            // REPORTING variant: a uid that is not on the doc page (the runtime overlay
            // outlived its node) changes nothing, and marking the document changed for it
            // would re-project the page and rewrite `layers.json` with identical content.
            if self.route_to_doc_reporting(page_idx, |doc| {
                let mut changed = false;
                for (uid, payload) in payloads {
                    if let Some(node) = doc.node_mut(page_idx, &uid)
                        && let crate::models::layer_model::layer_doc::NodeBody::Text {
                            render_data,
                            ..
                        } = &mut node.body
                        && *render_data != payload
                    {
                        *render_data = payload;
                        changed = true;
                    }
                }
                changed
            }) {
                wrote_to_doc = true;
            }
        }
        // Only a conversion that actually reached the DOC is owed a write; a runtime-only
        // conversion (no doc for that page) has nothing to persist, and marking dirty for
        // it would leave the layer permanently dirty.
        if wrote_to_doc {
            self.mark_placement_save_dirty();
        }
    }
}

/// Decode a persisted `raster_transform` object into a [`VectorMeshWarp`].
///
/// Expects `{ cols, rows, src_width_px, src_height_px, points_norm: [[x,y],..] }`
/// where `cols >= 2`, `rows >= 2`, and `points_norm.len() == cols * rows`
/// (row-major). Returns `None` for any missing key, non-object value, degenerate
/// grid, or point-count mismatch — the caller then treats the warp as absent
/// (identity / no warp). Never panics. A present-but-malformed object is logged
/// as a warning so a corrupted project is diagnosable.
pub(in crate::tabs::typing) fn decode_vector_mesh_warp(value: &Value) -> Option<VectorMeshWarp> {
    let Some(obj) = value.as_object() else {
        crate::trace_log!(cat::TYPING, "raster_transform: not an object, ignoring warp");
        return None;
    };
    let cols = obj
        .get("cols")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())?;
    let rows = obj
        .get("rows")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())?;
    if cols < 2 || rows < 2 {
        crate::trace_log!(
            cat::TYPING,
            "raster_transform: degenerate grid cols={cols} rows={rows}, ignoring warp"
        );
        return None;
    }
    let raw_points = obj.get("points_norm").and_then(Value::as_array)?;
    let expected = cols.checked_mul(rows)?;
    if raw_points.len() != expected {
        crate::trace_log!(
            cat::TYPING,
            "raster_transform: points_norm len={} != cols*rows={expected}, ignoring warp",
            raw_points.len()
        );
        return None;
    }
    let mut points_norm = Vec::with_capacity(expected);
    for point in raw_points {
        let arr = point.as_array()?;
        let x = arr.first().and_then(value_as_f32)?;
        let y = arr.get(1).and_then(value_as_f32)?;
        points_norm.push([x, y]);
    }
    Some(VectorMeshWarp {
        cols,
        rows,
        // Source-rect dims: when > 0 the renderer honors them as the warp
        // normalization-box size (Design B); a missing value defaults to 0.0,
        // which makes the renderer fall back to the live pre-warp box.
        src_width_px: obj.get("src_width_px").and_then(value_as_f32).unwrap_or(0.0),
        src_height_px: obj
            .get("src_height_px")
            .and_then(value_as_f32)
            .unwrap_or(0.0),
        points_norm,
    })
}

pub(super) fn text_formula_layout_params_from_value(value: Option<&Value>) -> TextFormulaLayoutParams {
    let Some(obj) = value.and_then(Value::as_object) else {
        return TextFormulaLayoutParams::default();
    };
    let defaults = TextFormulaLayoutParams::default();
    let mut vars = defaults.vars;
    if let Some(raw_vars) = obj.get("vars").and_then(Value::as_array) {
        for (idx, value) in raw_vars
            .iter()
            .take(TEXT_FORMULA_USER_VAR_COUNT)
            .enumerate()
        {
            if let Some(parsed) = value_as_f32(value) {
                vars[idx] = parsed;
            }
        }
    }
    TextFormulaLayoutParams {
        x_expr: obj
            .get("x_expr")
            .and_then(Value::as_str)
            .unwrap_or(defaults.x_expr.as_str())
            .to_string(),
        y_expr: obj
            .get("y_expr")
            .and_then(Value::as_str)
            .unwrap_or(defaults.y_expr.as_str())
            .to_string(),
        rotation_expr: obj
            .get("rotation_expr")
            .and_then(Value::as_str)
            .unwrap_or(defaults.rotation_expr.as_str())
            .to_string(),
        use_tangent_rotation: obj
            .get("use_tangent_rotation")
            .and_then(Value::as_bool)
            .unwrap_or(defaults.use_tangent_rotation),
        t_start: obj
            .get("t_start")
            .and_then(value_as_f32)
            .unwrap_or(defaults.t_start),
        t_end: obj
            .get("t_end")
            .and_then(value_as_f32)
            .unwrap_or(defaults.t_end),
        offset_x_px: obj
            .get("offset_x_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.offset_x_px),
        offset_y_px: obj
            .get("offset_y_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.offset_y_px),
        scale_x: obj
            .get("scale_x")
            .and_then(value_as_f32)
            .unwrap_or(defaults.scale_x),
        scale_y: obj
            .get("scale_y")
            .and_then(value_as_f32)
            .unwrap_or(defaults.scale_y),
        normal_offset_px: obj
            .get("normal_offset_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.normal_offset_px),
        letter_spacing_mul: obj
            .get("letter_spacing_mul")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_mul),
        letter_spacing_px: obj
            .get("letter_spacing_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_px),
        vars,
    }
}

pub(super) fn text_drawn_lines_layout_params_from_value(value: Option<&Value>) -> TextDrawnLinesLayoutParams {
    let Some(obj) = value.and_then(Value::as_object) else {
        return TextDrawnLinesLayoutParams::default();
    };
    let defaults = TextDrawnLinesLayoutParams::default();
    TextDrawnLinesLayoutParams {
        image_path: None,
        use_tangent_rotation: obj
            .get("use_tangent_rotation")
            .and_then(Value::as_bool)
            .unwrap_or(defaults.use_tangent_rotation),
        static_rotation_rad: obj
            .get("static_rotation_rad")
            .and_then(value_as_f32)
            .unwrap_or(defaults.static_rotation_rad),
        normal_offset_px: obj
            .get("normal_offset_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.normal_offset_px),
        letter_spacing_mul: obj
            .get("letter_spacing_mul")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_mul)
            .clamp(0.0, 8.0),
        letter_spacing_px: obj
            .get("letter_spacing_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_px)
            .clamp(-10_000.0, 10_000.0),
        color_tolerance: obj
            .get("color_tolerance")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(defaults.color_tolerance),
        continuation_alpha: obj
            .get("continuation_alpha")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(defaults.continuation_alpha),
        start_alpha: obj
            .get("start_alpha")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(defaults.start_alpha),
    }
}

pub(super) fn text_vector_lines_layout_params_from_value(
    value: Option<&Value>,
) -> TextVectorLinesLayoutParams {
    let Some(obj) = value.and_then(Value::as_object) else {
        return TextVectorLinesLayoutParams::default();
    };
    let defaults = TextVectorLinesLayoutParams::default();
    let lines = obj
        .get("lines")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(text_vector_line_params_from_value)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    TextVectorLinesLayoutParams {
        width_px: obj
            .get("width_px")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(defaults.width_px)
            .max(1),
        height_px: obj
            .get("height_px")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(defaults.height_px)
            .max(1),
        use_tangent_rotation: obj
            .get("use_tangent_rotation")
            .and_then(Value::as_bool)
            .unwrap_or(defaults.use_tangent_rotation),
        static_rotation_rad: obj
            .get("static_rotation_rad")
            .and_then(value_as_f32)
            .unwrap_or(defaults.static_rotation_rad),
        normal_offset_px: obj
            .get("normal_offset_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.normal_offset_px),
        letter_spacing_mul: obj
            .get("letter_spacing_mul")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_mul)
            .clamp(0.0, 8.0),
        letter_spacing_px: obj
            .get("letter_spacing_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_px)
            .clamp(-10_000.0, 10_000.0),
        lines,
    }
}

pub(super) fn text_vector_line_params_from_value(value: &Value) -> Option<TextVectorLine> {
    let obj = value.as_object()?;
    let points = obj
        .get("points")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(text_vector_point_params_from_value)
        .collect::<Vec<_>>();
    Some(TextVectorLine {
        points,
        corner_smoothing_px: obj
            .get("corner_smoothing_px")
            .and_then(value_as_f32)
            .unwrap_or(0.0)
            .clamp(0.0, 256.0),
        text_direction: vector_line_text_direction_from_value(obj.get("text_direction")),
        distance_mode: vector_line_distance_mode_from_value(obj.get("distance_mode")),
        flip_text: obj
            .get("flip_text")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

pub(super) fn text_vector_point_params_from_value(value: &Value) -> Option<TextVectorPoint> {
    let obj = value.as_object()?;
    Some(TextVectorPoint {
        x: obj.get("x").and_then(value_as_f32)?,
        y: obj.get("y").and_then(value_as_f32)?,
    })
}


/// Parse a serialized kerning-mode config string. Accepts the current tokens
/// (`"fixed"`/`"auto"`/`"optical"`) and the legacy `"metric"` token (font-pair
/// kerning), which maps to [`KerningMode::Auto`] so old projects render
/// identically. Returns `None` for unknown/missing values.
pub(super) fn parse_kerning_mode_config_str(raw: &str) -> Option<KerningMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "fixed" => Some(KerningMode::Fixed),
        "auto" | "metric" => Some(KerningMode::Auto),
        "optical" => Some(KerningMode::Optical),
        _ => None,
    }
}

/// Прочитать параметр `px-или-%`: сначала новый строковый ключ-токен, затем
/// устаревшие отдельные ключи `*_px`/`*_percent` (с приоритетом пикселей).
pub(super) fn read_render_param_px_or_percent(
    obj: &serde_json::Map<String, Value>,
    token_key: &str,
    legacy_px_key: &str,
    legacy_percent_key: &str,
    default: PxOrPercent,
) -> PxOrPercent {
    if let Some(value) = obj.get(token_key) {
        if let Some(text) = value.as_str() {
            if let Some(parsed) = PxOrPercent::parse(text) {
                return parsed;
            }
        } else if let Some(number) = value_as_f32(value) {
            // Голое число в ключе-токене встречается лишь в легаси `line_spacing`,
            // где оно означало пиксели.
            return PxOrPercent::px(number);
        }
    }
    let legacy_px = obj.get(legacy_px_key).and_then(value_as_f32);
    let legacy_percent = obj.get(legacy_percent_key).and_then(value_as_f32);
    if legacy_px.is_some() || legacy_percent.is_some() {
        return PxOrPercent::from_legacy_pair(
            legacy_px.unwrap_or(0.0),
            legacy_percent.unwrap_or(0.0),
        );
    }
    default
}

pub(super) fn parse_text_shape_config_str(raw: &str) -> Option<TextShape> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "free" => Some(TextShape::Free),
        "rectangle" => Some(TextShape::Rectangle),
        "oval" => Some(TextShape::Oval),
        "hexagon" => Some(TextShape::Hexagon),
        "soft_peak" | "soft" | "no_trees" => Some(TextShape::SoftPeak),
        _ => None,
    }
}

pub(super) fn parse_text_wrap_mode_config_str(raw: &str) -> Option<TextWrapMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "none" => Some(TextWrapMode::None),
        "whole_words" | "words" | "word" => Some(TextWrapMode::WholeWords),
        "minimal" => Some(TextWrapMode::Minimal),
        "moderate" => Some(TextWrapMode::Moderate),
        "aggressive" | "smart" => Some(TextWrapMode::Aggressive),
        _ => None,
    }
}

pub(super) fn text_wrap_mode_to_config_str(mode: TextWrapMode) -> &'static str {
    match mode {
        TextWrapMode::None => "none",
        TextWrapMode::WholeWords => "whole_words",
        TextWrapMode::Minimal => "minimal",
        TextWrapMode::Moderate => "moderate",
        TextWrapMode::Aggressive => "aggressive",
    }
}

/// Parse a persisted anti-aliasing token; `None` for unknown text.
pub(super) fn parse_anti_aliasing_config_str(raw: &str) -> Option<AntiAliasingMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "none" => Some(AntiAliasingMode::None),
        "sharp" => Some(AntiAliasingMode::Sharp),
        "crisp" => Some(AntiAliasingMode::Crisp),
        "strong" => Some(AntiAliasingMode::Strong),
        "smooth" => Some(AntiAliasingMode::Smooth),
        _ => None,
    }
}

pub(super) fn parse_text_line_mode_config_str(raw: &str) -> Option<TextLineMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "horizontal" => Some(TextLineMode::Horizontal),
        "vertical" => Some(TextLineMode::Vertical),
        _ => None,
    }
}

pub(super) fn parse_vertical_line_direction_config_str(raw: &str) -> Option<VerticalLineDirection> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "left_to_right" | "ltr" => Some(VerticalLineDirection::LeftToRight),
        "right_to_left" | "rtl" => Some(VerticalLineDirection::RightToLeft),
        _ => None,
    }
}

pub(super) fn parse_text_layout_mode_config_str(raw: &str) -> Option<TextLayoutMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "normal" => Some(TextLayoutMode::Normal),
        "formula" => Some(TextLayoutMode::Formula),
        "shape" => Some(TextLayoutMode::Shape),
        "drawn_lines"
        | "drawn-lines"
        | "drawnlines"
        | "custom_raster_lines"
        | "custom-raster-lines"
        | "customrasterlines" => Some(TextLayoutMode::CustomRasterLines),
        "vector_lines"
        | "vector-lines"
        | "vectorlines"
        | "custom_vector_lines"
        | "custom-vector-lines"
        | "customvectorlines" => Some(TextLayoutMode::CustomVectorLines),
        _ => None,
    }
}

// Parameters represent distinct required inputs with no natural grouping.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_storage_overlay_entry(
    uid: &str,
    kind: TypingOverlayKind,
    page_idx: usize,
    file_name: &str,
    original_file_name: Option<&str>,
    center_page_px: [f32; 2],
    mask_clip_enabled: bool,
    layer_idx: usize,
    rotation_deg: f32,
    scale: f32,
    deform_mesh: Option<TypingOverlayDeformMesh>,
    render_data: Option<Value>,
) -> Value {
    let mut out = serde_json::Map::<String, Value>::new();
    out.insert("uid".to_string(), Value::String(uid.to_string()));
    out.insert(
        "overlay_type".to_string(),
        Value::String(
            match kind {
                TypingOverlayKind::Text => "text",
                TypingOverlayKind::Image => "image",
            }
            .to_string(),
        ),
    );
    out.insert("img_idx".to_string(), Value::from(page_idx as u64));
    out.insert("file".to_string(), Value::String(file_name.to_string()));
    // Для image-оверлеев `file` хранит картинку ПОСЛЕ эффектов (она же идёт в показ/экспорт),
    // а `image_original_file` — исходную импортированную картинку, чтобы эффекты можно было
    // переприменять и отменять без потери качества.
    if let Some(original) = original_file_name.filter(|name| !name.is_empty() && *name != file_name)
    {
        out.insert(
            "image_original_file".to_string(),
            Value::String(original.to_string()),
        );
    }
    // Serialize position/rotation/scale through the shared encoder (single encode point: center →
    // img_x/y, rad → rotation_deg, scale). The caller supplies rotation in DEGREES, so convert to the
    // canonical radians `TransformRec` the encoder consumes.
    crate::models::layer_model::text_payload::encode_transform_fields(
        &crate::models::layer_model::manifest::TransformRec {
            cx: center_page_px[0],
            cy: center_page_px[1],
            rotation: rotation_deg.to_radians(),
            scale: scale.max(0.01),
        },
        &mut out,
    );
    out.insert(
        "mask_clip_enabled".to_string(),
        Value::from(mask_clip_enabled),
    );
    out.insert("layer_idx".to_string(), Value::from(layer_idx as u64));
    if let Some(mesh) = deform_mesh {
        // Serialize the deform mesh through the shared encoder (single encode point), converting the
        // runtime mesh to the canonical `DeformRec` first.
        let rec = crate::models::layer_model::manifest::DeformRec {
            cols: mesh.cols,
            rows: mesh.rows,
            points_px: mesh.points_px.clone(),
        };
        out.insert(
            "deform_mesh".to_string(),
            crate::models::layer_model::text_payload::encode_deform_mesh(&rec),
        );
    }
    if let Some(render_data) = render_data {
        out.insert("render_data".to_string(), render_data);
    }
    Value::Object(out)
}

pub(super) fn parse_overlay_render_data_json(
    obj: &serde_json::Map<String, Value>,
    fallback_width_px: u32,
) -> Option<Value> {
    if let Some(render_data_value) = obj.get("render_data")
        && let Some(normalized) = normalize_render_data_value(render_data_value, fallback_width_px)
    {
        return Some(normalized);
    }
    if let Some(render_params) = obj.get("render_params").and_then(Value::as_object) {
        return Some(render_params_object_to_render_data(
            render_params,
            fallback_width_px,
        ));
    }
    parse_legacy_static_render_data(obj, fallback_width_px)
}

pub(super) fn normalize_render_data_value(value: &Value, fallback_width_px: u32) -> Option<Value> {
    let obj = value.as_object()?;
    if obj.get("text_params").and_then(Value::as_object).is_some() {
        let text_params_obj = obj
            .get("text_params")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let text_params = normalize_text_params_object(&text_params_obj, fallback_width_px);
        let effects = obj
            .get("effects")
            .and_then(Value::as_array)
            .cloned()
            .or_else(|| {
                obj.get("effects_json")
                    .and_then(Value::as_str)
                    .map(parse_effects_json_array)
            })
            .unwrap_or_default();
        return Some(json!({
            "schema_version": 2,
            "text_params": text_params,
            "effects": effects,
        }));
    }
    Some(render_params_object_to_render_data(obj, fallback_width_px))
}

pub(super) fn render_params_object_to_render_data(
    obj: &serde_json::Map<String, Value>,
    fallback_width_px: u32,
) -> Value {
    let text_params = normalize_text_params_object(obj, fallback_width_px);
    let effects = parse_effects_list_from_render_params_object(obj);
    json!({
        "schema_version": 2,
        "text_params": text_params,
        "effects": effects,
    })
}

pub(super) fn normalize_text_params_object(
    obj: &serde_json::Map<String, Value>,
    fallback_width_px: u32,
) -> Value {
    // A payload that already declares the CURRENT schema is canonical by construction and
    // is passed through verbatim. Running the legacy whitelist over it would be actively
    // destructive: the whitelist manufactures its own legacy defaults for absent keys,
    // while in schema 2 an absent key means its FROZEN default — and it would drop
    // `schema`/`font` entirely, unnaming the overlay's font.
    if text_params_schema::text_params_schema_version(obj)
        >= text_params_schema::TEXT_PARAMS_SCHEMA_VERSION
    {
        return Value::Object(obj.clone());
    }
    let text = obj
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let text_color = obj
        .get("text_color")
        .and_then(parse_rgba_value)
        .or_else(|| obj.get("font_color_rgba").and_then(parse_rgba_value))
        .or_else(|| obj.get("color").and_then(parse_rgba_value))
        .unwrap_or([0, 0, 0, 255]);
    let width_px = obj
        .get("width_px")
        .and_then(value_as_f32)
        .map(|v| v.round().max(1.0) as u32)
        .unwrap_or_else(|| fallback_width_px.max(1));
    let align =
        normalize_align_legacy(obj.get("align").and_then(Value::as_str).unwrap_or("center"));
    let text_shape = normalize_text_shape_legacy(
        obj.get("text_shape")
            .and_then(Value::as_str)
            .unwrap_or("rectangle"),
    );
    let text_line_mode = normalize_text_line_mode_legacy(
        obj.get("text_line_mode")
            .and_then(Value::as_str)
            .unwrap_or("horizontal"),
    );
    let text_layout_mode = normalize_text_layout_mode_legacy(
        obj.get("text_layout_mode")
            .and_then(Value::as_str)
            .unwrap_or("normal"),
    );
    let text_wrap_mode = normalize_text_wrap_mode_legacy(
        obj.get("text_wrap_mode").and_then(Value::as_str),
        obj.get("aggressive_word_breaks").and_then(Value::as_bool),
        obj.get("allow_moderate_trees").and_then(Value::as_bool),
    );
    let formula_layout =
        normalize_formula_layout_object(obj.get("formula_layout").and_then(Value::as_object));
    let shape_layout =
        normalize_shape_layout_object(obj.get("shape_layout").and_then(Value::as_object));
    let drawn_lines_layout = normalize_drawn_lines_layout_object(
        obj.get("drawn_lines_layout").and_then(Value::as_object),
    );
    let vector_lines_layout = normalize_vector_lines_layout_object(
        obj.get("vector_lines_layout").and_then(Value::as_object),
    );
    let selected_face_index = obj
        .get("selected_face_index")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(0usize);

    let mut params = json!({
        "text": text,
        "text_color": text_color,
        "font_size_px": obj.get("font_size_px").and_then(value_as_f32).or_else(|| obj.get("font_size").and_then(value_as_f32)).or_else(|| obj.get("size").and_then(value_as_f32)).unwrap_or(24.0).max(1.0),
        // LOSSLESS tokens: this normalization REPLACES the legacy `*_px`/`*_percent` pair
        // it read, so a two-decimal rounding here is not recoverable afterwards.
        "line_spacing": read_render_param_px_or_percent(obj, "line_spacing", "line_spacing_px", "line_spacing_percent", PxOrPercent::percent(50.0)).to_token_lossless(),
        "kerning": read_render_param_px_or_percent(obj, "kerning", "kerning_px", "kerning_percent", PxOrPercent::percent(0.0)).to_token_lossless(),
        "glyph_height": read_render_param_px_or_percent(obj, "glyph_height", "", "glyph_height_percent", PxOrPercent::percent(100.0)).to_token_lossless(),
        "glyph_width": read_render_param_px_or_percent(obj, "glyph_width", "", "glyph_width_percent", PxOrPercent::percent(100.0)).to_token_lossless(),
        "width_px": width_px,
        "align": align,
        "text_line_mode": text_line_mode,
        "text_layout_mode": text_layout_mode,
        "formula_layout": formula_layout,
        "shape_layout": shape_layout,
        "drawn_lines_layout": drawn_lines_layout,
        "vector_lines_layout": vector_lines_layout,
        "selected_face_index": selected_face_index,
        "force_bold": obj.get("force_bold").and_then(Value::as_bool).unwrap_or(false),
        "force_italic": obj.get("force_italic").and_then(Value::as_bool).unwrap_or(false),
        "faux_bold": obj.get("faux_bold").and_then(Value::as_bool).unwrap_or(false),
        // SIGNED: a negative stored value is a legitimate THINNING request.
        "faux_bold_thicken_percent": obj.get("faux_bold_thicken_percent").and_then(value_as_f32).unwrap_or(3.0).clamp(FAUX_THICKEN_PERCENT_MIN, FAUX_THICKEN_PERCENT_MAX),
        "faux_bold_expand_percent": obj.get("faux_bold_expand_percent").and_then(value_as_f32).unwrap_or(0.0).clamp(0.0, 50.0),
        "faux_bold_sharp_corners": obj.get("faux_bold_sharp_corners").and_then(Value::as_bool).unwrap_or(true),
        "faux_bold_outward_only": obj.get("faux_bold_outward_only").and_then(Value::as_bool).unwrap_or(true),
        "faux_italic": obj.get("faux_italic").and_then(Value::as_bool).unwrap_or(false),
        "faux_italic_slant_deg": obj.get("faux_italic_slant_deg").and_then(value_as_f32).unwrap_or(14.0).clamp(-45.0, 45.0),
        "uppercase_text": obj.get("uppercase_text").and_then(Value::as_bool).unwrap_or(false),
        "enable_inline_style_tags": obj.get("enable_inline_style_tags").and_then(Value::as_bool).unwrap_or(false),
        "text_wrap_mode": text_wrap_mode,
        "allow_moderate_trees": obj.get("allow_moderate_trees").and_then(Value::as_bool).unwrap_or(false),
        "text_shape": text_shape,
        "shape_min_width_percent": obj.get("shape_min_width_percent").and_then(value_as_f32).unwrap_or(50.0),
        "shape_variant": obj.get("shape_variant").and_then(Value::as_u64).unwrap_or(5).clamp(1, 9),
    });

    // Современные поля панели, которых не было в легаси-схеме. Нормализатор строит
    // `text_params` по белому списку, поэтому без явного проброса они терялись при
    // загрузке проекта (напр. `formed_text` — сформированный текст «продвинутой
    // формы»). Сохраняем как есть, если присутствуют; иначе панель подставит свои
    // дефолты при чтении.
    //
    // THE LIST BELOW IS A DATA-LOSS SURFACE. Every key of the frozen schema-2 default set
    // that the literal above does not build must appear here, or a stored value dies on
    // load — and since the schema-1 -> schema-2 conversion runs on what this function
    // returned, that death is now PERMANENT (the converted payload is written back with
    // the default in its place). `normalization_preserves_every_schema_two_key` in
    // `tab/tests.rs` walks `text_params_schema::frozen_v2_defaults()` and fails when a key
    // is missing from both halves; keep it passing rather than trimming this list.
    if let Some(map) = params.as_object_mut() {
        for key in [
            // Legacy FONT keys, carried through VERBATIM and never manufactured. They are
            // the only surviving record of which font a schema-1 payload named, so the
            // normalizer must not drop them before the conversion pass has had a chance
            // to resolve them (`upgrade_text_params_to_v2`); the conversion itself is
            // what removes them, and only once the identity is known.
            "font_path",
            "font_label",
            "font_original_name",
            "font_family",
            "font",
            "formed_text",
            "kerning_mode",
            "hanging_punctuation",
            "new_line_after_sentence",
            "trim_extra_spaces",
            "replace_ellipsis_with_dots",
            "force_remove_ellipsis_glyph",
            "vertical_line_direction",
            // Vector rotation of the whole block, perpendicular line placement and its
            // reference band, and the anti-aliasing mode: panel-read parameters
            // (`create_apply`) with no legacy normalization of their own. Absent here they
            // were silently reset on every load — rotation to 0, the placement reference
            // to `glyph_height`, anti-aliasing to `strong`.
            "global_rotation_deg",
            "line_placement_percent",
            "line_placement_reference",
            "anti_aliasing",
            // Точное смещение выравнивания (слайдер лево↔право). Легаси-строка
            // `align` сохраняется отдельно для совместимости/PSD-экспорта, но
            // непрерывное значение живёт только здесь.
            "align_bias",
            // Векторная mesh-деформация текста (авторится на холсте, Phase 3).
            // Непрозрачный блоб — проносится как есть, чтобы re-normalize
            // легаси `text_info.json` его не терял.
            "raster_transform",
        ] {
            if let Some(value) = obj.get(key) {
                map.insert(key.to_string(), value.clone());
            }
        }
    }
    params
}

pub(super) fn parse_effects_list_from_render_params_object(
    obj: &serde_json::Map<String, Value>,
) -> Vec<Value> {
    if let Some(effects) = obj.get("effects").and_then(Value::as_array) {
        return effects.clone();
    }
    if let Some(effects_json) = obj.get("effects_json").and_then(Value::as_str) {
        return parse_effects_json_array(effects_json);
    }
    Vec::new()
}

pub(super) fn parse_legacy_static_render_data(
    obj: &serde_json::Map<String, Value>,
    fallback_width_px: u32,
) -> Option<Value> {
    let style = obj.get("style").and_then(Value::as_object);
    let text = obj
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if text.is_empty() && style.is_none() {
        return None;
    }

    let font_label = overlay_param_str(style, obj, "font_family")
        .or_else(|| overlay_param_str(style, obj, "font"))
        .unwrap_or_default();
    let font_size_px = overlay_param_f32(style, obj, "font_size")
        .or_else(|| overlay_param_f32(style, obj, "size"))
        .unwrap_or(24.0);
    let text_color = overlay_param_rgba(style, obj, "font_color_rgba")
        .or_else(|| overlay_param_rgba(style, obj, "color"))
        .unwrap_or([0, 0, 0, 255]);
    // В легаси-схеме `line_spacing` — пиксели, `line_spacing_percent` — проценты.
    let line_spacing = PxOrPercent::from_legacy_pair(
        overlay_param_f32(style, obj, "line_spacing").unwrap_or(4.0),
        overlay_param_f32(style, obj, "line_spacing_percent").unwrap_or(50.0),
    );
    let align = normalize_align_legacy(
        overlay_param_str(style, obj, "align")
            .unwrap_or_else(|| "center".to_string())
            .as_str(),
    );
    let text_shape = normalize_text_shape_legacy(
        overlay_param_str(style, obj, "text_shape")
            .unwrap_or_else(|| "rectangle".to_string())
            .as_str(),
    );
    let width_px = overlay_param_f32(style, obj, "width_px")
        .or_else(|| obj.get("width_px").and_then(value_as_f32))
        .map(|v| v.round().max(1.0) as u32)
        .unwrap_or_else(|| fallback_width_px.max(1));

    let effects = build_legacy_effects_json(style, obj);
    Some(json!({
        "schema_version": 2,
        "source": "legacy_static_style",
        "text_params": {
            "text": text,
            "text_color": text_color,
            "font_path": Value::Null,
            "font_label": font_label,
            "font_size_px": font_size_px.max(1.0),
            // Lossless for the same reason as in `normalize_text_params_object`: the token
            // replaces the legacy pair this value was folded from.
            "line_spacing": line_spacing.to_token_lossless(),
            "width_px": width_px,
            "align": align,
            "text_line_mode": "horizontal",
            "text_layout_mode": "normal",
            "formula_layout": normalize_formula_layout_object(None),
            "drawn_lines_layout": normalize_drawn_lines_layout_object(None),
            "vector_lines_layout": normalize_vector_lines_layout_object(None),
            "selected_face_index": 0,
            "force_bold": false,
            "force_italic": false,
            "faux_bold": false,
            "faux_bold_thicken_percent": 3.0,
            "faux_bold_expand_percent": 0.0,
            "faux_bold_sharp_corners": true,
            "faux_bold_outward_only": true,
            "faux_italic": false,
            "faux_italic_slant_deg": 14.0,
            "uppercase_text": false,
            "enable_inline_style_tags": false,
            "text_wrap_mode": "aggressive",
            "text_shape": text_shape,
            "shape_min_width_percent": 50.0,
            "shape_variant": 5,
        },
        "effects": effects,
    }))
}

pub(super) fn build_legacy_effects_json(
    style: Option<&serde_json::Map<String, Value>>,
    obj: &serde_json::Map<String, Value>,
) -> Vec<Value> {
    let mut out = Vec::<Value>::new();

    let stroke_width = overlay_param_f32(style, obj, "stroke_width").unwrap_or(0.0);
    if stroke_width > 0.0 {
        out.push(json!({
            "effect": "stroke",
            "enabled": true,
            "width": stroke_width,
            "color": overlay_param_rgba(style, obj, "stroke_color_rgba").unwrap_or([0, 0, 0, 255]),
            "opacity_mode": "static",
            "transparency": 0.0,
            "opacity": 100.0,
        }));
    }

    if let Some(shadow_color) = overlay_param_rgba(style, obj, "shadow_color_rgba") {
        out.push(json!({
            "effect": "shadow",
            "enabled": true,
            "offset_x": overlay_param_i32(style, obj, "shadow_dx").unwrap_or(0),
            "offset_y": overlay_param_i32(style, obj, "shadow_dy").unwrap_or(0),
            "transparency": 0.0,
            "opacity": 100.0,
            "mode": "single",
            "use_source_color": false,
            "color": shadow_color,
        }));
    }

    let glow_radius = overlay_param_f32(style, obj, "glow_radius").unwrap_or(0.0);
    if glow_radius > 0.0
        && let Some(glow_color) = overlay_param_rgba(style, obj, "glow_color_rgba")
    {
        out.push(json!({
            "effect": "glow_v1",
            "enabled": true,
            "radius": glow_radius,
            "color": glow_color,
            "opacity_mode": "static",
            "transparency": 0.0,
            "opacity": 100.0,
            "fade_strength": 0.0,
            "fade_shift": 0.0,
        }));
    }

    let grad2_c1 = overlay_param_rgba(style, obj, "grad2_c1_rgba");
    let grad2_c2 = overlay_param_rgba(style, obj, "grad2_c2_rgba");
    if let (Some(c1), Some(c2)) = (grad2_c1, grad2_c2) {
        out.push(json!({
            "effect": "gradient2",
            "enabled": true,
            "color1": c1,
            "color2": c2,
            "angle_deg": overlay_param_f32(style, obj, "grad_angle_deg").unwrap_or(90.0),
            "respect_source_alpha": true,
            "fill_mode": "all_opaque",
        }));
    }

    let grad4_tl = overlay_param_rgba(style, obj, "grad4_tl_rgba");
    let grad4_tr = overlay_param_rgba(style, obj, "grad4_tr_rgba");
    let grad4_bl = overlay_param_rgba(style, obj, "grad4_bl_rgba");
    let grad4_br = overlay_param_rgba(style, obj, "grad4_br_rgba");
    if let (Some(tl), Some(tr), Some(bl), Some(br)) = (grad4_tl, grad4_tr, grad4_bl, grad4_br) {
        out.push(json!({
            "effect": "gradient4",
            "enabled": true,
            "color_top_left": tl,
            "color_top_right": tr,
            "color_bottom_left": bl,
            "color_bottom_right": br,
            "respect_source_alpha": true,
            "fill_mode": "all_opaque",
        }));
    }

    if let Some(axis_raw) = overlay_param_str(style, obj, "reflect") {
        let axis = axis_raw.trim().to_ascii_lowercase();
        if axis == "x" || axis == "y" {
            out.push(json!({
                "effect": "reflect",
                "enabled": true,
                "axis": axis,
            }));
        }
    }

    if overlay_param_bool(style, obj, "shake_enabled").unwrap_or(false) {
        out.push(json!({
            "effect": "shake",
            "enabled": true,
            "angle_deg": overlay_param_f32(style, obj, "shake_angle_deg").unwrap_or(90.0),
            "up": overlay_param_f32(style, obj, "shake_up").unwrap_or(0.0),
            "down": overlay_param_f32(style, obj, "shake_down").unwrap_or(40.0),
            "steps": overlay_param_i32(style, obj, "shake_steps").unwrap_or(12).max(0) as u32,
            "base_fade": overlay_param_f32(style, obj, "shake_base_fade").unwrap_or(0.30),
            "decay": overlay_param_f32(style, obj, "shake_decay").unwrap_or(0.15),
            "blur": overlay_param_i32(style, obj, "shake_blur").unwrap_or(2).max(0) as u32,
            "autogrow": true,
            "grow_margin": 0,
        }));
    }

    out
}

pub(super) fn overlay_param_value<'a>(
    style: Option<&'a serde_json::Map<String, Value>>,
    obj: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Option<&'a Value> {
    style.and_then(|map| map.get(key)).or_else(|| obj.get(key))
}

pub(super) fn overlay_param_str(
    style: Option<&serde_json::Map<String, Value>>,
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Option<String> {
    overlay_param_value(style, obj, key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(super) fn overlay_param_bool(
    style: Option<&serde_json::Map<String, Value>>,
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Option<bool> {
    overlay_param_value(style, obj, key).and_then(Value::as_bool)
}

pub(super) fn overlay_param_f32(
    style: Option<&serde_json::Map<String, Value>>,
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Option<f32> {
    overlay_param_value(style, obj, key).and_then(value_as_f32)
}

pub(super) fn overlay_param_i32(
    style: Option<&serde_json::Map<String, Value>>,
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Option<i32> {
    let value = overlay_param_value(style, obj, key)?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|v| i64::try_from(v).ok()))
        .or_else(|| value.as_f64().map(|v| v.round() as i64))
        .and_then(|v| i32::try_from(v).ok())
}

pub(super) fn overlay_param_rgba(
    style: Option<&serde_json::Map<String, Value>>,
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Option<[u8; 4]> {
    overlay_param_value(style, obj, key).and_then(parse_rgba_value)
}

pub(super) fn parse_rgba_value(value: &Value) -> Option<[u8; 4]> {
    let arr = value.as_array()?;
    if arr.len() < 3 {
        return None;
    }
    let r = value_as_u8(arr.first()?)?;
    let g = value_as_u8(arr.get(1)?)?;
    let b = value_as_u8(arr.get(2)?)?;
    let a = arr.get(3).and_then(value_as_u8).unwrap_or(255);
    Some([r, g, b, a])
}

pub(super) fn value_as_u8(value: &Value) -> Option<u8> {
    if let Some(v) = value.as_u64() {
        return u8::try_from(v).ok();
    }
    value.as_f64().map(|v| v.round().clamp(0.0, 255.0) as u8)
}

pub(super) fn value_as_f32(value: &Value) -> Option<f32> {
    value.as_f64().map(|v| v as f32)
}

pub(super) fn normalize_align_legacy(value: &str) -> &'static str {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "left" | "center" | "right" | "justify" => normalized_to_static(&normalized),
        _ => "center",
    }
}

pub(super) fn normalize_text_shape_legacy(value: &str) -> &'static str {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "free" | "rectangle" | "oval" | "hexagon" | "soft_peak" => {
            normalized_to_static(&normalized)
        }
        "soft" | "no_trees" => "soft_peak",
        _ => "rectangle",
    }
}

pub(super) fn normalize_text_line_mode_legacy(value: &str) -> &'static str {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "horizontal" | "vertical" => normalized_to_static(&normalized),
        _ => "horizontal",
    }
}

pub(super) fn normalize_text_layout_mode_legacy(value: &str) -> &'static str {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "normal" | "formula" | "shape" | "custom_raster_lines" | "custom_vector_lines" => {
            normalized_to_static(&normalized)
        }
        "drawn_lines"
        | "drawn-lines"
        | "drawnlines"
        | "custom-raster-lines"
        | "customrasterlines" => "custom_raster_lines",
        "vector_lines"
        | "vector-lines"
        | "vectorlines"
        | "custom-vector-lines"
        | "customvectorlines" => "custom_vector_lines",
        _ => "normal",
    }
}

pub(super) fn normalize_text_wrap_mode_legacy(
    value: Option<&str>,
    aggressive_word_breaks: Option<bool>,
    allow_moderate_trees: Option<bool>,
) -> &'static str {
    let normalized = value
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(str::to_ascii_lowercase);
    match normalized.as_deref() {
        Some("none") => "none",
        Some("whole_words" | "words" | "word") => "whole_words",
        Some("minimal") => "minimal",
        Some("moderate") => "moderate",
        Some("aggressive") => "aggressive",
        Some("smart") => match aggressive_word_breaks {
            Some(true) => "aggressive",
            Some(false) => "minimal",
            None if allow_moderate_trees.unwrap_or(false) => "minimal",
            None => "aggressive",
        },
        _ => "aggressive",
    }
}

pub(super) fn normalize_shape_layout_object(
    obj: Option<&serde_json::Map<String, Value>>,
) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::<String, Value>::new();
    out.insert("kind".to_string(), Value::String("arc".to_string()));
    out.insert(
        "width_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("width_px"))
                .and_then(value_as_f32)
                .unwrap_or(320.0),
        ),
    );
    out.insert(
        "height_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("height_px"))
                .and_then(value_as_f32)
                .unwrap_or(80.0),
        ),
    );
    out.insert(
        "frequency".to_string(),
        Value::from(
            obj.and_then(|v| v.get("frequency"))
                .and_then(value_as_f32)
                .unwrap_or(1.0),
        ),
    );
    out
}

pub(super) fn normalize_formula_layout_object(
    obj: Option<&serde_json::Map<String, Value>>,
) -> serde_json::Map<String, Value> {
    let defaults = TextFormulaLayoutParams::default();
    let mut out = serde_json::Map::<String, Value>::new();
    out.insert(
        "x_expr".to_string(),
        Value::String(
            obj.and_then(|v| v.get("x_expr"))
                .and_then(Value::as_str)
                .unwrap_or(defaults.x_expr.as_str())
                .to_string(),
        ),
    );
    out.insert(
        "y_expr".to_string(),
        Value::String(
            obj.and_then(|v| v.get("y_expr"))
                .and_then(Value::as_str)
                .unwrap_or(defaults.y_expr.as_str())
                .to_string(),
        ),
    );
    out.insert(
        "rotation_expr".to_string(),
        Value::String(
            obj.and_then(|v| v.get("rotation_expr"))
                .and_then(Value::as_str)
                .unwrap_or(defaults.rotation_expr.as_str())
                .to_string(),
        ),
    );
    out.insert(
        "use_tangent_rotation".to_string(),
        Value::from(
            obj.and_then(|v| v.get("use_tangent_rotation"))
                .and_then(Value::as_bool)
                .unwrap_or(defaults.use_tangent_rotation),
        ),
    );
    out.insert(
        "t_start".to_string(),
        Value::from(
            obj.and_then(|v| v.get("t_start"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.t_start),
        ),
    );
    out.insert(
        "t_end".to_string(),
        Value::from(
            obj.and_then(|v| v.get("t_end"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.t_end),
        ),
    );
    out.insert(
        "offset_x_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("offset_x_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.offset_x_px),
        ),
    );
    out.insert(
        "offset_y_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("offset_y_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.offset_y_px),
        ),
    );
    out.insert(
        "scale_x".to_string(),
        Value::from(
            obj.and_then(|v| v.get("scale_x"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.scale_x),
        ),
    );
    out.insert(
        "scale_y".to_string(),
        Value::from(
            obj.and_then(|v| v.get("scale_y"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.scale_y),
        ),
    );
    out.insert(
        "normal_offset_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("normal_offset_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.normal_offset_px),
        ),
    );
    out.insert(
        "letter_spacing_mul".to_string(),
        Value::from(
            obj.and_then(|v| v.get("letter_spacing_mul"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.letter_spacing_mul),
        ),
    );
    out.insert(
        "letter_spacing_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("letter_spacing_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.letter_spacing_px),
        ),
    );
    out.insert(
        "vars".to_string(),
        Value::Array(normalize_formula_vars_array(
            obj.and_then(|v| v.get("vars")).and_then(Value::as_array),
            defaults.vars,
        )),
    );
    out
}

pub(super) fn normalize_drawn_lines_layout_object(
    obj: Option<&serde_json::Map<String, Value>>,
) -> serde_json::Map<String, Value> {
    let defaults = TextDrawnLinesLayoutParams::default();
    let mut out = serde_json::Map::<String, Value>::new();
    out.insert(
        "use_tangent_rotation".to_string(),
        Value::from(
            obj.and_then(|v| v.get("use_tangent_rotation"))
                .and_then(Value::as_bool)
                .unwrap_or(defaults.use_tangent_rotation),
        ),
    );
    out.insert(
        "static_rotation_rad".to_string(),
        Value::from(
            obj.and_then(|v| v.get("static_rotation_rad"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.static_rotation_rad),
        ),
    );
    out.insert(
        "normal_offset_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("normal_offset_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.normal_offset_px),
        ),
    );
    out.insert(
        "letter_spacing_mul".to_string(),
        Value::from(
            obj.and_then(|v| v.get("letter_spacing_mul"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.letter_spacing_mul)
                .clamp(0.0, 8.0),
        ),
    );
    out.insert(
        "letter_spacing_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("letter_spacing_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.letter_spacing_px)
                .clamp(-10_000.0, 10_000.0),
        ),
    );
    out.insert(
        "color_tolerance".to_string(),
        Value::from(
            obj.and_then(|v| v.get("color_tolerance"))
                .and_then(Value::as_u64)
                .and_then(|value| u8::try_from(value).ok())
                .unwrap_or(defaults.color_tolerance),
        ),
    );
    out.insert(
        "continuation_alpha".to_string(),
        Value::from(
            obj.and_then(|v| v.get("continuation_alpha"))
                .and_then(Value::as_u64)
                .and_then(|value| u8::try_from(value).ok())
                .unwrap_or(defaults.continuation_alpha),
        ),
    );
    out.insert(
        "start_alpha".to_string(),
        Value::from(
            obj.and_then(|v| v.get("start_alpha"))
                .and_then(Value::as_u64)
                .and_then(|value| u8::try_from(value).ok())
                .unwrap_or(defaults.start_alpha),
        ),
    );
    out
}

pub(super) fn normalize_vector_lines_layout_object(
    obj: Option<&serde_json::Map<String, Value>>,
) -> serde_json::Map<String, Value> {
    let defaults = TextVectorLinesLayoutParams::default();
    let mut out = serde_json::Map::<String, Value>::new();
    out.insert(
        "width_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("width_px"))
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(defaults.width_px)
                .max(1),
        ),
    );
    out.insert(
        "height_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("height_px"))
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(defaults.height_px)
                .max(1),
        ),
    );
    out.insert(
        "use_tangent_rotation".to_string(),
        Value::from(
            obj.and_then(|v| v.get("use_tangent_rotation"))
                .and_then(Value::as_bool)
                .unwrap_or(defaults.use_tangent_rotation),
        ),
    );
    out.insert(
        "static_rotation_rad".to_string(),
        Value::from(
            obj.and_then(|v| v.get("static_rotation_rad"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.static_rotation_rad),
        ),
    );
    out.insert(
        "normal_offset_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("normal_offset_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.normal_offset_px),
        ),
    );
    out.insert(
        "letter_spacing_mul".to_string(),
        Value::from(
            obj.and_then(|v| v.get("letter_spacing_mul"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.letter_spacing_mul)
                .clamp(0.0, 8.0),
        ),
    );
    out.insert(
        "letter_spacing_px".to_string(),
        Value::from(
            obj.and_then(|v| v.get("letter_spacing_px"))
                .and_then(value_as_f32)
                .unwrap_or(defaults.letter_spacing_px)
                .clamp(-10_000.0, 10_000.0),
        ),
    );
    let lines = obj
        .and_then(|v| v.get("lines"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(normalize_vector_line_value)
                .collect()
        })
        .unwrap_or_default();
    out.insert("lines".to_string(), Value::Array(lines));
    out
}

pub(super) fn normalize_vector_line_value(value: &Value) -> Option<Value> {
    let obj = value.as_object()?;
    let points = obj
        .get("points")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(normalize_vector_point_value)
        .collect::<Vec<_>>();
    Some(json!({
        "points": points,
        "corner_smoothing_px": obj
            .get("corner_smoothing_px")
            .and_then(value_as_f32)
            .unwrap_or(0.0)
            .clamp(0.0, 256.0),
        "text_direction": vector_line_text_direction_to_str(vector_line_text_direction_from_value(
            obj.get("text_direction"),
        )),
        "distance_mode": vector_line_distance_mode_to_str(vector_line_distance_mode_from_value(
            obj.get("distance_mode"),
        )),
        "flip_text": obj
            .get("flip_text")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }))
}

pub(super) fn normalize_vector_point_value(value: &Value) -> Option<Value> {
    let obj = value.as_object()?;
    Some(json!({
        "x": obj.get("x").and_then(value_as_f32)?,
        "y": obj.get("y").and_then(value_as_f32)?,
    }))
}

pub(super) fn normalize_formula_vars_array(
    vars: Option<&Vec<Value>>,
    defaults: [f32; TEXT_FORMULA_USER_VAR_COUNT],
) -> Vec<Value> {
    let mut out = Vec::<Value>::with_capacity(TEXT_FORMULA_USER_VAR_COUNT);
    for (idx, &default_val) in defaults.iter().enumerate() {
        let value = vars
            .and_then(|arr| arr.get(idx))
            .and_then(value_as_f32)
            .unwrap_or(default_val);
        out.push(Value::from(value));
    }
    out
}

pub(super) fn normalized_to_static(value: &str) -> &'static str {
    match value {
        "left" => "left",
        "center" => "center",
        "right" => "right",
        "justify" => "justify",
        "free" => "free",
        "rectangle" => "rectangle",
        "oval" => "oval",
        "hexagon" => "hexagon",
        "soft_peak" => "soft_peak",
        "horizontal" => "horizontal",
        "vertical" => "vertical",
        "normal" => "normal",
        "formula" => "formula",
        "shape" => "shape",
        "custom_raster_lines" => "custom_raster_lines",
        "custom_vector_lines" => "custom_vector_lines",
        _ => "",
    }
}

// Legacy per-entry geometry decoding (`transform_uv` quad, `deform_mesh`, `img_u`/`img_v`/`u`/`v`
// position, `angle`/`user_scale` aliases) now lives in the shared `text_payload` codec
// (`decode_overlay_placement` / `decode_deform_mesh`) — the single source of truth so the typing tab
// and the doc resolve old chapters identically. The former `parse_transform_uv` / `parse_deform_mesh`
// / `overlay_center_page_px_from_storage` here were removed.

pub(super) fn legacy_fallback_width_px(obj: &serde_json::Map<String, Value>) -> u32 {
    obj.get("width_px")
        .and_then(value_as_f32)
        .or_else(|| {
            obj.get("render_params")
                .and_then(Value::as_object)
                .and_then(|rp| rp.get("width_px"))
                .and_then(value_as_f32)
        })
        .or_else(|| {
            obj.get("render_data")
                .and_then(Value::as_object)
                .and_then(|rd| rd.get("text_params"))
                .and_then(Value::as_object)
                .and_then(|tp| tp.get("width_px"))
                .and_then(value_as_f32)
        })
        .map(|w| w.round().max(1.0) as u32)
        .unwrap_or(TEXT_RENDER_DATA_FALLBACK_WIDTH_PX)
}

pub(super) fn default_render_data_for_text(text: &str, width_px: u32) -> Value {
    json!({
        "schema_version": 2,
        "text_params": {
            "text": text,
            "text_color": [0, 0, 0, 255],
            "font_path": Value::Null,
            "font_label": Value::Null,
            "font_size_px": 24.0,
            "line_spacing": "50%",
            "width_px": width_px.max(1),
            "align": "center",
            "text_line_mode": "horizontal",
            "text_layout_mode": "normal",
            "formula_layout": normalize_formula_layout_object(None),
            "drawn_lines_layout": normalize_drawn_lines_layout_object(None),
            "vector_lines_layout": normalize_vector_lines_layout_object(None),
            "selected_face_index": 0,
            "force_bold": false,
            "force_italic": false,
            "faux_bold": false,
            "faux_bold_thicken_percent": 3.0,
            "faux_bold_expand_percent": 0.0,
            "faux_bold_sharp_corners": true,
            "faux_bold_outward_only": true,
            "faux_italic": false,
            "faux_italic_slant_deg": 14.0,
            "uppercase_text": false,
            "enable_inline_style_tags": false,
            "text_wrap_mode": "aggressive",
            "allow_moderate_trees": false,
            "text_shape": "rectangle",
            "shape_min_width_percent": 50.0,
            "shape_variant": 5
        },
        "effects": [],
    })
}

pub(super) fn overlay_render_data_width_hint(render_data: Option<&Value>, fallback_width_px: u32) -> u32 {
    render_data
        .and_then(Value::as_object)
        .and_then(|rd| rd.get("text_params"))
        .and_then(Value::as_object)
        .and_then(|tp| tp.get("width_px"))
        .and_then(value_as_f32)
        .map(|width| width.round().max(1.0) as u32)
        .unwrap_or_else(|| fallback_width_px.max(1))
}

/// Reads the vector `global_rotation_deg` from an overlay's `render_data_json`, defaulting to 0 when
/// absent (image overlays, legacy projects). Used to compose the centering frame's VISUAL rotation
/// (raster `angle_deg` + this). Cheap traversal, safe to call per frame — mirrors
/// `overlay_render_data_width_hint`.
pub(super) fn overlay_render_data_global_rotation_deg(render_data: Option<&Value>) -> f32 {
    render_data
        .and_then(Value::as_object)
        .and_then(|rd| rd.get("text_params"))
        .and_then(Value::as_object)
        .and_then(|tp| tp.get("global_rotation_deg"))
        .and_then(value_as_f32)
        .unwrap_or(0.0)
}

pub(super) fn parse_overlay_kind(obj: &serde_json::Map<String, Value>) -> TypingOverlayKind {
    match obj
        .get("overlay_type")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("image") => TypingOverlayKind::Image,
        _ => TypingOverlayKind::Text,
    }
}

pub(super) fn normalize_overlay_storage_entry(
    obj: &serde_json::Map<String, Value>,
    page_size: [usize; 2],
) -> Option<Value> {
    let kind = parse_overlay_kind(obj);
    let page_idx = obj
        .get("img_idx")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())?;
    let file_raw = obj
        .get("file")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let file_name = Path::new(file_raw)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())?;
    // Geometry decode through the SINGLE shared codec (center, rotation, scale, deform).
    let placement =
        crate::models::layer_model::text_payload::decode_overlay_placement(obj, page_size);
    let center_page_px = [placement.transform.cx, placement.transform.cy];
    let rotation_deg = placement.transform.rotation.to_degrees();
    let scale = placement.transform.scale;
    let deform_mesh = placement
        .deform
        .as_ref()
        .and_then(|rec| TypingOverlayDeformMesh::from_deform_rec(rec, page_size))
        .map(|mesh| normalize_deform_mesh_resolution(&mesh, page_size));
    let mask_clip_enabled = obj
        .get("mask_clip_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let layer_idx = obj
        .get("layer_idx")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(0);
    let render_data = if kind == TypingOverlayKind::Text {
        let fallback_width_px = legacy_fallback_width_px(obj);
        Some(
            parse_overlay_render_data_json(obj, fallback_width_px).unwrap_or_else(|| {
                default_render_data_for_text(
                    obj.get("text").and_then(Value::as_str).unwrap_or_default(),
                    fallback_width_px,
                )
            }),
        )
    } else {
        Some(parse_image_overlay_render_data(obj))
    };
    let original_file_name = if kind == TypingOverlayKind::Image {
        parse_overlay_original_file_name(obj)
    } else {
        None
    };

    // Preserve an existing stable id, or mint one so pre-uid overlays acquire it on this rewrite.
    let uid = obj
        .get("uid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| crate::models::layer_model::text_payload::stable_overlay_uid(&file_name));
    Some(build_storage_overlay_entry(
        &uid,
        kind,
        page_idx,
        file_name.as_str(),
        original_file_name.as_deref(),
        center_page_px,
        mask_clip_enabled,
        layer_idx,
        rotation_deg,
        scale,
        deform_mesh,
        render_data,
    ))
}

pub(super) fn decode_overlay_from_storage_entry(
    text_images_dir: &Path,
    obj: &serde_json::Map<String, Value>,
    page_size: [usize; 2],
) -> Option<TypingOverlayDecoded> {
    let kind = parse_overlay_kind(obj);
    let page_idx = obj
        .get("img_idx")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())?;
    let file_raw = obj
        .get("file")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let file_name = Path::new(file_raw)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())?;
    let image_path = text_images_dir.join(&file_name);
    let decoded = image::open(&image_path).ok()?.to_rgba8();
    let (w, h) = decoded.dimensions();
    if w == 0 || h == 0 {
        return None;
    }

    // Geometry decode (center, rotation, scale, deform incl. transform_uv) goes through the SINGLE
    // shared codec so the typing tab and the doc resolve legacy formats identically.
    let placement =
        crate::models::layer_model::text_payload::decode_overlay_placement(obj, page_size);
    let center_page_px = [placement.transform.cx, placement.transform.cy];
    let user_scale = placement.transform.scale;
    let angle_deg = placement.transform.rotation.to_degrees();
    let deform_mesh = placement
        .deform
        .as_ref()
        .and_then(|rec| TypingOverlayDeformMesh::from_deform_rec(rec, page_size))
        .map(|mesh| normalize_deform_mesh_resolution(&mesh, page_size));
    let mask_clip_enabled = obj
        .get("mask_clip_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let layer_idx = obj
        .get("layer_idx")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(0);
    let render_data_json = if kind == TypingOverlayKind::Text {
        let fallback_width_px = legacy_fallback_width_px(obj);
        parse_overlay_render_data_json(obj, fallback_width_px)
    } else {
        Some(parse_image_overlay_render_data(obj))
    };
    let original_file_name = if kind == TypingOverlayKind::Image {
        parse_overlay_original_file_name(obj)
    } else {
        None
    };

    let uid = obj
        .get("uid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| crate::models::layer_model::text_payload::stable_overlay_uid(&file_name));
    Some(TypingOverlayDecoded {
        uid,
        kind,
        page_idx,
        center_page_px,
        mask_clip_enabled,
        layer_idx,
        user_scale,
        angle_deg,
        deform_mesh,
        file_name,
        original_file_name,
        render_data_json,
        size_px: [w as usize, h as usize],
        rgba: decoded.into_raw(),
        warnings: Vec::new(),
        // A legacy-loaded overlay carries no text-center info; it is recomputed only on a re-render with
        // the "Отладка центра" flag on.
        extra: RenderedTextExtraInfo::default(),
    })
}

/// Парсит имя файла исходной картинки image-оверлея (`image_original_file`), очищая путь до имени.
pub(super) fn parse_overlay_original_file_name(obj: &serde_json::Map<String, Value>) -> Option<String> {
    obj.get("image_original_file")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|file| Path::new(file).file_name().and_then(|name| name.to_str()))
        .map(|name| name.to_string())
}

/// Парсит render-data image-оверлея (только список эффектов). Отсутствие/мусор → пустые эффекты.
pub(super) fn parse_image_overlay_render_data(obj: &serde_json::Map<String, Value>) -> Value {
    let effects = obj
        .get("render_data")
        .and_then(Value::as_object)
        .and_then(|render_data| render_data.get("effects"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    json!({ "effects": effects })
}
