/*
File: src/canvas/settings.rs

Purpose:
Canvas settings runtime and persistence helpers for `CanvasView`.

Main responsibilities:
- own `CanvasSettingsRuntime`;
- apply/publish shared canvas setting snapshots;
- sync `cache_pages` with `CleanOverlaysModel`;
- serialize project/user canvas settings without blocking the GUI thread.

Key structures:
- CanvasSettingsRuntime

Key functions:
- CanvasView::apply_canvas_snapshot()
- CanvasView::publish_canvas_settings()
- save_canvas_settings_to_project_file()
- save_canvas_settings_to_user_file()

Notes:
- Actual filesystem writes are executed by the background saver worker from `workers.rs`.
- This module keeps settings-specific logic out of the main canvas facade.
*/

use super::types::CanvasSettingsSaveRequest;
use super::workers::spawn_canvas_settings_saver_thread;
use super::{
    AsideBubbleCompactMode, AsideBubbleSideMode, BubbleMode, BubbleType, CanvasView, OnTopFocusMode,
};
use crate::config;
use crate::models::bubbles_model::SharedCanvasSettings;
use crate::project::ProjectData;
use crate::runtime_log;
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

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
        self.state.on_top_focus_mode = OnTopFocusMode::from_str(&snapshot.on_top_focus_mode);
        self.state.scale_bubbles = snapshot.scale_bubbles;
        self.state.aside_scale_pct = snapshot.aside_scale_pct.clamp(25, 300);
        self.state.auto_insert_last_character = snapshot.auto_insert_last_character;
        self.state.spellcheck_original = snapshot.spellcheck_original;
        self.state.spellcheck_translation = snapshot.spellcheck_translation;
        self.state.tabs_autosync_enabled = snapshot.tabs_autosync_enabled;
        self.state.cache_pages = snapshot.cache_pages;
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

    fn canvas_snapshot(&self) -> SharedCanvasSettings {
        SharedCanvasSettings {
            bubble_type: BubbleMode::Hybrid.as_str().to_string(),
            editable_bubble_type: self.state.hybrid_editable_bubble_type.as_str().to_string(),
            readonly_bubble_type: self.state.hybrid_readonly_bubble_type.as_str().to_string(),
            show_bubbles: self.state.show_bubbles,
            show_bubble_status: self.state.show_bubble_status,
            bubble_status_rules: self.state.bubble_status_rules.clone(),
            bubble_opacity: self.state.bubble_opacity,
            page_spacing: self.state.page_spacing,
            separate_pages: self.state.separate_pages,
            edge_margin: self.state.edge_margin,
            side_margin: self.state.side_margin,
            bubble_min_width: self.state.bubble_min_width,
            bubble_max_width: self.state.bubble_max_width,
            aside_compact_mode: self.state.aside_compact_mode.as_str().to_string(),
            aside_side_mode: self.state.aside_side_mode.as_str().to_string(),
            on_top_focus_mode: self.state.on_top_focus_mode.as_str().to_string(),
            scale_bubbles: self.state.scale_bubbles,
            aside_scale_pct: self.state.aside_scale_pct,
            auto_insert_last_character: self.state.auto_insert_last_character,
            spellcheck_original: self.state.spellcheck_original,
            spellcheck_translation: self.state.spellcheck_translation,
            tabs_autosync_enabled: self.state.tabs_autosync_enabled,
            cache_pages: self.state.cache_pages,
        }
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

fn load_json_object_root(path: &Path, scope: &str) -> Result<Value, String> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }

    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {scope} '{}': {err}", path.display()))?;
    let root = serde_json::from_str::<Value>(&raw)
        .map_err(|err| format!("failed to parse {scope} '{}': {err}", path.display()))?;
    if root.is_object() {
        Ok(root)
    } else {
        Err(format!(
            "failed to parse {scope} '{}': root JSON value is not an object",
            path.display()
        ))
    }
}

pub(crate) fn save_canvas_settings_to_project_file(
    settings_file: &Path,
    snapshot: &SharedCanvasSettings,
) -> Result<(), String> {
    let mut root = load_json_object_root(settings_file, "project canvas settings file")?;
    let Some(root_obj) = root.as_object_mut() else {
        return Err(format!(
            "project canvas settings root became non-object unexpectedly: '{}'",
            settings_file.display()
        ));
    };

    let mut canvas_obj = root_obj
        .get("canvas")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    canvas_obj.insert(
        "bubble_type".to_string(),
        Value::String(snapshot.bubble_type.clone()),
    );
    canvas_obj.insert(
        "editable_bubble_type".to_string(),
        Value::String(snapshot.editable_bubble_type.clone()),
    );
    canvas_obj.insert(
        "readonly_bubble_type".to_string(),
        Value::String(snapshot.readonly_bubble_type.clone()),
    );
    canvas_obj.insert(
        "on_top_focus_mode".to_string(),
        Value::String(snapshot.on_top_focus_mode.clone()),
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
        "page_spacing_px".to_string(),
        Value::from(snapshot.page_spacing.max(0.0).round() as i64),
    );
    canvas_obj.insert(
        "separate_pages".to_string(),
        Value::Bool(snapshot.separate_pages),
    );
    canvas_obj.insert(
        "vertical_edge_margin_px".to_string(),
        Value::from(snapshot.edge_margin.max(0.0).round() as i64),
    );
    canvas_obj.insert(
        "side_margin_px".to_string(),
        Value::from(snapshot.side_margin.max(0.0).round() as i64),
    );
    canvas_obj.remove("aside_min_width_px");
    canvas_obj.remove("aside_max_width_px");
    canvas_obj.remove("scale_bubbles");
    canvas_obj.insert(
        "aside_scale_pct".to_string(),
        Value::from(snapshot.aside_scale_pct.clamp(25, 300)),
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
    canvas_obj.remove("copy_from_field");
    canvas_obj.remove("paste_into_field");
    root_obj.insert("canvas".to_string(), Value::Object(canvas_obj));

    root_obj.insert(
        "bubble_type".to_string(),
        Value::String(snapshot.bubble_type.clone()),
    );
    root_obj.insert(
        "editable_bubble_type".to_string(),
        Value::String(snapshot.editable_bubble_type.clone()),
    );
    root_obj.insert(
        "readonly_bubble_type".to_string(),
        Value::String(snapshot.readonly_bubble_type.clone()),
    );
    root_obj.insert(
        "on_top_focus_mode".to_string(),
        Value::String(snapshot.on_top_focus_mode.clone()),
    );
    root_obj.remove("copy_from_field");
    root_obj.remove("paste_into_field");
    root_obj.remove("visible_page_radius");
    root_obj.remove("bubble_load_delay_ms");

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(settings_file, payload).map_err(|err| err.to_string())?;
    Ok(())
}

pub(crate) fn save_canvas_settings_to_user_file(
    user_settings_file: &Path,
    snapshot: &SharedCanvasSettings,
) -> Result<(), String> {
    let mut root = load_json_object_root(user_settings_file, "user canvas settings file")?;
    let Some(root_obj) = root.as_object_mut() else {
        return Err(format!(
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
        crate::bubble_status::bubble_status_rules_to_value(&snapshot.bubble_status_rules),
    );
    canvas_obj.insert(
        "aside_compact_mode".to_string(),
        Value::String(snapshot.aside_compact_mode.clone()),
    );
    canvas_obj.insert(
        "aside_side_mode".to_string(),
        Value::String(snapshot.aside_side_mode.clone()),
    );
    canvas_obj.insert(
        "scale_bubbles".to_string(),
        Value::Bool(snapshot.scale_bubbles),
    );
    canvas_obj.insert(
        "aside_min_width_px".to_string(),
        Value::from(snapshot.bubble_min_width.max(40.0).round() as i64),
    );
    canvas_obj.insert(
        "aside_max_width_px".to_string(),
        Value::from(snapshot.bubble_max_width.max(40.0).round() as i64),
    );
    canvas_obj.insert(
        "aside_compact_mode".to_string(),
        Value::String(snapshot.aside_compact_mode.clone()),
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
    root_obj.insert("Canvas".to_string(), Value::Object(canvas_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, payload).map_err(|err| err.to_string())?;
    Ok(())
}
