/*
File: src/studio/session/image_set/controller.rs

Purpose:
`ImageSetSession`, the image-set (single-image) session's save controller (the payload of
`SessionMode::ImageSet`, built only in that mode). It owns the pure `SaveMachine` and performs its side effects: the
native «Сохранить как» file dialog and the folder dialog (`ms_platform::file_dialog::pick_blocking`,
on workers), the commit of a dialog target and the `user_config` quality write (workers), the
per-frame drive of `TypingTabState::prepare_flatten_to_file` for the page the machine names, the
GUI-thread dispatch sequence (PS flush, typing flush, baseline capture, `request_flatten_to_file`),
the per-frame `poll_flatten_to_file`, the window title (computed once after the open, then after a
save moved page 0's file; handed out by `take_title`, the window host sets it), and the UI: the top bar (with the «Сохранить как…» format panel), the
JPEG options, the folder overwrite confirmation and the skipped-files notice.

Key structures:
- `ImageSetSession`

Key functions:
- `ImageSetSession::from_project()`: `Some` for an image-set `ProjectData`.
- `tick()`: call EVERY frame, whatever tab is active (it is also the app's one
  `poll_flatten_to_file` caller, which the typing tab requires).
- `draw_top_bar()` (also the format panel, `draw_format_panel`) / `draw_dialogs()`;
  `request_save*()`, `abandon_pending()`, `take_title()`.
- `spawn_worker()`: every worker of this file (`ms_thread`); a failed spawn is logged and surfaces
  through the poll's worker-lost path.

Notes:
The GUI thread does no file I/O here: the pickers (and the folder's "has entries" probe), the
flatten (compose, digest, encode, atomic write), the commit of a dialog target (after the stage's
files were written; a failure fails those pages) and the quality persistence run on workers; the
GUI thread only snapshots, flushes into the in-memory savers and polls channels. Encoding policy of
a save (plan D7): PNG / WebP drop alpha only when fully opaque, JPEG is flattened over white at the
session quality, the page's source ICC profile is re-embedded. Dirty inputs are defined here
(`edit_stamp`, `deferred_edits`); the machine only compares them. The file picker worker re-asks
with the full name when the chosen format changed the answer's name (extension appended or
replaced) and that file exists, so the native overwrite confirmation always saw the written name;
a folder that already holds entries is confirmed by the machine's `ConfirmFolder` dialog.

The format panel is an `egui::Popup` (`PopupKind::Menu` -> `Order::Foreground`) whose open state is
the machine's `Phase::ChoosingFormat`, not egui memory. It blocks the canvas the same way the exit
dialog does — z-order occlusion in egui's hit-test (an interactable area above the canvas layer
takes the hover / click / drag) — and, being a popup, also sets `Context::any_popup_open`, which the
cleaning and PS-editor canvas gates check on top of `layer_id_at`.
*/

use super::dialogs::{JpegOptionsChoice, draw_folder_overwrite_dialog, draw_jpeg_options_dialog, draw_skipped_notice};
use super::machine::{Commit, FormatNote, Next, PageJob, PageResult, PageSource, SaveError, SaveMachine, SaveTarget, Status, Step, changed_name_needs_confirmation, save_as_filter_extensions, save_as_suggestion};
use super::{SaveOutcome, SaveParts, image_window_title};
use crate::studio::session::DeferredTabEdits;
use crate::studio::PendingCloseAction;
use crate::models::autosave_gate::AutosaveGate;
use crate::project::{ProjectData, SessionKind, SingleImageSession};
use crate::runtime_log;
use crate::tabs::ps_editor::PsEditorTabState;
use crate::tabs::typing::image_encode::{AlphaPolicy, ImageEncoding, ImageSaveFormat};
use crate::tabs::typing::{FlattenOutcome, FlattenReadiness, FlattenToFileRequest, TypingTabState};
use eframe::egui;
use ms_platform::PlatformFeature;
use ms_platform::file_dialog::{self, DialogKind, FileDialogError, FileDialogRequest, FolderTarget, Picked, SaveTarget as DialogTarget};
use ms_thread as thread;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// How long a transient status ("Saved", "Cancelled") stays in the top bar, in seconds.
const TRANSIENT_STATUS_SECONDS: f64 = 5.0;

/// Glyph of the format panel's close button. A literal, not a translation: an icon chosen for its
/// shape, the same `✕` the studio's other small close buttons draw; its meaning is carried by the
/// localized hover text.
const CLOSE_GLYPH: &str = "✕";

/// Persistent id of the format panel popup (named by content, stable across language switches).
const FORMAT_PANEL_ID: &str = "single_image.save_as.format_panel";

/// What the format panel answered this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormatPanelChoice {
    /// Save in this format (the file or folder dialog opens next).
    Format(ImageSaveFormat),
    /// «✕» or Escape: close the panel without saving.
    Cancel,
}

/// Answer of the folder dialog worker: the folder and whether it already holds entries, `None` =
/// cancelled, `Err` = no dialog could be shown.
type FolderAnswer = Result<Option<(FolderTarget, bool)>, FileDialogError>;

/// Save controller of one image-set session (see the file header).
pub(crate) struct ImageSetSession {
    session: Arc<SingleImageSession>,
    machine: SaveMachine,
    /// Result of the file dialog while `Phase::PickingPath` (`Ok(None)` = cancelled, `Err` = no
    /// dialog could be shown).
    picker_rx: Option<Receiver<Result<Option<DialogTarget>, FileDialogError>>>,
    /// Result of the folder dialog while `Phase::PickingFolder`.
    folder_rx: Option<Receiver<FolderAnswer>>,
    /// Result of the commit worker while `Phase::Committing`.
    commit_rx: Option<Receiver<Result<(), FileDialogError>>>,
    /// Result of the last JPEG quality persistence.
    persist_rx: Option<Receiver<Result<(), String>>>,
    /// The status revision being timed and when it was first drawn (app time, seconds).
    status_shown: Option<(u64, f64)>,
    /// A save step produced outside `tick` / `draw_dialogs` (the format panel closed during a
    /// save-then-close, from the top bar or the hotkey), reported by the next `tick`.
    deferred_step: Step,
    /// «Стоп» / «Отмена» was clicked in the top bar; applied by the next `tick`, which has the
    /// dirty inputs the machine needs.
    stop_requested: bool,
    /// The skipped-files notice is still shown (opened once when the open left files out).
    skipped_notice_open: bool,
    /// The window title was computed from the opened pages (the startup title named the request).
    title_synced: bool,
    /// A window title computed by `tick` and not yet taken by `take_title` (the window host sets it).
    pending_title: Option<String>,
}

impl ImageSetSession {
    /// The controller of `project`'s image-set session, or `None` for a project session.
    /// `user_settings` is the startup `user_config` root (JPEG quality default).
    pub(crate) fn from_project(project: &ProjectData, user_settings: &Value) -> Option<Self> {
        let SessionKind::SingleImage(session) = project.session() else {
            return None;
        };
        let pages: Vec<PageSource> = session
            .sources()
            .iter()
            .map(|facts| PageSource {
                source_path: facts.source_path.clone(),
                in_place: ImageSaveFormat::in_place_for(facts).map(|format| SaveTarget { path: facts.source_path.clone(), format }),
                opened_digest: facts.opened_digest,
                animated: facts.source_animated,
            })
            .collect();
        let first = session.first_source();
        runtime_log::log_info(format!(
            "[single_image] session opened.\nFirst source: {}\nPages: {}\nFirst format: {}\nPages with an in-place target: {}\nAnimated pages: {}\nSkipped files: {}",
            first.source_path.display(),
            session.page_count(),
            first.source_format.as_str(),
            pages.iter().filter(|page| page.in_place.is_some()).count(),
            pages.iter().filter(|page| page.animated).count(),
            session.skipped().len()
        ));
        let machine = SaveMachine::new(pages, ms_config::single_image::jpeg_quality_from(user_settings), picked_paths_are_user_files());
        Some(Self {
            skipped_notice_open: !session.skipped().is_empty(),
            session: Arc::clone(session),
            machine,
            picker_rx: None,
            folder_rx: None,
            commit_rx: None,
            persist_rx: None,
            status_shown: None,
            deferred_step: Step::Nothing,
            stop_requested: false,
            title_synced: false,
            pending_title: None,
        })
    }

    /// The image differs from the last fully successful save (plan D6): the edit stamp moved, or
    /// an edit is still deferred in typing or the PS editor.
    pub(crate) fn is_dirty(&self, gate: &AutosaveGate, typing: &TypingTabState, ps_editor: &PsEditorTabState) -> bool {
        self.machine.is_dirty(edit_stamp(gate, typing), deferred_edits(typing, ps_editor))
    }

    /// A file is being written or committed right now (a close must wait for it).
    pub(crate) fn write_in_flight(&self) -> bool {
        self.machine.writing()
    }

    /// «Сохранить» (one page without an in-place target: opens the format panel).
    pub(crate) fn request_save(&mut self) {
        self.machine.request_save();
    }

    /// «Сохранить как»: toggles the format panel under the top-bar button. Closing it during a
    /// save-then-close is reported by the next `tick` (`ctx` schedules that frame).
    pub(crate) fn request_save_as(&mut self, ctx: &egui::Context) {
        let step = self.machine.request_save_as();
        self.defer_step(ctx, step);
    }

    /// «Сохранить» in the exit dialog: save, then `action` (reported by `tick` / `draw_dialogs`).
    pub(crate) fn request_save_then(&mut self, action: PendingCloseAction) {
        self.machine.request_save_then(action);
    }

    /// The window closes without saving: drop every save step that has not started writing. An
    /// open native dialog's answer is ignored (its channel is dropped).
    pub(crate) fn abandon_pending(&mut self) {
        self.machine.abandon_pending();
        self.picker_rx = None;
        self.folder_rx = None;
    }

    /// Per-frame drive. Polls the dialogs and the quality write, applies a requested stop, advances
    /// the page being prepared and dispatches it once its composite inputs are ready, polls the
    /// flatten worker (every frame, as the typing tab requires), starts and polls a commit, keeps
    /// the pending window title in step (`take_title`) and times transient statuses. Returns what the app must do when a
    /// save-then-close ended.
    pub(crate) fn tick(&mut self, ctx: &egui::Context, parts: SaveParts<'_>) -> Option<SaveOutcome> {
        let SaveParts { project, typing, ps_editor, gate } = parts;
        self.poll_persist();
        let deferred = std::mem::replace(&mut self.deferred_step, Step::Nothing);
        let mut step = merge_steps(deferred, self.poll_picker());
        step = merge_steps(step, self.poll_folder_picker());
        if std::mem::take(&mut self.stop_requested) {
            runtime_log::log_info("[single_image] save stopped by the user");
            let stopped = self.machine.stop(edit_stamp(gate, typing), deferred_edits(typing, ps_editor));
            step = merge_steps(step, stopped);
        }
        if let Some(job) = self.machine.job_to_prepare().cloned() {
            match typing.prepare_flatten_to_file(ctx, project, job.page_idx) {
                FlattenReadiness::Preparing { .. } => {}
                FlattenReadiness::Ready => {
                    if self.machine.flush_due() {
                        // Dispatch sequence (plan WP-2.4), all cheap GUI-thread calls, once per save
                        // stage: push the PS editor's active-page rasters and typing's deferred text into
                        // the savers (the flatten worker barriers them), THEN capture the baseline — the
                        // flushes note actions themselves and clear the deferred flags, so a fully
                        // successful save leaves the session clean.
                        ps_editor.flush_layers(project);
                        if let Err(err) = typing.flush_text_layers() {
                            // The flatten composes from the live document, not from staging, so the files
                            // are still correct; only the scratch staging copy is behind.
                            runtime_log::log_warn(format!("[single_image] text flush before save failed; the save composes from the live document.\nPage: {}\nError: {err:?}", job.page_idx));
                        }
                    }
                    let baseline = edit_stamp(gate, typing);
                    let dispatched = self.dispatch(ctx, project, typing, &job, baseline, deferred_edits(typing, ps_editor));
                    step = merge_steps(step, dispatched);
                }
            }
        }
        if let Some(result) = typing.poll_flatten_to_file() {
            let result = result
                .map(|report| match report.outcome {
                    FlattenOutcome::Written { target, bytes_written } => {
                        runtime_log::log_info(format!("[single_image] page saved.\nPage: {}\nTarget: {}\nBytes: {bytes_written}", report.page_idx, target.display()));
                        PageResult::Written(report.digest)
                    }
                    FlattenOutcome::Unchanged => PageResult::Unchanged,
                    FlattenOutcome::Composed => PageResult::Composed,
                })
                .map_err(|err| SaveError::from_flatten(&err));
            if !self.machine.writing() {
                runtime_log::log_warn(format!("[single_image] a flatten result arrived with no save in progress; ignored.\nResult: {result:?}"));
            }
            let finished = self.machine.write_finished(result, edit_stamp(gate, typing), deferred_edits(typing, ps_editor));
            step = merge_steps(step, finished);
        }
        if let Some(commit) = self.machine.take_commit() {
            self.spawn_commit(commit);
        }
        if let Some(result) = self.poll_commit() {
            let finished = self.machine.commit_finished(result, edit_stamp(gate, typing), deferred_edits(typing, ps_editor));
            step = merge_steps(step, finished);
        }
        self.sync_title();
        self.time_status(ctx);
        if self.machine.busy() && !self.machine.choosing_format() {
            // Pickers, preparation, writes and commits all finish without an input event (the
            // format panel and the confirmation dialogs only change on input).
            ctx.request_repaint_after(web_time::Duration::from_millis(100));
        }
        step_outcome(step)
    }

    /// Draws «Сохранить» / «Сохранить как» and the status line into the top bar, the format panel
    /// under «Сохранить как» while it is open, and «Отмена» (one page, while preparing) or «Стоп»
    /// (an image-set pass). The caller's layout is right-to-left, so items are added from the right
    /// edge inward.
    pub(crate) fn draw_top_bar(&mut self, ui: &mut egui::Ui) {
        let busy = self.machine.busy();
        let choosing = self.machine.choosing_format();
        // Enabled while the panel is open: a second click toggles it closed.
        let save_as = ui.add_enabled(!busy || choosing, egui::Button::new(t!("app.menu.save_image_as_button")).selected(choosing));
        if save_as.clicked() {
            self.request_save_as(ui.ctx());
        }
        if let Some(note) = self.machine.format_panel_note() {
            match draw_format_panel(ui, &save_as, note) {
                None => {}
                Some(FormatPanelChoice::Format(format)) => {
                    runtime_log::log_info(format!("[single_image] save format chosen: {}", format.label()));
                    let next = self.machine.choose_format(format);
                    self.run(next);
                }
                Some(FormatPanelChoice::Cancel) => {
                    let step = self.machine.cancel_format_choice();
                    self.defer_step(ui.ctx(), step);
                }
            }
        }
        if ui.add_enabled(!busy, egui::Button::new(t!("app.menu.save_image_button"))).clicked() {
            self.request_save();
        }
        if self.machine.pass_running() {
            if ui.add_enabled(!self.stop_requested, egui::Button::new(t!("single_image.save.stop_button"))).on_hover_text(t!("single_image.save.stop_tooltip")).clicked() {
                self.request_stop(ui.ctx());
            }
        } else if self.machine.job_to_prepare().is_some() && ui.button(t!("single_image.save.cancel_button")).on_hover_text(t!("single_image.save.cancel_tooltip")).clicked() {
            self.request_stop(ui.ctx());
        }
        let single = self.machine.page_count() == 1;
        let (status, _) = self.machine.status();
        if let Some(status) = status {
            let text = status_text(status, single);
            let label = if matches!(status, Status::Error(_) | Status::Partial { .. }) {
                let color = ui.visuals().error_fg_color;
                ui.label(egui::RichText::new(text).color(color))
            } else {
                ui.label(text)
            };
            if let Status::Partial { failed, .. } = status {
                label.on_hover_text(failed_pages_text(failed));
            }
        }
    }

    /// Draws the dialogs of the session: the skipped-files notice (once after the open), the
    /// folder overwrite confirmation and the JPEG options while a save waits on them. Returns
    /// `ReturnToExitDialog` when a save-then-close was cancelled in one of them.
    pub(crate) fn draw_dialogs(&mut self, ctx: &egui::Context) -> Option<SaveOutcome> {
        if self.skipped_notice_open && draw_skipped_notice(ctx, self.session.skipped()) {
            self.skipped_notice_open = false;
        }
        let mut step = Step::Nothing;
        if let Some(folder) = self.machine.confirm_folder_name().map(str::to_owned)
            && let Some(replace) = draw_folder_overwrite_dialog(ctx, &folder)
        {
            runtime_log::log_info(format!("[single_image] non-empty save folder {}.\nFolder: {folder}", if replace { "accepted" } else { "declined" }));
            step = merge_steps(step, self.machine.confirm_folder(replace));
        }
        if let Some(quality) = self.machine.jpeg_options_quality_mut() {
            match draw_jpeg_options_dialog(ctx, quality) {
                None => {}
                Some(JpegOptionsChoice::Save) => {
                    if let Some(quality) = self.machine.confirm_jpeg_options() {
                        self.persist_quality(quality);
                    }
                }
                Some(JpegOptionsChoice::Cancel) => step = merge_steps(step, self.machine.cancel_jpeg_options()),
            }
        }
        step_outcome(step)
    }

    /// The native file or folder dialog is open (it cannot be closed from here).
    pub(crate) fn picker_open(&self) -> bool {
        self.machine.picking_path()
    }

    /// Starts the side effect a format choice asked for.
    fn run(&mut self, next: Next) {
        match next {
            Next::Nothing => {}
            Next::OpenPicker(format) => self.spawn_picker(format),
            Next::OpenFolderPicker(format) => self.spawn_folder_picker(format),
        }
    }

    /// Keeps a save step for the next `tick` (merged with that frame's own) and schedules that frame.
    fn defer_step(&mut self, ctx: &egui::Context, step: Step) {
        if step != Step::Nothing {
            self.deferred_step = merge_steps(self.deferred_step, step);
            ctx.request_repaint();
        }
    }

    /// Remembers a stop click for the next `tick` and schedules that frame.
    fn request_stop(&mut self, ctx: &egui::Context) {
        self.stop_requested = true;
        ctx.request_repaint();
    }

    /// Computes the window title from the opened pages once (the startup title named the REQUESTED
    /// files, some of which may have been skipped), and again whenever a save moved page 0's file;
    /// the title waits in `pending_title` for the window host (`take_title`). A newer title
    /// replaces one not taken yet.
    fn sync_title(&mut self) {
        let moved = self.machine.take_retitle();
        if self.title_synced && moved.is_none() {
            return;
        }
        self.title_synced = true;
        let first = moved.unwrap_or_else(|| self.session.first_source().source_path.clone());
        self.pending_title = Some(image_window_title(env!("MS_APP_VERSION"), &first, self.machine.page_count()));
    }

    /// Returns and clears the window title computed since the last call (`StudioOutput::Title`).
    pub(crate) fn take_title(&mut self) -> Option<String> {
        self.pending_title.take()
    }

    /// Opens the native save dialog on a worker, filtered to `format`, in the current target's (else
    /// the image's) folder with `<stem>.<ext>` preselected. An answer whose name the chosen format
    /// changes (extension appended or replaced) while that file already exists is asked again with
    /// the full name (review M1: the dialog's own overwrite confirmation never saw it), until the user
    /// names a file that is written as named, or a free one, or cancels.
    /// Where paths are not written in place (see `SaveMachine`), the answer's name is never changed,
    /// so there is nothing to re-ask.
    fn spawn_picker(&mut self, format: ImageSaveFormat) {
        let (dir, file_name) = save_as_suggestion(&self.session.first_source().source_path, self.machine.target(), format);
        let writes_in_place = picked_paths_are_user_files();
        self.picker_rx = Some(spawn_worker("image-set-save-dialog", move |tx| {
            let extensions = save_as_filter_extensions(format);
            let mut picked = show_save_dialog(&extensions, dir.as_deref(), file_name, None);
            while let Some(changed) = picked
                .as_ref()
                .ok()
                .and_then(Option::as_ref)
                .filter(|_| writes_in_place)
                .and_then(|target| changed_name_needs_confirmation(target.write_path(), format, path_may_exist))
            {
                let name = changed.file_name().map_or_else(|| changed.display().to_string(), |name| name.to_string_lossy().into_owned());
                runtime_log::log_info(format!("[single_image] the chosen format changed the save dialog answer's name and that file exists; asking again.\nPath: {}", changed.display()));
                let title = tf!("single_image.save_as.confirm_name_title", name = name);
                picked = show_save_dialog(&extensions, changed.parent(), name, Some(title));
            }
            // The receiver is gone only when the controller abandoned the save (window closing).
            if tx.send(picked).is_err() {
                runtime_log::log_info("[single_image] save dialog answered after the save was abandoned; ignored");
            }
        }));
    }

    /// Opens the native folder dialog on a worker for pages written in `format`, starting in page
    /// 0's current target's (else its opened file's) folder. The worker also answers whether the
    /// chosen folder already holds entries (a blocking listing on desktop; an unreadable folder is
    /// logged and counts as holding some, so the user is asked rather than files replaced unasked).
    fn spawn_folder_picker(&mut self, format: ImageSaveFormat) {
        let start_dir = self.machine.target().map_or(self.session.first_source().source_path.as_path(), |target| target.path.as_path()).parent().map(Path::to_path_buf);
        self.folder_rx = Some(spawn_worker("image-set-folder-dialog", move |tx| {
            let answer = show_folder_dialog(start_dir.as_deref()).map(|picked| {
                picked.map(|folder| {
                    let has_entries = folder.has_entries().unwrap_or_else(|err| {
                        runtime_log::log_warn(format!("[single_image] could not list the chosen save folder; asking before replacing files.\nFolder: {}\nError: {err:?}", folder.display_name()));
                        true
                    });
                    (folder, has_entries)
                })
            });
            runtime_log::log_info(format!("[single_image] folder dialog closed.\nFormat: {}\nPicked: {:?}", format.label(), answer.as_ref().map(|picked| picked.as_ref().map(|(folder, has_entries)| (folder.write_dir().display().to_string(), *has_entries)))));
            // The receiver is gone only when the controller abandoned the save (window closing).
            if tx.send(answer).is_err() {
                runtime_log::log_info("[single_image] folder dialog answered after the save was abandoned; ignored");
            }
        }));
    }

    /// Publishes the written file(s) to their dialog target on a worker (`commit` blocks on
    /// Android; a no-op on desktop). The answer is taken by [`Self::poll_commit`].
    fn spawn_commit(&mut self, commit: Commit) {
        self.commit_rx = Some(spawn_worker("image-set-save-commit", move |tx| {
            let (name, result) = match commit {
                Commit::File(target) => (target.display_name().to_owned(), target.commit()),
                Commit::Folder(folder) => (folder.display_name().to_owned(), folder.commit()),
            };
            if let Err(err) = &result {
                runtime_log::log_error(format!("[single_image] the written image(s) could not be published to the chosen destination.\nName: {name}\nError: {err:?}"));
            }
            // The receiver outlives a commit (a close waits for `write_in_flight`); a gone one is logged.
            if tx.send(result).is_err() {
                runtime_log::log_warn("[single_image] save commit finished after its controller was dropped; result discarded");
            }
        }));
    }

    /// The commit worker's answer, once, as the machine's save result.
    fn poll_commit(&mut self) -> Option<Result<(), SaveError>> {
        let rx = self.commit_rx.as_ref()?;
        match rx.try_recv() {
            Ok(result) => {
                self.commit_rx = None;
                Some(result.map_err(|err| SaveError::Commit(err.user_message())))
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.commit_rx = None;
                runtime_log::log_error("[single_image] the save commit worker ended without an answer.\nPossible cause: a panic while publishing the file");
                Some(Err(SaveError::WorkerLost))
            }
        }
    }

    /// Feeds the file dialog's answer to the machine. A dialog that could not be shown (another
    /// one open, no picker on this platform, backend failure) is logged with its typed error and
    /// reported as `SaveError::PickerUnavailable` with the dialog layer's localized message.
    fn poll_picker(&mut self) -> Step {
        let Some(rx) = self.picker_rx.as_ref() else {
            return Step::Nothing;
        };
        match rx.try_recv() {
            Ok(Ok(picked)) => {
                self.picker_rx = None;
                runtime_log::log_info(format!("[single_image] save dialog closed.\nPicked: {:?}", picked.as_ref().map(|target| target.write_path().display().to_string())));
                self.machine.picker_closed(picked)
            }
            Ok(Err(err)) => {
                self.picker_rx = None;
                runtime_log::log_error(format!("[single_image] the save dialog could not be shown.\nError: {err:?}"));
                self.machine.picker_unavailable(err.user_message());
                Step::Nothing
            }
            Err(TryRecvError::Empty) => Step::Nothing,
            Err(TryRecvError::Disconnected) => {
                self.picker_rx = None;
                runtime_log::log_error("[single_image] the save dialog worker ended without an answer.\nPossible cause: the native file dialog failed or panicked");
                self.machine.picker_failed();
                Step::Nothing
            }
        }
    }

    /// Feeds the folder dialog's answer to the machine (errors as in [`Self::poll_picker`]).
    fn poll_folder_picker(&mut self) -> Step {
        let Some(rx) = self.folder_rx.as_ref() else {
            return Step::Nothing;
        };
        match rx.try_recv() {
            Ok(Ok(picked)) => {
                self.folder_rx = None;
                self.machine.folder_picked(picked)
            }
            Ok(Err(err)) => {
                self.folder_rx = None;
                runtime_log::log_error(format!("[single_image] the folder dialog could not be shown.\nError: {err:?}"));
                self.machine.picker_unavailable(err.user_message());
                Step::Nothing
            }
            Err(TryRecvError::Empty) => Step::Nothing,
            Err(TryRecvError::Disconnected) => {
                self.folder_rx = None;
                runtime_log::log_error("[single_image] the folder dialog worker ended without an answer.\nPossible cause: the native folder dialog failed or panicked");
                self.machine.picker_failed();
                Step::Nothing
            }
        }
    }

    /// Dispatches the flatten of `job` (encoded with the page's own ICC profile). A refusal (busy,
    /// no such page; logged by the typing tab) is reported to the machine with the dirty inputs
    /// `edit_stamp_now` (= `baseline`) / `deferred_edits_now`.
    fn dispatch(&mut self, ctx: &egui::Context, project: &ProjectData, typing: &mut TypingTabState, job: &PageJob, baseline: u64, deferred_edits_now: bool) -> Step {
        let icc_profile = match self.session.source(job.page_idx) {
            Some(facts) => facts.icc_profile.clone(),
            None => {
                // Not reachable for a page of the session (`sources()[i]` backs page `i`); the
                // request below refuses a page that is not in the chapter.
                runtime_log::log_warn(format!("[single_image] no source facts for page {}; it is written without an ICC profile", job.page_idx));
                None
            }
        };
        let encoding = ImageEncoding { format: job.format, jpeg_quality: self.machine.jpeg_quality(), alpha: AlphaPolicy::DropIfOpaque, icc_profile };
        let request = FlattenToFileRequest { page_idx: job.page_idx, destination: job.destination.clone(), encoding, unchanged_if: job.unchanged_if };
        match typing.request_flatten_to_file(ctx, project, request) {
            Ok(()) => {
                self.machine.dispatched(baseline);
                Step::Nothing
            }
            Err(err) => self.machine.dispatch_refused(SaveError::from_flatten(&err), baseline, deferred_edits_now),
        }
    }

    /// Persists a confirmed JPEG quality to `user_config` on a worker.
    fn persist_quality(&mut self, quality: u8) {
        let path = crate::config::user_config_path();
        self.persist_rx = Some(spawn_worker("image-set-jpeg-quality-save", move |tx| {
            let result = ms_config::single_image::save_jpeg_quality(&path, quality);
            // The receiver is gone only when the app closed meanwhile; the write itself happened (or
            // failed) and is logged below.
            if let Err(err) = &result {
                runtime_log::log_error(format!("[single_image] could not persist the JPEG quality.\nPath: {}\nQuality: {quality}\nError: {err}", path.display()));
            }
            if tx.send(result).is_err() {
                runtime_log::log_info("[single_image] JPEG quality persisted after the controller was dropped");
            }
        }));
    }

    /// Surfaces a failed quality persistence.
    fn poll_persist(&mut self) {
        let Some(rx) = self.persist_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(())) => self.persist_rx = None,
            Ok(Err(err)) => {
                self.persist_rx = None;
                self.machine.quality_not_persisted(err);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.persist_rx = None;
                runtime_log::log_error("[single_image] the JPEG quality writer ended without a result");
                self.machine.quality_not_persisted(t!("app.save.thread_crashed").to_owned());
            }
        }
    }

    /// Expires a transient status `TRANSIENT_STATUS_SECONDS` after it was first drawn.
    fn time_status(&mut self, ctx: &egui::Context) {
        let (status, revision) = self.machine.status();
        let Some(status) = status else {
            self.status_shown = None;
            return;
        };
        if !status.is_transient() {
            return;
        }
        let now = ctx.input(|input| input.time);
        match self.status_shown {
            Some((shown_revision, shown_at)) if shown_revision == revision => {
                if now - shown_at >= TRANSIENT_STATUS_SECONDS {
                    self.machine.expire_status(revision);
                    self.status_shown = None;
                } else {
                    ctx.request_repaint_after(web_time::Duration::from_secs(1));
                }
            }
            Some(_) | None => {
                self.status_shown = Some((revision, now));
                ctx.request_repaint_after(web_time::Duration::from_secs(1));
            }
        }
    }
}

/// Draws the «Сохранить как» format panel: a small menu-style popup right-aligned under the
/// `anchor` button, with a title, «✕», a line saying what is saved for an image set (`note`) and
/// one button per writable format (`ImageSaveFormat::save_formats`, the one table). Returns the
/// answer of this frame.
///
/// Input capture: the popup is an interactable `Order::Foreground` area sensing click AND drag over
/// its whole rect, so egui's hit-test gives every press, drag and hover over it to the panel and
/// none to the canvas layer beneath (the same occlusion that shields the exit dialog); a canvas
/// widget's `Response::hovered()` stays false there, and the canvas gates that ask
/// `Context::any_popup_open()` / `layer_id_at` refuse as well. Clicks elsewhere are ignored (it
/// closes only by a choice, «✕», Escape or the toggle), so a click that leaves the panel never
/// both closes it and lands on the canvas.
fn draw_format_panel(ui: &egui::Ui, anchor: &egui::Response, note: FormatNote) -> Option<FormatPanelChoice> {
    let popup = egui::Popup::new(egui::Id::new(FORMAT_PANEL_ID), ui.ctx().clone(), anchor, ui.layer_id())
        .kind(egui::PopupKind::Menu)
        .open(true)
        .align(egui::RectAlign::BOTTOM_END)
        .close_behavior(egui::PopupCloseBehavior::IgnoreClicks)
        // Drag too (the default is click only): a press-and-drag that starts on the panel's padding
        // must not fall through to a drag-sensing canvas tool underneath.
        .sense(egui::Sense::click_and_drag())
        .layout(egui::Layout::top_down_justified(egui::Align::Min));
    let shown = popup.show(|ui| {
        let mut choice = None;
        ui.horizontal(|ui| {
            ui.label(t!("single_image.save_as.format_title_label"));
            if ui.small_button(CLOSE_GLYPH).on_hover_text(t!("single_image.save_as.format_cancel_tooltip")).clicked() {
                choice = Some(FormatPanelChoice::Cancel);
            }
        });
        match note {
            FormatNote::File => {}
            FormatNote::AllPages(pages) => {
                ui.small(tp!("single_image.save.all_pages_folder_label", pages));
            }
            FormatNote::Homeless(pages) => {
                ui.small(tp!("single_image.save.needs_folder_label", pages));
            }
        }
        ui.separator();
        for format in ImageSaveFormat::save_formats() {
            if ui.button(format.label()).clicked() {
                choice = Some(FormatPanelChoice::Format(format));
            }
        }
        choice
    })?;
    // Escape (handled inside `Popup::show`) only marks the response as closing.
    shown.inner.or_else(|| shown.response.should_close().then_some(FormatPanelChoice::Cancel))
}

/// Shows the blocking save dialog (picker worker only) filtered to `extensions`, in `dir` with
/// `file_name` preselected and an optional window `title`. `Ok(None)` = cancelled. The answer is
/// the dialog target: write its write path, then commit it (the machine and `spawn_commit`).
///
/// # Errors
/// [`FileDialogError`] when no dialog could be shown.
fn show_save_dialog(extensions: &[String], dir: Option<&Path>, file_name: String, title: Option<String>) -> Result<Option<DialogTarget>, FileDialogError> {
    let mut request = FileDialogRequest::new(DialogKind::SaveFile).filter(t!("single_image.save_as.images_filter"), extensions).suggested_name(file_name);
    if let Some(dir) = dir {
        request = request.start_dir(dir);
    }
    if let Some(title) = title {
        request = request.title(title);
    }
    // `SaveFile` always answers `SaveTarget` (the ms-platform kind contract).
    Ok(file_dialog::pick_blocking(request)?.and_then(Picked::into_save_target))
}

/// Shows the blocking "save into folder" dialog (folder picker worker only), starting in `dir`.
/// `Ok(None)` = cancelled. The answer is the folder target: write into its write dir, then commit.
///
/// # Errors
/// [`FileDialogError`] when no dialog could be shown.
fn show_folder_dialog(dir: Option<&Path>) -> Result<Option<FolderTarget>, FileDialogError> {
    let mut request = FileDialogRequest::new(DialogKind::SaveIntoFolder).title(t!("single_image.save.folder_picker_title"));
    if let Some(dir) = dir {
        request = request.start_dir(dir);
    }
    // `SaveIntoFolder` always answers `FolderTarget` (the ms-platform kind contract).
    Ok(file_dialog::pick_blocking(request)?.and_then(Picked::into_folder_target))
}

/// Starts a detached save-controller worker named `name` (`ms_thread`, so it is a Web Worker on
/// wasm) that answers over the returned receiver. A failed spawn is logged and returns a receiver
/// whose sender is already gone: its next poll sees `Disconnected` and reports the worker as lost
/// through the poll's existing error path (picker failed, `SaveError::WorkerLost`, quality not
/// persisted), whatever the failed spawn did with the closure and its sender.
fn spawn_worker<T: Send + 'static>(name: &str, work: impl FnOnce(mpsc::Sender<T>) + Send + 'static) -> Receiver<T> {
    let (tx, rx) = mpsc::channel();
    // The handle is dropped: every worker here is detached and answers only through `tx`.
    match thread::Builder::new().name(name.to_owned()).spawn(move || work(tx)) {
        Ok(_detached) => rx,
        Err(err) => {
            runtime_log::log_error(format!("[single_image] could not start a save worker thread.\nThread: {name}\nError: {err}\nPossible cause: the OS thread limit or low memory"));
            let (_, lost) = mpsc::channel();
            lost
        }
    }
}

/// Whether paths the platform hands out (the opened images, a save dialog's write path) are the
/// user's own files, so a later «Сохранить» may rewrite them in place. That is exactly when a
/// picked directory path is usable in place (`PlatformFeature::PersistentDirectoryPicker`):
/// desktop yes; Android SAF no — it yields app-storage copies and staging files published to
/// document URIs, which must go through their dialog target every time.
fn picked_paths_are_user_files() -> bool {
    PlatformFeature::PersistentDirectoryPicker.is_available()
}

/// Whether something exists at `path` (a dangling symlink counts: writing there would replace it).
/// Blocking metadata I/O: picker worker only. An inspection error answers "may exist" (logged), so
/// the user is asked again rather than a file being replaced unasked.
fn path_may_exist(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            runtime_log::log_warn(format!("[single_image] could not inspect the save target; asking for the name again.\nPath: {}\nError: {err}", path.display()));
            true
        }
    }
}

/// The session's monotonic edit stamp: the autosave gate's action count (every gated writer notes
/// one action per gesture) plus typing's clip-mask edit count (the mask is persisted outside the
/// gated writers). Both only grow, so their sum moves exactly when either does.
fn edit_stamp(gate: &AutosaveGate, typing: &TypingTabState) -> u64 {
    gate.action_count().wrapping_add(typing.mask_edit_count())
}

/// An edit already changed the image but is not yet counted by [`edit_stamp`] (its persistence,
/// and with it `note_action`, waits for the next flush). Which tabs hold such edits is decided by
/// the session-wide owner [`DeferredTabEdits`], shared with the project's dirty check. A save
/// stage's first dispatch flushes them before the baseline is captured. The canvas bubble term is
/// `false` here: an image-set save never writes bubbles (see `DeferredTabEdits::canvas_bubbles`).
fn deferred_edits(typing: &TypingTabState, ps_editor: &PsEditorTabState) -> bool {
    DeferredTabEdits::of(typing, ps_editor, false).any()
}

/// Two save steps of one frame (a deferred format-panel close, the pickers, a stop and the write
/// cannot end the same save-then-close twice; the later one wins defensively).
fn merge_steps(first: Step, second: Step) -> Step {
    match second {
        Step::Nothing => first,
        Step::CloseNow(_) | Step::ReturnToExitDialog(_) => second,
    }
}

/// Maps a machine step onto the app-facing outcome.
fn step_outcome(step: Step) -> Option<SaveOutcome> {
    match step {
        Step::Nothing => None,
        Step::CloseNow(action) => Some(SaveOutcome::CloseNow(action)),
        Step::ReturnToExitDialog(action) => Some(SaveOutcome::ReturnToExitDialog(action)),
    }
}

/// The localized status line; `single` = a one-page session (its own animated-notice wording).
fn status_text(status: &Status, single: bool) -> String {
    match status {
        Status::AnimatedNotice { .. } if single => tf!("single_image.open.animated_notice", button = t!("app.menu.save_image_button")),
        Status::AnimatedNotice { pages } => tp!("single_image.open.animated_pages_notice", *pages),
        Status::Preparing => t!("app.save.image_preparing_status").to_owned(),
        Status::Writing { name } => tf!("single_image.save.writing_status", name = name),
        Status::Progress { done, total } => tf!("single_image.save.progress_status", done = done, total = total),
        Status::NeedsFolder { pages } => tp!("single_image.save.needs_folder_label", *pages),
        Status::Saved { name } => tf!("app.save.image_saved_status", name = name),
        Status::Unchanged { name } => tf!("single_image.save.unchanged_status", name = name),
        Status::SetSaved { written, unchanged } => tf!("single_image.save.set_saved_status", written = written, unchanged = unchanged),
        Status::Partial { written, failed } => tf!("single_image.save.partial_status", written = written, failed = failed.len()),
        Status::Stopped { written, unsaved } => tf!("single_image.save.stopped_status", written = written, unsaved = unsaved),
        Status::Cancelled => t!("single_image.save.cancelled_status").to_owned(),
        Status::Error(err) => error_text(err),
    }
}

/// The hover text of a partial save: one line per failed page with its error.
fn failed_pages_text(failed: &[(String, SaveError)]) -> String {
    failed.iter().map(|(name, err)| tf!("single_image.save.failed_page_line", name = name, err = error_text(err))).collect::<Vec<_>>().join("\n")
}

/// The localized text of a save error.
fn error_text(err: &SaveError) -> String {
    match err {
        SaveError::PickerFailed => t!("single_image.save.picker_error").to_owned(),
        SaveError::PickerUnavailable(message) => tf!("single_image.save.picker_unavailable_error", err = message),
        SaveError::Commit(message) => tf!("single_image.save.commit_error", err = message),
        SaveError::Busy => t!("single_image.save.busy_error").to_owned(),
        SaveError::NoPage => t!("single_image.save.no_page_error").to_owned(),
        SaveError::Compose(message) => tf!("single_image.save.compose_error", err = message),
        SaveError::Encode(message) => tf!("single_image.save.encode_error", err = message),
        SaveError::Write { path, reason, .. } => tf!("single_image.save.write_error", path = path.display(), err = reason),
        SaveError::WorkerLost => t!("single_image.save.worker_lost_error").to_owned(),
        SaveError::QualityNotPersisted(message) => tf!("single_image.save.quality_persist_error", err = message),
    }
}
