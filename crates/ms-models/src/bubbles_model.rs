/*
FILE OVERVIEW: crates/ms-models/src/bubbles_model.rs
Shared runtime model for translation bubbles and canvas settings snapshots.

Main items:
- `SharedCanvasSettings`: lightweight cross-tab canvas settings payload, including
  editable/readonly default display types for `default` bubbles. It maps its 13 layout fields to
  and from `ms_project::CanvasLayout` (`layout` / `apply_layout`, the one way a layout preset is
  applied to shared settings); its `Default` takes them from `ms_project::BASE_BUILTIN`.
- `BubblesModel`: thread-safe mutable bubbles store with revisions, async saver, and
  flush/hold barriers for destructive staging operations.
- `runtime_bubble_to_record`: adapter from canvas runtime fields to persisted `Bubble`,
  including per-bubble `bubble_class` and text display `bubble_type`.

Threading/persistence:
- Bubble writes are coalesced through `spawn_bubbles_saver_thread` so GUI thread only
  publishes snapshots and does not block on filesystem I/O. The saver channel carries
  shared `Arc<Vec<Bubble>>` snapshots, so publishing a save never deep-clones the list.
- With an `AutosaveGate` (`BubblesModel::new(.., Some(gate))`) every mutation reports one
  action and the saver HOLDS its latest snapshot until the gate is due; `BarrierAndHold`
  and `Shutdown` always write the held snapshot first (dropped only while paused).
  `None` = immediate mode (write as soon as the channel drains).
- `with_bubble`/`extra_of` let callers read a single bubble (or its `extra` map) by id
  without cloning the whole list via `snapshot()`.
- The saver always writes to the unsaved staging folder (`unsaved_bubbles_path`).
  The main chapter file is only updated by an explicit "save to project" merge.
- `has_unsaved_changes()` returns true when the unsaved staging bubbles document exists
  (probed through `ms_docstore::exists`, format-independent: `staged_bubbles_exist`). The
  initial probe runs in `BubblesModel::new`, or is pre-read by the caller off the GUI thread and
  passed to `BubblesModel::new_with_staging_probe` (the studio does this on its open worker).
- Every write of the bubbles document (`write_bubbles_snapshot_to`) goes through
  `ms_docstore` (atomic whole-document replace), never a raw file write.
*/

use ms_config::bubble_status::{BubbleStatusRule, default_bubble_status_rules};
use ms_project::{BASE_BUILTIN, Bubble, CanvasLayout};
use ms_docstore::{DocKind, DocRef};
use ms_log::runtime_log;
use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ms_thread::{self as thread, JoinHandle};
use crate::autosave_gate::AutosaveGate;

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
            runtime_log::log_error("[bubbles_saver] saver stopped before a held barrier could resume; later snapshots of this session are not persisted");
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
    /// Initial value of a newly created hint bubble's `hint_show_outside_translation` flag.
    /// User-level (cross-project) preference; see `canvas/settings.rs` for its persistence.
    pub hint_show_outside_default: bool,
}

impl Default for SharedCanvasSettings {
    /// The non-layout defaults (kept in step with `CanvasSettings::default` in
    /// `crates/ms-project/src/lib.rs`) plus the layout of `ms_project::BASE_BUILTIN`, the one
    /// layout table. `canvas_defaults_agree_across_the_three_mirrors` (ms-canvas `types.rs`)
    /// guards the agreement.
    fn default() -> Self {
        let CanvasLayout {
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
        } = BASE_BUILTIN.factory();
        Self {
            bubble_type: "hybrid".to_string(),
            editable_bubble_type,
            readonly_bubble_type,
            show_bubbles: true,
            show_bubble_status: false,
            bubble_status_rules: default_bubble_status_rules(),
            bubble_opacity: 1.0,
            page_spacing: px_to_world(page_spacing_px),
            separate_pages,
            edge_margin: px_to_world(vertical_edge_margin_px),
            side_margin: 20.0,
            bubble_min_width: px_to_world(aside_min_width_px),
            bubble_max_width: px_to_world(aside_max_width_px),
            aside_compact_mode,
            aside_side_mode,
            aside_second_column,
            on_top_focus_mode,
            scale_bubbles,
            aside_scale_pct,
            auto_insert_last_character: true,
            spellcheck_original: false,
            spellcheck_translation: true,
            tabs_autosync_enabled: true,
            cache_pages: true,
            translation_status_display: "marks".to_string(),
            hint_show_outside_default: false,
        }
    }
}

/// A persisted integer px length as shared-settings world units (`f32`).
///
/// Exact for |v| <= 2^24, far beyond any meaningful pixel length; above that the nearest `f32`
/// is taken, the same conversion the canvas applies on open (`ms-canvas` `pixel_setting_to_f32`).
fn px_to_world(value: i32) -> f32 {
    value as f32
}

/// A shared-settings world length as persisted integer px: negative values clamp to 0 and the
/// value is rounded to the nearest integer (the rounding of the project canvas writer).
///
/// Float-to-int `as` saturates at the `i32` bounds and maps NaN to 0; after `round()` no
/// fraction is lost, so that documented saturation is the only possible change of value.
fn world_to_px(value: f32) -> i32 {
    value.max(0.0).round() as i32
}

/// Assigns `value` to `slot` and reports whether the stored value changed.
fn assign_changed<T: PartialEq>(slot: &mut T, value: T) -> bool {
    if *slot == value {
        false
    } else {
        *slot = value;
        true
    }
}

impl SharedCanvasSettings {
    /// The preset-owned layout of these settings, normalized (`CanvasLayout::normalized`); world
    /// lengths become integer px through `world_to_px`.
    #[must_use]
    pub fn layout(&self) -> CanvasLayout {
        CanvasLayout {
            separate_pages: self.separate_pages,
            page_spacing_px: world_to_px(self.page_spacing),
            vertical_edge_margin_px: world_to_px(self.edge_margin),
            aside_compact_mode: self.aside_compact_mode.clone(),
            aside_min_width_px: world_to_px(self.bubble_min_width),
            aside_max_width_px: world_to_px(self.bubble_max_width),
            aside_scale_pct: self.aside_scale_pct,
            aside_side_mode: self.aside_side_mode.clone(),
            scale_bubbles: self.scale_bubbles,
            aside_second_column: self.aside_second_column,
            editable_bubble_type: self.editable_bubble_type.clone(),
            readonly_bubble_type: self.readonly_bubble_type.clone(),
            on_top_focus_mode: self.on_top_focus_mode.clone(),
        }
        .normalized()
    }

    /// Writes `layout` into these settings verbatim (px through `px_to_world`); every other
    /// field is left as it is. Returns `true` iff a field value actually changed.
    pub fn apply_layout(&mut self, layout: &CanvasLayout) -> bool {
        let CanvasLayout {
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
        // `|=`, not `||`: every field is assigned even after the first change.
        let mut changed = false;
        changed |= assign_changed(&mut self.separate_pages, *separate_pages);
        changed |= assign_changed(&mut self.page_spacing, px_to_world(*page_spacing_px));
        changed |= assign_changed(&mut self.edge_margin, px_to_world(*vertical_edge_margin_px));
        changed |= assign_changed(&mut self.aside_compact_mode, aside_compact_mode.clone());
        changed |= assign_changed(&mut self.bubble_min_width, px_to_world(*aside_min_width_px));
        changed |= assign_changed(&mut self.bubble_max_width, px_to_world(*aside_max_width_px));
        changed |= assign_changed(&mut self.aside_scale_pct, *aside_scale_pct);
        changed |= assign_changed(&mut self.aside_side_mode, aside_side_mode.clone());
        changed |= assign_changed(&mut self.scale_bubbles, *scale_bubbles);
        changed |= assign_changed(&mut self.aside_second_column, *aside_second_column);
        changed |= assign_changed(&mut self.editable_bubble_type, editable_bubble_type.clone());
        changed |= assign_changed(&mut self.readonly_bubble_type, readonly_bubble_type.clone());
        changed |= assign_changed(&mut self.on_top_focus_mode, on_top_focus_mode.clone());
        changed
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
    /// The project's autosave gate (`None` = immediate mode): every mutation reports one action and
    /// the saver holds its latest snapshot until the gate is due.
    autosave_gate: Option<Arc<AutosaveGate>>,
}

#[derive(Clone)]
pub struct BubblesSaveTask {
    snapshot: Arc<Vec<Bubble>>,
    saver_tx: Arc<Mutex<Sender<BubblesSaverMessage>>>,
    saver_gate: Arc<Mutex<BubblesSaverGate>>,
    unsaved_bubbles_path: PathBuf,
    bubbles_path: PathBuf,
    autosave_gate: Option<Arc<AutosaveGate>>,
}

impl BubblesSaveTask {
    /// Sends the snapshot to the saver and reports one autosave action (see
    /// `send_snapshot_to_bubbles_saver`).
    pub fn persist(&self) {
        send_snapshot_to_bubbles_saver(
            &self.saver_tx,
            &self.unsaved_bubbles_path,
            &self.bubbles_path,
            &self.snapshot,
            &self.saver_gate,
            self.autosave_gate.as_deref(),
        );
    }
}

/// State shared by the coalescing saver and a structural-operation quiescence barrier.
#[derive(Debug, Default)]
struct BubblesSaverGate {
    /// Why the saver is paused; `None` while it runs. While paused, held and published snapshots
    /// are dropped (the pausing operation owns the staging folder).
    pause_reason: Option<SaverPauseReason>,
}

/// Which staging-owning operation paused the bubbles saver (named in its log lines).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaverPauseReason {
    /// A structural page operation (`BubblesModel::pause_saver_for_page_op`).
    PageOp,
    /// A DISCARD of the session's staging (`BubblesModel::pause_saver_for_discard`).
    Discard,
}

impl SaverPauseReason {
    /// Log label of the reason.
    fn label(self) -> &'static str {
        match self {
            Self::PageOp => "page operation",
            Self::Discard => "discard",
        }
    }
}

/// Whether the staging bubbles document at `unsaved_bubbles_path` exists, in any format. Asks the
/// document store (`ms_docstore::exists`), which probes through the storage seam, so the web build
/// checks its in-memory/IndexedDB store. Blocking I/O: the initial "unsaved changes" flag of
/// [`BubblesModel::new_with_staging_probe`], pre-read off the GUI thread by the studio's open worker.
#[must_use]
pub fn staged_bubbles_exist(unsaved_bubbles_path: &Path) -> bool {
    ms_docstore::exists(&DocRef::new(unsaved_bubbles_path, DocKind::Bubbles))
}

#[allow(dead_code)]
impl BubblesModel {
    /// Builds the model and spawns its saver, probing the staging bubbles document itself
    /// ([`staged_bubbles_exist`], blocking I/O). Callers on the GUI thread pre-read the probe and
    /// use [`Self::new_with_staging_probe`] instead.
    pub fn new(
        bubbles: Vec<Bubble>,
        bubbles_path: PathBuf,
        unsaved_bubbles_path: PathBuf,
        canvas_settings: SharedCanvasSettings,
        autosave_gate: Option<Arc<AutosaveGate>>,
    ) -> Self {
        let has_staged_bubbles = staged_bubbles_exist(&unsaved_bubbles_path);
        Self::new_with_staging_probe(bubbles, bubbles_path, unsaved_bubbles_path, canvas_settings, autosave_gate, has_staged_bubbles)
    }

    /// Same as [`Self::new`], but takes the staging probe's answer (`has_staged_bubbles`: what
    /// [`staged_bubbles_exist`] returned for `unsaved_bubbles_path`, read shortly before) instead of
    /// performing it, so it does no document I/O. Spawns the saver thread.
    pub fn new_with_staging_probe(
        bubbles: Vec<Bubble>,
        bubbles_path: PathBuf,
        unsaved_bubbles_path: PathBuf,
        canvas_settings: SharedCanvasSettings,
        autosave_gate: Option<Arc<AutosaveGate>>,
        has_staged_bubbles: bool,
    ) -> Self {
        let saver_gate = Arc::new(Mutex::new(BubblesSaverGate::default()));
        let (saver_sender, saver_thread) = spawn_bubbles_saver_thread(
            unsaved_bubbles_path.clone(),
            Arc::clone(&saver_gate),
            autosave_gate.clone(),
        );
        let saver_tx = Arc::new(Mutex::new(saver_sender));
        let bubble_index_by_id = build_bubble_index(&bubbles);
        Self {
            bubbles: Arc::new(bubbles),
            bubble_index_by_id,
            bubbles_path,
            has_unsaved_changes: has_staged_bubbles,
            unsaved_bubbles_path,
            revision: 1,
            canvas_settings,
            canvas_revision: 1,
            saver_tx,
            saver_thread: Some(saver_thread),
            saver_gate,
            autosave_gate,
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
        self.pause_saver(SaverPauseReason::PageOp)
    }

    /// Same quiescence as [`Self::pause_saver_for_page_op`], for a DISCARD of the staging folder:
    /// held and later snapshots are dropped (logged as dropped for a discard). Permanent unless
    /// [`Self::resume_saver_after_failed_discard`] reverses it.
    pub fn pause_saver_for_discard(&mut self) -> Arc<Vec<Bubble>> {
        self.pause_saver(SaverPauseReason::Discard)
    }

    /// Pauses the saver for `reason` (waits for an in-progress write) and returns the current state.
    fn pause_saver(&mut self, reason: SaverPauseReason) -> Arc<Vec<Bubble>> {
        match self.saver_gate.lock() {
            Ok(mut gate) => gate.pause_reason = Some(reason),
            Err(poisoned) => poisoned.into_inner().pause_reason = Some(reason),
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
            Ok(mut gate) => gate.pause_reason = None,
            Err(poisoned) => poisoned.into_inner().pause_reason = None,
        }
        self.has_unsaved_changes = true;
        send_snapshot_to_bubbles_saver(
            &self.saver_tx,
            &self.unsaved_bubbles_path,
            &self.bubbles_path,
            &self.bubbles,
            &self.saver_gate,
            self.autosave_gate.as_deref(),
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

    /// Marks the model saved after a save-to-project merge without any file access:
    /// `saved_revision` is [`Self::revision`] read on the GUI thread when the save started (every
    /// edit up to it was sent to the saver before the worker's first barrier, so it is in the
    /// merge), and `staging_exists` the worker's post-merge stat of the staging directory. Any
    /// edit since the save started (the revision moved) or a re-created staging keeps the model
    /// unsaved; filesystem timing (a snapshot still queued in the saver) cannot hide such an edit.
    pub fn mark_saved_to_project_as_of(&mut self, saved_revision: u64, staging_exists: bool) {
        self.has_unsaved_changes = staging_exists || self.revision != saved_revision;
    }

    pub fn mark_saved_to_project(&mut self) {
        // A mutation accepted while the save barrier was held is written to a newly recreated
        // staging file after the merge. Preserve that post-save dirty state instead of clearing it.
        self.has_unsaved_changes = staged_bubbles_exist(&self.unsaved_bubbles_path);
    }

    fn touch_and_save(&mut self) -> Result<()> {
        let task = self.touch_and_prepare_save_task();
        task.persist();
        Ok(())
    }

    /// Bumps the revision, marks unsaved changes and returns the save task. The task's `persist`
    /// reports the gesture's ONE autosave action, right after its snapshot is sent (a deferred task
    /// may be persisted long after this touch; the saver polls the gate before draining, so the
    /// action must never precede the send).
    fn touch_and_prepare_save_task(&mut self) -> BubblesSaveTask {
        self.revision = self.revision.saturating_add(1);
        self.has_unsaved_changes = true;
        BubblesSaveTask {
            snapshot: Arc::clone(&self.bubbles),
            saver_tx: Arc::clone(&self.saver_tx),
            saver_gate: Arc::clone(&self.saver_gate),
            unsaved_bubbles_path: self.unsaved_bubbles_path.clone(),
            bubbles_path: self.bubbles_path.clone(),
            autosave_gate: self.autosave_gate.clone(),
        }
    }
}

/// Upper bound of one saver wait while a snapshot is held, so a gate epoch moved by another
/// writer's action (or a live policy change) is noticed within about a second.
const MAX_HOLD_WAIT: Duration = Duration::from_secs(1);

/// Spawns the coalescing bubbles saver.
///
/// The worker keeps the LATEST unwritten snapshot (`pending`, one `Arc` — superseded snapshots are
/// dropped by refcount, never deep-cloned) across iterations. With `autosave_gate: None` (immediate
/// mode) it is written as soon as the channel drains; with a gate it is held until the gate's flush
/// epoch moves. `BarrierAndHold` and `Shutdown` ALWAYS write what is held first (they are the force
/// points), unless the saver is paused for a page operation / discard — then it is dropped.
fn spawn_bubbles_saver_thread(
    bubbles_path: PathBuf,
    saver_gate: Arc<Mutex<BubblesSaverGate>>,
    autosave_gate: Option<Arc<AutosaveGate>>,
) -> (Sender<BubblesSaverMessage>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<BubblesSaverMessage>();
    // Polled on the spawning thread so a late thread start cannot swallow an epoch advance.
    let initial_epoch = autosave_gate.as_deref().map_or(0, AutosaveGate::poll);
    let handle = thread::spawn(move || {
        let autosave_gate = autosave_gate.as_deref();
        let mut worker = BubblesSaverWorker {
            bubbles_path,
            saver_gate,
            pending: None,
            last_write_error: None,
            seen_epoch: initial_epoch,
        };
        loop {
            let message = match wait_bubbles_message(&rx, autosave_gate, worker.pending.is_some()) {
                Ok(message) => Some(message),
                // The hold wait elapsed with no message: re-check the gate.
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    // Defensive: every sender is gone. Write what is held rather than lose it.
                    worker.write_pending();
                    break;
                }
            };
            // Poll BEFORE draining: a snapshot's action is reported after its send, so every snapshot
            // whose action this epoch reflects is already queued.
            let epoch = autosave_gate.map(AutosaveGate::poll);
            // Coalesce every immediately queued snapshot; stop at the first control message, which
            // is then processed AFTER the snapshots that preceded it (FIFO).
            let mut control = None;
            let mut next = message;
            while let Some(message) = next.take() {
                match message {
                    BubblesSaverMessage::Snapshot(snapshot) => worker.pending = Some(snapshot),
                    other => {
                        control = Some(other);
                        break;
                    }
                }
                next = rx.try_recv().ok();
            }
            let stopped_at_control = control.is_some();
            if let Some(message) = control
                && worker.process_control(message, &rx)
            {
                break;
            }
            match epoch {
                // Immediate mode: write as soon as the channel drains (or a hold ends).
                None => worker.write_pending(),
                // A drain that stopped at a control message may have left snapshots sent before the
                // poll in the channel, so `epoch` cannot be marked seen yet; the next wait (bounded
                // while holding) re-polls. Otherwise the channel was emptied: act on `epoch`.
                Some(epoch) if !stopped_at_control && epoch != worker.seen_epoch => {
                    worker.write_pending();
                    worker.seen_epoch = epoch;
                }
                Some(_) => {}
            }
        }
    });
    (tx, handle)
}

/// Waits for the next saver message: a blocking `recv` when nothing is held (or in immediate mode),
/// else a `recv_timeout` bounded by the gate's remaining window and [`MAX_HOLD_WAIT`]. `Timeout`
/// means "re-check the gate"; `Disconnected` means every sender is gone.
fn wait_bubbles_message(rx: &Receiver<BubblesSaverMessage>, gate: Option<&AutosaveGate>, holding: bool) -> Result<BubblesSaverMessage, RecvTimeoutError> {
    let Some(gate) = gate.filter(|_| holding) else {
        return rx.recv().map_err(|_| RecvTimeoutError::Disconnected);
    };
    let wait = gate.wait_deadline().map_or(MAX_HOLD_WAIT, |deadline| deadline.min(MAX_HOLD_WAIT));
    rx.recv_timeout(wait)
}

/// Worker-thread state of the bubbles saver.
struct BubblesSaverWorker {
    bubbles_path: PathBuf,
    saver_gate: Arc<Mutex<BubblesSaverGate>>,
    /// The latest snapshot not yet written (held for the autosave gate or a barrier hold).
    pending: Option<Arc<Vec<Bubble>>>,
    /// Outcome of the last write, reported to the next barrier.
    last_write_error: Option<String>,
    /// Autosave gate epoch this worker last acted on (forced writes never change it).
    seen_epoch: u64,
}

impl BubblesSaverWorker {
    /// Writes and clears `pending`, if any. While paused (a page operation or discard owns the staging
    /// folder) the snapshot is dropped instead. The pause lock is held through the write, so
    /// `pause_saver_for_page_op` returns only after an in-progress write has finished.
    fn write_pending(&mut self) {
        let Some(snapshot) = self.pending.take() else {
            return;
        };
        let gate = match self.saver_gate.lock() {
            Ok(gate) => gate,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(reason) = gate.pause_reason {
            let reason = reason.label();
            runtime_log::log_info(format!(
                "[bubbles_saver] dropped a held snapshot of {} bubbles for '{}': saver paused for a {reason}, which owns the staging folder",
                snapshot.len(),
                self.bubbles_path.display()
            ));
            return;
        }
        self.last_write_error = write_bubbles_snapshot_to(&self.bubbles_path, snapshot.as_slice())
            .map_err(|err| {
                let message = format!("failed to persist bubbles {}: {err:#}", self.bubbles_path.display());
                runtime_log::log_error(format!("[bubbles_saver] {message}; reported to the next barrier"));
                message
            })
            .err();
    }

    /// Processes a saver control message. Returns true when the worker must exit.
    ///
    /// `BarrierAndHold` first force-writes the held snapshot, acknowledges with the last write outcome,
    /// then holds every later snapshot (nested holds counted) until each guard resumes; a `Shutdown`
    /// received during the hold force-writes and exits once the hold ends. After a hold without a
    /// shutdown, the held snapshot stays `pending` for the normal gate decision.
    fn process_control(&mut self, message: BubblesSaverMessage, rx: &Receiver<BubblesSaverMessage>) -> bool {
        match message {
            BubblesSaverMessage::Snapshot(snapshot) => self.pending = Some(snapshot),
            BubblesSaverMessage::BarrierAndHold(ack) => {
                self.write_pending();
                let outcome = self.last_write_error.clone().map_or(Ok(()), Err);
                if ack.send(outcome).is_err() {
                    runtime_log::log_error("[bubbles_saver] barrier requester dropped before acknowledgement");
                }
                let mut hold_depth = 1_usize;
                let mut shutdown_ack = None;
                while hold_depth > 0 {
                    let Ok(message) = rx.recv() else {
                        runtime_log::log_error("[bubbles_saver] channel disconnected while snapshots were held; ending the hold");
                        break;
                    };
                    match message {
                        BubblesSaverMessage::Snapshot(snapshot) => self.pending = Some(snapshot),
                        BubblesSaverMessage::Resume => hold_depth -= 1,
                        BubblesSaverMessage::BarrierAndHold(nested_ack) => {
                            hold_depth = hold_depth.saturating_add(1);
                            let outcome = self.last_write_error.clone().map_or(Ok(()), Err);
                            if nested_ack.send(outcome).is_err() {
                                runtime_log::log_error("[bubbles_saver] nested barrier requester dropped before acknowledgement");
                            }
                        }
                        BubblesSaverMessage::Shutdown(ack) => {
                            if shutdown_ack.replace(ack).is_some() {
                                runtime_log::log_error("[bubbles_saver] duplicate shutdown request during a hold; only the latest is acknowledged");
                            }
                        }
                    }
                }
                if let Some(ack) = shutdown_ack {
                    self.write_pending();
                    if ack.send(()).is_err() {
                        runtime_log::log_error("[bubbles_saver] shutdown requester dropped before acknowledgement");
                    }
                    return true;
                }
            }
            BubblesSaverMessage::Resume => {}
            BubblesSaverMessage::Shutdown(ack) => {
                self.write_pending();
                if ack.send(()).is_err() {
                    runtime_log::log_error("[bubbles_saver] shutdown requester dropped before acknowledgement");
                }
                return true;
            }
        }
        false
    }
}

/// Synchronously persists a bubbles snapshot to the supplied unsaved staging path.
///
/// Structural page operations call this only after [`BubblesModel::pause_saver_for_page_op`],
/// which prevents the normal coalescing saver from racing the transaction.
///
/// The document is written whole through `ms_docstore` (atomic temp+rename on native, a
/// plain seam write on the web build), missing parent directories created. An existing
/// document keeps its format; a NEW one is created in the chapter's format
/// (`ms_page_ops::chapter_docs::chapter_doc_for_write`, docstore rule B.3), so a JSON chapter
/// stays JSON in Prod mode. JSON bytes are as before: 2-space pretty array, no trailing newline.
/// Durability per `ms_page_ops::chapter_docs::chapter_doc_durability`: a `{chapter}_unsaved`
/// staging document is not fsynced (JSON), any other keeps `Durability::Contents`.
///
/// # Errors
/// Returns an error naming `path` when serialization, directory creation or the write fails.
pub fn write_bubbles_snapshot_to(path: &Path, bubbles: &[Bubble]) -> Result<()> {
    use ms_page_ops::chapter_docs;
    let durability = chapter_docs::chapter_doc_durability(path, DocKind::Bubbles, ms_docstore::Durability::Contents);
    let opts = ms_docstore::WriteOptions { durability, ..ms_docstore::WriteOptions::default() };
    ms_docstore::write(&chapter_docs::chapter_doc_for_write(path, DocKind::Bubbles), bubbles, opts)
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

/// Sends a shared bubble snapshot to the coalescing saver thread, then reports one action to
/// `autosave_gate` (if any). Nothing is sent or reported while the saver is paused.
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
    autosave_gate: Option<&AutosaveGate>,
) {
    let paused = match saver_gate.lock() {
        Ok(gate) => gate.pause_reason.is_some(),
        Err(poisoned) => poisoned.into_inner().pause_reason.is_some(),
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
        // One published snapshot = one gesture. Reported AFTER the send: the saver polls the gate
        // before draining, so an action must never be counted before its snapshot is queued.
        if let Some(gate) = autosave_gate {
            gate.note_action();
        }
        return;
    }

    runtime_log::log_error(format!(
        "[bubbles_saver] saver unavailable; snapshot for '{}' could not be queued to staging '{}'",
        bubbles_path.display(),
        unsaved_bubbles_path.display()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// `apply_layout` writes every layout field, reports a change exactly when one happened,
    /// leaves the other fields alone, and `layout` reads the applied values back.
    #[test]
    fn apply_layout_reports_change() {
        let webtoon = ms_project::BuiltinLayoutPreset::Webtoon.factory();
        let mut settings = SharedCanvasSettings { side_margin: 31.0, translation_status_display: "none".to_string(), ..SharedCanvasSettings::default() };
        assert!(settings.apply_layout(&webtoon));
        assert_eq!(settings.layout(), webtoon);
        assert!(!settings.apply_layout(&webtoon), "re-apply is a no-op");
        assert_eq!((settings.side_margin, settings.translation_status_display.as_str()), (31.0, "none"));
        let mut one_field = webtoon.clone();
        one_field.on_top_focus_mode = "aside".to_string();
        assert!(settings.apply_layout(&one_field), "a single changed field counts");
        assert_eq!(settings.on_top_focus_mode, "aside");
    }

    /// `layout` is normalized: tokens folded, lengths rounded to px and clamped like the reader.
    #[test]
    fn layout_is_normalized() {
        let settings = SharedCanvasSettings { page_spacing: -2.0, edge_margin: 10.6, bubble_min_width: 12.0, bubble_max_width: 8.0, aside_compact_mode: " NONE".to_string(), aside_scale_pct: 400, ..SharedCanvasSettings::default() };
        let layout = settings.layout();
        assert_eq!((layout.page_spacing_px, layout.vertical_edge_margin_px, layout.aside_min_width_px, layout.aside_max_width_px, layout.aside_scale_pct), (0, 11, 40, 40, 300));
        assert_eq!(layout.aside_compact_mode, "none");
    }

    /// The default's layout is the base preset.
    #[test]
    fn default_layout_is_the_base_preset() {
        assert_eq!(SharedCanvasSettings::default().layout(), BASE_BUILTIN.factory());
    }

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
            None,
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

    /// A save completion keeps the model unsaved for an edit made after the save started even
    /// while that edit's snapshot is still held in the saver (no staging file yet), and for a
    /// re-created staging; it is clean only when neither happened.
    #[test]
    fn mark_saved_as_of_sees_an_edit_whose_snapshot_is_still_queued() {
        let mut model = model_with(Vec::new());
        let staging_path = model.unsaved_bubbles_path.clone();
        assert!(model.create_or_replace(bubble_with_extra(1, "k", "merged")).is_ok());
        let hold = model.saver_handle().barrier_and_hold_blocking();
        assert!(hold.is_ok(), "barrier must verify the queued write");
        let saved_revision = model.revision();
        assert!(std::fs::remove_file(&staging_path).is_ok(), "the merge deleted staging");

        // An edit after the save started: its snapshot waits behind the hold, staging stays absent.
        assert!(model.create_or_replace(bubble_with_extra(1, "k", "after")).is_ok());
        assert!(!staged_bubbles_exist(&staging_path), "the held snapshot has not been written");
        model.mark_saved_to_project_as_of(saved_revision, false);
        assert!(model.has_unsaved_changes(), "the revision moved: the edit is not in the merge");

        model.mark_saved_to_project_as_of(model.revision(), false);
        assert!(!model.has_unsaved_changes(), "nothing since the save and no staging: clean");
        model.mark_saved_to_project_as_of(model.revision(), true);
        assert!(model.has_unsaved_changes(), "a re-created staging keeps it unsaved");

        drop(hold);
        assert!(model.shutdown_saver().is_ok());
        if staging_path.exists() {
            assert!(std::fs::remove_file(&staging_path).is_ok());
        }
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
            None,
        );
        let Ok(barrier) = model.saver_handle().barrier_and_hold_blocking() else {
            panic!("initial barrier failed");
        };
        assert!(model.create_or_replace(bubble_with_extra(1, "k", "discarded")).is_ok());
        model.pause_saver_for_discard();
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

    /// The initial "unsaved changes" flag: `new` probes the staging document itself, and
    /// `new_with_staging_probe` takes the pre-read answer verbatim without probing.
    #[test]
    fn the_initial_unsaved_flag_follows_the_staging_probe() {
        let dir = tempfile::tempdir().expect("temp dir");
        let unsaved = dir.path().join("translation_bubbles.json");
        let committed = dir.path().join("committed.json");
        assert!(!staged_bubbles_exist(&unsaved));
        let fresh = BubblesModel::new(Vec::new(), committed.clone(), unsaved.clone(), SharedCanvasSettings::default(), None);
        assert!(!fresh.has_unsaved_changes());
        let told = BubblesModel::new_with_staging_probe(Vec::new(), committed.clone(), unsaved.clone(), SharedCanvasSettings::default(), None, true);
        assert!(told.has_unsaved_changes(), "the pre-read answer is taken verbatim");
        assert!(write_bubbles_snapshot_to(&unsaved, &[]).is_ok());
        assert!(staged_bubbles_exist(&unsaved));
        let staged = BubblesModel::new(Vec::new(), committed, unsaved, SharedCanvasSettings::default(), None);
        assert!(staged.has_unsaved_changes());
    }

    // ---- Autosave gate ------------------------------------------------------------------------

    fn gated_model(interval: std::time::Duration, action_threshold: u32) -> BubblesModel {
        let gate = Arc::new(AutosaveGate::with_policy_fn(move || ms_config::autosave_policy::AutosavePolicy { interval, action_threshold }));
        let unsaved = unique_unsaved_path();
        // The path is unique per process only; a file left by an earlier aborted run must not read
        // as "written by this test".
        if unsaved.exists() {
            assert!(std::fs::remove_file(&unsaved).is_ok(), "stale test staging file could not be removed");
        }
        BubblesModel::new(Vec::new(), unsaved.with_extension("saved.json"), unsaved, SharedCanvasSettings::default(), Some(gate))
    }

    fn saved_ids(path: &Path) -> Option<Vec<i64>> {
        let raw = std::fs::read_to_string(path).ok()?;
        let saved = serde_json::from_str::<Vec<Bubble>>(&raw).ok()?;
        Some(saved.iter().map(|bubble| bubble.id).collect())
    }

    /// With a gate, mutations are held (nothing written) until the gate is due, then the LATEST
    /// snapshot is written.
    #[test]
    fn gated_saver_holds_until_the_interval_elapses() {
        let mut model = gated_model(std::time::Duration::from_millis(400), 1000);
        let staging_path = model.unsaved_bubbles_path.clone();
        assert!(model.create_or_replace(bubble_with_extra(1, "k", "a")).is_ok());
        assert!(model.create_or_replace(bubble_with_extra(2, "k", "b")).is_ok());
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(!staging_path.exists(), "held snapshot written before the gate was due");
        let written = (0..500).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            saved_ids(&staging_path).is_some_and(|ids| ids == vec![1, 2])
        });
        assert!(written, "the held latest snapshot was never written");
        assert!(model.shutdown_saver().is_ok());
        assert!(std::fs::remove_file(staging_path).is_ok());
    }

    /// `BarrierAndHold` force-writes the held snapshot before acknowledging.
    #[test]
    fn barrier_and_hold_writes_the_held_snapshot() {
        let mut model = gated_model(std::time::Duration::from_secs(3600), 1000);
        let staging_path = model.unsaved_bubbles_path.clone();
        assert!(model.create_or_replace(bubble_with_extra(7, "k", "held")).is_ok());
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!staging_path.exists(), "held before the barrier");
        let Ok(barrier) = model.saver_handle().barrier_and_hold_blocking() else {
            panic!("barrier failed");
        };
        assert_eq!(saved_ids(&staging_path), Some(vec![7]), "barrier acknowledged before writing the held snapshot");
        drop(barrier);
        assert!(model.shutdown_saver().is_ok());
        assert!(std::fs::remove_file(staging_path).is_ok());
    }

    /// Shutdown writes a held snapshot; after a discard pause it is dropped instead.
    #[test]
    fn gated_shutdown_writes_held_unless_paused() {
        for paused in [false, true] {
            let mut model = gated_model(std::time::Duration::from_secs(3600), 1000);
            let staging_path = model.unsaved_bubbles_path.clone();
            assert!(model.create_or_replace(bubble_with_extra(3, "k", "held")).is_ok());
            std::thread::sleep(std::time::Duration::from_millis(50));
            if paused {
                let _snapshot = model.pause_saver_for_discard();
            }
            assert!(model.shutdown_saver().is_ok());
            if paused {
                assert!(!staging_path.exists(), "a paused shutdown wrote the held snapshot");
            } else {
                assert_eq!(saved_ids(&staging_path), Some(vec![3]), "shutdown lost the held snapshot");
                assert!(std::fs::remove_file(staging_path).is_ok());
            }
        }
    }
}
