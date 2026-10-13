/*
File: crates/ms-launcher/src/theme.rs

Purpose:
The launcher's style layer and dark styling helpers.

Main responsibilities:
- build the launcher screen style as a pure layer over `ms_theme::base_style()`
  (`launcher_style`); the host window (`src/shell/style.rs`, `web_entry.rs`) installs it through
  `ms_theme::install`, the only global-style writer;
- define shared colors, button states, and card surfaces;
- keep typography helpers and explicit launcher button rendering consistent with launcher.py
  (full-size `launcher_button` and its half-font `launcher_button_small` share one body);
- draw the amber notice banner (`notice_banner`) shared by every launcher notice that offers
  one action (the open page's unsaved-session recovery and chapter-format conversion).
*/

use egui::style::StyleModifier;
use egui::{
    Align, Button, Color32, CornerRadius, Frame, Label, Layout, Margin, Response,
    RichText, Stroke, Style, Ui, Vec2,
};

pub const CARD_FILL: Color32 = Color32::from_rgba_premultiplied(24, 24, 28, 135);
pub const CARD_STROKE: Color32 = Color32::from_rgba_premultiplied(255, 255, 255, 26);
pub const TEXT_MAIN: Color32 = Color32::from_rgb(237, 237, 237);
pub const TEXT_MUTED: Color32 = Color32::from_rgba_premultiplied(237, 237, 237, 178);
pub const TEXT_FAINT: Color32 = Color32::from_rgba_premultiplied(237, 237, 237, 140);
pub const BUTTON_FILL: Color32 = Color32::from_rgba_premultiplied(55, 55, 55, 15);
/// Hover fill for plain (non-`launcher_button`) buttons and combo boxes.
///
/// The launcher style never highlights with white: a white-alpha overlay reads
/// as an additive glow (a "white flash") on the dark card. This is a neutral,
/// slightly-lightened card tone (opaque) that lifts the widget subtly instead.
pub const BUTTON_HOVERED: Color32 = Color32::from_rgba_premultiplied(44, 44, 52, 236);
/// Pressed/active fill for plain buttons and combo boxes; neutral, never white.
///
/// A touch lighter than `BUTTON_HOVERED` so a press reads as a further lift,
/// staying within the neutral `COMBO_HOVERED`/`COMBO_PRESSED` family.
pub const BUTTON_PRESSED: Color32 = Color32::from_rgba_premultiplied(52, 52, 60, 244);
pub const BUTTON_STROKE: Color32 = Color32::from_rgba_premultiplied(255, 255, 255, 31);
pub const BUTTON_HOVER_EXPANSION: f32 = 2.0;
pub const COMBO_FILL: Color32 = Color32::from_rgba_premultiplied(24, 24, 28, 224);
pub const COMBO_HOVERED: Color32 = Color32::from_rgba_premultiplied(34, 34, 40, 236);
pub const COMBO_PRESSED: Color32 = Color32::from_rgba_premultiplied(42, 42, 50, 244);
pub const COMBO_POPUP_FILL: Color32 = Color32::from_rgb(24, 24, 28);
pub const VEIL_TINT: Color32 = Color32::from_rgba_premultiplied(0, 0, 0, 112);
pub const STATUS_SUCCESS: Color32 = Color32::from_rgb(56, 168, 72);
/// Status colour of an error line (failed checks, failed operations).
pub const STATUS_ERROR: Color32 = Color32::from_rgb(220, 120, 120);
/// Fill of a notice banner (dark amber).
pub const NOTICE_FILL: Color32 = Color32::from_rgb(72, 58, 0);
/// Text colour of a notice banner (amber on `NOTICE_FILL`).
pub const NOTICE_TEXT: Color32 = Color32::from_rgb(255, 210, 40);
/// Caption font size of a full-size [`launcher_button`].
const LAUNCHER_BUTTON_FONT_SIZE: f32 = 16.0;
/// Corner radius of a full-size [`launcher_button`].
const LAUNCHER_BUTTON_CORNER_RADIUS: u8 = 10;
/// Caption scale of [`launcher_button_small`] relative to [`launcher_button`].
const SMALL_BUTTON_SCALE: f32 = 0.7;
/// Padding of [`launcher_button_small`]: thin enough to keep the button about one caption
/// line tall.
const SMALL_BUTTON_PADDING: Vec2 = Vec2::new(8.0, 2.0);
/// Corner radius of [`launcher_button_small`], scaled down with the button.
const SMALL_BUTTON_CORNER_RADIUS: u8 = 6;
/// Size of the action button of a notice banner.
const NOTICE_BUTTON_SIZE: Vec2 = Vec2::new(130.0, 26.0);

/// The launcher's style layer, a pure function of `base`: launcher spacing, widget fills,
/// strokes and corner radii, transparent panels and opaque popups. Every `Visuals` field it does
/// not name (notably `error_fg_color` / `warn_fg_color`) is kept from `base`.
#[must_use]
pub fn launcher_style(base: Style) -> Style {
    let mut style = base;
    style.spacing.item_spacing = egui::vec2(12.0, 12.0);
    style.spacing.button_padding = egui::vec2(18.0, 12.0);
    style.visuals.extreme_bg_color = COMBO_FILL;
    style.visuals.faint_bg_color = Color32::from_rgba_premultiplied(255, 255, 255, 10);
    style.visuals.code_bg_color = Color32::from_rgba_premultiplied(20, 20, 24, 230);
    style.visuals.selection.bg_fill = Color32::from_rgba_premultiplied(120, 120, 140, 96);
    style.visuals.selection.stroke =
        Stroke::new(1.0, Color32::from_rgba_premultiplied(255, 255, 255, 72));
    style.visuals.override_text_color = Some(TEXT_MAIN);
    style.visuals.widgets.inactive.bg_fill = BUTTON_FILL;
    style.visuals.widgets.inactive.weak_bg_fill = BUTTON_FILL;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BUTTON_STROKE);
    style.visuals.widgets.inactive.corner_radius = CornerRadius::same(10);
    style.visuals.widgets.hovered.bg_fill = BUTTON_HOVERED;
    style.visuals.widgets.hovered.weak_bg_fill = BUTTON_HOVERED;
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, BUTTON_STROKE);
    style.visuals.widgets.hovered.corner_radius = CornerRadius::same(10);
    style.visuals.widgets.active.bg_fill = BUTTON_PRESSED;
    style.visuals.widgets.active.weak_bg_fill = BUTTON_PRESSED;
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.0, BUTTON_STROKE);
    style.visuals.widgets.active.corner_radius = CornerRadius::same(10);
    style.visuals.widgets.open.bg_fill = BUTTON_HOVERED;
    style.visuals.widgets.open.weak_bg_fill = BUTTON_HOVERED;
    style.visuals.widgets.open.bg_stroke = Stroke::new(1.0, BUTTON_STROKE);
    style.visuals.widgets.open.corner_radius = CornerRadius::same(10);
    style.visuals.widgets.noninteractive.bg_fill = BUTTON_FILL;
    style.visuals.widgets.noninteractive.weak_bg_fill = BUTTON_FILL;
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BUTTON_STROKE);
    style.visuals.widgets.noninteractive.corner_radius = CornerRadius::same(10);
    style.visuals.widgets.inactive.fg_stroke.color = TEXT_MAIN;
    style.visuals.widgets.hovered.fg_stroke.color = TEXT_MAIN;
    style.visuals.widgets.active.fg_stroke.color = TEXT_MAIN;
    style.visuals.widgets.open.fg_stroke.color = TEXT_MAIN;
    style.visuals.widgets.noninteractive.fg_stroke.color = TEXT_MAIN;
    style.visuals.widgets.inactive.expansion = 0.0;
    style.visuals.widgets.hovered.expansion = BUTTON_HOVER_EXPANSION;
    style.visuals.widgets.active.expansion = BUTTON_HOVER_EXPANSION;
    style.visuals.widgets.open.expansion = BUTTON_HOVER_EXPANSION;
    // Combo/menu/tooltip popups fill with `Frame::popup(style)`, which uses
    // `visuals.window_fill`. Keep it opaque so dropdowns (e.g. the shared AI
    // backend panel, which does not apply `combo_popup_style`) render as a solid
    // dark panel instead of a see-through frame, with a subtle launcher stroke.
    // `panel_fill` stays transparent so the launcher's own background stays visible.
    style.visuals.window_fill = COMBO_POPUP_FILL;
    style.visuals.window_stroke = Stroke::new(1.0, CARD_STROKE);
    style.visuals.panel_fill = Color32::TRANSPARENT;
    style.visuals.window_corner_radius = CornerRadius::same(18);
    style.visuals.menu_corner_radius = CornerRadius::same(12);
    style
}

pub fn hero_title(text: &str) -> RichText {
    RichText::new(text).size(36.0).strong().color(TEXT_MAIN)
}

pub fn footer(text: &str) -> RichText {
    RichText::new(text).size(12.0).color(TEXT_FAINT)
}

pub fn status(text: &str, color: Color32) -> RichText {
    RichText::new(text).size(12.0).color(color)
}

pub fn card_frame() -> Frame {
    Frame::new()
        .fill(CARD_FILL)
        .stroke(Stroke::new(1.0, CARD_STROKE))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(24))
}

/// Draws a full-size launcher button: `LAUNCHER_BUTTON_FONT_SIZE` caption, at least `size`,
/// global button padding. A disabled button is drawn in the inactive style and never clicks.
pub fn launcher_button(ui: &mut Ui, label: &str, size: Vec2, enabled: bool) -> Response {
    let padding = ui.spacing().button_padding;
    styled_launcher_button(ui, label, LAUNCHER_BUTTON_FONT_SIZE, size, padding, LAUNCHER_BUTTON_CORNER_RADIUS, enabled)
}

/// Draws the small variant of [`launcher_button`]: same fill, stroke, hover and disabled
/// styling, caption at `SMALL_BUTTON_SCALE` of the full-size font, sized to fit the caption
/// with `SMALL_BUTTON_PADDING`. Used for secondary actions tucked next to the main menu.
pub fn launcher_button_small(ui: &mut Ui, label: &str, enabled: bool) -> Response {
    styled_launcher_button(
        ui,
        label,
        LAUNCHER_BUTTON_FONT_SIZE * SMALL_BUTTON_SCALE,
        Vec2::ZERO,
        SMALL_BUTTON_PADDING,
        SMALL_BUTTON_CORNER_RADIUS,
        enabled,
    )
}

/// Shared body of the launcher button variants: applies the active or inactive launcher
/// button style and `padding` in a scope, so the caller's style is left untouched.
fn styled_launcher_button(
    ui: &mut Ui,
    label: &str,
    font_size: f32,
    min_size: Vec2,
    padding: Vec2,
    corner_radius: u8,
    enabled: bool,
) -> Response {
    let mut button_style = if enabled {
        active_button_style(ui.style().as_ref())
    } else {
        inactive_button_style(ui.style().as_ref())
    };
    button_style.spacing.button_padding = padding;
    ui.scope(|ui| {
        ui.set_style(button_style);
        ui.add_enabled(
            enabled,
            Button::new(RichText::new(label).size(font_size).color(TEXT_MAIN))
                .min_size(min_size)
                .fill(BUTTON_FILL)
                .stroke(Stroke::new(1.0, BUTTON_STROKE))
                .corner_radius(CornerRadius::same(corner_radius)),
        )
    })
    .inner
}

/// The single action button of a [`notice_banner`].
#[derive(Debug, Clone, Copy)]
pub struct NoticeButton<'a> {
    /// Localized button caption.
    pub label: &'a str,
    /// Whether the button can be clicked (a disabled button is drawn inactive).
    pub enabled: bool,
}

/// Draws an amber notice banner `width` points wide: `text` on the left (wrapping when it
/// does not fit beside the button) and, when `button` is `Some`, one action button on the
/// right. Returns whether that button was clicked this frame (always `false` without one).
///
/// `id_salt` must be stable and distinct per banner on a page: the banner's widgets are
/// scoped under it, so the ids never depend on the (localized) text.
pub fn notice_banner(ui: &mut Ui, id_salt: &str, width: f32, text: &str, button: Option<NoticeButton<'_>>) -> bool {
    ui.push_id(id_salt, |ui| {
        Frame::new()
            .fill(NOTICE_FILL)
            .inner_margin(Margin::symmetric(10, 8))
            .corner_radius(CornerRadius::same(6))
            .show(ui, |ui| {
                ui.set_width(width);
                // Right-to-left first so the button claims its slot before the text; the
                // text then lays out left-to-right in the remaining width and wraps there.
                // A horizontal `Align::Center` layout fills the height it is GIVEN, so the row
                // is allocated with zero desired height: it then grows to its content only,
                // whatever free vertical space the caller has.
                ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::right_to_left(Align::Center), |ui| {
                    let clicked = button.is_some_and(|button| launcher_button(ui, button.label, NOTICE_BUTTON_SIZE, button.enabled).clicked());
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(Label::new(RichText::new(text).color(NOTICE_TEXT)).wrap());
                    });
                    clicked
                })
                .inner
            })
            .inner
    })
    .inner
}

pub fn combo_box_style(style: &Style) -> Style {
    let mut style = style.clone();
    style.visuals.widgets.inactive.bg_fill = COMBO_FILL;
    style.visuals.widgets.inactive.weak_bg_fill = COMBO_FILL;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BUTTON_STROKE);
    style.visuals.widgets.inactive.fg_stroke.color = TEXT_MAIN;
    style.visuals.widgets.hovered.bg_fill = COMBO_HOVERED;
    style.visuals.widgets.hovered.weak_bg_fill = COMBO_HOVERED;
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, BUTTON_STROKE);
    style.visuals.widgets.hovered.fg_stroke.color = TEXT_MAIN;
    style.visuals.widgets.active.bg_fill = COMBO_PRESSED;
    style.visuals.widgets.active.weak_bg_fill = COMBO_PRESSED;
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.0, BUTTON_STROKE);
    style.visuals.widgets.active.fg_stroke.color = TEXT_MAIN;
    style.visuals.widgets.open.bg_fill = COMBO_HOVERED;
    style.visuals.widgets.open.weak_bg_fill = COMBO_HOVERED;
    style.visuals.widgets.open.bg_stroke = Stroke::new(1.0, BUTTON_STROKE);
    style.visuals.widgets.open.fg_stroke.color = TEXT_MAIN;
    style
}

pub fn combo_popup_style() -> StyleModifier {
    StyleModifier::new(|style| {
        style.visuals.window_fill = COMBO_POPUP_FILL;
        style.visuals.panel_fill = COMBO_POPUP_FILL;
        style.visuals.extreme_bg_color = COMBO_POPUP_FILL;
        style.visuals.widgets.inactive.bg_fill = COMBO_POPUP_FILL;
        style.visuals.widgets.inactive.weak_bg_fill = COMBO_POPUP_FILL;
        style.visuals.widgets.inactive.fg_stroke.color = TEXT_MAIN;
        style.visuals.widgets.hovered.bg_fill = COMBO_HOVERED;
        style.visuals.widgets.hovered.weak_bg_fill = COMBO_HOVERED;
        style.visuals.widgets.hovered.fg_stroke.color = TEXT_MAIN;
        style.visuals.widgets.active.bg_fill = COMBO_PRESSED;
        style.visuals.widgets.active.weak_bg_fill = COMBO_PRESSED;
        style.visuals.widgets.active.fg_stroke.color = TEXT_MAIN;
    })
}

pub fn inactive_button_style(style: &Style) -> Style {
    let mut style = style.clone();
    style.visuals.widgets.hovered = style.visuals.widgets.inactive;
    style.visuals.widgets.active = style.visuals.widgets.inactive;
    style.visuals.widgets.open = style.visuals.widgets.inactive;
    style
}

fn active_button_style(style: &Style) -> Style {
    let mut style = style.clone();
    style.visuals.widgets.inactive.bg_fill = BUTTON_FILL;
    style.visuals.widgets.inactive.weak_bg_fill = BUTTON_FILL;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BUTTON_STROKE);
    style.visuals.widgets.inactive.fg_stroke.color = TEXT_MAIN;
    style.visuals.widgets.hovered.bg_fill = BUTTON_HOVERED;
    style.visuals.widgets.hovered.weak_bg_fill = BUTTON_HOVERED;
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, BUTTON_STROKE);
    style.visuals.widgets.hovered.fg_stroke.color = TEXT_MAIN;
    style.visuals.widgets.hovered.expansion = BUTTON_HOVER_EXPANSION;
    style.visuals.widgets.active.bg_fill = BUTTON_PRESSED;
    style.visuals.widgets.active.weak_bg_fill = BUTTON_PRESSED;
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.0, BUTTON_STROKE);
    style.visuals.widgets.active.fg_stroke.color = TEXT_MAIN;
    style.visuals.widgets.active.expansion = BUTTON_HOVER_EXPANSION;
    style.visuals.widgets.open = style.visuals.widgets.hovered;
    style
}

#[cfg(test)]
mod tests {
    use super::{
        BUTTON_FILL, BUTTON_HOVER_EXPANSION, BUTTON_HOVERED, BUTTON_PRESSED, BUTTON_STROKE,
        CARD_STROKE, COMBO_FILL, COMBO_POPUP_FILL, TEXT_MAIN, launcher_style,
    };
    use egui::{Color32, CornerRadius, Stroke, Style};

    /// Frozen reference of the launcher style as the former `configure_context` produced it before the
    /// style ownership moved to `ms_theme` (characterization fixture: do NOT "simplify" it into
    /// a call of the production code, it exists to catch drift of that code). Applies the
    /// launcher layer to `style`.
    fn legacy_launcher_style(mut style: Style) -> Style {
        style.spacing.item_spacing = egui::vec2(12.0, 12.0);
        style.spacing.button_padding = egui::vec2(18.0, 12.0);
        style.visuals = egui::Visuals::dark();
        style.visuals.extreme_bg_color = COMBO_FILL;
        style.visuals.faint_bg_color = Color32::from_rgba_premultiplied(255, 255, 255, 10);
        style.visuals.code_bg_color = Color32::from_rgba_premultiplied(20, 20, 24, 230);
        style.visuals.selection.bg_fill = Color32::from_rgba_premultiplied(120, 120, 140, 96);
        style.visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgba_premultiplied(255, 255, 255, 72));
        style.visuals.override_text_color = Some(TEXT_MAIN);
        for (widget, fill) in [
            (&mut style.visuals.widgets.inactive, BUTTON_FILL),
            (&mut style.visuals.widgets.hovered, BUTTON_HOVERED),
            (&mut style.visuals.widgets.active, BUTTON_PRESSED),
            (&mut style.visuals.widgets.open, BUTTON_HOVERED),
            (&mut style.visuals.widgets.noninteractive, BUTTON_FILL),
        ] {
            widget.bg_fill = fill;
            widget.weak_bg_fill = fill;
            widget.bg_stroke = Stroke::new(1.0, BUTTON_STROKE);
            widget.corner_radius = CornerRadius::same(10);
            widget.fg_stroke.color = TEXT_MAIN;
        }
        style.visuals.widgets.inactive.expansion = 0.0;
        style.visuals.widgets.hovered.expansion = BUTTON_HOVER_EXPANSION;
        style.visuals.widgets.active.expansion = BUTTON_HOVER_EXPANSION;
        style.visuals.widgets.open.expansion = BUTTON_HOVER_EXPANSION;
        style.visuals.window_fill = COMBO_POPUP_FILL;
        style.visuals.window_stroke = Stroke::new(1.0, CARD_STROKE);
        style.visuals.panel_fill = Color32::TRANSPARENT;
        style.visuals.window_corner_radius = CornerRadius::same(18);
        style.visuals.menu_corner_radius = CornerRadius::same(12);
        style
    }

    /// The style a fresh context reported after `set_theme(Dark)`, which the
    /// former `configure_context` started from: egui's default dark style.
    fn stock_dark_style() -> Style {
        Style { visuals: egui::Visuals::dark(), ..Default::default() }
    }

    /// `Style: PartialEq` compares `number_formatter` by `Arc` POINTER, so two independently
    /// built styles are never equal; copy `reference`'s formatter into `style` so a comparison
    /// checks every other field.
    fn with_formatter_of(mut style: Style, reference: &Style) -> Style {
        style.number_formatter = reference.number_formatter.clone();
        style
    }

    /// The pure layer over the shared base equals the pre-refactor launcher style everywhere
    /// except the two status colours, which now come from the base (`ms_theme::status`).
    #[test]
    fn launcher_style_matches_legacy_except_status_colours() {
        let layered = launcher_style(ms_theme::base_style());
        assert_eq!(layered.visuals.error_fg_color, ms_theme::status::ERROR);
        assert_eq!(layered.visuals.warn_fg_color, ms_theme::status::WARNING);
        let mut legacy = legacy_launcher_style(stock_dark_style());
        legacy.visuals.error_fg_color = ms_theme::status::ERROR;
        legacy.visuals.warn_fg_color = ms_theme::status::WARNING;
        assert_eq!(layered, with_formatter_of(legacy, &layered));
    }
}
