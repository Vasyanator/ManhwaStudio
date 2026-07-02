/*
File: src/tabs/typing/segmentation/base.rs

Purpose:
Языко-независимое API сегментатора текста. Сегментатор режет абзац на блоки
(`Block`) и описывает «стык» (`Joint`) между соседними блоками: как их соединять,
если они остаются на одной строке, и как — при переносе на новую.

Идея «групп» стыков (расширяемо, см. конструкторы `Joint`):
- пробельный перенос: на той же строке — пробел(ы), при переносе — ничего;
- словарный мягкий перенос («ста|нешь»): на той же строке — ничего, при переносе —
  дефис в хвосте головной строки;
- существующий дефис («Рао-кун»): на той же строке — ничего (дефис уже в тексте),
  при переносе — тоже ничего (дефис уже на месте).
- отдельное тире/дефис между пробелами присоединяется к предыдущему сегменту, чтобы
  при разрыве строки знак оставался в конце предыдущей строки, а не начинал новую.

Языковые детали (правила связывания слов, словарный перенос, оценка качества
переноса) вынесены в хуки трейта `Segmenter`; конкретный язык реализует их в своём
модуле (см. `ru`). Алгоритм сегментации и сборки строки — общий и живёт здесь.
*/

use std::borrow::Cow;

use crate::text_punctuation::is_hanging_punctuation;

/// Мягкий перенос (U+00AD): маркер словарной точки переноса внутри слова.
pub const SOFT_HYPHEN: char = '\u{00AD}';
pub const NON_BREAKING_SPACE: char = '\u{00A0}';

/// «Категория консервативности» точки переноса: насколько вольным надо быть, чтобы
/// разрыв в этом стыке считался допустимым. Чем выше категория, тем рискованнее
/// (типографски «хуже») перенос здесь.
///
/// Идея «один граф — потом фильтр»: дерево форм строится без жёсткой склейки
/// служебных слов, но каждый стык несёт свою категорию. Консервативность *формы* —
/// это максимум категорий по всем её фактическим разрывам (см. перечисление форм).
/// Затем формы фильтруются по выбранному порогу: строгий порог `Safe` отбрасывает
/// любые формы с отрывом предлогов, вольный — пропускает всё.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Conservatism {
    /// Безопасный перенос — по пробелу между обычными словами, по словарю или по
    /// существующему дефису. Допустим на любом уровне.
    Safe,
    /// Отрыв «длинного» служебного слова (предлог/союз ≥3 букв, сокращение).
    Relaxed,
    /// Отрыв «короткого» служебного слова (2-буквенный предлог/союз, частица) либо
    /// разрыв внутри цепочки служебных слов.
    Bold,
    /// Отрыв однобуквенного предлога/союза или разрыв пары «число + единица» —
    /// самый рискованный класс.
    Reckless,
}

impl Conservatism {
    /// Все категории от безопасной к самой вольной.
    #[must_use]
    pub fn all() -> [Conservatism; 4] {
        [
            Conservatism::Safe,
            Conservatism::Relaxed,
            Conservatism::Bold,
            Conservatism::Reckless,
        ]
    }

    /// Короткая подпись категории для UI.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Conservatism::Safe => "базовая",
            Conservatism::Relaxed => "длинные предлоги",
            Conservatism::Bold => "короткие предлоги",
            Conservatism::Reckless => "однобуквенные",
        }
    }
}

/// Как поступать со «связанными» служебными словами (предлоги/частицы/«число +
/// единица») при разбиении абзаца на блоки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingMode {
    /// Склеивать связанные слова в один блок — между ними перенос невозможен
    /// (горизонтальный DP-врапер: гарантия против сиротливых предлогов).
    Glue,
    /// Оставлять связанные слова отдельными блоками, а стык помечать категорией
    /// консервативности. Граф форм строится один раз, фильтрация — после
    /// (перечисление форм).
    Annotate,
}

/// «Стык» между двумя соседними блоками — как их соединять.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Joint {
    /// Что вставляется между блоками, если они на одной строке (напр. `" "` / `""`).
    pub same_line: Cow<'static, str>,
    /// Что дописывается в хвост головной строки при переносе здесь (напр. `"-"` / `""`).
    pub wrap_suffix: Cow<'static, str>,
    /// Цена переноса в этом стыке (для сортировки форм; на отбор не влияет).
    pub break_cost: u32,
    /// Перенос здесь рвёт слово (непробельный разрыв).
    pub word_break: bool,
    /// Категория консервативности переноса в этом стыке (см. `Conservatism`).
    pub conservatism: Conservatism,
}

impl Joint {
    /// Группа «пробел»: та же строка → пробел(ы), новая строка → ничего.
    #[must_use]
    pub fn space(spaces: String) -> Self {
        Self {
            same_line: Cow::Owned(spaces),
            wrap_suffix: Cow::Borrowed(""),
            break_cost: 0,
            word_break: false,
            conservatism: Conservatism::Safe,
        }
    }

    /// Группа «словарный мягкий перенос»: та же строка → ничего, новая → дефис.
    #[must_use]
    pub fn soft_hyphen(break_cost: u32) -> Self {
        Self {
            same_line: Cow::Borrowed(""),
            wrap_suffix: Cow::Borrowed("-"),
            break_cost,
            word_break: true,
            conservatism: Conservatism::Safe,
        }
    }

    /// Группа «существующий дефис» («Рао-кун»): дефис уже в тексте головы.
    #[must_use]
    pub fn hard_hyphen() -> Self {
        Self {
            same_line: Cow::Borrowed(""),
            wrap_suffix: Cow::Borrowed(""),
            break_cost: 1,
            word_break: true,
            conservatism: Conservatism::Safe,
        }
    }

    /// Группа «склейка/конец»: ни на той же строке, ни при переносе ничего не
    /// добавляется (хвост абзаца, сохранённые краевые пробелы).
    #[must_use]
    pub fn glue() -> Self {
        Self {
            same_line: Cow::Borrowed(""),
            wrap_suffix: Cow::Borrowed(""),
            break_cost: 0,
            word_break: false,
            conservatism: Conservatism::Safe,
        }
    }

    /// Назначить стыку категорию консервативности (билдер).
    #[must_use]
    pub fn with_conservatism(mut self, conservatism: Conservatism) -> Self {
        self.conservatism = conservatism;
        self
    }

    /// Это стык словарного мягкого переноса (при переносе появляется дефис)?
    #[must_use]
    pub fn is_soft_hyphen(&self) -> bool {
        self.wrap_suffix == "-"
    }
}

/// Один блок текста плюс стык к следующему блоку.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Block {
    pub text: String,
    pub joint: Joint,
    pub unit_count: usize,
}

/// Опции сегментации.
#[derive(Debug, Clone, Copy)]
pub struct SegmentOptions {
    /// Учитывать висящую пунктуацию при подсчёте «юнитов» строки.
    pub hanging_punctuation: bool,
    /// Сохранять ведущие/хвостовые пробелы абзаца отдельными блоками.
    pub preserve_edge_spaces: bool,
    /// Разрешать перенос по уже существующим дефисам («Рао-кун»).
    pub allow_hard_hyphen_breaks: bool,
    /// Как поступать со связанными служебными словами: склеивать (`Glue`, врапер)
    /// или помечать стык категорией консервативности (`Annotate`, формы).
    pub binding: BindingMode,
}

/// Считает «юниты» строки: видимые символы без мягких переносов/перевода строки и
/// (опционально) без висящей пунктуации.
#[must_use]
pub fn count_layout_units(text: &str, hanging_punctuation: bool) -> usize {
    text.chars()
        .filter(|&ch| {
            ch != SOFT_HYPHEN
                && ch != '\n'
                && ch != '\r'
                && (!hanging_punctuation || !is_hanging_punctuation(ch))
        })
        .count()
}

/// Собирает текст строки из блоков и считает её юниты. Между блоками вставляется
/// `same_line`-склейка; если строка переносится после последнего блока
/// (`wraps_here`), в хвост дописывается `wrap_suffix`.
#[must_use]
pub fn build_line_text_and_units(blocks: &[Block], wraps_here: bool) -> (String, usize) {
    let mut line = String::new();
    let mut units = 0usize;

    for (idx, block) in blocks.iter().enumerate() {
        line.push_str(block.text.as_str());
        units = units.saturating_add(block.unit_count);
        let is_last = idx + 1 == blocks.len();
        if !is_last {
            let glue = block.joint.same_line.as_ref();
            if !glue.is_empty() {
                line.push_str(glue);
                units = units.saturating_add(glue.chars().count());
            }
        } else if wraps_here {
            line.push_str(block.joint.wrap_suffix.as_ref());
        }
    }

    (line, units)
}

/// Языковой сегментатор. Конкретный язык реализует хуки связывания/переноса, а
/// общий алгоритм сегментации предоставляется по умолчанию.
pub trait Segmenter {
    /// Категория консервативности переноса между соседними токенами `left`/`right`.
    /// `Safe` — обычный пробел (рвать можно свободно); выше — служебная связь
    /// (предлог/частица/аббревиатура/«число + единица»), отрыв которой тем
    /// рискованнее, чем выше категория. В режиме `BindingMode::Glue` всё, что выше
    /// `Safe`, склеивается в один блок; в `Annotate` — помечает стык этой категорией.
    fn binding_conservatism(&self, left_token: &str, right_token: &str) -> Conservatism;

    /// Вставить мягкие переносы (`SOFT_HYPHEN`) в одно слово по словарю; `None`,
    /// если слово переносить не нужно.
    fn hyphenate_word(&self, word: &str) -> Option<String>;

    /// Цена словарного переноса по голове/хвосту слова (для сортировки форм).
    fn hyphen_cost(&self, head_word: &str, tail_word: &str) -> u32;

    /// Является ли дефис в позиции `idx` (символ `ch`) точкой переноса по
    /// существующему дефису. Реализация по умолчанию языко-нейтральна.
    fn is_hard_hyphen_boundary(&self, text: &str, idx: usize, ch: char) -> bool {
        is_inline_hard_hyphen_break_char_default(text, idx, ch)
    }

    /// Вставляет мягкие переносы в «длинные» слова всего текста (≥ 4 символов,
    /// без URL/`@`/уже имеющихся дефисов).
    #[must_use]
    fn soft_hyphenate_overlong(&self, text: &str) -> String {
        let ranges = find_word_ranges(text);
        if ranges.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len() + text.len() / 8);
        let mut tail_start = 0usize;
        for (start, end) in ranges {
            out.push_str(&text[tail_start..start]);
            let word = &text[start..end];
            let replacement = self.hyphenate_word(word).unwrap_or_else(|| word.to_string());
            out.push_str(replacement.as_str());
            tail_start = end;
        }
        out.push_str(&text[tail_start..]);
        out
    }

    /// Режет абзац на блоки с языковыми правилами связывания и переносами.
    #[must_use]
    fn segment(&self, paragraph: &str, opts: SegmentOptions) -> Vec<Block> {
        let segments = self.build_segments(paragraph, opts.preserve_edge_spaces, opts.binding);
        let mut out = Vec::<Block>::new();

        let segment_count = segments.len();
        for segment_idx in 0..segment_count {
            let segment = segments[segment_idx].as_str();
            if opts.preserve_edge_spaces && segment.chars().all(is_breaking_whitespace) {
                out.push(Block {
                    unit_count: count_layout_units(segment, opts.hanging_punctuation),
                    text: segment.to_string(),
                    joint: Joint::glue(),
                });
                continue;
            }

            let trimmed_text = segment.trim_end_matches(is_breaking_whitespace);
            if trimmed_text.is_empty() {
                continue;
            }

            let trailing_ws = segment
                .chars()
                .rev()
                .take_while(|ch| is_breaking_whitespace(*ch))
                .count();
            let separator = (trailing_ws > 0).then(|| " ".repeat(trailing_ws));

            let is_last_segment = segment_idx + 1 == segment_count;
            // Категория консервативности стыка к следующему сегменту. В режиме
            // `Glue` соседние сегменты заведомо не связаны (иначе были бы склеены) —
            // выйдет `Safe`; в `Annotate` здесь и проступают отрывы предлогов.
            let next_first_word = segments
                .get(segment_idx + 1)
                .and_then(|next| next.split_whitespace().next());
            let conservatism = match (trimmed_text.split_whitespace().next_back(), next_first_word) {
                (Some(left), Some(right)) => self.binding_conservatism(left, right),
                _ => Conservatism::Safe,
            };

            let tail_joint = if opts.preserve_edge_spaces && is_last_segment {
                Joint::glue()
            } else if let Some(space) = separator.as_ref() {
                Joint::space(space.clone()).with_conservatism(conservatism)
            } else {
                Joint::glue()
            };

            let parts =
                self.split_segment_into_parts(trimmed_text, tail_joint, opts.allow_hard_hyphen_breaks);
            for (part, joint) in parts {
                out.push(Block {
                    unit_count: count_layout_units(part.as_str(), opts.hanging_punctuation),
                    text: part,
                    joint,
                });
            }

            if opts.preserve_edge_spaces
                && is_last_segment
                && let Some(space) = separator
            {
                out.push(Block {
                    unit_count: count_layout_units(space.as_str(), opts.hanging_punctuation),
                    text: space,
                    joint: Joint::glue(),
                });
            }
        }

        out
    }

    /// Группирует токены в сегменты. В режиме `BindingMode::Glue` связанные
    /// служебные слова склеиваются в один сегмент (между ними перенос невозможен);
    /// в `Annotate` каждое слово остаётся отдельным сегментом (связь выражается
    /// категорией консервативности стыка, см. `segment`).
    #[must_use]
    fn build_segments(
        &self,
        paragraph: &str,
        preserve_edge_spaces: bool,
        binding: BindingMode,
    ) -> Vec<String> {
        let tokens = tokenize_paragraph(paragraph);
        let mut out = Vec::<String>::new();
        let mut idx = 0usize;

        while idx < tokens.len() && tokens[idx].chars().all(is_breaking_whitespace) {
            if preserve_edge_spaces {
                out.push(tokens[idx].clone());
            }
            idx += 1;
        }

        let mut current_text = String::new();
        while idx < tokens.len() {
            let token = tokens[idx].as_str();
            if token.chars().all(is_breaking_whitespace) {
                idx += 1;
                continue;
            }

            current_text.push_str(token);

            let Some(space) = tokens.get(idx + 1) else {
                break;
            };
            if !space.chars().all(is_breaking_whitespace) {
                idx += 1;
                continue;
            }

            let Some(next_word) = tokens.get(idx + 2) else {
                current_text.push_str(space.as_str());
                break;
            };

            if is_line_end_dash_token(next_word.as_str()) {
                current_text.push_str(space.as_str());
                current_text.push_str(next_word.as_str());
                if let Some(after_dash_space) = tokens.get(idx + 3)
                    && after_dash_space.chars().all(is_breaking_whitespace)
                {
                    current_text.push_str(after_dash_space.as_str());
                    out.push(std::mem::take(&mut current_text));
                    idx += 4;
                    continue;
                }
                idx += 3;
                continue;
            }

            current_text.push_str(space.as_str());
            let glue_here = binding == BindingMode::Glue
                && self.binding_conservatism(token, next_word.as_str()) > Conservatism::Safe;
            if glue_here {
                idx += 2;
                continue;
            }

            out.push(std::mem::take(&mut current_text));
            idx += 2;
        }

        if !current_text.is_empty() {
            out.push(current_text);
        }

        out
    }

    /// Делит один (обрезанный) сегмент на части по мягким и существующим дефисам.
    /// Каждый внутрисловный стык получает оценку качества через `hyphen_cost`.
    #[must_use]
    fn split_segment_into_parts(
        &self,
        text: &str,
        tail_joint: Joint,
        allow_hard_hyphen_breaks: bool,
    ) -> Vec<(String, Joint)> {
        let mut out = Vec::<(String, Joint)>::new();
        let mut part_start = 0usize;

        for (idx, ch) in text.char_indices() {
            if ch == SOFT_HYPHEN {
                if part_start < idx {
                    let cost = self.hyphen_cost(
                        word_head_for_cost(text, part_start, idx),
                        word_tail_for_cost(text, idx + ch.len_utf8()),
                    );
                    out.push((text[part_start..idx].to_string(), Joint::soft_hyphen(cost)));
                }
                part_start = idx + ch.len_utf8();
                continue;
            }

            if allow_hard_hyphen_breaks && self.is_hard_hyphen_boundary(text, idx, ch) {
                let next_idx = idx + ch.len_utf8();
                out.push((text[part_start..next_idx].to_string(), Joint::hard_hyphen()));
                part_start = next_idx;
            }
        }

        if part_start < text.len() {
            out.push((text[part_start..].to_string(), tail_joint));
        } else if let Some((_, joint)) = out.last_mut() {
            *joint = tail_joint;
        }

        out
    }
}

/// Голова слова для оценки качества переноса: от ближайшей слева границы слова до
/// позиции переноса `break_at` (мягкие переносы выкидываются).
fn word_head_for_cost(text: &str, part_start: usize, break_at: usize) -> &str {
    let word_start = text[..part_start]
        .char_indices()
        .rev()
        .find(|(_, ch)| is_breaking_whitespace(*ch))
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    &text[word_start..break_at]
}

/// Хвост слова для оценки качества переноса: от позиции переноса до ближайшей
/// справа границы слова.
fn word_tail_for_cost(text: &str, break_at: usize) -> &str {
    let rel_end = text[break_at..]
        .char_indices()
        .find(|(_, ch)| is_breaking_whitespace(*ch) || *ch == SOFT_HYPHEN)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len() - break_at);
    &text[break_at..break_at + rel_end]
}

/// Диапазоны «слов» (≥ 4 алфавитно-цифровых символов) для словарного переноса.
#[must_use]
fn find_word_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::<(usize, usize)>::new();
    let mut run_start: Option<usize> = None;
    let mut run_len_chars = 0usize;

    for (idx, ch) in text.char_indices() {
        if is_word_char(ch) {
            if run_start.is_none() {
                run_start = Some(idx);
                run_len_chars = 0;
            }
            run_len_chars += 1;
            continue;
        }
        if let Some(start) = run_start.take()
            && run_len_chars >= 4
        {
            ranges.push((start, idx));
        }
        run_len_chars = 0;
    }

    if let Some(start) = run_start
        && run_len_chars >= 4
    {
        ranges.push((start, text.len()));
    }

    ranges
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

fn is_breaking_whitespace(ch: char) -> bool {
    ch.is_whitespace() && ch != NON_BREAKING_SPACE
}

/// True for standalone dash/hyphen tokens that should stay at the previous line end.
#[must_use]
fn is_line_end_dash_token(token: &str) -> bool {
    let trimmed = token.trim();
    !trimmed.is_empty() && trimmed.chars().all(is_line_end_dash_char)
}

fn is_line_end_dash_char(ch: char) -> bool {
    matches!(
        ch,
        '-' | '\u{2010}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}'
    )
}

/// Языко-нейтральная проверка дефиса как точки переноса по существующему дефису.
fn is_inline_hard_hyphen_break_char_default(text: &str, idx: usize, ch: char) -> bool {
    if !is_line_end_dash_char(ch) || text.contains("://") || text.contains('@') {
        return false;
    }

    let next_idx = idx + ch.len_utf8();
    if idx == 0 || next_idx >= text.len() {
        return false;
    }

    let left = text[..idx].chars().next_back();
    let right = text[next_idx..].chars().next();
    let (Some(left), Some(right)) = (left, right) else {
        return false;
    };

    !is_breaking_whitespace(left)
        && !is_breaking_whitespace(right)
        && (left.is_alphabetic() || right.is_alphabetic())
}

/// Разбивает абзац на чередующиеся токены «непробел / пробел».
#[must_use]
fn tokenize_paragraph(paragraph: &str) -> Vec<String> {
    let mut tokens = Vec::<String>::new();
    let mut start = 0usize;
    let mut mode_ws: Option<bool> = None;

    for (idx, ch) in paragraph.char_indices() {
        let is_ws = is_breaking_whitespace(ch);
        match mode_ws {
            None => mode_ws = Some(is_ws),
            Some(prev) if prev != is_ws => {
                tokens.push(paragraph[start..idx].to_string());
                start = idx;
                mode_ws = Some(is_ws);
            }
            _ => {}
        }
    }

    if start < paragraph.len() {
        tokens.push(paragraph[start..].to_string());
    }

    tokens
}
