/*
File: crates/ms-text-render/src/wrap/forms/mod.rs

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

This file keeps the public types (presets, `TextForm`, the metric types, `InlineTagScope`,
`FormEnumeration`, the search parameters), the constants, the memory-probe source and
the public re-exports. Children (map: `MODULE_README.md` in this directory):
- `width_metrics.rs` — `CharWidthMetric` / `GlyphWidths` (`GlyphWidths::build`, a CUSTOM
  KERNING site) and the hanging-edge split;
- `inline_tags.rs` — tag stripping for the search and `reapply_inline_tags_to_form_text`;
- `shape_match.rs` — shape predicates and the per-preset phase machine;
- `enumerate.rs` — `enumerate_forms` (the `choose_form` path);
- `params.rs` — corridor ladder, hyphen budget, quality weights, `FormSearchParams`;
- `quality.rs` — width statistics and the quality score;
- `search.rs` — the ranked `search_forms` and `choose_form`.

Pen spacing (`MetricKerning`):
`GlyphWidths::build` takes a `MetricKerning` — the render's face-keyed
`CustomKerningMap`, kerning mode and manual tracking — so the form search sizes lines
against the widths the renderer actually draws. Under `Fixed` or tracking each pair's
kern is the pen's own step plus tracking minus the spanning glyph's advance
(`measure_pair_units`); `Optical` is measured like `Auto`. Under plain spacing an
overridden pair REPLACES the font's own pair delta with the authored one. WHICH pairs are overridden is the pen's own
decision (`pair_advance::PairAdvanceModel::override_per_mille` over the two glyphs the
pair shapes into: same face carrying a table, one `char` per cluster), so a pair that
falls through to a fallback face or fuses into a ligature is measured like any other.
The units already agree — the metric shapes at `WIDTH_METRIC_EM` (1000) px per em and
the offsets are per-mille of the em — so the substitution is exact, and a `0.0` entry
is STORED (it cancels the font's own pair) rather than dropped by the `delta != 0`
filter that applies to measured deltas.

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
Висящая пунктуация задаётся СИЛОЙ `0.0..=1.0`, а не флагом: при `0.0` ведущая и
хвостовая висящая пунктуация (и дефис переноса) идёт в ширину целиком, при `1.0` —
не идёт вовсе, между ними ширина линейно смешивается между замером всей строки и
замером её ядра (`blend_hanging_width`). Смешиваются именно два ЗАМЕРА, а не
вычитается ширина краёв: вместе с краевыми знаками исчезают и пары кернинга на их
стыке с ядром, поэтому иначе конец диапазона не совпал бы с прежним поведением.
Сравнение ширин в предикатах формы идёт с допуском (`tolerance`), чтобы
суб-глифовый джиттер не создавал ложных подъёмов/спусков.

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

mod width_metrics;
mod inline_tags;
mod shape_match;
mod enumerate;
mod params;
mod quality;
mod search;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use crate::font_registry::CustomKerningMap;
use crate::pair_advance::KerningSettings;
use crate::types::KerningMode;
use ms_text_util::segmentation::{Block, Conservatism};

pub use width_metrics::split_hanging_edges;
pub use inline_tags::{reapply_inline_tags_to_form_text, scopes_strip_alike};
pub use shape_match::{is_christmas_tree, sequence_matches};
pub use enumerate::enumerate_forms;
pub use params::default_corridor_ladder;
pub use search::{choose_form, search_forms};

/// Максимум перечисляемых форм за один прогон (защита от комбинаторного взрыва).
pub const DEFAULT_MAX_FORMS: usize = 600;

/// Нижний порог свободной памяти, при котором перечисление останавливается
/// досрочно (`truncated`). ≈ 512 MiB — запас на последующую сортировку/клон/
/// отрисовку результатов, чтобы остановиться заметно раньше OOM-killer'а.
const MIN_AVAILABLE_MEMORY_BYTES: u64 = 512 * 1024 * 1024;

/// How often (in DFS nodes) free memory is checked: the OS probe is a file read or
/// a few syscalls, too costly for every node, so it runs once per N nodes.
const MEMORY_CHECK_INTERVAL_NODES: u64 = 8192;

/// Emergency ceiling on DFS nodes: guarantees termination when free memory cannot
/// be observed (`available_memory_bytes()` returned `None`) OR never drops below the
/// threshold. When memory is measurable it is the practical limit, not this ceiling
/// (in effect "no count limit, only a memory limit").
const SAFETY_NODE_CEILING: u64 = 50_000_000;

/// Available ("OOM-relevant", reclaimable) memory in bytes, from the one OS memory
/// probe `ms_memory::current_memory_availability` (Linux/Android `MemAvailable`,
/// Windows `ullAvailPhys`, macOS free+inactive+speculative pages; no subprocess).
/// `None` means unknown (the probe logs why); then only `SAFETY_NODE_CEILING`
/// bounds the enumeration.
fn available_memory_bytes() -> Option<u64> {
    ms_memory::current_memory_availability().map(|availability| availability.available_bytes)
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
/// on the UI-string catalog (see `dev-docs/i18n_exclusions.md` §F), so a localizable
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

/// Посимвольная метрика (число символов строки с учётом веса висящих краёв).
/// Запасной вариант без шрифта и прежнее поведение окна форм.
pub struct CharWidthMetric {
    /// Сила висящей пунктуации `0.0..=1.0` (`TextRenderParams::hanging_weight`).
    hanging: f32,
}

/// Попиксельная метрика: ширины глифов + поправки кернинга соседних пар,
/// заранее измеренные шейпером для алфавита текста.
pub struct GlyphWidths {
    advances: HashMap<char, u32>,
    kerns: HashMap<(char, char), i32>,
    /// Сила висящей пунктуации `0.0..=1.0` (`TextRenderParams::hanging_weight`).
    hanging: f32,
    tolerance: u32,
}

/// What the metric learned about one pair.
enum PairUnits {
    /// The pen decides this pair's kern itself (an override, `Fixed`, tracking): the
    /// signed kern in metric units, to be stored as is.
    Decided(i32),
    /// The plain shaped width of the pair in metric units; the kern is derived from it.
    Shaped(u32),
}

/// How the pen spaces a line, expressed for [`GlyphWidths`]: the user-authored overrides
/// (keyed by registered face), the kerning mode and the block's manual tracking.
///
/// Built once per metric with [`Self::new`] from the SAME values the render gets
/// (`TextRenderParams::{kerning_mode, kerning_px, kerning_percent, font_size_px,
/// glyph_width_percent}`), so the form search sizes lines with the widths the pen
/// draws. Unit convention: the metric measures widths BEFORE the horizontal glyph scale
/// and the form window divides the line height by that scale
/// ([`FormSearchParams::line_height_units`]), so a drawn px of tracking is converted to
/// `px / font_size_px * 1000 / width_scale` metric units.
///
/// `KerningMode::Optical` is measured like `Auto` here: its step depends on the median ink
/// gap of the whole DRAWN line, which a per-pair metric cannot express (the line wrap
/// measures it exactly; the form metric does not). Faux bold is not measured either: its
/// advance growth depends on each glyph's outline (the counter flag of `outward_only`),
/// which this per-character metric does not extract; the line wrap does measure it.
#[derive(Debug, Clone, Copy)]
pub struct MetricKerning<'a> {
    custom_kerning: &'a CustomKerningMap,
    mode: KerningMode,
    /// Tracking settings in metric units, before the width-scale division.
    settings: KerningSettings,
    /// Horizontal glyph scale (`glyph_width_percent / 100`, clamped like the renderer).
    width_scale: f32,
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
/// tags at all (`pipeline/`, `enable_inline_style_tags`), so `<b>` is literal text the
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

// --- Предикаты форм (по последовательности ширин строк) -------------------

/// Сравнение двух ширин с допуском: разница `≤ tol` считается равенством.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WidthCmp {
    Less,
    Equal,
    Greater,
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

enum Step {
    /// Строка валидна, продолжаем с новым состоянием фазы.
    Ok(PhaseState),
    /// Эта длина строки не подходит, но более длинная может — пробуем дальше.
    SkipEnd,
    /// Дальше по этой ветке всё хуже — отсекаем остаток.
    PruneRest,
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

/// Enumeration limiter: free memory plus the emergency node ceiling.
///
/// Two independent conditions:
/// - **Memory**: at most once per `MEMORY_CHECK_INTERVAL_NODES` nodes free memory is
///   probed; when it is measurable and below `MIN_AVAILABLE_MEMORY_BYTES` the
///   enumeration stops (the practical limit: "enumerate while memory lasts").
/// - **Emergency node ceiling** (`SAFETY_NODE_CEILING`): guarantees termination when
///   memory cannot be measured (`None`) or never drops below the threshold.
///
/// A separate `next_check_at` counter (not `nodes % N == 0`) is needed because the
/// check runs after every return from a child while the node counter does not grow
/// inside the `for end` loop: with a modulo test one "node out of 8192" probed the
/// OS memory up to `n` times in a row.
///
/// A tripped memory guard latches (`exhausted`): the stack unwind must keep
/// stopping instead of resuming the enumeration until the next check.
#[derive(Debug)]
struct MemoryProbe {
    /// Номер узла, начиная с которого разрешён следующий замер памяти.
    next_check_at: u64,
    /// Замер уже показал нехватку памяти — все дальнейшие ответы `true`.
    exhausted: bool,
}

/// Накопитель текущей ветки перебора: строки формы и их ширины, в одном порядке
/// и всегда одной длины. Ширина строки уже посчитана на узле DFS, поэтому
/// `finalize` не меряет строки заново.
struct LineTrail {
    lines: Vec<String>,
    widths: Vec<u32>,
}

/// The accumulated state of one enumeration branch: the pruning phase, the summed
/// break cost, the in-word break count, the worst joint conservatism and the width
/// range of the lines placed so far (`min_width == u32::MAX`: no line yet). Small
/// and `Copy`: the recursion passes it by value, each child gets its own copy.
#[derive(Clone, Copy)]
struct DfsPath {
    phase: PhaseState,
    cost_acc: u32,
    break_count: usize,
    cons_acc: Conservatism,
    max_width: u32,
    min_width: u32,
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
    /// (зеркало `pipeline::render_text_to_image`, где считаются
    /// `line_spacing_percent`/`extra_line_spacing_px`; сама формула связи —
    /// `pair_advance::effective_spacing_percent`). Обратите внимание: с
    /// `glyph_height_percent` связан ТОЛЬКО глобальный параметр; строчный тег
    /// `<stretching=W%,H%>` в интерлиньяж не входит (см. контракт GLYPH HEIGHT
    /// SCALE в `ms-text-render/src/MODULE_README.md`), поэтому поиск форм его и
    /// не учитывает. `units_per_em` — 1000 для [`GlyphWidths`] и
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

/// Разложенная оценка формы: итоговое `Q` и отдельно шероховатость (её панель
/// показывает как самостоятельную характеристику).
#[derive(Debug, Clone, Copy, PartialEq)]
struct QualityBreakdown {
    /// Итоговое `Q` — МЕНЬШЕ ЛУЧШЕ, `0.0` — идеально ровная форма без переносов.
    quality: f64,
    /// Терм `rough` в долях медианы.
    roughness: f64,
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
