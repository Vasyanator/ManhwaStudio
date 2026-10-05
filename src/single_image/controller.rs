/*
File: src/single_image/controller.rs

Purpose:
`SingleImageController`, the single-image session's save controller (one `MangaApp` field, `Some`
only in that mode). It owns the pure `SaveMachine` and performs its side effects: the native
«Сохранить как» file dialog and the `user_config` quality write (both on workers), the per-frame
drive of `TypingTabState::prepare_flatten_to_file`, the GUI-thread dispatch sequence (PS flush,
typing flush, baseline capture, `request_flatten_to_file`), the per-frame `poll_flatten_to_file`,
the window-title update after «Сохранить как», and the top bar (with the «Сохранить как…» format
panel) / JPEG options UI.

Key structures:
- `SingleImageController`

Key functions:
- `SingleImageController::from_project()`: `Some` for a single-image `ProjectData`.
- `tick()`: call EVERY frame, whatever tab is active (it is also the app's one
  `poll_flatten_to_file` caller, which the typing tab requires).
- `draw_top_bar()` (also the format panel, `draw_format_panel`) / `draw_dialogs()`;
  `request_save*()`, `abandon_pending()`.

Notes:
The GUI thread does no file I/O here: the picker, the flatten (compose, encode, atomic write) and
the quality persistence run on workers; the GUI thread only snapshots, flushes into the in-memory
savers and polls channels. Encoding policy of a save (plan D7): PNG / WebP drop alpha only when
fully opaque, JPEG is flattened over white at the session quality, the source's ICC profile is
re-embedded. Dirty inputs are defined here (`edit_stamp`, `deferred_edits`); the machine only
compares them. The picker worker re-asks with the full name when the chosen format changed the
answer's name (extension appended or replaced) and that file exists, so the native overwrite
confirmation always saw the written name.

The format panel is an `egui::Popup` (`PopupKind::Menu` -> `Order::Foreground`) whose open state is
the machine's `Phase::ChoosingFormat`, not egui memory. It blocks the canvas the same way the exit
dialog does — z-order occlusion in egui's hit-test (an interactable area above the canvas layer
takes the hover / click / drag) — and, being a popup, also sets `Context::any_popup_open`, which the
cleaning and PS-editor canvas gates check on top of `layer_id_at`.
*/

use super::dialogs::{JpegOptionsChoice, draw_jpeg_options_dialog};
use super::machine::{Next, Phase, SaveError, SaveMachine, SaveTarget, Status, Step, changed_name_needs_confirmation, save_as_filter_extensions, save_as_suggestion};
use super::{SaveOutcome, SaveParts, image_window_title};
use crate::app::PendingCloseAction;
use crate::models::autosave_gate::AutosaveGate;
use crate::project::{ProjectData, SessionKind, SingleImageSession};
use crate::runtime_log;
use crate::tabs::typing::image_encode::{AlphaPolicy, ImageEncoding, ImageSaveFormat};
use crate::tabs::ps_editor::PsEditorTabState;
use crate::tabs::typing::{FlattenReadiness, FlattenToFileRequest, TypingTabState};
use eframe::egui;
use ms_thread as thread;
use serde_json::Value;
use std::path::{Path, PathBuf};
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
    /// Save in this format (the file dialog opens next).
    Format(ImageSaveFormat),
    /// «✕» or Escape: close the panel without saving.
    Cancel,
}

/// Save controller of one single-image session (see the file header).
pub(crate) struct SingleImageController {
    session: Arc<SingleImageSession>,
    machine: SaveMachine,
    /// Result of the native file dialog while `Phase::PickingPath` (`None` inside = cancelled).
    picker_rx: Option<Receiver<Option<PathBuf>>>,
    /// Result of the last JPEG quality persistence.
    persist_rx: Option<Receiver<Result<(), String>>>,
    /// The status revision being timed and when it was first drawn (app time, seconds).
    status_shown: Option<(u64, f64)>,
    /// A save step produced outside `tick` / `draw_dialogs` (the format panel closed during a
    /// save-then-close, from the top bar or the hotkey), reported by the next `tick`.
    deferred_step: Step,
}

impl SingleImageController {
    /// The controller of `project`'s single-image session, or `None` for a project session.
    /// `user_settings` is the startup `user_config` root (JPEG quality default).
    pub(crate) fn from_project(project: &ProjectData, user_settings: &Value) -> Option<Self> {
        let SessionKind::SingleImage(session) = project.session() else {
            return None;
        };
        let in_place = ImageSaveFormat::in_place_for(session).map(|format| SaveTarget { path: session.source_path.clone(), format });
        runtime_log::log_info(format!(
            "[single_image] session opened.\nSource: {}\nFormat: {}\nIn-place save: {:?}\nAnimated: {}",
            session.source_path.display(),
            session.source_format.as_str(),
            in_place.as_ref().map(|target| target.format),
            session.source_animated
        ));
        let machine = SaveMachine::new(in_place, ms_config::single_image::jpeg_quality_from(user_settings), session.source_animated);
        Some(Self { session: Arc::clone(session), machine, picker_rx: None, persist_rx: None, status_shown: None, deferred_step: Step::Nothing })
    }

    /// The image differs from the last successful write to the file (plan D6): the edit stamp
    /// moved, or an edit is still deferred in typing or the PS editor.
    pub(crate) fn is_dirty(&self, gate: &AutosaveGate, typing: &TypingTabState, ps_editor: &PsEditorTabState) -> bool {
        self.machine.is_dirty(edit_stamp(gate, typing), deferred_edits(typing, ps_editor))
    }

    /// The file is being written right now (a close must wait for it).
    pub(crate) fn write_in_flight(&self) -> bool {
        self.machine.writing()
    }

    /// «Сохранить» (opens the format panel when the image has no in-place target).
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
    /// open native file dialog's answer is ignored (its channel is dropped).
    pub(crate) fn abandon_pending(&mut self) {
        self.machine.abandon_pending();
        self.picker_rx = None;
    }

    /// Per-frame drive. Polls the file dialog and the quality write, advances a preparing save and
    /// dispatches it once the composite inputs are ready, polls the flatten worker (every frame, as
    /// the typing tab requires), retitles the window after «Сохранить как» and times transient
    /// statuses. Returns what the app must do when a save-then-close ended.
    pub(crate) fn tick(&mut self, ctx: &egui::Context, parts: SaveParts<'_>) -> Option<SaveOutcome> {
        let SaveParts { project, typing, ps_editor, gate } = parts;
        self.poll_persist();
        let deferred = std::mem::replace(&mut self.deferred_step, Step::Nothing);
        let mut step = merge_steps(deferred, self.poll_picker());
        if let Phase::Preparing { target } = self.machine.phase() {
            let target = target.clone();
            match typing.prepare_flatten_to_file(ctx, project) {
                FlattenReadiness::Preparing { .. } => {}
                FlattenReadiness::Ready => {
                    // Dispatch sequence (plan WP-2.4), all cheap GUI-thread calls: push the PS editor's
                    // active-page rasters and typing's deferred text into the savers (the flatten worker
                    // barriers them), THEN capture the baseline — the flushes note actions themselves
                    // and clear the deferred flags, so a successful write leaves the session clean.
                    ps_editor.flush_layers(project);
                    if let Err(err) = typing.flush_text_layers() {
                        // The flatten composes from the live document, not from staging, so the file is
                        // still correct; only the scratch staging copy is behind.
                        runtime_log::log_warn(format!("[single_image] text flush before save failed; the save composes from the live document.\nTarget: {}\nError: {err:?}", target.path.display()));
                    }
                    let baseline = edit_stamp(gate, typing);
                    self.dispatch(ctx, project, typing, &target, baseline);
                }
            }
        }
        if let Some(result) = typing.poll_flatten_to_file() {
            let result = result.map(|report| {
                runtime_log::log_info(format!("[single_image] saved.\nTarget: {}\nBytes: {}", report.target.display(), report.bytes_written));
            });
            let result = result.map_err(|err| SaveError::from_flatten(&err));
            if !self.machine.writing() {
                runtime_log::log_warn(format!("[single_image] a flatten result arrived with no save in progress; ignored.\nResult: {result:?}"));
            }
            let finished = self.machine.write_finished(result, edit_stamp(gate, typing), deferred_edits(typing, ps_editor));
            step = merge_steps(step, finished);
        }
        if let Some(path) = self.machine.take_retitle() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(image_window_title(env!("MS_APP_VERSION"), &path)));
        }
        self.time_status(ctx);
        if self.machine.busy() && !self.machine.choosing_format() {
            // Picker, preparation and write all finish without an input event (the format panel
            // only changes on input).
            ctx.request_repaint_after(web_time::Duration::from_millis(100));
        }
        step_outcome(step)
    }

    /// Draws «Сохранить» / «Сохранить как» and the status line into the top bar, and the format
    /// panel under «Сохранить как» while it is open. The caller's layout is right-to-left, so items
    /// are added from the right edge inward.
    pub(crate) fn draw_top_bar(&mut self, ui: &mut egui::Ui) {
        let busy = self.machine.busy();
        let choosing = self.machine.choosing_format();
        // Enabled while the panel is open: a second click toggles it closed.
        let save_as = ui.add_enabled(!busy || choosing, egui::Button::new(t!("app.menu.save_image_as_button")).selected(choosing));
        if save_as.clicked() {
            self.request_save_as(ui.ctx());
        }
        if self.machine.choosing_format() {
            match draw_format_panel(ui, &save_as) {
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
        if matches!(self.machine.phase(), Phase::Preparing { .. })
            && ui.button(t!("single_image.save.cancel_button")).on_hover_text(t!("single_image.save.cancel_tooltip")).clicked()
        {
            runtime_log::log_info("[single_image] save cancelled while preparing");
            self.machine.cancel_preparing();
        }
        let (status, _) = self.machine.status();
        if let Some(status) = status {
            let text = status_text(status);
            if matches!(status, Status::Error(_)) {
                let color = ui.visuals().error_fg_color;
                ui.label(egui::RichText::new(text).color(color));
            } else {
                ui.label(text);
            }
        }
    }

    /// Draws the JPEG options dialog while a save waits on it. Returns `ReturnToExitDialog` when it
    /// was cancelled during a save-then-close.
    pub(crate) fn draw_dialogs(&mut self, ctx: &egui::Context) -> Option<SaveOutcome> {
        let quality = self.machine.jpeg_options_quality_mut()?;
        match draw_jpeg_options_dialog(ctx, quality)? {
            JpegOptionsChoice::Save => {
                if let Some(quality) = self.machine.confirm_jpeg_options() {
                    self.persist_quality(quality);
                }
                None
            }
            JpegOptionsChoice::Cancel => step_outcome(self.machine.cancel_jpeg_options()),
        }
    }

    /// Starts the side effect a format choice asked for.
    fn run(&mut self, next: Next) {
        match next {
            Next::Nothing => {}
            Next::OpenPicker(format) => self.spawn_picker(format),
        }
    }

    /// Keeps a save step for the next `tick` (merged with that frame's own) and schedules that frame.
    fn defer_step(&mut self, ctx: &egui::Context, step: Step) {
        if step != Step::Nothing {
            self.deferred_step = merge_steps(self.deferred_step, step);
            ctx.request_repaint();
        }
    }

    /// Opens the native save dialog on a worker, filtered to `format`, in the current target's (else
    /// the image's) folder with `<stem>.<ext>` preselected. An answer whose name the chosen format
    /// changes (extension appended or replaced) while that file already exists is asked again with
    /// the full name (review M1: the dialog's own overwrite confirmation never saw it), until the user
    /// names a file that is written as named, or a free one, or cancels.
    fn spawn_picker(&mut self, format: ImageSaveFormat) {
        let (dir, file_name) = save_as_suggestion(&self.session.source_path, self.machine.target(), format);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let extensions = save_as_filter_extensions(format);
            let mut picked = show_save_dialog(&extensions, dir.as_deref(), file_name, None);
            while let Some(changed) = picked.as_deref().and_then(|path| changed_name_needs_confirmation(path, format, path_may_exist)) {
                let name = changed.file_name().map_or_else(|| changed.display().to_string(), |name| name.to_string_lossy().into_owned());
                runtime_log::log_info(format!("[single_image] the chosen format changed the save dialog answer's name and that file exists; asking again.\nPath: {}", changed.display()));
                let title = tf!("single_image.save_as.confirm_name_title", name = name);
                picked = show_save_dialog(&extensions, changed.parent(), name, Some(title));
            }
            // The receiver is gone only when the controller abandoned the save (window closing).
            if tx.send(picked).is_err() {
                runtime_log::log_info("[single_image] save dialog answered after the save was abandoned; ignored");
            }
        });
        self.picker_rx = Some(rx);
    }

    /// The native «Сохранить как» dialog is open (it cannot be closed from here).
    pub(crate) fn picker_open(&self) -> bool {
        self.machine.picking_path()
    }

    /// Feeds the file dialog's answer to the machine.
    fn poll_picker(&mut self) -> Step {
        let Some(rx) = self.picker_rx.as_ref() else {
            return Step::Nothing;
        };
        match rx.try_recv() {
            Ok(picked) => {
                self.picker_rx = None;
                runtime_log::log_info(format!("[single_image] save dialog closed.\nPicked: {:?}", picked.as_ref().map(|path| path.display().to_string())));
                self.machine.picker_closed(picked)
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

    /// Dispatches the flatten of the session's page into `target`.
    fn dispatch(&mut self, ctx: &egui::Context, project: &ProjectData, typing: &mut TypingTabState, target: &SaveTarget, baseline: u64) {
        let Some(page_idx) = project.pages.first().map(|page| page.idx) else {
            runtime_log::log_error(format!("[single_image] save refused: the scratch chapter has no page.\nTarget: {}", target.path.display()));
            self.machine.dispatch_refused(SaveError::NoPage);
            return;
        };
        let encoding = ImageEncoding { format: target.format, jpeg_quality: self.machine.jpeg_quality(), alpha: AlphaPolicy::DropIfOpaque, icc_profile: self.session.icc_profile.clone() };
        let request = FlattenToFileRequest { page_idx, target: target.path.clone(), encoding };
        match typing.request_flatten_to_file(ctx, project, request) {
            Ok(()) => self.machine.dispatched(baseline),
            // Logged by the typing tab.
            Err(err) => self.machine.dispatch_refused(SaveError::from_flatten(&err)),
        }
    }

    /// Persists a confirmed JPEG quality to `user_config` on a worker.
    fn persist_quality(&mut self, quality: u8) {
        let (tx, rx) = mpsc::channel();
        let path = crate::config::user_config_path();
        thread::spawn(move || {
            let result = ms_config::single_image::save_jpeg_quality(&path, quality);
            // The receiver is gone only when the app closed meanwhile; the write itself happened (or
            // failed) and is logged below.
            if let Err(err) = &result {
                runtime_log::log_error(format!("[single_image] could not persist the JPEG quality.\nPath: {}\nQuality: {quality}\nError: {err}", path.display()));
            }
            if tx.send(result).is_err() {
                runtime_log::log_info("[single_image] JPEG quality persisted after the controller was dropped");
            }
        });
        self.persist_rx = Some(rx);
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
/// `anchor` button, with a title, «✕» and one button per writable format
/// (`ImageSaveFormat::save_formats`, the one table). Returns the answer of this frame.
///
/// Input capture: the popup is an interactable `Order::Foreground` area sensing click AND drag over
/// its whole rect, so egui's hit-test gives every press, drag and hover over it to the panel and
/// none to the canvas layer beneath (the same occlusion that shields the exit dialog); a canvas
/// widget's `Response::hovered()` stays false there, and the canvas gates that ask
/// `Context::any_popup_open()` / `layer_id_at` refuse as well. Clicks elsewhere are ignored (it
/// closes only by a choice, «✕», Escape or the toggle), so a click that leaves the panel never
/// both closes it and lands on the canvas.
fn draw_format_panel(ui: &egui::Ui, anchor: &egui::Response) -> Option<FormatPanelChoice> {
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

/// Shows the blocking native save dialog (picker worker only) filtered to `extensions`, in `dir`
/// with `file_name` preselected and an optional window `title`. `None` = cancelled.
fn show_save_dialog(extensions: &[String], dir: Option<&Path>, file_name: String, title: Option<String>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().add_filter(t!("single_image.save_as.images_filter"), extensions).set_file_name(file_name);
    if let Some(dir) = dir {
        dialog = dialog.set_directory(dir);
    }
    if let Some(title) = title {
        dialog = dialog.set_title(title);
    }
    dialog.save_file()
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

/// An edit already changed the image but is not yet counted by [`edit_stamp`]: typing's deferred
/// text-layer writes, or a PS-editor document edit whose persistence (and `note_action`) waits for
/// the next flush. The save's dispatch flushes both before capturing its baseline.
fn deferred_edits(typing: &TypingTabState, ps_editor: &PsEditorTabState) -> bool {
    typing.has_pending_text_edits() || ps_editor.has_deferred_layer_edits()
}

/// Two save steps of one frame (a deferred format-panel close, the picker and the write cannot end
/// the same save-then-close twice; the later one wins defensively).
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

/// The localized status line.
fn status_text(status: &Status) -> String {
    match status {
        Status::AnimatedNotice => tf!("single_image.open.animated_notice", button = t!("app.menu.save_image_button")),
        Status::Preparing => t!("app.save.image_preparing_status").to_owned(),
        Status::Writing { name } => tf!("single_image.save.writing_status", name = name),
        Status::Saved { name } => tf!("app.save.image_saved_status", name = name),
        Status::Cancelled => t!("single_image.save.cancelled_status").to_owned(),
        Status::Error(err) => error_text(err),
    }
}

/// The localized text of a save error.
fn error_text(err: &SaveError) -> String {
    match err {
        SaveError::PickerFailed => t!("single_image.save.picker_error").to_owned(),
        SaveError::Busy => t!("single_image.save.busy_error").to_owned(),
        SaveError::NoPage => t!("single_image.save.no_page_error").to_owned(),
        SaveError::Compose(message) => tf!("single_image.save.compose_error", err = message),
        SaveError::Encode(message) => tf!("single_image.save.encode_error", err = message),
        SaveError::Write { path, reason } => tf!("single_image.save.write_error", path = path.display(), err = reason),
        SaveError::WorkerLost => t!("single_image.save.worker_lost_error").to_owned(),
        SaveError::QualityNotPersisted(message) => tf!("single_image.save.quality_persist_error", err = message),
    }
}
