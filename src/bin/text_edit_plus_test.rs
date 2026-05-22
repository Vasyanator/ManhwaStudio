#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

/*
File: src/bin/text_edit_plus_test.rs

Purpose:
Focused egui tester for the `TextEditPlus` widget.

Main responsibilities:
- run a small standalone window with editable sample text;
- demonstrate per-range text color changes;
- demonstrate ordered overlapping rounded text backgrounds;
- expose ranges and colors as controls so line-boundary behavior can be tested manually.

Key structures:
- `TextEditPlusTestApp`
- `RangeControl`

Notes:
This binary includes the widget source directly because the project currently has no library
target. The same file is also compiled through `src/widgets/mod.rs` by the main application.
*/

// The shared widget is included by path for this standalone demo because this crate has no
// library target; some builder methods are exercised by the main app instead of this binary.
#[allow(dead_code)]
#[path = "../widgets/text_edit_plus.rs"]
mod text_edit_plus;

use eframe::egui;
use egui::{Color32, DragValue, RichText};
use text_edit_plus::{TextEditPlus, TextEditPlusBackground, TextEditPlusTextColor};

const APP_TITLE: &str = "text_edit_plus_test";
const DEFAULT_TEXT: &str = "Пример\nпереноса строки с выделенным фоном";

fn main() {
    let run_result = eframe::run_native(
        APP_TITLE,
        eframe::NativeOptions::default(),
        Box::new(|cc| Ok(Box::new(TextEditPlusTestApp::new(cc)))),
    );

    if let Err(err) = run_result {
        eprintln!("[{APP_TITLE}] failed to start: {err}");
    }
}

#[derive(Debug, Clone)]
struct RangeControl {
    start: usize,
    end: usize,
    color: Color32,
}

impl RangeControl {
    fn text_color(&self, char_count: usize) -> Option<TextEditPlusTextColor> {
        normalized_range(self.start, self.end, char_count)
            .map(|range| TextEditPlusTextColor::new(range, self.color))
    }

    fn background(&self, char_count: usize) -> Option<TextEditPlusBackground> {
        normalized_range(self.start, self.end, char_count)
            .map(|range| TextEditPlusBackground::new(range, self.color))
    }
}

#[derive(Debug)]
struct TextEditPlusTestApp {
    text: String,
    desired_width: f32,
    text_color: RangeControl,
    blue_background: RangeControl,
    pink_background: RangeControl,
}

impl TextEditPlusTestApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            text: DEFAULT_TEXT.to_string(),
            desired_width: 360.0,
            text_color: RangeControl {
                start: 0,
                end: 6,
                color: Color32::from_rgb(25, 80, 220),
            },
            blue_background: RangeControl {
                start: 1,
                end: 5,
                color: Color32::from_rgba_unmultiplied(80, 190, 255, 130),
            },
            pink_background: RangeControl {
                start: 3,
                end: 4,
                color: Color32::from_rgba_unmultiplied(255, 100, 190, 170),
            },
        }
    }
}

impl eframe::App for TextEditPlusTestApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::SidePanel::left("controls_panel")
            .resizable(false)
            .default_width(280.0)
            .show(ctx, |ui| {
                ui.heading("TextEditPlus");
                ui.label(format!("Символов: {}", self.text.chars().count()));
                ui.add_space(8.0);
                ui.label("Ширина редактора");
                ui.add(egui::Slider::new(&mut self.desired_width, 180.0..=720.0));
                ui.separator();
                draw_range_control(ui, "Цвет текста", &mut self.text_color);
                ui.separator();
                draw_range_control(ui, "Голубой фон", &mut self.blue_background);
                ui.separator();
                draw_range_control(ui, "Розовый фон выше", &mut self.pink_background);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Редактор");
            ui.label(
                RichText::new("Фоны рисуются по порядку: розовый элемент находится выше голубого.")
                    .small(),
            );
            ui.add_space(8.0);

            let char_count = self.text.chars().count();
            let text_colors: Vec<_> = self.text_color.text_color(char_count).into_iter().collect();
            let backgrounds: Vec<_> = [
                self.blue_background.background(char_count),
                self.pink_background.background(char_count),
            ]
            .into_iter()
            .flatten()
            .collect();

            TextEditPlus::multiline(&mut self.text)
                .id_salt("text_edit_plus_test")
                .desired_width(self.desired_width)
                .desired_rows(14)
                .text_colors(text_colors)
                .backgrounds(backgrounds)
                .show(ui);
        });
    }
}

fn draw_range_control(ui: &mut egui::Ui, label: &str, control: &mut RangeControl) {
    ui.label(label);
    ui.horizontal(|ui| {
        ui.label("start");
        ui.add(DragValue::new(&mut control.start).range(0..=10_000));
        ui.label("end");
        ui.add(DragValue::new(&mut control.end).range(0..=10_000));
    });
    ui.color_edit_button_srgba(&mut control.color);
}

fn normalized_range(start: usize, end: usize, char_count: usize) -> Option<std::ops::Range<usize>> {
    let start = start.min(char_count);
    let end = end.min(char_count);
    (start < end).then_some(start..end)
}
