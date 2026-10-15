/*
File: crates/ms-tab-cleaning/src/studio_tab.rs

Purpose:
The «Клининг» tab's answers to the studio root's order-free per-tab hooks
(`ms_studio_tab::StudioTab`).

Key items:
- `impl StudioTab for CleaningTabState`:
  - `input_contexts`: the canvas's contexts (`CanvasView::input_contexts`): `canvas.zoom_accepted`
    while the active tool does not block canvas zoom (the tab sets that block on the canvas every
    frame), so the shared zoom keys and Ctrl+wheel reach a tool with its own zoom (the region
    editor window) while it does;
  - `default_dock_layout`: `cleaning_default_dock_layout` (the tab hosts the panel dock);
  - `evict_tab_gpu_caches`: the canvas clean-overlay cache, then the detector-mask textures;
  - `source_page_window` / `wants_nearest_source`: answered by the tab's canvas;
  - `on_committed_tree_changed`: `request_orphan_clean_rescan`.

Notes:
- No background poll, exit-only setting or debounced store: those hooks keep their no-op
  defaults. Cleaning has no bottom hint, so it writes no `Canvas.*_hint_collapsed` key.
- The tab declares no input action of its own yet: zoom is the shared `view.*` set, executed by
  the canvas (its former `cleaning.canvas.zoom_*` overrides migrate to it in `ms_input::persist`).
*/

use std::collections::HashSet;

use ms_input::ContextId;
use ms_memory::{CacheEvictionReport, CacheEvictionRequest};
use ms_studio_tab::StudioTab;
use ms_widgets::panel_dock::DockLayout;

use crate::tab::{CleaningTabState, cleaning_default_dock_layout};

impl StudioTab for CleaningTabState {
    /// The canvas's contexts: `canvas.zoom_accepted` while the active tool does not block canvas
    /// zoom (the router then consumes the zoom keys and Ctrl+wheel for the canvas); the cleaning
    /// canvas does not edit bubbles.
    fn input_contexts(&self) -> Vec<ContextId> {
        self.canvas().input_contexts()
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

    /// The tab declares no action of its own (zoom is the shared `view.*` set, executed by the
    /// canvas), and with no tool blocking zoom it forwards the canvas's zoom context.
    #[test]
    fn the_tab_declares_nothing_and_forwards_the_canvas_contexts() {
        let tab = CleaningTabState::default();
        assert_eq!(StudioTab::input_declarations(&tab), ms_input::Declarations::default());
        assert_eq!(StudioTab::input_contexts(&tab), vec![ms_canvas::input::ZOOM_ACCEPTED]);
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
