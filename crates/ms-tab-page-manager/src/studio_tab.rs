/*
File: crates/ms-tab-page-manager/src/studio_tab.rs

Purpose:
The page manager's answers to the studio root's order-free per-tab hooks
(`ms_studio_tab::StudioTab`).

Key items:
- `impl StudioTab for PageManagerTabState`:
  - `poll_background`: `poll_clean_events` then `release_hidden_viewer_clean`, every frame
    whatever tab is active (before the root's busy gates read `clean_op_in_flight`);
  - `source_page_window`: the one page the viewer draws from the app's lent textures;
  - `wants_nearest_source`: the viewer zoomed past its NEAREST threshold;
  - `on_committed_tree_changed`: `request_clean_rescan` (a save-to-project merge rewrote the
    committed `clean_layers/` and removed the staged paths its clean cards show).

Notes:
- Not a panel-dock host, no hotkeys, no tab-owned GPU cache in the root's pressure eviction
  (the viewer frees its own clean textures in `release_hidden_viewer_clean`), no exit-only
  settings or debounced stores: those hooks keep their no-op defaults.
- `poll_background` needs no "is active" flag: `release_hidden_viewer_clean` works out "hidden"
  from the viewer's own `last_drawn_frame` stamp.
*/

use std::collections::HashSet;

use eframe::egui;
use ms_project::ProjectData;
use ms_studio_tab::StudioTab;

use crate::PageManagerTabState;

impl StudioTab for PageManagerTabState {
    /// The page viewer draws exactly ONE source page from the app's lent textures, so its window
    /// is that page alone (the radius does not apply): the per-frame trim neither drops nor
    /// thrashes it.
    fn source_page_window(&self, _neighbor_radius: usize) -> HashSet<usize> {
        self.viewer_source_page().into_iter().collect()
    }

    /// The viewer zoomed past its NEAREST threshold draws the page's nearest tiles.
    fn wants_nearest_source(&self) -> bool {
        self.viewer_wants_nearest_source()
    }

    /// Drains clean-worker replies (so `clean_op_in_flight` never goes stale off-tab), then frees
    /// the viewer's clean tiles and textures while this tab is not drawn. Same order as before
    /// the trait existed.
    fn poll_background(&mut self, ctx: &egui::Context, project: &ProjectData) {
        self.poll_clean_events(ctx, project);
        self.release_hidden_viewer_clean(ctx);
    }

    /// The committed chapter tree changed: the clean cards are rescanned on the next scan pass.
    fn on_committed_tree_changed(&mut self) {
        self.request_clean_rescan();
    }
}

#[cfg(test)]
mod tests {
    use ms_studio_tab::StudioTab;

    use crate::PageManagerTabState;

    /// Without an open viewer the tab needs no source page and no NEAREST sampling, and it does
    /// not host the panel dock.
    #[test]
    fn a_closed_viewer_needs_no_source_pages() {
        let tab = PageManagerTabState::default();
        let tab: &dyn StudioTab = &tab;
        assert!(tab.source_page_window(3).is_empty());
        assert!(!tab.wants_nearest_source());
        assert!(tab.default_dock_layout().is_none());
        assert!(tab.hotkey_specs().is_empty());
    }

    /// A committed-tree change schedules exactly the clean rescan `request_clean_rescan` does.
    #[test]
    fn a_committed_tree_change_requests_a_clean_rescan() {
        let mut tab = PageManagerTabState::default();
        let before = tab.clean_scan_epoch;
        StudioTab::on_committed_tree_changed(&mut tab);
        assert_eq!(tab.clean_scan_epoch, before.wrapping_add(1));
    }
}
