/*
File: crates/ms-text-render/src/wrap/forms/width_metrics.rs

Purpose:
Line-width metrics of the form search: the char-count metric, the font-measured
`GlyphWidths` table and the pen-faithful `MetricKerning` pair correction, plus the
hanging-edge split every metric measures the line core with. `GlyphWidths::build` is
one of the CUSTOM KERNING sites (crate `MODULE_README.md`).

Key items:
- impl `LineWidthMetric` for `CharWidthMetric` and `GlyphWidths`
- GlyphWidths::build(), measure_pair_units(), measure_units()
- split_hanging_edges(), metric_full_text() / metric_core_text(), blend_hanging_width()

Notes:
The metric types themselves are declared in `forms/mod.rs`.
*/

use std::collections::{BTreeSet, HashMap};
use cosmic_text::{Attrs, Buffer, FontSystem, Metrics, Shaping};
use crate::wrap::is_hanging_punctuation;
use ms_text_util::text_punctuation::clamp_hanging_weight;
use crate::font_registry::CustomKerningMap;
use crate::pair_advance::{
    KerningSettings, PairAdvanceModel, PairGlyph, RunOrientation, StepDirection, TrackingBasis,
    effective_spacing_percent,
};
use crate::types::KerningMode;
use ms_text_util::segmentation::SOFT_HYPHEN;
use super::{
    CharWidthMetric, GlyphWidths, InlineTagScope, LineWidthMetric, MetricKerning, PairUnits,
    WIDTH_METRIC_EM,
};
use super::inline_tags::prepare_inline_no_break_text;
use super::quality::round_to_u32;

/// Видимый текст строки для замера: без мягких переносов, без краевых пробелов.
#[must_use]
fn metric_full_text(line: &str) -> String {
    line.trim().chars().filter(|&ch| ch != SOFT_HYPHEN).collect()
}

/// «Ядро» строки: [`metric_full_text`] без ведущей/хвостовой висящей пунктуации.
#[must_use]
fn metric_core_text(line: &str) -> String {
    let (_, core, _) = split_hanging_edges(line);
    core
}

/// Смешивает замер полной строки и замер её ядра по силе висящей пунктуации
/// `weight` (`0.0..=1.0`): `0.0` — вся строка, `1.0` — только ядро.
///
/// Концы диапазона точны: при `weight == 0.0` возвращается ровно `full`, при
/// `1.0` — ровно `core`. Промежуточные значения линейно интерполируются и
/// округляются до целой единицы метрики.
#[must_use]
pub(super) fn blend_hanging_width(full: u32, core: u32, weight: f32) -> u32 {
    if weight <= 0.0 {
        return full;
    }
    if weight >= 1.0 {
        return core;
    }
    let weight = f64::from(weight);
    // `round_to_u32` сам округляет и насыщает: результат лежит между `full` и `core`,
    // то есть внутри диапазона, но приведение остаётся в одном проверенном месте.
    round_to_u32(f64::from(full) * (1.0 - weight) + f64::from(core) * weight)
}

/// Число видимых символов строки как единица посимвольной метрики.
///
/// Строк длиннее `u32::MAX` символов не бывает; такая насытится, а не завернётся.
#[must_use]
fn metric_char_units(text: &str) -> u32 {
    u32::try_from(text.chars().count()).unwrap_or(u32::MAX)
}

impl CharWidthMetric {
    /// `hanging` — сила висящей пунктуации `0.0..=1.0`: `0.0` считает все символы
    /// строки, `1.0` — только её ядро, промежуточное значение — линейно между.
    /// Значение нормализуется (`NaN` = `0.0`), выход за диапазон обрезается.
    #[must_use]
    pub fn new(hanging: f32) -> Self {
        Self {
            hanging: clamp_hanging_weight(hanging),
        }
    }
}

impl LineWidthMetric for CharWidthMetric {
    fn line_width(&self, line: &str) -> u32 {
        if self.hanging <= 0.0 {
            return metric_char_units(metric_full_text(line).as_str());
        }
        let core = metric_char_units(metric_core_text(line).as_str());
        if self.hanging >= 1.0 {
            return core;
        }
        blend_hanging_width(
            metric_char_units(metric_full_text(line).as_str()),
            core,
            self.hanging,
        )
    }

    fn tolerance(&self) -> u32 {
        0
    }
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
    ///
    /// `custom_kerning` — the user-authored kerning overrides keyed by registered face,
    /// bound to `font_system`'s faces exactly as the renderer binds them
    /// (`CustomKerningMap::record` after `load_font_content`; an empty map for a font
    /// without overrides). A pair the pen overrides REPLACES the font's own kerning for
    /// it: `kerns` holds precisely the font's own pair delta in the same per-mille unit
    /// the overrides use (`WIDTH_METRIC_EM` == 1000), so the replacement is an exact
    /// overwrite. Whether the pen overrides a pair is decided by the pen's own guard
    /// over the two glyphs the pair shapes into
    /// (`pair_advance::PairAdvanceModel::override_per_mille`), so a pair drawn by a
    /// fallback face or fused into a ligature is measured like any other.
    ///
    /// Only pairs that occur are measured (plus `(ch, '-')` for the wrap hyphen), so an
    /// unused table entry costs nothing.
    #[must_use]
    pub fn build(
        font_system: &mut FontSystem,
        attrs: &Attrs<'_>,
        form_source_text: &str,
        hanging: f32,
        tolerance: u32,
        scope: InlineTagScope,
        kerning: MetricKerning<'_>,
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
            match measure_pair_units(font_system, attrs, &scratch, kerning, &advances) {
                // A kern the pen decides for this pair (an override, `Fixed`, tracking).
                // `0` must survive as an entry: an override of `0.0` cancels the font's
                // own kerning, which is why the `delta != 0` filter below is not applied.
                PairUnits::Decided(delta) => {
                    kerns.insert((a, b), delta);
                }
                PairUnits::Shaped(pair_width) => {
                    let sum = i64::from(advances.get(&a).copied().unwrap_or(0))
                        + i64::from(advances.get(&b).copied().unwrap_or(0));
                    let delta = (i64::from(pair_width) - sum).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
                    if delta != 0 {
                        kerns.insert((a, b), delta);
                    }
                }
            }
        }

        Self {
            advances,
            kerns,
            hanging: clamp_hanging_weight(hanging),
            tolerance,
        }
    }
}

impl GlyphWidths {
    /// Ширина готового текста: сумма ширин глифов плюс поправки кернинга соседних пар.
    #[must_use]
    fn measured_width(&self, text: &str) -> u32 {
        let mut width: i64 = 0;
        let mut prev: Option<char> = None;
        for ch in text.chars() {
            width += i64::from(self.advances.get(&ch).copied().unwrap_or(0));
            if let Some(p) = prev {
                width += i64::from(self.kerns.get(&(p, ch)).copied().unwrap_or(0));
            }
            prev = Some(ch);
        }
        width.max(0) as u32
    }
}

impl LineWidthMetric for GlyphWidths {
    fn line_width(&self, line: &str) -> u32 {
        if self.hanging <= 0.0 {
            return self.measured_width(metric_full_text(line).as_str());
        }
        let core = self.measured_width(metric_core_text(line).as_str());
        if self.hanging >= 1.0 {
            return core;
        }
        // Смешиваются два ЗАМЕРА, а не вычитается ширина краёв: при полном подвесе
        // вместе с краевыми знаками исчезают и две пары кернинга на их стыке с ядром,
        // поэтому только так конец диапазона совпадает с прежним поведением.
        blend_hanging_width(
            self.measured_width(metric_full_text(line).as_str()),
            core,
            self.hanging,
        )
    }

    fn tolerance(&self) -> u32 {
        self.tolerance
    }
}

/// A two-character `pair` shaped at `WIDTH_METRIC_EM`, measured the way the pen draws it
/// under `kerning`.
///
/// When the pair shapes into exactly two glyphs, the pen's own decision applies:
/// - plain spacing (`Auto` / `Optical`, no tracking): an overridden pair is
///   `Decided(authored per-mille)` — the unit already matches — and any other pair is
///   `Shaped` (the font's own kern, derived by the caller exactly as before);
/// - otherwise: `Decided(round(|step| + tracking) - advance of the spanning glyph)`, where
///   `step` is `pair_advance`'s `resolve` (`Fixed` own advance, or the override, or the
///   shaped step) and the tracking is the pen's `TrackingBasis::HorizontalPen` term — so
///   `advances` + `kerns` sum to the pen's line width.
///
/// A pair that shapes into any other glyph count (a ligature, a split) is `Shaped`: the
/// pen places no step, and no tracking, inside it.
fn measure_pair_units(
    font_system: &mut FontSystem,
    attrs: &Attrs<'_>,
    pair: &str,
    kerning: MetricKerning<'_>,
    advances: &HashMap<char, u32>,
) -> PairUnits {
    let mut buffer = Buffer::new(font_system, Metrics::new(WIDTH_METRIC_EM, WIDTH_METRIC_EM));
    buffer.set_size(font_system, None, None);
    buffer.set_text(font_system, pair, attrs, Shaping::Advanced);
    buffer.shape_until_scroll(font_system, false);
    let model = PairAdvanceModel::new(kerning.custom_kerning);
    let mut width = 0.0f32;
    let mut decided = None;
    for run in buffer.layout_runs() {
        width = width.max(run.line_w);
        let [first, second] = run.glyphs else {
            continue;
        };
        if decided.is_some() {
            continue;
        }
        let prev = PairGlyph::from_run(run.text, first);
        let cur = PairGlyph::from_run(run.text, second);
        decided = if kerning.is_plain() {
            model.override_per_mille(prev, cur).map(|offset| {
                offset.round().clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i32
            })
        } else {
            let shaped_step = second.x - first.x;
            let step = model.resolve(
                font_system,
                kerning.pen_mode(),
                StepDirection::FollowRun,
                prev,
                cur,
                shaped_step,
            );
            let tracking = kerning.tracking_units(TrackingBasis::HorizontalPen {
                shaped_step_px: shaped_step,
                prev_w_px: first.w,
                default_advance_px: WIDTH_METRIC_EM * 0.5,
            });
            let spanning_char = match RunOrientation::of_step(shaped_step, first) {
                RunOrientation::LeftToRight => prev.cluster_char,
                RunOrientation::RightToLeft => cur.cluster_char,
            };
            let spanning_advance = spanning_char
                .and_then(|ch| advances.get(&ch).copied())
                .unwrap_or(0);
            let kern = ((step.step_px().abs() + tracking).round() as i64 - i64::from(spanning_advance))
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
            Some(kern)
        };
    }
    match decided {
        Some(delta) => PairUnits::Decided(delta),
        None => PairUnits::Shaped(width.round().max(0.0) as u32),
    }
}

impl<'a> MetricKerning<'a> {
    /// The pen's spacing for a block rendered with these `TextRenderParams` values.
    #[must_use]
    pub fn new(
        custom_kerning: &'a CustomKerningMap,
        kerning_mode: KerningMode,
        kerning_px: f32,
        kerning_percent: f32,
        font_size_px: f32,
        glyph_width_percent: f32,
    ) -> Self {
        let font_size_px = if font_size_px.is_finite() { font_size_px.max(1.0) } else { 1.0 };
        Self {
            custom_kerning,
            mode: kerning_mode,
            settings: KerningSettings {
                mode: kerning_mode,
                spacing_px: kerning_px.clamp(-300.0, 300.0) / font_size_px * WIDTH_METRIC_EM,
                spacing_percent: effective_spacing_percent(kerning_percent, glyph_width_percent),
                custom_pairs: false,
            },
            width_scale: (glyph_width_percent / 100.0).clamp(0.01, 3.0),
        }
    }

    /// Plain `Auto` spacing (no tracking, no glyph scale) over `custom_kerning`: the
    /// metric for a caller that has no spacing settings.
    #[must_use]
    pub fn auto(custom_kerning: &'a CustomKerningMap) -> Self {
        Self::new(custom_kerning, KerningMode::Auto, 0.0, 0.0, 100.0, 100.0)
    }

    /// Whether the pen keeps the shaped step for every non-overridden pair (no `Fixed`, no
    /// tracking): then the metric keeps measuring the font's own kerning by shaping.
    fn is_plain(self) -> bool {
        self.mode != KerningMode::Fixed
            && self.settings.spacing_px.abs() <= f32::EPSILON
            && self.settings.spacing_percent.abs() <= f32::EPSILON
    }

    /// The mode the per-pair step is resolved with (`Optical` -> `Auto`, see the type doc).
    fn pen_mode(self) -> KerningMode {
        match self.mode {
            KerningMode::Fixed => KerningMode::Fixed,
            KerningMode::Auto | KerningMode::Optical => KerningMode::Auto,
        }
    }

    /// The pen's tracking for one step in metric units, divided by the width scale.
    fn tracking_units(self, basis: TrackingBasis) -> f32 {
        self.settings.tracking_px(basis) / self.width_scale
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
