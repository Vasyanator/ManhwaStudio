/*
File: src/launcher/new_project/batch_processing/types.rs

Purpose:
Core type definitions for the node-based batch processing graph editor.

Main responsibilities:
- DataType: type of data flowing between nodes (Int, Str, ImageList)
- SocketKind: typed socket descriptor (Exec or Data with DataType)
- DataValue: runtime value passed between nodes during execution
- NodeParams: typed parameters for each of the 13 node kinds (replaces Python dict[str, object])
- SocketSpec: descriptor of a single socket on a node

Notes:
NodeParams uses struct variants so every field is checked at compile time,
avoiding the stringly-typed dict approach of the Python implementation.
*/

use image::RgbaImage;
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

// ─── Socket types ────────────────────────────────────────────────────────────

/// Type of data transported along a data socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    Int,
    Str,
    ImageList,
}

impl DataType {
    pub fn label(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Str => "str",
            Self::ImageList => t!("launcher.batch.image_list_type"),
        }
    }

    /// Accent colour for this type in the canvas UI (egui Color32 hex values).
    pub fn color(self) -> egui::Color32 {
        match self {
            Self::Int => egui::Color32::from_rgb(0x60, 0xa5, 0xfa), // blue-400
            Self::Str => egui::Color32::from_rgb(0xfb, 0x92, 0x3c), // orange-400
            Self::ImageList => egui::Color32::from_rgb(0x34, 0xd3, 0x99), // emerald-400
        }
    }
}

/// Determines what kind of connection a socket carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketKind {
    /// Control-flow edge.  Colour: gold.
    Exec,
    /// Typed data edge.
    Data(DataType),
}

impl SocketKind {
    pub fn color(&self) -> egui::Color32 {
        match self {
            Self::Exec => egui::Color32::from_rgb(0xfa, 0xcc, 0x15), // yellow-400
            Self::Data(dt) => dt.color(),
        }
    }
}

// ─── Socket spec ─────────────────────────────────────────────────────────────

/// Static description of one socket on a node template.
#[derive(Debug, Clone)]
pub struct SocketSpec {
    /// **Serialized identifier only** — NOT a display label. `Graph::to_json` writes it as
    /// an edge's `src_socket`/`dst_socket`, `from_json` resolves edges by it (`socket_spec`),
    /// and `executor.rs` looks runtime values up by it. The graph format is documented as
    /// compatible with the Python `version=1` format, so this must NEVER be localized — a
    /// translated name orphans every saved graph and breaks Python interop (see
    /// `docs/i18n_exclusions.md` §A2).
    ///
    /// It is a `Cow` so static node templates borrow a `&'static str` (allocation-free) while
    /// dynamic sockets (`node_defs::socket_specs_for_node`) own a user-authored `String`
    /// without leaking. The painted label is `label_key`, not this field.
    pub name: Cow<'static, str>,
    /// Catalog key for the UI label painted on the node (`canvas.rs`). `Some(key)` paints
    /// `t!(key)`; `None` paints `name` verbatim. Fixed template sockets carry a key so the
    /// editor localizes them; dynamic user-authored sockets (string-template placeholders,
    /// variable nodes) carry `None` and show their raw name.
    pub label_key: Option<&'static str>,
    pub is_input: bool,
    pub kind: SocketKind,
    /// Allow more than one connection to this socket (fan-in for exec, multi-source for data).
    pub allow_multiple: bool,
}

impl SocketSpec {
    pub fn exec_in(name: impl Into<Cow<'static, str>>) -> Self {
        Self {
            name: name.into(),
            label_key: None,
            is_input: true,
            kind: SocketKind::Exec,
            allow_multiple: true,
        }
    }

    pub fn exec_out(name: impl Into<Cow<'static, str>>) -> Self {
        Self {
            name: name.into(),
            label_key: None,
            is_input: false,
            kind: SocketKind::Exec,
            allow_multiple: false,
        }
    }

    pub fn data_in(name: impl Into<Cow<'static, str>>, dt: DataType) -> Self {
        Self {
            name: name.into(),
            label_key: None,
            is_input: true,
            kind: SocketKind::Data(dt),
            allow_multiple: false,
        }
    }

    pub fn data_out(name: impl Into<Cow<'static, str>>, dt: DataType) -> Self {
        Self {
            name: name.into(),
            label_key: None,
            is_input: false,
            kind: SocketKind::Data(dt),
            allow_multiple: false,
        }
    }

    /// Attach a catalog key for the painted label. Chained after a constructor for fixed
    /// template sockets; dynamic sockets omit it and display their raw `name`.
    #[must_use]
    pub fn with_label(mut self, label_key: &'static str) -> Self {
        self.label_key = Some(label_key);
        self
    }

    /// The text to paint for this socket in the editor: the localized `label_key` when
    /// present, otherwise the raw `name` (which is the wire identifier). `ms_i18n::lookup`
    /// is a wait-free, allocation-free catalog read, so this is safe on the egui paint path
    /// (unlike `tf!`/`tp!`, which allocate). Never localizes the wire identifier itself.
    #[must_use]
    pub fn display_label(&self) -> &str {
        match self.label_key {
            Some(key) => ms_i18n::lookup(key).unwrap_or(key),
            None => self.name.as_ref(),
        }
    }
}

// ─── Runtime value ───────────────────────────────────────────────────────────

/// Value flowing along a data edge during pipeline execution.
#[derive(Debug, Clone)]
pub enum DataValue {
    Null,
    Int(i64),
    Str(String),
    /// Shared ownership: images may be large and cloning is expensive.
    ImageList(Arc<Vec<Arc<RgbaImage>>>),
}

impl DataValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

// ─── Node parameters ─────────────────────────────────────────────────────────

/// Browser kind used by browser-automation nodes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserKind {
    Firefox,
    Chrome,
    Edge,
    Safari,
}

impl BrowserKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Firefox => "Firefox",
            Self::Chrome => "Chrome",
            Self::Edge => "Edge",
            Self::Safari => "Safari",
        }
    }

    pub fn all() -> &'static [BrowserKind] {
        &[Self::Firefox, Self::Chrome, Self::Edge, Self::Safari]
    }

    /// The string understood by `adv_fetch_cli.py`.
    pub fn as_daemon_str(&self) -> &'static str {
        match self {
            Self::Firefox => "firefox",
            Self::Chrome => "chrome",
            Self::Edge => "edge",
            Self::Safari => "safari",
        }
    }
}

/// Typed parameters for every supported node kind.
/// Each variant mirrors exactly one of the 13 Python node templates.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "template_key", rename_all = "snake_case")]
pub enum NodeParams {
    StartNumber {
        start: i64,
        step: i64,
        end: i64,
    },
    StartString {
        /// Path to a UTF-8 text file; one line per iteration.
        path: PathBuf,
    },
    StringTemplate {
        /// Template text with `{placeholder}` slots.
        template: String,
        /// Names of the placeholders (determines which data-input sockets are created).
        placeholders: Vec<String>,
    },
    QuickDownloader,
    OpenUrl {
        browser: BrowserKind,
    },
    ScrollPage,
    FetchFromBrowser {
        /// Wildcard pattern for filtering URLs (e.g. `*.jpg`).
        pattern: String,
    },
    StitchSplit {
        parts: Option<usize>,
        target_height: usize,
        band_rows: usize,
        tolerance: u8,
        search_radius: usize,
        prefer_up_first: bool,
        auto_cut: bool,
    },
    Waifu2x {
        scale: u32,
        noise: i32,
        tile_size: u32,
    },
    SaveFolder {
        path: PathBuf,
        name_prefix: String,
    },
    VariableRead {
        variable_name: String,
    },
    VariableWrite {
        variable_name: String,
    },
    End,
}

impl NodeParams {
    /// Stable identifier used in JSON serialisation (matches Python `template_key`).
    pub fn template_key(&self) -> &'static str {
        match self {
            Self::StartNumber { .. } => "start_number",
            Self::StartString { .. } => "start_string",
            Self::StringTemplate { .. } => "string_template",
            Self::QuickDownloader => "quick_downloader",
            Self::OpenUrl { .. } => "open_url",
            Self::ScrollPage => "scroll_page",
            Self::FetchFromBrowser { .. } => "fetch_from_browser",
            Self::StitchSplit { .. } => "stitch_split",
            Self::Waifu2x { .. } => "waifu2x",
            Self::SaveFolder { .. } => "save_folder",
            Self::VariableRead { .. } => "variable_read",
            Self::VariableWrite { .. } => "variable_write",
            Self::End => "end",
        }
    }

    /// Human-readable title shown in the node header.
    pub fn title(&self) -> &'static str {
        match self {
            Self::StartNumber { .. } => t!("launcher.batch.node_number_start_title"),
            Self::StartString { .. } => t!("launcher.batch.node_string_start_title"),
            Self::StringTemplate { .. } => t!("launcher.batch.node_string_template_title"),
            Self::QuickDownloader => t!("launcher.batch.node_quick_download_title"),
            Self::OpenUrl { .. } => t!("launcher.batch.node_open_url_title"),
            Self::ScrollPage => t!("launcher.batch.node_scroll_page_title"),
            Self::FetchFromBrowser { .. } => t!("launcher.batch.node_browser_fetch_title"),
            Self::StitchSplit { .. } => t!("launcher.batch.node_stitch_cut_title"),
            Self::Waifu2x { .. } => "Waifu2x",
            Self::SaveFolder { .. } => t!("launcher.batch.node_save_folder_title"),
            Self::VariableRead { .. } => t!("launcher.batch.node_read_variable_title"),
            Self::VariableWrite { .. } => t!("launcher.batch.node_write_variable_title"),
            Self::End => t!("launcher.batch.node_end_title"),
        }
    }

    /// Whether this node is an iterator that drives the execution cycle.
    pub fn is_start_node(&self) -> bool {
        matches!(self, Self::StartNumber { .. } | Self::StartString { .. })
    }

    /// Default parameter values for newly spawned nodes.
    pub fn default_for_key(key: &str) -> Option<Self> {
        match key {
            "start_number" => Some(Self::StartNumber {
                start: 0,
                step: 1,
                end: 10,
            }),
            "start_string" => Some(Self::StartString {
                path: PathBuf::new(),
            }),
            "string_template" => Some(Self::StringTemplate {
                template: String::new(),
                placeholders: Vec::new(),
            }),
            "quick_downloader" => Some(Self::QuickDownloader),
            "open_url" => Some(Self::OpenUrl {
                browser: BrowserKind::Firefox,
            }),
            "scroll_page" => Some(Self::ScrollPage),
            "fetch_from_browser" => Some(Self::FetchFromBrowser {
                pattern: String::new(),
            }),
            "stitch_split" => Some(Self::StitchSplit {
                parts: None,
                target_height: 4000,
                band_rows: 5,
                tolerance: 10,
                search_radius: 3000,
                prefer_up_first: true,
                auto_cut: true,
            }),
            "waifu2x" => Some(Self::Waifu2x {
                scale: 2,
                noise: 1,
                tile_size: 256,
            }),
            "save_folder" => Some(Self::SaveFolder {
                path: PathBuf::new(),
                name_prefix: String::from("page_"),
            }),
            "variable_read" => Some(Self::VariableRead {
                variable_name: String::new(),
            }),
            "variable_write" => Some(Self::VariableWrite {
                variable_name: String::new(),
            }),
            "end" => Some(Self::End),
            _ => None,
        }
    }
}
