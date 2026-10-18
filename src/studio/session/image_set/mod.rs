/*
File: src/studio/session/image_set/mod.rs

Purpose:
The image-set (single-image) strategy of a studio session (plan `dev-docs/single_image_mode_plan.md`,
WP-2.4): the session controller behind «Сохранить» / «Сохранить как», its dialogs, and the small pure
rules the session consults (which tabs exist, the window title, the save hotkeys). `StudioApp` never
reaches in here directly: `SessionMode::ImageSet` (`src/studio/session/mod.rs`) dispatches to it.

Key structures:
- `ImageSetSession` (native: `controller.rs`; wasm: an uninhabited stub): the payload of
  `SessionMode::ImageSet`, built only for `SessionKind::SingleImage`.
- `SaveParts`: the borrows of `StudioApp` a save tick needs.
- `SaveOutcome`: what the session must do after a save step that was started from the exit dialog.
- `ExitChoice`: the single-image exit dialog's answer.

Key functions:
- `tab_visible()`: the tab filter of the mode.
- `image_window_title()`: the studio window title of an image-set session (first file + count of
  the others; shared with the window host, `src/shell/target.rs`).
- `input_declarations()`: the save / save-as commands (Ctrl+S / Ctrl+Shift+S), declared only in
  a single-image session.

Notes:
The save machinery is native-only (it drives `TypingTabState::*_flatten_to_file`, which does not
exist on wasm). The web build never opens an image (plan D10), so there the controller is an
uninhabited enum: `SessionMode::ImageSet` can never be built and its methods are provably
unreachable, which keeps `src/studio` free of `cfg` attributes.
*/

#[cfg(not(target_arch = "wasm32"))]
mod controller;
mod dialogs;
#[cfg(not(target_arch = "wasm32"))]
mod machine;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use controller::ImageSetSession;
pub(crate) use dialogs::draw_exit_dialog;
pub(crate) use dialogs::draw_open_incoming_exit_dialog;

use crate::studio::PendingCloseAction;
use crate::models::autosave_gate::AutosaveGate;
use crate::project::ProjectData;
use crate::tabs::AppTab;
use crate::tabs::ps_editor::PsEditorTabState;
use crate::tabs::typing::TypingTabState;
use eframe::egui;
use ms_input::{ActionId, ActionMeta, ActionSpec, Binding, Button, Chord, CommandSpec, Declarations, ModifierKey, Scope};

/// Command of «Сохранить» (Ctrl+S) in a single-image session. Persistence key, never rename.
pub(crate) const HOTKEY_SAVE_IMAGE: ActionId = ActionId("single_image.save");
/// Command of «Сохранить как» (Ctrl+Shift+S) in a single-image session. Persistence key, never
/// rename.
pub(crate) const HOTKEY_SAVE_IMAGE_AS: ActionId = ActionId("single_image.save_as");

/// The `StudioApp` borrows one save tick needs (disjoint fields of the app).
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

/// What `StudioApp` must do after a save started from the exit dialog ended.
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

/// Studio window title of an image-set session: `ManhwaStudio v{version} - {file name}` for one
/// page (the whole path when it has no file-name component); for `count` > 1 pages the name is
/// followed by the number of the others (`single_image.window_title_more`, e.g. `b.png (+2)`).
/// `first` is page 0's file; `version` is the program version (`MS_APP_VERSION`). The one owner
/// of this format: startup (`StartupTarget::window_title`) and the controller (after «Сохранить
/// как» moved the save target) both call it. Native-only like both callers.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub(crate) fn image_window_title(version: &str, first: &std::path::Path, count: usize) -> String {
    let name = first.file_name().map_or_else(|| first.display().to_string(), |name| name.to_string_lossy().into_owned());
    if count <= 1 {
        return format!("ManhwaStudio v{version} - {name}");
    }
    let set_name = tf!("single_image.window_title_more", name = name, more = count - 1);
    format!("ManhwaStudio v{version} - {set_name}")
}

/// The mode's two save commands, session scope. Declared ONLY for a single-image session so a
/// project session neither lists them in Settings nor reacts to them. Modifier sets match
/// exactly, so Ctrl+S never fires «Сохранить как» and Ctrl+Shift+S never fires «Сохранить».
#[must_use]
pub(crate) fn input_declarations() -> Declarations {
    let command = |id: ActionId, title_key: &'static str, buttons: Vec<Button>| {
        ActionSpec::Command(CommandSpec {
            meta: ActionMeta { id, title_key, section_key: "app.hotkey.file_category", scope: Scope::Session, when: Vec::new(), defaults: vec![Binding::command(Chord::new(buttons))] },
            accepts: Vec::new(),
            repeat: false,
        })
    };
    let ctrl = Button::Modifier(ModifierKey::Command);
    let s_key = Button::Key(egui::Key::S);
    Declarations {
        actions: vec![
            command(HOTKEY_SAVE_IMAGE, "app.hotkey.save_image", vec![ctrl.clone(), s_key.clone()]),
            command(HOTKEY_SAVE_IMAGE_AS, "app.hotkey.save_image_as", vec![ctrl, Button::Modifier(ModifierKey::Shift), s_key]),
        ],
        ..Declarations::default()
    }
}

/// Web build stand-in: an UNINHABITED type. The web entry never opens an image (plan D10), so no
/// value can exist and every method is statically unreachable; it exists only so `StudioApp` keeps
/// one `SessionMode::ImageSet(ImageSetSession)` variant on every target.
#[cfg(target_arch = "wasm32")]
pub(crate) enum ImageSetSession {}

#[cfg(target_arch = "wasm32")]
impl ImageSetSession {
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

    pub(crate) fn take_title(&mut self) -> Option<String> {
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
        assert_eq!(image_window_title("1.2.3", Path::new("/home/u/pics/page one.jpg"), 1), "ManhwaStudio v1.2.3 - page one.jpg");
        assert_eq!(image_window_title("1.2.3", Path::new("/"), 1), "ManhwaStudio v1.2.3 - /");
        // A count of 0 cannot name a set; it reads as one page.
        assert_eq!(image_window_title("1.2.3", Path::new("/home/u/a.png"), 0), "ManhwaStudio v1.2.3 - a.png");
    }

    #[test]
    fn image_window_title_of_a_set_names_the_first_file_and_the_others() {
        // The test process installs no catalog, so the template is checked through the embedded
        // English catalog text rather than `tf!`'s output.
        let en: serde_json::Value = serde_json::from_str(include_str!("../../../../crates/ms-i18n/locales/en.json")).expect("en catalog parses");
        let template = en["single_image.window_title_more"].as_str().expect("key exists in en");
        assert_eq!(ms_i18n::interpolate(template, &[("name", &"b.png"), ("more", &2)]), "b.png (+2)");
        let title = image_window_title("1.2.3", Path::new("/home/u/b.png"), 3);
        assert!(title.starts_with("ManhwaStudio v1.2.3 - "), "{title}");
        assert_ne!(title, image_window_title("1.2.3", Path::new("/home/u/b.png"), 1));
    }

    #[test]
    fn the_save_commands_are_session_scoped_with_exact_chords() {
        use ms_input::{ActionKind, Binding, Button, Chord, ModifierKey, Scope};
        let declarations = input_declarations();
        let rows: Vec<(ms_input::ActionId, ActionKind, Scope, Vec<Binding>)> = declarations.actions.iter().map(|spec| (spec.meta().id, spec.kind(), spec.meta().scope, spec.meta().defaults.clone())).collect();
        let ctrl = Button::Modifier(ModifierKey::Command);
        let s_key = Button::Key(egui::Key::S);
        assert_eq!(
            rows,
            vec![
                (HOTKEY_SAVE_IMAGE, ActionKind::Command, Scope::Session, vec![Binding::command(Chord::new([ctrl.clone(), s_key.clone()]))]),
                (HOTKEY_SAVE_IMAGE_AS, ActionKind::Command, Scope::Session, vec![Binding::command(Chord::new([ctrl, Button::Modifier(ModifierKey::Shift), s_key]))]),
            ]
        );
        assert_eq!(HOTKEY_SAVE_IMAGE.0, "single_image.save");
        assert_eq!(HOTKEY_SAVE_IMAGE_AS.0, "single_image.save_as");
    }
}
