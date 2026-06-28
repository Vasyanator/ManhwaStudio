/*
File: src/launcher/new_project/batch_processing/executor.rs

Purpose:
Background batch pipeline executor for the node-based processing graph.

Main responsibilities:
- Run the node graph in a worker thread without blocking the GUI
- Implement exec/data routing: cycle loop, join-node wait, data propagation
- Execute all 13 node handlers (start_number, start_string, string_template,
  variable_read/write, quick_downloader, save_folder, stitch_split, waifu2x,
  end; browser nodes via Selenium JSON-RPC to the Python daemon)
- Stream progress events back via an mpsc channel

Key structures:
- ExecutorEvent   — progress/completed/failed variants sent to the window
- GraphSnapshot   — serialisable view of the graph passed to the worker thread
- spawn_executor  — spawns the background thread, returns Receiver<ExecutorEvent>

Notes:
The algorithm is a direct port of Python BatchPipelineExecutor:
  1. find start nodes
  2. for each iteration of each start node (cycle):
     a. reset non-persistent variables
     b. propagate data outputs from start node
     c. enqueue exec outputs from start node
     d. process exec queue: wait for required_exec_inputs, then execute node
  3. return stats summary string

Browser nodes (open_url, scroll_page, fetch_from_browser) communicate with the
adv_fetch_cli.py Python daemon via JSON-RPC over stdio, reusing the pattern from
advanced_download.rs.  The daemon is started lazily on first browser node use and
kept alive for the duration of the pipeline run.  The startup `ready` event is
consumed before browser node commands are sent.

Image downloads (quick_downloader) use ureq with browser-like headers.
Image saves (save_folder) write PNG files with sequential numbering.
Stitch/split delegates to the stitching module's synchronous functions via rayon.
Waifu2x calls the bundled waifu2x shared library via runtime FFI.
*/

use super::graph::{EdgeKind, GraphModel};
use super::types::{DataValue, NodeParams};
use crate::backend_ipc;
use crate::launcher::new_project::stitching::{StitchInputImage, StitchOptions, StitchSplitMode};
use crate::launcher::new_project::waifu2x::{Waifu2xInputImage, Waifu2xOptions};
use image::{ImageFormat, RgbaImage};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

// ─── Public event type ────────────────────────────────────────────────────────

pub enum ExecutorEvent {
    Progress {
        message: String,
        node_id: Option<u32>,
    },
    Cancelled,
    Completed {
        cycles: u32,
        nodes_executed: u32,
        end_hits: u32,
        downloaded_images: u32,
        saved_images: u32,
    },
    Failed {
        user_message: String,
        log_message: String,
    },
}

// ─── Snapshot passed to the worker ───────────────────────────────────────────

/// A fully-owned, cloneable snapshot of the graph for use in the worker thread.
#[derive(Clone)]
pub struct GraphSnapshot {
    pub nodes: Vec<SnapshotNode>,
    pub exec_edges: Vec<SnapshotExecEdge>,
    pub data_edges: Vec<SnapshotDataEdge>,
    pub variables: Vec<SnapshotVariable>,
}

#[derive(Clone)]
pub struct SnapshotNode {
    pub id: u32,
    pub params: NodeParams,
}

#[derive(Clone)]
pub struct SnapshotExecEdge {
    pub id: u32,
    pub src_node: u32,
    pub dst_node: u32,
}

#[derive(Clone)]
pub struct SnapshotDataEdge {
    pub src_node: u32,
    pub src_socket: String,
    pub dst_node: u32,
    pub dst_socket: String,
}

#[derive(Clone)]
pub struct SnapshotVariable {
    pub name: String,
    pub persist_between_cycles: bool,
}

impl GraphSnapshot {
    /// Build a snapshot from the live graph model.
    pub fn from_model(model: &GraphModel) -> Self {
        let nodes = model
            .nodes
            .iter()
            .map(|n| SnapshotNode {
                id: n.id,
                params: n.params.clone(),
            })
            .collect();

        let exec_edges = model
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Exec)
            .map(|e| SnapshotExecEdge {
                id: e.id,
                src_node: e.src_node,
                dst_node: e.dst_node,
            })
            .collect();

        let data_edges = model
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Data)
            .map(|e| SnapshotDataEdge {
                src_node: e.src_node,
                src_socket: e.src_socket.clone(),
                dst_node: e.dst_node,
                dst_socket: e.dst_socket.clone(),
            })
            .collect();

        let variables = model
            .variables
            .iter()
            .map(|v| SnapshotVariable {
                name: v.name.clone(),
                persist_between_cycles: v.persist_between_cycles,
            })
            .collect();

        Self {
            nodes,
            exec_edges,
            data_edges,
            variables,
        }
    }
}

// ─── Worker spawn ─────────────────────────────────────────────────────────────

/// Spawn the executor in a background thread.
/// Returns a receiver that yields `ExecutorEvent`s.
pub fn spawn_executor(
    snapshot: GraphSnapshot,
    stop_flag: Arc<AtomicBool>,
) -> Receiver<ExecutorEvent> {
    let (tx, rx) = mpsc::channel::<ExecutorEvent>();

    thread::Builder::new()
        .name("batch-executor".to_owned())
        .spawn(move || {
            let result = BatchExecutor::new(snapshot, tx.clone(), stop_flag).run();
            match result {
                Ok(stats) => {
                    let _ = tx.send(ExecutorEvent::Completed {
                        cycles: stats.cycles,
                        nodes_executed: stats.nodes_executed,
                        end_hits: stats.end_hits,
                        downloaded_images: stats.downloaded_images,
                        saved_images: stats.saved_images,
                    });
                }
                Err(err) => {
                    if err.cancelled {
                        let _ = tx.send(ExecutorEvent::Cancelled);
                    } else {
                        let _ = tx.send(ExecutorEvent::Failed {
                            user_message: err.user_message,
                            log_message: err.log_message,
                        });
                    }
                }
            }
        })
        .expect("failed to spawn batch-executor thread");

    rx
}

// ─── Internal error type ──────────────────────────────────────────────────────

struct ExecError {
    user_message: String,
    log_message: String,
    cancelled: bool,
}

impl ExecError {
    fn new(user: impl Into<String>, log: impl Into<String>) -> Self {
        Self {
            user_message: user.into(),
            log_message: log.into(),
            cancelled: false,
        }
    }

    fn cancelled() -> Self {
        Self {
            user_message: String::new(),
            log_message: String::new(),
            cancelled: true,
        }
    }
}

// ─── Execution key types ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ExecTask {
    cycle_id: u32,
    node_id: u32,
    edge_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DataKey {
    cycle_id: u32,
    node_id: u32,
    socket_name: String,
}

// ─── Execution stats ──────────────────────────────────────────────────────────

struct ExecStats {
    cycles: u32,
    nodes_executed: u32,
    end_hits: u32,
    downloaded_images: u32,
    saved_images: u32,
}

// ─── Batch executor ───────────────────────────────────────────────────────────

struct BatchExecutor {
    snapshot: GraphSnapshot,
    tx: Sender<ExecutorEvent>,
    stop_flag: Arc<AtomicBool>,

    // Pre-built routing tables.
    nodes_by_id: HashMap<u32, SnapshotNode>,
    exec_out: HashMap<u32, Vec<SnapshotExecEdge>>,
    data_out: HashMap<u32, Vec<SnapshotDataEdge>>,
    data_in_by_socket: HashMap<(u32, String), Vec<SnapshotDataEdge>>,
    persistent_vars: HashMap<String, bool>,

    // Browser daemon (lazily initialised).
    browser_daemon: Option<BrowserDaemon>,
}

impl BatchExecutor {
    fn new(snapshot: GraphSnapshot, tx: Sender<ExecutorEvent>, stop_flag: Arc<AtomicBool>) -> Self {
        let mut nodes_by_id = HashMap::new();
        let mut exec_out: HashMap<u32, Vec<SnapshotExecEdge>> = HashMap::new();
        let mut data_out: HashMap<u32, Vec<SnapshotDataEdge>> = HashMap::new();
        let mut data_in_by_socket: HashMap<(u32, String), Vec<SnapshotDataEdge>> = HashMap::new();

        for node in &snapshot.nodes {
            nodes_by_id.insert(node.id, node.clone());
        }
        for edge in &snapshot.exec_edges {
            exec_out
                .entry(edge.src_node)
                .or_default()
                .push(edge.clone());
        }
        for edge in &snapshot.data_edges {
            data_out
                .entry(edge.src_node)
                .or_default()
                .push(edge.clone());
            data_in_by_socket
                .entry((edge.dst_node, edge.dst_socket.clone()))
                .or_default()
                .push(edge.clone());
        }

        let persistent_vars: HashMap<String, bool> = snapshot
            .variables
            .iter()
            .map(|v| (v.name.clone(), v.persist_between_cycles))
            .collect();

        Self {
            snapshot,
            tx,
            stop_flag,
            nodes_by_id,
            exec_out,
            data_out,
            data_in_by_socket,
            persistent_vars,
            browser_daemon: None,
        }
    }

    fn run(&mut self) -> Result<ExecStats, ExecError> {
        self.check_cancelled()?;

        if self.nodes_by_id.is_empty() {
            return Err(ExecError::new(
                "Граф пуст. Добавьте узлы перед запуском.",
                "executor: empty graph",
            ));
        }

        let start_nodes: Vec<u32> = self
            .nodes_by_id
            .values()
            .filter(|n| n.params.is_start_node())
            .map(|n| n.id)
            .collect();

        if start_nodes.is_empty() {
            return Err(ExecError::new(
                "В графе нет стартовых узлов (Старт (число) / Старт (строка)).",
                "executor: no start nodes",
            ));
        }

        let mut stats = ExecStats {
            cycles: 0,
            nodes_executed: 0,
            end_hits: 0,
            downloaded_images: 0,
            saved_images: 0,
        };

        let mut variable_values: HashMap<String, DataValue> = self
            .persistent_vars
            .keys()
            .map(|name| (name.clone(), DataValue::Null))
            .collect();

        let mut data_values: HashMap<DataKey, DataValue> = HashMap::new();
        let mut exec_arrivals: HashMap<(u32, u32), HashSet<u32>> = HashMap::new();
        let mut cycle_id: u32 = 0;
        let max_steps: usize = 200_000;
        let mut steps: usize = 0;

        // Sort start nodes by id for deterministic order.
        let mut sorted_starts = start_nodes;
        sorted_starts.sort_unstable();

        for start_id in sorted_starts {
            self.check_cancelled()?;
            let start_node = self.nodes_by_id[&start_id].clone();

            for start_outputs in self.iterate_start_node(&start_node)? {
                self.check_cancelled()?;
                cycle_id += 1;
                stats.cycles += 1;

                self.reset_non_persistent_vars(&mut variable_values);
                self.propagate_data_outputs(start_id, cycle_id, &start_outputs, &mut data_values);

                let mut queue: VecDeque<ExecTask> = VecDeque::new();
                self.enqueue_exec_outputs(start_id, cycle_id, &mut queue);

                while let Some(task) = queue.pop_front() {
                    self.check_cancelled()?;
                    steps += 1;
                    if steps > max_steps {
                        return Err(ExecError::new(
                            "Превышен лимит шагов. Проверьте граф на зацикливание.",
                            format!("executor: step limit {max_steps} reached"),
                        ));
                    }

                    let node = match self.nodes_by_id.get(&task.node_id).cloned() {
                        Some(n) => n,
                        None => continue,
                    };

                    let arrived = exec_arrivals
                        .entry((task.cycle_id, task.node_id))
                        .or_default();
                    arrived.insert(task.edge_id);

                    let required = self.required_exec_inputs(task.node_id).max(1);
                    if arrived.len() < required {
                        continue;
                    }
                    arrived.clear();

                    let inputs = self.collect_data_inputs(
                        task.cycle_id,
                        &node,
                        &data_values,
                        &variable_values,
                    );

                    let outputs = self.execute_node(
                        &node,
                        inputs,
                        &mut variable_values,
                        task.cycle_id,
                        &mut stats,
                    )?;
                    stats.nodes_executed += 1;

                    self.propagate_data_outputs(
                        task.node_id,
                        task.cycle_id,
                        &outputs,
                        &mut data_values,
                    );
                    self.enqueue_exec_outputs(task.node_id, task.cycle_id, &mut queue);
                }
            }
        }

        Ok(stats)
    }

    // ── Iterator over start node cycles ──────────────────────────────────────

    fn iterate_start_node(
        &self,
        node: &SnapshotNode,
    ) -> Result<Vec<HashMap<String, DataValue>>, ExecError> {
        let mut results = Vec::new();
        match &node.params {
            NodeParams::StartNumber { start, step, end } => {
                if *step == 0 {
                    return Err(ExecError::new(
                        format!("Узел '{}': шаг не может быть 0.", node.params.title()),
                        "start_number: step=0",
                    ));
                }
                let mut value = *start;
                loop {
                    self.check_cancelled()?;
                    if *step > 0 && value > *end {
                        break;
                    }
                    if *step < 0 && value < *end {
                        break;
                    }
                    let _ = self.tx.send(ExecutorEvent::Progress {
                        message: format!("{}: индекс {value}", node.params.title()),
                        node_id: Some(node.id),
                    });
                    let mut map = HashMap::new();
                    map.insert("Индекс".to_owned(), DataValue::Int(value));
                    results.push(map);
                    value += step;
                }
            }
            NodeParams::StartString { path } => {
                if path.as_os_str().is_empty() {
                    return Err(ExecError::new(
                        format!(
                            "Узел '{}': не указан путь к txt-файлу.",
                            node.params.title()
                        ),
                        "start_string: empty path",
                    ));
                }
                let content = std::fs::read_to_string(path).map_err(|err| {
                    ExecError::new(
                        format!("Не удалось прочитать файл '{}'.", path.display()),
                        format!("start_string: read '{}': {err}", path.display()),
                    )
                })?;
                for (idx, line) in content.lines().enumerate() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    self.check_cancelled()?;
                    let _ = self.tx.send(ExecutorEvent::Progress {
                        message: format!("{}: строка {}", node.params.title(), idx + 1),
                        node_id: Some(node.id),
                    });
                    let mut map = HashMap::new();
                    map.insert("Строка".to_owned(), DataValue::Str(line.to_owned()));
                    results.push(map);
                }
            }
            _ => {
                return Err(ExecError::new(
                    format!("Узел '{}' не является стартовым.", node.params.title()),
                    "iterate_start_node: not a start node",
                ));
            }
        }
        Ok(results)
    }

    // ── Node execution dispatcher ─────────────────────────────────────────────

    fn execute_node(
        &mut self,
        node: &SnapshotNode,
        inputs: HashMap<String, DataValue>,
        variable_values: &mut HashMap<String, DataValue>,
        cycle_id: u32,
        stats: &mut ExecStats,
    ) -> Result<HashMap<String, DataValue>, ExecError> {
        self.check_cancelled()?;
        let _ = self.tx.send(ExecutorEvent::Progress {
            message: format!("[Цикл {cycle_id}] {}", node.params.title()),
            node_id: Some(node.id),
        });

        match &node.params.clone() {
            NodeParams::StringTemplate {
                template,
                placeholders,
            } => {
                let mut result = template.clone();
                for name in placeholders {
                    let val = inputs
                        .get(name)
                        .map(|v| match v {
                            DataValue::Int(n) => n.to_string(),
                            DataValue::Str(s) => s.clone(),
                            _ => String::new(),
                        })
                        .unwrap_or_default();
                    result = result.replace(&format!("{{{name}}}"), &val);
                }
                let mut out = HashMap::new();
                out.insert("Строка".to_owned(), DataValue::Str(result));
                Ok(out)
            }

            NodeParams::VariableWrite { variable_name } => {
                let value = inputs.get("Значение").cloned().unwrap_or(DataValue::Null);
                variable_values.insert(variable_name.clone(), value);
                Ok(HashMap::new())
            }

            NodeParams::VariableRead { variable_name } => {
                let value = variable_values
                    .get(variable_name)
                    .cloned()
                    .unwrap_or(DataValue::Null);
                let mut out = HashMap::new();
                out.insert("Значение".to_owned(), value);
                Ok(out)
            }

            NodeParams::QuickDownloader => {
                let url = inputs
                    .get("URL")
                    .and_then(DataValue::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                if url.is_empty() {
                    return Err(ExecError::new(
                        format!("Узел '{}': вход 'URL' пустой.", node.params.title()),
                        "quick_downloader: empty URL",
                    ));
                }
                let images =
                    self.run_interruptible(move || Self::download_images_blocking(&url))?;
                stats.downloaded_images += images.len() as u32;
                let list: Vec<Arc<RgbaImage>> = images.into_iter().map(Arc::new).collect();
                let mut out = HashMap::new();
                out.insert("Картинки".to_owned(), DataValue::ImageList(Arc::new(list)));
                Ok(out)
            }

            NodeParams::OpenUrl { browser } => {
                let url = inputs
                    .get("URL")
                    .and_then(DataValue::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                if url.is_empty() {
                    return Err(ExecError::new(
                        format!("Узел '{}': вход 'URL' пустой.", node.params.title()),
                        "open_url: empty URL",
                    ));
                }
                self.browser_open_url(browser.as_daemon_str(), &url)?;
                Ok(HashMap::new())
            }

            NodeParams::ScrollPage => {
                self.browser_scroll_page()?;
                Ok(HashMap::new())
            }

            NodeParams::FetchFromBrowser { pattern } => {
                let images = self.browser_fetch_images(pattern)?;
                stats.downloaded_images += images.len() as u32;
                let list: Vec<Arc<RgbaImage>> = images.into_iter().map(Arc::new).collect();
                let mut out = HashMap::new();
                out.insert("Картинки".to_owned(), DataValue::ImageList(Arc::new(list)));
                Ok(out)
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
                let source_images = self.coerce_image_list(inputs.get("Картинки"))?;
                let options = StitchOptions {
                    parts: *parts,
                    target_height: *target_height,
                    band_rows: *band_rows,
                    tolerance: *tolerance,
                    search_radius: *search_radius,
                    prefer_up_first: *prefer_up_first,
                    mode: if *auto_cut {
                        StitchSplitMode::AutoCut
                    } else {
                        StitchSplitMode::ManualCutPreview
                    },
                };
                let result_images = self.run_interruptible(move || {
                    Self::run_stitch_split_blocking(source_images, options)
                })?;
                let list: Vec<Arc<RgbaImage>> = result_images.into_iter().map(Arc::new).collect();
                let mut out = HashMap::new();
                out.insert("Картинки".to_owned(), DataValue::ImageList(Arc::new(list)));
                Ok(out)
            }

            NodeParams::Waifu2x {
                scale,
                noise,
                tile_size,
            } => {
                let source_images = self.coerce_image_list(inputs.get("Картинки"))?;
                let options = Waifu2xOptions {
                    scale: *scale,
                    noise: *noise,
                    tile_size: *tile_size,
                };
                let result_images = self.run_interruptible(move || {
                    Self::run_waifu2x_blocking(source_images, options)
                })?;
                let list: Vec<Arc<RgbaImage>> = result_images.into_iter().map(Arc::new).collect();
                let mut out = HashMap::new();
                out.insert("Картинки".to_owned(), DataValue::ImageList(Arc::new(list)));
                Ok(out)
            }

            NodeParams::SaveFolder { path, name_prefix } => {
                let source_images = self.coerce_image_list(inputs.get("Картинки"))?;
                // Socket "Путь" overrides the path configured in parameters.
                let effective_path = if let Some(DataValue::Str(s)) = inputs.get("Путь") {
                    PathBuf::from(s.as_str())
                } else {
                    path.clone()
                };
                let prefix = name_prefix.clone();
                let saved = self.run_interruptible(move || {
                    Self::save_images_to_folder_blocking(&source_images, &effective_path, &prefix)
                })?;
                stats.saved_images += saved as u32;
                Ok(HashMap::new())
            }

            NodeParams::End => {
                stats.end_hits += 1;
                Ok(HashMap::new())
            }

            NodeParams::StartNumber { .. } | NodeParams::StartString { .. } => {
                // Start nodes are handled by iterate_start_node; reaching here is a no-op.
                Ok(HashMap::new())
            }
        }
    }

    // ── Routing helpers ───────────────────────────────────────────────────────

    fn required_exec_inputs(&self, node_id: u32) -> usize {
        self.snapshot
            .exec_edges
            .iter()
            .filter(|e| e.dst_node == node_id)
            .count()
    }

    fn collect_data_inputs(
        &self,
        cycle_id: u32,
        node: &SnapshotNode,
        data_values: &HashMap<DataKey, DataValue>,
        variable_values: &HashMap<String, DataValue>,
    ) -> HashMap<String, DataValue> {
        // Determine which data input sockets this node has.
        let socket_names: Vec<String> = self
            .snapshot
            .data_edges
            .iter()
            .filter(|e| e.dst_node == node.id)
            .map(|e| e.dst_socket.clone())
            .collect();

        let mut inputs = HashMap::new();
        for socket_name in socket_names {
            let key = DataKey {
                cycle_id,
                node_id: node.id,
                socket_name: socket_name.clone(),
            };
            let value = if let Some(v) = data_values.get(&key) {
                v.clone()
            } else {
                // Try resolving on-demand from a variable_read node.
                self.resolve_from_variable_read(cycle_id, node.id, &socket_name, variable_values)
                    .unwrap_or(DataValue::Null)
            };
            inputs.insert(socket_name, value);
        }
        inputs
    }

    fn resolve_from_variable_read(
        &self,
        _cycle_id: u32,
        dst_node: u32,
        dst_socket: &str,
        variable_values: &HashMap<String, DataValue>,
    ) -> Option<DataValue> {
        let incoming_data = self
            .data_in_by_socket
            .get(&(dst_node, dst_socket.to_owned()))?;
        for edge in incoming_data {
            let src = self.nodes_by_id.get(&edge.src_node)?;
            if let NodeParams::VariableRead { variable_name } = &src.params {
                return Some(
                    variable_values
                        .get(variable_name)
                        .cloned()
                        .unwrap_or(DataValue::Null),
                );
            }
        }
        None
    }

    fn propagate_data_outputs(
        &self,
        src_node: u32,
        cycle_id: u32,
        outputs: &HashMap<String, DataValue>,
        data_values: &mut HashMap<DataKey, DataValue>,
    ) {
        if outputs.is_empty() {
            return;
        }
        for edge in self.data_out.get(&src_node).into_iter().flatten() {
            if let Some(value) = outputs.get(&edge.src_socket) {
                data_values.insert(
                    DataKey {
                        cycle_id,
                        node_id: edge.dst_node,
                        socket_name: edge.dst_socket.clone(),
                    },
                    value.clone(),
                );
            }
        }
    }

    fn enqueue_exec_outputs(&self, src_node: u32, cycle_id: u32, queue: &mut VecDeque<ExecTask>) {
        for edge in self.exec_out.get(&src_node).into_iter().flatten() {
            queue.push_back(ExecTask {
                cycle_id,
                node_id: edge.dst_node,
                edge_id: edge.id,
            });
        }
    }

    fn reset_non_persistent_vars(&self, variable_values: &mut HashMap<String, DataValue>) {
        for (name, is_persistent) in &self.persistent_vars {
            if !is_persistent {
                variable_values.insert(name.clone(), DataValue::Null);
            }
        }
    }

    fn check_cancelled(&self) -> Result<(), ExecError> {
        if self.stop_flag.load(Ordering::Relaxed) {
            Err(ExecError::cancelled())
        } else {
            Ok(())
        }
    }

    fn run_interruptible<T, F>(&self, operation: F) -> Result<T, ExecError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, ExecError> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("batch-node-worker".to_owned())
            .spawn(move || {
                let _ = tx.send(operation());
            })
            .map_err(|err| {
                ExecError::new(
                    "Не удалось запустить фоновое выполнение узла.",
                    format!("executor: failed to spawn node worker: {err}"),
                )
            })?;

        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => self.check_cancelled()?,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ExecError::new(
                        "Фоновый worker узла завершился неожиданно.",
                        "executor: node worker disconnected unexpectedly",
                    ));
                }
            }
        }
    }

    // ── Image helpers ─────────────────────────────────────────────────────────

    fn coerce_image_list(&self, value: Option<&DataValue>) -> Result<Vec<RgbaImage>, ExecError> {
        match value {
            Some(DataValue::ImageList(list)) => {
                Ok(list.iter().map(|img| (**img).clone()).collect())
            }
            _ => Ok(Vec::new()),
        }
    }

    fn download_images_blocking(url: &str) -> Result<Vec<RgbaImage>, ExecError> {
        // Attempt HTTP download with browser-like headers.
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(30))
            .build();
        let response = agent
            .get(url)
            .set(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
            )
            .set(
                "Accept",
                "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
            )
            .call()
            .map_err(|err| {
                ExecError::new(
                    format!("Не удалось загрузить URL: {url}"),
                    format!("quick_downloader: GET '{url}': {err}"),
                )
            })?;

        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut response.into_reader(), &mut bytes).map_err(|err| {
            ExecError::new(
                "Ошибка чтения загруженных данных.",
                format!("quick_downloader: read body: {err}"),
            )
        })?;

        let img = image::load_from_memory(&bytes).map_err(|err| {
            ExecError::new(
                "Не удалось декодировать скачанное изображение.",
                format!("quick_downloader: decode image: {err}"),
            )
        })?;

        Ok(vec![img.to_rgba8()])
    }

    fn save_images_to_folder_blocking(
        images: &[RgbaImage],
        folder: &Path,
        prefix: &str,
    ) -> Result<usize, ExecError> {
        if images.is_empty() {
            return Ok(0);
        }
        std::fs::create_dir_all(folder).map_err(|err| {
            ExecError::new(
                format!("Не удалось создать папку '{}'.", folder.display()),
                format!("save_folder: create_dir_all '{}': {err}", folder.display()),
            )
        })?;

        let width = images.len().to_string().len().max(4);
        for (idx, img) in images.iter().enumerate() {
            let filename = format!("{prefix}{:0>width$}.png", idx + 1, width = width);
            let out_path = folder.join(&filename);
            img.save_with_format(&out_path, ImageFormat::Png)
                .map_err(|err| {
                    ExecError::new(
                        format!("Не удалось сохранить '{filename}'."),
                        format!("save_folder: save '{}': {err}", out_path.display()),
                    )
                })?;
        }
        Ok(images.len())
    }

    // ── Stitch/split ──────────────────────────────────────────────────────────

    fn run_stitch_split_blocking(
        images: Vec<RgbaImage>,
        options: StitchOptions,
    ) -> Result<Vec<RgbaImage>, ExecError> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        let input: Vec<StitchInputImage> = images
            .into_iter()
            .enumerate()
            .map(|(i, img)| StitchInputImage {
                name: format!("{i:04}.png"),
                image: img,
            })
            .collect();

        // Run synchronously in the executor thread (already off the GUI thread).
        crate::launcher::new_project::stitching::run_stitch_split_sync(input, options).map_err(
            |err| {
                ExecError::new(
                    "Ошибка склейки/нарезки изображений.",
                    format!("stitch_split: {err}"),
                )
            },
        )
    }

    // ── Waifu2x ───────────────────────────────────────────────────────────────

    fn run_waifu2x_blocking(
        images: Vec<RgbaImage>,
        options: Waifu2xOptions,
    ) -> Result<Vec<RgbaImage>, ExecError> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        let input: Vec<Waifu2xInputImage> = images
            .into_iter()
            .enumerate()
            .map(|(i, img)| Waifu2xInputImage {
                name: format!("{i:04}.png"),
                image: img,
            })
            .collect();

        crate::launcher::new_project::waifu2x::run_waifu2x_sync(input, options)
            .map_err(|err| ExecError::new("Ошибка запуска waifu2x.", format!("waifu2x: {err}")))
    }

    // ── Browser automation ────────────────────────────────────────────────────

    fn ensure_browser_daemon(&mut self) -> Result<&mut BrowserDaemon, ExecError> {
        if self.browser_daemon.is_none() {
            self.browser_daemon = Some(BrowserDaemon::spawn().map_err(|err| {
                ExecError::new(
                    "Не удалось запустить Python daemon для браузера.",
                    format!("browser_daemon: spawn: {err}"),
                )
            })?);
        }
        Ok(self.browser_daemon.as_mut().expect("just initialised"))
    }

    fn browser_open_url(&mut self, browser: &str, url: &str) -> Result<(), ExecError> {
        self.check_cancelled()?;
        let stop_flag = Arc::clone(&self.stop_flag);
        let daemon = self.ensure_browser_daemon()?;
        daemon.browser_name = browser.to_owned();
        daemon
            .send_command_interruptible(
                &serde_json::json!({
                    "command": "open_url",
                    "browser": browser,
                    "url": url,
                }),
                ExpectedDaemonEvent::Opened,
                stop_flag.as_ref(),
            )
            .map_err(|err| match err {
                BrowserRpcError::Cancelled => ExecError::cancelled(),
                BrowserRpcError::Message(err) => ExecError::new(
                    format!("Не удалось открыть URL '{url}' в браузере."),
                    format!("open_url: daemon rpc: {err}"),
                ),
            })?;
        Ok(())
    }

    fn browser_scroll_page(&mut self) -> Result<(), ExecError> {
        self.check_cancelled()?;
        let stop_flag = Arc::clone(&self.stop_flag);
        let daemon = self.ensure_browser_daemon()?;
        daemon
            .send_command_interruptible(
                &serde_json::json!({ "command": "scroll_page" }),
                ExpectedDaemonEvent::Scrolled,
                stop_flag.as_ref(),
            )
            .map_err(|err| match err {
                BrowserRpcError::Cancelled => ExecError::cancelled(),
                BrowserRpcError::Message(err) => ExecError::new(
                    "Не удалось прокрутить страницу.",
                    format!("scroll_page: daemon rpc: {err}"),
                ),
            })?;
        Ok(())
    }

    fn browser_fetch_images(&mut self, pattern: &str) -> Result<Vec<RgbaImage>, ExecError> {
        self.check_cancelled()?;
        let stop_flag = Arc::clone(&self.stop_flag);
        let daemon = self.ensure_browser_daemon()?;
        let browser = daemon.browser_name.clone();
        // Use the existing "fetch" command; it stores images in a temp output_dir.
        let response = daemon
            .send_command_interruptible(
                &serde_json::json!({
                    "command": "fetch",
                    "browser": browser,
                    "pattern": pattern,
                    "max_parallel": 4,
                }),
                ExpectedDaemonEvent::Result,
                stop_flag.as_ref(),
            )
            .map_err(|err| match err {
                BrowserRpcError::Cancelled => ExecError::cancelled(),
                BrowserRpcError::Message(err) => ExecError::new(
                    "Не удалось получить изображения из браузера.",
                    format!("fetch_from_browser: daemon rpc: {err}"),
                ),
            })?;

        // Response: {"event": "result", "output_dir": "...", "downloaded_images": N}
        let output_dir = response
            .get("output_dir")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| {
                ExecError::new(
                    "Браузерный daemon не вернул output_dir.",
                    "fetch_from_browser: missing output_dir in response",
                )
            })?;

        load_images_from_dir(&output_dir).map_err(|err| {
            ExecError::new(
                "Не удалось загрузить изображения из временной папки браузера.",
                format!(
                    "fetch_from_browser: load from '{}': {err}",
                    output_dir.display()
                ),
            )
        })
    }
}

impl Drop for BatchExecutor {
    fn drop(&mut self) {
        let _ = self.browser_daemon.take();
    }
}

// ─── Browser daemon (JSON-RPC over stdio) ─────────────────────────────────────

#[derive(Debug)]
enum BrowserRpcError {
    Cancelled,
    Message(String),
}

/// Per-frame timeout for a batch browser IPC command (generous: some stages are
/// silent for a while). The browser session lives inside the unified AI backend.
const BATCH_BROWSER_TIMEOUT: Duration = Duration::from_secs(600);

/// Drives the in-process Selenium browser session in the unified AI backend over
/// framed IPC (method `browser.command`). The backend process is app-global, so
/// this no longer owns a child process — only a cloneable client handle.
struct BrowserDaemon {
    client: backend_ipc::BackendClient,
    /// Last browser name used (reused for fetch command).
    pub browser_name: String,
}

impl BrowserDaemon {
    fn spawn() -> Result<Self, String> {
        let client =
            crate::launcher::new_project::advanced_download::connect_browser_backend()?;
        let daemon = Self {
            client,
            browser_name: String::new(),
        };
        // Batch automation always uses the Selenium backend; best-effort select.
        let _ = daemon.send_simple(&serde_json::json!({
            "command": "set_backend",
            "backend": "selenium",
        }));
        Ok(daemon)
    }

    /// Fire-and-wait for a quick control command (no cancellation). Used for setup
    /// (`set_backend`) and teardown (`close`).
    fn send_simple(&self, cmd: &serde_json::Value) -> Result<serde_json::Value, String> {
        match self.client.call(
            backend_ipc::protocol::METHOD_BROWSER_COMMAND,
            serde_json::json!({ "payload": cmd }),
            &[],
            BATCH_BROWSER_TIMEOUT,
        ) {
            Ok((header, _blob)) => Ok(header),
            Err(err) => Err(format!("{err:?}")),
        }
    }

    fn send_command_interruptible(
        &mut self,
        cmd: &serde_json::Value,
        expected_event: ExpectedDaemonEvent,
        stop_flag: &AtomicBool,
    ) -> Result<serde_json::Value, BrowserRpcError> {
        let client = self.client.clone();
        let handle = client
            .begin_call(
                backend_ipc::protocol::METHOD_BROWSER_COMMAND,
                serde_json::json!({ "payload": cmd }),
                &[],
            )
            .map_err(BrowserRpcError::Message)?;
        let id = handle.id();
        let (tx, rx) = mpsc::channel();

        thread::scope(|scope| {
            scope.spawn(move || {
                // Ignore progress frames here; the batch flow only needs the result.
                let result = handle.wait_streaming(|_header, _blob| {}, BATCH_BROWSER_TIMEOUT);
                let _ = tx.send(result);
            });

            let mut cancel_sent = false;
            loop {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(result) => return interpret_batch_terminal(result, expected_event),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        // On cancel, ask the backend to stop; the worker then returns
                        // an `interrupted` terminal which maps to `Cancelled`.
                        if !cancel_sent && stop_flag.load(Ordering::Relaxed) {
                            let _ = client.cancel(id);
                            cancel_sent = true;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(BrowserRpcError::Message(
                            "browser ipc response channel disconnected".to_owned(),
                        ));
                    }
                }
            }
        })
    }
}

impl Drop for BrowserDaemon {
    fn drop(&mut self) {
        // Close the live browser session; the app-global backend process keeps running.
        let _ = self.send_simple(&serde_json::json!({ "command": "close" }));
    }
}

/// Maps a `browser.command` IPC outcome to the legacy `(Value | BrowserRpcError)`
/// the batch executor expects. The single terminal event dict is the response
/// header; `ExpectedDaemonEvent` is advisory (there is exactly one terminal).
fn interpret_batch_terminal(
    result: Result<(serde_json::Value, Vec<u8>), backend_ipc::CallError>,
    expected_event: ExpectedDaemonEvent,
) -> Result<serde_json::Value, BrowserRpcError> {
    let _ = expected_event;
    match result {
        Ok((header, _blob)) => {
            let event = header
                .get("event")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if event == "error" {
                let msg = header
                    .get("user_message")
                    .or_else(|| header.get("message"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown daemon error");
                return Err(BrowserRpcError::Message(msg.to_owned()));
            }
            Ok(header)
        }
        Err(backend_ipc::CallError::Interrupted(_)) => Err(BrowserRpcError::Cancelled),
        Err(backend_ipc::CallError::Error(msg)) => Err(BrowserRpcError::Message(msg)),
        Err(backend_ipc::CallError::Transport(msg)) => Err(BrowserRpcError::Message(msg)),
    }
}

// ─── File loading helper ──────────────────────────────────────────────────────

fn load_images_from_dir(dir: &Path) -> Result<Vec<RgbaImage>, String> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("read_dir '{}': {e}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            matches!(
                p.extension().and_then(|s| s.to_str()),
                Some("png" | "jpg" | "jpeg" | "webp")
            )
        })
        .collect();
    entries.sort_unstable();

    let mut images = Vec::with_capacity(entries.len());
    for path in &entries {
        let img = image::open(path)
            .map_err(|e| format!("open '{}': {e}", path.display()))?
            .to_rgba8();
        images.push(img);
    }
    Ok(images)
}

/// The terminal event a batch browser command expects. Advisory now (the IPC
/// call delivers exactly one terminal event), kept for call-site readability.
#[derive(Clone, Copy)]
enum ExpectedDaemonEvent {
    Opened,
    Scrolled,
    Result,
}
