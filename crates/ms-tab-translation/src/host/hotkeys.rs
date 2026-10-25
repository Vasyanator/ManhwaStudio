/*
FILE OVERVIEW: crates/ms-tab-translation/src/host/hotkeys.rs
The one dispatch of the «Перевод» tab's fired commands (`crate::input` declares them): bubble
create / delete / copy / paste, panel toggles and the lines edit commands.

Key items:
- `TranslationTab::run_fired_commands`: runs every command of this frame's input, in firing
  order.
- `TranslationTab::run_command`: the execute arms.

Notes:
- Zoom is the shared `ms_input::view` set, executed by the canvas itself (and by the
  advanced-recognition window for its own view); the host only forwards contexts.
- Where a command may run is decided by its declared conditions (detector edit modes and the
  advanced-recognition window exclude the bubble commands), never re-checked here.
*/

use ms_canvas::BubbleTextField;
use ms_input::{Fired, InputFrame};
use ms_project::ProjectData;

use super::TranslationTab;
use crate::input::{
    COPY_BUBBLE_ORIGINAL, COPY_BUBBLE_TRANSLATION, CREATE_BUBBLE, CREATE_HINT_BUBBLE, CREATE_IMAGE_BUBBLE, DELETE_BUBBLE, LINES_DELETE, LINES_NEW,
    PASTE_BUBBLE_ORIGINAL, PASTE_BUBBLE_TRANSLATION, TOGGLE_BUBBLES_PANEL, TOGGLE_COMPOSITION_PANEL, TOGGLE_DETECTOR_PANEL, TOGGLE_MT_PANEL,
    TOGGLE_OCR_PANEL, recent_char_rank,
};

impl TranslationTab {
    /// Runs every command `input` reports as fired that this tab executes, in firing order.
    /// Commands of other owners (the session's save commands, the canvas's and the view set's)
    /// are skipped.
    pub(super) fn run_fired_commands(&mut self, ctx: &egui::Context, project: &ProjectData, input: &InputFrame) {
        for fired in input.fired_commands() {
            self.run_command(ctx, project, fired);
        }
    }

    /// Runs one fired command. Returns `false` (nothing done) for an id this tab does not
    /// execute, `true` when it is this tab's — also when a missing pointer made it a no-op.
    pub(super) fn run_command(&mut self, ctx: &egui::Context, project: &ProjectData, fired: &Fired) -> bool {
        let Self { canvas, state, .. } = self;
        match fired.action {
            // The canvas methods report whether anything was there to act on; nothing to do then.
            DELETE_BUBBLE => {
                canvas.delete_selected_bubble_shortcut();
            }
            CREATE_BUBBLE => {
                if let Some(pointer_pos) = ctx.pointer_latest_pos() {
                    state.create_bubble_at_pointer_shortcut(canvas, project, pointer_pos, recent_char_rank(&fired.params));
                }
            }
            CREATE_HINT_BUBBLE => {
                if let Some(pointer_pos) = ctx.pointer_latest_pos() {
                    state.create_hint_bubble_at_pointer_shortcut(ctx, canvas, pointer_pos);
                }
            }
            CREATE_IMAGE_BUBBLE => {
                // Image bubbles are written through at once (their file and crop fields are
                // edited right after creation).
                if let Some(pointer_pos) = ctx.pointer_latest_pos()
                    && canvas.create_image_bubble_at_pointer_shortcut(ctx, pointer_pos)
                {
                    canvas.flush_pending_bubble_upserts_now(project);
                }
            }
            COPY_BUBBLE_ORIGINAL => {
                canvas.copy_selected_bubble_text_shortcut(ctx, BubbleTextField::Original);
            }
            COPY_BUBBLE_TRANSLATION => {
                canvas.copy_selected_bubble_text_shortcut(ctx, BubbleTextField::Translation);
            }
            PASTE_BUBBLE_ORIGINAL => {
                canvas.paste_selected_bubble_text_shortcut(ctx, BubbleTextField::Original);
            }
            PASTE_BUBBLE_TRANSLATION => {
                canvas.paste_selected_bubble_text_shortcut(ctx, BubbleTextField::Translation);
            }
            TOGGLE_BUBBLES_PANEL => state.toggle_bubbles_panel_hotkey(),
            TOGGLE_OCR_PANEL => state.toggle_ocr_panel_hotkey(),
            TOGGLE_COMPOSITION_PANEL => state.toggle_composition_panel_hotkey(),
            TOGGLE_MT_PANEL => state.toggle_machine_translation_panel_hotkey(),
            TOGGLE_DETECTOR_PANEL => state.toggle_text_detector_panel_hotkey(),
            LINES_DELETE => state.delete_selected_text_detector_line(),
            LINES_NEW => state.create_text_detector_line_at_page_center(canvas),
            // Modes and gestures never fire; every other id belongs to another owner.
            _ => return false,
        }
        true
    }
}
