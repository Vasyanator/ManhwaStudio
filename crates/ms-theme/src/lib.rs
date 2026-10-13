/*
File: crates/ms-theme/src/lib.rs

Purpose:
Crate root of `ms-theme`: the single owner of the egui GLOBAL style and of the studio's semantic
colour defaults. The base style is egui's stock DARK theme plus the project's status colours;
screens layer their own pure style on top of it (the launcher in
`crates/ms-launcher/src/theme.rs`), and only `install` ever writes the result into a `Context`.

Main responsibilities:
- build the base style (`base_style`), pure
- install a complete style as the context's global style (`install`) — the ONLY global-style
  writer in the workspace
- run a closure under a temporary global style and restore the previous one exactly
  (`with_scoped_global_style`, the one in-frame exception)
- expose the semantic colour modules: `status`, `canvas`, `checkerboard`

Key functions:
- base_style(), install(), apply(), with_scoped_global_style()

Notes:
`install` writes the same style into BOTH the dark and the light slot and pins the preference to
`Dark`, so the active style never depends on the OS theme. See `MODULE_README.md` for the
"no hand-typed semantic colours" and "one style writer" contracts.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]

pub mod canvas;
pub mod checkerboard;
pub mod status;

use std::sync::Arc;

pub use status::Severity;

/// Builds the project's base style: egui's default `Style` with stock dark visuals, where only
/// `Visuals::error_fg_color` is [`status::ERROR`] and `Visuals::warn_fg_color` is
/// [`status::WARNING`]. Pure; every screen style is a layer over this value.
#[must_use]
pub fn base_style() -> egui::Style {
    let mut visuals = egui::Visuals::dark();
    // Stock dark uses pure 255,0,0 / 255,143,0, which glare on dark panels; the studio's softer
    // status shades are the ones hand-picked across the tabs.
    visuals.error_fg_color = status::ERROR;
    visuals.warn_fg_color = status::WARNING;
    egui::Style { visuals, ..Default::default() }
}

/// Installs `style` as the complete global style of `ctx`: one `options_mut` call that pins
/// `theme_preference` to `Dark` and puts the same style into the dark and the light slot.
///
/// This is the ONLY function in the workspace that writes the global style (`Ui::set_style` and
/// `ui.style_mut()` stay allowed: they are local to one `Ui` subtree). Idempotent for an equal
/// `style`; the previous style is replaced wholesale, nothing of it is inherited.
pub fn install(ctx: &egui::Context, style: egui::Style) {
    install_shared(ctx, Arc::new(style));
}

/// [`install`] for a style that is already shared (no copy of the `Style`).
fn install_shared(ctx: &egui::Context, style: Arc<egui::Style>) {
    ctx.options_mut(|options| {
        options.theme_preference = egui::ThemePreference::Dark;
        options.dark_style = Arc::clone(&style);
        options.light_style = style;
    });
}

/// Installs the base style ([`install`] of [`base_style`]). Used by windows that have no screen
/// layer of their own: the separate prompt windows (installer, update, missing environment) and
/// the studio.
pub fn apply(ctx: &egui::Context) {
    install(ctx, base_style());
}

/// Runs `f` with `style` installed as the global style, then restores the previous style slots
/// and theme preference EXACTLY (the very same `Arc`s), also when `f` unwinds.
///
/// The one in-frame exception to "the global style is a function of the active screen": a
/// separate native window that shares the screen's `Context` but must render its own look while
/// it draws (reached through `ms_widgets::EmbeddedWindow::content_style`, e.g. New Project and
/// PSD import in Separate mode). Contract: the global style after the call equals the
/// style before it. Nesting is allowed and unwinds in LIFO order.
pub fn with_scoped_global_style<R>(ctx: &egui::Context, style: Arc<egui::Style>, f: impl FnOnce() -> R) -> R {
    // RAII guard held for its drop: restores the captured slots after `f`, even on unwind.
    let _restore = ScopedStyleRestore::capture(ctx);
    install_shared(ctx, style);
    f()
}

/// Saved global-style slots of a context; writes them back on drop (RAII restore of
/// [`with_scoped_global_style`], so a panicking closure cannot leak its style).
struct ScopedStyleRestore {
    ctx: egui::Context,
    dark_style: Arc<egui::Style>,
    light_style: Arc<egui::Style>,
    theme_preference: egui::ThemePreference,
}

impl ScopedStyleRestore {
    /// Snapshots the current style slots and preference of `ctx` (cheap `Arc` clones).
    fn capture(ctx: &egui::Context) -> Self {
        let (dark_style, light_style, theme_preference) =
            ctx.options(|options| (Arc::clone(&options.dark_style), Arc::clone(&options.light_style), options.theme_preference));
        Self { ctx: ctx.clone(), dark_style, light_style, theme_preference }
    }
}

impl Drop for ScopedStyleRestore {
    fn drop(&mut self) {
        let dark_style = Arc::clone(&self.dark_style);
        let light_style = Arc::clone(&self.light_style);
        let theme_preference = self.theme_preference;
        self.ctx.options_mut(|options| {
            options.theme_preference = theme_preference;
            options.dark_style = dark_style;
            options.light_style = light_style;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{apply, base_style, install, status, with_scoped_global_style};
    use std::sync::Arc;

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
        assert_eq!(*once, with_formatter_of((*twice).clone(), &once));

        let mut expected = egui::Visuals::dark();
        expected.error_fg_color = status::ERROR;
        expected.warn_fg_color = status::WARNING;
        assert_eq!(twice.visuals, expected);
    }

    /// Characterization of the installed dark style as a WHOLE `Style` (not only its visuals):
    /// egui's default dark style with exactly `error_fg_color` / `warn_fg_color` replaced.
    #[test]
    fn ms_theme_apply_characterization() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let installed = ctx.style_of(egui::Theme::Dark);
        let mut visuals = egui::Visuals::dark();
        visuals.error_fg_color = status::ERROR;
        visuals.warn_fg_color = status::WARNING;
        let expected = with_formatter_of(egui::Style { visuals, ..Default::default() }, &installed);
        assert_eq!(*installed, expected);
        assert_eq!(*ctx.global_style(), expected);
    }

    /// `Style: PartialEq` compares `number_formatter` by `Arc` POINTER, so two independently
    /// built styles are never equal; copy `reference`'s formatter into `style` so a comparison
    /// checks every other field.
    fn with_formatter_of(mut style: egui::Style, reference: &egui::Style) -> egui::Style {
        style.number_formatter = reference.number_formatter.clone();
        style
    }

    #[test]
    fn base_style_is_stock_dark_with_status_colours_only() {
        let base = base_style();
        let mut visuals = egui::Visuals::dark();
        visuals.error_fg_color = status::ERROR;
        visuals.warn_fg_color = status::WARNING;
        let expected = egui::Style { visuals, ..Default::default() };
        assert_eq!(base, with_formatter_of(expected, &base));
    }

    #[test]
    fn install_sets_both_slots_and_dark_preference() {
        let ctx = egui::Context::default();
        let mut style = base_style();
        style.spacing.item_spacing = egui::vec2(21.0, 7.0);
        install(&ctx, style);
        assert_eq!(ctx.theme(), egui::Theme::Dark);
        let dark = ctx.style_of(egui::Theme::Dark);
        let light = ctx.style_of(egui::Theme::Light);
        assert!(Arc::ptr_eq(&dark, &light));
        assert!(Arc::ptr_eq(&dark, &ctx.global_style()));
        assert_eq!(dark.spacing.item_spacing, egui::vec2(21.0, 7.0));
    }

    #[test]
    fn install_is_idempotent_and_replaces_wholesale() {
        let ctx = egui::Context::default();
        let mut layered = base_style();
        layered.spacing.button_padding = egui::vec2(30.0, 30.0);
        install(&ctx, layered);
        let style = base_style();
        install(&ctx, style.clone());
        let once = ctx.global_style();
        install(&ctx, style.clone());
        let twice = ctx.global_style();
        assert_eq!(*once, with_formatter_of(style.clone(), &once));
        assert_eq!(*twice, with_formatter_of(style, &twice));
    }

    #[test]
    fn scoped_global_style_restores_the_exact_previous_slots() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let before = ctx.global_style();
        let mut scoped = base_style();
        scoped.spacing.item_spacing = egui::vec2(40.0, 40.0);
        let scoped = Arc::new(scoped);
        let seen = with_scoped_global_style(&ctx, Arc::clone(&scoped), || ctx.global_style());
        assert!(Arc::ptr_eq(&seen, &scoped));
        assert!(Arc::ptr_eq(&ctx.global_style(), &before));
        assert!(Arc::ptr_eq(&ctx.style_of(egui::Theme::Light), &before));
        assert_eq!(ctx.theme(), egui::Theme::Dark);
    }

    #[test]
    fn scoped_global_style_restores_after_a_panic() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let before = ctx.global_style();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_scoped_global_style(&ctx, Arc::new(egui::Style::default()), || panic!("scoped closure panics"));
        }));
        assert!(result.is_err());
        assert!(Arc::ptr_eq(&ctx.global_style(), &before));
    }
}
