/*
File: crates/ms-input/src/frame.rs

Purpose:
The per-frame RESULT of the input router (`InputFrame`) and the view-surface publication
channel through which viewers tell the router where they are.

Key structures:
- `FrameEnv`: what the studio tells the router each frame (active tab, published contexts,
  modal prompt open).
- `ViewId`, `ViewSurface`: a viewer's identity and its last-drawn interactive rect.
- `InputFrame`: fired commands, active modes, axis samples, claimed canvas-wide gestures, the
  "armed" views and the press snapshots `gesture_for` resolves targeted gestures with.
- `Fired`, `AxisSample`, `ViewFocus`, `ClaimedGesture`, `GesturePhase`, `GestureHit`,
  `HitPhase`: the typed outputs.

Key functions:
- `publish_view_surface`: called by a viewer while it draws; read by the router at the start of
  the NEXT frame (one-frame lag, like the touch router's surfaces).
- `InputFrame::gesture_for`: resolves a targeted gesture from an egui `Response` with the chord
  held when the pointer button was PRESSED.

Notes:
- `InputFrame` is a snapshot: it holds an `Arc` of the registry it was computed with, so a
  binding change made during the frame (settings pane) takes effect from the next frame.
- Surfaces live in `egui::Context` temp data, replaced per `ViewId`, so the list is bounded by
  the number of distinct views even when no router drains it (e.g. a context without a studio).
*/

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use egui::{Context, Id, LayerId, PointerButton, Pos2, Rect, Response};
use ms_config::app_tab::AppTab;

use crate::action::{ActionId, ReleasePolicy, TargetId};
use crate::binding::GestureKind;
use crate::button::{Button, MouseButton};
use crate::context::{ActiveSet, ContextId};
use crate::matching::MatchState;
use crate::registry::InputRegistry;

/// What the studio tells the router about the current frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameEnv {
    /// The tab whose actions may be active (`None`: only tab-independent actions).
    pub active_tab: Option<AppTab>,
    /// Every context published this frame by the active tab, its tools and windows.
    pub contexts: Vec<ContextId>,
    /// A modal prompt is open: nothing fires, no mode is active, nothing is claimed (the matcher
    /// still tracks held buttons so nothing is stuck when the prompt closes).
    pub modal_open: bool,
}

/// Identity of one viewer (a canvas, the advanced-recognition view, ...). Content-named and stable
/// across `StudioApp` rebuilds in one `egui::Context`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ViewId(pub Id);

/// Where one viewer was interactive in the frame it was drawn in, in screen points of the root
/// viewport.
///
/// Overlapping surfaces: of those containing the pointer and not occluded, the one whose
/// `capture_layer` is the top layer there wins, otherwise the one published LATER in the frame.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewSurface {
    pub id: ViewId,
    /// The viewer's visible viewport.
    pub rect: Rect,
    /// Sub-rects of `rect` that do NOT count as the view (aside cards, the bottom hint, the
    /// Hangul keyboard, scrollbars): the pointer there is not "on the view".
    pub holes: Vec<Rect>,
    /// The layer of the viewer's own capture `Area` (raised while a canvas-wide gesture is armed),
    /// which therefore does not occlude the view. Any other non-background layer on top does.
    pub capture_layer: Option<LayerId>,
    /// The viewer that receives focus-less commands (zoom keys with the pointer elsewhere), with
    /// its centre as the anchor. One per frame; the first published wins.
    pub primary: bool,
    /// The viewer's touch-router owner id (`crate::touch::TouchSurface::owner`), when it takes
    /// finger input: touch binding alternatives are matched from that owner's deliveries.
    pub touch_owner: Option<Id>,
}

impl ViewSurface {
    /// Whether `pos` lies inside the surface and outside every hole.
    #[must_use]
    pub fn contains(&self, pos: Pos2) -> bool {
        self.rect.contains(pos) && !self.holes.iter().any(|hole| hole.contains(pos))
    }
}

fn surfaces_id() -> Id {
    Id::new("ms_input.view_surfaces")
}

/// Publishes `surface` for the frame being drawn; the router reads it at the start of the next
/// frame. Replaces an earlier publication with the same `ViewId` (repeated passes are
/// idempotent). Call it every frame the viewer is drawn: a frame without a publication means "no
/// such view" next frame.
pub fn publish_view_surface(ctx: &Context, surface: ViewSurface) {
    ctx.data_mut(|data| {
        let surfaces = data.get_temp_mut_or_default::<Vec<ViewSurface>>(surfaces_id());
        if let Some(slot) = surfaces.iter_mut().find(|slot| slot.id == surface.id) {
            *slot = surface;
        } else {
            surfaces.push(surface);
        }
    });
}

/// Takes every surface published since the previous call (the router's freeze step).
pub(crate) fn take_view_surfaces(ctx: &Context) -> Vec<ViewSurface> {
    ctx.data_mut(|data| data.remove_temp::<Vec<ViewSurface>>(surfaces_id()).unwrap_or_default())
}

/// The view an input acts on and the anchor point inside it (screen points): the pointer
/// position over the view under the pointer, else the centre of the primary view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewFocus {
    pub view: ViewId,
    pub pos: Pos2,
}

/// A command fired this frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Fired {
    pub action: ActionId,
    /// The accepted parameter modes active at the completing press (declaration order).
    pub params: Vec<ActionId>,
    pub focus: Option<ViewFocus>,
}

/// One frame of an axis.
///
/// Wheel sources: `delta` is in NOTCHES (a `Line` / `Page` event counts its value, a `Point`
/// event its value / 40), sign as egui's wheel delta (positive = wheel turned away from the
/// user / content moves down), times the binding's sensitivity, negated when inverted; `steps`
/// are whole notches (±1 per `Line` / `Page` event, a 40-point accumulator for `Point` events
/// that survives between frames), inverted but not scaled. Pinch: `delta` is the natural log of
/// the frame's zoom factor (apply `exp(delta)`), `steps` is 0. Drag sources: `delta` is the
/// pointer movement in points on that axis, `steps` is 0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisSample {
    pub action: ActionId,
    pub delta: f32,
    pub steps: i32,
    pub focus: Option<ViewFocus>,
}

/// Where a claimed canvas-wide gesture stands this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GesturePhase {
    /// The press happened this frame.
    Started,
    /// Still held (reported every frame).
    Active,
    /// Ended this frame: `Commit` on the button release (or a mode release whose policy commits),
    /// `Cancel` on a mode release whose policy cancels, focus loss or a modal prompt.
    Finished(ReleasePolicy),
}

/// A canvas-wide gesture the router claimed at its press (P2): a `Drag` binding on a
/// `canvas_wide` target pressed inside a view surface. The viewer drives it from here instead of
/// from widget responses.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimedGesture {
    pub action: ActionId,
    pub target: TargetId,
    pub view: ViewId,
    pub phase: GesturePhase,
    /// The press point (screen points).
    pub origin: Pos2,
    /// The latest pointer (or finger) position.
    pub current: Pos2,
    /// Accepted parameter modes as sampled per their `Sample` rule.
    pub params: Vec<ActionId>,
    /// The mouse button or touch source that drives it.
    pub button: Button,
}

/// The phase of a targeted gesture, read from an egui `Response`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitPhase {
    Clicked,
    DoubleClicked,
    DragStarted,
    Dragging,
    DragStopped,
}

/// A targeted gesture resolved by [`InputFrame::gesture_for`].
#[derive(Debug, Clone, PartialEq)]
pub struct GestureHit {
    pub action: ActionId,
    pub target: TargetId,
    pub phase: HitPhase,
    pub params: Vec<ActionId>,
    pub button: MouseButton,
}

/// The matcher state and activity at a pointer-button press, kept until that button's release
/// frame has passed, so a gesture resolves with the chord of its PRESS.
#[derive(Debug, Clone)]
pub(crate) struct PressSnapshot {
    pub(crate) state: MatchState,
    pub(crate) env: ActiveSet,
}

/// Everything the router decided for one frame. Read-only for consumers.
#[derive(Debug, Clone)]
pub struct InputFrame {
    pub(crate) registry: Arc<InputRegistry>,
    /// Modal prompt open or binding capture running: nothing fires and nothing resolves.
    pub(crate) suppressed: bool,
    pub(crate) active: ActiveSet,
    pub(crate) state: MatchState,
    pub(crate) fired: Vec<Fired>,
    pub(crate) modes: BTreeSet<ActionId>,
    pub(crate) axes: Vec<AxisSample>,
    pub(crate) claims: Vec<ClaimedGesture>,
    pub(crate) armed: Vec<ViewId>,
    pub(crate) touch_selectable: Vec<ViewId>,
    pub(crate) presses: BTreeMap<MouseButton, PressSnapshot>,
    pub(crate) view_under_pointer: Option<ViewId>,
}

/// The egui pointer buttons in the order `gesture_for` tries them.
const POINTER_BUTTONS: [PointerButton; 5] = [PointerButton::Primary, PointerButton::Secondary, PointerButton::Middle, PointerButton::Extra1, PointerButton::Extra2];

/// The binding button of an egui pointer button.
#[must_use]
pub(crate) fn mouse_button(button: PointerButton) -> MouseButton {
    match button {
        PointerButton::Primary => MouseButton::Left,
        PointerButton::Secondary => MouseButton::Right,
        PointerButton::Middle => MouseButton::Middle,
        PointerButton::Extra1 => MouseButton::Extra(1),
        PointerButton::Extra2 => MouseButton::Extra(2),
    }
}

impl InputFrame {
    /// An empty frame over `registry` (before the first `begin_frame`).
    pub(crate) fn empty(registry: Arc<InputRegistry>) -> Self {
        Self {
            registry,
            suppressed: false,
            active: ActiveSet::default(),
            state: MatchState::default(),
            fired: Vec::new(),
            modes: BTreeSet::new(),
            axes: Vec::new(),
            claims: Vec::new(),
            armed: Vec::new(),
            touch_selectable: Vec::new(),
            presses: BTreeMap::new(),
            view_under_pointer: None,
        }
    }

    /// The firing of command `id` this frame, if it fired.
    #[must_use]
    pub fn fired(&self, id: ActionId) -> Option<&Fired> {
        self.fired.iter().find(|fired| fired.action == id)
    }

    /// Every command fired this frame, in event order (one press fires at most one command).
    #[must_use]
    pub fn fired_commands(&self) -> &[Fired] {
        &self.fired
    }

    /// Whether mode `id` is active this frame.
    #[must_use]
    pub fn mode_active(&self, id: ActionId) -> bool {
        self.modes.contains(&id)
    }

    /// The accumulated sample of axis `id` this frame, if its source moved under its chord.
    #[must_use]
    pub fn axis(&self, id: ActionId) -> Option<&AxisSample> {
        self.axes.iter().find(|sample| sample.action == id)
    }

    /// The sample of axis `id` when its focus is `view`.
    #[must_use]
    pub fn axis_for_view(&self, id: ActionId, view: ViewId) -> Option<&AxisSample> {
        self.axis(id).filter(|sample| sample.focus.is_some_and(|focus| focus.view == view))
    }

    /// The canvas-wide gesture claimed on `view` (also in its `Finished` frame).
    #[must_use]
    pub fn claimed(&self, view: ViewId) -> Option<&ClaimedGesture> {
        self.claims.iter().find(|claim| claim.view == view)
    }

    /// Whether `view` must raise its full-surface capture `Area` this frame: the pointer is on it
    /// and a canvas-wide `Drag` gesture's chord holds (a press would be claimed), or a claimed
    /// mouse gesture on it is still running.
    #[must_use]
    pub fn canvas_wide_armed(&self, view: ViewId) -> bool {
        self.armed.contains(&view)
    }

    /// Whether a finger's long-press drag on `view` would drive a canvas-wide gesture now (feeds
    /// `TouchSurface::long_press_selection` of the view's touch surface).
    #[must_use]
    pub fn touch_selection_available(&self, view: ViewId) -> bool {
        self.touch_selectable.contains(&view)
    }

    /// The view under the pointer (last frame's surfaces, holes and occluding layers applied).
    #[must_use]
    pub fn view_under_pointer(&self) -> Option<ViewId> {
        self.view_under_pointer
    }

    /// Whether input is suppressed this frame (modal prompt or binding capture).
    #[must_use]
    pub fn suppressed(&self) -> bool {
        self.suppressed
    }

    /// Resolves the targeted gesture `response` reports this frame.
    ///
    /// `candidates` lists the gesture targets of the widget behind `response` with whether the
    /// pointer hits each one; only hit targets compete. The pointer button, phase and kind come
    /// from `response` (drag start / drag / drag stop, then double click, then click); the
    /// matching chord is the one held when that button was PRESSED (accepted `AtPress` modes
    /// likewise), `LatestDuringGesture` parameters are sampled now. The highest-ranked gesture
    /// wins (explained buttons, target priority, innermost scope). `None` while suppressed or when
    /// no gesture is bound to what happened.
    #[must_use]
    pub fn gesture_for(&self, response: &Response, candidates: &[(TargetId, bool)]) -> Option<GestureHit> {
        if self.suppressed {
            return None;
        }
        let targets: Vec<TargetId> = candidates.iter().filter(|(_, hit)| *hit).map(|(target, _)| *target).collect();
        if targets.is_empty() {
            return None;
        }
        for pointer in POINTER_BUTTONS {
            let button = mouse_button(pointer);
            let phases = [
                (response.drag_started_by(pointer), HitPhase::DragStarted, GestureKind::Drag),
                (response.dragged_by(pointer), HitPhase::Dragging, GestureKind::Drag),
                (response.drag_stopped_by(pointer), HitPhase::DragStopped, GestureKind::Drag),
                (response.double_clicked_by(pointer), HitPhase::DoubleClicked, GestureKind::DoubleClick),
                (response.clicked_by(pointer), HitPhase::Clicked, GestureKind::Click),
            ];
            for (happened, phase, kind) in phases {
                if !happened {
                    continue;
                }
                // Without a snapshot (the press predates the router) the current chord is the
                // best available evidence.
                let (state, env) = self.presses.get(&button).map_or((&self.state, &self.active), |snapshot| (&snapshot.state, &snapshot.env));
                if let Some(matched) = state.match_gesture(&self.registry, &Button::Mouse(button), kind, &targets, env) {
                    let params = self.state.gesture_params(&self.registry, &matched, &self.active);
                    return Some(GestureHit { action: matched.action, target: matched.target, phase, params, button });
                }
            }
        }
        None
    }
}
