//! Tests of the `TranslationTab` host: the input actions it declares (pinned to the studio
//! root's `input_registry_golden` Translation slice), the execute dispatch, the input contexts it
//! publishes, the exit-only hint-collapsed key round trip and the dock builder it hands the root.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use egui::Key;
use ms_canvas::CANVAS_RIBBON_TAB;
use ms_config::app_tab::AppTab;
use ms_input::{ActionKind, Binding, Button, Chord, InputRegistry, ModifierKey, Scope, view};
use ms_models::bubbles_model::{BubblesModel, SharedCanvasSettings};
use ms_project::{CanvasSettings, ProjectData, ProjectPaths};
use ms_studio_tab::StudioTab;
use ms_widgets::panel_dock::TabId;
use serde_json::{Value, json};

use super::TranslationTab;
use crate::tab::{TranslationTabState, translation_default_dock_layout};

/// (id, kind, scope, default alternatives).
type ActionRow = (&'static str, ActionKind, Scope, Vec<Binding>);

const TR: Scope = Scope::Tab(AppTab::Translation);

/// A command alternative "this key, no modifier".
fn key(key: Key) -> Vec<Binding> {
    vec![Binding::command(Chord::new([Button::Key(key)]))]
}

/// The Translation slice of the studio root's `input_registry_golden` (`src/studio.rs`), copied
/// verbatim: ids are persistence keys of the user's overrides.
fn golden_translation_rows() -> Vec<ActionRow> {
    use ActionKind::{Command, Mode};
    vec![
        ("translation.bubble.delete_selected", Command, TR, key(Key::Delete)),
        ("translation.bubble.create_at_cursor", Command, TR, key(Key::T)),
        ("translation.bubble.create_hint_at_cursor", Command, TR, key(Key::H)),
        ("translation.ocr.quick_mode", Mode, TR, vec![Binding::mode([Button::Modifier(ModifierKey::Shift)])]),
        ("translation.ocr.advanced_mode", Mode, TR, vec![Binding::mode([Button::Modifier(ModifierKey::Alt)])]),
        ("translation.panel.bubbles.toggle", Command, TR, key(Key::P)),
        ("translation.panel.ocr.toggle", Command, TR, key(Key::S)),
        ("translation.panel.composition.toggle", Command, TR, key(Key::K)),
        ("translation.panel.machine_translation.toggle", Command, TR, key(Key::M)),
        ("translation.panel.detector.toggle", Command, TR, key(Key::D)),
        ("translation.bubble.copy_original", Command, TR, Vec::new()),
        ("translation.bubble.copy_translation", Command, TR, Vec::new()),
        ("translation.bubble.paste_original", Command, TR, Vec::new()),
        ("translation.bubble.paste_translation", Command, TR, Vec::new()),
    ]
}

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
fn input_declarations_equal_the_root_golden_translation_slice() {
    let dir = tempfile::tempdir().expect("temp dir");
    let host = host_in(dir.path(), &Value::Null);
    let declarations = host.input_declarations();
    let actual: Vec<ActionRow> = declarations.actions.iter().map(|spec| (spec.meta().id.0, spec.kind(), spec.meta().scope, spec.meta().defaults.clone())).collect();
    assert_eq!(actual, golden_translation_rows());
    assert!(declarations.targets.is_empty() && declarations.fixed.is_empty() && declarations.exclusive_contexts.is_empty());
    // Together with the shared view set they form a valid registry (no duplicate id, every
    // `beats` names a declared mode).
    let mut all = view::declarations();
    all.extend(declarations);
    InputRegistry::new(all).expect("the Translation declarations form a valid registry");
}

/// The advanced OCR mode beats the quick one when both are held (Shift+Alt).
#[test]
fn the_advanced_ocr_mode_beats_the_quick_one() {
    let advanced = TranslationTabState::input_actions().into_iter().find(|spec| spec.meta().id.0 == "translation.ocr.advanced_mode").expect("advanced mode declared");
    let ms_input::ActionSpec::Mode(mode) = advanced else { panic!("the advanced OCR selection is a mode") };
    assert_eq!(mode.beats, vec![ms_input::ActionId("translation.ocr.quick_mode")]);
}

/// Every declared command is executed by this tab (so the declarations and the execute arms cannot
/// drift apart); modes, ids of the session or of another owner (the shared zoom commands are the
/// canvas's), and unknown ids are refused untouched.
#[test]
fn run_command_handles_exactly_its_own_commands() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut host = host_in(dir.path(), &Value::Null);
    let project = project_in(dir.path());
    let ctx = egui::Context::default();
    for foreign in ["single_image.save", "", "view.zoom", "view.pan", "cleaning.canvas.zoom_in", "translation.canvas.zoom_in"] {
        assert!(!host.run_command(&ctx, &project, ms_input::ActionId(foreign)), "{foreign:?} must not be handled by Translation");
    }
    for zoom in [view::ZOOM_IN, view::ZOOM_OUT, view::ZOOM_RESET] {
        assert!(!host.run_command(&ctx, &project, zoom), "{zoom:?} is executed by the canvas");
    }
    for (id, kind, _, _) in golden_translation_rows() {
        assert_eq!(host.run_command(&ctx, &project, ms_input::ActionId(id)), kind == ActionKind::Command, "{id}");
    }
}

/// The host forwards its canvas's contexts (the Translation canvas edits bubbles and, with the
/// advanced-recognition window closed, zooms), and publishes no brush context while no brush is on.
#[test]
fn the_host_forwards_the_canvas_contexts() {
    let dir = tempfile::tempdir().expect("temp dir");
    let host = host_in(dir.path(), &Value::Null);
    assert!(!host.state().blocks_canvas_zoom());
    assert!(!host.state().brush_input_active());
    assert_eq!(host.input_contexts(), vec![ms_canvas::input::EDITABLE, ms_canvas::input::ZOOM_ACCEPTED]);
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
