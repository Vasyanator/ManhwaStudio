/*
File: crates/ms-tab-cleaning/src/touch_selection.rs

Purpose:
Replays a finger long-press selection drag (`ms_widgets::touch_nav::TouchSelection`, read from
`CanvasView::touch_selection`) onto the active cleaning tool through the tool's ordinary stroke
hooks, for a tool that declares it (`CleaningTool::accepts_touch_selection`, the region tools).

Key functions:
- `drive_tool_touch_selection`: one frame of the mapping (begin at the press point, follow,
  commit on lift through `stroke_end`, abort on cancel through `cancel_touch_selection`).

Notes:
The finger has its OWN stroke state (the last replayed point), apart from the tab's mouse
stroke (`stroke_active` / `last_stroke_point`): the mouse path ends its stroke on many pointer
conditions (button released, pointer occluded, Space held) that say nothing about a finger the
router claimed. It is never a synthesized mouse press — the mouse buttons paint and erase here.
`wants_primary_stroke` is skipped on purpose: the Shift the mouse needs to start a region
selection does not apply to a finger (the user decision recorded in `MODULE_README.md`).
*/

use crate::tools::{CleaningTool, StrokeModifiers, StrokePoint};
use eframe::egui::Pos2;
use ms_canvas::CanvasView;
use ms_widgets::touch_nav::{TouchSelection, TouchSelectionEnd};

/// Applies one frame of the finger selection drag `selection` to `tool`; returns whether
/// anything was replayed (the caller requests a repaint then).
///
/// `last` is the finger's stroke state kept by the tab across frames (`None` = no finger
/// selection in flight). `page_at(canvas, previous_page, pos)` resolves the page under `pos`
/// exactly like the mouse path does: the previous page while it still contains `pos`, else the
/// page under it, `None` off every page.
///
/// - `began`: when no finger selection is in flight and the tool accepts one, the drag
///   begins at the press point (`stroke_begin`); a press off every page starts nothing and
///   the rest of the gesture is ignored.
/// - every frame: a move to a new position is replayed as `stroke_update` on the same page,
///   or as `stroke_end` + `stroke_begin` across pages (what the mouse path does).
/// - `Released`: `stroke_end`, which commits the selection like a mouse release.
/// - `Cancelled`: `cancel_touch_selection`, which drops the drag without committing it.
pub(crate) fn drive_tool_touch_selection(
    tool: &mut dyn CleaningTool,
    canvas: &mut CanvasView,
    last: &mut Option<StrokePoint>,
    selection: TouchSelection,
    page_at: impl Fn(&CanvasView, Option<usize>, Pos2) -> Option<usize>,
) -> bool {
    let mut replayed = false;
    if selection.began
        && last.is_none()
        && tool.accepts_touch_selection()
        && let Some(page_idx) = page_at(canvas, None, selection.origin)
    {
        let point = touch_point(page_idx, selection.origin);
        tool.stroke_begin(canvas, point);
        *last = Some(point);
        replayed = true;
    }
    let Some(prev) = *last else {
        return replayed;
    };
    if selection.pos != prev.scene_pos
        && let Some(page_idx) = page_at(canvas, Some(prev.page_idx), selection.pos)
    {
        let point = touch_point(page_idx, selection.pos);
        if page_idx == prev.page_idx {
            tool.stroke_update(canvas, prev, point);
        } else {
            tool.stroke_end(canvas);
            tool.stroke_begin(canvas, point);
        }
        *last = Some(point);
        replayed = true;
    }
    match selection.end {
        None => {}
        Some(TouchSelectionEnd::Released) => {
            *last = None;
            tool.stroke_end(canvas);
            replayed = true;
        }
        Some(TouchSelectionEnd::Cancelled) => {
            *last = None;
            tool.cancel_touch_selection();
            replayed = true;
        }
    }
    replayed
}

/// A finger stroke point: no modifiers (a finger has none; the region tools need no Shift
/// from it).
fn touch_point(page_idx: usize, scene_pos: Pos2) -> StrokePoint {
    StrokePoint { page_idx, scene_pos, modifiers: StrokeModifiers::default() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::pos2;

    /// What a recording tool saw, in order.
    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Begin(usize, Pos2),
        Update(Pos2, Pos2),
        End,
        Cancel,
    }

    struct RecordingTool {
        accepts: bool,
        calls: Vec<Call>,
    }

    impl CleaningTool for RecordingTool {
        fn tool_id(&self) -> &'static str {
            "test.recording"
        }

        fn title(&self) -> &'static str {
            "recording"
        }

        fn stroke_begin(&mut self, _canvas: &mut CanvasView, point: StrokePoint) {
            assert!(!point.modifiers.shift && !point.modifiers.ctrl, "a finger has no modifiers");
            self.calls.push(Call::Begin(point.page_idx, point.scene_pos));
        }

        fn stroke_update(&mut self, _canvas: &mut CanvasView, from: StrokePoint, to: StrokePoint) {
            self.calls.push(Call::Update(from.scene_pos, to.scene_pos));
        }

        fn stroke_end(&mut self, _canvas: &mut CanvasView) {
            self.calls.push(Call::End);
        }

        fn accepts_touch_selection(&self) -> bool {
            self.accepts
        }

        fn cancel_touch_selection(&mut self) {
            self.calls.push(Call::Cancel);
        }
    }

    /// Page 0 left of x = 100, page 1 from there to x = 200, nothing beyond; the previous page
    /// wins while it contains the point (the mouse path's rule).
    fn page_at(_canvas: &CanvasView, previous: Option<usize>, pos: Pos2) -> Option<usize> {
        let page = |x: f32| {
            if (0.0..100.0).contains(&x) {
                Some(0)
            } else if (100.0..200.0).contains(&x) {
                Some(1)
            } else {
                None
            }
        };
        previous.filter(|prev| page(pos.x) == Some(*prev)).or_else(|| page(pos.x))
    }

    fn sel(origin: Pos2, pos: Pos2, began: bool, end: Option<TouchSelectionEnd>) -> TouchSelection {
        TouchSelection { origin, pos, began, end }
    }

    fn run(tool: &mut RecordingTool, frames: &[TouchSelection]) -> Option<StrokePoint> {
        let mut canvas = CanvasView::default();
        let mut last = None;
        for frame in frames {
            drive_tool_touch_selection(tool, &mut canvas, &mut last, *frame, page_at);
        }
        last
    }

    #[test]
    fn a_finger_selection_begins_at_the_press_point_and_commits_on_lift() {
        let mut tool = RecordingTool { accepts: true, calls: Vec::new() };
        let origin = pos2(10.0, 10.0);
        let last = run(&mut tool, &[
            sel(origin, pos2(30.0, 20.0), true, None),
            sel(origin, pos2(30.0, 20.0), false, None),
            sel(origin, pos2(50.0, 40.0), false, Some(TouchSelectionEnd::Released)),
        ]);
        assert_eq!(tool.calls, vec![
            Call::Begin(0, origin),
            Call::Update(origin, pos2(30.0, 20.0)),
            Call::Update(pos2(30.0, 20.0), pos2(50.0, 40.0)),
            Call::End,
        ]);
        assert!(last.is_none());
    }

    #[test]
    fn a_cancelled_finger_selection_aborts_without_a_commit() {
        let mut tool = RecordingTool { accepts: true, calls: Vec::new() };
        let origin = pos2(10.0, 10.0);
        let last = run(&mut tool, &[
            sel(origin, pos2(20.0, 20.0), true, None),
            sel(origin, pos2(25.0, 25.0), false, Some(TouchSelectionEnd::Cancelled)),
        ]);
        assert_eq!(tool.calls, vec![
            Call::Begin(0, origin),
            Call::Update(origin, pos2(20.0, 20.0)),
            Call::Update(pos2(20.0, 20.0), pos2(25.0, 25.0)),
            Call::Cancel,
        ]);
        assert!(last.is_none());
    }

    #[test]
    fn a_tool_without_a_selection_or_a_press_off_the_pages_gets_nothing() {
        let mut painter = RecordingTool { accepts: false, calls: Vec::new() };
        let frames = [
            sel(pos2(10.0, 10.0), pos2(20.0, 20.0), true, None),
            sel(pos2(10.0, 10.0), pos2(30.0, 30.0), false, Some(TouchSelectionEnd::Released)),
        ];
        assert!(run(&mut painter, &frames).is_none());
        assert!(painter.calls.is_empty(), "a painting tool is never driven by a finger");

        let mut region = RecordingTool { accepts: true, calls: Vec::new() };
        let off_page = [
            sel(pos2(500.0, 10.0), pos2(20.0, 20.0), true, None),
            sel(pos2(500.0, 10.0), pos2(30.0, 30.0), false, Some(TouchSelectionEnd::Released)),
        ];
        assert!(run(&mut region, &off_page).is_none());
        assert!(region.calls.is_empty(), "a press off every page starts nothing");
    }

    #[test]
    fn crossing_pages_ends_and_restarts_like_the_mouse_and_off_page_moves_are_skipped() {
        let mut tool = RecordingTool { accepts: true, calls: Vec::new() };
        let origin = pos2(90.0, 10.0);
        let last = run(&mut tool, &[
            sel(origin, pos2(250.0, 10.0), true, None),
            sel(origin, pos2(150.0, 10.0), false, None),
        ]);
        assert_eq!(tool.calls, vec![Call::Begin(0, origin), Call::End, Call::Begin(1, pos2(150.0, 10.0))]);
        assert_eq!(last.map(|point| (point.page_idx, point.scene_pos)), Some((1, pos2(150.0, 10.0))));
    }
}
