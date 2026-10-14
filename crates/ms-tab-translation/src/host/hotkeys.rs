/*
FILE OVERVIEW: crates/ms-tab-translation/src/host/hotkeys.rs
The «Перевод» tab's configurable hotkeys as the host (`TranslationTab`) sees them: the canvas-level
command ids and specs the host declares on top of `TranslationTabState::hotkey_specs`, the one
execute dispatch for all of them, and the cached shortcut texts the canvas and the panels show.

Key items:
- `HOTKEY_TRANSLATION_ZOOM_IN` … `HOTKEY_TRANSLATION_CREATE_HINT_BUBBLE`: the six canvas command
  ids. They are persistence keys (`user_config` -> `Hotkeys`): never rename them.
- `host_hotkey_specs`: those six specs, in their registration order.
- `TranslationTab::execute_hotkey_command`: the sixteen execute arms (canvas zoom, bubble
  create/delete/copy/paste, panel toggles), each with the tab's own block check.
- `HotkeyHintCache` / `TranslationTab::refresh_hotkey_hints`: shortcut texts rebuilt only when the
  registry's bindings revision changes; the two modifier-only "held" flags are read every frame.

Notes:
- Registration order is observable within the Translation scope (`collect_triggered` runs the
  triggered commands in that order), so `host_hotkey_specs` keeps the six in their historical
  order and the host registers them BEFORE the tab state's eleven.
- Titles keep their `app.hotkey.*` i18n keys and the literal `"Canvas"` section of the registry.
*/

use ms_canvas::BubbleTextField;
use ms_config::app_tab::AppTab;
use ms_project::ProjectData;
use ms_widgets::input_manager_v2::{HotkeyScopeV2, HotkeySpecV2, InputManagerV2};

use super::TranslationTab;
use crate::tab::{
    HOTKEY_TRANSLATION_COPY_BUBBLE_ORIGINAL, HOTKEY_TRANSLATION_COPY_BUBBLE_TRANSLATION,
    HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE, HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE,
    HOTKEY_TRANSLATION_PASTE_BUBBLE_ORIGINAL, HOTKEY_TRANSLATION_PASTE_BUBBLE_TRANSLATION,
    HOTKEY_TRANSLATION_TOGGLE_BUBBLES_PANEL, HOTKEY_TRANSLATION_TOGGLE_COMPOSITION_PANEL,
    HOTKEY_TRANSLATION_TOGGLE_DETECTOR_PANEL, HOTKEY_TRANSLATION_TOGGLE_MT_PANEL,
    HOTKEY_TRANSLATION_TOGGLE_OCR_PANEL, TranslationHotkeyHints,
};

pub(super) const HOTKEY_TRANSLATION_ZOOM_IN: &str = "translation.canvas.zoom_in";
pub(super) const HOTKEY_TRANSLATION_ZOOM_OUT: &str = "translation.canvas.zoom_out";
pub(super) const HOTKEY_TRANSLATION_ZOOM_RESET: &str = "translation.canvas.zoom_reset";
pub(super) const HOTKEY_TRANSLATION_DELETE_BUBBLE: &str = "translation.bubble.delete_selected";
pub(super) const HOTKEY_TRANSLATION_CREATE_BUBBLE: &str = "translation.bubble.create_at_cursor";
pub(super) const HOTKEY_TRANSLATION_CREATE_HINT_BUBBLE: &str = "translation.bubble.create_hint_at_cursor";

/// The six canvas-level Translation hotkey specs the host declares, in registration order (zoom
/// in, zoom out, zoom reset, delete, create, create hint). The tab state's own eleven
/// (`TranslationTabState::hotkey_specs`) follow them; see `StudioTab::hotkey_specs`.
#[must_use]
pub(super) fn host_hotkey_specs() -> [HotkeySpecV2; 6] {
    [
        HotkeySpecV2 {
            id: HOTKEY_TRANSLATION_ZOOM_IN,
            title: t!("app.hotkey.zoom_in"),
            section: "Canvas",
            default_shortcut: Some(egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Equals)),
            default_modifier_only: None,
            scope: HotkeyScopeV2::Tab(AppTab::Translation),
            active_when_input: false,
        },
        HotkeySpecV2 {
            id: HOTKEY_TRANSLATION_ZOOM_OUT,
            title: t!("app.hotkey.zoom_out"),
            section: "Canvas",
            default_shortcut: Some(egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Minus)),
            default_modifier_only: None,
            scope: HotkeyScopeV2::Tab(AppTab::Translation),
            active_when_input: false,
        },
        HotkeySpecV2 {
            id: HOTKEY_TRANSLATION_ZOOM_RESET,
            title: t!("app.hotkey.zoom_reset"),
            section: "Canvas",
            default_shortcut: Some(egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Num0)),
            default_modifier_only: None,
            scope: HotkeyScopeV2::Tab(AppTab::Translation),
            active_when_input: false,
        },
        HotkeySpecV2 {
            id: HOTKEY_TRANSLATION_DELETE_BUBBLE,
            title: t!("app.hotkey.delete_bubble"),
            section: t!("app.hotkey.bubbles_category"),
            default_shortcut: Some(egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::Delete)),
            default_modifier_only: None,
            scope: HotkeyScopeV2::Tab(AppTab::Translation),
            active_when_input: false,
        },
        HotkeySpecV2 {
            id: HOTKEY_TRANSLATION_CREATE_BUBBLE,
            title: t!("app.hotkey.create_bubble"),
            section: t!("app.hotkey.bubbles_category"),
            default_shortcut: Some(egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::T)),
            default_modifier_only: None,
            scope: HotkeyScopeV2::Tab(AppTab::Translation),
            active_when_input: false,
        },
        HotkeySpecV2 {
            id: HOTKEY_TRANSLATION_CREATE_HINT_BUBBLE,
            title: t!("app.hotkey.create_hint_bubble"),
            section: t!("app.hotkey.bubbles_category"),
            default_shortcut: Some(egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::H)),
            default_modifier_only: None,
            scope: HotkeyScopeV2::Tab(AppTab::Translation),
            active_when_input: false,
        },
    ]
}

/// Shortcut texts of the Translation commands, rebuilt from the registry only when its bindings
/// revision changes (`shortcut_text` formats through egui every call). Starts at revision 0 with
/// no texts, exactly as the cache it replaces did.
#[derive(Debug, Clone, Default)]
pub(super) struct HotkeyHintCache {
    bindings_revision: u64,
    /// Shown by the canvas next to its «create bubble» affordance.
    create_bubble_hint: Option<String>,
    /// Pushed into the tab state every frame (panel captions, OCR selection hints); the two
    /// `*_modifier_down` flags are overwritten per frame, never cached.
    translation_hints: TranslationHotkeyHints,
}

impl TranslationTab {
    /// Runs one triggered Translation command. Returns `false` (nothing done) for an id that is
    /// not one of the host's six or the tab state's eleven; `true` when the id is this tab's,
    /// including when the tab's own block (advanced recognition open, detector edit mode) or a
    /// missing pointer made it a no-op — the command was still this tab's to decide.
    pub(super) fn execute_hotkey_command(&mut self, ctx: &egui::Context, command_id: &str, project: &ProjectData) -> bool {
        let Self { canvas, state, .. } = self;
        match command_id {
            HOTKEY_TRANSLATION_ZOOM_IN => {
                if !state.blocks_canvas_zoom() {
                    canvas.zoom_by_shortcut(1.1);
                }
            }
            HOTKEY_TRANSLATION_ZOOM_OUT => {
                if !state.blocks_canvas_zoom() {
                    canvas.zoom_by_shortcut(1.0 / 1.1);
                }
            }
            HOTKEY_TRANSLATION_ZOOM_RESET => {
                if !state.blocks_canvas_zoom() {
                    canvas.reset_zoom_shortcut();
                }
            }
            HOTKEY_TRANSLATION_DELETE_BUBBLE => {
                if !state.blocks_canvas_bubble_hotkeys() {
                    canvas.delete_selected_bubble_shortcut();
                }
            }
            HOTKEY_TRANSLATION_CREATE_BUBBLE => {
                if !state.blocks_canvas_bubble_hotkeys()
                    && let Some(pointer_pos) = ctx.pointer_latest_pos()
                {
                    state.create_bubble_at_pointer_shortcut(ctx, canvas, project, pointer_pos);
                }
            }
            HOTKEY_TRANSLATION_CREATE_HINT_BUBBLE => {
                if !state.blocks_canvas_bubble_hotkeys()
                    && let Some(pointer_pos) = ctx.pointer_latest_pos()
                {
                    state.create_hint_bubble_at_pointer_shortcut(ctx, canvas, pointer_pos);
                }
            }
            HOTKEY_TRANSLATION_COPY_BUBBLE_ORIGINAL => {
                if !state.blocks_canvas_bubble_hotkeys() {
                    canvas.copy_selected_bubble_text_shortcut(ctx, BubbleTextField::Original);
                }
            }
            HOTKEY_TRANSLATION_COPY_BUBBLE_TRANSLATION => {
                if !state.blocks_canvas_bubble_hotkeys() {
                    canvas.copy_selected_bubble_text_shortcut(ctx, BubbleTextField::Translation);
                }
            }
            HOTKEY_TRANSLATION_PASTE_BUBBLE_ORIGINAL => {
                if !state.blocks_canvas_bubble_hotkeys() {
                    canvas.paste_selected_bubble_text_shortcut(ctx, BubbleTextField::Original);
                }
            }
            HOTKEY_TRANSLATION_PASTE_BUBBLE_TRANSLATION => {
                if !state.blocks_canvas_bubble_hotkeys() {
                    canvas.paste_selected_bubble_text_shortcut(ctx, BubbleTextField::Translation);
                }
            }
            HOTKEY_TRANSLATION_TOGGLE_BUBBLES_PANEL => state.toggle_bubbles_panel_hotkey(),
            HOTKEY_TRANSLATION_TOGGLE_OCR_PANEL => state.toggle_ocr_panel_hotkey(),
            HOTKEY_TRANSLATION_TOGGLE_COMPOSITION_PANEL => state.toggle_composition_panel_hotkey(),
            HOTKEY_TRANSLATION_TOGGLE_MT_PANEL => state.toggle_machine_translation_panel_hotkey(),
            HOTKEY_TRANSLATION_TOGGLE_DETECTOR_PANEL => state.toggle_text_detector_panel_hotkey(),
            // The two OCR selection-mode ids are bound modifier-only (Settings keeps them so), and
            // `collect_triggered` skips modifier-only bindings: they are read through
            // `modifier_only_active` in `refresh_hotkey_hints` instead.
            _ => return false,
        }
        true
    }

    /// Per-frame hotkey-hint refresh, called by the root at its fixed point before the top bar
    /// (whatever tab is active). Rebuilds the cached shortcut texts when the registry's bindings
    /// revision changed, pushes the create-bubble hint into the canvas, and hands the tab state its
    /// hints with the two modifier-only "held" flags read from this frame's input.
    pub fn refresh_hotkey_hints(&mut self, ctx: &egui::Context, hotkeys: &InputManagerV2) {
        let bindings_revision = hotkeys.bindings_revision();
        if bindings_revision != self.hints.bindings_revision {
            self.hints = HotkeyHintCache {
                bindings_revision,
                create_bubble_hint: hotkeys.shortcut_text(ctx, HOTKEY_TRANSLATION_CREATE_BUBBLE),
                translation_hints: TranslationHotkeyHints {
                    ocr_quick_selection_mode: hotkeys.shortcut_text(ctx, HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE),
                    ocr_quick_selection_mode_modifier_down: false,
                    ocr_advanced_selection_mode: hotkeys.shortcut_text(ctx, HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE),
                    ocr_advanced_selection_mode_modifier_down: false,
                    bubbles_panel: hotkeys.shortcut_text(ctx, HOTKEY_TRANSLATION_TOGGLE_BUBBLES_PANEL),
                    ocr_panel: hotkeys.shortcut_text(ctx, HOTKEY_TRANSLATION_TOGGLE_OCR_PANEL),
                    composition_panel: hotkeys.shortcut_text(ctx, HOTKEY_TRANSLATION_TOGGLE_COMPOSITION_PANEL),
                    machine_translation_panel: hotkeys.shortcut_text(ctx, HOTKEY_TRANSLATION_TOGGLE_MT_PANEL),
                    text_detector_panel: hotkeys.shortcut_text(ctx, HOTKEY_TRANSLATION_TOGGLE_DETECTOR_PANEL),
                },
            };
        }
        self.canvas.set_create_bubble_shortcut_hint(self.hints.create_bubble_hint.clone());
        let mut hints = self.hints.translation_hints.clone();
        hints.ocr_quick_selection_mode_modifier_down = hotkeys.modifier_only_active(ctx, HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE);
        hints.ocr_advanced_selection_mode_modifier_down = hotkeys.modifier_only_active(ctx, HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE);
        self.state.set_hotkey_hints(hints);
    }
}
