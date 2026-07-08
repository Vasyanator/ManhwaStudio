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
- Algorithm selection uses frameless [`AiButton`] toggles (one per algorithm, wrapped
  ~3 per row) that self-gate on each algorithm's runtime capability and show a runtime
  marker; `Classic` has no runtime dependency and is a plain selectable.
- AI mode advanced params keep CTD-specific quality/runtime knobs; shared mask
  dilation lives in the common section with other detector-wide options; device selection
  is configured globally in `Настройки -> ИИ бэкенд`.
- Строка кнопок под чекбоксами: вход/выход из режима редактирования строк
  детектора и режима редактирования маски.
*/

use crate::widgets::{AiButton, AiRequirement, WheelSpinBox};

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum TextDetectorAlgorithm {
    #[default]
    Classic,
    PaddleOcr,
    Ai,
    Surya,
}

/// Selectable detector algorithms in display order (wrapped ~3 per row).
const DETECTOR_ALGORITHMS: [TextDetectorAlgorithm; 4] = [
    TextDetectorAlgorithm::Classic,
    TextDetectorAlgorithm::PaddleOcr,
    TextDetectorAlgorithm::Ai,
    TextDetectorAlgorithm::Surya,
];

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

/// Runtime capability an algorithm's selection button gates on, or `None` when it
/// has no local-runtime dependency (`Classic` is a pure local heuristic).
/// `PaddleOCR` runs on onnxruntime (native or backend); `ComicTextDetector` and
/// `Surya` run on PyTorch in the backend.
fn algorithm_requirement(algorithm: TextDetectorAlgorithm) -> Option<AiRequirement> {
    match algorithm {
        TextDetectorAlgorithm::Classic => None,
        TextDetectorAlgorithm::PaddleOcr => Some(AiRequirement::Onnx),
        TextDetectorAlgorithm::Ai => Some(AiRequirement::Torch),
        TextDetectorAlgorithm::Surya => Some(AiRequirement::Torch),
    }
}

/// Short runtime marker badge for an algorithm ("Torch"/"ONNX"), or `None` for the
/// dependency-free `Classic` algorithm.
fn algorithm_marker(algorithm: TextDetectorAlgorithm) -> Option<&'static str> {
    match algorithm {
        TextDetectorAlgorithm::Classic => None,
        TextDetectorAlgorithm::PaddleOcr => Some("ONNX"),
        TextDetectorAlgorithm::Ai => Some("Torch"),
        TextDetectorAlgorithm::Surya => Some("Torch"),
    }
}

/// Descriptive hover text for an algorithm (moved off the removed "Алгоритм:" label
/// onto each button).
fn algorithm_hover(algorithm: TextDetectorAlgorithm) -> &'static str {
    match algorithm {
        TextDetectorAlgorithm::Classic => "Классический: быстрый локальный эвристический поиск.",
        TextDetectorAlgorithm::PaddleOcr => {
            "PaddleOCR: Paddle det-модель на onnxruntime с точной маской текста."
        }
        TextDetectorAlgorithm::Ai => {
            "ComicTextDetector: CTD backend, медленнее, но часто лучше группирует сложные блоки."
        }
        TextDetectorAlgorithm::Surya => {
            "Surya: low-level Surya detector через PyTorch backend, строит строки и бинарную маску из heatmap."
        }
    }
}

/// Renders one detector-algorithm selection button. Runtime algorithms use a
/// frameless [`AiButton`] that self-gates on [`algorithm_requirement`] and shows a
/// runtime marker; `Classic` (no requirement) is a plain frameless selectable.
/// Returns `true` when the click selected this algorithm.
fn algorithm_select_button(
    ui: &mut egui::Ui,
    selected: &mut TextDetectorAlgorithm,
    algorithm: TextDetectorAlgorithm,
) -> bool {
    let is_selected = *selected == algorithm;
    let response = match algorithm_requirement(algorithm) {
        Some(requirement) => {
            let mut btn = AiButton::new(algorithm.title(), requirement)
                .selected(is_selected)
                .frame(false);
            if let Some(marker) = algorithm_marker(algorithm) {
                btn = btn.marker(marker);
            }
            btn.draw(ui).response
        }
        None => ui.selectable_label(is_selected, algorithm.title()),
    };
    let response = response.on_hover_text(algorithm_hover(algorithm));
    if response.clicked() {
        *selected = algorithm;
        return true;
    }
    false
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
    can_ocr_current: bool,
    can_ocr_all: bool,
    can_save: bool,
    edit_lines_mode: bool,
    edit_mask_mode: bool,
) -> TextDetectorPanelActions {
    let mut actions = TextDetectorPanelActions::default();

    ui.heading("Массовый детектор текста");
    ui.colored_label(status_color, status_text);
    if let Some((done, total)) = progress
        && total > 0
    {
        ui.small(format!("{done} / {total}"));
    }
    ui.separator();

    // Algorithm selector: frameless self-gating toggle buttons (no "Алгоритм:"
    // label so ~3 fit per row). Each runtime algorithm shows its marker and disables
    // with its reason when unavailable; the currently-selected disabled algorithm's
    // action is additionally covered by the `can_detect` hint below.
    ui.horizontal_wrapped(|ui| {
        for algorithm in DETECTOR_ALGORITHMS {
            if algorithm_select_button(ui, &mut options.algorithm, algorithm) {
                actions.options_changed = true;
            }
        }
    });

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

#[cfg(test)]
mod tests {
    use super::{
        AiRequirement, DETECTOR_ALGORITHMS, TextDetectorAlgorithm, algorithm_marker,
        algorithm_requirement,
    };

    #[test]
    fn algorithm_requirements_match_runtime() {
        assert_eq!(algorithm_requirement(TextDetectorAlgorithm::Classic), None);
        assert_eq!(
            algorithm_requirement(TextDetectorAlgorithm::PaddleOcr),
            Some(AiRequirement::Onnx)
        );
        assert_eq!(
            algorithm_requirement(TextDetectorAlgorithm::Ai),
            Some(AiRequirement::Torch)
        );
        assert_eq!(
            algorithm_requirement(TextDetectorAlgorithm::Surya),
            Some(AiRequirement::Torch)
        );
    }

    #[test]
    fn algorithm_markers_present_only_for_runtime_algorithms() {
        assert_eq!(algorithm_marker(TextDetectorAlgorithm::Classic), None);
        assert_eq!(
            algorithm_marker(TextDetectorAlgorithm::PaddleOcr),
            Some("ONNX")
        );
        assert_eq!(algorithm_marker(TextDetectorAlgorithm::Ai), Some("Torch"));
        assert_eq!(algorithm_marker(TextDetectorAlgorithm::Surya), Some("Torch"));
    }

    #[test]
    fn every_algorithm_is_listed_once() {
        assert_eq!(DETECTOR_ALGORITHMS.len(), 4);
        for algorithm in [
            TextDetectorAlgorithm::Classic,
            TextDetectorAlgorithm::PaddleOcr,
            TextDetectorAlgorithm::Ai,
            TextDetectorAlgorithm::Surya,
        ] {
            assert_eq!(
                DETECTOR_ALGORITHMS
                    .iter()
                    .filter(|&&a| a == algorithm)
                    .count(),
                1
            );
        }
    }
}
