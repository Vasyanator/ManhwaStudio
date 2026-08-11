/*
File: src/widgets/panel_dock/solver.rs

Purpose:
Pure layout solver of the dockable-panel system: turns the anchor graph of a
`DockLayout` into absolute rects inside one host area, preserving the docking
gap, keeping every chain inside the area, and charging the overflow to the
panels whose size actually causes it.

Main responsibilities:
- resolve anchors into rects (panel-to-panel, host-area edges, canvas controls);
- keep exactly `DOCK_GAP` between a panel and whatever it is attached to;
- translate a chain that hangs out of the area back inside it;
- shrink, on BOTH axes, only the panels whose size actually pushes a member of
  the chain out of the area, never below their floor, taking from the panels the
  user sized by hand LAST, and report the vertical result as `body_max_height`.

Key structures:
- `PanelSizes`: sanitized `PanelId -> Vec2` input map.
- `PanelChrome`: measured, style-dependent header/frame overhead of a panel.
- `SolvedPanel`, `SolvedLayout`: the solver's output.

Key functions:
- `solve`: the whole contract; deterministic, side-effect free, panic free.
- `relieve_overflow`: one shrink pass on one axis, driven by the exact
  derivative of each overflowing panel's edge with respect to every size.

Notes:
This file must stay a pure function of its inputs: no egui context, no memory,
no logging, no interior mutability. Everything it needs about the previous frame
(measured content sizes, manual resizes) arrives through `desired` / `mins` and
`PanelNode::size_override`.
*/

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use egui::{Pos2, Rect, Vec2};

use super::model::{DockEdge, DockLayout, HostId, PanelAnchor, PanelId, PanelNode};

/// Distance, in points, kept between a panel and the thing it is docked to.
///
/// The same constant is used for panel-to-panel gaps, the inset from a host
/// area edge, and the distance from the canvas-controls rect.
pub const DOCK_GAP: f32 = 8.0;

/// NOMINAL height, in points, of a panel's header strip.
///
/// It is only the value used before the widget has measured a real panel once:
/// the true height depends on the active style (button padding, frame margins),
/// and the drawn value arrives every frame as [`PanelChrome`]. Never assume this
/// constant describes what is on screen — a chain laid out with a header estimate
/// that is 12 pt too small overlaps its own panels.
pub const COLLAPSED_PANEL_HEIGHT: f32 = 24.0;

/// Default lower bound, in points, for the *body* of an expanded panel — the
/// floor the even-shrinking step must not push a panel below. Matches the
/// existing typing-panel section minimum.
pub const PANEL_MIN_CONTENT_HEIGHT: f32 = 120.0;

/// Smallest outer WIDTH, in points, a panel is ever laid out at.
///
/// Unlike a declared `min_size` this is not a preference but a physical bound:
/// `CollapsiblePanel` cannot draw a narrower frame, so a solved rect below it
/// would be narrower than the panel on screen and the neighbour placed one
/// [`DOCK_GAP`] away would overlap it.
pub const PANEL_MIN_WIDTH: f32 = 40.0;

/// Smallest body HEIGHT, in points, a drawn expanded panel keeps.
///
/// Same nature as [`PANEL_MIN_WIDTH`]: the widget always leaves this much room
/// under the header, so the solver must not hand out a shorter rect.
pub const PANEL_MIN_BODY_HEIGHT: f32 = 24.0;

/// Outer size, in points, used for a panel that has neither a measurement in
/// `desired` nor a `size_override` (a panel drawn for the very first time).
pub const DEFAULT_PANEL_SIZE: Vec2 =
    Vec2::new(320.0, COLLAPSED_PANEL_HEIGHT + PANEL_MIN_CONTENT_HEIGHT);

/// Sizes below this difference, in points, are treated as equal. Keeps the
/// shrink loop from iterating on floating-point dust.
const FIT_EPSILON: f32 = 0.01;

/// Upper bound on shrink iterations per chain and axis.
///
/// [`relieve_overflow`] assigns each overflowing panel exactly the reduction it
/// needs, so one iteration settles every layout whose set of overflowing panels
/// and whose leading member do not change when the sizes shrink. The loop exists
/// for the cases where they do (a panel hanging above the chain's root shrinks
/// past another one), and stops as soon as an iteration takes nothing, so this
/// bound is a guard against pathological inputs rather than a convergence knob.
const MAX_SHRINK_ITERATIONS: usize = 8;

/// How readily a panel gives up size when its chain does not fit the host area.
///
/// The distinction is a contract, not a heuristic: a content-driven size is a
/// REQUEST the widget derived from what it happens to draw, while a
/// `PanelNode::size_override` is the size a user dragged the panel to. Charging
/// both alike made growing a panel inside a chain that already fills the area
/// visibly impossible — the water-filling gave the whole deficit back to the
/// panel with the most slack, which is precisely the one that had just grown.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ShrinkPriority {
    /// The panel is laid out at the size its content measured. It yields first.
    ContentSized,
    /// The panel's size was set by hand. It yields only once every
    /// content-sized panel that could relieve the same overflow sits on its
    /// floor.
    ManuallySized,
}

/// The shrink tiers, in the order the deficit drains them.
///
/// Draining is strictly sequential: a tier is only asked for what the tiers
/// before it could not give. Floors still bound every tier, so the last one can
/// legitimately fail to absorb the rest — the chain is then translated and
/// reported as `shrunk`, exactly as when a single tier ran out of slack.
const SHRINK_PRIORITIES: [ShrinkPriority; 2] =
    [ShrinkPriority::ContentSized, ShrinkPriority::ManuallySized];

/// Tier `node` belongs to: manual whenever the user pinned a size on it.
fn shrink_priority(node: &PanelNode) -> ShrinkPriority {
    if node.size_override.is_some() {
        ShrinkPriority::ManuallySized
    } else {
        ShrinkPriority::ContentSized
    }
}

/// One of the two independent axes of the layout arithmetic.
///
/// Placement is separable: an edge displaces a panel along exactly one axis and
/// aligns it along the other, and neither expression mixes widths into
/// vertical positions. The shrink step therefore runs once per axis on the same
/// machinery instead of duplicating it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Axis {
    /// Horizontal: `start` is `left`, `length` is `width`.
    X,
    /// Vertical: `start` is `top`, `length` is `height`.
    Y,
}

/// What a [`DockEdge`] means for one axis.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum EdgeRole {
    /// The panel is placed BEFORE its target on this axis (`Top` for `Y`,
    /// `Left` for `X`), so its own size moves its start.
    Leading,
    /// The panel is placed AFTER its target on this axis (`Bottom` for `Y`,
    /// `Right` for `X`), so the target's size moves its start.
    Trailing,
    /// The edge runs along this axis, so the panel is only *aligned* on it and
    /// both sizes move its start by the alignment fraction.
    Perpendicular,
}

impl Axis {
    /// Leading coordinate of `rect` on this axis.
    fn start(self, rect: Rect) -> f32 {
        match self {
            Self::X => rect.left(),
            Self::Y => rect.top(),
        }
    }

    /// Trailing coordinate of `rect` on this axis.
    fn end(self, rect: Rect) -> f32 {
        match self {
            Self::X => rect.right(),
            Self::Y => rect.bottom(),
        }
    }

    /// Component of `size` on this axis.
    fn length(self, size: Vec2) -> f32 {
        match self {
            Self::X => size.x,
            Self::Y => size.y,
        }
    }

    /// Subtracts `by` from `size` on this axis.
    fn shrink(self, size: &mut Vec2, by: f32) {
        match self {
            Self::X => size.x -= by,
            Self::Y => size.y -= by,
        }
    }

    /// Role `edge` plays for this axis.
    fn role(self, edge: DockEdge) -> EdgeRole {
        match (self, edge) {
            (Self::Y, DockEdge::Top) | (Self::X, DockEdge::Left) => EdgeRole::Leading,
            (Self::Y, DockEdge::Bottom) | (Self::X, DockEdge::Right) => EdgeRole::Trailing,
            (Self::Y, DockEdge::Left | DockEdge::Right)
            | (Self::X, DockEdge::Top | DockEdge::Bottom) => EdgeRole::Perpendicular,
        }
    }
}

/// The style-dependent vertical overhead of a drawn panel, measured by
/// `CollapsiblePanel` and fed back into the next frame's solve.
///
/// Both values are OUTER heights in points and are identical for every panel of
/// one frame (same widget, same style), which is why the driver keeps a single
/// instance rather than a per-panel map. Before the first panel has ever been
/// drawn they fall back to the nominal [`COLLAPSED_PANEL_HEIGHT`].
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct PanelChrome {
    /// Outer height of a COLLAPSED panel: the frame margins plus the header
    /// strip. A collapsed panel is exactly this tall and never shrinks.
    pub collapsed_height: f32,
    /// Outer height an EXPANDED panel spends on everything that is not body:
    /// the frame margins, the header strip and the gap under it. Subtracted from
    /// the panel's height to obtain [`SolvedPanel::body_max_height`].
    pub body_overhead: f32,
}

impl Default for PanelChrome {
    fn default() -> Self {
        Self {
            collapsed_height: COLLAPSED_PANEL_HEIGHT,
            body_overhead: COLLAPSED_PANEL_HEIGHT,
        }
    }
}

impl PanelChrome {
    /// Builds a measurement, replacing a non-finite or negative component with
    /// the nominal [`COLLAPSED_PANEL_HEIGHT`] so a broken measurement degrades
    /// into the old estimate instead of poisoning the layout.
    #[must_use]
    pub fn new(collapsed_height: f32, body_overhead: f32) -> Self {
        Self {
            collapsed_height: sanitize_chrome(collapsed_height),
            body_overhead: sanitize_chrome(body_overhead),
        }
    }

    /// Largest difference, in points, between this measurement and `other`.
    /// The driver uses it to decide whether the layout still has to converge.
    #[must_use]
    pub fn max_difference(self, other: Self) -> f32 {
        (self.collapsed_height - other.collapsed_height)
            .abs()
            .max((self.body_overhead - other.body_overhead).abs())
    }
}

/// Replaces a non-finite or negative chrome height with the nominal one.
fn sanitize_chrome(value: f32) -> f32 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        COLLAPSED_PANEL_HEIGHT
    }
}

/// A `PanelId -> Vec2` map whose values are sanitized on insertion.
///
/// Sanitizing means: a non-finite component (`NaN`, `±inf`) or a negative
/// component becomes `0.0`. The solver therefore never has to reason about
/// `NaN` sizes, and a broken measurement degrades into "no preference" instead
/// of poisoning the whole chain's arithmetic.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelSizes {
    sizes: BTreeMap<PanelId, Vec2>,
}

impl PanelSizes {
    /// Creates an empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores a sanitized size, returning the previous one.
    pub fn insert(&mut self, id: PanelId, size: Vec2) -> Option<Vec2> {
        self.sizes.insert(id, sanitize_size(size))
    }

    /// Returns the sanitized size stored for `id`.
    #[must_use]
    pub fn get(&self, id: PanelId) -> Option<Vec2> {
        self.sizes.get(&id).copied()
    }

    /// Drops the entry for `id`, returning it.
    pub fn remove(&mut self, id: PanelId) -> Option<Vec2> {
        self.sizes.remove(&id)
    }

    /// Number of stored entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sizes.len()
    }

    /// `true` when no size is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sizes.is_empty()
    }
}

impl FromIterator<(PanelId, Vec2)> for PanelSizes {
    fn from_iter<I: IntoIterator<Item = (PanelId, Vec2)>>(iter: I) -> Self {
        let mut sizes = Self::new();
        for (id, size) in iter {
            sizes.insert(id, size);
        }
        sizes
    }
}

/// The resolved geometry of one panel.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SolvedPanel {
    /// Absolute outer rect of the panel, header included, in the same
    /// coordinate space as the `area` passed to [`solve`].
    pub rect: Rect,
    /// Height, in points, left for the active tab's body after the header. The
    /// body must scroll inside this height; it is never clipped silently.
    /// Always `0.0` for a collapsed panel.
    pub body_max_height: f32,
    /// `true` when the panel did not get what it asked for: its height was
    /// reduced below the requested one, or its rect does not fit inside the
    /// host area even after shrinking.
    pub shrunk: bool,
}

/// The solver's output: one [`SolvedPanel`] per panel of the solved host.
///
/// Iteration order is ascending [`PanelId`], which makes drawing order and
/// tests deterministic.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SolvedLayout {
    panels: BTreeMap<PanelId, SolvedPanel>,
}

impl SolvedLayout {
    /// Resolved geometry of one panel, or `None` when the panel is not part of
    /// the solved host.
    #[must_use]
    pub fn get(&self, id: PanelId) -> Option<SolvedPanel> {
        self.panels.get(&id).copied()
    }

    /// All resolved panels in ascending id order.
    pub fn iter(&self) -> impl Iterator<Item = (PanelId, SolvedPanel)> + '_ {
        self.panels.iter().map(|(id, panel)| (*id, *panel))
    }

    /// Number of resolved panels.
    #[must_use]
    pub fn len(&self) -> usize {
        self.panels.len()
    }

    /// `true` when the solved host has no panels.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.panels.is_empty()
    }
}

/// Resolves the anchors of every panel of `host` into absolute rects inside
/// `area`.
///
/// Rules, applied in this order (see `dev-docs/dockable_panels_plan.md` §4.5):
/// 1. **Chains.** Panels connected by `PanelAnchor::Panel` form one chain and
///    are laid out together.
/// 2. **Gap.** An attached panel sits exactly [`DOCK_GAP`] away from the side
///    it is attached to.
/// 3. **Propagation.** A panel's rect is derived from its anchor's resolved
///    rect, so resizing a target moves its dependants for free.
/// 4. **Shrinking.** A panel that sticks out of `area` is brought back by
///    reducing the sizes that actually place its far edge — itself and the
///    ancestors that push it — water-filled over those, floored per panel, on
///    both axes. Panels the overflow does not depend on keep their size: a
///    neighbour standing beside the offender must not collapse with it. The
///    order is tiered ([`SHRINK_PRIORITIES`]): a panel carrying a
///    `PanelNode::size_override` is the LAST to give, because that size is the
///    user's intent while a content-driven one is only a request.
/// 5. **Clamping.** What shrinking could not resolve is absorbed by translating
///    the chain as a whole, so its internal gaps stay exact.
/// 6. **Delivery.** What is left for the tab body is reported as
///    [`SolvedPanel::body_max_height`].
///
/// Inputs:
/// * `desired` — the outer size each panel would like (measured content plus
///   header). A `PanelNode::size_override` takes precedence over it, and
///   [`DEFAULT_PANEL_SIZE`] is used when neither is present. It is raised only
///   to the size the widget can physically draw ([`PANEL_MIN_WIDTH`], one
///   measured header plus [`PANEL_MIN_BODY_HEIGHT`]) — never to `mins`, which
///   would reserve space a content-sized panel does not draw and leave a hole
///   above the panel below it.
/// * `mins` — the outer size below which a panel must not be *shrunk*. Missing
///   entries default to `(0.0, chrome.collapsed_height + PANEL_MIN_CONTENT_HEIGHT)`,
///   and every minimum is raised to what the widget can physically draw.
/// * `canvas_controls` — rect of the `CanvasView` controls panel, in the same
///   space as `area`. It is an *anchor only*: nothing is ever pushed away from
///   it. When it is `None`, a `PanelAnchor::CanvasControls` resolves as
///   `PanelAnchor::Free` at the panel's own `pos`, because the anchor target
///   does not exist in this host.
/// * `chrome` — the header/frame overhead the widget MEASURED on the previous
///   frame. Passing [`PanelChrome::default`] reproduces the nominal estimate,
///   which is only correct before a panel has ever been drawn.
///
/// Guarantees: deterministic (equal inputs give equal output), idempotent
/// (re-solving a layout whose sizes are the solved ones reproduces the same
/// rects), and panic-free for every input — a degenerate `area`, non-finite
/// sizes and non-finite positions are sanitized to finite values rather than
/// propagated. When a chain cannot fit even with every panel at its floor, the
/// best-fitting placement is returned with [`SolvedPanel::shrunk`] set.
#[must_use]
pub fn solve(
    layout: &DockLayout,
    host: HostId,
    area: Rect,
    desired: &PanelSizes,
    mins: &PanelSizes,
    canvas_controls: Option<Rect>,
    chrome: PanelChrome,
) -> SolvedLayout {
    let area = sanitize_rect(area);
    let controls = canvas_controls.map(sanitize_rect);

    let nodes: BTreeMap<PanelId, &PanelNode> = layout
        .panels_in_host(host)
        .map(|node| (node.id, node))
        .collect();
    if nodes.is_empty() {
        return SolvedLayout::default();
    }

    // Requested and floor sizes, resolved once: the shrink loop only lowers
    // heights from `requested` towards `floor`.
    let mut requested: BTreeMap<PanelId, Vec2> = BTreeMap::new();
    let mut floor: BTreeMap<PanelId, Vec2> = BTreeMap::new();
    for (id, node) in &nodes {
        floor.insert(*id, panel_floor(node, mins, chrome));
        requested.insert(*id, panel_requested(node, desired, chrome));
    }

    let mut sizes = requested.clone();
    let mut solved = SolvedLayout::default();

    for chain in layout.chains() {
        // `chains` covers every host; keep the members that belong to this one.
        let members: Vec<PanelId> = chain
            .into_iter()
            .filter(|id| nodes.contains_key(id))
            .collect();
        if members.is_empty() {
            continue;
        }

        let chain = ChainContext {
            members: &members,
            nodes: &nodes,
            floor: &floor,
            area,
            controls,
        };
        let mut rects = propagate(&members, &nodes, &sizes, area, controls);
        // The axes are independent (see `Axis`), so each one is relieved on its
        // own; a chain too wide is shrunk exactly like a chain too tall, which
        // is what keeps every panel's resize grip inside the area.
        for axis in [Axis::Y, Axis::X] {
            for _ in 0..MAX_SHRINK_ITERATIONS {
                let taken = chain.relieve_overflow(axis, &mut sizes, &rects);
                if taken <= FIT_EPSILON {
                    // Either nothing sticks out any more, or everything that
                    // could give is already at its floor.
                    break;
                }
                rects = propagate(&members, &nodes, &sizes, area, controls);
            }
        }

        // Clamping is last: the chain keeps its internal geometry and is moved
        // as a whole, so gaps stay exact even when the chain overflows.
        if let Some(bounds) = bounding_rect(&members, &rects) {
            let shift = fitting_shift(bounds, area);
            if shift != Vec2::ZERO {
                for rect in rects.values_mut() {
                    *rect = rect.translate(shift);
                }
            }
        }

        for id in members {
            let Some(rect) = rects.get(&id).copied() else {
                continue;
            };
            let Some(node) = nodes.get(&id) else {
                continue;
            };
            let asked = requested.get(&id).copied().unwrap_or(rect.size());
            let shrunk = asked.y - rect.height() > FIT_EPSILON
                || asked.x - rect.width() > FIT_EPSILON
                || !contains_rect(area, rect);
            let body_max_height = if node.collapsed {
                0.0
            } else {
                (rect.height() - chrome.body_overhead).max(0.0)
            };
            solved.panels.insert(
                id,
                SolvedPanel {
                    rect,
                    body_max_height,
                    shrunk,
                },
            );
        }
    }

    solved
}

/// Smallest outer size `CollapsiblePanel` can physically draw for `node`.
///
/// This is a bound on the WIDGET, not a preference of the caller: a solved rect
/// below it would be smaller than the panel actually on screen, and everything
/// placed one [`DOCK_GAP`] away from that rect would overlap the panel.
fn panel_hard_floor(node: &PanelNode, chrome: PanelChrome) -> Vec2 {
    let height = if node.collapsed {
        chrome.collapsed_height
    } else {
        chrome.body_overhead + PANEL_MIN_BODY_HEIGHT
    };
    Vec2::new(PANEL_MIN_WIDTH, height)
}

/// Size below which the shrink step must not push a panel: `mins` when present,
/// the documented default otherwise, never below [`panel_hard_floor`].
fn panel_floor(node: &PanelNode, mins: &PanelSizes, chrome: PanelChrome) -> Vec2 {
    let min = mins.get(node.id).unwrap_or(Vec2::new(
        0.0,
        chrome.collapsed_height + PANEL_MIN_CONTENT_HEIGHT,
    ));
    let hard = panel_hard_floor(node, chrome);
    Vec2::new(min.x.max(hard.x), min.y.max(hard.y))
}

/// Outer size a panel asks for: a manual resize wins over the measured size,
/// which wins over the default; a collapsed panel is always exactly one MEASURED
/// header tall.
///
/// The declared minimum deliberately does NOT participate: `min_size` means
/// "never shrink me below this", and using it to raise the REQUEST would make a
/// panel whose content is shorter reserve height it does not draw — the panel
/// below it would then hang a `min_size`-sized hole away from a panel that ends
/// much earlier. Only [`panel_hard_floor`] applies, because the widget cannot
/// draw anything smaller than that.
fn panel_requested(node: &PanelNode, desired: &PanelSizes, chrome: PanelChrome) -> Vec2 {
    let asked = node
        .size_override
        .map(sanitize_size)
        .or_else(|| desired.get(node.id))
        .unwrap_or(DEFAULT_PANEL_SIZE);
    let hard = panel_hard_floor(node, chrome);
    let width = asked.x.max(hard.x);
    if node.collapsed {
        Vec2::new(width, chrome.collapsed_height)
    } else {
        Vec2::new(width, asked.y.max(hard.y))
    }
}

/// Resolves the rects of one chain from its anchors, parents before children.
///
/// Panels whose anchor target is missing from the chain (a cross-host anchor, a
/// dangling target, or a member of a cycle that `validate` would have rejected)
/// are treated as free-floating at their own `pos`, so a corrupt layout still
/// produces finite geometry instead of a panic or an infinite loop.
fn propagate(
    members: &[PanelId],
    nodes: &BTreeMap<PanelId, &PanelNode>,
    sizes: &BTreeMap<PanelId, Vec2>,
    area: Rect,
    controls: Option<Rect>,
) -> BTreeMap<PanelId, Rect> {
    // `BTreeSet` children keep the visiting order ascending by id without an
    // extra sort, which is what makes `propagate` deterministic.
    let mut children: BTreeMap<PanelId, BTreeSet<PanelId>> = BTreeMap::new();
    let mut roots: Vec<PanelId> = Vec::new();
    for id in members {
        let Some(node) = nodes.get(id) else {
            continue;
        };
        match node.anchor.target_panel() {
            Some(target) if nodes.contains_key(&target) && members.contains(&target) => {
                children.entry(target).or_default().insert(*id);
            }
            Some(_) | None => roots.push(*id),
        }
    }
    roots.sort_unstable();

    let mut rects: BTreeMap<PanelId, Rect> = BTreeMap::new();
    let mut queue: VecDeque<PanelId> = VecDeque::new();
    for id in roots {
        let Some(node) = nodes.get(&id) else {
            continue;
        };
        let size = sizes.get(&id).copied().unwrap_or(DEFAULT_PANEL_SIZE);
        rects.insert(id, place_root(node, size, area, controls));
        queue.push_back(id);
    }

    while let Some(parent) = queue.pop_front() {
        let Some(parent_rect) = rects.get(&parent).copied() else {
            continue;
        };
        let Some(list) = children.get(&parent) else {
            continue;
        };
        for id in list.iter().copied() {
            if rects.contains_key(&id) {
                continue;
            }
            let Some(node) = nodes.get(&id) else {
                continue;
            };
            let size = sizes.get(&id).copied().unwrap_or(DEFAULT_PANEL_SIZE);
            let (edge, align) = match node.anchor {
                PanelAnchor::Panel { edge, align, .. } => (edge, align),
                PanelAnchor::Free
                | PanelAnchor::ViewportEdge { .. }
                | PanelAnchor::CanvasControls { .. } => (DockEdge::Bottom, 0.0),
            };
            rects.insert(id, place_outside(parent_rect, edge, align, size));
            queue.push_back(id);
        }
    }

    // Members left unresolved can only come from a cycle, which `validate`
    // rejects; place them free so the output stays total.
    for id in members {
        if rects.contains_key(id) {
            continue;
        }
        let Some(node) = nodes.get(id) else {
            continue;
        };
        let size = sizes.get(id).copied().unwrap_or(DEFAULT_PANEL_SIZE);
        rects.insert(*id, Rect::from_min_size(free_pos(node, area), size));
    }

    rects
}

/// Places a chain root from its own anchor.
fn place_root(node: &PanelNode, size: Vec2, area: Rect, controls: Option<Rect>) -> Rect {
    match node.anchor {
        PanelAnchor::ViewportEdge { edge, along } => place_inside(area, edge, along, size),
        PanelAnchor::CanvasControls { edge, along } => match controls {
            Some(controls) => place_outside(controls, edge, along, size),
            // Documented degradation: without the controls rect the anchor has
            // no target in this host, so the panel keeps its own position.
            None => Rect::from_min_size(free_pos(node, area), size),
        },
        // A `Panel` anchor reaching this branch means the target is outside the
        // chain; treat it like `Free` rather than inventing a target.
        PanelAnchor::Free | PanelAnchor::Panel { .. } => {
            Rect::from_min_size(free_pos(node, area), size)
        }
    }
}

/// Absolute top-left of a free panel: `pos` is stored relative to the host
/// area's origin, and sanitized here so a corrupt value cannot produce `NaN`.
fn free_pos(node: &PanelNode, area: Rect) -> Pos2 {
    let offset = sanitize_offset(node.pos.to_vec2());
    area.min + offset
}

/// Places `size` outside `target`, adjacent to `edge`, `DOCK_GAP` away, at
/// `align` (`0.0..=1.0`) along the shared side.
///
/// Shared with `drag.rs`, which needs the rect an anchor WOULD produce to decide
/// whether the slot is already taken (the sibling rule). A private second copy
/// there is exactly how the preview and the solve would start disagreeing.
pub(super) fn place_outside(target: Rect, edge: DockEdge, align: f32, size: Vec2) -> Rect {
    let align = sanitize_fraction(align);
    let min = match edge {
        DockEdge::Top => Pos2::new(
            target.left() + align * (target.width() - size.x),
            target.top() - DOCK_GAP - size.y,
        ),
        DockEdge::Bottom => Pos2::new(
            target.left() + align * (target.width() - size.x),
            target.bottom() + DOCK_GAP,
        ),
        DockEdge::Left => Pos2::new(
            target.left() - DOCK_GAP - size.x,
            target.top() + align * (target.height() - size.y),
        ),
        DockEdge::Right => Pos2::new(
            target.right() + DOCK_GAP,
            target.top() + align * (target.height() - size.y),
        ),
    };
    Rect::from_min_size(min, size)
}

/// Places `size` inside `container`, flush against `edge` with a `DOCK_GAP`
/// inset, at `along` (`0.0..=1.0`) on the perpendicular axis.
///
/// Shared with `drag.rs` for the same reason as [`place_outside`].
pub(super) fn place_inside(container: Rect, edge: DockEdge, along: f32, size: Vec2) -> Rect {
    let along = sanitize_fraction(along);
    let min = match edge {
        DockEdge::Top => Pos2::new(
            container.left() + along * (container.width() - size.x),
            container.top() + DOCK_GAP,
        ),
        DockEdge::Bottom => Pos2::new(
            container.left() + along * (container.width() - size.x),
            container.bottom() - DOCK_GAP - size.y,
        ),
        DockEdge::Left => Pos2::new(
            container.left() + DOCK_GAP,
            container.top() + along * (container.height() - size.y),
        ),
        DockEdge::Right => Pos2::new(
            container.right() - DOCK_GAP - size.x,
            container.top() + along * (container.height() - size.y),
        ),
    };
    Rect::from_min_size(min, size)
}

/// Union of the chain's rects, or `None` when nothing was resolved.
fn bounding_rect(members: &[PanelId], rects: &BTreeMap<PanelId, Rect>) -> Option<Rect> {
    let mut bounds: Option<Rect> = None;
    for id in members {
        let Some(rect) = rects.get(id).copied() else {
            continue;
        };
        bounds = Some(match bounds {
            Some(current) => current.union(rect),
            None => rect,
        });
    }
    bounds
}

/// Translation that brings `bounds` inside `area`.
///
/// When `bounds` is larger than `area` on an axis it is aligned to the area's
/// start on that axis, so the overflow is always on the far side and the
/// leading edge stays reachable.
fn fitting_shift(bounds: Rect, area: Rect) -> Vec2 {
    Vec2::new(
        axis_shift(bounds.left(), bounds.right(), area.left(), area.right()),
        axis_shift(bounds.top(), bounds.bottom(), area.top(), area.bottom()),
    )
}

/// One-axis part of [`fitting_shift`].
fn axis_shift(bounds_min: f32, bounds_max: f32, area_min: f32, area_max: f32) -> f32 {
    if bounds_max - bounds_min > area_max - area_min {
        return area_min - bounds_min;
    }
    if bounds_min < area_min {
        area_min - bounds_min
    } else if bounds_max > area_max {
        area_max - bounds_max
    } else {
        0.0
    }
}

/// Everything the shrink step needs about ONE chain that does not change while
/// it runs: who the members are, what they are anchored to, how far each may be
/// shrunk, and the geometry the chain has to fit into.
struct ChainContext<'a> {
    /// Panels of this chain, ascending by id.
    members: &'a [PanelId],
    /// Every solved panel of the host, by id (a chain's anchors never leave it).
    nodes: &'a BTreeMap<PanelId, &'a PanelNode>,
    /// Per-panel floor size; the shrink never crosses it.
    floor: &'a BTreeMap<PanelId, Vec2>,
    /// Host area the chain has to fit into.
    area: Rect,
    /// Canvas-controls rect, when this host has one.
    controls: Option<Rect>,
}

impl ChainContext<'_> {
    /// Shrinks, on one axis, the panels that actually place the chain's overflowing
    /// far edges, and writes the reduced sizes back into `sizes`.
    ///
    /// A chain is measured from the member that starts first, because the chain is
    /// translated as a whole afterwards: a member overflows when
    /// `end - chain_start > area length`. For each such member the exact derivative
    /// of `end - chain_start` with respect to every panel's size on this axis is
    /// known ([`start_coefficients`]), so only the panels that genuinely move that
    /// edge are asked to give up size — a panel standing BESIDE the offender has
    /// derivative `0` and keeps its size, and parallel branches are each relieved by
    /// their own full deficit instead of splitting one.
    ///
    /// Within one overflowing member the reduction is water-filled over the shrink
    /// tiers ([`SHRINK_PRIORITIES`]): the panels sized by their content are drained
    /// to their floors before a panel the user resized by hand gives a single point,
    /// and inside a tier panels are visited by ascending remaining slack, each asked
    /// for the same number of points, whatever a floored panel cannot give being
    /// re-divided among the rest. Members are processed by descending deficit and
    /// what the earlier ones already freed is credited to the later ones, so no
    /// panel is shrunk twice for the same overlap. Collapsed panels never give
    /// height (they are exactly one header tall) but do give width.
    ///
    /// Returns the total number of points taken; `0.0` means "nothing overflows any
    /// more, or everything that could give is already at its floor" and is the
    /// caller's signal to stop iterating.
    fn relieve_overflow(
        &self,
        axis: Axis,
        sizes: &mut BTreeMap<PanelId, Vec2>,
        rects: &BTreeMap<PanelId, Rect>,
    ) -> f32 {
        let budget = axis.length(self.area.size()).max(0.0);
        let Some((leader, chain_start)) = self.leading_member(axis, rects) else {
            return 0.0;
        };

        let mut overflowing: Vec<(f32, PanelId)> = self
            .members
            .iter()
            .filter_map(|id| {
                let rect = rects.get(id)?;
                let deficit = axis.end(*rect) - chain_start - budget;
                (deficit > FIT_EPSILON).then_some((deficit, *id))
            })
            .collect();
        if overflowing.is_empty() {
            return 0.0;
        }
        // Descending deficit, ties broken by id: the worst offender is relieved
        // first so the smaller ones can be credited with what it already freed, and
        // the order must be total and deterministic.
        overflowing.sort_by(|left, right| {
            right
                .0
                .partial_cmp(&left.0)
                .unwrap_or(Ordering::Equal)
                .then(left.1.cmp(&right.1))
        });

        let leader_coefficients = self.start_coefficients(leader, axis);
        let mut assigned: BTreeMap<PanelId, f32> = BTreeMap::new();
        for (deficit, id) in overflowing {
            let mut gradient = self.start_coefficients(id, axis);
            // `end = start + own size`, and the reference edge is subtracted because
            // raising it shortens the overflow just as much as lowering the far edge.
            *gradient.entry(id).or_insert(0.0) += 1.0;
            for (panel, coefficient) in &leader_coefficients {
                *gradient.entry(*panel).or_insert(0.0) -= coefficient;
            }
            let credited: f32 = assigned
                .iter()
                .map(|(panel, taken)| gradient.get(panel).copied().unwrap_or(0.0) * taken)
                .sum();
            let remaining = deficit - credited;
            if remaining <= FIT_EPSILON {
                continue;
            }
            self.water_fill(axis, remaining, &gradient, sizes, &mut assigned);
        }

        let mut taken_total = 0.0;
        for (id, take) in assigned {
            if take <= FIT_EPSILON {
                continue;
            }
            if let Some(size) = sizes.get_mut(&id) {
                axis.shrink(size, take);
                taken_total += take;
            }
        }
        taken_total
    }

    /// The member whose leading edge comes first on `axis`, with that coordinate.
    ///
    /// Ties are broken by the smallest id so the reference edge — and therefore the
    /// whole shrink — stays deterministic.
    fn leading_member(
        &self,
        axis: Axis,
        rects: &BTreeMap<PanelId, Rect>,
    ) -> Option<(PanelId, f32)> {
        let mut leader: Option<(PanelId, f32)> = None;
        for id in self.members {
            let Some(rect) = rects.get(id).copied() else {
                continue;
            };
            let start = axis.start(rect);
            let better = match leader {
                Some((_, best)) => start < best,
                None => true,
            };
            if better {
                leader = Some((*id, start));
            }
        }
        leader
    }

    /// Distributes `deficit` points of overflow over the panels that can relieve it,
    /// draining the shrink tiers in [`SHRINK_PRIORITIES`] order.
    ///
    /// A tier is only asked for what the tiers before it could not give, which is
    /// what makes a manual size an intent rather than a request: the panels the
    /// user never touched are pushed to their floors before a single point is
    /// taken from the one being resized. Inside one tier the distribution is the
    /// water-filling of [`ChainContext::fill_tier`]. The result is ADDED to
    /// `assigned`, which the caller applies once per panel.
    fn water_fill(
        &self,
        axis: Axis,
        deficit: f32,
        gradient: &BTreeMap<PanelId, f32>,
        sizes: &BTreeMap<PanelId, Vec2>,
        assigned: &mut BTreeMap<PanelId, f32>,
    ) {
        let mut remaining = deficit;
        for priority in SHRINK_PRIORITIES {
            if remaining <= FIT_EPSILON {
                return;
            }
            remaining = self.fill_tier(axis, remaining, priority, gradient, sizes, assigned);
        }
    }

    /// Water-fills `deficit` over the panels of ONE shrink tier, returning what
    /// the tier could not absorb.
    ///
    /// `gradient` maps a panel to how much one point taken from it moves the
    /// overflowing edge; only strictly positive entries take part. The pool is
    /// visited by ascending remaining slack so that a panel which cannot give its
    /// full share is capped first and the rest is re-divided — classic water-filling,
    /// generalised to weighted contributions.
    fn fill_tier(
        &self,
        axis: Axis,
        deficit: f32,
        priority: ShrinkPriority,
        gradient: &BTreeMap<PanelId, f32>,
        sizes: &BTreeMap<PanelId, Vec2>,
        assigned: &mut BTreeMap<PanelId, f32>,
    ) -> f32 {
        let mut pool: Vec<(f32, f32, PanelId)> = Vec::new();
        for id in self.members {
            let Some(node) = self.nodes.get(id) else {
                continue;
            };
            if shrink_priority(node) != priority {
                continue;
            }
            // A collapsed panel is exactly one header tall; its width may still be
            // reduced, which is why this is not a blanket exclusion.
            if axis == Axis::Y && node.collapsed {
                continue;
            }
            let contribution = gradient.get(id).copied().unwrap_or(0.0);
            if contribution <= FIT_EPSILON {
                continue;
            }
            let Some(size) = sizes.get(id).copied() else {
                continue;
            };
            let min = self.floor.get(id).map_or(0.0, |min| axis.length(*min));
            let slack = axis.length(size) - min - assigned.get(id).copied().unwrap_or(0.0);
            if slack > FIT_EPSILON {
                pool.push((slack, contribution, *id));
            }
        }
        // Ascending slack, ties broken by id: the order decides who is capped first,
        // so it must be total and deterministic.
        pool.sort_by(|left, right| {
            left.0
                .partial_cmp(&right.0)
                .unwrap_or(Ordering::Equal)
                .then(left.2.cmp(&right.2))
        });

        let mut remaining = deficit;
        let mut contributions: f32 = pool.iter().map(|(_, contribution, _)| contribution).sum();
        for (slack, contribution, id) in pool {
            if remaining <= FIT_EPSILON || contributions <= FIT_EPSILON {
                break;
            }
            // Everyone still in the pool is asked for the same number of points; a
            // panel that cannot give them is capped and its share re-divided by the
            // next iteration, which recomputes the level from what is left.
            let level = (remaining / contributions).min(slack).max(0.0);
            *assigned.entry(id).or_insert(0.0) += level;
            remaining -= level * contribution;
            contributions -= contribution;
        }
        remaining.max(0.0)
    }

    /// Exact derivative of `id`'s leading edge on `axis` with respect to the size of
    /// every panel of its chain.
    ///
    /// Placement is affine in the sizes (see [`place_outside`] / [`place_inside`]),
    /// so a coefficient map describes it exactly rather than approximately: walking
    /// from `id` up to its chain root, each anchor contributes `+1` when the target
    /// pushes the panel along this axis, `-1` when the panel's own size pushes its
    /// start back, and `±align` when the edge merely aligns them. The root's own
    /// anchor contributes through [`root_start_coefficient`].
    ///
    /// The walk is bounded by the chain length: a cycle (which `DockLayout::validate`
    /// rejects) must not spin here.
    fn start_coefficients(&self, id: PanelId, axis: Axis) -> BTreeMap<PanelId, f32> {
        let mut coefficients: BTreeMap<PanelId, f32> = BTreeMap::new();
        let mut current = id;
        for _ in 0..self.members.len() {
            let Some(node) = self.nodes.get(&current) else {
                break;
            };
            let parent = match node.anchor {
                PanelAnchor::Panel {
                    target,
                    edge,
                    align,
                } if self.members.contains(&target) && self.nodes.contains_key(&target) => {
                    Some((target, edge, sanitize_fraction(align)))
                }
                // Every other anchor — including a `Panel` one whose target is not
                // in this chain, which `propagate` lays out free — ends the walk.
                PanelAnchor::Panel { .. }
                | PanelAnchor::Free
                | PanelAnchor::ViewportEdge { .. }
                | PanelAnchor::CanvasControls { .. } => None,
            };
            let Some((target, edge, align)) = parent else {
                let own = root_start_coefficient(node.anchor, axis, self.controls.is_some());
                if own != 0.0 {
                    *coefficients.entry(current).or_insert(0.0) += own;
                }
                break;
            };
            match axis.role(edge) {
                EdgeRole::Trailing => *coefficients.entry(target).or_insert(0.0) += 1.0,
                EdgeRole::Leading => *coefficients.entry(current).or_insert(0.0) -= 1.0,
                EdgeRole::Perpendicular => {
                    *coefficients.entry(target).or_insert(0.0) += align;
                    *coefficients.entry(current).or_insert(0.0) -= align;
                }
            }
            current = target;
        }
        coefficients
    }

}

/// Derivative of a chain ROOT's leading edge on `axis` with respect to its own
/// size, which is non-zero whenever the root is positioned by its far edge.
///
/// Mirrors [`place_root`]: a `ViewportEdge` root flush with the trailing side of
/// the area starts one own-size earlier, a `CanvasControls` root placed before
/// the controls rect likewise, and an alignment fraction takes that share of it.
/// A free root — and a `CanvasControls` root with no controls rect, which
/// degrades to free — is positioned by its `pos` and contributes nothing.
fn root_start_coefficient(anchor: PanelAnchor, axis: Axis, has_controls: bool) -> f32 {
    match anchor {
        PanelAnchor::Free | PanelAnchor::Panel { .. } => 0.0,
        PanelAnchor::ViewportEdge { edge, along } => match axis.role(edge) {
            EdgeRole::Leading => 0.0,
            EdgeRole::Trailing => -1.0,
            EdgeRole::Perpendicular => -sanitize_fraction(along),
        },
        PanelAnchor::CanvasControls { edge, along } => {
            if !has_controls {
                return 0.0;
            }
            match axis.role(edge) {
                EdgeRole::Leading => -1.0,
                EdgeRole::Trailing => 0.0,
                EdgeRole::Perpendicular => -sanitize_fraction(along),
            }
        }
    }
}

/// `true` when `outer` fully contains `inner`, tolerating sub-pixel error.
fn contains_rect(outer: Rect, inner: Rect) -> bool {
    inner.left() >= outer.left() - FIT_EPSILON
        && inner.right() <= outer.right() + FIT_EPSILON
        && inner.top() >= outer.top() - FIT_EPSILON
        && inner.bottom() <= outer.bottom() + FIT_EPSILON
}

/// Replaces non-finite or negative components with `0.0`.
fn sanitize_size(size: Vec2) -> Vec2 {
    Vec2::new(sanitize_scalar(size.x).max(0.0), sanitize_scalar(size.y).max(0.0))
}

/// Replaces non-finite components with `0.0`, keeping the sign.
fn sanitize_offset(offset: Vec2) -> Vec2 {
    Vec2::new(sanitize_scalar(offset.x), sanitize_scalar(offset.y))
}

/// Replaces a non-finite scalar with `0.0`.
fn sanitize_scalar(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

/// Clamps an alignment fraction into `0.0..=1.0`, mapping non-finite to `0.0`.
fn sanitize_fraction(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Makes a rect finite and non-inverted: non-finite coordinates become `0.0`,
/// and a `max` below `min` collapses onto `min`.
fn sanitize_rect(rect: Rect) -> Rect {
    let min = Pos2::new(sanitize_scalar(rect.min.x), sanitize_scalar(rect.min.y));
    let max = Pos2::new(sanitize_scalar(rect.max.x), sanitize_scalar(rect.max.y));
    Rect::from_min_max(min, Pos2::new(max.x.max(min.x), max.y.max(min.y)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::panel_dock::model::TabId;

    const TAB_A: TabId = TabId::new("test.a");
    const TAB_B: TabId = TabId::new("test.b");
    const TAB_C: TabId = TabId::new("test.c");
    const TAB_D: TabId = TabId::new("test.d");

    const AREA: Rect = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1000.0, 800.0));

    fn node(id: u32, tab: TabId) -> PanelNode {
        PanelNode::new(PanelId::new(id), HostId::MainWindow, vec![tab])
            .expect("test panel must be constructible")
    }

    fn free_at(id: u32, tab: TabId, x: f32, y: f32) -> PanelNode {
        let mut node = node(id, tab);
        node.pos = Pos2::new(x, y);
        node
    }

    fn attach(node: &mut PanelNode, target: u32, edge: DockEdge, align: f32) {
        node.anchor = PanelAnchor::Panel {
            target: PanelId::new(target),
            edge,
            align,
        };
    }

    fn sizes(entries: &[(u32, f32, f32)]) -> PanelSizes {
        entries
            .iter()
            .map(|(id, w, h)| (PanelId::new(*id), Vec2::new(*w, *h)))
            .collect()
    }

    fn rect_of(solved: &SolvedLayout, id: u32) -> Rect {
        solved
            .get(PanelId::new(id))
            .expect("panel must be solved")
            .rect
    }

    #[test]
    fn empty_layout_solves_to_nothing() {
        let layout = DockLayout::new();
        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &PanelSizes::new(),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        assert!(solved.is_empty());
        assert_eq!(solved.len(), 0);
    }

    #[test]
    fn other_hosts_are_not_solved() {
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        layout
            .insert_panel(
                PanelNode::new(PanelId::new(1), HostId::SubWindow(0), vec![TAB_B])
                    .expect("sub panel"),
            )
            .expect("insert 1");
        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &PanelSizes::new(),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        assert_eq!(solved.len(), 1);
        assert!(solved.get(PanelId::new(1)).is_none());
    }

    #[test]
    fn free_panel_is_placed_relative_to_the_area_origin() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(free_at(0, TAB_A, 40.0, 60.0))
            .expect("insert 0");
        let area = Rect::from_min_size(Pos2::new(100.0, 200.0), Vec2::new(1000.0, 800.0));
        let solved = solve(
            &layout,
            HostId::MainWindow,
            area,
            &sizes(&[(0, 300.0, 200.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        assert_eq!(
            rect_of(&solved, 0),
            Rect::from_min_size(Pos2::new(140.0, 260.0), Vec2::new(300.0, 200.0))
        );
    }

    #[test]
    fn viewport_edge_anchor_insets_by_the_gap() {
        let mut layout = DockLayout::new();
        let mut right = node(0, TAB_A);
        right.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Right,
            along: 0.0,
        };
        layout.insert_panel(right).expect("insert 0");
        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &sizes(&[(0, 300.0, 200.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        let rect = rect_of(&solved, 0);
        assert!((AREA.right() - rect.right() - DOCK_GAP).abs() < FIT_EPSILON);
        assert!((rect.top() - AREA.top()).abs() < FIT_EPSILON);
    }

    #[test]
    fn viewport_edge_along_fraction_is_clamped() {
        let mut layout = DockLayout::new();
        let mut top = node(0, TAB_A);
        top.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Top,
            along: 4.0,
        };
        layout.insert_panel(top).expect("insert 0");
        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &sizes(&[(0, 300.0, 200.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        let rect = rect_of(&solved, 0);
        // `along = 1.0` after clamping: flush with the area's right side.
        assert!((rect.right() - AREA.right()).abs() < FIT_EPSILON);
        assert!((rect.top() - AREA.top() - DOCK_GAP).abs() < FIT_EPSILON);
    }

    #[test]
    fn canvas_controls_anchor_uses_the_supplied_rect() {
        let mut layout = DockLayout::new();
        let mut panel = free_at(0, TAB_A, 500.0, 500.0);
        panel.anchor = PanelAnchor::CanvasControls {
            edge: DockEdge::Bottom,
            along: 0.0,
        };
        layout.insert_panel(panel).expect("insert 0");
        let controls = Rect::from_min_size(Pos2::new(20.0, 20.0), Vec2::new(200.0, 40.0));
        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &sizes(&[(0, 300.0, 200.0)]),
            &PanelSizes::new(),
            Some(controls),
            PanelChrome::default(),
        );
        let rect = rect_of(&solved, 0);
        assert!((rect.top() - controls.bottom() - DOCK_GAP).abs() < FIT_EPSILON);
        assert!((rect.left() - controls.left()).abs() < FIT_EPSILON);
    }

    #[test]
    fn canvas_controls_anchor_without_a_rect_falls_back_to_the_free_position() {
        let mut layout = DockLayout::new();
        let mut panel = free_at(0, TAB_A, 500.0, 400.0);
        panel.anchor = PanelAnchor::CanvasControls {
            edge: DockEdge::Bottom,
            along: 0.0,
        };
        layout.insert_panel(panel).expect("insert 0");
        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &sizes(&[(0, 300.0, 200.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        assert_eq!(rect_of(&solved, 0).min, Pos2::new(500.0, 400.0));
    }

    #[test]
    fn vertical_chain_keeps_exactly_one_gap() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(free_at(0, TAB_A, 50.0, 50.0))
            .expect("insert 0");
        let mut below = node(1, TAB_B);
        attach(&mut below, 0, DockEdge::Bottom, 0.0);
        layout.insert_panel(below).expect("insert 1");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &sizes(&[(0, 300.0, 200.0), (1, 260.0, 180.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        let top = rect_of(&solved, 0);
        let bottom = rect_of(&solved, 1);
        assert!((bottom.top() - top.bottom() - DOCK_GAP).abs() < FIT_EPSILON);
        assert!((bottom.left() - top.left()).abs() < FIT_EPSILON);
        assert!(!solved.get(PanelId::new(1)).expect("solved").shrunk);
    }

    #[test]
    fn align_one_makes_the_far_edges_flush() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(free_at(0, TAB_A, 50.0, 50.0))
            .expect("insert 0");
        let mut below = node(1, TAB_B);
        attach(&mut below, 0, DockEdge::Bottom, 1.0);
        layout.insert_panel(below).expect("insert 1");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &sizes(&[(0, 300.0, 200.0), (1, 260.0, 180.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        assert!((rect_of(&solved, 1).right() - rect_of(&solved, 0).right()).abs() < FIT_EPSILON);
    }

    #[test]
    fn horizontal_chain_keeps_the_gap_on_both_sides() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(free_at(0, TAB_A, 400.0, 100.0))
            .expect("insert 0");
        let mut right = node(1, TAB_B);
        attach(&mut right, 0, DockEdge::Right, 0.0);
        layout.insert_panel(right).expect("insert 1");
        let mut left = node(2, TAB_C);
        attach(&mut left, 0, DockEdge::Left, 0.0);
        layout.insert_panel(left).expect("insert 2");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &sizes(&[(0, 200.0, 200.0), (1, 150.0, 180.0), (2, 120.0, 160.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        let center = rect_of(&solved, 0);
        let right = rect_of(&solved, 1);
        let left = rect_of(&solved, 2);
        assert!((right.left() - center.right() - DOCK_GAP).abs() < FIT_EPSILON);
        assert!((center.left() - left.right() - DOCK_GAP).abs() < FIT_EPSILON);
        assert!((right.top() - center.top()).abs() < FIT_EPSILON);
        assert!((left.top() - center.top()).abs() < FIT_EPSILON);
    }

    #[test]
    fn mixed_chain_places_both_branches_of_one_target() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(free_at(0, TAB_A, 400.0, 300.0))
            .expect("insert 0");
        let mut left = node(1, TAB_B);
        attach(&mut left, 0, DockEdge::Left, 0.0);
        layout.insert_panel(left).expect("insert 1");
        let mut below = node(2, TAB_C);
        attach(&mut below, 0, DockEdge::Bottom, 0.0);
        layout.insert_panel(below).expect("insert 2");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &sizes(&[(0, 200.0, 200.0), (1, 150.0, 150.0), (2, 180.0, 160.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        let center = rect_of(&solved, 0);
        assert!((center.left() - rect_of(&solved, 1).right() - DOCK_GAP).abs() < FIT_EPSILON);
        assert!((rect_of(&solved, 2).top() - center.bottom() - DOCK_GAP).abs() < FIT_EPSILON);
        assert_eq!(layout.chains().len(), 1);
    }

    #[test]
    fn a_chain_hanging_out_is_translated_back_inside() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(free_at(0, TAB_A, 900.0, 700.0))
            .expect("insert 0");
        let mut below = node(1, TAB_B);
        attach(&mut below, 0, DockEdge::Bottom, 0.0);
        layout.insert_panel(below).expect("insert 1");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &sizes(&[(0, 300.0, 200.0), (1, 300.0, 200.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        let top = rect_of(&solved, 0);
        let bottom = rect_of(&solved, 1);
        assert!(contains_rect(AREA, top));
        assert!(contains_rect(AREA, bottom));
        // The gap survives the translation.
        assert!((bottom.top() - top.bottom() - DOCK_GAP).abs() < FIT_EPSILON);
        for id in [0_u32, 1] {
            assert!(!solved.get(PanelId::new(id)).expect("solved").shrunk);
        }
    }

    #[test]
    fn water_filling_splits_the_deficit_evenly() {
        // Three 300 pt panels + two gaps = 916 pt in a 700 pt area: a 216 pt
        // deficit shared by three panels with plenty of slack.
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 700.0));
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        let mut second = node(1, TAB_B);
        attach(&mut second, 0, DockEdge::Bottom, 0.0);
        layout.insert_panel(second).expect("insert 1");
        let mut third = node(2, TAB_C);
        attach(&mut third, 1, DockEdge::Bottom, 0.0);
        layout.insert_panel(third).expect("insert 2");

        let requested = sizes(&[(0, 300.0, 300.0), (1, 300.0, 300.0), (2, 300.0, 300.0)]);
        let solved = solve(
            &layout,
            HostId::MainWindow,
            area,
            &requested,
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );

        let deficit = 3.0f32.mul_add(300.0, 2.0 * DOCK_GAP) - area.height();
        let mut total_given = 0.0;
        for id in [0_u32, 1, 2] {
            let panel = solved.get(PanelId::new(id)).expect("solved");
            let given = 300.0 - panel.rect.height();
            total_given += given;
            assert!((given - deficit / 3.0).abs() < FIT_EPSILON, "given {given}");
            assert!(panel.shrunk);
            assert!(
                (panel.body_max_height - (panel.rect.height() - COLLAPSED_PANEL_HEIGHT)).abs()
                    < FIT_EPSILON
            );
        }
        assert!((total_given - deficit).abs() < FIT_EPSILON);
        // The chain now fits exactly, and the gaps are intact.
        let first = rect_of(&solved, 0);
        let last = rect_of(&solved, 2);
        assert!(contains_rect(area, first));
        assert!(contains_rect(area, last));
        assert!((rect_of(&solved, 1).top() - first.bottom() - DOCK_GAP).abs() < FIT_EPSILON);
    }

    #[test]
    fn water_filling_respects_individual_floors() {
        // Panel 0 must not go below 200 pt, the other two below the default
        // floor of 144 pt. The deficit is large enough to cap panel 1 and 2.
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 500.0));
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        let mut second = node(1, TAB_B);
        attach(&mut second, 0, DockEdge::Bottom, 0.0);
        layout.insert_panel(second).expect("insert 1");
        let mut third = node(2, TAB_C);
        attach(&mut third, 1, DockEdge::Bottom, 0.0);
        layout.insert_panel(third).expect("insert 2");

        let requested = sizes(&[(0, 300.0, 300.0), (1, 300.0, 200.0), (2, 300.0, 200.0)]);
        let mins = sizes(&[(0, 0.0, 200.0)]);
        let solved = solve(
            &layout,
            HostId::MainWindow,
            area,
            &requested,
            &mins,
            None,
            PanelChrome::default(),
        );

        let first = solved.get(PanelId::new(0)).expect("solved 0");
        let second = solved.get(PanelId::new(1)).expect("solved 1");
        let third = solved.get(PanelId::new(2)).expect("solved 2");
        assert!(first.rect.height() >= 200.0 - FIT_EPSILON);
        let default_floor = COLLAPSED_PANEL_HEIGHT + PANEL_MIN_CONTENT_HEIGHT;
        assert!(second.rect.height() >= default_floor - FIT_EPSILON);
        assert!(third.rect.height() >= default_floor - FIT_EPSILON);

        // 716 pt of content in 500 pt: the 216 pt deficit exceeds the 212 pt of
        // total slack (100 + 56 + 56), so every panel ends up on its floor and
        // stays flagged.
        assert!((first.rect.height() - 200.0).abs() < FIT_EPSILON);
        assert!((second.rect.height() - default_floor).abs() < FIT_EPSILON);
        assert!((third.rect.height() - default_floor).abs() < FIT_EPSILON);
        for panel in [first, second, third] {
            assert!(panel.shrunk);
        }
    }

    #[test]
    fn collapsed_panels_keep_their_header_and_never_shrink() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 400.0));
        let mut layout = DockLayout::new();
        let mut head = node(0, TAB_A);
        head.collapsed = true;
        layout.insert_panel(head).expect("insert 0");
        let mut below = node(1, TAB_B);
        attach(&mut below, 0, DockEdge::Bottom, 0.0);
        layout.insert_panel(below).expect("insert 1");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            area,
            &sizes(&[(0, 300.0, 400.0), (1, 300.0, 500.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        let collapsed = solved.get(PanelId::new(0)).expect("solved 0");
        assert!((collapsed.rect.height() - COLLAPSED_PANEL_HEIGHT).abs() < FIT_EPSILON);
        assert!((collapsed.body_max_height - 0.0).abs() < FIT_EPSILON);
        assert!(!collapsed.shrunk);
        // The whole deficit landed on the expanded neighbour.
        let expanded = solved.get(PanelId::new(1)).expect("solved 1");
        assert!(expanded.shrunk);
        assert!(expanded.rect.height() < 500.0);
    }

    #[test]
    fn a_collapsed_panel_is_exactly_the_measured_header_tall() {
        // The nominal 24 pt underestimates the real header by 12 pt; laying the
        // chain out with the nominal value used to overlap the panel below.
        let chrome = PanelChrome::new(36.0, 40.0);
        let mut layout = DockLayout::new();
        let mut head = node(0, TAB_A);
        head.collapsed = true;
        layout.insert_panel(head).expect("insert 0");
        let mut below = node(1, TAB_B);
        attach(&mut below, 0, DockEdge::Bottom, 0.0);
        layout.insert_panel(below).expect("insert 1");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &sizes(&[(0, 300.0, 400.0), (1, 300.0, 200.0)]),
            &PanelSizes::new(),
            None,
            chrome,
        );
        let collapsed = rect_of(&solved, 0);
        let below = rect_of(&solved, 1);
        assert!((collapsed.height() - 36.0).abs() < FIT_EPSILON);
        // No overlap: the neighbour starts one gap below the REAL header.
        assert!((below.top() - collapsed.bottom() - DOCK_GAP).abs() < FIT_EPSILON);
        // And the expanded panel's body budget pays the measured overhead.
        let expanded = solved.get(PanelId::new(1)).expect("solved 1");
        assert!((expanded.body_max_height - (200.0 - 40.0)).abs() < FIT_EPSILON);
    }

    #[test]
    fn a_broken_chrome_measurement_degrades_to_the_nominal_one() {
        let chrome = PanelChrome::new(f32::NAN, -8.0);
        assert_eq!(chrome, PanelChrome::default());
        assert!(chrome.max_difference(PanelChrome::default()).abs() < FIT_EPSILON);
        assert!(
            (PanelChrome::new(36.0, 40.0).max_difference(PanelChrome::default()) - 16.0).abs()
                < FIT_EPSILON
        );
    }

    #[test]
    fn the_default_floor_grows_with_the_measured_header() {
        let chrome = PanelChrome::new(36.0, 40.0);
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 120.0));
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        let solved = solve(
            &layout,
            HostId::MainWindow,
            area,
            &sizes(&[(0, 300.0, 400.0)]),
            &PanelSizes::new(),
            None,
            chrome,
        );
        // Floor = measured header + PANEL_MIN_CONTENT_HEIGHT, not the nominal one.
        let panel = solved.get(PanelId::new(0)).expect("solved 0");
        assert!((panel.rect.height() - (36.0 + PANEL_MIN_CONTENT_HEIGHT)).abs() < FIT_EPSILON);
    }

    #[test]
    fn an_unfittable_chain_is_reported_not_panicked() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(200.0, 100.0));
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        let mut below = node(1, TAB_B);
        attach(&mut below, 0, DockEdge::Bottom, 0.0);
        layout.insert_panel(below).expect("insert 1");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            area,
            &sizes(&[(0, 400.0, 400.0), (1, 400.0, 400.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        let first = solved.get(PanelId::new(0)).expect("solved 0");
        assert!(first.shrunk);
        // Best effort: the chain is aligned to the area's top-left corner.
        assert!((first.rect.left() - area.left()).abs() < FIT_EPSILON);
        assert!((first.rect.top() - area.top()).abs() < FIT_EPSILON);
        assert!(first.rect.height().is_finite());
    }

    #[test]
    fn solving_is_deterministic() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(free_at(0, TAB_A, 10.0, 10.0))
            .expect("insert 0");
        let mut below = node(1, TAB_B);
        attach(&mut below, 0, DockEdge::Bottom, 0.5);
        layout.insert_panel(below).expect("insert 1");
        let requested = sizes(&[(0, 300.0, 400.0), (1, 300.0, 400.0)]);

        let first = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &requested,
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        let second = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &requested,
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        assert_eq!(first, second);
    }

    #[test]
    fn solving_a_solved_layout_changes_nothing() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 700.0));
        let mut layout = DockLayout::new();
        layout
            .insert_panel(free_at(0, TAB_A, 600.0, 500.0))
            .expect("insert 0");
        let mut second = node(1, TAB_B);
        attach(&mut second, 0, DockEdge::Bottom, 0.0);
        layout.insert_panel(second).expect("insert 1");
        let mut third = node(2, TAB_C);
        attach(&mut third, 1, DockEdge::Bottom, 0.0);
        layout.insert_panel(third).expect("insert 2");
        let requested = sizes(&[(0, 300.0, 300.0), (1, 300.0, 300.0), (2, 300.0, 300.0)]);

        let first_pass = solve(
            &layout,
            HostId::MainWindow,
            area,
            &requested,
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );

        // Feed the solved geometry back as the model's own state, exactly as
        // the frame driver will after a manual resize or a drag.
        let mut settled = layout.clone();
        for (id, panel) in first_pass.iter() {
            settled
                .set_panel_pos(
                    id,
                    Pos2::new(panel.rect.left() - area.left(), panel.rect.top() - area.top()),
                )
                .expect("panel exists");
            settled
                .set_size_override(id, Some(panel.rect.size()))
                .expect("panel exists");
        }
        let second_pass = solve(
            &settled,
            HostId::MainWindow,
            area,
            &requested,
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        // Idempotence is a statement about geometry: `shrunk` legitimately
        // drops to `false` once the settled sizes are what the panels ask for.
        assert_eq!(first_pass.len(), second_pass.len());
        for (id, panel) in first_pass.iter() {
            let again = second_pass.get(id).expect("panel solved again");
            assert_eq!(panel.rect, again.rect, "panel {id}");
            assert!((panel.body_max_height - again.body_max_height).abs() < FIT_EPSILON);
        }
    }

    #[test]
    fn degenerate_area_and_nan_inputs_do_not_panic() {
        let mut layout = DockLayout::new();
        let mut broken = node(0, TAB_A);
        broken.pos = Pos2::new(f32::NAN, f32::INFINITY);
        layout.insert_panel(broken).expect("insert 0");
        let mut below = node(1, TAB_B);
        attach(&mut below, 0, DockEdge::Bottom, f32::NAN);
        layout.insert_panel(below).expect("insert 1");

        let mut requested = PanelSizes::new();
        requested.insert(PanelId::new(0), Vec2::new(f32::NAN, -50.0));
        requested.insert(PanelId::new(1), Vec2::new(f32::INFINITY, f32::NAN));
        let mut mins = PanelSizes::new();
        mins.insert(PanelId::new(1), Vec2::new(f32::NAN, f32::NAN));

        // Inverted, zero-sized and non-finite areas all have to be survivable.
        for area in [
            Rect::from_min_max(Pos2::new(100.0, 100.0), Pos2::new(0.0, 0.0)),
            Rect::from_min_size(Pos2::ZERO, Vec2::ZERO),
            Rect::from_min_max(Pos2::new(f32::NAN, 0.0), Pos2::new(f32::NAN, f32::NAN)),
            Rect::NOTHING,
        ] {
            let solved = solve(
                &layout,
                HostId::MainWindow,
                area,
                &requested,
                &mins,
                Some(Rect::NOTHING),
            PanelChrome::default(),
        );
            assert_eq!(solved.len(), 2);
            for (_, panel) in solved.iter() {
                assert!(panel.rect.min.x.is_finite());
                assert!(panel.rect.min.y.is_finite());
                assert!(panel.rect.max.x.is_finite());
                assert!(panel.rect.max.y.is_finite());
                assert!(panel.body_max_height >= 0.0);
            }
        }
    }

    #[test]
    fn panel_sizes_sanitizes_on_insert() {
        let mut map = PanelSizes::new();
        assert!(map.is_empty());
        map.insert(PanelId::new(0), Vec2::new(f32::NAN, -10.0));
        assert_eq!(map.get(PanelId::new(0)), Some(Vec2::ZERO));
        map.insert(PanelId::new(1), Vec2::new(120.0, 40.0));
        assert_eq!(map.len(), 2);
        assert_eq!(map.remove(PanelId::new(1)), Some(Vec2::new(120.0, 40.0)));
        assert_eq!(map.get(PanelId::new(1)), None);
    }

    #[test]
    fn size_override_wins_over_the_measured_size() {
        let mut layout = DockLayout::new();
        let mut fixed = free_at(0, TAB_A, 10.0, 10.0);
        fixed.size_override = Some(Vec2::new(420.0, 260.0));
        layout.insert_panel(fixed).expect("insert 0");
        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &sizes(&[(0, 300.0, 200.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        assert_eq!(rect_of(&solved, 0).size(), Vec2::new(420.0, 260.0));
    }

    #[test]
    fn a_neighbour_beside_the_overflowing_panel_keeps_its_size() {
        // A is 100 pt too tall for the area; B hangs off its RIGHT side and is
        // nowhere near the bottom. Sharing the deficit over "every member of the
        // chain" used to squeeze B down to its 144 pt floor while 350 pt of the
        // area next to it stood empty.
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 500.0));
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        let mut beside = node(1, TAB_B);
        attach(&mut beside, 0, DockEdge::Right, 0.0);
        layout.insert_panel(beside).expect("insert 1");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            area,
            &sizes(&[(0, 300.0, 600.0), (1, 300.0, 200.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        assert!((rect_of(&solved, 0).height() - 500.0).abs() < FIT_EPSILON);
        assert!((rect_of(&solved, 1).height() - 200.0).abs() < FIT_EPSILON);
        assert!(solved.get(PanelId::new(0)).expect("solved 0").shrunk);
        assert!(!solved.get(PanelId::new(1)).expect("solved 1").shrunk);
    }

    #[test]
    fn a_branching_chain_fits_and_re_solving_it_changes_nothing() {
        // Four panels asking 600 pt in a 500 pt area, three of them side by side
        // off the root's right edge. Splitting one bounding-box deficit over all
        // of them converged geometrically and ran out of iterations at 510 pt —
        // and every re-solve moved the result, so a one-pixel drag of the resize
        // grip SHRANK the panel.
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 500.0));
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        for (id, tab) in [(1_u32, TAB_B), (2, TAB_C), (3, TAB_D)] {
            let mut sibling = node(id, tab);
            attach(&mut sibling, 0, DockEdge::Right, 0.0);
            layout.insert_panel(sibling).expect("insert sibling");
        }
        let requested = sizes(&[
            (0, 300.0, 600.0),
            (1, 300.0, 600.0),
            (2, 300.0, 600.0),
            (3, 300.0, 600.0),
        ]);

        let first_pass = solve(
            &layout,
            HostId::MainWindow,
            area,
            &requested,
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        for id in [0_u32, 1, 2, 3] {
            let rect = rect_of(&first_pass, id);
            assert!(
                (rect.height() - 500.0).abs() < FIT_EPSILON,
                "panel {id} is {} tall",
                rect.height()
            );
            assert!(rect.top() >= area.top() - FIT_EPSILON);
            assert!(rect.bottom() <= area.bottom() + FIT_EPSILON);
        }

        // Feeding the solved sizes back in — exactly what the driver stores after
        // a manual resize — must reproduce the same geometry.
        let mut settled = layout.clone();
        for (id, panel) in first_pass.iter() {
            settled
                .set_size_override(id, Some(panel.rect.size()))
                .expect("panel exists");
        }
        let second_pass = solve(
            &settled,
            HostId::MainWindow,
            area,
            &requested,
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        for (id, panel) in first_pass.iter() {
            let again = second_pass.get(id).expect("panel solved again");
            assert_eq!(panel.rect, again.rect, "panel {id}");
        }
    }

    #[test]
    fn the_minimum_size_does_not_inflate_a_small_panels_request() {
        // A tab whose content needs 60 pt must be laid out at 60 pt: raising the
        // REQUEST to the 144 pt floor left the panel below it hanging a 92 pt
        // hole away from a panel that ends much earlier, because the drawn panel
        // shrinks to its content while the solver reserved the floor.
        let mut layout = DockLayout::new();
        layout
            .insert_panel(free_at(0, TAB_A, 20.0, 20.0))
            .expect("insert 0");
        let mut below = node(1, TAB_B);
        attach(&mut below, 0, DockEdge::Bottom, 0.0);
        layout.insert_panel(below).expect("insert 1");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &sizes(&[(0, 300.0, 60.0), (1, 300.0, 200.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        let small = rect_of(&solved, 0);
        assert!((small.height() - 60.0).abs() < FIT_EPSILON);
        assert!((rect_of(&solved, 1).top() - small.bottom() - DOCK_GAP).abs() < FIT_EPSILON);
        // The floor still bounds the SHRINK: nothing asked for it here.
        assert!(!solved.get(PanelId::new(0)).expect("solved 0").shrunk);
    }

    #[test]
    fn a_request_below_what_the_widget_can_draw_is_raised() {
        // The physical bound is not a preference: a rect narrower or shorter than
        // the drawn frame would let the neighbour placed one gap away overlap it.
        let chrome = PanelChrome::new(36.0, 40.0);
        let mut layout = DockLayout::new();
        layout
            .insert_panel(free_at(0, TAB_A, 10.0, 10.0))
            .expect("insert 0");
        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &sizes(&[(0, 5.0, 12.0)]),
            &PanelSizes::new(),
            None,
            chrome,
        );
        let rect = rect_of(&solved, 0);
        assert!((rect.width() - PANEL_MIN_WIDTH).abs() < FIT_EPSILON);
        assert!((rect.height() - (40.0 + PANEL_MIN_BODY_HEIGHT)).abs() < FIT_EPSILON);
    }

    #[test]
    fn a_chain_too_wide_is_shrunk_instead_of_hanging_out() {
        // 300 + gap + 300 in a 500 pt wide area. Leaving the overflow to the
        // horizontal translation pushed the right panel — and its resize grip —
        // outside the area, where the user could no longer make it smaller.
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(500.0, 800.0));
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        let mut beside = node(1, TAB_B);
        attach(&mut beside, 0, DockEdge::Right, 0.0);
        layout.insert_panel(beside).expect("insert 1");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            area,
            &sizes(&[(0, 300.0, 200.0), (1, 300.0, 200.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        let left = rect_of(&solved, 0);
        let right = rect_of(&solved, 1);
        assert!(contains_rect(area, left));
        assert!(contains_rect(area, right));
        // Both gave the same 54 pt, and the gap between them is untouched.
        assert!((left.width() - right.width()).abs() < FIT_EPSILON);
        assert!((right.left() - left.right() - DOCK_GAP).abs() < FIT_EPSILON);
        // Heights are none of the horizontal shrink's business.
        assert!((left.height() - 200.0).abs() < FIT_EPSILON);
    }

    #[test]
    fn a_width_floor_stops_the_horizontal_shrink() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(500.0, 800.0));
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        let mut beside = node(1, TAB_B);
        attach(&mut beside, 0, DockEdge::Right, 0.0);
        layout.insert_panel(beside).expect("insert 1");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            area,
            &sizes(&[(0, 300.0, 200.0), (1, 300.0, 200.0)]),
            &sizes(&[(0, 280.0, 0.0), (1, 280.0, 0.0)]),
            None,
            PanelChrome::default(),
        );
        for id in [0_u32, 1] {
            let panel = solved.get(PanelId::new(id)).expect("solved");
            assert!((panel.rect.width() - 280.0).abs() < FIT_EPSILON);
            // Still 68 pt too wide for the area, which is what `shrunk` reports.
            assert!(panel.shrunk);
        }
    }

    #[test]
    fn siblings_of_one_side_are_stacked_when_the_gesture_queues_them() {
        // THE SIBLING CONTRACT. Two panels docked to the same side of one target
        // used to be given the same anchor and landed on top of each other, and
        // the buried one could not be reached at all. The docking gesture now
        // QUEUES the second one behind the first (`drag::resolve_slot`), which is
        // the layout asserted here: a column, one `DOCK_GAP` per joint, fitted
        // into the area by the ordinary shrink.
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 500.0));
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        let mut first = node(1, TAB_B);
        attach(&mut first, 0, DockEdge::Bottom, 0.0);
        layout.insert_panel(first).expect("insert 1");
        let mut second = node(2, TAB_C);
        attach(&mut second, 1, DockEdge::Bottom, 0.0);
        layout.insert_panel(second).expect("insert 2");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            area,
            &sizes(&[(0, 300.0, 200.0), (1, 300.0, 400.0), (2, 300.0, 400.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        let root = rect_of(&solved, 0);
        let first = rect_of(&solved, 1);
        let second = rect_of(&solved, 2);
        for rect in [root, first, second] {
            assert!(contains_rect(area, rect));
        }
        // A column, not a pile: each panel starts one gap below the previous one
        // and none of them overlaps another.
        assert!((first.top() - root.bottom() - DOCK_GAP).abs() < FIT_EPSILON);
        assert!((second.top() - first.bottom() - DOCK_GAP).abs() < FIT_EPSILON);
        assert!(!first.intersects(second));
        assert!(!root.intersects(first));
    }

    #[test]
    fn a_viewport_edge_root_gives_up_flushness_before_pushing_a_child_out() {
        // The root is flush with the area's bottom and a panel hangs BELOW it, so
        // the chain can only fit by moving up. Contract: everything stays inside
        // the area and the gaps stay exact; flushness is the property that yields,
        // because a panel outside the area cannot be reached at all.
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 500.0));
        let mut layout = DockLayout::new();
        let mut root = node(0, TAB_A);
        root.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Bottom,
            along: 0.0,
        };
        layout.insert_panel(root).expect("insert 0");
        let mut below = node(1, TAB_B);
        attach(&mut below, 0, DockEdge::Bottom, 0.0);
        layout.insert_panel(below).expect("insert 1");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            area,
            &sizes(&[(0, 300.0, 200.0), (1, 300.0, 200.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        let root = rect_of(&solved, 0);
        let below = rect_of(&solved, 1);
        assert!(contains_rect(area, root));
        assert!(contains_rect(area, below));
        assert!((below.top() - root.bottom() - DOCK_GAP).abs() < FIT_EPSILON);
        // Neither panel was shrunk: translation alone was enough.
        assert!((root.height() - 200.0).abs() < FIT_EPSILON);
        assert!((below.height() - 200.0).abs() < FIT_EPSILON);
        // The chain, not the root, is what ends up flush with the bottom edge.
        assert!((below.bottom() - area.bottom()).abs() < FIT_EPSILON);
    }

    #[test]
    fn a_viewport_edge_root_shortens_the_chain_from_its_own_side() {
        // Same shape, but now the chain is 108 pt too tall. Shrinking the ROOT
        // does not move the child's bottom edge by a single point — the child
        // hangs one gap under a root whose own top is pinned to `area.bottom -
        // height` — yet it still shortens the chain, because the root's top moves
        // DOWN as it shrinks. The solver has to see that: both panels shorten the
        // span by one point per point, so both pay half of the deficit.
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 500.0));
        let mut layout = DockLayout::new();
        let mut root = node(0, TAB_A);
        root.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Bottom,
            along: 0.0,
        };
        layout.insert_panel(root).expect("insert 0");
        let mut below = node(1, TAB_B);
        attach(&mut below, 0, DockEdge::Bottom, 0.0);
        layout.insert_panel(below).expect("insert 1");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            area,
            &sizes(&[(0, 300.0, 200.0), (1, 300.0, 400.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        // 200 + gap + 400 = 608 pt of chain in 500 pt: 54 points each.
        let root = rect_of(&solved, 0);
        let below = rect_of(&solved, 1);
        assert!((root.height() - 146.0).abs() < FIT_EPSILON);
        assert!((below.height() - 346.0).abs() < FIT_EPSILON);
        assert!((below.top() - root.bottom() - DOCK_GAP).abs() < FIT_EPSILON);
        assert!(contains_rect(area, root));
        assert!(contains_rect(area, below));
    }

    #[test]
    fn a_panel_without_a_measurement_uses_the_default_size() {
        let mut layout = DockLayout::new();
        layout
            .insert_panel(free_at(0, TAB_A, 10.0, 10.0))
            .expect("insert 0");
        let solved = solve(
            &layout,
            HostId::MainWindow,
            AREA,
            &PanelSizes::new(),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        assert_eq!(rect_of(&solved, 0).size(), DEFAULT_PANEL_SIZE);
    }

    /// THE MANUAL-SIZE CONTRACT. A chain that already fills the area used to give
    /// a resized panel's whole gain straight back to it: the water-filling asked
    /// everyone for the same number of points, and the panel that had just grown
    /// was the one with the most slack. Dragging the grip down then changed
    /// nothing on screen — the defect the user reported as "the height does not
    /// change, only the width".
    #[test]
    fn a_manually_sized_panel_is_shrunk_last() {
        // 700 pt of area for a 300 pt content-sized panel above a 500 pt panel
        // the user dragged to that height: a 108 pt deficit, all of which the
        // content-sized panel can absorb (its floor is 144 pt).
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 700.0));
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        let mut pinned = node(1, TAB_B);
        attach(&mut pinned, 0, DockEdge::Bottom, 0.0);
        pinned.size_override = Some(Vec2::new(300.0, 500.0));
        layout.insert_panel(pinned).expect("insert 1");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            area,
            &sizes(&[(0, 300.0, 300.0), (1, 300.0, 200.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );

        let auto = rect_of(&solved, 0);
        let manual = rect_of(&solved, 1);
        assert!(
            (manual.height() - 500.0).abs() < FIT_EPSILON,
            "the manual height must survive intact, got {}",
            manual.height()
        );
        // 300 + gap + 500 = 808 in 700: the content-sized panel gave all 108.
        assert!((auto.height() - 192.0).abs() < FIT_EPSILON, "got {}", auto.height());
        assert!((manual.top() - auto.bottom() - DOCK_GAP).abs() < FIT_EPSILON);
        assert!(contains_rect(area, auto));
        assert!(contains_rect(area, manual));
    }

    /// The tier is a PRIORITY, not an exemption: once every content-sized panel
    /// sits on its floor the manual one pays the rest, and never below its own
    /// floor.
    #[test]
    fn a_manual_size_still_yields_once_the_content_sized_panels_are_floored() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 500.0));
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        let mut pinned = node(1, TAB_B);
        attach(&mut pinned, 0, DockEdge::Bottom, 0.0);
        pinned.size_override = Some(Vec2::new(300.0, 400.0));
        layout.insert_panel(pinned).expect("insert 1");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            area,
            &sizes(&[(0, 300.0, 300.0), (1, 300.0, 200.0)]),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );

        // 300 + gap + 400 = 708 in 500: a 208 pt deficit. The content-sized panel
        // gives its full 156 pt of slack (300 -> the 144 pt default floor) and the
        // manual one covers the remaining 52.
        let floor = COLLAPSED_PANEL_HEIGHT + PANEL_MIN_CONTENT_HEIGHT;
        let auto = rect_of(&solved, 0);
        let manual = rect_of(&solved, 1);
        assert!((auto.height() - floor).abs() < FIT_EPSILON, "got {}", auto.height());
        assert!((manual.height() - 348.0).abs() < FIT_EPSILON, "got {}", manual.height());
        assert!(manual.height() >= floor - FIT_EPSILON);
        assert!(contains_rect(area, manual));
    }

    /// Several manual panels share what the content-sized ones could not absorb,
    /// by the same water-filling that governs one tier — nobody is singled out,
    /// and the floors still hold.
    #[test]
    fn several_manual_panels_share_the_leftover_deficit() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 600.0));
        let mut layout = DockLayout::new();
        let mut first = node(0, TAB_A);
        first.size_override = Some(Vec2::new(300.0, 400.0));
        layout.insert_panel(first).expect("insert 0");
        let mut second = node(1, TAB_B);
        attach(&mut second, 0, DockEdge::Bottom, 0.0);
        second.size_override = Some(Vec2::new(300.0, 400.0));
        layout.insert_panel(second).expect("insert 1");

        let solved = solve(
            &layout,
            HostId::MainWindow,
            area,
            &PanelSizes::new(),
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );

        // 400 + gap + 400 = 808 in 600: 208 pt over two manual panels, 104 each.
        let first = rect_of(&solved, 0);
        let second = rect_of(&solved, 1);
        assert!((first.height() - 296.0).abs() < FIT_EPSILON, "got {}", first.height());
        assert!((second.height() - 296.0).abs() < FIT_EPSILON, "got {}", second.height());
        assert!((second.top() - first.bottom() - DOCK_GAP).abs() < FIT_EPSILON);
        assert!(contains_rect(area, first));
        assert!(contains_rect(area, second));
    }

    /// The tiers must not introduce a cycle: solving the same mixed chain twice,
    /// and solving it again once the shrunk sizes are what the content-sized panel
    /// reports (which is what the driver stores), has to reproduce the geometry.
    /// Without that a resize would oscillate between two layouts forever.
    #[test]
    fn a_tiered_shrink_does_not_oscillate() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 700.0));
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        let mut pinned = node(1, TAB_B);
        attach(&mut pinned, 0, DockEdge::Bottom, 0.0);
        pinned.size_override = Some(Vec2::new(300.0, 500.0));
        layout.insert_panel(pinned).expect("insert 1");
        let mut below = node(2, TAB_C);
        attach(&mut below, 1, DockEdge::Bottom, 0.0);
        layout.insert_panel(below).expect("insert 2");

        let requested = sizes(&[(0, 300.0, 300.0), (1, 300.0, 200.0), (2, 300.0, 300.0)]);
        let first_pass = solve(
            &layout,
            HostId::MainWindow,
            area,
            &requested,
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        let second_pass = solve(
            &layout,
            HostId::MainWindow,
            area,
            &requested,
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        assert_eq!(first_pass, second_pass);

        // The content-sized panels now measure what they were shrunk to — the
        // driver stores the drawn content height every frame — while the manual
        // one keeps asking for the size the user dragged it to.
        let mut settled = PanelSizes::new();
        for (id, panel) in first_pass.iter() {
            settled.insert(id, panel.rect.size());
        }
        let third_pass = solve(
            &layout,
            HostId::MainWindow,
            area,
            &settled,
            &PanelSizes::new(),
            None,
            PanelChrome::default(),
        );
        for (id, panel) in first_pass.iter() {
            let again = third_pass.get(id).expect("panel solved again");
            assert_eq!(panel.rect, again.rect, "panel {id}");
        }
    }
}
