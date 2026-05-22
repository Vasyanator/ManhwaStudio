/*
FILE OVERVIEW: src/tabs/translation/panels/text_detector.rs
Translation panel UI for text detection controls.

Main types:
- `TextDetectorAlgorithm`: detector backend mode selector (`Classic` /
  `PaddleOcr` / `Ai` / `Surya`).
- `TextDetectorPanelOptions`: editable UI options for detector run.
- `TextDetectorPanelActions`: one-frame UI actions returned to tab logic.

Flow:
- `draw_text_detector_panel`: renders status, options and action buttons.
- AI mode advanced params keep CTD-specific quality/runtime knobs; shared mask
  dilation lives in the common section with other detector-wide options; device selection
  is configured globally in `Настройки -> ИИ бэкенд`.
- Строка кнопок под чекбоксами: вход/выход из режима редактирования строк
  детектора и режима редактирования маски.
*/

use crate::widgets::{WheelComboBox, WheelSpinBox};

const PYTORCH_UNAVAILABLE_HINT: &str = "PyTorch не установлен";

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum TextDetectorAlgorithm {
    #[default]
    Classic,
    PaddleOcr,
    Ai,
    Surya,
}

impl TextDetectorAlgorithm {
    pub fn key(self) -> &'static str {
        match self {
            TextDetectorAlgorithm::Classic => "classic",
            TextDetectorAlgorithm::PaddleOcr => "paddleocr",
            TextDetectorAlgorithm::Ai => "ai",
            TextDetectorAlgorithm::Surya => "surya",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            TextDetectorAlgorithm::Classic => "Классический",
            TextDetectorAlgorithm::PaddleOcr => "PaddleOCR",
            TextDetectorAlgorithm::Ai => "ComicTextDetector",
            TextDetectorAlgorithm::Surya => "Surya",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TextDetectorPanelOptions {
    pub algorithm: TextDetectorAlgorithm,
    pub draw_lines: bool,
    pub draw_mask: bool,
    pub block_expand_px: i32,
    pub mask_dilate_size: i32,
    pub merge_gap_px: i32,
    pub ai_detect_size: i32,
    pub ai_det_rearrange_max_batches: i32,
    pub ai_font_size_multiplier: f32,
    pub ai_font_size_max: f32,
    pub ai_font_size_min: f32,
}

impl Default for TextDetectorPanelOptions {
    fn default() -> Self {
        Self {
            algorithm: TextDetectorAlgorithm::Classic,
            draw_lines: true,
            draw_mask: true,
            block_expand_px: 0,
            mask_dilate_size: 2,
            merge_gap_px: 5,
            ai_detect_size: 1280,
            ai_det_rearrange_max_batches: 4,
            ai_font_size_multiplier: 1.0,
            ai_font_size_max: -1.0,
            ai_font_size_min: -1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TextDetectorPanelActions {
    pub detect_current: bool,
    pub detect_all: bool,
    pub ocr_current: bool,
    pub ocr_all: bool,
    pub save_results: bool,
    pub clear_results: bool,
    pub toggle_edit_lines_mode: bool,
    pub toggle_edit_mask_mode: bool,
    pub options_changed: bool,
}

// Parameters represent distinct required inputs with no natural grouping.
#[allow(clippy::too_many_arguments)]
pub fn draw_text_detector_panel(
    ui: &mut egui::Ui,
    options: &mut TextDetectorPanelOptions,
    status_text: &str,
    status_color: egui::Color32,
    progress: Option<(usize, usize)>,
    detect_busy: bool,
    ocr_busy: bool,
    has_pages: bool,
    can_detect: bool,
    torch_available: Option<bool>,
    can_ocr_current: bool,
    can_ocr_all: bool,
    can_save: bool,
    edit_lines_mode: bool,
    edit_mask_mode: bool,
) -> TextDetectorPanelActions {
    let mut actions = TextDetectorPanelActions::default();
    let torch_mode_available = torch_available.unwrap_or(true);

    ui.heading("Массовый детектор текста");
    ui.colored_label(status_color, status_text);
    if let Some((done, total)) = progress
        && total > 0
    {
        ui.small(format!("{done} / {total}"));
    }
    ui.separator();

    ui.horizontal(|ui| {
        ui.label("Алгоритм:").on_hover_text("Классический: быстрый локальный эвристический поиск.\nPaddleOCR: Python backend с Paddle det-моделью и точной маской текста.\nИИ: CTD backend, более медленный, но часто лучше группирует сложные блоки."
            .to_owned()
            + "\nSurya: low-level Surya detector через PyTorch backend, строит строки и бинарную маску из heatmap."
            );
        WheelComboBox::from_id_salt("translation_text_detector_algorithm")
            .selected_text(options.algorithm.title())
            .show_ui(ui, |ui| {
                actions.options_changed |= ui
                    .selectable_value(
                        &mut options.algorithm,
                        TextDetectorAlgorithm::Classic,
                        TextDetectorAlgorithm::Classic.title(),
                    )
                    .changed();
                actions.options_changed |= ui
                    .selectable_value(
                        &mut options.algorithm,
                        TextDetectorAlgorithm::PaddleOcr,
                        TextDetectorAlgorithm::PaddleOcr.title(),
                    )
                    .changed();
                let response = ui.add_enabled(
                    torch_mode_available,
                    egui::Button::new(TextDetectorAlgorithm::Ai.title())
                        .selected(options.algorithm == TextDetectorAlgorithm::Ai),
                );
                let response = if torch_mode_available {
                    response
                } else {
                    response.on_disabled_hover_text(
                        egui::RichText::new(PYTORCH_UNAVAILABLE_HINT)
                            .color(egui::Color32::from_rgb(240, 102, 102)),
                    )
                };
                if response.clicked() {
                    options.algorithm = TextDetectorAlgorithm::Ai;
                    actions.options_changed = true;
                }
                let response = ui.add_enabled(
                    torch_mode_available,
                    egui::Button::new(TextDetectorAlgorithm::Surya.title())
                        .selected(options.algorithm == TextDetectorAlgorithm::Surya),
                );
                let response = if torch_mode_available {
                    response
                } else {
                    response.on_disabled_hover_text(
                        egui::RichText::new(PYTORCH_UNAVAILABLE_HINT)
                            .color(egui::Color32::from_rgb(240, 102, 102)),
                    )
                };
                if response.clicked() {
                    options.algorithm = TextDetectorAlgorithm::Surya;
                    actions.options_changed = true;
                }
            });
    });
    if matches!(
        options.algorithm,
        TextDetectorAlgorithm::Ai | TextDetectorAlgorithm::Surya
    ) && !torch_mode_available
    {
        ui.colored_label(
            egui::Color32::from_rgb(240, 102, 102),
            PYTORCH_UNAVAILABLE_HINT,
        );
    }

    actions.options_changed |= ui
        .checkbox(&mut options.draw_lines, "Показывать найденные блоки")
        .on_hover_text("Показывать синие прямоугольники блоков и зелёные прямоугольники строк")
        .changed();
    actions.options_changed |= ui
    .checkbox(&mut options.draw_mask, "Показывать маску")
    .on_hover_text("Показывать красную маску текста, которая в дальнейшем может быть использована для быстрого клина")
    .changed();
    ui.horizontal(|ui| {
        let edit_lines_label = if edit_lines_mode {
            "Выйти из режима изменения строк"
        } else {
            "Изменить найденные строки"
        };
        if ui.button(edit_lines_label).clicked() {
            actions.toggle_edit_lines_mode = true;
        }
    });
    ui.horizontal(|ui| {
        let edit_mask_label = if edit_mask_mode {
            "Выйти из режима изменения маски"
        } else {
            "Изменить маску текста"
        };
        if ui.button(edit_mask_label).clicked() {
            actions.toggle_edit_mask_mode = true;
        }
    });

    ui.horizontal(|ui| {
        ui.label("Расширение блока:")
        .on_hover_text("Расширять зелёные прямоугольники на столько пикселей, чтобы они лучше объединялись в один блок");
        let mut value = options.block_expand_px;
        if ui
            .add(WheelSpinBox::new(&mut value).range(0..=200).speed(0.2))
            .changed()
        {
            options.block_expand_px = value.clamp(0, 200);
            actions.options_changed = true;
        }
    });

    ui.horizontal(|ui| {
        ui.label("Расширение маски:")
            .on_hover_text("Расширять красную маску текста на указанное число пикселей после детекции. Работает для всех алгоритмов.");
        let mut value = options.mask_dilate_size;
        if ui
            .add(WheelSpinBox::new(&mut value).range(0..=30).speed(0.2))
            .changed()
        {
            options.mask_dilate_size = value.clamp(0, 30);
            actions.options_changed = true;
        }
    });

    ui.horizontal(|ui| {
        ui.label("Дистанция объединения:")
        .on_hover_text("На каком расстоянии (в пикселях) объединять не соприкасающиеся зелёные строки в один блок");
        let mut value = options.merge_gap_px;
        if ui
            .add(WheelSpinBox::new(&mut value).range(0..=200).speed(0.2))
            .changed()
        {
            options.merge_gap_px = value.clamp(0, 200);
            actions.options_changed = true;
        }
    });

    if options.algorithm == TextDetectorAlgorithm::Ai {
        ui.separator();
        egui::CollapsingHeader::new("Продвинутые настройки")
            .id_salt("ctd_params") // чтобы состояние свёрнутости сохранялось стабильно
            .default_open(false) // по умолчанию раскрыто
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Размер детекции:");
                    let mut value = options.ai_detect_size;
                    if ui
                        .add(WheelSpinBox::new(&mut value).range(896..=2048).speed(1.0))
                        .changed()
                    {
                        options.ai_detect_size = value.clamp(896, 2048);
                        actions.options_changed = true;
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Макс. батчи rearrange:");
                    let mut value = options.ai_det_rearrange_max_batches;
                    if ui
                        .add(WheelSpinBox::new(&mut value).range(1..=64).speed(0.2))
                        .changed()
                    {
                        options.ai_det_rearrange_max_batches = value.clamp(1, 64);
                        actions.options_changed = true;
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Множитель шрифта:");
                    let mut value = options.ai_font_size_multiplier;
                    if ui
                        .add(WheelSpinBox::new(&mut value).range(0.1..=8.0).speed(0.05))
                        .changed()
                    {
                        options.ai_font_size_multiplier = value.clamp(0.1, 8.0);
                        actions.options_changed = true;
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Макс. шрифт:");
                    let mut value = options.ai_font_size_max;
                    if ui
                        .add(WheelSpinBox::new(&mut value).range(-1.0..=500.0).speed(0.2))
                        .changed()
                    {
                        options.ai_font_size_max = value.clamp(-1.0, 500.0);
                        actions.options_changed = true;
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Мин. шрифт:");
                    let mut value = options.ai_font_size_min;
                    if ui
                        .add(WheelSpinBox::new(&mut value).range(-1.0..=500.0).speed(0.2))
                        .changed()
                    {
                        options.ai_font_size_min = value.clamp(-1.0, 500.0);
                        actions.options_changed = true;
                    }
                });
            });
    }

    ui.separator();
    let detect_enabled = has_pages && can_detect && !detect_busy && !ocr_busy;
    if ui
        .add_enabled(
            detect_enabled,
            egui::Button::new("Выделить весь текст на текущей странице"),
        )
        .clicked()
    {
        actions.detect_current = true;
    }
    if ui
        .add_enabled(detect_enabled, egui::Button::new("Выделить весь текст"))
        .clicked()
    {
        actions.detect_all = true;
    }
    if ui
        .add_enabled(
            can_save && !detect_busy && !ocr_busy,
            egui::Button::new("Сохранить выделение"),
        )
        .clicked()
    {
        actions.save_results = true;
    }
    if ui.button("Очистить результаты").clicked() {
        actions.clear_results = true;
    }

    ui.separator();
    if ui
        .add_enabled(
            can_ocr_current && !detect_busy && !ocr_busy,
            egui::Button::new("Распознать выделенное на текущей странице"),
        )
        .clicked()
    {
        actions.ocr_current = true;
    }
    if ui
        .add_enabled(
            can_ocr_all && !detect_busy && !ocr_busy,
            egui::Button::new("Распознать весь выделенный текст"),
        )
        .clicked()
    {
        actions.ocr_all = true;
    }

    if detect_busy {
        ui.small("Детектор работает в фоне...");
    }
    if ocr_busy {
        ui.small("Выполняется распознавание выделенного текста...");
    }
    if !can_detect {
        let disabled_hint = match options.algorithm {
            TextDetectorAlgorithm::Classic => None,
            TextDetectorAlgorithm::PaddleOcr => Some("PaddleOCR-детектор отключён флагом --no-ai."),
            TextDetectorAlgorithm::Ai => Some("ИИ-детектор отключён флагом --no-ai."),
            TextDetectorAlgorithm::Surya => Some("Surya-детектор отключён флагом --no-ai."),
        };
        if let Some(hint) = disabled_hint {
            ui.small(hint);
        }
    }

    actions
}
