/*
FILE OVERVIEW: crates/ms-tab-translation/src/tab/tool_panels.rs
The Translation tab's five tools — Bubbles, OCR, Composition, machine translation and the text
detector: the tool vocabulary, the on/off state of each tool, the «Инструменты» toolbar body that
toggles them, and one dock-tab body per tool.

Main items:
- `TranslationTool`: the five tools, their dock tab ids, persistence flag keys and captions.
- `ToolToggles`: which tools are switched on — independent flags, several may be on at once.
- `TRANSLATION_TOOLS_TAB`: the dock tab id of the «Инструменты» toolbar; its stored `TabExtras`
  carry the on-set across runs.
- `TranslationTabState::restore_tool_toggles` (host, first frame), `toggle_tool` /
  `set_tool_on` and the `toggle_*_panel_hotkey` entry points, `prepare_tool_frame` (host, before
  the canvas draw), `apply_tool_dock_requests` (before `PanelDock::begin`: focus + persistence).
- `TranslationTabState::draw_tools_bar_body` / `tools_tab_sizes`: the toolbar body (five
  latching buttons) and the outer size measured from their captions.
- `TranslationTabState::draw_tool_body`: routes to `draw_bubbles_tool_body`,
  `draw_ocr_tool_body`, `draw_composition_tool_body`, `draw_mt_tool_body`,
  `draw_detector_tool_body`.

Notes:
GUI thread only. "Open" is a STATE flag (`ToolToggles`), never read back from the dock: the
detector overlay is decided inside the canvas draw, before the dock runs. Switching a tool on
queues a focus request (its tab comes to the front of an expanded panel on the next dock frame,
unless it shares the toolbar's panel);
switching the detector off turns off its lines / mask edit modes. The bodies run the tools' panels
(`crate::panels::*`) and apply the actions they return; heavy work starts on the controllers'
workers.
*/

use eframe::egui;
use egui::{Color32, Vec2};
use ms_canvas::CanvasView;
use ms_config::app_tab::AppTab;
use ms_local_ai::local_ai::AiRouteKind;
use ms_project::ProjectData;
use ms_theme::Severity;
use ms_widgets::panel_dock::{PanelDockState, TabId};
use web_time::Duration;

use super::{TranslationTabState, build_ocr_runtime_options};
use crate::ocr::{OcrEngine, OcrLoadState};
use crate::panels::bubbles::{BubblesPanelContext, draw_bubbles_panel};
use crate::panels::composition::draw_composition_panel;
use crate::panels::machine_translation::{MtPanelRun, draw_machine_translation_panel};
use crate::panels::ocr::{OcrPanelModelView, OcrPanelNeurolate, OcrSelectionHints, draw_ocr_panel};
use crate::panels::text_detector::{
    TextDetectorAlgorithm, TextDetectorNeurolate, TextDetectorPanelView, draw_text_detector_panel,
};
use crate::text_detector::SURYA_DETECTOR_SPECS;

// Dock tab ids of the toolbar and the five tools. Stable, non-localised literals: they are the
// identity the persisted layout and every egui id of the owning panel derive from
// (`dev-docs/i18n_exclusions.md` §A9).
/// «Инструменты» — the toolbar of the five tool toggles.
pub(super) const TRANSLATION_TOOLS_TAB: TabId = TabId::new("translation.tools");
/// The Bubbles tool's dock tab.
const TRANSLATION_BUBBLES_TOOL_TAB: TabId = TabId::new("translation.tool.bubbles");
/// The OCR tool's dock tab.
const TRANSLATION_OCR_TOOL_TAB: TabId = TabId::new("translation.tool.ocr");
/// The Composition tool's dock tab.
const TRANSLATION_COMPOSITION_TOOL_TAB: TabId = TabId::new("translation.tool.composition");
/// The machine-translation tool's dock tab.
const TRANSLATION_MT_TOOL_TAB: TabId = TabId::new("translation.tool.machine_translation");
/// The text detector's dock tab.
const TRANSLATION_DETECTOR_TOOL_TAB: TabId = TabId::new("translation.tool.text_detector");

/// Stored value of a tool's on flag when the toolbar's extras say nothing: every tool starts off,
/// and a tool at its default leaves no trace in the config (`TabExtras::set_flag`).
const TOOL_ON_DEFAULT: bool = false;

/// Extra width, in points, added to every measured toolbar button. egui's real button frame also
/// carries the widget's expansion and border stroke, which vary with the interaction state; erring
/// high costs a few points of bar width, erring low wraps the bar onto a second row. Same value
/// and rationale as the cleaning tab's tool buttons.
const TOOL_BUTTON_WIDTH_SLACK_PX: f32 = 4.0;
/// Width, in points, a dock panel spends on its own chrome before its body: the `Frame::popup`
/// inner margin plus its border stroke, both sides, rounded up (same measurement as the cleaning
/// tab's `CLEANING_PANEL_CHROME_WIDTH_PX`).
const TOOLS_PANEL_CHROME_WIDTH_PX: f32 = 16.0;
/// Smallest outer height, in points, of the toolbar panel: the header strip plus one button row.
/// A floor, not a request — the body scrolls below it.
const TOOLS_TAB_MIN_HEIGHT_PX: f32 = 64.0;
/// Outer height, in points, the toolbar panel starts at: header plus one row of buttons. Only the
/// first frame uses it; the height converges to the content on the second.
const TOOLS_TAB_INITIAL_HEIGHT_PX: f32 = 72.0;
/// Smallest outer size, in points, of a tool panel. A floor only: the tool bodies scroll below it.
pub(super) const TOOL_TAB_MIN_SIZE_PX: Vec2 = Vec2::new(240.0, 120.0);
/// How often the OCR tool body asks for a frame while it is drawn: its status lines (engine load
/// state, backend health, model install state) change on worker / probe updates that raise no
/// egui event of their own. Requested from the body, so a hidden, collapsed or background OCR tab
/// costs no idle frames.
const OCR_BODY_REFRESH: Duration = Duration::from_millis(350);
/// Outer size, in points, a tool panel starts at — the default size of the floating tool window
/// the tool tabs replaced.
pub(super) const TOOL_TAB_INITIAL_SIZE_PX: Vec2 = Vec2::new(360.0, 460.0);

/// One of the Translation tab's five tools. Each has its own toolbar button, hotkey and dock tab.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum TranslationTool {
    Bubbles,
    Ocr,
    Composition,
    MachineTranslation,
    TextDetector,
}

impl TranslationTool {
    /// Every tool, in toolbar (and default tab) order.
    pub(super) const ALL: [Self; 5] = [
        Self::Bubbles,
        Self::Ocr,
        Self::Composition,
        Self::MachineTranslation,
        Self::TextDetector,
    ];

    /// Position in [`TranslationTool::ALL`], the index of the tool's flag in [`ToolToggles`].
    const fn index(self) -> usize {
        match self {
            Self::Bubbles => 0,
            Self::Ocr => 1,
            Self::Composition => 2,
            Self::MachineTranslation => 3,
            Self::TextDetector => 4,
        }
    }

    /// The tool's dock tab id; every one of them is named by `translation_default_dock_layout`.
    pub(super) const fn tab_id(self) -> TabId {
        match self {
            Self::Bubbles => TRANSLATION_BUBBLES_TOOL_TAB,
            Self::Ocr => TRANSLATION_OCR_TOOL_TAB,
            Self::Composition => TRANSLATION_COMPOSITION_TOOL_TAB,
            Self::MachineTranslation => TRANSLATION_MT_TOOL_TAB,
            Self::TextDetector => TRANSLATION_DETECTOR_TOOL_TAB,
        }
    }

    /// Key of the tool's on flag in the toolbar tab's stored `TabExtras`. A persistence key:
    /// never rename one without migrating the stored layouts.
    const fn extras_flag(self) -> &'static str {
        match self {
            Self::Bubbles => "tool.bubbles",
            Self::Ocr => "tool.ocr",
            Self::Composition => "tool.composition",
            Self::MachineTranslation => "tool.machine_translation",
            Self::TextDetector => "tool.text_detector",
        }
    }

    /// Localized caption of the tool's dock tab.
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Bubbles => t!("translation.tab.bubbles_title"),
            Self::Ocr => t!("translation.tab.panel_ocr_title"),
            Self::Composition => t!("translation.tab.panel_composition_title"),
            Self::MachineTranslation => t!("translation.tab.panel_mt_title"),
            Self::TextDetector => t!("translation.text_detector.title"),
        }
    }

    /// Localized caption of the tool's toolbar button (machine translation uses a shorter one).
    fn short_button_title(self) -> &'static str {
        match self {
            Self::Bubbles => t!("translation.tab.bubbles_title"),
            Self::Ocr => t!("translation.tab.panel_ocr_title"),
            Self::Composition => t!("translation.tab.panel_composition_title"),
            Self::MachineTranslation => t!("translation.tab.panel_mt_short_title"),
            Self::TextDetector => t!("translation.text_detector.title"),
        }
    }
}

/// Which of the five tools are switched on. Independent flags: any subset may be on.
///
/// The ONE source of truth for whether a tool is open — its dock tab is declared with
/// `.visible(is_on)`, the detector overlay reads it during the canvas draw — and persisted as the
/// toolbar tab's `TabExtras` flags ([`TranslationTool::extras_flag`]).
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub(super) struct ToolToggles {
    /// One flag per tool, indexed by [`TranslationTool::index`].
    on: [bool; 5],
}

impl ToolToggles {
    /// Whether `tool` is switched on.
    #[must_use]
    pub(super) fn is_on(self, tool: TranslationTool) -> bool {
        self.on[tool.index()]
    }

    /// Switches `tool` on or off.
    fn set(&mut self, tool: TranslationTool, on: bool) {
        self.on[tool.index()] = on;
    }

    /// Whether at least one tool is switched on.
    #[must_use]
    pub(super) fn any_on(self) -> bool {
        self.on.iter().any(|on| *on)
    }

    /// The on-set stored in the toolbar tab's extras of the «Перевод» layout; all off when the
    /// tab never stored anything.
    #[must_use]
    fn read(dock: &PanelDockState) -> Self {
        let mut toggles = Self::default();
        if let Some(extras) = dock.tab_extras(AppTab::Translation.key(), TRANSLATION_TOOLS_TAB) {
            for tool in TranslationTool::ALL {
                toggles.set(tool, extras.flag(tool.extras_flag(), TOOL_ON_DEFAULT));
            }
        }
        toggles
    }

    /// Writes the on-set into the toolbar tab's extras; the dock dirties itself (and so persists)
    /// only for a flag that really changed.
    fn write(self, dock: &mut PanelDockState) {
        for tool in TranslationTool::ALL {
            dock.set_tab_extras_flag(
                AppTab::Translation.key(),
                TRANSLATION_TOOLS_TAB,
                tool.extras_flag(),
                self.is_on(tool),
                TOOL_ON_DEFAULT,
            );
        }
    }
}

/// Outer `(min, initial)` widths, in points, of the toolbar panel for buttons of `button_widths`
/// laid out with `item_spacing_x` between them, plus `chrome` (panel frame and scroll-bar
/// reserve). The minimum holds the widest button (the row wraps below that); the initial width
/// holds the whole row, since a dock panel never re-measures its own width. Non-finite and
/// non-positive measurements are ignored so a broken galley cannot poison the solver.
#[must_use]
fn tools_bar_outer_widths(button_widths: &[f32], item_spacing_x: f32, chrome: f32) -> (f32, f32) {
    let usable = || button_widths.iter().copied().filter(|width| width.is_finite() && *width > 0.0);
    let widest = usable().fold(0.0_f32, f32::max);
    let row = usable().map(|width| width + item_spacing_x).sum::<f32>();
    // One spacing per GAP: the loop above added one after the last button too.
    let row = (row - item_spacing_x).max(widest);
    (widest + chrome, row + chrome)
}

impl TranslationTabState {
    fn tool_shortcut_hint(&self, tool: TranslationTool) -> Option<&str> {
        let labels = &self.input_labels;
        match tool {
            TranslationTool::Bubbles => labels.bubbles_panel.as_deref(),
            TranslationTool::Ocr => labels.ocr_panel.as_deref(),
            TranslationTool::Composition => labels.composition_panel.as_deref(),
            TranslationTool::MachineTranslation => labels.machine_translation_panel.as_deref(),
            TranslationTool::TextDetector => labels.text_detector_panel.as_deref(),
        }
    }

    /// Toolbar button caption: the short title plus the toggle's binding text (from the input
    /// registry, never spelled here).
    fn tool_button_label(&self, tool: TranslationTool) -> String {
        match self.tool_shortcut_hint(tool) {
            Some(shortcut) if !shortcut.is_empty() => {
                format!("{} ({shortcut})", tool.short_button_title())
            }
            _ => tool.short_button_title().to_string(),
        }
    }

    /// Adopts the on-set persisted in the dock, once per state (the first frame the host draws,
    /// before any hotkey of that frame). Restored tools take no switch-on side effects: their
    /// panel keeps the front tab and collapsed state the layout stored.
    pub fn restore_tool_toggles(&mut self, panel_dock: &PanelDockState) {
        if self.tools_restored {
            return;
        }
        self.tools_restored = true;
        self.tools = ToolToggles::read(panel_dock);
    }

    /// Per-frame tool bookkeeping the host runs before the canvas draw: caches the current page's
    /// size for the detector body (the overlay hook has no `page_infos`), and while any tool is
    /// on refreshes the backend health snapshot and the throttled OCR route inputs the tool
    /// bodies gate on.
    pub fn prepare_tool_frame(&mut self, current_page_size: Option<[u32; 2]>) {
        self.current_page_size = current_page_size;
        if !self.tools.any_on() {
            return;
        }
        self.refresh_ai_backend_health_snapshot_cache();
        // Kept fresh for every tool, not just OCR: the detector also gates its algorithm buttons
        // on onnx availability, so the native-armed signal must be published whichever tool is
        // shown. Throttled internally, so this is a cheap no-op most frames.
        #[cfg(not(target_arch = "wasm32"))]
        self.refresh_ocr_route_inputs_cache();
    }

    pub fn toggle_ocr_panel_hotkey(&mut self) {
        self.toggle_tool(TranslationTool::Ocr);
    }

    pub fn toggle_bubbles_panel_hotkey(&mut self) {
        self.toggle_tool(TranslationTool::Bubbles);
    }

    pub fn toggle_composition_panel_hotkey(&mut self) {
        self.toggle_tool(TranslationTool::Composition);
    }

    pub fn toggle_machine_translation_panel_hotkey(&mut self) {
        self.toggle_tool(TranslationTool::MachineTranslation);
    }

    pub fn toggle_text_detector_panel_hotkey(&mut self) {
        self.toggle_tool(TranslationTool::TextDetector);
    }

    /// Flips one tool, leaving the others as they are (toolbar button and hotkey).
    pub(super) fn toggle_tool(&mut self, tool: TranslationTool) {
        self.set_tool_on(tool, !self.tools.is_on(tool));
    }

    /// Switches `tool` on or off; a no-op when it already is. Switching ON queues the focus of its
    /// dock tab (front tab of an expanded panel, applied before the next `PanelDock::begin`) and,
    /// for Composition, a rebuild of the composed text; switching the detector OFF turns off its
    /// lines / mask edit modes. Every change is persisted on the next dock frame.
    pub(super) fn set_tool_on(&mut self, tool: TranslationTool, on: bool) {
        if self.tools.is_on(tool) == on {
            return;
        }
        self.tools.set(tool, on);
        self.tools_persist_pending = true;
        if on {
            if !self.pending_tool_focus.contains(&tool) {
                self.pending_tool_focus.push(tool);
            }
            if tool == TranslationTool::Composition {
                self.composition_rebuild_requested = true;
            }
        } else {
            self.pending_tool_focus.retain(|pending| *pending != tool);
            if tool == TranslationTool::TextDetector {
                self.set_text_detector_edit_lines_mode(false);
                self.set_text_detector_edit_mask_mode(false);
            }
        }
    }

    /// Applies what the tool switches of past frames asked of the dock, BEFORE `PanelDock::begin`
    /// (which holds the state borrow): each newly switched-on tool's tab is brought to the front of
    /// its panel, which is expanded (`PanelDockState::focus_tab`; a tab not seeded into a panel
    /// yet keeps its request for the next frame; a tool sharing the toolbar's panel is never
    /// focused, so the toolbar stays in front), and a changed on-set is written into the
    /// toolbar's extras, which is what persists it.
    pub(super) fn apply_tool_dock_requests(&mut self, panel_dock: &mut PanelDockState) {
        let key = AppTab::Translation.key();
        self.pending_tool_focus.retain(|tool| {
            // A tool the user docked INTO the toolbar's panel is not brought to the front: that
            // would bury the toolbar, the only way back to a switched-off tool. Its tab caption
            // still appears in the panel's strip. The request is consumed either way.
            let toolbar_panel = panel_dock.layout(key).and_then(|layout| layout.panel_of_tab(TRANSLATION_TOOLS_TAB));
            let tool_panel = panel_dock.layout(key).and_then(|layout| layout.panel_of_tab(tool.tab_id()));
            if tool_panel.is_some() && tool_panel == toolbar_panel {
                return false;
            }
            !panel_dock.focus_tab(key, tool.tab_id())
        });
        if self.tools_persist_pending {
            self.tools.write(panel_dock);
            self.tools_persist_pending = false;
        }
    }

    /// Outer `(min_size, initial_size)` of the toolbar tab, measured from this frame's button
    /// captions (text layout only, cached by egui; safe on the GUI thread). Re-measured every
    /// frame, so a language or binding change moves the width floor with no invalidation path.
    #[must_use]
    pub(super) fn tools_tab_sizes(&self, ctx: &egui::Context) -> (Vec2, Vec2) {
        let style = ctx.style_of(ctx.theme());
        let font_id = egui::TextStyle::Button.resolve(&style);
        let widths = TranslationTool::ALL.map(|tool| {
            let caption = self.tool_button_label(tool);
            // The colour is irrelevant to the measurement; the galley is never painted.
            let text = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(caption, font_id.clone(), Color32::WHITE).size().x);
            text + style.spacing.button_padding.x * 2.0 + TOOL_BUTTON_WIDTH_SLACK_PX
        });
        // The body's vertical scroll bar: the wider of what it takes and what it paints over
        // (egui's default floating bar allocates nothing but covers the body's right strip).
        let scroll = &style.spacing.scroll;
        let scrollbar_reserve = scroll.allocated_width().max(scroll.bar_width + scroll.bar_outer_margin);
        let (min_width, initial_width) =
            tools_bar_outer_widths(&widths, style.spacing.item_spacing.x, TOOLS_PANEL_CHROME_WIDTH_PX + scrollbar_reserve);
        (Vec2::new(min_width, TOOLS_TAB_MIN_HEIGHT_PX), Vec2::new(initial_width, TOOLS_TAB_INITIAL_HEIGHT_PX))
    }

    /// Draws the «Инструменты» toolbar body: one latching button per tool, showing whether the
    /// tool is on; a click flips that tool alone. The row wraps (never a caption) when the panel
    /// is narrower than the bar.
    pub(super) fn draw_tools_bar_body(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            // Inside a wrapping layout egui would break a long caption over two lines; `Extend`
            // keeps the natural caption width the panel's width floor is measured against.
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            for tool in TranslationTool::ALL {
                let button = egui::Button::new(self.tool_button_label(tool)).selected(self.tools.is_on(tool));
                if ui.add(button).clicked() {
                    self.toggle_tool(tool);
                }
            }
        });
    }

    /// Draws `tool`'s dock-tab body.
    pub(super) fn draw_tool_body(
        &mut self,
        tool: TranslationTool,
        ui: &mut egui::Ui,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        let ctx = ui.ctx().clone();
        match tool {
            TranslationTool::Bubbles => self.draw_bubbles_tool_body(ui, &ctx, canvas, project),
            TranslationTool::Ocr => self.draw_ocr_tool_body(ui, &ctx, project),
            TranslationTool::MachineTranslation => self.draw_mt_tool_body(ui, &ctx, canvas, project),
            TranslationTool::Composition => self.draw_composition_tool_body(ui, project),
            TranslationTool::TextDetector => self.draw_detector_tool_body(ui, &ctx, canvas, project),
        }
    }

    /// Draws the Bubbles tool body (bubble list and footer editor) and applies its edits to the
    /// tab's footer caches.
    fn draw_bubbles_tool_body(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        let mut panel_ctx = BubblesPanelContext {
            character_names: &self.character_names,
            footer_overrides: &mut self.footer_overrides,
            pending_footer_patches: &mut self.pending_footer_patches,
            pending_footer_patch_changed_at: &mut self.pending_footer_patch_changed_at,
            pending_characters_refresh: &mut self.pending_characters_refresh,
            last_is_known_character: &mut self.last_is_known_character,
            last_character_name: &mut self.last_character_name,
            last_clarification: &mut self.last_clarification,
            last_page_idx: &mut self.last_page_idx,
            last_bubble_order: &mut self.last_bubble_order,
            image_bubble_pick: &mut self.image_bubble_pick,
            input: self.input_frame.as_ref(),
        };
        draw_bubbles_panel(
            &mut self.bubbles_panel,
            ui,
            ctx,
            canvas,
            project,
            &mut panel_ctx,
        );
    }

    /// Draws the OCR tool body and applies its actions: model probe / download, backend health
    /// check for a backend route, engine load, generation stop, AI API and NeuroLate key steps.
    fn draw_ocr_tool_body(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, project: &ProjectData) {
        ctx.request_repaint_after(OCR_BODY_REFRESH);
        let selected_engine_before = self.ocr_panel_options.engine;
        let selected_variant_before = self.ocr_panel_options.paddle_vl_model;
        let model_specs = self.selected_ocr_model_specs();
        // Opening the panel on (or switching to) a model never probed this session.
        self.ocr_model_downloads.probe_unknown(&model_specs);
        let model_snapshot =
            (!model_specs.is_empty()).then(|| self.ocr_model_downloads.panel_snapshot(&model_specs));
        // The MT source language is the input language of OCR; `TextTab.text_language` is the
        // typesetting (output) language and the wrong signal here.
        let korean_source = self.mt_panel_options.source_lang.trim().eq_ignore_ascii_case("ko");
        let force_health_check = self.ocr_last_panel_engine != Some(selected_engine_before);
        // Route-aware: a native ONNX or cloud selection needs no backend, so we do
        // NOT probe backend health or mark it unavailable for it. The panel itself is
        // never disabled as a whole: AI API OCR runs with every local runtime off
        // (`--no-ai`, Android), and local engines self-gate on their capabilities.
        let route = self.current_ocr_route_availability();
        let requires_backend = matches!(route, Ok(AiRouteKind::Backend));
        if requires_backend {
            self.request_ai_backend_health_check(ctx, force_health_check);
            self.sync_ocr_states_from_backend_health_snapshot();
        }
        let backend_unavailable = requires_backend && self.ai_backend_unavailable();
        let quick_selection_active = self.ocr_quick_selection_mode_active();
        let advanced_selection_active = self.ocr_advanced_selection_mode_active();
        let actions = draw_ocr_panel(
            ui,
            self.ocr_state_for_engine(self.ocr_panel_options.engine),
            &mut self.ocr_panel_options,
            self.ocr_controller.last_error(),
            self.ocr_controller.last_result(),
            OcrSelectionHints {
                quick_binding: &self.input_labels.quick_rect,
                quick_active: quick_selection_active,
                advanced_binding: &self.input_labels.advanced_rect,
                advanced_active: advanced_selection_active,
            },
            &self.ocr_controller.generation_snapshot(),
            OcrPanelModelView {
                korean_source,
                source_lang: &self.mt_panel_options.source_lang,
                download: model_snapshot.as_ref(),
                // Only the engine whose command downloads shows its progress.
                engine_download: self
                    .ocr_model_download
                    .current()
                    .filter(|_| self.ocr_loading_engine == Some(selected_engine_before)),
            },
            OcrPanelNeurolate { settings: &mut self.neurolate_settings, key: &mut self.neurolate_key, project_type: project.comic_type },
        );
        // Pumped only while its UI is drawn (see `tab/neurolate.rs`).
        if self.ocr_panel_options.engine == OcrEngine::NeuroLate {
            self.pump_neurolate_key(ctx, actions.neurolate_key);
        }
        if actions.options_changed {
            self.ocr_settings_dirty = true;
            if selected_engine_before != self.ocr_panel_options.engine
                && self.ocr_requires_backend_runtime()
            {
                self.request_ai_backend_health_check(ctx, true);
                self.sync_ocr_states_from_backend_health_snapshot();
            }
        }
        self.ocr_ai_api_tasks.submit_actions(&mut self.ocr_panel_options.ai_api, actions.ai_api);
        self.apply_ocr_model_panel_changes(&model_specs, selected_variant_before, actions.model_download);
        if let Some(shown) = actions.stop_generation {
            self.stop_ocr_generation(ctx, shown);
        }
        if actions.cancel_model_download {
            self.ocr_controller.cancel_model_download();
        }
        if actions.request_load && !backend_unavailable {
            if let Err(off) = route {
                self.push_toast(ctx, off.user_message(), Severity::Error, 2.8);
            } else if let Some(error) = self.current_ocr_torch_requirement_error() {
                self.push_toast(ctx, error, Severity::Error, 2.8);
            } else {
                self.mark_ocr_load_requested_for_engine(self.ocr_panel_options.engine);
                self.ocr_controller.request_load(
                    self.ocr_panel_options.engine,
                    build_ocr_runtime_options(&self.ocr_panel_options, &self.mt_panel_options.source_lang, &self.neurolate_settings, project.comic_type),
                );
            }
        }
        // The selected local engine cannot run in this process: name the runtime and
        // the real reason (`--no-ai`, or not on this platform). Read after the panel
        // ran, so an engine switch this frame shows its own reason.
        if let Err(off) = self.selected_ocr_route_availability() {
            ui.separator();
            ui.colored_label(ms_theme::status::WARNING, off.user_message());
        }
        self.ocr_last_panel_engine = Some(self.ocr_panel_options.engine);
    }

    /// Draws the machine-translation tool body and queues the runs and request previews it asks
    /// for. The preview itself is polled and drawn every frame by the overlay hook, so it keeps
    /// working while this tab is behind a sibling or switched off.
    fn draw_mt_tool_body(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        let mt_busy = self.mt_controller.is_busy();
        let mt_can_cancel = self.mt_has_active_or_pending();
        // Machine translation (plain translators and AI API) is network code: no local
        // AI runtime is involved, so neither `--no-ai` nor the platform gates it.
        let actions = draw_machine_translation_panel(
            ui,
            MtPanelRun {
                busy: mt_busy,
                can_cancel: mt_can_cancel,
                progress: self.mt_progress,
                generation: self.mt_controller.generation_snapshot(),
                issues: self.mt_controller.run_issues(),
            },
            &mut self.mt_stop_notice,
            &mut self.mt_panel_options,
            self.session_caps.title_documents,
        );
        if actions.options_changed {
            self.mt_settings_dirty = true;
        }
        self.mt_ai_api_tasks.submit_actions(&mut self.mt_panel_options.ai_api, actions.ai_api);
        if actions.start_all {
            self.pending_mt_start_all = true;
        }
        if actions.start_page {
            self.pending_mt_start_page = true;
        }
        if actions.preview_request_page {
            self.start_ai_mt_request_preview(ctx, canvas, project, true);
        }
        if actions.preview_request_all {
            self.start_ai_mt_request_preview(ctx, canvas, project, false);
        }
        if actions.cancel {
            self.cancel_active_mt(ctx);
        }
    }

    /// Draws the Composition tool body and rebuilds the composed text when the panel asks for
    /// it. A pending rebuild (tool just switched on, settings loaded) is done by the overlay hook
    /// before the dock runs, so it never waits for this body to come to the front.
    fn draw_composition_tool_body(&mut self, ui: &mut egui::Ui, project: &ProjectData) {
        let actions = draw_composition_panel(
            ui,
            project,
            &mut self.composition_panel_state,
            &mut self.composition_panel_options,
        );
        if actions.options_changed {
            self.composition_settings_dirty = true;
        }
        if actions.request_rebuild {
            self.rebuild_composition_text(project);
        }
    }

    /// Draws the text-detector tool body and applies its actions (detect, block OCR, edit modes,
    /// save / clear results). The current page's plan notice reads the size the host cached
    /// before the canvas draw (`current_page_size`); the stored detection is loaded by the
    /// overlay hook while the tool is on (`detector_overlay_visible`).
    fn draw_detector_tool_body(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        canvas: &mut CanvasView,
        project: &ProjectData,
    ) {
        let has_pages = !project.pages.is_empty();
        let availability = self.text_detector_availability();
        // OCR over detected blocks runs the selected OCR engine: its route decides.
        let block_ocr_route = self.current_ocr_route_availability();
        let can_ocr_current = block_ocr_route.is_ok()
            && self.ocr_controller.state() == OcrLoadState::Ready
            && self.has_detected_blocks_on_page(canvas.current_page_idx());
        let can_ocr_all = block_ocr_route.is_ok()
            && self.ocr_controller.state() == OcrLoadState::Ready
            && self.has_detected_blocks_any();
        // The same pages `detector_region::detection_pages_for_save` writes: detected
        // ones, or hand-painted ones (an all-erased painted page is refused on click).
        let can_save = !self.text_detection_storage_busy
            && (!self.text_detector_results.is_empty() || self.has_hand_painted_only_pages());
        let ocr_busy = self.textdetector_ocr_is_running();
        let page_size = self.current_page_size;
        // The Surya checkpoint's install state, probed once per session on selection.
        let surya_model = (self.text_detector_options.algorithm == TextDetectorAlgorithm::Surya).then(|| {
            self.ocr_model_downloads.probe_unknown(&SURYA_DETECTOR_SPECS);
            self.ocr_model_downloads.panel_snapshot(&SURYA_DETECTOR_SPECS)
        });
        let view = TextDetectorPanelView {
            status_text: &self.text_detector_status,
            status_color: self.text_detector_status_severity.color(),
            progress: self.text_detector_progress,
            model_download: self.text_detector_model_download.current(),
            surya_model: surya_model.as_ref(),
            detect_busy: self.detector_busy_for_batches(),
            batch_running: self.text_detector_controller.is_batch_running(),
            ocr_busy,
            has_pages,
            availability,
            can_ocr_current,
            can_ocr_all,
            can_save,
            storage_available: self.session_caps.chapter_storage,
            edit_lines_mode: self.text_detector_edit_lines_mode,
            edit_mask_mode: self.text_detector_edit_mask_mode,
            page_size,
        };
        let neurolate = TextDetectorNeurolate {
            settings: &mut self.neurolate_settings,
            key: &mut self.neurolate_key,
            mt_source_lang: &self.mt_panel_options.source_lang,
            project_type: project.comic_type,
        };
        let actions = draw_text_detector_panel(
            ui,
            &mut self.text_detector_options,
            &mut self.text_detector_plan_notice,
            &view,
            neurolate,
        );
        if actions.options_changed {
            self.text_detector_settings_dirty = true;
        }
        // NeuroLate settings persist with the OCR section (shared with the OCR panel).
        self.ocr_settings_dirty |= actions.neurolate_changed;
        if self.text_detector_options.algorithm == TextDetectorAlgorithm::NeuroLate {
            self.pump_neurolate_key(ctx, actions.neurolate_key);
        }
        if actions.cancel_detection {
            self.text_detector_controller.cancel_detection();
        }
        if actions.stop_detection {
            self.text_detector_controller.request_stop();
        }
        self.apply_detector_model_panel_action(actions.surya_model_download);
        if actions.toggle_edit_lines_mode {
            self.set_text_detector_edit_lines_mode(!self.text_detector_edit_lines_mode);
        }
        if actions.toggle_edit_mask_mode {
            self.set_text_detector_edit_mask_mode(!self.text_detector_edit_mask_mode);
        }
        if actions.save_results {
            self.start_text_detection_storage_save(project);
        }
        if actions.clear_results {
            self.clear_detector_result_stores();
            self.text_detector_progress = None;
            self.clear_text_detector_line_edit_state();
            self.set_text_detector_status(t!("translation.tab.results_cleared_status"), Severity::Success);
        }
        if actions.detect_current {
            self.start_text_detection_for_current_page(project, canvas);
        }
        if actions.detect_all {
            self.start_text_detection_for_all_pages(project);
        }
        if actions.ocr_current {
            self.start_text_detector_ocr_for_indices(
                ctx,
                project,
                vec![canvas.current_page_idx()],
            );
        }
        if actions.ocr_all {
            self.start_text_detector_ocr_for_indices(
                ctx,
                project,
                self.detected_page_indices(),
            );
        }

        if let Err(off) = block_ocr_route {
            ui.separator();
            ui.colored_label(
                ms_theme::status::WARNING,
                tf!("translation.tab.block_ocr_unavailable_status", reason = off.user_message()),
            );
        } else if self.ocr_controller.state() != OcrLoadState::Ready {
            ui.separator();
            ui.small(t!("translation.tab.block_ocr_load_engine_hint"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TRANSLATION_TOOLS_TAB, ToolToggles, TranslationTabState, TranslationTool, tools_bar_outer_widths};
    use crate::tab::translation_default_dock_layout;
    use ms_config::app_tab::AppTab;
    use ms_widgets::panel_dock::{PanelDockState, PanelId};

    /// A dock state holding the «Перевод» default arrangement, with nothing dirty.
    fn dock_with_default_layout() -> PanelDockState {
        let mut dock = PanelDockState::new();
        dock.ensure_default_layout(AppTab::Translation.key(), translation_default_dock_layout);
        dock.clear_dirty();
        dock
    }

    /// The panel of the default layout that owns the five tool tabs.
    fn tools_panel(dock: &PanelDockState) -> PanelId {
        dock.layout(AppTab::Translation.key())
            .and_then(|layout| layout.panel_of_tab(TranslationTool::Ocr.tab_id()))
            .expect("the default layout names the OCR tool")
    }

    /// Each tool toggles on its own: several can be on together, and a second toggle switches
    /// only that tool off again.
    #[test]
    fn tools_toggle_independently_and_several_can_be_on() {
        let mut tab = TranslationTabState::default();
        assert!(!tab.tools.any_on());
        tab.toggle_tool(TranslationTool::Ocr);
        tab.toggle_tool(TranslationTool::TextDetector);
        assert!(tab.tools.is_on(TranslationTool::Ocr));
        assert!(tab.tools.is_on(TranslationTool::TextDetector));
        assert!(!tab.tools.is_on(TranslationTool::Bubbles));

        tab.toggle_ocr_panel_hotkey();
        assert!(!tab.tools.is_on(TranslationTool::Ocr));
        assert!(tab.tools.is_on(TranslationTool::TextDetector), "another tool's toggle leaves it on");
        assert!(tab.tools_persist_pending);
    }

    /// Switching a tool on queues its focus (once, however often it is toggled), switching it off
    /// withdraws the request, and the detector's edit modes end with the detector — not when
    /// another tool comes on.
    #[test]
    fn switching_on_queues_a_focus_and_the_detector_takes_its_edit_modes_down() {
        let mut tab = TranslationTabState::default();
        tab.set_tool_on(TranslationTool::TextDetector, true);
        tab.set_text_detector_edit_mask_mode(true);
        tab.set_tool_on(TranslationTool::Bubbles, true);
        assert!(tab.text_detector_edit_mask_mode, "another tool switching on keeps the edit mode");
        assert_eq!(tab.pending_tool_focus, vec![TranslationTool::TextDetector, TranslationTool::Bubbles]);

        tab.set_tool_on(TranslationTool::TextDetector, false);
        assert!(!tab.text_detector_edit_mask_mode);
        assert!(!tab.text_detector_edit_lines_mode);
        assert_eq!(tab.pending_tool_focus, vec![TranslationTool::Bubbles]);

        tab.set_tool_on(TranslationTool::Bubbles, true);
        assert_eq!(tab.pending_tool_focus, vec![TranslationTool::Bubbles], "already on: no-op");

        tab.composition_rebuild_requested = false;
        tab.set_tool_on(TranslationTool::Composition, true);
        assert!(tab.composition_rebuild_requested, "Composition rebuilds when switched on");
    }

    /// A tool switched on comes to the FRONT of its (shared) panel, which is expanded; a tool
    /// already on is not refocused on later frames.
    #[test]
    fn a_tool_switched_on_is_brought_to_the_front_of_an_expanded_panel() {
        let mut dock = dock_with_default_layout();
        let panel = tools_panel(&dock);
        dock.layout_mut(AppTab::Translation.key())
            .expect("layout")
            .set_collapsed(panel, true)
            .expect("collapse");
        let mut tab = TranslationTabState::default();

        tab.set_tool_on(TranslationTool::MachineTranslation, true);
        tab.apply_tool_dock_requests(&mut dock);
        let node = dock.layout(AppTab::Translation.key()).and_then(|layout| layout.panel(panel)).expect("panel");
        assert_eq!(node.active_tab, TranslationTool::MachineTranslation.tab_id());
        assert!(!node.collapsed);
        assert!(tab.pending_tool_focus.is_empty());

        // The user brings another tab to the front; nothing re-focuses MT on its own.
        dock.layout_mut(AppTab::Translation.key())
            .expect("layout")
            .set_active_tab(panel, TranslationTool::Bubbles.tab_id())
            .expect("activate");
        tab.apply_tool_dock_requests(&mut dock);
        let node = dock.layout(AppTab::Translation.key()).and_then(|layout| layout.panel(panel)).expect("panel");
        assert_eq!(node.active_tab, TranslationTool::Bubbles.tab_id());
    }

    /// A tool the user docked into the toolbar's own panel does not bury the toolbar when it is
    /// switched on, and the request is not kept for later.
    #[test]
    fn a_tool_sharing_the_toolbar_panel_does_not_take_the_front() {
        let mut dock = dock_with_default_layout();
        let key = AppTab::Translation.key();
        let layout = dock.layout_mut(key).expect("layout");
        let toolbar = layout.panel_of_tab(TRANSLATION_TOOLS_TAB).expect("toolbar panel");
        layout.move_tab(TranslationTool::Ocr.tab_id(), toolbar, 1).expect("dock OCR into the toolbar panel");
        let mut tab = TranslationTabState::default();

        tab.set_tool_on(TranslationTool::Ocr, true);
        tab.apply_tool_dock_requests(&mut dock);
        let node = dock.layout(key).and_then(|layout| layout.panel(toolbar)).expect("panel");
        assert_eq!(node.active_tab, TRANSLATION_TOOLS_TAB);
        assert!(tab.pending_tool_focus.is_empty());
    }

    /// A focus request for a tab no panel owns yet (seeded at the end of its first declared frame)
    /// waits for a later frame instead of being lost.
    #[test]
    fn a_focus_request_waits_for_the_tab_to_be_seeded() {
        let mut dock = PanelDockState::new();
        let mut tab = TranslationTabState::default();
        tab.set_tool_on(TranslationTool::Ocr, true);
        tab.apply_tool_dock_requests(&mut dock);
        assert_eq!(tab.pending_tool_focus, vec![TranslationTool::Ocr]);
    }

    /// The on-set survives a restart: written into the toolbar's dock extras (dirtying the dock,
    /// which is what triggers the config write) and adopted by a fresh state, without
    /// re-running the switch-on side effects; an unchanged set writes nothing.
    #[test]
    fn the_on_set_round_trips_through_the_toolbar_extras() {
        let mut dock = dock_with_default_layout();
        let mut tab = TranslationTabState::default();
        tab.set_tool_on(TranslationTool::Ocr, true);
        tab.set_tool_on(TranslationTool::Composition, true);
        tab.apply_tool_dock_requests(&mut dock);
        assert!(dock.is_dirty());
        assert!(!tab.tools_persist_pending);
        dock.clear_dirty();
        tab.apply_tool_dock_requests(&mut dock);
        assert!(!dock.is_dirty(), "nothing changed, nothing to write");

        let mut restored = TranslationTabState::default();
        restored.restore_tool_toggles(&dock);
        assert_eq!(restored.tools, tab.tools);
        assert!(restored.pending_tool_focus.is_empty());
        assert!(!restored.tools_persist_pending);

        // Restored once per state: a later dock change does not override the live state.
        let mut other = ToolToggles::default();
        other.set(TranslationTool::Bubbles, true);
        other.write(&mut dock);
        restored.restore_tool_toggles(&dock);
        assert_eq!(restored.tools, tab.tools);

        // An untouched dock restores everything off.
        let mut fresh = TranslationTabState::default();
        fresh.restore_tool_toggles(&PanelDockState::new());
        assert!(!fresh.tools.any_on());
    }

    /// The toolbar starts wide enough for its whole row and may shrink to its widest button;
    /// broken measurements are ignored.
    #[test]
    fn the_toolbar_width_fits_the_row_and_floors_at_the_widest_button() {
        let (min, initial) = tools_bar_outer_widths(&[50.0, 80.0, 60.0], 8.0, 20.0);
        assert_eq!(min, 80.0 + 20.0);
        assert_eq!(initial, 50.0 + 80.0 + 60.0 + 2.0 * 8.0 + 20.0);
        let (min, initial) = tools_bar_outer_widths(&[f32::NAN, -3.0, 40.0], 8.0, 20.0);
        assert_eq!((min, initial), (60.0, 60.0));
        assert_eq!(tools_bar_outer_widths(&[], 8.0, 20.0), (20.0, 20.0));
    }
}
