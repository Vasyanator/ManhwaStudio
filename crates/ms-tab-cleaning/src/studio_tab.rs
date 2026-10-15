/*
File: crates/ms-tab-cleaning/src/studio_tab.rs

Purpose:
The «Клининг» tab's answers to the studio root's order-free per-tab hooks
(`ms_studio_tab::StudioTab`).

Key items:
- `run_fired_commands`: executes the shared `ms_input::view` zoom commands (zoom in, zoom out,
  reset) that fired this frame, through the tab's own zoom methods, which refuse while the
  active tool blocks canvas zoom. Called at the top of `CleaningTabState::draw`.
- `impl StudioTab for CleaningTabState`:
  - `input_contexts`: `ms_studio_tab::VIEW_ZOOM_COMMANDS` while no active tool blocks canvas
    zoom, so the router fires (and consumes) the zoom keys only while this tab executes them
    and a tool with its own zoom keys (the region editor) still gets them (removed by WP5);
  - `default_dock_layout`: `cleaning_default_dock_layout` (the tab hosts the panel dock);
  - `evict_tab_gpu_caches`: the canvas clean-overlay cache, then the detector-mask textures;
  - `source_page_window` / `wants_nearest_source`: answered by the tab's canvas;
  - `on_committed_tree_changed`: `request_orphan_clean_rescan`.

Notes:
- No background poll, exit-only setting or debounced store: those hooks keep their no-op
  defaults. Cleaning has no bottom hint, so it writes no `Canvas.*_hint_collapsed` key.
- The tab declares no input action of its own yet: zoom is the shared `view.*` set (its former
  `cleaning.canvas.zoom_*` overrides migrate to it in `ms_input::persist`).
*/

use std::collections::HashSet;

use ms_input::{ActionId, ContextId, InputFrame, view};
use ms_memory::{CacheEvictionReport, CacheEvictionRequest};
use ms_studio_tab::{StudioTab, VIEW_ZOOM_COMMANDS};
use ms_widgets::panel_dock::DockLayout;

use crate::tab::{CleaningTabState, cleaning_default_dock_layout};

/// Zoom factor of one zoom-in shortcut step; zoom out divides by it.
const ZOOM_SHORTCUT_STEP: f32 = 1.1;

impl StudioTab for CleaningTabState {
    /// `VIEW_ZOOM_COMMANDS` while no active tool blocks canvas zoom: only then does this tab
    /// execute the shared zoom commands (`run_fired_commands`), and only then may the router
    /// consume their keys.
    fn input_contexts(&self) -> Vec<ContextId> {
        if self.active_tool_blocks_canvas_zoom() { Vec::new() } else { vec![VIEW_ZOOM_COMMANDS] }
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

/// Runs every shared zoom command `input` reports as fired (`run_command`), in firing order.
pub(crate) fn run_fired_commands(tab: &mut CleaningTabState, input: &InputFrame) {
    for fired in input.fired_commands() {
        run_command(tab, fired.action);
    }
}

/// Runs the command `action` on `tab`. Returns `true` when the id is one this tab executes, even
/// when the active tool blocked the zoom (the command was handled by being refused), and
/// `false` for a foreign id (nothing was done).
fn run_command(tab: &mut CleaningTabState, action: ActionId) -> bool {
    // The zoom methods report whether the camera moved; a blocked zoom is a normal outcome the
    // command has nothing to add to.
    match action {
        view::ZOOM_IN => {
            tab.zoom_by_shortcut(ZOOM_SHORTCUT_STEP);
        }
        view::ZOOM_OUT => {
            tab.zoom_by_shortcut(1.0 / ZOOM_SHORTCUT_STEP);
        }
        view::ZOOM_RESET => {
            tab.reset_zoom_shortcut();
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use ms_input::{ActionId, view};
    use ms_studio_tab::{StudioTab, VIEW_ZOOM_COMMANDS};
    use ms_widgets::panel_dock::TabId;

    use super::run_command;
    use crate::tab::{CleaningTabState, cleaning_default_dock_layout};

    /// The tab declares no action of its own (zoom is the shared `view.*` set), and with no tool
    /// blocking zoom it publishes the context under which the router fires the zoom commands.
    #[test]
    fn the_tab_declares_nothing_and_publishes_the_zoom_context() {
        let tab = CleaningTabState::default();
        assert_eq!(StudioTab::input_declarations(&tab), ms_input::Declarations::default());
        assert_eq!(StudioTab::input_contexts(&tab), vec![VIEW_ZOOM_COMMANDS]);
    }

    /// The three shared zoom commands are handled; a foreign id (including the migrated former
    /// Cleaning zoom ids) is reported as not this tab's.
    #[test]
    fn run_command_claims_only_the_zoom_commands() {
        let mut tab = CleaningTabState::default();
        for id in [view::ZOOM_IN, view::ZOOM_OUT, view::ZOOM_RESET] {
            assert!(run_command(&mut tab, id), "{id:?} must be handled");
        }
        for foreign in ["cleaning.canvas.zoom_in", "view.zoom", "translation.bubble.create_at_cursor"] {
            assert!(!run_command(&mut tab, ActionId(foreign)), "{foreign}");
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
