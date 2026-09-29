/*
File: tabs/ps_editor/correction/shader.rs

Purpose:
The GPU side of the PS editor's VIEW-ONLY «Коррекция»: turns the model's
`BrightnessContrastParams` into an `egui-shader-layers` shader layer over the canvas, and reports
whether that library can currently render it. This file owns no GL object and calls no GL
function — the shader, its resources and their lifetime belong to the library's glow backend,
which the binary installs on the egui context at window creation and destroys on exit.

Key functions:
- `correction_pass`: the `presets::brightness_contrast` pass for one set of model parameters.
- `paint_correction_layer`: paints that pass as a `ShaderLayer` at the painter's current position.
- `CorrectionAvailability::check` / `observe`: whether the panel must say the correction cannot
  be shown; either writes the cause to `runtime_log` once per session.

Notes:
The preset computes `clamp((c - 0.5) * contrast + 0.5 + brightness, 0, 1)` on unpremultiplied,
gamma-space colour; `model.rs::apply_channel` restates that formula and its tests pin the maths.
It equals the legacy `gain * c + bias` model on screen because the canvas under the page is opaque
(framebuffer alpha 1), so unpremultiplying is a no-op there.
The library reports its own failures only through the `log` facade, which the app routes nowhere;
`CorrectionAvailability` is what puts them into `runtime_log`.
*/

use eframe::egui;
use egui_shader_layers::{Pass, ShaderLayer, presets};

use super::model::BrightnessContrastParams;

/// The shader-layer pass rendering `params`: `presets::brightness_contrast` with the model's
/// offset as its `brightness` and the model's gain as its `contrast`.
///
/// The preset's effect is compiled once per process and shared; building the pass per frame only
/// copies the two parameters into its first slot.
#[must_use]
pub(crate) fn correction_pass(params: BrightnessContrastParams) -> Pass {
    presets::brightness_contrast(params.brightness_offset, params.contrast_gain)
}

/// Paints the correction over `rect` (screen points) at the painter's current position in its
/// paint list, so it filters everything painted before it and nothing painted after it.
///
/// The painter's clip rect bounds the captured region, so callers pass a painter already clipped
/// to `rect`. With no usable glow backend on the painter's context this paints nothing (the library
/// logs why once); `CorrectionAvailability` is what surfaces that state to the user.
pub(crate) fn paint_correction_layer(painter: &egui::Painter, rect: egui::Rect, params: BrightnessContrastParams) {
    ShaderLayer::new(rect, correction_pass(params)).paint(painter);
}

/// Once-per-session reporter of whether the correction can be rendered.
///
/// Lives in the tab state (`PsEditorTabState`), so the "already logged" latch is per tab session
/// rather than global mutable state.
#[derive(Debug, Default)]
pub(crate) struct CorrectionAvailability {
    /// Set once the unavailability has been written to `runtime_log`; never cleared, so a failure
    /// that persists every frame is logged exactly once.
    failure_logged: bool,
}

impl CorrectionAvailability {
    /// Whether the correction cannot be rendered on `ctx`, so the panel must say so instead of
    /// offering sliders that do nothing.
    ///
    /// True when the shader-layer backend is not installed, has failed for the session (e.g. an
    /// unsupported GL context), or failed to build the brightness/contrast effect on the GPU (a
    /// driver rejecting the translated shader). The first time it is true, the backend status and
    /// every failure of the preset's effect are written to `runtime_log` as an error.
    #[must_use]
    pub(crate) fn check(&mut self, ctx: &egui::Context) -> bool {
        let report = unavailability_report(ctx);
        let unavailable = report.is_some();
        self.log_once(report);
        unavailable
    }

    /// [`Self::check`] for a caller that only needs the one-shot log, not the answer (the canvas
    /// pass: the library itself paints nothing when the backend is unusable).
    pub(crate) fn observe(&mut self, ctx: &egui::Context) {
        self.log_once(unavailability_report(ctx));
    }

    /// Writes `report` to `runtime_log` if it is the first one this session.
    fn log_once(&mut self, report: Option<String>) {
        if let Some(report) = self.take_unlogged(report) {
            ms_log::runtime_log::log_error(report);
        }
    }

    /// Passes `report` through the latch: returns it only the first time a report is present.
    fn take_unlogged(&mut self, report: Option<String>) -> Option<String> {
        if self.failure_logged {
            return None;
        }
        let report = report?;
        self.failure_logged = true;
        Some(report)
    }
}

/// The diagnostic text for an unusable correction on `ctx`, or `None` when it can render.
///
/// Carries the backend `status` and the error text of every `failed_effects` entry belonging to
/// the preset's effect (its label is taken from the preset itself, so a rename inside the library
/// cannot silently turn this into "never failed").
fn unavailability_report(ctx: &egui::Context) -> Option<String> {
    let status = egui_shader_layers::status(ctx);
    let preset = correction_pass(BrightnessContrastParams::IDENTITY);
    let label = preset.effect().label();
    let effect_errors: Vec<String> = egui_shader_layers::failed_effects(ctx)
        .into_iter()
        .filter(|failure| failure.label == label)
        .map(|failure| failure.error)
        .collect();
    if status.is_usable() && effect_errors.is_empty() {
        return None;
    }
    Some(format_unavailability(&status.to_string(), label, &effect_errors))
}

/// Formats the one-shot `runtime_log` error for an unusable correction. Pure, for testing.
fn format_unavailability(status: &str, effect_label: &str, effect_errors: &[String]) -> String {
    let effects = if effect_errors.is_empty() {
        "none".to_owned()
    } else {
        effect_errors.join(" | ")
    };
    format!(
        "[ps_editor] correction: the «Коррекция» shader pass cannot render; the panel shows it as \
         unavailable. Shader-layer backend status: {status}. GPU failures of effect `{effect_label}`: \
         {effects}. Possible cause: the glow backend was not installed (no GL context at window \
         creation), the GL context is unsupported (WebGL1 / GL < 3.3), or the driver rejected the \
         translated shader"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The preset reads `params(0).x` as brightness and `.y` as contrast; the model's two values
    /// must land in exactly those components, or the on-screen maths would silently swap them.
    #[test]
    fn the_model_parameters_land_in_the_preset_slots() {
        let params = BrightnessContrastParams {
            brightness_offset: -0.125,
            contrast_gain: 1.75,
        };
        assert_eq!(correction_pass(params).param(0), Some([-0.125, 1.75, 0.0, 0.0]));
    }

    /// A context the binary never installed the backend on (a test harness, a future second host)
    /// must report the correction as unavailable rather than pretend the sliders work.
    #[test]
    fn a_context_without_the_backend_reports_unavailable() {
        let report = unavailability_report(&egui::Context::default());
        let report = report.unwrap_or_default();
        assert!(report.contains("no filter backend installed"), "{report}");
    }

    /// The latch hands out the first report only: a failure that persists every frame is logged
    /// once, and frames without a failure never consume the latch.
    #[test]
    fn the_latch_passes_only_the_first_report() {
        let mut availability = CorrectionAvailability::default();
        assert_eq!(availability.take_unlogged(None), None);
        assert_eq!(availability.take_unlogged(Some("first".to_owned())).as_deref(), Some("first"));
        assert_eq!(availability.take_unlogged(Some("second".to_owned())), None);
        assert_eq!(availability.take_unlogged(None), None);
    }

    /// The log line carries the status and every effect error, so the driver's message is not lost.
    #[test]
    fn the_report_names_status_and_every_effect_error() {
        let text = format_unavailability(
            "glow: FAILED: boom",
            "brightness_contrast",
            &["0:1: syntax".to_owned(), "link".to_owned()],
        );
        for needle in ["glow: FAILED: boom", "`brightness_contrast`", "0:1: syntax | link"] {
            assert!(text.contains(needle), "missing {needle}: {text}");
        }
        assert!(format_unavailability("x", "y", &[]).contains("`y`: none"));
    }
}
