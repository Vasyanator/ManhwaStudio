//! Tests of the `TranslationTab` host: the hotkey set it declares (pinned to the studio root's
//! `hotkey_registry_golden` Translation slice), the execute dispatch, the exit-only hint-collapsed
//! key round trip and the dock builder it hands the root.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use egui::{Key, Modifiers as M};
use ms_canvas::CANVAS_RIBBON_TAB;
use ms_config::app_tab::AppTab;
use ms_models::bubbles_model::{BubblesModel, SharedCanvasSettings};
use ms_project::{CanvasSettings, ProjectData, ProjectPaths};
use ms_studio_tab::StudioTab;
use ms_widgets::input_manager_v2::{HotkeyScopeV2, ModifierOnlyV2};
use ms_widgets::panel_dock::TabId;
use serde_json::{Value, json};

use super::TranslationTab;
use crate::tab::{TranslationTabState, translation_default_dock_layout};

/// (id, scope, default shortcut as (modifiers, key), default modifier-only, active when input).
type HotkeyRow = (&'static str, HotkeyScopeV2, Option<(M, Key)>, Option<ModifierOnlyV2>, bool);

const TR: HotkeyScopeV2 = HotkeyScopeV2::Tab(AppTab::Translation);

/// The Translation slice of the studio root's `hotkey_registry_golden` (`src/studio.rs`), copied
/// verbatim: ids are persistence keys and the order is observable within the scope.
const GOLDEN_TRANSLATION_ROWS: [HotkeyRow; 17] = [
    ("translation.canvas.zoom_in", TR, Some((M::COMMAND, Key::Equals)), None, false),
    ("translation.canvas.zoom_out", TR, Some((M::COMMAND, Key::Minus)), None, false),
    ("translation.canvas.zoom_reset", TR, Some((M::COMMAND, Key::Num0)), None, false),
    ("translation.bubble.delete_selected", TR, Some((M::NONE, Key::Delete)), None, false),
    ("translation.bubble.create_at_cursor", TR, Some((M::NONE, Key::T)), None, false),
    ("translation.bubble.create_hint_at_cursor", TR, Some((M::NONE, Key::H)), None, false),
    ("translation.ocr.quick.selection_mode", TR, None, Some(ModifierOnlyV2::Shift), false),
    ("translation.ocr.advanced.selection_mode", TR, None, Some(ModifierOnlyV2::Alt), false),
    ("translation.panel.bubbles.toggle", TR, Some((M::NONE, Key::P)), None, false),
    ("translation.panel.ocr.toggle", TR, Some((M::NONE, Key::S)), None, false),
    ("translation.panel.composition.toggle", TR, Some((M::NONE, Key::K)), None, false),
    ("translation.panel.machine_translation.toggle", TR, Some((M::NONE, Key::M)), None, false),
    ("translation.panel.detector.toggle", TR, Some((M::NONE, Key::D)), None, false),
    ("translation.bubble.copy_original", TR, None, None, false),
    ("translation.bubble.copy_translation", TR, None, None, false),
    ("translation.bubble.paste_original", TR, None, None, false),
    ("translation.bubble.paste_translation", TR, None, None, false),
];

/// A host over a default tab state and an empty bubbles model whose (never written) documents
/// live under `dir`, with `user_settings` as the startup config.
fn host_in(dir: &Path, user_settings: &Value) -> TranslationTab {
    let model = BubblesModel::new_with_staging_probe(Vec::new(), dir.join("bubbles.json"), dir.join("unsaved_bubbles.json"), SharedCanvasSettings::default(), None, false);
    TranslationTab::new(TranslationTabState::default(), Arc::new(Mutex::new(model)), user_settings)
}

/// A minimal `ProjectData` whose every path is `dir` (nothing here reads or writes it).
fn project_in(dir: &Path) -> ProjectData {
    let dir = PathBuf::from(dir);
    ProjectData {
        project_dir: dir.clone(),
        image_dir: dir.clone(),
        pages: Vec::new(),
        bubbles: Arc::new(Vec::new()),
        paths: ProjectPaths {
            project_dir: dir.clone(),
            title_dir: dir.clone(),
            notes_file: dir.clone(),
            char_favorites_file: dir.clone(),
            color_presets_file: dir.clone(),
            bubbles_file: dir.clone(),
            src_dir: dir.clone(),
            clean_layers_dir: dir.clone(),
            cleaned_dir: dir.clone(),
            alt_vers_dir: dir.clone(),
            saved_dir: dir.clone(),
            image_bubbles_dir: dir.clone(),
            text_images_dir: dir.clone(),
            layers_dir: dir.clone(),
            text_detection_dir: dir.clone(),
            characters_dir: dir.clone(),
            terms_file: dir.clone(),
            settings_file: dir.clone(),
            unsaved_dir: dir.clone(),
            unsaved_bubbles_file: dir.clone(),
            unsaved_clean_layers_dir: dir.clone(),
            unsaved_image_bubbles_dir: dir.clone(),
            unsaved_text_images_dir: dir.clone(),
            unsaved_layers_dir: dir,
        },
        comic_type: None,
        canvas_settings: CanvasSettings::default(),
        settings_data: Value::Null,
        session: Default::default(),
    }
}

#[test]
fn hotkey_specs_equal_the_root_golden_translation_slice() {
    let dir = tempfile::tempdir().expect("temp dir");
    let host = host_in(dir.path(), &Value::Null);
    let actual: Vec<HotkeyRow> = host
        .hotkey_specs()
        .iter()
        .map(|spec| (spec.id, spec.scope, spec.default_shortcut.map(|shortcut| (shortcut.modifiers, shortcut.logical_key)), spec.default_modifier_only, spec.active_when_input))
        .collect();
    assert_eq!(actual, GOLDEN_TRANSLATION_ROWS.to_vec());
}

/// Every declared id is executed by this tab (so the spec list and the execute arms cannot drift
/// apart), and an id of another tab, of the session or an unknown one is refused untouched.
#[test]
fn execute_hotkey_handles_exactly_its_own_ids() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut host = host_in(dir.path(), &Value::Null);
    let project = project_in(dir.path());
    let ctx = egui::Context::default();
    for foreign in ["cleaning.canvas.zoom_in", "single_image.save", "", "translation.canvas.zoom"] {
        assert!(!host.execute_hotkey(&ctx, foreign, &project), "{foreign:?} must not be handled by Translation");
    }
    // The two modifier-only ids are never reported by `collect_triggered`; every other declared id
    // must reach an execute arm.
    for (id, _, _, modifier_only, _) in GOLDEN_TRANSLATION_ROWS {
        assert_eq!(host.execute_hotkey(&ctx, id, &project), modifier_only.is_none(), "{id}");
    }
}

/// `new` seeds the bottom-hint collapsed flag from `Canvas.translation_hint_collapsed` (missing or
/// non-bool = expanded), and `write_exit_settings` writes the current flag back under the same key
/// while keeping every other key, including a stale cleaning flag.
#[test]
fn hint_collapsed_round_trips_through_user_config() {
    let dir = tempfile::tempdir().expect("temp dir");
    for absent in [Value::Null, json!({}), json!({ "Canvas": 3 }), json!({ "Canvas": { "translation_hint_collapsed": "yes" } }), json!({ "Canvas": { "typing_hint_collapsed": true } })] {
        assert!(!host_in(dir.path(), &absent).canvas().bottom_hint_collapsed(), "{absent}");
    }
    let host = host_in(dir.path(), &json!({ "Canvas": { "translation_hint_collapsed": true } }));
    assert!(host.canvas().bottom_hint_collapsed());

    let mut root = json!({ "Canvas": { "cache_pages": true, "cleaning_hint_collapsed": true }, "General": { "ui_language": "ru" } });
    host.write_exit_settings(&mut root);
    assert_eq!(root, json!({ "Canvas": { "cache_pages": true, "cleaning_hint_collapsed": true, "translation_hint_collapsed": true }, "General": { "ui_language": "ru" } }));

    let mut expanded = host_in(dir.path(), &Value::Null);
    let mut non_object = json!([1, 2]);
    expanded.canvas_mut().set_bottom_hint_collapsed(false);
    expanded.write_exit_settings(&mut non_object);
    assert_eq!(non_object, json!({ "Canvas": { "translation_hint_collapsed": false } }));
}

/// The host is a dock host whose builder is the tab's own default layout, and that layout names
/// both dock tabs this program tab declares (a stored tab missing from it would be dropped on load).
#[test]
fn the_dock_builder_is_the_tabs_own_and_names_its_dock_tabs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let host = host_in(dir.path(), &Value::Null);
    let builder = host.default_dock_layout().expect("Translation hosts the panel dock");
    assert!(std::ptr::fn_addr_eq(builder, translation_default_dock_layout as fn() -> _));
    let layout = builder();
    for id in [CANVAS_RIBBON_TAB, TabId::new("translation.recent_characters")] {
        assert!(layout.panel_of_tab(id).is_some(), "the default layout does not name {id}");
    }
}
