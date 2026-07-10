/*
File: src/launcher/new_project/batch_processing/window.rs

Purpose:
Main UI orchestrator for the batch node-based processing window.

Main responsibilities:
- Render the toolbar (Save / Load / Run / Stop), left palette panel, variables panel,
  and the central node canvas
- Poll the executor channel and display progress / errors in the status bar
- Handle canvas actions (connect sockets, delete nodes)
- Keep all node parameters embedded directly into the node body on the canvas
- Manage file save/load of the graph (JSON)

Key structures:
- BatchProcessingWindowState — root state for the entire window

Notes:
The window is opened as a native `ctx.show_viewport_immediate()` from
`new_project/window.rs`.  It does not own a project; it operates on standalone
image pipelines and saves results to user-specified folders.
*/

use super::canvas::{CanvasAction, CanvasState};
use super::executor::{ExecutorEvent, GraphSnapshot, spawn_executor};
use super::graph::{GraphModel, GraphVariable};
use super::node_defs::NodeDefs;
use super::types::{DataType, NodeParams};
use egui::{Color32, RichText, ScrollArea, Ui, ViewportClass};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;

const LEFT_PANEL_WIDTH: f32 = 220.0;
const STATUS_BAR_HEIGHT: f32 = 28.0;

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

    // Variable editor: add variable form
    var_form_name: String,
    var_form_type: DataType,
    var_form_persist: bool,

    // New node spawn position offset (incremented to avoid stacking).
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
            return false;
        }

        self.poll_executor(ctx);

        let mut keep_open = true;

        // ── Top toolbar ────────────────────────────────────────────────────
        egui::Panel::top("bp_toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button(t!("launcher.batch.save_graph_button")).clicked() {
                    self.save_graph(ctx);
                }
                if ui.button(t!("launcher.batch.load_graph_button")).clicked() {
                    self.load_graph(ctx);
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

        keep_open
    }

    // ── Palette panel ─────────────────────────────────────────────────────────

    fn show_palette_panel(&mut self, ui: &mut Ui) {
        ScrollArea::vertical().id_salt("bp_palette").show(ui, |ui| {
            for (category, keys) in NodeDefs::palette_groups() {
                ui.collapsing(category, |ui| {
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
            // Stagger spawn positions.
            self.spawn_offset += 30.0;
            if self.spawn_offset > 300.0 {
                self.spawn_offset = 0.0;
            }
            let pos = egui::pos2(100.0 + self.spawn_offset, 100.0 + self.spawn_offset);
            self.graph.add_node(params, pos);
        }
    }

    // ── Variables panel ───────────────────────────────────────────────────────

    fn show_variables_panel(&mut self, ui: &mut Ui) {
        // Add form.
        ui.group(|ui| {
            ui.label(t!("launcher.batch.new_variable_title"));
            ui.text_edit_singleline(&mut self.var_form_name);

            egui::ComboBox::new("launcher.batch.variable_type_label", t!("launcher.batch.variable_type_label"))
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
                            self.graph.add_node(p, egui::pos2(200.0, 200.0));
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
                            self.graph.add_node(p, egui::pos2(200.0, 200.0));
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
                CanvasAction::DeleteSelected => {
                    let selected: Vec<u32> = self.canvas.selected_nodes().iter().copied().collect();
                    for id in selected {
                        self.graph.remove_node(id);
                    }
                    self.canvas.clear_selection();
                }
            }
        }
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
                crate::runtime_log::log_error(format!(
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

    /// Web stub: saving a graph opens a native save dialog (`rfd`) and writes with
    /// `std::fs`; neither is available in the browser. Reports the missing capability.
    #[cfg(target_arch = "wasm32")]
    fn save_graph(&mut self, _ctx: &egui::Context) {
        self.set_status(t!("launcher.batch.save_web_unsupported"), true);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn save_graph(&mut self, _ctx: &egui::Context) {
        let path = if let Some(p) = &self.save_path {
            Some(p.clone())
        } else {
            rfd::FileDialog::new()
                .add_filter(t!("launcher.batch.graph_file_filter"), &["json"])
                .save_file()
        };
        if let Some(path) = path {
            let json = self.graph.to_json();
            match serde_json::to_string_pretty(&json) {
                Ok(text) => match std::fs::write(&path, text) {
                    Ok(()) => {
                        self.save_path = Some(path);
                        self.set_status(t!("launcher.batch.graph_saved_status"), false);
                    }
                    Err(err) => {
                        crate::runtime_log::log_error(format!(
                            "[batch-processing] save graph to '{}': {err}",
                            path.display()
                        ));
                        self.set_status(tf!("launcher.batch.save_file_error", err = err), true);
                    }
                },
                Err(err) => {
                    self.set_status(tf!("launcher.batch.serialize_error", err = err), true);
                }
            }
        }
    }

    /// Web stub: loading a graph opens a native open dialog (`rfd`) and reads with
    /// `std::fs`; neither is available in the browser. Reports the missing capability.
    #[cfg(target_arch = "wasm32")]
    fn load_graph(&mut self, _ctx: &egui::Context) {
        self.set_status(t!("launcher.batch.load_web_unsupported"), true);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn load_graph(&mut self, _ctx: &egui::Context) {
        let path = rfd::FileDialog::new()
            .add_filter(t!("launcher.batch.graph_file_filter"), &["json"])
            .pick_file();
        if let Some(path) = path {
            match std::fs::read_to_string(&path) {
                Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(json) => match GraphModel::from_json(&json) {
                        Ok(model) => {
                            self.graph = model;
                            self.canvas = CanvasState::new();
                            self.save_path = Some(path);
                            self.set_status(t!("launcher.batch.graph_loaded_status"), false);
                        }
                        Err(err) => {
                            crate::runtime_log::log_error(format!(
                                "[batch-processing] parse graph from '{}': {err}",
                                path.display()
                            ));
                            self.set_status(tf!("launcher.batch.load_graph_error", err = err), true);
                        }
                    },
                    Err(err) => {
                        self.set_status(tf!("launcher.batch.invalid_json_error", err = err), true);
                    }
                },
                Err(err) => {
                    self.set_status(tf!("launcher.batch.read_file_error", err = err), true);
                }
            }
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn set_status(&mut self, msg: impl Into<String>, is_error: bool) {
        self.status_message = msg.into();
        self.status_is_error = is_error;
    }
}
