/*
File: src/tabs/typing/segmentation/mod.rs

Purpose:
Сегментатор текста вкладки «Текст»: режет абзац на блоки (`Block`) и описывает
стык (`Joint`) между соседними блоками — как соединять на одной строке и при
переносе на новую. Язык-нейтральное ядро живёт в `base`, конкретные языки — в
отдельных подмодулях (`ru`).

Используется:
- `render_next::wrap` (горизонтальный DP-врапер и перечисление форм `forms`);
- runtime-перенос (`wrap::hyphenation`) переиспользует русские безопасные границы.

Точка расширения:
новый язык = новый подмодуль с `impl base::Segmenter`. `default_segmenter()` пока
возвращает русский сегментатор.
*/

pub mod base;
pub mod ru;

pub use base::{
    BindingMode, Block, Conservatism, NON_BREAKING_SPACE, SOFT_HYPHEN, SegmentOptions, Segmenter,
    build_line_text_and_units, count_layout_units,
};
pub use ru::{HyphenationDictionaries, RussianSegmenter};

thread_local! {
    static DEFAULT_SEGMENTER: RussianSegmenter = RussianSegmenter::new();
}

/// Выполняет `f` с текущим сегментатором по умолчанию (пока всегда русским).
pub fn with_default_segmenter<R>(f: impl FnOnce(&dyn Segmenter) -> R) -> R {
    DEFAULT_SEGMENTER.with(|seg| f(seg))
}
