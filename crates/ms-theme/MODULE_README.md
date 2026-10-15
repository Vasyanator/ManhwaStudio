# Module: crates/ms-theme

## Purpose
Single owner of the egui GLOBAL style and of the STUDIO's semantic colour defaults, layered on
top of egui's stock dark theme: the base style, status (severity) text colours, the editing
chrome drawn over page images, the modal scrim, and the transparency checkerboards. Values live
here once so they can be retuned in one place.

## Architecture
A leaf crate: depends on `egui` only, no I/O, no state beyond the checkerboard tile textures
cached in each `egui::Context`'s temp data. Every UI crate may depend on it (the launcher
included); it depends on no workspace crate.

- `base_style()` is pure: egui's default `Style` with `Visuals::dark()`, where only
  `error_fg_color = status::ERROR` and `warn_fg_color = status::WARNING` differ. Screen styles
  are pure layers over it (the launcher's `launcher_style` in `crates/ms-launcher/src/theme.rs`).
- `install(ctx, style)` writes a complete style in ONE `options_mut` call: preference pinned to
  `Dark`, the same `Arc<Style>` in the dark and the light slot. Nothing of the previous style is
  inherited.
- `apply(ctx)` = `install(ctx, base_style())`, for windows without a screen layer: the separate
  prompt windows (installer, update, missing-environment prompt), which take it through
  `ms_installer::window::prepare_window_context`, and the diagnostic examples. The main
  window and the web entry install per-screen styles through `shell::style` instead.
- `with_scoped_global_style(ctx, style, f)` installs `style` for the duration of `f` and then
  restores the previous slots and preference exactly (same `Arc`s, also on unwind). Reached through
  `ms_widgets::EmbeddedWindow::content_style` (New Project, PSD import and batch processing),
  whose separate windows share the launcher's `Context`.
- Everything else is `const` data plus the checkerboard painter.

## Files and submodules
- `src/lib.rs`: crate root, `base_style`, `install`, `apply`, `with_scoped_global_style`,
  re-export of `Severity`.
- `src/status.rs`: `Severity` (`Info`/`Success`/`Warning`/`Error`) and its `color()`; the
  panel-text constants `ERROR`, `WARNING`, `SUCCESS`, `INFO`, plus `DESTRUCTIVE_ARMED_FILL` and
  `ON_STATUS_FILL` (a glyph painted on an `ERROR`/`WARNING` fill, e.g. a "!" badge).
- `src/canvas.rs`: chrome drawn over content — selection accent, neutral border, outline
  backing, crop frame/shade, split line/handle/grip, refused/occupied tool state, mask tint,
  modal scrim. Brighter than `status` by design (must read over any page).
- `src/checkerboard.rs`: `Checkerboard` presets `CANVAS`, `INK_PREVIEW_DARK`,
  `INK_PREVIEW_LIGHT` (+ the two ink-preview border colours); `color_at` for pixel buffers,
  `tile_image` for one period, `paint` for egui (one textured rect per call).

## Contracts and invariants
- **Studio UI code takes semantic colours from `ms-theme` or `ui.visuals()`, never hand-types
  them.** A status message picks a `Severity`; canvas chrome picks a `canvas` constant; a
  transparency board uses a `checkerboard` preset.
- Pixel/raster colours of images and masks (tints baked into buffers, `put_pixel` values,
  `[u8; 3]` mask colours) are data, not theme, and stay local to their owners.
- **`install` is the only writer of the egui global style in the workspace.** No other code calls
  `Context::set_theme`, `set_global_style`, `global_style_mut`, `all_styles_mut`,
  `style_mut_of`, `set_style_of` or `set_visuals*`, or writes `Options::{dark_style,
  light_style, theme_preference}` (test-only headless contexts excepted). `Ui::set_style` and
  `ui.style_mut()` stay allowed: they are local to one `Ui` subtree. The only in-frame
  temporary change is `with_scoped_global_style`, whose contract is "global style after == global
  style before".
- The global style is a pure function of the active screen: `base_style()` plus that screen's
  pure layer. The launcher layers its palette over the base, so status colours read the same in
  the studio, the launcher and the shared `ms-settings-ui` panes.
- `Style: PartialEq` compares `number_formatter` by `Arc` pointer: two independently built
  styles are never `==`. Tests copy the formatter before comparing; production code must not use
  style equality to detect "unchanged".
- No speculative tokens: every constant names a role with existing studio users. Add a token
  only for a repeated role; a one-off colour stays local.
- Checkerboard `cell_px` is never zero (only the presets construct a board). `paint` never
  panics, paints nothing for an empty/non-finite rect, anchors the pattern at the rect's
  top-left corner and keeps cells screen-constant (one texel per point).

## Editing map
- Retune a status colour: `src/status.rs` (`ERROR`/`WARNING` also flow into `Visuals` via
  `base_style`).
- Retune canvas chrome: `src/canvas.rs`.
- Change a checkerboard palette or cell size: the presets in `src/checkerboard.rs`.
- Change what the base style overrides: `base_style` in `src/lib.rs`, and keep its tests in sync.
- Change how a style reaches the context: `install` (the single writer) in `src/lib.rs`.
