/*
File: crates/ms-region-postprocess/src/ui.rs

Purpose:
The body of the «Постобработка» settings section shared by the region editor engines: feather
dial, colour-match checkbox and the indented local strength / scale dials.

Key functions:
- `draw_postprocess_body()`: draws the body, reports changes.
- `section_title()`: the section's localized title for the host's chrome.

Notes:
Body only: each host wraps it in its own section chrome (separator + collapsing header, starting
collapsed), so the chrome keeps one owner per host crate. Values are clamped by the dials'
ranges, which come from `settings`.
*/

use ms_widgets::WheelSlider;

use crate::settings::{FEATHER_MAX_PX, LOCAL_PCT_MAX, PostprocessSettings, SCALE_MAX_PX, SCALE_MIN_PX};

/// Localized title of the post-processing section, for the host's section header (pin its
/// `id_salt` to a stable key, never to this text).
#[must_use]
pub fn section_title() -> &'static str {
    t!("cleaning.region_postprocess.section_title")
}

/// Draws the post-processing section body into `ui` under the stable `id_salt`: the feather
/// dial, «Подгонять цвета к оригиналу» and, indented, the local strength / scale dials (disabled
/// while the checkbox is off; their stored values are kept). Returns whether a value changed.
#[must_use]
pub fn draw_postprocess_body(ui: &mut egui::Ui, settings: &mut PostprocessSettings, id_salt: impl egui::AsIdSalt) -> bool {
    ui.push_id(id_salt, |ui| draw_body(ui, settings).changed).inner
}

/// What `draw_body` drew: the change flag plus the rects headless tests aim at.
#[derive(Debug, Clone, Copy)]
struct BodyResponse {
    changed: bool,
    #[cfg_attr(not(test), expect(dead_code, reason = "read only by the headless UI tests"))]
    color_match_rect: egui::Rect,
    #[cfg_attr(not(test), expect(dead_code, reason = "read only by the headless UI tests"))]
    local_rect: egui::Rect,
}

fn draw_body(ui: &mut egui::Ui, settings: &mut PostprocessSettings) -> BodyResponse {
    let mut changed = ui
        .add(WheelSlider::new(&mut settings.mask_feather_px, 0..=FEATHER_MAX_PX).suffix(t!("cleaning.tab.pixels_suffix")).text(t!("cleaning.region_postprocess.feather_label")))
        .on_hover_text(t!("cleaning.region_postprocess.feather_hint"))
        .changed();
    let color_match = ui
        .checkbox(&mut settings.color_match, t!("cleaning.region_postprocess.color_match_label"))
        .on_hover_text(t!("cleaning.region_postprocess.color_match_hint"));
    changed |= color_match.changed();
    // The local match refines the global one and runs only under it, so both dials close while
    // the checkbox is off. A disabled `WheelSlider` ignores the wheel itself (`egui-docs/04-widgets.md` §0.2).
    let enabled = settings.color_match;
    let local_rect = ui
        .indent("color_match_local", |ui| {
            let local = ui
                .add_enabled(
                    enabled,
                    WheelSlider::new(&mut settings.color_match_local_pct, 0..=LOCAL_PCT_MAX)
                        // A unit symbol, not prose: the project's percent dials spell it literally.
                        .suffix("%")
                        .text(t!("cleaning.region_postprocess.color_match_local_label")),
                )
                .on_hover_text(t!("cleaning.region_postprocess.color_match_local_hint"))
                .on_disabled_hover_text(t!("cleaning.region_postprocess.color_match_local_hint"));
            changed |= local.changed();
            changed |= ui
                .add_enabled(
                    enabled,
                    WheelSlider::new(&mut settings.color_match_scale_px, SCALE_MIN_PX..=SCALE_MAX_PX)
                        .suffix(t!("cleaning.tab.pixels_suffix"))
                        .text(t!("cleaning.region_postprocess.color_match_scale_label")),
                )
                .on_hover_text(t!("cleaning.region_postprocess.color_match_scale_hint"))
                .on_disabled_hover_text(t!("cleaning.region_postprocess.color_match_scale_hint"))
                .changed();
            local.rect
        })
        .inner;
    BodyResponse { changed, color_match_rect: color_match.rect, local_rect }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Modifiers, MouseWheelUnit, PointerButton, Pos2, RawInput, Rect, TouchPhase, Vec2};

    /// One headless pass drawing the body over `settings` with `events`.
    fn pass(ctx: &egui::Context, settings: &mut PostprocessSettings, events: Vec<Event>) -> BodyResponse {
        let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 400.0))), events, ..RawInput::default() };
        let mut out = None;
        ctx.run_ui(input, |ui| {
            out = Some(ui.push_id("test_body", |ui| draw_body(ui, settings)).inner);
        })
        .drop_without_applying_deltas();
        out.unwrap_or(BodyResponse { changed: false, color_match_rect: Rect::NOTHING, local_rect: Rect::NOTHING })
    }

    fn click(pos: Pos2) -> Vec<Event> {
        vec![
            Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE },
            Event::PointerButton { pos, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE },
        ]
    }

    #[test]
    fn toggling_the_checkbox_reports_a_change_and_keeps_local_values() {
        let ctx = egui::Context::default();
        let mut settings = PostprocessSettings { color_match_local_pct: 37, color_match_scale_px: 50, ..PostprocessSettings::default() };
        let first = pass(&ctx, &mut settings, Vec::new());
        assert!(!first.changed, "drawing alone changes nothing");
        let target = first.color_match_rect.left_center() + Vec2::new(6.0, 0.0);
        pass(&ctx, &mut settings, vec![Event::PointerMoved(target)]);
        let toggled = pass(&ctx, &mut settings, click(target));
        assert!(toggled.changed, "a click on the checkbox is a change");
        assert!(!settings.color_match);
        assert_eq!((settings.color_match_local_pct, settings.color_match_scale_px), (37, 50), "stored local values survive");
    }

    #[test]
    fn disabled_local_dial_ignores_the_wheel() {
        let ctx = egui::Context::default();
        let mut settings = PostprocessSettings { color_match: false, ..PostprocessSettings::default() };
        let first = pass(&ctx, &mut settings, Vec::new());
        let over = first.local_rect.center();
        pass(&ctx, &mut settings, vec![Event::PointerMoved(over)]);
        pass(&ctx, &mut settings, Vec::new());
        let wheel = Event::MouseWheel { unit: MouseWheelUnit::Line, delta: Vec2::new(0.0, 1.0), phase: TouchPhase::Move, modifiers: Modifiers::NONE };
        let after = pass(&ctx, &mut settings, vec![wheel]);
        assert!(!after.changed);
        assert_eq!(settings.color_match_local_pct, 60);
        // The same notch moves the dial once the checkbox is on (proves the pass reaches it).
        settings.color_match = true;
        pass(&ctx, &mut settings, Vec::new());
        pass(&ctx, &mut settings, vec![Event::PointerMoved(over)]);
        let wheel = Event::MouseWheel { unit: MouseWheelUnit::Line, delta: Vec2::new(0.0, 1.0), phase: TouchPhase::Move, modifiers: Modifiers::NONE };
        let enabled = pass(&ctx, &mut settings, vec![wheel]);
        assert!(enabled.changed);
        assert_ne!(settings.color_match_local_pct, 60);
    }
}
