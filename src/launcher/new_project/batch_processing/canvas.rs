/*
File: src/launcher/new_project/batch_processing/canvas.rs

Purpose:
Interactive egui node-graph canvas with pan, zoom, node rendering, and connection dragging.

Main responsibilities:
- Render all graph nodes as styled boxes with sockets using egui Painter
- Render editable node parameter controls directly inside the node body
- Draw Bezier curves for exec and data connections
- Handle mouse interaction: pan (middle-mouse / space+drag), zoom (scroll), node drag,
  socket-to-socket connection drag, node selection, and Delete key removal
- Return canvas-level actions (connection dropped, nodes deleted) to the caller

Key structures:
- CanvasState  — persistent UI state (pan, zoom, selection, drag)
- SocketRef    — identifies a specific socket on a node
- CanvasAction — events emitted by the canvas for the window to act on

Notes:
All coordinates in "world space" are stored in GraphNode.pos.
Canvas-to-screen transform: screen_pos = canvas_origin + pan + world_pos * zoom.
Painter draws directly; there are no retained graphics items.
*/

use super::graph::{EdgeKind, GraphModel};
use super::node_defs::NodeDefs;
use super::types::{BrowserKind, NodeParams};
use egui::{Color32, Pos2, Rect, Sense, Stroke, Style, TextStyle, Ui, UiBuilder, Vec2, pos2, vec2};
use std::collections::HashSet;

// ─── Layout constants ─────────────────────────────────────────────────────────

const NODE_WIDTH: f32 = 280.0;
const HEADER_HEIGHT: f32 = 28.0;
const SOCKET_ROW_HEIGHT: f32 = 24.0;
const SOCKET_RADIUS: f32 = 6.0;
const PARAM_ROW_HEIGHT: f32 = 30.0;
const NODE_ROUNDING: f32 = 6.0;
const GRID_STEP: f32 = 32.0;
const ZOOM_MIN: f32 = 0.25;
const ZOOM_MAX: f32 = 2.5;
const ZOOM_STEP: f32 = 0.12;

const COL_HEADER: Color32 = Color32::from_rgb(0x2d, 0x3a, 0x4a);
const COL_BODY: Color32 = Color32::from_rgb(0x1e, 0x26, 0x33);
const COL_SELECTED: Color32 = Color32::from_rgb(0x3b, 0x82, 0xf6);
const COL_ACTIVE: Color32 = Color32::from_rgb(0xf5, 0x9e, 0x0b);
const COL_GRID: Color32 = Color32::from_rgba_premultiplied(0xff, 0xff, 0xff, 0x0d);
const COL_TEXT: Color32 = Color32::from_rgb(0xe2, 0xe8, 0xf0);
const COL_TEXT_DIM: Color32 = Color32::from_rgb(0x94, 0xa3, 0xb8);

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
    /// Delete key pressed; the set of node ids to remove.
    DeleteSelected,
}

// ─── Drag state ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct DragSocket {
    origin: SocketRef,
    origin_screen_pos: Pos2,
    /// Current mouse position in screen space.
    current_screen_pos: Pos2,
}

struct NodeRenderOutput {
    rect: Rect,
    socket_positions: Vec<(SocketRef, Pos2)>,
    interactive_rects: Vec<Rect>,
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
    /// Node currently being dragged by its free background area.
    dragging_node: Option<u32>,
    /// True while Space is held (enables pan-by-drag).
    space_held: bool,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            pan: Vec2::ZERO,
            zoom: 1.0,
            drag_socket: None,
            selected_nodes: HashSet::new(),
            dragging_node: None,
            space_held: false,
        }
    }
}

impl CanvasState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Main entry point: render the node graph and return any actions the caller must handle.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        model: &mut GraphModel,
        defs: &NodeDefs,
        active_node_id: Option<u32>,
    ) -> Vec<CanvasAction> {
        let mut actions = Vec::new();

        let (resp, painter) =
            ui.allocate_painter(ui.available_size(), egui::Sense::click_and_drag());

        let canvas_rect = resp.rect;
        let canvas_origin = canvas_rect.min.to_vec2();

        // ── Keyboard input ─────────────────────────────────────────────────
        let delete_pressed = ui.input(|i| i.key_pressed(egui::Key::Delete));
        self.space_held = ui.input(|i| i.key_down(egui::Key::Space));

        if delete_pressed && !self.selected_nodes.is_empty() {
            actions.push(CanvasAction::DeleteSelected);
        }

        // ── Pan ────────────────────────────────────────────────────────────
        if resp.dragged_by(egui::PointerButton::Middle)
            || (self.space_held && resp.dragged_by(egui::PointerButton::Primary))
        {
            self.pan += resp.drag_delta();
        }

        // ── Zoom ───────────────────────────────────────────────────────────
        let scroll_delta = ui.input(|i| i.smooth_scroll_delta.y);
        if canvas_rect.contains(ui.input(|i| i.pointer.hover_pos().unwrap_or_default()))
            && scroll_delta != 0.0
        {
            let mouse_pos = ui.input(|i| i.pointer.hover_pos().unwrap_or(canvas_rect.center()));
            let old_zoom = self.zoom;
            self.zoom = (self.zoom + scroll_delta * ZOOM_STEP * 0.1).clamp(ZOOM_MIN, ZOOM_MAX);
            // Zoom toward the mouse cursor.
            let mouse_local = mouse_pos.to_vec2() - canvas_origin;
            let mouse_world = (mouse_local - self.pan) / old_zoom;
            self.pan = mouse_local - mouse_world * self.zoom;
        }

        // ── Background grid ────────────────────────────────────────────────
        draw_grid(&painter, canvas_rect, self.pan, self.zoom);

        // ── Node hit testing: determine hovered socket / node ──────────────
        let mouse_pos: Option<Pos2> = ui.input(|i| i.pointer.hover_pos());

        // ── Process node dragging and draw nodes ──────────────────────────
        // Collect layout info (socket screen positions) for edge drawing afterwards.
        let mut socket_screen_positions: Vec<(SocketRef, Pos2)> = Vec::new();

        // Determine what the user clicked on (for selection logic).
        let left_clicked = resp.clicked();
        let left_press = ui.input(|i| i.pointer.button_down(egui::PointerButton::Primary));
        let left_pressed = ui.input(|i| i.pointer.button_pressed(egui::PointerButton::Primary));
        let mut clicked_node_this_frame = false;

        // Clone node ids to avoid borrow issues while we mutate positions.
        let node_ids: Vec<u32> = model.nodes.iter().map(|n| n.id).collect();
        let variables_snapshot = model.variables.clone();

        for node_id in node_ids {
            let node = match model.node_by_id(node_id) {
                Some(n) => n.clone(),
                None => continue,
            };

            let sockets =
                defs.socket_specs_for_node(node.template_key(), &node.params, &variables_snapshot);

            let param_rows = param_row_count(&node.params);
            let estimated_node_height = HEADER_HEIGHT
                + sockets.len() as f32 * SOCKET_ROW_HEIGHT
                + param_rows as f32 * PARAM_ROW_HEIGHT
                + 36.0;

            let world_pos = node.pos;
            let screen_pos = self.world_to_screen(world_pos, canvas_origin);
            let estimated_node_rect = Rect::from_min_size(
                screen_pos,
                vec2(NODE_WIDTH * self.zoom, estimated_node_height * self.zoom),
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
                    estimated_rect: estimated_node_rect,
                    params: node_params,
                    variables: &variables_snapshot,
                    sockets: &sockets,
                    is_selected,
                    is_active: active_node_id == Some(node_id),
                    canvas_rect,
                },
            );
            if left_clicked
                && let Some(pointer_pos) = mouse_pos
                && node_render.rect.contains(pointer_pos)
            {
                clicked_node_this_frame = true;
            }
            let pointer_hits_node_widget = mouse_pos.is_some_and(|pointer_pos| {
                node_render
                    .interactive_rects
                    .iter()
                    .any(|rect| rect.contains(pointer_pos))
            });
            let pointer_hits_socket = mouse_pos.is_some_and(|pointer_pos| {
                let radius = SOCKET_RADIUS * self.zoom * 1.25;
                node_render
                    .socket_positions
                    .iter()
                    .any(|(_, socket_pos)| socket_pos.distance(pointer_pos) <= radius)
            });
            let pointer_hits_drag_area = mouse_pos.is_some_and(|pointer_pos| {
                node_render.rect.contains(pointer_pos)
                    && !pointer_hits_node_widget
                    && !pointer_hits_socket
            });
            for (socket_ref, socket_screen) in &node_render.socket_positions {
                let radius = SOCKET_RADIUS * self.zoom;
                let sock_rect = Rect::from_center_size(*socket_screen, Vec2::splat(radius * 2.5));
                let sock_resp = ui.interact(
                    sock_rect,
                    egui::Id::new((
                        "bpn_sock",
                        socket_ref.node_id,
                        socket_ref.socket_name.as_str(),
                        socket_ref.is_input,
                    )),
                    Sense::drag(),
                );

                if sock_resp.drag_started() {
                    self.drag_socket = Some(DragSocket {
                        origin: socket_ref.clone(),
                        origin_screen_pos: *socket_screen,
                        current_screen_pos: *socket_screen,
                    });
                }

                if let Some(drag) = &self.drag_socket
                    && sock_resp.hovered()
                    && !left_press
                {
                    let src = drag.origin.clone();
                    let dst = socket_ref.clone();
                    if src != dst && src.node_id != dst.node_id {
                        actions.push(CanvasAction::ConnectSockets { src, dst });
                    }
                }
            }
            socket_screen_positions.extend(node_render.socket_positions);

            if left_pressed && !self.space_held && pointer_hits_drag_area {
                self.dragging_node = Some(node_id);
                if !is_selected {
                    if !ui.input(|i| i.modifiers.shift) {
                        self.selected_nodes.clear();
                    }
                    self.selected_nodes.insert(node_id);
                }
            }

            if self.dragging_node == Some(node_id) && left_press {
                let delta = ui.input(|i| i.pointer.delta()) / self.zoom;
                if let Some(n) = model.node_by_id_mut(node_id) {
                    n.pos += delta;
                }
            }
            if self.dragging_node == Some(node_id) && !left_press {
                self.dragging_node = None;
            }

            if left_clicked && pointer_hits_drag_area {
                if !ui.input(|i| i.modifiers.shift) {
                    self.selected_nodes.clear();
                }
                self.selected_nodes.insert(node_id);
            }
        }

        // ── Update drag position ────────────────────────────────────────────
        if let Some(drag) = &mut self.drag_socket {
            if let Some(pos) = mouse_pos {
                drag.current_screen_pos = pos;
            }
            if !left_press {
                self.drag_socket = None;
            }
        }

        // ── Draw edges ─────────────────────────────────────────────────────
        for edge in &model.edges {
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
                let color = match edge.kind {
                    EdgeKind::Exec => Color32::from_rgb(0xfa, 0xcc, 0x15),
                    EdgeKind::Data => Color32::from_rgb(0x94, 0xa3, 0xb8),
                };
                draw_bezier(&painter, p0, p3, color, 2.0 * self.zoom);
            }
        }

        // ── Draw in-progress connection ────────────────────────────────────
        if let Some(drag) = &self.drag_socket {
            let color = Color32::WHITE.gamma_multiply(0.6);
            draw_bezier(
                &painter,
                drag.origin_screen_pos,
                drag.current_screen_pos,
                color,
                1.5,
            );
        }

        // ── Deselect on background click ────────────────────────────────────
        if left_clicked
            && self.dragging_node.is_none()
            && self.drag_socket.is_none()
            && !clicked_node_this_frame
        {
            self.selected_nodes.clear();
        }

        actions
    }

    // ─── Coordinate helpers ───────────────────────────────────────────────────

    pub fn world_to_screen(&self, world: Pos2, canvas_origin: Vec2) -> Pos2 {
        pos2(
            canvas_origin.x + self.pan.x + world.x * self.zoom,
            canvas_origin.y + self.pan.y + world.y * self.zoom,
        )
    }

    pub fn selected_nodes(&self) -> &HashSet<u32> {
        &self.selected_nodes
    }

    pub fn clear_selection(&mut self) {
        self.selected_nodes.clear();
    }

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
        let mut output = NodeRenderOutput {
            rect: estimated_rect,
            socket_positions: Vec::new(),
            interactive_rects: Vec::new(),
        };

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
                        Stroke::new(1.0, Color32::from_black_alpha(0))
                    })
                    .inner_margin(egui::Margin::same((8.0 * self.zoom).round() as i8));

                let frame_response = frame.show(node_ui, |node_ui| {
                    node_ui.set_width(NODE_WIDTH * self.zoom);
                    node_ui.spacing_mut().item_spacing = vec2(6.0 * self.zoom, 6.0 * self.zoom);

                    draw_node_header(node_ui, params.title(), self.zoom);
                    let socket_positions =
                        draw_socket_rows(node_ui, node_id, sockets, self.zoom, canvas_rect);
                    let mut interactive_rects = Vec::new();
                    show_inline_param_editor_ui(
                        node_ui,
                        node_id,
                        params,
                        variables,
                        self.zoom,
                        &mut interactive_rects,
                    );
                    (socket_positions, interactive_rects)
                });

                (
                    frame_response.response.rect,
                    frame_response.inner.0,
                    frame_response.inner.1,
                )
            },
        );

        output.rect = inner.inner.0;
        output.socket_positions = inner.inner.1;
        output.interactive_rects = inner.inner.2;
        output
    }
}

// ─── Drawing helpers ──────────────────────────────────────────────────────────

fn draw_grid(painter: &egui::Painter, rect: Rect, pan: Vec2, zoom: f32) {
    let step = GRID_STEP * zoom;
    if step < 4.0 {
        return;
    }

    let origin_x = rect.left() + pan.x;
    let origin_y = rect.top() + pan.y;

    let offset_x = (origin_x - rect.left()).rem_euclid(step);
    let offset_y = (origin_y - rect.top()).rem_euclid(step);

    let mut x = origin_x - offset_x;
    while x > rect.left() {
        x -= step;
    }
    while x < rect.right() {
        painter.line_segment(
            [pos2(x, rect.top()), pos2(x, rect.bottom())],
            Stroke::new(1.0, COL_GRID),
        );
        x += step;
    }

    let mut y = origin_y - offset_y;
    while y > rect.top() {
        y -= step;
    }
    while y < rect.bottom() {
        painter.line_segment(
            [pos2(rect.left(), y), pos2(rect.right(), y)],
            Stroke::new(1.0, COL_GRID),
        );
        y += step;
    }
}

fn draw_bezier(painter: &egui::Painter, p0: Pos2, p3: Pos2, color: Color32, width: f32) {
    let dx = (p3.x - p0.x).abs().max(80.0) * 0.5;
    let p1 = pos2(p0.x + dx, p0.y);
    let p2 = pos2(p3.x - dx, p3.y);
    let bezier = egui::epaint::CubicBezierShape::from_points_stroke(
        [p0, p1, p2, p3],
        false,
        Color32::TRANSPARENT,
        Stroke::new(width, color),
    );
    painter.add(bezier);
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
    style.visuals.clip_rect_margin *= zoom;
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
        COL_TEXT,
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
        positions.push((socket_ref.clone(), socket_pos));

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

fn show_inline_param_editor_ui(
    ui: &mut Ui,
    node_id: u32,
    params: &mut NodeParams,
    variables: &[super::graph::GraphVariable],
    zoom: f32,
    interactive_rects: &mut Vec<Rect>,
) {
    let text_width = (NODE_WIDTH - 56.0).max(80.0) * zoom;
    match params {
        NodeParams::StartNumber { start, step, end } => {
            inline_drag_value(ui, t!("launcher.batch.field_start"), start, zoom, interactive_rects);
            inline_drag_value(ui, t!("launcher.batch.field_step"), step, zoom, interactive_rects);
            inline_drag_value(ui, t!("launcher.batch.field_end"), end, zoom, interactive_rects);
        }
        NodeParams::StartString { path } => {
            inline_path_edit(
                ui,
                t!("launcher.batch.field_file"),
                path,
                text_width,
                t!("launcher.batch.open_text_file_button"),
                || {
                    // Native file picker (`rfd`); on web there is no dialog, so the
                    // picker yields no path (logged as a dropped capability).
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        rfd::FileDialog::new()
                            .add_filter(t!("launcher.batch.text_file_label"), &["txt"])
                            .pick_file()
                    }
                    #[cfg(target_arch = "wasm32")]
                    {
                        crate::runtime_log::log_warn("file picker unavailable on web build");
                        None::<std::path::PathBuf>
                    }
                },
                interactive_rects,
            );
        }
        NodeParams::StringTemplate {
            template,
            placeholders,
        } => {
            inline_text_edit(ui, t!("launcher.batch.field_template"), template, text_width, interactive_rects);

            let mut placeholder_text = placeholders.join(", ");
            ui.label(egui::RichText::new(t!("launcher.batch.fields_comma_separated")).text_style(TextStyle::Small));
            let response = ui.add_sized(
                [text_width, ui.spacing().interact_size.y],
                egui::TextEdit::singleline(&mut placeholder_text),
            );
            interactive_rects.push(response.rect);
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
            let response = egui::ComboBox::from_id_salt(("bpn_browser", node_id))
                .width(text_width)
                .selected_text(browser.label())
                .show_ui(ui, |ui| {
                    for candidate in BrowserKind::all() {
                        ui.selectable_value(browser, candidate.clone(), candidate.label());
                    }
                });
            interactive_rects.push(response.response.rect);
        }
        NodeParams::FetchFromBrowser { pattern } => {
            inline_text_edit(ui, t!("launcher.batch.field_url_pattern"), pattern, text_width, interactive_rects);
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
            interactive_rects.push(ui.checkbox(auto_cut, t!("launcher.batch.section_autocut")).rect);
            inline_drag_value_with_range(
                ui,
                t!("launcher.batch.field_target_height"),
                target_height,
                500..=20_000,
                zoom,
                interactive_rects,
            );
            inline_drag_value_with_range(
                ui,
                t!("launcher.batch.field_stitch_bands"),
                band_rows,
                1..=50,
                zoom,
                interactive_rects,
            );
            inline_drag_value_with_range(
                ui,
                t!("launcher.batch.field_tolerance"),
                tolerance,
                0..=255_u8,
                zoom,
                interactive_rects,
            );
            inline_drag_value_with_range(
                ui,
                t!("launcher.batch.field_search_radius"),
                search_radius,
                100..=10_000,
                zoom,
                interactive_rects,
            );
            interactive_rects.push(ui.checkbox(prefer_up_first, t!("launcher.batch.field_search_up_first")).rect);

            ui.label(t!("launcher.batch.field_parts"));
            let mut parts_value = parts.unwrap_or(0);
            let response = ui.add(
                egui::DragValue::new(&mut parts_value)
                    .range(0..=100)
                    .speed(f64::from(zoom.max(0.25))),
            );
            interactive_rects.push(response.rect);
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
            let scale_response = egui::ComboBox::from_id_salt(("bpn_w2x_scale", node_id))
                .width(text_width)
                .selected_text(scale.to_string())
                .show_ui(ui, |ui| {
                    for value in [1_u32, 2, 4] {
                        ui.selectable_value(scale, value, value.to_string());
                    }
                });
            interactive_rects.push(scale_response.response.rect);
            inline_drag_value_with_range(ui, t!("launcher.batch.field_noise"), noise, -1..=3_i32, zoom, interactive_rects);
            ui.label(t!("launcher.batch.field_tile"));
            let tile_response = egui::ComboBox::from_id_salt(("bpn_w2x_tile", node_id))
                .width(text_width)
                .selected_text(tile_size.to_string())
                .show_ui(ui, |ui| {
                    for value in [128_u32, 256, 512] {
                        ui.selectable_value(tile_size, value, value.to_string());
                    }
                });
            interactive_rects.push(tile_response.response.rect);
        }
        NodeParams::SaveFolder { path, name_prefix } => {
            inline_path_edit(
                ui,
                t!("launcher.batch.field_folder"),
                path,
                text_width,
                t!("launcher.batch.choose_folder_button"),
                || {
                    // Native folder picker (`rfd`); unavailable on web.
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        rfd::FileDialog::new().pick_folder()
                    }
                    #[cfg(target_arch = "wasm32")]
                    {
                        crate::runtime_log::log_warn("folder picker unavailable on web build");
                        None::<std::path::PathBuf>
                    }
                },
                interactive_rects,
            );
            inline_text_edit(ui, t!("launcher.batch.field_prefix"), name_prefix, text_width, interactive_rects);
        }
        NodeParams::VariableRead { variable_name }
        | NodeParams::VariableWrite { variable_name } => {
            ui.label(t!("launcher.batch.field_variable"));
            let response = egui::ComboBox::from_id_salt(("bpn_variable", node_id))
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
            interactive_rects.push(response.response.rect);
            if variables.is_empty() {
                ui.label(
                    egui::RichText::new(t!("launcher.batch.create_variable_hint")).text_style(TextStyle::Small),
                );
            }
        }
        NodeParams::QuickDownloader | NodeParams::ScrollPage | NodeParams::End => {}
    }
}

fn inline_text_edit(
    ui: &mut Ui,
    label: &str,
    value: &mut String,
    width: f32,
    interactive_rects: &mut Vec<Rect>,
) {
    ui.label(label);
    let response = ui.add_sized(
        [width, ui.spacing().interact_size.y],
        egui::TextEdit::singleline(value),
    );
    interactive_rects.push(response.rect);
}

fn inline_path_edit<F>(
    ui: &mut Ui,
    label: &str,
    path: &mut std::path::PathBuf,
    width: f32,
    button_hint: &str,
    pick_path: F,
    interactive_rects: &mut Vec<Rect>,
) where
    F: FnOnce() -> Option<std::path::PathBuf>,
{
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
        interactive_rects.push(response.rect);
        if response.changed() {
            *path = text.into();
        }
        let button_response = ui.button("...").on_hover_text(button_hint);
        interactive_rects.push(button_response.rect);
        if button_response.clicked()
            && let Some(selected_path) = pick_path()
        {
            *path = selected_path;
        }
    });
}

fn inline_drag_value<T>(
    ui: &mut Ui,
    label: &str,
    value: &mut T,
    zoom: f32,
    interactive_rects: &mut Vec<Rect>,
) where
    T: egui::emath::Numeric,
{
    ui.label(label);
    let response = ui.add(egui::DragValue::new(value).speed(f64::from(zoom.max(0.25))));
    interactive_rects.push(response.rect);
}

fn inline_drag_value_with_range<T>(
    ui: &mut Ui,
    label: &str,
    value: &mut T,
    range: std::ops::RangeInclusive<T>,
    zoom: f32,
    interactive_rects: &mut Vec<Rect>,
) where
    T: egui::emath::Numeric,
{
    ui.label(label);
    let response = ui.add(
        egui::DragValue::new(value)
            .range(range)
            .speed(f64::from(zoom.max(0.25))),
    );
    interactive_rects.push(response.rect);
}

/// Number of parameter rows to render below the sockets (visual height estimate).
fn param_row_count(params: &NodeParams) -> usize {
    match params {
        NodeParams::StartNumber { .. } => 3, // start / step / end
        NodeParams::StartString { .. } => 1, // path
        NodeParams::StringTemplate { placeholders, .. } => 1 + placeholders.len(),
        NodeParams::OpenUrl { .. } => 1,          // browser combo
        NodeParams::FetchFromBrowser { .. } => 1, // pattern
        NodeParams::StitchSplit { .. } => 4,
        NodeParams::Waifu2x { .. } => 3,
        NodeParams::SaveFolder { .. } => 3,
        NodeParams::VariableRead { .. } | NodeParams::VariableWrite { .. } => 1,
        _ => 0,
    }
}
