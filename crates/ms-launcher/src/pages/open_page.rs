/*
File: src/launcher/pages/open_page.rs

Purpose:
Working "Open chapter" launcher page backed by on-disk project discovery.

Main responsibilities:
- list titles and chapters from the configured projects root;
- validate the selected chapter in a background thread before opening, together with the
  storage-format probe of its owned documents (committed and `_unsaved` trees);
- offer converting a chapter whose documents are not in the current storage mode's format
  (banner + `launcher-chapter-convert` worker); opening without converting stays allowed;
- recovery banner for a `{chapter}_unsaved` session: "Restore" resumes it, plain "Open"
  discards it; when the validation worker finds a staging document that does not parse
  (`project_scan::damaged_unsaved_documents`), Restore is disabled and the banner names the
  damaged files and offers "Discard session and open" (the same discard path);
- remember the last opened title and per-title last opened chapters in `user_config.json`.

Key structures:
- `OpenPageState`: the page; `ChapterStorage` / `PendingChapterConvert` /
  `ChapterConvertNotice`: the chapter-format probe, the running conversion, its outcome.
- `ChapterStorageNotice`: pure classification of a format report against the current mode.
- `UnsavedDamage`: damaged staging documents of the title's unsaved session.

Key functions:
- `run_validation()`: body of the `launcher-open-validate` worker.
- `discard_unsaved_dir()`: body of the `launcher-open-cleanup-unsaved` worker.

Notes:
All filesystem scans, validation and conversion run in worker threads so the launcher UI
remains responsive. Open/Restore are disabled while a chapter conversion runs, and Convert is
disabled while the process-wide Dev/Prod conversion job (`storage_mode_job`) runs.
*/

use ms_config as config;
use crate::background::NO_MENU_IMGS_MARKER;
use crate::pages::base::{self, PageNavAction};
use crate::state::OpenProjectSelection;
use crate::theme;
use ms_log::runtime_log;
use ms_widgets::WheelComboBox;
use egui::{Align, Layout, RichText, Ui};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;
use ms_docstore::{ChapterFormatReport, DocFormat};
use ms_project::project_scan::{self, DamagedStagingDoc};
use ms_thread as thread;
use ms_settings_ui::storage_mode_job::{begin_chapter_conversion, failed_document_path, ChapterConversionLease};

const LAST_OPEN_TITLE_KEY: &str = "open_page_last_title";
const LAST_OPEN_CHAPTERS_BY_TITLE_KEY: &str = "open_page_last_chapters_by_title";
const LEGACY_LAST_OPEN_CHAPTER_KEY: &str = "open_page_last_chapter";
/// Width of the notice banners, matching the combo boxes above them.
const NOTICE_WIDTH: f32 = 432.0;
/// How many failed/unreadable document paths a notice lists before summarizing the rest.
const MAX_LISTED_PATHS: usize = 4;
/// Repaint interval while a conversion (chapter or process-wide) is running.
const CONVERT_REPAINT_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub struct OpenPageState {
    projects_root: PathBuf,
    titles: Vec<String>,
    chapters: Vec<String>,
    selected_title: Option<String>,
    selected_chapter: Option<String>,
    /// Chapter name for which an `_unsaved` folder was detected (if any).
    unsaved_chapter: Option<String>,
    /// Damaged staging documents of that unsaved session, from the last validation; only
    /// honoured while its `chapter_dir` still names the detected unsaved chapter.
    unsaved_damage: Option<UnsavedDamage>,
    /// Whether the selected title is used as menu decoration (i.e. the
    /// `no_menu_imgs` marker file is absent from its folder).
    use_as_menu_decoration: bool,
    status: OpenPageStatus,
    pending_refresh: Option<Receiver<OpenPageRefreshResult>>,
    pending_validation: Option<Receiver<OpenPageValidationResult>>,
    pending_open: Option<Receiver<Result<OpenProjectSelection, String>>>,
    /// Storage-format probe of the last validated chapter (see `ChapterStorage`).
    storage: Option<ChapterStorage>,
    /// The running chapter conversion, if any (at most one per page).
    pending_convert: Option<PendingChapterConvert>,
    /// Outcome of the last chapter conversion, shown while its chapter stays selected.
    convert_notice: Option<ChapterConvertNotice>,
    last_open_selection: LastOpenSelection,
}

/// Storage-format probe of one chapter, produced by the validation worker.
#[derive(Debug)]
struct ChapterStorage {
    /// The committed chapter directory the report belongs to.
    project_dir: PathBuf,
    /// Formats of its owned documents in both trees.
    report: ChapterFormatReport,
}

/// Staging documents of an unsaved session that exist but do not parse. Such a session
/// cannot be restored (every page load and save would fail); it can only be discarded.
#[derive(Debug)]
struct UnsavedDamage {
    /// The committed chapter directory whose `{chapter}_unsaved` tree was probed.
    chapter_dir: PathBuf,
    /// Never empty.
    docs: Vec<DamagedStagingDoc>,
}

/// Progress/result messages of the `launcher-chapter-convert` worker.
#[derive(Debug)]
enum ChapterConvertMessage {
    /// `done` of `total` documents processed.
    Progress { done: usize, total: usize },
    /// The conversion ended; `failed` lists `"<document>: <error>"` per failed document.
    Finished { converted: usize, failed: Vec<String> },
}

/// A chapter conversion running on its worker.
#[derive(Debug)]
struct PendingChapterConvert {
    project_dir: PathBuf,
    rx: Receiver<ChapterConvertMessage>,
    done: usize,
    total: usize,
}

/// What the page reports after a chapter conversion ended, for `project_dir` only.
#[derive(Debug)]
enum ChapterConvertNotice {
    /// Every document is in the target format now.
    Converted { project_dir: PathBuf },
    /// Some documents failed (they stay in their previous format and keep working), or the
    /// worker could not run; `lines` are shown under the banner.
    Failed { project_dir: PathBuf, headline: String, lines: Vec<String> },
}

impl ChapterConvertNotice {
    fn project_dir(&self) -> &Path {
        match self {
            Self::Converted { project_dir } | Self::Failed { project_dir, .. } => project_dir,
        }
    }
}

/// Combined format of a chapter's existing owned documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChapterStorageFormat {
    Json,
    Db,
    /// Some documents are JSON, others SQLite.
    Mixed,
}

/// What the format banner must say about a chapter relative to the current mode.
#[derive(Debug, PartialEq, Eq)]
enum ChapterStorageNotice {
    /// Every existing document is already in the target format (or there are none).
    InSync,
    /// Some document's format could not be determined: warn, offer no conversion.
    Unreadable(Vec<PathBuf>),
    /// Documents exist in another format: offer converting to the target.
    Mismatch(ChapterStorageFormat),
}

/// Classifies `report` against the current mode's `target` format. Unreadable documents win
/// over a mismatch: converting around a broken document is never offered.
fn classify_chapter_storage(report: &ChapterFormatReport, target: DocFormat) -> ChapterStorageNotice {
    if !report.unreadable.is_empty() {
        return ChapterStorageNotice::Unreadable(report.unreadable.iter().map(|(path, _)| path.clone()).collect());
    }
    if !report.needs_conversion(target) {
        return ChapterStorageNotice::InSync;
    }
    let mut formats = report.committed.iter().chain(&report.staging).map(|(_, format)| *format);
    // `needs_conversion` with no unreadable entry implies at least one existing document.
    let first = formats.next().unwrap_or(target);
    let combined = if formats.all(|format| format == first) {
        match first {
            DocFormat::Json => ChapterStorageFormat::Json,
            DocFormat::Db => ChapterStorageFormat::Db,
        }
    } else {
        ChapterStorageFormat::Mixed
    };
    ChapterStorageNotice::Mismatch(combined)
}

/// Joins at most `MAX_LISTED_PATHS` of `lines` with newlines, summarizing the rest.
fn listed_lines(lines: &[String]) -> String {
    let mut text = lines.iter().take(MAX_LISTED_PATHS).cloned().collect::<Vec<_>>().join("\n");
    if lines.len() > MAX_LISTED_PATHS {
        text.push('\n');
        text.push_str(&tf!("launcher.open_page.storage_more_files", count = lines.len() - MAX_LISTED_PATHS));
    }
    text
}

/// Localized name of the storage mode whose documents have `format`.
fn mode_name(format: DocFormat) -> String {
    match format {
        DocFormat::Db => t!("launcher.open_page.storage_mode_prod").to_string(),
        DocFormat::Json => t!("launcher.open_page.storage_mode_dev").to_string(),
    }
}

/// Localized name of a chapter's combined document format.
fn chapter_format_name(format: ChapterStorageFormat) -> String {
    match format {
        ChapterStorageFormat::Json => t!("launcher.open_page.storage_format_json").to_string(),
        ChapterStorageFormat::Db => t!("launcher.open_page.storage_format_db").to_string(),
        ChapterStorageFormat::Mixed => t!("launcher.open_page.storage_format_mixed").to_string(),
    }
}

/// Body of the `launcher-chapter-convert` worker: converts the chapter at `project_dir` to
/// `target` in both trees, streaming progress and the final summary through `tx`. A closed
/// receiver (the page went away) does not stop the conversion. `lease` registers the run in
/// the process-wide storage-mode job slot (a global Dev/Prod switch cannot start meanwhile);
/// it is released BEFORE the result is sent, so the page never sees a finished conversion
/// that still blocks the switch.
fn run_chapter_conversion(project_dir: &Path, target: DocFormat, tx: &Sender<ChapterConvertMessage>, lease: ChapterConversionLease) {
    runtime_log::log_info(format!("[launcher-open] converting chapter '{}' to .{}", project_dir.display(), target.extension()));
    let outcome = project_scan::convert_chapter_storage(project_dir, target, &|done, total| {
        if tx.send(ChapterConvertMessage::Progress { done, total }).is_err() {
            runtime_log::log_warn("[launcher-open] chapter conversion progress has no receiver; continuing");
        }
    });
    drop(lease);
    // The docstore reports failed documents by stem; show the file the user has to repair.
    let failed: Vec<String> = outcome.failed.iter().map(|(path, err)| format!("{}: {err}", failed_document_path(path, target).display())).collect();
    runtime_log::log_info(format!(
        "[launcher-open] chapter '{}' conversion finished: converted={}, skipped={}, failed={}",
        project_dir.display(),
        outcome.converted,
        outcome.skipped,
        failed.len()
    ));
    if let Err(err) = tx.send(ChapterConvertMessage::Finished { converted: outcome.converted, failed }) {
        runtime_log::log_warn(format!("[launcher-open] failed to send chapter conversion result: {err}"));
    }
}

#[derive(Debug)]
enum OpenPageStatus {
    Loading,
    RefreshError(String),
    Empty(String),
    Validating,
    Opening,
    Ready { image_count: usize },
    Invalid(String),
}

#[derive(Debug)]
struct OpenPageRefreshResult {
    titles: Vec<String>,
    selected_title: Option<String>,
    chapters: Vec<String>,
    selected_chapter: Option<String>,
    error_message: Option<String>,
}

#[derive(Debug)]
struct OpenPageValidationResult {
    project_dir: PathBuf,
    state: project_scan::ProjectValidationState,
    /// Formats of the chapter's owned documents (committed and `_unsaved`). Unreadable
    /// entries of a damaged unsaved session are reported by `unsaved_damage` instead.
    storage: ChapterFormatReport,
    /// Damage of the title's unsaved session (which may belong to another chapter).
    unsaved_damage: Option<UnsavedDamage>,
}

/// Validation worker body: validates `project_dir`, probes its storage formats, and, when
/// the title has an unsaved session (`unsaved_chapter_dir` = its committed chapter dir),
/// parses that session's staging documents. Blocking I/O: worker thread only.
fn run_validation(project_dir: PathBuf, unsaved_chapter_dir: Option<PathBuf>) -> OpenPageValidationResult {
    let state = project_scan::validate_project_dir_for_startup(&project_dir);
    let mut storage = project_scan::chapter_storage_report(&project_dir);
    let unsaved_damage = unsaved_chapter_dir.and_then(|chapter_dir| {
        let docs = project_scan::damaged_unsaved_documents(&chapter_dir);
        (!docs.is_empty()).then_some(UnsavedDamage { chapter_dir, docs })
    });
    // A damaged staging `.db` also fails the format sniff; it belongs to the discardable
    // session banner, not to the (non-discardable) chapter-format warning.
    // (`unreadable` holds extension-less document stems; the damage holds file paths.)
    if let Some(damage) = &unsaved_damage {
        storage.unreadable.retain(|(stem, _)| !damage.docs.iter().any(|doc| doc.path.with_extension("") == *stem));
    }
    OpenPageValidationResult { project_dir, state, storage, unsaved_damage }
}

/// Discard worker body: removes the unsaved session directory `unsaved_dir` (absent is
/// fine). Returns the localized error on failure.
fn discard_unsaved_dir(unsaved_dir: &Path) -> Result<(), String> {
    if !unsaved_dir.exists() {
        return Ok(());
    }
    fs::remove_dir_all(unsaved_dir)
        .map_err(|err| tf!("launcher.open_page.delete_temp_chapter_error", unsaved_dir = unsaved_dir.display(), err = err))
}

#[derive(Debug, Clone, Default)]
struct LastOpenSelection {
    title: String,
    chapters_by_title: HashMap<String, String>,
    legacy_chapter: String,
}

impl LastOpenSelection {
    fn chapter_for_title(&self, title: &str) -> Option<String> {
        self.chapters_by_title
            .get(title)
            .filter(|chapter| !chapter.is_empty())
            .cloned()
            .or_else(|| {
                (title == self.title && !self.legacy_chapter.is_empty())
                    .then(|| self.legacy_chapter.clone())
            })
    }
}

impl OpenPageState {
    pub fn new(projects_root: PathBuf, user_settings: &Value) -> Self {
        let last_open_selection = read_last_open_selection(user_settings);
        let mut state = Self {
            projects_root,
            titles: Vec::new(),
            chapters: Vec::new(),
            selected_title: None,
            selected_chapter: None,
            unsaved_chapter: None,
            unsaved_damage: None,
            use_as_menu_decoration: true,
            status: OpenPageStatus::Loading,
            pending_refresh: None,
            pending_validation: None,
            pending_open: None,
            storage: None,
            pending_convert: None,
            convert_notice: None,
            last_open_selection,
        };
        state.start_refresh(None);
        state
    }

    pub fn show(&mut self, ui: &mut Ui) -> Option<PageNavAction> {
        self.poll_refresh();
        self.poll_validation();
        self.poll_convert();
        let pending_open_action = self.poll_open();
        let mut ui_action = None;

        if let Some(back_action) = base::show_page_shell(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.10).max(12.0));
                ui.allocate_ui_with_layout(
                    egui::vec2(528.0, 0.0),
                    Layout::top_down(Align::Min),
                    |ui| {
                        theme::card_frame().show(ui, |ui| {
                            ui.set_max_width(480.0);
                            ui.vertical(|ui| {
                                ui.label(RichText::new(t!("launcher.open_page.heading")).size(24.0).strong());
                                ui.add_space(14.0);

                                ui.label(theme::status(t!("launcher.open_page.title_label"), theme::TEXT_MUTED));
                                let mut title_changed = false;
                                ui.scope(|ui| {
                                    ui.set_style(theme::combo_box_style(ui.style().as_ref()));
                                    WheelComboBox::from_id_salt("launcher_open_title")
                                        .width(432.0)
                                        .selected_text(
                                            self.selected_title.as_deref().unwrap_or("—"),
                                        )
                                        .popup_style(theme::combo_popup_style())
                                        .show_ui(ui, |ui| {
                                            for title in &self.titles {
                                                if ui
                                                    .selectable_value(
                                                        &mut self.selected_title,
                                                        Some(title.clone()),
                                                        title,
                                                    )
                                                    .changed()
                                                {
                                                    title_changed = true;
                                                }
                                            }
                                        });
                                });
                                if title_changed {
                                    self.refresh_unsaved_detection();
                                    self.refresh_menu_decoration_flag();
                                    self.start_refresh(self.selected_title.clone());
                                }

                                let mut decoration = self.use_as_menu_decoration;
                                if ui
                                    .add_enabled(
                                        self.selected_title.is_some(),
                                        egui::Checkbox::new(
                                            &mut decoration,
                                            t!("launcher.open_page.use_title_as_decor_check"),
                                        ),
                                    )
                                    .changed()
                                {
                                    self.set_menu_decoration(decoration);
                                }

                                ui.add_space(10.0);
                                ui.label(theme::status(t!("launcher.open_page.chapter_label"), theme::TEXT_MUTED));
                                let mut chapter_changed = false;
                                ui.scope(|ui| {
                                    ui.set_style(theme::combo_box_style(ui.style().as_ref()));
                                    WheelComboBox::from_id_salt("launcher_open_chapter")
                                        .width(432.0)
                                        .selected_text(
                                            self.selected_chapter.as_deref().unwrap_or("—"),
                                        )
                                        .popup_style(theme::combo_popup_style())
                                        .show_ui(ui, |ui| {
                                            for chapter in &self.chapters {
                                                if ui
                                                    .selectable_value(
                                                        &mut self.selected_chapter,
                                                        Some(chapter.clone()),
                                                        chapter,
                                                    )
                                                    .changed()
                                                {
                                                    chapter_changed = true;
                                                }
                                            }
                                        });
                                });
                                if chapter_changed {
                                    self.start_validation_for_current_selection();
                                }

                                // Recovery banner: shown when an _unsaved folder is detected.
                                if let Some(unsaved_name) = self.unsaved_chapter.clone() {
                                    ui.add_space(10.0);
                                    let damaged: Option<Vec<String>> = self.current_unsaved_damage().map(|damage| {
                                        damage.docs.iter().map(|doc| format!("{} ({})", doc.path.display(), doc.cause)).collect()
                                    });
                                    // Restore also waits for the validation that probes the
                                    // session, so a damaged session is never resumed.
                                    let restore = theme::NoticeButton {
                                        label: t!("launcher.open_page.restore_button"),
                                        enabled: self.pending_convert.is_none() && self.pending_validation.is_none() && damaged.is_none(),
                                    };
                                    let text = match &damaged {
                                        Some(lines) => format!(
                                            "{}\n{}\n{}",
                                            tf!("launcher.open_page.unsaved_session_damaged_label", unsaved_name = unsaved_name, count = lines.len()),
                                            listed_lines(lines),
                                            t!("launcher.open_page.unsaved_session_damaged_hint")
                                        ),
                                        None => tf!("launcher.open_page.unsaved_session_label", unsaved_name = unsaved_name),
                                    };
                                    if theme::notice_banner(
                                        ui,
                                        "launcher_open_unsaved_notice",
                                        NOTICE_WIDTH,
                                        &text,
                                        Some(restore),
                                    ) {
                                        let title = self.selected_title.clone().unwrap_or_default();
                                        let selection = OpenProjectSelection {
                                            project_dir: self.projects_root.join(&title).join(&unsaved_name),
                                            title,
                                            chapter: unsaved_name,
                                            resume_unsaved: true,
                                        };
                                        ui_action = Some(PageNavAction::OpenProject(selection));
                                    }
                                    if damaged.is_some() {
                                        ui.add_space(6.0);
                                        let can_open = self.can_open();
                                        ui.push_id("launcher_open_discard_unsaved", |ui| {
                                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                                if theme::launcher_button(ui, t!("launcher.open_page.discard_unsaved_open_button"), egui::vec2(260.0, 32.0), can_open).clicked() {
                                                    ui_action = self.start_open_current_selection();
                                                }
                                            });
                                        });
                                    }
                                }

                                self.show_storage_notice(ui);

                                ui.add_space(12.0);
                                if let Some(project_dir) = self.selected_project_dir() {
                                    ui.label(theme::footer(&project_dir.display().to_string()));
                                } else {
                                    ui.label(theme::footer(t!("launcher.common.select_title_chapter_error")));
                                }

                                ui.add_space(8.0);
                                show_status(ui, &self.status);

                                ui.add_space(18.0);
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    let can_open = self.can_open();
                                    if theme::launcher_button(
                                        ui,
                                        t!("launcher.open_page.open_button"),
                                        egui::vec2(118.0, 36.0),
                                        can_open,
                                    )
                                    .clicked()
                                    {
                                        ui_action = self.start_open_current_selection();
                                    }
                                    if theme::launcher_button(
                                        ui,
                                        t!("launcher.common.refresh_button"),
                                        egui::vec2(118.0, 36.0),
                                        true,
                                    )
                                    .clicked()
                                    {
                                        self.start_refresh(self.selected_title.clone());
                                    }
                                });
                            });
                        });
                    },
                );
            });
        }) {
            ui_action = Some(back_action);
        }

        pending_open_action.or(ui_action)
    }

    pub fn set_projects_root(&mut self, projects_root: PathBuf) {
        if self.projects_root == projects_root {
            return;
        }

        self.projects_root = projects_root;
        self.titles.clear();
        self.chapters.clear();
        self.selected_title = None;
        self.selected_chapter = None;
        self.unsaved_chapter = None;
        self.unsaved_damage = None;
        self.use_as_menu_decoration = true;
        self.pending_validation = None;
        self.pending_open = None;
        self.start_refresh(None);
    }

    fn current_selection(&self) -> Option<OpenProjectSelection> {
        let title = self.selected_title.clone()?;
        let chapter = self.selected_chapter.clone()?;
        Some(OpenProjectSelection {
            project_dir: self.projects_root.join(&title).join(&chapter),
            title,
            chapter,
            resume_unsaved: false,
        })
    }

    fn refresh_unsaved_detection(&mut self) {
        self.unsaved_chapter = self
            .selected_title
            .as_deref()
            .and_then(|title| project_scan::find_unsaved_chapter(&self.projects_root, title));
    }

    /// Path to the `no_menu_imgs` marker file for the selected title, if any.
    fn menu_decoration_marker_path(&self) -> Option<PathBuf> {
        let title = self.selected_title.as_ref()?;
        Some(self.projects_root.join(title).join(NO_MENU_IMGS_MARKER))
    }

    /// Sync the checkbox state with the on-disk marker for the selected title.
    fn refresh_menu_decoration_flag(&mut self) {
        self.use_as_menu_decoration = self
            .menu_decoration_marker_path()
            .map(|marker| !marker.exists())
            .unwrap_or(true);
    }

    /// Create or remove the `no_menu_imgs` marker so the title is (de)selected
    /// for the menu background pool.
    fn set_menu_decoration(&mut self, use_as_decoration: bool) {
        self.use_as_menu_decoration = use_as_decoration;
        let Some(marker) = self.menu_decoration_marker_path() else {
            return;
        };

        let result = if use_as_decoration {
            match fs::remove_file(&marker) {
                Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
                other => other,
            }
        } else if marker.exists() {
            Ok(())
        } else {
            fs::write(&marker, b"")
        };

        if let Err(err) = result {
            runtime_log::log_warn(format!(
                "[launcher-open] failed to update menu decoration marker '{}': {}",
                marker.display(),
                err
            ));
        }
    }

    fn can_open(&self) -> bool {
        self.pending_open.is_none()
            && self.pending_convert.is_none()
            && matches!(self.status, OpenPageStatus::Ready { .. })
            && self.selected_project_dir().is_some()
    }

    fn selected_project_dir(&self) -> Option<PathBuf> {
        self.current_selection()
            .map(|selection| selection.project_dir)
    }

    /// The detected unsaved session's committed chapter dir (`title/{chapter}`), if any.
    fn current_unsaved_chapter_dir(&self) -> Option<PathBuf> {
        let title = self.selected_title.as_ref()?;
        let unsaved_chapter = self.unsaved_chapter.as_ref()?;
        Some(self.projects_root.join(title).join(unsaved_chapter))
    }

    /// Damage found by the last validation, if it still concerns the detected session.
    fn current_unsaved_damage(&self) -> Option<&UnsavedDamage> {
        let chapter_dir = self.current_unsaved_chapter_dir()?;
        self.unsaved_damage.as_ref().filter(|damage| damage.chapter_dir == chapter_dir)
    }

    fn current_unsaved_dir(&self) -> Option<PathBuf> {
        let title = self.selected_title.as_ref()?;
        let unsaved_chapter = self.unsaved_chapter.as_ref()?;
        Some(
            self.projects_root
                .join(title)
                .join(format!("{unsaved_chapter}_unsaved")),
        )
    }

    fn start_refresh(&mut self, preferred_title_override: Option<String>) {
        self.status = OpenPageStatus::Loading;
        self.pending_validation = None;

        let projects_root = self.projects_root.clone();
        let preferred_title = preferred_title_override
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| self.last_open_selection.title.clone());
        let last_open_selection = self.last_open_selection.clone();

        let (tx, rx) = mpsc::channel();
        self.pending_refresh = Some(rx);
        let spawn_result = thread::Builder::new()
            .name("launcher-open-refresh".to_string())
            .spawn(move || {
                let result =
                    build_refresh_result(&projects_root, &preferred_title, &last_open_selection);
                if let Err(err) = tx.send(result) {
                    runtime_log::log_warn(format!(
                        "[launcher-open] failed to send refresh result: {}",
                        err
                    ));
                }
            });

        if let Err(err) = spawn_result {
            self.pending_refresh = None;
            self.status = OpenPageStatus::RefreshError(tf!("launcher.open_page.start_refresh_error", err = err));
        }
    }

    fn poll_refresh(&mut self) {
        let mut should_clear = false;
        if let Some(rx) = &self.pending_refresh {
            match rx.try_recv() {
                Ok(result) => {
                    should_clear = true;
                    self.titles = result.titles;
                    self.selected_title = result.selected_title;
                    self.chapters = result.chapters;
                    self.selected_chapter = result.selected_chapter;

                    self.status = if let Some(error_message) = result.error_message {
                        OpenPageStatus::RefreshError(error_message)
                    } else if self.titles.is_empty() {
                        OpenPageStatus::Empty(t!("launcher.open_page.no_projects_hint").to_string())
                    } else if self.chapters.is_empty() {
                        OpenPageStatus::Empty(t!("launcher.open_page.no_chapters_for_title").to_string())
                    } else {
                        OpenPageStatus::Validating
                    };

                    self.refresh_unsaved_detection();
                    self.refresh_menu_decoration_flag();

                    if self.selected_project_dir().is_some() {
                        self.start_validation_for_current_selection();
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    should_clear = true;
                    self.status = OpenPageStatus::RefreshError(
                        t!("launcher.open_page.refresh_failed").to_string(),
                    );
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if should_clear {
            self.pending_refresh = None;
        }
    }

    fn start_validation_for_current_selection(&mut self) {
        let Some(selection) = self.current_selection() else {
            self.status = OpenPageStatus::Empty(t!("launcher.common.select_title_chapter_error").to_string());
            return;
        };

        self.status = OpenPageStatus::Validating;
        self.storage = None;
        self.unsaved_damage = None;
        let project_dir = selection.project_dir.clone();
        let unsaved_chapter_dir = self.current_unsaved_chapter_dir();
        let (tx, rx) = mpsc::channel();
        self.pending_validation = Some(rx);
        let spawn_result = thread::Builder::new()
            .name("launcher-open-validate".to_string())
            .spawn(move || {
                if let Err(err) = tx.send(run_validation(project_dir, unsaved_chapter_dir)) {
                    runtime_log::log_warn(format!(
                        "[launcher-open] failed to send validation result: {}",
                        err
                    ));
                }
            });

        if let Err(err) = spawn_result {
            self.pending_validation = None;
            self.status =
                OpenPageStatus::Invalid(tf!("launcher.open_page.start_check_error", err = err));
        }
    }

    fn start_open_current_selection(&mut self) -> Option<PageNavAction> {
        let Some(selection) = self.current_selection() else {
            self.status = OpenPageStatus::Empty(t!("launcher.common.select_title_chapter_error").to_string());
            return None;
        };

        let Some(unsaved_dir) = self.current_unsaved_dir() else {
            return Some(PageNavAction::OpenProject(selection));
        };

        self.status = OpenPageStatus::Opening;
        let (tx, rx) = mpsc::channel();
        self.pending_open = Some(rx);
        let spawn_result = thread::Builder::new()
            .name("launcher-open-cleanup-unsaved".to_string())
            .spawn(move || {
                match discard_unsaved_dir(&unsaved_dir) {
                    Ok(()) => {
                        runtime_log::log_info(format!(
                            "[launcher-open] deleted stale unsaved chapter '{}'",
                            unsaved_dir.display()
                        ));
                        if let Err(err) = tx.send(Ok(selection)) {
                            runtime_log::log_warn(format!(
                                "[launcher-open] failed to send open result: {}",
                                err
                            ));
                        }
                    }
                    Err(message) => {
                        runtime_log::log_error(format!(
                            "[launcher-open] failed to delete stale unsaved chapter '{}': {}",
                            unsaved_dir.display(),
                            message
                        ));
                        if let Err(err) = tx.send(Err(message)) {
                            runtime_log::log_warn(format!(
                                "[launcher-open] failed to send open error: {}",
                                err
                            ));
                        }
                    }
                }
            });

        if let Err(err) = spawn_result {
            self.pending_open = None;
            self.status = OpenPageStatus::Invalid(tf!("launcher.open_page.start_cleanup_error", err = err));
        }

        None
    }

    fn poll_validation(&mut self) {
        let mut should_clear = false;
        if let Some(rx) = &self.pending_validation {
            match rx.try_recv() {
                Ok(result) => {
                    should_clear = true;
                    if self.selected_project_dir().as_ref() == Some(&result.project_dir) {
                        self.status = match result.state {
                            project_scan::ProjectValidationState::Valid { image_count } => {
                                OpenPageStatus::Ready { image_count }
                            }
                            project_scan::ProjectValidationState::Invalid { message } => {
                                OpenPageStatus::Invalid(message)
                            }
                        };
                        self.storage = Some(ChapterStorage { project_dir: result.project_dir, report: result.storage });
                        self.unsaved_damage = result.unsaved_damage;
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    should_clear = true;
                    self.status = OpenPageStatus::Invalid(
                        t!("launcher.open_page.check_chapter_failed").to_string(),
                    );
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if should_clear {
            self.pending_validation = None;
        }
    }

    /// Re-runs validation (and the storage-format probe) of the selected chapter, e.g. after
    /// the storage mode changed so the format banner compares against the new mode. No-op
    /// while a refresh or an open is in flight (both end in a validation of their own).
    pub fn revalidate_selection(&mut self) {
        if self.pending_refresh.is_some() || self.pending_open.is_some() || self.current_selection().is_none() {
            return;
        }
        self.start_validation_for_current_selection();
    }

    /// Draws the chapter-format banner of the selected chapter: conversion progress while
    /// the worker runs; otherwise, for a chapter that validated as openable, a warning for
    /// unreadable documents or the Convert offer when its documents are not in the current
    /// mode's format; then the outcome of the last conversion of this chapter.
    fn show_storage_notice(&mut self, ui: &mut Ui) {
        let Some(project_dir) = self.selected_project_dir() else { return };
        let convert_label = t!("launcher.open_page.convert_button");
        if let Some(pending) = &self.pending_convert {
            ui.add_space(10.0);
            let text = tf!("launcher.open_page.storage_converting_status", done = pending.done, total = pending.total);
            theme::notice_banner(ui, "launcher_open_storage_notice", NOTICE_WIDTH, &text, Some(theme::NoticeButton { label: convert_label, enabled: false }));
            ui.ctx().request_repaint_after(CONVERT_REPAINT_INTERVAL);
            return;
        }
        let ready = matches!(self.status, OpenPageStatus::Ready { .. });
        let notice = self
            .storage
            .as_ref()
            .filter(|storage| ready && storage.project_dir == project_dir)
            .map(|storage| classify_chapter_storage(&storage.report, ms_docstore::default_format()));
        match notice {
            None | Some(ChapterStorageNotice::InSync) => {}
            Some(ChapterStorageNotice::Unreadable(paths)) => {
                ui.add_space(10.0);
                let lines: Vec<String> = paths.iter().map(|path| path.display().to_string()).collect();
                let text = format!("{}\n{}", tf!("launcher.open_page.storage_unreadable_label", count = paths.len()), listed_lines(&lines));
                theme::notice_banner(ui, "launcher_open_storage_notice", NOTICE_WIDTH, &text, None);
            }
            Some(ChapterStorageNotice::Mismatch(chapter_format)) => {
                ui.add_space(10.0);
                // A chapter conversion must not interleave with the process-wide Dev/Prod
                // job: that job flips `default_format()`, i.e. this conversion's target.
                let global_busy = ms_settings_ui::storage_mode_job::conversion_job_state().is_running();
                let text = tf!(
                    "launcher.open_page.storage_mismatch_label",
                    chapter_format = chapter_format_name(chapter_format),
                    mode = mode_name(ms_docstore::default_format())
                );
                let button = theme::NoticeButton { label: convert_label, enabled: !global_busy && self.pending_open.is_none() };
                if theme::notice_banner(ui, "launcher_open_storage_notice", NOTICE_WIDTH, &text, Some(button)) {
                    self.start_chapter_conversion(project_dir.clone());
                }
                if global_busy {
                    ui.label(theme::status(t!("launcher.open_page.storage_wait_global"), theme::TEXT_MUTED));
                    ui.ctx().request_repaint_after(CONVERT_REPAINT_INTERVAL);
                }
            }
        }
        match self.convert_notice.as_ref().filter(|notice| notice.project_dir() == project_dir) {
            Some(ChapterConvertNotice::Converted { .. }) => {
                ui.label(theme::status(t!("launcher.open_page.storage_converted_status"), theme::STATUS_SUCCESS));
            }
            Some(ChapterConvertNotice::Failed { headline, lines, .. }) => {
                let text = if lines.is_empty() { headline.clone() } else { format!("{headline}\n{}", listed_lines(lines)) };
                ui.label(theme::status(&text, theme::STATUS_ERROR));
            }
            None => {}
        }
    }

    /// Spawns the `launcher-chapter-convert` worker converting the chapter at `project_dir`
    /// (both trees) into the current mode's format. Open/Restore stay disabled until
    /// `poll_convert` sees it finish.
    fn start_chapter_conversion(&mut self, project_dir: PathBuf) {
        if self.pending_convert.is_some() {
            return;
        }
        self.convert_notice = None;
        // Register first, then read the target: once registered, a global switch cannot
        // flip `default_format()` until this conversion ends.
        let lease = match begin_chapter_conversion() {
            Ok(lease) => lease,
            Err(err) => {
                runtime_log::log_warn(format!("[launcher-open] chapter conversion of '{}' not started: the global storage conversion runs ({err:?})", project_dir.display()));
                self.convert_notice = Some(ChapterConvertNotice::Failed { project_dir, headline: t!("launcher.open_page.storage_wait_global").to_string(), lines: Vec::new() });
                return;
            }
        };
        let target = ms_docstore::default_format();
        let (tx, rx) = mpsc::channel();
        let worker_dir = project_dir.clone();
        // On a spawn failure the closure (and the lease inside it) is dropped: registration released.
        let spawn_result = thread::Builder::new()
            .name("launcher-chapter-convert".to_string())
            .spawn(move || run_chapter_conversion(&worker_dir, target, &tx, lease));
        match spawn_result {
            Ok(_) => self.pending_convert = Some(PendingChapterConvert { project_dir, rx, done: 0, total: 0 }),
            Err(err) => {
                runtime_log::log_error(format!("[launcher-open] failed to start the chapter conversion of '{}': {err}", project_dir.display()));
                self.convert_notice = Some(ChapterConvertNotice::Failed {
                    project_dir,
                    headline: tf!("launcher.open_page.storage_convert_spawn_error", err = err),
                    lines: Vec::new(),
                });
            }
        }
    }

    /// Drains the conversion worker's messages; on completion records the outcome and
    /// re-validates the selection so the banner reflects the new on-disk formats.
    fn poll_convert(&mut self) {
        let Some(pending) = &mut self.pending_convert else { return };
        let mut finished = None;
        loop {
            match pending.rx.try_recv() {
                Ok(ChapterConvertMessage::Progress { done, total }) => {
                    pending.done = done;
                    pending.total = total;
                }
                Ok(ChapterConvertMessage::Finished { converted, failed }) => {
                    finished = Some(if failed.is_empty() {
                        runtime_log::log_info(format!("[launcher-open] chapter conversion done ({converted} document(s))"));
                        ChapterConvertNotice::Converted { project_dir: pending.project_dir.clone() }
                    } else {
                        ChapterConvertNotice::Failed {
                            project_dir: pending.project_dir.clone(),
                            headline: tf!("launcher.open_page.storage_convert_failed", count = failed.len()),
                            lines: failed,
                        }
                    });
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    runtime_log::log_error(format!("[launcher-open] chapter conversion worker of '{}' ended without a result", pending.project_dir.display()));
                    finished = Some(ChapterConvertNotice::Failed {
                        project_dir: pending.project_dir.clone(),
                        headline: t!("launcher.open_page.storage_convert_worker_lost").to_string(),
                        lines: Vec::new(),
                    });
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        if let Some(notice) = finished {
            self.pending_convert = None;
            self.convert_notice = Some(notice);
            self.revalidate_selection();
        }
    }

    fn poll_open(&mut self) -> Option<PageNavAction> {
        let mut should_clear = false;
        let mut action = None;
        if let Some(rx) = &self.pending_open {
            match rx.try_recv() {
                Ok(Ok(selection)) => {
                    should_clear = true;
                    action = Some(PageNavAction::OpenProject(selection));
                }
                Ok(Err(message)) => {
                    should_clear = true;
                    self.status = OpenPageStatus::Invalid(message);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    should_clear = true;
                    self.status =
                        OpenPageStatus::Invalid(t!("launcher.open_page.open_chapter_failed").to_string());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }

        if should_clear {
            self.pending_open = None;
        }

        action
    }
}

pub fn persist_last_selection_values(title: &str, chapter: &str) -> anyhow::Result<()> {
    let mut cfg = config::load_user_config()?;
    cfg.set_path(
        &["General", LAST_OPEN_TITLE_KEY],
        Value::String(title.to_string()),
    )?;

    let mut chapters_by_title = cfg
        .data
        .get("General")
        .and_then(|general| general.get(LAST_OPEN_CHAPTERS_BY_TITLE_KEY))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    chapters_by_title.insert(title.to_string(), Value::String(chapter.to_string()));
    cfg.set_path(
        &["General", LAST_OPEN_CHAPTERS_BY_TITLE_KEY],
        Value::Object(chapters_by_title),
    )?;
    Ok(())
}

fn build_refresh_result(
    projects_root: &std::path::Path,
    preferred_title: &str,
    last_open_selection: &LastOpenSelection,
) -> OpenPageRefreshResult {
    let titles = match project_scan::list_titles(projects_root) {
        Ok(titles) => titles,
        Err(err) if err.kind() == ErrorKind::NotFound => Vec::new(),
        Err(err) => {
            return OpenPageRefreshResult {
                titles: Vec::new(),
                selected_title: None,
                chapters: Vec::new(),
                selected_chapter: None,
                error_message: Some(tf!("launcher.common.read_projects_folder_error", projects_root = projects_root.display(), err = err)),
            };
        }
    };

    let selected_title = if preferred_title.is_empty() {
        titles.first().cloned()
    } else if titles.iter().any(|title| title == preferred_title) {
        Some(preferred_title.to_string())
    } else {
        titles.first().cloned()
    };

    let chapters = selected_title
        .as_ref()
        .map(|title| project_scan::list_chapters(projects_root, title).unwrap_or_default())
        .unwrap_or_default();

    let preferred_chapter = selected_title
        .as_deref()
        .and_then(|title| last_open_selection.chapter_for_title(title));
    let selected_chapter = preferred_chapter
        .filter(|preferred_chapter| chapters.iter().any(|chapter| chapter == preferred_chapter))
        .or_else(|| chapters.first().cloned());

    OpenPageRefreshResult {
        titles,
        selected_title,
        chapters,
        selected_chapter,
        error_message: None,
    }
}

fn read_last_open_selection(user_settings: &Value) -> LastOpenSelection {
    let general = user_settings.get("General").and_then(Value::as_object);
    let title = general
        .and_then(|general| general.get(LAST_OPEN_TITLE_KEY))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string();

    let chapters_by_title = general
        .and_then(|general| general.get(LAST_OPEN_CHAPTERS_BY_TITLE_KEY))
        .and_then(Value::as_object)
        .map(|chapters| {
            chapters
                .iter()
                .filter_map(|(title, chapter)| {
                    let chapter = chapter.as_str()?.trim();
                    (!title.trim().is_empty() && !chapter.is_empty())
                        .then(|| (title.clone(), chapter.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();

    let legacy_chapter = general
        .and_then(|general| general.get(LEGACY_LAST_OPEN_CHAPTER_KEY))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string();

    LastOpenSelection {
        title,
        chapters_by_title,
        legacy_chapter,
    }
}

fn show_status(ui: &mut Ui, status: &OpenPageStatus) {
    match status {
        OpenPageStatus::Loading => {
            ui.label(theme::status(
                t!("launcher.open_page.loading_titles_status"),
                theme::TEXT_MUTED,
            ));
        }
        OpenPageStatus::RefreshError(message) | OpenPageStatus::Invalid(message) => {
            ui.label(theme::status(message, theme::STATUS_ERROR));
        }
        OpenPageStatus::Empty(message) => {
            ui.label(theme::status(message, theme::TEXT_MUTED));
        }
        OpenPageStatus::Validating => {
            ui.label(theme::status(
                t!("launcher.open_page.checking_structure_status"),
                theme::TEXT_MUTED,
            ));
        }
        OpenPageStatus::Opening => {
            ui.label(theme::status(
                t!("launcher.open_page.cleaning_temp_status"),
                theme::TEXT_MUTED,
            ));
        }
        OpenPageStatus::Ready { image_count } => {
            ui.label(theme::status(
                &tf!("launcher.open_page.ready_to_open_status", image_count = image_count),
                theme::STATUS_SUCCESS,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ms_docstore::DocKind;
    use serde_json::json;

    fn report(committed: &[DocFormat], staging: &[DocFormat]) -> ChapterFormatReport {
        ChapterFormatReport {
            committed: committed.iter().map(|format| (DocKind::Layers, *format)).collect(),
            staging: staging.iter().map(|format| (DocKind::Bubbles, *format)).collect(),
            unreadable: Vec::new(),
        }
    }

    #[test]
    fn classifies_chapter_storage_against_the_mode() {
        assert_eq!(classify_chapter_storage(&report(&[], &[]), DocFormat::Db), ChapterStorageNotice::InSync);
        assert_eq!(classify_chapter_storage(&report(&[DocFormat::Db], &[DocFormat::Db]), DocFormat::Db), ChapterStorageNotice::InSync);
        assert_eq!(
            classify_chapter_storage(&report(&[DocFormat::Json, DocFormat::Json], &[]), DocFormat::Db),
            ChapterStorageNotice::Mismatch(ChapterStorageFormat::Json)
        );
        assert_eq!(classify_chapter_storage(&report(&[DocFormat::Db], &[]), DocFormat::Json), ChapterStorageNotice::Mismatch(ChapterStorageFormat::Db));
        // A staging document alone in another format still needs the conversion.
        assert_eq!(
            classify_chapter_storage(&report(&[DocFormat::Db], &[DocFormat::Json]), DocFormat::Db),
            ChapterStorageNotice::Mismatch(ChapterStorageFormat::Mixed)
        );
    }

    #[test]
    fn unreadable_documents_suppress_the_conversion_offer() {
        let mut broken = report(&[DocFormat::Json], &[]);
        broken.unreadable.push((PathBuf::from("/t/ch1/translation_bubbles"), "bad header".to_string()));
        assert_eq!(
            classify_chapter_storage(&broken, DocFormat::Db),
            ChapterStorageNotice::Unreadable(vec![PathBuf::from("/t/ch1/translation_bubbles")])
        );
    }

    #[test]
    fn conversion_worker_reports_progress_and_result() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("title").join("ch1");
        let layers = ms_page_ops::chapter_docs::layers_doc(&dir);
        std::fs::create_dir_all(layers.path_for(DocFormat::Json).parent().expect("parent")).expect("mkdir");
        ms_docstore::write_whole_atomic(&layers, &json!({"pages": []}), DocFormat::Json).expect("write");
        let (tx, rx) = mpsc::channel();
        let lease = begin_chapter_conversion().expect("no global job runs in this test process");
        assert!(ms_settings_ui::storage_mode_job::chapter_conversion_running());
        run_chapter_conversion(&dir, DocFormat::Db, &tx, lease);
        // The registration is released before the result is reported.
        assert!(!ms_settings_ui::storage_mode_job::chapter_conversion_running());
        drop(tx);
        let messages: Vec<ChapterConvertMessage> = rx.iter().collect();
        assert!(matches!(messages.first(), Some(ChapterConvertMessage::Progress { done: 1, total: 4 })));
        match messages.last() {
            Some(ChapterConvertMessage::Finished { converted, failed }) => {
                assert_eq!(*converted, 1);
                assert!(failed.is_empty());
            }
            other => panic!("unexpected last message: {other:?}"),
        }
        assert!(layers.path_for(DocFormat::Db).is_file());
    }

    #[test]
    fn reads_last_chapter_per_title() {
        let selection = read_last_open_selection(&json!({
            "General": {
                "open_page_last_title": "Title B",
                "open_page_last_chapters_by_title": {
                    "Title A": "001",
                    "Title B": "014"
                },
                "open_page_last_chapter": "legacy"
            }
        }));

        assert_eq!(selection.title, "Title B");
        assert_eq!(
            selection.chapter_for_title("Title A").as_deref(),
            Some("001")
        );
        assert_eq!(
            selection.chapter_for_title("Title B").as_deref(),
            Some("014")
        );
    }

    #[test]
    fn falls_back_to_legacy_chapter_only_for_last_title() {
        let selection = read_last_open_selection(&json!({
            "General": {
                "open_page_last_title": "Title B",
                "open_page_last_chapter": "legacy"
            }
        }));

        assert_eq!(
            selection.chapter_for_title("Title B").as_deref(),
            Some("legacy")
        );
        assert_eq!(selection.chapter_for_title("Title A"), None);
    }

    /// A committed chapter `title/ch1` with one page in `src/`; returns (chapter, staging) dirs.
    fn chapter_fixture(root: &Path) -> (PathBuf, PathBuf) {
        let chapter = root.join("title").join("ch1");
        std::fs::create_dir_all(chapter.join(config::SRC_DIR)).expect("mkdir src");
        std::fs::write(chapter.join(config::SRC_DIR).join("000.png"), b"png").expect("write page");
        (chapter, root.join("title").join("ch1_unsaved"))
    }

    /// Writes raw `bytes` as the `format` file of `doc` (creating parent dirs).
    fn put_raw(doc: &ms_docstore::DocRef, format: DocFormat, bytes: &[u8]) -> PathBuf {
        let path = doc.path_for(format);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, bytes).expect("write");
        path
    }

    #[test]
    fn validation_reports_a_damaged_unsaved_session() {
        use ms_page_ops::chapter_docs::{bubbles_doc, layers_doc};
        let tmp = tempfile::tempdir().expect("tempdir");
        let (chapter, staging) = chapter_fixture(tmp.path());
        let layers = put_raw(&layers_doc(&staging), DocFormat::Json, b"");
        let bubbles = put_raw(&bubbles_doc(&staging), DocFormat::Json, b"{ \"pages\": [");
        let result = run_validation(chapter.clone(), Some(chapter.clone()));
        assert!(matches!(result.state, project_scan::ProjectValidationState::Valid { .. }));
        let damage = result.unsaved_damage.expect("damaged session");
        assert_eq!(damage.chapter_dir, chapter);
        let mut paths: Vec<PathBuf> = damage.docs.into_iter().map(|doc| doc.path).collect();
        paths.sort();
        let mut expected = vec![layers, bubbles];
        expected.sort();
        assert_eq!(paths, expected);
    }

    #[test]
    fn damaged_staging_db_moves_from_the_format_warning_to_the_session_banner() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (chapter, staging) = chapter_fixture(tmp.path());
        put_raw(&ms_page_ops::chapter_docs::bubbles_doc(&staging), DocFormat::Db, b"not a sqlite file");
        let result = run_validation(chapter.clone(), Some(chapter));
        assert_eq!(result.unsaved_damage.expect("damaged session").docs.len(), 1);
        assert!(result.storage.unreadable.is_empty(), "{:?}", result.storage.unreadable);
    }

    #[test]
    fn committed_only_damage_is_not_a_discardable_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (chapter, _staging) = chapter_fixture(tmp.path());
        put_raw(&ms_page_ops::chapter_docs::bubbles_doc(&chapter), DocFormat::Db, b"not a sqlite file");
        put_raw(&ms_page_ops::chapter_docs::layers_doc(&chapter), DocFormat::Json, b"");
        let result = run_validation(chapter.clone(), Some(chapter));
        assert!(result.unsaved_damage.is_none());
        // The committed `.db` keeps the existing (non-discardable) format warning.
        assert_eq!(result.storage.unreadable.len(), 1);
        assert!(matches!(classify_chapter_storage(&result.storage, DocFormat::Json), ChapterStorageNotice::Unreadable(_)));
    }

    #[test]
    fn discard_removes_the_unsaved_session_and_tolerates_its_absence() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (_chapter, staging) = chapter_fixture(tmp.path());
        put_raw(&ms_page_ops::chapter_docs::layers_doc(&staging), DocFormat::Json, b"");
        assert_eq!(discard_unsaved_dir(&staging), Ok(()));
        assert!(!staging.exists());
        assert_eq!(discard_unsaved_dir(&staging), Ok(()));
    }
}
