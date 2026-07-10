/*
File: src/launcher/state.rs

Purpose:
Shared UI state for the Rust launcher shell.

Main responsibilities:
- define launcher pages up front;
- keep small page-independent labels used by the main menu UI;
- hold non-blocking page transition state for animated page navigation;
- track detached launcher windows that live outside the page stack.
- carry launcher exit intent back to the startup flow.
*/

use crate::launcher::pages::base::PageTransition;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherOutcome {
    OpenProject(OpenProjectSelection),
    StartUpdate,
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
        }
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
