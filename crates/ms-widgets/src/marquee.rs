/*
File: crates/ms-widgets/src/marquee.rs

Purpose:
Web-style "marquee" for a single-line label that does not fit its rect: the text rests at its
start, scrolls left until its end is visible, rests briefly, jumps back and repeats. Text that
fits is painted statically with the caller's alignment and costs no repaint.

Key items:
- `MarqueeTiming`: the pause / speed / pause cycle (validated constructor + `DEFAULT`).
- `MarqueeFrame` + `marquee_frame()`: the pure phase function — offset and next repaint delay
  for a given overflow and time. Unit-tested here.
- `paint_marquee_galley()`: paints a pre-laid-out galley clipped to a rect and schedules the
  next repaint only while the text overflows AND the rect is visible.

Notes:
Paint-only: it allocates no space and registers no hitbox, so it can draw the label of any
custom-painted button without changing that button's click rect. Colours and font are the
caller's (baked into the galley), so the helper carries no theme. The phase comes from the
absolute `InputState::time`, so it keeps no per-widget state and needs no id.
*/

use std::sync::Arc;
use std::time::Duration;

use egui::{Align, Galley, Rect, Ui, pos2};

/// Repaint interval while the text is moving (~60 fps). Also the cap on how stale a frame of a
/// moving marquee may get.
const SCROLL_FRAME: Duration = Duration::from_millis(16);

/// One marquee cycle: rest at the start, scroll at constant speed until the end of the text is
/// visible, rest at the end, then jump back to the start.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarqueeTiming {
    start_pause_s: f32,
    speed_pts_per_s: f32,
    end_pause_s: f32,
}

impl MarqueeTiming {
    /// The project default: 1.5 s at the start (enough to read the beginning), 35 pt/s, 0.8 s
    /// at the end.
    pub const DEFAULT: Self = Self { start_pause_s: 1.5, speed_pts_per_s: 35.0, end_pause_s: 0.8 };

    /// Builds a timing. Returns `None` when a pause is negative or not finite, or the speed is
    /// not a finite positive number of points per second.
    #[must_use]
    pub fn new(start_pause_s: f32, speed_pts_per_s: f32, end_pause_s: f32) -> Option<Self> {
        let pause_ok = |pause: f32| pause.is_finite() && pause >= 0.0;
        (pause_ok(start_pause_s) && pause_ok(end_pause_s) && speed_pts_per_s.is_finite() && speed_pts_per_s > 0.0)
            .then_some(Self { start_pause_s, speed_pts_per_s, end_pause_s })
    }
}

impl Default for MarqueeTiming {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The marquee state for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarqueeFrame {
    /// How far the text is shifted left, in points, within `0.0..=overflow`.
    pub offset: f32,
    /// When the picture changes next; `None` means it never changes (the text fits).
    pub repaint_after: Option<Duration>,
}

impl MarqueeFrame {
    const STATIC: Self = Self { offset: 0.0, repaint_after: None };
}

/// Pure marquee phase: for text `overflow` points wider than its rect at absolute time
/// `time_s` (seconds), returns the left shift and when to repaint next. `overflow <= 0` (or NaN)
/// is the "fits" case: offset 0 and no repaint. A non-finite `time_s` is treated the same way,
/// since no phase can be derived from it.
#[must_use]
pub fn marquee_frame(overflow: f32, time_s: f64, timing: &MarqueeTiming) -> MarqueeFrame {
    if !(overflow > 0.0 && overflow.is_finite()) || !time_s.is_finite() {
        return MarqueeFrame::STATIC;
    }
    let scroll_s = overflow / timing.speed_pts_per_s;
    let scroll_end_s = timing.start_pause_s + scroll_s;
    let cycle_s = scroll_end_s + timing.end_pause_s;
    // The phase is in `0..cycle_s`, a few seconds at most, so narrowing it to f32 keeps far more
    // precision than one frame needs; the absolute f64 time is reduced first for that reason
    // (there is no lossless f64 -> f32 conversion, and this one is bounded).
    let phase_s = time_s.rem_euclid(f64::from(cycle_s)) as f32;
    if phase_s < timing.start_pause_s {
        MarqueeFrame { offset: 0.0, repaint_after: Some(secs(timing.start_pause_s - phase_s)) }
    } else if phase_s < scroll_end_s {
        let offset = ((phase_s - timing.start_pause_s) * timing.speed_pts_per_s).min(overflow);
        MarqueeFrame { offset, repaint_after: Some(SCROLL_FRAME) }
    } else {
        MarqueeFrame { offset: overflow, repaint_after: Some(secs(cycle_s - phase_s)) }
    }
}

/// A pause remainder as a `Duration`. The remainder is positive and finite by construction
/// (validated timing, finite phase); should rounding ever produce something unrepresentable,
/// one scroll frame keeps the animation alive instead of freezing it.
fn secs(remaining_s: f32) -> Duration {
    Duration::try_from_secs_f32(remaining_s.max(0.0)).unwrap_or(SCROLL_FRAME)
}

/// Paints `galley` (single line, already coloured — build it with `Painter::layout_no_wrap`)
/// inside `rect`, vertically centred and clipped to `rect`. When it fits, it is placed by
/// `static_align` and nothing is scheduled; when it is wider, it runs the `timing` marquee and
/// requests the next repaint through `request_repaint_after`, but only while `rect` is visible.
/// Paint only: no space is allocated and no input is sensed.
pub fn paint_marquee_galley(ui: &Ui, rect: Rect, galley: Arc<Galley>, static_align: Align, timing: &MarqueeTiming) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    let text_size = galley.size();
    let overflow = text_size.x - rect.width();
    let y = rect.center().y - text_size.y * 0.5;
    let x = if overflow > 0.0 {
        let frame = marquee_frame(overflow, ui.input(|input| input.time), timing);
        if let Some(delay) = frame.repaint_after {
            ui.ctx().request_repaint_after(delay);
        }
        rect.left() - frame.offset
    } else {
        rect.left() + (rect.width() - text_size.x) * static_align.to_factor()
    };
    ui.painter().with_clip_rect(rect).galley(pos2(x, y), galley, ui.visuals().text_color());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1.5 s rest, 40 pt/s, 0.5 s rest; with 80 pt overflow the scroll takes 2 s and the cycle
    /// is 4 s.
    fn timing() -> MarqueeTiming {
        MarqueeTiming::new(1.5, 40.0, 0.5).expect("valid test timing")
    }

    fn assert_frame(time_s: f64, offset: f32, repaint_s: f32) {
        let frame = marquee_frame(80.0, time_s, &timing());
        assert!((frame.offset - offset).abs() < 1e-3, "t={time_s}: offset {} != {offset}", frame.offset);
        let repaint = frame.repaint_after.expect("an overflowing marquee always schedules a repaint").as_secs_f32();
        assert!((repaint - repaint_s).abs() < 1e-3, "t={time_s}: repaint {repaint} != {repaint_s}");
    }

    #[test]
    fn rests_at_the_start_and_wakes_when_the_pause_ends() {
        assert_frame(0.0, 0.0, 1.5);
        assert_frame(1.0, 0.0, 0.5);
    }

    #[test]
    fn scrolls_at_constant_speed_repainting_every_frame() {
        let frame_s = SCROLL_FRAME.as_secs_f32();
        assert_frame(1.5, 0.0, frame_s);
        assert_frame(2.5, 40.0, frame_s);
        assert_frame(3.25, 70.0, frame_s);
    }

    #[test]
    fn rests_at_the_end_with_the_whole_overflow_revealed() {
        assert_frame(3.5, 80.0, 0.5);
        assert_frame(3.75, 80.0, 0.25);
    }

    #[test]
    fn wraps_back_to_the_start_each_cycle() {
        assert_frame(4.0, 0.0, 1.5);
        assert_frame(4.0 * 1000.0 + 2.5, 40.0, SCROLL_FRAME.as_secs_f32());
    }

    #[test]
    fn text_that_fits_is_static_and_never_repaints() {
        for overflow in [0.0, -12.0, f32::NAN, f32::INFINITY] {
            assert_eq!(marquee_frame(overflow, 2.5, &timing()), MarqueeFrame::STATIC, "overflow {overflow}");
        }
        assert_eq!(marquee_frame(80.0, f64::NAN, &timing()), MarqueeFrame::STATIC);
    }

    #[test]
    fn invalid_timings_are_rejected() {
        assert!(MarqueeTiming::new(-1.0, 40.0, 0.5).is_none());
        assert!(MarqueeTiming::new(1.0, 0.0, 0.5).is_none());
        assert!(MarqueeTiming::new(1.0, f32::NAN, 0.5).is_none());
        assert!(MarqueeTiming::new(1.0, 40.0, f32::INFINITY).is_none());
        assert_eq!(MarqueeTiming::new(1.5, 35.0, 0.8), Some(MarqueeTiming::DEFAULT));
    }
}
