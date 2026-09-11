/*
File: crates/ms-tab-cleaning/src/lib.rs

Purpose:
Crate root of the «Клининг» (cleaning) tab. Re-exported by the binary from
`src/tabs/mod.rs` as `crate::tabs::cleaning`, so every existing
`crate::tabs::cleaning::…` call site keeps its path.

Layer:
The TOP of the library stack — above the canvas, the models, the tooling primitives, the
widgets and `ms-tab-translation` (it drives the AI backend health, the text detector and
the MT service through that crate), and below only `app.rs`, which calls
`CleaningTabState::draw`. It must never name `app` or `launcher`.
It does NOT depend on `ms-tab-typing` and must not start to: the one thing the two share is
the atomic document-write RECIPE that `tools/watermark_library.rs` documents and reimplements,
because `ms-tab-typing`'s `panel/doc_store.rs` is crate-private and unreachable from here.

Modules:
- `tab`: tab state, `CanvasHooks` implementation and the dock arrangement.
- `tools`: the whole cleaning tool set (mask brush, patch, region edit v2, the AI editor
  engines, watermark removal, AOT / Flux-Fill inpainting, mask generation).
- `autoclean`: the batch "clean the whole chapter" driver.
- `watermark_chapter`: the GUI-free chapter-level watermark decomposition engine.
*/

#![warn(clippy::all)]

// The `ms-i18n` UI-string macros (`t!` / `tf!` / `tp!`), mounted crate-wide exactly as
// `src/main.rs` mounts them for the binary: this tab's localized labels use the bare
// macro names the extraction tool emits and the key-validation test scans for.
#[macro_use]
extern crate ms_i18n;

mod autoclean;
mod tab;
mod tools;
// GUI-free chapter-level watermark decomposition engine (`I = c + s*B`, solved from several
// occurrences), consumed by the «По главе (точное вычитание)» mode of
// `tools/watermark_removal.rs`.
//
// `allow(dead_code)`: the tool uses the flat-sample path, so the engine's estimated-background
// REFINEMENT surface (`refit_with_refined_backgrounds`, `provisional_background`,
// `SampleBackground::Estimated` and the constants and error variants that belong to it) plus a
// handful of accessors currently have no product caller. They are a finished, deliberately kept
// capability — a chapter with no flat-ring occurrence at all needs them — and every one of them is
// exercised by this module's own tests, which `dead_code` does not count. Removing them would be
// removing measured functionality, not dead weight.
#[allow(dead_code)]
mod watermark_chapter;

pub use tab::{CleaningDrawParams, CleaningTabState};
/// The «Клининг» dock arrangement builder, handed to the app-owned dock state by
/// `app.rs::restore_panel_dock` before the first frame. `pub`, not crate-private: the
/// caller lives in the binary.
pub use tab::cleaning_default_dock_layout;
