/*
FILE OVERVIEW: crates/ms-models/src/lib.rs
Crate root of `ms-models`: the shared runtime models the tabs and the canvas read and
mutate. Re-exported by the binary as `crate::models`, so every `crate::models::...`
call site keeps working unchanged; `page_view` is re-exported separately under its own
name because it was a crate-root module, not a member of `models`.

Layer: ABOVE `ms-project` (it loads and saves the chapter domain model) and BELOW the
canvas and the tabs. egui appears here only as a pixel format (`ColorImage` / `Color32`);
nothing in this crate may take an `egui::Ui` or an `egui::Painter` — drawing belongs to
the layers above.
*/

#![warn(clippy::all)]

pub mod bubbles_model;
pub mod clean_assign;
pub mod clean_overlays_model;
pub mod layer_model;
// Source-page view model shared by the app shell, the canvas and the tabs: page geometry
// with its load state plus the tiled GPU residency of a decoded source page. It sits in
// this crate (and not in `app.rs`) so `canvas` and `tabs` never reference the app shell
// upwards; producing and evicting the textures stays in `app.rs`.
pub mod page_view;
pub mod text_mask_model;
