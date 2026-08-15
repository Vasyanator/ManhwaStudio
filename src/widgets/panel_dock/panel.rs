/*
File: src/widgets/panel_dock/panel.rs

Purpose:
Widget #2 of the dockable-panel system: `CollapsiblePanel`, the drawing of ONE
`PanelNode` at the geometry the solver produced for it. Header strip (collapse
toggle + one clickable caption per visible tab), body of the active tab inside a
bounded `ScrollArea` that scrolls on BOTH axes, and a bottom-right resize grip.

Main responsibilities:
- draw one panel as an `Order::Foreground` `egui::Area` at a driver-supplied
  position, in the project's `Frame::popup` visual language;
- report back what the user did (tab switch, collapse toggle, manual resize) and
  what the content would like to be, without mutating the layout model itself.

Key structures:
- `PanelTabHeader`: one header entry (stable id + already localised caption).
- `CollapsiblePanel`: the builder.
- `CollapsiblePanelOutput`: everything the frame driver has to apply afterwards.
- `TabDrop`: a tab released over this panel's header strip.
- `ChromeGate`: the pure rule deciding whether a transparent panel is visible.

Key functions:
- `CollapsiblePanel::show`: draws the panel and runs the active tab's body.

Notes:
- The panel is deliberately NOT movable by `egui::Area`: dragging is the dock's
  own gesture (`drag.rs`) because it needs snap previews, which `Area`'s built-in
  move cannot provide. Position comes from the solver through `Area::current_pos`.
- The widget only REPORTS the reorganisation gestures: the header carries two
  move zones whose drag start it reports, the header strip is a drop zone that
  reports the tab released on it, and both a tab caption and the header's move
  zones carry a «Переместить в окно →» submenu whose chosen destination they
  report. Every layout change is applied by the frame driver through the model's
  checked operations.
- The submenu's ENTRIES are supplied by the driver (`move_targets`): only the
  driver knows which windows exist and which one this panel is drawn in, and the
  labels are localised there so the widget stays free of window bookkeeping.
- The header strip's geometry (`header_strip`, `header_rects`) is reported for a
  drop this widget CANNOT sense: a drag that crossed a window border leaves the
  receiving window without a pointer, so the driver hit-tests those rects on its
  behalf (`cross_window.rs`). `accepts_drop(false)` is the other half of that
  rule — it stops a window that merely still receives pointer events from
  claiming a drop the user made over a window floating above it.
- The header strip reads, left to right: collapse arrow, drag grip, tab captions,
  bare background. The panel is grabbed by the grip OR by that bare background,
  and both carry the layout context menu; only the grip is painted. The grip's
  slot is reserved with `Ui::add_space`, which in a left-to-right layout expands
  the row's x and nothing else — the strip's height is this panel's measured
  `PanelChrome`, so it must stay exactly what the button and the captions make it.
- Every gesture that starts on a widget which is ALSO a click target — a tab
  caption, the header handle — is sensed as `Sense::click_and_drag()` so egui
  postpones the click/drag verdict until the press is decidedly a drag. The two
  thresholds are egui's own (`InputOptions::max_click_dist` /
  `max_click_duration`); this file must not grow a timer of its own.
- The DRAWN size of a panel is exactly the size the solver gave it. That costs
  one non-obvious subtraction: `egui::Frame` allocates its stroke OUTSIDE the
  inner margin, so both the content width and the body's height budget have to
  be charged for it. Getting it wrong is not a cosmetic error — the resize grip
  used to read the drawn rect back into the request and the panel grew by the
  stroke on every dragged frame.
- The panel must stay on `Order::Foreground`: canvas input gating is z-order
  based (`crate::input_util::pointer_over_floating_area`), so a panel on any
  lower order would stop shielding the canvas underneath it.
- Every egui `Id` here derives from `PanelId` / `TabId` literals and the layout
  key, never from the localised caption (`egui-docs/05-ids-and-i18n.md` §2).
- `egui::Resize` is deliberately not used: its size lives in egui memory, which
  is exactly what forced the `id_salt`-revision hack in the old typing panels.
  The size lives in our model instead.
- A panel may be declared TRANSPARENT UNTIL HOVER, which fades its frame, its
  collapse arrow, its two grips and the BODY'S SCROLL BARS out while the pointer
  is elsewhere (and its captions too, but only when it shows exactly one of
  them). Everything drawn AROUND the content is chrome; the content itself, and
  any scroll area of its own, never fades. The mode is
  PAINTING ONLY: same header row, same `Frame` margins and stroke WIDTH, same
  `Ui::interact` calls, same `Order::Foreground` `Area`. Nothing else is
  possible — the dock's `PanelChrome` is one global value taken from the last
  panel drawn, so a header that measured differently in one mode would move
  every panel of every program tab and repaint forever. The corollary is that an
  invisible panel still intercepts pointer input over its rect; that is the
  accepted cost of leaving the `Area` interactive, which the canvas' own input
  gating depends on.
- The body FILLS its budget on both axes and scrolls whatever does not fit, and
  what it reports back is the CONTENT's size, not the drawn one. Those two rules
  are one design: the panel is as tall as its largest tab, so a smaller tab must
  stretch — and the moment a measurement is taken from what was drawn instead of
  from what the content asked for, every solver decision (a shrink, a manual
  size, another tab) becomes this tab's own request and the panel can never get
  smaller again.
*/

use egui::scroll_area::ScrollBarVisibility;
use egui::{Id, Pos2, Rect, Sense, Stroke, Vec2};

use super::drag::{DraggedTab, SNAP_LINE_WIDTH, insertion_index};
use super::model::{PanelId, TabId};
use super::solver::{
    COLLAPSED_PANEL_HEIGHT, PANEL_MIN_BODY_HEIGHT, PANEL_MIN_WIDTH, PanelChrome, SolvedPanel,
};
use super::window::MoveTarget;

/// Side, in points, of the square resize grip in the panel's bottom-right corner.
const RESIZE_GRIP_SIZE: f32 = 12.0;

/// Space, in points, between the header strip and the active tab's body.
const HEADER_BODY_SPACING: f32 = 4.0;

/// Gap, in points, kept between a tab caption and the header's move zones —
/// after the drag handle and before the bare space right of the last caption —
/// so a click aimed at a tab never lands on a move zone.
const HEADER_HANDLE_GAP: f32 = 6.0;

/// Width, in points, of the drag handle's slot at the head of the header strip.
///
/// The slot is reserved before the first caption, so it exists whatever the
/// captions do to the strip: a panel that cannot be grabbed cannot be
/// reorganised at all.
const HEADER_HANDLE_WIDTH: f32 = 16.0;

/// Length, in points, of the two grip lines painted in the drag handle.
const HEADER_GRIP_LINE_LENGTH: f32 = 10.0;

/// Vertical distance, in points, between the two grip lines of the drag handle.
const HEADER_GRIP_LINE_SPACING: f32 = 4.0;

/// A tab released over a panel's header strip.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TabDrop {
    /// The tab that was dropped.
    pub tab: TabId,
    /// The panel it came from. Equal to the receiving panel when the gesture is
    /// a reorder inside one header strip.
    pub from: PanelId,
    /// Index it should take in the receiving panel's tab order, already clamped
    /// to the strip's header count.
    pub index: usize,
}

// The two physical minimums below live in `solver.rs` on purpose: the solver
// floors every requested size at them, so a panel is never solved smaller than
// the frame this widget can draw. Keeping a second private copy here is exactly
// how the drawn panel would start overflowing its solved rect again.

/// One entry of the panel's header strip.
///
/// `id` is the stable, non-localised tab key (it seeds the egui id of the header
/// widget); `title` is the already localised caption to paint.
#[derive(Copy, Clone, Debug)]
pub struct PanelTabHeader<'a> {
    /// Stable tab key; the egui id source.
    pub id: TabId,
    /// Localised caption.
    pub title: &'a str,
}

/// One entry of the «Переместить в окно →» submenu.
///
/// The widget knows nothing about windows: the driver decides which
/// destinations exist for the host this panel is drawn in
/// ([`super::window::move_targets`]) and localises them
/// ([`super::window::move_target_label`]). `target` — never the label — seeds
/// the egui id of the entry, so the menu keeps its state across a language
/// switch (`egui-docs/05-ids-and-i18n.md` §2).
#[derive(Copy, Clone, Debug)]
pub struct MoveTargetEntry<'a> {
    /// Where the tab or the panel would go.
    pub target: MoveTarget,
    /// Already localised label.
    pub label: &'a str,
}

/// What the user did to a panel during one frame, plus what its content wants.
///
/// The widget never mutates the layout model: the frame driver applies these
/// fields, so every model invariant stays enforced in exactly one place.
#[derive(Clone, Debug, PartialEq)]
pub struct CollapsiblePanelOutput {
    /// Outer rect the panel actually occupied on screen this frame. It is the
    /// rect the solver produced: the body fills its whole budget, so a tab whose
    /// content is shorter stretches into the panel instead of shortening it.
    pub rect: Rect,
    /// Tab whose header was clicked this frame, if any.
    pub activated_tab: Option<TabId>,
    /// `true` when the collapse arrow was clicked this frame.
    pub toggle_collapsed: bool,
    /// New outer size requested by a drag of the resize grip, in points. `None`
    /// while the grip is idle — an idle grip must not overwrite a size the
    /// solver is driving.
    pub size_override: Option<Vec2>,
    /// Outer size the content would like to have: this panel's own overhead plus
    /// the height the ACTIVE TAB's content measured, whether that content fitted
    /// the budget or not. `None` for a collapsed panel, whose drawn height says
    /// nothing about its content.
    ///
    /// It is deliberately independent of the height the panel was solved at: the
    /// body fills its budget, so reporting the drawn height would feed every
    /// solver decision back in as this tab's own request.
    ///
    /// Only the height carries content information. The width is reported as the
    /// width the panel was GIVEN — the widget lays the body out inside it and
    /// never measures a preferred width — which is why the driver keeps the width
    /// the panel asked for instead of storing this one.
    pub measured_size: Option<Vec2>,
    /// Style-dependent vertical overhead this panel actually drew, reported in
    /// every state (collapsed included) so the solver stops guessing it from the
    /// nominal [`COLLAPSED_PANEL_HEIGHT`].
    pub chrome: PanelChrome,
    /// `true` on the frame a drag of the header background started. The driver
    /// turns it into a move session; the widget itself never moves the panel.
    pub drag_started: bool,
    /// Tab released over this panel's header strip this frame, if any.
    pub tab_drop: Option<TabDrop>,
    /// `true` when the header's context menu asked for the program tab's default
    /// layout to be restored.
    pub reset_layout: bool,
    /// Tab whose context menu asked to be moved, and where to.
    ///
    /// The platform-independent half of the cross-window move (plan §4.8): it
    /// needs no pointer information at all, so it works on every backend
    /// regardless of how that backend routes events while a mouse button is
    /// held — and on Wayland it is the only path there is.
    pub move_tab: Option<(TabId, MoveTarget)>,
    /// Where this panel's own header context menu asked the WHOLE panel — every
    /// tab it holds — to be moved.
    pub move_panel: Option<MoveTarget>,
    /// The header strip's rect — the zone a dragged tab is dropped on — in this
    /// window's screen coordinates.
    ///
    /// Reported so the DRIVER can decide a drop this window never saw: a drag
    /// that crossed a window border keeps its pointer events in the window it
    /// started in, so the receiving window has no pointer to hit-test with and
    /// must be told what the cursor was over (`cross_window.rs`).
    pub header_strip: Rect,
    /// Rects of the tab captions inside that strip, in strip order. Same reason
    /// as [`CollapsiblePanelOutput::header_strip`]: they carry the insertion
    /// index of a drop the widget itself could not sense.
    pub header_rects: Vec<Rect>,
}

/// Draws one panel of a `DockLayout`.
///
/// Everything the widget needs is passed in per frame; it owns no state. The
/// caller (the `PanelDock` frame driver) supplies the solved geometry, the
/// header entries, the active tab and the id scope.
#[derive(Clone, Debug)]
pub struct CollapsiblePanel<'a> {
    id: PanelId,
    id_scope: &'a str,
    rect: Rect,
    collapsed: bool,
    tabs: &'a [PanelTabHeader<'a>],
    active_tab: Option<TabId>,
    min_size: Vec2,
    accepts_drop: bool,
    transparent_until_hover: bool,
    force_visible: bool,
    move_targets: &'a [MoveTargetEntry<'a>],
}

impl<'a> CollapsiblePanel<'a> {
    /// Starts a panel drawn at the origin with no tabs.
    ///
    /// `id_scope` namespaces every egui id of this panel; pass the layout key
    /// (the program tab's stable key), never a localised string, so two program
    /// tabs cannot collide on a panel index.
    #[must_use]
    pub fn new(id: PanelId, id_scope: &'a str) -> Self {
        Self {
            id,
            id_scope,
            rect: Rect::from_min_size(Pos2::ZERO, Vec2::ZERO),
            collapsed: false,
            tabs: &[],
            active_tab: None,
            min_size: Vec2::new(PANEL_MIN_WIDTH, COLLAPSED_PANEL_HEIGHT),
            accepts_drop: true,
            transparent_until_hover: false,
            force_visible: false,
            move_targets: &[],
        }
    }

    /// Whether this panel hides its own CHROME while the pointer is elsewhere.
    ///
    /// Chrome is everything drawn AROUND the content: the panel's frame
    /// (background, border, shadow), the collapse arrow, the drag grip, the
    /// resize grip and the body's SCROLL BARS (see [`faded_scroll_style`]). The
    /// active tab's body never fades — nor does a scroll area of its own — and
    /// the tab captions fade only when the panel shows exactly one of them (see
    /// [`caption_opacity`]).
    ///
    /// **The mode changes painting and nothing else.** The header strip is laid
    /// out identically, [`egui::Frame`] keeps its inner margin and its stroke
    /// WIDTH, every `Ui::interact` still runs and the panel keeps its
    /// `Order::Foreground` `Area`, so an invisible panel occupies the same rect,
    /// reports the same [`PanelChrome`] and the same measurement, and still
    /// intercepts pointer input over its rect. That is a hard requirement, not a
    /// simplification: `PanelChrome` is global to the whole dock state and taken
    /// from the last panel drawn, so a panel whose header measured differently
    /// would move every other panel of every program tab and repaint forever.
    ///
    /// The driver passes what the tab being DRAWN declared
    /// ([`super::PanelTab::transparent_until_hover`]).
    #[must_use]
    pub fn transparent_until_hover(mut self, transparent: bool) -> Self {
        self.transparent_until_hover = transparent;
        self
    }

    /// Forces a transparent panel to show its chrome this frame wherever the
    /// pointer is. Ignored by a panel that is not transparent.
    ///
    /// The driver raises it for the gestures the widget cannot see the whole of:
    /// a tab is in flight ANYWHERE in the dock (every transparent panel has to
    /// be visible, or there is nothing to aim the drop at) and THIS panel is
    /// being moved (the panel is pinned to the dock area's border while the
    /// cursor is pulled past it, so the cursor is regularly not over the panel
    /// it drags). Gestures the widget CAN see — a press held anywhere in its own
    /// rect, a drag of one of its chrome zones — need no help from the driver;
    /// see [`ChromeGate`].
    #[must_use]
    pub fn force_visible(mut self, force: bool) -> Self {
        self.force_visible = force;
        self
    }

    /// Destinations the «Переместить в окно →» submenu offers, in menu order.
    ///
    /// Empty — the default — hides the submenu entirely: a menu that can only
    /// say "nowhere" is worse than no menu. The driver builds the list once per
    /// window and per frame, because every panel of one window offers the same
    /// destinations.
    #[must_use]
    pub fn move_targets(mut self, targets: &'a [MoveTargetEntry<'a>]) -> Self {
        self.move_targets = targets;
        self
    }

    /// Whether this panel's header strip may take a tab released over it this
    /// frame.
    ///
    /// The driver passes `false` while the shared-frame pointer says the drop
    /// belongs to ANOTHER of our windows (`cross_window.rs`). Without it a window
    /// that merely still receives pointer events — the one the drag started in,
    /// which keeps an implicit grab for as long as the button is held — would
    /// claim a drop the user made over a window floating above it, and both
    /// windows would show an insertion marker at once.
    #[must_use]
    pub fn accepts_drop(mut self, accepts_drop: bool) -> Self {
        self.accepts_drop = accepts_drop;
        self
    }

    /// Applies the geometry the solver produced for this panel.
    ///
    /// Only `SolvedPanel::rect` is used. The body's height budget is derived
    /// from that rect and the header this frame actually drew rather than from
    /// `SolvedPanel::body_max_height`: the solver works from the PREVIOUS
    /// frame's `PanelChrome`, and on the frame the style changes a mismatch
    /// would make the drawn panel taller than the solved rect — which, through
    /// the resize grip, would inflate the panel by that difference on every
    /// drag frame.
    #[must_use]
    pub fn geometry(mut self, solved: SolvedPanel) -> Self {
        self.rect = solved.rect;
        self
    }

    /// Sets the collapsed state. A collapsed panel draws its header only and
    /// never runs the body closure.
    #[must_use]
    pub fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }

    /// Sets the header strip: the tabs to show, in panel order, and which of
    /// them is drawn. `active` must be one of `tabs` or `None`; anything else
    /// simply leaves no header selected.
    #[must_use]
    pub fn tabs(mut self, tabs: &'a [PanelTabHeader<'a>], active: Option<TabId>) -> Self {
        self.tabs = tabs;
        self.active_tab = active;
        self
    }

    /// Lower bound the resize grip clamps the outer size to, in points.
    ///
    /// [`COLLAPSED_PANEL_HEIGHT`] is used here as an absolute floor, not as an
    /// estimate of the header: a size pinned below the real header height is
    /// raised again by the solver, which knows the measured [`PanelChrome`].
    #[must_use]
    pub fn min_size(mut self, min_size: Vec2) -> Self {
        self.min_size = Vec2::new(
            min_size.x.max(PANEL_MIN_WIDTH),
            min_size.y.max(COLLAPSED_PANEL_HEIGHT),
        );
        self
    }

    /// Draws the panel and, unless it is collapsed, runs `body` for the active
    /// tab inside a scroll area bounded by what the solved rect leaves under the
    /// header.
    ///
    /// `body` is called at most once, and never when the panel is collapsed or
    /// has no active tab.
    pub fn show(
        self,
        ctx: &egui::Context,
        body: impl FnOnce(&mut egui::Ui),
    ) -> CollapsiblePanelOutput {
        let area_id = egui::Id::new(("ms_panel_dock_panel", self.id_scope, self.id.get()));
        let outer_width = self.rect.width().max(PANEL_MIN_WIDTH);

        egui::Area::new(area_id)
            .order(egui::Order::Foreground)
            // Dragging is the dock's own gesture (phase 4); the solver owns the
            // position, and it already clamps the panel into the dock area, so
            // egui must not clamp it a second time.
            .movable(false)
            .interactable(true)
            .constrain(false)
            .current_pos(self.rect.min)
            .show(ctx, |ui| self.contents(ui, outer_width, body))
            .inner
    }

    /// Body of the `Area` closure: the popup frame, the header, the tab body and
    /// the resize grip.
    fn contents(
        &self,
        ui: &mut egui::Ui,
        outer_width: f32,
        body: impl FnOnce(&mut egui::Ui),
    ) -> CollapsiblePanelOutput {
        // Decided BEFORE anything is painted: `egui::Frame` takes its colours as
        // builder arguments, so how faded the chrome is has to be known before
        // `Frame::show` runs.
        let chrome_opacity = self.chrome_opacity(ui);
        // `multiply_with_opacity` scales `fill`, `stroke.color` and
        // `shadow.color` and NOTHING else
        // (`egui-0.35.0/src/containers/frame.rs:313-318`). The inner margin and
        // the stroke WIDTH survive it untouched, which is exactly what keeps the
        // transparent mode geometry-neutral: `margin`, `border`, the body budget
        // and the reported `PanelChrome` below are the same numbers in both
        // states. At `0.0` the shadow disappears with the rest, so no separate
        // `Shadow::NONE` is needed.
        let frame = egui::Frame::popup(ui.style()).multiply_with_opacity(chrome_opacity);
        let margin = frame.inner_margin.sum();
        // `Frame` draws its stroke OUTSIDE the inner margin and allocates
        // `content + inner_margin + stroke` in the parent
        // (`egui-0.35.0/src/containers/frame.rs:340-352`). Charging the border to
        // the content is what makes the DRAWN outer rect exactly the rect the
        // solver produced; leaving it out made the drawn panel `2 * stroke`
        // wider and `stroke` taller than the solve, which the resize grip then
        // read back into the size request once per frame.
        let border = frame.stroke.width * 2.0;
        let inner_width = (outer_width - margin.x - border).max(PANEL_MIN_WIDTH);
        ui.set_width(outer_width);
        ui.set_min_width(outer_width);
        ui.set_max_width(outer_width);
        if !self.collapsed {
            // THE HEIGHT CEILING. An `Area`'s `Ui` is built with the size the area
            // measured on the PREVIOUS frame as its `max_rect`
            // (`egui-0.35.0/src/containers/area.rs:610`, `:666`), and a
            // `ScrollArea` never allocates more than
            // `available_rect_before_wrap()` (`scroll_area.rs:763-766`). Without
            // this line the body is therefore capped at the height the panel
            // already had, and NOTHING can make a panel taller than it once was —
            // not the solver, not a manual resize, not a bigger tab. The width was
            // never affected, because `set_width` above states it outright; this
            // is the same statement for the other axis. `set_height` also raises
            // `min_rect`, so the area reports the solved size back and the
            // ceiling of the next frame is the size this frame was solved at.
            ui.set_height(self.rect.height().max(0.0));
        }

        let mut activated_tab: Option<TabId> = None;
        let mut move_tab: Option<(TabId, MoveTarget)> = None;
        let mut toggle_collapsed = false;
        // Room the body was GIVEN and size the content actually asked for. The
        // difference between them is what makes the measurement independent of
        // the budget: the panel's own overhead is `drawn height - budget`, and
        // what the content wants is `overhead + content`, whether the body was
        // stretched into a taller panel or is scrolling inside a shorter one.
        let mut body_budget_height = 0.0_f32;
        let mut body_content_height = 0.0_f32;
        // Drawn height of the header strip alone, without the frame margins.
        let mut header_height = 0.0_f32;
        // Laid-out rect of the header row, and of every tab header inside it.
        // Both are needed AFTER the frame closes: the row bounds the drag handle
        // and the drop zone, the headers decide the insertion index.
        let mut header_row: Option<Rect> = None;
        let mut header_rects: Vec<Rect> = Vec::with_capacity(self.tabs.len());
        // Horizontal span `(left, right)` the drag handle reserved between the
        // collapse button and the first caption. Needed after the frame closes,
        // where the strip's vertical extent is finally known.
        let mut handle_span: Option<(f32, f32)> = None;

        let frame_response = frame.show(ui, |ui| {
            ui.set_width(inner_width);
            ui.set_min_width(inner_width);
            ui.set_max_width(inner_width);
            let header = ui.horizontal(|ui| {
                let (icon, hint) = if self.collapsed {
                    ("▶", t!("widgets.panel_dock.expand_panel_tooltip"))
                } else {
                    ("▼", t!("widgets.panel_dock.collapse_panel_tooltip"))
                };
                // The collapse arrow is chrome and fades with the frame. Faded,
                // never hidden: it keeps its slot in the row, so the strip's
                // height — this panel's `PanelChrome` — cannot depend on the
                // mode.
                if faded(ui, chrome_opacity, |ui| {
                    ui.small_button(icon).on_hover_text(hint).clicked()
                }) {
                    toggle_collapsed = true;
                }
                // Reserve the drag handle's slot right of the collapse button and
                // LEFT of the first caption. Space is advanced, never allocated:
                // in a left-to-right layout `Ui::add_space` expands the region's
                // x only (`egui-0.35.0/src/layout.rs:674-679`), so the strip stays
                // exactly as tall as the button and the captions make it. That
                // height is reported as this panel's `PanelChrome`, so a header
                // grown by one point grows every panel in the layout by one point.
                let handle_left = ui.cursor().left();
                ui.add_space(HEADER_HANDLE_WIDTH);
                let handle_right = ui.cursor().left();
                // The gap belongs INSIDE the row: egui inserts its item spacing
                // only AFTER an allocated widget, and advanced space allocates
                // nothing, so without this the first caption would start flush
                // against the handle and the two gestures would share a pixel.
                ui.add_space(HEADER_HANDLE_GAP);
                handle_span = Some((handle_left, handle_right));
                let captions = caption_opacity(self.tabs.len(), chrome_opacity);
                faded(ui, captions, |ui| {
                    for tab in self.tabs {
                        header_rects.push(self.tab_header(
                            ui,
                            tab,
                            &mut activated_tab,
                            &mut move_tab,
                        ));
                    }
                });
            });
            header_height = header.response.rect.height();
            header_row = Some(header.response.rect);

            if self.collapsed {
                return;
            }
            let Some(active) = self.active_tab else {
                return;
            };
            ui.add_space(HEADER_BODY_SPACING);
            // Exact budget: whatever is left of the solved rect once the header
            // that was actually drawn and the frame's bottom margin are paid for.
            // `Ui::cursor` is already advanced past the header and its spacing,
            // so this is where the scroll area really starts.
            let body_top = ui.cursor().top();
            // The frame's bottom stroke is paid for here for the same reason the
            // width is: it sits outside the inner margin, so a budget that
            // ignored it made the drawn panel one stroke taller than its rect.
            let body_bottom =
                self.rect.bottom() - frame.inner_margin.bottomf() - frame.stroke.width;
            let body_max_height = (body_bottom - body_top).max(PANEL_MIN_BODY_HEIGHT);
            // THE SCROLL BARS ARE CHROME. They float OVER the body — an outline
            // around the content, not part of it — and a bar the user is
            // dragging stays fully lit (`interact_handle_opacity`) however far
            // the pointer wanders off the panel, so a transparent panel would
            // leave a bright bar floating on the bare canvas. `ScrollArea` reads
            // this style from the `Ui` it is shown in, both for its sizes
            // (`scroll_area.rs:756`) and for the two colours it paints
            // (`:1268`, `:1469-1519`), so fading the style here is the whole
            // fix. Restored INSIDE the body: a scroll area of the tab's own
            // content is content, and content never fades.
            let unfaded_scroll = ui.spacing().scroll;
            let fade_bars = chrome_opacity < 1.0;
            if fade_bars {
                ui.spacing_mut().scroll = faded_scroll_style(unfaded_scroll, chrome_opacity);
            }
            let scroll = egui::ScrollArea::both()
                .id_salt(("ms_panel_dock_body", self.id_scope, active.as_str()))
                .max_height(body_max_height)
                // Explicit although it is egui's default
                // (`egui-0.35.0/src/containers/scroll_area.rs:129-134`): a bar
                // that appears only when the content does not fit is a contract
                // of this widget, not a style detail.
                .scroll_bar_visibility(ScrollBarVisibility::VisibleWhenNeeded)
                // egui would otherwise refuse to make a scrolled axis smaller
                // than 64 pt (`scroll_area.rs:766-779`), which is ABOVE both
                // bounds a panel may be SOLVED at ([`PANEL_MIN_WIDTH`] = 40,
                // [`PANEL_MIN_BODY_HEIGHT`] = 24): a shrunk panel would then draw
                // a body wider and taller than the rect it was solved at, and the
                // neighbour one gap away would be overlapped by it. The panel is
                // the authority on its own budget.
                .min_scrolled_width(0.0)
                .min_scrolled_height(0.0)
                // The body always occupies the whole budget, on both axes: the
                // panel is as big as its LARGEST tab, so a smaller tab has to
                // stretch into the room the panel was solved at instead of
                // hugging its content in a corner of it. This cannot feed back
                // into the panel's size, because the measurement below is taken
                // from the content, never from what the body was given.
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if fade_bars {
                        ui.spacing_mut().scroll = unfaded_scroll;
                    }
                    body(ui);
                });
            if fade_bars {
                ui.spacing_mut().scroll = unfaded_scroll;
            }
            // `ScrollAreaOutput::inner_rect` is the rect the body was given
            // BEFORE any auto-shrink adjustment (`scroll_area.rs:1040`), i.e.
            // exactly the budget above — which is what makes the subtraction
            // below the panel's overhead and nothing else.
            body_budget_height = scroll.inner_rect.height();
            body_content_height = scroll.content_size.y;
        });

        let frame_rect = frame_response.response.rect;
        // Both gestures live in the header strip, which spans the frame's inner
        // width rather than only the width the header row happened to use.
        let content_right = frame_rect.right() - frame.inner_margin.rightf();
        let header = match header_row.zip(handle_span) {
            Some((row, span)) => self.header_handle(
                ui,
                handle_zone(row, span, content_right),
                spare_move_zone(row, content_right),
                chrome_opacity,
            ),
            None => HeaderMenuOutcome::default(),
        };
        let HeaderMenuOutcome {
            drag_started,
            reset_layout,
            move_panel,
        } = header;
        let header_strip = header_row.map_or(Rect::NOTHING, |row| {
            header_strip_rect(row, content_right)
        });
        let tab_drop = header_row
            .and_then(|row| self.tab_drop_zone(ui, row, content_right, &header_rects));
        let size_override = self.resize_grip(ui, frame_rect, chrome_opacity);
        // What the solver has to assume next frame. `collapsed_height` is the
        // outer height a collapsed panel occupies (border + margins + header);
        // the expanded panel additionally spends `HEADER_BODY_SPACING` before
        // the body starts, which is exactly the budget `body_max_height` is
        // derived from above. Both are reported in EVERY state, so the estimate
        // is already correct on the frame a panel is collapsed.
        let chrome = PanelChrome::new(
            margin.y + border + header_height,
            margin.y + border + header_height + HEADER_BODY_SPACING,
        );
        let measured_size = if self.collapsed {
            None
        } else {
            // The CONTENT height plus this panel's own overhead, never the drawn
            // height: the body now always fills its budget (a small tab stretches
            // into a panel sized by a bigger sibling tab), so the drawn height is
            // the height the solver handed out and reporting it would turn every
            // solver decision — a shrink, a pin, another tab's size — into this
            // tab's own request. That is the same feedback loop the width rule
            // below exists to prevent, and it would additionally stop a panel
            // ever getting narrower or shorter again.
            //
            // Width is reported as the width we were GIVEN, never as the drawn
            // one: feeding the drawn width back would add the frame margin to
            // the request on every frame and the panel would creep wider.
            let overhead = (frame_rect.height() - body_budget_height).max(0.0);
            Some(Vec2::new(outer_width, overhead + body_content_height))
        };

        CollapsiblePanelOutput {
            rect: frame_rect,
            activated_tab,
            toggle_collapsed,
            size_override,
            measured_size,
            chrome,
            drag_started,
            tab_drop,
            reset_layout,
            move_tab,
            move_panel,
            header_strip,
            header_rects,
        }
    }

    /// Draws one tab header and returns its laid-out rect.
    ///
    /// The header is BOTH a click target (activate the tab) and a drag source
    /// (move the tab to another panel), and it is ONE widget sensing both. That
    /// is what makes a short click switch tabs instead of throwing the caption
    /// at the cursor: a widget sensing click AND drag is only marked `dragged`
    /// once the press became "decidedly a drag" — the pointer moved further than
    /// [`egui::InputOptions::max_click_dist`] or was held longer than
    /// [`egui::InputOptions::max_click_duration`]
    /// (`egui-0.35.0/src/interaction.rs:191-206`,
    /// `egui-0.35.0/src/input_state/mod.rs:1512-1518`).
    ///
    /// [`egui::Ui::dnd_drag_source`] is deliberately NOT used: it allocates its
    /// source with `Sense::drag()` alone (`egui-0.35.0/src/ui.rs:2676-2680`), so
    /// the header started moving on the first pressed frame — and because it
    /// then re-parents the caption into a tooltip layer, the click target the
    /// press had latched onto disappears from the next frame's widget rects and
    /// `interaction.rs:118-122` drops the pending click. Switching tabs by
    /// clicking therefore only worked when press and release fell inside one
    /// frame.
    ///
    /// A tab has no close affordance on purpose: a tab can only be MOVED, never
    /// closed (plan §9.1, settled with the user). Its context menu carries the
    /// moves the mouse cannot always express — «Переместить в окно →», reported
    /// through `moved`.
    fn tab_header(
        &self,
        ui: &mut egui::Ui,
        tab: &PanelTabHeader<'_>,
        activated: &mut Option<TabId>,
        moved: &mut Option<(TabId, MoveTarget)>,
    ) -> Rect {
        let selected = self.active_tab == Some(tab.id);
        // Salt the label with its stable tab key: the caption is localised, so
        // an id derived from it would move on a language switch.
        let caption = ui
            .push_id(tab.id.as_str(), |ui| {
                ui.selectable_label(selected, tab.title)
            })
            .inner;
        // Upgrading the caption's OWN widget keeps a single id for both roles:
        // the hover highlight, the click and the drag all belong to it, and the
        // hit-test can no longer route them to two different widgets.
        let response = caption.interact(Sense::DRAG);
        if response.clicked() {
            *activated = Some(tab.id);
        }
        if !self.move_targets.is_empty() {
            response.context_menu(|ui| {
                if let Some(target) = move_to_window_submenu(ui, self.move_targets) {
                    *moved = Some((tab.id, target));
                }
            });
        }
        // Only the primary button moves a tab. The caption now carries the
        // context menu on the same widget, and a held secondary button is
        // "decidedly dragging" to egui exactly like a held primary one — without
        // this filter, opening the menu slowly would post a drag payload.
        if response.dragged_by(egui::PointerButton::Primary) {
            self.carry_dragged_tab(ui, tab, selected, &response);
        }
        response.rect
    }

    /// Publishes the drag-and-drop payload of a tab whose header is being
    /// dragged and paints the caption again under the cursor.
    ///
    /// The preview is painted through a `Painter` on `Order::Tooltip` rather
    /// than by re-parenting the caption into that layer: a second `Ui` inside
    /// the header row would advance the row's auto-id counter and rename every
    /// caption after it for as long as the drag lasts. Painting registers no
    /// widget at all, so the ids of the strip are the same whether a drag is in
    /// flight or not.
    ///
    /// The preview is placed so the point the header was GRABBED at stays under
    /// the cursor (requirement 7), which is also the offset the payload carries
    /// for the driver to place a detached panel with.
    fn carry_dragged_tab(
        &self,
        ui: &mut egui::Ui,
        tab: &PanelTabHeader<'_>,
        selected: bool,
        response: &egui::Response,
    ) {
        let rect = response.rect;
        // The gesture is already in flight and its owner is this very widget's
        // `Response`, so reading the pointer here is geometry of an active
        // gesture, not hover detection.
        let press_origin = ui.ctx().input(|input| input.pointer.press_origin());
        let grab_offset = header_grab_offset(rect, press_origin).unwrap_or_else(|| rect.size() * 0.5);
        egui::DragAndDrop::set_payload(
            ui.ctx(),
            DraggedTab {
                tab: tab.id,
                from: self.id,
                grab_offset,
                // The driver paints the tear-out outline around this very
                // preview, and only the widget knows how big the caption is.
                header_size: rect.size(),
            },
        );
        let Some(pointer) = ui.ctx().pointer_interact_pos() else {
            // The pointer left the window while the button is held: the payload
            // stays alive (that is what a cross-window drag needs), only the
            // preview has nowhere to go.
            return;
        };
        let preview = Rect::from_min_size(pointer - grab_offset, rect.size());
        let layer_id = egui::LayerId::new(
            egui::Order::Tooltip,
            Id::new((
                "ms_panel_dock_tab_drag",
                self.id_scope,
                self.id.get(),
                tab.id.as_str(),
            )),
        );
        let visuals = ui.style().interact_selectable(response, selected);
        let painter = ui.ctx().layer_painter(layer_id);
        painter.rect(
            preview,
            visuals.corner_radius,
            visuals.weak_bg_fill,
            visuals.bg_stroke,
            egui::StrokeKind::Inside,
        );
        painter.text(
            preview.center(),
            egui::Align2::CENTER_CENTER,
            tab.title,
            egui::TextStyle::Button.resolve(ui.style()),
            visuals.text_color(),
        );
    }

    /// Senses the panel's two move zones and paints the drag grip.
    ///
    /// `handle` is the grip's own slot at the HEAD of the strip — between the
    /// collapse button and the first caption — and `spare` is the bare header
    /// background right of the last caption, absent when the captions reach the
    /// strip's end. Both grab the panel and both anchor the layout context menu;
    /// only `handle` is painted, so the strip shows exactly one grip wherever the
    /// user happens to grab it.
    ///
    /// Returns everything the two zones and their shared context menu asked for
    /// this frame ([`HeaderMenuOutcome`]). The zones deliberately exclude the
    /// collapse button and every tab caption: those are their own gestures.
    ///
    /// `chrome_opacity` fades the painted grip only. Both zones are SENSED in
    /// every state: an invisible panel is still grabbed, docked and given a
    /// context menu by exactly the same pixels.
    fn header_handle(
        &self,
        ui: &mut egui::Ui,
        handle: Rect,
        spare: Option<Rect>,
        chrome_opacity: f32,
    ) -> HeaderMenuOutcome {
        let mut menu = HeaderMenuOutcome::default();
        let grip = self.move_zone(ui, handle, "ms_panel_dock_move", &mut menu);

        let color = if grip.dragged() || grip.hovered() {
            ui.visuals().widgets.active.fg_stroke.color
        } else {
            ui.visuals().widgets.inactive.fg_stroke.color
        };
        let stroke = Stroke::new(1.0, color);
        let center = handle.center();
        let half = HEADER_GRIP_LINE_LENGTH * 0.5;
        faded(ui, chrome_opacity, |ui| {
            for step in [-1.0_f32, 1.0] {
                let y = center.y + step * HEADER_GRIP_LINE_SPACING * 0.5;
                ui.painter().line_segment(
                    [
                        Pos2::new(center.x - half, y),
                        Pos2::new(center.x + half, y),
                    ],
                    stroke,
                );
            }
        });

        menu.drag_started |= grip.drag_started();
        if let Some(rect) = spare {
            let bare = self.move_zone(ui, rect, "ms_panel_dock_move_spare", &mut menu);
            menu.drag_started |= bare.drag_started();
        }
        menu
    }

    /// Senses one rect of the header strip as a place the panel can be grabbed
    /// by, and anchors the layout context menu on it.
    ///
    /// `id_key` namespaces the zone inside the panel's own `Ui`, so the several
    /// zones of one strip never share an id. `menu` is only ever raised, never
    /// lowered, which is what lets every zone of one strip write into the same
    /// outcome.
    ///
    /// The zone senses click AND drag on purpose: a widget sensing drag alone
    /// starts moving on the first pressed frame, while `click_and_drag` makes
    /// egui postpone the verdict until the press is decidedly a drag (see
    /// [`CollapsiblePanel::tab_header`]).
    fn move_zone(
        &self,
        ui: &mut egui::Ui,
        rect: Rect,
        id_key: &str,
        menu: &mut HeaderMenuOutcome,
    ) -> egui::Response {
        let response = ui
            .interact(rect, ui.id().with(id_key), Sense::click_and_drag())
            .on_hover_cursor(egui::CursorIcon::Grab);
        response.context_menu(|ui| {
            if !self.move_targets.is_empty() {
                if let Some(target) = move_to_window_submenu(ui, self.move_targets) {
                    menu.move_panel = Some(target);
                }
                ui.separator();
            }
            if ui.button(t!("widgets.panel_dock.reset_layout")).clicked() {
                menu.reset_layout = true;
                ui.close();
            }
        });
        response
    }

    /// Senses the whole header strip as a drop zone for a dragged tab, painting
    /// the insertion marker and reporting the tab released on it.
    ///
    /// Hover is decided by `Response::contains_pointer` (through
    /// `dnd_hover_payload`), never by `hovered()`: while a drag is in flight egui
    /// reports `hovered() == false` for every widget, which is exactly why
    /// `dnd_drop_zone` uses the same accessor
    /// (`egui-0.35.0/src/ui.rs:2705-2712`).
    ///
    /// A panel the driver marked as not accepting drops
    /// ([`CollapsiblePanel::accepts_drop`]) neither paints the marker nor takes
    /// the payload: the pointer this window still receives belongs to another of
    /// our windows, and claiming the drop here would steal it.
    fn tab_drop_zone(
        &self,
        ui: &mut egui::Ui,
        header: Rect,
        content_right: f32,
        headers: &[Rect],
    ) -> Option<TabDrop> {
        if !self.accepts_drop {
            return None;
        }
        let rect = header_strip_rect(header, content_right);
        let response = ui.interact(
            rect,
            ui.id().with("ms_panel_dock_tab_strip"),
            Sense::hover(),
        );
        let centers: Vec<f32> = headers.iter().map(|rect| rect.center().x).collect();
        // The pointer position is read for GEOMETRY only — whether the strip is
        // under the pointer at all is decided by the response above.
        let index = ui
            .ctx()
            .input(|input| input.pointer.interact_pos())
            .map_or(headers.len(), |pos| insertion_index(&centers, pos.x));

        if response.dnd_hover_payload::<DraggedTab>().is_some() {
            let x = headers
                .get(index)
                .map_or_else(|| headers.last().map_or(rect.left(), Rect::right), Rect::left);
            let color = ui.visuals().selection.stroke.color;
            ui.painter().line_segment(
                [
                    Pos2::new(x, rect.top()),
                    Pos2::new(x, rect.bottom()),
                ],
                Stroke::new(SNAP_LINE_WIDTH, color),
            );
        }

        let dropped = response.dnd_release_payload::<DraggedTab>()?;
        Some(TabDrop {
            tab: dropped.tab,
            from: dropped.from,
            index,
        })
    }

    /// Draws and senses the bottom-right resize grip, returning the outer size
    /// requested by an in-progress drag.
    ///
    /// Allocated with `Ui::interact` (no layout space) AFTER the frame content,
    /// so it wins the hit-test over whatever the body painted underneath it.
    ///
    /// While the grip is held the GESTURE is authoritative: the size is measured
    /// from the panel's size and the pointer position at the moment the grip was
    /// grabbed, which are kept in egui's per-frame data store for the duration
    /// of the drag. Nothing about the panel's content or its currently drawn
    /// rect enters the answer — see [`resized_outer_size`] for why that matters.
    ///
    /// `chrome_opacity` fades the painted corner only: the grip is sensed in
    /// every state, so an invisible panel is still resizable.
    fn resize_grip(
        &self,
        ui: &mut egui::Ui,
        frame_rect: Rect,
        chrome_opacity: f32,
    ) -> Option<Vec2> {
        if self.collapsed {
            return None;
        }
        let grip_rect = Rect::from_min_max(
            frame_rect.max - Vec2::splat(RESIZE_GRIP_SIZE),
            frame_rect.max,
        );
        let grip_id = ui.id().with("ms_panel_dock_resize");
        let response = ui
            .interact(grip_rect, grip_id, Sense::drag())
            .on_hover_cursor(egui::CursorIcon::ResizeSouthEast);

        let color = if response.dragged() || response.hovered() {
            ui.visuals().widgets.active.fg_stroke.color
        } else {
            ui.visuals().widgets.inactive.fg_stroke.color
        };
        let stroke = Stroke::new(1.0, color);
        faded(ui, chrome_opacity, |ui| {
            let painter = ui.painter();
            for step in 1_u8..=3 {
                let inset = RESIZE_GRIP_SIZE * (f32::from(step) / 4.0);
                painter.line_segment(
                    [
                        Pos2::new(grip_rect.right() - inset, grip_rect.bottom()),
                        Pos2::new(grip_rect.right(), grip_rect.bottom() - inset),
                    ],
                    stroke,
                );
            }
        });

        if response.drag_stopped() {
            // The gesture is over; its anchor must not survive into the next one.
            ui.ctx()
                .data_mut(|data| data.remove::<ResizeAnchor>(grip_id));
            return None;
        }
        if !response.dragged() {
            return None;
        }
        let pointer = response.interact_pointer_pos()?;
        if response.drag_started() {
            ui.ctx().data_mut(|data| {
                data.insert_temp(
                    grip_id,
                    ResizeAnchor {
                        size: frame_rect.size(),
                        origin: pointer,
                    },
                );
            });
        }
        // An anchor is missing only when the frame that would have stored it
        // never saw a pointer position. The gesture then reports nothing at all
        // rather than resizing from a stale one.
        let anchor = ui.ctx().data(|data| data.get_temp::<ResizeAnchor>(grip_id))?;
        resized_outer_size(anchor, pointer, self.min_size)
    }

    /// Opacity every chrome element of this panel is painted at this frame, in
    /// `0.0..=1.0`. Always `1.0` unless
    /// [`CollapsiblePanel::transparent_until_hover`] was set.
    ///
    /// The two states are crossfaded with
    /// [`egui::Context::animate_bool_responsive`] (`egui-0.35.0/src/context.rs:3099`),
    /// which requests the repaints the animation needs by itself
    /// (`:3145-3148`) and returns the TARGET value on the first call for an id
    /// (`egui-0.35.0/src/animation_manager.rs:38-46`), so a panel never flashes
    /// on the frame it is first drawn. The animation id derives from the panel's
    /// own `Ui` id — the layout key plus the `PanelId` — never from a caption.
    ///
    /// Everything it reads is available BEFORE the panel draws, which is the
    /// point: the frame's colours are a builder argument, the ids of the three
    /// chrome zones are deterministic and the pointer press is raw input, so no
    /// widget has to exist yet for the answer to be exact.
    fn chrome_opacity(&self, ui: &egui::Ui) -> f32 {
        // This is the `!transparent` arm of [`ChromeGate::shows_chrome`], taken
        // early so an ordinary panel touches nothing at all: no animation entry
        // per panel, no popup or drag lookup on an idle frame.
        if !self.transparent_until_hover {
            return 1.0;
        }
        let ctx = ui.ctx();
        let base = ui.id();
        // A gesture on one of this panel's own CHROME zones. Asked of the
        // CONTEXT by id rather than of a `Response`, because the answer is
        // needed before the zones are sensed — and because `Response::hovered()`
        // is useless here anyway: while a button is held egui reports only the
        // dragged widget as hovered (`egui-0.35.0/src/interaction.rs:239-243`),
        // so a panel would vanish from under its own resize grip. The header
        // strip's drop zone is deliberately absent: it senses hover only, and a
        // drop is already covered by the driver's `force_visible`.
        let gesture = [
            "ms_panel_dock_move",
            "ms_panel_dock_move_spare",
            "ms_panel_dock_resize",
        ]
        .into_iter()
        .any(|key| ctx.is_being_dragged(base.with(key)));
        // Everything the pointer press says, read once. `press_origin` is set on
        // every press and cleared on every release
        // (`egui-0.35.0/src/input_state/mod.rs:1146`, `:1194`), and a
        // `PointerGone` clears NEITHER it nor `down` (`:1200-1206`) — which is
        // exactly what makes it survive a drag pulled out of the window.
        let (any_down, press_origin) =
            ctx.input(|input| (input.pointer.any_down(), input.pointer.press_origin()));
        let gate = ChromeGate {
            transparent: self.transparent_until_hover,
            forced: self.force_visible,
            // Occlusion-aware, unlike a raw pointer read: a panel covered by
            // another one does not light up through it
            // (`egui-0.35.0/src/context.rs:3036-3057`).
            pointer_inside: ui.rect_contains_pointer(self.rect),
            // The generic form of "a gesture aimed at this panel is in flight",
            // and the only one that reaches a widget of the BODY: a scroll
            // handle, a slider, a drag-adjusted value. None of them has an id
            // this widget knows, and all of them routinely take the cursor off
            // the panel while the button is held.
            pressed_inside: press_started_inside(self.rect, any_down, press_origin),
            gesture,
            // `Popup::is_any_open`, not the exact `Popup::is_id_open`. A context
            // menu's id is `response.id.with("popup")`
            // (`egui-0.35.0/src/containers/popup.rs:639`, `:653`), which IS
            // derivable for the two move zones — but not for a tab caption,
            // whose `Button::selectable` takes its id from the `Ui`'s auto-id
            // counter. A caption's «Переместить в окно →» menu has to keep its
            // panel visible just as much as the header's does, so the question
            // is asked of the whole context. The over-approximation costs a
            // transparent panel a moment of visibility while an unrelated popup
            // is open somewhere, which errs towards SHOWING the panel — the only
            // direction that cannot lose the user's UI.
            menu_open: egui::Popup::is_any_open(ctx),
        };
        ctx.animate_bool_responsive(base.with("ms_panel_dock_transparency"), gate.shows_chrome())
    }
}

/// The per-frame facts that decide whether a transparent-until-hover panel shows
/// its chrome. Kept as a value so the rule stays a pure, testable function of
/// them instead of a condition buried in the drawing code.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
struct ChromeGate {
    /// The panel declared [`CollapsiblePanel::transparent_until_hover`].
    transparent: bool,
    /// The driver forced visibility ([`CollapsiblePanel::force_visible`]).
    forced: bool,
    /// The pointer is over the panel's rect, occlusion taken into account.
    pointer_inside: bool,
    /// A pointer button is held and the press that started it landed inside the
    /// panel's rect (see [`press_started_inside`]).
    ///
    /// This is the general "the user is doing something to this panel" fact, and
    /// the only one that covers a widget of the BODY — a scroll-bar handle, a
    /// slider, a drag-adjusted value. Such a gesture has no id this widget knows
    /// and routinely drags the cursor off the panel, so without it the whole
    /// panel would fade out in the middle of the user's own drag.
    pressed_inside: bool,
    /// A drag of one of this panel's own CHROME zones (the two move zones or the
    /// resize grip) is in flight.
    ///
    /// It is NOT subsumed by [`ChromeGate::pressed_inside`], although those
    /// three zones are all inside the panel's rect: a gesture on them MOVES the
    /// rect out from under the press origin, which is a fixed screen point. A
    /// resize that shrinks the panel past the corner it was grabbed at is the
    /// standing example — the press origin ends up outside the new rect while
    /// the drag is still going. (The move gesture is additionally covered by
    /// [`ChromeGate::forced`], but the resize is not reported to the driver at
    /// all.) The converse does not hold either, so both are kept.
    gesture: bool,
    /// A context menu (or any other popup) is open.
    menu_open: bool,
}

impl ChromeGate {
    /// `true` when the panel paints its frame, its collapse arrow and its grips
    /// at full opacity this frame.
    ///
    /// A panel that never asked for the mode always shows them; a transparent
    /// one shows them while any single reason to be visible holds. The reasons
    /// are ORed on purpose: each of them is "the user is doing something with
    /// this panel", and the union is what keeps the panel from blinking when one
    /// reason hands over to the next (pointer enters → button goes down → the
    /// drag pulls the cursor outside → the panel itself moves away).
    #[must_use]
    fn shows_chrome(self) -> bool {
        !self.transparent
            || self.forced
            || self.pointer_inside
            || self.pressed_inside
            || self.gesture
            || self.menu_open
    }
}

/// `true` while a pointer button is held AND the press that started the current
/// gesture landed inside `rect`.
///
/// This is the widget-agnostic form of "the user is dragging something of this
/// panel". It is what keeps a transparent panel lit while the user works a
/// widget of its BODY — a scroll-bar handle, a slider, a drag-adjusted value —
/// and takes the cursor off the panel doing so; none of those has an id the
/// panel could ask about.
///
/// Both inputs come from [`egui::PointerState`], and their exact semantics are
/// what make the rule a latch that cannot get stuck:
/// * `press_origin` is written on every press and cleared on every release
///   (`egui-0.35.0/src/input_state/mod.rs:1146`, `:1194`), and the release frame
///   clears `down` too (`:1197`), so the latch drops exactly when the gesture
///   ends and never one frame later;
/// * a `PointerGone` clears NEITHER (`:1200-1206`, deliberately, so a drag
///   survives leaving the window), which is precisely the case this rule exists
///   for.
///
/// A press that started OUTSIDE `rect` never lights the panel, however far the
/// cursor travels afterwards — `press_origin` is where the button went down, not
/// where the pointer is now. Two accepted imprecisions: with several buttons the
/// latest press wins (egui keeps one origin), and the test is geometric, so a
/// press on a panel stacked ON TOP of this one at that point also counts. Both
/// err towards showing a panel, which is the only direction that cannot lose the
/// user's UI.
#[must_use]
fn press_started_inside(rect: Rect, any_down: bool, press_origin: Option<Pos2>) -> bool {
    any_down && press_origin.is_some_and(|origin| rect.contains(origin))
}

/// Runs `draw` with the painter of `ui` faded by `opacity` and restores the
/// previous opacity afterwards.
///
/// `opacity` MULTIPLIES what the `Ui` already has, so an `Area` that is itself
/// fading in is not overridden.
///
/// Deliberately not [`egui::Ui::scope`]: a scope allocates a child `Ui` and
/// advances the parent's cursor by what that child used, which is a layout
/// change — and this widget's contract is that the transparent mode changes
/// nothing about geometry. Opacity lives on the painter alone
/// (`egui-0.35.0/src/ui.rs:560`), so setting and restoring it paints differently
/// and lays out identically. It is also not [`egui::Ui::set_invisible`]
/// (`egui-0.35.0/src/ui.rs:537-540`), which additionally DISABLES the `Ui`: a
/// faded panel stays fully interactive.
fn faded<R>(ui: &mut egui::Ui, opacity: f32, draw: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let restore = ui.opacity();
    ui.set_opacity(restore * opacity);
    let result = draw(ui);
    ui.set_opacity(restore);
    result
}

/// Opacity the tab captions of a panel are painted at, given how many captions
/// its header strip shows and how faded its chrome is.
///
/// A panel showing SEVERAL captions keeps them fully opaque even when it is
/// invisible: they are then the only thing on screen that says the panel is
/// there and which of its tabs is showing, and captions the user cannot see are
/// captions the user cannot click. A panel showing exactly one caption has
/// nothing to choose between, so that caption fades with the rest of the chrome
/// and the body is left alone on the canvas.
#[must_use]
fn caption_opacity(shown_captions: usize, chrome_opacity: f32) -> f32 {
    if shown_captions > 1 {
        1.0
    } else {
        chrome_opacity
    }
}

/// `scroll` with the six opacities of a FLOATING scroll bar multiplied by
/// `opacity`, and every other field returned untouched.
///
/// `ScrollArea` multiplies `*_handle_opacity` straight into the handle's colour
/// and `*_background_opacity` into the bar's background
/// (`egui-0.35.0/src/containers/scroll_area.rs:1469-1519`), so scaling the six
/// of them is exactly "paint this bar `opacity` as strongly" and nothing else.
///
/// **Not one size-bearing field is touched** — `floating`, `bar_width`,
/// `floating_width`, `floating_allocated_width`, `bar_inner_margin`,
/// `bar_outer_margin`, `handle_min_length`, `content_margin`. That is a
/// contract, not tidiness: `ScrollStyle::allocated_width()`
/// (`egui-0.35.0/src/style.rs:652-658`) is `0.0` for a floating bar, which is
/// what lets a bar appear without taking room from the content and therefore
/// without being able to oscillate the measurement the panel's size is derived
/// from (see this directory's `MODULE_README.md`, "The body FILLS its budget").
/// A fade that moved any of them would reintroduce exactly that coupling.
///
/// **Limitation, stated rather than hidden:** egui hard-codes both opacities to
/// `1.0` for a SOLID scroll style (`scroll_area.rs:1483-1484`, `:1495-1496`), so
/// this function fades nothing there. It is not a problem today — no code in
/// this repo assigns `Spacing::scroll`, so every scroll area runs on egui's
/// default `ScrollStyle::floating()` (`egui-0.35.0/src/style.rs:639-650`) — but
/// a project-wide switch to solid bars would have to fade them another way, and
/// would break the no-oscillation contract above first.
#[must_use]
fn faded_scroll_style(scroll: egui::style::ScrollStyle, opacity: f32) -> egui::style::ScrollStyle {
    egui::style::ScrollStyle {
        dormant_handle_opacity: scroll.dormant_handle_opacity * opacity,
        active_handle_opacity: scroll.active_handle_opacity * opacity,
        interact_handle_opacity: scroll.interact_handle_opacity * opacity,
        dormant_background_opacity: scroll.dormant_background_opacity * opacity,
        active_background_opacity: scroll.active_background_opacity * opacity,
        interact_background_opacity: scroll.interact_background_opacity * opacity,
        ..scroll
    }
}

/// Where a resize gesture started, kept for as long as the grip is held.
///
/// Stored in `Context::data` under the grip's own id, and removed on the frame
/// the drag stops. This is NOT the panel's size living in egui memory (which is
/// exactly what `egui::Resize` was rejected for): the size stays in
/// `PanelNode::size_override`, and this is only the anchor of a gesture that
/// cannot outlive the pointer press.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
struct ResizeAnchor {
    /// Outer size the panel was drawn at when its grip was grabbed, in points.
    size: Vec2,
    /// Pointer position on that same frame.
    origin: Pos2,
}

/// New outer size for a panel whose bottom-right grip is being dragged, clamped
/// to `min_size` per axis.
///
/// The size is `anchor.size` plus the distance the pointer travelled since the
/// grab, on BOTH axes, so the corner keeps the offset from the cursor it had at
/// the grab and a backwards drag narrows the panel exactly as a forward one
/// widens it.
///
/// It is deliberately not `drawn_size + drag_delta()`. That accumulates, and the
/// drawn size is not the requested one: `egui::Frame` allocates its stroke
/// outside the inner margin, so every dragged frame re-added the border to the
/// request and the panel crept `2 * stroke` wider per frame no matter which way
/// the mouse went. Content-driven feedback of that kind is impossible here by
/// construction: nothing but the gesture is read.
///
/// Returns `None` while the pointer has not left the point it grabbed at, so a
/// mere press on the grip does not pin a size the solver may still be
/// converging on.
fn resized_outer_size(anchor: ResizeAnchor, pointer: Pos2, min_size: Vec2) -> Option<Vec2> {
    let moved = pointer - anchor.origin;
    if moved == Vec2::ZERO {
        return None;
    }
    Some(Vec2::new(
        (anchor.size.x + moved.x).max(min_size.x),
        (anchor.size.y + moved.y).max(min_size.y),
    ))
}

/// What the header's context menus asked for this frame, merged over the
/// several move zones one strip carries.
///
/// Every field is raised and never lowered: the grip and the bare background are
/// two widgets showing the SAME menu, so whichever of them the user opened it on
/// must be able to write into one outcome.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
struct HeaderMenuOutcome {
    /// `true` on the frame a drag of one of the move zones started.
    drag_started: bool,
    /// `true` when the menu asked for the program tab's default layout.
    reset_layout: bool,
    /// Where the menu asked this whole panel to be moved.
    move_panel: Option<MoveTarget>,
}

/// Shows the «Переместить в окно →» submenu and returns the destination the user
/// picked, if any.
///
/// Used by BOTH context menus of a panel — a tab caption moves one tab, the
/// header's move zones move the whole panel — so the two can never drift apart.
/// `targets` must be non-empty; the caller hides the whole submenu otherwise.
///
/// `Ui::menu_button` turns itself into a SUBMENU button automatically when it is
/// called inside a menu (`egui-0.35.0/src/ui.rs:2787-2798`), so no `SubMenuButton`
/// is spelled out here. Each entry is wrapped in `Ui::push_id` keyed by its
/// TARGET rather than by its localised label, which is the loop rule of
/// `egui-docs/05-ids-and-i18n.md` §4 and keeps the ids stable across a language
/// switch.
fn move_to_window_submenu(
    ui: &mut egui::Ui,
    targets: &[MoveTargetEntry<'_>],
) -> Option<MoveTarget> {
    let mut chosen: Option<MoveTarget> = None;
    ui.menu_button(t!("widgets.panel_dock.move_to_window"), |ui| {
        for entry in targets {
            let clicked = ui
                .push_id(entry.target, |ui| ui.button(entry.label).clicked())
                .inner;
            if clicked {
                chosen = Some(entry.target);
                // Closes the submenu; egui's menu machinery propagates that to
                // the context menu that opened it
                // (`egui-0.35.0/src/containers/menu.rs`, `SubMenu::show`).
                ui.close();
            }
        }
    });
    chosen
}

/// Rect of the drag handle: the horizontal span it reserved inside the header
/// row, over that row's full height, clamped to the frame's content edge.
///
/// `span` is `(left, right)` as measured by the row's layout cursor. The clamp
/// matters at the solver's [`PANEL_MIN_WIDTH`] floor, where the collapse button
/// and the reserved slot together are wider than the content: a zone reaching
/// past `content_right` would sense drags on the canvas OUTSIDE the panel it
/// belongs to. A narrower grip is still a grip; the zone only becomes empty when
/// the collapse button alone fills the strip, and an empty rect matches no
/// hit-test rather than inverting into one that matches everything.
#[must_use]
fn handle_zone(row: Rect, span: (f32, f32), content_right: f32) -> Rect {
    let (left, right) = span;
    let right = right.min(content_right);
    Rect::from_min_max(
        Pos2::new(left.min(right), row.top()),
        Pos2::new(right, row.bottom()),
    )
}

/// Rect of the header strip: the header row stretched to the frame's content
/// edge, which is what a dragged tab is dropped on.
///
/// The strip spans the frame's inner width rather than only the width the
/// captions happened to use, so the empty space right of the last caption still
/// takes a drop. `content_right` never shortens it below the row's own left
/// edge, so the rect can never invert into one that matches everything.
#[must_use]
fn header_strip_rect(row: Rect, content_right: f32) -> Rect {
    Rect::from_min_max(
        Pos2::new(row.left(), row.top()),
        Pos2::new(content_right.max(row.left()), row.bottom()),
    )
}

/// Bare header background right of the last caption, kept [`HEADER_HANDLE_GAP`]
/// clear of it so a click aimed at a tab cannot start a panel move.
///
/// `content_right` is where the frame's inner content ends, which is further
/// right than the row's own width whenever the captions do not fill the strip.
/// `None` when nothing is left — the captions reach the end, or overflow it —
/// in which case the handle at the head of the strip is the only grab point, and
/// that one is reserved before the captions and therefore always there.
#[must_use]
fn spare_move_zone(row: Rect, content_right: f32) -> Option<Rect> {
    let left = row.right() + HEADER_HANDLE_GAP;
    (left < content_right).then(|| {
        Rect::from_min_max(
            Pos2::new(left, row.top()),
            Pos2::new(content_right, row.bottom()),
        )
    })
}

/// Where inside a tab header the pointer grabbed it, in points from the header's
/// top-left.
///
/// `rect` is the rect the header occupies THIS frame — the caption keeps its
/// slot in the strip for the whole drag, so it is its own reference — and
/// `press_origin` is where the pointer went down.
///
/// `None` when there is no press or when the press started outside the header
/// (another widget's gesture). The caller then falls back to a centred preview
/// instead of inventing an offset.
fn header_grab_offset(rect: Rect, press_origin: Option<Pos2>) -> Option<Vec2> {
    let origin = press_origin?;
    let offset = origin - rect.min;
    (offset.x >= 0.0
        && offset.y >= 0.0
        && offset.x <= rect.width()
        && offset.y <= rect.height())
    .then_some(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two thresholds that decide when a press on a tab caption stops being
    /// a click and becomes a drag are egui's, not ours (see [`CollapsiblePanel::tab_header`]).
    /// This pins them: an egui upgrade that changed either would silently change
    /// the gesture, and the widget has no timer of its own to fall back on.
    #[test]
    fn the_drag_thresholds_come_from_egui_input_options() {
        let options = egui::InputOptions::default();
        assert!(
            options.max_click_dist > 0.0,
            "a zero distance threshold would make every press a drag"
        );
        assert!(
            options.max_click_duration > 0.0,
            "a zero duration threshold would make every held press a drag"
        );
    }

    #[test]
    fn a_press_that_has_not_moved_requests_no_size() {
        let anchor = ResizeAnchor {
            size: Vec2::new(300.0, 200.0),
            origin: Pos2::new(400.0, 500.0),
        };
        assert_eq!(
            resized_outer_size(anchor, Pos2::new(400.0, 500.0), Vec2::new(50.0, 40.0)),
            None
        );
    }

    #[test]
    fn the_corner_follows_the_pointer_on_both_axes() {
        let anchor = ResizeAnchor {
            size: Vec2::new(300.0, 200.0),
            origin: Pos2::new(400.0, 500.0),
        };
        assert_eq!(
            resized_outer_size(anchor, Pos2::new(437.0, 512.0), Vec2::new(50.0, 40.0)),
            Some(Vec2::new(337.0, 212.0))
        );
    }

    #[test]
    fn a_backwards_drag_narrows_and_shortens_the_panel() {
        let anchor = ResizeAnchor {
            size: Vec2::new(300.0, 200.0),
            origin: Pos2::new(400.0, 500.0),
        };
        assert_eq!(
            resized_outer_size(anchor, Pos2::new(360.0, 470.0), Vec2::new(50.0, 40.0)),
            Some(Vec2::new(260.0, 170.0))
        );
    }

    /// The floor is per axis: an axis that hit it must not freeze the other one,
    /// which is what a `Vec2::max` on the whole vector would do.
    #[test]
    fn the_minimum_size_clamps_each_axis_on_its_own() {
        let anchor = ResizeAnchor {
            size: Vec2::new(300.0, 200.0),
            origin: Pos2::new(400.0, 500.0),
        };
        assert_eq!(
            resized_outer_size(anchor, Pos2::new(100.0, 560.0), Vec2::new(120.0, 40.0)),
            Some(Vec2::new(120.0, 260.0))
        );
        assert_eq!(
            resized_outer_size(anchor, Pos2::new(460.0, 100.0), Vec2::new(120.0, 40.0)),
            Some(Vec2::new(360.0, 40.0))
        );
    }

    /// The size is measured from the GRAB, so repeating the same pointer
    /// position must repeat the same answer however many frames pass — this is
    /// the property the old `drawn_size + drag_delta` accumulation lacked.
    #[test]
    fn the_requested_size_depends_only_on_the_gesture() {
        let anchor = ResizeAnchor {
            size: Vec2::new(300.0, 200.0),
            origin: Pos2::new(400.0, 500.0),
        };
        let min = Vec2::new(50.0, 40.0);
        let pointer = Pos2::new(430.0, 520.0);
        let first = resized_outer_size(anchor, pointer, min);
        assert_eq!(first, resized_outer_size(anchor, pointer, min));
        assert_eq!(first, Some(Vec2::new(330.0, 220.0)));
    }

    #[test]
    fn the_grab_offset_is_measured_from_the_header_corner() {
        let rect = Rect::from_min_size(Pos2::new(100.0, 60.0), Vec2::new(80.0, 20.0));
        assert_eq!(
            header_grab_offset(rect, Some(Pos2::new(112.0, 70.0))),
            Some(Vec2::new(12.0, 10.0))
        );
        assert_eq!(
            header_grab_offset(rect, Some(Pos2::new(100.0, 60.0))),
            Some(Vec2::ZERO)
        );
        assert_eq!(
            header_grab_offset(rect, Some(Pos2::new(180.0, 80.0))),
            Some(Vec2::new(80.0, 20.0))
        );
    }

    /// The grip owns a slot at the HEAD of the strip and spans the row's full
    /// height, so grabbing it anywhere between the collapse button and the first
    /// caption moves the panel.
    #[test]
    fn the_handle_zone_spans_its_slot_over_the_whole_row() {
        let row = Rect::from_min_max(Pos2::new(10.0, 40.0), Pos2::new(210.0, 62.0));
        let zone = handle_zone(row, (28.0, 44.0), 300.0);
        assert_eq!(zone.left(), 28.0);
        assert_eq!(zone.right(), 44.0);
        assert_eq!(zone.top(), row.top());
        assert_eq!(zone.bottom(), row.bottom());
    }

    /// The grip never senses past the frame's content edge — beyond it lies the
    /// canvas, not this panel. A strip too narrow for the whole slot keeps a
    /// narrower grip; one that cannot host it at all keeps none, and an empty
    /// rect is what "none" must look like: an inverted one contains everything.
    #[test]
    fn the_handle_zone_never_reaches_past_the_content_edge() {
        let row = Rect::from_min_max(Pos2::new(10.0, 40.0), Pos2::new(210.0, 62.0));
        let narrowed = handle_zone(row, (28.0, 44.0), 38.0);
        assert_eq!(narrowed.left(), 28.0);
        assert_eq!(narrowed.right(), 38.0);
        let none = handle_zone(row, (28.0, 44.0), 24.0);
        assert!(none.width() <= 0.0);
        assert!(!none.contains(Pos2::new(30.0, 50.0)));
    }

    /// The two zones never overlap, and neither of them touches a caption: the
    /// grip ends where the row's captions begin, and the bare space starts a gap
    /// after the row ends.
    #[test]
    fn the_move_zones_are_disjoint_and_clear_of_the_captions() {
        let row = Rect::from_min_max(Pos2::new(10.0, 40.0), Pos2::new(210.0, 62.0));
        let handle = handle_zone(row, (28.0, 44.0), 300.0);
        let spare = spare_move_zone(row, 300.0).expect("bare space is left of the content edge");
        assert!(handle.right() < spare.left());
        assert_eq!(spare.left(), row.right() + HEADER_HANDLE_GAP);
        assert_eq!(spare.right(), 300.0);
        assert_eq!(spare.top(), row.top());
        assert_eq!(spare.bottom(), row.bottom());
        // A point on the caption belongs to neither zone.
        assert!(!handle.contains(Pos2::new(120.0, 50.0)));
        assert!(!spare.contains(Pos2::new(120.0, 50.0)));
    }

    /// Captions that reach — or overrun — the content edge leave no bare space,
    /// and the widget must then sense none rather than an inverted rect.
    #[test]
    fn captions_filling_the_strip_leave_no_bare_move_zone() {
        let row = Rect::from_min_max(Pos2::new(10.0, 40.0), Pos2::new(298.0, 62.0));
        assert_eq!(spare_move_zone(row, 300.0), None);
        assert_eq!(spare_move_zone(row, 250.0), None);
    }

    /// The strip a tab is dropped on reaches the frame's content edge, so the
    /// empty space right of the last caption still takes a drop — and it is the
    /// very rect the driver records for a drop this window could not sense.
    #[test]
    fn the_header_strip_reaches_the_content_edge_and_never_inverts() {
        let row = Rect::from_min_max(Pos2::new(10.0, 40.0), Pos2::new(210.0, 62.0));
        let strip = header_strip_rect(row, 300.0);
        assert_eq!(strip.left(), 10.0);
        assert_eq!(strip.right(), 300.0);
        assert_eq!(strip.top(), row.top());
        assert_eq!(strip.bottom(), row.bottom());
        let clamped = header_strip_rect(row, 4.0);
        assert!(clamped.width() <= 0.0);
        assert!(!clamped.contains(Pos2::new(100.0, 50.0)));
    }

    /// An ordinary panel is never touched by the rule, whatever the other flags
    /// say — the mode is opt-in per drawn tab.
    #[test]
    fn a_panel_that_did_not_ask_for_the_mode_always_shows_its_chrome() {
        assert!(ChromeGate::default().shows_chrome());
        assert!(
            ChromeGate {
                transparent: false,
                ..ChromeGate::default()
            }
            .shows_chrome()
        );
    }

    /// The only state in which a transparent panel hides: nothing is happening
    /// to it and the pointer is somewhere else.
    #[test]
    fn a_transparent_panel_hides_only_when_nothing_points_at_it() {
        let idle = ChromeGate {
            transparent: true,
            ..ChromeGate::default()
        };
        assert!(!idle.shows_chrome());
        assert!(
            ChromeGate {
                pointer_inside: true,
                ..idle
            }
            .shows_chrome()
        );
    }

    /// Every reason on its own is enough. They are ORed because the reasons hand
    /// over to each other mid-gesture — the pointer enters, a drag starts, the
    /// pointer is then pulled outside the panel it is dragging — and a panel that
    /// required all of them at once would blink at each handover.
    #[test]
    fn each_reason_alone_keeps_a_transparent_panel_visible() {
        let idle = ChromeGate {
            transparent: true,
            ..ChromeGate::default()
        };
        for gate in [
            ChromeGate { forced: true, ..idle },
            ChromeGate {
                pointer_inside: true,
                ..idle
            },
            ChromeGate {
                pressed_inside: true,
                ..idle
            },
            ChromeGate { gesture: true, ..idle },
            ChromeGate {
                menu_open: true,
                ..idle
            },
        ] {
            assert!(gate.shows_chrome(), "{gate:?} must show the chrome");
        }
    }

    /// The case the press-origin rule exists for: the user grabs a widget of the
    /// BODY — a scroll handle, a slider — and drags the cursor off the panel.
    /// The pointer is no longer inside and no chrome zone is being dragged, so
    /// the press is the only thing left that says the gesture is this panel's.
    #[test]
    fn a_drag_that_started_in_the_body_keeps_the_panel_visible_off_its_rect() {
        let dragging_off_the_panel = ChromeGate {
            transparent: true,
            pointer_inside: false,
            gesture: false,
            pressed_inside: true,
            ..ChromeGate::default()
        };
        assert!(dragging_off_the_panel.shows_chrome());
    }

    /// A press held inside the panel's rect latches the chrome on, whatever the
    /// pointer does afterwards — including leaving the window, which egui
    /// deliberately does not treat as a release.
    #[test]
    fn a_held_press_that_started_inside_latches_the_chrome_on() {
        let rect = Rect::from_min_size(Pos2::new(100.0, 80.0), Vec2::new(200.0, 150.0));
        assert!(press_started_inside(
            rect,
            true,
            Some(Pos2::new(150.0, 120.0))
        ));
        // The corners belong to the panel too.
        assert!(press_started_inside(rect, true, Some(rect.min)));
        assert!(press_started_inside(rect, true, Some(rect.max)));
    }

    /// No false positives. A press that began on the canvas must never light the
    /// panel, however far the cursor travels over it afterwards — the rule reads
    /// where the button went DOWN, not where the pointer is now.
    #[test]
    fn a_press_that_started_outside_never_lights_the_panel() {
        let rect = Rect::from_min_size(Pos2::new(100.0, 80.0), Vec2::new(200.0, 150.0));
        assert!(!press_started_inside(
            rect,
            true,
            Some(Pos2::new(99.0, 120.0))
        ));
        assert!(!press_started_inside(
            rect,
            true,
            Some(Pos2::new(150.0, 231.0))
        ));
    }

    /// The latch drops exactly when the gesture ends: egui clears `press_origin`
    /// and `down` in the same release event, so neither an idle pointer nor a
    /// stale origin can keep a panel lit.
    #[test]
    fn the_press_latch_drops_when_no_button_is_held() {
        let rect = Rect::from_min_size(Pos2::new(100.0, 80.0), Vec2::new(200.0, 150.0));
        // Released this frame: egui reports no origin and nothing down.
        assert!(!press_started_inside(rect, false, None));
        // Neither half alone may latch: a hovering pointer with no button, and a
        // held button egui could not give an origin for.
        assert!(!press_started_inside(
            rect,
            false,
            Some(Pos2::new(150.0, 120.0))
        ));
        assert!(!press_started_inside(rect, true, None));
    }

    /// A panel showing several captions keeps them readable while it is
    /// invisible: they are the only thing left to say it is there and to switch
    /// tabs with.
    #[test]
    fn several_captions_never_fade_with_the_chrome() {
        assert_eq!(caption_opacity(2, 0.0), 1.0);
        assert_eq!(caption_opacity(5, 0.35), 1.0);
    }

    /// A lone caption has nothing to choose between, so it fades with the rest
    /// and leaves the body alone on the canvas. An empty strip follows the same
    /// branch, which costs nothing: there is no caption to paint.
    #[test]
    fn a_lone_caption_fades_with_the_chrome() {
        assert_eq!(caption_opacity(1, 0.0), 0.0);
        assert_eq!(caption_opacity(1, 0.4), 0.4);
        assert_eq!(caption_opacity(1, 1.0), 1.0);
        assert_eq!(caption_opacity(0, 0.0), 0.0);
    }

    /// A visible panel paints its captions exactly as before the mode existed,
    /// whatever the strip holds — the fade must not become a permanent tint.
    #[test]
    fn a_visible_panel_paints_every_caption_opaque() {
        for count in 0..4_usize {
            assert_eq!(caption_opacity(count, 1.0), 1.0);
        }
    }

    /// The bars are chrome: every one of the six opacities scales with it, in
    /// all three interaction states. `interact_*` matters most — a bar the user
    /// is DRAGGING stays lit however far the pointer leaves the panel, which is
    /// the one state in which an invisible panel could show a bright bar.
    #[test]
    fn the_scroll_bar_opacities_scale_with_the_chrome() {
        let style = egui::style::ScrollStyle::floating();
        let faded = faded_scroll_style(style, 0.0);
        assert_eq!(faded.dormant_handle_opacity, 0.0);
        assert_eq!(faded.active_handle_opacity, 0.0);
        assert_eq!(faded.interact_handle_opacity, 0.0);
        assert_eq!(faded.dormant_background_opacity, 0.0);
        assert_eq!(faded.active_background_opacity, 0.0);
        assert_eq!(faded.interact_background_opacity, 0.0);

        let half = faded_scroll_style(style, 0.5);
        assert_eq!(half.active_handle_opacity, style.active_handle_opacity * 0.5);
        assert_eq!(
            half.interact_handle_opacity,
            style.interact_handle_opacity * 0.5
        );
        assert_eq!(
            half.interact_background_opacity,
            style.interact_background_opacity * 0.5
        );
    }

    /// A visible panel must paint its bars exactly as an ordinary panel does,
    /// or the mode would leave a permanent tint on a hovered panel.
    #[test]
    fn a_visible_panel_leaves_the_scroll_style_alone() {
        let style = egui::style::ScrollStyle::floating();
        assert_eq!(faded_scroll_style(style, 1.0), style);
    }

    /// THE contract of this fade: not one field that decides a SIZE may move.
    /// `allocated_width() == 0` for a floating bar is what keeps a bar that
    /// appears from taking room off the content — and therefore from oscillating
    /// the measurement the panel's own size is derived from.
    #[test]
    fn fading_the_scroll_bars_moves_no_geometry() {
        for style in [
            egui::style::ScrollStyle::floating(),
            egui::style::ScrollStyle::solid(),
            egui::style::ScrollStyle::thin(),
        ] {
            for opacity in [0.0_f32, 0.37, 1.0] {
                let faded = faded_scroll_style(style, opacity);
                assert_eq!(faded.floating, style.floating);
                assert_eq!(faded.bar_width, style.bar_width);
                assert_eq!(faded.floating_width, style.floating_width);
                assert_eq!(
                    faded.floating_allocated_width,
                    style.floating_allocated_width
                );
                assert_eq!(faded.bar_inner_margin, style.bar_inner_margin);
                assert_eq!(faded.bar_outer_margin, style.bar_outer_margin);
                assert_eq!(faded.handle_min_length, style.handle_min_length);
                assert_eq!(faded.content_margin, style.content_margin);
                assert_eq!(faded.allocated_width(), style.allocated_width());
            }
        }
    }

    /// The project runs on egui's default scroll style, and the fade only works
    /// on a FLOATING bar (egui hard-codes both opacities to 1.0 for a solid
    /// one). If a future egui default stopped being floating, the bars would
    /// silently stop fading — this pins the assumption instead.
    #[test]
    fn the_default_scroll_style_is_the_floating_one_the_fade_needs() {
        let default = egui::style::ScrollStyle::default();
        assert!(default.floating);
        assert_eq!(default.allocated_width(), 0.0);
    }

    #[test]
    fn a_press_outside_the_header_yields_no_grab_offset() {
        let rect = Rect::from_min_size(Pos2::new(100.0, 60.0), Vec2::new(80.0, 20.0));
        assert_eq!(header_grab_offset(rect, None), None);
        assert_eq!(header_grab_offset(rect, Some(Pos2::new(99.0, 70.0))), None);
        assert_eq!(header_grab_offset(rect, Some(Pos2::new(112.0, 81.0))), None);
    }
}
