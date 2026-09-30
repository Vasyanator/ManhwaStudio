/*
File: crates/ms-launcher/src/new_project/batch_processing/canvas.rs

Purpose:
Interactive egui node-graph canvas with pan, zoom, node rendering, and connection dragging.

Main responsibilities:
- Own the canvas transform: `CanvasState::world_to_screen` / `screen_to_world` /
  `world_to_screen_len` / `world_to_screen_size` / `screen_to_world_size` are the ONLY places
  that apply `pan`/`zoom` to a model-space position or extent. The grid, the node placement
  and hit rect, the node drag delta, the spawn point of a new node and the pointer-anchored
  zoom all go through them. Constants painted at a fixed apparent size — font sizes, margins,
  corner radii, stroke widths, the socket radius — are per-widget `Style`/decoration scaling
  and are multiplied by `zoom` where they are used.
- Own the canvas ground and grid: the canvas fills its own rect before painting anything
  (`visuals.panel_fill` is transparent launcher-wide) and draws a two-level grid anchored in
  GRAPH space, so nodes and grid translate and scale together.
- Render all graph nodes as styled boxes with sockets using egui Painter
- Render editable node parameter controls directly inside the node body, using the project's
  `Wheel*` widgets (`egui-docs/04-widgets.md` §0.2) rather than stock egui
  `ComboBox`/`DragValue`
- Draw Bezier curves for exec and data connections, behind the node boxes
- Drive every pointer gesture (pan, zoom, node drag, node selection, socket-to-socket
  connection drag) from egui `Response`s, never from raw `InputState::pointer` hit-testing
- Return canvas-level actions (connection dropped, nodes deleted, file dialog requested) to
  the caller

Key structures:
- CanvasState     — persistent UI state (pan, zoom, selection, socket drag, canvas-local node
                    order, measured node sizes)
- SocketRef       — identifies a specific socket on a node
- CanvasAction    — events emitted by the canvas for the window to act on
- NodePathPurpose — which path parameter a requested file dialog belongs to

No blocking work here:
This module runs entirely on the paint path, so it opens no file dialog and touches no
filesystem (`CLAUDE.md` §5). The "..." button of a path parameter only emits
`CanvasAction::PickNodePath`; `window.rs` runs the dialog off-thread and writes the result
back into the node's parameters on a later frame.

Notes:
All coordinates in "world space" are stored in GraphNode.pos.
Canvas-to-screen transform: screen_pos = canvas_origin + pan + world_pos * zoom.

Interaction model (read before changing any gesture):
- Each node body is registered with `Ui::interact` BEFORE its inner widgets are drawn.
  egui breaks same-layer hit-test ties by taking the LAST registered widget
  (`egui-0.35.0/src/hit_test.rs:416-438`), so registering the body first lets the node's own
  widgets, and the socket rects registered after them, win the pointer. Popups live in a
  higher `Order`, so egui drops the whole canvas layer under them
  (`egui-0.35.0/src/hit_test.rs:117-126`) and an open combo list no longer drags the node.
- Node paint and hit order is `CanvasState::node_order`, a canvas-local list. The model's
  `GraphModel::nodes` order is graph data and is never reshuffled for z-order.
- Node rects are measured by egui and cached for the NEXT frame; see
  `CanvasState::estimated_node_world_size`.
- Wheel-zoom is read AFTER the node pass, and gated on `ms_widgets::combo_popup_open`. The
  `Wheel*` widgets inside a node consume `smooth_scroll_delta` as they are drawn, so an
  earlier read makes one notch over a numeric field both step the value and zoom the canvas.
  The new scale therefore lands on the next frame, which `request_repaint` makes immediate.
- The grid is painted from WORLD-space line indices derived from the visible world rect
  (`grid_line_range`), so its cost depends on the viewport, never on how far the user panned,
  and its phase cannot drift away from the nodes. A line's position comes only from
  `CanvasState::grid_line_screen_x` / `_y`; indices are bounded by `GRID_INDEX_LIMIT`, past
  which no grid is painted. Colours are opaque and fade by lerping toward the ground:
  `Color32` is premultiplied, so a white-alpha hairline is additive.

Palette:
Shared neutral tones come from `crate::theme`. Canvas-local colours exist only where the
theme has no equivalent (ground, grid levels, node body/header, the selection and running
accents, the data-edge neutral); each one documents why. Socket colours belong to
`types.rs` and stay there — they are type information, not decoration.
*/

use super::graph::{EdgeKind, GraphModel};
use super::node_defs::NodeDefs;
use super::types::{BrowserKind, NodeParams};
use egui::layers::ShapeIdx;
use egui::{
    Color32, PointerButton, Pos2, Rect, Sense, Shape, Stroke, Style, TextStyle, Ui, UiBuilder,
    Vec2, pos2, vec2,
};
use ms_widgets::{WheelComboBox, WheelSpinBox};
use std::collections::{HashMap, HashSet};

// ─── Layout constants ─────────────────────────────────────────────────────────

const NODE_WIDTH: f32 = 280.0;
const HEADER_HEIGHT: f32 = 28.0;
const SOCKET_ROW_HEIGHT: f32 = 24.0;
const SOCKET_RADIUS: f32 = 6.0;
const PARAM_ROW_HEIGHT: f32 = 30.0;
const NODE_ROUNDING: f32 = 6.0;
const ZOOM_MIN: f32 = 0.25;
const ZOOM_MAX: f32 = 2.5;
const ZOOM_STEP: f32 = 0.12;

// ─── Grid ─────────────────────────────────────────────────────────────────────

/// Fine grid spacing, in WORLD units.
///
/// World units, not screen points: the grid is anchored in graph space, so a node keeps the
/// same intersection under it through any pan and any zoom.
const GRID_STEP: f32 = 32.0;

/// Fine cells per coarse cell.
///
/// Four, so a coarse cell is 128 world units — a little under half a node body
/// ([`NODE_WIDTH`]). That is close enough to give the eye a reference frame while zoomed in
/// and far enough apart to survive being zoomed out to [`ZOOM_MIN`], where the coarse grid is
/// the only thing left carrying the structure.
const GRID_COARSE_MULTIPLE: i64 = 4;

/// Screen spacing (points) at or above which the fine grid is at full strength.
///
/// Below roughly this spacing a grid stops reading as texture and starts reading as a hatched
/// surface, so the fine level is ramped out between here and [`GRID_FINE_FADE_OUT_PX`] instead
/// of being switched off at one threshold — a hard cutoff pops visibly mid-wheel-notch.
const GRID_FINE_FADE_FULL_PX: f32 = 14.0;

/// Screen spacing (points) at or below which the fine grid is not emitted at all.
///
/// At the current zoom limits (`GRID_STEP * ZOOM_MIN` = 8pt) the fine grid only ever reaches
/// the middle of the ramp, so it never actually disappears; the bound exists so the level of
/// detail stays correct if the zoom range is ever widened.
const GRID_FINE_FADE_OUT_PX: f32 = 6.0;

/// Largest absolute grid-line index the grid will paint: 2^24.
///
/// A line's world coordinate is `index * GRID_STEP`, and producing it converts the index to
/// `f32`. `f32` represents every integer exactly only up to 2^24 (16 777 216); past that the
/// cast silently snaps to the nearest representable value and the grid would drift out of
/// phase with the nodes — the exact failure the graph-space anchoring exists to prevent. The
/// index is ABSOLUTE (it counts cells from the world origin) and grows with `pan`, which is
/// unclamped, so the bound cannot be argued from the visible span.
///
/// Beyond it [`grid_line_range`] returns `None` and no grid is painted at all; the nodes and
/// every gesture keep working. Reaching it needs `|pan|` of about `2^24 * GRID_STEP * zoom`
/// points — 1.3e8 points at [`ZOOM_MIN`], i.e. tens of hours of uninterrupted dragging.
const GRID_INDEX_LIMIT: f32 = 16_777_216.0;

// ─── Palette ──────────────────────────────────────────────────────────────────
//
// Shared launcher tones come from `crate::theme`. A colour is declared here only when the
// theme has no equivalent, and then it says why it is canvas-local. Everything neutral stays
// on the launcher's grey ramp (24,24,28 -> 34,34,40 -> 44,44,52 -> 52,52,60); the canvas used
// to run a second, blue-slate palette of its own, which was visible inside a single node
// wherever hand-painted text sat next to a widget drawn with `override_text_color`.

/// Opaque canvas ground, painted before anything else.
///
/// Canvas-local because the theme has no "ground" tone: `visuals.panel_fill` is deliberately
/// transparent so the launcher's own background shows through its pages, and a node editor
/// must not inherit whatever happens to be behind it. One step below
/// `theme::COMBO_POPUP_FILL` so node bodies read as raised above it.
const COL_GROUND: Color32 = Color32::from_rgb(20, 20, 24);

/// Fine grid line colour at full strength.
///
/// Opaque, on the launcher's neutral ramp, and deliberately NOT a white-alpha tint: `Color32`
/// is premultiplied, so a white-alpha hairline adds a full 255 of white while occluding almost
/// nothing and renders as an additive streak (`theme.rs`, `BUTTON_HOVERED`, documents the same
/// trap). Fine and coarse differ only in weight, never in hue.
const COL_GRID_FINE: Color32 = Color32::from_rgb(38, 38, 45);

/// Coarse grid line colour: the same neutral one step brighter, so the coarse level reads as
/// structure and the fine level as texture.
const COL_GRID_COARSE: Color32 = Color32::from_rgb(52, 52, 61);

/// Node body fill — between `theme::COMBO_POPUP_FILL` and `theme::COMBO_HOVERED` on the
/// launcher ramp, lifted clear of [`COL_GROUND`] so a node reads as a surface on the canvas.
const COL_BODY: Color32 = Color32::from_rgb(32, 32, 38);

/// Node header fill — the `theme::BUTTON_HOVERED` tone, opaque, so the title bar reads as the
/// node's interactive handle without introducing a second hue.
const COL_HEADER: Color32 = Color32::from_rgb(46, 46, 54);

/// Socket-label text.
///
/// Canvas-local and opaque: `theme::TEXT_MUTED` / `TEXT_FAINT` are white-alpha, and because
/// `Color32` is premultiplied they composite to roughly rgb(247) on a dark node body — i.e.
/// BRIGHTER than `theme::TEXT_MAIN`, the opposite of the hierarchy a secondary label needs.
/// This is the same neutral family, genuinely a step down in luminance. The node title and the
/// embedded widgets both use `theme::TEXT_MAIN`, so a node no longer shows two "main" tones.
const COL_TEXT_DIM: Color32 = Color32::from_rgb(158, 158, 168);

/// Idle node border.
///
/// Canvas-local rather than `theme::CARD_STROKE`: that constant is a white-alpha hairline, and
/// `Color32` being premultiplied makes it additive — acceptable on a handful of launcher cards,
/// a field of glowing outlines on a canvas full of nodes. This is the same neutral ramp one
/// step above [`COL_HEADER`], so a node has a defined edge without any glow.
const COL_NODE_BORDER: Color32 = Color32::from_rgb(58, 58, 68);

/// Selection accent. Canvas-local: the launcher's `visuals.selection.bg_fill` is a neutral
/// wash for text selection, which would be invisible as a node outline on a dark ground.
/// Kept clearly apart in hue from [`COL_ACTIVE`] — the two can be on at once.
const COL_SELECTED: Color32 = Color32::from_rgb(0x5b, 0x9d, 0xf8);

/// "Currently executing" accent. Canvas-local for the same reason as [`COL_SELECTED`]; amber
/// rather than `theme::STATUS_SUCCESS` green, which reads as "finished", not "running".
const COL_ACTIVE: Color32 = Color32::from_rgb(0xf5, 0x9e, 0x0b);

/// Data-edge colour. Canvas-local and deliberately neutral: a data edge may carry any
/// [`super::types::DataType`], so it must not claim one type's socket colour. Exec edges do
/// take their colour from `SocketKind::Exec`, which has exactly one meaning.
const COL_EDGE_DATA: Color32 = Color32::from_rgb(0x8a, 0x8f, 0x9c);

/// Screen-space inset from the canvas corner at which a newly created node is placed.
///
/// In points rather than world units so the spawn point sits the same distance inside the
/// viewport at every zoom level.
const SPAWN_INSET_PX: f32 = 48.0;

/// Parameter rows assumed for a node that has never been measured yet.
///
/// Only ever used on the single frame a node first appears; from the next frame on the real
/// measured size is cached. It is deliberately a node-kind-INDEPENDENT guess: a per-kind row
/// table is a second model of what the parameter editor draws, and the previous one had
/// already drifted out of sync with it.
const ESTIMATED_PARAM_ROWS: f32 = 3.0;

// ─── Socket reference ─────────────────────────────────────────────────────────

/// Identifies a specific socket (node_id + socket_name + is_input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketRef {
    pub node_id: u32,
    pub socket_name: String,
    pub is_input: bool,
}

// ─── Canvas action ────────────────────────────────────────────────────────────

/// Events emitted by `CanvasState::show` that the window must handle.
#[derive(Debug)]
pub enum CanvasAction {
    /// User finished dragging from src socket to dst socket — try to add an edge.
    ConnectSockets { src: SocketRef, dst: SocketRef },
    /// Delete was pressed while nodes were selected.
    ///
    /// `node_ids` is the full selection at the moment the key was pressed. The handler deletes
    /// exactly these and then calls `CanvasState::clear_selection`; it must not read the
    /// selection back out of the canvas, which owns it purely as UI state.
    DeleteSelected { node_ids: Vec<u32> },
    /// The "..." button of a node's path parameter was clicked — open a file dialog for it.
    ///
    /// The canvas only REQUESTS the dialog. Opening it is blocking work and must not happen on
    /// the paint path (`CLAUDE.md` §5), so `window.rs` owns the off-thread dialog and writes the
    /// picked path back into `purpose`'s parameter once it arrives. The node may have been
    /// deleted or retyped by then, which is why the request carries the purpose rather than a
    /// borrow of the parameter.
    PickNodePath {
        node_id: u32,
        purpose: NodePathPurpose,
    },
}

/// Which path parameter of a node a file dialog was requested for.
///
/// Deliberately a small enum rather than "file or folder": it names the PARAMETER, so the
/// handler that applies the result can check that the node still holds the parameter the user
/// clicked on, instead of guessing from the node's current kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodePathPurpose {
    /// `NodeParams::StartString::path` — the `.txt` file the node reads its lines from.
    StartStringFile,
    /// `NodeParams::SaveFolder::path` — the output directory images are written to.
    SaveFolderDirectory,
}

// ─── Drag state ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct DragSocket {
    origin: SocketRef,
    origin_screen_pos: Pos2,
    /// Current mouse position in screen space.
    current_screen_pos: Pos2,
}

/// What one node's render pass produced: the rect egui actually laid out, where its sockets
/// ended up in screen space, and whether its parameter editor asked for a file dialog.
///
/// `path_pick_request` is a single `Option` rather than a list because a node has exactly one
/// `NodeParams` variant and no variant carries two path parameters.
struct NodeRenderOutput {
    rect: Rect,
    socket_positions: Vec<(SocketRef, Pos2)>,
    path_pick_request: Option<NodePathPurpose>,
}

struct NodeUiRequest<'a> {
    node_id: u32,
    estimated_rect: Rect,
    params: &'a mut NodeParams,
    variables: &'a [super::graph::GraphVariable],
    sockets: &'a [super::types::SocketSpec],
    is_selected: bool,
    is_active: bool,
    canvas_rect: Rect,
}

// ─── Canvas state ─────────────────────────────────────────────────────────────

pub struct CanvasState {
    /// World-space translation: screen_pos = canvas_origin + pan + world_pos * zoom.
    pub pan: Vec2,
    pub zoom: f32,
    drag_socket: Option<DragSocket>,
    selected_nodes: HashSet<u32>,
    /// True while Space is held (enables pan-by-drag).
    space_held: bool,
    /// Canvas-local paint and hit order, front-most LAST.
    ///
    /// Deliberately separate from `GraphModel::nodes`: node order in the model is graph data
    /// (and part of the saved JSON), so raising a node on click must not touch it.
    node_order: Vec<u32>,
    /// Node body sizes in WORLD units, measured by egui on the previous frame.
    ///
    /// Stored unzoomed so a zoom change does not invalidate the entry. Evicted for nodes that
    /// no longer exist in the model, so the map cannot outgrow the graph.
    node_sizes: HashMap<u32, Vec2>,
    /// Canvas rect in SCREEN space as laid out on the previous frame.
    ///
    /// `None` until the canvas has been drawn once. Kept here so that "where is the user
    /// currently looking, in world units?" has the same owner as the transform itself; the
    /// alternative — re-deriving `pan`/`zoom` in `window.rs` to place a new node — is exactly
    /// the second copy of the transform this module keeps out.
    last_canvas_rect: Option<Rect>,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            pan: Vec2::ZERO,
            zoom: 1.0,
            drag_socket: None,
            selected_nodes: HashSet::new(),
            space_held: false,
            node_order: Vec::new(),
            node_sizes: HashMap::new(),
            last_canvas_rect: None,
        }
    }
}

impl CanvasState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Main entry point: render the node graph and return any actions the caller must handle.
    ///
    /// Mutates `model` directly for node positions and node parameters (the established design
    /// of this module); everything structural is reported through the returned `CanvasAction`s.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        model: &mut GraphModel,
        defs: &NodeDefs,
        active_node_id: Option<u32>,
    ) -> Vec<CanvasAction> {
        let mut actions = Vec::new();

        let (background, painter) =
            ui.allocate_painter(ui.available_size(), egui::Sense::click_and_drag());

        let canvas_rect = background.rect;
        let canvas_origin = canvas_rect.min.to_vec2();
        // Published for node placement: `window.rs` asks the canvas where the user is looking
        // instead of keeping its own idea of pan/zoom.
        self.last_canvas_rect = Some(canvas_rect);

        // ── Keyboard input ─────────────────────────────────────────────────
        // `TextEdit` consumes nothing in egui 0.35, so without this gate Delete would erase the
        // selected nodes and Space would flip pan mode while the user types inside a node.
        let typing = ui.ctx().egui_wants_keyboard_input();
        self.space_held = !typing && ui.input(|i| i.key_down(egui::Key::Space));

        if !typing
            && ui.input(|i| i.key_pressed(egui::Key::Delete))
            && !self.selected_nodes.is_empty()
        {
            actions.push(CanvasAction::DeleteSelected {
                node_ids: self.selected_nodes.iter().copied().collect(),
            });
        }

        // ── Pan ────────────────────────────────────────────────────────────
        self.pan += self.pan_delta_of(&background);

        // ── Ground and grid ────────────────────────────────────────────────
        // The canvas owns its ground: `visuals.panel_fill` is transparent launcher-wide, so
        // without this fill the node editor would render over eframe's clear colour.
        painter.rect_filled(canvas_rect, egui::CornerRadius::ZERO, COL_GROUND);
        draw_grid(&painter, canvas_rect, self);

        // ── Reserve the edge shapes BEFORE the nodes are painted ───────────
        // Shapes are drawn in the order they are added to the layer, and socket positions are
        // only known after a node has been laid out. Reserving one slot per edge here and
        // filling it with `Painter::set` after the node pass is what puts the connections
        // behind the node boxes instead of across them.
        let edge_slots: Vec<ShapeIdx> = model
            .edges
            .iter()
            .map(|_| painter.add(Shape::Noop))
            .collect();

        // ── Node pass ──────────────────────────────────────────────────────
        let live_ids: Vec<u32> = model.nodes.iter().map(|n| n.id).collect();
        reconcile_node_order(&mut self.node_order, &live_ids);
        let paint_order = self.node_order.clone();
        let variables_snapshot = model.variables.clone();

        let mut socket_screen_positions: Vec<(SocketRef, Pos2)> = Vec::new();
        let mut raise_to_front: Option<u32> = None;
        // Pan captured by a node body (middle-drag or pan mode over a node). Applied after the
        // pass so that every node in THIS frame is placed with one and the same transform; the
        // gesture therefore lags the background-driven pan by exactly one frame.
        let mut pan_from_nodes = Vec2::ZERO;
        let mut drag_pointer_pos: Option<Pos2> = None;
        let mut drag_origin_active = false;
        let mut drop_target: Option<SocketRef> = None;
        // Modifier state is constant for the frame; read it once instead of per node.
        let additive = ui.input(|i| i.modifiers.shift);

        for node_id in paint_order {
            let Some((world_pos, sockets)) = model.node_by_id(node_id).map(|node| {
                (
                    node.pos,
                    defs.socket_specs_for_node(
                        node.template_key(),
                        &node.params,
                        &variables_snapshot,
                    ),
                )
            }) else {
                continue;
            };

            let screen_pos = self.world_to_screen(world_pos, canvas_origin);
            let estimated_rect = Rect::from_min_size(
                screen_pos,
                self.world_to_screen_size(self.estimated_node_world_size(node_id, sockets.len())),
            );

            // Registered BEFORE the node's own widgets so that egui's tie-breaking (last
            // registered wins) hands the pointer to those widgets, and to the socket rects
            // registered after them, rather than to the node body underneath.
            let node_response = ui.interact(
                estimated_rect,
                egui::Id::new(("bpn_node", node_id)),
                Sense::click_and_drag(),
            );

            let is_selected = self.selected_nodes.contains(&node_id);
            let Some(node_params) = model.node_by_id_mut(node_id).map(|node| &mut node.params)
            else {
                continue;
            };
            let node_render = self.draw_node_ui(
                ui,
                NodeUiRequest {
                    node_id,
                    estimated_rect,
                    params: node_params,
                    variables: &variables_snapshot,
                    sockets: &sockets,
                    is_selected,
                    is_active: active_node_id == Some(node_id),
                    canvas_rect,
                },
            );
            // Feed the measurement back for the next frame's placement and hit rect.
            self.node_sizes
                .insert(node_id, self.screen_to_world_size(node_render.rect.size()));

            // The dialog itself is the window's job: opening one blocks, and the paint path
            // must not (`CLAUDE.md` §5).
            if let Some(purpose) = node_render.path_pick_request {
                actions.push(CanvasAction::PickNodePath { node_id, purpose });
            }

            // ── Sockets ────────────────────────────────────────────────────
            for (socket_ref, socket_screen) in &node_render.socket_positions {
                let socket_response = ui.interact(
                    socket_hit_rect(*socket_screen, self.zoom),
                    egui::Id::new((
                        "bpn_sock",
                        socket_ref.node_id,
                        socket_ref.socket_name.as_str(),
                        socket_ref.is_input,
                    )),
                    Sense::drag(),
                );

                if socket_response.drag_started() {
                    self.drag_socket = Some(DragSocket {
                        origin: socket_ref.clone(),
                        origin_screen_pos: *socket_screen,
                        current_screen_pos: *socket_screen,
                    });
                }

                let is_drag_origin = self
                    .drag_socket
                    .as_ref()
                    .is_some_and(|drag| &drag.origin == socket_ref);
                if is_drag_origin {
                    if socket_response.dragged() {
                        drag_origin_active = true;
                        drag_pointer_pos = socket_response.interact_pointer_pos();
                    }
                } else if socket_response.contains_pointer() {
                    // `contains_pointer` stays true while another widget owns the drag
                    // (`egui-0.35.0/src/response.rs:317-327`), which is exactly the
                    // drop-target query a connection drag needs.
                    drop_target = Some(socket_ref.clone());
                }
            }
            socket_screen_positions.extend(node_render.socket_positions);

            // ── Node gestures ──────────────────────────────────────────────
            pan_from_nodes += self.pan_delta_of(&node_response);

            let grabbed = !self.space_held && node_response.drag_started_by(PointerButton::Primary);
            if node_response.clicked() || grabbed {
                raise_to_front = Some(node_id);
                // A click always collapses the selection onto this node; grabbing an
                // already-selected node keeps the selection so a multi-selection survives.
                if node_response.clicked() || !is_selected {
                    if !additive {
                        self.selected_nodes.clear();
                    }
                    self.selected_nodes.insert(node_id);
                }
            }

            if !self.space_held && node_response.dragged_by(PointerButton::Primary) {
                let delta = self.screen_to_world_size(node_response.drag_delta());
                if let Some(node) = model.node_by_id_mut(node_id) {
                    node.pos += delta;
                }
            }
        }

        // ── Zoom ───────────────────────────────────────────────────────────
        // Read AFTER the node pass, and that ordering is the whole point. The `Wheel*` widgets
        // inside a node body step from the raw `Event::MouseWheel` stream and only then zero
        // `smooth_scroll_delta` (`ms-widgets/src/wheel_spin_box.rs`,
        // `consume_wheel_scroll_delta`), so a read placed before they are drawn can never see
        // that consumption: one notch over a node's numeric field stepped the value AND zoomed
        // the canvas. Reading here costs one frame of latency on the new scale — the same
        // one-frame convention `pan_from_nodes` already follows — and `request_repaint` below
        // pins the lag at exactly one frame rather than "until the next input event".
        //
        // The `combo_popup_open` gate is mandatory for any canvas that reads the raw wheel
        // delta (`ms-widgets/src/wheel_input_guard.rs`); the same shape is used by
        // `ms-tab-page-manager/src/split.rs` and `crop.rs`. An open combo list owns the wheel,
        // and its popup layer does not consume the delta on the canvas's behalf.
        let scroll_delta = if ms_widgets::combo_popup_open(ui.ctx()) {
            0.0
        } else {
            ui.input(|i| i.smooth_scroll_delta.y)
        };
        // `contains_pointer()` rather than `hovered()`: a node body covers the background
        // response, and wheel-zoom must keep working over nodes. It is still occlusion-aware —
        // an open popup or tooltip is a higher layer and suppresses it.
        if scroll_delta != 0.0
            && background.contains_pointer()
            && let Some(mouse_pos) = ui.input(|i| i.pointer.hover_pos())
        {
            // Keep the world point under the cursor pinned while the scale changes. The anchor
            // is resolved with the transform THIS frame was painted with, which is why the
            // zoom is applied before `pan_from_nodes` is folded in below.
            let anchor_world = self.screen_to_world(mouse_pos, canvas_origin);
            self.zoom = (self.zoom + scroll_delta * ZOOM_STEP * 0.1).clamp(ZOOM_MIN, ZOOM_MAX);
            self.pan = mouse_pos.to_vec2() - canvas_origin - anchor_world.to_vec2() * self.zoom;
            ui.ctx().request_repaint();
        }

        self.pan += pan_from_nodes;
        if let Some(node_id) = raise_to_front {
            raise_node_to_front(&mut self.node_order, node_id);
        }
        evict_stale_node_sizes(&mut self.node_sizes, &live_ids);

        // ── Resolve the in-progress connection ─────────────────────────────
        if self.drag_socket.is_some() {
            if drag_origin_active {
                if let Some(pos) = drag_pointer_pos
                    && let Some(drag) = &mut self.drag_socket
                {
                    drag.current_screen_pos = pos;
                }
            } else if let Some(drag) = self.drag_socket.take() {
                // The origin socket is no longer being dragged, so this frame is the drop.
                // Taking the drag unconditionally is what cancels it when it landed on
                // nothing, on itself, or on the same node.
                if let Some(dst) = drop_target
                    && drag.origin != dst
                    && drag.origin.node_id != dst.node_id
                {
                    actions.push(CanvasAction::ConnectSockets {
                        src: drag.origin,
                        dst,
                    });
                }
            }
        }

        // ── Fill the reserved edge shapes ──────────────────────────────────
        // Slots left untouched keep their `Shape::Noop` placeholder, which costs nothing.
        for (edge, slot) in model.edges.iter().zip(edge_slots) {
            let src_pos = socket_screen_positions
                .iter()
                .find(|(r, _)| {
                    r.node_id == edge.src_node && r.socket_name == edge.src_socket && !r.is_input
                })
                .map(|(_, p)| *p);
            let dst_pos = socket_screen_positions
                .iter()
                .find(|(r, _)| {
                    r.node_id == edge.dst_node && r.socket_name == edge.dst_socket && r.is_input
                })
                .map(|(_, p)| *p);

            if let (Some(p0), Some(p3)) = (src_pos, dst_pos) {
                // Exec edges borrow the exec socket's colour from `types.rs` rather than
                // repeating the literal; data edges stay neutral because one edge can carry
                // any `DataType` and must not claim a single type's accent.
                let color = match edge.kind {
                    EdgeKind::Exec => super::types::SocketKind::Exec.color(),
                    EdgeKind::Data => COL_EDGE_DATA,
                };
                painter.set(slot, bezier_shape(p0, p3, color, 2.0 * self.zoom));
            }
        }

        // ── Draw in-progress connection ────────────────────────────────────
        // Added after the node pass on purpose: the rubber-band belongs on top of everything.
        if let Some(drag) = &self.drag_socket {
            // The rubber-band is transient and must stand out against both the ground and any
            // node it crosses, so it is the one place a bright neutral is wanted.
            let color = crate::theme::TEXT_MAIN;
            painter.add(bezier_shape(
                drag.origin_screen_pos,
                drag.current_screen_pos,
                color,
                1.5,
            ));
        }

        // ── Deselect on background click ────────────────────────────────────
        // A click that landed on a node was claimed by that node's response, so the background
        // response only clicks on genuinely empty canvas.
        if background.clicked() {
            self.selected_nodes.clear();
        }

        actions
    }

    // ─── Coordinate helpers ───────────────────────────────────────────────────

    /// Canvas transform, forward direction.
    ///
    /// The convention is `screen = canvas_origin + pan + world * zoom`, with `canvas_origin`
    /// the top-left of the canvas rect as a vector. This and [`Self::screen_to_world`] are the
    /// ONLY places allowed to apply `pan`/`zoom` to a coordinate — the grid used to keep a
    /// private copy of this maths and drifted away from the node placement.
    pub fn world_to_screen(&self, world: Pos2, canvas_origin: Vec2) -> Pos2 {
        pos2(
            canvas_origin.x + self.pan.x + world.x * self.zoom,
            canvas_origin.y + self.pan.y + world.y * self.zoom,
        )
    }

    /// Canvas transform, inverse direction: `world = (screen - canvas_origin - pan) / zoom`.
    ///
    /// Exact inverse of [`Self::world_to_screen`] for the same `canvas_origin`. `zoom` is
    /// clamped to `ZOOM_MIN..=ZOOM_MAX` on every mutation, so the division never sees zero.
    pub fn screen_to_world(&self, screen: Pos2, canvas_origin: Vec2) -> Pos2 {
        pos2(
            (screen.x - canvas_origin.x - self.pan.x) / self.zoom,
            (screen.y - canvas_origin.y - self.pan.y) / self.zoom,
        )
    }

    /// Canvas transform applied to a LENGTH rather than a position: the screen extent of
    /// `world_length` world units.
    ///
    /// Translation-free by definition, so it is exact for any `pan` — deriving a length by
    /// subtracting two [`Self::world_to_screen`] results loses precision once `pan` is large.
    /// This is for canvas geometry only; the per-widget scaling of fonts, margins and paddings
    /// inside a node is `Style` scaling (`apply_zoomed_node_style`), not a canvas transform.
    pub fn world_to_screen_len(&self, world_length: f32) -> f32 {
        world_length * self.zoom
    }

    /// Two-dimensional form of [`Self::world_to_screen_len`]: the screen extent of a world
    /// size or displacement.
    ///
    /// Translation-free, so it is the right conversion for a node's measured size, an
    /// estimated body extent, or any other quantity that has a magnitude but no position.
    pub fn world_to_screen_size(&self, world_size: Vec2) -> Vec2 {
        vec2(
            self.world_to_screen_len(world_size.x),
            self.world_to_screen_len(world_size.y),
        )
    }

    /// Inverse of [`Self::world_to_screen_size`]: the world extent of a screen size or
    /// displacement.
    ///
    /// The missing half of the length owner — without it every screen-to-world extent (a drag
    /// delta, a measured node rect, the spawn stagger) had to divide by `zoom` by hand, which
    /// is the second copy of the transform this module keeps out. `zoom` is clamped to
    /// `ZOOM_MIN..=ZOOM_MAX` on every mutation, so the division never sees zero.
    pub fn screen_to_world_size(&self, screen_size: Vec2) -> Vec2 {
        screen_size / self.zoom
    }

    /// World-space rectangle currently visible inside `canvas_rect`.
    ///
    /// Both corners go through [`Self::screen_to_world`], so the result follows the one
    /// transform owner. Used by the grid to bound its iteration to what can actually be seen.
    fn visible_world_rect(&self, canvas_rect: Rect) -> Rect {
        let canvas_origin = canvas_rect.min.to_vec2();
        Rect::from_two_pos(
            self.screen_to_world(canvas_rect.min, canvas_origin),
            self.screen_to_world(canvas_rect.max, canvas_origin),
        )
    }

    /// Screen x of the VERTICAL grid line with absolute index `index`.
    ///
    /// The whole position of a grid line, in one place and as a pure function of the canvas
    /// state: `draw_grid` may not derive a line position any other way. That is what the grid
    /// bug was — the previous implementation quantized `pan` to whole cells
    /// (`pan - pan.rem_euclid(step)`), which cancels `pan` out of the result entirely and lets
    /// the grid slide out of phase with the nodes. Everything here goes through
    /// [`Self::world_to_screen`], so there is no second copy of the transform to drift.
    fn grid_line_screen_x(&self, index: i64, canvas_origin: Vec2) -> f32 {
        self.world_to_screen(pos2(grid_line_world(index), 0.0), canvas_origin)
            .x
    }

    /// Screen y of the HORIZONTAL grid line with absolute index `index`.
    /// See [`Self::grid_line_screen_x`] for why this is the only way to place a grid line.
    fn grid_line_screen_y(&self, index: i64, canvas_origin: Vec2) -> f32 {
        self.world_to_screen(pos2(0.0, grid_line_world(index)), canvas_origin)
            .y
    }

    /// World position a newly created node should be anchored at.
    ///
    /// [`SPAWN_INSET_PX`] points inside the top-left of the canvas as it was last laid out,
    /// so a node added after the user panned or zoomed away still appears on screen instead of
    /// at a fixed world coordinate far outside the view. Falls back to the world origin before
    /// the canvas has ever been drawn — the caller cannot have clicked anything by then, so the
    /// fallback is only ever a defined value, never a displayed one.
    pub fn visible_spawn_origin(&self) -> Pos2 {
        match self.last_canvas_rect {
            Some(rect) => self.screen_to_world(
                rect.min + Vec2::splat(SPAWN_INSET_PX),
                rect.min.to_vec2(),
            ),
            None => Pos2::ZERO,
        }
    }

    pub fn selected_nodes(&self) -> &HashSet<u32> {
        &self.selected_nodes
    }

    pub fn clear_selection(&mut self) {
        self.selected_nodes.clear();
    }

    /// Pan contributed by one response this frame.
    ///
    /// A middle-button drag, or a primary drag while Space is held, pans the canvas no matter
    /// which widget captured it. egui starts a drag on ANY pointer button, so once node bodies
    /// sense drags the background response alone can no longer see a middle-drag that began
    /// over a node — every response that can capture one is asked here instead.
    fn pan_delta_of(&self, response: &egui::Response) -> Vec2 {
        if response.dragged_by(PointerButton::Middle)
            || (self.space_held && response.dragged_by(PointerButton::Primary))
        {
            response.drag_delta()
        } else {
            Vec2::ZERO
        }
    }

    /// Node body size in world units used to place and hit-test the node this frame.
    ///
    /// Returns the size egui measured on the previous frame when there is one, otherwise a
    /// node-kind-independent estimate from the header, the socket rows and
    /// [`ESTIMATED_PARAM_ROWS`]. Consequence of the one-frame lag: on the frame a node first
    /// appears — and on the frame its parameter editor changes height, e.g. when a template
    /// gains a placeholder socket — the interact rect is the previous size. It is a single
    /// frame of a slightly wrong hit rect, and it buys the single source of truth for node
    /// height: the parameter editor itself.
    fn estimated_node_world_size(&self, node_id: u32, socket_count: usize) -> Vec2 {
        if let Some(size) = self.node_sizes.get(&node_id) {
            return *size;
        }
        let height = HEADER_HEIGHT
            + socket_count as f32 * SOCKET_ROW_HEIGHT
            + ESTIMATED_PARAM_ROWS * PARAM_ROW_HEIGHT
            + 36.0;
        vec2(NODE_WIDTH, height)
    }

    /// Draw one node body and report the rect egui laid out plus its socket screen positions.
    fn draw_node_ui(&self, ui: &mut Ui, request: NodeUiRequest<'_>) -> NodeRenderOutput {
        let NodeUiRequest {
            node_id,
            estimated_rect,
            params,
            variables,
            sockets,
            is_selected,
            is_active,
            canvas_rect,
        } = request;

        let inner = ui.scope_builder(
            UiBuilder::new()
                .id_salt(("bpn_node_ui", node_id))
                .max_rect(estimated_rect),
            |node_ui| {
                apply_zoomed_node_style(node_ui, self.zoom);
                node_ui.set_clip_rect(canvas_rect);

                let frame = egui::Frame::new()
                    .fill(COL_BODY)
                    .corner_radius(egui::CornerRadius::same(
                        (NODE_ROUNDING * self.zoom).round().clamp(0.0, 255.0) as u8,
                    ))
                    .stroke(if is_active {
                        Stroke::new(3.0, COL_ACTIVE)
                    } else if is_selected {
                        Stroke::new(2.0, COL_SELECTED)
                    } else {
                        Stroke::new(1.0, COL_NODE_BORDER)
                    })
                    // Clamped like the corner radii above: `Margin` stores `i8`, so the cast
                    // must be bounded at the point of use rather than by `ZOOM_MAX` happening
                    // to be small enough today (`CLAUDE.md` §17).
                    .inner_margin(egui::Margin::same(
                        (8.0 * self.zoom).round().clamp(0.0, 127.0) as i8,
                    ));

                let frame_response = frame.show(node_ui, |node_ui| {
                    node_ui.set_width(NODE_WIDTH * self.zoom);
                    node_ui.spacing_mut().item_spacing = vec2(6.0 * self.zoom, 6.0 * self.zoom);

                    draw_node_header(node_ui, params.title(), self.zoom);
                    let socket_positions =
                        draw_socket_rows(node_ui, node_id, sockets, self.zoom, canvas_rect);
                    let path_pick_request =
                        show_inline_param_editor_ui(node_ui, node_id, params, variables, self.zoom);
                    (socket_positions, path_pick_request)
                });

                (frame_response.response.rect, frame_response.inner)
            },
        );

        let (rect, (socket_positions, path_pick_request)) = inner.inner;
        NodeRenderOutput {
            rect,
            socket_positions,
            path_pick_request,
        }
    }
}

// ─── Canvas-local node order and size cache ───────────────────────────────────

/// Bring `order` in sync with the live node set, preserving the existing z-order.
///
/// Ids that disappeared from the model are dropped; ids the canvas has not seen yet are
/// appended in model order, i.e. a freshly added node starts at the front.
fn reconcile_node_order(order: &mut Vec<u32>, live: &[u32]) {
    let live_set: HashSet<u32> = live.iter().copied().collect();
    order.retain(|id| live_set.contains(id));
    let known: HashSet<u32> = order.iter().copied().collect();
    order.extend(live.iter().copied().filter(|id| !known.contains(id)));
}

/// Move `node_id` to the front (last position) of the canvas-local paint/hit order.
///
/// A no-op when the id is absent. Never touches `GraphModel::nodes`.
fn raise_node_to_front(order: &mut Vec<u32>, node_id: u32) {
    if let Some(index) = order.iter().position(|id| *id == node_id) {
        let id = order.remove(index);
        order.push(id);
    }
}

/// Drop cached sizes of nodes that no longer exist, so the cache stays bounded by the graph.
fn evict_stale_node_sizes(sizes: &mut HashMap<u32, Vec2>, live: &[u32]) {
    let live_set: HashSet<u32> = live.iter().copied().collect();
    sizes.retain(|id, _| live_set.contains(id));
}

/// The one owner of a socket's screen-space hit region.
///
/// `center` is the painted socket centre, `zoom` the current canvas zoom. Everything that has
/// to answer "is the pointer on this socket?" must use this rect: the previous split between a
/// circle (node-drag exclusion) and a square (interact rect) of the same half-extent made the
/// four corners start a socket drag and a node drag at the same time.
fn socket_hit_rect(center: Pos2, zoom: f32) -> Rect {
    Rect::from_center_size(center, Vec2::splat(SOCKET_RADIUS * zoom * 2.5))
}

// ─── Drawing helpers ──────────────────────────────────────────────────────────

/// Strength of the fine grid level for a given on-screen spacing, in `0.0..=1.0`.
///
/// `screen_step` is the distance between two adjacent fine lines in points. Returns `0.0` at
/// or below [`GRID_FINE_FADE_OUT_PX`] (the caller then emits nothing), `1.0` at or above
/// [`GRID_FINE_FADE_FULL_PX`], and a linear ramp in between so a zoom gesture never pops a
/// whole grid level in or out on one frame.
fn fine_grid_strength(screen_step: f32) -> f32 {
    if !screen_step.is_finite() {
        return 0.0;
    }
    ((screen_step - GRID_FINE_FADE_OUT_PX) / (GRID_FINE_FADE_FULL_PX - GRID_FINE_FADE_OUT_PX))
        .clamp(0.0, 1.0)
}

/// Inclusive range of grid line indices covering the world span `min..=max`.
///
/// A line of index `i` sits at world coordinate `i * step`. The range is derived from the
/// VISIBLE world span, so the number of lines depends only on the size of the viewport and
/// never on how far the user has panned — the previous implementation walked from the world
/// origin and emitted thousands of off-screen segments at large pans.
///
/// Returns `None` when the span cannot be turned into usable indices: a non-finite coordinate,
/// a non-positive `step`, or a span so far from the origin that an index would leave
/// [`GRID_INDEX_LIMIT`]. The grid is then simply not painted, which is the only safe answer —
/// a saturated index pair would make the caller iterate across the whole `i64` range, and an
/// index past the limit no longer survives the `f32` round trip [`grid_line_world`] performs.
fn grid_line_range(min: f32, max: f32, step: f32) -> Option<std::ops::RangeInclusive<i64>> {
    if !step.is_finite() || step <= 0.0 || !min.is_finite() || !max.is_finite() || max < min {
        return None;
    }
    let first = (min / step).floor();
    let last = (max / step).ceil();
    // Both bounds are already integral f32 values here, and the limit keeps them inside the
    // range where f32 and i64 agree on every integer — so the cast below is exact in both
    // directions, and f32 -> i64 never reaches its saturating behaviour.
    if first < -GRID_INDEX_LIMIT || last > GRID_INDEX_LIMIT {
        return None;
    }
    Some((first as i64)..=(last as i64))
}

/// World coordinate of the grid line with absolute index `index`.
///
/// The single place where a grid index becomes a world coordinate, and therefore the single
/// `index as f32` cast: it is exact because [`grid_line_range`] admits only indices within
/// [`GRID_INDEX_LIMIT`], where every integer is representable in `f32`.
fn grid_line_world(index: i64) -> f32 {
    index as f32 * GRID_STEP
}

/// Paint the canvas grid: a fine level every [`GRID_STEP`] world units and a coarse level
/// every [`GRID_COARSE_MULTIPLE`] fine cells.
///
/// Takes the whole `CanvasState` because every line position must come from
/// [`CanvasState::grid_line_screen_x`] / [`CanvasState::grid_line_screen_y`], which are in
/// turn built on [`CanvasState::world_to_screen`]; the grid must not own a second copy of the
/// transform. That is what anchors the grid in graph space — a node keeps the same
/// intersection under it through any pan and any zoom. This function therefore only decides
/// WHICH lines to emit and in what colour; WHERE each one goes is not its business. Stroke
/// width stays 1 point at every zoom: scaling it would thicken the grid as the user zooms in,
/// which is the complaint this function exists to fix.
fn draw_grid(painter: &egui::Painter, rect: Rect, state: &CanvasState) {
    let canvas_origin = rect.min.to_vec2();
    let fine_step_px = state.world_to_screen_len(GRID_STEP);
    let world_view = state.visible_world_rect(rect);

    let (Some(x_range), Some(y_range)) = (
        grid_line_range(world_view.min.x, world_view.max.x, GRID_STEP),
        grid_line_range(world_view.min.y, world_view.max.y, GRID_STEP),
    ) else {
        return;
    };

    let fine_strength = fine_grid_strength(fine_step_px);
    // Fading by lerping toward the ground instead of by alpha keeps every grid colour opaque.
    // `Color32` is premultiplied, so an alpha fade of a bright tint is additive and was the
    // reason the old grid read as a solid white streak.
    let fine_color = COL_GROUND.lerp_to_gamma(COL_GRID_FINE, fine_strength);
    let draw_fine = fine_strength > 0.0;

    // Capacity hint only, and clamped: the real line count is bounded by the viewport size
    // divided by the (zoom-clamped) step, but `grid_line_range` guards absolute position
    // rather than span, so an absurd span must not turn into an absurd allocation.
    let line_budget = (x_range.end() - x_range.start()) + (y_range.end() - y_range.start()) + 2;
    let mut shapes: Vec<Shape> =
        Vec::with_capacity(usize::try_from(line_budget.clamp(0, 8192)).unwrap_or(0));

    for index in x_range {
        let is_coarse = index.rem_euclid(GRID_COARSE_MULTIPLE) == 0;
        if !is_coarse && !draw_fine {
            continue;
        }
        let color = if is_coarse { COL_GRID_COARSE } else { fine_color };
        // Snapping to a pixel centre is what keeps a 1-point line one pixel wide: at a
        // fractional coordinate epaint feathers it across two or three pixels, which is the
        // second reason the old grid looked thick (`egui-0.35.0/src/painter.rs:187-191`).
        let x = painter.round_to_pixel_center(state.grid_line_screen_x(index, canvas_origin));
        shapes.push(Shape::line_segment(
            [pos2(x, rect.top()), pos2(x, rect.bottom())],
            Stroke::new(1.0, color),
        ));
    }

    for index in y_range {
        let is_coarse = index.rem_euclid(GRID_COARSE_MULTIPLE) == 0;
        if !is_coarse && !draw_fine {
            continue;
        }
        let color = if is_coarse { COL_GRID_COARSE } else { fine_color };
        let y = painter.round_to_pixel_center(state.grid_line_screen_y(index, canvas_origin));
        shapes.push(Shape::line_segment(
            [pos2(rect.left(), y), pos2(rect.right(), y)],
            Stroke::new(1.0, color),
        ));
    }

    // One batched submission instead of one `Painter::line_segment` call per line.
    painter.extend(shapes);
}

/// Build the connection curve between two sockets without painting it.
///
/// Returned as a `Shape` rather than painted directly so the same geometry can either be
/// appended (`Painter::add`, for the rubber-band on top) or written into a slot reserved
/// before the node pass (`Painter::set`, for edges behind the nodes).
fn bezier_shape(p0: Pos2, p3: Pos2, color: Color32, width: f32) -> Shape {
    let dx = (p3.x - p0.x).abs().max(80.0) * 0.5;
    let p1 = pos2(p0.x + dx, p0.y);
    let p2 = pos2(p3.x - dx, p3.y);
    Shape::CubicBezier(egui::epaint::CubicBezierShape::from_points_stroke(
        [p0, p1, p2, p3],
        false,
        Color32::TRANSPARENT,
        Stroke::new(width, color),
    ))
}

fn apply_zoomed_node_style(ui: &mut Ui, zoom: f32) {
    let mut style: Style = (*ui.style()).as_ref().clone();
    for font_id in style.text_styles.values_mut() {
        font_id.size = (font_id.size * zoom).max(1.0);
    }
    style.spacing.button_padding *= zoom;
    style.spacing.item_spacing *= zoom;
    style.spacing.icon_spacing *= zoom;
    style.spacing.icon_width *= zoom;
    style.spacing.icon_width_inner *= zoom;
    style.spacing.combo_width *= zoom;
    style.spacing.combo_height *= zoom;
    style.spacing.indent *= zoom;
    style.spacing.interact_size *= zoom;
    style.spacing.menu_margin *= zoom;
    style.spacing.slider_width *= zoom;
    ui.set_style(style);
}

fn draw_node_header(ui: &mut Ui, title: &str, zoom: f32) -> (Rect, egui::Response) {
    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(vec2(width, HEADER_HEIGHT * zoom), Sense::hover());
    ui.painter().rect_filled(
        rect,
        egui::CornerRadius::same((NODE_ROUNDING * zoom).round().clamp(0.0, 255.0) as u8),
        COL_HEADER,
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        title,
        egui::FontId::proportional((12.0 * zoom).max(1.0)),
        crate::theme::TEXT_MAIN,
    );
    (rect, response)
}

fn draw_socket_rows(
    ui: &mut Ui,
    node_id: u32,
    sockets: &[super::types::SocketSpec],
    zoom: f32,
    canvas_rect: Rect,
) -> Vec<(SocketRef, Pos2)> {
    let mut positions = Vec::with_capacity(sockets.len());

    for spec in sockets {
        let width = ui.available_width();
        let (row_rect, _) =
            ui.allocate_exact_size(vec2(width, SOCKET_ROW_HEIGHT * zoom), Sense::hover());
        let y = row_rect.center().y;
        let socket_x = if spec.is_input {
            row_rect.left()
        } else {
            row_rect.right()
        };
        let socket_pos = pos2(socket_x, y);
        let socket_ref = SocketRef {
            node_id,
            socket_name: spec.name.to_string(),
            is_input: spec.is_input,
        };
        positions.push((socket_ref, socket_pos));

        let socket_painter = ui.painter().with_clip_rect(canvas_rect);
        let radius = SOCKET_RADIUS * zoom;
        socket_painter.circle_filled(socket_pos, radius, spec.kind.color());
        socket_painter.circle_stroke(
            socket_pos,
            radius,
            Stroke::new(1.0, Color32::from_black_alpha(120)),
        );
        let label_x = if spec.is_input {
            socket_pos.x + radius + 4.0 * zoom
        } else {
            socket_pos.x - radius - 4.0 * zoom
        };
        let align = if spec.is_input {
            egui::Align2::LEFT_CENTER
        } else {
            egui::Align2::RIGHT_CENTER
        };
        // Painted label is localized when the socket carries a catalog key; dynamic
        // user-authored sockets (string-template placeholders) show their raw name.
        // `display_label` is a wait-free catalog read, safe on the paint path.
        socket_painter.text(
            pos2(label_x, socket_pos.y),
            align,
            spec.display_label(),
            egui::FontId::proportional((10.0 * zoom).max(1.0)),
            COL_TEXT_DIM,
        );
    }

    positions
}

/// Draw the editable parameters of one node inside its body.
///
/// Returns the path parameter a file dialog was requested for, if the user clicked one of the
/// "..." buttons this frame. The dialog is NOT opened here: it blocks, and this runs on the
/// paint path (`CLAUDE.md` §5); the request travels to `window.rs` as a
/// [`CanvasAction::PickNodePath`].
///
/// `zoom` is the canvas zoom. Every widget drawn here inherits the zoom-scaled `Style` applied
/// by `apply_zoomed_node_style`, so sizes and fonts follow it; `zoom` itself is only used for
/// the few lengths that are not style-derived and for the drag speed of numeric fields.
fn show_inline_param_editor_ui(
    ui: &mut Ui,
    node_id: u32,
    params: &mut NodeParams,
    variables: &[super::graph::GraphVariable],
    zoom: f32,
) -> Option<NodePathPurpose> {
    let text_width = (NODE_WIDTH - 56.0).max(80.0) * zoom;
    let mut path_pick_request = None;
    match params {
        NodeParams::StartNumber { start, step, end } => {
            inline_spin_box(ui, t!("launcher.batch.field_start"), start, zoom);
            inline_spin_box(ui, t!("launcher.batch.field_step"), step, zoom);
            inline_spin_box(ui, t!("launcher.batch.field_end"), end, zoom);
        }
        NodeParams::StartString { path } => {
            if inline_path_edit(
                ui,
                t!("launcher.batch.field_file"),
                path,
                text_width,
                t!("launcher.batch.open_text_file_button"),
            ) {
                path_pick_request = Some(NodePathPurpose::StartStringFile);
            }
        }
        NodeParams::StringTemplate {
            template,
            placeholders,
        } => {
            inline_text_edit(ui, t!("launcher.batch.field_template"), template, text_width);

            let mut placeholder_text = placeholders.join(", ");
            ui.label(egui::RichText::new(t!("launcher.batch.fields_comma_separated")).text_style(TextStyle::Small));
            let response = ui.add_sized(
                [text_width, ui.spacing().interact_size.y],
                egui::TextEdit::singleline(&mut placeholder_text),
            );
            if response.changed() {
                *placeholders = placeholder_text
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(ToOwned::to_owned)
                    .collect();
            }
        }
        NodeParams::OpenUrl { browser } => {
            ui.label(t!("launcher.batch.section_browser"));
            WheelComboBox::from_id_salt(("bpn_browser", node_id))
                .width(text_width)
                .selected_text(browser.label())
                .show_ui(ui, |ui| {
                    for candidate in BrowserKind::all() {
                        ui.selectable_value(browser, candidate.clone(), candidate.label());
                    }
                });
        }
        NodeParams::FetchFromBrowser { pattern } => {
            inline_text_edit(ui, t!("launcher.batch.field_url_pattern"), pattern, text_width);
            ui.label(egui::RichText::new(t!("launcher.batch.url_pattern_hint")).text_style(TextStyle::Small));
        }
        NodeParams::StitchSplit {
            parts,
            target_height,
            band_rows,
            tolerance,
            search_radius,
            prefer_up_first,
            auto_cut,
        } => {
            ui.checkbox(auto_cut, t!("launcher.batch.section_autocut"));
            inline_spin_box_with_range(
                ui,
                t!("launcher.batch.field_target_height"),
                target_height,
                500..=20_000,
                zoom,
            );
            inline_spin_box_with_range(
                ui,
                t!("launcher.batch.field_stitch_bands"),
                band_rows,
                1..=50,
                zoom,
            );
            inline_spin_box_with_range(
                ui,
                t!("launcher.batch.field_tolerance"),
                tolerance,
                0..=255_u8,
                zoom,
            );
            inline_spin_box_with_range(
                ui,
                t!("launcher.batch.field_search_radius"),
                search_radius,
                100..=10_000,
                zoom,
            );
            ui.checkbox(prefer_up_first, t!("launcher.batch.field_search_up_first"));

            ui.label(t!("launcher.batch.field_parts"));
            // `parts` is `Option<u32>` where `None` means "decide from the target height".
            // Zero is the editor's spelling of `None`, so the spin box edits a plain number and
            // the mapping back to the option happens on change.
            let mut parts_value = parts.unwrap_or(0);
            let response = ui.add(
                WheelSpinBox::new(&mut parts_value)
                    .range(0..=100)
                    .speed(f64::from(zoom.max(0.25))),
            );
            if response.changed() {
                *parts = if parts_value == 0 {
                    None
                } else {
                    Some(parts_value)
                };
            }
        }
        NodeParams::Waifu2x {
            scale,
            noise,
            tile_size,
        } => {
            ui.label(t!("launcher.batch.field_scale"));
            WheelComboBox::from_id_salt(("bpn_w2x_scale", node_id))
                .width(text_width)
                .selected_text(scale.to_string())
                .show_ui(ui, |ui| {
                    for value in [1_u32, 2, 4] {
                        ui.selectable_value(scale, value, value.to_string());
                    }
                });
            inline_spin_box_with_range(ui, t!("launcher.batch.field_noise"), noise, -1..=3_i32, zoom);
            ui.label(t!("launcher.batch.field_tile"));
            WheelComboBox::from_id_salt(("bpn_w2x_tile", node_id))
                .width(text_width)
                .selected_text(tile_size.to_string())
                .show_ui(ui, |ui| {
                    for value in [128_u32, 256, 512] {
                        ui.selectable_value(tile_size, value, value.to_string());
                    }
                });
        }
        NodeParams::SaveFolder { path, name_prefix } => {
            if inline_path_edit(
                ui,
                t!("launcher.batch.field_folder"),
                path,
                text_width,
                t!("launcher.batch.choose_folder_button"),
            ) {
                path_pick_request = Some(NodePathPurpose::SaveFolderDirectory);
            }
            inline_text_edit(ui, t!("launcher.batch.field_prefix"), name_prefix, text_width);
        }
        NodeParams::VariableRead { variable_name }
        | NodeParams::VariableWrite { variable_name } => {
            ui.label(t!("launcher.batch.field_variable"));
            WheelComboBox::from_id_salt(("bpn_variable", node_id))
                .width(text_width)
                .selected_text(if variable_name.is_empty() {
                    t!("launcher.batch.variable_not_selected")
                } else {
                    variable_name.as_str()
                })
                .show_ui(ui, |ui| {
                    for variable in variables {
                        ui.selectable_value(
                            variable_name,
                            variable.name.clone(),
                            variable.name.as_str(),
                        );
                    }
                });
            if variables.is_empty() {
                ui.label(
                    egui::RichText::new(t!("launcher.batch.create_variable_hint")).text_style(TextStyle::Small),
                );
            }
        }
        NodeParams::QuickDownloader | NodeParams::ScrollPage | NodeParams::End => {}
    }
    path_pick_request
}

/// Labelled single-line text field of `width` points.
fn inline_text_edit(ui: &mut Ui, label: &str, value: &mut String, width: f32) {
    ui.label(label);
    ui.add_sized(
        [width, ui.spacing().interact_size.y],
        egui::TextEdit::singleline(value),
    );
}

/// Labelled path field plus a "..." button, occupying `width` points in total.
///
/// The text field edits `path` directly, so a typed path takes effect immediately. Returns
/// `true` on the frame the "..." button was clicked; the CALLER turns that into a dialog
/// request. This function must never open a dialog itself — it runs on the paint path.
fn inline_path_edit(
    ui: &mut Ui,
    label: &str,
    path: &mut std::path::PathBuf,
    width: f32,
    button_hint: &str,
) -> bool {
    ui.label(label);
    ui.horizontal(|ui| {
        let mut text = path.to_string_lossy().into_owned();
        let response = ui.add_sized(
            [
                width - ui.spacing().interact_size.y - ui.spacing().item_spacing.x,
                ui.spacing().interact_size.y,
            ],
            egui::TextEdit::singleline(&mut text),
        );
        if response.changed() {
            *path = text.into();
        }
        ui.button("...").on_hover_text(button_hint).clicked()
    })
    .inner
}

/// Labelled unbounded numeric field.
///
/// `WheelSpinBox` rather than a raw `egui::DragValue` (`egui-docs/04-widgets.md` §0.2): it
/// steps on hover+wheel, and it zeroes the frame's `smooth_scroll_delta` when it does. That
/// consumption is what stops one notch from stepping the value and zooming the canvas at the
/// same time — but only because `CanvasState::show` reads its own zoom delta AFTER the node
/// pass; a read placed before this widget is drawn cannot see it. The widget is sized from the
/// ambient `Style`, so it follows the node's zoom scaling; `zoom` only scales the drag speed so
/// a drag of one screen point moves the value by roughly the same amount at any zoom.
fn inline_spin_box<T>(ui: &mut Ui, label: &str, value: &mut T, zoom: f32)
where
    T: egui::emath::Numeric,
{
    ui.label(label);
    ui.add(WheelSpinBox::new(value).speed(f64::from(zoom.max(0.25))));
}

/// Labelled numeric field clamped to `range`. See [`inline_spin_box`] for the widget choice.
fn inline_spin_box_with_range<T>(
    ui: &mut Ui,
    label: &str,
    value: &mut T,
    range: std::ops::RangeInclusive<T>,
    zoom: f32,
) where
    T: egui::emath::Numeric,
{
    ui.label(label);
    ui.add(
        WheelSpinBox::new(value)
            .range(range)
            .speed(f64::from(zoom.max(0.25))),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The transform pair must be an exact round trip for every pan/zoom the canvas allows.
    #[test]
    fn transform_round_trips() {
        let origins = [Vec2::ZERO, vec2(220.0, 64.0), vec2(-37.5, 11.25)];
        let pans = [Vec2::ZERO, vec2(37.0, -200.0), vec2(-1234.5, 678.25)];
        let zooms = [ZOOM_MIN, 0.5, 1.0, 1.5, ZOOM_MAX];

        for canvas_origin in origins {
            for pan in pans {
                for zoom in zooms {
                    let state = CanvasState {
                        pan,
                        zoom,
                        ..CanvasState::default()
                    };
                    for world in [Pos2::ZERO, pos2(100.0, 100.0), pos2(-512.0, 2048.0)] {
                        let screen = state.world_to_screen(world, canvas_origin);
                        let back = state.screen_to_world(screen, canvas_origin);
                        assert!(
                            (back.x - world.x).abs() < 1e-3 && (back.y - world.y).abs() < 1e-3,
                            "round trip failed: world={world:?} back={back:?} pan={pan:?} zoom={zoom}"
                        );
                    }
                }
            }
        }
    }

    /// A length must be translation-free: the same under any pan, and exact for large pans
    /// (which is why the grid step is not derived by subtracting two `world_to_screen` calls).
    #[test]
    fn length_transform_ignores_pan() {
        for pan in [Vec2::ZERO, vec2(1.0e8, -1.0e8), vec2(-37.0, 200.0)] {
            let state = CanvasState {
                pan,
                zoom: 0.75,
                ..CanvasState::default()
            };
            assert_eq!(state.world_to_screen_len(GRID_STEP), GRID_STEP * 0.75);
        }
    }

    /// The documented convention itself: `screen = canvas_origin + pan + world * zoom`.
    #[test]
    fn transform_matches_documented_convention() {
        let state = CanvasState {
            pan: vec2(10.0, -20.0),
            zoom: 2.0,
            ..CanvasState::default()
        };
        let screen = state.world_to_screen(pos2(5.0, 7.0), vec2(100.0, 200.0));
        assert_eq!(screen, pos2(100.0 + 10.0 + 10.0, 200.0 - 20.0 + 14.0));
    }

    #[test]
    fn node_order_keeps_existing_z_order_and_appends_new_nodes() {
        let mut order = vec![3, 1, 2];
        reconcile_node_order(&mut order, &[1, 2, 3, 4]);
        assert_eq!(order, vec![3, 1, 2, 4], "new ids must land in front");
    }

    #[test]
    fn node_order_drops_removed_nodes() {
        let mut order = vec![3, 1, 2];
        reconcile_node_order(&mut order, &[2]);
        assert_eq!(order, vec![2]);
    }

    #[test]
    fn node_order_survives_repeated_reconciliation() {
        let mut order = Vec::new();
        reconcile_node_order(&mut order, &[1, 2]);
        reconcile_node_order(&mut order, &[1, 2]);
        assert_eq!(order, vec![1, 2], "reconciliation must be idempotent");
    }

    #[test]
    fn raising_moves_a_node_to_the_front_only() {
        let mut order = vec![1, 2, 3];
        raise_node_to_front(&mut order, 1);
        assert_eq!(order, vec![2, 3, 1]);
        raise_node_to_front(&mut order, 99);
        assert_eq!(order, vec![2, 3, 1], "unknown ids must be ignored");
    }

    #[test]
    fn size_cache_evicts_deleted_nodes() {
        let mut sizes = HashMap::from([
            (1_u32, vec2(280.0, 120.0)),
            (2, vec2(280.0, 90.0)),
            (3, vec2(280.0, 70.0)),
        ]);
        evict_stale_node_sizes(&mut sizes, &[2]);
        assert_eq!(sizes.len(), 1);
        assert!(sizes.contains_key(&2));
    }

    #[test]
    fn estimated_size_prefers_the_measured_cache() {
        let mut state = CanvasState::new();
        let fresh = state.estimated_node_world_size(7, 2);
        assert_eq!(fresh.x, NODE_WIDTH);
        assert_eq!(
            fresh.y,
            HEADER_HEIGHT + 2.0 * SOCKET_ROW_HEIGHT + ESTIMATED_PARAM_ROWS * PARAM_ROW_HEIGHT + 36.0
        );

        state.node_sizes.insert(7, vec2(NODE_WIDTH, 321.0));
        assert_eq!(
            state.estimated_node_world_size(7, 2),
            vec2(NODE_WIDTH, 321.0)
        );
    }

    /// The real grid-line placement must be anchored in GRAPH space: a world point keeps the
    /// same position inside its grid cell under every pan and every zoom, which is what makes
    /// a node sit on the same intersection while the user navigates.
    ///
    /// This drives `CanvasState::grid_line_screen_x` / `_y` — the functions `draw_grid`
    /// actually places its lines with — rather than re-deriving a position from
    /// `world_to_screen`, which would only prove the transform is affine.
    ///
    /// It fails against the old formula. That one emitted lines at
    /// `rect.left() + pan - pan.rem_euclid(step_px) + k * step_px`, i.e. at exact multiples of
    /// `step_px` from the canvas edge, with the sub-cell part of `pan` discarded. Worked
    /// example, `pan.x = 37`, `zoom = 1`, `world.x = 77` (`step_px = 32`, cell index 2): the
    /// old line lands at `left + 37 - 5 + 64 = left + 96` while the point is at
    /// `left + 37 + 77 = left + 114`, so the measured phase is `18/32 = 0.5625` instead of
    /// `frac(77/32) = 0.40625` — a drift of 0.156 cells against a tolerance of 1e-4.
    #[test]
    fn grid_line_phase_is_invariant_under_pan_and_zoom() {
        let world = pos2(77.0, -413.5);
        // Where the point sits inside its cell, as a fraction, computed from world units alone.
        let expected = (
            (world.x / GRID_STEP).rem_euclid(1.0),
            (world.y / GRID_STEP).rem_euclid(1.0),
        );
        // The index of the cell the point is in — the line `draw_grid` paints at its start.
        let index_x = (world.x / GRID_STEP).floor() as i64;
        let index_y = (world.y / GRID_STEP).floor() as i64;

        for canvas_origin in [Vec2::ZERO, vec2(220.0, 64.0)] {
            // Every pan here has a non-integral cell offset at at least one zoom, which is
            // precisely what the old formula threw away.
            for pan in [
                Vec2::ZERO,
                vec2(37.0, -200.0),
                vec2(-1234.5, 678.25),
                vec2(8.125, 8.125),
            ] {
                for zoom in [ZOOM_MIN, 0.5, 1.0, 1.5, ZOOM_MAX] {
                    let state = CanvasState {
                        pan,
                        zoom,
                        ..CanvasState::default()
                    };
                    let step_px = state.world_to_screen_len(GRID_STEP);
                    let point_px = state.world_to_screen(world, canvas_origin);

                    let offset = (
                        (point_px.x - state.grid_line_screen_x(index_x, canvas_origin)) / step_px,
                        (point_px.y - state.grid_line_screen_y(index_y, canvas_origin)) / step_px,
                    );
                    assert!(
                        (offset.0 - expected.0).abs() < 1e-4
                            && (offset.1 - expected.1).abs() < 1e-4,
                        "grid phase drifted: offset={offset:?} expected={expected:?} \
                         pan={pan:?} zoom={zoom} origin={canvas_origin:?}"
                    );
                }
            }
        }
    }

    /// Adjacent grid lines must sit exactly one screen cell apart, whatever the pan. The old
    /// formula satisfied this too — it is the companion of the phase test above, not a
    /// replacement for it: together they pin both the spacing and the origin of the grid.
    #[test]
    fn adjacent_grid_lines_are_one_screen_cell_apart() {
        for pan in [Vec2::ZERO, vec2(37.0, -200.0), vec2(-1234.5, 678.25)] {
            for zoom in [ZOOM_MIN, 1.0, ZOOM_MAX] {
                let state = CanvasState {
                    pan,
                    zoom,
                    ..CanvasState::default()
                };
                let origin = vec2(220.0, 64.0);
                let step_px = state.world_to_screen_len(GRID_STEP);
                for index in [-3_i64, 0, 5] {
                    let delta = state.grid_line_screen_x(index + 1, origin)
                        - state.grid_line_screen_x(index, origin);
                    assert!(
                        (delta - step_px).abs() < 1e-3,
                        "line spacing {delta} != {step_px} at pan={pan:?} zoom={zoom}"
                    );
                }
            }
        }
    }

    /// Every index the grid will actually paint must survive the `i64 -> f32 -> i64` round
    /// trip that placing a line performs; past the limit the grid is refused outright.
    #[test]
    fn grid_line_indices_stay_exact_in_f32() {
        let limit = GRID_INDEX_LIMIT as i64;
        for index in [-limit, -limit + 1, -7, 0, 7, limit - 1, limit] {
            let world = grid_line_world(index);
            assert_eq!(
                (world / GRID_STEP) as i64,
                index,
                "index {index} did not survive the f32 round trip"
            );
        }
        // One past the limit is exactly where f32 stops representing every integer: 2^24 + 1
        // rounds back to 2^24. That is why `grid_line_range` must refuse it outright instead
        // of painting a grid whose lines have silently collapsed onto each other.
        assert_eq!((limit + 1) as f32, limit as f32);
    }

    /// A span whose indices would leave [`GRID_INDEX_LIMIT`] yields no grid at all; the span
    /// just inside it still does.
    #[test]
    fn grid_line_range_stops_at_the_exact_f32_index_limit() {
        let limit_world = GRID_INDEX_LIMIT * GRID_STEP;
        assert!(
            grid_line_range(0.0, limit_world, GRID_STEP).is_some(),
            "the last exactly representable index must still be painted"
        );
        assert!(
            grid_line_range(0.0, limit_world * 2.0, GRID_STEP).is_none(),
            "an index past 2^24 must disable the grid, not mis-phase it"
        );
        assert!(
            grid_line_range(-limit_world * 2.0, 0.0, GRID_STEP).is_none(),
            "the negative side is bounded too"
        );
    }

    /// The transform pair for extents must round-trip, and must ignore `pan` entirely.
    #[test]
    fn size_transform_round_trips_and_ignores_pan() {
        for pan in [Vec2::ZERO, vec2(1.0e8, -1.0e8)] {
            for zoom in [ZOOM_MIN, 1.0, ZOOM_MAX] {
                let state = CanvasState {
                    pan,
                    zoom,
                    ..CanvasState::default()
                };
                let world = vec2(280.0, 137.5);
                let screen = state.world_to_screen_size(world);
                assert_eq!(screen, world * zoom);
                let back = state.screen_to_world_size(screen);
                assert!((back.x - world.x).abs() < 1e-3 && (back.y - world.y).abs() < 1e-3);
            }
        }
    }

    /// The number of emitted lines must depend on the VISIBLE area only. At a pan of one
    /// million world units the old implementation walked from the world origin and emitted
    /// tens of thousands of off-screen segments per frame.
    #[test]
    fn grid_line_count_depends_only_on_the_visible_area() {
        let canvas_rect = Rect::from_min_size(pos2(220.0, 64.0), vec2(970.0, 720.0));
        for pan in [Vec2::ZERO, Vec2::splat(1.0e6), Vec2::splat(-1.0e6)] {
            for zoom in [ZOOM_MIN, 1.0, ZOOM_MAX] {
                let state = CanvasState {
                    pan,
                    zoom,
                    ..CanvasState::default()
                };
                let view = state.visible_world_rect(canvas_rect);
                let range = grid_line_range(view.min.x, view.max.x, GRID_STEP)
                    .expect("visible span must yield a line range");
                let count = range.end() - range.start() + 1;
                // Upper bound for this rect: width / (GRID_STEP * ZOOM_MIN) plus the two
                // partially visible edge lines.
                let bound = (canvas_rect.width() / (GRID_STEP * ZOOM_MIN)).ceil() as i64 + 2;
                assert!(
                    count <= bound,
                    "unbounded grid: {count} lines (bound {bound}) at pan={pan:?} zoom={zoom}"
                );
            }
        }
    }

    /// The visible span must actually be covered — one line before it and one after.
    #[test]
    fn grid_line_range_brackets_the_visible_span() {
        let range = grid_line_range(-70.0, 70.0, GRID_STEP).expect("finite span");
        assert_eq!(*range.start(), -3, "floor(-70/32) = -3");
        assert_eq!(*range.end(), 3, "ceil(70/32) = 3");
    }

    /// A degenerate span must yield no grid rather than an unbounded loop.
    #[test]
    fn grid_line_range_rejects_unusable_spans() {
        assert!(grid_line_range(0.0, 100.0, 0.0).is_none(), "zero step");
        assert!(grid_line_range(f32::NAN, 100.0, GRID_STEP).is_none(), "NaN");
        assert!(
            grid_line_range(0.0, f32::INFINITY, GRID_STEP).is_none(),
            "infinite span"
        );
        assert!(
            grid_line_range(-1.0e30, 1.0e30, GRID_STEP).is_none(),
            "index would saturate on the f32 -> i64 cast"
        );
    }

    /// The fine level must ramp, not pop: monotone, clamped at both ends, and strictly
    /// between them inside the fade band.
    #[test]
    fn fine_grid_fades_instead_of_popping() {
        assert_eq!(fine_grid_strength(GRID_FINE_FADE_OUT_PX), 0.0);
        assert_eq!(fine_grid_strength(GRID_FINE_FADE_OUT_PX - 1.0), 0.0);
        assert_eq!(fine_grid_strength(GRID_FINE_FADE_FULL_PX), 1.0);
        assert_eq!(fine_grid_strength(GRID_FINE_FADE_FULL_PX + 100.0), 1.0);
        assert_eq!(fine_grid_strength(f32::NAN), 0.0);

        let mid = fine_grid_strength((GRID_FINE_FADE_OUT_PX + GRID_FINE_FADE_FULL_PX) * 0.5);
        assert!(mid > 0.0 && mid < 1.0, "mid-band strength was {mid}");

        // Monotone across the whole zoom range the canvas allows.
        let mut previous = -1.0_f32;
        for step in 0_i16..=100 {
            let zoom = ZOOM_MIN + (ZOOM_MAX - ZOOM_MIN) * f32::from(step) / 100.0;
            let strength = fine_grid_strength(GRID_STEP * zoom);
            assert!(strength >= previous, "strength must not decrease with zoom");
            previous = strength;
        }
    }

    /// Fading toward the ground keeps every grid colour opaque, which is what stops the
    /// additive-white streak the premultiplied `Color32` contract produced before.
    #[test]
    fn grid_colours_are_opaque_at_every_fade_level() {
        for strength in [0.0, 0.25, 0.5, 1.0] {
            let color = COL_GROUND.lerp_to_gamma(COL_GRID_FINE, strength);
            assert_eq!(color.a(), 255, "grid colour must never be alpha-blended");
        }
        assert_eq!(COL_GROUND.lerp_to_gamma(COL_GRID_FINE, 0.0), COL_GROUND);
        assert_eq!(COL_GROUND.lerp_to_gamma(COL_GRID_FINE, 1.0), COL_GRID_FINE);
        assert_eq!(COL_GRID_COARSE.a(), 255);
        assert_eq!(COL_GROUND.a(), 255);
    }

    /// A node created after the user panned away must land inside the visible world area.
    #[test]
    fn spawn_origin_follows_the_view() {
        let canvas_rect = Rect::from_min_size(pos2(220.0, 64.0), vec2(970.0, 720.0));
        for pan in [Vec2::ZERO, vec2(-4000.0, 2500.0), vec2(1500.0, -900.0)] {
            for zoom in [ZOOM_MIN, 1.0, ZOOM_MAX] {
                let state = CanvasState {
                    pan,
                    zoom,
                    last_canvas_rect: Some(canvas_rect),
                    ..CanvasState::default()
                };
                let spawn = state.visible_spawn_origin();
                assert!(
                    state.visible_world_rect(canvas_rect).contains(spawn),
                    "spawn {spawn:?} outside the view at pan={pan:?} zoom={zoom}"
                );
            }
        }
    }

    /// Before the canvas has ever been laid out there is no view to anchor to; the fallback
    /// must be defined rather than panicking on a missing rect.
    #[test]
    fn spawn_origin_has_a_defined_fallback() {
        assert_eq!(CanvasState::new().visible_spawn_origin(), Pos2::ZERO);
    }

    #[test]
    fn socket_hit_rect_is_centred_and_scales_with_zoom() {
        let rect = socket_hit_rect(pos2(10.0, 20.0), 2.0);
        assert_eq!(rect.center(), pos2(10.0, 20.0));
        assert_eq!(rect.width(), SOCKET_RADIUS * 2.0 * 2.5);
        assert_eq!(rect.height(), rect.width());
    }
}
