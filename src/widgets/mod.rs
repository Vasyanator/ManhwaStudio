/*
FILE HEADER (widgets/mod.rs)
- Назначение: публичный реэкспорт переиспользуемых UI-виджетов приложения.
- Экспорт:
  - `EditableComboBox`: редактируемый комбобокс, который совмещает строку ввода
    и popup со списком готовых значений.
  - `SpellcheckedTextEdit`: многострочный `TextEdit` с фоновой проверкой орфографии
    через pure-Rust Hunspell-совместимый backend и подчёркиванием ошибочных слов.
  - `AutocompleteLine`: однострочное поле ввода с выпадающим списком автодополнения
    и настраиваемым лимитом количества подсказок.
  - `WheelComboBox`: combobox, который переключает элементы колесом мыши и
    глушит прокрутку родительского интерфейса.
  - `WheelSlider`: слайдер, который меняет значение колесом мыши на один логический шаг
    при наведении и гасит прокрутку родительского интерфейса.
  - `WheelSpinBox`: spinbox на базе `DragValue` с таким же поведением колеса мыши.
  - `SeedSpinBox`: spinbox для seed-значения с кнопкой генерации случайного seed.
  - `TextEditPlus`: многострочный редактор с цветом текста по диапазонам и
    упорядоченными цветными фонами под диапазонами символов.
  - `wheel_input_guard`: общий runtime guard, блокирующий wheel-реакции нижних
    виджетов, когда открыт popup combobox.
  - `ViewportColorSelector`: селектор цвета с кнопкой `Пипетка`, который
    умеет брать цвет из пикселя текущего viewport через screenshot-события egui.
  - `MarkedScrollArea`: вертикальный скролл с разметкой бара (типизированные/
    свободные пометки под ползунком) и жёлобом элементов слева от бара.
  - `AiButton`: an AI-tool launch button that gates its own availability on the
    process-global capability signals (backend/torch/onnxruntime) and paints an
    optional corner marker badge with the painter only.
  - `HelpHint`: a light-gray circled "?" icon whose hover tooltip carries a
    localized text line, an animated WebP hint (`ms-gifs` asset) streamed on a
    short-lived background worker, or both — text above the animation. An optional
    `with_action` button sits below that content; `show_with_action` returns a
    `HelpHintResponse` reporting its click.
*/
mod ai_button;
mod autocomplete_line;
mod editable_combo_box;
mod font_preview;
mod help_hint;
mod marked_scroll;
mod seed_spin_box;
mod spellchecked_line;
mod text_edit_plus;
mod viewport_color_selector;
mod wheel_combo_box;
mod wheel_input_guard;
mod wheel_slider;
mod wheel_spin_box;

#[allow(unused_imports)]
pub use ai_button::{AiButton, AiButtonResponse, AiCaps, AiRequirement};
#[allow(unused_imports)]
pub use autocomplete_line::{AutocompleteLine, AutocompleteLineResponse};
#[allow(unused_imports)]
pub use editable_combo_box::{EditableComboBox, EditableComboBoxResponse};
#[allow(unused_imports)]
pub use font_preview::{combo_font_family_name, ensure_font_family, is_font_family_bound};
#[allow(unused_imports)]
pub use help_hint::{HelpHint, HelpHintResponse};
#[allow(unused_imports)]
pub use marked_scroll::{
    ArrowStyle, BarGeometry, GutterItem, GutterSlot, MarkFill, MarkKind, MarkedScrollArea,
    MarkedScrollOutput, ScrollMark, ScrollSector, ScrollSpan, arrow, paint_marks_on_bar,
};
#[allow(unused_imports)]
pub use seed_spin_box::{SeedSpinBox, random_seed};
#[allow(unused_imports)]
pub use spellchecked_line::{
    SpellcheckedTextEdit, current_spellcheck_words_revision, invalidate_spellcheck_cache,
    load_custom_spellcheck_words, load_project_spellcheck_words, misspelled_word_at_pointer,
    queue_word_to_global_exceptions, queue_word_to_project_exceptions,
    save_custom_spellcheck_words, save_project_spellcheck_words,
    set_project_spellcheck_settings_file,
};
#[allow(unused_imports)]
pub use text_edit_plus::{TextEditPlus, TextEditPlusBackground, TextEditPlusTextColor};
#[allow(unused_imports)]
pub use viewport_color_selector::ViewportColorSelector;
#[allow(unused_imports)]
pub use wheel_combo_box::WheelComboBox;
#[allow(unused_imports)]
pub use wheel_slider::WheelSlider;
#[allow(unused_imports)]
pub use wheel_spin_box::WheelSpinBox;
