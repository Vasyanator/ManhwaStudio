/*
File: tab/touch_layers.rs

Purpose:
Finger input on the typing tab's text layers (text/image overlays). The finger decision itself is
owned by `ms_widgets::touch_nav` and reaches this tab only as typed canvas output
(`CanvasView::touch_tap` / `touch_long_press_armed_at` / `touch_selection`); nothing here reads raw
touches. The rules, kept deliberately small:
- a TAP on a text layer selects it (the selected layer keeps the tap inside its own grab area, like
  the mouse's sticky focus); a tap anywhere else deselects. A tap while the create editor is open
  only commits the editor (`create_upload.rs`).
- a PRESS-AND-HOLD on the SELECTED layer, then a drag, moves that layer through the shared move
  primitive (`move_layer.rs`, source `Touch`): one session per gesture, the lift settles it (one
  deferred write), a cancel puts the layer back and writes nothing.
- everything else is the canvas' and the create tool's: a finger elsewhere pans, a hold lifted
  without moving opens the canvas menu, a hold-drag on any other point is the create rectangle
  when one can start (else it pans).

Key structures:
- `TypingTouchFrame`: this frame's finger facts, copied from the canvas.
- `TypingTouchGates`, `TypingTouchOutcome`: the driver's input gates and its answer to the hook.
- `TypingTouchLayerMove`: the running finger move and the geometry a cancel restores.

Key functions:
- `TypingTextOverlayLayer::drive_touch_layer_input`: the once-per-frame driver, called from the
  canvas' top-left hook (it runs after the canvas took the frame's touch output and before the
  canvas publishes its touch surface, so the long-press flag it answers reaches the router in time).
- `touch_overlay_at` / `touch_move_target_at`: the hit-tests, over the mouse's grab rule
  (`overlay_grab_mesh_scene`) and the composite draw order.
- `canvas_page_view` / `canvas_page_view_at`: the `PageView` of a page as the canvas drew it.

Notes:
Raster layers do not react to a finger (the mouse keeps selecting them); a tap on one reads as a tap
outside any text layer. A layer in a transform mode (raster or vector) is never moved by a finger:
its handles are mouse-only, and a whole-layer move there would fight them.
*/

use super::*;
use ms_widgets::touch_nav::{TouchSelection, TouchSelectionEnd};

/// This frame's finger facts on the typing canvas, copied from `CanvasView` (all screen points).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct TypingTouchFrame {
    /// `CanvasView::touch_tap`: a short still touch lifted this frame, at its press point.
    pub(super) tap: Option<Pos2>,
    /// `CanvasView::touch_long_press_armed_at`: a held finger whose long-press armed.
    pub(super) armed_at: Option<Pos2>,
    /// `CanvasView::touch_selection`: the long-press drag in progress, if the canvas reports one.
    pub(super) selection: Option<TouchSelection>,
}

impl TypingTouchFrame {
    /// Copies this frame's finger facts out of `canvas` (valid inside its `draw`, after its input
    /// step — i.e. in the canvas hooks).
    pub(super) fn from_canvas(canvas: &CanvasView) -> Self {
        Self {
            tap: canvas.touch_tap(),
            armed_at: canvas.touch_long_press_armed_at(),
            selection: canvas.touch_selection(),
        }
    }
}

/// Per-frame gates of the finger driver, from state the layer does not own.
#[derive(Debug, Clone, Copy)]
pub(super) struct TypingTouchGates {
    /// A panel owns the canvas clicks this frame (the mask panel is open, or the eyedropper is
    /// active): no finger selection change and no new finger move starts. A running move still ends.
    pub(super) panel_blocks: bool,
    /// The panel's whole-pixel movement policy, applied to a finger move as to a mouse move.
    pub(super) strict_pixel_movement: bool,
}

/// What the finger driver tells the hook.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct TypingTouchOutcome {
    /// This frame's `touch_selection` belongs to a layer move: the create tool must not see it.
    pub(super) selection_consumed: bool,
    /// The armed long-press sits on the selected layer: set `CanvasView::set_long_press_selection`
    /// THIS frame, so the drag that follows arrives as a selection (it would otherwise pan).
    pub(super) long_press_selection: bool,
    /// Selection or geometry changed: request a repaint.
    pub(super) changed: bool,
}

/// A finger move in progress: which overlay, on which page, and its geometry before the gesture
/// (restored exactly when the drag is cancelled).
#[derive(Debug, Clone)]
pub(super) struct TypingTouchLayerMove {
    overlay_idx: usize,
    page_idx: usize,
    original_center: [f32; 2],
    original_mesh: Option<TypingOverlayDeformMesh>,
}

/// The `PageView` of `page_idx` as `canvas` drew it this frame (`None` when the page was not
/// drawn). Its rect is the one the per-page hooks received, so hit-tests agree with the mouse.
pub(super) fn canvas_page_view(canvas: &CanvasView, page_idx: usize) -> Option<PageView> {
    canvas.page_scene_rect(page_idx).map(|image_rect| PageView { page_idx, image_rect, zoom: canvas.zoom() })
}

/// The `PageView` of the page drawn under `pos` this frame, if any.
pub(super) fn canvas_page_view_at(canvas: &CanvasView, pos: Pos2) -> Option<PageView> {
    canvas.page_index_at_scene_pos(pos).and_then(|page_idx| canvas_page_view(canvas, page_idx))
}

impl TypingTextOverlayLayer {
    /// Applies this frame's finger facts to the text layers; call once per frame from the canvas'
    /// top-left hook, BEFORE the create UI (which must not see a consumed drag) and before the
    /// selection is synced into the edit panel.
    ///
    /// `view_at` resolves the page drawn under a screen point, `view_of` a page by index (see
    /// [`canvas_page_view_at`] / [`canvas_page_view`]). Order: a running finger move is driven or
    /// ended first; then a drag that began this frame on the selected layer opens a move; then a tap
    /// selects / deselects; last, the armed point decides the long-press flag.
    pub(super) fn drive_touch_layer_input(
        &mut self,
        frame: TypingTouchFrame,
        gates: TypingTouchGates,
        view_at: &dyn Fn(Pos2) -> Option<PageView>,
        view_of: &dyn Fn(usize) -> Option<PageView>,
    ) -> TypingTouchOutcome {
        let mut outcome = TypingTouchOutcome::default();
        let may_start = !gates.panel_blocks && self.layout_editor.is_none();
        if let Some(active) = self.touch_layer_move.take() {
            outcome.changed = true;
            match frame.selection {
                Some(selection) => {
                    outcome.selection_consumed = true;
                    self.step_touch_layer_move(active, selection, gates.strict_pixel_movement, view_of);
                }
                None => {
                    // The router reports a running selection every frame until its end, so a
                    // missing one means the end was never delivered (the canvas skipped a frame's
                    // take). Keep what the user sees: settle, as a lift would.
                    ms_log::runtime_log::log_warn(format!(
                        "[typing] a finger move of text layer {} on page {} lost its end; settling it as a lift.",
                        active.overlay_idx, active.page_idx
                    ));
                    if self.touch_move_session_open(&active) {
                        self.settle_layer_move();
                    }
                }
            }
        } else if let Some(selection) = frame.selection
            && selection.began
            && may_start
            && let Some(view) = view_at(selection.origin)
            && let Some(overlay_idx) = self.touch_move_target_at(view, selection.origin)
        {
            // The drag started on the selected layer: it is a move, never the create rectangle,
            // even when the move cannot open.
            outcome.selection_consumed = true;
            outcome.changed = true;
            self.begin_touch_layer_move(overlay_idx, view, selection, gates.strict_pixel_movement, view_of);
        }

        // A tap while the create editor is open only commits it (`draw_text_editor`).
        if let Some(tap) = frame.tap
            && may_start
            && self.create_editor.is_none()
            && self.touch_layer_move.is_none()
        {
            outcome.changed |= self.apply_touch_tap(tap, view_at);
        }

        outcome.long_press_selection = may_start
            && self.touch_layer_move.is_none()
            && frame
                .armed_at
                .is_some_and(|point| view_at(point).and_then(|view| self.touch_move_target_at(view, point)).is_some());
        outcome
    }

    /// Opens the finger move of `overlay_idx` (the selected layer under `selection.origin`) and
    /// applies the drag's first frame.
    fn begin_touch_layer_move(
        &mut self,
        overlay_idx: usize,
        view: PageView,
        selection: TouchSelection,
        strict_pixel_movement: bool,
        view_of: &dyn Fn(usize) -> Option<PageView>,
    ) {
        let Some((original_center, original_mesh)) =
            self.overlays.get(overlay_idx).map(|overlay| (overlay.center_page_px, overlay.deform_mesh.clone()))
        else {
            return;
        };
        let target = TypingLayerMoveTarget::Overlay(overlay_idx);
        if !self.begin_layer_move(target, view.page_idx, TypingLayerMoveSource::Touch, Some(selection.origin), view) {
            ms_log::trace_log!(cat::INPUT, "touch_layer_move_begin_failed idx={overlay_idx} page={}", view.page_idx);
            return;
        }
        ms_log::trace_log!(cat::INPUT, "touch_layer_move_begin idx={overlay_idx} page={}", view.page_idx);
        let active = TypingTouchLayerMove { overlay_idx, page_idx: view.page_idx, original_center, original_mesh };
        self.step_touch_layer_move(active, selection, strict_pixel_movement, view_of);
    }

    /// One frame of a finger move: follow the finger, or end the gesture on its last frame —
    /// `Released` settles (one deferred write), `Cancelled` restores the original geometry and
    /// writes nothing. A session that something else already ended (a selection change, a flush,
    /// an arrow key) leaves the finger with nothing to drive.
    fn step_touch_layer_move(
        &mut self,
        active: TypingTouchLayerMove,
        selection: TouchSelection,
        strict_pixel_movement: bool,
        view_of: &dyn Fn(usize) -> Option<PageView>,
    ) {
        if !self.touch_move_session_open(&active) {
            return;
        }
        let target = TypingLayerMoveTarget::Overlay(active.overlay_idx);
        match selection.end {
            Some(TouchSelectionEnd::Cancelled) => {
                ms_log::trace_log!(cat::INPUT, "touch_layer_move_cancel idx={}", active.overlay_idx);
                self.cancel_touch_layer_move(&active);
            }
            end @ (None | Some(TouchSelectionEnd::Released)) => {
                // A page scrolled out of the drawn strip has no view this frame: the layer stays
                // where the last drawn frame put it.
                if let Some(view) = view_of(active.page_idx) {
                    self.drive_pointer_layer_move(target, active.page_idx, selection.pos, view, strict_pixel_movement);
                }
                if end.is_some() {
                    ms_log::trace_log!(cat::INPUT, "touch_layer_move_end idx={}", active.overlay_idx);
                    self.settle_layer_move();
                } else {
                    self.touch_layer_move = Some(active);
                }
            }
        }
    }

    /// Whether the open move session is still the one `active` drives.
    fn touch_move_session_open(&self, active: &TypingTouchLayerMove) -> bool {
        self.move_session.as_ref().is_some_and(|session| {
            session.source == TypingLayerMoveSource::Touch
                && session.target == TypingLayerMoveTarget::Overlay(active.overlay_idx)
                && session.page_idx == active.page_idx
        })
    }

    /// Ends a cancelled finger move like Esc: the session is dropped unsettled (no dirty mark, no
    /// write) and the overlay gets its pre-gesture center and mesh back exactly.
    fn cancel_touch_layer_move(&mut self, active: &TypingTouchLayerMove) {
        self.discard_layer_move();
        let Some(overlay) = self.overlays.get_mut(active.overlay_idx) else {
            return;
        };
        let changed = overlay.center_page_px != active.original_center || overlay.deform_mesh != active.original_mesh;
        overlay.center_page_px = active.original_center;
        overlay.deform_mesh = active.original_mesh.clone();
        if changed {
            // The mask clip followed the moved geometry; recompute it for the restored one.
            self.mark_overlay_geometry_changed(active.overlay_idx, true);
        }
    }

    /// A finger tap at `tap`: selects the text layer there, or deselects when there is none (also
    /// over a raster layer or outside every page). Returns whether the selection changed.
    fn apply_touch_tap(&mut self, tap: Pos2, view_at: &dyn Fn(Pos2) -> Option<PageView>) -> bool {
        match view_at(tap).and_then(|view| self.touch_overlay_at(view, tap)) {
            Some(overlay_idx) => {
                if self.selected_overlay_idx == Some(overlay_idx) {
                    return false;
                }
                ms_log::trace_log!(cat::TYPING, "touch_select overlay_idx={overlay_idx}");
                // A selection change ends whatever was being moved (as `select_raster` does).
                self.settle_layer_move();
                self.selected_overlay_idx = Some(overlay_idx);
                // One selection at a time across the two layer kinds; a transform mode belongs to
                // the layer that was selected (the mouse's next frame clears both the same way).
                self.selected_raster_idx = None;
                self.selected_raster_page = None;
                self.transform_mode_raster_idx = None;
                if self.transform_mode_overlay_idx.is_some_and(|idx| idx != overlay_idx) {
                    self.transform_mode_overlay_idx = None;
                }
                true
            }
            None => {
                let had_selection = self.selected_overlay_idx.is_some() || self.selected_raster_idx.is_some();
                if had_selection {
                    ms_log::trace_log!(cat::TYPING, "touch_deselect");
                    self.clear_selection();
                }
                had_selection
            }
        }
    }

    /// Whether `point` lies in the grab area of overlay `overlay_idx` drawn in `view` — the mouse's
    /// rule (`overlay_grab_mesh_scene`): the selected text's area is grown to the minimum side.
    fn overlay_grab_contains(&self, overlay_idx: usize, view: PageView, point: Pos2) -> bool {
        let Some(overlay) = self.overlays.get(overlay_idx) else {
            return false;
        };
        let geometry = overlay_scene_geometry(overlay, view);
        let is_selected_text = self.selected_overlay_idx == Some(overlay_idx) && overlay.kind == TypingOverlayKind::Text;
        let grab = overlay_grab_mesh_scene(&geometry.mesh_scene, geometry.mesh_cols, geometry.mesh_rows, is_selected_text);
        deform_mesh_contains_point(&grab, geometry.mesh_cols, geometry.mesh_rows, point)
    }

    /// The overlays of `view.page_idx` a finger can address, bottom-to-top in the composite draw
    /// order (the order the mouse's widgets register in, so the topmost wins): drawn (a texture, a
    /// non-degenerate footprint, not hidden by the plan) and not owned by the layout editor.
    fn touch_overlay_candidates(&self, view: PageView) -> Vec<usize> {
        let page_idx = view.page_idx;
        let editing_idx = self
            .layout_editor
            .as_ref()
            .filter(|editor| editor.mode == TypingLayoutEditorMode::Editing && editor.page_idx == page_idx)
            .map(|editor| editor.overlay_idx);
        let indices: Vec<usize> = self
            .overlays
            .iter()
            .enumerate()
            .filter(|(idx, overlay)| {
                overlay.page_idx == page_idx && overlay.texture.is_some() && Some(*idx) != editing_idx && {
                    let bounds = overlay_scene_geometry(overlay, view).bounds_rect;
                    bounds.width() > 0.5 && bounds.height() > 0.5
                }
            })
            .map(|(idx, _)| idx)
            .collect();
        self.page_fill_plan(page_idx, &indices)
            .iter()
            .filter_map(|step| match step.item {
                super::draw_page::MergedFillItem::Overlay(pos) => indices.get(pos).copied(),
                super::draw_page::MergedFillItem::Raster(_) => None,
            })
            .collect()
    }

    /// The overlay a finger at `point` addresses on `view.page_idx`: the selected overlay when the
    /// point is in its grab area (sticky, as the mouse's click in the selected frame), else the
    /// topmost overlay whose grab area contains it.
    pub(super) fn touch_overlay_at(&self, view: PageView, point: Pos2) -> Option<usize> {
        let candidates = self.touch_overlay_candidates(view);
        if let Some(selected) = self.selected_overlay_idx
            && candidates.contains(&selected)
            && self.overlay_grab_contains(selected, view, point)
        {
            return Some(selected);
        }
        candidates.into_iter().rev().find(|idx| self.overlay_grab_contains(*idx, view, point))
    }

    /// The selected overlay when `point` is in its grab area and a finger may move it there (it is
    /// on `view.page_idx`, drawn, and in no transform mode); `None` otherwise.
    pub(super) fn touch_move_target_at(&self, view: PageView, point: Pos2) -> Option<usize> {
        let selected = self.selected_overlay_idx?;
        if self.transform_mode_overlay_idx == Some(selected) {
            return None;
        }
        (self.touch_overlay_candidates(view).contains(&selected) && self.overlay_grab_contains(selected, view, point)).then_some(selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ms_models::layer_model::ordering::Band;

    /// The page view of the fixtures: a 100 px page drawn 1:1 at the scene origin.
    fn view() -> PageView {
        PageView { page_idx: 0, image_rect: Rect::from_min_size(Pos2::ZERO, Vec2::splat(100.0)), zoom: 1.0 }
    }

    fn view_at(pos: Pos2) -> Option<PageView> {
        view().image_rect.contains(pos).then(view)
    }

    fn view_of(page_idx: usize) -> Option<PageView> {
        (page_idx == 0).then(view)
    }

    fn gates() -> TypingTouchGates {
        TypingTouchGates { panel_blocks: false, strict_pixel_movement: false }
    }

    /// A drawn 20x20 text overlay `uid` on page 0 centred at `center` (page px).
    fn text_overlay(ctx: &egui::Context, uid: &str, center: [f32; 2]) -> TypingOverlayRuntime {
        let mut overlay = text_runtime_from_doc_node(uid, 0, center, 1.0, 0.0, None, false, false, 0, None, [20, 20], vec![255; 20 * 20 * 4]);
        overlay.texture = Some(ctx.load_texture(format!("touch_{uid}"), ColorImage::filled([20, 20], Color32::WHITE), TextureOptions::LINEAR));
        overlay
    }

    /// Page 0 with `a` at (30, 30) and `b` at (70, 70); `b` is above `a`.
    fn two_layers(ctx: &egui::Context) -> TypingTextOverlayLayer {
        let mut layer = TypingTextOverlayLayer::default();
        layer.bands_by_page.insert(0, vec![Band::PinnedText { uid: "a".into(), z: 0 }, Band::PinnedText { uid: "b".into(), z: 1 }]);
        layer.overlays = vec![text_overlay(ctx, "a", [30.0, 30.0]), text_overlay(ctx, "b", [70.0, 70.0])];
        layer
    }

    fn tap(at: Pos2) -> TypingTouchFrame {
        TypingTouchFrame { tap: Some(at), ..TypingTouchFrame::default() }
    }

    fn drag(origin: Pos2, pos: Pos2, began: bool, end: Option<TouchSelectionEnd>) -> TypingTouchFrame {
        TypingTouchFrame { selection: Some(TouchSelection { origin, pos, began, end }), ..TypingTouchFrame::default() }
    }

    fn drive(layer: &mut TypingTextOverlayLayer, frame: TypingTouchFrame) -> TypingTouchOutcome {
        layer.drive_touch_layer_input(frame, gates(), &view_at, &view_of)
    }

    #[test]
    fn tap_selects_switches_and_deselects() {
        let ctx = egui::Context::default();
        let mut layer = two_layers(&ctx);
        let outcome = drive(&mut layer, tap(Pos2::new(30.0, 30.0)));
        assert_eq!(layer.selected_overlay_idx, Some(0), "a tap on a layer selects it");
        assert!(outcome.changed);
        assert!(!outcome.selection_consumed);
        drive(&mut layer, tap(Pos2::new(72.0, 68.0)));
        assert_eq!(layer.selected_overlay_idx, Some(1), "a tap on another layer switches to it");
        let outcome = drive(&mut layer, tap(Pos2::new(5.0, 90.0)));
        assert_eq!(layer.selected_overlay_idx, None, "a tap outside every layer deselects");
        assert!(outcome.changed);
        let outcome = drive(&mut layer, tap(Pos2::new(5.0, 90.0)));
        assert!(!outcome.changed, "nothing to deselect: nothing changes");
    }

    #[test]
    fn tap_picks_the_topmost_layer_but_keeps_the_selected_one() {
        let ctx = egui::Context::default();
        let mut layer = two_layers(&ctx);
        layer.overlays[1].center_page_px = [35.0, 35.0];
        let overlap = Pos2::new(33.0, 33.0);
        drive(&mut layer, tap(overlap));
        assert_eq!(layer.selected_overlay_idx, Some(1), "the higher band wins an overlap");
        layer.selected_overlay_idx = Some(0);
        drive(&mut layer, tap(overlap));
        assert_eq!(layer.selected_overlay_idx, Some(0), "a tap inside the selected layer keeps it (sticky focus)");
    }

    #[test]
    fn tap_is_ignored_while_a_panel_owns_canvas_clicks() {
        let ctx = egui::Context::default();
        let mut layer = two_layers(&ctx);
        let blocked = TypingTouchGates { panel_blocks: true, strict_pixel_movement: false };
        layer.drive_touch_layer_input(tap(Pos2::new(30.0, 30.0)), blocked, &view_at, &view_of);
        assert_eq!(layer.selected_overlay_idx, None);
    }

    #[test]
    fn armed_long_press_asks_for_a_selection_only_on_the_selected_layer() {
        let ctx = egui::Context::default();
        let mut layer = two_layers(&ctx);
        layer.selected_overlay_idx = Some(0);
        let armed = |at: Pos2| TypingTouchFrame { armed_at: Some(at), ..TypingTouchFrame::default() };
        assert!(drive(&mut layer, armed(Pos2::new(30.0, 30.0))).long_press_selection, "hold on the selected layer");
        assert!(!drive(&mut layer, armed(Pos2::new(70.0, 70.0))).long_press_selection, "hold on an unselected layer");
        assert!(!drive(&mut layer, armed(Pos2::new(5.0, 90.0))).long_press_selection, "hold on bare page");
        layer.transform_mode_overlay_idx = Some(0);
        assert!(!drive(&mut layer, armed(Pos2::new(30.0, 30.0))).long_press_selection, "no finger move in a transform mode");
        assert_eq!(layer.selected_overlay_idx, Some(0), "an armed hold never changes the selection");
    }

    #[test]
    fn hold_drag_on_the_selected_layer_moves_it_and_the_lift_settles_once() {
        let ctx = egui::Context::default();
        let mut layer = two_layers(&ctx);
        layer.selected_overlay_idx = Some(0);
        let origin = Pos2::new(30.0, 30.0);
        let outcome = drive(&mut layer, drag(origin, Pos2::new(40.0, 35.0), true, None));
        assert!(outcome.selection_consumed, "the create tool must not see a move drag");
        assert_eq!(layer.overlays[0].center_page_px, [40.0, 35.0]);
        assert!(layer.move_session.as_ref().is_some_and(|session| session.source == TypingLayerMoveSource::Touch));
        // A frame without a pointer button never settles a finger move (a claimed finger never
        // presses), so the gesture stays one session.
        layer.drive_layer_move_settle(&ctx);
        assert!(layer.move_session.is_some());
        assert!(!layer.has_pending_placement_save(), "nothing is written mid-gesture");
        drive(&mut layer, drag(origin, Pos2::new(50.0, 45.0), false, None));
        assert_eq!(layer.overlays[0].center_page_px, [50.0, 45.0], "the total delta is from the press point");
        let outcome = drive(&mut layer, drag(origin, Pos2::new(55.0, 45.0), false, Some(TouchSelectionEnd::Released)));
        assert!(outcome.selection_consumed);
        assert_eq!(layer.overlays[0].center_page_px, [55.0, 45.0]);
        assert!(layer.move_session.is_none() && layer.touch_layer_move.is_none(), "the lift ends the gesture");
        assert!(layer.has_pending_placement_save(), "one deferred write for the whole gesture");
        assert_eq!(layer.overlays[1].center_page_px, [70.0, 70.0], "the other layer never moves");
    }

    #[test]
    fn drag_from_an_unselected_layer_is_left_to_the_create_tool() {
        let ctx = egui::Context::default();
        let mut layer = two_layers(&ctx);
        layer.selected_overlay_idx = Some(0);
        let outcome = drive(&mut layer, drag(Pos2::new(70.0, 70.0), Pos2::new(90.0, 90.0), true, None));
        assert!(!outcome.selection_consumed);
        assert!(layer.move_session.is_none());
        assert_eq!(layer.overlays[1].center_page_px, [70.0, 70.0]);
    }

    #[test]
    fn cancelled_move_restores_the_layer_and_writes_nothing() {
        let ctx = egui::Context::default();
        let mut layer = two_layers(&ctx);
        layer.selected_overlay_idx = Some(0);
        let origin = Pos2::new(30.0, 30.0);
        drive(&mut layer, drag(origin, Pos2::new(60.0, 20.0), true, None));
        assert_eq!(layer.overlays[0].center_page_px, [60.0, 20.0]);
        drive(&mut layer, drag(origin, Pos2::new(61.0, 20.0), false, Some(TouchSelectionEnd::Cancelled)));
        assert_eq!(layer.overlays[0].center_page_px, [30.0, 30.0], "cancel puts the layer back");
        assert!(layer.move_session.is_none() && layer.touch_layer_move.is_none());
        assert!(!layer.has_pending_placement_save(), "a cancelled gesture writes nothing");
        assert_eq!(layer.selected_overlay_idx, Some(0), "cancel keeps the selection");
    }

    #[test]
    fn a_lost_end_settles_the_move_as_a_lift() {
        let ctx = egui::Context::default();
        let mut layer = two_layers(&ctx);
        layer.selected_overlay_idx = Some(0);
        drive(&mut layer, drag(Pos2::new(30.0, 30.0), Pos2::new(40.0, 30.0), true, None));
        drive(&mut layer, TypingTouchFrame::default());
        assert!(layer.move_session.is_none() && layer.touch_layer_move.is_none());
        assert_eq!(layer.overlays[0].center_page_px, [40.0, 30.0]);
        assert!(layer.has_pending_placement_save());
    }
}
