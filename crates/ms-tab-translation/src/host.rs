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
- `TranslationDrawParams` / `TranslationTab::draw`: the fired input commands and the binding
  hints of this frame, the per-frame canvas configuration, the floating side panel and the
  canvas draw inside the root's central panel.
- `translation_hint_rows`: the fixed bottom-hint rows (`canvas.bottom_hint.translation.*`).
- `impl StudioTab for TranslationTab`: input declarations and contexts, dock builder, GPU
  eviction, source-page window, NEAREST sampling, the exit-only hint-collapsed key.
- `hotkeys` (submodule): the three canvas-level command ids, their specs, the execute arms and
  the per-frame binding hints (`hotkey_hints`).

Notes:
- Field order is drop order: the canvas drops before the tab state, as it did as a root field.
- `TranslationTabState` keeps taking `canvas: &mut CanvasView` and implementing `CanvasHooks` on
  itself; this struct is what makes the split borrow `(&mut canvas, &mut state)` possible.
*/

mod hotkeys;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use ms_canvas::{CanvasBottomHint, CanvasDrawParams, CanvasHintRow, CanvasUiStatus, CanvasView, SourceTextureUploadBudget};
use ms_config::CANVAS_SECTION;
use ms_memory::{CacheEvictionReport, CacheEvictionRequest};
use ms_models::bubbles_model::BubblesModel;
use ms_models::page_view::{PageImageInfo, PageTexture};
use ms_project::ProjectData;
use ms_input::{ContextId, Declarations, InputFrame, InputRegistry};
use ms_studio_tab::StudioTab;
use ms_widgets::panel_dock::{DockLayout, PanelDockState};
use serde_json::Value;

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
    /// The window-root `Ui`: the tab opens its own `CentralPanel` in it, after its side panel.
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
    /// the fired commands this tab executes and its active modes.
    pub input: &'a InputFrame,
    /// The studio's input registry: binding labels for the hints.
    pub input_registry: &'a InputRegistry,
}

impl TranslationTab {
    /// Builds the tab around an already-constructed `state` (the root injects its services).
    /// The canvas gets the Translation scroll salt, `bubbles_model`, and the bottom-hint collapsed
    /// flag read from `user_settings` (`Canvas.translation_hint_collapsed`; missing or non-bool =
    /// expanded). No canvas settings are seeded: the canvas pulls them from `bubbles_model` by
    /// revision on its first draw.
    #[must_use]
    pub fn new(state: TranslationTabState, bubbles_model: Arc<Mutex<BubblesModel>>, user_settings: &Value) -> Self {
        let mut canvas = CanvasView::default();
        canvas.set_bottom_hint_collapsed(ms_config::section_bool(user_settings, CANVAS_SECTION, TRANSLATION_HINT_COLLAPSED_KEY, false));
        canvas.set_scroll_area_id_salt(TRANSLATION_CANVAS_SCROLL_SALT);
        canvas.set_bubbles_model(bubbles_model);
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

    /// Draws the tab for one frame, in this order: the fired input commands, the binding hints,
    /// bottom-hint rows, the lazy per-title settings load, the floating side panel, the canvas'
    /// per-frame flags, then the canvas inside a `CentralPanel` of `p.ui`. The panel dock runs
    /// inside the canvas draw (overlay hook).
    pub fn draw(&mut self, p: TranslationDrawParams<'_>) {
        let TranslationDrawParams { ctx, ui, project, page_infos, texture_cache, status, panel_dock, input, input_registry } = p;
        self.run_fired_commands(ctx, project, input);
        if self.state.apply_brush_input(input) {
            ctx.request_repaint();
        }
        let (create_bubble_hint, hints) = hotkeys::hotkey_hints(input_registry, input);
        let Self { canvas, state } = self;
        canvas.set_create_bubble_shortcut_hint(create_bubble_hint);
        state.set_hotkey_hints(hints);
        canvas.set_bottom_hint(Some(CanvasBottomHint { rows: translation_hint_rows() }));
        state.sync_with_project_settings(project);
        state.draw_side_panel(ctx, canvas, project, page_infos);
        canvas.set_drag_scroll_blocked(false);
        canvas.set_wheel_scroll_blocked(false);
        canvas.set_zoom_blocked(state.blocks_canvas_zoom());
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
    }
}

impl StudioTab for TranslationTab {
    /// The host's three canvas commands (delete, create, create hint), then the tab state's own
    /// actions (OCR selection modes, panel toggles, bubble copy / paste), all in the Translation
    /// scope; pinned by `host/tests.rs` and the root's `input_registry_golden`.
    fn input_declarations(&self) -> Declarations {
        let mut actions = hotkeys::host_input_actions();
        actions.extend(TranslationTabState::input_actions());
        Declarations { actions, ..Declarations::default() }
    }

    /// The canvas's contexts (`CanvasView::input_contexts`: it edits bubbles; it zooms while
    /// the advanced-recognition window does not block canvas zoom, so that window's own zoom keys
    /// reach it), plus `ms_tools::mask_brush::BRUSH_ACTIVE` while a brush of this tab is on (the
    /// detector mask edit, the advanced-recognition brush).
    fn input_contexts(&self) -> Vec<ContextId> {
        let mut contexts = self.canvas.input_contexts();
        if self.state.brush_input_active() {
            contexts.push(ms_tools::mask_brush::BRUSH_ACTIVE);
        }
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

/// Builds the fixed, hand-authored Translation bottom-hint rows.
///
/// A curated set of localized label/key pairs (via `t!`), NOT sourced from the hotkey registry.
/// Rebuilt every frame so a runtime language switch re-localizes them. The order is canonical and
/// must match the documented Translation shortcut set. No row carries help, so the hint stays
/// two-column; attach one with `CanvasHintRow::with_help` when a shortcut needs it.
#[must_use]
fn translation_hint_rows() -> Vec<CanvasHintRow> {
    vec![
        CanvasHintRow::new(t!("canvas.bottom_hint.translation.create_bubble_label"), t!("canvas.bottom_hint.translation.create_bubble_keys")),
        CanvasHintRow::new(t!("canvas.bottom_hint.translation.create_bubble_recent_char_label"), t!("canvas.bottom_hint.translation.create_bubble_recent_char_keys")),
        CanvasHintRow::new(t!("canvas.bottom_hint.translation.ocr_bubble_label"), t!("canvas.bottom_hint.translation.ocr_bubble_keys")),
        CanvasHintRow::new(t!("canvas.bottom_hint.translation.ocr_bubble_recent_char_label"), t!("canvas.bottom_hint.translation.ocr_bubble_recent_char_keys")),
        CanvasHintRow::new(t!("canvas.bottom_hint.translation.advanced_ocr_label"), t!("canvas.bottom_hint.translation.advanced_ocr_keys")),
        CanvasHintRow::new(t!("canvas.bottom_hint.translation.create_image_bubble_label"), t!("canvas.bottom_hint.translation.create_image_bubble_keys")),
        CanvasHintRow::new(t!("canvas.bottom_hint.translation.create_image_bubble_crop_label"), t!("canvas.bottom_hint.translation.create_image_bubble_crop_keys")),
        CanvasHintRow::new(t!("canvas.bottom_hint.translation.create_hint_bubble_label"), t!("canvas.bottom_hint.translation.create_hint_bubble_keys")),
        CanvasHintRow::new(t!("canvas.bottom_hint.translation.delete_bubble_label"), t!("canvas.bottom_hint.translation.delete_bubble_keys")),
    ]
}

#[cfg(test)]
mod tests;
