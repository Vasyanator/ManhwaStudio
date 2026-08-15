# Module: src/widgets/panel_dock

## Purpose
The reusable panel system every floating panel of the studio is built from: a data model of how
panels and their tabs are arranged, a layout solver that turns that arrangement into rects, the two
widgets that draw it, and the per-frame driver that ties them together. The design contract is
`dev-docs/dockable_panels_plan.md`; this directory implements it phase by phase.

Current state: **phases 0–6**. The pure layer (`model.rs`, `solver.rs`, `drag.rs`,
`cross_window.rs`), the widget
layer (`tab.rs`, `panel.rs`), the `PanelDock` driver in `mod.rs` including the reorganisation
gestures, the persistence layer (`persist.rs`) and the detached OS windows (`window.rs`). The
production consumers are the three CANVAS program tabs: «Текст», whose eight tabs plus the canvas'
own «Лента» live in seven default panels, «Клининг», whose «Клин» / «Инструменты клина» /
«Выбранный инструмент» / «Быстрый клин найденного текста» join «Лента» in five default panels, and
«Перевод», which declares «Лента» alone.
The dedicated canvas-controls anchor those panels used to hang off is gone with the panel it named;
only its STORED tag survives, decode-only, so an arrangement written by an older build still loads
(see «A retired stored tag must keep DECODING» below).

Sizes measured for a tab are cached PER PROGRAM TAB (`PanelDockState::measured`, keyed by
`AppTab::key()` and then by `TabId`), never by `TabId` alone: «Лента» is ONE tab id declared by
three program tabs that share one state, and a global entry made the width the user dragged it to
in «Текст» size the «Клининг» panel — which pins no `size_override` and therefore takes its size
straight from that cache.

## Architecture
Three layers, the lower two free of GUI state:

```text
PanelDock (mod.rs)   per-frame driver: declarations -> plan -> solve -> draw -> gestures -> write-back
   |  DockLayout + measured sizes                  ^ CollapsiblePanel (panel.rs) draws one panel
   |                                               | PanelTab (tab.rs) declares one tab
   |  solved rects + pointer                       | drag.rs decides where a gesture lands
   v
DockLayout (model.rs)      panel/tab graph, invariants, mutations
   |  anchors + per-panel sizes
   v
solve()  (solver.rs)       -> SolvedLayout: rect + body_max_height + shrunk per panel
```

A panel is a movable frame owning one or more tabs and showing one of them. A panel is either free
(its own `pos` is authoritative) or anchored to another panel or to a side of the host area.
Anchors form a forest; connected components are called *chains* and are laid out and clamped as a
unit.

The solver applies, in order: chain grouping, `DOCK_GAP` placement, propagation from the anchor's
resolved rect, shrinking of what sticks out of the host area, and whole-chain translation of what
shrinking could not resolve. What survives the shrink is handed to the caller as `body_max_height`;
the body scrolls inside it and is never clipped silently.

Shrinking is **responsibility-driven**, on both axes. A chain is measured from its leading member,
and every member whose far edge lands beyond the area's length is relieved by exactly the panels
that place that edge: placement is affine in the sizes, so the derivative of each overflowing edge
with respect to every panel's size is computed exactly (`start_coefficients`) and only strictly
positive contributors join the water-filling pool. Three consequences are contractual:

* a panel standing BESIDE the offender has derivative `0` and keeps its size — the old
  bounding-box deficit collapsed such neighbours to their floor with half the area free next to
  them;
* parallel branches each pay their own full deficit instead of splitting one, so a branching chain
  fits after a single pass and re-solving a settled layout reproduces it (idempotence — without it
  one pixel of resize-grip drag SHRANK the panel);
* a chain too WIDE for the area is shrunk exactly like a chain too tall, because a panel pushed out
  sideways takes its resize grip with it and the user can no longer make it smaller.

Shrinking is also **tiered**. A panel carrying a `size_override` is the LAST to give: the deficit
drains `SHRINK_PRIORITIES` in order, so every content-sized panel of the pool sits on its floor
before a manually sized one loses a point. A content size is a request the widget derived from what
it happened to draw; a manual size is what a user dragged the panel to. Charging them alike made
growing a panel inside a chain that already fills the area *impossible*: the water-filling asks
everyone for the same number of points, and the panel with the most slack is by definition the one
that has just grown, so the whole gain came back off it and the height never changed. The tier is a
priority, not an exemption — a floored tier passes the rest on, and no floor is ever crossed. It
changes nothing about determinism or idempotence: a panel's tier is a property of the layout and is
constant for the whole solve.

### The inversion (plan §4.2)
A call site cannot own a panel, because the LAYOUT decides which panel a tab lives in. So the caller
declares TABS and the dock places them:

```rust
state.ensure_default_layout(AppTab::Typing.key(), typing_default_dock_layout);
let mut cx = TypingDockCx { top_panel, text_overlays, page_idx };   // one per frame
let mut dock = PanelDock::begin(ctx, &mut self.panel_dock, DockArea {
    rect: canvas_rect,                     // where panels may live
    layout_key: AppTab::Typing.key(),      // never a localized title
});
dock.tab(TYPING_PREVIEW_TAB)
    .title(|| t!("typing.preview.panel_heading"))
    .visible(create_mode)
    .min_size(min)
    .initial_size(initial)
    .show(|ui, cx| cx.top_panel.draw_preview_tab_body(ui));   // QUEUED, not drawn
dock.tab(TYPING_LAYERS_TAB)
    .title(|| t!("typing.panel.layers_tab"))
    .show(|ui, cx| cx.text_overlays.draw_layers_tab_body(ui, cx.page_idx));
let out = dock.end(&mut cx);                                  // solve + draw + write-back
```

`show` stores `Box<dyn FnOnce(&mut Ui, &mut C) + 'frame>` and `end(cx)` runs it. A body captures
NOTHING of the caller: it receives the caller's per-frame context `C`, which `end` lends to one
body at a time. That is the point. Capturing borrows directly would only work while no two tabs
needed the same state, and the typing tab breaks that immediately — its preview, params, effects
and actions bodies all need `&mut TypingTopPanelState` while «Слои» needs
`&mut TypingTextOverlayLayer`. Sequential exclusive access through `C` serves both without
cloning a 100-field state or reaching for a `RefCell`.

### Frame model (plan §4.3)
Geometry lags content by one frame, on purpose — a two-pass layout would run every heavy tab body
twice per frame.

1. `end()` solves with the sizes measured LAST frame; a tab drawn for the first time uses its
   declared `initial_size`, then its `min_size`, then `DEFAULT_PANEL_SIZE`.
2. Each body is drawn inside a bounded `ScrollArea`; what the panel reports back is its own
   overhead plus the height its CONTENT measured, and so is the panel's own style-dependent
   overhead (`PanelChrome`).
3. A content height that differs from the height the drawn tab CONTRIBUTED to its panel's request
   by at least one point triggers `ctx.request_repaint()`, so the layout converges within a couple
   of frames.

`egui::Resize` is deliberately NOT used: its size lives in egui memory, which is exactly what forced
the `id_salt`-revision hack in the old typing panels. Sizes live in `PanelNode::size_override`.

## Files and submodules
- `mod.rs`: the public re-export surface, `PanelDockState`, `DockArea`, `PanelDock`,
  `PanelDockOutput`, and the three pure frame-planning helpers (`ensure_declared_tabs`,
  `plan_frame`, `frame_layout`) the driver is built from. Edit when the driver's contract or the
  public surface changes.
- `model.rs`: `TabId`, `PanelId`, `HostId`, `DockEdge`, `PanelAnchor`, `PanelNode`, `DockLayout`,
  `MoveTabOutcome`, `DockModelError`. All layout mutations (insert, remove, re-anchor, move a tab)
  live here, together with `validate()` and `chains()`.
- `solver.rs`: `PanelSizes` (sanitized input map), `PanelChrome` (measured header/frame overhead),
  `solve()`, `SolvedPanel`, `SolvedLayout`, the per-chain shrink (`ChainContext`), and the layout
  constants `DOCK_GAP`, `COLLAPSED_PANEL_HEIGHT`, `PANEL_MIN_CONTENT_HEIGHT`, `PANEL_MIN_WIDTH`,
  `PANEL_MIN_BODY_HEIGHT`, `DEFAULT_PANEL_SIZE`.
- `tab.rs`: widget #1, `PanelTab` — the per-frame declaration builder returned by `PanelDock::tab`.
- `panel.rs`: widget #2, `CollapsiblePanel` — draws ONE panel (header strip, active tab's body,
  resize grip, header drag handle, tab drop zone, and the two context menus) and reports what the
  user did through `CollapsiblePanelOutput` / `TabDrop` / `MoveTargetEntry`. It never touches the
  model and knows nothing about windows: the destinations of the «Переместить в окно →» submenu
  are handed in per frame.
- `persist.rs`: the `PanelLayout` section of `user_config.json` — its serde mirror, the two
  conversions to and from `DockLayout`, and `PanelLayoutWriter`, this section's handle on the
  shared `config_saver::ConfigSaver` writer thread. The ONLY file of this directory that touches
  the disk. Edit it when the stored shape or a compatibility rule changes; the debounce/retry
  policy itself lives in `src/config_saver.rs` and is shared with the `Window` section, so
  changing it there changes it for both.
- `window.rs`: the detached OS windows. `SubWindowNode` (one window's identity and last known
  geometry), the pure tear-out model (`drag_tension`, `DragTension`,
  `DETACH_TENSION_DISTANCE`) and the detach verdict built on it (`detach_trigger`,
  `DragEndContext`, `DetachTrigger`), the lifecycle questions (`next_sub_window_index`,
  `obsolete_sub_windows`, `geometry_changed`), the menu destinations (`MoveTarget`,
  `move_targets`) and the window NAMES the menu and the OS title bar share (`sub_window_name`,
  `sub_window_title`, `move_target_label`), plus the two egui plumbing helpers
  (`sub_window_viewport_id`, `sub_window_builder`). Everything but the last two is pure; the
  naming helpers are pure apart from the catalog lookup.
- `drag.rs`: the reorganisation gestures. `DragSession` (a panel move in flight, owned by
  `PanelDockState`), `DraggedTab` (the drag-and-drop payload of a tab header), the snap search
  (`find_snap`, `panel_snap_candidates`, `SnapCandidate`, `SnapTargets`, `SNAP_DISTANCE`), the
  sibling rule (`resolve_slot`), the two tab-drop questions (`insertion_index`,
  `empty_space_drop`) and the three previews (`paint_snap_preview` for docking,
  `paint_detach_preview` for tearing out, `paint_insertion_marker` for a drop the receiving
  window could not sense). Everything but the painters is a pure function.
- `cross_window.rs`: addressing a gesture that crossed a WINDOW border. `WindowGeometry` (one
  window in the shared monitor frame, plus the conversions into and out of it), the overlap rule
  (`window_at`), the three-way verdict (`address_drop`, `DropAddress`) and what a tab
  (`tab_landing`, `TabLanding`, `PanelDropTarget`) or a whole panel (`panel_landing`) lands on
  inside the receiving window. Entirely pure; the live geometry is fed in by
  `mod.rs::window_geometries`.

### Gestures (plan §4.8)
Two gestures, both decided in `drag.rs` and applied by the driver through the model:

* **Moving a panel.** The header strip reads, left to right: collapse arrow, drag grip, tab
  captions, bare background. The panel is grabbed by the GRIP or by that BARE BACKGROUND — never by
  a tab caption, the collapse button or the resize grip — and both zones also anchor the layout
  context menu; only the grip is painted, so the strip shows one grip wherever it is grabbed. The
  grip's slot is reserved with `Ui::add_space` BEFORE the first caption, so it survives captions
  that fill the whole strip (a panel that cannot be grabbed cannot be reorganised at all) and it
  cannot change the strip's height: in a left-to-right layout that call expands the row's x alone,
  and the strip's height is the panel's measured `PanelChrome`. The gesture DETACHES the panel on
  the first frame (anchor `Free`,
  position taken from the rect it was solved at) and then recomputes its `pos` from the pointer at
  the START of every frame, before the solve, so the panel is laid out under the cursor in the
  frame the cursor moved instead of one frame behind it. While it is in flight the nearest edge
  within `SNAP_DISTANCE` is previewed with a two-point line in the style's selection colour, painted
  through a `Painter` on `Order::Tooltip` so it registers no hitbox. On release the candidate
  becomes the panel's anchor; no candidate means "stay free where it was dropped", and a release
  past the border's resistance (below) means "into a window of its own". A panel's own dependants
  stay anchored to it and travel with it.
* **Moving a tab.** The caption is ONE widget sensing click AND drag, and it carries `DraggedTab`
  itself; `Ui::dnd_drag_source` is not used, see the threshold rule below. While the drag is in
  flight the caption keeps its slot in the strip and a copy of it is PAINTED under the cursor on
  `Order::Tooltip` — painted, not re-parented, because a second `Ui` inside the header row would
  advance the row's auto-id counter and rename every caption after it for the duration of the drag.
  The copy is placed so the point the header was GRABBED at stays under the cursor (requirement 7),
  measured from `pointer.press_origin()` against the rect the header occupies this frame. A header
  strip under the pointer shows an insertion marker and takes the drop at the hovered index; a
  release on BARE dock area gives the tab a panel of its own at the cursor
  (`DockLayout::detach_tab`); a release over anything else cancels the move. A panel emptied by a
  drop is removed by `DockLayout::move_tab` itself.

**A gesture that shares a widget with a click never starts before egui says so.** A tab caption and
the header handle are both sensed as `Sense::click_and_drag()`, which makes egui postpone the
verdict until the press is *decidedly a drag* — moved further than `InputOptions::max_click_dist`
or held longer than `InputOptions::max_click_duration`. Until then the caption does not move and a
release counts as a click. Sensing drag ALONE — which is what `Ui::dnd_drag_source` allocates — is
what made a tab jump to the cursor on the first pressed frame; worse, that source re-parents the
caption into a tooltip layer, so the click target the press had latched onto vanished from the next
frame's widget rects and egui dropped the pending click. Switching tabs by clicking then only
worked when press and release fell inside a single frame. **No hand-rolled distance or time
threshold belongs in this directory**: both numbers are egui's, and `panel.rs` only has a test
pinning that they exist.

### Tearing out: one resistance model for both gestures (plan §4.8)
A drag leaves the window the same way whatever it carries. While the pointer is INSIDE the dock
area everything behaves as described above — the panel follows the cursor, the docking line
appears, a tab lands on a header strip. Step outside and the gesture RESISTS: `window::drag_tension`
measures how far past the area's border the pointer is (`Rect::distance_to_pos`, so a diagonal pull
past a corner counts as one distance), and

* `Inside` — ordinary docking;
* `Resisting { pull }` — outside, but not far enough. The panel does not follow out there: its
  `pos` is written unclamped and the solver's chain clamp pins the DRAWN panel to the border, so
  what grows is the visible gap between the border and the cursor;
* `TornOff` — past `DETACH_TENSION_DISTANCE` (48 pt, twice `SNAP_DISTANCE`), or the pointer is not
  reportable at all (`PointerGone` — the cursor is somewhere this window cannot measure, which is
  by definition further out than any threshold inside it).

The tension carries **no latch**: it is a pure function of where the pointer is now, so coming back
inside restores ordinary docking however far it was pulled before. What the user sees at the moment
it breaks free is `drag::paint_detach_preview` — a DASHED contour, in the same selection colour as
the docking line, around what would fly out: the panel's own rect at the unclamped position, or the
carried tab caption (which is why `DraggedTab` carries `header_size`). Solid line along an edge =
"docks here"; dashed contour = "leaves the window". The docking preview is suppressed while torn
off: the two verdicts are mutually exclusive.

The tension only ever DECIDES on release (`window::detach_trigger`), never mid-gesture — see the
sub-window section for why. **A tear-out rule may never be measured INSIDE the dock area**: edge
docking lives there, a panel snaps to the area's sides from up to `SNAP_DISTANCE` away, and any
"released close to the border" rule would make docking to that border impossible — which is why
such a rule could only ever have been given to the tab gesture and the two gestures would answer
differently at the same place on screen.

### Sub-windows (plan §4.9)
A tab (or a whole panel) can be moved into a real OS window. The panel's `HostId` becomes
`SubWindow(index)`, and `PanelDock::end` draws one `ctx.show_viewport_immediate` per window,
EVERY frame — an immediate viewport exists only while it is shown. Immediate, never deferred:
the deferred form needs `Fn + Send + Sync + 'static`, which the caller's tab bodies (borrowing
the typing tab's hundred-field states) cannot satisfy. Parent and child therefore repaint
together; that is the accepted price and must not be "optimised" away.

Inside a sub-window there is a `CentralPanel` for the neutral background and the same
`draw_host` loop the main window runs — the same panels, the same gestures, no canvas.
`draw_host` reads `ctx` while that window's viewport is current, which
is what makes the pointer, the drawn `Area`s and the drag session per-window without a single
branch on the host.

**Detach detection is window-LOCAL, by necessity.** egui exposes no ready-made global cursor
position, so the dock area's own border is the only ruler a window has for "how far out is the
cursor", and the tension model above is built on it. Two rules, in order:

1. `latest_pos() == None && any_down()` — the cursor left the window while the button was held
   (`PointerGone` clears the position but deliberately keeps the drag alive). Latched, not acted
   on: the latch is cleared the moment the pointer is reportable again, so a brush past the
   border is not a drag-out and an Alt-Tab mid-drag cannot detach anything. A release under this
   latch is `DetachTrigger::PointerLeftWindow`.
2. the release landed further than `DETACH_TENSION_DISTANCE` past the dock area's border
   (`DetachTrigger::PulledPastArea`). This covers the implicit-grab case for free — a platform
   that keeps reporting coordinates outside the window simply reports ones very far outside the
   area — and it is the same rule for a tab and for a panel.

### Moving between windows without a gesture: «Переместить в окно →»
Both context menus of a panel — a tab caption's and the header's move zones' — carry a
`Переместить в окно` SUBMENU (`Ui::menu_button` turns itself into a submenu button inside a menu).
It lists the main window, every existing sub-window by its own name, and «Новое окно»; the host the
panel is drawn in is left out, and an empty list hides the submenu entirely. A tab caption moves
ONE tab, the header moves the WHOLE panel with every tab it holds. The old standalone
«Вынести в отдельное окно» item is gone: it is the submenu's last entry now, and two items doing
one thing is how they drift apart.

**This is the only cross-window move that is not platform-dependent**, and on Wayland it is the
only one that works AT ALL: it reads no pointer, no window geometry and no monitor coordinates, so
none of the assumptions the drag path needs (`inner_rect`, an implicit pointer grab, window
placement) enters it. `DetachTrigger::ContextMenu` names it in the log.

**Where things land depends on the KIND of window that receives them**, because the two kinds are
used for opposite things. The MODEL stands in for the cursor the menu does not have
(`menu_tab_landing`, `menu_panel_slot`):

* **the main window** always gives the newcomer a panel of its OWN, free, in the MIDDLE of its dock
  area (`centered_slot_in_host`). It is the window the user is looking at and it is already full of
  panels: appending the tab to whichever panel happens to be first there buries it behind an
  unrelated caption in a corner of the canvas, and the user has to hunt for what they just moved.
  If the middle is taken, `step_off_occupied` cascades off it — two panels less than
  `AUTO_PANEL_CASCADE_STEP` apart on both axes count as stacked, and the newcomer is never what
  covers a header strip;
* **a sub-window that already holds panels** takes a TAB into the END of the FIRST panel there —
  the oldest one, which is also the one the solver lays out first — through the same
  `apply_tab_landing` a cross-window drop uses, so it is visible the moment the window is raised. A
  sub-window is a small tool window opened FOR panels, so collecting tabs in the one that is there
  is what picking that window asked for;
* **a sub-window with no panels** gives the tab a panel of its own at `free_slot_in_host`, the
  cascade `ensure_declared_tabs` uses, stepped by how many panels are already there;
* **a whole panel** cannot merge into another one, so it only chooses a position: centred in the
  main window, cascaded in a sub-window.

The size the centring is computed against is `menu_subject_size` — the tab's last measurement, or a
panel's pinned size / largest measured tab — and the area is the destination's own dock area as it
was drawn THIS frame (`PanelDockState::host_areas`, written by `draw_host` on every frame, unlike
the gesture-only `frame_hosts`). A destination that drew no dock this frame has no middle, and the
slot degrades to the corner cascade rather than to an invented coordinate. The sibling QUEUE rule
(`drag::resolve_slot`) is deliberately NOT used anywhere here: it resolves ANCHORED slots, and a
panel moved by the menu is anchored to nothing. Mouse DRAGS are untouched by all of this — there the
drop point names the receiver, which is the whole reason the two paths differ.

An emptied source panel is removed by the model itself and an emptied window is closed by
`prune_sub_windows`, exactly as after a drag. The destination window is then raised with
`ViewportCommand::Focus` (a request — egui states it does nothing on Wayland, and nothing depends on
it); a NEW window is not, because a freshly mapped window is focused by the window manager.

**Window names are single-source.** `window::sub_window_name(index)` is `«Окно {number}»` with
`number = index + 1`, and both the submenu entry and the OS title bar
(`window::sub_window_title`, `«{name} — ManhwaStudio»`) are built from it, so the window the user
picks in the menu is the one they can read off the title bar. The number is the window's PERSISTED
index, not its position in a list: closing «Окно 1» leaves «Окно 2» named «Окно 2», and the next
window opened reuses the freed number. The title is rebuilt from the index on every frame the window
is shown and eframe patches the live window with `ViewportCommand::Title` when it differs, so a
language switch or a change of identity needs no invalidation path of its own. Both templates keep
their placeholders under test: a `sub_window_title` value without `{name}` drops the number silently
and leaves every window with the same title — which is how the number went missing once, from a
STALE `locale/<tag>.json` on disk (the disk layer only backfills keys a file LACKS and never updates
a value it already has, `src/locale_store.rs`).

The DnD payload is shared by every viewport of one `Context`, but the drag GESTURE is not
(`Memory::interactions` is a `ViewportIdMap`). So the window that saw the release does NOT take
the payload when it decides "this ended outside": it records the verdict, every other window of
the same pass gets its chance to claim the drop through its own header strip, and only then does
`PanelDock::end` resolve what is left.

### Where a drop that crossed a window border goes (`cross_window.rs`)
A gesture cannot be handed over by hit-testing. The drag gesture does not cross viewports, and a
held mouse button keeps an implicit pointer grab on the window the press started in, so the
window the cursor is actually OVER receives no pointer events at all: `is_being_dragged` is false
there, nothing is hovered, and `dnd_hover_payload` reports nothing. The drop is therefore
addressed by GEOMETRY.

* **The shared frame is physical pixels.** `ViewportInfo::inner_rect` is a window's content rect
  in monitor space divided by THAT window's `pixels_per_point`, so two windows on monitors with
  different scale factors express monitor space in two different units. `WindowGeometry`
  multiplies each rect by its own `zoom_factor * native_pixels_per_point` to put them back into
  the one frame the window manager uses. `RawInput::viewports` carries the info of ALL viewports
  in every pass, so one window can read where the others are (one frame stale for immediate
  viewports, which a continuously repainting drag cannot notice).
* **Lifting and lowering.** `global = (inner_rect.min + local) * ppp`, and the inverse brings a
  monitor point back into a window's own screen coordinates. `to_viewport_points` divides by the
  scale alone, which is what `ViewportBuilder::with_position` consumes.
* **The overlap rule.** Windows overlap and neither egui nor winit exposes their stacking order,
  so the SMALLEST window containing the point wins: one that covers another entirely cannot be on
  top there without making the covered one invisible, and sub-windows are small tool windows
  floating over a large main window. An exact tie goes to the later sub-window, and the main
  window loses to every sub-window.
* **Three branches, and the source window is excluded from the containment test.** A release
  inside another of our windows moves the tab into that window's header strip (at the hovered
  index) or onto its bare dock area (a panel of its own), and a whole panel becomes free-floating
  at the drop point — it does not snap there, because it never followed the cursor in that window
  and no docking preview was shown. A release on the bare desktop opens a window AT THE RELEASE
  POINT. A release on something that takes no tabs — a panel's body, or the receiving window's own
  toolbar — cancels, exactly as the in-window rules do. The source window never claims its own
  drop back: a gesture pulled past ITS dock area's border and released over its own toolbar is the
  tear-out the dashed outline promised.
* **The decision is taken at the end of the frame** (`apply_frame_detaches`), when every window
  has drawn and recorded its area, its panels and their header strips (`HostRecord`). That is what
  lets a window accept a drop it never saw, whichever order the windows drew in.
* **Feedback comes from the driver, in the receiving window.** While the cursor is over another of
  our windows, that window paints the insertion line or the dashed "a panel would appear here"
  contour (`paint_cross_window_feedback`), and the source window suppresses its own tear-out
  outline — the two verdicts are mutually exclusive. A window that does not own the pointer is
  additionally drawn with `accepts_drop(false)`, so it cannot claim a drop through a header strip
  the user is not actually over.
* **The shared-frame pointer is published, not queried.** Only the window holding the grab can see
  the cursor, so it writes the lifted position into `PanelDockState::drag_pointer_global` and the
  others read it. For the final ADDRESS a stale reading is refused: the source's own reading wins,
  and the fallback is another window's reading from THIS frame only — an older one would address
  the drop against wherever the cursor happened to be earlier in the gesture.
* **Wayland has none of this and says so.** `inner_rect` / `outer_rect` are always `None` there,
  `outer_position()` fails below egui, and `with_position` is ignored, so no shared frame exists
  at any level. `WindowGeometry` is then never built, every window keeps answering for itself, and
  the behaviour degrades to exactly what the tension model already did: in-window moves, tear-out
  past the border and the context-menu item all keep working, a torn-out window lands wherever the
  compositor puts it, and a tab dragged onto an existing detached window opens a new one instead
  of moving into it. The reason is logged once (`resolve_window_placement`). Nothing invents a
  coordinate.

**Lifecycle.** A window closes when it holds no panel in ANY layout (requirement 10) or when the
user presses its close button (`ViewportInfo::close_requested`); both paths return its panels to
the main window, because a tab that lives in no host is a tab the program can never show again.
A window that is merely empty in the program tab currently drawn stays OPEN and grey
(requirement 11) — and on a frame where no program tab of this dock draws at all,
`PanelDockState::show_idle_sub_windows` keeps the viewports alive. That call must happen exactly
once per frame and only when `PanelDock::end` did not run, because showing one viewport twice in
a pass renders it twice.

**Platform honesty.** A new window is placed at the release point, lifted into the shared frame
and lowered back into viewport points with the source window's scale — available on
X11/Windows/macOS, `None` on Wayland. There the position is simply not passed, the compositor
decides, and the reason is logged once.
`ViewportCommand::StartDrag` is deliberately NOT sent: the detach fires when the gesture ENDS,
so the mouse button is already up and there is no drag for the window manager to carry. The
stored position is applied ONCE, on the frame the window is created; re-asserting it every frame
would fight the user and the window manager over a window they are allowed to move.

**The sibling rule** (`resolve_slot`) is what keeps the two gestures from producing an unusable
layout: two panels with the same anchor land on exactly the same rect, the second covering the
first, and the buried one cannot be reached — not even to drag it back out. So the gesture QUEUES
instead: while the rect an anchor would produce overlaps a panel that is already there, the dragged
panel is re-anchored to THAT panel, on the side the queue grows along (the same side for an anchor
that places a panel outside a target; downwards/rightwards for a `ViewportEdge`, whose own side
points at the area). Occupancy is decided from the rects, not by comparing anchors: two different
anchors can name the same spot. The dragged panel and its dependants are never occupants, because
anchoring to a dependant closes a cycle.

### Persistence (plan §5)
The arrangement lives in the self-versioned `PanelLayout` section of `user_config.json`, keyed by
`AppTab::key()` inside it and by the `TabId` literal inside a panel. Three rules matter:

* **One writer, off the GUI thread.** `PanelLayoutWriter` (owned by the application, not by
  `PanelDockState`) debounces a burst and performs ONE write through
  `config::update_user_config_file`, the locked read-modify-write border of the file. It is fed by
  `PanelDockState::take_dirty_layouts`, polled once per frame right after the tab draws, because
  `dirty` is raised on every frame a panel drag advances. `MangaApp::on_exit` polls once more and
  flushes. **The writer is the last owner of a snapshot** — polling clears `dirty`, so a snapshot
  the writer drops exists nowhere else. A failed write therefore keeps its snapshot and retries it
  on a doubling, capped backoff; a newer snapshot is folded OVER the held one by the same per-tab
  rule the debounce uses; the shutdown makes the final attempt; and an arrangement that cannot be
  written even then is logged as lost, never dropped silently.
* **Restore beats default.** `install_persisted_layouts` runs before the first frame, so
  `ensure_default_layout` finds the key taken and never builds over it. Restoring does not raise
  `dirty` — the config is not a user change.
* **A retired stored tag must keep DECODING.** `StoredAnchor` is internally tagged
  (`tag = "kind"`) and carries no `#[serde(other)]`, and the whole section is decoded by ONE
  `serde_json::from_value`, so a tag this build does not know fails the ENTIRE section: every
  program tab falls back to its default arrangement and the first dirty write makes that permanent
  (`#[serde(default)]` does not help — it covers a MISSING field, not one that fails to parse, and
  the section version does not change, so the `NewerVersion` refusal does not fire either). The
  standing example is `StoredAnchor::CanvasControls`, written while the canvas' controls were an
  anchor rather than the «Лента» panel: it is decoded to `Free` at the panel's stored `pos`, which
  is where the panel was last DRAWN under that anchor, since the driver refreshes `pos` from the
  solved rect every frame; it is never encoded again. That is the whole guarantee — a stored panel
  carrying no `pos` (or a non-finite one) lands at the host area's ORIGIN, like any other
  position-less stored panel. Retiring such a tag for real needs a section version bump and a
  migration, never a plain deletion.
* **Nothing wedges the layout.** A stored tab this build no longer declares is dropped; a declared
  tab the file does not mention is re-created by `ensure_declared_tabs`; a malformed section, an
  unusable stored layout (a tab in two panels, an anchor cycle) or a section from a NEWER schema
  version all degrade to the default arrangement, and a newer section is additionally never
  overwritten. `PanelId`s are not stored at all: panels are written in order and an anchor target
  is that order's index, so a load renumbers them and rewrites the anchors.

**Sub-windows in the file.** They live in the section's own `sub_windows` list (`index`, outer
`pos` — absent where the platform does not report one — and inner `size`), and a panel addresses
one through its `host`. The list is GLOBAL, not per program tab, so the writer REPLACES it while
merging the layouts per key. **That question is settled: there is exactly ONE dock state per studio
window** (`MangaApp::panel_dock`), lent to whichever program tab draws, so the writer has a single
feeder and the list has a single owner. A panel naming a window the list does not describe comes
back to the main window, and a window no restored layout puts a panel in is not opened.

**Constraint for phase 8** (not solved here — do not build machinery for it in advance): the writer
merges layouts per key but REPLACES the global `sub_windows` list, while `layouts_from_user_settings`
decodes only the keys present in the caller's defaults slice. A stored program-tab key the running
build does not pass in that slice therefore keeps its layout in the file but loses its sub-window
association on the next write. Unreachable today — all three canvas keys are in the slice, and a
build that starts hosting the dock in another program tab has to add it there anyway. Phase 8 must
either keep undecoded keys addressable or move the window list under the per-key merge.

**Reset.** The header's context menu — below the «Переместить в окно →» submenu — restores the
program tab's default layout. `PanelDockState`
keeps the `fn() -> DockLayout` every caller passes to `ensure_default_layout` precisely so it can
rebuild one without routing the request back through the caller; that is also why the builder is a
plain `fn` and not a closure.

## Contracts and invariants
- **Both widgets are mandatory.** Every floating panel of the studio is built from
  `CollapsiblePanel` + `PanelTab`. No new `Area + Frame::popup` with a hand-rolled collapse arrow,
  and no bare `egui::Window` used as a panel.
- **One dock state per studio window, and it is the APPLICATION's.** `MangaApp` owns it and lends a
  `&mut` to the program tab that draws; a tab may not keep one of its own. Three global concerns
  forbid splitting it per tab: a sub-window's `ViewportId` is derived from an index minted by a
  PER-STATE counter (two states would show two immediate viewports under one id in a single pass),
  the persisted `sub_windows` list is one global list a second feeder would overwrite, and
  `install_persisted_sub_windows` / `prune_sub_windows` judge a window against the layouts of THAT
  state, so several states would each drop the others' windows. The layouts of all program tabs
  coexist in one state anyway — they are keyed by `AppTab::key()`.
- **The dock state must be its own borrow.** `PanelDock::begin` borrows `PanelDockState` mutably for
  the whole frame, so it may not be part of the per-frame context `C` either. Keep it disjoint from
  every field the bodies touch — that is what makes the deferred API compile at the call site. A
  lent-in parameter (`TypingDrawParams::panel_dock`) satisfies this by construction, since it is
  disjoint from the callee's `self`.
- **Tab bodies reach caller state only through `C`.** A body is `FnOnce(&mut Ui, &mut C)`; `end`
  hands it the context and runs it, one body at a time. Bodies must not capture caller state
  directly — several tabs of one frame legitimately need the same `&mut`, which no set of captured
  borrows can express.
- **The solver is a pure function.** `solve()` holds no egui state, performs no I/O and no logging,
  and has no interior mutability. Same inputs => same output, always.
- **No egui runtime types below `mod.rs`.** `Pos2` / `Vec2` / `Rect` are used as plain geometry;
  `egui::Context`, `egui::Ui` and `egui::Memory` must never appear in `model.rs` or `solver.rs`,
  because that is what keeps both unit-testable without a window.
- **Model invariants** (enforced on every mutation, checkable via `DockLayout::validate`): unique
  panel ids; every panel holds at least one tab, without duplicates, and its `active_tab` is one of
  them; a `TabId` belongs to exactly one panel; `PanelAnchor::Panel` targets an existing panel of
  the same host and never closes a cycle. A panel emptied by `move_tab` is removed by the model
  itself and reported in `MoveTabOutcome`; removing a panel re-anchors its dependants to the
  removed panel's own anchor.
- **No `&mut PanelNode` leaves the model.** Mutations go through `insert_panel` / `remove_panel` /
  `move_tab` / `set_anchor`, the targeted setters (`set_panel_pos`, `set_size_override`,
  `set_collapsed`, `set_active_tab` — none of which can break an invariant, so only an unknown
  panel is an error), or `edit`, which validates and ROLLS BACK. A handed-out node put every
  invariant at the consumer's mercy: clearing its `tabs` leaves a panel that can never be drawn and
  never leaves the layout (the driver keeps re-creating a panel for the orphaned tab), and reusing
  a `PanelId` gives two panels the same egui `Id`. An invalid layout is therefore unconstructible
  outside tests, which is what `DockLayout::from_panels_unchecked` (test-only) exists for.
- **Positions are refreshed from the solve, and do not dirty the state.** `PanelNode::pos` is
  authoritative only while the anchor is `Free`, but it is what the model falls back to whenever an
  anchor stops resolving — which happens on ordinary frames: `frame_layout` drops a panel with
  nothing to draw and hands its OWN anchor down, so a dependant of a hidden FREE root becomes free
  itself; a panel whose anchor target lives in another host is laid out free as well; and a panel
  restored from a stored anchor this build no longer has (`persist::decode_anchor`) arrives free.
  `PanelDock::end` therefore writes every solved rect's origin back
  (`write_back_positions`), so a panel falls back to where it was last drawn instead of jumping into
  the area's corner. It is a derived value, so it must never raise `dirty` — that flag is
  persistence's signal that the USER changed something.
- **`min_size` is a shrink floor, not a request.** A panel is laid out at the size its content
  measured; the declared minimum only stops the shrink step. Raising the request to it instead made
  a short panel reserve height it does not draw, leaving a hole between it and the panel docked
  below. The only bound that does raise a request is the physical one — `PANEL_MIN_WIDTH` and one
  measured header plus `PANEL_MIN_BODY_HEIGHT` — because `CollapsiblePanel` cannot draw a smaller
  frame, and a solved rect smaller than the drawn panel would be overlapped by its own neighbour.
- **A panel is as big as its LARGEST tab.** `plan_frame` requests the component-wise MAXIMUM over
  every tab the panel SHOWS this frame — its measurements, or what the caller declared for a tab
  that has never been drawn (`initial_size`, else `min_size`) — and takes the maximum of the
  declared minimums too, because the panel has to satisfy all of them at once. Sizing a panel from
  its ACTIVE tab made it jump on every tab click. A tab that has neither been drawn nor declared a
  size contributes nothing rather than an invented number; it joins the maximum with its real size
  on the first frame it is shown, at the cost of the one extra frame every first measurement costs.
  The smaller tabs are then STRETCHED into the panel — see the body rule below.
- **A measurement is compared against what the DRAWN TAB contributed.** `PanelPlan::active_request`
  is the size the active tab put into the panel's request; the drawn measurement is compared against
  that, and a tab that contributed nothing is always news. Two things it must not be: the panel's
  `assumed_size` (the request is the maximum over the tabs, so a tab smaller than its panel would
  differ from it every frame and repaint forever), and `desired` alone (a tab that declared no size
  at all has no entry there, so the comparison was with itself, the repaint was skipped, and the
  panels docked under it stayed overlapped until some unrelated event woke egui up). Only the HEIGHT
  is compared: a width difference only ever means "the solver decided otherwise", and re-solving
  reproduces it.
- **Geometry contracts.** `pos` is stored relative to the host area's top-left. `align` / `along`
  are fractions in `0.0..=1.0` of the free travel along the shared side. `DockEdge` means "outside
  the target, next to this side" for a `Panel` anchor and "inside the area, flush with this side"
  for `ViewportEdge`. Attached panels always sit exactly `DOCK_GAP` away.
- **Sizes are outer sizes**, header and frame margins included. The header overhead is MEASURED,
  not assumed: `CollapsiblePanel` reports a `PanelChrome` (collapsed outer height, and the overhead
  an expanded panel spends before its body) on every drawn frame, the driver stores the latest one
  in `PanelDockState`, and `solve` is given it. `COLLAPSED_PANEL_HEIGHT` is only the nominal
  fallback for the very first frame — the real header is style-dependent and roughly 12 pt taller,
  and a chain laid out on the nominal value overlaps its own panels. A collapsed panel is exactly
  `chrome.collapsed_height` tall, never shrinks, and reports `body_max_height == 0.0`.
  `CollapsiblePanel` still derives the body's own budget from the solved RECT minus the header it
  drew THIS frame **and minus the frame's border**, so a style change cannot make the drawn panel
  exceed the solved rect: `egui::Frame` places its stroke outside the inner margin, so a budget that
  ignores it leaves the panel one stroke taller than the rect it was solved at. A panel's WIDTH is never re-measured from what was drawn: the widget
  reports the width it was GIVEN (otherwise the frame margin would join the request every frame and
  the panel would creep wider), and the driver stores the width the panel ASKED for — storing the
  given one would turn a width the solver shrank to fit a narrow area into the panel's own request,
  and the panel would never widen again when the area does.
- **The body FILLS its budget and scrolls both axes; the measurement comes from the CONTENT.** One
  rule in three parts, and it only holds as a whole:
  - the body is a `ScrollArea::both` with `VisibleWhenNeeded` bars (a bar appears only when the
    content does not fit) and `auto_shrink([false, false])`, so a tab smaller than its panel is
    stretched into it instead of hugging its content in a corner. `min_scrolled_width/height` are
    forced to `0`: egui's 64 pt default is above both bounds a panel may be SOLVED at
    (`PANEL_MIN_WIDTH`, `PANEL_MIN_BODY_HEIGHT`), and a body bigger than its panel overlaps the
    neighbour one gap away;
  - horizontal overflow SCROLLS. With the horizontal axis disabled egui expands a scroll area to fit
    its content (`(false, false)` in `scroll_area.rs:1182-1188` means "expand"), which silently made
    the drawn frame wider than the rect it was solved at;
  - a bar cannot start a size oscillation. The content is always laid out for the viewport, not for
    an infinite strip (`content_max_size = inner_size`, `scroll_area.rs:784-796`), so enabling the
    horizontal axis does not change what the content reports; and the project's scroll style is
    egui's default FLOATING one, whose `allocated_width()` is `0` (`style.rs:639-657`), so a bar
    that appears takes no room from the content and cannot change the measurement that made it
    appear. A style with solid bars would reintroduce that coupling — it is egui's own hysteresis,
    but this is the place it would show up first;
  - and what the widget reports back is `its own overhead + ScrollAreaOutput::content_size.y` —
    never the drawn height. This is what makes the first two safe: the moment the drawn height is
    fed back, a shrink, a manual size or a bigger sibling tab becomes this tab's own request and the
    panel can never get smaller again. Same trap as the width rule above, same answer.
- **A panel states its own height, or it can never grow.** An `egui::Area` builds its `Ui` with the
  size the area measured on the PREVIOUS frame as `max_rect` (`area.rs:610`, `:666`), and a
  `ScrollArea` never allocates past `available_rect_before_wrap()`. Without an explicit
  `ui.set_height(solved height)` the body is capped at the height the panel already had, and
  NOTHING can make a panel taller than it once was — not the solver, not a manual resize, not a
  bigger tab. The width was never affected only because `set_width` states it outright. Collapsed
  panels are exempt: their height is one measured header and nothing else.
- **A resize is a GESTURE, and nothing drawn may feed back into it.** While the grip is held,
  `panel.rs` computes the requested size from the panel's size and the pointer position captured on
  the frame the grip was GRABBED (`ResizeAnchor`, kept in `Context::data` for the duration of the
  drag and dropped when it stops) plus how far the pointer has travelled since. It must never be
  `drawn_size + drag_delta()`: that accumulates, and the drawn size is not the requested one unless
  the widget is exact to the point — `egui::Frame` allocates its stroke OUTSIDE the inner margin, so
  the old accumulation re-added `2 * stroke` to the width and `1 * stroke` to the height on every
  dragged frame. The panel ran away sideways at ~120 pt/s while the height looked frozen, because a
  vertical chain that already fills the dock area has every over-request taken back by the shrink
  step. The drawn rect now matches the solved rect exactly (the frame's border is charged to the
  content width and to the body's height budget), and the gesture is authoritative regardless.
- **Declaration rules.**
  - A tab declared for the first time (owned by no panel of the layout) gets its OWN new panel. It
    is never appended to an existing panel: merging unrelated tabs behind the caller's back is not
    undoable without a drag, while a lone panel can always be docked onto another one.
  - **Where that panel goes is asked of the program tab's DEFAULT layout first**
    (`PanelDockState::ensure_default_layout` keeps the builder). A user with a stored arrangement
    never runs that builder — restore beats default — so a tab a new build adds reaches
    `ensure_declared_tabs` for every existing user on the first launch after the update, and a
    cascade from the area origin drops a brand-new panel into the middle of their canvas, on top of
    what they arranged. The default already says where the tab belongs, so its anchor, position and
    `size_override` are reused. A `Panel` anchor is re-addressed by the TAB the default's target
    holds, because a `PanelId` of the default means nothing in a restored layout. Only when the
    default does not know the tab, or its anchor cannot be resolved here (the target tab is absent,
    or lives in another window), does the panel fall back to the free cascade by
    `AUTO_PANEL_CASCADE_STEP` from the area origin.
  - **A seeded panel never buries a live one.** The default answers for the whole default PANEL
    while the seeded panel holds the new tab ALONE, so the natural way to add a tab — putting it
    into an existing default panel — hands the newcomer that panel's anchor and pinned size, i.e.
    exactly the rect the live panel holding the older tabs occupies. It has the highest id, is drawn
    last, and covers it completely (the cascade this replaced always left a corner visible). So an
    anchored placement QUEUES behind whoever already holds the slot (`free_anchor_slot`, on
    `drag::queue_edge` so a queue grown here reads like one grown by a drag) and a free one steps off
    what stands at its position (`step_off_occupied`). Occupancy is decided by comparing ANCHORS,
    not rects: `drag::resolve_slot`'s geometric test needs the frame's solved rects and this runs
    before the solve, on panels that have never been laid out. That answers the exact coincidence
    seeding produces, which is the case that buries anything.
  - **Seeding order is not declaration order.** A default anchor is resolved against the LIVE
    layout, so a tab hanging off another newly declared tab must be created after it; walking the
    declared tabs directly made an arrangement depend on the order two `dock.tab(..)` calls happen
    to be written in. `seeding_order` walks each new tab's default anchor chain first and seeds
    roots before dependants.
  - `visible(false)` keeps the tab's slot: its header is not shown, and a panel whose tabs are all
    hidden is not drawn — but neither is deleted. Deletion is a user action, never a visibility
    consequence.
  - A tab present in the layout but NOT declared this frame (another program tab's, or one the
    caller skipped) is treated exactly like a hidden one.
  - A panel with nothing to draw is dropped from the layout the SOLVER sees for that frame, and its
    dependants inherit its own anchor for the frame (`frame_layout`, built on
    `DockLayout::remove_panel`). The chain closes over the hole instead of reserving space where
    nothing is drawn: hiding «Превью текста» in edit mode lifts «Действия/Слои» into its place. The
    STORED layout is never touched, so the panel finds its place again when it comes back.
  - The drawn tab is the panel's `active_tab` when that one is showable, else the first showable
    tab. A hidden active tab is NEVER overwritten in the model — visibility is not a user choice.
  - Declaring one `TabId` twice in one frame keeps the first declaration and logs a warning.
- **Ids never come from a caption.** Every egui `Id` in `panel.rs` derives from the layout key plus
  `PanelId` / `TabId` literals (`egui-docs/05-ids-and-i18n.md` §2, `dev-docs/i18n_exclusions.md`).
  Captions and tooltips come from `t!` and are added to all five locale files at once.
- **A `PanelDockOutput` answers for the MAIN window, and it is host-aware so it cannot answer for
  anything else.** One output is filled by EVERY window of the dock — `PanelDock::end` hands the
  same `&mut` to the main window's `draw_host` and to every sub-window's — and a sub-window's rect
  is in THAT window's frame, whose dock area starts near the origin. Every rect is therefore
  recorded together with its `HostId`, and `panel_rect` / `tab_rect` / `drawn_panels` / `is_empty`
  report main-window panels ONLY: a detached panel reads as "not on screen", which is what a caller
  anchoring other main-window UI to a dock panel (translation's detector edit boxes) or occluding
  main-window pointer input with the rects (cleaning's `panel_rects`) has to do with it anyway.
  Handing a sub-window rect out unlabelled put an invisible dead zone in the main window's top-left
  corner and drew anchored UI in the wrong place. A sub-window's geometry is deliberately not
  exposed: nothing needs it, and the day something does, it must ask for it by host.
- **A window never claims a drop the shared frame gave to another one.** `accepts_drop(false)`,
  the suppressed tear-out preview and the forced `PendingTabDrop`/`PendingPanelDrop` are one rule
  in three places: while `cross_window::window_at` names a window other than this one, this window
  neither paints feedback nor consumes the payload. Where no shared frame exists (Wayland) every
  window owns its own pointer again, and a cursor out on the bare DESKTOP never takes the gesture
  away from the window it started in — that is the tear-out the tension model exists for.
- **A sub-window is shown every frame or it ceases to exist.** `PanelDock::end` shows them while
  a program tab of this dock draws; `PanelDockState::show_idle_sub_windows` shows them on every
  other frame. Exactly one of the two per frame.
- **Panels stay on `Order::Foreground`.** Canvas input gating is z-order based
  (`crate::input_util::pointer_over_floating_area`, `src/input_util.rs`), so a panel drawn on a
  lower order would stop shielding the canvas beneath it.
- **Total, panic-free solving.** Non-finite or negative sizes and positions are sanitized to `0.0`
  on the way in, a degenerate or inverted `area` collapses onto its `min` corner, and a chain that
  cannot fit even at its floors is returned aligned to the area's top-left with `shrunk: true`.
- **`dirty` means the USER changed the layout.** Every gesture raises it — a move (on every frame
  it advances), a dock, a tab move, a new panel from a drop, a collapse, a resize, a tab switch, a
  reset — and nothing derived does: the per-frame position write-back explicitly must not, and
  neither does restoring a layout from the config. The persistence writer is driven off this flag,
  so a flag raised every frame would make it useless.
- **A program tab's default layout names every tab it can declare.** `persist.rs` resolves a stored
  tab key against the default layout's tab set, so a `TabId` the default does not hold would be
  dropped from the user's arrangement on every load. Adding a tab to a program tab therefore means
  adding it to that tab's `ensure_default_layout` builder too.
- **A tab has no close affordance.** A tab can only be MOVED (settled with the user, plan §9.1), so
  a panel is emptied only by moving its last tab away — which is exactly when the model deletes it.
- **The widgets never mutate the layout.** `CollapsiblePanel` reports gestures; the driver applies
  them through `set_anchor` / `move_tab` / `detach_tab` / the targeted setters, all of which refuse
  an invariant breach. That is why a gesture can be tested without a window: the decision is a pure
  function and the application is a checked model call.
- **The whole subsystem is cross-target.** The web port draws this same interface, so every file
  here must keep compiling for `wasm32-unknown-unknown` (`cargo +nightly wcheck`): no native-only
  dependency, no `std::fs`, no `cfg(not(wasm32))` gate on anything a caller needs. Config reads and
  writes go through `crate::config`, which owns the storage abstraction. `thiserror` (used by
  `DockModelError` / `PanelLayoutError`) is a SHARED dependency in `Cargo.toml` for exactly this
  reason — moving it back under `cfg(not(target_arch = "wasm32"))` breaks the web build.

## Editing map
- To change what a panel *is* (fields, tabs, hosts) or how the graph may be mutated, edit
  `model.rs` and extend its invariant tests.
- To change placement, gaps, clamping or the shrink policy — including WHICH panels are shrunk
  first (`ShrinkPriority` / `SHRINK_PRIORITIES`) — edit `solver.rs`; every rule there is covered by
  a test asserting the contract, not the implementation.
- To change how big a panel asks to be (the maximum over its tabs, what an unmeasured tab
  contributes, which minimum applies), edit `plan_frame` / `tab_request` in `mod.rs`.
- To change how a panel LOOKS (header, collapse arrow, scroll behaviour, resize grip), edit
  `panel.rs`.
- To change what a caller may declare about a tab, edit `tab.rs` and the `TabMeta` it fills in
  `mod.rs`.
- To change the frame model (which sizes are fed to the solver, which panels are solved, which tab
  is drawn, when a repaint is requested), edit `plan_frame` / `frame_layout` / `PanelDock::end` in
  `mod.rs`. All planning helpers are pure and unit-tested; keep new decisions inside them rather
  than inside the drawing loop, so they stay testable without a GPU.
- Constants (`DOCK_GAP`, `COLLAPSED_PANEL_HEIGHT`, `PANEL_MIN_CONTENT_HEIGHT`, `PANEL_MIN_WIDTH`,
  `PANEL_MIN_BODY_HEIGHT`) are single-source: add new ones next to them in `solver.rs` rather than
  scattering literals over the drawing code. The last two are shared with `panel.rs` on purpose —
  the solver floors every request at what that widget can draw, and a private second copy is
  exactly how the drawn panel starts overflowing its solved rect again.
- To change what is stored, how a stored layout is repaired, or when it is written, edit
  `persist.rs`; bump `PANEL_LAYOUT_SECTION_VERSION` for a shape change and keep
  `config::user_config_defaults()` — which reads the constant — in step.
- To change where a drop that crossed a window border goes — how a monitor coordinate is built,
  which window claims an overlapped point, what a tab or a panel lands on inside the receiving
  window — edit `cross_window.rs`; every rule there is a pure function with a test. To change how
  the live geometry reaches it, or when the verdict is applied, edit `window_geometries` /
  `apply_addressed_tab_drop` / `apply_addressed_panel_drop` in `mod.rs`. To change what the
  receiving window SHOWS while the cursor is over it, edit `paint_cross_window_feedback` there and
  the painters in `drag.rs`.
- To change when a drag becomes a detach — how much resistance the border offers
  (`DETACH_TENSION_DISTANCE`), what counts as torn off (`drag_tension`), which release opens a
  window (`detach_trigger`) — or what a sub-window looks like and how one is placed and
  closed, edit `window.rs` for the DECISIONS (all pure and unit-tested) and
  `show_sub_windows` / `observe_sub_window_geometry` / `apply_frame_detaches` in `mod.rs` for the
  egui plumbing.
- To change what the «Переместить в окно →» submenu OFFERS (which windows, in which order, under
  which name), edit `move_targets` / `move_target_label` / `sub_window_name` in `window.rs` — pure
  and unit-tested. To change where a menu move LANDS, edit `menu_tab_landing` / `menu_panel_slot` /
  `centered_slot_in_host` / `step_off_occupied` / `menu_subject_size` / `free_slot_in_host` /
  `move_into_existing_host` / `apply_menu_move` in `mod.rs`. To change how the submenu is drawn,
  edit `move_to_window_submenu` in `panel.rs`. Never derive an entry's egui id from its localised
  label; the entry is keyed by its `MoveTarget`.
- To change how a gesture DECIDES (snap distance, target priority, the sibling rule, where a
  dropped tab lands), edit `drag.rs`; every rule there is a pure function with a test. To change how
  a gesture is SENSED (which pixels grab the panel, what the insertion marker looks like), edit
  `panel.rs`. To change when it is applied, edit `advance_panel_drag` / `begin_panel_drag` /
  `apply_panel_drop` / `apply_tab_drop` in `mod.rs`.
- To change where a NEWLY DECLARED tab's panel lands — what the default is asked, how a slot that
  is already taken is answered, in which order the new panels are created — edit
  `default_placement` / `default_anchor_tab` / `free_anchor_slot` / `seeding_order` /
  `ensure_declared_tabs` in `mod.rs`; all five are pure and unit-tested.
- The solver still lays two panels with an identical `target` + `edge` + `align` on top of each
  other — it is a total function of whatever layout it is given. Three things guarantee the user
  never sees that, one per writer of anchors: the sibling rule in `drag.rs` for a gesture,
  `free_anchor_slot` for a panel seeded from the default, and the default layouts themselves, which
  give conditional panels distinct anchors by hand.
