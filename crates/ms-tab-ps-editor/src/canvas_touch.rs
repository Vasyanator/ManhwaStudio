/*
File: crates/ms-tab-ps-editor/src/canvas_touch.rs

Purpose:
The PS editor canvas' side of `ms_widgets::touch_nav` (finger pan / pinch / long-press): publish
the canvas as a touch surface every frame, take the router's output for it, and draw the
long-press feedback marker. The camera math lives in `viewport.rs`
(`PsViewport::apply_touch_navigation`), the finger selection in the tools
(`PsTool::touch_selection`), the menu open path in `draw_selection_menu`.

Key functions:
- `publish_and_take`: one call per drawn canvas frame (publish for the next frame + take-once).
- `draw_long_press_marker`: the ring shown while a finger's long-press is armed.

Notes:
The surface has NO holes: every dock panel, popup and region-tool `Area` over the canvas sits on
a layer above `Order::Background`, and the router never claims a touch under such a layer.
*/

use eframe::egui;
use egui::{Context, Id, Painter, Pos2, Rect, Stroke};
use ms_widgets::touch_nav::{self, TouchOutput, TouchSurface};

/// Radius of the long-press feedback ring, in screen points.
const LONG_PRESS_MARKER_RADIUS: f32 = 18.0;

/// Content-named touch owner id of the PS editor canvas (stable across `MangaApp` rebuilds).
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
    touch_nav::publish_surface(
        ctx,
        TouchSurface { owner: owner_id(), rect, holes: Vec::new(), long_press_selection },
    );
    touch_nav::take_touch_output(ctx, owner_id())
}

/// Draws the "long-press armed" ring at `pos` (screen points): a dark backing under a selection
/// coloured stroke, so it stays legible over any page content.
pub(crate) fn draw_long_press_marker(painter: &Painter, pos: Pos2) {
    painter.circle_stroke(pos, LONG_PRESS_MARKER_RADIUS, Stroke::new(4.0, ms_theme::canvas::OUTLINE_BACKING));
    painter.circle_stroke(pos, LONG_PRESS_MARKER_RADIUS, Stroke::new(2.0, ms_theme::canvas::SELECTION));
}
