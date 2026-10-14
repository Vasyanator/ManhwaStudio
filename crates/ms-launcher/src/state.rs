/*
File: crates/ms-launcher/src/state.rs

Purpose:
Shared UI state for the Rust launcher shell.

Main responsibilities:
- define launcher pages up front;
- keep small page-independent labels used by the main menu UI;
- hold non-blocking page transition state for animated page navigation;
- own the open / collapsed state of the launcher's secondary windows (New Project, PSD import;
  `ms_widgets::EmbeddedWindowState`).
- carry launcher exit intent back to the startup flow;
- carry the process facts the launcher needs from startup (`LauncherHost`);
- remember which storage-conversion failure notice the main page already dismissed.
- say whether a surface that can create the projects folder is open
  (`project_creator_open`, the edge the settings-warnings recheck of that folder uses).
*/

use crate::pages::base::PageTransition;
use ms_widgets::EmbeddedWindowState;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherPage {
    Main,
    OpenProject,
    ImportChapter,
    ExportChapter,
    Settings,
}

/// The chapter the user chose to open. It carries no resume/fresh choice: the host resumes
/// whenever a `{chapter}_unsaved` folder exists (the Open page deletes it for a fresh open).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenProjectSelection {
    pub project_dir: PathBuf,
    pub title: String,
    pub chapter: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateNotification {
    pub local_version: String,
    pub remote_version: String,
}

/// Why the launcher screen exits, handed to its host through `LauncherApp::take_outcome`
/// (`lib.rs` / `web_entry.rs` route on it). The launcher never starts the studio itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherOutcome {
    OpenProject(OpenProjectSelection),
    StartUpdate,
    /// Open picture files as one image set, pages in this order (the pick, natural-sorted by
    /// file name). Never empty; at least one path was checked to exist and be a file when
    /// picked, the others and decodability are left to the studio open (it lists what it
    /// skipped). Never produced by the wasm build (no picker there).
    OpenImages(Vec<PathBuf>),
}

/// Facts about the running process the launcher cannot derive itself, handed in by
/// `lib.rs` / `web_entry.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LauncherHost {
    /// `--ignore-installed`: the launcher never modifies the OS records of an installed copy;
    /// the System registration tab is read-only and its warnings are silent.
    pub ignore_installed: bool,
    /// This build's version core (`version_format::version_core` of `CARGO_PKG_VERSION`):
    /// the `DisplayVersion` a written Uninstall entry carries.
    pub version_core: &'static str,
}

#[derive(Debug)]
pub struct LauncherState {
    pub current_page: LauncherPage,
    pub page_transition: Option<PageTransition>,
    /// The New Project window (open while collapsed too).
    pub new_project_window_state: EmbeddedWindowState,
    /// The PSD import window (open while collapsed too).
    pub psd_import_window_state: EmbeddedWindowState,
    pub import_popup_open: bool,
    pub main_page_message: Option<String>,
    pub footer_label: String,
    /// Id of the finished storage-conversion job whose failure notice the user dismissed
    /// on the main page (`storage_mode_job` ids are unique per process).
    pub storage_notice_dismissed_job: Option<u64>,
}

impl LauncherState {
    pub fn new() -> Self {
        Self {
            current_page: LauncherPage::Main,
            page_transition: None,
            new_project_window_state: EmbeddedWindowState::new(),
            psd_import_window_state: EmbeddedWindowState::new(),
            import_popup_open: false,
            main_page_message: None,
            footer_label: t!("launcher.about.credits").to_string(),
            storage_notice_dismissed_job: None,
        }
    }

    /// Whether a surface that can create the projects folder is open: the Import page (an
    /// archive import creates `{root}/{title}`), the new-project window or the PSD-import
    /// window (both save a title under the root; a collapsed window is still open). `LauncherApp` rechecks the projects-folder
    /// warning when this turns false.
    #[must_use]
    pub fn project_creator_open(&self) -> bool {
        self.current_page == LauncherPage::ImportChapter || self.new_project_window_state.is_open() || self.psd_import_window_state.is_open()
    }

    pub fn begin_transition(&mut self, target: LauncherPage) {
        if self.current_page == target || self.page_transition.is_some() {
            return;
        }
        self.page_transition = Some(PageTransition::new(self.current_page, target));
    }

    pub fn settle_transition_if_finished(&mut self) {
        let should_finish = self
            .page_transition
            .as_ref()
            .map(PageTransition::is_finished)
            .unwrap_or(false);
        if should_finish && let Some(transition) = self.page_transition.take() {
            self.current_page = transition.target();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LauncherPage, LauncherState};

    #[test]
    fn project_creator_open_covers_import_page_and_creating_windows() {
        let ctx = egui::Context::default();
        let mut state = LauncherState::new();
        assert!(!state.project_creator_open());
        state.current_page = LauncherPage::ImportChapter;
        assert!(state.project_creator_open());
        state.current_page = LauncherPage::Settings;
        assert!(!state.project_creator_open());
        state.new_project_window_state.open(&ctx);
        assert!(state.project_creator_open());
        state.new_project_window_state.collapse();
        assert!(state.project_creator_open(), "a collapsed window is still open");
        state.new_project_window_state.close();
        state.psd_import_window_state.open(&ctx);
        assert!(state.project_creator_open());
        state.psd_import_window_state.close();
        assert!(!state.project_creator_open());
    }
}
