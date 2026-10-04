/*
File: region_edit_v2/mod.rs

Purpose:
Entry point of the on-canvas region-editing framework and of its generic AI host. The framework
replaces the detached region-editor window with a selection FRAME drawn on the canvas over the
page strip, plus two dock panels: a compact part in «Выбранный инструмент» and a main part in
its own panel. The host turns the frame plus an engine catalog into a whole `CleaningTool`.

Main responsibilities:
- declare the framework's and the host's submodules

Key submodules:
- `geometry`: GUI-free maths — size constraints, viewport clamping, page transition, arrow
- `layers`: the mask layer stack and the processed-result layer
- `frame`: `RegionFrame` — the state and the per-frame pass a tool drives
- `render`: painting of the frame, its handles, its chrome rows and the off-screen arrow
- `input`: handle hit geometry and the move/resize maths behind a drag
- `engine`: the `AiEngine` contract between the host and the engines it runs, and the one
  size-violation wording
- `engine_settings`: engine-agnostic settings helpers shared by every hosted engine (the save gate)
- `host`: `HostSpec` and `RegionEditHost` — the generic hosted tool
- `host_panels`: the host's two panel bodies
- `size_oracle` (test only): the frozen legacy size rules the live ones are swept against

Notes:
A consumer tool declares a `host::HostSpec` (id, title, id salts, log tag, engine catalog) and
builds its tool with `host::RegionEditHost::new`; its engines implement `engine::AiEngine`.
Everything is imported straight from the submodule that owns it; `render`, `input` and
`host_panels` are internal. There are deliberately no flattening re-exports here.
Design and the decisions behind it: `dev-docs/region_edit_v2_plan.md`.
*/

pub mod engine;
pub mod engine_settings;
pub mod frame;
pub mod geometry;
pub mod host;
pub mod layers;

// Internal to the framework: their items are `pub(super)` and exist only to serve `frame`.
mod input;
mod render;
// The panel bodies of `host::RegionEditHost`, split from `host.rs` for size; one type.
mod host_panels;
// TEST ONLY: the frozen pre-extension size rules the live ones are swept against.
#[cfg(test)]
pub(in crate::tools) mod size_oracle;
