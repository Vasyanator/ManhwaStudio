/*
FILE OVERVIEW: crates/ms-tab-translation/src/tab/canvas_input.rs
The Translation tab's pointer drivers on its canvas, all fed by this frame's `ms_input::InputFrame`
(`TranslationTabState::input_frame`, set by the host for the draw): no key, modifier or raw pointer
button is read here.

Main items:
- `CanvasSelectionKind` / `CanvasSelection` / `advance_selection`: the canvas-wide rect
  selections (quick OCR, advanced recognition, image-bubble page crop) driven from the router's
  claim of `OCR_QUICK_RECT` / `OCR_ADVANCED_RECT` / `IMAGE_CROP_RECT` on the canvas view (mouse
  or finger alike); `is_committable` is the 4x4 minimum.
- `TranslationTabState::drive_canvas_selection`: per-frame step, route refusal toast, commit,
  and the selection rect painting.
- `line_edit_hits`: which line / handle a point hits (the targets of the lines edit gestures).
- `TranslationTabState::draw_text_detector_line_edit_overlay_on_page`: the detected-lines editor
  (select / deselect clicks, move / resize drags) through `InputFrame::gesture_for`.
- `TranslationTabState::draw_text_detector_mask_edit_overlay_on_page`: the text-mask brush
  (paint / erase drags) through `InputFrame::gesture_for`.
- `TranslationTabState::delete_selected_text_detector_line` /
  `create_text_detector_line_at_page_center`: the lines edit commands the host executes.
- `TranslationTabState::flush_line_edits_to_text_mask_model`: hands the edited lines to the shared
  `TextMaskModel` (Cleaning's quick clean reads its boxes) once no drag runs on the page;
  `drop_line_edit_state_on_page` drops a selection / drag whose block index went stale.

Notes:
- A claimed selection starts at the PRESS point (the claim's origin) and commits on the button
  release or on the release of its mode (the modes' policy is `Commit`); a cancelled claim drops
  it. A press outside every page starts nothing.
- Targeted gestures resolve with the chord and the hit of their PRESS point
  (`PointerState::press_origin`), so a drag that leaves its line keeps moving it.
*/

use eframe::egui;
use egui::{Color32, Pos2, Rect, Stroke};
use ms_canvas::CanvasView;
use ms_input::{ActionId, ClaimedGesture, GesturePhase, HitPhase, ReleasePolicy};
use ms_project::ProjectData;
use ms_theme::Severity;

use super::{
    TextDetectorLineDragKind, TextDetectorLineDragState, TextDetectorLineSelection, TextDetectorMaskStrokeState, TextMaskPageGeom, TranslationTabState,
};
use super::detection_mask::{scene_pos_to_source, source_rect_to_scene_rect};
use crate::text_detector::TextDetectorRect;
use crate::input::{
    IMAGE_CROP_RECT, LINES_DESELECT, LINES_MOVE, LINES_RESIZE, LINES_SELECT, MASK_ERASE, MASK_PAINT, OCR_ADVANCED_RECT, OCR_QUICK_RECT, OCR_QUICK_RECT_TOUCH,
    TARGET_LINE, TARGET_LINE_HANDLE, TARGET_LINES_PAGE, TARGET_MASK_PAGE, recent_char_rank,
};

/// The smallest selection side (screen points) that is committed; a smaller rect (a click, a
/// twitch) is dropped.
const MIN_SELECTION_SIDE_PT: f32 = 4.0;
/// How close (screen points) the press must be to a handle of the selected line to resize it.
const LINE_HANDLE_HIT_RADIUS_PT: f32 = 8.0;

/// What a canvas-wide rect selection does when committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CanvasSelectionKind {
    /// Quick OCR of the rect (optionally creating a bubble with a recent character).
    QuickOcr,
    /// Opens the advanced recognition window on the rect.
    AdvancedOcr,
    /// The page crop of an image bubble (the selected one, or a new one).
    ImageCrop,
}

impl CanvasSelectionKind {
    /// The selection kind a claimed gesture drives, `None` for a claim of another action (the
    /// shared zoom drag).
    #[must_use]
    pub(super) fn for_action(action: ActionId) -> Option<Self> {
        match action {
            OCR_QUICK_RECT | OCR_QUICK_RECT_TOUCH => Some(Self::QuickOcr),
            OCR_ADVANCED_RECT => Some(Self::AdvancedOcr),
            IMAGE_CROP_RECT => Some(Self::ImageCrop),
            _ => None,
        }
    }

    /// Whether committing this kind needs an available OCR route.
    #[must_use]
    fn needs_ocr_route(self) -> bool {
        match self {
            Self::QuickOcr | Self::AdvancedOcr => true,
            Self::ImageCrop => false,
        }
    }
}

/// A running canvas-wide rect selection (screen points).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct CanvasSelection {
    pub(super) kind: CanvasSelectionKind,
    pub(super) start: Pos2,
    pub(super) current: Pos2,
    /// The recent-character rank last held during the gesture (a released digit keeps it).
    pub(super) recent_character_rank: Option<usize>,
}

impl CanvasSelection {
    /// The selected rect.
    #[must_use]
    pub(super) fn rect(&self) -> Rect {
        Rect::from_two_pos(self.start, self.current)
    }
}

/// Whether `rect` is large enough to commit (both sides at least [`MIN_SELECTION_SIDE_PT`]).
#[must_use]
pub(super) fn is_committable(rect: Rect) -> bool {
    rect.width() >= MIN_SELECTION_SIDE_PT && rect.height() >= MIN_SELECTION_SIDE_PT
}

/// What one frame did to the selection slot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum SelectionStep {
    /// No selection runs (or a claim of another action).
    Idle,
    /// A selection started this frame.
    Started,
    /// The selection follows the pointer.
    Running,
    /// The selection ended committed: run it (if large enough).
    Commit(CanvasSelection),
    /// The selection ended cancelled, or its claim vanished: nothing runs.
    Dropped,
}

/// Advances `slot` by this frame's `claim` of the canvas view (pure). A `Started` claim of a
/// selection action starts a selection at its origin when `may_start` (the press is on a page and
/// the kind may run); an `Active` one moves its corner and samples the recent-character rank (a
/// rank that is no longer held keeps the last one); `Finished(Commit)` commits it,
/// `Finished(Cancel)` or a vanished claim drops it.
pub(super) fn advance_selection(slot: &mut Option<CanvasSelection>, claim: Option<&ClaimedGesture>, may_start: bool) -> SelectionStep {
    let Some((claim, kind)) = claim.and_then(|claim| CanvasSelectionKind::for_action(claim.action).map(|kind| (claim, kind))) else {
        return if slot.take().is_some() { SelectionStep::Dropped } else { SelectionStep::Idle };
    };
    let rank = recent_char_rank(&claim.params);
    match claim.phase {
        GesturePhase::Started => {
            *slot = may_start.then_some(CanvasSelection { kind, start: claim.origin, current: claim.current, recent_character_rank: rank });
            if slot.is_some() { SelectionStep::Started } else { SelectionStep::Idle }
        }
        GesturePhase::Active => match slot.as_mut() {
            Some(selection) => {
                selection.current = claim.current;
                selection.recent_character_rank = rank.or(selection.recent_character_rank);
                SelectionStep::Running
            }
            None => SelectionStep::Idle,
        },
        GesturePhase::Finished(policy) => match (slot.take(), policy) {
            (Some(mut selection), ReleasePolicy::Commit) => {
                selection.current = claim.current;
                selection.recent_character_rank = rank.or(selection.recent_character_rank);
                SelectionStep::Commit(selection)
            }
            (Some(_), ReleasePolicy::Cancel) => SelectionStep::Dropped,
            (None, ReleasePolicy::Commit | ReleasePolicy::Cancel) => SelectionStep::Idle,
        },
    }
}

/// What a point hits in the lines edit mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct LineEditHit {
    /// (line index, handle index) of a handle of the SELECTED line within the hit radius.
    pub(super) handle: Option<(usize, usize)>,
    /// The topmost (last drawn) line containing the point.
    pub(super) line: Option<usize>,
}

/// The line / handle `pos` hits among `line_rects` (scene rects per line, `None` for a line that
/// has no positive scene rect); handles exist only on the `selected` line (pure).
#[must_use]
pub(super) fn line_edit_hits(pos: Pos2, line_rects: &[Option<Rect>], selected: Option<usize>) -> LineEditHit {
    let handle = selected.and_then(|line_idx| {
        let rect = line_rects.get(line_idx).copied().flatten()?;
        text_detector_line_handle_points(rect).into_iter().position(|handle| handle.distance(pos) <= LINE_HANDLE_HIT_RADIUS_PT).map(|handle_idx| (line_idx, handle_idx))
    });
    let line = line_rects.iter().enumerate().rev().find_map(|(line_idx, rect)| rect.filter(|rect| rect.contains(pos)).map(|_| line_idx));
    LineEditHit { handle, line }
}

impl TranslationTabState {
    /// Drives this frame's canvas-wide rect selection from the router's claim on `canvas`'s view:
    /// starts it (refusing an OCR selection on an unavailable route with a toast), follows it,
    /// commits it, and paints the running rect.
    pub(super) fn drive_canvas_selection(&mut self, ctx: &egui::Context, canvas: &mut CanvasView, project: &ProjectData) {
        let claim = self.input_frame.as_ref().and_then(|frame| frame.claimed(canvas.view_id())).cloned();
        let mut may_start = false;
        if let Some(claim) = claim.as_ref().filter(|claim| claim.phase == GesturePhase::Started)
            && let Some(kind) = CanvasSelectionKind::for_action(claim.action)
            && contains_any_page(canvas, project, claim.origin)
        {
            match (kind.needs_ocr_route(), self.current_ocr_route_availability()) {
                // The press is claimed either way (it does not pan); the refusal is said aloud
                // once, at the start, instead of drawing a rect that cannot run.
                (true, Err(off)) => self.push_toast(ctx, off.user_message(), Severity::Error, 2.6),
                (true, Ok(_)) | (false, _) => may_start = true,
            }
        }
        if let SelectionStep::Commit(selection) = advance_selection(&mut self.canvas_selection, claim.as_ref(), may_start) {
            self.finish_canvas_selection(ctx, canvas, project, selection);
        }
        if let Some(selection) = self.canvas_selection {
            let rect = selection.rect();
            let color = match selection.kind {
                CanvasSelectionKind::ImageCrop => Color32::from_rgb(220, 35, 35),
                CanvasSelectionKind::QuickOcr | CanvasSelectionKind::AdvancedOcr => ms_theme::canvas::SELECTION,
            };
            let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("translation_canvas_selection_painter")));
            painter.rect_filled(rect, 0.0, color.gamma_multiply_u8(60));
            painter.rect_stroke(rect, 0.0, Stroke::new(2.0, color), egui::StrokeKind::Outside);
            ctx.request_repaint();
        }
    }

    /// Runs a committed selection of at least 4x4 points: quick OCR, the advanced window, or the
    /// image-bubble page crop. A smaller one is dropped.
    fn finish_canvas_selection(&mut self, ctx: &egui::Context, canvas: &mut CanvasView, project: &ProjectData, selection: CanvasSelection) {
        let rect = selection.rect();
        if !is_committable(rect) {
            return;
        }
        match selection.kind {
            CanvasSelectionKind::QuickOcr => self.start_ocr_for_scene_rect(ctx, canvas, project, rect, selection.recent_character_rank),
            CanvasSelectionKind::AdvancedOcr => self.open_advanced_recognition_for_scene_rect(ctx, canvas, project, rect),
            CanvasSelectionKind::ImageCrop => self.apply_image_crop_selection(ctx, canvas, project, rect),
        }
    }

    /// The detected-lines editor on page `page_idx`: an interaction rect over the page whose
    /// gestures select (click on a line), deselect (click outside every line), move (drag a line)
    /// and resize (drag a handle of the selected line) the page's detected lines, then the lines
    /// and the selected line's handles painted with `painter`.
    pub(super) fn draw_text_detector_line_edit_overlay_on_page(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, page_idx: usize, page_rect: Rect) {
        let Some(result) = self.text_detector_results.get(&page_idx) else {
            if self.text_detector_line_selection.is_some_and(|selection| selection.page_idx == page_idx) {
                self.text_detector_line_selection = None;
                self.text_detector_line_drag_state = None;
            }
            return;
        };
        let source_w = result.source_size[0].max(1) as f32;
        let source_h = result.source_size[1].max(1) as f32;
        let lines = result.blocks.clone();
        let line_rects: Vec<Option<Rect>> = lines.iter().map(|rect| source_rect_to_scene_rect(page_rect, source_w, source_h, *rect)).collect();
        let selected_idx = self.text_detector_line_selection.filter(|selection| selection.page_idx == page_idx).map(|selection| selection.line_idx);

        let response = ui.interact(page_rect, egui::Id::new(("translation_text_detector_lines_edit", page_idx)), egui::Sense::click_and_drag());
        // Targets are decided at the press point, so a drag that leaves its line keeps it.
        let press = ui.ctx().input(|input| input.pointer.press_origin()).or_else(|| response.interact_pointer_pos());
        let hit = press.map(|pos| line_edit_hits(pos, &line_rects, selected_idx)).unwrap_or_default();
        let candidates = [(TARGET_LINE_HANDLE, hit.handle.is_some()), (TARGET_LINE, hit.line.is_some()), (TARGET_LINES_PAGE, true)];
        let gesture = self.input_frame.as_ref().and_then(|frame| frame.gesture_for(&response, &candidates));

        if let Some(gesture) = gesture {
            match (gesture.action, gesture.phase) {
                (LINES_SELECT, HitPhase::Clicked) => {
                    if let Some(line_idx) = hit.line {
                        self.text_detector_line_selection = Some(TextDetectorLineSelection { page_idx, line_idx });
                    }
                }
                (LINES_DESELECT, HitPhase::Clicked) => {
                    self.text_detector_line_selection = None;
                    self.text_detector_line_drag_state = None;
                }
                (LINES_RESIZE | LINES_MOVE, HitPhase::DragStarted) => {
                    let kind = match (gesture.action, hit.handle, hit.line) {
                        (LINES_RESIZE, Some((line_idx, handle_idx)), _) => Some((line_idx, TextDetectorLineDragKind::Resize { handle_idx })),
                        (LINES_MOVE, _, Some(line_idx)) => Some((line_idx, TextDetectorLineDragKind::Move)),
                        _ => None,
                    };
                    if let Some((line_idx, kind)) = kind
                        && let Some(press) = press
                        && let Some(start_pointer_src) = scene_pos_to_source(page_rect, source_w, source_h, press)
                        && let Some(start_rect) = lines.get(line_idx).copied()
                    {
                        let selection = TextDetectorLineSelection { page_idx, line_idx };
                        self.text_detector_line_selection = Some(selection);
                        self.text_detector_line_drag_state = Some(TextDetectorLineDragState { selection, start_pointer_src, start_rect, kind });
                    }
                }
                _ => {}
            }
            if matches!(gesture.action, LINES_RESIZE | LINES_MOVE) && matches!(gesture.phase, HitPhase::DragStarted | HitPhase::Dragging) {
                self.apply_line_drag(page_idx, page_rect, source_w, source_h, response.interact_pointer_pos());
            }
        }
        if response.drag_stopped() && self.text_detector_line_drag_state.is_some_and(|drag| drag.selection.page_idx == page_idx) {
            self.text_detector_line_drag_state = None;
        }
        self.draw_line_edit_page_menu(&response, page_idx, page_rect);

        let selected_idx = self.text_detector_line_selection.filter(|selection| selection.page_idx == page_idx).map(|selection| selection.line_idx);
        let lines = self.text_detector_results.get(&page_idx).map(|result| result.blocks.clone()).unwrap_or_default();
        for (line_idx, rect) in lines.iter().copied().enumerate() {
            let Some(scene_rect) = source_rect_to_scene_rect(page_rect, source_w, source_h, rect) else {
                continue;
            };
            let selected = Some(line_idx) == selected_idx;
            let stroke = if selected { Stroke::new(2.2, Color32::from_rgb(120, 255, 120)) } else { Stroke::new(1.2, Color32::from_rgb(0, 255, 0)) };
            painter.rect_stroke(scene_rect, 0.0, stroke, egui::StrokeKind::Outside);
            if selected {
                for handle_pos in text_detector_line_handle_points(scene_rect) {
                    painter.circle_filled(handle_pos, 4.0, Color32::from_rgb(120, 255, 120));
                    painter.circle_stroke(handle_pos, 4.0, Stroke::new(1.0, Color32::from_rgb(20, 90, 20)));
                }
            }
        }
    }

    /// The page menu of the lines edit mode ("create line" at the pointer). The editor's rect lies
    /// over the canvas page row and takes its secondary clicks, so it opens the menu itself, on
    /// the canvas's own page-menu gesture (`ms_canvas::input::PAGE_MENU`); a secondary click that
    /// binding does not own opens nothing.
    fn draw_line_edit_page_menu(&mut self, response: &egui::Response, page_idx: usize, page_rect: Rect) {
        let opened = self
            .input_frame
            .as_ref()
            .and_then(|frame| frame.gesture_for(response, &[(ms_canvas::input::TARGET_PAGE, true)]))
            .is_some_and(|hit| hit.action == ms_canvas::input::PAGE_MENU && hit.phase == HitPhase::Clicked);
        if response.secondary_clicked() && !opened {
            return;
        }
        if opened {
            // The menu remembers where it opened: the new line goes there.
            let at = response.interact_pointer_pos().or_else(|| response.hover_pos());
            self.text_detector_line_menu_uv = at.map(|pos| page_uv(page_rect, pos));
        }
        let anchor = (opened && !response.secondary_clicked()).then(|| response.interact_pointer_pos().or_else(|| response.hover_pos())).flatten();
        let mut create_at = None;
        ms_input::touch::context_menu_popup(response, anchor).show(|ui| {
            if ui.button(t!("translation.tab.create_line_button")).clicked() {
                create_at = Some(self.text_detector_line_menu_uv.unwrap_or(egui::pos2(0.5, 0.5)));
                ui.close();
            }
        });
        if let Some(uv) = create_at
            && let Err(error) = self.add_text_detector_line_at_uv(page_idx, uv)
        {
            ms_log::runtime_log::log_warn(format!("[translation] lines edit: could not add a line on page {page_idx}: {error}"));
        }
    }

    /// Moves or resizes the dragged line of page `page_idx` to follow `pointer` (screen points).
    fn apply_line_drag(&mut self, page_idx: usize, page_rect: Rect, source_w: f32, source_h: f32, pointer: Option<Pos2>) {
        let Some(drag) = self.text_detector_line_drag_state.filter(|drag| drag.selection.page_idx == page_idx) else {
            return;
        };
        let Some(pointer_src) = pointer.and_then(|pointer| scene_pos_to_source(page_rect, source_w, source_h, pointer)) else {
            return;
        };
        let Some(line) = self.text_detector_results.get_mut(&page_idx).and_then(|result| result.blocks.get_mut(drag.selection.line_idx)) else {
            return;
        };
        let moved = match drag.kind {
            TextDetectorLineDragKind::Move => move_text_detector_rect(drag.start_rect, drag.start_pointer_src, pointer_src, source_w, source_h),
            TextDetectorLineDragKind::Resize { handle_idx } => resize_text_detector_rect(drag.start_rect, pointer_src, handle_idx, source_w, source_h),
        };
        if *line != moved {
            *line = moved;
            self.line_edited_pages.insert(page_idx);
        }
    }

    /// Deletes the selected detected line (lines edit command); nothing without a selection.
    pub(crate) fn delete_selected_text_detector_line(&mut self) {
        let Some(selection) = self.text_detector_line_selection.take() else {
            return;
        };
        if let Some(result) = self.text_detector_results.get_mut(&selection.page_idx)
            && selection.line_idx < result.blocks.len()
        {
            result.blocks.remove(selection.line_idx);
            self.line_edited_pages.insert(selection.page_idx);
        }
        self.text_detector_line_drag_state = None;
    }

    /// Drops the line selection and drag of page `page_idx` (its blocks were replaced or
    /// re-sorted, so their index is stale); another page's selection is kept.
    pub(super) fn drop_line_edit_state_on_page(&mut self, page_idx: usize) {
        if self.text_detector_line_selection.is_some_and(|selection| selection.page_idx == page_idx) {
            self.text_detector_line_selection = None;
        }
        if self.text_detector_line_drag_state.is_some_and(|drag| drag.selection.page_idx == page_idx) {
            self.text_detector_line_drag_state = None;
        }
    }

    /// Hands every line-edited page (`line_edited_pages`) whose drag has ended to the shared
    /// `TextMaskModel`, so Cleaning's quick clean sees the edited boxes. Per page:
    /// - no model page, or one whose boxes are unknown (a hand-edited mask) or of another size:
    ///   left alone (a hand-edited mask's boxes stay unknown);
    /// - a mask-less (legacy) result: the model page is the display raster of the OLD resolved
    ///   lines, so it is rebuilt from the edited ones (`materialize_text_mask_page_from_blocks_if_missing`);
    /// - otherwise the model keeps its mask and takes the result's raw lines as its boxes.
    ///
    /// Called every frame; a no-op while nothing was edited.
    pub(super) fn flush_line_edits_to_text_mask_model(&mut self) {
        if self.line_edited_pages.is_empty() {
            return;
        }
        let dragging = self.text_detector_line_drag_state.map(|drag| drag.selection.page_idx);
        let ready: Vec<usize> = self.line_edited_pages.iter().copied().filter(|page_idx| Some(*page_idx) != dragging).collect();
        for page_idx in ready {
            self.line_edited_pages.remove(&page_idx);
            self.sync_text_mask_blocks_after_line_edit(page_idx);
        }
    }

    /// One page of [`Self::flush_line_edits_to_text_mask_model`].
    fn sync_text_mask_blocks_after_line_edit(&mut self, page_idx: usize) {
        let Some(model) = self.text_mask_model.clone() else {
            return;
        };
        let Some(result) = self.text_detector_results.get(&page_idx) else {
            return;
        };
        let rebuild_raster = {
            let mut model = match model.lock() {
                Ok(model) => model,
                Err(error) => {
                    ms_log::runtime_log::log_error(format!("[text-detector] text mask model lock poisoned, edited lines of page {page_idx} not mirrored: {error}"));
                    return;
                }
            };
            let Some(page) = model.page(page_idx).filter(|page| page.blocks.is_some() && page.source_size == result.source_size) else {
                return;
            };
            if result.mask_alpha.is_empty() {
                model.remove_page(page_idx);
                true
            } else {
                let page = page.clone();
                model.set_page_with_blocks(page_idx, page.source_size, page.mask_size, page.mask_alpha, super::detection_mask::detector_rects_to_blocks_px(&result.blocks));
                false
            }
        };
        if rebuild_raster {
            self.materialize_text_mask_page_from_blocks_if_missing(page_idx);
        }
        self.text_detector_mask_textures.remove(&page_idx);
    }

    /// Adds a line in the middle of `canvas`'s current page (lines edit command). A page whose
    /// detector entry cannot be created (its size is not known yet) is logged and the detector
    /// status line says why.
    pub(crate) fn create_text_detector_line_at_page_center(&mut self, canvas: &CanvasView) {
        let page_idx = canvas.current_page_idx();
        if let Err(error) = self.create_text_detector_line_at_uv(page_idx, egui::pos2(0.5, 0.5)) {
            ms_log::runtime_log::log_warn(format!("[translation] lines edit: could not add a line on page {page_idx}: {error}"));
            self.set_text_detector_status(error, Severity::Error);
        }
    }

    /// The text-mask brush on page `page_idx`: an interaction rect over the page whose paint /
    /// erase drags edit the mask (a stroke continues from the last point while its kind stays the
    /// same), plus the brush cursor under the pointer.
    pub(super) fn draw_text_detector_mask_edit_overlay_on_page(&mut self, ui: &mut egui::Ui, page_idx: usize, page_rect: Rect, zoom: f32) {
        self.materialize_text_mask_page_from_blocks_if_missing(page_idx);
        let response = ui.interact(page_rect, egui::Id::new(("translation_text_detector_mask_edit", page_idx)), egui::Sense::click_and_drag());
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }
        let gesture = self.input_frame.as_ref().and_then(|frame| frame.gesture_for(&response, &[(TARGET_MASK_PAGE, true)]));
        let press = ui.ctx().input(|input| input.pointer.press_origin());
        let erase_of = |action| match action {
            MASK_PAINT => Some(false),
            MASK_ERASE => Some(true),
            _ => None,
        };
        // A click dabs one brush footprint at the press point and ends there.
        if let Some(gesture) = gesture.as_ref().filter(|gesture| gesture.phase == HitPhase::Clicked)
            && let Some(erase) = erase_of(gesture.action)
            && let Some(pos) = press.or_else(|| response.interact_pointer_pos()).filter(|pos| page_rect.contains(*pos))
        {
            if let Some((source_size, mask_size)) = self.text_mask_target_sizes(page_idx, page_rect, zoom) {
                self.paint_text_mask_segment(TextMaskPageGeom { page_idx, source_size, mask_size, page_rect }, pos, pos, erase);
            }
            self.text_detector_mask_stroke_state = None;
            self.draw_mask_brush_cursor(ui, page_idx, page_rect, zoom, response.hover_pos());
            return;
        }
        let stroke = gesture.and_then(|gesture| match gesture.phase {
            HitPhase::DragStarted | HitPhase::Dragging => erase_of(gesture.action).map(|erase| (erase, gesture.phase == HitPhase::DragStarted)),
            HitPhase::Clicked | HitPhase::DoubleClicked | HitPhase::DragStopped => None,
        });
        let Some((erase, started)) = stroke else {
            self.text_detector_mask_stroke_state = None;
            self.draw_mask_brush_cursor(ui, page_idx, page_rect, zoom, response.hover_pos());
            return;
        };
        if let Some(pos) = response.interact_pointer_pos().filter(|pos| page_rect.contains(*pos)) {
            // A new stroke starts at the press point (the drag threshold was crossed since), a
            // running one at its last point.
            let start_pos = match self.text_detector_mask_stroke_state {
                Some(state) if !started && state.page_idx == page_idx && state.erase == erase => state.last_scene_pos,
                _ => press.filter(|press| started && page_rect.contains(*press)).unwrap_or(pos),
            };
            if let Some((source_size, mask_size)) = self.text_mask_target_sizes(page_idx, page_rect, zoom) {
                self.paint_text_mask_segment(TextMaskPageGeom { page_idx, source_size, mask_size, page_rect }, start_pos, pos, erase);
            }
            self.text_detector_mask_stroke_state = Some(TextDetectorMaskStrokeState { page_idx, erase, last_scene_pos: pos });
        }
        self.draw_mask_brush_cursor(ui, page_idx, page_rect, zoom, response.hover_pos());
    }

    /// Paints the mask brush's circle cursor at `hover` when it is over the page.
    fn draw_mask_brush_cursor(&self, ui: &mut egui::Ui, page_idx: usize, page_rect: Rect, zoom: f32, hover: Option<Pos2>) {
        let Some(pointer) = hover.filter(|pointer| page_rect.contains(*pointer)) else {
            return;
        };
        let mask_size = self.text_mask_target_sizes(page_idx, page_rect, zoom).map_or([1, 1], |(_, mask_size)| mask_size);
        let image_size = [usize::try_from(mask_size[0].max(1)).unwrap_or(1), usize::try_from(mask_size[1].max(1)).unwrap_or(1)];
        self.text_detector_mask_brush.draw_circle_cursor_on_image(ui, page_rect, image_size, pointer);
    }
}

/// `pos` (screen points) as page-normalized coordinates of `page_rect`, clamped to the page.
#[must_use]
pub(super) fn page_uv(page_rect: Rect, pos: Pos2) -> Pos2 {
    let width = page_rect.width().max(f32::EPSILON);
    let height = page_rect.height().max(f32::EPSILON);
    egui::pos2(((pos.x - page_rect.left()) / width).clamp(0.0, 1.0), ((pos.y - page_rect.top()) / height).clamp(0.0, 1.0))
}

/// Whether `pos` (screen points) lies on any page of `project` on `canvas`.
pub(super) fn contains_any_page(canvas: &CanvasView, project: &ProjectData, pos: Pos2) -> bool {
    project.pages.iter().any(|page| canvas.page_scene_rect(page.idx).is_some_and(|rect| rect.contains(pos)))
}

/// The eight resize handles of a line's scene rect: corners and edge midpoints, clockwise from
/// the top-left (the handle index `resize_text_detector_rect` understands).
fn text_detector_line_handle_points(scene_rect: Rect) -> [Pos2; 8] {
    [
        scene_rect.left_top(),
        egui::pos2(scene_rect.center().x, scene_rect.top()),
        scene_rect.right_top(),
        egui::pos2(scene_rect.right(), scene_rect.center().y),
        scene_rect.right_bottom(),
        egui::pos2(scene_rect.center().x, scene_rect.bottom()),
        scene_rect.left_bottom(),
        egui::pos2(scene_rect.left(), scene_rect.center().y),
    ]
}

/// `start_rect` moved by the pointer's travel since `start_pointer_src` (source pixels), kept
/// inside the page.
fn move_text_detector_rect(start_rect: TextDetectorRect, start_pointer_src: Pos2, pointer_src: Pos2, source_w: f32, source_h: f32) -> TextDetectorRect {
    let width = (start_rect.x2 - start_rect.x1).max(1.0);
    let height = (start_rect.y2 - start_rect.y1).max(1.0);
    let dx = pointer_src.x - start_pointer_src.x;
    let dy = pointer_src.y - start_pointer_src.y;
    let max_x1 = (source_w - width).max(0.0);
    let max_y1 = (source_h - height).max(0.0);
    let x1 = (start_rect.x1 + dx).clamp(0.0, max_x1);
    let y1 = (start_rect.y1 + dy).clamp(0.0, max_y1);
    TextDetectorRect::from_xyxy(x1, y1, x1 + width, y1 + height).unwrap_or(start_rect)
}

/// `start_rect` with the edges of handle `handle_idx` moved to the pointer (source pixels), at
/// least 4 px per side and inside the page.
fn resize_text_detector_rect(start_rect: TextDetectorRect, pointer_src: Pos2, handle_idx: usize, source_w: f32, source_h: f32) -> TextDetectorRect {
    let min_side = 4.0f32;
    let mut x1 = start_rect.x1;
    let mut y1 = start_rect.y1;
    let mut x2 = start_rect.x2;
    let mut y2 = start_rect.y2;

    let move_left = matches!(handle_idx, 0 | 6 | 7);
    let move_right = matches!(handle_idx, 2..=4);
    let move_top = matches!(handle_idx, 0..=2);
    let move_bottom = matches!(handle_idx, 4..=6);

    if move_left {
        x1 = pointer_src.x.clamp(0.0, (x2 - min_side).max(0.0));
    }
    if move_right {
        x2 = pointer_src.x.clamp((x1 + min_side).min(source_w), source_w);
    }
    if move_top {
        y1 = pointer_src.y.clamp(0.0, (y2 - min_side).max(0.0));
    }
    if move_bottom {
        y2 = pointer_src.y.clamp((y1 + min_side).min(source_h), source_h);
    }

    TextDetectorRect::from_xyxy(x1, y1, x2, y2).unwrap_or(start_rect)
}

#[cfg(test)]
mod tests;
