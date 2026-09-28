/*
File: crates/ms-theme/src/lib.rs

Purpose:
Crate root of `ms-theme`, the single owner of the studio's semantic colour defaults. The studio
runs on egui's stock DARK theme; this crate layers the project's own semantic colours on top of
it and nothing more (widget styling stays egui's).

Main responsibilities:
- install the studio theme on a `Context` (`apply`)
- expose the semantic colour modules: `status`, `canvas`, `checkerboard`

Key functions:
- apply()

Notes:
Called once per studio `eframe` window, from its app-creation closure. The launcher has its own
palette (`crates/ms-launcher/src/theme.rs`) and never calls this. See `MODULE_README.md` for the
"no hand-typed semantic colours" contract.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]

pub mod canvas;
pub mod checkerboard;
pub mod status;

pub use status::Severity;

/// Installs the studio theme on `ctx`: selects [`egui::Theme::Dark`] and overrides, on the DARK
/// style only, `Visuals::error_fg_color` with [`status::ERROR`] and `Visuals::warn_fg_color` with
/// [`status::WARNING`].
///
/// Everything else stays egui's stock dark theme. Idempotent: calling it again leaves the context
/// in the same state. Cheap (one options lock per call); meant to be called once per window at
/// startup, before the first frame.
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        // Stock dark uses pure 255,0,0 / 255,143,0, which glare on dark panels; the studio's
        // softer status shades are the ones hand-picked across the tabs.
        style.visuals.error_fg_color = status::ERROR;
        style.visuals.warn_fg_color = status::WARNING;
    });
}

#[cfg(test)]
mod tests {
    use super::{apply, status};

    #[test]
    fn apply_sets_dark_theme_and_status_visuals() {
        let ctx = egui::Context::default();
        apply(&ctx);
        assert_eq!(ctx.theme(), egui::Theme::Dark);
        let dark = ctx.style_of(egui::Theme::Dark);
        assert_eq!(dark.visuals.error_fg_color, status::ERROR);
        assert_eq!(dark.visuals.warn_fg_color, status::WARNING);
    }

    #[test]
    fn apply_is_idempotent_and_leaves_the_rest_of_dark_stock() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let once = ctx.style_of(egui::Theme::Dark);
        apply(&ctx);
        let twice = ctx.style_of(egui::Theme::Dark);
        assert_eq!(*once, *twice);

        let mut expected = egui::Visuals::dark();
        expected.error_fg_color = status::ERROR;
        expected.warn_fg_color = status::WARNING;
        assert_eq!(twice.visuals, expected);
    }
}
