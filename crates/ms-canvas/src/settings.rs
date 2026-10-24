/*
File: crates/ms-canvas/src/settings.rs

Purpose:
Canvas settings runtime and persistence helpers for `CanvasView`.

Main responsibilities:
- own `CanvasSettingsRuntime`;
- own the pure conversions project `CanvasSettings` -> `CanvasState` -> `SharedCanvasSettings`
  (the seed a project's shared canvas settings start from);
- apply/publish shared canvas setting snapshots;
- sync `cache_pages` with `CleanOverlaysModel`;
- serialize project/user canvas settings without blocking the GUI thread.

Key structures:
- CanvasSettingsRuntime

Key functions:
- canvas_state_from_project() / shared_settings_from_state() / shared_canvas_settings_from_project()
- CanvasView::apply_canvas_snapshot()
- CanvasView::canvas_snapshot() (pub: delegates to `shared_settings_from_state`)
- CanvasView::publish_canvas_settings()
- save_canvas_settings_to_project_file()
- save_canvas_settings_to_user_file()

Notes:
- Actual filesystem writes are executed by the background saver worker from `workers.rs`.
- The title `settings.json` is written ONLY through `ms_docstore::update` (serialized,
  atomic, malformed file never overwritten); this module edits only its own keys there. The
  project writer stores the whole per-title layout plus `canvas.layout_per_title` (the per-title
  marker, `ms_project::canvas_layout`); the user writer stores only the cross-title preferences.
- This module keeps settings-specific logic out of the main canvas facade.
*/

use super::types::{CanvasSettingsSaveRequest, pixel_setting_to_f32};
use super::workers::spawn_canvas_settings_saver_thread;
use super::{
    AsideBubbleCompactMode, AsideBubbleSideMode, BubbleMode, BubbleType, CanvasState, CanvasView,
    OnTopFocusMode, TranslationStatusDisplay,
};
use ms_config as config;
use ms_models::bubbles_model::SharedCanvasSettings;
use ms_project::canvas_layout::LAYOUT_PER_TITLE_KEY;
use ms_project::{CanvasLayout, CanvasSettings, ProjectData};
use ms_docstore::{DocKind, DocRef, WriteOptions};
use ms_log::runtime_log;
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::mpsc::Sender;
use ms_thread::JoinHandle;

pub(super) struct CanvasSettingsRuntime {
    pub(super) synced_canvas_revision: u64,
    pub(super) last_published_canvas_snapshot: Option<SharedCanvasSettings>,
    pub(super) canvas_settings_save_tx: Sender<Option<CanvasSettingsSaveRequest>>,
    pub(super) canvas_settings_save_thread: Option<JoinHandle<()>>,
}

impl Default for CanvasSettingsRuntime {
    fn default() -> Self {
        let (canvas_settings_save_tx, canvas_settings_save_thread) =
            spawn_canvas_settings_saver_thread();
        Self {
            synced_canvas_revision: 0,
            last_published_canvas_snapshot: None,
            canvas_settings_save_tx,
            canvas_settings_save_thread: Some(canvas_settings_save_thread),
        }
    }
}

/// The `CanvasState` a canvas opened on a project starts from, built from its `CanvasSettings`.
///
/// Every persisted field is taken from `settings`; view-only fields (`zoom`) keep their
/// `CanvasState::default` values. The stored `bubble_type` only picks the editable/readonly
/// display types (a legacy `aside` / `on_top` mode pins both, `hybrid` resolves the stored pair
/// with `on_top` / `aside` fallbacks); the resulting mode is always `Hybrid`. Unknown enum strings
/// fall back through each enum's total `from_str`. Nothing is clamped here (unlike
/// `CanvasView::apply_canvas_snapshot`), so the values reach the shared model as stored.
#[must_use]
pub fn canvas_state_from_project(settings: &CanvasSettings) -> CanvasState {
    // The 13 layout fields go through the one layout -> state mapping; the legacy mode pin
    // below then overrides the two display types it decides.
    let base = CanvasState::with_layout(&CanvasLayout::of(settings));
    let (hybrid_editable_bubble_type, hybrid_readonly_bubble_type) = match BubbleMode::from_str(&settings.bubble_type) {
        BubbleMode::Aside => (BubbleType::Aside, BubbleType::Aside),
        BubbleMode::OnTop => (BubbleType::OnTop, BubbleType::OnTop),
        BubbleMode::Hybrid => (base.hybrid_editable_bubble_type, base.hybrid_readonly_bubble_type),
    };
    CanvasState {
        bubble_mode: BubbleMode::Hybrid,
        hybrid_editable_bubble_type,
        hybrid_readonly_bubble_type,
        show_bubbles: settings.show_bubbles,
        show_bubble_status: settings.show_bubble_status,
        bubble_status_rules: settings.bubble_status_rules.clone(),
        bubble_opacity: settings.bubble_opacity,
        side_margin: pixel_setting_to_f32(settings.side_margin_px),
        auto_insert_last_character: settings.auto_insert_last_character,
        spellcheck_original: settings.spellcheck_original,
        spellcheck_translation: settings.spellcheck_translation,
        tabs_autosync_enabled: settings.tabs_autosync_enabled,
        cache_pages: settings.cache_pages,
        translation_status_display: TranslationStatusDisplay::from_str(&settings.translation_status_display),
        hint_show_outside_default: settings.hint_show_outside_default,
        ..base
    }
}

/// `state`'s shared settings as a `SharedCanvasSettings` value.
///
/// The one owner of the `CanvasState` -> `SharedCanvasSettings` mapping: `bubble_type` is
/// always `Hybrid`, enums are stored by `as_str`, every other field is copied as is (no
/// clamping; `CanvasView::apply_canvas_snapshot` clamps on the way back in). Pure.
#[must_use]
pub fn shared_settings_from_state(state: &CanvasState) -> SharedCanvasSettings {
    SharedCanvasSettings {
        bubble_type: BubbleMode::Hybrid.as_str().to_string(),
        editable_bubble_type: state.hybrid_editable_bubble_type.as_str().to_string(),
        readonly_bubble_type: state.hybrid_readonly_bubble_type.as_str().to_string(),
        show_bubbles: state.show_bubbles,
        show_bubble_status: state.show_bubble_status,
        bubble_status_rules: state.bubble_status_rules.clone(),
        bubble_opacity: state.bubble_opacity,
        page_spacing: state.page_spacing,
        separate_pages: state.separate_pages,
        edge_margin: state.edge_margin,
        side_margin: state.side_margin,
        bubble_min_width: state.bubble_min_width,
        bubble_max_width: state.bubble_max_width,
        aside_compact_mode: state.aside_compact_mode.as_str().to_string(),
        aside_side_mode: state.aside_side_mode.as_str().to_string(),
        aside_second_column: state.aside_second_column,
        on_top_focus_mode: state.on_top_focus_mode.as_str().to_string(),
        scale_bubbles: state.scale_bubbles,
        aside_scale_pct: state.aside_scale_pct,
        auto_insert_last_character: state.auto_insert_last_character,
        spellcheck_original: state.spellcheck_original,
        spellcheck_translation: state.spellcheck_translation,
        tabs_autosync_enabled: state.tabs_autosync_enabled,
        cache_pages: state.cache_pages,
        translation_status_display: state.translation_status_display.as_str().to_string(),
        hint_show_outside_default: state.hint_show_outside_default,
    }
}

/// The shared canvas settings a project opens with: its stored `CanvasSettings` normalized
/// through `CanvasState` (see [`canvas_state_from_project`] and [`shared_settings_from_state`]).
///
/// The seed of `BubblesModel`'s canvas settings; every canvas pulls it on its first draw. Pure.
#[must_use]
pub fn shared_canvas_settings_from_project(settings: &CanvasSettings) -> SharedCanvasSettings {
    shared_settings_from_state(&canvas_state_from_project(settings))
}

impl CanvasView {
    pub(super) fn apply_canvas_snapshot(&mut self, snapshot: &SharedCanvasSettings) {
        let loaded_bubble_mode = BubbleMode::from_str(&snapshot.bubble_type);
        self.state.bubble_mode = BubbleMode::Hybrid;
        self.state.hybrid_editable_bubble_type = match loaded_bubble_mode {
            BubbleMode::Aside => BubbleType::Aside,
            BubbleMode::OnTop => BubbleType::OnTop,
            BubbleMode::Hybrid => {
                BubbleType::from_str(&snapshot.editable_bubble_type).resolved(BubbleType::OnTop)
            }
        };
        self.state.hybrid_readonly_bubble_type = match loaded_bubble_mode {
            BubbleMode::Aside => BubbleType::Aside,
            BubbleMode::OnTop => BubbleType::OnTop,
            BubbleMode::Hybrid => {
                BubbleType::from_str(&snapshot.readonly_bubble_type).resolved(BubbleType::Aside)
            }
        };
        self.state.show_bubbles = snapshot.show_bubbles;
        self.state.show_bubble_status = snapshot.show_bubble_status;
        self.state.bubble_status_rules = snapshot.bubble_status_rules.clone();
        self.state.bubble_opacity = snapshot.bubble_opacity.clamp(0.0, 1.0);
        self.state.page_spacing = snapshot.page_spacing.max(0.0);
        self.state.separate_pages = snapshot.separate_pages;
        self.state.edge_margin = snapshot.edge_margin.max(0.0);
        self.state.side_margin = snapshot.side_margin.max(0.0);
        self.state.bubble_min_width = snapshot.bubble_min_width.max(40.0);
        self.state.bubble_max_width = snapshot.bubble_max_width.max(self.state.bubble_min_width);
        self.state.aside_compact_mode =
            AsideBubbleCompactMode::from_str(&snapshot.aside_compact_mode);
        self.state.aside_side_mode = AsideBubbleSideMode::from_str(&snapshot.aside_side_mode);
        self.state.aside_second_column = snapshot.aside_second_column;
        self.state.on_top_focus_mode = OnTopFocusMode::from_str(&snapshot.on_top_focus_mode);
        self.state.scale_bubbles = snapshot.scale_bubbles;
        self.state.aside_scale_pct = snapshot.aside_scale_pct.clamp(25, 300);
        self.state.auto_insert_last_character = snapshot.auto_insert_last_character;
        self.state.spellcheck_original = snapshot.spellcheck_original;
        self.state.spellcheck_translation = snapshot.spellcheck_translation;
        self.state.tabs_autosync_enabled = snapshot.tabs_autosync_enabled;
        self.state.cache_pages = snapshot.cache_pages;
        self.state.translation_status_display =
            TranslationStatusDisplay::from_str(&snapshot.translation_status_display);
        self.state.hint_show_outside_default = snapshot.hint_show_outside_default;
        self.sync_cache_pages_setting_to_model();
    }

    pub(super) fn publish_canvas_settings(&mut self, project: &ProjectData) {
        let snapshot = self.canvas_snapshot();
        if self
            .settings_runtime
            .last_published_canvas_snapshot
            .as_ref()
            == Some(&snapshot)
        {
            return;
        }

        if let Some(model) = &self.bubble_runtime.bubbles_model
            && let Ok(mut locked) = model.lock()
        {
            locked.set_canvas_settings(snapshot.clone());
            self.settings_runtime.synced_canvas_revision = locked.canvas_revision();
        }

        self.settings_runtime.last_published_canvas_snapshot = Some(snapshot.clone());
        self.sync_cache_pages_setting_to_model();
        self.queue_canvas_settings_save(&project.paths.settings_file, snapshot);
    }

    /// The canvas' current shared settings as a `SharedCanvasSettings` value.
    ///
    /// Delegates to [`shared_settings_from_state`], the one owner of that mapping. Pure: it
    /// publishes nothing.
    #[must_use]
    pub fn canvas_snapshot(&self) -> SharedCanvasSettings {
        shared_settings_from_state(&self.state)
    }

    pub(super) fn sync_cache_pages_setting_to_model(&mut self) {
        let Some(model) = self.overlay_runtime.overlays_model.as_ref() else {
            return;
        };
        match model.lock() {
            Ok(mut locked) => locked.set_cache_pages_enabled(self.state.cache_pages),
            Err(_) => runtime_log::log_warn(format!(
                "[canvas::settings] failed to lock CleanOverlaysModel while syncing cache_pages={}",
                self.state.cache_pages
            )),
        }
    }

    fn queue_canvas_settings_save(
        &self,
        project_settings_file: &Path,
        snapshot: SharedCanvasSettings,
    ) {
        if project_settings_file.as_os_str().is_empty() {
            return;
        }
        let request = CanvasSettingsSaveRequest {
            project_settings_file: project_settings_file.to_path_buf(),
            user_settings_file: config::user_config_path(),
            snapshot,
        };
        if self
            .settings_runtime
            .canvas_settings_save_tx
            .send(Some(request))
            .is_err()
        {
            runtime_log::log_error(format!(
                "[canvas::settings] failed to queue canvas settings save; project_settings_file={}",
                project_settings_file.display()
            ));
        }
    }
}

/// Persists the project-scoped canvas subset into the title's `settings.json` through the
/// serialized `ms_docstore::update` read-modify-write.
///
/// The mutator edits ONLY the keys this writer owns (`canvas.*` fields of the snapshot including
/// the whole layout and the per-title marker, the four top-level legacy bubble/focus mirrors and
/// the listed retired-key removals); every other section
/// (`OCR`, `canvas.project_custom_spellcheck_words`, ...) is kept as found, and the document
/// lock serializes this writer against the other `settings.json` writers.
///
/// # Errors
/// Returns a technical message when the existing file is malformed or its root is not an
/// object (the file is left untouched), or when the atomic write fails.
pub fn save_canvas_settings_to_project_file(
    settings_file: &Path,
    snapshot: &SharedCanvasSettings,
) -> Result<(), String> {
    let doc = DocRef::new(settings_file, DocKind::ProjectSettings);
    // Default options = the historical layout (2-space pretty, no trailing newline, parent
    // directories created).
    ms_docstore::update(&doc, WriteOptions::default(), |root| {
        let Some(root_obj) = root.as_object_mut() else {
            return Err(format!(
                "failed to parse project canvas settings file '{}': root JSON value is not an object",
                settings_file.display()
            ));
        };
        apply_project_canvas_snapshot(root_obj, snapshot);
        Ok(())
    })
    .map_err(|err| format!("failed to save project canvas settings file '{}': {err}", settings_file.display()))
}

/// Writes the project-scoped canvas keys of `snapshot` into the `settings.json` root object: the
/// whole layout (`CanvasLayout::write_json_object` of `snapshot.layout()`) plus the per-title marker
/// `canvas.layout_per_title = true`, the other project-scoped fields, and, for older builds, the
/// legacy root mirror of the bubble mode, the two display types and the on-top focus. Removes a
/// few retired keys; every other key is kept.
fn apply_project_canvas_snapshot(root_obj: &mut Map<String, Value>, snapshot: &SharedCanvasSettings) {
    let layout = snapshot.layout();
    let mut canvas_obj = root_obj
        .get("canvas")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    CanvasLayout::write_json_object(&layout, &mut canvas_obj);
    canvas_obj.insert(LAYOUT_PER_TITLE_KEY.to_string(), Value::Bool(true));
    canvas_obj.insert(
        "bubble_type".to_string(),
        Value::String(snapshot.bubble_type.clone()),
    );
    canvas_obj.insert(
        "show_bubbles".to_string(),
        Value::Bool(snapshot.show_bubbles),
    );
    canvas_obj.insert(
        "show_bubble_status".to_string(),
        Value::Bool(snapshot.show_bubble_status),
    );
    canvas_obj.insert(
        "bubble_opacity".to_string(),
        Value::from(snapshot.bubble_opacity.clamp(0.0, 1.0) as f64),
    );
    canvas_obj.insert(
        "side_margin_px".to_string(),
        Value::from(snapshot.side_margin.max(0.0).round() as i64),
    );
    canvas_obj.remove("load_all_bubbles");
    canvas_obj.remove("visible_page_radius");
    canvas_obj.remove("bubble_load_delay_ms");
    canvas_obj.insert(
        "tabs_autosync_enabled".to_string(),
        Value::Bool(snapshot.tabs_autosync_enabled),
    );
    canvas_obj.insert(
        "auto_insert_last_character".to_string(),
        Value::Bool(snapshot.auto_insert_last_character),
    );
    canvas_obj.insert(
        "spellcheck_original".to_string(),
        Value::Bool(snapshot.spellcheck_original),
    );
    canvas_obj.insert(
        "spellcheck_translation".to_string(),
        Value::Bool(snapshot.spellcheck_translation),
    );
    canvas_obj.insert("cache_pages".to_string(), Value::Bool(snapshot.cache_pages));
    canvas_obj.insert(
        "translation_status_display".to_string(),
        Value::String(snapshot.translation_status_display.clone()),
    );
    canvas_obj.insert(
        "hint_show_outside_default".to_string(),
        Value::Bool(snapshot.hint_show_outside_default),
    );
    canvas_obj.remove("copy_from_field");
    canvas_obj.remove("paste_into_field");
    root_obj.insert("canvas".to_string(), Value::Object(canvas_obj));

    // Legacy root mirror: builds before the per-title layout read these root keys as a fallback.
    root_obj.insert(
        "bubble_type".to_string(),
        Value::String(snapshot.bubble_type.clone()),
    );
    root_obj.insert(
        "editable_bubble_type".to_string(),
        Value::String(layout.editable_bubble_type.clone()),
    );
    root_obj.insert(
        "readonly_bubble_type".to_string(),
        Value::String(layout.readonly_bubble_type.clone()),
    );
    root_obj.insert(
        "on_top_focus_mode".to_string(),
        Value::String(layout.on_top_focus_mode),
    );
    root_obj.remove("copy_from_field");
    root_obj.remove("paste_into_field");
    root_obj.remove("visible_page_radius");
    root_obj.remove("bubble_load_delay_ms");
}

/// Persists the cross-title canvas preferences (`bubble_status_rules`, `cache_pages`,
/// `translation_status_display`, `hint_show_outside_default`) into `user_config` `Canvas` through
/// the serialized user-config RMW boundary. Nothing of the per-title layout or the per-title
/// spellcheck toggles is written there (older builds did; those keys are left as found).
///
/// The process-wide lock is acquired inside [`config::update_user_config_file`] and
/// remains held from the initial read through the full-file write, preventing a stale
/// canvas snapshot from erasing a concurrently persisted ORT crash-guard marker.
pub fn save_canvas_settings_to_user_file(
    user_settings_file: &Path,
    snapshot: &SharedCanvasSettings,
) -> Result<(), String> {
    config::update_user_config_file(user_settings_file, |root| {
        let Some(root_obj) = root.as_object_mut() else {
            return Err(anyhow::anyhow!(
                "user canvas settings root became non-object unexpectedly: '{}'",
                user_settings_file.display()
            ));
        };
        let mut canvas_obj = root_obj
            .get("Canvas")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        canvas_obj.insert(
            "bubble_status_rules".to_string(),
            ms_widgets::bubble_status::bubble_status_rules_to_value(&snapshot.bubble_status_rules),
        );
        canvas_obj.insert("cache_pages".to_string(), Value::Bool(snapshot.cache_pages));
        canvas_obj.insert(
            "translation_status_display".to_string(),
            Value::String(snapshot.translation_status_display.clone()),
        );
        canvas_obj.insert(
            "hint_show_outside_default".to_string(),
            Value::Bool(snapshot.hint_show_outside_default),
        );
        root_obj.insert("Canvas".to_string(), Value::Object(canvas_obj));
        Ok(())
    })
    .map_err(|err| err.to_string())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    /// Verbatim copy of the studio root's former field-by-field seeding of the Translation canvas
    /// (`src/studio.rs` `StudioApp::new`, before the seed moved here), ending in the snapshot the
    /// shared model was built from. Characterization oracle: never edit it to match new code.
    /// Two mechanical substitutions only: the `cache_pages` local reads the project field it was
    /// copied from, and the bottom-hint collapse call is dropped (view state, not in the snapshot).
    fn legacy_seed_oracle(settings: &CanvasSettings) -> SharedCanvasSettings {
        let project_canvas_settings = settings;
        let cache_pages = project_canvas_settings.cache_pages;
        let mut canvas = CanvasView::default();
        let loaded_bubble_mode = BubbleMode::from_str(&project_canvas_settings.bubble_type);
        canvas.state.bubble_mode = BubbleMode::Hybrid;
        canvas.state.hybrid_editable_bubble_type = match loaded_bubble_mode {
            BubbleMode::Aside => BubbleType::Aside,
            BubbleMode::OnTop => BubbleType::OnTop,
            BubbleMode::Hybrid => {
                BubbleType::from_str(&project_canvas_settings.editable_bubble_type)
                    .resolved(BubbleType::OnTop)
            }
        };
        canvas.state.hybrid_readonly_bubble_type = match loaded_bubble_mode {
            BubbleMode::Aside => BubbleType::Aside,
            BubbleMode::OnTop => BubbleType::OnTop,
            BubbleMode::Hybrid => {
                BubbleType::from_str(&project_canvas_settings.readonly_bubble_type)
                    .resolved(BubbleType::Aside)
            }
        };
        canvas.state.show_bubbles = project_canvas_settings.show_bubbles;
        canvas.state.show_bubble_status = project_canvas_settings.show_bubble_status;
        canvas.state.bubble_status_rules = project_canvas_settings.bubble_status_rules.clone();
        canvas.state.bubble_opacity = project_canvas_settings.bubble_opacity;
        canvas.state.bubble_min_width = project_canvas_settings.aside_min_width_px as f32;
        canvas.state.bubble_max_width = project_canvas_settings.aside_max_width_px as f32;
        canvas.state.aside_compact_mode =
            AsideBubbleCompactMode::from_str(&project_canvas_settings.aside_compact_mode);
        canvas.state.aside_side_mode =
            AsideBubbleSideMode::from_str(&project_canvas_settings.aside_side_mode);
        canvas.state.aside_second_column = project_canvas_settings.aside_second_column;
        canvas.state.on_top_focus_mode =
            OnTopFocusMode::from_str(&project_canvas_settings.on_top_focus_mode);
        canvas.state.scale_bubbles = project_canvas_settings.scale_bubbles;
        canvas.state.page_spacing = project_canvas_settings.page_spacing_px as f32;
        canvas.state.separate_pages = project_canvas_settings.separate_pages;
        canvas.state.edge_margin = project_canvas_settings.vertical_edge_margin_px as f32;
        canvas.state.side_margin = project_canvas_settings.side_margin_px as f32;
        canvas.state.aside_scale_pct = project_canvas_settings.aside_scale_pct;
        canvas.state.auto_insert_last_character =
            project_canvas_settings.auto_insert_last_character;
        canvas.state.spellcheck_original = project_canvas_settings.spellcheck_original;
        canvas.state.spellcheck_translation = project_canvas_settings.spellcheck_translation;
        canvas.state.tabs_autosync_enabled = project_canvas_settings.tabs_autosync_enabled;
        canvas.state.cache_pages = cache_pages;
        canvas.state.translation_status_display =
            TranslationStatusDisplay::from_str(&project_canvas_settings.translation_status_display);
        canvas.state.hint_show_outside_default =
            project_canvas_settings.hint_show_outside_default;
        canvas.canvas_snapshot()
    }

    /// Project settings with every field moved off its default, so a dropped field shows up.
    fn non_default_settings() -> CanvasSettings {
        let mut rules = ms_config::bubble_status::default_bubble_status_rules();
        rules.reverse();
        rules.truncate(1);
        CanvasSettings {
            bubble_type: "hybrid".to_string(),
            editable_bubble_type: "aside".to_string(),
            readonly_bubble_type: "on_top".to_string(),
            show_bubbles: false,
            show_bubble_status: true,
            bubble_status_rules: rules,
            bubble_opacity: 0.25,
            aside_min_width_px: 123,
            aside_max_width_px: 777,
            aside_compact_mode: "strong".to_string(),
            aside_side_mode: "right".to_string(),
            aside_second_column: false,
            on_top_focus_mode: "aside".to_string(),
            scale_bubbles: false,
            page_spacing_px: 17,
            separate_pages: false,
            vertical_edge_margin_px: 33,
            side_margin_px: 44,
            aside_scale_pct: 250,
            auto_insert_last_character: false,
            spellcheck_original: true,
            spellcheck_translation: false,
            tabs_autosync_enabled: false,
            cache_pages: false,
            translation_status_display: "until_next".to_string(),
            hint_show_outside_default: true,
            layout_preset: ms_project::LayoutPresetSelection::None,
        }
    }

    /// Every case the conversion must agree with the legacy seeding on, labelled for failures.
    fn oracle_cases() -> Vec<(&'static str, CanvasSettings)> {
        let base = CanvasSettings::default;
        let with = |label: &'static str, edit: &dyn Fn(&mut CanvasSettings)| {
            let mut settings = base();
            edit(&mut settings);
            (label, settings)
        };
        let mut cases = vec![
            ("default", base()),
            ("non_default", non_default_settings()),
            with("bubble_type_aside", &|s| s.bubble_type = "aside".to_string()),
            with("bubble_type_on_top", &|s| s.bubble_type = "ON_TOP".to_string()),
            with("bubble_type_hybrid_default_pair", &|s| {
                s.editable_bubble_type = "default".to_string();
                s.readonly_bubble_type = "default".to_string();
            }),
            with("bubble_type_hybrid_on_top_pair", &|s| {
                s.editable_bubble_type = "on_top".to_string();
                s.readonly_bubble_type = "on_top".to_string();
            }),
            with("bubble_type_garbage", &|s| s.bubble_type = "sideways".to_string()),
            with("garbage_editable_readonly", &|s| {
                s.editable_bubble_type = "???".to_string();
                s.readonly_bubble_type = String::new();
            }),
            with("unknown_enum_strings", &|s| {
                s.aside_compact_mode = "tight".to_string();
                s.aside_side_mode = "up".to_string();
                s.on_top_focus_mode = "center".to_string();
                s.translation_status_display = "bars".to_string();
            }),
            with("mixed_case_enum_strings", &|s| {
                s.aside_compact_mode = "MODERATE".to_string();
                s.aside_side_mode = "Left".to_string();
                s.on_top_focus_mode = "ASIDE".to_string();
                s.translation_status_display = "None".to_string();
            }),
            with("numeric_extremes_max", &|s| {
                s.bubble_opacity = f32::MAX;
                s.aside_min_width_px = i32::MAX;
                s.aside_max_width_px = i32::MAX;
                s.page_spacing_px = i32::MAX;
                s.vertical_edge_margin_px = i32::MAX;
                s.side_margin_px = i32::MAX;
                s.aside_scale_pct = i32::MAX;
            }),
            with("numeric_extremes_min", &|s| {
                s.bubble_opacity = -1.0;
                s.aside_min_width_px = i32::MIN;
                s.aside_max_width_px = -1;
                s.page_spacing_px = i32::MIN;
                s.vertical_edge_margin_px = -5;
                s.side_margin_px = i32::MIN;
                s.aside_scale_pct = i32::MIN;
            }),
            with("numeric_unrepresentable_px", &|s| {
                s.aside_min_width_px = 16_777_217;
                s.page_spacing_px = -16_777_219;
            }),
        ];
        for preset in ms_project::BuiltinLayoutPreset::ALL {
            let mut settings = non_default_settings();
            preset.factory().apply_to(&mut settings);
            cases.push((preset.id(), settings));
        }
        cases
    }

    /// The pure conversion reproduces the legacy seeding exactly on every case.
    #[test]
    fn shared_settings_from_project_matches_legacy_seeding() {
        for (label, settings) in oracle_cases() {
            assert_eq!(shared_canvas_settings_from_project(&settings), legacy_seed_oracle(&settings), "case {label}");
        }
    }

    /// Both built-in layout presets are among the oracle cases (a vanished preset would silently
    /// shrink it).
    #[test]
    fn oracle_cases_cover_both_layout_presets() {
        let labels: Vec<&str> = oracle_cases().into_iter().map(|(label, _)| label).collect();
        assert!(labels.contains(&"pages"), "{labels:?}");
        assert!(labels.contains(&"webtoon"), "{labels:?}");
    }

    /// The legacy `bubble_type` modes pin both display types; the result is always `hybrid`.
    #[test]
    fn legacy_bubble_modes_pin_both_display_types() {
        let mut settings = non_default_settings();
        settings.bubble_type = "aside".to_string();
        let aside = shared_canvas_settings_from_project(&settings);
        assert_eq!((aside.bubble_type.as_str(), aside.editable_bubble_type.as_str(), aside.readonly_bubble_type.as_str()), ("hybrid", "aside", "aside"));
        settings.bubble_type = "on_top".to_string();
        let on_top = shared_canvas_settings_from_project(&settings);
        assert_eq!((on_top.editable_bubble_type.as_str(), on_top.readonly_bubble_type.as_str()), ("on_top", "on_top"));
    }

    /// `canvas_snapshot` of a seeded view equals the free mapping of its state, and the seeded
    /// state reaches the shared settings unchanged.
    #[test]
    fn canvas_snapshot_is_unchanged_on_a_seeded_view() {
        let settings = non_default_settings();
        let mut canvas = CanvasView::default();
        canvas.state = canvas_state_from_project(&settings);
        assert_eq!(canvas.canvas_snapshot(), shared_settings_from_state(&canvas.state));
        assert_eq!(canvas.canvas_snapshot(), legacy_seed_oracle(&settings));
    }

    /// The preset strings survive the enum round trip, so a preset written through the shared
    /// settings and one written through a canvas agree.
    #[test]
    fn comic_preset_strings_round_trip_through_the_enum() {
        for preset in ["moderate", "none"] {
            assert_eq!(AsideBubbleCompactMode::from_str(preset).as_str(), preset);
        }
    }

    /// View-only state keeps its defaults: the conversion seeds persisted fields only.
    #[test]
    fn canvas_state_from_project_keeps_view_defaults() {
        let state = canvas_state_from_project(&non_default_settings());
        assert!((state.zoom - CanvasState::default().zoom).abs() < f32::EPSILON);
        assert_eq!(state.bubble_mode, BubbleMode::Hybrid);
    }

    /// The canvas writer must keep keys owned by other `settings.json` writers.
    #[test]
    fn project_save_keeps_foreign_keys() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let path = dir.path().join("settings.json");
        let seed = r#"{"OCR":{"engine":"paddle"},"canvas":{"project_custom_spellcheck_words":"a\nb","scale_bubbles":true}}"#;
        std::fs::write(&path, seed).map_err(|err| err.to_string())?;
        save_canvas_settings_to_project_file(&path, &SharedCanvasSettings::default())?;
        let raw = std::fs::read_to_string(&path).map_err(|err| err.to_string())?;
        let root: Value = serde_json::from_str(&raw).map_err(|err| err.to_string())?;
        assert_eq!(root["OCR"]["engine"], "paddle");
        assert_eq!(root["canvas"]["project_custom_spellcheck_words"], "a\nb");
        assert_eq!(root["canvas"]["bubble_type"], "hybrid");
        assert_eq!(root["canvas"]["scale_bubbles"], SharedCanvasSettings::default().scale_bubbles);
        assert_eq!(root["canvas"]["layout_per_title"], true);
        assert_eq!(root["bubble_type"], "hybrid");
        assert!(!raw.ends_with('\n'));
        Ok(())
    }

    /// Sorted keys of the JSON object at `pointer` in `root` (empty when absent).
    fn object_keys(root: &Value, pointer: &str) -> Vec<String> {
        let mut keys: Vec<String> = root.pointer(pointer).and_then(Value::as_object).map(|obj| obj.keys().cloned().collect()).unwrap_or_default();
        keys.sort();
        keys
    }

    /// The project writer's key set: the exact `canvas.*` and root keys it leaves in a
    /// `settings.json` that held every key it removes, plus one foreign key each. The canvas set is
    /// the whole layout (`CanvasLayout::JSON_KEYS`), the per-title marker and the other fields.
    #[test]
    fn project_writer_key_set() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let path = dir.path().join("settings.json");
        let removed_canvas = ["load_all_bubbles", "visible_page_radius", "bubble_load_delay_ms", "copy_from_field", "paste_into_field"];
        let removed_root = ["copy_from_field", "paste_into_field", "visible_page_radius", "bubble_load_delay_ms"];
        let mut canvas = Map::new();
        for key in removed_canvas.iter().chain(["foreign_canvas"].iter()) {
            canvas.insert((*key).to_string(), Value::from(1));
        }
        let mut root = Map::new();
        for key in removed_root.iter().chain(["foreign_root"].iter()) {
            root.insert((*key).to_string(), Value::from(1));
        }
        root.insert("canvas".to_string(), Value::Object(canvas));
        std::fs::write(&path, Value::Object(root).to_string()).map_err(|err| err.to_string())?;
        save_canvas_settings_to_project_file(&path, &SharedCanvasSettings::default())?;
        let written: Value = serde_json::from_str(&std::fs::read_to_string(&path).map_err(|err| err.to_string())?).map_err(|err| err.to_string())?;
        let mut expected_canvas: Vec<String> = ["layout_per_title", "bubble_type", "show_bubbles", "show_bubble_status", "bubble_opacity", "side_margin_px", "tabs_autosync_enabled", "auto_insert_last_character", "spellcheck_original", "spellcheck_translation", "cache_pages", "translation_status_display", "hint_show_outside_default", "foreign_canvas"].iter().chain(CanvasLayout::JSON_KEYS.iter()).map(|key| (*key).to_string()).collect();
        expected_canvas.sort();
        assert_eq!(object_keys(&written, "/canvas"), expected_canvas);
        let mut expected_root: Vec<String> = ["canvas", "bubble_type", "editable_bubble_type", "readonly_bubble_type", "on_top_focus_mode", "foreign_root"].iter().map(|key| (*key).to_string()).collect();
        expected_root.sort();
        assert_eq!(object_keys(&written, ""), expected_root);
        Ok(())
    }

    /// The user writer's key set: only the cross-title preferences, with a foreign key in that
    /// section, a legacy layout key and a foreign section kept as found.
    #[test]
    fn user_writer_key_set() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let path = dir.path().join("user_config.json");
        std::fs::write(&path, r#"{"General":{"x":1},"Canvas":{"foreign_canvas":1,"aside_min_width_px":600}}"#).map_err(|err| err.to_string())?;
        save_canvas_settings_to_user_file(&path, &SharedCanvasSettings::default())?;
        let written: Value = serde_json::from_str(&std::fs::read_to_string(&path).map_err(|err| err.to_string())?).map_err(|err| err.to_string())?;
        let mut expected: Vec<String> = ["bubble_status_rules", "cache_pages", "translation_status_display", "hint_show_outside_default", "foreign_canvas", "aside_min_width_px"].iter().map(|key| (*key).to_string()).collect();
        expected.sort();
        assert_eq!(object_keys(&written, "/Canvas"), expected);
        assert_eq!(written["Canvas"]["aside_min_width_px"], 600, "a legacy global layout value is left as found");
        assert_eq!(written["General"]["x"], 1);
        Ok(())
    }

    /// What the project writer stores is read back as the same layout by the title reader (the
    /// per-title marker makes the reader ignore the user config).
    #[test]
    fn project_writer_round_trips_the_layout() {
        let mut snapshot = SharedCanvasSettings::default();
        snapshot.apply_layout(&ms_project::BuiltinLayoutPreset::Webtoon.factory());
        snapshot.bubble_min_width = 333.0;
        let mut root = Map::new();
        apply_project_canvas_snapshot(&mut root, &snapshot);
        let Some(canvas) = root.get("canvas").and_then(Value::as_object) else {
            panic!("the writer stores a canvas object");
        };
        assert_eq!(CanvasLayout::from_json_object(canvas, &ms_project::BASE_BUILTIN.factory()), snapshot.layout());
    }

    /// A malformed or non-object `settings.json` is reported and left byte-identical.
    #[test]
    fn project_save_never_overwrites_malformed_file() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|err| err.to_string())?;
        let path = dir.path().join("settings.json");
        for seed in ["{ not json", "[1, 2]"] {
            std::fs::write(&path, seed).map_err(|err| err.to_string())?;
            assert!(save_canvas_settings_to_project_file(&path, &SharedCanvasSettings::default()).is_err());
            assert_eq!(std::fs::read_to_string(&path).map_err(|err| err.to_string())?, seed);
        }
        Ok(())
    }
}
