/*
File: tab/codec.rs

Purpose:
Serialization/parsing helpers for the typing tab: converting between stored
overlay JSON (render data / render params) and the in-memory typed parameter
structs, plus the legacy-format normalization and storage-entry
encode/decode/normalize routines.

Main responsibilities:
- parse `render_data` / render-param JSON into `TextRenderParams` and the
  per-layout parameter structs;
- parse config-string enums (shape, wrap mode, anti-aliasing, line mode, etc.);
- build, normalize, and decode overlay storage entries, including legacy formats.

Notes:
Extracted verbatim from `tab.rs`. Free fns are `pub(super)` so `tab.rs` and
sibling submodules of `tab` can use them. `use super::*;` pulls in the parent
module's types and imports.
*/

use super::*;

pub(super) fn text_render_params_from_render_data(render_data: &Value) -> Option<TextRenderParams> {
    let render_obj = render_data.as_object()?;
    let text_params = render_obj.get("text_params")?.as_object()?;
    // The renderer now references fonts by NAME (resolved through the provider).
    // Derive the working name from the persisted keys, newest first, and fall back
    // to the legacy `font_path` file stem so old projects keep opening. Bail only
    // when NONE of these yields a non-empty name.
    let read_name = |key: &str| {
        text_params
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    };
    let font_name = read_name("font_label")
        .or_else(|| read_name("font_family"))
        .or_else(|| read_name("font"))
        .or_else(|| {
            text_params
                .get("font_path")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .and_then(|path| {
                    Path::new(path)
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .map(ToOwned::to_owned)
                })
        })?;
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
        uppercase_text: text_params
            .get("uppercase_text")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        trim_extra_spaces: text_params
            .get("trim_extra_spaces")
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
    })
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
    let font_path = obj
        .get("font_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let font_label = obj
        .get("font_label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            obj.get("font_family")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            obj.get("font")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        });
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
        "font_path": font_path,
        "font_label": font_label,
        "font_size_px": obj.get("font_size_px").and_then(value_as_f32).or_else(|| obj.get("font_size").and_then(value_as_f32)).or_else(|| obj.get("size").and_then(value_as_f32)).unwrap_or(24.0).max(1.0),
        "line_spacing": read_render_param_px_or_percent(obj, "line_spacing", "line_spacing_px", "line_spacing_percent", PxOrPercent::percent(50.0)).to_token(),
        "kerning": read_render_param_px_or_percent(obj, "kerning", "kerning_px", "kerning_percent", PxOrPercent::percent(0.0)).to_token(),
        "glyph_height": read_render_param_px_or_percent(obj, "glyph_height", "", "glyph_height_percent", PxOrPercent::percent(100.0)).to_token(),
        "glyph_width": read_render_param_px_or_percent(obj, "glyph_width", "", "glyph_width_percent", PxOrPercent::percent(100.0)).to_token(),
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
    if let Some(map) = params.as_object_mut() {
        for key in [
            "formed_text",
            "kerning_mode",
            "hanging_punctuation",
            "new_line_after_sentence",
            "trim_extra_spaces",
            "vertical_line_direction",
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
            "line_spacing": line_spacing.to_token(),
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
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
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
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
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
