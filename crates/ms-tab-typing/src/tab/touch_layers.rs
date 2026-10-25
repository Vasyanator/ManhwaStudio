/*
File: tab/touch_layers.rs

Purpose:
Finger input on the typing tab's text layers (text/image overlays). The finger decision itself is
owned by `ms_input::touch` and reaches this tab only as typed canvas output
(`CanvasView::touch_tap` / `touch_long_press_armed_at` / `touch_long_press_at` /
`touch_region_drag`); nothing here reads raw touches. The rules, kept deliberately small:
- a TAP on a text layer selects it (the selected layer keeps the tap inside its own grab area, like
  the mouse's sticky focus); a tap anywhere else deselects. A tap while the create editor is open
  only commits the editor (`create_upload.rs`).
- a finger PRESSED on the SELECTED layer and dragged (no hold needed; a hold first works too) moves
  that layer through the shared move primitive (`move_layer.rs`, source `Touch`): the selected
  layer's grab area is published as the canvas' touch drag region (`touch_drag_region`), so the
  router delivers that finger as a region drag instead of a pan. One session per gesture, the lift
  settles it (one deferred write), a cancel (second finger) puts the layer back and writes nothing.
- a LONG-PRESS lifted without moving on a text layer opens THAT layer's context menu (the one a
  right-click opens, with the same select side effect), never the canvas page menu.
- everything else is the canvas' and the create tool's: a finger elsewhere pans; a hold lifted
  off every text layer does nothing (the Typing canvas is read-only, so it has no page menu); a
  hold-drag off the selected layer is the create rectangle when one can start — the touch
  alternative of the `typing.create_rect` gesture, claimed by the input router (`create_upload.rs`)
  — and pans otherwise.

Key structures:
- `TypingTouchFrame`: this frame's finger facts, copied from the canvas.
- `TypingTouchGates`, `TypingTouchOutcome`: the driver's input gates and its answer to the hook.
- `TypingTouchLayerMove`: the running finger move and the geometry a cancel restores.
- `TypingTouchLayerMenu`: a long-press on a text layer whose lift opens that layer's menu.

Key functions:
- `TypingTextOverlayLayer::drive_touch_layer_input`: the once-per-frame driver, called from the
  canvas' top-left hook (it runs after the canvas took the frame's touch output and before the
  canvas publishes its touch surface, so the long-press flag it answers reaches the router in time).
- `touch_overlay_at` / `touch_move_target_at`: the hit-tests, over the mouse's grab rule
  (`overlay_grab_mesh_scene`) and the composite draw order.
- `touch_drag_region`: the selected layer's grab outline, published as the touch drag region.
- `suppresses_canvas_page_menu_for_touch` / `take_touch_layer_menu_at`: the layer-menu hand-off
  to the canvas page menu (suppressed) and to `draw_page.rs` (opened).
- `canvas_page_view` / `canvas_page_view_at`: the `PageView` of a page as the canvas drew it.

Notes:
The layer menu is decided from the ARMED press, one frame ahead: the canvas runs its page menu
(`suppress_canvas_page_context_menu`) before this tab's top-left hook sees the lift, so the frame
before the lift records which layer the armed press is on (`touch_layer_menu_candidate`; the router
reports the arm every frame while held). The lift then turns exactly that candidate into the menu
request, so the page menu is suppressed exactly when the layer menu opens — never both, never
neither. The layer's own context-menu widget opens it on the next frame's page pass (one frame
later than a right-click; a repaint is requested).
Raster layers do not react to a finger (the mouse keeps selecting them); a tap on one reads as a tap
outside any text layer. A layer in a transform mode (raster or vector) is never moved by a finger:
its handles are mouse-only, and a whole-layer move there would fight them.
*/

use super::*;
use ms_input::touch::{TouchSelection, TouchSelectionEnd};

/// This frame's finger facts on the typing canvas, copied from `CanvasView` (all screen points).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct TypingTouchFrame {
    /// `CanvasView::touch_tap`: a short still touch lifted this frame, at its press point.
    pub(super) tap: Option<Pos2>,
    /// `CanvasView::touch_long_press_armed_at`: a held finger whose long-press armed.
    pub(super) armed_at: Option<Pos2>,
    /// `CanvasView::touch_long_press_at`: an armed finger lifted without moving this frame.
    pub(super) long_press_at: Option<Pos2>,
    /// `CanvasView::touch_region_drag`: the drag of a finger that started in the published drag
    /// region (the selected layer's grab area), if one is running.
    pub(super) region_drag: Option<TouchSelection>,
}

impl TypingTouchFrame {
    /// Copies this frame's finger facts out of `canvas` (valid inside its `draw`, after its input
    /// step — i.e. in the canvas hooks).
    pub(super) fn from_canvas(canvas: &CanvasView) -> Self {
        Self {
            tap: canvas.touch_tap(),
            armed_at: canvas.touch_long_press_armed_at(),
            long_press_at: canvas.touch_long_press_at(),
            region_drag: canvas.touch_region_drag(),
        }
    }
}

/// Per-frame gates of the finger driver, from state the layer does not own.
#[derive(Debug, Clone, Copy)]
pub(super) struct TypingTouchGates {
    /// A panel owns the canvas clicks this frame (the mask panel is open, or the eyedropper is
    /// active): no finger selection change, no layer menu and no new finger move starts (and no
    /// drag region is published). A running move still ends.
    pub(super) panel_blocks: bool,
    /// The panel's whole-pixel movement policy, applied to a finger move as to a mouse move.
    pub(super) strict_pixel_movement: bool,
}

/// What the finger driver tells the hook.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct TypingTouchOutcome {
    /// Selection, geometry or a layer-menu request changed: request a repaint.
    pub(super) changed: bool,
}

/// A finger long-press on a text layer: while armed, the candidate whose lift opens the layer's
/// context menu; after the lift, the request `draw_page.rs` opens (see the file notes).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct TypingTouchLayerMenu {
    /// The text layer under the press (`touch_overlay_at`).
    pub(super) overlay_idx: usize,
    /// Its page.
    pub(super) page_idx: usize,
    /// The press point (screen points): the menu anchor.
    pub(super) at: Pos2,
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
    /// [`canvas_page_view_at`] /
    /// [`canvas_page_view`]). Order: last frame's layer-menu request is dropped (its page pass has
    /// run); a running finger move is driven or ended; a region drag that began this frame on the
    /// selected layer opens a move; a tap selects / deselects; a long-press lift turns last frame's
    /// menu candidate into this frame's request; last, the armed point decides the next candidate.
    pub(super) fn drive_touch_layer_input(
        &mut self,
        frame: TypingTouchFrame,
        gates: TypingTouchGates,
        view_at: &dyn Fn(Pos2) -> Option<PageView>,
        view_of: &dyn Fn(usize) -> Option<PageView>,
    ) -> TypingTouchOutcome {
        let mut outcome = TypingTouchOutcome::default();
        let may_start = !gates.panel_blocks && self.layout_editor.is_none();
        // The request was for the page pass that ran before this hook: it opened there or its
        // layer was not drawn; either way it is spent.
        self.touch_layer_menu_request = None;
        if let Some(active) = self.touch_layer_move.take() {
            outcome.changed = true;
            match frame.region_drag {
                Some(drag) => self.step_touch_layer_move(active, drag, gates.strict_pixel_movement, view_of),
                None => {
                    // The router reports a running region drag every frame until its end, so a
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
        } else if let Some(drag) = frame.region_drag
            && drag.began
        {
            // The router tested the press against LAST frame's region; re-check the target now. A
            // refused drag (the selection or a gate changed in between) moves nothing until the lift.
            let target = if may_start {
                view_at(drag.origin).and_then(|view| self.touch_move_target_at(view, drag.origin).map(|overlay_idx| (overlay_idx, view)))
            } else {
                None
            };
            match target {
                Some((overlay_idx, view)) => {
                    outcome.changed = true;
                    self.begin_touch_layer_move(overlay_idx, view, drag, gates.strict_pixel_movement, view_of);
                }
                None => ms_log::trace_log!(cat::INPUT, "touch_layer_move_refused origin={:?}", drag.origin),
            }
        }

        // A tap while the create editor is open only commits it (`draw_text_editor`).
        if let Some(tap) = frame.tap
            && may_start
            && self.create_editor.is_none()
            && self.touch_layer_move.is_none()
        {
            outcome.changed |= self.apply_touch_tap(tap, view_at);
        }

        // The lift of the press the candidate was recorded for: the page menu was suppressed for
        // it this frame, so the layer menu must open (see the file notes).
        let candidate = self.touch_layer_menu_candidate.take();
        if let Some(at) = frame.long_press_at
            && let Some(menu) = candidate.filter(|menu| menu.at == at)
        {
            ms_log::trace_log!(cat::TYPING, "touch_layer_menu overlay_idx={} page={}", menu.overlay_idx, menu.page_idx);
            self.touch_layer_menu_request = Some(menu);
            outcome.changed = true;
        }
        // The tap's gates: a menu never opens over the create editor or under a running move.
        self.touch_layer_menu_candidate = if may_start && self.create_editor.is_none() && self.touch_layer_move.is_none() {
            frame.armed_at.and_then(|at| {
                let view = view_at(at)?;
                let overlay_idx = self.touch_overlay_at(view, at)?;
                Some(TypingTouchLayerMenu { overlay_idx, page_idx: view.page_idx, at })
            })
        } else {
            None
        };
        outcome
    }

    /// Whether the canvas page menu must stay shut on `page_idx` this frame: an armed finger
    /// long-press there sits on a text layer, so its lift opens that layer's menu instead.
    pub(super) fn suppresses_canvas_page_menu_for_touch(&self, page_idx: usize) -> bool {
        self.touch_layer_menu_candidate.is_some_and(|menu| menu.page_idx == page_idx)
    }

    /// The anchor of the finger layer-menu request for `overlay_idx` on `page_idx`, taken (it opens
    /// once). The vector-transform menu (`vector_transform.rs`) calls it directly: its right-click
    /// has no side effects either.
    pub(super) fn take_touch_layer_menu_at(&mut self, overlay_idx: usize, page_idx: usize) -> Option<Pos2> {
        let menu = self.touch_layer_menu_request.filter(|menu| menu.overlay_idx == overlay_idx && menu.page_idx == page_idx)?;
        self.touch_layer_menu_request = None;
        Some(menu.at)
    }

    /// Takes the finger layer-menu request of `overlay_idx` on `page_idx` and applies what a mouse
    /// right-click on the layer does before its menu opens (`draw_page.rs`): the layer becomes the
    /// selected overlay and its shape-variant preview starts at the press point. Returns the menu
    /// anchor to pass to `context_menu_popup`, `None` when no request is for this layer.
    pub(super) fn open_touch_layer_menu(&mut self, ctx: &egui::Context, overlay_idx: usize, page_idx: usize) -> Option<Pos2> {
        let at = self.take_touch_layer_menu_at(overlay_idx, page_idx)?;
        self.selected_overlay_idx = Some(overlay_idx);
        self.start_shape_variant_preview_if_available(ctx, overlay_idx, at);
        Some(at)
    }

    /// The grab-area outline (screen points) of the selected text layer when a finger may move it —
    /// the canvas' touch drag region for the next frame, so a finger pressed there drags it without
    /// a hold. `None` while a gate blocks finger moves (`TypingTouchGates::panel_blocks`, the layout
    /// editor), in a transform mode, or when the layer is not drawn. Same rule as
    /// [`Self::touch_move_target_at`], which re-checks the press when the drag begins.
    pub(super) fn touch_drag_region(&self, gates: TypingTouchGates, view_of: &dyn Fn(usize) -> Option<PageView>) -> Option<Vec<Pos2>> {
        if gates.panel_blocks || self.layout_editor.is_some() {
            return None;
        }
        let selected = self.selected_overlay_idx?;
        if self.transform_mode_overlay_idx == Some(selected) {
            return None;
        }
        let overlay = self.overlays.get(selected)?;
        let view = view_of(overlay.page_idx)?;
        if !self.touch_overlay_candidates(view).contains(&selected) {
            return None;
        }
        let geometry = overlay_scene_geometry(overlay, view);
        let grab = overlay_grab_mesh_scene(&geometry.mesh_scene, geometry.mesh_cols, geometry.mesh_rows, overlay.kind == TypingOverlayKind::Text);
        let outline = mesh_boundary_path(&grab, geometry.mesh_cols, geometry.mesh_rows);
        (!outline.is_empty()).then_some(outline)
    }

    /// Opens the finger move of `overlay_idx` (the selected layer under `selection.origin`, the
    /// region drag's press point) and applies the drag's first frame.
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

    /// One frame of a finger move (`selection` is the region drag): follow the finger, or end the
    /// gesture on its last frame —
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
        let mut overlay = text_runtime_from_doc_node(uid, 0, OverlayPlacement::at(center), false, None, [20, 20], vec![255; 20 * 20 * 4]);
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

    /// A region drag (the router's drag of a finger pressed in the published drag region).
    fn drag(origin: Pos2, pos: Pos2, began: bool, end: Option<TouchSelectionEnd>) -> TypingTouchFrame {
        TypingTouchFrame { region_drag: Some(TouchSelection { origin, pos, began, end }), ..TypingTouchFrame::default() }
    }

    fn armed(at: Pos2) -> TypingTouchFrame {
        TypingTouchFrame { armed_at: Some(at), ..TypingTouchFrame::default() }
    }

    fn lifted(at: Pos2) -> TypingTouchFrame {
        TypingTouchFrame { long_press_at: Some(at), ..TypingTouchFrame::default() }
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

    /// Whether `outline` (a drag region) contains `point`, by the router's own even-odd rule
    /// restated for the test (crossings of the ray to +x).
    fn outline_contains(outline: &[Pos2], point: Pos2) -> bool {
        let mut inside = false;
        for (index, next) in outline.iter().enumerate() {
            let prev = outline[(index + outline.len() - 1) % outline.len()];
            if (next.y > point.y) != (prev.y > point.y) && point.x < next.x + (point.y - next.y) * (prev.x - next.x) / (prev.y - next.y) {
                inside = !inside;
            }
        }
        inside
    }

    #[test]
    fn drag_region_is_the_grab_area_of_the_selected_layer_only() {
        let ctx = egui::Context::default();
        let mut layer = two_layers(&ctx);
        assert_eq!(layer.touch_drag_region(gates(), &view_of), None, "no selection, no region");
        layer.selected_overlay_idx = Some(0);
        let outline = layer.touch_drag_region(gates(), &view_of).unwrap_or_default();
        assert!(outline_contains(&outline, Pos2::new(30.0, 30.0)), "the selected layer is a region: {outline:?}");
        assert!(!outline_contains(&outline, Pos2::new(70.0, 70.0)), "an unselected layer is not");
        assert!(!outline_contains(&outline, Pos2::new(5.0, 90.0)), "bare page is not");
        let blocked = TypingTouchGates { panel_blocks: true, strict_pixel_movement: false };
        assert_eq!(layer.touch_drag_region(blocked, &view_of), None, "a panel owning the clicks publishes none");
        layer.transform_mode_overlay_idx = Some(0);
        assert_eq!(layer.touch_drag_region(gates(), &view_of), None, "no finger move in a transform mode");
        layer.transform_mode_overlay_idx = None;
        assert_eq!(layer.touch_drag_region(gates(), &|_| None), None, "a page not drawn publishes none");
    }

    #[test]
    fn press_drag_on_the_selected_layer_moves_it_and_the_lift_settles_once() {
        let ctx = egui::Context::default();
        let mut layer = two_layers(&ctx);
        layer.selected_overlay_idx = Some(0);
        let origin = Pos2::new(30.0, 30.0);
        let outcome = drive(&mut layer, drag(origin, Pos2::new(40.0, 35.0), true, None));
        assert!(outcome.changed);
        assert_eq!(layer.overlays[0].center_page_px, [40.0, 35.0]);
        assert!(layer.move_session.as_ref().is_some_and(|session| session.source == TypingLayerMoveSource::Touch));
        // A frame without a pointer button never settles a finger move (a claimed finger never
        // presses), so the gesture stays one session. (No bindings: a finger session ignores keys.)
        let router = ms_input::InputRouter::new(ms_input::InputRegistry::empty());
        layer.drive_layer_move_settle(&ctx, router.frame());
        assert!(layer.move_session.is_some());
        assert!(!layer.has_pending_placement_save(), "nothing is written mid-gesture");
        drive(&mut layer, drag(origin, Pos2::new(50.0, 45.0), false, None));
        assert_eq!(layer.overlays[0].center_page_px, [50.0, 45.0], "the total delta is from the press point");
        drive(&mut layer, drag(origin, Pos2::new(55.0, 45.0), false, Some(TouchSelectionEnd::Released)));
        assert_eq!(layer.overlays[0].center_page_px, [55.0, 45.0]);
        assert!(layer.move_session.is_none() && layer.touch_layer_move.is_none(), "the lift ends the gesture");
        assert!(layer.has_pending_placement_save(), "one deferred write for the whole gesture");
        assert_eq!(layer.overlays[1].center_page_px, [70.0, 70.0], "the other layer never moves");
    }

    #[test]
    fn region_drag_off_the_selected_layer_moves_nothing() {
        // The region was last frame's; the selection changed since, so the press is no longer on it.
        let ctx = egui::Context::default();
        let mut layer = two_layers(&ctx);
        layer.selected_overlay_idx = Some(0);
        drive(&mut layer, drag(Pos2::new(70.0, 70.0), Pos2::new(90.0, 90.0), true, None));
        drive(&mut layer, drag(Pos2::new(70.0, 70.0), Pos2::new(95.0, 95.0), false, None));
        assert!(layer.move_session.is_none() && layer.touch_layer_move.is_none());
        assert_eq!(layer.overlays[0].center_page_px, [30.0, 30.0]);
        assert_eq!(layer.overlays[1].center_page_px, [70.0, 70.0]);
    }

    #[test]
    fn long_press_lift_on_a_layer_opens_its_menu_and_selects_it_like_a_right_click() {
        let ctx = egui::Context::default();
        let mut layer = two_layers(&ctx);
        layer.selected_overlay_idx = Some(0);
        let at = Pos2::new(70.0, 70.0);
        drive(&mut layer, armed(at));
        assert!(layer.suppresses_canvas_page_menu_for_touch(0), "an armed hold on a layer keeps the page menu shut");
        assert!(!layer.suppresses_canvas_page_menu_for_touch(1), "only on its own page");
        assert_eq!(layer.selected_overlay_idx, Some(0), "an armed hold never changes the selection");
        // Lift frame: the canvas consults the hook BEFORE the driver runs, so the page menu is
        // still suppressed when the router reports the lift.
        assert!(layer.suppresses_canvas_page_menu_for_touch(0));
        let outcome = drive(&mut layer, lifted(at));
        assert!(outcome.changed, "the menu opens on the next frame: repaint");
        assert!(!layer.suppresses_canvas_page_menu_for_touch(0), "the next frame's page menu is free again");
        assert_eq!(layer.open_touch_layer_menu(&ctx, 0, 0), None, "the request is for the layer under the press");
        assert_eq!(layer.open_touch_layer_menu(&ctx, 1, 0), Some(at), "anchored at the press point");
        assert_eq!(layer.selected_overlay_idx, Some(1), "selected like a right-click");
        assert_eq!(layer.open_touch_layer_menu(&ctx, 1, 0), None, "it opens once");
    }

    #[test]
    fn long_press_lift_off_every_layer_leaves_the_page_menu() {
        let ctx = egui::Context::default();
        let mut layer = two_layers(&ctx);
        let at = Pos2::new(5.0, 90.0);
        drive(&mut layer, armed(at));
        assert!(!layer.suppresses_canvas_page_menu_for_touch(0), "bare page: the canvas menu opens");
        drive(&mut layer, lifted(at));
        assert!(!layer.suppresses_canvas_page_menu_for_touch(0));
        assert_eq!((layer.open_touch_layer_menu(&ctx, 0, 0), layer.open_touch_layer_menu(&ctx, 1, 0)), (None, None), "no layer menu");
    }

    #[test]
    fn exactly_one_menu_per_long_press() {
        let ctx = egui::Context::default();
        let at = Pos2::new(30.0, 30.0);
        // A lift the arm was not seen for (no candidate): the page menu ran unsuppressed, so no
        // layer menu may open on top of it.
        let mut layer = two_layers(&ctx);
        drive(&mut layer, lifted(at));
        assert_eq!(layer.open_touch_layer_menu(&ctx, 0, 0), None, "never both");
        // Gates closed while armed (a panel owns the clicks): no candidate, so the page menu opens.
        let mut layer = two_layers(&ctx);
        let blocked = TypingTouchGates { panel_blocks: true, strict_pixel_movement: false };
        layer.drive_touch_layer_input(armed(at), blocked, &view_at, &view_of);
        assert!(!layer.suppresses_canvas_page_menu_for_touch(0));
        // A request not opened by the next page pass is dropped by the next driver run.
        let mut layer = two_layers(&ctx);
        drive(&mut layer, armed(at));
        drive(&mut layer, lifted(at));
        drive(&mut layer, TypingTouchFrame::default());
        assert_eq!(layer.open_touch_layer_menu(&ctx, 0, 0), None, "a stale request never opens later");
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
