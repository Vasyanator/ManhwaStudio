/*
File: src/tutorial/controller.rs

Purpose:
Per-surface tutorial runner. Owns the per-frame target registry, the active
`Tutorial<C>`, and the shared progress handle, and maps a `TutorialId` to a step
script through a small catalog. Enforces one active tutorial per surface, drives
autoplay-on-entry, and persists completion when a tutorial finishes or is skipped.

Key structure:
- `TutorialController<C>` — generic over the surface context `C` that step
  `on_enter` side effects mutate (e.g. `LauncherState`).

Per-frame contract (mirror of the engine's own contract):
1. `begin_frame()` at the top of the frame (clears the registry).
2. optionally `maybe_autoplay(id)` on entering the surface/tab (edge-triggered by
   the caller).
3. `sync(&mut ctx)` before building the UI (runs a step's `on_enter`).
4. `mark(key, rect)` at each addressable widget while building the UI.
5. `render(ctx)` last (draws the overlay; persists completion on the finish edge).
*/

use std::sync::PoisonError;

use eframe::egui::{Color32, Context, Rect};

use super::engine::{Tutorial, TutorialRegistry, TutorialStep};
use super::id::TutorialId;
use super::progress::TutorialProgressHandle;

/// A tutorial's step-script builder. A plain `fn` pointer (no captures) since a
/// script is a pure function of its target keys and copy.
type StepsFn<C> = fn() -> Vec<TutorialStep<C>>;

/// Owns and drives the tutorial overlay for one surface (launcher, studio, …).
///
/// `C` is the surface context that a step's `on_enter` mutates to move the UI to
/// where the next highlight lives (open a tab, navigate a page).
pub struct TutorialController<C> {
    registry: TutorialRegistry,
    tutorial: Tutorial<C>,
    /// Which id the current `tutorial` was built from, so completion can be
    /// recorded against it on the finish/skip edge. `None` while idle.
    current: Option<TutorialId>,
    catalog: Vec<(TutorialId, StepsFn<C>)>,
    progress: TutorialProgressHandle,
    /// Dim strength applied to every tutorial this surface starts. `None` keeps
    /// the engine default; the launcher lowers it so its animated background
    /// stays visible under the tour.
    dim_alpha: Option<u8>,
    /// Callout box tint applied to every tutorial this surface starts. `None`
    /// keeps the themed popup fill; the launcher sets an opaque tint so text
    /// stays readable over its lightly-dimmed backdrop.
    callout_tint: Option<Color32>,
}

impl<C> TutorialController<C> {
    /// Build a controller for one surface. `catalog` maps the ids this surface
    /// can run to their step scripts; `progress` is shared with the surface's
    /// settings pane so a reset there is observed here immediately.
    #[must_use]
    pub fn new(progress: TutorialProgressHandle, catalog: Vec<(TutorialId, StepsFn<C>)>) -> Self {
        Self {
            registry: TutorialRegistry::default(),
            tutorial: Tutorial::new(Vec::new()),
            current: None,
            catalog,
            progress,
            dim_alpha: None,
            callout_tint: None,
        }
    }

    /// Lower the dim of every tutorial this surface starts (see
    /// [`Tutorial::with_dim_alpha`]).
    #[must_use]
    pub fn with_dim_alpha(mut self, alpha: u8) -> Self {
        self.dim_alpha = Some(alpha);
        self
    }

    /// Tint the callout box of every tutorial this surface starts (see
    /// [`Tutorial::with_callout_tint`]).
    #[must_use]
    pub fn with_callout_tint(mut self, tint: Color32) -> Self {
        self.callout_tint = Some(tint);
        self
    }

    /// Clear the per-frame target registry. Call once at the top of the frame.
    pub fn begin_frame(&mut self) {
        self.registry.begin_frame();
    }

    /// Record the current-frame rect of an addressable widget. One line per site.
    pub fn mark(&mut self, key: &'static str, rect: Rect) {
        self.registry.mark(key, rect);
    }

    /// Start `id` from its first step, replacing any active tutorial. No-op if
    /// `id` is not in this surface's catalog (nothing to run here).
    pub fn start(&mut self, id: TutorialId) {
        let Some((_, steps_fn)) = self.catalog.iter().find(|(cid, _)| *cid == id) else {
            return;
        };
        let mut tutorial = Tutorial::new(steps_fn());
        if let Some(alpha) = self.dim_alpha {
            tutorial = tutorial.with_dim_alpha(alpha);
        }
        if let Some(tint) = self.callout_tint {
            tutorial = tutorial.with_callout_tint(tint);
        }
        self.tutorial = tutorial;
        self.tutorial.start();
        self.current = Some(id);
    }

    /// Start `id` iff autoplay is on and `id` is not yet completed. Meant to be
    /// called edge-triggered by the caller (on entering the surface/tab), so it
    /// fires once per entry rather than every frame. No-op while another tutorial
    /// is active (single-active guard).
    pub fn maybe_autoplay(&mut self, id: TutorialId) {
        if self.tutorial.is_active() {
            return;
        }
        let (autoplay, completed) = {
            let progress = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
            (progress.autoplay(), progress.is_completed(id))
        };
        if autoplay && !completed {
            self.start(id);
        }
    }

    /// Drive the active tutorial one tick: run `on_enter` on entry, evaluate the
    /// current step's gate, and auto-advance when it holds. Call before building
    /// the UI (uses the previous frame's registry, which is still intact until
    /// `begin_frame`).
    pub fn sync(&mut self, ctx: &mut C) {
        self.tutorial.sync(ctx, &self.registry);
    }

    /// Draw the overlay for the current step and advance on button clicks. On the
    /// active -> inactive edge (last step reached or "Пропустить"), record the
    /// current id as completed and persist. Call last, after all panels.
    pub fn render(&mut self, ctx: &Context) {
        let was_active = self.tutorial.is_active();
        self.tutorial.render(ctx, &self.registry);
        // Any deactivation of an active tutorial is a finish or a skip; both mean
        // "seen", so completion is recorded uniformly.
        if was_active
            && !self.tutorial.is_active()
            && let Some(id) = self.current.take()
        {
            let mut progress = self.progress.lock().unwrap_or_else(PoisonError::into_inner);
            progress.mark_completed(id);
        }
    }
}
