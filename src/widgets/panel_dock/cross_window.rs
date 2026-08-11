/*
File: src/widgets/panel_dock/cross_window.rs

Purpose:
Addressing a reorganisation gesture that crossed a window border (plan §4.8,
§4.9). A drag gesture does not travel between egui viewports — `Memory::{interactions,
focus,areas}` are `ViewportIdMap`s (`egui-0.35.0/src/memory/mod.rs:103-118`) and a
held mouse button keeps an implicit pointer grab on the window the press started
in — so the window under the cursor cannot answer "is the drop mine?" by
hit-testing. This module answers it from GEOMETRY instead: every window reports
where it is on the monitor, the release point is lifted into that shared frame,
and the window it lands in is found by containment.

Main responsibilities:
- describe one window of the dock in the shared monitor frame (`WindowGeometry`)
  and convert points between that frame and a window's own screen coordinates;
- decide which of our windows a point belongs to (`window_at`), including the
  overlap rule;
- decide whether a finished drag landed in one of our windows or on the bare
  desktop (`address_drop`, `DropAddress`);
- decide what a tab (`tab_landing`, `TabLanding`) or a whole panel
  (`panel_landing`) released inside a window actually lands on.

Key structures:
- `WindowGeometry`: one window's rect in the shared frame plus its scale.
- `PanelDropTarget`: what one drawn panel offers a drop.
- `DropAddress`, `TabLanding`.

Key functions:
- `window_at`, `address_drop`, `tab_landing`, `panel_landing`.

Notes:
THE SHARED FRAME IS PHYSICAL PIXELS. `ViewportInfo::inner_rect` is the window's
content rect in monitor space divided by THAT window's `pixels_per_point`
(`egui-winit-0.35.0/src/lib.rs:1329-1333`, `:51-55`), so two windows on monitors
with different scale factors express monitor space in two different units.
Multiplying each window's rect by its own scale puts them back into the one frame
the window manager actually uses.

WAYLAND HAS NO SUCH FRAME. `ViewportInfo::inner_rect` and `outer_rect` are always
`None` there (`egui-0.35.0/src/data/input/viewport_info.rs:52-66`), and winit's
`outer_position()` returns `Err`, so no global coordinate exists at all — not in
egui, not below it. A caller that gets no geometry must degrade honestly: this
module simply produces no `WindowGeometry` and every decision falls back to the
window-local tension model of `window.rs`. Nothing here invents a coordinate.

Everything in this file is a pure function of plain geometry: no `egui::Context`,
no viewport ids, no logging — which is what makes the whole addressing model
testable without a window. Building the geometry list from a live `Context` is
`mod.rs::window_geometries`.
*/

use std::cmp::Ordering;

use egui::{Pos2, Rect, Vec2};

use super::drag::insertion_index;
use super::model::{HostId, PanelId};

/// Geometry of one window of the dock, in the shared monitor frame.
///
/// `inner_rect` is `ViewportInfo::inner_rect` — the window's CONTENT rect in
/// monitor space, expressed in that window's own points — and `pixels_per_point`
/// is what turns those points into the physical pixels every window shares
/// (`zoom_factor * native_pixels_per_point`).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct WindowGeometry {
    /// Which window this is.
    pub host: HostId,
    /// Content rect in monitor space, in this window's points.
    pub inner_rect: Rect,
    /// Points → physical pixels of this window. Always finite and `> 0.0`.
    pub pixels_per_point: f32,
}

impl WindowGeometry {
    /// Describes one window, or `None` when the platform's report cannot address
    /// anything.
    ///
    /// A rect that is not finite or has no area is refused outright: it would
    /// either match nothing or, inverted, match everything. A scale that is not
    /// a usable factor is replaced by `1.0`, which is exactly right whenever the
    /// session has one scale for every window and is the only assumption
    /// available when it does not.
    #[must_use]
    pub fn new(host: HostId, inner_rect: Rect, pixels_per_point: f32) -> Option<Self> {
        if !inner_rect.is_finite() || !inner_rect.is_positive() {
            return None;
        }
        let pixels_per_point = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
            pixels_per_point
        } else {
            1.0
        };
        Some(Self {
            host,
            inner_rect,
            pixels_per_point,
        })
    }

    /// This window's content rect in the shared frame, in physical pixels.
    #[must_use]
    pub fn pixel_rect(&self) -> Rect {
        let scale = self.pixels_per_point;
        Rect::from_min_max(
            Pos2::new(self.inner_rect.left() * scale, self.inner_rect.top() * scale),
            Pos2::new(
                self.inner_rect.right() * scale,
                self.inner_rect.bottom() * scale,
            ),
        )
    }

    /// Lifts a point from this window's screen coordinates into the shared frame.
    ///
    /// A window's screen coordinates start at its content's top-left, so the
    /// monitor position of `local` is `inner_rect.min + local`, and the shared
    /// frame is that scaled by this window's `pixels_per_point`.
    #[must_use]
    pub fn to_global(&self, local: Pos2) -> Option<Pos2> {
        if !local.x.is_finite() || !local.y.is_finite() {
            return None;
        }
        let scale = self.pixels_per_point;
        Some(Pos2::new(
            (self.inner_rect.left() + local.x) * scale,
            (self.inner_rect.top() + local.y) * scale,
        ))
    }

    /// Brings a point of the shared frame back into this window's screen
    /// coordinates. The exact inverse of [`WindowGeometry::to_global`].
    #[must_use]
    pub fn to_local(&self, global: Pos2) -> Option<Pos2> {
        let points = self.to_viewport_points(global)?;
        Some(Pos2::new(
            points.x - self.inner_rect.left(),
            points.y - self.inner_rect.top(),
        ))
    }

    /// Expresses a point of the shared frame in the MONITOR-space points a
    /// `ViewportBuilder::with_position` consumes, using this window's scale.
    ///
    /// The scale of the window that does not exist yet cannot be known, so a new
    /// window is placed with the scale of the window the gesture came from. That
    /// is exact whenever both sit on monitors of the same scale, and it is the
    /// only honest estimate when they do not.
    #[must_use]
    pub fn to_viewport_points(&self, global: Pos2) -> Option<Pos2> {
        if !global.x.is_finite() || !global.y.is_finite() {
            return None;
        }
        let scale = self.pixels_per_point;
        Some(Pos2::new(global.x / scale, global.y / scale))
    }
}

/// The window of `windows` a point of the shared frame belongs to.
///
/// **The overlap rule.** Windows overlap on screen and neither egui nor winit
/// exposes their stacking order, so the one that owns a point has to be chosen
/// by a rule. It is the SMALLEST window containing it: a window that covers
/// another one entirely cannot be the one on top at that point without making the
/// covered window invisible there, and the dock's sub-windows are small tool
/// windows that float over the large main window. On an exact tie the
/// most recently created sub-window wins, and the main window loses to every
/// sub-window — the same "the smaller, later window floats above" reading, made
/// deterministic.
///
/// `None` means the point is on the bare desktop, or on a window that is not
/// ours.
#[must_use]
pub fn window_at(windows: &[WindowGeometry], global: Pos2) -> Option<&WindowGeometry> {
    if !global.x.is_finite() || !global.y.is_finite() {
        return None;
    }
    windows
        .iter()
        .filter(|window| window.pixel_rect().contains(global))
        .min_by(|left, right| {
            let left_area = pixel_area(left);
            let right_area = pixel_area(right);
            left_area
                .partial_cmp(&right_area)
                .unwrap_or(Ordering::Equal)
                .then_with(|| stacking_rank(right.host).cmp(&stacking_rank(left.host)))
        })
}

/// Area, in square physical pixels, of one window's content rect.
fn pixel_area(window: &WindowGeometry) -> f32 {
    let rect = window.pixel_rect();
    rect.width() * rect.height()
}

/// Tie-break rank of a host on an exact area tie: higher floats above.
///
/// A sub-window is always above the main window, and a later sub-window above an
/// earlier one, because indices are handed out in creation order.
fn stacking_rank(host: HostId) -> u64 {
    match host {
        HostId::MainWindow => 0,
        HostId::SubWindow(index) => u64::from(index) + 1,
    }
}

/// Which window a finished drag has to be applied to.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum DropAddress {
    /// It landed inside another of our windows, at `local` in that window's
    /// screen coordinates.
    Window {
        /// The window the drop belongs to.
        host: HostId,
        /// The release point in that window's screen coordinates.
        local: Pos2,
    },
    /// It landed on the bare desktop: a window of its own opens for it.
    Desktop {
        /// Prospective outer position of the new window in monitor-space points.
        /// `None` where the platform reports no window geometry (Wayland), where
        /// the compositor places the window instead.
        place_at: Option<Pos2>,
    },
}

/// Where a drag that its own window did not consume has to be applied.
///
/// `source` — the window whose pass saw the release — is deliberately EXCLUDED
/// from the containment test. A release inside the source window that got this
/// far was already refused by that window's own rules: the tension model
/// (`window.rs`) measures against the dock AREA, not the window, so a gesture
/// pulled far past the area's border and released over the program's own toolbar
/// is a tear-out the user was promised by the dashed outline. Letting the source
/// window claim itself here would take that promise back.
///
/// `global` is the release point in the shared frame; `None` (no window geometry
/// at all — Wayland) can only mean "a window of its own, wherever the compositor
/// puts it", which is exactly the behaviour the tension model already has there.
#[must_use]
pub fn address_drop(
    windows: &[WindowGeometry],
    source: HostId,
    global: Option<Pos2>,
) -> DropAddress {
    let Some(global) = global else {
        return DropAddress::Desktop { place_at: None };
    };
    if let Some(window) = window_at(windows, global)
        && window.host != source
        && let Some(local) = window.to_local(global)
    {
        return DropAddress::Window {
            host: window.host,
            local,
        };
    }
    let place_at = windows
        .iter()
        .find(|window| window.host == source)
        .and_then(|window| window.to_viewport_points(global));
    DropAddress::Desktop { place_at }
}

/// What one panel drawn this frame offers a drop landing on it.
///
/// Recorded by the driver while a gesture is in flight, so a window that never
/// saw the pointer can still be told exactly what the cursor was over.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelDropTarget {
    /// The panel.
    pub panel: PanelId,
    /// Its outer rect, in its window's screen coordinates.
    pub rect: Rect,
    /// Its header strip — the drop zone that takes a tab — in the same
    /// coordinates.
    pub header_strip: Rect,
    /// Rects of the tab captions inside that strip, in strip order.
    pub header_rects: Vec<Rect>,
}

/// What a tab released inside one of our windows lands on.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum TabLanding {
    /// A panel's header strip took it, at this index in that panel's tab order.
    HeaderStrip {
        /// The receiving panel.
        panel: PanelId,
        /// Index the tab takes in its tab order.
        index: usize,
    },
    /// Bare dock area: the tab gets a panel of its own at this position, in the
    /// receiving window's screen coordinates.
    BareArea {
        /// The release point.
        pos: Pos2,
    },
    /// A panel's body, or anywhere outside the dock area: the move is cancelled.
    ///
    /// Dropping a tab onto a panel's BODY would bury a brand-new panel under the
    /// panel it was dropped on — the same defect the sibling rule exists to
    /// prevent — and anything outside the area is not part of the dock at all.
    Cancelled,
}

/// Decides what a tab released at `local` inside a window lands on.
///
/// `area` is that window's dock area and `panels` everything it drew this frame,
/// both in the window's own screen coordinates. Header strips are tested FIRST:
/// a strip is inside its panel's rect, so testing the rects first would never let
/// a strip take anything.
#[must_use]
pub fn tab_landing(area: Rect, panels: &[PanelDropTarget], local: Pos2) -> TabLanding {
    if !local.x.is_finite() || !local.y.is_finite() {
        return TabLanding::Cancelled;
    }
    if let Some(target) = panels
        .iter()
        .find(|target| target.header_strip.contains(local))
    {
        let centers: Vec<f32> = target
            .header_rects
            .iter()
            .map(|rect| rect.center().x)
            .collect();
        return TabLanding::HeaderStrip {
            panel: target.panel,
            index: insertion_index(&centers, local.x),
        };
    }
    if !area.contains(local) || panels.iter().any(|target| target.rect.contains(local)) {
        return TabLanding::Cancelled;
    }
    TabLanding::BareArea { pos: local }
}

/// Position a whole panel released at `local` takes inside the receiving window,
/// relative to that window's area origin. `None` cancels the move.
///
/// A panel that crosses a window border simply becomes free-floating where it was
/// dropped: it does not snap to the receiving window's edges, because it never
/// followed the cursor in that window and the user never saw a docking preview
/// there. Docking it is the next gesture, inside its new window.
#[must_use]
pub fn panel_landing(area: Rect, local: Pos2, grab_offset: Vec2) -> Option<Pos2> {
    if !local.x.is_finite() || !local.y.is_finite() || !area.contains(local) {
        return None;
    }
    Some(local - area.min.to_vec2() - grab_offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h))
    }

    fn main_window() -> WindowGeometry {
        WindowGeometry::new(HostId::MainWindow, rect(0.0, 0.0, 1600.0, 900.0), 1.0)
            .expect("a sane main window")
    }

    fn sub_window(index: u32, x: f32, y: f32) -> WindowGeometry {
        WindowGeometry::new(
            HostId::SubWindow(index),
            rect(x, y, 420.0, 560.0),
            1.0,
        )
        .expect("a sane sub-window")
    }

    fn target(id: u32, rect_: Rect, strip: Rect, headers: &[Rect]) -> PanelDropTarget {
        PanelDropTarget {
            panel: PanelId::new(id),
            rect: rect_,
            header_strip: strip,
            header_rects: headers.to_vec(),
        }
    }

    #[test]
    fn a_local_point_becomes_a_monitor_point_and_back() {
        let window = sub_window(0, 300.0, 200.0);
        let global = window
            .to_global(Pos2::new(40.0, 30.0))
            .expect("a finite local point");
        assert_eq!(global, Pos2::new(340.0, 230.0));
        assert_eq!(
            window.to_local(global),
            Some(Pos2::new(40.0, 30.0))
        );
    }

    /// The shared frame is physical pixels precisely so two windows on monitors
    /// with different scale factors agree on where a point is.
    #[test]
    fn windows_of_different_scales_share_one_frame() {
        // A window on a 2x monitor: its own points say the content starts at
        // (0, 0), which is 0 physical pixels, and it is 800 points = 1600 pixels
        // wide.
        let dense =
            WindowGeometry::new(HostId::MainWindow, rect(0.0, 0.0, 800.0, 450.0), 2.0)
                .expect("a sane window");
        // A window on the 1x monitor to its right: its own points say it starts
        // at 1600, which is also 1600 physical pixels.
        let sparse =
            WindowGeometry::new(HostId::SubWindow(0), rect(1600.0, 0.0, 420.0, 560.0), 1.0)
                .expect("a sane window");
        let global = dense
            .to_global(Pos2::new(700.0, 100.0))
            .expect("a finite local point");
        // 700 points of a 2x window is 1400 physical pixels: still inside the
        // dense window, and NOT inside the sparse one that starts at 1600.
        assert_eq!(global, Pos2::new(1400.0, 200.0));
        let windows = [dense, sparse];
        assert_eq!(
            window_at(&windows, global).map(|window| window.host),
            Some(HostId::MainWindow)
        );
        // One physical pixel inside the sparse window belongs to it, even though
        // the same number in the dense window's points would not.
        assert_eq!(
            window_at(&windows, Pos2::new(1601.0, 200.0)).map(|window| window.host),
            Some(HostId::SubWindow(0))
        );
    }

    #[test]
    fn a_point_outside_every_window_belongs_to_none() {
        let windows = [main_window(), sub_window(0, 1700.0, 100.0)];
        assert!(window_at(&windows, Pos2::new(1650.0, 950.0)).is_none());
        assert!(window_at(&windows, Pos2::new(f32::NAN, 10.0)).is_none());
    }

    /// THE OVERLAP RULE: a sub-window floating over the main window claims the
    /// point, because it is the smaller of the two containing it.
    #[test]
    fn the_smallest_window_containing_the_point_claims_it() {
        let windows = [main_window(), sub_window(3, 400.0, 300.0)];
        assert_eq!(
            window_at(&windows, Pos2::new(500.0, 400.0)).map(|window| window.host),
            Some(HostId::SubWindow(3))
        );
        // Outside the sub-window, the main window is the only candidate left.
        assert_eq!(
            window_at(&windows, Pos2::new(100.0, 100.0)).map(|window| window.host),
            Some(HostId::MainWindow)
        );
    }

    /// Two sub-windows of the same size stacked on the same spot: the later one
    /// wins, deterministically.
    #[test]
    fn an_exact_tie_goes_to_the_later_window() {
        let windows = [sub_window(1, 100.0, 100.0), sub_window(4, 100.0, 100.0)];
        assert_eq!(
            window_at(&windows, Pos2::new(200.0, 200.0)).map(|window| window.host),
            Some(HostId::SubWindow(4))
        );
        let reversed = [sub_window(4, 100.0, 100.0), sub_window(1, 100.0, 100.0)];
        assert_eq!(
            window_at(&reversed, Pos2::new(200.0, 200.0)).map(|window| window.host),
            Some(HostId::SubWindow(4))
        );
    }

    #[test]
    fn a_drop_inside_another_window_is_addressed_to_it() {
        let windows = [main_window(), sub_window(0, 400.0, 300.0)];
        let global = Pos2::new(450.0, 350.0);
        assert_eq!(
            address_drop(&windows, HostId::MainWindow, Some(global)),
            DropAddress::Window {
                host: HostId::SubWindow(0),
                local: Pos2::new(50.0, 50.0),
            }
        );
    }

    /// The reverse direction is the same decision — this is the defect where a
    /// tab could not be brought back from a detached window.
    #[test]
    fn a_drop_from_a_sub_window_into_the_main_one_is_addressed_to_it() {
        let windows = [main_window(), sub_window(0, 400.0, 300.0)];
        let global = Pos2::new(120.0, 140.0);
        assert_eq!(
            address_drop(&windows, HostId::SubWindow(0), Some(global)),
            DropAddress::Window {
                host: HostId::MainWindow,
                local: Pos2::new(120.0, 140.0),
            }
        );
    }

    /// The source window never claims its own drop back: a gesture pulled past
    /// its dock area's border and released over its own toolbar is a tear-out,
    /// which is what the dashed outline promised the user.
    #[test]
    fn the_source_window_never_claims_its_own_drop() {
        let windows = [main_window()];
        assert_eq!(
            address_drop(&windows, HostId::MainWindow, Some(Pos2::new(500.0, 20.0))),
            DropAddress::Desktop {
                place_at: Some(Pos2::new(500.0, 20.0)),
            }
        );
    }

    #[test]
    fn a_drop_on_the_bare_desktop_opens_a_window_where_it_landed() {
        let windows = [main_window(), sub_window(0, 400.0, 300.0)];
        assert_eq!(
            address_drop(&windows, HostId::MainWindow, Some(Pos2::new(1700.0, 950.0))),
            DropAddress::Desktop {
                place_at: Some(Pos2::new(1700.0, 950.0)),
            }
        );
    }

    /// WAYLAND. No window reports its position, so there is no shared frame at
    /// all: the drop degrades to the window-local tension model's answer — a new
    /// window, placed by the compositor — and nothing is invented.
    #[test]
    fn without_any_window_geometry_the_drop_degrades_to_an_unplaced_window() {
        assert_eq!(
            address_drop(&[], HostId::MainWindow, None),
            DropAddress::Desktop { place_at: None }
        );
        assert_eq!(
            address_drop(&[], HostId::SubWindow(2), Some(Pos2::new(10.0, 10.0))),
            DropAddress::Desktop { place_at: None }
        );
    }

    #[test]
    fn a_window_report_that_cannot_address_anything_is_refused() {
        assert!(
            WindowGeometry::new(HostId::MainWindow, rect(0.0, 0.0, 0.0, 0.0), 1.0).is_none()
        );
        assert!(
            WindowGeometry::new(
                HostId::MainWindow,
                Rect::from_min_max(Pos2::new(f32::NAN, 0.0), Pos2::new(10.0, 10.0)),
                1.0,
            )
            .is_none()
        );
        // A scale that is not a factor is replaced rather than refused: the rect
        // still addresses a window, and one scale for every window is the only
        // assumption left.
        let window = WindowGeometry::new(HostId::MainWindow, rect(0.0, 0.0, 10.0, 10.0), 0.0)
            .expect("the rect is usable");
        assert_eq!(window.pixels_per_point, 1.0);
    }

    #[test]
    fn a_tab_dropped_on_a_header_strip_lands_at_the_hovered_index() {
        let strip = rect(100.0, 100.0, 300.0, 24.0);
        let panels = [target(
            1,
            rect(100.0, 100.0, 300.0, 200.0),
            strip,
            &[rect(120.0, 102.0, 60.0, 20.0), rect(190.0, 102.0, 60.0, 20.0)],
        )];
        assert_eq!(
            tab_landing(rect(0.0, 0.0, 1000.0, 800.0), &panels, Pos2::new(130.0, 110.0)),
            TabLanding::HeaderStrip {
                panel: PanelId::new(1),
                index: 0,
            }
        );
        assert_eq!(
            tab_landing(rect(0.0, 0.0, 1000.0, 800.0), &panels, Pos2::new(380.0, 110.0)),
            TabLanding::HeaderStrip {
                panel: PanelId::new(1),
                index: 2,
            }
        );
    }

    #[test]
    fn a_tab_dropped_on_bare_area_asks_for_a_panel_of_its_own() {
        let panels = [target(
            1,
            rect(100.0, 100.0, 300.0, 200.0),
            rect(100.0, 100.0, 300.0, 24.0),
            &[],
        )];
        assert_eq!(
            tab_landing(rect(0.0, 0.0, 1000.0, 800.0), &panels, Pos2::new(700.0, 500.0)),
            TabLanding::BareArea {
                pos: Pos2::new(700.0, 500.0),
            }
        );
    }

    #[test]
    fn a_tab_dropped_on_a_panel_body_or_outside_the_area_is_cancelled() {
        let panels = [target(
            1,
            rect(100.0, 100.0, 300.0, 200.0),
            rect(100.0, 100.0, 300.0, 24.0),
            &[],
        )];
        let area = rect(0.0, 60.0, 1000.0, 740.0);
        assert_eq!(
            tab_landing(area, &panels, Pos2::new(200.0, 200.0)),
            TabLanding::Cancelled
        );
        // Above the dock area — the program's own toolbar of the RECEIVING
        // window — is not part of the dock.
        assert_eq!(
            tab_landing(area, &panels, Pos2::new(500.0, 20.0)),
            TabLanding::Cancelled
        );
        assert_eq!(
            tab_landing(area, &panels, Pos2::new(f32::NAN, 20.0)),
            TabLanding::Cancelled
        );
    }

    #[test]
    fn a_panel_lands_free_where_it_was_dropped_and_nowhere_else() {
        let area = rect(0.0, 60.0, 1000.0, 740.0);
        assert_eq!(
            panel_landing(area, Pos2::new(300.0, 260.0), Vec2::new(20.0, 10.0)),
            Some(Pos2::new(280.0, 190.0))
        );
        assert_eq!(
            panel_landing(area, Pos2::new(300.0, 20.0), Vec2::ZERO),
            None
        );
    }
}
