/*
File: src/launcher/theme.rs

Purpose:
Dark theme styling helpers for the Rust launcher test UI.

Main responsibilities:
- configure egui visuals for the launcher overlay;
- define shared colors, button states, and card surfaces;
- keep typography helpers and explicit launcher button rendering consistent with launcher.py.
*/

use egui::style::StyleModifier;
use egui::{
    Button, Color32, Context, CornerRadius, Frame, Margin, Response, RichText, Stroke, Style, Ui,
    Vec2,
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

pub fn configure_context(ctx: &Context) {
    let mut style = (*ctx.global_style()).clone();
    style.spacing.item_spacing = egui::vec2(12.0, 12.0);
    style.spacing.button_padding = egui::vec2(18.0, 12.0);
    style.visuals = egui::Visuals::dark();
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
    ctx.set_global_style(style);
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

pub fn launcher_button(ui: &mut Ui, label: &str, size: Vec2, enabled: bool) -> Response {
    let button_style = if enabled {
        active_button_style(ui.style().as_ref())
    } else {
        inactive_button_style(ui.style().as_ref())
    };
    ui.scope(|ui| {
        ui.set_style(button_style);
        ui.add_enabled(
            enabled,
            Button::new(RichText::new(label).size(16.0).color(TEXT_MAIN))
                .min_size(size)
                .fill(BUTTON_FILL)
                .stroke(Stroke::new(1.0, BUTTON_STROKE))
                .corner_radius(CornerRadius::same(10)),
        )
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
