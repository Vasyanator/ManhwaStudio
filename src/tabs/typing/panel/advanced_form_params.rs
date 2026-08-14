/*
File: panel/advanced_form_params.rs

Purpose:
The eight user-tunable knobs of the advanced text-form search («Продвинутая форма
текста», `dev-docs/text_forms_ranking_plan.md` §3b/§3c): their supported ranges and
defaults, the process-wide runtime value, the persisted JSON shape, and the mapping
onto the engine's `FormSearchParams`.

Main responsibilities:
- own the supported range and the default of every knob — the window's parameter
  section binds these constants, so a control can never offer a value the search
  refuses;
- hold the process-wide runtime value, seeded at startup from
  `TextTab.advanced_form_search` (`main.rs::seed_advanced_form_search_from_config`)
  and persisted by `tabs::settings::save_advanced_form_search_params`;
- translate the knobs into `ms_text_render::wrap::forms::FormSearchParams` (layers A/B)
  and into the layer-C ordering constants used by `text_forms::order_advanced_forms`.

Key structures:
- `AdvancedFormParams`

Key functions:
- `advanced_form_params()` / `set_advanced_form_params()`
- `AdvancedFormParams::to_search_params()`
- `AdvancedFormParams::to_config_value()` / `AdvancedFormParams::from_config_value()`
- `AdvancedFormParams::clamp_to_supported_range()`

Notes:
This module is the SINGLE owner of the persisted field names: the writer (settings)
and the reader (startup seed) both go through `to_config_value` / `from_config_value`,
so the two can never drift. It is GUI-free — the controls live in `create_advanced.rs`.
*/

use crate::runtime_log;
use crate::tabs::typing::render_next::forms::{
    self, CorridorLevel, FormSearchParams, HyphenBudget,
};
use serde_json::{Map, Value};
use std::sync::{Once, OnceLock, RwLock};

/// Наименьшая «ровность строк»: коридор ширин вдвое уже штатного. Ниже панель
/// голодает (замер плана §3b).
pub const EVENNESS_MIN: f32 = 0.5;
/// Наибольшая «ровность строк»: коридор вдвое шире штатного. Замер по корпусу
/// (2089 реплик): медиана 12 карточек против 6 на единице, худшая реплика 252 мс
/// против 51 — предел выбран по этой цене, а не по насыщению отдачи.
pub const EVENNESS_MAX: f32 = 2.0;
/// Штатная ровность: коридоры `forms::default_corridor_ladder()` как есть.
pub const EVENNESS_DEFAULT: f32 = 1.0;

/// Наименьший допустимый потолок пропорции формы `ширина : высота`.
pub const ASPECT_MAX_MIN: f32 = 1.2;
/// Наибольший допустимый потолок пропорции формы. Выше 4.0 обратно допускаются
/// «ленточные» формы, ради отсечения которых потолок и вводился.
pub const ASPECT_MAX_MAX: f32 = 4.0;
/// Штатный потолок пропорции — 21:9.
pub const ASPECT_MAX_DEFAULT: f32 = 21.0 / 9.0;

/// Наименьшая допустимая доля строк с переносом при комфортной ширине.
pub const HYPHEN_RATIO_MIN: f32 = 0.2;
/// Наибольшая допустимая доля строк с переносом при комфортной ширине
/// (`1.0` — бюджет переносов фактически выключен).
pub const HYPHEN_RATIO_MAX: f32 = 1.0;
/// Штатная доля строк с переносом: половина строк формы.
pub const HYPHEN_RATIO_DEFAULT: f32 = 0.5;

/// Наименьший люфт, начиная с которого действует строгая доля переносов.
pub const HYPHEN_RELAX_SLACK_MIN: f32 = 1.0;
/// Наибольший такой люфт: чем он больше, тем шире зона послабления.
pub const HYPHEN_RELAX_SLACK_MAX: f32 = 3.0;
/// Штатное значение — `HyphenBudget::DEFAULT.slack_hi`.
pub const HYPHEN_RELAX_SLACK_DEFAULT: f32 = 2.0;

/// Наименьший порог качества: показываются только формы, почти не уступающие лучшей.
pub const QUALITY_FLOOR_MIN: f32 = 0.2;
/// Наибольший порог качества: отбрасываются лишь совсем провальные корзины.
pub const QUALITY_FLOOR_MAX: f32 = 3.0;
/// Штатный порог качества (план §2.3): `Q_best + 0.90`.
pub const QUALITY_FLOOR_DEFAULT: f32 = 0.9;

/// Наименьшее число вариантов на одну высоту формы.
pub const PER_BUCKET_MIN: usize = 1;
/// Наибольшее число вариантов на одну высоту формы.
pub const PER_BUCKET_MAX: usize = 40;
/// Штатное число вариантов на высоту (план §2.3).
pub const PER_BUCKET_DEFAULT: usize = 14;

/// Наименьшее число мест «узкой» корзины в одном круге показа (без приоритета).
pub const NARROW_SLOTS_MIN: usize = 1;
/// Наибольшее число мест «узкой» корзины в одном круге показа.
pub const NARROW_SLOTS_MAX: usize = 3;
/// Штатный приоритет узких форм: два места за круг (план §2.3).
pub const NARROW_SLOTS_DEFAULT: usize = 2;

/// По умолчанию фильтры окна (число строк, ширина) СОКРАЩАЮТ перебор, а не
/// фильтруют его результат.
pub const FILTERS_PRUNE_DEFAULT: bool = true;

/// Множитель перевода порога качества в единицы `TextForm::quality_milli`
/// (оценка `Q` хранится в тысячных).
const QUALITY_MILLI_SCALE: f32 = 1000.0;

// Имена полей персиста (`user_config.json` → `TextTab.advanced_form_search`).
// Приватные: наружу формат уходит только через `to_config_value`/`from_config_value`.
const EVENNESS_KEY: &str = "evenness";
const ASPECT_MAX_KEY: &str = "aspect_max";
const HYPHEN_RATIO_KEY: &str = "hyphen_ratio";
const HYPHEN_RELAX_SLACK_KEY: &str = "hyphen_relax_slack";
const QUALITY_FLOOR_KEY: &str = "quality_floor";
const PER_BUCKET_KEY: &str = "per_bucket";
const NARROW_SLOTS_KEY: &str = "narrow_slots";
const FILTERS_PRUNE_KEY: &str = "filters_prune";

/// Настройки поиска и показа форм текста, которыми управляет пользователь окна
/// «Продвинутая форма текста».
///
/// Все поля лежат в диапазонах `*_MIN..=*_MAX` этого модуля: значение вне
/// диапазона возможно только у руками правленого конфига и приводится к нему
/// [`AdvancedFormParams::clamp_to_supported_range`] — движок никогда не видит
/// «отравленных» чисел.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdvancedFormParams {
    /// «Ровность строк» — множитель полуширины коридора ширин
    /// ([`EVENNESS_MIN`]..=[`EVENNESS_MAX`], по умолчанию [`EVENNESS_DEFAULT`]).
    /// Меньше — коридор уже, форм меньше, но они ровнее.
    pub evenness: f32,
    /// Потолок пропорции формы `max_width / (строк × высота строки)`
    /// ([`ASPECT_MAX_MIN`]..=[`ASPECT_MAX_MAX`], по умолчанию
    /// [`ASPECT_MAX_DEFAULT`] = 21/9). Более широкие формы не перебираются вовсе.
    pub aspect_max: f32,
    /// Доля строк, которым разрешён перенос при комфортной ширине
    /// ([`HYPHEN_RATIO_MIN`]..=[`HYPHEN_RATIO_MAX`], по умолчанию
    /// [`HYPHEN_RATIO_DEFAULT`]) — `HyphenBudget::ratio_strict`.
    pub hyphen_ratio: f32,
    /// Люфт (`max_width / минимально возможная ширина`), начиная с которого
    /// действует `hyphen_ratio` без послабления — `HyphenBudget::slack_hi`
    /// ([`HYPHEN_RELAX_SLACK_MIN`]..=[`HYPHEN_RELAX_SLACK_MAX`], по умолчанию
    /// [`HYPHEN_RELAX_SLACK_DEFAULT`]). Ниже него доля растёт к 100 %.
    pub hyphen_relax_slack: f32,
    /// Порог качества показа: форма отбрасывается, если её `Q` хуже лучшей более
    /// чем на это значение ([`QUALITY_FLOOR_MIN`]..=[`QUALITY_FLOOR_MAX`], по
    /// умолчанию [`QUALITY_FLOOR_DEFAULT`]).
    pub quality_floor: f32,
    /// Сколько лучших форм оставлять в корзине одной высоты
    /// ([`PER_BUCKET_MIN`]..=[`PER_BUCKET_MAX`], по умолчанию
    /// [`PER_BUCKET_DEFAULT`]).
    pub per_bucket: usize,
    /// Сколько карточек «узкая» корзина отдаёт за один круг показа
    /// ([`NARROW_SLOTS_MIN`]..=[`NARROW_SLOTS_MAX`], по умолчанию
    /// [`NARROW_SLOTS_DEFAULT`]); остальные корзины отдают по одной.
    pub narrow_slots: usize,
    /// Фильтры окна (число строк, ширина) СОКРАЩАЮТ перебор, а не фильтруют его
    /// результат (по умолчанию [`FILTERS_PRUNE_DEFAULT`]).
    ///
    /// Переключение в ЛЮБУЮ сторону сами диапазоны СОХРАНЯЕТ: этот флаг решает
    /// только, попадают ли они в ключ поиска, поэтому он намеренно не входит в
    /// `AdvancedFormSearchBase` (сменой базы стёрло бы оба диапазона).
    pub filters_prune: bool,
}

impl Default for AdvancedFormParams {
    fn default() -> Self {
        Self {
            evenness: EVENNESS_DEFAULT,
            aspect_max: ASPECT_MAX_DEFAULT,
            hyphen_ratio: HYPHEN_RATIO_DEFAULT,
            hyphen_relax_slack: HYPHEN_RELAX_SLACK_DEFAULT,
            quality_floor: QUALITY_FLOOR_DEFAULT,
            per_bucket: PER_BUCKET_DEFAULT,
            narrow_slots: NARROW_SLOTS_DEFAULT,
            filters_prune: FILTERS_PRUNE_DEFAULT,
        }
    }
}

impl AdvancedFormParams {
    /// Загоняет каждое поле в поддерживаемый диапазон этого модуля.
    ///
    /// Нечисловое (`NaN`) значение заменяется дефолтом поля, бесконечность
    /// прижимается к границе. Вызывается на каждом входе значений извне (персист,
    /// сеттер) и ещё раз перед сборкой [`FormSearchParams`], поэтому руками
    /// правленый `user_config.json` не может отравить перебор.
    pub fn clamp_to_supported_range(&mut self) {
        self.evenness = clamp_knob(self.evenness, EVENNESS_MIN, EVENNESS_MAX, EVENNESS_DEFAULT);
        self.aspect_max = clamp_knob(
            self.aspect_max,
            ASPECT_MAX_MIN,
            ASPECT_MAX_MAX,
            ASPECT_MAX_DEFAULT,
        );
        self.hyphen_ratio = clamp_knob(
            self.hyphen_ratio,
            HYPHEN_RATIO_MIN,
            HYPHEN_RATIO_MAX,
            HYPHEN_RATIO_DEFAULT,
        );
        self.hyphen_relax_slack = clamp_knob(
            self.hyphen_relax_slack,
            HYPHEN_RELAX_SLACK_MIN,
            HYPHEN_RELAX_SLACK_MAX,
            HYPHEN_RELAX_SLACK_DEFAULT,
        );
        self.quality_floor = clamp_knob(
            self.quality_floor,
            QUALITY_FLOOR_MIN,
            QUALITY_FLOOR_MAX,
            QUALITY_FLOOR_DEFAULT,
        );
        self.per_bucket = self.per_bucket.clamp(PER_BUCKET_MIN, PER_BUCKET_MAX);
        self.narrow_slots = self.narrow_slots.clamp(NARROW_SLOTS_MIN, NARROW_SLOTS_MAX);
    }

    /// Порог качества в единицах `TextForm::quality_milli` (тысячные `Q`).
    ///
    /// Форма показывается, пока её `quality_milli` не превышает
    /// `лучшее + это значение`. Считается по прижатому к диапазону полю.
    #[must_use]
    pub fn quality_floor_milli(&self) -> u32 {
        let floor = clamp_knob(
            self.quality_floor,
            QUALITY_FLOOR_MIN,
            QUALITY_FLOOR_MAX,
            QUALITY_FLOOR_DEFAULT,
        );
        round_to_u32(floor * QUALITY_MILLI_SCALE)
    }

    /// Строит настройки движкового поиска форм ([`forms::search_forms`]).
    ///
    /// `line_height_units` — высота строки В ЕДИНИЦАХ ТОЙ ЖЕ МЕТРИКИ, что и
    /// ширины (перевод из пикселей — забота вызывающего, см.
    /// [`FormSearchParams::line_height_units`]). `line_range` / `width_range` —
    /// жёсткие диапазоны фильтров окна; они попадают в поиск ТОЛЬКО при включённом
    /// [`AdvancedFormParams::filters_prune`], иначе оба уходят как `None` и
    /// фильтрация остаётся пост-обработкой показа.
    ///
    /// Отображение «ровности» `k` на лестницу коридоров (замеренное отображение
    /// плана §3b, применяется к КАЖДОЙ ступени `forms::default_corridor_ladder()`):
    ///
    /// ```text
    /// interior_lo → 1 − (1 − interior_lo)·k
    /// interior_hi → 1 + (interior_hi − 1)·k
    /// head_lo     → 1 − (1 − head_lo)·k
    /// tail_lo     → 1 − (1 − tail_lo)·k
    /// ```
    ///
    /// При `k = 1.0` лестница воспроизводится ТОЧНО (на единице все четыре
    /// пересчёта точны в f32). ВСЕ ЧЕТЫРЕ границы подчиняются одному закону:
    /// стягиваются к идеальной ширине `T_L` при `k < 1` и расходятся при `k > 1`.
    /// Мультипликативное отображение краёв (`head_lo·k`) отвергнуто замером — оно
    /// ОСЛАБЛЯЛО края ровно тогда, когда пользователь просит БОЛЬШЕЙ ровности, и
    /// число карточек переставало монотонно зависеть от ручки (медиана по корпусу
    /// росла до `k = 1.5` и падала на `k = 2.0`). С этим законом отклик монотонный:
    /// медиана 2 / 6 / 10 / 12 карточек при `k` = 0.5 / 1.0 / 1.5 / 2.0.
    ///
    /// Остальные поля (веса качества, бюджеты узлов) берутся из
    /// [`FormSearchParams::default`] — пользователю они не показываются.
    #[must_use]
    pub fn to_search_params(
        self,
        line_height_units: f32,
        line_range: Option<(usize, usize)>,
        width_range: Option<(u32, u32)>,
    ) -> FormSearchParams {
        let mut knobs = self;
        knobs.clamp_to_supported_range();
        let tightness = knobs.evenness;
        let corridor_levels = forms::default_corridor_ladder()
            .into_iter()
            .map(|level| {
                // At the default evenness the engine ladder must survive BIT-EXACTLY:
                // `1.0 - (1.0 - x)` is not the identity in f32 (0.34 has no exact binary
                // form), so a round-trip would silently shift every bound by an ULP and
                // the panel would stop reproducing `FormSearchParams::default()`.
                if (tightness - EVENNESS_DEFAULT).abs() < f32::EPSILON {
                    return level;
                }
                CorridorLevel {
                    interior_lo: 1.0 - (1.0 - level.interior_lo) * tightness,
                    interior_hi: 1.0 + (level.interior_hi - 1.0) * tightness,
                    head_lo: 1.0 - (1.0 - level.head_lo) * tightness,
                    tail_lo: 1.0 - (1.0 - level.tail_lo) * tightness,
                }
            })
            .collect();
        let budget = HyphenBudget::DEFAULT;
        let hyphen = HyphenBudget {
            ratio_strict: knobs.hyphen_ratio,
            // `slack_hi <= slack_lo` — вырожденная настройка, при которой движок
            // молча возвращается к строгой доле; поднимаем нижнюю границу вместо
            // того, чтобы отдать её движку и потерять послабление целиком.
            slack_hi: knobs.hyphen_relax_slack.max(budget.slack_lo),
            ..budget
        };
        let (line_range, width_range) = if knobs.filters_prune {
            (line_range, width_range)
        } else {
            (None, None)
        };
        FormSearchParams {
            aspect_max: knobs.aspect_max,
            line_height_units,
            corridor_levels,
            hyphen,
            per_bucket: knobs.per_bucket,
            line_range,
            width_range,
            ..FormSearchParams::default()
        }
    }

    /// JSON-объект, который пишется в `user_config.json` под
    /// `TextTab.advanced_form_search`. Пишутся ВСЕ поля: объект целиком заменяет
    /// прежний, а частичный объект остаётся читаемым (см.
    /// [`AdvancedFormParams::from_config_value`]).
    #[must_use]
    pub fn to_config_value(self) -> Value {
        let mut knobs = self;
        knobs.clamp_to_supported_range();
        let mut object = Map::new();
        object.insert(EVENNESS_KEY.to_string(), f32_to_value(knobs.evenness));
        object.insert(ASPECT_MAX_KEY.to_string(), f32_to_value(knobs.aspect_max));
        object.insert(
            HYPHEN_RATIO_KEY.to_string(),
            f32_to_value(knobs.hyphen_ratio),
        );
        object.insert(
            HYPHEN_RELAX_SLACK_KEY.to_string(),
            f32_to_value(knobs.hyphen_relax_slack),
        );
        object.insert(
            QUALITY_FLOOR_KEY.to_string(),
            f32_to_value(knobs.quality_floor),
        );
        object.insert(PER_BUCKET_KEY.to_string(), usize_to_value(knobs.per_bucket));
        object.insert(
            NARROW_SLOTS_KEY.to_string(),
            usize_to_value(knobs.narrow_slots),
        );
        object.insert(FILTERS_PRUNE_KEY.to_string(), Value::Bool(knobs.filters_prune));
        Value::Object(object)
    }

    /// Читает сохранённый объект настроек.
    ///
    /// Каждое ОТСУТСТВУЮЩЕЕ или неразобравшееся поле сохраняет свой
    /// скомпилированный дефолт, поэтому частичный (и вообще любой) объект —
    /// поддерживаемый вход; не-объект целиком даёт дефолты. Результат уже прижат
    /// к поддерживаемым диапазонам.
    #[must_use]
    pub fn from_config_value(value: &Value) -> Self {
        let mut params = Self::default();
        let Some(object) = value.as_object() else {
            return params;
        };
        params.evenness = read_f32(object, EVENNESS_KEY, params.evenness);
        params.aspect_max = read_f32(object, ASPECT_MAX_KEY, params.aspect_max);
        params.hyphen_ratio = read_f32(object, HYPHEN_RATIO_KEY, params.hyphen_ratio);
        params.hyphen_relax_slack = read_f32(
            object,
            HYPHEN_RELAX_SLACK_KEY,
            params.hyphen_relax_slack,
        );
        params.quality_floor = read_f32(object, QUALITY_FLOOR_KEY, params.quality_floor);
        params.per_bucket = read_usize(object, PER_BUCKET_KEY, params.per_bucket);
        params.narrow_slots = read_usize(object, NARROW_SLOTS_KEY, params.narrow_slots);
        params.filters_prune = object
            .get(FILTERS_PRUNE_KEY)
            .and_then(Value::as_bool)
            .unwrap_or(params.filters_prune);
        params.clamp_to_supported_range();
        params
    }
}

/// Прижимает вещественную «ручку» к `[min, max]`; `NaN` заменяется дефолтом поля.
///
/// `f32::clamp` пропускает `NaN` насквозь (и паникует при `min > max`), поэтому
/// нечисловое значение обрабатывается до него.
#[must_use]
fn clamp_knob(value: f32, min: f32, max: f32, default: f32) -> f32 {
    if value.is_nan() {
        return default;
    }
    value.clamp(min, max)
}

/// Округляет неотрицательное конечное `f32` к `u32` с насыщением.
#[must_use]
fn round_to_u32(value: f32) -> u32 {
    if value.is_nan() || value <= 0.0 {
        return 0;
    }
    let rounded = f64::from(value).round();
    if rounded >= f64::from(u32::MAX) {
        return u32::MAX;
    }
    // Проверки выше доказывают `0.0 < rounded < u32::MAX` — приведение точное.
    rounded as u32
}

/// Конечное `f32` как JSON-число (`f32` → `f64` расширяется без потерь).
#[must_use]
fn f32_to_value(value: f32) -> Value {
    Value::from(f64::from(value))
}

/// `usize` как JSON-число. Насыщение недостижимо (значения прижаты к
/// `*_MAX <= 40`), но держит функцию тотальной без `unwrap`.
#[must_use]
fn usize_to_value(value: usize) -> Value {
    Value::from(u64::try_from(value).unwrap_or(u64::MAX))
}

/// Читает вещественное поле объекта; отсутствующее, не-число и нечисловое
/// (`NaN`/бесконечность) значение даёт `default`.
#[must_use]
fn read_f32(object: &Map<String, Value>, key: &str, default: f32) -> f32 {
    let Some(raw) = object
        .get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
    else {
        return default;
    };
    // Единственное сужение без проверяемой формы в стандартной библиотеке.
    // Значение конечно и сразу после этого прижимается к диапазону ручки
    // (`clamp_to_supported_range` у вызывающего), поэтому потеря ограничена
    // младшими битами мантиссы, а выход за `f32` насыщается в бесконечность и
    // прижимается к границе.
    raw as f32
}

/// Читает целочисленное поле объекта; отсутствующее, не-целое и не помещающееся
/// в `usize` значение даёт `default`.
#[must_use]
fn read_usize(object: &Map<String, Value>, key: &str, default: usize) -> usize {
    object
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|raw| usize::try_from(raw).ok())
        .unwrap_or(default)
}

/// Хранилище процесс-глобального значения.
///
/// Восемь полей не помещаются в атомик, поэтому берётся минимальное достаточное:
/// `RwLock` за `OnceLock` (дефолт поля `aspect_max` — деление, не `const`-выражение,
/// так что `static RwLock::new(..)` его не инициализирует). Чтение — раз в кадр при
/// открытом окне, запись — только из секции параметров и стартового засева, так что
/// незанятый `RwLock` заведомо ниже шума.
static PARAMS: OnceLock<RwLock<AdvancedFormParams>> = OnceLock::new();

/// Единожды сообщает об отравленной блокировке: и чтение, и запись идут за кадр,
/// а лог не должен превращаться в поток.
static POISON_REPORTED: Once = Once::new();

/// Хранилище значения, инициализируемое дефолтами при первом обращении.
fn params_lock() -> &'static RwLock<AdvancedFormParams> {
    PARAMS.get_or_init(|| RwLock::new(AdvancedFormParams::default()))
}

/// Сообщает об отравленной блокировке один раз за процесс.
fn report_poisoned_lock(operation: &str) {
    POISON_REPORTED.call_once(|| {
        runtime_log::log_warn(format!(
            "[typing] advanced form search params lock is poisoned ({operation}); \
             falling back to the compiled-in defaults"
        ));
    });
}

/// Текущие настройки поиска форм.
///
/// Отравленная блокировка (возможна только если поток паниковал, держа её, — под
/// ней выполняется единственное копирование) отдаёт скомпилированные дефолты, а
/// не панику: окно форм не имеет права уронить GUI-поток.
#[must_use]
pub fn advanced_form_params() -> AdvancedFormParams {
    match params_lock().read() {
        Ok(guard) => *guard,
        Err(_) => {
            report_poisoned_lock("read");
            AdvancedFormParams::default()
        }
    }
}

/// Устанавливает настройки поиска форм; значение прижимается к поддерживаемым
/// диапазонам. Подхватывается следующей перестройкой кэша форм; ничего другого
/// не инвалидируется.
pub fn set_advanced_form_params(params: AdvancedFormParams) {
    let mut clamped = params;
    clamped.clamp_to_supported_range();
    match params_lock().write() {
        Ok(mut guard) => *guard = clamped,
        Err(_) => report_poisoned_lock("write"),
    }
}
