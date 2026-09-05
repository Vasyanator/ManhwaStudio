/*
File: tabs/ps_editor/tools/select.rs

Purpose:
Selection tools for the PS-like editor: rectangular marquee and freehand/polygonal lasso. Both
build the page `Selection` mask used to clip the brush, and both support the Photoshop combination
modes (new / add / subtract / intersect). The same struct backs both tools, parameterized by
`SelectMode`.

Key structures:
- `SelectMode`: `Rect` or `Lasso`.
- `SelectTool`: persistent `base_op` plus the live gesture state.
- `GestureInput`: GUI-free snapshot of one frame of input (the state machine's only input).
- `GestureAction` / `CommitShape`: what one `step` decided — nothing, abort, or commit this shape.

Key functions:
- `gesture_op`: pure modifier -> `SelectionOp` mapping, sampled once per gesture.
- `SelectTool::step`: the whole press / alt / release / pending-polygon state machine, pure and
  testable without a `PsToolContext`.
- `SelectTool::apply_commit`: the only place that writes into the page `Selection`.
- `PsTool::reset` / `PsTool::freeze`: the two lifecycle hooks. `reset` drops the outline when its
  context is gone (tool switch, page switch, Esc during suppressed input); `freeze` marks a frame
  the tab did not route here, so the release it swallowed commits instead of aborting.

Notes:
Alt means two different things and the two are told apart by WHEN it is held: Alt down BEFORE the
press selects the "subtract" combination mode, Alt held AFTER the press draws straight segments.
That is why the combination mode is sampled exactly once, on the press frame. The two meanings are
kept from overlapping by `Gesture::alt_armed`: Alt that was already down at the press stays the
mode latch until it is RELEASED once inside the gesture, so an Alt-drag subtracts freehand instead
of subtracting in polyline steps.
A click with no meaningful drag (a degenerate rect / fewer than three lasso points) reaches
`apply_rect` / `apply_polygon` anyway: with `Replace` those clear the mask (click to deselect),
while `Add`/`Subtract`/`Intersect` leave the existing selection alone. The commit always ends in
`PsToolContext::normalize_selection`, so an empty result is `None` and never `Some(all-zero)`.
*/

use super::{PsTool, PsToolContext, PsToolId, ToolOutcome};
use crate::tabs::ps_editor::selection::SelectionOp;
use crate::tabs::ps_editor::viewport::ViewTransform;
use eframe::egui;
use egui::{Color32, Pos2, Stroke};

/// Minimum distance in image pixels between two freehand lasso samples.
///
/// Sparse sampling keeps the committed polygon small; the rasterizer interpolates between
/// vertices anyway.
const FREEHAND_MIN_STEP: f32 = 2.0;

/// Minimum distance in image pixels between two explicitly placed anchors.
///
/// Only guards against exact duplicates (a click that does not move the pointer); anchors are
/// otherwise honored exactly, unlike freehand samples.
const ANCHOR_MIN_STEP: f32 = 0.01;

/// Which selection shape a `SelectTool` instance builds.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SelectMode {
    Rect,
    Lasso,
}

/// Combination mode for one gesture, following Photoshop's modifier rules.
///
/// Shift adds, Alt subtracts, Shift+Alt intersects; with no modifier the tool's persistent
/// `base_op` (set in the options row) applies. Sampled once, on the press frame — see the file
/// header for why the modifiers must not be re-read mid-gesture.
#[must_use]
pub fn gesture_op(base: SelectionOp, mods: egui::Modifiers) -> SelectionOp {
    match (mods.shift, mods.alt) {
        (true, true) => SelectionOp::Intersect,
        (true, false) => SelectionOp::Add,
        (false, true) => SelectionOp::Subtract,
        (false, false) => base,
    }
}

/// GUI-free snapshot of one frame of input, as far as the selection state machine cares.
///
/// Built from `PsToolContext` by `interact`; constructed directly by the unit tests, which is the
/// whole point of the split.
#[derive(Debug, Clone, Copy)]
pub struct GestureInput {
    /// Pointer position in image pixels (fractional), or `None` when unavailable.
    pub pointer: Option<Pos2>,
    /// True when the pointer is inside the viewport rect; gates gesture STARTS only.
    pub pointer_in_viewport: bool,
    pub primary_pressed: bool,
    pub primary_down: bool,
    pub primary_released: bool,
    pub modifiers: egui::Modifiers,
    /// Escape: abandon the in-progress outline without touching the committed selection.
    pub cancel_pressed: bool,
    /// Backspace/Delete: drop the last placed vertex of an in-progress outline.
    pub remove_point_pressed: bool,
}

/// Geometry a finished gesture wants written into the page selection.
///
/// Both variants may be degenerate (a click): that is not an error, it is how "click to deselect"
/// reaches `Selection` — see the file header.
#[derive(Debug, Clone, PartialEq)]
pub enum CommitShape {
    /// Rectangle corners in image pixels: the press anchor and the release pointer.
    Rect(Pos2, Pos2),
    /// Implicitly closed polygon vertices in image pixels.
    Polygon(Vec<(f32, f32)>),
}

/// What one `SelectTool::step` decided.
#[derive(Debug, Clone, PartialEq)]
pub enum GestureAction {
    /// Nothing to commit; a gesture may still be in progress.
    None,
    /// The in-progress gesture ended without touching the selection.
    Aborted,
    /// The gesture finished: write `shape` into the selection with `op`.
    Commit { op: SelectionOp, shape: CommitShape },
}

/// Internal decision of the gesture-handling block, taken while the gesture is borrowed.
///
/// Exists so the borrow of `SelectTool::gesture` ends before the gesture is cleared or moved out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GestureStep {
    Idle,
    Abort,
    Finish,
}

/// State of one in-progress selection gesture.
#[derive(Debug, Clone)]
struct Gesture {
    /// Combination mode sampled on the press frame; immutable for the rest of the gesture.
    op: SelectionOp,
    /// Rect anchor in image pixels (`Rect` mode only).
    anchor: Option<Pos2>,
    /// Committed lasso vertices in image pixels (`Lasso` mode only).
    points: Vec<(f32, f32)>,
    /// Whether the primary button is currently held.
    button_down: bool,
    /// True while the outline stays alive with the button UP, waiting for the next anchor click.
    ///
    /// Entered by releasing the button with Alt held (Photoshop's polygonal lasso); left by
    /// pressing again (places an anchor) or by releasing Alt (closes the path).
    pending_polygon: bool,
    /// Whether Alt is allowed to mean "straight segment" yet.
    ///
    /// False for a gesture begun WITH Alt held, until Alt is released once inside the gesture.
    /// See the arming comment in [`SelectTool::step`] for why the two meanings of Alt must not
    /// overlap.
    alt_armed: bool,
    /// True when the segment from the last anchor to the pointer is a rubber band rather than a
    /// sampled part of the path. Recomputed every frame because `draw_overlay` sees no modifiers.
    rubber_band: bool,
}

impl Gesture {
    /// Starts an empty gesture with its combination mode already fixed.
    ///
    /// `alt_at_press` is the Alt state on the press frame: it selected the combination mode, so
    /// Alt stays disarmed as the straight-segment modifier until it is released once.
    fn new(op: SelectionOp, alt_at_press: bool) -> Self {
        Self {
            op,
            anchor: None,
            points: Vec::new(),
            button_down: true,
            pending_polygon: false,
            rubber_band: false,
            alt_armed: !alt_at_press,
        }
    }
}

/// Rectangular-marquee or freehand/polygonal-lasso selection tool.
#[derive(Debug, Clone)]
pub struct SelectTool {
    mode: SelectMode,
    /// Combination mode used when no modifier is held at the press. Set by the options row.
    base_op: SelectionOp,
    /// The gesture in progress, if any.
    gesture: Option<Gesture>,
    /// Latest pointer position in image pixels (rect preview + rect commit).
    last_pointer: Option<Pos2>,
    /// Set by [`PsTool::freeze`] when the tab did not route a frame's input here (canvas pan,
    /// text drag), and consumed by the first `step` that follows.
    ///
    /// Those frames hide the button events, so a release performed during them never reaches the
    /// state machine. On resume the missing release must be read as a real release (commit the
    /// outline), NOT as the interrupted drag that the same input pattern means otherwise.
    suspended: bool,
}

impl SelectTool {
    /// Creates a selection tool for `mode` with the "new selection" combination mode.
    #[must_use]
    pub fn new(mode: SelectMode) -> Self {
        Self {
            mode,
            base_op: SelectionOp::default(),
            gesture: None,
            last_pointer: None,
            suspended: false,
        }
    }

    /// Advances the gesture state machine by one frame of input.
    ///
    /// Pure: touches no `PsToolContext`, no `Ui` and no `Selection`. On
    /// [`GestureAction::Commit`] the gesture has already been taken out of the tool, so the same
    /// geometry is never committed twice.
    ///
    /// A `suspended` flag left by [`PsTool::freeze`] is consumed here: on the first frame after a
    /// suppressed one, "button up with no release event" means the release was swallowed by the
    /// suppression and the outline is committed instead of dropped.
    #[must_use]
    fn step(&mut self, input: GestureInput) -> GestureAction {
        let mode = self.mode;
        // Taken before the gesture is borrowed, and consumed by this frame whatever it decides:
        // the flag describes the gap between the last routed frame and this one, nothing later.
        let resumed = std::mem::take(&mut self.suspended);
        if let Some(pointer) = input.pointer {
            self.last_pointer = Some(pointer);
        }

        // Esc abandons the outline. It must never reach `Selection`: the previously committed
        // selection has to survive an aborted gesture untouched.
        if input.cancel_pressed {
            return if self.gesture.take().is_some() {
                GestureAction::Aborted
            } else {
                GestureAction::None
            };
        }

        // Backspace/Delete undoes the last placed vertex, and the gesture dies with its last one.
        // Lasso only: a rect gesture has no vertex list, and killing a rect drag on Backspace
        // would be a surprise rather than an undo.
        if input.remove_point_pressed
            && mode == SelectMode::Lasso
            && let Some(gesture) = self.gesture.as_mut()
        {
            gesture.points.pop();
            if gesture.points.is_empty() {
                self.gesture = None;
                return GestureAction::Aborted;
            }
            return GestureAction::None;
        }

        // A gesture may only START inside the viewport; once running it follows the pointer out.
        if input.primary_pressed
            && input.pointer_in_viewport
            && let Some(pointer) = input.pointer
        {
            self.begin_or_extend(pointer, input.modifiers);
            // Deliberately no early return: egui can deliver `primary_pressed` and
            // `primary_released` in the SAME frame for a fast click, and that click must still
            // reach the release branch below — it is what performs "click to deselect".
        }

        let next = {
            let Some(gesture) = self.gesture.as_mut() else {
                return GestureAction::None;
            };
            // Alt means two different things, and which one is decided by what came FIRST. Held
            // at the press it picked Subtract/Intersect (`gesture_op`), and that latch must not
            // double as the straight-segment modifier: an Alt-drag would then subtract in polyline
            // steps instead of freehand, which is the one place Photoshop's own key assignment is
            // genuinely ambiguous. So Alt only starts meaning "straight segment" once it has been
            // RELEASED inside the gesture. A gesture begun without Alt is armed from the start, so
            // pressing Alt mid-stroke still switches to straight segments immediately.
            if !input.modifiers.alt {
                gesture.alt_armed = true;
            }
            let alt_segments = input.modifiers.alt && gesture.alt_armed;
            gesture.rubber_band = gesture.pending_polygon || (gesture.button_down && alt_segments);

            if input.primary_down {
                gesture.button_down = true;
                // With Alt held the pointer is not sampled at all, so the segment from the last
                // anchor stays straight until the next click commits it.
                if mode == SelectMode::Lasso
                    && !alt_segments
                    && let Some(pointer) = input.pointer
                {
                    push_vertex(&mut gesture.points, pointer, FREEHAND_MIN_STEP);
                }
                GestureStep::Idle
            } else if input.primary_released {
                gesture.button_down = false;
                match mode {
                    SelectMode::Rect => GestureStep::Finish,
                    SelectMode::Lasso => {
                        if let Some(pointer) = input.pointer {
                            push_vertex(&mut gesture.points, pointer, ANCHOR_MIN_STEP);
                        }
                        if alt_segments {
                            // Photoshop: releasing with Alt held only commits an anchor. The
                            // outline stays alive with the button up until Alt is let go.
                            gesture.pending_polygon = true;
                            gesture.rubber_band = true;
                            GestureStep::Idle
                        } else {
                            GestureStep::Finish
                        }
                    }
                }
            } else if gesture.pending_polygon {
                // Alt released while the polygon was pending: Photoshop closes the path there.
                if alt_segments {
                    GestureStep::Idle
                } else {
                    GestureStep::Finish
                }
            } else if resumed {
                // The frames since the last routed one were suppressed (canvas pan / text drag),
                // so the release that ended the drag never reached us. Panning mid-lasso is a
                // normal workflow: honor the swallowed release and commit rather than discard the
                // traced path.
                GestureStep::Finish
            } else {
                // Button up, no release event, nothing pending, and no suppressed frames to blame:
                // the drag really was interrupted (focus loss, another widget grabbed the button).
                // Drop the outline.
                GestureStep::Abort
            }
        };

        match next {
            GestureStep::Idle => GestureAction::None,
            GestureStep::Abort => {
                self.gesture = None;
                GestureAction::Aborted
            }
            GestureStep::Finish => self.take_commit(),
        }
    }

    /// Starts a new gesture, or places the next anchor of a pending polygon.
    ///
    /// A new gesture samples its combination mode here and keeps it until it commits.
    fn begin_or_extend(&mut self, pointer: Pos2, modifiers: egui::Modifiers) {
        let mode = self.mode;
        if let Some(gesture) = self.gesture.as_mut() {
            gesture.pending_polygon = false;
            gesture.button_down = true;
            match mode {
                SelectMode::Rect => gesture.anchor = Some(pointer),
                SelectMode::Lasso => push_vertex(&mut gesture.points, pointer, ANCHOR_MIN_STEP),
            }
            return;
        }
        let mut gesture = Gesture::new(gesture_op(self.base_op, modifiers), modifiers.alt);
        match mode {
            SelectMode::Rect => gesture.anchor = Some(pointer),
            SelectMode::Lasso => gesture.points.push((pointer.x, pointer.y)),
        }
        crate::trace_log!(
            crate::trace::cat::INPUT,
            "selection gesture_begin mode={:?} op={:?} at=({:.1},{:.1})",
            mode,
            gesture.op,
            pointer.x,
            pointer.y
        );
        self.gesture = Some(gesture);
    }

    /// Takes the finished gesture out of the tool and turns it into a commit action.
    ///
    /// Returns [`GestureAction::Aborted`] when the geometry cannot be formed (a rect whose anchor
    /// or end pointer is unknown), so the caller never writes a half-built shape.
    fn take_commit(&mut self) -> GestureAction {
        let Some(gesture) = self.gesture.take() else {
            return GestureAction::None;
        };
        let shape = match self.mode {
            SelectMode::Rect => {
                let (Some(anchor), Some(end)) = (gesture.anchor, self.last_pointer) else {
                    return GestureAction::Aborted;
                };
                CommitShape::Rect(anchor, end)
            }
            SelectMode::Lasso => CommitShape::Polygon(gesture.points),
        };
        GestureAction::Commit { op: gesture.op, shape }
    }

    /// Writes a finished shape into the page selection and reports the outcome.
    ///
    /// The only place in this tool that touches `Selection`. Always ends in
    /// `normalize_selection`, so a gesture that selected nothing leaves `None` rather than a
    /// phantom all-zero mask that would silently disable the brush.
    fn apply_commit(&self, ctx: &mut PsToolContext<'_>, op: SelectionOp, shape: &CommitShape) -> ToolOutcome {
        use crate::trace::cat;
        let mut outcome = ToolOutcome::default();
        match shape {
            CommitShape::Rect(start, end) => {
                ctx.ensure_selection().apply_rect(
                    start.x.round() as i32,
                    start.y.round() as i32,
                    end.x.round() as i32,
                    end.y.round() as i32,
                    op,
                );
                ctx.normalize_selection();
                crate::trace_log!(
                    cat::PS_EDITOR,
                    "selection commit_rect mode={:?} op={:?} from=({:.1},{:.1}) to=({:.1},{:.1}) any={}",
                    self.mode,
                    op,
                    start.x,
                    start.y,
                    end.x,
                    end.y,
                    ctx.selection.as_ref().is_some_and(|s| s.any())
                );
            }
            CommitShape::Polygon(points) => {
                ctx.ensure_selection().apply_polygon(points, op);
                ctx.normalize_selection();
                crate::trace_log!(
                    cat::PS_EDITOR,
                    "selection commit_lasso mode={:?} op={:?} points={} any={}",
                    self.mode,
                    op,
                    points.len(),
                    ctx.selection.as_ref().is_some_and(|s| s.any())
                );
            }
        }
        outcome.selection_changed = true;
        outcome
    }

    /// Draws the four mutually exclusive combination-mode buttons for `base_op`.
    ///
    /// The label sits on its own line and the buttons flow in a WRAPPING row: the tool panel is a
    /// fixed 220 px `Panel::left`, which the four localized mode names plus a leading label
    /// overflow in Russian and by more in French/Portuguese — on a single non-wrapping row the
    /// last button is clipped and unclickable.
    fn mode_row(&mut self, ui: &mut egui::Ui) {
        // Photoshop's option-bar order: new, add, subtract, intersect.
        let choices = [
            (SelectionOp::Replace, t!("ps_editor.tools.select_mode_new"), t!("ps_editor.tools.select_mode_new_tip")),
            (SelectionOp::Add, t!("ps_editor.tools.select_mode_add"), t!("ps_editor.tools.select_mode_add_tip")),
            (SelectionOp::Subtract, t!("ps_editor.tools.select_mode_subtract"), t!("ps_editor.tools.select_mode_subtract_tip")),
            (SelectionOp::Intersect, t!("ps_editor.tools.select_mode_intersect"), t!("ps_editor.tools.select_mode_intersect_tip")),
        ];
        ui.label(t!("ps_editor.tools.select_mode_label"));
        ui.horizontal_wrapped(|ui| {
            for (op, label, tip) in choices {
                if ui.selectable_label(self.base_op == op, label).on_hover_text(tip).clicked() {
                    self.base_op = op;
                }
            }
        });
    }
}

impl PsTool for SelectTool {
    fn id(&self) -> PsToolId {
        match self.mode {
            SelectMode::Rect => PsToolId::SelectRect,
            SelectMode::Lasso => PsToolId::SelectLasso,
        }
    }

    fn title(&self) -> &'static str {
        match self.mode {
            SelectMode::Rect => t!("ps_editor.tools.rect_select_title"),
            SelectMode::Lasso => t!("ps_editor.tools.lasso_title"),
        }
    }

    fn interact(&mut self, ctx: &mut PsToolContext<'_>) -> ToolOutcome {
        let input = GestureInput {
            pointer: ctx.pointer_image,
            pointer_in_viewport: ctx.pointer_in_viewport,
            primary_pressed: ctx.primary_pressed,
            primary_down: ctx.primary_down,
            primary_released: ctx.primary_released,
            modifiers: ctx.modifiers,
            cancel_pressed: ctx.cancel_pressed,
            remove_point_pressed: ctx.remove_point_pressed,
        };
        match self.step(input) {
            GestureAction::None | GestureAction::Aborted => ToolOutcome::default(),
            GestureAction::Commit { op, shape } => self.apply_commit(ctx, op, &shape),
        }
    }

    /// Drops the in-progress outline. The committed page selection is never touched: an abandoned
    /// gesture must leave exactly what was selected before it started.
    ///
    /// A pending polygon (button UP, waiting for Alt to be released) would otherwise outlive the
    /// tool switch or page switch that made its coordinates and its press-time combination mode
    /// meaningless, and commit on some later frame.
    fn reset(&mut self) {
        if self.gesture.take().is_some() {
            crate::trace_log!(crate::trace::cat::INPUT, "selection gesture_reset mode={:?}", self.mode);
        }
        self.suspended = false;
    }

    /// Records that this frame's input went to a canvas pan / text drag instead of the tool, so
    /// the next `step` reads a missing release as a swallowed one rather than an interruption.
    ///
    /// Only meaningful while a gesture is live; otherwise the flag is consumed harmlessly.
    fn freeze(&mut self) {
        if self.gesture.is_some() {
            self.suspended = true;
        }
    }

    fn draw_overlay(&self, painter: &egui::Painter, view: &ViewTransform, pointer_image: Option<Pos2>) {
        let Some(gesture) = self.gesture.as_ref() else {
            return;
        };
        // Thin black-and-white dashed preview, matching the committed marquee (never blue).
        match self.mode {
            SelectMode::Rect => {
                if let (Some(start), Some(end)) = (gesture.anchor, self.last_pointer) {
                    let rect = egui::Rect::from_two_pos(view.world_to_screen(start), view.world_to_screen(end));
                    let corners = [
                        rect.left_top(),
                        rect.right_top(),
                        rect.right_bottom(),
                        rect.left_bottom(),
                        rect.left_top(),
                    ];
                    draw_dashed_preview(painter, &corners);
                }
            }
            SelectMode::Lasso => {
                let mut path: Vec<Pos2> = gesture
                    .points
                    .iter()
                    .map(|&(x, y)| view.world_to_screen(Pos2::new(x, y)))
                    .collect();
                // In straight-segment or pending-polygon state the pointer is not a sampled vertex,
                // so the segment from the last anchor to it must be drawn as a rubber band.
                if gesture.rubber_band
                    && let Some(pointer) = pointer_image.or(self.last_pointer)
                {
                    path.push(view.world_to_screen(pointer));
                }
                if path.len() >= 2 {
                    draw_dashed_preview(painter, &path);
                }
            }
        }
    }

    fn options_ui(&mut self, ui: &mut egui::Ui) {
        self.mode_row(ui);
        match self.mode {
            SelectMode::Rect => {
                ui.label(t!("ps_editor.tools.rect_select_hint"));
            }
            SelectMode::Lasso => {
                ui.label(t!("ps_editor.tools.lasso_hint"));
            }
        }
        ui.label(t!("ps_editor.tools.select_mods_hint"));
        if self.mode == SelectMode::Lasso {
            ui.label(t!("ps_editor.tools.lasso_hint_alt"));
            ui.label(t!("ps_editor.tools.lasso_hint_keys"));
        }
        ui.label(t!("ps_editor.tools.select_clear_hint"));
    }
}

/// Appends `pointer` to `points` unless it is closer than `min_dist` to the current last vertex.
///
/// Freehand sampling passes a coarse `min_dist` to keep the polygon small; explicitly placed
/// anchors pass a tiny one, which only rejects an exact duplicate.
fn push_vertex(points: &mut Vec<(f32, f32)>, pointer: Pos2, min_dist: f32) {
    let far_enough = points
        .last()
        .is_none_or(|&(lx, ly)| (lx - pointer.x).hypot(ly - pointer.y) >= min_dist);
    if far_enough {
        points.push((pointer.x, pointer.y));
    }
}

/// Draws a thin black-and-white dashed path (offset white over black) for the in-progress marquee.
fn draw_dashed_preview(painter: &egui::Painter, path: &[Pos2]) {
    let dash = 5.0;
    let gap = 5.0;
    let mut shapes = Vec::new();
    for segment in path.windows(2) {
        egui::Shape::dashed_line_many(
            segment,
            Stroke::new(1.0, Color32::BLACK),
            dash,
            gap,
            &mut shapes,
        );
        egui::Shape::dashed_line_many_with_offset(
            segment,
            Stroke::new(1.0, Color32::WHITE),
            &[dash],
            &[gap],
            dash,
            &mut shapes,
        );
    }
    painter.extend(shapes);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Modifier snapshot with only shift/alt set — the two the selection tools read.
    fn mods(shift: bool, alt: bool) -> egui::Modifiers {
        egui::Modifiers { shift, alt, ..Default::default() }
    }

    /// A frame with no pointer activity and no keys.
    fn idle(m: egui::Modifiers) -> GestureInput {
        GestureInput {
            pointer: None,
            pointer_in_viewport: true,
            primary_pressed: false,
            primary_down: false,
            primary_released: false,
            modifiers: m,
            cancel_pressed: false,
            remove_point_pressed: false,
        }
    }

    fn press(x: f32, y: f32, m: egui::Modifiers) -> GestureInput {
        GestureInput { pointer: Some(Pos2::new(x, y)), primary_pressed: true, primary_down: true, ..idle(m) }
    }

    fn drag(x: f32, y: f32, m: egui::Modifiers) -> GestureInput {
        GestureInput { pointer: Some(Pos2::new(x, y)), primary_down: true, ..idle(m) }
    }

    fn release(x: f32, y: f32, m: egui::Modifiers) -> GestureInput {
        GestureInput { pointer: Some(Pos2::new(x, y)), primary_released: true, ..idle(m) }
    }

    #[test]
    fn gesture_op_maps_photoshop_modifiers() {
        assert_eq!(gesture_op(SelectionOp::Replace, mods(true, true)), SelectionOp::Intersect);
        assert_eq!(gesture_op(SelectionOp::Replace, mods(true, false)), SelectionOp::Add);
        assert_eq!(gesture_op(SelectionOp::Replace, mods(false, true)), SelectionOp::Subtract);
        assert_eq!(gesture_op(SelectionOp::Replace, mods(false, false)), SelectionOp::Replace);
    }

    #[test]
    fn gesture_op_falls_back_to_base_op_only_without_modifiers() {
        // With no modifier the options-row choice wins...
        assert_eq!(gesture_op(SelectionOp::Subtract, mods(false, false)), SelectionOp::Subtract);
        // ...and a modifier overrides it for that one gesture.
        assert_eq!(gesture_op(SelectionOp::Subtract, mods(true, false)), SelectionOp::Add);
    }

    #[test]
    fn mode_is_sampled_at_press_and_not_reread() {
        let mut tool = SelectTool::new(SelectMode::Lasso);
        // Shift at the press = Add for the whole gesture.
        assert_eq!(tool.step(press(0.0, 0.0, mods(true, false))), GestureAction::None);
        // Shift let go mid-drag: the mode must NOT fall back to Replace.
        assert_eq!(tool.step(drag(20.0, 0.0, mods(false, false))), GestureAction::None);
        assert_eq!(tool.step(drag(20.0, 20.0, mods(false, false))), GestureAction::None);
        match tool.step(release(0.0, 20.0, mods(false, false))) {
            GestureAction::Commit { op, shape } => {
                assert_eq!(op, SelectionOp::Add, "mode must stay as sampled at press");
                match shape {
                    CommitShape::Polygon(points) => assert_eq!(points.len(), 4),
                    CommitShape::Rect(..) => panic!("lasso must commit a polygon"),
                }
            }
            other => panic!("expected a commit, got {other:?}"),
        }
        assert!(tool.gesture.is_none(), "the gesture must be consumed by the commit");
    }

    #[test]
    fn base_op_used_when_no_modifier_is_held() {
        let mut tool = SelectTool::new(SelectMode::Rect);
        tool.base_op = SelectionOp::Intersect;
        assert_eq!(tool.step(press(1.0, 1.0, mods(false, false))), GestureAction::None);
        match tool.step(release(9.0, 9.0, mods(false, false))) {
            GestureAction::Commit { op, shape } => {
                assert_eq!(op, SelectionOp::Intersect);
                assert_eq!(shape, CommitShape::Rect(Pos2::new(1.0, 1.0), Pos2::new(9.0, 9.0)));
            }
            other => panic!("expected a commit, got {other:?}"),
        }
    }

    #[test]
    fn alt_during_drag_suppresses_freehand_sampling() {
        let mut tool = SelectTool::new(SelectMode::Lasso);
        assert_eq!(tool.step(press(0.0, 0.0, mods(false, false))), GestureAction::None);
        assert_eq!(tool.step(drag(50.0, 50.0, mods(false, true))), GestureAction::None);
        let gesture = tool.gesture.as_ref().expect("gesture is live between press and release");
        assert_eq!(gesture.points.len(), 1, "Alt draws a straight segment, it does not sample");
        assert!(gesture.rubber_band, "the aimed segment must be previewed as a rubber band");
    }

    #[test]
    fn alt_held_from_the_press_subtracts_freehand_instead_of_drawing_segments() {
        let mut tool = SelectTool::new(SelectMode::Lasso);
        // Alt at the press selected Subtract, so it must NOT also mean "straight segment":
        // the stroke has to stay freehand or the user gets a polyline subtraction.
        assert_eq!(tool.step(press(0.0, 0.0, mods(false, true))), GestureAction::None);
        assert_eq!(tool.step(drag(50.0, 50.0, mods(false, true))), GestureAction::None);
        {
            let gesture = tool.gesture.as_ref().expect("gesture is live between press and release");
            assert_eq!(gesture.op, SelectionOp::Subtract);
            assert_eq!(gesture.points.len(), 2, "an Alt-started stroke keeps sampling freehand");
            assert!(!gesture.rubber_band, "no rubber band while Alt is still the mode latch");
        }
        // ... and releasing with Alt still held commits, rather than opening a pending polygon.
        match tool.step(release(80.0, 80.0, mods(false, true))) {
            GestureAction::Commit { op, shape } => {
                assert_eq!(op, SelectionOp::Subtract);
                assert_eq!(shape, CommitShape::Polygon(vec![(0.0, 0.0), (50.0, 50.0), (80.0, 80.0)]));
            }
            other => panic!("expected an immediate subtract commit, got {other:?}"),
        }
        assert!(tool.gesture.is_none());
    }

    #[test]
    fn releasing_alt_inside_a_subtract_stroke_arms_the_straight_segments() {
        let mut tool = SelectTool::new(SelectMode::Lasso);
        assert_eq!(tool.step(press(0.0, 0.0, mods(false, true))), GestureAction::None);
        // Alt let go mid-stroke: the mode stays Subtract, and Alt is now free to mean "segment".
        assert_eq!(tool.step(drag(50.0, 0.0, mods(false, false))), GestureAction::None);
        assert_eq!(tool.step(drag(50.0, 50.0, mods(false, true))), GestureAction::None);
        let gesture = tool.gesture.as_ref().expect("gesture is live between press and release");
        assert_eq!(gesture.op, SelectionOp::Subtract, "the mode latched at the press never changes");
        assert_eq!(gesture.points.len(), 2, "the second Alt press stops sampling");
        assert!(gesture.rubber_band, "the aimed segment must be previewed as a rubber band");
    }

    #[test]
    fn alt_release_keeps_polygon_pending_until_alt_is_let_go() {
        let mut tool = SelectTool::new(SelectMode::Lasso);
        assert_eq!(tool.step(press(0.0, 0.0, mods(false, false))), GestureAction::None);
        // Release with Alt held: an anchor is committed, the outline survives with the button up.
        assert_eq!(tool.step(release(40.0, 0.0, mods(false, true))), GestureAction::None);
        {
            let gesture = tool.gesture.as_ref().expect("the pending polygon must stay alive");
            assert!(gesture.pending_polygon);
            assert_eq!(gesture.points.len(), 2);
        }
        // A further click places another anchor, still with Alt held.
        assert_eq!(tool.step(press(40.0, 40.0, mods(false, true))), GestureAction::None);
        assert_eq!(tool.step(release(40.0, 40.0, mods(false, true))), GestureAction::None);
        assert_eq!(tool.gesture.as_ref().map(|g| g.points.len()), Some(3));
        // Alt let go while pending: Photoshop closes the path here.
        match tool.step(idle(mods(false, false))) {
            GestureAction::Commit { op, shape } => {
                assert_eq!(op, SelectionOp::Replace);
                assert_eq!(shape, CommitShape::Polygon(vec![(0.0, 0.0), (40.0, 0.0), (40.0, 40.0)]));
            }
            other => panic!("expected the pending polygon to close, got {other:?}"),
        }
    }

    #[test]
    fn escape_aborts_without_committing() {
        let mut tool = SelectTool::new(SelectMode::Lasso);
        assert_eq!(tool.step(press(0.0, 0.0, mods(false, false))), GestureAction::None);
        assert_eq!(tool.step(drag(30.0, 0.0, mods(false, false))), GestureAction::None);
        let cancel = GestureInput { cancel_pressed: true, primary_down: true, ..idle(mods(false, false)) };
        assert_eq!(tool.step(cancel), GestureAction::Aborted);
        assert!(tool.gesture.is_none());
        // The release that follows the abort must not resurrect or commit anything.
        assert_eq!(tool.step(release(30.0, 30.0, mods(false, false))), GestureAction::None);
    }

    #[test]
    fn backspace_removes_the_last_point_then_aborts_at_zero() {
        let mut tool = SelectTool::new(SelectMode::Lasso);
        assert_eq!(tool.step(press(0.0, 0.0, mods(false, false))), GestureAction::None);
        assert_eq!(tool.step(drag(30.0, 0.0, mods(false, false))), GestureAction::None);
        assert_eq!(tool.gesture.as_ref().map(|g| g.points.len()), Some(2));
        let remove = GestureInput { remove_point_pressed: true, primary_down: true, ..idle(mods(false, false)) };
        assert_eq!(tool.step(remove), GestureAction::None);
        assert_eq!(tool.gesture.as_ref().map(|g| g.points.len()), Some(1));
        assert_eq!(tool.step(remove), GestureAction::Aborted);
        assert!(tool.gesture.is_none(), "removing the last point ends the gesture");
    }

    #[test]
    fn click_without_drag_commits_degenerate_geometry() {
        // The degenerate shape must still reach `Selection`: with `Replace` it is what clears the
        // selection, with the other ops it is a no-op. Returning early would break both.
        let mut lasso = SelectTool::new(SelectMode::Lasso);
        assert_eq!(lasso.step(press(7.0, 7.0, mods(false, false))), GestureAction::None);
        match lasso.step(release(7.0, 7.0, mods(false, false))) {
            GestureAction::Commit { op, shape } => {
                assert_eq!(op, SelectionOp::Replace);
                assert_eq!(shape, CommitShape::Polygon(vec![(7.0, 7.0)]));
            }
            other => panic!("expected a degenerate commit, got {other:?}"),
        }

        let mut rect = SelectTool::new(SelectMode::Rect);
        assert_eq!(rect.step(press(7.0, 7.0, mods(false, false))), GestureAction::None);
        match rect.step(release(7.0, 7.0, mods(false, false))) {
            GestureAction::Commit { shape, .. } => {
                assert_eq!(shape, CommitShape::Rect(Pos2::new(7.0, 7.0), Pos2::new(7.0, 7.0)));
            }
            other => panic!("expected a degenerate commit, got {other:?}"),
        }
    }

    #[test]
    fn press_outside_the_viewport_starts_nothing() {
        let mut tool = SelectTool::new(SelectMode::Lasso);
        let outside = GestureInput { pointer_in_viewport: false, ..press(5.0, 5.0, mods(false, false)) };
        assert_eq!(tool.step(outside), GestureAction::None);
        assert!(tool.gesture.is_none());
    }

    #[test]
    fn lost_button_drops_the_outline() {
        let mut tool = SelectTool::new(SelectMode::Lasso);
        assert_eq!(tool.step(press(0.0, 0.0, mods(false, false))), GestureAction::None);
        // Neither down nor released and nothing pending: the drag was interrupted.
        assert_eq!(tool.step(idle(mods(false, false))), GestureAction::Aborted);
        assert!(tool.gesture.is_none());
    }

    #[test]
    fn reset_abandons_a_pending_polygon_so_nothing_commits_later() {
        let mut tool = SelectTool::new(SelectMode::Lasso);
        assert_eq!(tool.step(press(0.0, 0.0, mods(false, false))), GestureAction::None);
        // Release with Alt: the outline survives with the button up, waiting for Alt to be let go.
        assert_eq!(tool.step(release(40.0, 0.0, mods(false, true))), GestureAction::None);
        assert!(tool.gesture.is_some(), "the pending polygon must be alive before the reset");
        tool.reset();
        assert!(tool.gesture.is_none(), "reset must drop the pending outline");
        // The frame that would have closed the path (Alt released) must now commit nothing.
        assert_eq!(tool.step(idle(mods(false, false))), GestureAction::None);
    }

    #[test]
    fn reset_on_an_idle_tool_is_a_no_op() {
        let mut tool = SelectTool::new(SelectMode::Rect);
        tool.reset();
        assert!(tool.gesture.is_none());
        // A fresh gesture still works right after a reset.
        assert_eq!(tool.step(press(2.0, 2.0, mods(false, false))), GestureAction::None);
        assert!(tool.gesture.is_some());
    }

    #[test]
    fn suppressed_frames_freeze_the_gesture_and_the_swallowed_release_commits() {
        let mut tool = SelectTool::new(SelectMode::Lasso);
        assert_eq!(tool.step(press(0.0, 0.0, mods(false, false))), GestureAction::None);
        assert_eq!(tool.step(drag(30.0, 0.0, mods(false, false))), GestureAction::None);
        // The canvas starts panning: the tab stops routing input and freezes the tool instead.
        tool.freeze();
        tool.freeze();
        assert!(tool.gesture.is_some(), "freezing must keep the outline alive");
        // Panning ends after the button was released, so the release event never arrived.
        match tool.step(idle(mods(false, false))) {
            GestureAction::Commit { op, shape } => {
                assert_eq!(op, SelectionOp::Replace);
                assert_eq!(shape, CommitShape::Polygon(vec![(0.0, 0.0), (30.0, 0.0)]));
            }
            other => panic!("a release swallowed by the pan must commit, got {other:?}"),
        }
    }

    #[test]
    fn a_frozen_gesture_still_continues_when_the_button_is_held() {
        let mut tool = SelectTool::new(SelectMode::Lasso);
        assert_eq!(tool.step(press(0.0, 0.0, mods(false, false))), GestureAction::None);
        tool.freeze();
        // Input resumes with the button still down: the drag simply continues.
        assert_eq!(tool.step(drag(30.0, 30.0, mods(false, false))), GestureAction::None);
        assert_eq!(tool.gesture.as_ref().map(|g| g.points.len()), Some(2));
        // The suspension was consumed by that frame, so a genuine interruption still aborts.
        assert_eq!(tool.step(idle(mods(false, false))), GestureAction::Aborted);
        assert!(tool.gesture.is_none());
    }

    #[test]
    fn freeze_does_not_resurrect_an_interrupted_drag() {
        let mut tool = SelectTool::new(SelectMode::Lasso);
        assert_eq!(tool.step(press(0.0, 0.0, mods(false, false))), GestureAction::None);
        // No suppressed frame: the same input pattern must still read as an interruption.
        assert_eq!(tool.step(idle(mods(false, false))), GestureAction::Aborted);
        // Freezing with no gesture leaves nothing armed for the next gesture to trip over.
        tool.freeze();
        assert!(!tool.suspended);
        assert_eq!(tool.step(press(5.0, 5.0, mods(false, false))), GestureAction::None);
        assert_eq!(tool.step(idle(mods(false, false))), GestureAction::Aborted, "the fresh gesture must abort normally");
    }
}
