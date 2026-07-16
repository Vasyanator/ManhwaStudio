/*
File: panel/presets_io.rs

Purpose:
Free-function helpers extracted verbatim from panel.rs for TextTab preset
persistence and text layout serde conversions.

Main responsibilities:
- load the TextTab legacy inline-tags flag;
- load and save the TextTab imported-system-fonts path list;
- load, default, and save the create presets and formula presets;
- convert formula, drawn-lines, and vector-lines layouts to and from serde_json Value.

Notes:
Uses `use super::*;` to pull in the parent module's types and imports. Moved
free fns are `pub(super)` so panel.rs and sibling submodules can call them.
*/

use super::*;

/// Читает настройку «использовать обычные inline-теги вместо машиночитаемых».
/// По умолчанию `false` — панель пишет компактный `<m ...>`. Пока не подключено к UI.
pub(super) fn load_text_tab_use_legacy_inline_tags() -> bool {
    let user_settings_file = config::user_config_path();
    let Ok(raw) = fs::read_to_string(user_settings_file) else {
        return false;
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    payload
        .get("TextTab")
        .and_then(Value::as_object)
        .and_then(|text_tab| text_tab.get(TEXT_TAB_USE_LEGACY_INLINE_TAGS_KEY))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(super) fn load_text_tab_create_presets() -> HashMap<String, TypingCreatePreset> {
    let user_settings_file = config::user_config_path();
    let Ok(raw) = fs::read_to_string(user_settings_file) else {
        return HashMap::new();
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return HashMap::new();
    };
    let Some(presets_obj) = payload
        .get("TextTab")
        .and_then(Value::as_object)
        .and_then(|text_tab| text_tab.get(TEXT_TAB_CREATE_PRESETS_KEY))
        .and_then(Value::as_object)
    else {
        return HashMap::new();
    };

    let mut out = HashMap::new();
    for (name, raw_preset) in presets_obj {
        let Some(obj) = raw_preset.as_object() else {
            continue;
        };
        let primary_font_key = obj
            .get("primary_font_key")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if primary_font_key.is_empty() {
            continue;
        }
        let primary_font_path = obj
            .get("primary_font_path")
            .and_then(Value::as_str)
            .map(str::to_string);
        let primary_font_label = obj
            .get("primary_font_label")
            .and_then(Value::as_str)
            .map(str::to_string);
        let font_profiles = obj
            .get("font_profiles")
            .and_then(Value::as_object)
            .map(|profiles| {
                profiles
                    .iter()
                    .map(|(font_key, profile)| (font_key.clone(), profile.clone()))
                    .collect::<HashMap<String, Value>>()
            })
            .unwrap_or_default();
        out.insert(
            name.clone(),
            TypingCreatePreset {
                primary_font_key,
                primary_font_path,
                primary_font_label,
                font_profiles,
            },
        );
    }
    out
}

pub(super) fn default_text_tab_formula_presets() -> HashMap<String, TypingFormulaPreset> {
    let mut out = HashMap::<String, TypingFormulaPreset>::new();
    out.insert(
        "Дуга (мягкая)".to_string(),
        formula_preset(
            "t * w",
            "120 * sin((t - 0.5) * pi)",
            "0",
            true,
            1.25,
            [0.0; TEXT_FORMULA_USER_VAR_COUNT],
        ),
    );
    out.insert(
        "Наклонная линия".to_string(),
        formula_preset(
            "t * w",
            "0.35 * t * w",
            "0",
            false,
            1.1,
            [0.0; TEXT_FORMULA_USER_VAR_COUNT],
        ),
    );
    out.insert(
        "Волна".to_string(),
        formula_preset(
            "t * w",
            "80 * sin(2 * pi * t)",
            "0.15 * sin(2 * pi * t)",
            false,
            1.2,
            [0.0; TEXT_FORMULA_USER_VAR_COUNT],
        ),
    );
    out.insert(
        "Спираль".to_string(),
        formula_preset(
            "(a + b * t) * cos(c * tau * t)",
            "(a + b * t) * sin(c * tau * t)",
            "0",
            true,
            1.35,
            [40.0, 180.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out.insert(
        "Экспонента".to_string(),
        formula_preset(
            "t * w",
            "140 * (exp(a * t) - 1) / (exp(a) - 1)",
            "0",
            true,
            1.2,
            [3.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out.insert(
        "Парабола".to_string(),
        formula_preset(
            "t * w",
            "a * pow(2 * t - 1, 2) - b",
            "0",
            true,
            1.15,
            [180.0, 50.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out.insert(
        "Пульс".to_string(),
        formula_preset(
            "t * w",
            "a * pow(sin(pi * t), b) * sin(c * tau * t)",
            "0",
            false,
            1.08,
            [140.0, 8.0, 2.5, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out.insert(
        "Лемниската".to_string(),
        formula_preset(
            "a * cos(tau * t) / (1 + pow(sin(tau * t), 2))",
            "b * sin(tau * t) * cos(tau * t) / (1 + pow(sin(tau * t), 2))",
            "0",
            true,
            1.25,
            [240.0, 220.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out.insert(
        "Сердце".to_string(),
        formula_preset(
            "16 * a * pow(sin(tau * t), 3)",
            "-(13 * a * cos(tau * t) - 5 * a * cos(2 * tau * t) - 2 * a * cos(3 * tau * t) - a * cos(4 * tau * t))",
            "0",
            true,
            1.4,
            [10.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out.insert(
        "Капля".to_string(),
        formula_preset(
            "a * (1 - c * sin(tau * t)) * cos(tau * t)",
            "b * (1 - c * sin(tau * t)) * sin(tau * t)",
            "0",
            true,
            1.22,
            [180.0, 210.0, 0.35, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out.insert(
        "Вертикальная волна".to_string(),
        formula_preset(
            "90 * sin(2 * pi * t)",
            "a * (t - 0.5)",
            "0",
            true,
            1.18,
            [360.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out
}

pub(super) fn formula_preset(
    x_expr: &str,
    y_expr: &str,
    rotation_expr: &str,
    use_tangent_rotation: bool,
    letter_spacing_mul: f32,
    vars: [f32; TEXT_FORMULA_USER_VAR_COUNT],
) -> TypingFormulaPreset {
    TypingFormulaPreset {
        layout: TextFormulaLayoutParams {
            x_expr: x_expr.to_string(),
            y_expr: y_expr.to_string(),
            rotation_expr: rotation_expr.to_string(),
            use_tangent_rotation,
            t_start: 0.0,
            t_end: 1.0,
            offset_x_px: 0.0,
            offset_y_px: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
            normal_offset_px: 0.0,
            letter_spacing_mul,
            letter_spacing_px: 0.0,
            vars,
        },
    }
}

pub(super) fn text_formula_layout_from_value(value: &Value) -> Option<TextFormulaLayoutParams> {
    let obj = value.as_object()?;
    let mut out = TextFormulaLayoutParams::default();
    if let Some(raw) = obj.get("x_expr").and_then(Value::as_str) {
        out.x_expr = raw.to_string();
    }
    if let Some(raw) = obj.get("y_expr").and_then(Value::as_str) {
        out.y_expr = raw.to_string();
    }
    if let Some(raw) = obj.get("rotation_expr").and_then(Value::as_str) {
        out.rotation_expr = raw.to_string();
    }
    out.use_tangent_rotation = obj
        .get("use_tangent_rotation")
        .and_then(Value::as_bool)
        .unwrap_or(out.use_tangent_rotation);
    out.t_start = obj
        .get("t_start")
        .and_then(value_as_f32)
        .unwrap_or(out.t_start);
    out.t_end = obj.get("t_end").and_then(value_as_f32).unwrap_or(out.t_end);
    out.offset_x_px = obj
        .get("offset_x_px")
        .and_then(value_as_f32)
        .unwrap_or(out.offset_x_px);
    out.offset_y_px = obj
        .get("offset_y_px")
        .and_then(value_as_f32)
        .unwrap_or(out.offset_y_px);
    out.scale_x = obj
        .get("scale_x")
        .and_then(value_as_f32)
        .unwrap_or(out.scale_x);
    out.scale_y = obj
        .get("scale_y")
        .and_then(value_as_f32)
        .unwrap_or(out.scale_y);
    out.normal_offset_px = obj
        .get("normal_offset_px")
        .and_then(value_as_f32)
        .unwrap_or(out.normal_offset_px);
    out.letter_spacing_mul = obj
        .get("letter_spacing_mul")
        .and_then(value_as_f32)
        .unwrap_or(out.letter_spacing_mul)
        .clamp(0.0, 8.0);
    out.letter_spacing_px = obj
        .get("letter_spacing_px")
        .and_then(value_as_f32)
        .unwrap_or(out.letter_spacing_px)
        .clamp(-10_000.0, 10_000.0);
    if let Some(vars) = obj.get("vars").and_then(Value::as_array) {
        for (idx, value) in vars.iter().take(TEXT_FORMULA_USER_VAR_COUNT).enumerate() {
            if let Some(parsed) = value_as_f32(value) {
                out.vars[idx] = parsed;
            }
        }
    }
    Some(out)
}

pub(super) fn text_drawn_lines_layout_from_value(value: &Value) -> Option<TextDrawnLinesLayoutParams> {
    let obj = value.as_object()?;
    let defaults = TextDrawnLinesLayoutParams::default();
    Some(TextDrawnLinesLayoutParams {
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
    })
}

pub(super) fn text_vector_lines_layout_from_value(value: &Value) -> Option<TextVectorLinesLayoutParams> {
    let obj = value.as_object()?;
    let defaults = TextVectorLinesLayoutParams::default();
    let lines = obj
        .get("lines")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(text_vector_line_from_value)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(TextVectorLinesLayoutParams {
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
    })
}

pub(super) fn text_vector_line_from_value(value: &Value) -> Option<TextVectorLine> {
    let obj = value.as_object()?;
    let points = obj
        .get("points")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(text_vector_point_from_value)
        .collect::<Vec<_>>();
    Some(TextVectorLine {
        points,
        corner_smoothing_px: obj
            .get("corner_smoothing_px")
            .and_then(value_as_f32)
            .unwrap_or(0.0)
            .clamp(0.0, 256.0),
        text_direction: text_vector_line_text_direction_from_value(obj.get("text_direction")),
        distance_mode: text_vector_line_distance_mode_from_value(obj.get("distance_mode")),
        flip_text: obj
            .get("flip_text")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

pub(super) fn text_vector_point_from_value(value: &Value) -> Option<TextVectorPoint> {
    let obj = value.as_object()?;
    Some(TextVectorPoint {
        x: obj.get("x").and_then(value_as_f32)?,
        y: obj.get("y").and_then(value_as_f32)?,
    })
}

pub(super) fn text_formula_layout_to_value(layout: &TextFormulaLayoutParams) -> Value {
    json!({
        "x_expr": layout.x_expr.as_str(),
        "y_expr": layout.y_expr.as_str(),
        "rotation_expr": layout.rotation_expr.as_str(),
        "use_tangent_rotation": layout.use_tangent_rotation,
        "t_start": layout.t_start,
        "t_end": layout.t_end,
        "offset_x_px": layout.offset_x_px,
        "offset_y_px": layout.offset_y_px,
        "scale_x": layout.scale_x,
        "scale_y": layout.scale_y,
        "normal_offset_px": layout.normal_offset_px,
        "letter_spacing_mul": layout.letter_spacing_mul,
        "letter_spacing_px": layout.letter_spacing_px,
        "vars": layout.vars,
    })
}

pub(super) fn text_drawn_lines_layout_to_value(layout: &TextDrawnLinesLayoutParams) -> Value {
    json!({
        "use_tangent_rotation": layout.use_tangent_rotation,
        "static_rotation_rad": layout.static_rotation_rad,
        "normal_offset_px": layout.normal_offset_px,
        "letter_spacing_mul": layout.letter_spacing_mul,
        "letter_spacing_px": layout.letter_spacing_px,
        "color_tolerance": layout.color_tolerance,
        "continuation_alpha": layout.continuation_alpha,
        "start_alpha": layout.start_alpha,
    })
}

pub(super) fn text_vector_lines_layout_to_value(layout: &TextVectorLinesLayoutParams) -> Value {
    let lines = layout
        .lines
        .iter()
        .map(|line| {
            let points = line
                .points
                .iter()
                .map(|point| json!({ "x": point.x, "y": point.y }))
                .collect::<Vec<_>>();
            json!({
                "points": points,
                "corner_smoothing_px": line.corner_smoothing_px,
                "text_direction": text_vector_line_text_direction_to_str(line.text_direction),
                "distance_mode": text_vector_line_distance_mode_to_str(line.distance_mode),
                "flip_text": line.flip_text,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "width_px": layout.width_px.max(1),
        "height_px": layout.height_px.max(1),
        "use_tangent_rotation": layout.use_tangent_rotation,
        "static_rotation_rad": layout.static_rotation_rad,
        "normal_offset_px": layout.normal_offset_px,
        "letter_spacing_mul": layout.letter_spacing_mul,
        "letter_spacing_px": layout.letter_spacing_px,
        "lines": lines,
    })
}

pub(super) fn text_vector_line_text_direction_to_str(direction: TextVectorLineTextDirection) -> &'static str {
    match direction {
        TextVectorLineTextDirection::LeftToRight => "left_to_right",
        TextVectorLineTextDirection::RightToLeft => "right_to_left",
    }
}

pub(super) fn text_vector_line_text_direction_from_value(
    value: Option<&Value>,
) -> TextVectorLineTextDirection {
    match value.and_then(Value::as_str).unwrap_or("left_to_right") {
        "right_to_left" | "rtl" => TextVectorLineTextDirection::RightToLeft,
        "left_to_right" | "ltr" => TextVectorLineTextDirection::LeftToRight,
        _ => TextVectorLineTextDirection::LeftToRight,
    }
}

pub(super) fn text_vector_line_distance_mode_to_str(mode: TextVectorLineDistanceMode) -> &'static str {
    match mode {
        TextVectorLineDistanceMode::ByLineLength => "by_line_length",
        TextVectorLineDistanceMode::MinimumPreviousDistance => "minimum_previous_distance",
    }
}

pub(super) fn text_vector_line_distance_mode_from_value(value: Option<&Value>) -> TextVectorLineDistanceMode {
    match value.and_then(Value::as_str).unwrap_or("by_line_length") {
        "minimum_previous_distance" | "min_previous_distance" | "minimum_distance" => {
            TextVectorLineDistanceMode::MinimumPreviousDistance
        }
        "by_line_length" | "line_length" => TextVectorLineDistanceMode::ByLineLength,
        _ => TextVectorLineDistanceMode::ByLineLength,
    }
}

pub(super) fn load_text_tab_formula_presets() -> HashMap<String, TypingFormulaPreset> {
    let fallback = default_text_tab_formula_presets();
    let user_settings_file = config::user_config_path();
    let Ok(raw) = fs::read_to_string(user_settings_file) else {
        return fallback;
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return fallback;
    };
    let Some(presets_obj) = payload
        .get("TextTab")
        .and_then(Value::as_object)
        .and_then(|text_tab| text_tab.get(TEXT_TAB_FORMULA_PRESETS_KEY))
        .and_then(Value::as_object)
    else {
        return fallback;
    };

    let mut out = fallback;
    for (name, raw_preset) in presets_obj {
        let Some(layout) = text_formula_layout_from_value(raw_preset) else {
            continue;
        };
        out.insert(name.clone(), TypingFormulaPreset { layout });
    }
    out
}

/// Loads the per-effect-kind default overrides from `TextTab.effect_defaults`.
///
/// The returned map is keyed by the effect discriminator string (e.g. `"stroke"`,
/// `"glow_v1"`); each value is the one-card JSON object stored earlier. A missing key,
/// unreadable file, or malformed JSON yields an empty map — this never panics and never
/// surfaces an error, because absence simply means "use the built-in defaults".
pub(super) fn load_text_tab_effect_defaults() -> HashMap<String, Value> {
    let user_settings_file = config::user_config_path();
    let Ok(raw) = fs::read_to_string(user_settings_file) else {
        return HashMap::new();
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return HashMap::new();
    };
    let Some(defaults_obj) = payload
        .get("TextTab")
        .and_then(Value::as_object)
        .and_then(|text_tab| text_tab.get(TEXT_TAB_EFFECT_DEFAULTS_KEY))
        .and_then(Value::as_object)
    else {
        return HashMap::new();
    };
    defaults_obj
        .iter()
        .filter(|(_, value)| value.is_object())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// Persists the whole per-effect-kind default map to `TextTab.effect_defaults`
/// (read-modify-write of `user_config.json`, mirroring `save_text_tab_formula_presets`).
///
/// The caller passes the FULL current snapshot; keys are written sorted for a stable
/// file. Returns a human-readable error string on any I/O or serialization failure.
/// Callers must invoke this off the GUI thread.
pub(super) fn save_text_tab_effect_defaults(defaults: &HashMap<String, Value>) -> Result<(), String> {
    save_text_tab_effect_defaults_to(&config::user_config_path(), defaults)
}

/// Path-parameterized effect-default saver used by tests to verify the same
/// serialized transaction as the production `user_config.json` writer.
fn save_text_tab_effect_defaults_to(
    user_settings_file: &Path,
    defaults: &HashMap<String, Value>,
) -> Result<(), String> {
    let mut defaults_obj = Map::new();
    let mut keys: Vec<&String> = defaults.keys().collect();
    keys.sort();
    for key in keys {
        if let Some(value) = defaults.get(key) {
            defaults_obj.insert(key.clone(), value.clone());
        }
    }
    update_text_tab_value(
        user_settings_file,
        TEXT_TAB_EFFECT_DEFAULTS_KEY,
        Value::Object(defaults_obj),
    )
}

/// Updates one `TextTab` member through config's serialized user-config transaction.
/// Existing malformed JSON is reported instead of being replaced by this subsection.
fn update_text_tab_value(user_settings_file: &Path, key: &str, value: Value) -> Result<(), String> {
    config::update_user_config_file(user_settings_file, move |root| {
        let root_obj = root
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("user config root must be an object"))?;
        let text_tab = root_obj
            .entry("TextTab".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !text_tab.is_object() {
            *text_tab = Value::Object(Map::new());
        }
        let text_tab_obj = text_tab
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("TextTab must be an object"))?;
        text_tab_obj.insert(key.to_string(), value);
        Ok(())
    })
    .map_err(|err| err.to_string())
}

/// Loads the user-imported system font FILE paths from `TextTab.imported_system_fonts`.
///
/// The stored value is an array of strings; each non-empty string element maps to a
/// `PathBuf`. Non-string or empty elements are skipped. A missing key, unreadable file,
/// or malformed JSON yields an empty vector — this never panics and never surfaces an
/// error, mirroring `load_text_tab_effect_defaults`.
pub(in crate::tabs::typing) fn load_text_tab_imported_system_fonts() -> Vec<PathBuf> {
    load_imported_system_fonts_from(&config::user_config_path())
}

/// Path-parameterized core of `load_text_tab_imported_system_fonts`, split out so the
/// read logic can be unit-tested against a temp file instead of the real user config.
fn load_imported_system_fonts_from(user_settings_file: &Path) -> Vec<PathBuf> {
    let Ok(raw) = fs::read_to_string(user_settings_file) else {
        return Vec::new();
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let Some(array) = payload
        .get("TextTab")
        .and_then(Value::as_object)
        .and_then(|text_tab| text_tab.get(crate::config::TEXT_TAB_IMPORTED_SYSTEM_FONTS_KEY))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(Value::as_str)
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Persists the user-imported system font FILE paths to `TextTab.imported_system_fonts`
/// (read-modify-write of `user_config.json`, mirroring `save_text_tab_effect_defaults`
/// so sibling keys survive). Paths are stored as an array of strings via
/// `path.to_string_lossy()`. Returns a human-readable error on I/O or serialization
/// failure. Callers must invoke this off the GUI thread. Wired off-thread by
/// `font_settings_store::persist_off_thread` (the store's add/remove mutators, driven by
/// the settings font-settings UI).
///
/// Always compiled (including the test profile) so the real save API type-checks under
/// tests. Under test its sole caller `persist_off_thread` early-returns before invoking it,
/// so it is unreached there — hence `#[cfg_attr(test, allow(dead_code))]`. The
/// read-modify-write recipe is unit-tested through `save_imported_system_fonts_to`.
#[cfg_attr(test, allow(dead_code))]
pub(in crate::tabs::typing) fn save_text_tab_imported_system_fonts(
    paths: &[PathBuf],
) -> Result<(), String> {
    save_imported_system_fonts_to(&config::user_config_path(), paths)
}

/// Path-parameterized core of `save_text_tab_imported_system_fonts`, split out so the
/// read-modify-write recipe can be unit-tested against a temp file.
fn save_imported_system_fonts_to(
    user_settings_file: &Path,
    paths: &[PathBuf],
) -> Result<(), String> {
    let array = paths
        .iter()
        .map(|path| Value::String(path.to_string_lossy().to_string()))
        .collect::<Vec<_>>();
    update_text_tab_value(
        user_settings_file,
        crate::config::TEXT_TAB_IMPORTED_SYSTEM_FONTS_KEY,
        Value::Array(array),
    )
}

pub(super) fn save_text_tab_create_presets(
    presets: &HashMap<String, TypingCreatePreset>,
) -> Result<(), String> {
    let mut presets_obj = Map::new();
    let mut names: Vec<&String> = presets.keys().collect();
    names.sort();
    for name in names {
        let Some(preset) = presets.get(name) else {
            continue;
        };
        if preset.primary_font_key.trim().is_empty() {
            continue;
        }
        let mut font_profiles_obj = Map::new();
        let mut font_keys: Vec<&String> = preset.font_profiles.keys().collect();
        font_keys.sort();
        for font_key in font_keys {
            if let Some(profile) = preset.font_profiles.get(font_key) {
                font_profiles_obj.insert(font_key.clone(), profile.clone());
            }
        }
        presets_obj.insert(
            name.clone(),
            json!({
                "primary_font_key": preset.primary_font_key,
                "primary_font_path": preset.primary_font_path,
                "primary_font_label": preset.primary_font_label,
                "font_profiles": font_profiles_obj,
            }),
        );
    }
    update_text_tab_value(
        &config::user_config_path(),
        TEXT_TAB_CREATE_PRESETS_KEY,
        Value::Object(presets_obj),
    )
}

pub(super) fn save_text_tab_formula_presets(
    presets: &HashMap<String, TypingFormulaPreset>,
) -> Result<(), String> {
    let mut presets_obj = Map::new();
    let mut names: Vec<&String> = presets.keys().collect();
    names.sort();
    for name in names {
        let Some(preset) = presets.get(name) else {
            continue;
        };
        presets_obj.insert(name.clone(), text_formula_layout_to_value(&preset.layout));
    }
    update_text_tab_value(
        &config::user_config_path(),
        TEXT_TAB_FORMULA_PRESETS_KEY,
        Value::Object(presets_obj),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Returns a unique temp file path for a config round-trip test, so parallel tests
    /// never share a file and the real user config is never touched.
    fn unique_temp_config_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("ms_test_{tag}_{nanos}.json"))
    }

    #[test]
    fn imported_system_fonts_round_trip_through_temp_config() {
        let path = unique_temp_config_path("imported_fonts_roundtrip");
        let fonts = vec![
            PathBuf::from("/fonts/Roboto-Regular.ttf"),
            PathBuf::from("/usr/share/fonts/NotoSans.otf"),
        ];
        save_imported_system_fonts_to(&path, &fonts).expect("save must succeed");
        let loaded = load_imported_system_fonts_from(&path);
        assert_eq!(loaded, fonts);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn imported_system_fonts_missing_file_is_empty() {
        let path = unique_temp_config_path("imported_fonts_missing");
        // The file was never created, so loading must yield an empty list, not panic.
        assert!(load_imported_system_fonts_from(&path).is_empty());
    }

    #[test]
    fn imported_system_fonts_save_preserves_sibling_keys() {
        let path = unique_temp_config_path("imported_fonts_siblings");
        // Seed a config that already carries an unrelated TextTab key.
        let seed = json!({ "TextTab": { "use_system_fonts": true } });
        fs::write(&path, serde_json::to_string(&seed).expect("serialize seed"))
            .expect("write seed");
        save_imported_system_fonts_to(&path, &[PathBuf::from("/fonts/A.ttf")])
            .expect("save must succeed");
        let raw = fs::read_to_string(&path).expect("read back");
        let value: Value = serde_json::from_str(&raw).expect("parse back");
        assert_eq!(
            value
                .get("TextTab")
                .and_then(|t| t.get("use_system_fonts"))
                .and_then(Value::as_bool),
            Some(true),
            "sibling key must survive the imported-fonts write"
        );
        assert_eq!(
            load_imported_system_fonts_from(&path),
            vec![PathBuf::from("/fonts/A.ttf")]
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn imported_system_fonts_skips_non_string_and_empty_elements() {
        let path = unique_temp_config_path("imported_fonts_skip");
        // Mixed array: one valid path, an empty string, and a non-string element.
        let seed = json!({
            "TextTab": { "imported_system_fonts": ["/fonts/A.ttf", "", 42] }
        });
        fs::write(&path, serde_json::to_string(&seed).expect("serialize seed"))
            .expect("write seed");
        assert_eq!(
            load_imported_system_fonts_from(&path),
            vec![PathBuf::from("/fonts/A.ttf")]
        );
        let _ = fs::remove_file(&path);
    }

    /// Effect-default persistence and the ORT crash marker share the serialized
    /// user-config transaction, so neither full-file update can erase the other.
    #[test]
    fn effect_defaults_save_preserves_concurrent_ort_marker() -> anyhow::Result<()> {
        let path = unique_temp_config_path("effect_defaults_ort_marker");
        let seed = json!({"General": {"other": true}});
        fs::write(&path, serde_json::to_string(&seed)?)?;

        let mut defaults = HashMap::new();
        defaults.insert("stroke".to_string(), json!({"width": 3}));
        let effect_path = path.clone();
        let effect_thread = std::thread::spawn(move || {
            save_text_tab_effect_defaults_to(&effect_path, &defaults)
        });
        let marker_path = path.clone();
        let marker_thread = std::thread::spawn(move || {
            config::update_user_config_file(&marker_path, |root| {
                let root_obj = root
                    .as_object_mut()
                    .ok_or_else(|| anyhow::anyhow!("test root must be an object"))?;
                let general = root_obj
                    .entry("General".to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
                let general_obj = general
                    .as_object_mut()
                    .ok_or_else(|| anyhow::anyhow!("test General must be an object"))?;
                general_obj.insert(
                    crate::config::GENERAL_ORT_LOAD_STATE_KEY.to_string(),
                    json!({"cpu@1.20.1": {"attempted": true, "succeeded": false}}),
                );
                Ok(())
            })
        });

        let effect_result = effect_thread
            .join()
            .map_err(|_| anyhow::anyhow!("effect-default worker panicked"))?;
        effect_result.map_err(anyhow::Error::msg)?;
        marker_thread
            .join()
            .map_err(|_| anyhow::anyhow!("ORT-marker worker panicked"))??;

        let saved: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
        assert_eq!(
            saved.pointer("/TextTab/effect_defaults/stroke/width"),
            Some(&Value::from(3))
        );
        assert_eq!(
            saved.pointer("/General/ort_load_state/cpu@1.20.1/attempted"),
            Some(&Value::Bool(true))
        );
        fs::remove_file(&path)?;
        Ok(())
    }

    /// A malformed config is never replaced by a one-section effect-default file.
    #[test]
    fn malformed_config_makes_effect_defaults_save_fail_without_truncation() -> anyhow::Result<()> {
        let path = unique_temp_config_path("effect_defaults_malformed");
        let malformed = "{ this is not valid JSON";
        fs::write(&path, malformed)?;
        let mut defaults = HashMap::new();
        defaults.insert("stroke".to_string(), json!({"width": 3}));

        assert!(save_text_tab_effect_defaults_to(&path, &defaults).is_err());
        assert_eq!(fs::read_to_string(&path)?, malformed);
        fs::remove_file(&path)?;
        Ok(())
    }
}
