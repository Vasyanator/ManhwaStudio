/*
File: crates/ms-tab-cleaning/src/studio_tab.rs

Purpose:
The «Клининг» tab's answers to the studio root's order-free per-tab hooks
(`ms_studio_tab::StudioTab`).

Key items:
- `impl StudioTab for CleaningTabState`:
  - `input_declarations`: `crate::input::declarations()` — the clean-overlay history, the tool
    modes / gestures / axes / commands, the region frame and the legacy editor window;
  - `input_contexts`: the canvas's contexts (`CanvasView::input_contexts`: none, the cleaning
    canvas does not edit bubbles) plus the active tool's (`CleaningTool::input_contexts`);
  - `default_dock_layout`: `cleaning_default_dock_layout` (the tab hosts the panel dock);
  - `evict_tab_gpu_caches`: the canvas clean-overlay cache, then the detector-mask textures;
  - `source_page_window` / `wants_nearest_source`: answered by the tab's canvas;
  - `on_committed_tree_changed`: `request_orphan_clean_rescan`;
  - `poll_background`: the text-mask load and quick text clean polls
    (`CleaningTabState::poll_background_jobs`), with a slow repaint while one runs.

Notes:
- No exit-only setting or debounced store: those hooks keep their no-op defaults. The region-edit
  tools' jobs are polled only while the tab draws (their poll needs the canvas). Cleaning has
  no bottom hint, so it writes no `Canvas.*_hint_collapsed` key.
- Zoom is the shared `view.*` set, executed by the canvas or, over an open legacy editor window,
  by that window (its former `cleaning.canvas.zoom_*` overrides migrate to `view.*` in
  `ms_input::persist`).
*/

use std::collections::HashSet;

use eframe::egui;
use ms_input::{ContextId, Declarations};
use ms_memory::{CacheEvictionReport, CacheEvictionRequest};
use ms_project::ProjectData;
use ms_studio_tab::StudioTab;
use ms_widgets::panel_dock::DockLayout;

use crate::tab::{CleaningTabState, cleaning_default_dock_layout};

/// How often `poll_background` re-polls while a background job runs and the tab may not be
/// drawn.
const BACKGROUND_JOB_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

impl StudioTab for CleaningTabState {
    /// Every action of the tab, its tools, the region frame and the legacy editor window
    /// (`crate::input`).
    fn input_declarations(&self) -> Declarations {
        crate::input::declarations()
    }

    /// The canvas's contexts (none: the cleaning canvas does not edit bubbles), then the active
    /// tool's.
    fn input_contexts(&self) -> Vec<ContextId> {
        let mut contexts = self.canvas().input_contexts();
        contexts.extend(self.active_tool_input_contexts());
        contexts
    }

    /// The cleaning tab hosts the panel dock; its default arrangement is the SAME `fn` item the
    /// tab hands to `ensure_default_layout` every frame.
    fn default_dock_layout(&self) -> Option<fn() -> DockLayout> {
        Some(cleaning_default_dock_layout)
    }

    /// The canvas clean-overlay textures first, then the detector-mask textures; one merged report.
    fn evict_tab_gpu_caches(&mut self, request: &CacheEvictionRequest) -> CacheEvictionReport {
        let mut report = self.evict_clean_overlay_gpu_cache(request);
        report.append(self.evict_cleaning_mask_gpu_cache(request));
        report
    }

    /// The visible pages of the tab's canvas plus `neighbor_radius` on each side.
    fn source_page_window(&self, neighbor_radius: usize) -> HashSet<usize> {
        self.canvas().active_source_page_window(neighbor_radius)
    }

    /// The canvas zoomed into pixel inspection draws NEAREST source tiles.
    fn wants_nearest_source(&self) -> bool {
        self.canvas().source_pixel_inspection_active()
    }

    /// Drains the text-mask load and the quick text clean while another tab is active
    /// (`CleaningTabState::poll_background_jobs`), so their model writes are not held back until
    /// the user returns; keeps a slow repaint going while one runs, because its worker wakes no
    /// frame (the drawn tab repaints every frame meanwhile anyway).
    fn poll_background(&mut self, ctx: &egui::Context, _project: &ProjectData) {
        if self.poll_background_jobs() {
            ctx.request_repaint_after(BACKGROUND_JOB_POLL_INTERVAL);
        }
    }

    /// A save-to-project merge rewrote the committed clean folders: rescan the «Клин» status area.
    fn on_committed_tree_changed(&mut self) {
        self.request_orphan_clean_rescan();
    }
}

#[cfg(test)]
mod tests {
    use ms_studio_tab::StudioTab;
    use ms_widgets::panel_dock::TabId;

    use crate::tab::{CleaningTabState, cleaning_default_dock_layout};

    /// The tab declares the cleaning set and forwards the canvas's contexts (none) plus the active
    /// tool's contexts (Замазка first: the brush tool, Замазка, the shared brush).
    #[test]
    fn the_tab_declares_its_set_and_forwards_canvas_and_tool_contexts() {
        let tab = CleaningTabState::default();
        assert_eq!(StudioTab::input_declarations(&tab), crate::input::declarations());
        assert_eq!(
            StudioTab::input_contexts(&tab),
            vec![crate::input::TOOL_BRUSH, crate::input::TOOL_ZAMAZKA, ms_tools::mask_brush::BRUSH_ACTIVE]
        );
    }

    /// Every registered tool publishes exactly one owner state of the canvas (the exclusive
    /// group of `crate::input::declarations`) in its idle state.
    #[test]
    fn every_tool_publishes_one_canvas_owner_context() {
        let owners = [crate::input::TOOL_BRUSH, crate::input::TOOL_PATCH, crate::input::REGION_SELECT, crate::input::REGION_FRAME, crate::input::EDITOR_WINDOW];
        let tab = CleaningTabState::default();
        for (idx, contexts) in tab.tool_input_contexts_for_tests().into_iter().enumerate() {
            let published = contexts.iter().filter(|context| owners.contains(context)).count();
            assert_eq!(published, 1, "tool {idx}: {contexts:?}");
        }
    }

    /// Every dock tab the cleaning tab declares; a default layout missing one drops a stored
    /// arrangement of that tab on every load (`panel_dock/persist.rs::known_tabs`).
    const CLEANING_TABS: &[TabId] = &[
        ms_canvas::CANVAS_RIBBON_TAB,
        TabId::new("cleaning.clean"),
        TabId::new("cleaning.tools"),
        TabId::new("cleaning.active_tool"),
        TabId::new("cleaning.quick_clean"),
        TabId::new("cleaning.area_editor"),
    ];

    /// The hook registers the very builder the tab passes to `ensure_default_layout`, and its
    /// layout names every declared dock tab (the semantic backstop, since function-pointer
    /// equality is not guaranteed across codegen-unit merging).
    #[test]
    fn the_dock_hook_registers_the_builder_the_tab_declares() {
        let tab = CleaningTabState::default();
        let tab: &dyn StudioTab = &tab;
        let builder = tab.default_dock_layout().expect("the cleaning tab hosts the panel dock");
        assert!(
            std::ptr::fn_addr_eq(builder, cleaning_default_dock_layout as fn() -> _),
            "the cleaning tab registers a builder other than the one it declares"
        );
        let layout = builder();
        for id in CLEANING_TABS {
            assert!(
                layout.panel_of_tab(*id).is_some(),
                "the cleaning default layout does not name {id}, so a stored {id} would be \
                 dropped on every load"
            );
        }
    }
}
