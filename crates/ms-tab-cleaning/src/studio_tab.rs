/*
File: crates/ms-tab-cleaning/src/studio_tab.rs

Purpose:
The «Клининг» tab's answers to the studio root's order-free per-tab hooks
(`ms_studio_tab::StudioTab`).

Key items:
- `HOTKEY_CLEANING_ZOOM_IN` / `_OUT` / `_RESET`: the tab's three canvas-zoom hotkey ids. They are
  persistence keys (`user_config` -> `Hotkeys`): never rename them.
- `impl StudioTab for CleaningTabState`:
  - `hotkey_specs` / `execute_hotkey`: the three zoom hotkeys (zoom in, zoom out, reset, in that
    registration order); execution goes through the tab's own zoom methods, which refuse while
    the active tool blocks canvas zoom;
  - `default_dock_layout`: `cleaning_default_dock_layout` (the tab hosts the panel dock);
  - `evict_tab_gpu_caches`: the canvas clean-overlay cache, then the detector-mask textures;
  - `source_page_window` / `wants_nearest_source`: answered by the tab's canvas;
  - `on_committed_tree_changed`: `request_orphan_clean_rescan`.

Notes:
- No background poll, exit-only setting or debounced store: those hooks keep their no-op
  defaults. Cleaning has no bottom hint, so it writes no `Canvas.*_hint_collapsed` key.
*/

use std::collections::HashSet;

use eframe::egui;
use ms_config::app_tab::AppTab;
use ms_memory::{CacheEvictionReport, CacheEvictionRequest};
use ms_project::ProjectData;
use ms_studio_tab::StudioTab;
use ms_widgets::input_manager_v2::{HotkeyScopeV2, HotkeySpecV2};
use ms_widgets::panel_dock::DockLayout;

use crate::tab::{CleaningTabState, cleaning_default_dock_layout};

/// Zooms the cleaning canvas in by one shortcut step (persistence key, never rename).
const HOTKEY_CLEANING_ZOOM_IN: &str = "cleaning.canvas.zoom_in";
/// Zooms the cleaning canvas out by one shortcut step (persistence key, never rename).
const HOTKEY_CLEANING_ZOOM_OUT: &str = "cleaning.canvas.zoom_out";
/// Resets the cleaning canvas zoom (persistence key, never rename).
const HOTKEY_CLEANING_ZOOM_RESET: &str = "cleaning.canvas.zoom_reset";

/// Zoom factor of one zoom-in shortcut step; zoom out divides by it.
const ZOOM_SHORTCUT_STEP: f32 = 1.1;

impl StudioTab for CleaningTabState {
    /// The three canvas-zoom hotkeys, in registration order: zoom in, zoom out, reset.
    fn hotkey_specs(&self) -> Vec<HotkeySpecV2> {
        vec![
            HotkeySpecV2 {
                id: HOTKEY_CLEANING_ZOOM_IN,
                title: t!("app.hotkey.zoom_in"),
                section: "Canvas",
                default_shortcut: Some(egui::KeyboardShortcut::new(
                    egui::Modifiers::COMMAND,
                    egui::Key::Equals,
                )),
                default_modifier_only: None,
                scope: HotkeyScopeV2::Tab(AppTab::Cleaning),
                active_when_input: false,
            },
            HotkeySpecV2 {
                id: HOTKEY_CLEANING_ZOOM_OUT,
                title: t!("app.hotkey.zoom_out"),
                section: "Canvas",
                default_shortcut: Some(egui::KeyboardShortcut::new(
                    egui::Modifiers::COMMAND,
                    egui::Key::Minus,
                )),
                default_modifier_only: None,
                scope: HotkeyScopeV2::Tab(AppTab::Cleaning),
                active_when_input: false,
            },
            HotkeySpecV2 {
                id: HOTKEY_CLEANING_ZOOM_RESET,
                title: t!("app.hotkey.zoom_reset"),
                section: "Canvas",
                default_shortcut: Some(egui::KeyboardShortcut::new(
                    egui::Modifiers::COMMAND,
                    egui::Key::Num0,
                )),
                default_modifier_only: None,
                scope: HotkeyScopeV2::Tab(AppTab::Cleaning),
                active_when_input: false,
            },
        ]
    }

    /// Runs one of the three zoom hotkeys (see [`run_zoom_hotkey`]).
    fn execute_hotkey(&mut self, _ctx: &egui::Context, command_id: &str, _project: &ProjectData) -> bool {
        run_zoom_hotkey(self, command_id)
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

/// Runs the zoom hotkey `command_id` on `tab`. Returns `true` when the id is one of this tab's,
/// even when the active tool blocked the zoom (the command was handled by being refused), and
/// `false` for a foreign id (nothing was done).
fn run_zoom_hotkey(tab: &mut CleaningTabState, command_id: &str) -> bool {
    // The zoom methods report whether the camera moved; a blocked zoom is a normal outcome the
    // hotkey has nothing to add to, exactly as when the root dispatched it.
    match command_id {
        HOTKEY_CLEANING_ZOOM_IN => {
            tab.zoom_by_shortcut(ZOOM_SHORTCUT_STEP);
        }
        HOTKEY_CLEANING_ZOOM_OUT => {
            tab.zoom_by_shortcut(1.0 / ZOOM_SHORTCUT_STEP);
        }
        HOTKEY_CLEANING_ZOOM_RESET => {
            tab.reset_zoom_shortcut();
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use eframe::egui;
    use ms_config::app_tab::AppTab;
    use ms_studio_tab::StudioTab;
    use ms_widgets::input_manager_v2::HotkeyScopeV2;
    use ms_widgets::panel_dock::TabId;

    use super::{HOTKEY_CLEANING_ZOOM_IN, HOTKEY_CLEANING_ZOOM_OUT, HOTKEY_CLEANING_ZOOM_RESET, run_zoom_hotkey};
    use crate::tab::{CleaningTabState, cleaning_default_dock_layout};

    /// One expected registry row: id, default shortcut, modifier-only binding, active in text input.
    type SpecRow = (&'static str, Option<(egui::Modifiers, egui::Key)>, bool, bool);

    /// The ids, shortcuts and order must equal the Cleaning slice of the root's
    /// `hotkey_registry_golden` (`src/studio.rs`): ids are persistence keys and the order is
    /// observable within the scope.
    #[test]
    fn the_zoom_hotkeys_match_the_pinned_registry_slice() {
        let tab = CleaningTabState::default();
        let specs = StudioTab::hotkey_specs(&tab);
        let actual: Vec<SpecRow> = specs
            .iter()
            .map(|spec| (spec.id, spec.default_shortcut.map(|s| (s.modifiers, s.logical_key)), spec.default_modifier_only.is_some(), spec.active_when_input))
            .collect();
        let expected: Vec<SpecRow> = vec![
            ("cleaning.canvas.zoom_in", Some((egui::Modifiers::COMMAND, egui::Key::Equals)), false, false),
            ("cleaning.canvas.zoom_out", Some((egui::Modifiers::COMMAND, egui::Key::Minus)), false, false),
            ("cleaning.canvas.zoom_reset", Some((egui::Modifiers::COMMAND, egui::Key::Num0)), false, false),
        ];
        assert_eq!(actual, expected);
        assert!(specs.iter().all(|spec| spec.scope == HotkeyScopeV2::Tab(AppTab::Cleaning)));
    }

    /// Every own id is handled; a foreign id is reported as not this tab's.
    #[test]
    fn execute_hotkey_claims_only_its_own_ids() {
        let mut tab = CleaningTabState::default();
        for id in [HOTKEY_CLEANING_ZOOM_IN, HOTKEY_CLEANING_ZOOM_OUT, HOTKEY_CLEANING_ZOOM_RESET] {
            assert!(run_zoom_hotkey(&mut tab, id), "{id} must be handled");
        }
        assert!(!run_zoom_hotkey(&mut tab, "translation.canvas.zoom_in"));
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
