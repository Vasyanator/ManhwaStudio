/*
File: crates/ms-tab-typing/src/tab/studio_tab.rs

Purpose:
The «Текст» tab's answers to the studio root's order-free per-tab hooks
(`ms_studio_tab::StudioTab`), plus the two tab-owned pieces those hooks rely on: the exit-only
`user_config` keys (read once at construction, written once at teardown) and the canvas
bottom-hint rows.

Key items:
- `typing_hint_rows`: the curated bottom-hint rows, set by `TypingTabState::draw` every frame.
- `TypingTabState::load_exit_settings`: seeds the exit-only flags from the startup `user_config`.
- `impl StudioTab for TypingTabState`:
  - `input_contexts`: the canvas's contexts, plus the brush context while the mask panel is open;
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
- No declared actions yet (the tab reads its own keys while drawing, input-unification WP8), no
  background poll, no committed-tree view: those hooks keep their no-op defaults.
*/

use std::collections::HashSet;

use ms_canvas::{CanvasHintHelp, CanvasHintRow};
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

/// Builds the fixed, hand-authored Typing-tab bottom-hint rows.
///
/// Content is a curated set of localized label/key pairs (via `t!`), NOT sourced from the hotkey
/// registry. Rebuilt every frame so a runtime language switch re-localizes the rows. Order is
/// canonical and must match the documented Typing shortcut set. The select-text-area row carries an
/// animated "?" help icon, which puts this hint into the grid's three-column mode.
#[must_use]
pub(super) fn typing_hint_rows() -> Vec<CanvasHintRow> {
    vec![
        CanvasHintRow::new(
            t!("canvas.bottom_hint.typing.select_text_area_label"),
            t!("canvas.bottom_hint.typing.select_text_area_keys"),
        )
        .with_help(CanvasHintHelp::Animation(ms_gifs::typing::SELECT_TEXT_AREA)),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.typing.quick_font_size_label"),
            t!("canvas.bottom_hint.typing.quick_font_size_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.typing.rotate_label"),
            t!("canvas.bottom_hint.typing.rotate_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.typing.scale_label"),
            t!("canvas.bottom_hint.typing.scale_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.typing.nudge_label"),
            t!("canvas.bottom_hint.typing.nudge_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.typing.toggle_clean_label"),
            t!("canvas.bottom_hint.typing.toggle_clean_keys"),
        ),
        CanvasHintRow::new(
            t!("canvas.bottom_hint.typing.toggle_centering_assist_label"),
            t!("canvas.bottom_hint.typing.toggle_centering_assist_keys"),
        ),
    ]
}

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
    /// The canvas's contexts (`CanvasView::input_contexts`: `canvas.zoom_accepted` while no
    /// selected layer blocks canvas zoom, so Ctrl+wheel keeps rotating a selected layer), plus
    /// `ms_tools::mask_brush::BRUSH_ACTIVE` while the mask panel is open (its brush takes the
    /// shared brush actions).
    fn input_contexts(&self) -> Vec<ms_input::ContextId> {
        let mut contexts = self.canvas.input_contexts();
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

    use super::typing_hint_rows;
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

    /// Seven rows in canonical order; only the first carries a help icon (three-column mode).
    #[test]
    fn the_hint_has_seven_rows_and_one_help_icon() {
        let rows = typing_hint_rows();
        assert_eq!(rows.len(), 7);
        assert!(rows[0].help.is_some());
        assert!(rows[1..].iter().all(|row| row.help.is_none()));
    }
}
