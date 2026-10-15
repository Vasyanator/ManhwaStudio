/*
File: crates/ms-tab-ps-editor/src/canvas_touch.rs

Purpose:
The PS editor canvas' side of `ms_input::touch` (finger pan / pinch / long-press): publish
the canvas as a touch surface every frame, take the router's output for it, and draw the
long-press feedback marker. The camera math lives in `viewport.rs`
(`PsViewport::apply_touch_navigation`), the finger selection in the tools
(`PsTool::touch_selection`), the menu open path in `draw_selection_menu`.

Key functions:
- `publish_and_take`: one call per drawn canvas frame (publish for the next frame + take-once).
- `tool_frame_route`: whether the active tool gets the finger selection, egui's pointer frame,
  or both (the frame a finger selection is cancelled) this frame.
- `draw_long_press_marker`: the ring shown while a finger's long-press is armed.

Notes:
The surface has NO holes: every dock panel and popup over the canvas is an `Area` above
`Order::Background`, and the router never claims a touch under such a layer. The patch tool's
outline is a bare layer painter without an `Area`, invisible to `Context::layer_id_at`, so a finger
over it is claimed like bare canvas (see `MODULE_README.md` «Finger input»).
*/

use eframe::egui;
use egui::{Context, Id, Painter, Pos2, Rect, Stroke};
use ms_input::touch::{self, TouchOutput, TouchSelection, TouchSelectionEnd, TouchSurface};

/// Radius of the long-press feedback ring, in screen points.
const LONG_PRESS_MARKER_RADIUS: f32 = 18.0;

/// Content-named touch owner id of the PS editor canvas (stable across `StudioApp` rebuilds).
fn owner_id() -> Id {
    Id::new("ps_editor.canvas")
}

/// Publishes the canvas `rect` as this frame's touch surface and takes this frame's output.
///
/// Must run on every frame the canvas is drawn with a page (the router claims next frame only
/// on a surface published this frame, and an untaken output is dropped). `long_press_selection`
/// is whether the ACTIVE tool takes a finger selection drag (`PsTool::accepts_touch_selection`);
/// with `false` the router turns a long-press drag into a pan.
#[must_use]
pub(crate) fn publish_and_take(ctx: &Context, rect: Rect, long_press_selection: bool) -> TouchOutput {
    touch::publish_surface(
        ctx,
        TouchSurface { owner: owner_id(), rect, holes: Vec::new(), long_press_selection, drag_regions: Vec::new() },
    );
    touch::take_touch_output(ctx, owner_id())
}

/// What the active tool is fed on one canvas frame (see [`tool_frame_route`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ToolFrameRoute {
    /// The finger selection to pass to `PsTool::touch_selection` first, if any.
    pub(crate) touch: Option<TouchSelection>,
    /// Whether `PsTool::interact` runs with egui's pointer frame (after `touch`, if both).
    pub(crate) interact: bool,
}

/// Decides how the router's `selection` reaches a tool that does (`accepts_touch_selection`)
/// or does not take finger selections.
///
/// A running finger selection replaces the pointer frame: its button is held but invisible to
/// egui, so `interact` would read it as an interrupted drag. On the frame it is CANCELLED the
/// finger is done, and the cancel is often a mouse/pen press (the router's palm rule) that must
/// start its own drag in that very frame, so both run: the cancel first, then `interact`.
#[must_use]
pub(crate) fn tool_frame_route(selection: Option<TouchSelection>, accepts_touch_selection: bool) -> ToolFrameRoute {
    let touch = selection.filter(|_| accepts_touch_selection);
    let interact = touch.is_none_or(|selection| selection.end == Some(TouchSelectionEnd::Cancelled));
    ToolFrameRoute { touch, interact }
}

/// Draws the "long-press armed" ring at `pos` (screen points): a dark backing under a selection
/// coloured stroke, so it stays legible over any page content.
pub(crate) fn draw_long_press_marker(painter: &Painter, pos: Pos2) {
    painter.circle_stroke(pos, LONG_PRESS_MARKER_RADIUS, Stroke::new(4.0, ms_theme::canvas::OUTLINE_BACKING));
    painter.circle_stroke(pos, LONG_PRESS_MARKER_RADIUS, Stroke::new(2.0, ms_theme::canvas::SELECTION));
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    fn selection(end: Option<TouchSelectionEnd>) -> TouchSelection {
        TouchSelection { origin: pos2(1.0, 1.0), pos: pos2(9.0, 9.0), began: false, end }
    }

    #[test]
    fn running_or_released_finger_selection_replaces_the_pointer_frame() {
        for end in [None, Some(TouchSelectionEnd::Released)] {
            let route = tool_frame_route(Some(selection(end)), true);
            assert_eq!(route, ToolFrameRoute { touch: Some(selection(end)), interact: false }, "{end:?}");
        }
    }

    #[test]
    fn cancelled_finger_selection_lets_the_cancelling_press_reach_interact() {
        let cancelled = selection(Some(TouchSelectionEnd::Cancelled));
        assert_eq!(tool_frame_route(Some(cancelled), true), ToolFrameRoute { touch: Some(cancelled), interact: true });
    }

    #[test]
    fn no_selection_or_a_tool_without_finger_selection_gets_the_pointer_frame() {
        assert_eq!(tool_frame_route(None, true), ToolFrameRoute { touch: None, interact: true });
        assert_eq!(tool_frame_route(Some(selection(None)), false), ToolFrameRoute { touch: None, interact: true });
    }
}
