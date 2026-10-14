/*
File: window.rs

Purpose:
The one window-context preparation shared by every standalone `eframe` window of this crate
(installer, existing-install, uninstall, update, missing-environment prompt).

Key items:
- `WindowLook`: whether a window runs the studio theme or egui's stock visuals.
- `prepare_window_context()`: called first in every window creator closure.

Notes:
Today the windows differ in one point only, the theme: the update window and the
missing-environment prompt apply `ms_theme`, the three `install.rs` windows never did. The
difference is kept as an explicit `WindowLook` argument rather than unified, because applying
the theme there would change what those windows look like. Every window then gets the
process-global UI scale (`ms_widgets::ui_scale`), like every product window of the app; this
reads the in-memory slot only (no config I/O on the GUI thread), so a window created before the
startup seed (the Windows `--uninstall` service mode) renders at the default 100 %.
*/

use eframe::egui;

/// The visual style a standalone window starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowLook {
    /// `ms_theme::apply`: the studio theme (stock dark plus the studio status colours).
    StudioTheme,
    /// egui's own visuals, untouched (the `install.rs` windows).
    EguiDefault,
}

/// Prepares a fresh window context: applies the theme for `look`, mounts the bundled UI font
/// stack (`ui_fonts::Tier::Core`) and applies the process-global UI scale
/// (`ui_scale::apply_ui_scale_to_context`). Call it exactly once, from the window's
/// `eframe::run_native` creator closure, before the app is built.
pub fn prepare_window_context(ctx: &egui::Context, look: WindowLook) {
    match look {
        WindowLook::StudioTheme => ms_theme::apply(ctx),
        WindowLook::EguiDefault => {}
    }
    ms_widgets::ui_fonts::install(ctx, ms_widgets::ui_fonts::Tier::Core);
    ms_widgets::ui_scale::apply_ui_scale_to_context(ctx);
}
