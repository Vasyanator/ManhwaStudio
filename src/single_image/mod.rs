/*
File: src/single_image/mod.rs

Purpose:
App-shell half of the single-image mode (plan `dev-docs/single_image_mode_plan.md`, WP-2.4): the
session controller behind «Сохранить» / «Сохранить как», its dialogs, and the small pure rules
`MangaApp` consults (which tabs exist, the window title, the save hotkeys). `src/app.rs` holds only
hooks into this module (plan D12: `app.rs` is over the 5000-line gate).

Key structures:
- `SingleImageController` (native: `controller.rs`; wasm: an uninhabited stub): one field of
  `MangaApp`, `Some` only in a single-image session.
- `SaveParts`: the borrows of `MangaApp` a save tick needs.
- `SaveOutcome`: what the app must do after a save step that was started from the exit dialog.
- `ExitChoice`: the single-image exit dialog's answer.

Key functions:
- `tab_visible()`: the tab filter of the mode.
- `image_window_title()`: the studio window title of an image session (shared with startup).
- `hotkey_specs()`: Ctrl+S / Ctrl+Shift+S, registered only in a single-image session.

Notes:
The save machinery is native-only (it drives `TypingTabState::*_flatten_to_file`, which does not
exist on wasm). The web build never opens an image (plan D10), so there the controller is an
uninhabited enum: `Option<SingleImageController>` is always `None` and its methods are provably
unreachable, which keeps `app.rs` free of `cfg` attributes.
*/

#[cfg(not(target_arch = "wasm32"))]
mod controller;
mod dialogs;
#[cfg(not(target_arch = "wasm32"))]
mod machine;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use controller::SingleImageController;
pub(crate) use dialogs::draw_exit_dialog;

use crate::app::PendingCloseAction;
use crate::input_manager_v2::{HotkeyScopeV2, HotkeySpecV2};
use crate::models::autosave_gate::AutosaveGate;
use crate::project::ProjectData;
use crate::tabs::AppTab;
use crate::tabs::ps_editor::PsEditorTabState;
use crate::tabs::typing::TypingTabState;
use eframe::egui;

/// Hotkey id of «Сохранить» (Ctrl+S) in a single-image session.
pub(crate) const HOTKEY_SAVE_IMAGE: &str = "single_image.save";
/// Hotkey id of «Сохранить как» (Ctrl+Shift+S) in a single-image session.
pub(crate) const HOTKEY_SAVE_IMAGE_AS: &str = "single_image.save_as";

/// The `MangaApp` borrows one save tick needs (disjoint fields of the app).
pub(crate) struct SaveParts<'a> {
    /// The open (scratch) chapter.
    pub project: &'a ProjectData,
    /// Owner of the flatten-to-file API and of the deferred text edits.
    pub typing: &'a mut TypingTabState,
    /// Its active page's raster layers are flushed before the flatten.
    pub ps_editor: &'a mut PsEditorTabState,
    /// The project's autosave gate; its `action_count` is the dirty baseline.
    pub gate: &'a AutosaveGate,
}

/// What `MangaApp` must do after a save started from the exit dialog ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaveOutcome {
    /// The file was written and nothing changed since: run the close action now.
    CloseNow(PendingCloseAction),
    /// The user backed out of the save (format panel closed, file picker or JPEG options
    /// cancelled), or edited the image while it was being written: show the exit dialog for this
    /// action again.
    ReturnToExitDialog(PendingCloseAction),
}

/// The answer of the single-image exit dialog for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitChoice {
    /// Keep the window open.
    Cancel,
    /// Save to the file, then run the close action.
    Save,
    /// Close without writing the file.
    DontSave,
}

/// Whether `tab` exists in the tab bar. A single-image session has no project, so Page Manager,
/// Characters, Terms and Notes (all chapter- or title-scoped) are hidden; every tab exists in a
/// project session.
#[must_use]
pub(crate) fn tab_visible(tab: AppTab, single_image: bool) -> bool {
    if !single_image {
        return true;
    }
    match tab {
        AppTab::PageManager | AppTab::Characters | AppTab::Terms | AppTab::Notes => false,
        AppTab::Translation | AppTab::Cleaning | AppTab::Typing | AppTab::PsEditor | AppTab::Settings | AppTab::Wiki => true,
    }
}

/// Studio window title of an image session: `ManhwaStudio v{version} - {file name}` (the whole
/// path when it has no file-name component). `version` is the display version (`MS_APP_VERSION`).
/// The one owner of this format: startup (`StudioOpenRequest::window_title`) and the controller
/// (after «Сохранить как» moved the save target) both call it. Native-only like both callers.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub(crate) fn image_window_title(version: &str, path: &std::path::Path) -> String {
    let name = path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned());
    format!("ManhwaStudio v{version} - {name}")
}

/// The two save hotkeys of the mode, both `Global`. Registered ONLY for a single-image session so
/// a project session neither lists them in Settings nor reacts to them. «Сохранить как» comes
/// FIRST: egui matches shortcuts logically (an extra Shift still matches Ctrl+S), so the more
/// specific shortcut must consume the key event before the plain one sees it.
#[must_use]
pub(crate) fn hotkey_specs() -> [HotkeySpecV2; 2] {
    [
        HotkeySpecV2 {
            id: HOTKEY_SAVE_IMAGE_AS,
            title: t!("app.hotkey.save_image_as"),
            section: t!("app.hotkey.file_category"),
            default_shortcut: Some(egui::KeyboardShortcut::new(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::S)),
            default_modifier_only: None,
            scope: HotkeyScopeV2::Global,
            active_when_input: false,
        },
        HotkeySpecV2 {
            id: HOTKEY_SAVE_IMAGE,
            title: t!("app.hotkey.save_image"),
            section: t!("app.hotkey.file_category"),
            default_shortcut: Some(egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::S)),
            default_modifier_only: None,
            scope: HotkeyScopeV2::Global,
            active_when_input: false,
        },
    ]
}

/// Web build stand-in: an UNINHABITED type. The web entry never opens an image (plan D10), so no
/// value can exist and every method is statically unreachable; it exists only so `MangaApp` keeps
/// one `Option<SingleImageController>` field on every target.
#[cfg(target_arch = "wasm32")]
pub(crate) enum SingleImageController {}

#[cfg(target_arch = "wasm32")]
impl SingleImageController {
    /// Always `None` on the web: an image session cannot be opened there.
    pub(crate) fn from_project(project: &ProjectData, _user_settings: &serde_json::Value) -> Option<Self> {
        if project.is_single_image() {
            crate::runtime_log::log_error("[single_image] a single-image session reached the web build; the mode is native-only, saving is unavailable");
        }
        None
    }

    pub(crate) fn is_dirty(&self, _gate: &AutosaveGate, _typing: &TypingTabState, _ps_editor: &PsEditorTabState) -> bool {
        match *self {}
    }

    pub(crate) fn write_in_flight(&self) -> bool {
        match *self {}
    }

    pub(crate) fn picker_open(&self) -> bool {
        match *self {}
    }

    pub(crate) fn request_save(&mut self) {
        match *self {}
    }

    pub(crate) fn request_save_as(&mut self, _ctx: &egui::Context) {
        match *self {}
    }

    pub(crate) fn request_save_then(&mut self, _action: PendingCloseAction) {
        match *self {}
    }

    pub(crate) fn abandon_pending(&mut self) {
        match *self {}
    }

    pub(crate) fn tick(&mut self, _ctx: &egui::Context, _parts: SaveParts<'_>) -> Option<SaveOutcome> {
        match *self {}
    }

    pub(crate) fn draw_top_bar(&mut self, _ui: &mut egui::Ui) {
        match *self {}
    }

    pub(crate) fn draw_dialogs(&mut self, _ctx: &egui::Context) -> Option<SaveOutcome> {
        match *self {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn tab_visible_hides_only_project_tabs_in_single_image() {
        for tab in AppTab::ALL {
            assert!(tab_visible(tab, false), "{tab:?} must exist in a project session");
            let expected = !matches!(tab, AppTab::PageManager | AppTab::Characters | AppTab::Terms | AppTab::Notes);
            assert_eq!(tab_visible(tab, true), expected, "{tab:?} in a single-image session");
        }
    }

    #[test]
    fn image_window_title_uses_the_file_name() {
        assert_eq!(image_window_title("1.2.3", Path::new("/home/u/pics/page one.jpg")), "ManhwaStudio v1.2.3 - page one.jpg");
        assert_eq!(image_window_title("1.2.3", Path::new("/")), "ManhwaStudio v1.2.3 - /");
    }

    #[test]
    fn save_as_hotkey_is_registered_before_save() {
        let specs = hotkey_specs();
        assert_eq!(specs[0].id, HOTKEY_SAVE_IMAGE_AS);
        assert_eq!(specs[1].id, HOTKEY_SAVE_IMAGE);
        assert!(specs.iter().all(|spec| spec.scope == HotkeyScopeV2::Global));
    }
}
