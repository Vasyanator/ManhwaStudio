# Module: crates/puffin_egui

## Purpose
Vendored fork of the upstream crate `puffin_egui` 0.30.0 (EmbarkStudios/puffin), ported to
egui 0.36. It renders the in-app puffin profiler flamegraph/stats window. ManhwaStudio keeps a
local copy because upstream had no egui-0.34/0.35 release when it was vendored; the main crate depends on it by `path`
(`crates/puffin_egui`) and only behind the optional `profiling` feature.

## Architecture
Third-party GUI code, NOT a ManhwaStudio logic layer. `src/lib.rs` owns the public entry points
(`profiler_ui`, `profiler_window`, `show_viewport`, `ProfilerUi`); `flamegraph.rs` and `stats.rs`
draw the two views via egui + egui_extras; `filter.rs` is the search box; `maybe_mut_ref.rs` is a
small helper. It talks to the `puffin` crate's global profiler (frame data), not to any egui state
ManhwaStudio owns.

## Contracts and invariants
- Keep this fork as close to upstream as possible. Only apply the minimal changes egui/egui_extras
  version bumps require; do NOT restyle, refactor, or add project-style doc comments.
- The ManhwaStudio CLAUDE.md Rust rules (clippy-pedantic, typed errors, doc comments) do NOT apply
  here — this is upstream code held for compatibility only.
- egui-0.35 port deltas applied: viewport callback `|ctx, class|` → `|ui, class|` (derive
  `ctx = ui.ctx().clone()`); `ViewportClass::Embedded` → `EmbeddedWindow`;
  `CentralPanel::show(ctx,…)` → `show(ui,…)`; `InputState::raw_scroll_delta` → `smooth_scroll_delta`.
  egui_extras `TableBuilder`/`Column` API was unchanged between 0.33 and 0.35.
- egui-0.36 port delta applied: the `ui.visuals_mut().clip_rect_margin = 0.0` line in
  `flamegraph.rs` was dropped (`Visuals::clip_rect_margin` is deprecated and has no effect in 0.36,
  `egui-0.36.2/src/style.rs:1086-1087`).

## Editing map
- To re-sync with a newer upstream: replace `src/` from the upstream release and re-apply the
  version-bump deltas above (or drop the fork entirely once upstream targets the egui version the workspace uses).
- Cargo pins (egui/egui_extras = 0.36.2) live in `Cargo.toml`.
