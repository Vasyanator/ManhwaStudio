/*
File: crates/ms-tab-typing/src/tab/studio_tab.rs

Purpose:
The «Текст» tab's answers to the studio root's order-free per-tab hooks
(`ms_studio_tab::StudioTab`), plus the exit-only `user_config` keys those hooks rely on (read once
at construction, written once at teardown).

Key items:
- `TypingTabState::load_exit_settings`: seeds the exit-only flags from the startup `user_config`.
- `impl StudioTab for TypingTabState`:
  - `input_declarations`: the tab's input set (`tab/input.rs::declarations`);
  - `input_contexts`: the canvas's contexts, the tab's own (`tab/input.rs::contexts_for`), plus the
    brush context while the mask panel is open;
  - `default_dock_layout`: `typing_default_dock_layout` (the tab hosts the panel dock);
  - `evict_tab_gpu_caches`: the canvas clean-overlay cache, then the tab's own mask and overlay
    textures (`evict_gpu_caches`);
  - `source_page_window` / `wants_nearest_source`: answered by the tab's canvas;
  - `write_exit_settings`: `Canvas.typing_hint_collapsed`, `TextTab.centering_assist_enabled`,
    `TextTab.centering_show_center`;
  - `flush_debounced_stores_blocking`: the per-font settings store, then the default
    local-preset set.

Notes:
- The exit-only keys and their defaults are persisted user data: absent `typing_hint_collapsed`
  = expanded (`false`), absent `centering_assist_enabled` = off (`false`), absent
  `centering_show_center` = shown (`true`). The bound-center KIND is session-only and has no key.
  They are written only at teardown, never per toggle.
- No background poll and no committed-tree view: those hooks keep their no-op defaults.
- The canvas bottom-hint rows are built from the input registry's labels in `tab/input.rs`.
*/

use std::collections::HashSet;

use ms_config::{CANVAS_SECTION, TEXT_TAB_SECTION, section_bool, set_section_bool};
use ms_memory::{CacheEvictionReport, CacheEvictionRequest};
use ms_studio_tab::StudioTab;
use ms_widgets::panel_dock::DockLayout;
use serde_json::Value;

use super::{TypingTabState, typing_default_dock_layout};

/// `Canvas` key of the typing canvas' bottom-hint collapsed flag (absent = `false`, expanded).
const CANVAS_TYPING_HINT_COLLAPSED_KEY: &str = "typing_hint_collapsed";
/// `TextTab` key of the «Помочь с центровкой» toggle (absent = `false`, off).
const TEXT_TAB_CENTERING_ASSIST_ENABLED_KEY: &str = "centering_assist_enabled";
/// `TextTab` key of the «Показывать центр» marker (absent = `true`, shown: the dependent marker
/// is on by default, which is why its default differs from the assist toggle's).
const TEXT_TAB_CENTERING_SHOW_CENTER_KEY: &str = "centering_show_center";

impl TypingTabState {
    /// Seeds the exit-only flags ONCE at construction from the merged startup `user_settings`:
    /// the bottom-hint collapsed state and both centering-assist checkboxes. A missing section or
    /// key, or a non-boolean value, yields the documented default. Must not be called every frame
    /// (it would override the user's live toggles). Pure: no I/O.
    pub fn load_exit_settings(&mut self, user_settings: &Value) {
        self.set_hint_collapsed(section_bool(user_settings, CANVAS_SECTION, CANVAS_TYPING_HINT_COLLAPSED_KEY, false));
        self.set_centering_assist_persisted_state(
            section_bool(user_settings, TEXT_TAB_SECTION, TEXT_TAB_CENTERING_ASSIST_ENABLED_KEY, false),
            section_bool(user_settings, TEXT_TAB_SECTION, TEXT_TAB_CENTERING_SHOW_CENTER_KEY, true),
        );
    }
}

impl StudioTab for TypingTabState {
    /// The tab's commands, modes, axes, gestures and targets (`tab/input.rs`).
    fn input_declarations(&self) -> ms_input::Declarations {
        super::input::declarations()
    }

    /// The canvas's contexts (`CanvasView::input_contexts`; the tab never blocks canvas zoom: a
    /// selected layer's Ctrl+wheel rotation shadows only the wheel), the tab's own contexts
    /// (`tab/input.rs::contexts_for`: selection, layout editor, mask, create, font-size target),
    /// plus `ms_tools::mask_brush::BRUSH_ACTIVE` while the mask panel is open (its brush takes the
    /// shared brush actions).
    fn input_contexts(&self) -> Vec<ms_input::ContextId> {
        let mut contexts = self.canvas.input_contexts();
        contexts.extend(super::input::contexts_for(self.input_state()));
        if self.mask_layer.is_panel_open() {
            contexts.push(ms_tools::mask_brush::BRUSH_ACTIVE);
        }
        contexts
    }

    /// The typing tab hosts the panel dock; its default arrangement is the SAME `fn` item the tab
    /// hands to `ensure_default_layout` every frame.
    fn default_dock_layout(&self) -> Option<fn() -> DockLayout> {
        Some(typing_default_dock_layout)
    }

    /// The canvas clean-overlay textures first, then the tab's own mask and overlay textures;
    /// one merged report.
    fn evict_tab_gpu_caches(&mut self, request: &CacheEvictionRequest) -> CacheEvictionReport {
        let mut report = self.evict_clean_overlay_gpu_cache(request);
        report.append(self.evict_gpu_caches(request));
        report
    }

    /// The visible pages of the tab's canvas plus `neighbor_radius` on each side.
    fn source_page_window(&self, neighbor_radius: usize) -> HashSet<usize> {
        self.canvas.active_source_page_window(neighbor_radius)
    }

    /// The canvas zoomed into pixel inspection draws NEAREST source tiles.
    fn wants_nearest_source(&self) -> bool {
        self.canvas.source_pixel_inspection_active()
    }

    /// Writes the three exit-only flags; every other key of `root` and of both sections survives,
    /// a non-object root or section is replaced (`ms_config::set_section_bool`).
    fn write_exit_settings(&self, root: &mut Value) {
        set_section_bool(root, CANVAS_SECTION, CANVAS_TYPING_HINT_COLLAPSED_KEY, self.hint_collapsed());
        set_section_bool(root, TEXT_TAB_SECTION, TEXT_TAB_CENTERING_ASSIST_ENABLED_KEY, self.centering_assist_enabled());
        set_section_bool(root, TEXT_TAB_SECTION, TEXT_TAB_CENTERING_SHOW_CENTER_KEY, self.centering_show_center());
    }

    /// Synchronously writes a still-pending per-font-settings save, then a still-owed default
    /// local-preset set. Both stores persist through a multi-second debounce whose writer is a
    /// detached thread that dies with the process, so closing inside that window would lose the
    /// edit. Synchronous by necessity: no GUI frame follows teardown. No-op when nothing is owed.
    fn flush_debounced_stores_blocking(&mut self) {
        if crate::font_admin::flush_pending_saves() {
            ms_log::runtime_log::log_info("[typing] on_exit: flushed a pending fonts_data.json write");
        }
        if self.flush_pending_local_presets_save() {
            ms_log::runtime_log::log_info("[typing] on_exit: flushed a pending fonts/presets.json local-preset write");
        }
    }
}

#[cfg(test)]
mod tests {
    use ms_studio_tab::StudioTab;
    use ms_widgets::panel_dock::TabId;
    use serde_json::{Value, json};

    use crate::tab::{TypingTabState, typing_default_dock_layout};

    /// Loads `user_settings` into a fresh tab and writes its exit settings into `root`.
    fn round_trip(user_settings: &Value, mut root: Value) -> Value {
        let mut tab = TypingTabState::default();
        tab.load_exit_settings(user_settings);
        StudioTab::write_exit_settings(&tab, &mut root);
        root
    }

    /// Absent keys seed the documented defaults: expanded hint, assist off, center marker shown.
    #[test]
    fn absent_keys_seed_the_documented_defaults() {
        let root = round_trip(&json!({}), json!({}));
        assert_eq!(
            root,
            json!({
                "Canvas": { "typing_hint_collapsed": false },
                "TextTab": { "centering_assist_enabled": false, "centering_show_center": true },
            })
        );
    }

    /// A non-boolean stored value reads as the default, exactly like an absent key.
    #[test]
    fn non_boolean_values_read_as_defaults() {
        let stored = json!({
            "Canvas": { "typing_hint_collapsed": "yes" },
            "TextTab": { "centering_assist_enabled": 1, "centering_show_center": null },
        });
        let root = round_trip(&stored, json!({}));
        assert_eq!(root["Canvas"]["typing_hint_collapsed"], json!(false));
        assert_eq!(root["TextTab"]["centering_assist_enabled"], json!(false));
        assert_eq!(root["TextTab"]["centering_show_center"], json!(true));
    }

    /// Stored values survive a load -> write round trip.
    #[test]
    fn stored_values_round_trip() {
        let stored = json!({
            "Canvas": { "typing_hint_collapsed": true },
            "TextTab": { "centering_assist_enabled": true, "centering_show_center": false },
        });
        let root = round_trip(&stored, json!({}));
        assert_eq!(root, stored);
    }

    /// The write keeps every other key: the Translation hint flag, a stale
    /// `cleaning_hint_collapsed`, other `TextTab` keys and other sections are left untouched.
    #[test]
    fn the_write_preserves_other_keys() {
        let existing = json!({
            "Canvas": { "cache_pages": true, "translation_hint_collapsed": true, "cleaning_hint_collapsed": true },
            "TextTab": { "text_language": "en" },
            "General": { "x": 1 },
        });
        let root = round_trip(&json!({}), existing);
        assert_eq!(
            root,
            json!({
                "Canvas": { "cache_pages": true, "translation_hint_collapsed": true, "cleaning_hint_collapsed": true, "typing_hint_collapsed": false },
                "TextTab": { "text_language": "en", "centering_assist_enabled": false, "centering_show_center": true },
                "General": { "x": 1 },
            })
        );
    }

    /// A non-object root, or a non-object section, is replaced so the flags are never dropped.
    #[test]
    fn non_object_root_and_sections_are_replaced() {
        let root = round_trip(&json!({}), json!([1, 2]));
        assert_eq!(root["Canvas"]["typing_hint_collapsed"], json!(false));
        assert_eq!(root["TextTab"]["centering_show_center"], json!(true));
        let root = round_trip(&json!({}), json!({ "Canvas": 3, "TextTab": "x" }));
        assert_eq!(root["Canvas"], json!({ "typing_hint_collapsed": false }));
        assert_eq!(root["TextTab"], json!({ "centering_assist_enabled": false, "centering_show_center": true }));
    }

    /// Every dock tab the typing tab declares; a default layout missing one drops a stored
    /// arrangement of that tab on every load (`panel_dock/persist.rs::known_tabs`).
    const TYPING_TABS: &[TabId] = &[
        ms_canvas::CANVAS_RIBBON_TAB,
        TabId::new("typing.preview"),
        TabId::new("typing.params"),
        TabId::new("typing.effects"),
        TabId::new("typing.actions"),
        TabId::new("typing.layers"),
        TabId::new("typing.mask"),
        TabId::new("typing.deform"),
        TabId::new("typing.layout_editor"),
    ];

    /// The hook registers the very builder the tab passes to `ensure_default_layout`, and its
    /// layout names every declared dock tab (the semantic backstop, since function-pointer
    /// equality is not guaranteed across codegen-unit merging).
    #[test]
    fn the_dock_hook_registers_the_builder_the_tab_declares() {
        let tab = TypingTabState::default();
        let tab: &dyn StudioTab = &tab;
        let builder = tab.default_dock_layout().expect("the typing tab hosts the panel dock");
        assert!(
            std::ptr::fn_addr_eq(builder, typing_default_dock_layout as fn() -> _),
            "the typing tab registers a builder other than the one it declares"
        );
        let layout = builder();
        for id in TYPING_TABS {
            assert!(
                layout.panel_of_tab(*id).is_some(),
                "the typing default layout does not name {id}, so a stored {id} would be dropped \
                 on every load"
            );
        }
    }
}
