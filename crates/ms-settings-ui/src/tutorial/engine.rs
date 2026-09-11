/*
File: src/tutorial/engine.rs

Purpose:
Reusable in-app tutorial / onboarding overlay engine for egui 0.35. It dims the
whole viewport except a rectangular "hole" around one target element (or the
union of a group), draws a dashed outline with padding around the hole, and
shows a text callout beside the hole (biased toward the screen centre) with an
arrow pointing from the callout to the highlight.

Design contract (why it is built this way):
- Targeting is decoupled from the UI. The UI code records the on-screen `Rect`
  of any addressable element once per frame via `TutorialRegistry::mark(key,
  rect)`; a step then references those elements by string key. Pointing the
  tutorial at a real widget therefore needs only that single `mark` line at the
  widget site — no restructuring of the surrounding UI.
- Input is blocked by an OVERLAPPING HITBOX, not by manually disabling widgets.
  A single full-viewport `Area` on `Order::Middle` (above the panels on
  `Order::Background`) allocates one `Sense::click_and_drag` rect over the whole
  screen. egui's hit-test walks layers top-to-bottom and, once a hit covers the
  pointer's search area, drops every widget in the layers beneath it from BOTH
  the click target and the hover set (`WidgetHits::contains_pointer`, which is
  what egui uses for hovering, otherwise keeps lower layers). A full-viewport
  sensor covers that search area at every point, so no panel widget receives
  hover or click. This is why a single full-screen hitbox is used rather than
  four strips around the hole: four strips leave the search area uncovered near
  the hole and at the seams, and hover then leaks to the widgets underneath.
- Consequence: the highlighted element is spotlighted (its region is not dimmed)
  but is itself inert, exactly like everything else outside — the requirement is
  that nothing under the overlay reacts. The dim is painted as four strips around
  the hole purely for the VISUAL cut-out; the hitbox is separate and full-screen.
- Decoration (dashed outline + arrow) is painted through `Context::layer_painter`
  on `Order::Tooltip`, which registers NO widget, so it adds no hitbox.

Key structures:
- `TutorialRegistry` — per-frame map of `&'static str` key -> `Rect`.
- `TutorialStep` — target key(s) + title + body for one step.
- `Tutorial` — ordered step list, current index, active flag, hole padding.

Key functions:
- `Tutorial::render` — draws the whole overlay and advances on button clicks.

Notes:
Verified against egui 0.35 APIs: `Context::viewport_rect` (not the removed
`screen_rect`), `Area`/`Order`, `Ui::allocate_rect`, `Shape::dashed_line`,
`Painter::arrow`, `Context::layer_painter`.
*/

// This is a reusable overlay engine with a deliberately broad public surface
// (start/stop/is_active/sync/render, group targeting, padding). Any single
// consumer — the standalone demo bin or a surface controller — exercises only a
// subset, so unused-item lints here are false positives for a shared engine
// rather than genuinely dead code.
#![allow(dead_code)]

use std::collections::HashMap;

use std::f32::consts::FRAC_1_SQRT_2;

use eframe::egui::{
    self, Align, Area, Color32, Context, Id, LayerId, Layout, Order, Pos2, Rect, Sense, Shape,
    Stroke, Vec2,
};

/// Default black-alpha for the dim outside the highlight. Surfaces with a
/// content-rich backdrop (the launcher's animated wall) can lighten this via
/// [`Tutorial::with_dim_alpha`] so the background stays visible under the tour.
const DEFAULT_DIM_ALPHA: u8 = 190;
/// Bright accent used for the dashed outline and the pointer arrow.
const ACCENT_COLOR: Color32 = Color32::from_rgb(255, 206, 84);
/// Callout button fill on hover — a subtle lift, NOT egui's default bright
/// highlight (which reads as a jarring white flash over the dark overlay).
const CALLOUT_BTN_HOVER_FILL: Color32 = Color32::from_rgb(70, 72, 78);
/// Callout button fill while pressed.
const CALLOUT_BTN_ACTIVE_FILL: Color32 = Color32::from_rgb(92, 94, 100);
/// Callout button growth on hover/press, in points (the "gets slightly bigger"
/// affordance without a colour change).
const CALLOUT_BTN_EXPANSION: f32 = 1.5;
/// Dashed-outline stroke width in points.
const OUTLINE_WIDTH: f32 = 2.5;
/// Dash / gap lengths of the outline in points.
const OUTLINE_DASH: f32 = 8.0;
const OUTLINE_GAP: f32 = 5.0;
/// Fixed callout content width (points); fixing it keeps placement deterministic.
const CALLOUT_WIDTH: f32 = 300.0;
/// Fixed arrow length (points) from the callout anchor to the highlight edge.
const ARROW_LEN: f32 = 64.0;

/// Per-frame lookup table from a stable element key to its current on-screen
/// `Rect`. The UI rebuilds it every frame: call `begin_frame` before the UI is
/// built, then `mark` at each addressable widget site.
#[derive(Default, Debug)]
pub struct TutorialRegistry {
    rects: HashMap<&'static str, Rect>,
}

impl TutorialRegistry {
    /// Drop all recorded rects. Call once at the top of each frame.
    pub fn begin_frame(&mut self) {
        self.rects.clear();
    }

    /// Record the current-frame rect of the element identified by `key`. This is
    /// the only line a widget site needs to add to become tutorial-addressable.
    pub fn mark(&mut self, key: &'static str, rect: Rect) {
        self.rects.insert(key, rect);
    }

    /// Whether `key` was marked this frame — used by step gates that wait for an
    /// element to appear.
    #[must_use]
    pub fn contains(&self, key: &'static str) -> bool {
        self.rects.contains_key(key)
    }

    /// Bounding union of the rects for every present key in `keys`, or `None`
    /// when none of them was marked this frame (e.g. the target is off-screen).
    #[must_use]
    fn union(&self, keys: &[&'static str]) -> Option<Rect> {
        keys.iter()
            .filter_map(|key| self.rects.get(key).copied())
            .reduce(|acc, rect| acc.union(rect))
    }
}

/// Side effect run when a step is entered, with the app's mutable context `C`.
type EnterAction<C> = Box<dyn FnMut(&mut C)>;

/// Predicate that decides when a gated step may advance. Receives read access to
/// the app context `C` and the per-frame target registry (via [`GateCtx`]).
type Gate<C> = Box<dyn Fn(&GateCtx<'_, C>) -> bool>;

/// Where a step goes when it advances. The default is [`StepNext::Linear`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepNext {
    /// The next step in the list; finishes the tutorial if past the end.
    Linear,
    /// Jump to the step whose [`TutorialStep::id`] equals this key. An unknown id
    /// finishes the tutorial (logged) rather than panicking.
    Goto(&'static str),
    /// End the tutorial. Use on the last step of a branch that is not last in the
    /// list, so `Linear` does not fall through into the next branch.
    Finish,
}

/// A branch choice shown as its own callout button. Selecting it applies `next`.
struct TutorialChoice {
    label: String,
    next: StepNext,
}

/// How a step advances past itself.
enum Advance<C> {
    /// "Далее" is always enabled (default).
    Manual,
    /// Wait for `gate`. When `auto`, the step advances by itself as soon as the
    /// gate holds (a spinner shows meanwhile); otherwise "Далее" is shown but
    /// stays disabled until the gate holds.
    Await { gate: Gate<C>, auto: bool },
}

/// Read-only view handed to a step gate: the app context plus element presence.
pub struct GateCtx<'a, C> {
    /// The app context snapshot for this frame.
    pub ctx: &'a C,
    registry: &'a TutorialRegistry,
}

impl<C> GateCtx<'_, C> {
    /// Whether a target element was marked this frame (i.e. is on screen).
    #[must_use]
    pub fn has_target(&self, key: &'static str) -> bool {
        self.registry.contains(key)
    }
}

/// One tutorial step: which element(s) to highlight, what to say, an optional
/// side effect on entry, optional branch choices, an advance gate, and where it
/// goes next.
///
/// `on_enter` lets the tutorial drive the app's own state (open a tab, set a
/// mode, trigger an action) without the UI knowing about the tutorial. A step
/// with an empty `targets` list has no spotlight: the whole viewport is dimmed
/// and the callout is centred (used for intro/branch/summary steps).
pub struct TutorialStep<C> {
    /// Stable label so [`StepNext::Goto`] / choices can jump to this step.
    id: Option<&'static str>,
    /// Keys of the target element(s); the hole is the union of their rects.
    targets: Vec<&'static str>,
    /// Bold heading shown at the top of the callout.
    title: String,
    /// Body text of the callout.
    body: String,
    /// Runs once each time this step becomes current (see [`Tutorial::sync`]).
    on_enter: Option<EnterAction<C>>,
    /// Branch choices; when non-empty the callout shows one button per choice
    /// instead of the "Далее" row.
    choices: Vec<TutorialChoice>,
    /// Gate controlling when the step may advance.
    advance: Advance<C>,
    /// Where "Далее" / auto-advance goes.
    next: StepNext,
}

impl<C> std::fmt::Debug for TutorialStep<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TutorialStep")
            .field("id", &self.id)
            .field("targets", &self.targets)
            .field("title", &self.title)
            .field("body", &self.body)
            .field("has_on_enter", &self.on_enter.is_some())
            .field("choices", &self.choices.len())
            .field("next", &self.next)
            .finish()
    }
}

impl<C> TutorialStep<C> {
    /// Build a highlight step from target keys plus title and body text.
    #[must_use]
    pub fn new(
        targets: impl IntoIterator<Item = &'static str>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            id: None,
            targets: targets.into_iter().collect(),
            title: title.into(),
            body: body.into(),
            on_enter: None,
            choices: Vec::new(),
            advance: Advance::Manual,
            next: StepNext::Linear,
        }
    }

    /// Build a target-less step: no spotlight, whole viewport dimmed, callout
    /// centred. For intros, branch prompts, and summaries.
    #[must_use]
    pub fn message(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self::new(std::iter::empty(), title, body)
    }

    /// Give the step a stable label so choices / [`StepNext::Goto`] can target it.
    #[must_use]
    pub fn id(mut self, id: &'static str) -> Self {
        self.id = Some(id);
        self
    }

    /// Attach a side effect run when this step is entered, e.g. to open a tab or
    /// (via a command sink in `C`) trigger an action. Runs on forward entry AND
    /// on "Назад" re-entry, so keep it idempotent.
    #[must_use]
    pub fn on_enter(mut self, action: impl FnMut(&mut C) + 'static) -> Self {
        self.on_enter = Some(Box::new(action));
        self
    }

    /// Add a branch choice button that jumps to the step with id `goto`. Repeat
    /// for multiple choices; any choice replaces the normal "Далее" row.
    #[must_use]
    pub fn choice(mut self, label: impl Into<String>, goto: &'static str) -> Self {
        self.choices.push(TutorialChoice {
            label: label.into(),
            next: StepNext::Goto(goto),
        });
        self
    }

    /// Auto-advance as soon as `gate` holds (a spinner shows while waiting). Use
    /// for steps that trigger an async action and wait for it to finish.
    #[must_use]
    pub fn await_gate(mut self, gate: impl Fn(&GateCtx<'_, C>) -> bool + 'static) -> Self {
        self.advance = Advance::Await {
            gate: Box::new(gate),
            auto: true,
        };
        self
    }

    /// Show "Далее" but keep it disabled until `gate` holds (user controls pace).
    #[must_use]
    pub fn gated(mut self, gate: impl Fn(&GateCtx<'_, C>) -> bool + 'static) -> Self {
        self.advance = Advance::Await {
            gate: Box::new(gate),
            auto: false,
        };
        self
    }

    /// Override where the step advances to (default [`StepNext::Linear`]).
    #[must_use]
    pub fn link(mut self, next: StepNext) -> Self {
        self.next = next;
        self
    }

    /// Shorthand for `.link(StepNext::Finish)` — end the tutorial after this step.
    #[must_use]
    pub fn finish(self) -> Self {
        self.link(StepNext::Finish)
    }
}

/// What a callout button requested this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CalloutAction {
    /// Advance following the current step's `next` link.
    Advance,
    /// Go back to the previously shown step.
    Back,
    /// End the tutorial immediately.
    Stop,
    /// Jump per a chosen branch.
    Go(StepNext),
}

/// Enabled/disabled/spinner state of the "Далее" button for the current step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NextButton {
    /// Shown and clickable.
    Enabled,
    /// Shown but disabled (a manual gate is not yet satisfied).
    Disabled,
    /// Replaced by a spinner + "Ожидание…" (auto gate is waiting).
    Waiting,
    /// Not shown at all (the step uses choice buttons instead).
    Hidden,
}

/// The zone of the viewport (relative to its centre) that the highlight sits in.
/// The viewport is split into 8 sectors by rays from the centre to the points
/// that divide each side into equal thirds: the middle third of a side is a
/// straight zone; the outer thirds of two adjacent sides form a corner zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Zone {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

/// Resolved geometry for one step's callout + arrow.
#[derive(Clone, Copy, Debug)]
struct CalloutPlacement {
    /// Top-left of the callout box.
    pos: Pos2,
    /// Arrow tail (on the callout's element-facing edge/corner).
    tail: Pos2,
    /// Arrow tip (on the highlight's centre-facing edge/corner).
    tip: Pos2,
}

/// Ordered tutorial runner plus overlay renderer, parameterised by the app
/// context `C` that step `on_enter` side effects mutate.
pub struct Tutorial<C> {
    steps: Vec<TutorialStep<C>>,
    /// Visited step indices; the last entry is the current step. A stack (not a
    /// single index) so "Назад" is correct across branch jumps.
    history: Vec<usize>,
    active: bool,
    /// Extra points added around the target rect before dimming/outlining.
    padding: f32,
    /// Callout size measured last frame, used to anchor it against the fixed
    /// arrow this frame (its content changes only on step change, so it is
    /// stable frame-to-frame; only the first frame of a step uses the estimate).
    last_callout_size: Vec2,
    /// Black-alpha of the dim outside the highlight (see [`DEFAULT_DIM_ALPHA`]).
    dim_alpha: u8,
    /// Optional override for the callout box fill. `None` keeps egui's themed
    /// popup fill; a surface with a light dim (the launcher) sets an opaque tint
    /// so the callout text stays readable over the visible backdrop.
    callout_tint: Option<Color32>,
    /// Whether the current step's `on_enter` side effect has already run.
    entered: bool,
    /// Cached result of the current step's gate, refreshed each `sync` (render
    /// has no `&C`, so it reads this to enable/disable "Далее").
    gate_satisfied: bool,
    /// True when no step branches or gates — enables the "N / M" step counter.
    /// Branched tutorials show only the visited-step ordinal.
    linear: bool,
}

impl<C> std::fmt::Debug for Tutorial<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tutorial")
            .field("steps", &self.steps.len())
            .field("history", &self.history)
            .field("active", &self.active)
            .field("entered", &self.entered)
            .finish()
    }
}

impl<C> Tutorial<C> {
    /// Create an inactive tutorial from an ordered list of steps.
    #[must_use]
    pub fn new(steps: Vec<TutorialStep<C>>) -> Self {
        // A tutorial is "linear" (eligible for the N/M counter) only when every
        // step falls through in order with no branch buttons and no jump/finish.
        let linear = steps.iter().all(|step| {
            step.choices.is_empty() && matches!(step.next, StepNext::Linear)
        });
        Self {
            steps,
            history: Vec::new(),
            active: false,
            padding: 6.0,
            last_callout_size: Vec2::new(CALLOUT_WIDTH + 20.0, 130.0),
            dim_alpha: DEFAULT_DIM_ALPHA,
            callout_tint: None,
            entered: false,
            gate_satisfied: false,
            linear,
        }
    }

    /// Override the dim strength (black-alpha, 0..=255). Lower it on surfaces
    /// whose backdrop should stay visible under the tour (e.g. the launcher).
    #[must_use]
    pub fn with_dim_alpha(mut self, alpha: u8) -> Self {
        self.dim_alpha = alpha;
        self
    }

    /// Override the callout box fill (see [`Tutorial::callout_tint`]). Use an
    /// opaque-ish tint on surfaces with a light dim so the text stays legible.
    #[must_use]
    pub fn with_callout_tint(mut self, tint: Color32) -> Self {
        self.callout_tint = Some(tint);
        self
    }

    /// Whether the overlay is currently shown.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Start (or restart) the tutorial from the first step.
    pub fn start(&mut self) {
        if self.steps.is_empty() {
            return;
        }
        self.history = vec![0];
        self.active = true;
        self.entered = false;
        self.gate_satisfied = false;
    }

    /// Stop the tutorial and hide the overlay.
    pub fn stop(&mut self) {
        self.active = false;
    }

    /// Index of the current step (top of the history stack). Only meaningful
    /// while active; defaults to 0 otherwise.
    fn current(&self) -> usize {
        self.history.last().copied().unwrap_or(0)
    }

    /// Find a step by its `id` label.
    fn index_of(&self, id: &'static str) -> Option<usize> {
        self.steps.iter().position(|step| step.id == Some(id))
    }

    /// Advance/drive the tutorial once per frame. Call at the START of the frame,
    /// before building the UI, with the app context `C` and this frame's registry
    /// (the previous frame's marks — begin_frame has not run yet).
    ///
    /// On the frame a step is entered it runs `on_enter` and does NOT evaluate its
    /// gate (so an action triggered in `on_enter` takes effect first). On later
    /// frames it evaluates the gate: an auto-gate advances the step; a manual gate
    /// only caches its result so `render` can enable "Далее". At most one step is
    /// taken per frame, so every step gets its own entry frame.
    pub fn sync(&mut self, app: &mut C, registry: &TutorialRegistry) {
        if !self.active {
            return;
        }
        let idx = self.current();
        if !self.entered {
            self.entered = true;
            self.gate_satisfied = false;
            if let Some(step) = self.steps.get_mut(idx)
                && let Some(action) = step.on_enter.as_mut()
            {
                action(app);
            }
            return;
        }

        let Some(step) = self.steps.get(idx) else {
            self.active = false;
            return;
        };
        let (satisfied, auto, next) = match &step.advance {
            Advance::Manual => (true, false, step.next),
            Advance::Await { gate, auto } => {
                let gate_ctx = GateCtx {
                    ctx: &*app,
                    registry,
                };
                (gate(&gate_ctx), *auto, step.next)
            }
        };
        self.gate_satisfied = satisfied;
        if satisfied && auto {
            self.go(next);
        }
    }

    /// Follow a `StepNext` link, ending the tutorial when it runs off the list or
    /// targets an unknown id. Re-arms `on_enter` for the entered step.
    fn go(&mut self, next: StepNext) {
        match next {
            StepNext::Linear => {
                let n = self.current() + 1;
                if n >= self.steps.len() {
                    self.active = false;
                } else {
                    self.history.push(n);
                    self.entered = false;
                }
            }
            StepNext::Goto(id) => match self.index_of(id) {
                Some(n) => {
                    self.history.push(n);
                    self.entered = false;
                }
                None => {
                    // A dangling id is a script bug; surface it loudly (the engine
                    // stays dependency-light for the demo bin, so no structured
                    // logger here) and end rather than hang.
                    eprintln!("[tutorial] step id '{id}' not found; ending tutorial");
                    self.active = false;
                }
            },
            StepNext::Finish => {
                self.active = false;
            }
        }
    }

    /// Return to the previously shown step (pops the history stack). No-op at the
    /// first step. Re-runs the previous step's `on_enter`.
    fn back(&mut self) {
        if self.history.len() > 1 {
            self.history.pop();
            self.entered = false;
        }
    }

    /// Apply a callout button action.
    fn apply(&mut self, action: CalloutAction) {
        match action {
            CalloutAction::Advance => {
                let next = self.steps[self.current()].next;
                self.go(next);
            }
            CalloutAction::Back => self.back(),
            CalloutAction::Stop => self.active = false,
            CalloutAction::Go(next) => self.go(next),
        }
    }

    /// Draw the full overlay for the current step and advance on button clicks.
    /// No-op when inactive or when the current step index is out of range.
    pub fn render(&mut self, ctx: &Context, registry: &TutorialRegistry) {
        if !self.active {
            return;
        }
        let idx = self.current();
        let Some(step) = self.steps.get(idx) else {
            self.active = false;
            return;
        };
        // Snapshot the small display data so `self` can be mutated after (button
        // clicks) without holding a borrow across the callout UI.
        let targets = step.targets.clone();
        let title = step.title.clone();
        let body = step.body.clone();
        let choices: Vec<(String, StepNext)> = step
            .choices
            .iter()
            .map(|choice| (choice.label.clone(), choice.next))
            .collect();
        // "Готово" vs "Далее": whether advancing ends the tutorial.
        let terminal = match step.next {
            StepNext::Finish => true,
            StepNext::Linear => idx + 1 >= self.steps.len(),
            StepNext::Goto(id) => self.index_of(id).is_none(),
        };
        let auto_step = matches!(step.advance, Advance::Await { auto: true, .. });
        let next_button = if !choices.is_empty() {
            NextButton::Hidden
        } else {
            match step.advance {
                Advance::Manual => NextButton::Enabled,
                Advance::Await { auto: true, .. } => NextButton::Waiting,
                Advance::Await { auto: false, .. } => {
                    if self.gate_satisfied {
                        NextButton::Enabled
                    } else {
                        NextButton::Disabled
                    }
                }
            }
        };
        // No "Назад" out of a fork or an auto-advancing step (nothing to pause on).
        let show_back = self.history.len() > 1 && choices.is_empty() && !auto_step;
        let counter = if self.linear {
            format!("{} / {}", idx + 1, self.steps.len())
        } else {
            tf!("tutorial.step_counter", step = self.history.len())
        };

        let screen = ctx.viewport_rect();
        let hole = registry
            .union(&targets)
            .map(|rect| rect.expand(self.padding));

        // 1. Dim + input-absorbing hitboxes covering the viewport minus the hole.
        Self::paint_dim(ctx, screen, hole, self.dim_alpha);

        // 2. Resolve where the callout goes and where the fixed-length arrow runs,
        // using the callout size measured last frame so the arrow keeps its length.
        let placement =
            hole.map(|hole| Self::compute_placement(hole, screen, self.last_callout_size));
        let callout_pos = match placement {
            Some(placement) => placement.pos,
            // No visible target: centre the callout, no arrow.
            None => Pos2::new(
                screen.center().x - self.last_callout_size.x * 0.5,
                screen.center().y - self.last_callout_size.y * 0.5,
            ),
        };

        // 3. Callout box (interactive, above the dim). Returns any button action.
        let (callout_rect, action) = Self::show_callout(
            ctx,
            screen,
            callout_pos,
            &title,
            &body,
            &counter,
            &choices,
            next_button,
            terminal,
            show_back,
            self.callout_tint,
        );
        self.last_callout_size = callout_rect.size();

        // 4. Decoration on top of everything: dashed outline + the fixed arrow.
        if let (Some(hole), Some(placement)) = (hole, placement) {
            Self::paint_decoration(ctx, hole, placement.tail, placement.tip);
        }

        // Keep the frame loop alive while active so gates that wait on async ops
        // are polled even when the surface is otherwise idle.
        ctx.request_repaint();

        if let Some(action) = action {
            self.apply(action);
        }
    }

    /// Paint the dim (viewport minus the hole, four strips) and absorb ALL
    /// pointer input with a single full-viewport hitbox in the same top layer, so
    /// no widget beneath the overlay — inside or outside the hole — receives
    /// hover or clicks. With no hole the whole viewport is dimmed.
    fn paint_dim(ctx: &Context, screen: Rect, hole: Option<Rect>, dim_alpha: u8) {
        let dim_color = Color32::from_black_alpha(dim_alpha);
        // Dim rectangles: everything except the hole, so the highlighted element
        // stays visually bright. These are the VISUAL cut-out only.
        let strips: Vec<Rect> = match hole {
            None => vec![screen],
            Some(hole) => vec![
                // Top strip spans the full width above the hole.
                Rect::from_min_max(screen.left_top(), Pos2::new(screen.right(), hole.top())),
                // Bottom strip spans the full width below the hole.
                Rect::from_min_max(
                    Pos2::new(screen.left(), hole.bottom()),
                    screen.right_bottom(),
                ),
                // Left strip fills the gap left of the hole, at the hole's height.
                Rect::from_min_max(
                    Pos2::new(screen.left(), hole.top()),
                    Pos2::new(hole.left(), hole.bottom()),
                ),
                // Right strip fills the gap right of the hole, at the hole's height.
                Rect::from_min_max(
                    Pos2::new(hole.right(), hole.top()),
                    Pos2::new(screen.right(), hole.bottom()),
                ),
            ],
        };

        Area::new(Id::new("tutorial_blocker"))
            .order(Order::Middle)
            .fixed_pos(screen.min)
            .constrain(false)
            .movable(false)
            .interactable(true)
            .show(ctx, |ui| {
                // Widen the clip so first-frame painting/sensing is not truncated
                // to a zero-size initial area rect.
                ui.set_clip_rect(screen);
                for strip in &strips {
                    if strip.is_positive() {
                        ui.painter().rect_filled(*strip, 0.0, dim_color);
                    }
                }
                // One full-viewport sensor. Because it covers the pointer's search
                // area everywhere, egui's hit-test drops every lower-layer widget
                // from both the click target and the hover set — pure overlap, no
                // per-widget disabling. Four strips would leave gaps near the hole
                // through which hover leaks, so a single full sensor is used.
                ui.allocate_rect(screen, Sense::click_and_drag());
            });
    }

    /// Show the callout box at `pos` (top-left) and return its final rect plus any
    /// button action. Width is fixed so anchoring against the arrow is stable.
    #[allow(clippy::too_many_arguments)]
    // Kept flat on purpose: bundling these small primitives into a struct would
    // add indirection without clarifying this single call site.
    fn show_callout(
        ctx: &Context,
        screen: Rect,
        pos: Pos2,
        title: &str,
        body: &str,
        counter: &str,
        choices: &[(String, StepNext)],
        next_button: NextButton,
        terminal: bool,
        show_back: bool,
        callout_tint: Option<Color32>,
    ) -> (Rect, Option<CalloutAction>) {
        // constrain(false): keep the exact position so the arrow enters/leaves at
        // the computed anchor. Placement is biased toward the centre, so the box
        // stays on-screen without clamping (which would break the fixed arrow).
        let inner = Area::new(Id::new("tutorial_callout"))
            .order(Order::Foreground)
            .fixed_pos(pos)
            .constrain(false)
            .movable(false)
            .interactable(true)
            .show(ctx, |ui| {
                ui.set_clip_rect(screen);
                // Keep the themed popup stroke/shadow/rounding; only override the
                // fill when a surface asks for an opaque tint (readability under a
                // light dim).
                let mut frame = egui::Frame::popup(ui.style());
                if let Some(tint) = callout_tint {
                    frame = frame.fill(tint);
                }
                frame
                    .show(ui, |ui| {
                        ui.set_width(CALLOUT_WIDTH);
                        // Tame the nav buttons' hover: a subtle darker lift + a
                        // small growth instead of egui's default bright fill,
                        // which flashes white over the dim.
                        let widgets = &mut ui.visuals_mut().widgets;
                        widgets.hovered.bg_fill = CALLOUT_BTN_HOVER_FILL;
                        widgets.hovered.weak_bg_fill = CALLOUT_BTN_HOVER_FILL;
                        widgets.hovered.expansion = CALLOUT_BTN_EXPANSION;
                        widgets.active.bg_fill = CALLOUT_BTN_ACTIVE_FILL;
                        widgets.active.weak_bg_fill = CALLOUT_BTN_ACTIVE_FILL;
                        widgets.active.expansion = CALLOUT_BTN_EXPANSION;
                        let mut action = None;
                        ui.strong(title);
                        ui.add_space(6.0);
                        ui.label(body);
                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(4.0);
                        // Counter on its OWN line: at the fixed callout width the
                        // 3-button footer (Пропустить/Назад/Готово) can grow left
                        // past a same-line counter and overlap it, so the two
                        // never share a row.
                        ui.weak(counter);
                        ui.add_space(2.0);
                        if choices.is_empty() {
                            Self::show_nav_row(ui, next_button, terminal, show_back, &mut action);
                        } else {
                            Self::show_choice_buttons(ui, choices, &mut action);
                        }
                        action
                    })
                    .inner
            });

        (inner.response.rect, inner.inner)
    }

    /// Render the standard footer row: "Далее" (per `next_button`), optional
    /// "Назад", and "Пропустить", right-aligned.
    fn show_nav_row(
        ui: &mut egui::Ui,
        next_button: NextButton,
        terminal: bool,
        show_back: bool,
        action: &mut Option<CalloutAction>,
    ) {
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let next_label = if terminal { t!("tutorial.done_button") } else { t!("tutorial.next_button") };
                match next_button {
                    NextButton::Enabled => {
                        if ui.button(next_label).clicked() {
                            *action = Some(CalloutAction::Advance);
                        }
                    }
                    NextButton::Disabled => {
                        ui.add_enabled(false, egui::Button::new(next_label));
                    }
                    NextButton::Waiting => {
                        ui.add(egui::Spinner::new().size(16.0));
                        ui.label(t!("tutorial.waiting"));
                    }
                    NextButton::Hidden => {}
                }
                if show_back && ui.button(t!("tutorial.back_button")).clicked() {
                    *action = Some(CalloutAction::Back);
                }
                if ui.button(t!("tutorial.skip_button")).clicked() {
                    *action = Some(CalloutAction::Stop);
                }
            });
        });
    }

    /// Render a branch step's choice buttons (full width, stacked) plus a
    /// right-aligned "Пропустить".
    fn show_choice_buttons(
        ui: &mut egui::Ui,
        choices: &[(String, StepNext)],
        action: &mut Option<CalloutAction>,
    ) {
        for (label, next) in choices {
            if ui
                .add_sized([CALLOUT_WIDTH, 28.0], egui::Button::new(label))
                .clicked()
            {
                *action = Some(CalloutAction::Go(*next));
            }
        }
        ui.add_space(4.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button(t!("tutorial.skip_button")).clicked() {
                *action = Some(CalloutAction::Stop);
            }
        });
    }

    /// Resolve the callout position and the fixed-length arrow from the zone the
    /// highlight sits in.
    ///
    /// The arrow tip lands on the highlight's centre-facing edge/corner (opposite
    /// the zone), the tail is `ARROW_LEN` back toward the centre along the zone
    /// direction (straight for a side zone, 45° for a corner zone), and the
    /// callout is anchored by its element-facing edge/corner to that tail — so the
    /// arrow leaves the callout at the mirror of where it enters the highlight.
    fn compute_placement(hole: Rect, screen: Rect, callout_size: Vec2) -> CalloutPlacement {
        let zone = classify_zone(hole, screen);
        let s = FRAC_1_SQRT_2;
        // `tip` = highlight edge/corner facing the centre; `dir` = tail→tip unit
        // vector (points outward toward the highlight, i.e. in the zone direction).
        let (tip, dir) = match zone {
            Zone::Right => (
                Pos2::new(hole.left(), hole.center().y),
                Vec2::new(1.0, 0.0),
            ),
            Zone::Left => (
                Pos2::new(hole.right(), hole.center().y),
                Vec2::new(-1.0, 0.0),
            ),
            Zone::Top => (
                Pos2::new(hole.center().x, hole.bottom()),
                Vec2::new(0.0, -1.0),
            ),
            Zone::Bottom => (
                Pos2::new(hole.center().x, hole.top()),
                Vec2::new(0.0, 1.0),
            ),
            Zone::TopLeft => (hole.right_bottom(), Vec2::new(-s, -s)),
            Zone::TopRight => (hole.left_bottom(), Vec2::new(s, -s)),
            Zone::BottomRight => (hole.left_top(), Vec2::new(s, s)),
            Zone::BottomLeft => (hole.right_top(), Vec2::new(-s, s)),
        };

        let tail = tip - dir * ARROW_LEN;
        // The callout's anchor is its edge/corner facing the highlight (the +dir
        // side); place the box so that anchor sits on `tail`.
        let frac = anchor_fraction(dir);
        let pos = Pos2::new(
            tail.x - frac.x * callout_size.x,
            tail.y - frac.y * callout_size.y,
        );
        CalloutPlacement { pos, tail, tip }
    }

    /// Paint the dashed outline around the hole and the fixed arrow from `tail`
    /// (on the callout) to `tip` (on the highlight), on a non-interactive top
    /// layer so it adds no hitbox.
    fn paint_decoration(ctx: &Context, hole: Rect, tail: Pos2, tip: Pos2) {
        let painter =
            ctx.layer_painter(LayerId::new(Order::Tooltip, Id::new("tutorial_decoration")));
        let stroke = Stroke::new(OUTLINE_WIDTH, ACCENT_COLOR);

        // Dashed rectangle: pass the four corners plus a repeat of the first to
        // close the loop (dashed_line connects consecutive points only).
        let corners = [
            hole.left_top(),
            hole.right_top(),
            hole.right_bottom(),
            hole.left_bottom(),
            hole.left_top(),
        ];
        painter.extend(Shape::dashed_line(&corners, stroke, OUTLINE_DASH, OUTLINE_GAP));

        // Fixed-length arrow: tail on the callout, arrowhead on the highlight.
        painter.arrow(tail, tip - tail, stroke);
    }
}

/// Classify which of the 8 zones the highlight centre falls in.
///
/// Works in centre-relative coordinates normalised by the viewport half-extents,
/// so the ray that exits a side at normalised offset `±1/3` marks the boundary
/// between that side's middle third (a straight zone) and its outer thirds
/// (corner zones) — exactly the equal-thirds split of each side.
fn classify_zone(hole: Rect, screen: Rect) -> Zone {
    let center = screen.center();
    let half_w = (screen.width() * 0.5).max(1.0);
    let half_h = (screen.height() * 0.5).max(1.0);
    let u = (hole.center().x - center.x) / half_w;
    let v = (hole.center().y - center.y) / half_h;
    let third = 1.0 / 3.0;

    if u.abs() >= v.abs() {
        // Ray exits the left/right side; `cross` is where on that side.
        let cross = if u.abs() > f32::EPSILON { v / u.abs() } else { 0.0 };
        if cross.abs() <= third {
            if u >= 0.0 { Zone::Right } else { Zone::Left }
        } else if u >= 0.0 {
            if v < 0.0 { Zone::TopRight } else { Zone::BottomRight }
        } else if v < 0.0 {
            Zone::TopLeft
        } else {
            Zone::BottomLeft
        }
    } else {
        // Ray exits the top/bottom side.
        let cross = if v.abs() > f32::EPSILON { u / v.abs() } else { 0.0 };
        if cross.abs() <= third {
            if v >= 0.0 { Zone::Bottom } else { Zone::Top }
        } else if v < 0.0 {
            if u < 0.0 { Zone::TopLeft } else { Zone::TopRight }
        } else if u < 0.0 {
            Zone::BottomLeft
        } else {
            Zone::BottomRight
        }
    }
}

/// The callout corner/edge (as top-left-relative fractions of its size) that the
/// arrow leaves from, given the arrow direction `dir`: the side facing the
/// highlight (the `+dir` side), so a straight arrow leaves an edge midpoint and a
/// 45° arrow leaves a corner.
fn anchor_fraction(dir: Vec2) -> Vec2 {
    let frac = |component: f32| {
        if component > f32::EPSILON {
            1.0
        } else if component < -f32::EPSILON {
            0.0
        } else {
            0.5
        }
    };
    Vec2::new(frac(dir.x), frac(dir.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Ctx {
        ready: bool,
        entered: Vec<&'static str>,
    }

    fn build() -> Tutorial<Ctx> {
        Tutorial::new(vec![
            TutorialStep::message("intro", "")
                .id("intro")
                .choice("A", "a")
                .choice("B", "b"),
            TutorialStep::message("a", "")
                .id("a")
                .on_enter(|c: &mut Ctx| c.entered.push("a"))
                .await_gate(|g| g.ctx.ready),
            TutorialStep::message("a2", "").id("a2").finish(),
            TutorialStep::message("b", "").id("b").finish(),
        ])
    }

    #[test]
    fn branch_choice_jumps_to_labeled_step() {
        let mut t = build();
        t.start();
        assert!(t.is_active());
        assert_eq!(t.current(), 0);
        // A branched tutorial is not "linear" (no N/M counter).
        assert!(!t.linear);
        t.apply(CalloutAction::Go(StepNext::Goto("b")));
        assert_eq!(t.current(), 3);
        // "b" is a finish step; advancing ends the tutorial.
        t.apply(CalloutAction::Advance);
        assert!(!t.is_active());
    }

    #[test]
    fn auto_gate_waits_then_advances() {
        let reg = TutorialRegistry::default();
        let mut ctx = Ctx::default();
        let mut t = build();
        t.start();
        t.apply(CalloutAction::Go(StepNext::Goto("a")));
        assert_eq!(t.current(), 1);
        // Entry frame: on_enter runs, gate NOT evaluated (no advance).
        t.sync(&mut ctx, &reg);
        assert_eq!(ctx.entered, vec!["a"]);
        assert_eq!(t.current(), 1);
        // Gate false: stays put.
        t.sync(&mut ctx, &reg);
        assert_eq!(t.current(), 1);
        assert!(!t.gate_satisfied);
        // Gate true: auto-advances one step.
        ctx.ready = true;
        t.sync(&mut ctx, &reg);
        assert_eq!(t.current(), 2);
    }

    #[test]
    fn back_pops_history_across_a_jump() {
        let mut t = build();
        t.start();
        t.apply(CalloutAction::Go(StepNext::Goto("a")));
        assert_eq!(t.current(), 1);
        t.apply(CalloutAction::Back);
        assert_eq!(t.current(), 0);
        // Back at the first step is a no-op.
        t.apply(CalloutAction::Back);
        assert_eq!(t.current(), 0);
    }

    #[test]
    fn linear_tutorial_is_flagged() {
        let t: Tutorial<Ctx> = Tutorial::new(vec![
            TutorialStep::new(["x"], "1", ""),
            TutorialStep::new(["y"], "2", ""),
        ]);
        assert!(t.linear);
    }

    #[test]
    fn message_step_has_no_targets() {
        let step: TutorialStep<Ctx> = TutorialStep::message("t", "b");
        assert!(step.targets.is_empty());
    }
}
