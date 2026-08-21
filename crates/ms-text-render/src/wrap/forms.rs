/*
File: src/tabs/typing/render_next/wrap/forms.rs

Purpose:
Общая (shared) логика дискретных «форм» текста: разбиение текста на строки так,
чтобы последовательность ширин строк удовлетворяла выбранной форме.

Used by:
- панель typing (`panel.rs`) — окно «Продвинутая форма текста» получает
  ранжированный набор форм (`search_forms`) и отображает их обычным egui-текстом;
- новый рендер (`render_next`) — `choose_form` подбирает одну форму поверх
  существующего scored-wrap, не переписывая его.

Две точки входа в перебор:
- `enumerate_forms` — исчерпывающий перебор без ранжирования (порядок обхода
  дерева, ограничители: `max_forms`, свободная память, потолок узлов). Путь
  `choose_form`; на больших текстах комбинаторно взрывается.
- `search_forms` — ранжированный поиск (`dev-docs/text_forms_ranking_plan.md`).
  Отдельный ограниченный перебор НА КАЖДУЮ ВЫСОТУ формы внутри коридора ширин,
  с потолком пропорции, бюджетом переносов и оценкой качества `Q`. Все числовые
  решения вынесены в `FormSearchParams`; дедлайна по часам нет (крейт собирается
  и под wasm) — только детерминированные бюджеты по числу узлов.

The input text is RAW, and the tags come back:
Both search entry points and the width metric (`GlyphWidths::build`) take the text WITH
its inline tags, plus an `InlineTagScope` saying which of them are markup here. This file
is the only place that removes them (`strip_inline_tags`, private for exactly that reason)
and the only place that puts them back (`reapply_inline_tags_to_form_text`, applied to the
CHOSEN form only). The vocabulary itself is not defined here: it comes from
`inline_styles::classify_inline_tag_body`, the same parser the renderer uses, so "what is a
tag" cannot differ between the text a form is built from and the text that gets drawn.
Handing this file an already stripped string leaves it with no tags to honour — no
protection and nothing to restore. See `wrap/MODULE_README.md`, "Inline tags: the text
arrives RAW and the tags go back".

Дерево перебора:
Текст заранее делится на блоки сегментатором (`segmentation::Segmenter::segment`
после словарной разметки мягкими переносами, в режиме `BindingMode::Annotate`):
орфографические точки переноса (словарь + существующие дефисы; аварийных разрывов
нет). Служебные слова (предлоги/частицы/«число + единица») НЕ склеиваются — вместо
этого стык к следующему блоку несёт категорию консервативности (`Conservatism`).
Каждый блок несёт стык (`Joint`) к следующему: пробел / словарный перенос /
существующий дефис. Дерево на каждой границе решает «рвём или нет»; ветка отмирает,
как только закрытая строка нарушает форму.

Консервативность формы («один граф — потом фильтр»):
строим перебор один раз, а каждой форме приписываем `conservatism` = максимум
категорий по её фактическим разрывам (на каждом ветвлении берём `max` с категорией
взятого стыка). Затем формы фильтруются по выбранному порогу: `Safe` — только
безопасные переносы (поведение «как при склейке предлогов»), выше — допускаются
отрывы предлогов/частиц. См. окно «Продвинутая форма текста».

Цена разрыва (поле `Joint::break_cost`, только для сортировки, на отбор не влияет):
- пробел — 0;
- существующий дефис («Рао-кун») — 1;
- словарный перенос — 2/3/4 по типографско-лингвистическому качеству
  (хорошо/средне/неприятно), оценку даёт сегментатор языка.

Width metric:
Ширина строки берётся через `LineWidthMetric`. По умолчанию панель строит
`GlyphWidths` — попиксельную метрику тем же шейпером (cosmic-text,
`Shaping::Advanced`), что и финальный рендер: заранее меряются ширины глифов
встретившихся символов (плюс дефис переноса) и поправки кернинга для соседних
пар, после чего ширина строки = сумма ширин глифов + кернинг. Это ловит случаи,
где пробел/узкий глиф делает строку короче при равном числе символов. Если шрифт
недоступен, используется `CharWidthMetric` (счёт символов, прежнее поведение).
Висящая пунктуация: при включённой — ведущая/хвостовая висящая пунктуация (и
дефис переноса) не идёт в ширину; при выключенной — считается. Сравнение ширин в
предикатах формы идёт с допуском (`tolerance`), чтобы суб-глифовый джиттер не
создавал ложных подъёмов/спусков.

Notes:
- пропорция формы считается в единицах метрики: `line_height_units` приходит от
  вызывающего (см. док-комментарий поля), потому что перевод пикселей в единицы
  знает только он — `GlyphWidths` меряет в 1/1000 em, `CharWidthMetric` в символах;
- `search_forms` держит memo ширин `(start, end) → ширина` (`WidthMemo`): плотная
  таблица на `n²` ячеек по 8 байт, пока `n²` укладывается в
  `DENSE_WIDTH_MEMO_MAX_CELLS`, дальше — разрежённая хеш-таблица с потолком
  записей. Текст строки в memo не хранится: он нужен только при выдаче формы и
  собирается там заново;
- вещественные поля `FormSearchParams` санируются на входе `search_forms`
  (`NaN`/бесконечность/вне области → значение по умолчанию): крейт GUI-free,
  логировать и падать здесь нечему, а `NaN` молча снимал бы гарантии слоя A;
- бюджет узлов `node_budget_total` — один на весь вызов `search_forms`, включая
  аварийный прогон со снятым потолком пропорции.
*/

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping};

use super::is_hanging_punctuation;
use crate::inline_styles::{InlineTagClass, classify_inline_tag_body};
use ms_text_util::segmentation::{
    BindingMode, Block, Conservatism, NON_BREAKING_SPACE, SOFT_HYPHEN, SegmentOptions,
    build_line_text_and_units, is_line_end_dash_char, with_default_segmenter,
};

/// Максимум перечисляемых форм за один прогон (защита от комбинаторного взрыва).
pub const DEFAULT_MAX_FORMS: usize = 600;

/// Нижний порог свободной памяти, при котором перечисление останавливается
/// досрочно (`truncated`). ≈ 512 MiB — запас на последующую сортировку/клон/
/// отрисовку результатов, чтобы остановиться заметно раньше OOM-killer'а.
const MIN_AVAILABLE_MEMORY_BYTES: u64 = 512 * 1024 * 1024;

/// Как часто (в узлах DFS) проверять свободную память. Чтение `/proc/meminfo`
/// на каждом узле было бы дорого, поэтому проверяем раз в N узлов.
const MEMORY_CHECK_INTERVAL_NODES: u64 = 8192;

/// Аварийный потолок числа узлов DFS — гарантия завершения для случая, когда
/// свободную память наблюдать нельзя (`available_memory_bytes()` вернул `None`,
/// напр. не-Linux) ИЛИ память просто никогда не падает ниже порога. На Linux с
/// читаемым `MemAvailable` практический ограничитель — память, а не этот потолок
/// (то есть фактически «без лимита по количеству, только по памяти»).
const SAFETY_NODE_CEILING: u64 = 50_000_000;

/// Доступная («OOM-релевантная») память процесса в байтах. На Linux читает
/// `MemAvailable:` из `/proc/meminfo` (значение в кБ → ×1024); эта метрика
/// учитывает освобождаемый кеш, поэтому точнее, чем `MemFree`. На macOS запускает
/// `vm_stat` и приближает доступную память как (free+inactive+speculative) страниц
/// × размер страницы — reclaimable-аналог `MemAvailable`. Возвращает `None`, если
/// источник нельзя прочитать/распарсить (прочие ОС или ошибка).
#[cfg(target_os = "linux")]
fn available_memory_bytes() -> Option<u64> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

/// TTL для кеша macOS-замера памяти. `available_memory_bytes` дёргается из DFS
/// перебора форм (раз в `MEMORY_CHECK_INTERVAL_NODES` узлов, а перечислений может
/// быть много подряд). На Linux это дешёвое чтение `/proc/meminfo`, а на macOS —
/// fork+exec `vm_stat` (несколько мс). 750мс схлопывает частые вызовы в один
/// замер за окно, не мешая защите по памяти вовремя останавливать перебор.
#[cfg(target_os = "macos")]
const MACOS_MEM_CACHE_TTL: std::time::Duration = std::time::Duration::from_millis(750);

/// macOS: приближение доступной памяти через `vm_stat` (см. общий doc выше),
/// с коротким TTL-кешем (`MACOS_MEM_CACHE_TTL`) поверх подпроцесса, чтобы частые
/// вызовы из перебора форм не форкали `vm_stat` каждый раз. Контракт возврата тот
/// же, что у сырого замера: `None` — память неизвестна. Кеш только на macOS;
/// Linux/прочие ОС вызывают свой путь без кеша.
#[cfg(target_os = "macos")]
fn available_memory_bytes() -> Option<u64> {
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    // Локальный TTL-кеш: обоснованное ограниченное исключение из правила «без
    // глобального мутабельного состояния» (чистый временной кеш). Отравленный
    // (poisoned) lock не паникует, а откатывается к свежему замеру. Почти дубль
    // кешей в `memory_manager.rs` / `clean_overlays_model.rs` — они в других
    // крейтах, поэтому реализация повторяется локально.
    static CACHE: OnceLock<Mutex<Option<(Instant, Option<u64>)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock()
        && let Some((stamped_at, value)) = guard.as_ref()
        && stamped_at.elapsed() < MACOS_MEM_CACHE_TTL
    {
        return *value;
    }
    // Замер БЕЗ удержания lock: никогда не fork+exec под Mutex.
    let value = probe_available_memory_bytes();
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), value));
    }
    value
}

/// Сырой (без кеша) macOS-замер доступной памяти через `vm_stat`. `None`, если
/// команда не запустилась/вышла с ошибкой или вывод не распарсился. Вызывающие
/// идут через [`available_memory_bytes`], который кеширует результат на TTL.
#[cfg(target_os = "macos")]
fn probe_available_memory_bytes() -> Option<u64> {
    let out = std::process::Command::new("vm_stat").output().ok()?;
    if !out.status.success() {
        ms_log::runtime_log::log_warn("[wrap/forms] `vm_stat` exited with failure status");
        return None;
    }
    parse_vm_stat_available_bytes(&String::from_utf8_lossy(&out.stdout))
}

/// На прочих (не-Linux, не-macOS) ОС метрика недоступна — память не наблюдаем,
/// работает только аварийный потолок узлов (`SAFETY_NODE_CEILING`).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn available_memory_bytes() -> Option<u64> {
    None
}

/// Extracts the page size in bytes from the `vm_stat` header line ending with
/// "(page size of N bytes)". Returns `None` if the marker is absent or unparseable.
#[cfg(any(target_os = "macos", test))]
fn parse_vm_stat_page_size(line: &str) -> Option<u64> {
    line.split("page size of")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
}

/// Parses a `vm_stat` "Pages <name>:" row into its page count, stripping the
/// trailing '.'. Returns `None` when the prefix does not match or the number
/// cannot be parsed.
#[cfg(any(target_os = "macos", test))]
fn parse_vm_stat_pages(line: &str, key: &str) -> Option<u64> {
    line.strip_prefix(key)?
        .trim()
        .trim_end_matches('.')
        .parse::<u64>()
        .ok()
}

/// Approximates macOS available memory (bytes) from `vm_stat` output as
/// (free+inactive+speculative) pages * page size — the reclaimable-memory analog
/// of Linux `MemAvailable`. Inactive/speculative pages are included because they
/// are reclaimable under pressure. Uses checked arithmetic; on the physically
/// unreachable overflow it warns and returns `None`. Returns `None` if the page
/// size or any required page count is missing/unparseable.
#[cfg(any(target_os = "macos", test))]
fn parse_vm_stat_available_bytes(text: &str) -> Option<u64> {
    let mut page_size: Option<u64> = None;
    let mut free: Option<u64> = None;
    let mut inactive: Option<u64> = None;
    let mut speculative: Option<u64> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if page_size.is_none()
            && let Some(size) = parse_vm_stat_page_size(line)
        {
            page_size = Some(size);
        }
        if let Some(v) = parse_vm_stat_pages(line, "Pages free:") {
            free = Some(v);
        } else if let Some(v) = parse_vm_stat_pages(line, "Pages inactive:") {
            inactive = Some(v);
        } else if let Some(v) = parse_vm_stat_pages(line, "Pages speculative:") {
            speculative = Some(v);
        }
    }

    let page_size = page_size?;
    let pages = free?.checked_add(inactive?)?.checked_add(speculative?)?;
    match pages.checked_mul(page_size) {
        Some(bytes) => Some(bytes),
        None => {
            ms_log::runtime_log::log_warn(
                "[wrap/forms] vm_stat page count * page size overflowed u64; reporting unknown",
            );
            None
        }
    }
}

/// Источник свободной памяти, который консультирует `enumerate_dfs`. В обычной
/// сборке — прямой вызов `available_memory_bytes()`. В тестах подменяется через
/// `tests::with_memory_source`, чтобы проверять защиту без реального исчерпания
/// ОЗУ.
#[cfg(test)]
fn current_available_memory() -> Option<u64> {
    tests::test_available_memory()
}

#[cfg(not(test))]
#[inline]
fn current_available_memory() -> Option<u64> {
    available_memory_bytes()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextFormPreset {
    /// «Свободный без ёлки».
    FreeNoTree,
    /// «(  )».
    Lens,
    /// «/  \».
    Widen,
    /// «\  /».
    Narrow,
}

/// How a preset's UI label is produced. This crate is GUI-free and must not depend
/// on the UI-string catalog (see `docs/i18n_exclusions.md` §F), so a localizable
/// label is returned as a catalog key for the binary to resolve, while a fixed
/// ASCII shape sketch is returned verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetLabel {
    /// Catalog key; the binary resolves it via `ms_i18n::lookup(key).unwrap_or(key)`.
    Key(&'static str),
    /// Literal ASCII shape sketch — geometry (`(  )`, `/  \`, `\  /`), not words.
    /// Painted verbatim; must never be localized.
    Shape(&'static str),
}

impl TextFormPreset {
    /// UI label for this preset. Only `FreeNoTree` is prose (a catalog key); the
    /// other three are ASCII shape sketches painted verbatim. Total (exhaustive
    /// match, no catch-all).
    #[must_use]
    pub fn label(self) -> PresetLabel {
        match self {
            TextFormPreset::FreeNoTree => {
                PresetLabel::Key("typing.advanced.form_preset_free_no_tree")
            }
            // ASCII shapes are geometric sketches of the form, not translatable text.
            TextFormPreset::Lens => PresetLabel::Shape("(  )"),
            TextFormPreset::Widen => PresetLabel::Shape("/  \\"),
            TextFormPreset::Narrow => PresetLabel::Shape("\\  /"),
        }
    }

    #[must_use]
    pub fn all() -> [TextFormPreset; 4] {
        [
            TextFormPreset::FreeNoTree,
            TextFormPreset::Lens,
            TextFormPreset::Widen,
            TextFormPreset::Narrow,
        ]
    }
}

/// Одна конкретная форма — текст, разбитый на строки, плюс метрики для
/// группировки/сортировки в окне.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextForm {
    pub lines: Vec<String>,
    /// Число переносов слов (непробельных разрывов: словарных + существующих дефисов).
    pub word_break_count: usize,
    /// Максимальная ширина строки формы (в единицах метрики, см. `LineWidthMetric`).
    pub max_width: u32,
    /// Минимальная ширина строки формы (в единицах метрики).
    pub min_width: u32,
    /// Медианная ширина строки формы (в единицах метрики).
    pub median_width: u32,
    /// «Неравномерность» формы в % — среднее отклонение ширин строк от медианы,
    /// в долях медианы. `0%` — все строки одной ширины; выше — сильнее разброс.
    pub unevenness_pct: u32,
    /// Накопленная цена разрывов формы (для сортировки внутри одинаковой ширины).
    pub break_cost: u32,
    /// Консервативность формы — максимум категорий по её фактическим разрывам.
    /// `Safe` — все переносы безопасны; выше — есть отрыв предлога/частицы и т.п.
    /// По этому полю формы фильтруются (см. окно «Продвинутая форма текста»).
    pub conservatism: Conservatism,
    /// Ширина каждой строки формы (в единицах метрики), в порядке строк. Заполняют
    /// оба пути перебора; потребителям не нужно перемерять строки заново.
    pub line_widths: Vec<u32>,
    /// Оценка качества `Q` × 1000 (плановый слой B, §2.2): МЕНЬШЕ — ЛУЧШЕ.
    /// Заполняет только [`search_forms`]; [`enumerate_forms`] проставляет
    /// [`UNSCORED_QUALITY_MILLI`] («не оценивалось»), чтобы «неоценённое» никогда
    /// не выглядело идеальным при сортировке по возрастанию.
    pub quality_milli: u32,
    /// «Шероховатость» профиля ширин в % (терм `rough` из §2.2 × 100): смесь
    /// максимального и среднего скачка ширины между соседними строками,
    /// нормированная на медианную ширину, с уценкой краевых переходов.
    /// `0%` — все строки одной ширины. Заполняют оба пути перебора.
    pub roughness_pct: u32,
    /// Пропорция формы `max_width / (число строк × line_height_units)` × 1000
    /// (1000 = квадрат, больше = шире). Заполняет только [`search_forms`]:
    /// [`enumerate_forms`] не знает высоту строки и оставляет `0` («неизвестно»).
    pub aspect_milli: u32,
}

/// Значение [`TextForm::quality_milli`] для форм, которые не проходили оценку
/// качества (выход [`enumerate_forms`]). Намеренно «худшее возможное»: сортировка
/// по возрастанию `quality_milli` не поднимет неоценённую форму наверх.
pub const UNSCORED_QUALITY_MILLI: u32 = u32::MAX;

/// База отсчёта пиковости: с чем сравнивать самую длинную строку.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeakBase {
    /// Самая короткая строка.
    Min,
    /// Медианная строка.
    Median,
}

impl TextForm {
    #[must_use]
    pub fn to_text(&self) -> String {
        self.lines.join("\n")
    }

    /// Число строк формы.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// «Пиковость» формы в процентах: на сколько самая длинная строка длиннее
    /// базовой — `round((max − base) / base × 100)`. База — минимальная или
    /// медианная строка. `0%` — самая длинная не длиннее базовой.
    #[must_use]
    pub fn peakiness_pct(&self, base: PeakBase) -> u32 {
        let base_width = match base {
            PeakBase::Min => self.min_width,
            PeakBase::Median => self.median_width,
        };
        if base_width == 0 {
            return 0;
        }
        let diff = self.max_width.saturating_sub(base_width);
        ((f64::from(diff) / f64::from(base_width)) * 100.0).round() as u32
    }
}

/// Медиана набора ширин (округление половины вверх для чётного числа строк).
#[must_use]
fn median_of_widths(widths: &[u32]) -> u32 {
    if widths.is_empty() {
        return 0;
    }
    let mut sorted = widths.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]).div_ceil(2)
    }
}

/// «Неравномерность» в долях медианы: среднее абсолютное отклонение ширин от
/// медианы, делённое на медиану. Устойчива к одиночным выбросам (короткая
/// последняя строка почти не влияет), но штрафует общий разброс
/// («лесенку»/«воронку»). `0.0` для пустого набора и нулевой медианы.
#[must_use]
fn unevenness_of_widths(widths: &[u32], median: u32) -> f64 {
    if widths.is_empty() || median == 0 {
        return 0.0;
    }
    let mean_abs_dev = widths
        .iter()
        .map(|&w| (f64::from(w) - f64::from(median)).abs())
        .sum::<f64>()
        / count_as_f64(widths.len());
    mean_abs_dev / f64::from(median)
}

/// «Неравномерность» в % — то же, что [`unevenness_of_widths`], округлённое до
/// целых процентов (поле [`TextForm::unevenness_pct`]).
#[must_use]
fn unevenness_pct_of_widths(widths: &[u32], median: u32) -> u32 {
    round_to_u32(unevenness_of_widths(widths, median) * 100.0)
}

// --- Метрика ширины строки -------------------------------------------------

/// Кегль (единиц на em), на котором меряются глифы попиксельной метрики. Числа
/// получаются целыми и не зависят от реального размера текста.
const WIDTH_METRIC_EM: f32 = 1000.0;

/// Допуск сравнения ширин по умолчанию (в единицах метрики, ~3.5% em): разница
/// `≤ tolerance` считается «равно» при классификации формы.
pub const DEFAULT_WIDTH_TOLERANCE: u32 = 35;

/// Источник ширины строки для перебора форм.
///
/// # Контракт для реализаций
/// Ранжированный поиск ([`search_forms`]) обрывает просмотр всё более длинных
/// строк, опираясь на ОДНО свойство метрики (монотонность ширины по длине строки
/// НЕ требуется — её нарушает уже хвостовой дефис переноса, становящийся
/// внутренним): удлинение строки ещё одним блоком не может уменьшить её ширину
/// больше, чем на сумму «ширина самого широкого одиночного блока» +
/// `line_width("-")`. Обе штатные метрики ([`CharWidthMetric`], [`GlyphWidths`])
/// этому удовлетворяют с большим запасом: ширина строки у них — сумма вкладов
/// символов, а единственный исчезающий при удлинении вклад — дефис переноса.
pub trait LineWidthMetric {
    /// Ширина строки в единицах метрики, с учётом висящей пунктуации по краям.
    fn line_width(&self, line: &str) -> u32;
    /// Допуск сравнения ширин для предикатов формы.
    fn tolerance(&self) -> u32;
}

/// Видимое «ядро» строки для замера: без мягких переносов; при включённой
/// висящей пунктуации — ещё и без ведущей/хвостовой висящей пунктуации.
#[must_use]
fn metric_core_text(line: &str, hanging: bool) -> String {
    if hanging {
        let (_, core, _) = split_hanging_edges(line);
        core
    } else {
        line.trim().chars().filter(|&ch| ch != SOFT_HYPHEN).collect()
    }
}

/// Посимвольная метрика (число символов ядра). Запасной вариант без шрифта и
/// прежнее поведение окна форм.
pub struct CharWidthMetric {
    hanging: bool,
}

impl CharWidthMetric {
    #[must_use]
    pub fn new(hanging: bool) -> Self {
        Self { hanging }
    }
}

impl LineWidthMetric for CharWidthMetric {
    fn line_width(&self, line: &str) -> u32 {
        metric_core_text(line, self.hanging).chars().count() as u32
    }

    fn tolerance(&self) -> u32 {
        0
    }
}

/// Попиксельная метрика: ширины глифов + поправки кернинга соседних пар,
/// заранее измеренные шейпером для алфавита текста.
pub struct GlyphWidths {
    advances: HashMap<char, u32>,
    kerns: HashMap<(char, char), i32>,
    hanging: bool,
    tolerance: u32,
}

impl GlyphWidths {
    /// Строит таблицу для символов `form_source_text` (плюс дефис переноса)
    /// выбранным шрифтом `attrs` в системе `font_system`.
    ///
    /// `form_source_text` — СЫРОЙ текст формы, ровно тот, что уйдёт в
    /// [`search_forms`]/[`enumerate_forms`]: инлайновые теги снимаются здесь же
    /// (какие именно — решает `scope`), а пробелы защищённых диапазонов заменяются на
    /// NBSP — то есть алфавит и пары кернинга измеряются по тем символам, которые
    /// реально окажутся в строках формы. Передавать сюда уже очищенный текст
    /// нельзя: тогда NBSP защищённых диапазонов в алфавит не попадёт и такой
    /// пробел будет измерен как нулевая ширина, а пара символов по краям
    /// снятого тега — потеряна.
    ///
    /// `scope` ОБЯЗАН совпадать с тем, что получит [`search_forms`]/[`enumerate_forms`]
    /// для того же текста: иначе метрика меряет не тот алфавит, который сегментируется.
    #[must_use]
    pub fn build(
        font_system: &mut FontSystem,
        attrs: &Attrs<'_>,
        form_source_text: &str,
        hanging: bool,
        tolerance: u32,
        scope: InlineTagScope,
    ) -> Self {
        let text = prepare_inline_no_break_text(form_source_text, scope);
        let visible: Vec<char> = text
            .chars()
            .filter(|&ch| ch != SOFT_HYPHEN && ch != '\n' && ch != '\r')
            .collect();
        let mut alphabet: BTreeSet<char> = visible.iter().copied().collect();
        // Дефис переноса может быть добавлен при разрыве строки.
        alphabet.insert('-');

        let mut scratch = String::new();
        let mut advances = HashMap::with_capacity(alphabet.len());
        for &ch in &alphabet {
            scratch.clear();
            scratch.push(ch);
            advances.insert(ch, measure_units(font_system, attrs, &scratch));
        }

        // Пары: реально встречающиеся подряд + (символ, дефис) на случай переноса.
        let mut pairs: BTreeSet<(char, char)> = BTreeSet::new();
        for window in visible.windows(2) {
            pairs.insert((window[0], window[1]));
        }
        for &ch in &alphabet {
            pairs.insert((ch, '-'));
        }
        let mut kerns = HashMap::with_capacity(pairs.len());
        for &(a, b) in &pairs {
            scratch.clear();
            scratch.push(a);
            scratch.push(b);
            let pair_width = i64::from(measure_units(font_system, attrs, &scratch));
            let sum = i64::from(advances.get(&a).copied().unwrap_or(0))
                + i64::from(advances.get(&b).copied().unwrap_or(0));
            let delta = (pair_width - sum).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
            if delta != 0 {
                kerns.insert((a, b), delta);
            }
        }

        Self {
            advances,
            kerns,
            hanging,
            tolerance,
        }
    }
}

impl LineWidthMetric for GlyphWidths {
    fn line_width(&self, line: &str) -> u32 {
        let chars: Vec<char> = metric_core_text(line, self.hanging).chars().collect();
        let mut width: i64 = 0;
        let mut prev: Option<char> = None;
        for &ch in &chars {
            width += i64::from(self.advances.get(&ch).copied().unwrap_or(0));
            if let Some(p) = prev {
                width += i64::from(self.kerns.get(&(p, ch)).copied().unwrap_or(0));
            }
            prev = Some(ch);
        }
        width.max(0) as u32
    }

    fn tolerance(&self) -> u32 {
        self.tolerance
    }
}

/// Ширина текста в единицах `WIDTH_METRIC_EM`, измеренная шейпером.
#[must_use]
fn measure_units(font_system: &mut FontSystem, attrs: &Attrs<'_>, text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let mut buffer = Buffer::new(font_system, Metrics::new(WIDTH_METRIC_EM, WIDTH_METRIC_EM));
    buffer.set_size(font_system, None, None);
    buffer.set_text(font_system, text, attrs, Shaping::Advanced);
    buffer.shape_until_scroll(font_system, false);
    buffer
        .layout_runs()
        .fold(0.0f32, |max_w, run| max_w.max(run.line_w))
        .round()
        .max(0.0) as u32
}

/// Результат перечисления форм.
#[derive(Debug, Clone)]
pub struct FormEnumeration {
    pub forms: Vec<TextForm>,
    /// Список форм усечён: достигнут лимит `max_forms`, бюджет узлов/форм
    /// [`FormSearchParams`], сработала защита по свободной памяти
    /// (`MIN_AVAILABLE_MEMORY_BYTES`) или аварийный потолок узлов
    /// (`SAFETY_NODE_CEILING`). Отбор `per_bucket` лучших форм в корзине
    /// усечением НЕ считается — это курирование, а не исчерпание бюджета.
    pub truncated: bool,
    /// Сколько узлов дерева перебора было посещено (суммарно по всем корзинам и,
    /// для [`search_forms`], включая аварийный повторный прогон). Диагностика и
    /// проверка того, что жёсткие ограничения поиска действительно СОКРАЩАЮТ
    /// перебор, а не фильтруют его результат.
    pub nodes_visited: u64,
}

/// Which inline tags the form search removes from the RAW text it is handed.
///
/// The choice is not cosmetic: with «Инлайновые теги» OFF the renderer does not parse
/// tags at all (`pipeline.rs`, `enable_inline_style_tags`), so `<b>` is literal text the
/// user wants DRAWN and measured — removing it would make the form describe a text
/// nobody sees. With the flag ON the renderer consumes the tags, so leaving them in
/// would measure markup as if it were letters.
///
/// The search and its width metric ([`GlyphWidths::build`]) MUST be given the same
/// value: a mismatch measures a different alphabet than it segments.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InlineTagScope {
    /// Only the no-break control vocabulary (`<no-break>`/`<nobreak>`/`<nobr>` and the
    /// machine `<m …>` frame that can carry the `j` flag). Everything else stays literal
    /// text. This is what the renderer's own single-pick path ([`choose_form`]) uses.
    NoBreakOnly,
    /// Every inline tag the renderer's parser consumes.
    All {
        /// The `font_size_px` the render of this very text will use.
        ///
        /// Recognition of `<offset=…>` / `<stretching=…>` depends on it (a percent value
        /// is resolved against the font size and a non-finite result stops the body from
        /// being a tag), so passing a different size here than the render receives would
        /// let this strip and the renderer disagree about what is markup.
        base_font_size_px: f32,
    },
}

/// What the form search does with one recognized inline tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagAction {
    /// Leave it in the text: it is not markup at this scope, so the renderer draws it.
    Keep,
    /// Remove it and never put it back — a control tag the form itself supersedes.
    Consume,
    /// Remove it, and remember it so the APPLIED form carries it back verbatim.
    Reapply {
        /// Closing tag: it lands at the END of the preceding line at a break.
        closing: bool,
    },
}

impl InlineTagScope {
    /// Font size the tag vocabulary is consulted with.
    ///
    /// `NoBreakOnly` acts only on the no-break and machine classes, and neither depends
    /// on the font size (they are decided before any percent value is resolved), so the
    /// value is arbitrary there — a style tag classified differently at `0.0` than at the
    /// real size is `Keep`ed either way.
    #[must_use]
    fn classify_font_size_px(self) -> f32 {
        match self {
            Self::NoBreakOnly => 0.0,
            Self::All { base_font_size_px } => base_font_size_px,
        }
    }

    /// What to do with a tag of `class` at this scope.
    ///
    /// `<no-break>` and `<br>` are CONSUMED rather than re-applied, deliberately: a form
    /// IS a complete line-break decision, so re-emitting the user's manual break would
    /// fight the form they just picked, and the protected range the no-break tag marks
    /// has already done its work by the time a form exists. `<br>` is consistent with a
    /// literal newline here, which the segmenter already treats as ordinary breaking
    /// whitespace (`ms-text-util`'s `is_breaking_whitespace`).
    ///
    /// Machine `<m …>` tags are re-applied at BOTH scopes: they are stripped at both, and
    /// a stripped tag that is never restored silently destroys the inline bold/colour/
    /// size/font/offset it carried — they are the panel's DEFAULT tag form.
    #[must_use]
    fn action_for(self, class: InlineTagClass) -> TagAction {
        match class {
            InlineTagClass::NoBreakOpen | InlineTagClass::NoBreakClose => TagAction::Consume,
            InlineTagClass::MachineOpen { .. } => TagAction::Reapply { closing: false },
            InlineTagClass::MachineClose => TagAction::Reapply { closing: true },
            InlineTagClass::Break => match self {
                Self::NoBreakOnly => TagAction::Keep,
                Self::All { .. } => TagAction::Consume,
            },
            InlineTagClass::StyleOpen => match self {
                Self::NoBreakOnly => TagAction::Keep,
                Self::All { .. } => TagAction::Reapply { closing: false },
            },
            InlineTagClass::StyleClose => match self {
                Self::NoBreakOnly => TagAction::Keep,
                Self::All { .. } => TagAction::Reapply { closing: true },
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InlineNoBreakRun {
    text: String,
    no_break: bool,
}

/// One removed inline tag, remembered so the applied form can carry it back.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TagAnchor {
    /// Byte offset into the PRODUCED stripped text, i.e. its length at the moment the tag
    /// was met. Recording the produced length rather than an offset into the raw source is
    /// what makes the anchor immune to the NBSP widening: a protected 1-byte space becomes
    /// a 2-byte NBSP, so raw byte offsets would drift while these do not.
    plain_offset: usize,
    /// The tag exactly as the user wrote it, angle brackets included. Re-emitted verbatim:
    /// re-serializing from a parsed style model would rewrite `<b>` as `<m b=1>`, normalize
    /// the user's spelling, and have to invent a nesting for unclosed or stray tags.
    source: String,
    /// Closing tag (`</b>`, `</m>`): at a line break it attaches to the end of the
    /// preceding line, while everything else attaches to the start of the following one.
    closing: bool,
}

/// Everything the single strip pass produces from a raw form source text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StrippedFormText {
    /// Alternating protected / unprotected runs; concatenated they are the stripped text.
    runs: Vec<InlineNoBreakRun>,
    /// Removed tags that must come back onto an applied form, in source order and with
    /// ascending `plain_offset`.
    anchors: Vec<TagAnchor>,
}

/// Removes inline tags from a RAW form source text and makes the whitespace
/// inside no-break ranges non-breaking (NBSP).
///
/// This is the character-level shape of what a form will contain, so it is what a width
/// metric has to measure ([`GlyphWidths::build`] calls it). It is deliberately PRIVATE:
/// the strip must happen exactly once per consumer, on the raw text. Handing an already
/// prepared string to the enumerators would leave them with no tags to find, every run
/// marked breakable, and the protected ranges hyphenated — the defect this privacy
/// prevents from coming back.
#[must_use]
fn prepare_inline_no_break_text(text: &str, scope: InlineTagScope) -> String {
    strip_inline_tags(text, scope)
        .runs
        .into_iter()
        .map(|run| run.text)
        .collect()
}

/// The single strip pass: splits a RAW form source text into protected/unprotected runs
/// and records every tag it removed.
///
/// The vocabulary is not defined here — it comes from `inline_styles::classify_inline_tag_body`,
/// the same parser the renderer uses, so "what is a tag" cannot differ between the text the
/// form is built from and the text the renderer draws. `scope` decides only which of the
/// recognized classes are acted on ([`InlineTagScope::action_for`]).
#[must_use]
fn strip_inline_tags(text: &str, scope: InlineTagScope) -> StrippedFormText {
    let classify_font_size_px = scope.classify_font_size_px();
    let mut runs = Vec::<InlineNoBreakRun>::new();
    let mut anchors = Vec::<TagAnchor>::new();
    let mut plain_len = 0usize;
    let mut no_break_depth = 0usize;
    let mut machine_stack = Vec::<bool>::new();
    let mut cursor = 0usize;

    while cursor < text.len() {
        let Some(ch) = text[cursor..].chars().next() else {
            break;
        };
        if ch == '<'
            && let Some(rel_end) = text[cursor + ch.len_utf8()..].find('>')
        {
            let body_start = cursor + ch.len_utf8();
            let body_end = body_start + rel_end;
            let after_tag = body_end + '>'.len_utf8();
            if let Some(class) =
                classify_inline_tag_body(&text[body_start..body_end], classify_font_size_px)
            {
                let action = scope.action_for(class);
                if action != TagAction::Keep {
                    // The no-break bookkeeping belongs to the classes that are stripped at
                    // EVERY scope, so it can never be skipped by a `Keep`.
                    match class {
                        InlineTagClass::NoBreakOpen => {
                            no_break_depth = no_break_depth.saturating_add(1);
                        }
                        InlineTagClass::NoBreakClose => {
                            no_break_depth = no_break_depth.saturating_sub(1);
                        }
                        InlineTagClass::MachineOpen { protects_no_break } => {
                            if protects_no_break {
                                no_break_depth = no_break_depth.saturating_add(1);
                            }
                            machine_stack.push(protects_no_break);
                        }
                        InlineTagClass::MachineClose => {
                            if machine_stack.pop().unwrap_or(false) {
                                no_break_depth = no_break_depth.saturating_sub(1);
                            }
                        }
                        InlineTagClass::Break
                        | InlineTagClass::StyleOpen
                        | InlineTagClass::StyleClose => {}
                    }
                    if let TagAction::Reapply { closing } = action {
                        anchors.push(TagAnchor {
                            plain_offset: plain_len,
                            source: text[cursor..after_tag].to_string(),
                            closing,
                        });
                    }
                    cursor = after_tag;
                    continue;
                }
            }
        }

        plain_len += push_inline_no_break_text(
            &mut runs,
            &text[cursor..cursor + ch.len_utf8()],
            no_break_depth > 0,
        );
        cursor += ch.len_utf8();
    }

    StrippedFormText { runs, anchors }
}

/// Do two scopes strip `text` to exactly the same thing — same characters, same protected
/// runs, same tag anchors?
///
/// Everything the form engine derives from a (raw text, scope) pair goes through the one
/// strip pass: the break graph ([`search_forms`]), the width alphabet
/// ([`GlyphWidths::build`]) and the markup put back on the applied form
/// ([`reapply_inline_tags_to_form_text`]). Two scopes that strip a text alike are therefore
/// the SAME input for it, and a caller that caches a search by its input may treat them as
/// one — this is an exact statement about the engine, not an approximation of it.
///
/// That caller is the typing panel. [`InlineTagScope::All`] carries `base_font_size_px`
/// because the size decides whether `<offset=…>`/`<stretching=…>` are tags at all, so it
/// genuinely belongs to the input; but for a text whose tag bodies do not depend on it — any
/// text without such a tag, and any realistic size even with one — a font-size change cannot
/// move a single form, and restarting the search on it only throws the user's window filters
/// away.
#[must_use]
pub fn scopes_strip_alike(text: &str, a: InlineTagScope, b: InlineTagScope) -> bool {
    a == b || strip_inline_tags(text, a) == strip_inline_tags(text, b)
}

/// Appends one source character to the run list and returns how many BYTES it added to the
/// stripped text (an NBSP substitution makes that differ from the source character's size).
fn push_inline_no_break_text(
    runs: &mut Vec<InlineNoBreakRun>,
    text: &str,
    no_break: bool,
) -> usize {
    let prepared = if no_break {
        text.chars()
            .map(|ch| {
                if ch.is_whitespace() {
                    NON_BREAKING_SPACE
                } else {
                    ch
                }
            })
            .collect::<String>()
    } else {
        text.to_string()
    };
    if prepared.is_empty() {
        return 0;
    }
    let added = prepared.len();
    if let Some(last) = runs.last_mut()
        && last.no_break == no_break
    {
        last.text.push_str(prepared.as_str());
        return added;
    }
    runs.push(InlineNoBreakRun {
        text: prepared,
        no_break,
    });
    added
}

/// The inline tags of an applied form could not be put back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagReapplyError {
    /// The form text and the stripped source it was built from cannot be aligned:
    /// at `plain_offset` / `form_offset` there is neither a matching character nor any
    /// transformation the walk knows (a normalized separator, a line break, a wrap hyphen
    /// or a consumed soft hyphen).
    ///
    /// Reported instead of resynchronizing on a later match: a silently misplaced
    /// `<font=…>` restyles the wrong words, which is worse than not restyling at all.
    Unalignable {
        /// Byte offset into the stripped source text.
        plain_offset: usize,
        /// Byte offset into the form text.
        form_offset: usize,
    },
}

impl std::fmt::Display for TagReapplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unalignable {
                plain_offset,
                form_offset,
            } => write!(
                f,
                "form text diverges from its source at source byte {plain_offset} / form byte \
                 {form_offset}"
            ),
        }
    }
}

impl std::error::Error for TagReapplyError {}

/// Puts the inline tags of `source_text` back onto `form_text`, verbatim.
///
/// `form_text` must be [`TextForm::to_text`] of a form built from THIS `source_text` at
/// THIS `scope`; the walk aligns the two and inserts each removed tag before the character
/// it originally preceded. `<no-break>` and (at [`InlineTagScope::All`]) `<br>` are not
/// restored — see [`InlineTagScope::action_for`].
///
/// Placement rules, all of them contract:
/// - at a line break a CLOSING tag lands at the end of the preceding line (after a wrap
///   hyphen, so the hyphen stays inside the span it belongs to) and everything else at the
///   start of the following line;
/// - several tags at one source position keep their source order, except that a closing
///   and an opening tag at the same BREAK necessarily go to different lines;
/// - a tag inside a word that gets hyphenated stays attached to the character it preceded;
/// - a tag inside a protected range is an ordinary interior position;
/// - unclosed, nested and stray-closing tags are neither validated nor repaired: the output
///   is the user's own character sequence with line breaks interposed, so the renderer's
///   parser sees exactly what it sees for the un-formed text.
///
/// # Errors
/// [`TagReapplyError::Unalignable`] when the two texts cannot be aligned. The caller is
/// expected to fall back to the untagged `form_text` and say so — never to guess.
pub fn reapply_inline_tags_to_form_text(
    source_text: &str,
    scope: InlineTagScope,
    form_text: &str,
) -> Result<String, TagReapplyError> {
    let stripped = strip_inline_tags(source_text, scope);
    if stripped.anchors.is_empty() {
        return Ok(form_text.to_string());
    }
    let plain: String = stripped.runs.iter().map(|run| run.text.as_str()).collect();
    let mut placements = place_tag_anchors(plain.as_str(), form_text, &stripped.anchors)?;
    // Stable sort: anchors come out of the walk in source order, and only a break can send
    // a later anchor to an earlier offset (an ill-nested `<i></b>` split across two lines).
    placements.sort_by_key(|&(offset, _)| offset);

    let extra: usize = stripped
        .anchors
        .iter()
        .map(|anchor| anchor.source.len())
        .sum();
    let mut out = String::with_capacity(form_text.len() + extra);
    let mut copied = 0usize;
    for (offset, index) in placements {
        out.push_str(&form_text[copied..offset]);
        out.push_str(stripped.anchors[index].source.as_str());
        copied = offset;
    }
    out.push_str(&form_text[copied..]);
    Ok(out)
}

/// Breaking whitespace, exactly as the segmenter defines it
/// (`ms-text-util/src/segmentation/base.rs`, `is_breaking_whitespace`): everything
/// `char::is_whitespace` accepts except NBSP, which a protected range relies on.
#[must_use]
fn is_form_breaking_whitespace(ch: char) -> bool {
    ch.is_whitespace() && ch != NON_BREAKING_SPACE
}

/// End of the (possibly empty) run of breaking whitespace starting at `from`.
#[must_use]
fn breaking_whitespace_run_end(text: &str, from: usize) -> usize {
    let mut end = from;
    for ch in text[from..].chars() {
        if !is_form_breaking_whitespace(ch) {
            break;
        }
        end += ch.len_utf8();
    }
    end
}

/// Is the form text about to close a line here, and where do the two sides of that break
/// begin?
///
/// Returns `(offset for a closing tag, offset for everything else)`. An optional wrap
/// hyphen is skipped first, so a closing tag lands AFTER it — the hyphen belongs to the
/// word the span covers.
#[must_use]
fn peek_form_line_break(
    plain: &str,
    src: usize,
    form_text: &str,
    tgt: usize,
) -> Option<(usize, usize)> {
    let mut at = tgt;
    if form_text[at..].starts_with('-') && is_form_wrap_hyphen(plain, src, form_text, at) {
        at += '-'.len_utf8();
    }
    form_text[at..]
        .starts_with('\n')
        .then(|| (at, at + '\n'.len_utf8()))
}

/// Is the `'-'` at `tgt` a hyphen the wrap inserted rather than one the source carries?
///
/// Unambiguous against a real hard hyphen, which the source has at the same position and
/// which therefore matches before this is ever consulted.
///
/// A wrap hyphen is always followed by the `'\n'` that separates it from the rest of the
/// source: `build_line_text_and_units` appends `Joint::wrap_suffix` only to a line that
/// WRAPS (`ms-text-util/src/segmentation/base.rs`), so the last line of a form never ends
/// on one. End of text is therefore NOT a legal context — accepting it let an alleged form
/// append a visible character to the text and still be re-tagged.
#[must_use]
fn is_form_wrap_hyphen(plain: &str, src: usize, form_text: &str, tgt: usize) -> bool {
    !plain[src..].starts_with('-') && form_text[tgt + '-'.len_utf8()..].starts_with('\n')
}

/// The whitespace transformations a form can actually apply to its source text, as the
/// two-cursor walk of [`place_tag_anchors`] sees them.
///
/// This set IS the "refuse rather than guess" contract of
/// [`reapply_inline_tags_to_form_text`]: anything outside it means the form text does not
/// come from this source, and a tag placed by a walk that resynchronized on it would
/// restyle words the user never marked. Each variant is a documented consequence of one
/// `Joint` kind of `ms-text-util`'s segmenter, so the set can be checked against the code
/// that produces it rather than against a description of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormWhitespaceStep {
    /// Equal character counts on both sides: the separator survived the join.
    /// `Joint::space` carries `" ".repeat(n)` where `n` is the character count of the
    /// source run, so a source `'\t'`/`'\n'` arrives as `' '` — same count, different
    /// character — and whitespace the segmenter kept LITERAL inside a block (the
    /// whitespace around a standalone dash token) arrives unchanged.
    Normalised { src_char: char, tgt_char: char },
    /// A whole source run replaced by the single `'\n'` of a break: `Joint::space`'s
    /// `wrap_suffix` is empty, so the separator vanishes and the two lines are joined by
    /// `'\n'`.
    CollapsedIntoBreak,
    /// A break at a junction the source has NO whitespace at. Only a hyphen junction can
    /// be one: `Joint::soft_hyphen` appends the wrap hyphen the walk has just consumed and
    /// `Joint::hard_hyphen` leaves the source's own dash at the end of the head, so the
    /// character before the `'\n'` is always a dash. `Joint::glue` — the only other
    /// no-separator junction — belongs to the last block alone and is never a break.
    BreakAfterDash,
    /// Leading or trailing whitespace of the WHOLE text, which the segmenter drops:
    /// `segment_form_blocks` passes `preserve_edge_spaces: false`, and a trailing
    /// separator is never emitted because the last line does not wrap. There is no
    /// interior trim — every non-final segment ends on whitespace and keeps it as a
    /// `Joint::space`.
    TrimmedTextEdge,
}

/// Classifies the whitespace boundary at (`src`, `tgt`), or `None` when no form of this
/// source could have produced it.
///
/// `src_run_end` / `tgt_run_end` are the ends of the (possibly empty) breaking-whitespace
/// runs starting at the two cursors.
#[must_use]
fn classify_form_whitespace(
    plain: &str,
    src: usize,
    src_run_end: usize,
    form_text: &str,
    tgt: usize,
    tgt_run_end: usize,
) -> Option<FormWhitespaceStep> {
    let src_run = &plain[src..src_run_end];
    let tgt_run = &form_text[tgt..tgt_run_end];
    if !src_run.is_empty()
        && src_run.chars().count() == tgt_run.chars().count()
        && let (Some(src_char), Some(tgt_char)) = (src_run.chars().next(), tgt_run.chars().next())
    {
        return Some(FormWhitespaceStep::Normalised { src_char, tgt_char });
    }
    if tgt_run == "\n" {
        if !src_run.is_empty() {
            return Some(FormWhitespaceStep::CollapsedIntoBreak);
        }
        return form_text[..tgt]
            .chars()
            .next_back()
            .is_some_and(is_line_end_dash_char)
            .then_some(FormWhitespaceStep::BreakAfterDash);
    }
    let at_text_start = src == 0 && tgt == 0;
    let at_text_end = src_run_end == plain.len() && tgt == form_text.len();
    (tgt_run.is_empty() && !src_run.is_empty() && (at_text_start || at_text_end))
        .then_some(FormWhitespaceStep::TrimmedTextEdge)
}

/// Aligns the stripped source with the form text and returns `(form byte offset, anchor
/// index)` for every anchor, in anchor order.
///
/// Two monotone cursors, one target character consumed per step; the walk never searches,
/// so it cannot lock onto a later occurrence of a repeated word. Every step is one of: a
/// literal match, a whitespace boundary the form engine can actually produce
/// ([`FormWhitespaceStep`] — that enum, not this list, is the authority), a wrap hyphen the
/// form added ([`is_form_wrap_hyphen`]), or a soft hyphen the segmenter consumed. Anything
/// else is [`TagReapplyError::Unalignable`].
fn place_tag_anchors(
    plain: &str,
    form_text: &str,
    anchors: &[TagAnchor],
) -> Result<Vec<(usize, usize)>, TagReapplyError> {
    let mut placements = Vec::with_capacity(anchors.len());
    let mut src = 0usize;
    let mut tgt = 0usize;
    let mut next = 0usize;

    loop {
        // Anchors the source cursor has reached. `<=` and not `==`: a collapsing whitespace
        // run moves the cursor by more than one character, and its interior anchors are
        // placed by that step itself.
        while next < anchors.len() && anchors[next].plain_offset <= src {
            let offset = match peek_form_line_break(plain, src, form_text, tgt) {
                Some((close_at, open_at)) => {
                    if anchors[next].closing {
                        close_at
                    } else {
                        open_at
                    }
                }
                None => tgt,
            };
            placements.push((offset, next));
            next += 1;
        }
        if src >= plain.len() && tgt >= form_text.len() {
            break;
        }

        let src_ch = plain[src..].chars().next();
        let tgt_ch = form_text[tgt..].chars().next();

        // A soft hyphen the user typed: the segmenter cuts at one and drops it, so it is in
        // the source and never in the form.
        if src_ch == Some(SOFT_HYPHEN) && tgt_ch != Some(SOFT_HYPHEN) {
            src += SOFT_HYPHEN.len_utf8();
            continue;
        }

        if src_ch.is_some_and(is_form_breaking_whitespace)
            || tgt_ch.is_some_and(is_form_breaking_whitespace)
        {
            let src_run_end = breaking_whitespace_run_end(plain, src);
            let tgt_run_end = breaking_whitespace_run_end(form_text, tgt);
            let step =
                classify_form_whitespace(plain, src, src_run_end, form_text, tgt, tgt_run_end)
                    .ok_or(TagReapplyError::Unalignable {
                        plain_offset: src,
                        form_offset: tgt,
                    })?;
            match step {
                // Consume one pair at a time so interior anchors are still seen by the
                // loop head.
                FormWhitespaceStep::Normalised { src_char, tgt_char } => {
                    src += src_char.len_utf8();
                    tgt += tgt_char.len_utf8();
                }
                // The whole run is replaced at once — by a break, or by nothing at a
                // trimmed edge. Its interior anchors take the two sides of the break.
                FormWhitespaceStep::CollapsedIntoBreak
                | FormWhitespaceStep::BreakAfterDash
                | FormWhitespaceStep::TrimmedTextEdge => {
                    let close_at = tgt;
                    let open_at = tgt_run_end;
                    while next < anchors.len() && anchors[next].plain_offset < src_run_end {
                        let offset = if anchors[next].closing {
                            close_at
                        } else {
                            open_at
                        };
                        placements.push((offset, next));
                        next += 1;
                    }
                    src = src_run_end;
                    tgt = tgt_run_end;
                }
            }
            continue;
        }

        if let (Some(s), Some(t)) = (src_ch, tgt_ch)
            && s == t
        {
            src += s.len_utf8();
            tgt += t.len_utf8();
            continue;
        }

        if tgt_ch == Some('-') && is_form_wrap_hyphen(plain, src, form_text, tgt) {
            tgt += '-'.len_utf8();
            continue;
        }

        return Err(TagReapplyError::Unalignable {
            plain_offset: src,
            form_offset: tgt,
        });
    }

    Ok(placements)
}

/// Делит строку на ведущую висящую пунктуацию, «ядро» и хвостовую висящую
/// пунктуацию. Мягкие переносы (`SOFT_HYPHEN`) выбрасываются полностью.
#[must_use]
pub fn split_hanging_edges(line: &str) -> (String, String, String) {
    let chars: Vec<char> = line
        .trim()
        .chars()
        .filter(|&ch| ch != SOFT_HYPHEN)
        .collect();
    let mut start = 0;
    while start < chars.len() && is_hanging_punctuation(chars[start]) {
        start += 1;
    }
    let mut end = chars.len();
    while end > start && is_hanging_punctuation(chars[end - 1]) {
        end -= 1;
    }
    let lead: String = chars[..start].iter().collect();
    let core: String = chars[start..end].iter().collect();
    let trail: String = chars[end..].iter().collect();
    (lead, core, trail)
}

// --- Предикаты форм (по последовательности ширин строк) -------------------

/// Сравнение двух ширин с допуском: разница `≤ tol` считается равенством.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WidthCmp {
    Less,
    Equal,
    Greater,
}

#[must_use]
fn width_cmp(a: u32, b: u32, tol: u32) -> WidthCmp {
    if a > b && a - b > tol {
        WidthCmp::Greater
    } else if b > a && b - a > tol {
        WidthCmp::Less
    } else {
        WidthCmp::Equal
    }
}

/// Унимодальная «гора»: неубывающая, затем невозрастающая (монотонные тоже).
/// Долина (спад, затем подъём) горой не является — это «ёлка».
#[must_use]
fn is_mountain(widths: &[u32], tol: u32) -> bool {
    let mut descending = false;
    for pair in widths.windows(2) {
        match width_cmp(pair[1], pair[0], tol) {
            WidthCmp::Greater => {
                if descending {
                    return false;
                }
            }
            WidthCmp::Less => descending = true,
            WidthCmp::Equal => {}
        }
    }
    true
}

/// «Ёлка» — любая последовательность ширин, которая не является горой
/// (то есть имеет внутреннюю долину).
#[must_use]
pub fn is_christmas_tree(widths: &[u32], tol: u32) -> bool {
    !is_mountain(widths, tol)
}

/// Линза «(  )»: гора, у которой пик строго внутри — есть и подъём, и спуск.
#[must_use]
fn is_lens(widths: &[u32], tol: u32) -> bool {
    if !is_mountain(widths, tol) {
        return false;
    }
    let mut ascended = false;
    let mut descended = false;
    for pair in widths.windows(2) {
        match width_cmp(pair[1], pair[0], tol) {
            WidthCmp::Greater => ascended = true,
            WidthCmp::Less => descended = true,
            WidthCmp::Equal => {}
        }
    }
    ascended && descended
}

/// Соответствует ли последовательность ширин выбранной форме.
#[must_use]
pub fn sequence_matches(widths: &[u32], preset: TextFormPreset, tol: u32) -> bool {
    match preset {
        TextFormPreset::Widen => widths
            .windows(2)
            .all(|pair| width_cmp(pair[1], pair[0], tol) != WidthCmp::Less),
        TextFormPreset::Narrow => widths
            .windows(2)
            .all(|pair| width_cmp(pair[1], pair[0], tol) != WidthCmp::Greater),
        TextFormPreset::FreeNoTree => is_mountain(widths, tol),
        TextFormPreset::Lens => is_lens(widths, tol),
    }
}

// --- Дерево перебора -------------------------------------------------------

/// Состояние частичной формы во время обхода (для инкрементальной отсечки).
#[derive(Clone, Copy)]
struct PhaseState {
    last_width: Option<u32>,
    descending: bool,
    ascended: bool,
    descended: bool,
}

impl PhaseState {
    const START: Self = Self {
        last_width: None,
        descending: false,
        ascended: false,
        descended: false,
    };
}

enum Step {
    /// Строка валидна, продолжаем с новым состоянием фазы.
    Ok(PhaseState),
    /// Эта длина строки не подходит, но более длинная может — пробуем дальше.
    SkipEnd,
    /// Дальше по этой ветке всё хуже — отсекаем остаток.
    PruneRest,
}

/// Проверяет переход от предыдущей строки к новой шириной `width` (с допуском `tol`).
fn advance_step(preset: TextFormPreset, phase: PhaseState, width: u32, tol: u32) -> Step {
    let Some(last) = phase.last_width else {
        return Step::Ok(PhaseState {
            last_width: Some(width),
            descending: false,
            ascended: false,
            descended: false,
        });
    };
    let cmp = width_cmp(width, last, tol);
    match preset {
        TextFormPreset::Widen => match cmp {
            WidthCmp::Less => Step::SkipEnd,
            WidthCmp::Equal | WidthCmp::Greater => Step::Ok(PhaseState {
                last_width: Some(width),
                descending: false,
                ascended: phase.ascended || cmp == WidthCmp::Greater,
                descended: false,
            }),
        },
        TextFormPreset::Narrow => match cmp {
            WidthCmp::Greater => Step::PruneRest,
            WidthCmp::Equal | WidthCmp::Less => Step::Ok(PhaseState {
                last_width: Some(width),
                descending: false,
                ascended: false,
                descended: phase.descended || cmp == WidthCmp::Less,
            }),
        },
        TextFormPreset::FreeNoTree | TextFormPreset::Lens => match cmp {
            WidthCmp::Greater => {
                if phase.descending {
                    // Подъём после спуска = долина (ёлка) — ветка мертва.
                    Step::PruneRest
                } else {
                    Step::Ok(PhaseState {
                        last_width: Some(width),
                        descending: false,
                        ascended: true,
                        descended: phase.descended,
                    })
                }
            }
            WidthCmp::Equal => Step::Ok(PhaseState {
                last_width: Some(width),
                ..phase
            }),
            WidthCmp::Less => {
                // Строка начинает (или продолжает) спуск.
                if !phase.descending && preset == TextFormPreset::Lens && !phase.ascended {
                    // В линзе нельзя начать спуск, ещё не поднявшись — пробуем шире.
                    Step::SkipEnd
                } else {
                    Step::Ok(PhaseState {
                        last_width: Some(width),
                        descending: true,
                        ascended: phase.ascended,
                        descended: true,
                    })
                }
            }
        },
    }
}

struct EnumContext<'a> {
    blocks: &'a [Block],
    preset: TextFormPreset,
    max_forms: usize,
    metric: &'a dyn LineWidthMetric,
    tol: u32,
    out: Vec<TextForm>,
    seen: HashSet<String>,
    nodes: u64,
    memory: MemoryProbe,
    truncated: bool,
}

/// Segmenter input built from a raw form source text, plus the byte ranges of that
/// input the user protected with an inline no-break tag.
///
/// `protected` ranges are ascending and never overlap or touch: adjacent runs with the
/// same flag are merged by [`push_inline_no_break_text`], so every range is maximal.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedFormText {
    /// Tag-free text: protected ranges carry NBSP instead of their spaces, the rest is
    /// marked up with soft hyphens.
    text: String,
    /// Ranges of `text` that must end up inside a single block.
    protected: Vec<Range<usize>>,
}

/// Снимает инлайновые теги с СЫРОГО текста формы и размечает мягкими переносами
/// всё, кроме защищённых участков.
///
/// Это ЕДИНСТВЕННОЕ место, где перебор форм снимает теги: подавать сюда уже
/// очищенный текст нельзя — тегов в нём нет, все участки считаются обычными, и
/// защищённый диапазон получает словарные переносы (ровно тот дефект, из-за
/// которого «Не разрывать» не работало в окне «Продвинутая форма текста»).
///
/// Marking up the unprotected runs one by one is safe because dictionary
/// hyphenation is local to a word: a run hyphenates exactly as it would inside the
/// whole text, so a no-break tag never changes how the text OUTSIDE it breaks.
#[must_use]
fn prepare_form_text(
    seg: &dyn ms_text_util::segmentation::Segmenter,
    raw: &str,
    scope: InlineTagScope,
) -> PreparedFormText {
    let runs = strip_inline_tags(raw, scope).runs;
    // Capacity hint only (the string still grows if needed): the bytes freed by the
    // dropped tags roughly offset the soft hyphens the markup inserts.
    let mut text = String::with_capacity(raw.len() + raw.len() / 8);
    let mut protected = Vec::new();
    for run in runs {
        let start = text.len();
        if run.no_break {
            // Защищённый участок НЕ размечаем мягкими переносами: словарный
            // перенос внутри него — ровно тот разрыв, который запрещён.
            text.push_str(run.text.as_str());
            protected.push(start..text.len());
        } else {
            text.push_str(seg.soft_hyphenate_overlong(run.text.as_str()).as_str());
        }
    }
    PreparedFormText { text, protected }
}

/// Byte ranges the segmenter's blocks occupy in the text it segmented.
///
/// Relies on one property of the segmenter (`ms-text-util/src/segmentation/base.rs`):
/// block texts are ordered, non-overlapping LITERAL substrings of its input, and the
/// gaps between them hold nothing but breaking whitespace (moved into `Joint::same_line`)
/// and the soft hyphens it consumed. `tokenize_paragraph` keeps every character,
/// `build_segments` concatenates tokens verbatim, `segment` trims only a segment's
/// trailing breaking whitespace, and `split_segment_into_parts` cuts at `SOFT_HYPHEN`
/// (dropped) or right after a hard hyphen (kept in the head).
///
/// Returns `None` when that property does not hold — the gap between two blocks carries
/// something else, or a block text is not found at all. The mapping would then be shifted
/// and the caller must decline to use it instead of gluing the wrong junction.
#[must_use]
fn block_spans(blocks: &[Block], text: &str) -> Option<Vec<Range<usize>>> {
    let mut spans = Vec::with_capacity(blocks.len());
    let mut cursor = 0usize;
    for block in blocks {
        let start = cursor + text[cursor..].find(block.text.as_str())?;
        let gap_is_segmenter_debris = text[cursor..start]
            .chars()
            .all(|ch| ch == SOFT_HYPHEN || (ch.is_whitespace() && ch != NON_BREAKING_SPACE));
        if !gap_is_segmenter_debris {
            return None;
        }
        let end = start + block.text.len();
        spans.push(start..end);
        cursor = end;
    }
    Some(spans)
}

/// Стык `head_end .. tail_start` лежит СТРОГО внутри защищённого диапазона?
///
/// Стык на самой границе диапазона защищённым не считается: сам защищённый текст
/// при таком разрыве остаётся целым.
#[must_use]
fn junction_is_protected(head_end: usize, tail_start: usize, protected: &[Range<usize>]) -> bool {
    protected
        .iter()
        .any(|range| range.start < head_end && tail_start < range.end)
}

/// Склеивает соседние блоки, стык между которыми попал внутрь защищённого
/// диапазона, в один неразрывный блок.
///
/// Пробел внутри диапазона уже не разрыв (стал NBSP), словарных переносов там нет
/// ([`prepare_form_text`]) — остаётся УЖЕ СУЩЕСТВУЮЩИЙ дефис: сегментатор режет по
/// нему всегда (`allow_hard_hyphen_breaks: true`, это нужно остальному тексту), и
/// без склейки `<no-break>что-то важное</no-break>` рвался по дефису.
///
/// Склейка повторяет [`build_line_text_and_units`]: между блоками вставляется
/// `same_line`-склейка головного стыка, юниты складываются вместе с ней, а стык
/// склеенного блока — стык хвостового.
#[must_use]
fn glue_protected_junctions(blocks: Vec<Block>, prepared: &PreparedFormText) -> Vec<Block> {
    if prepared.protected.is_empty() || blocks.len() < 2 {
        return blocks;
    }
    let Some(spans) = block_spans(blocks.as_slice(), prepared.text.as_str()) else {
        // Не должно случаться: контракт сегментатора описан на `block_spans`. Если
        // он изменится, лучше потерять защиту дефиса (поведение до этой правки),
        // чем склеить произвольные блоки по сдвинутой разметке.
        ms_log::runtime_log::log_warn(
            "[wrap/forms] segmenter blocks could not be located in the text they were built \
             from; inline no-break ranges keep their hard-hyphen break points this time",
        );
        return blocks;
    };

    let mut out: Vec<Block> = Vec::with_capacity(blocks.len());
    for (idx, block) in blocks.into_iter().enumerate() {
        let protected_junction = idx > 0
            && junction_is_protected(spans[idx - 1].end, spans[idx].start, &prepared.protected);
        if protected_junction
            && let Some(head) = out.last_mut()
        {
            let glue = head.joint.same_line.to_string();
            head.text.push_str(glue.as_str());
            head.text.push_str(block.text.as_str());
            head.unit_count = head
                .unit_count
                .saturating_add(glue.chars().count())
                .saturating_add(block.unit_count);
            head.joint = block.joint;
            continue;
        }
        out.push(block);
    }
    out
}

/// Делит СЫРОЙ (с инлайновыми тегами) `text` на блоки перебора форм.
///
/// Инлайновые теги снимаются здесь ([`prepare_form_text`], область — `scope`), пробелы
/// защищённых диапазонов становятся NBSP, словарные переносы внутрь этих диапазонов не
/// ставятся, а после сегментации стыки внутри них склеиваются
/// ([`glue_protected_junctions`]) — защищённый диапазон гарантированно оказывается
/// в одном блоке и не рвётся ни по пробелу, ни по словарному, ни по существующему
/// дефису. Остальной текст сегментатор режет в режиме [`BindingMode::Annotate`]:
/// граф строится один раз, а служебные связи помечаются категорией
/// консервативности на стыке.
#[must_use]
fn segment_form_blocks(
    seg: &dyn ms_text_util::segmentation::Segmenter,
    text: &str,
    scope: InlineTagScope,
) -> Vec<Block> {
    let prepared = prepare_form_text(seg, text, scope);
    let blocks = seg.segment(
        prepared.text.as_str(),
        SegmentOptions {
            hanging_punctuation: false,
            preserve_edge_spaces: false,
            allow_hard_hyphen_breaks: true,
            // Строим граф один раз: служебные слова не склеиваем, а помечаем
            // стык категорией консервативности — фильтрация форм потом.
            binding: BindingMode::Annotate,
        },
    );
    glue_protected_junctions(blocks, &prepared)
}

/// Перечисляет за один прогон все формы `text`, удовлетворяющие `preset`.
/// Повторов нет: каждая комбинация разрывов даёт уникальный текст. Ширины строк
/// берутся из `metric`.
///
/// `text` — СЫРОЙ текст с инлайновыми тегами: снимает их (в области `scope`) и
/// оставляет защищённые диапазоны неразрывными [`segment_form_blocks`]. Строки формы
/// уже без снятых тегов; вернуть их на применённую форму — дело
/// [`reapply_inline_tags_to_form_text`]. `scope` обязан совпадать с тем, что получила
/// метрика ([`GlyphWidths::build`]).
///
/// Это ИСХОДНЫЙ исчерпывающий путь (без ранжирования): порядок форм — порядок
/// обхода дерева, `quality_milli` не считается ([`UNSCORED_QUALITY_MILLI`]),
/// `aspect_milli` неизвестен (`0`). Ранжированный поиск — [`search_forms`].
#[must_use]
pub fn enumerate_forms(
    text: &str,
    preset: TextFormPreset,
    max_forms: usize,
    metric: &dyn LineWidthMetric,
    scope: InlineTagScope,
) -> FormEnumeration {
    if max_forms == 0 || text.split_whitespace().next().is_none() {
        return FormEnumeration {
            forms: Vec::new(),
            truncated: false,
            nodes_visited: 0,
        };
    }

    with_default_segmenter(|seg| {
        let blocks = segment_form_blocks(seg, text, scope);
        if blocks.is_empty() {
            return FormEnumeration {
                forms: Vec::new(),
                truncated: false,
                nodes_visited: 0,
            };
        }
        let mut ctx = EnumContext {
            blocks: blocks.as_slice(),
            preset,
            max_forms,
            metric,
            tol: metric.tolerance(),
            out: Vec::new(),
            seen: HashSet::new(),
            nodes: 0,
            memory: MemoryProbe::new(),
            truncated: false,
        };
        let mut trail = LineTrail::new();
        enumerate_dfs(
            &mut ctx,
            0,
            PhaseState::START,
            0,
            0,
            Conservatism::Safe,
            0,
            u32::MAX,
            &mut trail,
        );
        FormEnumeration {
            forms: ctx.out,
            truncated: ctx.truncated,
            nodes_visited: ctx.nodes,
        }
    })
}

/// Ограничитель перебора: свободная память плюс аварийный потолок узлов.
///
/// Два независимых условия:
/// - **Память**: не чаще одного раза на `MEMORY_CHECK_INTERVAL_NODES` узлов
///   запрашиваем свободную память; если её удалось измерить и она ниже
///   `MIN_AVAILABLE_MEMORY_BYTES` — останавливаемся (на Linux это и есть
///   практический ограничитель: «перечисляем, пока есть память»).
/// - **Аварийный потолок узлов** (`SAFETY_NODE_CEILING`): гарантия завершения,
///   когда память измерить нельзя (`None`, напр. не-Linux) или она никогда не
///   падает ниже порога.
///
/// Отдельный счётчик `next_check_at` (а не `nodes % N == 0`) нужен потому, что
/// проверку вызывают после каждого возврата из ребёнка, а счётчик узлов внутри
/// цикла `for end` не растёт: с проверкой по остатку один «узел из 8192» заново
/// читал и парсил `/proc/meminfo` до `n` раз подряд.
///
/// Сработавшая защита по памяти «залипает» (`exhausted`): раскрутка стека обязана
/// продолжать останавливаться, а не возобновлять перебор до следующей проверки.
#[derive(Debug)]
struct MemoryProbe {
    /// Номер узла, начиная с которого разрешён следующий замер памяти.
    next_check_at: u64,
    /// Замер уже показал нехватку памяти — все дальнейшие ответы `true`.
    exhausted: bool,
}

impl MemoryProbe {
    /// Свежий ограничитель: первый замер — на узле `MEMORY_CHECK_INTERVAL_NODES`.
    const fn new() -> Self {
        Self {
            next_check_at: MEMORY_CHECK_INTERVAL_NODES,
            exhausted: false,
        }
    }

    /// Нужно ли прервать перебор, находясь на узле номер `nodes`.
    fn should_stop(&mut self, nodes: u64) -> bool {
        if self.exhausted {
            return true;
        }
        if nodes >= self.next_check_at {
            self.next_check_at = nodes.saturating_add(MEMORY_CHECK_INTERVAL_NODES);
            if let Some(bytes) = current_available_memory()
                && bytes < MIN_AVAILABLE_MEMORY_BYTES
            {
                self.exhausted = true;
                return true;
            }
        }
        nodes > SAFETY_NODE_CEILING
    }
}

/// Накопитель текущей ветки перебора: строки формы и их ширины, в одном порядке
/// и всегда одной длины. Ширина строки уже посчитана на узле DFS, поэтому
/// `finalize` не меряет строки заново.
struct LineTrail {
    lines: Vec<String>,
    widths: Vec<u32>,
}

impl LineTrail {
    const fn new() -> Self {
        Self {
            lines: Vec::new(),
            widths: Vec::new(),
        }
    }

    /// Добавляет строку с уже известной шириной.
    fn push(&mut self, line: String, width: u32) {
        self.lines.push(line);
        self.widths.push(width);
    }

    /// Снимает последнюю строку вместе с её шириной.
    fn pop(&mut self) {
        self.lines.pop();
        self.widths.pop();
    }
}

#[allow(clippy::too_many_arguments)]
fn enumerate_dfs(
    ctx: &mut EnumContext<'_>,
    start: usize,
    phase: PhaseState,
    cost_acc: u32,
    break_count: usize,
    cons_acc: Conservatism,
    max_width: u32,
    min_width: u32,
    trail: &mut LineTrail,
) {
    if ctx.out.len() >= ctx.max_forms {
        ctx.truncated = true;
        return;
    }
    ctx.nodes += 1;
    if ctx.memory.should_stop(ctx.nodes) {
        ctx.truncated = true;
        return;
    }

    let n = ctx.blocks.len();
    for end in (start + 1)..=n {
        let wraps_here = end < n;
        let (line_text, _) = build_line_text_and_units(&ctx.blocks[start..end], wraps_here);
        let width = ctx.metric.line_width(&line_text);
        match advance_step(ctx.preset, phase, width, ctx.tol) {
            Step::PruneRest => break,
            Step::SkipEnd => continue,
            Step::Ok(next_phase) => {
                let new_max = max_width.max(width);
                let new_min = min_width.min(width);
                trail.push(line_text, width);
                if end == n {
                    finalize(
                        ctx, next_phase, cost_acc, break_count, cons_acc, new_max, new_min, trail,
                    );
                } else {
                    let joint = &ctx.blocks[end - 1].joint;
                    enumerate_dfs(
                        ctx,
                        end,
                        next_phase,
                        cost_acc + joint.break_cost,
                        break_count + usize::from(joint.word_break),
                        cons_acc.max(joint.conservatism),
                        new_max,
                        new_min,
                        trail,
                    );
                }
                trail.pop();
                if ctx.out.len() >= ctx.max_forms || ctx.memory.should_stop(ctx.nodes) {
                    ctx.truncated = true;
                    return;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finalize(
    ctx: &mut EnumContext<'_>,
    phase: PhaseState,
    cost_acc: u32,
    break_count: usize,
    cons_acc: Conservatism,
    max_width: u32,
    min_width: u32,
    trail: &LineTrail,
) {
    if ctx.preset == TextFormPreset::Lens && !(phase.ascended && phase.descended) {
        return;
    }
    let key = trail.lines.join("\n");
    if ctx.seen.insert(key) {
        // Ширины уже посчитаны на узлах DFS — строки не меряются второй раз.
        let widths = trail.widths.as_slice();
        let median_width = median_of_widths(widths);
        ctx.out.push(TextForm {
            lines: trail.lines.clone(),
            word_break_count: break_count,
            max_width,
            min_width: if min_width == u32::MAX { 0 } else { min_width },
            median_width,
            unevenness_pct: unevenness_pct_of_widths(widths, median_width),
            break_cost: cost_acc,
            conservatism: cons_acc,
            line_widths: widths.to_vec(),
            // Исчерпывающий путь не ранжирует формы: качество не считается, а
            // пропорция неизвестна без высоты строки (см. `search_forms`).
            quality_milli: UNSCORED_QUALITY_MILLI,
            roughness_pct: roughness_pct_of_widths(widths, &QualityWeights::DEFAULT),
            aspect_milli: 0,
        });
    }
}

// --- Ранжированный поиск форм (слои A/B, план §2.1/§2.2/§2.4) ---------------

/// Одна ступень «лестницы» коридора ширин (план §2.4, шаг 5).
///
/// Все границы — доли идеальной ширины строки корзины
/// `T_L = ширина_всего_текста_в_одну_строку / L`: `1.0` — ровно идеальная ширина.
/// Ступень применяется целиком; лестница проходится ТОЛЬКО для корзины, которая
/// оказалась ПУСТОЙ, и никогда глобально — иначе богатая корзина была бы
/// разбавлена послаблением, которое понадобилось совсем другой высоте.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CorridorLevel {
    /// Нижняя граница внутренних строк (не первой и не последней), доля `T_L`.
    /// Разумный диапазон `0.0..=1.0`.
    pub interior_lo: f32,
    /// Верхняя граница ЛЮБОЙ строки корзины, доля `T_L`. Разумный диапазон `>= 1.0`.
    pub interior_hi: f32,
    /// Нижняя граница первой строки, доля `T_L`. Намеренно слабее `interior_lo`:
    /// короткая первая строка — нормальный типографский приём.
    pub head_lo: f32,
    /// Нижняя граница последней строки, доля `T_L`. Слабее всех: хвост абзаца —
    /// единственная строка, которой позволено быть огрызком.
    pub tail_lo: f32,
}

impl CorridorLevel {
    /// Строгая (первая) ступень лестницы: `[0.72, 1.32]`, голова `0.34`, хвост `0.30`.
    pub const STRICT: Self = Self {
        interior_lo: 0.72,
        interior_hi: 1.32,
        head_lo: 0.34,
        tail_lo: 0.30,
    };
    /// Первое послабление: `[0.60, 1.45]`, голова `0.24`, хвост `0.20`.
    pub const RELAXED: Self = Self {
        interior_lo: 0.60,
        interior_hi: 1.45,
        head_lo: 0.24,
        tail_lo: 0.20,
    };
    /// Последнее послабление: `[0.45, 1.60]`, голова `0.12`, хвост `0.10`.
    pub const LOOSE: Self = Self {
        interior_lo: 0.45,
        interior_hi: 1.60,
        head_lo: 0.12,
        tail_lo: 0.10,
    };
}

impl Default for CorridorLevel {
    fn default() -> Self {
        Self::STRICT
    }
}

/// Лестница коридоров по умолчанию: строгая ступень, затем два послабления
/// (план §2.4, шаг 5). Порядок значим — берётся первая ступень, давшая формы.
#[must_use]
pub fn default_corridor_ladder() -> Vec<CorridorLevel> {
    vec![
        CorridorLevel::STRICT,
        CorridorLevel::RELAXED,
        CorridorLevel::LOOSE,
    ]
}

/// Бюджет переносов формы (план §2.1): какая доля строк вправе нести дефис.
///
/// Послабление привязано к «люфту» `slack = max_width / min_possible_width`, а НЕ к
/// пропорции формы. Для маленького текста «вертикальная» и «узкая» формы совпадают,
/// для большого — нет: 24 строки по 13 символов очень высоки (пропорция ≈ 0.33), но
/// 13 символов — это масса места, где переносы вовсе не вынуждены. Привязка к
/// пропорции выдала бы большому тексту право переносить 20 строк из 24; привязка к
/// люфту послабляет ровно те формы, где перенос неизбежен, при любом размере текста.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HyphenBudget {
    /// Доля строк с переносом при комфортной ширине (`slack >= slack_hi`).
    pub ratio_strict: f32,
    /// Доля строк с переносом, когда переносы вынуждены (`slack <= slack_lo`).
    pub ratio_relaxed: f32,
    /// Люфт, при котором и ниже действует `ratio_relaxed`.
    pub slack_lo: f32,
    /// Люфт, при котором и выше действует `ratio_strict`. Между `slack_lo` и
    /// `slack_hi` доля интерполируется линейно.
    pub slack_hi: f32,
}

impl HyphenBudget {
    /// Значения плана §2.1: 100 % при люфте ≤ 1.25, 50 % при люфте ≥ 2.0.
    pub const DEFAULT: Self = Self {
        ratio_strict: 0.50,
        ratio_relaxed: 1.00,
        slack_lo: 1.25,
        slack_hi: 2.00,
    };

    /// Допустимая доля строк с переносом при люфте `slack`
    /// (`max_width / min_possible_width`).
    ///
    /// Ниже `slack_lo` — `ratio_relaxed`, выше `slack_hi` — `ratio_strict`, между
    /// ними линейная интерполяция. Нечисловой (`NaN`/бесконечный) люфт трактуется
    /// как «места вдоволь» → `ratio_strict`. Вырожденная настройка
    /// `slack_hi <= slack_lo` также даёт `ratio_strict`.
    #[must_use]
    pub fn allowed_ratio(self, slack: f64) -> f64 {
        let lo = f64::from(self.slack_lo);
        let hi = f64::from(self.slack_hi);
        let strict = f64::from(self.ratio_strict);
        let relaxed = f64::from(self.ratio_relaxed);
        if !slack.is_finite() || slack >= hi || hi <= lo {
            return strict;
        }
        if slack <= lo {
            return relaxed;
        }
        relaxed + (strict - relaxed) * (slack - lo) / (hi - lo)
    }
}

impl Default for HyphenBudget {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Веса и внутренние константы оценки качества `Q` (план §2.2). `Q` намеренно НЕ
/// содержит предпочтения по ширине: ширина — дело порядка показа, а не качества.
/// Все нормированы на медианную ширину самой формы, поэтому `Q` сравним между
/// разными высотами. МЕНЬШЕ — ЛУЧШЕ.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualityWeights {
    /// Вес «шероховатости» — скачков ширины между соседними строками.
    pub rough: f32,
    /// Вес «неравномерности» — среднего отклонения ширин от медианы (ловит
    /// «лесенку», которую мелкие пошаговые скачки прячут).
    pub uneven: f32,
    /// Вес заполненности бюджета переносов (`переносов / разрешено`).
    pub hyphen: f32,
    /// Вес средней цены разрыва (типографское качество каждого переноса).
    pub break_cost: f32,
    /// Вес короткой первой строки.
    pub head: f32,
    /// Вес короткой последней строки.
    pub tail: f32,
    /// Вес доли строк внутри серии подряд идущих переносов.
    pub hyphen_runs: f32,
    /// Множитель ПЕРВОГО и ПОСЛЕДНЕГО перехода в терме `rough`. Уценка не
    /// косметическая: у обеих эталонных панелей плана §2.2 краевая строка
    /// намеренно короткая, и без уценки она оценивалась бы как худший из
    /// возможных скачков.
    pub edge_transition: f32,
    /// Доля МАКСИМАЛЬНОГО скачка в терме `rough`.
    pub rough_max_mix: f32,
    /// Доля СРЕДНЕГО скачка в терме `rough`.
    pub rough_mean_mix: f32,
    /// Порог «короткой первой строки» в долях медианы: штраф растёт от нуля на
    /// пороге до единицы при нулевой ширине.
    pub head_threshold: f32,
    /// Порог «короткой последней строки» в долях медианы.
    pub tail_threshold: f32,
    /// Длина серии подряд идущих строк с переносом, начиная с которой серия
    /// штрафуется термом `runs`.
    pub hyphen_run_len: usize,
    /// Нормировка средней цены разрыва (`Joint::break_cost` максимум 4).
    pub break_cost_norm: f32,
}

impl QualityWeights {
    /// Веса плана §2.2 (настроены на реальном корпусе реплик).
    pub const DEFAULT: Self = Self {
        rough: 1.00,
        uneven: 0.70,
        hyphen: 0.55,
        break_cost: 0.25,
        head: 0.45,
        tail: 0.35,
        hyphen_runs: 0.30,
        edge_transition: 0.45,
        rough_max_mix: 0.6,
        rough_mean_mix: 0.4,
        head_threshold: 0.55,
        tail_threshold: 0.45,
        hyphen_run_len: 3,
        break_cost_norm: 4.0,
    };
}

impl Default for QualityWeights {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Высота строки в единицах метрики по умолчанию: `GlyphWidths` меряет в 1/1000 em,
/// типичный интерлиньяж — 120 % кегля. См. [`FormSearchParams::line_height_units`].
const DEFAULT_LINE_HEIGHT_UNITS: f32 = 1200.0;

/// Настройки ранжированного поиска форм [`search_forms`]. Все числовые решения
/// плана вынесены сюда: в теле алгоритма «магических» констант нет.
///
/// # Санация входа
/// [`search_forms`] прогоняет настройки через [`FormSearchParams::sanitized`] и
/// работает с результатом: каждое ВЕЩЕСТВЕННОЕ поле (включая вложенные
/// [`CorridorLevel`], [`HyphenBudget`], [`QualityWeights`]), оказавшееся `NaN`,
/// бесконечным там, где бесконечность не определена, или вне своей области
/// (отрицательным; для высоты строки — ещё и нулевым), молча заменяется значением
/// по умолчанию. Замена именно молчаливая и детерминированная: крейт GUI-free и
/// вызывается на каждое изображение текста, сообщать об этом некому, а без замены
/// одно `NaN` бесшумно снимает жёсткую гарантию (любое сравнение с `NaN` ложно,
/// поэтому `NaN` в потолке пропорции = «потолка нет», `NaN` в доле переносов =
/// «бюджет не ограничивает»). Целочисленные поля не санируются: они не создают
/// неупорядоченных сравнений, а их вырожденные значения (например `per_bucket: 0`)
/// означают ровно то, что написано в их док-комментариях.
#[derive(Debug, Clone, PartialEq)]
pub struct FormSearchParams {
    /// Потолок пропорции формы `max_width / (число строк × line_height_units)`.
    /// По умолчанию `21/9` — формы шире отбрасываются ещё на этапе перебора.
    /// `f32::INFINITY` снимает потолок.
    pub aspect_max: f32,
    /// Высота одной строки В ЕДИНИЦАХ ТОЙ ЖЕ МЕТРИКИ, что и ширины. Перевод из
    /// пикселей — забота вызывающего, потому что единицы метрики знает только он:
    ///
    /// ```text
    /// spacing%      = effective_spacing_percent(line_spacing_percent, glyph_height_percent)
    /// line_height_px= font_size_px + line_spacing_px + font_size_px * spacing% / 100
    /// units         = units_per_em * line_height_px / font_size_px
    ///                 / (glyph_width_percent / 100)
    /// ```
    ///
    /// (зеркало `pipeline.rs:433-436`). `units_per_em` — 1000 для [`GlyphWidths`] и
    /// ≈2 символа на em для [`CharWidthMetric`]. Горизонтальный масштаб глифов
    /// (`glyph_width_percent`) обязан входить в делитель: ширины меряются без него,
    /// и иначе потолок пропорции молча разъедется с тем, что видит пользователь.
    /// Значение по умолчанию — [`GlyphWidths`] при интерлиньяже 120 %.
    pub line_height_units: f32,
    /// Лестница коридоров ширин, от строгой ступени к слабой (план §2.4, шаг 5).
    /// Пустой список означает «форм нет»: коридор — не фильтр, а условие допуска.
    pub corridor_levels: Vec<CorridorLevel>,
    /// Бюджет переносов (план §2.1).
    pub hyphen: HyphenBudget,
    /// Веса оценки качества (план §2.2).
    pub quality: QualityWeights,
    /// Сколько лучших (по `quality_milli`) форм оставлять в корзине одной высоты.
    /// По умолчанию 14.
    pub per_bucket: usize,
    /// Потолок узлов дерева перебора на ОДНУ попытку корзины. Лестница коридоров
    /// взводит его заново на каждой ступени. По умолчанию 300 000.
    pub node_budget_per_bucket: u64,
    /// Потолок числа форм, накапливаемых в одной корзине до отбора `per_bucket`.
    /// По умолчанию 3000.
    pub form_cap_per_bucket: usize,
    /// Потолок узлов на ВЕСЬ вызов [`search_forms`], включая аварийный прогон
    /// §2.1: тот тратит остаток этого же бюджета, а не получает второй такой же.
    /// По умолчанию 5 000 000. Дедлайна по часам сознательно нет: крейт собирается
    /// в том числе под wasm, где `std::time::Instant` небезопасен, а бюджет по узлам
    /// вдобавок детерминирован и проверяем тестом.
    pub node_budget_total: u64,
    /// Жёсткий диапазон числа строк `[min, max]` включительно. Задан — высоты вне
    /// диапазона НЕ перебираются вовсе (это сокращение перебора, а не фильтр).
    pub line_range: Option<(usize, usize)>,
    /// Жёсткий диапазон `max_width` формы `[min, max]` включительно. Верхняя
    /// граница зажимает верх коридора (перебор обрывается раньше), нижняя
    /// отбрасывает корзины, которые физически не могут её достичь.
    pub width_range: Option<(u32, u32)>,
}

impl Default for FormSearchParams {
    fn default() -> Self {
        Self {
            aspect_max: 21.0 / 9.0,
            line_height_units: DEFAULT_LINE_HEIGHT_UNITS,
            corridor_levels: default_corridor_ladder(),
            hyphen: HyphenBudget::DEFAULT,
            quality: QualityWeights::DEFAULT,
            per_bucket: 14,
            node_budget_per_bucket: 300_000,
            form_cap_per_bucket: 3_000,
            node_budget_total: 5_000_000,
            line_range: None,
            width_range: None,
        }
    }
}

/// Конечное неотрицательное значение или `fallback` (`NaN`, бесконечность и
/// отрицательные — вне области для долей коридора, весов и порогов).
#[must_use]
fn sane_non_negative(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        fallback
    }
}

/// Конечное СТРОГО положительное значение или `fallback` (там, где ноль обнулил
/// бы знаменатель — например высота строки в пропорции формы).
#[must_use]
fn sane_positive(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}

impl FormSearchParams {
    /// Копия настроек, пригодная для арифметики поиска: каждое вещественное поле
    /// вне своей области значений заменено значением по умолчанию (см. раздел
    /// «Санация входа» в описании типа).
    ///
    /// Область значений по полям: `aspect_max` — строго положительное, при этом
    /// `f32::INFINITY` разрешена и означает «потолка нет»; `line_height_units` —
    /// конечное строго положительное; доли коридора, доли/люфты бюджета переносов
    /// и все веса качества — конечные неотрицательные. Целочисленные поля,
    /// `line_range` и `width_range` копируются как есть.
    #[must_use]
    pub fn sanitized(&self) -> Self {
        let defaults = Self::default();
        // Потолок пропорции: бесконечность осмысленна («без потолка»), а `NaN` и
        // неположительное — нет.
        let aspect_max = if self.aspect_max.is_nan() || self.aspect_max <= 0.0 {
            defaults.aspect_max
        } else {
            self.aspect_max
        };
        let strict = CorridorLevel::STRICT;
        let corridor_levels = self
            .corridor_levels
            .iter()
            .map(|level| CorridorLevel {
                interior_lo: sane_non_negative(level.interior_lo, strict.interior_lo),
                interior_hi: sane_non_negative(level.interior_hi, strict.interior_hi),
                head_lo: sane_non_negative(level.head_lo, strict.head_lo),
                tail_lo: sane_non_negative(level.tail_lo, strict.tail_lo),
            })
            .collect();
        let hyphen_default = HyphenBudget::DEFAULT;
        let hyphen = HyphenBudget {
            ratio_strict: sane_non_negative(self.hyphen.ratio_strict, hyphen_default.ratio_strict),
            ratio_relaxed: sane_non_negative(
                self.hyphen.ratio_relaxed,
                hyphen_default.ratio_relaxed,
            ),
            slack_lo: sane_non_negative(self.hyphen.slack_lo, hyphen_default.slack_lo),
            slack_hi: sane_non_negative(self.hyphen.slack_hi, hyphen_default.slack_hi),
        };
        let weights_default = QualityWeights::DEFAULT;
        let quality = QualityWeights {
            rough: sane_non_negative(self.quality.rough, weights_default.rough),
            uneven: sane_non_negative(self.quality.uneven, weights_default.uneven),
            hyphen: sane_non_negative(self.quality.hyphen, weights_default.hyphen),
            break_cost: sane_non_negative(self.quality.break_cost, weights_default.break_cost),
            head: sane_non_negative(self.quality.head, weights_default.head),
            tail: sane_non_negative(self.quality.tail, weights_default.tail),
            hyphen_runs: sane_non_negative(self.quality.hyphen_runs, weights_default.hyphen_runs),
            edge_transition: sane_non_negative(
                self.quality.edge_transition,
                weights_default.edge_transition,
            ),
            rough_max_mix: sane_non_negative(
                self.quality.rough_max_mix,
                weights_default.rough_max_mix,
            ),
            rough_mean_mix: sane_non_negative(
                self.quality.rough_mean_mix,
                weights_default.rough_mean_mix,
            ),
            head_threshold: sane_non_negative(
                self.quality.head_threshold,
                weights_default.head_threshold,
            ),
            tail_threshold: sane_non_negative(
                self.quality.tail_threshold,
                weights_default.tail_threshold,
            ),
            hyphen_run_len: self.quality.hyphen_run_len,
            break_cost_norm: sane_non_negative(
                self.quality.break_cost_norm,
                weights_default.break_cost_norm,
            ),
        };
        Self {
            aspect_max,
            line_height_units: sane_positive(self.line_height_units, defaults.line_height_units),
            corridor_levels,
            hyphen,
            quality,
            per_bucket: self.per_bucket,
            node_budget_per_bucket: self.node_budget_per_bucket,
            form_cap_per_bucket: self.form_cap_per_bucket,
            node_budget_total: self.node_budget_total,
            line_range: self.line_range,
            width_range: self.width_range,
        }
    }
}

/// Приводит счётчик к `f64` без потери точности на реальных размерах текста
/// (счётчики форм/строк/узлов заведомо меньше `u32::MAX`; больше — насыщение).
#[must_use]
fn count_as_f64(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}

/// Округляет неотрицательное вещественное к `u32` с насыщением. `NaN`,
/// бесконечность и отрицательные значения дают `0`/`u32::MAX` соответственно.
#[must_use]
fn round_to_u32(value: f64) -> u32 {
    if value.is_nan() || value <= 0.0 {
        return 0;
    }
    let rounded = value.round();
    if rounded >= f64::from(u32::MAX) {
        return u32::MAX;
    }
    // Проверки выше доказывают `0.0 < rounded < u32::MAX` — приведение точное.
    rounded as u32
}

/// «Шероховатость» профиля ширин (терм `rough` плана §2.2), в долях медианы.
///
/// Для каждого перехода между соседними строками берётся `|Δw| / медиана`;
/// первый и последний переходы умножаются на `weights.edge_transition`. Результат —
/// смесь `rough_max_mix × максимум + rough_mean_mix × среднее`. Одна строка (нет
/// переходов) и нулевая медиана дают `0.0`.
#[must_use]
fn roughness_of_widths(widths: &[u32], weights: &QualityWeights) -> f64 {
    let median = f64::from(median_of_widths(widths));
    if widths.len() < 2 || median <= 0.0 {
        return 0.0;
    }
    let transitions = widths.len() - 1;
    let edge = f64::from(weights.edge_transition);
    let mut max_jump = 0.0f64;
    let mut sum = 0.0f64;
    for (index, pair) in widths.windows(2).enumerate() {
        let jump = f64::from(pair[0].abs_diff(pair[1])) / median;
        // Краевые переходы уценены: короткая первая/последняя строка — приём
        // наборщика, а не «резкий скачок ширины» (план §2.2).
        let weighted = if index == 0 || index + 1 == transitions {
            jump * edge
        } else {
            jump
        };
        max_jump = max_jump.max(weighted);
        sum += weighted;
    }
    let mean = sum / count_as_f64(transitions);
    f64::from(weights.rough_max_mix) * max_jump + f64::from(weights.rough_mean_mix) * mean
}

/// «Шероховатость» в процентах для поля [`TextForm::roughness_pct`].
#[must_use]
fn roughness_pct_of_widths(widths: &[u32], weights: &QualityWeights) -> u32 {
    round_to_u32(roughness_of_widths(widths, weights) * 100.0)
}

/// Доля строк, попавших в серию из `run_len` и более подряд идущих строк с
/// переносом (терм `runs` плана §2.2). `run_len == 0` трактуется как «серий нет».
#[must_use]
fn hyphen_run_share(hyphen_flags: &[bool], run_len: usize) -> f64 {
    if hyphen_flags.is_empty() || run_len == 0 {
        return 0.0;
    }
    let mut in_runs = 0usize;
    let mut current = 0usize;
    for &flag in hyphen_flags {
        if flag {
            current += 1;
        } else {
            if current >= run_len {
                in_runs += current;
            }
            current = 0;
        }
    }
    if current >= run_len {
        in_runs += current;
    }
    count_as_f64(in_runs) / count_as_f64(hyphen_flags.len())
}

/// Разложенная оценка формы: итоговое `Q` и отдельно шероховатость (её панель
/// показывает как самостоятельную характеристику).
#[derive(Debug, Clone, Copy, PartialEq)]
struct QualityBreakdown {
    /// Итоговое `Q` — МЕНЬШЕ ЛУЧШЕ, `0.0` — идеально ровная форма без переносов.
    quality: f64,
    /// Терм `rough` в долях медианы.
    roughness: f64,
}

/// Считает оценку качества `Q` формы (план §2.2, слой B).
///
/// `hyphen_flags[i]` — несёт ли строка `i` дефис переноса (у последней строки это
/// всегда `false`); длина совпадает с `widths`. `break_cost_total` — сумма
/// `Joint::break_cost` по фактическим разрывам, `allowed_hyphens` — бюджет
/// переносов слоя A в строках (может быть дробным). Оценка width-agnostic: все
/// термы нормированы медианной шириной самой формы.
#[must_use]
fn form_quality(
    widths: &[u32],
    hyphen_flags: &[bool],
    break_cost_total: u32,
    allowed_hyphens: f64,
    weights: &QualityWeights,
) -> QualityBreakdown {
    let roughness = roughness_of_widths(widths, weights);
    let median = f64::from(median_of_widths(widths));
    let line_count = widths.len();
    if line_count == 0 {
        return QualityBreakdown {
            quality: 0.0,
            roughness,
        };
    }

    let uneven = unevenness_of_widths(widths, median_of_widths(widths));

    let hyphen_lines = count_as_f64(hyphen_flags.iter().filter(|&&flag| flag).count());
    // Бюджет исчерпан «в ноль» только у одностроч­ных форм, где переносов и быть
    // не может; ненулевой перенос при нулевом бюджете — максимальный штраф.
    let hyphen_term = if allowed_hyphens > 0.0 {
        hyphen_lines / allowed_hyphens
    } else if hyphen_lines > 0.0 {
        1.0
    } else {
        0.0
    };

    let breaks = line_count - 1;
    let norm = f64::from(weights.break_cost_norm);
    let break_term = if breaks == 0 || norm <= 0.0 {
        0.0
    } else {
        f64::from(break_cost_total) / count_as_f64(breaks) / norm
    };

    let edge_term = |width: u32, threshold: f32| -> f64 {
        let threshold = f64::from(threshold);
        if threshold <= 0.0 || median <= 0.0 {
            return 0.0;
        }
        let ratio = f64::from(width) / median;
        (threshold - ratio).max(0.0) / threshold
    };
    let head_term = edge_term(widths[0], weights.head_threshold);
    let tail_term = edge_term(widths[line_count - 1], weights.tail_threshold);
    let runs_term = hyphen_run_share(hyphen_flags, weights.hyphen_run_len);

    let quality = f64::from(weights.rough) * roughness
        + f64::from(weights.uneven) * uneven
        + f64::from(weights.hyphen) * hyphen_term
        + f64::from(weights.break_cost) * break_term
        + f64::from(weights.head) * head_term
        + f64::from(weights.tail) * tail_term
        + f64::from(weights.hyphen_runs) * runs_term;

    QualityBreakdown { quality, roughness }
}

/// Потолок числа ячеек ПЛОТНОЙ memo-таблицы ширин (`n²`, где `n` — число блоков).
/// Ячейка — `Option<u32>` (8 байт), то есть потолок ≈ 8 МБ. С запасом покрывает
/// любой осмысленный текст реплики (самая длинная реплика корпуса — n ≈ 120, то
/// есть 14 400 ячеек) и не даёт вставке на сотню тысяч символов (n ≈ 20 000,
/// `n²` = 4·10⁸ ячеек ≈ 3 ГБ) выделить память ЕЩЁ ДО того, как сработает хоть
/// один бюджет перебора.
const DENSE_WIDTH_MEMO_MAX_CELLS: usize = 1_000_000;

/// Потолок числа записей РАЗРЕЖЁННОЙ memo-таблицы. Достигнут — новые ширины
/// просто не запоминаются: перебор продолжается, лишь снова меряя строки. Memo —
/// чистая оптимизация, поэтому от её отключения результат не меняется, зато
/// расход памяти ограничен сверху при любом размере текста.
const SPARSE_WIDTH_MEMO_MAX_ENTRIES: usize = 1_000_000;

/// Memo ширин строк перебора: `(start, end) → ширина по активной метрике`.
///
/// Текст строки НЕ хранится — он нужен только при выдаче готовой формы
/// (`emit_form`), где собирается заново; хранение строк делало memo главным
/// потребителем памяти. Плотная таблица используется, пока `n²` ячеек
/// укладывается в [`DENSE_WIDTH_MEMO_MAX_CELLS`]; иначе таблица разрежённая, с
/// потолком записей [`SPARSE_WIDTH_MEMO_MAX_ENTRIES`]. Размер считается
/// checked-арифметикой: `n * n` для огромного `n` переполнило бы `usize` (на
/// wasm32 он 32-битный).
#[derive(Debug)]
enum WidthMemo {
    /// Плотная таблица `n × n`: индекс `start * stride + (end - 1)`.
    Dense { widths: Vec<Option<u32>>, stride: usize },
    /// Разрежённая таблица для текстов, которым плотная не по карману.
    Sparse(HashMap<(usize, usize), u32>),
}

impl WidthMemo {
    /// Таблица для текста из `n` блоков (см. описание типа: плотная или
    /// разрежённая — решает [`DENSE_WIDTH_MEMO_MAX_CELLS`]).
    #[must_use]
    fn new(n: usize) -> Self {
        match n.checked_mul(n) {
            Some(cells) if cells <= DENSE_WIDTH_MEMO_MAX_CELLS => Self::Dense {
                widths: vec![None; cells],
                stride: n,
            },
            _ => Self::Sparse(HashMap::new()),
        }
    }

    /// Запомненная ширина строки `[start, end)`; `None` — ещё не мерили (или
    /// запись не поместилась в разрежённую таблицу). `end > start`.
    #[must_use]
    fn get(&self, start: usize, end: usize) -> Option<u32> {
        match self {
            Self::Dense { widths, stride } => widths.get(start * stride + (end - 1)).copied()?,
            Self::Sparse(map) => map.get(&(start, end)).copied(),
        }
    }

    /// Запоминает ширину строки `[start, end)`. Разрежённая таблица, дойдя до
    /// потолка записей, перестаёт запоминать (см. [`SPARSE_WIDTH_MEMO_MAX_ENTRIES`]).
    fn insert(&mut self, start: usize, end: usize, width: u32) {
        match self {
            Self::Dense { widths, stride } => {
                if let Some(cell) = widths.get_mut(start * *stride + (end - 1)) {
                    *cell = Some(width);
                }
            }
            Self::Sparse(map) => {
                if map.len() < SPARSE_WIDTH_MEMO_MAX_ENTRIES {
                    map.insert((start, end), width);
                }
            }
        }
    }
}

/// План перебора одной корзины (одной высоты формы).
#[derive(Debug)]
struct BucketPlan {
    /// Ровно столько строк должно быть у каждой формы корзины.
    target_lines: usize,
    /// Нижняя граница ширины первой строки, в единицах метрики.
    head_lo: f64,
    /// Нижняя граница ширины внутренних строк.
    interior_lo: f64,
    /// Нижняя граница ширины последней строки.
    tail_lo: f64,
    /// Верхняя граница ширины ЛЮБОЙ строки: минимум из верха коридора, потолка
    /// пропорции для этой высоты и верха `width_range`.
    upper: f64,
    /// Запас, за которым превышение `upper` уже необратимо и просмотр всё более
    /// длинных строк можно оборвать: «ширина самого широкого одиночного блока» +
    /// «ширина дефиса переноса» (контракт [`LineWidthMetric`]). Одинаков для всех
    /// корзин поиска; лежит здесь, потому что применяется рядом с `upper`.
    break_slop: f64,
    /// Ширина самого широкого одиночного блока, измеренного как переносимая
    /// строка (знаменатель люфта бюджета переносов).
    min_possible_width: f64,
    /// Высота строки в единицах метрики (для поля `aspect_milli`).
    line_height_units: f64,
}

impl BucketPlan {
    /// Сколько строк корзины вправе нести перенос при текущей максимальной ширине
    /// формы `max_width`. Дробное значение (бюджет слоя A) — целочисленный предел
    /// берётся отбрасыванием дробной части.
    fn allowed_hyphens(&self, budget: HyphenBudget, max_width: u32) -> f64 {
        // Нулевая `min_possible_width` (текст без измеримых блоков) → люфта нет,
        // трактуем как «переносы вынуждены».
        let slack = if self.min_possible_width > 0.0 {
            f64::from(max_width) / self.min_possible_width
        } else {
            0.0
        };
        budget.allowed_ratio(slack) * count_as_f64(self.target_lines)
    }
}

/// Накопленное состояние текущей ветки перебора корзины.
#[derive(Debug)]
struct PartialForm {
    /// Индексы блоков, на которых закрывается каждая уже поставленная строка.
    cuts: Vec<usize>,
    /// Ширины уже поставленных строк, в том же порядке.
    widths: Vec<u32>,
    /// Несёт ли каждая поставленная строка дефис переноса.
    hyphen_flags: Vec<bool>,
    /// Сумма `Joint::break_cost` по уже сделанным разрывам.
    break_cost: u32,
    /// Максимум категорий консервативности по уже сделанным разрывам.
    conservatism: Conservatism,
    max_width: u32,
    /// `u32::MAX`, пока не поставлено ни одной строки.
    min_width: u32,
    /// Сколько поставленных строк несут перенос.
    hyphen_lines: usize,
}

impl PartialForm {
    /// Пустая ветка: строк нет, ширины ещё не наблюдались.
    const fn new() -> Self {
        Self {
            cuts: Vec::new(),
            widths: Vec::new(),
            hyphen_flags: Vec::new(),
            break_cost: 0,
            conservatism: Conservatism::Safe,
            max_width: 0,
            min_width: u32::MAX,
            hyphen_lines: 0,
        }
    }
}

/// Снимок полей `PartialForm`, которые меняются при спуске и должны быть
/// восстановлены при откате ветки.
#[derive(Debug, Clone, Copy)]
struct PartialSnapshot {
    break_cost: u32,
    conservatism: Conservatism,
    max_width: u32,
    min_width: u32,
    hyphen_lines: usize,
}

/// Контекст ранжированного поиска: неизменные входы, memo ширин, счётчики
/// бюджетов и накопитель текущей корзины.
///
/// Контекст живёт на ВЕСЬ вызов [`search_forms`], включая аварийный прогон со
/// снятым потолком пропорции: и бюджет узлов, и защита по памяти, и memo — одни
/// на оба прогона (`node_budget_total` — потолок всего вызова, а не одного
/// прогона).
struct SearchContext<'a> {
    blocks: &'a [Block],
    preset: TextFormPreset,
    metric: &'a dyn LineWidthMetric,
    tol: u32,
    params: &'a FormSearchParams,
    /// Memo ширин строк (см. [`WidthMemo`]). Заполняется лениво: строка не
    /// меряется дважды ни внутри прогона, ни между прогонами.
    widths: WidthMemo,
    nodes_total: u64,
    nodes_bucket: u64,
    memory: MemoryProbe,
    truncated: bool,
    /// Формы текущей корзины до отбора `per_bucket` лучших.
    bucket: Vec<TextForm>,
}

impl<'a> SearchContext<'a> {
    /// Свежий контекст поиска по блокам `blocks`. Счётчики бюджетов обнулены —
    /// на весь вызов [`search_forms`] контекст создаётся ровно один раз.
    fn new(
        blocks: &'a [Block],
        preset: TextFormPreset,
        metric: &'a dyn LineWidthMetric,
        params: &'a FormSearchParams,
    ) -> Self {
        Self {
            blocks,
            preset,
            metric,
            tol: metric.tolerance(),
            params,
            widths: WidthMemo::new(blocks.len()),
            nodes_total: 0,
            nodes_bucket: 0,
            memory: MemoryProbe::new(),
            truncated: false,
            bucket: Vec::new(),
        }
    }

    /// Текст строки `[start, end)` ровно в том виде, в каком её меряет метрика.
    /// Перенос (а с ним и хвостовой дефис) есть у любой строки, кроме доходящей
    /// до конца текста. `end > start` и `end <= blocks.len()`.
    #[must_use]
    fn line_text(&self, start: usize, end: usize) -> String {
        let n = self.blocks.len();
        build_line_text_and_units(&self.blocks[start..end], end < n).0
    }

    /// Ширина строки `[start, end)` по активной метрике (через memo).
    fn ensure_width(&mut self, start: usize, end: usize) -> u32 {
        if let Some(width) = self.widths.get(start, end) {
            return width;
        }
        let width = self.metric.line_width(&self.line_text(start, end));
        self.widths.insert(start, end, width);
        width
    }

    /// Исчерпан ли бюджет текущей корзины или всего поиска. Побочный эффект:
    /// помечает результат `truncated`.
    fn bucket_exhausted(&mut self) -> bool {
        if self.bucket.len() >= self.params.form_cap_per_bucket
            || self.nodes_bucket >= self.params.node_budget_per_bucket
        {
            self.truncated = true;
            return true;
        }
        self.search_exhausted()
    }

    /// Исчерпан ли бюджет всего поиска (узлы или свободная память). Побочный
    /// эффект: помечает результат `truncated`.
    fn search_exhausted(&mut self) -> bool {
        if self.nodes_total >= self.params.node_budget_total {
            self.truncated = true;
            return true;
        }
        if self.memory.should_stop(self.nodes_total) {
            self.truncated = true;
            return true;
        }
        false
    }
}

/// Перебор одной корзины: формы ровно из `plan.target_lines` строк, каждая строка
/// внутри коридора ширин, с инкрементальной отсечкой по форме пресета и бюджету
/// переносов. Найденные формы складываются в `ctx.bucket`.
///
/// Дедупликации нет и она не нужна: тождество формы — её вектор разрезов, а
/// каждый допустимый вектор перебирается РОВНО один раз. Доказательство: путь от
/// корня к листу — это и есть вектор разрезов (на каждом узле выбирается очередной
/// `end`, строго больший предыдущего, и цикл `for end` посещает каждое значение
/// не более одного раза), поэтому разные пути дают разные векторы; лист (`end == n`)
/// достижим только когда `remaining_lines == 1`, значит длина вектора всегда равна
/// `plan.target_lines` и векторы разных корзин не совпадают; повторный проход по
/// ступени лестницы коридора делается ТОЛЬКО для пустой корзины, то есть после
/// нуля выданных форм.
fn search_dfs(
    ctx: &mut SearchContext<'_>,
    plan: &BucketPlan,
    start: usize,
    phase: PhaseState,
    state: &mut PartialForm,
) {
    ctx.nodes_total += 1;
    ctx.nodes_bucket += 1;
    if ctx.bucket_exhausted() {
        return;
    }

    let n = ctx.blocks.len();
    let placed = state.cuts.len();
    let remaining_lines = plan.target_lines - placed;
    // Последняя строка обязана дойти ровно до конца текста; остальным нужно
    // оставить хотя бы по одному блоку на каждую ещё не поставленную строку.
    let (first_end, last_end) = if remaining_lines == 1 {
        (n, n)
    } else {
        // Блоков может не хватить на оставшиеся строки — тогда ветка мертва.
        let Some(last_end) = n.checked_sub(remaining_lines - 1) else {
            return;
        };
        (start + 1, last_end)
    };
    if first_end > last_end {
        return;
    }

    for end in first_end..=last_end {
        let width = ctx.ensure_width(start, end);
        let width_f = f64::from(width);
        if width_f > plan.upper {
            // Верх коридора нарушен (план §2.4, шаг 3). Оборвать ОСТАТОК ветки
            // можно только тогда, когда ни один следующий шаг не вернёт ширину
            // обратно под потолок. Монотонности ширины по `end` метрика не
            // обещает: присоединение следующего блока делает уже посчитанный
            // хвостовой дефис переноса внутренним, и новый блок может оказаться
            // уже этого дефиса.
            //
            // Доказательство границы. Пусть `B(start, end)` — ширина строки без
            // хвостового дефиса переноса; по контракту `LineWidthMetric` она не
            // убывает с ростом `end`. Тогда для любого `end' > end`
            // `width(start, end') >= B(start, end') >= B(start, end) >=
            // width(start, end) - hyphen`, то есть один шаг может вернуть не
            // больше ширины дефиса переноса. `plan.break_slop` берёт этот запас
            // с добавкой ширины самого широкого одиночного блока — оценка
            // заведомо не меньше необходимой, поэтому обрыв за ней допустим
            // (admissible): ни одна форма, которую примет финальная проверка, не
            // теряется.
            if width_f > plan.upper + plan.break_slop {
                break;
            }
            continue;
        }
        let lower = if placed == 0 {
            plan.head_lo
        } else if end == n {
            plan.tail_lo
        } else {
            plan.interior_lo
        };
        if width_f < lower {
            continue;
        }

        // Форма пресета — та же инкрементальная отсечка, что и в исчерпывающем
        // переборе: `PruneRest` убивает остаток ветки, `SkipEnd` пробует шире.
        let next_phase = match advance_step(ctx.preset, phase, width, ctx.tol) {
            Step::PruneRest => break,
            Step::SkipEnd => continue,
            Step::Ok(next_phase) => next_phase,
        };

        let carries_hyphen = end < n && ctx.blocks[end - 1].joint.word_break;
        let new_hyphens = state.hyphen_lines + usize::from(carries_hyphen);
        let new_max = state.max_width.max(width);
        // Отсечка по бюджету переносов точна: вдоль ветки число переносов только
        // растёт, максимальная ширина только растёт, а бюджет с ростом ширины
        // только падает — значит превышение здесь превышением и останется.
        if count_as_f64(new_hyphens) > plan.allowed_hyphens(ctx.params.hyphen, new_max) {
            continue;
        }

        let snapshot = PartialSnapshot {
            break_cost: state.break_cost,
            conservatism: state.conservatism,
            max_width: state.max_width,
            min_width: state.min_width,
            hyphen_lines: state.hyphen_lines,
        };
        state.cuts.push(end);
        state.widths.push(width);
        state.hyphen_flags.push(carries_hyphen);
        state.max_width = new_max;
        state.min_width = state.min_width.min(width);
        state.hyphen_lines = new_hyphens;

        if end == n {
            emit_form(ctx, plan, next_phase, state);
        } else {
            let joint = &ctx.blocks[end - 1].joint;
            state.break_cost = state.break_cost.saturating_add(joint.break_cost);
            state.conservatism = state.conservatism.max(joint.conservatism);
            search_dfs(ctx, plan, end, next_phase, state);
        }

        state.cuts.pop();
        state.widths.pop();
        state.hyphen_flags.pop();
        state.break_cost = snapshot.break_cost;
        state.conservatism = snapshot.conservatism;
        state.max_width = snapshot.max_width;
        state.min_width = snapshot.min_width;
        state.hyphen_lines = snapshot.hyphen_lines;

        if ctx.bucket_exhausted() {
            return;
        }
    }
}

/// Достраивает завершённую ветку до [`TextForm`] и кладёт её в текущую корзину.
/// Отбрасывает формы, нарушающие остаточные жёсткие условия (линза без пика,
/// нижняя граница `width_range`). Повторов на входе не бывает — см. доказательство
/// в [`search_dfs`].
fn emit_form(
    ctx: &mut SearchContext<'_>,
    plan: &BucketPlan,
    phase: PhaseState,
    state: &PartialForm,
) {
    if ctx.preset == TextFormPreset::Lens && !(phase.ascended && phase.descended) {
        return;
    }
    // Нижняя граница `width_range` — свойство всей формы, а не отдельной строки,
    // поэтому проверяется здесь; корзины, которым она недостижима, отсекаются
    // целиком ещё до перебора.
    if let Some((min_width, _)) = ctx.params.width_range
        && state.max_width < min_width
    {
        return;
    }
    // Текст строк собирается только здесь, на готовой форме: memo хранит одни
    // ширины, а число выданных форм ограничено `form_cap_per_bucket`.
    let mut lines = Vec::with_capacity(state.cuts.len());
    let mut start = 0usize;
    for &end in &state.cuts {
        lines.push(ctx.line_text(start, end));
        start = end;
    }

    let widths = state.widths.as_slice();
    let median_width = median_of_widths(widths);
    let allowed = plan.allowed_hyphens(ctx.params.hyphen, state.max_width);
    let scored = form_quality(
        widths,
        &state.hyphen_flags,
        state.break_cost,
        allowed,
        &ctx.params.quality,
    );
    let height = count_as_f64(plan.target_lines) * plan.line_height_units;
    let aspect = if height > 0.0 {
        f64::from(state.max_width) / height
    } else {
        0.0
    };

    ctx.bucket.push(TextForm {
        lines,
        word_break_count: state.hyphen_lines,
        max_width: state.max_width,
        min_width: if state.min_width == u32::MAX {
            0
        } else {
            state.min_width
        },
        median_width,
        unevenness_pct: unevenness_pct_of_widths(widths, median_width),
        break_cost: state.break_cost,
        conservatism: state.conservatism,
        line_widths: widths.to_vec(),
        quality_milli: round_to_u32(scored.quality * 1000.0),
        roughness_pct: round_to_u32(scored.roughness * 100.0),
        aspect_milli: round_to_u32(aspect * 1000.0),
    });
}

/// Один полный прогон поиска по всем высотам при заданном потолке пропорции.
/// `aspect_max` передаётся отдельно от `ctx.params`, потому что аварийный прогон
/// §2.1 повторяет поиск ровно с этим одним снятым ограничением.
///
/// Контекст общий на оба прогона: бюджеты узлов/памяти и memo ширин НЕ
/// сбрасываются между вызовами (`node_budget_total` — потолок всего поиска).
/// Ожидает уже санированные `params` (см. [`FormSearchParams::sanitized`]): в
/// частности `line_height_units > 0` и отсутствие `NaN` в границах.
fn run_search(ctx: &mut SearchContext<'_>, aspect_max: f64) -> Vec<TextForm> {
    let n = ctx.blocks.len();
    let params = ctx.params;

    // Идеальная ширина строки корзины считается от ширины всего текста в одну
    // строку; знаменатель люфта переносов — самый широкий одиночный блок.
    let total_width = f64::from(ctx.ensure_width(0, n));
    let min_possible_width = f64::from(
        (0..n)
            .map(|index| ctx.ensure_width(index, index + 1))
            .max()
            .unwrap_or(0),
    );
    // Запас, за которым превышение верха коридора необратимо (см. `search_dfs`):
    // самый широкий одиночный блок плюс ширина дефиса переноса в единицах этой же
    // метрики (у метрики с висящей пунктуацией дефис не считается вовсе — там
    // ширина дефиса честный ноль).
    let break_slop = min_possible_width + f64::from(ctx.metric.line_width("-"));
    let line_height_units = f64::from(params.line_height_units);
    let width_cap = params
        .width_range
        .map_or(f64::INFINITY, |(_, max_width)| f64::from(max_width));

    let mut forms: Vec<TextForm> = Vec::new();
    for target_lines in 1..=n {
        if let Some((min_lines, max_lines)) = params.line_range
            && (target_lines < min_lines || target_lines > max_lines)
        {
            continue;
        }
        if ctx.search_exhausted() {
            break;
        }

        let ideal = total_width / count_as_f64(target_lines);
        let aspect_cap = aspect_max * count_as_f64(target_lines) * line_height_units;
        // Предварительного отсева корзины по `ideal` здесь НЕТ, и это не упущение
        // (план §2.4, шаг 2 предлагал его, но он неадмиссибелен):
        // `T_L = ширина_в_одну_строку / L` НЕ является нижней оценкой максимальной
        // ширины L-строчной формы — разрыв строки съедает межсловный пробел,
        // поэтому сумма ширин строк, вообще говоря, МЕНЬШЕ ширины того же текста
        // в одну строку. Контрпример: «a b» посимвольной метрикой без висящей
        // пунктуации, L = 2: одна строка = 3, `ideal` = 1.5, а реальная форма
        // ["a", "b"] имеет максимум 1 и проходит все финальные проверки — отсев
        // по `ideal` выбросил бы её при потолке между 1 и 1.5. Корзина, чьи
        // строки в потолок не лезут, и так умирает на глубине 1: верх коридора
        // зажат `aspect_cap`.
        for level in &params.corridor_levels {
            let upper = (f64::from(level.interior_hi) * ideal)
                .min(aspect_cap)
                .min(width_cap);
            if let Some((min_width, _)) = params.width_range
                && upper < f64::from(min_width)
            {
                // Ни одна строка этой корзины не дотянет до нижней границы ширины:
                // послабления коридора верх не поднимают, лестницу можно не идти.
                break;
            }
            let plan = BucketPlan {
                target_lines,
                head_lo: f64::from(level.head_lo) * ideal,
                interior_lo: f64::from(level.interior_lo) * ideal,
                tail_lo: f64::from(level.tail_lo) * ideal,
                upper,
                break_slop,
                min_possible_width,
                line_height_units,
            };
            ctx.nodes_bucket = 0;
            let mut state = PartialForm::new();
            search_dfs(ctx, &plan, 0, PhaseState::START, &mut state);
            if !ctx.bucket.is_empty() {
                break;
            }
        }

        // Слой C начинается здесь: корзина отдаётся отсортированной по качеству и
        // обрезанной до `per_bucket`; порядок ПОКАЗА корзин — дело вызывающего.
        ctx.bucket
            .sort_by_key(|form| (form.quality_milli, form.max_width, form.break_cost));
        ctx.bucket.truncate(params.per_bucket);
        forms.append(&mut ctx.bucket);
    }

    forms
}

/// Ранжированный поиск форм текста (план §2.1/§2.2/§2.4).
///
/// В отличие от исчерпывающего [`enumerate_forms`], перебор ведётся ОТДЕЛЬНО для
/// каждой высоты формы внутри коридора ширин, а результат ранжируется:
///
/// * **слой A (допуск)** — потолок пропорции `aspect_max`, бюджет переносов по
///   люфту, коридор ширин, предикат пресета и жёсткие диапазоны
///   `line_range`/`width_range`; всё это СОКРАЩАЕТ перебор, а не фильтрует его
///   результат;
/// * **слой B (качество)** — оценка `Q` (`quality_milli`, меньше лучше), не
///   содержащая предпочтения по ширине;
/// * **слой C (порядок)** — здесь только группировка: формы идут корзинами по
///   возрастанию числа строк, внутри корзины — по возрастанию `quality_milli`, не
///   более `params.per_bucket` штук. Порядок показа (round-robin по корзинам,
///   уклон в узкие, порог качества) строит вызывающий.
///
/// Гарантии: ни одна возвращённая форма не шире `aspect_max` (кроме аварийного
/// прогона ниже), не нарушает бюджет переносов и заданные диапазоны, и каждая
/// форма встречается ровно один раз. Если при действующем потолке пропорции не
/// нашлось НИЧЕГО (текст из одного длинного неразрывного слова), поиск
/// повторяется один раз без потолка — окно не должно быть пустым. Пустой ответ
/// остаётся возможным, когда форм не существует в принципе (например `Lens` на
/// одном блоке).
///
/// `text` — СЫРОЙ текст с инлайновыми тегами: снимает их (в области `scope`) и
/// оставляет защищённые диапазоны (`<no-break>`/`<nobr>`/`<m …j…>`) неразрывными
/// [`segment_form_blocks`]. Строки формы уже без снятых тегов; вернуть их на
/// ПРИМЕНЁННУЮ форму — дело [`reapply_inline_tags_to_form_text`]. `scope` обязан
/// совпадать с тем, что получила метрика ([`GlyphWidths::build`]): иначе она меряет
/// не тот алфавит, который сегментируется.
///
/// Входные `params` санируются (см. [`FormSearchParams::sanitized`]): гарантии
/// выше держатся при любых, в том числе враждебных, значениях полей.
///
/// Бюджет узлов `params.node_budget_total` — потолок ВСЕГО вызова: аварийный
/// прогон тратит то, что осталось от первого. Если первый прогон бюджет уже
/// исчерпал, аварийный не запускается вовсе, а результат помечен `truncated` —
/// исчерпанный жёсткий бюджет сильнее правила «окно не должно быть пустым»,
/// иначе `node_budget_total` перестаёт быть потолком.
///
/// `truncated` означает исчерпание бюджета (узлы/формы/память), но не отбор
/// `per_bucket` лучших. `nodes_visited` — суммарное число посещённых узлов.
#[must_use]
pub fn search_forms(
    text: &str,
    preset: TextFormPreset,
    metric: &dyn LineWidthMetric,
    params: &FormSearchParams,
    scope: InlineTagScope,
) -> FormEnumeration {
    if text.split_whitespace().next().is_none() {
        return FormEnumeration {
            forms: Vec::new(),
            truncated: false,
            nodes_visited: 0,
        };
    }
    // Санация до начала работы: `NaN` в любом сравнении даёт `false` и молча
    // снимает то ограничение, которое им задано (потолок пропорции, коридор,
    // бюджет переносов), а нулевая высота строки обнуляет знаменатель пропорции.
    let params = params.sanitized();

    with_default_segmenter(|seg| {
        let blocks = segment_form_blocks(seg, text, scope);
        if blocks.is_empty() {
            return FormEnumeration {
                forms: Vec::new(),
                truncated: false,
                nodes_visited: 0,
            };
        }

        let mut ctx = SearchContext::new(&blocks, preset, metric, &params);
        let mut forms = run_search(&mut ctx, f64::from(params.aspect_max));
        if forms.is_empty() && params.aspect_max.is_finite() {
            // План §2.1 (обязательный аварийный прогон): потолок пропорции снят,
            // остальные ограничения в силе. Измерено: 57 реплик из 2089 без этого
            // прогона не дали бы ни одной формы. Бюджет узлов общий с первым
            // прогоном — если он исчерпан, прогон завершится, ничего не посетив.
            forms = run_search(&mut ctx, f64::INFINITY);
        }
        FormEnumeration {
            forms,
            truncated: ctx.truncated,
            nodes_visited: ctx.nodes_total,
        }
    })
}

/// Подбирает одну форму поверх scored-wrap рендера: предпочитает форму, где
/// все строки не шире `target_line_width`, минимизируя число строк; иначе —
/// форму с наименьшей максимальной шириной строки.
#[must_use]
pub fn choose_form(
    text: &str,
    preset: TextFormPreset,
    target_line_width: usize,
) -> Option<Vec<String>> {
    // Здесь шрифт недоступен — используем посимвольную метрику (как раньше).
    let metric = CharWidthMetric::new(true);
    // Рендер зовёт это ПОСЛЕ разбора inline-стилей, то есть на уже очищенном тексте;
    // остаются только управляющие теги «не разрывать», и вернуть сюда нечего —
    // функция отдаёт строки, а не текст.
    let enumeration = enumerate_forms(
        text,
        preset,
        DEFAULT_MAX_FORMS,
        &metric,
        InlineTagScope::NoBreakOnly,
    );
    let target = target_line_width.max(1) as u32;
    let mut best_key: Option<(bool, usize, u32, u32, u32)> = None;
    let mut best_lines: Option<Vec<String>> = None;
    for form in &enumeration.forms {
        // Без явного выбора пользователя берём только безопасные формы (без отрыва
        // служебных слов) — как при склейке предлогов в горизонтальном врапере.
        if form.conservatism != Conservatism::Safe {
            continue;
        }
        let fits = form.max_width <= target;
        let overflow = form.max_width.saturating_sub(target);
        let key = (
            !fits,
            form.lines.len(),
            overflow,
            form.max_width,
            form.break_cost,
        );
        if best_key.is_none_or(|current| key < current) {
            best_key = Some(key);
            best_lines = Some(form.lines.clone());
        }
    }
    best_lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        /// Подменяемый источник свободной памяти для тестов защиты по памяти.
        /// `None` (по умолчанию) → реальный `available_memory_bytes()`.
        /// `Some(value)` → защита видит ровно `value` (где `value` сам `Option`).
        static MEMORY_OVERRIDE: Cell<Option<Option<u64>>> = const { Cell::new(None) };
    }

    /// Значение свободной памяти, которое видит `enumerate_dfs` через
    /// `current_available_memory` в тестовой сборке.
    pub(super) fn test_available_memory() -> Option<u64> {
        match MEMORY_OVERRIDE.with(Cell::get) {
            Some(forced) => forced,
            None => super::available_memory_bytes(),
        }
    }

    /// Выполняет `body` с подменённым источником свободной памяти, затем
    /// восстанавливает прежнее значение.
    fn with_memory_source<T>(forced: Option<u64>, body: impl FnOnce() -> T) -> T {
        let prev = MEMORY_OVERRIDE.with(|c| c.replace(Some(forced)));
        let result = body();
        MEMORY_OVERRIDE.with(|c| c.set(prev));
        result
    }

    /// Посимвольная метрика с висящими краями — прежнее поведение окна форм.
    const CHAR_METRIC: CharWidthMetric = CharWidthMetric { hanging: true };

    fn widths_of(form: &TextForm) -> Vec<u32> {
        form.lines
            .iter()
            .map(|line| CHAR_METRIC.line_width(line))
            .collect()
    }

    #[test]
    fn preset_labels_split_prose_from_shapes() {
        // Only the prose preset carries a catalog key; the shapes are literal ASCII.
        assert!(matches!(
            TextFormPreset::FreeNoTree.label(),
            PresetLabel::Key(k) if !k.is_empty()
        ));
        for preset in [
            TextFormPreset::Lens,
            TextFormPreset::Widen,
            TextFormPreset::Narrow,
        ] {
            assert!(
                matches!(preset.label(), PresetLabel::Shape(s) if !s.is_empty()),
                "{preset:?} must be a literal shape"
            );
        }
    }

    #[test]
    fn width_ignores_edge_punctuation_but_keeps_internal() {
        let count = |s: &str| s.chars().count() as u32;
        assert_eq!(CHAR_METRIC.line_width("«Привет!»"), count("Привет"));
        assert_eq!(CHAR_METRIC.line_width("что-то,"), count("что-то"));
        // Внутренняя пунктуация считается, хвостовой дефис переноса — нет.
        assert_eq!(CHAR_METRIC.line_width("из-за-"), count("из-за"));
    }

    #[test]
    fn mountain_accepts_monotone_and_peak_rejects_valley() {
        assert!(is_mountain(&[1, 2, 3], 0));
        assert!(is_mountain(&[3, 2, 1], 0));
        assert!(is_mountain(&[1, 3, 3, 2], 0));
        assert!(is_christmas_tree(&[5, 4, 6], 0));
        assert!(!is_mountain(&[5, 4, 6], 0));
    }

    #[test]
    fn tolerance_treats_near_equal_widths_as_flat() {
        // Разница 2 при допуске 3 — «ровно», гора, не ёлка.
        assert!(is_mountain(&[100, 102, 100], 3));
        assert!(!is_lens(&[100, 102, 100], 3));
        // Та же последовательность без допуска — это линза.
        assert!(is_lens(&[100, 102, 100], 0));
    }

    #[test]
    fn lens_requires_interior_peak() {
        assert!(is_lens(&[1, 3, 1], 0));
        assert!(is_lens(&[2, 2, 4, 2], 0));
        assert!(!is_lens(&[1, 2, 3], 0)); // только подъём
        assert!(!is_lens(&[3, 2, 1], 0)); // только спуск
        assert!(!is_lens(&[5, 4, 6], 0)); // долина
    }

    #[test]
    fn enumerate_widen_is_non_decreasing_only() {
        let result = enumerate_forms(
            "a bb ccc",
            TextFormPreset::Widen,
            1000,
            &CHAR_METRIC,
            InlineTagScope::NoBreakOnly,
        );
        assert!(!result.forms.is_empty());
        for form in &result.forms {
            assert!(sequence_matches(&widths_of(form), TextFormPreset::Widen, 0));
        }
    }

    #[test]
    fn enumerate_has_no_duplicates_in_single_pass() {
        let result =
            enumerate_forms(
                "one two three four",
                TextFormPreset::FreeNoTree,
                1000,
                &CHAR_METRIC,
                InlineTagScope::NoBreakOnly,
            );
        let mut seen = std::collections::HashSet::new();
        for form in &result.forms {
            assert!(
                seen.insert(form.to_text()),
                "duplicate form: {:?}",
                form.lines
            );
            assert!(!is_christmas_tree(&widths_of(form), 0));
        }
    }

    #[test]
    fn enumerate_lens_only_returns_bulging_forms() {
        let result = enumerate_forms(
            "aa b ccc dd e",
            TextFormPreset::Lens,
            1000,
            &CHAR_METRIC,
            InlineTagScope::NoBreakOnly,
        );
        for form in &result.forms {
            assert!(is_lens(&widths_of(form), 0), "{:?}", form.lines);
        }
    }

    #[test]
    fn whitespace_only_breaks_have_zero_cost_and_no_word_breaks() {
        // Короткие слова (<4 символов) не переносятся словарём — только пробелы.
        let result = enumerate_forms(
            "aa bb cc",
            TextFormPreset::FreeNoTree,
            1000,
            &CHAR_METRIC,
            InlineTagScope::NoBreakOnly,
        );
        assert!(!result.forms.is_empty());
        for form in &result.forms {
            assert_eq!(form.word_break_count, 0, "{:?}", form.lines);
            assert_eq!(form.break_cost, 0, "{:?}", form.lines);
        }
    }

    #[test]
    fn prepare_inline_no_break_text_strips_tags_and_uses_nbsp() {
        assert_eq!(
            prepare_inline_no_break_text(
                "aa <no-break>bb cc</no-break> dd",
                InlineTagScope::NoBreakOnly,
            ),
            "aa bb\u{00A0}cc dd"
        );
        assert_eq!(
            prepare_inline_no_break_text("aa <m j>bb cc</m> dd", InlineTagScope::NoBreakOnly),
            "aa bb\u{00A0}cc dd"
        );
    }

    #[test]
    fn no_break_inline_tag_keeps_contents_in_one_form_block() {
        let result = enumerate_forms(
            "aa <no-break>bb cc</no-break> dd",
            TextFormPreset::FreeNoTree,
            1000,
            &CHAR_METRIC,
            InlineTagScope::NoBreakOnly,
        );

        assert!(!result.forms.is_empty());
        assert!(result.forms.iter().all(|form| {
            form.lines
                .iter()
                .all(|line| !line.contains("<no-break>") && !line.contains("</no-break>"))
                && !form.lines.iter().any(|line| line == "bb" || line == "cc")
        }));
    }

    /// Формы обоих входов перебора для СЫРОГО (с тегами) текста — построчно.
    ///
    /// Оба входа проверяются вместе: разметку блоков они делят одну
    /// ([`segment_form_blocks`]), но ходят по дереву по-разному, и защита обязана
    /// держаться в обоих.
    fn all_form_lines_of(raw: &str) -> Vec<Vec<String>> {
        all_form_lines_of_at(raw, InlineTagScope::NoBreakOnly)
    }

    /// То же, но в заданной области снятия инлайновых тегов.
    fn all_form_lines_of_at(raw: &str, scope: InlineTagScope) -> Vec<Vec<String>> {
        let enumerated =
            enumerate_forms(raw, TextFormPreset::FreeNoTree, 1000, &CHAR_METRIC, scope);
        let searched = search_forms(
            raw,
            TextFormPreset::FreeNoTree,
            &CHAR_METRIC,
            &char_search_params(),
            scope,
        );
        assert!(
            !enumerated.forms.is_empty(),
            "enumerate_forms не нашёл ни одной формы для {raw:?}"
        );
        assert!(
            !searched.forms.is_empty(),
            "search_forms не нашёл ни одной формы для {raw:?}"
        );
        enumerated
            .forms
            .iter()
            .chain(searched.forms.iter())
            .map(|form| form.lines.clone())
            .collect()
    }

    /// `fragment` (уже подготовленный: без тегов, с NBSP вместо пробелов) целиком
    /// лежит на ОДНОЙ строке каждой формы `raw` — то есть защищённый диапазон не
    /// разорван ни пробелом, ни переносом, ни существующим дефисом.
    fn assert_protected_range_is_never_split(raw: &str, fragment: &str) {
        assert_protected_range_is_never_split_at(raw, fragment, InlineTagScope::NoBreakOnly);
    }

    /// То же, но в заданной области снятия инлайновых тегов.
    fn assert_protected_range_is_never_split_at(raw: &str, fragment: &str, scope: InlineTagScope) {
        for lines in all_form_lines_of_at(raw, scope) {
            assert!(
                lines.iter().any(|line| line.contains(fragment)),
                "форма {lines:?} разорвала защищённый диапазон {fragment:?} текста {raw:?}"
            );
        }
    }

    /// Пробел внутри `<no-break>` не разрыв: панель отдаёт движку СЫРОЙ текст, теги
    /// на месте, и диапазон становится одним блоком.
    ///
    /// Регрессия: панель снимала теги сама, движок снимал их ВТОРОЙ раз с уже чистого
    /// текста, не находил ничего и размечал защищённый диапазон переносами.
    #[test]
    fn no_break_range_with_a_space_is_never_split() {
        assert_protected_range_is_never_split(
            "пример <no-break>не разрывать</no-break> конец",
            "не\u{00A0}разрывать",
        );
    }

    /// Слово от 4 символов внутри `<no-break>` не переносится по словарю.
    ///
    /// Контроль на том же слове без тега обязателен: без него тест прошёл бы и на
    /// тексте, который словарь вообще не умеет переносить.
    #[test]
    fn no_break_range_never_hyphenates_a_long_word() {
        assert!(
            all_form_lines_of("начало переносимое конец")
                .iter()
                .any(|lines| lines.iter().any(|line| line.ends_with('-'))),
            "контрольный текст без тега обязан переноситься словарём"
        );
        assert_protected_range_is_never_split(
            "начало <nobr>переносимое</nobr> конец",
            "переносимое",
        );
    }

    /// Существующий дефис внутри `<no-break>` не точка переноса.
    ///
    /// Сегментатор режет по таким дефисам всегда (`allow_hard_hyphen_breaks: true` —
    /// это нужно остальному тексту), поэтому защиту даёт склейка стыков внутри
    /// диапазона ([`glue_protected_junctions`]).
    #[test]
    fn no_break_range_never_breaks_at_a_hard_hyphen() {
        assert!(
            all_form_lines_of("текст что-то важное дальше")
                .iter()
                .any(|lines| lines.iter().any(|line| line.ends_with("что-"))),
            "контрольный текст без тега обязан рваться по существующему дефису"
        );
        assert_protected_range_is_never_split(
            "текст <no-break>что-то важное</no-break> дальше",
            "что-то\u{00A0}важное",
        );
    }

    /// Машинный `<m j>` — та же защита, что и `<no-break>`: набор форм совпадает
    /// построчно. Машинная форма тега — та, которую панель ставит по умолчанию.
    #[test]
    fn machine_join_tag_protects_exactly_like_no_break() {
        assert_eq!(
            all_form_lines_of("пример <m j>не разрывать</m> конец"),
            all_form_lines_of("пример <no-break>не разрывать</no-break> конец")
        );
        assert_protected_range_is_never_split(
            "пример <m b j>не разрывать</m> конец",
            "не\u{00A0}разрывать",
        );
    }

    /// Заглавный текст переносится по словарю — что бы ни стояло рядом.
    ///
    /// Регрессия, ради которой эвристика «заглавное слово — аббревиатура» была
    /// удалена целиком. Пока она была относительной («заглавное слово в СМЕШАННОМ
    /// тексте»), заглавную строку лишала переносов любая строчная латиница рядом:
    /// инлайновый тег, который движок не снимает (`<b>`, `<i>`, `<font=…>`), или
    /// одно строчное слово. Теперь регистр не участвует в решении вовсе.
    #[test]
    fn all_caps_text_hyphenates_whatever_stands_next_to_it() {
        for raw in [
            "ЭТО ПРЕДЛОЖЕНИЕ ПЕРЕНОСИТСЯ",
            // Тег, который движок НЕ снимает: его строчная латиница попадает в текст
            // сегментатора и раньше «расколдовывала» защиту аббревиатуры.
            "<b>ЭТО</b> ПРЕДЛОЖЕНИЕ ПЕРЕНОСИТСЯ",
            // Одно строчное слово в конце — тот же эффект.
            "ЭТО ПРЕДЛОЖЕНИЕ ПЕРЕНОСИТСЯ ня",
            // Снимаемый тег «Не разрывать» тоже ничего не меняет.
            "СМОТРИ <no-break>СЮДА</no-break> ПЕРЕНОСИМОЕ",
        ] {
            assert!(
                all_form_lines_of(raw)
                    .iter()
                    .any(|lines| lines.iter().any(|line| line.ends_with('-'))),
                "заглавный текст {raw:?} обязан переноситься словарём"
            );
        }
    }

    /// Регрессия: текст БЕЗ тегов даёт ровно те же формы, в том же порядке, что и до
    /// переноса снятия тегов внутрь движка — со словарными переносами и разрывом по
    /// существующему дефису включительно.
    #[test]
    fn untagged_text_keeps_producing_the_same_forms() {
        const EXPECTED: [&[&str]; 11] = [
            &["что-", "то важ-", "ное", "тут"],
            &["что-", "то важ-", "ное тут"],
            &["что-", "то важное", "тут"],
            &["что-", "то важное тут"],
            &["что-то", "важ-", "ное", "тут"],
            &["что-то", "важное", "тут"],
            &["что-то", "важное тут"],
            &["что-то важ-", "ное", "тут"],
            &["что-то важ-", "ное тут"],
            &["что-то важное", "тут"],
            &["что-то важное тут"],
        ];

        let result = enumerate_forms(
            "что-то важное тут",
            TextFormPreset::FreeNoTree,
            1000,
            &CHAR_METRIC,
            InlineTagScope::NoBreakOnly,
        );
        assert_eq!(result.forms.len(), EXPECTED.len());
        for (form, expected) in result.forms.iter().zip(EXPECTED) {
            let lines: Vec<&str> = form.lines.iter().map(String::as_str).collect();
            assert_eq!(lines.as_slice(), expected);
        }
    }

    /// Все написания тега (и закрывающего), незакрытый и вложенный диапазоны.
    ///
    /// Проверяется подготовленный текст: из него берут и алфавит метрики
    /// ([`GlyphWidths::build`]), и блоки перебора, поэтому пропущенное написание
    /// молча снимало бы защиту целиком.
    #[test]
    fn no_break_tag_aliases_close_and_nest_case_insensitively() {
        const RANGES: [(&str, &str); 9] = [
            ("<no-break>", "</no-break>"),
            ("<nobreak>", "</nobreak>"),
            ("<nobr>", "</nobr>"),
            ("<NO-BREAK>", "</NO-BREAK>"),
            ("<NoBreak>", "</nobreak>"),
            ("<nobr>", "</NOBR>"),
            ("<m j>", "</m>"),
            ("<M J>", "</M>"),
            ("<m j b>", "</m>"),
        ];
        for (open, close) in RANGES {
            assert_eq!(
                prepare_inline_no_break_text(
                    &format!("aa {open}bb cc{close} dd"),
                    InlineTagScope::NoBreakOnly,
                ),
                "aa bb\u{00A0}cc dd",
                "написание {open} … {close}"
            );
        }

        // Незакрытый тег защищает до конца текста — иначе редактирование в середине
        // ввода снимало бы защиту с уже набранного.
        assert_eq!(
            prepare_inline_no_break_text("aa <no-break>bb cc", InlineTagScope::NoBreakOnly),
            "aa bb\u{00A0}cc"
        );
        // Вложенность считается глубиной: внутренний закрывающий тег не снимает
        // внешнюю защиту.
        assert_eq!(
            prepare_inline_no_break_text(
                "aa <no-break>bb <nobr>cc dd</nobr> ee</no-break> ff",
                InlineTagScope::NoBreakOnly,
            ),
            "aa bb\u{00A0}cc\u{00A0}dd\u{00A0}ee ff"
        );
        // Закрывающий тег без открывающего — просто ничего не защищает.
        assert_eq!(
            prepare_inline_no_break_text("aa</nobr> bb cc", InlineTagScope::NoBreakOnly),
            "aa bb cc"
        );
    }

    #[test]
    fn single_short_token_yields_one_form_except_lens() {
        assert_eq!(
            enumerate_forms(
                "кот",
                TextFormPreset::FreeNoTree,
                64,
                &CHAR_METRIC,
                InlineTagScope::NoBreakOnly,
            )
                .forms
                .len(),
            1
        );
        assert!(
            enumerate_forms(
                "кот",
                TextFormPreset::Lens,
                64,
                &CHAR_METRIC,
                InlineTagScope::NoBreakOnly,
            )
                .forms
                .is_empty()
        );
    }

    #[test]
    fn min_median_and_peakiness_track_line_widths() {
        let result = enumerate_forms(
            "aa bb ccccc",
            TextFormPreset::FreeNoTree,
            1000,
            &CHAR_METRIC,
            InlineTagScope::NoBreakOnly,
        );
        assert!(!result.forms.is_empty());
        for form in &result.forms {
            let mut widths = widths_of(form);
            let expected_min = widths.iter().copied().min().unwrap_or(0);
            let expected_max = widths.iter().copied().max().unwrap_or(0);
            assert_eq!(form.min_width, expected_min, "{:?}", form.lines);
            assert_eq!(form.max_width, expected_max, "{:?}", form.lines);

            widths.sort_unstable();
            let n = widths.len();
            let expected_median = if n % 2 == 1 {
                widths[n / 2]
            } else {
                (widths[n / 2 - 1] + widths[n / 2]).div_ceil(2)
            };
            assert_eq!(form.median_width, expected_median, "{:?}", form.lines);

            let peak = |base: u32| {
                if base == 0 {
                    0
                } else {
                    ((f64::from(expected_max - base) / f64::from(base)) * 100.0).round() as u32
                }
            };
            assert_eq!(form.peakiness_pct(PeakBase::Min), peak(expected_min), "{:?}", form.lines);
            assert_eq!(
                form.peakiness_pct(PeakBase::Median),
                peak(expected_median),
                "{:?}",
                form.lines
            );
        }
        // Однострочная форма всегда ровная (пиковость 0%).
        let single = result
            .forms
            .iter()
            .find(|form| form.line_count() == 1)
            .expect("single-line form exists");
        assert_eq!(single.peakiness_pct(PeakBase::Min), 0);
        assert_eq!(single.peakiness_pct(PeakBase::Median), 0);
    }

    #[test]
    fn unevenness_matches_mean_abs_deviation_from_median() {
        let result = enumerate_forms(
            "aa bb ccccc dd",
            TextFormPreset::FreeNoTree,
            1000,
            &CHAR_METRIC,
            InlineTagScope::NoBreakOnly,
        );
        assert!(!result.forms.is_empty());
        for form in &result.forms {
            let widths = widths_of(form);
            let mut sorted = widths.clone();
            sorted.sort_unstable();
            let n = sorted.len();
            let median = if n % 2 == 1 {
                sorted[n / 2]
            } else {
                (sorted[n / 2 - 1] + sorted[n / 2]).div_ceil(2)
            };
            let expected = if median == 0 {
                0
            } else {
                let mad = widths
                    .iter()
                    .map(|&w| (f64::from(w) - f64::from(median)).abs())
                    .sum::<f64>()
                    / widths.len() as f64;
                ((mad / f64::from(median)) * 100.0).round() as u32
            };
            assert_eq!(form.unevenness_pct, expected, "{:?}", form.lines);
        }
        // Ровные строки → 0%, «лесенка» → заметно больше.
        assert_eq!(unevenness_pct_of_widths(&[10, 10, 10], median_of_widths(&[10, 10, 10])), 0);
        let ladder = [2, 4, 6, 8, 10, 12];
        assert!(unevenness_pct_of_widths(&ladder, median_of_widths(&ladder)) >= 30);
    }

    #[test]
    fn forms_carry_conservatism_as_max_over_breaks() {
        // «на» — двухбуквенный предлог: отрыв в конец строки → Bold. Единственная
        // служебная связь в тексте, поэтому консервативность не выше Bold.
        let result =
            enumerate_forms(
                "кот на ветке",
                TextFormPreset::FreeNoTree,
                1000,
                &CHAR_METRIC,
                InlineTagScope::NoBreakOnly,
            );
        assert!(!result.forms.is_empty());
        // Граф один: в нём есть и безопасные формы, и формы с отрывом предлога.
        assert!(result.forms.iter().any(|f| f.conservatism == Conservatism::Safe));
        assert!(result.forms.iter().any(|f| f.conservatism == Conservatism::Bold));
        assert!(result.forms.iter().all(|f| f.conservatism <= Conservatism::Bold));
        // Форма, оставляющая «кот на» в строке, помечена Bold (разрыв после «на»).
        let split = result
            .forms
            .iter()
            .find(|f| f.lines.iter().any(|line| line.trim_end() == "кот на"))
            .expect("форма с «кот на» существует");
        assert_eq!(split.conservatism, Conservatism::Bold);
    }

    #[test]
    fn forms_filtered_to_safe_match_glued_behavior() {
        // Фильтр по `Safe` оставляет только формы без отрыва служебных слов — это и
        // есть прежнее поведение «склейки предлогов».
        let result =
            enumerate_forms(
                "кот на ветке",
                TextFormPreset::FreeNoTree,
                1000,
                &CHAR_METRIC,
                InlineTagScope::NoBreakOnly,
            );
        let safe: Vec<_> = result
            .forms
            .iter()
            .filter(|f| f.conservatism == Conservatism::Safe)
            .collect();
        assert!(!safe.is_empty());
        // Ни одна безопасная форма не отрывает «на» от «ветке».
        for form in safe {
            assert!(
                !form.lines.iter().any(|line| line.trim_end().ends_with(" на")),
                "{:?}",
                form.lines
            );
        }
    }

    #[test]
    fn choose_form_prefers_fitting_then_fewer_lines() {
        let chosen = choose_form("aa bb cc dd", TextFormPreset::Narrow, 4).unwrap();
        for line in &chosen {
            assert!(CHAR_METRIC.line_width(line) <= 4, "{line}");
        }
    }

    /// Текст с большим деревом перебора, чтобы DFS успел перешагнуть
    /// `MEMORY_CHECK_INTERVAL_NODES` и сработала проверка памяти.
    const BIG_TEXT: &str = "one two three four five six seven eight nine ten \
        eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen";

    #[test]
    fn memory_guard_stops_enumeration_when_low() {
        // Свободной памяти «осталось» меньше порога → защита срабатывает на первой
        // же проверке (узел кратный MEMORY_CHECK_INTERVAL_NODES).
        let low = MIN_AVAILABLE_MEMORY_BYTES - 1;
        let started = web_time::Instant::now();
        let result = with_memory_source(Some(low), || {
            enumerate_forms(
                BIG_TEXT,
                TextFormPreset::FreeNoTree,
                usize::MAX,
                &CHAR_METRIC,
                InlineTagScope::NoBreakOnly,
            )
        });
        assert!(result.truncated, "low memory must truncate enumeration");
        // Остановились рано: перечислили заметно меньше, чем дал бы полный обход
        // (в идеале — ничего/немного), и точно меньше потолка узлов.
        assert!(
            result.forms.len() < 10_000,
            "expected an early stop, got {} forms",
            result.forms.len()
        );
        assert!(
            started.elapsed() < web_time::Duration::from_secs(5),
            "memory guard must return promptly"
        );
    }

    #[test]
    fn memory_guard_disabled_lets_enumeration_complete() {
        // Памяти «вдоволь» — защита по памяти молчит, маленький вход исчерпывается.
        let high = MIN_AVAILABLE_MEMORY_BYTES * 16;
        let result = with_memory_source(Some(high), || {
            enumerate_forms(
                "aa bb cc",
                TextFormPreset::FreeNoTree,
                usize::MAX,
                &CHAR_METRIC,
                InlineTagScope::NoBreakOnly,
            )
        });
        assert!(!result.truncated, "small input must complete");
        assert!(!result.forms.is_empty());
    }

    #[test]
    fn max_forms_cap_still_enforced() {
        // Явный маленький cap по-прежнему действует (путь choose_form не затронут).
        let result = with_memory_source(Some(MIN_AVAILABLE_MEMORY_BYTES * 16), || {
            enumerate_forms(
                BIG_TEXT,
                TextFormPreset::FreeNoTree,
                5,
                &CHAR_METRIC,
                InlineTagScope::NoBreakOnly,
            )
        });
        assert!(result.forms.len() <= 5, "got {}", result.forms.len());
        assert!(result.truncated, "more than 5 forms exist → truncated");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn available_memory_parses_meminfo() {
        let bytes = available_memory_bytes().expect("MemAvailable readable on Linux");
        assert!(bytes > 0);
    }

    #[test]
    fn parses_macos_vm_stat_available_bytes() {
        // 16 KiB pages: (8000 free + 4000 inactive + 1000 speculative) * 16384.
        let sample = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
Pages free:                                8000.\n\
Pages active:                            234567.\n\
Pages inactive:                            4000.\n\
Pages speculative:                         1000.\n\
Pages wired down:                        333333.\n";
        assert_eq!(
            super::parse_vm_stat_available_bytes(sample),
            Some(13_000 * 16_384)
        );
    }

    #[test]
    fn macos_vm_stat_bad_input_is_none_not_panic() {
        assert_eq!(super::parse_vm_stat_available_bytes(""), None);
        // No page-size header => unknown, never a wrong number.
        assert_eq!(
            super::parse_vm_stat_available_bytes("Pages free: 10.\nPages inactive: 5.\nPages speculative: 2.\n"),
            None
        );
    }

    // --- Ранжированный поиск форм (`search_forms`) --------------------------

    /// Высота строки для `CHAR_METRIC` в единицах ЭТОЙ метрики: ~2 символа на em
    /// (плана §3, п.1) при интерлиньяже 120 % → `2 * 1.2`.
    const CHAR_METRIC_LINE_HEIGHT: f32 = 2.4;

    /// Настройки поиска по умолчанию, переведённые в единицы посимвольной метрики.
    fn char_search_params() -> FormSearchParams {
        FormSearchParams {
            line_height_units: CHAR_METRIC_LINE_HEIGHT,
            ..FormSearchParams::default()
        }
    }

    /// Ширина самого широкого одиночного блока текста, измеренного как
    /// переносимая строка — знаменатель люфта бюджета переносов (план §2.1).
    fn min_possible_width_of(text: &str) -> u32 {
        with_default_segmenter(|seg| {
            let blocks = segment_form_blocks(seg, text, InlineTagScope::NoBreakOnly);
            let last = blocks.len();
            (0..last)
                .map(|index| {
                    let (line, _) =
                        build_line_text_and_units(&blocks[index..=index], index + 1 < last);
                    CHAR_METRIC.line_width(&line)
                })
                .max()
                .unwrap_or(0)
        })
    }

    /// Пропорция формы в тех же единицах, что и потолок `aspect_max`.
    fn aspect_of(form: &TextForm, line_height_units: f32) -> f64 {
        f64::from(form.max_width) / (count_as_f64(form.line_count()) * f64::from(line_height_units))
    }

    /// Формы, сгруппированные по числу строк, в порядке появления в выдаче.
    fn buckets_of(forms: &[TextForm]) -> Vec<(usize, Vec<u32>)> {
        let mut buckets: Vec<(usize, Vec<u32>)> = Vec::new();
        for form in forms {
            match buckets.last_mut() {
                Some((lines, qualities)) if *lines == form.line_count() => {
                    qualities.push(form.quality_milli);
                }
                _ => buckets.push((form.line_count(), vec![form.quality_milli])),
            }
        }
        buckets
    }

    /// Реплика на 209 символов из реального корпуса (`dev-docs/text_forms_ranking_plan.md`
    /// §1, между p99 и максимумом): достаточно велика, чтобы дать полтора десятка
    /// корзин, и достаточно мала, чтобы прогоняться в тестах несколько раз.
    const MEDIUM_REPLICA: &str = "Кста, народ, может создать тг канал и чат вокруг моего \
перевода? Буду делиться новостями перевода, показывать процесс, отвечать на вопросы. \
Могу так же научить кого-то, или взять ещё какой-то заброшенный тайтл";

    /// Самая длинная реплика корпуса — 382 символа, 67 слов (план §1). На ней
    /// исчерпывающий перебор упирался в 4-секундный потолок и возвращал смещённую
    /// выборку; ранжированный поиск обязан завершиться в пределах бюджета узлов.
    const BIG_REPLICA: &str = "Всем привет, это ashen! МНе было очень весело делать эту \
мангу! (и больно, лол) Это моя первая манга, и это не самая лучшая работа, много ошибок \
в рисовке Я надеюсь, вы простите меня, так как работа создавалась для конкурса и у меня \
был всего месяц. Плюс коллеги вымещают свою злобу на мне но, я надеюсь, вам понравилось! \
я надеюсь в будущем создать больше историй, так что увидимся";

    /// Текст, у которого под потолком пропорции формы заведомо есть: аварийный
    /// прогон §2.1 на нём не срабатывает, поэтому потолок обязан соблюдаться.
    const MEDIUM_TEXT: &str = "один два три четыре пять шесть семь восемь";

    #[test]
    fn quality_prefers_a_flat_block_over_a_staircase_of_equal_median() {
        let weights = QualityWeights::DEFAULT;
        let no_hyphens = [false; 5];
        // Одинаковая медиана (10), разный профиль: ровный блок против «лесенки».
        let flat = form_quality(&[10, 10, 10, 10, 10], &no_hyphens, 0, 2.5, &weights);
        let staircase = form_quality(&[6, 8, 10, 12, 14], &no_hyphens, 0, 2.5, &weights);
        assert!(
            flat.quality < staircase.quality,
            "flat {} must beat staircase {}",
            flat.quality,
            staircase.quality
        );
        // Ровный блок без переносов — идеал: все термы нулевые.
        assert!(flat.quality.abs() < 1e-12, "flat block must score 0");
        assert!(staircase.roughness > 0.0);
    }

    /// Потолок `Q`, под который обязаны попадать обе эталонные панели плана §2.2.
    /// Значение выбрано между их фактическими оценками (0.766 и 1.040) и оценкой
    /// второй панели без уценки краевого перехода (1.353).
    const REFERENCE_QUALITY_CEILING: f64 = 1.10;

    #[test]
    fn reference_panels_score_under_the_ceiling_and_need_the_edge_discount() {
        let weights = QualityWeights::DEFAULT;
        // Панель p95 «ТЫ ПОСЛЕДНИЙ ЧЕЛОВЕК…»: 3 переноса на 8 строк, бюджет 0.5×8.
        let wide = form_quality(
            &[9, 11, 11, 13, 12, 12, 9, 7],
            &[true, false, true, false, true, false, false, false],
            6,
            4.0,
            &weights,
        );
        // Малая панель «ЭТО ЧТО-ТО ТИПА ТЕСТИРОВАНИЯ ЛОКАЦИИ.»: 2 переноса на 5
        // строк, намеренно короткая первая строка (3 при медиане 7).
        let small_widths = [3, 9, 8, 7, 7];
        let small_hyphens = [false, true, false, true, false];
        let small = form_quality(&small_widths, &small_hyphens, 4, 2.5, &weights);

        assert!(
            wide.quality < REFERENCE_QUALITY_CEILING,
            "wide panel scored {}",
            wide.quality
        );
        assert!(
            small.quality < REFERENCE_QUALITY_CEILING,
            "small panel scored {}",
            small.quality
        );

        // Уценка краевого перехода несущая: без неё короткая первая строка малой
        // панели читается как худший из возможных скачков и вердикт переворачивается.
        let no_discount = QualityWeights {
            edge_transition: 1.0,
            ..weights
        };
        let small_without_discount =
            form_quality(&small_widths, &small_hyphens, 4, 2.5, &no_discount);
        assert!(
            small_without_discount.quality > REFERENCE_QUALITY_CEILING,
            "without the edge discount the small panel must fail the ceiling, got {}",
            small_without_discount.quality
        );
        assert!(small.roughness < small_without_discount.roughness);
    }

    #[test]
    fn search_returns_nothing_wider_than_the_aspect_cap() {
        let params = char_search_params();
        let result = search_forms(
            MEDIUM_TEXT,
            TextFormPreset::FreeNoTree,
            &CHAR_METRIC,
            &params,
            InlineTagScope::NoBreakOnly,
        );
        assert!(!result.forms.is_empty(), "text must admit forms under the cap");
        let cap = f64::from(params.aspect_max);
        for form in &result.forms {
            let aspect = aspect_of(form, params.line_height_units);
            assert!(
                aspect <= cap + 1e-9,
                "aspect {aspect} over cap {cap} for {:?}",
                form.line_widths
            );
            // Поле `aspect_milli` обязано соответствовать посчитанной пропорции.
            assert_eq!(form.aspect_milli, round_to_u32(aspect * 1000.0));
        }
    }

    #[test]
    fn hyphen_budget_is_strict_with_slack_and_relaxed_without_it() {
        let text = "тестирование локации территории";
        let min_possible = min_possible_width_of(text);
        assert!(min_possible > 0);
        let params = char_search_params();
        let result = search_forms(
            text,
            TextFormPreset::FreeNoTree,
            &CHAR_METRIC,
            &params,
            InlineTagScope::NoBreakOnly,
        );
        assert!(!result.forms.is_empty());

        let mut saw_strict_slack = false;
        for form in &result.forms {
            let slack = f64::from(form.max_width) / f64::from(min_possible);
            let allowed = params.hyphen.allowed_ratio(slack) * count_as_f64(form.line_count());
            assert!(
                count_as_f64(form.word_break_count) <= allowed + 1e-9,
                "{} hyphens over budget {allowed} at slack {slack}",
                form.word_break_count
            );
            if slack >= f64::from(params.hyphen.slack_hi) {
                saw_strict_slack = true;
                assert!(
                    count_as_f64(form.word_break_count)
                        <= f64::from(params.hyphen.ratio_strict)
                            * count_as_f64(form.line_count())
                            + 1e-9,
                    "comfortable width must keep hyphens under the strict share"
                );
            }
        }
        assert!(saw_strict_slack, "expected forms with slack >= slack_hi");

        // Текст из равносложных слов, прижатый по ширине к самому широкому блоку:
        // люфта нет (slack = 1.0 ≤ slack_lo), переносы вынуждены — правило обязано
        // разрешить их сверх строгой доли.
        let forced_text = "молоко молоко";
        let forced_width = min_possible_width_of(forced_text);
        let forced = FormSearchParams {
            width_range: Some((0, forced_width)),
            ..char_search_params()
        };
        let forced_result =
            search_forms(
                forced_text,
                TextFormPreset::FreeNoTree,
                &CHAR_METRIC,
                &forced,
                InlineTagScope::NoBreakOnly,
            );
        assert!(!forced_result.forms.is_empty(), "narrow forms must exist");
        assert!(
            forced_result.forms.iter().any(|form| {
                f64::from(form.max_width) / f64::from(forced_width)
                    <= f64::from(params.hyphen.slack_lo)
                    && count_as_f64(form.word_break_count)
                        > f64::from(params.hyphen.ratio_strict) * count_as_f64(form.line_count())
            }),
            "at zero slack the hyphen share must be allowed above `ratio_strict`"
        );
    }

    #[test]
    fn empty_result_falls_back_to_a_lifted_aspect_cap() {
        // Одно длинное неразрывное слово: под потолком пропорции нет ни одной
        // формы, но окно не должно оставаться пустым (план §2.1).
        let text = "ааааааааааааааааааааааааааааа";
        let params = char_search_params();
        let result = search_forms(
            text,
            TextFormPreset::FreeNoTree,
            &CHAR_METRIC,
            &params,
            InlineTagScope::NoBreakOnly,
        );
        assert_eq!(result.forms.len(), 1, "fallback must yield the single form");
        let only = &result.forms[0];
        assert_eq!(only.line_count(), 1);
        assert!(
            aspect_of(only, params.line_height_units) > f64::from(params.aspect_max),
            "the fallback form is exactly the one the cap rejected"
        );

        // Формы может не существовать в принципе — тогда пустой ответ законен.
        assert!(
            search_forms(
                text,
                TextFormPreset::Lens,
                &CHAR_METRIC,
                &params,
                InlineTagScope::NoBreakOnly,
            )
                .forms
                .is_empty()
        );
    }

    #[test]
    fn buckets_are_capped_and_ordered_by_quality() {
        let params = FormSearchParams {
            per_bucket: 4,
            ..char_search_params()
        };
        let result = search_forms(
            MEDIUM_REPLICA,
            TextFormPreset::FreeNoTree,
            &CHAR_METRIC,
            &params,
            InlineTagScope::NoBreakOnly,
        );
        assert!(!result.forms.is_empty());

        let buckets = buckets_of(&result.forms);
        let mut seen_line_counts: Vec<usize> = Vec::new();
        for (line_count, qualities) in &buckets {
            assert!(
                qualities.len() <= params.per_bucket,
                "bucket L={line_count} kept {} forms",
                qualities.len()
            );
            assert!(
                qualities.windows(2).all(|pair| pair[0] <= pair[1]),
                "bucket L={line_count} is not sorted by quality: {qualities:?}"
            );
            assert!(
                !seen_line_counts.contains(line_count),
                "line count {line_count} appears in two separate runs"
            );
            seen_line_counts.push(*line_count);
        }
        // Корзины идут по возрастанию высоты формы.
        assert!(seen_line_counts.windows(2).all(|pair| pair[0] < pair[1]));
        // Все формы корзины действительно имеют её высоту, а ширины строк
        // сохранены (потребителю не нужно перемерять).
        for form in &result.forms {
            assert_eq!(form.line_widths.len(), form.line_count());
            assert_eq!(
                form.line_widths,
                form.lines
                    .iter()
                    .map(|line| CHAR_METRIC.line_width(line))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn big_replica_completes_within_the_node_budget() {
        assert!(BIG_REPLICA.chars().count() >= 350);
        let params = char_search_params();
        let result = search_forms(
            BIG_REPLICA,
            TextFormPreset::FreeNoTree,
            &CHAR_METRIC,
            &params,
            InlineTagScope::NoBreakOnly,
        );
        assert!(!result.forms.is_empty(), "a long replica must still get a window");
        assert!(
            result.nodes_visited <= params.node_budget_total,
            "node budget overrun: {}",
            result.nodes_visited
        );
        // Бюджет корзины тоже держится: ни одна не набрала больше `form_cap`.
        for (line_count, qualities) in buckets_of(&result.forms) {
            assert!(
                qualities.len() <= params.per_bucket,
                "bucket L={line_count} kept {} forms",
                qualities.len()
            );
        }
    }

    #[test]
    fn line_and_width_ranges_prune_the_search_instead_of_filtering_it() {
        let baseline = search_forms(
            MEDIUM_REPLICA,
            TextFormPreset::FreeNoTree,
            &CHAR_METRIC,
            &char_search_params(),
            InlineTagScope::NoBreakOnly,
        );
        assert!(!baseline.forms.is_empty());

        let line_limited = FormSearchParams {
            line_range: Some((9, 10)),
            ..char_search_params()
        };
        let by_lines = search_forms(
            MEDIUM_REPLICA,
            TextFormPreset::FreeNoTree,
            &CHAR_METRIC,
            &line_limited,
            InlineTagScope::NoBreakOnly,
        );
        assert!(!by_lines.forms.is_empty());
        assert!(
            by_lines
                .forms
                .iter()
                .all(|form| (9..=10).contains(&form.line_count())),
            "line_range must be a hard constraint"
        );
        // Сокращение перебора, а не фильтрация результата: две высоты из двух
        // десятков обязаны стоить кратно меньше узлов.
        assert!(
            by_lines.nodes_visited * 4 < baseline.nodes_visited,
            "line_range visited {} nodes vs baseline {}",
            by_lines.nodes_visited,
            baseline.nodes_visited
        );

        let widest = baseline
            .forms
            .iter()
            .map(|form| form.max_width)
            .max()
            .unwrap_or(0);
        let narrow_cap = widest / 2;
        let width_limited = FormSearchParams {
            width_range: Some((0, narrow_cap)),
            ..char_search_params()
        };
        let by_width = search_forms(
            MEDIUM_REPLICA,
            TextFormPreset::FreeNoTree,
            &CHAR_METRIC,
            &width_limited,
            InlineTagScope::NoBreakOnly,
        );
        assert!(!by_width.forms.is_empty());
        assert!(
            by_width
                .forms
                .iter()
                .all(|form| form.max_width <= narrow_cap),
            "width_range must clamp the corridor, not filter afterwards"
        );
        assert!(
            by_width.nodes_visited < baseline.nodes_visited,
            "width_range visited {} nodes vs baseline {}",
            by_width.nodes_visited,
            baseline.nodes_visited
        );
    }

    #[test]
    fn the_aspect_pre_skip_never_discards_an_admissible_form() {
        // Контрпример: «a b» посимвольной метрикой БЕЗ висящей пунктуации. Текст в
        // одну строку — 3 символа, значит `T_2` = 1.5, но разрыв съедает пробел:
        // реальная форма ["a", "b"] имеет максимум 1 и проходит потолок пропорции
        // (1 / (2 строки × 1.0) = 0.5 ≤ 0.6). Отсев корзины по `T_L > aspect_cap`
        // (1.5 > 1.2) выбрасывал её — то есть был неадмиссибелен.
        let metric = CharWidthMetric::new(false);
        assert_eq!(metric.line_width("a b"), 3);
        let params = FormSearchParams {
            aspect_max: 0.6,
            line_height_units: 1.0,
            ..FormSearchParams::default()
        };
        let result = search_forms(
            "a b",
            TextFormPreset::FreeNoTree,
            &metric,
            &params,
            InlineTagScope::NoBreakOnly,
        );
        assert!(
            result.forms.iter().any(|form| form.lines == ["a", "b"]),
            "the two-line form must survive, got {:?}",
            result
                .forms
                .iter()
                .map(|form| form.lines.clone())
                .collect::<Vec<_>>()
        );
        // Аварийный прогон при этом не понадобился: потолок соблюдён всеми формами.
        for form in &result.forms {
            let aspect = aspect_of(form, params.line_height_units);
            assert!(
                aspect <= f64::from(params.aspect_max) + 1e-9,
                "aspect {aspect} over cap for {:?}",
                form.lines
            );
        }
    }

    /// Метрика, у которой ширина строки НЕ растёт монотонно с индексом разрыва:
    /// строка, КОНЧАЮЩАЯСЯ на `z`, получает +10, а присоединение следующего блока
    /// делает эту `z` внутренней, и ширина падает. Это огрублённая модель
    /// реального механизма — хвостового дефиса переноса, который при удлинении
    /// строки исчезает.
    struct TrailingPenaltyMetric;

    impl LineWidthMetric for TrailingPenaltyMetric {
        fn line_width(&self, line: &str) -> u32 {
            let core = line.trim();
            let penalty = u32::from(core.ends_with('z')) * 10;
            u32::try_from(core.chars().count()).unwrap_or(u32::MAX) + penalty
        }

        fn tolerance(&self) -> u32 {
            0
        }
    }

    #[test]
    fn the_corridor_break_survives_a_non_monotone_metric() {
        let metric = TrailingPenaltyMetric;
        // Ширины префиксов первой строки: 2, 16, 9, 12 — второй выше верха
        // коридора (12), а следующий за ним снова под ним.
        assert_eq!(metric.line_width("aa"), 2);
        assert_eq!(metric.line_width("aa bbz"), 16);
        assert_eq!(metric.line_width("aa bbz cc"), 9);
        assert_eq!(metric.line_width("aa bbz cc dd"), 12);

        // Одна ступень коридора с широким верхом: `upper` = 2.0 × T_2 = 12.
        let params = FormSearchParams {
            aspect_max: 4.0,
            line_height_units: 3.0,
            corridor_levels: vec![CorridorLevel {
                interior_lo: 0.30,
                interior_hi: 2.0,
                head_lo: 0.30,
                tail_lo: 0.30,
            }],
            line_range: Some((2, 2)),
            ..FormSearchParams::default()
        };
        let result = search_forms(
            "aa bbz cc dd",
            TextFormPreset::FreeNoTree,
            &metric,
            &params,
            InlineTagScope::NoBreakOnly,
        );
        assert!(
            result
                .forms
                .iter()
                .any(|form| form.lines == ["aa bbz cc", "dd"]),
            "the break past a non-monotone bump lost the form, got {:?}",
            result
                .forms
                .iter()
                .map(|form| form.lines.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_node_budget_is_one_total_for_both_runs() {
        // Одно длинное неразрывное слово: под потолком пропорции форм нет, значит
        // аварийный прогон §2.1 обязателен — и обязан тратить ТОТ ЖЕ бюджет узлов.
        let text = "ааааааааааааааааааааааааааааа";
        let generous = char_search_params();
        let full = search_forms(
            text,
            TextFormPreset::FreeNoTree,
            &CHAR_METRIC,
            &generous,
            InlineTagScope::NoBreakOnly,
        );
        assert_eq!(full.forms.len(), 1, "a full budget must reach the fallback");
        assert!(!full.truncated);

        // Бюджет меньше того, что тратит ПЕРВЫЙ прогон: аварийный прогон не
        // запускается вовсе, а результат помечен `truncated` — документированный
        // выбор `search_forms` (жёсткий потолок сильнее «окно не должно пустовать»).
        let starved = FormSearchParams {
            node_budget_total: full.nodes_visited / 2,
            ..char_search_params()
        };
        assert!(starved.node_budget_total >= 1, "the test needs a real budget");
        let result = search_forms(
            text,
            TextFormPreset::FreeNoTree,
            &CHAR_METRIC,
            &starved,
            InlineTagScope::NoBreakOnly,
        );
        assert!(result.truncated, "an exhausted budget must be reported");
        assert!(
            result.forms.is_empty(),
            "the fallback must not get a second budget"
        );
        // Счётчик узлов проверяется ПОСЛЕ инкремента, поэтому вызов DFS, начатый на
        // последнем узле, успевает посчитать себя: перерасход ограничен числом
        // ступеней лестницы коридора, а не вторым таким же бюджетом.
        let slack = u64::try_from(starved.corridor_levels.len()).unwrap_or(0);
        assert!(
            result.nodes_visited <= starved.node_budget_total + slack,
            "spent {} nodes on a budget of {}",
            result.nodes_visited,
            starved.node_budget_total
        );
    }

    #[test]
    fn the_width_memo_degrades_instead_of_allocating_n_squared() {
        // Маленький текст — плотная таблица.
        assert!(matches!(WidthMemo::new(64), WidthMemo::Dense { .. }));
        // Вставка на сотню тысяч символов: `n²` — сотни миллионов ячеек. Таблица
        // обязана деградировать, а не выделять гигабайты.
        assert!(matches!(WidthMemo::new(100_000), WidthMemo::Sparse(_)));
        // Порог: `n²` ровно на потолке — ещё плотная, на один блок больше — уже нет.
        assert!(matches!(WidthMemo::new(1_001), WidthMemo::Sparse(_)));
        // `n * n` для абсурдного `n` не должно переполнять `usize` (checked-арифметика).
        assert!(matches!(WidthMemo::new(usize::MAX), WidthMemo::Sparse(_)));

        // Обе ветки одинаково отдают записанное и молчат про незаписанное.
        let mut memos = [WidthMemo::new(8), WidthMemo::new(100_000)];
        for memo in &mut memos {
            assert_eq!(memo.get(3, 5), None);
            memo.insert(3, 5, 42);
            assert_eq!(memo.get(3, 5), Some(42));
            assert_eq!(memo.get(3, 4), None);
            assert_eq!(memo.get(2, 5), None);
        }
    }

    #[test]
    fn hostile_params_are_sanitized_before_the_search() {
        let nan = f32::NAN;
        // Каждое вещественное поле — вне области значений: `NaN` в потолке
        // пропорции, нулевая высота строки, `NaN` в границах коридора, в долях
        // бюджета переносов и в весах качества.
        let hostile = FormSearchParams {
            aspect_max: nan,
            line_height_units: 0.0,
            corridor_levels: vec![CorridorLevel {
                interior_lo: nan,
                interior_hi: nan,
                head_lo: nan,
                tail_lo: nan,
            }],
            hyphen: HyphenBudget {
                ratio_strict: nan,
                ratio_relaxed: nan,
                slack_lo: nan,
                slack_hi: nan,
            },
            quality: QualityWeights {
                rough: nan,
                uneven: nan,
                head: nan,
                edge_transition: nan,
                ..QualityWeights::DEFAULT
            },
            ..FormSearchParams::default()
        };
        let defaults = FormSearchParams::default();
        // Санация точечная: заменяется каждое отдельное поле, а не структура
        // целиком — ступень коридора остаётся одна, но со строгими границами.
        assert_eq!(
            hostile.sanitized(),
            FormSearchParams {
                corridor_levels: vec![CorridorLevel::STRICT],
                ..FormSearchParams::default()
            },
            "every out-of-domain real must fall back to its default"
        );

        let result = search_forms(
            MEDIUM_REPLICA,
            TextFormPreset::FreeNoTree,
            &CHAR_METRIC,
            &hostile,
            InlineTagScope::NoBreakOnly,
        );
        assert!(!result.forms.is_empty(), "sanitised params must still search");

        // Потолок пропорции действует (значением по умолчанию, а не «нет потолка»).
        for form in &result.forms {
            let aspect = aspect_of(form, defaults.line_height_units);
            assert!(
                aspect <= f64::from(defaults.aspect_max) + 1e-9,
                "aspect {aspect} over the default cap"
            );
        }
        // Бюджет переносов действует: `NaN` в доле пропускал абсолютно всё.
        let min_possible = f64::from(min_possible_width_of(MEDIUM_REPLICA));
        assert!(min_possible > 0.0);
        for form in &result.forms {
            let slack = f64::from(form.max_width) / min_possible;
            let allowed = defaults.hyphen.allowed_ratio(slack) * count_as_f64(form.line_count());
            assert!(
                count_as_f64(form.word_break_count) <= allowed + 1e-9,
                "{} hyphens over budget {allowed} at slack {slack}",
                form.word_break_count
            );
        }
        // Оценка качества посчитана, а не превращена `NaN`-весами в «идеал» (0).
        assert!(
            result.forms.iter().any(|form| form.quality_milli > 0),
            "NaN weights would have scored every form as perfect"
        );
    }

    #[test]
    fn the_search_emits_each_form_exactly_once() {
        // Тождество формы — её вектор разрезов, и каждый перебирается ровно раз
        // (доказательство — в `search_dfs`); дедупликации по хешу, способной
        // потерять форму на коллизии, в поиске нет.
        let result = search_forms(
            MEDIUM_REPLICA,
            TextFormPreset::FreeNoTree,
            &CHAR_METRIC,
            &char_search_params(),
            InlineTagScope::NoBreakOnly,
        );
        assert!(result.forms.len() > 10, "expected a rich sample");
        let mut seen: HashSet<Vec<String>> = HashSet::new();
        for form in &result.forms {
            assert!(
                seen.insert(form.lines.clone()),
                "duplicate form: {:?}",
                form.lines
            );
        }
    }

    #[test]
    fn exhaustive_enumeration_keeps_widths_but_stays_unscored() {
        let result = enumerate_forms(
            "aa bb ccccc",
            TextFormPreset::FreeNoTree,
            1000,
            &CHAR_METRIC,
            InlineTagScope::NoBreakOnly,
        );
        assert!(!result.forms.is_empty());
        for form in &result.forms {
            assert_eq!(form.line_widths, widths_of(form));
            assert_eq!(form.quality_milli, UNSCORED_QUALITY_MILLI);
            // Пропорция без известной высоты строки не определена.
            assert_eq!(form.aspect_milli, 0);
            assert_eq!(
                form.roughness_pct,
                roughness_pct_of_widths(&form.line_widths, &QualityWeights::DEFAULT)
            );
        }
    }

    /// Ножницы для тестов обхода: область `All` с типичным кеглем.
    const ALL_TAGS: InlineTagScope = InlineTagScope::All {
        base_font_size_px: 24.0,
    };

    #[test]
    fn the_strip_vocabulary_is_the_renderers_vocabulary() {
        // Тот же корпус, что и у `inline_styles`: если словарь снятия разойдётся с
        // парсером рендера, теги применённой формы уедут относительно того, что рисуется.
        for body in [
            "b", "/b", "i", "em", "font=Arial", "size=24", "color=#ff0000", "align=center",
            "offset=1,2", "stretching=120%,90%", "m b=1", "/m", "br", "no-break", "/nobr",
        ] {
            let text = format!("aa<{body}>bb");
            assert_eq!(
                prepare_inline_no_break_text(&text, ALL_TAGS),
                "aabb",
                "<{body}> must be stripped at the All scope"
            );
        }
        for body in ["unknown", "size=abc", "m=1", "font=", ""] {
            let text = format!("aa<{body}>bb");
            assert_eq!(
                prepare_inline_no_break_text(&text, ALL_TAGS),
                text,
                "<{body}> is not a tag and must stay literal"
            );
        }
    }

    #[test]
    fn no_break_only_scope_keeps_style_tags_as_literal_text() {
        // Со снятой галкой «Инлайновые теги» рендер рисует `<b>` буквально — перебор форм
        // обязан мерить его как текст.
        assert_eq!(
            prepare_inline_no_break_text("aa<b>bb</b>cc", InlineTagScope::NoBreakOnly),
            "aa<b>bb</b>cc"
        );
        assert_eq!(
            prepare_inline_no_break_text("aa<br>bb", InlineTagScope::NoBreakOnly),
            "aa<br>bb"
        );
        // Управляющие теги снимаются в обеих областях.
        assert_eq!(
            prepare_inline_no_break_text("aa<no-break>b c</no-break>", InlineTagScope::NoBreakOnly),
            "aab\u{00A0}c"
        );
        assert_eq!(
            prepare_inline_no_break_text("aa<m b=1>bb</m>", InlineTagScope::NoBreakOnly),
            "aabb"
        );
    }

    #[test]
    fn anchors_carry_the_tag_verbatim_and_offsets_of_the_stripped_text() {
        let stripped = strip_inline_tags("aa<b>bb</b>", ALL_TAGS);
        assert_eq!(
            stripped.anchors,
            vec![
                TagAnchor {
                    plain_offset: 2,
                    source: "<b>".to_string(),
                    closing: false,
                },
                TagAnchor {
                    plain_offset: 4,
                    source: "</b>".to_string(),
                    closing: true,
                },
            ]
        );
    }

    #[test]
    fn anchor_offsets_account_for_the_nbsp_widening() {
        // Пробел защищённого диапазона становится двухбайтовым NBSP: якорь берёт длину
        // УЖЕ ОЧИЩЕННОГО текста, поэтому сдвинуться не может.
        let stripped = strip_inline_tags("<no-break>aa <b>bb</b></no-break>", ALL_TAGS);
        let offsets: Vec<usize> = stripped
            .anchors
            .iter()
            .map(|anchor| anchor.plain_offset)
            .collect();
        assert_eq!(offsets, vec!["aa\u{00A0}".len(), "aa\u{00A0}bb".len()]);
        assert_eq!(
            reapply_inline_tags_to_form_text(
                "<no-break>aa <b>bb</b></no-break>",
                ALL_TAGS,
                "aa\u{00A0}bb"
            ),
            Ok("aa\u{00A0}<b>bb</b>".to_string())
        );
    }

    #[test]
    fn a_single_line_form_reproduces_the_source_markup_exactly() {
        // Неправильно вложенные и «висячие» теги не чинятся и не переставляются.
        for source in [
            "<b>aa<i>bb</b>cc</i>dd",
            "</b>aa",
            "<b><i>aa</i></b>",
            "<m b=1 c=ff0000>aa</m>",
        ] {
            let plain: String = strip_inline_tags(source, ALL_TAGS)
                .runs
                .into_iter()
                .map(|run| run.text)
                .collect();
            assert_eq!(
                reapply_inline_tags_to_form_text(source, ALL_TAGS, plain.as_str()),
                Ok(source.to_string()),
                "single-line form must round-trip {source}"
            );
        }
    }

    #[test]
    fn a_closing_tag_at_a_break_stays_on_the_preceding_line() {
        assert_eq!(
            reapply_inline_tags_to_form_text("<b>aaa</b> bbb", ALL_TAGS, "aaa\nbbb"),
            Ok("<b>aaa</b>\nbbb".to_string())
        );
    }

    #[test]
    fn an_opening_tag_at_a_break_starts_the_following_line() {
        // Якорь стоит ПЕРЕД пробелом разрыва — открывающий тег всё равно уезжает за него.
        assert_eq!(
            reapply_inline_tags_to_form_text("aaa<b> bbb</b>", ALL_TAGS, "aaa\nbbb"),
            Ok("aaa\n<b>bbb</b>".to_string())
        );
        // И тот же тег, стоящий ПОСЛЕ пробела.
        assert_eq!(
            reapply_inline_tags_to_form_text("aaa <b>bbb</b>", ALL_TAGS, "aaa\nbbb"),
            Ok("aaa\n<b>bbb</b>".to_string())
        );
    }

    #[test]
    fn a_wrap_hyphen_stays_inside_the_span_it_belongs_to() {
        // Словарный перенос: дефис дописан формой, в исходнике его нет.
        assert_eq!(
            reapply_inline_tags_to_form_text("<b>краси</b>вый", ALL_TAGS, "краси-\nвый"),
            Ok("<b>краси-</b>\nвый".to_string())
        );
        assert_eq!(
            reapply_inline_tags_to_form_text("краси<b>вый</b>", ALL_TAGS, "краси-\nвый"),
            Ok("краси-\n<b>вый</b>".to_string())
        );
    }

    #[test]
    fn an_existing_hard_hyphen_is_not_mistaken_for_a_wrap_hyphen() {
        assert_eq!(
            reapply_inline_tags_to_form_text("Рао<b>-кун</b>", ALL_TAGS, "Рао-\nкун"),
            Ok("Рао<b>-\nкун</b>".to_string())
        );
        assert_eq!(
            reapply_inline_tags_to_form_text("Рао-<b>кун</b>", ALL_TAGS, "Рао-\nкун"),
            Ok("Рао-\n<b>кун</b>".to_string())
        );
    }

    #[test]
    fn a_machine_tag_survives_the_form_it_was_split_by() {
        assert_eq!(
            reapply_inline_tags_to_form_text(
                "aaa <m b=1 c=ff0000>bbb ccc</m> ddd",
                ALL_TAGS,
                "aaa bbb\nccc ddd"
            ),
            Ok("aaa <m b=1 c=ff0000>bbb\nccc</m> ddd".to_string())
        );
    }

    #[test]
    fn no_break_and_br_are_consumed_and_never_restored() {
        assert_eq!(
            reapply_inline_tags_to_form_text(
                "<no-break>aa bb</no-break> cc",
                ALL_TAGS,
                "aa\u{00A0}bb\ncc"
            ),
            Ok("aa\u{00A0}bb\ncc".to_string())
        );
        assert_eq!(
            reapply_inline_tags_to_form_text("aa<br>bb", ALL_TAGS, "aa\nbb"),
            Ok("aa\nbb".to_string())
        );
    }

    #[test]
    fn collapsed_and_normalised_separators_do_not_displace_tags() {
        // Табуляция/перевод строки нормализуются в пробел с тем же числом символов.
        assert_eq!(
            reapply_inline_tags_to_form_text("<b>aa</b>\tbb", ALL_TAGS, "aa bb"),
            Ok("<b>aa</b> bb".to_string())
        );
        // Несколько пробелов схлопываются в один перенос.
        assert_eq!(
            reapply_inline_tags_to_form_text("<b>aa</b>   bb", ALL_TAGS, "aa\nbb"),
            Ok("<b>aa</b>\nbb".to_string())
        );
        // Якорь ВНУТРИ схлопнутого прогона.
        assert_eq!(
            reapply_inline_tags_to_form_text("aa <b>  bb</b>", ALL_TAGS, "aa\nbb"),
            Ok("aa\n<b>bb</b>".to_string())
        );
        // Ведущие пробелы сегментатор выбрасывает.
        assert_eq!(
            reapply_inline_tags_to_form_text("  <b>aa</b>", ALL_TAGS, "aa"),
            Ok("<b>aa</b>".to_string())
        );
    }

    #[test]
    fn a_user_typed_soft_hyphen_is_consumed_without_shifting_tags() {
        assert_eq!(
            reapply_inline_tags_to_form_text("aa\u{00AD}<b>bb</b>", ALL_TAGS, "aabb"),
            Ok("aa<b>bb</b>".to_string())
        );
    }

    #[test]
    fn an_ill_nested_pair_at_a_break_is_split_across_the_lines_without_panicking() {
        // `<i></b>` уже неправильно вложен: на разрыве закрывающий уходит в конец
        // предыдущей строки, открывающий — в начало следующей, и их порядок меняется.
        // Документированный остаток, а не дефект — чинить чужую разметку здесь нечем,
        // и рендер увидит ровно тот набор тегов, что и без формы.
        assert_eq!(
            reapply_inline_tags_to_form_text("aaa<i></b> bbb", ALL_TAGS, "aaa\nbbb"),
            Ok("aaa</b>\n<i>bbb".to_string())
        );
    }


    /// The refusal must catch only what the engine cannot produce.
    ///
    /// The counterpart of the two refusal tests below: they pin that an illegal
    /// transformation is rejected, this one pins that no LEGAL one is — every form the
    /// enumerator actually emits, for texts exercising every whitespace shape the walk
    /// knows (multiple spaces, tabs, a literal newline kept inside a dash segment, edge
    /// whitespace, dictionary and hard hyphens, soft hyphens, protected ranges), must
    /// re-tag and strip back to itself. A tightening of [`classify_form_whitespace`] that
    /// starts refusing real forms shows up here instead of in the panel.
    #[test]
    fn no_form_the_engine_actually_produces_is_refused() {
        const CORPUS: &[&str] = &[
            "начало <b>переносимое</b> слово <m c=ff0000>и ещё</m> конец",
            "слово — <b>другое</b> слово ещё",
            "начало  \n  — <b>конец</b>",
            "  <b>ведущие</b> пробелы и хвостовые   ",
            "Рао<b>-кун</b> сказал что-то <i>важное</i> сегодня",
            "текст\tс\tтабами <b>и</b> тегами внутри",
            "<no-break>не разрывать</no-break> дальше <b>жирный</b> текст",
            "мягкий\u{00AD}перенос <b>внутри</b> слова тоже бывает",
            "<m j c=ff0000>защищено машинным</m> и <font=Arial>шрифт</font> дальше",
            "a<b>b</b> c d e f g h i j",
            "один—два <b>три</b>—четыре пять",
            "very long english <b>hyphenation</b> candidate words here",
            "текст <b>с</b>  двумя  пробелами  везде  между  словами",
        ];
        for raw in CORPUS {
            for lines in all_form_lines_of_at(raw, ALL_TAGS) {
                let form_text = lines.join("\n");
                let retagged = reapply_inline_tags_to_form_text(raw, ALL_TAGS, form_text.as_str())
                    .unwrap_or_else(|err| panic!("{raw:?} form {lines:?} refused: {err}"));
                assert_eq!(
                    prepare_inline_no_break_text(retagged.as_str(), ALL_TAGS),
                    form_text,
                    "{raw:?} form {lines:?}"
                );
            }
            for lines in all_form_lines_of(raw) {
                let form_text = lines.join("\n");
                reapply_inline_tags_to_form_text(raw, InlineTagScope::NoBreakOnly, form_text.as_str())
                    .unwrap_or_else(|err| panic!("nb {raw:?} form {lines:?} refused: {err}"));
            }
        }
    }

    #[test]
    fn a_form_text_that_does_not_come_from_this_source_is_refused() {
        assert_eq!(
            reapply_inline_tags_to_form_text("<b>aaa</b>", ALL_TAGS, "zzz"),
            Err(TagReapplyError::Unalignable {
                plain_offset: 0,
                form_offset: 0,
            })
        );
        // Отказ, а не пересинхронизация на более позднем совпадении.
        assert!(
            reapply_inline_tags_to_form_text("aaa <b>bbb</b>", ALL_TAGS, "aaa bbb extra").is_err()
        );
    }

    /// A whitespace change the form engine cannot make is a refusal, not a guess.
    ///
    /// The walk enters its whitespace branch whenever EITHER side sees whitespace, so an
    /// alleged form that invents a separator or swallows one would otherwise be accepted
    /// and the tags placed around text the user never sees. `Unalignable` is the whole
    /// safety property of this function, and it has to hold for these two as well.
    #[test]
    fn whitespace_the_form_engine_cannot_produce_is_refused() {
        // A space out of nowhere: the source has none at that junction.
        assert_eq!(
            reapply_inline_tags_to_form_text("a<b>b</b>", ALL_TAGS, "a b"),
            Err(TagReapplyError::Unalignable {
                plain_offset: 1,
                form_offset: 1,
            })
        );
        // And a separator that disappeared without a break taking its place: interior
        // whitespace is only ever normalized or collapsed INTO a break, never dropped.
        assert_eq!(
            reapply_inline_tags_to_form_text("a <b>b</b>", ALL_TAGS, "ab"),
            Err(TagReapplyError::Unalignable {
                plain_offset: 1,
                form_offset: 1,
            })
        );
    }

    /// A trailing `'-'` is not a wrap hyphen: the last line of a form never wraps.
    ///
    /// `build_line_text_and_units` appends `Joint::wrap_suffix` only for a line that wraps
    /// (`ms-text-util/src/segmentation/base.rs`), so a hyphen the source does not have can
    /// only appear before the `'\n'` that separates it from the rest of the source.
    /// Accepting one at end of text let an alleged form add a visible character.
    #[test]
    fn a_trailing_hyphen_no_break_follows_is_not_a_wrap_hyphen() {
        assert_eq!(
            reapply_inline_tags_to_form_text("<b>abc</b>", ALL_TAGS, "abc-"),
            Err(TagReapplyError::Unalignable {
                plain_offset: 3,
                form_offset: 3,
            })
        );
    }

    /// A literal `'\n'` INSIDE a block keeps the whitespace around it, and the walk must
    /// map it character by character.
    ///
    /// `build_segments` glues a standalone dash token to the previous word together with
    /// the whitespace tokens on both sides, verbatim (`ms-text-util/src/segmentation/
    /// base.rs`), so a block's text can contain a real `'\n'` with more whitespace around
    /// it. Only the run LENGTHS tell that apart from a run the form collapsed into the
    /// single `'\n'` of a break — "collapse whenever the target is `'\n'`" would eat the
    /// block's own spaces and displace every later tag.
    #[test]
    fn a_literal_newline_inside_a_block_keeps_its_whitespace_run() {
        const TAGGED: &str = "начало  \n  — <b>конец</b>";
        let forms = all_form_lines_of_at(TAGGED, ALL_TAGS);
        assert!(!forms.is_empty());
        let mut saw_literal_run = false;
        for lines in forms {
            let form_text = lines.join("\n");
            saw_literal_run |= form_text.contains("  \n  —");
            let retagged = reapply_inline_tags_to_form_text(TAGGED, ALL_TAGS, form_text.as_str())
                .unwrap_or_else(|err| panic!("form {lines:?} could not be re-tagged: {err}"));
            for tag in ["<b>", "</b>"] {
                assert_eq!(
                    retagged.matches(tag).count(),
                    1,
                    "form {lines:?} lost or duplicated {tag}: {retagged:?}"
                );
            }
            // Снятие того же текста возвращает ровно строки формы — значит теги вернулись
            // на свои места, а пробелы блока никуда не делись.
            assert_eq!(
                prepare_inline_no_break_text(retagged.as_str(), ALL_TAGS),
                form_text
            );
        }
        assert!(
            saw_literal_run,
            "the fixture must actually produce a block carrying a literal newline"
        );
    }

    #[test]
    fn a_text_without_reapplied_tags_comes_back_untouched() {
        assert_eq!(
            reapply_inline_tags_to_form_text("aaa bbb", ALL_TAGS, "aaa\nbbb"),
            Ok("aaa\nbbb".to_string())
        );
        // Область `NoBreakOnly` якорей стилей не заводит вовсе.
        assert_eq!(
            reapply_inline_tags_to_form_text(
                "<b>aaa</b> bbb",
                InlineTagScope::NoBreakOnly,
                "<b>aaa</b>\nbbb"
            ),
            Ok("<b>aaa</b>\nbbb".to_string())
        );
    }

    #[test]
    fn style_tags_stay_in_the_form_lines_at_the_no_break_only_scope() {
        // Со снятой галкой «Инлайновые теги» рендер `<b>` не разбирает и рисует его как
        // текст: перебор обязан его сохранить И померить, иначе форма описывает текст,
        // которого пользователь не увидит.
        let all = all_form_lines_of("aa <b>bbbb</b> cc");
        assert!(!all.is_empty());
        for lines in &all {
            let text = lines.join("\n");
            assert!(
                text.contains("<b>") && text.contains("</b>"),
                "form {lines:?} dropped a tag the renderer draws"
            );
        }
        // Ширины считаются по строкам ВМЕСТЕ с тегами.
        let result = enumerate_forms(
            "aa <b>bbbb</b> cc",
            TextFormPreset::FreeNoTree,
            1000,
            &CHAR_METRIC,
            InlineTagScope::NoBreakOnly,
        );
        for form in &result.forms {
            assert_eq!(form.line_widths, widths_of(form));
        }
    }

    #[test]
    fn style_tags_leave_both_the_form_lines_and_the_widths_at_the_all_scope() {
        const TAGGED: &str = "aa <b>bbbb</b> <m c=ff0000>cccc</m> <font=Arial>dd</font>";
        const PLAIN: &str = "aa bbbb cccc dd";

        for lines in all_form_lines_of_at(TAGGED, ALL_TAGS) {
            assert!(
                !lines.iter().any(|line| line.contains('<')),
                "form {lines:?} still carries markup"
            );
        }
        // Сильнее «тегов не видно»: набор форм обязан СОВПАСТЬ с набором форм текста,
        // в котором тегов не было вовсе — то есть их символы нигде не померены.
        assert_eq!(
            all_form_lines_of_at(TAGGED, ALL_TAGS),
            all_form_lines_of_at(PLAIN, ALL_TAGS)
        );
    }

    #[test]
    fn a_protected_range_survives_the_all_scope_too() {
        // Три вида разрыва, теперь при снятых тегах стиля.
        assert_protected_range_is_never_split_at(
            "пример <no-break>не разрывать</no-break> конец",
            "не\u{00A0}разрывать",
            ALL_TAGS,
        );
        assert_protected_range_is_never_split_at(
            "начало <nobr>переносимое</nobr> конец",
            "переносимое",
            ALL_TAGS,
        );
        assert_protected_range_is_never_split_at(
            "текст <no-break>что-то важное</no-break> дальше",
            "что-то\u{00A0}важное",
            ALL_TAGS,
        );
        // Машинный `<m j>` защищает так же — и при этом остаётся возвращаемым.
        assert_protected_range_is_never_split_at(
            "пример <m j c=ff0000>не разрывать</m> конец",
            "не\u{00A0}разрывать",
            ALL_TAGS,
        );
    }

    #[test]
    fn every_form_of_a_tagged_text_can_be_re_tagged() {
        const TAGGED: &str = "начало <b>переносимое</b> слово <m c=ff0000>и ещё</m> конец";
        let lines = all_form_lines_of_at(TAGGED, ALL_TAGS);
        assert!(!lines.is_empty());
        for lines in lines {
            let form_text = lines.join("\n");
            let retagged = reapply_inline_tags_to_form_text(TAGGED, ALL_TAGS, form_text.as_str())
                .unwrap_or_else(|err| panic!("form {lines:?} could not be re-tagged: {err}"));
            // Каждый снятый тег вернулся ровно один раз, в исходном порядке.
            for tag in ["<b>", "</b>", "<m c=ff0000>", "</m>"] {
                assert_eq!(
                    retagged.matches(tag).count(),
                    1,
                    "form {lines:?} lost or duplicated {tag}: {retagged:?}"
                );
            }
            // И снятие того же текста возвращает ровно строки формы.
            assert_eq!(
                prepare_inline_no_break_text(retagged.as_str(), ALL_TAGS),
                form_text
            );
        }
    }
}
