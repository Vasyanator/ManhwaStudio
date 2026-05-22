/*
FILE OVERVIEW: src/models/bubbles_model.rs
Shared runtime model for translation bubbles and canvas settings snapshots.

Main items:
- `SharedCanvasSettings`: lightweight cross-tab canvas settings payload, including
  editable/readonly default display types for `default` bubbles.
- `BubblesModel`: thread-safe mutable bubbles store with revisions and async saver.
- `runtime_bubble_to_record`: adapter from canvas runtime fields to persisted `Bubble`,
  including per-bubble `bubble_type`.

Threading/persistence:
- Bubble writes are coalesced through `spawn_bubbles_saver_thread` so GUI thread only
  publishes snapshots and does not block on filesystem I/O.
- The saver always writes to the unsaved staging folder (`unsaved_bubbles_path`).
  The main chapter file is only updated by an explicit "save to project" merge.
- `has_unsaved_changes()` returns true when the unsaved staging file exists on disk.
*/

use crate::bubble_status::{BubbleStatusRule, default_bubble_status_rules};
use crate::project::Bubble;
use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Debug, Clone, PartialEq)]
pub struct SharedCanvasSettings {
    pub bubble_type: String,
    pub editable_bubble_type: String,
    pub readonly_bubble_type: String,
    pub show_bubbles: bool,
    pub show_bubble_status: bool,
    pub bubble_status_rules: Vec<BubbleStatusRule>,
    pub bubble_opacity: f32,
    pub page_spacing: f32,
    pub separate_pages: bool,
    pub edge_margin: f32,
    pub side_margin: f32,
    pub bubble_min_width: f32,
    pub bubble_max_width: f32,
    pub aside_compact_mode: String,
    pub aside_side_mode: String,
    pub on_top_focus_mode: String,
    pub scale_bubbles: bool,
    pub aside_scale_pct: i32,
    pub auto_insert_last_character: bool,
    pub spellcheck_original: bool,
    pub spellcheck_translation: bool,
    pub tabs_autosync_enabled: bool,
    pub cache_pages: bool,
}

impl Default for SharedCanvasSettings {
    fn default() -> Self {
        Self {
            bubble_type: "hybrid".to_string(),
            editable_bubble_type: "on_top".to_string(),
            readonly_bubble_type: "aside".to_string(),
            show_bubbles: true,
            show_bubble_status: false,
            bubble_status_rules: default_bubble_status_rules(),
            bubble_opacity: 1.0,
            page_spacing: 200.0,
            separate_pages: true,
            edge_margin: 200.0,
            side_margin: 20.0,
            bubble_min_width: 500.0,
            bubble_max_width: 550.0,
            aside_compact_mode: "none".to_string(),
            aside_side_mode: "auto".to_string(),
            on_top_focus_mode: "around".to_string(),
            scale_bubbles: true,
            aside_scale_pct: 100,
            auto_insert_last_character: true,
            spellcheck_original: false,
            spellcheck_translation: true,
            tabs_autosync_enabled: true,
            cache_pages: true,
        }
    }
}

#[derive(Debug)]
pub struct BubblesModel {
    bubbles: Arc<Vec<Bubble>>,
    bubble_index_by_id: HashMap<i64, usize>,
    bubbles_path: PathBuf,
    unsaved_bubbles_path: PathBuf,
    has_unsaved_changes: bool,
    revision: u64,
    canvas_settings: SharedCanvasSettings,
    canvas_revision: u64,
    saver_tx: Arc<Mutex<Sender<Vec<Bubble>>>>,
}

#[derive(Clone)]
pub struct BubblesSaveTask {
    snapshot: Arc<Vec<Bubble>>,
    saver_tx: Arc<Mutex<Sender<Vec<Bubble>>>>,
    unsaved_bubbles_path: PathBuf,
    bubbles_path: PathBuf,
}

impl BubblesSaveTask {
    pub fn persist(&self) {
        send_snapshot_to_bubbles_saver(
            &self.saver_tx,
            &self.unsaved_bubbles_path,
            &self.bubbles_path,
            &self.snapshot,
        );
    }
}

#[allow(dead_code)]
impl BubblesModel {
    pub fn new(
        bubbles: Vec<Bubble>,
        bubbles_path: PathBuf,
        unsaved_bubbles_path: PathBuf,
        canvas_settings: SharedCanvasSettings,
    ) -> Self {
        let saver_tx = Arc::new(Mutex::new(spawn_bubbles_saver_thread(
            unsaved_bubbles_path.clone(),
        )));
        let bubble_index_by_id = build_bubble_index(&bubbles);
        Self {
            bubbles: Arc::new(bubbles),
            bubble_index_by_id,
            bubbles_path,
            has_unsaved_changes: unsaved_bubbles_path.exists(),
            unsaved_bubbles_path,
            revision: 1,
            canvas_settings,
            canvas_revision: 1,
            saver_tx,
        }
    }

    /// Returns true when the unsaved staging file exists on disk, meaning there
    /// are in-session mutations that have not been merged into the project yet.
    pub fn has_unsaved_changes(&self) -> bool {
        self.has_unsaved_changes
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn snapshot(&self) -> Vec<Bubble> {
        self.bubbles.as_ref().clone()
    }

    pub fn snapshot_shared(&self) -> Arc<Vec<Bubble>> {
        Arc::clone(&self.bubbles)
    }

    pub fn canvas_revision(&self) -> u64 {
        self.canvas_revision
    }

    pub fn canvas_snapshot(&self) -> SharedCanvasSettings {
        self.canvas_settings.clone()
    }

    pub fn set_canvas_settings(&mut self, settings: SharedCanvasSettings) {
        if self.canvas_settings == settings {
            return;
        }
        self.canvas_settings = settings;
        self.canvas_revision = self.canvas_revision.saturating_add(1);
    }

    pub fn create_or_replace(&mut self, rec: Bubble) -> Result<()> {
        let bid = rec.id;
        if let Some(index) = self.bubble_index_by_id.get(&bid).copied() {
            if let Some(existing) = Arc::make_mut(&mut self.bubbles).get_mut(index) {
                *existing = rec;
            }
        } else {
            let bubbles = Arc::make_mut(&mut self.bubbles);
            bubbles.push(rec);
            self.bubble_index_by_id.insert(bid, bubbles.len() - 1);
        }
        self.touch_and_save()
    }

    // Parameters represent distinct required inputs with no natural grouping.
    #[allow(clippy::too_many_arguments)]
    pub fn update_patch(
        &mut self,
        bid: i64,
        text: Option<String>,
        original_text: Option<String>,
        img_idx: Option<usize>,
        img_u: Option<f32>,
        img_v: Option<f32>,
        side: Option<Option<String>>,
    ) -> Result<()> {
        let Some(index) = self.bubble_index_by_id.get(&bid).copied() else {
            return Ok(());
        };
        let Some(existing) = Arc::make_mut(&mut self.bubbles).get_mut(index) else {
            return Ok(());
        };
        if let Some(v) = text {
            existing.text = v;
        }
        if let Some(v) = original_text {
            existing.original_text = v;
        }
        if let Some(v) = img_idx {
            existing.img_idx = v;
        }
        if let Some(v) = img_u {
            existing.img_u = v;
        }
        if let Some(v) = img_v {
            existing.img_v = v;
        }
        if let Some(v) = side {
            existing.side = v;
        }
        self.touch_and_save()
    }

    pub fn update_translation_result(
        &mut self,
        bid: i64,
        translated_text: String,
        translation_status: &str,
    ) -> Result<()> {
        let Some(index) = self.bubble_index_by_id.get(&bid).copied() else {
            return Ok(());
        };
        let Some(existing) = Arc::make_mut(&mut self.bubbles).get_mut(index) else {
            return Ok(());
        };
        existing.text = translated_text;
        existing.extra.insert(
            "translation_status".to_string(),
            Value::String(translation_status.to_string()),
        );
        self.touch_and_save()
    }

    pub fn update_translation_result_deferred_save(
        &mut self,
        bid: i64,
        translated_text: String,
        translation_status: &str,
    ) -> Result<Option<(u64, BubblesSaveTask)>> {
        let Some(index) = self.bubble_index_by_id.get(&bid).copied() else {
            return Ok(None);
        };
        let Some(existing) = Arc::make_mut(&mut self.bubbles).get_mut(index) else {
            return Ok(None);
        };
        existing.text = translated_text;
        existing.extra.insert(
            "translation_status".to_string(),
            Value::String(translation_status.to_string()),
        );
        let task = self.touch_and_prepare_save_task();
        Ok(Some((self.revision, task)))
    }

    pub fn delete(&mut self, bid: i64) -> Result<()> {
        let Some(index) = self.bubble_index_by_id.remove(&bid) else {
            return Ok(());
        };
        let bubbles = Arc::make_mut(&mut self.bubbles);
        bubbles.remove(index);
        rebuild_bubble_index(&mut self.bubble_index_by_id, bubbles);
        self.touch_and_save()?;
        Ok(())
    }

    pub fn unplace(&mut self, bid: i64) -> Result<()> {
        let Some(index) = self.bubble_index_by_id.get(&bid).copied() else {
            return Ok(());
        };
        let Some(existing) = Arc::make_mut(&mut self.bubbles).get_mut(index) else {
            return Ok(());
        };
        existing.img_idx = usize::MAX;
        existing.img_u = 0.0;
        existing.img_v = 0.0;
        existing.side = None;
        self.touch_and_save()
    }

    pub fn reset(&mut self, records: Vec<Bubble>) -> Result<()> {
        self.bubble_index_by_id = build_bubble_index(&records);
        self.bubbles = Arc::new(records);
        self.touch_and_save()
    }

    pub fn mark_saved_to_project(&mut self) {
        self.has_unsaved_changes = false;
    }

    fn touch_and_save(&mut self) -> Result<()> {
        let task = self.touch_and_prepare_save_task();
        task.persist();
        Ok(())
    }

    fn touch_and_prepare_save_task(&mut self) -> BubblesSaveTask {
        self.revision = self.revision.saturating_add(1);
        self.has_unsaved_changes = true;
        BubblesSaveTask {
            snapshot: Arc::clone(&self.bubbles),
            saver_tx: Arc::clone(&self.saver_tx),
            unsaved_bubbles_path: self.unsaved_bubbles_path.clone(),
            bubbles_path: self.bubbles_path.clone(),
        }
    }
}

fn spawn_bubbles_saver_thread(bubbles_path: PathBuf) -> Sender<Vec<Bubble>> {
    let (tx, rx) = mpsc::channel::<Vec<Bubble>>();
    thread::spawn(move || {
        while let Ok(first) = rx.recv() {
            let mut latest = first;
            while let Ok(next) = rx.try_recv() {
                latest = next;
            }
            if let Err(err) = write_bubbles_file(&bubbles_path, &latest) {
                eprintln!(
                    "failed to persist bubbles {}: {err:#}",
                    bubbles_path.display()
                );
            }
        }
    });
    tx
}

fn write_bubbles_file(path: &Path, bubbles: &[Bubble]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(bubbles).context("failed to serialize bubbles")?;
    fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

// Parameters represent distinct required inputs with no natural grouping.
#[allow(clippy::too_many_arguments)]
pub fn runtime_bubble_to_record(
    id: i64,
    img_idx: usize,
    img_u: f32,
    img_v: f32,
    side: Option<String>,
    bubble_type: Option<String>,
    text: String,
    original_text: String,
    extra: Option<Map<String, Value>>,
) -> Bubble {
    Bubble {
        id,
        img_idx,
        img_u,
        img_v,
        side,
        bubble_type,
        text,
        original_text,
        extra: extra.unwrap_or_default(),
    }
}

fn build_bubble_index(bubbles: &[Bubble]) -> HashMap<i64, usize> {
    bubbles
        .iter()
        .enumerate()
        .map(|(index, bubble)| (bubble.id, index))
        .collect()
}

fn rebuild_bubble_index(index_by_id: &mut HashMap<i64, usize>, bubbles: &[Bubble]) {
    *index_by_id = build_bubble_index(bubbles);
}

fn send_snapshot_to_bubbles_saver(
    saver_tx: &Arc<Mutex<Sender<Vec<Bubble>>>>,
    unsaved_bubbles_path: &Path,
    bubbles_path: &Path,
    snapshot: &Arc<Vec<Bubble>>,
) {
    let sender = match saver_tx.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    if sender.send(snapshot.as_ref().clone()).is_ok() {
        return;
    }

    eprintln!(
        "WARN bubbles saver thread gone, respawning: {}",
        unsaved_bubbles_path.display()
    );
    let new_sender = spawn_bubbles_saver_thread(unsaved_bubbles_path.to_path_buf());
    match saver_tx.lock() {
        Ok(mut guard) => *guard = new_sender.clone(),
        Err(poisoned) => *poisoned.into_inner() = new_sender.clone(),
    }
    if new_sender.send(snapshot.as_ref().clone()).is_err() {
        eprintln!(
            "ERROR failed to send to newly spawned bubbles saver thread: {}",
            bubbles_path.display()
        );
    }
}
