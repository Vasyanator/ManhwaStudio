/*
FILE OVERVIEW: src/tabs/translation/mod.rs
Translation tab module wiring.

Submodules:
- `adv_rec`: floating advanced-recognition window for manual OCR region selection.
- `backend_health`: push-driven AI-backend health (`TOPIC_HEALTH` v2 events) + device-control helpers.
- `machine_translators`: concrete MT backends (Google/Yandex/DeepL) used by worker.
- `machine_translation`: MT controller/worker and backend dispatch integration.
- `ocr`: OCR controller/worker and backend transport.
- `text_detector`: text detector controller/worker (classic + Paddle/CTD/Surya backend modes).
- `panels`: UI subpanels for Translation tab.
- `tab`: top-level Translation tab state implementing `CanvasHooks`.
*/

mod adv_rec;
pub(crate) mod backend_health;
mod machine_translation;
mod machine_translators;
mod ocr;
pub mod panels;
mod tab;
pub(crate) mod text_detector;

pub use tab::{
    HOTKEY_TRANSLATION_COPY_BUBBLE_ORIGINAL, HOTKEY_TRANSLATION_COPY_BUBBLE_TRANSLATION,
    HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE, HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE,
    HOTKEY_TRANSLATION_PASTE_BUBBLE_ORIGINAL, HOTKEY_TRANSLATION_PASTE_BUBBLE_TRANSLATION,
    HOTKEY_TRANSLATION_TOGGLE_BUBBLES_PANEL, HOTKEY_TRANSLATION_TOGGLE_COMPOSITION_PANEL,
    HOTKEY_TRANSLATION_TOGGLE_DETECTOR_PANEL, HOTKEY_TRANSLATION_TOGGLE_MT_PANEL,
    HOTKEY_TRANSLATION_TOGGLE_OCR_PANEL, TranslationHotkeyHints, TranslationTabState,
};
