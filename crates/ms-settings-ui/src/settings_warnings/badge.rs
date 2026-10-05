/*
FILE OVERVIEW: crates/ms-settings-ui/src/settings_warnings/badge.rs
The only egui file of the settings warnings: the "!" badge.

Purpose:
- `paint_warning_badge` / `paint_corner_badge`: PAINT-ONLY badges (no hitbox, no layout
  space; `egui-docs/06-overlays.md` §3) for buttons that already own their click rect —
  the launcher's settings tabs and its main-menu Settings button.
- `item_warning_badge`: the inline badge next to a settings item. It allocates exactly one
  small hover-sensed square in the current layout and carries the item's messages as its
  hover tooltip (`Response::on_hover_text`, `egui-docs/06-overlays.md` §7).
- `WarningLevel::color()`: the fill colour of a level, from `ms_theme::status`.

Notes:
The "!" is drawn with shapes (a bar and a dot), not text: it is a symbol, not a caption,
and needs no font. egui APIs verified against `egui-docs/api/symbols.txt`:
`Painter::circle_filled` (egui-0.36.2/src/painter.rs:356), `Painter::line_segment` (:318),
`Ui::allocate_exact_size` (ui.rs:1151), `Sense::hover` (sense.rs:45),
`Response::on_hover_text` (response.rs:727), `Rect::right_top` (emath rect.rs:622),
`Ui::text_style_height` (ui.rs:633), `TextStyle::Body` (style.rs:77).
*/

use egui::{Color32, Painter, Pos2, Rect, Response, Sense, Stroke, TextStyle, Ui, Vec2, vec2};

use super::model::{SettingKey, WarningLevel, WarningSet};

/// Badge radius on a corner of a button, in points: legible on the 36 px launcher tab
/// buttons without covering their label.
const CORNER_BADGE_RADIUS: f32 = 7.0;

impl WarningLevel {
    /// The badge fill of this level: `ms_theme::status::ERROR` for Red,
    /// `ms_theme::status::WARNING` for Yellow.
    #[must_use]
    pub fn color(self) -> Color32 {
        match self {
            WarningLevel::Red => ms_theme::status::ERROR,
            WarningLevel::Yellow => ms_theme::status::WARNING,
        }
    }
}

/// Paints a filled circle of `radius` points at `center` in the level colour with an
/// `ms_theme::status::ON_STATUS_FILL` "!" on it. Paint only: registers no hitbox and
/// takes no layout space.
pub fn paint_warning_badge(painter: &Painter, center: Pos2, radius: f32, level: WarningLevel) {
    painter.circle_filled(center, radius, level.color());
    // The "!": a bar over the upper ~55 % and a dot below it, scaled with the radius.
    let glyph = ms_theme::status::ON_STATUS_FILL;
    let stroke_width = (radius * 0.28).max(1.0);
    let bar_top = center + vec2(0.0, -radius * 0.55);
    let bar_bottom = center + vec2(0.0, radius * 0.12);
    painter.line_segment([bar_top, bar_bottom], Stroke::new(stroke_width, glyph));
    painter.circle_filled(center + vec2(0.0, radius * 0.48), stroke_width * 0.6, glyph);
}

/// Paints the badge on `rect`'s top-right corner, inset so it stays inside a parent clip
/// that ends at `rect` (tab buttons, the main-menu button). Paint only: the button's click
/// rect is unchanged.
pub fn paint_corner_badge(painter: &Painter, rect: Rect, level: WarningLevel) {
    let inset = CORNER_BADGE_RADIUS * 0.6;
    let center = rect.right_top() + vec2(-inset, inset);
    paint_warning_badge(painter, center, CORNER_BADGE_RADIUS, level);
}

/// Inline item badge: allocates ONE square (`Sense::hover`) at the current layout position,
/// sized to the body text height so it never makes the item's row taller than its own
/// content (a label row stays put when the badge appears), paints the item's worst level in it and
/// shows every message of `key` as the hover tooltip. Allocates nothing and returns `None`
/// when `warnings` is `None` (the studio passes `None`) or the key is clean.
pub fn item_warning_badge(ui: &mut Ui, warnings: Option<&WarningSet>, key: SettingKey) -> Option<Response> {
    let set = warnings?;
    let level = set.item_level(key)?;
    let tooltip = set.tooltip_text(key)?;
    let side = ui.text_style_height(&TextStyle::Body);
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
    paint_warning_badge(ui.painter(), rect.center(), side * 0.4, level);
    Some(response.on_hover_text(tooltip))
}
