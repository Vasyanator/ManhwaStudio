/*
File: crates/ms-launcher/src/pages/system_info/view.rs

Purpose:
egui drawing of the "System info" tab's report cards: the launcher's dark rounded info card,
"label: value" rows with a fixed label column, bold adapter headings and indented detail lines.

Key functions:
- draw_cards(): every card, top to bottom, with a gap between them.

Notes:
Draw only: the cards arrive fully worded from `report.rs`, so what is shown and what "Copy
report" copies are the same strings. Rows wrap at narrow widths (`horizontal_wrapped`).
*/

use egui::{Color32, CornerRadius, Frame, Label, Margin, RichText, Stroke, Ui};

use super::report::{ReportCard, ReportItem, Tone};
use crate::theme;

/// Fill of an info card (dark translucent, as the other launcher settings cards).
const CARD_FILL: Color32 = Color32::from_rgba_premultiplied(12, 12, 16, 168);
/// Width of the row label column; values start after it and wrap.
const LABEL_WIDTH: f32 = 170.0;
/// Indent of a detail line under its row or heading.
const DETAIL_INDENT: f32 = 14.0;
/// Vertical gap between cards.
const CARD_GAP: f32 = 10.0;

/// Draws every card, top to bottom.
pub(super) fn draw_cards(ui: &mut Ui, cards: &[ReportCard]) {
    for (index, card) in cards.iter().enumerate() {
        if index > 0 {
            ui.add_space(CARD_GAP);
        }
        draw_card(ui, card);
    }
}

/// The launcher colour of a text tone.
fn tone_color(tone: Tone) -> Color32 {
    match tone {
        Tone::Main => theme::TEXT_MAIN,
        Tone::Muted => theme::TEXT_MUTED,
        Tone::Warning => ms_theme::status::WARNING,
    }
}

/// One card: title, then its items.
fn draw_card(ui: &mut Ui, card: &ReportCard) {
    Frame::new()
        .fill(CARD_FILL)
        .stroke(Stroke::new(1.0, theme::BUTTON_STROKE))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(&card.title).size(18.0).strong().color(theme::TEXT_MAIN));
            ui.add_space(8.0);
            for item in &card.items {
                draw_item(ui, item);
            }
        });
}

/// One card line.
fn draw_item(ui: &mut Ui, item: &ReportItem) {
    match item {
        ReportItem::Row { label, value, tone } => {
            ui.horizontal_wrapped(|ui| {
                ui.set_min_height(24.0);
                ui.add_sized([LABEL_WIDTH, 20.0], Label::new(theme::status(label, theme::TEXT_MUTED)));
                ui.label(theme::status(value, tone_color(*tone)));
            });
        }
        ReportItem::Heading(text) => {
            ui.add_space(4.0);
            ui.label(theme::status(text, theme::TEXT_MAIN).strong());
        }
        ReportItem::Detail { text, tone } => {
            ui.horizontal_wrapped(|ui| {
                ui.add_space(DETAIL_INDENT);
                ui.label(theme::status(text, tone_color(*tone)));
            });
        }
        ReportItem::Note { text, tone } => {
            ui.label(theme::status(text, tone_color(*tone)));
        }
    }
}
