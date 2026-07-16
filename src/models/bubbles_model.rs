/*
FILE OVERVIEW: src/models/bubbles_model.rs
Shared runtime model for translation bubbles and canvas settings snapshots.

Main items:
- `SharedCanvasSettings`: lightweight cross-tab canvas settings payload, including
  editable/readonly default display types for `default` bubbles.
- `BubblesModel`: thread-safe mutable bubbles store with revisions, async saver, and
  flush/hold barriers for destructive staging operations.
- `runtime_bubble_to_record`: adapter from canvas runtime fields to persisted `Bubble`,
  including per-bubble `bubble_class` and text display `bubble_type`.

Threading/persistence:
- Bubble writes are coalesced through `spawn_bubbles_saver_thread` so GUI thread only
  publishes snapshots and does not block on filesystem I/O. The saver channel carries
  shared `Arc<Vec<Bubble>>` snapshots, so publishing a save never deep-clones the list.
- `with_bubble`/`extra_of` let callers read a single bubble (or its `extra` map) by id
  without cloning the whole list via `snapshot()`.
- The saver always writes to the unsaved staging folder (`unsaved_bubbles_path`).
  The main chapter file is only updated by an explicit "save to project" merge.
- `has_unsaved_changes()` returns true when the unsaved staging file exists on disk.
*/

use crate::bubble_status::{BubbleStatusRule, default_bubble_status_rules};
use crate::project::Bubble;
use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use ms_thread::{self as thread, JoinHandle};

/// Commands accepted by the bubbles saver worker.
#[derive(Debug)]
enum BubblesSaverMessage {
    Snapshot(Arc<Vec<Bubble>>),
    BarrierAndHold(Sender<Result<(), String>>),
    Resume,
    Shutdown(Sender<()>),
}

/// Cloneable worker endpoint used by non-GUI persistence workers.
#[derive(Debug, Clone)]
pub struct BubblesSaverHandle {
    sender: Sender<BubblesSaverMessage>,
}

/// Keeps post-barrier snapshots queued until a destructive staging operation finishes.
#[derive(Debug)]
pub struct BubblesSaverBarrierGuard {
    sender: Sender<BubblesSaverMessage>,
}

impl Drop for BubblesSaverBarrierGuard {
    fn drop(&mut self) {
        if self.sender.send(BubblesSaverMessage::Resume).is_err() {
            eprintln!("ERROR bubbles saver stopped before a held barrier could resume");
        }
    }
}

impl BubblesSaverHandle {
    /// Persists all snapshots enqueued before this call and holds later snapshots in memory.
    ///
    /// The returned guard resumes normal writes when dropped. `Err` means the barrier could not be
    /// verified; callers must not perform a destructive staging operation in that case.
    pub fn barrier_and_hold_blocking(&self) -> Result<BubblesSaverBarrierGuard, String> {
        let (ack_tx, ack_rx) = mpsc::channel();
        self.sender
            .send(BubblesSaverMessage::BarrierAndHold(ack_tx))
            .map_err(|err| format!("failed to enqueue bubbles saver barrier: {err}"))?;
        let outcome = ack_rx
            .recv()
            .map_err(|err| format!("bubbles saver stopped before barrier acknowledgement: {err}"))?;
        if let Err(err) = outcome {
            if self.sender.send(BubblesSaverMessage::Resume).is_err() {
                return Err(format!("{err}; saver also stopped before it could resume"));
            }
            return Err(err);
        }
        Ok(BubblesSaverBarrierGuard {
            sender: self.sender.clone(),
        })
    }
}

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
    pub aside_second_column: bool,
    pub on_top_focus_mode: String,
    pub scale_bubbles: bool,
    pub aside_scale_pct: i32,
    pub auto_insert_last_character: bool,
    pub spellcheck_original: bool,
    pub spellcheck_translation: bool,
    pub tabs_autosync_enabled: bool,
    pub cache_pages: bool,
    pub translation_status_display: String,
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
            aside_second_column: false,
            on_top_focus_mode: "around".to_string(),
            scale_bubbles: true,
            aside_scale_pct: 100,
            auto_insert_last_character: true,
            spellcheck_original: false,
            spellcheck_translation: true,
            tabs_autosync_enabled: true,
            cache_pages: true,
            translation_status_display: "until_next".to_string(),
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
    saver_tx: Arc<Mutex<Sender<BubblesSaverMessage>>>,
    saver_thread: Option<JoinHandle<()>>,
    /// Serializes saver writes with a structural page operation's final synchronous snapshot.
    saver_gate: Arc<Mutex<BubblesSaverGate>>,
}

#[derive(Clone)]
pub struct BubblesSaveTask {
    snapshot: Arc<Vec<Bubble>>,
    saver_tx: Arc<Mutex<Sender<BubblesSaverMessage>>>,
    saver_gate: Arc<Mutex<BubblesSaverGate>>,
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
            &self.saver_gate,
        );
    }
}

/// State shared by the coalescing saver and a structural-operation quiescence barrier.
#[derive(Debug, Default)]
struct BubblesSaverGate {
    paused: bool,
}

#[allow(dead_code)]
impl BubblesModel {
    pub fn new(
        bubbles: Vec<Bubble>,
        bubbles_path: PathBuf,
        unsaved_bubbles_path: PathBuf,
        canvas_settings: SharedCanvasSettings,
    ) -> Self {
        let saver_gate = Arc::new(Mutex::new(BubblesSaverGate::default()));
        let (saver_sender, saver_thread) = spawn_bubbles_saver_thread(
            unsaved_bubbles_path.clone(),
            Arc::clone(&saver_gate),
        );
        let saver_tx = Arc::new(Mutex::new(saver_sender));
        let bubble_index_by_id = build_bubble_index(&bubbles);
        // Query the storage seam for the staging file so the web build checks its
        // in-memory/IndexedDB store instead of the desktop filesystem.
        let has_unsaved_changes = {
            let unsaved_str = unsaved_bubbles_path.to_string_lossy();
            crate::storage::storage().exists(unsaved_str.as_ref())
        };
        Self {
            bubbles: Arc::new(bubbles),
            bubble_index_by_id,
            bubbles_path,
            has_unsaved_changes,
            unsaved_bubbles_path,
            revision: 1,
            canvas_settings,
            canvas_revision: 1,
            saver_tx,
            saver_thread: Some(saver_thread),
            saver_gate,
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

    /// Returns a cheap clone of the saver endpoint for use by a background merge worker.
    #[must_use]
    pub fn saver_handle(&self) -> BubblesSaverHandle {
        let sender = match self.saver_tx.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        BubblesSaverHandle { sender }
    }

    /// Flushes all queued snapshots, waits for every active hold, then drains and joins its worker.
    ///
    /// This is a blocking teardown operation and must only be called from an exit path or worker.
    pub fn shutdown_saver(&mut self) -> Result<(), String> {
        let handle = self.saver_handle();
        let barrier_error = match handle.barrier_and_hold_blocking() {
            Ok(barrier) => {
                drop(barrier);
                None
            }
            Err(err) => Some(err),
        };
        let (ack_tx, ack_rx) = mpsc::channel();
        handle
            .sender
            .send(BubblesSaverMessage::Shutdown(ack_tx))
            .map_err(|err| format!("failed to request bubbles saver shutdown: {err}"))?;
        ack_rx
            .recv()
            .map_err(|err| format!("bubbles saver stopped before shutdown acknowledgement: {err}"))?;
        if let Some(join_handle) = self.saver_thread.take() {
            join_handle
                .join()
                .map_err(|_| "bubbles saver thread panicked during shutdown".to_string())?;
        }
        barrier_error.map_or(Ok(()), Err)
    }

    /// Stops future asynchronous writes after any in-progress write has left the critical section.
    ///
    /// Callers use this immediately before a structural page transaction and must reload the
    /// model afterwards. The returned snapshot is therefore safe to write synchronously without
    /// a stale coalesced saver overwriting remapped page indices.
    pub fn pause_saver_for_page_op(&mut self) -> Arc<Vec<Bubble>> {
        match self.saver_gate.lock() {
            Ok(mut gate) => gate.paused = true,
            Err(poisoned) => poisoned.into_inner().paused = true,
        }
        self.snapshot_shared()
    }

    /// Re-enables persistence after a discard cleanup failed and republishes current state.
    ///
    /// The failed cleanup returns control to the editor, so the otherwise permanent discard pause
    /// must be reversed. Republishing the latest full snapshot also covers edits accepted while the
    /// cleanup worker was running and snapshot publication was suppressed.
    pub fn resume_saver_after_failed_discard(&mut self) {
        match self.saver_gate.lock() {
            Ok(mut gate) => gate.paused = false,
            Err(poisoned) => poisoned.into_inner().paused = false,
        }
        self.has_unsaved_changes = true;
        send_snapshot_to_bubbles_saver(
            &self.saver_tx,
            &self.unsaved_bubbles_path,
            &self.bubbles_path,
            &self.bubbles,
            &self.saver_gate,
        );
    }

    /// Runs `f` against the bubble with id `bid` without cloning the whole list.
    ///
    /// Uses the maintained `bubble_index_by_id`, so it is O(1) lookup. Returns `None`
    /// when no bubble has that id; otherwise returns `Some(f(&bubble))`. The borrow is
    /// confined to `f`, so callers must copy out whatever they need from the bubble.
    #[must_use]
    pub fn with_bubble<R>(&self, bid: i64, f: impl FnOnce(&Bubble) -> R) -> Option<R> {
        let index = self.bubble_index_by_id.get(&bid).copied()?;
        self.bubbles.get(index).map(f)
    }

    /// Borrows the `extra` map of the bubble with id `bid` without cloning the list.
    ///
    /// Returns `None` when no bubble has that id. Intended for callers that previously
    /// took a full `snapshot()` only to read one bubble's `extra`.
    #[must_use]
    pub fn extra_of(&self, bid: i64) -> Option<&Map<String, Value>> {
        let index = self.bubble_index_by_id.get(&bid).copied()?;
        self.bubbles.get(index).map(|bubble| &bubble.extra)
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

    pub fn update_translation_and_original_deferred_save(
        &mut self,
        bid: i64,
        original_text: String,
        translated_text: String,
        translation_status: &str,
    ) -> Result<Option<(u64, BubblesSaveTask)>> {
        let Some(index) = self.bubble_index_by_id.get(&bid).copied() else {
            return Ok(None);
        };
        let Some(existing) = Arc::make_mut(&mut self.bubbles).get_mut(index) else {
            return Ok(None);
        };
        existing.original_text = original_text;
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
        // A mutation accepted while the save barrier was held is written to a newly recreated
        // staging file after the merge. Preserve that post-save dirty state instead of clearing it.
        let unsaved_path = self.unsaved_bubbles_path.to_string_lossy();
        self.has_unsaved_changes = crate::storage::storage().exists(unsaved_path.as_ref());
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
            saver_gate: Arc::clone(&self.saver_gate),
            unsaved_bubbles_path: self.unsaved_bubbles_path.clone(),
            bubbles_path: self.bubbles_path.clone(),
        }
    }
}

fn spawn_bubbles_saver_thread(
    bubbles_path: PathBuf,
    saver_gate: Arc<Mutex<BubblesSaverGate>>,
) -> (Sender<BubblesSaverMessage>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<BubblesSaverMessage>();
    let handle = thread::spawn(move || {
        // Coalesce queued snapshots and persist only the latest one. The channel now carries
        // shared `Arc<Vec<Bubble>>` snapshots, so superseded snapshots are dropped (refcount
        // decrement) without an extra deep clone of the bubble list.
        let mut held_snapshot = None;
        let mut last_write_error = None;
        while let Ok(message) = rx.recv() {
            let BubblesSaverMessage::Snapshot(first) = message else {
                if process_bubbles_saver_control(
                    message,
                    &rx,
                    &bubbles_path,
                    &saver_gate,
                    &mut held_snapshot,
                    &mut last_write_error,
                ) {
                    break;
                }
                continue;
            };
            let mut latest = first;
            let mut control = None;
            while let Ok(next) = rx.try_recv() {
                match next {
                    BubblesSaverMessage::Snapshot(snapshot) => latest = snapshot,
                    other => {
                        control = Some(other);
                        break;
                    }
                }
            }
            let gate = match saver_gate.lock() {
                Ok(gate) => gate,
                Err(poisoned) => poisoned.into_inner(),
            };
            if !gate.paused {
                // Keep the gate locked through the filesystem write. `pause_saver_for_page_op`
                // therefore returns only after any already-selected coalesced snapshot is done.
                last_write_error = write_bubbles_snapshot_to(&bubbles_path, latest.as_slice())
                    .map_err(|err| {
                        let message = format!(
                            "failed to persist bubbles {}: {err:#}",
                            bubbles_path.display()
                        );
                        eprintln!("{message}");
                        message
                    })
                    .err();
            }
            drop(gate);
            if control.is_some_and(|message| {
                process_bubbles_saver_control(
                    message,
                    &rx,
                    &bubbles_path,
                    &saver_gate,
                    &mut held_snapshot,
                    &mut last_write_error,
                )
            }) {
                break;
            }
        }
    });
    (tx, handle)
}

/// Processes a saver control message. Returns true when the worker must exit.
fn process_bubbles_saver_control(
    message: BubblesSaverMessage,
    rx: &Receiver<BubblesSaverMessage>,
    bubbles_path: &Path,
    saver_gate: &Arc<Mutex<BubblesSaverGate>>,
    held_snapshot: &mut Option<Arc<Vec<Bubble>>>,
    last_write_error: &mut Option<String>,
) -> bool {
    match message {
        BubblesSaverMessage::Snapshot(snapshot) => *held_snapshot = Some(snapshot),
        BubblesSaverMessage::BarrierAndHold(ack) => {
            let outcome = last_write_error.clone().map_or(Ok(()), Err);
            if ack.send(outcome).is_err() {
                eprintln!("ERROR bubbles saver barrier requester dropped before acknowledgement");
            }
            let mut hold_depth = 1_usize;
            let mut shutdown_ack = None;
            while hold_depth > 0 {
                let Ok(message) = rx.recv() else {
                    eprintln!("ERROR bubbles saver channel disconnected while snapshots were held");
                    break;
                };
                match message {
                    BubblesSaverMessage::Snapshot(snapshot) => *held_snapshot = Some(snapshot),
                    BubblesSaverMessage::Resume => hold_depth -= 1,
                    BubblesSaverMessage::BarrierAndHold(nested_ack) => {
                        hold_depth = hold_depth.saturating_add(1);
                        let outcome = last_write_error.clone().map_or(Ok(()), Err);
                        if nested_ack.send(outcome).is_err() {
                            eprintln!("ERROR nested bubbles saver barrier requester dropped");
                        }
                    }
                    BubblesSaverMessage::Shutdown(ack) => {
                        if shutdown_ack.replace(ack).is_some() {
                            eprintln!("ERROR bubbles saver received duplicate shutdown requests");
                        }
                    }
                }
            }
            if let Some(snapshot) = held_snapshot.take() {
                let gate = match saver_gate.lock() {
                    Ok(gate) => gate,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if !gate.paused {
                    *last_write_error = write_bubbles_snapshot_to(bubbles_path, snapshot.as_slice())
                        .map_err(|err| {
                            let message = format!(
                                "failed to persist bubbles {}: {err:#}",
                                bubbles_path.display()
                            );
                            eprintln!("{message}");
                            message
                        })
                        .err();
                } else {
                    eprintln!(
                        "INFO bubbles saver intentionally dropped a held snapshot while paused"
                    );
                }
            }
            if let Some(ack) = shutdown_ack {
                if ack.send(()).is_err() {
                    eprintln!("ERROR bubbles saver shutdown requester dropped");
                }
                return true;
            }
        }
        BubblesSaverMessage::Resume => {}
        BubblesSaverMessage::Shutdown(ack) => {
            if ack.send(()).is_err() {
                eprintln!("ERROR bubbles saver shutdown requester dropped");
            }
            return true;
        }
    }
    false
}

/// Synchronously persists a bubbles snapshot to the supplied unsaved staging path.
///
/// Structural page operations call this only after [`BubblesModel::pause_saver_for_page_op`],
/// which prevents the normal coalescing saver from racing the transaction.
pub fn write_bubbles_snapshot_to(path: &Path, bubbles: &[Bubble]) -> Result<()> {
    // Routed through the storage seam so the web build persists bubbles to its
    // in-memory/IndexedDB store instead of the desktop filesystem.
    let store = crate::storage::storage();
    if let Some(parent) = path.parent() {
        let parent_str = parent.to_string_lossy();
        store
            .create_dir_all(parent_str.as_ref())
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    let raw = serde_json::to_string_pretty(bubbles).context("failed to serialize bubbles")?;
    store
        .write(path.to_string_lossy().as_ref(), raw.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
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
    bubble_class: Option<String>,
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
        bubble_class,
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

/// Sends a shared bubble snapshot to the coalescing saver thread.
///
/// The snapshot is shared (`Arc<Vec<Bubble>>`); only the `Arc` is cloned, never the
/// underlying bubble list. A stopped saver is reported; it cannot be replaced without also
/// replacing the model-owned join handle.
fn send_snapshot_to_bubbles_saver(
    saver_tx: &Arc<Mutex<Sender<BubblesSaverMessage>>>,
    unsaved_bubbles_path: &Path,
    bubbles_path: &Path,
    snapshot: &Arc<Vec<Bubble>>,
    saver_gate: &Arc<Mutex<BubblesSaverGate>>,
) {
    let paused = match saver_gate.lock() {
        Ok(gate) => gate.paused,
        Err(poisoned) => poisoned.into_inner().paused,
    };
    if paused {
        return;
    }
    let sender = match saver_tx.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    if sender
        .send(BubblesSaverMessage::Snapshot(Arc::clone(snapshot)))
        .is_ok()
    {
        return;
    }

    eprintln!(
        "ERROR bubbles saver unavailable; snapshot for '{}' could not be queued to staging '{}'",
        bubbles_path.display(),
        unsaved_bubbles_path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Builds a unique temporary staging path so the background saver of each test model
    /// writes to its own file and tests do not collide.
    fn unique_unsaved_path() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("manhwastudio_bubbles_model_test_{n}.json"))
    }

    /// Creates a bubble whose `extra` map carries a single string entry under `key`.
    fn bubble_with_extra(id: i64, key: &str, value: &str) -> Bubble {
        let mut extra = Map::new();
        extra.insert(key.to_string(), Value::String(value.to_string()));
        runtime_bubble_to_record(
            id,
            0,
            0.5,
            0.5,
            None,
            None,
            None,
            String::new(),
            String::new(),
            Some(extra),
        )
    }

    /// Builds a model from `bubbles` with isolated temporary save paths.
    fn model_with(bubbles: Vec<Bubble>) -> BubblesModel {
        let unsaved = unique_unsaved_path();
        BubblesModel::new(
            bubbles,
            unsaved.with_extension("saved.json"),
            unsaved,
            SharedCanvasSettings::default(),
        )
    }

    #[test]
    fn extra_of_returns_only_matching_bubble_extra() {
        let model = model_with(vec![
            bubble_with_extra(1, "image_source_type", "external"),
            bubble_with_extra(2, "image_source_type", "page_crop"),
        ]);
        let extra = model.extra_of(2).expect("bubble 2 should exist");
        assert_eq!(
            extra.get("image_source_type").and_then(Value::as_str),
            Some("page_crop")
        );
        // Reading one bubble's extra must not require the other bubble's data.
        assert!(model.extra_of(99).is_none());
    }

    #[test]
    fn with_bubble_reads_fields_by_id_without_cloning_list() {
        let model = model_with(vec![
            bubble_with_extra(10, "k", "a"),
            bubble_with_extra(20, "k", "b"),
        ]);
        let id = model
            .with_bubble(20, |bubble| bubble.id)
            .expect("bubble 20 should exist");
        assert_eq!(id, 20);
        assert!(model.with_bubble(7, |bubble| bubble.id).is_none());
    }

    #[test]
    fn extra_of_tracks_index_after_delete() {
        let mut model = model_with(vec![
            bubble_with_extra(1, "k", "first"),
            bubble_with_extra(2, "k", "second"),
            bubble_with_extra(3, "k", "third"),
        ]);
        model.delete(1).expect("delete should succeed");
        // After removing the first element the index must still resolve the remaining ids.
        assert_eq!(
            model
                .extra_of(3)
                .and_then(|e| e.get("k"))
                .and_then(Value::as_str),
            Some("third")
        );
        assert!(model.extra_of(1).is_none());
    }

    /// A successful barrier means every mutation enqueued before it is present in staging.
    #[test]
    fn saver_barrier_persists_preceding_mutation() {
        let mut model = model_with(Vec::new());
        let staging_path = model.unsaved_bubbles_path.clone();
        assert!(model.create_or_replace(bubble_with_extra(1, "k", "before")).is_ok());
        let barrier = model.saver_handle().barrier_and_hold_blocking();
        assert!(barrier.is_ok(), "barrier must verify the queued write");
        drop(barrier);

        let Ok(raw) = std::fs::read_to_string(&staging_path) else {
            panic!("barrier returned success without a readable staging file");
        };
        let Ok(saved) = serde_json::from_str::<Vec<Bubble>>(&raw) else {
            panic!("barrier returned success with invalid staging JSON");
        };
        assert_eq!(saved.first().map(|bubble| bubble.id), Some(1));
        assert!(model.shutdown_saver().is_ok());
        assert!(std::fs::remove_file(staging_path).is_ok());
    }

    /// Releasing a barrier resumes persistence instead of permanently pausing the saver.
    #[test]
    fn saver_continues_after_barrier() {
        let mut model = model_with(Vec::new());
        let staging_path = model.unsaved_bubbles_path.clone();
        assert!(model.create_or_replace(bubble_with_extra(1, "k", "before")).is_ok());
        let Ok(barrier) = model.saver_handle().barrier_and_hold_blocking() else {
            panic!("first barrier failed");
        };
        drop(barrier);
        assert!(model.create_or_replace(bubble_with_extra(2, "k", "after")).is_ok());
        let second_barrier = model.saver_handle().barrier_and_hold_blocking();
        assert!(second_barrier.is_ok(), "saver did not accept a barrier after resume");
        drop(second_barrier);

        let Ok(raw) = std::fs::read_to_string(&staging_path) else {
            panic!("resumed saver did not produce a readable staging file");
        };
        let Ok(saved) = serde_json::from_str::<Vec<Bubble>>(&raw) else {
            panic!("resumed saver produced invalid staging JSON");
        };
        assert!(saved.iter().any(|bubble| bubble.id == 2));
        assert!(model.shutdown_saver().is_ok());
        assert!(std::fs::remove_file(staging_path).is_ok());
    }

    /// Pausing for discard drops an already-held snapshot and cannot recreate deleted staging.
    #[test]
    fn paused_saver_shutdown_does_not_recreate_staging() {
        let staging_dir = unique_unsaved_path().with_extension("staging");
        let staging_path = staging_dir.join("translation_bubbles.json");
        if staging_dir.exists() {
            assert!(std::fs::remove_dir_all(&staging_dir).is_ok());
        }
        let mut model = BubblesModel::new(
            Vec::new(),
            staging_dir.join("committed.json"),
            staging_path.clone(),
            SharedCanvasSettings::default(),
        );
        let Ok(barrier) = model.saver_handle().barrier_and_hold_blocking() else {
            panic!("initial barrier failed");
        };
        assert!(model.create_or_replace(bubble_with_extra(1, "k", "discarded")).is_ok());
        model.pause_saver_for_page_op();
        assert!(std::fs::remove_dir_all(&staging_dir).is_ok() || !staging_dir.exists());
        drop(barrier);
        assert!(model.shutdown_saver().is_ok());
        assert!(!staging_dir.exists(), "paused shutdown recreated discarded staging");
    }

    /// Each barrier owns one hold level; releasing an outer guard cannot release a nested hold.
    #[test]
    fn nested_barrier_requires_every_guard_to_resume() {
        let mut model = model_with(Vec::new());
        let staging_path = model.unsaved_bubbles_path.clone();
        if staging_path.exists() {
            assert!(std::fs::remove_file(&staging_path).is_ok());
        }
        let handle = model.saver_handle();
        let Ok(first) = handle.barrier_and_hold_blocking() else {
            panic!("first barrier failed");
        };
        assert!(model.create_or_replace(bubble_with_extra(1, "k", "held")).is_ok());
        let Ok(second) = handle.barrier_and_hold_blocking() else {
            panic!("nested barrier failed");
        };
        drop(first);
        let Ok(probe) = handle.barrier_and_hold_blocking() else {
            panic!("probe barrier failed");
        };
        assert!(!staging_path.exists(), "first resume released a nested hold");
        drop(second);
        assert!(!staging_path.exists(), "second resume released the probe hold");
        drop(probe);
        let Ok(flushed) = handle.barrier_and_hold_blocking() else {
            panic!("post-resume barrier failed");
        };
        drop(flushed);
        assert!(staging_path.exists(), "final resume did not persist the held snapshot");
        assert!(model.shutdown_saver().is_ok());
        assert!(std::fs::remove_file(staging_path).is_ok());
    }
}
