/*
File: crates/ms-text-render/src/wrap/forms/enumerate.rs

Purpose:
The exhaustive form enumeration (`enumerate_forms`, the `choose_form` path): form-text
preparation and block segmentation (protected junctions kept together), then a DFS
bounded by `max_forms`, free memory and the node ceiling.

Key functions:
- prepare_form_text(), segment_form_blocks(), glue_protected_junctions()
- enumerate_forms(), enumerate_dfs(), finalize()
- `MemoryProbe`, `LineTrail`, `DfsPath` impls

Notes:
The memory probe source (`current_available_memory`, test-substitutable) stays in
`forms/mod.rs`.
*/

use std::collections::HashSet;
use std::ops::Range;
use ms_text_util::segmentation::{
    BindingMode, Block, Conservatism, NON_BREAKING_SPACE, SOFT_HYPHEN, SegmentOptions,
    build_line_text_and_units, with_default_segmenter,
};
use super::{
    DfsPath, EnumContext, FormEnumeration, InlineTagScope, LineTrail, LineWidthMetric,
    MEMORY_CHECK_INTERVAL_NODES, MIN_AVAILABLE_MEMORY_BYTES, MemoryProbe, PhaseState,
    PreparedFormText, QualityWeights, SAFETY_NODE_CEILING, Step, TextForm, TextFormPreset,
    UNSCORED_QUALITY_MILLI, current_available_memory,
};
use super::inline_tags::strip_inline_tags;
use super::shape_match::advance_step;
use super::quality::{median_of_widths, roughness_pct_of_widths, unevenness_pct_of_widths};

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
pub(super) fn segment_form_blocks(
    seg: &dyn ms_text_util::segmentation::Segmenter,
    text: &str,
    scope: InlineTagScope,
) -> Vec<Block> {
    let prepared = prepare_form_text(seg, text, scope);
    let blocks = seg.segment(
        prepared.text.as_str(),
        SegmentOptions {
            hanging_punctuation: 0.0,
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
        enumerate_dfs(&mut ctx, 0, DfsPath::START, &mut trail);
        FormEnumeration {
            forms: ctx.out,
            truncated: ctx.truncated,
            nodes_visited: ctx.nodes,
        }
    })
}

impl MemoryProbe {
    /// Свежий ограничитель: первый замер — на узле `MEMORY_CHECK_INTERVAL_NODES`.
    pub(super) const fn new() -> Self {
        Self {
            next_check_at: MEMORY_CHECK_INTERVAL_NODES,
            exhausted: false,
        }
    }

    /// Нужно ли прервать перебор, находясь на узле номер `nodes`.
    pub(super) fn should_stop(&mut self, nodes: u64) -> bool {
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

impl DfsPath {
    /// The empty branch: no line yet, the start phase.
    pub(super) const START: Self = Self {
        phase: PhaseState::START,
        cost_acc: 0,
        break_count: 0,
        cons_acc: Conservatism::Safe,
        max_width: 0,
        min_width: u32::MAX,
    };
}

/// Recursive enumeration: extends branch `path` with every line that starts at
/// block `start`; complete forms go to `finalize`.
fn enumerate_dfs(
    ctx: &mut EnumContext<'_>,
    start: usize,
    path: DfsPath,
    trail: &mut LineTrail,
) {
    let DfsPath {
        phase,
        cost_acc,
        break_count,
        cons_acc,
        max_width,
        min_width,
    } = path;
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
                        ctx,
                        DfsPath {
                            phase: next_phase,
                            max_width: new_max,
                            min_width: new_min,
                            ..path
                        },
                        trail,
                    );
                } else {
                    let joint = &ctx.blocks[end - 1].joint;
                    enumerate_dfs(
                        ctx,
                        end,
                        DfsPath {
                            phase: next_phase,
                            cost_acc: cost_acc + joint.break_cost,
                            break_count: break_count + usize::from(joint.word_break),
                            cons_acc: cons_acc.max(joint.conservatism),
                            max_width: new_max,
                            min_width: new_min,
                        },
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

/// Records the complete branch `path` (its lines and widths in `trail`) as a form
/// when it passes the preset's final check and was not seen before.
fn finalize(ctx: &mut EnumContext<'_>, path: DfsPath, trail: &LineTrail) {
    let DfsPath {
        phase,
        cost_acc,
        break_count,
        cons_acc,
        max_width,
        min_width,
    } = path;
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
