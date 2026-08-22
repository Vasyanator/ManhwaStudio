/*
FILE HEADER (tabs/typing/panel.rs)
- Назначение: состояние панели вкладки `Текст` для режимов `Создание` и
  `Редактирование` выбранного оверлея. Сами панели живут в панельном доке
  (`src/widgets/panel_dock`): вкладки `typing.preview`, `typing.params`,
  `typing.effects`, `typing.actions` объявляются в `tab.rs`, а их тела — методы
  `TypingTopPanelState` в `panel/facade.rs`. Позиция, размер, сворачивание и
  активная вкладка принадлежат раскладке дока, а не этому модулю.
- Ключевые сущности:
  - `TypingTopPanelState`: общее состояние панели (mode, create/edit state,
    биндинг к выделенному оверлею, переключатель панели маски обрезки и очередь
    edit-запросов в `tab.rs`, состояние чекбокса видимости clean-overlay,
    а также состояние панели `Авто-тайп` (debug + параметры смещения)).
    `active_main_tab` — только зеркало того, какая из вкладок
    `Параметры`/`Эффекты` рисовалась последней; его единственный потребитель —
    `emit_edit_request`, который по нему отличает правку эффектов картинки от
    чистой трансформации.
- `TypingCreatePanelState`: параметры текста/эффектов, загрузка шрифтов, рендер preview
  в фоне (включается только для режима `Создание`), память параметров по каждому шрифту
  и именованные пресеты (`fonts/presets.json`, версия 1: ОДИН ключ-идентичность главного
  шрифта + профили ТОЛЬКО тех шрифтов, что правились в этой сессии), а также
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
  (cosmic-text, кернинг пар) и передаёт как `LineWidthMetric` в `forms::search_forms`;
  при недоступном шрифте — `CharWidthMetric` (счёт символов); байты шрифта берутся
  из `FontProvider` ФОНОВЫМ потоком (`poll_advanced_form_font`), и ПЕРВЫЙ поиск ждёт
  их прихода — иначе один и тот же кэш строился бы дважды (сначала посимвольно,
  потом попиксельно). Висящая пунктуация
  оверлея учитывается (при включённой края не идут в ширину). Метрика
  перестраивается при смене текста/шрифта/начертания/висячести
  (`AdvancedFormMetricSignature`).
  ПЕРЕБОР НЕ ИДЁТ НА GUI-ПОТОКЕ. Весь вход поиска описан ключом
  `AdvancedFormSearchKey` (текст, пресет, `AdvancedFormMetricSignature`, ручки
  перебора из `AdvancedFormParams`, высота строки в em и диапазоны фильтров);
  его смена взводит ~200 мс debounce, после которого запускается именованный
  воркер `typing-form-search`, а замена задачи взводит её `Arc<AtomicBool>`
  через `Drop` — то есть отменяет предыдущую без явного вызова на каждой
  площадке мутации. Пока поиск в полёте, окно продолжает рисовать ПРЕЖНИЙ
  результат и строку «пересчёт», а не пустую сетку. Диапазоны числа строк и
  ширины — ВХОД поиска, пока включён `filters_prune` (тогда их границы берутся
  из последнего НЕограниченного прогона, иначе сузивший себя фильтр было бы не
  расширить); пиковость, неравномерность и консервативность остаются фильтрами
  показа. Сама галочка `filters_prune` в БАЗУ ключа не входит, поэтому её
  переключение в любую сторону диапазоны СОХРАНЯЕТ и лишь меняет, попадают ли
  они в перебор. Порядок карточек — `text_forms::order_advanced_forms` (слой C плана
  `dev-docs/text_forms_ranking_plan.md`): порог качества, корзины по числу строк,
  уклон в узкие, круговой показ. Порог качества и приоритет узких форм
  (`AdvancedFormOrderKey`) НЕ перезапускают перебор — только пересортировку.
  Ручки перебора живут в сворачиваемой секции «Параметры поиска» самого окна
  (`advanced_form_params`, персист `TextTab.advanced_form_search`).
  Само окно стартует
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
  - `TypingTopPanelState::draw_preview_tab_body`: тело вкладки `typing.preview`
    панельного дока; сама панель (позиция/размер/сворачивание) принадлежит доку.
  - `TypingTopPanelState::draw_params_tab_body` / `draw_effects_tab_body` /
    `draw_actions_tab_body`: тела вкладок дока `typing.params`, `typing.effects`
    и `typing.actions`; для image-оверлея вкладка `Параметры` показывает только
    трансформацию, а `Эффекты` остаётся доступной.
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
  - Нулевым пунктом списка идёт синтетический «Встроенный шрифт интерфейса»
    (`FontEntryKind::BundledUiStack`) — бандловый стек `fonts/ui` как обычный выбираемый
    шрифт. Единственный пункт с локализуемой ПОДПИСЬЮ и зарезервированной, НЕ
    локализуемой идентичностью; полный контракт — в `panel/MODULE_README.md`.

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
use crate::tabs::typing::tab::decode_vector_mesh_warp;
use crate::tabs::typing::render_next::forms::{
    self, InlineTagScope, PeakBase, PresetLabel, TextForm, TextFormPreset,
};
use crate::tabs::typing::segmentation::Conservatism;
use crate::tabs::typing::render_next::{FontContent, FontFaceCache, load_font_content};
use crate::tabs::typing::render_next::render_text_to_image;
use crate::tabs::typing::render_next::FontProvider;
use crate::tabs::typing::render_next::types::{
    AntiAliasingMode, FAUX_THICKEN_PERCENT_MAX, FAUX_THICKEN_PERCENT_MIN, FauxBoldParams,
    FontFallbackReport, HorizontalAlign, KerningMode,
    LinePlacementReference, PxOrPercent, RenderExtraInfoRequest, RenderedTextImage,
    TEXT_FORMULA_USER_VAR_COUNT, parse_machine_tag,
    TextDrawnLinesLayoutParams, TextFormulaLayoutParams, TextLayoutMode, TextLineMode,
    TextRenderParams, TextShape, TextVectorLine, TextVectorLineDistanceMode,
    TextVectorLineTextDirection, TextVectorLinesLayoutParams, TextVectorPoint, TextWrapMode,
    VerticalLineDirection,
};
use crate::widgets::{
    ColorPresets, SeedSpinBox, TabExtras, TextEditPlus, TextEditPlusTextColor,
    ViewportColorSelector, WheelComboBox, WheelSlider, WheelSpinBox, random_seed,
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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use ms_thread as thread;
// The panel compiles for wasm too, where `std::time::Instant` panics; `web_time`
// is the workspace-wide shim (same API, browser clock under wasm).
use web_time::{Duration, Instant};

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
use create_main_text::{
    FontSectionGates, ParamSectionId, collapsing_param_section, section_flag_key,
};
mod create_advanced;
mod create_edit;
mod create_apply;
// The SINGLE owner of the persisted `render_data.text_params` schema (version, frozen
// defaults, write/read). `pub(in crate::tabs::typing)` because the tab-side codec and
// the PSD export read stored payloads through it too.
pub(in crate::tabs::typing) mod text_params_schema;
// The eight user knobs of the advanced text-form search: their supported ranges and
// defaults, the process-global runtime value, the persisted JSON shape and the mapping
// onto the engine's `FormSearchParams`. `pub(crate)` (re-exported by `tabs::typing`)
// because the startup seed (`main.rs`) and the config writer (`tabs::settings`) live
// outside the typing subtree.
pub(crate) mod advanced_form_params;
use advanced_form_params::AdvancedFormParams;
mod text_forms;
use text_forms::*;
mod inline_tags;
use inline_tags::*;
mod effect_cards;
use effect_cards::*;
// Font discovery/loading + the per-font settings store/data. Exposed to the typing
// subtree (`pub(in crate::tabs::typing)`) so the `font_admin` facade can wrap them for
// the settings font-settings UI, which lives OUTSIDE typing; nothing here is `pub(crate)`.
pub(in crate::tabs::typing) mod fonts;
use fonts::*;
mod font_provider;
use font_provider::TabFontProvider;
mod presets_io;
use presets_io::*;
// The SINGLE owner of `fonts/presets.json` (the create-preset document), its atomic write
// and the one-shot migration out of `user_config.TextTab.create_presets`.
mod presets_store;
/// Shared crash-safe write recipe + optimistic-concurrency vocabulary of the panel's two
/// JSON documents (`fonts_data.json`, `presets.json`).
pub(in crate::tabs::typing) mod doc_store;
mod ui_helpers;
use ui_helpers::*;
mod effect_parse;
use effect_parse::*;
mod effect_defaults;
pub(in crate::tabs::typing) mod font_settings_store;
pub(in crate::tabs::typing) mod fonts_data;
mod font_coverage;
use font_coverage::{FontLanguageCoverage, FontLanguageSupport};
mod char_table;
/// The SINGLE owner of the title-scoped color-preset document
/// (`{title_dir}/color_presets.json`) offered by the tab's color pickers.
mod color_presets_store;
use color_presets_store::ColorPresetsStore;
use ms_text_util::language::{TextLanguage, text_language};
// Public editor widget for per-effect-kind default parameters, rendered from the
// settings pane; plus the startup seeding of the runtime-global defaults store.
pub(crate) use effect_defaults::{EffectDefaultsEditorState, seed_effect_defaults_from_config};
// Startup seeding of the runtime-global imported-system-fonts store. The store's
// `pub(in crate::tabs::typing)` mutators are reached by panel descendants via
// `font_settings_store::…`.
pub(crate) use font_settings_store::seed_imported_system_fonts_from_config;

/// One saved create preset: the font it selects plus the per-font parameter profiles it
/// restores. Persisted in `fonts/presets.json` (`presets_store`), never in `user_config`.
///
/// `font` and every `font_profiles` key are the font IDENTITY. The three historical
/// competing references (`primary_font_key` / `primary_font_path` / `primary_font_label`)
/// collapsed into this ONE key in phase 5 of
/// `dev-docs/font_identity_postscript_plan.md`. A value the migration could not resolve to
/// a loaded font is kept VERBATIM in its legacy spelling rather than dropped — it is the
/// only remaining clue about the font it meant — and `create_presets::apply_preset_by_name`
/// still resolves such a leftover through the one legacy door.
///
/// The profiles here are the preset's OWN overrides; the font's DEFAULT profile lives in
/// `fonts_data.fonts.<identity>.profile` ("variant A" of the same plan), so presets stay
/// independent of each other and of what every font remembers on disk.
#[derive(Debug, Clone, Default)]
struct TypingCreatePreset {
    font: String,
    font_profiles: HashMap<String, Value>,
}

#[derive(Clone)]
struct TypingFormulaPreset {
    layout: TextFormulaLayoutParams,
}

/// Something a background `fonts/presets.json` worker has to tell the GUI thread.
///
/// One channel for all of it, because every variant ends in the same place: the create
/// panel's `presets_by_name` (or its status line). Reading, migrating and writing the
/// document all happen OFF the GUI thread (CLAUDE.md §5); what needs the panel — the font
/// list the legacy references resolve against, and the widgets — happens in
/// `create_presets::poll_preset_store_events`.
#[derive(Debug)]
enum PresetStoreEvent {
    /// The startup read finished: the stored document (empty when there is none yet) and,
    /// when a one-shot migration is owed, the legacy `user_config` payload to convert.
    Seeded {
        /// Presets read from `fonts/presets.json`.
        presets: HashMap<String, TypingCreatePreset>,
        /// `Some` only when the document was missing or corrupt, i.e. when the one-shot
        /// migration out of `user_config.json` still has to run. An empty vector means
        /// there was nothing to migrate — the legacy-key cleanup still runs.
        legacy: Option<Vec<presets_store::LegacyPresetEntry>>,
    },
    /// A save found presets another running app instance had written and merged them into
    /// the document; the panel adopts them so its next snapshot cannot drop them again.
    MergedFromDisk(HashMap<String, TypingCreatePreset>),
    /// A save failed. Carries the technical reason for the log line and the status line.
    SaveFailed(String),
}

/// What kind of evidence resolved a font reference persisted by an OLDER build
/// (`create_state::match_font_by_legacy_reference`).
///
/// The distinction is the whole point: a stored NAME (identity, family, label, stem) is
/// evidence about the FONT, while a stored PATH is only evidence that some font file sits
/// at that location today. A caller that SELECTS a font must act on `ByName` only —
/// treating `PathOnly` as proof is how a replaced font file used to silently re-render a
/// layer in a different typeface (`dev-docs/font_identity_postscript_plan.md`, safety
/// rule D). A caller re-keying stored data may accept `PathOnly` as the weakest form, but
/// must rank it below every name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LegacyFontMatch {
    /// A stored name resolved to the font at this index.
    ByName(usize),
    /// No stored name resolved, but the stored path still points at this loaded font.
    PathOnly(usize),
}

impl LegacyFontMatch {
    /// Index of the matched font, whatever the evidence was.
    #[must_use]
    pub(super) fn font_idx(self) -> usize {
        match self {
            Self::ByName(idx) | Self::PathOnly(idx) => idx,
        }
    }
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
    mode: TypingTopPanelMode,
    /// Which of the two main dock tabs («Параметры» / «Эффекты») drew last.
    ///
    /// The dock owns tab activation; this is only a MIRROR, refreshed by whichever
    /// of the two bodies ran this frame. Its single consumer is
    /// `emit_edit_request`, which must know whether an image overlay is being
    /// edited through its effects (re-render) or only its transform (no re-render)
    /// — including when the request comes from a hotkey rather than the panel.
    active_main_tab: TypingMainTab,
    create_panel: TypingCreatePanelState,
    edit_panel: TypingCreatePanelState,
    /// The ONE color-preset set of the tab, shared by both panels above.
    ///
    /// It lives here rather than in `TypingCreatePanelState` precisely because there
    /// are two of those: a per-panel set would let «Создание» and «Редактирование»
    /// drift apart, and would load and save the same title document twice.
    color_presets: ColorPresetsStore,
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
    /// Pending in-app deep-link request, drained each frame from either sub-panel's
    /// font-group "?" help icon (`draw`) and exposed to the app via `take_settings_link`
    /// so it can switch to the settings tab and reveal the target block.
    pending_settings_link: Option<crate::settings_shared::SettingsDeepLink>,
    auto_typing_panel_open: bool,
    auto_typing_debug_visuals: bool,
    auto_typing_extra_downward_shift_percent: f32,
    strict_pixel_movement: bool,
    /// "Помочь с центровкой" (centering assist) toggle. When on, production text renders request the
    /// renderer's mean/median centers, the canvas draws a page-anchored guide frame with corner handles
    /// over the selected text layer, and the layer stays centered on the bound center across re-renders.
    /// Persisted in `user_config.json` (`TextTab.centering_assist_enabled`); default `false`.
    centering_assist_enabled: bool,
    /// Mirror of the CURRENTLY EDITED layer's "sticky centers" bit (see
    /// `TypingSelectedOverlayForEdit::has_centering_centers`). `false` in create mode and whenever
    /// nothing is selected. The panel owns `centering_assist_enabled` but not the overlay runtimes,
    /// so this is how the edit dispatch learns that the target already has stored centers.
    edit_overlay_has_centering_centers: bool,
    /// Which overlay center the assist frame binds to (image / mean / median). Transient; default `Mean`.
    centering_assist_kind: CenteringAssistCenterKind,
    /// "Показывать центр" (show center) toggle inside the centering-assist block. Gates ONLY the drawn
    /// bound-center marker (the red cross+circle); the guide frame, corner handles, binding, and
    /// renderer center computation stay governed by `centering_assist_enabled` alone. Persisted in
    /// `user_config.json` (`TextTab.centering_show_center`); default `true`.
    centering_show_center: bool,
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
        /// What the OUTPUT FORMAT could not express, in a run that otherwise succeeded —
        /// shown under the success line so an approximation is never silent. Today only
        /// the PSD font-name ambiguity (`psd_export::AmbiguousExportFont`). Usually empty.
        warnings: Vec<String>,
    },
    Error {
        message: String,
    },
}

/// Which font the on-canvas text EDITOR should draw its text in.
///
/// Carries the font IDENTITY, not a path: the bytes are obtained through the panel's
/// `FontProvider` (the same resolution the renderer performs), so the editor cannot end
/// up showing a different file than the one that will be rendered, and the tab-side cache
/// keys on the identity as well.
#[derive(Clone)]
pub(super) struct TypingEditorFontSpec {
    pub font_identity: String,
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
    /// The selected TEXT overlay already carries renderer-measured centering-assist centers (the
    /// "sticky centers" bit). Mirrored onto the panel so the EDIT render dispatch keeps requesting
    /// them even with the assist off, instead of erasing the persisted value. Always `false` for a
    /// raster target and for an image overlay.
    pub has_centering_centers: bool,
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

/// Which overlay center the "Помочь с центровкой" (centering assist) guide frame is BOUND to. The
/// selected kind chooses both the drawn marker and the point kept on the frame center. `Mean`/`Median`
/// come from the renderer's extra-info; when that metric is absent they fall back to the plain image
/// center (`Image`). Transient UI state (not persisted); default `Mean`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum CenteringAssistCenterKind {
    Image,
    Mean,
    Median,
}

/// Cycles the bound-center kind by `steps` mouse-wheel notches (positive = forward), wrapping around
/// the three kinds. Used by the panel's `WheelComboBox` wheel handler.
pub(super) fn cycle_centering_assist_kind(
    current: CenteringAssistCenterKind,
    steps: i32,
) -> CenteringAssistCenterKind {
    const ORDER: [CenteringAssistCenterKind; 3] = [
        CenteringAssistCenterKind::Image,
        CenteringAssistCenterKind::Mean,
        CenteringAssistCenterKind::Median,
    ];
    let current_idx = ORDER.iter().position(|k| *k == current).unwrap_or(0);
    let len = i32::try_from(ORDER.len()).unwrap_or(1).max(1);
    let next_idx = (i32::try_from(current_idx).unwrap_or(0) + steps).rem_euclid(len);
    ORDER[usize::try_from(next_idx).unwrap_or(0)]
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
        let mut create_panel = TypingCreatePanelState::new(true);
        let mut edit_panel = TypingCreatePanelState::new(false);
        // Both panels start with an EMPTY font list and share ONE background load of it:
        // reading, hashing and parsing every font file must not happen on the GUI thread
        // (CLAUDE.md §5), and it must not happen twice for one startup either.
        create_state::spawn_shared_font_reload(&mut create_panel, &mut edit_panel);
        Self {
            mode: TypingTopPanelMode::CreateText,
            active_main_tab: TypingMainTab::Parameters,
            create_panel,
            edit_panel,
            color_presets: ColorPresetsStore::default(),
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
            pending_settings_link: None,
            pending_create_image_request: None,
            auto_typing_panel_open: false,
            auto_typing_debug_visuals: false,
            auto_typing_extra_downward_shift_percent: 0.0,
            strict_pixel_movement: true,
            centering_assist_enabled: false,
            edit_overlay_has_centering_centers: false,
            centering_assist_kind: CenteringAssistCenterKind::Mean,
            centering_show_center: true,
            coverage_language: text_language(),
        }
    }
}

/// The two main dock tabs of the «Текст» tab's parameters panel.
///
/// Only a mirror of what the dock drew — the tab captions and the tab identities
/// (`typing.params` / `typing.effects`) live in `tab.rs`, next to the other dock
/// tab declarations.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
enum TypingMainTab {
    #[default]
    Parameters,
    Effects,
}

/// What a `FontEntry` stands for.
///
/// Almost every entry is a real font FILE. The one exception is the synthetic entry
/// for the bundled `fonts/ui` stack (`dev-docs/unicode_base_font_plan.md`, phase 5):
/// it is shown in the typing font combo as the "built-in interface font", points at
/// the FIRST core file of the stack (so previews, the advanced-form metric and PSD
/// export all have a real file to work with), and gets the rest of the stack behind
/// it for free through the renderer's `common_fallback` chain.
#[derive(Clone, Copy, Debug)]
pub(in crate::tabs::typing) enum FontEntryKind {
    /// A real font file: found in the project fonts dir or imported by the user.
    File,
    /// The synthetic bundled-stack entry, carrying the core font whose bytes
    /// `ms-fonts` already holds resident for the whole process.
    BundledUiStack(&'static ms_fonts::StackFont),
}

// Re-exported (type only) crate-wide via `crate::tabs::typing::font_admin`, so the
// settings font-settings UI can hold it. The type is `pub(crate)` but its FIELDS stay
// private to the typing subtree (external code cannot construct or mutate it); external
// readers go through the `pub(crate)` accessors below.
#[derive(Clone)]
pub(crate) struct FontEntry {
    /// Whether this is a real font file or the synthetic bundled-stack entry.
    /// Governs the DISPLAY name (the bundled entry's is localized) and keeps the
    /// bundled entry's reserved identity out of the collision-aware identity pass.
    kind: FontEntryKind,
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
    /// PostScript name (`name` table id 6) of the REPRESENTATIVE face, i.e. always
    /// `faces[0].post_script_name`, captured structurally at load time so no consumer
    /// has to parse it back out of the decorated face `label`. Empty only when the
    /// file could not be parsed (`fontdb` refuses a face without a PostScript name)
    /// or for the synthetic bundled-stack entry, which stands for a whole font chain
    /// rather than one file.
    post_script_name: String,
    /// Hash of the representative FILE's raw bytes (`fonts::font_content_hash`), i.e.
    /// the same value that keys the duplicate merge. It is the source of the identity
    /// COLLISION SUFFIX (`fonts::suffixed_font_identity_name`), which is why it is
    /// carried on the entry rather than dropped after the merge: the suffix must be a
    /// pure function of the entry's OWN bytes.
    ///
    /// `0` means "not computed": the synthetic bundled-stack entry (which stands for a
    /// chain of files), a file that could not be READ at all, and the system-font
    /// picker catalog (`fonts::load_system_fonts` enumerates faces through `fontdb`
    /// without reading whole files). Those lists never feed
    /// `assign_font_identity_names`, so a shared `0` cannot fabricate a collision on a
    /// panel list.
    content_hash: u64,
    /// Optional user display-name override from `fonts_data.json`, resolved at load
    /// time via `font_settings_store::font_display_name_override`. DISPLAY ONLY: it
    /// changes the name shown in the UI, never the render/inline-tag identity.
    display_name: Option<String>,
    /// Canonical render/inline-tag identity: the representative face's POSTSCRIPT NAME
    /// (`name` table id 6), computed for the FINALIZED panel font list by
    /// `fonts::assign_font_identity_names`.
    ///
    /// Stored with its original casing and compared case-insensitively. Two files
    /// claiming one PostScript name with DIFFERENT bytes each get
    /// `"{ps_name}%{16 hex digits of their own content hash}"`, so an entry's identity
    /// is derived from its own bytes and does not shift when another claimant of the name
    /// is added or removed. The separator is a character the PostScript spec FORBIDS
    /// (`fonts::IDENTITY_HASH_SEPARATOR`), so a suffixed identity can never collide with
    /// some other font's real name. A file with no VALID, readable PostScript name falls
    /// back to `fonts::base_font_identity_str`'s rule (family name, else file-stem
    /// `label`). Set to the per-entry base default at construction; overwritten by
    /// `assign_font_identity_names` once the full list is known.
    identity_name: String,
    /// Per-VIRTUAL-group display aliases for this font, keyed by the (merged) group
    /// name → the alias to SHOW while that group is active. Populated by
    /// `fonts::apply_virtual_groups` from each membership's optional per-group alias.
    /// DISPLAY ONLY: never a resolution key, never persisted into layers/presets, and
    /// never sent to the renderer — it only changes what the font-selection combo
    /// shows while the owning virtual group is the active group. Empty by default and
    /// for fonts with no aliased virtual membership.
    virtual_group_aliases: BTreeMap<String, String>,
}

impl FontEntry {
    /// Name to SHOW in the UI: the user display-name override when set, else `label`.
    ///
    /// This is DISPLAY ONLY. The render/inline-tag identity is `render_identity_name()`
    /// (the representative face's PostScript name), with the family name, label and
    /// file-stem kept as legacy resolution aliases; a display override must never reach
    /// any of those resolution paths. `pub(crate)` so the settings font-settings UI
    /// (via `font_admin`) can present it.
    ///
    /// The bundled-stack entry is the one entry whose shown name is LOCALIZED: its
    /// stored `label`/identity is a fixed reserved string (it is persisted into
    /// projects and must never depend on the interface language), so the human name
    /// is resolved here, at the presentation site, on every call.
    pub(crate) fn display_label(&self) -> &str {
        match self.kind {
            FontEntryKind::BundledUiStack(_) => t!("typing.fonts.bundled_ui_font_label"),
            FontEntryKind::File => self.display_name.as_deref().unwrap_or(&self.label),
        }
    }

    /// The bundled `fonts/ui` stack font this entry stands for, or `None` for a real
    /// font file. `TabFontProvider` uses it to hand the renderer the process-resident
    /// `'static` bytes instead of reading the file a second time.
    pub(in crate::tabs::typing) fn bundled_stack_font(&self) -> Option<&'static ms_fonts::StackFont> {
        match self.kind {
            FontEntryKind::BundledUiStack(font) => Some(font),
            FontEntryKind::File => None,
        }
    }

    /// Name to SHOW for this font WITHIN a given active font group.
    ///
    /// When `active_group` is `Some(group)` and this font carries a per-group alias for
    /// `group` (a VIRTUAL-group membership alias from `fonts_data`), that alias is
    /// returned; otherwise this falls back to `display_label()`. DISPLAY ONLY — like
    /// `display_label`, the result is never a resolution key, never persisted, and never
    /// reaches the renderer; it only changes what the font-selection combo shows.
    pub(in crate::tabs::typing) fn display_label_in_group(&self, active_group: Option<&str>) -> &str {
        if let Some(group) = active_group
            && let Some(alias) = self.virtual_group_aliases.get(group)
        {
            alias
        } else {
            self.display_label()
        }
    }

    /// Representative font FILE path. `pub(crate)` accessor for the settings font UI.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Base render/inline-tag label (file stem, no disambiguation). `pub(crate)` for the
    /// settings font UI's search predicate; never a display override.
    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    /// Real family/name read from the representative face. `pub(crate)` for the settings
    /// font UI (identity header + search).
    pub(crate) fn original_name(&self) -> &str {
        &self.original_name
    }

    /// PostScript name (`name` table id 6) of the representative face, read from the
    /// font file at load time.
    ///
    /// Returns an empty string when the file could not be parsed, when the declared name
    /// is not spec-valid (`fonts::is_valid_post_script_name` — such a name counts as
    /// absent everywhere), and for the synthetic bundled-stack entry (which stands for a
    /// chain of files, not one face). It is the structural source of the render IDENTITY
    /// (`render_identity_name`), never a display string.
    pub(crate) fn post_script_name(&self) -> &str {
        &self.post_script_name
    }

    /// UNSUFFIXED identity this entry claims: its representative face's PostScript
    /// name, or the family-or-label fallback when the file carried none.
    ///
    /// This is the name BEFORE any collision suffix, so it is what a second claimant of
    /// the same name would collide with and what `TabFontProvider` registers as the
    /// bare-name resolution alias. Meaningless for the synthetic bundled-stack entry
    /// (whose identity is reserved, not derived) — callers skip that entry.
    fn base_identity_name(&self) -> String {
        self.base_identity_str().to_string()
    }

    /// Borrowing form of [`Self::base_identity_name`] for the allocation-sensitive name
    /// matchers (`ui_helpers`), which evaluate it for every font on every lookup.
    fn base_identity_str(&self) -> &str {
        fonts::base_font_identity_str(self.post_script_name(), &self.original_name, &self.label)
    }

    /// Canonical render/inline-tag IDENTITY name — the value persisted in
    /// `render_data`/`TextRenderParams.font_name` and emitted in `<font=...>` tags.
    ///
    /// Returns the `identity_name` computed for the panel list by
    /// `fonts::assign_font_identity_names`: the representative face's PostScript name,
    /// suffixed with `%{16 hex of the content hash}` only when another file claims the
    /// same name with different bytes (or when the base identity cannot be a valid
    /// PostScript name; see `fonts::assign_font_identity_names`). It is NOT a display string — user-facing
    /// combos/lists use `display_label()`. `TabFontProvider` keys this identity as its
    /// primary lookup and keeps the family name / label / stem as READ-ONLY legacy
    /// aliases, so projects persisted before the identity became the PostScript name
    /// still resolve. Falls back to the per-entry base identity if the collision-aware
    /// pass was never run (a non-panel list).
    ///
    /// `pub(crate)` (via the `font_admin` re-export of this opaque type) so the settings
    /// font UI can key its own-typeface previews and its per-font window on the identity
    /// instead of on a file path.
    pub(crate) fn render_identity_name(&self) -> String {
        let identity = self.identity_name.trim();
        if identity.is_empty() {
            self.base_identity_name()
        } else {
            identity.to_string()
        }
    }

    /// Hash of the representative FILE's bytes, or `0` when it was not computed (see the
    /// `content_hash` field).
    ///
    /// `pub(crate)` (via the `font_admin` re-export) so every own-typeface PREVIEW site can
    /// put it into the egui family key: a registration hands egui a SNAPSHOT of the bytes,
    /// which egui never refreshes, so without this discriminant a replaced font file would
    /// keep being previewed from its old content. Never a resolution key and never
    /// persisted — the identity already carries the hash when (and only when) two files
    /// contest one PostScript name.
    pub(crate) fn content_hash(&self) -> u64 {
        self.content_hash
    }

    /// Face index of the representative face (0 for single-face files). `pub(crate)` for
    /// the settings font UI's own-typeface preview.
    pub(crate) fn representative_face_index(&self) -> usize {
        self.faces.first().map(|face| face.face_index).unwrap_or(0)
    }

    /// Representative face label, but only for MULTI-face files (`None` otherwise), for
    /// the settings font-properties identity header. `pub(crate)` accessor.
    pub(crate) fn representative_face_label(&self) -> Option<String> {
        if self.faces.len() > 1 {
            self.faces.first().map(|face| face.label.clone())
        } else {
            None
        }
    }
}

/// One selectable face of a font FILE (a `.ttc` collection has several).
#[derive(Clone)]
struct FontFaceEntry {
    /// DISPLAY label of the face in the face combo:
    /// `#{index} {family} | {style} | w{weight} | {post_script_name}`. Presentation
    /// only — every consumer that needs one of those parts reads it structurally
    /// (`post_script_name` below, `FontEntry::original_name`), never by splitting
    /// this string.
    label: String,
    /// Index of the face inside its file, as passed to `fontdb`/`swash`.
    face_index: usize,
    /// PostScript name (`name` table id 6) of THIS face as `fontdb` read it, VALIDATED
    /// against the spec (`fonts::validated_post_script_name`). Empty for the placeholder
    /// face of a file that could not be parsed — `fontdb` rejects a real face that
    /// carries no PostScript name — and for a face whose declared name the spec forbids,
    /// which is treated as no name at all so it can never become an identity. The face
    /// `label` above still shows the raw string, so a malformed name stays diagnosable.
    post_script_name: String,
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
    Interference,
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
            Self::Interference => t!("typing.effects.interference_title"),
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
    Interference(InterferenceEffectCard),
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
            Self::Blur(_) | Self::MotionBlur(_) | Self::Interference(_) => false,
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
            Self::Blur(_) | Self::MotionBlur(_) | Self::Interference(_) => false,
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

/// Shape of the structuring element the soft-glow effect dilates the source
/// contour with. Serialized as the JSON `shape` string (`square` / `round`)
/// consumed by the renderer's `soft_glow` effect.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GlowOutlineShape {
    Square,
    Round,
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

/// Rectangle the gradient ramp is stretched over. Serialized as the JSON `area_mode`
/// string (`full_image` / `affected_area`) consumed by the renderer's gradient effects.
///
/// `FullImage` is the legacy behavior and the default for cards restored from projects
/// saved before this parameter existed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum GradientAreaMode {
    /// Bounding box of everything non-transparent.
    FullImage,
    /// Bounding box of only the pixels the fill mode replaces.
    AffectedArea,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReflectAxis {
    X,
    Y,
}

/// The color-preset set a draw pass may offer, plus the verdict "a cell was
/// overwritten, persist it".
///
/// It exists because the presets have to reach ~15 color selectors nested deep
/// inside the panel drawing code, and every one of them can report that the user
/// confirmed a preset edit. Threading a bare `Option<&mut ColorPresets>` down would
/// force each of those call sites to ALSO propagate a second return value; here the
/// verdict is collected in one place ([`ColorPresetsBinding::draw_selector`]), so no
/// call site can forget it, and the owner reads it once when the pass is over.
///
/// `None` is not a degraded mode: it selects the stock egui color button, which is
/// what a caller without an open title (the settings-side effect-defaults editor)
/// must show — there is no title to store presets in.
#[derive(Debug)]
struct ColorPresetsBinding<'a> {
    /// The set to offer, or `None` for the stock egui color button.
    presets: Option<&'a mut ColorPresets>,
    /// Whether any selector of this pass overwrote a preset cell.
    changed: bool,
}

impl<'a> ColorPresetsBinding<'a> {
    /// Binds `presets` to one draw pass.
    fn new(presets: Option<&'a mut ColorPresets>) -> Self {
        Self {
            presets,
            changed: false,
        }
    }

    /// A pass that offers no presets, for a caller with no open title.
    fn none() -> Self {
        Self::new(None)
    }

    /// Draws one color selector against this binding and records whether it
    /// overwrote a preset cell.
    ///
    /// Returns whether the COLOR changed, which is the only part of the selector's
    /// response the panel has ever used; the preset verdict is accumulated here
    /// instead of being returned, so it cannot be dropped on the way up.
    fn draw_selector(
        &mut self,
        ui: &mut egui::Ui,
        selector: &mut ViewportColorSelector,
        color: &mut Color32,
    ) -> bool {
        let response = selector.draw_with_presets(ui, color, self.presets.as_deref_mut());
        self.changed |= response.presets_changed;
        response.changed
    }

    /// Whether a preset cell was overwritten during this pass.
    ///
    /// `#[must_use]`: this verdict is the ONLY record that the user's edit still has
    /// to be persisted — the binding is dropped at the end of the pass, so a caller
    /// that reads it and ignores the answer silently loses the change.
    #[must_use]
    fn presets_changed(&self) -> bool {
        self.changed
    }
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

    /// Draws the labelled color row. `presets` decides whether the swatch opens the
    /// preset picker or the stock egui palette; returns whether the COLOR changed.
    fn draw(
        &mut self,
        ui: &mut egui::Ui,
        label: &str,
        presets: &mut ColorPresetsBinding<'_>,
    ) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label(label);
            changed |= presets.draw_selector(ui, &mut self.picker, &mut self.value);
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

/// Sub-type selector of the interference effect card; serialized as the
/// JSON `kind` string (`white_noise`/`digital`/`rgb_split`/`scanlines`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum InterferenceKind {
    WhiteNoise,
    Digital,
    RgbSplit,
    Scanlines,
}

/// UI model of the "interference" (glitch/noise) effect. Holds the parameters
/// of ALL kinds simultaneously so switching `kind` never loses values; every
/// field is always serialized (see `effect_card_to_value`). Contract twin of
/// `InterferenceEffectParams` in ms-text-render `effects/parse.rs`.
struct InterferenceEffectCard {
    kind: InterferenceKind,
    seed: u64,
    amount: f32,
    scale_px: f32,
    density: f32,
    monochrome: bool,
    alpha_noise: f32,
    slice_height_px: i32,
    height_jitter: f32,
    max_shift_px: f32,
    probability: f32,
    rgb_split_px: f32,
    autogrow: bool,
    offset_px: f32,
    angle_deg: f32,
    per_row_jitter: f32,
    line_height_px: i32,
    gap_px: i32,
    darken: f32,
    jitter_px: f32,
}

/// UI model shared by the three glow effect cards. `version` selects both the
/// serialized JSON kind (`glow_v1` / `glow_v2` / `soft_glow`) and the controls
/// drawn by `draw_effect_card_controls`; the fields that do not belong to the
/// active version hold inert placeholders and are never serialized.
///
/// The soft-glow expansion fields are ADDITIONAL dilation in whole pixels applied
/// on one side each, on top of `radius_px`: `x_plus` = right, `x_minus` = left,
/// `y_plus` = down, `y_minus` = up.
struct GlowEffectCard {
    version: GlowEffectVersion,
    radius_px: f32,
    /// Soft glow only: SIGMA of the gaussian blur applied to the dilated outline, in px
    /// (the renderer passes this value straight in as the blur sigma, so the visible
    /// reach is roughly 3x it). The user-visible label calls it a radius on purpose.
    blur_radius_px: f32,
    /// Soft glow only: blur response bias, in percent (-100..=100).
    blur_bias: f32,
    /// Soft glow only: blur response knee, in percent (0..=100); 100 = no knee.
    blur_knee: f32,
    /// Soft glow only: extra dilation on the right side, in px (-512..=512).
    expand_x_plus_px: i32,
    /// Soft glow only: extra dilation on the left side, in px (-512..=512).
    expand_x_minus_px: i32,
    /// Soft glow only: extra dilation on the bottom side, in px (-512..=512).
    expand_y_plus_px: i32,
    /// Soft glow only: extra dilation on the top side, in px (-512..=512).
    expand_y_minus_px: i32,
    /// Soft glow only: shape of the dilation structuring element.
    outline_shape: GlowOutlineShape,
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
    /// `SpecificColor` only: allowed deviation from `target_color`, in percent of the
    /// RGB cube diagonal (0 = byte-exact match).
    color_tolerance_percent: f32,
    area_mode: GradientAreaMode,
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
    /// `SpecificColor` only: allowed deviation from `target_color`, in percent of the
    /// RGB cube diagonal (0 = byte-exact match).
    color_tolerance_percent: f32,
    area_mode: GradientAreaMode,
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

/// Per-font parameter memory of one create/edit panel, keyed by the font IDENTITY.
///
/// TWO LAYERS, deliberately. The in-RAM map is the SESSION memory and, unchanged from
/// before, the payload a saved create preset carries (`TypingCreatePreset.font_profiles`).
/// Behind it sits the font's PERSISTENT DEFAULT profile in `fonts/fonts_data.json`
/// (`fonts_data.fonts.<identity>.profile`, the "variant A" split of
/// `dev-docs/font_identity_postscript_plan.md`): a lookup that misses in RAM falls back to
/// the stored default and caches it, and a store writes BOTH. That is what makes the
/// parameters a user set for a font come back in the next session instead of dying with
/// the panel, while presets keep their own independent per-font overrides.
///
/// WHICH LAYER A STORE WRITES IS DECIDED BY THE CALLER, and the rule is the ownership split
/// of "variant A": a panel that is currently showing a PRESET updates that preset's working
/// set only ([`DefaultProfileWrite::PresetOnly`]), never the font's persisted default.
/// Otherwise preset A's parameters silently became the font's default and every fresh,
/// preset-less panel opened with them.
///
/// The persisted write goes through `font_settings_store::set_font_profile`, which is
/// DEBOUNCED: a profile is rewritten on every parameter edit, so an immediate atomic save
/// per edit would be pure write amplification.
#[derive(Debug, Default, Clone)]
pub(super) struct FontProfileMemory {
    /// Session memory: identity -> profile JSON. Also the preset payload.
    ram: HashMap<String, Value>,
}

/// Whether a [`FontProfileMemory`] store also updates the font's PERSISTED DEFAULT profile.
///
/// The two layers have different owners ("variant A" of
/// `dev-docs/font_identity_postscript_plan.md`): `fonts_data.fonts.<identity>.profile` is
/// what a font remembers by itself, `presets.<name>.profiles` is what ONE preset remembers
/// for it. An edit made while a preset is applied belongs to the preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DefaultProfileWrite {
    /// No preset is applied: the parameters on screen are what this font should remember,
    /// so both layers are written.
    UpdateFontDefault,
    /// A preset is applied: only the session map (that preset's working set) is written.
    /// The preset itself reaches disk when the user saves it.
    PresetOnly,
}

impl FontProfileMemory {
    /// Wraps an existing identity-keyed profile map (a preset being applied), replacing the
    /// session memory. Persisted defaults are untouched: applying a preset must not rewrite
    /// what every font remembers on disk.
    #[must_use]
    pub(super) fn from_map(ram: HashMap<String, Value>) -> Self {
        Self { ram }
    }

    /// Snapshot of the SESSION memory only, for storing into a preset. The persisted
    /// defaults of fonts this session never touched deliberately stay out of it.
    #[must_use]
    pub(super) fn to_map(&self) -> HashMap<String, Value> {
        self.ram.clone()
    }

    /// The profile remembered for `identity`: the session value, else the font's persisted
    /// default (which is then cached in the session map, exactly as if the user had just
    /// selected that font). `None` when the font has no remembered parameters at all.
    pub(super) fn get(&mut self, identity: &str) -> Option<&Value> {
        self.get_with(identity, Self::persisted_profile)
    }

    /// Remembers `profile` for `identity` in this session and — only with
    /// [`DefaultProfileWrite::UpdateFontDefault`] — as the font's persisted default.
    /// Returns the previous session value, mirroring `HashMap::insert`.
    pub(super) fn insert(
        &mut self,
        identity: String,
        profile: Value,
        write: DefaultProfileWrite,
    ) -> Option<Value> {
        match write {
            DefaultProfileWrite::UpdateFontDefault => {
                self.insert_with(identity, profile, Self::persist_profile)
            }
            // The session map IS the applied preset's working set; the font's own default
            // must not learn a preset's parameters.
            DefaultProfileWrite::PresetOnly => self.insert_with(identity, profile, |_, _| {}),
        }
    }

    /// [`Self::get`] with an explicit persisted-profile source, so the two-layer rule
    /// (session hit → session; session miss → load, CACHE, return) is unit-testable without
    /// the process-global store.
    fn get_with(
        &mut self,
        identity: &str,
        load: impl FnOnce(&str) -> Option<Value>,
    ) -> Option<&Value> {
        if !self.ram.contains_key(identity)
            && let Some(stored) = load(identity)
        {
            self.ram.insert(identity.to_string(), stored);
        }
        self.ram.get(identity)
    }

    /// [`Self::insert`] with an explicit persisted-profile sink; see [`Self::get_with`].
    fn insert_with(
        &mut self,
        identity: String,
        profile: Value,
        save: impl FnOnce(&str, &Value),
    ) -> Option<Value> {
        save(&identity, &profile);
        self.ram.insert(identity, profile)
    }

    /// Reads the persisted default profile of `identity`.
    ///
    /// Disabled under `#[cfg(test)]`: the font-settings store is PROCESS-GLOBAL, so a panel
    /// unit test reading it would see profiles written by any other test in the binary
    /// (and vice versa). The persistence itself is covered by `font_settings_store`'s own
    /// serialized tests. Same precedent as `font_settings_store::persist_off_thread`.
    fn persisted_profile(identity: &str) -> Option<Value> {
        if cfg!(test) {
            return None;
        }
        font_settings_store::font_profile(identity)
    }

    /// Writes the persisted default profile of `identity`.
    ///
    /// Under `#[cfg(test)]` the process-global store is not touched (see
    /// [`Self::persisted_profile`]); the write is RECORDED in a per-thread journal instead,
    /// because "which layer did this edit reach" is precisely the contract of
    /// [`DefaultProfileWrite`] and cannot be observed any other way.
    #[cfg(test)]
    fn persist_profile(identity: &str, _profile: &Value) {
        PERSISTED_DEFAULT_WRITES.with(|writes| writes.borrow_mut().push(identity.to_string()));
    }

    /// Production build: the write reaches the process-global store.
    #[cfg(not(test))]
    fn persist_profile(identity: &str, profile: &Value) {
        font_settings_store::set_font_profile(identity, Some(profile.clone()));
    }

    /// Whether the SESSION memory holds a profile for `identity` (ignoring the persisted
    /// default). Test-only: production code always wants the two-layer `get`.
    #[cfg(test)]
    #[must_use]
    pub(super) fn contains_key(&self, identity: &str) -> bool {
        self.ram.contains_key(identity)
    }

    /// Number of profiles in the SESSION memory. Test-only.
    #[cfg(test)]
    #[must_use]
    pub(super) fn stored_count(&self) -> usize {
        self.ram.len()
    }
}

#[cfg(test)]
thread_local! {
    /// Identities whose PERSISTED DEFAULT profile was written on this thread, in order.
    /// Per-thread so parallel tests cannot see each other's writes.
    static PERSISTED_DEFAULT_WRITES: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Takes (and clears) the identities whose persisted default profile was written on this
/// thread since the last call. Test-only; see [`FontProfileMemory::persist_profile`].
#[cfg(test)]
#[must_use]
pub(super) fn take_persisted_default_writes() -> Vec<String> {
    PERSISTED_DEFAULT_WRITES.with(|writes| std::mem::take(&mut *writes.borrow_mut()))
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
    /// Pending in-app deep-link request raised by the font-group "?" help icon. `Some`
    /// while a click awaits draining by the facade layer, which forwards it to the app
    /// so it can switch to the settings tab and reveal the target block. Reset on take.
    pending_settings_link_request: Option<crate::settings_shared::SettingsDeepLink>,
    font_reload_rx: Option<Receiver<FontReloadResult>>,
    latest_font_reload_token: u64,
    fonts_reload_in_flight: bool,
    /// `true` once this panel has installed a list built by the COMBINED loader
    /// (`fonts::load_fonts`), i.e. folder fonts AND imported system fonts together.
    ///
    /// `false` from construction until the first reload result lands. It gates the one-shot
    /// legacy-preset migration: run against a list that cannot see the imported system
    /// fonts, the migration resolves none of their references, keeps them verbatim, deletes
    /// the legacy `user_config` key and never retries — leaving `presets.json` and
    /// `fonts_data.json` permanently disagreeing about the same font.
    font_list_is_authoritative: bool,
    /// Legacy `user_config.TextTab.create_presets` payload that arrived (from the
    /// off-thread seed) BEFORE the authoritative font list, parked until it lands.
    ///
    /// The read of `presets.json` and the font load run concurrently, so their finish order
    /// is not something the panel may rely on; this is what makes the ordering a guarantee
    /// instead of a race. Drained by `poll_font_reload_results`.
    pending_legacy_presets_migration: Option<Vec<presets_store::LegacyPresetEntry>>,
    /// Per-font parameter memory, keyed by the font IDENTITY: this session's map plus each
    /// font's PERSISTED default profile behind it (see [`FontProfileMemory`]).
    ///
    /// Its session half is also the payload of a saved create preset
    /// (`TypingCreatePreset.font_profiles`). A preset written by an older build carries PATH
    /// keys; `apply_preset_by_name` converts them to identities on load (an unresolvable key
    /// is kept verbatim rather than dropped), so nothing downstream has to know about the
    /// legacy form.
    font_profiles_by_identity: FontProfileMemory,
    /// IDENTITY of the font whose profile is currently loaded into the panel fields —
    /// the anchor `store_current_font_profile_by_idx` writes back to and the key the
    /// selection is restored by after a background font reload.
    active_font_identity: Option<String>,
    /// Имя шрифта выбранного для редактирования оверлея, если этот шрифт не найден
    /// среди доступных. Пока поле `Some`, рендер оверлея заблокирован, а все
    /// параметры (кроме выбора шрифта) на панели редактирования недоступны.
    missing_font: Option<String>,
    presets_by_name: HashMap<String, TypingCreatePreset>,
    /// Sending end of the preset-store event channel, cloned into every background preset
    /// worker (the off-GUI-thread seed read, the legacy-payload read, and each save).
    preset_store_tx: Sender<PresetStoreEvent>,
    /// Receiving end, drained once per frame by `create_presets::poll_preset_store_events`:
    /// it installs the seeded document, finishes the one-shot `user_config` migration
    /// (which needs THIS panel's font list, hence the GUI-thread half), adopts presets
    /// another app instance wrote, and surfaces a save failure in the status line.
    preset_store_rx: Receiver<PresetStoreEvent>,
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
    /// Reference band `line_placement_percent` snaps to on `CustomVectorLines`:
    /// `LineBox` = shared font line (all glyphs on one baseline, a clean curved
    /// string); `GlyphHeight` = each glyph's own bitmap height (legacy). New text
    /// defaults to `LineBox`; projects saved before the option load as `GlyphHeight`.
    line_placement_reference: LinePlacementReference,
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
    faux_bold: bool,
    faux_bold_thicken_percent: f32,
    faux_bold_expand_percent: f32,
    faux_bold_sharp_corners: bool,
    faux_bold_outward_only: bool,
    faux_italic: bool,
    faux_italic_slant_deg: f32,
    uppercase_text: bool,
    trim_extra_spaces: bool,
    replace_ellipsis_with_dots: bool,
    /// Sub-parameter of `replace_ellipsis_with_dots`: after the substitution, also strip
    /// the font's `. . . -> …` GSUB ligature so the three dots cannot be shaped back into
    /// a single ellipsis glyph. Meaningful (and shown in the panel) ONLY while the parent
    /// flag is on; the renderer applies the effect only when BOTH are set.
    force_remove_ellipsis_glyph: bool,
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
    /// Font diagnostic of the LAST COMPLETED preview render: which characters of
    /// the current text the renderer's fallback chain drew instead of the selected
    /// font, and which nothing could draw. Its lifetime mirrors `preview_texture` —
    /// both describe the same finished render — so it is replaced on a successful
    /// render and cleared when a render fails. Empty means "the selected font
    /// served the whole text", which is also the state while no render has
    /// completed yet. Answers a different question from `FontEntry.coverage`
    /// (static per-font language support); see `panel/font_coverage.rs`.
    preview_font_fallbacks: FontFallbackReport,
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
    /// Поиск форм, выполняющийся ПРЯМО СЕЙЧАС в фоновом потоке; не более одного.
    /// Замена значения отменяет прежнюю задачу (`Drop` взводит её флаг отмены).
    advanced_form_search: Option<AdvancedFormSearchJob>,
    /// Debounce запуска поиска: ключ, ради которого «тикает» таймер, и момент
    /// последней смены этого ключа. Пока пользователь печатает, ключ меняется
    /// каждый кадр и таймер взводится заново, поэтому серия нажатий запускает
    /// ОДИН перебор, а не по одному на кадр.
    advanced_form_search_debounce: Option<(AdvancedFormSearchKey, Instant)>,
    /// Ручки поиска правились и ещё не записаны в `user_config.json`; значение —
    /// момент последней правки. Значение УЖЕ применено к процесс-глобальному
    /// состоянию, ждёт только диск: слайдер отдаёт новое число на каждом кадре
    /// перетаскивания, и запись на кадр была бы чистой амплификацией.
    advanced_form_params_save_pending: Option<Instant>,
    /// Font bytes for the advanced-form width metric, resolved OFF the GUI thread; see
    /// [`AdvancedFormFont`]. `None` until the window has asked for a font at all.
    advanced_form_font: Option<AdvancedFormFont>,
    /// In-flight resolve feeding `advanced_form_font`; at most one at a time (the newest
    /// selection wins).
    advanced_form_font_request: Option<AdvancedFormFontRequest>,
    /// Сформированный (разбитый на строки) текст. Если не пуст — в рендер идёт
    /// именно он, а `text` остаётся исходным. Пуст — рендерится `text`.
    formed_text: String,
    /// The form in `formed_text` could not carry the inline tags of its source back and
    /// was applied WITHOUT them (`create_advanced::apply_advanced_form`).
    ///
    /// A sticky field of its own and NOT `status_line`, because the create panel — the one
    /// that hosts the form window — renders a preview, and the render owns that line:
    /// `queue_preview_render` overwrites it SYNCHRONOUSLY inside the very call that
    /// applies the form, and `poll_preview_render_results` overwrites it again when the
    /// render lands. A warning about silently dropped styling has to outlive a render
    /// cycle, so it is drawn as its own row next to the per-render font diagnostic
    /// (`create_sections::draw_preview_section`).
    ///
    /// Describes the formed text currently in effect: every form application sets or
    /// clears it, and it is cleared wherever `formed_text` is replaced from another
    /// source (overlay switch, document load, «Вернуть исходный»).
    advanced_form_tags_lost: bool,
    /// Какой из двух текстов развёрнут в панели (конкурирующий аккордеон):
    /// `true` — сформированный, `false` — исходный.
    advanced_text_show_formed: bool,
    /// Фильтр по числу строк `(min, max)`. `None` — «весь диапазон»: фильтр не
    /// сужен пользователем, поэтому в поиск он не передаётся и его границы
    /// берутся из полученного набора форм. При включённом
    /// [`AdvancedFormParams::filters_prune`] это ВХОД перебора, а не фильтр показа.
    advanced_form_line_range: Option<(usize, usize)>,
    /// Фильтр по ширине самой длинной строки `(min, max)` в единицах метрики;
    /// `None` — «весь диапазон» (см. [`Self::advanced_form_line_range`]).
    advanced_form_width_range: Option<(u32, u32)>,
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
    /// Состояние окна «Таблица символов» (вкладки, размер ячейки, избранное,
    /// фоновая карта покрытия глифов). Чистые данные: окно рисуется в
    /// `char_table::draw_char_table_window`, которое вызывается раз за кадр из
    /// `create_edit::draw_edit_params_section`.
    char_table: char_table::CharTableState,
}

/// Сколько карточек форм максимум отрисовываем в окне за раз. Это предел
/// ОТРИСОВКИ, а не данных: кэш хранит все удачные формы и фильтрует их целиком,
/// а в список попадают первые `ADVANCED_FORM_DISPLAY_LIMIT` (лучшие по сортировке)
/// из прошедших фильтр.
const ADVANCED_FORM_DISPLAY_LIMIT: usize = 600;

/// The search text and the scope the engine will strip it with, compared as ONE input.
///
/// The forms depend on the pair ONLY through `forms::strip_inline_tags(raw, scope)` — the
/// break graph, the width alphabet and the markup put back on the applied form all start
/// there — so two pairs that strip alike ARE the same input and must not read as a base
/// change. A base change restarts the search AND wipes the window's display filters
/// (`reset_display_filters`), which is exactly what must not happen for a difference the
/// engine cannot see.
///
/// The difference that would otherwise be reported is the `base_font_size_px` inside
/// `InlineTagScope::All`. It belongs to the input (it decides whether `<offset=…>` /
/// `<stretching=…>` are tags at all), but for a text whose tag bodies do not depend on it
/// — any text without such a tag — every font size gives the same forms, and before the
/// scope entered the base a size change did not restart the search at all: the metric is
/// measured in 1/1000 em and `advanced_form_line_height_em` is size-invariant while line
/// spacing and glyph height/width are in PERCENT (their defaults).
///
/// `PartialEq` is manual for that reason and stays reflexive even if the size were `NaN`:
/// a scope always strips its own text exactly like itself.
#[derive(Debug, Clone)]
struct AdvancedFormSearchText {
    /// СЫРОЙ исходный текст с инлайновыми тегами (`advanced_form_source_text`):
    /// теги снимает сам движок форм, панель их не трогает.
    raw: String,
    /// Какие инлайновые теги движок снимает — производное от «Инлайновых тегов»
    /// (`advanced_form_inline_tag_scope`).
    scope: InlineTagScope,
}

impl PartialEq for AdvancedFormSearchText {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
            && (self.scope == other.scope
                || forms::scopes_strip_alike(self.raw.as_str(), self.scope, other.scope))
    }
}

/// Всё, от чего зависит НАБОР найденных форм, кроме диапазонов фильтров окна.
///
/// Смена базы означает другой текст/шрифт/пресет/ручку перебора, то есть другой
/// набор форм: сужённые под прошлый набор диапазоны фильтров и пороги показа
/// теряют смысл и сбрасываются, чего смена одних лишь диапазонов не делает.
///
/// [`AdvancedFormParams::filters_prune`] сюда НЕ входит намеренно: он не меняет
/// набор сам по себе, а лишь решает, попадают ли диапазоны окна в
/// [`AdvancedFormSearchKey`]. Будь он полем базы, переключение галочки считалось
/// бы сменой базы и стирало бы оба диапазона — то есть ни превратить фильтр
/// показа в ограничение перебора, ни вернуть его обратно было бы нельзя.
///
/// Все вещественные поля КОНЕЧНЫ по построению
/// (`AdvancedFormParams::clamp_to_supported_range` для ручек и
/// `advanced_form_line_height_em` для высоты строки), поэтому производное
/// `PartialEq` рефлексивно и сравнение ключа не может зациклить перезапуск.
/// The one real that is NOT a field of this struct — the `base_font_size_px` inside
/// `text.scope` — is covered twice over: `advanced_form_inline_tag_scope` builds it as
/// `font_size_px.max(1.0)` and `f32::max` returns the operand that is not `NaN`, and
/// [`AdvancedFormSearchText`]'s own `PartialEq` is reflexive even if it ever were `NaN`.
#[derive(Debug, Clone, PartialEq)]
struct AdvancedFormSearchBase {
    /// Текст поиска и область снятия тегов — ОДИН вход, см. [`AdvancedFormSearchText`].
    text: AdvancedFormSearchText,
    preset: TextFormPreset,
    /// Шрифт/начертание/висячая пунктуация — от них зависят ширины строк.
    metric: AdvancedFormMetricSignature,
    /// Ручки, влияющие на ПЕРЕБОР. Порог качества и приоритет узких форм сюда не
    /// входят: они меняют только порядок показа ([`AdvancedFormOrderKey`]).
    evenness: f32,
    aspect_max: f32,
    hyphen_ratio: f32,
    hyphen_relax_slack: f32,
    per_bucket: usize,
    /// Высота строки в долях em — вторая половина потолка пропорции формы
    /// (первая — ширины метрики). Строго положительна и конечна.
    line_height_em: f32,
}

/// Полный вход поиска форм: база плюс диапазоны фильтров окна, которые при
/// включённом `filters_prune` СОКРАЩАЮТ перебор (и потому попадают в ключ), а
/// иначе передаются как `None` и остаются фильтром показа.
#[derive(Debug, Clone, PartialEq)]
struct AdvancedFormSearchKey {
    base: AdvancedFormSearchBase,
    line_range: Option<(usize, usize)>,
    width_range: Option<(u32, u32)>,
}

/// Ручки, влияющие ТОЛЬКО на порядок показа карточек
/// (`text_forms::order_advanced_forms`). Их смена пересортировывает уже
/// найденный набор и НИКОГДА не перезапускает перебор: пересортировка дешёвая,
/// перебор — нет.
#[derive(Debug, Clone, Copy, PartialEq)]
struct AdvancedFormOrderKey {
    /// Порог качества в единицах `TextForm::quality_milli`
    /// (`AdvancedFormParams::quality_floor_milli`).
    quality_floor_milli: u32,
    narrow_slots: usize,
}

/// Результат фонового поиска форм — ровно то, что вернул `forms::search_forms`.
struct AdvancedFormSearchResult {
    forms: Vec<TextForm>,
    truncated: bool,
}

/// Выполняющийся в фоне поиск форм для окна «Продвинутая форма текста».
///
/// Отмена устроена как у `TypingShapeVariantPreviewState`: воркер держит копию
/// `cancel`, а `Drop` этой структуры взводит флаг — поэтому ПРИСВОЕНИЕ нового
/// значения полю панели отменяет предыдущую задачу, и ни одна площадка мутации
/// не обязана помнить про явный вызов отмены.
struct AdvancedFormSearchJob {
    /// Вход, ради которого задача запущена; он же становится ключом кэша.
    key: AdvancedFormSearchKey,
    /// Взводится `Drop`'ом; воркер проверяет его до и после перебора.
    cancel: Arc<AtomicBool>,
    /// Отдаёт результат ровно один раз.
    rx: Receiver<AdvancedFormSearchResult>,
    /// Сбросить пороги показа (пиковость, неравномерность, консервативность,
    /// группа переносов) при приёме результата: взводится, только когда сменилась
    /// БАЗА ключа, иначе правка одного диапазона обнуляла бы чужие фильтры.
    reset_display_filters: bool,
}

impl Drop for AdvancedFormSearchJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Кэш найденных форм для окна «Продвинутая форма текста».
struct AdvancedFormCache {
    /// Вход, которым получен этот результат. Несовпадение с текущим входом — и
    /// есть повод запустить новый поиск.
    key: AdvancedFormSearchKey,
    /// Формы КАК ИХ ВЕРНУЛ `forms::search_forms` (по корзинам высот, внутри
    /// корзины — по возрастанию `quality_milli`). Хранятся отдельно от показа,
    /// потому что порог качества ВЫБРАСЫВАЕТ формы, а его ослабление обязано
    /// вернуть их без нового перебора.
    searched_forms: Vec<TextForm>,
    /// Порядок показа (`order_advanced_forms`) при [`Self::order_key`].
    forms: Vec<TextForm>,
    /// Ручки показа, при которых собран [`Self::forms`].
    order_key: AdvancedFormOrderKey,
    /// Встретившиеся значения числа переносов слов (для динамических кнопок).
    group_counts: Vec<usize>,
    /// Границы диапазонных фильтров. Обновляются по фактическим данным только
    /// после НЕограниченного прогона; после прогона, СУЖЕННОГО этими же
    /// фильтрами, переносятся с прошлого кэша (объединённые с наблюдёнными),
    /// иначе сузивший себя фильтр было бы уже не расширить.
    line_bounds: (usize, usize),
    width_bounds: (u32, u32),
    /// Максимальная пиковость в % для каждой базы (минимум/медиана).
    peak_max_bound_min: u32,
    peak_max_bound_median: u32,
    /// Максимальная неравномерность в % среди форм (верхняя граница фильтра).
    uneven_max_bound: u32,
    /// Самая вольная консервативность среди форм (верхняя граница фильтра). Если
    /// `Safe` — отрывов служебных слов нет, селектор консервативности не нужен.
    conservatism_bound: Conservatism,
    /// Перебор форм оказался неполным: выбит бюджет узлов (не лимит отрисовки).
    /// Означает, что в кэше лежат не все возможные формы.
    truncated: bool,
}

/// The font bytes the advanced-form width metric measures with, as the panel's own
/// `FontProvider` resolved them — i.e. exactly the bytes the renderer draws with.
///
/// Resolving means a possible `fs::read`, so it never happens on the GUI thread
/// (`CLAUDE.md` §5); the window shows the coarse per-character metric until this arrives.
struct AdvancedFormFont {
    /// Font identity this was resolved FOR. A different selection invalidates it.
    identity: String,
    /// `None` when the identity resolved to nothing (unknown name, unreadable file).
    /// Remembered rather than retried, so a missing font does not spawn a resolver per
    /// frame for as long as the window stays open.
    content: Option<FontContent>,
}

/// Снимок всего, что нужно для ПОСТРОЕНИЯ метрики ширины окна форм, — чтобы её
/// строил фоновый воркер, а не GUI-поток.
///
/// Байты уже разрешены (`AdvancedFormFont`), поэтому чтения файла здесь нет; но
/// разбор фейса, регистрация в `fontdb` и шейпинг алфавита — работа, которой на
/// GUI-потоке делать нечего (`CLAUDE.md` §5). Путь и подпись шрифта нужны только
/// для диагностики и для пропуска уже зарегистрированного файла в бандловой
/// цепочке; ключом ничто из них не является.
#[derive(Clone)]
struct AdvancedFormMetricSpec {
    /// Разрешённые байты выбранного шрифта; `None` — метрика будет посимвольной.
    content: Option<FontContent>,
    /// Индекс фейса ВЫБРАННОГО пользователем начертания внутри файла.
    face_index: usize,
    /// Выбран синтетический «Встроенный шрифт интерфейса»: в базу метрики
    /// доливается остаток бандловой цепочки `core`.
    bundled_stack: bool,
    /// Файл выбранного фейса — источник байт и то, что пропускается при доливке
    /// цепочки, чтобы фейс не задвоился.
    path: PathBuf,
    /// Подпись шрифта для сообщений в лог.
    display_label: String,
    force_bold: bool,
    faux_bold: bool,
    force_italic: bool,
    faux_italic: bool,
    hanging_punctuation: bool,
}

/// An in-flight background resolve of [`AdvancedFormFont`].
struct AdvancedFormFontRequest {
    /// Identity being resolved; also what tells a stale request from the current one.
    identity: String,
    /// Delivers the worker's result exactly once.
    rx: Receiver<Option<FontContent>>,
}

/// От чего зависят пиксельные ширины глифов в окне форм. При смене любого поля
/// метрику (и кэш форм) надо пересобрать.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct AdvancedFormMetricSignature {
    /// IDENTITY of the selected font (`FontEntry::render_identity_name`), `None` when no
    /// font is selected.
    ///
    /// The identity — not the file path — is what distinguishes two measurements: the
    /// BUILT-IN interface entry points at the real file `core[0]` and is measured with
    /// the whole bundled `core` chain in its database
    /// (`create_advanced::register_bundled_core_fallback`), while a user who imported
    /// that very file is measured with that file alone. Keying on the path made those two
    /// share a signature, which is why a separate `bundled_ui_stack` flag had to exist;
    /// their identities differ (`ManhwaStudio-UI` vs the file's PostScript name), so the
    /// flag is gone.
    font_identity: Option<String>,
    /// Content id of the font bytes the metric was built from, `None` while they are
    /// still being resolved off the GUI thread (the cache then holds CHARACTER-width
    /// forms) or when the identity resolves to nothing.
    ///
    /// It is what turns the arrival of the bytes into a cache rebuild: the identity has
    /// not changed at that moment, so without this field the window would keep showing
    /// the coarse fallback metric until the user touched something else. It doubles as
    /// the "the file behind this identity was replaced" discriminant, for the same
    /// reason `widgets::font_preview` keys its registrations by content.
    font_content_id: Option<u64>,
    face_index: usize,
    force_bold: bool,
    force_italic: bool,
    faux_bold: bool,
    faux_bold_thicken_percent: u32,
    faux_bold_expand_percent: u32,
    faux_bold_sharp_corners: bool,
    faux_bold_outward_only: bool,
    /// Faux italic toggles the synthesized-slant path, which keeps the Regular
    /// (upright) face instead of switching to the family's real Italic face.
    /// That face switch changes per-glyph advances for families that ship a real
    /// Italic, so the width metric must be rebuilt when it flips. The signed
    /// slant magnitude itself is a pure shear and leaves advances unchanged, so
    /// it stays out of this signature.
    faux_italic: bool,
    hanging_punctuation: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct TypingInlineTagStyle {
    bold: bool,
    italic: bool,
    /// `Some` = faux (synthesized) bold on the Regular face with these params;
    /// `None` while `bold == true` = the family's real Bold face. Mirrors the
    /// renderer's per-span resolution (see `pipeline.rs::faux_bold_params_at_offset`).
    faux_bold: Option<FauxBoldParams>,
    faux_italic_slant: Option<f32>,
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
    FauxBold(FauxBoldParams),
    FauxItalic(f32),
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
