/*
File: crates/ms-project/src/canvas_settings_tests.rs

Purpose:
Tests of the title canvas-settings reader (`canvas_settings_from_config` in `lib.rs`) for the
13 preset-owned layout fields. The `legacy_*` tests are characterization tests written against
the reader as it was before the layout table (`canvas_layout.rs`) existed: they pin what a title
WITHOUT the per-title marker and without a stored kind reads (which document wins per field, the
legacy root fallbacks, the clamps). The `marker_*` / `kind_*` tests pin the per-title read and the
kind-dependent parts of the migration rule (`canvas_layout::read_title_layout`).

Key tests:
- legacy_canvas_read_precedence (the 13-field scope matrix)
- legacy_canvas_read_clamps
- legacy_canvas_read_falls_back_to_defaults
- layout_json_keys_are_the_read_keys
- marker_title_reads_only_the_title
- kind_decides_the_legacy_compact_mode / kind_picks_the_fallback_preset
- fallback_follows_the_users_builtin_override

Notes:
Pure: the reader takes JSON values, no file is touched.
*/

use super::{BuiltinLayoutPreset, CanvasLayout, CanvasSettings, LayoutPresetSelection, canvas_settings_from_config};
use serde_json::{Value, json};

/// One preset-owned field as the reader sees it: its JSON key, a getter rendering the read value
/// as JSON (for uniform comparison), a "user" probe value and a "project" probe value, both
/// valid and different from each other and from the default.
struct Probe {
    key: &'static str,
    get: fn(&CanvasSettings) -> Value,
    user: Value,
    project: Value,
}

/// The 13 preset-owned fields with distinct probe values.
fn probes() -> Vec<Probe> {
    vec![
        Probe { key: "separate_pages", get: |s| json!(s.separate_pages), user: json!(true), project: json!(false) },
        Probe { key: "page_spacing_px", get: |s| json!(s.page_spacing_px), user: json!(11), project: json!(22) },
        Probe { key: "vertical_edge_margin_px", get: |s| json!(s.vertical_edge_margin_px), user: json!(33), project: json!(44) },
        Probe { key: "aside_compact_mode", get: |s| json!(s.aside_compact_mode), user: json!("strong"), project: json!("moderate") },
        Probe { key: "aside_min_width_px", get: |s| json!(s.aside_min_width_px), user: json!(301), project: json!(302) },
        Probe { key: "aside_max_width_px", get: |s| json!(s.aside_max_width_px), user: json!(901), project: json!(902) },
        Probe { key: "aside_scale_pct", get: |s| json!(s.aside_scale_pct), user: json!(150), project: json!(175) },
        Probe { key: "aside_side_mode", get: |s| json!(s.aside_side_mode), user: json!("left"), project: json!("right") },
        Probe { key: "scale_bubbles", get: |s| json!(s.scale_bubbles), user: json!(false), project: json!(false) },
        Probe { key: "aside_second_column", get: |s| json!(s.aside_second_column), user: json!(false), project: json!(false) },
        Probe { key: "editable_bubble_type", get: |s| json!(s.editable_bubble_type), user: json!("default"), project: json!("on_top") },
        Probe { key: "readonly_bubble_type", get: |s| json!(s.readonly_bubble_type), user: json!("default"), project: json!("on_top") },
        Probe { key: "on_top_focus_mode", get: |s| json!(s.on_top_focus_mode), user: json!("around2"), project: json!("aside") },
    ]
}

/// Fields read user-file first with the title `canvas` object as fallback.
const USER_FIRST: [&str; 6] = ["aside_min_width_px", "aside_max_width_px", "aside_compact_mode", "aside_side_mode", "aside_second_column", "scale_bubbles"];
/// Fields that additionally fall back to a legacy key at the title ROOT.
const ROOT_FALLBACK: [&str; 3] = ["editable_bubble_type", "readonly_bubble_type", "on_top_focus_mode"];

fn read(settings: &Value, user: &Value) -> CanvasSettings {
    canvas_settings_from_config(settings, user)
}

/// The scope matrix: for every field, which of user `Canvas.*`, title `canvas.*` and title root
/// is read, and in which order.
#[test]
fn legacy_canvas_read_precedence() {
    assert_eq!(probes().len(), 13);
    for probe in probes() {
        let key = probe.key;
        // Booleans with equal user/project probes cannot tell the two sources apart; the
        // single-source cases below still pin each source.
        let user_only = read(&json!({}), &json!({"Canvas": {key: probe.user}}));
        let project_only = read(&json!({"canvas": {key: probe.project}}), &json!({}));
        let both = read(&json!({"canvas": {key: probe.project}}), &json!({"Canvas": {key: probe.user}}));
        let default = (probe.get)(&CanvasSettings::default());
        assert_eq!((probe.get)(&project_only), probe.project, "{key}: the title canvas value is read");
        if USER_FIRST.contains(&key) {
            assert_eq!((probe.get)(&user_only), probe.user, "{key}: the user value is read");
            assert_eq!((probe.get)(&both), probe.user, "{key}: the user value wins over the title");
        } else {
            assert_eq!((probe.get)(&user_only), default, "{key}: the user value is ignored");
            assert_eq!((probe.get)(&both), probe.project, "{key}: only the title is read");
        }
        let root_only = read(&json!({key: probe.project}), &json!({}));
        let root_and_canvas = read(&json!({key: probe.user, "canvas": {key: probe.project}}), &json!({}));
        if ROOT_FALLBACK.contains(&key) {
            assert_eq!((probe.get)(&root_only), probe.project, "{key}: the legacy root key is the fallback");
            assert_eq!((probe.get)(&root_and_canvas), probe.project, "{key}: the canvas object wins over the root");
        } else {
            assert_eq!((probe.get)(&root_only), default, "{key}: no root fallback");
        }
    }
}

/// A legacy `aside` / `on_top` `bubble_type` pins both display types and reads back as `hybrid`.
#[test]
fn legacy_canvas_read_pins_display_types_from_a_legacy_mode() {
    let out = read(&json!({"bubble_type": "on_top", "canvas": {"editable_bubble_type": "aside", "readonly_bubble_type": "aside"}}), &json!({}));
    assert_eq!((out.bubble_type.as_str(), out.editable_bubble_type.as_str(), out.readonly_bubble_type.as_str()), ("hybrid", "on_top", "on_top"));
}

/// The reader's clamps: widths at least 40 and max >= min, spacing / margin at least 0, scale
/// 25..=300. Mode strings are stored verbatim (no trimming, no case folding).
#[test]
fn legacy_canvas_read_clamps() {
    let out = read(&json!({"canvas": {"page_spacing_px": -5, "vertical_edge_margin_px": -7, "aside_scale_pct": 10, "aside_compact_mode": " MODERATE "}}), &json!({"Canvas": {"aside_min_width_px": 10, "aside_max_width_px": 5}}));
    assert_eq!((out.aside_min_width_px, out.aside_max_width_px), (40, 40));
    assert_eq!((out.page_spacing_px, out.vertical_edge_margin_px, out.aside_scale_pct), (0, 0, 25));
    assert_eq!(out.aside_compact_mode, " MODERATE ");
    let out = read(&json!({"canvas": {"aside_scale_pct": 999}}), &json!({"Canvas": {"aside_min_width_px": 600, "aside_max_width_px": 500}}));
    assert_eq!((out.aside_min_width_px, out.aside_max_width_px, out.aside_scale_pct), (600, 600, 300));
}

/// A key of the wrong JSON type is ignored, and it does NOT fall through to the next source:
/// the source lookup stops at the first PRESENT key, so the default is kept.
#[test]
fn legacy_canvas_read_ignores_wrongly_typed_values_without_falling_through() {
    let out = read(&json!({"canvas": {"aside_min_width_px": 333, "separate_pages": "yes"}}), &json!({"Canvas": {"aside_min_width_px": "wide"}}));
    assert_eq!(out.aside_min_width_px, CanvasSettings::default().aside_min_width_px);
    assert_eq!(out.separate_pages, CanvasSettings::default().separate_pages);
}

/// Empty documents read as `CanvasSettings::default` in every layout field.
#[test]
fn legacy_canvas_read_falls_back_to_defaults() {
    let out = read(&json!({}), &json!({}));
    let default = CanvasSettings::default();
    for probe in probes() {
        assert_eq!((probe.get)(&out), (probe.get)(&default), "{}", probe.key);
    }
}

/// `CanvasLayout::JSON_KEYS` names exactly the 13 keys the reader reads, in declaration order.
#[test]
fn layout_json_keys_are_the_read_keys() {
    let read_keys: Vec<&str> = probes().iter().map(|probe| probe.key).collect();
    assert_eq!(read_keys, CanvasLayout::JSON_KEYS);
}

/// A title with `canvas.layout_per_title` reads its layout from its own `canvas` object only:
/// user-config values and legacy root keys are ignored, tokens are normalized.
#[test]
fn marker_title_reads_only_the_title() {
    let user = json!({"Canvas": {"aside_min_width_px": 777, "aside_compact_mode": "strong", "scale_bubbles": false}});
    let out = read(&json!({"editable_bubble_type": "on_top", "canvas": {"layout_per_title": true, "aside_min_width_px": 333, "aside_side_mode": " LEFT"}}), &user);
    assert_eq!(out.aside_min_width_px, 333);
    assert_eq!(out.aside_side_mode, "left");
    let pages = BuiltinLayoutPreset::Pages.factory();
    assert_eq!(out.aside_compact_mode, pages.aside_compact_mode, "a missing key takes the fallback preset, not the user value");
    assert_eq!((out.scale_bubbles, out.editable_bubble_type.as_str()), (pages.scale_bubbles, pages.editable_bubble_type.as_str()));
}

/// Only a literal `true` is the marker.
#[test]
fn marker_must_be_true() {
    let user = json!({"Canvas": {"aside_min_width_px": 777}});
    for marker in [json!(false), json!("true"), json!(1)] {
        let out = read(&json!({"canvas": {"layout_per_title": marker, "aside_min_width_px": 333}}), &user);
        assert_eq!(out.aside_min_width_px, 777, "marker {marker}");
    }
}

/// The legacy compact mode of a `pages` / `ribbon` title is the one its kind was derived from;
/// a `custom` or kind-less title keeps the old user-first read.
#[test]
fn kind_decides_the_legacy_compact_mode() {
    let user = |mode: &str| json!({"Canvas": {"aside_compact_mode": mode}});
    assert_eq!(read(&json!({"comic_type": "pages"}), &user("none")).aside_compact_mode, "moderate");
    assert_eq!(read(&json!({"comic_type": "ribbon"}), &user("moderate")).aside_compact_mode, "none");
    assert_eq!(read(&json!({"comic_type": "ribbon", "canvas": {"aside_compact_mode": "strong"}}), &json!({})).aside_compact_mode, "none");
    assert_eq!(read(&json!({"comic_type": "custom"}), &user("strong")).aside_compact_mode, "strong");
    assert_eq!(read(&json!({}), &user("strong")).aside_compact_mode, "strong");
}

/// A legacy title keeps the user's global aside widths (the only record they had).
#[test]
fn kind_keeps_the_legacy_global_widths() {
    let out = read(&json!({"comic_type": "pages", "canvas": {"aside_min_width_px": 450}}), &json!({"Canvas": {"aside_min_width_px": 600, "aside_max_width_px": 700}}));
    assert_eq!((out.aside_min_width_px, out.aside_max_width_px), (600, 700));
}

/// What a title does not store comes from its kind's preset: a ribbon title without
/// `separate_pages` is continuous, in both the legacy and the per-title read.
#[test]
fn kind_picks_the_fallback_preset() {
    assert!(!read(&json!({"comic_type": "ribbon"}), &json!({})).separate_pages);
    assert!(!read(&json!({"comic_type": "ribbon", "canvas": {"layout_per_title": true}}), &json!({})).separate_pages);
    assert!(read(&json!({"comic_type": "custom", "canvas": {"layout_per_title": true}}), &json!({})).separate_pages);
    assert_eq!(CanvasLayout::of(&read(&json!({"comic_type": "ribbon", "canvas": {"layout_per_title": true}}), &json!({}))), BuiltinLayoutPreset::Webtoon.factory());
}

/// The selection: absent reads by kind, a stored value wins, a malformed one is no preset.
#[test]
fn reader_sets_the_preset_selection() {
    assert_eq!(read(&json!({"comic_type": "pages"}), &json!({})).layout_preset, LayoutPresetSelection::Builtin(BuiltinLayoutPreset::Pages));
    assert_eq!(read(&json!({"comic_type": "ribbon"}), &json!({})).layout_preset, LayoutPresetSelection::Builtin(BuiltinLayoutPreset::Webtoon));
    assert_eq!(read(&json!({"comic_type": "custom"}), &json!({})).layout_preset, LayoutPresetSelection::None);
    assert_eq!(read(&json!({}), &json!({})).layout_preset, LayoutPresetSelection::None);
    assert_eq!(read(&json!({"comic_type": "pages", "canvas": {"layout_preset": "none"}}), &json!({})).layout_preset, LayoutPresetSelection::None);
    assert_eq!(read(&json!({"comic_type": "pages", "canvas": {"layout_preset": {"builtin": "webtoon"}}}), &json!({})).layout_preset, LayoutPresetSelection::Builtin(BuiltinLayoutPreset::Webtoon));
    assert_eq!(read(&json!({"comic_type": "pages", "canvas": {"layout_preset": {"builtin": "ribbon"}}}), &json!({})).layout_preset, LayoutPresetSelection::None);
}

/// A title that stores nothing takes its kind's preset AS THE USER EDITED IT (`user_config`
/// `Canvas.layout_presets` override), while the legacy compact-mode recovery keeps the factory row.
#[test]
fn fallback_follows_the_users_builtin_override() {
    let user = json!({"Canvas": {"layout_presets": {"version": 1, "builtin": {"webtoon": {"page_spacing_px": 77, "aside_compact_mode": "strong"}}}}});
    let marker = read(&json!({"comic_type": "ribbon", "canvas": {"layout_per_title": true}}), &user);
    let mut expected = BuiltinLayoutPreset::Webtoon.factory();
    expected.page_spacing_px = 77;
    expected.aside_compact_mode = "strong".to_owned();
    assert_eq!(CanvasLayout::of(&marker), expected);
    let legacy = read(&json!({"comic_type": "ribbon"}), &user);
    assert_eq!(legacy.page_spacing_px, 77, "a field the legacy title lacks takes the override");
    assert_eq!(legacy.aside_compact_mode, "none", "the legacy compact mode is the factory row's");
    let pages = read(&json!({"comic_type": "pages", "canvas": {"layout_per_title": true}}), &user);
    assert_eq!(CanvasLayout::of(&pages), BuiltinLayoutPreset::Pages.factory(), "another kind is untouched");
}
