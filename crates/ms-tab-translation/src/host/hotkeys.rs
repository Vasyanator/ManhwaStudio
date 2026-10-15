/*
FILE OVERVIEW: crates/ms-tab-translation/src/host/hotkeys.rs
The «Перевод» tab's input actions as the host (`TranslationTab`) sees them: the canvas-level
command ids and specs the host declares on top of `TranslationTabState::input_actions`, the one
dispatch of everything that fired for this tab, and the binding texts the canvas and the panels
show.

Key items:
- `HOTKEY_TRANSLATION_DELETE_BUBBLE` / `_CREATE_BUBBLE` / `_CREATE_HINT_BUBBLE`: the three canvas
  command ids. They are persistence keys (`user_config` -> `Hotkeys`): never rename them.
- `host_input_actions`: those three command specs, in declaration order.
- `TranslationTab::run_fired_commands`: the execute arms (bubble create/delete/copy/paste, panel
  toggles), each with the tab's own block check.
- `hotkey_hints`: the binding texts (registry labels, alternatives joined by " / ") and the two
  OCR selection-mode "held" flags, rebuilt every frame.

Notes:
- Zoom is the shared `ms_input::view` set, executed by the canvas itself (`CanvasView` reads it
  from the frame); the host only forwards the canvas's contexts (`StudioTab::input_contexts`).
- The OCR selection modes are `ModeSpec`s read through `InputFrame::mode_active`.
*/

use ms_canvas::BubbleTextField;
use ms_input::{ActionId, ActionSpec, InputFrame, InputRegistry};
use ms_project::ProjectData;

use super::TranslationTab;
use crate::tab::{
    HOTKEY_TRANSLATION_COPY_BUBBLE_ORIGINAL, HOTKEY_TRANSLATION_COPY_BUBBLE_TRANSLATION,
    HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE, HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE,
    HOTKEY_TRANSLATION_PASTE_BUBBLE_ORIGINAL, HOTKEY_TRANSLATION_PASTE_BUBBLE_TRANSLATION,
    HOTKEY_TRANSLATION_TOGGLE_BUBBLES_PANEL, HOTKEY_TRANSLATION_TOGGLE_COMPOSITION_PANEL,
    HOTKEY_TRANSLATION_TOGGLE_DETECTOR_PANEL, HOTKEY_TRANSLATION_TOGGLE_MT_PANEL,
    HOTKEY_TRANSLATION_TOGGLE_OCR_PANEL, TranslationHotkeyHints, single_key_binding,
    translation_command_spec,
};

/// Deletes the selected bubble (default Delete). Persistence key, never rename.
pub(super) const HOTKEY_TRANSLATION_DELETE_BUBBLE: ActionId = ActionId("translation.bubble.delete_selected");
/// Creates a bubble at the pointer (default T). Persistence key, never rename.
pub(super) const HOTKEY_TRANSLATION_CREATE_BUBBLE: ActionId = ActionId("translation.bubble.create_at_cursor");
/// Creates a hint bubble at the pointer (default H). Persistence key, never rename.
pub(super) const HOTKEY_TRANSLATION_CREATE_HINT_BUBBLE: ActionId = ActionId("translation.bubble.create_hint_at_cursor");

/// The three canvas-level Translation commands the host declares, in declaration order (delete,
/// create, create hint). The tab state's own actions (`TranslationTabState::input_actions`)
/// follow them; see `StudioTab::input_declarations`.
#[must_use]
pub(super) fn host_input_actions() -> Vec<ActionSpec> {
    const BUBBLES: &str = "app.hotkey.bubbles_category";
    vec![
        translation_command_spec(HOTKEY_TRANSLATION_DELETE_BUBBLE, "app.hotkey.delete_bubble", BUBBLES, single_key_binding(egui::Key::Delete)),
        translation_command_spec(HOTKEY_TRANSLATION_CREATE_BUBBLE, "app.hotkey.create_bubble", BUBBLES, single_key_binding(egui::Key::T)),
        translation_command_spec(HOTKEY_TRANSLATION_CREATE_HINT_BUBBLE, "app.hotkey.create_hint_bubble", BUBBLES, single_key_binding(egui::Key::H)),
    ]
}

/// The binding text of `id` for a hint, or `None` when the action has no binding (the hint is
/// then omitted, as for a disabled binding before).
fn hint_label(registry: &InputRegistry, id: ActionId) -> Option<String> {
    (!registry.bindings(id).is_empty()).then(|| registry.label(id))
}

/// The create-bubble hint the canvas shows and the tab state's hints for this frame: binding texts
/// from `registry`, the two OCR selection-mode flags from `input`.
#[must_use]
pub(super) fn hotkey_hints(registry: &InputRegistry, input: &InputFrame) -> (Option<String>, TranslationHotkeyHints) {
    let hints = TranslationHotkeyHints {
        ocr_quick_selection_mode: hint_label(registry, HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE),
        ocr_quick_selection_mode_modifier_down: input.mode_active(HOTKEY_TRANSLATION_OCR_QUICK_SELECTION_MODE),
        ocr_advanced_selection_mode: hint_label(registry, HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE),
        ocr_advanced_selection_mode_modifier_down: input.mode_active(HOTKEY_TRANSLATION_OCR_ADVANCED_SELECTION_MODE),
        bubbles_panel: hint_label(registry, HOTKEY_TRANSLATION_TOGGLE_BUBBLES_PANEL),
        ocr_panel: hint_label(registry, HOTKEY_TRANSLATION_TOGGLE_OCR_PANEL),
        composition_panel: hint_label(registry, HOTKEY_TRANSLATION_TOGGLE_COMPOSITION_PANEL),
        machine_translation_panel: hint_label(registry, HOTKEY_TRANSLATION_TOGGLE_MT_PANEL),
        text_detector_panel: hint_label(registry, HOTKEY_TRANSLATION_TOGGLE_DETECTOR_PANEL),
    };
    (hint_label(registry, HOTKEY_TRANSLATION_CREATE_BUBBLE), hints)
}

impl TranslationTab {
    /// Runs every command `input` reports as fired that this tab executes, in firing order.
    /// Commands of other owners (the session's save hotkeys) are skipped: the router fires only
    /// actions active for this tab or tab-independent ones, and the session runs its own.
    pub(super) fn run_fired_commands(&mut self, ctx: &egui::Context, project: &ProjectData, input: &InputFrame) {
        for fired in input.fired_commands() {
            self.run_command(ctx, project, fired.action);
        }
    }

    /// Runs one fired command. Returns `false` (nothing done) for an id this tab does not
    /// execute, `true` when it is this tab's — also when the tab's own block (advanced
    /// recognition open, detector edit mode) or a missing pointer made it a no-op.
    pub(super) fn run_command(&mut self, ctx: &egui::Context, project: &ProjectData, action: ActionId) -> bool {
        let Self { canvas, state, .. } = self;
        match action {
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
            // The two OCR selection modes are `Mode` actions (never fired, read through
            // `mode_active`); every other id belongs to another owner.
            _ => return false,
        }
        true
    }
}
