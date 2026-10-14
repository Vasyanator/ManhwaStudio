/*
FILE OVERVIEW: crates/ms-models/src/lib.rs
Crate root of `ms-models`: the shared runtime models the tabs and the canvas read and
mutate. Re-exported by the binary as `crate::models`; `page_view` is re-exported
separately under its own name (`crate::page_view`).

Layer: ABOVE `ms-project` (it loads and saves the chapter domain model) and BELOW the
canvas and the tabs. egui appears here only as data types (`ColorImage` / `Color32`, and
`TextureHandle` / `Vec2` in `page_view`); nothing in this crate may take an `egui::Ui` or
an `egui::Painter` — drawing belongs to the layers above.
*/

#![warn(clippy::all)]

// Per-project autosave flush gate shared by the layer, bubbles and clean-overlay writers.
pub mod autosave_gate;
pub mod bubbles_model;
pub mod clean_assign;
pub mod clean_overlays_model;
pub mod layer_model;
// Source-page view model shared by the app shell, the canvas and the tabs: page geometry
// with its load state plus the tiled GPU residency of a decoded source page. It sits in
// this crate (and not in `studio.rs`) so `canvas` and `tabs` never reference the app shell
// upwards; producing and evicting the textures stays in `studio.rs`.
pub mod page_view;
// What the open studio session offers (`SessionCapabilities`), pushed by the root into every
// tab instead of a per-tab "is this an image set" bool.
pub mod session_caps;
pub mod text_mask_model;

pub use session_caps::SessionCapabilities;
