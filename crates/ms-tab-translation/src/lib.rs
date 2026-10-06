/*
FILE OVERVIEW: crates/ms-tab-translation/src/lib.rs
Crate root of the «Перевод» (translation) tab. Re-exported by the binary from
`src/tabs/mod.rs` as `crate::tabs::translation`, so every existing
`crate::tabs::translation::…` call site keeps its path.

Layer: near the top of the library stack — above the canvas, the models, the widgets and
`ms-tabs-simple` (it reads character and term note entries), and below `app.rs`, the AI
backend panel/supervisor, the launcher's reline flow and the `cleaning` tab, all of which
reach into `backend_health` / `text_detector` / `machine_translation`. It must never name
`app` or `launcher`.

Submodules:
- `adv_rec`: floating advanced-recognition window for manual OCR region selection.
- `ai_mt_response`: reading, validating and merging AI API MT answers, content checks, the
  strict-output schema (native only).
- `backend_health`: push-driven AI-backend health (`TOPIC_HEALTH` v2 events) + device-control helpers.
- `machine_translators`: concrete MT backends (Google/Yandex/DeepL) used by worker.
- `machine_translation`: MT controller/worker and backend dispatch integration.
- `ocr`: OCR controller/worker and backend transport.
- `ocr_model_download`: status/download controller of the external OCR models (Baberu OCR,
  PaddleOCR-VL variants) the OCR panel downloads explicitly.
- `ocr_case_fix`: pure post-OCR "ALL CAPS" -> sentence-case normalization.
- `text_detector`: text detector controller/worker (classic + Paddle/CTD/Surya backend modes).
- `panels`: UI subpanels for Translation tab.
- `tab`: top-level Translation tab state implementing `CanvasHooks`, plus this program tab's
  default panel-dock arrangement (`translation_default_dock_layout`).
*/

#![warn(clippy::all)]

// The `ms-i18n` UI-string macros (`t!` / `tf!` / `tp!`), mounted crate-wide exactly as
// `src/main.rs` mounts them for the binary: this tab's localized labels use the bare
// macro names the extraction tool emits and the key-validation test scans for.
#[macro_use]
extern crate ms_i18n;

mod adv_rec;
#[cfg(not(target_arch = "wasm32"))]
mod ai_mt_response;
// `pub` rather than crate-private: the binary's AI backend panel and supervisor, the
// launcher's reline flow and the `cleaning` tab all read the push-driven health snapshot
// through it.
pub mod backend_health;
// `pub` rather than crate-private: the `cleaning` tab's Flux.2 engine translates prompts
// through `translate_texts_via_translator`.
pub mod machine_translation;
mod machine_translators;
mod ocr;
mod ocr_case_fix;
mod ocr_model_download;
pub mod panels;
mod tab;
// `pub` rather than crate-private: the `cleaning` tab's mask-generation and
// watermark-removal tools drive the detector through it.
pub mod text_detector;

/// The «Перевод» dock arrangement builder, handed to the app-owned dock state by
/// `app.rs::restore_panel_dock` before the first frame.
pub use tab::translation_default_dock_layout;
pub use tab::{
    HOTKEY_TRANSLATION_COPY_BUBBLE_ORIGINAL, HOTKEY_TRANSLATION_COPY_BUBBLE_TRANSLATION,
    HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE, HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE,
    HOTKEY_TRANSLATION_PASTE_BUBBLE_ORIGINAL, HOTKEY_TRANSLATION_PASTE_BUBBLE_TRANSLATION,
    HOTKEY_TRANSLATION_TOGGLE_BUBBLES_PANEL, HOTKEY_TRANSLATION_TOGGLE_COMPOSITION_PANEL,
    HOTKEY_TRANSLATION_TOGGLE_DETECTOR_PANEL, HOTKEY_TRANSLATION_TOGGLE_MT_PANEL,
    HOTKEY_TRANSLATION_TOGGLE_OCR_PANEL, TranslationHotkeyHints, TranslationTabState,
};
