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
    bubble_fingerprint, bubbles_stamp, sanitize_clipboard_text, side_to_string,
    upsert_rect_coords_into_extra,
};
use super::types::{
    AsideDragState, BubbleAction, BubbleCopyPasteTarget, BubbleHistoryEntry, BubbleTextField,
    CanvasContextMenuTarget, CopiedBubbleData, FocusedBubbleTextInput, OnTopDragState,
    PendingBubblePaste, RectCoords, RuntimeBubble,
};
use super::{
    BUBBLE_HISTORY_LIMIT, BubbleType, CanvasHooks, CanvasView, DUPLICATE_BUBBLE_OFFSET_PX,
    TEXT_UPSERT_DEBOUNCE_SECS, bubbles_history_hash, read_system_clipboard_text,
    rect_coords_from_bubble,
};
use crate::models::bubbles_model::{BubblesModel, SharedCanvasSettings, runtime_bubble_to_record};
use crate::project::{Bubble, ProjectData, Side};
use crate::runtime_log;
use eframe::egui;
use egui::{Pos2, Rect};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub(super) struct BubbleRuntimeState {
    pub(super) runtime_bubbles: HashMap<i64, RuntimeBubble>,
    pub(super) selected_bubble: Option<i64>,
    pub(super) move_active_bid: Option<i64>,
    pub(super) active_rect_handle: Option<(i64, usize)>,
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
                bubble_type: BubbleType::Default,
                text: String::new(),
                original_text,
                rect_coords,
                anchor_y,
                max_width_px: self.state.bubble_min_width,
                height_px: 80.0,
                line_x: 0.0,
                mounted: false,
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
            Some(rt_bubble_type.as_str().to_string()),
            rt_text,
            rt_original_text,
            Some(extra),
        );

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

    pub(super) fn capture_bubble_history_before_mutation(&mut self) {
        let Some(model) = self.bubble_runtime.bubbles_model.as_ref().map(Arc::clone) else {
            return;
        };
        let Ok(locked) = model.lock() else {
            runtime_log::log_warn(
                "[canvas::bubble_runtime] failed to lock BubblesModel for undo snapshot",
            );
            return;
        };
        self.push_bubble_undo_snapshot(locked.snapshot());
    }

    fn push_bubble_undo_snapshot(&mut self, bubbles: Vec<Bubble>) {
        let hash = bubbles_history_hash(&bubbles);
        if self
            .bubble_runtime
            .bubble_undo_stack
            .last()
            .is_some_and(|entry| entry.hash == hash)
        {
            return;
        }
        self.bubble_runtime
            .bubble_undo_stack
            .push(BubbleHistoryEntry { bubbles, hash });
        if self.bubble_runtime.bubble_undo_stack.len() > BUBBLE_HISTORY_LIMIT {
            let overflow = self.bubble_runtime.bubble_undo_stack.len() - BUBBLE_HISTORY_LIMIT;
            self.bubble_runtime.bubble_undo_stack.drain(0..overflow);
        }
        self.bubble_runtime.bubble_redo_stack.clear();
    }

    fn push_bubble_redo_snapshot(&mut self, bubbles: Vec<Bubble>) {
        let hash = bubbles_history_hash(&bubbles);
        if self
            .bubble_runtime
            .bubble_redo_stack
            .last()
            .is_some_and(|entry| entry.hash == hash)
        {
            return;
        }
        self.bubble_runtime
            .bubble_redo_stack
            .push(BubbleHistoryEntry { bubbles, hash });
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
        let current = locked.snapshot();
        let current_hash = bubbles_history_hash(&current);
        let mut redo_pushed = false;
        if self
            .bubble_runtime
            .bubble_redo_stack
            .last()
            .is_none_or(|entry| entry.hash != current_hash)
        {
            self.push_bubble_redo_snapshot(current);
            redo_pushed = true;
        }
        if let Err(err) = locked.reset(target.bubbles.clone()) {
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
        let current = locked.snapshot();
        let current_hash = bubbles_history_hash(&current);
        let mut undo_pushed = false;
        if self
            .bubble_runtime
            .bubble_undo_stack
            .last()
            .is_none_or(|entry| entry.hash != current_hash)
        {
            self.bubble_runtime
                .bubble_undo_stack
                .push(BubbleHistoryEntry {
                    bubbles: current,
                    hash: current_hash,
                });
            if self.bubble_runtime.bubble_undo_stack.len() > BUBBLE_HISTORY_LIMIT {
                let overflow = self.bubble_runtime.bubble_undo_stack.len() - BUBBLE_HISTORY_LIMIT;
                self.bubble_runtime.bubble_undo_stack.drain(0..overflow);
            }
            undo_pushed = true;
        }
        if let Err(err) = locked.reset(target.bubbles.clone()) {
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
        let events = ctx.input(|i| i.events.clone());
        for ev in events {
            match ev {
                egui::Event::Copy => {
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
                egui::Event::Cut => {
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
                egui::Event::Paste(mut text) => {
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
                _ => {}
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
        let new_u = bubble.img_u.clamp(0.0, 1.0);
        let new_v = bubble.img_v.clamp(0.0, 1.0);
        let (min_margin_u, min_margin_v) = self.bubble_min_uv_margin_for_page(bubble.img_idx);
        let coords_from_record = rect_coords_from_bubble(bubble);
        if let Some(existing) = self.bubble_runtime.runtime_bubbles.get_mut(&bubble.id) {
            let du = new_u - existing.img_u;
            let dv = new_v - existing.img_v;
            existing.img_idx = bubble.img_idx;
            existing.side = side;
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
                    bubble_type,
                    text: bubble.text.clone(),
                    original_text: bubble.original_text.clone(),
                    rect_coords,
                    anchor_y: 0.0,
                    max_width_px: self.state.bubble_min_width,
                    height_px: 80.0,
                    line_x: 0.0,
                    mounted: false,
                },
            );
        }
        self.bubble_runtime
            .runtime_fingerprints
            .insert(bubble.id, fingerprint);
    }

    pub(super) fn page_bubbles(
        &self,
        page_idx: usize,
        side: Side,
        bubble_type: BubbleType,
    ) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .bubble_runtime
            .runtime_bubbles
            .values()
            .filter(|b| {
                b.img_idx == page_idx
                    && b.side == side
                    && self.displayed_bubble_type_for_runtime(b) == bubble_type
            })
            .map(|b| b.id)
            .collect();

        ids.sort_by(|a, b| {
            match (
                self.bubble_runtime.runtime_bubbles.get(a),
                self.bubble_runtime.runtime_bubbles.get(b),
            ) {
                (Some(a_ref), Some(b_ref)) => a_ref
                    .img_v
                    .total_cmp(&b_ref.img_v)
                    .then_with(|| a_ref.img_u.total_cmp(&b_ref.img_u))
                    .then_with(|| a_ref.id.cmp(&b_ref.id)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
        });
        ids.retain(|bid| self.bubble_runtime.runtime_bubbles.contains_key(bid));
        ids
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
        let mut extra_by_id: HashMap<i64, Map<String, Value>> = locked
            .snapshot()
            .into_iter()
            .map(|bubble| (bubble.id, bubble.extra))
            .collect();
        let mut had_success = false;
        for bid in pending {
            if self.is_bubble_locally_locked(bid) {
                continue;
            }
            let Some(rt) = self.bubble_runtime.runtime_bubbles.get(&bid) else {
                self.bubble_runtime.pending_upsert.remove(&bid);
                continue;
            };
            let extra = extra_by_id.remove(&bid).or_else(|| {
                project
                    .bubbles
                    .iter()
                    .find(|b| b.id == bid)
                    .map(|b| b.extra.clone())
            });
            let mut extra = extra.unwrap_or_default();
            upsert_rect_coords_into_extra(&mut extra, rt.rect_coords);
            let rec = runtime_bubble_to_record(
                rt.id,
                rt.img_idx,
                rt.img_u,
                rt.img_v,
                Some(side_to_string(rt.side)),
                Some(rt.bubble_type.as_str().to_string()),
                rt.text.clone(),
                rt.original_text.clone(),
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
                bubble_type: BubbleType::Default,
                text: String::new(),
                original_text: String::new(),
                rect_coords: self.default_rect_coords_for_page(page_idx, page_rect, uv.x, uv.y),
                anchor_y: scene_pos.y,
                max_width_px: self.state.bubble_min_width,
                height_px: 80.0,
                line_x: 0.0,
                mounted: false,
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
    }
}
