/*
File: src/canvas/bubble_runtime.rs

Purpose:
Bubble runtime subsystem for `CanvasView`: mutable bubble state, shared-model sync,
undo/redo snapshots, clipboard flows, and pending write/delete queues.

Main responsibilities:
- own `BubbleRuntimeState`;
- apply bubble mutations outside of UI layout code;
- sync runtime bubbles with `BubblesModel` / project snapshot;
- keep undo/redo and internal whole-bubble clipboard consistent;
- log and preserve failed writes instead of silently dropping them.

Key structures:
- BubbleRuntimeState

Key functions:
- apply_machine_translation_result()
- flush_bubble_upserts_to_model()
- sync_runtime_from_model_or_project()
- apply_pending_actions()

Notes:
- This module intentionally excludes aside/on-top widget layout and scene drawing.
- Heavy filesystem persistence still happens through `BubblesModel` saver threads; GUI thread
  only mutates runtime state and shared model snapshots.
*/

use super::helpers::{
    bubble_fingerprint, bubbles_stamp, default_text_area_box, image_area_rect_from_bubble,
    normalize_image_text_areas, parse_image_text_areas, sanitize_clipboard_text,
    serialize_image_text_areas, side_to_string, upsert_rect_coords_into_extra,
};
use super::types::{
    AsideDragState, AsideItem, BubbleAction, BubbleCopyPasteTarget, BubbleHistoryEntry,
    BubbleTextField, CanvasContextMenuTarget, CopiedBubbleData, FocusedBubbleTextInput,
    ImageTextArea, OnTopDragState, PageBubbleBuckets, PendingBubblePaste, RectCoords,
    RuntimeBubble,
};
use super::{
    BUBBLE_HISTORY_LIMIT, BubbleClass, BubbleType, CanvasHooks, CanvasView,
    DUPLICATE_BUBBLE_OFFSET_PX, TEXT_UPSERT_DEBOUNCE_SECS, read_system_clipboard_text,
    rect_coords_from_bubble,
};
use crate::models::bubbles_model::{BubblesModel, SharedCanvasSettings, runtime_bubble_to_record};
use crate::project::{Bubble, ProjectData, Side};
use crate::runtime_log;
use eframe::egui;
use egui::{Pos2, Rect};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// The subset of egui input events the canvas reacts to in `capture_clipboard_events`.
///
/// Extracted from `ctx.input` by reference so the per-frame events vector is not cloned;
/// only the small paste payload is copied out. Project-owned, so matches stay exhaustive.
#[derive(Debug)]
enum ClipboardEvent {
    Copy,
    Cut,
    Paste(String),
}

/// One sortable aside/on-top entry: `(item, vertical anchor, horizontal anchor, bubble id)`.
///
/// The trailing three fields are the sort key (vertical, then horizontal anchor, then id for
/// stability) used to order a column top-to-bottom before the keys are dropped.
type PageBubbleEntry = (AsideItem, f32, f32, i64);

/// Single-pass accumulator for the four `(side, type)` aside/on-top columns of one page.
///
/// Entries are routed into per-column vectors during one scan of the runtime bubbles, then each
/// column is sorted and the sort keys are dropped by [`Self::finish`].
#[derive(Default)]
struct PageBubbleBucketAccumulator {
    aside_left: Vec<PageBubbleEntry>,
    aside_right: Vec<PageBubbleEntry>,
    on_top_left: Vec<PageBubbleEntry>,
    on_top_right: Vec<PageBubbleEntry>,
}

impl PageBubbleBucketAccumulator {
    /// Routes one entry into the column for its `(displayed_type, side)`.
    ///
    /// Displayed bubble types resolve to Aside or OnTop only; a `Default` would be dropped here,
    /// matching the old per-call filter that compared against a concrete Aside/OnTop request.
    fn push(&mut self, displayed_type: BubbleType, side: Side, entry: PageBubbleEntry) {
        match (displayed_type, side) {
            (BubbleType::Aside, Side::Left) => self.aside_left.push(entry),
            (BubbleType::Aside, Side::Right) => self.aside_right.push(entry),
            (BubbleType::OnTop, Side::Left) => self.on_top_left.push(entry),
            (BubbleType::OnTop, Side::Right) => self.on_top_right.push(entry),
            (BubbleType::Default, Side::Left | Side::Right) => {}
        }
    }

    /// Sorts each column top-to-bottom and returns the ordered, key-stripped buckets.
    fn finish(self) -> PageBubbleBuckets {
        PageBubbleBuckets {
            aside_left: sort_page_bubble_column(self.aside_left),
            aside_right: sort_page_bubble_column(self.aside_right),
            on_top_left: sort_page_bubble_column(self.on_top_left),
            on_top_right: sort_page_bubble_column(self.on_top_right),
        }
    }
}

/// Sorts one column by `(vertical anchor, horizontal anchor, bubble id)` and drops the sort keys.
fn sort_page_bubble_column(mut entries: Vec<PageBubbleEntry>) -> Vec<AsideItem> {
    entries.sort_by(|a, b| {
        a.1.total_cmp(&b.1)
            .then_with(|| a.2.total_cmp(&b.2))
            .then_with(|| a.3.cmp(&b.3))
    });
    entries.into_iter().map(|(item, ..)| item).collect()
}

pub(super) struct BubbleRuntimeState {
    pub(super) runtime_bubbles: HashMap<i64, RuntimeBubble>,
    pub(super) selected_bubble: Option<i64>,
    pub(super) move_active_bid: Option<i64>,
    pub(super) active_rect_handle: Option<(i64, usize)>,
    /// Active image-bubble text-area resize handle as `(bubble_id, area_idx, handle_idx)`.
    pub(super) active_area_handle: Option<(i64, usize, usize)>,
    pub(super) aside_drag_state: Option<AsideDragState>,
    pub(super) on_top_drag_state: Option<OnTopDragState>,
    pub(super) next_bubble_id: i64,
    pub(super) pending_delete: HashSet<i64>,
    pub(super) pending_translate: HashSet<i64>,
    pub(super) pending_upsert: HashSet<i64>,
    pub(super) pending_text_upsert: HashMap<i64, f64>,
    pub(super) copied_bubble_data: Option<CopiedBubbleData>,
    pub(super) project_sync_stamp: u64,
    pub(super) runtime_fingerprints: HashMap<i64, u64>,
    pub(super) focused_bubbles: HashSet<i64>,
    pub(super) focused_text_input: Option<FocusedBubbleTextInput>,
    pub(super) deferred_remote_bubbles: HashMap<i64, Bubble>,
    pub(super) deferred_remote_deletes: HashSet<i64>,
    pub(super) canvas_context_menu_target: Option<CanvasContextMenuTarget>,
    pub(super) bubble_context_menu_misspelled_word: Option<String>,
    pub(super) pending_bubble_paste: Option<PendingBubblePaste>,
    pub(super) bubble_undo_stack: Vec<BubbleHistoryEntry>,
    pub(super) bubble_redo_stack: Vec<BubbleHistoryEntry>,
    pub(super) synced_bubbles_revision: u64,
    pub(super) bubbles_model: Option<Arc<Mutex<BubblesModel>>>,
}

impl Default for BubbleRuntimeState {
    fn default() -> Self {
        Self {
            runtime_bubbles: HashMap::new(),
            selected_bubble: None,
            move_active_bid: None,
            active_rect_handle: None,
            active_area_handle: None,
            aside_drag_state: None,
            on_top_drag_state: None,
            next_bubble_id: 1,
            pending_delete: HashSet::new(),
            pending_translate: HashSet::new(),
            pending_upsert: HashSet::new(),
            pending_text_upsert: HashMap::new(),
            copied_bubble_data: None,
            project_sync_stamp: 0,
            runtime_fingerprints: HashMap::new(),
            focused_bubbles: HashSet::new(),
            focused_text_input: None,
            deferred_remote_bubbles: HashMap::new(),
            deferred_remote_deletes: HashSet::new(),
            canvas_context_menu_target: None,
            bubble_context_menu_misspelled_word: None,
            pending_bubble_paste: None,
            bubble_undo_stack: Vec::new(),
            bubble_redo_stack: Vec::new(),
            synced_bubbles_revision: 0,
            bubbles_model: None,
        }
    }
}

impl BubbleRuntimeState {
    /// Returns true while a continuous positional drag/resize gesture is in progress.
    ///
    /// These are the per-frame "follow the pointer" gestures (aside drag, on-top drag, the
    /// 8-point rect resize, and an image text-area resize). Positional model writes are debounced
    /// while one is active and committed when it ends; one-shot placement (`move_active_bid`) is
    /// not included because it writes once per click, not every frame.
    fn positional_drag_gesture_active(&self) -> bool {
        self.aside_drag_state.is_some()
            || self.on_top_drag_state.is_some()
            || self.active_rect_handle.is_some()
            || self.active_area_handle.is_some()
    }
}

impl CanvasView {
    pub fn set_bubbles_model(&mut self, model: Arc<Mutex<BubblesModel>>) {
        self.bubble_runtime.bubbles_model = Some(model);
        self.bubble_runtime.bubble_undo_stack.clear();
        self.bubble_runtime.bubble_redo_stack.clear();
    }

    pub fn delete_selected_bubble_shortcut(&mut self) -> bool {
        if !self.editable {
            return false;
        }
        let Some(bid) = self.bubble_runtime.selected_bubble else {
            return false;
        };
        self.bubble_runtime.pending_delete.insert(bid);
        true
    }

    pub fn is_bubble_move_mode_active(&self, bubble_id: i64) -> bool {
        self.bubble_runtime.move_active_bid == Some(bubble_id)
    }

    pub fn toggle_move_mode_for_bubble(&mut self, bubble_id: i64) -> bool {
        if !self.editable || !self.bubble_runtime.runtime_bubbles.contains_key(&bubble_id) {
            return false;
        }
        self.bubble_runtime.selected_bubble = Some(bubble_id);
        self.bubble_runtime.move_active_bid =
            if self.bubble_runtime.move_active_bid == Some(bubble_id) {
                None
            } else {
                Some(bubble_id)
            };
        true
    }

    pub fn request_delete_bubble(&mut self, bubble_id: i64) -> bool {
        if !self.editable || !self.bubble_runtime.runtime_bubbles.contains_key(&bubble_id) {
            return false;
        }
        self.bubble_runtime.selected_bubble = Some(bubble_id);
        self.bubble_runtime.pending_delete.insert(bubble_id);
        true
    }

    pub fn request_translate_bubble(&mut self, bubble_id: i64) -> bool {
        if !self.editable || !self.bubble_runtime.runtime_bubbles.contains_key(&bubble_id) {
            return false;
        }
        self.bubble_runtime.selected_bubble = Some(bubble_id);
        self.bubble_runtime.pending_translate.insert(bubble_id);
        true
    }

    pub fn set_bubble_texts_from_panel(
        &mut self,
        bubble_id: i64,
        text: Option<String>,
        original_text: Option<String>,
        now_s: f64,
        commit_now: bool,
    ) -> bool {
        let Some(bubble) = self.bubble_runtime.runtime_bubbles.get_mut(&bubble_id) else {
            return false;
        };

        let mut changed = false;
        if let Some(new_text) = text
            && bubble.text != new_text
        {
            bubble.text = new_text;
            changed = true;
        }
        if let Some(new_original_text) = original_text
            && bubble.original_text != new_original_text
        {
            bubble.original_text = new_original_text;
            changed = true;
        }

        if !changed {
            return true;
        }
        bubble.mounted = true;
        self.schedule_text_upsert(bubble_id, now_s);
        if commit_now {
            self.commit_text_upsert_now(bubble_id);
        }
        true
    }

    pub fn copy_bubble_text_to_clipboard(
        &mut self,
        ctx: &egui::Context,
        bubble_id: i64,
        field: BubbleTextField,
    ) -> bool {
        let Some(bubble) = self.bubble_runtime.runtime_bubbles.get(&bubble_id) else {
            return false;
        };
        let text = match field {
            BubbleTextField::Original => bubble.original_text.clone(),
            BubbleTextField::Translation => bubble.text.clone(),
        };
        ctx.copy_text(text);
        true
    }

    pub fn copy_selected_bubble_text_shortcut(
        &mut self,
        ctx: &egui::Context,
        field: BubbleTextField,
    ) -> bool {
        let Some(bubble_id) = self.bubble_runtime.selected_bubble else {
            return false;
        };
        self.copy_bubble_text_to_clipboard(ctx, bubble_id, field)
    }

    pub fn paste_bubble_text_from_clipboard(
        &mut self,
        ctx: &egui::Context,
        bubble_id: i64,
        field: BubbleTextField,
    ) -> Option<String> {
        if !self.editable || !self.bubble_runtime.runtime_bubbles.contains_key(&bubble_id) {
            return None;
        }
        let before = self
            .bubble_runtime
            .runtime_bubbles
            .get(&bubble_id)
            .map(|bubble| match field {
                BubbleTextField::Original => bubble.original_text.clone(),
                BubbleTextField::Translation => bubble.text.clone(),
            })
            .unwrap_or_default();
        let text = read_system_clipboard_text()?;
        self.apply_paste_text(bubble_id, field, text, ctx.input(|i| i.time));
        let after =
            self.bubble_runtime
                .runtime_bubbles
                .get(&bubble_id)
                .map(|bubble| match field {
                    BubbleTextField::Original => bubble.original_text.clone(),
                    BubbleTextField::Translation => bubble.text.clone(),
                })?;
        (before != after).then_some(after)
    }

    pub fn paste_selected_bubble_text_shortcut(
        &mut self,
        ctx: &egui::Context,
        field: BubbleTextField,
    ) -> Option<String> {
        let bubble_id = self.bubble_runtime.selected_bubble?;
        self.paste_bubble_text_from_clipboard(ctx, bubble_id, field)
    }

    pub fn create_bubble_at_pointer_shortcut(&mut self, pointer_pos: Pos2) -> bool {
        self.create_bubble_at_pointer(pointer_pos).is_some()
    }

    pub fn create_bubble_with_original_text_at_page_uv_rect(
        &mut self,
        page_idx: usize,
        uv_rect: [f32; 4],
        original_text: String,
    ) -> bool {
        if !self.editable {
            return false;
        }
        let mut rect_coords = RectCoords {
            p1: egui::pos2(uv_rect[0], uv_rect[1]),
            p2: egui::pos2(uv_rect[2], uv_rect[3]),
        }
        .normalized();
        rect_coords.p1.x = rect_coords.p1.x.clamp(0.0, 1.0);
        rect_coords.p1.y = rect_coords.p1.y.clamp(0.0, 1.0);
        rect_coords.p2.x = rect_coords.p2.x.clamp(0.0, 1.0);
        rect_coords.p2.y = rect_coords.p2.y.clamp(0.0, 1.0);

        let center = rect_coords.center_uv();
        let side = if center.x < 0.5 {
            Side::Left
        } else {
            Side::Right
        };
        let anchor_y = self
            .page_scene_rect(page_idx)
            .map(|rect| rect.top() + rect.height() * center.y)
            .unwrap_or(0.0);

        let id = self.bubble_runtime.next_bubble_id;
        self.bubble_runtime.next_bubble_id += 1;
        self.bubble_runtime.runtime_bubbles.insert(
            id,
            RuntimeBubble {
                id,
                img_idx: page_idx,
                img_u: center.x,
                img_v: center.y,
                side,
                bubble_class: BubbleClass::Text,
                bubble_type: BubbleType::Default,
                text: String::new(),
                original_text,
                rect_coords,
                anchor_y,
                max_width_px: self.state.bubble_min_width,
                height_px: 80.0,
                line_x: 0.0,
                mounted: false,
                text_areas: Vec::new(),
                image_block_rects: Vec::new(),
            },
        );
        self.bubble_runtime.pending_upsert.insert(id);
        self.bubble_runtime.selected_bubble = Some(id);
        true
    }

    fn create_bubble_at_pointer(&mut self, pointer_pos: Pos2) -> Option<i64> {
        if !self.editable {
            return None;
        }
        for (idx, rect) in self.scene.page_rects.iter().enumerate() {
            if rect.contains(pointer_pos) {
                return Some(self.create_bubble_at(idx, *rect, pointer_pos));
            }
        }
        None
    }

    pub fn flush_pending_bubble_upserts_now(&mut self, project: &ProjectData) {
        self.flush_bubble_upserts_to_model(project);
    }

    pub fn apply_machine_translation_result(
        &mut self,
        bubble_id: i64,
        translated_text: String,
    ) -> bool {
        // Image bubbles keep their text in `text_areas`; the persist/flush path derives the
        // canonical text from `text_areas[0]`, so a result written only to the legacy `text`
        // field would be overwritten by the stale area on the next flush. Route image bubbles
        // through the area-aware path, preserving the existing area-0 original.
        let image_area_original = self
            .bubble_runtime
            .runtime_bubbles
            .get(&bubble_id)
            .filter(|rt| rt.bubble_class == BubbleClass::Image && !rt.text_areas.is_empty())
            .map(|rt| rt.text_areas[0].original.clone());
        if let Some(original) = image_area_original {
            return self
                .apply_machine_translation_areas(bubble_id, vec![(original, translated_text)]);
        }
        let Some(rt) = self.bubble_runtime.runtime_bubbles.get_mut(&bubble_id) else {
            return false;
        };
        rt.text = translated_text.clone();
        rt.mounted = true;
        self.bubble_runtime.pending_text_upsert.remove(&bubble_id);
        self.bubble_runtime.pending_upsert.insert(bubble_id);

        let Some(model) = self.bubble_runtime.bubbles_model.as_ref().map(Arc::clone) else {
            return true;
        };

        self.capture_bubble_history_before_mutation();
        let update_result = {
            let Ok(mut locked) = model.lock() else {
                runtime_log::log_warn(format!(
                    "[canvas::bubble_runtime] failed to lock BubblesModel for translation result; bubble_id={bubble_id}"
                ));
                return false;
            };
            locked.update_translation_result_deferred_save(bubble_id, translated_text, "translated")
        };
        match update_result {
            Ok(Some((revision, save_task))) => {
                save_task.persist();
                self.bubble_runtime.synced_bubbles_revision = revision;
                self.bubble_runtime.pending_upsert.remove(&bubble_id);
                true
            }
            Ok(None) => true,
            Err(err) => {
                runtime_log::log_error(format!(
                    "[canvas::bubble_runtime] failed to persist translation result; bubble_id={bubble_id}; error={err:#}"
                ));
                false
            }
        }
    }

    pub fn apply_machine_translation_result_with_original(
        &mut self,
        bubble_id: i64,
        original_text: String,
        translated_text: String,
    ) -> bool {
        // Image bubbles store text per area in `text_areas`, and the persist/flush path reads the
        // canonical text from `text_areas[0]`. Writing only the legacy fields here would be
        // discarded on the next flush, so route single-area image results through the area-aware
        // path that updates `text_areas[0]` and mirrors it back to the legacy fields.
        let is_image_with_areas = self
            .bubble_runtime
            .runtime_bubbles
            .get(&bubble_id)
            .is_some_and(|rt| rt.bubble_class == BubbleClass::Image && !rt.text_areas.is_empty());
        if is_image_with_areas {
            return self.apply_machine_translation_areas(
                bubble_id,
                vec![(original_text, translated_text)],
            );
        }
        let Some(rt) = self.bubble_runtime.runtime_bubbles.get_mut(&bubble_id) else {
            return false;
        };
        rt.original_text = original_text.clone();
        rt.text = translated_text.clone();
        rt.mounted = true;
        self.bubble_runtime.pending_text_upsert.remove(&bubble_id);
        self.bubble_runtime.pending_upsert.insert(bubble_id);

        let Some(model) = self.bubble_runtime.bubbles_model.as_ref().map(Arc::clone) else {
            return true;
        };

        self.capture_bubble_history_before_mutation();
        let update_result = {
            let Ok(mut locked) = model.lock() else {
                runtime_log::log_warn(format!(
                    "[canvas::bubble_runtime] failed to lock BubblesModel for multimodal translation result; bubble_id={bubble_id}"
                ));
                return false;
            };
            locked.update_translation_and_original_deferred_save(
                bubble_id,
                original_text,
                translated_text,
                "translated",
            )
        };
        match update_result {
            Ok(Some((revision, save_task))) => {
                save_task.persist();
                self.bubble_runtime.synced_bubbles_revision = revision;
                self.bubble_runtime.pending_upsert.remove(&bubble_id);
                true
            }
            Ok(None) => true,
            Err(err) => {
                runtime_log::log_error(format!(
                    "[canvas::bubble_runtime] failed to persist multimodal translation result; bubble_id={bubble_id}; error={err:#}"
                ));
                false
            }
        }
    }

    /// Applies a per-area AI translation result to an image bubble.
    ///
    /// Used for both multi-area image bubbles and single-area ones (the legacy single-result
    /// apply paths delegate here, since image-bubble text is canonically stored in `text_areas`).
    /// `areas` holds `(original_text, translation)` in text-area order; entry `i` updates
    /// `text_areas[i]`. Area 0 is mirrored to the legacy fields. Returns `false` when the bubble is
    /// missing or has no text areas. Persistence is routed through the normal pending-upsert flush
    /// (which serializes `text_areas` and mirrors area 0), so the result is captured for undo and
    /// saved like any other area edit.
    pub fn apply_machine_translation_areas(
        &mut self,
        bubble_id: i64,
        areas: Vec<(String, String)>,
    ) -> bool {
        let Some(rt) = self.bubble_runtime.runtime_bubbles.get_mut(&bubble_id) else {
            return false;
        };
        if rt.text_areas.is_empty() || areas.is_empty() {
            return false;
        }
        for (idx, (original, translation)) in areas.iter().enumerate() {
            if let Some(area) = rt.text_areas.get_mut(idx) {
                area.original = original.clone();
                area.translation = translation.clone();
            }
        }
        if let Some(first) = rt.text_areas.first() {
            rt.original_text = first.original.clone();
            rt.text = first.translation.clone();
        }
        rt.mounted = true;
        self.bubble_runtime.pending_text_upsert.remove(&bubble_id);
        self.capture_bubble_history_before_mutation();
        self.bubble_runtime.pending_upsert.insert(bubble_id);
        true
    }

    pub fn patch_bubble_extra_fields(
        &mut self,
        project: &ProjectData,
        bubble_id: i64,
        patch: &Map<String, Value>,
    ) -> bool {
        if patch.is_empty() {
            return true;
        }
        let Some(model) = self.bubble_runtime.bubbles_model.as_ref().map(Arc::clone) else {
            return false;
        };
        let Some(rt) = self.bubble_runtime.runtime_bubbles.get(&bubble_id) else {
            return false;
        };
        let rt_id = rt.id;
        let rt_img_idx = rt.img_idx;
        let rt_img_u = rt.img_u;
        let rt_img_v = rt.img_v;
        let rt_side = rt.side;
        let rt_bubble_class = rt.bubble_class;
        let rt_bubble_type = rt.bubble_type;
        let rt_text = rt.text.clone();
        let rt_original_text = rt.original_text.clone();
        let rt_rect_coords = rt.rect_coords;
        let mut extra = project
            .bubbles
            .iter()
            .find(|bubble| bubble.id == bubble_id)
            .map(|bubble| bubble.extra.clone())
            .unwrap_or_default();
        upsert_rect_coords_into_extra(&mut extra, rt_rect_coords);
        let mut changed = false;
        for (key, value) in patch {
            if extra.get(key) != Some(value) {
                extra.insert(key.clone(), value.clone());
                changed = true;
            }
        }
        if !changed {
            return true;
        }
        self.capture_bubble_history_before_mutation();

        let rec = runtime_bubble_to_record(
            rt_id,
            rt_img_idx,
            rt_img_u,
            rt_img_v,
            Some(side_to_string(rt_side)),
            Some(rt_bubble_class.as_str().to_string()),
            Some(rt_bubble_type.as_str().to_string()),
            rt_text,
            rt_original_text,
            Some(extra),
        );

        // When the patch changes the image-area rect (e.g. a Shift+Q crop selection sets `crop_rect`
        // + `rect_coords`), the runtime rect must be updated too: this call advances the model
        // revision, so the regular model→runtime sync would not re-apply it and the red area would
        // keep its default size. Only react when the rect was actually patched, so unrelated footer
        // patches do not revert a stale crop rect over a freshly resized runtime rect.
        let patched_rect = if patch.contains_key("rect_coords") || patch.contains_key("crop_rect") {
            if rt_bubble_class == BubbleClass::Image {
                image_area_rect_from_bubble(&rec)
            } else {
                rect_coords_from_bubble(&rec)
            }
        } else {
            None
        };

        let Ok(mut locked) = model.lock() else {
            runtime_log::log_warn(format!(
                "[canvas::bubble_runtime] failed to lock BubblesModel for extra patch; bubble_id={bubble_id}"
            ));
            return false;
        };
        match locked.create_or_replace(rec) {
            Ok(()) => {
                self.bubble_runtime.synced_bubbles_revision = locked.revision();
                self.bubble_runtime.pending_upsert.remove(&bubble_id);
                self.bubble_runtime.pending_text_upsert.remove(&bubble_id);
                if let Some(rect) = patched_rect
                    && let Some(rt) = self.bubble_runtime.runtime_bubbles.get_mut(&bubble_id)
                {
                    let rect = rect.normalized();
                    rt.rect_coords = rect;
                    if rt.bubble_class == BubbleClass::Image {
                        normalize_image_text_areas(&mut rt.text_areas, rect);
                    }
                    // Height/anchor change with the new rect; re-measure on next layout.
                    rt.mounted = false;
                }
                true
            }
            Err(err) => {
                runtime_log::log_error(format!(
                    "[canvas::bubble_runtime] failed to persist bubble extra patch; bubble_id={bubble_id}; error={err:#}"
                ));
                false
            }
        }
    }

    /// Captures the pre-mutation bubble state for undo, exactly once per logical change.
    ///
    /// The snapshot is taken as a shared `Arc` (O(1)) together with the model revision and is
    /// deduplicated by revision: the model revision is monotonic and bumped on every mutation, so
    /// repeated captures while the model is unchanged (e.g. the per-frame flush calls during one
    /// continuous drag, which is debounced and writes the model only on release) collapse to a
    /// single undo entry. This replaces the former per-call full snapshot + `serde_json` content
    /// hash.
    pub(super) fn capture_bubble_history_before_mutation(&mut self) {
        let Some(model) = self.bubble_runtime.bubbles_model.as_ref().map(Arc::clone) else {
            return;
        };
        let (bubbles, revision) = {
            let Ok(locked) = model.lock() else {
                runtime_log::log_warn(
                    "[canvas::bubble_runtime] failed to lock BubblesModel for undo snapshot",
                );
                return;
            };
            (locked.snapshot_shared(), locked.revision())
        };
        self.push_bubble_undo_snapshot(bubbles, revision);
    }

    fn push_bubble_undo_snapshot(&mut self, bubbles: Arc<Vec<Bubble>>, revision: u64) {
        // Dedup by revision: a capture for a model state already on top of the stack is a no-op,
        // so a gesture that changed nothing (or repeated captures within one unchanged model
        // state) does not push a duplicate undo entry.
        if self
            .bubble_runtime
            .bubble_undo_stack
            .last()
            .is_some_and(|entry| entry.revision == revision)
        {
            return;
        }
        self.bubble_runtime
            .bubble_undo_stack
            .push(BubbleHistoryEntry { bubbles, revision });
        if self.bubble_runtime.bubble_undo_stack.len() > BUBBLE_HISTORY_LIMIT {
            let overflow = self.bubble_runtime.bubble_undo_stack.len() - BUBBLE_HISTORY_LIMIT;
            self.bubble_runtime.bubble_undo_stack.drain(0..overflow);
        }
        self.bubble_runtime.bubble_redo_stack.clear();
    }

    fn push_bubble_redo_snapshot(&mut self, bubbles: Arc<Vec<Bubble>>, revision: u64) {
        if self
            .bubble_runtime
            .bubble_redo_stack
            .last()
            .is_some_and(|entry| entry.revision == revision)
        {
            return;
        }
        self.bubble_runtime
            .bubble_redo_stack
            .push(BubbleHistoryEntry { bubbles, revision });
        if self.bubble_runtime.bubble_redo_stack.len() > BUBBLE_HISTORY_LIMIT {
            let overflow = self.bubble_runtime.bubble_redo_stack.len() - BUBBLE_HISTORY_LIMIT;
            self.bubble_runtime.bubble_redo_stack.drain(0..overflow);
        }
    }

    pub(super) fn try_undo_bubbles_history(&mut self) -> bool {
        let Some(model) = self.bubble_runtime.bubbles_model.as_ref().map(Arc::clone) else {
            return false;
        };
        let Some(target) = self.bubble_runtime.bubble_undo_stack.pop() else {
            return false;
        };
        let Ok(mut locked) = model.lock() else {
            self.bubble_runtime.bubble_undo_stack.push(target);
            runtime_log::log_warn(
                "[canvas::bubble_runtime] failed to lock BubblesModel for undo operation",
            );
            return false;
        };
        let current = locked.snapshot_shared();
        let current_revision = locked.revision();
        let mut redo_pushed = false;
        if self
            .bubble_runtime
            .bubble_redo_stack
            .last()
            .is_none_or(|entry| entry.revision != current_revision)
        {
            self.push_bubble_redo_snapshot(current, current_revision);
            redo_pushed = true;
        }
        if let Err(err) = locked.reset(target.bubbles.as_ref().clone()) {
            if redo_pushed {
                self.bubble_runtime.bubble_redo_stack.pop();
            }
            self.bubble_runtime.bubble_undo_stack.push(target);
            runtime_log::log_error(format!(
                "[canvas::bubble_runtime] failed to apply undo snapshot; error={err:#}"
            ));
            return false;
        }
        let revision = locked.revision();
        drop(locked);
        self.apply_bubbles_history_snapshot(&target.bubbles, revision);
        true
    }

    pub(super) fn try_redo_bubbles_history(&mut self) -> bool {
        let Some(model) = self.bubble_runtime.bubbles_model.as_ref().map(Arc::clone) else {
            return false;
        };
        let Some(target) = self.bubble_runtime.bubble_redo_stack.pop() else {
            return false;
        };
        let Ok(mut locked) = model.lock() else {
            self.bubble_runtime.bubble_redo_stack.push(target);
            runtime_log::log_warn(
                "[canvas::bubble_runtime] failed to lock BubblesModel for redo operation",
            );
            return false;
        };
        let current = locked.snapshot_shared();
        let current_revision = locked.revision();
        let mut undo_pushed = false;
        if self
            .bubble_runtime
            .bubble_undo_stack
            .last()
            .is_none_or(|entry| entry.revision != current_revision)
        {
            self.bubble_runtime
                .bubble_undo_stack
                .push(BubbleHistoryEntry {
                    bubbles: current,
                    revision: current_revision,
                });
            if self.bubble_runtime.bubble_undo_stack.len() > BUBBLE_HISTORY_LIMIT {
                let overflow = self.bubble_runtime.bubble_undo_stack.len() - BUBBLE_HISTORY_LIMIT;
                self.bubble_runtime.bubble_undo_stack.drain(0..overflow);
            }
            undo_pushed = true;
        }
        if let Err(err) = locked.reset(target.bubbles.as_ref().clone()) {
            if undo_pushed {
                self.bubble_runtime.bubble_undo_stack.pop();
            }
            self.bubble_runtime.bubble_redo_stack.push(target);
            runtime_log::log_error(format!(
                "[canvas::bubble_runtime] failed to apply redo snapshot; error={err:#}"
            ));
            return false;
        }
        let revision = locked.revision();
        drop(locked);
        self.apply_bubbles_history_snapshot(&target.bubbles, revision);
        true
    }

    fn apply_bubbles_history_snapshot(&mut self, bubbles: &[Bubble], revision: u64) {
        self.bubble_runtime.pending_delete.clear();
        self.bubble_runtime.pending_translate.clear();
        self.bubble_runtime.pending_upsert.clear();
        self.bubble_runtime.pending_text_upsert.clear();
        self.bubble_runtime.pending_bubble_paste = None;
        self.bubble_runtime.move_active_bid = None;
        self.bubble_runtime.active_rect_handle = None;
        self.bubble_runtime.aside_drag_state = None;
        self.bubble_runtime.on_top_drag_state = None;
        self.bubble_runtime.canvas_context_menu_target = None;
        self.bubble_runtime.focused_bubbles.clear();
        self.bubble_runtime.deferred_remote_bubbles.clear();
        self.bubble_runtime.deferred_remote_deletes.clear();
        self.scene.on_top_hit_rects.clear();
        let target_by_id: HashMap<i64, &Bubble> =
            bubbles.iter().map(|bubble| (bubble.id, bubble)).collect();
        let to_remove: Vec<i64> = self
            .bubble_runtime
            .runtime_bubbles
            .keys()
            .copied()
            .filter(|bid| !target_by_id.contains_key(bid))
            .collect();
        for bid in to_remove {
            self.remove_runtime_bubble(bid);
        }

        for bubble in bubbles {
            let fingerprint = bubble_fingerprint(bubble);
            let needs_upsert = self
                .bubble_runtime
                .runtime_fingerprints
                .get(&bubble.id)
                .copied()
                .map(|prev| prev != fingerprint)
                .unwrap_or(true)
                || !self.bubble_runtime.runtime_bubbles.contains_key(&bubble.id);
            if needs_upsert {
                self.upsert_runtime_from_bubble(bubble, fingerprint);
            }
        }
        let mut next_id = 1i64;
        for bubble in bubbles {
            next_id = next_id.max(bubble.id.saturating_add(1));
        }
        self.bubble_runtime.next_bubble_id = next_id;
        self.bubble_runtime.project_sync_stamp = bubbles_stamp(bubbles);
        self.bubble_runtime.synced_bubbles_revision = revision;
        if self
            .bubble_runtime
            .selected_bubble
            .is_some_and(|bid| !self.bubble_runtime.runtime_bubbles.contains_key(&bid))
        {
            self.bubble_runtime.selected_bubble = None;
        }
    }

    fn build_copied_bubble_data(
        &self,
        project: &ProjectData,
        bid: i64,
    ) -> Option<CopiedBubbleData> {
        let bubble = self.bubble_runtime.runtime_bubbles.get(&bid)?;
        Some(CopiedBubbleData {
            bubble_type: bubble.bubble_type,
            bubble_class: bubble.bubble_class,
            text: bubble.text.clone(),
            original_text: bubble.original_text.clone(),
            extra: self.bubble_extra_without_rect_coords(project, bid),
        })
    }

    pub(super) fn copy_whole_bubble_to_internal_buffer(
        &mut self,
        project: &ProjectData,
        bid: i64,
    ) -> bool {
        let Some(payload) = self.build_copied_bubble_data(project, bid) else {
            return false;
        };
        self.bubble_runtime.copied_bubble_data = Some(payload);
        true
    }

    fn apply_copied_bubble_data_to_bid(
        &mut self,
        project: &ProjectData,
        bid: i64,
        payload: &CopiedBubbleData,
        now_s: f64,
    ) -> bool {
        let Some(current_rt) = self.bubble_runtime.runtime_bubbles.get(&bid).cloned() else {
            return false;
        };
        let bubble_type_changed = current_rt.bubble_type != payload.bubble_type;
        let text_changed =
            current_rt.text != payload.text || current_rt.original_text != payload.original_text;
        let extra_changed = self.bubble_extra_without_rect_coords(project, bid) != payload.extra;
        if !bubble_type_changed && !text_changed && !extra_changed {
            return false;
        }

        if let Some(rt) = self.bubble_runtime.runtime_bubbles.get_mut(&bid) {
            rt.bubble_type = payload.bubble_type;
            rt.bubble_class = payload.bubble_class;
            rt.text = payload.text.clone();
            rt.original_text = payload.original_text.clone();
            rt.mounted = true;
        }

        let Some(updated_rt) = self.bubble_runtime.runtime_bubbles.get(&bid).cloned() else {
            return false;
        };
        let mut extra = payload.extra.clone();
        upsert_rect_coords_into_extra(&mut extra, updated_rt.rect_coords);

        if let Some(model) = self.bubble_runtime.bubbles_model.as_ref().map(Arc::clone) {
            self.capture_bubble_history_before_mutation();
            match model.lock() {
                Ok(mut locked) => {
                    let rec = runtime_bubble_to_record(
                        updated_rt.id,
                        updated_rt.img_idx,
                        updated_rt.img_u,
                        updated_rt.img_v,
                        Some(side_to_string(updated_rt.side)),
                        Some(updated_rt.bubble_class.as_str().to_string()),
                        Some(updated_rt.bubble_type.as_str().to_string()),
                        updated_rt.text.clone(),
                        updated_rt.original_text.clone(),
                        Some(extra),
                    );
                    match locked.create_or_replace(rec) {
                        Ok(()) => {
                            self.bubble_runtime.synced_bubbles_revision = locked.revision();
                            self.bubble_runtime.pending_upsert.remove(&bid);
                            self.bubble_runtime.pending_text_upsert.remove(&bid);
                            return true;
                        }
                        Err(err) => {
                            runtime_log::log_error(format!(
                                "[canvas::bubble_runtime] failed to persist copied bubble payload; bubble_id={bid}; error={err:#}"
                            ));
                        }
                    }
                }
                Err(_) => runtime_log::log_warn(format!(
                    "[canvas::bubble_runtime] failed to lock BubblesModel while applying copied bubble payload; bubble_id={bid}"
                )),
            }
        }

        if text_changed || bubble_type_changed {
            self.schedule_text_upsert(bid, now_s);
            self.commit_text_upsert_now(bid);
        }
        if extra_changed {
            self.bubble_runtime.pending_upsert.insert(bid);
        }
        true
    }

    pub(super) fn paste_copied_whole_bubble_into_bid(
        &mut self,
        project: &ProjectData,
        bid: i64,
        now_s: f64,
    ) -> bool {
        let Some(payload) = self.bubble_runtime.copied_bubble_data.clone() else {
            return false;
        };
        self.apply_copied_bubble_data_to_bid(project, bid, &payload, now_s)
    }

    fn paste_copied_whole_bubble_into_focused_or_create(
        &mut self,
        project: &ProjectData,
        pointer_pos: Option<Pos2>,
        now_s: f64,
    ) -> bool {
        if !self.editable || self.bubble_runtime.copied_bubble_data.is_none() {
            return false;
        }
        if let Some(bid) = self
            .bubble_runtime
            .selected_bubble
            .filter(|bid| self.bubble_runtime.runtime_bubbles.contains_key(bid))
        {
            return self.paste_copied_whole_bubble_into_bid(project, bid, now_s);
        }
        let Some(pos) = pointer_pos else {
            return false;
        };
        let Some(new_bid) = self.create_bubble_at_pointer(pos) else {
            return false;
        };
        self.paste_copied_whole_bubble_into_bid(project, new_bid, now_s)
    }

    pub(super) fn duplicate_bubble_below(
        &mut self,
        project: &ProjectData,
        bid: i64,
        now_s: f64,
    ) -> bool {
        if !self.editable {
            return false;
        }
        let Some(source) = self.bubble_runtime.runtime_bubbles.get(&bid).cloned() else {
            return false;
        };
        let Some(page_rect) = self.page_scene_rect(source.img_idx) else {
            return false;
        };
        let Some(payload) = self.build_copied_bubble_data(project, bid) else {
            return false;
        };
        self.bubble_runtime.copied_bubble_data = Some(payload.clone());

        let src_x = page_rect.left() + page_rect.width() * source.img_u.clamp(0.0, 1.0);
        let src_y = page_rect.top() + page_rect.height() * source.img_v.clamp(0.0, 1.0);
        let scene_pos = egui::pos2(
            src_x.clamp(page_rect.left(), page_rect.right()),
            (src_y + DUPLICATE_BUBBLE_OFFSET_PX).clamp(page_rect.top(), page_rect.bottom()),
        );
        let new_bid = self.create_bubble_at(source.img_idx, page_rect, scene_pos);
        self.bubble_runtime.selected_bubble = Some(new_bid);
        self.apply_copied_bubble_data_to_bid(project, new_bid, &payload, now_s)
    }

    pub(super) fn duplicate_focused_bubble_shortcut(
        &mut self,
        project: &ProjectData,
        now_s: f64,
    ) -> bool {
        let Some(bid) = self.bubble_runtime.selected_bubble else {
            return false;
        };
        self.duplicate_bubble_below(project, bid, now_s)
    }

    fn copy_from_focused_bubble_shortcut(&mut self, project: &ProjectData) -> bool {
        let Some(bid) = self.bubble_runtime.selected_bubble else {
            return false;
        };
        self.copy_whole_bubble_to_internal_buffer(project, bid)
    }

    fn cut_focused_bubble_shortcut(&mut self, project: &ProjectData) -> bool {
        if !self.editable {
            return false;
        }
        let Some(bid) = self.bubble_runtime.selected_bubble else {
            return false;
        };
        if !self.bubble_runtime.runtime_bubbles.contains_key(&bid) {
            return false;
        }
        if !self.copy_whole_bubble_to_internal_buffer(project, bid) {
            return false;
        }
        self.bubble_runtime.pending_delete.insert(bid);
        true
    }

    fn copy_focused_text_input_shortcut(
        &mut self,
        ctx: &egui::Context,
        focused: FocusedBubbleTextInput,
    ) -> bool {
        let Some(bubble) = self.bubble_runtime.runtime_bubbles.get(&focused.bid) else {
            return false;
        };
        let text = match focused.field {
            BubbleTextField::Original => bubble.original_text.clone(),
            BubbleTextField::Translation => bubble.text.clone(),
        };
        ctx.copy_text(text);
        true
    }

    pub(super) fn note_focused_bubble_text_input(
        &mut self,
        ctx: &egui::Context,
        bid: i64,
        field: BubbleTextField,
        response: &egui::Response,
    ) {
        if !response.has_focus() {
            return;
        }
        let has_selection = egui::TextEdit::load_state(ctx, response.id)
            .and_then(|state| state.cursor.char_range())
            .is_some_and(|range| !range.is_empty());
        self.bubble_runtime.focused_text_input = Some(FocusedBubbleTextInput {
            bid,
            field,
            has_selection,
        });
    }

    fn paste_into_focused_bubble_or_create_shortcut(
        &mut self,
        project: &ProjectData,
        pointer_pos: Option<Pos2>,
        now_s: f64,
    ) -> bool {
        self.paste_copied_whole_bubble_into_focused_or_create(project, pointer_pos, now_s)
    }

    pub(super) fn capture_clipboard_events(&mut self, project: &ProjectData, ctx: &egui::Context) {
        let keyboard_input_active = ctx.wants_keyboard_input();
        // Inspect events by reference inside the input closure and copy out only the few
        // clipboard variants we act on (Copy/Cut and the small paste payload), instead of
        // cloning the whole per-frame events vector (key/pointer/text events included).
        let clipboard_events: Vec<ClipboardEvent> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|ev| match ev {
                    egui::Event::Copy => Some(ClipboardEvent::Copy),
                    egui::Event::Cut => Some(ClipboardEvent::Cut),
                    egui::Event::Paste(text) => Some(ClipboardEvent::Paste(text.clone())),
                    _ => None,
                })
                .collect()
        });
        for ev in clipboard_events {
            match ev {
                ClipboardEvent::Copy => {
                    if let Some(focused) = self.bubble_runtime.focused_text_input {
                        if focused.has_selection {
                            continue;
                        }
                        if !self.copy_focused_text_input_shortcut(ctx, focused) {
                            runtime_log::log_warn(format!(
                                "[canvas::bubble_runtime] failed to copy focused bubble text; bubble_id={}",
                                focused.bid
                            ));
                        }
                        continue;
                    }
                    if keyboard_input_active {
                        continue;
                    }
                    if !self.copy_from_focused_bubble_shortcut(project)
                        && self.bubble_runtime.selected_bubble.is_some()
                    {
                        runtime_log::log_warn(
                            "[canvas::bubble_runtime] failed to copy focused bubble payload",
                        );
                    }
                }
                ClipboardEvent::Cut => {
                    if self.bubble_runtime.focused_text_input.is_some() || keyboard_input_active {
                        continue;
                    }
                    if !self.cut_focused_bubble_shortcut(project)
                        && self.bubble_runtime.selected_bubble.is_some()
                    {
                        runtime_log::log_warn(
                            "[canvas::bubble_runtime] failed to cut focused bubble payload",
                        );
                    }
                }
                ClipboardEvent::Paste(mut text) => {
                    text = sanitize_clipboard_text(&text);
                    if let Some(pending) = self.bubble_runtime.pending_bubble_paste.take() {
                        let now_s = ctx.input(|i| i.time);
                        self.apply_paste_text(pending.bid, pending.field, text, now_s);
                        continue;
                    }
                    if self.bubble_runtime.focused_text_input.is_some() || keyboard_input_active {
                        continue;
                    }
                    if !self.editable {
                        continue;
                    }
                    let now_s = ctx.input(|i| i.time);
                    if !self.paste_into_focused_bubble_or_create_shortcut(
                        project,
                        ctx.pointer_latest_pos(),
                        now_s,
                    ) {
                        runtime_log::log_warn(
                            "[canvas::bubble_runtime] failed to paste copied bubble into focused bubble or create a new one",
                        );
                    }
                }
            }
        }
    }

    pub(super) fn request_paste_from_clipboard(
        &mut self,
        ctx: &egui::Context,
        bid: i64,
        field: BubbleTextField,
    ) {
        self.bubble_runtime.pending_bubble_paste = Some(PendingBubblePaste { bid, field });
        ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
    }

    fn apply_paste_text(&mut self, bid: i64, field: BubbleTextField, text: String, now_s: f64) {
        let Some(bubble) = self.bubble_runtime.runtime_bubbles.get_mut(&bid) else {
            return;
        };
        let changed = match field {
            BubbleTextField::Original => {
                if bubble.original_text == text {
                    false
                } else {
                    bubble.original_text = text;
                    true
                }
            }
            BubbleTextField::Translation => {
                if bubble.text == text {
                    false
                } else {
                    bubble.text = text;
                    true
                }
            }
        };
        if !changed {
            return;
        }
        bubble.mounted = true;
        self.schedule_text_upsert(bid, now_s);
        self.commit_text_upsert_now(bid);
    }

    pub(super) fn sync_runtime_from_model_or_project(&mut self, project: &ProjectData) {
        if let Some(model) = self.bubble_runtime.bubbles_model.clone() {
            let mut model_bubbles: Option<(u64, Vec<Bubble>)> = None;
            let mut model_canvas: Option<(u64, SharedCanvasSettings)> = None;
            match model.lock() {
                Ok(locked) => {
                    let bubbles_revision = locked.revision();
                    if bubbles_revision != self.bubble_runtime.synced_bubbles_revision
                        || self.bubble_runtime.runtime_bubbles.is_empty()
                    {
                        model_bubbles = Some((bubbles_revision, locked.snapshot()));
                    }
                    let canvas_revision = locked.canvas_revision();
                    if canvas_revision != self.settings_runtime.synced_canvas_revision {
                        model_canvas = Some((canvas_revision, locked.canvas_snapshot()));
                    }
                }
                Err(_) => {
                    runtime_log::log_warn(
                        "[canvas::bubble_runtime] failed to lock BubblesModel during runtime sync; falling back to project bubbles",
                    );
                    self.sync_runtime_from_bubbles(project.bubbles.as_slice());
                    return;
                }
            }
            if let Some((revision, bubbles)) = model_bubbles {
                self.sync_runtime_from_bubbles(&bubbles);
                self.bubble_runtime.synced_bubbles_revision = revision;
            }
            if let Some((revision, canvas)) = model_canvas {
                self.apply_canvas_snapshot(&canvas);
                self.settings_runtime.synced_canvas_revision = revision;
            }
            return;
        }
        self.sync_runtime_from_bubbles(project.bubbles.as_slice());
    }

    fn sync_runtime_from_bubbles(&mut self, bubbles: &[Bubble]) {
        let stamp = bubbles_stamp(bubbles);
        if self.bubble_runtime.project_sync_stamp == stamp
            && !self.bubble_runtime.runtime_bubbles.is_empty()
        {
            return;
        }
        let mut seen = HashSet::with_capacity(bubbles.len());
        let mut next_bubble_id = 1;
        for bubble in bubbles {
            seen.insert(bubble.id);
            self.bubble_runtime
                .deferred_remote_deletes
                .remove(&bubble.id);
            next_bubble_id = next_bubble_id.max(bubble.id + 1);
            let fingerprint = bubble_fingerprint(bubble);
            if self.is_bubble_locally_locked(bubble.id) {
                let should_defer = self
                    .bubble_runtime
                    .runtime_fingerprints
                    .get(&bubble.id)
                    .copied()
                    .map(|f| f != fingerprint)
                    .unwrap_or(true);
                if should_defer {
                    self.bubble_runtime
                        .deferred_remote_bubbles
                        .insert(bubble.id, bubble.clone());
                }
                continue;
            }
            self.bubble_runtime
                .deferred_remote_bubbles
                .remove(&bubble.id);
            let unchanged = self
                .bubble_runtime
                .runtime_fingerprints
                .get(&bubble.id)
                .copied()
                .map(|f| f == fingerprint)
                .unwrap_or(false);
            if unchanged && self.bubble_runtime.runtime_bubbles.contains_key(&bubble.id) {
                continue;
            }
            self.upsert_runtime_from_bubble(bubble, fingerprint);
        }

        let removed: Vec<i64> = self
            .bubble_runtime
            .runtime_bubbles
            .keys()
            .copied()
            .filter(|id| !seen.contains(id))
            .collect();
        for bid in removed {
            if self.is_bubble_locally_locked(bid) {
                self.bubble_runtime.deferred_remote_deletes.insert(bid);
                self.bubble_runtime.deferred_remote_bubbles.remove(&bid);
                continue;
            }
            self.remove_runtime_bubble(bid);
        }

        self.bubble_runtime.next_bubble_id = next_bubble_id;
        self.bubble_runtime.project_sync_stamp = stamp;
    }

    pub(super) fn apply_deferred_remote_updates(&mut self) {
        if !self.bubble_runtime.deferred_remote_bubbles.is_empty() {
            let mut ready = Vec::new();
            for bid in self.bubble_runtime.deferred_remote_bubbles.keys().copied() {
                if !self.is_bubble_locally_locked(bid) {
                    ready.push(bid);
                }
            }
            for bid in ready {
                if let Some(bubble) = self.bubble_runtime.deferred_remote_bubbles.remove(&bid) {
                    let fingerprint = bubble_fingerprint(&bubble);
                    self.upsert_runtime_from_bubble(&bubble, fingerprint);
                }
            }
        }

        if !self.bubble_runtime.deferred_remote_deletes.is_empty() {
            let mut ready = Vec::new();
            for bid in self.bubble_runtime.deferred_remote_deletes.iter().copied() {
                if !self.is_bubble_locally_locked(bid) {
                    ready.push(bid);
                }
            }
            for bid in ready {
                self.bubble_runtime.deferred_remote_deletes.remove(&bid);
                self.remove_runtime_bubble(bid);
            }
        }
    }

    fn upsert_runtime_from_bubble(&mut self, bubble: &Bubble, fingerprint: u64) {
        let side = super::bubble_side(bubble);
        let bubble_type = self.effective_bubble_type_for_record(bubble);
        let bubble_class = self.effective_bubble_class_for_record(bubble);
        let new_u = bubble.img_u.clamp(0.0, 1.0);
        let new_v = bubble.img_v.clamp(0.0, 1.0);
        let (min_margin_u, min_margin_v) = self.bubble_min_uv_margin_for_page(bubble.img_idx);
        // For image bubbles the single red rect is the image area (the page-crop region for
        // page-crop bubbles); text bubbles use their plain `rect_coords`.
        let coords_from_record = if bubble_class == BubbleClass::Image {
            image_area_rect_from_bubble(bubble)
        } else {
            rect_coords_from_bubble(bubble)
        };
        // Image bubbles carry their own text areas; text bubbles keep this empty.
        let text_areas = parse_image_text_areas(bubble);
        if let Some(existing) = self.bubble_runtime.runtime_bubbles.get_mut(&bubble.id) {
            let du = new_u - existing.img_u;
            let dv = new_v - existing.img_v;
            existing.img_idx = bubble.img_idx;
            existing.side = side;
            existing.bubble_class = bubble_class;
            existing.bubble_type = bubble_type;
            existing.text = bubble.text.clone();
            existing.original_text = bubble.original_text.clone();
            if let Some(coords) = coords_from_record {
                existing.rect_coords = coords.normalized();
            } else if du.abs() > f32::EPSILON || dv.abs() > f32::EPSILON {
                let rc = existing.rect_coords;
                existing.rect_coords = RectCoords {
                    p1: egui::pos2(
                        (rc.p1.x + du).clamp(0.0, 1.0),
                        (rc.p1.y + dv).clamp(0.0, 1.0),
                    ),
                    p2: egui::pos2(
                        (rc.p2.x + du).clamp(0.0, 1.0),
                        (rc.p2.y + dv).clamp(0.0, 1.0),
                    ),
                }
                .normalized();
            }
            let anchor = Self::clamp_anchor_to_rect(
                new_u,
                new_v,
                existing.rect_coords,
                min_margin_u,
                min_margin_v,
            );
            existing.img_u = anchor.x;
            existing.img_v = anchor.y;
            existing.text_areas = text_areas;
        } else {
            let rect_coords = coords_from_record.unwrap_or_else(|| {
                self.default_rect_coords_for_page_idx(bubble.img_idx, new_u, new_v)
            });
            let rect_coords = rect_coords.normalized();
            let anchor =
                Self::clamp_anchor_to_rect(new_u, new_v, rect_coords, min_margin_u, min_margin_v);
            self.bubble_runtime.runtime_bubbles.insert(
                bubble.id,
                RuntimeBubble {
                    id: bubble.id,
                    img_idx: bubble.img_idx,
                    img_u: anchor.x,
                    img_v: anchor.y,
                    side,
                    bubble_class,
                    bubble_type,
                    text: bubble.text.clone(),
                    original_text: bubble.original_text.clone(),
                    rect_coords,
                    anchor_y: 0.0,
                    max_width_px: self.state.bubble_min_width,
                    height_px: 80.0,
                    line_x: 0.0,
                    mounted: false,
                    text_areas,
                    image_block_rects: Vec::new(),
                },
            );
        }
        self.bubble_runtime
            .runtime_fingerprints
            .insert(bubble.id, fingerprint);
    }

    /// Buckets every runtime bubble on `page_idx` into the four aside/on-top columns in one pass.
    ///
    /// Each column is ordered by vertical anchor, then horizontal anchor, then bubble id (for a
    /// stable order), with read-only image-area splitting: a read-only image bubble contributes one
    /// item per text area routed to the side of that area's own anchor, while text bubbles and
    /// editable image bubbles contribute one item (`area_idx = None`). This is the single full scan
    /// of the page's runtime bubbles per frame; callers read the column they need via
    /// [`PageBubbleBuckets::bucket`].
    pub(super) fn page_bubbles_bucketed(&self, page_idx: usize) -> PageBubbleBuckets {
        let mut acc = PageBubbleBucketAccumulator::default();
        for b in self.bubble_runtime.runtime_bubbles.values() {
            if b.img_idx != page_idx {
                continue;
            }
            let displayed_type = self.displayed_bubble_type_for_runtime(b);
            if !self.editable && b.bubble_class == BubbleClass::Image && !b.text_areas.is_empty() {
                // Read-only image bubble: split into one item per text area, routed to the side of
                // that area's own anchor.
                for (idx, area) in b.text_areas.iter().enumerate() {
                    let area_side = if area.anchor.x < 0.5 {
                        Side::Left
                    } else {
                        Side::Right
                    };
                    acc.push(
                        displayed_type,
                        area_side,
                        (
                            AsideItem {
                                bid: b.id,
                                area_idx: Some(idx),
                            },
                            area.anchor.y,
                            area.anchor.x,
                            b.id,
                        ),
                    );
                }
            } else {
                acc.push(
                    displayed_type,
                    b.side,
                    (
                        AsideItem {
                            bid: b.id,
                            area_idx: None,
                        },
                        b.img_v,
                        b.img_u,
                        b.id,
                    ),
                );
            }
        }
        acc.finish()
    }

    pub(super) fn apply_pending_actions(&mut self, hooks: &mut dyn CanvasHooks) {
        let mut pending_translate: Vec<i64> =
            self.bubble_runtime.pending_translate.drain().collect();
        pending_translate.sort_unstable();
        for bid in pending_translate {
            hooks.on_bubble_action(BubbleAction::Translate, bid);
        }

        if !self.bubble_runtime.pending_delete.is_empty()
            && self.bubble_runtime.bubbles_model.is_some()
        {
            self.capture_bubble_history_before_mutation();
        }

        let mut pending_delete: Vec<i64> = self.bubble_runtime.pending_delete.drain().collect();
        pending_delete.sort_unstable();
        for bid in pending_delete {
            if let Some(model) = &self.bubble_runtime.bubbles_model {
                let delete_result = match model.lock() {
                    Ok(mut locked) => {
                        let result = locked.delete(bid);
                        if result.is_ok() {
                            self.bubble_runtime.synced_bubbles_revision = locked.revision();
                        }
                        result
                    }
                    Err(_) => {
                        runtime_log::log_warn(format!(
                            "[canvas::bubble_runtime] failed to lock BubblesModel for delete; bubble_id={bid}"
                        ));
                        self.bubble_runtime.pending_delete.insert(bid);
                        continue;
                    }
                };
                if let Err(err) = delete_result {
                    runtime_log::log_error(format!(
                        "[canvas::bubble_runtime] failed to delete bubble from model; bubble_id={bid}; error={err:#}"
                    ));
                    self.bubble_runtime.pending_delete.insert(bid);
                    continue;
                }
            }
            self.remove_runtime_bubble(bid);
            hooks.on_bubble_action(BubbleAction::Delete, bid);
        }
    }

    pub(super) fn schedule_text_upsert(&mut self, bid: i64, now_s: f64) {
        self.bubble_runtime.pending_text_upsert.insert(bid, now_s);
    }

    pub(super) fn commit_text_upsert_now(&mut self, bid: i64) {
        self.bubble_runtime.pending_text_upsert.remove(&bid);
        self.bubble_runtime.pending_upsert.insert(bid);
    }

    pub(super) fn promote_debounced_text_upserts(&mut self, now_s: f64) {
        if self.bubble_runtime.pending_text_upsert.is_empty() {
            return;
        }
        let ready: Vec<i64> = self
            .bubble_runtime
            .pending_text_upsert
            .iter()
            .filter_map(|(bid, changed_at)| {
                if now_s - *changed_at >= TEXT_UPSERT_DEBOUNCE_SECS {
                    Some(*bid)
                } else {
                    None
                }
            })
            .collect();
        for bid in ready {
            self.commit_text_upsert_now(bid);
        }
    }

    pub(super) fn flush_bubble_upserts_to_model(&mut self, project: &ProjectData) {
        let Some(model) = self.bubble_runtime.bubbles_model.as_ref().map(Arc::clone) else {
            return;
        };
        if self.bubble_runtime.pending_upsert.is_empty() {
            return;
        }
        // Debounce positional model writes during a continuous drag/resize: the runtime bubble
        // already follows the pointer this frame (visual state is live), but committing every frame
        // would run `create_or_replace` -> `Arc::make_mut` (full Vec deep clone) + a save snapshot
        // per frame. The pending ids stay queued; the gesture-end handlers re-insert the dragged id
        // into `pending_upsert`, so the final position is committed on release. Text upserts use
        // their own time-based debounce and do not overlap a drag in practice.
        if self.bubble_runtime.positional_drag_gesture_active() {
            return;
        }
        let mut pending: Vec<i64> = self.bubble_runtime.pending_upsert.iter().copied().collect();
        pending.sort_unstable();
        let will_flush = pending.iter().any(|bid| {
            !self.is_bubble_locally_locked(*bid)
                && self.bubble_runtime.runtime_bubbles.contains_key(bid)
        });
        if !will_flush {
            return;
        }
        self.capture_bubble_history_before_mutation();
        let Ok(mut locked) = model.lock() else {
            runtime_log::log_warn(
                "[canvas::bubble_runtime] failed to lock BubblesModel for pending upsert flush",
            );
            return;
        };
        let mut had_success = false;
        for bid in pending {
            if self.is_bubble_locally_locked(bid) {
                continue;
            }
            let Some(rt) = self.bubble_runtime.runtime_bubbles.get(&bid) else {
                self.bubble_runtime.pending_upsert.remove(&bid);
                continue;
            };
            // Read only this bubble's `extra` from the model by id (no full snapshot clone);
            // fall back to the loaded project bubble when the model has not seen this id yet.
            let extra = locked.extra_of(bid).cloned().or_else(|| {
                project
                    .bubbles
                    .iter()
                    .find(|b| b.id == bid)
                    .map(|b| b.extra.clone())
            });
            let mut extra = extra.unwrap_or_default();
            upsert_rect_coords_into_extra(&mut extra, rt.rect_coords);
            // For image bubbles persist all text areas; area 0's text is mirrored to the legacy
            // record fields (and extra.description) so it stays the single canonical primary.
            let (record_text, record_original) =
                if rt.bubble_class == BubbleClass::Image && !rt.text_areas.is_empty() {
                    extra.insert(
                        "text_areas".to_string(),
                        serialize_image_text_areas(&rt.text_areas),
                    );
                    // The image area rect (red) lives in `rect_coords`; for page-crop bubbles keep the
                    // crop region equal to it so the loader/preview crop matches the canvas red rect.
                    if extra
                        .get("image_source_type")
                        .and_then(Value::as_str)
                        .unwrap_or("external")
                        == "page_crop"
                    {
                        let coords = rt.rect_coords.normalized();
                        extra.insert(
                            "crop_rect".to_string(),
                            Value::Array(vec![
                                Value::from(f64::from(coords.p1.x)),
                                Value::from(f64::from(coords.p1.y)),
                                Value::from(f64::from(coords.p2.x)),
                                Value::from(f64::from(coords.p2.y)),
                            ]),
                        );
                    }
                    // SAFETY (index): this arm is only entered when `!rt.text_areas.is_empty()`
                    // (the `if` guard above), so index 0 is always in bounds.
                    let first = &rt.text_areas[0];
                    extra.insert(
                        "description".to_string(),
                        Value::String(first.description.clone()),
                    );
                    (first.translation.clone(), first.original.clone())
                } else {
                    (rt.text.clone(), rt.original_text.clone())
                };
            let rec = runtime_bubble_to_record(
                rt.id,
                rt.img_idx,
                rt.img_u,
                rt.img_v,
                Some(side_to_string(rt.side)),
                Some(rt.bubble_class.as_str().to_string()),
                Some(rt.bubble_type.as_str().to_string()),
                record_text,
                record_original,
                Some(extra),
            );
            match locked.create_or_replace(rec) {
                Ok(()) => {
                    had_success = true;
                    self.bubble_runtime.pending_text_upsert.remove(&bid);
                    self.bubble_runtime.pending_upsert.remove(&bid);
                }
                Err(err) => runtime_log::log_error(format!(
                    "[canvas::bubble_runtime] failed to flush bubble upsert; bubble_id={bid}; error={err:#}"
                )),
            }
        }
        if had_success {
            self.bubble_runtime.synced_bubbles_revision = locked.revision();
        }
    }

    pub(super) fn create_bubble_from_canvas_context_menu(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        paste_target: Option<BubbleCopyPasteTarget>,
    ) -> bool {
        let Some(target) = self.bubble_runtime.canvas_context_menu_target else {
            return false;
        };
        let Some(page_rect) = self
            .scene
            .page_rects
            .get(target.page_idx)
            .copied()
            .filter(|rect| rect.is_positive())
        else {
            return false;
        };
        let scene_pos = egui::pos2(
            page_rect.left() + page_rect.width() * target.page_uv.x.clamp(0.0, 1.0),
            page_rect.top() + page_rect.height() * target.page_uv.y.clamp(0.0, 1.0),
        );
        let new_bid = self.create_bubble_at(target.page_idx, page_rect, scene_pos);
        if let Some(field) = paste_target.and_then(BubbleCopyPasteTarget::as_text_field) {
            self.request_paste_from_clipboard(ctx, new_bid, field);
        } else if paste_target == Some(BubbleCopyPasteTarget::WholeBubble)
            && !self.paste_copied_whole_bubble_into_bid(project, new_bid, ctx.input(|i| i.time))
        {
            runtime_log::log_warn(format!(
                "[canvas::bubble_runtime] failed to apply copied bubble payload to new bubble from context menu; bubble_id={new_bid}"
            ));
        }
        true
    }

    pub fn create_image_bubble_from_canvas_context_menu(
        &mut self,
        ctx: &egui::Context,
        _project: &ProjectData,
    ) -> bool {
        let Some(target) = self.bubble_runtime.canvas_context_menu_target else {
            return false;
        };
        let Some(page_rect) = self
            .scene
            .page_rects
            .get(target.page_idx)
            .copied()
            .filter(|rect| rect.is_positive())
        else {
            return false;
        };
        let scene_pos = egui::pos2(
            page_rect.left() + page_rect.width() * target.page_uv.x.clamp(0.0, 1.0),
            page_rect.top() + page_rect.height() * target.page_uv.y.clamp(0.0, 1.0),
        );
        self.create_image_bubble_at_scene_pos(ctx, scene_pos)
            .is_some()
    }

    pub fn create_image_bubble_at_pointer_shortcut(
        &mut self,
        ctx: &egui::Context,
        pointer_pos: Pos2,
    ) -> bool {
        self.create_image_bubble_at_scene_pos(ctx, pointer_pos)
            .is_some()
    }

    pub fn create_image_bubble_at_scene_pos(
        &mut self,
        ctx: &egui::Context,
        scene_pos: Pos2,
    ) -> Option<i64> {
        if !self.editable {
            return None;
        }
        let (page_idx, page_rect) = self
            .scene
            .page_rects
            .iter()
            .enumerate()
            .find_map(|(idx, rect)| rect.contains(scene_pos).then_some((idx, *rect)))?;
        let new_bid = self.create_bubble_at(page_idx, page_rect, scene_pos);
        self.promote_bubble_to_external_image(ctx, new_bid);
        Some(new_bid)
    }

    fn promote_bubble_to_external_image(&mut self, ctx: &egui::Context, bid: i64) {
        if let Some(rt) = self.bubble_runtime.runtime_bubbles.get_mut(&bid) {
            rt.bubble_class = BubbleClass::Image;
            rt.bubble_type = BubbleType::Aside;
            rt.text.clear();
            rt.original_text.clear();
            // Start with a single small text-area box around the bubble anchor (a sub-box of the
            // red image area, not covering it).
            let anchor = egui::pos2(rt.img_u, rt.img_v);
            rt.text_areas = vec![ImageTextArea {
                area_rect: default_text_area_box(rt.rect_coords, anchor),
                anchor,
                original: String::new(),
                description: String::new(),
                translation: String::new(),
            }];
        }
        let mut extra = Map::new();
        extra.insert(
            "image_source_type".to_string(),
            Value::String("external".to_string()),
        );
        extra.insert("description".to_string(), Value::String(String::new()));
        if let Some(rt) = self.bubble_runtime.runtime_bubbles.get(&bid) {
            upsert_rect_coords_into_extra(&mut extra, rt.rect_coords);
            extra.insert(
                "text_areas".to_string(),
                serialize_image_text_areas(&rt.text_areas),
            );
            if let Some(model) = self.bubble_runtime.bubbles_model.as_ref().map(Arc::clone) {
                let rec = runtime_bubble_to_record(
                    rt.id,
                    rt.img_idx,
                    rt.img_u,
                    rt.img_v,
                    Some(side_to_string(rt.side)),
                    Some(rt.bubble_class.as_str().to_string()),
                    Some(rt.bubble_type.as_str().to_string()),
                    rt.text.clone(),
                    rt.original_text.clone(),
                    Some(extra),
                );
                if let Ok(mut locked) = model.lock()
                    && locked.create_or_replace(rec).is_ok()
                {
                    self.bubble_runtime.synced_bubbles_revision = locked.revision();
                    self.bubble_runtime.pending_upsert.remove(&bid);
                }
            }
        }
        self.bubble_runtime.pending_upsert.insert(bid);
        self.bubble_runtime.selected_bubble = Some(bid);
        self.bubble_runtime.canvas_context_menu_target = None;
        ctx.request_repaint();
    }

    fn create_bubble_at(&mut self, page_idx: usize, page_rect: Rect, scene_pos: Pos2) -> i64 {
        let side = if scene_pos.x < page_rect.center().x {
            Side::Left
        } else {
            Side::Right
        };
        let uv = Self::uv_from_scene(page_rect, scene_pos);
        let id = self.bubble_runtime.next_bubble_id;
        self.bubble_runtime.next_bubble_id += 1;

        self.bubble_runtime.runtime_bubbles.insert(
            id,
            RuntimeBubble {
                id,
                img_idx: page_idx,
                img_u: uv.x,
                img_v: uv.y,
                side,
                bubble_class: BubbleClass::Text,
                bubble_type: BubbleType::Default,
                text: String::new(),
                original_text: String::new(),
                rect_coords: self.default_rect_coords_for_page(page_idx, page_rect, uv.x, uv.y),
                anchor_y: scene_pos.y,
                max_width_px: self.state.bubble_min_width,
                height_px: 80.0,
                line_x: 0.0,
                mounted: false,
                text_areas: Vec::new(),
                image_block_rects: Vec::new(),
            },
        );
        self.bubble_runtime.pending_upsert.insert(id);
        self.bubble_runtime.selected_bubble = Some(id);
        id
    }

    pub(super) fn place_or_move_bubble(
        &mut self,
        bid: i64,
        page_idx: usize,
        page_rect: Rect,
        scene_pos: Pos2,
    ) {
        let side = if scene_pos.x < page_rect.center().x {
            Side::Left
        } else {
            Side::Right
        };
        let uv = Self::uv_from_scene(page_rect, scene_pos);
        if let Some(b) = self.bubble_runtime.runtime_bubbles.get_mut(&bid) {
            b.img_idx = page_idx;
            b.side = side;
        }
        self.move_bubble_anchor(bid, uv.x, uv.y, true);
        if let Some(b) = self.bubble_runtime.runtime_bubbles.get_mut(&bid) {
            b.side = side;
        }
    }

    fn remove_runtime_bubble(&mut self, bid: i64) {
        self.bubble_runtime.runtime_bubbles.remove(&bid);
        self.bubble_runtime.runtime_fingerprints.remove(&bid);
        self.bubble_runtime.pending_upsert.remove(&bid);
        self.bubble_runtime.pending_text_upsert.remove(&bid);
        self.bubble_runtime.focused_bubbles.remove(&bid);
        self.bubble_runtime.deferred_remote_bubbles.remove(&bid);
        self.bubble_runtime.deferred_remote_deletes.remove(&bid);
        if self
            .bubble_runtime
            .pending_bubble_paste
            .is_some_and(|pending| pending.bid == bid)
        {
            self.bubble_runtime.pending_bubble_paste = None;
        }
        if self.bubble_runtime.selected_bubble == Some(bid) {
            self.bubble_runtime.selected_bubble = None;
        }
        if self.bubble_runtime.move_active_bid == Some(bid) {
            self.bubble_runtime.move_active_bid = None;
        }
        if self
            .bubble_runtime
            .active_rect_handle
            .is_some_and(|(handle_bid, _)| handle_bid == bid)
        {
            self.bubble_runtime.active_rect_handle = None;
        }
        if self
            .bubble_runtime
            .aside_drag_state
            .is_some_and(|state| state.bid == bid)
        {
            self.bubble_runtime.aside_drag_state = None;
        }
        if self
            .bubble_runtime
            .on_top_drag_state
            .is_some_and(|state| state.bid == bid)
        {
            self.bubble_runtime.on_top_drag_state = None;
        }
        if self
            .bubble_runtime
            .focused_text_input
            .is_some_and(|focused| focused.bid == bid)
        {
            self.bubble_runtime.focused_text_input = None;
        }
        self.scene.on_top_hit_rects.remove(&bid);
        // Evict per-bubble image caches so deleted ids do not leak across a session and a
        // reused bubble id cannot serve a stale fingerprint/preview from the previous bubble.
        self.image_bubble_meta_cache.remove(&bid);
        self.image_bubble_preview_cache.remove(&bid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::types::{BubbleClass, BubbleType, ImageTextArea, RectCoords};
    use crate::project::Side;

    /// Builds a minimal image runtime bubble with `area_count` text areas and inserts it into
    /// `canvas`. Each area starts empty so a translation result can be observed landing in it.
    fn insert_image_bubble_with_areas(canvas: &mut CanvasView, bubble_id: i64, area_count: usize) {
        let rect = RectCoords {
            p1: egui::pos2(0.1, 0.1),
            p2: egui::pos2(0.5, 0.5),
        };
        let text_areas = (0..area_count)
            .map(|_| ImageTextArea {
                area_rect: rect,
                anchor: egui::pos2(0.2, 0.2),
                original: String::new(),
                description: String::new(),
                translation: String::new(),
            })
            .collect::<Vec<_>>();
        canvas.bubble_runtime.runtime_bubbles.insert(
            bubble_id,
            RuntimeBubble {
                id: bubble_id,
                img_idx: 0,
                img_u: 0.3,
                img_v: 0.3,
                side: Side::Left,
                bubble_class: BubbleClass::Image,
                bubble_type: BubbleType::Default,
                text: String::new(),
                original_text: String::new(),
                rect_coords: rect,
                anchor_y: 0.0,
                max_width_px: 200.0,
                height_px: 80.0,
                line_x: 0.0,
                mounted: false,
                text_areas,
                image_block_rects: Vec::new(),
            },
        );
    }

    #[test]
    fn single_area_image_translation_lands_in_text_areas_not_just_legacy_text() {
        // Regression: single-area image bubbles route through the legacy
        // `apply_machine_translation_result_with_original`, but image-bubble text is canonically
        // stored in `text_areas[0]`. The result must reach `text_areas[0]`, otherwise the flush
        // (which reads `text_areas[0]`) discards it.
        let mut canvas = CanvasView::default();
        insert_image_bubble_with_areas(&mut canvas, 7, 1);

        let applied = canvas.apply_machine_translation_result_with_original(
            7,
            "HELLO".to_string(),
            "Привет".to_string(),
        );

        assert!(applied);
        let rt = &canvas.bubble_runtime.runtime_bubbles[&7];
        assert_eq!(rt.text_areas.len(), 1);
        assert_eq!(rt.text_areas[0].original, "HELLO");
        assert_eq!(rt.text_areas[0].translation, "Привет");
        // Area 0 is mirrored back to the legacy fields so renderers/flush stay consistent.
        assert_eq!(rt.original_text, "HELLO");
        assert_eq!(rt.text, "Привет");
        assert!(canvas.bubble_runtime.pending_upsert.contains(&7));
    }

    #[test]
    fn multi_area_image_translation_updates_each_area_in_order() {
        let mut canvas = CanvasView::default();
        insert_image_bubble_with_areas(&mut canvas, 9, 2);

        let applied = canvas.apply_machine_translation_areas(
            9,
            vec![
                ("A".to_string(), "А".to_string()),
                ("B".to_string(), "Б".to_string()),
            ],
        );

        assert!(applied);
        let rt = &canvas.bubble_runtime.runtime_bubbles[&9];
        assert_eq!(rt.text_areas.len(), 2);
        assert_eq!(rt.text_areas[0].translation, "А");
        assert_eq!(rt.text_areas[1].original, "B");
        assert_eq!(rt.text_areas[1].translation, "Б");
        // Area 0 mirrors to the legacy fields.
        assert_eq!(rt.text, "А");
        assert_eq!(rt.original_text, "A");
    }

    #[test]
    fn image_bubble_without_text_areas_uses_legacy_text_field() {
        // An image bubble that has never been expanded keeps `text_areas` empty; the legacy path
        // must still write the plain `text`/`original_text` fields used by the flush fallback.
        let mut canvas = CanvasView::default();
        insert_image_bubble_with_areas(&mut canvas, 11, 0);

        let applied = canvas.apply_machine_translation_result_with_original(
            11,
            "SIGN".to_string(),
            "Вывеска".to_string(),
        );

        assert!(applied);
        let rt = &canvas.bubble_runtime.runtime_bubbles[&11];
        assert!(rt.text_areas.is_empty());
        assert_eq!(rt.original_text, "SIGN");
        assert_eq!(rt.text, "Вывеска");
    }

    #[test]
    fn remove_runtime_bubble_evicts_image_caches() {
        // Regression: deleting a bubble must drop its per-id image fingerprint/preview cache
        // entries, otherwise they leak for the whole session and a reused id can serve a stale
        // fingerprint/preview from the previous bubble.
        use super::super::ImageBubblePreviewCacheEntry;
        use std::time::Instant;

        let mut canvas = CanvasView::default();
        insert_image_bubble_with_areas(&mut canvas, 21, 1);
        canvas
            .image_bubble_meta_cache
            .insert(21, (Instant::now(), "len:mtime".to_string()));
        canvas.image_bubble_preview_cache.insert(
            21,
            ImageBubblePreviewCacheEntry {
                key: "len:mtime".to_string(),
                texture: None,
                size_px: [4, 4],
                error: None,
            },
        );

        canvas.remove_runtime_bubble(21);

        assert!(!canvas.image_bubble_meta_cache.contains_key(&21));
        assert!(!canvas.image_bubble_preview_cache.contains_key(&21));
        assert!(!canvas.bubble_runtime.runtime_bubbles.contains_key(&21));
    }

    use crate::project::{CanvasSettings, ProjectPaths};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Builds an empty in-memory `ProjectData` (no pages, no bubbles) for runtime tests.
    ///
    /// Only `bubbles` is read by the bubble-runtime flush/sync paths exercised here; all paths are
    /// empty placeholders and nothing is written to disk by the project itself.
    fn empty_project() -> ProjectData {
        let empty = || std::path::PathBuf::new();
        ProjectData {
            project_dir: empty(),
            image_dir: empty(),
            pages: Vec::new(),
            bubbles: Arc::new(Vec::new()),
            paths: ProjectPaths {
                project_dir: empty(),
                title_dir: empty(),
                notes_file: empty(),
                bubbles_file: empty(),
                src_dir: empty(),
                clean_layers_dir: empty(),
                cleaned_dir: empty(),
                alt_vers_dir: empty(),
                saved_dir: empty(),
                image_bubbles_dir: empty(),
                text_images_dir: empty(),
                layers_dir: empty(),
                text_detection_dir: empty(),
                characters_dir: empty(),
                terms_file: empty(),
                settings_file: empty(),
                unsaved_dir: empty(),
                unsaved_bubbles_file: empty(),
                unsaved_clean_layers_dir: empty(),
                unsaved_image_bubbles_dir: empty(),
                unsaved_text_images_dir: empty(),
                unsaved_layers_dir: empty(),
            },
            comic_type: None,
            canvas_settings: CanvasSettings::default(),
            settings_data: Value::Null,
        }
    }

    /// Builds a `BubblesModel` with isolated temporary save paths so the background saver does not
    /// touch any real project and concurrent tests do not collide.
    fn test_model(bubbles: Vec<Bubble>) -> Arc<Mutex<BubblesModel>> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let unsaved =
            std::env::temp_dir().join(format!("manhwastudio_bubble_runtime_test_{n}.json"));
        Arc::new(Mutex::new(BubblesModel::new(
            bubbles,
            unsaved.with_extension("saved.json"),
            unsaved,
            SharedCanvasSettings::default(),
        )))
    }

    /// Builds a persisted text `Bubble` record at the given page/anchor.
    fn text_bubble_record(id: i64, img_idx: usize, u: f32, v: f32, side: Side) -> Bubble {
        runtime_bubble_to_record(
            id,
            img_idx,
            u,
            v,
            Some(side_to_string(side)),
            Some("text".to_string()),
            Some("aside".to_string()),
            String::new(),
            String::new(),
            None,
        )
    }

    /// Inserts a plain text runtime bubble (no image areas) into `canvas`.
    fn insert_text_runtime_bubble(
        canvas: &mut CanvasView,
        id: i64,
        img_idx: usize,
        u: f32,
        v: f32,
        side: Side,
        bubble_type: BubbleType,
    ) {
        canvas.bubble_runtime.runtime_bubbles.insert(
            id,
            RuntimeBubble {
                id,
                img_idx,
                img_u: u,
                img_v: v,
                side,
                bubble_class: BubbleClass::Text,
                bubble_type,
                text: String::new(),
                original_text: String::new(),
                rect_coords: RectCoords {
                    p1: egui::pos2((u - 0.05).max(0.0), (v - 0.05).max(0.0)),
                    p2: egui::pos2((u + 0.05).min(1.0), (v + 0.05).min(1.0)),
                },
                anchor_y: 0.0,
                max_width_px: 200.0,
                height_px: 80.0,
                line_x: 0.0,
                mounted: true,
                text_areas: Vec::new(),
                image_block_rects: Vec::new(),
            },
        );
    }

    #[test]
    fn drag_gesture_pushes_single_undo_entry_and_debounces_positional_flush() {
        // A continuous positional drag must (1) write the model only on release (debounced), not
        // every frame, and (2) push exactly one undo entry capturing the pre-gesture state.
        let project = empty_project();
        let model = test_model(vec![text_bubble_record(1, 0, 0.30, 0.30, Side::Left)]);
        let mut canvas = CanvasView::default();
        canvas.set_bubbles_model(Arc::clone(&model));
        canvas.sync_runtime_from_model_or_project(&project);
        let revision_before = model.lock().expect("lock model").revision();

        // Gesture start: a drag state is active and the bubble follows the pointer each frame.
        canvas.bubble_runtime.aside_drag_state = Some(AsideDragState {
            bid: 1,
            target: super::super::types::AsideDragTarget::BubbleBody,
            last_pointer_pos: egui::pos2(0.0, 0.0),
            moved: true,
        });
        for step in 1..=5u16 {
            let u = 0.30 + 0.05 * f32::from(step);
            canvas.move_bubble_anchor_impl(1, u, 0.30, true, true);
            // Per-frame flush during the active drag is debounced: the model is not written, so its
            // revision stays unchanged and no undo entry is captured yet.
            canvas.flush_bubble_upserts_to_model(&project);
        }
        assert_eq!(
            model.lock().expect("lock model").revision(),
            revision_before,
            "model must not be written while the drag is active"
        );
        assert!(canvas.bubble_runtime.bubble_undo_stack.is_empty());
        // The dragged bubble's final runtime position followed the pointer live.
        let final_u = canvas.bubble_runtime.runtime_bubbles[&1].img_u;
        assert!(
            (final_u - 0.55).abs() < 1e-4,
            "runtime anchor must track pointer: {final_u}"
        );

        // Gesture end: clearing the drag state and re-queuing the upsert (as finish_*_drag does)
        // commits the final position on the next flush.
        canvas.bubble_runtime.aside_drag_state = None;
        canvas.bubble_runtime.pending_upsert.insert(1);
        canvas.flush_bubble_upserts_to_model(&project);

        // Exactly one undo entry, capturing the pre-gesture anchor.
        assert_eq!(canvas.bubble_runtime.bubble_undo_stack.len(), 1);
        let undo_anchor = canvas.bubble_runtime.bubble_undo_stack[0].bubbles[0].img_u;
        assert!(
            (undo_anchor - 0.30).abs() < 1e-4,
            "undo must hold pre-gesture anchor: {undo_anchor}"
        );

        // The model now holds the committed final position.
        let committed_u = model
            .lock()
            .expect("lock model")
            .with_bubble(1, |b| b.img_u)
            .expect("bubble 1 exists");
        assert!(
            (committed_u - 0.55).abs() < 1e-4,
            "final position must commit: {committed_u}"
        );
    }

    #[test]
    fn pointer_up_fallback_commits_lingering_aside_drag_exactly_once() {
        // DATA-LOSS regression: if a positional aside drag's page scrolls off-screen mid-drag, the
        // widget never delivers `drag_stopped()`, so `finish_aside_drag` never runs from the widget
        // path. The per-frame pointer-up fallback must commit the final runtime position exactly
        // once and clear the drag-state, so the move is not lost on reload.
        let project = empty_project();
        let model = test_model(vec![text_bubble_record(1, 0, 0.30, 0.30, Side::Left)]);
        let mut canvas = CanvasView::default();
        canvas.set_bubbles_model(Arc::clone(&model));
        canvas.sync_runtime_from_model_or_project(&project);

        // Simulate an in-progress drag that already moved the runtime bubble to its final position
        // (as the per-frame pointer-follow does), but whose widget never delivered `drag_stopped`.
        canvas.bubble_runtime.aside_drag_state = Some(AsideDragState {
            bid: 1,
            target: super::super::types::AsideDragTarget::BubbleBody,
            last_pointer_pos: egui::pos2(0.0, 0.0),
            moved: true,
        });
        canvas.move_bubble_anchor_impl(1, 0.62, 0.30, true, true);
        let runtime_u = canvas.bubble_runtime.runtime_bubbles[&1].img_u;

        // Pointer-up fallback: commits the lingering gesture and clears the drag-state.
        canvas.commit_lingering_drag_gestures_on_pointer_up();
        assert!(
            canvas.bubble_runtime.aside_drag_state.is_none(),
            "fallback must clear the lingering drag-state"
        );
        assert!(
            canvas.bubble_runtime.pending_upsert.contains(&1),
            "fallback must re-queue the dragged id so the final position commits"
        );

        // Exactly one commit: flush writes the runtime position to the model.
        canvas.flush_bubble_upserts_to_model(&project);
        assert!(
            !canvas.bubble_runtime.pending_upsert.contains(&1),
            "the queued upsert must be consumed by exactly one flush"
        );
        let committed_u = model
            .lock()
            .expect("lock model")
            .with_bubble(1, |b| b.img_u)
            .expect("bubble 1 exists");
        assert!(
            (committed_u - runtime_u).abs() < 1e-4,
            "committed position must equal the runtime position: {committed_u} vs {runtime_u}"
        );

        // Double-commit guard: a second fallback with the state already cleared is a no-op.
        canvas.commit_lingering_drag_gestures_on_pointer_up();
        assert!(
            canvas.bubble_runtime.pending_upsert.is_empty(),
            "fallback must not re-queue once the gesture state is cleared"
        );
    }

    #[test]
    fn pointer_up_fallback_commits_lingering_handle_gestures() {
        // The rect/area resize-handle paths normally clear `active_*_handle` on `drag_stopped`; if
        // the widget never delivers it, the fallback must clear them and re-queue the upsert.
        let project = empty_project();
        let model = test_model(vec![text_bubble_record(1, 0, 0.30, 0.30, Side::Left)]);
        let mut canvas = CanvasView::default();
        canvas.set_bubbles_model(Arc::clone(&model));
        canvas.sync_runtime_from_model_or_project(&project);

        canvas.bubble_runtime.active_rect_handle = Some((1, 2));
        canvas.bubble_runtime.active_area_handle = Some((1, 0, 4));
        canvas.commit_lingering_drag_gestures_on_pointer_up();

        assert!(canvas.bubble_runtime.active_rect_handle.is_none());
        assert!(canvas.bubble_runtime.active_area_handle.is_none());
        assert!(canvas.bubble_runtime.pending_upsert.contains(&1));
    }

    #[test]
    fn rect_handle_resize_commits_final_rect_on_normal_release() {
        // Regression (possible data loss): the rect-handle resize debounces model writes while
        // `active_rect_handle` is set (see `positional_drag_gesture_active`). The on-screen
        // resize path (`bubble_on_top_ui::resize_rect_by_handle`) re-queues the dragged id in
        // `pending_upsert` every dragged frame; the `drag_stopped` handler then only clears the
        // handle. This test simulates that gesture with real runtime/model state and asserts the
        // final rect is committed to the model on a normal release, so the resize is not lost.
        let project = empty_project();
        let model = test_model(vec![text_bubble_record(1, 0, 0.30, 0.30, Side::Left)]);
        let mut canvas = CanvasView::default();
        canvas.set_bubbles_model(Arc::clone(&model));
        canvas.sync_runtime_from_model_or_project(&project);
        insert_text_runtime_bubble(&mut canvas, 1, 0, 0.30, 0.30, Side::Left, BubbleType::Aside);

        // Frame 1 of the drag: handle becomes active, the runtime rect follows the pointer, and the
        // resize fn re-queues the id. The debounced flush must be a no-op while the gesture runs.
        canvas.bubble_runtime.active_rect_handle = Some((1, 4));
        let mid_rect = RectCoords {
            p1: egui::pos2(0.20, 0.20),
            p2: egui::pos2(0.40, 0.40),
        };
        canvas
            .bubble_runtime
            .runtime_bubbles
            .get_mut(&1)
            .expect("bubble 1")
            .rect_coords = mid_rect;
        canvas.bubble_runtime.pending_upsert.insert(1);
        canvas.flush_bubble_upserts_to_model(&project);
        assert!(
            canvas.bubble_runtime.pending_upsert.contains(&1),
            "the flush must stay debounced while the rect-handle gesture is active"
        );
        assert!(
            model
                .lock()
                .expect("lock model")
                .with_bubble(1, rect_coords_from_bubble)
                .flatten()
                .is_none(),
            "no rect must be written to the model mid-gesture"
        );

        // Final dragged frame: the runtime rect reaches its on-screen position and is re-queued one
        // last time (mirrors `resize_rect_by_handle`'s per-frame `pending_upsert.insert`).
        let final_rect = RectCoords {
            p1: egui::pos2(0.15, 0.10),
            p2: egui::pos2(0.55, 0.62),
        };
        canvas
            .bubble_runtime
            .runtime_bubbles
            .get_mut(&1)
            .expect("bubble 1")
            .rect_coords = final_rect;
        canvas.bubble_runtime.pending_upsert.insert(1);

        // Normal on-screen release: `draw_rect_handles`'s `drag_stopped` only clears the handle. The
        // id is still queued from the final frame, so the next flush must commit the final rect.
        canvas.bubble_runtime.active_rect_handle = None;
        assert!(
            canvas.bubble_runtime.pending_upsert.contains(&1),
            "the final dragged frame must leave the id queued for the post-release flush"
        );

        canvas.flush_bubble_upserts_to_model(&project);
        assert!(
            !canvas.bubble_runtime.pending_upsert.contains(&1),
            "the queued upsert must be consumed by exactly one post-release flush"
        );
        let committed = model
            .lock()
            .expect("lock model")
            .with_bubble(1, rect_coords_from_bubble)
            .flatten()
            .expect("the final rect must be persisted to the model")
            .normalized();
        let expected = final_rect.normalized();
        assert!(
            (committed.p1.x - expected.p1.x).abs() < 1e-4
                && (committed.p1.y - expected.p1.y).abs() < 1e-4
                && (committed.p2.x - expected.p2.x).abs() < 1e-4
                && (committed.p2.y - expected.p2.y).abs() < 1e-4,
            "committed rect must equal the final on-screen rect: {committed:?} vs {expected:?}"
        );
    }

    #[test]
    fn hook_bubbles_revision_changes_when_runtime_only_bubble_appears() {
        // A bubble that lives only in `runtime_bubbles` (created but not yet flushed) must bump the
        // fingerprint so a caller gating per-frame work on it does not miss the new bubble.
        let project = empty_project();
        let model = test_model(Vec::new());
        let mut canvas = CanvasView::default();
        canvas.set_bubbles_model(Arc::clone(&model));
        canvas.sync_runtime_from_model_or_project(&project);

        let before = canvas.hook_bubbles_revision();
        insert_text_runtime_bubble(&mut canvas, 1, 0, 0.20, 0.30, Side::Left, BubbleType::Aside);
        let after = canvas.hook_bubbles_revision();
        assert_ne!(
            before, after,
            "a runtime-only bubble must change the bubbles-revision fingerprint"
        );
    }

    #[test]
    fn capture_history_dedups_by_revision_not_content() {
        // Two captures with no intervening model mutation share a revision and must collapse to a
        // single undo entry; a real mutation between them yields a second entry.
        let project = empty_project();
        let model = test_model(vec![text_bubble_record(1, 0, 0.40, 0.40, Side::Left)]);
        let mut canvas = CanvasView::default();
        canvas.set_bubbles_model(Arc::clone(&model));
        canvas.sync_runtime_from_model_or_project(&project);

        canvas.capture_bubble_history_before_mutation();
        canvas.capture_bubble_history_before_mutation();
        assert_eq!(
            canvas.bubble_runtime.bubble_undo_stack.len(),
            1,
            "repeat capture of an unchanged model is a no-op"
        );

        // First real mutation: the pre-write capture dedups against the existing entry (same
        // revision), then the write advances the model revision. Stack stays at one entry holding
        // the pre-mutation state.
        canvas
            .bubble_runtime
            .runtime_bubbles
            .get_mut(&1)
            .expect("bubble 1")
            .img_u = 0.60;
        canvas.bubble_runtime.pending_upsert.insert(1);
        canvas.flush_bubble_upserts_to_model(&project);
        assert_eq!(
            canvas.bubble_runtime.bubble_undo_stack.len(),
            1,
            "the first mutation's pre-state was already captured"
        );

        // Second real mutation: now the pre-write capture sees the advanced revision and pushes a
        // distinct entry, proving dedup is by revision, not content.
        canvas
            .bubble_runtime
            .runtime_bubbles
            .get_mut(&1)
            .expect("bubble 1")
            .img_u = 0.20;
        canvas.bubble_runtime.pending_upsert.insert(1);
        canvas.flush_bubble_upserts_to_model(&project);
        assert_eq!(canvas.bubble_runtime.bubble_undo_stack.len(), 2);
    }

    #[test]
    fn page_bubbles_bucketed_filters_and_orders_each_column() {
        // The single-pass bucketing must (1) keep only the requested page, (2) route each bubble to
        // its (side, displayed-type) column, and (3) order each column by vertical anchor, then
        // horizontal anchor, then bubble id. Assertions use inline expected ids derived by hand from
        // the inputs (an independent reference), not the bucketer's own output.
        let mut canvas = CanvasView::default();
        // Mixed page/side/type bubbles, plus one on another page that must be excluded. Bubbles 2
        // and 7 share a vertical anchor (0.10) to exercise the horizontal-then-id tiebreak.
        insert_text_runtime_bubble(&mut canvas, 1, 0, 0.20, 0.30, Side::Left, BubbleType::Aside);
        insert_text_runtime_bubble(&mut canvas, 2, 0, 0.10, 0.10, Side::Left, BubbleType::Aside);
        insert_text_runtime_bubble(
            &mut canvas,
            3,
            0,
            0.80,
            0.50,
            Side::Right,
            BubbleType::Aside,
        );
        insert_text_runtime_bubble(&mut canvas, 4, 0, 0.30, 0.20, Side::Left, BubbleType::OnTop);
        insert_text_runtime_bubble(
            &mut canvas,
            5,
            0,
            0.90,
            0.40,
            Side::Right,
            BubbleType::OnTop,
        );
        insert_text_runtime_bubble(&mut canvas, 6, 0, 0.15, 0.05, Side::Left, BubbleType::Aside);
        insert_text_runtime_bubble(&mut canvas, 7, 0, 0.25, 0.10, Side::Left, BubbleType::Aside);
        // Other-page bubble (must be excluded entirely).
        insert_text_runtime_bubble(
            &mut canvas,
            99,
            1,
            0.20,
            0.20,
            Side::Left,
            BubbleType::Aside,
        );

        let buckets = canvas.page_bubbles_bucketed(0);
        let ids = |side, bubble_type| {
            buckets
                .bucket(side, bubble_type)
                .iter()
                .map(|item| item.bid)
                .collect::<Vec<i64>>()
        };

        // Left aside: 6@v0.05, then the v0.10 pair ordered by horizontal anchor (2@u0.10 before
        // 7@u0.25), then 1@v0.30. Excludes the other-page bubble 99.
        assert_eq!(ids(Side::Left, BubbleType::Aside), vec![6, 2, 7, 1]);
        // Right aside: only bubble 3.
        assert_eq!(ids(Side::Right, BubbleType::Aside), vec![3]);
        // Left on-top: only bubble 4.
        assert_eq!(ids(Side::Left, BubbleType::OnTop), vec![4]);
        // Right on-top: only bubble 5.
        assert_eq!(ids(Side::Right, BubbleType::OnTop), vec![5]);
    }

    #[test]
    fn page_bubbles_bucketed_orders_equal_anchors_by_bubble_id() {
        // Final tiebreak: two bubbles sharing both anchors must order by ascending bubble id so the
        // ordering is stable frame-to-frame.
        let mut canvas = CanvasView::default();
        insert_text_runtime_bubble(
            &mut canvas,
            20,
            0,
            0.40,
            0.40,
            Side::Left,
            BubbleType::Aside,
        );
        insert_text_runtime_bubble(
            &mut canvas,
            10,
            0,
            0.40,
            0.40,
            Side::Left,
            BubbleType::Aside,
        );

        let order: Vec<i64> = canvas
            .page_bubbles_bucketed(0)
            .bucket(Side::Left, BubbleType::Aside)
            .iter()
            .map(|item| item.bid)
            .collect();
        assert_eq!(order, vec![10, 20]);
    }
}
