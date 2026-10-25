/*
File: crates/ms-tab-ps-editor/src/lib.rs

Purpose:
Crate root of the "PS-подобный редактор" tab. Re-exported by the binary from
`src/tabs/mod.rs` as `crate::tabs::ps_editor`, so every existing
`crate::tabs::ps_editor::…` call site keeps its path.

Layer: near the top of the library stack — above the canvas, the models, the tooling
primitives, the widgets and `ms-tab-typing` (the layers panel renders typing's text
preview label), and below `studio.rs` and the `page_manager` tab. It must never name `studio`
or `launcher`.

Orchestrates the "PS-подобный редактор" tab: a standalone, single-page, layered editor that is
deliberately NOT a `CanvasView`. It owns its own pan/zoom viewport, layer stack, selection, tool
set, and tiled GPU cache.

Key structures:
- `PsEditorTabState`: the tab state held by `StudioApp`.
- `PsEditorDockCx`: the per-frame context the panel dock lends to one tab body at a time.
- `PsEditorPanelVisibility`: the five "is this panel shown" flags, stored in the MAIN tab's
  `TabExtras` bag and toggled from its «Панели…» menu.

Architecture:
- UI shape: the canvas is the full-area BACKGROUND and every surface over it is a dock tab
  (`ms_widgets::panel_dock`) — «PS редактор», «Инструменты», «Выбранный инструмент»,
  «Горячие клавиши», «Слои», «Коррекция». `ps_editor_default_dock_layout` is their default
  arrangement, and the tab supplies its OWN `DockArea` rect (it is not a canvas tab, so
  `canvas::dock_area_rect` — which reserves the canvas' scrollbar strip — does not apply).
- `viewport`: own camera (pan/zoom/fit), independent of the shared canvas engine.
- `studio_tab`: `impl ms_studio_tab::StudioTab` — the tab's answers to the studio root's
  order-free hooks (only the panel-dock default layout).
- `canvas_touch`: finger input from `ms_input::touch` (surface publication, long-press
  marker); `draw_canvas` applies its navigation to the camera, its selection drag to the active
  tool (`PsTool::touch_selection`) and its long-press to `draw_selection_menu`.
- `layers`: ordered layer stack with two locked base layers (source + clean) and user raster
  layers above them.
- `page_loader`: background worker that produces the two base-layer images for the active page.
- `layer_render`: per-layer tiled texture cache (budgeted upload, dirty tiles).
- `tools`: `PsTool` trait + the selection / brush / transform / deform / patch tools; the tab routes
  pointer input to the active tool.
  Tool-gesture lifecycle is the TAB's job: `set_active_tool` (the only writer of `active_tool_idx`)
  resets the outgoing tool, `request_page` resets the active one, and a frame routed to the pan /
  text drag instead calls `PsTool::freeze` (or `reset` on Esc) so a multi-frame outline neither
  outlives its page nor dies on a suppressed frame. The tool's cursor preview obeys the same
  occlusion gate as its input: `overlay_pointer` hides it under a floating panel unless
  `PsTool::gesture_in_flight` says a gesture started on bare canvas is still running. The brush's
  cursor is a PIXEL-EXACT outline of the pixels its next stamp would affect, drawn with this
  module's `walk_dash_runs`; see `tools/MODULE_README.md` for its contract and its fallbacks.
  A finished brush stroke is committed by `commit_brush_stroke`, driven by the tool's own
  stroke-end latch and called OUTSIDE the routing gate — a pointer release performed while the
  canvas pans never reaches the gate, and a commit inside it would silently lose the stroke.
- layers panel active row: `panel_primary` is the row the panel highlights and the row whose
  controls the strip shows. While a page is loaded it is ALWAYS `Some` and always names an existing
  row — `normalize_panel_primary` re-establishes that after every projection and page load, seeding
  it from `LayerStack::active_id()` (`Клин` on a fresh page). Base rows are selectable but
  structurally locked; `RowSel::is_base` is the predicate every structural consumer asks.
- selection storage: `set_selection` / `non_empty_selection` keep the page selection `Some` only
  while it selects at least one pixel — an all-zero mask draws no marquee yet still clips the brush.
- selection marquee: `draw_selection_marquee` paints the boundary loops of `Selection` as a static
  black/white dashed border. The loops trace the mask's PIXEL EDGES and can carry many thousands of
  one-pixel staircase steps, so the screen path is first decimated (`decimate_to_screen`) and then
  dashed by CUMULATIVE arc length (`walk_dash_runs`) — dashing each segment on its own would restart
  the pattern every pixel and paint a solid line.
- raster effects: applying a non-destructive effects chain runs the expensive
  `apply_effects_to_color_image` on a worker thread (`PsRasterEffectsJob::run`), never the GUI
  thread. `apply_effects_to_raster` clones the base pixels and spawns the render (stashing a
  latest-wins request via `pending_raster_effects` if one is already in flight);
  `poll_ps_raster_effects_jobs` (once per frame) does the cheap GUI-side apply (recenter, doc
  routing, reversible persist). Mirrors the typing tab's `apply_raster_effects_edit` pipeline.
- page-rectangle compositing: `visible_layers_bottom_to_top` resolves the composite ORDER and the
  per-layer opacity once, `composite_rect` walks the rectangle. Both ROI consumers go through them —
  selection copy/cut and the patch tool's destination composite and storage backdrop — so the two
  can never disagree about what a page rectangle looks like.
- «Коррекция»: a VIEW-ONLY colour correction of the composited canvas (`correction/`). It is the
  project's first GPU shader pass — an `egui_glow` paint callback inserted between `draw_composite`
  and the pixel-grid pass — and it never touches layer pixels, the doc or the saved project. Its GL
  objects are built lazily inside the callback and freed from `StudioApp::on_exit`.

- layer persistence: a page is read from disk ONCE (the page loader's decode, inserted into the
  shared `LayerDoc`); afterwards the doc is the only source, text pin meta included
  (`materialize_text_runtime_from_doc`). Every `layers.json` write is a doc saver job (sync only
  without a saver / without a doc); band / grouping edits (which also rewrite the texts' pin flags,
  see `persist::apply_band_order`) go through `apply_structural_edit`. There is no pin toggle: text
  order is set only by ▲▼ (every text is its own band). Group visibility / opacity and the text eye
  are doc-level (`write_group_meta`, `write_text_visibility`), so the typing tab and its export
  follow them; group collapse is panel-only view state, owned by the doc as well.

File map (this file keeps the `mod` list, constants and every type; behaviour is split by concern
into sibling files, each an `impl PsEditorTabState` block or a group of free functions reaching the
root through `use super::*`):
- `frame.rs` (`draw`, dock tab contents), `dock_panels.rs` (default layout, tab bodies),
  `doc_sync.rs` (doc -> view projection), `history.rs` (undo/redo, op appliers, doc writers),
  `page_session.rs` (camera, loader, persistence, tool/selection setters).
- `layers_panel.rs` («Слои» panel), `grouping.rs` (band moves, group ops), `raster_effects.rs`
  (effects editor + off-thread render), `canvas_draw.rs` (canvas, tool commits, composite, shortcuts),
  `selection_ops.rs` (text drag, marquee, clip ops, merge-down).
- `composite.rs`, `raster_ops.rs`, `marquee.rs`: pure helpers; `tests.rs`: unit tests.

Notes:
Base layers mirror existing models read-only and are never written back.
*/

#![warn(clippy::all)]

// The `ms-i18n` UI-string macros (`t!` / `tf!` / `tp!`), mounted crate-wide exactly as
// `src/lib.rs` mounts them for the binary: this tab's localized labels use the bare
// macro names the extraction tool emits and the key-validation test scans for.
#[macro_use]
extern crate ms_i18n;

mod canvas_draw;
mod canvas_touch;
mod composite;
pub mod correction;
mod doc_sync;
mod dock_panels;
pub mod edit_op;
mod frame;
mod grouping;
mod history;
pub mod layer_render;
pub mod layers;
mod layers_panel;
mod marquee;
pub mod page_loader;
mod page_session;
mod raster_effects;
mod raster_ops;
pub mod selection;
mod selection_ops;
mod studio_tab;
pub mod text_layers;
pub mod tools;
pub mod tree;
pub mod viewport;

#[cfg(test)]
mod composite_plan_tests;

// `pub`: `studio.rs::restore_panel_dock` (another crate) hands it to the app-owned dock state.
pub use dock_panels::ps_editor_default_dock_layout;

// Crate-private free functions of the child files that more than their own file calls, imported
// here so that every sibling and both test modules reach them through `use super::*`. A name only
// the tests use is imported by the test module itself.
use canvas_draw::ps_editor_common_hotkey_rows;
use composite::{
    composite_rect, composite_steps, composite_to_page, over, scale_premultiplied,
    stack_group_folds, stack_group_uids, visible_layers_bottom_to_top,
};
use dock_panels::{
    draw_active_tool_tab_body, draw_correction_tab_body, draw_hotkey_rows_grid,
    draw_hotkeys_tab_body, draw_layers_tab_body, draw_main_tab_body, draw_tool_panel_tab_body,
    draw_tools_tab_body,
};
use grouping::group_mut_by_uid;
use marquee::{decimate_to_screen, walk_dash_runs};
use raster_ops::{
    blend_premul_region, clip_into_new_layer, collect_brush_key_actions, deform_eq,
    layer_to_raster_node, layer_touches_selection, non_empty_selection, overlay_pointer,
    page_footprint_rect, page_pixel_index, pixel_region_edit_op, raster_below_by_band_z,
    rec_to_transform, sample_layer_world, tool_panel_visible, transform_to_rec, usize_pair_to_u32,
};

use ms_canvas::OverlayRectPx;
use ms_memory::{MemoryBudget, MemoryProfile};
use ms_models::clean_overlays_model::CleanOverlaysModel;
use ms_models::layer_model::effects;
use ms_models::layer_model::manifest::TransformRec;
use ms_models::layer_model::ordering::{self, Band, CompositeItem, CompositeKey, GroupFold};
use ms_models::layer_model::persist;
use ms_models::layer_model::saver;
use ms_project::ProjectData;
use ms_config::app_tab::AppTab;
use ms_log::trace::cat;
use ms_widgets::panel_dock::{
    DockArea, DockEdge, DockLayout, HostId, PanelAnchor, PanelDock, PanelDockState, PanelId,
    PanelNode, TabExtras, TabId,
};
use correction::CorrectionState;
use edit_op::{GroupMetaValue, LayerFieldPatch, LifecycleDir, PsEditOp};
use eframe::egui;
use egui::{Color32, ColorImage, CornerRadius, Pos2, Rect, Sense, Stroke, Vec2};
use layer_render::TiledTexture;
use layers::{GroupId, Layer, LayerGroup, LayerId, LayerKind, LayerStack, LayerTransform};
use ms_actions::{ActionHistory, ApplyDirection, RasterDiff};
use ms_thread as thread;
use page_loader::{PageLoadRequest, PageLoaderHandles, spawn_page_loader_thread};
use selection::{Selection, SelectionBounds};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use text_layers::PsTextLayer;
use tools::brush::BrushTool;
use tools::deform::DeformTool;
use tools::patch::PatchTool;
use tools::select::{SelectMode, SelectTool};
use tools::transform::TransformTool;
use tools::{PsTool, PsToolContext, PsToolId, PsToolSection, ToolOutcome, TouchSelectionInput};

/// Max layer tiles uploaded to the GPU per frame across all layers (spreads big-page uploads).
const TILE_UPLOAD_BUDGET_PER_FRAME: usize = 8;

/// Max undo steps retained by the PS-editor per-page history (in addition to the byte budget).
const PS_EDITOR_UNDO_LIMIT: usize = 128;

/// Screen-space length (px) of one dash of the selection marquee. Black and white runs alternate
/// every `MARQUEE_DASH_PX` of arc length. Fixed in SCREEN space, so the pattern is zoom-invariant.
const MARQUEE_DASH_PX: f32 = 4.0;

/// Minimum screen-space distance (px) between two consecutive marquee path points. Boundary loops
/// trace the mask's pixel edges, so at zoom < 1 they carry sub-pixel staircase steps; dropping
/// those bounds the shape count by the loop's screen length rather than by the mask's complexity.
/// Below 1.0 so nothing is dropped at zoom >= 1, where the marquee must stay pixel-accurate.
const MARQUEE_MIN_STEP_PX: f32 = 0.75;

/// Screen-space length (px) below which a dash run (or a leftover dash phase) is treated as zero.
/// Guards against emitting invisible shapes and against zero-length steps in the arc-length walk.
const MARQUEE_MIN_RUN_PX: f32 = 1e-3;

/// Hard cap on the dash runs emitted for one selection-marquee frame, across all boundary loops.
/// Decimation already bounds the count in practice; this is the safety net that keeps a pathological
/// mask from stalling the GUI thread. Past the cap the remaining loops are simply not drawn.
const MARQUEE_MAX_RUNS: usize = 20_000;

/// Tile edge (px) used to partition PS-editor undo `RasterDiff`s. Matches the 1024px tiling used by
/// `layer_render::TiledTexture` and the clean-overlay history.
const PS_UNDO_TILE_SIDE: u32 = 1024;

/// Number of text characters shown in a text-layer row preview (`Текст (preview)`) in the layers
/// panel. Fixed budget (the typing tab makes this width-adaptive; a constant is enough here).
const PS_TEXT_PREVIEW_CHARS: usize = 16;

// ---------------------------------------------------------------------------------------------
// Dock tabs
//
// The seven `TabId` literals below are STABLE, NON-LOCALIZED PERSISTENCE IDENTITIES: they key the
// tab inside the `PanelLayout` section of `user_config.json` and inside `PanelDockState`'s
// per-program-tab maps, so renaming one silently drops the user's arrangement for that panel.
// They are therefore deliberately not routed through the `t!` catalog — a §A9 i18n exclusion
// (`dev-docs/i18n_exclusions.md`). The visible captions come from `ps_editor.tab.*`.
// ---------------------------------------------------------------------------------------------

/// «PS редактор»: page switch, zoom, load/effects status, and the «Панели…» visibility menu.
const PS_EDITOR_MAIN_TAB: TabId = TabId::new("ps_editor.main");
/// «Инструменты»: the tool selector plus the two whole-layer selection actions.
const PS_EDITOR_TOOLS_TAB: TabId = TabId::new("ps_editor.tools");
/// «Выбранный инструмент»: the active tool's `options_ui`, or the "no parameters" line.
const PS_EDITOR_ACTIVE_TOOL_TAB: TabId = TabId::new("ps_editor.active_tool");
/// «Горячие клавиши»: the active tool's `hotkey_rows` plus the tab-level shortcuts.
const PS_EDITOR_HOTKEYS_TAB: TabId = TabId::new("ps_editor.hotkeys");
/// «Слои»: the unified layer tree and the active-layer controls strip.
const PS_EDITOR_LAYERS_TAB: TabId = TabId::new("ps_editor.layers");
/// «Коррекция»: the view-only colour correction of the canvas (`correction/`).
const PS_EDITOR_CORRECTION_TAB: TabId = TabId::new("ps_editor.correction");
/// The ACTIVE TOOL's own dock panel (`PsTool::draw_main_panel`), shown only while that tool asks
/// for it (`PsTool::wants_main_panel`).
///
/// Unlike the five secondary panels it carries NO user visibility flag and is absent from the
/// «Панели…» menu: it belongs to a tool, not to the tab, so the tool alone decides whether it
/// exists this frame. Its caption is the tool's own `PsTool::title`, which is why no
/// `ps_editor.tab.*` key backs it.
const PS_EDITOR_TOOL_PANEL_TAB: TabId = TabId::new("ps_editor.tool_panel");

/// `TabExtras` flag key of the «Инструменты» panel's visibility. Stored on the MAIN tab.
const PS_EDITOR_FLAG_TOOLS_PANEL: &str = "panels.tools";
/// `TabExtras` flag key of the «Выбранный инструмент» panel's visibility.
const PS_EDITOR_FLAG_ACTIVE_TOOL_PANEL: &str = "panels.active_tool";
/// `TabExtras` flag key of the «Горячие клавиши» panel's visibility.
const PS_EDITOR_FLAG_HOTKEYS_PANEL: &str = "panels.hotkeys";
/// `TabExtras` flag key of the «Слои» panel's visibility.
const PS_EDITOR_FLAG_LAYERS_PANEL: &str = "panels.layers";
/// `TabExtras` flag key of the «Коррекция» panel's visibility.
const PS_EDITOR_FLAG_CORRECTION_PANEL: &str = "panels.correction";

/// Every panel starts shown; `TabExtras::set_flag` keeps a flag at its default out of the config.
const PS_EDITOR_PANEL_VISIBLE_DEFAULT: bool = true;

/// Shrink floor of «PS редактор» (points). Below this its TWO wrapped rows (page/zoom, then the
/// «Сглаживание» / «Сетка пикселей» toggles) stop being readable.
const PS_EDITOR_MAIN_TAB_MIN_SIZE_PX: Vec2 = Vec2::new(240.0, 74.0);
/// First-frame size of «PS редактор» (two wrapped rows), before its content has ever been measured.
const PS_EDITOR_MAIN_TAB_INITIAL_SIZE_PX: Vec2 = Vec2::new(420.0, 98.0);
/// Shrink floor of «Инструменты».
const PS_EDITOR_TOOLS_TAB_MIN_SIZE_PX: Vec2 = Vec2::new(180.0, 120.0);
/// First-frame size of «Инструменты».
const PS_EDITOR_TOOLS_TAB_INITIAL_SIZE_PX: Vec2 = Vec2::new(220.0, 320.0);
/// Shrink floor of «Выбранный инструмент».
const PS_EDITOR_ACTIVE_TOOL_TAB_MIN_SIZE_PX: Vec2 = Vec2::new(180.0, 60.0);
/// First-frame size of «Выбранный инструмент».
const PS_EDITOR_ACTIVE_TOOL_TAB_INITIAL_SIZE_PX: Vec2 = Vec2::new(220.0, 140.0);
/// Shrink floor of «Горячие клавиши».
const PS_EDITOR_HOTKEYS_TAB_MIN_SIZE_PX: Vec2 = Vec2::new(200.0, 80.0);
/// First-frame size of «Горячие клавиши».
const PS_EDITOR_HOTKEYS_TAB_INITIAL_SIZE_PX: Vec2 = Vec2::new(300.0, 220.0);
/// Shrink floor of «Слои».
const PS_EDITOR_LAYERS_TAB_MIN_SIZE_PX: Vec2 = Vec2::new(220.0, 140.0);
/// First-frame size of «Слои».
const PS_EDITOR_LAYERS_TAB_INITIAL_SIZE_PX: Vec2 = Vec2::new(280.0, 380.0);
/// Shrink floor of «Коррекция»: the combo plus the widest of its parameter cards.
const PS_EDITOR_CORRECTION_TAB_MIN_SIZE_PX: Vec2 = Vec2::new(200.0, 80.0);
/// First-frame size of «Коррекция» (the combo and the two-slider card).
const PS_EDITOR_CORRECTION_TAB_INITIAL_SIZE_PX: Vec2 = Vec2::new(280.0, 140.0);
/// Shrink floor of the active tool's own panel. The tool sizes its content; this only keeps a
/// button row readable.
const PS_EDITOR_TOOL_PANEL_TAB_MIN_SIZE_PX: Vec2 = Vec2::new(200.0, 80.0);
/// First-frame size of the active tool's own panel, before its content has ever been measured.
const PS_EDITOR_TOOL_PANEL_TAB_INITIAL_SIZE_PX: Vec2 = Vec2::new(300.0, 200.0);

/// Which of the five secondary panels the «Панели…» menu currently shows.
///
/// Read straight off the dock state BEFORE the tabs are declared, so the `visible(..)` a tab is
/// declared with is already the value the user last chose — reading it from a body would show the
/// panel for one frame before hiding it. The MAIN tab's body writes the values back through its
/// `TabExtras`, which is what raises `changed` → `dirty` → the persistence write.
///
/// «PS редактор» itself is deliberately absent: the menu lives in it, so a flag hiding it could never
/// be flipped back.
#[derive(Debug, Clone, Copy)]
struct PsEditorPanelVisibility {
    tools: bool,
    active_tool: bool,
    hotkeys: bool,
    layers: bool,
    correction: bool,
}

impl PsEditorPanelVisibility {
    /// Reads the five flags out of the MAIN tab's stored extras, defaulting to "shown".
    ///
    /// A tab that never stored anything yields no bag at all, hence the `map_or` over the whole
    /// lookup rather than per flag.
    #[must_use]
    fn read(dock: &PanelDockState, layout_key: &str) -> Self {
        let Some(extras) = dock.tab_extras(layout_key, PS_EDITOR_MAIN_TAB) else {
            return Self {
                tools: PS_EDITOR_PANEL_VISIBLE_DEFAULT,
                active_tool: PS_EDITOR_PANEL_VISIBLE_DEFAULT,
                hotkeys: PS_EDITOR_PANEL_VISIBLE_DEFAULT,
                layers: PS_EDITOR_PANEL_VISIBLE_DEFAULT,
                correction: PS_EDITOR_PANEL_VISIBLE_DEFAULT,
            };
        };
        Self {
            tools: extras.flag(PS_EDITOR_FLAG_TOOLS_PANEL, PS_EDITOR_PANEL_VISIBLE_DEFAULT),
            active_tool: extras.flag(
                PS_EDITOR_FLAG_ACTIVE_TOOL_PANEL,
                PS_EDITOR_PANEL_VISIBLE_DEFAULT,
            ),
            hotkeys: extras.flag(
                PS_EDITOR_FLAG_HOTKEYS_PANEL,
                PS_EDITOR_PANEL_VISIBLE_DEFAULT,
            ),
            layers: extras.flag(PS_EDITOR_FLAG_LAYERS_PANEL, PS_EDITOR_PANEL_VISIBLE_DEFAULT),
            correction: extras.flag(
                PS_EDITOR_FLAG_CORRECTION_PANEL,
                PS_EDITOR_PANEL_VISIBLE_DEFAULT,
            ),
        }
    }

    /// The five toggles as `(flag key, localized title, value)`, in menu order.
    ///
    /// One source for both the menu rows and the write-back below, so a panel can never appear in
    /// the menu under a key that is not the one persisted for it.
    fn entries(&mut self) -> [(&'static str, &'static str, &mut bool); 5] {
        [
            (
                PS_EDITOR_FLAG_TOOLS_PANEL,
                t!("ps_editor.tab.tools"),
                &mut self.tools,
            ),
            (
                PS_EDITOR_FLAG_ACTIVE_TOOL_PANEL,
                t!("ps_editor.tab.active_tool"),
                &mut self.active_tool,
            ),
            (
                PS_EDITOR_FLAG_HOTKEYS_PANEL,
                t!("ps_editor.tab.hotkeys"),
                &mut self.hotkeys,
            ),
            (
                PS_EDITOR_FLAG_LAYERS_PANEL,
                t!("ps_editor.tab.layers"),
                &mut self.layers,
            ),
            (
                PS_EDITOR_FLAG_CORRECTION_PANEL,
                t!("ps_editor.tab.correction"),
                &mut self.correction,
            ),
        ]
    }
}

/// Per-frame context the panel dock hands to one «PS редактор» tab body at a time.
///
/// Every body of this tab needs the tab state itself — the tool list and `set_active_tool` for
/// «Инструменты», the layer tree for «Слои», `request_page` and the viewport for «PS редактор» — so the
/// context lends `&mut PsEditorTabState` as a whole rather than a set of disjoint field borrows.
/// That is sound by construction here: `PanelDockState` is LENT IN to `PsEditorTabState::draw` by
/// the application, so the dock's own frame-long borrow is provably disjoint from the tab state
/// (`panel_dock/MODULE_README.md`, «The dock state must be its own borrow»).
///
/// Bodies still defer the expensive layer mutations: `panel_actions` is the tab's existing
/// [`PanelActions`] request struct, drained and applied by `apply_panel_actions` after
/// `PanelDock::end` has returned, exactly as the static right panel used to apply it after its
/// panel closure.
struct PsEditorDockCx<'a> {
    /// The tab state every body draws from and (for the cheap, already-per-frame writes) mutates.
    tab: &'a mut PsEditorTabState,
    /// The open project: page list for «PS редактор», persistence target for the deferred actions.
    project: &'a ProjectData,
    /// Layer-panel actions collected by «Слои», applied after the dock frame.
    panel_actions: PanelActions,
    /// Panel visibility as the frame started; «PS редактор» edits it and writes it into its extras.
    visibility: PsEditorPanelVisibility,
}

/// State of the PS-like editor tab.
pub struct PsEditorTabState {
    overlays_model: Option<Arc<Mutex<CleanOverlaysModel>>>,
    loader: Option<PageLoaderHandles>,
    /// Gated legacy `text_images/` dir for the shared-doc decode, cached per chapter to keep the
    /// GUI thread off a per-page-switch `layers.json` parse: `(committed layers dir, gated legacy dir)`.
    /// The gated value is `Some(text_images_dir)` for an un-migrated chapter, else `None`.
    doc_legacy_text_dir_cache: Option<(std::path::PathBuf, Option<std::path::PathBuf>)>,
    viewport: viewport::PsViewport,
    stack: Option<LayerStack>,
    selection: Option<Selection>,
    /// Per-layer tiled GPU caches, keyed by layer id.
    render_cache: HashMap<LayerId, TiledTexture>,
    /// Layers the current selection overlaps, computed when the selection context menu opens
    /// (bottom-to-top order). Drives the copy/cut "from layer(s)" picker.
    clip_touched_layers: Vec<LayerId>,
    /// Layers ticked in the "Из слоя/слоёв…" multi-select picker (reset on each menu open).
    clip_selected_layers: HashSet<LayerId>,
    tools: Vec<Box<dyn PsTool>>,
    active_tool_idx: usize,
    /// Page currently shown or being loaded.
    active_page_idx: Option<usize>,
    requested_page_idx: Option<usize>,
    pending_job_id: Option<u64>,
    next_job_id: u64,
    load_error: Option<String>,
    /// Raster node uids the PS editor deleted/merged-away this session, per page, that are not yet
    /// persisted. `save_page_rasters` preserves manifest rasters the stack does not own (added by
    /// the typing tab), so a deletion must be explicit; the merge skips these so a revision bump
    /// does not resurrect a just-deleted raster. Cleared for a page once its save drops them.
    deleted_raster_uids: HashMap<usize, HashSet<String>>,
    /// True when the current page has received an edit needing persistence that has NOT yet been
    /// enqueued/flushed. Per-edit flushes already enqueue (clearing this), so the tab-switch
    /// `flush_layers` only needs to run when this is set. Conservative: set on any page-mutating edit;
    /// cleared after a `route_to_doc` enqueue or a `persist_current_page` flush.
    layers_dirty: bool,
    /// Read-only display of the typing tab's overlays for the current page (text/image overlays
    /// mirrored as text nodes in `layers.json`). Rebuilt on each page load.
    text_layers: Vec<PsTextLayer>,
    /// Unified Z order (bands) for the current page, from `layers.json`. Drives compositing order
    /// and the unified layer panel. Rebuilt on each page load / reload.
    bands: Vec<Band>,
    /// Multi-selected rows in the unified layers panel (for batch group ops). Cleared on page change.
    /// NEVER contains a `RowSel::Base`: base layers are structurally locked and take no part in
    /// group ops (`select_row` / `selectable_row_order` keep them out).
    panel_selection: HashSet<RowSel>,
    /// Anchor row for Shift-range selection in the panel.
    panel_anchor: Option<RowSel>,
    /// The row whose controls the "active layer" strip shows and which the panel draws highlighted
    /// (last plain/ctrl click, or the seeded active layer).
    ///
    /// INVARIANT: while a page is loaded (`stack.is_some()`) this is always `Some` and always names
    /// a row that exists in the current tree. [`PsEditorTabState::normalize_panel_primary`] is the
    /// single enforcer — it runs after every projection that can remove a row and after a page load,
    /// re-pointing a `None`/dangling primary at `LayerStack::active_id()` (which is `Клин` on a
    /// fresh page). It is `None` only before the first page finishes loading.
    panel_primary: Option<RowSel>,
    /// Open destructive effects editor: the target raster layer and its effects-JSON text.
    effects_editor: Option<(LayerId, String)>,
    /// Index into `text_layers` currently being dragged with the Transform tool.
    dragging_text_layer: Option<usize>,
    /// How the active text-layer drag transforms the overlay (set from modifiers at press).
    text_drag_mode: TextDragMode,
    /// Last pointer position (page px) during a translate drag.
    text_drag_last: Vec2,
    /// Reference for a rotate (last angle, rad) or scale (last distance, px) drag.
    text_drag_ref: f32,
    /// `(page, LayerDoc::page_revision)` of this tab's last projection from the doc. Each frame, if
    /// the active page or its live page revision differs, the tab re-projects the active page from
    /// the shared doc — the in-memory cross-tab sync. Keyed by page so an edit of ANOTHER page (the
    /// typing preload, a typing edit elsewhere) never re-projects the active one. `None` before the
    /// first projection.
    last_projected_revision: Option<(usize, u64)>,
    /// Trace-only: number of composite steps emitted on the previous frame. Used to gate the
    /// per-frame `draw_composite` detail log so it only fires when the plan size changes (the
    /// composite is rebuilt every frame, so unconditional logging would flood the trace at 60/s).
    trace_last_composite_steps: usize,
    /// Shared unified layer document (app-owned): the source of truth for per-page layer MODEL state,
    /// shared with the typing tab. `None` until `set_layer_doc` is called by studio.rs.
    layer_doc:
        Option<std::sync::Arc<std::sync::Mutex<ms_models::layer_model::layer_doc::LayerDoc>>>,
    /// Set by the "100%" button; consumed in `draw_canvas` where the real canvas rect is known.
    pending_actual_size: bool,
    /// «Сглаживание»: bilinear (`true`, the default and the historic behaviour) vs NEAREST sampling
    /// of EVERY PS-editor texture — layer tiles (`TiledTexture`) and typing text overlays alike, so
    /// a magnified page reads consistently. A flip re-uploads the affected textures; see
    /// `TiledTexture::set_options` for why re-upload beats a second handle per tile.
    ///
    /// Deliberately NOT persisted: it is a per-session viewing aid, and nothing else in this tab's
    /// UI state survives a restart except the dock arrangement and its panel-visibility flags.
    smoothing_enabled: bool,
    /// «Сетка пикселей»: paint the shared per-source-pixel grid over the page (`false` by default).
    ///
    /// The checkbox is the user's INTENT; the grid additionally stays gated on
    /// `canvas::pixel_inspection_recommended_for`, which is a cost and legibility guard rather than
    /// a second opinion — below ~4 device pixels per source pixel the grid degenerates into a solid
    /// grey field and costs tens of thousands of segments per frame. The checkbox therefore stays
    /// enabled at every zoom and simply shows nothing until the zoom is sufficient (its tooltip says
    /// so). Not persisted, for the same reason as `smoothing_enabled`.
    pixel_grid_enabled: bool,
    /// «Коррекция»: the VIEW-ONLY colour correction the canvas is drawn through (`correction/`).
    ///
    /// Purely a property of the picture on screen — it never reaches layer pixels, the shared doc,
    /// `layers.json` or `CleanOverlaysModel`. Deliberately NOT persisted, for the same reason as
    /// `smoothing_enabled` and `pixel_grid_enabled`: it is a per-session viewing aid.
    correction: CorrectionState,
    /// Whether the correction's shader layer can render, with the once-per-session latch that
    /// writes an unavailability to `runtime_log` (the library logs only through `log`).
    correction_availability: correction::CorrectionAvailability,
    /// Camera synced in from `CanvasView`, applied once its target page is loaded so the async
    /// page load (which refits the camera) does not clobber it. See `sync_view_from_canvas`.
    pending_camera: Option<CameraSync>,
    /// `CleanOverlaysModel::revision` observed at the last base-layer load, used to detect
    /// external clean-overlay edits (e.g. from the Cleaning tab) and refresh the `Клин` layer.
    last_overlay_revision: u64,
    /// Per-node GPU-cache generation tracking, keyed by `(page_idx, node uid)`. `sync_view_from_doc`
    /// preserves a raster's `render_cache` / a text's texture handle when the doc node's `generation`
    /// is unchanged, and invalidates it (forcing a re-upload) when it changed. Mirrors the typing
    /// tab's `raster_texture_generations`.
    node_generations: HashMap<(usize, String), u64>,
    /// Lazily-cached page pixel sizes `[w, h]` keyed by page index (header-only `image_dimensions`),
    /// so the full chapter map can be handed to the shared doc for the legacy ribbon migration without
    /// re-reading every page image on each page load.
    page_sizes_px: HashMap<usize, [usize; 2]>,
    /// In-flight non-destructive raster-effects render (the expensive `apply_effects_to_color_image`
    /// runs on a worker thread, never the GUI thread). `poll_ps_raster_effects_jobs` consumes the
    /// result. Mirrors the typing tab's `raster_effects_state`.
    raster_effects_state: Option<Receiver<Result<PsRasterEffectsResult, String>>>,
    /// A raster-effects edit that arrived while a render was already in flight. Only the latest is
    /// kept (newer edits supersede); it is re-dispatched when the current render completes so the last
    /// requested effects are never silently dropped (e.g. effecting a second raster right after a
    /// first). Mirrors the typing tab's `pending_raster_effects`.
    pending_raster_effects: Option<PendingPsRasterEffects>,
    /// Per-page undo/redo engine (Phase 3a). Each entry is a reversible tiled+zstd raster delta. The
    /// history is PER-PAGE-SESSION: it is cleared on every page switch (`request_page`) because a diff
    /// is only valid while its page's layer image buffers are resident. Bounded by
    /// `PS_EDITOR_UNDO_LIMIT` steps and a per-memory-profile COMPRESSED byte budget.
    history: ActionHistory<PsEditOp>,
    /// Accumulated union of the active brush stroke's per-segment dirty rects (layer-local px,
    /// inclusive). Reset on the stroke's press frame and consumed at release to build a region-bounded
    /// undo diff. `None` when no stroke is in progress or nothing was painted.
    brush_stroke_dirty: Option<tools::DirtyRect>,
    /// Active opacity-slider gesture: `(raster LayerId, opacity BEFORE the drag)`. Set on the first
    /// slider change of a drag, consumed one undo entry per completed gesture (the first idle frame
    /// with no further opacity change), so a drag records a single reversible step, not one per tick.
    opacity_gesture: Option<(LayerId, f32)>,
    /// Active GROUP opacity-slider gesture: `(page, group uid, group meta BEFORE the drag)`. Same
    /// one-undo-step-per-drag contract as `opacity_gesture`, closed by
    /// `finish_group_opacity_gesture` on the first panel frame without a `GroupOp::GroupOpacity`.
    group_opacity_gesture: Option<(usize, String, GroupMetaValue)>,
    /// Page whose TEXT payload an undo/redo step changed in the doc (`apply_ps_text_visibility`);
    /// `finish_history_step` enqueues that page's text save, which `persist_current_page` (rasters
    /// and groups only) does not cover. Taken (cleared) by every finished history step.
    history_text_flush_page: Option<usize>,
    /// Transform-tool gesture start snapshot: `(raster uid, transform BEFORE the gesture)`. Captured on
    /// the press frame and consumed at release to record ONE `FieldPatch::Transform` per gesture.
    transform_gesture_before: Option<(String, LayerTransform)>,
    /// Deform-tool gesture start snapshot: `(raster uid, deform BEFORE the gesture)`. Captured on the
    /// press frame and consumed at release to record ONE `FieldPatch::Deform` per gesture.
    deform_gesture_before: Option<(
        String,
        Option<ms_models::layer_model::manifest::DeformRec>,
    )>,
    /// Screen rects of THIS frame's drawn dock panels (main window only), refilled from
    /// `PanelDockOutput::drawn_panels` right after `PanelDock::end`. The canvas is drawn afterwards
    /// and gates its pointer input on them (`canvas_pointer_occluded`), so a click on a floating
    /// panel never reaches the active tool.
    panel_rects: Vec<Rect>,
}

/// Worker result for a non-destructive raster effects render (mirrors the typing tab's
/// `TypingRasterEffectsResult`): the rendered display image plus the pixel `origin` of the original
/// base content inside it, used by the GUI-side recenter math. The base PNG is never touched, so the
/// chain stays reversible.
#[derive(Debug)]
struct PsRasterEffectsResult {
    page_idx: usize,
    /// Stable doc uid of the effected raster.
    uid: String,
    /// Session `LayerId` of the effected raster (used to drop its `render_cache` entry).
    id: LayerId,
    /// The post-effects render to display.
    new_image: ColorImage,
    /// Pixel offset of the original (pre-effects) base content's top-left inside `new_image`
    /// (effects like shadow/glow grow the canvas), feeding the recenter anchoring. Matches the
    /// `[i32; 2]` content origin returned by `apply_effects_to_color_image`.
    origin: [i32; 2],
    /// Size `[w, h]` of the pre-effects base image the render started from (recenter reference).
    base_size: [usize; 2],
    /// The pre-effects base transform the render started from (recenter reference).
    base_t: LayerTransform,
    /// The parsed effects chain that produced `new_image`. Empty means "clear effects".
    effects: Vec<serde_json::Value>,
}

/// Inputs needed to re-dispatch a stashed raster-effects request once the in-flight render finishes
/// (latest-wins): `(layer id, effects-JSON text)`. The page is implicit (the active page when the
/// poll re-dispatches), matching the typing tab's `pending_raster_effects` stash.
type PendingPsRasterEffects = (LayerId, String);

/// A camera handed in from `CanvasView`, pending until `page_idx` finishes loading.
///
/// `center_world` of `None` means "center the page" and is resolved to the page center at apply
/// time, when the loaded page size is known.
#[derive(Debug, Clone, Copy)]
struct CameraSync {
    page_idx: usize,
    zoom: f32,
    center_world: Option<Vec2>,
}

/// How a text-layer drag in the PS editor transforms the overlay (chosen by modifier at press:
/// Shift = rotate, Ctrl/Cmd = scale, otherwise translate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextDragMode {
    Translate,
    Rotate,
    Scale,
}

/// A selectable row in the unified layers panel. Texts and groups key on their stable uid (survives
/// reloads); rasters and base layers key on the session `LayerId` (matches `LayerStack::active`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RowSel {
    /// One of the two STRUCTURALLY LOCKED base layers (`Исходник` / `Клин`). It exists only so the
    /// panel can show the active layer — the editor always has one and it defaults to `Клин`. It is
    /// never allowed into `panel_selection`, a Shift range, a group op or a band move; see
    /// [`RowSel::is_base`] for the list of guards that enforce that.
    Base(LayerId),
    Raster(LayerId),
    Text(String),
    Group(String),
}

impl RowSel {
    /// Whether this row is a structurally locked base layer.
    ///
    /// A base row may be the panel's PRIMARY row (it is what makes the active layer visible), but it
    /// must never take part in anything structural: multi-selection, Shift ranges, grouping,
    /// deletion, merging, baking or reordering. Every such consumer asks this instead of relying on
    /// the older "a base leaf carries no `RowSel`" lock.
    fn is_base(&self) -> bool {
        matches!(self, RowSel::Base(_))
    }
}

/// A batch grouping operation requested from the panel's right-click menu, resolved into a
/// `persist::GroupingEdit` (+ in-memory stack mirror) after the panel closure ends.
#[derive(Debug, Clone)]
enum GroupOp {
    /// Create a new group from the current panel selection.
    NewFromSelection,
    /// Move the current selection into the existing group with this uid.
    MoveTo(String),
    /// Remove the current selection from whatever group(s) it is in.
    Ungroup,
    /// Delete the group with this uid (members are ungrouped).
    DeleteGroup(String),
    /// Toggle a group's collapse state.
    ToggleCollapse(String),
    /// Toggle a group's visibility.
    ToggleGroupVisible(String),
    /// Set a group's opacity.
    GroupOpacity(String, f32),
    /// Move a group's whole contiguous block one step up (`true`) or down (`false`) in Z.
    MoveGroup(String, bool),
}

/// Deferred actions collected while drawing the layers panel, applied after the panel closure ends
/// (they need `&mut self` / `project` that the panel's tree snapshot has borrowed immutably).
#[derive(Default)]
struct PanelActions {
    add_layer: bool,
    new_empty_group: bool,
    set_active_raster: Option<LayerId>,
    toggle_visible_raster: Option<LayerId>,
    toggle_visible_text: Option<usize>,
    opacity_raster: Option<(LayerId, f32)>,
    move_band: Option<(RowSel, bool)>,
    remove_raster: Option<LayerId>,
    merge_req: Option<LayerId>,
    bake_req: Option<LayerId>,
    open_effects: Option<LayerId>,
    text_op: Option<(usize, TextLayerOp)>,
    group_op: Option<GroupOp>,
    /// Set on a primary layer-row click: after the active layer/primary is updated, set the canvas
    /// marquee to that layer's footprint (`select_active_layer_fully`).
    request_select_active: bool,
}

/// An owned, render-ready snapshot of one layer-panel row, built from the tree + stack + text
/// layers *before* the render loop so the loop can mutate `self.panel_selection` without holding an
/// immutable borrow of `self`.
enum PanelRow {
    Group(tree::GroupHeader),
    Leaf(PanelLeaf),
}

struct PanelLeaf {
    /// Selection key. `None` only when the row has no identity to key on — a text leaf whose
    /// `PsTextLayer` runtime is missing. Base layers DO carry one (`RowSel::Base`) so they can be
    /// the primary row; their structural lock is enforced by [`RowSel::is_base`] at every consumer,
    /// not by the absence of a key.
    sel: Option<RowSel>,
    kind: tree::LeafKind,
    depth: u8,
    name: String,
    visible: bool,
    is_base: bool,
}

/// The read-only, per-frame context every layers-panel ROW draw needs, snapshotted before the
/// render loop (which mutates `self.panel_selection` / `self.panel_primary`) so no borrow of `self`
/// is held across it.
struct PanelRowCx<'a> {
    /// The current multi-selection (batch group ops).
    selection: &'a HashSet<RowSel>,
    /// The primary row: highlighted, and the subject of the controls strip.
    primary: Option<&'a RowSel>,
    /// The group uid the primary row belongs to, so a COLLAPSED header can borrow its highlight.
    /// `None` when the primary is ungrouped, is a group header itself, or is a base layer.
    primary_group_uid: Option<&'a str>,
    /// The displayed rows a Shift range may span (base rows excluded, see `selectable_row_order`).
    row_sels: &'a [RowSel],
    /// Existing groups as `(uid, name)`, for the "move to group" submenu.
    group_list: &'a [(String, String)],
}

/// A deferred action on a typing text layer, triggered from the PS layers panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextLayerOp {
    /// Bake the overlay's pixels into an owned raster layer and remove the overlay.
    Rasterize,
}

/// Whether a selection clip operation copies the chosen layers or also clears them (cut).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipMode {
    Copy,
    Cut,
}

impl ClipMode {
    fn verb(self) -> &'static str {
        match self {
            ClipMode::Copy => t!("ps_editor.clip.copy_verb"),
            ClipMode::Cut => t!("ps_editor.clip.cut_verb"),
        }
    }
}

impl Default for PsEditorTabState {
    fn default() -> Self {
        let tools: Vec<Box<dyn PsTool>> = vec![
            Box::new(SelectTool::new(SelectMode::Rect)),
            Box::new(SelectTool::new(SelectMode::Lasso)),
            Box::new(BrushTool::default()),
            Box::new(TransformTool::default()),
            Box::new(DeformTool::default()),
            // Appended LAST on purpose: `active_tool_idx` below is a position in this vector, and
            // inserting anywhere earlier would silently change which tool starts selected.
            Box::new(PatchTool::default()),
        ];
        Self {
            overlays_model: None,
            loader: None,
            doc_legacy_text_dir_cache: None,
            viewport: viewport::PsViewport::default(),
            stack: None,
            selection: None,
            render_cache: HashMap::new(),
            clip_touched_layers: Vec::new(),
            clip_selected_layers: HashSet::new(),
            tools,
            active_tool_idx: 2,
            active_page_idx: None,
            requested_page_idx: None,
            pending_job_id: None,
            next_job_id: 1,
            load_error: None,
            deleted_raster_uids: HashMap::new(),
            layers_dirty: false,
            text_layers: Vec::new(),
            bands: Vec::new(),
            panel_selection: HashSet::new(),
            panel_anchor: None,
            panel_primary: None,
            effects_editor: None,
            dragging_text_layer: None,
            text_drag_mode: TextDragMode::Translate,
            text_drag_last: Vec2::ZERO,
            text_drag_ref: 0.0,
            last_projected_revision: None,
            trace_last_composite_steps: usize::MAX,
            layer_doc: None,
            pending_actual_size: false,
            smoothing_enabled: true,
            pixel_grid_enabled: false,
            correction: CorrectionState::default(),
            correction_availability: correction::CorrectionAvailability::default(),
            pending_camera: None,
            last_overlay_revision: 0,
            node_generations: HashMap::new(),
            page_sizes_px: HashMap::new(),
            raster_effects_state: None,
            pending_raster_effects: None,
            // Start with the count cap and a default (Medium-profile) byte budget. The PS editor has
            // no live `MemoryProfile` handle wired in Part A, so this fixed Medium cap stands in for a
            // profile-driven budget; wiring `set_memory_profile` through the tab is a follow-up.
            history: ActionHistory::with_weight_budget(
                PS_EDITOR_UNDO_LIMIT,
                MemoryBudget::for_profile(MemoryProfile::default()).ps_editor_undo_bytes_usize(),
            ),
            brush_stroke_dirty: None,
            opacity_gesture: None,
            group_opacity_gesture: None,
            history_text_flush_page: None,
            transform_gesture_before: None,
            deform_gesture_before: None,
            panel_rects: Vec::new(),
        }
    }
}

/// One drawable item of the PS canvas composite above the base layers, in the order
/// [`composite_steps`] returns. `opacity` is the effective opacity to draw with.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PsStep {
    /// A user raster layer of the stack; `opacity` already includes its group's factor.
    Raster { id: LayerId, opacity: f32 },
    /// The text overlay at `index` in `PsEditorTabState::text_layers`; `opacity` is its unified
    /// group's opacity (1.0 outside a group).
    Text { index: usize, opacity: f32 },
}

/// The composite-relevant facts of one USER raster layer (base layers are excluded): the layer's
/// OWN visibility and opacity plus its unified group uid. The group fold is NOT pre-applied: the
/// shared owner (`ordering::composite_plan`) applies it, exactly as for texts.
#[derive(Debug, Clone, PartialEq)]
struct PsRasterView {
    id: LayerId,
    /// Stable uid string, matched against `Band::Raster::uid`.
    uid: String,
    /// Unified PS group uid (`LayerGroup::uid` as a string); `None` outside a group or when the
    /// layer names a group id the stack no longer has (folds as visible / 1.0, like an unknown uid).
    group_uid: Option<String>,
    visible: bool,
    opacity: f32,
}

impl PsRasterView {
    /// Views of every non-base layer of `stack`, bottom-to-top in stack order.
    fn collect(stack: &LayerStack) -> Vec<Self> {
        stack
            .layers()
            .iter()
            .filter(|layer| !layer.kind.is_base())
            .map(|layer| Self {
                id: layer.id,
                uid: layer.uid.to_string(),
                group_uid: layer
                    .group
                    .and_then(|g| stack.group(g))
                    .map(|g| g.uid.to_string()),
                visible: layer.visible,
                opacity: layer.opacity,
            })
            .collect()
    }
}

/// The composite-relevant facts of one text overlay, borrowed from its `PsTextLayer`. A PS text
/// overlay has no own opacity (it enters the plan at 1.0); its unified-group fold is applied by the
/// owner from `groups`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PsTextView<'a> {
    /// Index into `PsEditorTabState::text_layers`.
    index: usize,
    uid: &'a str,
    /// Legacy text group (`Band::TextGroup::layer_idx`): the band-lookup fallback when no
    /// `PinnedText` band names `uid`.
    layer_idx: u32,
    visible: bool,
    /// Unified PS group uid (`LayerGroup::uid` as a string), if any.
    group_uid: Option<&'a str>,
}

impl<'a> PsTextView<'a> {
    /// The view of `layer`, which sits at `index` in `PsEditorTabState::text_layers`.
    fn of(index: usize, layer: &'a PsTextLayer) -> Self {
        Self {
            index,
            uid: &layer.uid,
            layer_idx: layer.layer_idx,
            visible: layer.visible,
            group_uid: layer.group_uid.as_deref(),
        }
    }
}

/// Snapshot of pointer/keyboard input read once per frame for the canvas.
struct CanvasInput {
    hover_pos: Option<Pos2>,
    primary_down: bool,
    primary_pressed: bool,
    primary_released: bool,
    /// Right button held this frame — the brush's Alt + right-drag HUD gesture.
    secondary_down: bool,
    middle_down: bool,
    space_down: bool,
    pointer_delta: Vec2,
    scroll_y: f32,
    modifiers: egui::Modifiers,
    /// Escape pressed this frame, already gated on "no widget holds keyboard focus".
    cancel_pressed: bool,
    /// Backspace or Delete pressed this frame, gated the same way as `cancel_pressed`.
    remove_point_pressed: bool,
}

/// One brush parameter change requested by a key press this frame.
///
/// Produced by [`collect_brush_key_actions`] and applied by
/// [`PsEditorTabState::brush_shortcuts`]; splitting the two keeps the key decoding pure and
/// unit-testable without an `egui::Context`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum BrushKeyAction {
    /// `[` / `]`: one step on Photoshop's non-linear diameter table.
    DiameterStep { up: bool },
    /// `-` / `=` / `+`: the legacy multiplicative size change.
    DiameterScale { down: bool },
    /// `Shift+[` / `Shift+]`: one 25 % step of hardness.
    HardnessStep { up: bool },
    /// A digit: opacity as a fraction of 1.0.
    Opacity(f32),
    /// `Shift` + a digit: flow as a fraction of 1.0.
    Flow(f32),
}

/// One layer of an ordered composite: the layer itself and the opacity it contributes at.
///
/// The opacity travels WITH the layer because it is not a property of the layer alone — group
/// opacity multiplies into it (`LayerStack::layer_opacity`) — and a caller that resolved the
/// order already resolved the opacity in the same walk.
type CompositeLayer<'a> = (&'a Layer, f32);

/// Which part of a stack one composite covers, relative to a stack index.
///
/// The two forms are ONE index apart and mean opposite things, so they are named rather than
/// written as an off-by-one at the call site: a patch is solved in the plane its target layer is
/// PART of ([`CompositeBound::UpTo`]) and stored over the plane BELOW it
/// ([`CompositeBound::Below`]), and confusing the two applies every layer above the target twice —
/// once because the solver copied it into the target, once because the render paints it again.
///
/// The whole visible stack is [`CompositeBound::UpTo`] the topmost index; no production caller
/// composites a page rectangle that way, so it has no variant of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompositeBound {
    /// Visible layers STRICTLY BELOW the index — the backdrop a pixel written into the layer at
    /// that index is composited over.
    Below(usize),
    /// Visible layers up to and INCLUDING the index — the plane the layer at that index is part
    /// of, i.e. what the page shows when everything above it is stripped away.
    UpTo(usize),
}

impl CompositeBound {
    /// Whether the layer at stack index `idx` is inside this bound.
    ///
    /// Monotone in `idx` — once it answers `false` it answers `false` for every deeper index — which
    /// is what makes the `take_while` in [`visible_layers_bottom_to_top`] an exact filter rather
    /// than a truncation.
    fn includes(self, idx: usize) -> bool {
        match self {
            Self::Below(limit) => idx < limit,
            Self::UpTo(limit) => idx <= limit,
        }
    }
}

/// One non-destructive raster effects render, resolved on the GUI thread (the base already cloned,
/// every lock dropped) and moved whole into the worker thread that calls [`PsRasterEffectsJob::run`].
#[derive(Debug)]
struct PsRasterEffectsJob {
    /// Page the raster lives on (echoed into the result).
    page_idx: usize,
    /// Stable doc uid of the raster.
    uid: String,
    /// Session `LayerId` of the raster.
    id: LayerId,
    /// The pre-effects image the chain is rendered from.
    base_image: ColorImage,
    /// Size `[w, h]` of `base_image` (recenter reference).
    base_size: [usize; 2],
    /// The raster's transform at dispatch time (recenter reference).
    base_t: LayerTransform,
    /// The effects chain as the editor's JSON text, fed to the renderer.
    json: String,
    /// The same chain parsed; empty means "clear effects".
    effects: Vec<serde_json::Value>,
}

/// One emitted piece of the selection marquee: a straight screen-space run of a single colour.
///
/// Runs tile the path end to end with no gaps; `black` alternates every `dash_len` of ARC LENGTH,
/// so the black/white contrast is what makes the border read on any background.
#[derive(Debug, Clone, Copy)]
struct DashRun {
    from: Pos2,
    to: Pos2,
    /// `true` for a black run, `false` for a white one.
    black: bool,
}

#[cfg(test)]
mod tests;
