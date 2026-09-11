/*
File: crates/ms-widgets/src/bubble_status.rs

Purpose:
egui half of the bubble status feature: it paints a rule's border around a bubble
widget and re-exports the GUI-free rule model that lives in `ms_config::bubble_status`.

Main responsibilities:
- paint solid/dashed/dotted/wavy borders for bubble widgets;
- give the persisted `[u8; 4]` colour of a rule its `egui::Color32` flavour;
- keep every `crate::bubble_status::...` path valid by re-exporting the model (the binary
  re-exports this module under that name).

Key items:
- paint_bubble_status_border()
- BubbleBorderPaintColor (extension trait: `color32()` / `set_color32()`)

Notes:
- The rule model itself (kinds, conditions, rules, defaults, JSON, evaluation) is
  `ms_config::bubble_status`: `config::user_config_defaults()` embeds the default
  preset, so the model has to sit at or below the config layer and cannot depend on
  egui. Only the drawing code and the colour conversions live here.
- Rule order matters: the first matching rule wins (see the model module).
*/

use egui::{Color32, CornerRadius, Painter, Pos2, Rect, Stroke};

// The GUI-free rule model. Re-exported wholesale so that `crate::bubble_status::X`
// keeps naming the same items it always did, whichever half X ended up in.
pub use ms_config::bubble_status::*;

const DEFAULT_STATUS_BORDER_WIDTH: f32 = 2.0;
const DASH_LENGTH_PX: f32 = 10.0;
const DASH_GAP_PX: f32 = 6.0;
const DOT_SPACING_PX: f32 = 10.0;
const DOT_RADIUS_PX: f32 = 1.8;
const WAVE_STEP_PX: f32 = 4.0;
const WAVE_LENGTH_PX: f32 = 18.0;
const WAVE_AMPLITUDE_PX: f32 = 2.5;

/// egui colour view of a [`BubbleBorderStyle`], whose stored form is a raw
/// non-premultiplied `[r, g, b, a]` quadruple.
///
/// The style type is declared in the GUI-free `ms-config` crate (the default preset is
/// part of `user_config.json`), so these two conversions cannot be inherent methods on
/// it. They keep the original call syntax (`style.color32()` / `set_color32(..)`) at
/// every UI site; import the trait alongside the type.
pub trait BubbleBorderPaintColor {
    /// Returns the border colour as a non-premultiplied `Color32`.
    fn color32(self) -> Color32;
    /// Overwrites the border colour from a `Color32`, dropping premultiplication.
    fn set_color32(&mut self, color: Color32);
}

impl BubbleBorderPaintColor for BubbleBorderStyle {
    fn color32(self) -> Color32 {
        Color32::from_rgba_unmultiplied(self.color[0], self.color[1], self.color[2], self.color[3])
    }

    fn set_color32(&mut self, color: Color32) {
        self.color = [color.r(), color.g(), color.b(), color.a()];
    }
}

/// Paints `style`'s border inside `rect` with the given corner radius.
///
/// `rect` is the bubble widget's own rectangle and the stroke is drawn INSIDE it, so the
/// border never grows the widget. The stroke width is fixed
/// ([`DEFAULT_STATUS_BORDER_WIDTH`]); only kind and colour come from `style`.
pub fn paint_bubble_status_border(
    painter: &Painter,
    rect: Rect,
    corner_radius: CornerRadius,
    style: BubbleBorderStyle,
) {
    let color = style.color32();
    match style.kind {
        BubbleBorderKind::Solid => {
            painter.rect_stroke(
                rect,
                corner_radius,
                Stroke::new(DEFAULT_STATUS_BORDER_WIDTH, color),
                egui::StrokeKind::Inside,
            );
        }
        BubbleBorderKind::Dashed => paint_dashed_rect(painter, rect, color),
        BubbleBorderKind::Dotted => paint_dotted_rect(painter, rect, color),
        BubbleBorderKind::Wavy => paint_wavy_rect(painter, rect, color),
    }
}

fn paint_dashed_rect(painter: &Painter, rect: Rect, color: Color32) {
    paint_dashed_segment(
        painter,
        Pos2::new(rect.left(), rect.top()),
        Pos2::new(rect.right(), rect.top()),
        color,
    );
    paint_dashed_segment(
        painter,
        Pos2::new(rect.right(), rect.top()),
        Pos2::new(rect.right(), rect.bottom()),
        color,
    );
    paint_dashed_segment(
        painter,
        Pos2::new(rect.right(), rect.bottom()),
        Pos2::new(rect.left(), rect.bottom()),
        color,
    );
    paint_dashed_segment(
        painter,
        Pos2::new(rect.left(), rect.bottom()),
        Pos2::new(rect.left(), rect.top()),
        color,
    );
}

fn paint_dashed_segment(painter: &Painter, start: Pos2, end: Pos2, color: Color32) {
    let delta = end - start;
    let length = delta.length();
    if length <= 0.0 {
        return;
    }
    let dir = delta / length;
    let mut cursor = 0.0;
    while cursor < length {
        let dash_end = (cursor + DASH_LENGTH_PX).min(length);
        let p1 = start + dir * cursor;
        let p2 = start + dir * dash_end;
        painter.line_segment([p1, p2], Stroke::new(DEFAULT_STATUS_BORDER_WIDTH, color));
        cursor += DASH_LENGTH_PX + DASH_GAP_PX;
    }
}

fn paint_dotted_rect(painter: &Painter, rect: Rect, color: Color32) {
    paint_dotted_segment(
        painter,
        Pos2::new(rect.left(), rect.top()),
        Pos2::new(rect.right(), rect.top()),
        color,
    );
    paint_dotted_segment(
        painter,
        Pos2::new(rect.right(), rect.top()),
        Pos2::new(rect.right(), rect.bottom()),
        color,
    );
    paint_dotted_segment(
        painter,
        Pos2::new(rect.right(), rect.bottom()),
        Pos2::new(rect.left(), rect.bottom()),
        color,
    );
    paint_dotted_segment(
        painter,
        Pos2::new(rect.left(), rect.bottom()),
        Pos2::new(rect.left(), rect.top()),
        color,
    );
}

fn paint_dotted_segment(painter: &Painter, start: Pos2, end: Pos2, color: Color32) {
    let delta = end - start;
    let length = delta.length();
    if length <= 0.0 {
        return;
    }
    let dir = delta / length;
    let mut cursor = 0.0;
    while cursor <= length {
        let center = start + dir * cursor;
        painter.circle_filled(center, DOT_RADIUS_PX, color);
        cursor += DOT_SPACING_PX;
    }
}

fn paint_wavy_rect(painter: &Painter, rect: Rect, color: Color32) {
    paint_wavy_edge(
        painter,
        Pos2::new(rect.left(), rect.top()),
        Pos2::new(rect.right(), rect.top()),
        egui::vec2(0.0, -1.0),
        color,
    );
    paint_wavy_edge(
        painter,
        Pos2::new(rect.right(), rect.top()),
        Pos2::new(rect.right(), rect.bottom()),
        egui::vec2(1.0, 0.0),
        color,
    );
    paint_wavy_edge(
        painter,
        Pos2::new(rect.right(), rect.bottom()),
        Pos2::new(rect.left(), rect.bottom()),
        egui::vec2(0.0, 1.0),
        color,
    );
    paint_wavy_edge(
        painter,
        Pos2::new(rect.left(), rect.bottom()),
        Pos2::new(rect.left(), rect.top()),
        egui::vec2(-1.0, 0.0),
        color,
    );
}

fn paint_wavy_edge(painter: &Painter, start: Pos2, end: Pos2, normal: egui::Vec2, color: Color32) {
    let delta = end - start;
    let length = delta.length();
    if length <= 0.0 {
        return;
    }
    let dir = delta / length;
    let steps = ((length / WAVE_STEP_PX).ceil() as usize).max(2);
    let mut points = Vec::with_capacity(steps + 1);
    for step in 0..=steps {
        let t = step as f32 / steps as f32;
        let dist = t * length;
        let phase = dist / WAVE_LENGTH_PX * std::f32::consts::TAU;
        let offset = normal * phase.sin() * WAVE_AMPLITUDE_PX;
        points.push(start + dir * dist + offset);
    }
    painter.add(egui::Shape::line(
        points,
        Stroke::new(DEFAULT_STATUS_BORDER_WIDTH, color),
    ));
}
