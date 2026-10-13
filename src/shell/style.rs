/*
File: src/shell/style.rs

Purpose:
The global egui style of the main window as a pure function of the active screen. The shell
rebuilds the complete `Style` from scratch on every screen switch and installs it with
`ms_theme::install` (the one global-style writer); nothing is inherited from the screen before,
so a style can never leak from one screen into the next.

Key items:
- `StyleScreen`: the screens that own a style (launcher, studio).
- `style_for(screen)`: launcher = `ms_launcher::theme::launcher_style(ms_theme::base_style())`,
  studio = `ms_theme::base_style()`.
- `apply(ctx, screen)`: `ms_theme::install(ctx, style_for(screen))`.

Notes:
Compiled on every target: the web entry (`web_entry.rs`) installs the same two styles at its
launcher -> editor swap. Never compare two `Style`s to skip an install: `Style: PartialEq` compares
`number_formatter` by `Arc` pointer (egui-0.36.2/src/style.rs:57-61), so independently built
styles are never equal; the shell simply installs on every switch.
*/

/// A global style owner. The launcher style also covers the PreparingLauncher screen; Closing /
/// Exiting keep the style of the screen being left (`screen::style_screen`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StyleScreen {
    /// The launcher: its own layer over the shared base.
    Launcher,
    /// The studio: the shared base (stock dark + studio status colours).
    Studio,
}

/// The complete style of `screen`, built from scratch (pure).
#[must_use]
pub(crate) fn style_for(screen: StyleScreen) -> egui::Style {
    match screen {
        StyleScreen::Launcher => crate::launcher::theme::launcher_style(ms_theme::base_style()),
        StyleScreen::Studio => ms_theme::base_style(),
    }
}

/// Installs the style of `screen` as the global style of `ctx` (both themes, dark preference),
/// replacing whatever was installed before.
pub(crate) fn apply(ctx: &egui::Context, screen: StyleScreen) {
    ms_theme::install(ctx, style_for(screen));
}

#[cfg(test)]
mod tests {
    use super::{StyleScreen, apply, style_for};

    /// `Style: PartialEq` compares `number_formatter` by `Arc` pointer; copy `reference`'s
    /// formatter into `style` so a comparison checks every other field.
    fn with_formatter_of(mut style: egui::Style, reference: &egui::Style) -> egui::Style {
        style.number_formatter = reference.number_formatter.clone();
        style
    }

    /// Launcher -> studio -> launcher on one context ends exactly on the launcher style: every
    /// switch replaces the style wholesale, nothing of the studio style survives.
    #[test]
    fn switching_back_and_forth_ends_on_the_pure_launcher_style() {
        let ctx = egui::Context::default();
        apply(&ctx, StyleScreen::Launcher);
        apply(&ctx, StyleScreen::Studio);
        apply(&ctx, StyleScreen::Launcher);
        let installed = ctx.global_style();
        assert_eq!(*installed, with_formatter_of(style_for(StyleScreen::Launcher), &installed));
        assert_eq!(ctx.theme(), egui::Theme::Dark);
    }

    /// The studio style after a launcher -> studio switch equals the studio style installed on a
    /// fresh context: the launcher layer leaks nothing into the studio.
    #[test]
    fn studio_after_launcher_equals_studio_on_a_fresh_context() {
        let switched = egui::Context::default();
        apply(&switched, StyleScreen::Launcher);
        apply(&switched, StyleScreen::Studio);
        let fresh = egui::Context::default();
        apply(&fresh, StyleScreen::Studio);
        let after_switch = switched.global_style();
        let on_fresh = fresh.global_style();
        assert_eq!(*after_switch, with_formatter_of((*on_fresh).clone(), &after_switch));
        assert_eq!(*on_fresh, with_formatter_of(ms_theme::base_style(), &on_fresh));
    }

    /// A foreign style installed before is replaced, never merged, and a repeated install of the
    /// same screen agrees with the first.
    #[test]
    fn install_replaces_a_foreign_style_idempotently() {
        let ctx = egui::Context::default();
        let mut foreign = ms_theme::base_style();
        foreign.spacing.indent = 99.0;
        ms_theme::install(&ctx, foreign);
        apply(&ctx, StyleScreen::Launcher);
        let once = ctx.global_style();
        assert_eq!(*once, with_formatter_of(style_for(StyleScreen::Launcher), &once));
        apply(&ctx, StyleScreen::Launcher);
        let twice = ctx.global_style();
        assert_eq!(*twice, with_formatter_of((*once).clone(), &twice));
    }

    /// The two screens really differ (the launcher layer is not a no-op), so the round-trip tests
    /// above prove a switch, not a coincidence.
    #[test]
    fn launcher_and_studio_styles_differ() {
        let launcher = style_for(StyleScreen::Launcher);
        let studio = style_for(StyleScreen::Studio);
        assert_ne!(launcher, with_formatter_of(studio, &launcher));
    }
}
