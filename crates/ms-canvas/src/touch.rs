/*
File: crates/ms-canvas/src/touch.rs

Purpose:
The canvas side of finger gestures. `ms_widgets::touch_nav` (driven by the shell's raw-input
hook) decides which touches belong to this canvas and turns them into typed output; this file
publishes the canvas surface to it and applies what comes back: pan / pinch through the same
view anchor the wheel zoom uses, and the long-press point that opens the page context menu.

Main responsibilities:
- `CanvasView::touch_owner_id`: the content-named owner id of this canvas;
- publish the `TouchSurface` (scroll viewport minus the holes where egui keeps the touch:
  on-top bubbles, aside cards, scrollbars, the bottom hint) once per frame;
- take the frame's `TouchOutput` and apply its navigation (zoom about the previous centroid,
  then move it onto the new one; pan only while the tab blocks zoom);
- hand the long-press selection drag to the tab: the tab declares per frame whether its
  active mode has a selection (`set_long_press_selection`), reads this frame's drag
  (`touch_selection`) and drives its own selection state with it (`drive_touch_drag` for
  rectangle-drag capture modes);
- draw the armed long-press feedback marker.

Key functions:
- `CanvasView::touch_owner_id`
- `CanvasView::take_touch_output_for_frame` / `apply_touch_navigation`
- `CanvasView::publish_touch_surface` / `draw_touch_long_press_feedback`
- `CanvasView::set_long_press_selection` / `touch_selection`
- `drive_touch_drag` + `TouchDrivenDrag`: the one mapping of a `TouchSelection` onto a
  rectangle-drag capture mode (begin at the press point, follow, commit on release, drop on
  cancel).
- `context_menu_open_pos`: the one rule for "where does the page menu open this frame".

Notes:
The page context menu itself stays in `scene.rs::handle_canvas_page_interactions`; a long-press
reaches it through `context_menu_open_pos` and the same target code as a secondary click.
The canvas itself has no selection target: every selection drag lives in a tab crate (Translation
OCR / crop capture areas, Typing create rectangle, Cleaning region tools). A tab whose active
mode has one sets `long_press_selection` for the frame and maps `touch_selection` onto that mode;
otherwise the flag stays `false` and a drag after an armed long-press pans. See `MODULE_README.md`
"Touch gestures".
*/

use super::{CANVAS_ZOOM_MAX, CANVAS_ZOOM_MIN, CanvasView};
use eframe::egui;
use egui::{Id, Pos2, Rect, Stroke};
use ms_widgets::touch_nav::{self, TouchNavigation, TouchSelection, TouchSelectionEnd, TouchSurface};

/// Radius (screen points) of the armed long-press feedback ring: about a fingertip, so the
/// marker stays visible around the finger that covers its center.
const LONG_PRESS_FEEDBACK_RADIUS_PT: f32 = 28.0;

/// Builds the touch owner id of a canvas whose scroll area uses `scroll_area_id_salt`.
///
/// Content-named (never an instance counter): every `MangaApp` rebuild in the same
/// `egui::Context` gets the same id for the same tab canvas, and canvases of different tabs
/// differ because their scroll salts differ.
#[must_use]
fn touch_owner_id_for_salt(scroll_area_id_salt: &'static str) -> Id {
    Id::new(("ms_canvas.touch_surface", scroll_area_id_salt))
}

/// Where the page context menu of one page row opens this frame, or `None`.
///
/// `secondary_click_pos` is the interact position of a secondary click on the row (mouse or
/// pen barrel, or egui's own long-touch on an unclaimed touch); `long_press_at` is the router's
/// finger long-press point. The long-press counts only inside `row_rect`, the row's response
/// rect — the same area a secondary click must land in to reach the row. Both are resolved
/// into a target by the same code afterwards.
#[must_use]
pub(super) fn context_menu_open_pos(
    secondary_click_pos: Option<Pos2>,
    long_press_at: Option<Pos2>,
    row_rect: Rect,
) -> Option<Pos2> {
    secondary_click_pos.or_else(|| long_press_at.filter(|pos| row_rect.contains(*pos)))
}

/// An in-flight rectangle drag of a tab's capture mode that a finger can drive.
///
/// Implemented by the mode's drag state (start point plus current corner). A drag started by
/// the MOUSE never reports `is_touch_driven`, so [`drive_touch_drag`] leaves it alone and the
/// mode's pointer path keeps owning it.
pub trait TouchDrivenDrag {
    /// Whether this drag was started by a finger long-press ([`drive_touch_drag`]'s `begin`).
    fn is_touch_driven(&self) -> bool;
    /// Moves the drag's current corner to `pos` (screen points).
    fn set_current(&mut self, pos: Pos2);
}

/// Applies this frame's long-press `selection` to a rectangle-drag capture mode's `slot` and
/// returns the drag to COMMIT, if the finger was lifted this frame.
///
/// The one mapping every rectangle-drag mode shares, so the finger behaves the same in all of
/// them:
/// - `began` with an empty slot: `begin(origin)` builds the touch-driven drag at the press
///   point, or refuses (`None`, e.g. the press is not on a page) — then the rest of this
///   gesture does nothing. A slot already holding a mouse drag is never replaced.
/// - every frame: a touch-driven drag follows the finger (`set_current(pos)`).
/// - `end == Released`: the touch-driven drag is taken out of the slot and returned; the
///   caller commits it exactly as its mouse release would.
/// - `end == Cancelled`: the touch-driven drag is dropped (like Esc), nothing is committed.
///
/// Call it BEFORE the mode's pointer path in the frame, so a mouse drag that starts on the
/// frame the router cancels the finger (a press interrupts a selection) is not dropped with it.
pub fn drive_touch_drag<S: TouchDrivenDrag>(
    slot: &mut Option<S>,
    selection: TouchSelection,
    begin: impl FnOnce(Pos2) -> Option<S>,
) -> Option<S> {
    if selection.began && slot.is_none() {
        *slot = begin(selection.origin);
    }
    let drag = slot.as_mut().filter(|drag| drag.is_touch_driven())?;
    drag.set_current(selection.pos);
    match selection.end {
        None => None,
        Some(TouchSelectionEnd::Released) => slot.take(),
        Some(TouchSelectionEnd::Cancelled) => {
            *slot = None;
            None
        }
    }
}

impl CanvasView {
    /// Declares, for THIS frame, whether the active tab mode has a selection a finger
    /// long-press drag drives (`TouchSurface::long_press_selection`). With `false` (the default
    /// every frame the tab does not set it) a drag after an armed long-press pans instead.
    ///
    /// A one-frame flag, unlike `set_zoom_blocked`: the surface publication at the end of
    /// `draw` resets it, so a tab that stops setting it can never leave a stale `true`. Call it
    /// before `draw`, or from a hook that runs inside `draw` with `&mut CanvasView`. The router
    /// copies the flag when a finger is claimed, so it decides the gesture of a touch that
    /// starts on the NEXT frame.
    pub fn set_long_press_selection(&mut self, enabled: bool) {
        self.scene.long_press_selection = enabled;
    }

    /// This frame's long-press selection drag on this canvas, if a finger is performing one.
    ///
    /// Valid from `draw`'s input step on (hooks inside `draw` and code after it read the
    /// current frame); reported on every frame while the drag lasts, with `began` on its first
    /// frame and `end` on its last. Only reported while the tab declared a selection
    /// ([`CanvasView::set_long_press_selection`]) when the finger was claimed.
    #[must_use]
    pub fn touch_selection(&self) -> Option<TouchSelection> {
        self.scene.touch_selection
    }

    /// Stable id under which this canvas publishes its touch surface and takes its touch
    /// output (`ms_widgets::touch_nav`). Tabs pass it to `touch_nav::publish_capture_layer` for
    /// their capture overlays, so a finger pans under them too.
    #[must_use]
    pub fn touch_owner_id(&self) -> Id {
        touch_owner_id_for_salt(self.scroll_area_id_salt)
    }

    /// Takes this frame's touch output (once per frame; it also schedules the router's
    /// long-press repaint), applies its navigation and keeps the long-press points for the
    /// scene pass. Called from `handle_shortcuts` after the wheel and keyboard zoom paths.
    pub(super) fn take_touch_output_for_frame(&mut self, ctx: &egui::Context, canvas_rect: Rect) {
        let output = touch_nav::take_touch_output(ctx, self.touch_owner_id());
        self.scene.touch_long_press_at = output.long_press_at;
        self.scene.touch_long_press_armed_at = output.long_press_armed_at;
        self.scene.touch_selection = output.selection;
        if let Some(navigation) = output.navigation
            && self.apply_touch_navigation(navigation, canvas_rect)
        {
            ctx.request_repaint();
        }
    }

    /// Applies one frame of finger navigation; returns whether the view will move.
    ///
    /// Zoom is multiplied by `zoom_factor` (clamped to the canvas zoom range) unless the active
    /// tab blocks zoom, in which case only the pan applies. The anchor is captured at the OLD
    /// zoom, then the zoom changes, so the scene pass places the world point that was under
    /// `prev_centroid` under `centroid`. A zoom or view request already recorded this frame (a
    /// Ctrl+wheel or keyboard zoom, a public zoom setter) wins: its anchor was captured against
    /// the same scroll offset and must not be overwritten with a mismatched one, so this frame's
    /// finger movement is dropped.
    pub(super) fn apply_touch_navigation(
        &mut self,
        navigation: TouchNavigation,
        fallback_rect: Rect,
    ) -> bool {
        if self.scene.pending_zoom_anchor.is_some() {
            return false;
        }
        let old_zoom = self.state.zoom;
        let new_zoom = if self.scene.zoom_blocked {
            old_zoom
        } else {
            (old_zoom * navigation.zoom_factor).clamp(CANVAS_ZOOM_MIN, CANVAS_ZOOM_MAX)
        };
        let zoom_changes = (new_zoom - old_zoom).abs() > f32::EPSILON;
        if !zoom_changes && navigation.pan_delta().length_sq() <= f32::EPSILON {
            return false;
        }
        self.capture_pending_view_anchor(
            navigation.prev_centroid,
            navigation.centroid,
            fallback_rect,
        );
        if zoom_changes {
            self.state.zoom = new_zoom;
        }
        self.scene.pending_zoom_anchor.is_some()
    }

    /// Publishes this canvas' touch surface for the router's next frame.
    ///
    /// Called at the end of `draw`, after every per-frame rect it uses is final: the scroll
    /// viewport (`scroll_inner_rect`), the on-top bubble and aside card hit rects, the
    /// scrollbars and the bottom hint. Floating windows and dock panels are not holes: they sit
    /// on non-background layers, which the router never claims on.
    pub(super) fn publish_touch_surface(&mut self, ctx: &egui::Context) {
        if let Some(surface) = self.take_touch_surface() {
            touch_nav::publish_surface(ctx, surface);
        }
    }

    /// Builds this frame's touch surface (`None` before the first scene pass) and consumes the
    /// one-frame `long_press_selection` flag whether or not a surface results.
    fn take_touch_surface(&mut self) -> Option<TouchSurface> {
        let long_press_selection = std::mem::take(&mut self.scene.long_press_selection);
        let rect = self.scene.scroll_inner_rect?;
        Some(TouchSurface {
            owner: self.touch_owner_id(),
            rect,
            holes: self.touch_surface_holes(),
            long_press_selection,
        })
    }

    /// Records the hit rect of one small bubble handle drawn this frame as a touch hole,
    /// grown by egui's interact radius — the slack egui's own hit test gives a pointer around
    /// the widget (read from the global style, as that hit test does,
    /// `egui-0.36.2/src/context.rs:486`) — so every touch that would hit the handle stays
    /// with egui.
    pub(super) fn note_touch_handle_hole(&mut self, ui: &egui::Ui, hit_rect: Rect) {
        let radius = ui.ctx().global_style().interaction.interact_radius;
        self.scene.handle_hit_rects.push(hit_rect.expand(radius));
    }

    /// Rects inside the surface where a finger stays with egui (bubble widgets, scrollbars,
    /// the bottom hint), in screen points of this frame.
    fn touch_surface_holes(&self) -> Vec<Rect> {
        self.scene
            .on_top_hit_rects
            .values()
            .copied()
            .chain(self.scene.aside_card_hit_rects.iter().copied())
            .chain(self.scene.handle_hit_rects.iter().copied())
            .chain(self.scene.scroll_vertical_bar_rect)
            .chain(self.scene.scroll_horizontal_bar_rect)
            .chain(self.scene.canvas_bottom_hint_rect)
            .collect()
    }

    /// Draws the armed long-press marker (a ring around the press point) while the finger is
    /// still held, so the user sees that lifting now opens the menu.
    pub(super) fn draw_touch_long_press_feedback(&self, ui: &egui::Ui) {
        let Some(point) = self.scene.touch_long_press_armed_at else {
            return;
        };
        let clip = self.scene.scroll_inner_rect.unwrap_or_else(|| ui.max_rect());
        let painter = ui.painter_at(clip);
        painter.circle_filled(
            point,
            LONG_PRESS_FEEDBACK_RADIUS_PT,
            ms_theme::canvas::SELECTION.gamma_multiply(0.25),
        );
        painter.circle_stroke(
            point,
            LONG_PRESS_FEEDBACK_RADIUS_PT,
            Stroke::new(2.0, ms_theme::canvas::SELECTION),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Vec2, pos2, vec2};

    /// A canvas with the scene state the pure navigation helpers read (no live `Ui`): one page
    /// row filling the content width, last frame's viewport and scroll offset.
    fn view_with(zoom: f32, offset: Vec2, viewport: Rect) -> CanvasView {
        let mut view = CanvasView::default();
        view.state.zoom = zoom;
        view.scene.content_world_width = 4000.0;
        view.scene.max_page_world_width = 4000.0;
        view.scene.scroll_inner_rect = Some(viewport);
        view.scene.scroll_offset = offset;
        view
    }

    fn viewport() -> Rect {
        Rect::from_min_size(pos2(37.0, 64.0), vec2(900.0, 700.0))
    }

    /// The scroll offset the next scene pass requests for the recorded anchor.
    fn requested_offset(view: &CanvasView) -> Vec2 {
        let anchor = view.scene.pending_zoom_anchor.expect("navigation must record an anchor");
        view.scroll_offset_for_zoom_anchor(anchor)
    }

    /// World point under `screen` for the view's zoom and `offset` (the capture formula with
    /// that offset), used to check fixed points.
    fn world_under(view: &CanvasView, offset: Vec2, screen: Pos2) -> Vec2 {
        let mut probe = view_with(view.state.zoom, offset, viewport());
        probe.capture_pending_view_anchor(screen, screen, viewport());
        probe.scene.pending_zoom_anchor.expect("positive viewport").world_focus
    }

    fn navigation(prev: Pos2, now: Pos2, zoom_factor: f32) -> TouchNavigation {
        TouchNavigation { prev_centroid: prev, centroid: now, zoom_factor, touches: 1 }
    }

    #[test]
    fn view_anchor_with_equal_points_equals_the_zoom_anchor() {
        for anchor in [pos2(400.0, 300.0), pos2(-50.0, 2000.0), pos2(900.0, 70.0)] {
            let mut zoom_view = view_with(2.5, vec2(12000.0, 1200.0), viewport());
            zoom_view.capture_pending_zoom_anchor(Some(anchor), viewport());
            let mut pan_view = view_with(2.5, vec2(12000.0, 1200.0), viewport());
            pan_view.capture_pending_view_anchor(anchor, anchor, viewport());
            let zoom_anchor = zoom_view.scene.pending_zoom_anchor.expect("zoom anchor");
            let pan_anchor = pan_view.scene.pending_zoom_anchor.expect("view anchor");
            assert_eq!(zoom_anchor.viewport_local, pan_anchor.viewport_local);
            assert_eq!(zoom_anchor.world_focus, pan_anchor.world_focus);
        }
    }

    #[test]
    fn a_pan_of_d_moves_the_scroll_offset_by_minus_d() {
        let start = vec2(12000.0, 1200.0);
        let mut view = view_with(2.5, start, viewport());
        let d = vec2(35.0, -20.0);
        let prev = pos2(400.0, 300.0);
        assert!(view.apply_touch_navigation(navigation(prev, prev + d, 1.0), viewport()));
        assert!((view.state.zoom - 2.5).abs() <= f32::EPSILON, "a pan never zooms");
        let offset = requested_offset(&view);
        assert!((offset.x - (start.x - d.x)).abs() <= 1e-2, "x {offset:?}");
        assert!((offset.y - (start.y - d.y)).abs() <= 1e-2, "y {offset:?}");
    }

    #[test]
    fn a_pan_that_leaves_the_viewport_keeps_its_full_delta() {
        let start = vec2(12000.0, 1200.0);
        let mut view = view_with(2.5, start, viewport());
        // The finger ends above the viewport: the clamp applies to the press side only.
        let prev = pos2(400.0, 80.0);
        let now = pos2(400.0, 20.0);
        assert!(view.apply_touch_navigation(navigation(prev, now, 1.0), viewport()));
        let offset = requested_offset(&view);
        assert!((offset.y - (start.y + 60.0)).abs() <= 1e-2, "y {offset:?}");
    }

    #[test]
    fn a_pinch_keeps_the_world_point_under_the_moving_centroid() {
        let start = vec2(12000.0, 1200.0);
        let mut view = view_with(2.0, start, viewport());
        let prev = pos2(500.0, 400.0);
        let now = pos2(520.0, 380.0);
        let world_before = world_under(&view, start, prev);
        assert!(view.apply_touch_navigation(navigation(prev, now, 1.25), viewport()));
        assert!((view.state.zoom - 2.5).abs() <= 1e-5, "zoom {}", view.state.zoom);
        let offset = requested_offset(&view);
        let world_after = world_under(&view, offset, now);
        assert!((world_after - world_before).length() <= 1e-2, "{world_before:?} -> {world_after:?}");
    }

    #[test]
    fn a_pinch_is_clamped_to_the_canvas_zoom_range() {
        let mut view = view_with(4.8, vec2(30000.0, 1200.0), viewport());
        let prev = pos2(500.0, 400.0);
        assert!(view.apply_touch_navigation(navigation(prev, prev, 2.0), viewport()));
        assert!((view.state.zoom - CANVAS_ZOOM_MAX).abs() <= f32::EPSILON);
    }

    #[test]
    fn zoom_blocked_turns_a_pinch_into_a_pan() {
        let start = vec2(12000.0, 1200.0);
        let mut view = view_with(2.5, start, viewport());
        view.scene.zoom_blocked = true;
        let prev = pos2(500.0, 400.0);
        let d = vec2(-10.0, 25.0);
        assert!(view.apply_touch_navigation(navigation(prev, prev + d, 1.5), viewport()));
        assert!((view.state.zoom - 2.5).abs() <= f32::EPSILON, "zoom must stay blocked");
        let offset = requested_offset(&view);
        assert!((offset - (start - d)).length() <= 1e-2, "{offset:?}");
    }

    #[test]
    fn a_still_frame_records_nothing() {
        let mut view = view_with(2.5, vec2(12000.0, 1200.0), viewport());
        let prev = pos2(500.0, 400.0);
        assert!(!view.apply_touch_navigation(navigation(prev, prev, 1.0), viewport()));
        assert!(view.scene.pending_zoom_anchor.is_none());
    }

    #[test]
    fn a_zoom_request_of_this_frame_wins_over_finger_navigation() {
        let mut view = view_with(2.5, vec2(12000.0, 1200.0), viewport());
        view.capture_pending_zoom_anchor(Some(pos2(100.0, 100.0)), viewport());
        let recorded = view.scene.pending_zoom_anchor.expect("zoom anchor");
        let prev = pos2(500.0, 400.0);
        assert!(!view.apply_touch_navigation(navigation(prev, prev + vec2(40.0, 0.0), 1.2), viewport()));
        let kept = view.scene.pending_zoom_anchor.expect("zoom anchor kept");
        assert_eq!(kept.viewport_local, recorded.viewport_local);
        assert!((view.state.zoom - 2.5).abs() <= f32::EPSILON);
    }

    #[test]
    fn the_long_press_opens_where_a_secondary_click_at_the_same_point_would() {
        let row = Rect::from_min_size(pos2(0.0, 100.0), vec2(1000.0, 1500.0));
        let image = Rect::from_min_size(pos2(200.0, 100.0), vec2(600.0, 1500.0));
        let frame = crate::types::CanvasScenePageFrame {
            page_idx: 3,
            row_rect: row,
            image_rect: image,
            page_in_view: true,
        };
        let view = CanvasView::default();
        for point in [pos2(450.0, 700.0), pos2(201.0, 101.0), pos2(100.0, 700.0)] {
            let by_click = context_menu_open_pos(Some(point), None, row)
                .and_then(|pos| view.canvas_context_menu_target_at(frame, pos));
            let by_long_press = context_menu_open_pos(None, Some(point), row)
                .and_then(|pos| view.canvas_context_menu_target_at(frame, pos));
            assert_eq!(by_click, by_long_press, "point {point:?}");
        }
        let target = context_menu_open_pos(None, Some(pos2(450.0, 700.0)), row)
            .and_then(|pos| view.canvas_context_menu_target_at(frame, pos))
            .expect("bare page point resolves to a target");
        assert_eq!(target.page_idx, 3);
        assert!((target.page_uv.x - 250.0 / 600.0).abs() <= 1e-6, "{target:?}");
        assert!((target.page_uv.y - 600.0 / 1500.0).abs() <= 1e-6, "{target:?}");
    }

    #[test]
    fn a_long_press_outside_the_row_does_not_open_its_menu() {
        let row = Rect::from_min_size(pos2(0.0, 100.0), vec2(1000.0, 1500.0));
        assert_eq!(context_menu_open_pos(None, Some(pos2(500.0, 50.0)), row), None);
        // A secondary click wins over a long-press of the same frame.
        let click = pos2(10.0, 200.0);
        assert_eq!(context_menu_open_pos(Some(click), Some(pos2(500.0, 500.0)), row), Some(click));
    }

    #[test]
    fn owner_ids_are_content_named_per_scroll_salt() {
        let mut a = CanvasView::default();
        let b = CanvasView::default();
        assert_eq!(a.touch_owner_id(), b.touch_owner_id());
        a.set_scroll_area_id_salt("typing_canvas_scroll");
        assert_ne!(a.touch_owner_id(), b.touch_owner_id());
    }

    /// A rectangle drag of a capture mode, as the tab crates model theirs.
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Drag {
        start: Pos2,
        current: Pos2,
        touch: bool,
    }

    impl TouchDrivenDrag for Drag {
        fn is_touch_driven(&self) -> bool {
            self.touch
        }

        fn set_current(&mut self, pos: Pos2) {
            self.current = pos;
        }
    }

    fn sel(origin: Pos2, pos: Pos2, began: bool, end: Option<TouchSelectionEnd>) -> TouchSelection {
        TouchSelection { origin, pos, began, end }
    }

    fn touch_drag_at(origin: Pos2) -> Option<Drag> {
        Some(Drag { start: origin, current: origin, touch: true })
    }

    #[test]
    fn a_touch_drag_begins_at_the_press_point_follows_and_commits_on_release() {
        let origin = pos2(10.0, 20.0);
        let mut slot = None;
        assert_eq!(drive_touch_drag(&mut slot, sel(origin, pos2(30.0, 40.0), true, None), touch_drag_at), None);
        assert_eq!(slot, Some(Drag { start: origin, current: pos2(30.0, 40.0), touch: true }));
        assert_eq!(drive_touch_drag(&mut slot, sel(origin, pos2(50.0, 60.0), false, None), touch_drag_at), None);
        let committed = drive_touch_drag(&mut slot, sel(origin, pos2(70.0, 80.0), false, Some(TouchSelectionEnd::Released)), touch_drag_at);
        assert_eq!(committed, Some(Drag { start: origin, current: pos2(70.0, 80.0), touch: true }));
        assert_eq!(slot, None);
    }

    #[test]
    fn a_whole_drag_within_one_frame_begins_and_commits() {
        let mut slot = None;
        let committed = drive_touch_drag(&mut slot, sel(pos2(1.0, 1.0), pos2(9.0, 9.0), true, Some(TouchSelectionEnd::Released)), touch_drag_at);
        assert_eq!(committed.map(|drag| (drag.start, drag.current)), Some((pos2(1.0, 1.0), pos2(9.0, 9.0))));
        assert_eq!(slot, None);
    }

    #[test]
    fn a_cancelled_touch_drag_is_dropped_without_a_commit() {
        let origin = pos2(10.0, 20.0);
        let mut slot = None;
        assert_eq!(drive_touch_drag(&mut slot, sel(origin, origin, true, None), touch_drag_at), None);
        assert!(slot.is_some());
        assert_eq!(drive_touch_drag(&mut slot, sel(origin, pos2(5.0, 5.0), false, Some(TouchSelectionEnd::Cancelled)), touch_drag_at), None);
        assert_eq!(slot, None);
    }

    #[test]
    fn a_refused_begin_ignores_the_rest_of_the_gesture() {
        let mut slot: Option<Drag> = None;
        assert_eq!(drive_touch_drag(&mut slot, sel(pos2(1.0, 1.0), pos2(2.0, 2.0), true, None), |_| None), None);
        assert_eq!(slot, None);
        assert_eq!(drive_touch_drag(&mut slot, sel(pos2(1.0, 1.0), pos2(3.0, 3.0), false, Some(TouchSelectionEnd::Released)), touch_drag_at), None);
        assert_eq!(slot, None);
    }

    #[test]
    fn a_mouse_drag_in_the_slot_is_never_touched() {
        let mouse = Drag { start: pos2(0.0, 0.0), current: pos2(4.0, 4.0), touch: false };
        let mut slot = Some(mouse);
        let begun = drive_touch_drag(&mut slot, sel(pos2(50.0, 50.0), pos2(60.0, 60.0), true, None), touch_drag_at);
        assert_eq!(begun, None);
        assert_eq!(slot, Some(mouse), "a finger never replaces or moves a mouse drag");
        for end in [TouchSelectionEnd::Released, TouchSelectionEnd::Cancelled] {
            assert_eq!(drive_touch_drag(&mut slot, sel(pos2(50.0, 50.0), pos2(70.0, 70.0), false, Some(end)), touch_drag_at), None);
            assert_eq!(slot, Some(mouse), "{end:?} of a finger must not end a mouse drag");
        }
    }

    #[test]
    fn long_press_selection_is_a_one_frame_flag() {
        let mut view = view_with(1.0, Vec2::ZERO, viewport());
        view.set_long_press_selection(true);
        assert_eq!(view.take_touch_surface().map(|surface| surface.long_press_selection), Some(true));
        assert_eq!(view.take_touch_surface().map(|surface| surface.long_press_selection), Some(false), "a frame without the setter publishes false");
        // Consumed even when no surface can be built yet.
        let mut unlaid = CanvasView::default();
        unlaid.set_long_press_selection(true);
        assert_eq!(unlaid.take_touch_surface(), None);
        assert!(!unlaid.scene.long_press_selection);
    }

    #[test]
    fn handle_holes_join_the_published_surface() {
        let ctx = egui::Context::default();
        let mut view = view_with(1.0, Vec2::ZERO, viewport());
        let handle = Rect::from_center_size(pos2(200.0, 200.0), vec2(10.0, 10.0));
        let radius = ctx.global_style().interaction.interact_radius;
        ctx.run_ui(egui::RawInput::default(), |ui| view.note_touch_handle_hole(ui, handle))
            .drop_without_applying_deltas();
        assert!(view.touch_surface_holes().contains(&handle.expand(radius)));
    }
}
