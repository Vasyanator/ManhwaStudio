/*
FILE OVERVIEW: crates/ms-tab-translation/src/lib.rs
Crate root of the «Перевод» (translation) tab. Re-exported by the binary from
`src/tabs/mod.rs` as `crate::tabs::translation`, so every existing
`crate::tabs::translation::…` call site keeps its path.

Layer: near the top of the library stack — above the canvas, the models, the widgets,
`ms-local-ai` (backend health, local-AI availability) and `ms-tabs-simple` (it reads character
and term note entries), and below `studio.rs` and the `cleaning` tab, which reach into
`text_detector` / `machine_translation`. It must never name `studio` or `launcher`.

Submodules:
- `adv_rec`: floating advanced-recognition window for manual OCR region selection.
- `ai_mt_response`: reading, validating and merging AI API MT answers, content checks, the
  strict-output schema (native only).
- `machine_translators`: concrete MT backends (Google/Yandex/DeepL over built-in HTTP, and the
  browser-extension provider `browser`) used by worker.
- `machine_translation`: MT controller/worker and backend dispatch integration.
- `mt_limits`: process-wide request-rate limiter of the MT services (pacing, cooldowns,
  cancellable reported waits, long-text split); GUI-free, shared with the Cleaning tab's
  prompt translator through `translate_texts_via_translator`.
- `mt_transport`: the live per-service MT transport (built-in / browser), seeded at startup.
- `ocr`: OCR controller/worker and backend transport.
- `ocr_model_download`: status/download controller of the app-managed external model bundles
  (Baberu OCR, PaddleOCR-VL variants, EasyOCR scripts, Surya checkpoints; the Cleaning tab's
  SDXL configs) that panels download explicitly.
- `model_download_status`: the one owner of how an app-managed model download is shown
  (latest event, text, progress bar); `pub` because the Cleaning tools draw it too.
- `ocr_case_fix`: pure post-OCR "ALL CAPS" -> sentence-case normalization.
- `neurolate`: the tab's NeuroLate settings (language, comic type, "create mask") with the one
  owner of their "Auto" rules and request parameters, and their shared settings view.
- `page_crop`: the one uv selection -> page pixel rect rule (OCR crop and the detector's region
  detection of the same selection).
- `text_detector`: text detector controller/worker (classic + Paddle/CTD/Surya backend modes).
- `panels`: UI subpanels for Translation tab.
- `tab`: top-level Translation tab state implementing `CanvasHooks`, plus this program tab's
  default panel-dock arrangement (`translation_default_dock_layout`).
- `host`: `TranslationTab`, the program tab as the studio root holds it — the tab's own
  `CanvasView` plus `TranslationTabState`, its draw, its command dispatch and hint rows, and
  its `ms_studio_tab::StudioTab` implementation.
- `input`: every input action of the tab (`ms_input` declarations), its contexts and the
  binding texts of its hints; the only place in the crate that says which input does what.
*/

#![warn(clippy::all)]

// The `ms-i18n` UI-string macros (`t!` / `tf!` / `tp!`), mounted crate-wide exactly as
// `src/lib.rs` mounts them for the binary: this tab's localized labels use the bare
// macro names the extraction tool emits and the key-validation test scans for.
#[macro_use]
extern crate ms_i18n;

mod adv_rec;
// `TranslationTab`: the program tab as the studio holds it (canvas + state + `StudioTab`).
mod host;
mod input;
#[cfg(not(target_arch = "wasm32"))]
mod ai_mt_response;
// `pub` rather than crate-private: the `cleaning` tab's Flux.2 engine translates prompts
// through `translate_texts_via_translator`.
pub mod machine_translation;
mod machine_translators;
// The one owner of MT request pacing and cooldowns (`mt_limits/MODULE_README.md`).
mod mt_limits;
// `pub` rather than crate-private: the root seeds the live per-service MT transport at startup.
pub mod mt_transport;
// The tab's NeuroLate settings and request-parameter rules (detector mode, OCR engine).
mod neurolate;
// `pub` rather than crate-private: the `cleaning` tab's mask generation, region-inpaint editor
// and LaMa / SDXL engines show their model downloads through it.
pub mod model_download_status;
mod ocr;
mod ocr_case_fix;
// The one uv selection -> page pixel rect rule, shared by the OCR crop and region detection.
mod page_crop;
// `pub` rather than crate-private: the `cleaning` tab's SDXL engine downloads its pinned
// config specs through the same controller and draws the same block
// (`panels::ocr_model_download`).
pub mod ocr_model_download;
pub mod panels;
mod tab;
// `pub` rather than crate-private: the `cleaning` tab's mask-generation and
// watermark-removal tools drive the detector through it.
pub mod text_detector;

/// The «Перевод» program tab (its canvas plus its state) the studio root holds and draws; its
/// dock builder and input declarations reach the root through `ms_studio_tab::StudioTab`.
pub use host::{TranslationDrawParams, TranslationTab};
/// The tab state the root constructs (services injected) before wrapping it in a
/// `TranslationTab`.
pub use tab::TranslationTabState;

/// Test-only: holds the process-wide locale lock (`ms_config::locale_store::GLOBAL_LOCALE_LOCK`,
/// the one every test that switches the active catalog takes) for the life of the guard, so a
/// test comparing localized strings sees one catalog throughout — whichever one is active.
/// Not reentrant: a test that holds it must not also take a locale-installing helper's guard.
#[cfg(test)]
pub(crate) fn hold_locale_for_test() -> std::sync::MutexGuard<'static, ()> {
    ms_config::locale_store::GLOBAL_LOCALE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Test-only: takes the process-wide locale lock (as [`hold_locale_for_test`]) AND installs the
/// embedded English catalog, for a test whose assertion needs real catalog text (e.g. an
/// interpolated `{error}`). Before any install the active catalog is empty and `tf!` returns the
/// bare key, so such a test must not rely on another test having installed one first.
#[cfg(test)]
pub(crate) fn english_locale_for_test() -> std::sync::MutexGuard<'static, ()> {
    let guard = hold_locale_for_test();
    let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
    ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");
    guard
}
