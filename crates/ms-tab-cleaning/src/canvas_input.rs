/*
File: crates/ms-tab-cleaning/src/canvas_input.rs

Purpose:
The stroke driver of the «Клининг» canvas: turns this frame's canvas-wide claim
(`InputFrame::claimed(canvas view)`, one of `input::STROKE_ACTIONS` / `input::PRESS_ACTIONS`) into
the active tool's stroke calls. It replaces the tab's former raw pointer / modifier reads and the
finger-selection replay: a mouse drag and a finger long-press drag are the same claim here.

Key structures:
- `ClaimSample`: the part of a `ms_input::ClaimedGesture` the driver needs (action, phase,
  position, the sampled modes, whether a finger drives it).
- `StrokeEvent`: what the tab must do to the tool this frame.
- `StrokeDriver`: the per-tab state machine (idle / waiting for a page / stroking / ignoring the
  rest of a claim).

Key functions:
- `claim_sample`: picks the claim the driver owns out of an `InputFrame`.
- `StrokeDriver::step`: pure decision of one frame, unit-tested.
- `StrokeDriver::abort`: the tab ends a running stroke on its own (tool switch).

Rules (they keep the old dispatch's behaviour):
- a stroke begins at the first claimed position over a page that the tool accepts
  (`CleaningTool::wants_primary_stroke`), checked once; a refused claim is ignored to its end;
- a point off every page is skipped; a point equal to the last one is skipped; a point on another
  page ends the stroke and begins a new one there (`StrokeEvent::Seam`);
- a press action (`PICK_COLOR`, `STAMP_SET_ANCHOR`) fires once at the claim's `Started` frame;
- while BLOCKED (Space-pan held, or a mouse pointer that left the canvas view for a dock panel or
  a floating window) no stroke starts, and a running one ends;
- the claim's release commits (`End`), a cancelled claim (focus loss, a second finger) cancels.
*/

use eframe::egui::Pos2;
use ms_input::{ActionId, Button, GesturePhase, InputFrame, ReleasePolicy, ViewId};

use crate::input::{PRESS_ACTIONS, STROKE_ACTIONS, StrokeModes};
use crate::tools::StrokePoint;

/// The claim the driver acts on this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ClaimSample {
    pub(crate) action: ActionId,
    pub(crate) phase: GesturePhase,
    /// The latest pointer (or finger) position, screen points.
    pub(crate) pos: Pos2,
    /// The accepted modes as the claim sampled them.
    pub(crate) modes: StrokeModes,
    /// A finger drives the claim (no mouse pointer exists for it).
    pub(crate) touch: bool,
}

/// The claim on `view` this frame when it is one the driver owns (a stroke or a press action);
/// a zoom drag or another viewer's claim is `None`.
#[must_use]
pub(crate) fn claim_sample(input: &InputFrame, view: ViewId) -> Option<ClaimSample> {
    let claim = input.claimed(view)?;
    if !STROKE_ACTIONS.contains(&claim.action) && !PRESS_ACTIONS.contains(&claim.action) {
        return None;
    }
    Some(ClaimSample {
        action: claim.action,
        phase: claim.phase,
        pos: claim.current,
        modes: StrokeModes::from_params(&claim.params),
        touch: matches!(claim.button, Button::Touch(_)),
    })
}

/// What the tab does to the active tool this frame, in order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum StrokeEvent {
    /// A press action fired on a page (`CleaningTool::on_page_press`).
    Press { action: ActionId, point: StrokePoint },
    /// `CleaningTool::stroke_begin`.
    Begin(StrokePoint),
    /// `CleaningTool::stroke_update`.
    Update { from: StrokePoint, to: StrokePoint },
    /// The stroke crossed a page seam: `stroke_end`, then `stroke_begin` at the point.
    Seam(StrokePoint),
    /// `CleaningTool::stroke_end` (commit).
    End,
    /// `CleaningTool::stroke_cancel`.
    Cancel,
}

/// The driver's state between frames.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum DriverState {
    /// No claim of ours runs.
    #[default]
    Idle,
    /// A stroke claim runs but has not reached a page yet.
    Pending,
    /// A stroke runs; `last` is its last delivered point.
    Stroking { last: StrokePoint, touch: bool },
    /// The rest of the running claim does nothing (refused, blocked, a press action, aborted).
    Ignoring,
}

/// The per-tab stroke state machine. See the file header for the rules.
#[derive(Debug, Default)]
pub(crate) struct StrokeDriver {
    state: DriverState,
}

impl StrokeDriver {
    /// Whether a stroke runs (the tool has had `stroke_begin` and no end yet).
    #[must_use]
    pub(crate) fn stroking(&self) -> bool {
        matches!(self.state, DriverState::Stroking { .. })
    }

    /// The last point of the running stroke.
    #[must_use]
    pub(crate) fn last_point(&self) -> Option<StrokePoint> {
        match self.state {
            DriverState::Stroking { last, .. } => Some(last),
            DriverState::Idle | DriverState::Pending | DriverState::Ignoring => None,
        }
    }

    /// The last point of a running FINGER stroke (the cursor anchor: egui has no pointer for it).
    #[must_use]
    pub(crate) fn touch_point(&self) -> Option<StrokePoint> {
        match self.state {
            DriverState::Stroking { last, touch: true } => Some(last),
            DriverState::Stroking { touch: false, .. } | DriverState::Idle | DriverState::Pending | DriverState::Ignoring => None,
        }
    }

    /// Stops driving the running claim (the tab ended the stroke itself, e.g. on a tool switch).
    /// Returns whether a stroke was running, i.e. whether the caller owes the tool a
    /// `stroke_end`.
    pub(crate) fn abort(&mut self) -> bool {
        let was_stroking = self.stroking();
        if !matches!(self.state, DriverState::Idle) {
            self.state = DriverState::Ignoring;
        }
        was_stroking
    }

    /// Decides one frame.
    ///
    /// `claim` is this frame's [`claim_sample`]; `blocked` says no stroke may run now (Space-pan
    /// held, or a mouse pointer off the canvas view); `page_at(pos, preferred)` answers the page
    /// under a screen position, keeping `preferred` while it still contains it; `wants` is the
    /// tool's veto on a stroke's first point.
    pub(crate) fn step(
        &mut self,
        claim: Option<ClaimSample>,
        blocked: bool,
        page_at: impl Fn(Pos2, Option<usize>) -> Option<usize>,
        mut wants: impl FnMut(StrokePoint) -> bool,
    ) -> Vec<StrokeEvent> {
        let mut out = Vec::new();
        let Some(claim) = claim else {
            // The claim vanished without a finish (suppressed input, a modal prompt): commit
            // what was drawn, as a release would.
            if self.stroking() {
                out.push(StrokeEvent::End);
            }
            self.state = DriverState::Idle;
            return out;
        };
        let point_at = |page_idx: usize| StrokePoint { page_idx, scene_pos: claim.pos, modes: claim.modes };
        match claim.phase {
            GesturePhase::Finished(policy) => {
                if self.stroking() {
                    out.push(match policy {
                        ReleasePolicy::Commit => StrokeEvent::End,
                        ReleasePolicy::Cancel => StrokeEvent::Cancel,
                    });
                }
                self.state = DriverState::Idle;
                return out;
            }
            GesturePhase::Started => {
                // A new press: a stroke left over from a claim whose end was never seen ends first.
                if self.stroking() {
                    out.push(StrokeEvent::End);
                }
                if blocked {
                    self.state = DriverState::Ignoring;
                    return out;
                }
                if PRESS_ACTIONS.contains(&claim.action) {
                    if let Some(page_idx) = page_at(claim.pos, None) {
                        out.push(StrokeEvent::Press { action: claim.action, point: point_at(page_idx) });
                    }
                    self.state = DriverState::Ignoring;
                    return out;
                }
                self.state = DriverState::Pending;
            }
            GesturePhase::Active => {}
        }
        match self.state {
            // `Idle` on an `Active` claim: it started while the driver was not watching (another
            // tab was active); it is not ours to pick up halfway.
            DriverState::Idle | DriverState::Ignoring => {}
            DriverState::Pending => {
                if blocked {
                    self.state = DriverState::Ignoring;
                } else if let Some(page_idx) = page_at(claim.pos, None) {
                    let point = point_at(page_idx);
                    if wants(point) {
                        out.push(StrokeEvent::Begin(point));
                        self.state = DriverState::Stroking { last: point, touch: claim.touch };
                    } else {
                        self.state = DriverState::Ignoring;
                    }
                }
            }
            DriverState::Stroking { last, touch } => {
                if blocked {
                    out.push(StrokeEvent::End);
                    self.state = DriverState::Ignoring;
                } else if let Some(page_idx) = page_at(claim.pos, Some(last.page_idx))
                    && claim.pos != last.scene_pos
                {
                    let point = point_at(page_idx);
                    out.push(if page_idx == last.page_idx { StrokeEvent::Update { from: last, to: point } } else { StrokeEvent::Seam(point) });
                    self.state = DriverState::Stroking { last: point, touch };
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{PICK_COLOR, STROKE};
    use eframe::egui::pos2;

    /// Two pages stacked: page 0 is y < 100, page 1 is 100 <= y < 200; nothing below.
    fn page_at(pos: Pos2, preferred: Option<usize>) -> Option<usize> {
        let page = |y: f32| if (0.0..100.0).contains(&y) { Some(0) } else if (100.0..200.0).contains(&y) { Some(1) } else { None };
        preferred.filter(|idx| page(pos.y) == Some(*idx)).or_else(|| page(pos.y))
    }

    fn sample(action: ActionId, phase: GesturePhase, pos: Pos2) -> Option<ClaimSample> {
        Some(ClaimSample { action, phase, pos, modes: StrokeModes::default(), touch: false })
    }

    fn step(driver: &mut StrokeDriver, claim: Option<ClaimSample>, blocked: bool) -> Vec<StrokeEvent> {
        driver.step(claim, blocked, page_at, |_| true)
    }

    fn kinds(events: &[StrokeEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|event| match event {
                StrokeEvent::Press { .. } => "press",
                StrokeEvent::Begin(_) => "begin",
                StrokeEvent::Update { .. } => "update",
                StrokeEvent::Seam(_) => "seam",
                StrokeEvent::End => "end",
                StrokeEvent::Cancel => "cancel",
            })
            .collect()
    }

    #[test]
    fn a_drag_begins_updates_and_commits() {
        let mut driver = StrokeDriver::default();
        assert_eq!(kinds(&step(&mut driver, sample(STROKE, GesturePhase::Started, pos2(10.0, 10.0)), false)), ["begin"]);
        assert_eq!(kinds(&step(&mut driver, sample(STROKE, GesturePhase::Active, pos2(10.0, 10.0)), false)), Vec::<&str>::new(), "an unmoved point is skipped");
        let events = step(&mut driver, sample(STROKE, GesturePhase::Active, pos2(20.0, 30.0)), false);
        assert!(matches!(events.as_slice(), [StrokeEvent::Update { from, to }] if from.scene_pos == pos2(10.0, 10.0) && to.scene_pos == pos2(20.0, 30.0)));
        assert_eq!(kinds(&step(&mut driver, sample(STROKE, GesturePhase::Finished(ReleasePolicy::Commit), pos2(20.0, 30.0)), false)), ["end"]);
        assert!(!driver.stroking());
    }

    #[test]
    fn crossing_a_page_seam_restarts_the_stroke_and_off_page_points_are_skipped() {
        let mut driver = StrokeDriver::default();
        step(&mut driver, sample(STROKE, GesturePhase::Started, pos2(10.0, 90.0)), false);
        let events = step(&mut driver, sample(STROKE, GesturePhase::Active, pos2(10.0, 120.0)), false);
        assert!(matches!(events.as_slice(), [StrokeEvent::Seam(point)] if point.page_idx == 1));
        assert!(step(&mut driver, sample(STROKE, GesturePhase::Active, pos2(10.0, 500.0)), false).is_empty(), "off every page");
        assert_eq!(driver.last_point().map(|point| point.page_idx), Some(1));
    }

    #[test]
    fn a_press_off_the_pages_waits_for_the_first_page() {
        let mut driver = StrokeDriver::default();
        assert!(step(&mut driver, sample(STROKE, GesturePhase::Started, pos2(10.0, 500.0)), false).is_empty());
        assert_eq!(kinds(&step(&mut driver, sample(STROKE, GesturePhase::Active, pos2(10.0, 50.0)), false)), ["begin"]);
    }

    #[test]
    fn a_refused_stroke_is_ignored_to_its_end() {
        let mut driver = StrokeDriver::default();
        assert!(driver.step(sample(STROKE, GesturePhase::Started, pos2(10.0, 10.0)), false, page_at, |_| false).is_empty());
        assert!(driver.step(sample(STROKE, GesturePhase::Active, pos2(30.0, 10.0)), false, page_at, |_| true).is_empty(), "the veto is asked once");
        assert!(step(&mut driver, sample(STROKE, GesturePhase::Finished(ReleasePolicy::Commit), pos2(30.0, 10.0)), false).is_empty());
    }

    #[test]
    fn blocking_ends_a_stroke_and_stops_a_new_one() {
        let mut driver = StrokeDriver::default();
        step(&mut driver, sample(STROKE, GesturePhase::Started, pos2(10.0, 10.0)), false);
        assert_eq!(kinds(&step(&mut driver, sample(STROKE, GesturePhase::Active, pos2(20.0, 10.0)), true)), ["end"], "Space pressed mid-stroke");
        assert!(step(&mut driver, sample(STROKE, GesturePhase::Active, pos2(30.0, 10.0)), false).is_empty(), "the claim stays ignored");
        assert!(step(&mut driver, sample(STROKE, GesturePhase::Started, pos2(10.0, 10.0)), true).is_empty(), "a press while panning starts nothing");
    }

    #[test]
    fn a_press_action_fires_once_at_the_press() {
        let mut driver = StrokeDriver::default();
        let events = step(&mut driver, sample(PICK_COLOR, GesturePhase::Started, pos2(10.0, 10.0)), false);
        assert!(matches!(events.as_slice(), [StrokeEvent::Press { action, point }] if *action == PICK_COLOR && point.page_idx == 0));
        assert!(step(&mut driver, sample(PICK_COLOR, GesturePhase::Active, pos2(40.0, 10.0)), false).is_empty());
        assert!(step(&mut driver, sample(PICK_COLOR, GesturePhase::Started, pos2(10.0, 500.0)), false).is_empty(), "off the pages nothing fires");
    }

    #[test]
    fn a_cancelled_claim_cancels_and_a_vanished_one_commits() {
        let mut driver = StrokeDriver::default();
        step(&mut driver, sample(STROKE, GesturePhase::Started, pos2(10.0, 10.0)), false);
        assert_eq!(kinds(&step(&mut driver, sample(STROKE, GesturePhase::Finished(ReleasePolicy::Cancel), pos2(10.0, 10.0)), false)), ["cancel"]);
        step(&mut driver, sample(STROKE, GesturePhase::Started, pos2(10.0, 10.0)), false);
        assert_eq!(kinds(&step(&mut driver, None, false)), ["end"]);
    }

    #[test]
    fn modes_ride_on_every_point_and_a_finger_is_remembered() {
        let mut driver = StrokeDriver::default();
        let mut claim = ClaimSample { action: STROKE, phase: GesturePhase::Started, pos: pos2(10.0, 10.0), modes: StrokeModes { erase: false, rect: true }, touch: true };
        let events = step(&mut driver, Some(claim), false);
        assert!(matches!(events.as_slice(), [StrokeEvent::Begin(point)] if point.modes.rect && !point.modes.erase));
        assert_eq!(driver.touch_point().map(|point| point.scene_pos), Some(pos2(10.0, 10.0)));
        claim.phase = GesturePhase::Active;
        claim.pos = pos2(15.0, 10.0);
        claim.modes.erase = true;
        let events = step(&mut driver, Some(claim), false);
        assert!(matches!(events.as_slice(), [StrokeEvent::Update { to, .. }] if to.modes.erase), "the latest erase mode");
    }

    #[test]
    fn abort_owes_one_end_and_ignores_the_rest_of_the_claim() {
        let mut driver = StrokeDriver::default();
        assert!(!driver.abort(), "nothing runs");
        step(&mut driver, sample(STROKE, GesturePhase::Started, pos2(10.0, 10.0)), false);
        assert!(driver.abort());
        assert!(step(&mut driver, sample(STROKE, GesturePhase::Active, pos2(30.0, 10.0)), false).is_empty());
        assert!(step(&mut driver, sample(STROKE, GesturePhase::Finished(ReleasePolicy::Commit), pos2(30.0, 10.0)), false).is_empty(), "no second end");
    }
}
