/*
File: src/tutorial/mod.rs

Purpose:
Shared tutorial / onboarding layer. One overlay engine (`engine.rs`) drives every
surface; a surface owns a `TutorialController<C>` that maps a stable `TutorialId`
to a step script and persists per-tutorial completion.

Main responsibilities:
- promote the overlay engine from the demo bin into the main binary so launcher
  and studio surfaces can `use crate::tutorial`;
- define the stable persistence keys (`TutorialId`), the persisted progress model
  (`TutorialProgress`), and the per-surface runner (`TutorialController`);
- expose the shared "Обучение" settings pane (`draw_tutorials_pane`) reused by
  both the studio Settings tab and the launcher settings page.

Key modules:
- `engine`: the reusable dim/spotlight/callout overlay (also mounted by the demo
  bin via `#[path]`, so it must stay dependency-light: egui + std only).
- `id`: `TutorialId` central enum + stable keys + display titles + availability.
- `progress`: `TutorialProgress` completed-set + autoplay, persisted to config.
- `controller`: `TutorialController<C>` — registry + active tutorial + progress,
  with autoplay-on-entry and completion persistence.
- `settings_pane`: the surface-agnostic replay pane.
*/

pub mod controller;
pub mod engine;
pub mod id;
pub mod progress;
pub mod settings_pane;

pub use controller::TutorialController;
pub use engine::TutorialStep;
pub use id::TutorialId;
pub use progress::{TutorialProgressHandle, shared_progress};
pub use settings_pane::draw_tutorials_pane;
