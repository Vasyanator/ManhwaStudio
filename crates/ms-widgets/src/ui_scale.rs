/*
File: ui_scale.rs

Purpose:
The process-global interface scale (`General.ui_scale_percent`) and its application to an
egui context. Single owner of the live value for every window of the process: the main
window (launcher + studio screens), the startup prompt windows and the installer / update
windows all read it here.

Key items:
- `seed_ui_scale_from_user_settings()`: startup seed from the loaded user settings.
- `ui_scale_percent()`: the current value, in percent.
- `apply_ui_scale()`: a live change (settings slider): records the value and rescales a context.
- `apply_ui_scale_to_context()`: applies the current value to a freshly created context.

Notes:
It sits in `ms-widgets` (next to `ui_fonts`, the other per-context window concern) because
applying it needs egui, which `ms-config` does not have; the GUI-free read / clamp / factor
rules stay in `ms-config` (`ui_scale_percent_from_user_settings`, `ui_scale_factor_from_percent`).
Persisting a changed value is the settings pane's job (`ms-settings-ui`), not this module's.
Until the startup seed runs the value is `ms_config::UI_SCALE_PERCENT_DEFAULT` (native size).
*/

use std::sync::atomic::{AtomicU32, Ordering};

/// Process-global interface scale, in percent. The single source of truth while the
/// process runs, mirroring how the UI language lives in the `ms-i18n` runtime rather
/// than in a settings snapshot.
///
/// It must NOT be re-derived from the startup `user_settings` snapshot per window:
/// `run_main` reads that snapshot ONCE and reuses it for every window of the session,
/// so a scale changed in one surface would be invisible to the next window opened after it.
static UI_SCALE_PERCENT: AtomicU32 = AtomicU32::new(ms_config::UI_SCALE_PERCENT_DEFAULT);

/// Seeds the process-global interface scale from the startup user settings.
///
/// Called ONCE during startup, before any window that should honor the scale is created
/// (next to the other `seed_*` calls of the startup sequence). Out-of-range stored values
/// are clamped by [`ms_config::ui_scale_percent_from_user_settings`].
pub fn seed_ui_scale_from_user_settings(user_settings: &serde_json::Value) {
    UI_SCALE_PERCENT.store(ms_config::ui_scale_percent_from_user_settings(user_settings), Ordering::Relaxed);
}

/// The current process-global interface scale, in percent.
#[must_use]
pub fn ui_scale_percent() -> u32 {
    UI_SCALE_PERCENT.load(Ordering::Relaxed)
}

/// Applies an interface scale (in percent) to `ctx` via `Context::set_zoom_factor` and
/// records it as the new process-global value.
///
/// Out-of-range input is clamped by [`ms_config::ui_scale_factor_from_percent`].
/// The zoom change becomes active at the start of the next pass (egui defers it to
/// avoid jitter) and egui requests the repaint itself. Only the passed context is
/// rescaled — other open windows own separate `Context`s and pick the value up when
/// they are (re)created.
pub fn apply_ui_scale(ctx: &egui::Context, percent: u32) {
    UI_SCALE_PERCENT.store(percent, Ordering::Relaxed);
    ctx.set_zoom_factor(ms_config::ui_scale_factor_from_percent(percent));
}

/// Applies the process-global interface scale to a freshly created egui context.
///
/// Every product `eframe::run_native` creator closure calls this ONCE, next to
/// `ui_fonts::install*`. A surface that does not call it simply renders at native size.
pub fn apply_ui_scale_to_context(ctx: &egui::Context) {
    ctx.set_zoom_factor(ms_config::ui_scale_factor_from_percent(ui_scale_percent()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_scale_seed_sets_process_global_and_clamps() {
        seed_ui_scale_from_user_settings(&serde_json::json!({"General": {"ui_scale_percent": 90}}));
        assert_eq!(ui_scale_percent(), 90);
        // A hand-edited absurd value is clamped, never stored as-is.
        seed_ui_scale_from_user_settings(
            &serde_json::json!({"General": {"ui_scale_percent": 10_000}}),
        );
        assert_eq!(ui_scale_percent(), ms_config::UI_SCALE_PERCENT_MAX);
        // Restore the default so the shared global does not leak into other tests.
        seed_ui_scale_from_user_settings(&serde_json::json!({}));
        assert_eq!(ui_scale_percent(), ms_config::UI_SCALE_PERCENT_DEFAULT);
    }
}
