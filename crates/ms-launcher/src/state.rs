/*
File: src/launcher/state.rs

Purpose:
Shared UI state for the Rust launcher shell.

Main responsibilities:
- define launcher pages up front;
- keep small page-independent labels used by the main menu UI;
- hold non-blocking page transition state for animated page navigation;
- track detached launcher windows that live outside the page stack.
- carry launcher exit intent back to the startup flow;
- remember which storage-conversion failure notice the main page already dismissed.
- say whether a surface that can create the projects folder is open
  (`project_creator_open`, the edge the settings-warnings recheck of that folder uses).
*/

use crate::pages::base::PageTransition;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherPage {
    Main,
    OpenProject,
    ImportChapter,
    ExportChapter,
    Settings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenProjectSelection {
    pub project_dir: PathBuf,
    pub title: String,
    pub chapter: String,
    /// When true the project was opened in crash-recovery mode: the `_unsaved` staging
    /// folder was detected and the user chose to resume from it.
    pub resume_unsaved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateNotification {
    pub local_version: String,
    pub remote_version: String,
}

/// Why the launcher closed; `main.rs` / `web_entry.rs` route on it. The launcher never starts
/// the studio itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherOutcome {
    OpenProject(OpenProjectSelection),
    StartUpdate,
    /// Open one picture file in single-image mode. The path was checked to exist and be a
    /// file when picked; decodability is left to the studio loading screen. Never produced by
    /// the wasm build (no picker there).
    OpenImage(PathBuf),
}

#[derive(Debug)]
pub struct LauncherState {
    pub current_page: LauncherPage,
    pub page_transition: Option<PageTransition>,
    pub new_project_window_open: bool,
    pub psd_import_window_open: bool,
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
            new_project_window_open: false,
            psd_import_window_open: false,
            import_popup_open: false,
            main_page_message: None,
            footer_label: t!("launcher.about.credits").to_string(),
            storage_notice_dismissed_job: None,
        }
    }

    /// Whether a surface that can create the projects folder is open: the Import page (an
    /// archive import creates `{root}/{title}`), the new-project window or the PSD-import
    /// window (both save a title under the root). `LauncherApp` rechecks the projects-folder
    /// warning when this turns false.
    #[must_use]
    pub fn project_creator_open(&self) -> bool {
        self.current_page == LauncherPage::ImportChapter || self.new_project_window_open || self.psd_import_window_open
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
        let mut state = LauncherState::new();
        assert!(!state.project_creator_open());
        state.current_page = LauncherPage::ImportChapter;
        assert!(state.project_creator_open());
        state.current_page = LauncherPage::Settings;
        assert!(!state.project_creator_open());
        state.new_project_window_open = true;
        assert!(state.project_creator_open());
        state.new_project_window_open = false;
        state.psd_import_window_open = true;
        assert!(state.project_creator_open());
    }
}
