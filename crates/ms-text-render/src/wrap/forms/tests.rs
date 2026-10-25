//! Unit tests of the text-form search (`wrap/forms/mod.rs` and its children), including the
//! test memory source `with_memory_source` / `test_available_memory` that
//! `current_available_memory` reads under `cfg(test)`.

use super::*;
use super::{enumerate::*, inline_tags::*, quality::*, search::*, shape_match::*, width_metrics::*};
use ms_text_util::segmentation::{build_line_text_and_units, with_default_segmenter};
use std::cell::Cell;

thread_local! {
    /// Подменяемый источник свободной памяти для тестов защиты по памяти.
    /// `None` (по умолчанию) → реальный `available_memory_bytes()`.
    /// `Some(value)` → защита видит ровно `value` (где `value` сам `Option`).
    #[cfg_attr(target_os = "android", expect(clippy::missing_const_for_thread_local, reason = "clippy false positive on targets without native TLS: std's os-TLS thread_local wraps the const initializer in a plain init fn"))]
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
const CHAR_METRIC: CharWidthMetric = CharWidthMetric { hanging: 1.0 };

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
    let metric = CharWidthMetric::new(0.0);
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

#[test]
fn the_char_metric_weights_the_hanging_edges_linearly() {
    // «Aa!» — ведущий знак «, ядро "Aa", хвостовая серия "!»": 5 символов, 2 в ядре.
    const LINE: &str = "«Aa!»";
    assert_eq!(CharWidthMetric::new(0.0).line_width(LINE), 5);
    assert_eq!(CharWidthMetric::new(1.0).line_width(LINE), 2);
    // Промежуточный вес — линейно между концами: 5 - 0.5*3 = 3.5 -> 4.
    assert_eq!(CharWidthMetric::new(0.5).line_width(LINE), 4);
    assert_eq!(CharWidthMetric::new(0.25).line_width(LINE), 4);
    assert_eq!(CharWidthMetric::new(0.75).line_width(LINE), 3);
    // Ширина не растёт с весом — иначе перебор форм дёргался бы на слайдере.
    let mut prev = u32::MAX;
    for weight in [0.0f32, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0] {
        let width = CharWidthMetric::new(weight).line_width(LINE);
        assert!(width <= prev, "width must not grow with the weight at {weight}");
        prev = width;
    }
    // Строка без висящих краёв не зависит от веса вовсе.
    for weight in [0.0f32, 0.5, 1.0] {
        assert_eq!(CharWidthMetric::new(weight).line_width("Aa"), 2);
    }
}

#[test]
fn a_broken_hanging_weight_is_normalized_by_the_metric() {
    const LINE: &str = "«Aa!»";
    // NaN — это «выключено», а не паника и не половина.
    assert_eq!(CharWidthMetric::new(f32::NAN).line_width(LINE), 5);
    assert_eq!(CharWidthMetric::new(-4.0).line_width(LINE), 5);
    assert_eq!(CharWidthMetric::new(9.0).line_width(LINE), 2);
}

#[test]
fn the_width_blend_is_exact_at_both_ends() {
    // Контракт `blend_hanging_width`: концы диапазона возвращаются без
    // арифметики, иначе метрика перестала бы совпадать с прежним поведением.
    assert_eq!(blend_hanging_width(100, 60, 0.0), 100);
    assert_eq!(blend_hanging_width(100, 60, 1.0), 60);
    assert_eq!(blend_hanging_width(100, 60, 0.5), 80);
    assert_eq!(blend_hanging_width(100, 60, 0.25), 90);
    // Вырожденный случай: нечего подвешивать — вес ни на что не влияет.
    assert_eq!(blend_hanging_width(42, 42, 0.37), 42);
}

/// Fixture font for the width metric: the same tracked Arimo caller font the
/// pipeline tests use (`crate::test_fixtures`). Returns the system, the family name and the registered face id (what
/// an override table binds to).
fn metric_fixture_system() -> (cosmic_text::FontSystem, String, cosmic_text::fontdb::ID) {
    use cosmic_text::{FontSystem, fontdb};
    let path = crate::test_fixtures::caller_font_path();
    let bytes = std::fs::read(path).expect("fixture font bytes");
    let mut db = fontdb::Database::new();
    db.load_font_data(bytes);
    let face = db.faces().next().expect("fixture face");
    let face_id = face.id;
    let family = face
        .families
        .first()
        .map(|(name, _language)| name.clone())
        .expect("fixture family name");
    (
        FontSystem::new_with_locale_and_db("en-US".to_string(), db),
        family,
        face_id,
    )
}

/// `pairs` bound to `face`, as `CustomKerningMap::record` binds a content's table.
fn map_on(face: cosmic_text::fontdb::ID, pairs: &[(char, char, f32)]) -> CustomKerningMap {
    let mut map = CustomKerningMap::default();
    map.insert(
        face,
        std::sync::Arc::new(crate::font_provider::CustomKerningTable::from_pairs(pairs.iter().copied())),
        "fixture",
    );
    map
}

/// The form-search width metric must REPLACE the font's own pair kerning with
/// the authored value, in the same per-mille unit it already measures in. If it
/// did not, the form window would size lines against widths the renderer does
/// not draw.
#[test]
fn the_glyph_width_metric_replaces_the_fonts_pair_with_the_authored_one() {
    use cosmic_text::{Attrs, Family, Metrics};

    let (mut font_system, family, face) = metric_fixture_system();
    let attrs = Attrs::new()
        .family(Family::Name(family.as_str()))
        .metrics(Metrics::new(WIDTH_METRIC_EM, WIDTH_METRIC_EM));

    let plain = GlyphWidths::build(
        &mut font_system,
        &attrs,
        "AV",
        0.0,
        DEFAULT_WIDTH_TOLERANCE,
        InlineTagScope::NoBreakOnly,
        MetricKerning::auto(&CustomKerningMap::default()),
    );
    let kerned = GlyphWidths::build(
        &mut font_system,
        &attrs,
        "AV",
        0.0,
        DEFAULT_WIDTH_TOLERANCE,
        InlineTagScope::NoBreakOnly,
        MetricKerning::auto(&map_on(face, &[('A', 'V', -120.0)])),
    );

    // The font's own `AV` kern is what gets replaced, so it must be non-zero for
    // this test to mean anything.
    let own = plain.kerns.get(&('A', 'V')).copied().unwrap_or(0);
    assert!(own < 0, "the Arimo fixture must kern 'AV' itself; got {own}");
    assert_eq!(
        kerned.kerns.get(&('A', 'V')).copied(),
        Some(-120),
        "the authored per-mille offset must land verbatim in the metric"
    );
    assert_eq!(
        i64::from(kerned.line_width("AV")) - i64::from(plain.line_width("AV")),
        i64::from(-120 - own),
        "the measured line must move by authored minus the font's own value"
    );
}

/// A `0.0` entry CANCELS the font's pair kerning in the metric too, and must
/// survive the `delta != 0` filter that drops the font's own zero deltas.
#[test]
fn a_zero_authored_entry_cancels_the_fonts_kern_in_the_metric() {
    use cosmic_text::{Attrs, Family, Metrics};

    let (mut font_system, family, face) = metric_fixture_system();
    let attrs = Attrs::new()
        .family(Family::Name(family.as_str()))
        .metrics(Metrics::new(WIDTH_METRIC_EM, WIDTH_METRIC_EM));
    let cancelled = GlyphWidths::build(
        &mut font_system,
        &attrs,
        "AV",
        0.0,
        DEFAULT_WIDTH_TOLERANCE,
        InlineTagScope::NoBreakOnly,
        MetricKerning::auto(&map_on(face, &[('A', 'V', 0.0)])),
    );
    assert_eq!(
        cancelled.kerns.get(&('A', 'V')).copied(),
        Some(0),
        "a 0.0 entry must be STORED as zero, not dropped as 'no kerning'"
    );
    let advances: i64 = "AV"
        .chars()
        .map(|ch| i64::from(cancelled.advances.get(&ch).copied().unwrap_or(0)))
        .sum();
    assert_eq!(
        i64::from(cancelled.line_width("AV")),
        advances,
        "with the pair cancelled the line is exactly the sum of the two advances"
    );
}

/// A pair the pen does not override must not be overridden in the metric either:
/// here the selected (Latin) font carries a table that lists a Hebrew pair, but
/// Hebrew is drawn by ANOTHER face of the database — the pen's same-face guard
/// rejects it, so the metric keeps the shaped delta. Both tracked `fonts/ui` faces
/// sit in one database, so the shaper really falls back.
#[test]
fn a_pair_drawn_by_another_face_is_not_overridden_in_the_metric() {
    use crate::characterization::TrackedFont;
    use cosmic_text::{Attrs, Family, FontSystem, Metrics, fontdb};

    const FALLBACK_PAIR: &str = "\u{05D0}\u{05D1}";
    let mut db = fontdb::Database::new();
    db.load_font_data(TrackedFont::Latin.bytes());
    let (latin_face, latin_family) = db
        .faces()
        .next()
        .map(|face| (face.id, face.families[0].0.clone()))
        .expect("latin face");
    db.load_font_data(TrackedFont::Hebrew.bytes());
    let mut font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);
    let attrs = Attrs::new()
        .family(Family::Name(latin_family.as_str()))
        .metrics(Metrics::new(WIDTH_METRIC_EM, WIDTH_METRIC_EM));
    let table = map_on(latin_face, &[('\u{05D0}', '\u{05D1}', -400.0), ('A', 'V', -400.0)]);

    let build = |font_system: &mut FontSystem, text: &str, map: &CustomKerningMap| {
        GlyphWidths::build(
            font_system,
            &attrs,
            text,
            0.0,
            DEFAULT_WIDTH_TOLERANCE,
            InlineTagScope::NoBreakOnly,
            MetricKerning::auto(map),
        )
    };
    let plain = build(&mut font_system, FALLBACK_PAIR, &CustomKerningMap::default());
    let overridden = build(&mut font_system, FALLBACK_PAIR, &table);
    assert_eq!(
        overridden.kerns.get(&('\u{05D0}', '\u{05D1}')).copied(),
        plain.kerns.get(&('\u{05D0}', '\u{05D1}')).copied(),
        "a pair drawn by a face without the table must keep the SHAPED delta"
    );
    assert_eq!(overridden.line_width(FALLBACK_PAIR), plain.line_width(FALLBACK_PAIR));

    // Control on the SAME map: a pair the selected face draws still applies, so the
    // assertions above prove the face guard, not a dead override path.
    let covered = build(&mut font_system, "AV", &table);
    assert_eq!(
        covered.kerns.get(&('A', 'V')).copied(),
        Some(-400),
        "a same-face pair must still take the authored value verbatim"
    );
}

