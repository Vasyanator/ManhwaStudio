/*
File: models/layer_model/manifest.rs

Purpose:
On-disk schema (serde) for the unified layer model shared by the PS editor and (in later phases)
the typing tab. This is the stable contract written to `{chapter}/layers/layers.json`; the
in-memory model and capability rules live in the parent module. Only this file decides the JSON
shape, so it is deliberately forward-looking: it already carries the fields that future phases need
(layer groups, deform mesh, effects chain, a payload reference into `text_images/`) even though
phase 1 only writes raster layers.

Layout on disk:
    {chapter}/layers/
        layers.json                      // this manifest (all pages)
        ps_p{page:04}_{uid}.png          // per-layer base pixels (pre-effects)
        ps_p{page:04}_{uid}_rendered.png // optional post-effects cache (future)

Versioning:
`schema_version` is explicit (unlike `text_info.json`, which infers format from field presence) so
groups / effects / typing-sync can evolve the shape with a real migration step.
*/

use serde::{Deserialize, Serialize};

/// Current on-disk schema version for `layers.json`.
///
/// v2 adds `LayerRec.pinned_by_group` and `GroupRec.collapsed` — both additive with serde defaults,
/// so a v1 file still reads cleanly and a v2 file reads best-effort on an older binary.
///
/// v3 inlines the text payload onto a TEXT `LayerRec` (`render_data`, the rendered-PNG filename in
/// `rendered_file`, `mask_clip`, and the reused `transform`/`deform` geometry) so `layers.json`
/// becomes self-sufficient for text and `text_info.json` is read-only legacy. All v3 fields are
/// serde-default `Option`s, so a v2 file (text payload only in `text_info.json`) still reads cleanly
/// and the load layer folds the legacy payload on read (see `compat.rs` / `layer_doc::ensure_page_loaded`).
pub const LAYERS_SCHEMA_VERSION: u32 = 3;

/// Root of `layers.json`: every page's layer tree in one file (mirrors `text_info.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayersManifest {
    pub schema_version: u32,
    #[serde(default)]
    pub pages: Vec<PageLayers>,
}

impl LayersManifest {
    /// An empty manifest at the current schema version.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            schema_version: LAYERS_SCHEMA_VERSION,
            pages: Vec::new(),
        }
    }

    /// The page entry for `img_idx`, if present.
    #[must_use]
    pub fn page(&self, img_idx: usize) -> Option<&PageLayers> {
        self.pages.iter().find(|p| p.img_idx == img_idx)
    }

    /// Inserts or replaces the entry for `page.img_idx`, keeping `pages` ordered by index.
    pub fn upsert_page(&mut self, page: PageLayers) {
        match self.pages.iter().position(|p| p.img_idx == page.img_idx) {
            Some(i) => self.pages[i] = page,
            None => {
                self.pages.push(page);
                self.pages.sort_by_key(|p| p.img_idx);
            }
        }
    }

    /// Removes the entry for `img_idx`. Returns `true` if one existed.
    pub fn remove_page(&mut self, img_idx: usize) -> bool {
        let before = self.pages.len();
        self.pages.retain(|p| p.img_idx != img_idx);
        self.pages.len() != before
    }
}

/// All layers for a single page.
///
/// The unified bottom-to-top Z is over *bands*: each raster node (`LayerRec` with a `z`), each text
/// group (`TextGroupRec` with a `z`), and each pinned text node (`LayerRec` with `pinned = true` and
/// a `z`) is one band. An unpinned text node carries no band of its own — it belongs to the text
/// group named by its `layer_idx`, and within that group's band the texts auto-order by their page-Y
/// (resolved by the tab from `text_info.json`). See `ordering::page_bands`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageLayers {
    pub img_idx: usize,
    /// PS raster-layer groups (organizational; collapse/visibility). Distinct from text groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<GroupRec>,
    /// Text-group bands keyed by `layer_idx`, each with its band `z`. One entry per distinct
    /// `layer_idx` present among the page's unpinned text nodes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub text_groups: Vec<TextGroupRec>,
    #[serde(default)]
    pub tree: Vec<LayerRec>,
}

/// A text-group band: all unpinned text nodes sharing `layer_idx` render together at band `z`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextGroupRec {
    pub layer_idx: u32,
    /// Band position on the unified Z axis (shared with raster nodes and pinned texts).
    pub z: u32,
    pub name: String,
}

/// Distinguishes the layer kinds and carries the agreed capability rules.
///
/// The rules encode the design: a normal raster layer accepts every pixel operation; a text layer
/// is re-renderable but must be rasterized before any operation a later text render would discard
/// (paint / cut / merge); a group is a container. Effects (the second render type) apply to any
/// non-group layer, so they are *not* gated by kind here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerKindRec {
    Raster,
    Text,
    Group,
}

// The per-kind capability rules (can_paint / can_text_render / …) were design scaffolding never wired
// into production — the tabs gate operations inline by `LayerKind` — so they were removed in Phase D
// to keep `manifest.rs` free of dead code. Reintroduce them next to this enum if a future phase needs
// a shared capability table.

fn default_kind() -> LayerKindRec {
    LayerKindRec::Raster
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One layer node in a page's tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerRec {
    /// Stable identifier (UUID string) that survives sessions and enables cross-tab references.
    pub uid: String,
    pub name: String,
    #[serde(default = "default_kind")]
    pub kind: LayerKindRec,
    /// Band position on the unified Z axis. Meaningful for raster nodes and *pinned* text nodes;
    /// for an unpinned text node the effective Z comes from its text group instead.
    pub z: u32,
    /// Text node only: which text group (Группа текста N) it belongs to. `None` for rasters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_idx: Option<u32>,
    /// Text node only: when true the text was given an explicit Z in the PS editor and no longer
    /// auto-orders by page-Y within a group — it is its own band at `z`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned: bool,
    /// Text node only: the `pinned` flag was set *implicitly* because the text was put into a PS
    /// group (a grouped text must own its Z band to sit anywhere in the group's contiguous run).
    /// Distinguishes this from a user's explicit pin so ungrouping can restore page-Y auto-order
    /// without clobbering a real pin. See `tabs/ps_editor` unified layer tree.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pinned_by_group: bool,
    /// Unified PS-tree group membership (the `group_uid` of the `GroupRec` this node belongs to).
    /// Set for both raster and text nodes — a PS group may mix the two. Orthogonal to a text node's
    /// `layer_idx` (which stays the typing tab's «Группа текста N» axis).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_uid: Option<String>,
    pub visible: bool,
    pub opacity: f32,
    /// Owned placement, for nodes that own their geometry (raster layers). Text nodes leave this
    /// `None`: their geometry lives in `text_info.json`, reached through `payload_ref`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<TransformRec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deform: Option<DeformRec>,
    /// Pre-effects base pixels (relative filename in the layers dir). Absent for pure references.
    /// A TEXT node has no separate pre-effects base — its only image is the text render — so it
    /// leaves this `None` and stores the rendered PNG in `rendered_file` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_file: Option<String>,
    /// For a RASTER node: the cached post-effects result, regenerable from `base_file` + `effects`.
    /// For a TEXT node (schema v3): the node's single rendered PNG — the text render that the tabs
    /// display. Reused (rather than a new `text_image_file`) because the two semantics never overlap:
    /// a text node has no raster effects chain and no `base_file`, so `rendered_file` unambiguously
    /// names "the layer's rendered image" for both kinds. The PNG lives in the same layers dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendered_file: Option<String>,
    /// Intrinsic size of `base_file`, used to validate decoded pixels on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_size: Option<[usize; 2]>,
    /// Post-effects chain (the second render type), opaque here. Future use.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<serde_json::Value>,
    /// Reference to a payload owned by another store (e.g. a text overlay in `text_images/`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_ref: Option<PayloadRef>,
    /// TEXT node only (schema v3): the opaque text render payload (font / glyph / styling params),
    /// formerly only in `text_info.json`'s `render_data`. `Some` marks a node as carrying its full
    /// inline payload — the load layer then builds the text node from `layers.json` alone and no longer
    /// needs `text_info.json` (a v2 node leaves this `None` and is folded from the legacy file on read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_data: Option<serde_json::Value>,
    /// TEXT and RASTER nodes (schema v3): persists the typing tab's mask-clip flag (whether the layer
    /// is clipped to the page mask). Previously in-memory only and lost across save/restart. `None` ⇒
    /// default (no clip; rasters default OFF, text defaults per the typing tab).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_clip: Option<bool>,
}

/// Affine placement of a layer image in page-pixel space (center-anchored, matches the typing tab).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TransformRec {
    pub cx: f32,
    pub cy: f32,
    pub rotation: f32,
    pub scale: f32,
}

/// Mesh deformation grid in page pixels (same shape the typing tab already stores).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeformRec {
    pub cols: usize,
    pub rows: usize,
    pub points_px: Vec<[f32; 2]>,
}

/// A layer group (folder) in the unified PS tree. May contain both raster and text nodes (members
/// carry this group's `uid` in their `group_uid`). `parent_uid` is reserved for future nesting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupRec {
    pub uid: String,
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    /// Panel-only: the group is collapsed (children hidden in the PS layers panel). Does not affect
    /// compositing or the typing tab.
    #[serde(default, skip_serializing_if = "is_false")]
    pub collapsed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_uid: Option<String>,
}

/// Pointer from a layer node to a payload stored elsewhere (e.g. typing's `text_info.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayloadRef {
    /// Logical store name, e.g. `"text_images"`.
    pub store: String,
    /// Stable id of the referenced item within that store.
    pub uid: String,
}
