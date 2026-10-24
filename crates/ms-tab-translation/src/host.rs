/*
FILE OVERVIEW: crates/ms-tab-translation/src/host.rs
`TranslationTab`: the «Перевод» program tab as the studio root holds it — the tab's own
`CanvasView` together with its `TranslationTabState`, so the root never touches the Translation
canvas directly. Mirrors what the Cleaning and Typing tabs already are (they own their canvases).

Key items:
- `TranslationTab`: owns the canvas and the tab state.
- `TranslationTab::new`: builds the canvas (scroll salt, bubbles model, bottom-hint collapsed
  seed from `user_config` `Canvas.translation_hint_collapsed`). It seeds NO canvas settings: the
  canvas pulls `SharedCanvasSettings` from the bubbles model on its first draw.
- `TranslationDrawParams` / `TranslationTab::draw`: this frame's input (handed to the tab state
  for the draw, fired commands executed), the bottom-hint rows built from binding labels, the
  per-frame canvas configuration, the tools' on-set restore and per-frame preparation, and the
  canvas draw inside the root's central panel (the dock, with the tools, runs in its hook).
- `impl StudioTab for TranslationTab`: input declarations (`crate::input::declarations`) and
  contexts (the canvas's plus the tab's), dock builder, GPU eviction, source-page window,
  NEAREST sampling, the exit-only hint-collapsed key.
- `hotkeys` (submodule): the execute arms of every fired command.

Notes:
- Field order is drop order: the canvas drops before the tab state, as it did as a root field.
- `TranslationTabState` keeps taking `canvas: &mut CanvasView` and implementing `CanvasHooks` on
  itself; this struct is what makes the split borrow `(&mut canvas, &mut state)` possible.
*/

mod hotkeys;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::path::PathBuf;

use ms_canvas::{CanvasBottomHint, CanvasDrawParams, CanvasUiStatus, CanvasView, SourceTextureUploadBudget};
use ms_config::CANVAS_SECTION;
use ms_memory::{CacheEvictionReport, CacheEvictionRequest};
use ms_models::bubbles_model::BubblesModel;
use ms_models::page_view::{PageImageInfo, PageTexture};
use ms_project::ProjectData;
use ms_input::{ContextId, Declarations, InputFrame, InputRegistry};
use ms_studio_tab::StudioTab;
use ms_widgets::panel_dock::{DockLayout, PanelDockState};
use serde_json::Value;

use crate::panels::text_detector::detector_page_size;
use crate::tab::{TranslationTabState, translation_default_dock_layout};

/// `user_config` key (under `Canvas`) of the Translation canvas' bottom-hint collapsed flag.
/// Absent = `false` (expanded). Read once in [`TranslationTab::new`], written only on exit
/// ([`StudioTab::write_exit_settings`]); never saved per toggle.
const TRANSLATION_HINT_COLLAPSED_KEY: &str = "translation_hint_collapsed";

/// The scroll-area id salt of the Translation canvas (distinct from the Cleaning / Typing ones, so
/// the three canvases never share scroll state).
const TRANSLATION_CANVAS_SCROLL_SALT: &str = "translation_canvas_scroll";

/// The «Перевод» program tab: its canvas and its state.
pub struct TranslationTab {
    // Declared first: dropped before the state, the order the root's fields had.
    canvas: CanvasView,
    state: TranslationTabState,
}

/// What [`TranslationTab::draw`] borrows from the root for one frame.
pub struct TranslationDrawParams<'a> {
    pub ctx: &'a egui::Context,
    /// The window-root `Ui`: the tab opens its own `CentralPanel` in it.
    pub ui: &'a mut egui::Ui,
    pub project: &'a ProjectData,
    /// Source-page geometry and load state (also the detector panel's per-page plan notice).
    pub page_infos: &'a HashMap<usize, PageImageInfo>,
    /// The resident source textures lent by the root (the canvas may re-upload into them).
    pub texture_cache: &'a mut HashMap<usize, PageTexture>,
    pub status: CanvasUiStatus,
    /// The studio window's panel dock; the canvas carries it through to this tab's overlay hook.
    pub panel_dock: &'a mut PanelDockState,
    /// This frame's input (`InputRouter::begin_frame`, run by the root before any tab draws):
    /// the fired commands this tab executes, its modes, claims and gestures.
    pub input: &'a InputFrame,
    /// The studio's input registry: binding labels for the hints.
    pub input_registry: &'a InputRegistry,
}

impl TranslationTab {
    /// Builds the tab around an already-constructed `state` (the root injects its services).
    /// The canvas gets the Translation scroll salt, `bubbles_model`, and the bottom-hint collapsed
    /// flag read from `user_settings` (`Canvas.translation_hint_collapsed`; missing or non-bool =
    /// expanded). The composition presets are seeded from `user_settings` too and persist their
    /// operations to `preset_store` (production: `ms_config::user_config_path()`; `None` keeps them
    /// in memory — every test passes `None`, so no test can write the real `user_config`). No
    /// canvas settings are seeded: the canvas pulls them from `bubbles_model` by revision on its
    /// first draw.
    #[must_use]
    pub fn new(mut state: TranslationTabState, bubbles_model: Arc<Mutex<BubblesModel>>, user_settings: &Value, preset_store: Option<PathBuf>) -> Self {
        let mut canvas = CanvasView::default();
        canvas.set_bottom_hint_collapsed(ms_config::section_bool(user_settings, CANVAS_SECTION, TRANSLATION_HINT_COLLAPSED_KEY, false));
        canvas.set_scroll_area_id_salt(TRANSLATION_CANVAS_SCROLL_SALT);
        canvas.set_bubbles_model(bubbles_model);
        state.seed_composition_presets(user_settings, preset_store);
        Self { canvas, state }
    }

    /// The tab's canvas (viewport snapshot, view sync, bubble flush).
    #[must_use]
    pub fn canvas(&self) -> &CanvasView {
        &self.canvas
    }

    /// The tab's canvas, mutably (view sync, the cross-canvas bubble flush).
    pub fn canvas_mut(&mut self) -> &mut CanvasView {
        &mut self.canvas
    }

    /// The tab state.
    #[must_use]
    pub fn state(&self) -> &TranslationTabState {
        &self.state
    }

    /// The tab state, mutably (session capabilities, characters-changed notification).
    pub fn state_mut(&mut self) -> &mut TranslationTabState {
        &mut self.state
    }

    /// Draws the tab for one frame, in this order: the frame's input handed to the tab state
    /// (with this frame's binding labels), the tools' on-set restored from the dock (first frame
    /// only, before any hotkey), the fired commands, the brush actions, the bottom-hint rows, the
    /// lazy per-title settings load, the tools' per-frame preparation (current page size for the
    /// detector, health and route-input caches), the canvas' per-frame flags, the canvas inside a
    /// `CentralPanel` of `p.ui` (the panel dock with the toolbar and the tool tabs runs inside it,
    /// in the overlay hook), then the input copy is dropped.
    pub fn draw(&mut self, p: TranslationDrawParams<'_>) {
        let TranslationDrawParams { ctx, ui, project, page_infos, texture_cache, status, panel_dock, input, input_registry } = p;
        self.state.begin_input_frame(input, input_registry);
        self.state.restore_tool_toggles(panel_dock);
        self.run_fired_commands(ctx, project, input);
        if self.state.apply_brush_input(input) {
            ctx.request_repaint();
        }
        let Self { canvas, state } = self;
        canvas.set_bottom_hint(Some(CanvasBottomHint { rows: state.input_labels().hint_rows(state.lines_edit_active()) }));
        state.sync_with_project_settings(project);
        state.prepare_tool_frame(detector_page_size(page_infos.get(&canvas.current_page_idx())));
        canvas.set_overlay_render_suppressed(false);
        canvas.set_overlay_upload_min_interval_s(0.0);
        // `panel_dock` is lent for the frame and disjoint from `state` and `canvas`; the canvas
        // only carries it through to the overlay hook, where this tab runs its dock.
        egui::CentralPanel::default().show(ui, |ui| {
            let mut source_upload_budget = SourceTextureUploadBudget::source_page_reupload_default();
            canvas.draw(CanvasDrawParams {
                ctx,
                ui,
                project,
                page_infos,
                texture_cache,
                status,
                source_upload_budget: &mut source_upload_budget,
                hooks: state,
                panel_dock,
                input,
                input_registry,
            });
        });
        state.end_input_frame();
    }
}

impl StudioTab for TranslationTab {
    /// The tab's whole input set (`crate::input::declarations`); pinned by `input/tests.rs`
    /// (the root's `input_registry_golden` checks the merged registry stays conflict-free).
    fn input_declarations(&self) -> Declarations {
        crate::input::declarations()
    }

    /// The canvas's contexts (`CanvasView::input_contexts`: it edits bubbles),
    /// then the tab's (`TranslationTabState::input_contexts`: detector edit modes, the
    /// advanced-recognition window and its brush, `BRUSH_ACTIVE` while a brush is on).
    fn input_contexts(&self) -> Vec<ContextId> {
        let mut contexts = self.canvas.input_contexts();
        contexts.extend(self.state.input_contexts());
        contexts
    }

    fn default_dock_layout(&self) -> Option<fn() -> DockLayout> {
        Some(translation_default_dock_layout)
    }

    /// The canvas' clean-overlay GPU cache, then the detector-mask textures.
    fn evict_tab_gpu_caches(&mut self, request: &CacheEvictionRequest) -> CacheEvictionReport {
        let mut report = self.canvas.evict_clean_overlay_gpu_cache(request);
        report.append(self.state.evict_text_detector_gpu_cache(request));
        report
    }

    fn source_page_window(&self, neighbor_radius: usize) -> HashSet<usize> {
        self.canvas.active_source_page_window(neighbor_radius)
    }

    fn wants_nearest_source(&self) -> bool {
        self.canvas.source_pixel_inspection_active()
    }

    /// Writes `Canvas.translation_hint_collapsed` (the canvas' current collapsed state).
    fn write_exit_settings(&self, root: &mut Value) {
        ms_config::set_section_bool(root, CANVAS_SECTION, TRANSLATION_HINT_COLLAPSED_KEY, self.canvas.bottom_hint_collapsed());
    }
}

#[cfg(test)]
mod tests;
