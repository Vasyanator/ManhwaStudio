/*
FILE OVERVIEW: crates/ms-studio-tab/src/lib.rs
Crate root of `ms-studio-tab`: the `StudioTab` trait, the per-tab hook surface the studio root
(`src/studio/tabs.rs`, `Tabs`) iterates instead of matching on `AppTab` at every call site.

Key items:
- `StudioTab`: order-free hooks, each with a no-op default, so a tab implements only what it has.

Notes:
- Only ORDER-FREE hooks belong here: the root may call them for every tab in `AppTab::ALL`
  order and the result must not depend on that order. Drawing and every ordered §12 sequence
  (staging writers, save, page ops, discard, shutdown barriers) stay explicit in the root.
- The trait must stay object safe: the root holds `&dyn StudioTab` / `&mut dyn StudioTab`.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]

use std::collections::HashSet;

use ms_input::{ContextId, Declarations};
use ms_memory::{CacheEvictionReport, CacheEvictionRequest};
use ms_project::ProjectData;
use ms_widgets::panel_dock::DockLayout;

/// Order-free per-tab hooks the studio root iterates (`Tabs`). Every method has a no-op default.
///
/// Contract for every implementation: it never touches the staging writers, never reads another
/// tab, never blocks (except [`StudioTab::flush_debounced_stores_blocking`]) and does no I/O
/// outside that method. Hooks are called on the GUI thread.
pub trait StudioTab {
    /// The input actions this tab declares for the studio's `ms_input::InputRegistry` (merged in
    /// `AppTab::ALL` order with the shared `view.*` set and the session's own). Pure and stable:
    /// called once when the studio is built. Action ids are persistence keys of the user's
    /// overrides (`user_config` -> `Hotkeys`) and must never be renamed. A tab reads what fired
    /// from the `ms_input::InputFrame` it is handed for its draw; there is no execute hook.
    #[must_use]
    fn input_declarations(&self) -> Declarations {
        Declarations::default()
    }

    /// The input contexts this tab publishes this frame while it is the active tab (a tool, an
    /// edit mode, an open window). Pure, cheap, called once per frame before the input router
    /// runs; actions declaring `Condition::Context(..)` are active only while it is listed.
    #[must_use]
    fn input_contexts(&self) -> Vec<ContextId> {
        Vec::new()
    }

    /// The default-layout builder this tab passes to `PanelDockState::ensure_default_layout`.
    /// `Some` means this tab hosts the panel dock: the single answer to "is a dock host".
    #[must_use]
    fn default_dock_layout(&self) -> Option<fn() -> DockLayout> {
        None
    }

    /// Frees this tab's own GPU caches under `request` and reports what it dropped. Named apart
    /// from the typing tab's inherent `evict_gpu_caches`, which its implementation calls.
    fn evict_tab_gpu_caches(&mut self, _request: &CacheEvictionRequest) -> CacheEvictionReport {
        CacheEvictionReport::default()
    }

    /// Source-page indices this tab needs resident while it is active; `neighbor_radius` pages
    /// on each side of the visible ones. Empty = this tab needs no source pages.
    #[must_use]
    fn source_page_window(&self, _neighbor_radius: usize) -> HashSet<usize> {
        HashSet::new()
    }

    /// Whether this tab, while active, draws source pages with NEAREST sampling (pixel inspection).
    #[must_use]
    fn wants_nearest_source(&self) -> bool {
        false
    }

    /// Called every frame whatever tab is active, at the root's fixed point (before the busy
    /// gates are read). Must only poll its own background work; never blocks.
    fn poll_background(&mut self, _ctx: &egui::Context, _project: &ProjectData) {}

    /// Writes this tab's exit-only `user_config` keys into `root`. Pure: the root owns the one
    /// read-modify-write that runs every tab's writer, so no I/O happens here.
    fn write_exit_settings(&self, _root: &mut serde_json::Value) {}

    /// Teardown only: synchronously flushes this tab's debounced user-global stores and logs
    /// what it flushed. The one hook allowed to block.
    fn flush_debounced_stores_blocking(&mut self) {}

    /// The committed chapter tree changed (a save-to-project merge succeeded); a tab that caches
    /// a view of the committed tree schedules its rescan here.
    fn on_committed_tree_changed(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::StudioTab;

    /// A tab with no hooks: every default must be the documented no-op.
    struct Bare;

    impl StudioTab for Bare {}

    #[test]
    fn the_trait_is_object_safe_and_defaults_are_no_ops() {
        let mut bare = Bare;
        let tab: &mut dyn StudioTab = &mut bare;
        assert_eq!(tab.input_declarations(), ms_input::Declarations::default());
        assert_eq!(tab.input_contexts(), [] as [ms_input::ContextId; 0]);
        assert!(tab.default_dock_layout().is_none());
        assert!(tab.source_page_window(3).is_empty());
        assert!(!tab.wants_nearest_source());
        let mut root = serde_json::json!({ "Canvas": { "cache_pages": true } });
        tab.write_exit_settings(&mut root);
        assert_eq!(root, serde_json::json!({ "Canvas": { "cache_pages": true } }));
        let request = ms_memory::CacheEvictionRequest {
            profile: ms_memory::MemoryProfile::Medium,
            pressure: ms_memory::MemoryPressure::Critical,
            target_free_bytes: u64::MAX,
            pinned_pages: std::collections::BTreeSet::new(),
        };
        assert_eq!(tab.evict_tab_gpu_caches(&request), ms_memory::CacheEvictionReport::default());
        tab.flush_debounced_stores_blocking();
        tab.on_committed_tree_changed();
        let shared: &dyn StudioTab = &Bare;
        assert_eq!(shared.input_contexts(), [] as [ms_input::ContextId; 0]);
    }
}
