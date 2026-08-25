/*
FILE HEADER (widgets/wheel_input_guard.rs)
- Назначение: общий guard для wheel-aware UI-виджетов.
- Ключевые сущности:
  - `OpenComboPopupGuard`: temp-состояние egui о том, что на текущем кадре открыт
    popup combobox, и последний известный viewport-rect его списка.
- Ключевые функции:
  - `publish_combo_popup_open`: вызывается combobox-виджетом при открытом popup.
  - `publish_combo_popup_rect`: публикует область выпадающего списка.
  - `combo_popup_open`: проверяет, нужно ли нижележащим wheel-виджетам игнорировать колесо.
  - `combo_popup_blocks_pointer`: проверяет, находится ли курсор над списком popup-а.
- Особенности:
  - guard хранится в `egui::Context::data` только для текущего/соседнего кадра;
  - нужен, чтобы wheel-события popup-слоя не попадали в слайдеры и spinbox'ы,
    которые геометрически находятся под выпадающим списком.

Shared wheel-step helpers (English, added later):
- `wheel_steps_if_hovered`: the one logical wheel step a CLOSED combo-box button reacts to.
- `cycle_wrapped_index`: moves a selected index by that step, wrapping at both ends.
- `raw_wheel_events_delta` / `axis_wheel_delta`: the per-notch delta those two read.
They live here rather than in one combo box because `WheelComboBox` and `SearchableComboBox`
must react to the wheel identically; a copy per widget is exactly how that contract drifts.
*/

use eframe::egui;
use egui::{Context, Id, Rect, Response, Vec2};

const OPEN_COMBO_POPUP_GUARD_ID: &str = "wheel_input_open_combo_popup_guard";

#[derive(Clone, Copy, Debug)]
struct OpenComboPopupGuard {
    frame_nr: u64,
    rect: Option<Rect>,
}

pub(super) fn publish_combo_popup_open(ctx: &egui::Context) {
    let frame_nr = ctx.cumulative_frame_nr();
    ctx.data_mut(|data| {
        data.insert_temp(
            Id::new(OPEN_COMBO_POPUP_GUARD_ID),
            OpenComboPopupGuard {
                frame_nr,
                rect: None,
            },
        );
    });
}

pub(super) fn publish_combo_popup_rect(ctx: &egui::Context, rect: Rect) {
    let frame_nr = ctx.cumulative_frame_nr();
    ctx.data_mut(|data| {
        data.insert_temp(
            Id::new(OPEN_COMBO_POPUP_GUARD_ID),
            OpenComboPopupGuard {
                frame_nr,
                rect: Some(rect),
            },
        );
    });
}

/// Whether a combo-box popup is open this frame (or was on the previous one).
///
/// Wheel-aware consumers — including canvases that read the raw wheel delta, such
/// as the page manager's split board — must skip their wheel reaction while it is
/// true, so the wheel belongs to the open list alone.
pub fn combo_popup_open(ctx: &egui::Context) -> bool {
    let Some(guard) =
        ctx.data(|data| data.get_temp::<OpenComboPopupGuard>(Id::new(OPEN_COMBO_POPUP_GUARD_ID)))
    else {
        return false;
    };

    ctx.cumulative_frame_nr().saturating_sub(guard.frame_nr) <= 1
}

pub(super) fn combo_popup_blocks_pointer(ctx: &egui::Context) -> bool {
    let Some(guard) =
        ctx.data(|data| data.get_temp::<OpenComboPopupGuard>(Id::new(OPEN_COMBO_POPUP_GUARD_ID)))
    else {
        return false;
    };

    if ctx.cumulative_frame_nr().saturating_sub(guard.frame_nr) > 1 {
        return false;
    }
    let Some(rect) = guard.rect else {
        return false;
    };

    ctx.input(|input| {
        input
            .pointer
            .hover_pos()
            .or_else(|| input.pointer.interact_pos())
            .is_some_and(|pos| rect.contains(pos))
    })
}

/// Wraps `index` by `steps` positions inside a list of `len` items.
///
/// One wheel notch on a combo box moves one logical step and wraps around both ends. An empty
/// list or a zero step is a no-op.
///
/// An out-of-range `index` is reduced into range FIRST: a combo box may be handed a selection
/// its caller has not cleaned up yet (`SearchableComboBox::show` documents that this never
/// panics), and the naive `index + shift` overflows in debug builds for an index within `len`
/// of `usize::MAX`.
pub(super) fn cycle_wrapped_index(index: usize, len: usize, steps: i32) -> usize {
    if len == 0 || steps == 0 {
        return index;
    }
    let current = index % len;
    let magnitude = usize::try_from(steps.unsigned_abs()).unwrap_or(0) % len;
    // Stepping back by `magnitude` is stepping forward by `len - magnitude`.
    let shift = if steps > 0 {
        magnitude
    } else {
        len - magnitude
    };
    // Both branches compute `(current + shift) % len` without ever forming that sum, which
    // would overflow for a `len` above `usize::MAX / 2`.
    if current < len - shift {
        current + shift
    } else {
        current - (len - shift)
    }
}

/// One logical wheel step over a CLOSED combo-box button, or `None` when the wheel must be
/// ignored.
///
/// Consumes the frame's smoothed scroll delta whenever it reacts, so the parent `ScrollArea`
/// does not scroll under the cursor, and returns nothing while ANY combo popup is open —
/// including the caller's own, whose list owns the wheel while it is up.
///
/// Exactly ONE step is reported per frame, however many notches arrived in it: only the SIGN
/// of the raw delta is read. That is deliberate and shared by every combo box in the project,
/// so that the wheel moves one row per notch at any wheel speed.
pub(super) fn wheel_steps_if_hovered(ctx: &Context, response: &Response) -> Option<i32> {
    if combo_popup_open(ctx) {
        return None;
    }
    if !response.hovered() && !response.has_focus() {
        return None;
    }

    let (raw_wheel, smooth_wheel) = ctx.input(|input| {
        (
            axis_wheel_delta(raw_wheel_events_delta(input)),
            axis_wheel_delta(input.smooth_scroll_delta),
        )
    });
    if raw_wheel.abs() <= f32::EPSILON && smooth_wheel.abs() <= f32::EPSILON {
        return None;
    }

    ctx.input_mut(|input| {
        input.smooth_scroll_delta = Vec2::ZERO;
    });
    if smooth_wheel.abs() > f32::EPSILON {
        ctx.request_repaint();
    }

    // Only the RAW delta marks a physical notch: the smoothed one ramps over several frames
    // and would move the selection once per frame of that ramp.
    if raw_wheel.abs() <= f32::EPSILON {
        None
    } else if raw_wheel > 0.0 {
        Some(-1)
    } else {
        Some(1)
    }
}

/// Sums the raw (unsmoothed) mouse-wheel delta reported this frame.
///
/// egui 0.35 removed `InputState::raw_scroll_delta`, so the per-frame unsmoothed
/// wheel movement is recovered by summing `Event::MouseWheel` deltas. Unlike
/// `smooth_scroll_delta`, which ramps over several frames, this is nonzero only on
/// the frame a physical wheel notch arrives, so it yields exactly one step per notch.
/// Only the sign is used downstream, so the event `unit` is irrelevant.
fn raw_wheel_events_delta(input: &egui::InputState) -> Vec2 {
    input
        .events
        .iter()
        .filter_map(|event| match event {
            egui::Event::MouseWheel { delta, .. } => Some(*delta),
            _ => None,
        })
        .fold(Vec2::ZERO, |acc, delta| acc + delta)
}

/// Vertical wheel delta, falling back to the horizontal one for wheels that only report X.
fn axis_wheel_delta(delta: Vec2) -> f32 {
    if delta.y.abs() > f32::EPSILON {
        delta.y
    } else {
        delta.x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_guard_cycles_index_and_tolerates_empty_lists() {
        assert_eq!(cycle_wrapped_index(0, 3, 1), 1);
        assert_eq!(cycle_wrapped_index(2, 3, 1), 0);
        assert_eq!(cycle_wrapped_index(0, 3, -1), 2);
        assert_eq!(cycle_wrapped_index(5, 0, 1), 5);
        assert_eq!(cycle_wrapped_index(1, 3, 0), 1);
        // A step of a full turn (and of several) lands back where it started.
        assert_eq!(cycle_wrapped_index(1, 3, 3), 1);
        assert_eq!(cycle_wrapped_index(1, 3, -3), 1);
    }

    #[test]
    fn wheel_guard_cycles_an_out_of_range_index_without_overflowing() {
        // The regression this guards: `index + shift` panicked in debug builds here.
        assert_eq!(
            cycle_wrapped_index(usize::MAX, 3, 1),
            (usize::MAX % 3 + 1) % 3
        );
        assert_eq!(
            cycle_wrapped_index(usize::MAX, 3, -1),
            (usize::MAX % 3 + 2) % 3
        );
        assert_eq!(cycle_wrapped_index(usize::MAX, 1, 1), 0);
        // `index` is exactly `len` here, so it reduces to 0 and one step forward is 1.
        assert_eq!(cycle_wrapped_index(usize::MAX, usize::MAX, 1), 1);
    }

    #[test]
    fn wheel_guard_axis_delta_falls_back_to_the_horizontal_wheel() {
        assert!((axis_wheel_delta(Vec2::new(0.0, -2.0)) + 2.0).abs() < f32::EPSILON);
        assert!((axis_wheel_delta(Vec2::new(3.0, 0.0)) - 3.0).abs() < f32::EPSILON);
        assert!(axis_wheel_delta(Vec2::ZERO).abs() < f32::EPSILON);
    }
}
