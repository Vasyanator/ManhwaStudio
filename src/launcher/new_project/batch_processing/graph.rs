/*
File: src/launcher/new_project/batch_processing/graph.rs

Purpose:
In-memory graph model for the node-based batch processing editor.

Main responsibilities:
- Store nodes (GraphNode), edges (GraphEdge), and variables (GraphVariable)
- Validate connections before insertion (direction, kind, type compatibility)
- Serialize/deserialize the graph to/from JSON (compatible with the Python version=1 format)
- Provide mutation helpers (add/remove node/edge) used by the canvas and variable panel

Key structures:
- GraphModel   — root model
- GraphNode    — one node on the canvas
- GraphEdge    — one connection between sockets
- GraphVariable — one user-defined variable

Notes:
JSON compatibility with the Python format is maintained for version=1 files.
The "params" field is stored as a serde_json::Value flat object so legacy files
can be round-tripped even when new fields are added.
*/

use super::node_defs::NodeDefs;
use super::types::{DataType, NodeParams, SocketKind, SocketSpec};
use serde_json::{Value, json};

// ─── Model structs ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct GraphNode {
    pub id: u32,
    pub params: NodeParams,
    pub pos: egui::Pos2,
}

impl GraphNode {
    pub fn template_key(&self) -> &'static str {
        self.params.template_key()
    }
}

/// Direction of a connection edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    Exec,
    Data,
}

#[derive(Debug, Clone)]
pub struct GraphEdge {
    pub id: u32,
    pub kind: EdgeKind,
    pub src_node: u32,
    pub src_socket: String,
    pub dst_node: u32,
    pub dst_socket: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GraphVariable {
    pub name: String,
    pub data_type: DataType,
    pub persist_between_cycles: bool,
}

// ─── Connection validation error ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum ConnectError {
    /// Source socket not found on its node.
    SrcSocketNotFound,
    /// Destination socket not found on its node.
    DstSocketNotFound,
    /// Source must be an output, destination must be an input.
    WrongDirection,
    /// Exec↔Data mismatch.
    KindMismatch,
    /// Incompatible data types.
    TypeMismatch { src: DataType, dst: DataType },
    /// The input socket already has a connection and does not allow multiple.
    InputAlreadyConnected,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // These messages are surfaced to the user via `launcher.batch.connect_error`
        // (window.rs). They are message *values*, never persisted or `==`-compared.
        match self {
            Self::SrcSocketNotFound => {
                write!(f, "{}", t!("launcher.batch.graph.connect_src_socket_not_found"))
            }
            Self::DstSocketNotFound => {
                write!(f, "{}", t!("launcher.batch.graph.connect_dst_socket_not_found"))
            }
            Self::WrongDirection => {
                write!(f, "{}", t!("launcher.batch.graph.connect_wrong_direction"))
            }
            Self::KindMismatch => {
                write!(f, "{}", t!("launcher.batch.graph.connect_kind_mismatch"))
            }
            Self::TypeMismatch { src, dst } => {
                write!(
                    f,
                    "{}",
                    tf!(
                        "launcher.batch.graph.connect_type_mismatch",
                        src = src.label(),
                        dst = dst.label()
                    )
                )
            }
            Self::InputAlreadyConnected => {
                write!(f, "{}", t!("launcher.batch.graph.connect_input_already_connected"))
            }
        }
    }
}

// ─── GraphModel ───────────────────────────────────────────────────────────────

pub struct GraphModel {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub variables: Vec<GraphVariable>,
    next_node_id: u32,
    next_edge_id: u32,
}

impl Default for GraphModel {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphModel {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            variables: Vec::new(),
            next_node_id: 1,
            next_edge_id: 1,
        }
    }

    // ── Node mutation ────────────────────────────────────────────────────────

    pub fn add_node(&mut self, params: NodeParams, pos: egui::Pos2) -> u32 {
        let id = self.next_node_id;
        self.next_node_id += 1;
        self.nodes.push(GraphNode { id, params, pos });
        id
    }

    pub fn remove_node(&mut self, id: u32) {
        self.nodes.retain(|n| n.id != id);
        self.edges.retain(|e| e.src_node != id && e.dst_node != id);
    }

    pub fn node_by_id(&self, id: u32) -> Option<&GraphNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn node_by_id_mut(&mut self, id: u32) -> Option<&mut GraphNode> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    // ── Edge mutation ────────────────────────────────────────────────────────

    /// Validate and insert an edge. Returns an error without modifying state if invalid.
    pub fn add_edge(
        &mut self,
        defs: &NodeDefs,
        src_node: u32,
        src_socket: &str,
        dst_node: u32,
        dst_socket: &str,
    ) -> Result<u32, ConnectError> {
        let src_spec = self
            .node_by_id(src_node)
            .and_then(|n| defs.socket_spec(n.template_key(), src_socket))
            .ok_or(ConnectError::SrcSocketNotFound)?;

        let dst_spec = self
            .node_by_id(dst_node)
            .and_then(|n| defs.socket_spec(n.template_key(), dst_socket))
            .ok_or(ConnectError::DstSocketNotFound)?;

        validate_connection(&src_spec, &dst_spec)?;

        // Check if the input slot is already occupied (for non-allow_multiple sockets).
        if !dst_spec.allow_multiple {
            let occupied = self
                .edges
                .iter()
                .any(|e| e.dst_node == dst_node && e.dst_socket == dst_socket);
            if occupied {
                return Err(ConnectError::InputAlreadyConnected);
            }
        }

        let kind = match &src_spec.kind {
            SocketKind::Exec => EdgeKind::Exec,
            SocketKind::Data(_) => EdgeKind::Data,
        };

        let id = self.next_edge_id;
        self.next_edge_id += 1;
        self.edges.push(GraphEdge {
            id,
            kind,
            src_node,
            src_socket: src_socket.to_owned(),
            dst_node,
            dst_socket: dst_socket.to_owned(),
        });
        Ok(id)
    }

    // ── Variable mutation ────────────────────────────────────────────────────

    pub fn add_variable(&mut self, var: GraphVariable) {
        self.variables.push(var);
    }

    pub fn remove_variable(&mut self, name: &str) {
        self.variables.retain(|v| v.name != name);
        // Drop variable_read/write nodes that referenced this variable.
        let ids_to_remove: Vec<u32> = self
            .nodes
            .iter()
            .filter(|n| match &n.params {
                NodeParams::VariableRead { variable_name }
                | NodeParams::VariableWrite { variable_name } => variable_name == name,
                _ => false,
            })
            .map(|n| n.id)
            .collect();
        for id in ids_to_remove {
            self.remove_node(id);
        }
    }

    // ── Serialisation ────────────────────────────────────────────────────────

    /// Serialise to JSON format compatible with Python version=1.
    pub fn to_json(&self) -> Value {
        let nodes: Vec<Value> = self
            .nodes
            .iter()
            .map(|n| {
                let params_value = params_to_flat_json(&n.params);
                json!({
                    "id": n.id,
                    "template_key": n.template_key(),
                    "x": n.pos.x,
                    "y": n.pos.y,
                    "params": params_value,
                })
            })
            .collect();

        let edges: Vec<Value> = self
            .edges
            .iter()
            .map(|e| {
                let kind_str = match e.kind {
                    EdgeKind::Exec => "exec",
                    EdgeKind::Data => "data",
                };
                json!({
                    "kind": kind_str,
                    "src_node_id": e.src_node,
                    "src_socket": e.src_socket,
                    "dst_node_id": e.dst_node,
                    "dst_socket": e.dst_socket,
                })
            })
            .collect();

        let variables: Vec<Value> = self
            .variables
            .iter()
            .map(|v| {
                json!({
                    "name": v.name,
                    "data_type": match v.data_type {
                        DataType::Int => "int",
                        DataType::Str => "str",
                        DataType::ImageList => "image_list",
                    },
                    "persist_between_cycles": v.persist_between_cycles,
                })
            })
            .collect();

        json!({
            "version": 1,
            "nodes": nodes,
            "edges": edges,
            "variables": variables,
        })
    }

    /// Deserialise from JSON. Returns a new model or an error string.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let version = value.get("version").and_then(Value::as_u64).unwrap_or(1);
        if version != 1 {
            return Err(tf!("launcher.batch.graph.unsupported_version_error", version = version));
        }

        let mut model = Self::new();
        let mut max_node_id: u32 = 0;
        let mut max_edge_id: u32 = 0;

        // Parse variables first (nodes may reference them).
        if let Some(vars) = value.get("variables").and_then(Value::as_array) {
            for v in vars {
                let name = v
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let data_type = match v.get("data_type").and_then(Value::as_str) {
                    Some("int") => DataType::Int,
                    Some("str") => DataType::Str,
                    Some("image_list") => DataType::ImageList,
                    other => {
                        return Err(tf!(
                            "launcher.batch.graph.unknown_variable_type_error",
                            name = name,
                            other = format!("{other:?}")
                        ));
                    }
                };
                let persist = v
                    .get("persist_between_cycles")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                model.variables.push(GraphVariable {
                    name,
                    data_type,
                    persist_between_cycles: persist,
                });
            }
        }

        // Parse nodes.
        if let Some(nodes) = value.get("nodes").and_then(Value::as_array) {
            for n in nodes {
                let id = n
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or("node missing id")?;
                let id = u32::try_from(id).map_err(|_| "node id overflow")?;
                let key = n.get("template_key").and_then(Value::as_str).unwrap_or("");
                let x = n.get("x").and_then(Value::as_f64).unwrap_or(0.0) as f32;
                let y = n.get("y").and_then(Value::as_f64).unwrap_or(0.0) as f32;
                let flat_params = n
                    .get("params")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));

                let params = params_from_flat_json(key, &flat_params)
                    .ok_or_else(|| tf!("launcher.batch.graph.unknown_node_type_error", key = key))?;

                model.nodes.push(GraphNode {
                    id,
                    params,
                    pos: egui::pos2(x, y),
                });
                max_node_id = max_node_id.max(id);
            }
        }

        // Parse edges.
        if let Some(edges) = value.get("edges").and_then(Value::as_array) {
            for e in edges {
                let kind = match e.get("kind").and_then(Value::as_str) {
                    Some("exec") => EdgeKind::Exec,
                    Some("data") => EdgeKind::Data,
                    other => {
                        return Err(tf!(
                            "launcher.batch.graph.unknown_edge_kind_error",
                            other = format!("{other:?}")
                        ));
                    }
                };
                let src_node = u32::try_from(
                    e.get("src_node_id")
                        .and_then(Value::as_u64)
                        .ok_or("edge missing src_node_id")?,
                )
                .map_err(|_| "edge src_node_id overflow")?;
                let dst_node = u32::try_from(
                    e.get("dst_node_id")
                        .and_then(Value::as_u64)
                        .ok_or("edge missing dst_node_id")?,
                )
                .map_err(|_| "edge dst_node_id overflow")?;
                let src_socket = e
                    .get("src_socket")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let dst_socket = e
                    .get("dst_socket")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();

                let edge_id = max_edge_id + 1;
                max_edge_id = edge_id;
                model.edges.push(GraphEdge {
                    id: edge_id,
                    kind,
                    src_node,
                    src_socket,
                    dst_node,
                    dst_socket,
                });
            }
        }

        model.next_node_id = max_node_id + 1;
        model.next_edge_id = max_edge_id + 1;
        Ok(model)
    }
}

// ─── Connection validation ────────────────────────────────────────────────────

fn validate_connection(src: &SocketSpec, dst: &SocketSpec) -> Result<(), ConnectError> {
    if src.is_input || !dst.is_input {
        return Err(ConnectError::WrongDirection);
    }
    match (&src.kind, &dst.kind) {
        (SocketKind::Exec, SocketKind::Exec) => {}
        (SocketKind::Data(a), SocketKind::Data(b)) => {
            if a != b {
                return Err(ConnectError::TypeMismatch { src: *a, dst: *b });
            }
        }
        _ => return Err(ConnectError::KindMismatch),
    }
    Ok(())
}

// ─── JSON helpers ─────────────────────────────────────────────────────────────

/// Flatten NodeParams into a plain JSON object for the `params` key.
fn params_to_flat_json(params: &NodeParams) -> Value {
    match params {
        NodeParams::StartNumber { start, step, end } => {
            json!({ "start": start, "step": step, "end": end })
        }
        NodeParams::StartString { path } => {
            json!({ "path": path.to_string_lossy() })
        }
        NodeParams::StringTemplate {
            template,
            placeholders,
        } => {
            json!({ "template": template, "placeholders": placeholders })
        }
        NodeParams::QuickDownloader | NodeParams::ScrollPage | NodeParams::End => {
            json!({})
        }
        NodeParams::OpenUrl { browser } => {
            json!({ "browser": browser.as_daemon_str() })
        }
        NodeParams::FetchFromBrowser { pattern } => {
            json!({ "pattern": pattern })
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
            json!({
                "parts": parts,
                "target_height": target_height,
                "band_rows": band_rows,
                "tolerance": tolerance,
                "search_radius": search_radius,
                "prefer_up_first": prefer_up_first,
                "auto_cut": auto_cut,
            })
        }
        NodeParams::Waifu2x {
            scale,
            noise,
            tile_size,
        } => {
            json!({ "scale": scale, "noise": noise, "tile_size": tile_size })
        }
        NodeParams::SaveFolder { path, name_prefix } => {
            json!({ "path": path.to_string_lossy(), "name_prefix": name_prefix })
        }
        NodeParams::VariableRead { variable_name } => {
            json!({ "variable_name": variable_name })
        }
        NodeParams::VariableWrite { variable_name } => {
            json!({ "variable_name": variable_name })
        }
    }
}

/// Reconstruct NodeParams from a flat JSON object (reverse of `params_to_flat_json`).
fn params_from_flat_json(key: &str, v: &Value) -> Option<NodeParams> {
    let str_field = |field: &str| {
        v.get(field)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let i64_field =
        |field: &str, default: i64| v.get(field).and_then(Value::as_i64).unwrap_or(default);
    let u64_field =
        |field: &str, default: u64| v.get(field).and_then(Value::as_u64).unwrap_or(default);
    let bool_field =
        |field: &str, default: bool| v.get(field).and_then(Value::as_bool).unwrap_or(default);

    match key {
        "start_number" => Some(NodeParams::StartNumber {
            start: i64_field("start", 0),
            step: i64_field("step", 1),
            end: i64_field("end", 10),
        }),
        "start_string" => Some(NodeParams::StartString {
            path: str_field("path").into(),
        }),
        "string_template" => {
            let placeholders = v
                .get("placeholders")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            Some(NodeParams::StringTemplate {
                template: str_field("template"),
                placeholders,
            })
        }
        "quick_downloader" => Some(NodeParams::QuickDownloader),
        "open_url" => {
            let browser = match v.get("browser").and_then(Value::as_str) {
                Some("chrome") => super::types::BrowserKind::Chrome,
                Some("edge") => super::types::BrowserKind::Edge,
                Some("safari") => super::types::BrowserKind::Safari,
                _ => super::types::BrowserKind::Firefox,
            };
            Some(NodeParams::OpenUrl { browser })
        }
        "scroll_page" => Some(NodeParams::ScrollPage),
        "fetch_from_browser" => Some(NodeParams::FetchFromBrowser {
            pattern: str_field("pattern"),
        }),
        "stitch_split" => Some(NodeParams::StitchSplit {
            parts: v.get("parts").and_then(Value::as_u64).map(|v| v as usize),
            target_height: u64_field("target_height", 4000) as usize,
            band_rows: u64_field("band_rows", 5) as usize,
            tolerance: u64_field("tolerance", 10) as u8,
            search_radius: u64_field("search_radius", 3000) as usize,
            prefer_up_first: bool_field("prefer_up_first", true),
            auto_cut: bool_field("auto_cut", true),
        }),
        "waifu2x" => Some(NodeParams::Waifu2x {
            scale: u64_field("scale", 2) as u32,
            noise: i64_field("noise", 1) as i32,
            tile_size: u64_field("tile_size", 256) as u32,
        }),
        "save_folder" => Some(NodeParams::SaveFolder {
            path: str_field("path").into(),
            name_prefix: {
                let p = str_field("name_prefix");
                if p.is_empty() { "page_".to_owned() } else { p }
            },
        }),
        "variable_read" => Some(NodeParams::VariableRead {
            variable_name: str_field("variable_name"),
        }),
        "variable_write" => Some(NodeParams::VariableWrite {
            variable_name: str_field("variable_name"),
        }),
        "end" => Some(NodeParams::End),
        _ => None,
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{GraphModel, GraphVariable};
    use crate::launcher::new_project::batch_processing::node_defs::NodeDefs;
    use crate::launcher::new_project::batch_processing::types::{DataType, NodeParams, SocketKind};
    use serde_json::Value;
    use std::path::PathBuf;

    /// Builds a small graph exercising an exec edge and an `ImageList` data edge, each
    /// wired by the Cyrillic socket identifiers (`"Далее"`/`"Вход"`, `"Картинки"`).
    fn sample_graph_with_cyrillic_edges(defs: &NodeDefs) -> GraphModel {
        let mut g = GraphModel::new();
        let start = g.add_node(
            NodeParams::StartNumber {
                start: 0,
                step: 1,
                end: 5,
            },
            egui::pos2(0.0, 0.0),
        );
        let downloader = g.add_node(NodeParams::QuickDownloader, egui::pos2(200.0, 0.0));
        let save = g.add_node(
            NodeParams::SaveFolder {
                path: PathBuf::new(),
                name_prefix: "page_".to_owned(),
            },
            egui::pos2(400.0, 0.0),
        );
        g.add_variable(GraphVariable {
            name: "cnt".to_owned(),
            data_type: DataType::Int,
            persist_between_cycles: true,
        });
        g.add_edge(defs, start, "Далее", downloader, "Вход")
            .expect("exec edge connects");
        g.add_edge(defs, downloader, "Картинки", save, "Картинки")
            .expect("image data edge connects");
        g
    }

    /// Asserts every edge reconnects: both endpoints resolve by their raw socket name,
    /// i.e. `add_edge` would not raise `Src/DstSocketNotFound` for the loaded graph.
    fn assert_all_edges_resolve(model: &GraphModel, defs: &NodeDefs) {
        for e in &model.edges {
            let src_key = model.node_by_id(e.src_node).expect("src node").template_key();
            let dst_key = model.node_by_id(e.dst_node).expect("dst node").template_key();
            assert!(
                defs.socket_spec(src_key, &e.src_socket).is_some(),
                "src socket {:?} did not resolve on {src_key}",
                e.src_socket
            );
            assert!(
                defs.socket_spec(dst_key, &e.dst_socket).is_some(),
                "dst socket {:?} did not resolve on {dst_key}",
                e.dst_socket
            );
        }
    }

    #[test]
    fn graph_round_trips_through_json_preserving_cyrillic_sockets() {
        let defs = NodeDefs::build();
        let g = sample_graph_with_cyrillic_edges(&defs);
        let json_before = g.to_json();
        let reloaded = GraphModel::from_json(&json_before).expect("reload succeeds");
        // Semantic round-trip: re-serializing the reloaded graph reproduces the wire form.
        assert_eq!(reloaded.to_json(), json_before);
        // The Cyrillic identifiers survived verbatim in the persisted edges.
        assert!(
            reloaded
                .edges
                .iter()
                .any(|e| e.src_socket == "Далее" && e.dst_socket == "Вход")
        );
        assert!(
            reloaded
                .edges
                .iter()
                .any(|e| e.src_socket == "Картинки" && e.dst_socket == "Картинки")
        );
        assert_all_edges_resolve(&reloaded, &defs);
    }

    #[test]
    fn python_format_cyrillic_graph_loads_and_edges_resolve() {
        let defs = NodeDefs::build();
        // Hand-authored JSON in the Python version=1 shape, edges keyed by the Cyrillic
        // socket names the Python implementation emits.
        let src = r#"{
            "version": 1,
            "nodes": [
                {"id": 1, "template_key": "start_number", "x": 0.0, "y": 0.0,
                 "params": {"start": 0, "step": 1, "end": 5}},
                {"id": 2, "template_key": "quick_downloader", "x": 100.0, "y": 0.0, "params": {}},
                {"id": 3, "template_key": "save_folder", "x": 200.0, "y": 0.0,
                 "params": {"path": "", "name_prefix": "page_"}}
            ],
            "edges": [
                {"kind": "exec", "src_node_id": 1, "src_socket": "Далее",
                 "dst_node_id": 2, "dst_socket": "Вход"},
                {"kind": "data", "src_node_id": 2, "src_socket": "Картинки",
                 "dst_node_id": 3, "dst_socket": "Картинки"}
            ],
            "variables": []
        }"#;
        let value: Value = serde_json::from_str(src).expect("fixture parses");
        let model = GraphModel::from_json(&value).expect("python-format graph loads");
        assert_eq!(model.nodes.len(), 3);
        assert_eq!(model.edges.len(), 2);
        assert_all_edges_resolve(&model, &defs);
    }

    #[test]
    fn executor_input_socket_identifiers_match_node_defs() {
        // `executor.rs` fetches node inputs by literal name (`inputs.get("Картинки")`,
        // `inputs.get("Путь")`, `inputs.get("Значение")`). Those literals must match the
        // declared socket identifiers, otherwise the lookup silently returns `None`.
        let defs = NodeDefs::build();
        let save_images = defs
            .socket_spec("save_folder", "Картинки")
            .expect("save_folder has a Картинки input");
        assert!(save_images.is_input);
        assert!(matches!(
            save_images.kind,
            SocketKind::Data(DataType::ImageList)
        ));
        assert!(
            defs.socket_spec("save_folder", "Путь")
                .expect("save_folder has a Путь input")
                .is_input
        );
        assert!(defs.socket_spec("stitch_split", "Картинки").is_some());
        assert!(defs.socket_spec("waifu2x", "Картинки").is_some());
        assert!(defs.socket_spec("variable_write", "Значение").is_some());
    }
}
