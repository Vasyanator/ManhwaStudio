# Module: crates/ms-theme

## Purpose
Single owner of the STUDIO's semantic colour defaults, layered on top of egui's stock dark
theme: status (severity) text colours, the editing chrome drawn over page images, the modal
scrim, and the transparency checkerboards. Values live here once so they can be retuned in one
place.

## Architecture
A leaf crate: depends on `egui` only, no I/O, no state beyond the checkerboard tile textures
cached in each `egui::Context`'s temp data. Every studio crate may depend on it; it depends on no
workspace crate.

- `apply(ctx)` selects `Theme::Dark` and overrides, on the dark style only,
  `Visuals::error_fg_color = status::ERROR` and `Visuals::warn_fg_color = status::WARNING`.
  Nothing else of stock dark is touched (no widget restyling). It is called from the
  app-creation closure of every studio window (`src/main.rs`: studio, update check,
  missing-Python prompt, basic project chooser; `src/web_entry.rs` on the launcher→editor swap)
  and of the installer's update window (`crates/ms-installer/src/update.rs`).
- Everything else is `const` data plus the checkerboard painter.

## Files and submodules
- `src/lib.rs`: crate root, `apply`, re-export of `Severity`.
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
- The launcher has its own theme (`crates/ms-launcher/src/theme.rs`): its own chrome does not use
  this crate and `apply` is never called on a launcher window. The shared `ms-settings-ui` panes
  the launcher embeds (double-interface pattern) do use `status` colours, so a status reads the
  same in the studio and the launcher.
- No speculative tokens: every constant names a role with existing studio users. Add a token
  only for a repeated role; a one-off colour stays local.
- Checkerboard `cell_px` is never zero (only the presets construct a board). `paint` never
  panics, paints nothing for an empty/non-finite rect, anchors the pattern at the rect's
  top-left corner and keeps cells screen-constant (one texel per point).

## Editing map
- Retune a status colour: `src/status.rs` (`ERROR`/`WARNING` also flow into `Visuals` via `apply`).
- Retune canvas chrome: `src/canvas.rs`.
- Change a checkerboard palette or cell size: the presets in `src/checkerboard.rs`.
- Change what `apply` overrides on the dark style: `src/lib.rs`, and keep its tests in sync.
