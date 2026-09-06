/*
File: tabs/ps_editor/mod.rs

Purpose:
Orchestrates the "PS-подобный редактор" tab: a standalone, single-page, layered editor that is
deliberately NOT a `CanvasView`. It owns its own pan/zoom viewport, layer stack, selection, tool
set, and tiled GPU cache.

Key structures:
- `PsEditorTabState`: the tab state held by `MangaApp`.
- `PsEditorDockCx`: the per-frame context the panel dock lends to one tab body at a time.
- `PsEditorPanelVisibility`: the four "is this panel shown" flags, stored in the MAIN tab's
  `TabExtras` bag and toggled from its «Панели…» menu.

Architecture:
- UI shape: the canvas is the full-area BACKGROUND and every surface over it is a dock tab
  (`crate::widgets::panel_dock`) — «PS редактор», «Инструменты», «Выбранный инструмент»,
  «Горячие клавиши», «Слои». `ps_editor_default_dock_layout` is their default arrangement, and
  the tab supplies its OWN `DockArea` rect (it is not a canvas tab, so `canvas::dock_area_rect`
  — which reserves the canvas' scrollbar strip — does not apply).
- `viewport`: own camera (pan/zoom/fit), independent of the shared canvas engine.
- `layers`: ordered layer stack with two locked base layers (source + clean) and user raster
  layers above them.
- `page_loader`: background worker that produces the two base-layer images for the active page.
- `layer_render`: per-layer tiled texture cache (budgeted upload, dirty tiles).
- `tools`: `PsTool` trait + selection/brush tools; the tab routes pointer input to the active tool.
  Tool-gesture lifecycle is the TAB's job: `set_active_tool` (the only writer of `active_tool_idx`)
  resets the outgoing tool, `request_page` resets the active one, and a frame routed to the pan /
  text drag instead calls `PsTool::freeze` (or `reset` on Esc) so a multi-frame outline neither
  outlives its page nor dies on a suppressed frame. The tool's cursor preview obeys the same
  occlusion gate as its input: `overlay_pointer` hides it under a floating panel unless
  `PsTool::gesture_in_flight` says a gesture started on bare canvas is still running.
- selection storage: `set_selection` / `non_empty_selection` keep the page selection `Some` only
  while it selects at least one pixel — an all-zero mask draws no marquee yet still clips the brush.
- selection marquee: `draw_selection_marquee` paints the boundary loops of `Selection` as a static
  black/white dashed border. The loops trace the mask's PIXEL EDGES and can carry many thousands of
  one-pixel staircase steps, so the screen path is first decimated (`decimate_to_screen`) and then
  dashed by CUMULATIVE arc length (`walk_dash_runs`) — dashing each segment on its own would restart
  the pattern every pixel and paint a solid line.
- raster effects: applying a non-destructive effects chain runs the expensive
  `apply_effects_to_color_image` on a worker thread (`render_ps_raster_effects`), never the GUI
  thread. `apply_effects_to_raster` clones the base pixels and spawns the render (stashing a
  latest-wins request via `pending_raster_effects` if one is already in flight);
  `poll_ps_raster_effects_jobs` (once per frame) does the cheap GUI-side apply (recenter, doc
  routing, reversible persist). Mirrors the typing tab's `apply_raster_effects_edit` pipeline.

Notes:
Base layers mirror existing models read-only and are never written back. User raster layers are
session-scoped in memory (kept per page); on-disk persistence is a future phase.
*/

pub mod edit_op;
pub mod layer_render;
pub mod layers;
pub mod page_loader;
pub mod selection;
pub mod text_layers;
pub mod tools;
pub mod tree;
pub mod viewport;

use crate::memory_manager::{MemoryBudget, MemoryProfile};
use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::models::layer_model::effects;
use crate::models::layer_model::manifest::TransformRec;
use crate::models::layer_model::ordering::Band;
use crate::models::layer_model::persist;
use crate::models::layer_model::saver;
use crate::project::ProjectData;
use crate::tabs::AppTab;
use crate::trace::cat;
use crate::widgets::panel_dock::{
    DockArea, DockEdge, DockLayout, HostId, PanelAnchor, PanelDock, PanelDockState, PanelId,
    PanelNode, TabExtras, TabId,
};
use edit_op::{LayerFieldPatch, LifecycleDir, PsEditOp};
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
use tools::select::{SelectMode, SelectTool};
use tools::transform::TransformTool;
use tools::{PsTool, PsToolContext, PsToolId, PsToolSection, ToolOutcome};

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
// The five `TabId` literals below are STABLE, NON-LOCALIZED PERSISTENCE IDENTITIES: they key the
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

/// `TabExtras` flag key of the «Инструменты» panel's visibility. Stored on the MAIN tab.
const PS_EDITOR_FLAG_TOOLS_PANEL: &str = "panels.tools";
/// `TabExtras` flag key of the «Выбранный инструмент» panel's visibility.
const PS_EDITOR_FLAG_ACTIVE_TOOL_PANEL: &str = "panels.active_tool";
/// `TabExtras` flag key of the «Горячие клавиши» panel's visibility.
const PS_EDITOR_FLAG_HOTKEYS_PANEL: &str = "panels.hotkeys";
/// `TabExtras` flag key of the «Слои» panel's visibility.
const PS_EDITOR_FLAG_LAYERS_PANEL: &str = "panels.layers";

/// Every panel starts shown; `TabExtras::set_flag` keeps a flag at its default out of the config.
const PS_EDITOR_PANEL_VISIBLE_DEFAULT: bool = true;

/// Shrink floor of «PS редактор» (points). Below this its one wrapped row stops being readable.
const PS_EDITOR_MAIN_TAB_MIN_SIZE_PX: Vec2 = Vec2::new(240.0, 48.0);
/// First-frame size of «PS редактор», before its content has ever been measured.
const PS_EDITOR_MAIN_TAB_INITIAL_SIZE_PX: Vec2 = Vec2::new(420.0, 72.0);
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

/// Default panel arrangement of the «PS редактор» program tab.
///
/// Two columns over the canvas. On the LEFT, «PS редактор» → «Инструменты» → «Выбранный инструмент»,
/// which mirrors what the static layout showed as a top strip plus a left toolbar. On the RIGHT,
/// «Слои» → «Горячие клавиши», mirroring the old right panel and keeping the left chain from
/// stacking four panels deep on a short window.
///
/// It must name EVERY `TabId` this program tab can declare: `panel_dock::persist` resolves a
/// stored tab key against the default layout's tab set, so a tab missing here would be dropped
/// from the user's arrangement on every load (`panel_dock/persist.rs::known_tabs`).
///
/// No size is pinned: each tab states its own `min_size` / `initial_size` per frame, and the
/// solver sizes a panel from what its content measured.
///
/// Used only when no layout exists yet for this program tab; a restored one always wins. Handed to
/// the app-owned dock state as a plain `fn` pointer, both when the persisted layouts are restored
/// before the first frame and by `ensure_default_layout` on every frame this tab draws — the state
/// keeps that pointer to serve the header's «Сбросить раскладку» item. A model refusal is logged
/// and skipped, never panicked on: the dock then creates a panel for the orphaned tab on its own,
/// which is a degraded arrangement rather than a lost tab.
#[must_use]
pub(crate) fn ps_editor_default_dock_layout() -> DockLayout {
    let mut layout = DockLayout::new();
    let main = PanelId::new(0);
    let tools = PanelId::new(1);
    let layers = PanelId::new(3);
    let panels = [
        // Insertion order IS anchor order: `insert_panel` rejects an anchor whose target does not
        // exist yet, so every chain root precedes its dependants.
        (
            main,
            vec![PS_EDITOR_MAIN_TAB],
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Left,
                along: 0.0,
            },
        ),
        (
            tools,
            vec![PS_EDITOR_TOOLS_TAB],
            PanelAnchor::Panel {
                target: main,
                edge: DockEdge::Bottom,
                align: 0.0,
            },
        ),
        (
            PanelId::new(2),
            vec![PS_EDITOR_ACTIVE_TOOL_TAB],
            PanelAnchor::Panel {
                target: tools,
                edge: DockEdge::Bottom,
                align: 0.0,
            },
        ),
        (
            layers,
            vec![PS_EDITOR_LAYERS_TAB],
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Right,
                along: 0.0,
            },
        ),
        (
            PanelId::new(4),
            vec![PS_EDITOR_HOTKEYS_TAB],
            PanelAnchor::Panel {
                target: layers,
                edge: DockEdge::Bottom,
                align: 0.0,
            },
        ),
    ];
    for (id, tabs, anchor) in panels {
        let node = match PanelNode::new(id, HostId::MainWindow, tabs) {
            Ok(mut node) => {
                node.anchor = anchor;
                node
            }
            Err(error) => {
                crate::runtime_log::log_warn(format!(
                    "[ps_editor] default dock layout: could not build panel {id} ({error}); \
                     the dock will create one per orphaned tab on its own"
                ));
                continue;
            }
        };
        if let Err(error) = layout.insert_panel(node) {
            crate::runtime_log::log_warn(format!(
                "[ps_editor] default dock layout: could not insert panel {id} ({error}); \
                 the dock will create one per orphaned tab on its own"
            ));
        }
    }
    layout
}

/// Which of the four secondary panels the «Панели…» menu currently shows.
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
}

impl PsEditorPanelVisibility {
    /// Reads the four flags out of the MAIN tab's stored extras, defaulting to "shown".
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
        }
    }

    /// The four toggles as `(flag key, localized title, value)`, in menu order.
    ///
    /// One source for both the menu rows and the write-back below, so a panel can never appear in
    /// the menu under a key that is not the one persisted for it.
    fn entries(&mut self) -> [(&'static str, &'static str, &mut bool); 4] {
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

/// Draws the «PS редактор» tab body: the page/zoom strip plus the «Панели…» visibility menu.
///
/// The only body declared with `show_with_extras`, because it is the only one that WRITES the
/// dock's per-tab extra state: the four flags are pushed on every frame with the value the menu
/// currently shows, which is the usage `TabExtras::set_flag` is designed for — it stores nothing
/// while a flag equals its default and raises `changed` only when the content really moved.
fn draw_main_tab_body(ui: &mut egui::Ui, cx: &mut PsEditorDockCx<'_>, extras: &mut TabExtras) {
    cx.tab.main_tab_contents(ui, cx.project);
    ui.separator();
    ps_editor_panels_menu(ui, &mut cx.visibility);
    for (key, _title, value) in cx.visibility.entries() {
        extras.set_flag(key, *value, PS_EDITOR_PANEL_VISIBLE_DEFAULT);
    }
}

/// The «Панели…» menu button: one checkbox per SECONDARY panel of this program tab.
///
/// «PS редактор» itself is never listed — the menu lives in it, so hiding it could not be undone.
///
/// The menu is configured `CloseOnClickOutside` instead of egui's default `CloseOnClick`, so the
/// user can toggle several panels in one visit; egui closes it on Esc or on a click elsewhere
/// (`egui-0.35.0/src/containers/popup.rs:77-91`). Ids come from the flag-key literals, never from
/// the localized captions (`egui-docs/05-ids-and-i18n.md` §2).
fn ps_editor_panels_menu(ui: &mut egui::Ui, visibility: &mut PsEditorPanelVisibility) {
    let (response, _inner) = egui::menu::MenuButton::new(t!("ps_editor.panels_menu.button"))
        .config(
            egui::menu::MenuConfig::new()
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
        )
        .ui(ui, |ui| {
            for (key, title, value) in visibility.entries() {
                ui.push_id(key, |ui| ui.checkbox(value, title));
            }
        });
    response.on_hover_text(t!("ps_editor.panels_menu.tooltip"));
}

/// Draws the «Инструменты» tab body.
fn draw_tools_tab_body(ui: &mut egui::Ui, cx: &mut PsEditorDockCx<'_>) {
    cx.tab.tools_tab_contents(ui);
}

/// Draws the «Выбранный инструмент» tab body.
fn draw_active_tool_tab_body(ui: &mut egui::Ui, cx: &mut PsEditorDockCx<'_>) {
    cx.tab.active_tool_tab_contents(ui);
}

/// Draws the «Горячие клавиши» tab body.
fn draw_hotkeys_tab_body(ui: &mut egui::Ui, cx: &mut PsEditorDockCx<'_>) {
    cx.tab.hotkeys_tab_contents(ui);
}

/// Draws the «Слои» tab body, parking its deferred actions in the frame context.
///
/// The actions are NOT applied here: `apply_panel_actions` rewrites the layer stack, the shared
/// document and the on-disk manifest, and it must run after the dock frame has finished — the same
/// place the static right panel applied them from.
fn draw_layers_tab_body(ui: &mut egui::Ui, cx: &mut PsEditorDockCx<'_>) {
    cx.panel_actions = cx.tab.layers_panel_body(ui);
}

/// Lays a shortcut inventory out as a two-column grid: the action, then its keys in monospace.
///
/// `id_salt` must be a stable literal — two grids in one panel need distinct ids, and a localized
/// caption must never become one (`egui-docs/05-ids-and-i18n.md` §2).
///
/// Deliberately a LOCAL renderer rather than a reuse of the canvas' equivalent
/// (`CanvasScene::draw_hint_rows_grid`): that one is private to the canvas impl, this tab is not a
/// canvas tab, and lifting it into a shared widget would be a refactor of another subsystem for no
/// gain here. The duplication is a few lines and is recorded in this module's `MODULE_README.md`.
fn draw_hotkey_rows_grid(ui: &mut egui::Ui, id_salt: &'static str, rows: &[tools::PsHotkeyRow]) {
    egui::Grid::new(id_salt)
        .num_columns(2)
        .striped(true)
        .show(ui, |ui| {
            for row in rows {
                ui.label(&row.action);
                ui.monospace(&row.keys);
                ui.end_row();
            }
        });
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
    panel_selection: HashSet<RowSel>,
    /// Anchor row for Shift-range selection in the panel.
    panel_anchor: Option<RowSel>,
    /// The row whose controls the "active layer" strip shows (last plain/ctrl click).
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
    /// Last `LayerDoc::version` this tab projected. Each frame, if the live doc version differs, the
    /// tab re-projects its current page from the shared doc — the in-memory cross-tab sync.
    last_doc_version: u64,
    /// Trace-only: number of composite steps emitted on the previous frame. Used to gate the
    /// per-frame `draw_composite` detail log so it only fires when the plan size changes (the
    /// composite is rebuilt every frame, so unconditional logging would flood the trace at 60/s).
    trace_last_composite_steps: usize,
    /// Shared unified layer document (app-owned): the source of truth for per-page layer MODEL state,
    /// shared with the typing tab. `None` until `set_layer_doc` is called by app.rs.
    layer_doc:
        Option<std::sync::Arc<std::sync::Mutex<crate::models::layer_model::layer_doc::LayerDoc>>>,
    /// Set by the "100%" button; consumed in `draw_canvas` where the real canvas rect is known.
    pending_actual_size: bool,
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
    /// Transform-tool gesture start snapshot: `(raster uid, transform BEFORE the gesture)`. Captured on
    /// the press frame and consumed at release to record ONE `FieldPatch::Transform` per gesture.
    transform_gesture_before: Option<(String, LayerTransform)>,
    /// Deform-tool gesture start snapshot: `(raster uid, deform BEFORE the gesture)`. Captured on the
    /// press frame and consumed at release to record ONE `FieldPatch::Deform` per gesture.
    deform_gesture_before: Option<(
        String,
        Option<crate::models::layer_model::manifest::DeformRec>,
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
/// reloads); rasters key on the session `LayerId` (matches `LayerStack::active`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RowSel {
    Raster(LayerId),
    Text(String),
    Group(String),
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
    /// Selection key (`None` for the locked base layers, which take no part in group ops).
    sel: Option<RowSel>,
    kind: tree::LeafKind,
    depth: u8,
    name: String,
    visible: bool,
    is_base: bool,
}

/// One band with the keys needed to build a contiguous unified order (`build_unified_order`).
struct BandItem {
    band: persist::BandRef,
    /// Unified Z (band index).
    primary: u32,
    /// Tiebreak at equal Z (page-Y for texts, 0 for rasters) — mirrors `draw_composite`.
    secondary: f32,
    /// Final PS-group membership of this band (`None` for text-group bands, never grouped as a unit).
    group: Option<String>,
}

/// A deferred action on a typing text layer, triggered from the PS layers panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextLayerOp {
    /// Pin/unpin the overlay as its own Z band (vs. auto page-Y order within its group).
    TogglePin,
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
            last_doc_version: 0,
            trace_last_composite_steps: usize::MAX,
            layer_doc: None,
            pending_actual_size: false,
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
            transform_gesture_before: None,
            deform_gesture_before: None,
            panel_rects: Vec::new(),
        }
    }
}

impl PsEditorTabState {
    /// Wires the shared clean-overlay model used as the source/clean layer provider.
    pub fn set_overlays_model(&mut self, model: Arc<Mutex<CleanOverlaysModel>>) {
        self.overlays_model = Some(model);
    }

    /// Wires the app-owned shared unified layer document (see `layer_doc`).
    pub fn set_layer_doc(
        &mut self,
        doc: std::sync::Arc<std::sync::Mutex<crate::models::layer_model::layer_doc::LayerDoc>>,
    ) {
        self.layer_doc = Some(doc);
    }

    /// Pixel sizes for EVERY page of the chapter, keyed by page index (memoized via header-only
    /// `image_dimensions`). The shared doc needs the full map — not just the loaded page — because the
    /// legacy absolute-ribbon migration recovers a chapter-wide ribbon scale from every page's aspect.
    fn page_sizes_map(&mut self, project: &ProjectData) -> HashMap<usize, [usize; 2]> {
        let mut out = HashMap::with_capacity(project.pages.len());
        for page in &project.pages {
            let size = match self.page_sizes_px.get(&page.idx) {
                Some(size) => *size,
                None => {
                    let size = image::image_dimensions(&page.path)
                        .map(|(w, h)| [w as usize, h as usize])
                        .unwrap_or([1, 1]);
                    self.page_sizes_px.insert(page.idx, size);
                    size
                }
            };
            out.insert(page.idx, size);
        }
        out
    }

    /// Seeds the PS-owned text-node METADATA (pin / group / text-group) from `layers.json` for
    /// `page_idx`, plus the unified band order. The skeletal text layers are filled with image +
    /// geometry by the subsequent `sync_view_from_doc` projection from the shared doc. Reads NO
    /// `text_info.json` — the doc is the source of truth for text (the typing tab no longer writes that
    /// legacy file). A page whose text still lives only in legacy `text_info.json` yields no metadata
    /// here; the doc projection then materializes those nodes (with default pins).
    fn reload_overlays_view(&mut self, project: &ProjectData, page_idx: usize) {
        self.text_layers = text_layers::load_page_text_layer_meta(
            &project.paths.unsaved_layers_dir,
            &project.paths.layers_dir,
            page_idx,
        );
        self.bands = persist::load_page_bands(
            &project.paths.unsaved_layers_dir,
            Some(&project.paths.layers_dir),
            page_idx,
        );
    }

    /// Rebuilds this tab's per-page projections (`stack` raster layers + groups, `text_layers`,
    /// `bands`) from the shared `LayerDoc`'s resident page, which is the source of truth for layer
    /// MODEL state (transform, effects, display pixels, z, visibility, opacity, group). Local
    /// runtime/GPU/UI state is preserved and matched by uid:
    ///
    /// - Rasters: each doc Raster node is reconciled onto the `LayerStack` raster with the same uid
    ///   (its `LayerId` — and thus its `render_cache` `TiledTexture` — is preserved); a node without a
    ///   matching stack raster gets a fresh layer, and a stack raster whose uid left the doc is
    ///   removed. The render-cache tile is dropped (forcing re-upload) only when the node's
    ///   `generation` or image size changed. The stack's raster order is set to the doc z order.
    /// - Groups: rebuilt from the doc page's `GroupMeta`, mapping each uid to a stable session
    ///   `GroupId` (reusing the existing id when the group survived), and each raster's group is set.
    /// - Text layers: each doc Text node is reconciled onto the existing `PsTextLayer` with the same
    ///   uid — MODEL fields (transform/deform/visible/image/group) are updated while pin / text-group
    ///   (`layer_idx`) metadata and the GPU texture are preserved (the texture re-uploads only on a
    ///   generation change). Text nodes without a local runtime are skipped (the disk-loaded
    ///   `reload_overlays_view` owns runtime creation; this preserves projected indices).
    /// - Bands: one `Raster`/`PinnedText` band per node, z taken directly from the node.
    ///
    /// Replaces the disk-reload path: callers load the doc page (page-load / bridge) then project here.
    fn sync_view_from_doc(&mut self, page_idx: usize) {
        use crate::models::layer_model::layer_doc::{NodeBody, NodeKind};
        let Some(doc) = self.layer_doc.clone() else {
            return;
        };
        let Ok(guard) = doc.lock() else {
            return;
        };
        let Some(page) = guard.page(page_idx) else {
            return;
        };
        let Some(stack) = self.stack.as_mut() else {
            return;
        };

        let _s = crate::trace_scope!(
            cat::SYNC,
            "sync_view_from_doc page={} doc_version={}",
            page_idx,
            guard.version()
        );

        // --- Groups: rebuild from the doc, reusing session ids for surviving uids. ---
        let mut uid_to_gid: HashMap<String, GroupId> = stack
            .groups()
            .iter()
            .map(|g| (g.uid.to_string(), g.id))
            .collect();
        // Drop groups no longer in the doc.
        let doc_group_uids: HashSet<String> = page.groups.iter().map(|g| g.uid.clone()).collect();
        let stale_gids: Vec<GroupId> = stack
            .groups()
            .iter()
            .filter(|g| !doc_group_uids.contains(&g.uid.to_string()))
            .map(|g| g.id)
            .collect();
        for gid in stale_gids {
            stack.remove_group(gid);
        }
        for gmeta in &page.groups {
            let gid = if let Some(&gid) = uid_to_gid.get(&gmeta.uid) {
                gid
            } else {
                let parsed =
                    uuid::Uuid::parse_str(&gmeta.uid).unwrap_or_else(|_| uuid::Uuid::new_v4());
                let gid = stack.add_group_with_uid(gmeta.name.clone(), parsed);
                uid_to_gid.insert(gmeta.uid.clone(), gid);
                gid
            };
            if let Some(g) = stack.group_mut(gid) {
                g.name = gmeta.name.clone();
                g.visible = gmeta.visible;
                g.opacity = gmeta.opacity;
                g.collapsed = gmeta.collapsed;
            }
        }

        // --- Rasters: reconcile doc Raster nodes onto stack rasters by uid. ---
        // uid -> existing session LayerId for current stack rasters.
        let existing_ids: HashMap<String, LayerId> = stack
            .layers()
            .iter()
            .filter(|l| l.kind == LayerKind::Raster)
            .map(|l| (l.uid.to_string(), l.id))
            .collect();
        let doc_raster_uids: HashSet<String> = page
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Raster)
            .map(|n| n.uid.clone())
            .collect();
        // Remove stack rasters whose uid left the doc; drop their caches.
        let removed_ids: Vec<LayerId> = stack
            .layers()
            .iter()
            .filter(|l| {
                l.kind == LayerKind::Raster && !doc_raster_uids.contains(&l.uid.to_string())
            })
            .map(|l| l.id)
            .collect();
        for id in &removed_ids {
            stack.remove_layer(*id);
        }
        // The active raster id, preserved across the rebuild when it survives.
        let prev_active = stack.active_id();

        // Caches to drop after the borrow ends (generation/size changed or layer removed).
        let mut drop_caches: Vec<LayerId> = removed_ids;
        // Ordered (bottom-to-top by doc z) raster ids, to set the stack order afterwards.
        let mut ordered_raster_ids: Vec<LayerId> = Vec::new();

        for node in &page.nodes {
            let NodeBody::Raster {
                base_image,
                display_image,
                effects,
                ..
            } = &node.body
            else {
                continue;
            };
            let cache_key = (page_idx, node.uid.clone());
            let gen_changed =
                self.node_generations.get(&cache_key).copied() != Some(node.generation);
            let group = node
                .group_uid
                .as_ref()
                .and_then(|u| uid_to_gid.get(u).copied());
            if let Some(&id) = existing_ids.get(&node.uid) {
                if let Some(layer) = stack.layer_mut(id) {
                    // A layer with uncommitted base-pixel edits that are ALSO still dirty in the doc
                    // node (an in-progress paint not yet committed/flushed) keeps its live pixels; only
                    // its non-pixel model fields are reconciled, so a revision-driven projection never
                    // clobbers in-flight work. Once routed+flushed (doc node clean), the projection
                    // adopts the doc's pixels and clears the local dirty flag.
                    let keep_pixels = layer.pixels_dirty && node.pixels_dirty;
                    let size_changed = !keep_pixels && layer.image.size != display_image.size;
                    layer.name = node.name.clone();
                    layer.visible = node.visible;
                    layer.opacity = node.opacity;
                    layer.group = group;
                    // Deform is geometry (absolute page px), not pixels — sync it regardless of the
                    // pixels-dirty guard so a mesh edit in either tab reflects immediately.
                    layer.deform = node.deform.clone();
                    if !keep_pixels {
                        layer.transform = rec_to_transform(node.transform);
                        layer.image = display_image.clone();
                        layer.base_image = base_image.clone();
                        layer.effects = effects.clone();
                        layer.pixels_dirty = node.pixels_dirty;
                        if gen_changed || size_changed {
                            drop_caches.push(id);
                        }
                    }
                }
                ordered_raster_ids.push(id);
            } else {
                let id = stack.add_raster_layer_image(
                    node.name.clone(),
                    display_image.clone(),
                    rec_to_transform(node.transform),
                );
                if let Some(layer) = stack.layer_mut(id) {
                    layer.uid =
                        uuid::Uuid::parse_str(&node.uid).unwrap_or_else(|_| uuid::Uuid::new_v4());
                    layer.visible = node.visible;
                    layer.opacity = node.opacity;
                    layer.base_image = base_image.clone();
                    layer.effects = effects.clone();
                    layer.group = group;
                    layer.deform = node.deform.clone();
                }
                ordered_raster_ids.push(id);
            }
            self.node_generations.insert(cache_key, node.generation);
        }
        // Set the stack's raster order to the doc z order (bottom-to-top), keeping base layers first.
        stack.reorder_rasters(&ordered_raster_ids);
        stack.set_active(prev_active);

        // --- Text layers: reconcile doc Text nodes onto local runtimes by uid. ---
        // Build the new text-layer list in doc order, preserving pin / layer_idx / texture by uid.
        let mut prev_text: HashMap<String, PsTextLayer> = self
            .text_layers
            .drain(..)
            .map(|t| (t.uid().to_string(), t))
            .collect();
        let mut new_text: Vec<PsTextLayer> = Vec::new();
        for node in &page.nodes {
            let NodeBody::Text {
                image, render_data, ..
            } = &node.body
            else {
                continue;
            };
            // Raw overlay text (for the panel row preview); empty when render_data lacks it.
            let text_content = render_data
                .get("text_params")
                .and_then(|tp| tp.get("text"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let cache_key = (page_idx, node.uid.clone());
            let gen_changed =
                self.node_generations.get(&cache_key).copied() != Some(node.generation);
            // Reconcile onto the prior runtime when one exists (preserving pin / text-group / texture);
            // otherwise BUILD a fresh PsTextLayer from the doc node. The doc is now the source of truth
            // for text (typing no longer writes `text_info.json`), so PS must materialize a text node it
            // has not seen before — e.g. an overlay just created in the typing tab. Pin / pinned_by_group
            // and the text-group (`layer_idx`) come from the prior runtime when present, else from the
            // node (`text_layer_idx`) with pins defaulting false (the bands carry the live Z).
            let prev = prev_text.remove(&node.uid);
            // Text is fully-manual pinned-with-explicit-Z now: a freshly-projected text node defaults to
            // PINNED so it gets its own unified band (its Z comes from the page's `PinnedText` band; a
            // not-yet-resaved legacy chapter still falls back to its group band Z until the first save
            // flattens it). Was `false`, which would have made every text look unpinned (a `TextGroup`).
            let (layer_idx, pinned, pinned_by_group) = match &prev {
                Some(p) => (p.layer_idx, p.pinned, p.pinned_by_group),
                None => (node.text_layer_idx.unwrap_or(0), true, false),
            };
            // Preserve the GPU texture unless the node's pixels changed (only possible with a prior).
            let texture = match prev {
                Some(mut p) if !gen_changed => p.take_texture(),
                _ => None,
            };
            self.node_generations.insert(cache_key, node.generation);
            new_text.push(PsTextLayer::from_doc_node(
                node.uid.clone(),
                node.name.clone(),
                node.visible,
                layer_idx,
                node.group_uid.clone(),
                pinned,
                pinned_by_group,
                text_content,
                image.clone(),
                LayerTransform {
                    center: Vec2::new(node.transform.cx, node.transform.cy),
                    rotation: node.transform.rotation,
                    scale: node.transform.scale,
                },
                node.deform.clone(),
                texture,
            ));
        }
        self.text_layers = new_text;

        // --- Bands: derive unified Z directly from the doc node z. ---
        let mut bands: Vec<Band> = Vec::with_capacity(page.nodes.len());
        for node in &page.nodes {
            match node.kind {
                NodeKind::Raster => bands.push(Band::Raster {
                    uid: node.uid.clone(),
                    z: node.z,
                }),
                NodeKind::Text => bands.push(Band::PinnedText {
                    uid: node.uid.clone(),
                    z: node.z,
                }),
            }
        }
        self.bands = bands;

        // Record the doc version we just projected so the per-frame version check does not
        // redundantly re-project until the doc changes again.
        self.last_doc_version = guard.version();

        drop(guard);
        for id in drop_caches {
            self.render_cache.remove(&id);
        }
    }

    /// Routes a raster/group MODEL edit to the shared `LayerDoc`: locks it, runs `edit` against the
    /// resident page (loaded by `poll_loader`), flushes the page to disk (so it survives reloads /
    /// save-to-project), and re-projects the view from the doc. The doc mutation bumps the doc
    /// version, so the typing tab re-projects via its per-frame version check. Returns false (the
    /// caller keeps its legacy path) when no doc is wired or the page isn't resident.
    fn route_to_doc<F>(&mut self, page_idx: usize, project: &ProjectData, edit: F) -> bool
    where
        F: FnOnce(&mut crate::models::layer_model::layer_doc::LayerDoc),
    {
        let Some(doc) = self.layer_doc.clone() else {
            return false;
        };
        {
            let Ok(mut guard) = doc.lock() else {
                return false;
            };
            if guard.page(page_idx).is_none() {
                return false;
            }
            edit(&mut guard);
            // Guarantee a cross-tab notification even if `edit` mutated node fields directly via
            // `node_mut` (which does not bump the version). Idempotent if `edit` already bumped.
            guard.mark_changed();
            // Persist so the change survives a reload / save-to-project. ASYNC: enqueue the page job
            // to the background saver (PNG encode + manifest RMW off the GUI thread); falls back to a
            // synchronous flush when no saver is enabled. The save-to-project merge worker and the
            // app-close drain barrier the queue, so an enqueued write is never lost.
            if let Err(err) = guard.enqueue_page_save(
                page_idx,
                &project.paths.unsaved_layers_dir,
                Some(&project.paths.layers_dir),
            ) {
                crate::runtime_log::log_warn(format!("[ps_editor] doc flush: {err}"));
            }
            // This page now has its persist enqueued; the tab-switch flush is redundant for it.
            self.layers_dirty = false;
        }
        self.sync_view_from_doc(page_idx);
        true
    }

    /// Undoes the most recent PS-editor edit on the current page, if any. Returns whether anything
    /// changed. Routes the reverted pixels to the shared doc + enqueues a disk save so cross-tab
    /// state and persistence stay in sync. Safe on the GUI thread: the raster apply is a bounded,
    /// per-tile delta reversal (no full-image work beyond the changed tiles).
    pub fn undo(&mut self, project: &ProjectData) -> bool {
        // Take-and-restore idiom (see `take_history`): the op `Ctx` is `Self` but `history` is a field
        // of `Self`, so `self.history.undo(self)` would double-borrow. Restore the history
        // UNCONDITIONALLY (no `?` between take and restore) so the stack is never lost on an error path.
        let mut history = self.take_history();
        let result = history.undo(self);
        self.history = history;
        self.finish_history_step(result, project, "undo")
    }

    /// Redoes the most recently undone PS-editor edit on the current page, if any. See [`Self::undo`].
    pub fn redo(&mut self, project: &ProjectData) -> bool {
        let mut history = self.take_history();
        let result = history.redo(self);
        self.history = history;
        self.finish_history_step(result, project, "redo")
    }

    /// Shared tail of `undo`/`redo`: on a real change, persist the active page so the reverted state
    /// survives a reload / save-to-project; logs and swallows an apply error (nothing changed).
    ///
    /// Uses `persist_current_page` (not a bare doc enqueue) because it reads the reconciled
    /// `self.stack` and carries the EXPLICIT `removed_uids` from `deleted_raster_uids` — required so a
    /// `LayerLifecycle` delete/undo drops or keeps the on-disk raster PNG correctly (the doc's own
    /// `enqueue_page_save` passes an empty removed set, which would resurrect a just-deleted raster).
    /// The PNG encode still runs off the GUI thread (the saver owns it).
    fn finish_history_step(
        &mut self,
        result: Result<bool, edit_op::PsEditOpError>,
        project: &ProjectData,
        op: &str,
    ) -> bool {
        match result {
            Ok(true) => {
                self.persist_current_page(project);
                true
            }
            Ok(false) => false,
            Err(err) => {
                crate::runtime_log::log_warn(format!("[ps_editor] {op}: {err}"));
                false
            }
        }
    }

    /// Move the undo history out of `self`, leaving an empty history that preserves the count limit
    /// and byte budget. Used only by the take-and-restore undo/redo idiom; the caller MUST put a
    /// history back before returning.
    fn take_history(&mut self) -> ActionHistory<PsEditOp> {
        let limit = self.history.limit();
        let replacement = match self.history.weight_budget() {
            Some(budget) => ActionHistory::with_weight_budget(limit, budget),
            None => ActionHistory::new(limit),
        };
        std::mem::replace(&mut self.history, replacement)
    }

    /// Applies a reversible raster delta to the resident layer identified by `layer_uid` on
    /// `page_idx`, in `dir`. Mutates the layer's `image` + `base_image` (via
    /// `edit_op::apply_raster_diff_to_layer`), marks the affected `render_cache` tiles dirty, and
    /// pushes the resulting pixels to the shared doc (in-memory) so a later reprojection and cross-tab
    /// consumers see the same result. This is the only mutation path used by PS-editor undo/redo.
    ///
    /// # Errors
    /// - [`edit_op::PsEditOpError::NotResident`] if the stack is absent, on a different page, or the
    ///   uid is no longer a resident raster.
    /// - [`edit_op::PsEditOpError::Raster`] if the delta cannot be applied (size mismatch / corrupt).
    fn apply_ps_raster_edit(
        &mut self,
        page_idx: usize,
        layer_uid: &str,
        diff: &RasterDiff,
        dir: ApplyDirection,
    ) -> Result<(), edit_op::PsEditOpError> {
        // Resolve the target raster (resident + matching page) and mutate its pixels.
        let (id, uid, reverted, dirty) = {
            let stack = self
                .stack
                .as_mut()
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            if stack.page_idx() != page_idx {
                return Err(edit_op::PsEditOpError::NotResident { page_idx });
            }
            let id = stack
                .layers()
                .iter()
                .find(|l| l.kind == LayerKind::Raster && l.uid.to_string() == layer_uid)
                .map(|l| l.id)
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            let layer = stack
                .layer_mut(id)
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            let dirty = edit_op::apply_raster_diff_to_layer(layer, diff, dir)?;
            layer.pixels_dirty = true;
            (id, layer.uid.to_string(), layer.image.clone(), dirty)
        };

        // Invalidate only the touched tiles so the next upload re-sends the reverted pixels.
        if let Some(cache) = self.render_cache.get_mut(&id) {
            for rect in &dirty {
                cache.mark_dirty_rect(tools::DirtyRect {
                    min_x: rect.origin_px[0] as usize,
                    min_y: rect.origin_px[1] as usize,
                    max_x: rect.origin_px[0].saturating_add(rect.size_px[0].saturating_sub(1))
                        as usize,
                    max_y: rect.origin_px[1].saturating_add(rect.size_px[1].saturating_sub(1))
                        as usize,
                });
            }
        }

        // Route the reverted pixels to the shared doc (in-memory) so cross-tab consumers and the next
        // `sync_view_from_doc` reprojection agree. A paintable raster has no effects, so
        // base == display == reverted pixels. Disk persistence is enqueued by the undo/redo caller.
        if let Some(doc) = self.layer_doc.clone()
            && let Ok(mut guard) = doc.lock()
            && guard.page(page_idx).is_some()
        {
            guard.set_raster_pixels(page_idx, &uid, reverted.clone(), reverted, Vec::new(), true);
            guard.mark_changed();
        }
        Ok(())
    }

    /// Writes the `Клин` base layer's CURRENT pixels over the page-space rect `(x, y, w, h)` back
    /// into the shared `CleanOverlaysModel`, and re-establishes `base_image == image` over the same
    /// rect.
    ///
    /// This is the ONE write-back path for every Clean-layer pixel edit (brush commit, cut,
    /// merge-into, undo/redo), so the rules below hold exactly once:
    /// - only the given rect is pushed — a ribbon page can be ~800x19000 px, so the whole buffer is
    ///   never sent;
    /// - `chunk` is a raw crop of the premultiplied `Layer::image`; the model converts it to
    ///   straight RGBA itself;
    /// - the model write happens FIRST and the local commit (`base_image` mirror, `pixels_dirty`)
    ///   only after the model accepted it — otherwise a rejected write would leave the edit alive
    ///   solely in this tab's stack, to die unnoticed at the next reload;
    /// - the write is REFUSED when the model's materialized overlay for this page has a different
    ///   size than the stack's page: `replace_region` would clamp to the MODEL's size and scale the
    ///   chunk into it, landing a brush stroke scaled and misaligned. That legacy mismatch is the
    ///   case `page_loader` answers with a transparent page-sized `Клин`, so it is a real state, and
    ///   a silent rescale is exactly the incorrect fallback the project forbids. NOT covered: an
    ///   overlay the model remembers a size for but has not materialized (only reachable through
    ///   `detach_page_overlay`) — the model exposes no size accessor for that state;
    /// - `last_overlay_revision` is adopted IN THE SAME LOCK SCOPE, but ONLY when it still equals
    ///   the revision read just before our own write — i.e. when nothing foreign changed the model
    ///   while this tab was away. Adopting it unconditionally would swallow a background writer's
    ///   bump (the page manager's clean-attach worker is one) and permanently suppress the reload
    ///   that reconciles it, leaving the tab painting on stale pixels for the rest of the session.
    ///   Declining to adopt costs one redundant full-page reload at the next
    ///   `sync_view_from_canvas`; it never costs the user's work, which the model already holds and
    ///   the reload restores from `overlay_rgba`.
    /// - `pixels_dirty` is cleared: the shared model (and its autosave worker) owns Clean
    ///   persistence, and `persist_current_page` deliberately never writes base layers.
    ///
    /// Returns `true` when the model accepted the region. `false` means the stack is absent or on
    /// another page, the rect is empty after clamping, there is no Clean layer, no model is bound,
    /// the model's lock is poisoned, or the overlay size mismatched — the first three are ordinary
    /// no-ops, the rest are logged with context.
    fn write_clean_region_to_model(
        &mut self,
        page_idx: usize,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
    ) -> bool {
        // Phase 1 — crop the chunk out of the stack, mutating nothing. The local commit is phase 3,
        // after the model has actually taken the pixels.
        let Some(stack) = self.stack.as_ref() else {
            return false;
        };
        if stack.page_idx() != page_idx {
            return false;
        }
        let page_size = stack.size();
        let Some(clean) = stack.layers().iter().find(|l| l.kind == LayerKind::Clean) else {
            return false;
        };
        let clean_id = clean.id;
        // Clamp the requested rect into the layer image; an empty result is a no-op.
        let [iw, ih] = clean.image.size;
        let x0 = x.min(iw);
        let y0 = y.min(ih);
        let x1 = x.saturating_add(w).min(iw);
        let y1 = y.saturating_add(h).min(ih);
        if x0 >= x1 || y0 >= y1 {
            return false;
        }
        let (cw, ch) = (x1 - x0, y1 - y0);
        let mut pixels: Vec<Color32> = Vec::with_capacity(cw.saturating_mul(ch));
        for row in y0..y1 {
            let start = row * iw + x0;
            let Some(span) = clean.image.pixels.get(start..start + cw) else {
                // Clamped above, so this cannot happen; bail out instead of panicking.
                return false;
            };
            pixels.extend_from_slice(span);
        }
        let chunk = ColorImage::new([cw, ch], pixels);

        // Phase 2 — push to the shared model.
        let Some(model) = self.overlays_model.as_ref() else {
            crate::runtime_log::log_error(format!(
                "[ps_editor] no clean overlay model is bound; the Клин edit on page {page_idx} \
                 rect ({x0},{y0},{cw},{ch}) was not shared"
            ));
            return false;
        };
        let Ok(mut locked) = model.lock() else {
            crate::runtime_log::log_error(format!(
                "[ps_editor] clean overlay model lock poisoned; the Клин edit on page {page_idx} \
                 rect ({x0},{y0},{cw},{ch}) was not shared"
            ));
            return false;
        };
        // Refuse rather than let `replace_region` scale our chunk into a differently-sized overlay.
        // `overlay_size` is the REMEMBERED page size, which is what the model normalizes a write to;
        // a materialized-buffer check (`get`) would miss a detached page, where no buffer exists yet
        // the remembered size still drives the rescale.
        if let Some(model_size) = locked.overlay_size(page_idx)
            && model_size != page_size
        {
            drop(locked);
            crate::runtime_log::log_error(format!(
                "[ps_editor] clean overlay size mismatch on page {page_idx}: model has \
                 {}x{} px, the editor page is {}x{} px; the Клин edit was refused instead of \
                 being rescaled",
                model_size[0], model_size[1], page_size[0], page_size[1]
            ));
            return false;
        }
        // Read the revision BEFORE our write so the adoption below can tell our own bump apart from
        // a foreign one that landed while this tab was not syncing.
        let revision_before = locked.revision();
        let accepted = locked.replace_region(page_idx, page_size, x0, y0, cw, ch, &chunk);
        // Same lock scope as the write: adopt the revision our own write just produced, so the next
        // `sync_view_from_canvas` does not mistake it for another tab's edit and reload the page.
        // Conditional on purpose — see the doc comment: if a foreign writer bumped the revision
        // while we were away, adopting now would hide that edit from the reload forever.
        if accepted && self.last_overlay_revision == revision_before {
            self.last_overlay_revision = locked.revision();
        }
        drop(locked);
        crate::trace_log!(
            cat::SYNC,
            "clean_write page={} rect=({},{},{},{}) accepted={} rev_before={} rev_known={}",
            page_idx,
            x0,
            y0,
            cw,
            ch,
            accepted,
            revision_before,
            self.last_overlay_revision
        );
        if !accepted {
            crate::runtime_log::log_error(format!(
                "[ps_editor] the clean overlay model rejected the Клин edit on page {page_idx} \
                 rect ({x0},{y0},{cw},{ch}); the local pixels were left uncommitted"
            ));
            return false;
        }

        // Phase 3 — the model holds the write, so commit the local bookkeeping.
        let Some(layer) = self.stack.as_mut().and_then(|s| s.layer_mut(clean_id)) else {
            return false;
        };
        // The undo "before" is read from `base_image`, so it must track `image` once an edit is
        // committed — the same invariant `edit_op::apply_raster_diff_to_layer` keeps for rasters.
        if layer.base_image.size == layer.image.size {
            for row in y0..y1 {
                let start = row * iw + x0;
                let (Some(src), Some(dst)) = (
                    layer.image.pixels.get(start..start + cw),
                    layer.base_image.pixels.get_mut(start..start + cw),
                ) else {
                    continue;
                };
                dst.copy_from_slice(src);
            }
        }
        layer.pixels_dirty = false;
        true
    }

    /// Applies a reversible pixel delta to the `Клин` base layer on `page_idx`, in `dir`, and pushes
    /// the result back to the shared `CleanOverlaysModel` (see [`PsEditOp::CleanPixels`]).
    ///
    /// The target is resolved by KIND, never by uid: base-layer uids are regenerated on every page
    /// load, so a uid recorded at stroke time would not match after a reload. Mutates the layer's
    /// `image` + `base_image`, marks the affected `render_cache` tiles dirty, and routes each
    /// changed rect through `write_clean_region_to_model` so an undo is as durable and as
    /// cross-tab-visible as the forward edit.
    ///
    /// # Errors
    /// - [`edit_op::PsEditOpError::NotResident`] if the stack is absent, on a different page, or has
    ///   no Clean layer.
    /// - [`edit_op::PsEditOpError::Raster`] if the delta cannot be applied (size mismatch / corrupt).
    fn apply_ps_clean_edit(
        &mut self,
        page_idx: usize,
        diff: &RasterDiff,
        dir: ApplyDirection,
    ) -> Result<(), edit_op::PsEditOpError> {
        let (id, dirty) = {
            let stack = self
                .stack
                .as_mut()
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            if stack.page_idx() != page_idx {
                return Err(edit_op::PsEditOpError::NotResident { page_idx });
            }
            let id = stack
                .layers()
                .iter()
                .find(|l| l.kind == LayerKind::Clean)
                .map(|l| l.id)
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            let layer = stack
                .layer_mut(id)
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            let dirty = edit_op::apply_raster_diff_to_layer(layer, diff, dir)?;
            (id, dirty)
        };

        // Invalidate only the touched tiles so the next upload re-sends the reverted pixels.
        if let Some(cache) = self.render_cache.get_mut(&id) {
            for rect in &dirty {
                cache.mark_dirty_rect(tools::DirtyRect {
                    min_x: rect.origin_px[0] as usize,
                    min_y: rect.origin_px[1] as usize,
                    max_x: rect.origin_px[0].saturating_add(rect.size_px[0].saturating_sub(1))
                        as usize,
                    max_y: rect.origin_px[1].saturating_add(rect.size_px[1].saturating_sub(1))
                        as usize,
                });
            }
        }

        // Share the reverted pixels. NOTE: `replace_region` records its OWN reversible diff in the
        // clean model's cross-tab history, so this undo also lands there as a further FORWARD edit.
        // State never diverges; the only oddity is that a later Ctrl+Z on the cleaning tab can
        // re-apply what PS just reverted. Documented in `MODULE_README.md`.
        for rect in &dirty {
            self.write_clean_region_to_model(
                page_idx,
                rect.origin_px[0] as usize,
                rect.origin_px[1] as usize,
                rect.size_px[0] as usize,
                rect.size_px[1] as usize,
            );
        }
        Ok(())
    }

    /// Realizes a whole-raster-layer add/delete for undo/redo (see [`PsEditOp::LayerLifecycle`]).
    /// `dir == Added` re-inserts `layer` (with its retained pixels) into the shared doc at Z `z` and
    /// re-projects, so the stack + `render_cache` are rebuilt by `sync_view_from_doc`; `dir == Removed`
    /// removes the node by uid and re-projects (which drops its cache). Deletion bookkeeping
    /// (`deleted_raster_uids`) is updated so the next `persist_current_page` drops/keeps the on-disk
    /// PNG correctly. Never panics.
    ///
    /// # Errors
    /// [`edit_op::PsEditOpError::NotResident`] if no doc is wired, the target page is not resident, or
    /// the add/remove could not be applied.
    fn apply_ps_layer_lifecycle(
        &mut self,
        page_idx: usize,
        layer: &Layer,
        z: u32,
        dir: LifecycleDir,
    ) -> Result<(), edit_op::PsEditOpError> {
        // The op only makes sense while its page's stack is resident (the history is per-page).
        if self.stack.as_ref().map(LayerStack::page_idx) != Some(page_idx) {
            return Err(edit_op::PsEditOpError::NotResident { page_idx });
        }
        let Some(doc) = self.layer_doc.clone() else {
            return Err(edit_op::PsEditOpError::NotResident { page_idx });
        };
        let uid = layer.uid.to_string();
        let ok = {
            let Ok(mut guard) = doc.lock() else {
                return Err(edit_op::PsEditOpError::NotResident { page_idx });
            };
            if guard.page(page_idx).is_none() {
                return Err(edit_op::PsEditOpError::NotResident { page_idx });
            }
            match dir {
                LifecycleDir::Added => {
                    // Rebuild the doc node from the retained layer. `pixels_dirty = true` so the next
                    // persist rewrites its base PNG (the delete pruned it); preserve the deform mesh.
                    let mut node = layer_to_raster_node(layer);
                    node.pixels_dirty = true;
                    if let crate::models::layer_model::layer_doc::NodeBody::Raster {
                        base_image,
                        display_image,
                        ..
                    } = &mut node.body
                    {
                        *base_image = layer.base_image.clone();
                        *display_image = layer.image.clone();
                    }
                    node.deform = layer.deform.clone();
                    let added = guard.add_node_at_z(page_idx, node, z);
                    if added {
                        guard.mark_changed();
                    }
                    added
                }
                LifecycleDir::Removed => {
                    let removed = guard.remove_node(page_idx, &uid);
                    if removed {
                        guard.mark_changed();
                    }
                    removed
                }
            }
        };
        if !ok {
            return Err(edit_op::PsEditOpError::NotResident { page_idx });
        }
        // Keep the deletion bookkeeping consistent so `persist_current_page` drops (Removed) or keeps
        // (Added) the on-disk raster: `save_page_rasters` preserves manifest rasters not in the stack,
        // so a removal must be explicit.
        match dir {
            LifecycleDir::Added => {
                if let Some(set) = self.deleted_raster_uids.get_mut(&page_idx) {
                    set.remove(&uid);
                }
            }
            LifecycleDir::Removed => {
                self.deleted_raster_uids
                    .entry(page_idx)
                    .or_default()
                    .insert(uid);
            }
        }
        // Rebuild the stack raster layers + text + bands (and prune/create the render cache) from the
        // mutated doc — the same projection the forward add/delete paths use.
        self.sync_view_from_doc(page_idx);
        Ok(())
    }

    /// Applies a single metadata/geometry field change (visibility / opacity / transform / deform) to
    /// the raster identified by `layer_uid` on `page_idx`, driving it to the patch's `after` value (see
    /// [`PsEditOp::FieldPatch`]). Routes through the shared doc setter (so cross-tab consumers agree)
    /// and re-projects; falls back to a direct stack mutation when no doc page is resident. These
    /// fields do not change pixels, so no `render_cache` invalidation is needed (compositing re-reads
    /// them each frame). Never panics.
    ///
    /// # Errors
    /// [`edit_op::PsEditOpError::NotResident`] if the stack is absent, on a different page, or the uid
    /// is no longer a resident raster.
    fn apply_ps_field_patch(
        &mut self,
        page_idx: usize,
        layer_uid: &str,
        field: &LayerFieldPatch,
    ) -> Result<(), edit_op::PsEditOpError> {
        // Resolve the target raster (resident + matching page).
        let id = {
            let stack = self
                .stack
                .as_ref()
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            if stack.page_idx() != page_idx {
                return Err(edit_op::PsEditOpError::NotResident { page_idx });
            }
            stack
                .layers()
                .iter()
                .find(|l| l.kind == LayerKind::Raster && l.uid.to_string() == layer_uid)
                .map(|l| l.id)
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?
        };
        // Drive the doc to the `after` value (mirrors the forward edit's setter), then re-project.
        let applied = self.edit_doc_node(page_idx, |doc| match field {
            LayerFieldPatch::Visibility { after, .. } => {
                doc.set_visibility(page_idx, layer_uid, *after);
            }
            LayerFieldPatch::Opacity { after, .. } => doc.set_opacity(page_idx, layer_uid, *after),
            LayerFieldPatch::Transform { after, .. } => {
                doc.set_transform(page_idx, layer_uid, transform_to_rec(*after));
            }
            LayerFieldPatch::Deform { after, .. } => {
                doc.set_deform(page_idx, layer_uid, after.clone());
            }
        });
        if !applied {
            // No doc page resident: mutate the stack layer directly (matches the forward edits' local
            // fallback). `edit_doc_node` returning false means the doc was never touched.
            let layer = self
                .stack
                .as_mut()
                .and_then(|s| s.layer_mut(id))
                .ok_or(edit_op::PsEditOpError::NotResident { page_idx })?;
            edit_op::apply_field_patch_to_layer(layer, field);
        }
        Ok(())
    }

    /// Records the just-committed brush stroke as a reversible undo entry (observer style — the
    /// forward edit was already applied live). Builds a region-bounded `RasterDiff` from the
    /// pre-stroke `base_image` ("before") and the painted `image` ("after") over the stroke's dirty
    /// union, so no full-image scan is needed. A no-op stroke (empty diff) is not recorded.
    ///
    /// Handles BOTH paintable kinds. A user raster records a `PsEditOp::RasterPixels` keyed on its
    /// stable doc uid; the `Клин` base layer records a `PsEditOp::CleanPixels`, which carries NO uid
    /// — base-layer uids are regenerated on every page load, so only the layer KIND identifies it.
    /// Must be called BEFORE the pixels are pushed onward, while `base_image` still holds the
    /// pre-stroke state.
    fn record_brush_stroke(&mut self, page_idx: usize) {
        let Some(union) = self.brush_stroke_dirty else {
            return;
        };
        // Capture the region-local before/after buffers + uid while borrowing the stack immutably.
        let captured = {
            let Some(stack) = self.stack.as_ref() else {
                return;
            };
            if stack.page_idx() != page_idx {
                return;
            }
            let Some(layer) = stack.layer(stack.active_id()) else {
                return;
            };
            // Only a directly-editable layer (raster without effects, or Клин) has
            // base_image == pre-stroke pixels.
            if !layer.can_edit_pixels() {
                return;
            }
            let kind = layer.kind;
            let size = layer.image.size;
            let max_x = size[0].saturating_sub(1);
            let max_y = size[1].saturating_sub(1);
            let x0 = union.min_x.min(max_x);
            let y0 = union.min_y.min(max_y);
            let x1 = union.max_x.min(max_x);
            let y1 = union.max_y.min(max_y);
            if x1 < x0 || y1 < y0 {
                return;
            }
            let w = x1 - x0 + 1;
            let h = y1 - y0 + 1;
            let before = edit_op::copy_region_premul(&layer.base_image, x0, y0, w, h);
            let after = edit_op::copy_region_premul(&layer.image, x0, y0, w, h);
            (
                kind,
                layer.uid.to_string(),
                size,
                [x0, y0],
                [w, h],
                before,
                after,
            )
        };
        let (kind, uid, size, origin, region, before, after) = captured;
        let (Some(origin), Some(region), Some(image_size)) = (
            usize_pair_to_u32(origin),
            usize_pair_to_u32(region),
            usize_pair_to_u32(size),
        ) else {
            return;
        };
        match RasterDiff::from_region_pixels(
            &before,
            &after,
            origin,
            region,
            image_size,
            PS_UNDO_TILE_SIDE,
        ) {
            Ok(diff) if diff.is_empty() => {}
            // A real `match` over the kind (no `_ =>`): `Source` is never paintable, so it is
            // unreachable here, but a new paintable kind must force this site to be revisited.
            Ok(diff) => match kind {
                LayerKind::Raster => self.history.record(PsEditOp::RasterPixels {
                    page_idx,
                    layer_uid: uid,
                    diff: Arc::new(diff),
                    dir: ApplyDirection::Forward,
                    label: t!("ps_editor.edit_op.brush_stroke").to_string(),
                }),
                LayerKind::Clean => self.history.record(PsEditOp::CleanPixels {
                    page_idx,
                    diff: Arc::new(diff),
                    dir: ApplyDirection::Forward,
                    label: t!("ps_editor.edit_op.brush_stroke").to_string(),
                }),
                LayerKind::Source => {}
            },
            Err(err) => {
                crate::runtime_log::log_warn(format!(
                    "[ps_editor] failed to build brush undo diff (page {page_idx}): {err}"
                ));
            }
        }
    }

    /// The unified Z of the raster band with `uid` from the current `bands` projection, or 0 when
    /// absent (a just-added raster whose band has not been projected yet, restored on top on redo).
    fn raster_band_z(&self, uid: &str) -> u32 {
        self.bands
            .iter()
            .find_map(|band| match band {
                Band::Raster { uid: u, z } if u == uid => Some(*z),
                Band::Raster { .. } | Band::TextGroup { .. } | Band::PinnedText { .. } => None,
            })
            .unwrap_or(0)
    }

    /// Resolves a raster `LayerId` to its stable doc uid (the cross-tab identity).
    fn raster_uid(&self, id: LayerId) -> Option<String> {
        self.stack
            .as_ref()
            .and_then(|s| s.layer(id))
            .filter(|l| l.kind == LayerKind::Raster)
            .map(|l| l.uid.to_string())
    }

    /// Mutates the shared `LayerDoc` in memory for a high-frequency / live edit (e.g. an opacity
    /// slider or a visibility toggle) and re-projects, WITHOUT flushing to disk or bumping the
    /// revision — matching the legacy behavior where such edits persisted only on page-leave. Returns
    /// false (caller keeps its legacy local-only path) when no doc is wired or the page isn't resident.
    fn edit_doc_node<F>(&mut self, page_idx: usize, edit: F) -> bool
    where
        F: FnOnce(&mut crate::models::layer_model::layer_doc::LayerDoc),
    {
        let Some(doc) = self.layer_doc.clone() else {
            return false;
        };
        {
            let Ok(mut guard) = doc.lock() else {
                return false;
            };
            if guard.page(page_idx).is_none() {
                return false;
            }
            edit(&mut guard);
            // Guarantee a cross-tab notification even if `edit` mutated node fields directly via
            // `node_mut` (which does not bump the version). Idempotent if `edit` already bumped.
            guard.mark_changed();
        }
        // This edit only changed in-memory MODEL state (it deferred disk persistence to page-leave /
        // tab-switch), so the page now needs a flush on the next tab-switch.
        self.layers_dirty = true;
        self.sync_view_from_doc(page_idx);
        true
    }

    /// Flushes the shared doc's TEXT payload for `page_idx` into the staging `layers.json` (inline v3),
    /// after a PS-side text edit routed through `edit_doc_node`. Text-only — leaves rasters on disk
    /// untouched. The doc is the sole text writer; PS no longer writes `text_info.json`.
    fn flush_text_page(&mut self, page_idx: usize, project: &ProjectData) {
        let Some(doc) = self.layer_doc.clone() else {
            return;
        };
        let Ok(mut guard) = doc.lock() else {
            return;
        };
        // ASYNC text-only persist: enqueue to the background saver (falls back to sync flush when no
        // saver is enabled). The save-to-project/app-close barriers guarantee the enqueued text lands.
        if let Err(err) = guard.enqueue_page_text_save(
            page_idx,
            &project.paths.unsaved_layers_dir,
            Some(&project.paths.layers_dir),
        ) {
            crate::runtime_log::log_warn(format!("[ps_editor] doc text flush: {err}"));
        }
    }

    /// Per-frame cross-tab sync: re-project the current page when the shared `LayerDoc` changed
    /// (its `version` advanced) since this tab last projected. Any edit in the typing tab (or our own
    /// that routed through the doc) bumps the doc version; this is the in-memory cross-tab path
    /// (replacing the old disk-revision bridge).
    fn refresh_view_if_doc_version_changed(&mut self, project: &ProjectData) {
        let Some(doc) = self.layer_doc.clone() else {
            return;
        };
        let current = match doc.lock() {
            Ok(guard) => guard.version(),
            Err(_) => return,
        };
        if current == self.last_doc_version {
            return;
        }
        if self.dragging_text_layer.is_some() {
            return; // don't yank a layer out from under an in-progress drag
        }
        let Some(page_idx) = self.active_page_idx else {
            // No active page to project yet; adopt the version so we don't re-check every frame.
            self.last_doc_version = current;
            return;
        };
        crate::trace_log!(
            cat::SYNC,
            "doc_version_changed old={} new={} page={} -> reproject",
            self.last_doc_version,
            current,
            page_idx
        );
        // Reload the disk-truth text-layer RUNTIME (so a text layer the typing tab just created has a
        // local `PsTextLayer` for the projection to reconcile onto), then project the shared doc over
        // the stack rasters / text model / bands. `sync_view_from_doc` updates `last_doc_version` and
        // preserves uncommitted in-memory edits (pixels_dirty layers).
        self.reload_overlays_view(project, page_idx);
        self.sync_view_from_doc(page_idx);
    }

    /// Page currently shown or being loaded (falls back to the requested page mid-load).
    pub fn current_page(&self) -> Option<usize> {
        self.active_page_idx.or(self.requested_page_idx)
    }

    /// Current camera as `(zoom, center_world)` in image-pixel space, for syncing the view back
    /// to `CanvasView` when leaving this tab.
    pub fn camera(&self) -> (f32, Vec2) {
        (self.viewport.zoom(), self.viewport.center_world())
    }

    /// Synchronizes this tab's view from the shared canvas world when the tab becomes active.
    ///
    /// Mirrors three things from `CanvasView` (best-effort, "в доступных пределах"):
    /// - the current page (loads it if different from the one shown);
    /// - the clean overlay (reloads the page when the shared model changed under us so the `Клин`
    ///   base layer reflects edits made on other tabs — raster layers are preserved across the
    ///   reload, the camera is re-applied below);
    /// - zoom + camera position, deferred via `pending_camera` until the page is loaded so the
    ///   async load's refit does not clobber it.
    ///
    /// `center_world` is the page-local source-pixel point to center; `None` keeps the fitted
    /// center and only applies the zoom.
    pub fn sync_view_from_canvas(
        &mut self,
        project: &ProjectData,
        page_idx: usize,
        zoom: f32,
        center_world: Option<Vec2>,
    ) {
        self.ensure_loader();
        let model_revision = self.overlay_model_revision();
        let page_changed = self.active_page_idx != Some(page_idx);
        let overlay_changed = model_revision != self.last_overlay_revision;
        if page_changed || overlay_changed {
            self.request_page(project, page_idx);
        }
        self.pending_camera = Some(CameraSync {
            page_idx,
            zoom,
            center_world,
        });
        self.apply_pending_camera();
    }

    /// Applies a pending synced camera once its target page is the loaded one.
    ///
    /// Deferred until no load is in flight: a reload (page change or clean-overlay refresh) calls
    /// `viewport.invalidate`, so applying before the reload settles would be overwritten by the
    /// post-load refit. Applying after `poll_loader` has cleared `pending_job_id` re-marks the
    /// camera initialized so the later `fit_page_if_needed` is a no-op.
    fn apply_pending_camera(&mut self) {
        let Some(sync) = self.pending_camera else {
            return;
        };
        if self.pending_job_id.is_some() || self.active_page_idx != Some(sync.page_idx) {
            return;
        }
        let Some(stack) = &self.stack else {
            return;
        };
        let size = stack.size();
        let (w, h) = (size[0] as f32, size[1] as f32);
        // Absent center -> page center; otherwise clamp the synced point into the page bounds.
        let center = sync.center_world.map_or_else(
            || Vec2::new(w * 0.5, h * 0.5),
            |c| Vec2::new(c.x.clamp(0.0, w), c.y.clamp(0.0, h)),
        );
        self.viewport.set_camera(sync.zoom, center);
        self.pending_camera = None;
    }

    /// Reads the shared clean-overlay model revision (0 when no model is bound).
    fn overlay_model_revision(&self) -> u64 {
        self.overlays_model
            .as_ref()
            .and_then(|model| model.lock().ok().map(|locked| locked.revision()))
            .unwrap_or(0)
    }

    /// Lazily starts the page loader worker once the model is known.
    fn ensure_loader(&mut self) {
        if self.loader.is_some() {
            return;
        }
        if let Some(model) = &self.overlays_model {
            self.loader = Some(spawn_page_loader_thread(Arc::clone(model)));
        }
    }

    /// Requests loading of `page_idx`, persisting the page being left first.
    fn request_page(&mut self, project: &ProjectData, page_idx: usize) {
        let Some(page) = project.pages.iter().find(|p| p.idx == page_idx) else {
            return;
        };
        // Persist the page we are leaving (committed edits are already flushed; this catches any
        // model state not yet written). The new page reloads fresh from disk + the shared doc.
        self.persist_current_page(project);
        // Undo history is per-page-session: a recorded diff is only valid while its page's layer image
        // buffers are resident, and the new page rebuilds the stack from scratch. Drop it (and any
        // in-progress brush-stroke union) so an undo cannot apply to the wrong page's pixels.
        self.history.clear();
        self.brush_stroke_dirty = None;
        // Drop any in-progress gesture snapshots too, so a gesture straddling a page switch cannot
        // record an undo step against the wrong page.
        self.opacity_gesture = None;
        self.transform_gesture_before = None;
        self.deform_gesture_before = None;
        // Same reason for the tool's own gesture: an outline traced on the page being left holds
        // that page's image coordinates (and a combination mode sampled there), so it must never
        // reach the next page's selection.
        self.reset_active_tool();
        if self.loader.is_none() {
            return;
        }
        let job_id = self.next_job_id;
        self.next_job_id += 1;
        self.pending_job_id = Some(job_id);
        self.requested_page_idx = Some(page_idx);
        self.load_error = None;
        // The worker decodes the persisted user-layer payload off-thread, so it needs the layer dirs
        // and the FULL chapter page-size map (the doc's legacy ribbon migration requires every page's
        // aspect ratio). Capturing the page paths here keeps the borrow off `loader` (so we can call
        // `page_sizes_map(&mut self)`); re-borrow the loader after building the request.
        let page_path = page.path.clone();
        let unsaved_layers_dir = project.paths.unsaved_layers_dir.clone();
        let layers_dir = project.paths.layers_dir.clone();
        // Gate the legacy `text_images/` fallback ONCE per chapter, cached by the committed layers dir:
        // a migrated chapter (inline text present) decodes with `None`; an un-migrated one feeds the
        // legacy dir so its text reaches the doc. Recomputing on every page switch would re-parse
        // `layers.json` on the GUI thread — the cache keeps that off the hot path.
        let legacy_text_dir = if let Some((cached_layers_dir, cached_legacy_text_dir)) =
            &self.doc_legacy_text_dir_cache
            && cached_layers_dir == &project.paths.layers_dir
        {
            cached_legacy_text_dir.clone()
        } else {
            let gated = if crate::models::layer_model::migrate::manifest_has_inline_text(
                &project.paths.layers_dir,
            ) {
                None
            } else {
                Some(project.paths.text_images_dir.clone())
            };
            self.doc_legacy_text_dir_cache =
                Some((project.paths.layers_dir.clone(), gated.clone()));
            gated
        };
        let page_sizes = self.page_sizes_map(project);
        crate::trace_log!(
            cat::PERSIST,
            "page_load request job={} page={}",
            job_id,
            page_idx
        );
        let Some(loader) = &self.loader else {
            return;
        };
        let _ = loader.request_tx.send(Some(PageLoadRequest {
            job_id,
            page_idx,
            page_path,
            unsaved_layers_dir,
            layers_dir,
            legacy_text_dir,
            page_sizes,
        }));
    }

    /// Writes the current page's raster layers to the unsaved staging dir (`*_unsaved/layers/`).
    ///
    /// Base layers are skipped (they mirror `src/` and `clean_layers/`). Called when leaving a page
    /// and on an explicit project save; previously-visited pages were already written on their own
    /// page switch.
    ///
    /// This path is NOT redundant with the per-edit `route_to_doc` enqueue: it reads from the PS
    /// `self.stack` (not the doc) and carries the EXPLICIT `removed_uids` from `self.deleted_raster_uids`.
    /// The doc's `enqueue_page_save` passes an empty removed set, so it would PRESERVE a raster the PS
    /// editor deleted as "another tab's" — a deleted raster would resurrect on disk. So this builds an
    /// OWNED save job (raster part only — no effects reconcile, mirroring the sync `save_page_rasters`
    /// that preserves another tab's effects) with the explicit removed set and enqueues it through the
    /// saver handle (moving the PNG encode off-thread) while preserving the EXACT deletion contract.
    /// Falls back to the synchronous `save_page_rasters` when no saver is enabled.
    fn persist_current_page(&mut self, project: &ProjectData) {
        let Some(stack) = &self.stack else {
            return;
        };
        let page_idx = stack.page_idx();
        let _s = crate::trace_scope!(cat::PERSIST, "persist_current_page page={}", page_idx);
        // Owned raster layers for the async job. `effects` is left empty + `display_image` None so the
        // saver's effects-reconcile loop is a no-op for these — mirroring the sync `save_page_rasters`
        // here, which never reconciles effects (it PRESERVES another tab's on-disk chain on a non-dirty
        // raster). PS does not own the typing-tab mask-clip flag; `None` preserves the on-disk value.
        let owned_layers: Vec<saver::OwnedRasterLayer> = stack
            .layers()
            .iter()
            .filter(|layer| layer.kind == LayerKind::Raster)
            .map(|layer| saver::OwnedRasterLayer {
                uid: layer.uid.to_string(),
                name: layer.name.clone(),
                visible: layer.visible,
                opacity: layer.opacity,
                transform: transform_to_rec(layer.transform),
                deform: layer.deform.clone(),
                group_uid: layer
                    .group
                    .and_then(|gid| stack.group(gid).map(|g| g.uid.to_string())),
                base_image: layer.image.clone(),
                pixels_dirty: layer.pixels_dirty,
                mask_clip: None,
                display_image: None,
                effects: Vec::new(),
            })
            .collect();
        let groups: Vec<persist::GroupMeta> = stack
            .groups()
            .iter()
            .map(|g| persist::GroupMeta {
                uid: g.uid.to_string(),
                name: g.name.clone(),
                visible: g.visible,
                opacity: g.opacity,
                collapsed: g.collapsed,
            })
            .collect();
        let removed_uids: Vec<String> = self
            .deleted_raster_uids
            .get(&page_idx)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();
        let raster_count = owned_layers.len();
        let group_count = groups.len();
        let removed_count = removed_uids.len();

        // Try the off-thread path: capture the saver handle (clone, no doc lock held during the write).
        let saver = self.layer_doc.as_ref().and_then(|doc| {
            doc.lock().ok().and_then(|mut guard| {
                guard.saver_handle().map(|handle| {
                    let epoch = guard.next_save_epoch(page_idx, saver::SaveKind::Raster);
                    (handle, epoch)
                })
            })
        });
        let persist_result: Result<(), String> = if let Some((handle, raster_epoch)) = saver {
            handle.enqueue(saver::PageSaveJob {
                page_idx,
                layers_dir: project.paths.unsaved_layers_dir.clone(),
                fallback_dir: Some(project.paths.layers_dir.clone()),
                raster: Some(saver::RasterSavePart {
                    layers: owned_layers,
                    groups,
                    removed_uids,
                }),
                raster_epoch: Some(raster_epoch),
                text: None,
                text_epoch: None,
                effects: Vec::new(),
            });
            Ok(())
        } else {
            // No saver: synchronous fallback, byte-identical to the previous direct call.
            let outs: Vec<persist::RasterLayerOut> = owned_layers
                .iter()
                .map(saver::OwnedRasterLayer::as_out)
                .collect();
            persist::save_page_rasters(
                &project.paths.unsaved_layers_dir,
                page_idx,
                &outs,
                &groups,
                &removed_uids,
            )
        };

        match persist_result {
            Ok(()) => {
                crate::trace_log!(
                    cat::PERSIST,
                    "persist_current_page saved page={} rasters={} groups={} removed={}",
                    page_idx,
                    raster_count,
                    group_count,
                    removed_count
                );
                // The deletions are captured by the enqueued job (or already written); stop carrying
                // them so a later flush does not re-emit the now-dropped removed set.
                self.deleted_raster_uids.remove(&page_idx);
                // This page's persist is enqueued/written; the tab-switch flush is redundant for it.
                self.layers_dirty = false;
                // Base PNGs are (being) written: clear `pixels_dirty` so a later flush (e.g. on project
                // save) treats these rasters as clean and preserves a non-destructive effects chain
                // the typing tab added in the meantime, instead of rewriting the base and dropping it.
                if let Some(stack) = self.stack.as_mut() {
                    stack.mark_rasters_persisted();
                }
                // No cross-tab signal needed: this is pure persistence of state whose MODEL changes
                // already routed through the shared doc (bumping its version).
            }
            Err(err) => crate::runtime_log::log_error(format!(
                "[ps_editor] persist layers (page {page_idx}): {err}"
            )),
        }
    }

    /// Flushes the active page's raster layers to disk. Call before merging the unsaved staging
    /// folder into the project on "save to project".
    pub fn flush_layers(&mut self, project: &ProjectData) {
        let _s = crate::trace_scope!(cat::PERSIST, "flush_layers");
        self.persist_current_page(project);
    }

    /// Flushes the active page only when an `edit_doc_node` mutation deferred persistence since the
    /// last flush (tracked by `layers_dirty`; the per-edit `route_to_doc` path enqueues immediately
    /// and clears it). Used on a PS→other-tab switch so an unchanged page does not pay a redundant
    /// snapshot+enqueue every switch. `flush_layers` clears the flag via `persist_current_page`.
    pub fn flush_layers_if_dirty(&mut self, project: &ProjectData) {
        if !self.layers_dirty {
            return;
        }
        self.flush_layers(project);
    }

    /// Records that a raster node was removed from the stack this session so the next page save
    /// drops it from the manifest (`save_page_rasters` otherwise preserves rasters it does not own)
    /// and the merge does not resurrect it. No-op for non-raster layers.
    fn record_raster_deletion(&mut self, layer_id: LayerId) {
        let Some(stack) = self.stack.as_ref() else {
            return;
        };
        let page_idx = stack.page_idx();
        let Some(layer) = stack.layer(layer_id) else {
            return;
        };
        if layer.kind != LayerKind::Raster {
            return;
        }
        let uid = layer.uid.to_string();
        self.deleted_raster_uids
            .entry(page_idx)
            .or_default()
            .insert(uid);
    }

    /// Drains finished load jobs, building a fresh stack for the matching page.
    fn poll_loader(&mut self, project: &ProjectData) {
        let Some(loader) = &self.loader else {
            return;
        };
        let mut latest: Option<page_loader::PageLoadResult> = None;
        while let Ok(result) = loader.result_rx.try_recv() {
            latest = Some(result);
        }
        let Some(result) = latest else {
            return;
        };
        if Some(result.job_id) != self.pending_job_id {
            return;
        }
        self.pending_job_id = None;
        let _s = crate::trace_scope!(
            cat::PERSIST,
            "page_load complete job={} page={}",
            result.job_id,
            result.page_idx
        );
        match result.outcome {
            Ok(page) => {
                crate::trace_log!(
                    cat::SYNC,
                    "page_load base_ready page={} size=[{},{}]",
                    result.page_idx,
                    page.size[0],
                    page.size[1]
                );
                // Build a fresh stack with only the two base layers; the user raster layers are
                // materialized below by `sync_view_from_doc` from the shared `LayerDoc` (the post-
                // refactor source of truth), so there is NO separate `load_persisted_into_stack` decode
                // here — the raster PNGs were already decoded ONCE off-thread by the worker.
                let stack = LayerStack::new(result.page_idx, page.size, page.source, page.clean);
                self.stack = Some(stack);
                // Seed the loaded page's size from the freshly-built stack (authoritative) so a memoized
                // header read can't disagree with the worker's page-size map.
                if let Some(size) = self.stack.as_ref().map(|s| s.size()) {
                    self.page_sizes_px.insert(result.page_idx, size);
                }
                // Move the worker-decoded user-layer payload into the shared doc under a BRIEF lock
                // (no decode is performed here — the heavy PNG decode already ran lock-free on the
                // worker). `insert_decoded_page` is memoized: if the page was already resident (e.g. a
                // concurrent edit between request and insert), it discards the payload and keeps the
                // live in-memory page. If the worker's decode failed (`layers == None`), the page is
                // left un-inserted and the projection below shows just the base layers.
                if let Some(payload) = page.layers {
                    if let Some(doc) = &self.layer_doc
                        && let Ok(mut doc) = doc.lock()
                    {
                        doc.insert_decoded_page(result.page_idx, payload);
                    }
                } else {
                    crate::runtime_log::log_warn(format!(
                        "[ps_editor] page {} loaded without a layer payload (decode failed on worker)",
                        result.page_idx
                    ));
                }
                // Load the disk-truth text-layer metadata + bands (pin / text-group axis the doc node
                // does not carry), then project the doc over the stack rasters / text / bands so both
                // tabs read one model.
                self.reload_overlays_view(project, result.page_idx);
                self.active_page_idx = Some(result.page_idx);
                self.selection = None;
                // Panel selection keys on session ids that the new page's stack reuses; reset it.
                self.panel_selection.clear();
                self.panel_anchor = None;
                self.panel_primary = None;
                self.render_cache.clear();
                // A fresh page: forget prior per-node generations so the first projection uploads.
                self.node_generations
                    .retain(|(p, _), _| *p == result.page_idx);
                self.viewport.invalidate();
                self.last_overlay_revision = self.overlay_model_revision();
                self.load_error = None;
                // Project the shared doc over the freshly-loaded stack / text / bands.
                self.sync_view_from_doc(result.page_idx);
            }
            Err(err) => {
                crate::trace_log!(
                    cat::PERSIST,
                    "page_load failed page={} err={}",
                    result.page_idx,
                    err
                );
                crate::runtime_log::log_error(format!("[ps_editor] page load failed: {err}"));
                self.load_error = Some(err);
            }
        }
    }

    /// Index of the active tool in `self.tools` for a given id.
    fn tool_index(&self, id: PsToolId) -> Option<usize> {
        self.tools.iter().position(|tool| tool.id() == id)
    }

    fn active_tool_id(&self) -> PsToolId {
        self.tools[self.active_tool_idx].id()
    }

    /// Switches the active tool, abandoning the OUTGOING tool's in-progress gesture first.
    ///
    /// Every write to `active_tool_idx` must go through here (toolbar click and B/M/L/V hotkeys):
    /// a gesture that ends with the button UP — a pending lasso polygon — otherwise survives the
    /// switch and commits on some later frame, with a combination mode sampled long before. Out
    /// of range or already-active indices are ignored, so a caller may pass any candidate.
    fn set_active_tool(&mut self, idx: usize) {
        if idx >= self.tools.len() || idx == self.active_tool_idx {
            return;
        }
        if let Some(outgoing) = self.tools.get_mut(self.active_tool_idx) {
            outgoing.reset();
        }
        self.active_tool_idx = idx;
    }

    /// Abandons the active tool's in-progress gesture, keeping the tool selected.
    ///
    /// Used on a page switch: the outline's image coordinates belong to the page being left, so
    /// committing it against the next page would paint a selection nowhere near where it was drawn.
    fn reset_active_tool(&mut self) {
        if let Some(tool) = self.tools.get_mut(self.active_tool_idx) {
            tool.reset();
        }
    }

    /// Stores `selection` as the page selection, enforcing "a selection is `Some` only when it
    /// selects at least one pixel".
    ///
    /// An all-zero mask stored as `Some` is invisible (`Selection::any` is false, so the marquee
    /// draws nothing) yet still clips the brush, which only tests `Option::is_some` — the user
    /// sees a tool that silently refuses to paint until Ctrl+D. Reachable whenever a footprint
    /// falls entirely outside the page (a raster moved off-page, an off-page text row).
    fn set_selection(&mut self, selection: Selection) {
        self.selection = non_empty_selection(selection);
    }

    /// Main per-frame entry point. Renders the whole tab inside the provided `ui`.
    ///
    /// `panel_dock` is the APPLICATION's single dock state, lent in for this frame: it is
    /// deliberately not a field of this tab, because `PanelDock::begin` borrows it for the whole
    /// frame and the tab bodies need `&mut Self` at the same time.
    ///
    /// Frame order, and every step of it is load-bearing:
    /// 1. one `area_rect` is resolved once and serves as BOTH the dock area and the canvas rect;
    /// 2. the panel-visibility flags are read off the dock state BEFORE the tabs are declared, so
    ///    a hidden panel never blinks into view for a frame;
    /// 3. the five tabs are declared and drawn (every one of them on every frame — a hidden tab
    ///    keeps its slot, a skipped declaration would lose it);
    /// 4. this frame's panel rects are stored and the deferred layer actions applied;
    /// 5. the canvas is drawn LAST, over the same rect. It still ends up UNDERNEATH: panels live
    ///    on `Order::Foreground` areas while the canvas paints into this `Ui`'s `Order::Background`
    ///    layer, and egui composites by layer order, not by call order
    ///    (`egui-docs/06-overlays.md` §1). Drawing it last is what lets its input gate use THIS
    ///    frame's panel rects instead of the previous frame's.
    pub fn draw(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        project: &ProjectData,
        panel_dock: &mut PanelDockState,
    ) {
        // Per-frame span. Detailed events inside the editor are gated on real state changes
        // (page load, doc-version change, tool activity) so an idle frame stays quiet.
        let _frame = crate::trace_scope!(cat::FRAME, "ps_draw page={:?}", self.active_page_idx);
        self.ensure_loader();
        self.poll_loader(project);
        // Consume any finished non-destructive raster-effects render (computed off the GUI thread).
        if self.poll_ps_raster_effects_jobs(project) {
            ctx.request_repaint();
        }
        self.refresh_view_if_doc_version_changed(project);
        // A synced camera waits here until its page finishes loading (the load refits otherwise).
        self.apply_pending_camera();

        // Kick off the first page once the loader is ready.
        if self.active_page_idx.is_none()
            && self.pending_job_id.is_none()
            && self.loader.is_some()
            && let Some(first) = project.pages.first().map(|p| p.idx)
        {
            self.request_page(project, first);
        }

        // One rect for the dock and the canvas alike. `canvas::dock_area_rect` is not used here:
        // it reserves the shared canvas' scrollbar strip, and this tab has no canvas scrollbar.
        let area_rect = ui.available_rect_before_wrap();
        let layout_key = AppTab::PsEditor.key();
        panel_dock.ensure_default_layout(layout_key, ps_editor_default_dock_layout);
        let visibility = PsEditorPanelVisibility::read(panel_dock, layout_key);

        let mut cx = PsEditorDockCx {
            tab: self,
            project,
            panel_actions: PanelActions::default(),
            visibility,
        };
        let mut dock = PanelDock::begin(
            ctx,
            panel_dock,
            DockArea {
                rect: area_rect,
                layout_key,
            },
        );
        // Declared WITH its extras: the «Панели…» menu writes the four visibility flags into the
        // main tab's bag, and that write is the only thing that persists them.
        dock.tab(PS_EDITOR_MAIN_TAB)
            .title(|| t!("ps_editor.tab.main"))
            .min_size(PS_EDITOR_MAIN_TAB_MIN_SIZE_PX)
            .initial_size(PS_EDITOR_MAIN_TAB_INITIAL_SIZE_PX)
            .show_with_extras(draw_main_tab_body);
        // The four secondary tabs are declared on EVERY frame regardless of `visible`: a hidden tab
        // keeps its slot in the layout and only its panel is skipped, so re-enabling it returns it
        // to wherever the user put it. Skipping the declaration would make the dock treat it as
        // another program tab's and seed it a fresh panel on the next open.
        dock.tab(PS_EDITOR_TOOLS_TAB)
            .title(|| t!("ps_editor.tab.tools"))
            .visible(visibility.tools)
            .min_size(PS_EDITOR_TOOLS_TAB_MIN_SIZE_PX)
            .initial_size(PS_EDITOR_TOOLS_TAB_INITIAL_SIZE_PX)
            .show(draw_tools_tab_body);
        dock.tab(PS_EDITOR_ACTIVE_TOOL_TAB)
            .title(|| t!("ps_editor.tab.active_tool"))
            .visible(visibility.active_tool)
            .min_size(PS_EDITOR_ACTIVE_TOOL_TAB_MIN_SIZE_PX)
            .initial_size(PS_EDITOR_ACTIVE_TOOL_TAB_INITIAL_SIZE_PX)
            .show(draw_active_tool_tab_body);
        dock.tab(PS_EDITOR_HOTKEYS_TAB)
            .title(|| t!("ps_editor.tab.hotkeys"))
            .visible(visibility.hotkeys)
            .min_size(PS_EDITOR_HOTKEYS_TAB_MIN_SIZE_PX)
            .initial_size(PS_EDITOR_HOTKEYS_TAB_INITIAL_SIZE_PX)
            .show(draw_hotkeys_tab_body);
        dock.tab(PS_EDITOR_LAYERS_TAB)
            .title(|| t!("ps_editor.tab.layers"))
            .visible(visibility.layers)
            .min_size(PS_EDITOR_LAYERS_TAB_MIN_SIZE_PX)
            .initial_size(PS_EDITOR_LAYERS_TAB_INITIAL_SIZE_PX)
            .show(draw_layers_tab_body);
        // MAIN-WINDOW panels only, by construction: `drawn_panels` never reports a panel the user
        // detached into a sub-window, whose rect lives in that window's own frame and would carve a
        // dead zone out of this window's top-left corner (`PanelDockOutput`).
        let out = dock.end(&mut cx);
        let panel_rects: Vec<Rect> = out.drawn_panels().map(|(_, rect)| rect).collect();
        let panel_actions = std::mem::take(&mut cx.panel_actions);

        self.panel_rects = panel_rects;
        self.apply_panel_actions(panel_actions, project);
        self.draw_effects_editor(ctx);
        self.draw_canvas(ctx, ui, area_rect, project);
    }

    /// Everything the «PS редактор» tab shows except its «Панели…» menu: the page switch, the two
    /// zoom presets, the zoom readout, the load / effects spinners and the last load error.
    ///
    /// The row WRAPS: the dock body scrolls both axes, and a panel the user narrows must break the
    /// row rather than push the buttons behind a horizontal scrollbar.
    fn main_tab_contents(&mut self, ui: &mut egui::Ui, project: &ProjectData) {
        ui.horizontal_wrapped(|ui| {
            ui.label(t!("ps_editor.top_bar.page_label"));
            let page_indices: Vec<usize> = project.pages.iter().map(|p| p.idx).collect();
            let current = self.active_page_idx.or(self.requested_page_idx);
            let current_pos = current.and_then(|idx| page_indices.iter().position(|&p| p == idx));

            let prev_enabled = current_pos.map(|p| p > 0).unwrap_or(false);
            if ui
                .add_enabled(prev_enabled, egui::Button::new("◀"))
                .clicked()
                && let Some(pos) = current_pos
            {
                self.request_page(project, page_indices[pos - 1]);
            }

            let label = current
                .map(|i| (i + 1).to_string())
                .unwrap_or_else(|| "—".into());
            ui.label(format!("{label} / {}", page_indices.len().max(1)));

            let next_enabled = current_pos
                .map(|p| p + 1 < page_indices.len())
                .unwrap_or(false);
            if ui
                .add_enabled(next_enabled, egui::Button::new("▶"))
                .clicked()
                && let Some(pos) = current_pos
            {
                self.request_page(project, page_indices[pos + 1]);
            }

            ui.separator();
            // Both refit using the real canvas rect, resolved in `draw_canvas`.
            if ui.button(t!("ps_editor.top_bar.fit_button")).clicked() {
                self.viewport.invalidate();
            }
            if ui.button("100%").clicked() {
                self.pending_actual_size = true;
            }
            ui.label(tf!(
                "ps_editor.top_bar.zoom",
                percent = format!("{:.0}", self.viewport.zoom() * 100.0)
            ));

            if self.pending_job_id.is_some() {
                ui.spinner();
                ui.label(t!("ps_editor.top_bar.loading_page"));
            }
            if self.raster_effects_state.is_some() {
                ui.spinner();
                ui.label(t!("ps_editor.top_bar.applying_effects"));
            }
            if let Some(err) = &self.load_error {
                ui.colored_label(Color32::from_rgb(220, 80, 80), err);
            }
        });
    }

    /// «Инструменты» body: the tool selector grouped into `PsToolSection`s, plus the two
    /// whole-layer selection actions.
    ///
    /// The grouping is a DRAW-TIME regrouping only: `active_tool_idx` indexes `self.tools`, so the
    /// registration order of that vector must never be disturbed here. Within a section the tools
    /// keep their registration order, and a section with no registered tool renders nothing.
    ///
    /// The active tool's own parameters live in the separate «Выбранный инструмент» tab.
    fn tools_tab_contents(&mut self, ui: &mut egui::Ui) {
        for section in PsToolSection::ORDER {
            // The section heading is emitted lazily, on the first tool that belongs to it,
            // so an empty section leaves no dangling header.
            let mut heading_drawn = false;
            for index in 0..self.tools.len() {
                if self.tools[index].id().section() != section {
                    continue;
                }
                if !heading_drawn {
                    // Subordinate to the panel caption: a strong label, not a `heading`
                    // (the project's section-label idiom).
                    ui.label(egui::RichText::new(section.title()).strong());
                    heading_drawn = true;
                }
                let selected = index == self.active_tool_idx;
                let title = self.tools[index].title();
                if ui.selectable_label(selected, title).clicked() {
                    self.set_active_tool(index);
                }
            }
        }

        ui.separator();
        if ui
            .button(t!("ps_editor.toolbar.select_whole_layer"))
            .clicked()
        {
            self.select_active_layer_fully();
        }
        if ui.button(t!("ps_editor.toolbar.clear_selection")).clicked() {
            self.clear_selection();
        }
    }

    /// «Выбранный инструмент» body: the active tool's parameters, or the "none" line.
    ///
    /// `PsTool::has_options` is the only question asked — a tool that answers `false` has no
    /// `options_ui` at all (the trait method defaults to a no-op), so calling it anyway would print
    /// an empty panel instead of saying why it is empty.
    fn active_tool_tab_contents(&mut self, ui: &mut egui::Ui) {
        let Some(tool) = self.tools.get_mut(self.active_tool_idx) else {
            ui.label(t!("ps_editor.active_tool.no_options"));
            return;
        };
        if tool.has_options() {
            tool.options_ui(ui);
        } else {
            ui.label(t!("ps_editor.active_tool.no_options"));
        }
    }

    /// «Горячие клавиши» body: the active tool's own inventory, then the tab-level shortcuts.
    ///
    /// The tool section is rebuilt from `PsTool::hotkey_rows` on every frame, so a runtime language
    /// switch and a tool switch are both reflected without any invalidation path.
    fn hotkeys_tab_contents(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new(t!("ps_editor.hotkeys.tool_section")).strong());
        let tool_rows = self
            .tools
            .get(self.active_tool_idx)
            .map(|tool| tool.hotkey_rows())
            .unwrap_or_default();
        if tool_rows.is_empty() {
            ui.label(t!("ps_editor.hotkeys.none"));
        } else {
            draw_hotkey_rows_grid(ui, "ps_editor_hotkeys_tool", &tool_rows);
        }

        ui.separator();
        ui.label(egui::RichText::new(t!("ps_editor.hotkeys.common_section")).strong());
        draw_hotkey_rows_grid(
            ui,
            "ps_editor_hotkeys_common",
            &ps_editor_common_hotkey_rows(),
        );
    }

    /// Sets the selection to the active layer's full footprint. When the panel's primary row is a TEXT
    /// layer, the footprint is that overlay's page-space outline (`PsTextLayer::footprint_polygon`),
    /// since text layers are NOT in `LayerStack`. Otherwise it falls back to the stack's active layer: a
    /// page rectangle for a base / page-sized identity layer, or the (possibly rotated/scaled) image
    /// quad for a transformed raster layer.
    fn select_active_layer_fully(&mut self) {
        let Some(page) = self.stack.as_ref().map(LayerStack::size) else {
            return;
        };
        // Text layers live outside the stack: use the primary-row text overlay's footprint directly.
        if let Some(RowSel::Text(uid)) = &self.panel_primary {
            // Collect the polygon before borrowing `self.selection` mutably (no overlapping borrows).
            let polygon = self
                .text_layers
                .iter()
                .find(|t| t.uid() == uid)
                .map(PsTextLayer::footprint_polygon);
            if let Some(polygon) = polygon {
                let mut selection = Selection::empty(page[0], page[1]);
                selection.set_polygon(&polygon);
                // An overlay dragged fully off the page has an empty footprint: store `None`, never
                // an invisible all-zero mask that would keep clipping the brush.
                self.set_selection(selection);
            }
            return;
        }
        let Some(stack) = self.stack.as_ref() else {
            return;
        };
        let active = stack.active_id();
        let Some(layer) = stack.layer(active) else {
            return;
        };
        let mut selection = Selection::empty(page[0], page[1]);
        if layer.image.size == page && layer.transform.is_identity_for(layer.image.size) {
            selection.set_rect(0, 0, page[0] as i32, page[1] as i32);
        } else {
            let pts: Vec<(f32, f32)> = layer.world_corners().iter().map(|p| (p.x, p.y)).collect();
            selection.set_polygon(&pts);
        }
        // Same invariant as above: a raster moved entirely off-page selects nothing at all.
        self.set_selection(selection);
    }

    /// «Слои» body: the unified, Photoshop-like layer tree. Compact rows (eye + name + group
    /// indent), collapsible/movable groups that may mix rasters and texts, Shift/Ctrl multi-select,
    /// a right-click menu for grouping, and a controls strip for the active layer.
    ///
    /// Returns the deferred actions; the caller applies them with `apply_panel_actions` after the
    /// dock frame. The tree + per-row data are snapshotted into owned values first, so the render
    /// loop can mutate `self.panel_selection` without holding any borrow of `self.stack` /
    /// `self.text_layers`.
    fn layers_panel_body(&mut self, ui: &mut egui::Ui) -> PanelActions {
        let mut actions = PanelActions::default();
        if self.stack.is_none() {
            ui.label(t!("ps_editor.layers_panel.no_page"));
            return actions;
        }

        ui.horizontal(|ui| {
            if ui
                .button(t!("ps_editor.layers_panel.add_layer_button"))
                .clicked()
            {
                actions.add_layer = true;
            }
            if ui
                .button(t!("ps_editor.layers_panel.add_group_button"))
                .clicked()
            {
                actions.new_empty_group = true;
            }
        });
        ui.separator();

        // Owned snapshot: rows (top-to-bottom), the selectable-row order (for Shift range), and the
        // existing-group list (for the "move to group" submenu).
        let rows = self.build_panel_rows();
        let row_sels: Vec<RowSel> = rows
            .iter()
            .filter_map(|r| match r {
                PanelRow::Group(h) => Some(RowSel::Group(h.uid.clone())),
                PanelRow::Leaf(l) => l.sel.clone(),
            })
            .collect();
        let group_list: Vec<(String, String)> = self
            .stack
            .as_ref()
            .map(|s| {
                s.groups()
                    .iter()
                    .map(|g| (g.uid.to_string(), g.name.clone()))
                    .collect()
            })
            .unwrap_or_default();

        let selection = self.panel_selection.clone();

        // NO nested scroll area and NO hand-computed height reserve here. The dock already draws
        // every tab body inside a bounded `ScrollArea::both` and derives the panel's size request
        // from the CONTENT's measured height, so a fixed-height inner scroll area would fight that
        // measurement and make the panel oscillate or never grow
        // (`panel_dock/MODULE_README.md`, «The body FILLS its budget…»). The intended height is
        // expressed by `PS_EDITOR_LAYERS_TAB_MIN_SIZE_PX` / `..._INITIAL_SIZE_PX` instead.
        for row in &rows {
            match row {
                PanelRow::Group(h) => {
                    self.draw_group_row(ui, h, &selection, &mut actions);
                }
                PanelRow::Leaf(leaf) => {
                    self.draw_leaf_row(ui, leaf, &selection, &row_sels, &group_list, &mut actions);
                }
            }
        }

        ui.separator();
        self.draw_active_controls(ui, &mut actions);
        actions
    }

    /// Builds the owned per-row snapshot from the unified tree + stack + text layers.
    fn build_panel_rows(&self) -> Vec<PanelRow> {
        let Some(stack) = self.stack.as_ref() else {
            return Vec::new();
        };
        let tree = tree::build_unified_tree(stack, &self.text_layers, &self.bands);
        tree.into_iter()
            .map(|item| match item {
                tree::TreeItem::Group(h) => PanelRow::Group(h),
                tree::TreeItem::Leaf(leaf) => {
                    let (sel, name, visible, is_base) = match &leaf.kind {
                        tree::LeafKind::Base(id) => {
                            let l = stack.layer(*id);
                            (
                                None,
                                l.map_or_else(
                                    || t!("ps_editor.layers_panel.leaf_fallback_layer").into(),
                                    |l| l.name.clone(),
                                ),
                                l.is_some_and(|l| l.visible),
                                true,
                            )
                        }
                        tree::LeafKind::Raster(id) => {
                            let l = stack.layer(*id);
                            (
                                Some(RowSel::Raster(*id)),
                                l.map_or_else(
                                    || t!("ps_editor.layers_panel.leaf_fallback_raster").into(),
                                    |l| l.name.clone(),
                                ),
                                l.is_some_and(|l| l.visible),
                                false,
                            )
                        }
                        tree::LeafKind::Text(i) => {
                            let t = self.text_layers.get(*i);
                            // Show a text preview (`Текст (preview)`) using the same logic as the typing
                            // tab; fall back to the stored node name when the overlay has no text. The
                            // `🅣` icon is added later in `draw_leaf_row`, so it is omitted here.
                            let name = t.map_or_else(
                                || t!("ps_editor.layers_panel.leaf_fallback_text").into(),
                                |t| {
                                    let preview = crate::tabs::typing::text_preview_label(
                                        &t.text_content,
                                        PS_TEXT_PREVIEW_CHARS,
                                    );
                                    if preview.is_empty() {
                                        t.name.clone()
                                    } else {
                                        tf!(
                                            "ps_editor.layers_panel.text_preview",
                                            preview = preview
                                        )
                                    }
                                },
                            );
                            (
                                t.map(|t| RowSel::Text(t.uid.clone())),
                                name,
                                t.is_some_and(|t| t.visible),
                                false,
                            )
                        }
                    };
                    PanelRow::Leaf(PanelLeaf {
                        sel,
                        kind: leaf.kind,
                        depth: leaf.depth,
                        name,
                        visible,
                        is_base,
                    })
                }
            })
            .collect()
    }

    /// One group-header row: collapse arrow, visibility eye, name, block move arrows, context menu.
    fn draw_group_row(
        &mut self,
        ui: &mut egui::Ui,
        header: &tree::GroupHeader,
        selection: &HashSet<RowSel>,
        actions: &mut PanelActions,
    ) {
        let selected = selection.contains(&RowSel::Group(header.uid.clone()));
        let resp = ui
            .horizontal(|ui| {
                ui.add_space(header.depth as f32 * tree::INDENT);
                let arrow = if header.collapsed { "▸" } else { "▾" };
                if ui
                    .add(egui::Button::new(arrow).small().frame(false))
                    .clicked()
                {
                    actions.group_op = Some(GroupOp::ToggleCollapse(header.uid.clone()));
                }
                let mut vis = header.visible;
                if ui.checkbox(&mut vis, "").changed() {
                    actions.group_op = Some(GroupOp::ToggleGroupVisible(header.uid.clone()));
                }
                let label = ui.selectable_label(selected, format!("📁 {}", header.name));
                if ui.add(egui::Button::new("▲").small()).clicked() {
                    actions.group_op = Some(GroupOp::MoveGroup(header.uid.clone(), true));
                }
                if ui.add(egui::Button::new("▼").small()).clicked() {
                    actions.group_op = Some(GroupOp::MoveGroup(header.uid.clone(), false));
                }
                label
            })
            .inner;
        if resp.clicked() {
            let mods = ui.input(|i| i.modifiers);
            self.select_row(RowSel::Group(header.uid.clone()), mods, &[]);
        }
        egui::Popup::context_menu(&resp)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                if ui
                    .button(t!("ps_editor.layers_panel.delete_group_button"))
                    .clicked()
                {
                    actions.group_op = Some(GroupOp::DeleteGroup(header.uid.clone()));
                    egui::Popup::close_all(ui.ctx());
                }
            });
    }

    /// One leaf row (raster / text / locked base): visibility eye + indented name + selection +
    /// right-click grouping menu.
    fn draw_leaf_row(
        &mut self,
        ui: &mut egui::Ui,
        leaf: &PanelLeaf,
        selection: &HashSet<RowSel>,
        row_sels: &[RowSel],
        group_list: &[(String, String)],
        actions: &mut PanelActions,
    ) {
        let selected = leaf.sel.as_ref().is_some_and(|s| selection.contains(s));
        let icon = match leaf.kind {
            tree::LeafKind::Text(_) => "🅣",
            _ => "▦",
        };
        let resp = ui
            .horizontal(|ui| {
                ui.add_space(leaf.depth as f32 * tree::INDENT + tree::INDENT);
                // Visibility toggle (base layers can be hidden but not edited).
                let mut vis = leaf.visible;
                if ui.checkbox(&mut vis, "").changed() {
                    match &leaf.kind {
                        tree::LeafKind::Raster(id) | tree::LeafKind::Base(id) => {
                            actions.toggle_visible_raster = Some(*id);
                        }
                        tree::LeafKind::Text(i) => actions.toggle_visible_text = Some(*i),
                    }
                }
                let label = ui.selectable_label(selected, format!("{icon} {}", leaf.name));
                if leaf.is_base {
                    ui.label("🔒");
                }
                label
            })
            .inner;

        let Some(sel) = leaf.sel.clone() else {
            // Base layer: clicking just makes it active (no group ops).
            if resp.clicked()
                && let tree::LeafKind::Base(id) = leaf.kind
            {
                actions.set_active_raster = Some(id);
                // Base rows are not in `RowSel`, so they don't call `select_row` and would leave a
                // STALE `panel_primary`. If it still held a `RowSel::Text`, `select_active_layer_fully`
                // would draw that text overlay's marquee instead of the base layer's footprint. Clear
                // it so the selection falls through to the stack/`active_id()` (base) path.
                self.panel_primary = None;
                actions.request_select_active = true;
            }
            return;
        };

        if resp.clicked() {
            let mods = ui.input(|i| i.modifiers);
            self.select_row(sel.clone(), mods, row_sels);
            if let tree::LeafKind::Raster(id) = leaf.kind {
                actions.set_active_raster = Some(id);
            }
            // Cover raster AND text rows: show the clicked (primary) layer's marquee immediately.
            actions.request_select_active = true;
        }
        // Right-click acts on the current multi-selection; if this row is not selected, select it.
        if resp.secondary_clicked() && !self.panel_selection.contains(&sel) {
            self.select_row(sel.clone(), egui::Modifiers::default(), row_sels);
        }
        egui::Popup::context_menu(&resp)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                if !group_list.is_empty() {
                    ui.menu_button(t!("ps_editor.layers_panel.move_to_group"), |ui| {
                        for (uid, name) in group_list {
                            if ui.button(format!("📁 {name}")).clicked() {
                                actions.group_op = Some(GroupOp::MoveTo(uid.clone()));
                                egui::Popup::close_all(ui.ctx());
                            }
                        }
                    });
                }
                if ui
                    .button(t!("ps_editor.layers_panel.create_group_from_selection"))
                    .clicked()
                {
                    actions.group_op = Some(GroupOp::NewFromSelection);
                    egui::Popup::close_all(ui.ctx());
                }
                if ui
                    .button(t!("ps_editor.layers_panel.remove_from_group"))
                    .clicked()
                {
                    actions.group_op = Some(GroupOp::Ungroup);
                    egui::Popup::close_all(ui.ctx());
                }
            });
    }

    /// Applies a row click to the panel selection: plain = replace, Ctrl/Cmd = toggle, Shift = range
    /// over the displayed selectable rows (`row_sels`) from the anchor.
    fn select_row(&mut self, sel: RowSel, mods: egui::Modifiers, row_sels: &[RowSel]) {
        if mods.shift
            && !row_sels.is_empty()
            && let Some(anchor) = self.panel_anchor.clone()
        {
            let a = row_sels.iter().position(|r| *r == anchor);
            let b = row_sels.iter().position(|r| *r == sel);
            if let (Some(a), Some(b)) = (a, b) {
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                self.panel_selection = row_sels[lo..=hi].iter().cloned().collect();
                self.panel_primary = Some(sel);
                return;
            }
            // No usable anchor: fall through to a plain select.
        }
        if mods.command {
            if !self.panel_selection.remove(&sel) {
                self.panel_selection.insert(sel.clone());
            }
            self.panel_anchor = Some(sel.clone());
            self.panel_primary = Some(sel);
            return;
        }
        self.panel_selection.clear();
        self.panel_selection.insert(sel.clone());
        self.panel_anchor = Some(sel.clone());
        self.panel_primary = Some(sel);
    }

    /// Controls strip for the active row (`panel_primary`): opacity / merge / delete / fx for a
    /// raster, pin / rasterize for a text, opacity / delete for a group.
    fn draw_active_controls(&self, ui: &mut egui::Ui, actions: &mut PanelActions) {
        let Some(primary) = self.panel_primary.clone() else {
            ui.label(t!("ps_editor.active_controls.select_layer_hint"));
            return;
        };
        let Some(stack) = self.stack.as_ref() else {
            return;
        };
        match primary {
            RowSel::Raster(id) => {
                let Some(layer) = stack.layer(id) else {
                    return;
                };
                ui.label(format!("▦ {}", layer.name));
                let mut opacity = layer.opacity;
                if ui
                    .add(
                        crate::widgets::WheelSlider::new(&mut opacity, 0.0..=1.0)
                            .text(t!("ps_editor.active_controls.opacity_label")),
                    )
                    .changed()
                {
                    actions.opacity_raster = Some((id, opacity));
                }
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new("▲").small()).clicked() {
                        actions.move_band = Some((RowSel::Raster(id), true));
                    }
                    if ui.add(egui::Button::new("▼").small()).clicked() {
                        actions.move_band = Some((RowSel::Raster(id), false));
                    }
                    if ui.add(egui::Button::new("🗑").small()).clicked() {
                        actions.remove_raster = Some(id);
                    }
                    let mergeable = self.is_mergeable(id);
                    if ui
                        .add_enabled(mergeable, egui::Button::new("⤓").small())
                        .on_hover_text(t!("ps_editor.active_controls.merge_down"))
                        .clicked()
                    {
                        actions.merge_req = Some(id);
                    }
                    if ui
                        .add(egui::Button::new("fx").small())
                        .on_hover_text(t!("ps_editor.active_controls.effects_rasterize"))
                        .clicked()
                    {
                        actions.open_effects = Some(id);
                    }
                    // Bake a raster that is showing a non-destructive effects chain: flatten the
                    // render into the base pixels and clear the chain so it becomes directly editable.
                    if !layer.effects.is_empty()
                        && ui
                            .add(
                                egui::Button::new(t!("ps_editor.active_controls.bake_button"))
                                    .small(),
                            )
                            .on_hover_text(t!("ps_editor.active_controls.bake_tooltip"))
                            .clicked()
                    {
                        actions.bake_req = Some(id);
                    }
                });
            }
            RowSel::Text(uid) => {
                let Some((index, text)) = self
                    .text_layers
                    .iter()
                    .enumerate()
                    .find(|(_, t)| t.uid == uid)
                else {
                    return;
                };
                ui.label(format!("🅣 {}", text.name));
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new(if text.pinned { "📌" } else { "📍" }).small())
                        .on_hover_text(t!("ps_editor.active_controls.pin_z"))
                        .clicked()
                    {
                        actions.text_op = Some((index, TextLayerOp::TogglePin));
                    }
                    if ui
                        .add(egui::Button::new("⊞").small())
                        .on_hover_text(t!("ps_editor.active_controls.bake_into_layer"))
                        .clicked()
                    {
                        actions.text_op = Some((index, TextLayerOp::Rasterize));
                    }
                    // Every text is pinned-with-explicit-Z now (fully-manual unified Z), so the ⬆/⬇
                    // band-move is ALWAYS available — same path as rasters (`move_band` → `move_band_one`
                    // → `save_page_band_order` + `doc.set_z_order`), so the typing tab reflects it live.
                    if ui.add(egui::Button::new("▲").small()).clicked() {
                        actions.move_band = Some((RowSel::Text(uid.clone()), true));
                    }
                    if ui.add(egui::Button::new("▼").small()).clicked() {
                        actions.move_band = Some((RowSel::Text(uid.clone()), false));
                    }
                });
            }
            RowSel::Group(uid) => {
                let Some(group) = stack.group_by_uid(&uid) else {
                    return;
                };
                ui.label(format!("📁 {}", group.name));
                let mut opacity = group.opacity;
                if ui
                    .add(
                        crate::widgets::WheelSlider::new(&mut opacity, 0.0..=1.0)
                            .text(t!("ps_editor.active_controls.opacity_label")),
                    )
                    .changed()
                {
                    actions.group_op = Some(GroupOp::GroupOpacity(uid.clone(), opacity));
                }
                if ui
                    .add(
                        egui::Button::new(t!("ps_editor.layers_panel.delete_group_button")).small(),
                    )
                    .clicked()
                {
                    actions.group_op = Some(GroupOp::DeleteGroup(uid));
                }
            }
        }
    }

    /// Every legal merge-down TARGET as `(uid, band_z)` in stack order: the user rasters plus the
    /// `Клин` base layer, which sits below all of them.
    ///
    /// A raster's `band_z` is its `Band::Raster` Z from `self.bands` (or the past-the-top fallback
    /// for a raster without a band, mirroring `draw_composite`), SHIFTED UP BY ONE so that `Клин`
    /// can occupy the synthetic Z 0 strictly beneath every raster — the shift preserves the rasters'
    /// relative order exactly, and keeps the whole axis in `u32`. `Исходник` is excluded: it is
    /// immutable and must never become a merge target. The order within the Vec is the stack order,
    /// used only as the stable tiebreak.
    fn merge_candidates_by_band_z(&self) -> Vec<(String, u32)> {
        let Some(stack) = self.stack.as_ref() else {
            return Vec::new();
        };
        let (raster_z, _, _) = self.band_z_maps();
        let top_z = self.bands.len() as u32;
        stack
            .layers()
            .iter()
            .filter(|l| l.kind != LayerKind::Source)
            .map(|l| {
                let uid = l.uid.to_string();
                let z = match l.kind {
                    // Reserved bottom slot: Клин is composited under every raster.
                    LayerKind::Clean => 0,
                    LayerKind::Raster => raster_z
                        .get(&uid)
                        .copied()
                        .unwrap_or(top_z)
                        .saturating_add(1),
                    // Filtered out above.
                    LayerKind::Source => u32::MAX,
                };
                (uid, z)
            })
            .collect()
    }

    /// The layer directly beneath `id` on the unified band-Z axis (the visually-below layer the user
    /// sees in the composite), or `None` if `id` has nothing below it or is itself a base layer.
    ///
    /// The result may be the `Клин` base layer (the bottom-most raster merges INTO it); it is never
    /// `Исходник`. `id` itself must be a non-base layer: base layers are never the UPPER participant
    /// of a merge, which is also why the panel gives them no control strip.
    /// Band-Z based, so a manual reorder picks the correct pair (not the stack neighbor).
    fn raster_below_uid(&self, id: LayerId) -> Option<String> {
        let stack = self.stack.as_ref()?;
        let target = stack.layer(id).filter(|l| !l.kind.is_base())?;
        let target_uid = target.uid.to_string();
        raster_below_by_band_z(&self.merge_candidates_by_band_z(), &target_uid)
    }

    /// A raster can be merged down when there is another merge target directly beneath it on the
    /// unified band-Z axis — another raster, or `Клин` for the bottom-most one. Base layers are
    /// never mergeable themselves, and `Исходник` is never a target.
    fn is_mergeable(&self, id: LayerId) -> bool {
        self.raster_below_uid(id).is_some()
    }

    /// Applies the panel's deferred actions (everything that needs `&mut self` / `project`).
    fn apply_panel_actions(&mut self, actions: PanelActions, project: &ProjectData) {
        let page_idx = self.active_page_idx;
        if actions.add_layer
            && let Some(stack) = self.stack.as_mut()
        {
            // Add the local layer first (it is `pixels_dirty`, so a re-projection won't clobber its
            // empty pixels), then mirror it as a doc node so cross-tab reads see it.
            let id = stack.add_raster_layer();
            crate::trace_log!(
                cat::PS_EDITOR,
                "panel add_layer id={} page={:?}",
                id,
                page_idx
            );
            self.panel_primary = Some(RowSel::Raster(id));
            if let (Some(page_idx), Some(node)) = (
                page_idx,
                self.stack
                    .as_ref()
                    .and_then(|s| s.layer(id))
                    .map(layer_to_raster_node),
            ) {
                self.route_to_doc(page_idx, project, |doc| {
                    doc.add_node(page_idx, node);
                });
                // Record the ADD (observer style — the layer is already live). Undo → inverse
                // (Removed) deletes it; redo re-adds at the captured Z. Read the Z back from the
                // re-projected bands (the doc assigned it on top).
                if let Some(layer) = self.stack.as_ref().and_then(|s| s.layer(id)).cloned() {
                    let z = self.raster_band_z(&layer.uid.to_string());
                    self.history.record(PsEditOp::LayerLifecycle {
                        page_idx,
                        layer: Box::new(layer),
                        z,
                        dir: LifecycleDir::Added,
                    });
                }
            }
        }
        if actions.new_empty_group
            && let Some(stack) = self.stack.as_mut()
        {
            let n = stack.groups().len() + 1;
            // Persisted group name (round-trips to `layers.json`); stable literal, not
            // localized. See docs/i18n_exclusions.md §A (persisted layer/group names).
            let gid = stack.add_group(format!("Группа {n}"));
            crate::trace_log!(cat::PS_EDITOR, "panel new_empty_group gid={}", gid);
        }
        if let Some(id) = actions.set_active_raster
            && let Some(stack) = self.stack.as_mut()
        {
            crate::trace_log!(cat::PS_EDITOR, "panel set_active_raster id={}", id);
            // Active selection is LOCAL-only (not a doc model field): keep it on the stack.
            stack.set_active(id);
        }
        // After the active layer/primary is updated (and the `&mut self.stack` borrow above is
        // dropped), show the selected layer's marquee immediately. `select_active_layer_fully`
        // borrows `self.stack`/`self.text_layers` and writes `self.selection`, so it must run
        // OUTSIDE the `self.stack.as_mut()` scope.
        if actions.request_select_active {
            self.select_active_layer_fully();
        }
        if let Some(id) = actions.toggle_visible_raster {
            let is_base = self
                .stack
                .as_ref()
                .and_then(|s| s.layer(id))
                .is_some_and(|l| l.kind.is_base());
            if is_base {
                // Base-layer visibility is VIEW-ONLY session state: `Исходник`/`Клин` have no doc
                // node to patch and are deliberately never persisted (`persist_current_page`
                // filters to `LayerKind::Raster`), so there is nothing to route and nothing to
                // record in the history. `draw_composite` already honours `Layer::visible` for base
                // layers, which makes a plain stack write sufficient.
                if let Some(layer) = self.stack.as_mut().and_then(|s| s.layer_mut(id)) {
                    layer.visible = !layer.visible;
                    crate::trace_log!(
                        cat::PS_EDITOR,
                        "panel toggle_visible_base id={} visible={}",
                        id,
                        layer.visible
                    );
                }
            } else if let (Some(page_idx), Some(uid)) = (page_idx, self.raster_uid(id)) {
                let new_visible = self
                    .stack
                    .as_ref()
                    .and_then(|s| s.layer(id))
                    .is_some_and(|l| !l.visible);
                crate::trace_log!(
                    cat::PS_EDITOR,
                    "panel toggle_visible_raster id={} visible={}",
                    id,
                    new_visible
                );
                if !self.route_to_doc(page_idx, project, |doc| {
                    doc.set_visibility(page_idx, &uid, new_visible);
                }) && let Some(layer) = self.stack.as_mut().and_then(|s| s.layer_mut(id))
                {
                    layer.visible = new_visible;
                }
                // Record the toggle (a toggle always changes value → always record).
                self.history.record(PsEditOp::FieldPatch {
                    page_idx,
                    layer_uid: uid,
                    field: LayerFieldPatch::Visibility {
                        before: !new_visible,
                        after: new_visible,
                    },
                });
            }
        }
        if let Some(i) = actions.toggle_visible_text
            && let Some(layer) = self.text_layers.get_mut(i)
        {
            layer.visible = !layer.visible;
            crate::trace_log!(
                cat::PS_EDITOR,
                "panel toggle_visible_text index={} visible={}",
                i,
                layer.visible
            );
        }
        if let Some((id, value)) = actions.opacity_raster {
            // Live slider: fires only on actual value change (drag steps), not every idle frame.
            crate::trace_log!(
                cat::PS_EDITOR,
                "panel opacity_raster id={} value={:.3}",
                id,
                value
            );
            // Snapshot the pre-drag opacity ONCE per gesture (the stack still holds it before this
            // frame's apply), so the whole drag records a single undo step (see `opacity_gesture`).
            if self.opacity_gesture.is_none()
                && let Some(before) = self
                    .stack
                    .as_ref()
                    .and_then(|s| s.layer(id))
                    .map(|l| l.opacity)
            {
                self.opacity_gesture = Some((id, before));
            }
            // Live slider: mutate the doc node in memory + re-project, but don't flush each frame
            // (persisted on page-leave). Falls back to a local edit if no doc page is resident.
            if let (Some(page_idx), Some(uid)) = (page_idx, self.raster_uid(id)) {
                if !self.edit_doc_node(page_idx, |doc| {
                    doc.set_opacity(page_idx, &uid, value);
                }) && let Some(layer) = self.stack.as_mut().and_then(|s| s.layer_mut(id))
                {
                    layer.opacity = value;
                }
            } else if let Some(layer) = self.stack.as_mut().and_then(|s| s.layer_mut(id)) {
                layer.opacity = value;
            }
        } else if let Some((id, before)) = self.opacity_gesture.take() {
            // First frame with no further opacity change ⇒ the drag ended. Record one `FieldPatch`
            // for the whole gesture if the value actually moved.
            if let (Some(page_idx), Some(uid), Some(after)) = (
                page_idx,
                self.raster_uid(id),
                self.stack
                    .as_ref()
                    .and_then(|s| s.layer(id))
                    .map(|l| l.opacity),
            ) && (after - before).abs() > f32::EPSILON
            {
                self.history.record(PsEditOp::FieldPatch {
                    page_idx,
                    layer_uid: uid,
                    field: LayerFieldPatch::Opacity { before, after },
                });
            }
        }
        if let Some(id) = actions.remove_raster {
            crate::trace_log!(
                cat::PS_EDITOR,
                "panel remove_raster id={} page={:?}",
                id,
                page_idx
            );
            // Capture the FULL layer (with pixels) + its Z BEFORE removal so an undo can re-add it.
            let captured = self
                .stack
                .as_ref()
                .and_then(|s| s.layer(id))
                .filter(|l| l.kind == LayerKind::Raster)
                .cloned();
            let captured_z = captured
                .as_ref()
                .map(|l| self.raster_band_z(&l.uid.to_string()));
            // Record the deletion (so the manifest save drops it — `flush_page`/`save_page_rasters`
            // preserve unowned rasters, so a removal must be explicit), remove it from the doc in
            // memory + re-project, then persist via `persist_current_page` (which carries the removed
            // uid and bumps the revision).
            self.record_raster_deletion(id);
            if let (Some(page_idx), Some(uid)) = (page_idx, self.raster_uid(id)) {
                self.edit_doc_node(page_idx, |doc| {
                    doc.remove_node(page_idx, &uid);
                });
            } else if let Some(stack) = self.stack.as_mut() {
                stack.remove_layer(id);
            }
            self.persist_current_page(project);
            // Record the DELETE (observer style). Undo → inverse (Added) re-adds it at its prior Z.
            if let (Some(page_idx), Some(layer), Some(z)) = (page_idx, captured, captured_z) {
                self.history.record(PsEditOp::LayerLifecycle {
                    page_idx,
                    layer: Box::new(layer),
                    z,
                    dir: LifecycleDir::Removed,
                });
            }
        }
        if let Some(id) = actions.merge_req {
            crate::trace_log!(cat::PS_EDITOR, "panel merge_down id={}", id);
            self.merge_down(id, project);
        }
        if let Some(id) = actions.bake_req {
            crate::trace_log!(cat::PS_EDITOR, "panel bake_raster id={}", id);
            self.bake_raster(id, project);
        }
        if let Some(id) = actions.open_effects {
            crate::trace_log!(cat::PS_EDITOR, "panel open_effects id={}", id);
            // Seed the editor with the layer's current (non-destructive) chain so effects can be
            // tweaked or cleared rather than always starting blank.
            let seed = self
                .stack
                .as_ref()
                .and_then(|s| s.layer(id))
                .filter(|l| !l.effects.is_empty())
                .map(|l| serde_json::to_string_pretty(&l.effects).unwrap_or_default())
                .unwrap_or_default();
            self.effects_editor = Some((id, seed));
        }
        if let Some((index, op)) = actions.text_op {
            crate::trace_log!(cat::PS_EDITOR, "panel text_op index={} op={:?}", index, op);
            self.apply_text_layer_op(index, op, project);
        }
        if let Some((sel, up)) = actions.move_band {
            crate::trace_log!(cat::PS_EDITOR, "panel move_band sel={:?} up={}", sel, up);
            self.move_band_one(sel, up, project);
        }
        if let Some(op) = actions.group_op {
            crate::trace_log!(cat::PS_EDITOR, "panel group_op op={:?}", op);
            self.apply_group_op(op, project);
        }
    }
}

/// Lexicographic `<` on a `(Z, tiebreak)` unified-order key.
fn key_lt(a: (u32, f32), b: (u32, f32)) -> bool {
    a.0 < b.0 || (a.0 == b.0 && a.1.total_cmp(&b.1) == std::cmp::Ordering::Less)
}

/// Segments a contiguous unified order into inclusive blocks `[lo, hi]`. A run of bands sharing the
/// same `Some(group)` is one block; every ungrouped band (`None`) is its own block.
fn segment_blocks(order: &[(persist::BandRef, Option<String>)]) -> Vec<(usize, usize)> {
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for (i, (_, g)) in order.iter().enumerate() {
        if let Some((_, hi)) = blocks.last_mut()
            && g.is_some()
            && &order[*hi].1 == g
        {
            *hi = i;
        } else {
            blocks.push((i, i));
        }
    }
    blocks
}

/// Mutable group lookup by uid (`LayerStack` exposes only an immutable `group_by_uid`).
fn group_mut_by_uid<'a>(stack: &'a mut LayerStack, uid: &str) -> Option<&'a mut LayerGroup> {
    let gid = stack.group_by_uid(uid).map(|g| g.id)?;
    stack.group_mut(gid)
}

impl PsEditorTabState {
    /// Band-Z lookup maps from `self.bands`: raster uid→z, text-group layer_idx→z, pinned uid→z.
    fn band_z_maps(
        &self,
    ) -> (
        HashMap<String, u32>,
        HashMap<u32, u32>,
        HashMap<String, u32>,
    ) {
        let mut raster_z = HashMap::new();
        let mut group_z = HashMap::new();
        let mut pinned_z = HashMap::new();
        for band in &self.bands {
            match band {
                Band::Raster { uid, z } => {
                    raster_z.insert(uid.clone(), *z);
                }
                Band::TextGroup { layer_idx, z, .. } => {
                    group_z.insert(*layer_idx, *z);
                }
                Band::PinnedText { uid, z } => {
                    pinned_z.insert(uid.clone(), *z);
                }
            }
        }
        (raster_z, group_z, pinned_z)
    }

    /// Flattens a unified band `order` (bottom-to-top) into one node uid per band, expanding each
    /// `TextGroup(layer_idx)` band into its member text uids sub-ordered by ascending page-Y (lower on
    /// the page sorts lower in the stack), mirroring the render tiebreak (and the typing tab's
    /// `flatten_page_bands_to_refs`). Used to apply the SAME order the structure ops persist to disk onto the in-memory doc
    /// (whose nodes carry an explicit per-node Z, with no group-band concept).
    fn expand_order_to_node_uids(&self, order: &[persist::BandRef]) -> Vec<String> {
        let mut uids: Vec<String> = Vec::with_capacity(order.len());
        for band in order {
            match band {
                persist::BandRef::Raster(uid) | persist::BandRef::PinnedText(uid) => {
                    uids.push(uid.clone());
                }
                persist::BandRef::TextGroup(layer_idx) => {
                    let mut members: Vec<&PsTextLayer> = self
                        .text_layers
                        .iter()
                        .filter(|t| t.layer_idx == *layer_idx && !t.pinned)
                        .collect();
                    members.sort_by(|a, b| {
                        a.center()
                            .y
                            .partial_cmp(&b.center().y)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    uids.extend(members.iter().map(|t| t.uid.clone()));
                }
            }
        }
        uids
    }

    /// Current PS-group membership of every node (raster uid / text uid → group uid) plus the set of
    /// currently-pinned text uids.
    fn current_membership(&self) -> (HashMap<String, Option<String>>, HashSet<String>) {
        let mut group_of: HashMap<String, Option<String>> = HashMap::new();
        let mut pinned: HashSet<String> = HashSet::new();
        if let Some(stack) = self.stack.as_ref() {
            for layer in stack.layers() {
                if layer.kind.is_base() {
                    continue;
                }
                group_of.insert(layer.uid.to_string(), stack.layer_group_uid(layer.id));
            }
        }
        for text in &self.text_layers {
            group_of.insert(text.uid.clone(), text.group_uid.clone());
            if text.pinned {
                pinned.insert(text.uid.clone());
            }
        }
        (group_of, pinned)
    }

    /// Builds a complete, contiguous unified band order (bottom-to-top) for the given final
    /// membership + pin state: each group's bands are pulled together at the group's lowest member
    /// Z, preserving relative order. Returns each band paired with its final group, for callers that
    /// segment into group blocks. Mirrors `draw_composite`'s tiebreak so panel == composite order.
    fn build_unified_order(
        &self,
        group_of: &HashMap<String, Option<String>>,
        pinned: &HashSet<String>,
    ) -> Vec<(persist::BandRef, Option<String>)> {
        let (raster_z, group_z, pinned_z) = self.band_z_maps();
        let top = self.bands.len() as u32;
        let mut items: Vec<BandItem> = Vec::new();

        if let Some(stack) = self.stack.as_ref() {
            for layer in stack.layers() {
                if layer.kind.is_base() {
                    continue;
                }
                let uid = layer.uid.to_string();
                items.push(BandItem {
                    band: persist::BandRef::Raster(uid.clone()),
                    primary: raster_z.get(&uid).copied().unwrap_or(top),
                    secondary: 0.0,
                    group: group_of.get(&uid).cloned().flatten(),
                });
            }
        }
        let mut unpinned_groups: std::collections::BTreeSet<u32> =
            std::collections::BTreeSet::new();
        for text in &self.text_layers {
            if pinned.contains(&text.uid) {
                let pz = pinned_z
                    .get(&text.uid)
                    .copied()
                    .or_else(|| group_z.get(&text.layer_idx).copied())
                    .unwrap_or(top);
                items.push(BandItem {
                    band: persist::BandRef::PinnedText(text.uid.clone()),
                    primary: pz,
                    secondary: text.center().y,
                    group: group_of.get(&text.uid).cloned().flatten(),
                });
            } else {
                unpinned_groups.insert(text.layer_idx);
            }
        }
        for layer_idx in unpinned_groups {
            items.push(BandItem {
                band: persist::BandRef::TextGroup(layer_idx),
                primary: group_z.get(&layer_idx).copied().unwrap_or(top),
                secondary: 0.0,
                group: None,
            });
        }

        // Anchor each group at the lexicographically-lowest (primary, secondary) of its members.
        let mut anchor: HashMap<String, (u32, f32)> = HashMap::new();
        for it in &items {
            if let Some(g) = &it.group {
                let key = (it.primary, it.secondary);
                anchor
                    .entry(g.clone())
                    .and_modify(|e| {
                        if key_lt(key, *e) {
                            *e = key;
                        }
                    })
                    .or_insert(key);
            }
        }
        items.sort_by(|a, b| {
            let ka = a
                .group
                .as_ref()
                .map_or((a.primary, a.secondary), |g| anchor[g]);
            let kb = b
                .group
                .as_ref()
                .map_or((b.primary, b.secondary), |g| anchor[g]);
            ka.0.cmp(&kb.0)
                .then(ka.1.total_cmp(&kb.1))
                .then(a.primary.cmp(&b.primary))
                .then(a.secondary.total_cmp(&b.secondary))
        });
        items.into_iter().map(|it| (it.band, it.group)).collect()
    }

    /// Moves a single raster or pinned-text band one step in Z. A grouped band reorders only within
    /// its group's run; an ungrouped band hops over the whole neighbouring block (group or band).
    ///
    // Z-reorder undo is a LATER part, not B1: this path writes the band order to disk synchronously
    // (`save_page_band_order`, which must run LAST so it wins over the raster save) in addition to the
    // doc — a dual-persistence ordering the unified undo/redo persist tail cannot reproduce without a
    // dedicated per-op persistence hook. So no `PsEditOp` is recorded here yet.
    fn move_band_one(&mut self, sel: RowSel, up: bool, project: &ProjectData) {
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        let target = match &sel {
            RowSel::Raster(id) => {
                let Some(uid) = self
                    .stack
                    .as_ref()
                    .and_then(|s| s.layer(*id))
                    .map(|l| l.uid.to_string())
                else {
                    return;
                };
                persist::BandRef::Raster(uid)
            }
            RowSel::Text(uid) => persist::BandRef::PinnedText(uid.clone()),
            RowSel::Group(_) => return,
        };
        // Ensure the page's rasters are on disk BEFORE the synchronous band-order write below:
        // `persist_current_page` now ENQUEUES (async) so it cannot guarantee the raster nodes are
        // written in time, and `apply_band_order` SILENTLY SKIPS a `BandRef::Raster` missing from the
        // manifest (dropping its new Z → the reorder is lost on reload). A synchronous `flush_page`
        // writes them now; `persist_current_page` still runs for the deletion (`removed_uids`)
        // bookkeeping, and its later job preserves the band Z set here (z is read back from disk).
        if let Some(doc) = self.layer_doc.clone()
            && let Ok(mut guard) = doc.lock()
            && let Err(err) = guard.flush_page(
                page_idx,
                &project.paths.unsaved_layers_dir,
                Some(&project.paths.layers_dir),
            )
        {
            crate::runtime_log::log_warn(format!(
                "[ps_editor] sync flush before band reorder (page {page_idx}): {err}"
            ));
        }
        self.persist_current_page(project);
        let (group_of, pinned) = self.current_membership();
        let order = self.build_unified_order(&group_of, &pinned);
        let Some(pos) = order.iter().position(|(b, _)| *b == target) else {
            return;
        };
        let mut bands: Vec<(persist::BandRef, Option<String>)> = order;
        let my_group = bands[pos].1.clone();
        // The single node uid this band addresses (rasters / pinned text are single-node bands).
        let target_uid = match &target {
            persist::BandRef::Raster(u) | persist::BandRef::PinnedText(u) => Some(u.clone()),
            persist::BandRef::TextGroup(_) => None,
        };
        // A grouped band moving within its run is a single adjacent-node swap (→ `reorder_node_one`);
        // an ungrouped band hops a whole block (→ apply the recomputed order via `set_z_order`).
        let grouped_swap = my_group.is_some();
        if my_group.is_some() {
            // Reorder within the group's run only.
            let nb = if up { pos + 1 } else { pos.wrapping_sub(1) };
            if nb < bands.len() && bands[nb].1 == my_group {
                bands.swap(pos, nb);
            } else {
                return;
            }
        } else {
            // Ungrouped: hop over the neighbouring block.
            let blocks = segment_blocks(&bands);
            let bi = blocks.iter().position(|(lo, hi)| pos >= *lo && pos <= *hi);
            let Some(bi) = bi else { return };
            let target_block = if up { bi + 1 } else { bi.wrapping_sub(1) };
            if target_block >= blocks.len() {
                return;
            }
            // Move our singleton block to the far side of the neighbour block.
            let (nlo, nhi) = blocks[target_block];
            let item = bands.remove(pos);
            let insert_at = if up {
                // neighbour shifted down by one after removal of our lower element.
                nhi // after removal, neighbour occupies [nlo-1, nhi-1]; insert after it
            } else {
                nlo
            };
            bands.insert(insert_at.min(bands.len()), item);
        }
        let order_refs: Vec<persist::BandRef> = bands.into_iter().map(|(b, _)| b).collect();
        match persist::save_page_band_order(
            &project.paths.unsaved_layers_dir,
            page_idx,
            &order_refs,
        ) {
            Ok(()) => {
                self.reload_overlays_view(project, page_idx);
                // Apply the SAME reorder in-memory so the doc (and, via its version bump, the typing
                // tab) re-project without a disk round-trip. A grouped intra-run move is one adjacent
                // node swap; an ungrouped block-hop reassigns the whole order.
                let node_order = self.expand_order_to_node_uids(&order_refs);
                self.edit_doc_node(page_idx, |doc| {
                    if grouped_swap && let Some(uid) = &target_uid {
                        doc.reorder_node_one(page_idx, uid, up);
                    } else {
                        doc.set_z_order(page_idx, &node_order);
                    }
                });
            }
            Err(err) => crate::runtime_log::log_warn(format!("[ps_editor] move band: {err}")),
        }
    }

    /// Resolves a `GroupOp` into a `persist::GroupingEdit`, mirrors the raster-side changes into the
    /// in-memory stack, persists, and reloads the overlays/bands view.
    ///
    // Group ops are OUT of scope for undo Part B1 (they share the same disk-band-order dual write as
    // `move_band_one`); no `PsEditOp` is recorded here. Deferred to a later part.
    fn apply_group_op(&mut self, op: GroupOp, project: &ProjectData) {
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        let (group_of, pinned) = self.current_membership();

        // Resolve the current selection into node uids (rasters + texts) and the raster ids.
        let mut sel_raster_ids: Vec<LayerId> = Vec::new();
        let mut sel_node_uids: Vec<String> = Vec::new();
        if let Some(stack) = self.stack.as_ref() {
            for s in &self.panel_selection {
                match s {
                    RowSel::Raster(id) => {
                        if let Some(l) = stack.layer(*id)
                            && !l.kind.is_base()
                        {
                            sel_raster_ids.push(*id);
                            sel_node_uids.push(l.uid.to_string());
                        }
                    }
                    RowSel::Text(uid) => sel_node_uids.push(uid.clone()),
                    RowSel::Group(_) => {}
                }
            }
        }
        // Text metadata: uid -> (currently pinned, pinned_by_group).
        let text_pin: HashMap<String, (bool, bool)> = self
            .text_layers
            .iter()
            .map(|t| (t.uid.clone(), (t.pinned, t.pinned_by_group)))
            .collect();
        let is_user_pinned = |uid: &str| text_pin.get(uid).is_some_and(|(p, pg)| *p && !*pg);
        let sel_text_uids: Vec<String> = sel_node_uids
            .iter()
            .filter(|u| text_pin.contains_key(*u))
            .cloned()
            .collect();

        let mut edit = persist::GroupingEdit::default();
        let mut new_gid: Option<(String, String)> = None; // (uid, name)

        // Group-meta ops (collapse / visibility / opacity) are stack-only: `draw_composite` folds
        // them live from the stack and `save_page_rasters` persists them on page/tab-leave, so there
        // is no per-tick disk write (important for the opacity slider). They MUST mark `layers_dirty`
        // so the dirty-gated tab-switch flush (`flush_layers_if_dirty`) still persists them — without
        // it a vis/opacity/collapse change would revert on the next PS reload (it is not in the doc).
        match &op {
            GroupOp::ToggleCollapse(uid) => {
                if let Some(g) = self.stack.as_mut().and_then(|s| group_mut_by_uid(s, uid)) {
                    g.collapsed = !g.collapsed;
                }
                self.layers_dirty = true;
                return;
            }
            GroupOp::ToggleGroupVisible(uid) => {
                if let Some(g) = self.stack.as_mut().and_then(|s| group_mut_by_uid(s, uid)) {
                    g.visible = !g.visible;
                }
                self.layers_dirty = true;
                return;
            }
            GroupOp::GroupOpacity(uid, v) => {
                if let Some(g) = self.stack.as_mut().and_then(|s| group_mut_by_uid(s, uid)) {
                    g.opacity = *v;
                }
                self.layers_dirty = true;
                return;
            }
            GroupOp::MoveGroup(uid, up) => {
                self.move_group_block(uid.clone(), *up, project);
                return;
            }
            _ => {}
        }

        // Membership-changing ops below all rebuild the unified order.
        let mut final_group = group_of.clone();
        let mut final_pinned = pinned.clone();

        let target_group: Option<String> = match &op {
            GroupOp::NewFromSelection => {
                let n = self.stack.as_ref().map_or(0, |s| s.groups().len()) + 1;
                let uid = uuid::Uuid::new_v4().to_string();
                // Persisted group name (round-trips to `layers.json`); stable literal.
                // See docs/i18n_exclusions.md §A (persisted layer/group names).
                let name = format!("Группа {n}");
                edit.new_groups.push(persist::GroupMeta {
                    uid: uid.clone(),
                    name: name.clone(),
                    visible: true,
                    opacity: 1.0,
                    collapsed: false,
                });
                new_gid = Some((uid.clone(), name));
                Some(uid)
            }
            GroupOp::MoveTo(uid) => Some(uid.clone()),
            GroupOp::Ungroup => None,
            GroupOp::DeleteGroup(uid) => {
                edit.remove_groups.push(uid.clone());
                // Members of the deleted group ungroup and (if group-pinned) unpin.
                let members: Vec<String> = final_group
                    .iter()
                    .filter(|(_, g)| g.as_deref() == Some(uid.as_str()))
                    .map(|(n, _)| n.clone())
                    .collect();
                for n in &members {
                    edit.set_membership.push((n.clone(), None));
                    final_group.insert(n.clone(), None);
                    if text_pin.get(n).is_some_and(|(_, pg)| *pg) {
                        final_pinned.remove(n);
                        edit.unpin_for_group.push(n.clone());
                    }
                }
                // Mirror: removing the group ungroups its raster members in the stack too.
                if let Some(stack) = self.stack.as_mut()
                    && let Some(gid) = stack.group_by_uid(uid).map(|g| g.id)
                {
                    stack.remove_group(gid);
                }
                let order = self
                    .build_unified_order(&final_group, &final_pinned)
                    .into_iter()
                    .map(|(b, _)| b)
                    .collect();
                edit.order = order;
                self.persist_grouping(edit, page_idx, project);
                return;
            }
            _ => None,
        };

        // Apply membership for NewFromSelection / MoveTo / Ungroup.
        for uid in &sel_node_uids {
            edit.set_membership
                .push((uid.clone(), target_group.clone()));
            final_group.insert(uid.clone(), target_group.clone());
        }
        if target_group.is_some() {
            // Entering a group: every selected text must own its Z band (auto-pin).
            for uid in &sel_text_uids {
                final_pinned.insert(uid.clone());
                if !is_user_pinned(uid) {
                    edit.pin_for_group.push(uid.clone());
                }
            }
        } else {
            // Ungroup: release group-owned pins (keep real user pins).
            for uid in &sel_text_uids {
                if text_pin.get(uid).is_some_and(|(_, pg)| *pg) {
                    final_pinned.remove(uid);
                    edit.unpin_for_group.push(uid.clone());
                }
            }
        }

        let order = self
            .build_unified_order(&final_group, &final_pinned)
            .into_iter()
            .map(|(b, _)| b)
            .collect();
        edit.order = order;

        // Mirror raster membership / group creation into the stack.
        if let Some(stack) = self.stack.as_mut() {
            let target_gid = match (&op, &new_gid, &target_group) {
                (GroupOp::NewFromSelection, Some((uid, name)), _) => {
                    let parsed =
                        uuid::Uuid::parse_str(uid).unwrap_or_else(|_| uuid::Uuid::new_v4());
                    Some(stack.add_group_with_uid(name.clone(), parsed))
                }
                (_, _, Some(uid)) => stack.group_by_uid(uid).map(|g| g.id),
                _ => None,
            };
            for id in &sel_raster_ids {
                stack.set_layer_group(*id, target_gid);
            }
        }

        self.persist_grouping(edit, page_idx, project);
    }

    /// Moves a group's whole contiguous block one step in Z by swapping it with the neighbouring
    /// block, then persists the resulting band order.
    fn move_group_block(&mut self, uid: String, up: bool, project: &ProjectData) {
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        // Ensure the page's rasters are on disk BEFORE the synchronous band-order write below:
        // `persist_current_page` now ENQUEUES (async) so it cannot guarantee the raster nodes are
        // written in time, and `apply_band_order` SILENTLY SKIPS a `BandRef::Raster` missing from the
        // manifest (dropping its new Z → the reorder is lost on reload). A synchronous `flush_page`
        // writes them now; `persist_current_page` still runs for the deletion (`removed_uids`)
        // bookkeeping, and its later job preserves the band Z set here (z is read back from disk).
        if let Some(doc) = self.layer_doc.clone()
            && let Ok(mut guard) = doc.lock()
            && let Err(err) = guard.flush_page(
                page_idx,
                &project.paths.unsaved_layers_dir,
                Some(&project.paths.layers_dir),
            )
        {
            crate::runtime_log::log_warn(format!(
                "[ps_editor] sync flush before band reorder (page {page_idx}): {err}"
            ));
        }
        self.persist_current_page(project);
        let (group_of, pinned) = self.current_membership();
        let bands = self.build_unified_order(&group_of, &pinned);
        let blocks = segment_blocks(&bands);
        let Some(bi) = blocks
            .iter()
            .position(|(lo, _)| bands[*lo].1.as_deref() == Some(uid.as_str()))
        else {
            return;
        };
        let target = if up { bi + 1 } else { bi.wrapping_sub(1) };
        if target >= blocks.len() {
            return;
        }
        // Rebuild the order with the two blocks swapped.
        let mut order_blocks: Vec<Vec<(persist::BandRef, Option<String>)>> = blocks
            .iter()
            .map(|(lo, hi)| bands[*lo..=*hi].to_vec())
            .collect();
        order_blocks.swap(bi, target);
        let order_refs: Vec<persist::BandRef> =
            order_blocks.into_iter().flatten().map(|(b, _)| b).collect();
        match persist::save_page_band_order(
            &project.paths.unsaved_layers_dir,
            page_idx,
            &order_refs,
        ) {
            Ok(()) => {
                self.reload_overlays_view(project, page_idx);
                // Apply the same group-block move in-memory so the doc (and, via its version bump, the
                // typing tab) re-project without a disk round-trip.
                self.edit_doc_node(page_idx, |doc| {
                    doc.reorder_group_block(page_idx, &uid, up);
                });
            }
            Err(err) => crate::runtime_log::log_warn(format!("[ps_editor] move group: {err}")),
        }
    }

    /// Writes a grouping edit to the unsaved layers dir, then reloads the overlays/bands view and
    /// mirrors the SAME edit onto the shared doc in-memory (so it and the typing tab re-project
    /// without a disk round-trip). Flushes the page's rasters first so freshly-added raster layers
    /// already have manifest nodes for the edit's membership / order to land on.
    fn persist_grouping(
        &mut self,
        edit: persist::GroupingEdit,
        page_idx: usize,
        project: &ProjectData,
    ) {
        self.persist_current_page(project);
        // Snapshot the in-memory doc effect of `edit` BEFORE it is moved into the disk write. The band
        // order expansion uses the current text-layer page-Y order (unchanged by membership), matching
        // what `save_page_grouping`'s `apply_band_order` records on disk.
        let node_order = self.expand_order_to_node_uids(&edit.order);
        let new_groups = edit.new_groups.clone();
        let remove_groups = edit.remove_groups.clone();
        let set_membership = edit.set_membership.clone();
        match persist::save_page_grouping(&project.paths.unsaved_layers_dir, page_idx, &edit) {
            Ok(()) => {
                self.reload_overlays_view(project, page_idx);
                // Apply removes → creates → membership → order, mirroring `save_page_grouping`.
                self.edit_doc_node(page_idx, |doc| {
                    for g in &remove_groups {
                        doc.remove_group(page_idx, g);
                    }
                    for g in new_groups {
                        doc.add_group(page_idx, g);
                    }
                    for (node_uid, group_uid) in &set_membership {
                        doc.set_group(page_idx, node_uid, group_uid.clone());
                    }
                    if !node_order.is_empty() {
                        doc.set_z_order(page_idx, &node_order);
                    }
                });
            }
            Err(err) => crate::runtime_log::log_warn(format!("[ps_editor] grouping: {err}")),
        }
    }

    /// Floating editor for a raster layer's effects chain (non-destructive: applying renders the
    /// chain off the GUI thread via `apply_effects_to_raster`, leaving the base pixels reversible).
    fn draw_effects_editor(&mut self, ctx: &egui::Context) {
        if self.effects_editor.is_none() {
            return;
        }
        let mut open = true;
        let mut apply = false;
        let mut cancel = false;
        egui::Window::new(t!("ps_editor.effects_editor.window_title"))
            .id(egui::Id::new("ps_editor.effects_editor.window_title"))
            .collapsible(false)
            .resizable(true)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(t!("ps_editor.effects_editor.chain_label"));
                if let Some((_, text)) = self.effects_editor.as_mut() {
                    ui.add(
                        egui::TextEdit::multiline(text)
                            .desired_rows(6)
                            .desired_width(360.0)
                            .code_editor(),
                    );
                }
                ui.horizontal(|ui| {
                    if ui
                        .button(t!("ps_editor.effects_editor.apply_button"))
                        .clicked()
                    {
                        apply = true;
                    }
                    if ui
                        .button(t!("ps_editor.effects_editor.cancel_button"))
                        .clicked()
                    {
                        cancel = true;
                    }
                });
                ui.small(t!("ps_editor.effects_editor.example_hint"));
            });

        if apply {
            if let Some((id, text)) = self.effects_editor.take() {
                self.apply_effects_to_raster(id, &text);
            }
        } else if cancel || !open {
            self.effects_editor = None;
        }
    }

    /// Applies an effects chain to raster layer `id` **non-destructively** (reversibly), matching the
    /// typing tab: effects render from the layer's pre-effects base pixels, the rendered result
    /// becomes the display `image`, and the chain is stored on the layer + persisted via
    /// `update_raster_effects` (base PNG untouched). An empty/blank chain clears effects and restores
    /// the base pixels.
    ///
    /// The expensive `apply_effects_to_color_image` call (tens of ms on a large page) runs on a
    /// worker thread, never the GUI thread: this method only parses the JSON, clones the base
    /// ColorImage (dropping every lock before spawning), and spawns the render. The result is applied
    /// by `poll_ps_raster_effects_jobs`, which does the cheap recenter / doc-routing / persist on the
    /// GUI thread. If a render is already in flight, the latest request is stashed (latest-wins) so a
    /// second raster's effects are not lost.
    fn apply_effects_to_raster(&mut self, id: LayerId, json: &str) {
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        // Parse the editor text into the on-disk effects chain shape (a JSON array of objects, the
        // typing-tab contract). A blank string is "no effects".
        let effects: Vec<serde_json::Value> = if json.trim().is_empty() {
            Vec::new()
        } else {
            match serde_json::from_str::<Vec<serde_json::Value>>(json) {
                Ok(chain) => chain,
                Err(err) => {
                    crate::runtime_log::log_warn(format!("[ps_editor] effects parse: {err}"));
                    return;
                }
            }
        };

        if self.raster_effects_state.is_some() {
            // A render is already in flight: stash the latest request (superseding any older pending
            // one) so `poll_ps_raster_effects_jobs` re-dispatches it once the current render finishes.
            // Otherwise this edit would be silently lost — e.g. effecting a second raster right after
            // a first, leaving the second without its effects on save.
            self.pending_raster_effects = Some((id, json.to_string()));
            return;
        }

        // Resolve the pre-effects render source on the GUI thread (a cheap clone), then drop the
        // stack borrow BEFORE spawning so no lock is held across the worker. For a RAW raster the
        // source is the current display pixels (what's on screen now); for an effected raster it is
        // the preserved base, so re-applying replaces (not stacks) the chain.
        let prepared: Option<(ColorImage, [usize; 2], LayerTransform)> = {
            let Some(stack) = self.stack.as_ref() else {
                return;
            };
            let Some(layer) = stack.layer(id) else {
                return;
            };
            if layer.kind.is_base() {
                return;
            }
            let is_raw = layer.effects.is_empty();
            let base = if is_raw {
                layer.image.clone()
            } else {
                layer.base_image.clone()
            };
            let base_size = base.size;
            Some((base, base_size, layer.transform))
        };
        let Some((base_image, base_size, base_t)) = prepared else {
            return;
        };
        let Some(uid) = self.raster_uid(id) else {
            return;
        };

        // Spawn the expensive render off the GUI thread; `poll_ps_raster_effects_jobs` applies it.
        let json_owned = json.to_string();
        let effects_owned = effects;
        let (tx, rx) = mpsc::channel::<Result<PsRasterEffectsResult, String>>();
        thread::spawn(move || {
            let _ = tx.send(render_ps_raster_effects(
                page_idx,
                uid,
                id,
                base_image,
                base_size,
                base_t,
                json_owned,
                effects_owned,
            ));
        });
        self.raster_effects_state = Some(rx);
    }

    /// Polls the non-destructive raster-effects worker once per frame. When a result arrives it does
    /// the GUI-side cheap work the typing tab keeps on the main thread: the recenter anchoring math,
    /// the `edit_doc_node` routing (swap base/display/effects + bump generation), the reversible
    /// `update_raster_effects` persist, and the `render_cache` drop. Then it re-dispatches any
    /// request stashed while the render was in flight (latest-wins). Returns `true` when a result was
    /// consumed (the frame should repaint).
    fn poll_ps_raster_effects_jobs(&mut self, project: &ProjectData) -> bool {
        let recv = {
            let Some(rx) = self.raster_effects_state.as_ref() else {
                return false;
            };
            match rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    Some(Err(t!("ps_editor.effects.raster_interrupted").to_string()))
                }
            }
        };
        let Some(recv) = recv else {
            return false;
        };
        self.raster_effects_state = None;
        let result = match recv {
            Ok(r) => r,
            Err(err) => {
                crate::runtime_log::log_warn(format!("[ps_editor] effects: {err}"));
                // Still re-dispatch a stashed request so a queued edit is not stranded.
                self.dispatch_pending_raster_effects();
                return true;
            }
        };
        self.apply_ps_raster_effects_result(result, project);
        // Re-dispatch an edit that arrived while this render was in flight, so the last requested
        // effects (e.g. on a second raster) are not lost. `raster_effects_state` is now `None`, so
        // this spawns a fresh render instead of re-stashing.
        self.dispatch_pending_raster_effects();
        true
    }

    /// Re-dispatches the stashed raster-effects request, if any. Called from
    /// `poll_ps_raster_effects_jobs` after the in-flight render is consumed.
    fn dispatch_pending_raster_effects(&mut self) {
        if let Some((id, json)) = self.pending_raster_effects.take() {
            self.apply_effects_to_raster(id, &json);
        }
    }

    /// GUI-side apply step for a completed raster-effects render (mirrors the typing tab's
    /// `poll_raster_effects_jobs` body). Performs the recenter anchoring, routes the swap to the
    /// shared doc, persists the chain reversibly (base PNG untouched), and drops the layer's GPU
    /// cache so the new display re-uploads. No long work, no decode, no held lock across a worker.
    fn apply_ps_raster_effects_result(
        &mut self,
        result: PsRasterEffectsResult,
        project: &ProjectData,
    ) {
        let PsRasterEffectsResult {
            page_idx,
            uid,
            id,
            new_image,
            origin,
            base_size,
            base_t,
            effects,
        } = result;

        let new_size = new_image.size;
        // World-space center shift that keeps the original content anchored after effects grow the
        // image (shadow/glow). Pure math, unit-tested in `effects_recenter_offset`.
        let rotated = effects_recenter_offset(new_size, origin, base_size, base_t);

        // Compute the post-effects display, base snapshot, and recentered transform, then write them
        // to the shared doc. The base snapshot only happens going RAW→effects — when effects already
        // exist, the doc node's base is left untouched. Routed via the doc so the projection re-derives
        // the stack layer from one model.
        let (display, new_transform): (Option<ColorImage>, TransformRec) = if effects.is_empty() {
            // No effects: display is the (current) base, placed at the base transform.
            (None, transform_to_rec(base_t))
        } else {
            let mut t = base_t;
            t.center = base_t.center + rotated;
            (Some(new_image), transform_to_rec(t))
        };
        let effects_for_doc = effects.clone();
        // The RAW→effects base snapshot is keyed on the doc node's own effects state (`e.is_empty()`)
        // below, the authoritative trigger — the same rule the original synchronous path used.
        self.edit_doc_node(page_idx, |doc| {
            if let Some(node) = doc.node_mut(page_idx, &uid)
                && let crate::models::layer_model::layer_doc::NodeBody::Raster {
                    base_image,
                    display_image,
                    effects: e,
                    ..
                } = &mut node.body
            {
                if effects_for_doc.is_empty() {
                    // Clear: display becomes the base; chain emptied; transform restored.
                    *display_image = base_image.clone();
                    *e = Vec::new();
                } else {
                    // RAW→effects: snapshot the current display as the new base first.
                    if e.is_empty() {
                        *base_image = display_image.clone();
                    }
                    if let Some(d) = display.clone() {
                        *display_image = d;
                    }
                    *e = effects_for_doc.clone();
                }
                node.transform = new_transform;
                node.bump_generation();
            }
        });

        // Persist reversibly: writes the effects chain + the `_fx` rendered PNG (or clears them both),
        // leaving the base PNG intact. (`flush_page` only re-writes non-empty chains, so the CLEAR case
        // is handled here.) ASYNC: route through the doc's effects-only saver path (PNG encode
        // off-thread; targeted single-raster RMW, never a whole-page rewrite) — falls back to the sync
        // `update_raster_effects` when no saver is enabled. The save-to-project / app-close barriers
        // guarantee the enqueued effects land. Then the cross-tab bump already happened via the doc edit.
        let rendered_for_persist = if effects.is_empty() {
            None
        } else {
            display.as_ref()
        };
        let effects_persist = self
            .layer_doc
            .as_ref()
            .and_then(|doc| {
                doc.lock().ok().map(|mut guard| {
                    guard.enqueue_raster_effects(
                        page_idx,
                        &project.paths.unsaved_layers_dir,
                        Some(&project.paths.layers_dir),
                        &uid,
                        &effects,
                        rendered_for_persist,
                    )
                })
            })
            // No doc wired (defensive): fall back to a direct synchronous effects write so the disk
            // state is still correct, identical to the pre-async behavior.
            .unwrap_or_else(|| {
                persist::update_raster_effects(
                    &project.paths.unsaved_layers_dir,
                    page_idx,
                    &uid,
                    &effects,
                    rendered_for_persist,
                    Some(&project.paths.layers_dir),
                )
            });
        if let Err(err) = effects_persist {
            crate::runtime_log::log_warn(format!("[ps_editor] persist effects: {err}"));
        }
        self.render_cache.remove(&id);
        // No cross-tab signal needed: the MODEL change routed through `edit_doc_node` above, which
        // bumped the doc version (so the typing tab re-projects).
    }

    /// Bakes (запекает) raster `id`: flattens its non-destructive effects render into the base
    /// pixels and clears the chain, turning it into an ordinary directly-editable raster. The dirty
    /// save rewrites the base PNG and drops the chain — a permanent flatten. A raw raster (empty
    /// effects) is a no-op.
    fn bake_raster(&mut self, id: LayerId, project: &ProjectData) {
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        // Read the rendered display + effects state from the stack (the projection mirror of the doc).
        let Some((uid, display, has_effects)) = self
            .stack
            .as_ref()
            .and_then(|s| s.layer(id))
            .filter(|l| l.kind == LayerKind::Raster)
            .map(|l| (l.uid.to_string(), l.image.clone(), !l.effects.is_empty()))
        else {
            return;
        };
        if !has_effects {
            return; // raw raster: nothing to bake
        }
        // Route to the doc: the rendered display becomes the new base, the chain is dropped, and the
        // node is marked pixels_dirty so the flush rewrites the base PNG. `route_to_doc` flushes +
        // bumps + re-projects (the projection drops the stale cache via the generation change).
        let base = display.clone();
        self.route_to_doc(page_idx, project, |doc| {
            doc.set_raster_pixels(page_idx, &uid, base, display, Vec::new(), true);
        });
        self.render_cache.remove(&id);
    }

    /// Applies a pin/rasterize action on the text layer at `index`.
    fn apply_text_layer_op(&mut self, index: usize, op: TextLayerOp, project: &ProjectData) {
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        match op {
            TextLayerOp::TogglePin => {
                let Some(layer) = self.text_layers.get(index) else {
                    return;
                };
                let (uid, layer_idx, pinned) = (layer.uid.clone(), layer.layer_idx, layer.pinned);
                let mut order: Vec<persist::BandRef> =
                    self.bands.iter().map(Band::to_ref).collect();
                if pinned {
                    // Drop its pinned band so it rejoins its text group's auto-Y order.
                    order.retain(|b| !matches!(b, persist::BandRef::PinnedText(u) if *u == uid));
                } else {
                    // Give it its own band, just above its text group.
                    let after = self
                        .bands
                        .iter()
                        .position(|b| matches!(b, Band::TextGroup { layer_idx: li, .. } if *li == layer_idx))
                        .map_or(order.len(), |p| p + 1);
                    order.insert(after, persist::BandRef::PinnedText(uid));
                }
                match persist::save_page_band_order(
                    &project.paths.unsaved_layers_dir,
                    page_idx,
                    &order,
                ) {
                    Ok(()) => {
                        self.reload_overlays_view(project, page_idx);
                        // In the unified doc, pinning is purely a Z-order change: text nodes carry an
                        // explicit per-node `z` and no pin axis (the `pinned` flag is a disk-only
                        // concept for re-deriving Z on load). So the doc effect is exactly the new band
                        // order — applied in-memory so it (and, via its version bump, the typing tab)
                        // re-project without a disk round-trip.
                        let node_order = self.expand_order_to_node_uids(&order);
                        self.edit_doc_node(page_idx, |doc| {
                            doc.set_z_order(page_idx, &node_order);
                        });
                    }
                    Err(err) => {
                        crate::runtime_log::log_warn(format!("[ps_editor] pin text: {err}"))
                    }
                }
            }
            TextLayerOp::Rasterize => {
                let Some(layer) = self.text_layers.get(index) else {
                    return;
                };
                let (uid, name, image, transform) = (
                    layer.uid.clone(),
                    layer.name.clone(),
                    layer.image().clone(),
                    layer.transform(),
                );
                // Add the baked raster on the stack first (it becomes pixels_dirty so a re-projection
                // keeps it), then mirror the op onto the shared doc: add the new Raster node and remove
                // the Text node. The new raster carries the text overlay's placement.
                let new_id = self
                    .stack
                    .as_mut()
                    // Persisted raster-layer name (round-trips to `layers.json`); stable
                    // literal, not localized. See docs/i18n_exclusions.md §A.
                    .map(|s| {
                        s.add_raster_layer_image(format!("Запечён: {name}"), image, transform)
                    });
                // The doc is the sole text writer: removing the Text node and flushing drops it from
                // `layers.json` (a migrated page ignores the stale `text_info.json` entry, so the
                // rasterized overlay does not resurrect). No `text_info.json` write here.
                let new_node = new_id
                    .and_then(|id| self.stack.as_ref().and_then(|s| s.layer(id)))
                    .map(layer_to_raster_node);
                if let Some(node) = new_node {
                    let text_uid = uid.clone();
                    self.route_to_doc(page_idx, project, |doc| {
                        doc.remove_node(page_idx, &text_uid);
                        doc.add_node(page_idx, node);
                    });
                }
            }
        }
    }

    /// Whether `pointer_pos` is inside one of THIS frame's drawn dock panels.
    ///
    /// The rects come from `PanelDockOutput::drawn_panels` and are main-window rects by
    /// construction: a panel the user detached into a sub-window is never reported, so it can never
    /// blank out a region of this window.
    fn pointer_in_any_panel(&self, pointer_pos: Pos2) -> bool {
        self.panel_rects
            .iter()
            .any(|panel_rect| panel_rect.contains(pointer_pos))
    }

    /// Whether canvas pointer input at `pointer_pos` is claimed by UI floating above the canvas.
    ///
    /// Three terms, mirroring the cleaning tab's gate (`tabs/cleaning/tab.rs`):
    /// * an open popup or menu anywhere — its own layer may not cover this point yet on the frame
    ///   it opens;
    /// * this frame's dock-panel rects — the tab's uniform statement of "a floating surface of mine
    ///   is here", independent of the panel widget staying an interactable `Area`;
    /// * z-order occlusion: anything above `Order::Background` at that point, which covers the
    ///   effects-editor window, tooltips and context menus. This is the same test
    ///   `crate::input_util::pointer_over_floating_area` performs, asked at the HOVER position
    ///   rather than at `interact_pos`, which is the position the canvas gate is about; calling
    ///   both would only add a second, drag-latched copy of the same answer.
    fn canvas_pointer_occluded(&self, ctx: &egui::Context, pointer_pos: Pos2) -> bool {
        ctx.any_popup_open()
            || self.pointer_in_any_panel(pointer_pos)
            || ctx.layer_id_at(pointer_pos).is_some_and(|layer| {
                matches!(
                    layer.order,
                    egui::Order::Middle
                        | egui::Order::Foreground
                        | egui::Order::Tooltip
                        | egui::Order::Debug
                )
            })
    }

    /// Central pan/zoom canvas: input handling, layer composite, overlays.
    ///
    /// `rect` is the SAME rect the dock was given this frame, so the canvas fills the whole
    /// program-tab area and the floating panels sit over it rather than beside it.
    fn draw_canvas(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        rect: Rect,
        project: &ProjectData,
    ) {
        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        ui.painter()
            .rect_filled(rect, CornerRadius::ZERO, Color32::from_gray(40));

        let Some(page_size) = self.stack.as_ref().map(|stack| stack.size()) else {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                t!("ps_editor.canvas.no_page"),
                egui::FontId::proportional(16.0),
                Color32::from_gray(160),
            );
            return;
        };

        self.viewport.fit_page_if_needed(rect, page_size);
        if self.pending_actual_size {
            self.viewport.reset_to_actual_size(page_size);
            self.pending_actual_size = false;
        }

        // Keyboard gestures routed to tools (Esc / Backspace / Delete) must not be stolen from a
        // focused text field — the same discipline `handle_hotkeys` applies to its letter keys.
        // Resolved BEFORE the `ui.input` closure so the memory and input locks are never nested.
        let keyboard_free = !ctx.memory(|m| m.focused().is_some());

        // Gather pointer / button / wheel input for this frame.
        let input = ui.input(|i| CanvasInput {
            hover_pos: i.pointer.hover_pos(),
            primary_down: i.pointer.primary_down(),
            primary_pressed: i.pointer.primary_pressed(),
            primary_released: i.pointer.primary_released(),
            middle_down: i.pointer.middle_down(),
            space_down: i.key_down(egui::Key::Space),
            pointer_delta: i.pointer.delta(),
            scroll_y: i.smooth_scroll_delta.y,
            modifiers: i.modifiers,
            // `key_pressed` only observes the event; it does not consume it, so no other handler
            // (widgets use `consume_key` while focused) loses its key.
            cancel_pressed: keyboard_free && i.key_pressed(egui::Key::Escape),
            remove_point_pressed: keyboard_free
                && (i.key_pressed(egui::Key::Backspace) || i.key_pressed(egui::Key::Delete)),
        });
        // The canvas now fills the WHOLE program-tab area and the dock panels float over it, so
        // "the pointer is inside the canvas rect" is no longer the same question as "the pointer is
        // on bare canvas". Everything derived from the pointer — the wheel, the zoom anchor and the
        // routing to the active tool — is gated on this instead of on `response.hovered()` alone;
        // without it a click on a panel would paint underneath it.
        let pointer_occluded = input
            .hover_pos
            .is_some_and(|pos| self.canvas_pointer_occluded(ctx, pos));
        let hovered = response.hovered() && !pointer_occluded;
        let pointer_in_viewport = hovered && input.hover_pos.is_some_and(|p| rect.contains(p));
        // Panning is deliberately NOT gated on `pointer_occluded`: the gate answers where the
        // pointer is RIGHT NOW, and a pan that started on bare canvas must survive the pointer
        // crossing a panel. What it costs is a middle-drag begun on a panel also panning — the same
        // behaviour this tab had before the panels floated, and harmless (a pan destroys nothing).
        let pan_active = input.middle_down || (input.space_down && input.primary_down);

        // Wheel: Shift+wheel adjusts the brush; plain wheel zooms toward the cursor.
        let mut wheel_for_zoom = 0.0;
        if hovered && input.scroll_y.abs() > f32::EPSILON {
            if input.modifiers.shift && self.active_tool_id() == PsToolId::Brush {
                if let Some(brush) = self.brush_tool_mut() {
                    brush.handle_wheel(input.scroll_y, input.modifiers);
                }
            } else {
                wheel_for_zoom = input.scroll_y;
            }
        }
        let pan_delta = if pan_active {
            input.pointer_delta
        } else {
            Vec2::ZERO
        };
        let anchor = input.hover_pos.filter(|p| rect.contains(*p));
        self.viewport
            .handle_input(rect, anchor, wheel_for_zoom, pan_delta);

        if self.active_tool_id() == PsToolId::Brush {
            self.brush_size_shortcuts(ctx);
        }

        let view = self.viewport.transform(rect);

        // With the Transform tool, dragging a typing text layer translates it (and writes the new
        // position back to text_info.json on release). This takes priority over raster transforms
        // when the press lands on a text layer, since text sits on top.
        let text_drag_active = !pan_active
            && self.active_tool_id() == PsToolId::Transform
            && self.handle_text_layer_drag(&input, &view, pointer_in_viewport, project);

        // Hint once on a fresh brush press when the active raster still shows an effects chain: it
        // is not directly editable until baked (`active_editable_mut` will refuse the paint).
        if !pan_active
            && input.primary_pressed
            && pointer_in_viewport
            && self.active_tool_id() == PsToolId::Brush
            && self
                .stack
                .as_ref()
                .and_then(|s| s.layer(s.active_id()))
                .is_some_and(|l| l.kind == LayerKind::Raster && !l.can_edit_pixels())
        {
            crate::runtime_log::log_warn("[ps_editor] Сначала запеките слой");
        }

        // Route input to the active tool unless the user is panning or dragging a text layer.
        if !pan_active && !text_drag_active {
            // Snapshot the active raster's transform/deform at gesture START (before the tool mutates
            // the stack this frame), so the release commit records ONE undo step per gesture — not one
            // per drag frame. Cleared/consumed in the matching release blocks below.
            if input.primary_pressed && self.active_tool_id() == PsToolId::Transform {
                self.transform_gesture_before = self
                    .stack
                    .as_ref()
                    .and_then(|s| s.layer(s.active_id()))
                    .filter(|l| l.kind == LayerKind::Raster)
                    .map(|l| (l.uid.to_string(), l.transform));
            }
            if input.primary_pressed && self.active_tool_id() == PsToolId::Deform {
                self.deform_gesture_before = self
                    .stack
                    .as_ref()
                    .and_then(|s| s.layer(s.active_id()))
                    .filter(|l| l.kind == LayerKind::Raster)
                    .map(|l| (l.uid.to_string(), l.deform.clone()));
            }
            let outcome = if let Some(stack) = self.stack.as_mut() {
                let pointer_image = input.hover_pos.map(|p| view.screen_to_world(p));
                let mut tool_ctx = PsToolContext {
                    page_size,
                    pointer_image,
                    pointer_in_viewport,
                    primary_pressed: input.primary_pressed && pointer_in_viewport,
                    primary_down: input.primary_down,
                    primary_released: input.primary_released,
                    view,
                    stack,
                    selection: &mut self.selection,
                    modifiers: input.modifiers,
                    cancel_pressed: input.cancel_pressed,
                    remove_point_pressed: input.remove_point_pressed,
                };
                self.tools[self.active_tool_idx].interact(&mut tool_ctx)
            } else {
                ToolOutcome::default()
            };
            // Log tool routing only on genuine activity (press / release / a selection change),
            // never on every idle or mid-drag frame — per-stroke detail lives in the tools.
            if crate::trace::trace_enabled()
                && (input.primary_pressed || input.primary_released || outcome.selection_changed)
            {
                let pi = input.hover_pos.map(|p| view.screen_to_world(p));
                crate::trace_log!(
                    cat::INPUT,
                    "tool_interact tool={:?} pressed={} released={} ptr={:?} dirty={:?} sel_changed={}",
                    self.active_tool_id(),
                    input.primary_pressed,
                    input.primary_released,
                    pi.map(|p| (p.x.round() as i32, p.y.round() as i32)),
                    outcome.dirty.map(|d| (d.min_x, d.min_y, d.max_x, d.max_y)),
                    outcome.selection_changed
                );
            }
            self.apply_tool_outcome(outcome);

            // Accumulate the active brush stroke's per-segment dirty rects into a union so the release
            // commit can build a region-bounded undo diff. Reset on the press frame (which also paints
            // the first stamp, so reset must run before accumulating this frame's dirty rect).
            if self.active_tool_id() == PsToolId::Brush {
                if input.primary_pressed {
                    self.brush_stroke_dirty = None;
                }
                if let Some(d) = outcome.dirty {
                    self.brush_stroke_dirty = Some(match self.brush_stroke_dirty {
                        Some(u) => tools::DirtyRect {
                            min_x: u.min_x.min(d.min_x),
                            min_y: u.min_y.min(d.min_y),
                            max_x: u.max_x.max(d.max_x),
                            max_y: u.max_y.max(d.max_y),
                        },
                        None => d,
                    });
                }
            }

            // Transform tool release on a raster: commit the live (stack-mutated) transform to the
            // shared doc so the move/rotate/scale is the model truth (and cross-tab visible).
            if input.primary_released
                && self.active_tool_id() == PsToolId::Transform
                && let Some(page_idx) = self.active_page_idx
                && let Some((uid, after_lt)) = self
                    .stack
                    .as_ref()
                    .and_then(|s| s.layer(s.active_id()))
                    .filter(|l| l.kind == LayerKind::Raster)
                    .map(|l| (l.uid.to_string(), l.transform))
            {
                crate::trace_log!(cat::SYNC, "commit transform page={} uid={}", page_idx, uid);
                self.route_to_doc(page_idx, project, |doc| {
                    doc.set_transform(page_idx, &uid, transform_to_rec(after_lt));
                });
                // Record ONE `FieldPatch::Transform` for the completed gesture, if it actually moved
                // the same layer we snapshotted at press.
                if let Some((before_uid, before_lt)) = self.transform_gesture_before.take()
                    && before_uid == uid
                    && before_lt != after_lt
                {
                    self.history.record(PsEditOp::FieldPatch {
                        page_idx,
                        layer_uid: uid,
                        field: LayerFieldPatch::Transform {
                            before: before_lt,
                            after: after_lt,
                        },
                    });
                }
            }

            // Deform tool release on a raster: commit the live mesh grid (stack-mutated) to the
            // shared doc as the model truth. The tool seeds an identity grid on entry, so even a
            // bare click persists the (no-op) mesh; subsequent grid-point drags persist the warp.
            if input.primary_released
                && self.active_tool_id() == PsToolId::Deform
                && let Some(page_idx) = self.active_page_idx
                && let Some((uid, deform)) = self
                    .stack
                    .as_ref()
                    .and_then(|s| s.layer(s.active_id()))
                    .filter(|l| l.kind == LayerKind::Raster)
                    .map(|l| (l.uid.to_string(), l.deform.clone()))
            {
                crate::trace_log!(cat::SYNC, "commit deform page={} uid={}", page_idx, uid);
                self.route_to_doc(page_idx, project, |doc| {
                    doc.set_deform(page_idx, &uid, deform.clone());
                });
                // Record ONE `FieldPatch::Deform` for the completed gesture, if the mesh actually
                // changed on the same layer we snapshotted at press (entering deform mode seeds an
                // identity grid: None → Some(identity) is a real, undoable state change).
                if let Some((before_uid, before_deform)) = self.deform_gesture_before.take()
                    && before_uid == uid
                    && !deform_eq(&before_deform, &deform)
                {
                    self.history.record(PsEditOp::FieldPatch {
                        page_idx,
                        layer_uid: uid,
                        field: LayerFieldPatch::Deform {
                            before: before_deform,
                            after: deform,
                        },
                    });
                }
            }

            // Brush stroke commit: on pointer-up, record the reversible undo diff, then push the
            // painted pixels onward. During the stroke the local `image` is mutated live
            // (responsive) while `base_image` still holds the pre-stroke pixels, so
            // `record_brush_stroke` reads the "before" from `base_image` for free — no stroke-start
            // snapshot. A paintable raster has no effects, so base == display == painted pixels.
            //
            // The destination depends on the painted layer's KIND: a user raster commits to the
            // shared `LayerDoc` (the model truth, cross-tab visible); the `Клин` base layer is not a
            // doc node at all, so its stroke goes to the shared `CleanOverlaysModel` instead —
            // bounded to the stroke's dirty union, because a ribbon page can be ~800x19000 px.
            if input.primary_released
                && self.active_tool_id() == PsToolId::Brush
                && let Some(page_idx) = self.active_page_idx
            {
                // Read the stroke union BEFORE `record_brush_stroke`, which does not consume it but
                // is followed by the reset below.
                let stroke_union = self.brush_stroke_dirty;
                // Record BEFORE pushing the pixels onward (the push + next reprojection sync
                // base_image to the painted pixels, which would erase the "before").
                self.record_brush_stroke(page_idx);
                let painted = self
                    .stack
                    .as_ref()
                    .and_then(|s| s.layer(s.active_id()))
                    .filter(|l| l.pixels_dirty && l.can_edit_pixels())
                    .map(|l| (l.kind, l.uid.to_string(), l.image.clone()));
                match painted {
                    Some((LayerKind::Raster, uid, painted)) => {
                        let base = painted.clone();
                        crate::trace_log!(
                            cat::SYNC,
                            "commit brush_pixels page={} uid={}",
                            page_idx,
                            uid
                        );
                        self.route_to_doc(page_idx, project, |doc| {
                            doc.set_raster_pixels(page_idx, &uid, base, painted, Vec::new(), true);
                        });
                    }
                    Some((LayerKind::Clean, _, _)) => {
                        if let Some(union) = stroke_union {
                            self.write_clean_region_to_model(
                                page_idx,
                                union.min_x,
                                union.min_y,
                                union.max_x.saturating_sub(union.min_x) + 1,
                                union.max_y.saturating_sub(union.min_y) + 1,
                            );
                        }
                    }
                    // `Source` is never paintable (`can_edit_pixels`), and `None` means the stroke
                    // changed nothing.
                    Some((LayerKind::Source, _, _)) | None => {}
                }
                self.brush_stroke_dirty = None;
            }
        } else if input.cancel_pressed {
            // Esc means "abandon the outline" even while the canvas is panning: the tool never sees
            // this frame's input, so the tab performs the abandonment on its behalf.
            self.reset_active_tool();
        } else {
            // Input went to the pan / text drag instead of the tool. Freeze the active tool so a
            // multi-frame gesture survives: without this the release performed during the pan is
            // never delivered, and the first routed frame afterwards reads "button up, no release"
            // as an interrupted drag and discards the traced outline.
            if let Some(tool) = self.tools.get_mut(self.active_tool_idx) {
                tool.freeze();
            }
        }

        // Upload + composite layers bottom-to-top in unified band order (rasters + typing overlays).
        self.sync_render_cache();
        self.upload_layers(ctx);
        self.draw_composite(ctx, ui, &view);
        self.draw_selection_marquee(ui, &view);

        // Right-click menu on the selection: copy/cut from chosen layers.
        self.draw_selection_menu(&response, project);

        // Tool cursor / preview overlay. It obeys the SAME occlusion gate as the input path
        // (`pointer_occluded`, resolved once above): a brush circle that keeps tracking the cursor
        // under a floating panel advertises a stroke the tools already refuse to start there.
        // The exception is a gesture the tool has actually accepted — see `overlay_pointer` for why
        // that question goes to the tool and not to `input.primary_down`.
        let pointer_image = overlay_pointer(
            input.hover_pos,
            rect,
            pointer_occluded,
            self.tools[self.active_tool_idx].gesture_in_flight(),
        )
        .map(|p| view.screen_to_world(p));
        let painter = ui.painter_at(rect);
        self.tools[self.active_tool_idx].draw_overlay(&painter, &view, pointer_image);

        // Page border for orientation.
        let page_rect = view.world_rect_to_screen(Rect::from_min_size(
            Pos2::ZERO,
            Vec2::new(page_size[0] as f32, page_size[1] as f32),
        ));
        painter.rect_stroke(
            page_rect,
            CornerRadius::ZERO,
            egui::Stroke::new(1.0, Color32::from_gray(90)),
            egui::StrokeKind::Outside,
        );

        if response.hovered()
            || pan_active
            || self.pending_job_id.is_some()
            || self.raster_effects_state.is_some()
        {
            // Keep polling so a finished off-thread effects render is consumed promptly even with no
            // pointer activity.
            ctx.request_repaint();
        }
    }

    /// Applies a tool outcome: tile invalidation + selection-overlay refresh.
    fn apply_tool_outcome(&mut self, outcome: ToolOutcome) {
        if let Some(dirty) = outcome.dirty {
            let Some(active) = self.stack.as_ref().map(|s| s.active_id()) else {
                return;
            };
            if let Some(cache) = self.render_cache.get_mut(&active) {
                cache.mark_dirty_rect(dirty);
            }
            // A pixel-dirty outcome (e.g. a brush stroke) edited the active layer's base pixels, so
            // a save must rewrite its base PNG and bake in any non-destructive effects.
            if let Some(layer) = self.stack.as_mut().and_then(|s| s.layer_mut(active)) {
                layer.pixels_dirty = true;
            }
        }
        // The marquee is drawn from `Selection::outline_loops`, set by the tools, so a changed
        // selection needs no extra rebuild here.
    }

    /// Ensures a render cache entry exists for every current layer and drops stale ones.
    fn sync_render_cache(&mut self) {
        let Some(stack) = &self.stack else {
            return;
        };
        let ids: Vec<LayerId> = stack.layers().iter().map(|l| l.id).collect();
        self.render_cache.retain(|id, _| ids.contains(id));
        for layer in stack.layers() {
            let size = layer.image.size;
            // Recreate the cache when a layer's image was resized (e.g. a freshly cropped clip).
            let needs_new = self
                .render_cache
                .get(&layer.id)
                .is_none_or(|cache| !cache.matches_size(size));
            if needs_new {
                self.render_cache.insert(
                    layer.id,
                    TiledTexture::new(size, format!("ps_layer_{}", layer.id)),
                );
            }
        }
    }

    /// Uploads dirty layer tiles within the per-frame budget.
    fn upload_layers(&mut self, ctx: &egui::Context) {
        let Some(stack) = &self.stack else {
            return;
        };
        let mut budget = TILE_UPLOAD_BUDGET_PER_FRAME;
        for layer in stack.layers() {
            if budget == 0 {
                break;
            }
            if let Some(cache) = self.render_cache.get_mut(&layer.id) {
                let uploaded = cache.upload_budgeted(ctx, &layer.image, budget);
                budget = budget.saturating_sub(uploaded);
            }
        }
    }

    /// Composites everything bottom-to-top in unified band order: the locked base layers first,
    /// then raster layers and typing overlays interleaved by their band Z (`self.bands`). Unsaved
    /// rasters / overlays without a band sit on top. Within a text group, overlays sub-order by
    /// page-Y (lower on the page = higher in the stack), matching the typing tab.
    fn draw_composite(
        &mut self,
        ctx: &egui::Context,
        ui: &egui::Ui,
        view: &viewport::ViewTransform,
    ) {
        enum Step {
            Raster { id: LayerId, opacity: f32 },
            Text { index: usize, opacity: f32 },
        }

        // Unified-group visibility/opacity, folded over both rasters (via the stack) and texts.
        let group_meta: HashMap<String, (bool, f32)> = self
            .stack
            .as_ref()
            .map(|s| {
                s.groups()
                    .iter()
                    .map(|g| (g.uid.to_string(), (g.visible, g.opacity)))
                    .collect()
            })
            .unwrap_or_default();

        // Band Z lookups (owned, so the `self.bands` borrow ends before the plan/borrow dance).
        let mut raster_z: HashMap<String, u32> = HashMap::new();
        let mut group_z: HashMap<u32, u32> = HashMap::new();
        let mut pinned_z: HashMap<String, u32> = HashMap::new();
        for band in &self.bands {
            match band {
                Band::Raster { uid, z } => {
                    raster_z.insert(uid.clone(), *z);
                }
                Band::TextGroup { layer_idx, z, .. } => {
                    group_z.insert(*layer_idx, *z);
                }
                Band::PinnedText { uid, z } => {
                    pinned_z.insert(uid.clone(), *z);
                }
            }
        }
        let top_z = self.bands.len() as u32;

        let painter = ui.painter_at(view.viewport_rect);

        // Base layers (source/clean) are always the bottom and are not bands.
        let mut plan: Vec<(u32, f32, Step)> = Vec::new();
        {
            let Some(stack) = self.stack.as_ref() else {
                return;
            };
            // Transparency checkerboard under the page: a transparent hole must read as a hole,
            // not as the viewport void. Drawn before every layer so it stays the bottom-most mark.
            layer_render::draw_page_checkerboard(&painter, ctx, view, stack.size());
            for layer in stack.layers() {
                if !stack.layer_visible(layer) {
                    continue;
                }
                let opacity = stack.layer_opacity(layer);
                if opacity <= 0.0 {
                    continue;
                }
                if layer.kind.is_base() {
                    if let Some(cache) = self.render_cache.get(&layer.id) {
                        cache.draw(&painter, view, opacity, layer);
                    }
                    continue;
                }
                let z = raster_z
                    .get(&layer.uid.to_string())
                    .copied()
                    .unwrap_or(top_z);
                plan.push((
                    z,
                    0.0,
                    Step::Raster {
                        id: layer.id,
                        opacity,
                    },
                ));
            }
        }
        for (index, layer) in self.text_layers.iter().enumerate() {
            if !layer.visible {
                continue;
            }
            // Fold the unified group: skip a hidden group, dim by its opacity.
            let mut group_opacity = 1.0;
            if let Some(uid) = &layer.group_uid {
                match group_meta.get(uid) {
                    Some((false, _)) => continue,
                    Some((_, op)) => group_opacity = *op,
                    None => {}
                }
            }
            if group_opacity <= 0.0 {
                continue;
            }
            let z = if layer.pinned {
                pinned_z.get(&layer.uid).copied()
            } else {
                group_z.get(&layer.layer_idx).copied()
            }
            .unwrap_or(top_z);
            plan.push((
                z,
                layer.center().y,
                Step::Text {
                    index,
                    opacity: group_opacity,
                },
            ));
        }
        plan.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));

        // The composite is rebuilt every frame (no cache), so only emit when the plan size changes
        // to avoid a 60/s flood. `usize::MAX` sentinel forces the first frame to log.
        if crate::trace::trace_enabled() && plan.len() != self.trace_last_composite_steps {
            crate::trace_log!(
                cat::RENDER,
                "draw_composite page={:?} steps={}",
                self.active_page_idx,
                plan.len()
            );
            self.trace_last_composite_steps = plan.len();
        }

        for (_, _, step) in plan {
            match step {
                Step::Raster { id, opacity } => {
                    if let (Some(stack), Some(cache)) =
                        (self.stack.as_ref(), self.render_cache.get(&id))
                        && let Some(layer) = stack.layer(id)
                    {
                        if layer.deform.is_some() {
                            cache.draw_deform(&painter, view, opacity, layer);
                        } else {
                            cache.draw(&painter, view, opacity, layer);
                        }
                    }
                }
                Step::Text { index, opacity } => {
                    if let Some(layer) = self.text_layers.get_mut(index) {
                        layer.draw(ctx, &painter, view, opacity);
                    }
                }
            }
        }
    }

    /// Drags the topmost text layer under the pointer (translate only). Returns true while a drag is
    /// active so the raster tool is bypassed. On release, routes the new transform to the shared doc
    /// (bumping its version so the typing tab re-projects) and flushes the doc's inline text payload to
    /// `layers.json`.
    fn handle_text_layer_drag(
        &mut self,
        input: &CanvasInput,
        view: &viewport::ViewTransform,
        pointer_in_viewport: bool,
        project: &ProjectData,
    ) -> bool {
        let pointer_world = input.hover_pos.map(|p| view.screen_to_world(p).to_vec2());

        if self.dragging_text_layer.is_none() {
            if input.primary_pressed
                && pointer_in_viewport
                && let Some(world) = pointer_world
                && let Some(idx) = self
                    .text_layers
                    .iter()
                    .rposition(|l| l.visible && !l.has_deform() && l.contains_world(world))
            {
                self.dragging_text_layer = Some(idx);
                self.text_drag_mode = if input.modifiers.shift {
                    TextDragMode::Rotate
                } else if input.modifiers.command {
                    TextDragMode::Scale
                } else {
                    TextDragMode::Translate
                };
                self.text_drag_last = world;
                if let Some(layer) = self.text_layers.get(idx) {
                    let c = layer.center();
                    self.text_drag_ref = match self.text_drag_mode {
                        TextDragMode::Rotate => (world.y - c.y).atan2(world.x - c.x),
                        TextDragMode::Scale => (world - c).length(),
                        TextDragMode::Translate => 0.0,
                    };
                }
            }
            return self.dragging_text_layer.is_some();
        }

        let Some(idx) = self.dragging_text_layer else {
            return false;
        };
        if input.primary_down {
            if let (Some(world), Some(layer)) = (pointer_world, self.text_layers.get_mut(idx)) {
                match self.text_drag_mode {
                    TextDragMode::Translate => layer.translate(world - self.text_drag_last),
                    TextDragMode::Rotate => {
                        let c = layer.center();
                        let angle = (world.y - c.y).atan2(world.x - c.x);
                        layer.rotate_by(angle - self.text_drag_ref);
                        self.text_drag_ref = angle;
                    }
                    TextDragMode::Scale => {
                        let c = layer.center();
                        let dist = (world - c).length();
                        if self.text_drag_ref > 1e-3 {
                            layer.scale_by(dist / self.text_drag_ref);
                        }
                        self.text_drag_ref = dist.max(1e-3);
                    }
                }
                self.text_drag_last = world;
            }
            return true;
        }

        // Released: persist the new placement. Read the layer data out first so the `text_layers`
        // borrow ends before `edit_doc_node` takes `&mut self`.
        let target = self
            .text_layers
            .get(idx)
            .map(|l| (l.uid.clone(), l.center(), l.rotation(), l.scale()));
        if let Some((uid, center, rotation, scale)) = target {
            // Update the shared doc's Text node transform in memory (bumping its version, so the typing
            // tab re-projects), then flush the doc's INLINE text payload to `layers.json`. The doc is
            // the sole text writer — PS no longer writes `text_info.json`.
            if let Some(page_idx) = self.active_page_idx {
                let rec = TransformRec {
                    cx: center.x,
                    cy: center.y,
                    rotation,
                    scale,
                };
                self.edit_doc_node(page_idx, |doc| {
                    doc.set_transform(page_idx, &uid, rec);
                });
                self.flush_text_page(page_idx, project);
            }
        }
        self.dragging_text_layer = None;
        true
    }

    /// Draws the selection as a thin alternating black/white dashed marquee along its boundary
    /// loops, instead of a translucent fill. The pattern is static (no animation).
    ///
    /// The loops come from `Selection::outline_loops` and follow the mask's PIXEL EDGES, so a
    /// freehand lasso arrives as a staircase of one-pixel steps. Two consequences shape this code
    /// and both run on the GUI thread every frame:
    /// - the dash phase must accumulate along the WHOLE loop (see `walk_dash_runs`), otherwise the
    ///   pattern restarts on every one-pixel step and the marquee degenerates into a solid line;
    /// - the point count is unbounded, so the screen-space path is decimated (below) and the total
    ///   number of emitted runs is capped by `MARQUEE_MAX_RUNS`.
    ///
    /// All loops share one shape buffer and a single `Painter::extend`.
    fn draw_selection_marquee(&self, ui: &egui::Ui, view: &viewport::ViewTransform) {
        let Some(selection) = self.selection.as_ref() else {
            return;
        };
        if !selection.any() {
            return;
        }
        let painter = ui.painter_at(view.viewport_rect);
        let mut shapes: Vec<egui::Shape> = Vec::new();
        let mut screen: Vec<Pos2> = Vec::new();
        let mut budget = MARQUEE_MAX_RUNS;
        for loop_pts in selection.outline_loops() {
            if budget == 0 {
                break;
            }
            if loop_pts.len() < 2 {
                continue;
            }
            screen.clear();
            decimate_to_screen(loop_pts, view, &mut screen);
            let emitted = walk_dash_runs(&screen, MARQUEE_DASH_PX, budget, |run| {
                let color = if run.black {
                    Color32::BLACK
                } else {
                    Color32::WHITE
                };
                shapes.push(egui::Shape::line_segment(
                    [run.from, run.to],
                    Stroke::new(1.0, color),
                ));
            });
            budget -= emitted;
        }
        painter.extend(shapes);
    }

    /// Right-click menu on the canvas offering copy/cut of the selection from chosen layers.
    ///
    /// The touched-layer list is recomputed once per menu open (on the secondary click) so the
    /// per-frame menu closure stays cheap even for large selections.
    fn draw_selection_menu(&mut self, response: &egui::Response, project: &ProjectData) {
        if response.secondary_clicked() {
            self.refresh_clip_touched_layers();
            self.clip_selected_layers.clear();
        }
        let has_selection = self.selection.as_ref().is_some_and(Selection::any);
        egui::Popup::context_menu(response)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                if !has_selection {
                    ui.label(t!("ps_editor.selection_menu.no_selection"));
                    return;
                }
                ui.menu_button(t!("ps_editor.selection_menu.copy"), |ui| {
                    self.clip_op_submenu(ui, ClipMode::Copy, project)
                });
                ui.menu_button(t!("ps_editor.selection_menu.cut"), |ui| {
                    self.clip_op_submenu(ui, ClipMode::Cut, project)
                });
            });
    }

    /// One copy/cut submenu: top layer, a multi-select layer picker, or all layers.
    fn clip_op_submenu(&mut self, ui: &mut egui::Ui, mode: ClipMode, project: &ProjectData) {
        let touched = self.clip_touched_layers.clone();
        if ui
            .add_enabled(
                !touched.is_empty(),
                egui::Button::new(t!("ps_editor.selection_menu.clip_from_top_layer")),
            )
            .clicked()
        {
            if let Some(&top) = touched.last() {
                self.perform_clip(mode, &[top], project);
            }
            egui::Popup::close_all(ui.ctx());
        }
        ui.menu_button(t!("ps_editor.selection_menu.clip_from_layers"), |ui| {
            self.clip_layer_picker(ui, mode, &touched, project);
        });
        if ui
            .button(t!("ps_editor.selection_menu.clip_from_all_layers"))
            .clicked()
        {
            let all: Vec<LayerId> = self
                .stack
                .as_ref()
                .map(|stack| stack.layers().iter().map(|layer| layer.id).collect())
                .unwrap_or_default();
            self.perform_clip(mode, &all, project);
            egui::Popup::close_all(ui.ctx());
        }
    }

    /// Multi-select picker listing the layers the selection touches (top-to-bottom in the UI).
    fn clip_layer_picker(
        &mut self,
        ui: &mut egui::Ui,
        mode: ClipMode,
        touched: &[LayerId],
        project: &ProjectData,
    ) {
        if touched.is_empty() {
            ui.label(t!("ps_editor.selection_menu.no_layers_touched"));
            return;
        }
        // Display top-to-bottom (reverse of the bottom-to-top stack order).
        for &id in touched.iter().rev() {
            let name = self
                .stack
                .as_ref()
                .and_then(|stack| stack.layer(id))
                .map_or_else(
                    || tf!("ps_editor.selection_menu.layer_fallback", id = id),
                    |layer| layer.name.clone(),
                );
            let mut checked = self.clip_selected_layers.contains(&id);
            if ui.checkbox(&mut checked, name).changed() {
                if checked {
                    self.clip_selected_layers.insert(id);
                } else {
                    self.clip_selected_layers.remove(&id);
                }
            }
        }
        ui.separator();
        let count = self.clip_selected_layers.len();
        let label = format!("{} ({count})", mode.verb());
        if ui
            .add_enabled(count > 0, egui::Button::new(label))
            .clicked()
        {
            // Preserve bottom-to-top stack order for compositing.
            let ids: Vec<LayerId> = touched
                .iter()
                .copied()
                .filter(|id| self.clip_selected_layers.contains(id))
                .collect();
            self.perform_clip(mode, &ids, project);
            egui::Popup::close_all(ui.ctx());
        }
    }

    /// Recomputes which layers the current selection overlaps (bottom-to-top).
    fn refresh_clip_touched_layers(&mut self) {
        let mut touched = Vec::new();
        if let (Some(stack), Some(selection)) = (self.stack.as_ref(), self.selection.as_ref())
            && let Some(bounds) = selection.bounds()
        {
            for layer in stack.layers() {
                if layer_touches_selection(layer, selection, bounds) {
                    touched.push(layer.id);
                }
            }
        }
        self.clip_touched_layers = touched;
    }

    /// Copies (or, for [`ClipMode::Cut`], moves) the selection from `layer_ids` (bottom-to-top)
    /// into a new raster layer composited from those layers in order.
    ///
    /// For a cut, the selected pixels are cleared from every chosen layer **except** the locked
    /// source layer, which is immutable and can never be cut from; the clean overlay and raster
    /// layers are cleared normally. Cleared `Клин` pixels are written back to the shared
    /// `CleanOverlaysModel` (it is not a doc node), so a cut from it survives a page switch — the
    /// same write-back path the brush commit and merge-into use.
    fn perform_clip(&mut self, mode: ClipMode, layer_ids: &[LayerId], project: &ProjectData) {
        let Some(page_idx) = self.active_page_idx else {
            return;
        };
        // The selection bounds bound the Clean write-back below (a ribbon page is far too large to
        // push whole).
        let bounds = {
            let (stack, selection) = match (self.stack.as_mut(), self.selection.as_ref()) {
                (Some(stack), Some(selection)) if selection.any() => (stack, selection),
                _ => return,
            };
            match clip_into_new_layer(stack, selection, mode, layer_ids) {
                Some(bounds) => bounds,
                None => return,
            }
        };

        // The new clip layer is now the active raster on the stack; mirror it as a doc node. For a
        // cut, also push the cleared base pixels of every source RASTER back to the doc (dirty). The
        // clean base layer (cut from but not a doc node) is unaffected here. `route_to_doc` flushes +
        // bumps + re-projects.
        let new_node = self
            .stack
            .as_ref()
            .and_then(|s| s.layer(s.active_id()))
            .map(layer_to_raster_node);
        // Snapshot cleared source rasters (cut only): uid -> cleared pixels.
        let cleared: Vec<(String, ColorImage)> = if mode == ClipMode::Cut {
            self.stack
                .as_ref()
                .map(|s| {
                    layer_ids
                        .iter()
                        .filter_map(|id| s.layer(*id))
                        .filter(|l| l.kind == LayerKind::Raster)
                        .map(|l| (l.uid.to_string(), l.image.clone()))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if let Some(node) = new_node {
            self.route_to_doc(page_idx, project, |doc| {
                for (uid, img) in &cleared {
                    doc.set_raster_pixels(
                        page_idx,
                        uid,
                        img.clone(),
                        img.clone(),
                        Vec::new(),
                        true,
                    );
                }
                doc.add_node(page_idx, node);
            });
        }

        // A cut clears pixels from the chosen layers; re-upload them fully (the cleared region in
        // a transformed layer is not axis-aligned). The fresh layer's cache entry is created by
        // `sync_render_cache` next frame, already fully dirty.
        if mode == ClipMode::Cut {
            for &id in layer_ids {
                if let Some(cache) = self.render_cache.get_mut(&id) {
                    cache.mark_all_dirty();
                }
            }
            // `Клин` was cut from but has no doc node, so the routing above skipped it. Without this
            // the cleared pixels lived only in the tab's stack and died on the next page switch or
            // overlay-revision reload.
            let cut_clean = self.stack.as_ref().is_some_and(|s| {
                layer_ids
                    .iter()
                    .filter_map(|id| s.layer(*id))
                    .any(|l| l.kind == LayerKind::Clean)
            });
            if cut_clean {
                self.write_clean_region_to_model(
                    page_idx,
                    bounds.min_x,
                    bounds.min_y,
                    bounds.max_x.saturating_sub(bounds.min_x) + 1,
                    bounds.max_y.saturating_sub(bounds.min_y) + 1,
                );
            }
        }
    }

    fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Merges the raster layer `id` down onto the layer directly beneath it on the unified band-Z
    /// axis. The lower layer becomes a page-sized identity layer holding both composited (src-over);
    /// the upper layer is removed. No-op when `id` is a base layer or has nothing beneath it.
    ///
    /// The lower participant may be the `Клин` base layer (the bottom-most raster merges into it),
    /// in which case the merged pixels go to the shared `CleanOverlaysModel` instead of the doc —
    /// `Клин` is not a doc node — bounded to the upper layer's page footprint
    /// ([`page_footprint_rect`]), which is the only region a merge can change. `Исходник` is never a
    /// target. The UPPER participant is always a user raster.
    ///
    /// NOT undoable (by design, like the cut): it removes a layer AND rewrites another's pixels,
    /// which needs a batch op the history does not have yet.
    fn merge_down(&mut self, id: LayerId, project: &ProjectData) -> bool {
        let Some(page_idx) = self.active_page_idx else {
            return false;
        };
        // Pick the layer directly BELOW by unified band-Z (the visually-below raster), not the stack
        // neighbor — so a manual reorder merges the pair the user actually sees. Resolve this before
        // borrowing the stack to keep the borrow checker happy.
        let Some(below_uid) = self.raster_below_uid(id) else {
            return false;
        };
        let Some(stack) = self.stack.as_ref() else {
            return false;
        };
        let size = stack.size();
        // Build the merged pixels and resolve both participants' ids/uids (the borrow ends here).
        let (below_id, below_kind, upper_uid, upper_footprint, merged) = {
            let layers = stack.layers();
            let Some(upper) = layers.iter().find(|l| l.id == id) else {
                return false;
            };
            let Some(below) = layers.iter().find(|l| l.uid.to_string() == below_uid) else {
                return false;
            };
            // The UPPER participant must be a user raster (base layers are never merged away). The
            // lower one may be `Клин`, but never `Исходник` — which `merge_candidates_by_band_z`
            // already excludes; the check is repeated here so the invariant is enforced at the
            // mutation site and not only by the candidate query.
            if upper.kind.is_base() || below.kind == LayerKind::Source {
                return false;
            }
            // A merge rewrites the lower layer's base pixels, so refuse while either participant
            // still shows a non-destructive effects chain — bake it first.
            if !upper.effects.is_empty() || !below.effects.is_empty() {
                crate::runtime_log::log_warn("[ps_editor] Сначала запеките слой");
                return false;
            }
            // Bottom-to-top: below then upper, so the upper composites OVER the below.
            let merged = composite_to_page(&[below, upper], size);
            (
                below.id,
                below.kind,
                upper.uid.to_string(),
                page_footprint_rect(upper, size),
                merged,
            )
        };
        // Record the upper raster's deletion (so the manifest save drops it — `save_page_rasters`
        // preserves unowned rasters otherwise), then absorb the composited pixels into the lower
        // participant. Which store that is depends on its kind: a raster becomes a page-sized
        // identity doc node (pixels_dirty), while `Клин` — not a doc node — takes the pixels on the
        // stack and pushes them to the shared clean model. Either way the upper raster's doc node is
        // removed and `persist_current_page` (below) writes the manifest with the removed uid.
        self.record_raster_deletion(id);
        match below_kind {
            LayerKind::Raster => {
                let merged_for_doc = merged;
                let identity = transform_to_rec(LayerTransform::identity_for(size));
                let below_uid_for_doc = below_uid.clone();
                self.edit_doc_node(page_idx, |doc| {
                    doc.set_raster_pixels(
                        page_idx,
                        &below_uid_for_doc,
                        merged_for_doc.clone(),
                        merged_for_doc,
                        Vec::new(),
                        true,
                    );
                    doc.set_transform(page_idx, &below_uid_for_doc, identity);
                    doc.remove_node(page_idx, &upper_uid);
                });
            }
            LayerKind::Clean => {
                // `Клин` is not a doc node, so only the upper raster is removed there; the merged
                // pixels are absorbed by the base layer and pushed to the shared overlay model.
                self.edit_doc_node(page_idx, |doc| {
                    doc.remove_node(page_idx, &upper_uid);
                });
                if let Some(layer) = self.stack.as_mut().and_then(|s| s.layer_mut(below_id)) {
                    layer.transform = LayerTransform::identity_for(size);
                    layer.base_image = merged.clone();
                    layer.image = merged;
                }
                // Push ONLY the upper layer's page footprint, not the whole page. Everywhere else
                // `composite_to_page` reproduces the `Клин` pixels unchanged (the upper sample is
                // transparent there and `Клин` is a page-sized identity layer), so a bounded write
                // is complete — and a whole-page write on a ~800x19000 ribbon means two full RGBA
                // copies plus a tiled zstd diff of both, on the GUI thread and under the model lock
                // (the autosave worker shares it). An upper layer that really does cover the page
                // still costs the whole page; that is inherent to the merge it asked for.
                if let Some((fx, fy, fw, fh)) = upper_footprint {
                    self.write_clean_region_to_model(page_idx, fx, fy, fw, fh);
                }
            }
            // `raster_below_uid` never yields the immutable source layer.
            LayerKind::Source => return false,
        }
        self.persist_current_page(project);
        // Refresh caches and selection locally (the projection rebuilt the stack).
        self.render_cache.remove(&id);
        if let Some(cache) = self.render_cache.get_mut(&below_id) {
            cache.mark_all_dirty();
        }
        if let Some(stack) = self.stack.as_mut() {
            stack.set_active(below_id);
        }
        true
    }

    /// Mutable access to the brush tool instance, if present.
    fn brush_tool_mut(&mut self) -> Option<&mut BrushTool> {
        self.tools.iter_mut().find_map(|tool| tool.as_brush_mut())
    }

    fn brush_size_shortcuts(&mut self, ctx: &egui::Context) {
        if let Some(brush) = self.brush_tool_mut() {
            brush.handle_size_shortcuts(ctx);
        }
    }

    /// Handles tab-local tool/selection hotkeys. Called from the root hotkey dispatch.
    ///
    /// Letter shortcuts are suppressed while a widget holds keyboard focus so they do not fire
    /// while the user is interacting with a focused control.
    pub fn handle_hotkeys(&mut self, ctx: &egui::Context, project: &ProjectData) {
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        let (b, m, l, v, deselect, undo, redo) = ctx.input(|i| {
            let cmd = i.modifiers.command;
            (
                i.key_pressed(egui::Key::B),
                i.key_pressed(egui::Key::M),
                i.key_pressed(egui::Key::L),
                i.key_pressed(egui::Key::V),
                cmd && i.key_pressed(egui::Key::D),
                // Ctrl/Cmd+Z (without Shift) = undo.
                cmd && !i.modifiers.shift && i.key_pressed(egui::Key::Z),
                // Ctrl/Cmd+Shift+Z or Ctrl/Cmd+Y = redo.
                cmd && ((i.modifiers.shift && i.key_pressed(egui::Key::Z))
                    || i.key_pressed(egui::Key::Y)),
            )
        });
        if b && let Some(idx) = self.tool_index(PsToolId::Brush) {
            self.set_active_tool(idx);
        }
        if m && let Some(idx) = self.tool_index(PsToolId::SelectRect) {
            self.set_active_tool(idx);
        }
        if l && let Some(idx) = self.tool_index(PsToolId::SelectLasso) {
            self.set_active_tool(idx);
        }
        if v && let Some(idx) = self.tool_index(PsToolId::Transform) {
            self.set_active_tool(idx);
        }
        if deselect {
            self.clear_selection();
        }
        // Undo takes priority; the two are mutually exclusive for a given key event anyway.
        if undo {
            self.undo(project);
        } else if redo {
            self.redo(project);
        }
    }
}

/// The TAB-level shortcuts, already localized, for the «Горячие клавиши» panel.
///
/// Deliberately placed immediately next to [`PsEditorTabState::handle_hotkeys`] — which dispatches
/// the letter keys, Ctrl+D and undo/redo — and next to `draw_canvas`, which reads the pan buttons
/// and the wheel: the displayed list and the dispatched keys have no other coupling, so keeping
/// them apart is exactly how the two drift.
///
/// A TOOL's own shortcuts are not here: they come from `PsTool::hotkey_rows`, so a tool stays the
/// single owner of the keys it interprets. `PsToolId::Deform` has no tool letter today, which is
/// why the list names four tools and not five.
#[must_use]
fn ps_editor_common_hotkey_rows() -> Vec<tools::PsHotkeyRow> {
    use tools::PsHotkeyRow;
    vec![
        // Tool letters — `handle_hotkeys`, suppressed while a widget holds keyboard focus.
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.tool_brush_label"),
            t!("ps_editor.hotkeys.tool_brush_keys"),
        ),
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.tool_rect_select_label"),
            t!("ps_editor.hotkeys.tool_rect_select_keys"),
        ),
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.tool_lasso_label"),
            t!("ps_editor.hotkeys.tool_lasso_keys"),
        ),
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.tool_transform_label"),
            t!("ps_editor.hotkeys.tool_transform_keys"),
        ),
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.clear_selection_label"),
            t!("ps_editor.hotkeys.clear_selection_keys"),
        ),
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.undo_label"),
            t!("ps_editor.hotkeys.undo_keys"),
        ),
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.redo_label"),
            t!("ps_editor.hotkeys.redo_keys"),
        ),
        // Canvas gestures — `draw_canvas`: middle button or Space+LMB pans, the wheel zooms.
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.pan_label"),
            t!("ps_editor.hotkeys.pan_keys"),
        ),
        PsHotkeyRow::new(
            t!("ps_editor.hotkeys.zoom_label"),
            t!("ps_editor.hotkeys.zoom_keys"),
        ),
    ]
}

/// Snapshot of pointer/keyboard input read once per frame for the canvas.
struct CanvasInput {
    hover_pos: Option<Pos2>,
    primary_down: bool,
    primary_pressed: bool,
    primary_released: bool,
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

/// Source-over composite of premultiplied-alpha colors (`src` painted over `dst`).
///
/// egui's `Color32` stores premultiplied sRGB, so straight additive over-compositing is correct:
/// `out = src + dst * (1 - src_a)`.
fn over(src: Color32, dst: Color32) -> Color32 {
    let sa = src.a() as u32;
    if sa == 255 {
        return src;
    }
    if sa == 0 {
        return dst;
    }
    let inv = 255 - sa;
    let blend = |s: u8, d: u8| -> u8 { (s as u32 + (d as u32 * inv) / 255).min(255) as u8 };
    Color32::from_rgba_premultiplied(
        blend(src.r(), dst.r()),
        blend(src.g(), dst.g()),
        blend(src.b(), dst.b()),
        blend(src.a(), dst.a()),
    )
}

/// Nearest-neighbor sample of `layer` at page pixel `(wx, wy)` through its transform, or
/// transparent when the page point falls outside the layer image.
fn sample_layer_world(layer: &Layer, wx: usize, wy: usize) -> Color32 {
    let local = layer.world_to_local(Vec2::new(wx as f32 + 0.5, wy as f32 + 0.5));
    let lx = local.x.floor();
    let ly = local.y.floor();
    if lx < 0.0 || ly < 0.0 {
        return Color32::TRANSPARENT;
    }
    let (lx, ly) = (lx as usize, ly as usize);
    let [w, h] = layer.image.size;
    if lx >= w || ly >= h {
        return Color32::TRANSPARENT;
    }
    layer.image.pixels[ly * w + lx]
}

/// Selects the raster directly BENEATH `target_uid` on the unified band-Z axis (the layer the user
/// sees beneath it in the composite), for "merge down". This is the band-Z order — NOT the layer
/// stack index — so after a manual reorder the correct visually-below raster is chosen.
///
/// `rasters` lists every merge candidate as `(uid, band_z)` — see `merge_candidates_by_band_z`,
/// which supplies each raster's shifted `Band::Raster` Z and reserves Z 0 for `Клин`. `Исходник` is
/// excluded by the caller so it can never be a merge target. Among the candidates strictly below
/// the target's Z, the nearest one (greatest Z) wins; ties break toward the earlier list position
/// (stable, matching `draw_composite`'s constant raster tiebreak). Returns `None` when the target is
/// not found or is already the bottom-most candidate.
fn raster_below_by_band_z(rasters: &[(String, u32)], target_uid: &str) -> Option<String> {
    let target_z = rasters
        .iter()
        .find(|(uid, _)| uid == target_uid)
        .map(|(_, z)| *z)?;
    rasters
        .iter()
        .enumerate()
        .filter(|(_, (uid, z))| uid != target_uid && *z < target_z)
        // Nearest below = greatest Z; on a Z tie keep the earlier list position (lower stack index).
        .max_by(|(ia, (_, za)), (ib, (_, zb))| za.cmp(zb).then(ib.cmp(ia)))
        .map(|(_, (uid, _))| uid.clone())
}

/// Page-space pixel rect `(x, y, w, h)` bounding every pixel [`composite_to_page`] can take from
/// `layer`, clamped to a `size` page. `None` when the layer's footprint misses the page entirely.
///
/// This is a strict SUPERSET of the pixels the layer can alter, which is what makes it safe to
/// bound a merge write-back with: `composite_to_page` samples through [`sample_layer_world`], which
/// maps a page point back with the AFFINE `LayerTransform` only — `Layer::deform` is not consulted
/// there — so the four affine `world_corners()` bound every sample that can return a non-transparent
/// pixel, deformed layer or not. Should `sample_layer_world` ever become deform-aware, this bound
/// must grow with it or a merge will silently lose pixels.
///
/// The box is rounded OUTWARD by a whole pixel on each side: a page pixel contributes when its
/// CENTRE falls inside the quad, and the margin also absorbs the `f32` rounding of the corner math.
/// A non-finite corner (a degenerate transform) yields the whole page rather than a guess.
fn page_footprint_rect(layer: &Layer, size: [usize; 2]) -> Option<(usize, usize, usize, usize)> {
    let [pw, ph] = size;
    if pw == 0 || ph == 0 {
        return None;
    }
    let whole_page = Some((0, 0, pw, ph));
    let corners = layer.world_corners();
    if corners.iter().any(|c| !c.x.is_finite() || !c.y.is_finite()) {
        return whole_page;
    }
    let (page_w, page_h) = (pw as f32, ph as f32);
    let mut min = Vec2::new(f32::INFINITY, f32::INFINITY);
    let mut max = Vec2::new(f32::NEG_INFINITY, f32::NEG_INFINITY);
    for c in corners {
        min.x = min.x.min(c.x);
        min.y = min.y.min(c.y);
        max.x = max.x.max(c.x);
        max.y = max.y.max(c.y);
    }
    // Clamp into the page in float space first, so the casts below are on finite, non-negative
    // values no larger than the page dimensions — no truncation or sign loss is possible.
    let x0 = (min.x - 1.0).floor().clamp(0.0, page_w) as usize;
    let y0 = (min.y - 1.0).floor().clamp(0.0, page_h) as usize;
    let x1 = (max.x + 1.0).ceil().clamp(0.0, page_w) as usize;
    let y1 = (max.y + 1.0).ceil().clamp(0.0, page_h) as usize;
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    Some((x0, y0, x1 - x0, y1 - y0))
}

/// Composites `layers` (bottom-to-top, src-over) into a fresh page-sized image, sampling each
/// through its transform so rotated/scaled/incomplete layers contribute correctly.
fn composite_to_page(layers: &[&Layer], size: [usize; 2]) -> ColorImage {
    let [w, h] = size;
    let mut out = ColorImage::filled(size, Color32::TRANSPARENT);
    for y in 0..h {
        for x in 0..w {
            let mut px = Color32::TRANSPARENT;
            for layer in layers {
                px = over(sample_layer_world(layer, x, y), px);
            }
            out.pixels[y * w + x] = px;
        }
    }
    out
}

/// Composites `layer_ids` (bottom-to-top, src-over) within the selection into a new raster layer
/// **cropped to the selection bounds** and placed in page space, then pushed on top of `stack`.
/// For [`ClipMode::Cut`] the selected pixels are cleared from every chosen layer except the
/// immutable source. Layers are sampled through their transforms, so partial/rotated/scaled source
/// layers contribute correctly. Returns the selection bounds, or `None` when there is nothing to do.
fn clip_into_new_layer(
    stack: &mut LayerStack,
    selection: &Selection,
    mode: ClipMode,
    layer_ids: &[LayerId],
) -> Option<SelectionBounds> {
    let bounds = selection.bounds()?;
    if layer_ids.is_empty() {
        return None;
    }
    let crop_w = bounds.max_x - bounds.min_x + 1;
    let crop_h = bounds.max_y - bounds.min_y + 1;

    // Composite into a buffer cropped to the selection bounds (an "incomplete" layer).
    let mut pixels = vec![Color32::TRANSPARENT; crop_w * crop_h];
    for &id in layer_ids {
        let Some(layer) = stack.layer(id) else {
            continue;
        };
        for y in bounds.min_y..=bounds.max_y {
            for x in bounds.min_x..=bounds.max_x {
                if selection.contains(x, y) {
                    let idx = (y - bounds.min_y) * crop_w + (x - bounds.min_x);
                    pixels[idx] = over(sample_layer_world(layer, x, y), pixels[idx]);
                }
            }
        }
    }
    let clip_image = ColorImage::new([crop_w, crop_h], pixels);

    if mode == ClipMode::Cut {
        for &id in layer_ids {
            let Some(layer) = stack.layer_mut(id) else {
                continue;
            };
            // The source layer is immutable: it can be copied from but never cut.
            if layer.kind == LayerKind::Source {
                continue;
            }
            // A cut removes base pixels: refuse on a raster still showing a non-destructive effects
            // chain (bake it first). Copying from it is fine; only the destructive clear is blocked.
            if layer.kind == LayerKind::Raster && !layer.can_edit_pixels() {
                crate::runtime_log::log_warn("[ps_editor] Сначала запеките слой");
                continue;
            }
            clear_selected_pixels(layer, selection, bounds);
            layer.pixels_dirty = true; // pixels removed → bake effects, rewrite base on save
        }
    }

    let name = match mode {
        // Persisted raster-layer names (round-trip to `layers.json`); stable literals,
        // not localized. See docs/i18n_exclusions.md §A (persisted layer/group names).
        ClipMode::Copy => "Копия".to_string(),
        ClipMode::Cut => "Вырезка".to_string(),
    };
    // Place the cropped layer so it sits exactly where it was lifted from.
    let center = Vec2::new(
        (bounds.min_x + bounds.max_x + 1) as f32 * 0.5,
        (bounds.min_y + bounds.max_y + 1) as f32 * 0.5,
    );
    let transform = layers::LayerTransform {
        center,
        rotation: 0.0,
        scale: 1.0,
    };
    let new_id = stack.add_raster_layer_image(name, clip_image, transform);
    if let Some(layer) = stack.layer_mut(new_id) {
        layer.pixels_dirty = true; // freshly-composited PS pixels
    }
    Some(bounds)
}

/// Clears every pixel of `layer` whose page position falls inside the selection.
///
/// An identity (axis-aligned, page-placed) layer is cleared directly over the bounds; a
/// transformed layer is scanned in its own pixel space, each pixel mapped to page space.
fn clear_selected_pixels(layer: &mut Layer, selection: &Selection, bounds: SelectionBounds) {
    let [w, h] = layer.image.size;
    if layer.transform.is_identity_for(layer.image.size) {
        for y in bounds.min_y..=bounds.max_y.min(h.saturating_sub(1)) {
            let row = y * w;
            for x in bounds.min_x..=bounds.max_x.min(w.saturating_sub(1)) {
                if selection.contains(x, y) {
                    layer.image.pixels[row + x] = Color32::TRANSPARENT;
                }
            }
        }
        return;
    }
    let transform = layer.transform;
    let local_center = layer.image_size() * 0.5;
    for ly in 0..h {
        let row = ly * w;
        for lx in 0..w {
            let local = Vec2::new(lx as f32 + 0.5, ly as f32 + 0.5);
            let world = transform.center
                + rotate_vec((local - local_center) * transform.scale, transform.rotation);
            if world.x < 0.0 || world.y < 0.0 {
                continue;
            }
            if selection.contains(world.x as usize, world.y as usize) {
                layer.image.pixels[row + lx] = Color32::TRANSPARENT;
            }
        }
    }
}

/// Rotates `v` by `angle` radians (clockwise in image space, +y down).
fn rotate_vec(v: Vec2, angle: f32) -> Vec2 {
    let (s, c) = angle.sin_cos();
    Vec2::new(v.x * c - v.y * s, v.x * s + v.y * c)
}

/// Whether any opaque pixel of `layer` lies inside the selection within `bounds` (transform-aware).
fn layer_touches_selection(layer: &Layer, selection: &Selection, bounds: SelectionBounds) -> bool {
    for y in bounds.min_y..=bounds.max_y {
        for x in bounds.min_x..=bounds.max_x {
            if selection.contains(x, y) && sample_layer_world(layer, x, y).a() > 0 {
                return true;
            }
        }
    }
    false
}

/// Worker: renders a raster's effects chain from the supplied pre-effects base image
/// (non-destructive — the caller already cloned the base and dropped every lock). Runs the expensive
/// `apply_effects_to_color_image` off the GUI thread and returns the data the GUI-side apply step
/// needs (the new display image, the base content `origin` inside it, and the recenter references).
///
/// An empty `effects` chain means "clear effects": the result carries the base image unchanged with a
/// zero `origin`, so the GUI step restores the base placement.
///
/// # Errors
/// Returns a human-readable message string when the effects render fails (the caller logs it and
/// keeps the raster unchanged).
#[allow(clippy::too_many_arguments)]
// Justification: this is a pure data-shuttle for the worker thread; every argument is an independent
// piece of the already-resolved render context (no shared state to group them into) and bundling them
// into an ad-hoc struct would only add indirection without clarifying the contract.
fn render_ps_raster_effects(
    page_idx: usize,
    uid: String,
    id: LayerId,
    base_image: ColorImage,
    base_size: [usize; 2],
    base_t: LayerTransform,
    json: String,
    effects: Vec<serde_json::Value>,
) -> Result<PsRasterEffectsResult, String> {
    if effects.is_empty() {
        // No render needed: clearing effects restores the base. `origin` is the base top-left.
        return Ok(PsRasterEffectsResult {
            page_idx,
            uid,
            id,
            new_image: base_image,
            origin: [0, 0],
            base_size,
            base_t,
            effects,
        });
    }
    match effects::apply_effects_to_color_image(&base_image, &json) {
        Ok((new_image, origin)) => Ok(PsRasterEffectsResult {
            page_idx,
            uid,
            id,
            new_image,
            origin,
            base_size,
            base_t,
            effects,
        }),
        Err(err) => Err(err),
    }
}

/// World-space center offset that keeps a raster's original content anchored after a
/// non-destructive effects render resized the image (effects like shadow/glow grow the canvas and
/// shift the base content to `origin` inside the new image).
///
/// `new_size`/`base_size` are `[w, h]` in pixels; `origin` is the base content's top-left inside the
/// new image (the `[i32; 2]` content origin from `apply_effects_to_color_image`). The raw center
/// delta is taken in base-local pixels, then mapped through the base transform's scale + rotation so
/// it is added directly to `LayerTransform::center` in world space. Repeated re-applies stay stable
/// because the delta is always measured against the same pre-effects base reference.
#[must_use]
fn effects_recenter_offset(
    new_size: [usize; 2],
    origin: [i32; 2],
    base_size: [usize; 2],
    base_t: LayerTransform,
) -> Vec2 {
    let dx = new_size[0] as f32 * 0.5 - origin[0] as f32 - base_size[0] as f32 * 0.5;
    let dy = new_size[1] as f32 * 0.5 - origin[1] as f32 - base_size[1] as f32 * 0.5;
    let (sin, cos) = base_t.rotation.sin_cos();
    let scaled = Vec2::new(dx, dy) * base_t.scale;
    Vec2::new(
        scaled.x * cos - scaled.y * sin,
        scaled.x * sin + scaled.y * cos,
    )
}

/// Converts an in-memory layer transform to its on-disk record (center-anchored, page pixels).
fn transform_to_rec(t: LayerTransform) -> TransformRec {
    TransformRec {
        cx: t.center.x,
        cy: t.center.y,
        rotation: t.rotation,
        scale: t.scale,
    }
}

/// Converts a `[usize; 2]` pixel pair into `[u32; 2]`, or `None` if either component exceeds `u32`
/// (unreachable for realistic image dimensions). Keeps the raster-diff conversions free of lossy
/// `as` casts.
fn usize_pair_to_u32(pair: [usize; 2]) -> Option<[u32; 2]> {
    Some([u32::try_from(pair[0]).ok()?, u32::try_from(pair[1]).ok()?])
}

/// Structural equality of two optional deform meshes (`DeformRec` has no `PartialEq`): both `None`,
/// or both `Some` with equal grid dimensions and identical control points. Used to detect whether a
/// deform gesture actually changed the mesh before recording an undo step.
fn deform_eq(
    a: &Option<crate::models::layer_model::manifest::DeformRec>,
    b: &Option<crate::models::layer_model::manifest::DeformRec>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.cols == b.cols && a.rows == b.rows && a.points_px == b.points_px,
        (None, Some(_)) | (Some(_), None) => false,
    }
}

/// Inverse of [`transform_to_rec`].
fn rec_to_transform(r: TransformRec) -> LayerTransform {
    LayerTransform {
        center: Vec2::new(r.cx, r.cy),
        rotation: r.rotation,
        scale: r.scale,
    }
}

/// Builds a `LayerDoc` Raster `LayerNode` from a stack `Layer`, for adding/mirroring it into the doc
/// (its `z` is reassigned by `add_node`). Carries the layer's uid, name, visibility, opacity,
/// transform, group uid (resolved by the caller's stack), base/display pixels, and effects chain.
fn layer_to_raster_node(layer: &Layer) -> crate::models::layer_model::layer_doc::LayerNode {
    use crate::models::layer_model::layer_doc::{LayerNode, NodeBody, NodeKind};
    LayerNode {
        uid: layer.uid.to_string(),
        name: layer.name.clone(),
        kind: NodeKind::Raster,
        z: 0,
        visible: layer.visible,
        opacity: layer.opacity,
        group_uid: None,
        text_layer_idx: None,
        transform: transform_to_rec(layer.transform),
        deform: None,
        generation: 0,
        pixels_dirty: true,
        body: NodeBody::Raster {
            base_image: layer.base_image.clone(),
            display_image: layer.image.clone(),
            effects: layer.effects.clone(),
            base_file: format!("{}.png", layer.uid),
            // A PS-created raster (e.g. rasterize) defaults to no mask-clip.
            mask_clip: None,
        },
    }
}

/// Wraps a freshly built mask in the page-selection invariant: `Some` only when it selects at
/// least one pixel, `None` otherwise.
///
/// The pure decision behind `PsEditorTabState::set_selection`, and the tab-side twin of
/// `tools::normalize_selection_slot`. A `Some(all-zero)` selection is the phantom the whole
/// invariant exists to prevent: invisible (no marquee) but still clipping every brush stroke.
#[must_use]
fn non_empty_selection(selection: Selection) -> Option<Selection> {
    selection.any().then_some(selection)
}

/// Screen position the active tool's `draw_overlay` may follow this frame, or `None` to hide it.
///
/// The pure decision behind the overlay call in `draw_canvas`, split out so it can be exercised
/// without a GUI. Three terms:
/// * `hover_pos` outside `rect` — the pointer left the program-tab area entirely;
/// * `pointer_occluded` — a dock panel, popup or window covers the pointer, the same gate the
///   input path uses (`PsEditorTabState::canvas_pointer_occluded`);
/// * `gesture_in_flight` — the escape hatch that keeps a preview alive once its gesture legitimately
///   started on bare canvas and was then dragged over a panel, mirroring the panning rule.
///
/// The in-flight term is asked of the TOOL and must not be replaced by "the primary button is
/// down": a press that BEGINS on a panel also holds the button down, and the tools correctly refuse
/// to start a gesture there — so `primary_down` would keep painting the preview under the panel in
/// exactly the case this function exists to fix.
#[must_use]
fn overlay_pointer(
    hover_pos: Option<Pos2>,
    rect: Rect,
    pointer_occluded: bool,
    gesture_in_flight: bool,
) -> Option<Pos2> {
    hover_pos
        .filter(|p| rect.contains(*p))
        .filter(|_| !pointer_occluded || gesture_in_flight)
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

/// Maps an image-space boundary loop to screen space, dropping points that land closer than
/// `MARQUEE_MIN_STEP_PX` to the previously kept one.
///
/// `loop_pts` traces the mask's pixel edges, so at zoom < 1 a staircase contributes many sub-pixel
/// steps that no dash pattern can resolve; decimating them bounds the emitted run count by the
/// loop's SCREEN length instead of by the mask's complexity. At zoom >= 1 a one-pixel step is at
/// least one screen pixel, so nothing is dropped and the marquee stays pixel-accurate. The first
/// and last points are always kept, so a closed loop stays closed.
///
/// `out` is cleared by the caller and reused across loops.
fn decimate_to_screen(
    loop_pts: &[(f32, f32)],
    view: &viewport::ViewTransform,
    out: &mut Vec<Pos2>,
) {
    let Some((&first, rest)) = loop_pts.split_first() else {
        return;
    };
    let mut last_kept = view.world_to_screen(Pos2::new(first.0, first.1));
    out.push(last_kept);
    let last_idx = rest.len().saturating_sub(1);
    for (i, &(x, y)) in rest.iter().enumerate() {
        let p = view.world_to_screen(Pos2::new(x, y));
        // Keep the closing point unconditionally; otherwise keep only points that advance the
        // path by at least one decimation step.
        if i == last_idx || (p - last_kept).length() >= MARQUEE_MIN_STEP_PX {
            out.push(p);
            last_kept = p;
        }
    }
}

/// Walks a screen-space polyline and emits alternating black/white dash runs by CUMULATIVE arc
/// length, calling `emit` once per run. Returns the number of runs emitted (never above `budget`).
///
/// The dash phase is carried across segment boundaries, which is the whole point: a marquee path
/// traced along mask pixel edges is made of one-pixel segments, far shorter than `dash_len`, so
/// dashing each segment independently restarts the phase every pixel and paints a solid line. Here
/// one dash may span many segments and one segment may hold many dashes.
///
/// `dash_len` is in SCREEN pixels, so the pattern is zoom-invariant. A path shorter than one dash
/// emits a single black run. Returns 0 for a path with fewer than two points, a non-positive
/// `dash_len`, or a zero `budget`. Pure: no painter, no global state.
fn walk_dash_runs(
    path: &[Pos2],
    dash_len: f32,
    budget: usize,
    mut emit: impl FnMut(DashRun),
) -> usize {
    if path.len() < 2 || budget == 0 || !(dash_len.is_finite() && dash_len > 0.0) {
        return 0;
    }
    let mut emitted = 0usize;
    // Phase carried across segments: distance left in the current dash, and its colour.
    let mut dash_left = dash_len;
    let mut black = true;
    for pair in path.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let delta = b - a;
        let seg_len = delta.length();
        // A broken `ViewTransform` can map a point to infinity or NaN. An infinite `seg_len` never
        // shrinks below the loop threshold, so the inner loop would emit `budget` NaN runs from
        // this one segment; NaN would silently poison the phase. Skip such a segment entirely.
        if !seg_len.is_finite() || seg_len <= MARQUEE_MIN_RUN_PX {
            continue;
        }
        let dir = delta / seg_len;
        // Distance from `a` already emitted, and the part of this segment still to emit.
        let mut cursor = 0.0f32;
        let mut rest = seg_len;
        while rest > MARQUEE_MIN_RUN_PX {
            if emitted == budget {
                return emitted;
            }
            let take = dash_left.min(rest);
            let next = cursor + take;
            // Float stall guard: on a very long path `take` can fall below the ulp of `cursor`,
            // which would leave `cursor` unchanged and spin forever. Abandon the segment instead.
            if next <= cursor {
                break;
            }
            emit(DashRun {
                from: a + dir * cursor,
                to: a + dir * next,
                black,
            });
            emitted += 1;
            dash_left -= take;
            if dash_left <= MARQUEE_MIN_RUN_PX {
                dash_left = dash_len;
                black = !black;
            }
            rest -= take;
            cursor = next;
        }
        // Fold the sub-threshold tail into the phase so the pattern does not drift over a
        // staircase of thousands of tiny segments.
        if rest > 0.0 && rest <= MARQUEE_MIN_RUN_PX {
            dash_left -= rest;
            if dash_left <= MARQUEE_MIN_RUN_PX {
                dash_left = dash_len;
                black = !black;
            }
        }
    }
    emitted
}

#[cfg(test)]
mod tests {
    use super::*;
    use layers::LayerStack;
    use std::collections::BTreeSet;

    /// The canvas rect the overlay-gate tests position their pointer inside.
    const OVERLAY_RECT: Rect = Rect {
        min: Pos2::new(0.0, 0.0),
        max: Pos2::new(100.0, 100.0),
    };

    /// The plain case: bare canvas, no gesture — the preview follows the cursor.
    #[test]
    fn the_overlay_follows_the_pointer_on_bare_canvas() {
        let p = Pos2::new(10.0, 10.0);
        assert_eq!(
            overlay_pointer(Some(p), OVERLAY_RECT, false, false),
            Some(p)
        );
    }

    /// The reported bug: hovering a floating dock panel with no gesture running must HIDE the tool
    /// preview, not merely stop it from acting. `rect.contains` alone cannot do this — the canvas
    /// is the full-area background, so a panel's pixels are inside the canvas rect.
    #[test]
    fn the_overlay_is_dropped_over_a_panel_when_no_gesture_runs() {
        let p = Pos2::new(10.0, 10.0);
        assert_eq!(
            overlay_pointer(Some(p), OVERLAY_RECT, true, false),
            None,
            "a preview must not be painted under a floating panel"
        );
    }

    /// The other half of the rule, and the reason the in-flight term is asked of the TOOL: a
    /// gesture that legitimately started on bare canvas keeps its preview while it is dragged over
    /// a panel, exactly as a pan started on bare canvas survives the crossing.
    #[test]
    fn a_gesture_in_flight_keeps_its_overlay_over_a_panel() {
        let p = Pos2::new(10.0, 10.0);
        assert_eq!(
            overlay_pointer(Some(p), OVERLAY_RECT, true, true),
            Some(p),
            "a preview dragged over a panel by a running gesture must stay visible"
        );
    }

    /// The occlusion gate is added to the rect filter, never substituted for it: a pointer outside
    /// the program-tab area has nothing to preview, in flight or not.
    #[test]
    fn the_overlay_still_requires_the_pointer_inside_the_canvas_rect() {
        let outside = Pos2::new(200.0, 10.0);
        assert_eq!(
            overlay_pointer(Some(outside), OVERLAY_RECT, false, false),
            None
        );
        assert_eq!(
            overlay_pointer(Some(outside), OVERLAY_RECT, false, true),
            None
        );
        assert_eq!(overlay_pointer(None, OVERLAY_RECT, false, true), None);
    }

    /// The default arrangement is what the dock is handed on a first run AND the dictionary
    /// `panel_dock::persist` resolves stored tab keys against, so it must be well-formed and must
    /// name every tab this program tab can declare — a tab missing here would be dropped from the
    /// user's stored arrangement on every load.
    #[test]
    fn the_default_dock_layout_places_the_five_ps_editor_panels() {
        let layout = ps_editor_default_dock_layout();
        assert_eq!(layout.validate(), Ok(()));
        assert_eq!(layout.panels().len(), 5);

        let declared: BTreeSet<TabId> = layout
            .panels()
            .iter()
            .flat_map(|panel| panel.tabs.iter().copied())
            .collect();
        let expected: BTreeSet<TabId> = [
            PS_EDITOR_MAIN_TAB,
            PS_EDITOR_TOOLS_TAB,
            PS_EDITOR_ACTIVE_TOOL_TAB,
            PS_EDITOR_HOTKEYS_TAB,
            PS_EDITOR_LAYERS_TAB,
        ]
        .into_iter()
        .collect();
        assert_eq!(
            declared, expected,
            "the default layout must name exactly the five tabs the program tab declares"
        );
    }

    /// The two columns the doc comment promises: «PS редактор» roots the left chain at the viewport's
    /// left edge, «Слои» roots the right one at its right edge, and each dependant hangs below its
    /// own target. Anchors, not rects: the solver is what turns these into geometry.
    #[test]
    fn the_default_dock_layout_builds_two_columns() {
        let layout = ps_editor_default_dock_layout();
        let panel_of = |tab: TabId| {
            let id = layout
                .panel_of_tab(tab)
                .unwrap_or_else(|| panic!("{tab} has a panel"));
            layout
                .panel(id)
                .unwrap_or_else(|| panic!("{tab}'s panel exists"))
        };
        let main = panel_of(PS_EDITOR_MAIN_TAB);
        let layers = panel_of(PS_EDITOR_LAYERS_TAB);
        assert_eq!(
            main.anchor,
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Left,
                along: 0.0
            }
        );
        assert_eq!(
            layers.anchor,
            PanelAnchor::ViewportEdge {
                edge: DockEdge::Right,
                along: 0.0
            }
        );
        assert_eq!(
            panel_of(PS_EDITOR_TOOLS_TAB).anchor,
            PanelAnchor::Panel {
                target: main.id,
                edge: DockEdge::Bottom,
                align: 0.0
            }
        );
        assert_eq!(
            panel_of(PS_EDITOR_ACTIVE_TOOL_TAB).anchor,
            PanelAnchor::Panel {
                target: panel_of(PS_EDITOR_TOOLS_TAB).id,
                edge: DockEdge::Bottom,
                align: 0.0
            }
        );
        assert_eq!(
            panel_of(PS_EDITOR_HOTKEYS_TAB).anchor,
            PanelAnchor::Panel {
                target: layers.id,
                edge: DockEdge::Bottom,
                align: 0.0
            }
        );
    }

    /// The tab-level shortcut list is rendered as a two-column grid, so a blank half is an
    /// authoring bug (a forgotten key) or a translation bug (an empty catalog value) and must not
    /// reach the UI.
    ///
    /// The reference catalog is installed first on purpose: `t!` falls back to the KEY text on a
    /// miss, which is never empty, so without a real catalog an empty translation would slip past.
    #[test]
    fn the_common_hotkey_rows_are_well_formed() {
        let _guard = crate::locale_store::GLOBAL_LOCALE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
        ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");

        let rows = ps_editor_common_hotkey_rows();
        assert!(
            !rows.is_empty(),
            "the tab has shortcuts, so the list is never empty"
        );
        for row in &rows {
            assert!(
                !row.action.trim().is_empty(),
                "a common hotkey row has an empty action (keys {:?})",
                row.keys
            );
            assert!(
                !row.keys.trim().is_empty(),
                "common hotkey row {:?} has empty keys",
                row.action
            );
        }
    }

    /// One visibility flag key together with the accessor of the field it must be read into.
    type VisibilityKeyProbe = (&'static str, fn(&PsEditorPanelVisibility) -> bool);

    /// One `entries()` row under test: its flag key, its menu caption, and the mutation that hides
    /// that panel and no other.
    type VisibilityRowProbe = (&'static str, &'static str, fn(&mut PsEditorPanelVisibility));

    /// The four visibility flags default to "shown" when the dock has stored nothing, and each one
    /// is read from its OWN key — a copy-paste slip between the four keys would otherwise show up
    /// only as two panels that hide together.
    ///
    /// The stored case has to go through a real `TabExtras` bag seeded with
    /// [`PanelDockState::put_tab_extras`]: on a bare `PanelDockState` the MAIN tab has no bag at
    /// all, so `read` leaves through its `let Some(..) else` branch and the four `extras.flag(..)`
    /// lines — the thing being tested — never run. One key is turned off at a time, because with
    /// all four flags equal any permutation of the keys looks identical.
    #[test]
    fn panel_visibility_defaults_to_all_shown_and_reads_each_key() {
        let layout_key = AppTab::PsEditor.key();

        // Nothing stored: no bag for the MAIN tab, so every panel falls back to shown.
        let state = PanelDockState::new();
        let visibility = PsEditorPanelVisibility::read(&state, layout_key);
        assert!(visibility.tools);
        assert!(visibility.active_tool);
        assert!(visibility.hotkeys);
        assert!(visibility.layers);

        // Each flag key, with the field it must land in.
        let cases: [VisibilityKeyProbe; 4] = [
            (PS_EDITOR_FLAG_TOOLS_PANEL, |v| v.tools),
            (PS_EDITOR_FLAG_ACTIVE_TOOL_PANEL, |v| v.active_tool),
            (PS_EDITOR_FLAG_HOTKEYS_PANEL, |v| v.hotkeys),
            (PS_EDITOR_FLAG_LAYERS_PANEL, |v| v.layers),
        ];
        for (key, field) in cases {
            let mut state = PanelDockState::new();
            let mut extras = TabExtras::default();
            extras.set_flag(
                key,
                !PS_EDITOR_PANEL_VISIBLE_DEFAULT,
                PS_EDITOR_PANEL_VISIBLE_DEFAULT,
            );
            state.put_tab_extras(layout_key, PS_EDITOR_MAIN_TAB, extras);

            let visibility = PsEditorPanelVisibility::read(&state, layout_key);
            assert!(
                !field(&visibility),
                "`{key}` must be read by its own field, not by another panel's"
            );
            let hidden = cases
                .iter()
                .filter(|(_, other)| !other(&visibility))
                .count();
            assert_eq!(hidden, 1, "`{key}` must hide exactly one panel — its own");
        }
    }

    /// `entries()` is the single owner of the (flag key, menu caption, field) triple: the «Панели…»
    /// checkbox, the persisted flag and the panel that actually hides all come from ONE row. A
    /// copy-paste swap between two rows would toggle the wrong panel and persist it under the wrong
    /// key, so each field is probed on its own — with all four `true` any permutation looks alike.
    ///
    /// The `TabId` leg of the pairing is deliberately absent here: `entries()` carries no `TabId`.
    /// It lives on the `dock.tab(..).title(..).visible(..)` chains in `draw`, where the caption and
    /// the visibility field sit on the same builder — which is exactly why the caption is asserted
    /// here: a row whose caption belongs to another panel is the same defect seen from the menu.
    ///
    /// The reference catalog is installed first on purpose: `t!` falls back to the KEY text on a
    /// miss, so without a real catalog every caption would compare equal to its own key text.
    #[test]
    fn each_entries_row_pairs_its_own_key_caption_and_field() {
        let _guard = crate::locale_store::GLOBAL_LOCALE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tag = ms_i18n::LocaleTag::parse("en").expect("the `en` tag parses");
        ms_i18n::set_locale(&tag).expect("the embedded English catalog installs");

        let all_shown = PsEditorPanelVisibility {
            tools: true,
            active_tool: true,
            hotkeys: true,
            layers: true,
        };
        let cases: [VisibilityRowProbe; 4] = [
            (PS_EDITOR_FLAG_TOOLS_PANEL, t!("ps_editor.tab.tools"), |v| {
                v.tools = false;
            }),
            (
                PS_EDITOR_FLAG_ACTIVE_TOOL_PANEL,
                t!("ps_editor.tab.active_tool"),
                |v| v.active_tool = false,
            ),
            (
                PS_EDITOR_FLAG_HOTKEYS_PANEL,
                t!("ps_editor.tab.hotkeys"),
                |v| v.hotkeys = false,
            ),
            (
                PS_EDITOR_FLAG_LAYERS_PANEL,
                t!("ps_editor.tab.layers"),
                |v| {
                    v.layers = false;
                },
            ),
        ];
        for (key, caption, hide) in cases {
            let mut visibility = all_shown;
            hide(&mut visibility);
            let hidden: Vec<(&'static str, &'static str)> = visibility
                .entries()
                .into_iter()
                .filter(|(_, _, value)| !**value)
                .map(|(row_key, row_caption, _)| (row_key, row_caption))
                .collect();
            assert_eq!(
                hidden,
                vec![(key, caption)],
                "hiding the panel of `{key}` must report exactly that key and its own caption"
            );
        }

        let mut visibility = all_shown;
        let keys: Vec<&'static str> = visibility
            .entries()
            .into_iter()
            .map(|(key, _, _)| key)
            .collect();
        assert_eq!(
            keys,
            vec![
                PS_EDITOR_FLAG_TOOLS_PANEL,
                PS_EDITOR_FLAG_ACTIVE_TOOL_PANEL,
                PS_EDITOR_FLAG_HOTKEYS_PANEL,
                PS_EDITOR_FLAG_LAYERS_PANEL,
            ],
            "the menu rows and the persisted keys are one list, in one order"
        );
    }

    fn filled(size: [usize; 2], color: Color32) -> ColorImage {
        ColorImage::filled(size, color)
    }

    /// Selection covering the inner pixels (1,1)..=(2,2) of a 4x4 page.
    fn inner_selection() -> Selection {
        let mut sel = Selection::empty(4, 4);
        sel.set_rect(1, 1, 3, 3);
        sel
    }

    /// Collects every run `walk_dash_runs` emits for `path` at `dash`, with an unrestrictive budget.
    fn collect_runs(path: &[Pos2], dash: f32) -> Vec<DashRun> {
        let mut runs = Vec::new();
        let emitted = walk_dash_runs(path, dash, 10_000, |r| runs.push(r));
        assert_eq!(
            emitted,
            runs.len(),
            "returned count must match the emitted runs"
        );
        runs
    }

    /// Asserts the runs tile `path` end to end without gaps or overlaps.
    fn assert_contiguous(runs: &[DashRun], start: Pos2, end: Pos2) {
        let (Some(first), Some(last)) = (runs.first(), runs.last()) else {
            panic!("expected at least one run");
        };
        assert!(
            (first.from - start).length() < 1e-3,
            "first run must start at the path start"
        );
        for pair in runs.windows(2) {
            assert!(
                (pair[0].to - pair[1].from).length() < 1e-3,
                "runs must be contiguous: {:?} -> {:?}",
                pair[0],
                pair[1]
            );
        }
        assert!(
            (last.to - end).length() < 1e-3,
            "last run must end at the path end"
        );
    }

    #[test]
    fn dash_phase_carries_across_short_staircase_segments() {
        // The case the arc-length rewrite exists for: a pixel-traced lasso outline is a staircase of
        // one-pixel steps, each far shorter than the dash period. Dashing every segment on its own
        // would restart the phase ten times and paint a solid line; here the phase accumulates, so
        // the colour flips only where 4px of ARC LENGTH have been walked.
        let path: Vec<Pos2> = (0u8..=10).map(|i| Pos2::new(f32::from(i), 0.0)).collect();
        let runs = collect_runs(&path, 4.0);
        // One run per segment (each segment is shorter than a dash, so none is subdivided).
        assert_eq!(
            runs.len(),
            10,
            "expected one run per unit segment, got {runs:?}"
        );
        assert_contiguous(&runs, Pos2::new(0.0, 0.0), Pos2::new(10.0, 0.0));
        // 4 black, 4 white, then the 2px tail of the next black dash.
        let expected = [
            true, true, true, true, false, false, false, false, true, true,
        ];
        let actual: Vec<bool> = runs.iter().map(|r| r.black).collect();
        assert_eq!(
            actual, expected,
            "dash colours must follow cumulative arc length"
        );
    }

    #[test]
    fn one_long_segment_holds_several_dashes() {
        // The mirror case: a single collinear-merged edge (a rectangular selection is ~5 points)
        // must be subdivided into as many dashes as it is long.
        let path = [Pos2::new(0.0, 0.0), Pos2::new(20.0, 0.0)];
        let runs = collect_runs(&path, 4.0);
        assert_eq!(runs.len(), 5, "20px / 4px dash = 5 runs, got {runs:?}");
        assert_contiguous(&runs, path[0], path[1]);
        for (i, run) in runs.iter().enumerate() {
            let len = (run.to - run.from).length();
            assert!(
                (len - 4.0).abs() < 1e-3,
                "run {i} should be one dash long, got {len}"
            );
            assert_eq!(run.black, i % 2 == 0, "colours must alternate, run {i}");
        }
    }

    #[test]
    fn path_shorter_than_one_dash_emits_a_single_black_run() {
        let path = [Pos2::new(0.0, 0.0), Pos2::new(1.0, 0.0)];
        let runs = collect_runs(&path, 4.0);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].black, "the pattern starts black");
        assert_contiguous(&runs, path[0], path[1]);
    }

    #[test]
    fn degenerate_dash_inputs_emit_nothing() {
        let path = [Pos2::new(0.0, 0.0), Pos2::new(20.0, 0.0)];
        // Fewer than two points, a non-positive/non-finite dash, or a zero budget: no runs, no panic.
        assert_eq!(walk_dash_runs(&path[..1], 4.0, 10, |_| {}), 0);
        assert_eq!(walk_dash_runs(&path, 0.0, 10, |_| {}), 0);
        assert_eq!(walk_dash_runs(&path, f32::NAN, 10, |_| {}), 0);
        assert_eq!(walk_dash_runs(&path, 4.0, 0, |_| {}), 0);
        // A zero-length path (all points coincide) produces nothing either.
        let collapsed = [
            Pos2::new(3.0, 3.0),
            Pos2::new(3.0, 3.0),
            Pos2::new(3.0, 3.0),
        ];
        assert_eq!(walk_dash_runs(&collapsed, 4.0, 10, |_| {}), 0);
    }

    #[test]
    fn dash_run_budget_is_respected() {
        // 100px at a 1px dash would be 100 runs; the budget cuts the walk short instead.
        let path = [Pos2::new(0.0, 0.0), Pos2::new(100.0, 0.0)];
        let mut count = 0usize;
        let emitted = walk_dash_runs(&path, 1.0, 7, |_| count += 1);
        assert_eq!(emitted, 7);
        assert_eq!(count, 7);
    }

    #[test]
    fn recenter_offset_zero_when_growth_is_symmetric() {
        // Base 10x10 grown to 14x14 with the content centered (origin = 2,2): the content center is
        // unchanged, so no recenter is needed regardless of transform.
        let t = LayerTransform {
            center: Vec2::new(5.0, 5.0),
            rotation: 0.0,
            scale: 1.0,
        };
        let off = effects_recenter_offset([14, 14], [2, 2], [10, 10], t);
        assert!(
            off.length() < 1e-4,
            "symmetric growth needs no recenter, got {off:?}"
        );
    }

    #[test]
    fn recenter_offset_anchors_asymmetric_growth() {
        // Base 10x10 grown to 16x10 with the content at origin (0,0): the new center sits 3px right of
        // the base center, so the layer center must shift +3px in x (identity transform).
        let t = LayerTransform {
            center: Vec2::ZERO,
            rotation: 0.0,
            scale: 1.0,
        };
        let off = effects_recenter_offset([16, 10], [0, 0], [10, 10], t);
        assert!((off.x - 3.0).abs() < 1e-4, "x offset wrong: {off:?}");
        assert!(off.y.abs() < 1e-4, "y offset wrong: {off:?}");
    }

    #[test]
    fn recenter_offset_applies_scale_then_rotation() {
        // Same +3px base-local x delta, but the base layer is scaled 2x and rotated 90°. Scale doubles
        // it to 6px, then a 90° rotation maps +x to +y (within float tolerance).
        let t = LayerTransform {
            center: Vec2::ZERO,
            rotation: std::f32::consts::FRAC_PI_2,
            scale: 2.0,
        };
        let off = effects_recenter_offset([16, 10], [0, 0], [10, 10], t);
        assert!(off.x.abs() < 1e-3, "x should be ~0 after 90° rot: {off:?}");
        assert!(
            (off.y - 6.0).abs() < 1e-3,
            "y should be ~6 (3*2 scaled, rotated): {off:?}"
        );
    }

    #[test]
    fn over_composites_premultiplied() {
        assert_eq!(over(Color32::RED, Color32::TRANSPARENT), Color32::RED);
        assert_eq!(over(Color32::TRANSPARENT, Color32::GREEN), Color32::GREEN);
        // Opaque src fully replaces dst.
        assert_eq!(over(Color32::BLUE, Color32::RED), Color32::BLUE);
    }

    #[test]
    fn composite_to_page_blends_bottom_then_top() {
        let size = [2, 2];
        let mut stack = LayerStack::new(
            0,
            size,
            filled(size, Color32::TRANSPARENT),
            filled(size, Color32::TRANSPARENT),
        );
        let bottom = stack.add_raster_layer();
        stack.layer_mut(bottom).unwrap().image = filled(size, Color32::BLUE);
        let top = stack.add_raster_layer();
        {
            let img = &mut stack.layer_mut(top).unwrap().image;
            *img = filled(size, Color32::TRANSPARENT);
            img.pixels[0] = Color32::RED; // opaque only at (0,0)
        }
        let out = composite_to_page(
            &[stack.layer(bottom).unwrap(), stack.layer(top).unwrap()],
            size,
        );
        assert_eq!(
            out.pixels[0],
            Color32::RED,
            "top opaque pixel wins over bottom"
        );
        assert_eq!(
            out.pixels[1],
            Color32::BLUE,
            "bottom shows where top is transparent"
        );
    }

    #[test]
    fn copy_builds_layer_from_selection_without_touching_sources() {
        let size = [4, 4];
        let mut stack = LayerStack::new(
            0,
            size,
            filled(size, Color32::RED),
            filled(size, Color32::TRANSPARENT),
        );
        let sel = inner_selection();
        let source_id = stack.layers()[0].id;

        clip_into_new_layer(&mut stack, &sel, ClipMode::Copy, &[source_id]);

        // A new raster layer is on top, cropped to the 2x2 selection bounds and centered on it.
        assert_eq!(stack.layers().len(), 3);
        let top = stack.layers().last().unwrap();
        assert_eq!(top.image.size, [2, 2]);
        assert!(top.image.pixels.iter().all(|&p| p == Color32::RED));
        assert_eq!(top.transform.center, egui::Vec2::new(2.0, 2.0));
        // The source is untouched (still page-sized red).
        assert_eq!(stack.layers()[0].image.size, [4, 4]);
        assert_eq!(stack.layers()[0].image.pixels[4 + 1], Color32::RED);
    }

    #[test]
    fn cut_clears_mutable_layers_but_never_the_source() {
        let size = [4, 4];
        let mut stack = LayerStack::new(
            0,
            size,
            filled(size, Color32::RED),
            filled(size, Color32::GREEN),
        );
        let source_id = stack.layers()[0].id;
        let clean_id = stack.layers()[1].id;
        let sel = inner_selection();

        clip_into_new_layer(&mut stack, &sel, ClipMode::Cut, &[source_id, clean_id]);

        // Clean (mutable) is cleared inside the selection; source (immutable) is not.
        assert_eq!(
            stack.layer(clean_id).unwrap().image.pixels[4 + 1],
            Color32::TRANSPARENT
        );
        assert_eq!(
            stack.layer(source_id).unwrap().image.pixels[4 + 1],
            Color32::RED
        );
        // The new layer (2x2 crop) is the composite (green over red opaque = green).
        let top = stack.layers().last().unwrap();
        assert_eq!(top.image.size, [2, 2]);
        assert!(top.image.pixels.iter().all(|&p| p == Color32::GREEN));
        // Untouched pixels outside the selection stay put.
        assert_eq!(
            stack.layer(clean_id).unwrap().image.pixels[0],
            Color32::GREEN
        );
    }

    #[test]
    fn touched_detection_skips_transparent_layers() {
        let size = [4, 4];
        let stack = LayerStack::new(
            0,
            size,
            filled(size, Color32::RED),
            filled(size, Color32::TRANSPARENT),
        );
        let sel = inner_selection();
        let bounds = sel.bounds().unwrap();
        assert!(layer_touches_selection(&stack.layers()[0], &sel, bounds));
        assert!(!layer_touches_selection(&stack.layers()[1], &sel, bounds));
    }

    #[test]
    fn moved_layer_samples_from_its_transformed_position() {
        // A 2x2 opaque blue layer placed so its center sits at page (1,1) covers pixels (0,0)..(1,1).
        let size = [4, 4];
        let mut stack = LayerStack::new(
            0,
            size,
            filled(size, Color32::TRANSPARENT),
            filled(size, Color32::TRANSPARENT),
        );
        let transform = layers::LayerTransform {
            center: egui::Vec2::new(1.0, 1.0),
            rotation: 0.0,
            scale: 1.0,
        };
        let id =
            stack.add_raster_layer_image("blue".into(), filled([2, 2], Color32::BLUE), transform);

        // Page pixel (0,0) maps into the layer; (3,3) does not.
        assert_eq!(
            sample_layer_world(stack.layer(id).unwrap(), 0, 0),
            Color32::BLUE
        );
        assert_eq!(
            sample_layer_world(stack.layer(id).unwrap(), 3, 3),
            Color32::TRANSPARENT
        );
    }

    #[test]
    fn raster_below_by_band_z_picks_visually_below_not_stack_neighbor() {
        // Stack (insertion) order is r_a, r_b, r_c, but the user reordered them so the band-Z order is
        // r_b (z=0, bottom), r_c (z=1), r_a (z=2, top). The list is in STACK order with each raster's
        // BAND z — the helper must use band-Z, not list position.
        let rasters = vec![
            ("r_a".to_string(), 2u32),
            ("r_b".to_string(), 0u32),
            ("r_c".to_string(), 1u32),
        ];

        // r_a is the top band (z=2): directly below it is r_c (z=1), NOT its stack neighbor r_b.
        assert_eq!(
            raster_below_by_band_z(&rasters, "r_a").as_deref(),
            Some("r_c")
        );
        // r_c (z=1): below is r_b (z=0).
        assert_eq!(
            raster_below_by_band_z(&rasters, "r_c").as_deref(),
            Some("r_b")
        );
        // r_b is the bottom band (z=0): nothing below.
        assert_eq!(raster_below_by_band_z(&rasters, "r_b"), None);
        // Unknown uid → None.
        assert_eq!(raster_below_by_band_z(&rasters, "nope"), None);
    }

    #[test]
    fn merge_selection_uses_band_z_after_reorder_and_protects_base_layers() {
        // Integration: a stack with two rasters whose BAND-Z order is the REVERSE of their stack
        // insertion order. `raster_below_uid` / `is_mergeable` must follow band-Z (the visually-below
        // raster). `Клин` is the target below the bottom-most raster; `Исходник` never is, and
        // neither base layer is ever the UPPER participant.
        let size = [2, 2];
        let mut stack = LayerStack::new(
            0,
            size,
            filled(size, Color32::TRANSPARENT),
            filled(size, Color32::TRANSPARENT),
        );
        // Inserted first (lower stack index) but placed ON TOP by band-Z below.
        let first = stack.add_raster_layer();
        let second = stack.add_raster_layer();
        let first_uid = stack.layer(first).unwrap().uid.to_string();
        let second_uid = stack.layer(second).unwrap().uid.to_string();

        // Band-Z: `second` is the BOTTOM band (z=0), `first` is the TOP band (z=1) — reverse of stack.
        let ps = PsEditorTabState {
            bands: vec![
                Band::Raster {
                    uid: second_uid.clone(),
                    z: 0,
                },
                Band::Raster {
                    uid: first_uid.clone(),
                    z: 1,
                },
            ],
            stack: Some(stack),
            ..Default::default()
        };

        // `first` is visually on top (band z=1): directly below it is `second` (band z=0), NOT a base
        // layer and NOT its stack neighbour.
        assert_eq!(
            ps.raster_below_uid(first).as_deref(),
            Some(second_uid.as_str())
        );
        assert!(ps.is_mergeable(first), "top-by-band raster is mergeable");

        // `second` is the bottom-most raster by band-Z: below it is the `Клин` base layer, so it IS
        // mergeable — merging flattens it into the clean overlay.
        let stack_ref = ps.stack.as_ref().expect("stack resident");
        let clean_uid = stack_ref
            .layers()
            .iter()
            .find(|l| l.kind == LayerKind::Clean)
            .map(|l| l.uid.to_string())
            .expect("clean base layer present");
        let source_uid = stack_ref
            .layers()
            .iter()
            .find(|l| l.kind == LayerKind::Source)
            .map(|l| l.uid.to_string())
            .expect("source base layer present");
        assert_eq!(
            ps.raster_below_uid(second).as_deref(),
            Some(clean_uid.as_str()),
            "the bottom-most raster merges into Клин"
        );
        assert!(
            ps.is_mergeable(second),
            "bottom-by-band raster is mergeable into Клин"
        );

        // `Исходник` is never a merge candidate at all.
        let candidates = ps.merge_candidates_by_band_z();
        assert!(
            !candidates.iter().any(|(uid, _)| *uid == source_uid),
            "Исходник must never be a merge target"
        );
        let clean_z = candidates
            .iter()
            .find(|(uid, _)| *uid == clean_uid)
            .map(|(_, z)| *z)
            .expect("Клин is a merge candidate");
        assert!(
            candidates
                .iter()
                .filter(|(uid, _)| *uid != clean_uid)
                .all(|(_, z)| *z > clean_z),
            "Клин sits strictly below every raster on the band-Z axis"
        );

        // Neither base layer is ever the UPPER participant of a merge (they have no control strip,
        // and the query refuses them by construction).
        let base_ids: Vec<LayerId> = ps
            .stack
            .as_ref()
            .unwrap()
            .layers()
            .iter()
            .filter(|l| l.kind.is_base())
            .map(|l| l.id)
            .collect();
        assert_eq!(base_ids.len(), 2, "source + clean base layers present");
        for id in base_ids {
            assert!(!ps.is_mergeable(id), "a base layer is never merged away");
            assert_eq!(
                ps.raster_below_uid(id),
                None,
                "a base layer is never the upper participant"
            );
        }
    }

    #[test]
    fn merge_candidates_keep_raster_order_and_reserve_the_bottom_slot_for_clean() {
        // The +1 shift that makes room for Клин at Z 0 must not disturb the rasters' relative order.
        let size = [2, 2];
        let mut stack = LayerStack::new(
            0,
            size,
            filled(size, Color32::TRANSPARENT),
            filled(size, Color32::TRANSPARENT),
        );
        let a = stack.add_raster_layer();
        let b = stack.add_raster_layer();
        let a_uid = stack.layer(a).expect("resident").uid.to_string();
        let b_uid = stack.layer(b).expect("resident").uid.to_string();
        let ps = PsEditorTabState {
            bands: vec![
                Band::Raster {
                    uid: a_uid.clone(),
                    z: 0,
                },
                Band::Raster {
                    uid: b_uid.clone(),
                    z: 1,
                },
            ],
            stack: Some(stack),
            ..Default::default()
        };
        let candidates = ps.merge_candidates_by_band_z();
        let z_of = |uid: &str| {
            candidates
                .iter()
                .find(|(u, _)| u == uid)
                .map(|(_, z)| *z)
                .expect("candidate present")
        };
        assert_eq!(z_of(&a_uid), 1, "band z 0 shifts to 1");
        assert_eq!(z_of(&b_uid), 2, "band z 1 shifts to 2");
        // `a` (the lower raster) is what `b` merges into; `a` in turn merges into Клин.
        assert_eq!(ps.raster_below_uid(b).as_deref(), Some(a_uid.as_str()));
        assert!(ps.raster_below_uid(a).is_some(), "Клин is below the bottom raster");
    }

    #[test]
    fn raster_below_by_band_z_breaks_z_ties_toward_lower_stack_index() {
        // Two rasters share a band-Z (e.g. both fell back to the past-the-top fallback). The one
        // earlier in the list (lower stack index) is treated as below, matching `draw_composite`'s
        // stable raster tiebreak.
        let rasters = vec![
            ("low".to_string(), 5u32),  // earlier in the list
            ("high".to_string(), 5u32), // later in the list, same z
            ("top".to_string(), 9u32),
        ];
        // `top` (z=9) is above both; the nearest below is the tied pair — the earlier list entry wins.
        assert_eq!(
            raster_below_by_band_z(&rasters, "top").as_deref(),
            Some("low")
        );
        // Equal-Z peers are NOT below each other (strict `z <` only).
        assert_eq!(raster_below_by_band_z(&rasters, "low"), None);
        assert_eq!(raster_below_by_band_z(&rasters, "high"), None);
    }

    #[test]
    fn an_empty_footprint_never_becomes_a_phantom_selection() {
        // "Select layer fully" on a layer whose footprint misses the page entirely: the mask stays
        // all-zero, and storing it as `Some` would block the brush behind an invisible marquee.
        let mut off_page = Selection::empty(4, 4);
        off_page.set_polygon(&[
            (-30.0, -30.0),
            (-10.0, -30.0),
            (-10.0, -10.0),
            (-30.0, -10.0),
        ]);
        assert!(!off_page.any(), "the fixture must select nothing");
        assert!(non_empty_selection(off_page).is_none());
    }

    #[test]
    fn a_real_footprint_is_stored_as_is() {
        let sel = inner_selection();
        let stored = non_empty_selection(sel).expect("a non-empty mask must be kept");
        assert!(stored.contains(1, 1));
        assert!(!stored.contains(0, 0));
    }

    #[test]
    fn dash_walker_skips_non_finite_segments() {
        // A broken `ViewTransform` can hand the walker an infinite point. It must not emit a single
        // run for that segment (an infinite length would otherwise exhaust the whole budget).
        let path = [
            Pos2::new(0.0, 0.0),
            Pos2::new(f32::INFINITY, 0.0),
            Pos2::new(10.0, 0.0),
        ];
        let mut runs = Vec::new();
        let emitted = walk_dash_runs(&path, 4.0, 10_000, |r| runs.push(r));
        assert_eq!(
            emitted, 0,
            "no run may be emitted from or into a non-finite point"
        );
        assert!(runs.is_empty());

        let nan_path = [Pos2::new(0.0, 0.0), Pos2::new(f32::NAN, f32::NAN)];
        assert_eq!(walk_dash_runs(&nan_path, 4.0, 10_000, |_| ()), 0);

        // A finite path around the bad segment still dashes normally.
        let good = [Pos2::new(0.0, 0.0), Pos2::new(12.0, 0.0)];
        assert_eq!(walk_dash_runs(&good, 4.0, 10_000, |_| ()), 3);
    }

    // ---------------------------------------------------------------------------------------
    // `write_clean_region_to_model` — the ONE `Клин` write-back path (see its doc comment).
    // ---------------------------------------------------------------------------------------

    /// A 1-page clean-overlay model whose page 0 overlay is materialized at `size`.
    ///
    /// `replace` adopts the size of the image it is given on a model that has none yet, which is
    /// how these tests give the model a size that either matches or deliberately mismatches the
    /// editor's page.
    fn overlays_model_with_overlay(size: [usize; 2]) -> Arc<Mutex<CleanOverlaysModel>> {
        let mut model = CleanOverlaysModel::new_from_pages(&[std::path::PathBuf::from("p0.png")]);
        model.replace(0, &filled(size, Color32::TRANSPARENT));
        Arc::new(Mutex::new(model))
    }

    /// A tab state on page 0 with a `size` page and the given model bound, its `Клин` layer painted
    /// `paint` over the whole page while `base_image` still holds transparent (the state a live
    /// stroke leaves behind: `image` mutated, `base_image` pre-stroke, `pixels_dirty` set).
    fn ps_with_painted_clean(
        size: [usize; 2],
        paint: Color32,
        model: Arc<Mutex<CleanOverlaysModel>>,
    ) -> (PsEditorTabState, LayerId) {
        let mut stack = LayerStack::new(
            0,
            size,
            filled(size, Color32::TRANSPARENT),
            filled(size, Color32::TRANSPARENT),
        );
        let clean_id = stack
            .layers()
            .iter()
            .find(|l| l.kind == LayerKind::Clean)
            .map(|l| l.id)
            .expect("clean base layer present");
        let clean = stack.layer_mut(clean_id).expect("clean layer resident");
        clean.image = filled(size, paint);
        clean.pixels_dirty = true;
        let ps = PsEditorTabState {
            stack: Some(stack),
            overlays_model: Some(model),
            active_page_idx: Some(0),
            ..Default::default()
        };
        (ps, clean_id)
    }

    /// The straight-RGBA pixel the model holds for page 0 at `(x, y)`.
    fn model_pixel(model: &Arc<Mutex<CleanOverlaysModel>>, x: u32, y: u32) -> [u8; 4] {
        let locked = model.lock().expect("model lock");
        let rgba = locked.overlay_rgba(0).expect("overlay materialized");
        rgba.get_pixel(x, y).0
    }

    /// The happy path: the requested rect (and ONLY it) reaches the model, `base_image` is
    /// re-established over that rect so the next undo reads the committed pixels, and
    /// `pixels_dirty` is cleared because the shared model owns Clean persistence.
    #[test]
    fn clean_write_back_pushes_only_the_requested_rect_and_commits_local_state() {
        let size = [4, 4];
        let model = overlays_model_with_overlay(size);
        let (mut ps, clean_id) = ps_with_painted_clean(size, Color32::RED, Arc::clone(&model));

        assert!(ps.write_clean_region_to_model(0, 1, 1, 2, 2));

        // Inside the rect the model took the painted pixel; outside it is untouched.
        assert_eq!(model_pixel(&model, 1, 1), [255, 0, 0, 255]);
        assert_eq!(model_pixel(&model, 2, 2), [255, 0, 0, 255]);
        assert_eq!(model_pixel(&model, 0, 0), [0, 0, 0, 0]);
        assert_eq!(model_pixel(&model, 3, 3), [0, 0, 0, 0]);

        let clean = ps
            .stack
            .as_ref()
            .and_then(|s| s.layer(clean_id))
            .expect("clean layer resident");
        assert_eq!(
            clean.base_image.pixels[5], // (x=1, y=1) on a 4-wide page
            Color32::RED,
            "base_image must mirror image inside the committed rect"
        );
        assert_eq!(
            clean.base_image.pixels[0],
            Color32::TRANSPARENT,
            "base_image outside the rect is not touched"
        );
        assert!(!clean.pixels_dirty);
    }

    /// The rect is clamped into the page, and a rect entirely outside it is a no-op that commits
    /// nothing — neither to the model nor to the layer's bookkeeping.
    #[test]
    fn clean_write_back_clamps_the_rect_and_refuses_one_outside_the_page() {
        let size = [4, 4];
        let model = overlays_model_with_overlay(size);
        let (mut ps, clean_id) = ps_with_painted_clean(size, Color32::RED, Arc::clone(&model));

        // Overhanging rect: clamped to (3,3)-(4,4), so exactly the corner pixel is written.
        assert!(ps.write_clean_region_to_model(0, 3, 3, 100, 100));
        assert_eq!(model_pixel(&model, 3, 3), [255, 0, 0, 255]);
        assert_eq!(model_pixel(&model, 2, 3), [0, 0, 0, 0]);

        // Wholly outside: nothing written, nothing committed.
        let (mut ps_out, clean_out) =
            ps_with_painted_clean(size, Color32::GREEN, overlays_model_with_overlay(size));
        assert!(!ps_out.write_clean_region_to_model(0, 4, 4, 2, 2));
        let clean = ps_out
            .stack
            .as_ref()
            .and_then(|s| s.layer(clean_out))
            .expect("clean layer resident");
        assert!(
            clean.pixels_dirty,
            "a refused write must not clear the dirty flag"
        );

        // The first tab's clamped write left its own layer committed.
        assert!(
            !ps.stack
                .as_ref()
                .and_then(|s| s.layer(clean_id))
                .expect("clean layer resident")
                .pixels_dirty
        );
    }

    /// A write for a page the resident stack is not on is refused outright — the crop would come
    /// from the wrong page's pixels.
    #[test]
    fn clean_write_back_refuses_a_foreign_page_index() {
        let size = [4, 4];
        let model = overlays_model_with_overlay(size);
        let (mut ps, _) = ps_with_painted_clean(size, Color32::RED, Arc::clone(&model));
        assert!(!ps.write_clean_region_to_model(1, 0, 0, 4, 4));
        assert_eq!(model_pixel(&model, 0, 0), [0, 0, 0, 0]);
    }

    /// Revision adoption, half one: with no foreign edit in between, the tab adopts the revision its
    /// own write produced, so `sync_view_from_canvas` does not reload the whole page after a stroke.
    #[test]
    fn clean_write_back_adopts_its_own_revision_when_nothing_foreign_happened() {
        let size = [4, 4];
        let model = overlays_model_with_overlay(size);
        let (mut ps, _) = ps_with_painted_clean(size, Color32::RED, Arc::clone(&model));
        // In sync with the model, as the last page load left us.
        let before = model.lock().expect("model lock").revision();
        ps.last_overlay_revision = before;

        assert!(ps.write_clean_region_to_model(0, 0, 0, 4, 4));

        let after = model.lock().expect("model lock").revision();
        assert!(after > before, "replace_region must bump the revision");
        assert_eq!(
            ps.last_overlay_revision, after,
            "our own bump must be adopted, or every stroke triggers a full page reload"
        );
    }

    /// Revision adoption, half two — the bug this guards: a background writer (the page manager's
    /// clean-attach worker) bumped the revision while the tab was elsewhere. Adopting now would make
    /// the tab equal to the model forever and the foreign edit would NEVER be loaded. The tab must
    /// keep its stale revision so the next `sync_view_from_canvas` reloads and reconciles; its own
    /// write is not lost by that, the model holds it.
    #[test]
    fn clean_write_back_keeps_a_stale_revision_when_a_foreign_edit_intervened() {
        let size = [4, 4];
        let model = overlays_model_with_overlay(size);
        let (mut ps, _) = ps_with_painted_clean(size, Color32::RED, Arc::clone(&model));
        // Last sync saw this revision...
        let synced = model.lock().expect("model lock").revision();
        ps.last_overlay_revision = synced;
        // ...then somebody else wrote to the model while the tab was not looking.
        model
            .lock()
            .expect("model lock")
            .replace(0, &filled(size, Color32::BLUE));
        let foreign = model.lock().expect("model lock").revision();
        assert!(foreign > synced);

        assert!(ps.write_clean_region_to_model(0, 0, 0, 4, 4));

        assert_eq!(
            ps.last_overlay_revision, synced,
            "a foreign bump must stay visible as a difference, so the page still reloads"
        );
        assert_ne!(
            ps.last_overlay_revision,
            model.lock().expect("model lock").revision()
        );
        // The write itself still reached the model — the reload restores it from there.
        assert_eq!(model_pixel(&model, 0, 0), [255, 0, 0, 255]);
    }

    /// A legacy overlay whose size disagrees with the editor's page must be REFUSED, not rescaled:
    /// `replace_region` clamps to the model's size and scales the chunk into it, which would land a
    /// brush stroke misaligned. `page_loader` answers the same mismatch with a transparent
    /// page-sized `Клин`, so this state is reachable.
    #[test]
    fn clean_write_back_refuses_a_size_mismatched_overlay_instead_of_rescaling() {
        // The model remembers an 8x8 overlay; the editor page is 4x4.
        let model = overlays_model_with_overlay([8, 8]);
        let (mut ps, clean_id) =
            ps_with_painted_clean([4, 4], Color32::RED, Arc::clone(&model));
        ps.last_overlay_revision = model.lock().expect("model lock").revision();

        assert!(!ps.write_clean_region_to_model(0, 0, 0, 4, 4));

        // Nothing scaled into the model, and no local state was committed.
        assert_eq!(model_pixel(&model, 0, 0), [0, 0, 0, 0]);
        assert_eq!(model_pixel(&model, 7, 7), [0, 0, 0, 0]);
        assert!(
            ps.stack
                .as_ref()
                .and_then(|s| s.layer(clean_id))
                .expect("clean layer resident")
                .pixels_dirty,
            "a refused write leaves the layer uncommitted"
        );
    }

    /// Without a bound model there is nowhere to push to: the write fails and, crucially, leaves the
    /// local bookkeeping uncommitted so the edit is not silently declared durable.
    #[test]
    fn clean_write_back_without_a_model_commits_nothing() {
        let size = [4, 4];
        let mut stack = LayerStack::new(
            0,
            size,
            filled(size, Color32::TRANSPARENT),
            filled(size, Color32::TRANSPARENT),
        );
        let clean_id = stack
            .layers()
            .iter()
            .find(|l| l.kind == LayerKind::Clean)
            .map(|l| l.id)
            .expect("clean base layer present");
        stack
            .layer_mut(clean_id)
            .expect("clean layer resident")
            .pixels_dirty = true;
        let mut ps = PsEditorTabState {
            stack: Some(stack),
            active_page_idx: Some(0),
            ..Default::default()
        };

        assert!(!ps.write_clean_region_to_model(0, 0, 0, 4, 4));
        assert!(
            ps.stack
                .as_ref()
                .and_then(|s| s.layer(clean_id))
                .expect("clean layer resident")
                .pixels_dirty
        );
    }

    // ---------------------------------------------------------------------------------------
    // `page_footprint_rect` — the bound a merge-into-`Клин` write-back uses.
    // ---------------------------------------------------------------------------------------

    /// The contract that makes the bound safe: outside the upper layer's footprint the composite
    /// reproduces the lower layer's pixels EXACTLY, so a write-back limited to that rect loses
    /// nothing. Asserted against `composite_to_page` itself, not against the rect math alone.
    #[test]
    fn a_merge_changes_no_pixel_outside_the_upper_layers_footprint() {
        let size = [8, 8];
        let mut stack = LayerStack::new(
            0,
            size,
            filled(size, Color32::TRANSPARENT),
            filled(size, Color32::from_rgb(10, 20, 30)),
        );
        let upper_id = stack.add_raster_layer();
        {
            let upper = stack.layer_mut(upper_id).expect("raster resident");
            upper.image = filled([2, 2], Color32::RED);
            upper.base_image = upper.image.clone();
            // Placed over page pixels (1,1)..=(2,2).
            upper.transform = LayerTransform {
                center: Vec2::new(2.0, 2.0),
                rotation: 0.0,
                scale: 1.0,
            };
        }
        let layers = stack.layers();
        let below = layers
            .iter()
            .find(|l| l.kind == LayerKind::Clean)
            .expect("clean base layer present");
        let upper = layers
            .iter()
            .find(|l| l.id == upper_id)
            .expect("raster resident");
        let merged = composite_to_page(&[below, upper], size);
        let (fx, fy, fw, fh) = page_footprint_rect(upper, size).expect("footprint on the page");

        for y in 0..size[1] {
            for x in 0..size[0] {
                let inside = x >= fx && x < fx + fw && y >= fy && y < fy + fh;
                if !inside {
                    assert_eq!(
                        merged.pixels[y * size[0] + x],
                        below.image.pixels[y * size[0] + x],
                        "pixel ({x},{y}) lies outside the footprint and must be unchanged"
                    );
                }
            }
        }
        // And the bound is actually a bound, not the whole page.
        assert!(fw < size[0] && fh < size[1], "the footprint must be tight");
    }

    /// A layer dragged entirely off the page contributes nothing, so there is no rect to write.
    #[test]
    fn a_footprint_entirely_off_the_page_is_none() {
        let size = [8, 8];
        let mut stack = LayerStack::new(
            0,
            size,
            filled(size, Color32::TRANSPARENT),
            filled(size, Color32::TRANSPARENT),
        );
        let id = stack.add_raster_layer();
        let layer = stack.layer_mut(id).expect("raster resident");
        layer.image = filled([2, 2], Color32::RED);
        layer.transform = LayerTransform {
            center: Vec2::new(-50.0, -50.0),
            rotation: 0.0,
            scale: 1.0,
        };
        assert_eq!(page_footprint_rect(layer, size), None);
    }

    /// A degenerate transform must widen the bound to the whole page, never guess a smaller one:
    /// losing merged pixels is worse than one expensive write.
    #[test]
    fn a_non_finite_footprint_falls_back_to_the_whole_page() {
        let size = [8, 8];
        let mut stack = LayerStack::new(
            0,
            size,
            filled(size, Color32::TRANSPARENT),
            filled(size, Color32::TRANSPARENT),
        );
        let id = stack.add_raster_layer();
        let layer = stack.layer_mut(id).expect("raster resident");
        layer.image = filled([2, 2], Color32::RED);
        layer.transform = LayerTransform {
            center: Vec2::new(f32::NAN, 0.0),
            rotation: 0.0,
            scale: 1.0,
        };
        assert_eq!(page_footprint_rect(layer, size), Some((0, 0, 8, 8)));
    }
}
