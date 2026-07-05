/*
File: src/bin/tutorial_test/main.rs

Purpose:
Standalone debug binary for developing and visually verifying the tutorial /
onboarding overlay engine (`tutorial.rs`). It opens a tabbed egui window whose
controls are built the SAME way the main application builds them — reusing the
real `WheelSlider` / `WheelSpinBox` / `WheelComboBox` widgets via `#[path]`
mounts — so a step only has to point the tutorial at an element's key, without
reworking the surrounding UI.

How it reaches shared code:
The package has no library target, so the reusable wheel widgets are mounted with
`#[path = "../../widgets/..."]`. The wheel widgets reference
`super::wheel_input_guard`, so they are mounted as children of one `mod widgets`
parent that also mounts `wheel_input_guard`.

Key structures:
- `TutorialTestApp`: window state + `TutorialRegistry` + `Tutorial`.
- `Tab`: which demo tab is shown.

Key functions:
- `main`: eframe entry; dark theme; window options.
- `build_steps`: the demo tutorial script pointing at real widget keys.
- `TutorialTestApp::ui`: builds the tabbed UI, records target rects, renders the
  overlay.

Notes:
Run with the inspection harness for MCP verification:
  EGUI_INSPECTION=1 cargo run --bin tutorial_test --features inspection
*/

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#![allow(dead_code)]

use eframe::egui::{self, Align, Layout};

// Reusable widgets from the main application. The package has no lib target, so
// they are physically mounted with `#[path]`. The wheel widgets reference
// `super::wheel_input_guard`, so all four are mounted at the crate root (their
// `super` then resolves to `wheel_input_guard` here). Nested inline-mod mounts
// fail because their base directory is a `widgets/` dir that does not exist.
#[path = "../../widgets/wheel_input_guard.rs"]
mod wheel_input_guard;
#[path = "../../widgets/wheel_slider.rs"]
mod wheel_slider;
#[path = "../../widgets/wheel_spin_box.rs"]
mod wheel_spin_box;
#[path = "../../widgets/wheel_combo_box.rs"]
mod wheel_combo_box;

// The engine has been promoted to `src/tutorial/engine.rs` so launcher and studio
// surfaces can `use crate::tutorial`. The demo mounts the real engine directly so
// the demo and production overlay can never diverge.
#[path = "../../tutorial/engine.rs"]
mod tutorial;

use tutorial::{Tutorial, TutorialRegistry, TutorialStep};
use wheel_combo_box::WheelComboBox;
use wheel_slider::WheelSlider;
use wheel_spin_box::WheelSpinBox;

const APP_TITLE: &str = "Tutorial Overlay Test";

/// OCR engine names shown in the demo combo box (label parity with the app).
const OCR_ENGINES: [&str; 4] = ["MangaOCR", "EasyOCR", "PaddleOCR", "Surya"];

/// Which demo tab is currently shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Tools,
    Text,
    Settings,
}

/// Window + tutorial state for the test binary.
struct TutorialTestApp {
    active_tab: Tab,
    registry: TutorialRegistry,
    // The tutorial's `on_enter` side effects mutate a `Tab` (the app state it
    // needs to drive), so the context type is `Tab`.
    tutorial: Tutorial<Tab>,

    brush_size: f32,
    opacity: f32,
    iterations: i32,
    engine_idx: usize,
    spell_check: bool,
    deep_intercept: bool,
    cloak: bool,
    notes: String,
    title_text: String,
    radio_choice: usize,
}

impl TutorialTestApp {
    /// Build the app with default demo state and the demo tutorial script.
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            active_tab: Tab::Tools,
            registry: TutorialRegistry::default(),
            tutorial: Tutorial::new(build_steps()),
            brush_size: 24.0,
            opacity: 80.0,
            iterations: 3,
            engine_idx: 0,
            spell_check: true,
            deep_intercept: false,
            cloak: true,
            notes: String::from("Пример многострочного текста…"),
            title_text: String::from("Глава 1"),
            radio_choice: 0,
        }
    }
}

/// The demo tutorial script. Each step points at element key(s) recorded by the
/// UI below; the group step shows highlighting the union of two elements.
fn build_steps() -> Vec<TutorialStep<Tab>> {
    vec![
        TutorialStep::new(
            ["tab_text"],
            "Вкладки",
            "Здесь переключаются разделы. Подсветка работает и по элементам панели вкладок.",
        ),
        // The tutorial opens the «Текст» tab itself, then highlights a field on
        // it — driving app state via `on_enter`, without touching the tab code.
        TutorialStep::new(
            ["text_title"],
            "Смена вкладки",
            "Туториал сам открыл вкладку «Текст» и подсветил поле — состояние меняется наложением, без правок UI.",
        )
        .on_enter(|tab| *tab = Tab::Text),
        TutorialStep::new(
            ["brush_size"],
            "Размер кисти",
            "Одиночный элемент подсвечивается пунктиром с отступом, остальной экран затеняется.",
        )
        // Switch back to «Инструменты» so the next highlights are visible.
        .on_enter(|tab| *tab = Tab::Tools),
        TutorialStep::new(
            ["btn_apply", "btn_cancel"],
            "Группа элементов",
            "Можно указать несколько ключей — подсветится их общий прямоугольник.",
        ),
        TutorialStep::new(
            ["t_right"],
            "Зона: право",
            "Центр элемента в правой зоне — стрелка приходит прямой в его левый бок.",
        ),
        TutorialStep::new(
            ["t_bottom"],
            "Зона: низ",
            "Нижняя зона — стрелка приходит прямой сверху в верхний бок элемента.",
        ),
        TutorialStep::new(
            ["t_br"],
            "Зона: угол",
            "Угловая зона — стрелка приходит под 45° в ближний к центру угол элемента.",
        ),
        TutorialStep::new(
            ["spellcheck_checkbox"],
            "Готово",
            "Всё под затенением инертно: клики и наведение поглощаются одним хитбоксом на весь экран. Выделенный элемент только подсвечен.",
        ),
    ]
}

impl eframe::App for TutorialTestApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Split the borrow so the UI can write into `registry` while mutating the
        // individual widget-state fields at the same time.
        let Self {
            active_tab,
            registry,
            tutorial,
            brush_size,
            opacity,
            iterations,
            engine_idx,
            spell_check,
            deep_intercept,
            cloak,
            notes,
            title_text,
            radio_choice,
        } = self;

        // Run any pending step `on_enter` BEFORE building the UI, so a step that
        // opens a tab takes effect this frame and its highlight target is drawn.
        tutorial.sync(active_tab);

        registry.begin_frame();

        egui::Panel::top("tutorial_test_top").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // Tab bar built with the same idiom as the app's selectable tabs.
                let tools = ui.selectable_label(*active_tab == Tab::Tools, "Инструменты");
                if tools.clicked() {
                    *active_tab = Tab::Tools;
                }

                let text = ui.selectable_label(*active_tab == Tab::Text, "Текст");
                if text.clicked() {
                    *active_tab = Tab::Text;
                }
                // Record the "Текст" tab so a step can point at it.
                registry.mark("tab_text", text.rect);

                let settings = ui.selectable_label(*active_tab == Tab::Settings, "Настройки");
                if settings.clicked() {
                    *active_tab = Tab::Settings;
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if tutorial.is_active() {
                        if ui.button("Остановить обучение").clicked() {
                            tutorial.stop();
                        }
                    } else if ui.button("Начать обучение").clicked() {
                        tutorial.start();
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ui, |ui| match *active_tab {
            Tab::Tools => {
                ui.heading("Инструменты");
                ui.add_space(8.0);

                let brush =
                    ui.add(WheelSlider::new(brush_size, 1.0..=100.0).text("Размер кисти (px)"));
                registry.mark("brush_size", brush.rect);

                ui.add(WheelSlider::new(opacity, 0.0..=100.0).text("Непрозрачность (%)"));

                ui.horizontal(|ui| {
                    ui.label("Итераций:");
                    ui.add(WheelSpinBox::new(iterations).range(1..=20));
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Движок OCR:");
                    let combo = WheelComboBox::from_id_salt("engine_ocr").show_index(
                        ui,
                        engine_idx,
                        OCR_ENGINES.len(),
                        |i| OCR_ENGINES[i],
                    );
                    registry.mark("engine_combo", combo.rect);
                });

                ui.add_space(6.0);
                let spell = ui.checkbox(spell_check, "Проверять орфографию");
                registry.mark("spellcheck_checkbox", spell.rect);

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let apply = ui.button("Применить");
                    registry.mark("btn_apply", apply.rect);
                    let cancel = ui.button("Отмена");
                    registry.mark("btn_cancel", cancel.rect);
                });

                ui.add_space(12.0);
                ui.label("Заметки:");
                ui.add(
                    egui::TextEdit::multiline(notes)
                        .desired_rows(4)
                        .desired_width(400.0),
                );

                // Scattered targets placed in the empty part of the panel so the
                // tutorial can point at different viewport zones (right-centre,
                // bottom-centre, bottom-right corner) and show straight vs 45°
                // arrows.
                let panel = ui.max_rect();
                let place = |ui: &mut egui::Ui, center: egui::Pos2, text: &str| {
                    let rect = egui::Rect::from_center_size(center, egui::vec2(150.0, 32.0));
                    ui.put(rect, egui::Button::new(text)).rect
                };
                registry.mark(
                    "t_right",
                    place(
                        ui,
                        egui::pos2(panel.right() - 90.0, panel.center().y),
                        "Цель: право",
                    ),
                );
                registry.mark(
                    "t_bottom",
                    place(
                        ui,
                        egui::pos2(panel.center().x, panel.bottom() - 40.0),
                        "Цель: низ",
                    ),
                );
                registry.mark(
                    "t_br",
                    place(
                        ui,
                        egui::pos2(panel.right() - 90.0, panel.bottom() - 40.0),
                        "Цель: угол",
                    ),
                );
            }
            Tab::Text => {
                ui.heading("Текст");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Заголовок:");
                    let title = ui.text_edit_singleline(title_text);
                    registry.mark("text_title", title.rect);
                });
                ui.add_space(8.0);
                ui.label("Выравнивание:");
                ui.radio_value(radio_choice, 0, "По левому краю");
                ui.radio_value(radio_choice, 1, "По центру");
                ui.radio_value(radio_choice, 2, "По правому краю");
            }
            Tab::Settings => {
                ui.heading("Настройки");
                ui.add_space(8.0);
                ui.checkbox(deep_intercept, "Глубокий перехват");
                ui.checkbox(cloak, "Cloak по умолчанию");
                ui.add_space(8.0);
                ui.label("Раздел настроек — заполнитель для теста подсветки.");
            }
        });

        // Draw the overlay last so it sits above the panels built this frame.
        // The full-viewport hitbox is a higher egui layer, so it occludes the
        // widgets beneath (including WheelSlider, now that it respects overlap).
        tutorial.render(&ctx, registry);
    }
}

fn main() {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([980.0, 680.0]),
        ..Default::default()
    };
    let run_result = eframe::run_native(
        APP_TITLE,
        native_options,
        Box::new(|cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            Ok(Box::new(TutorialTestApp::new(cc)))
        }),
    );
    if let Err(err) = run_result {
        eprintln!("[{APP_TITLE}] failed to start: {err}");
    }
}
