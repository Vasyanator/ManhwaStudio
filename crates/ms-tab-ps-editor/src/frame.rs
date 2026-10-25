/*
File: crates/ms-tab-ps-editor/src/frame.rs

Purpose:
The per-frame entry point of the tab and the contents of its non-layer dock tabs.

Key functions:
- `draw()` (pub): one frame — dock area, panel visibility, tab declarations, deferred layer actions,
  canvas last.
- `poll_tools()`, `active_tool_title()`.
- `main_tab_contents()`, `tools_tab_contents()`, `active_tool_tab_contents()`,
  `hotkeys_tab_contents()`: dock tab bodies.

Notes:
The frame order documented on `draw` is load-bearing; see its declaration comment.
A child of the crate root (`lib.rs`); reaches the root's types, constants and imports through
`use super::*`.
*/

use super::*;

impl PsEditorTabState {
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
        let _frame = ms_log::trace_scope!(cat::FRAME, "ps_draw page={:?}", self.active_page_idx);
        self.ensure_loader();
        self.poll_loader();
        // Consume every TOOL-owned worker channel, next to the loader poll and for the same reason:
        // a tool that computes anything expensive must never block `interact` (`CLAUDE.md` §5), so
        // its results arrive on a channel and are drained here. The pixel writes such a result asks
        // for are queued as `PsToolAction`s and applied by `apply_tool_actions` below.
        if self.poll_tools() {
            ctx.request_repaint();
        }
        // Consume any finished non-destructive raster-effects render (computed off the GUI thread).
        if self.poll_ps_raster_effects_jobs(project) {
            ctx.request_repaint();
        }
        self.refresh_view_if_doc_version_changed();
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
        // The active tool's panel is resolved BEFORE `cx` takes `&mut self`, for the same reason
        // the visibility flags are read before the tabs are declared: the `visible` flag and the
        // caption must both be this frame's answer, and neither can be asked for once the tool list
        // is inside the borrow the dock frame holds.
        let tool_panel_visible = tool_panel_visible(&self.tools, self.active_tool_idx);
        let tool_panel_title = self.active_tool_title();

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
        // The five secondary tabs are declared on EVERY frame regardless of `visible`: a hidden tab
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
        dock.tab(PS_EDITOR_CORRECTION_TAB)
            .title(|| t!("ps_editor.tab.correction"))
            .visible(visibility.correction)
            .min_size(PS_EDITOR_CORRECTION_TAB_MIN_SIZE_PX)
            .initial_size(PS_EDITOR_CORRECTION_TAB_INITIAL_SIZE_PX)
            .show(draw_correction_tab_body);
        // The ACTIVE TOOL's own panel. Declared on every frame like the five above, but its
        // `visible` comes from the TOOL (`wants_main_panel`) rather than from a stored user flag,
        // and its caption is the tool's own title — the panel belongs to a tool, so its name must
        // say which one.
        dock.tab(PS_EDITOR_TOOL_PANEL_TAB)
            .title(move || tool_panel_title)
            .visible(tool_panel_visible)
            .min_size(PS_EDITOR_TOOL_PANEL_TAB_MIN_SIZE_PX)
            .initial_size(PS_EDITOR_TOOL_PANEL_TAB_INITIAL_SIZE_PX)
            .show(draw_tool_panel_tab_body);
        // MAIN-WINDOW panels only, by construction: `drawn_panels` never reports a panel the user
        // detached into a sub-window, whose rect lives in that window's own frame and would carve a
        // dead zone out of this window's top-left corner (`PanelDockOutput`).
        let out = dock.end(&mut cx);
        let panel_rects: Vec<Rect> = out.drawn_panels().map(|(_, rect)| rect).collect();
        let panel_actions = std::mem::take(&mut cx.panel_actions);

        // Hand this frame's panel rects to the active tool BEFORE they are stored, so a tool that
        // places a surface on the canvas can cut the floating panels out of the viewport. Read from
        // the local `panel_rects` rather than from `self.panel_rects`, which the `self.tools`
        // borrow below would conflict with.
        if let Some(tool) = self.tools.get_mut(self.active_tool_idx) {
            tool.set_panel_rects(&panel_rects);
        }
        self.panel_rects = panel_rects;
        self.apply_panel_actions(panel_actions, project);
        self.draw_effects_editor(ctx);
        self.draw_canvas(ctx, ui, area_rect, project);
        // The ONE drain point for tool-queued pixel writes, and deliberately AFTER `draw_canvas`:
        // that is the only site reached on every frame, because `draw_canvas` returns early while
        // no page is resident and a queue drained only there would grow without bound. The cost is
        // one frame of latency on a commit that is asynchronous to begin with (a worker result, or
        // a gesture that has already ended), so nothing the user is currently dragging is delayed.
        if self.apply_tool_actions(project) {
            ctx.request_repaint();
        }
    }

    /// Polls every tool's own worker channels, whether or not it is the active tool.
    ///
    /// Called once per frame from [`PsEditorTabState::draw`], next to `poll_loader`. Inactive tools
    /// are polled too: a job dispatched before a tool switch still finishes, and its result must be
    /// consumed rather than stranded — the same reason the loader poll is unconditional.
    ///
    /// Returns `true` when at least one tool wants the next frame (a job in flight, or a poll that
    /// changed something visible); the caller turns that into a repaint request, which is what
    /// makes an off-thread result appear without any pointer movement.
    pub(crate) fn poll_tools(&mut self) -> bool {
        let mut repaint = false;
        for tool in &mut self.tools {
            repaint |= tool.poll_workers();
        }
        repaint
    }

    /// Localized title of the active tool, used as the caption of its own dock panel.
    #[must_use]
    pub(crate) fn active_tool_title(&self) -> &'static str {
        self.tools
            .get(self.active_tool_idx)
            .map_or("", |tool| tool.title())
    }

    /// Everything the «PS редактор» tab shows except its «Панели…» menu, in two wrapped rows: the
    /// page switch, the two zoom presets, the zoom readout, the load / effects spinners and the last
    /// load error; then the two view toggles («Сглаживание», «Сетка пикселей»).
    ///
    /// Each row WRAPS: the dock body scrolls both axes, and a panel the user narrows must break the
    /// row rather than push the buttons behind a horizontal scrollbar.
    pub(crate) fn main_tab_contents(&mut self, ui: &mut egui::Ui, project: &ProjectData) {
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
                ui.colored_label(ms_theme::status::ERROR, err);
            }
        });

        // Second row: the manual counterparts of what the cleaning tab switches on automatically at
        // high zoom. No `id_salt` is needed despite the localized labels — `Checkbox` takes its `Id`
        // from `Ui::next_auto_id`, not from its text (`egui-0.35.0/src/widgets/checkbox.rs:72`), so
        // the rule of `egui-docs/05-ids-and-i18n.md` §2 does not reach it. The checkboxes are added
        // straight to the wrapping row: a `push_id` scope lays out as one atomic rect and could not
        // break onto a second line (`egui-0.35.0/src/placer.rs:165`).
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(
                &mut self.smoothing_enabled,
                t!("ps_editor.top_bar.smoothing_checkbox"),
            )
            .on_hover_text(t!("ps_editor.top_bar.smoothing_tooltip"));
            ui.checkbox(
                &mut self.pixel_grid_enabled,
                t!("ps_editor.top_bar.pixel_grid_checkbox"),
            )
            .on_hover_text(t!("ps_editor.top_bar.pixel_grid_tooltip"));
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
    pub(crate) fn tools_tab_contents(&mut self, ui: &mut egui::Ui, project: &ProjectData) {
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
                    self.set_active_tool(index, project);
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
    pub(crate) fn active_tool_tab_contents(&mut self, ui: &mut egui::Ui) {
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
    pub(crate) fn hotkeys_tab_contents(&mut self, ui: &mut egui::Ui) {
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
}
