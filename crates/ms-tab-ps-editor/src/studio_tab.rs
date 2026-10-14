/*
File: crates/ms-tab-ps-editor/src/studio_tab.rs

Purpose:
The PS editor's answers to the studio root's order-free per-tab hooks
(`ms_studio_tab::StudioTab`).

Key items:
- `impl StudioTab for PsEditorTabState`: only `default_dock_layout` (the tab hosts the panel
  dock with `ps_editor_default_dock_layout`).

Notes:
- Every other hook keeps its no-op default on purpose: the editor owns its page residency
  through `CleanOverlaysModel`'s page cache (no shared source-page window, no NEAREST source
  sampling), its tiled GPU cache is not part of the root's pressure eviction, and it has no
  background poll, exit-only setting, debounced store or committed-tree view.
- Its hotkeys stay outside the trait: `handle_hotkeys` runs from the root's draw arm (an
  ordered per-frame step, not an order-free hook).
*/

use ms_studio_tab::StudioTab;
use ms_widgets::panel_dock::DockLayout;

use crate::{PsEditorTabState, ps_editor_default_dock_layout};

impl StudioTab for PsEditorTabState {
    /// The PS editor hosts the panel dock; its default arrangement is the SAME `fn` item the
    /// tab hands to `ensure_default_layout` every frame (the dock keeps that pointer).
    fn default_dock_layout(&self) -> Option<fn() -> DockLayout> {
        Some(ps_editor_default_dock_layout)
    }
}

#[cfg(test)]
mod tests {
    use ms_studio_tab::StudioTab;
    use ms_widgets::panel_dock::TabId;

    use crate::{PsEditorTabState, ps_editor_default_dock_layout};

    /// Every dock tab the editor declares; a default layout that misses one drops a stored
    /// arrangement of that tab on every load (`panel_dock/persist.rs::known_tabs`).
    const PS_EDITOR_TABS: &[TabId] = &[
        TabId::new("ps_editor.main"),
        TabId::new("ps_editor.tools"),
        TabId::new("ps_editor.active_tool"),
        TabId::new("ps_editor.hotkeys"),
        TabId::new("ps_editor.layers"),
        TabId::new("ps_editor.correction"),
    ];

    /// The builder the hook registers must be the very `fn` the tab passes to
    /// `ensure_default_layout` (identity, which the «Сбросить раскладку» item relies on), and
    /// its layout must name every tab the editor declares (the semantic backstop, since
    /// function-pointer equality is not guaranteed across codegen-unit merging).
    #[test]
    fn the_dock_hook_registers_the_builder_the_tab_declares() {
        let tab = PsEditorTabState::default();
        let tab: &dyn StudioTab = &tab;
        let builder = tab.default_dock_layout().expect("the PS editor hosts the panel dock");
        assert!(
            std::ptr::fn_addr_eq(builder, ps_editor_default_dock_layout as fn() -> _),
            "the PS editor registers a builder other than the one it declares"
        );
        let layout = builder();
        for id in PS_EDITOR_TABS {
            assert!(
                layout.panel_of_tab(*id).is_some(),
                "the PS editor's default layout does not name {id}, so a stored {id} would be \
                 dropped on every load"
            );
        }
    }

    /// The editor has no shared source-page window and never asks for NEAREST source sampling.
    #[test]
    fn the_editor_needs_no_shared_source_pages() {
        let tab = PsEditorTabState::default();
        assert!(tab.source_page_window(3).is_empty());
        assert!(!tab.wants_nearest_source());
    }
}
