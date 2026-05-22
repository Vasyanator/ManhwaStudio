/*
File: src/widgets/text_edit_plus.rs

Purpose:
Multiline `egui::TextEdit` wrapper with per-character-range text colors and ordered
rounded background highlights.

Main responsibilities:
- build a custom `LayoutJob` so caller-provided character ranges can recolor text;
- paint colored rounded rectangles behind selected text ranges after layout;
- split background rectangles by visual rows and remove rounding where a range continues
  through a line boundary;
- repaint the laid out text over custom backgrounds because `TextEdit` paints before
  widget-level custom overlays are available;
- keep normal `TextEdit` editing behavior and expose the original `TextEditOutput`.

Key structures:
- `TextEditPlus`
- `TextEditPlusTextColor`
- `TextEditPlusBackground`

Notes:
Ranges are expressed in character indices, not bytes, so callers can safely style UTF-8 text.
Backgrounds are painted in caller order; later backgrounds appear above earlier backgrounds.
*/

use egui::epaint::text::{LayoutJob, TextFormat};
use egui::text_edit::TextEditOutput;
use egui::{
    Align, Color32, CornerRadius, FontSelection, Id, Rect, Response, TextBuffer, TextEdit, Ui,
    Vec2, Widget, vec2,
};
use std::hash::Hash;
use std::ops::Range;
use std::sync::Arc;

const DEFAULT_BACKGROUND_RADIUS: u8 = 4;
const HIGHLIGHT_X_PADDING: f32 = 2.0;
const HIGHLIGHT_Y_PADDING: f32 = 1.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEditPlusTextColor {
    pub char_range: Range<usize>,
    pub color: Color32,
}

impl TextEditPlusTextColor {
    #[must_use]
    pub fn new(char_range: Range<usize>, color: Color32) -> Self {
        Self { char_range, color }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEditPlusBackground {
    pub char_range: Range<usize>,
    pub color: Color32,
    pub radius: u8,
}

impl TextEditPlusBackground {
    #[must_use]
    pub fn new(char_range: Range<usize>, color: Color32) -> Self {
        Self {
            char_range,
            color,
            radius: DEFAULT_BACKGROUND_RADIUS,
        }
    }

    #[must_use]
    pub fn radius(mut self, radius: u8) -> Self {
        self.radius = radius;
        self
    }
}

pub struct TextEditPlus<'a> {
    text: &'a mut String,
    hint_text: String,
    id: Option<Id>,
    desired_width: Option<f32>,
    min_size: Option<Vec2>,
    desired_rows: usize,
    horizontal_align: Align,
    vertical_align: Align,
    text_colors: Vec<TextEditPlusTextColor>,
    backgrounds: Vec<TextEditPlusBackground>,
}

impl<'a> TextEditPlus<'a> {
    #[must_use]
    pub fn multiline(text: &'a mut String) -> Self {
        Self {
            text,
            hint_text: String::new(),
            id: None,
            desired_width: None,
            min_size: None,
            desired_rows: 1,
            horizontal_align: Align::LEFT,
            vertical_align: Align::TOP,
            text_colors: Vec::new(),
            backgrounds: Vec::new(),
        }
    }

    #[must_use]
    pub fn id(mut self, id: Id) -> Self {
        self.id = Some(id);
        self
    }

    #[must_use]
    pub fn id_salt(mut self, salt: impl Hash) -> Self {
        self.id = Some(Id::new(salt));
        self
    }

    #[must_use]
    pub fn hint_text(mut self, hint_text: impl Into<String>) -> Self {
        self.hint_text = hint_text.into();
        self
    }

    #[must_use]
    pub fn desired_width(mut self, desired_width: f32) -> Self {
        self.desired_width = Some(desired_width);
        self
    }

    #[must_use]
    pub fn min_size(mut self, min_size: Vec2) -> Self {
        self.min_size = Some(min_size);
        self
    }

    #[must_use]
    pub fn desired_rows(mut self, desired_rows: usize) -> Self {
        self.desired_rows = desired_rows;
        self
    }

    #[must_use]
    pub fn horizontal_align(mut self, align: Align) -> Self {
        self.horizontal_align = align;
        self
    }

    #[must_use]
    pub fn vertical_align(mut self, align: Align) -> Self {
        self.vertical_align = align;
        self
    }

    #[must_use]
    pub fn text_color(mut self, char_range: Range<usize>, color: Color32) -> Self {
        self.text_colors
            .push(TextEditPlusTextColor::new(char_range, color));
        self
    }

    #[must_use]
    pub fn text_colors(mut self, text_colors: Vec<TextEditPlusTextColor>) -> Self {
        self.text_colors = text_colors;
        self
    }

    #[must_use]
    pub fn background(mut self, background: TextEditPlusBackground) -> Self {
        self.backgrounds.push(background);
        self
    }

    #[must_use]
    pub fn backgrounds(mut self, backgrounds: Vec<TextEditPlusBackground>) -> Self {
        self.backgrounds = backgrounds;
        self
    }

    pub fn show(self, ui: &mut Ui) -> TextEditOutput {
        let text_colors = self.text_colors;
        let mut layouter = move |ui: &Ui, buffer: &dyn TextBuffer, wrap_width: f32| {
            build_text_edit_plus_galley(ui, buffer.as_str(), wrap_width, &text_colors)
        };

        let mut edit = TextEdit::multiline(self.text)
            .hint_text(self.hint_text)
            .desired_rows(self.desired_rows)
            .horizontal_align(self.horizontal_align)
            .vertical_align(self.vertical_align);
        if let Some(id) = self.id {
            edit = edit.id(id);
        }
        if let Some(width) = self.desired_width {
            edit = edit.desired_width(width);
        }
        if let Some(min_size) = self.min_size {
            edit = edit.min_size(min_size);
        }

        let output = edit.layouter(&mut layouter).show(ui);
        paint_backgrounds(ui, &output, &self.backgrounds);
        output
    }
}

impl Widget for TextEditPlus<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        self.show(ui).response
    }
}

fn build_text_edit_plus_galley(
    ui: &Ui,
    text: &str,
    wrap_width: f32,
    text_colors: &[TextEditPlusTextColor],
) -> Arc<egui::Galley> {
    let font_id = ui
        .style()
        .override_font_id
        .clone()
        .unwrap_or_else(|| FontSelection::Default.resolve(ui.style()));
    let default_color = ui.visuals().text_color();
    let default_format = TextFormat::simple(font_id.clone(), default_color);
    let char_count = text.chars().count();
    let color_segments = build_color_segments(char_count, default_color, text_colors);
    let byte_offsets = byte_offsets_by_char(text);

    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap_width;
    job.halign = Align::LEFT;
    job.text = text.to_string();

    if color_segments.is_empty() {
        push_layout_section(&mut job, 0..text.len(), default_format);
    } else {
        for segment in color_segments {
            let byte_start = byte_offsets[segment.char_range.start];
            let byte_end = byte_offsets[segment.char_range.end];
            let mut format = default_format.clone();
            format.color = segment.color;
            push_layout_section(&mut job, byte_start..byte_end, format);
        }
    }

    ui.fonts_mut(|fonts| fonts.layout_job(job))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ColorSegment {
    char_range: Range<usize>,
    color: Color32,
}

fn build_color_segments(
    char_count: usize,
    default_color: Color32,
    text_colors: &[TextEditPlusTextColor],
) -> Vec<ColorSegment> {
    if char_count == 0 {
        return Vec::new();
    }

    let mut colors = vec![default_color; char_count];
    for style in text_colors {
        let start = style.char_range.start.min(char_count);
        let end = style.char_range.end.min(char_count);
        if start >= end {
            continue;
        }
        for color in &mut colors[start..end] {
            *color = style.color;
        }
    }

    let mut segments = Vec::new();
    let mut start = 0usize;
    let mut active_color = colors[0];
    for (idx, color) in colors.iter().enumerate().skip(1) {
        if *color != active_color {
            segments.push(ColorSegment {
                char_range: start..idx,
                color: active_color,
            });
            start = idx;
            active_color = *color;
        }
    }
    segments.push(ColorSegment {
        char_range: start..char_count,
        color: active_color,
    });
    segments
}

fn push_layout_section(job: &mut LayoutJob, byte_range: Range<usize>, format: TextFormat) {
    if byte_range.is_empty() {
        return;
    }
    job.sections.push(egui::text::LayoutSection {
        leading_space: 0.0,
        byte_range,
        format,
    });
}

fn paint_backgrounds(ui: &Ui, output: &TextEditOutput, backgrounds: &[TextEditPlusBackground]) {
    if backgrounds.is_empty() || output.galley.rows.is_empty() {
        return;
    }

    let char_count = output.galley.text().chars().count();
    if char_count == 0 {
        return;
    }

    let painter = ui.painter_at(output.response.rect);
    for background in backgrounds {
        let start = background.char_range.start.min(char_count);
        let end = background.char_range.end.min(char_count);
        if start >= end {
            continue;
        }
        paint_background_range(&painter, output, start..end, background);
    }
    painter.galley(
        output.galley_pos,
        Arc::clone(&output.galley),
        ui.visuals().text_color(),
    );
}

fn paint_background_range(
    painter: &egui::Painter,
    output: &TextEditOutput,
    char_range: Range<usize>,
    background: &TextEditPlusBackground,
) {
    let mut row_char_start = 0usize;
    for placed_row in &output.galley.rows {
        let row_char_count = placed_row.char_count_excluding_newline();
        let row_char_end = row_char_start + row_char_count;
        let row_char_end_with_newline = row_char_start + placed_row.char_count_including_newline();
        let overlap_start = char_range.start.max(row_char_start);
        let overlap_end = char_range.end.min(row_char_end);

        if overlap_start < overlap_end {
            let start_column = overlap_start - row_char_start;
            let end_column = overlap_end - row_char_start;
            let x_start = placed_row.x_offset(start_column);
            let x_end = placed_row.x_offset(end_column);
            if x_end > x_start {
                let row_rect = placed_row.rect().translate(output.galley_pos.to_vec2());
                let rect = Rect::from_min_max(
                    row_rect.left_top() + vec2(x_start - HIGHLIGHT_X_PADDING, HIGHLIGHT_Y_PADDING),
                    row_rect.left_top()
                        + vec2(
                            x_end + HIGHLIGHT_X_PADDING,
                            row_rect.height() - HIGHLIGHT_Y_PADDING,
                        ),
                );
                let continues_from_previous_row = char_range.start < row_char_start;
                let continues_to_next_row = char_range.end > row_char_end_with_newline;
                painter.rect_filled(
                    rect,
                    background_corner_radius(
                        background.radius,
                        continues_from_previous_row,
                        continues_to_next_row,
                    ),
                    background.color,
                );
            }
        }

        row_char_start = row_char_end_with_newline;
    }
}

fn background_corner_radius(
    radius: u8,
    continues_from_previous_row: bool,
    continues_to_next_row: bool,
) -> CornerRadius {
    CornerRadius {
        nw: if continues_from_previous_row {
            0
        } else {
            radius
        },
        sw: if continues_from_previous_row {
            0
        } else {
            radius
        },
        ne: if continues_to_next_row { 0 } else { radius },
        se: if continues_to_next_row { 0 } else { radius },
    }
}

fn byte_offsets_by_char(text: &str) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(text.chars().count() + 1);
    offsets.push(0);
    offsets.extend(text.char_indices().map(|(idx, ch)| idx + ch.len_utf8()));
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_offsets_handle_utf8() {
        let offsets = byte_offsets_by_char("При");
        assert_eq!(offsets, vec![0, 2, 4, 6]);
    }

    #[test]
    fn later_text_color_overrides_previous_color() {
        let blue = Color32::from_rgb(0, 100, 255);
        let pink = Color32::from_rgb(255, 100, 180);
        let segments = build_color_segments(
            6,
            Color32::WHITE,
            &[
                TextEditPlusTextColor::new(1..5, blue),
                TextEditPlusTextColor::new(3..4, pink),
            ],
        );

        assert_eq!(
            segments,
            vec![
                ColorSegment {
                    char_range: 0..1,
                    color: Color32::WHITE,
                },
                ColorSegment {
                    char_range: 1..3,
                    color: blue,
                },
                ColorSegment {
                    char_range: 3..4,
                    color: pink,
                },
                ColorSegment {
                    char_range: 4..5,
                    color: blue,
                },
                ColorSegment {
                    char_range: 5..6,
                    color: Color32::WHITE,
                },
            ]
        );
    }

    #[test]
    fn continuing_background_removes_line_boundary_rounding() {
        assert_eq!(
            background_corner_radius(4, false, true),
            CornerRadius {
                nw: 4,
                ne: 0,
                sw: 4,
                se: 0,
            }
        );
        assert_eq!(
            background_corner_radius(4, true, false),
            CornerRadius {
                nw: 0,
                ne: 4,
                sw: 0,
                se: 4,
            }
        );
    }
}
