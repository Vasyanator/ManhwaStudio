/*
File: crates/ms-tab-translation/src/tab/detection_mask.rs

Purpose:
The text detector's editable state on the canvas: status line and edit modes, the sync between
the tab's detector results and the shared `TextMaskModel`, brush painting of the mask, the mask
and line overlays, and the detector geometry helpers (rect conversion, block rasterization,
expand / merge) and tiled mask textures.

Key functions:
- TranslationTabState::set_text_detector_status / set_text_detector_edit_lines_mode /
  set_text_detector_edit_mask_mode / clear_text_detector_line_edit_state.
- sync_text_mask_page_from_result / sync_text_mask_revision_cache / text_mask_page_snapshot /
  materialize_text_mask_page_from_blocks_if_missing / paint_text_mask_segment: shared-mask sync
  and editing.
- draw_text_detector_mask_layer_on_page / draw_text_detector_additional_overlay_on_page: canvas
  layers (drawn while `detector_overlay_visible()`).
- create_text_detector_line_at_uv / add_text_detector_line_at_uv / ensure_text_detector_page_entry
  / note_page_sizes.
- detector_rects_to_blocks_px / rasterize_resolved_blocks / detector_blocks_with_options /
  detector_expand_blocks / detector_merge_blocks / source_rect_to_scene_rect / scene_pos_to_source:
  pure geometry (also used by `tab/canvas_input.rs`, `tab/detector_region.rs`,
  `tab/block_bubbles.rs`).
- draw_text_detector_mask_overlay_on_page / build_text_detector_mask_texture_page /
  update_text_detector_mask_texture_tiles / text_detector_mask_texture_page_estimated_bytes:
  tiled GPU mask textures (params type `TextDetectorMaskOverlayDrawParams` lives in `tab.rs`).
*/

use ms_models::text_mask_model::TextMaskPage;
use ms_models::page_view::PageImageInfo;
use crate::panels::text_detector::{TextDetectorPanelOptions, detector_page_size};
use crate::text_detector::{TextDetectorPageResult, TextDetectorRect};
use eframe::egui;
use egui::{Color32, Pos2, Rect, Stroke};
use ms_theme::Severity;
use std::collections::HashMap;
use super::{TEXT_DETECTOR_MASK_TEXTURE_OPTIONS, TEXT_DETECTOR_MASK_TILE_SIDE, TEXT_DETECTOR_MASK_VISUAL_ALPHA_MAX, TextDetectorLineSelection, TextDetectorMaskOverlayDrawParams, TextDetectorMaskTexturePage, TextDetectorMaskTextureTile, TextMaskPageGeom, TranslationTabState};

impl TranslationTabState {
    /// Replaces the detector panel status line; `severity` selects its themed colour
    /// (`Info` for in-progress work, `Warning` for partial results/retries).
    pub(super) fn set_text_detector_status(&mut self, text: impl Into<String>, severity: Severity) {
        self.text_detector_status = text.into();
        self.text_detector_status_severity = severity;
    }

    pub(super) fn clear_text_detector_line_edit_state(&mut self) {
        self.text_detector_line_selection = None;
        self.text_detector_line_drag_state = None;
    }

    pub(super) fn set_text_detector_edit_lines_mode(&mut self, enabled: bool) {
        self.text_detector_edit_lines_mode = enabled;
        if enabled {
            self.set_text_detector_edit_mask_mode(false);
            return;
        }
        self.clear_text_detector_line_edit_state();
    }

    pub(super) fn set_text_detector_edit_mask_mode(&mut self, enabled: bool) {
        self.text_detector_edit_mask_mode = enabled;
        if enabled {
            self.text_detector_edit_lines_mode = false;
            self.clear_text_detector_line_edit_state();
            return;
        }
        self.text_detector_mask_stroke_state = None;
    }

    pub(super) fn clear_text_mask_model(&mut self) {
        if let Some(model) = self.text_mask_model.as_ref()
            && let Ok(mut model) = model.lock()
        {
            model.clear_all();
        }
        self.text_mask_synced_revision = 0;
    }

    pub(super) fn sync_text_mask_page_from_result(&mut self, page_idx: usize) {
        let Some(result) = self.text_detector_results.get(&page_idx) else {
            if let Some(model) = self.text_mask_model.as_ref()
                && let Ok(mut model) = model.lock()
            {
                model.remove_page(page_idx);
            }
            return;
        };
        // The stored mask is the RAW backend glyph mask (`result.mask_alpha`), not a
        // raster of the expanded/merged blocks, so mirror the RAW detector boxes
        // (`result.blocks`) alongside it. Autoclean's box candidate must match what the
        // mask represents; resolved boxes belong only to the synthesized-rect path in
        // `materialize_text_mask_page_from_blocks_if_missing`.
        let blocks = detector_rects_to_blocks_px(&result.blocks);
        if let Some(model) = self.text_mask_model.as_ref()
            && let Ok(mut model) = model.lock()
        {
            model.set_page_with_blocks(
                page_idx,
                result.source_size,
                result.mask_size,
                result.mask_alpha.clone(),
                blocks,
            );
        }
    }

    fn sync_text_mask_revision_cache(&mut self) {
        let Some(model) = self.text_mask_model.as_ref() else {
            return;
        };
        let revision = model.lock().map(|m| m.revision()).unwrap_or(0);
        if revision != self.text_mask_synced_revision {
            self.text_mask_synced_revision = revision;
            self.text_detector_mask_textures.clear();
        }
    }

    pub(super) fn text_mask_page_snapshot(&self, page_idx: usize) -> Option<TextMaskPage> {
        let model = self.text_mask_model.as_ref()?;
        let guard = model.lock().ok()?;
        guard.page(page_idx).cloned()
    }

    pub(super) fn materialize_text_mask_page_from_blocks_if_missing(&mut self, page_idx: usize) {
        if self
            .text_mask_page_snapshot(page_idx)
            .is_some_and(|page| !page.mask_alpha.is_empty())
        {
            return;
        }
        let Some(result) = self.text_detector_results.get(&page_idx) else {
            return;
        };
        if !result.mask_alpha.is_empty() || result.blocks.is_empty() {
            return;
        }
        let Some((mask_alpha, resolved)) = rasterize_resolved_blocks(result, &self.text_detector_options) else {
            return;
        };
        // The synthesized mask IS a raster of the resolved (expanded/merged) blocks, so
        // store those same resolved blocks as the page's box set — they match the mask.
        let mask_size = result.source_size;
        let blocks = detector_rects_to_blocks_px(&resolved);
        let source_size = result.source_size;
        if let Some(model) = self.text_mask_model.as_ref()
            && let Ok(mut model) = model.lock()
        {
            model.set_page_with_blocks(page_idx, source_size, mask_size, mask_alpha, blocks);
        }
    }

    pub(super) fn text_mask_target_sizes(
        &self,
        page_idx: usize,
        page_rect: Rect,
        zoom: f32,
    ) -> Option<([u32; 2], [u32; 2])> {
        if let Some(mask) = self.text_mask_page_snapshot(page_idx) {
            return Some((mask.source_size, mask.mask_size));
        }
        if let Some(result) = self.text_detector_results.get(&page_idx) {
            let source_size = result.source_size;
            let mask_size = if result.mask_alpha.is_empty() {
                source_size
            } else {
                result.mask_size
            };
            return Some((source_size, mask_size));
        }
        let safe_zoom = zoom.max(f32::EPSILON);
        let source_w = (page_rect.width() / safe_zoom).round().max(1.0) as u32;
        let source_h = (page_rect.height() / safe_zoom).round().max(1.0) as u32;
        if source_w == 0 || source_h == 0 {
            return None;
        }
        Some(([source_w, source_h], [source_w, source_h]))
    }

    /// Paints (or, with `erase`, clears) one brush segment from `from_scene` to `to_scene` (scene
    /// positions over `geom.page_rect`) into the page's text mask in the shared
    /// `TextMaskModel`, then refreshes the dirty tiles of the page's mask texture. Returns `true`
    /// when the mask changed (no model, an off-page point or a no-op stroke return `false`).
    pub(super) fn paint_text_mask_segment(&mut self, geom: TextMaskPageGeom, from_scene: Pos2, to_scene: Pos2, erase: bool) -> bool {
        let TextMaskPageGeom { page_idx, source_size, mask_size, page_rect } = geom;
        let Some(src0) = scene_pos_to_source(
            page_rect,
            mask_size[0] as f32,
            mask_size[1] as f32,
            from_scene,
        ) else {
            return false;
        };
        let Some(src1) = scene_pos_to_source(
            page_rect,
            mask_size[0] as f32,
            mask_size[1] as f32,
            to_scene,
        ) else {
            return false;
        };
        let Some(model) = self.text_mask_model.as_ref().cloned() else {
            return false;
        };
        let brush = self.text_detector_mask_brush.clone();
        let brush_radius = brush.radius_px().max(1) as f32;
        let target_value = if erase { 0u8 } else { 255u8 };
        let mut dirty_rect: Option<(usize, usize, usize, usize)> = None;
        let mask_w = mask_size[0] as usize;
        let mask_h = mask_size[1] as usize;
        let mut changed = false;
        if let Ok(mut model) = model.lock() {
            changed = model.edit_page_mask(
                page_idx,
                source_size,
                mask_size,
                |mask_alpha, mask_w, mask_h| {
                    if mask_w == 0 || mask_h == 0 {
                        return false;
                    }
                    let min_x = (src0.x.min(src1.x) - brush_radius).floor().max(0.0) as usize;
                    let min_y = (src0.y.min(src1.y) - brush_radius).floor().max(0.0) as usize;
                    let max_x = (src0.x.max(src1.x) + brush_radius)
                        .ceil()
                        .min(mask_w.saturating_sub(1) as f32)
                        as usize;
                    let max_y = (src0.y.max(src1.y) + brush_radius)
                        .ceil()
                        .min(mask_h.saturating_sub(1) as f32)
                        as usize;
                    let mut has_delta = false;
                    for y in min_y..=max_y {
                        let row = y.saturating_mul(mask_w);
                        for x in min_x..=max_x {
                            if mask_alpha[row + x] != target_value {
                                has_delta = true;
                                break;
                            }
                        }
                        if has_delta {
                            break;
                        }
                    }
                    if !has_delta {
                        return false;
                    }
                    dirty_rect = Some((min_x, min_y, max_x, max_y));
                    let segment = ms_tools::LineSegmentPx {
                        x0: src0.x.round() as i32,
                        y0: src0.y.round() as i32,
                        x1: src1.x.round() as i32,
                        y1: src1.y.round() as i32,
                    };
                    brush.paint_binary_mask_segment(
                        ms_tools::BinaryMaskMut { data: mask_alpha, width: mask_w, height: mask_h },
                        segment,
                        erase,
                    );
                    true
                },
            );
            if changed {
                self.text_mask_synced_revision = model.revision();
                if let (Some(page), Some(dirty)) = (model.page(page_idx), dirty_rect)
                    && let Some(page_tex) = self.text_detector_mask_textures.get_mut(&page_idx)
                {
                    if page_tex.size == [mask_w, mask_h] {
                        update_text_detector_mask_texture_tiles(
                            page_tex,
                            page.mask_size.map(|v| v as usize),
                            &page.mask_alpha,
                            dirty,
                        );
                    } else {
                        self.text_detector_mask_textures.remove(&page_idx);
                    }
                }
            }
        }
        changed
    }
}

impl TranslationTabState {
    pub(super) fn draw_text_detector_mask_layer_on_page(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        page_idx: usize,
        page_rect: Rect,
        zoom: f32,
    ) {
        if !self.detector_overlay_visible() {
            return;
        }
        self.sync_text_mask_revision_cache();
        let clip_rect = ui.clip_rect().intersect(page_rect);
        if !clip_rect.is_positive() {
            return;
        }
        let painter = ui.painter().with_clip_rect(clip_rect);

        if self.text_detector_edit_lines_mode {
            return;
        }
        if self.text_detector_edit_mask_mode {
            self.draw_text_detector_mask_edit_overlay_on_page(ui, page_idx, page_rect, zoom);
        }

        let mask_color = Color32::from_rgba_unmultiplied(255, 0, 0, 60);
        let options = self.text_detector_options.clone();
        let result = self.text_detector_results.get(&page_idx);
        let mask_page = self.text_mask_page_snapshot(page_idx);
        let has_mask_alpha = mask_page
            .as_ref()
            .is_some_and(|mask| !mask.mask_alpha.is_empty());
        if result.is_none() && !has_mask_alpha {
            return;
        }

        let show_mask = options.draw_mask || self.text_detector_edit_mask_mode;
        let needs_merged_blocks = show_mask && !has_mask_alpha;
        let merged = match (needs_merged_blocks, result) {
            (true, Some(result)) => {
                let source_w = result.source_size[0].max(1) as f32;
                let source_h = result.source_size[1].max(1) as f32;
                let expanded = detector_expand_blocks(
                    &result.blocks,
                    options.block_expand_px,
                    source_w,
                    source_h,
                );
                detector_merge_blocks(&expanded, options.merge_gap_px)
            }
            _ => Vec::new(),
        };

        if show_mask {
            if let Some(mask) = mask_page
                .as_ref()
                .filter(|mask| !mask.mask_alpha.is_empty())
            {
                let mask_textures = &mut self.text_detector_mask_textures;
                draw_text_detector_mask_overlay_on_page(TextDetectorMaskOverlayDrawParams {
                    textures: mask_textures,
                    ctx,
                    painter: &painter,
                    page_idx,
                    page_rect,
                    mask_size: mask.mask_size,
                    mask_alpha: &mask.mask_alpha,
                    current_frame: ctx.cumulative_frame_nr(),
                });
            } else if let Some(result) = result {
                let source_w = result.source_size[0].max(1) as f32;
                let source_h = result.source_size[1].max(1) as f32;
                for rect in &merged {
                    if let Some(scene_rect) =
                        source_rect_to_scene_rect(page_rect, source_w, source_h, *rect)
                    {
                        painter.rect_filled(scene_rect, 0.0, mask_color);
                    }
                }
            }
        }
    }

    pub(super) fn draw_text_detector_additional_overlay_on_page(
        &mut self,
        ui: &mut egui::Ui,
        page_idx: usize,
        page_rect: Rect,
    ) {
        if !self.detector_overlay_visible() {
            return;
        }
        let clip_rect = ui.clip_rect().intersect(page_rect);
        if !clip_rect.is_positive() {
            return;
        }
        let painter = ui.painter().with_clip_rect(clip_rect);

        if self.text_detector_edit_lines_mode {
            self.draw_text_detector_line_edit_overlay_on_page(ui, &painter, page_idx, page_rect);
            return;
        }
        if self.text_detector_edit_mask_mode {
            return;
        }

        let raw_stroke = Stroke::new(1.2, Color32::from_rgb(0, 255, 0));
        let merged_stroke = Stroke::new(2.0, Color32::from_rgb(0, 160, 255));
        let options = self.text_detector_options.clone();
        let Some(result) = self.text_detector_results.get(&page_idx) else {
            return;
        };
        if !options.draw_lines {
            return;
        }
        let source_w = result.source_size[0].max(1) as f32;
        let source_h = result.source_size[1].max(1) as f32;
        let expanded =
            detector_expand_blocks(&result.blocks, options.block_expand_px, source_w, source_h);
        let merged = detector_merge_blocks(&expanded, options.merge_gap_px);
        for rect in &expanded {
            if let Some(scene_rect) =
                source_rect_to_scene_rect(page_rect, source_w, source_h, *rect)
            {
                painter.rect_stroke(scene_rect, 0.0, raw_stroke, egui::StrokeKind::Outside);
            }
        }
        for rect in &merged {
            if let Some(scene_rect) =
                source_rect_to_scene_rect(page_rect, source_w, source_h, *rect)
            {
                painter.rect_stroke(scene_rect, 0.0, merged_stroke, egui::StrokeKind::Outside);
            }
        }
    }

    /// Adds a line centred at `uv` (page-normalized) on page `page_idx`, creating the page's
    /// detector entry first when it has none, and selects it.
    pub(super) fn create_text_detector_line_at_uv(&mut self, page_idx: usize, uv: Pos2) -> Result<(), String> {
        self.ensure_text_detector_page_entry(page_idx)?;
        self.add_text_detector_line_at_uv(page_idx, uv)
    }

    /// Adds a line centred at `uv` (page-normalized) to the EXISTING detector entry of page
    /// `page_idx` and selects it. Errors when the page has no entry or the rect is degenerate.
    pub(super) fn add_text_detector_line_at_uv(&mut self, page_idx: usize, uv: Pos2) -> Result<(), String> {
        let next_idx = {
            let result = self
                .text_detector_results
                .get_mut(&page_idx)
                .ok_or_else(|| tf!("translation.tab.prepare_detection_error", page_idx = page_idx))?;
            let source_w = result.source_size[0].max(1) as f32;
            let source_h = result.source_size[1].max(1) as f32;
            let center_x = uv.x.clamp(0.0, 1.0) * source_w;
            let center_y = uv.y.clamp(0.0, 1.0) * source_h;
            let half_w = (source_w * 0.08).clamp(20.0, 220.0);
            let half_h = (source_h * 0.03).clamp(12.0, 120.0);
            let rect = TextDetectorRect::from_xyxy(
                (center_x - half_w).clamp(0.0, source_w),
                (center_y - half_h).clamp(0.0, source_h),
                (center_x + half_w).clamp(0.0, source_w),
                (center_y + half_h).clamp(0.0, source_h),
            )
            .ok_or_else(|| t!("translation.tab.create_line_invalid_coords_error").to_string())?;
            result.blocks.push(rect);
            result.blocks.len().saturating_sub(1)
        };
        self.line_edited_pages.insert(page_idx);
        self.text_detector_line_selection = Some(TextDetectorLineSelection {
            page_idx,
            line_idx: next_idx,
        });
        self.text_detector_line_drag_state = None;
        Ok(())
    }

    /// Refreshes `known_page_sizes` from this frame's page map (host, before the canvas draw):
    /// a page whose size is not known (loading, failed, absent) has no entry. O(pages), no I/O.
    pub(crate) fn note_page_sizes(&mut self, page_infos: &HashMap<usize, PageImageInfo>) {
        self.known_page_sizes.retain(|page_idx, _| page_infos.contains_key(page_idx));
        for (&page_idx, info) in page_infos {
            match detector_page_size(Some(info)) {
                Some(size) => {
                    self.known_page_sizes.insert(page_idx, size);
                }
                None => {
                    self.known_page_sizes.remove(&page_idx);
                }
            }
        }
    }

    /// The detector entry of page `page_idx`, created empty when it has none. The page size comes
    /// from the page loader's `PageImageInfo` (`known_page_sizes`), never from disk: this runs on
    /// the GUI thread. Errors while that size is unknown (the page is loading or unreadable).
    fn ensure_text_detector_page_entry(&mut self, page_idx: usize) -> Result<&mut TextDetectorPageResult, String> {
        if let std::collections::hash_map::Entry::Vacant(e) =
            self.text_detector_results.entry(page_idx)
        {
            let [w, h] = self
                .known_page_sizes
                .get(&page_idx)
                .copied()
                .ok_or_else(|| tf!("translation.tab.page_size_unknown_error", page_idx = page_idx))?;
            e.insert(TextDetectorPageResult {
                source_size: [w, h],
                blocks: Vec::new(),
                mask_size: [w, h],
                mask_alpha: Vec::new(),
            });
        }
        self.text_detector_results
            .get_mut(&page_idx)
            .ok_or_else(|| tf!("translation.tab.prepare_detection_error", page_idx = page_idx))
    }
}

/// Rounds one float detector rect outward into an integer `[x1, y1, x2, y2]` covering
/// rect in source-page pixel space, or `None` when it cannot be represented.
///
/// `x1`/`y1` are floored, `x2`/`y2` ceiled, so the integer rect fully covers the float
/// rect. Any non-finite coordinate, or one whose rounded form falls outside the `i32`
/// range, makes the whole rect unrepresentable: it is skipped (with a warning log)
/// rather than wrapped into a bogus box. In-range negative coordinates are preserved so
/// callers can clamp them to their own raster as needed.
fn detector_rect_to_px_rect(rect: &TextDetectorRect) -> Option<[i32; 4]> {
    let round = |value: f32, ceil: bool| -> Option<i32> {
        if !value.is_finite() {
            return None;
        }
        let rounded = if ceil { value.ceil() } else { value.floor() };
        // Bounds-check before converting so a stray coordinate is skipped, never
        // truncated or wrapped into a valid-looking box.
        if rounded < i32::MIN as f32 || rounded > i32::MAX as f32 {
            return None;
        }
        Some(rounded as i32)
    };
    match (
        round(rect.x1, false),
        round(rect.y1, false),
        round(rect.x2, true),
        round(rect.y2, true),
    ) {
        (Some(x1), Some(y1), Some(x2), Some(y2)) => Some([x1, y1, x2, y2]),
        _ => {
            ms_log::runtime_log::log_warn(format!(
                "autoclean: skipping detector rect with non-finite or out-of-i32-range coordinates: {rect:?}"
            ));
            None
        }
    }
}

/// Converts float detector rects into integer `[x1, y1, x2, y2]` covering rects in
/// source-page pixel space for `TextMaskModel`.
///
/// Each edge is rounded outward via [`detector_rect_to_px_rect`], which skips any rect
/// with a non-finite or out-of-range coordinate. Returns `None` for an empty slice, and
/// also when every rect was skipped, so the model's "`None` == no detector boxes known"
/// contract holds (an empty box vector is never stored).
pub(super) fn detector_rects_to_blocks_px(rects: &[TextDetectorRect]) -> Option<Vec<[i32; 4]>> {
    if rects.is_empty() {
        return None;
    }
    let blocks: Vec<[i32; 4]> = rects.iter().filter_map(detector_rect_to_px_rect).collect();
    if blocks.is_empty() {
        None
    } else {
        Some(blocks)
    }
}

/// Rasterizes a mask-less result's blocks as the user sees them: the blocks resolved with the
/// detector panel's options (expanded / merged) filled into a source-size 0/255 mask. Returns
/// the mask and those resolved blocks, or `None` when the page has no size or no block covers a
/// pixel. The one owner of the legacy "blocks without a stored mask" display mask.
pub(super) fn rasterize_resolved_blocks(result: &TextDetectorPageResult, options: &TextDetectorPanelOptions) -> Option<(Vec<u8>, Vec<TextDetectorRect>)> {
    let mask_w = usize::try_from(result.source_size[0]).ok().unwrap_or(0);
    let mask_h = usize::try_from(result.source_size[1]).ok().unwrap_or(0);
    if mask_w == 0 || mask_h == 0 {
        return None;
    }
    let resolved = detector_blocks_with_options(result, options);
    let mut mask_alpha = vec![0u8; mask_w.saturating_mul(mask_h)];
    for rect in &resolved {
        // Same checked float->int rounding as the block set, then clamp the covering
        // rect into the mask raster (negative edges -> 0, far edges -> mask extent).
        let Some([rx1, ry1, rx2, ry2]) = detector_rect_to_px_rect(rect) else {
            continue;
        };
        let x0 = usize::try_from(rx1.max(0)).unwrap_or(0).min(mask_w);
        let y0 = usize::try_from(ry1.max(0)).unwrap_or(0).min(mask_h);
        let x1 = usize::try_from(rx2.max(0)).unwrap_or(0).min(mask_w);
        let y1 = usize::try_from(ry2.max(0)).unwrap_or(0).min(mask_h);
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        for y in y0..y1 {
            let row = y.saturating_mul(mask_w);
            for x in x0..x1 {
                mask_alpha[row + x] = 255;
            }
        }
    }
    if !mask_alpha.iter().any(|&px| px != 0) {
        return None;
    }
    Some((mask_alpha, resolved))
}

pub(super) fn detector_blocks_with_options(
    result: &TextDetectorPageResult,
    options: &TextDetectorPanelOptions,
) -> Vec<TextDetectorRect> {
    let source_w = result.source_size[0].max(1) as f32;
    let source_h = result.source_size[1].max(1) as f32;
    let expanded =
        detector_expand_blocks(&result.blocks, options.block_expand_px, source_w, source_h);
    detector_merge_blocks(&expanded, options.merge_gap_px)
}

pub(super) fn detector_expand_blocks(
    blocks: &[TextDetectorRect],
    expand_px: i32,
    source_w: f32,
    source_h: f32,
) -> Vec<TextDetectorRect> {
    let exp = expand_px.max(0) as f32;
    let mut out = Vec::with_capacity(blocks.len());
    for rect in blocks {
        let x1 = (rect.x1 - exp).clamp(0.0, source_w);
        let y1 = (rect.y1 - exp).clamp(0.0, source_h);
        let x2 = (rect.x2 + exp).clamp(0.0, source_w);
        let y2 = (rect.y2 + exp).clamp(0.0, source_h);
        if let Some(next) = TextDetectorRect::from_xyxy(x1, y1, x2, y2) {
            out.push(next);
        }
    }
    out
}

fn detector_merge_blocks(blocks: &[TextDetectorRect], merge_gap_px: i32) -> Vec<TextDetectorRect> {
    let gap = merge_gap_px.max(0) as f32;
    let mut merged = Vec::<TextDetectorRect>::new();
    for rect in blocks {
        let mut cur = *rect;
        let mut i = 0usize;
        while i < merged.len() {
            if detector_rects_touch_or_near(cur, merged[i], gap) {
                let next = TextDetectorRect::from_xyxy(
                    cur.x1.min(merged[i].x1),
                    cur.y1.min(merged[i].y1),
                    cur.x2.max(merged[i].x2),
                    cur.y2.max(merged[i].y2),
                );
                merged.swap_remove(i);
                if let Some(value) = next {
                    cur = value;
                    continue;
                }
            }
            i += 1;
        }
        merged.push(cur);
    }

    ms_text_detect::blocks::sort_reading_order(&mut merged);
    merged
}

fn detector_rects_touch_or_near(a: TextDetectorRect, b: TextDetectorRect, gap: f32) -> bool {
    !(a.x2 + gap < b.x1 || a.x1 - gap > b.x2 || a.y2 + gap < b.y1 || a.y1 - gap > b.y2)
}

pub(super) fn source_rect_to_scene_rect(
    page_rect: Rect,
    source_w: f32,
    source_h: f32,
    src: TextDetectorRect,
) -> Option<Rect> {
    if source_w <= 0.0 || source_h <= 0.0 {
        return None;
    }
    let x1 = page_rect.left() + page_rect.width() * (src.x1 / source_w).clamp(0.0, 1.0);
    let y1 = page_rect.top() + page_rect.height() * (src.y1 / source_h).clamp(0.0, 1.0);
    let x2 = page_rect.left() + page_rect.width() * (src.x2 / source_w).clamp(0.0, 1.0);
    let y2 = page_rect.top() + page_rect.height() * (src.y2 / source_h).clamp(0.0, 1.0);
    let rect = Rect::from_min_max(egui::pos2(x1, y1), egui::pos2(x2, y2));
    if rect.is_positive() { Some(rect) } else { None }
}

pub(super) fn scene_pos_to_source(
    page_rect: Rect,
    source_w: f32,
    source_h: f32,
    scene_pos: Pos2,
) -> Option<Pos2> {
    if source_w <= 0.0 || source_h <= 0.0 || !page_rect.is_positive() {
        return None;
    }
    let u = ((scene_pos.x - page_rect.left()) / page_rect.width()).clamp(0.0, 1.0);
    let v = ((scene_pos.y - page_rect.top()) / page_rect.height()).clamp(0.0, 1.0);
    Some(egui::pos2(u * source_w, v * source_h))
}

fn draw_text_detector_mask_overlay_on_page(params: TextDetectorMaskOverlayDrawParams<'_>) {
    let TextDetectorMaskOverlayDrawParams {
        textures,
        ctx,
        painter,
        page_idx,
        page_rect,
        mask_size,
        mask_alpha,
        current_frame,
    } = params;
    if mask_alpha.is_empty() {
        return;
    }
    let mask_w = mask_size[0] as usize;
    let mask_h = mask_size[1] as usize;
    if mask_w == 0 || mask_h == 0 {
        return;
    }
    let expected_len = mask_w.saturating_mul(mask_h);
    if expected_len == 0 || expected_len != mask_alpha.len() {
        return;
    }

    let needs_rebuild = textures
        .get(&page_idx)
        .map(|page| page.size != [mask_w, mask_h])
        .unwrap_or(true);
    if needs_rebuild {
        let page_tex =
            build_text_detector_mask_texture_page(ctx, page_idx, [mask_w, mask_h], mask_alpha);
        textures.insert(page_idx, page_tex);
    }

    let Some(page_tex) = textures.get_mut(&page_idx) else {
        return;
    };
    page_tex.last_used_frame = current_frame;
    let src_w = page_tex.size[0] as f32;
    let src_h = page_tex.size[1] as f32;
    if src_w <= 0.0 || src_h <= 0.0 {
        return;
    }
    for tile in &page_tex.tiles {
        let ox = tile.origin_px[0] as f32;
        let oy = tile.origin_px[1] as f32;
        let tw = tile.size_px[0] as f32;
        let th = tile.size_px[1] as f32;
        if tw <= 0.0 || th <= 0.0 {
            continue;
        }
        let dst = Rect::from_min_size(
            egui::pos2(
                page_rect.left() + page_rect.width() * (ox / src_w),
                page_rect.top() + page_rect.height() * (oy / src_h),
            ),
            egui::vec2(
                page_rect.width() * (tw / src_w),
                page_rect.height() * (th / src_h),
            ),
        );
        painter.image(
            tile.texture.id(),
            dst,
            Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
}

fn build_text_detector_mask_texture_page(
    ctx: &egui::Context,
    page_idx: usize,
    size: [usize; 2],
    alpha: &[u8],
) -> TextDetectorMaskTexturePage {
    let w = size[0];
    let h = size[1];
    if w == 0 || h == 0 {
        return TextDetectorMaskTexturePage {
            size,
            tiles: Vec::new(),
            last_used_frame: 0,
        };
    }
    let mut tiles = Vec::new();
    let mut y = 0usize;
    while y < h {
        let mut x = 0usize;
        while x < w {
            let tw = (w - x).min(TEXT_DETECTOR_MASK_TILE_SIDE);
            let th = (h - y).min(TEXT_DETECTOR_MASK_TILE_SIDE);
            let tile_img = build_text_detector_mask_tile_image(size, alpha, x, y, tw, th);
            let texture = ctx.load_texture(
                format!("text-detector-mask-{page_idx}-{x}-{y}"),
                tile_img,
                TEXT_DETECTOR_MASK_TEXTURE_OPTIONS,
            );
            tiles.push(TextDetectorMaskTextureTile {
                texture,
                origin_px: [x, y],
                size_px: [tw, th],
            });
            x += TEXT_DETECTOR_MASK_TILE_SIDE;
        }
        y += TEXT_DETECTOR_MASK_TILE_SIDE;
    }
    TextDetectorMaskTexturePage {
        size,
        tiles,
        last_used_frame: 0,
    }
}

pub(super) fn text_detector_mask_texture_page_estimated_bytes(page_tex: &TextDetectorMaskTexturePage) -> u64 {
    let bytes = page_tex
        .tiles
        .iter()
        .map(|tile| {
            tile.size_px[0]
                .saturating_mul(tile.size_px[1])
                .saturating_mul(4)
        })
        .fold(0usize, usize::saturating_add);
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

fn update_text_detector_mask_texture_tiles(
    page_tex: &mut TextDetectorMaskTexturePage,
    size: [usize; 2],
    alpha: &[u8],
    dirty_rect: (usize, usize, usize, usize),
) {
    let (min_x, min_y, max_x, max_y) = dirty_rect;
    if size[0] == 0 || size[1] == 0 || alpha.is_empty() || min_x > max_x || min_y > max_y {
        return;
    }
    let tx0 = min_x / TEXT_DETECTOR_MASK_TILE_SIDE;
    let ty0 = min_y / TEXT_DETECTOR_MASK_TILE_SIDE;
    let tx1 = max_x / TEXT_DETECTOR_MASK_TILE_SIDE;
    let ty1 = max_y / TEXT_DETECTOR_MASK_TILE_SIDE;
    for tile in &mut page_tex.tiles {
        let tile_x = tile.origin_px[0] / TEXT_DETECTOR_MASK_TILE_SIDE;
        let tile_y = tile.origin_px[1] / TEXT_DETECTOR_MASK_TILE_SIDE;
        if tile_x < tx0 || tile_x > tx1 || tile_y < ty0 || tile_y > ty1 {
            continue;
        }
        let tile_img = build_text_detector_mask_tile_image(
            size,
            alpha,
            tile.origin_px[0],
            tile.origin_px[1],
            tile.size_px[0],
            tile.size_px[1],
        );
        tile.texture
            .set(tile_img, TEXT_DETECTOR_MASK_TEXTURE_OPTIONS);
    }
}

fn build_text_detector_mask_tile_image(
    size: [usize; 2],
    alpha: &[u8],
    origin_x: usize,
    origin_y: usize,
    tile_w: usize,
    tile_h: usize,
) -> egui::ColorImage {
    let full_w = size[0];
    let mut raw = vec![0u8; tile_w * tile_h * 4];
    for ty in 0..tile_h {
        let sy = origin_y + ty;
        let row_off = sy * full_w;
        for tx in 0..tile_w {
            let sx = origin_x + tx;
            let src_idx = row_off + sx;
            let dst_idx = (ty * tile_w + tx) * 4;
            let src_alpha = alpha.get(src_idx).copied().unwrap_or(0);
            let a = ((src_alpha as u16 * TEXT_DETECTOR_MASK_VISUAL_ALPHA_MAX as u16) / 255) as u8;
            // ColorImage::from_rgba_premultiplied expects RGB already multiplied by alpha.
            // Keep transparent pixels fully colorless to avoid red tint in black mask regions.
            raw[dst_idx] = a;
            raw[dst_idx + 1] = 0;
            raw[dst_idx + 2] = 0;
            raw[dst_idx + 3] = a;
        }
    }
    egui::ColorImage::from_rgba_premultiplied([tile_w, tile_h], &raw)
}
