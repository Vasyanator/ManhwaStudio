/*
FILE HEADER (tabs/typing/panel.rs)
- Назначение: панель вкладки `Текст` в вертикальном формате с набором плавающих панелей
  для режимов `Создание` и `Редактирование` выбранного оверлея.
  Для режима `Создание` отдельное preview остаётся в плавающей панели (drag + collapse).
- Ключевые сущности:
  - `TypingTopPanelState`: общее состояние панели (layout/collapsed/mode, create/edit state,
    биндинг к выделенному оверлею, переключатель панели маски обрезки и очередь
    edit-запросов в `tab.rs`, состояние чекбокса видимости clean-overlay и
    состояние плавающих панелей preview/vertical, а также состояние панели
    `Авто-тайп` (debug + параметры смещения).
    Используются 2 отдельных окна:
    основная панель с вкладками `Параметры` (пресеты + основные параметры)
    и `Эффекты`, а также окно `Действия` (маска/импорт/экспорт);
    `Действия` по умолчанию якорится под preview-панелью.
- `TypingCreatePanelState`: параметры текста/эффектов, загрузка шрифтов, рендер preview
  в фоне (включается только для режима `Создание`), память параметров по каждому шрифту
  и именованные пресеты (содержат snapshot всех шрифтов + главный шрифт), а также
  отдельные пресеты формульной раскладки (`TextTab.formula_presets` в `user_config.json`).
  В базовых параметрах есть сворачиваемый блок `Расширенные параметры`,
  включая направление строки (`Горизонтальная/Вертикальная`) и режим формулы
  раскладки символов (выражения `x/y/rotation`, параметры `t`, константы `a..h`).
  Поле текста — конкурирующий аккордеон `draw_text_accordion`: «Изначальный текст»
  (`text`, ▼ если развёрнут / ◀ если свёрнут) и «Сформированный текст»
  (`formed_text`, ▲ / ◀); развёрнут ровно один. Если `formed_text` пуст —
  развёрнут исходный, иначе сформированный. В рендер идёт `formed_text`, если он
  не пуст (тогда авто-перенос принудительно `None`), иначе `text`
  (`effective_render_text`/`uses_formed_text`; то же в `tab.rs`
  `text_render_params_from_render_data`). Кнопки `Продвинутая форма текста`
  (окно перебора форм по исходному `text`; клик по форме пишет результат в
  `formed_text`, разворачивает сформированный пан и закрывает окно) и
  `Вернуть исходный` (очищает `formed_text` и разворачивает исходный).
  `formed_text` персонален для каждого оверлея: сериализуется в
  `text_params.formed_text` (переживает перезапуск) и
  загружается/сбрасывается в `load_from_selected_overlay`, чтобы не
  «наследоваться» от ранее выбранного оверлея. В окне формы делятся на
  динамические группы по числу переносов слов (кнопки только для встретившихся
  значений + «Все») и дополнительно фильтруются: два диапазона
  (`advanced_form_range_row`, спинбоксы `WheelSpinBox`) — число строк и ширина
  самой длинной строки (в условных единицах метрики) — верхний порог пиковости
  в % (`WheelSlider`, `peakiness_pct` = `(max−base)/base`, база минимум/медиана
  через `PeakBase`) и верхний порог неравномерности в % (`WheelSlider`,
  `unevenness_pct` = среднее |ширина−медиана| / медиана — общий разброс строк,
  устойчивый к одиночным выбросам). Ширина строк
  меряется попиксельно: панель строит `forms::GlyphWidths` выбранным шрифтом
  (cosmic-text, кернинг пар) и передаёт как `LineWidthMetric` в `enumerate_forms`;
  при недоступном шрифте — `CharWidthMetric` (счёт символов). Висящая пунктуация
  оверлея учитывается (при включённой края не идут в ширину). Метрика
  перестраивается при смене текста/шрифта/начертания/висячести
  (`AdvancedFormMetricSignature`). Границы берутся из фактических данных
  (`AdvancedFormCache`) и сбрасываются при пересборке кэша; смена базы пиковости
  раскрывает порог на максимум для новой базы. Сортировка — по ширине
  (узкие → широкие), в пределах допуска по ширине сначала по ровности (меньшая
  неравномерность раньше), затем по цене разрывов, пиковости и числу переносов
  (`sort_advanced_forms`). Само окно стартует
  размером 80%×80% вьюпорта, поднято на `Order::Tooltip` (над панелями
  параметров/действий) и при открытии центрируется по вьюпорту: первый кадр
  скрыт (`set_opacity(0)`), пока не измерен итоговый размер, после чего
  показывается по центру без дёрганья.
  - `TypingSelectedOverlayForEdit` / `TypingOverlayEditRequest`: payload синхронизации
    между `tab.rs` и edit-панелью, включая два типа оверлеев (`text` и `image`).
- Ключевые методы:
  - `TypingTopPanelState::sync_selected_overlay_for_edit`: авто-переключает режим
    панели `Create <-> Edit`, подгружает параметры выделенного оверлея; для текущего
    выделения live-синхронизирует `Масштаб/Угол` с изменениями на canvas
    (ручка вращения, `Ctrl+колесо`, `-`/`=`/`0`).
  - `TypingTopPanelState::take_edit_request`: отдаёт изменения edit-панели для
    live-рендера оверлея в `tab.rs`.
  - `TypingTopPanelState::adjust_selected_text_overlay_font_size_by_wheel_steps`: меняет
    `Размер (px)` у выделенного text-оверлея от внешнего hotkey (`Shift+колесо`) и
    эмитит edit-запрос для немедленного фонового рендера.
  - `TypingTopPanelState::auto_typing_settings`: отдаёт параметры панели `Авто-тайп`
    (debug + смещение центра вниз) для runtime-логики в `tab.rs`.
  - `TypingTopPanelState::draw_create_preview_panel`: рисует отдельную плавающую preview-панель,
    скрывает её в `EditText`, но сохраняет пользовательскую позицию.
  - `TypingTopPanelState::draw_vertical_panel`: рисует основную вкладочную панель
    параметров/эффектов и отдельную панель действий; для image-оверлея вкладка
    эффектов скрывается.
  - wheel-helpers (`cycle_wrapped_index`, scroll helpers): обслуживают
    переключение индексов и прокрутку панелей.
  - чекбокс `Использовать системные шрифты`: общий для `Create/Edit`, состояние
    хранится в `user_config.json` (`TextTab.use_system_fonts`), при пустой папке
    `fonts` автоматически включается и подмешивает системные шрифты в список.
  - `ComboBox` шрифтов (`Шрифт`) отображает каждый пункт с его собственной гарнитурой:
    UI-шрифт lazily регистрируется в `egui` по `(font_path, face_index)` и кэшируется.
  - Дубликаты шрифтов (одно имя файла в корне/разных группах): `merge_duplicate_fonts`
    объединяет байт-идентичные копии (совпадает имя и хэш содержимого) в один пункт
    `FontEntry` с объединением групп (`groups`) и `alt_paths` для сопоставления по
    сохранённому пути; различающиеся по содержимому остаются раздельными, а
    `assign_font_disambiguators` добавляет к имени название группы в скобках. Скобки
    показывает только `font_display_label` при выбранных «Все группы»; при конкретной
    группе имя без скобок.
*/
use crate::config;
use crate::trace::cat;
use crate::tabs::typing::auto_typing::TypingAutoTypingSettings;
use crate::tabs::typing::tab::TypingExportFormat;
use crate::tabs::typing::tab::TypingTextOverlayLayer;
use crate::tabs::typing::render_next::forms::{self, PeakBase, TextForm, TextFormPreset};
use crate::tabs::typing::segmentation::Conservatism;
use crate::tabs::typing::render_next::load_selected_font_from_path;
use crate::tabs::typing::render_next::render_text_to_image;
use crate::tabs::typing::render_next::types::{
    HorizontalAlign, InlineFontEntry, KerningMode, PxOrPercent, RenderedTextImage,
    TEXT_FORMULA_USER_VAR_COUNT, parse_machine_tag,
    TextDrawnLinesLayoutParams, TextFormulaLayoutParams, TextLayoutMode, TextLineMode,
    TextRenderParams, TextShape, TextVectorLine, TextVectorLineDistanceMode,
    TextVectorLineTextDirection, TextVectorLinesLayoutParams, TextVectorPoint, TextWrapMode,
    VerticalLineDirection,
};
use crate::widgets::{
    SeedSpinBox, TextEditPlus, TextEditPlusTextColor, ViewportColorSelector, WheelComboBox,
    WheelSlider, WheelSpinBox, random_seed,
};
use cosmic_text::{Attrs, FontSystem, Metrics, fontdb};
use eframe::egui;
use egui::text::{CCursor, CCursorRange};
use egui::text_selection::visuals::paint_text_selection;
use egui::{Align, Color32, ColorImage, Id, Rect, TextureHandle, TextureOptions, Vec2};
use rfd::FileDialog;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

const CANVAS_LEFT_TOP_CONTROLS_AREA_ID: &str = "canvas_left_top_controls";
const TYPING_VERTICAL_PANEL_AREA_ID: &str = "typing_canvas_vertical_panel";
const TYPING_VERTICAL_ACTIONS_PANEL_AREA_ID: &str = "typing_canvas_vertical_actions_panel";
const TYPING_VERTICAL_PANEL_DEFAULT_WIDTH_PX: f32 = 420.0;
const TYPING_VERTICAL_PANEL_MIN_WIDTH_PX: f32 = 340.0;
const TYPING_VERTICAL_PANEL_MAX_WIDTH_PX: f32 = 560.0;
const TYPING_VERTICAL_ACTIONS_DEFAULT_WIDTH_PX: f32 = 320.0;
const TYPING_VERTICAL_ACTIONS_MIN_WIDTH_PX: f32 = 260.0;
const TYPING_VERTICAL_ACTIONS_MAX_WIDTH_PX: f32 = 420.0;
const TYPING_VERTICAL_PANEL_GAP_PX: f32 = 12.0;
const TYPING_VERTICAL_PANEL_SCROLLBAR_RESERVE_PX: f32 = 24.0;
const TYPING_VERTICAL_PANEL_INITIAL_HEIGHT_RATIO: f32 = 0.8;
const TYPING_VERTICAL_PANEL_DEFAULT_HEIGHT_PX: f32 = 290.0;
const TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX: f32 = 120.0;
const TYPING_PREVIEW_PANEL_AREA_ID: &str = "typing_canvas_preview_panel";
const TYPING_PREVIEW_PANEL_CONTROLS_GAP_PX: f32 = 10.0;
const TYPING_VERTICAL_ACTIONS_PANEL_PREVIEW_GAP_PX: f32 = 18.0;
const TYPING_PREVIEW_PANEL_DEFAULT_WIDTH_PX: f32 = 300.0;
const CREATE_PREVIEW_HEIGHT_PX: f32 = 200.0;
const EDIT_TEXT_FIELD_HEIGHT_PX: f32 = 170.0;

const PREVIEW_TEXTURE_ID: &str = "typing-create-preview-texture";
const DEFAULT_PREVIEW_TEXT: &str = "Текст будет выглядеть так";
const DEFAULT_PREVIEW_WIDTH_PX: u32 = 300;
const TEXT_TAB_USE_SYSTEM_FONTS_KEY: &str = "use_system_fonts";
const TEXT_TAB_USE_LEGACY_INLINE_TAGS_KEY: &str = "use_legacy_inline_tags";
const TEXT_TAB_CREATE_PRESETS_KEY: &str = "create_presets";
const TEXT_TAB_FORMULA_PRESETS_KEY: &str = "formula_presets";
const TEXT_PRESET_NONE_LABEL: &str = "Нет";
const INLINE_TAG_DIM_TEXT_COLOR: Color32 = Color32::from_gray(120);
const INLINE_TAG_CONTENT_TEXT_COLOR: Color32 = Color32::WHITE;

#[derive(Clone)]
struct TypingCreatePreset {
    primary_font_key: String,
    primary_font_path: Option<String>,
    primary_font_label: Option<String>,
    font_profiles: HashMap<String, Value>,
}

#[derive(Clone)]
struct TypingFormulaPreset {
    layout: TextFormulaLayoutParams,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TypingShapeLayoutKind {
    Arc,
    Circle,
    Spiral,
    Polygon,
    Zigzag,
    SCurve,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TypingArcOrientation {
    Horizontal,
    Vertical,
}

impl TypingArcOrientation {
    fn as_config_str(self) -> &'static str {
        match self {
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
        }
    }

    fn from_config_str(value: &str) -> Option<Self> {
        match value {
            "horizontal" => Some(Self::Horizontal),
            "vertical" => Some(Self::Vertical),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Horizontal => "Горизонтальная",
            Self::Vertical => "Вертикальная",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TypingArcShapeLayoutParams {
    length_px: f32,
    amplitude_px: f32,
    frequency: f32,
    orientation: TypingArcOrientation,
}

impl Default for TypingArcShapeLayoutParams {
    fn default() -> Self {
        Self {
            length_px: 320.0,
            amplitude_px: 80.0,
            frequency: 1.0,
            orientation: TypingArcOrientation::Horizontal,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TypingCircleShapeLayoutParams {
    width_px: f32,
    height_px: f32,
}

impl Default for TypingCircleShapeLayoutParams {
    fn default() -> Self {
        Self {
            width_px: 320.0,
            height_px: 220.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TypingSpiralShapeLayoutParams {
    width_px: f32,
    height_px: f32,
    turns: f32,
    inner_ratio: f32,
}

impl Default for TypingSpiralShapeLayoutParams {
    fn default() -> Self {
        Self {
            width_px: 320.0,
            height_px: 240.0,
            turns: 2.5,
            inner_ratio: 0.2,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TypingPolygonShapeLayoutParams {
    width_px: f32,
    height_px: f32,
    sides: u32,
}

impl Default for TypingPolygonShapeLayoutParams {
    fn default() -> Self {
        Self {
            width_px: 320.0,
            height_px: 220.0,
            sides: 6,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TypingZigzagShapeLayoutParams {
    width_px: f32,
    height_px: f32,
    segments: f32,
}

impl Default for TypingZigzagShapeLayoutParams {
    fn default() -> Self {
        Self {
            width_px: 320.0,
            height_px: 90.0,
            segments: 3.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TypingSCurveShapeLayoutParams {
    width_px: f32,
    height_px: f32,
    bends: f32,
}

impl Default for TypingSCurveShapeLayoutParams {
    fn default() -> Self {
        Self {
            width_px: 320.0,
            height_px: 120.0,
            bends: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TypingPanelLayout {
    Vertical,
}

impl TypingPanelLayout {
    pub fn as_config_str(self) -> &'static str {
        "vertical"
    }

    pub fn from_config_str(value: &str) -> Option<Self> {
        match value {
            "vertical" => Some(Self::Vertical),
            "horizontal" => Some(Self::Vertical),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TypingTopPanelMode {
    CreateText,
    EditText,
}

pub struct TypingTopPanelState {
    use_system_fonts: bool,
    collapsed: bool,
    mode: TypingTopPanelMode,
    vertical_panel: TypingFloatingPanelState,
    vertical_actions_panel: TypingFloatingPanelState,
    /// Active tab of the combined Actions/Layers panel (default «Действия»).
    actions_panel_tab: TypingActionsPanelTab,
    vertical_panel_tab: TypingVerticalMainTab,
    vertical_panel_params_content_height_px: f32,
    vertical_panel_effects_content_height_px: f32,
    vertical_panel_resize_revision: u64,
    vertical_panel_last_tab: TypingVerticalMainTab,
    vertical_panel_last_auto_target_height_px: f32,
    last_canvas_height_px: f32,
    create_preview_panel: TypingFloatingPreviewPanelState,
    create_panel: TypingCreatePanelState,
    edit_panel: TypingCreatePanelState,
    edit_overlay_idx: Option<usize>,
    /// What the edit panel currently targets (overlay or raster). Drives request routing.
    edit_target: Option<TypingEditTarget>,
    edit_overlay_kind: Option<TypingOverlayKind>,
    edit_render_data_snapshot: Option<Value>,
    mask_panel_open: bool,
    clean_overlays_visible: bool,
    clean_overlays_initialized: bool,
    pending_clean_overlays_visible: Option<bool>,
    pending_export_to_folder: Option<PathBuf>,
    export_format: TypingExportFormat,
    pending_round_text_positions: bool,
    export_default_dir: Option<PathBuf>,
    export_status: TypingExportUiStatus,
    pending_edit_request: Option<TypingOverlayEditRequest>,
    pending_create_image_request: Option<TypingCreateImageRequest>,
    auto_typing_panel_open: bool,
    auto_typing_debug_visuals: bool,
    auto_typing_extra_downward_shift_percent: f32,
    strict_pixel_movement: bool,
}

#[derive(Clone, Default)]
pub(super) enum TypingExportUiStatus {
    #[default]
    Hidden,
    Running {
        done: usize,
        total: usize,
    },
    Success {
        done: usize,
        total: usize,
    },
    Error {
        message: String,
    },
}

#[derive(Clone)]
pub(super) struct TypingEditorFontSpec {
    pub font_path: PathBuf,
    pub face_index: usize,
    pub ui_font_size_px: f32,
}

#[derive(Clone)]
pub(super) struct TypingSelectedOverlayForEdit {
    pub overlay_idx: usize,
    pub overlay_kind: TypingOverlayKind,
    pub render_data_json: Option<Value>,
    pub width_px_hint: u32,
    pub user_scale: f32,
    pub rotation_deg: f32,
    /// What the edit panel is targeting — a typing overlay or a raster layer. Rasters use the same
    /// `Image` UI (transform + effects, no text params).
    pub target: TypingEditTarget,
}

/// The thing the edit panel currently edits: a typing overlay (by index) or a raster layer (by
/// page + stable uid).
#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) enum TypingEditTarget {
    Overlay(usize),
    Raster { page_idx: usize, uid: String },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum TypingOverlayKind {
    Text,
    Image,
}

pub(super) enum TypingOverlayEditRequest {
    Text {
        overlay_idx: usize,
        render_params: Box<TextRenderParams>,
        render_data_json: Value,
        user_scale: f32,
        rotation_deg: f32,
    },
    ImageTransform {
        target: TypingEditTarget,
        user_scale: f32,
        rotation_deg: f32,
    },
    ImageEffects {
        target: TypingEditTarget,
        render_data_json: Value,
        user_scale: f32,
        rotation_deg: f32,
    },
}

pub(super) enum TypingCreateImageRequest {
    FromClipboard,
    FromFile(PathBuf),
}

impl Default for TypingTopPanelState {
    fn default() -> Self {
        let use_system_fonts = load_text_tab_use_system_fonts();
        let create_panel = TypingCreatePanelState::new(true, use_system_fonts);
        let edit_panel = TypingCreatePanelState::new(false, use_system_fonts);
        let effective_use_system_fonts =
            create_panel.use_system_fonts() || edit_panel.use_system_fonts();
        Self {
            use_system_fonts: effective_use_system_fonts,
            collapsed: false,
            mode: TypingTopPanelMode::CreateText,
            vertical_panel: TypingFloatingPanelState::default(),
            vertical_actions_panel: TypingFloatingPanelState::default(),
            actions_panel_tab: TypingActionsPanelTab::Actions,
            vertical_panel_tab: TypingVerticalMainTab::Parameters,
            vertical_panel_params_content_height_px: 0.0,
            vertical_panel_effects_content_height_px: 0.0,
            vertical_panel_resize_revision: 0,
            vertical_panel_last_tab: TypingVerticalMainTab::Parameters,
            vertical_panel_last_auto_target_height_px: 0.0,
            last_canvas_height_px: 0.0,
            create_preview_panel: TypingFloatingPreviewPanelState::default(),
            create_panel,
            edit_panel,
            edit_overlay_idx: None,
            edit_target: None,
            edit_overlay_kind: None,
            edit_render_data_snapshot: None,
            mask_panel_open: false,
            clean_overlays_visible: true,
            clean_overlays_initialized: false,
            pending_clean_overlays_visible: None,
            pending_export_to_folder: None,
            export_format: TypingExportFormat::default(),
            pending_round_text_positions: false,
            export_default_dir: None,
            export_status: TypingExportUiStatus::Hidden,
            pending_edit_request: None,
            pending_create_image_request: None,
            auto_typing_panel_open: false,
            auto_typing_debug_visuals: false,
            auto_typing_extra_downward_shift_percent: 0.0,
            strict_pixel_movement: true,
        }
    }
}

impl TypingTopPanelState {
    pub(super) fn draw(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        text_overlays: &mut TypingTextOverlayLayer,
        page_idx: usize,
    ) {
        self.create_panel.poll_font_reload_results();
        self.edit_panel.poll_font_reload_results();
        self.create_panel.reset_text_input_focus_tracking();
        self.edit_panel.reset_text_input_focus_tracking();
        if self.create_panel.fonts_reload_in_flight() || self.edit_panel.fonts_reload_in_flight() {
            ctx.request_repaint();
        }
        if let Some(use_system_fonts) = self
            .create_panel
            .take_use_system_fonts_toggle_request()
            .or_else(|| self.edit_panel.take_use_system_fonts_toggle_request())
        {
            self.apply_use_system_fonts(use_system_fonts, true);
        }
        // Синхронизация выбранной группы шрифтов между панелями создания и
        // редактирования: запрос с любой панели применяется к обеим.
        if let Some(group) = self
            .create_panel
            .take_font_group_request()
            .or_else(|| self.edit_panel.take_font_group_request())
        {
            self.create_panel.set_font_group(group.clone());
            self.edit_panel.set_font_group(group);
        }
        if self.mode == TypingTopPanelMode::CreateText {
            self.create_panel.poll_preview_render_results(ctx);
            self.create_panel.ensure_initial_preview_request();
            if self.create_panel.render_in_flight {
                ctx.request_repaint();
            }
        }

        self.draw_vertical_panel(ctx, canvas_rect, text_overlays, page_idx);
    }

    fn apply_use_system_fonts(&mut self, use_system_fonts: bool, persist: bool) {
        if self.use_system_fonts == use_system_fonts
            && self.create_panel.use_system_fonts() == use_system_fonts
            && self.edit_panel.use_system_fonts() == use_system_fonts
        {
            return;
        }
        self.use_system_fonts = use_system_fonts;
        self.create_panel.set_use_system_fonts(use_system_fonts);
        self.edit_panel.set_use_system_fonts(use_system_fonts);
        if persist {
            let _ = thread::Builder::new()
                .name("typing-save-use-system-fonts".to_string())
                .spawn(move || {
                    let _ = save_text_tab_use_system_fonts(use_system_fonts);
                });
        }
    }

    pub(super) fn set_panel_layout(&mut self, layout: TypingPanelLayout) {
        let _ = layout;
    }

    pub(super) fn has_focused_text_input(&self, ctx: &egui::Context) -> bool {
        self.create_panel.has_focused_text_input(ctx) || self.edit_panel.has_focused_text_input(ctx)
    }

    pub(super) fn eyedropper_active(&self) -> bool {
        self.create_panel.eyedropper_active() || self.edit_panel.eyedropper_active()
    }

    pub(super) fn eyedropper_consumed_primary_click_this_frame(&self) -> bool {
        self.create_panel
            .eyedropper_consumed_primary_click_this_frame()
            || self
                .edit_panel
                .eyedropper_consumed_primary_click_this_frame()
    }

    pub(super) fn auto_typing_settings(&self) -> TypingAutoTypingSettings {
        TypingAutoTypingSettings {
            debug_visuals: self.auto_typing_debug_visuals,
            extra_downward_shift_percent: self.auto_typing_extra_downward_shift_percent,
        }
    }

    fn draw_vertical_panel(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        text_overlays: &mut TypingTextOverlayLayer,
        page_idx: usize,
    ) {
        // Для image-оверлея вкладка «Параметры» показывает только трансформацию, но вкладка
        // «Эффекты» доступна так же, как для текста — эффекты применяются к сторонней картинке.
        let image_edit_only = self.mode == TypingTopPanelMode::EditText
            && self.edit_overlay_kind == Some(TypingOverlayKind::Image);
        if self.vertical_panel_tab != self.vertical_panel_last_tab {
            self.vertical_panel_resize_revision =
                self.vertical_panel_resize_revision.wrapping_add(1);
            self.vertical_panel_last_tab = self.vertical_panel_tab;
            ctx.request_repaint();
        }
        if self.last_canvas_height_px > 0.0
            && (canvas_rect.height() - self.last_canvas_height_px).abs() >= 1.0
        {
            self.vertical_panel_resize_revision =
                self.vertical_panel_resize_revision.wrapping_add(1);
            ctx.request_repaint();
        }
        self.last_canvas_height_px = canvas_rect.height();
        let panel_w = TYPING_VERTICAL_PANEL_DEFAULT_WIDTH_PX
            .clamp(
                TYPING_VERTICAL_PANEL_MIN_WIDTH_PX,
                TYPING_VERTICAL_PANEL_MAX_WIDTH_PX,
            )
            .min((canvas_rect.width() - TYPING_VERTICAL_PANEL_GAP_PX * 2.0).max(220.0));
        let actions_panel_w = TYPING_VERTICAL_ACTIONS_DEFAULT_WIDTH_PX
            .clamp(
                TYPING_VERTICAL_ACTIONS_MIN_WIDTH_PX,
                TYPING_VERTICAL_ACTIONS_MAX_WIDTH_PX,
            )
            .min((canvas_rect.width() - TYPING_VERTICAL_PANEL_GAP_PX * 2.0).max(220.0));
        let viewport_rect = ctx.content_rect();
        let min_x = viewport_rect.left();
        let right_limit = viewport_rect.right() - TYPING_VERTICAL_PANEL_SCROLLBAR_RESERVE_PX;
        let max_x = (right_limit - panel_w).max(min_x);
        let actions_min_x = canvas_rect.left();
        let actions_max_x = (canvas_rect.right() - actions_panel_w).max(actions_min_x);
        let min_y = canvas_rect.top();
        let max_y = (canvas_rect.bottom() - 48.0).max(min_y);
        let default_panel_top = canvas_rect.top() + TYPING_VERTICAL_PANEL_GAP_PX;
        let default_pos = egui::pos2(
            (right_limit - panel_w - TYPING_VERTICAL_PANEL_GAP_PX).max(min_x),
            default_panel_top,
        );
        let panel_pos = self
            .vertical_panel
            .pos
            .filter(|_| self.vertical_panel.user_positioned)
            .unwrap_or(default_pos)
            .clamp(egui::pos2(min_x, min_y), egui::pos2(max_x, max_y));
        let viewport_target_height =
            (canvas_rect.height() * TYPING_VERTICAL_PANEL_INITIAL_HEIGHT_RATIO).clamp(
                TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX,
                (canvas_rect.height() - TYPING_VERTICAL_PANEL_GAP_PX * 2.0)
                    .max(TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX),
            );
        let available_panel_height = (canvas_rect.height() - TYPING_VERTICAL_PANEL_GAP_PX * 2.0)
            .max(TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX);
        let current_content_height = match self.vertical_panel_tab {
            TypingVerticalMainTab::Parameters => self.vertical_panel_params_content_height_px,
            TypingVerticalMainTab::Effects => self.vertical_panel_effects_content_height_px,
        };
        let panel_default_height = if current_content_height > 0.0 {
            current_content_height
                .min(viewport_target_height)
                .max(TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX)
        } else {
            viewport_target_height.max(TYPING_VERTICAL_PANEL_DEFAULT_HEIGHT_PX)
        };
        let panel_max_height = if current_content_height > 0.0 {
            current_content_height
                .min(available_panel_height)
                .max(TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX)
        } else {
            available_panel_height
        };
        let auto_target_height = compute_typing_vertical_panel_auto_height(
            current_content_height,
            viewport_target_height,
            available_panel_height,
        );
        if self.vertical_panel_last_auto_target_height_px > 0.0
            && (auto_target_height - self.vertical_panel_last_auto_target_height_px).abs() >= 1.0
        {
            self.vertical_panel_resize_revision =
                self.vertical_panel_resize_revision.wrapping_add(1);
            ctx.request_repaint();
        }
        self.vertical_panel_last_auto_target_height_px = auto_target_height;

        let mut changed = false;
        let params_area_response = egui::Area::new(TYPING_VERTICAL_PANEL_AREA_ID.into())
            .order(egui::Order::Foreground)
            .movable(true)
            .interactable(true)
            .current_pos(panel_pos)
            .show(ctx, |ui| {
                ui.set_width(panel_w);
                ui.set_min_width(panel_w);
                ui.set_max_width(panel_w);
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(panel_w);
                    ui.set_min_width(panel_w);
                    ui.set_max_width(panel_w);
                    ui.horizontal(|ui| {
                        let toggle_icon = if self.collapsed { "▶" } else { "▼" };
                        let toggle_hint = if self.collapsed {
                            "Развернуть панель текста"
                        } else {
                            "Свернуть панель текста"
                        };
                        if ui
                            .small_button(toggle_icon)
                            .on_hover_text(toggle_hint)
                            .clicked()
                        {
                            self.collapsed = !self.collapsed;
                        }
                        ui.selectable_value(
                            &mut self.vertical_panel_tab,
                            TypingVerticalMainTab::Parameters,
                            TypingVerticalMainTab::Parameters.label(),
                        );
                        ui.selectable_value(
                            &mut self.vertical_panel_tab,
                            TypingVerticalMainTab::Effects,
                            TypingVerticalMainTab::Effects.label(),
                        );
                    });
                    if self.collapsed {
                        return;
                    }

                    ui.add_space(4.0);
                    egui::Resize::default()
                        .id_salt((
                            "typing_vertical_main_resize",
                            self.vertical_panel_resize_revision,
                        ))
                        .resizable([false, true])
                        .default_size(egui::vec2(ui.available_width(), panel_default_height))
                        .min_size(egui::vec2(0.0, TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX))
                        .max_size(egui::vec2(ui.available_width(), panel_max_height))
                        .show(ui, |ui| {
                            let mut content_height_px = 0.0;
                            egui::ScrollArea::vertical()
                                .id_salt("typing_vertical_main_vscroll")
                                .show(ui, |ui| match self.vertical_panel_tab {
                                    TypingVerticalMainTab::Parameters => {
                                        if self.mode == TypingTopPanelMode::CreateText {
                                            self.create_panel.draw_create_presets_section(ui);
                                            ui.add_space(6.0);
                                        }
                                        let params_title = if image_edit_only {
                                            "Параметры картинки"
                                        } else {
                                            "Основные параметры текста"
                                        };
                                        ui.label(egui::RichText::new(params_title).strong());
                                        ui.scope(|ui| {
                                            ui.style_mut().always_scroll_the_only_direction = true;
                                            egui::ScrollArea::horizontal()
                                                .id_salt("typing_vertical_params_hscroll")
                                                .scroll_source(egui::scroll_area::ScrollSource {
                                                    scroll_bar: true,
                                                    drag: true,
                                                    mouse_wheel: false,
                                                })
                                                .auto_shrink([false, true])
                                                .show(ui, |ui| match self.mode {
                                                    TypingTopPanelMode::CreateText => {
                                                        self.create_panel.clamp_face_index();
                                                        self.create_panel
                                                            .draw_params_section(ui, true, false);
                                                    }
                                                    TypingTopPanelMode::EditText => {
                                                        if image_edit_only {
                                                            changed |= self
                                                                .edit_panel
                                                                .draw_image_transform_only_section(
                                                                    ui, false,
                                                                );
                                                        } else {
                                                            changed |= self
                                                                .edit_panel
                                                                .draw_edit_params_section(
                                                                    ui, true, false,
                                                                );
                                                        }
                                                    }
                                                });
                                        });
                                        content_height_px = ui.min_rect().height();
                                    }
                                    TypingVerticalMainTab::Effects => {
                                        changed |= match self.mode {
                                            TypingTopPanelMode::CreateText => {
                                                self.create_panel.draw_effects_section(ui, true)
                                            }
                                            TypingTopPanelMode::EditText => {
                                                // Эффекты тоже вызывают перерендер:
                                                // при ненайденном шрифте блокируем их
                                                // вместе с остальными параметрами.
                                                let font_missing =
                                                    self.edit_panel.missing_font.is_some();
                                                ui.add_enabled_ui(!font_missing, |ui| {
                                                    self.edit_panel.draw_effects_section(ui, true)
                                                })
                                                .inner
                                            }
                                        };
                                        content_height_px = ui.min_rect().height();
                                    }
                                });
                            match self.vertical_panel_tab {
                                TypingVerticalMainTab::Parameters => {
                                    self.vertical_panel_params_content_height_px =
                                        content_height_px;
                                }
                                TypingVerticalMainTab::Effects => {
                                    self.vertical_panel_effects_content_height_px =
                                        content_height_px;
                                }
                            }
                            let measured_auto_target_height =
                                compute_typing_vertical_panel_auto_height(
                                    content_height_px,
                                    viewport_target_height,
                                    available_panel_height,
                                );
                            if (measured_auto_target_height
                                - self.vertical_panel_last_auto_target_height_px)
                                .abs()
                                >= 1.0
                            {
                                self.vertical_panel_last_auto_target_height_px =
                                    measured_auto_target_height;
                                self.vertical_panel_resize_revision =
                                    self.vertical_panel_resize_revision.wrapping_add(1);
                                ctx.request_repaint();
                            }
                            if content_height_px > 0.0 && content_height_px < panel_max_height {
                                ctx.request_repaint();
                            }
                        });
                });
            });
        if params_area_response.response.dragged() {
            self.vertical_panel.user_positioned = true;
        }
        if self.vertical_panel.user_positioned {
            self.vertical_panel.pos = Some(params_area_response.response.rect.min);
        }

        let params_rect = params_area_response.response.rect;
        let preview_rect =
            self.draw_create_preview_panel(ctx, canvas_rect, panel_pos.x, panel_pos.y, panel_w);
        let actions_default_anchor = preview_rect.unwrap_or(params_rect);
        let actions_default_pos = egui::pos2(
            actions_default_anchor.min.x,
            actions_default_anchor.max.y + TYPING_VERTICAL_ACTIONS_PANEL_PREVIEW_GAP_PX,
        );
        let actions_pos = self
            .vertical_actions_panel
            .pos
            .unwrap_or(actions_default_pos)
            .clamp(
                egui::pos2(actions_min_x, min_y),
                egui::pos2(actions_max_x, max_y),
            );
        // On the «Слои» tab the layer list's inner width-resize (persisted `layers_panel_width`) must be
        // able to widen the panel, so let the Frame grow to at least that width; the «Действия» tab keeps
        // the fixed actions width. (Both tabs share the resulting width.)
        let panel_w_for_tab = if self.actions_panel_tab == TypingActionsPanelTab::Layers
            && !self.vertical_actions_panel.collapsed
        {
            actions_panel_w.max(text_overlays.layers_panel_width())
        } else {
            actions_panel_w
        };
        let actions_area_response = egui::Area::new(TYPING_VERTICAL_ACTIONS_PANEL_AREA_ID.into())
            .order(egui::Order::Foreground)
            .movable(true)
            .interactable(true)
            .current_pos(actions_pos)
            .show(ctx, |ui| {
                ui.set_width(panel_w_for_tab);
                ui.set_min_width(panel_w_for_tab);
                ui.set_max_width(panel_w_for_tab);
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(panel_w_for_tab);
                    ui.set_min_width(panel_w_for_tab);
                    ui.set_max_width(panel_w_for_tab);
                    // 2-tab header (mirrors the Параметры/Эффекты panel): collapse toggle + «Действия» /
                    // «Слои» tabs.
                    ui.horizontal(|ui| {
                        let toggle_icon = if self.vertical_actions_panel.collapsed {
                            "▶"
                        } else {
                            "▼"
                        };
                        let toggle_hint = if self.vertical_actions_panel.collapsed {
                            "Развернуть панель"
                        } else {
                            "Свернуть панель"
                        };
                        if ui
                            .small_button(toggle_icon)
                            .on_hover_text(toggle_hint)
                            .clicked()
                        {
                            self.vertical_actions_panel.collapsed =
                                !self.vertical_actions_panel.collapsed;
                        }
                        ui.selectable_value(
                            &mut self.actions_panel_tab,
                            TypingActionsPanelTab::Actions,
                            TypingActionsPanelTab::Actions.label(),
                        );
                        ui.selectable_value(
                            &mut self.actions_panel_tab,
                            TypingActionsPanelTab::Layers,
                            TypingActionsPanelTab::Layers.label(),
                        );
                    });
                    if self.vertical_actions_panel.collapsed {
                        return;
                    }
                    ui.add_space(4.0);
                    match self.actions_panel_tab {
                        TypingActionsPanelTab::Actions => {
                            let actions = match self.mode {
                                TypingTopPanelMode::CreateText => {
                                    self.create_panel.draw_right_section(
                                        ui,
                                        self.mask_panel_open,
                                        self.clean_overlays_visible,
                                        self.strict_pixel_movement,
                                        self.export_default_dir.as_deref(),
                                        &self.export_status,
                                        self.export_format,
                                    )
                                }
                                TypingTopPanelMode::EditText => self.edit_panel.draw_right_section(
                                    ui,
                                    self.mask_panel_open,
                                    self.clean_overlays_visible,
                                    self.strict_pixel_movement,
                                    self.export_default_dir.as_deref(),
                                    &self.export_status,
                                    self.export_format,
                                ),
                            };
                            if actions.toggle_mask {
                                self.mask_panel_open = !self.mask_panel_open;
                            }
                            if let Some(visible) = actions.changed_clean_overlays {
                                self.clean_overlays_visible = visible;
                                self.pending_clean_overlays_visible = Some(visible);
                            }
                            if let Some(format) = actions.changed_export_format {
                                self.export_format = format;
                            }
                            if let Some(path) = actions.export_to_folder {
                                self.pending_export_to_folder = Some(path);
                            }
                            if actions.round_text_positions {
                                self.pending_round_text_positions = true;
                            }
                            if actions.create_image_request.is_some() {
                                self.pending_create_image_request = actions.create_image_request;
                            }
                            if let Some(strict_pixel_movement) =
                                actions.changed_strict_pixel_movement
                            {
                                self.strict_pixel_movement = strict_pixel_movement;
                            }
                            self.draw_auto_typing_controls(ui);
                        }
                        TypingActionsPanelTab::Layers => {
                            text_overlays.draw_layers_tab_body(ui, page_idx);
                        }
                    }
                });
            });
        self.vertical_actions_panel.pos = Some(actions_area_response.response.rect.min);

        if self.mode == TypingTopPanelMode::EditText && changed {
            self.emit_edit_request();
        }
    }

    pub(super) fn build_create_text_render_bundle(
        &self,
        text: String,
        width_px: u32,
    ) -> Result<(TextRenderParams, Value), String> {
        let render_params = self
            .create_panel
            .build_render_params_for(text.clone(), width_px.max(1))
            .ok_or_else(|| {
                format!(
                    "Шрифты не найдены в {}",
                    self.create_panel.fonts_dir.display()
                )
            })?;
        let render_data_json = self
            .create_panel
            .build_render_data_json_for(text, width_px.max(1))
            .ok_or_else(|| {
                format!(
                    "Шрифты не найдены в {}",
                    self.create_panel.fonts_dir.display()
                )
            })?;
        Ok((render_params, render_data_json))
    }

    pub(super) fn create_editor_font_spec(&self) -> Option<TypingEditorFontSpec> {
        self.create_panel.editor_font_spec()
    }

    pub(super) fn adjust_create_font_size_by_wheel_steps(&mut self, steps: i32) -> bool {
        if self.mode != TypingTopPanelMode::CreateText {
            return false;
        }
        self.create_panel.adjust_font_size_by_wheel_steps(steps)
    }

    pub(super) fn adjust_selected_text_overlay_font_size_by_wheel_steps(
        &mut self,
        steps: i32,
    ) -> bool {
        if self.mode != TypingTopPanelMode::EditText {
            return false;
        }
        if self.edit_overlay_kind != Some(TypingOverlayKind::Text) {
            return false;
        }
        if !self.edit_panel.adjust_font_size_by_wheel_steps(steps) {
            return false;
        }
        self.emit_edit_request();
        true
    }

    pub(super) fn sync_selected_overlay_for_edit(
        &mut self,
        selected: Option<TypingSelectedOverlayForEdit>,
    ) {
        match selected {
            Some(selected) => {
                let render_data_changed =
                    self.edit_render_data_snapshot != selected.render_data_json;
                let target_changed = self.edit_target.as_ref() != Some(&selected.target);
                if target_changed || render_data_changed {
                    match selected.overlay_kind {
                        TypingOverlayKind::Text => {
                            self.edit_panel.load_from_selected_overlay(&selected);
                        }
                        TypingOverlayKind::Image => {
                            self.edit_panel
                                .sync_overlay_transform_from_selected_overlay(&selected);
                            if let Some(render_data) = selected.render_data_json.as_ref() {
                                self.edit_panel.load_effects_only_from_render_data(render_data);
                            }
                        }
                    }
                    self.pending_edit_request = None;
                } else {
                    self.edit_panel
                        .sync_overlay_transform_from_selected_overlay(&selected);
                }
                self.edit_overlay_idx = Some(selected.overlay_idx);
                self.edit_target = Some(selected.target.clone());
                self.edit_overlay_kind = Some(selected.overlay_kind);
                self.edit_render_data_snapshot = selected.render_data_json.clone();
                self.mode = TypingTopPanelMode::EditText;
            }
            None => {
                self.edit_overlay_idx = None;
                self.edit_target = None;
                self.edit_overlay_kind = None;
                self.edit_render_data_snapshot = None;
                self.pending_edit_request = None;
                self.mode = TypingTopPanelMode::CreateText;
            }
        }
    }

    pub(super) fn take_edit_request(&mut self) -> Option<TypingOverlayEditRequest> {
        self.pending_edit_request.take()
    }

    pub(super) fn is_mask_panel_open(&self) -> bool {
        self.mask_panel_open
    }

    pub(super) fn strict_pixel_movement(&self) -> bool {
        self.strict_pixel_movement
    }

    pub(super) fn sync_clean_overlays_visible_from_canvas(&mut self, visible: bool) {
        if self.clean_overlays_initialized {
            return;
        }
        self.clean_overlays_visible = visible;
        self.clean_overlays_initialized = true;
    }

    pub(super) fn take_clean_overlays_visible_request(&mut self) -> Option<bool> {
        self.pending_clean_overlays_visible.take()
    }

    pub(super) fn take_export_to_folder_request(&mut self) -> Option<(PathBuf, TypingExportFormat)> {
        self.pending_export_to_folder
            .take()
            .map(|path| (path, self.export_format))
    }

    pub(super) fn take_round_text_positions_request(&mut self) -> bool {
        std::mem::take(&mut self.pending_round_text_positions)
    }

    pub(super) fn take_create_image_request(&mut self) -> Option<TypingCreateImageRequest> {
        self.pending_create_image_request.take()
    }

    pub(super) fn set_export_default_dir(&mut self, path: PathBuf) {
        self.export_default_dir = Some(path);
    }

    pub(super) fn sync_export_status(&mut self, status: TypingExportUiStatus) {
        self.export_status = status;
    }

    fn emit_edit_request(&mut self) {
        let Some(target) = self.edit_target.clone() else {
            return;
        };
        let overlay_kind = self.edit_overlay_kind.unwrap_or(TypingOverlayKind::Text);
        self.pending_edit_request = match overlay_kind {
            TypingOverlayKind::Text => {
                // Text editing only applies to overlays.
                let TypingEditTarget::Overlay(overlay_idx) = target else {
                    return;
                };
                // Шрифт оверлея не найден: рендер заблокирован, пока пользователь не
                // выберет другой доступный шрифт. Иначе текст отрисовался бы чужим
                // (подставленным) шрифтом.
                if self.edit_panel.missing_font.is_some() {
                    return;
                }
                let Some(render_params) = self.edit_panel.build_render_params() else {
                    return;
                };
                let Some(render_data_json) = self.edit_panel.build_render_data_json_for(
                    self.edit_panel.text.clone(),
                    self.edit_panel.width_px.max(1),
                ) else {
                    return;
                };
                Some(TypingOverlayEditRequest::Text {
                    overlay_idx,
                    render_params: Box::new(render_params),
                    render_data_json,
                    user_scale: self.edit_panel.overlay_scale.clamp(0.05, 20.0),
                    rotation_deg: normalize_angle_deg(self.edit_panel.overlay_rotation_deg),
                })
            }
            TypingOverlayKind::Image => {
                let user_scale = self.edit_panel.overlay_scale.clamp(0.05, 20.0);
                let rotation_deg = normalize_angle_deg(self.edit_panel.overlay_rotation_deg);
                // Изменения во вкладке «Эффекты» требуют перерендера картинки; чистая
                // трансформация (масштаб/угол) применяется на показе без перерендера.
                if self.vertical_panel_tab == TypingVerticalMainTab::Effects {
                    Some(TypingOverlayEditRequest::ImageEffects {
                        target,
                        render_data_json: self.edit_panel.build_image_effects_render_data(),
                        user_scale,
                        rotation_deg,
                    })
                } else {
                    Some(TypingOverlayEditRequest::ImageTransform {
                        target,
                        user_scale,
                        rotation_deg,
                    })
                }
            }
        };
    }

    fn draw_create_preview_panel(
        &mut self,
        ctx: &egui::Context,
        canvas_rect: Rect,
        panel_left: f32,
        panel_top: f32,
        panel_width: f32,
    ) -> Option<Rect> {
        if self.mode != TypingTopPanelMode::CreateText {
            return None;
        }

        let min_x = canvas_rect.left();
        let max_x = (canvas_rect.right() - 80.0).max(min_x);
        let min_y = canvas_rect.top();
        let max_y = (canvas_rect.bottom() - 40.0).max(min_y);
        let controls_rect =
            ctx.memory(|mem| mem.area_rect(Id::new(CANVAS_LEFT_TOP_CONTROLS_AREA_ID)));
        let default_pos = controls_rect
            .map(|rect| {
                egui::pos2(
                    rect.left(),
                    rect.bottom() + TYPING_PREVIEW_PANEL_CONTROLS_GAP_PX,
                )
            })
            .unwrap_or(egui::pos2(
                panel_left,
                panel_top + TYPING_PREVIEW_PANEL_CONTROLS_GAP_PX,
            ));
        let panel_pos = self
            .create_preview_panel
            .pos
            .unwrap_or(default_pos)
            .clamp(egui::pos2(min_x, min_y), egui::pos2(max_x, max_y));
        let panel_w = TYPING_PREVIEW_PANEL_DEFAULT_WIDTH_PX.min(panel_width.max(220.0));

        let area_response = egui::Area::new(TYPING_PREVIEW_PANEL_AREA_ID.into())
            .order(egui::Order::Foreground)
            .movable(true)
            .interactable(true)
            .current_pos(panel_pos)
            .show(ctx, |ui| {
                ui.set_width(panel_w);
                ui.set_min_width(panel_w);
                ui.set_max_width(panel_w);
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(panel_w);
                    ui.set_min_width(panel_w);
                    ui.set_max_width(panel_w);
                    ui.horizontal(|ui| {
                        let toggle_icon = if self.create_preview_panel.collapsed {
                            "▶"
                        } else {
                            "▼"
                        };
                        let toggle_hint = if self.create_preview_panel.collapsed {
                            "Развернуть превью текста"
                        } else {
                            "Свернуть превью текста"
                        };
                        if ui
                            .small_button(toggle_icon)
                            .on_hover_text(toggle_hint)
                            .clicked()
                        {
                            self.create_preview_panel.collapsed =
                                !self.create_preview_panel.collapsed;
                        }
                        ui.label("Превью текста");
                    });
                    if self.create_preview_panel.collapsed {
                        return;
                    }
                    ui.add_space(4.0);
                    self.create_panel.draw_preview_section(ui);
                });
            });

        self.create_preview_panel.pos = Some(area_response.response.rect.min);
        Some(area_response.response.rect)
    }

    fn draw_auto_typing_controls(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        let toggle_label = if self.auto_typing_panel_open {
            "Закрыть Авто-тайп"
        } else {
            "Открыть Авто-тайп"
        };
        if ui.button(toggle_label).clicked() {
            self.auto_typing_panel_open = !self.auto_typing_panel_open;
        }

        if !self.auto_typing_panel_open {
            return;
        }

        ui.add_space(4.0);
        ui.group(|ui| {
            ui.label(egui::RichText::new("Авто-тайп").strong());
            ui.label("Hotkey: C (для выделенного текстового оверлея)");
            ui.checkbox(&mut self.auto_typing_debug_visuals, "Показывать отладку");
            ui.add(
                WheelSlider::new(
                    &mut self.auto_typing_extra_downward_shift_percent,
                    -25.0..=50.0,
                )
                .text("Доп. смещение вниз (%)"),
            );
        });
    }
}

#[derive(Default)]
struct TypingFloatingPreviewPanelState {
    collapsed: bool,
    pos: Option<egui::Pos2>,
}

#[derive(Default)]
struct TypingFloatingPanelState {
    collapsed: bool,
    pos: Option<egui::Pos2>,
    user_positioned: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
enum TypingVerticalMainTab {
    #[default]
    Parameters,
    Effects,
}

impl TypingVerticalMainTab {
    fn label(self) -> &'static str {
        match self {
            Self::Parameters => "Параметры",
            Self::Effects => "Эффекты",
        }
    }
}

/// The two tabs of the combined Actions/Layers floating panel.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
enum TypingActionsPanelTab {
    #[default]
    Actions,
    Layers,
}

impl TypingActionsPanelTab {
    fn label(self) -> &'static str {
        match self {
            Self::Actions => "Действия",
            Self::Layers => "Слои",
        }
    }
}

#[derive(Clone)]
struct FontEntry {
    /// Базовое отображаемое имя (имя файла без расширения), без скобок-уточнения.
    label: String,
    /// Представительный файл шрифта.
    path: PathBuf,
    /// Прочие байт-идентичные копии того же шрифта (объединены в один пункт);
    /// нужны для сопоставления по сохранённому пути.
    alt_paths: Vec<PathBuf>,
    /// Группы, в которых встречается шрифт (`None` — корень папки шрифтов).
    /// У объединённой копии — объединение групп всех копий.
    groups: Vec<Option<String>>,
    /// Скобочное уточнение (название группы) для отображения, когда выбрано «Все
    /// группы» и базовое имя неоднозначно. `None` — уточнение не нужно.
    disambig: Option<String>,
    faces: Vec<FontFaceEntry>,
}

#[derive(Clone)]
struct FontFaceEntry {
    label: String,
    face_index: usize,
}

/// Какой текстовый буфер сейчас активен для выделения и вставки инлайн-тегов:
/// исходный `text` или сформированный `formed_text`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InlineTextTarget {
    Source,
    Formed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AvailableEffectKind {
    TextShake,
    Stroke,
    Shadow,
    Blur,
    MotionBlur,
    DryMedia,
    GlowV1,
    GlowV2,
    SoftGlow,
    Gradient2,
    Gradient4,
    Reflect,
    Shake,
}

impl AvailableEffectKind {
    fn label(self) -> &'static str {
        match self {
            Self::TextShake => "Тряска текста",
            Self::Stroke => "Обводка",
            Self::Shadow => "Тень",
            Self::Blur => "Размытие",
            Self::MotionBlur => "Размытие в движении",
            Self::DryMedia => "Мел/Карандаш",
            Self::GlowV1 => "Свечение V1",
            Self::GlowV2 => "Свечение V2",
            Self::SoftGlow => "Мягкое свечение",
            Self::Gradient2 => "Градиент 2",
            Self::Gradient4 => "Градиент 4",
            Self::Reflect => "Отражение",
            Self::Shake => "Тряска",
        }
    }
}

enum EffectCard {
    TextShake(TextShakeEffectCard),
    Stroke(StrokeEffectCard),
    Shadow(ShadowEffectCard),
    Blur(BlurEffectCard),
    MotionBlur(MotionBlurEffectCard),
    DryMedia(DryMediaEffectCard),
    Glow(GlowEffectCard),
    Gradient2(Gradient2EffectCard),
    Gradient4(Gradient4EffectCard),
    Reflect(ReflectEffectCard),
    Shake(ShakeEffectCard),
}

impl EffectCard {
    fn eyedropper_active(&self) -> bool {
        match self {
            Self::TextShake(_) => false,
            Self::Stroke(card) => card.color.eyedropper_active(),
            Self::Shadow(card) => card.color.eyedropper_active(),
            Self::Blur(_) | Self::MotionBlur(_) => false,
            Self::DryMedia(card) => !card.use_source_color && card.color.eyedropper_active(),
            Self::Glow(card) => card.color.eyedropper_active(),
            Self::Gradient2(card) => {
                card.color1.eyedropper_active()
                    || card.color2.eyedropper_active()
                    || card.target_color.eyedropper_active()
            }
            Self::Gradient4(card) => {
                card.color_top_left.eyedropper_active()
                    || card.color_top_right.eyedropper_active()
                    || card.color_bottom_left.eyedropper_active()
                    || card.color_bottom_right.eyedropper_active()
                    || card.target_color.eyedropper_active()
            }
            Self::Reflect(_) | Self::Shake(_) => false,
        }
    }

    fn eyedropper_consumed_primary_click_this_frame(&self) -> bool {
        match self {
            Self::TextShake(_) => false,
            Self::Stroke(card) => card.color.eyedropper_consumed_primary_click_this_frame(),
            Self::Shadow(card) => card.color.eyedropper_consumed_primary_click_this_frame(),
            Self::Blur(_) | Self::MotionBlur(_) => false,
            Self::DryMedia(card) => {
                !card.use_source_color && card.color.eyedropper_consumed_primary_click_this_frame()
            }
            Self::Glow(card) => card.color.eyedropper_consumed_primary_click_this_frame(),
            Self::Gradient2(card) => {
                card.color1.eyedropper_consumed_primary_click_this_frame()
                    || card.color2.eyedropper_consumed_primary_click_this_frame()
                    || card
                        .target_color
                        .eyedropper_consumed_primary_click_this_frame()
            }
            Self::Gradient4(card) => {
                card.color_top_left
                    .eyedropper_consumed_primary_click_this_frame()
                    || card
                        .color_top_right
                        .eyedropper_consumed_primary_click_this_frame()
                    || card
                        .color_bottom_left
                        .eyedropper_consumed_primary_click_this_frame()
                    || card
                        .color_bottom_right
                        .eyedropper_consumed_primary_click_this_frame()
                    || card
                        .target_color
                        .eyedropper_consumed_primary_click_this_frame()
            }
            Self::Reflect(_) | Self::Shake(_) => false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StrokeOpacityMode {
    Static,
    FromContour,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShadowColorMode {
    SingleColor,
    SourceColors,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GlowEffectVersion {
    V1,
    V2,
    Soft,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Gradient2FillMode {
    AllOpaque,
    SpecificColor,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Gradient4FillMode {
    AllOpaque,
    SpecificColor,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReflectAxis {
    X,
    Y,
}

struct ColorField {
    value: Color32,
    picker: ViewportColorSelector,
}

impl ColorField {
    fn new(value: Color32) -> Self {
        Self {
            value,
            picker: ViewportColorSelector::default(),
        }
    }

    fn rgba(&self) -> [u8; 4] {
        self.value.to_srgba_unmultiplied()
    }

    fn draw(&mut self, ui: &mut egui::Ui, label: &str) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label(label);
            let resp = self.picker.draw(ui, &mut self.value);
            changed |= resp.changed;
        });
        changed
    }

    fn eyedropper_active(&self) -> bool {
        self.picker.eyedropper_active()
    }

    fn eyedropper_consumed_primary_click_this_frame(&self) -> bool {
        self.picker.primary_click_consumed_this_frame()
    }
}

struct TextShakeEffectCard {
    spread_x_px: f32,
    spread_y_px: f32,
    seed: u64,
}

struct StrokeEffectCard {
    width_px: f32,
    color: ColorField,
    opacity_mode: StrokeOpacityMode,
    transparency_percent: f32,
    smoothing: bool,
    smoothing_strength_percent: f32,
}

struct ShadowEffectCard {
    offset_x_px: i32,
    offset_y_px: i32,
    transparency_percent: f32,
    blur_radius_px: f32,
    color_mode: ShadowColorMode,
    color: ColorField,
}

struct BlurEffectCard {
    radius_px: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MotionBlurSharpCopyMode {
    None,
    Over,
    Under,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DryMediaMaterial {
    Pencil,
    Chalk,
}

struct MotionBlurEffectCard {
    angle_deg: f32,
    distance_px: f32,
    sharp_copy_mode: MotionBlurSharpCopyMode,
}

struct DryMediaEffectCard {
    material: DryMediaMaterial,
    strength: f32,
    seed: u64,
    grain_scale_px: f32,
    grain_amount: f32,
    edge_roughness: f32,
    porosity: f32,
    direction_deg: f32,
    directional_amount: f32,
    dust_amount: f32,
    dust_radius_px: f32,
    softness_px: f32,
    use_source_color: bool,
    color: ColorField,
}

struct GlowEffectCard {
    version: GlowEffectVersion,
    radius_px: f32,
    softness_px: f32,
    color: ColorField,
    opacity_mode: StrokeOpacityMode,
    transparency_percent: f32,
    fade_strength: f32,
    fade_shift: f32,
}

struct Gradient2EffectCard {
    color1: ColorField,
    color2: ColorField,
    angle_deg: f32,
    width_percent: f32,
    respect_source_alpha: bool,
    fill_mode: Gradient2FillMode,
    target_color: ColorField,
}

struct Gradient4EffectCard {
    color_top_left: ColorField,
    color_top_right: ColorField,
    color_bottom_left: ColorField,
    color_bottom_right: ColorField,
    width_percent: f32,
    respect_source_alpha: bool,
    fill_mode: Gradient4FillMode,
    target_color: ColorField,
}

struct ReflectEffectCard {
    axis: ReflectAxis,
}

struct ShakeEffectCard {
    angle_deg: f32,
    up_px: f32,
    down_px: f32,
    steps: u32,
    base_fade: f32,
    decay: f32,
    blur_px: u32,
    autogrow: bool,
    grow_margin_px: u32,
}

#[derive(Clone)]
struct PreviewRenderJob {
    token: u64,
    params: TextRenderParams,
}

struct PreviewRenderResult {
    token: u64,
    image: Result<RenderedTextImage, String>,
}

struct FontReloadResult {
    token: u64,
    fonts: Vec<FontEntry>,
    font_groups: Vec<String>,
}

struct TypingRightSectionActions {
    toggle_mask: bool,
    changed_clean_overlays: Option<bool>,
    export_to_folder: Option<PathBuf>,
    changed_export_format: Option<TypingExportFormat>,
    round_text_positions: bool,
    create_image_request: Option<TypingCreateImageRequest>,
    changed_strict_pixel_movement: Option<bool>,
}

struct TypingCreatePanelState {
    fonts_dir: PathBuf,
    fonts: Vec<FontEntry>,
    font_groups: Vec<String>,
    selected_font_group: Option<String>,
    use_system_fonts: bool,
    pending_use_system_fonts_toggle_request: Option<bool>,
    /// Запрос смены группы шрифтов для синхронизации между панелями `create`/`edit`.
    /// Внешний `Some` — есть запрос; внутреннее значение — новая `selected_font_group`
    /// (`None` = «Все группы»).
    pending_font_group_request: Option<Option<String>>,
    font_reload_rx: Option<Receiver<FontReloadResult>>,
    latest_font_reload_token: u64,
    fonts_reload_in_flight: bool,
    combo_font_family_cache: HashMap<(PathBuf, usize), String>,
    font_profiles_by_key: HashMap<String, Value>,
    active_font_key: Option<String>,
    /// Имя шрифта выбранного для редактирования оверлея, если этот шрифт не найден
    /// среди доступных. Пока поле `Some`, рендер оверлея заблокирован, а все
    /// параметры (кроме выбора шрифта) на панели редактирования недоступны.
    missing_font: Option<String>,
    presets_by_name: HashMap<String, TypingCreatePreset>,
    selected_preset_name: Option<String>,
    preset_name_input: String,
    formula_presets_by_name: HashMap<String, TypingFormulaPreset>,
    selected_formula_preset_name: Option<String>,
    formula_preset_name_input: String,
    preview_enabled: bool,
    selected_font_idx: usize,
    selected_face_idx: usize,
    text: String,
    text_color: Color32,
    text_color_selector: ViewportColorSelector,
    font_size_px: f32,
    line_spacing: PxOrPercent,
    kerning_mode: KerningMode,
    kerning: PxOrPercent,
    glyph_height: PxOrPercent,
    glyph_width: PxOrPercent,
    width_px: u32,
    align: HorizontalAlign,
    text_line_mode: TextLineMode,
    vertical_line_direction: VerticalLineDirection,
    text_layout_mode: TextLayoutMode,
    formula_layout: TextFormulaLayoutParams,
    drawn_lines_layout: TextDrawnLinesLayoutParams,
    vector_lines_layout: TextVectorLinesLayoutParams,
    shape_layout_kind: TypingShapeLayoutKind,
    arc_shape_layout: TypingArcShapeLayoutParams,
    circle_shape_layout: TypingCircleShapeLayoutParams,
    spiral_shape_layout: TypingSpiralShapeLayoutParams,
    polygon_shape_layout: TypingPolygonShapeLayoutParams,
    zigzag_shape_layout: TypingZigzagShapeLayoutParams,
    s_curve_shape_layout: TypingSCurveShapeLayoutParams,
    formula_help_open: bool,
    text_shape: TextShape,
    text_wrap_mode: TextWrapMode,
    allow_moderate_trees: bool,
    shape_min_width_percent: f32,
    shape_variant: u8,
    force_bold: bool,
    force_italic: bool,
    uppercase_text: bool,
    trim_extra_spaces: bool,
    hanging_punctuation: bool,
    new_line_after_sentence: bool,
    enable_inline_style_tags: bool,
    // Писать обычные («человекочитаемые») inline-теги вместо компактного `<m ...>`.
    // Пока не подключено к UI — будет переключаться в будущей вкладке настроек тайпа.
    use_legacy_inline_tags: bool,
    overlay_scale: f32,
    overlay_rotation_deg: f32,
    effect_to_add: AvailableEffectKind,
    effects: Vec<EffectCard>,
    request_tx: Sender<PreviewRenderJob>,
    result_rx: Receiver<PreviewRenderResult>,
    latest_token: u64,
    render_in_flight: bool,
    needs_initial_preview: bool,
    status_line: String,
    preview_texture: Option<TextureHandle>,
    preview_size: [usize; 2],
    tracked_text_input_ids: Vec<Id>,
    text_selection_char_range: Option<Range<usize>>,
    pending_text_selection_restore: Option<Range<usize>>,
    /// Буфер, к которому относятся выделение и инлайн-теги (исходный/сформированный).
    inline_text_target: InlineTextTarget,
    advanced_form_open: bool,
    advanced_form_preset: TextFormPreset,
    /// Выбранная группа по числу переносов слов; `None` — «Все».
    advanced_form_group: Option<usize>,
    advanced_form_cache: Option<AdvancedFormCache>,
    /// Сформированный (разбитый на строки) текст. Если не пуст — в рендер идёт
    /// именно он, а `text` остаётся исходным. Пуст — рендерится `text`.
    formed_text: String,
    /// Какой из двух текстов развёрнут в панели (конкурирующий аккордеон):
    /// `true` — сформированный, `false` — исходный.
    advanced_text_show_formed: bool,
    /// Фильтр по числу строк `(min, max)`; задаётся границами кэша.
    advanced_form_line_range: (usize, usize),
    /// Фильтр по ширине самой длинной строки `(min, max)`, в единицах метрики.
    advanced_form_width_range: (u32, u32),
    /// Верхний порог пиковости в % (показываем формы не «пиковее» него).
    advanced_form_peak_max: u32,
    /// База отсчёта пиковости (минимум/медиана).
    advanced_form_peak_base: PeakBase,
    /// Верхний порог неравномерности в % (показываем формы не «разбросаннее» него).
    advanced_form_uneven_max: u32,
    /// Верхний порог консервативности: показываем формы, чья консервативность не
    /// выше выбранной (`Safe` — только безопасные переносы, без отрыва предлогов).
    advanced_form_conservatism_max: Conservatism,
    /// Окно уже отцентрировано (узнало итоговый размер). До этого окно скрыто,
    /// чтобы не было дёрганья при позиционировании.
    advanced_form_centered: bool,
}

/// Сколько карточек форм максимум отрисовываем в окне за раз. Это предел
/// ОТРИСОВКИ, а не данных: кэш хранит все удачные формы и фильтрует их целиком,
/// а в список попадают первые `ADVANCED_FORM_DISPLAY_LIMIT` (лучшие по сортировке)
/// из прошедших фильтр.
const ADVANCED_FORM_DISPLAY_LIMIT: usize = 600;

/// Кэш перечисленных форм для окна «Продвинутая форма текста».
struct AdvancedFormCache {
    source_text: String,
    preset: TextFormPreset,
    /// Формы, отсортированные по ширине (узкие → широкие), а в пределах ±1
    /// символа — по накопленной цене разрывов.
    forms: Vec<TextForm>,
    /// Встретившиеся значения числа переносов слов (для динамических кнопок).
    group_counts: Vec<usize>,
    /// Границы фильтров по фактическим данным: число строк, ширина, пиковость %.
    line_bounds: (usize, usize),
    width_bounds: (u32, u32),
    /// Сигнатура шрифта/режима, при которой построена метрика ширины. Смена —
    /// повод пересобрать кэш (ширины меняются).
    metric_signature: AdvancedFormMetricSignature,
    /// Максимальная пиковость в % для каждой базы (минимум/медиана).
    peak_max_bound_min: u32,
    peak_max_bound_median: u32,
    /// Максимальная неравномерность в % среди форм (верхняя граница фильтра).
    uneven_max_bound: u32,
    /// Самая вольная консервативность среди форм (верхняя граница фильтра). Если
    /// `Safe` — отрывов служебных слов нет, селектор консервативности не нужен.
    conservatism_bound: Conservatism,
    /// Перебор форм оказался неполным: выбит бюджет узлов рекурсии (не лимит
    /// отрисовки). Означает, что в кэше лежат не все возможные формы.
    truncated: bool,
}

/// От чего зависят пиксельные ширины глифов в окне форм. При смене любого поля
/// метрику (и кэш форм) надо пересобрать.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct AdvancedFormMetricSignature {
    font_path: Option<String>,
    face_index: usize,
    force_bold: bool,
    force_italic: bool,
    hanging_punctuation: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct TypingInlineTagStyle {
    bold: bool,
    italic: bool,
    font_label: Option<String>,
    font_size_px: Option<f32>,
    text_color: Option<Color32>,
    line_spacing: Option<PxOrPercent>,
    kerning: Option<PxOrPercent>,
    glyph_stretching: Option<[PxOrPercent; 2]>,
    glyph_offset: Option<TypingInlineOffsetStyle>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct TypingInlineOffsetStyle {
    global_x: PxOrPercent,
    global_y: PxOrPercent,
    line: PxOrPercent,
    shift_following: bool,
    group_rotation_deg: f32,
    glyph_rotation_deg: f32,
}

impl TypingInlineOffsetStyle {
    // Свежее смещение по умолчанию задаётся в процентах (как и остальные параметры).
    fn global_only(global: [f32; 2]) -> Self {
        Self {
            global_x: PxOrPercent::percent(global[0]),
            global_y: PxOrPercent::percent(global[1]),
            line: PxOrPercent::percent(0.0),
            shift_following: false,
            group_rotation_deg: 0.0,
            glyph_rotation_deg: 0.0,
        }
    }
}

#[derive(Debug, Clone)]
struct TypingInlineSelectionContext {
    char_range: Range<usize>,
    text_byte_range: Range<usize>,
    opening_wrapper_range: Range<usize>,
    closing_wrapper_range: Range<usize>,
    style: TypingInlineTagStyle,
}

#[derive(Debug, Clone, PartialEq)]
enum TypingInlineTagKind {
    Bold,
    Italic,
    Font(String),
    Size(f32),
    Color(Color32),
    LineSpacing(PxOrPercent),
    Kerning(PxOrPercent),
    Stretching([PxOrPercent; 2]),
    Offset(TypingInlineOffsetStyle),
    /// Машиночитаемый тег `<m ...>`, совмещающий все параметры в одном теге.
    Machine(TypingInlineTagStyle),
}

#[derive(Debug, Clone)]
struct TypingInlineTagToken {
    byte_range: Range<usize>,
    kind: TypingInlineTagKind,
}

impl Default for TypingCreatePanelState {
    fn default() -> Self {
        Self::new(true, load_text_tab_use_system_fonts())
    }
}

impl TypingCreatePanelState {
    fn new(preview_enabled: bool, use_system_fonts: bool) -> Self {
        let fonts_dir = resolve_fonts_dir();
        let local_fonts = load_fonts_from_dir(&fonts_dir);
        let font_groups = load_font_groups(&fonts_dir);
        let auto_enable_system_fonts = local_fonts.is_empty();
        let effective_use_system_fonts = use_system_fonts || auto_enable_system_fonts;
        let fonts = local_fonts;
        let presets_by_name = if preview_enabled {
            load_text_tab_create_presets()
        } else {
            HashMap::new()
        };
        let formula_presets_by_name = load_text_tab_formula_presets();
        let (request_tx, result_rx) = spawn_preview_render_worker();
        let status_line = if auto_enable_system_fonts {
            format!(
                "Локальные шрифты не найдены в {}, загружаю системные",
                fonts_dir.display()
            )
        } else if fonts.is_empty() {
            format!("Не найдено шрифтов в {}", fonts_dir.display())
        } else {
            "Готово к рендеру".to_string()
        };
        let mut state = Self {
            fonts_dir,
            fonts,
            font_groups,
            selected_font_group: None,
            use_system_fonts: effective_use_system_fonts,
            pending_use_system_fonts_toggle_request: None,
            pending_font_group_request: None,
            font_reload_rx: None,
            latest_font_reload_token: 0,
            fonts_reload_in_flight: false,
            combo_font_family_cache: HashMap::new(),
            font_profiles_by_key: HashMap::new(),
            active_font_key: None,
            missing_font: None,
            presets_by_name,
            selected_preset_name: None,
            preset_name_input: String::new(),
            formula_presets_by_name,
            selected_formula_preset_name: None,
            formula_preset_name_input: String::new(),
            preview_enabled,
            selected_font_idx: 0,
            selected_face_idx: 0,
            text: DEFAULT_PREVIEW_TEXT.to_string(),
            text_color: Color32::BLACK,
            text_color_selector: ViewportColorSelector::default(),
            font_size_px: 24.0,
            line_spacing: PxOrPercent::percent(0.0),
            kerning_mode: KerningMode::Metric,
            kerning: PxOrPercent::percent(0.0),
            glyph_height: PxOrPercent::percent(100.0),
            glyph_width: PxOrPercent::percent(100.0),
            width_px: DEFAULT_PREVIEW_WIDTH_PX,
            align: HorizontalAlign::CENTER,
            text_line_mode: TextLineMode::Horizontal,
            vertical_line_direction: VerticalLineDirection::RightToLeft,
            text_layout_mode: TextLayoutMode::Normal,
            formula_layout: TextFormulaLayoutParams::default(),
            drawn_lines_layout: TextDrawnLinesLayoutParams::default(),
            vector_lines_layout: TextVectorLinesLayoutParams::default(),
            shape_layout_kind: TypingShapeLayoutKind::Arc,
            arc_shape_layout: TypingArcShapeLayoutParams::default(),
            circle_shape_layout: TypingCircleShapeLayoutParams::default(),
            spiral_shape_layout: TypingSpiralShapeLayoutParams::default(),
            polygon_shape_layout: TypingPolygonShapeLayoutParams::default(),
            zigzag_shape_layout: TypingZigzagShapeLayoutParams::default(),
            s_curve_shape_layout: TypingSCurveShapeLayoutParams::default(),
            formula_help_open: false,
            text_shape: TextShape::Free,
            text_wrap_mode: TextWrapMode::Aggressive,
            allow_moderate_trees: false,
            shape_min_width_percent: 50.0,
            shape_variant: 5,
            force_bold: false,
            force_italic: false,
            uppercase_text: false,
            trim_extra_spaces: true,
            hanging_punctuation: true,
            new_line_after_sentence: false,
            enable_inline_style_tags: false,
            use_legacy_inline_tags: load_text_tab_use_legacy_inline_tags(),
            overlay_scale: 1.0,
            overlay_rotation_deg: 0.0,
            effect_to_add: AvailableEffectKind::Stroke,
            effects: Vec::new(),
            request_tx,
            result_rx,
            latest_token: 0,
            render_in_flight: false,
            needs_initial_preview: true,
            status_line,
            preview_texture: None,
            preview_size: [1, 1],
            tracked_text_input_ids: Vec::new(),
            text_selection_char_range: None,
            pending_text_selection_restore: None,
            inline_text_target: InlineTextTarget::Source,
            advanced_form_open: false,
            advanced_form_preset: TextFormPreset::FreeNoTree,
            advanced_form_group: None,
            advanced_form_cache: None,
            formed_text: String::new(),
            advanced_text_show_formed: false,
            advanced_form_line_range: (0, 0),
            advanced_form_width_range: (0, 0),
            advanced_form_peak_max: 0,
            advanced_form_peak_base: PeakBase::Min,
            advanced_form_uneven_max: 0,
            advanced_form_conservatism_max: Conservatism::Safe,
            advanced_form_centered: false,
        };
        state.active_font_key = state.current_font_key();
        state.sync_current_font_profile_memory();
        state.sync_selected_formula_preset_by_layout();
        if state.use_system_fonts {
            state.spawn_font_reload();
        }
        state
    }

    fn use_system_fonts(&self) -> bool {
        self.use_system_fonts
    }

    fn reset_text_input_focus_tracking(&mut self) {
        self.tracked_text_input_ids.clear();
    }

    fn track_text_input(&mut self, response: &egui::Response) {
        self.tracked_text_input_ids.push(response.id);
    }

    fn has_focused_text_input(&self, ctx: &egui::Context) -> bool {
        let Some(focused) = ctx.memory(|mem| mem.focused()) else {
            return false;
        };
        self.tracked_text_input_ids.contains(&focused)
    }

    fn eyedropper_active(&self) -> bool {
        if self.text_color_selector.eyedropper_active() {
            return true;
        }
        self.effects.iter().any(EffectCard::eyedropper_active)
    }

    fn eyedropper_consumed_primary_click_this_frame(&self) -> bool {
        if self.text_color_selector.primary_click_consumed_this_frame() {
            return true;
        }
        self.effects
            .iter()
            .any(EffectCard::eyedropper_consumed_primary_click_this_frame)
    }

    fn set_use_system_fonts(&mut self, use_system_fonts: bool) {
        if self.use_system_fonts == use_system_fonts {
            return;
        }
        self.use_system_fonts = use_system_fonts;
        self.spawn_font_reload();
    }

    fn take_use_system_fonts_toggle_request(&mut self) -> Option<bool> {
        self.pending_use_system_fonts_toggle_request.take()
    }

    fn take_font_group_request(&mut self) -> Option<Option<String>> {
        self.pending_font_group_request.take()
    }

    /// Применяет выбранную группу шрифтов (для синхронизации между панелями).
    /// Возвращает `true`, если группа изменилась.
    fn set_font_group(&mut self, group: Option<String>) -> bool {
        if self.selected_font_group == group {
            return false;
        }
        self.selected_font_group = group;
        self.sync_selected_font_group();
        self.ensure_selected_font_in_group();
        if self.preview_enabled {
            self.queue_preview_render();
        }
        true
    }

    fn spawn_font_reload(&mut self) {
        self.latest_font_reload_token = self.latest_font_reload_token.wrapping_add(1);
        let token = self.latest_font_reload_token;
        let fonts_dir = self.fonts_dir.clone();
        let use_system_fonts = self.use_system_fonts;
        let (tx, rx) = mpsc::channel::<FontReloadResult>();
        self.font_reload_rx = Some(rx);
        self.fonts_reload_in_flight = true;
        self.status_line = "Обновление списка шрифтов...".to_string();
        let _ = thread::Builder::new()
            .name("typing-font-reload-worker".to_string())
            .spawn(move || {
                let fonts = load_fonts(fonts_dir.as_path(), use_system_fonts);
                let font_groups = load_font_groups(fonts_dir.as_path());
                let _ = tx.send(FontReloadResult {
                    token,
                    fonts,
                    font_groups,
                });
            });
    }

    fn poll_font_reload_results(&mut self) {
        let Some(rx) = self.font_reload_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                if result.token == self.latest_font_reload_token {
                    let previous_font_key = self
                        .active_font_key
                        .clone()
                        .or_else(|| self.current_font_key());
                    self.fonts = result.fonts;
                    self.font_groups = result.font_groups;
                    self.sync_selected_font_group();
                    self.selected_font_idx = previous_font_key
                        .as_deref()
                        .and_then(|font_key| self.find_font_idx_by_key(font_key))
                        .unwrap_or_else(|| {
                            self.selected_font_idx
                                .min(self.fonts.len().saturating_sub(1))
                        });
                    self.ensure_selected_font_in_group();
                    self.clamp_face_index();
                    self.active_font_key = self.current_font_key();
                    self.status_line = if self.fonts.is_empty() {
                        if self.use_system_fonts {
                            "Не найдены ни локальные, ни системные шрифты".to_string()
                        } else {
                            format!("Не найдено шрифтов в {}", self.fonts_dir.display())
                        }
                    } else {
                        "Готово к рендеру".to_string()
                    };
                    if self.preview_enabled
                        && let Some(font_key) = self.current_font_key()
                    {
                        if let Some(profile) = self.font_profiles_by_key.get(&font_key).cloned() {
                            self.apply_render_data_json_with_options(&profile, false);
                            self.clamp_face_index();
                        } else {
                            self.sync_current_font_profile_memory();
                        }
                    }
                    self.queue_preview_render();
                }
                self.font_reload_rx = None;
                self.fonts_reload_in_flight = false;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.font_reload_rx = None;
                self.fonts_reload_in_flight = false;
                self.status_line = "Ошибка обновления списка шрифтов".to_string();
            }
        }
    }

    fn fonts_reload_in_flight(&self) -> bool {
        self.fonts_reload_in_flight
    }

    fn current_font_key(&self) -> Option<String> {
        self.font_key_by_idx(self.selected_font_idx)
    }

    fn font_key_by_idx(&self, idx: usize) -> Option<String> {
        self.fonts
            .get(idx)
            .map(|font| font.path.to_string_lossy().to_string())
    }

    fn font_label_by_idx(&self, idx: usize) -> Option<String> {
        self.fonts.get(idx).map(|font| font.label.clone())
    }

    /// Имя шрифта для показа в списке: с уточнением в скобках, только когда
    /// выбраны «Все группы» и имя неоднозначно; при конкретной группе — без скобок.
    fn font_display_label(&self, font: &FontEntry) -> String {
        match (self.selected_font_group.is_none(), font.disambig.as_deref()) {
            (true, Some(suffix)) => format!("{} ({})", font.label, suffix),
            _ => font.label.clone(),
        }
    }

    fn find_font_idx_by_key(&self, font_key: &str) -> Option<usize> {
        self.fonts
            .iter()
            .position(|font| font_matches_path(font, font_key))
    }

    fn filtered_font_indices(&self) -> Vec<usize> {
        self.fonts
            .iter()
            .enumerate()
            .filter_map(|(idx, font)| {
                if self
                    .selected_font_group
                    .as_deref()
                    .is_none_or(|group_name| font_in_group(font, group_name))
                {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect()
    }

    fn sync_selected_font_group(&mut self) {
        if self
            .selected_font_group
            .as_ref()
            .is_some_and(|selected| !self.font_groups.iter().any(|group| group == selected))
        {
            self.selected_font_group = None;
        }
    }

    fn ensure_selected_font_in_group(&mut self) {
        if self.selected_font_group.as_deref().is_none() {
            return;
        }

        let selected_group_matches = self
            .selected_font_group
            .as_deref()
            .zip(self.fonts.get(self.selected_font_idx))
            .is_some_and(|(group, font)| font_in_group(font, group));
        if selected_group_matches {
            return;
        }

        if let Some(filtered_idx) = self.filtered_font_indices().into_iter().next() {
            self.selected_font_idx = filtered_idx;
            self.selected_face_idx = 0;
        }
    }

    fn find_font_idx_by_path_or_label(
        &self,
        font_path: Option<&str>,
        font_label: Option<&str>,
    ) -> Option<usize> {
        let mut selected_idx = None;
        if let Some(path_raw) = font_path {
            selected_idx = self
                .fonts
                .iter()
                .position(|font| font_matches_path(font, path_raw));
        }
        if selected_idx.is_none()
            && let Some(label_raw) = font_label
        {
            let label_norm = label_raw.trim().to_ascii_lowercase();
            if !label_norm.is_empty() {
                selected_idx = self.fonts.iter().position(|font| {
                    font.label.to_ascii_lowercase() == label_norm
                        || font
                            .path
                            .file_stem()
                            .and_then(|v| v.to_str())
                            .map(|stem| stem.to_ascii_lowercase() == label_norm)
                            .unwrap_or(false)
                });
            }
        }
        selected_idx
    }

    /// render-data для image-оверлея: только список эффектов (без text_params).
    fn build_image_effects_render_data(&self) -> Value {
        json!({ "effects": self.effects_value_array() })
    }

    /// Загружает только эффекты из render-data (для image-оверлеев без text_params).
    fn load_effects_only_from_render_data(&mut self, render_data: &Value) {
        self.effects = render_data
            .as_object()
            .and_then(|obj| obj.get("effects"))
            .and_then(Value::as_array)
            .map(|effects| parse_effect_cards(effects, self.text_color))
            .unwrap_or_default();
    }

    fn effects_value_array(&self) -> Vec<Value> {
        let mut out = Vec::with_capacity(self.effects.len());
        for effect in self.effects.iter() {
            match effect {
                EffectCard::TextShake(shake) => out.push(json!({
                    "effect": "text_shake",
                    "effect_type": "preprocess",
                    "enabled": true,
                    "spread_x": shake.spread_x_px,
                    "spread_y": shake.spread_y_px,
                    "seed": shake.seed,
                })),
                EffectCard::Stroke(stroke) => out.push(json!({
                    "effect": "stroke",
                    "enabled": true,
                    "width": stroke.width_px,
                    "color": stroke.color.rgba(),
                    "opacity_mode": if stroke.opacity_mode == StrokeOpacityMode::FromContour { "from_contour" } else { "static" },
                    "transparency": stroke.transparency_percent,
                    "opacity": 100.0 - stroke.transparency_percent,
                    "smoothing": stroke.smoothing,
                    "smoothing_strength": stroke.smoothing_strength_percent,
                })),
                EffectCard::Shadow(shadow) => out.push(json!({
                    "effect": "shadow",
                    "enabled": true,
                    "offset_x": shadow.offset_x_px,
                    "offset_y": shadow.offset_y_px,
                    "transparency": shadow.transparency_percent,
                    "opacity": 100.0 - shadow.transparency_percent,
                    "blur": shadow.blur_radius_px,
                    "blur_radius": shadow.blur_radius_px,
                    "blur_px": shadow.blur_radius_px,
                    "mode": if shadow.color_mode == ShadowColorMode::SourceColors { "source" } else { "single" },
                    "use_source_color": shadow.color_mode == ShadowColorMode::SourceColors,
                    "color": shadow.color.rgba(),
                })),
                EffectCard::Blur(blur) => out.push(json!({
                    "effect": "blur",
                    "enabled": true,
                    "radius": blur.radius_px,
                    "blur": blur.radius_px,
                })),
                EffectCard::MotionBlur(blur) => out.push(json!({
                    "effect": "motion_blur",
                    "enabled": true,
                    "angle_deg": blur.angle_deg,
                    "distance": blur.distance_px,
                    "distance_px": blur.distance_px,
                    "sharp_copy": match blur.sharp_copy_mode {
                        MotionBlurSharpCopyMode::None => "none",
                        MotionBlurSharpCopyMode::Over => "over",
                        MotionBlurSharpCopyMode::Under => "under",
                    },
                })),
                EffectCard::DryMedia(dry_media) => out.push(json!({
                    "effect": "dry_media",
                    "enabled": true,
                    "material": match dry_media.material {
                        DryMediaMaterial::Pencil => "pencil",
                        DryMediaMaterial::Chalk => "chalk",
                    },
                    "strength": dry_media.strength,
                    "seed": dry_media.seed,
                    "grain_scale_px": dry_media.grain_scale_px,
                    "grain_amount": dry_media.grain_amount,
                    "edge_roughness": dry_media.edge_roughness,
                    "porosity": dry_media.porosity,
                    "direction_deg": dry_media.direction_deg,
                    "directional_amount": dry_media.directional_amount,
                    "dust_amount": dry_media.dust_amount,
                    "dust_radius_px": dry_media.dust_radius_px,
                    "softness_px": dry_media.softness_px,
                    "use_source_color": dry_media.use_source_color,
                    "color": dry_media.color.rgba(),
                })),
                EffectCard::Glow(glow) => match glow.version {
                    GlowEffectVersion::V1 | GlowEffectVersion::V2 => out.push(json!({
                        "effect": if glow.version == GlowEffectVersion::V1 { "glow_v1" } else { "glow_v2" },
                        "enabled": true,
                        "radius": glow.radius_px,
                        "color": glow.color.rgba(),
                        "opacity_mode": if glow.opacity_mode == StrokeOpacityMode::FromContour { "from_contour" } else { "static" },
                        "transparency": glow.transparency_percent,
                        "opacity": 100.0 - glow.transparency_percent,
                        "fade_strength": glow.fade_strength,
                        "fade_shift": glow.fade_shift,
                    })),
                    GlowEffectVersion::Soft => out.push(json!({
                        "effect": "soft_glow",
                        "enabled": true,
                        "radius": glow.radius_px.round().max(0.0),
                        "softness": glow.softness_px,
                        "color": glow.color.rgba(),
                    })),
                },
                EffectCard::Gradient2(gradient) => out.push(json!({
                    "effect": "gradient2",
                    "enabled": true,
                    "color1": gradient.color1.rgba(),
                    "color2": gradient.color2.rgba(),
                    "angle_deg": gradient.angle_deg,
                    "width_percent": gradient.width_percent,
                    "respect_source_alpha": gradient.respect_source_alpha,
                    "fill_mode": if gradient.fill_mode == Gradient2FillMode::AllOpaque { "all_opaque" } else { "specific_color" },
                    "target_color": gradient.target_color.rgba(),
                })),
                EffectCard::Gradient4(gradient) => out.push(json!({
                    "effect": "gradient4",
                    "enabled": true,
                    "color_top_left": gradient.color_top_left.rgba(),
                    "color_top_right": gradient.color_top_right.rgba(),
                    "color_bottom_left": gradient.color_bottom_left.rgba(),
                    "color_bottom_right": gradient.color_bottom_right.rgba(),
                    "width_percent": gradient.width_percent,
                    "respect_source_alpha": gradient.respect_source_alpha,
                    "fill_mode": if gradient.fill_mode == Gradient4FillMode::AllOpaque { "all_opaque" } else { "specific_color" },
                    "target_color": gradient.target_color.rgba(),
                })),
                EffectCard::Reflect(reflect) => out.push(json!({
                    "effect": "reflect",
                    "enabled": true,
                    "axis": if reflect.axis == ReflectAxis::X { "x" } else { "y" },
                })),
                EffectCard::Shake(shake) => out.push(json!({
                    "effect": "shake",
                    "enabled": true,
                    "angle_deg": shake.angle_deg,
                    "up": shake.up_px,
                    "down": shake.down_px,
                    "steps": shake.steps,
                    "base_fade": shake.base_fade,
                    "decay": shake.decay,
                    "blur": shake.blur_px,
                    "autogrow": shake.autogrow,
                    "grow_margin": shake.grow_margin_px,
                })),
            }
        }
        out
    }

    fn build_current_font_profile_json(&self) -> Value {
        self.build_font_profile_json_for_idx(self.selected_font_idx)
    }

    fn build_font_profile_json_for_idx(&self, font_idx: usize) -> Value {
        let font_path = self
            .fonts
            .get(font_idx)
            .map(|font| font.path.to_string_lossy().to_string())
            .unwrap_or_default();
        let font_label = self
            .fonts
            .get(font_idx)
            .map(|font| font.label.clone())
            .unwrap_or_default();
        self.build_render_data_json_with_font(
            self.text.clone(),
            self.width_px.max(1),
            Some(font_path),
            Some(font_label),
        )
    }

    fn build_render_data_json_for(&self, text: String, width_px: u32) -> Option<Value> {
        let font = self.fonts.get(self.selected_font_idx)?;
        Some(self.build_render_data_json_with_font(
            text,
            width_px.max(1),
            Some(font.path.to_string_lossy().to_string()),
            Some(font.label.clone()),
        ))
    }

    fn build_render_data_json_with_font(
        &self,
        text: String,
        width_px: u32,
        font_path: Option<String>,
        font_label: Option<String>,
    ) -> Value {
        json!({
            "text_params": {
                "text": text,
                "text_color": [self.text_color.r(), self.text_color.g(), self.text_color.b(), self.text_color.a()],
                "font_size_px": self.font_size_px,
                "line_spacing": self.line_spacing.to_token(),
                "kerning_mode": match self.kerning_mode {
                    KerningMode::Metric => "metric",
                    KerningMode::Optical => "optical",
                },
                "kerning": self.kerning.to_token(),
                "glyph_height": self.glyph_height.to_token(),
                "glyph_width": self.glyph_width.to_token(),
                "width_px": width_px.max(1),
                // `align` — легаси-совместимая строка (PSD-экспорт, старые ридеры),
                // `align_bias` — точное непрерывное смещение слайдера лево↔право.
                "align": self.align.legacy_str(),
                "align_bias": self.align.bias,
                "text_line_mode": match self.text_line_mode {
                    TextLineMode::Horizontal => "horizontal",
                    TextLineMode::Vertical => "vertical",
                },
                "vertical_line_direction": match self.vertical_line_direction {
                    VerticalLineDirection::LeftToRight => "left_to_right",
                    VerticalLineDirection::RightToLeft => "right_to_left",
                },
                "text_layout_mode": match self.text_layout_mode {
                    TextLayoutMode::Normal => "normal",
                    TextLayoutMode::Formula => "formula",
                    TextLayoutMode::Shape => "shape",
                    TextLayoutMode::CustomRasterLines => "custom_raster_lines",
                    TextLayoutMode::CustomVectorLines => "custom_vector_lines",
                },
                "formula_layout": text_formula_layout_to_value(&self.formula_layout),
                "shape_layout": self.shape_layout_to_value(),
                "drawn_lines_layout": text_drawn_lines_layout_to_value(&self.drawn_lines_layout_for_render()),
                "vector_lines_layout": text_vector_lines_layout_to_value(&self.vector_lines_layout),
                "selected_face_index": self.selected_face_idx,
                "force_bold": self.force_bold,
                "force_italic": self.force_italic,
                "uppercase_text": self.uppercase_text,
                "trim_extra_spaces": self.trim_extra_spaces,
                "hanging_punctuation": self.hanging_punctuation,
                "new_line_after_sentence": self.new_line_after_sentence,
                "enable_inline_style_tags": self.enable_inline_style_tags,
                "text_wrap_mode": match self.text_wrap_mode {
                    TextWrapMode::None => "none",
                    TextWrapMode::WholeWords => "whole_words",
                    TextWrapMode::Minimal => "minimal",
                    TextWrapMode::Moderate => "moderate",
                    TextWrapMode::Aggressive => "aggressive",
                },
                "allow_moderate_trees": self.allow_moderate_trees,
                "text_shape": match self.text_shape {
                    TextShape::Free => "free",
                    TextShape::Rectangle => "rectangle",
                    TextShape::Oval => "oval",
                    TextShape::Hexagon => "hexagon",
                    TextShape::SoftPeak => "soft_peak",
                },
                "shape_min_width_percent": self.shape_min_width_percent,
                "shape_variant": self.shape_variant,
                "font_path": font_path,
                "font_label": font_label,
                // Сформированный (разбитый на строки) текст «продвинутой формы».
                // Если не пуст — именно он идёт в рендер; `text` остаётся исходным.
                // Переживает перезапуск.
                "formed_text": self.formed_text,
            },
            "effects": self.effects_value_array(),
        })
    }

    fn shape_layout_to_value(&self) -> Value {
        match self.shape_layout_kind {
            TypingShapeLayoutKind::Arc => json!({
                "kind": "arc",
                "length_px": self.arc_shape_layout.length_px,
                "amplitude_px": self.arc_shape_layout.amplitude_px,
                "width_px": self.arc_shape_layout.length_px,
                "height_px": self.arc_shape_layout.amplitude_px,
                "frequency": self.arc_shape_layout.frequency,
                "orientation": self.arc_shape_layout.orientation.as_config_str(),
            }),
            TypingShapeLayoutKind::Circle => json!({
                "kind": "circle",
                "width_px": self.circle_shape_layout.width_px,
                "height_px": self.circle_shape_layout.height_px,
            }),
            TypingShapeLayoutKind::Spiral => json!({
                "kind": "spiral",
                "width_px": self.spiral_shape_layout.width_px,
                "height_px": self.spiral_shape_layout.height_px,
                "turns": self.spiral_shape_layout.turns,
                "inner_ratio": self.spiral_shape_layout.inner_ratio,
            }),
            TypingShapeLayoutKind::Polygon => json!({
                "kind": "polygon",
                "width_px": self.polygon_shape_layout.width_px,
                "height_px": self.polygon_shape_layout.height_px,
                "sides": self.polygon_shape_layout.sides,
            }),
            TypingShapeLayoutKind::Zigzag => json!({
                "kind": "zigzag",
                "width_px": self.zigzag_shape_layout.width_px,
                "height_px": self.zigzag_shape_layout.height_px,
                "segments": self.zigzag_shape_layout.segments,
            }),
            TypingShapeLayoutKind::SCurve => json!({
                "kind": "s_curve",
                "width_px": self.s_curve_shape_layout.width_px,
                "height_px": self.s_curve_shape_layout.height_px,
                "bends": self.s_curve_shape_layout.bends,
            }),
        }
    }

    fn apply_shape_layout_json(&mut self, obj: &Map<String, Value>) {
        let kind = obj
            .get("kind")
            .and_then(Value::as_str)
            .map(|raw| raw.trim().to_ascii_lowercase())
            .unwrap_or_else(|| "arc".to_string());
        self.shape_layout_kind = match kind.as_str() {
            "arc" => TypingShapeLayoutKind::Arc,
            "circle" | "ellipse" | "oval" => TypingShapeLayoutKind::Circle,
            "spiral" => TypingShapeLayoutKind::Spiral,
            "polygon" => TypingShapeLayoutKind::Polygon,
            "zigzag" => TypingShapeLayoutKind::Zigzag,
            "s_curve" | "s-curve" | "scurve" => TypingShapeLayoutKind::SCurve,
            _ => TypingShapeLayoutKind::Arc,
        };
        self.arc_shape_layout.length_px = obj
            .get("length_px")
            .and_then(value_as_f32)
            .or_else(|| obj.get("width_px").and_then(value_as_f32))
            .unwrap_or(self.arc_shape_layout.length_px)
            .clamp(1.0, 10_000.0);
        self.arc_shape_layout.amplitude_px = obj
            .get("amplitude_px")
            .and_then(value_as_f32)
            .or_else(|| obj.get("height_px").and_then(value_as_f32))
            .unwrap_or(self.arc_shape_layout.amplitude_px)
            .clamp(-10_000.0, 10_000.0);
        self.arc_shape_layout.frequency = obj
            .get("frequency")
            .and_then(value_as_f32)
            .unwrap_or(self.arc_shape_layout.frequency)
            .clamp(0.1, 32.0);
        self.arc_shape_layout.orientation = obj
            .get("orientation")
            .and_then(Value::as_str)
            .and_then(TypingArcOrientation::from_config_str)
            .unwrap_or(self.arc_shape_layout.orientation);
        self.circle_shape_layout.width_px = obj
            .get("width_px")
            .and_then(value_as_f32)
            .unwrap_or(self.circle_shape_layout.width_px)
            .clamp(1.0, 10_000.0);
        self.circle_shape_layout.height_px = obj
            .get("height_px")
            .and_then(value_as_f32)
            .unwrap_or(self.circle_shape_layout.height_px)
            .clamp(1.0, 10_000.0);
        self.spiral_shape_layout.width_px = obj
            .get("width_px")
            .and_then(value_as_f32)
            .unwrap_or(self.spiral_shape_layout.width_px)
            .clamp(1.0, 10_000.0);
        self.spiral_shape_layout.height_px = obj
            .get("height_px")
            .and_then(value_as_f32)
            .unwrap_or(self.spiral_shape_layout.height_px)
            .clamp(1.0, 10_000.0);
        self.spiral_shape_layout.turns = obj
            .get("turns")
            .and_then(value_as_f32)
            .unwrap_or(self.spiral_shape_layout.turns)
            .clamp(0.25, 16.0);
        self.spiral_shape_layout.inner_ratio = obj
            .get("inner_ratio")
            .and_then(value_as_f32)
            .unwrap_or(self.spiral_shape_layout.inner_ratio)
            .clamp(0.0, 0.98);
        self.polygon_shape_layout.width_px = obj
            .get("width_px")
            .and_then(value_as_f32)
            .unwrap_or(self.polygon_shape_layout.width_px)
            .clamp(1.0, 10_000.0);
        self.polygon_shape_layout.height_px = obj
            .get("height_px")
            .and_then(value_as_f32)
            .unwrap_or(self.polygon_shape_layout.height_px)
            .clamp(1.0, 10_000.0);
        self.polygon_shape_layout.sides = obj
            .get("sides")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(self.polygon_shape_layout.sides)
            .clamp(3, 12);
        self.zigzag_shape_layout.width_px = obj
            .get("width_px")
            .and_then(value_as_f32)
            .unwrap_or(self.zigzag_shape_layout.width_px)
            .clamp(1.0, 10_000.0);
        self.zigzag_shape_layout.height_px = obj
            .get("height_px")
            .and_then(value_as_f32)
            .unwrap_or(self.zigzag_shape_layout.height_px)
            .clamp(-10_000.0, 10_000.0);
        self.zigzag_shape_layout.segments = obj
            .get("segments")
            .and_then(value_as_f32)
            .unwrap_or(self.zigzag_shape_layout.segments)
            .clamp(0.5, 32.0);
        self.s_curve_shape_layout.width_px = obj
            .get("width_px")
            .and_then(value_as_f32)
            .unwrap_or(self.s_curve_shape_layout.width_px)
            .clamp(1.0, 10_000.0);
        self.s_curve_shape_layout.height_px = obj
            .get("height_px")
            .and_then(value_as_f32)
            .unwrap_or(self.s_curve_shape_layout.height_px)
            .clamp(-10_000.0, 10_000.0);
        self.s_curve_shape_layout.bends = obj
            .get("bends")
            .and_then(value_as_f32)
            .unwrap_or(self.s_curve_shape_layout.bends)
            .clamp(0.25, 8.0);
    }

    fn formula_layout_for_render(&self) -> TextFormulaLayoutParams {
        match self.text_layout_mode {
            TextLayoutMode::Shape => self.shape_formula_layout(),
            _ => self.formula_layout.clone(),
        }
    }

    fn drawn_lines_layout_for_render(&self) -> TextDrawnLinesLayoutParams {
        self.drawn_lines_layout.clone()
    }

    fn shape_formula_layout(&self) -> TextFormulaLayoutParams {
        let mut layout = self.formula_layout.clone();
        match self.shape_layout_kind {
            TypingShapeLayoutKind::Arc => {
                match self.arc_shape_layout.orientation {
                    TypingArcOrientation::Horizontal => {
                        layout.x_expr = "a * (t - 0.5)".to_string();
                        layout.y_expr = "b * sin(pi * c * t)".to_string();
                    }
                    TypingArcOrientation::Vertical => {
                        layout.x_expr = "b * sin(pi * c * t)".to_string();
                        layout.y_expr = "a * (t - 0.5)".to_string();
                    }
                }
                layout.rotation_expr = "0".to_string();
                layout.t_start = 0.0;
                layout.t_end = 1.0;
                layout.offset_x_px = 0.0;
                layout.offset_y_px = 0.0;
                layout.scale_x = 1.0;
                layout.scale_y = 1.0;
                layout.vars[0] = self.arc_shape_layout.length_px.clamp(1.0, 10_000.0);
                layout.vars[1] = self
                    .arc_shape_layout
                    .amplitude_px
                    .clamp(-10_000.0, 10_000.0);
                layout.vars[2] = self.arc_shape_layout.frequency.clamp(0.1, 32.0);
            }
            TypingShapeLayoutKind::Circle => {
                layout.x_expr = "a * cos(tau * t)".to_string();
                layout.y_expr = "b * sin(tau * t)".to_string();
                layout.rotation_expr = "0".to_string();
                layout.t_start = 0.0;
                layout.t_end = 1.0;
                layout.offset_x_px = 0.0;
                layout.offset_y_px = 0.0;
                layout.scale_x = 1.0;
                layout.scale_y = 1.0;
                layout.vars[0] = (self.circle_shape_layout.width_px * 0.5).clamp(1.0, 10_000.0);
                layout.vars[1] = (self.circle_shape_layout.height_px * 0.5).clamp(1.0, 10_000.0);
            }
            TypingShapeLayoutKind::Spiral => {
                layout.x_expr = "(a * (d + (1 - d) * t)) * cos(tau * c * t)".to_string();
                layout.y_expr = "(b * (d + (1 - d) * t)) * sin(tau * c * t)".to_string();
                layout.rotation_expr = "0".to_string();
                layout.t_start = 0.0;
                layout.t_end = 1.0;
                layout.offset_x_px = 0.0;
                layout.offset_y_px = 0.0;
                layout.scale_x = 1.0;
                layout.scale_y = 1.0;
                layout.vars[0] = (self.spiral_shape_layout.width_px * 0.5).clamp(1.0, 10_000.0);
                layout.vars[1] = (self.spiral_shape_layout.height_px * 0.5).clamp(1.0, 10_000.0);
                layout.vars[2] = self.spiral_shape_layout.turns.clamp(0.25, 16.0);
                layout.vars[3] = self.spiral_shape_layout.inner_ratio.clamp(0.0, 0.98);
            }
            TypingShapeLayoutKind::Polygon => {
                layout.x_expr = "a * cos(tau * t) * cos(pi / c) / cos(atan2(sin(c * tau * t), cos(c * tau * t)) / c)".to_string();
                layout.y_expr = "b * sin(tau * t) * cos(pi / c) / cos(atan2(sin(c * tau * t), cos(c * tau * t)) / c)".to_string();
                layout.rotation_expr = "0".to_string();
                layout.t_start = 0.0;
                layout.t_end = 1.0;
                layout.offset_x_px = 0.0;
                layout.offset_y_px = 0.0;
                layout.scale_x = 1.0;
                layout.scale_y = 1.0;
                layout.vars[0] = (self.polygon_shape_layout.width_px * 0.5).clamp(1.0, 10_000.0);
                layout.vars[1] = (self.polygon_shape_layout.height_px * 0.5).clamp(1.0, 10_000.0);
                layout.vars[2] = self.polygon_shape_layout.sides.clamp(3, 12) as f32;
            }
            TypingShapeLayoutKind::Zigzag => {
                layout.x_expr = "a * (t - 0.5)".to_string();
                layout.y_expr = "b * (2 / pi) * asin(sin(pi * c * t))".to_string();
                layout.rotation_expr = "0".to_string();
                layout.t_start = 0.0;
                layout.t_end = 1.0;
                layout.offset_x_px = 0.0;
                layout.offset_y_px = 0.0;
                layout.scale_x = 1.0;
                layout.scale_y = 1.0;
                layout.vars[0] = self.zigzag_shape_layout.width_px.clamp(1.0, 10_000.0);
                layout.vars[1] = self
                    .zigzag_shape_layout
                    .height_px
                    .clamp(-10_000.0, 10_000.0);
                layout.vars[2] = self.zigzag_shape_layout.segments.clamp(0.5, 32.0);
            }
            TypingShapeLayoutKind::SCurve => {
                layout.x_expr = "a * (t - 0.5)".to_string();
                layout.y_expr = "b * sin(pi * c * (t - 0.5))".to_string();
                layout.rotation_expr = "0".to_string();
                layout.t_start = 0.0;
                layout.t_end = 1.0;
                layout.offset_x_px = 0.0;
                layout.offset_y_px = 0.0;
                layout.scale_x = 1.0;
                layout.scale_y = 1.0;
                layout.vars[0] = self.s_curve_shape_layout.width_px.clamp(1.0, 10_000.0);
                layout.vars[1] = self
                    .s_curve_shape_layout
                    .height_px
                    .clamp(-10_000.0, 10_000.0);
                layout.vars[2] = self.s_curve_shape_layout.bends.clamp(0.25, 8.0);
            }
        }
        layout
    }

    fn store_current_font_profile_by_idx(&mut self, idx: usize) {
        if !self.preview_enabled {
            return;
        }
        let Some(font_key) = self.font_key_by_idx(idx) else {
            return;
        };
        self.font_profiles_by_key
            .insert(font_key.clone(), self.build_font_profile_json_for_idx(idx));
        self.active_font_key = Some(font_key);
    }

    fn sync_current_font_profile_memory(&mut self) {
        if !self.preview_enabled {
            return;
        }
        self.store_current_font_profile_by_idx(self.selected_font_idx);
    }

    fn handle_create_font_selection_change(&mut self, prev_font_idx: usize) -> bool {
        if !self.preview_enabled {
            return false;
        }
        self.store_current_font_profile_by_idx(prev_font_idx);
        let Some(new_font_key) = self.current_font_key() else {
            return false;
        };
        self.active_font_key = Some(new_font_key.clone());
        if let Some(profile) = self.font_profiles_by_key.get(&new_font_key).cloned() {
            self.apply_render_data_json_with_options(&profile, false);
            self.clamp_face_index();
            return true;
        }
        self.selected_face_idx = 0;
        self.sync_current_font_profile_memory();
        true
    }

    fn draw_create_presets_section(&mut self, ui: &mut egui::Ui) {
        if !self.preview_enabled {
            return;
        }
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Пресеты").strong());
                let mut names: Vec<String> = self.presets_by_name.keys().cloned().collect();
                names.sort();
                let selected_text = self
                    .selected_preset_name
                    .as_deref()
                    .unwrap_or(TEXT_PRESET_NONE_LABEL);
                let prev_selected = self.selected_preset_name.clone();
                let preset_len = names.len() + 1;
                let mut preset_idx = self
                    .selected_preset_name
                    .as_ref()
                    .and_then(|selected| names.iter().position(|name| name == selected))
                    .map(|idx| idx + 1)
                    .unwrap_or(0);
                let preset_combo = WheelComboBox::from_label("Текущий пресет")
                    .selected_text(selected_text)
                    .show_ui_with_wheel(ui, |ui| {
                        if ui
                            .selectable_label(preset_idx == 0, TEXT_PRESET_NONE_LABEL)
                            .clicked()
                        {
                            preset_idx = 0;
                        }
                        for (idx, name) in names.iter().enumerate() {
                            if ui.selectable_label(preset_idx == idx + 1, name).clicked() {
                                preset_idx = idx + 1;
                            }
                        }
                    });
                if let Some(steps) = preset_combo.wheel_steps {
                    cycle_wrapped_index(&mut preset_idx, preset_len, steps);
                }
                self.selected_preset_name = if preset_idx == 0 {
                    None
                } else {
                    names.get(preset_idx - 1).cloned()
                };
                if self.selected_preset_name != prev_selected
                    && let Some(name) = self.selected_preset_name.clone()
                {
                    self.apply_preset_by_name(name);
                    self.queue_preview_render();
                }
            });
            ui.horizontal(|ui| {
                let preset_name_resp = ui.add(
                    egui::TextEdit::singleline(&mut self.preset_name_input)
                        .id_salt("typing_preset_name_input")
                        .hint_text("Сохранить пресет")
                        .desired_width((ui.available_width() - 96.0).max(120.0)),
                );
                self.track_text_input(&preset_name_resp);
                if ui.button("Сохранить").clicked() {
                    self.save_current_preset();
                }
            });
        });
    }

    fn apply_preset_by_name(&mut self, name: String) {
        let Some(preset) = self.presets_by_name.get(&name).cloned() else {
            return;
        };
        self.font_profiles_by_key = preset.font_profiles;

        let target_idx = self
            .find_font_idx_by_key(&preset.primary_font_key)
            .or_else(|| {
                self.find_font_idx_by_path_or_label(
                    preset.primary_font_path.as_deref(),
                    preset.primary_font_label.as_deref(),
                )
            });
        if let Some(idx) = target_idx {
            self.selected_font_idx = idx;
        }
        self.active_font_key = self.current_font_key();
        if let Some(font_key) = self.current_font_key() {
            if let Some(profile) = self.font_profiles_by_key.get(&font_key).cloned() {
                self.apply_render_data_json_with_options(&profile, false);
            } else {
                self.selected_face_idx = 0;
                self.sync_current_font_profile_memory();
            }
        }
        self.clamp_face_index();
        self.selected_preset_name = Some(name);
    }

    fn save_current_preset(&mut self) {
        if !self.preview_enabled {
            return;
        }
        let preset_name = self.preset_name_input.trim().to_string();
        if preset_name.is_empty() {
            return;
        }

        self.sync_current_font_profile_memory();

        let mut font_profiles = self.font_profiles_by_key.clone();
        let current_profile = self.build_current_font_profile_json();
        for idx in 0..self.fonts.len() {
            if let Some(key) = self.font_key_by_idx(idx) {
                font_profiles
                    .entry(key)
                    .or_insert_with(|| current_profile.clone());
            }
        }
        let primary_font_key = self.current_font_key().unwrap_or_default();
        let primary_font_path = self
            .fonts
            .get(self.selected_font_idx)
            .map(|font| font.path.to_string_lossy().to_string());
        let primary_font_label = self.font_label_by_idx(self.selected_font_idx);
        self.presets_by_name.insert(
            preset_name.clone(),
            TypingCreatePreset {
                primary_font_key,
                primary_font_path,
                primary_font_label,
                font_profiles,
            },
        );
        self.selected_preset_name = Some(preset_name.clone());

        let presets = self.presets_by_name.clone();
        let _ = thread::Builder::new()
            .name("typing-save-create-presets".to_string())
            .spawn(move || {
                let _ = save_text_tab_create_presets(&presets);
            });
    }

    fn apply_formula_preset_by_name(&mut self, name: String) -> bool {
        let Some(preset) = self.formula_presets_by_name.get(&name).cloned() else {
            return false;
        };
        self.formula_layout = preset.layout;
        self.selected_formula_preset_name = Some(name);
        true
    }

    fn save_current_formula_preset(&mut self) {
        let preset_name = self.formula_preset_name_input.trim().to_string();
        if preset_name.is_empty() {
            return;
        }
        self.formula_presets_by_name.insert(
            preset_name.clone(),
            TypingFormulaPreset {
                layout: self.formula_layout.clone(),
            },
        );
        self.selected_formula_preset_name = Some(preset_name);
        let presets = self.formula_presets_by_name.clone();
        let _ = thread::Builder::new()
            .name("typing-save-formula-presets".to_string())
            .spawn(move || {
                let _ = save_text_tab_formula_presets(&presets);
            });
    }

    fn swap_formula_xy_expressions(&mut self) {
        std::mem::swap(
            &mut self.formula_layout.x_expr,
            &mut self.formula_layout.y_expr,
        );
        self.selected_formula_preset_name = None;
    }

    fn sync_selected_formula_preset_by_layout(&mut self) {
        self.selected_formula_preset_name =
            self.formula_presets_by_name
                .iter()
                .find_map(|(name, preset)| {
                    if formula_layout_approx_eq(&self.formula_layout, &preset.layout) {
                        Some(name.clone())
                    } else {
                        None
                    }
                });
    }

    fn ensure_combo_font_family(
        &mut self,
        ctx: &egui::Context,
        font_path: &Path,
        face_index: usize,
    ) -> Option<egui::FontFamily> {
        let cache_key = (font_path.to_path_buf(), face_index);
        // Имя egui-семейства детерминированно выводится из (путь, индекс начертания):
        // один и тот же файл всегда даёт одно имя, разные файлы — разные имена. Это
        // критично, потому что `create_panel` и `edit_panel` — две независимые панели
        // с общим egui-`Context`. При последовательной нумерации обе генерировали
        // совпадающие имена (`typing-panel-combo-font-1` …) для РАЗНЫХ файлов, а egui
        // хранит данные шрифта по имени — поздняя регистрация затирала раннюю, и одна
        // панель начинала рисовать чужой шрифт (в т.ч. в окне продвинутой формы).
        let font_name = combo_font_family_name(font_path, face_index);
        let family = egui::FontFamily::Name(font_name.clone().into());
        if is_font_family_bound(ctx, &family) {
            self.combo_font_family_cache.insert(cache_key, font_name);
            return Some(family);
        }

        let font_bytes = fs::read(font_path).ok()?;
        let mut font_data = egui::FontData::from_owned(font_bytes);
        font_data.index = face_index as u32;
        ctx.add_font(egui::epaint::text::FontInsert::new(
            font_name.as_str(),
            font_data,
            vec![egui::epaint::text::InsertFontFamily {
                family: egui::FontFamily::Name(font_name.clone().into()),
                priority: egui::epaint::text::FontPriority::Highest,
            }],
        ));
        self.combo_font_family_cache.insert(cache_key, font_name);
        if is_font_family_bound(ctx, &family) {
            Some(family)
        } else {
            None
        }
    }

    fn draw_font_combo_option(
        &mut self,
        ui: &mut egui::Ui,
        label: &str,
        font_path: &Path,
        face_index: usize,
        selected: bool,
    ) -> bool {
        let prev_override = ui.style().override_font_id.clone();
        if let Some(family) = self.ensure_combo_font_family(ui.ctx(), font_path, face_index) {
            ui.style_mut().override_font_id = Some(egui::FontId::new(14.0, family));
        }
        let clicked = ui.selectable_label(selected, label).clicked();
        ui.style_mut().override_font_id = prev_override;
        clicked
    }

    fn ensure_initial_preview_request(&mut self) {
        if !self.preview_enabled {
            return;
        }
        if !self.needs_initial_preview {
            return;
        }
        self.needs_initial_preview = false;
        self.queue_preview_render();
    }

    fn clamp_face_index(&mut self) {
        if let Some(font) = self.fonts.get(self.selected_font_idx) {
            let max_idx = font.faces.len().saturating_sub(1);
            self.selected_face_idx = self.selected_face_idx.min(max_idx);
        } else {
            self.selected_face_idx = 0;
        }
    }

    fn draw_preview_section(&mut self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                if self.render_in_flight || self.fonts_reload_in_flight {
                    ui.spinner();
                }
                let status_width = ui.available_width().max(1.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(status_width, 0.0),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| {
                        ui.set_max_width(status_width);
                        ui.add(egui::Label::new(self.status_line.as_str()).wrap());
                    },
                );
            });

            ui.add_space(4.0);

            egui::Frame::group(ui.style()).show(ui, |ui| {
                let box_size = egui::vec2(ui.available_width(), CREATE_PREVIEW_HEIGHT_PX);
                let (preview_rect, _) = ui.allocate_exact_size(box_size, egui::Sense::hover());
                if let Some(texture) = &self.preview_texture {
                    let image_size = fit_size_to_box(texture.size(), preview_rect.size());
                    let image_rect = Rect::from_center_size(preview_rect.center(), image_size);
                    let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                    ui.painter()
                        .image(texture.id(), image_rect, uv, Color32::WHITE);
                } else {
                    ui.scope_builder(
                        egui::UiBuilder::new().max_rect(preview_rect).layout(
                            egui::Layout::centered_and_justified(egui::Direction::TopDown),
                        ),
                        |ui| {
                            ui.label("Превью ещё не готово.");
                        },
                    );
                }
            });
        });
    }

    fn draw_params_section(
        &mut self,
        ui: &mut egui::Ui,
        stacked_columns: bool,
        remap_wheel_to_horizontal: bool,
    ) {
        let mut params_changed = false;
        params_changed |= self.draw_main_text_params(
            ui,
            stacked_columns,
            remap_wheel_to_horizontal,
            self.preview_enabled,
            // Панель создания всегда работает с доступным шрифтом.
            false,
        );

        if params_changed {
            self.sync_current_font_profile_memory();
            self.queue_preview_render();
        }
    }

    fn draw_effects_section(&mut self, ui: &mut egui::Ui, vertical_cards: bool) -> bool {
        let mut changed = false;
        ui.label(if vertical_cards {
            "Порядок применения: сверху вниз"
        } else {
            "Порядок применения: слева направо"
        });
        ui.horizontal(|ui| {
            let effect_kinds = [
                AvailableEffectKind::TextShake,
                AvailableEffectKind::Stroke,
                AvailableEffectKind::Shadow,
                AvailableEffectKind::Blur,
                AvailableEffectKind::MotionBlur,
                AvailableEffectKind::DryMedia,
                AvailableEffectKind::GlowV1,
                AvailableEffectKind::GlowV2,
                AvailableEffectKind::SoftGlow,
                AvailableEffectKind::Gradient2,
                AvailableEffectKind::Gradient4,
                AvailableEffectKind::Reflect,
                AvailableEffectKind::Shake,
            ];
            let mut effect_idx = effect_kinds
                .iter()
                .position(|kind| *kind == self.effect_to_add)
                .unwrap_or(0);
            let effect_combo = WheelComboBox::from_label("Добавить эффект")
                .selected_text(self.effect_to_add.label())
                .show_ui_with_wheel(ui, |ui| {
                    for (idx, kind) in effect_kinds.iter().enumerate() {
                        if ui
                            .selectable_label(effect_idx == idx, kind.label())
                            .clicked()
                        {
                            effect_idx = idx;
                        }
                    }
                });
            if let Some(steps) = effect_combo.wheel_steps {
                cycle_wrapped_index(&mut effect_idx, effect_kinds.len(), steps);
            }
            self.effect_to_add = effect_kinds[effect_idx];

            if ui.button("+ Добавить").clicked() {
                self.effects.push(Self::default_effect_card(
                    self.effect_to_add,
                    self.text_color,
                ));
                changed = true;
            }
        });

        ui.add_space(4.0);
        if self.effects.is_empty() {
            ui.label("Эффекты не добавлены.");
        } else {
            let mut move_up: Option<usize> = None;
            let mut move_down: Option<usize> = None;
            let mut remove_idx: Option<usize> = None;
            if vertical_cards {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    let effects_len = self.effects.len();
                    for idx in 0..effects_len {
                        ui.push_id(("typing_effect_card_vertical", idx), |ui| {
                            ui.group(|ui| {
                                ui.horizontal(|ui| {
                                    ui.label(format!(
                                        "#{} {}",
                                        idx + 1,
                                        effect_card_title(&self.effects[idx])
                                    ));
                                    if ui
                                        .add_enabled(idx > 0, egui::Button::new("↑"))
                                        .on_hover_text("Переместить выше")
                                        .clicked()
                                    {
                                        move_up = Some(idx);
                                    }
                                    if ui
                                        .add_enabled(idx + 1 < effects_len, egui::Button::new("↓"))
                                        .on_hover_text("Переместить ниже")
                                        .clicked()
                                    {
                                        move_down = Some(idx);
                                    }
                                    if ui.button("X").on_hover_text("Удалить").clicked() {
                                        remove_idx = Some(idx);
                                    }
                                });
                                ui.separator();
                                changed |= draw_effect_card_controls(ui, &mut self.effects[idx]);
                            });
                        });
                        if idx + 1 < effects_len {
                            ui.add_space(4.0);
                        }
                    }
                });
            } else {
                let cards_viewport_h = ui.available_height().clamp(170.0, 260.0);
                let card_w = 320.0;

                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_max_height(cards_viewport_h);

                    ui.scope(|ui| {
                        ui.style_mut().always_scroll_the_only_direction = true;
                        egui::ScrollArea::horizontal()
                            .id_salt("typing_create_effects_hscroll")
                            .scroll_source(egui::scroll_area::ScrollSource {
                                scroll_bar: true,
                                drag: true,
                                mouse_wheel: false,
                            })
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.horizontal_top(|ui| {
                                    let effects_len = self.effects.len();
                                    for idx in 0..effects_len {
                                        ui.group(|ui| {
                                            ui.set_width(card_w);
                                            ui.set_min_width(card_w);
                                            ui.set_max_width(card_w);
                                            ui.set_max_height(cards_viewport_h - 12.0);

                                            ui.with_layout(
                                                egui::Layout::top_down(Align::Min),
                                                |ui| {
                                                    ui.push_id(("typing_effect_card", idx), |ui| {
                                                        ui.horizontal(|ui| {
                                                            ui.label(format!(
                                                                "#{} {}",
                                                                idx + 1,
                                                                effect_card_title(
                                                                    &self.effects[idx]
                                                                )
                                                            ));
                                                            if ui
                                                                .add_enabled(
                                                                    idx > 0,
                                                                    egui::Button::new("←"),
                                                                )
                                                                .on_hover_text("Переместить влево")
                                                                .clicked()
                                                            {
                                                                move_up = Some(idx);
                                                            }
                                                            if ui
                                                                .add_enabled(
                                                                    idx + 1 < effects_len,
                                                                    egui::Button::new("→"),
                                                                )
                                                                .on_hover_text("Переместить вправо")
                                                                .clicked()
                                                            {
                                                                move_down = Some(idx);
                                                            }
                                                            if ui
                                                                .button("X")
                                                                .on_hover_text("Удалить")
                                                                .clicked()
                                                            {
                                                                remove_idx = Some(idx);
                                                            }
                                                        });
                                                        ui.separator();

                                                        egui::ScrollArea::vertical()
                                                            .id_salt((
                                                                "typing_effect_card_vscroll",
                                                                idx,
                                                            ))
                                                            .auto_shrink([false, false])
                                                            .max_height(cards_viewport_h - 82.0)
                                                            .show(ui, |ui| {
                                                                changed |=
                                                                    draw_effect_card_controls(
                                                                        ui,
                                                                        &mut self.effects[idx],
                                                                    );
                                                            });
                                                    });
                                                },
                                            );
                                        });
                                    }
                                });
                            });
                    });
                });
            }

            if let Some(idx) = remove_idx {
                self.effects.remove(idx);
                changed = true;
            }
            if let Some(idx) = move_up {
                self.effects.swap(idx - 1, idx);
                changed = true;
            }
            if let Some(idx) = move_down {
                self.effects.swap(idx, idx + 1);
                changed = true;
            }
        }

        if changed {
            self.sync_current_font_profile_memory();
            self.queue_preview_render();
        }
        changed
    }

    fn default_effect_card(kind: AvailableEffectKind, text_color: Color32) -> EffectCard {
        match kind {
            AvailableEffectKind::TextShake => EffectCard::TextShake(TextShakeEffectCard {
                spread_x_px: 2.0,
                spread_y_px: 2.0,
                seed: random_seed(),
            }),
            AvailableEffectKind::Stroke => EffectCard::Stroke(StrokeEffectCard {
                width_px: 2.96,
                color: ColorField::new(Color32::WHITE),
                opacity_mode: StrokeOpacityMode::Static,
                transparency_percent: 0.0,
                smoothing: false,
                smoothing_strength_percent: 100.0,
            }),
            AvailableEffectKind::Shadow => EffectCard::Shadow(ShadowEffectCard {
                offset_x_px: 4,
                offset_y_px: 4,
                transparency_percent: 40.0,
                blur_radius_px: 0.0,
                color_mode: ShadowColorMode::SingleColor,
                color: ColorField::new(Color32::BLACK),
            }),
            AvailableEffectKind::Blur => EffectCard::Blur(BlurEffectCard { radius_px: 4.0 }),
            AvailableEffectKind::MotionBlur => EffectCard::MotionBlur(MotionBlurEffectCard {
                angle_deg: 20.0,
                distance_px: 11.0,
                sharp_copy_mode: MotionBlurSharpCopyMode::None,
            }),
            AvailableEffectKind::DryMedia => EffectCard::DryMedia(DryMediaEffectCard {
                material: DryMediaMaterial::Pencil,
                strength: 0.65,
                seed: 1,
                grain_scale_px: 2.0,
                grain_amount: 0.35,
                edge_roughness: 0.45,
                porosity: 0.20,
                direction_deg: 82.0,
                directional_amount: 0.30,
                dust_amount: 0.08,
                dust_radius_px: 2.0,
                softness_px: 0.6,
                use_source_color: true,
                color: ColorField::new(text_color),
            }),
            AvailableEffectKind::GlowV1 => EffectCard::Glow(GlowEffectCard {
                version: GlowEffectVersion::V1,
                radius_px: 16.0,
                softness_px: 0.0,
                color: ColorField::new(Color32::BLACK),
                opacity_mode: StrokeOpacityMode::FromContour,
                transparency_percent: 0.0,
                fade_strength: 0.0,
                fade_shift: 0.0,
            }),
            AvailableEffectKind::GlowV2 => EffectCard::Glow(GlowEffectCard {
                version: GlowEffectVersion::V2,
                radius_px: 16.0,
                softness_px: 0.0,
                color: ColorField::new(Color32::BLACK),
                opacity_mode: StrokeOpacityMode::FromContour,
                transparency_percent: 0.0,
                fade_strength: 0.0,
                fade_shift: 0.0,
            }),
            AvailableEffectKind::SoftGlow => EffectCard::Glow(GlowEffectCard {
                version: GlowEffectVersion::Soft,
                radius_px: 8.0,
                softness_px: 4.0,
                color: ColorField::new(Color32::BLACK),
                opacity_mode: StrokeOpacityMode::FromContour,
                transparency_percent: 0.0,
                fade_strength: 0.0,
                fade_shift: 0.0,
            }),
            AvailableEffectKind::Gradient2 => EffectCard::Gradient2(Gradient2EffectCard {
                color1: ColorField::new(Color32::WHITE),
                color2: ColorField::new(Color32::BLACK),
                angle_deg: 90.0,
                width_percent: 100.0,
                respect_source_alpha: true,
                fill_mode: Gradient2FillMode::AllOpaque,
                target_color: ColorField::new(text_color),
            }),
            AvailableEffectKind::Gradient4 => EffectCard::Gradient4(Gradient4EffectCard {
                color_top_left: ColorField::new(Color32::WHITE),
                color_top_right: ColorField::new(Color32::WHITE),
                color_bottom_left: ColorField::new(Color32::BLACK),
                color_bottom_right: ColorField::new(Color32::BLACK),
                width_percent: 100.0,
                respect_source_alpha: true,
                fill_mode: Gradient4FillMode::AllOpaque,
                target_color: ColorField::new(text_color),
            }),
            AvailableEffectKind::Reflect => EffectCard::Reflect(ReflectEffectCard {
                axis: ReflectAxis::Y,
            }),
            AvailableEffectKind::Shake => EffectCard::Shake(ShakeEffectCard {
                angle_deg: 90.0,
                up_px: 0.0,
                down_px: 40.0,
                steps: 12,
                base_fade: 0.30,
                decay: 0.15,
                blur_px: 2,
                autogrow: true,
                grow_margin_px: 0,
            }),
        }
    }

    fn effects_json(&self) -> String {
        serde_json::to_string(&self.effects_value_array()).unwrap_or_else(|_| "[]".to_string())
    }

    fn draw_right_section(
        &mut self,
        ui: &mut egui::Ui,
        mask_panel_open: bool,
        clean_overlays_visible: bool,
        strict_pixel_movement: bool,
        export_default_dir: Option<&Path>,
        export_status: &TypingExportUiStatus,
        export_format: TypingExportFormat,
    ) -> TypingRightSectionActions {
        let mut out = TypingRightSectionActions {
            toggle_mask: false,
            changed_clean_overlays: None,
            export_to_folder: None,
            changed_export_format: None,
            round_text_positions: false,
            create_image_request: None,
            changed_strict_pixel_movement: None,
        };
        ui.vertical(|ui| {
            let mask_button_label = if mask_panel_open {
                "Закрыть маску обрезки"
            } else {
                "Открыть маску обрезки"
            };
            if ui.button(mask_button_label).clicked() {
                out.toggle_mask = true;
            }
            if self.preview_enabled {
                let mut format = export_format;
                ui.horizontal(|ui| {
                    ui.label("Формат:");
                    if ui
                        .selectable_value(&mut format, TypingExportFormat::Png, "PNG")
                        .clicked()
                        || ui
                            .selectable_value(&mut format, TypingExportFormat::Psd, "PSD")
                            .clicked()
                    {
                        out.changed_export_format = Some(format);
                    }
                });
            }
            if self.preview_enabled && ui.button("Наложить и сохранить в папку").clicked()
            {
                let mut dialog = FileDialog::new();
                if let Some(path) = export_default_dir {
                    dialog = dialog.set_directory(path);
                }
                out.export_to_folder = dialog.pick_folder();
            }
            if self.preview_enabled {
                if ui.button("Вставить картинку из буфера обмена").clicked()
                {
                    out.create_image_request = Some(TypingCreateImageRequest::FromClipboard);
                }
                if ui.button("Выбрать картинку из файла").clicked() {
                    let mut dialog = FileDialog::new();
                    if let Some(path) = export_default_dir {
                        dialog = dialog.set_directory(path);
                    }
                    if let Some(path) = dialog
                        .add_filter("Картинки", &["png", "jpg", "jpeg", "webp", "bmp"])
                        .pick_file()
                    {
                        out.create_image_request = Some(TypingCreateImageRequest::FromFile(path));
                    }
                }
            }
            if self.preview_enabled {
                match export_status {
                    TypingExportUiStatus::Hidden => {}
                    TypingExportUiStatus::Running { done, total } => {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("Обработка страниц: {done}/{total}"));
                        });
                        let progress = if *total == 0 {
                            0.0
                        } else {
                            (*done as f32 / *total as f32).clamp(0.0, 1.0)
                        };
                        ui.add(
                            egui::ProgressBar::new(progress)
                                .desired_width(ui.available_width())
                                .show_percentage(),
                        );
                    }
                    TypingExportUiStatus::Success { done, total } => {
                        ui.add_space(4.0);
                        let text = format!("Готово: {done}/{total}");
                        let rich = egui::RichText::new(text).color(Color32::from_rgb(90, 230, 120));
                        ui.label(rich);
                        ui.add(
                            egui::ProgressBar::new(1.0)
                                .desired_width(ui.available_width())
                                .show_percentage()
                                .fill(Color32::from_rgb(90, 230, 120)),
                        );
                    }
                    TypingExportUiStatus::Error { message } => {
                        ui.add_space(4.0);
                        ui.colored_label(Color32::from_rgb(240, 110, 110), message);
                    }
                }
            }
            // Чекбокс видимости клина доступен в обоих режимах (создание и
            // редактирование); остальные действия ниже — только при создании.
            ui.separator();
            let mut show_clean = clean_overlays_visible;
            if ui.checkbox(&mut show_clean, "Показывать клин").changed() {
                out.changed_clean_overlays = Some(show_clean);
            }
            if self.preview_enabled {
                let mut strict_pixel_movement_value = strict_pixel_movement;
                if ui
                    .checkbox(
                        &mut strict_pixel_movement_value,
                        "Перемещение строго по пикселям",
                    )
                    .changed()
                {
                    out.changed_strict_pixel_movement = Some(strict_pixel_movement_value);
                }
                if ui.button("Округлить позиции текста").clicked() {
                    out.round_text_positions = true;
                }
            }
        });
        out
    }

    fn draw_main_text_params(
        &mut self,
        ui: &mut egui::Ui,
        stacked_columns: bool,
        remap_wheel_to_horizontal: bool,
        font_memory_enabled: bool,
        font_missing: bool,
    ) -> bool {
        let mut changed = false;
        let mut block_hscroll_by_hovered_param = false;
        let inline_selection = if self.preview_enabled {
            None
        } else {
            self.inline_selection_context()
        };
        let selection_mode = inline_selection.is_some();
        let mut inline_style = inline_selection
            .as_ref()
            .map(|selection| self.effective_inline_tag_style(selection));

        ui.vertical(|ui| {
            // Комбобокс группы шрифтов показывается на обеих панелях (создание и
            // редактирование); выбор синхронизируется между ними через
            // `pending_font_group_request` (см. обработку во внешнем цикле).
            {
                let mut selected_group_idx = self
                    .selected_font_group
                    .as_ref()
                    .and_then(|selected| {
                        self.font_groups.iter().position(|group| group == selected)
                    })
                    .map_or(0usize, |idx| idx + 1);
                let group_count = self.font_groups.len() + 1;
                let selected_group_text =
                    self.selected_font_group.as_deref().unwrap_or("Все группы");
                let group_combo = WheelComboBox::from_label("Группа шрифта")
                    .selected_text(selected_group_text)
                    .show_ui_with_wheel(ui, |ui| {
                        ui.selectable_value(&mut selected_group_idx, 0usize, "Все группы");
                        for (idx, group_name) in self.font_groups.iter().enumerate() {
                            ui.selectable_value(&mut selected_group_idx, idx + 1, group_name);
                        }
                    });
                mark_hscroll_block_on_hover(
                    &mut block_hscroll_by_hovered_param,
                    &group_combo.inner.response,
                );
                if let Some(steps) = group_combo.wheel_steps {
                    cycle_wrapped_index(&mut selected_group_idx, group_count, steps);
                }
                let previous_group = self.selected_font_group.clone();
                self.selected_font_group = if selected_group_idx == 0 {
                    None
                } else {
                    self.font_groups.get(selected_group_idx - 1).cloned()
                };
                if self.selected_font_group != previous_group {
                    self.ensure_selected_font_in_group();
                    self.pending_font_group_request = Some(self.selected_font_group.clone());
                    changed = true;
                }
            }

            let prev_font_idx = self.selected_font_idx;
            let filtered_font_indices = self.filtered_font_indices();
            let selected_font_text: String = if font_missing {
                // Шрифт оверлея не найден: показываем его имя, чтобы было понятно,
                // какой именно шрифт отсутствует и какой надо заменить.
                self.missing_font
                    .as_ref()
                    .map(|name| format!("{name} (не найден)"))
                    .unwrap_or_else(|| "<шрифт>".to_string())
            } else {
                inline_style
                    .as_ref()
                    .and_then(|style| style.font_label.clone())
                    .or_else(|| {
                        self.fonts
                            .get(self.selected_font_idx)
                            .map(|font| self.font_display_label(font))
                    })
                    .unwrap_or_else(|| "<шрифт>".to_string())
            };
            let mut font_idx = inline_style
                .as_ref()
                .and_then(|style| {
                    self.find_font_idx_by_path_or_label(None, style.font_label.as_deref())
                })
                .unwrap_or(self.selected_font_idx);
            if !filtered_font_indices.contains(&font_idx)
                && let Some(first_filtered_idx) = filtered_font_indices.first().copied()
            {
                font_idx = first_filtered_idx;
            }
            let font_combo = WheelComboBox::from_label("Шрифт")
                .selected_text(selected_font_text)
                .show_ui_with_wheel(ui, |ui| {
                    for idx in filtered_font_indices.iter().copied() {
                        let (label, path, face_index) = {
                            let font = &self.fonts[idx];
                            (
                                self.font_display_label(font),
                                font.path.clone(),
                                font.faces.first().map(|face| face.face_index).unwrap_or(0),
                            )
                        };
                        let selected = font_idx == idx;
                        if self.draw_font_combo_option(
                            ui,
                            &label,
                            path.as_path(),
                            face_index,
                            selected,
                        ) {
                            font_idx = idx;
                        }
                    }
                });
            mark_hscroll_block_on_hover(
                &mut block_hscroll_by_hovered_param,
                &font_combo.inner.response,
            );
            if let Some(steps) = font_combo.wheel_steps {
                cycle_wrapped_index_in_values(&mut font_idx, &filtered_font_indices, steps);
            }
            if let Some(style) = inline_style.as_mut() {
                if let Some(label) = self.font_label_by_idx(font_idx) {
                    style.font_label = Some(label);
                }
            } else {
                self.selected_font_idx = font_idx;
                if self.selected_font_idx != prev_font_idx {
                    // Любой выбор из списка — это доступный шрифт, поэтому снимаем
                    // блокировку рендера по ненайденному шрифту.
                    self.missing_font = None;
                    if font_memory_enabled {
                        changed |= self.handle_create_font_selection_change(prev_font_idx);
                    } else {
                        self.selected_face_idx = 0;
                        changed = true;
                    }
                }
            }

            if font_missing {
                ui.colored_label(
                    Color32::from_rgb(240, 200, 60),
                    "Выберите другой доступный шрифт, иначе рендер заблокирован",
                );
            }

            ui.add_enabled_ui(!selection_mode, |ui| {
                let prev_face_idx = self.selected_face_idx;
                let selected_face_text = self
                    .fonts
                    .get(self.selected_font_idx)
                    .and_then(|font| font.faces.get(self.selected_face_idx))
                    .map(|face| face.label.as_str())
                    .unwrap_or("<face>");
                let face_count = self
                    .fonts
                    .get(self.selected_font_idx)
                    .map(|font| font.faces.len())
                    .unwrap_or(0);
                let mut face_idx = self.selected_face_idx;
                let face_combo = WheelComboBox::from_label("Face")
                    .selected_text(selected_face_text)
                    .show_ui_with_wheel(ui, |ui| {
                        if let Some(font) = self.fonts.get(self.selected_font_idx) {
                            for (idx, face) in font.faces.iter().enumerate() {
                                ui.selectable_value(&mut face_idx, idx, &face.label);
                            }
                        }
                    });
                mark_hscroll_block_on_hover(
                    &mut block_hscroll_by_hovered_param,
                    &face_combo.inner.response,
                );
                if let Some(steps) = face_combo.wheel_steps {
                    cycle_wrapped_index(&mut face_idx, face_count, steps);
                }
                self.selected_face_idx = face_idx;
                if self.selected_face_idx != prev_face_idx {
                    changed = true;
                }

                let mut requested_use_system_fonts = self.use_system_fonts;
                let use_system_fonts_resp = ui.checkbox(
                    &mut requested_use_system_fonts,
                    "Использовать системные шрифты",
                );
                mark_hscroll_block_on_hover(
                    &mut block_hscroll_by_hovered_param,
                    &use_system_fonts_resp,
                );
                if use_system_fonts_resp.changed() {
                    self.pending_use_system_fonts_toggle_request = Some(requested_use_system_fonts);
                }
            });

            ui.add_space(4.0);

            let spacing_x = ui.spacing().item_spacing.x;
            let available_w = ui.available_width().max(1.0);
            let columns_w = (available_w - spacing_x).max(1.0);
            let left_ratio = 1.3 / 2.3;
            let min_left_w = 160.0;
            let min_right_w = 120.0;
            let mut left_w = columns_w * left_ratio;
            let mut right_w = columns_w - left_w;
            if columns_w >= (min_left_w + min_right_w) {
                if left_w < min_left_w {
                    left_w = min_left_w;
                    right_w = columns_w - left_w;
                }
                if right_w < min_right_w {
                    right_w = min_right_w;
                    left_w = columns_w - right_w;
                }
            }

            // Остальные параметры влияют на рендер: при ненайденном шрифте они
            // блокируются, доступным остаётся только выбор шрифта выше.
            ui.add_enabled_ui(!font_missing, |ui| {
                if stacked_columns {
                    ui.allocate_ui_with_layout(
                        Vec2::new(columns_w, 0.0),
                        egui::Layout::top_down(Align::Min),
                        |ui| {
                            self.draw_main_text_left_column(
                                ui,
                                &mut changed,
                                &mut block_hscroll_by_hovered_param,
                                inline_style.as_mut(),
                            )
                        },
                    );
                    ui.add_space(6.0);
                    ui.allocate_ui_with_layout(
                        Vec2::new(columns_w, 0.0),
                        egui::Layout::top_down(Align::Min),
                        |ui| {
                            self.draw_main_text_right_column(
                                ui,
                                &mut changed,
                                &mut block_hscroll_by_hovered_param,
                                inline_style.as_mut(),
                            )
                        },
                    );
                } else {
                    ui.horizontal_top(|ui| {
                        ui.allocate_ui_with_layout(
                            Vec2::new(left_w, 0.0),
                            egui::Layout::top_down(Align::Min),
                            |ui| {
                                self.draw_main_text_left_column(
                                    ui,
                                    &mut changed,
                                    &mut block_hscroll_by_hovered_param,
                                    inline_style.as_mut(),
                                )
                            },
                        );

                        ui.allocate_ui_with_layout(
                            Vec2::new(right_w, 0.0),
                            egui::Layout::top_down(Align::Min),
                            |ui| {
                                self.draw_main_text_right_column(
                                    ui,
                                    &mut changed,
                                    &mut block_hscroll_by_hovered_param,
                                    inline_style.as_mut(),
                                )
                            },
                        );
                    });
                }
            });

            // Extra bottom padding so the horizontal scrollbar doesn't overlap the last checkbox text.
            ui.add_space(ui.spacing().scroll.allocated_width() + 4.0);
        });

        if remap_wheel_to_horizontal {
            apply_horizontal_wheel_scroll_if_idle(ui, block_hscroll_by_hovered_param);
        } else if block_hscroll_by_hovered_param {
            consume_wheel_scroll_delta(ui);
        }
        if let (Some(selection), Some(style)) = (inline_selection, inline_style) {
            changed |= self.apply_inline_style_to_selection(selection, style);
        }
        changed
    }

    fn draw_inline_offset_controls(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        inline_style: Option<&mut TypingInlineTagStyle>,
    ) {
        let inline_font_size_px = inline_style
            .as_ref()
            .and_then(|style| style.font_size_px)
            .unwrap_or(self.font_size_px)
            .max(1.0);
        ui.add_enabled_ui(inline_style.is_some(), |ui| {
            let mut offset = inline_style
                .as_ref()
                .and_then(|style| style.glyph_offset)
                .unwrap_or_else(|| TypingInlineOffsetStyle::global_only([0.0, 0.0]));
            px_or_percent_param_row(
                ui,
                "Смещение X",
                &mut offset.global_x,
                -100.0..=100.0,
                1.0,
                inline_font_size_px,
                changed,
                block_hscroll_by_hovered_param,
            );
            px_or_percent_param_row(
                ui,
                "Смещение Y",
                &mut offset.global_y,
                -100.0..=100.0,
                1.0,
                inline_font_size_px,
                changed,
                block_hscroll_by_hovered_param,
            );
            px_or_percent_param_row(
                ui,
                "Смещение по линии",
                &mut offset.line,
                -300.0..=300.0,
                1.0,
                inline_font_size_px,
                changed,
                block_hscroll_by_hovered_param,
            );

            *changed |= ui
                .checkbox(&mut offset.shift_following, "Сдвигать следующие символы")
                .changed();

            let group_enabled = inline_style
                .as_ref()
                .is_some_and(|_| self.selected_inline_char_count() > 1);
            ui.add_enabled_ui(group_enabled, |ui| {
                let group_resp = ui.add(
                    WheelSlider::new(&mut offset.group_rotation_deg, -180.0..=180.0)
                        .text("Поворот группы")
                        .wheel_step(1.0),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &group_resp);
                *changed |= group_resp.changed();
                if let Some(steps) = wheel_steps_if_hovered(ui, &group_resp) {
                    *changed |= apply_wheel_step_f32(
                        &mut offset.group_rotation_deg,
                        steps,
                        1.0,
                        -180.0,
                        180.0,
                    );
                }
            });
            if !group_enabled {
                offset.group_rotation_deg = 0.0;
            }

            let glyph_resp = ui.add(
                WheelSlider::new(&mut offset.glyph_rotation_deg, -180.0..=180.0)
                    .text("Поворот символа")
                    .wheel_step(1.0),
            );
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &glyph_resp);
            *changed |= glyph_resp.changed();
            if let Some(steps) = wheel_steps_if_hovered(ui, &glyph_resp) {
                *changed |=
                    apply_wheel_step_f32(&mut offset.glyph_rotation_deg, steps, 1.0, -180.0, 180.0);
            }
            if let Some(style) = inline_style {
                style.glyph_offset = Some(offset);
            }
        });
    }

    fn selected_inline_char_count(&self) -> usize {
        self.text_selection_char_range
            .as_ref()
            .map(|range| range.end.saturating_sub(range.start))
            .unwrap_or(0)
    }

    fn draw_main_text_left_column(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        mut inline_style: Option<&mut TypingInlineTagStyle>,
    ) {
        let selection_mode = inline_style.is_some();
        if let Some(style) = inline_style.as_mut() {
            let mut text_color = style.text_color.unwrap_or(self.text_color);
            let color_resp = self.text_color_selector.draw(ui, &mut text_color);
            *changed |= color_resp.changed;
            style.text_color = Some(text_color);
            let mut font_size_px = style
                .font_size_px
                .unwrap_or(self.font_size_px)
                .clamp(1.0, 256.0);
            let font_size_resp = ui.add(
                WheelSlider::new(&mut font_size_px, 1.0..=256.0)
                    .text("Размер (px)")
                    .wheel_step(1.0),
            );
            *changed |= font_size_resp.changed();
            style.font_size_px = Some(font_size_px);
        } else {
            let color_resp = self.text_color_selector.draw(ui, &mut self.text_color);
            *changed |= color_resp.changed;
            let font_size_resp = ui.add(
                WheelSlider::new(&mut self.font_size_px, 1.0..=256.0)
                    .text("Размер (px)")
                    .wheel_step(1.0),
            );
            *changed |= font_size_resp.changed();
        }

        let base_font_size_px = self.font_size_px.max(1.0);
        if let Some(style) = inline_style.as_mut() {
            let inline_font_size_px = style.font_size_px.unwrap_or(base_font_size_px).max(1.0);
            let mut line_spacing = style.line_spacing.unwrap_or(self.line_spacing);
            px_or_percent_param_row(
                ui,
                "Межстрочный отступ",
                &mut line_spacing,
                -300.0..=300.0,
                2.0,
                inline_font_size_px,
                changed,
                block_hscroll_by_hovered_param,
            );
            style.line_spacing = Some(line_spacing);

            ui.horizontal(|ui| {
                ui.label("Кернинг");
                ui.add_enabled(
                    false,
                    egui::Button::new("Метрический")
                        .selected(self.kerning_mode == KerningMode::Metric),
                );
                ui.add_enabled(
                    false,
                    egui::Button::new("Оптический")
                        .selected(self.kerning_mode == KerningMode::Optical),
                );
            });
            let mut kerning = style.kerning.unwrap_or(self.kerning);
            px_or_percent_param_row(
                ui,
                "Кернинг",
                &mut kerning,
                -300.0..=300.0,
                2.0,
                inline_font_size_px,
                changed,
                block_hscroll_by_hovered_param,
            );
            style.kerning = Some(kerning);

            let mut stretching = style
                .glyph_stretching
                .unwrap_or([self.glyph_width, self.glyph_height]);
            px_or_percent_param_row(
                ui,
                "Высота символа",
                &mut stretching[1],
                1.0..=300.0,
                5.0,
                inline_font_size_px,
                changed,
                block_hscroll_by_hovered_param,
            );
            px_or_percent_param_row(
                ui,
                "Ширина символа",
                &mut stretching[0],
                1.0..=300.0,
                5.0,
                inline_font_size_px,
                changed,
                block_hscroll_by_hovered_param,
            );
            style.glyph_stretching = Some(stretching);
        } else {
            px_or_percent_param_row(
                ui,
                "Межстрочный отступ",
                &mut self.line_spacing,
                -300.0..=300.0,
                2.0,
                base_font_size_px,
                changed,
                block_hscroll_by_hovered_param,
            );

            ui.horizontal(|ui| {
                ui.label("Кернинг");
                *changed |= ui
                    .selectable_value(&mut self.kerning_mode, KerningMode::Metric, "Метрический")
                    .changed();
                *changed |= ui
                    .selectable_value(&mut self.kerning_mode, KerningMode::Optical, "Оптический")
                    .changed();
            });

            px_or_percent_param_row(
                ui,
                "Кернинг",
                &mut self.kerning,
                -300.0..=300.0,
                2.0,
                base_font_size_px,
                changed,
                block_hscroll_by_hovered_param,
            );

            px_or_percent_param_row(
                ui,
                "Высота символа",
                &mut self.glyph_height,
                1.0..=300.0,
                5.0,
                base_font_size_px,
                changed,
                block_hscroll_by_hovered_param,
            );

            px_or_percent_param_row(
                ui,
                "Ширина символа",
                &mut self.glyph_width,
                1.0..=300.0,
                5.0,
                base_font_size_px,
                changed,
                block_hscroll_by_hovered_param,
            );
        }

        if selection_mode {
            self.draw_inline_offset_controls(
                ui,
                changed,
                block_hscroll_by_hovered_param,
                inline_style,
            );
        }
    }

    /// Управление выравниванием на ОДНОЙ строке: слайдер лево↔право (`-100..100`),
    /// быстрые кнопки (⬅ влево / ⬇ по центру / ➡ вправо) и зажимаемая кнопка-тоггл
    /// ⬌ (justify, «Растягивать по ширине блока»). Слайдер и стрелки отключаются при
    /// включённом justify; кнопка ⬌ остаётся активной, чтобы его можно было выключить.
    fn draw_alignment_controls(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
    ) {
        let free_align = self.align.justify;
        ui.horizontal(|ui| {
            // Слайдер + стрелки отключаются при включённом justify.
            ui.add_enabled_ui(!free_align, |ui| {
                let mut bias_percent = (self.align.bias.clamp(-1.0, 1.0) * 100.0).round() as i32;
                let slider_resp = ui.add(
                    WheelSlider::new(&mut bias_percent, -100..=100)
                        .text("Выравнивание")
                        .wheel_step(5),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &slider_resp);
                if slider_resp.changed() {
                    self.align.bias = bias_percent as f32 / 100.0;
                    *changed = true;
                }

                if ui.button("⬅").on_hover_text("По левому краю").clicked() {
                    self.align.bias = -1.0;
                    *changed = true;
                }
                if ui.button("⬇").on_hover_text("По центру").clicked() {
                    self.align.bias = 0.0;
                    *changed = true;
                }
                if ui.button("➡").on_hover_text("По правому краю").clicked() {
                    self.align.bias = 1.0;
                    *changed = true;
                }
            });

            // Зажимаемая кнопка-тоггл justify — остаётся активной даже при включённом
            // justify, чтобы его можно было снять.
            if ui
                .add(egui::Button::new("⬌").selected(self.align.justify))
                .on_hover_text("Растягивать строки по ширине блока")
                .clicked()
            {
                self.align.justify = !self.align.justify;
                *changed = true;
            }
        });
    }

    fn draw_main_text_right_column(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        inline_style: Option<&mut TypingInlineTagStyle>,
    ) {
        let selection_mode = inline_style.is_some();
        ui.add_enabled_ui(!selection_mode, |ui| {
            self.draw_alignment_controls(ui, changed, block_hscroll_by_hovered_param);

            let prev_shape = self.text_shape;
            let shape_combo = WheelComboBox::from_label("Форма")
                .selected_text(match self.text_shape {
                    TextShape::Free => "Свободно",
                    TextShape::Rectangle => "[  ]",
                    TextShape::Oval => "(  )",
                    TextShape::Hexagon => "<  >",
                    TextShape::SoftPeak => "Мягкая",
                })
                .show_ui_with_wheel(ui, |ui| {
                    ui.selectable_value(&mut self.text_shape, TextShape::Free, "Свободно");
                    ui.selectable_value(&mut self.text_shape, TextShape::Rectangle, "[  ]");
                    ui.selectable_value(&mut self.text_shape, TextShape::Oval, "(  )");
                    ui.selectable_value(&mut self.text_shape, TextShape::Hexagon, "<  >");
                    ui.selectable_value(&mut self.text_shape, TextShape::SoftPeak, "Мягкая");
                });
            mark_hscroll_block_on_hover(
                block_hscroll_by_hovered_param,
                &shape_combo.inner.response,
            );
            if let Some(steps) = shape_combo.wheel_steps {
                *changed |= cycle_text_shape(&mut self.text_shape, steps);
            }
            if self.text_shape != prev_shape {
                *changed = true;
            }

            let prev_wrap_mode = self.text_wrap_mode;
            let wrap_combo = WheelComboBox::from_label("Перенос")
                .selected_text(text_wrap_mode_label(self.text_wrap_mode))
                .show_ui_with_wheel(ui, |ui| {
                    ui.selectable_value(
                        &mut self.text_wrap_mode,
                        TextWrapMode::None,
                        text_wrap_mode_label(TextWrapMode::None),
                    );
                    ui.selectable_value(
                        &mut self.text_wrap_mode,
                        TextWrapMode::WholeWords,
                        text_wrap_mode_label(TextWrapMode::WholeWords),
                    );
                    ui.selectable_value(
                        &mut self.text_wrap_mode,
                        TextWrapMode::Minimal,
                        text_wrap_mode_label(TextWrapMode::Minimal),
                    );
                    ui.selectable_value(
                        &mut self.text_wrap_mode,
                        TextWrapMode::Moderate,
                        text_wrap_mode_label(TextWrapMode::Moderate),
                    );
                    ui.selectable_value(
                        &mut self.text_wrap_mode,
                        TextWrapMode::Aggressive,
                        text_wrap_mode_label(TextWrapMode::Aggressive),
                    );
                });
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &wrap_combo.inner.response);
            if let Some(steps) = wrap_combo.wheel_steps {
                *changed |= cycle_text_wrap_mode(&mut self.text_wrap_mode, steps);
            }
            if self.text_wrap_mode != prev_wrap_mode {
                self.sync_wrap_mode_constraints();
                *changed = true;
            }
            let moderate_trees_resp = ui.add_enabled(
                self.moderate_trees_checkbox_enabled(),
                egui::Checkbox::new(&mut self.allow_moderate_trees, "Разрешить умеренные ёлки"),
            );
            *changed |= moderate_trees_resp.changed();

            if matches!(self.text_shape, TextShape::Oval | TextShape::Hexagon) {
                let min_width_resp = ui.add(
                    WheelSlider::new(&mut self.shape_min_width_percent, 5.0..=100.0)
                        .text("Минимальная ширина (%)"),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &min_width_resp);
                *changed |= min_width_resp.changed();
                if let Some(steps) = wheel_steps_if_hovered(ui, &min_width_resp) {
                    *changed |= apply_wheel_step_f32(
                        &mut self.shape_min_width_percent,
                        steps,
                        1.0,
                        5.0,
                        100.0,
                    );
                }
            }
            if self.text_shape == TextShape::SoftPeak {
                let variant_resp =
                    ui.add(WheelSlider::new(&mut self.shape_variant, 1..=9).text("Вариант формы"));
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &variant_resp);
                *changed |= variant_resp.changed();
                if let Some(steps) = wheel_steps_if_hovered(ui, &variant_resp) {
                    *changed |= apply_wheel_step_u8(&mut self.shape_variant, steps, 1, 1, 9);
                }
            }
        });
        if let Some(style) = inline_style {
            let mut bold = style.bold;
            let force_bold_resp = ui.checkbox(&mut bold, "Bold");
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &force_bold_resp);
            *changed |= force_bold_resp.changed();
            style.bold = bold;

            let mut italic = style.italic;
            let force_italic_resp = ui.checkbox(&mut italic, "Italic");
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &force_italic_resp);
            *changed |= force_italic_resp.changed();
            style.italic = italic;
        } else {
            let force_bold_resp = ui.checkbox(&mut self.force_bold, "Bold");
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &force_bold_resp);
            *changed |= force_bold_resp.changed();
            let force_italic_resp = ui.checkbox(&mut self.force_italic, "Italic");
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &force_italic_resp);
            *changed |= force_italic_resp.changed();
        }
        ui.add_enabled_ui(!selection_mode, |ui| {
            let hanging_punct_resp =
                ui.checkbox(&mut self.hanging_punctuation, "Висящая пунктуация");
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &hanging_punct_resp);
            *changed |= hanging_punct_resp.changed();
            let trim_spaces_resp =
                ui.checkbox(&mut self.trim_extra_spaces, "Удалять лишние пробелы");
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &trim_spaces_resp);
            *changed |= trim_spaces_resp.changed();
            let sentence_nl_resp = ui.checkbox(
                &mut self.new_line_after_sentence,
                "Новая строка после конца предложения",
            );
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &sentence_nl_resp);
            *changed |= sentence_nl_resp.changed();
            let uppercase_text_resp =
                ui.checkbox(&mut self.uppercase_text, "Всё в верхнем регистре");
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &uppercase_text_resp);
            *changed |= uppercase_text_resp.changed();
            let inline_tags_resp = ui.checkbox(
                &mut self.enable_inline_style_tags,
                "Парсить теги <b>/<i> в тексте",
            );
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &inline_tags_resp);
            *changed |= inline_tags_resp.changed();

            self.draw_advanced_text_params_section(
                ui,
                changed,
                block_hscroll_by_hovered_param,
                "typing_advanced_text_params_right_column",
            );
        });
    }

    fn draw_advanced_text_params_section(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        id_salt: &'static str,
    ) {
        ui.add_space(6.0);
        egui::CollapsingHeader::new("Расширенные параметры")
            .id_salt((id_salt, self.preview_enabled))
            .default_open(false)
            .show(ui, |ui| {
                let prev_mode = self.text_line_mode;
                let line_mode_combo = WheelComboBox::from_label("Строка")
                    .selected_text(match self.text_line_mode {
                        TextLineMode::Horizontal => "Горизонтальная",
                        TextLineMode::Vertical => "Вертикальная",
                    })
                    .show_ui_with_wheel(ui, |ui| {
                        ui.selectable_value(
                            &mut self.text_line_mode,
                            TextLineMode::Horizontal,
                            "Горизонтальная",
                        );
                        ui.selectable_value(
                            &mut self.text_line_mode,
                            TextLineMode::Vertical,
                            "Вертикальная",
                        );
                    });
                mark_hscroll_block_on_hover(
                    block_hscroll_by_hovered_param,
                    &line_mode_combo.inner.response,
                );
                if let Some(steps) = line_mode_combo.wheel_steps {
                    *changed |= cycle_text_line_mode(&mut self.text_line_mode, steps);
                }
                if self.text_line_mode != prev_mode {
                    *changed = true;
                }
                if self.text_line_mode == TextLineMode::Vertical {
                    let prev_direction = self.vertical_line_direction;
                    let direction_combo = WheelComboBox::from_label("Расположение строк")
                        .selected_text(match self.vertical_line_direction {
                            VerticalLineDirection::LeftToRight => "Слева направо",
                            VerticalLineDirection::RightToLeft => "Справа налево",
                        })
                        .show_ui_with_wheel(ui, |ui| {
                            ui.selectable_value(
                                &mut self.vertical_line_direction,
                                VerticalLineDirection::LeftToRight,
                                "Слева направо",
                            );
                            ui.selectable_value(
                                &mut self.vertical_line_direction,
                                VerticalLineDirection::RightToLeft,
                                "Справа налево",
                            );
                        });
                    mark_hscroll_block_on_hover(
                        block_hscroll_by_hovered_param,
                        &direction_combo.inner.response,
                    );
                    if let Some(steps) = direction_combo.wheel_steps {
                        *changed |=
                            cycle_vertical_line_direction(&mut self.vertical_line_direction, steps);
                    }
                    if self.vertical_line_direction != prev_direction {
                        *changed = true;
                    }
                }

                let prev_layout_mode = self.text_layout_mode;
                let layout_mode_combo = WheelComboBox::from_label("Раскладка")
                    .selected_text(match self.text_layout_mode {
                        TextLayoutMode::Normal => "Стандартный",
                        TextLayoutMode::Formula => "Формула",
                        TextLayoutMode::Shape => "Форма",
                        TextLayoutMode::CustomRasterLines => "Кастомный: векторные линии",
                        TextLayoutMode::CustomVectorLines => "Кастомный: векторные линии",
                    })
                    .show_ui_with_wheel(ui, |ui| {
                        ui.selectable_value(
                            &mut self.text_layout_mode,
                            TextLayoutMode::Normal,
                            "Стандартный",
                        );
                        ui.selectable_value(
                            &mut self.text_layout_mode,
                            TextLayoutMode::Formula,
                            "Формула",
                        );
                        ui.selectable_value(
                            &mut self.text_layout_mode,
                            TextLayoutMode::CustomVectorLines,
                            "Кастомный: векторные линии",
                        );
                    });
                mark_hscroll_block_on_hover(
                    block_hscroll_by_hovered_param,
                    &layout_mode_combo.inner.response,
                );
                if let Some(steps) = layout_mode_combo.wheel_steps {
                    *changed |= cycle_text_layout_mode(&mut self.text_layout_mode, steps);
                }
                if self.text_layout_mode != prev_layout_mode {
                    *changed = true;
                }

                match self.text_layout_mode {
                    TextLayoutMode::Normal => {}
                    TextLayoutMode::Formula => {
                        self.draw_formula_layout_controls(
                            ui,
                            changed,
                            block_hscroll_by_hovered_param,
                        );
                    }
                    TextLayoutMode::Shape => {
                        self.draw_shape_layout_controls(
                            ui,
                            changed,
                            block_hscroll_by_hovered_param,
                        );
                    }
                    TextLayoutMode::CustomRasterLines => {}
                    TextLayoutMode::CustomVectorLines => {
                        ui.add_space(4.0);
                        ui.label(
                            "Для управления векторной кастомной раскладкой войдите в этот режим через меню ЛКМ",
                        );
                    }
                }
            });
    }

    fn draw_formula_layout_controls(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
    ) {
        ui.add_space(4.0);
        let mut formula_direct_edit_changed = false;
        ui.horizontal(|ui| {
            ui.label("Пресет:");
            let mut names: Vec<String> = self.formula_presets_by_name.keys().cloned().collect();
            names.sort();
            let prev_selected = self.selected_formula_preset_name.clone();
            let selected_text = self
                .selected_formula_preset_name
                .as_deref()
                .unwrap_or(TEXT_PRESET_NONE_LABEL);
            let preset_len = names.len() + 1;
            let mut preset_idx = self
                .selected_formula_preset_name
                .as_ref()
                .and_then(|selected| names.iter().position(|name| name == selected))
                .map(|idx| idx + 1)
                .unwrap_or(0);
            let combo_resp =
                WheelComboBox::from_id_salt(("typing_formula_preset_combo", self.preview_enabled))
                    .selected_text(selected_text)
                    .show_ui_with_wheel(ui, |ui| {
                        if ui
                            .selectable_label(preset_idx == 0, TEXT_PRESET_NONE_LABEL)
                            .clicked()
                        {
                            preset_idx = 0;
                        }
                        for (idx, name) in names.iter().enumerate() {
                            if ui.selectable_label(preset_idx == idx + 1, name).clicked() {
                                preset_idx = idx + 1;
                            }
                        }
                    });
            if let Some(steps) = combo_resp.wheel_steps {
                cycle_wrapped_index(&mut preset_idx, preset_len, steps);
            }
            self.selected_formula_preset_name = if preset_idx == 0 {
                None
            } else {
                names.get(preset_idx - 1).cloned()
            };
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &combo_resp.inner.response);
            if self.selected_formula_preset_name != prev_selected
                && let Some(name) = self.selected_formula_preset_name.clone()
                && self.apply_formula_preset_by_name(name)
            {
                *changed = true;
            }
        });
        ui.horizontal(|ui| {
            let preset_name_resp = ui.add(
                egui::TextEdit::singleline(&mut self.formula_preset_name_input)
                    .id_salt(("typing_formula_preset_name_input", self.preview_enabled))
                    .hint_text("Сохранить пресет")
                    .desired_width((ui.available_width() - 96.0).max(120.0)),
            );
            self.track_text_input(&preset_name_resp);
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &preset_name_resp);
            if ui.button("Сохранить").clicked() {
                self.save_current_formula_preset();
            }
        });

        ui.horizontal(|ui| {
            ui.label("Формула:");
            let x_resp = ui.add(
                egui::TextEdit::singleline(&mut self.formula_layout.x_expr)
                    .hint_text("x(t, ...)")
                    .desired_width(150.0),
            );
            self.track_text_input(&x_resp);
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &x_resp);
            formula_direct_edit_changed |= x_resp.changed();
            *changed |= x_resp.changed();

            let swap_resp = ui
                .small_button("⇄")
                .on_hover_text("Поменять выражения X и Y местами.");
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &swap_resp);
            if swap_resp.clicked() {
                self.swap_formula_xy_expressions();
                formula_direct_edit_changed = true;
                *changed = true;
            }

            let y_resp = ui.add(
                egui::TextEdit::singleline(&mut self.formula_layout.y_expr)
                    .hint_text("y(t, ...)")
                    .desired_width(150.0),
            );
            self.track_text_input(&y_resp);
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &y_resp);
            formula_direct_edit_changed |= y_resp.changed();
            *changed |= y_resp.changed();
        });

        ui.horizontal(|ui| {
            ui.label("rotation:");
            let rot_resp = ui.add(
                egui::TextEdit::singleline(&mut self.formula_layout.rotation_expr)
                    .hint_text("rot (rad)")
                    .desired_width(110.0),
            );
            self.track_text_input(&rot_resp);
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &rot_resp);
            formula_direct_edit_changed |= rot_resp.changed();
            *changed |= rot_resp.changed();

            if ui.small_button("?").clicked() {
                self.formula_help_open = !self.formula_help_open;
            }
        });

        if self.formula_help_open {
            ui.label("Переменные: t/u/i/n/s/line/line_t/line_n/w/fs/a..h/pi/tau/math_e");
            ui.label("Функции: sin cos tan asin acos atan atan2 sqrt abs exp ln log min max clamp pow rad deg floor ceil round sign.");
            ui.label("`t` пробегает диапазон [t_start..t_end], `rot` задаётся в радианах.");
            ui.label("Символы теперь раскладываются по длине кривой: короткая строка центрируется на участке, длинная сжимается в его длину.");
        }

        let tangent_resp = ui.checkbox(
            &mut self.formula_layout.use_tangent_rotation,
            "Поворот по касательной",
        );
        mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &tangent_resp);
        formula_direct_edit_changed |= tangent_resp.changed();
        *changed |= tangent_resp.changed();

        ui.horizontal(|ui| {
            let t_start_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.t_start)
                    .speed(0.01)
                    .prefix("Старт t "),
            );
            let t_start_resp =
                t_start_resp.on_hover_text("Начало диапазона параметра t для формулы.");
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &t_start_resp);
            formula_direct_edit_changed |= t_start_resp.changed();
            *changed |= t_start_resp.changed();
            let t_end_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.t_end)
                    .speed(0.01)
                    .prefix("Конец t "),
            );
            let t_end_resp = t_end_resp.on_hover_text("Конец диапазона параметра t для формулы.");
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &t_end_resp);
            formula_direct_edit_changed |= t_end_resp.changed();
            *changed |= t_end_resp.changed();
        });
        ui.horizontal(|ui| {
            let offset_x_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.offset_x_px)
                    .speed(1.0)
                    .prefix("Сдвиг X "),
            );
            let offset_x_resp =
                offset_x_resp.on_hover_text("Сдвиг всей траектории по горизонтали в пикселях.");
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &offset_x_resp);
            formula_direct_edit_changed |= offset_x_resp.changed();
            *changed |= offset_x_resp.changed();
            let offset_y_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.offset_y_px)
                    .speed(1.0)
                    .prefix("Сдвиг Y "),
            );
            let offset_y_resp =
                offset_y_resp.on_hover_text("Сдвиг всей траектории по вертикали в пикселях.");
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &offset_y_resp);
            formula_direct_edit_changed |= offset_y_resp.changed();
            *changed |= offset_y_resp.changed();
        });
        ui.horizontal(|ui| {
            let scale_x_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.scale_x)
                    .speed(0.01)
                    .prefix("Масштаб X "),
            );
            let scale_x_resp = scale_x_resp.on_hover_text("Масштабирует формулу по оси X.");
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &scale_x_resp);
            formula_direct_edit_changed |= scale_x_resp.changed();
            *changed |= scale_x_resp.changed();
            let scale_y_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.scale_y)
                    .speed(0.01)
                    .prefix("Масштаб Y "),
            );
            let scale_y_resp = scale_y_resp.on_hover_text("Масштабирует формулу по оси Y.");
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &scale_y_resp);
            formula_direct_edit_changed |= scale_y_resp.changed();
            *changed |= scale_y_resp.changed();
        });
        self.draw_formula_spacing_controls(
            ui,
            changed,
            block_hscroll_by_hovered_param,
            &mut formula_direct_edit_changed,
        );

        ui.label("Константы формулы (a..h):");
        egui::Grid::new(("typing_formula_vars_grid", self.preview_enabled)).show(ui, |ui| {
            for idx in 0..TEXT_FORMULA_USER_VAR_COUNT {
                ui.label(format!("{} =", (b'a' + idx as u8) as char));
                let resp = ui.add(
                    WheelSpinBox::new(&mut self.formula_layout.vars[idx])
                        .speed(0.05)
                        .range(-100000.0..=100000.0),
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &resp);
                formula_direct_edit_changed |= resp.changed();
                *changed |= resp.changed();
                if idx % 2 == 1 {
                    ui.end_row();
                }
            }
        });
        if formula_direct_edit_changed {
            self.selected_formula_preset_name = None;
        }
    }

    fn draw_shape_layout_controls(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
    ) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Форма:");
            let prev_kind = self.shape_layout_kind;
            let mut kind_idx = match self.shape_layout_kind {
                TypingShapeLayoutKind::Arc => 0,
                TypingShapeLayoutKind::Circle => 1,
                TypingShapeLayoutKind::Spiral => 2,
                TypingShapeLayoutKind::Polygon => 3,
                TypingShapeLayoutKind::Zigzag => 4,
                TypingShapeLayoutKind::SCurve => 5,
            };
            let combo_resp =
                WheelComboBox::from_id_salt(("typing_shape_layout_kind", self.preview_enabled))
                    .selected_text(match self.shape_layout_kind {
                        TypingShapeLayoutKind::Arc => "Дуга",
                        TypingShapeLayoutKind::Circle => "Круг / эллипс",
                        TypingShapeLayoutKind::Spiral => "Спираль",
                        TypingShapeLayoutKind::Polygon => "Многоугольник",
                        TypingShapeLayoutKind::Zigzag => "Зигзаг",
                        TypingShapeLayoutKind::SCurve => "S-кривая",
                    })
                    .show_ui_with_wheel(ui, |ui| {
                        for (idx, label) in [
                            "Дуга",
                            "Круг / эллипс",
                            "Спираль",
                            "Многоугольник",
                            "Зигзаг",
                            "S-кривая",
                        ]
                        .iter()
                        .enumerate()
                        {
                            if ui.selectable_label(kind_idx == idx, *label).clicked() {
                                kind_idx = idx;
                            }
                        }
                    });
            if let Some(steps) = combo_resp.wheel_steps {
                cycle_wrapped_index(&mut kind_idx, 6, steps);
            }
            self.shape_layout_kind = match kind_idx {
                0 => TypingShapeLayoutKind::Arc,
                1 => TypingShapeLayoutKind::Circle,
                2 => TypingShapeLayoutKind::Spiral,
                3 => TypingShapeLayoutKind::Polygon,
                4 => TypingShapeLayoutKind::Zigzag,
                _ => TypingShapeLayoutKind::SCurve,
            };
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &combo_resp.inner.response);
            if self.shape_layout_kind != prev_kind {
                *changed = true;
            }
        });

        match self.shape_layout_kind {
            TypingShapeLayoutKind::Arc => {
                ui.horizontal(|ui| {
                    ui.label("Ориентация:");
                    let prev_orientation = self.arc_shape_layout.orientation;
                    let mut orientation_idx = match self.arc_shape_layout.orientation {
                        TypingArcOrientation::Horizontal => 0,
                        TypingArcOrientation::Vertical => 1,
                    };
                    let combo_resp = WheelComboBox::from_id_salt((
                        "typing_arc_shape_orientation",
                        self.preview_enabled,
                    ))
                    .selected_text(self.arc_shape_layout.orientation.label())
                    .show_ui_with_wheel(ui, |ui| {
                        for (idx, orientation) in [
                            TypingArcOrientation::Horizontal,
                            TypingArcOrientation::Vertical,
                        ]
                        .iter()
                        .enumerate()
                        {
                            if ui
                                .selectable_label(orientation_idx == idx, orientation.label())
                                .clicked()
                            {
                                orientation_idx = idx;
                            }
                        }
                    });
                    if let Some(steps) = combo_resp.wheel_steps {
                        cycle_wrapped_index(&mut orientation_idx, 2, steps);
                    }
                    self.arc_shape_layout.orientation = match orientation_idx {
                        0 => TypingArcOrientation::Horizontal,
                        _ => TypingArcOrientation::Vertical,
                    };
                    mark_hscroll_block_on_hover(
                        block_hscroll_by_hovered_param,
                        &combo_resp.inner.response,
                    );
                    if self.arc_shape_layout.orientation != prev_orientation {
                        *changed = true;
                    }
                });

                let width_resp = ui.add(
                    WheelSlider::new(&mut self.arc_shape_layout.length_px, 32.0..=2000.0)
                        .text("Длина"),
                );
                let width_resp =
                    width_resp.on_hover_text("Длина дуги по основной оси раскладки текста.");
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &width_resp);
                *changed |= width_resp.changed();

                let height_resp = ui.add(
                    WheelSlider::new(&mut self.arc_shape_layout.amplitude_px, -800.0..=800.0)
                        .text("Амплитуда"),
                );
                let height_resp = height_resp.on_hover_text(
                    "Насколько дуга отклоняется от основной оси. Отрицательное значение переворачивает форму.",
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &height_resp);
                *changed |= height_resp.changed();

                let freq_resp = ui.add(
                    WheelSlider::new(&mut self.arc_shape_layout.frequency, 0.25..=6.0)
                        .text("Частота"),
                );
                let freq_resp = freq_resp.on_hover_text(
                    "Сколько полуволн укладывается по ширине. 1.0 даёт обычную дугу, больше 1.0 превращает её в волну.",
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &freq_resp);
                *changed |= freq_resp.changed();
            }
            TypingShapeLayoutKind::Circle => {
                let width_resp = ui.add(
                    WheelSlider::new(&mut self.circle_shape_layout.width_px, 32.0..=2000.0)
                        .text("Ширина"),
                );
                let width_resp =
                    width_resp.on_hover_text("Горизонтальный диаметр круга или эллипса.");
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &width_resp);
                *changed |= width_resp.changed();

                let height_resp = ui.add(
                    WheelSlider::new(&mut self.circle_shape_layout.height_px, 32.0..=2000.0)
                        .text("Высота"),
                );
                let height_resp = height_resp
                    .on_hover_text("Вертикальный диаметр. Если равен ширине, получится круг.");
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &height_resp);
                *changed |= height_resp.changed();
            }
            TypingShapeLayoutKind::Spiral => {
                let width_resp = ui.add(
                    WheelSlider::new(&mut self.spiral_shape_layout.width_px, 32.0..=2000.0)
                        .text("Ширина"),
                );
                let width_resp =
                    width_resp.on_hover_text("Внешний диаметр спирали по горизонтали.");
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &width_resp);
                *changed |= width_resp.changed();

                let height_resp = ui.add(
                    WheelSlider::new(&mut self.spiral_shape_layout.height_px, 32.0..=2000.0)
                        .text("Высота"),
                );
                let height_resp =
                    height_resp.on_hover_text("Внешний диаметр спирали по вертикали.");
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &height_resp);
                *changed |= height_resp.changed();

                let turns_resp = ui.add(
                    WheelSlider::new(&mut self.spiral_shape_layout.turns, 0.25..=8.0)
                        .text("Обороты"),
                );
                let turns_resp =
                    turns_resp.on_hover_text("Сколько витков проходит текст от центра к краю.");
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &turns_resp);
                *changed |= turns_resp.changed();

                let inner_resp = ui.add(
                    WheelSlider::new(&mut self.spiral_shape_layout.inner_ratio, 0.0..=0.95)
                        .text("Внутр. радиус"),
                );
                let inner_resp =
                    inner_resp.on_hover_text("Насколько большой зазор оставлять в центре спирали.");
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &inner_resp);
                *changed |= inner_resp.changed();
            }
            TypingShapeLayoutKind::Polygon => {
                let width_resp = ui.add(
                    WheelSlider::new(&mut self.polygon_shape_layout.width_px, 32.0..=2000.0)
                        .text("Ширина"),
                );
                let width_resp = width_resp.on_hover_text("Горизонтальный размер многоугольника.");
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &width_resp);
                *changed |= width_resp.changed();

                let height_resp = ui.add(
                    WheelSlider::new(&mut self.polygon_shape_layout.height_px, 32.0..=2000.0)
                        .text("Высота"),
                );
                let height_resp = height_resp.on_hover_text("Вертикальный размер многоугольника.");
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &height_resp);
                *changed |= height_resp.changed();

                let sides_resp = ui.add(
                    WheelSlider::new(&mut self.polygon_shape_layout.sides, 3..=12).text("Стороны"),
                );
                let sides_resp =
                    sides_resp.on_hover_text("Количество сторон у регулярного многоугольника.");
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &sides_resp);
                *changed |= sides_resp.changed();
            }
            TypingShapeLayoutKind::Zigzag => {
                let width_resp = ui.add(
                    WheelSlider::new(&mut self.zigzag_shape_layout.width_px, 32.0..=2000.0)
                        .text("Ширина"),
                );
                let width_resp = width_resp.on_hover_text("Длина зигзага по горизонтали.");
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &width_resp);
                *changed |= width_resp.changed();

                let height_resp = ui.add(
                    WheelSlider::new(&mut self.zigzag_shape_layout.height_px, -800.0..=800.0)
                        .text("Высота"),
                );
                let height_resp = height_resp.on_hover_text(
                    "Амплитуда зубцов. Отрицательное значение переворачивает зигзаг.",
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &height_resp);
                *changed |= height_resp.changed();

                let segments_resp = ui.add(
                    WheelSlider::new(&mut self.zigzag_shape_layout.segments, 0.5..=12.0)
                        .text("Сегменты"),
                );
                let segments_resp =
                    segments_resp.on_hover_text("Сколько зубцов поместится по ширине.");
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &segments_resp);
                *changed |= segments_resp.changed();
            }
            TypingShapeLayoutKind::SCurve => {
                let width_resp = ui.add(
                    WheelSlider::new(&mut self.s_curve_shape_layout.width_px, 32.0..=2000.0)
                        .text("Ширина"),
                );
                let width_resp = width_resp.on_hover_text("Длина S-кривой по горизонтали.");
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &width_resp);
                *changed |= width_resp.changed();

                let height_resp = ui.add(
                    WheelSlider::new(&mut self.s_curve_shape_layout.height_px, -800.0..=800.0)
                        .text("Высота"),
                );
                let height_resp = height_resp.on_hover_text(
                    "Амплитуда S-кривой. Отрицательное значение переворачивает форму.",
                );
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &height_resp);
                *changed |= height_resp.changed();

                let bends_resp = ui.add(
                    WheelSlider::new(&mut self.s_curve_shape_layout.bends, 0.5..=4.0)
                        .text("Изгибы"),
                );
                let bends_resp = bends_resp.on_hover_text("Сколько S-петель проходит по ширине.");
                mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &bends_resp);
                *changed |= bends_resp.changed();
            }
        }

        let mut shape_changed = false;
        let tangent_resp = ui.checkbox(
            &mut self.formula_layout.use_tangent_rotation,
            "Поворот по касательной",
        );
        mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &tangent_resp);
        shape_changed |= tangent_resp.changed();
        *changed |= tangent_resp.changed();
        self.draw_formula_spacing_controls(
            ui,
            changed,
            block_hscroll_by_hovered_param,
            &mut shape_changed,
        );
    }

    fn draw_formula_spacing_controls(
        &mut self,
        ui: &mut egui::Ui,
        changed: &mut bool,
        block_hscroll_by_hovered_param: &mut bool,
        local_changed: &mut bool,
    ) {
        ui.horizontal(|ui| {
            let normal_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.normal_offset_px)
                    .speed(0.5)
                    .prefix("Отступ "),
            );
            let normal_resp = normal_resp.on_hover_text(
                "Сдвиг текста по нормали к линии: наружу или внутрь относительно траектории.",
            );
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &normal_resp);
            *local_changed |= normal_resp.changed();
            *changed |= normal_resp.changed();
            let spacing_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.letter_spacing_mul)
                    .range(0.0..=8.0)
                    .speed(0.01)
                    .prefix("Трекинг "),
            );
            let spacing_resp = spacing_resp
                .on_hover_text("Множитель реального шага между символами вдоль линии формулы.");
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &spacing_resp);
            *local_changed |= spacing_resp.changed();
            *changed |= spacing_resp.changed();
        });
        ui.horizontal(|ui| {
            let spacing_px_resp = ui.add(
                WheelSpinBox::new(&mut self.formula_layout.letter_spacing_px)
                    .speed(0.25)
                    .range(-1000.0..=1000.0)
                    .prefix("Интервал "),
            );
            let spacing_px_resp = spacing_px_resp.on_hover_text(
                "Дополнительное расстояние в пикселях, прибавляется к шагу между символами после tracking.",
            );
            mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &spacing_px_resp);
            *local_changed |= spacing_px_resp.changed();
            *changed |= spacing_px_resp.changed();
        });
    }

    /// Конкурирующий аккордеон «Изначальный текст» / «Сформированный текст»:
    /// развёрнут ровно один. Без сформированного текста развёрнут исходный.
    /// Возвращает `true`, если что-то изменилось.
    fn draw_text_accordion(
        &mut self,
        ui: &mut egui::Ui,
        id_suffix: &str,
        block_hscroll: &mut bool,
    ) -> bool {
        let mut changed = false;
        // Без сформированного текста всегда развёрнут исходный.
        if self.formed_text.trim().is_empty() {
            self.advanced_text_show_formed = false;
        }
        let show_formed = self.advanced_text_show_formed;

        // Заголовок «Изначальный текст»: ▼ если развёрнут, ◀ если свёрнут.
        let source_arrow = if show_formed { "◀" } else { "▼" };
        if ui
            .selectable_label(!show_formed, format!("Изначальный текст {source_arrow}"))
            .clicked()
            && show_formed
        {
            // Переключение пана: старое выделение относилось к другому буферу.
            self.clear_inline_text_selection();
            self.advanced_text_show_formed = false;
        }
        if !show_formed {
            self.inline_text_target = InlineTextTarget::Source;
            let text_colors = build_inline_tag_editor_text_colors(&self.text);
            let text_output = TextEditPlus::multiline(&mut self.text)
                .id_salt(format!("typing_edit_text_source_{id_suffix}"))
                .desired_width(f32::INFINITY)
                .min_size(egui::vec2(ui.available_width(), EDIT_TEXT_FIELD_HEIGHT_PX))
                .text_colors(text_colors)
                .show(ui);
            self.paint_persistent_text_selection_if_needed(ui, &text_output);
            self.track_text_input(&text_output.response);
            self.sync_text_selection_from_text_edit(
                ui.ctx(),
                text_output.response.id,
                &text_output.response,
                text_output.cursor_range,
            );
            mark_hscroll_block_on_hover(block_hscroll, &text_output.response);
            changed |= text_output.response.changed();
        }

        // Сформированный текст раскрывается НАД своим заголовком (поэтому ▲).
        if show_formed {
            self.inline_text_target = InlineTextTarget::Formed;
            let text_colors = build_inline_tag_editor_text_colors(&self.formed_text);
            let formed_output = TextEditPlus::multiline(&mut self.formed_text)
                .id_salt(format!("typing_edit_text_formed_{id_suffix}"))
                .desired_width(f32::INFINITY)
                .min_size(egui::vec2(ui.available_width(), EDIT_TEXT_FIELD_HEIGHT_PX))
                .text_colors(text_colors)
                .show(ui);
            self.paint_persistent_text_selection_if_needed(ui, &formed_output);
            self.track_text_input(&formed_output.response);
            self.sync_text_selection_from_text_edit(
                ui.ctx(),
                formed_output.response.id,
                &formed_output.response,
                formed_output.cursor_range,
            );
            mark_hscroll_block_on_hover(block_hscroll, &formed_output.response);
            changed |= formed_output.response.changed();
        }

        // Заголовок «Сформированный текст»: ▲ если развёрнут (поле над ним), ◀ если свёрнут.
        let formed_arrow = if show_formed { "▲" } else { "◀" };
        if ui
            .selectable_label(show_formed, format!("Сформированный текст {formed_arrow}"))
            .clicked()
            && !show_formed
            && !self.formed_text.trim().is_empty()
        {
            // Переключение пана: старое выделение относилось к другому буферу.
            self.clear_inline_text_selection();
            self.advanced_text_show_formed = true;
        }

        ui.add_space(6.0);
        changed |= self.draw_advanced_form_buttons(ui);
        changed
    }

    /// Кнопки «Продвинутая форма текста» и «Вернуть исходный» под полем текста.
    fn draw_advanced_form_buttons(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            if ui.button("Продвинутая форма текста").clicked() {
                self.advanced_form_open = true;
                self.advanced_form_cache = None;
                self.advanced_form_centered = false;
            }
            // «Вернуть исходный» просто очищает сформированный текст и
            // разворачивает исходный.
            let has_formed = !self.formed_text.is_empty();
            let revert = ui.add_enabled(has_formed, egui::Button::new("Вернуть исходный"));
            if revert.clicked() {
                self.formed_text.clear();
                self.advanced_text_show_formed = false;
                self.queue_preview_render();
                changed = true;
            }
        });
        changed
    }

    /// Шрифт для отображения форм (тот же, что выбран в панели), или дефолтный.
    fn advanced_form_preview_font(&mut self, ctx: &egui::Context) -> egui::FontId {
        const PREVIEW_FONT_SIZE_PX: f32 = 22.0;
        if let Some(font) = self.fonts.get(self.selected_font_idx) {
            let face_index = font
                .faces
                .get(self.selected_face_idx)
                .map_or(0, |face| face.face_index);
            let path = font.path.clone();
            if let Some(family) = self.ensure_combo_font_family(ctx, &path, face_index) {
                return egui::FontId::new(PREVIEW_FONT_SIZE_PX, family);
            }
        }
        egui::FontId::new(PREVIEW_FONT_SIZE_PX, egui::FontFamily::Proportional)
    }

    /// Текст, по которому перебираются формы — всегда исходный (`text`).
    fn advanced_form_source_text(&self) -> String {
        self.text.clone()
    }

    /// От чего зависят пиксельные ширины глифов в окне форм.
    fn advanced_form_metric_signature(&self) -> AdvancedFormMetricSignature {
        let font = self.fonts.get(self.selected_font_idx);
        AdvancedFormMetricSignature {
            font_path: font.map(|font| font.path.to_string_lossy().to_string()),
            face_index: font
                .and_then(|font| font.faces.get(self.selected_face_idx))
                .map_or(0, |face| face.face_index),
            force_bold: self.force_bold,
            force_italic: self.force_italic,
            hanging_punctuation: self.hanging_punctuation,
        }
    }

    /// Строит попиксельную метрику ширины (`GlyphWidths`) выбранным шрифтом для
    /// символов `source_text`. `None`, если шрифт не выбран/не читается — тогда
    /// падаем на посимвольную метрику.
    fn build_advanced_form_glyph_widths(&self, source_text: &str) -> Option<forms::GlyphWidths> {
        // Единицы на em для замеров (должно совпадать с метрикой внутри forms).
        const METRIC_EM: f32 = 1000.0;
        let font = self.fonts.get(self.selected_font_idx)?;
        let face_index = font
            .faces
            .get(self.selected_face_idx)
            .map_or(0, |face| face.face_index);
        let path = font.path.clone();
        // Лёгкая система шрифтов: пустая БД + только нужный файл (без системных шрифтов).
        let mut font_system =
            FontSystem::new_with_locale_and_db("en-US".to_string(), fontdb::Database::new());
        let selected_face = load_selected_font_from_path(&mut font_system, &path, face_index).ok()?;
        let mut attrs = Attrs::new().metrics(Metrics::new(METRIC_EM, METRIC_EM));
        attrs = selected_face.apply_to_attrs(attrs);
        if self.force_bold {
            attrs = attrs.weight(cosmic_text::Weight::BOLD);
        }
        if self.force_italic {
            attrs = attrs.style(cosmic_text::Style::Italic);
        }
        Some(forms::GlyphWidths::build(
            &mut font_system,
            &attrs,
            source_text,
            self.hanging_punctuation,
            forms::DEFAULT_WIDTH_TOLERANCE,
        ))
    }

    fn rebuild_advanced_form_cache_if_needed(&mut self) {
        let source_text = self.advanced_form_source_text();
        let signature = self.advanced_form_metric_signature();
        let stale = match &self.advanced_form_cache {
            Some(cache) => {
                cache.source_text != source_text
                    || cache.preset != self.advanced_form_preset
                    || cache.metric_signature != signature
            }
            None => true,
        };
        if !stale {
            return;
        }
        // Попиксельная метрика выбранным шрифтом; при отсутствии шрифта —
        // посимвольная (с учётом висящей пунктуации).
        let glyph_widths = self.build_advanced_form_glyph_widths(&source_text);
        let char_metric = forms::CharWidthMetric::new(self.hanging_punctuation);
        let metric: &dyn forms::LineWidthMetric = match &glyph_widths {
            Some(glyph_widths) => glyph_widths,
            None => &char_metric,
        };
        // Храним ВСЕ удачные формы (перебор ограничен лишь бюджетом узлов
        // рекурсии). Фильтры применяются ко всему набору; ограничение на 600 —
        // только в отрисовке (`ADVANCED_FORM_DISPLAY_LIMIT`).
        let enumeration = forms::enumerate_forms(
            &source_text,
            self.advanced_form_preset,
            usize::MAX,
            metric,
        );
        let mut forms = enumeration.forms;
        sort_advanced_forms(&mut forms);
        let mut group_counts: Vec<usize> =
            forms.iter().map(|form| form.word_break_count).collect();
        group_counts.sort_unstable();
        group_counts.dedup();
        // Сбрасываем выбор группы, если такого числа переносов больше нет.
        if let Some(selected) = self.advanced_form_group
            && !group_counts.contains(&selected)
        {
            self.advanced_form_group = None;
        }
        let line_bounds = inclusive_bounds(forms.iter().map(|form| form.line_count()));
        let width_bounds = inclusive_bounds(forms.iter().map(|form| form.max_width));
        let peak_max_bound_min = forms
            .iter()
            .map(|form| form.peakiness_pct(PeakBase::Min))
            .max()
            .unwrap_or(0);
        let peak_max_bound_median = forms
            .iter()
            .map(|form| form.peakiness_pct(PeakBase::Median))
            .max()
            .unwrap_or(0);
        let uneven_max_bound = forms.iter().map(|form| form.unevenness_pct).max().unwrap_or(0);
        let conservatism_bound = forms
            .iter()
            .map(|form| form.conservatism)
            .max()
            .unwrap_or(Conservatism::Safe);
        // Диапазоны фильтров заново раскрываются на всю ширину данных; пороги
        // пиковости и неравномерности — на максимум (показываем всё).
        self.advanced_form_line_range = line_bounds;
        self.advanced_form_width_range = width_bounds;
        self.advanced_form_peak_max = match self.advanced_form_peak_base {
            PeakBase::Min => peak_max_bound_min,
            PeakBase::Median => peak_max_bound_median,
        };
        self.advanced_form_uneven_max = uneven_max_bound;
        // Консервативность по умолчанию строгая (`Safe`): показываем только формы
        // без отрыва служебных слов, как раньше. Пользователь ослабляет вручную.
        self.advanced_form_conservatism_max = Conservatism::Safe;
        self.advanced_form_cache = Some(AdvancedFormCache {
            source_text,
            preset: self.advanced_form_preset,
            forms,
            group_counts,
            line_bounds,
            width_bounds,
            metric_signature: signature,
            peak_max_bound_min,
            peak_max_bound_median,
            uneven_max_bound,
            conservatism_bound,
            truncated: enumeration.truncated,
        });
    }

    /// Применяет выбранную форму: записывает её как сформированный текст (исходный
    /// `text` не трогаем) и разворачивает сформированный пан.
    fn apply_advanced_form(&mut self, form: &TextForm) {
        self.formed_text = form.to_text();
        self.advanced_text_show_formed = true;
        self.queue_preview_render();
    }

    /// Плавающее окно перебора форм текста.
    fn draw_advanced_form_window(&mut self, ctx: &egui::Context) -> bool {
        if !self.advanced_form_open {
            return false;
        }
        self.rebuild_advanced_form_cache_if_needed();
        let font_id = self.advanced_form_preview_font(ctx);
        let current_preset = self.advanced_form_preset;
        let current_group = self.advanced_form_group;
        let cache = self.advanced_form_cache.take();

        // Окно центрируется по вьюпорту по итоговому размеру. На первых кадрах
        // (пока размер ещё не измерен) окно скрыто, чтобы не дёргалось.
        let centering = !self.advanced_form_centered;
        let viewport = ctx.content_rect();
        let screen_center = viewport.center();
        let default_size = egui::vec2(viewport.width() * 0.8, viewport.height() * 0.8);

        let mut line_range = self.advanced_form_line_range;
        let mut width_range = self.advanced_form_width_range;
        let mut peak_max = self.advanced_form_peak_max;
        let mut peak_base = self.advanced_form_peak_base;
        let mut uneven_max = self.advanced_form_uneven_max;
        let mut conservatism_max = self.advanced_form_conservatism_max;

        let mut open = true;
        let mut new_preset = current_preset;
        let mut new_group = current_group;
        let mut clicked: Option<usize> = None;

        let mut window = egui::Window::new("Продвинутая форма текста")
            .open(&mut open)
            .resizable(true)
            // Над панелями параметров/действий (они на `Order::Foreground`).
            .order(egui::Order::Tooltip)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_size(default_size);
        if centering {
            window = window.current_pos(screen_center);
        }

        let inner = window.show(ctx, |ui| {
            if centering {
                // Прячем содержимое, пока окно не встанет по центру.
                ui.set_opacity(0.0);
            }
            ui.small(
                "Перебор вариантов переноса. Это не финальный рендер — \
                 просто чёрный текст на белом с висящей пунктуацией.",
            );
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label("Форма:");
                for preset in TextFormPreset::all() {
                    if ui
                        .selectable_label(preset == current_preset, preset.label())
                        .clicked()
                    {
                        new_preset = preset;
                    }
                }
            });
            ui.separator();
            match cache.as_ref() {
                Some(cache) if !cache.forms.is_empty() => {
                    if cache.group_counts.len() > 1 {
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Переносов слов:");
                            if ui
                                .selectable_label(current_group.is_none(), "Все")
                                .clicked()
                            {
                                new_group = None;
                            }
                            for &count in &cache.group_counts {
                                if ui
                                    .selectable_label(
                                        current_group == Some(count),
                                        count.to_string(),
                                    )
                                    .clicked()
                                {
                                    new_group = Some(count);
                                }
                            }
                        });
                    }
                    // Диапазонные фильтры: число строк и ширина строки.
                    let has_line = advanced_form_range_row(
                        ui,
                        "Строк:",
                        "",
                        &mut line_range,
                        cache.line_bounds,
                    );
                    let has_width = advanced_form_range_row(
                        ui,
                        "Ширина (усл.):",
                        "",
                        &mut width_range,
                        cache.width_bounds,
                    );
                    // Порог пиковости: насколько % самая длинная строка длиннее
                    // базовой (минимальной/медианной). Один верхний предел.
                    let peak_bound = match peak_base {
                        PeakBase::Min => cache.peak_max_bound_min,
                        PeakBase::Median => cache.peak_max_bound_median,
                    };
                    let has_peak = peak_bound > 0;
                    if has_peak {
                        ui.add(
                            WheelSlider::new(&mut peak_max, 0..=peak_bound)
                                .text("Длиннее базы не более чем на")
                                .suffix("%"),
                        );
                        ui.horizontal(|ui| {
                            ui.label("База пиковости:");
                            if ui
                                .selectable_label(peak_base == PeakBase::Min, "минимум")
                                .clicked()
                            {
                                peak_base = PeakBase::Min;
                            }
                            if ui
                                .selectable_label(peak_base == PeakBase::Median, "медиана")
                                .clicked()
                            {
                                peak_base = PeakBase::Median;
                            }
                        });
                    }
                    // Порог неравномерности: средний разброс ширин строк от
                    // медианы. Меньше — ровнее форма.
                    let uneven_bound = cache.uneven_max_bound;
                    let has_uneven = uneven_bound > 0;
                    if has_uneven {
                        ui.add(
                            WheelSlider::new(&mut uneven_max, 0..=uneven_bound)
                                .text("Неравномерность не более")
                                .suffix("%"),
                        );
                    }
                    // Порог консервативности: какие отрывы служебных слов допускать.
                    // `Safe` («нет») — только безопасные переносы; каждая следующая
                    // категория добавляет более рискованные отрывы.
                    let has_conservatism = cache.conservatism_bound > Conservatism::Safe;
                    if has_conservatism {
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Отрыв служебных слов:");
                            for level in Conservatism::all() {
                                if level > cache.conservatism_bound {
                                    break;
                                }
                                let text = if level == Conservatism::Safe {
                                    "нет".to_string()
                                } else {
                                    format!("+ {}", level.label())
                                };
                                if ui
                                    .selectable_label(conservatism_max == level, text)
                                    .clicked()
                                {
                                    conservatism_max = level;
                                }
                            }
                        });
                    }
                    if (has_line || has_width || has_peak || has_uneven || has_conservatism)
                        && ui.small_button("Сбросить фильтры").clicked()
                    {
                        line_range = cache.line_bounds;
                        width_range = cache.width_bounds;
                        peak_max = peak_bound;
                        uneven_max = uneven_bound;
                        conservatism_max = Conservatism::Safe;
                        new_group = None;
                    }

                    let passes = |form: &TextForm| {
                        new_group.is_none_or(|c| form.word_break_count == c)
                            && (line_range.0..=line_range.1).contains(&form.line_count())
                            && (width_range.0..=width_range.1).contains(&form.max_width)
                            && form.peakiness_pct(peak_base) <= peak_max
                            && form.unevenness_pct <= uneven_max
                            && form.conservatism <= conservatism_max
                    };

                    let visible = cache.forms.iter().filter(|form| passes(form)).count();
                    let shown = visible.min(ADVANCED_FORM_DISPLAY_LIMIT);
                    let mut status = if shown < visible {
                        format!("Вариантов: {visible}, показаны первые {shown}.")
                    } else {
                        format!("Вариантов: {visible}.")
                    };
                    if cache.truncated {
                        status.push_str(" Перебор форм неполный (достигнут предел).");
                    }
                    ui.small(status);
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                let mut drawn = 0usize;
                                for (idx, form) in cache.forms.iter().enumerate() {
                                    if !passes(form) {
                                        continue;
                                    }
                                    if drawn >= ADVANCED_FORM_DISPLAY_LIMIT {
                                        break;
                                    }
                                    drawn += 1;
                                    if draw_advanced_form_card(ui, &font_id, &form.lines)
                                        .clicked()
                                    {
                                        clicked = Some(idx);
                                    }
                                }
                            });
                        });
                }
                Some(_) => {
                    ui.label("Нет вариантов, удовлетворяющих этой форме.");
                }
                None => {
                    ui.label("Введите текст, чтобы подобрать формы.");
                }
            }
        });

        // Как только окно отрисовалось и знает свой размер — на следующем кадре
        // оно уже стоит по центру; делаем его видимым.
        if centering {
            if inner.is_some_and(|inner| {
                inner.response.rect.width() > 1.0 && inner.response.rect.height() > 1.0
            }) {
                self.advanced_form_centered = true;
            }
            ctx.request_repaint();
        }

        self.advanced_form_line_range = line_range;
        self.advanced_form_width_range = width_range;
        // Смена базы делает старый порог несопоставимым — раскрываем его на
        // максимум для новой базы.
        if peak_base != self.advanced_form_peak_base {
            self.advanced_form_peak_base = peak_base;
            if let Some(cache) = cache.as_ref() {
                peak_max = match peak_base {
                    PeakBase::Min => cache.peak_max_bound_min,
                    PeakBase::Median => cache.peak_max_bound_median,
                };
            }
        }
        self.advanced_form_peak_max = peak_max;
        self.advanced_form_uneven_max = uneven_max;
        self.advanced_form_conservatism_max = conservatism_max;

        let mut changed = false;
        if let Some(idx) = clicked
            && let Some(cache) = cache.as_ref()
            && let Some(form) = cache.forms.get(idx)
        {
            self.apply_advanced_form(form);
            // После выбора формы окно закрывается.
            open = false;
            changed = true;
        }
        self.advanced_form_cache = cache;
        if new_preset != self.advanced_form_preset {
            self.advanced_form_preset = new_preset;
            self.advanced_form_cache = None;
        }
        self.advanced_form_group = new_group;
        self.advanced_form_open = open;
        changed
    }

    fn draw_edit_params_section(
        &mut self,
        ui: &mut egui::Ui,
        stacked_columns: bool,
        remap_wheel_to_horizontal: bool,
    ) -> bool {
        let mut changed = self.draw_advanced_form_window(ui.ctx());
        let mut block_hscroll_by_hovered_param = false;

        if stacked_columns {
            let font_missing = self.missing_font.is_some();
            ui.vertical(|ui| {
                if let Some(missing) = self.missing_font.clone() {
                    ui.colored_label(
                        Color32::from_rgb(240, 110, 110),
                        format!("⚠ Шрифт «{missing}» не найден среди доступных."),
                    );
                    ui.add_space(4.0);
                }
                ui.add_enabled_ui(!font_missing, |ui| {
                    changed |= self.draw_text_accordion(
                        ui,
                        "stacked",
                        &mut block_hscroll_by_hovered_param,
                    );
                });
                ui.add_space(6.0);

                let selection_mode = self.inline_selection_context().is_some();
                ui.add_enabled_ui(!selection_mode && !font_missing, |ui| {
                    let width_resp = ui
                        .add(WheelSlider::new(&mut self.width_px, 16..=4096).text("Ширина (px)"));
                    mark_hscroll_block_on_hover(&mut block_hscroll_by_hovered_param, &width_resp);
                    changed |= width_resp.changed();
                    if let Some(steps) = wheel_steps_if_hovered(ui, &width_resp) {
                        changed |= apply_wheel_step_u32(&mut self.width_px, steps, 10, 16, 4096);
                    }

                    let scale_resp = ui.add(
                        WheelSlider::new(&mut self.overlay_scale, 0.05..=20.0).text("Масштаб"),
                    );
                    mark_hscroll_block_on_hover(&mut block_hscroll_by_hovered_param, &scale_resp);
                    changed |= scale_resp.changed();
                    if let Some(steps) = wheel_steps_if_hovered(ui, &scale_resp) {
                        changed |= apply_wheel_step_f32(
                            &mut self.overlay_scale,
                            steps,
                            0.05,
                            0.05,
                            20.0,
                        );
                    }

                    let angle_resp = ui.add(
                        WheelSlider::new(&mut self.overlay_rotation_deg, -180.0..=180.0)
                            .text("Угол (°)"),
                    );
                    mark_hscroll_block_on_hover(&mut block_hscroll_by_hovered_param, &angle_resp);
                    changed |= angle_resp.changed();
                    if let Some(steps) = wheel_steps_if_hovered(ui, &angle_resp) {
                        changed |= apply_wheel_step_f32(
                            &mut self.overlay_rotation_deg,
                            steps,
                            1.0,
                            -180.0,
                            180.0,
                        );
                    }
                });

                ui.separator();
                changed |= self.draw_main_text_params(
                    ui,
                    true,
                    remap_wheel_to_horizontal,
                    false,
                    font_missing,
                );
                if selection_mode {
                    ui.add_space(4.0);
                    ui.small(
                        "При выделении `Шрифт`, `Размер`, `Межстрочный отступ`, `Кернинг`, `Высота/Ширина символа`, `Bold`, `Italic` и `Смещение X/Y` меняют inline-теги; остальные параметры редактируют базовый стиль.",
                    );
                }
            });
            if remap_wheel_to_horizontal {
                apply_horizontal_wheel_scroll_if_idle(ui, block_hscroll_by_hovered_param);
            } else if block_hscroll_by_hovered_param {
                consume_wheel_scroll_delta(ui);
            }
            if changed {
                self.queue_preview_render();
            }
            return changed;
        }

        let inline_selection = self.inline_selection_context();
        let selection_mode = inline_selection.is_some();
        let mut inline_style = inline_selection
            .as_ref()
            .map(|selection| self.effective_inline_tag_style(selection));

        ui.vertical(|ui| {
            let spacing_x = ui.spacing().item_spacing.x;
            let available_w = ui.available_width().max(1.0);
            let columns_w = (available_w - spacing_x).max(1.0);
            let left_ratio = 0.34;
            let min_left_w = 170.0;
            let min_right_w = 300.0;
            let mut left_w = columns_w * left_ratio;
            let mut right_w = columns_w - left_w;
            if columns_w >= (min_left_w + min_right_w) {
                if left_w < min_left_w {
                    left_w = min_left_w;
                    right_w = columns_w - left_w;
                }
                if right_w < min_right_w {
                    right_w = min_right_w;
                    left_w = columns_w - right_w;
                }
            }

            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(left_w, 0.0),
                    egui::Layout::top_down(Align::Min),
                    |ui| {
                        changed |= self.draw_text_accordion(
                            ui,
                            "columns",
                            &mut block_hscroll_by_hovered_param,
                        );
                    },
                );

                ui.allocate_ui_with_layout(
                    Vec2::new(right_w, 0.0),
                    egui::Layout::top_down(Align::Min),
                    |ui| {
                        ui.horizontal_top(|ui| {
                            let inner_spacing_x = ui.spacing().item_spacing.x;
                            let inner_available_w = ui.available_width().max(1.0);
                            let mut right_col_w = (inner_available_w * 0.28).max(165.0);
                            let mut left_cluster_w =
                                (inner_available_w - inner_spacing_x - right_col_w).max(1.0);
                            if inner_available_w >= 480.0 && left_cluster_w < 280.0 {
                                left_cluster_w = 280.0;
                                right_col_w =
                                    (inner_available_w - inner_spacing_x - left_cluster_w).max(1.0);
                            }

                            ui.allocate_ui_with_layout(
                                Vec2::new(left_cluster_w, 0.0),
                                egui::Layout::top_down(Align::Min),
                                |ui| {
                                    ui.group(|ui| {
                                        ui.set_width(ui.available_width());
                                        ui.set_min_width(ui.available_width());
                                        ui.set_max_width(ui.available_width());
                                        ui.label(egui::RichText::new("Шрифт").strong());
                                        ui.horizontal(|ui| {
                                            let prev_font_idx = self.selected_font_idx;
                                            let selected_font_text = inline_style
                                                .as_ref()
                                                .and_then(|style| style.font_label.as_deref())
                                                .or_else(|| {
                                                    self.fonts
                                                        .get(self.selected_font_idx)
                                                        .map(|font| font.label.as_str())
                                                })
                                                .unwrap_or("<шрифт>");
                                            let mut font_idx = inline_style
                                                .as_ref()
                                                .and_then(|style| {
                                                    self.find_font_idx_by_path_or_label(
                                                        None,
                                                        style.font_label.as_deref(),
                                                    )
                                                })
                                                .unwrap_or(self.selected_font_idx);
                                            let font_count = self.fonts.len();
                                            let font_combo = WheelComboBox::from_label("Шрифт")
                                                .selected_text(selected_font_text)
                                                .show_ui_with_wheel(ui, |ui| {
                                                    for idx in 0..self.fonts.len() {
                                                        let (label, path, face_index) = {
                                                            let font = &self.fonts[idx];
                                                            (
                                                                font.label.clone(),
                                                                font.path.clone(),
                                                                font.faces
                                                                    .first()
                                                                    .map(|face| face.face_index)
                                                                    .unwrap_or(0),
                                                            )
                                                        };
                                                        let selected = font_idx == idx;
                                                        if self.draw_font_combo_option(
                                                            ui,
                                                            &label,
                                                            path.as_path(),
                                                            face_index,
                                                            selected,
                                                        ) {
                                                            font_idx = idx;
                                                        }
                                                    }
                                                });
                                            mark_hscroll_block_on_hover(
                                                &mut block_hscroll_by_hovered_param,
                                                &font_combo.inner.response,
                                            );
                                            if let Some(steps) = font_combo.wheel_steps {
                                                cycle_wrapped_index(&mut font_idx, font_count, steps);
                                            }
                                            if let Some(style) = inline_style.as_mut() {
                                                if let Some(label) = self.font_label_by_idx(font_idx) {
                                                    style.font_label = Some(label);
                                                }
                                            } else {
                                                self.selected_font_idx = font_idx;
                                                if self.selected_font_idx != prev_font_idx {
                                                    self.selected_face_idx = 0;
                                                    changed = true;
                                                }
                                            }

                                            ui.add_enabled_ui(!selection_mode, |ui| {
                                                let prev_face_idx = self.selected_face_idx;
                                                let selected_face_text = self
                                                    .fonts
                                                    .get(self.selected_font_idx)
                                                    .and_then(|font| {
                                                        font.faces.get(self.selected_face_idx)
                                                    })
                                                    .map(|face| face.label.as_str())
                                                    .unwrap_or("<face>");
                                                let face_count = self
                                                    .fonts
                                                    .get(self.selected_font_idx)
                                                    .map(|font| font.faces.len())
                                                    .unwrap_or(0);
                                                let mut face_idx = self.selected_face_idx;
                                                let face_combo = WheelComboBox::from_label("Face")
                                                    .selected_text(selected_face_text)
                                                    .show_ui_with_wheel(ui, |ui| {
                                                        if let Some(font) =
                                                            self.fonts.get(self.selected_font_idx)
                                                        {
                                                            for (idx, face) in
                                                                font.faces.iter().enumerate()
                                                            {
                                                                ui.selectable_value(
                                                                    &mut face_idx,
                                                                    idx,
                                                                    &face.label,
                                                                );
                                                            }
                                                        }
                                                    });
                                                mark_hscroll_block_on_hover(
                                                    &mut block_hscroll_by_hovered_param,
                                                    &face_combo.inner.response,
                                                );
                                                if let Some(steps) = face_combo.wheel_steps {
                                                    cycle_wrapped_index(
                                                        &mut face_idx,
                                                        face_count,
                                                        steps,
                                                    );
                                                }
                                                self.selected_face_idx = face_idx;
                                                if self.selected_face_idx != prev_face_idx {
                                                    changed = true;
                                                }

                                                let mut requested_use_system_fonts =
                                                    self.use_system_fonts;
                                                let use_system_fonts_resp = ui.checkbox(
                                                    &mut requested_use_system_fonts,
                                                    "Использовать системные шрифты",
                                                );
                                                mark_hscroll_block_on_hover(
                                                    &mut block_hscroll_by_hovered_param,
                                                    &use_system_fonts_resp,
                                                );
                                                if use_system_fonts_resp.changed() {
                                                    self.pending_use_system_fonts_toggle_request =
                                                        Some(requested_use_system_fonts);
                                                }
                                            });
                                        });
                                    });

                                    ui.add_space(4.0);

                                    let mid_available_w = ui.available_width().max(1.0);
                                    let mut mid_col_w = (mid_available_w - inner_spacing_x) / 2.0;
                                    if mid_col_w <= 0.0 {
                                        mid_col_w = 1.0;
                                    }

                                    ui.horizontal_top(|ui| {
                                        ui.allocate_ui_with_layout(
                                            Vec2::new(mid_col_w, 0.0),
                                            egui::Layout::top_down(Align::Min),
                                            |ui| {
                                                ui.add_enabled_ui(!selection_mode, |ui| {
                                                    let width_resp = ui.add(
                                                        WheelSlider::new(
                                                            &mut self.width_px,
                                                            16..=4096,
                                                        )
                                                        .text("Ширина (px)"),
                                                    );
                                                    mark_hscroll_block_on_hover(
                                                        &mut block_hscroll_by_hovered_param,
                                                        &width_resp,
                                                    );
                                                    changed |= width_resp.changed();
                                                    if let Some(steps) =
                                                        wheel_steps_if_hovered(ui, &width_resp)
                                                    {
                                                        changed |= apply_wheel_step_u32(
                                                            &mut self.width_px,
                                                            steps,
                                                            10,
                                                            16,
                                                            4096,
                                                        );
                                                    }

                                                    let scale_resp = ui.add(
                                                        WheelSlider::new(
                                                            &mut self.overlay_scale,
                                                            0.05..=20.0,
                                                        )
                                                        .text("Масштаб"),
                                                    );
                                                    mark_hscroll_block_on_hover(
                                                        &mut block_hscroll_by_hovered_param,
                                                        &scale_resp,
                                                    );
                                                    changed |= scale_resp.changed();
                                                    if let Some(steps) =
                                                        wheel_steps_if_hovered(ui, &scale_resp)
                                                    {
                                                        changed |= apply_wheel_step_f32(
                                                            &mut self.overlay_scale,
                                                            steps,
                                                            0.05,
                                                            0.05,
                                                            20.0,
                                                        );
                                                    }

                                                    let angle_resp = ui.add(
                                                        WheelSlider::new(
                                                            &mut self.overlay_rotation_deg,
                                                            -180.0..=180.0,
                                                        )
                                                        .text("Угол (°)"),
                                                    );
                                                    mark_hscroll_block_on_hover(
                                                        &mut block_hscroll_by_hovered_param,
                                                        &angle_resp,
                                                    );
                                                    changed |= angle_resp.changed();
                                                    if let Some(steps) =
                                                        wheel_steps_if_hovered(ui, &angle_resp)
                                                    {
                                                        changed |= apply_wheel_step_f32(
                                                            &mut self.overlay_rotation_deg,
                                                            steps,
                                                            1.0,
                                                            -180.0,
                                                            180.0,
                                                        );
                                                    }
                                                });
                                            },
                                        );

                                        ui.allocate_ui_with_layout(
                                            Vec2::new(mid_col_w, 0.0),
                                            egui::Layout::top_down(Align::Min),
                                            |ui| {
                                                let color_resp = self
                                                    .text_color_selector
                                                    .draw(ui, &mut self.text_color);
                                                changed |= color_resp.changed;
                                                if let Some(style) = inline_style.as_mut() {
                                                    let mut font_size_px = style
                                                        .font_size_px
                                                        .unwrap_or(self.font_size_px)
                                                        .clamp(1.0, 256.0);
                                                    let font_size_resp = ui.add(
                                                        WheelSlider::new(
                                                            &mut font_size_px,
                                                            1.0..=256.0,
                                                        )
                                                        .text("Размер (px)")
                                                        .wheel_step(1.0),
                                                    );
                                                    changed |= font_size_resp.changed();
                                                    style.font_size_px = Some(font_size_px);
                                                } else {
                                                    let font_size_resp = ui.add(
                                                        WheelSlider::new(
                                                            &mut self.font_size_px,
                                                            1.0..=256.0,
                                                        )
                                                        .text("Размер (px)")
                                                        .wheel_step(1.0),
                                                    );
                                                    changed |= font_size_resp.changed();
                                                }

                                                let base_font_size_px = self.font_size_px.max(1.0);
                                                if let Some(style) = inline_style.as_mut() {
                                                    let inline_font_size_px = style
                                                        .font_size_px
                                                        .unwrap_or(base_font_size_px)
                                                        .max(1.0);
                                                    let mut line_spacing = style
                                                        .line_spacing
                                                        .unwrap_or(self.line_spacing);
                                                    px_or_percent_param_row(
                                                        ui,
                                                        "Межстрочный отступ",
                                                        &mut line_spacing,
                                                        -300.0..=300.0,
                                                        2.0,
                                                        inline_font_size_px,
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                    style.line_spacing = Some(line_spacing);

                                                    ui.horizontal(|ui| {
                                                        ui.label("Кернинг");
                                                        ui.add_enabled(
                                                            false,
                                                            egui::Button::new("Метрический")
                                                                .selected(self.kerning_mode == KerningMode::Metric),
                                                        );
                                                        ui.add_enabled(
                                                            false,
                                                            egui::Button::new("Оптический")
                                                                .selected(self.kerning_mode == KerningMode::Optical),
                                                        );
                                                    });

                                                    let mut kerning = style
                                                        .kerning
                                                        .unwrap_or(self.kerning);
                                                    px_or_percent_param_row(
                                                        ui,
                                                        "Кернинг",
                                                        &mut kerning,
                                                        -300.0..=300.0,
                                                        2.0,
                                                        inline_font_size_px,
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                    style.kerning = Some(kerning);

                                                    let mut stretching = style
                                                        .glyph_stretching
                                                        .unwrap_or([self.glyph_width, self.glyph_height]);
                                                    px_or_percent_param_row(
                                                        ui,
                                                        "Высота символа",
                                                        &mut stretching[1],
                                                        1.0..=300.0,
                                                        5.0,
                                                        inline_font_size_px,
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                    px_or_percent_param_row(
                                                        ui,
                                                        "Ширина символа",
                                                        &mut stretching[0],
                                                        1.0..=300.0,
                                                        5.0,
                                                        inline_font_size_px,
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                    style.glyph_stretching = Some(stretching);
                                                    self.draw_inline_offset_controls(
                                                        ui,
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                        Some(style),
                                                    );
                                                } else {
                                                    px_or_percent_param_row(
                                                        ui,
                                                        "Межстрочный отступ",
                                                        &mut self.line_spacing,
                                                        -300.0..=300.0,
                                                        2.0,
                                                        base_font_size_px,
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                    ui.horizontal(|ui| {
                                                        ui.label("Кернинг");
                                                        changed |= ui.selectable_value(&mut self.kerning_mode, KerningMode::Metric, "Метрический").changed();
                                                        changed |= ui.selectable_value(&mut self.kerning_mode, KerningMode::Optical, "Оптический").changed();
                                                    });
                                                    px_or_percent_param_row(
                                                        ui,
                                                        "Кернинг",
                                                        &mut self.kerning,
                                                        -300.0..=300.0,
                                                        2.0,
                                                        base_font_size_px,
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                    px_or_percent_param_row(
                                                        ui,
                                                        "Высота символа",
                                                        &mut self.glyph_height,
                                                        1.0..=300.0,
                                                        5.0,
                                                        base_font_size_px,
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                    px_or_percent_param_row(
                                                        ui,
                                                        "Ширина символа",
                                                        &mut self.glyph_width,
                                                        1.0..=300.0,
                                                        5.0,
                                                        base_font_size_px,
                                                        &mut changed,
                                                        &mut block_hscroll_by_hovered_param,
                                                    );
                                                }
                                            },
                                        );
                                    });
                                },
                            );

                            ui.allocate_ui_with_layout(
                                Vec2::new(right_col_w, 0.0),
                                egui::Layout::top_down(Align::Min),
                                |ui| {
                                        self.draw_alignment_controls(
                                            ui,
                                            &mut changed,
                                            &mut block_hscroll_by_hovered_param,
                                        );

                                        let prev_shape = self.text_shape;
                                        let shape_combo = WheelComboBox::from_label("Форма")
                                            .selected_text(match self.text_shape {
                                                TextShape::Free => "Свободно",
                                                TextShape::Rectangle => "[  ]",
                                                TextShape::Oval => "(  )",
                                                TextShape::Hexagon => "<  >",
                                                TextShape::SoftPeak => "Мягкая",
                                            })
                                            .show_ui_with_wheel(ui, |ui| {
                                                ui.selectable_value(
                                                    &mut self.text_shape,
                                                    TextShape::Free,
                                                    "Свободно",
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_shape,
                                                    TextShape::Rectangle,
                                                    "[  ]",
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_shape,
                                                    TextShape::Oval,
                                                    "(  )",
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_shape,
                                                    TextShape::Hexagon,
                                                    "<  >",
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_shape,
                                                    TextShape::SoftPeak,
                                                    "Мягкая",
                                                );
                                            });
                                        mark_hscroll_block_on_hover(
                                            &mut block_hscroll_by_hovered_param,
                                            &shape_combo.inner.response,
                                        );
                                        if let Some(steps) = shape_combo.wheel_steps {
                                            changed |=
                                                cycle_text_shape(&mut self.text_shape, steps);
                                        }
                                        if self.text_shape != prev_shape {
                                            changed = true;
                                        }

                                        let prev_wrap_mode = self.text_wrap_mode;
                                        let wrap_combo = WheelComboBox::from_label("Перенос")
                                            .selected_text(text_wrap_mode_label(
                                                self.text_wrap_mode,
                                            ))
                                            .show_ui_with_wheel(ui, |ui| {
                                                ui.selectable_value(
                                                    &mut self.text_wrap_mode,
                                                    TextWrapMode::None,
                                                    text_wrap_mode_label(TextWrapMode::None),
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_wrap_mode,
                                                    TextWrapMode::WholeWords,
                                                    text_wrap_mode_label(TextWrapMode::WholeWords),
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_wrap_mode,
                                                    TextWrapMode::Minimal,
                                                    text_wrap_mode_label(TextWrapMode::Minimal),
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_wrap_mode,
                                                    TextWrapMode::Moderate,
                                                    text_wrap_mode_label(TextWrapMode::Moderate),
                                                );
                                                ui.selectable_value(
                                                    &mut self.text_wrap_mode,
                                                    TextWrapMode::Aggressive,
                                                    text_wrap_mode_label(TextWrapMode::Aggressive),
                                                );
                                            });
                                        mark_hscroll_block_on_hover(
                                            &mut block_hscroll_by_hovered_param,
                                            &wrap_combo.inner.response,
                                        );
                                        if let Some(steps) = wrap_combo.wheel_steps {
                                            changed |=
                                                cycle_text_wrap_mode(&mut self.text_wrap_mode, steps);
                                        }
                                        if self.text_wrap_mode != prev_wrap_mode {
                                            self.sync_wrap_mode_constraints();
                                            changed = true;
                                        }
                                        let moderate_trees_resp = ui.add_enabled(
                                            self.moderate_trees_checkbox_enabled(),
                                            egui::Checkbox::new(
                                                &mut self.allow_moderate_trees,
                                                "Разрешить умеренные ёлки",
                                            ),
                                        );
                                        changed |= moderate_trees_resp.changed();

                                        if matches!(
                                            self.text_shape,
                                            TextShape::Oval | TextShape::Hexagon
                                        ) {
                                            let min_width_resp = ui.add(
                                                WheelSlider::new(
                                                    &mut self.shape_min_width_percent,
                                                    5.0..=100.0,
                                                )
                                                .text("Минимальная ширина (%)"),
                                            );
                                            mark_hscroll_block_on_hover(
                                                &mut block_hscroll_by_hovered_param,
                                                &min_width_resp,
                                            );
                                            changed |= min_width_resp.changed();
                                            if let Some(steps) =
                                                wheel_steps_if_hovered(ui, &min_width_resp)
                                            {
                                                changed |= apply_wheel_step_f32(
                                                    &mut self.shape_min_width_percent,
                                                    steps,
                                                    1.0,
                                                    5.0,
                                                    100.0,
                                                );
                                            }
                                        }
                                        if self.text_shape == TextShape::SoftPeak {
                                            let variant_resp = ui.add(
                                                WheelSlider::new(&mut self.shape_variant, 1..=9)
                                                    .text("Вариант формы"),
                                            );
                                            mark_hscroll_block_on_hover(
                                                &mut block_hscroll_by_hovered_param,
                                                &variant_resp,
                                            );
                                            changed |= variant_resp.changed();
                                            if let Some(steps) =
                                                wheel_steps_if_hovered(ui, &variant_resp)
                                            {
                                                changed |= apply_wheel_step_u8(
                                                    &mut self.shape_variant,
                                                    steps,
                                                    1,
                                                    1,
                                                    9,
                                                );
                                            }
                                        }
                                        if let Some(style) = inline_style.as_mut() {
                                            let mut bold = style.bold;
                                            let force_bold_resp = ui.checkbox(&mut bold, "Bold");
                                            mark_hscroll_block_on_hover(
                                                &mut block_hscroll_by_hovered_param,
                                                &force_bold_resp,
                                            );
                                            changed |= force_bold_resp.changed();
                                            style.bold = bold;

                                            let mut italic = style.italic;
                                            let force_italic_resp = ui.checkbox(&mut italic, "Italic");
                                            mark_hscroll_block_on_hover(
                                                &mut block_hscroll_by_hovered_param,
                                                &force_italic_resp,
                                            );
                                            changed |= force_italic_resp.changed();
                                            style.italic = italic;
                                        } else {
                                            let force_bold_resp =
                                                ui.checkbox(&mut self.force_bold, "Bold");
                                            mark_hscroll_block_on_hover(
                                                &mut block_hscroll_by_hovered_param,
                                                &force_bold_resp,
                                            );
                                            changed |= force_bold_resp.changed();
                                            let force_italic_resp =
                                                ui.checkbox(&mut self.force_italic, "Italic");
                                            mark_hscroll_block_on_hover(
                                                &mut block_hscroll_by_hovered_param,
                                                &force_italic_resp,
                                            );
                                            changed |= force_italic_resp.changed();
                                        }
                                        let hanging_punct_resp = ui.checkbox(
                                            &mut self.hanging_punctuation,
                                            "Висящая пунктуация",
                                        );
                                        mark_hscroll_block_on_hover(
                                            &mut block_hscroll_by_hovered_param,
                                            &hanging_punct_resp,
                                        );
                                        changed |= hanging_punct_resp.changed();
                                        let trim_spaces_resp = ui.checkbox(
                                            &mut self.trim_extra_spaces,
                                            "Удалять лишние пробелы",
                                        );
                                        mark_hscroll_block_on_hover(
                                            &mut block_hscroll_by_hovered_param,
                                            &trim_spaces_resp,
                                        );
                                        changed |= trim_spaces_resp.changed();
                                        let sentence_nl_resp = ui.checkbox(
                                            &mut self.new_line_after_sentence,
                                            "Новая строка после конца предложения",
                                        );
                                        mark_hscroll_block_on_hover(
                                            &mut block_hscroll_by_hovered_param,
                                            &sentence_nl_resp,
                                        );
                                        changed |= sentence_nl_resp.changed();
                                        let uppercase_text_resp = ui.checkbox(
                                            &mut self.uppercase_text,
                                            "Всё в верхнем регистре",
                                        );
                                        mark_hscroll_block_on_hover(
                                            &mut block_hscroll_by_hovered_param,
                                            &uppercase_text_resp,
                                        );
                                        changed |= uppercase_text_resp.changed();
                                        let inline_tags_resp = ui.checkbox(
                                            &mut self.enable_inline_style_tags,
                                            "Парсить теги <b>/<i> в тексте",
                                        );
                                        mark_hscroll_block_on_hover(
                                            &mut block_hscroll_by_hovered_param,
                                            &inline_tags_resp,
                                        );
                                        changed |= inline_tags_resp.changed();

                                        self.draw_advanced_text_params_section(
                                            ui,
                                            &mut changed,
                                            &mut block_hscroll_by_hovered_param,
                                            "typing_advanced_text_params_edit_columns",
                                        );
                                },
                            );
                        });
                    },
                );
            });

            if selection_mode {
                ui.add_space(4.0);
                ui.small(
                    "При выделении `Цвет`, `Шрифт`, `Размер`, `Межстрочный отступ`, `Кернинг`, `Высота/Ширина символа`, `Bold`, `Italic` и `Смещение X/Y` меняют inline-теги; остальные параметры редактируют базовый стиль.",
                );
            }

            // Extra bottom padding so the horizontal scrollbar doesn't overlap the last checkbox text.
            ui.add_space(ui.spacing().scroll.allocated_width() + 4.0);
        });
        if remap_wheel_to_horizontal {
            apply_horizontal_wheel_scroll_if_idle(ui, block_hscroll_by_hovered_param);
        } else if block_hscroll_by_hovered_param {
            consume_wheel_scroll_delta(ui);
        }
        if let (Some(selection), Some(style)) = (inline_selection, inline_style) {
            changed |= self.apply_inline_style_to_selection(selection, style);
        }
        if changed {
            self.queue_preview_render();
        }
        changed
    }

    fn sync_text_selection_from_text_edit(
        &mut self,
        ctx: &egui::Context,
        text_edit_id: Id,
        response: &egui::Response,
        cursor_range: Option<CCursorRange>,
    ) {
        if let Some(range) = self.pending_text_selection_restore.take() {
            let clamped = clamp_char_range(self.active_inline_text(), range);
            let mut state = egui::TextEdit::load_state(ctx, text_edit_id).unwrap_or_default();
            state.cursor.set_char_range(Some(CCursorRange::two(
                CCursor::new(clamped.start),
                CCursor::new(clamped.end),
            )));
            state.store(ctx, text_edit_id);
            self.text_selection_char_range = Some(clamped);
            return;
        }

        if let Some(range) = cursor_range.map(|range| range.as_sorted_char_range()) {
            if range.start < range.end {
                self.text_selection_char_range = Some(range);
            } else if response.clicked() || response.dragged() {
                self.text_selection_char_range = None;
            }
        }
    }

    fn paint_persistent_text_selection_if_needed(
        &self,
        ui: &egui::Ui,
        text_output: &egui::text_edit::TextEditOutput,
    ) {
        if text_output.response.has_focus() {
            return;
        }

        let Some(char_range) = self.text_selection_char_range.as_ref() else {
            return;
        };
        if char_range.start >= char_range.end {
            return;
        }

        let clamped = clamp_char_range(self.active_inline_text(), char_range.clone());
        if clamped.start >= clamped.end {
            return;
        }

        let mut galley = text_output.galley.clone();
        paint_text_selection(
            &mut galley,
            ui.visuals(),
            &CCursorRange::two(CCursor::new(clamped.start), CCursor::new(clamped.end)),
            None,
        );

        ui.painter()
            .with_clip_rect(text_output.text_clip_rect)
            .galley(text_output.galley_pos, galley, ui.visuals().text_color());
    }

    /// Активный буфер для выделения и инлайн-тегов (исходный/сформированный).
    fn active_inline_text(&self) -> &str {
        match self.inline_text_target {
            InlineTextTarget::Source => &self.text,
            InlineTextTarget::Formed => &self.formed_text,
        }
    }

    fn set_active_inline_text(&mut self, value: String) {
        match self.inline_text_target {
            InlineTextTarget::Source => self.text = value,
            InlineTextTarget::Formed => self.formed_text = value,
        }
    }

    /// Сбрасывает текущее выделение (например, при переключении панов аккордеона).
    fn clear_inline_text_selection(&mut self) {
        self.text_selection_char_range = None;
        self.pending_text_selection_restore = None;
    }

    fn inline_selection_context(&self) -> Option<TypingInlineSelectionContext> {
        let char_range = self.text_selection_char_range.as_ref()?.clone();
        if char_range.start >= char_range.end {
            return None;
        }
        let text = self.active_inline_text();
        let text_byte_range = char_range_to_byte_range(text, &char_range)?;
        if text_byte_range.start >= text_byte_range.end {
            return None;
        }

        let opening_tags = collect_adjacent_opening_inline_tags(text, text_byte_range.start);
        let closing_tags = collect_adjacent_closing_inline_tags(text, text_byte_range.end);
        let matched_count = opening_tags
            .iter()
            .zip(closing_tags.iter())
            .take_while(|(open_tag, close_tag)| {
                inline_tag_kinds_match(&open_tag.kind, &close_tag.kind)
            })
            .count();

        let opening_wrapper_range = if matched_count > 0 {
            let start = opening_tags
                .get(matched_count.saturating_sub(1))
                .map(|tag| tag.byte_range.start)
                .unwrap_or(text_byte_range.start);
            start..text_byte_range.start
        } else {
            text_byte_range.start..text_byte_range.start
        };
        let closing_wrapper_range = if matched_count > 0 {
            let end = closing_tags
                .get(matched_count.saturating_sub(1))
                .map(|tag| tag.byte_range.end)
                .unwrap_or(text_byte_range.end);
            text_byte_range.end..end
        } else {
            text_byte_range.end..text_byte_range.end
        };

        let mut style = TypingInlineTagStyle::default();
        for tag in opening_tags.iter().take(matched_count) {
            match &tag.kind {
                TypingInlineTagKind::Bold => style.bold = true,
                TypingInlineTagKind::Italic => style.italic = true,
                TypingInlineTagKind::Font(label) => style.font_label = Some(label.clone()),
                TypingInlineTagKind::Size(size_px) => style.font_size_px = Some(*size_px),
                TypingInlineTagKind::Color(color) => style.text_color = Some(*color),
                TypingInlineTagKind::LineSpacing(value) => style.line_spacing = Some(*value),
                TypingInlineTagKind::Kerning(value) => style.kerning = Some(*value),
                TypingInlineTagKind::Stretching(value) => style.glyph_stretching = Some(*value),
                TypingInlineTagKind::Offset(offset) => style.glyph_offset = Some(*offset),
                TypingInlineTagKind::Machine(machine) => {
                    if machine.bold {
                        style.bold = true;
                    }
                    if machine.italic {
                        style.italic = true;
                    }
                    if machine.font_label.is_some() {
                        style.font_label = machine.font_label.clone();
                    }
                    if machine.font_size_px.is_some() {
                        style.font_size_px = machine.font_size_px;
                    }
                    if machine.text_color.is_some() {
                        style.text_color = machine.text_color;
                    }
                    if machine.line_spacing.is_some() {
                        style.line_spacing = machine.line_spacing;
                    }
                    if machine.kerning.is_some() {
                        style.kerning = machine.kerning;
                    }
                    if machine.glyph_stretching.is_some() {
                        style.glyph_stretching = machine.glyph_stretching;
                    }
                    if machine.glyph_offset.is_some() {
                        style.glyph_offset = machine.glyph_offset;
                    }
                }
            }
        }

        Some(TypingInlineSelectionContext {
            char_range,
            text_byte_range,
            opening_wrapper_range,
            closing_wrapper_range,
            style,
        })
    }

    fn effective_inline_tag_style(
        &self,
        selection: &TypingInlineSelectionContext,
    ) -> TypingInlineTagStyle {
        let base_font_label = self
            .font_label_by_idx(self.selected_font_idx)
            .unwrap_or_else(|| "<шрифт>".to_string());
        TypingInlineTagStyle {
            bold: selection.style.bold || self.force_bold,
            italic: selection.style.italic || self.force_italic,
            font_label: Some(
                selection
                    .style
                    .font_label
                    .clone()
                    .unwrap_or(base_font_label),
            ),
            font_size_px: Some(selection.style.font_size_px.unwrap_or(self.font_size_px)),
            text_color: Some(selection.style.text_color.unwrap_or(self.text_color)),
            line_spacing: Some(selection.style.line_spacing.unwrap_or(self.line_spacing)),
            kerning: Some(selection.style.kerning.unwrap_or(self.kerning)),
            glyph_stretching: Some(
                selection
                    .style
                    .glyph_stretching
                    .unwrap_or([self.glyph_width, self.glyph_height]),
            ),
            glyph_offset: Some(
                selection
                    .style
                    .glyph_offset
                    .unwrap_or_else(|| TypingInlineOffsetStyle::global_only([0.0, 0.0])),
            ),
        }
    }

    fn apply_inline_style_to_selection(
        &mut self,
        selection: TypingInlineSelectionContext,
        desired_effective_style: TypingInlineTagStyle,
    ) -> bool {
        let desired_tag_style = self.normalize_desired_inline_tag_style(desired_effective_style);
        // По умолчанию панель пишет компактный машиночитаемый тег `<m ...>`.
        // Настройка `use_legacy_inline_tags` (пока не подключена к UI) вернёт обычные теги.
        let (opening_tags, closing_tags) = if self.use_legacy_inline_tags {
            (
                build_inline_opening_tags(&desired_tag_style),
                build_inline_closing_tags(&desired_tag_style),
            )
        } else {
            let opening = build_inline_machine_tag(&desired_tag_style);
            let closing = if opening.is_empty() {
                String::new()
            } else {
                "</m>".to_string()
            };
            (opening, closing)
        };

        let (new_text, new_selection_start_byte, new_selection_end_byte) = {
            let text = self.active_inline_text();
            let selected_text = text[selection.text_byte_range.clone()].to_string();
            let mut new_text = String::with_capacity(
                text.len()
                    + opening_tags.len()
                    + closing_tags.len()
                    + selection
                        .opening_wrapper_range
                        .len()
                        .saturating_sub(selection.closing_wrapper_range.len()),
            );
            new_text.push_str(&text[..selection.opening_wrapper_range.start]);
            new_text.push_str(&opening_tags);
            new_text.push_str(selected_text.as_str());
            new_text.push_str(&closing_tags);
            new_text.push_str(&text[selection.closing_wrapper_range.end..]);
            let start = selection.opening_wrapper_range.start + opening_tags.len();
            let end = start + selected_text.len();
            (new_text, start, end)
        };

        if new_text == self.active_inline_text() {
            return false;
        }

        self.set_active_inline_text(new_text);
        self.enable_inline_style_tags = true;
        self.pending_text_selection_restore = Some(
            byte_range_to_char_range(
                self.active_inline_text(),
                &(new_selection_start_byte..new_selection_end_byte),
            )
            .unwrap_or(selection.char_range),
        );
        self.queue_preview_render();
        true
    }

    fn normalize_desired_inline_tag_style(
        &self,
        desired_effective_style: TypingInlineTagStyle,
    ) -> TypingInlineTagStyle {
        let base_font_label = self.font_label_by_idx(self.selected_font_idx);
        let desired_font_label = desired_effective_style
            .font_label
            .map(|label| label.trim().to_string())
            .filter(|label| !label.is_empty());
        let font_label = desired_font_label.and_then(|label| {
            if base_font_label
                .as_deref()
                .is_some_and(|base| base.eq_ignore_ascii_case(label.as_str()))
            {
                None
            } else {
                Some(label)
            }
        });
        let font_size_px = desired_effective_style
            .font_size_px
            .map(|value| value.clamp(1.0, 256.0))
            .filter(|value| (value - self.font_size_px).abs() > 0.05);
        let text_color = desired_effective_style
            .text_color
            .filter(|value| *value != self.text_color);
        let line_spacing = desired_effective_style
            .line_spacing
            .map(|value| clamp_px_or_percent(value, 300.0))
            .filter(|value| px_or_percent_differs(*value, self.line_spacing));
        let kerning = desired_effective_style
            .kerning
            .map(|value| clamp_px_or_percent(value, 300.0))
            .filter(|value| px_or_percent_differs(*value, self.kerning));
        let glyph_stretching = desired_effective_style
            .glyph_stretching
            .map(|value| {
                [
                    clamp_stretch_px_or_percent(value[0]),
                    clamp_stretch_px_or_percent(value[1]),
                ]
            })
            .filter(|value| {
                px_or_percent_differs(value[0], self.glyph_width)
                    || px_or_percent_differs(value[1], self.glyph_height)
            });
        let glyph_offset = desired_effective_style
            .glyph_offset
            .map(normalize_inline_offset_style)
            .filter(inline_offset_style_is_non_default);

        TypingInlineTagStyle {
            bold: desired_effective_style.bold && !self.force_bold,
            italic: desired_effective_style.italic && !self.force_italic,
            font_label,
            font_size_px,
            text_color,
            line_spacing,
            kerning,
            glyph_stretching,
            glyph_offset,
        }
    }

    fn draw_image_transform_only_section(
        &mut self,
        ui: &mut egui::Ui,
        remap_wheel_to_horizontal: bool,
    ) -> bool {
        let mut changed = false;
        let mut block_hscroll_by_hovered_param = false;
        ui.vertical(|ui| {
            let scale_resp =
                ui.add(WheelSlider::new(&mut self.overlay_scale, 0.05..=20.0).text("Масштаб"));
            mark_hscroll_block_on_hover(&mut block_hscroll_by_hovered_param, &scale_resp);
            changed |= scale_resp.changed();
            if let Some(steps) = wheel_steps_if_hovered(ui, &scale_resp) {
                changed |= apply_wheel_step_f32(&mut self.overlay_scale, steps, 0.05, 0.05, 20.0);
            }

            let angle_resp = ui.add(
                WheelSlider::new(&mut self.overlay_rotation_deg, -180.0..=180.0).text("Угол (°)"),
            );
            mark_hscroll_block_on_hover(&mut block_hscroll_by_hovered_param, &angle_resp);
            changed |= angle_resp.changed();
            if let Some(steps) = wheel_steps_if_hovered(ui, &angle_resp) {
                changed |=
                    apply_wheel_step_f32(&mut self.overlay_rotation_deg, steps, 1.0, -180.0, 180.0);
            }
        });
        if remap_wheel_to_horizontal {
            apply_horizontal_wheel_scroll_if_idle(ui, block_hscroll_by_hovered_param);
        } else if block_hscroll_by_hovered_param {
            consume_wheel_scroll_delta(ui);
        }
        changed
    }

    fn load_from_selected_overlay(&mut self, selected: &TypingSelectedOverlayForEdit) {
        self.overlay_scale = selected.user_scale.max(0.05);
        self.overlay_rotation_deg = normalize_angle_deg(selected.rotation_deg);
        self.width_px = selected.width_px_hint.max(1);

        // Сбрасываем флаг ненайденного шрифта: его заново выставит
        // `apply_render_data_json_with_options`/`select_font_by_path_or_label`,
        // если шрифт нового оверлея отсутствует среди доступных.
        self.missing_font = None;

        // Сформированный текст персонален для оверлея: сбрасываем перед загрузкой,
        // чтобы он не «наследовался» от ранее выбранного оверлея.
        // `apply_render_data_json_with_options` восстановит его из JSON, если есть.
        self.formed_text.clear();
        self.advanced_text_show_formed = false;
        // Кэш окна форм относится к прошлому оверлею — инвалидируем.
        self.advanced_form_cache = None;
        if let Some(render_data) = selected.render_data_json.as_ref() {
            self.apply_render_data_json_with_options(render_data, true);
        }
        self.clamp_face_index();
    }

    fn sync_overlay_transform_from_selected_overlay(
        &mut self,
        selected: &TypingSelectedOverlayForEdit,
    ) {
        self.overlay_scale = selected.user_scale.max(0.05);
        self.overlay_rotation_deg = normalize_angle_deg(selected.rotation_deg);
    }

    fn apply_render_data_json_with_options(
        &mut self,
        render_data: &Value,
        apply_font_selection: bool,
    ) {
        let Some(render_data_obj) = render_data.as_object() else {
            return;
        };
        let Some(text_params_obj) = render_data_obj
            .get("text_params")
            .and_then(Value::as_object)
        else {
            return;
        };

        if let Some(text) = text_params_obj.get("text").and_then(Value::as_str) {
            self.text = text.to_string();
        }
        if let Some(text_color) = text_params_obj
            .get("text_color")
            .and_then(parse_color32_value)
        {
            self.text_color = text_color;
        }
        self.font_size_px = text_params_obj
            .get("font_size_px")
            .and_then(value_as_f32)
            .unwrap_or(self.font_size_px)
            .clamp(1.0, 256.0);
        self.line_spacing = clamp_px_or_percent(
            read_legacy_or_token_px_or_percent(
                text_params_obj,
                "line_spacing",
                "line_spacing_px",
                "line_spacing_percent",
                self.line_spacing,
            ),
            300.0,
        );
        self.kerning_mode = text_params_obj
            .get("kerning_mode")
            .and_then(Value::as_str)
            .and_then(parse_kerning_mode_str)
            .unwrap_or(KerningMode::Metric);
        self.kerning = clamp_px_or_percent(
            read_legacy_or_token_px_or_percent(
                text_params_obj,
                "kerning",
                "kerning_px",
                "kerning_percent",
                self.kerning,
            ),
            300.0,
        );
        self.glyph_height = clamp_stretch_px_or_percent(read_legacy_or_token_px_or_percent(
            text_params_obj,
            "glyph_height",
            "",
            "glyph_height_percent",
            self.glyph_height,
        ));
        self.glyph_width = clamp_stretch_px_or_percent(read_legacy_or_token_px_or_percent(
            text_params_obj,
            "glyph_width",
            "",
            "glyph_width_percent",
            self.glyph_width,
        ));
        self.width_px = text_params_obj
            .get("width_px")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(self.width_px)
            .max(1);
        if text_params_obj.get("align").is_some() || text_params_obj.get("align_bias").is_some() {
            self.align = HorizontalAlign::from_config(
                text_params_obj.get("align").and_then(Value::as_str),
                text_params_obj.get("align_bias").and_then(value_as_f32),
            );
        }
        if let Some(text_line_mode) = text_params_obj
            .get("text_line_mode")
            .and_then(Value::as_str)
            .and_then(parse_text_line_mode_str)
        {
            self.text_line_mode = text_line_mode;
        }
        if let Some(vertical_line_direction) = text_params_obj
            .get("vertical_line_direction")
            .and_then(Value::as_str)
            .and_then(parse_vertical_line_direction_str)
        {
            self.vertical_line_direction = vertical_line_direction;
        } else {
            self.vertical_line_direction = VerticalLineDirection::RightToLeft;
        }
        if let Some(text_layout_mode) = text_params_obj
            .get("text_layout_mode")
            .and_then(Value::as_str)
            .and_then(parse_text_layout_mode_str)
        {
            self.text_layout_mode = text_layout_mode;
        } else {
            self.text_layout_mode = TextLayoutMode::Normal;
        }
        if let Some(formula_obj) = text_params_obj
            .get("formula_layout")
            .and_then(Value::as_object)
        {
            if let Some(x_expr) = formula_obj.get("x_expr").and_then(Value::as_str) {
                self.formula_layout.x_expr = x_expr.to_string();
            }
            if let Some(y_expr) = formula_obj.get("y_expr").and_then(Value::as_str) {
                self.formula_layout.y_expr = y_expr.to_string();
            }
            if let Some(rotation_expr) = formula_obj.get("rotation_expr").and_then(Value::as_str) {
                self.formula_layout.rotation_expr = rotation_expr.to_string();
            }
            self.formula_layout.use_tangent_rotation = formula_obj
                .get("use_tangent_rotation")
                .and_then(Value::as_bool)
                .unwrap_or(self.formula_layout.use_tangent_rotation);
            self.formula_layout.t_start = formula_obj
                .get("t_start")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.t_start);
            self.formula_layout.t_end = formula_obj
                .get("t_end")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.t_end);
            self.formula_layout.offset_x_px = formula_obj
                .get("offset_x_px")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.offset_x_px);
            self.formula_layout.offset_y_px = formula_obj
                .get("offset_y_px")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.offset_y_px);
            self.formula_layout.scale_x = formula_obj
                .get("scale_x")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.scale_x);
            self.formula_layout.scale_y = formula_obj
                .get("scale_y")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.scale_y);
            self.formula_layout.normal_offset_px = formula_obj
                .get("normal_offset_px")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.normal_offset_px);
            self.formula_layout.letter_spacing_mul = formula_obj
                .get("letter_spacing_mul")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.letter_spacing_mul)
                .clamp(0.0, 8.0);
            self.formula_layout.letter_spacing_px = formula_obj
                .get("letter_spacing_px")
                .and_then(value_as_f32)
                .unwrap_or(self.formula_layout.letter_spacing_px)
                .clamp(-10_000.0, 10_000.0);
            if let Some(vars_arr) = formula_obj.get("vars").and_then(Value::as_array) {
                for (idx, value) in vars_arr
                    .iter()
                    .take(TEXT_FORMULA_USER_VAR_COUNT)
                    .enumerate()
                {
                    if let Some(parsed) = value_as_f32(value) {
                        self.formula_layout.vars[idx] = parsed;
                    }
                }
            }
        } else {
            self.formula_layout = TextFormulaLayoutParams::default();
        }
        self.drawn_lines_layout = text_params_obj
            .get("drawn_lines_layout")
            .and_then(text_drawn_lines_layout_from_value)
            .unwrap_or_default();
        self.vector_lines_layout = text_params_obj
            .get("vector_lines_layout")
            .and_then(text_vector_lines_layout_from_value)
            .unwrap_or_default();
        if let Some(shape_layout_obj) = text_params_obj
            .get("shape_layout")
            .and_then(Value::as_object)
        {
            self.apply_shape_layout_json(shape_layout_obj);
        } else {
            self.shape_layout_kind = TypingShapeLayoutKind::Arc;
            self.arc_shape_layout = TypingArcShapeLayoutParams::default();
            self.circle_shape_layout = TypingCircleShapeLayoutParams::default();
            self.spiral_shape_layout = TypingSpiralShapeLayoutParams::default();
            self.polygon_shape_layout = TypingPolygonShapeLayoutParams::default();
            self.zigzag_shape_layout = TypingZigzagShapeLayoutParams::default();
            self.s_curve_shape_layout = TypingSCurveShapeLayoutParams::default();
        }
        self.selected_face_idx = text_params_obj
            .get("selected_face_index")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(0usize);
        self.force_bold = text_params_obj
            .get("force_bold")
            .and_then(Value::as_bool)
            .unwrap_or(self.force_bold);
        self.force_italic = text_params_obj
            .get("force_italic")
            .and_then(Value::as_bool)
            .unwrap_or(self.force_italic);
        self.hanging_punctuation = text_params_obj
            .get("hanging_punctuation")
            .and_then(Value::as_bool)
            .unwrap_or(self.hanging_punctuation);
        self.trim_extra_spaces = text_params_obj
            .get("trim_extra_spaces")
            .and_then(Value::as_bool)
            .unwrap_or(self.trim_extra_spaces);
        self.new_line_after_sentence = text_params_obj
            .get("new_line_after_sentence")
            .and_then(Value::as_bool)
            .unwrap_or(self.new_line_after_sentence);
        self.enable_inline_style_tags = text_params_obj
            .get("enable_inline_style_tags")
            .and_then(Value::as_bool)
            .unwrap_or(self.enable_inline_style_tags);
        self.uppercase_text = text_params_obj
            .get("uppercase_text")
            .and_then(Value::as_bool)
            .unwrap_or(self.uppercase_text);
        if let Some(shape) = text_params_obj
            .get("text_shape")
            .and_then(Value::as_str)
            .and_then(parse_text_shape_str)
        {
            self.text_shape = shape;
        }
        if let Some(wrap_mode) = text_params_obj
            .get("text_wrap_mode")
            .and_then(Value::as_str)
            .and_then(parse_text_wrap_mode_str)
        {
            self.text_wrap_mode = wrap_mode;
        }
        // Сформированный текст (если был применён «продвинутый» перенос).
        // Разворачиваем сформированный, если он есть, иначе исходный.
        self.formed_text = text_params_obj
            .get("formed_text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.advanced_text_show_formed = !self.formed_text.trim().is_empty();
        self.allow_moderate_trees = text_params_obj
            .get("allow_moderate_trees")
            .and_then(Value::as_bool)
            .unwrap_or(self.allow_moderate_trees);
        self.sync_wrap_mode_constraints();
        self.shape_min_width_percent = text_params_obj
            .get("shape_min_width_percent")
            .and_then(value_as_f32)
            .unwrap_or(self.shape_min_width_percent)
            .clamp(5.0, 100.0);
        self.shape_variant = text_params_obj
            .get("shape_variant")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(self.shape_variant)
            .clamp(1, 9);

        if apply_font_selection {
            let font_path = text_params_obj
                .get("font_path")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let font_label = text_params_obj
                .get("font_label")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            self.select_font_by_path_or_label(font_path, font_label);
        }

        self.effects = render_data_obj
            .get("effects")
            .and_then(Value::as_array)
            .map(|effects| parse_effect_cards(effects, self.text_color))
            .unwrap_or_default();
        self.sync_selected_formula_preset_by_layout();
    }

    fn select_font_by_path_or_label(&mut self, font_path: Option<&str>, font_label: Option<&str>) {
        if let Some(idx) = self.find_font_idx_by_path_or_label(font_path, font_label) {
            self.selected_font_idx = idx;
            self.active_font_key = self.current_font_key();
            self.missing_font = None;
        } else {
            // Шрифт оверлея отсутствует среди доступных: запоминаем его имя, чтобы
            // показать предупреждение и заблокировать рендер до выбора другого шрифта.
            let name = font_label
                .map(str::to_string)
                .or_else(|| {
                    font_path.map(|path| {
                        Path::new(path)
                            .file_name()
                            .and_then(|stem| stem.to_str())
                            .unwrap_or(path)
                            .to_string()
                    })
                })
                .unwrap_or_else(|| "<неизвестный шрифт>".to_string());
            self.missing_font = Some(name);
        }
    }

    fn queue_preview_render(&mut self) {
        if !self.preview_enabled {
            return;
        }
        let Some(params) = self.build_render_params() else {
            self.render_in_flight = false;
            self.status_line = format!("Шрифты не найдены в {}", self.fonts_dir.display());
            return;
        };

        self.latest_token = self.latest_token.saturating_add(1);
        crate::trace_log!(
            cat::SYNC,
            "preview_render dispatch token={} layout={:?} line_mode={:?} width_px={} preempting_inflight={}",
            self.latest_token,
            params.text_layout_mode,
            params.text_line_mode,
            params.width_px,
            self.render_in_flight
        );
        let job = PreviewRenderJob {
            token: self.latest_token,
            params,
        };
        match self.request_tx.send(job) {
            Ok(()) => {
                self.render_in_flight = true;
                self.status_line = "Рендер в фоне...".to_string();
            }
            Err(err) => {
                crate::trace_log!(cat::SYNC, "preview_render dispatch send_err token={} err={}", self.latest_token, err);
                self.render_in_flight = false;
                self.status_line = format!("Не удалось отправить задачу рендера: {err}");
            }
        }
    }

    fn poll_preview_render_results(&mut self, ctx: &egui::Context) {
        if !self.preview_enabled {
            return;
        }
        let mut has_updates = false;
        while let Ok(result) = self.result_rx.try_recv() {
            if result.token != self.latest_token {
                crate::trace_log!(
                    cat::SYNC,
                    "preview_render result=stale_dropped token={} latest={}",
                    result.token,
                    self.latest_token
                );
                continue;
            }
            has_updates = true;
            self.render_in_flight = false;
            match result.image {
                Ok(image) => {
                    crate::trace_log!(
                        cat::SYNC,
                        "preview_render result=ok token={} size={}x{}",
                        result.token,
                        image.width,
                        image.height
                    );
                    self.preview_size = [image.width as usize, image.height as usize];
                    let color_image = ColorImage::from_rgba_unmultiplied(
                        self.preview_size,
                        image.rgba.as_slice(),
                    );
                    if let Some(texture) = &mut self.preview_texture {
                        texture.set(color_image, TextureOptions::LINEAR);
                    } else {
                        self.preview_texture = Some(ctx.load_texture(
                            PREVIEW_TEXTURE_ID,
                            color_image,
                            TextureOptions::LINEAR,
                        ));
                    }
                    self.status_line = if image.warnings.is_empty() {
                        "Рендер завершён".to_string()
                    } else {
                        format!("Рендер с предупреждением: {}", image.warnings.join("; "))
                    };
                }
                Err(err) => {
                    crate::trace_log!(cat::SYNC, "preview_render result=err token={} err={}", result.token, err);
                    self.status_line = format!("Ошибка рендера: {err}");
                }
            }
        }
        if has_updates {
            ctx.request_repaint();
        }
    }

    /// В рендер идёт сформированный текст, если он не пуст, иначе исходный.
    fn uses_formed_text(&self) -> bool {
        !self.formed_text.trim().is_empty()
    }

    fn effective_render_text(&self) -> String {
        if self.uses_formed_text() {
            self.formed_text.clone()
        } else {
            self.text.clone()
        }
    }

    fn build_render_params(&self) -> Option<TextRenderParams> {
        self.build_render_params_for(self.effective_render_text(), self.width_px.max(1))
    }

    fn adjust_font_size_by_wheel_steps(&mut self, steps: i32) -> bool {
        if steps == 0 {
            return false;
        }
        if !apply_wheel_step_f32(&mut self.font_size_px, steps, 1.0, 1.0, 256.0) {
            return false;
        }
        self.sync_current_font_profile_memory();
        self.queue_preview_render();
        true
    }

    fn editor_font_spec(&self) -> Option<TypingEditorFontSpec> {
        let font = self.fonts.get(self.selected_font_idx)?;
        let face_index = font
            .faces
            .get(self.selected_face_idx)
            .map(|face| face.face_index)
            .unwrap_or(0usize);
        Some(TypingEditorFontSpec {
            font_path: font.path.clone(),
            face_index,
            ui_font_size_px: self.font_size_px.clamp(8.0, 128.0),
        })
    }

    fn build_render_params_for(&self, text: String, width_px: u32) -> Option<TextRenderParams> {
        let font = self.fonts.get(self.selected_font_idx)?;
        let selected_face_index = font
            .faces
            .get(self.selected_face_idx)
            .map(|face| face.face_index)
            .unwrap_or(0usize);
        let formula_layout = self.formula_layout_for_render();
        let drawn_lines_layout = self.drawn_lines_layout_for_render();
        let vector_lines_layout = self.vector_lines_layout.clone();
        let available_inline_fonts = self.available_inline_fonts();

        Some(TextRenderParams {
            text,
            text_color: [
                self.text_color.r(),
                self.text_color.g(),
                self.text_color.b(),
                self.text_color.a(),
            ],
            font_path: font.path.clone(),
            available_inline_fonts,
            font_size_px: self.font_size_px.max(1.0),
            line_spacing_px: self.line_spacing.as_px_percent().0,
            line_spacing_percent: self.line_spacing.as_px_percent().1,
            kerning_mode: self.kerning_mode,
            kerning_px: self.kerning.as_px_percent().0,
            kerning_percent: self.kerning.as_px_percent().1,
            glyph_height_percent: self.glyph_height.as_percent_of(self.font_size_px.max(1.0)),
            glyph_width_percent: self.glyph_width.as_percent_of(self.font_size_px.max(1.0)),
            width_px: width_px.max(1),
            align: self.align,
            text_line_mode: self.text_line_mode,
            vertical_line_direction: self.vertical_line_direction,
            text_layout_mode: self.text_layout_mode,
            formula_layout,
            drawn_lines_layout,
            vector_lines_layout,
            selected_face_index,
            force_bold: self.force_bold,
            force_italic: self.force_italic,
            uppercase_text: self.uppercase_text,
            trim_extra_spaces: self.trim_extra_spaces,
            hanging_punctuation: self.hanging_punctuation,
            new_line_after_sentence: self.new_line_after_sentence,
            enable_inline_style_tags: self.enable_inline_style_tags,
            // Сформированный текст уже разбит на строки — не переносим заново.
            text_wrap_mode: if self.uses_formed_text() {
                TextWrapMode::None
            } else {
                self.text_wrap_mode
            },
            text_shape: self.text_shape,
            shape_min_width_percent: self.shape_min_width_percent,
            shape_variant: self.shape_variant,
            compare_shape_with: None,
            allow_moderate_trees: self.allow_moderate_trees,
            effects_json: self.effects_json(),
        })
    }

    fn available_inline_fonts(&self) -> Vec<InlineFontEntry> {
        self.fonts
            .iter()
            .map(|font| InlineFontEntry {
                label: font.label.clone(),
                font_path: font.path.clone(),
                face_index: font.faces.first().map(|face| face.face_index).unwrap_or(0),
            })
            .collect()
    }

    fn sync_wrap_mode_constraints(&mut self) {
        if !self.moderate_trees_checkbox_enabled() {
            self.allow_moderate_trees = false;
        }
    }

    fn moderate_trees_checkbox_enabled(&self) -> bool {
        matches!(
            self.text_wrap_mode,
            TextWrapMode::WholeWords | TextWrapMode::Minimal
        )
    }
}

fn clamp_char_range(text: &str, range: Range<usize>) -> Range<usize> {
    let text_char_count = text.chars().count();
    let start = range.start.min(text_char_count);
    let end = range.end.min(text_char_count);
    start.min(end)..end.max(start)
}

fn cycle_wrapped_index_in_values(current: &mut usize, values: &[usize], steps: i32) {
    if steps == 0 || values.is_empty() {
        return;
    }
    let current_pos = values
        .iter()
        .position(|value| value == current)
        .unwrap_or(0);
    let mut next_pos = i32::try_from(current_pos).unwrap_or(0) + steps;
    let values_len = i32::try_from(values.len()).unwrap_or(0);
    while next_pos < 0 {
        next_pos += values_len;
    }
    while next_pos >= values_len {
        next_pos -= values_len;
    }
    if let Some(next_value) = usize::try_from(next_pos)
        .ok()
        .and_then(|idx| values.get(idx))
        .copied()
    {
        *current = next_value;
    }
}

fn char_range_to_byte_range(text: &str, range: &Range<usize>) -> Option<Range<usize>> {
    let clamped = clamp_char_range(text, range.clone());
    let start = char_index_to_byte_index(text, clamped.start)?;
    let end = char_index_to_byte_index(text, clamped.end)?;
    Some(start..end)
}

fn byte_range_to_char_range(text: &str, range: &Range<usize>) -> Option<Range<usize>> {
    let start = byte_index_to_char_index(text, range.start)?;
    let end = byte_index_to_char_index(text, range.end)?;
    Some(start..end)
}

fn char_index_to_byte_index(text: &str, char_index: usize) -> Option<usize> {
    let char_count = text.chars().count();
    if char_index > char_count {
        return None;
    }
    if char_index == char_count {
        return Some(text.len());
    }
    text.char_indices()
        .nth(char_index)
        .map(|(byte_index, _)| byte_index)
}

fn byte_index_to_char_index(text: &str, byte_index: usize) -> Option<usize> {
    if byte_index > text.len() || !text.is_char_boundary(byte_index) {
        return None;
    }
    Some(text[..byte_index].chars().count())
}

/// `(min, max)` значений итератора; `(0, 0)` для пустого. `Default` даёт ноль
/// для числовых типов.
fn inclusive_bounds<T: Ord + Copy + Default>(values: impl Iterator<Item = T>) -> (T, T) {
    let mut iter = values;
    let Some(first) = iter.next() else {
        return (T::default(), T::default());
    };
    let mut lo = first;
    let mut hi = first;
    for value in iter {
        if value < lo {
            lo = value;
        }
        if value > hi {
            hi = value;
        }
    }
    (lo, hi)
}

/// Строка фильтра-диапазона `(от, до)` для окна форм. Не рисуется, если границы
/// схлопнуты (`bounds.0 >= bounds.1`) — фильтровать нечего. Возвращает `true`,
/// если строка была показана.
fn advanced_form_range_row<T>(
    ui: &mut egui::Ui,
    label: &str,
    suffix: &str,
    value: &mut (T, T),
    bounds: (T, T),
) -> bool
where
    T: egui::emath::Numeric + Ord + Copy,
{
    if bounds.0 >= bounds.1 {
        // Все формы имеют одно значение — фильтр бессмыслен; держим диапазон полным.
        *value = bounds;
        return false;
    }
    value.0 = value.0.clamp(bounds.0, bounds.1);
    value.1 = value.1.clamp(bounds.0, bounds.1);
    if value.0 > value.1 {
        value.0 = value.1;
    }
    // Шаг колеса/перетаскивания ~1/100 диапазона, чтобы крупные пиксельные
    // ширины не приходилось крутить по единице, а мелкие счётчики шли точно.
    let span = bounds.1.to_f64() - bounds.0.to_f64();
    let step = (span / 100.0).max(1.0);
    ui.horizontal(|ui| {
        ui.label(label);
        let hi_now = value.1;
        ui.add(
            WheelSpinBox::new(&mut value.0)
                .range(bounds.0..=hi_now)
                .wheel_step(step)
                .speed(step)
                .suffix(suffix),
        );
        ui.label("–");
        let lo_now = value.0;
        ui.add(
            WheelSpinBox::new(&mut value.1)
                .range(lo_now..=bounds.1)
                .wheel_step(step)
                .speed(step)
                .suffix(suffix),
        );
    });
    true
}

/// Сортировка форм для окна: узкие → широкие; в пределах допуска по ширине —
/// по ровности (меньшая неравномерность раньше), затем по цене разрывов,
/// пиковости и числу переносов.
fn sort_advanced_forms(forms: &mut [TextForm]) {
    forms.sort_by(|a, b| a.max_width.cmp(&b.max_width));
    let mut i = 0;
    while i < forms.len() {
        let run_min = forms[i].max_width;
        let mut j = i + 1;
        while j < forms.len() && forms[j].max_width <= run_min + forms::DEFAULT_WIDTH_TOLERANCE {
            j += 1;
        }
        forms[i..j].sort_by(|a, b| {
            a.conservatism
                .cmp(&b.conservatism)
                .then(a.unevenness_pct.cmp(&b.unevenness_pct))
                .then(a.break_cost.cmp(&b.break_cost))
                .then(a.max_width.cmp(&b.max_width))
                .then(a.peakiness_pct(PeakBase::Min).cmp(&b.peakiness_pct(PeakBase::Min)))
                .then(a.word_break_count.cmp(&b.word_break_count))
        });
        i = j;
    }
}

/// Рисует одну карточку формы: чёрный текст на белом, строки центрированы по
/// «ядру», висящая пунктуация выходит за края. Возвращает отклик клика.
fn draw_advanced_form_card(
    ui: &mut egui::Ui,
    font_id: &egui::FontId,
    lines: &[String],
) -> egui::Response {
    const PAD_PX: f32 = 8.0;
    let row_height = ui.fonts_mut(|fonts| fonts.row_height(font_id));

    struct CardRow {
        lead: Arc<egui::Galley>,
        core: Arc<egui::Galley>,
        trail: Arc<egui::Galley>,
        core_w: f32,
        lead_w: f32,
    }

    let mut rows: Vec<CardRow> = Vec::with_capacity(lines.len());
    let mut half_extent = PAD_PX;
    for line in lines {
        let (lead_text, core_text, trail_text) = forms::split_hanging_edges(line);
        let (lead, core, trail) = ui.fonts_mut(|fonts| {
            (
                fonts.layout_no_wrap(lead_text, font_id.clone(), Color32::BLACK),
                fonts.layout_no_wrap(core_text, font_id.clone(), Color32::BLACK),
                fonts.layout_no_wrap(trail_text, font_id.clone(), Color32::BLACK),
            )
        });
        let core_w = core.size().x;
        let lead_w = lead.size().x;
        let trail_w = trail.size().x;
        half_extent = half_extent
            .max(core_w / 2.0 + lead_w)
            .max(core_w / 2.0 + trail_w);
        rows.push(CardRow {
            lead,
            core,
            trail,
            core_w,
            lead_w,
        });
    }

    let card_w = (half_extent * 2.0 + PAD_PX * 2.0).max(48.0);
    let card_h = PAD_PX * 2.0 + row_height * lines.len().max(1) as f32;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(card_w, card_h), egui::Sense::click());

    let hovered = response.hovered();
    let painter = ui.painter();
    let bg = if hovered {
        Color32::from_gray(244)
    } else {
        Color32::WHITE
    };
    painter.rect_filled(rect, 4.0, bg);
    let border = if hovered {
        Color32::from_rgb(90, 140, 220)
    } else {
        Color32::from_gray(170)
    };
    painter.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );

    let center_x = rect.center().x;
    let mut y = rect.top() + PAD_PX;
    for row in rows {
        let core_x0 = center_x - row.core_w / 2.0;
        painter.galley(
            egui::pos2(core_x0 - row.lead_w, y),
            row.lead,
            Color32::BLACK,
        );
        painter.galley(egui::pos2(core_x0, y), row.core, Color32::BLACK);
        painter.galley(
            egui::pos2(core_x0 + row.core_w, y),
            row.trail,
            Color32::BLACK,
        );
        y += row_height;
    }

    response
}

fn build_inline_tag_editor_text_colors(text: &str) -> Vec<TextEditPlusTextColor> {
    let mut content_styles = Vec::new();
    let mut tag_styles = Vec::new();
    let mut stack = Vec::<TypingInlineTagToken>::new();
    let mut cursor = 0usize;

    while cursor < text.len() {
        let Some(relative_start) = text[cursor..].find('<') else {
            break;
        };
        let tag_start = cursor + relative_start;
        let Some(relative_end) = text[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + relative_end + 1;
        let raw = &text[tag_start + 1..tag_end - 1];

        if let Some(kind) = parse_opening_inline_tag(raw) {
            push_editor_text_color(
                text,
                tag_start..tag_end,
                INLINE_TAG_DIM_TEXT_COLOR,
                &mut tag_styles,
            );
            stack.push(TypingInlineTagToken {
                byte_range: tag_start..tag_end,
                kind,
            });
        } else if let Some(kind) = parse_closing_inline_tag(raw) {
            push_editor_text_color(
                text,
                tag_start..tag_end,
                INLINE_TAG_DIM_TEXT_COLOR,
                &mut tag_styles,
            );
            if let Some(open_idx) = stack
                .iter()
                .rposition(|open_tag| inline_tag_kinds_match(&open_tag.kind, &kind))
            {
                let open_tag = stack.remove(open_idx);
                push_editor_text_color(
                    text,
                    open_tag.byte_range.end..tag_start,
                    INLINE_TAG_CONTENT_TEXT_COLOR,
                    &mut content_styles,
                );
            }
        }

        cursor = tag_end;
    }

    let mut styles = content_styles;
    styles.extend(tag_styles);
    styles
}

fn push_editor_text_color(
    text: &str,
    byte_range: Range<usize>,
    color: Color32,
    out: &mut Vec<TextEditPlusTextColor>,
) {
    if byte_range.is_empty() {
        return;
    }
    let Some(char_start) = byte_index_to_char_index(text, byte_range.start) else {
        return;
    };
    let Some(char_end) = byte_index_to_char_index(text, byte_range.end) else {
        return;
    };
    if char_start < char_end {
        out.push(TextEditPlusTextColor::new(char_start..char_end, color));
    }
}

fn collect_adjacent_opening_inline_tags(
    text: &str,
    selection_start: usize,
) -> Vec<TypingInlineTagToken> {
    let mut out = Vec::new();
    let mut cursor = selection_start;
    while cursor > 0 {
        let Some(raw_start) = text[..cursor].rfind('<') else {
            break;
        };
        if !text[raw_start..cursor].ends_with('>') {
            break;
        }
        let raw = &text[raw_start + 1..cursor - 1];
        let Some(kind) = parse_opening_inline_tag(raw) else {
            break;
        };
        out.push(TypingInlineTagToken {
            byte_range: raw_start..cursor,
            kind,
        });
        cursor = raw_start;
    }
    out
}

fn collect_adjacent_closing_inline_tags(
    text: &str,
    selection_end: usize,
) -> Vec<TypingInlineTagToken> {
    let mut out = Vec::new();
    let mut cursor = selection_end;
    while cursor < text.len() {
        let rest = &text[cursor..];
        if !rest.starts_with('<') {
            break;
        }
        let Some(rel_end) = rest.find('>') else {
            break;
        };
        let tag_end = cursor + rel_end + 1;
        let raw = &text[cursor + 1..tag_end - 1];
        let Some(kind) = parse_closing_inline_tag(raw) else {
            break;
        };
        out.push(TypingInlineTagToken {
            byte_range: cursor..tag_end,
            kind,
        });
        cursor = tag_end;
    }
    out
}

fn parse_opening_inline_tag(raw: &str) -> Option<TypingInlineTagKind> {
    let compact = raw
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    match compact.as_str() {
        "b" | "strong" => return Some(TypingInlineTagKind::Bold),
        "i" | "em" => return Some(TypingInlineTagKind::Italic),
        _ => {}
    }

    if let Some(style) = parse_machine_tag_style(raw) {
        return Some(TypingInlineTagKind::Machine(style));
    }

    if let Some((tag_name, value)) = raw.split_once('=')
        && tag_name.trim().eq_ignore_ascii_case("font")
    {
        let label = value
            .trim()
            .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
            .trim();
        if !label.is_empty() {
            return Some(TypingInlineTagKind::Font(label.to_string()));
        }
    }

    if let Some((tag_name, value)) = raw.split_once('=')
        && tag_name.trim().eq_ignore_ascii_case("size")
    {
        let value = value
            .trim()
            .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
            .trim()
            .strip_suffix("px")
            .unwrap_or(value)
            .trim();
        if let Ok(parsed) = value.parse::<f32>()
            && parsed.is_finite()
            && parsed > 0.0
        {
            return Some(TypingInlineTagKind::Size(parsed));
        }
    }

    if let Some((tag_name, value)) = raw.split_once('=')
        && tag_name.trim().eq_ignore_ascii_case("color")
        && let Some(color) = parse_inline_hex_color(value)
    {
        return Some(TypingInlineTagKind::Color(color));
    }

    if let Some(value) = parse_inline_value_or_legacy_pair(raw, "line-spacing", 300.0) {
        return Some(TypingInlineTagKind::LineSpacing(value));
    }

    if let Some(value) = parse_inline_value_or_legacy_pair(raw, "kerning", 300.0) {
        return Some(TypingInlineTagKind::Kerning(value));
    }

    if let Some(value) = parse_inline_stretch_value(raw) {
        return Some(TypingInlineTagKind::Stretching(value));
    }

    if let Some(offset) = parse_inline_offset_value(raw) {
        return Some(TypingInlineTagKind::Offset(offset));
    }

    None
}

fn parse_closing_inline_tag(raw: &str) -> Option<TypingInlineTagKind> {
    let compact = raw
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    match compact.as_str() {
        "/b" | "/strong" => Some(TypingInlineTagKind::Bold),
        "/i" | "/em" => Some(TypingInlineTagKind::Italic),
        "/font" => Some(TypingInlineTagKind::Font(String::new())),
        "/size" => Some(TypingInlineTagKind::Size(0.0)),
        "/color" => Some(TypingInlineTagKind::Color(Color32::TRANSPARENT)),
        "/line-spacing" => Some(TypingInlineTagKind::LineSpacing(PxOrPercent::percent(0.0))),
        "/kerning" => Some(TypingInlineTagKind::Kerning(PxOrPercent::percent(0.0))),
        "/stretching" => Some(TypingInlineTagKind::Stretching([
            PxOrPercent::percent(100.0),
            PxOrPercent::percent(100.0),
        ])),
        "/offset" => Some(TypingInlineTagKind::Offset(
            TypingInlineOffsetStyle::global_only([0.0, 0.0]),
        )),
        "/m" => Some(TypingInlineTagKind::Machine(TypingInlineTagStyle::default())),
        _ => None,
    }
}

fn inline_tag_kinds_match(left: &TypingInlineTagKind, right: &TypingInlineTagKind) -> bool {
    matches!(
        (left, right),
        (TypingInlineTagKind::Bold, TypingInlineTagKind::Bold)
            | (TypingInlineTagKind::Italic, TypingInlineTagKind::Italic)
            | (TypingInlineTagKind::Font(_), TypingInlineTagKind::Font(_))
            | (TypingInlineTagKind::Size(_), TypingInlineTagKind::Size(_))
            | (TypingInlineTagKind::Color(_), TypingInlineTagKind::Color(_))
            | (
                TypingInlineTagKind::LineSpacing(_),
                TypingInlineTagKind::LineSpacing(_)
            )
            | (
                TypingInlineTagKind::Kerning(_),
                TypingInlineTagKind::Kerning(_)
            )
            | (
                TypingInlineTagKind::Stretching(_),
                TypingInlineTagKind::Stretching(_)
            )
            | (
                TypingInlineTagKind::Offset(_),
                TypingInlineTagKind::Offset(_)
            )
            | (
                TypingInlineTagKind::Machine(_),
                TypingInlineTagKind::Machine(_)
            )
    )
}

/// Собрать машиночитаемый тег `<m ...>` (см. контракт ключей в `parse_machine_tag`).
/// Возвращает пустую строку, если стиль ничего не задаёт.
fn build_inline_machine_tag(style: &TypingInlineTagStyle) -> String {
    let mut out = String::from("<m");
    if style.bold {
        out.push_str(" b");
    }
    if style.italic {
        out.push_str(" i");
    }
    if let Some(font_label) = style.font_label.as_deref() {
        let sanitized = font_label.replace(['"', '<', '>'], "");
        out.push_str(format!(" f=\"{sanitized}\"").as_str());
    }
    if let Some(font_size_px) = style.font_size_px {
        out.push_str(format!(" s={font_size_px:.2}").as_str());
    }
    if let Some(color) = style.text_color {
        out.push_str(
            format!(
                " c={:02X}{:02X}{:02X}{:02X}",
                color.r(),
                color.g(),
                color.b(),
                color.a()
            )
            .as_str(),
        );
    }
    if let Some(line_spacing) = style.line_spacing {
        out.push_str(format!(" l={}", line_spacing.to_token()).as_str());
    }
    if let Some(kerning) = style.kerning {
        out.push_str(format!(" k={}", kerning.to_token()).as_str());
    }
    if let Some([stretch_x, stretch_y]) = style.glyph_stretching {
        out.push_str(format!(" w={} h={}", stretch_x.to_token(), stretch_y.to_token()).as_str());
    }
    if let Some(offset) = style.glyph_offset {
        if offset.global_x.value != 0.0 {
            out.push_str(format!(" x={}", offset.global_x.to_token()).as_str());
        }
        if offset.global_y.value != 0.0 {
            out.push_str(format!(" y={}", offset.global_y.to_token()).as_str());
        }
        if offset.line.value != 0.0 {
            out.push_str(format!(" n={}", offset.line.to_token()).as_str());
        }
        if offset.shift_following {
            out.push_str(" q");
        }
        if offset.group_rotation_deg != 0.0 {
            out.push_str(format!(" g={:.2}", offset.group_rotation_deg).as_str());
        }
        if offset.glyph_rotation_deg != 0.0 {
            out.push_str(format!(" r={:.2}", offset.glyph_rotation_deg).as_str());
        }
    }
    out.push('>');
    if out == "<m>" { String::new() } else { out }
}

fn build_inline_opening_tags(style: &TypingInlineTagStyle) -> String {
    let mut out = String::new();
    if let Some(font_label) = style.font_label.as_deref() {
        out.push_str(format!("<font={font_label}>").as_str());
    }
    if let Some(font_size_px) = style.font_size_px {
        out.push_str(format!("<size={font_size_px:.2}>").as_str());
    }
    if let Some(text_color) = style.text_color {
        out.push_str(format_inline_color_tag(text_color).as_str());
    }
    if let Some(line_spacing) = style.line_spacing {
        out.push_str(format!("<line-spacing={}>", line_spacing.to_token()).as_str());
    }
    if let Some(kerning) = style.kerning {
        out.push_str(format!("<kerning={}>", kerning.to_token()).as_str());
    }
    if let Some([stretch_x, stretch_y]) = style.glyph_stretching {
        out.push_str(
            format!("<stretching={},{}>", stretch_x.to_token(), stretch_y.to_token()).as_str(),
        );
    }
    if let Some(offset) = style.glyph_offset {
        out.push_str(format_inline_offset_tag(offset).as_str());
    }
    if style.bold {
        out.push_str("<b>");
    }
    if style.italic {
        out.push_str("<i>");
    }
    out
}

fn build_inline_closing_tags(style: &TypingInlineTagStyle) -> String {
    let mut out = String::new();
    if style.italic {
        out.push_str("</i>");
    }
    if style.bold {
        out.push_str("</b>");
    }
    if style.glyph_offset.is_some() {
        out.push_str("</offset>");
    }
    if style.glyph_stretching.is_some() {
        out.push_str("</stretching>");
    }
    if style.kerning.is_some() {
        out.push_str("</kerning>");
    }
    if style.line_spacing.is_some() {
        out.push_str("</line-spacing>");
    }
    if style.text_color.is_some() {
        out.push_str("</color>");
    }
    if style.font_size_px.is_some() {
        out.push_str("</size>");
    }
    if style.font_label.is_some() {
        out.push_str("</font>");
    }
    out
}

fn format_inline_offset_tag(offset: TypingInlineOffsetStyle) -> String {
    format!(
        "<offset={},{},{},{},{:.2},{:.2}>",
        offset.global_x.to_token(),
        offset.global_y.to_token(),
        offset.line.to_token(),
        if offset.shift_following { 1 } else { 0 },
        offset.group_rotation_deg,
        offset.glyph_rotation_deg
    )
}

fn clamp_px_or_percent(value: PxOrPercent, limit: f32) -> PxOrPercent {
    PxOrPercent {
        value: value.value.clamp(-limit, limit),
        is_percent: value.is_percent,
    }
}

/// Считаются ли два значения различающимися (по единице или по величине).
fn px_or_percent_differs(left: PxOrPercent, right: PxOrPercent) -> bool {
    left.is_percent != right.is_percent || (left.value - right.value).abs() > 0.05
}

/// Прочитать параметр `px-или-%`: сначала новый строковый ключ-токен, затем
/// устаревшие отдельные ключи `*_px`/`*_percent` (с приоритетом пикселей).
fn read_legacy_or_token_px_or_percent(
    obj: &serde_json::Map<String, Value>,
    token_key: &str,
    legacy_px_key: &str,
    legacy_percent_key: &str,
    default: PxOrPercent,
) -> PxOrPercent {
    if let Some(value) = obj.get(token_key) {
        if let Some(text) = value.as_str() {
            if let Some(parsed) = PxOrPercent::parse(text) {
                return parsed;
            }
        } else if let Some(number) = value_as_f32(value) {
            // Голое число в ключе-токене встречается лишь в легаси `line_spacing`,
            // где оно означало пиксели.
            return PxOrPercent::px(number);
        }
    }
    let legacy_px = obj.get(legacy_px_key).and_then(value_as_f32);
    let legacy_percent = obj.get(legacy_percent_key).and_then(value_as_f32);
    if legacy_px.is_some() || legacy_percent.is_some() {
        return PxOrPercent::from_legacy_pair(
            legacy_px.unwrap_or(0.0),
            legacy_percent.unwrap_or(0.0),
        );
    }
    default
}

fn normalize_inline_offset_style(offset: TypingInlineOffsetStyle) -> TypingInlineOffsetStyle {
    TypingInlineOffsetStyle {
        global_x: clamp_px_or_percent(offset.global_x, 100.0),
        global_y: clamp_px_or_percent(offset.global_y, 100.0),
        line: clamp_px_or_percent(offset.line, 300.0),
        shift_following: offset.shift_following,
        group_rotation_deg: offset.group_rotation_deg.clamp(-180.0, 180.0),
        glyph_rotation_deg: offset.glyph_rotation_deg.clamp(-180.0, 180.0),
    }
}

fn inline_offset_style_is_non_default(offset: &TypingInlineOffsetStyle) -> bool {
    offset.global_x.value.abs() > 0.05
        || offset.global_y.value.abs() > 0.05
        || offset.line.value.abs() > 0.05
        || offset.shift_following
        || offset.group_rotation_deg.abs() > 0.05
        || offset.glyph_rotation_deg.abs() > 0.05
}

fn format_inline_color_tag(color: Color32) -> String {
    format!(
        "<color=#{:02X}{:02X}{:02X}{:02X}>",
        color.r(),
        color.g(),
        color.b(),
        color.a()
    )
}

fn parse_inline_hex_color(value: &str) -> Option<Color32> {
    let hex = value
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
        .trim()
        .strip_prefix('#')
        .unwrap_or(value.trim())
        .trim();
    match hex.len() {
        6 => {
            let rgb = u32::from_str_radix(hex, 16).ok()?;
            Some(Color32::from_rgba_unmultiplied(
                u8::try_from((rgb >> 16) & 0xFF).ok()?,
                u8::try_from((rgb >> 8) & 0xFF).ok()?,
                u8::try_from(rgb & 0xFF).ok()?,
                255,
            ))
        }
        8 => {
            let rgba = u32::from_str_radix(hex, 16).ok()?;
            Some(Color32::from_rgba_unmultiplied(
                u8::try_from((rgba >> 24) & 0xFF).ok()?,
                u8::try_from((rgba >> 16) & 0xFF).ok()?,
                u8::try_from((rgba >> 8) & 0xFF).ok()?,
                u8::try_from(rgba & 0xFF).ok()?,
            ))
        }
        _ => None,
    }
}

/// Разобрать машиночитаемый тег `<m ...>` в полный inline-стиль панели.
fn parse_machine_tag_style(raw: &str) -> Option<TypingInlineTagStyle> {
    let attrs = parse_machine_tag(raw)?;
    let mut style = TypingInlineTagStyle::default();
    let mut offset = TypingInlineOffsetStyle::global_only([0.0, 0.0]);
    let mut has_offset = false;
    let mut stretch_w: Option<PxOrPercent> = None;
    let mut stretch_h: Option<PxOrPercent> = None;

    for (key, value) in &attrs {
        match key {
            'b' => style.bold = true,
            'i' => style.italic = true,
            'f' => {
                let label = value.trim();
                if !label.is_empty() {
                    style.font_label = Some(label.to_string());
                }
            }
            's' => {
                if let Ok(px) = value.trim().parse::<f32>()
                    && px.is_finite()
                    && px > 0.0
                {
                    style.font_size_px = Some(px);
                }
            }
            'c' => {
                if let Some(color) = parse_inline_hex_color(value) {
                    style.text_color = Some(color);
                }
            }
            'l' => {
                if let Some(parsed) = PxOrPercent::parse(value) {
                    style.line_spacing = Some(clamp_px_or_percent(parsed, 300.0));
                }
            }
            'k' => {
                if let Some(parsed) = PxOrPercent::parse(value) {
                    style.kerning = Some(clamp_px_or_percent(parsed, 300.0));
                }
            }
            'w' => stretch_w = PxOrPercent::parse(value).map(clamp_stretch_px_or_percent),
            'h' => stretch_h = PxOrPercent::parse(value).map(clamp_stretch_px_or_percent),
            'x' => {
                if let Some(parsed) = PxOrPercent::parse(value) {
                    offset.global_x = clamp_px_or_percent(parsed, 100.0);
                    has_offset = true;
                }
            }
            'y' => {
                if let Some(parsed) = PxOrPercent::parse(value) {
                    offset.global_y = clamp_px_or_percent(parsed, 100.0);
                    has_offset = true;
                }
            }
            'n' => {
                if let Some(parsed) = PxOrPercent::parse(value) {
                    offset.line = clamp_px_or_percent(parsed, 300.0);
                    has_offset = true;
                }
            }
            'g' => {
                if let Ok(deg) = value.trim().parse::<f32>()
                    && deg.is_finite()
                {
                    offset.group_rotation_deg = deg.clamp(-180.0, 180.0);
                    has_offset = true;
                }
            }
            'r' => {
                if let Ok(deg) = value.trim().parse::<f32>()
                    && deg.is_finite()
                {
                    offset.glyph_rotation_deg = deg.clamp(-180.0, 180.0);
                    has_offset = true;
                }
            }
            'q' => {
                offset.shift_following = true;
                has_offset = true;
            }
            _ => {}
        }
    }

    if stretch_w.is_some() || stretch_h.is_some() {
        style.glyph_stretching = Some([
            stretch_w.unwrap_or(PxOrPercent::percent(100.0)),
            stretch_h.unwrap_or(PxOrPercent::percent(100.0)),
        ]);
    }
    if has_offset {
        style.glyph_offset = Some(offset);
    }

    Some(style)
}

fn parse_inline_offset_value(raw: &str) -> Option<TypingInlineOffsetStyle> {
    let (tag_name, value) = raw.split_once('=')?;
    if !tag_name.trim().eq_ignore_ascii_case("offset") {
        return None;
    }

    let value = value
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
        .trim();
    let parts = value.split(',').map(str::trim).collect::<Vec<_>>();
    // X/Y/«по линии» поддерживают суффикс `%` (проценты от кегля), иначе пиксели.
    let global_x = PxOrPercent::parse(parts.first()?)?;
    let global_y = PxOrPercent::parse(parts.get(1)?)?;
    if !global_x.value.is_finite() || !global_y.value.is_finite() {
        return None;
    }

    let line = parts
        .get(2)
        .and_then(|value| PxOrPercent::parse(value))
        .filter(|value| value.value.is_finite())
        .unwrap_or(PxOrPercent::px(0.0));
    let shift_following = parts
        .get(3)
        .is_some_and(|value| parse_inline_bool(value).unwrap_or(false));
    let group_rotation_deg = parts
        .get(4)
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or(0.0);
    let glyph_rotation_deg = parts
        .get(5)
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or(0.0);

    Some(normalize_inline_offset_style(TypingInlineOffsetStyle {
        global_x,
        global_y,
        line,
        shift_following,
        group_rotation_deg,
        glyph_rotation_deg,
    }))
}

fn parse_inline_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Извлечь значение тега `name=...`, обрезав кавычки/пробелы.
fn inline_tag_value<'a>(raw: &'a str, tag_name: &str) -> Option<&'a str> {
    let (raw_name, value) = raw.split_once('=')?;
    if !raw_name.trim().eq_ignore_ascii_case(tag_name) {
        return None;
    }
    Some(
        value
            .trim()
            .trim_matches(|ch| matches!(ch, '"' | '\'' | ' '))
            .trim(),
    )
}

/// Одиночное значение `px-или-%` (или устаревшая пара `px,percent`, которая
/// сворачивается с приоритетом пикселей) для тегов line-spacing/kerning.
fn parse_inline_value_or_legacy_pair(
    raw: &str,
    tag_name: &str,
    clamp_abs: f32,
) -> Option<PxOrPercent> {
    let value = inline_tag_value(raw, tag_name)?;
    if let Some((x_raw, y_raw)) = value.split_once(',') {
        let px = x_raw.trim().parse::<f32>().ok()?;
        let percent = y_raw.trim().parse::<f32>().ok()?;
        if !px.is_finite() || !percent.is_finite() {
            return None;
        }
        return Some(clamp_px_or_percent(
            PxOrPercent::from_legacy_pair(px, percent),
            clamp_abs,
        ));
    }
    let parsed = PxOrPercent::parse(value)?;
    if !parsed.value.is_finite() {
        return None;
    }
    Some(clamp_px_or_percent(parsed, clamp_abs))
}

/// `stretching=ширина,высота`, где каждая компонента — `px-или-%` (1..=300).
fn parse_inline_stretch_value(raw: &str) -> Option<[PxOrPercent; 2]> {
    let value = inline_tag_value(raw, "stretching")?;
    let (x_raw, y_raw) = value.split_once(',')?;
    let width = PxOrPercent::parse(x_raw)?;
    let height = PxOrPercent::parse(y_raw)?;
    if !width.value.is_finite() || !height.value.is_finite() {
        return None;
    }
    Some([
        clamp_stretch_px_or_percent(width),
        clamp_stretch_px_or_percent(height),
    ])
}

fn clamp_stretch_px_or_percent(value: PxOrPercent) -> PxOrPercent {
    PxOrPercent {
        value: value.value.clamp(1.0, 300.0),
        is_percent: value.is_percent,
    }
}

fn effect_card_title(effect: &EffectCard) -> &'static str {
    match effect {
        EffectCard::TextShake(_) => "Тряска текста",
        EffectCard::Stroke(_) => "Обводка",
        EffectCard::Shadow(_) => "Тень",
        EffectCard::Blur(_) => "Размытие",
        EffectCard::MotionBlur(_) => "Размытие в движении",
        EffectCard::DryMedia(_) => "Мел/Карандаш",
        EffectCard::Glow(glow) => match glow.version {
            GlowEffectVersion::V1 => "Свечение V1",
            GlowEffectVersion::V2 => "Свечение V2",
            GlowEffectVersion::Soft => "Мягкое свечение",
        },
        EffectCard::Gradient2(_) => "Градиент 2",
        EffectCard::Gradient4(_) => "Градиент 4",
        EffectCard::Reflect(_) => "Отражение",
        EffectCard::Shake(_) => "Тряска",
    }
}

fn draw_effect_card_controls(ui: &mut egui::Ui, effect: &mut EffectCard) -> bool {
    let mut changed = false;
    match effect {
        EffectCard::TextShake(shake) => {
            changed |= ui
                .add(WheelSlider::new(&mut shake.spread_x_px, 0.0..=256.0).text("Разброс по X"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.spread_y_px, 0.0..=256.0).text("Разброс по Y"))
                .changed();
            changed |= SeedSpinBox::new(&mut shake.seed)
                .prefix("Сид ")
                .draw(ui)
                .changed();
        }
        EffectCard::Stroke(stroke) => {
            changed |= ui
                .add(WheelSlider::new(&mut stroke.width_px, 0.0..=24.0).text("Ширина (px)"))
                .changed();
            changed |= stroke.color.draw(ui, "Цвет:");
            changed |= ui.checkbox(&mut stroke.smoothing, "Сглаживание").changed();
            ui.add_enabled_ui(stroke.smoothing, |ui| {
                changed |= ui
                    .add(
                        WheelSlider::new(&mut stroke.smoothing_strength_percent, 0.0..=100.0)
                            .text("Сила сглаживания (%)"),
                    )
                    .changed();
            });
            let mut opacity_idx = if stroke.opacity_mode == StrokeOpacityMode::Static {
                0
            } else {
                1
            };
            let stroke_opacity_prev = opacity_idx;
            let stroke_opacity_combo = WheelComboBox::from_label("Прозрачность контура")
                .selected_text(match stroke.opacity_mode {
                    StrokeOpacityMode::Static => "Статическая",
                    StrokeOpacityMode::FromContour => "От контура",
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui
                        .selectable_label(opacity_idx == 0, "Статическая")
                        .clicked()
                    {
                        opacity_idx = 0;
                    }
                    if ui
                        .selectable_label(opacity_idx == 1, "От контура")
                        .clicked()
                    {
                        opacity_idx = 1;
                    }
                });
            if let Some(steps) = stroke_opacity_combo.wheel_steps {
                cycle_wrapped_index(&mut opacity_idx, 2, steps);
            }
            changed |= opacity_idx != stroke_opacity_prev;
            stroke.opacity_mode = if opacity_idx == 0 {
                StrokeOpacityMode::Static
            } else {
                StrokeOpacityMode::FromContour
            };
            ui.add_enabled_ui(stroke.opacity_mode == StrokeOpacityMode::Static, |ui| {
                changed |= ui
                    .add(
                        WheelSlider::new(&mut stroke.transparency_percent, 0.0..=100.0)
                            .text("Прозрачность (%)"),
                    )
                    .changed();
            });
        }
        EffectCard::Shadow(shadow) => {
            changed |= ui
                .add(WheelSlider::new(&mut shadow.offset_x_px, -400..=400).text("Смещение X (px)"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shadow.offset_y_px, -400..=400).text("Смещение Y (px)"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shadow.blur_radius_px, 0.0..=64.0).text("Размытие (px)"))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut shadow.transparency_percent, 0.0..=100.0)
                        .text("Прозрачность (%)"),
                )
                .changed();
            let mut color_mode_idx = if shadow.color_mode == ShadowColorMode::SingleColor {
                0
            } else {
                1
            };
            let color_mode_prev = color_mode_idx;
            let color_mode_combo = WheelComboBox::from_label("Режим цвета")
                .selected_text(match shadow.color_mode {
                    ShadowColorMode::SingleColor => "Один цвет",
                    ShadowColorMode::SourceColors => "Исходные цвета",
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui
                        .selectable_label(color_mode_idx == 0, "Один цвет")
                        .clicked()
                    {
                        color_mode_idx = 0;
                    }
                    if ui
                        .selectable_label(color_mode_idx == 1, "Исходные цвета")
                        .clicked()
                    {
                        color_mode_idx = 1;
                    }
                });
            if let Some(steps) = color_mode_combo.wheel_steps {
                cycle_wrapped_index(&mut color_mode_idx, 2, steps);
            }
            changed |= color_mode_idx != color_mode_prev;
            shadow.color_mode = if color_mode_idx == 0 {
                ShadowColorMode::SingleColor
            } else {
                ShadowColorMode::SourceColors
            };
            ui.add_enabled_ui(shadow.color_mode == ShadowColorMode::SingleColor, |ui| {
                changed |= shadow.color.draw(ui, "Цвет:");
            });
        }
        EffectCard::Blur(blur) => {
            changed |= ui
                .add(WheelSlider::new(&mut blur.radius_px, 0.0..=128.0).text("Радиус (px)"))
                .changed();
        }
        EffectCard::MotionBlur(blur) => {
            changed |= ui
                .add(WheelSlider::new(&mut blur.angle_deg, -360.0..=360.0).text("Угол (°)"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut blur.distance_px, 0.0..=512.0).text("Смещение (px)"))
                .changed();
            let mut sharp_copy_idx = match blur.sharp_copy_mode {
                MotionBlurSharpCopyMode::None => 0,
                MotionBlurSharpCopyMode::Over => 1,
                MotionBlurSharpCopyMode::Under => 2,
            };
            let sharp_copy_prev = sharp_copy_idx;
            let sharp_copy_combo = WheelComboBox::from_label("Неразмытая копия")
                .selected_text(match blur.sharp_copy_mode {
                    MotionBlurSharpCopyMode::None => "Нет",
                    MotionBlurSharpCopyMode::Over => "Сверху",
                    MotionBlurSharpCopyMode::Under => "Снизу",
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui.selectable_label(sharp_copy_idx == 0, "Нет").clicked() {
                        sharp_copy_idx = 0;
                    }
                    if ui.selectable_label(sharp_copy_idx == 1, "Сверху").clicked() {
                        sharp_copy_idx = 1;
                    }
                    if ui.selectable_label(sharp_copy_idx == 2, "Снизу").clicked() {
                        sharp_copy_idx = 2;
                    }
                });
            if let Some(steps) = sharp_copy_combo.wheel_steps {
                cycle_wrapped_index(&mut sharp_copy_idx, 3, steps);
            }
            changed |= sharp_copy_idx != sharp_copy_prev;
            blur.sharp_copy_mode = match sharp_copy_idx {
                1 => MotionBlurSharpCopyMode::Over,
                2 => MotionBlurSharpCopyMode::Under,
                _ => MotionBlurSharpCopyMode::None,
            };
        }
        EffectCard::DryMedia(dry_media) => {
            let mut material_idx = if dry_media.material == DryMediaMaterial::Pencil {
                0
            } else {
                1
            };
            let material_prev = material_idx;
            let material_combo = WheelComboBox::from_label("Материал")
                .selected_text(match dry_media.material {
                    DryMediaMaterial::Pencil => "Карандаш",
                    DryMediaMaterial::Chalk => "Мел",
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui.selectable_label(material_idx == 0, "Карандаш").clicked() {
                        material_idx = 0;
                    }
                    if ui.selectable_label(material_idx == 1, "Мел").clicked() {
                        material_idx = 1;
                    }
                });
            if let Some(steps) = material_combo.wheel_steps {
                cycle_wrapped_index(&mut material_idx, 2, steps);
            }
            changed |= material_idx != material_prev;
            dry_media.material = if material_idx == 0 {
                DryMediaMaterial::Pencil
            } else {
                DryMediaMaterial::Chalk
            };

            changed |= ui
                .add(WheelSlider::new(&mut dry_media.strength, 0.0..=1.0).text("Сила"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut dry_media.seed, 0..=u64::MAX).text("Сид"))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut dry_media.grain_scale_px, 0.5..=32.0)
                        .text("Размер зерна (px)"),
                )
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut dry_media.grain_amount, 0.0..=1.0).text("Зернистость"))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut dry_media.edge_roughness, 0.0..=1.0)
                        .text("Рваность края"),
                )
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut dry_media.porosity, 0.0..=1.0).text("Пористость"))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut dry_media.direction_deg, -360.0..=360.0)
                        .text("Угол штриха (°)"),
                )
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut dry_media.directional_amount, 0.0..=1.0)
                        .text("Сила штриховки"),
                )
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut dry_media.dust_amount, 0.0..=1.0).text("Пыль"))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut dry_media.dust_radius_px, 0.0..=32.0)
                        .text("Радиус пыли (px)"),
                )
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut dry_media.softness_px, 0.0..=16.0).text("Мягкость (px)"))
                .changed();
            changed |= ui
                .checkbox(&mut dry_media.use_source_color, "Сохранить исходный цвет")
                .changed();
            ui.add_enabled_ui(!dry_media.use_source_color, |ui| {
                changed |= dry_media.color.draw(ui, "Цвет:");
            });
        }
        EffectCard::Glow(glow) => {
            changed |= ui
                .add(WheelSlider::new(&mut glow.radius_px, 0.0..=300.0).text("Радиус (px)"))
                .changed();
            if glow.version == GlowEffectVersion::Soft {
                changed |= ui
                    .add(WheelSlider::new(&mut glow.softness_px, 0.0..=100.0).text("Мягкость (px)"))
                    .changed();
                changed |= glow.color.draw(ui, "Цвет:");
            } else {
                changed |= glow.color.draw(ui, "Цвет:");
                let mut opacity_idx = if glow.opacity_mode == StrokeOpacityMode::Static {
                    0
                } else {
                    1
                };
                let glow_opacity_prev = opacity_idx;
                let glow_opacity_combo = WheelComboBox::from_label("Прозрачность")
                    .selected_text(match glow.opacity_mode {
                        StrokeOpacityMode::Static => "Статическая",
                        StrokeOpacityMode::FromContour => "От контура",
                    })
                    .show_ui_with_wheel(ui, |ui| {
                        if ui
                            .selectable_label(opacity_idx == 0, "Статическая")
                            .clicked()
                        {
                            opacity_idx = 0;
                        }
                        if ui
                            .selectable_label(opacity_idx == 1, "От контура")
                            .clicked()
                        {
                            opacity_idx = 1;
                        }
                    });
                if let Some(steps) = glow_opacity_combo.wheel_steps {
                    cycle_wrapped_index(&mut opacity_idx, 2, steps);
                }
                changed |= opacity_idx != glow_opacity_prev;
                glow.opacity_mode = if opacity_idx == 0 {
                    StrokeOpacityMode::Static
                } else {
                    StrokeOpacityMode::FromContour
                };
                ui.add_enabled_ui(glow.opacity_mode == StrokeOpacityMode::Static, |ui| {
                    changed |= ui
                        .add(
                            WheelSlider::new(&mut glow.transparency_percent, 0.0..=100.0)
                                .text("Прозрачность (%)"),
                        )
                        .changed();
                });
                changed |= ui
                    .add(
                        WheelSlider::new(&mut glow.fade_strength, -100.0..=100.0)
                            .text("Сила затухания"),
                    )
                    .changed();
                changed |= ui
                    .add(
                        WheelSlider::new(&mut glow.fade_shift, -100.0..=100.0)
                            .text("Смещение затухания"),
                    )
                    .changed();
            }
        }
        EffectCard::Gradient2(gradient) => {
            changed |= gradient.color1.draw(ui, "Цвет 1:");
            changed |= gradient.color2.draw(ui, "Цвет 2:");
            changed |= ui
                .add(WheelSlider::new(&mut gradient.angle_deg, -360.0..=360.0).text("Угол (°)"))
                .changed();
            changed |= ui
                .add(
                    WheelSlider::new(&mut gradient.width_percent, 1.0..=400.0)
                        .text("Ширина градиента (%)"),
                )
                .changed();
            changed |= ui
                .checkbox(&mut gradient.respect_source_alpha, "Учитывать прозрачность")
                .changed();
            let mut fill_mode_idx = if gradient.fill_mode == Gradient2FillMode::AllOpaque {
                0
            } else {
                1
            };
            let gradient2_fill_prev = fill_mode_idx;
            let gradient2_fill_combo = WheelComboBox::from_label("Тип заполнения")
                .selected_text(match gradient.fill_mode {
                    Gradient2FillMode::AllOpaque => "Всё непрозрачное",
                    Gradient2FillMode::SpecificColor => "Конкретный цвет",
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui
                        .selectable_label(fill_mode_idx == 0, "Всё непрозрачное")
                        .clicked()
                    {
                        fill_mode_idx = 0;
                    }
                    if ui
                        .selectable_label(fill_mode_idx == 1, "Конкретный цвет")
                        .clicked()
                    {
                        fill_mode_idx = 1;
                    }
                });
            if let Some(steps) = gradient2_fill_combo.wheel_steps {
                cycle_wrapped_index(&mut fill_mode_idx, 2, steps);
            }
            changed |= fill_mode_idx != gradient2_fill_prev;
            gradient.fill_mode = if fill_mode_idx == 0 {
                Gradient2FillMode::AllOpaque
            } else {
                Gradient2FillMode::SpecificColor
            };
            ui.add_enabled_ui(
                gradient.fill_mode == Gradient2FillMode::SpecificColor,
                |ui| {
                    changed |= gradient.target_color.draw(ui, "Заменяемый:");
                },
            );
        }
        EffectCard::Gradient4(gradient) => {
            changed |= gradient.color_top_left.draw(ui, "Левый верх:");
            changed |= gradient.color_top_right.draw(ui, "Правый верх:");
            changed |= gradient.color_bottom_left.draw(ui, "Левый низ:");
            changed |= gradient.color_bottom_right.draw(ui, "Правый низ:");
            changed |= ui
                .add(
                    WheelSlider::new(&mut gradient.width_percent, 1.0..=400.0)
                        .text("Ширина градиента (%)"),
                )
                .changed();
            changed |= ui
                .checkbox(&mut gradient.respect_source_alpha, "Учитывать прозрачность")
                .changed();
            let mut fill_mode_idx = if gradient.fill_mode == Gradient4FillMode::AllOpaque {
                0
            } else {
                1
            };
            let gradient4_fill_prev = fill_mode_idx;
            let gradient4_fill_combo = WheelComboBox::from_label("Тип заполнения")
                .selected_text(match gradient.fill_mode {
                    Gradient4FillMode::AllOpaque => "Всё непрозрачное",
                    Gradient4FillMode::SpecificColor => "Конкретный цвет",
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui
                        .selectable_label(fill_mode_idx == 0, "Всё непрозрачное")
                        .clicked()
                    {
                        fill_mode_idx = 0;
                    }
                    if ui
                        .selectable_label(fill_mode_idx == 1, "Конкретный цвет")
                        .clicked()
                    {
                        fill_mode_idx = 1;
                    }
                });
            if let Some(steps) = gradient4_fill_combo.wheel_steps {
                cycle_wrapped_index(&mut fill_mode_idx, 2, steps);
            }
            changed |= fill_mode_idx != gradient4_fill_prev;
            gradient.fill_mode = if fill_mode_idx == 0 {
                Gradient4FillMode::AllOpaque
            } else {
                Gradient4FillMode::SpecificColor
            };
            ui.add_enabled_ui(
                gradient.fill_mode == Gradient4FillMode::SpecificColor,
                |ui| {
                    changed |= gradient.target_color.draw(ui, "Заменяемый:");
                },
            );
        }
        EffectCard::Reflect(reflect) => {
            let mut axis_idx = if reflect.axis == ReflectAxis::X { 0 } else { 1 };
            let reflect_axis_prev = axis_idx;
            let reflect_axis_combo = WheelComboBox::from_label("Ось отражения")
                .selected_text(match reflect.axis {
                    ReflectAxis::X => "X (верх-низ)",
                    ReflectAxis::Y => "Y (лево-право)",
                })
                .show_ui_with_wheel(ui, |ui| {
                    if ui.selectable_label(axis_idx == 0, "X (верх-низ)").clicked() {
                        axis_idx = 0;
                    }
                    if ui
                        .selectable_label(axis_idx == 1, "Y (лево-право)")
                        .clicked()
                    {
                        axis_idx = 1;
                    }
                });
            if let Some(steps) = reflect_axis_combo.wheel_steps {
                cycle_wrapped_index(&mut axis_idx, 2, steps);
            }
            changed |= axis_idx != reflect_axis_prev;
            reflect.axis = if axis_idx == 0 {
                ReflectAxis::X
            } else {
                ReflectAxis::Y
            };
        }
        EffectCard::Shake(shake) => {
            changed |= ui
                .add(WheelSlider::new(&mut shake.angle_deg, -360.0..=360.0).text("Угол (°)"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.up_px, 0.0..=1000.0).text("Ампл. вверх (px)"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.down_px, 0.0..=1000.0).text("Ампл. вниз (px)"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.steps, 0..=128).text("Шаги"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.base_fade, 0.0..=1.0).text("Базовое затухание"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.decay, 0.0..=1.0).text("Спад"))
                .changed();
            changed |= ui
                .add(WheelSlider::new(&mut shake.blur_px, 0..=64).text("Blur (px)"))
                .changed();
            changed |= ui
                .checkbox(&mut shake.autogrow, "Auto-grow canvas")
                .changed();
            ui.add_enabled_ui(shake.autogrow, |ui| {
                changed |= ui
                    .add(WheelSlider::new(&mut shake.grow_margin_px, 0..=1024).text("Доп. отступ"))
                    .changed();
            });
        }
    }

    changed
}

fn spawn_preview_render_worker() -> (Sender<PreviewRenderJob>, Receiver<PreviewRenderResult>) {
    let (request_tx, request_rx) = mpsc::channel::<PreviewRenderJob>();
    let (result_tx, result_rx) = mpsc::channel::<PreviewRenderResult>();

    let _ = thread::Builder::new()
        .name("typing-text-preview-render-worker".to_string())
        .spawn(move || {
            while let Ok(mut job) = request_rx.recv() {
                let mut dropped = 0u32;
                while let Ok(newer_job) = request_rx.try_recv() {
                    job = newer_job;
                    dropped += 1;
                }
                crate::trace_log!(
                    cat::RENDER,
                    "preview_render_worker start token={} dropped_stale={}",
                    job.token,
                    dropped
                );

                let result = render_text_to_image(&job.params, None);
                if let Err(err) = result.as_ref() {
                    eprintln!(
                        "ERROR typing::preview_render layout={:?} shape={:?} wrap={:?} line_mode={:?} width_px={} err={}",
                        job.params.text_layout_mode,
                        job.params.text_shape,
                        job.params.text_wrap_mode,
                        job.params.text_line_mode,
                        job.params.width_px,
                        err
                    );
                }
                if result_tx
                    .send(PreviewRenderResult {
                        token: job.token,
                        image: result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

    (request_tx, result_rx)
}

fn resolve_fonts_dir() -> PathBuf {
    if let Ok(cwd) = env::current_dir() {
        let candidate = cwd.join("fonts");
        if candidate.is_dir() {
            return candidate;
        }
    }

    if let Ok(exe_path) = env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        let candidate = exe_dir.join("fonts");
        if candidate.is_dir() {
            return candidate;
        }
    }

    PathBuf::from("fonts")
}

fn load_fonts(fonts_dir: &Path, use_system_fonts: bool) -> Vec<FontEntry> {
    let mut entries = load_fonts_from_dir(fonts_dir);
    if !use_system_fonts {
        return entries;
    }

    let mut known_paths: HashSet<PathBuf> = entries
        .iter()
        .flat_map(|font| std::iter::once(font.path.clone()).chain(font.alt_paths.iter().cloned()))
        .collect();
    for system_font in load_system_fonts() {
        if known_paths.insert(system_font.path.clone()) {
            entries.push(system_font);
        }
    }
    entries.sort_by_key(|font| font.label.to_lowercase());
    entries
}

/// Одна найденная копия файла шрифта до объединения дубликатов.
struct RawFontFile {
    path: PathBuf,
    stem: String,
    group: Option<String>,
    content_hash: u64,
    faces: Vec<FontFaceEntry>,
}

fn load_fonts_from_dir(fonts_dir: &Path) -> Vec<FontEntry> {
    let mut files = Vec::<PathBuf>::new();
    collect_font_files_recursive(fonts_dir, fonts_dir, &mut files);
    files.sort_by_key(|path| path.to_string_lossy().to_lowercase());

    // Читаем каждый файл один раз: и для перечня faces, и для хэша содержимого.
    let raws: Vec<RawFontFile> = files
        .into_iter()
        .map(|path| {
            let bytes = fs::read(&path).ok();
            let content_hash = bytes.as_deref().map_or(0, font_content_hash);
            let faces = bytes
                .as_deref()
                .map_or_else(default_single_face, font_faces_from_bytes);
            let stem = path
                .file_stem()
                .and_then(|v| v.to_str())
                .unwrap_or("font")
                .to_string();
            let group = font_group_name_for_path(fonts_dir, &path);
            RawFontFile {
                path,
                stem,
                group,
                content_hash,
                faces,
            }
        })
        .collect();

    let mut entries = merge_duplicate_fonts(raws);
    assign_font_disambiguators(&mut entries);
    entries
}

/// Объединяет копии одного шрифта (совпадает имя файла и содержимое — «тот же
/// хэш») в один пункт со списком групп; разные по содержимому остаются раздельно.
fn merge_duplicate_fonts(raws: Vec<RawFontFile>) -> Vec<FontEntry> {
    // Кластеризация по (имя файла без регистра, хэш содержимого), с сохранением
    // порядка первого появления.
    let mut order: Vec<(String, u64)> = Vec::new();
    let mut clusters: HashMap<(String, u64), Vec<RawFontFile>> = HashMap::new();
    for raw in raws {
        let key = (raw.stem.to_lowercase(), raw.content_hash);
        if !clusters.contains_key(&key) {
            order.push(key.clone());
        }
        clusters.entry(key).or_default().push(raw);
    }

    let mut entries = Vec::with_capacity(order.len());
    for key in order {
        let mut cluster = clusters.remove(&key).unwrap_or_default();
        // Представитель — первый по пути (детерминированно).
        cluster.sort_by(|a, b| a.path.cmp(&b.path));
        let rep = &cluster[0];
        let label = rep.stem.clone();
        let faces = rep.faces.clone();
        let path = rep.path.clone();
        let alt_paths = cluster[1..].iter().map(|raw| raw.path.clone()).collect();
        // Объединение групп копий (без повторов, в стабильном порядке).
        let mut groups: Vec<Option<String>> = Vec::new();
        for raw in &cluster {
            if !groups.contains(&raw.group) {
                groups.push(raw.group.clone());
            }
        }
        entries.push(FontEntry {
            label,
            path,
            alt_paths,
            groups,
            disambig: None,
            faces,
        });
    }
    entries
}

/// Проставляет скобочное уточнение (по группам) тем пунктам, у которых базовое
/// имя совпадает с другим пунктом.
fn assign_font_disambiguators(entries: &mut [FontEntry]) {
    let mut label_counts: HashMap<String, usize> = HashMap::new();
    for entry in entries.iter() {
        *label_counts.entry(entry.label.to_lowercase()).or_insert(0) += 1;
    }
    // Уникальное имя — уточнение не нужно.
    let mut used: HashMap<String, usize> = HashMap::new();
    for entry in entries.iter_mut() {
        if label_counts.get(&entry.label.to_lowercase()).copied().unwrap_or(0) <= 1 {
            entry.disambig = None;
            continue;
        }
        let mut suffix = font_groups_label(&entry.groups);
        // Если уточнения совпали (например, два корневых) — добавим индекс.
        let key = format!("{}\u{0}{}", entry.label.to_lowercase(), suffix.to_lowercase());
        let n = used.entry(key).or_insert(0);
        *n += 1;
        if *n > 1 {
            suffix = format!("{suffix} {n}");
        }
        entry.disambig = Some(suffix);
    }
}

/// Отображаемое имя группы для уточнения: имя группы или «корень».
fn font_groups_label(groups: &[Option<String>]) -> String {
    let parts: Vec<&str> = groups
        .iter()
        .map(|group| group.as_deref().unwrap_or("корень"))
        .collect();
    if parts.is_empty() {
        "корень".to_string()
    } else {
        parts.join(", ")
    }
}

#[must_use]
fn font_content_hash(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

#[must_use]
fn default_single_face() -> Vec<FontFaceEntry> {
    vec![FontFaceEntry {
        label: "Face 0".to_string(),
        face_index: 0,
    }]
}

fn load_font_groups(fonts_dir: &Path) -> Vec<String> {
    let groups_dir = fonts_dir.join("groups");
    let Ok(read_dir) = fs::read_dir(groups_dir) else {
        return Vec::new();
    };

    let mut groups = read_dir
        .filter_map(|entry_result| {
            let entry = entry_result.ok()?;
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            path.file_name()
                .and_then(|value| value.to_str())
                .map(ToOwned::to_owned)
        })
        .collect::<Vec<_>>();
    groups.sort_by_key(|group_name| group_name.to_lowercase());
    groups
}

fn load_system_fonts() -> Vec<FontEntry> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    let mut by_path = HashMap::<PathBuf, Vec<FontFaceEntry>>::new();
    for face in db.faces() {
        let path = match &face.source {
            fontdb::Source::File(path) => path.clone(),
            _ => continue,
        };
        let family = face
            .families
            .first()
            .map(|(name, _)| name.as_str())
            .unwrap_or("Unknown");
        let style = match face.style {
            fontdb::Style::Normal => "Normal",
            fontdb::Style::Italic => "Italic",
            fontdb::Style::Oblique => "Oblique",
        };
        let face_index = face.index as usize;
        by_path.entry(path).or_default().push(FontFaceEntry {
            label: format!(
                "#{face_index} {family} | {style} | w{} | {}",
                face.weight.0, face.post_script_name
            ),
            face_index,
        });
    }

    let mut files: Vec<PathBuf> = by_path.keys().cloned().collect();
    files.sort_by_key(|path| path.to_string_lossy().to_lowercase());

    let mut used_labels = HashMap::<String, usize>::new();
    let mut entries = Vec::<FontEntry>::with_capacity(files.len());
    for path in files {
        let mut faces = by_path.remove(&path).unwrap_or_default();
        faces.sort_by_key(|face| face.face_index);
        if faces.is_empty() {
            faces.push(FontFaceEntry {
                label: "Face 0".to_string(),
                face_index: 0,
            });
        }

        let stem = path
            .file_stem()
            .and_then(|v| v.to_str())
            .unwrap_or("system font");
        let base_label = format!("{stem} [system]");
        let count = used_labels.entry(base_label.clone()).or_insert(0);
        *count += 1;
        let label = if *count > 1 {
            format!("{base_label} ({count})")
        } else {
            base_label
        };
        entries.push(FontEntry {
            label,
            path,
            alt_paths: Vec::new(),
            groups: vec![None],
            disambig: None,
            faces,
        });
    }

    entries
}

fn font_faces_from_bytes(bytes: &[u8]) -> Vec<FontFaceEntry> {
    let mut db = fontdb::Database::new();
    let ids = db.load_font_source(fontdb::Source::Binary(Arc::new(bytes.to_vec())));
    if ids.is_empty() {
        return default_single_face();
    }

    let mut faces = Vec::with_capacity(ids.len());
    for (idx, id) in ids.iter().enumerate() {
        let label = if let Some(face) = db.face(*id) {
            let family = face
                .families
                .first()
                .map(|(name, _)| name.as_str())
                .unwrap_or("Unknown");
            let style = match face.style {
                fontdb::Style::Normal => "Normal",
                fontdb::Style::Italic => "Italic",
                fontdb::Style::Oblique => "Oblique",
            };
            format!(
                "#{idx} {family} | {style} | w{} | {}",
                face.weight.0, face.post_script_name
            )
        } else {
            format!("#{idx} Face")
        };
        faces.push(FontFaceEntry {
            label,
            face_index: idx,
        });
    }

    if faces.is_empty() {
        faces.push(FontFaceEntry {
            label: "Face 0".to_string(),
            face_index: 0,
        });
    }
    faces
}

fn font_group_name_for_path(fonts_dir: &Path, path: &Path) -> Option<String> {
    let mut components = path.strip_prefix(fonts_dir).ok()?.components();
    let first = components.next()?.as_os_str().to_str()?;
    if !first.eq_ignore_ascii_case("groups") {
        return None;
    }
    components
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .map(ToOwned::to_owned)
}

fn collect_font_files_recursive(root_dir: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };

    for entry_result in read_dir {
        let Ok(entry) = entry_result else {
            continue;
        };
        let path = entry.path();
        if path.is_dir() {
            if should_skip_font_dir(root_dir, &path) {
                continue;
            }
            collect_font_files_recursive(root_dir, &path, out);
            continue;
        }

        let ext = path
            .extension()
            .and_then(|v| v.to_str())
            .map(|v| v.to_ascii_lowercase())
            .unwrap_or_default();
        if matches!(ext.as_str(), "ttf" | "otf" | "ttc") {
            out.push(path);
        }
    }
}

fn should_skip_font_dir(root_dir: &Path, dir: &Path) -> bool {
    dir.strip_prefix(root_dir)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .is_some_and(|component| component.eq_ignore_ascii_case("ui"))
}

fn load_text_tab_use_system_fonts() -> bool {
    let user_settings_file = config::user_config_path();
    let Ok(raw) = fs::read_to_string(user_settings_file) else {
        return false;
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    payload
        .get("TextTab")
        .and_then(Value::as_object)
        .and_then(|text_tab| text_tab.get(TEXT_TAB_USE_SYSTEM_FONTS_KEY))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Читает настройку «использовать обычные inline-теги вместо машиночитаемых».
/// По умолчанию `false` — панель пишет компактный `<m ...>`. Пока не подключено к UI.
fn load_text_tab_use_legacy_inline_tags() -> bool {
    let user_settings_file = config::user_config_path();
    let Ok(raw) = fs::read_to_string(user_settings_file) else {
        return false;
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    payload
        .get("TextTab")
        .and_then(Value::as_object)
        .and_then(|text_tab| text_tab.get(TEXT_TAB_USE_LEGACY_INLINE_TAGS_KEY))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn load_text_tab_create_presets() -> HashMap<String, TypingCreatePreset> {
    let user_settings_file = config::user_config_path();
    let Ok(raw) = fs::read_to_string(user_settings_file) else {
        return HashMap::new();
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return HashMap::new();
    };
    let Some(presets_obj) = payload
        .get("TextTab")
        .and_then(Value::as_object)
        .and_then(|text_tab| text_tab.get(TEXT_TAB_CREATE_PRESETS_KEY))
        .and_then(Value::as_object)
    else {
        return HashMap::new();
    };

    let mut out = HashMap::new();
    for (name, raw_preset) in presets_obj {
        let Some(obj) = raw_preset.as_object() else {
            continue;
        };
        let primary_font_key = obj
            .get("primary_font_key")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if primary_font_key.is_empty() {
            continue;
        }
        let primary_font_path = obj
            .get("primary_font_path")
            .and_then(Value::as_str)
            .map(str::to_string);
        let primary_font_label = obj
            .get("primary_font_label")
            .and_then(Value::as_str)
            .map(str::to_string);
        let font_profiles = obj
            .get("font_profiles")
            .and_then(Value::as_object)
            .map(|profiles| {
                profiles
                    .iter()
                    .map(|(font_key, profile)| (font_key.clone(), profile.clone()))
                    .collect::<HashMap<String, Value>>()
            })
            .unwrap_or_default();
        out.insert(
            name.clone(),
            TypingCreatePreset {
                primary_font_key,
                primary_font_path,
                primary_font_label,
                font_profiles,
            },
        );
    }
    out
}

fn default_text_tab_formula_presets() -> HashMap<String, TypingFormulaPreset> {
    let mut out = HashMap::<String, TypingFormulaPreset>::new();
    out.insert(
        "Дуга (мягкая)".to_string(),
        formula_preset(
            "t * w",
            "120 * sin((t - 0.5) * pi)",
            "0",
            true,
            1.25,
            [0.0; TEXT_FORMULA_USER_VAR_COUNT],
        ),
    );
    out.insert(
        "Наклонная линия".to_string(),
        formula_preset(
            "t * w",
            "0.35 * t * w",
            "0",
            false,
            1.1,
            [0.0; TEXT_FORMULA_USER_VAR_COUNT],
        ),
    );
    out.insert(
        "Волна".to_string(),
        formula_preset(
            "t * w",
            "80 * sin(2 * pi * t)",
            "0.15 * sin(2 * pi * t)",
            false,
            1.2,
            [0.0; TEXT_FORMULA_USER_VAR_COUNT],
        ),
    );
    out.insert(
        "Спираль".to_string(),
        formula_preset(
            "(a + b * t) * cos(c * tau * t)",
            "(a + b * t) * sin(c * tau * t)",
            "0",
            true,
            1.35,
            [40.0, 180.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out.insert(
        "Экспонента".to_string(),
        formula_preset(
            "t * w",
            "140 * (exp(a * t) - 1) / (exp(a) - 1)",
            "0",
            true,
            1.2,
            [3.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out.insert(
        "Парабола".to_string(),
        formula_preset(
            "t * w",
            "a * pow(2 * t - 1, 2) - b",
            "0",
            true,
            1.15,
            [180.0, 50.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out.insert(
        "Пульс".to_string(),
        formula_preset(
            "t * w",
            "a * pow(sin(pi * t), b) * sin(c * tau * t)",
            "0",
            false,
            1.08,
            [140.0, 8.0, 2.5, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out.insert(
        "Лемниската".to_string(),
        formula_preset(
            "a * cos(tau * t) / (1 + pow(sin(tau * t), 2))",
            "b * sin(tau * t) * cos(tau * t) / (1 + pow(sin(tau * t), 2))",
            "0",
            true,
            1.25,
            [240.0, 220.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out.insert(
        "Сердце".to_string(),
        formula_preset(
            "16 * a * pow(sin(tau * t), 3)",
            "-(13 * a * cos(tau * t) - 5 * a * cos(2 * tau * t) - 2 * a * cos(3 * tau * t) - a * cos(4 * tau * t))",
            "0",
            true,
            1.4,
            [10.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out.insert(
        "Капля".to_string(),
        formula_preset(
            "a * (1 - c * sin(tau * t)) * cos(tau * t)",
            "b * (1 - c * sin(tau * t)) * sin(tau * t)",
            "0",
            true,
            1.22,
            [180.0, 210.0, 0.35, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out.insert(
        "Вертикальная волна".to_string(),
        formula_preset(
            "90 * sin(2 * pi * t)",
            "a * (t - 0.5)",
            "0",
            true,
            1.18,
            [360.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        ),
    );
    out
}

fn formula_preset(
    x_expr: &str,
    y_expr: &str,
    rotation_expr: &str,
    use_tangent_rotation: bool,
    letter_spacing_mul: f32,
    vars: [f32; TEXT_FORMULA_USER_VAR_COUNT],
) -> TypingFormulaPreset {
    TypingFormulaPreset {
        layout: TextFormulaLayoutParams {
            x_expr: x_expr.to_string(),
            y_expr: y_expr.to_string(),
            rotation_expr: rotation_expr.to_string(),
            use_tangent_rotation,
            t_start: 0.0,
            t_end: 1.0,
            offset_x_px: 0.0,
            offset_y_px: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
            normal_offset_px: 0.0,
            letter_spacing_mul,
            letter_spacing_px: 0.0,
            vars,
        },
    }
}

fn text_formula_layout_from_value(value: &Value) -> Option<TextFormulaLayoutParams> {
    let obj = value.as_object()?;
    let mut out = TextFormulaLayoutParams::default();
    if let Some(raw) = obj.get("x_expr").and_then(Value::as_str) {
        out.x_expr = raw.to_string();
    }
    if let Some(raw) = obj.get("y_expr").and_then(Value::as_str) {
        out.y_expr = raw.to_string();
    }
    if let Some(raw) = obj.get("rotation_expr").and_then(Value::as_str) {
        out.rotation_expr = raw.to_string();
    }
    out.use_tangent_rotation = obj
        .get("use_tangent_rotation")
        .and_then(Value::as_bool)
        .unwrap_or(out.use_tangent_rotation);
    out.t_start = obj
        .get("t_start")
        .and_then(value_as_f32)
        .unwrap_or(out.t_start);
    out.t_end = obj.get("t_end").and_then(value_as_f32).unwrap_or(out.t_end);
    out.offset_x_px = obj
        .get("offset_x_px")
        .and_then(value_as_f32)
        .unwrap_or(out.offset_x_px);
    out.offset_y_px = obj
        .get("offset_y_px")
        .and_then(value_as_f32)
        .unwrap_or(out.offset_y_px);
    out.scale_x = obj
        .get("scale_x")
        .and_then(value_as_f32)
        .unwrap_or(out.scale_x);
    out.scale_y = obj
        .get("scale_y")
        .and_then(value_as_f32)
        .unwrap_or(out.scale_y);
    out.normal_offset_px = obj
        .get("normal_offset_px")
        .and_then(value_as_f32)
        .unwrap_or(out.normal_offset_px);
    out.letter_spacing_mul = obj
        .get("letter_spacing_mul")
        .and_then(value_as_f32)
        .unwrap_or(out.letter_spacing_mul)
        .clamp(0.0, 8.0);
    out.letter_spacing_px = obj
        .get("letter_spacing_px")
        .and_then(value_as_f32)
        .unwrap_or(out.letter_spacing_px)
        .clamp(-10_000.0, 10_000.0);
    if let Some(vars) = obj.get("vars").and_then(Value::as_array) {
        for (idx, value) in vars.iter().take(TEXT_FORMULA_USER_VAR_COUNT).enumerate() {
            if let Some(parsed) = value_as_f32(value) {
                out.vars[idx] = parsed;
            }
        }
    }
    Some(out)
}

fn text_drawn_lines_layout_from_value(value: &Value) -> Option<TextDrawnLinesLayoutParams> {
    let obj = value.as_object()?;
    let defaults = TextDrawnLinesLayoutParams::default();
    Some(TextDrawnLinesLayoutParams {
        image_path: None,
        use_tangent_rotation: obj
            .get("use_tangent_rotation")
            .and_then(Value::as_bool)
            .unwrap_or(defaults.use_tangent_rotation),
        static_rotation_rad: obj
            .get("static_rotation_rad")
            .and_then(value_as_f32)
            .unwrap_or(defaults.static_rotation_rad),
        normal_offset_px: obj
            .get("normal_offset_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.normal_offset_px),
        letter_spacing_mul: obj
            .get("letter_spacing_mul")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_mul)
            .clamp(0.0, 8.0),
        letter_spacing_px: obj
            .get("letter_spacing_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_px)
            .clamp(-10_000.0, 10_000.0),
        color_tolerance: obj
            .get("color_tolerance")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(defaults.color_tolerance),
        continuation_alpha: obj
            .get("continuation_alpha")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(defaults.continuation_alpha),
        start_alpha: obj
            .get("start_alpha")
            .and_then(Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(defaults.start_alpha),
    })
}

fn text_vector_lines_layout_from_value(value: &Value) -> Option<TextVectorLinesLayoutParams> {
    let obj = value.as_object()?;
    let defaults = TextVectorLinesLayoutParams::default();
    let lines = obj
        .get("lines")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(text_vector_line_from_value)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(TextVectorLinesLayoutParams {
        width_px: obj
            .get("width_px")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(defaults.width_px)
            .max(1),
        height_px: obj
            .get("height_px")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(defaults.height_px)
            .max(1),
        use_tangent_rotation: obj
            .get("use_tangent_rotation")
            .and_then(Value::as_bool)
            .unwrap_or(defaults.use_tangent_rotation),
        static_rotation_rad: obj
            .get("static_rotation_rad")
            .and_then(value_as_f32)
            .unwrap_or(defaults.static_rotation_rad),
        normal_offset_px: obj
            .get("normal_offset_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.normal_offset_px),
        letter_spacing_mul: obj
            .get("letter_spacing_mul")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_mul)
            .clamp(0.0, 8.0),
        letter_spacing_px: obj
            .get("letter_spacing_px")
            .and_then(value_as_f32)
            .unwrap_or(defaults.letter_spacing_px)
            .clamp(-10_000.0, 10_000.0),
        lines,
    })
}

fn text_vector_line_from_value(value: &Value) -> Option<TextVectorLine> {
    let obj = value.as_object()?;
    let points = obj
        .get("points")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(text_vector_point_from_value)
        .collect::<Vec<_>>();
    Some(TextVectorLine {
        points,
        corner_smoothing_px: obj
            .get("corner_smoothing_px")
            .and_then(value_as_f32)
            .unwrap_or(0.0)
            .clamp(0.0, 256.0),
        text_direction: text_vector_line_text_direction_from_value(obj.get("text_direction")),
        distance_mode: text_vector_line_distance_mode_from_value(obj.get("distance_mode")),
        flip_text: obj
            .get("flip_text")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn text_vector_point_from_value(value: &Value) -> Option<TextVectorPoint> {
    let obj = value.as_object()?;
    Some(TextVectorPoint {
        x: obj.get("x").and_then(value_as_f32)?,
        y: obj.get("y").and_then(value_as_f32)?,
    })
}

fn text_formula_layout_to_value(layout: &TextFormulaLayoutParams) -> Value {
    json!({
        "x_expr": layout.x_expr.as_str(),
        "y_expr": layout.y_expr.as_str(),
        "rotation_expr": layout.rotation_expr.as_str(),
        "use_tangent_rotation": layout.use_tangent_rotation,
        "t_start": layout.t_start,
        "t_end": layout.t_end,
        "offset_x_px": layout.offset_x_px,
        "offset_y_px": layout.offset_y_px,
        "scale_x": layout.scale_x,
        "scale_y": layout.scale_y,
        "normal_offset_px": layout.normal_offset_px,
        "letter_spacing_mul": layout.letter_spacing_mul,
        "letter_spacing_px": layout.letter_spacing_px,
        "vars": layout.vars,
    })
}

fn text_drawn_lines_layout_to_value(layout: &TextDrawnLinesLayoutParams) -> Value {
    json!({
        "use_tangent_rotation": layout.use_tangent_rotation,
        "static_rotation_rad": layout.static_rotation_rad,
        "normal_offset_px": layout.normal_offset_px,
        "letter_spacing_mul": layout.letter_spacing_mul,
        "letter_spacing_px": layout.letter_spacing_px,
        "color_tolerance": layout.color_tolerance,
        "continuation_alpha": layout.continuation_alpha,
        "start_alpha": layout.start_alpha,
    })
}

fn text_vector_lines_layout_to_value(layout: &TextVectorLinesLayoutParams) -> Value {
    let lines = layout
        .lines
        .iter()
        .map(|line| {
            let points = line
                .points
                .iter()
                .map(|point| json!({ "x": point.x, "y": point.y }))
                .collect::<Vec<_>>();
            json!({
                "points": points,
                "corner_smoothing_px": line.corner_smoothing_px,
                "text_direction": text_vector_line_text_direction_to_str(line.text_direction),
                "distance_mode": text_vector_line_distance_mode_to_str(line.distance_mode),
                "flip_text": line.flip_text,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "width_px": layout.width_px.max(1),
        "height_px": layout.height_px.max(1),
        "use_tangent_rotation": layout.use_tangent_rotation,
        "static_rotation_rad": layout.static_rotation_rad,
        "normal_offset_px": layout.normal_offset_px,
        "letter_spacing_mul": layout.letter_spacing_mul,
        "letter_spacing_px": layout.letter_spacing_px,
        "lines": lines,
    })
}

fn text_vector_line_text_direction_to_str(direction: TextVectorLineTextDirection) -> &'static str {
    match direction {
        TextVectorLineTextDirection::LeftToRight => "left_to_right",
        TextVectorLineTextDirection::RightToLeft => "right_to_left",
    }
}

fn text_vector_line_text_direction_from_value(
    value: Option<&Value>,
) -> TextVectorLineTextDirection {
    match value.and_then(Value::as_str).unwrap_or("left_to_right") {
        "right_to_left" | "rtl" => TextVectorLineTextDirection::RightToLeft,
        "left_to_right" | "ltr" => TextVectorLineTextDirection::LeftToRight,
        _ => TextVectorLineTextDirection::LeftToRight,
    }
}

fn text_vector_line_distance_mode_to_str(mode: TextVectorLineDistanceMode) -> &'static str {
    match mode {
        TextVectorLineDistanceMode::ByLineLength => "by_line_length",
        TextVectorLineDistanceMode::MinimumPreviousDistance => "minimum_previous_distance",
    }
}

fn text_vector_line_distance_mode_from_value(value: Option<&Value>) -> TextVectorLineDistanceMode {
    match value.and_then(Value::as_str).unwrap_or("by_line_length") {
        "minimum_previous_distance" | "min_previous_distance" | "minimum_distance" => {
            TextVectorLineDistanceMode::MinimumPreviousDistance
        }
        "by_line_length" | "line_length" => TextVectorLineDistanceMode::ByLineLength,
        _ => TextVectorLineDistanceMode::ByLineLength,
    }
}

fn load_text_tab_formula_presets() -> HashMap<String, TypingFormulaPreset> {
    let fallback = default_text_tab_formula_presets();
    let user_settings_file = config::user_config_path();
    let Ok(raw) = fs::read_to_string(user_settings_file) else {
        return fallback;
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return fallback;
    };
    let Some(presets_obj) = payload
        .get("TextTab")
        .and_then(Value::as_object)
        .and_then(|text_tab| text_tab.get(TEXT_TAB_FORMULA_PRESETS_KEY))
        .and_then(Value::as_object)
    else {
        return fallback;
    };

    let mut out = fallback;
    for (name, raw_preset) in presets_obj {
        let Some(layout) = text_formula_layout_from_value(raw_preset) else {
            continue;
        };
        out.insert(name.clone(), TypingFormulaPreset { layout });
    }
    out
}

fn save_text_tab_use_system_fonts(enabled: bool) -> Result<(), String> {
    let user_settings_file = config::user_config_path();
    let mut root = if user_settings_file.exists() {
        match fs::read_to_string(&user_settings_file) {
            Ok(raw) => {
                serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::Object(Map::new()))
            }
            Err(_) => Value::Object(Map::new()),
        }
    } else {
        Value::Object(Map::new())
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let root_obj = root.as_object_mut().expect("object ensured");
    let mut text_tab_obj = root_obj
        .get("TextTab")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    text_tab_obj.insert(
        TEXT_TAB_USE_SYSTEM_FONTS_KEY.to_string(),
        Value::Bool(enabled),
    );
    root_obj.insert("TextTab".to_string(), Value::Object(text_tab_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, payload).map_err(|err| err.to_string())
}

fn save_text_tab_create_presets(
    presets: &HashMap<String, TypingCreatePreset>,
) -> Result<(), String> {
    let user_settings_file = config::user_config_path();
    let mut root = if user_settings_file.exists() {
        match fs::read_to_string(&user_settings_file) {
            Ok(raw) => {
                serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::Object(Map::new()))
            }
            Err(_) => Value::Object(Map::new()),
        }
    } else {
        Value::Object(Map::new())
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let root_obj = root.as_object_mut().expect("object ensured");
    let mut text_tab_obj = root_obj
        .get("TextTab")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let mut presets_obj = Map::new();
    let mut names: Vec<&String> = presets.keys().collect();
    names.sort();
    for name in names {
        let Some(preset) = presets.get(name) else {
            continue;
        };
        if preset.primary_font_key.trim().is_empty() {
            continue;
        }
        let mut font_profiles_obj = Map::new();
        let mut font_keys: Vec<&String> = preset.font_profiles.keys().collect();
        font_keys.sort();
        for font_key in font_keys {
            if let Some(profile) = preset.font_profiles.get(font_key) {
                font_profiles_obj.insert(font_key.clone(), profile.clone());
            }
        }
        presets_obj.insert(
            name.clone(),
            json!({
                "primary_font_key": preset.primary_font_key,
                "primary_font_path": preset.primary_font_path,
                "primary_font_label": preset.primary_font_label,
                "font_profiles": font_profiles_obj,
            }),
        );
    }
    text_tab_obj.insert(
        TEXT_TAB_CREATE_PRESETS_KEY.to_string(),
        Value::Object(presets_obj),
    );
    root_obj.insert("TextTab".to_string(), Value::Object(text_tab_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, payload).map_err(|err| err.to_string())
}

fn save_text_tab_formula_presets(
    presets: &HashMap<String, TypingFormulaPreset>,
) -> Result<(), String> {
    let user_settings_file = config::user_config_path();
    let mut root = if user_settings_file.exists() {
        match fs::read_to_string(&user_settings_file) {
            Ok(raw) => {
                serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| Value::Object(Map::new()))
            }
            Err(_) => Value::Object(Map::new()),
        }
    } else {
        Value::Object(Map::new())
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let root_obj = root.as_object_mut().expect("object ensured");
    let mut text_tab_obj = root_obj
        .get("TextTab")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut presets_obj = Map::new();
    let mut names: Vec<&String> = presets.keys().collect();
    names.sort();
    for name in names {
        let Some(preset) = presets.get(name) else {
            continue;
        };
        presets_obj.insert(name.clone(), text_formula_layout_to_value(&preset.layout));
    }
    text_tab_obj.insert(
        TEXT_TAB_FORMULA_PRESETS_KEY.to_string(),
        Value::Object(presets_obj),
    );
    root_obj.insert("TextTab".to_string(), Value::Object(text_tab_obj));

    let payload = serde_json::to_string_pretty(&root).map_err(|err| err.to_string())?;
    if let Some(parent) = user_settings_file.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(user_settings_file, payload).map_err(|err| err.to_string())
}

fn is_font_family_bound(ctx: &egui::Context, family: &egui::FontFamily) -> bool {
    ctx.fonts(|fonts| fonts.definitions().families.contains_key(family))
}

/// Детерминированное имя egui-семейства для UI-превью шрифта в комбобоксе.
/// Зависит только от (путь, индекс начертания), поэтому один и тот же файл всегда
/// регистрируется под одним именем (безопасно разделяется между панелями `create`
/// и `edit`, у которых общий egui-`Context`), а разные файлы получают разные имена.
fn combo_font_family_name(font_path: &Path, face_index: usize) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    font_path.hash(&mut hasher);
    face_index.hash(&mut hasher);
    format!("typing-panel-combo-font-{:016x}", hasher.finish())
}

/// Принадлежит ли шрифт группе `group` (учитывает объединённые копии).
fn font_in_group(font: &FontEntry, group: &str) -> bool {
    font.groups.iter().any(|g| g.as_deref() == Some(group))
}

/// Совпадает ли `raw`-путь с представительным или альтернативным путём шрифта.
fn font_matches_path(font: &FontEntry, raw: &str) -> bool {
    let candidate = Path::new(raw);
    font.path == candidate
        || font.path.to_string_lossy() == raw
        || font
            .alt_paths
            .iter()
            .any(|alt| alt == candidate || alt.to_string_lossy() == raw)
}

fn fit_size_to_box(source_size: [usize; 2], box_size: Vec2) -> Vec2 {
    let src_w = source_size[0].max(1) as f32;
    let src_h = source_size[1].max(1) as f32;
    let box_w = box_size.x.max(1.0);
    let box_h = box_size.y.max(1.0);
    let scale = (box_w / src_w).min(box_h / src_h).min(1.0);
    Vec2::new((src_w * scale).max(1.0), (src_h * scale).max(1.0))
}

fn mark_hscroll_block_on_hover(block: &mut bool, response: &egui::Response) {
    let _ = (block, response);
}

fn apply_horizontal_wheel_scroll_if_idle(ui: &mut egui::Ui, block_by_hovered_param: bool) {
    if block_by_hovered_param || !ui.ui_contains_pointer() {
        return;
    }

    let scroll_delta = ui.ctx().input(|input| {
        // For horizontal-only strip we intentionally treat vertical wheel as horizontal scroll.
        input.smooth_scroll_delta.x + input.smooth_scroll_delta.y
    });
    if scroll_delta.abs() <= f32::EPSILON {
        return;
    }

    ui.scroll_with_delta(Vec2::new(scroll_delta, 0.0));
    consume_wheel_scroll_delta(ui);
}

fn consume_wheel_scroll_delta(ui: &egui::Ui) {
    ui.ctx().input_mut(|input| {
        input.smooth_scroll_delta = Vec2::ZERO;
        input.raw_scroll_delta = Vec2::ZERO;
    });
}

fn wheel_steps_if_hovered(ui: &egui::Ui, response: &egui::Response) -> Option<i32> {
    let _ = (ui, response);
    None
}

/// Строка параметра «значение + переключатель X / X%» (пиксели или проценты от кегля).
///
/// При переключении единицы значение пересчитывается через `font_size_px`, чтобы
/// итоговый результат остался максимально близким (px ↔ % от размера шрифта).
fn px_or_percent_param_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut PxOrPercent,
    range: std::ops::RangeInclusive<f32>,
    wheel_step: f32,
    font_size_px: f32,
    changed: &mut bool,
    block_hscroll_by_hovered_param: &mut bool,
) {
    ui.horizontal(|ui| {
        let min = *range.start();
        let max = *range.end();
        let slider_resp = ui.add(WheelSlider::new(&mut value.value, range).text(label));
        mark_hscroll_block_on_hover(block_hscroll_by_hovered_param, &slider_resp);
        *changed |= slider_resp.changed();
        if let Some(steps) = wheel_steps_if_hovered(ui, &slider_resp) {
            *changed |= apply_wheel_step_f32(&mut value.value, steps, wheel_step, min, max);
        }
        let mut want_percent = value.is_percent;
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(4, 1))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if ui
                    .selectable_label(!want_percent, "X")
                    .on_hover_text("Пиксели")
                    .clicked()
                {
                    want_percent = false;
                }
                if ui
                    .selectable_label(want_percent, "X%")
                    .on_hover_text("Проценты от размера шрифта")
                    .clicked()
                {
                    want_percent = true;
                }
            });
        if want_percent != value.is_percent {
            // Подбираем значение в новой единице с наиболее близким результатом.
            let converted = if want_percent {
                value.as_percent_of(font_size_px)
            } else {
                value.as_px_of(font_size_px)
            };
            value.value = converted.clamp(min, max);
            value.is_percent = want_percent;
            *changed = true;
        }
    });
}

fn apply_wheel_step_f32(value: &mut f32, steps: i32, step_size: f32, min: f32, max: f32) -> bool {
    if steps == 0 {
        return false;
    }
    let prev = *value;
    *value = (*value + steps as f32 * step_size).clamp(min, max);
    (*value - prev).abs() > f32::EPSILON
}

fn apply_wheel_step_u32(value: &mut u32, steps: i32, step_size: u32, min: u32, max: u32) -> bool {
    if steps == 0 || step_size == 0 {
        return false;
    }
    let prev = *value;
    let signed = *value as i64 + steps as i64 * step_size as i64;
    *value = signed.clamp(min as i64, max as i64) as u32;
    *value != prev
}

fn apply_wheel_step_u8(value: &mut u8, steps: i32, step_size: u8, min: u8, max: u8) -> bool {
    if steps == 0 || step_size == 0 {
        return false;
    }
    let prev = *value;
    let signed = i32::from(*value) + steps.saturating_mul(i32::from(step_size));
    let clamped = signed.clamp(i32::from(min), i32::from(max));
    let Ok(next) = u8::try_from(clamped) else {
        return false;
    };
    *value = next;
    *value != prev
}

fn cycle_wrapped_index(index: &mut usize, len: usize, steps: i32) -> bool {
    if len == 0 || steps == 0 {
        return false;
    }

    let prev = (*index).min(len - 1);
    let shift = (steps.unsigned_abs() as usize) % len;
    if shift == 0 {
        return false;
    }

    *index = if steps > 0 {
        (prev + shift) % len
    } else {
        (prev + len - shift) % len
    };
    *index != prev
}

fn cycle_text_shape(shape: &mut TextShape, steps: i32) -> bool {
    let mut idx = match *shape {
        TextShape::Free => 0,
        TextShape::Rectangle => 1,
        TextShape::Oval => 2,
        TextShape::Hexagon => 3,
        TextShape::SoftPeak => 4,
    };
    if !cycle_wrapped_index(&mut idx, 5, steps) {
        return false;
    }

    *shape = match idx {
        0 => TextShape::Free,
        1 => TextShape::Rectangle,
        2 => TextShape::Oval,
        3 => TextShape::Hexagon,
        _ => TextShape::SoftPeak,
    };
    true
}

fn cycle_text_wrap_mode(mode: &mut TextWrapMode, steps: i32) -> bool {
    let mut idx = match *mode {
        TextWrapMode::None => 0,
        TextWrapMode::WholeWords => 1,
        TextWrapMode::Minimal => 2,
        TextWrapMode::Moderate => 3,
        TextWrapMode::Aggressive => 4,
    };
    if !cycle_wrapped_index(&mut idx, 5, steps) {
        return false;
    }

    *mode = match idx {
        0 => TextWrapMode::None,
        1 => TextWrapMode::WholeWords,
        2 => TextWrapMode::Minimal,
        3 => TextWrapMode::Moderate,
        _ => TextWrapMode::Aggressive,
    };
    true
}

fn cycle_text_line_mode(mode: &mut TextLineMode, steps: i32) -> bool {
    let mut idx = match *mode {
        TextLineMode::Horizontal => 0,
        TextLineMode::Vertical => 1,
    };
    if !cycle_wrapped_index(&mut idx, 2, steps) {
        return false;
    }
    *mode = if idx == 0 {
        TextLineMode::Horizontal
    } else {
        TextLineMode::Vertical
    };
    true
}

fn cycle_vertical_line_direction(direction: &mut VerticalLineDirection, steps: i32) -> bool {
    let mut idx = match *direction {
        VerticalLineDirection::LeftToRight => 0,
        VerticalLineDirection::RightToLeft => 1,
    };
    if !cycle_wrapped_index(&mut idx, 2, steps) {
        return false;
    }
    *direction = if idx == 0 {
        VerticalLineDirection::LeftToRight
    } else {
        VerticalLineDirection::RightToLeft
    };
    true
}

fn cycle_text_layout_mode(mode: &mut TextLayoutMode, steps: i32) -> bool {
    let mut idx = match *mode {
        TextLayoutMode::Normal => 0,
        TextLayoutMode::Formula => 1,
        TextLayoutMode::Shape => 1,
        TextLayoutMode::CustomRasterLines | TextLayoutMode::CustomVectorLines => 2,
    };
    if !cycle_wrapped_index(&mut idx, 3, steps) {
        return false;
    }
    *mode = match idx {
        0 => TextLayoutMode::Normal,
        1 => TextLayoutMode::Formula,
        _ => TextLayoutMode::CustomVectorLines,
    };
    true
}

fn compute_typing_vertical_panel_auto_height(
    content_height_px: f32,
    viewport_target_height: f32,
    available_panel_height: f32,
) -> f32 {
    if content_height_px > 0.0 {
        content_height_px
            .min(viewport_target_height)
            .min(available_panel_height)
            .max(TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX)
    } else {
        viewport_target_height
            .min(available_panel_height)
            .max(TYPING_VERTICAL_SECTION_MIN_HEIGHT_PX)
    }
}

fn parse_text_shape_str(raw: &str) -> Option<TextShape> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "free" => Some(TextShape::Free),
        "rectangle" => Some(TextShape::Rectangle),
        "oval" => Some(TextShape::Oval),
        "hexagon" => Some(TextShape::Hexagon),
        "soft_peak" | "soft" | "no_trees" => Some(TextShape::SoftPeak),
        _ => None,
    }
}

fn parse_text_wrap_mode_str(raw: &str) -> Option<TextWrapMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "none" => Some(TextWrapMode::None),
        "whole_words" | "words" | "word" => Some(TextWrapMode::WholeWords),
        "minimal" => Some(TextWrapMode::Minimal),
        "moderate" => Some(TextWrapMode::Moderate),
        "aggressive" | "smart" => Some(TextWrapMode::Aggressive),
        _ => None,
    }
}


fn text_wrap_mode_label(mode: TextWrapMode) -> &'static str {
    match mode {
        TextWrapMode::None => "Нет",
        TextWrapMode::WholeWords => "Слова целиком",
        TextWrapMode::Minimal => "Минимальный перенос",
        TextWrapMode::Moderate => "Умеренный перенос",
        TextWrapMode::Aggressive => "Активный перенос",
    }
}

fn parse_text_line_mode_str(raw: &str) -> Option<TextLineMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "horizontal" => Some(TextLineMode::Horizontal),
        "vertical" => Some(TextLineMode::Vertical),
        _ => None,
    }
}

fn parse_vertical_line_direction_str(raw: &str) -> Option<VerticalLineDirection> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "left_to_right" | "ltr" => Some(VerticalLineDirection::LeftToRight),
        "right_to_left" | "rtl" => Some(VerticalLineDirection::RightToLeft),
        _ => None,
    }
}

fn parse_text_layout_mode_str(raw: &str) -> Option<TextLayoutMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "normal" => Some(TextLayoutMode::Normal),
        "formula" => Some(TextLayoutMode::Formula),
        "shape" => Some(TextLayoutMode::Shape),
        "drawn_lines"
        | "drawn-lines"
        | "drawnlines"
        | "custom_raster_lines"
        | "custom-raster-lines"
        | "customrasterlines" => Some(TextLayoutMode::CustomRasterLines),
        "vector_lines"
        | "vector-lines"
        | "vectorlines"
        | "custom_vector_lines"
        | "custom-vector-lines"
        | "customvectorlines" => Some(TextLayoutMode::CustomVectorLines),
        _ => None,
    }
}

fn parse_kerning_mode_str(raw: &str) -> Option<KerningMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "metric" => Some(KerningMode::Metric),
        "optical" => Some(KerningMode::Optical),
        _ => None,
    }
}

fn parse_color32_value(value: &Value) -> Option<Color32> {
    let arr = value.as_array()?;
    if arr.len() < 3 {
        return None;
    }
    let r = value_as_u8(arr.first()?)?;
    let g = value_as_u8(arr.get(1)?)?;
    let b = value_as_u8(arr.get(2)?)?;
    let a = arr.get(3).and_then(value_as_u8).unwrap_or(255);
    Some(Color32::from_rgba_unmultiplied(r, g, b, a))
}

fn value_as_u8(value: &Value) -> Option<u8> {
    if let Some(v) = value.as_u64() {
        return u8::try_from(v).ok();
    }
    value.as_f64().map(|v| v.round().clamp(0.0, 255.0) as u8)
}

fn value_as_u64(value: &Value) -> Option<u64> {
    if let Some(v) = value.as_u64() {
        return Some(v);
    }

    value.as_f64().and_then(|v| {
        let rounded = v.round();
        if rounded.is_finite() && rounded >= 0.0 && rounded <= u64::MAX as f64 {
            Some(rounded as u64)
        } else {
            None
        }
    })
}

fn value_as_f32(value: &Value) -> Option<f32> {
    value.as_f64().map(|v| v as f32)
}

fn formula_layout_approx_eq(a: &TextFormulaLayoutParams, b: &TextFormulaLayoutParams) -> bool {
    const EPS: f32 = 0.0005;
    if a.x_expr.trim() != b.x_expr.trim() {
        return false;
    }
    if a.y_expr.trim() != b.y_expr.trim() {
        return false;
    }
    if a.rotation_expr.trim() != b.rotation_expr.trim() {
        return false;
    }
    if a.use_tangent_rotation != b.use_tangent_rotation {
        return false;
    }
    if (a.t_start - b.t_start).abs() > EPS
        || (a.t_end - b.t_end).abs() > EPS
        || (a.offset_x_px - b.offset_x_px).abs() > EPS
        || (a.offset_y_px - b.offset_y_px).abs() > EPS
        || (a.scale_x - b.scale_x).abs() > EPS
        || (a.scale_y - b.scale_y).abs() > EPS
        || (a.normal_offset_px - b.normal_offset_px).abs() > EPS
        || (a.letter_spacing_mul - b.letter_spacing_mul).abs() > EPS
        || (a.letter_spacing_px - b.letter_spacing_px).abs() > EPS
    {
        return false;
    }
    for idx in 0..TEXT_FORMULA_USER_VAR_COUNT {
        if (a.vars[idx] - b.vars[idx]).abs() > EPS {
            return false;
        }
    }
    true
}

fn normalize_angle_deg(angle: f32) -> f32 {
    ((angle + 180.0).rem_euclid(360.0)) - 180.0
}

fn parse_effect_cards(effects: &[Value], text_color: Color32) -> Vec<EffectCard> {
    let mut out = Vec::<EffectCard>::new();
    for effect in effects {
        let Some(obj) = effect.as_object() else {
            continue;
        };
        let kind = obj
            .get("effect")
            .or_else(|| obj.get("type"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match kind.as_str() {
            "text_shake" | "text_jitter" | "character_shake" => {
                out.push(EffectCard::TextShake(TextShakeEffectCard {
                    spread_x_px: obj
                        .get("spread_x")
                        .or_else(|| obj.get("spread_x_px"))
                        .or_else(|| obj.get("x"))
                        .and_then(value_as_f32)
                        .unwrap_or(2.0)
                        .clamp(0.0, 256.0),
                    spread_y_px: obj
                        .get("spread_y")
                        .or_else(|| obj.get("spread_y_px"))
                        .or_else(|| obj.get("y"))
                        .and_then(value_as_f32)
                        .unwrap_or(2.0)
                        .clamp(0.0, 256.0),
                    seed: obj
                        .get("seed")
                        .and_then(value_as_u64)
                        .unwrap_or_else(random_seed),
                }));
            }
            "stroke" => out.push(EffectCard::Stroke(StrokeEffectCard {
                width_px: obj
                    .get("width")
                    .and_then(value_as_f32)
                    .unwrap_or(2.0)
                    .clamp(0.0, 24.0),
                color: ColorField::new(
                    obj.get("color")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::BLACK),
                ),
                opacity_mode: match obj
                    .get("opacity_mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "from_contour" => StrokeOpacityMode::FromContour,
                    _ => StrokeOpacityMode::Static,
                },
                transparency_percent: obj
                    .get("transparency")
                    .or_else(|| obj.get("opacity"))
                    .and_then(value_as_f32)
                    .map(|v| {
                        if obj.get("transparency").is_some() {
                            v
                        } else {
                            100.0 - v
                        }
                    })
                    .unwrap_or(0.0)
                    .clamp(0.0, 100.0),
                smoothing: obj
                    .get("smoothing")
                    .or_else(|| obj.get("smooth"))
                    .or_else(|| obj.get("antialias"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                smoothing_strength_percent: obj
                    .get("smoothing_strength")
                    .or_else(|| obj.get("smoothing_strength_percent"))
                    .or_else(|| obj.get("smooth_strength"))
                    .or_else(|| obj.get("antialias_strength"))
                    .and_then(value_as_f32)
                    .unwrap_or(100.0)
                    .clamp(0.0, 100.0),
            })),
            "shadow" => out.push(EffectCard::Shadow(ShadowEffectCard {
                offset_x_px: obj
                    .get("offset_x")
                    .and_then(value_as_f32)
                    .map(|v| v.round() as i32)
                    .unwrap_or(4)
                    .clamp(-400, 400),
                offset_y_px: obj
                    .get("offset_y")
                    .and_then(value_as_f32)
                    .map(|v| v.round() as i32)
                    .unwrap_or(4)
                    .clamp(-400, 400),
                transparency_percent: obj
                    .get("transparency")
                    .or_else(|| obj.get("opacity"))
                    .and_then(value_as_f32)
                    .map(|v| {
                        if obj.get("transparency").is_some() {
                            v
                        } else {
                            100.0 - v
                        }
                    })
                    .unwrap_or(40.0)
                    .clamp(0.0, 100.0),
                blur_radius_px: obj
                    .get("blur")
                    .or_else(|| obj.get("blur_radius"))
                    .or_else(|| obj.get("blur_px"))
                    .and_then(value_as_f32)
                    .unwrap_or(0.0)
                    .clamp(0.0, 128.0),
                color_mode: if obj
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .eq_ignore_ascii_case("source")
                    || obj
                        .get("use_source_color")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                {
                    ShadowColorMode::SourceColors
                } else {
                    ShadowColorMode::SingleColor
                },
                color: ColorField::new(
                    obj.get("color")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::BLACK),
                ),
            })),
            "blur" | "gaussian_blur" => out.push(EffectCard::Blur(BlurEffectCard {
                radius_px: obj
                    .get("radius")
                    .or_else(|| obj.get("radius_px"))
                    .or_else(|| obj.get("blur"))
                    .or_else(|| obj.get("blur_px"))
                    .or_else(|| obj.get("sigma"))
                    .and_then(value_as_f32)
                    .unwrap_or(4.0)
                    .clamp(0.0, 128.0),
            })),
            "motion_blur" | "directional_blur" => {
                out.push(EffectCard::MotionBlur(MotionBlurEffectCard {
                    angle_deg: obj
                        .get("angle_deg")
                        .or_else(|| obj.get("angle"))
                        .and_then(value_as_f32)
                        .unwrap_or(20.0)
                        .clamp(-360.0, 360.0),
                    distance_px: obj
                        .get("distance")
                        .or_else(|| obj.get("distance_px"))
                        .or_else(|| obj.get("offset"))
                        .or_else(|| obj.get("offset_px"))
                        .and_then(value_as_f32)
                        .unwrap_or(11.0)
                        .clamp(0.0, 512.0),
                    sharp_copy_mode: match obj
                        .get("sharp_copy")
                        .or_else(|| obj.get("unblurred_copy"))
                        .or_else(|| obj.get("sharp_copy_mode"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_ascii_lowercase()
                        .as_str()
                    {
                        "over" | "top" | "above" => MotionBlurSharpCopyMode::Over,
                        "under" | "bottom" | "below" => MotionBlurSharpCopyMode::Under,
                        _ => MotionBlurSharpCopyMode::None,
                    },
                }))
            }
            "dry_media" | "chalk_pencil" | "dry_brush" => {
                out.push(EffectCard::DryMedia(DryMediaEffectCard {
                    material: match obj
                        .get("material")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_ascii_lowercase()
                        .as_str()
                    {
                        "chalk" | "mel" => DryMediaMaterial::Chalk,
                        _ => DryMediaMaterial::Pencil,
                    },
                    strength: obj
                        .get("strength")
                        .and_then(value_as_f32)
                        .unwrap_or(0.65)
                        .clamp(0.0, 1.0),
                    seed: obj.get("seed").and_then(value_as_u64).unwrap_or(1),
                    grain_scale_px: obj
                        .get("grain_scale_px")
                        .or_else(|| obj.get("grain_scale"))
                        .or_else(|| obj.get("grain_size_px"))
                        .and_then(value_as_f32)
                        .unwrap_or(2.0)
                        .clamp(0.5, 32.0),
                    grain_amount: obj
                        .get("grain_amount")
                        .or_else(|| obj.get("grain"))
                        .and_then(value_as_f32)
                        .unwrap_or(0.35)
                        .clamp(0.0, 1.0),
                    edge_roughness: obj
                        .get("edge_roughness")
                        .or_else(|| obj.get("roughness"))
                        .and_then(value_as_f32)
                        .unwrap_or(0.45)
                        .clamp(0.0, 1.0),
                    porosity: obj
                        .get("porosity")
                        .or_else(|| obj.get("holes"))
                        .and_then(value_as_f32)
                        .unwrap_or(0.20)
                        .clamp(0.0, 1.0),
                    direction_deg: obj
                        .get("direction_deg")
                        .or_else(|| obj.get("angle_deg"))
                        .or_else(|| obj.get("angle"))
                        .and_then(value_as_f32)
                        .unwrap_or(82.0)
                        .clamp(-360.0, 360.0),
                    directional_amount: obj
                        .get("directional_amount")
                        .or_else(|| obj.get("stroke_amount"))
                        .or_else(|| obj.get("hatching"))
                        .and_then(value_as_f32)
                        .unwrap_or(0.30)
                        .clamp(0.0, 1.0),
                    dust_amount: obj
                        .get("dust_amount")
                        .or_else(|| obj.get("dust"))
                        .and_then(value_as_f32)
                        .unwrap_or(0.08)
                        .clamp(0.0, 1.0),
                    dust_radius_px: obj
                        .get("dust_radius_px")
                        .or_else(|| obj.get("dust_radius"))
                        .and_then(value_as_f32)
                        .unwrap_or(2.0)
                        .clamp(0.0, 32.0),
                    softness_px: obj
                        .get("softness_px")
                        .or_else(|| obj.get("softness"))
                        .or_else(|| obj.get("blur"))
                        .and_then(value_as_f32)
                        .unwrap_or(0.6)
                        .clamp(0.0, 16.0),
                    use_source_color: obj
                        .get("use_source_color")
                        .or_else(|| obj.get("respect_source_color"))
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    color: ColorField::new(
                        obj.get("color")
                            .and_then(parse_color32_value)
                            .unwrap_or(text_color),
                    ),
                }))
            }
            "glow_v1" | "glow_v2" => out.push(EffectCard::Glow(GlowEffectCard {
                version: if kind == "glow_v2" {
                    GlowEffectVersion::V2
                } else {
                    GlowEffectVersion::V1
                },
                radius_px: obj
                    .get("radius")
                    .and_then(value_as_f32)
                    .unwrap_or(16.0)
                    .clamp(0.0, 300.0),
                softness_px: 0.0,
                color: ColorField::new(
                    obj.get("color")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::BLACK),
                ),
                opacity_mode: match obj
                    .get("opacity_mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "from_contour" => StrokeOpacityMode::FromContour,
                    _ => StrokeOpacityMode::Static,
                },
                transparency_percent: obj
                    .get("transparency")
                    .or_else(|| obj.get("opacity"))
                    .and_then(value_as_f32)
                    .map(|v| {
                        if obj.get("transparency").is_some() {
                            v
                        } else {
                            100.0 - v
                        }
                    })
                    .unwrap_or(0.0)
                    .clamp(0.0, 100.0),
                fade_strength: obj
                    .get("fade_strength")
                    .and_then(value_as_f32)
                    .unwrap_or(0.0)
                    .clamp(-100.0, 100.0),
                fade_shift: obj
                    .get("fade_shift")
                    .and_then(value_as_f32)
                    .unwrap_or(0.0)
                    .clamp(-100.0, 100.0),
            })),
            "soft_glow" | "glow_soft" => out.push(EffectCard::Glow(GlowEffectCard {
                version: GlowEffectVersion::Soft,
                radius_px: obj
                    .get("radius")
                    .or_else(|| obj.get("glow_radius"))
                    .and_then(value_as_f32)
                    .unwrap_or(8.0)
                    .clamp(0.0, 300.0),
                softness_px: obj
                    .get("softness")
                    .or_else(|| obj.get("softness_px"))
                    .or_else(|| obj.get("glow_softness"))
                    .or_else(|| obj.get("blur"))
                    .and_then(value_as_f32)
                    .unwrap_or(4.0)
                    .clamp(0.0, 100.0),
                color: ColorField::new(
                    obj.get("color")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::BLACK),
                ),
                opacity_mode: StrokeOpacityMode::FromContour,
                transparency_percent: 0.0,
                fade_strength: 0.0,
                fade_shift: 0.0,
            })),
            "gradient2" => out.push(EffectCard::Gradient2(Gradient2EffectCard {
                color1: ColorField::new(
                    obj.get("color1")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::WHITE),
                ),
                color2: ColorField::new(
                    obj.get("color2")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::BLACK),
                ),
                angle_deg: obj
                    .get("angle_deg")
                    .and_then(value_as_f32)
                    .unwrap_or(90.0)
                    .clamp(-360.0, 360.0),
                width_percent: obj
                    .get("width_percent")
                    .or_else(|| obj.get("gradient_width_percent"))
                    .and_then(value_as_f32)
                    .unwrap_or(100.0)
                    .clamp(1.0, 400.0),
                respect_source_alpha: obj
                    .get("respect_source_alpha")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                fill_mode: match obj
                    .get("fill_mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "specific_color" => Gradient2FillMode::SpecificColor,
                    _ => Gradient2FillMode::AllOpaque,
                },
                target_color: ColorField::new(
                    obj.get("target_color")
                        .and_then(parse_color32_value)
                        .unwrap_or(text_color),
                ),
            })),
            "gradient4" => out.push(EffectCard::Gradient4(Gradient4EffectCard {
                color_top_left: ColorField::new(
                    obj.get("color_top_left")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::WHITE),
                ),
                color_top_right: ColorField::new(
                    obj.get("color_top_right")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::WHITE),
                ),
                color_bottom_left: ColorField::new(
                    obj.get("color_bottom_left")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::BLACK),
                ),
                color_bottom_right: ColorField::new(
                    obj.get("color_bottom_right")
                        .and_then(parse_color32_value)
                        .unwrap_or(Color32::BLACK),
                ),
                width_percent: obj
                    .get("width_percent")
                    .or_else(|| obj.get("gradient_width_percent"))
                    .and_then(value_as_f32)
                    .unwrap_or(100.0)
                    .clamp(1.0, 400.0),
                respect_source_alpha: obj
                    .get("respect_source_alpha")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                fill_mode: match obj
                    .get("fill_mode")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "specific_color" => Gradient4FillMode::SpecificColor,
                    _ => Gradient4FillMode::AllOpaque,
                },
                target_color: ColorField::new(
                    obj.get("target_color")
                        .and_then(parse_color32_value)
                        .unwrap_or(text_color),
                ),
            })),
            "reflect" => out.push(EffectCard::Reflect(ReflectEffectCard {
                axis: match obj
                    .get("axis")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "x" => ReflectAxis::X,
                    _ => ReflectAxis::Y,
                },
            })),
            "shake" => out.push(EffectCard::Shake(ShakeEffectCard {
                angle_deg: obj
                    .get("angle_deg")
                    .and_then(value_as_f32)
                    .unwrap_or(90.0)
                    .clamp(-360.0, 360.0),
                up_px: obj
                    .get("up")
                    .and_then(value_as_f32)
                    .unwrap_or(0.0)
                    .clamp(0.0, 1000.0),
                down_px: obj
                    .get("down")
                    .and_then(value_as_f32)
                    .unwrap_or(40.0)
                    .clamp(0.0, 1000.0),
                steps: obj
                    .get("steps")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(12)
                    .min(128),
                base_fade: obj
                    .get("base_fade")
                    .and_then(value_as_f32)
                    .unwrap_or(0.30)
                    .clamp(0.0, 1.0),
                decay: obj
                    .get("decay")
                    .and_then(value_as_f32)
                    .unwrap_or(0.15)
                    .clamp(0.0, 1.0),
                blur_px: obj
                    .get("blur")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(2)
                    .min(64),
                autogrow: obj.get("autogrow").and_then(Value::as_bool).unwrap_or(true),
                grow_margin_px: obj
                    .get("grow_margin")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(0)
                    .min(1024),
            })),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_field_serializes_straight_alpha_rgba() {
        let color = ColorField::new(Color32::from_rgba_unmultiplied(255, 255, 255, 128));

        assert_eq!(color.rgba(), [255, 255, 255, 128]);
    }

    #[test]
    fn machine_tag_round_trips_through_build_and_parse() {
        let style = TypingInlineTagStyle {
            bold: true,
            italic: false,
            font_label: Some("My Font".to_string()),
            font_size_px: Some(36.0),
            text_color: Some(Color32::from_rgb(0x11, 0x22, 0x33)),
            line_spacing: Some(PxOrPercent::percent(50.0)),
            kerning: Some(PxOrPercent::px(10.0)),
            glyph_stretching: Some([PxOrPercent::percent(120.0), PxOrPercent::px(80.0)]),
            glyph_offset: Some(TypingInlineOffsetStyle {
                global_x: PxOrPercent::px(3.0),
                global_y: PxOrPercent::percent(0.0),
                line: PxOrPercent::px(12.0),
                shift_following: true,
                group_rotation_deg: 30.0,
                glyph_rotation_deg: 0.0,
            }),
        };

        let tag = build_inline_machine_tag(&style);
        assert!(tag.starts_with("<m ") && tag.ends_with('>'));
        let inner = &tag[1..tag.len() - 1];
        let parsed = parse_machine_tag_style(inner).expect("machine tag should parse");

        assert_eq!(parsed, style);
    }

    #[test]
    fn empty_machine_tag_is_not_emitted() {
        assert!(build_inline_machine_tag(&TypingInlineTagStyle::default()).is_empty());
    }

    #[test]
    fn inline_tag_editor_colors_dim_tags_and_whiten_content() {
        let colors = build_inline_tag_editor_text_colors("<b>Пример</b>");

        assert_eq!(
            colors,
            vec![
                TextEditPlusTextColor::new(3..9, INLINE_TAG_CONTENT_TEXT_COLOR),
                TextEditPlusTextColor::new(0..3, INLINE_TAG_DIM_TEXT_COLOR),
                TextEditPlusTextColor::new(9..13, INLINE_TAG_DIM_TEXT_COLOR),
            ]
        );
    }

    #[test]
    fn inline_tag_editor_colors_keep_nested_tags_dimmed() {
        let colors = build_inline_tag_editor_text_colors("<b>А<i>Б</i></b>");
        let outer_content = 3..12;
        let inner_opening_tag = 4..7;

        assert!(
            colors
                .iter()
                .position(|style| style.char_range == outer_content
                    && style.color == INLINE_TAG_CONTENT_TEXT_COLOR)
                .is_some_and(|content_idx| {
                    colors.iter().skip(content_idx + 1).any(|style| {
                        style.char_range == inner_opening_tag
                            && style.color == INLINE_TAG_DIM_TEXT_COLOR
                    })
                })
        );
    }

    fn raw_font(path: &str, group: Option<&str>, hash: u64) -> RawFontFile {
        RawFontFile {
            path: PathBuf::from(path),
            stem: PathBuf::from(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string(),
            group: group.map(ToOwned::to_owned),
            content_hash: hash,
            faces: default_single_face(),
        }
    }

    #[test]
    fn identical_fonts_merge_and_union_groups() {
        // Одинаковое имя + одинаковый хэш в корне и в группе → один шрифт.
        let entries = merge_duplicate_fonts(vec![
            raw_font("/fonts/Разговор.ttf", None, 42),
            raw_font("/fonts/groups/A/Разговор.ttf", Some("A"), 42),
        ]);
        assert_eq!(entries.len(), 1);
        let font = &entries[0];
        assert_eq!(font.label, "Разговор");
        assert!(font.groups.contains(&None));
        assert!(font.groups.contains(&Some("A".to_string())));
        // Альтернативный путь сохранён для сопоставления.
        assert!(font_matches_path(font, "/fonts/groups/A/Разговор.ttf"));
        assert!(font_in_group(font, "A"));
    }

    #[test]
    fn same_name_different_content_stays_separate_and_disambiguated() {
        let mut entries = merge_duplicate_fonts(vec![
            raw_font("/fonts/groups/A/Разговор.ttf", Some("A"), 1),
            raw_font("/fonts/groups/B/Разговор.ttf", Some("B"), 2),
        ]);
        assert_eq!(entries.len(), 2);
        assign_font_disambiguators(&mut entries);
        let suffixes: Vec<Option<String>> =
            entries.iter().map(|font| font.disambig.clone()).collect();
        assert!(suffixes.contains(&Some("A".to_string())));
        assert!(suffixes.contains(&Some("B".to_string())));
    }

    #[test]
    fn unique_name_gets_no_disambiguator() {
        let mut entries = merge_duplicate_fonts(vec![raw_font(
            "/fonts/Уникальный.ttf",
            None,
            7,
        )]);
        assign_font_disambiguators(&mut entries);
        assert_eq!(entries[0].disambig, None);
    }

    #[test]
    fn selecting_missing_overlay_font_sets_warning_and_clears_on_found() {
        let mut state = TypingCreatePanelState::new(false, false);
        state.fonts = merge_duplicate_fonts(vec![raw_font("/fonts/Доступный.ttf", None, 11)]);
        state.selected_font_idx = 0;

        // Шрифт оверлея отсутствует среди доступных → запоминаем его имя.
        state.select_font_by_path_or_label(Some("/fonts/Пропавший.ttf"), Some("Пропавший"));
        assert_eq!(state.missing_font.as_deref(), Some("Пропавший"));

        // Без метки берём имя файла из пути.
        state.select_font_by_path_or_label(Some("/fonts/ДругойПропавший.otf"), None);
        assert_eq!(state.missing_font.as_deref(), Some("ДругойПропавший.otf"));

        // Найденный шрифт снимает блокировку рендера.
        state.select_font_by_path_or_label(Some("/fonts/Доступный.ttf"), Some("Доступный"));
        assert!(state.missing_font.is_none());
        assert_eq!(state.selected_font_idx, 0);
    }
}
