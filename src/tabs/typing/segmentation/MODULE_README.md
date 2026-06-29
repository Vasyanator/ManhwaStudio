# segmentation

Сегментатор текста вкладки «Текст»: режет абзац на блоки и описывает, как соединять
соседние блоки на одной строке и при переносе на новую. Языко-нейтральное ядро
отделено от языковых правил, чтобы новые языки добавлялись одним подмодулем.

## Файлы

- `base.rs` — язык-нейтральное API:
  - `Block { text, joint, unit_count }` — блок текста плюс стык к следующему;
  - `Joint { same_line, wrap_suffix, break_cost, word_break, conservatism }` —
    «группа» стыка: что вставлять между блоками на одной строке (`same_line`) и что
    дописывать в хвост головной строки при переносе (`wrap_suffix`). Конструкторы-
    группы: `Joint::space` (пробел / ничего), `Joint::soft_hyphen` (ничего / дефис),
    `Joint::hard_hyphen` («Рао-кун», дефис уже в тексте), `Joint::glue` (конец);
    билдер `with_conservatism` помечает стык категорией консервативности;
  - standalone dash/hyphen tokens between words are attached to the previous segment so a line
    break adjacent to the sign can place it only at the previous line end, never at the next line
    start;
  - NBSP (`U+00A0`) is treated as visible non-breaking whitespace: it counts as text but does not
    split tokenizer segments or create a wrap boundary;
  - `Conservatism` (`Safe` < `Relaxed` < `Bold` < `Reckless`) — насколько вольным
    надо быть, чтобы разрыв в стыке считался допустимым (обычный пробел → `Safe`,
    отрыв предлога/частицы/«число + единица» → выше);
  - `BindingMode` — как поступать со связанными служебными словами: `Glue`
    (склеивать в один блок — горизонтальный врапер) или `Annotate` (оставлять
    отдельными блоками с категорией стыка — перечисление форм, «один граф — фильтр»);
  - трейт `Segmenter` с языковыми хуками (`binding_conservatism`, `hyphenate_word`,
    `hyphen_cost`, `is_hard_hyphen_boundary`) и общими методами по умолчанию
    (`segment`, `build_segments`, `soft_hyphenate_overlong`, `split_segment_into_parts`);
  - общие хелперы `count_layout_units`, `build_line_text_and_units`.
- `ru.rs` — русская реализация `Segmenter` (`RussianSegmenter`): словари переноса
  (`HyphenationDictionaries`), категории связывания (`binding_conservatism`:
  однобуквенный предлог/«число + единица» → `Reckless`, короткий предлог/частица →
  `Bold`, длинный предлог/сокращение → `Relaxed`, обычная пара слов → `Safe`),
  словарный мягкий перенос и **безопасные границы** переноса.
  Правило ь/ъ/й: их нельзя оставлять в начале новой строки (справа от разрыва), но
  переносить *после* них можно — «силь-нее», «подъ-езд», «май-ка».
- `mod.rs` — реэкспорты + `with_default_segmenter` (пока всегда русский сегментатор).

## Кто использует

- `render_next::wrap::forms` — перечисление дискретных форм текста (блоки + `Joint`);
- `render_next::wrap::horizontal` — DP-подбор переносов поверх готовых блоков;
- `render_next::wrap::hyphenation` — runtime словарный/аварийный перенос
  переиспользует русские безопасные границы;
- `render_next::pipeline` — пред-перенос длинных слов (`soft_hyphenate_overlong`).

## Расширение

Новый язык = новый подмодуль с `impl base::Segmenter`. Затем подключить его выбор в
`with_default_segmenter` (в перспективе — по языку проекта/оверлея).
