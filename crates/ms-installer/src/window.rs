/*
File: window.rs

Purpose:
The one window-context preparation shared by every standalone `eframe` window of this crate
(installer, existing-install, uninstall, update, missing-environment prompt).

Key items:
- `prepare_window_context()`: called first in every window creator closure.

Notes:
Every window gets the studio theme (`ms_theme`), the bundled UI fonts and the process-global UI
scale (`ms_widgets::ui_scale`), like every product window of the app; the scale read touches the
in-memory slot only (no config I/O on the GUI thread), so a window created before the startup
seed (the Windows `--uninstall` service mode) renders at the default 100 %.
*/

use eframe::egui;

/// Prepares a fresh window context: applies the studio theme (`ms_theme::apply`), mounts the
/// bundled UI font stack (`ui_fonts::Tier::Core`) and applies the process-global UI scale
/// (`ui_scale::apply_ui_scale_to_context`). Call it exactly once, from the window's
/// `eframe::run_native` creator closure, before the app is built.
pub fn prepare_window_context(ctx: &egui::Context) {
    ms_theme::apply(ctx);
    ms_widgets::ui_fonts::install(ctx, ms_widgets::ui_fonts::Tier::Core);
    ms_widgets::ui_scale::apply_ui_scale_to_context(ctx);
}
