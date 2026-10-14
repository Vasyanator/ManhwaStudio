/*
FILE OVERVIEW: src/lib.rs
Library root of the `manhwastudio_rs` package: the module tree (`studio`, `args`, `startup`, `shell`,
`tabs`, …), the `crate::x` re-export shims onto the `ms-*` crates, and the two public entry
points. The `manhwastudio_rs` binary (`src/main.rs`) is a thin shell that calls `native_main()`
(desktop) or `web_main()` (wasm32); these two functions are the library's only public items. The
split exists so a platform build that needs a library (the Android `cdylib`, built with
`cargo rustc --lib --crate-type cdylib`) can reach the same startup code: on Android the
unmangled `android_main` of `android_entry.rs` prepares the process and calls `native_main()`.

- `native_main`: runs the native startup flow (`startup::run_native`, see
  `src/startup/MODULE_README.md`) and logs how the process ended.
- `web_main`: boots the wasm32 web entry (`web_entry.rs`).
*/


// Bring the `ms-i18n` UI-string macros (`t!` / `tf!` / `tp!`) into crate-wide
// scope so migrated call sites can use the bare macro names the extraction tool
// (`tools/i18n_extract.py`) emits and the `ms-i18n` key-validation test scans for.
// A single crate-root `#[macro_use]` avoids a per-file `use ms_i18n::{t, tf, tp};`
// across the hundreds of UI files the localization migration touches.
#[macro_use]
extern crate ms_i18n;

// Android process entry (`android_main`, exported from the `cdylib` the APK loads): logcat,
// TLS verifier, runtime root, then the same `native_main` startup as the desktop binary.
#[cfg(target_os = "android")]
mod android_entry;
mod studio;
mod args;
mod i18n_resolve;
// The main window: ONE `run_native` hosting the launcher and the studio as screens (launcher
// entry, studio load and studio teardown on workers). Its `style` part is shared with the web
// entry; everything else is native-only (gated inside).
mod shell;
mod tabs;
// The startup check for a newer release (GitHub), started once by the main window.
#[cfg(not(target_arch = "wasm32"))]
mod update_check;
// Native process startup: CLI, service flags, the pre-window prompts and the one main window
// (`startup::run_native`, called by `native_main`). Nothing of it exists on wasm32.
#[cfg(not(target_arch = "wasm32"))]
mod startup;
#[cfg(target_arch = "wasm32")]
mod web_entry;

// The crate-shim map below: each `pub(crate) use` mounts an extracted `ms-*` crate (or one of
// its modules) under the binary-era `crate::…` name its call sites still use. The shims are
// crate-internal on purpose — the library's public API is only `native_main` / `web_main` —
// so an alias that no call site uses any more is reported as an unused import and removed;
// code needing such a crate names it directly (`ms_tools::…`).

// The settings surface both shells share — the menu-level `settings_shared` layer, the
// double-interface panes (`general_settings_panel`, plus the `ai_backend_supervisor` the
// panes drive) and the onboarding `tutorial` subsystem — now lives in the standalone
// `ms-settings-ui` crate. The launcher's settings page and the studio's Settings tab render
// the SAME panes, so neither could own them. The re-exports keep every
// `crate::settings_shared::…` / `crate::general_settings_panel::…` /
// `crate::ai_backend_supervisor::…` / `crate::tutorial::…` path valid without touching a
// call site.
pub(crate) use ms_settings_ui::{ai_backend_supervisor, general_settings_panel, settings_shared, storage_mode_job};

// The launcher shell — project catalogue, the detached "New Project" window with every
// download/stitch/reline/batch flow, PSD import, the import/export pages, the first-run
// language modal and the launcher settings page — now lives in the standalone
// `ms-launcher` crate. It is a screen of the main window: the studio never names it, only the
// main window (`shell/`) and `web_entry.rs` host it. The re-export keeps every `crate::launcher::…`
// path valid without touching a call site.
pub(crate) use ms_launcher as launcher;
// Gated exactly as the module was. The root `tutorial` feature FORWARDS to
// `ms-settings-ui/tutorial` (see `Cargo.toml`): features are not inherited, so without the
// forward this re-export would not exist while the binary's own `mark` sites compiled in.
// The overlay demo is the `tutorial_test` example of `ms-settings-ui`, built with that
// crate's `tutorial` feature (`required-features`).
#[cfg(feature = "tutorial")]
pub(crate) use ms_settings_ui::tutorial;
// Startup monitor + window geometry contract (`Window` section of `user_config.json`) now
// lives in the standalone `ms-window-geometry` crate — it is also the only direct user of
// `winit`, which left the binary's manifest with it. The re-export keeps every
// `crate::window_geometry::…` path valid without touching call sites. Native-only: it talks
// to winit monitors and OS windows, neither of which exists on web.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use ms_window_geometry as window_geometry;

// `runtime_log` and `trace` now live in the standalone `ms-log` crate. These
// re-exports keep the existing `crate::runtime_log::…` / `crate::trace::…` module
// paths and the `crate::trace_log!` macro path valid across the whole crate without
// touching call sites.
pub(crate) use ms_log::{runtime_log, trace, trace_log};

// `backend_ipc` now lives in the standalone `ms-backend-ipc` crate. The re-export keeps
// every existing `crate::backend_ipc::…` path valid across the binary without touching
// call sites, exactly like the `ms-log` shim above.
pub(crate) use ms_backend_ipc as backend_ipc;

// `ai_install_probe` and `python_manager` now live in the standalone `ms-sysprobe` crate.
// The re-exports keep `crate::ai_install_probe::…` and `crate::python_manager::…` valid
// without touching call sites.
pub(crate) use ms_sysprobe::{ai_install_probe, python_manager};

// `hf_token` (the process-wide Hugging Face access token backed by the OS secret store,
// seeded at startup by `seed_hf_token_from_secret_store`) moved into the same crate. The
// re-export keeps `crate::hf_token::…` valid unchanged.
pub(crate) use ms_sysprobe::hf_token;

// `memory_manager` now lives in the standalone `ms-memory` crate. It sits BELOW `config`
// (which reads `MemoryProfile` from it), so it had to leave the binary before `config` can.
// The re-export keeps every `crate::memory_manager::…` path valid without touching call sites.
pub(crate) use ms_memory as memory_manager;

// The process-wide storage backend selection now lives in the `ms-storage` crate
// (`ms_storage::global`). The re-export keeps every `crate::storage::…` path valid
// across the binary without touching call sites. It sits BELOW `config` (which reads
// and writes `user_config.json` through it), so it had to leave the binary first.
pub(crate) use ms_storage::global as storage;

// `config` — the project's global config / runtime-path hub — now lives in the
// standalone `ms-config` crate, together with the crate-root module its default trees
// name for `General.enabled_tabs`: `app_tab`. The two re-exports keep every existing
// `crate::config::…` and `crate::app_tab::…` path valid without touching call sites, and
// with them the `tabs::AppTab` re-export layered on top.
pub(crate) use ms_config as config;
pub(crate) use ms_config::app_tab;

// `locale_store` (the editable on-disk `locale/` catalog, whose active tag comes out of
// `General.ui_language`) moved down into the same crate: it sits directly on the
// `user_config.json` border that crate owns. The re-export keeps `crate::locale_store::…`
// valid without touching call sites. It keeps its native-only gate: on wasm there is no
// folder next to an executable, and `web_entry.rs` installs the embedded catalog directly.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use ms_config::locale_store;

// The desktop installer/updater subsystem now lives in the standalone `ms-installer`
// crate. The re-export keeps every `crate::installer::…` path valid across the binary and
// the launcher without touching call sites. Native-only, exactly as the module was: it
// installs the managed Python environment and replaces the shipped executable, and its
// modules are compiled out on wasm inside the crate itself.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use ms_installer as installer;
// The `--check-venv` readiness check moved into the same crate: it reads the installer's
// own package requirements and shares two predicates with its repair worker, so a copy
// outside would be free to drift. The re-export keeps `crate::venv_check::…` valid.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use ms_installer::venv_check;

// Pure version-string composition/stripping, shared verbatim with `build.rs` through
// `include!("crates/ms-config/src/version_format.rs")`. It moved into `ms-config` so the
// installer and launcher crates can reach it without going through the binary. Defines
// the human/machine split: `MS_APP_VERSION` (composed at build time) is for display,
// `CARGO_PKG_VERSION` is for every comparison and cross-process parse.
pub(crate) use ms_config::version_format;

// The reusable UI-primitive layer — `crates/ms-widgets/src/` plus two neighbouring leaves of the
// same layer: `ui_fonts` (the single owner of the bundled `fonts/ui` stack) and
// `bubble_status` (the egui half of the bubble-status feature: it paints a rule's border and
// re-exports the GUI-free model from `ms-config`) — now lives in the standalone `ms-widgets`
// crate. The re-exports keep every `crate::widgets::…`, `crate::ui_fonts::…` and
// `crate::bubble_status::…` path valid without touching call sites. The crate knows nothing
// about the project domain: `canvas` and the tabs depend on IT, never the other way round.
pub(crate) use ms_widgets as widgets;
pub(crate) use ms_widgets::{bubble_status, ui_fonts};
// The configurable-hotkey registry moved into `ms-widgets` (it is an egui-input
// primitive over `AppTab`, and the `translation` tab crate registers specs with it).
// This re-export keeps every `crate::input_manager_v2::…` path valid in `studio.rs` and the
// settings hotkeys pane.
pub(crate) use ms_widgets::input_manager_v2;

// The shared runtime models — the bubble model, the layer document, the clean-overlay
// model and the text-mask model — now live in the standalone `ms-models` crate, together
// with `page_view` (the source-page view model, a crate-root module of the same layer).
// The two re-exports keep every `crate::models::…` and `crate::page_view::…` path valid
// without touching call sites. The crate sits ABOVE `ms-project` (it loads and saves the
// chapter domain model) and BELOW the canvas and the tabs.
pub(crate) use ms_models as models;
pub(crate) use ms_models::page_view;

// The canvas facade — page rendering, bubble editing and clean-overlay painting — now lives
// in the standalone `ms-canvas` crate. The re-export keeps every `crate::canvas::…` path
// valid without touching call sites. It DECLARES the `CanvasHooks` trait that the tabs
// IMPLEMENT, so the dependency points down only and no cycle appears.
pub(crate) use ms_canvas as canvas;

// `page_ops` — the journalled structural page transaction — now lives in the standalone
// `ms-page-ops` crate, and `project` — the chapter domain model — in `ms-project`. The two
// re-exports keep every `crate::page_ops::…` and `crate::project::…` path valid across the
// binary without touching call sites.
//
// `Page` and `ProjectPaths` were DECLARED in `project.rs` and are now declared in
// `ms-page-ops` and re-exported by `ms-project`, so `crate::project::{Page, ProjectPaths}`
// still resolves. They had to move: `ProjectData::load` resolves a pending page-op journal
// before any reconcile pass runs (a contract that cannot be hoisted into callers without
// turning a crash-safety invariant into a convention), so `ms-project` depends on
// `ms-page-ops` and not the other way round.
pub(crate) use ms_page_ops as page_ops;
pub(crate) use ms_project as project;
// The projects-root catalogue scan (`list_titles`/`list_chapters`/
// `validate_project_dir_for_startup`) moved into `ms-project` as well: it is the same
// domain one level up from the chapter load, and `ms_config`'s path constants were its
// only dependency. The re-export keeps every `crate::project_scan::…` path valid.
pub(crate) use ms_project::project_scan;

// `text_punctuation` now lives in the config-free `ms-text-util` crate. Re-export
// keeps `crate::text_punctuation::…` valid across the binary. The crate no longer
// reads user config itself; `seed_hanging_punctuation_from_config` seeds it at startup.
pub(crate) use ms_text_util::text_punctuation;

// Typesetting-language model + process-global selection live in `ms-text-util`.
// The crate is config-free; `seed_text_language_from_config` seeds the user's
// choice at startup, mirroring `seed_hanging_punctuation_from_config`.
pub(crate) use ms_text_util::language;

/// Web entry, called by the wasm32 `fn main` of the `manhwastudio_rs` binary: boots the eframe
/// `WebRunner` on the page canvas. Real startup lives in `web_entry.rs`; the native `startup`
/// module (launcher/installer flow) is compiled out.
#[cfg(target_arch = "wasm32")]
pub fn web_main() {
    web_entry::start();
}

/// Native process entry, called by the desktop `fn main` of the `manhwastudio_rs` binary: runs
/// the whole startup flow (CLI parsing, service flags, launcher / studio windows) and logs how
/// the process ended. Service flags may terminate the process directly with their own exit code
/// (`std::process::exit`) instead of returning.
///
/// # Errors
/// Returns the fatal startup/runtime error of `startup::run_native` (already logged here), which the
/// binary's `main` turns into a non-zero exit code.
#[cfg(not(target_arch = "wasm32"))]
pub fn native_main() -> anyhow::Result<()> {
    let result = startup::run_native();
    match &result {
        Ok(()) => runtime_log::log_info("manhwastudio_rs exited normally"),
        Err(err) => runtime_log::log_error(format!("fatal startup/runtime error: {err:#}")),
    }
    result
}
