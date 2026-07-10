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
  - загрузка шрифтов config-driven: список = папка `fonts` ПЛЮС пользовательский набор
    импортированных путей к файлам системных шрифтов (`font_settings_store`). Панели
    `Create/Edit` берут снимок путей при создании и подхватывают правки из настроек вживую
    через `poll_font_settings_changes` (ревизия стора). Отдельного чекбокса больше нет.
  - `ComboBox` шрифтов (`Шрифт`) отображает каждый пункт с его собственной гарнитурой:
    UI-шрифт lazily регистрируется в `egui` по `(font_path, face_index)` и кэшируется.
  - Дубликаты шрифтов (одно имя файла в корне/разных группах): `merge_duplicate_fonts`
    объединяет байт-идентичные копии (совпадает имя и хэш содержимого) в один пункт
    `FontEntry` с объединением групп (`groups`) и `alt_paths` для сопоставления по
    сохранённому пути; различающиеся по содержимому остаются раздельными, а
    `assign_font_disambiguators` добавляет к имени название группы в скобках. Скобки
    показывает только `font_display_label` при выбранных «Все группы»; при конкретной
    группе имя без скобок.

Module root note (EN):
This file is the module root of the top panel. It keeps ALL struct/enum/const
definitions and the small `Default`/enum-helper impls; the behavior lives in
child submodules under the `panel/` directory. `impl TypingTopPanelState` is in
`panel/facade.rs`; `impl TypingCreatePanelState` is split across
`panel/create_*.rs`; free-fn slabs are in `panel/text_forms.rs`,
`inline_tags.rs`, `effect_cards.rs`, `fonts.rs`, `presets_io.rs`, `ui_helpers.rs`,
`effect_parse.rs`; unit tests are in `panel/tests.rs`. Child modules use
`use super::*;` and are descendants of `panel`, so they access the models'
private fields directly. See `MODULE_README.md` for the per-file editing map.
*/
use crate::config;
use crate::trace::cat;
use crate::tabs::typing::auto_typing::TypingAutoTypingSettings;
use crate::tabs::typing::tab::TypingExportFormat;
use crate::tabs::typing::tab::TypingTextOverlayLayer;
use crate::tabs::typing::tab::decode_vector_mesh_warp;
use crate::tabs::typing::render_next::forms::{
    self, PeakBase, PresetLabel, TextForm, TextFormPreset,
};
use crate::tabs::typing::segmentation::Conservatism;
use crate::tabs::typing::render_next::{FontFaceCache, load_selected_font_from_path};
use crate::tabs::typing::render_next::render_text_to_image;
use crate::tabs::typing::render_next::FontProvider;
use crate::tabs::typing::render_next::types::{
    AntiAliasingMode, HorizontalAlign, KerningMode, PxOrPercent, RenderedTextImage,
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
// Native-only file dialog; the `rfd` crate is absent on the wasm target.
#[cfg(not(target_arch = "wasm32"))]
use rfd::FileDialog;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use ms_thread as thread;

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
const DEFAULT_PREVIEW_WIDTH_PX: u32 = 300;

/// Localized default preview text shown before the user types anything.
///
/// A runtime accessor rather than a `const` because `t!` is a locale-catalog lookup,
/// not a `const` expression, and the active locale can change at runtime.
fn default_preview_text() -> &'static str {
    t!("typing.panel.default_preview_text")
}

/// Localized label for the "no preset" entry at the top of the preset dropdowns.
///
/// A runtime accessor rather than a `const` because `t!` is a locale-catalog lookup,
/// not a `const` expression, and the active locale can change at runtime.
fn text_preset_none_label() -> &'static str {
    t!("typing.presets.none_option")
}
const TEXT_TAB_USE_LEGACY_INLINE_TAGS_KEY: &str = "use_legacy_inline_tags";
const TEXT_TAB_CREATE_PRESETS_KEY: &str = "create_presets";
const TEXT_TAB_FORMULA_PRESETS_KEY: &str = "formula_presets";
// Per-effect-kind default parameter overrides, keyed by the effect discriminator
// string (see `effect_defaults::effect_kind_key`); value = the one-card JSON object.
const TEXT_TAB_EFFECT_DEFAULTS_KEY: &str = "effect_defaults";
const INLINE_TAG_DIM_TEXT_COLOR: Color32 = Color32::from_gray(120);
const INLINE_TAG_CONTENT_TEXT_COLOR: Color32 = Color32::WHITE;
mod facade;
mod create_state;
mod create_render_data;
mod create_presets;
mod create_sections;
mod create_main_text;
mod create_advanced;
mod create_edit;
mod create_apply;
mod text_forms;
use text_forms::*;
mod inline_tags;
use inline_tags::*;
mod effect_cards;
use effect_cards::*;
mod fonts;
use fonts::*;
mod font_provider;
use font_provider::TabFontProvider;
mod presets_io;
use presets_io::*;
mod ui_helpers;
use ui_helpers::*;
mod effect_parse;
use effect_parse::*;
mod effect_defaults;
mod font_settings;
mod font_settings_store;
mod font_coverage;
use font_coverage::{FontLanguageCoverage, FontLanguageSupport};
use ms_text_util::language::{TextLanguage, text_language};
// Public editor widget for per-effect-kind default parameters, rendered from the
// settings pane; plus the startup seeding of the runtime-global defaults store.
pub(crate) use effect_defaults::{EffectDefaultsEditorState, seed_effect_defaults_from_config};
// Public editor widget for the settings "Настройки шрифтов" block (font categories +
// system-font import), rendered from the settings pane.
pub(crate) use font_settings::FontSettingsEditorState;
// Startup seeding of the runtime-global imported-system-fonts store. The store's
// `pub(in crate::tabs::typing)` mutators are reached by panel descendants via
// `font_settings_store::…`.
pub(crate) use font_settings_store::seed_imported_system_fonts_from_config;

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
            Self::Horizontal => t!("typing.params.line_mode_horizontal"),
            Self::Vertical => t!("typing.params.line_mode_vertical"),
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
    /// Layer that owns the edit panel's saved inline text selection. Kept separate from
    /// `edit_target` (which is nulled on deselection) so the selection survives losing focus and is
    /// reset only when a genuinely different layer is selected.
    inline_selection_owner: Option<TypingEditTarget>,
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
    /// Typesetting language the cached font coverage (`FontEntry.coverage`) was
    /// computed against. Font coverage is cached at load time, so a runtime change
    /// of `ms_text_util::language::text_language()` would leave it stale; `draw`
    /// compares this against the current language and reloads both font lists when
    /// they differ (see `facade.rs`). Seeded from the current language so the first
    /// frame never triggers a spurious reload.
    coverage_language: TextLanguage,
}

#[derive(Clone, Default)]
pub(super) enum TypingExportUiStatus {
    #[default]
    Hidden,
    /// Whole-project page preload running before a deferred export (Phase 2): `done`/`total` come from
    /// `TypingTextOverlayLayer::preload_all_pages_progress`. Shown in the same panel slot as `Running`.
    Preparing {
        done: usize,
        total: usize,
    },
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
        let create_panel = TypingCreatePanelState::new(true);
        let edit_panel = TypingCreatePanelState::new(false);
        Self {
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
            inline_selection_owner: None,
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
            coverage_language: text_language(),
        }
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
            Self::Parameters => t!("typing.panel.params_tab"),
            Self::Effects => t!("typing.panel.effects_tab"),
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
            Self::Actions => t!("typing.panel.actions_tab"),
            Self::Layers => t!("typing.panel.layers_tab"),
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
    /// How well this font covers the program language's writing system, computed
    /// once at load time from the representative face. Drives the red/yellow
    /// highlight in the font dropdown.
    coverage: FontLanguageCoverage,
    /// Original family/name read from the font file (representative face); future
    /// virtual fonts synthesize it as `VirtualFont_a_b_c`. Persisted so PSD export
    /// and future virtual fonts can recover the real font identity by name.
    original_name: String,
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
            Self::TextShake => t!("typing.effects.text_shake_title"),
            Self::Stroke => t!("typing.effects.stroke_title"),
            Self::Shadow => t!("typing.effects.shadow_title"),
            Self::Blur => t!("typing.effects.blur_title"),
            Self::MotionBlur => t!("typing.effects.motion_blur_title"),
            Self::DryMedia => t!("typing.effects.dry_media_title"),
            Self::GlowV1 => t!("typing.effects.glow_v1_title"),
            Self::GlowV2 => t!("typing.effects.glow_v2_title"),
            Self::SoftGlow => t!("typing.effects.soft_glow_title"),
            Self::Gradient2 => t!("typing.effects.gradient2_title"),
            Self::Gradient4 => t!("typing.effects.gradient4_title"),
            Self::Reflect => t!("typing.effects.reflection_title"),
            Self::Shake => t!("typing.effects.shake_title"),
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
    /// Font source for this render, captured at dispatch time so a later font
    /// reload cannot change the fonts under an in-flight preview render.
    fonts: Arc<dyn FontProvider>,
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

/// Read-only inputs for `draw_right_section`: current panel/editor state the right-side actions
/// column reflects (mask visibility, clean-overlay visibility, movement mode, export config).
struct TypingRightSectionInputs<'a> {
    /// Whether the clip-mask panel is currently open (drives the toggle button label).
    mask_panel_open: bool,
    /// Whether clean overlays are currently shown (drives the checkbox state).
    clean_overlays_visible: bool,
    /// Whether strict pixel-snapped movement is enabled (drives the checkbox state).
    strict_pixel_movement: bool,
    /// Default directory for the export folder picker, when known.
    export_default_dir: Option<&'a Path>,
    /// Current export progress/result to render.
    export_status: &'a TypingExportUiStatus,
    /// Currently selected export format.
    export_format: TypingExportFormat,
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
    /// App-side font source handed to every render: maps a working name (font
    /// label) to bytes/face. Rebuilt whenever `fonts` is (re)assigned and shared
    /// (`Arc`) with background render threads.
    font_provider: Arc<dyn FontProvider>,
    font_groups: Vec<String>,
    selected_font_group: Option<String>,
    /// Snapshot of the user-imported system-font FILE paths (from
    /// `font_settings_store`), merged with the folder fonts by `spawn_font_reload`.
    imported_system_fonts: Vec<PathBuf>,
    /// Last-seen `font_settings_store` revision; when it advances, `poll_font_settings_changes`
    /// refreshes `imported_system_fonts` and reloads the font list live.
    imported_fonts_revision: u64,
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
    /// Global rotation of the whole text block in degrees, applied to glyph
    /// outlines while still vector (before rasterization). 0.0 = no rotation.
    global_rotation_deg: f32,
    /// Perpendicular placement of glyphs relative to the line/path, in percent
    /// `[-100, 100]`. `0` centers the glyph ink on the line, `+100` above
    /// (сверху), `-100` below (снизу). Only shown/used for line-based layouts
    /// (`Formula`, `CustomVectorLines`).
    line_placement_percent: f32,
    /// Raw `raster_transform` object carried verbatim through render_data
    /// rebuilds; authored on the canvas in Phase 3. `None` = no warp; a `Some`
    /// value is re-emitted into `text_params` on every rebuild so it survives
    /// text/param edits, and decoded for the live preview render.
    pending_raster_transform: Option<serde_json::Value>,
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
    anti_aliasing: AntiAliasingMode,
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
    no_break: bool,
    align: Option<HorizontalAlign>,
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
    NoBreak,
    Align(HorizontalAlign),
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
        Self::new(true)
    }
}

/// Одна найденная копия файла шрифта до объединения дубликатов.
struct RawFontFile {
    path: PathBuf,
    stem: String,
    group: Option<String>,
    content_hash: u64,
    faces: Vec<FontFaceEntry>,
    coverage: FontLanguageCoverage,
    /// Original family/name read from the representative face of this file
    /// (fallback: post_script_name, then the file stem). See `FontEntry.original_name`.
    original_name: String,
}

#[cfg(test)]
mod tests;
