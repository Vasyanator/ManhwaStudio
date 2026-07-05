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

/// One tutorial step: which element(s) to highlight, what to say, and an optional
/// side effect to run when the step becomes current.
///
/// `on_enter` lets the tutorial drive the app's own state (open a tab, set a
/// mode) without the UI knowing about the tutorial: the widget code is untouched;
/// the step, defined in the tutorial script, just mutates the app context.
pub struct TutorialStep<C> {
    /// Keys of the target element(s); the hole is the union of their rects.
    pub targets: Vec<&'static str>,
    /// Bold heading shown at the top of the callout.
    pub title: String,
    /// Body text of the callout.
    pub body: String,
    /// Runs once each time this step becomes current (see [`Tutorial::sync`]).
    pub on_enter: Option<EnterAction<C>>,
}

impl<C> std::fmt::Debug for TutorialStep<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TutorialStep")
            .field("targets", &self.targets)
            .field("title", &self.title)
            .field("body", &self.body)
            .field("has_on_enter", &self.on_enter.is_some())
            .finish()
    }
}

impl<C> TutorialStep<C> {
    /// Build a highlight-only step from target keys plus title and body text.
    #[must_use]
    pub fn new(
        targets: impl IntoIterator<Item = &'static str>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            targets: targets.into_iter().collect(),
            title: title.into(),
            body: body.into(),
            on_enter: None,
        }
    }

    /// Attach a side effect run when this step is entered, e.g. to open a tab so
    /// the tutorial can move the UI to where the next highlight lives.
    #[must_use]
    pub fn on_enter(mut self, action: impl FnMut(&mut C) + 'static) -> Self {
        self.on_enter = Some(Box::new(action));
        self
    }
}

/// What a callout button requested this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CalloutAction {
    /// Advance to the next step (or finish on the last step).
    Next,
    /// Go back to the previous step.
    Prev,
    /// End the tutorial immediately.
    Stop,
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
    current: usize,
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
}

impl<C> std::fmt::Debug for Tutorial<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tutorial")
            .field("steps", &self.steps.len())
            .field("current", &self.current)
            .field("active", &self.active)
            .field("entered", &self.entered)
            .finish()
    }
}

impl<C> Tutorial<C> {
    /// Create an inactive tutorial from an ordered list of steps.
    #[must_use]
    pub fn new(steps: Vec<TutorialStep<C>>) -> Self {
        Self {
            steps,
            current: 0,
            active: false,
            padding: 6.0,
            last_callout_size: Vec2::new(CALLOUT_WIDTH + 20.0, 130.0),
            dim_alpha: DEFAULT_DIM_ALPHA,
            callout_tint: None,
            entered: false,
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
        self.current = 0;
        self.active = true;
        self.entered = false;
    }

    /// Stop the tutorial and hide the overlay.
    pub fn stop(&mut self) {
        self.active = false;
    }

    /// Run the current step's `on_enter` side effect once per entry. Call this at
    /// the START of the frame, before building the UI, so a step that opens a tab
    /// takes effect the same frame its highlight target is drawn. Passing the
    /// app's mutable state lets the tutorial change UI state without the widgets
    /// knowing about it. No-op while inactive or once the step has been entered.
    pub fn sync(&mut self, app: &mut C) {
        if !self.active || self.entered {
            return;
        }
        self.entered = true;
        if let Some(step) = self.steps.get_mut(self.current)
            && let Some(action) = step.on_enter.as_mut()
        {
            action(app);
        }
    }

    /// Apply a callout action, ending the tutorial when it runs off either end.
    /// Any step change re-arms `on_enter` so it runs again on the next `sync`.
    fn apply(&mut self, action: CalloutAction) {
        match action {
            CalloutAction::Next => {
                if self.current + 1 >= self.steps.len() {
                    self.active = false;
                } else {
                    self.current += 1;
                    self.entered = false;
                }
            }
            CalloutAction::Prev => {
                let prev = self.current;
                self.current = self.current.saturating_sub(1);
                if self.current != prev {
                    self.entered = false;
                }
            }
            CalloutAction::Stop => {
                self.active = false;
            }
        }
    }

    /// Draw the full overlay for the current step and advance on button clicks.
    /// No-op when inactive or when the current step index is out of range.
    pub fn render(&mut self, ctx: &Context, registry: &TutorialRegistry) {
        if !self.active {
            return;
        }
        let Some(step) = self.steps.get(self.current) else {
            self.active = false;
            return;
        };
        // Copy the small display data (not the `on_enter` closure) so `self` can
        // be mutated after (button clicks) without holding a borrow across render.
        let targets = step.targets.clone();
        let title = step.title.clone();
        let body = step.body.clone();

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
        let is_first = self.current == 0;
        let is_last = self.current + 1 >= self.steps.len();
        let (callout_rect, action) = Self::show_callout(
            ctx,
            screen,
            callout_pos,
            &title,
            &body,
            self.current + 1,
            self.steps.len(),
            is_first,
            is_last,
            self.callout_tint,
        );
        self.last_callout_size = callout_rect.size();

        // 4. Decoration on top of everything: dashed outline + the fixed arrow.
        if let (Some(hole), Some(placement)) = (hole, placement) {
            Self::paint_decoration(ctx, hole, placement.tail, placement.tip);
        }

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
        step_number: usize,
        step_total: usize,
        is_first: bool,
        is_last: bool,
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
                        ui.weak(format!("{step_number} / {step_total}"));
                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let next_label = if is_last { "Готово" } else { "Далее" };
                                if ui.button(next_label).clicked() {
                                    action = Some(CalloutAction::Next);
                                }
                                if !is_first && ui.button("Назад").clicked() {
                                    action = Some(CalloutAction::Prev);
                                }
                                if ui.button("Пропустить").clicked() {
                                    action = Some(CalloutAction::Stop);
                                }
                            });
                        });
                        action
                    })
                    .inner
            });

        (inner.response.rect, inner.inner)
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
