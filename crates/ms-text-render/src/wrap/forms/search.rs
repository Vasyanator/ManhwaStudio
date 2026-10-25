/*
File: crates/ms-text-render/src/wrap/forms/search.rs

Purpose:
The ranked form search (`search_forms`): one bounded DFS per form height inside the
width corridor, with the aspect ceiling, hyphen budget and node budgets, width memo
and bucketed result keeping; `choose_form` picks one form for the renderer.

Key functions:
- search_forms(), choose_form()
- run_search(), search_dfs(), emit_form()
- `WidthMemo`, `BucketPlan`, `PartialForm`, `SearchContext` impls
*/

use std::collections::HashMap;
use ms_text_util::segmentation::{
    Block, Conservatism, build_line_text_and_units, with_default_segmenter,
};
use super::{
    BucketPlan, CharWidthMetric, DEFAULT_MAX_FORMS, DENSE_WIDTH_MEMO_MAX_CELLS, FormEnumeration,
    FormSearchParams, HyphenBudget, InlineTagScope, LineWidthMetric, MemoryProbe, PartialForm,
    PartialSnapshot, PhaseState, SPARSE_WIDTH_MEMO_MAX_ENTRIES, SearchContext, Step, TextForm,
    TextFormPreset, WidthMemo,
};
use super::shape_match::advance_step;
use super::enumerate::{enumerate_forms, segment_form_blocks};
use super::quality::{
    count_as_f64, form_quality, median_of_widths, round_to_u32, unevenness_pct_of_widths,
};

impl WidthMemo {
    /// Таблица для текста из `n` блоков (см. описание типа: плотная или
    /// разрежённая — решает [`DENSE_WIDTH_MEMO_MAX_CELLS`]).
    #[must_use]
    pub(super) fn new(n: usize) -> Self {
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
    pub(super) fn get(&self, start: usize, end: usize) -> Option<u32> {
        match self {
            Self::Dense { widths, stride } => widths.get(start * stride + (end - 1)).copied()?,
            Self::Sparse(map) => map.get(&(start, end)).copied(),
        }
    }

    /// Запоминает ширину строки `[start, end)`. Разрежённая таблица, дойдя до
    /// потолка записей, перестаёт запоминать (см. [`SPARSE_WIDTH_MEMO_MAX_ENTRIES`]).
    pub(super) fn insert(&mut self, start: usize, end: usize, width: u32) {
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
    let metric = CharWidthMetric::new(1.0);
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
