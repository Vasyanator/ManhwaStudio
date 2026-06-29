/*
File: src/tabs/typing/render_next/wrap/forms.rs

Purpose:
Общая (shared) логика дискретных «форм» текста: разбиение текста на строки так,
чтобы последовательность ширин строк удовлетворяла выбранной форме.

Used by:
- панель typing (`panel.rs`) — окно «Продвинутая форма текста» перечисляет все
  валидные формы (`enumerate_forms`) и отображает их обычным egui-текстом;
- новый рендер (`render_next`) — `choose_form` подбирает одну форму поверх
  существующего scored-wrap, не переписывая его.

Дерево перебора:
Текст заранее делится на блоки сегментатором (`segmentation::Segmenter::segment`
после `soft_hyphenate_overlong`, в режиме `BindingMode::Annotate`):
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
*/

use std::collections::{BTreeSet, HashMap, HashSet};

use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping};

use super::is_hanging_punctuation;
use crate::tabs::typing::render_next::types::parse_machine_tag;
use crate::tabs::typing::segmentation::{
    BindingMode, Block, Conservatism, NON_BREAKING_SPACE, SOFT_HYPHEN, SegmentOptions,
    build_line_text_and_units, with_default_segmenter,
};

/// Максимум перечисляемых форм за один прогон (защита от комбинаторного взрыва).
pub(crate) const DEFAULT_MAX_FORMS: usize = 600;

/// Нижний порог свободной памяти, при котором перечисление останавливается
/// досрочно (`truncated`). ≈ 512 MiB — запас на последующую сортировку/клон/
/// отрисовку результатов, чтобы остановиться заметно раньше OOM-killer'а.
const MIN_AVAILABLE_MEMORY_BYTES: u64 = 512 * 1024 * 1024;

/// Как часто (в узлах DFS) проверять свободную память. Чтение `/proc/meminfo`
/// на каждом узле было бы дорого, поэтому проверяем раз в N узлов.
const MEMORY_CHECK_INTERVAL_NODES: usize = 8192;

/// Аварийный потолок числа узлов DFS — гарантия завершения для случая, когда
/// свободную память наблюдать нельзя (`available_memory_bytes()` вернул `None`,
/// напр. не-Linux) ИЛИ память просто никогда не падает ниже порога. На Linux с
/// читаемым `MemAvailable` практический ограничитель — память, а не этот потолок
/// (то есть фактически «без лимита по количеству, только по памяти»).
const SAFETY_NODE_CEILING: usize = 50_000_000;

/// Доступная («OOM-релевантная») память процесса в байтах. На Linux читает
/// `MemAvailable:` из `/proc/meminfo` (значение в кБ → ×1024); эта метрика
/// учитывает освобождаемый кеш, поэтому точнее, чем `MemFree`. Возвращает `None`,
/// если файл/строку нельзя прочитать или распарсить (не-Linux или ошибка).
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

/// На не-Linux метрика недоступна — память не наблюдаем, работает только
/// аварийный потолок узлов (`SAFETY_NODE_CEILING`).
#[cfg(not(target_os = "linux"))]
fn available_memory_bytes() -> Option<u64> {
    None
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
pub(crate) enum TextFormPreset {
    /// «Свободный без ёлки».
    FreeNoTree,
    /// «(  )».
    Lens,
    /// «/  \».
    Widen,
    /// «\  /».
    Narrow,
}

impl TextFormPreset {
    #[must_use]
    pub(crate) fn label(self) -> &'static str {
        match self {
            TextFormPreset::FreeNoTree => "Свободный без ёлки",
            TextFormPreset::Lens => "(  )",
            TextFormPreset::Widen => "/  \\",
            TextFormPreset::Narrow => "\\  /",
        }
    }

    #[must_use]
    pub(crate) fn all() -> [TextFormPreset; 4] {
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
pub(crate) struct TextForm {
    pub(crate) lines: Vec<String>,
    /// Число переносов слов (непробельных разрывов: словарных + существующих дефисов).
    pub(crate) word_break_count: usize,
    /// Максимальная ширина строки формы (в единицах метрики, см. `LineWidthMetric`).
    pub(crate) max_width: u32,
    /// Минимальная ширина строки формы (в единицах метрики).
    pub(crate) min_width: u32,
    /// Медианная ширина строки формы (в единицах метрики).
    pub(crate) median_width: u32,
    /// «Неравномерность» формы в % — среднее отклонение ширин строк от медианы,
    /// в долях медианы. `0%` — все строки одной ширины; выше — сильнее разброс.
    pub(crate) unevenness_pct: u32,
    /// Накопленная цена разрывов формы (для сортировки внутри одинаковой ширины).
    pub(crate) break_cost: u32,
    /// Консервативность формы — максимум категорий по её фактическим разрывам.
    /// `Safe` — все переносы безопасны; выше — есть отрыв предлога/частицы и т.п.
    /// По этому полю формы фильтруются (см. окно «Продвинутая форма текста»).
    pub(crate) conservatism: Conservatism,
}

/// База отсчёта пиковости: с чем сравнивать самую длинную строку.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeakBase {
    /// Самая короткая строка.
    Min,
    /// Медианная строка.
    Median,
}

impl TextForm {
    #[must_use]
    pub(crate) fn to_text(&self) -> String {
        self.lines.join("\n")
    }

    /// Число строк формы.
    #[must_use]
    pub(crate) fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// «Пиковость» формы в процентах: на сколько самая длинная строка длиннее
    /// базовой — `round((max − base) / base × 100)`. База — минимальная или
    /// медианная строка. `0%` — самая длинная не длиннее базовой.
    #[must_use]
    pub(crate) fn peakiness_pct(&self, base: PeakBase) -> u32 {
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

/// «Неравномерность» в %: среднее абсолютное отклонение ширин от медианы,
/// делённое на медиану. Устойчива к одиночным выбросам (короткая последняя
/// строка почти не влияет), но штрафует общий разброс («лесенку»/«воронку»).
#[must_use]
fn unevenness_pct_of_widths(widths: &[u32], median: u32) -> u32 {
    if widths.is_empty() || median == 0 {
        return 0;
    }
    let mean_abs_dev = widths
        .iter()
        .map(|&w| (f64::from(w) - f64::from(median)).abs())
        .sum::<f64>()
        / widths.len() as f64;
    ((mean_abs_dev / f64::from(median)) * 100.0).round() as u32
}

// --- Метрика ширины строки -------------------------------------------------

/// Кегль (единиц на em), на котором меряются глифы попиксельной метрики. Числа
/// получаются целыми и не зависят от реального размера текста.
const WIDTH_METRIC_EM: f32 = 1000.0;

/// Допуск сравнения ширин по умолчанию (в единицах метрики, ~3.5% em): разница
/// `≤ tolerance` считается «равно» при классификации формы.
pub(crate) const DEFAULT_WIDTH_TOLERANCE: u32 = 35;

/// Источник ширины строки для перебора форм.
pub(crate) trait LineWidthMetric {
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
pub(crate) struct CharWidthMetric {
    hanging: bool,
}

impl CharWidthMetric {
    #[must_use]
    pub(crate) fn new(hanging: bool) -> Self {
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
pub(crate) struct GlyphWidths {
    advances: HashMap<char, u32>,
    kerns: HashMap<(char, char), i32>,
    hanging: bool,
    tolerance: u32,
}

impl GlyphWidths {
    /// Строит таблицу для символов `text` (плюс дефис переноса) выбранным
    /// шрифтом `attrs` в системе `font_system`.
    #[must_use]
    pub(crate) fn build(
        font_system: &mut FontSystem,
        attrs: &Attrs<'_>,
        text: &str,
        hanging: bool,
        tolerance: u32,
    ) -> Self {
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
pub(crate) struct FormEnumeration {
    pub(crate) forms: Vec<TextForm>,
    /// Список форм усечён: достигнут лимит `max_forms`, сработала защита по
    /// свободной памяти (`MIN_AVAILABLE_MEMORY_BYTES`) или аварийный потолок
    /// узлов (`SAFETY_NODE_CEILING`).
    pub(crate) truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InlineNoBreakRun {
    text: String,
    no_break: bool,
}

/// Removes no-break inline tags and makes whitespace inside them non-breaking.
///
/// The source editor text keeps the tags. Form enumeration uses this prepared text,
/// so applying a form writes `formed_text` without the control tags.
#[must_use]
pub(crate) fn prepare_inline_no_break_text(text: &str) -> String {
    inline_no_break_runs(text)
        .into_iter()
        .map(|run| run.text)
        .collect()
}

fn inline_no_break_runs(text: &str) -> Vec<InlineNoBreakRun> {
    let mut runs = Vec::<InlineNoBreakRun>::new();
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
            let tag_end = cursor + ch.len_utf8() + rel_end;
            let raw = text[cursor + ch.len_utf8()..tag_end].trim();
            let compact = raw
                .chars()
                .filter(|character| !character.is_ascii_whitespace())
                .collect::<String>()
                .to_ascii_lowercase();

            if matches!(compact.as_str(), "no-break" | "nobreak" | "nobr") {
                no_break_depth = no_break_depth.saturating_add(1);
                cursor = tag_end + '>'.len_utf8();
                continue;
            }
            if matches!(compact.as_str(), "/no-break" | "/nobreak" | "/nobr") {
                no_break_depth = no_break_depth.saturating_sub(1);
                cursor = tag_end + '>'.len_utf8();
                continue;
            }
            if compact == "/m" {
                if machine_stack.pop().unwrap_or(false) {
                    no_break_depth = no_break_depth.saturating_sub(1);
                }
                cursor = tag_end + '>'.len_utf8();
                continue;
            }
            if let Some(attrs) = parse_machine_tag(raw) {
                let protects = attrs.iter().any(|(key, _)| matches!(*key, 'j' | 'J'));
                if protects {
                    no_break_depth = no_break_depth.saturating_add(1);
                }
                machine_stack.push(protects);
                cursor = tag_end + '>'.len_utf8();
                continue;
            }
        }

        push_inline_no_break_text(
            &mut runs,
            &text[cursor..cursor + ch.len_utf8()],
            no_break_depth > 0,
        );
        cursor += ch.len_utf8();
    }

    runs
}

fn push_inline_no_break_text(runs: &mut Vec<InlineNoBreakRun>, text: &str, no_break: bool) {
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
        return;
    }
    if let Some(last) = runs.last_mut()
        && last.no_break == no_break
    {
        last.text.push_str(prepared.as_str());
        return;
    }
    runs.push(InlineNoBreakRun {
        text: prepared,
        no_break,
    });
}

/// Делит строку на ведущую висящую пунктуацию, «ядро» и хвостовую висящую
/// пунктуацию. Мягкие переносы (`SOFT_HYPHEN`) выбрасываются полностью.
#[must_use]
pub(crate) fn split_hanging_edges(line: &str) -> (String, String, String) {
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
pub(crate) fn is_christmas_tree(widths: &[u32], tol: u32) -> bool {
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
pub(crate) fn sequence_matches(widths: &[u32], preset: TextFormPreset, tol: u32) -> bool {
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
    nodes: usize,
    truncated: bool,
}

/// Перечисляет за один прогон все формы `text`, удовлетворяющие `preset`.
/// Повторов нет: каждая комбинация разрывов даёт уникальный текст. Ширины строк
/// берутся из `metric`.
#[must_use]
pub(crate) fn enumerate_forms(
    text: &str,
    preset: TextFormPreset,
    max_forms: usize,
    metric: &dyn LineWidthMetric,
) -> FormEnumeration {
    if max_forms == 0 || text.split_whitespace().next().is_none() {
        return FormEnumeration {
            forms: Vec::new(),
            truncated: false,
        };
    }

    with_default_segmenter(|seg| {
        let hyphenated = inline_no_break_runs(text)
            .into_iter()
            .map(|run| {
                if run.no_break {
                    run.text
                } else {
                    seg.soft_hyphenate_overlong(run.text.as_str())
                }
            })
            .collect::<String>();
        let blocks = seg.segment(
            &hyphenated,
            SegmentOptions {
                hanging_punctuation: false,
                preserve_edge_spaces: false,
                allow_hard_hyphen_breaks: true,
                // Строим граф один раз: служебные слова не склеиваем, а помечаем
                // стык категорией консервативности — фильтрация форм потом.
                binding: BindingMode::Annotate,
            },
        );
        if blocks.is_empty() {
            return FormEnumeration {
                forms: Vec::new(),
                truncated: false,
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
            truncated: false,
        };
        let mut lines: Vec<String> = Vec::new();
        enumerate_dfs(
            &mut ctx,
            0,
            PhaseState::START,
            0,
            0,
            Conservatism::Safe,
            0,
            u32::MAX,
            &mut lines,
        );
        FormEnumeration {
            forms: ctx.out,
            truncated: ctx.truncated,
        }
    })
}

/// Решает, нужно ли досрочно прервать перечисление (помимо лимита `max_forms`).
///
/// Два независимых условия:
/// - **Память**: раз в `MEMORY_CHECK_INTERVAL_NODES` узлов запрашиваем свободную
///   память; если её удалось измерить и она ниже `MIN_AVAILABLE_MEMORY_BYTES` —
///   останавливаемся (на Linux это и есть практический ограничитель: «перечисляем,
///   пока есть память»).
/// - **Аварийный потолок узлов** (`SAFETY_NODE_CEILING`): гарантия завершения,
///   когда память измерить нельзя (`None`, напр. не-Linux) или она никогда не
///   падает ниже порога.
fn should_stop_enumeration(ctx: &EnumContext<'_>) -> bool {
    if ctx.nodes % MEMORY_CHECK_INTERVAL_NODES == 0 {
        if let Some(bytes) = current_available_memory() {
            if bytes < MIN_AVAILABLE_MEMORY_BYTES {
                return true;
            }
        }
    }
    ctx.nodes > SAFETY_NODE_CEILING
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
    lines: &mut Vec<String>,
) {
    if ctx.out.len() >= ctx.max_forms {
        ctx.truncated = true;
        return;
    }
    ctx.nodes += 1;
    if should_stop_enumeration(ctx) {
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
                lines.push(line_text);
                if end == n {
                    finalize(
                        ctx, next_phase, cost_acc, break_count, cons_acc, new_max, new_min, lines,
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
                        lines,
                    );
                }
                lines.pop();
                if ctx.out.len() >= ctx.max_forms || should_stop_enumeration(ctx) {
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
    lines: &[String],
) {
    if ctx.preset == TextFormPreset::Lens && !(phase.ascended && phase.descended) {
        return;
    }
    let key = lines.join("\n");
    if ctx.seen.insert(key) {
        let widths: Vec<u32> = lines.iter().map(|line| ctx.metric.line_width(line)).collect();
        let median_width = median_of_widths(&widths);
        ctx.out.push(TextForm {
            lines: lines.to_vec(),
            word_break_count: break_count,
            max_width,
            min_width: if min_width == u32::MAX { 0 } else { min_width },
            median_width,
            unevenness_pct: unevenness_pct_of_widths(&widths, median_width),
            break_cost: cost_acc,
            conservatism: cons_acc,
        });
    }
}

/// Подбирает одну форму поверх scored-wrap рендера: предпочитает форму, где
/// все строки не шире `target_line_width`, минимизируя число строк; иначе —
/// форму с наименьшей максимальной шириной строки.
#[must_use]
pub(crate) fn choose_form(
    text: &str,
    preset: TextFormPreset,
    target_line_width: usize,
) -> Option<Vec<String>> {
    // Здесь шрифт недоступен — используем посимвольную метрику (как раньше).
    let metric = CharWidthMetric::new(true);
    let enumeration = enumerate_forms(text, preset, DEFAULT_MAX_FORMS, &metric);
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
        let result = enumerate_forms("a bb ccc", TextFormPreset::Widen, 1000, &CHAR_METRIC);
        assert!(!result.forms.is_empty());
        for form in &result.forms {
            assert!(sequence_matches(&widths_of(form), TextFormPreset::Widen, 0));
        }
    }

    #[test]
    fn enumerate_has_no_duplicates_in_single_pass() {
        let result =
            enumerate_forms("one two three four", TextFormPreset::FreeNoTree, 1000, &CHAR_METRIC);
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
        let result = enumerate_forms("aa b ccc dd e", TextFormPreset::Lens, 1000, &CHAR_METRIC);
        for form in &result.forms {
            assert!(is_lens(&widths_of(form), 0), "{:?}", form.lines);
        }
    }

    #[test]
    fn whitespace_only_breaks_have_zero_cost_and_no_word_breaks() {
        // Короткие слова (<4 символов) не переносятся словарём — только пробелы.
        let result = enumerate_forms("aa bb cc", TextFormPreset::FreeNoTree, 1000, &CHAR_METRIC);
        assert!(!result.forms.is_empty());
        for form in &result.forms {
            assert_eq!(form.word_break_count, 0, "{:?}", form.lines);
            assert_eq!(form.break_cost, 0, "{:?}", form.lines);
        }
    }

    #[test]
    fn prepare_inline_no_break_text_strips_tags_and_uses_nbsp() {
        assert_eq!(
            prepare_inline_no_break_text("aa <no-break>bb cc</no-break> dd"),
            "aa bb\u{00A0}cc dd"
        );
        assert_eq!(
            prepare_inline_no_break_text("aa <m j>bb cc</m> dd"),
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
        );

        assert!(!result.forms.is_empty());
        assert!(result.forms.iter().all(|form| {
            form.lines
                .iter()
                .all(|line| !line.contains("<no-break>") && !line.contains("</no-break>"))
                && !form.lines.iter().any(|line| line == "bb" || line == "cc")
        }));
    }

    #[test]
    fn single_short_token_yields_one_form_except_lens() {
        assert_eq!(
            enumerate_forms("кот", TextFormPreset::FreeNoTree, 64, &CHAR_METRIC)
                .forms
                .len(),
            1
        );
        assert!(
            enumerate_forms("кот", TextFormPreset::Lens, 64, &CHAR_METRIC)
                .forms
                .is_empty()
        );
    }

    #[test]
    fn min_median_and_peakiness_track_line_widths() {
        let result = enumerate_forms("aa bb ccccc", TextFormPreset::FreeNoTree, 1000, &CHAR_METRIC);
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
                (widths[n / 2 - 1] + widths[n / 2] + 1) / 2
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
        let result = enumerate_forms("aa bb ccccc dd", TextFormPreset::FreeNoTree, 1000, &CHAR_METRIC);
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
            enumerate_forms("кот на ветке", TextFormPreset::FreeNoTree, 1000, &CHAR_METRIC);
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
            enumerate_forms("кот на ветке", TextFormPreset::FreeNoTree, 1000, &CHAR_METRIC);
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
        let started = std::time::Instant::now();
        let result = with_memory_source(Some(low), || {
            enumerate_forms(BIG_TEXT, TextFormPreset::FreeNoTree, usize::MAX, &CHAR_METRIC)
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
            started.elapsed() < std::time::Duration::from_secs(5),
            "memory guard must return promptly"
        );
    }

    #[test]
    fn memory_guard_disabled_lets_enumeration_complete() {
        // Памяти «вдоволь» — защита по памяти молчит, маленький вход исчерпывается.
        let high = MIN_AVAILABLE_MEMORY_BYTES * 16;
        let result = with_memory_source(Some(high), || {
            enumerate_forms("aa bb cc", TextFormPreset::FreeNoTree, usize::MAX, &CHAR_METRIC)
        });
        assert!(!result.truncated, "small input must complete");
        assert!(!result.forms.is_empty());
    }

    #[test]
    fn max_forms_cap_still_enforced() {
        // Явный маленький cap по-прежнему действует (путь choose_form не затронут).
        let result = with_memory_source(Some(MIN_AVAILABLE_MEMORY_BYTES * 16), || {
            enumerate_forms(BIG_TEXT, TextFormPreset::FreeNoTree, 5, &CHAR_METRIC)
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
}
