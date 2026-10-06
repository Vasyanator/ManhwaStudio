/*
FILE HEADER (crates/ms-widgets/src/lib.rs)
- Назначение: публичный реэкспорт переиспользуемых UI-виджетов приложения.
  Крейт `ms-widgets` реэкспортируется бинарником как `crate::widgets`
  (`src/main.rs`), поэтому все существующие пути `crate::widgets::…` валидны.
  Вместе с виджетами сюда переехали два соседних листа того же слоя:
  `input_util` (общие egui-хелперы ввода) и `ui_fonts` (единственный владелец
  UI-шрифтового стека); оба тоже реэкспортированы бинарником под своими именами.
- Экспорт:
  - `EditableComboBox`: редактируемый комбобокс, который совмещает строку ввода
    и popup со списком готовых значений.
  - `SpellcheckedTextEdit`: `TextEdit` с фоновой проверкой орфографии через pure-Rust
    Hunspell-совместимый backend и подчёркиванием ошибочных слов; конструкторы
    `multiline` и `singleline` выбирают режим поля (общий layouter).
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
  - `ColorPresetPicker` (+ `ColorPresets`, `PresetDefaults`,
    `ColorPresetPickerOutput`): the stock egui palette popup extended with two
    rows of color presets and an explicit update/cancel pair. The widget owns
    only the UI state (which cell is targeted and the color it was last
    synchronized with); the preset set itself is caller-owned data and the
    caller persists it when `presets_changed` is reported.
  - `MarkedScrollArea`: вертикальный скролл с разметкой бара (типизированные/
    свободные пометки под ползунком) и жёлобом элементов слева от бара.
  - `AiButton`: an AI-tool launch button that gates its own availability on the
    process-global capability signals (backend/torch/onnxruntime) and paints an
    optional corner marker badge with the painter only.
  - `HangulKeyboard` (`HangulKeyboardState` + `show_hangul_keyboard`): an
    on-screen Korean jamo keyboard. `Compose` mode latches one key per L/V/T row
    and emits the assembled syllable on the `Insert` button; an explicit
    replace-previous toggle (`HangulInsertPlacement`) lets the user choose whether
    Insert appends a new syllable or overwrites the character before the caret.
    `Direct` mode emits a single compatibility jamo per click. The widget only
    draws: it never mutates text and never touches `egui::TextEditState`, it
    returns a `HangulKeyboardOutcome` (`insert` + `replace_previous`) and the
    consumer decides where the text goes.
  - `marquee` (`paint_marquee_galley` + the pure `marquee_frame`, `MarqueeTiming`):
    paints a single-line galley clipped to a rect; when it is wider than the rect it
    scrolls like a web marquee (rest at the start, scroll, short rest at the end, jump
    back) and schedules repaints only while it overflows and is visible.
  - `panel_dock`: the dockable-panel system (`dev-docs/dockable_panels_plan.md`).
    Pure layer: `DockLayout` + `PanelNode` describe how panels and their tabs are
    arranged and anchored, and `solve()` resolves that graph into rects (gap
    preservation, clamping into the host area, even shrinking). Widget layer:
    `PanelTab` declares one tab per frame, `CollapsiblePanel` draws one panel, and
    the `PanelDock` frame driver (`begin` → `tab(..).show(..)` → `end`) queues the
    tab bodies and runs them in panel order, so two tabs can borrow `&mut` of two
    different fields of the caller. `drag.rs` owns the reorganisation gestures,
    `persist.rs` the `PanelLayout` section of `user_config.json`, and `window.rs`
    the detached OS windows a tab can be dragged into (immediate child viewports,
    one per `HostId::SubWindow`).
  - `SearchableComboBox` (+ `SearchableComboItem`, `SearchableComboResponse`,
    `RowLayout`): a combo box whose drop-down rows carry a main line (optionally
    drawn in the row's own `egui::FontFamily`) and a smaller grey second line in
    the interface font, plus an on-demand search field that filters the list by a
    case-insensitive substring of either line and colours every match. The search
    field is a MODE: the popup opens as a plain list, and the field appears — above
    the list, pushing it down — when the user types into the open popup or presses
    the square magnifier button the widget draws after the combo button. `width(..)`
    covers BOTH buttons (`search_button_overhang` reports the difference), and the
    popup is that wide. `RowLayout` places that second line either UNDER the main
    one (`Tall`, the default) or AFTER it on the same line (`Wide`, one line per
    row). Row height is uniform across the list within a layout, and the caller's
    font resolver is called only for the rows actually drawn this frame.
  - `HelpHint`: a light-gray circled "?" icon whose hover tooltip carries a
    localized text line, an animated WebP hint (`ms-gifs` asset) streamed on a
    short-lived background worker, or both — text above the animation. An optional
    `with_action` button sits below that content; `show_with_action` returns a
    `HelpHintResponse` reporting its click.
*/
#![warn(clippy::all)]

// Brings the `ms-i18n` UI-string macros (`t!` / `tf!` / `tp!`) into crate-wide scope,
// exactly as `src/main.rs` does for the binary: the widgets' localized labels use the
// bare macro names the extraction tool emits and the key-validation test scans for.
#[macro_use]
extern crate ms_i18n;

// Shared egui input helpers (raw wheel delta, "pointer over a floating layer"). A leaf
// of the same UI-primitive layer as the widgets; `panel_dock/panel.rs` is one of its
// callers, and the canvas/tabs above reach it through the binary's re-export.
pub mod input_util;
// The user-configurable hotkey registry (`InputManagerV2`): code-declared specs, the
// persisted `Hotkeys` overrides in `user_config.json`, and the per-frame dispatch of
// triggered commands from `egui::InputState`. It lives here rather than in the binary
// because it is a pure egui-input primitive over `ms_config::app_tab::AppTab`, and both
// the binary (`app.rs`, the settings hotkeys pane) and the `translation` tab crate
// register specs with it.
pub mod input_manager_v2;
// The egui half of the bubble-status feature: it paints a rule's border with a bare
// `Painter` and re-exports the GUI-free rule model from `ms_config::bubble_status`. A
// painting primitive with no domain knowledge, so it belongs to this layer; the canvas
// and the settings tab above it both draw through it.
pub mod bubble_status;
// Single owner of the bundled `fonts/ui` stack: every egui context the app creates
// installs the same chain through it. It sits here because it is a pure egui-context
// concern shared by every window, launcher and installer included.
pub mod ui_fonts;

mod ai_button;
mod autocomplete_line;
mod color_preset_picker;
mod editable_combo_box;
mod font_preview;
mod hangul_keyboard;
mod help_hint;
mod marked_scroll;
mod marquee;
pub mod panel_dock;
mod searchable_combo_box;
mod seed_spin_box;
mod spellchecked_line;
mod text_edit_plus;
mod viewport_color_selector;
mod wheel_combo_box;
mod wheel_input_guard;
mod wheel_slider;
mod wheel_spin_box;

#[allow(unused_imports)]
pub use ai_button::{AiButton, AiButtonResponse, AiCaps, AiRequirement, marker_badge_overhang};
#[allow(unused_imports)]
pub use autocomplete_line::{AutocompleteLine, AutocompleteLineResponse};
#[allow(unused_imports)]
pub use color_preset_picker::{
    ColorPresetPicker, ColorPresetPickerOutput, ColorPresets, PRESET_COLUMNS, PRESET_COUNT,
    PRESET_ROWS, PresetDefaults,
};
#[allow(unused_imports)]
pub use editable_combo_box::{EditableComboBox, EditableComboBoxResponse};
#[allow(unused_imports)]
pub use font_preview::{
    PreviewFontFamily, combo_font_family_name, is_font_family_bound, request_font_family,
};
#[allow(unused_imports)]
pub use hangul_keyboard::{
    HangulInsertPlacement, HangulKeyboardMode, HangulKeyboardOutcome, HangulKeyboardState,
    show_hangul_keyboard,
};
#[allow(unused_imports)]
pub use help_hint::{HelpHint, HelpHintResponse};
#[allow(unused_imports)]
pub use marked_scroll::{
    ArrowStyle, BarGeometry, GutterItem, GutterSlot, MarkFill, MarkKind, MarkedScrollArea,
    MarkedScrollOutput, ScrollMark, ScrollSector, ScrollSpan, arrow, paint_marks_on_bar,
};
pub use marquee::{MarqueeFrame, MarqueeTiming, marquee_frame, paint_marquee_galley};
#[allow(unused_imports)]
pub use panel_dock::{
    CollapsiblePanel, CollapsiblePanelOutput, DetachTrigger, DockArea, DockEdge, DockLayout,
    DockModelError, DragEndContext, HostId, MoveTabOutcome, PanelAnchor, PanelChrome, PanelDock,
    PanelDockOutput, PanelDockState, PanelId, PanelLayoutError, PanelLayoutSnapshot,
    PanelLayoutWriter, PanelNode, PanelSizes, PanelTab, PanelTabHeader, SolvedLayout, SolvedPanel,
    SubWindowNode, TabExtras, TabId,
};
#[allow(unused_imports)]
pub use searchable_combo_box::{
    RowLayout, SearchableComboBox, SearchableComboItem, SearchableComboResponse,
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
pub use wheel_input_guard::combo_popup_open;
#[allow(unused_imports)]
pub use wheel_slider::WheelSlider;
#[allow(unused_imports)]
pub use wheel_spin_box::WheelSpinBox;
