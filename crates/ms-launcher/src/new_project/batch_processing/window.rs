/*
File: src/launcher/new_project/batch_processing/window.rs

Purpose:
Main UI orchestrator for the batch node-based processing window.

Main responsibilities:
- Render the toolbar (Save / Load / Run / Stop), left palette panel, variables panel,
  and the central node canvas
- Poll the executor channel and display progress / errors in the status bar
- Handle canvas actions (connect sockets, delete nodes, open a node's path dialog)
- Keep all node parameters embedded directly into the node body on the canvas
- Own every file dialog and every filesystem access of this window, and run them on a
  worker thread

Key structures:
- BatchProcessingWindowState — root state for the entire window
- FileTaskEvent — result of one off-thread file dialog / filesystem task

File dialogs and the GUI thread:
A native file dialog blocks until the user answers it, and reading or writing a graph is
filesystem work; neither may run on the GUI thread (`AGENTS.md` §5). Both therefore live in
`spawn_graph_save` / `spawn_graph_load` / `spawn_node_path_pick`, which return a
`Receiver<FileTaskEvent>` that `poll_file_task` drains once per frame — the same
dialog-off-thread shape used by the cleaning tab's watermark library window. At most one such
task exists at a time (`file_task_rx`), so a second click while a dialog is open is dropped
rather than stacking two dialogs.

A task never outlives the window that started it. This state struct is owned by the parent
window and is NOT recreated between openings, so `on_window_closed` drops the in-flight
receiver on both close paths: the worker's `send` then fails (which it already reports as a log
line) instead of parking an answer that the next opening would apply to a different graph.

Only the dialog itself differs between targets (`pick_path`): the web build has no `rfd`, so it
answers with a ready-made failure event, which the task bodies report rather than silently
treating as a cancellation. Everything else — the worker, the channel, the events and the poll —
is one code path on every target.

Notes:
The window is opened as a native `ctx.show_viewport_immediate()` from
`new_project/window.rs`.  It does not own a project; it operates on standalone
image pipelines and saves results to user-specified folders.
*/

use super::canvas::{CanvasAction, CanvasState, NodePathPurpose};
use super::executor::{ExecutorEvent, GraphSnapshot, spawn_executor};
use super::graph::{GraphModel, GraphVariable};
use super::node_defs::NodeDefs;
use super::types::{DataType, NodeParams};
use egui::{Color32, RichText, ScrollArea, Ui, ViewportClass};
use ms_thread as thread;
use ms_widgets::WheelComboBox;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

const LEFT_PANEL_WIDTH: f32 = 220.0;
const STATUS_BAR_HEIGHT: f32 = 28.0;

/// Poll interval used while an off-thread file dialog is open.
///
/// egui only repaints on demand, so without an explicit wake-up a finished dialog would sit
/// unread in its channel until the user happened to move the mouse over the window. 100 ms is
/// invisible to a human answering a dialog and costs ten idle frames per second.
const FILE_TASK_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Screen-space offset (points) added between two consecutively created nodes.
///
/// Points rather than world units, like `SPAWN_INSET_PX` and for the same reason: the stagger
/// exists so a run of new nodes reads as a cascade on screen, which it only does if it is the
/// same size on screen at every zoom.
const SPAWN_STAGGER_PX: f32 = 30.0;

/// Screen-space offset (points) at which the stagger wraps back to the spawn origin, so the
/// cascade stays inside the viewport instead of walking out of it.
const SPAWN_STAGGER_WRAP_PX: f32 = 300.0;

// ─── Off-thread file dialogs ──────────────────────────────────────────────────

/// Result of one off-thread file dialog / filesystem task.
///
/// Every task answers with exactly one of these and then drops its sender. `Cancelled` is a
/// normal outcome (the user dismissed the dialog) and leaves the graph untouched; `Failed`
/// carries both halves required by `AGENTS.md` §7 — a translated sentence for the status bar
/// and an untranslated, context-rich line for the runtime log.
#[derive(Debug)]
enum FileTaskEvent {
    /// The user dismissed the dialog without choosing anything.
    Cancelled,
    /// The graph was serialized and written to `path`.
    GraphSaved { path: PathBuf },
    /// `path` was read and parsed as JSON; the caller still has to build the model from it.
    ///
    /// Parsing stops at `serde_json::Value` because that is the expensive, I/O-adjacent half;
    /// turning a `Value` into a `GraphModel` is a small in-memory walk and stays on the GUI
    /// thread, which keeps `GraphModel` off the thread boundary entirely.
    GraphLoaded { path: PathBuf, json: serde_json::Value },
    /// The user picked `path` for the `purpose` parameter of node `node_id`.
    NodePathPicked {
        node_id: u32,
        purpose: NodePathPurpose,
        path: PathBuf,
    },
    /// The task failed. `user_message` is shown; `log_message` is logged.
    Failed {
        user_message: String,
        log_message: String,
    },
}

// ─── State ────────────────────────────────────────────────────────────────────

pub struct BatchProcessingWindowState {
    graph: GraphModel,
    canvas: CanvasState,
    defs: NodeDefs,

    // Left panel tab: 0 = nodes palette, 1 = variables
    left_tab: usize,

    // Executor state
    executor_rx: Option<Receiver<ExecutorEvent>>,
    stop_flag: Arc<AtomicBool>,
    is_running: bool,
    active_node_id: Option<u32>,
    status_message: String,
    status_is_error: bool,

    // Save/load path for the graph JSON.
    save_path: Option<PathBuf>,

    /// The single in-flight file dialog / filesystem task, if any.
    ///
    /// `Some` means a dialog is open or its follow-up I/O is still running; further requests
    /// are refused while it is, so the user cannot stack two native dialogs on top of each
    /// other. Drained by [`BatchProcessingWindowState::poll_file_task`].
    file_task_rx: Option<Receiver<FileTaskEvent>>,

    // Variable editor: add variable form
    var_form_name: String,
    var_form_type: DataType,
    var_form_persist: bool,

    /// Current stagger of the next created node, in SCREEN POINTS from the spawn origin.
    ///
    /// Screen points, not world units: see [`BatchProcessingWindowState::next_spawn_pos`].
    spawn_offset: f32,
}

impl BatchProcessingWindowState {
    pub fn new() -> Self {
        Self {
            graph: GraphModel::new(),
            canvas: CanvasState::new(),
            defs: NodeDefs::build(),
            left_tab: 0,
            executor_rx: None,
            stop_flag: Arc::new(AtomicBool::new(false)),
            is_running: false,
            active_node_id: None,
            status_message: String::new(),
            status_is_error: false,
            save_path: None,
            file_task_rx: None,
            var_form_name: String::new(),
            var_form_type: DataType::Str,
            var_form_persist: false,
            spawn_offset: 0.0,
        }
    }

    /// Main entry point called every frame from the launcher.
    /// Returns false when the window should close.
    pub fn show(&mut self, ui: &mut egui::Ui, _class: ViewportClass) -> bool {
        // The viewport callback hands us a `Ui`; derive the child viewport `Context`
        // (cheap Arc clone) for input polling and executor progress repaints.
        let ctx_owned = ui.ctx().clone();
        let ctx = &ctx_owned;
        if ctx.input(|input| input.viewport().close_requested()) {
            self.on_window_closed();
            return false;
        }

        self.poll_executor(ctx);
        self.poll_file_task(ctx);

        let mut keep_open = true;

        // ── Top toolbar ────────────────────────────────────────────────────
        egui::Panel::top("bp_toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                // Disabled while a dialog is already open: a second native dialog would appear
                // behind the first one with no way to tell which is which.
                let file_idle = self.file_task_rx.is_none();
                if ui
                    .add_enabled(file_idle, egui::Button::new(t!("launcher.batch.save_graph_button")))
                    .clicked()
                {
                    self.save_graph();
                }
                if ui
                    .add_enabled(file_idle, egui::Button::new(t!("launcher.batch.load_graph_button")))
                    .clicked()
                {
                    self.load_graph();
                }
                ui.separator();
                let run_enabled = !self.is_running;
                if ui
                    .add_enabled(run_enabled, egui::Button::new(t!("launcher.batch.run_button")))
                    .clicked()
                {
                    self.begin_run();
                }
                let stop_enabled = self.is_running;
                if ui
                    .add_enabled(stop_enabled, egui::Button::new(t!("launcher.batch.stop_button")))
                    .clicked()
                {
                    self.stop_flag.store(true, Ordering::Relaxed);
                }
                ui.separator();
                if ui.button(t!("launcher.batch.close_button")).clicked() {
                    keep_open = false;
                }
            });
        });

        // ── Status bar ─────────────────────────────────────────────────────
        egui::Panel::bottom("bp_status")
            .exact_size(STATUS_BAR_HEIGHT)
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    if self.is_running {
                        ui.spinner();
                        ui.label(t!("launcher.batch.running_status"));
                        ui.separator();
                    }
                    // A dialog runs on its own thread, so the window stays live and would
                    // otherwise look idle while the user is answering it.
                    if self.file_task_rx.is_some() {
                        ui.spinner();
                        ui.separator();
                    }
                    if !self.status_message.is_empty() {
                        let color = if self.status_is_error {
                            Color32::from_rgb(0xf8, 0x71, 0x71)
                        } else {
                            Color32::from_rgb(0x86, 0xef, 0xac)
                        };
                        ui.label(RichText::new(&self.status_message).color(color));
                    }
                });
            });

        // ── Left panel ─────────────────────────────────────────────────────
        egui::Panel::left("bp_left_panel")
            .exact_size(LEFT_PANEL_WIDTH)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.selectable_label(self.left_tab == 0, t!("launcher.batch.nodes_palette_title")).clicked() {
                        self.left_tab = 0;
                    }
                    if ui
                        .selectable_label(self.left_tab == 1, t!("launcher.batch.variables_title"))
                        .clicked()
                    {
                        self.left_tab = 1;
                    }
                });
                ui.separator();
                match self.left_tab {
                    0 => self.show_palette_panel(ui),
                    1 => self.show_variables_panel(ui),
                    _ => {}
                }
            });

        // ── Central canvas ─────────────────────────────────────────────────
        egui::CentralPanel::default().show(ui, |ui| {
            let actions = self
                .canvas
                .show(ui, &mut self.graph, &self.defs, self.active_node_id);
            self.handle_canvas_actions(actions);
        });

        if !keep_open {
            self.on_window_closed();
        }
        keep_open
    }

    /// Release everything that must not outlive this session of the window.
    ///
    /// Called on BOTH close paths (the viewport's own close request and the toolbar's Close
    /// button), because the parent keeps this state struct alive between openings: without
    /// this, an in-flight dialog's worker would still hold a live sender, its `send` would
    /// succeed into a receiver nobody reads, and the next opening would apply that answer —
    /// silently replacing a freshly built graph with a `GraphLoaded`, or writing a
    /// `NodePathPicked` into a node id that now belongs to a different graph. Dropping the
    /// receiver makes that `send` fail instead, which the worker already handles as a log
    /// line, and it clears the "a dialog is open" state so a reopened window is not stuck
    /// showing a spinner with Save/Load disabled.
    ///
    /// Not an error path from the user's point of view — they closed the window — so it logs
    /// and shows nothing (`AGENTS.md` §7).
    fn on_window_closed(&mut self) {
        if self.file_task_rx.take().is_some() {
            ms_log::runtime_log::log_info(
                "[batch-processing] window closed with a file dialog in flight: its result is abandoned",
            );
        }
    }

    // ── Palette panel ─────────────────────────────────────────────────────────

    fn show_palette_panel(&mut self, ui: &mut Ui) {
        ScrollArea::vertical().id_salt("bp_palette").show(ui, |ui| {
            for (category, keys) in NodeDefs::palette_groups() {
                // `CollapsingHeader` hashes its heading text into its `Id`
                // (`egui-docs/05-ids-and-i18n.md` §2), and `category` is a translated string —
                // without a pinned salt every group would collapse itself on a language switch.
                egui::CollapsingHeader::new(category)
                    .id_salt(("bp_palette_group", palette_group_id_salt(&keys)))
                    .show(ui, |ui| {
                        for key in keys {
                            let title = self.defs.get(key).map(|d| d.title).unwrap_or(key);
                            let description = self.defs.get(key).map(|d| d.description).unwrap_or("");

                            let resp = ui
                                .add(egui::Button::new(title).wrap_mode(egui::TextWrapMode::Extend))
                                .on_hover_text(description);

                            if resp.double_clicked() || resp.clicked() {
                                self.add_node_from_key(key);
                            }
                        }
                    });
            }
        });
        ui.separator();
        ui.label(RichText::new(t!("launcher.batch.add_node_hint")).small().weak());
    }

    fn add_node_from_key(&mut self, key: &str) {
        if let Some(params) = NodeParams::default_for_key(key) {
            let pos = self.next_spawn_pos();
            self.graph.add_node(params, pos);
        }
    }

    /// World position for a node created right now, staggered so repeated adds do not stack.
    ///
    /// Anchored on `CanvasState::visible_spawn_origin`, i.e. on what the user is currently
    /// looking at, rather than on a fixed world coordinate: a node added after panning away
    /// used to land off-screen. The stagger is accumulated in POINTS and converted through the
    /// canvas transform owner, so the visible offset between two consecutive nodes is the same
    /// at every zoom; in world units it grew to 750 world points at `ZOOM_MAX` and marched the
    /// later nodes of a run straight back off the screen `visible_spawn_origin` just put them
    /// on.
    fn next_spawn_pos(&mut self) -> egui::Pos2 {
        self.spawn_offset += SPAWN_STAGGER_PX;
        if self.spawn_offset > SPAWN_STAGGER_WRAP_PX {
            self.spawn_offset = 0.0;
        }
        let stagger_world = self
            .canvas
            .screen_to_world_size(egui::vec2(self.spawn_offset, self.spawn_offset));
        self.canvas.visible_spawn_origin() + stagger_world
    }

    // ── Variables panel ───────────────────────────────────────────────────────

    fn show_variables_panel(&mut self, ui: &mut Ui) {
        // Add form.
        ui.group(|ui| {
            ui.label(t!("launcher.batch.new_variable_title"));
            ui.text_edit_singleline(&mut self.var_form_name);

            // The id salt is the catalog KEY, not the translated label, so the popup's
            // open/closed state survives a language switch (`egui-docs/05-ids-and-i18n.md` §2).
            WheelComboBox::new("launcher.batch.variable_type_label", t!("launcher.batch.variable_type_label"))
                .selected_text(match self.var_form_type {
                    DataType::Int => "int",
                    DataType::Str => "str",
                    DataType::ImageList => t!("launcher.batch.image_list_type"),
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.var_form_type, DataType::Int, "int");
                    ui.selectable_value(&mut self.var_form_type, DataType::Str, "str");
                    ui.selectable_value(
                        &mut self.var_form_type,
                        DataType::ImageList,
                        t!("launcher.batch.image_list_type"),
                    );
                });

            ui.checkbox(&mut self.var_form_persist, t!("launcher.batch.keep_between_cycles"));

            if ui.button(t!("launcher.batch.add_button")).clicked() {
                let name = self.var_form_name.trim().to_owned();
                if !name.is_empty() && self.graph.variables.iter().all(|v| v.name != name) {
                    self.graph.add_variable(GraphVariable {
                        name: name.clone(),
                        data_type: self.var_form_type,
                        persist_between_cycles: self.var_form_persist,
                    });
                    self.var_form_name.clear();
                }
            }
        });

        ui.separator();

        // List of existing variables.
        let var_names: Vec<String> = self
            .graph
            .variables
            .iter()
            .map(|v| v.name.clone())
            .collect();

        ScrollArea::vertical().id_salt("bp_vars").show(ui, |ui| {
            let mut to_delete: Option<String> = None;
            for name in &var_names {
                ui.horizontal(|ui| {
                    let var = self.graph.variables.iter().find(|v| &v.name == name);
                    let type_label = var.map(|v| v.data_type.label()).unwrap_or("?");
                    let persist_label = var
                        .map(|v| {
                            if v.persist_between_cycles {
                                "∞"
                            } else {
                                "○"
                            }
                        })
                        .unwrap_or("");
                    ui.label(format!("{persist_label} {name}: {type_label}"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("✕").clicked() {
                            to_delete = Some(name.clone());
                        }
                        if ui
                            .small_button("W")
                            .on_hover_text(t!("launcher.batch.add_write_node_button"))
                            .clicked()
                            && NodeParams::default_for_key("variable_write").is_some()
                        {
                            let p = NodeParams::VariableWrite {
                                variable_name: name.clone(),
                            };
                            let pos = self.next_spawn_pos();
                            self.graph.add_node(p, pos);
                        }
                        if ui
                            .small_button("R")
                            .on_hover_text(t!("launcher.batch.add_read_node_button"))
                            .clicked()
                            && NodeParams::default_for_key("variable_read").is_some()
                        {
                            let p = NodeParams::VariableRead {
                                variable_name: name.clone(),
                            };
                            let pos = self.next_spawn_pos();
                            self.graph.add_node(p, pos);
                        }
                    });
                });
            }
            if let Some(name) = to_delete {
                self.graph.remove_variable(&name);
            }
        });
    }

    // ── Canvas action handler ─────────────────────────────────────────────────

    fn handle_canvas_actions(&mut self, actions: Vec<CanvasAction>) {
        for action in actions {
            match action {
                CanvasAction::ConnectSockets { src, dst } => {
                    match self.graph.add_edge(
                        &self.defs,
                        src.node_id,
                        &src.socket_name,
                        dst.node_id,
                        &dst.socket_name,
                    ) {
                        Ok(_) => {}
                        Err(err) => {
                            self.set_status(tf!("launcher.batch.connect_error", err = err), true);
                        }
                    }
                }
                CanvasAction::DeleteSelected { node_ids } => {
                    // The action carries the selection it was raised with; do not read it back
                    // out of the canvas, which owns it only as UI state.
                    for id in node_ids {
                        self.graph.remove_node(id);
                    }
                    self.canvas.clear_selection();
                }
                CanvasAction::PickNodePath { node_id, purpose } => {
                    self.begin_pick_node_path(node_id, purpose);
                }
            }
        }
    }

    /// Open the file dialog for one node's path parameter, off the GUI thread.
    ///
    /// Silently ignored while another file task is in flight: the canvas has no way to disable
    /// its "..." button, so this is where a double request is dropped. The reason is logged
    /// rather than shown, because the dialog the user already opened is on screen and is the
    /// answer to "why did nothing happen?".
    fn begin_pick_node_path(&mut self, node_id: u32, purpose: NodePathPurpose) {
        if self.file_task_rx.is_some() {
            ms_log::runtime_log::log_warn(format!(
                "[batch-processing] path dialog for node {node_id} ignored: another file dialog is already open"
            ));
            return;
        }
        self.file_task_rx = Some(spawn_node_path_pick(node_id, purpose));
    }

    // ── Executor ─────────────────────────────────────────────────────────────

    fn begin_run(&mut self) {
        self.stop_flag.store(false, Ordering::Relaxed);
        let snapshot = GraphSnapshot::from_model(&self.graph);
        let stop_flag = Arc::clone(&self.stop_flag);
        self.executor_rx = Some(spawn_executor(snapshot, stop_flag));
        self.is_running = true;
        self.active_node_id = None;
        self.set_status(t!("launcher.batch.run_starting_status"), false);
    }

    fn poll_executor(&mut self, ctx: &egui::Context) {
        let rx = match self.executor_rx.take() {
            Some(rx) => rx,
            None => return,
        };

        match rx.try_recv() {
            Ok(ExecutorEvent::Progress { message, node_id }) => {
                ctx.request_repaint();
                self.active_node_id = node_id;
                self.set_status(message, false);
                self.executor_rx = Some(rx);
            }
            Ok(ExecutorEvent::Cancelled) => {
                ctx.request_repaint();
                self.is_running = false;
                self.active_node_id = None;
                self.set_status(t!("launcher.batch.run_stopped_status"), false);
            }
            Ok(ExecutorEvent::Completed {
                cycles,
                nodes_executed,
                end_hits,
                downloaded_images,
                saved_images,
            }) => {
                ctx.request_repaint();
                self.is_running = false;
                self.active_node_id = None;
                self.set_status(
                    tf!("launcher.batch.run_done_summary", cycles = cycles, nodes_executed = nodes_executed, end_hits = end_hits, downloaded_images = downloaded_images, saved_images = saved_images),
                    false,
                );
            }
            Ok(ExecutorEvent::Failed {
                user_message,
                log_message,
            }) => {
                ctx.request_repaint();
                self.is_running = false;
                self.active_node_id = None;
                ms_log::runtime_log::log_error(format!(
                    "[batch-processing] execution failed: {log_message}"
                ));
                self.set_status(user_message, true);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.executor_rx = Some(rx);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.is_running = false;
                self.active_node_id = None;
            }
        }
    }

    // ── File save / load ──────────────────────────────────────────────────────

    /// Drain the in-flight file task, if it has answered, and apply its result.
    ///
    /// Keeps the task alive and schedules a wake-up while it is still running: egui repaints on
    /// demand, and nothing else in this window ticks while the user stares at a native dialog.
    fn poll_file_task(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.file_task_rx.take() else {
            return;
        };

        let event = match rx.try_recv() {
            Ok(event) => event,
            Err(mpsc::TryRecvError::Empty) => {
                self.file_task_rx = Some(rx);
                ctx.request_repaint_after(FILE_TASK_POLL_INTERVAL);
                return;
            }
            // The worker died without answering. Nothing was applied, so the only correct
            // outcome is to stop waiting and say so.
            Err(mpsc::TryRecvError::Disconnected) => FileTaskEvent::Failed {
                user_message: t!("launcher.batch.file_task_failed_error").to_string(),
                log_message: "file task worker disconnected without sending a result".to_string(),
            },
        };
        ctx.request_repaint();

        match event {
            FileTaskEvent::Cancelled => {}
            FileTaskEvent::GraphSaved { path } => {
                self.save_path = Some(path);
                self.set_status(t!("launcher.batch.graph_saved_status"), false);
            }
            FileTaskEvent::GraphLoaded { path, json } => match GraphModel::from_json(&json) {
                Ok(model) => {
                    self.graph = model;
                    // A different graph invalidates every piece of canvas UI state keyed by
                    // node id (selection, z-order, measured sizes), so the canvas starts over.
                    self.canvas = CanvasState::new();
                    self.save_path = Some(path);
                    self.set_status(t!("launcher.batch.graph_loaded_status"), false);
                }
                Err(err) => {
                    ms_log::runtime_log::log_error(format!(
                        "[batch-processing] parse graph from '{}': {err}",
                        path.display()
                    ));
                    self.set_status(tf!("launcher.batch.load_graph_error", err = err), true);
                }
            },
            FileTaskEvent::NodePathPicked {
                node_id,
                purpose,
                path,
            } => {
                let applied = self
                    .graph
                    .node_by_id_mut(node_id)
                    .is_some_and(|node| apply_picked_node_path(&mut node.params, purpose, &path));
                if !applied {
                    // The node was deleted, or its kind changed, while the dialog was open.
                    // Dropping the path is the only safe answer — there is no parameter left
                    // that the user asked to fill.
                    ms_log::runtime_log::log_warn(format!(
                        "[batch-processing] picked path '{}' dropped: node {node_id} no longer holds a {purpose:?} parameter",
                        path.display()
                    ));
                }
            }
            FileTaskEvent::Failed {
                user_message,
                log_message,
            } => {
                ms_log::runtime_log::log_error(format!("[batch-processing] {log_message}"));
                self.set_status(user_message, true);
            }
        }
    }

    /// Start saving the graph: serialize on the GUI thread, dialog and write on a worker.
    ///
    /// The snapshot (`GraphModel::to_json`) is taken here because it borrows the live model;
    /// everything that blocks — the dialog when there is no remembered path, the
    /// pretty-printing and the write — happens on the worker.
    fn save_graph(&mut self) {
        if self.file_task_rx.is_some() {
            return;
        }
        let json = self.graph.to_json();
        self.file_task_rx = Some(spawn_graph_save(json, self.save_path.clone()));
    }

    /// Start loading a graph: dialog, read and JSON parse all happen on a worker.
    fn load_graph(&mut self) {
        if self.file_task_rx.is_some() {
            return;
        }
        self.file_task_rx = Some(spawn_graph_load());
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn set_status(&mut self, msg: impl Into<String>, is_error: bool) {
        self.status_message = msg.into();
        self.status_is_error = is_error;
    }
}

// ─── File task workers ────────────────────────────────────────────────────────

/// Which native dialog a file task needs.
///
/// Only [`pick_path`] interprets it; every other part of a task body is target-independent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickKind {
    /// Where the graph JSON should be written.
    GraphSaveTarget,
    /// An existing graph JSON to read.
    GraphSource,
    /// A `.txt` file for `NodeParams::StartString`.
    TextFile,
    /// An output directory for `NodeParams::SaveFolder`.
    Directory,
}

/// Start a worker that runs `task` and delivers its single `FileTaskEvent`.
///
/// Every file task of this window goes through here so that the "one thread, one channel, one
/// answer" shape — and the handling of a spawn failure — exists in exactly one place. A spawn
/// failure is answered on the channel itself rather than returned, so the caller never has to
/// distinguish "could not start" from "failed while running".
fn spawn_file_task<F>(what: &'static str, task: F) -> Receiver<FileTaskEvent>
where
    F: FnOnce() -> FileTaskEvent + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    let worker_tx = tx.clone();
    match thread::Builder::new()
        .name("batch-file-dialog".to_string())
        .spawn(move || {
            if worker_tx.send(task()).is_err() {
                // The window was closed while the dialog was open. Nothing was applied and
                // nothing can be reported, so this is a log line and not an error path.
                ms_log::runtime_log::log_warn(format!(
                    "[batch-processing] {what} result dropped: the window is gone"
                ));
            }
        }) {
        // The handle is dropped on purpose: the worker is never joined, it reports through the
        // channel and ends on its own. Joining it anywhere would block the GUI thread.
        Ok(_) => {}
        Err(err) => {
            let event = FileTaskEvent::Failed {
                user_message: t!("launcher.batch.file_task_failed_error").to_string(),
                log_message: format!("failed to spawn the {what} worker: {err}"),
            };
            if tx.send(event).is_err() {
                ms_log::runtime_log::log_error(format!(
                    "[batch-processing] could not report the {what} spawn failure: {err}"
                ));
            }
        }
    }
    rx
}

/// Ask the user for a path with the native dialog for `kind`, blocking until they answer.
///
/// Called only from inside a file-task worker — it blocks, so it must never reach the GUI
/// thread (`AGENTS.md` §5). Returns `Ok(Some(path))` for a choice and `Ok(None)` for a
/// dismissed dialog; `Err` carries a ready-made failure event for a build that has no dialog at
/// all. Those two are deliberately distinct outcomes: reporting "no dialog exists here" as a
/// cancellation would be the silent fallback `AGENTS.md` §6 forbids. `what` and
/// `unsupported_message` describe the capability for that case and are unused on native.
#[cfg(not(target_arch = "wasm32"))]
fn pick_path(
    kind: PickKind,
    _what: &str,
    _unsupported_message: &str,
) -> Result<Option<PathBuf>, FileTaskEvent> {
    Ok(match kind {
        PickKind::GraphSaveTarget => rfd::FileDialog::new()
            .add_filter(t!("launcher.batch.graph_file_filter"), &["json"])
            .save_file(),
        PickKind::GraphSource => rfd::FileDialog::new()
            .add_filter(t!("launcher.batch.graph_file_filter"), &["json"])
            .pick_file(),
        PickKind::TextFile => rfd::FileDialog::new()
            .add_filter(t!("launcher.batch.text_file_label"), &["txt"])
            .pick_file(),
        PickKind::Directory => rfd::FileDialog::new().pick_folder(),
    })
}

/// Web build: `rfd` is native-only, so there is no dialog to open and the missing capability is
/// reported instead of being passed off as a cancellation the user never made.
#[cfg(target_arch = "wasm32")]
fn pick_path(
    _kind: PickKind,
    what: &str,
    unsupported_message: &str,
) -> Result<Option<PathBuf>, FileTaskEvent> {
    Err(FileTaskEvent::Failed {
        user_message: unsupported_message.to_string(),
        log_message: format!("{what} needs a native file dialog, which this build does not have"),
    })
}

/// Worker body for "save graph": dialog (only when no path is remembered), serialize, write.
///
/// `json` is the already-taken snapshot of the model, so the worker never touches live UI
/// state. `known_path` is the path a previous save settled on; when it is `Some` the save is
/// silent and no dialog is shown, which is the behaviour the toolbar has always had.
fn spawn_graph_save(json: serde_json::Value, known_path: Option<PathBuf>) -> Receiver<FileTaskEvent> {
    spawn_file_task("graph save", move || {
        let path = match known_path {
            Some(path) => path,
            None => match pick_path(
                PickKind::GraphSaveTarget,
                "graph save",
                t!("launcher.batch.save_web_unsupported"),
            ) {
                Ok(Some(path)) => path,
                Ok(None) => return FileTaskEvent::Cancelled,
                Err(event) => return event,
            },
        };
        let text = match serde_json::to_string_pretty(&json) {
            Ok(text) => text,
            Err(err) => {
                return FileTaskEvent::Failed {
                    user_message: tf!("launcher.batch.serialize_error", err = err),
                    log_message: format!("serialize graph for '{}': {err}", path.display()),
                };
            }
        };
        match std::fs::write(&path, text) {
            Ok(()) => FileTaskEvent::GraphSaved { path },
            Err(err) => FileTaskEvent::Failed {
                user_message: tf!("launcher.batch.save_file_error", err = err),
                log_message: format!("save graph to '{}': {err}", path.display()),
            },
        }
    })
}

/// Worker body for "load graph": dialog, read, JSON parse.
fn spawn_graph_load() -> Receiver<FileTaskEvent> {
    spawn_file_task("graph load", || {
        let path = match pick_path(
            PickKind::GraphSource,
            "graph load",
            t!("launcher.batch.load_web_unsupported"),
        ) {
            Ok(Some(path)) => path,
            Ok(None) => return FileTaskEvent::Cancelled,
            Err(event) => return event,
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                return FileTaskEvent::Failed {
                    user_message: tf!("launcher.batch.read_file_error", err = err),
                    log_message: format!("read graph from '{}': {err}", path.display()),
                };
            }
        };
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(json) => FileTaskEvent::GraphLoaded { path, json },
            Err(err) => FileTaskEvent::Failed {
                user_message: tf!("launcher.batch.invalid_json_error", err = err),
                log_message: format!("parse graph JSON from '{}': {err}", path.display()),
            },
        }
    })
}

/// Worker body for a node's path parameter: the file or folder dialog that fits `purpose`.
fn spawn_node_path_pick(node_id: u32, purpose: NodePathPurpose) -> Receiver<FileTaskEvent> {
    spawn_file_task("node path pick", move || {
        let kind = match purpose {
            NodePathPurpose::StartStringFile => PickKind::TextFile,
            NodePathPurpose::SaveFolderDirectory => PickKind::Directory,
        };
        match pick_path(
            kind,
            "node path pick",
            t!("launcher.batch.pick_path_web_unsupported"),
        ) {
            Ok(Some(path)) => FileTaskEvent::NodePathPicked {
                node_id,
                purpose,
                path,
            },
            Ok(None) => FileTaskEvent::Cancelled,
            Err(event) => event,
        }
    })
}

// ─── Pure helpers ─────────────────────────────────────────────────────────────

/// Write a picked path into the parameter the dialog was opened for.
///
/// Returns `false` when `params` no longer holds that parameter — the node was retyped while
/// the dialog was open — in which case nothing is written. Matching on `purpose` rather than on
/// `params` is deliberate: `purpose` is the thing the USER clicked, and the node's current kind
/// only has to confirm it.
fn apply_picked_node_path(
    params: &mut NodeParams,
    purpose: NodePathPurpose,
    picked: &std::path::Path,
) -> bool {
    match purpose {
        NodePathPurpose::StartStringFile => {
            if let NodeParams::StartString { path } = params {
                *path = picked.to_path_buf();
                return true;
            }
            false
        }
        NodePathPurpose::SaveFolderDirectory => {
            if let NodeParams::SaveFolder { path, .. } = params {
                *path = picked.to_path_buf();
                return true;
            }
            false
        }
    }
}

/// Language-independent identity of one palette group, for use as a widget `id_salt`.
///
/// `NodeDefs::palette_groups` names its groups with translated labels and carries no stable
/// group key, so the group's FIRST node template key stands in for one: template keys are the
/// English node identity (see this module's `MODULE_README.md`) and are unique per group. The
/// fallback only guards against an empty group, which the registry does not currently produce.
fn palette_group_id_salt(keys: &[&'static str]) -> &'static str {
    keys.first().copied().unwrap_or("empty")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A picked file lands in the parameter the dialog was opened for.
    #[test]
    fn picked_file_is_written_into_the_start_string_node() {
        let mut params = NodeParams::StartString {
            path: PathBuf::new(),
        };
        assert!(apply_picked_node_path(
            &mut params,
            NodePathPurpose::StartStringFile,
            Path::new("/tmp/lines.txt"),
        ));
        match params {
            NodeParams::StartString { path } => assert_eq!(path, PathBuf::from("/tmp/lines.txt")),
            other => panic!("parameter kind changed: {other:?}"),
        }
    }

    /// A picked folder lands in the save node without disturbing its other parameters.
    #[test]
    fn picked_folder_is_written_into_the_save_folder_node() {
        let mut params = NodeParams::SaveFolder {
            path: PathBuf::from("/old"),
            name_prefix: "page".to_string(),
        };
        assert!(apply_picked_node_path(
            &mut params,
            NodePathPurpose::SaveFolderDirectory,
            Path::new("/new/out"),
        ));
        match params {
            NodeParams::SaveFolder { path, name_prefix } => {
                assert_eq!(path, PathBuf::from("/new/out"));
                assert_eq!(name_prefix, "page");
            }
            other => panic!("parameter kind changed: {other:?}"),
        }
    }

    /// A node retyped while the dialog was open must not be overwritten by the stale result.
    #[test]
    fn a_result_for_a_retyped_node_is_refused() {
        let mut params = NodeParams::SaveFolder {
            path: PathBuf::from("/keep"),
            name_prefix: String::new(),
        };
        assert!(!apply_picked_node_path(
            &mut params,
            NodePathPurpose::StartStringFile,
            Path::new("/tmp/lines.txt"),
        ));
        match params {
            NodeParams::SaveFolder { path, .. } => assert_eq!(path, PathBuf::from("/keep")),
            other => panic!("parameter kind changed: {other:?}"),
        }
    }

    /// The two path purposes must not be interchangeable: each one only fills its own node.
    #[test]
    fn a_folder_result_does_not_fill_a_file_parameter() {
        let mut params = NodeParams::StartString {
            path: PathBuf::from("/keep.txt"),
        };
        assert!(!apply_picked_node_path(
            &mut params,
            NodePathPurpose::SaveFolderDirectory,
            Path::new("/new/out"),
        ));
        match params {
            NodeParams::StartString { path } => assert_eq!(path, PathBuf::from("/keep.txt")),
            other => panic!("parameter kind changed: {other:?}"),
        }
    }

    /// Every palette group must get a salt that is unique and free of translated text.
    #[test]
    fn palette_group_salts_are_unique_and_language_independent() {
        let groups = NodeDefs::palette_groups();
        assert!(!groups.is_empty(), "the palette registry is empty");

        let mut seen = std::collections::HashSet::new();
        for (_label, keys) in &groups {
            let salt = palette_group_id_salt(keys);
            assert_eq!(
                Some(salt),
                keys.first().copied(),
                "the salt must be the group's first template key"
            );
            assert!(
                salt.is_ascii(),
                "a template key is an English identifier, got {salt:?}"
            );
            assert!(seen.insert(salt), "duplicate palette group salt: {salt}");
        }
    }

    /// An empty group still gets a defined salt instead of panicking on `first()`.
    #[test]
    fn an_empty_palette_group_has_a_defined_salt() {
        assert_eq!(palette_group_id_salt(&[]), "empty");
    }

    /// Closing the window must make an in-flight file task unable to deliver: the state
    /// struct survives the close, so a parked result would otherwise be applied to whatever
    /// graph is on screen the next time the window is opened.
    #[test]
    fn closing_the_window_abandons_an_in_flight_file_task() {
        let mut state = BatchProcessingWindowState::new();
        let (tx, rx) = mpsc::channel::<FileTaskEvent>();
        state.file_task_rx = Some(rx);

        state.on_window_closed();

        assert!(
            state.file_task_rx.is_none(),
            "a reopened window must not still be waiting on the closed session's dialog"
        );
        assert!(
            tx.send(FileTaskEvent::Cancelled).is_err(),
            "the worker's send must fail, which is what makes a stale result unappliable"
        );
    }

    /// The close path must be safe when nothing is in flight, and must leave the window ready
    /// to start a new task on the next opening.
    #[test]
    fn closing_the_window_without_a_file_task_leaves_it_idle() {
        let mut state = BatchProcessingWindowState::new();
        state.on_window_closed();
        assert!(state.file_task_rx.is_none());
        state.on_window_closed();
        assert!(state.file_task_rx.is_none(), "closing twice must be a no-op");
    }
}
