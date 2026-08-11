/*
File: src/widgets/panel_dock/drag.rs

Purpose:
The reorganisation gestures of the dockable-panel system (plan §4.8): moving a
whole panel until it snaps to a neighbouring edge, and moving a single tab from
one panel's header strip into another one.

Main responsibilities:
- carry the state of an in-progress panel move (`DragSession`) and the payload of
  an in-progress tab move (`DraggedTab`);
- find the edge a dragged panel would dock to, and describe the preview line that
  announces it (`find_snap`, `paint_snap_preview`);
- announce that a gesture has been pulled past the dock area's border hard enough
  to leave the window (`paint_detach_preview`; the verdict itself is
  `window::drag_tension`);
- keep two panels from being docked into the SAME slot (`resolve_slot`);
- answer the two questions a tab drop asks: at which index does it land in a
  header strip (`insertion_index`), and did it land on bare dock area
  (`empty_space_drop`).

Key structures:
- `DraggedTab`: drag-and-drop payload of a tab header.
- `DragSession`: the panel move in flight, owned by `PanelDockState`.
- `SnapCandidate`, `SnapTargets`: the result and the inputs of the snap search.

Key functions:
- `find_snap`, `panel_snap_candidates`, `resolve_slot`, `insertion_index`,
  `empty_space_drop`, `paint_snap_preview`, `paint_detach_preview`,
  `paint_insertion_marker`.

Notes:
Everything here except the two `paint_*` helpers is a pure function of plain geometry
and the `DockLayout`: the gestures are decided without a `Ui`, and the driver in
`mod.rs` only feeds them this frame's solved rects and applies the result through
the model's checked operations. That is what makes the whole phase testable
without a window.

`DragSession` is deliberately a struct owned by `PanelDockState` rather than a
tuple threaded through the driver: "the pointer left the window" and "which
window owns this gesture" are two fields here plus a branch in the release
handling (`window.rs` decides, `mod.rs` applies), not a second gesture.
*/

use std::collections::BTreeMap;

use egui::{Pos2, Rect, Stroke, Vec2};

use super::model::{DockEdge, DockLayout, HostId, PanelAnchor, PanelId, TabId};
use super::solver::{DOCK_GAP, place_inside, place_outside};

/// Largest distance, in points, between a dragged panel's edge and the edge it
/// would dock to for the snap to be offered at all.
pub const SNAP_DISTANCE: f32 = 24.0;

/// Width, in points, of the preview line painted along the prospective shared
/// edge while a panel is being dragged.
pub const SNAP_LINE_WIDTH: f32 = 2.0;

/// Length, in points, of one dash of the detach preview outline.
const DETACH_PREVIEW_DASH: f32 = 8.0;

/// Length, in points, of the gap between two dashes of the detach preview
/// outline.
const DETACH_PREVIEW_GAP: f32 = 5.0;

/// Overlap, in points, below which two panel rects are considered to merely
/// touch rather than to occupy the same slot. Keeps the sibling rule from firing
/// on a one-pixel seam.
const SLOT_OVERLAP_EPSILON: f32 = 1.0;

/// Free travel, in points, below which an alignment fraction is meaningless
/// (the panel is as long as the side it is aligned along) and reported as `0.0`.
const ALIGN_EPSILON: f32 = 0.5;

/// Drag-and-drop payload of a tab header (plan §4.8b).
///
/// Set by `CollapsiblePanel` on the caption's own dragged `Response` (never
/// through `Ui::dnd_drag_source`, see `panel.rs`) and read by whatever the tab is
/// released over. It must be read in the SAME pass as the release:
/// egui's drag-and-drop plugin clears the payload in `on_end_pass` of any pass
/// that saw `any_released()` (`egui-0.35.0/src/drag_and_drop.rs:52-62`).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct DraggedTab {
    /// The tab being moved.
    pub tab: TabId,
    /// The panel it was taken from.
    pub from: PanelId,
    /// Where inside the header the pointer grabbed it, in points from the
    /// header's top-left. This is what keeps the header under the cursor at the
    /// point it was grabbed (requirement 7); half the header's size when the
    /// press cannot be located inside it, which places the preview centred on
    /// the cursor instead.
    pub grab_offset: Vec2,
    /// Outer size of the header being carried, in points.
    ///
    /// Carried in the payload because the DRIVER, not the widget, decides when
    /// the gesture has torn out of the window, and the tear feedback has to be
    /// drawn around the very rect the widget is painting under the cursor
    /// (`grab_offset` gives its origin, this gives its extent).
    pub header_size: Vec2,
}

/// A panel move in flight, owned by `PanelDockState`.
///
/// The gesture is driven from the pointer position rather than from the widget's
/// frame-delayed drag delta: the panel's stored `pos` is recomputed at the start
/// of every frame as `panel_origin + (pointer - grab_pointer)`, so the panel is
/// laid out at the cursor in the SAME frame the cursor moved.
#[derive(Clone, Debug, PartialEq)]
pub struct DragSession {
    /// Program tab whose layout owns the dragged panel. A gesture never survives
    /// a switch to another program tab — the panel it addresses is not on screen
    /// any more.
    pub layout_key: String,
    /// Window the gesture belongs to. Every window of the dock advances the
    /// session once per frame, and only its owner may act on it: the pointer and
    /// the coordinates it is driven from are per-viewport.
    pub host: HostId,
    /// The panel being moved.
    pub panel: PanelId,
    /// Pointer position when the gesture started, in screen coordinates.
    pub grab_pointer: Pos2,
    /// The panel's position when the gesture started, relative to the host
    /// area's top-left (the same space as `PanelNode::pos`).
    pub panel_origin: Pos2,
    /// Where inside the panel the pointer grabbed it, in points from the panel's
    /// top-left.
    ///
    /// Carried because the gesture can END in another of our windows, which never
    /// laid this panel out and therefore knows neither where it was grabbed nor
    /// how big it is. Both are needed there: to paint the feedback under the
    /// cursor, and to place the panel so it keeps the point it was grabbed at.
    pub grab_offset: Vec2,
    /// Outer size the panel had when the gesture started, in points. Same reason
    /// as [`DragSession::grab_offset`].
    pub carried_size: Vec2,
    /// `true` while the pointer is outside the owning window with the button
    /// still held. Latched rather than acted on immediately, and cleared the
    /// moment the pointer is reportable again, so only a release out there
    /// detaches the panel into a window of its own (plan §4.8).
    pub left_window: bool,
}

/// What a snap candidate attaches to. Also its priority on a tie: a panel edge
/// is a more specific intent than the dock area's own edge.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SnapKind {
    /// Another panel's edge.
    Panel,
    /// An edge of the `CanvasView` controls rect.
    CanvasControls,
    /// An edge of the dock area.
    ViewportEdge,
}

/// One prospective docking target of a dragged panel.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct SnapCandidate {
    /// The anchor that would be stored if the panel were released now — before
    /// the sibling rule of [`resolve_slot`] is applied to it.
    pub anchor: PanelAnchor,
    /// Segment, in screen coordinates, along the prospective shared edge. This
    /// is what the preview line is painted along.
    pub line: [Pos2; 2],
    /// Distance, in points, between the dragged edge and the edge it would dock
    /// to. Always `<= SNAP_DISTANCE`.
    pub distance: f32,
}

/// Everything a dragged panel may snap to this frame.
#[derive(Copy, Clone, Debug)]
pub struct SnapTargets<'a> {
    /// The dock area; its four sides are `PanelAnchor::ViewportEdge` targets.
    pub area: Rect,
    /// Other panels of the same host with their solved rects, already filtered
    /// by [`panel_snap_candidates`].
    pub panels: &'a [(PanelId, Rect)],
    /// The `CanvasView` controls rect, when it exists this frame.
    pub canvas_controls: Option<Rect>,
}

/// Panels a dragged panel may legally dock to, with the rects they were solved
/// at this frame.
///
/// Excludes the dragged panel itself and everything hanging off it: anchoring a
/// panel to one of its own dependants closes a cycle, which
/// [`DockLayout::set_anchor`] rejects — offering it as a snap target would show
/// the user a preview line that does nothing on release.
#[must_use]
pub fn panel_snap_candidates(
    layout: &DockLayout,
    dragged: PanelId,
    solved: impl Iterator<Item = (PanelId, Rect)>,
) -> Vec<(PanelId, Rect)> {
    solved
        .filter(|(id, _)| *id != dragged && !layout.is_anchored_to(*id, dragged))
        .collect()
}

/// Finds the nearest edge `dragged` could dock to, within [`SNAP_DISTANCE`].
///
/// Candidates are, in priority order on an equal distance: another panel's edge,
/// an edge of the canvas-controls rect, an edge of the dock area. A panel-to-
/// panel or panel-to-controls candidate additionally requires the two rects to
/// OVERLAP along the shared side — docking "below" a panel that stands far to
/// the left is not what the user is doing.
///
/// Returns `None` when nothing is close enough, which the caller must treat as
/// "the panel stays free-floating where it was dropped".
#[must_use]
pub fn find_snap(dragged: Rect, targets: SnapTargets<'_>) -> Option<SnapCandidate> {
    let mut best: Option<(f32, SnapKind, u32, SnapCandidate)> = None;
    let mut consider = |distance: f32, kind: SnapKind, tie: u32, candidate: SnapCandidate| {
        if !distance.is_finite() || distance > SNAP_DISTANCE {
            return;
        }
        let better = match &best {
            Some((best_distance, best_kind, best_tie, _)) => {
                (distance, kind, tie) < (*best_distance, *best_kind, *best_tie)
            }
            None => true,
        };
        if better {
            best = Some((distance, kind, tie, candidate));
        }
    };

    for (id, rect) in targets.panels {
        for edge in EDGES {
            if let Some((distance, align, line)) = outside_candidate(dragged, *rect, edge) {
                consider(distance, SnapKind::Panel, id.get(), SnapCandidate {
                    anchor: PanelAnchor::Panel {
                        target: *id,
                        edge,
                        align,
                    },
                    line,
                    distance,
                });
            }
        }
    }

    if let Some(controls) = targets.canvas_controls {
        for edge in EDGES {
            if let Some((distance, along, line)) = outside_candidate(dragged, controls, edge) {
                consider(distance, SnapKind::CanvasControls, 0, SnapCandidate {
                    anchor: PanelAnchor::CanvasControls { edge, along },
                    line,
                    distance,
                });
            }
        }
    }

    for edge in EDGES {
        let (distance, along, line) = inside_candidate(dragged, targets.area, edge);
        consider(distance, SnapKind::ViewportEdge, 0, SnapCandidate {
            anchor: PanelAnchor::ViewportEdge { edge, along },
            line,
            distance,
        });
    }

    best.map(|(_, _, _, candidate)| candidate)
}

/// The four sides, in a fixed order so the snap search is deterministic.
const EDGES: [DockEdge; 4] = [
    DockEdge::Top,
    DockEdge::Bottom,
    DockEdge::Left,
    DockEdge::Right,
];

/// Evaluates "`dragged` sits outside `target`, next to `edge`".
///
/// Returns the distance between the two edges, the alignment fraction along the
/// shared side, and the segment the preview line is painted along — or `None`
/// when the two rects do not overlap along that side at all.
fn outside_candidate(
    dragged: Rect,
    target: Rect,
    edge: DockEdge,
) -> Option<(f32, f32, [Pos2; 2])> {
    if edge.is_vertical() {
        let low = dragged.left().max(target.left());
        let high = dragged.right().min(target.right());
        if high <= low {
            return None;
        }
        let (distance, y) = match edge {
            DockEdge::Top => (
                (dragged.bottom() - (target.top() - DOCK_GAP)).abs(),
                target.top() - DOCK_GAP * 0.5,
            ),
            DockEdge::Bottom => (
                (dragged.top() - (target.bottom() + DOCK_GAP)).abs(),
                target.bottom() + DOCK_GAP * 0.5,
            ),
            DockEdge::Left | DockEdge::Right => return None,
        };
        let align = fraction(dragged.left() - target.left(), target.width() - dragged.width());
        Some((distance, align, [Pos2::new(low, y), Pos2::new(high, y)]))
    } else {
        let low = dragged.top().max(target.top());
        let high = dragged.bottom().min(target.bottom());
        if high <= low {
            return None;
        }
        let (distance, x) = match edge {
            DockEdge::Left => (
                (dragged.right() - (target.left() - DOCK_GAP)).abs(),
                target.left() - DOCK_GAP * 0.5,
            ),
            DockEdge::Right => (
                (dragged.left() - (target.right() + DOCK_GAP)).abs(),
                target.right() + DOCK_GAP * 0.5,
            ),
            DockEdge::Top | DockEdge::Bottom => return None,
        };
        let align = fraction(dragged.top() - target.top(), target.height() - dragged.height());
        Some((distance, align, [Pos2::new(x, low), Pos2::new(x, high)]))
    }
}

/// Evaluates "`dragged` sits inside `area`, flush with `edge`".
///
/// Unlike [`outside_candidate`] this never fails: every panel is inside the dock
/// area, so all four sides are always meaningful targets.
fn inside_candidate(dragged: Rect, area: Rect, edge: DockEdge) -> (f32, f32, [Pos2; 2]) {
    match edge {
        DockEdge::Top | DockEdge::Bottom => {
            let y = match edge {
                DockEdge::Top => area.top() + DOCK_GAP * 0.5,
                _ => area.bottom() - DOCK_GAP * 0.5,
            };
            let distance = match edge {
                DockEdge::Top => (dragged.top() - (area.top() + DOCK_GAP)).abs(),
                _ => (dragged.bottom() - (area.bottom() - DOCK_GAP)).abs(),
            };
            let along = fraction(dragged.left() - area.left(), area.width() - dragged.width());
            let low = dragged.left().max(area.left());
            let high = dragged.right().min(area.right());
            (distance, along, [Pos2::new(low, y), Pos2::new(high, y)])
        }
        DockEdge::Left | DockEdge::Right => {
            let x = match edge {
                DockEdge::Left => area.left() + DOCK_GAP * 0.5,
                _ => area.right() - DOCK_GAP * 0.5,
            };
            let distance = match edge {
                DockEdge::Left => (dragged.left() - (area.left() + DOCK_GAP)).abs(),
                _ => (dragged.right() - (area.right() - DOCK_GAP)).abs(),
            };
            let along = fraction(dragged.top() - area.top(), area.height() - dragged.height());
            let low = dragged.top().max(area.top());
            let high = dragged.bottom().min(area.bottom());
            (distance, along, [Pos2::new(x, low), Pos2::new(x, high)])
        }
    }
}

/// Position along a side as a fraction of the free travel, clamped to
/// `0.0..=1.0`. A side with no free travel (or a non-finite input) reports
/// `0.0`, which the solver reads as "flush with the start of the side".
fn fraction(offset: f32, travel: f32) -> f32 {
    if !offset.is_finite() || !travel.is_finite() || travel <= ALIGN_EPSILON {
        return 0.0;
    }
    (offset / travel).clamp(0.0, 1.0)
}

/// Applies the SIBLING RULE to an anchor the snap search produced.
///
/// The defect this closes: two panels with the same anchor land on exactly the
/// same rect, the second one covering the first completely, and the buried panel
/// can no longer be reached — not even to drag it out again. The rule is
/// therefore *queueing*: while the slot an anchor points at is already occupied
/// by a panel, the dragged panel is re-anchored to that panel instead, on the
/// side the queue grows along ([`queue_edge`]). Docking "to the right edge of the
/// area" where a panel already stands means "under the panel already standing
/// there", and docking under a panel that already carries one means "under that
/// one".
///
/// Occupancy is decided GEOMETRICALLY — the rect the anchor would produce
/// against the rects the other panels were solved at — rather than by comparing
/// anchors, because two different anchors can perfectly well name the same spot.
/// The dragged panel and everything hanging off it are never treated as
/// occupants: re-anchoring to a dependant would close a cycle.
///
/// The walk is bounded by the panel count and never revisits an occupant, so it
/// terminates on any layout; the anchor it returns is still only a *proposal*,
/// and [`DockLayout::set_anchor`] remains the authority that may refuse it.
#[must_use]
pub fn resolve_slot(
    layout: &DockLayout,
    dragged: PanelId,
    dragged_size: Vec2,
    anchor: PanelAnchor,
    rects: &BTreeMap<PanelId, Rect>,
    area: Rect,
    canvas_controls: Option<Rect>,
) -> PanelAnchor {
    let mut anchor = anchor;
    let mut visited: Vec<PanelId> = Vec::new();
    for _ in 0..layout.panels().len() {
        let Some(rect) = prospective_rect(anchor, dragged_size, rects, area, canvas_controls)
        else {
            break;
        };
        let occupant = rects
            .iter()
            .filter(|(id, _)| {
                **id != dragged && !layout.is_anchored_to(**id, dragged) && !visited.contains(id)
            })
            .filter_map(|(id, other)| {
                let overlap = rect.intersect(*other);
                (overlap.width() > SLOT_OVERLAP_EPSILON && overlap.height() > SLOT_OVERLAP_EPSILON)
                    .then_some((*id, overlap.width() * overlap.height()))
            })
            // The largest overlap is the panel actually in the way; the id keeps
            // the choice deterministic when two overlaps are identical.
            .max_by(|left, right| {
                left.1
                    .partial_cmp(&right.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(right.0.cmp(&left.0))
            });
        let Some((occupant, _)) = occupant else {
            break;
        };
        visited.push(occupant);
        anchor = PanelAnchor::Panel {
            target: occupant,
            edge: queue_edge(anchor),
            // Flush with the panel it queues behind: a column of docked panels
            // reads as a column only if its members share a side.
            align: 0.0,
        };
    }
    anchor
}

/// Side along which a queue of panels docked into one slot grows.
///
/// For an anchor that places a panel OUTSIDE a target the queue simply continues
/// in the same direction (below a panel docked below). For a `ViewportEdge` the
/// edge points at the area's side, so the queue has to grow along it instead:
/// down a left/right column, right along a top/bottom row.
fn queue_edge(anchor: PanelAnchor) -> DockEdge {
    match anchor {
        PanelAnchor::Panel { edge, .. } | PanelAnchor::CanvasControls { edge, .. } => edge,
        PanelAnchor::ViewportEdge { edge, .. } => {
            if edge.is_horizontal() {
                DockEdge::Bottom
            } else {
                DockEdge::Right
            }
        }
        PanelAnchor::Free => DockEdge::Bottom,
    }
}

/// Rect a panel of `size` would occupy under `anchor`, using this frame's solved
/// rects for the anchor's target. `None` for a free panel and for an anchor
/// whose target is not on screen this frame.
fn prospective_rect(
    anchor: PanelAnchor,
    size: Vec2,
    rects: &BTreeMap<PanelId, Rect>,
    area: Rect,
    canvas_controls: Option<Rect>,
) -> Option<Rect> {
    match anchor {
        PanelAnchor::Free => None,
        PanelAnchor::Panel {
            target,
            edge,
            align,
        } => Some(place_outside(*rects.get(&target)?, edge, align, size)),
        PanelAnchor::CanvasControls { edge, along } => {
            Some(place_outside(canvas_controls?, edge, along, size))
        }
        PanelAnchor::ViewportEdge { edge, along } => Some(place_inside(area, edge, along, size)),
    }
}

/// Index a tab dropped at `pointer_x` takes in a header strip whose existing
/// headers have the given horizontal centres, in strip order.
///
/// A drop left of a header's centre lands before it, right of it after it, so
/// the insertion point follows the cursor without a dead zone.
#[must_use]
pub fn insertion_index(header_centers: &[f32], pointer_x: f32) -> usize {
    header_centers
        .iter()
        .filter(|center| center.is_finite() && **center < pointer_x)
        .count()
}

/// Decides whether a tab released at `pos` landed on BARE dock area, which is
/// what creates a new panel (requirement 8).
///
/// Returns the drop point when it did. A release outside the area, or over a
/// panel that did not take the tab itself, returns `None` and cancels the move:
/// dropping a tab onto a panel's body would otherwise bury a brand-new panel
/// under the panel it was dropped on, which is the same defect the sibling rule
/// exists to prevent.
#[must_use]
pub fn empty_space_drop(area: Rect, occupied: &[Rect], pos: Pos2) -> Option<Pos2> {
    if !area.contains(pos) {
        return None;
    }
    if occupied.iter().any(|rect| rect.contains(pos)) {
        return None;
    }
    Some(pos)
}

/// Paints the docking preview line of `candidate` — a two-point blue segment
/// along the prospective shared edge (plan §4.8a).
///
/// Painted through a `Painter` on `Order::Tooltip`, which registers no widget
/// and therefore cannot steal input from the panels underneath
/// (`egui-docs/06-overlays.md` §3). The colour is the style's selection stroke,
/// never a literal, so it follows the theme.
pub fn paint_snap_preview(ctx: &egui::Context, candidate: &SnapCandidate) {
    // `Context::style()` does not exist in 0.35: styles are per theme, and the
    // active one is `style_of(theme())` (`egui-0.35.0/src/context.rs:2090`,
    // `:2153`) — the same style a `Ui` resolves `visuals()` from.
    let color = ctx.style_of(ctx.theme()).visuals.selection.stroke.color;
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("ms_panel_dock_snap_preview"),
    ));
    painter.line_segment(candidate.line, Stroke::new(SNAP_LINE_WIDTH, color));
}

/// Paints the insertion marker of a tab drop the receiving window could not sense
/// itself: a vertical line at `x`, spanning `strip`.
///
/// The same mark `CollapsiblePanel` paints while a tab is dragged over its own
/// header strip, in the same selection colour — but drawn by the DRIVER, because
/// a drag that crossed a window border leaves the receiving window without a
/// pointer to hover with (`cross_window.rs`). Painted on `Order::Tooltip`, so it
/// registers no widget and cannot steal the drop it announces.
pub fn paint_insertion_marker(ctx: &egui::Context, strip: Rect, x: f32) {
    if !strip.is_finite() || !x.is_finite() {
        return;
    }
    let color = ctx.style_of(ctx.theme()).visuals.selection.stroke.color;
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("ms_panel_dock_cross_window_marker"),
    ));
    painter.line_segment(
        [
            Pos2::new(x, strip.top()),
            Pos2::new(x, strip.bottom()),
        ],
        Stroke::new(SNAP_LINE_WIDTH, color),
    );
}

/// Paints the TEAR-OUT preview: a dashed outline of what would fly into a window
/// of its own if the gesture were released right now
/// (`window::DragTension::TornOff`).
///
/// This is the only feedback the user gets for a threshold they cannot see, so it
/// speaks the same visual language as the docking preview — the style's selection
/// colour, the same stroke width — and differs from it in FORM: a solid line
/// along an edge means "this docks here", a dashed contour means "this leaves the
/// window". Dashes are also what the tutorial overlay uses for "a region, not a
/// widget" (`egui-docs/06-overlays.md` §4).
///
/// Painted through a `Painter` on `Order::Tooltip`, so it registers no widget and
/// cannot steal the release it is announcing.
pub fn paint_detach_preview(ctx: &egui::Context, outline: Rect) {
    if !outline.is_finite() || !outline.is_positive() {
        return;
    }
    let color = ctx.style_of(ctx.theme()).visuals.selection.stroke.color;
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("ms_panel_dock_detach_preview"),
    ));
    // `dashed_line` only connects consecutive points, so the first corner is
    // repeated to close the rectangle (`epaint-0.35.0/src/shapes/shape.rs:170`).
    let corners = [
        outline.left_top(),
        outline.right_top(),
        outline.right_bottom(),
        outline.left_bottom(),
        outline.left_top(),
    ];
    painter.extend(egui::Shape::dashed_line(
        &corners,
        Stroke::new(SNAP_LINE_WIDTH, color),
        DETACH_PREVIEW_DASH,
        DETACH_PREVIEW_GAP,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::panel_dock::model::{HostId, PanelNode};

    const TAB_A: TabId = TabId::new("test.a");
    const TAB_B: TabId = TabId::new("test.b");
    const TAB_C: TabId = TabId::new("test.c");

    const AREA: Rect = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1000.0, 800.0));

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h))
    }

    fn node(id: u32, tab: TabId) -> PanelNode {
        PanelNode::new(PanelId::new(id), HostId::MainWindow, vec![tab])
            .expect("test panel must be constructible")
    }

    #[test]
    fn a_panel_dropped_below_another_one_snaps_to_its_bottom_edge() {
        let target = rect(100.0, 100.0, 300.0, 200.0);
        // Six points above the exact docking position: inside SNAP_DISTANCE.
        let dragged = rect(100.0, 100.0 + 200.0 + DOCK_GAP - 6.0, 300.0, 150.0);
        let panels = [(PanelId::new(0), target)];
        let candidate = find_snap(dragged, SnapTargets {
            area: AREA,
            panels: &panels,
            canvas_controls: None,
        })
        .expect("a candidate within the snap distance");
        assert_eq!(candidate.anchor, PanelAnchor::Panel {
            target: PanelId::new(0),
            edge: DockEdge::Bottom,
            align: 0.0,
        });
        assert!((candidate.distance - 6.0).abs() < 0.01);
        // The preview line runs along the shared side, halfway inside the gap.
        assert!((candidate.line[0].y - (target.bottom() + DOCK_GAP * 0.5)).abs() < 0.01);
        assert!((candidate.line[0].x - 100.0).abs() < 0.01);
        assert!((candidate.line[1].x - 400.0).abs() < 0.01);
    }

    #[test]
    fn nothing_snaps_beyond_the_snap_distance() {
        let target = rect(100.0, 100.0, 300.0, 200.0);
        // Far from every panel edge AND from every area edge.
        let dragged = rect(500.0, 400.0, 300.0, 150.0);
        let panels = [(PanelId::new(0), target)];
        assert_eq!(
            find_snap(dragged, SnapTargets {
                area: AREA,
                panels: &panels,
                canvas_controls: None,
            }),
            None
        );
    }

    #[test]
    fn a_panel_that_does_not_share_the_side_is_not_a_candidate() {
        // Vertically in the right place to dock below the target, but so far to
        // the right that the two rects share no horizontal extent at all.
        let target = rect(100.0, 100.0, 300.0, 200.0);
        let dragged = rect(600.0, 100.0 + 200.0 + DOCK_GAP, 300.0, 150.0);
        let panels = [(PanelId::new(0), target)];
        let candidate = find_snap(dragged, SnapTargets {
            area: AREA,
            panels: &panels,
            canvas_controls: None,
        });
        assert!(candidate.is_none_or(|found| found.anchor.target_panel().is_none()));
    }

    #[test]
    fn the_area_edge_is_a_target_and_loses_a_tie_to_a_panel() {
        // The dragged panel is exactly at the right edge inset AND exactly one
        // gap to the right of the other panel: both candidates are at distance
        // 0, and the panel must win.
        let dragged = rect(
            AREA.right() - DOCK_GAP - 300.0,
            200.0,
            300.0,
            150.0,
        );
        let target = rect(dragged.left() - DOCK_GAP - 200.0, 200.0, 200.0, 150.0);
        let panels = [(PanelId::new(3), target)];
        let candidate = find_snap(dragged, SnapTargets {
            area: AREA,
            panels: &panels,
            canvas_controls: None,
        })
        .expect("both candidates are exact");
        assert_eq!(candidate.anchor, PanelAnchor::Panel {
            target: PanelId::new(3),
            edge: DockEdge::Right,
            align: 0.0,
        });

        // Without the panel, the same position snaps to the area's right edge.
        let candidate = find_snap(dragged, SnapTargets {
            area: AREA,
            panels: &[],
            canvas_controls: None,
        })
        .expect("the area edge is a candidate on its own");
        assert_eq!(candidate.anchor, PanelAnchor::ViewportEdge {
            edge: DockEdge::Right,
            along: fraction(200.0 - AREA.top(), AREA.height() - 150.0),
        });
    }

    #[test]
    fn the_canvas_controls_rect_is_a_target_and_outranks_the_area_edge() {
        let controls = rect(20.0, 20.0, 200.0, 40.0);
        let dragged = rect(20.0, 20.0 + 40.0 + DOCK_GAP, 200.0, 150.0);
        let candidate = find_snap(dragged, SnapTargets {
            area: AREA,
            panels: &[],
            canvas_controls: Some(controls),
        })
        .expect("the controls rect is a candidate");
        assert_eq!(candidate.anchor, PanelAnchor::CanvasControls {
            edge: DockEdge::Bottom,
            along: 0.0,
        });
    }

    #[test]
    fn the_alignment_fraction_follows_where_the_panel_was_dropped() {
        let target = rect(100.0, 100.0, 400.0, 200.0);
        // Flush with the target's right side: align == 1.0.
        let dragged = rect(400.0, 100.0 + 200.0 + DOCK_GAP, 100.0, 150.0);
        let panels = [(PanelId::new(0), target)];
        let candidate = find_snap(dragged, SnapTargets {
            area: AREA,
            panels: &panels,
            canvas_controls: None,
        })
        .expect("a candidate");
        match candidate.anchor {
            PanelAnchor::Panel { align, .. } => assert!((align - 1.0).abs() < 0.01),
            other => panic!("expected a panel anchor, got {other:?}"),
        }
    }

    #[test]
    fn a_second_panel_docked_to_one_side_queues_behind_the_first() {
        // THE SIBLING RULE. Panel 1 already hangs under panel 0; dropping panel 2
        // into the same slot must stack it under panel 1 instead of burying it.
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        let mut first = node(1, TAB_B);
        first.anchor = PanelAnchor::Panel {
            target: PanelId::new(0),
            edge: DockEdge::Bottom,
            align: 0.0,
        };
        layout.insert_panel(first).expect("insert 1");
        layout.insert_panel(node(2, TAB_C)).expect("insert 2");

        let root = rect(100.0, 100.0, 300.0, 200.0);
        let sibling = rect(100.0, root.bottom() + DOCK_GAP, 300.0, 150.0);
        let rects: BTreeMap<PanelId, Rect> =
            [(PanelId::new(0), root), (PanelId::new(1), sibling)]
                .into_iter()
                .collect();

        let resolved = resolve_slot(
            &layout,
            PanelId::new(2),
            Vec2::new(300.0, 150.0),
            PanelAnchor::Panel {
                target: PanelId::new(0),
                edge: DockEdge::Bottom,
                align: 0.0,
            },
            &rects,
            AREA,
            None,
        );
        assert_eq!(resolved, PanelAnchor::Panel {
            target: PanelId::new(1),
            edge: DockEdge::Bottom,
            align: 0.0,
        });
    }

    #[test]
    fn an_empty_slot_is_left_exactly_as_the_snap_found_it() {
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        layout.insert_panel(node(1, TAB_B)).expect("insert 1");
        let root = rect(100.0, 100.0, 300.0, 200.0);
        let rects: BTreeMap<PanelId, Rect> = [(PanelId::new(0), root)].into_iter().collect();
        let anchor = PanelAnchor::Panel {
            target: PanelId::new(0),
            edge: DockEdge::Bottom,
            align: 0.25,
        };
        assert_eq!(
            resolve_slot(
                &layout,
                PanelId::new(1),
                Vec2::new(300.0, 150.0),
                anchor,
                &rects,
                AREA,
                None,
            ),
            anchor
        );
    }

    #[test]
    fn a_taken_area_edge_queues_the_new_panel_below_the_panel_standing_there() {
        // A `ViewportEdge` slot cannot queue along its own edge — that would push
        // the panel out of the area — so the queue grows down the column.
        let mut layout = DockLayout::new();
        let mut first = node(0, TAB_A);
        first.anchor = PanelAnchor::ViewportEdge {
            edge: DockEdge::Right,
            along: 0.0,
        };
        layout.insert_panel(first).expect("insert 0");
        layout.insert_panel(node(1, TAB_B)).expect("insert 1");

        let occupied = place_inside(AREA, DockEdge::Right, 0.0, Vec2::new(300.0, 200.0));
        let rects: BTreeMap<PanelId, Rect> = [(PanelId::new(0), occupied)].into_iter().collect();
        let resolved = resolve_slot(
            &layout,
            PanelId::new(1),
            Vec2::new(300.0, 150.0),
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Right,
                along: 0.0,
            },
            &rects,
            AREA,
            None,
        );
        assert_eq!(resolved, PanelAnchor::Panel {
            target: PanelId::new(0),
            edge: DockEdge::Bottom,
            align: 0.0,
        });
    }

    #[test]
    fn the_sibling_rule_never_offers_a_dependant_of_the_dragged_panel() {
        // Panel 1 hangs off panel 2, which is the one being dragged. Anchoring 2
        // to 1 would close a cycle, so 1 must not be treated as an occupant even
        // though it sits exactly in the slot.
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        layout.insert_panel(node(2, TAB_C)).expect("insert 2");
        let mut child = node(1, TAB_B);
        child.anchor = PanelAnchor::Panel {
            target: PanelId::new(2),
            edge: DockEdge::Bottom,
            align: 0.0,
        };
        layout.insert_panel(child).expect("insert 1");

        let root = rect(100.0, 100.0, 300.0, 200.0);
        let child_rect = rect(100.0, root.bottom() + DOCK_GAP, 300.0, 150.0);
        let rects: BTreeMap<PanelId, Rect> =
            [(PanelId::new(0), root), (PanelId::new(1), child_rect)]
                .into_iter()
                .collect();
        let anchor = PanelAnchor::Panel {
            target: PanelId::new(0),
            edge: DockEdge::Bottom,
            align: 0.0,
        };
        assert_eq!(
            resolve_slot(
                &layout,
                PanelId::new(2),
                Vec2::new(300.0, 150.0),
                anchor,
                &rects,
                AREA,
                None,
            ),
            anchor
        );
    }

    #[test]
    fn panel_snap_candidates_drop_the_dragged_panel_and_its_dependants() {
        let mut layout = DockLayout::new();
        layout.insert_panel(node(0, TAB_A)).expect("insert 0");
        layout.insert_panel(node(1, TAB_B)).expect("insert 1");
        let mut child = node(2, TAB_C);
        child.anchor = PanelAnchor::Panel {
            target: PanelId::new(1),
            edge: DockEdge::Bottom,
            align: 0.0,
        };
        layout.insert_panel(child).expect("insert 2");

        let solved = [
            (PanelId::new(0), rect(0.0, 0.0, 10.0, 10.0)),
            (PanelId::new(1), rect(20.0, 0.0, 10.0, 10.0)),
            (PanelId::new(2), rect(40.0, 0.0, 10.0, 10.0)),
        ];
        let candidates = panel_snap_candidates(&layout, PanelId::new(1), solved.into_iter());
        assert_eq!(
            candidates.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![PanelId::new(0)]
        );
    }

    #[test]
    fn the_insertion_index_follows_the_pointer_across_the_header_centres() {
        let centers = [100.0, 200.0, 300.0];
        assert_eq!(insertion_index(&centers, 40.0), 0);
        assert_eq!(insertion_index(&centers, 150.0), 1);
        assert_eq!(insertion_index(&centers, 250.0), 2);
        assert_eq!(insertion_index(&centers, 900.0), 3);
        assert_eq!(insertion_index(&[], 900.0), 0);
    }

    #[test]
    fn only_bare_dock_area_accepts_a_new_panel() {
        let panels = [rect(100.0, 100.0, 200.0, 200.0)];
        assert_eq!(
            empty_space_drop(AREA, &panels, Pos2::new(600.0, 400.0)),
            Some(Pos2::new(600.0, 400.0))
        );
        // Over an existing panel: cancelled, so nothing is buried.
        assert_eq!(empty_space_drop(AREA, &panels, Pos2::new(150.0, 150.0)), None);
        // Outside the dock area: cancelled (the sub-window phase claims this case).
        assert_eq!(
            empty_space_drop(AREA, &panels, Pos2::new(-40.0, 400.0)),
            None
        );
    }

    #[test]
    fn a_degenerate_side_reports_a_zero_fraction_instead_of_a_division_by_zero() {
        assert_eq!(fraction(10.0, 0.0), 0.0);
        assert_eq!(fraction(f32::NAN, 100.0), 0.0);
        assert_eq!(fraction(-50.0, 100.0), 0.0);
        assert_eq!(fraction(500.0, 100.0), 1.0);
    }
}
