/*
File: src/launcher/new_project/quick_download/mod.rs

Purpose:
Module root of the direct ("quick") chapter downloader used by the New Project launcher.

Main responsibilities:
- declare the layered submodules of the downloader;
- re-export the launcher-facing API unchanged (`window.rs` imports only from here).

Key items:
- QuickDownloadController, QuickDownloadEvent (re-exported from `controller`)
- supported_sites_tooltip()

Key submodules:
- controller — worker thread, progress events, ordered image download
- plan       — SiteDownloadPlan, QuickDownloadError, host dispatch
- sites      — one file per supported site (the only layer that knows host names)
- http       — HTTP fetch primitives and the wasm stubs
- url_util   — URL parsing/normalization primitives
- html       — HTML tag/attribute scanning primitives
- base64     — standard base64 decoding primitive

Notes:
Layering is one-way: controller -> plan -> sites -> (http | url_util | html | base64).
Nothing outside `sites/` may mention a concrete host; see MODULE_README.md.
The downloader mirrors the old Python quick downloader from `modules/downloader.py`, but keeps
all network and image decoding work in a worker thread so the egui window stays responsive.
*/

mod base64;
mod controller;
mod html;
mod http;
mod plan;
mod sites;
mod url_util;

// The launcher-facing surface is exactly what `new_project/window.rs` imports.
// `QuickDownloadSuccess` is intentionally NOT re-exported: it is only ever received inside
// `QuickDownloadEvent::Loaded`, and an unused `pub use` is a hard error under `-D warnings`
// in this binary crate.
pub use controller::{QuickDownloadController, QuickDownloadEvent};

/// Supported-sites tooltip for the quick downloader. Runtime accessor (not `const`)
/// because `t!` is not `const`.
#[must_use]
pub fn supported_sites_tooltip() -> &'static str {
    t!("launcher.new_project.quick_dl.supported_sites_hint")
}
