//! Tests of the `TranslationTab` host: it declares exactly the tab's input set (the golden lives
//! in `input/tests.rs`), the execute dispatch covers every declared command, the input contexts
//! it publishes, the exit-only hint-collapsed key round trip and the dock builder it hands the
//! root.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ms_canvas::CANVAS_RIBBON_TAB;
use ms_input::{ActionId, ActionKind, Fired, view};
use ms_models::bubbles_model::{BubblesModel, SharedCanvasSettings};
use ms_project::{CanvasSettings, ProjectData, ProjectPaths};
use ms_studio_tab::StudioTab;
use ms_widgets::panel_dock::TabId;
use serde_json::{Value, json};

use super::TranslationTab;
use crate::tab::{TranslationTabState, translation_default_dock_layout};

/// A host over a default tab state and an empty bubbles model whose (never written) documents
/// live under `dir`, with `user_settings` as the startup config.
fn host_in(dir: &Path, user_settings: &Value) -> TranslationTab {
    let model = BubblesModel::new_with_staging_probe(Vec::new(), dir.join("bubbles.json"), dir.join("unsaved_bubbles.json"), SharedCanvasSettings::default(), None, false);
    TranslationTab::new(TranslationTabState::default(), Arc::new(Mutex::new(model)), user_settings, None)
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

/// The host declares exactly the tab's input set.
#[test]
fn input_declarations_are_the_tabs_set() {
    let dir = tempfile::tempdir().expect("temp dir");
    let host = host_in(dir.path(), &Value::Null);
    assert_eq!(host.input_declarations(), crate::input::declarations());
}

/// A fired command without parameters or focus.
fn fired(id: &'static str) -> Fired {
    Fired { action: ActionId(id), params: Vec::new(), focus: None }
}

/// Every declared command is executed by this tab (so the declarations and the execute arms cannot
/// drift apart); modes, gestures, ids of the session or of another owner (the shared zoom commands
/// are the canvas's), and unknown ids are refused untouched.
#[test]
fn run_command_handles_exactly_its_own_commands() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut host = host_in(dir.path(), &Value::Null);
    let project = project_in(dir.path());
    let ctx = egui::Context::default();
    for foreign in ["single_image.save", "", "view.zoom", "view.pan", "cleaning.canvas.zoom_in", "translation.canvas.zoom_in", "canvas.bubble.undo"] {
        assert!(!host.run_command(&ctx, &project, &fired(foreign)), "{foreign:?} must not be handled by Translation");
    }
    for zoom in [view::ZOOM_IN, view::ZOOM_OUT, view::ZOOM_RESET] {
        assert!(!host.run_command(&ctx, &project, &fired(zoom.0)), "{zoom:?} is executed by the canvas");
    }
    for spec in crate::input::declarations().actions {
        let id = spec.meta().id;
        assert_eq!(host.run_command(&ctx, &project, &fired(id.0)), spec.kind() == ActionKind::Command, "{id:?}");
    }
}

/// The host forwards its canvas's contexts (the Translation canvas edits bubbles) and
/// adds the tab's own, none while no edit mode, window or brush is on.
#[test]
fn the_host_forwards_the_canvas_contexts() {
    let dir = tempfile::tempdir().expect("temp dir");
    let host = host_in(dir.path(), &Value::Null);
    assert!(host.state().input_contexts().is_empty());
    assert_eq!(host.input_contexts(), vec![ms_canvas::input::EDITABLE]);
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
/// every dock tab this program tab declares — «Лента», «Последние персонажи», the toolbar and the
/// five tools (a stored tab missing from it would be dropped on load). The ids are persistence
/// keys, so they are spelled out here rather than taken from the constants.
#[test]
fn the_dock_builder_is_the_tabs_own_and_names_its_dock_tabs() {
    let dir = tempfile::tempdir().expect("temp dir");
    let host = host_in(dir.path(), &Value::Null);
    let builder = host.default_dock_layout().expect("Translation hosts the panel dock");
    assert!(std::ptr::fn_addr_eq(builder, translation_default_dock_layout as fn() -> _));
    let layout = builder();
    let tabs = [
        CANVAS_RIBBON_TAB,
        TabId::new("translation.recent_characters"),
        TabId::new("translation.tools"),
        TabId::new("translation.tool.bubbles"),
        TabId::new("translation.tool.ocr"),
        TabId::new("translation.tool.composition"),
        TabId::new("translation.tool.machine_translation"),
        TabId::new("translation.tool.text_detector"),
    ];
    for id in tabs {
        assert!(layout.panel_of_tab(id).is_some(), "the default layout does not name {id}");
    }
    assert_eq!(layout.panels().iter().map(|panel| panel.tabs.len()).sum::<usize>(), tabs.len(), "a tab the test does not know");
}
