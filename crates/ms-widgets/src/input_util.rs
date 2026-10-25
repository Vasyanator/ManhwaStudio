/*
File: input_util.rs

Purpose:
Small shared egui input helpers used by the canvas and tab wheel/zoom handlers.

Key functions:
- raw_wheel_delta(): unsmoothed per-frame mouse-wheel delta (Ctrl/Cmd-safe).

Notes:
egui 0.35 removed `InputState::raw_scroll_delta`; this reconstructs it from the
frame's `Event::MouseWheel` events so Ctrl+wheel zoom keeps working.
*/
//! Shared egui input helpers.

/// Raw, unsmoothed mouse-wheel delta for the current frame, summed from this
/// frame's [`egui::Event::MouseWheel`] events.
///
/// Restores the semantics of egui's removed `InputState::raw_scroll_delta`: it
/// stays non-zero while the zoom modifier (Ctrl/Cmd) is held. Under that modifier
/// egui diverts the smoothed scroll into `zoom_delta` and leaves
/// `smooth_scroll_delta` at zero, so custom Ctrl+wheel zoom/rotate logic must read
/// the wheel events directly.
///
/// The sign matches `smooth_scroll_delta` (egui accumulates `smooth_wheel_delta`
/// from the same event deltas without negating them; positive Y = content moved
/// down). The magnitude is the raw per-event delta and is unit-dependent
/// (line/point/page), which is sufficient for the sign/threshold-based wheel
/// handling that used `raw_scroll_delta`.
#[must_use]
pub fn raw_wheel_delta(input: &egui::InputState) -> egui::Vec2 {
    input
        .events
        .iter()
        .filter_map(|event| match event {
            egui::Event::MouseWheel { delta, .. } => Some(*delta),
            _ => None,
        })
        .fold(egui::Vec2::ZERO, |acc, delta| acc + delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wheel(y: f32, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, y),
            phase: egui::TouchPhase::Move,
            modifiers,
        }
    }

    /// The regression this guards against: Ctrl+wheel must still be counted, since
    /// egui zeroes `smooth_scroll_delta` under the zoom modifier.
    #[test]
    fn raw_wheel_delta_sums_wheel_events_including_ctrl() {
        let mut input = egui::InputState::default();
        input.events = vec![
            wheel(1.0, egui::Modifiers::CTRL),
            wheel(2.0, egui::Modifiers::default()),
            egui::Event::PointerGone,
        ];
        assert!((raw_wheel_delta(&input).y - 3.0).abs() < f32::EPSILON);
    }

    #[test]
    fn raw_wheel_delta_is_zero_without_wheel_events() {
        let mut input = egui::InputState::default();
        input.events = vec![egui::Event::PointerGone];
        assert_eq!(raw_wheel_delta(&input), egui::Vec2::ZERO);
    }
}
