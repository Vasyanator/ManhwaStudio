/*
File: src/canvas/mod.rs

Purpose:
Compact canvas facade for page rendering, bubble editing, and clean-overlay painting.

Main types:
- `CanvasUiStatus`: per-frame loading status for canvas overlays/hooks.
- `CanvasFrameParams`: per-frame viewport/interaction flags shared by scene + viewport passes.
- `CanvasScenePageFrame`: geometry snapshot for one page row within the scene pass.
- `OverlayUploadBudget`: per-frame clean-overlay upload budget used to keep GUI responsive.
- `BubbleAction`: user actions emitted from bubble UI (`Translate`, `Delete`).
- `BubbleType`: per-bubble stored type (`Default`, `Aside`, `OnTop`); `Default` resolves
  through canvas editable/readonly settings before rendering.
- `BubbleMode`: legacy persisted canvas mode retained for settings migration/compatibility.
- `BubbleCopyPasteTarget`: bubble context-menu paste targets (`Original`, `Translation`, `WholeBubble`).
- `RectCoords`: normalized UV rectangle used for bubble placement and resize logic.
- `RuntimeBubble`: mutable runtime bubble state used by canvas editing/render.
- `CopiedBubbleData`: internal clipboard payload for whole-bubble copy/paste (all non-positional data).
- `AsideDragTarget`: source of active aside drag (`BubbleBody` or `RectArea`).
- `AsideDragState`: runtime state of active aside drag (`bid`, source target, last pointer pos, moved flag).
- `OnTopDragState`: runtime state of active on-top move-handle drag (`bid`, last pointer pos, moved flag).
- `BubbleHistoryEntry`: one global undo/redo snapshot for all bubbles.
- `BubbleLink`: geometry for anchor line from bubble to image point.
- Source page textures are optional GPU residency: page geometry comes from `PageImageInfo`, while
  source tile LINEAR/NEAREST handles may be dropped and recreated from decoded tile bytes.
- `OverlayTextureTile` / `OverlayTexturePage`: GPU texture storage for tiled overlays.
- `OverlayPrepareRequest` / `OverlayPreparedTile` / `OverlayPrepareResult` / `OverlayPreparedPage`:
  async CPU-side overlay tiling payloads for background preparation + throttled GPU upload.
- `CanvasSettingsSaveRequest`: async write request for persisted canvas settings.
- `OverlayRectPx`: pixel-space overlay rectangle used for region edits and blits.
- `CanvasHooks`: extension points for tabs to customize canvas behavior/UI.
- `CanvasView`: main canvas controller (state, render pass, sync, editing, async workers).
- `CanvasState`: persistent user-tunable canvas settings.

Internal submodules:
- `types`: passive canvas types and DTO/runtime payload structs.
- `helpers`: stateless helpers for geometry, overlay tiling, hashing, and text sizing.
- `scene`: page strip, viewport draw pipeline, page interactions, and floating controls.
- `bubble_runtime`: bubble runtime bucket (state, model sync, clipboard, history, pending writes).
- `bubble_aside_ui`: aside bubble columns, repack, anchor links, and aside drag interactions.
- `bubble_on_top_ui`: on-top bubble widgets, move/resize handles, and hit-rect tracking.
- `overlay_runtime`: clean-overlay runtime bucket (state, worker, tile upload, draw/edit ops).
- `settings`: canvas settings snapshot/publish/persistence logic.
- `workers`: background worker bootstrap for overlay prepare and settings save.

Key constants:
- `TEXT_UPSERT_DEBOUNCE_SECS`: debounce for text writes to shared bubbles model.
- `OVERLAY_TILE_SIDE`: tile size for overlay texture partitioning.
- `OVERLAY_UPLOAD_TILE_BUDGET_PER_FRAME` / `OVERLAY_UPLOAD_BYTES_BUDGET_PER_FRAME`:
  per-frame GPU upload budgets to keep UI responsive.
- `OVERLAY_FULL_REBUILD_DIRTY_TILES_THRESHOLD`: threshold to switch partial/full dirty updates.
- `ON_TOP_FOCUS_GAP_PX`: vertical gaps for focused on-top bubble controls.
- `ON_TOP_MOVE_HANDLE_OFFSET_PX` / `ON_TOP_MOVE_HANDLE_RADIUS_PX`: geometry for the focused
  on-top palm move-handle drawn above the bubble rect.
- `DEFAULT_BUBBLE_RECT_SIDE_SRC_PX`: default bubble rectangle side in source pixels.
- `LEGACY_DEFAULT_RECT_DELTA_UV`: fallback UV span for legacy bubbles without rect coords.
- `BUBBLE_HISTORY_LIMIT`: max undo/redo depth for full-bubble snapshots.
- `BUBBLE_ANCHOR_OUTSIDE_RECT_SPAN_MULT`: max anchor overshoot outside rect in multiples of rect span.

CanvasHooks methods:
- `wants_canvas_shift_drag_selection`: lets tab capture Shift+drag interactions.
- `draw_canvas_mask_overlay_on_page`: tab-owned per-page mask-layer draw pass rendered above clean overlay.
- `draw_canvas_overlay_on_page`: tab-owned per-page additional-elements draw pass rendered above mask.
- `draw_canvas_overlay_top_left`: tab-owned overlay UI drawn above canvas.
- `build_bubble_header`: tab-owned bubble header UI extension.
- `readonly_aside_header_width_hint`: optional width hint for readonly aside header chrome.
- `build_bubble_footer`: tab-owned bubble footer UI extension.
- `on_bubble_action`: callback for bubble actions triggered from canvas.
- `draw_canvas_page_context_menu`: optional override для ПКМ-меню по странице
  (если вернул `true`, стандартные пункты CanvasView не показываются).

RectCoords helpers:
- `normalized`: keeps p1/p2 ordered by min/max axes.
- `center_uv`: returns center point in UV space.

RuntimeBubble helper:
- `display_text`: returns translated text or fallback to original text.

CanvasView method map:
- Overlay lifecycle/preparation:
  `ensure_overlay_for_page_size`, `page_source_size_from_scene`,
  `reset_overlay_prepare_state`, `poll_overlay_prepare_results`,
  `has_pending_overlay_work`, `mark_overlay_dirty_full`, `draw_overlay_on_page`,
  `commit_overlay_page_to_model`.
- External wiring/state toggles:
  `set_bubbles_model`, `set_overlays_model`, `set_drag_scroll_blocked`,
  `set_wheel_scroll_blocked`, `set_zoom_blocked`, `set_overlay_render_suppressed`,
  `set_overlay_upload_min_interval_s`, `set_clean_overlays_visible`,
  `set_clean_overlays_visible_for_canvas_only`, `set_scroll_area_id_salt`,
  `viewport_snapshot`, `apply_viewport_snapshot`, `clean_overlays_visible`.
- Navigation/shortcuts/actions:
  `current_page_idx`, `zoom_by_shortcut`, `reset_zoom_shortcut`,
  `delete_selected_bubble_shortcut`, `create_bubble_at_pointer_shortcut`,
  `copy_from_focused_bubble_shortcut`, `cut_focused_bubble_shortcut`,
  `copy_whole_bubble_to_internal_buffer`,
  `paste_into_focused_bubble_or_create_shortcut`,
  `paste_copied_whole_bubble_into_focused_or_create`,
  `duplicate_focused_bubble_shortcut`, `duplicate_bubble_below`,
  `is_bubble_move_mode_active`, `toggle_move_mode_for_bubble`,
  `request_delete_bubble`, `request_translate_bubble`,
  `set_bubble_texts_from_panel`, `copy_bubble_text_to_clipboard`,
  `paste_bubble_text_from_clipboard`,
  `flush_pending_bubble_upserts_now`, `apply_machine_translation_result`,
  `patch_bubble_extra_fields`, `capture_bubble_history_before_mutation`,
  `try_undo_bubbles_history`, `try_redo_bubbles_history`, `handle_shortcuts`.
- Scene and overlay geometry API:
  `page_index_at_scene_pos`, `bubble_original_text`, `page_contains_scene_pos`,
  `visible_scene_rect`, `overlay_image`, `clear_overlay_index`,
  `scene_point_to_overlay_xy`, `scene_rect_to_overlay_rect`,
  `replace_overlay_region`, `replace_overlay_region_px`, `replace_overlay_region_local`,
  `replace_overlay_region_impl`,
  `page_scene_rect`, `overlay_size`, `canvas_left_top_controls_rect`,
  `rect_from_coords`, `uv_from_scene`,
  `default_rect_coords_for_page`, `default_rect_coords_for_page_idx`.
- Main render pipeline:
  `draw` stays in the facade, while `begin_canvas_frame`, `draw_canvas_scene`,
  `reserve_canvas_page_frame`, `draw_canvas_page_layers`,
  `handle_canvas_page_interactions`, `draw_canvas_viewport_ui`,
  `draw_canvas_controls`, and `draw_canvas_settings_section`
  now live in `scene.rs`.
  Bubble aside/on-top rendering, hit-tests, and drag/resize widgets live in
  `bubble_aside_ui` / `bubble_on_top_ui` and are called from the scene pass.
  Canvas context-menu on page image is shown only when `editable == true`.
  `draw_pixel_grid_overlay` lets tabs redraw the transient inspection grid over
  their own late-painted overlays.
- Runtime/model synchronization and clipboard:
  `is_bubble_locally_locked`, `bubble_extra_from_model_or_project`, `hook_bubble_for_runtime`,
  `build_copied_bubble_data`, `apply_copied_bubble_data_to_bid`,
  `note_focused_bubble_text_input`, `capture_clipboard_events`, `request_paste_from_clipboard`,
  `apply_paste_text`, `sync_runtime_from_model_or_project`, `sync_overlays_from_model`,
  `sync_runtime_from_bubbles`, `apply_deferred_remote_updates`, `upsert_runtime_from_bubble`,
  `apply_bubbles_history_snapshot`.
- Settings persistence and visibility windows:
  `apply_canvas_snapshot`, `publish_canvas_settings`, `canvas_snapshot`,
  `queue_canvas_settings_save`.
- Bubble layout/edit helpers:
  `calc_bubble_width`, `aside_scale_factor`, `page_bubbles`, `apply_pending_actions`,
  `schedule_text_upsert`, `commit_text_upsert_now`, `promote_debounced_text_upserts`,
  `flush_bubble_upserts_to_model`, `create_bubble_at`, `place_or_move_bubble`,
  `create_bubble_from_canvas_context_menu`, `move_bubble_anchor`, `move_bubble_anchor_impl`,
  `show_bubble_context_menu`,
  `bubble_min_uv_margin_for_page`,
  `shift_rect_to_include_anchor`, `clamp_anchor_to_rect`, `clamp_anchor_axis_to_rect`, `clamp_rect_shift_axis`,
  `clamp_bubble_shift_axis`.

CanvasView lifecycle impls:
- `Default::default`: initializes state and starts background workers.
- `Drop::drop`: gracefully stops/join worker threads.

CanvasState lifecycle:
- `Default::default`: supplies initial UI/canvas preferences.

Module-level utility functions:
- Overlay/image processing:
  `sanitize_clipboard_text`, `blit_scaled_chunk`, `build_overlay_prepared_tiles`,
  `rgba_from_overlay_tile`, `build_overlay_tile_image`, `paint_line_with_brush`, `paint_circle`.
- Bubble metadata and hashing:
  `bubble_side`, `side_to_string`, `bubbles_stamp`, `bubble_fingerprint`,
  `bubble_fingerprint_with_hasher`, `bubbles_history_hash`.
- Rect coords serialization defaults:
  `default_rect_coords`, `default_rect_coords_from_source_px`,
  `rect_coords_from_bubble`, `rect_coords_from_value`, `read_rect_coord_value`,
  `upsert_rect_coords_into_extra`.
- Text/geometry estimation helpers:
  `estimate_bubble_height`, `measure_text_widget_content_height`,
  `draw_anchor_link`, `page_info_content_size`.

Key CanvasView state groups (important fields):
- Bubble runtime/edit state: `runtime_bubbles`, `selected_bubble`, `move_active_bid`,
  `active_rect_handle`, `aside_drag_state`, `bubble_undo_stack`, `bubble_redo_stack`,
  `pending_*`, `focused_bubbles`, `deferred_remote_*`, `canvas_context_menu_target`.
- Page/view state: `page_rects`, `scroll_center_idx`, `scroll_offset`, `visible_scene_rect`,
  `scroll_inner_rect`, `pending_zoom_anchor`, `pending_scroll_offset`.
- Overlay runtime: `overlays_visible`, `overlay_images`, `overlay_textures`,
  `overlay_dirty_tiles`, `overlay_prepare_*`, `overlay_prepared_pages`,
  `overlay_upload_min_interval_s`, `overlay_last_upload_s`.
- Shared-model bindings: `bubbles_model`, `overlays_model`, `synced_*_revision`.
- Interaction gates: `drag_scroll_blocked`, `wheel_scroll_blocked`, `zoom_blocked`,
  `zoom_drag_active`, `zoom_drag_last_x`, `overlay_render_suppressed`.
- Async settings persistence: `last_published_canvas_snapshot`,
  `canvas_settings_save_tx`, `canvas_settings_save_thread`.
- Pixel inspection: `pixel_sampling_nearest` and `pixel_grid_visible` are transient canvas render
  switches for tab-owned inspection modes; they are not persisted in `CanvasState`.
*/

use self::bubble_runtime::BubbleRuntimeState;
use self::helpers::*;
use self::overlay_runtime::OverlayRuntimeState;
use self::scene::CanvasSceneState;
use self::settings::CanvasSettingsRuntime;
use self::types::{OverlayUploadBudget, RuntimeBubble};
use crate::app::{PageImageInfo, PageTexture};
use crate::bubble_status::BubbleBorderStyle;
use crate::memory_manager::{CacheEvictionReport, CacheEvictionRequest, CacheResourceInfo};
use crate::models::bubbles_model::runtime_bubble_to_record;
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::project::{Bubble, ProjectData};
use crate::runtime_log;
use crate::widgets::{queue_word_to_global_exceptions, queue_word_to_project_exceptions};
use arboard::Clipboard;
use eframe::egui;
use egui::{Pos2, Rect, Vec2};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasViewportSnapshot {
    pub zoom: f32,
    pub scroll_offset: Vec2,
}

pub(crate) const BUBBLE_ORIGINAL_SPELLCHECK_DISABLED_KEY: &str = "spellcheck_original_disabled";
pub(crate) const BUBBLE_TRANSLATION_SPELLCHECK_DISABLED_KEY: &str =
    "spellcheck_translation_disabled";

mod bubble_aside_ui;
mod bubble_on_top_ui;
mod bubble_runtime;
mod helpers;
mod overlay_runtime;
mod scene;
mod settings;
mod types;
mod workers;

pub(crate) use self::settings::{
    save_canvas_settings_to_project_file, save_canvas_settings_to_user_file,
};

pub use self::workers::spawn_overlay_autosave_thread;

pub use self::types::{
    AsideBubbleCompactMode, AsideBubbleSideMode, BubbleAction, BubbleCopyPasteTarget, BubbleMode,
    BubbleTextField, BubbleType, CanvasState, CanvasUiStatus, OnTopFocusMode, OverlayRectPx,
    RectCoords, SourceTextureUploadBudget,
};

const TEXT_UPSERT_DEBOUNCE_SECS: f64 = 1.0;
const OVERLAY_TILE_SIDE: usize = 1024;
const OVERLAY_UPLOAD_TILE_BUDGET_PER_FRAME: usize = 2;
const OVERLAY_UPLOAD_BYTES_BUDGET_PER_FRAME: usize = 8 * 1024 * 1024;
const OVERLAY_FULL_REBUILD_DIRTY_TILES_THRESHOLD: usize = 12;
const ON_TOP_FOCUS_GAP_PX: f32 = 30.0;
const ON_TOP_MOVE_HANDLE_OFFSET_PX: Vec2 = Vec2::new(16.0, -16.0);
const ON_TOP_MOVE_HANDLE_RADIUS_PX: f32 = 12.0;
const DEFAULT_BUBBLE_RECT_SIDE_SRC_PX: f32 = 100.0;
const LEGACY_DEFAULT_RECT_DELTA_UV: f32 = 0.08;
const BUBBLE_HISTORY_LIMIT: usize = 128;
const BUBBLE_MIN_ANCHOR_MARGIN_PX: f32 = 10.0;
const BUBBLE_ANCHOR_OUTSIDE_RECT_SPAN_MULT: f32 = 0.0;
const DUPLICATE_BUBBLE_OFFSET_PX: f32 = 40.0;
const ON_TOP_FOOTER_RESERVED_HEIGHT_PX: f32 = 220.0;

pub trait CanvasHooks {
    fn wants_canvas_shift_drag_selection(&self, _ctx: &egui::Context) -> bool {
        false
    }

    fn draw_canvas_mask_overlay_on_page(
        &mut self,
        _ui: &mut egui::Ui,
        _ctx: &egui::Context,
        _page_idx: usize,
        _image_rect: Rect,
        _zoom: f32,
    ) {
    }

    fn draw_canvas_overlay_on_page(
        &mut self,
        _ui: &mut egui::Ui,
        _ctx: &egui::Context,
        _page_idx: usize,
        _image_rect: Rect,
        _zoom: f32,
    ) {
    }

    fn draw_canvas_overlay_top_left(
        &mut self,
        _ctx: &egui::Context,
        _canvas_rect: egui::Rect,
        _canvas: &mut CanvasView,
        _project: &ProjectData,
        _status: CanvasUiStatus,
    ) {
    }

    fn has_bubble_header(&mut self, _bubble: &Bubble, _editable: bool) -> bool {
        false
    }

    fn build_bubble_header(&mut self, _ui: &mut egui::Ui, _bubble: &Bubble, _editable: bool) {}

    fn readonly_aside_header_width_hint(
        &mut self,
        _ui: &egui::Ui,
        _bubble: &Bubble,
        _editable: bool,
    ) -> Option<f32> {
        None
    }

    fn build_bubble_footer(&mut self, _ui: &mut egui::Ui, _bubble: &Bubble, _editable: bool) {}

    fn bubble_status_style(
        &mut self,
        _bubble: &Bubble,
        _editable: bool,
        _canvas: &CanvasView,
    ) -> Option<BubbleBorderStyle> {
        None
    }

    fn should_hide_on_top_bubble(
        &mut self,
        _page_idx: usize,
        _bubble: &Bubble,
        _bubble_rect: Rect,
    ) -> bool {
        false
    }

    fn should_hide_aside_bubble_line(
        &mut self,
        _page_idx: usize,
        _bubble: &Bubble,
        _line_start: Pos2,
        _line_end: Pos2,
    ) -> bool {
        false
    }

    fn on_bubble_action(&mut self, _action: BubbleAction, _bubble_id: i64) {}

    fn draw_canvas_page_context_menu(
        &mut self,
        _ui: &mut egui::Ui,
        _project: &ProjectData,
        _page_idx: usize,
        _page_uv: Pos2,
    ) -> bool {
        false
    }

    fn suppress_canvas_page_context_menu(&self, _page_idx: usize) -> bool {
        false
    }
}

pub struct CanvasDrawParams<'a> {
    pub ctx: &'a egui::Context,
    pub ui: &'a mut egui::Ui,
    pub project: &'a ProjectData,
    pub page_infos: &'a HashMap<usize, PageImageInfo>,
    pub texture_cache: &'a mut HashMap<usize, PageTexture>,
    pub status: CanvasUiStatus,
    pub source_upload_budget: &'a mut SourceTextureUploadBudget,
    pub hooks: &'a mut dyn CanvasHooks,
}

pub struct CanvasView {
    pub state: CanvasState,
    pub editable: bool,
    scroll_area_id_salt: &'static str,
    create_bubble_shortcut_hint: Option<String>,
    bubble_runtime: BubbleRuntimeState,
    overlay_runtime: OverlayRuntimeState,
    scene: CanvasSceneState,
    settings_runtime: CanvasSettingsRuntime,
    bubble_unicode_fonts_initialized: bool,
    pixel_sampling_nearest: bool,
    pixel_grid_visible: bool,
}

impl Default for CanvasView {
    fn default() -> Self {
        Self {
            state: CanvasState::default(),
            editable: true,
            scroll_area_id_salt: "canvas_scroll",
            create_bubble_shortcut_hint: None,
            bubble_runtime: BubbleRuntimeState::default(),
            overlay_runtime: OverlayRuntimeState::default(),
            scene: CanvasSceneState::default(),
            settings_runtime: CanvasSettingsRuntime::default(),
            bubble_unicode_fonts_initialized: false,
            pixel_sampling_nearest: false,
            pixel_grid_visible: false,
        }
    }
}

impl Drop for CanvasView {
    fn drop(&mut self) {
        self.overlay_runtime.shutdown();
        if self
            .settings_runtime
            .canvas_settings_save_tx
            .send(None)
            .is_err()
        {
            runtime_log::log_warn(
                "[canvas::settings] failed to signal canvas settings saver shutdown",
            );
        }
        if let Some(handle) = self.settings_runtime.canvas_settings_save_thread.take()
            && handle.join().is_err()
        {
            runtime_log::log_warn(
                "[canvas::settings] canvas settings saver thread panicked during shutdown",
            );
        }
    }
}

impl CanvasView {
    fn ensure_bubble_unicode_fonts(&mut self, ctx: &egui::Context, project: &ProjectData) {
        if self.bubble_unicode_fonts_initialized {
            return;
        }
        self.bubble_unicode_fonts_initialized = true;

        let Some(fonts_dir) = resolve_canvas_ui_fonts_dir(project) else {
            runtime_log::log_warn(
                "[canvas::fonts] fonts/ui directory not found; bubble unicode fallback disabled",
            );
            return;
        };

        let font_paths = collect_canvas_ui_font_paths(&fonts_dir);
        if font_paths.is_empty() {
            runtime_log::log_warn(format!(
                "[canvas::fonts] no font files found in {}; bubble unicode fallback disabled",
                fonts_dir.display()
            ));
            return;
        }

        let mut loaded_paths = Vec::new();
        for (idx, font_path) in font_paths.iter().enumerate() {
            let font_bytes = match fs::read(font_path) {
                Ok(bytes) => bytes,
                Err(err) => {
                    runtime_log::log_warn(format!(
                        "[canvas::fonts] failed to read UI font '{}': {err}",
                        font_path.display()
                    ));
                    continue;
                }
            };

            let font_name = format!("canvas-bubble-unicode-{idx}");
            ctx.add_font(egui::epaint::text::FontInsert::new(
                font_name.as_str(),
                egui::FontData::from_owned(font_bytes),
                vec![egui::epaint::text::InsertFontFamily {
                    family: bubble_text_font_family(),
                    priority: egui::epaint::text::FontPriority::Highest,
                }],
            ));
            loaded_paths.push(font_path.display().to_string());
        }

        if loaded_paths.is_empty() {
            runtime_log::log_warn(format!(
                "[canvas::fonts] failed to load any UI fonts from {}",
                fonts_dir.display()
            ));
            return;
        }

        runtime_log::log_info(format!(
            "[canvas::fonts] loaded {} UI unicode fonts from {}: {}",
            loaded_paths.len(),
            fonts_dir.display(),
            loaded_paths.join(", ")
        ));
    }

    pub fn set_create_bubble_shortcut_hint(&mut self, shortcut_hint: Option<String>) {
        self.create_bubble_shortcut_hint = shortcut_hint;
    }
}

impl CanvasView {
    fn ensure_overlay_for_page_size(&mut self, page_idx: usize, size_px: [usize; 2]) -> bool {
        self.overlay_runtime
            .ensure_overlay_for_page_size(page_idx, size_px)
    }

    fn page_source_size_from_scene(&self, page_idx: usize) -> Option<[usize; 2]> {
        let page_rect = self.page_world_rect(page_idx)?;
        let w = page_rect.width().round().max(1.0) as usize;
        let h = page_rect.height().round().max(1.0) as usize;
        Some([w, h])
    }

    fn reset_overlay_prepare_state(&mut self, page_idx: usize) {
        self.overlay_runtime.reset_prepare_state(page_idx);
    }

    fn poll_overlay_prepare_results(&mut self) {
        self.overlay_runtime.poll_prepare_results();
    }

    fn has_pending_overlay_work(&self) -> bool {
        self.overlay_runtime.has_pending_work()
    }

    pub fn set_overlays_model(&mut self, model: Arc<Mutex<CleanOverlaysModel>>) {
        self.overlay_runtime.set_model(model);
        self.sync_cache_pages_setting_to_model();
    }

    pub fn clean_overlays_model_handle(&self) -> Option<Arc<Mutex<CleanOverlaysModel>>> {
        self.overlay_runtime.model_handle()
    }

    pub fn hook_bubbles_snapshot(&self, project: &ProjectData) -> Vec<Bubble> {
        let mut bubbles = project.bubbles.as_ref().clone();
        let mut known_ids: HashSet<i64> = bubbles.iter().map(|bubble| bubble.id).collect();
        let mut runtime_only = self
            .bubble_runtime
            .runtime_bubbles
            .values()
            .filter(|runtime| known_ids.insert(runtime.id))
            .map(|runtime| self.hook_bubble_for_runtime(project, runtime))
            .collect::<Vec<_>>();
        bubbles.append(&mut runtime_only);
        bubbles.sort_by_key(|bubble| bubble.id);
        bubbles
    }

    pub fn set_drag_scroll_blocked(&mut self, blocked: bool) {
        self.scene.drag_scroll_blocked = blocked;
    }

    pub fn set_wheel_scroll_blocked(&mut self, blocked: bool) {
        self.scene.wheel_scroll_blocked = blocked;
    }

    pub fn set_zoom_blocked(&mut self, blocked: bool) {
        self.scene.zoom_blocked = blocked;
    }

    pub fn set_overlay_render_suppressed(&mut self, suppressed: bool) {
        self.overlay_runtime.set_render_suppressed(suppressed);
    }

    pub fn set_overlay_upload_min_interval_s(&mut self, seconds: f64) {
        self.overlay_runtime.set_upload_min_interval_s(seconds);
    }

    pub fn set_pixel_sampling_nearest(&mut self, enabled: bool) {
        self.pixel_sampling_nearest = enabled;
    }

    pub fn set_pixel_grid_visible(&mut self, visible: bool) {
        self.pixel_grid_visible = visible;
    }

    pub fn pixel_sampling_nearest(&self) -> bool {
        self.pixel_sampling_nearest
    }

    pub fn source_pixel_inspection_active(&self) -> bool {
        self.pixel_sampling_nearest
    }

    pub fn draw_pixel_grid_overlay(&self, ui: &mut egui::Ui) {
        self.draw_visible_pixel_grid_overlay(ui);
    }

    pub fn active_source_page_window(&self, neighbor_radius: usize) -> HashSet<usize> {
        let mut active = HashSet::new();
        if let Some(visible_scene_rect) = self.scene.visible_scene_rect {
            for (page_idx, rect) in self.scene.page_rects.iter().enumerate() {
                if rect.is_positive() && rect.intersects(visible_scene_rect) {
                    active.insert(page_idx);
                }
            }
        }
        let center = self.scene.scroll_center_idx;
        let first = center.saturating_sub(neighbor_radius);
        let last = center.saturating_add(neighbor_radius);
        active.extend(first..=last);
        active
    }

    pub fn set_clean_overlays_visible(&mut self, visible: bool) {
        self.overlay_runtime.set_clean_overlays_visible(visible);
    }

    pub fn set_clean_overlays_visible_for_canvas_only(&mut self, visible: bool) {
        self.overlay_runtime
            .set_clean_overlays_visible_for_canvas_only(visible);
    }

    pub fn clean_overlays_visible(&self) -> bool {
        self.overlay_runtime.clean_overlays_visible()
    }

    pub fn clean_overlay_gpu_memory_snapshot(
        &self,
        pinned_pages: &std::collections::BTreeSet<usize>,
    ) -> Vec<CacheResourceInfo> {
        self.overlay_runtime.memory_usage_snapshot(pinned_pages)
    }

    pub fn evict_clean_overlay_gpu_cache(
        &mut self,
        request: &CacheEvictionRequest,
    ) -> CacheEvictionReport {
        self.overlay_runtime.evict_cache(request)
    }

    pub fn current_page_idx(&self) -> usize {
        self.scene.scroll_center_idx
    }

    pub fn set_scroll_area_id_salt(&mut self, id_salt: &'static str) {
        self.scroll_area_id_salt = id_salt;
    }

    pub fn viewport_snapshot(&self) -> CanvasViewportSnapshot {
        CanvasViewportSnapshot {
            zoom: self.state.zoom,
            scroll_offset: self.scene.scroll_offset,
        }
    }

    pub fn apply_viewport_snapshot(&mut self, snapshot: CanvasViewportSnapshot) {
        self.state.zoom = snapshot.zoom.clamp(0.2, 5.0);
        let scroll_offset = egui::vec2(
            snapshot.scroll_offset.x.max(0.0),
            snapshot.scroll_offset.y.max(0.0),
        );
        self.scene.scroll_offset = scroll_offset;
        self.scene.pending_scroll_offset = Some(scroll_offset);
        self.scene.pending_zoom_anchor = None;
        self.scene.initial_horizontal_scroll_centered = true;
    }

    pub fn zoom_by_shortcut(&mut self, factor: f32) -> bool {
        if factor <= 0.0 {
            return false;
        }
        self.apply_zoom_value(self.state.zoom * factor, None, None)
    }

    pub fn reset_zoom_shortcut(&mut self) -> bool {
        self.apply_zoom_value(1.0, None, None)
    }

    pub fn zoom(&self) -> f32 {
        self.state.zoom
    }

    pub fn page_index_at_scene_pos(&self, scene_pos: Pos2) -> Option<usize> {
        self.scene
            .page_rects
            .iter()
            .position(|rect| rect.contains(scene_pos))
    }

    pub fn bubble_original_text(&self, bubble_id: i64) -> Option<String> {
        self.bubble_runtime
            .runtime_bubbles
            .get(&bubble_id)
            .map(|bubble| bubble.original_text.clone())
    }

    pub fn page_contains_scene_pos(&self, page_idx: usize, scene_pos: Pos2) -> bool {
        self.scene
            .page_rects
            .get(page_idx)
            .is_some_and(|rect| rect.contains(scene_pos))
    }

    pub fn visible_scene_rect(&self) -> Option<Rect> {
        self.scene.visible_scene_rect
    }

    pub fn overlay_image(&self, page_idx: usize) -> Option<&egui::ColorImage> {
        self.overlay_runtime.overlay_image(page_idx)
    }

    pub fn clear_overlay_index(&mut self, page_idx: usize) {
        let Some(size_px) = self.page_source_size_from_scene(page_idx) else {
            return;
        };
        self.overlay_runtime.clear_overlay_index(page_idx, size_px);
    }

    pub fn scene_point_to_overlay_xy(
        &self,
        page_idx: usize,
        scene_pt: Pos2,
    ) -> Option<(usize, usize)> {
        let page_rect = self.page_scene_rect(page_idx)?;
        self.overlay_runtime
            .scene_point_to_overlay_xy(page_idx, scene_pt, page_rect)
    }

    pub fn scene_rect_to_overlay_rect(
        &self,
        page_idx: usize,
        scene_rect: Rect,
    ) -> Option<OverlayRectPx> {
        let page_rect = self.page_scene_rect(page_idx)?;
        self.overlay_runtime
            .scene_rect_to_overlay_rect(page_idx, scene_rect, page_rect)
    }

    pub fn replace_overlay_region(
        &mut self,
        page_idx: usize,
        scene_rect: Rect,
        chunk: &egui::ColorImage,
    ) -> bool {
        self.replace_overlay_region_impl(page_idx, scene_rect, chunk, true)
    }

    pub fn replace_overlay_region_px(
        &mut self,
        page_idx: usize,
        target: OverlayRectPx,
        chunk: &egui::ColorImage,
    ) -> bool {
        let Some(size_px) = self
            .overlay_size(page_idx)
            .or_else(|| self.page_source_size_from_scene(page_idx))
        else {
            return false;
        };
        self.overlay_runtime
            .replace_overlay_region_px(page_idx, size_px, target, chunk, true)
    }

    pub fn replace_overlay_region_local(
        &mut self,
        page_idx: usize,
        scene_rect: Rect,
        chunk: &egui::ColorImage,
    ) -> bool {
        self.replace_overlay_region_impl(page_idx, scene_rect, chunk, false)
    }

    fn replace_overlay_region_impl(
        &mut self,
        page_idx: usize,
        scene_rect: Rect,
        chunk: &egui::ColorImage,
        sync_model: bool,
    ) -> bool {
        let Some(size_px) = self.page_source_size_from_scene(page_idx) else {
            return false;
        };
        let Some(page_rect) = self.page_scene_rect(page_idx) else {
            return false;
        };
        self.overlay_runtime
            .replace_overlay_region(page_idx, size_px, scene_rect, page_rect, chunk, sync_model)
    }

    pub fn commit_overlay_page_to_model(&mut self, page_idx: usize) -> bool {
        self.overlay_runtime.commit_overlay_page_to_model(page_idx)
    }

    fn mark_overlay_dirty_full(&mut self, page_idx: usize) {
        self.overlay_runtime.mark_dirty_full(page_idx);
    }

    pub fn draw(&mut self, params: CanvasDrawParams<'_>) {
        let CanvasDrawParams {
            ctx,
            ui,
            project,
            page_infos,
            texture_cache,
            status,
            source_upload_budget,
            hooks,
        } = params;
        self.ensure_bubble_unicode_fonts(ctx, project);
        self.poll_overlay_prepare_results();
        self.sync_overlays_from_model();
        self.sync_runtime_from_model_or_project(project);
        self.bubble_runtime.focused_bubbles.clear();
        self.bubble_runtime.focused_text_input = None;
        self.scene.on_top_hit_rects.clear();
        self.scene.page_aside_presence.clear();
        if self.state.show_bubbles {
            for bubble in self.bubble_runtime.runtime_bubbles.values() {
                if self.displayed_bubble_type_for_runtime(bubble) != BubbleType::Aside {
                    continue;
                }
                let presence = self
                    .scene
                    .page_aside_presence
                    .entry(bubble.img_idx)
                    .or_insert([false, false]);
                match bubble.side {
                    crate::project::Side::Left => presence[0] = true,
                    crate::project::Side::Right => presence[1] = true,
                }
            }
        }
        let canvas_rect = ui.max_rect();
        let (suppress_wheel_scroll, zoom_drag_active) =
            self.handle_shortcuts(ctx, project, canvas_rect);
        self.scene.page_rects.clear();
        self.scene.page_world_rects.clear();
        self.scene.page_aside_widths.clear();
        self.scene.visible_scene_rect = None;
        self.overlay_runtime
            .overlay_textures
            .retain(|idx, _| self.overlay_runtime.overlay_images.contains_key(idx));
        self.overlay_runtime
            .overlay_texture_last_used_frame
            .retain(|idx, _| self.overlay_runtime.overlay_images.contains_key(idx));
        self.overlay_runtime
            .overlay_dirty_tiles
            .retain(|idx, _| self.overlay_runtime.overlay_images.contains_key(idx));
        self.overlay_runtime
            .overlay_last_upload_s
            .retain(|idx, _| self.overlay_runtime.overlay_images.contains_key(idx));
        self.overlay_runtime
            .overlay_prepare_inflight
            .retain(|idx, _| self.overlay_runtime.overlay_images.contains_key(idx));
        self.overlay_runtime
            .overlay_prepared_pages
            .retain(|idx, _| self.overlay_runtime.overlay_images.contains_key(idx));
        if !self.editable {
            self.bubble_runtime.canvas_context_menu_target = None;
        }
        let frame = self.begin_canvas_frame(
            ctx,
            canvas_rect,
            suppress_wheel_scroll,
            zoom_drag_active,
            hooks,
        );
        let mut overlay_budget = OverlayUploadBudget {
            tile_budget: OVERLAY_UPLOAD_TILE_BUDGET_PER_FRAME,
            bytes_budget: OVERLAY_UPLOAD_BYTES_BUDGET_PER_FRAME,
        };
        let scroll_output = self.draw_canvas_scene(scene::CanvasSceneDrawParams {
            ctx,
            ui,
            project,
            page_infos,
            texture_cache,
            hooks,
            frame,
            overlay_budget: &mut overlay_budget,
            source_upload_budget,
        });
        self.scene.scroll_offset = egui::vec2(
            scroll_output.state.offset.x.max(0.0),
            scroll_output.state.offset.y.max(0.0),
        );
        self.scene.scroll_inner_rect = Some(scroll_output.inner_rect);
        self.scene.scroll_content_size = scroll_output.content_size;
        self.scene.pending_zoom_anchor = None;
        if self.bubble_runtime.aside_drag_state.is_some()
            && !ctx.input(|i| i.pointer.primary_down())
            && let Some(bid) = self.bubble_runtime.aside_drag_state.map(|state| state.bid)
        {
            bubble_aside_ui::finish_aside_drag(self, bid);
        }

        self.capture_clipboard_events(project, ctx);
        self.apply_pending_actions(hooks);
        let now_s = ctx.input(|i| i.time);
        self.promote_debounced_text_upserts(now_s);
        self.apply_deferred_remote_updates();
        self.flush_bubble_upserts_to_model(project);
        self.draw_canvas_viewport_ui(ctx, project, status, frame, hooks);
        self.publish_canvas_settings(project);
        if self.has_pending_overlay_work() {
            ctx.request_repaint();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn show_bubble_context_menu(
        &mut self,
        ui: &mut egui::Ui,
        project: &ProjectData,
        bid: i64,
        bubble_type: BubbleType,
        original_text: &str,
        translated_text: &str,
        _misspelled_word: Option<&str>,
        want_copy_whole_bubble: &mut bool,
        want_duplicate_bubble: &mut bool,
        want_paste_whole_bubble: &mut bool,
        want_paste_original: &mut bool,
        want_paste_translation: &mut bool,
        want_switch_bubble_type: &mut Option<BubbleType>,
        interacted_with_bubble: &mut bool,
    ) {
        if ui.button("Копировать пузырь").clicked() {
            *want_copy_whole_bubble = true;
            *interacted_with_bubble = true;
            ui.close();
        }
        if ui
            .add_enabled(self.editable, egui::Button::new("Дублировать пузырь"))
            .clicked()
        {
            *want_duplicate_bubble = true;
            *interacted_with_bubble = true;
            ui.close();
        }
        if ui
            .add_enabled(
                self.editable && self.bubble_runtime.copied_bubble_data.is_some(),
                egui::Button::new("Вставить пузырь"),
            )
            .clicked()
        {
            *want_paste_whole_bubble = true;
            *interacted_with_bubble = true;
            ui.close();
        }
        ui.separator();
        if ui.button("Копировать оригинал").clicked() {
            ui.ctx().copy_text(original_text.to_owned());
            *interacted_with_bubble = true;
            ui.close();
        }
        if ui.button("Копировать перевод").clicked() {
            ui.ctx().copy_text(translated_text.to_owned());
            *interacted_with_bubble = true;
            ui.close();
        }
        ui.separator();
        if ui
            .add_enabled(
                self.editable && bubble_type != BubbleType::Default,
                egui::Button::new("Сделать стандартным"),
            )
            .clicked()
        {
            *want_switch_bubble_type = Some(BubbleType::Default);
            *interacted_with_bubble = true;
            ui.close();
        }
        if ui
            .add_enabled(
                self.editable && bubble_type != BubbleType::Aside,
                egui::Button::new("Сделать сбоку"),
            )
            .clicked()
        {
            *want_switch_bubble_type = Some(BubbleType::Aside);
            *interacted_with_bubble = true;
            ui.close();
        }
        if ui
            .add_enabled(
                self.editable && bubble_type != BubbleType::OnTop,
                egui::Button::new("Сделать поверх"),
            )
            .clicked()
        {
            *want_switch_bubble_type = Some(BubbleType::OnTop);
            *interacted_with_bubble = true;
            ui.close();
        }
        ui.separator();
        if ui
            .add_enabled(self.editable, egui::Button::new("Вставить в оригинал"))
            .clicked()
        {
            *want_paste_original = true;
            *interacted_with_bubble = true;
            ui.close();
        }
        if ui
            .add_enabled(self.editable, egui::Button::new("Вставить в перевод"))
            .clicked()
        {
            *want_paste_translation = true;
            *interacted_with_bubble = true;
            ui.close();
        }
        if self.editable {
            ui.separator();
            let mut original_spellcheck_enabled =
                !self.bubble_spellcheck_disabled(project, bid, BubbleTextField::Original);
            if ui
                .checkbox(
                    &mut original_spellcheck_enabled,
                    "Проверять орфографию в оригинале",
                )
                .changed()
            {
                let disabled = !original_spellcheck_enabled;
                if self.set_bubble_spellcheck_disabled(
                    project,
                    bid,
                    BubbleTextField::Original,
                    disabled,
                ) {
                    *interacted_with_bubble = true;
                }
            }
            let mut translation_spellcheck_enabled =
                !self.bubble_spellcheck_disabled(project, bid, BubbleTextField::Translation);
            if ui
                .checkbox(
                    &mut translation_spellcheck_enabled,
                    "Проверять орфографию в переводе",
                )
                .changed()
            {
                let disabled = !translation_spellcheck_enabled;
                if self.set_bubble_spellcheck_disabled(
                    project,
                    bid,
                    BubbleTextField::Translation,
                    disabled,
                ) {
                    *interacted_with_bubble = true;
                }
            }
        }
        if let Some(word) = self
            .bubble_runtime
            .bubble_context_menu_misspelled_word
            .as_deref()
        {
            ui.separator();
            ui.label(format!("Орфография: {word}"));
            if ui.button("Добавить в общие исключения").clicked() {
                queue_word_to_global_exceptions(word);
                *interacted_with_bubble = true;
                ui.close();
            }
            if ui.button("Добавить в исключения проекта").clicked() {
                queue_word_to_project_exceptions(word);
                *interacted_with_bubble = true;
                ui.close();
            }
        }
    }

    fn handle_shortcuts(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        canvas_rect: Rect,
    ) -> (bool, bool) {
        let (mods, wheel_delta_y, hover_pos, interact_pos, z_down, primary_down) = ctx.input(|i| {
            (
                i.modifiers,
                i.smooth_scroll_delta.y + i.raw_scroll_delta.y,
                i.pointer.hover_pos(),
                i.pointer.interact_pos(),
                i.key_down(egui::Key::Z),
                i.pointer.primary_down(),
            )
        });
        let ctrl_or_command = mods.ctrl || mods.command;
        let zoom_modifier_down = ctrl_or_command || z_down;
        let pointer_pos = interact_pos.or(hover_pos);
        let inside_canvas = pointer_pos
            .map(|p| canvas_rect.contains(p))
            .unwrap_or(false);
        if self.editable && !ctx.wants_keyboard_input() {
            let command_shift_mods = egui::Modifiers {
                command: true,
                shift: true,
                ..Default::default()
            };
            let (redo, undo) = ctx.input_mut(|i| {
                (
                    i.consume_key(command_shift_mods, egui::Key::Z),
                    i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z),
                )
            });
            if (redo && self.try_redo_bubbles_history())
                || (undo && self.try_undo_bubbles_history())
            {
                ctx.request_repaint();
            }
        }
        let has_focused_bubble = self
            .bubble_runtime
            .selected_bubble
            .is_some_and(|bid| self.bubble_runtime.runtime_bubbles.contains_key(&bid));
        let keyboard_input_active = ctx.wants_keyboard_input();
        let can_duplicate_shortcut = self.editable && has_focused_bubble && !keyboard_input_active;
        let duplicate_shortcut = ctx.input_mut(|i| {
            if can_duplicate_shortcut {
                i.consume_key(egui::Modifiers::COMMAND, egui::Key::D)
            } else {
                false
            }
        });
        if duplicate_shortcut {
            self.duplicate_focused_bubble_shortcut(project, ctx.input(|i| i.time));
        }

        if !self.scene.zoom_blocked
            && inside_canvas
            && wheel_delta_y.abs() > f32::EPSILON
            && (ctrl_or_command || z_down)
        {
            let factor = if wheel_delta_y > 0.0 { 1.1 } else { 1.0 / 1.1 };
            self.apply_zoom_with_anchor(factor, pointer_pos, canvas_rect);
        }
        if self.scene.zoom_blocked {
            self.scene.zoom_drag_active = false;
        } else if self.scene.zoom_drag_active {
            if !zoom_modifier_down || !primary_down {
                self.scene.zoom_drag_active = false;
            } else if let Some(pos) = pointer_pos {
                let dx = pos.x - self.scene.zoom_drag_last_x;
                if dx.abs() > f32::EPSILON {
                    let factor = (dx * 0.005).exp().clamp(0.5, 2.0);
                    self.apply_zoom_with_anchor(factor, pointer_pos, canvas_rect);
                }
                self.scene.zoom_drag_last_x = pos.x;
            }
        } else if zoom_modifier_down
            && primary_down
            && inside_canvas
            && let Some(pos) = pointer_pos
        {
            self.scene.zoom_drag_active = true;
            self.scene.zoom_drag_last_x = pos.x;
        }
        let zoom_drag_now = self.scene.zoom_drag_active
            || (!self.scene.zoom_blocked && zoom_modifier_down && primary_down && inside_canvas);
        if zoom_drag_now {
            ctx.request_repaint();
        }
        if !self.scene.zoom_blocked
            && zoom_modifier_down
            && inside_canvas
            && !ctx.wants_keyboard_input()
        {
            let mut zoom_in = false;
            let mut zoom_out = false;
            let mut zoom_reset = false;
            let key_modifiers = [
                ctrl_or_command.then_some(egui::Modifiers::COMMAND),
                z_down.then_some(egui::Modifiers::NONE),
            ];
            for key_mods in key_modifiers.into_iter().flatten() {
                let (plus, equals, minus, zero) = ctx.input_mut(|i| {
                    (
                        i.consume_key(key_mods, egui::Key::Plus),
                        i.consume_key(key_mods, egui::Key::Equals),
                        i.consume_key(key_mods, egui::Key::Minus),
                        i.consume_key(key_mods, egui::Key::Num0),
                    )
                });
                zoom_in |= plus || equals;
                zoom_out |= minus;
                zoom_reset |= zero;
            }

            if zoom_reset {
                let _ = self.apply_zoom_value(1.0, pointer_pos, Some(canvas_rect));
            } else {
                if zoom_in {
                    self.apply_zoom_with_anchor(1.1, pointer_pos, canvas_rect);
                }
                if zoom_out {
                    self.apply_zoom_with_anchor(1.0 / 1.1, pointer_pos, canvas_rect);
                }
            }
        }
        let suppress_wheel_scroll = (zoom_modifier_down && inside_canvas) || zoom_drag_now;

        (suppress_wheel_scroll, zoom_drag_now)
    }

    fn apply_zoom_with_anchor(
        &mut self,
        factor: f32,
        pointer_pos: Option<Pos2>,
        canvas_rect: Rect,
    ) -> bool {
        if factor <= 0.0 {
            return false;
        }
        self.apply_zoom_value(self.state.zoom * factor, pointer_pos, Some(canvas_rect))
    }

    fn apply_zoom_value(
        &mut self,
        zoom: f32,
        pointer_pos: Option<Pos2>,
        fallback_rect: Option<Rect>,
    ) -> bool {
        let old_zoom = self.state.zoom;
        let new_zoom = zoom.clamp(0.2, 5.0);
        if (new_zoom - old_zoom).abs() <= f32::EPSILON {
            return false;
        }
        if let Some(fallback_rect) = fallback_rect.or(self.scene.scroll_inner_rect) {
            self.capture_pending_zoom_anchor(pointer_pos, fallback_rect);
        }
        self.state.zoom = new_zoom;
        true
    }

    fn is_bubble_locally_locked(&self, bid: i64) -> bool {
        self.bubble_runtime.focused_bubbles.contains(&bid)
            || self.bubble_runtime.pending_text_upsert.contains_key(&bid)
            || self
                .bubble_runtime
                .pending_bubble_paste
                .is_some_and(|pending| pending.bid == bid)
    }

    fn bubble_extra_from_model_or_project(
        &self,
        project: &ProjectData,
        bid: i64,
    ) -> Map<String, Value> {
        if let Some(model) = self.bubble_runtime.bubbles_model.as_ref().map(Arc::clone)
            && let Ok(locked) = model.lock()
            && let Some(extra) = locked
                .snapshot()
                .into_iter()
                .find(|bubble| bubble.id == bid)
                .map(|bubble| bubble.extra)
        {
            return extra;
        }
        project
            .bubbles
            .iter()
            .find(|bubble| bubble.id == bid)
            .map(|bubble| bubble.extra.clone())
            .unwrap_or_default()
    }

    fn bubble_extra_without_rect_coords(
        &self,
        project: &ProjectData,
        bid: i64,
    ) -> Map<String, Value> {
        let mut extra = self.bubble_extra_from_model_or_project(project, bid);
        for key in ["rect_coords", "img_idx", "img_u", "img_v", "side"] {
            extra.remove(key);
        }
        extra
    }

    fn bubble_spellcheck_disabled(
        &self,
        project: &ProjectData,
        bid: i64,
        field: BubbleTextField,
    ) -> bool {
        self.bubble_extra_from_model_or_project(project, bid)
            .get(bubble_spellcheck_disabled_key(field))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    fn set_bubble_spellcheck_disabled(
        &mut self,
        project: &ProjectData,
        bid: i64,
        field: BubbleTextField,
        disabled: bool,
    ) -> bool {
        let Some(runtime) = self.bubble_runtime.runtime_bubbles.get(&bid).cloned() else {
            return false;
        };
        let mut extra = self.bubble_extra_without_rect_coords(project, bid);
        let key = bubble_spellcheck_disabled_key(field);
        let was_disabled = extra.get(key).and_then(Value::as_bool).unwrap_or(false);
        if was_disabled == disabled {
            return false;
        }
        if disabled {
            extra.insert(key.to_string(), Value::Bool(true));
        } else {
            extra.remove(key);
        }
        upsert_rect_coords_into_extra(&mut extra, runtime.rect_coords);

        let rec = runtime_bubble_to_record(
            runtime.id,
            runtime.img_idx,
            runtime.img_u,
            runtime.img_v,
            Some(side_to_string(runtime.side)),
            Some(runtime.bubble_type.as_str().to_string()),
            runtime.text,
            runtime.original_text,
            Some(extra),
        );

        let Some(model) = self.bubble_runtime.bubbles_model.as_ref().map(Arc::clone) else {
            runtime_log::log_warn(format!(
                "[canvas] failed to persist spellcheck flag without BubblesModel; bubble_id={bid}"
            ));
            return false;
        };

        self.capture_bubble_history_before_mutation();
        match model.lock() {
            Ok(mut locked) => match locked.create_or_replace(rec) {
                Ok(()) => {
                    self.bubble_runtime.synced_bubbles_revision = locked.revision();
                    true
                }
                Err(err) => {
                    runtime_log::log_error(format!(
                        "[canvas] failed to persist spellcheck flag; bubble_id={bid}; error={err:#}"
                    ));
                    false
                }
            },
            Err(_) => {
                runtime_log::log_warn(format!(
                    "[canvas] failed to lock BubblesModel while persisting spellcheck flag; bubble_id={bid}"
                ));
                false
            }
        }
    }

    fn editable_bubble_type_default(&self) -> BubbleType {
        self.state
            .hybrid_editable_bubble_type
            .resolved(BubbleType::OnTop)
    }

    fn readonly_bubble_display_type(&self) -> BubbleType {
        self.state
            .hybrid_readonly_bubble_type
            .resolved(BubbleType::Aside)
    }

    fn displayed_bubble_type_for_runtime(&self, bubble: &RuntimeBubble) -> BubbleType {
        let display_type = if self.editable {
            bubble
                .bubble_type
                .resolved(self.editable_bubble_type_default())
        } else {
            bubble
                .bubble_type
                .resolved(self.readonly_bubble_display_type())
        };
        if self.editable
            && display_type == BubbleType::OnTop
            && self.state.on_top_focus_mode == OnTopFocusMode::Aside
            && self.bubble_runtime.selected_bubble == Some(bubble.id)
        {
            BubbleType::Aside
        } else {
            display_type
        }
    }

    fn effective_bubble_type_for_record(&self, bubble: &Bubble) -> BubbleType {
        bubble
            .bubble_type
            .as_deref()
            .map(BubbleType::from_str)
            .unwrap_or(BubbleType::Default)
    }

    fn set_bubble_type_for_bid(&mut self, bid: i64, bubble_type: BubbleType) -> bool {
        let Some(rt) = self.bubble_runtime.runtime_bubbles.get_mut(&bid) else {
            return false;
        };
        if rt.bubble_type == bubble_type {
            return false;
        }
        rt.bubble_type = bubble_type;
        rt.mounted = false;
        self.bubble_runtime.pending_upsert.insert(bid);
        true
    }

    fn hook_bubble_for_runtime(&self, project: &ProjectData, runtime: &RuntimeBubble) -> Bubble {
        let mut extra = self.bubble_extra_from_model_or_project(project, runtime.id);
        upsert_rect_coords_into_extra(&mut extra, runtime.rect_coords);
        Bubble {
            id: runtime.id,
            img_idx: runtime.img_idx,
            img_u: runtime.img_u,
            img_v: runtime.img_v,
            side: Some(side_to_string(runtime.side)),
            bubble_type: Some(runtime.bubble_type.as_str().to_string()),
            text: runtime.text.clone(),
            original_text: runtime.original_text.clone(),
            extra,
        }
    }

    fn sync_overlays_from_model(&mut self) {
        let Some(delta) = ({
            let Some(model) = self.overlay_runtime.overlays_model.as_ref() else {
                return;
            };
            let Ok(mut locked) = model.lock() else {
                return;
            };
            locked.take_delta(self.overlay_runtime.synced_overlays_revision)
        }) else {
            return;
        };
        if let Some(visible) = delta.visibility {
            self.overlay_runtime.apply_model_visibility(visible);
        }
        for (idx, maybe_img) in delta.changed {
            self.reset_overlay_prepare_state(idx);
            if let Some(img) = maybe_img {
                if img.size[0] > 0 && img.size[1] > 0 {
                    self.overlay_runtime
                        .overlay_images
                        .insert(idx, Arc::new(img));
                    self.mark_overlay_dirty_full(idx);
                } else {
                    self.overlay_runtime.overlay_images.remove(&idx);
                    self.overlay_runtime.overlay_textures.remove(&idx);
                    self.overlay_runtime.overlay_dirty_tiles.remove(&idx);
                    self.overlay_runtime.overlay_last_upload_s.remove(&idx);
                }
            } else {
                self.overlay_runtime.overlay_images.remove(&idx);
                self.overlay_runtime.overlay_textures.remove(&idx);
                self.overlay_runtime.overlay_dirty_tiles.remove(&idx);
                self.overlay_runtime.overlay_last_upload_s.remove(&idx);
            }
        }
        self.overlay_runtime.synced_overlays_revision = delta.revision;
    }

    fn calc_bubble_width(&self, span: f32) -> f32 {
        let min_width = self.state.bubble_min_width.max(1.0);
        if self.state.scale_bubbles {
            span.clamp(min_width, self.state.bubble_max_width.max(min_width))
        } else {
            min_width
        }
    }

    fn aside_scale_factor(&self) -> f32 {
        (self.state.aside_scale_pct as f32 / 100.0).clamp(0.25, 3.0)
    }

    fn move_bubble_anchor(&mut self, bid: i64, u: f32, v: f32, move_rect: bool) {
        self.move_bubble_anchor_impl(bid, u, v, move_rect, true);
    }

    fn move_bubble_anchor_impl(
        &mut self,
        bid: i64,
        u: f32,
        v: f32,
        move_rect: bool,
        mark_upsert: bool,
    ) {
        let Some(page_idx) = self
            .bubble_runtime
            .runtime_bubbles
            .get(&bid)
            .map(|bubble| bubble.img_idx)
        else {
            return;
        };
        let (min_margin_u, min_margin_v) = self.bubble_min_uv_margin_for_page(page_idx);

        let Some(b) = self.bubble_runtime.runtime_bubbles.get_mut(&bid) else {
            return;
        };

        let min_margin_u = min_margin_u.clamp(0.0, 0.49);
        let min_margin_v = min_margin_v.clamp(0.0, 0.49);
        let desired_u = u.clamp(min_margin_u, 1.0 - min_margin_u);
        let desired_v = v.clamp(min_margin_v, 1.0 - min_margin_v);
        let mut rect = b.rect_coords.normalized();

        if move_rect {
            // Anchor can move with configurable overshoot outside rect; rect starts moving
            // only when anchor exceeds that allowance.
            rect = Self::shift_rect_to_include_anchor(rect, desired_u, desired_v);
        }
        let anchor =
            Self::clamp_anchor_to_rect(desired_u, desired_v, rect, min_margin_u, min_margin_v);
        b.rect_coords = rect;
        b.img_u = anchor.x;
        b.img_v = anchor.y;
        if mark_upsert {
            self.bubble_runtime.pending_upsert.insert(bid);
        }
    }

    fn bubble_min_uv_margin_for_page(&self, page_idx: usize) -> (f32, f32) {
        let margin_from_span = |span: f32| -> f32 {
            if span <= f32::EPSILON {
                0.0
            } else {
                (BUBBLE_MIN_ANCHOR_MARGIN_PX / span).clamp(0.0, 0.49)
            }
        };
        if let Some(page_rect) = self.page_scene_rect(page_idx) {
            return (
                margin_from_span(page_rect.width()),
                margin_from_span(page_rect.height()),
            );
        }
        if let Some([w, h]) = self.overlay_size(page_idx) {
            return (margin_from_span(w as f32), margin_from_span(h as f32));
        }
        (0.0, 0.0)
    }

    fn shift_rect_to_include_anchor(rect: RectCoords, anchor_u: f32, anchor_v: f32) -> RectCoords {
        let mut rect = rect.normalized();
        rect.p1.x = rect.p1.x.clamp(0.0, 1.0);
        rect.p1.y = rect.p1.y.clamp(0.0, 1.0);
        rect.p2.x = rect.p2.x.clamp(0.0, 1.0);
        rect.p2.y = rect.p2.y.clamp(0.0, 1.0);
        rect = rect.normalized();

        let span_x = (rect.p2.x - rect.p1.x).max(0.0);
        let extra_x = span_x * BUBBLE_ANCHOR_OUTSIDE_RECT_SPAN_MULT.max(0.0);
        let min_allowed_x = rect.p1.x - extra_x;
        let max_allowed_x = rect.p2.x + extra_x;
        let desired_shift_x = if anchor_u < min_allowed_x {
            anchor_u - min_allowed_x
        } else if anchor_u > max_allowed_x {
            anchor_u - max_allowed_x
        } else {
            0.0
        };
        let shift_x = Self::clamp_rect_shift_axis(rect.p1.x, rect.p2.x, desired_shift_x);
        rect.p1.x += shift_x;
        rect.p2.x += shift_x;

        let span_y = (rect.p2.y - rect.p1.y).max(0.0);
        let extra_y = span_y * BUBBLE_ANCHOR_OUTSIDE_RECT_SPAN_MULT.max(0.0);
        let min_allowed_y = rect.p1.y - extra_y;
        let max_allowed_y = rect.p2.y + extra_y;
        let desired_shift_y = if anchor_v < min_allowed_y {
            anchor_v - min_allowed_y
        } else if anchor_v > max_allowed_y {
            anchor_v - max_allowed_y
        } else {
            0.0
        };
        let shift_y = Self::clamp_rect_shift_axis(rect.p1.y, rect.p2.y, desired_shift_y);
        rect.p1.y += shift_y;
        rect.p2.y += shift_y;

        rect.normalized()
    }

    fn clamp_anchor_to_rect(
        desired_u: f32,
        desired_v: f32,
        rect: RectCoords,
        min_margin_u: f32,
        min_margin_v: f32,
    ) -> Pos2 {
        let rect = rect.normalized();
        egui::pos2(
            Self::clamp_anchor_axis_to_rect(desired_u, rect.p1.x, rect.p2.x, min_margin_u),
            Self::clamp_anchor_axis_to_rect(desired_v, rect.p1.y, rect.p2.y, min_margin_v),
        )
    }

    fn clamp_anchor_axis_to_rect(
        desired: f32,
        rect_min: f32,
        rect_max: f32,
        min_margin: f32,
    ) -> f32 {
        let low = rect_min.min(rect_max).clamp(0.0, 1.0);
        let high = rect_min.max(rect_max).clamp(0.0, 1.0);
        if high < low {
            return desired.clamp(0.0, 1.0);
        }

        let span = (high - low).max(0.0);
        let extra = span * BUBBLE_ANCHOR_OUTSIDE_RECT_SPAN_MULT.max(0.0);
        let margin_low = min_margin.clamp(0.0, 0.49);
        let margin_high = 1.0 - margin_low;
        let min_bound = (low - extra).max(margin_low);
        let max_bound = (high + extra).min(margin_high);
        if min_bound <= max_bound {
            desired.clamp(min_bound, max_bound)
        } else {
            desired.clamp(margin_low, margin_high)
        }
    }

    fn clamp_rect_shift_axis(rect_min: f32, rect_max: f32, desired_shift: f32) -> f32 {
        let low = rect_min.min(rect_max).clamp(0.0, 1.0);
        let high = rect_min.max(rect_max).clamp(0.0, 1.0);
        let min_shift = -low;
        let max_shift = 1.0 - high;
        if min_shift <= max_shift {
            desired_shift.clamp(min_shift, max_shift)
        } else {
            0.0
        }
    }

    fn clamp_bubble_shift_axis(
        rect_min: f32,
        rect_max: f32,
        anchor: f32,
        min_margin: f32,
        desired_shift: f32,
    ) -> f32 {
        let margin = min_margin.clamp(0.0, 0.49);
        let low = rect_min.min(rect_max).clamp(0.0, 1.0);
        let high = rect_min.max(rect_max).clamp(0.0, 1.0);
        let min_shift = (-low).max(margin - anchor);
        let max_shift = (1.0 - high).min((1.0 - margin) - anchor);
        if min_shift <= max_shift {
            desired_shift.clamp(min_shift, max_shift)
        } else {
            0.0
        }
    }

    fn uv_from_scene(image_rect: Rect, pos: Pos2) -> Pos2 {
        let w = image_rect.width().max(1.0);
        let h = image_rect.height().max(1.0);
        egui::pos2(
            ((pos.x - image_rect.left()) / w).clamp(0.0, 1.0),
            ((pos.y - image_rect.top()) / h).clamp(0.0, 1.0),
        )
    }

    fn default_rect_coords_for_page(
        &self,
        page_idx: usize,
        page_rect: Rect,
        u: f32,
        v: f32,
    ) -> RectCoords {
        if let Some([w, h]) = self.overlay_size(page_idx) {
            return default_rect_coords_from_source_px(
                u,
                v,
                w as f32,
                h as f32,
                DEFAULT_BUBBLE_RECT_SIDE_SRC_PX,
            );
        }

        let zoom = self.state.zoom.max(f32::EPSILON);
        let source_w = (page_rect.width() / zoom).max(1.0);
        let source_h = (page_rect.height() / zoom).max(1.0);
        default_rect_coords_from_source_px(
            u,
            v,
            source_w,
            source_h,
            DEFAULT_BUBBLE_RECT_SIDE_SRC_PX,
        )
    }

    fn default_rect_coords_for_page_idx(&self, page_idx: usize, u: f32, v: f32) -> RectCoords {
        if let Some([w, h]) = self.overlay_size(page_idx) {
            return default_rect_coords_from_source_px(
                u,
                v,
                w as f32,
                h as f32,
                DEFAULT_BUBBLE_RECT_SIDE_SRC_PX,
            );
        }

        if let Some(page_rect) = self.page_scene_rect(page_idx) {
            return self.default_rect_coords_for_page(page_idx, page_rect, u, v);
        }

        default_rect_coords(
            u,
            v,
            LEGACY_DEFAULT_RECT_DELTA_UV,
            LEGACY_DEFAULT_RECT_DELTA_UV,
        )
    }

    fn page_world_rect(&self, page_idx: usize) -> Option<Rect> {
        let rect = self.scene.page_world_rects.get(page_idx).copied()?;
        if !rect.is_positive() {
            return None;
        }
        Some(rect)
    }

    pub fn page_scene_rect(&self, page_idx: usize) -> Option<Rect> {
        let rect = self.scene.page_rects.get(page_idx).copied()?;
        if !rect.is_positive() {
            return None;
        }
        Some(rect)
    }

    pub fn overlay_size(&self, page_idx: usize) -> Option<[usize; 2]> {
        let size = self.overlay_runtime.overlay_images.get(&page_idx)?.size;
        if size[0] == 0 || size[1] == 0 {
            return None;
        }
        Some(size)
    }

    pub fn canvas_left_top_controls_rect(&self) -> Option<Rect> {
        self.scene.canvas_left_top_controls_rect
    }

    fn rect_from_coords(image_rect: Rect, coords: RectCoords) -> Rect {
        let p1 = egui::pos2(
            image_rect.left() + image_rect.width() * coords.p1.x.clamp(0.0, 1.0),
            image_rect.top() + image_rect.height() * coords.p1.y.clamp(0.0, 1.0),
        );
        let p2 = egui::pos2(
            image_rect.left() + image_rect.width() * coords.p2.x.clamp(0.0, 1.0),
            image_rect.top() + image_rect.height() * coords.p2.y.clamp(0.0, 1.0),
        );
        Rect::from_two_pos(p1, p2)
    }

    fn new_scene_overlay_child(
        ui: &mut egui::Ui,
        rect: Rect,
        scene_clip_rect: Rect,
        layout: egui::Layout,
    ) -> egui::Ui {
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
        child.set_clip_rect(scene_clip_rect.intersect(rect));
        child
    }

    fn draw_overlay_on_page(
        &mut self,
        ui: &mut egui::Ui,
        page_idx: usize,
        image_rect: Rect,
        tile_budget: &mut usize,
        bytes_budget: &mut usize,
    ) {
        self.overlay_runtime.draw_overlay_on_page(
            ui,
            page_idx,
            image_rect,
            tile_budget,
            bytes_budget,
            self.pixel_sampling_nearest,
        );
    }
}

fn bubble_spellcheck_disabled_key(field: BubbleTextField) -> &'static str {
    match field {
        BubbleTextField::Original => BUBBLE_ORIGINAL_SPELLCHECK_DISABLED_KEY,
        BubbleTextField::Translation => BUBBLE_TRANSLATION_SPELLCHECK_DISABLED_KEY,
    }
}

fn resolve_canvas_ui_fonts_dir(project: &ProjectData) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    candidates.push(project.paths.title_dir.join("fonts").join("ui"));
    candidates.push(project.project_dir.join("fonts").join("ui"));

    if let Ok(cwd) = env::current_dir() {
        candidates.push(cwd.join("fonts").join("ui"));
    }
    if let Ok(exe_path) = env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        candidates.push(exe_dir.join("fonts").join("ui"));
    }

    let mut seen = HashSet::<PathBuf>::new();
    candidates.into_iter().find(|path| {
        if !seen.insert(path.clone()) {
            return false;
        }
        path.is_dir()
    })
}

fn collect_canvas_ui_font_paths(fonts_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(fonts_dir) else {
        return Vec::new();
    };

    let mut font_paths = entries
        .filter_map(|entry| entry.ok().map(|item| item.path()))
        .filter(|path| path.is_file() && is_supported_canvas_ui_font(path))
        .collect::<Vec<_>>();
    font_paths.sort_by_cached_key(|path| canvas_ui_font_sort_key(path.as_path()));
    font_paths
}

fn is_supported_canvas_ui_font(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase()),
        Some(ext) if matches!(ext.as_str(), "otf" | "ttf" | "ttc" | "otc")
    )
}

fn canvas_ui_font_sort_key(path: &Path) -> (u8, u32, String) {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let lower_name = file_name.to_lowercase();

    if let Some((priority, rest_name)) = parse_canvas_ui_font_priority(file_name) {
        return (0, priority, rest_name.to_lowercase());
    }

    (1, u32::MAX, lower_name)
}

fn parse_canvas_ui_font_priority(file_name: &str) -> Option<(u32, &str)> {
    let (priority_raw, rest_name) = file_name.split_once(':')?;
    if priority_raw.is_empty() || rest_name.is_empty() {
        return None;
    }
    let priority = priority_raw.parse::<u32>().ok()?;
    Some((priority, rest_name))
}

fn read_system_clipboard_text() -> Option<String> {
    let mut clipboard = Clipboard::new().ok()?;
    let raw = clipboard.get_text().ok()?;
    Some(sanitize_clipboard_text(&raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_viewport_snapshot_sets_zoom_and_pending_scroll() {
        let mut canvas = CanvasView::default();
        let snapshot = CanvasViewportSnapshot {
            zoom: 2.25,
            scroll_offset: egui::vec2(120.0, 340.0),
        };

        canvas.apply_viewport_snapshot(snapshot);

        assert!((canvas.zoom() - 2.25).abs() <= f32::EPSILON);
        assert_eq!(canvas.scene.scroll_offset, snapshot.scroll_offset);
        assert_eq!(
            canvas.scene.pending_scroll_offset,
            Some(snapshot.scroll_offset)
        );
        assert!(canvas.scene.pending_zoom_anchor.is_none());
    }

    #[test]
    fn horizontal_scroll_range_starts_before_content_reaches_viewport_width() {
        let viewport_width = 1000.0;

        assert_eq!(
            CanvasView::canvas_row_screen_width_for_content(viewport_width, 500.0),
            viewport_width
        );
        assert!(
            CanvasView::canvas_row_screen_width_for_content(viewport_width, 700.0) > viewport_width
        );
    }

    #[test]
    fn zoom_anchor_x_offset_is_clamped_to_scrollable_range() {
        let mut canvas = CanvasView::default();
        canvas.scene.content_world_width = 400.0;
        canvas.state.zoom = 1.0;
        let anchor = crate::canvas::types::PendingZoomAnchor {
            viewport_local: egui::vec2(900.0, 0.0),
            world_focus: egui::vec2(400.0, 0.0),
        };

        let offset = canvas.scroll_offset_for_zoom_anchor(anchor);

        assert!(
            offset.x <= canvas.max_scroll_offset_x_for_viewport(1000.0),
            "zoom-anchor x offset must not exceed current horizontal scroll range"
        );
    }

    #[test]
    fn initial_horizontal_scroll_is_centered_once_when_range_exists() {
        let mut canvas = CanvasView::default();
        canvas.scene.content_world_width = 800.0;
        canvas.state.zoom = 1.0;
        let viewport_width = 1000.0;
        let max_scroll_x = canvas.max_scroll_offset_x_for_viewport(viewport_width);

        let first_offset = canvas.initial_horizontal_center_scroll_offset(viewport_width);
        let second_offset = canvas.initial_horizontal_center_scroll_offset(viewport_width);

        assert!(max_scroll_x > 0.0);
        assert_eq!(first_offset, Some(egui::vec2(max_scroll_x * 0.5, 0.0)));
        assert!(second_offset.is_none());
    }

    #[test]
    fn aside_widths_stretch_between_minimum_and_maximum() {
        let mut canvas = CanvasView::default();
        canvas.state.scale_bubbles = true;
        canvas.state.side_margin = 20.0;
        canvas.state.bubble_min_width = 200.0;
        canvas.state.bubble_max_width = 500.0;
        let image_rect =
            egui::Rect::from_min_max(egui::pos2(400.0, 0.0), egui::pos2(900.0, 1000.0));
        let viewport_rect =
            egui::Rect::from_min_max(egui::pos2(250.0, 0.0), egui::pos2(1000.0, 800.0));

        let [left_width, right_width] =
            canvas.aside_available_widths_for_page_viewport(image_rect, viewport_rect);

        assert_eq!(left_width, 200.0);
        assert_eq!(right_width, 200.0);
    }

    #[test]
    fn aside_widths_ignore_viewport_distance_when_stretching_is_disabled() {
        let mut canvas = CanvasView::default();
        canvas.state.scale_bubbles = false;
        canvas.state.side_margin = 20.0;
        canvas.state.bubble_min_width = 200.0;
        canvas.state.bubble_max_width = 500.0;
        let image_rect =
            egui::Rect::from_min_max(egui::pos2(400.0, 0.0), egui::pos2(900.0, 1000.0));
        let viewport_rect =
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1500.0, 800.0));

        let [left_width, right_width] =
            canvas.aside_available_widths_for_page_viewport(image_rect, viewport_rect);

        assert_eq!(left_width, 200.0);
        assert_eq!(right_width, 200.0);
    }
}
