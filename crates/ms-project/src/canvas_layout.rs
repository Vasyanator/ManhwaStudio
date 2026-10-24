/*
File: crates/ms-project/src/canvas_layout.rs

Purpose:
The one owner of the canvas LAYOUT presets: the preset-owned subset of `CanvasSettings` (13
fields: page separation and spacing, aside-bubble geometry and placement, default display types,
on-top focus), the factory values of the built-in presets «Pages» and «Webtoon», the layout's JSON
codec in a title's `settings.json`, the per-title migration rule and the per-title preset
selection. Every canvas default in the workspace (`CanvasSettings::default` here,
`SharedCanvasSettings::default` in `ms-models`, `CanvasState::default` in `ms-canvas`) takes its
layout fields from this table.

Key structures:
- CanvasLayout: the 13 fields in their persisted vocabulary (px as `i32`, modes as tokens)
- BuiltinLayoutPreset: `Pages` / `Webtoon`, with `factory()` = THE table
- BASE_BUILTIN: the preset the defaults are built from
- LayoutPresetSelection: the title's selected preset (`canvas.layout_preset`)
- CanvasLayoutPresets: the app-wide preset store (`user_config` `Canvas.layout_presets`):
  built-in overrides and user presets
- LayoutPresetOp / LayoutPresetStoreError: one persisted store operation and its refusal

Key functions:
- CanvasLayout::of() / apply_to() / normalized() / from_json_object() / write_json_object()
- BuiltinLayoutPreset::factory() / id() / from_id() / for_kind() / display_name()
- read_title_layout(): the per-title read with the legacy migration rule
- save_layout_preset_selection() / seed_image_set_title(): the two `settings.json` writers here
- CanvasLayoutPresets::effective_builtin(): what a built-in means NOW (its override, else the
  factory row); the one source of every built-in apply (title fallback, image sets, first-open
  prompt, picker selection)
- apply_layout_preset_op() / apply_layout_preset_op_to_user_config(): the store's writer

Notes:
- The layout is stored PER TITLE in the `canvas` object of its `settings.json` (keys
  `CanvasLayout::JSON_KEYS`), marked by `canvas.layout_per_title = true`, which the project canvas
  writer (`ms-canvas` `save_canvas_settings_to_project_file`) sets with every save. A title without
  the marker was last saved by an older build that kept six of the fields in the user config:
  `read_title_layout` reproduces what that build showed (see its declaration comment), and the
  first save of the new build writes everything plus the marker.
- `comic_type` (the title KIND) is not part of the layout and is never derived from it; it only
  picks the fallback preset (`BuiltinLayoutPreset::for_kind`).
- Every mapping between `CanvasLayout` and another settings type destructures the layout WITHOUT
  `..`, so a new field is a compile error at each mapping site.
- The two presets differ ONLY in `aside_compact_mode` and `separate_pages` (`WEBTOON` is spelled
  as `..PAGES`).
- Built-in overrides: the user may edit «Pages» / «Webtoon»; the edit is stored under the
  built-in's id while it differs from the factory row. Every place that APPLIES a built-in uses
  the effective values; only a revert and the legacy compact-mode recovery of
  `read_title_layout` use the factory row, which stays the single owner of the defaults.
- Pure except the three documented writers: the two title writers go through
  `ms_docstore::update`, the store writer through `ms_config::update_user_config_file`.
*/

use crate::{CanvasSettings, ComicType};
use ms_docstore::{DocKind, DocRef, Durability, WriteOptions};
use ms_log::runtime_log;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// The preset-owned subset of [`CanvasSettings`] (the 13 layout fields), in its persisted
/// vocabulary: pixel lengths as integer px, modes as their lowercase config tokens.
///
/// Every mapping destructures it WITHOUT `..`, so a new field is a compile error at each site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanvasLayout {
    pub separate_pages: bool,
    pub page_spacing_px: i32,
    pub vertical_edge_margin_px: i32,
    pub aside_compact_mode: String,
    pub aside_min_width_px: i32,
    pub aside_max_width_px: i32,
    pub aside_scale_pct: i32,
    pub aside_side_mode: String,
    pub scale_bubbles: bool,
    pub aside_second_column: bool,
    pub editable_bubble_type: String,
    pub readonly_bubble_type: String,
    pub on_top_focus_mode: String,
}

impl CanvasLayout {
    /// JSON key of every field, in declaration order. The title `canvas` object spells the layout
    /// with exactly these keys (the reader `canvas_settings_from_config` reads them).
    pub const JSON_KEYS: [&'static str; 13] = [
        "separate_pages",
        "page_spacing_px",
        "vertical_edge_margin_px",
        "aside_compact_mode",
        "aside_min_width_px",
        "aside_max_width_px",
        "aside_scale_pct",
        "aside_side_mode",
        "scale_bubbles",
        "aside_second_column",
        "editable_bubble_type",
        "readonly_bubble_type",
        "on_top_focus_mode",
    ];

    /// The layout fields of `settings`, copied verbatim (no normalization).
    #[must_use]
    pub fn of(settings: &CanvasSettings) -> Self {
        Self {
            separate_pages: settings.separate_pages,
            page_spacing_px: settings.page_spacing_px,
            vertical_edge_margin_px: settings.vertical_edge_margin_px,
            aside_compact_mode: settings.aside_compact_mode.clone(),
            aside_min_width_px: settings.aside_min_width_px,
            aside_max_width_px: settings.aside_max_width_px,
            aside_scale_pct: settings.aside_scale_pct,
            aside_side_mode: settings.aside_side_mode.clone(),
            scale_bubbles: settings.scale_bubbles,
            aside_second_column: settings.aside_second_column,
            editable_bubble_type: settings.editable_bubble_type.clone(),
            readonly_bubble_type: settings.readonly_bubble_type.clone(),
            on_top_focus_mode: settings.on_top_focus_mode.clone(),
        }
    }

    /// Writes these layout fields into `settings` verbatim; every other field is left as it is.
    pub fn apply_to(&self, settings: &mut CanvasSettings) {
        let Self {
            separate_pages,
            page_spacing_px,
            vertical_edge_margin_px,
            aside_compact_mode,
            aside_min_width_px,
            aside_max_width_px,
            aside_scale_pct,
            aside_side_mode,
            scale_bubbles,
            aside_second_column,
            editable_bubble_type,
            readonly_bubble_type,
            on_top_focus_mode,
        } = self;
        settings.separate_pages = *separate_pages;
        settings.page_spacing_px = *page_spacing_px;
        settings.vertical_edge_margin_px = *vertical_edge_margin_px;
        settings.aside_compact_mode.clone_from(aside_compact_mode);
        settings.aside_min_width_px = *aside_min_width_px;
        settings.aside_max_width_px = *aside_max_width_px;
        settings.aside_scale_pct = *aside_scale_pct;
        settings.aside_side_mode.clone_from(aside_side_mode);
        settings.scale_bubbles = *scale_bubbles;
        settings.aside_second_column = *aside_second_column;
        settings.editable_bubble_type.clone_from(editable_bubble_type);
        settings.readonly_bubble_type.clone_from(readonly_bubble_type);
        settings.on_top_focus_mode.clone_from(on_top_focus_mode);
    }

    /// This layout in canonical form: mode tokens trimmed and ASCII-lowercased; the title reader's
    /// clamps applied (widths at least 40 px and max >= min, spacing / margin at least 0, scale
    /// 25..=300 %). Unknown tokens are kept (each consumer parses them totally).
    #[must_use]
    pub fn normalized(self) -> Self {
        let Self {
            separate_pages,
            page_spacing_px,
            vertical_edge_margin_px,
            aside_compact_mode,
            aside_min_width_px,
            aside_max_width_px,
            aside_scale_pct,
            aside_side_mode,
            scale_bubbles,
            aside_second_column,
            editable_bubble_type,
            readonly_bubble_type,
            on_top_focus_mode,
        } = self;
        let token = |raw: String| raw.trim().to_ascii_lowercase();
        let aside_min_width_px = aside_min_width_px.max(MIN_ASIDE_WIDTH_PX);
        Self {
            separate_pages,
            page_spacing_px: page_spacing_px.max(0),
            vertical_edge_margin_px: vertical_edge_margin_px.max(0),
            aside_compact_mode: token(aside_compact_mode),
            aside_min_width_px,
            aside_max_width_px: aside_max_width_px.max(MIN_ASIDE_WIDTH_PX).max(aside_min_width_px),
            aside_scale_pct: aside_scale_pct.clamp(25, 300),
            aside_side_mode: token(aside_side_mode),
            scale_bubbles,
            aside_second_column,
            editable_bubble_type: token(editable_bubble_type),
            readonly_bubble_type: token(readonly_bubble_type),
            on_top_focus_mode: token(on_top_focus_mode),
        }
    }
}

/// Smallest aside bubble width the reader accepts, in px.
const MIN_ASIDE_WIDTH_PX: i32 = 40;

/// A built-in canvas layout preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BuiltinLayoutPreset {
    /// Page-based comics: separated pages, moderate aside compaction.
    Pages,
    /// Continuous webtoon ribbon: no page separation, no aside compaction.
    Webtoon,
}

/// The base of every canvas default (`CanvasSettings::default` and its two mirrors).
pub const BASE_BUILTIN: BuiltinLayoutPreset = BuiltinLayoutPreset::Pages;

/// Factory values of one built-in preset in `'static` form, so the table is plain `const` data.
#[derive(Debug)]
struct FactorySpec {
    separate_pages: bool,
    page_spacing_px: i32,
    vertical_edge_margin_px: i32,
    aside_compact_mode: &'static str,
    aside_min_width_px: i32,
    aside_max_width_px: i32,
    aside_scale_pct: i32,
    aside_side_mode: &'static str,
    scale_bubbles: bool,
    aside_second_column: bool,
    editable_bubble_type: &'static str,
    readonly_bubble_type: &'static str,
    on_top_focus_mode: &'static str,
}

/// THE table, «Pages» row.
const PAGES: FactorySpec = FactorySpec {
    separate_pages: true,
    page_spacing_px: 200,
    vertical_edge_margin_px: 200,
    aside_compact_mode: "moderate",
    aside_min_width_px: 450,
    aside_max_width_px: 550,
    aside_scale_pct: 100,
    aside_side_mode: "auto",
    scale_bubbles: true,
    aside_second_column: true,
    editable_bubble_type: "aside",
    readonly_bubble_type: "aside",
    on_top_focus_mode: "around",
};

/// THE table, «Webtoon» row: «Pages» with a continuous ribbon and no aside compaction.
const WEBTOON: FactorySpec = FactorySpec {
    separate_pages: false,
    aside_compact_mode: "none",
    ..PAGES
};

impl BuiltinLayoutPreset {
    /// Every built-in preset, in display order.
    pub const ALL: [Self; 2] = [Self::Pages, Self::Webtoon];

    /// This preset's row of the table.
    const fn spec(self) -> &'static FactorySpec {
        match self {
            Self::Pages => &PAGES,
            Self::Webtoon => &WEBTOON,
        }
    }

    /// The factory values of this preset (already in normalized form).
    #[must_use]
    pub fn factory(self) -> CanvasLayout {
        let FactorySpec {
            separate_pages,
            page_spacing_px,
            vertical_edge_margin_px,
            aside_compact_mode,
            aside_min_width_px,
            aside_max_width_px,
            aside_scale_pct,
            aside_side_mode,
            scale_bubbles,
            aside_second_column,
            editable_bubble_type,
            readonly_bubble_type,
            on_top_focus_mode,
        } = self.spec();
        CanvasLayout {
            separate_pages: *separate_pages,
            page_spacing_px: *page_spacing_px,
            vertical_edge_margin_px: *vertical_edge_margin_px,
            aside_compact_mode: (*aside_compact_mode).to_string(),
            aside_min_width_px: *aside_min_width_px,
            aside_max_width_px: *aside_max_width_px,
            aside_scale_pct: *aside_scale_pct,
            aside_side_mode: (*aside_side_mode).to_string(),
            scale_bubbles: *scale_bubbles,
            aside_second_column: *aside_second_column,
            editable_bubble_type: (*editable_bubble_type).to_string(),
            readonly_bubble_type: (*readonly_bubble_type).to_string(),
            on_top_focus_mode: (*on_top_focus_mode).to_string(),
        }
    }

    /// The persistence id of this preset (`"pages"` / `"webtoon"`); never rename one.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Pages => "pages",
            Self::Webtoon => "webtoon",
        }
    }

    /// The preset with persistence id `raw` (exact match), or `None`.
    #[must_use]
    pub fn from_id(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.id() == raw)
    }

    /// The built-in preset of a title kind: `Pages` -> `Pages`, `Ribbon` -> `Webtoon`; `Custom`
    /// and an unknown kind have none.
    #[must_use]
    pub const fn for_kind(kind: Option<ComicType>) -> Option<Self> {
        match kind {
            Some(ComicType::Pages) => Some(Self::Pages),
            Some(ComicType::Ribbon) => Some(Self::Webtoon),
            Some(ComicType::Custom) | None => None,
        }
    }

    /// The preset whose values fill in what a title of kind `kind` does not store: the kind's own
    /// preset ([`Self::for_kind`]), else [`BASE_BUILTIN`].
    #[must_use]
    pub const fn fallback_for_kind(kind: Option<ComicType>) -> Self {
        match Self::for_kind(kind) {
            Some(preset) => preset,
            None => BASE_BUILTIN,
        }
    }

    /// The localized display name of this preset (Settings › «Лента и пузыри»).
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Pages => t!("settings.canvas_ribbon.preset_builtin_pages_label"),
            Self::Webtoon => t!("settings.canvas_ribbon.preset_builtin_webtoon_label"),
        }
    }
}

/// `canvas.layout_per_title`: `true` once the title's own `canvas` object holds its whole layout
/// (written by the project canvas writer and by [`seed_image_set_title`]; never backfilled).
pub const LAYOUT_PER_TITLE_KEY: &str = "layout_per_title";

/// `canvas.layout_preset`: the title's selected layout preset ([`LayoutPresetSelection::to_json`]),
/// written only by the Settings pane's saver ([`save_layout_preset_selection`]).
pub const LAYOUT_PRESET_KEY: &str = "layout_preset";

/// A persisted JSON integer as `i32`, saturating at the `i32` bounds (no wrap-around).
fn saturating_i32(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(if value < 0 { i32::MIN } else { i32::MAX })
}

impl CanvasLayout {
    /// The layout stored in a `canvas` JSON object: for every field the value under its
    /// [`Self::JSON_KEYS`] key when it has the right JSON type, else `fallback`'s; the result is
    /// [`Self::normalized`]. Never fails: a wrongly typed or missing key just takes the fallback.
    #[must_use]
    pub fn from_json_object(obj: &Map<String, Value>, fallback: &Self) -> Self {
        let Self {
            separate_pages,
            page_spacing_px,
            vertical_edge_margin_px,
            aside_compact_mode,
            aside_min_width_px,
            aside_max_width_px,
            aside_scale_pct,
            aside_side_mode,
            scale_bubbles,
            aside_second_column,
            editable_bubble_type,
            readonly_bubble_type,
            on_top_focus_mode,
        } = fallback;
        let boolean = |key: &str, fallback: bool| obj.get(key).and_then(Value::as_bool).unwrap_or(fallback);
        let int = |key: &str, fallback: i32| obj.get(key).and_then(Value::as_i64).map_or(fallback, saturating_i32);
        let token = |key: &str, fallback: &String| obj.get(key).and_then(Value::as_str).map_or_else(|| fallback.clone(), str::to_owned);
        Self {
            separate_pages: boolean("separate_pages", *separate_pages),
            page_spacing_px: int("page_spacing_px", *page_spacing_px),
            vertical_edge_margin_px: int("vertical_edge_margin_px", *vertical_edge_margin_px),
            aside_compact_mode: token("aside_compact_mode", aside_compact_mode),
            aside_min_width_px: int("aside_min_width_px", *aside_min_width_px),
            aside_max_width_px: int("aside_max_width_px", *aside_max_width_px),
            aside_scale_pct: int("aside_scale_pct", *aside_scale_pct),
            aside_side_mode: token("aside_side_mode", aside_side_mode),
            scale_bubbles: boolean("scale_bubbles", *scale_bubbles),
            aside_second_column: boolean("aside_second_column", *aside_second_column),
            editable_bubble_type: token("editable_bubble_type", editable_bubble_type),
            readonly_bubble_type: token("readonly_bubble_type", readonly_bubble_type),
            on_top_focus_mode: token("on_top_focus_mode", on_top_focus_mode),
        }
        .normalized()
    }

    /// Writes every field of `layout` under its [`Self::JSON_KEYS`] key into `obj` (verbatim; the
    /// caller normalizes); every other key of `obj` is kept.
    pub fn write_json_object(layout: &Self, obj: &mut Map<String, Value>) {
        let Self {
            separate_pages,
            page_spacing_px,
            vertical_edge_margin_px,
            aside_compact_mode,
            aside_min_width_px,
            aside_max_width_px,
            aside_scale_pct,
            aside_side_mode,
            scale_bubbles,
            aside_second_column,
            editable_bubble_type,
            readonly_bubble_type,
            on_top_focus_mode,
        } = layout;
        let mut put = |key: &str, value: Value| {
            obj.insert(key.to_owned(), value);
        };
        put("separate_pages", Value::Bool(*separate_pages));
        put("page_spacing_px", Value::from(*page_spacing_px));
        put("vertical_edge_margin_px", Value::from(*vertical_edge_margin_px));
        put("aside_compact_mode", Value::String(aside_compact_mode.clone()));
        put("aside_min_width_px", Value::from(*aside_min_width_px));
        put("aside_max_width_px", Value::from(*aside_max_width_px));
        put("aside_scale_pct", Value::from(*aside_scale_pct));
        put("aside_side_mode", Value::String(aside_side_mode.clone()));
        put("scale_bubbles", Value::Bool(*scale_bubbles));
        put("aside_second_column", Value::Bool(*aside_second_column));
        put("editable_bubble_type", Value::String(editable_bubble_type.clone()));
        put("readonly_bubble_type", Value::String(readonly_bubble_type.clone()));
        put("on_top_focus_mode", Value::String(on_top_focus_mode.clone()));
    }
}

/// The layout of one title. `settings` is the title's `settings.json` root, `user_settings` the
/// `user_config` root, `kind` the title's `comic_type`, `fallback` the values of what the title
/// does not store (normally `CanvasLayoutPresets::fallback_for_kind`: the kind's preset with the user's override).
///
/// - Marker title (`canvas.layout_per_title == true`): only the title's `canvas` object is read
///   ([`CanvasLayout::from_json_object`], normalized).
/// - Legacy title (no marker; last saved by an older build, or new and not yet saved): the read
///   of that build, so the title shows what it showed there — except `aside_compact_mode` of a
///   `pages` / `ribbon` title, which comes from its kind's FACTORY preset: the older build derived
///   the stored kind from exactly that field and `separate_pages` at its last save, while the
///   user-config copy it read first was shared by every title. The other five formerly global
///   fields (aside widths, side, second column, stretch) read `user_config` `Canvas.*` first,
///   then the title; the remaining seven read the title `canvas`, the bubble types and the
///   focus mode also the legacy title root. The first key FOUND per field decides (a wrongly
///   typed one keeps the fallback). Mode tokens are kept verbatim; the clamps are the reader's.
#[must_use]
pub(crate) fn read_title_layout(settings: &Value, user_settings: &Value, kind: Option<ComicType>, fallback: &CanvasLayout) -> CanvasLayout {
    let canvas = settings.get("canvas");
    if let Some(obj) = canvas.and_then(Value::as_object)
        && obj.get(LAYOUT_PER_TITLE_KEY).and_then(Value::as_bool) == Some(true)
    {
        return CanvasLayout::from_json_object(obj, fallback);
    }
    let user_canvas = user_settings.get("Canvas");
    let title = |key: &str| canvas.and_then(|c| c.get(key));
    let user_first = |key: &str| user_canvas.and_then(|c| c.get(key)).or_else(|| title(key));
    let title_or_root = |key: &str| title(key).or_else(|| settings.get(key));
    let int = |found: Option<&Value>| found.and_then(Value::as_i64).map(saturating_i32);
    let boolean = |found: Option<&Value>| found.and_then(Value::as_bool);
    let token = |found: Option<&Value>| found.and_then(Value::as_str).map(str::to_owned);

    let mut out = fallback.clone();
    if let Some(v) = int(user_first("aside_min_width_px")) {
        out.aside_min_width_px = v.max(MIN_ASIDE_WIDTH_PX);
    }
    if let Some(v) = int(user_first("aside_max_width_px")) {
        out.aside_max_width_px = v.max(MIN_ASIDE_WIDTH_PX);
    }
    match BuiltinLayoutPreset::for_kind(kind) {
        Some(preset) => out.aside_compact_mode = preset.factory().aside_compact_mode,
        None => {
            if let Some(v) = token(user_first("aside_compact_mode")) {
                out.aside_compact_mode = v;
            }
        }
    }
    if let Some(v) = token(user_first("aside_side_mode")) {
        out.aside_side_mode = v;
    }
    if let Some(v) = boolean(user_first("aside_second_column")) {
        out.aside_second_column = v;
    }
    if let Some(v) = boolean(user_first("scale_bubbles")) {
        out.scale_bubbles = v;
    }
    if let Some(v) = token(title_or_root("editable_bubble_type")) {
        out.editable_bubble_type = v;
    }
    if let Some(v) = token(title_or_root("readonly_bubble_type")) {
        out.readonly_bubble_type = v;
    }
    if let Some(v) = token(title_or_root("on_top_focus_mode")) {
        out.on_top_focus_mode = v;
    }
    if let Some(v) = int(title("page_spacing_px")) {
        out.page_spacing_px = v.max(0);
    }
    if let Some(v) = boolean(title("separate_pages")) {
        out.separate_pages = v;
    }
    if let Some(v) = int(title("vertical_edge_margin_px")) {
        out.vertical_edge_margin_px = v.max(0);
    }
    if let Some(v) = int(title("aside_scale_pct")) {
        out.aside_scale_pct = v.clamp(25, 300);
    }
    if out.aside_max_width_px < out.aside_min_width_px {
        out.aside_max_width_px = out.aside_min_width_px;
    }
    out
}

/// The layout preset a title has selected (`canvas.layout_preset`). Not part of the layout
/// values: selecting a preset copies its values, editing a value keeps the selection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LayoutPresetSelection {
    /// No preset selected (stored as `"none"`).
    #[default]
    None,
    /// A built-in preset (stored as `{"builtin": "<id>"}`, [`BuiltinLayoutPreset::id`]).
    Builtin(BuiltinLayoutPreset),
    /// A user preset by its verbatim name (stored as `{"user": "<name>"}`).
    User(String),
}

impl LayoutPresetSelection {
    /// The selection stored in a title's `canvas` object. An ABSENT key (a title never saved by
    /// the Settings pane) reads as the kind's built-in preset (`pages` -> «Pages», `ribbon` ->
    /// «Webtoon», otherwise none); a malformed value reads as `None` and is logged.
    #[must_use]
    pub fn from_title(canvas: Option<&Value>, kind: Option<ComicType>) -> Self {
        let Some(raw) = canvas.and_then(|c| c.get(LAYOUT_PRESET_KEY)) else {
            return BuiltinLayoutPreset::for_kind(kind).map_or(Self::None, Self::Builtin);
        };
        Self::from_json(raw).unwrap_or_else(|| {
            runtime_log::log_warn(format!(
                "[canvas_layout] malformed canvas.{LAYOUT_PRESET_KEY} in the title settings; reading it as no preset; value={raw}"
            ));
            Self::None
        })
    }

    /// Decodes [`Self::to_json`]'s shapes; `None` for anything else (an unknown built-in id too).
    fn from_json(raw: &Value) -> Option<Self> {
        if raw.as_str() == Some("none") {
            return Some(Self::None);
        }
        let obj = raw.as_object().filter(|obj| obj.len() == 1)?;
        if let Some(id) = obj.get("builtin") {
            return id.as_str().and_then(BuiltinLayoutPreset::from_id).map(Self::Builtin);
        }
        obj.get("user").and_then(Value::as_str).map(|name| Self::User(name.to_owned()))
    }

    /// The persisted JSON form (see the variants).
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::None => Value::String("none".to_owned()),
            Self::Builtin(preset) => serde_json::json!({ "builtin": preset.id() }),
            Self::User(name) => serde_json::json!({ "user": name }),
        }
    }
}

/// Edits `root`'s `canvas` object in place through `edit`, creating it when absent.
///
/// # Errors
/// A technical message when `root` or an existing `canvas` value is not an object (nothing changed).
fn edit_canvas_object(root: &mut Value, settings_file: &Path, edit: impl FnOnce(&mut Map<String, Value>)) -> Result<(), String> {
    let Some(root_obj) = root.as_object_mut() else {
        return Err(format!("project settings root is not an object: '{}'", settings_file.display()));
    };
    let canvas = root_obj.entry("canvas").or_insert_with(|| Value::Object(Map::new()));
    let Some(canvas_obj) = canvas.as_object_mut() else {
        return Err(format!("project settings 'canvas' is not an object: '{}'", settings_file.display()));
    };
    edit(canvas_obj);
    Ok(())
}

/// Stores `selection` as `canvas.layout_preset` of the title's `settings.json` in ONE serialized
/// `ms_docstore::update`; every other key is kept, an absent file is created.
///
/// `durability`: `Durability::None` on the GUI thread, `Durability::Contents` on a worker.
///
/// # Errors
/// A technical message (logged by the caller) when the file cannot be read, is not valid JSON
/// (left untouched, never overwritten), its root or `canvas` is not an object (left untouched),
/// or it cannot be written.
pub fn save_layout_preset_selection(settings_file: &Path, selection: &LayoutPresetSelection, durability: Durability) -> Result<(), String> {
    let doc = DocRef::new(settings_file, DocKind::ProjectSettings);
    ms_docstore::update(&doc, WriteOptions { durability, ..WriteOptions::default() }, |root| {
        edit_canvas_object(root, settings_file, |canvas| {
            canvas.insert(LAYOUT_PRESET_KEY.to_owned(), selection.to_json());
        })
    })
    .map_err(|err| format!("failed to save the layout preset selection to project settings '{}': {err}", settings_file.display()))
}

/// Seeds an image-set scratch title: `comic_type = "pages"` and `canvas.layout_per_title = true`
/// in ONE `ms_docstore::update`, so the load yields kind `Pages`, opens no comic-type prompt and
/// takes the whole layout from the «Pages» preset (never from the user's legacy global values).
///
/// # Errors
/// As [`save_layout_preset_selection`].
pub fn seed_image_set_title(settings_file: &Path, durability: Durability) -> Result<(), String> {
    let doc = DocRef::new(settings_file, DocKind::ProjectSettings);
    ms_docstore::update(&doc, WriteOptions { durability, ..WriteOptions::default() }, |root| {
        edit_canvas_object(root, settings_file, |canvas| {
            canvas.insert(LAYOUT_PER_TITLE_KEY.to_owned(), Value::Bool(true));
        })?;
        // `edit_canvas_object` succeeded, so the root is an object: this branch always runs.
        if let Some(root_obj) = root.as_object_mut() {
            root_obj.insert("comic_type".to_owned(), Value::String(ComicType::Pages.as_config_str().to_owned()));
        }
        Ok(())
    })
    .map_err(|err| format!("failed to seed the image-set title settings '{}': {err}", settings_file.display()))
}

/// `user_config` section that holds the store (the user-level canvas preferences).
const USER_CANVAS_SECTION: &str = "Canvas";

/// `Canvas.layout_presets` in `user_config`: the app-wide layout preset store, shaped
/// `{"version": 1, "builtin": {"<id>": {layout}}, "user": {"<name>": {layout}}}` (layouts in the
/// `CanvasLayout::JSON_KEYS` vocabulary). Rust-only: the Python backend never reads it.
pub const LAYOUT_PRESETS_KEY: &str = "layout_presets";

/// The store format version this build reads and writes.
const LAYOUT_PRESETS_VERSION: u64 = 1;

/// Why a store operation was refused (nothing was changed, in memory or on disk).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutPresetStoreError {
    /// The store carries a format `version` this build does not know (a newer build wrote it):
    /// it is read as empty and never written, so the newer presets survive. `version` is the raw
    /// JSON value.
    UnsupportedVersion { version: String },
}

impl std::fmt::Display for LayoutPresetStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion { version } => write!(
                f,
                "user_config {USER_CANVAS_SECTION}.{LAYOUT_PRESETS_KEY} has format version {version}, this build writes only version {LAYOUT_PRESETS_VERSION}"
            ),
        }
    }
}

impl std::error::Error for LayoutPresetStoreError {}

/// One persisted operation on the preset store, applied the same way in memory
/// ([`CanvasLayoutPresets::apply`]) and on disk ([`apply_layout_preset_op`]). Layouts are stored
/// normalized. Name validation (empty, taken) is the caller's: every operation is total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutPresetOp {
    /// Inserts or replaces the user preset `name` (taken verbatim).
    UpsertUser { name: String, layout: CanvasLayout },
    /// Removes the user preset `from` and stores `layout` under `to`, in one step.
    RenameUser { from: String, to: String, layout: CanvasLayout },
    /// Removes the user preset `name`; an absent one is not an error.
    RemoveUser { name: String },
    /// Stores `layout` as the override of `preset`; the factory values clear the override
    /// instead (no copy of the factory row is ever stored).
    SaveBuiltin { preset: BuiltinLayoutPreset, layout: CanvasLayout },
    /// Clears the override of `preset`: its factory values come back.
    ResetBuiltin { preset: BuiltinLayoutPreset },
}

impl LayoutPresetOp {
    /// A one-line log description (kind and names, never the values).
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::UpsertUser { name, .. } => format!("upsert user preset {name:?}"),
            Self::RenameUser { from, to, .. } => format!("rename user preset {from:?} -> {to:?}"),
            Self::RemoveUser { name } => format!("remove user preset {name:?}"),
            Self::SaveBuiltin { preset, .. } => format!("save built-in preset {:?}", preset.id()),
            Self::ResetBuiltin { preset } => format!("reset built-in preset {:?}", preset.id()),
        }
    }
}

/// The app-wide layout presets: the overrides of the built-ins and the user's own presets,
/// decoded from `user_config` `Canvas.layout_presets` ([`LAYOUT_PRESETS_KEY`]). Every layout is
/// normalized; an override exists only while it differs from its factory row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CanvasLayoutPresets {
    /// Edited built-ins.
    overrides: BTreeMap<BuiltinLayoutPreset, CanvasLayout>,
    /// User presets by verbatim name (byte-sorted).
    user: BTreeMap<String, CanvasLayout>,
    /// `Some(raw version)` when the stored store has a format this build does not know: nothing
    /// was read and every operation is refused.
    unsupported_version: Option<String>,
}

/// The store's format version: `Ok` for an absent `version` or [`LAYOUT_PRESETS_VERSION`], else
/// the raw value.
fn check_store_version(store: &Map<String, Value>) -> Result<(), LayoutPresetStoreError> {
    match store.get("version") {
        None => Ok(()),
        Some(version) if version.as_u64() == Some(LAYOUT_PRESETS_VERSION) => Ok(()),
        Some(version) => Err(LayoutPresetStoreError::UnsupportedVersion { version: version.to_string() }),
    }
}

impl CanvasLayoutPresets {
    /// Decodes the store from a `user_config` root (no I/O). Lenient: an absent store is empty; a
    /// non-object store, a non-object entry, an unknown built-in id and an empty user name are
    /// skipped with a warning (and left on disk as they are); a missing or wrongly typed field takes
    /// the factory value (the built-in's own row for an override, [`BASE_BUILTIN`] for a user
    /// preset). An unknown `version` yields an empty store that refuses every operation.
    #[must_use]
    pub fn from_user_config(root: &Value) -> Self {
        let Some(raw) = root.get(USER_CANVAS_SECTION).and_then(|canvas| canvas.get(LAYOUT_PRESETS_KEY)) else {
            return Self::default();
        };
        let Some(store) = raw.as_object() else {
            runtime_log::log_warn(format!("[canvas_layout] user_config {USER_CANVAS_SECTION}.{LAYOUT_PRESETS_KEY} is not a JSON object; no layout presets read"));
            return Self::default();
        };
        if let Err(LayoutPresetStoreError::UnsupportedVersion { version }) = check_store_version(store) {
            runtime_log::log_warn(format!(
                "[canvas_layout] user_config {USER_CANVAS_SECTION}.{LAYOUT_PRESETS_KEY} has format version {version} (this build knows {LAYOUT_PRESETS_VERSION}); presets are not read and will not be written"
            ));
            return Self { unsupported_version: Some(version), ..Self::default() };
        }
        let mut out = Self::default();
        for (id, entry) in store.get("builtin").and_then(Value::as_object).into_iter().flatten() {
            let Some(preset) = BuiltinLayoutPreset::from_id(id) else {
                runtime_log::log_warn(format!("[canvas_layout] skipped an override of an unknown built-in layout preset {id:?}"));
                continue;
            };
            let Some(obj) = entry.as_object() else {
                runtime_log::log_warn(format!("[canvas_layout] skipped the override of built-in layout preset {id:?}: not a JSON object"));
                continue;
            };
            let factory = preset.factory();
            let layout = CanvasLayout::from_json_object(obj, &factory);
            if layout != factory {
                out.overrides.insert(preset, layout);
            }
        }
        for (name, entry) in store.get("user").and_then(Value::as_object).into_iter().flatten() {
            if name.is_empty() {
                runtime_log::log_warn("[canvas_layout] skipped a user layout preset with an empty name");
                continue;
            }
            let Some(obj) = entry.as_object() else {
                runtime_log::log_warn(format!("[canvas_layout] skipped user layout preset {name:?}: not a JSON object"));
                continue;
            };
            out.user.insert(name.clone(), CanvasLayout::from_json_object(obj, &BASE_BUILTIN.factory()));
        }
        out
    }

    /// What `preset` means now: its override when the user edited it, else its factory row. THE
    /// one source of every built-in apply (title fallback, image sets, first-open prompt, picker).
    #[must_use]
    pub fn effective_builtin(&self, preset: BuiltinLayoutPreset) -> CanvasLayout {
        self.overrides.get(&preset).cloned().unwrap_or_else(|| preset.factory())
    }

    /// The values that fill in what a title of kind `kind` does not store:
    /// [`Self::effective_builtin`] of [`BuiltinLayoutPreset::fallback_for_kind`].
    #[must_use]
    pub fn fallback_for_kind(&self, kind: Option<ComicType>) -> CanvasLayout {
        self.effective_builtin(BuiltinLayoutPreset::fallback_for_kind(kind))
    }

    /// Whether `preset` carries an override (its values differ from the factory row).
    #[must_use]
    pub fn is_builtin_modified(&self, preset: BuiltinLayoutPreset) -> bool {
        self.overrides.contains_key(&preset)
    }

    /// The values `selection` stands for; `None` for no selection and for a user preset that does
    /// not exist.
    #[must_use]
    pub fn effective(&self, selection: &LayoutPresetSelection) -> Option<CanvasLayout> {
        match selection {
            LayoutPresetSelection::None => None,
            LayoutPresetSelection::Builtin(preset) => Some(self.effective_builtin(*preset)),
            LayoutPresetSelection::User(name) => self.user.get(name).cloned(),
        }
    }

    /// The user preset names, byte-sorted, verbatim.
    pub fn user_names(&self) -> impl Iterator<Item = &str> {
        self.user.keys().map(String::as_str)
    }

    /// Whether a user preset is called `name` (exact match).
    #[must_use]
    pub fn contains_user(&self, name: &str) -> bool {
        self.user.contains_key(name)
    }

    /// Why this store refuses operations (`Some` only for an unknown format version).
    #[must_use]
    pub fn read_only_reason(&self) -> Option<LayoutPresetStoreError> {
        self.unsupported_version.clone().map(|version| LayoutPresetStoreError::UnsupportedVersion { version })
    }

    /// Applies `op` in memory, with the same rules as the disk writer.
    ///
    /// # Errors
    /// [`LayoutPresetStoreError::UnsupportedVersion`] for a read-only store; nothing changes then.
    pub fn apply(&mut self, op: &LayoutPresetOp) -> Result<(), LayoutPresetStoreError> {
        if let Some(err) = self.read_only_reason() {
            return Err(err);
        }
        match op {
            LayoutPresetOp::UpsertUser { name, layout } => {
                self.user.insert(name.clone(), layout.clone().normalized());
            }
            LayoutPresetOp::RenameUser { from, to, layout } => {
                self.user.remove(from);
                self.user.insert(to.clone(), layout.clone().normalized());
            }
            LayoutPresetOp::RemoveUser { name } => {
                self.user.remove(name);
            }
            LayoutPresetOp::SaveBuiltin { preset, layout } => {
                let layout = layout.clone().normalized();
                if layout == preset.factory() {
                    self.overrides.remove(preset);
                } else {
                    self.overrides.insert(*preset, layout);
                }
            }
            LayoutPresetOp::ResetBuiltin { preset } => {
                self.overrides.remove(preset);
            }
        }
        Ok(())
    }
}

/// The object inside `value`, or an empty one for anything else (a non-object value along the
/// store's path was already skipped as malformed by the reader).
fn take_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// Edits the object under `key` of `parent` through `edit` (an absent or non-object value
/// becomes an empty object first). `serde_json` maps are key-sorted here (`preserve_order` is
/// never enabled), so taking an entry out and putting it back keeps the document's bytes stable.
fn edit_object(parent: &mut Map<String, Value>, key: &str, edit: impl FnOnce(&mut Map<String, Value>)) {
    let mut child = take_object(parent.remove(key).unwrap_or(Value::Null));
    edit(&mut child);
    parent.insert(key.to_owned(), Value::Object(child));
}

/// The JSON form of one stored layout.
fn layout_json(layout: &CanvasLayout) -> Value {
    let mut obj = Map::new();
    CanvasLayout::write_json_object(&layout.clone().normalized(), &mut obj);
    Value::Object(obj)
}

/// Applies `op` to a `user_config` root, editing ONLY the touched entries of
/// `Canvas.layout_presets` (and setting its `version`): every other key, every other preset and
/// every entry the reader skipped as malformed stays as it is. A non-object value along the path
/// is replaced by an object (the reader already skipped it).
///
/// # Errors
/// [`LayoutPresetStoreError::UnsupportedVersion`] when the stored store has a format this build
/// does not know; `root` is then unchanged.
pub fn apply_layout_preset_op_to_user_config(root: &mut Value, op: &LayoutPresetOp) -> Result<(), LayoutPresetStoreError> {
    if let Some(store) = root.get(USER_CANVAS_SECTION).and_then(|canvas| canvas.get(LAYOUT_PRESETS_KEY)).and_then(Value::as_object) {
        check_store_version(store)?;
    }
    let mut root_obj = take_object(std::mem::take(root));
    edit_object(&mut root_obj, USER_CANVAS_SECTION, |canvas| {
        edit_object(canvas, LAYOUT_PRESETS_KEY, |store| {
            store.insert("version".to_owned(), Value::from(LAYOUT_PRESETS_VERSION));
            match op {
                LayoutPresetOp::UpsertUser { name, layout } => edit_object(store, "user", |user| {
                    user.insert(name.clone(), layout_json(layout));
                }),
                LayoutPresetOp::RenameUser { from, to, layout } => edit_object(store, "user", |user| {
                    user.remove(from);
                    user.insert(to.clone(), layout_json(layout));
                }),
                LayoutPresetOp::RemoveUser { name } => edit_object(store, "user", |user| {
                    user.remove(name);
                }),
                LayoutPresetOp::SaveBuiltin { preset, layout } => edit_object(store, "builtin", |builtin| {
                    if layout.clone().normalized() == preset.factory() {
                        builtin.remove(preset.id());
                    } else {
                        builtin.insert(preset.id().to_owned(), layout_json(layout));
                    }
                }),
                LayoutPresetOp::ResetBuiltin { preset } => edit_object(store, "builtin", |builtin| {
                    builtin.remove(preset.id());
                }),
            }
        });
    });
    *root = Value::Object(root_obj);
    Ok(())
}

/// Persists `op` into the `user_config` document at `user_config_path` in ONE serialized
/// read-modify-write ([`ms_config::update_user_config_file`] +
/// [`apply_layout_preset_op_to_user_config`]), so operations of several app instances merge entry
/// by entry. Blocking I/O: worker threads only.
///
/// # Errors
/// A technical message when the document cannot be read, is malformed (left untouched), cannot be
/// written, or carries an unknown store version (left untouched).
pub fn apply_layout_preset_op(user_config_path: &Path, op: &LayoutPresetOp) -> Result<(), String> {
    ms_config::update_user_config_file(user_config_path, |root| apply_layout_preset_op_to_user_config(root, op).map_err(anyhow::Error::from))
        .map_err(|err| format!("{err:#}"))
}

#[cfg(test)]
mod tests {
    use super::{
        BASE_BUILTIN, BuiltinLayoutPreset, CanvasLayout, CanvasLayoutPresets, LAYOUT_PER_TITLE_KEY, LAYOUT_PRESET_KEY, LayoutPresetOp, LayoutPresetSelection, LayoutPresetStoreError,
        apply_layout_preset_op, apply_layout_preset_op_to_user_config, save_layout_preset_selection, seed_image_set_title,
    };
    use crate::{CanvasSettings, ComicType};
    use ms_docstore::Durability;
    use serde_json::{Map, Value, json};
    use std::collections::HashSet;

    /// A layout with every field off both factory rows.
    fn odd_layout() -> CanvasLayout {
        CanvasLayout {
            separate_pages: false,
            page_spacing_px: 7,
            vertical_edge_margin_px: 9,
            aside_compact_mode: "strong".to_string(),
            aside_min_width_px: 111,
            aside_max_width_px: 999,
            aside_scale_pct: 260,
            aside_side_mode: "right".to_string(),
            scale_bubbles: false,
            aside_second_column: false,
            editable_bubble_type: "on_top".to_string(),
            readonly_bubble_type: "default".to_string(),
            on_top_focus_mode: "aside".to_string(),
        }
    }

    /// The two rows differ exactly in compaction and page separation, and carry the pairs the
    /// former comic-type presets wrote (the legacy migration rule relies on them).
    #[test]
    fn factory_values_differ_only_in_compact_and_separate() {
        let pages = BuiltinLayoutPreset::Pages.factory();
        let webtoon = BuiltinLayoutPreset::Webtoon.factory();
        assert_eq!((pages.aside_compact_mode.as_str(), pages.separate_pages), ("moderate", true));
        assert_eq!((webtoon.aside_compact_mode.as_str(), webtoon.separate_pages), ("none", false));
        let mut aligned = webtoon.clone();
        aligned.aside_compact_mode.clone_from(&pages.aside_compact_mode);
        aligned.separate_pages = pages.separate_pages;
        assert_eq!(aligned, pages);
    }

    /// The factory rows are already canonical.
    #[test]
    fn factory_values_are_normalized() {
        for preset in BuiltinLayoutPreset::ALL {
            assert_eq!(preset.factory().normalized(), preset.factory(), "{preset:?}");
        }
    }

    /// `of` and `apply_to` are inverse and `apply_to` touches only the layout fields.
    #[test]
    fn layout_of_apply_to_round_trip() {
        let mut settings = CanvasSettings::default();
        let before = settings.clone();
        odd_layout().apply_to(&mut settings);
        assert_eq!(CanvasLayout::of(&settings), odd_layout());
        BASE_BUILTIN.factory().apply_to(&mut settings);
        assert_eq!(CanvasLayout::of(&settings), CanvasLayout::of(&before));
        assert_eq!((settings.bubble_type.as_str(), settings.side_margin_px, settings.translation_status_display.as_str()), (before.bubble_type.as_str(), before.side_margin_px, before.translation_status_display.as_str()));
    }

    /// `CanvasSettings::default` is the base preset in every layout field.
    #[test]
    fn canvas_settings_default_is_the_base_preset() {
        assert_eq!(CanvasLayout::of(&CanvasSettings::default()), BASE_BUILTIN.factory());
    }

    /// The JSON keys are 13 distinct names.
    #[test]
    fn json_keys_are_distinct() {
        let keys: HashSet<&str> = CanvasLayout::JSON_KEYS.into_iter().collect();
        assert_eq!(keys.len(), CanvasLayout::JSON_KEYS.len());
    }

    /// Normalization folds tokens and applies the reader's clamps.
    #[test]
    fn normalized_folds_tokens_and_clamps() {
        let raw = CanvasLayout {
            page_spacing_px: -3,
            vertical_edge_margin_px: -4,
            aside_compact_mode: " MODERATE ".to_string(),
            aside_min_width_px: 10,
            aside_max_width_px: 5,
            aside_scale_pct: 1000,
            aside_side_mode: "Left".to_string(),
            editable_bubble_type: "ON_TOP".to_string(),
            readonly_bubble_type: " Aside".to_string(),
            on_top_focus_mode: "AROUND ".to_string(),
            ..odd_layout()
        };
        let out = raw.normalized();
        assert_eq!((out.page_spacing_px, out.vertical_edge_margin_px, out.aside_min_width_px, out.aside_max_width_px, out.aside_scale_pct), (0, 0, 40, 40, 300));
        assert_eq!((out.aside_compact_mode.as_str(), out.aside_side_mode.as_str(), out.editable_bubble_type.as_str(), out.readonly_bubble_type.as_str(), out.on_top_focus_mode.as_str()), ("moderate", "left", "on_top", "aside", "around"));
        let wide = CanvasLayout { aside_min_width_px: 600, aside_max_width_px: 500, aside_scale_pct: 1, ..odd_layout() }.normalized();
        assert_eq!((wide.aside_min_width_px, wide.aside_max_width_px, wide.aside_scale_pct), (600, 600, 25));
    }

    /// Persistence ids round-trip and are exact.
    #[test]
    fn preset_ids_round_trip() {
        for preset in BuiltinLayoutPreset::ALL {
            assert_eq!(BuiltinLayoutPreset::from_id(preset.id()), Some(preset));
        }
        assert_eq!(BuiltinLayoutPreset::from_id("Pages"), None);
        assert_eq!(BuiltinLayoutPreset::from_id("ribbon"), None);
    }

    /// The kind -> preset map.
    #[test]
    fn preset_for_kind() {
        assert_eq!(BuiltinLayoutPreset::for_kind(Some(ComicType::Pages)), Some(BuiltinLayoutPreset::Pages));
        assert_eq!(BuiltinLayoutPreset::for_kind(Some(ComicType::Ribbon)), Some(BuiltinLayoutPreset::Webtoon));
        assert_eq!(BuiltinLayoutPreset::for_kind(Some(ComicType::Custom)), None);
        assert_eq!(BuiltinLayoutPreset::for_kind(None), None);
    }

    /// The JSON codec writes exactly `JSON_KEYS` and reads back what it wrote, whatever the fallback.
    #[test]
    fn json_object_round_trip() {
        let mut obj = Map::new();
        obj.insert("foreign".to_owned(), json!(1));
        let layout = odd_layout().normalized();
        CanvasLayout::write_json_object(&layout, &mut obj);
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).filter(|key| *key != "foreign").collect();
        keys.sort_unstable();
        let mut expected = CanvasLayout::JSON_KEYS.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected);
        assert_eq!(obj["foreign"], 1);
        for preset in BuiltinLayoutPreset::ALL {
            assert_eq!(CanvasLayout::from_json_object(&obj, &preset.factory()), layout, "{preset:?}");
        }
    }

    /// Missing and wrongly typed keys take the fallback per field; integers saturate.
    #[test]
    fn json_object_falls_back_per_key() {
        let fallback = BuiltinLayoutPreset::Webtoon.factory();
        let obj = json!({"separate_pages": "yes", "page_spacing_px": 9_000_000_000_i64, "aside_side_mode": 3}).as_object().cloned().unwrap_or_default();
        let out = CanvasLayout::from_json_object(&obj, &fallback);
        assert_eq!(out.page_spacing_px, i32::MAX);
        assert_eq!(CanvasLayout { page_spacing_px: fallback.page_spacing_px, ..out }, fallback);
    }

    /// The selection's JSON forms round-trip; malformed values decode to nothing.
    #[test]
    fn selection_json_round_trip() {
        for selection in [LayoutPresetSelection::None, LayoutPresetSelection::Builtin(BuiltinLayoutPreset::Pages), LayoutPresetSelection::Builtin(BuiltinLayoutPreset::Webtoon), LayoutPresetSelection::User("Мой «пресет»".to_owned())] {
            let canvas = json!({ LAYOUT_PRESET_KEY: selection.to_json() });
            assert_eq!(LayoutPresetSelection::from_title(Some(&canvas), Some(ComicType::Ribbon)), selection);
        }
        for malformed in [json!("pages"), json!(null), json!({}), json!({"builtin": "pages", "user": "x"}), json!({"user": 3}), json!(["none"])] {
            let canvas = json!({ LAYOUT_PRESET_KEY: malformed });
            assert_eq!(LayoutPresetSelection::from_title(Some(&canvas), Some(ComicType::Pages)), LayoutPresetSelection::None, "{malformed}");
        }
        assert_eq!(LayoutPresetSelection::from_title(None, Some(ComicType::Ribbon)), LayoutPresetSelection::Builtin(BuiltinLayoutPreset::Webtoon));
    }

    /// No layout key, marker or selection is in either `ms-config` defaults tree: a backfill would
    /// otherwise write stale layout values into every opened title (and mark it migrated).
    #[test]
    fn defaults_trees_carry_no_layout_key() {
        let user = ms_config::user_config_defaults();
        let project = ms_config::project_config_defaults();
        let objects = [("user Canvas", user.get("Canvas")), ("project root", Some(&project)), ("project canvas", project.get("canvas"))];
        for (label, object) in objects {
            let Some(object) = object.and_then(Value::as_object) else {
                panic!("{label} is missing from the defaults");
            };
            for key in CanvasLayout::JSON_KEYS.iter().chain([LAYOUT_PER_TITLE_KEY, LAYOUT_PRESET_KEY].iter()) {
                assert!(!object.contains_key(*key), "{label} carries {key}");
            }
        }
    }

    /// A unique scratch `settings.json` path under the system temp dir (the directory is removed
    /// by the returned guard).
    fn scratch_settings() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap_or_else(|err| panic!("temp dir: {err}"));
        let path = dir.path().join("settings.json");
        (dir, path)
    }

    fn read_json(path: &std::path::Path) -> Value {
        let raw = std::fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        serde_json::from_str(&raw).unwrap_or_else(|err| panic!("parse {}: {err}", path.display()))
    }

    /// The selection writer edits only `canvas.layout_preset` and creates an absent file.
    #[test]
    fn selection_writer_keeps_foreign_keys() -> Result<(), String> {
        let (_dir, path) = scratch_settings();
        save_layout_preset_selection(&path, &LayoutPresetSelection::None, Durability::Contents)?;
        assert_eq!(read_json(&path), json!({"canvas": {"layout_preset": "none"}}));
        std::fs::write(&path, r#"{"comic_type":"ribbon","OCR":{"engine":"paddle"},"canvas":{"side_margin_px":7}}"#).map_err(|err| err.to_string())?;
        save_layout_preset_selection(&path, &LayoutPresetSelection::Builtin(BuiltinLayoutPreset::Webtoon), Durability::Contents)?;
        assert_eq!(read_json(&path), json!({"comic_type": "ribbon", "OCR": {"engine": "paddle"}, "canvas": {"side_margin_px": 7, "layout_preset": {"builtin": "webtoon"}}}));
        Ok(())
    }

    /// Neither writer touches a malformed file, a non-object root or a non-object `canvas`.
    #[test]
    fn title_writers_never_overwrite_a_malformed_file() -> Result<(), String> {
        let (_dir, path) = scratch_settings();
        for seed in ["{ not json", "[1, 2]", r#"{"canvas": 5}"#] {
            std::fs::write(&path, seed).map_err(|err| err.to_string())?;
            assert!(save_layout_preset_selection(&path, &LayoutPresetSelection::None, Durability::Contents).is_err(), "{seed}");
            assert!(seed_image_set_title(&path, Durability::Contents).is_err(), "{seed}");
            assert_eq!(std::fs::read_to_string(&path).map_err(|err| err.to_string())?, seed);
        }
        Ok(())
    }

    /// The image-set seed writes the kind and the marker together.
    #[test]
    fn image_set_seed_writes_kind_and_marker() -> Result<(), String> {
        let (_dir, path) = scratch_settings();
        seed_image_set_title(&path, Durability::Contents)?;
        assert_eq!(read_json(&path), json!({"comic_type": "pages", "canvas": {"layout_per_title": true}}));
        Ok(())
    }

    /// Every built-in has a non-empty display name.
    #[test]
    fn builtin_display_names_are_present() {
        for preset in BuiltinLayoutPreset::ALL {
            assert!(!preset.display_name().is_empty(), "{preset:?}");
        }
    }

    /// The fallback preset of each kind.
    #[test]
    fn fallback_for_kind() {
        assert_eq!(BuiltinLayoutPreset::fallback_for_kind(Some(ComicType::Ribbon)), BuiltinLayoutPreset::Webtoon);
        assert_eq!(BuiltinLayoutPreset::fallback_for_kind(Some(ComicType::Pages)), BuiltinLayoutPreset::Pages);
        assert_eq!(BuiltinLayoutPreset::fallback_for_kind(Some(ComicType::Custom)), BASE_BUILTIN);
        assert_eq!(BuiltinLayoutPreset::fallback_for_kind(None), BASE_BUILTIN);
    }

    /// A `user_config` root holding `store` as `Canvas.layout_presets`.
    fn user_config_with(store: Value) -> Value {
        json!({"General": {"ui_language": "ru"}, "Canvas": {"cache_pages": true, "layout_presets": store}})
    }

    /// The decoder skips what it cannot read, fills missing fields from the right factory row and
    /// drops an override equal to its factory row.
    #[test]
    fn store_decode_is_lenient() {
        let root = user_config_with(json!({
            "builtin": {"pages": {"page_spacing_px": 5}, "webtoon": {}, "manga": {"page_spacing_px": 1}},
            "user": {"mine": {"separate_pages": false}, "": {}, "broken": 7},
        }));
        let presets = CanvasLayoutPresets::from_user_config(&root);
        assert_eq!(presets.read_only_reason(), None);
        let pages = CanvasLayout { page_spacing_px: 5, ..BuiltinLayoutPreset::Pages.factory() };
        assert_eq!(presets.effective_builtin(BuiltinLayoutPreset::Pages), pages);
        assert!(presets.is_builtin_modified(BuiltinLayoutPreset::Pages));
        assert!(!presets.is_builtin_modified(BuiltinLayoutPreset::Webtoon), "an override equal to the factory row is no override");
        assert_eq!(presets.user_names().collect::<Vec<_>>(), vec!["mine"]);
        let mine = CanvasLayout { separate_pages: false, ..BASE_BUILTIN.factory() };
        assert_eq!(presets.effective(&LayoutPresetSelection::User("mine".to_owned())), Some(mine));
        assert_eq!(presets.effective(&LayoutPresetSelection::User("gone".to_owned())), None);
        assert_eq!(presets.effective(&LayoutPresetSelection::None), None);
        assert_eq!(presets.fallback_for_kind(Some(ComicType::Pages)), pages);
        assert_eq!(presets.fallback_for_kind(Some(ComicType::Ribbon)), BuiltinLayoutPreset::Webtoon.factory());
        assert_eq!(CanvasLayoutPresets::from_user_config(&json!({})), CanvasLayoutPresets::default());
        assert_eq!(CanvasLayoutPresets::from_user_config(&user_config_with(json!(3))), CanvasLayoutPresets::default());
    }

    /// The in-memory operations: user presets verbatim, built-in save and reset rules.
    #[test]
    fn store_operations_in_memory() -> Result<(), LayoutPresetStoreError> {
        let mut presets = CanvasLayoutPresets::default();
        let odd = odd_layout().normalized();
        presets.apply(&LayoutPresetOp::UpsertUser { name: " a ".to_owned(), layout: odd_layout() })?;
        assert_eq!(presets.effective(&LayoutPresetSelection::User(" a ".to_owned())), Some(odd.clone()), "names are verbatim, layouts normalized");
        presets.apply(&LayoutPresetOp::RenameUser { from: " a ".to_owned(), to: "b".to_owned(), layout: BASE_BUILTIN.factory() })?;
        assert_eq!(presets.user_names().collect::<Vec<_>>(), vec!["b"]);
        presets.apply(&LayoutPresetOp::RemoveUser { name: "b".to_owned() })?;
        presets.apply(&LayoutPresetOp::RemoveUser { name: "absent".to_owned() })?;
        assert_eq!(presets.user_names().count(), 0);

        let preset = BuiltinLayoutPreset::Webtoon;
        presets.apply(&LayoutPresetOp::SaveBuiltin { preset, layout: odd.clone() })?;
        assert_eq!(presets.effective_builtin(preset), odd);
        presets.apply(&LayoutPresetOp::SaveBuiltin { preset, layout: preset.factory() })?;
        assert!(!presets.is_builtin_modified(preset), "saving the factory values clears the override");
        presets.apply(&LayoutPresetOp::SaveBuiltin { preset, layout: odd })?;
        presets.apply(&LayoutPresetOp::ResetBuiltin { preset })?;
        assert_eq!(presets.effective_builtin(preset), preset.factory());
        Ok(())
    }

    /// A store of an unknown format version is read as empty and refuses every write, in memory
    /// and on disk.
    #[test]
    fn unknown_store_version_is_read_only() {
        let root = user_config_with(json!({"version": 2, "user": {"newer": {}}}));
        let mut presets = CanvasLayoutPresets::from_user_config(&root);
        let refused = Err(LayoutPresetStoreError::UnsupportedVersion { version: "2".to_owned() });
        assert_eq!(presets.read_only_reason(), refused.clone().err());
        assert_eq!(presets.user_names().count(), 0);
        let op = LayoutPresetOp::RemoveUser { name: "newer".to_owned() };
        assert_eq!(presets.apply(&op), refused);
        let mut on_disk = root.clone();
        assert_eq!(apply_layout_preset_op_to_user_config(&mut on_disk, &op), refused);
        assert_eq!(on_disk, root);
    }

    /// The JSON writer edits only the touched entries: foreign keys, other presets and malformed
    /// entries survive; what it writes reads back as the in-memory result.
    #[test]
    fn store_json_writer_edits_only_touched_entries() -> Result<(), LayoutPresetStoreError> {
        let mut root = user_config_with(json!({"user": {"keep": {"page_spacing_px": 3}, "broken": 7, "old": {}}, "future_key": true}));
        let mut memory = CanvasLayoutPresets::from_user_config(&root);
        let ops = [
            LayoutPresetOp::RenameUser { from: "old".to_owned(), to: "new".to_owned(), layout: odd_layout() },
            LayoutPresetOp::SaveBuiltin { preset: BuiltinLayoutPreset::Pages, layout: odd_layout() },
            LayoutPresetOp::UpsertUser { name: "added".to_owned(), layout: BASE_BUILTIN.factory() },
            LayoutPresetOp::SaveBuiltin { preset: BuiltinLayoutPreset::Webtoon, layout: BuiltinLayoutPreset::Webtoon.factory() },
        ];
        for op in &ops {
            apply_layout_preset_op_to_user_config(&mut root, op)?;
            memory.apply(op)?;
        }
        assert_eq!(root["General"]["ui_language"], "ru");
        assert_eq!(root["Canvas"]["cache_pages"], true);
        let store = &root["Canvas"]["layout_presets"];
        assert_eq!((store["version"].clone(), store["future_key"].clone(), store["user"]["broken"].clone()), (json!(1), json!(true), json!(7)));
        assert!(store["user"].get("old").is_none() && store["builtin"].get("webtoon").is_none());
        assert_eq!(CanvasLayoutPresets::from_user_config(&root), memory);

        let mut garbage = json!({"Canvas": 5});
        apply_layout_preset_op_to_user_config(&mut garbage, &ops[2])?;
        assert_eq!(garbage["Canvas"]["layout_presets"]["user"]["added"], layout_json_of(&BASE_BUILTIN.factory()));
        Ok(())
    }

    fn layout_json_of(layout: &CanvasLayout) -> Value {
        let mut obj = Map::new();
        CanvasLayout::write_json_object(layout, &mut obj);
        Value::Object(obj)
    }

    /// The disk writer keeps a foreign `user_config` and never overwrites a malformed one.
    #[test]
    fn store_disk_writer_round_trips_and_never_overwrites_a_malformed_file() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let path = dir.path().join("user_config.json");
        std::fs::write(&path, r#"{"General":{"ui_language":"ru"}}"#).map_err(|err| err.to_string())?;
        apply_layout_preset_op(&path, &LayoutPresetOp::UpsertUser { name: "Мой".to_owned(), layout: odd_layout() })?;
        let root = read_json(&path);
        assert_eq!(root["General"]["ui_language"], "ru");
        assert_eq!(CanvasLayoutPresets::from_user_config(&root).effective(&LayoutPresetSelection::User("Мой".to_owned())), Some(odd_layout().normalized()));
        let malformed = r#"{"Canvas": {"#;
        std::fs::write(&path, malformed).map_err(|err| err.to_string())?;
        assert!(apply_layout_preset_op(&path, &LayoutPresetOp::RemoveUser { name: "Мой".to_owned() }).is_err());
        assert_eq!(std::fs::read_to_string(&path).map_err(|err| err.to_string())?, malformed);
        Ok(())
    }
}
