/*
File: region_edit_v2/mod.rs

Purpose:
Entry point of the on-canvas region-editing framework. The framework replaces the detached
region-editor window with a selection FRAME drawn on the canvas over the page strip, plus two
dock panels: a compact part in «Выбранный инструмент» and a main part in its own panel.

Main responsibilities:
- declare the framework's submodules and re-export the surface a host tool talks to

Key submodules:
- `geometry`: GUI-free maths — size constraints, viewport clamping, page transition, arrow
- `layers`: the mask layer stack and the processed-result layer
- `frame`: `RegionFrame` — the state and the per-frame pass a tool drives
- `render`: painting of the frame, its handles, its chrome rows and the off-screen arrow
- `input`: handle hit geometry and the move/resize maths behind a drag

Notes:
A host tool imports `frame::{RegionFrame, FrameHost, FrameOutcome}` and, from `geometry`,
`FrameConstraints`, straight from the submodule that owns them; `render` and `input` are
internal to the framework. There are deliberately no flattening re-exports here: in a binary
crate a `pub use` inside a private module is unreachable and only produces an unused-import
warning.
Design and the decisions behind it: `dev-docs/region_edit_v2_plan.md`.
*/

pub mod frame;
pub mod geometry;
pub mod layers;

// Internal to the framework: their items are `pub(super)` and exist only to serve `frame`.
mod input;
mod render;
