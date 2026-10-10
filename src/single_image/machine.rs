/*
File: src/single_image/machine.rs

Purpose:
The pure state machine of single-image saving: which file a save writes, whether the session is
dirty, the phases of one save (choose a format -> pick a path -> JPEG options -> prepare -> write)
and the status the top bar shows. No egui, no threads, no I/O — `controller.rs` feeds it events (button presses,
picker results, readiness, the flatten result) and performs the side effects it asks for, which is
what makes every transition unit-testable.

Key structures:
- `SaveTarget`: a path plus the format it is written in.
- `SaveMachine`: target, dirty baseline, current `Phase`, JPEG quality, status.
- `Phase`, `Status`, `SaveError`, `Next`, `Step`.

Key functions:
- `SaveMachine::request_save` / `request_save_as` / `request_save_then`
- `choose_format`, `cancel_format_choice`
- `picker_closed`, `confirm_jpeg_options`, `cancel_jpeg_options`, `cancel_preparing`
- `picker_unavailable`, `dispatched`, `dispatch_refused`, `write_finished`, `take_commit`,
  `commit_finished`
- `resolve_save_as_path()` / `extension_enforced_path()`: picked path -> target; the CHOSEN format
  is authoritative, the extension is appended or replaced to match it.

Notes:
Dirty (plan D6) = the session's monotonic EDIT STAMP differs from the baseline captured at the last
successful write, or an edit is still deferred (not yet counted by the stamp). The controller
defines both inputs (`controller::edit_stamp` / `deferred_edits`); this machine only compares. The
baseline is captured when the write is DISPATCHED (after the save's own flushes) and applied only
when the write succeeds, so an edit made while the file is being written keeps the session dirty,
and a failed write changes nothing.

Dialog targets (plan WP1d): a «Сохранить как» answer is an `ms_platform` `SaveTarget`. The machine
writes its write path, keeps the dialog target as `pending_commit` and, after the flatten wrote
the file, enters `Committing` until the controller's commit worker reports (`take_commit` ->
`commit_finished`); a commit failure is a failed save. Whether a written path may later be
rewritten IN PLACE without the dialog (`writes_in_place`) is the caller's platform answer: on
desktop the dialog's write path IS the chosen file, so the opened file and the last «Сохранить
как» target are «Сохранить» targets; where it is not (Android SAF: a staging file published to a
document URI), there is no in-place target at all and every «Сохранить» goes through the dialog,
so a copied path is never written without its commit.
*/

use crate::app::PendingCloseAction;
use crate::tabs::typing::FlattenToFileError;
use ms_platform::file_dialog::SaveTarget as DialogTarget;
use crate::tabs::typing::image_encode::ImageSaveFormat;
use std::path::{Path, PathBuf};

/// A file a save writes, and the format it is written in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SaveTarget {
    /// The path as the user named it (an existing symlink is resolved by the writer).
    pub path: PathBuf,
    /// The encoder used for it.
    pub format: ImageSaveFormat,
}

impl SaveTarget {
    /// The file name shown in statuses (the whole path when it has none).
    pub fn display_name(&self) -> String {
        self.path.file_name().map_or_else(|| self.path.display().to_string(), |name| name.to_string_lossy().into_owned())
    }
}

/// Where one save currently is. Every phase but `Idle` blocks a new save request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Phase {
    /// No save in progress.
    Idle,
    /// The format panel under «Сохранить как…» is open: the user picks the format the file is
    /// written in (or closes the panel).
    ChoosingFormat,
    /// The native «Сохранить как» file dialog is open (on a worker thread), filtered to `format`.
    PickingPath {
        /// The format chosen in the panel; it decides the written extension (see
        /// [`extension_enforced_path`]).
        format: ImageSaveFormat,
    },
    /// The «Параметры JPEG» dialog is shown for `target`; `quality` is what its slider edits.
    JpegOptions {
        /// The JPEG file the save will write.
        target: SaveTarget,
        /// Quality being edited, `1..=100`.
        quality: u8,
    },
    /// Waiting for `TypingTabState::prepare_flatten_to_file` to report `Ready`.
    Preparing {
        /// The file the save will write.
        target: SaveTarget,
    },
    /// The flatten worker is writing `target`; `baseline` is the edit stamp captured at dispatch,
    /// applied as the saved baseline only when the write succeeds.
    Writing {
        /// The file being written.
        target: SaveTarget,
        /// Edit stamp at dispatch.
        baseline: u64,
    },
    /// The file was written; the commit worker publishes it to the dialog target. Like `Writing`,
    /// it cannot be cancelled and the save succeeds only when it does.
    Committing {
        /// The written file.
        target: SaveTarget,
        /// Edit stamp at dispatch.
        baseline: u64,
    },
}

/// Why a save did not happen. Typed so the localized text is chosen in one place
/// (`controller::status_text`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SaveError {
    /// The native file dialog could not be shown (its worker ended without an answer).
    PickerFailed,
    /// No dialog could be shown (another one is open, none on this platform, backend failure);
    /// the dialog layer's localized message.
    PickerUnavailable(String),
    /// The file was written but could not be published to the chosen destination; the dialog
    /// layer's localized message.
    Commit(String),
    /// The typing tab refused the flatten (another flatten or an export is running).
    Busy,
    /// The scratch chapter has no page.
    NoPage,
    /// The page could not be composed (the export pipeline's message).
    Compose(String),
    /// The composed page could not be encoded (technical detail).
    Encode(String),
    /// The file could not be written; the previous file is intact.
    Write {
        /// The path the write was attempted at.
        path: PathBuf,
        /// Technical cause.
        reason: String,
    },
    /// The flatten worker ended without a result.
    WorkerLost,
    /// The confirmed JPEG quality could not be persisted to `user_config` (the save itself went on).
    QualityNotPersisted(String),
}

impl SaveError {
    /// Maps the typing tab's typed flatten failure onto the save error shown to the user.
    pub fn from_flatten(err: &FlattenToFileError) -> Self {
        match err {
            FlattenToFileError::Busy => Self::Busy,
            FlattenToFileError::NoSuchPage(_) => Self::NoPage,
            FlattenToFileError::Compose(message) => Self::Compose(message.clone()),
            FlattenToFileError::Encode(encode) => Self::Encode(encode.to_string()),
            FlattenToFileError::Write { path, reason } => Self::Write { path: path.clone(), reason: reason.clone() },
            FlattenToFileError::WorkerLost => Self::WorkerLost,
        }
    }
}

/// The top-bar status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Status {
    /// Shown on open for an animated source (only the first frame was opened; «Сохранить» acts as
    /// «Сохранить как»).
    AnimatedNotice,
    /// Waiting for the composite inputs.
    Preparing,
    /// The file is being written.
    Writing {
        /// Display name of the target.
        name: String,
    },
    /// The last save succeeded (transient).
    Saved {
        /// Display name of the written file.
        name: String,
    },
    /// The user cancelled a save (transient).
    Cancelled,
    /// The last save (or quality persistence) failed; stays until the next save attempt.
    Error(SaveError),
}

impl Status {
    /// Transient statuses disappear after a few seconds; the others stay until replaced.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Saved { .. } | Self::Cancelled)
    }
}

/// A side effect the controller must start after a format was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(super) enum Next {
    /// Nothing to start.
    Nothing,
    /// Spawn the native «Сохранить как» file dialog, filtered to this format.
    OpenPicker(ImageSaveFormat),
}

/// How a save that ENDED relates to a pending close action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(super) enum Step {
    /// Nothing for the app to do.
    Nothing,
    /// The save succeeded and the session is clean: run the close action.
    CloseNow(PendingCloseAction),
    /// The user backed out of a save-then-close, or edited during the write: ask again.
    ReturnToExitDialog(PendingCloseAction),
}

/// Pure single-image save state (see the file header).
#[derive(Debug)]
pub(super) struct SaveMachine {
    /// What «Сохранить» writes: the opened file in its own format when that is writable in place,
    /// later the last successful «Сохранить как» target. `None` => «Сохранить» acts as «Сохранить как».
    target: Option<SaveTarget>,
    /// Edit stamp at the last successful write (0 = the freshly opened state).
    saved_baseline: u64,
    phase: Phase,
    /// JPEG quality of the session (pre-filled from `user_config`, updated on confirm).
    jpeg_quality: u8,
    /// The JPEG target whose options were confirmed this session; «Сохранить» to it is silent.
    jpeg_confirmed_for: Option<PathBuf>,
    status: Option<Status>,
    /// Bumped on every status change, so the controller can time transient statuses.
    status_revision: u64,
    /// The close action to run once the current save succeeds (save-then-close from the exit dialog).
    after_save: Option<PendingCloseAction>,
    /// The dialog target of the save in progress, committed after its write (see the file header).
    pending_commit: Option<DialogTarget>,
    /// Whether a written path may be rewritten in place by a later «Сохранить» (see the header).
    writes_in_place: bool,
    /// Set when a successful save moved the target to a new path (the window title must follow).
    retitle_to: Option<PathBuf>,
}

impl SaveMachine {
    /// A fresh session. `in_place` is `ImageSaveFormat::in_place_for(session)` with the opened
    /// path; `animated` shows the first-frame notice; `writes_in_place` = paths the platform hands
    /// out are the user's own files (see the file header). Without it `in_place` is dropped.
    pub fn new(in_place: Option<SaveTarget>, jpeg_quality: u8, animated: bool, writes_in_place: bool) -> Self {
        let mut machine = Self {
            target: in_place.filter(|_| writes_in_place),
            saved_baseline: 0,
            phase: Phase::Idle,
            jpeg_quality,
            jpeg_confirmed_for: None,
            status: None,
            status_revision: 0,
            after_save: None,
            retitle_to: None,
            pending_commit: None,
            writes_in_place,
        };
        if animated {
            machine.set_status(Status::AnimatedNotice);
        }
        machine
    }

    /// Dirty = the edit stamp moved since the last successful write, or an edit is still deferred
    /// (plan D6; see the file header).
    pub fn is_dirty(&self, edit_stamp: u64, deferred_edits: bool) -> bool {
        edit_stamp != self.saved_baseline || deferred_edits
    }

    /// The current phase.
    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// A save is in progress (no new one can start). The open format panel counts: its
    /// «Сохранить как…» button only toggles it closed.
    pub fn busy(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// The format panel is open (`Phase::ChoosingFormat`).
    pub fn choosing_format(&self) -> bool {
        self.phase == Phase::ChoosingFormat
    }

    /// The native file dialog is open (`Phase::PickingPath`); it cannot be closed from code.
    pub fn picking_path(&self) -> bool {
        matches!(self.phase, Phase::PickingPath { .. })
    }

    /// The file is being written or committed right now (cannot be cancelled).
    pub fn writing(&self) -> bool {
        matches!(self.phase, Phase::Writing { .. } | Phase::Committing { .. })
    }

    /// The current «Сохранить» target, if any.
    pub fn target(&self) -> Option<&SaveTarget> {
        self.target.as_ref()
    }

    /// The session's JPEG quality (pre-filled from `user_config`, or the last confirmed one).
    pub fn jpeg_quality(&self) -> u8 {
        self.jpeg_quality
    }

    /// The status line and its revision.
    pub fn status(&self) -> (Option<&Status>, u64) {
        (self.status.as_ref(), self.status_revision)
    }

    /// Drops a transient status the controller timed out (no-op when `revision` is stale).
    pub fn expire_status(&mut self, revision: u64) {
        if revision == self.status_revision && self.status.as_ref().is_some_and(Status::is_transient) {
            self.status = None;
            self.status_revision = self.status_revision.wrapping_add(1);
        }
    }

    /// «Сохранить»: write the current target, or act as «Сохранить как» without one (the format
    /// panel opens). A JPEG target whose options were not confirmed this session shows the options
    /// first. Ignored while busy.
    pub fn request_save(&mut self) {
        if self.busy() {
            return;
        }
        match self.target.clone() {
            None => self.phase = Phase::ChoosingFormat,
            Some(target) => {
                if target.format == ImageSaveFormat::Jpeg && self.jpeg_confirmed_for.as_ref() != Some(&target.path) {
                    self.phase = Phase::JpegOptions { target, quality: self.jpeg_quality };
                } else {
                    self.start_preparing(target);
                }
            }
        }
    }

    /// «Сохранить как…» (button or hotkey) TOGGLES the format panel: opens it when idle, closes it
    /// like [`Self::cancel_format_choice`] when it is open. Ignored during any other save phase.
    pub fn request_save_as(&mut self) -> Step {
        match self.phase {
            Phase::Idle => {
                self.phase = Phase::ChoosingFormat;
                Step::Nothing
            }
            Phase::ChoosingFormat => self.cancel_format_choice(),
            Phase::PickingPath { .. } | Phase::JpegOptions { .. } | Phase::Preparing { .. } | Phase::Writing { .. } | Phase::Committing { .. } => Step::Nothing,
        }
    }

    /// «Сохранить» from the exit dialog: remembers `action` for after a successful write. A save
    /// already in progress (the format panel included) keeps going and adopts the action.
    pub fn request_save_then(&mut self, action: PendingCloseAction) {
        self.after_save = Some(action);
        if self.busy() {
            return;
        }
        self.request_save();
    }

    /// A format was chosen in the panel: the native dialog opens, filtered to it, and the written
    /// file will carry its extension. Ignored unless the panel is open.
    pub fn choose_format(&mut self, format: ImageSaveFormat) -> Next {
        if self.phase != Phase::ChoosingFormat {
            return Next::Nothing;
        }
        self.phase = Phase::PickingPath { format };
        Next::OpenPicker(format)
    }

    /// The panel was closed without a choice («✕», Escape, or the «Сохранить как…» toggle): nothing
    /// happens and no status is shown; a save-then-close returns to the exit dialog, as a cancelled
    /// file dialog does.
    pub fn cancel_format_choice(&mut self) -> Step {
        if self.phase != Phase::ChoosingFormat {
            return Step::Nothing;
        }
        self.phase = Phase::Idle;
        self.after_save.take().map_or(Step::Nothing, Step::ReturnToExitDialog)
    }

    /// The file dialog closed. `None` = the user cancelled it (a save-then-close returns to the exit
    /// dialog). The target is written in the CHOSEN format; where paths are written in place its
    /// extension is made to match ([`resolve_save_as_path`]), elsewhere the dialog's write path is
    /// kept as is (its name belongs to the document it is committed to). The dialog target is kept
    /// for the commit after the write. A JPEG target always shows the options (they are the
    /// format's options step for «Сохранить как»).
    pub fn picker_closed(&mut self, picked: Option<DialogTarget>) -> Step {
        let Phase::PickingPath { format } = self.phase else {
            return Step::Nothing;
        };
        let Some(picked) = picked else {
            self.phase = Phase::Idle;
            self.set_status(Status::Cancelled);
            return self.after_save.take().map_or(Step::Nothing, Step::ReturnToExitDialog);
        };
        let written = picked.write_path().to_path_buf();
        let target = if self.writes_in_place { resolve_save_as_path(written, format) } else { SaveTarget { path: written, format } };
        self.pending_commit = Some(picked);
        if target.format == ImageSaveFormat::Jpeg {
            self.phase = Phase::JpegOptions { target, quality: self.jpeg_quality };
        } else {
            self.start_preparing(target);
        }
        Step::Nothing
    }

    /// The file dialog's worker ended without an answer.
    pub fn picker_failed(&mut self) {
        if self.picking_path() {
            self.fail(SaveError::PickerFailed);
        }
    }

    /// No dialog could be shown; `message` is the dialog layer's localized explanation.
    pub fn picker_unavailable(&mut self, message: String) {
        if self.picking_path() {
            self.fail(SaveError::PickerUnavailable(message));
        }
    }

    /// The quality the JPEG options slider edits, while that dialog is shown.
    pub fn jpeg_options_quality_mut(&mut self) -> Option<&mut u8> {
        match &mut self.phase {
            Phase::JpegOptions { quality, .. } => Some(quality),
            Phase::Idle | Phase::ChoosingFormat | Phase::PickingPath { .. } | Phase::Preparing { .. } | Phase::Writing { .. } | Phase::Committing { .. } => None,
        }
    }

    /// «Сохранить» in the JPEG options: adopts the quality for the session, marks the target as
    /// confirmed and starts preparing. Returns the quality to persist to `user_config`.
    pub fn confirm_jpeg_options(&mut self) -> Option<u8> {
        let Phase::JpegOptions { target, quality } = self.phase.clone() else {
            return None;
        };
        self.jpeg_quality = quality;
        self.jpeg_confirmed_for = Some(target.path.clone());
        self.start_preparing(target);
        Some(quality)
    }

    /// «Отмена» in the JPEG options: nothing is written; a save-then-close returns to the exit dialog.
    pub fn cancel_jpeg_options(&mut self) -> Step {
        if !matches!(self.phase, Phase::JpegOptions { .. }) {
            return Step::Nothing;
        }
        self.phase = Phase::Idle;
        self.pending_commit = None;
        self.set_status(Status::Cancelled);
        self.after_save.take().map_or(Step::Nothing, Step::ReturnToExitDialog)
    }

    /// The user stopped a save that is still waiting for its inputs (they may never arrive if a
    /// loader worker died). Nothing is written; a pending close action is dropped (the window stays).
    pub fn cancel_preparing(&mut self) {
        if matches!(self.phase, Phase::Preparing { .. }) {
            self.phase = Phase::Idle;
            self.after_save = None;
            self.pending_commit = None;
            self.set_status(Status::Cancelled);
        }
    }

    /// Abandons whatever has not started writing yet (the window is closing without saving). A
    /// write in flight cannot be stopped and is left to finish.
    pub fn abandon_pending(&mut self) {
        self.after_save = None;
        match self.phase {
            Phase::ChoosingFormat | Phase::PickingPath { .. } | Phase::JpegOptions { .. } | Phase::Preparing { .. } => {
                self.phase = Phase::Idle;
                self.pending_commit = None;
            }
            Phase::Idle | Phase::Writing { .. } | Phase::Committing { .. } => {}
        }
    }

    /// The flatten was dispatched with the edit stamp `baseline` (captured after the save's own
    /// flushes).
    pub fn dispatched(&mut self, baseline: u64) {
        let Phase::Preparing { target } = self.phase.clone() else {
            return;
        };
        self.set_status(Status::Writing { name: target.display_name() });
        self.phase = Phase::Writing { target, baseline };
    }

    /// The typing tab refused the dispatch: nothing was written.
    pub fn dispatch_refused(&mut self, err: SaveError) {
        if matches!(self.phase, Phase::Preparing { .. }) {
            self.fail(err);
        }
    }

    /// The flatten worker reported. A save to a dialog target then enters `Committing` (the
    /// controller takes the target with [`Self::take_commit`]); any other success finishes the save
    /// (see [`Self::commit_finished`] for what success does). On failure nothing changes but the
    /// status, and a pending close action is dropped (the window stays open with the error).
    pub fn write_finished(&mut self, result: Result<(), SaveError>, edit_stamp_now: u64, deferred_edits_now: bool) -> Step {
        let Phase::Writing { target, baseline } = self.phase.clone() else {
            return Step::Nothing;
        };
        self.phase = Phase::Idle;
        if let Err(err) = result {
            self.fail(err);
            return Step::Nothing;
        }
        if self.pending_commit.is_some() {
            self.phase = Phase::Committing { target, baseline };
            return Step::Nothing;
        }
        self.finish_success(target, baseline, edit_stamp_now, deferred_edits_now)
    }

    /// The dialog target to commit, once (`Some` only in `Committing`). The controller commits it
    /// on a worker and reports through [`Self::commit_finished`].
    pub fn take_commit(&mut self) -> Option<DialogTarget> {
        if matches!(self.phase, Phase::Committing { .. }) { self.pending_commit.take() } else { None }
    }

    /// The commit worker reported. On success the dispatch baseline becomes the saved baseline,
    /// the target becomes the written path where paths are rewritten in place (a «Сохранить как»
    /// moves it; elsewhere the next «Сохранить» asks for a destination again) and a pending close
    /// action runs — unless the session became dirty again meanwhile (`edit_stamp_now` /
    /// `deferred_edits_now`), which asks again instead. A failure is a failed save.
    pub fn commit_finished(&mut self, result: Result<(), SaveError>, edit_stamp_now: u64, deferred_edits_now: bool) -> Step {
        let Phase::Committing { target, baseline } = self.phase.clone() else {
            return Step::Nothing;
        };
        self.phase = Phase::Idle;
        if let Err(err) = result {
            self.fail(err);
            return Step::Nothing;
        }
        self.finish_success(target, baseline, edit_stamp_now, deferred_edits_now)
    }

    /// The common end of a successful save (`write_finished` without a commit, `commit_finished`).
    fn finish_success(&mut self, target: SaveTarget, baseline: u64, edit_stamp_now: u64, deferred_edits_now: bool) -> Step {
        self.saved_baseline = baseline;
        if self.target.as_ref().map(|current| &current.path) != Some(&target.path) {
            self.retitle_to = Some(target.path.clone());
        }
        self.set_status(Status::Saved { name: target.display_name() });
        self.target = self.writes_in_place.then_some(target);
        match self.after_save.take() {
            None => Step::Nothing,
            Some(action) if self.is_dirty(edit_stamp_now, deferred_edits_now) => Step::ReturnToExitDialog(action),
            Some(action) => Step::CloseNow(action),
        }
    }

    /// Reports a failed quality persistence (the save itself is unaffected).
    pub fn quality_not_persisted(&mut self, err: String) {
        self.set_status(Status::Error(SaveError::QualityNotPersisted(err)));
    }

    /// Returns (once) the new target path after a save moved it, for the window title.
    pub fn take_retitle(&mut self) -> Option<PathBuf> {
        self.retitle_to.take()
    }

    fn start_preparing(&mut self, target: SaveTarget) {
        self.phase = Phase::Preparing { target };
        self.set_status(Status::Preparing);
    }

    /// Ends the save with `err`; a pending close action and an uncommitted dialog target are
    /// dropped (the window stays open).
    fn fail(&mut self, err: SaveError) {
        self.phase = Phase::Idle;
        self.after_save = None;
        self.pending_commit = None;
        self.set_status(Status::Error(err));
    }

    fn set_status(&mut self, status: Status) {
        self.status = Some(status);
        self.status_revision = self.status_revision.wrapping_add(1);
    }
}

/// Maps a «Сохранить как» answer onto a target written in the CHOSEN `format`: the path as named
/// when it already carries one of `format`'s extensions, else [`extension_enforced_path`]'s path.
/// Never fails: the format was chosen before the dialog, the extension only follows it.
pub(super) fn resolve_save_as_path(picked: PathBuf, format: ImageSaveFormat) -> SaveTarget {
    let path = extension_enforced_path(&picked, format).unwrap_or(picked);
    SaveTarget { path, format }
}

/// The path a «Сохранить как» answer is really written to when it does not already carry one of
/// `format`'s extensions (ASCII case-insensitive, any alias: `x.JPEG` stays for JPEG): `picked` with
/// `format`'s canonical extension appended (no extension, or an empty one, `name.`) or REPLACING the
/// other one (`x.png` -> `x.jpg` for JPEG; a non-UTF-8 extension is replaced too). `None` = written
/// as named. The one owner of that rule for the resolver and the picker worker.
pub(super) fn extension_enforced_path(picked: &Path, format: ImageSaveFormat) -> Option<PathBuf> {
    match picked.extension().and_then(std::ffi::OsStr::to_str) {
        Some(ext) if ImageSaveFormat::from_extension(ext) == Some(format) => None,
        Some(_) | None => Some(picked.with_extension(format.extension())),
    }
}

/// Review M1, generalized: the native dialog checked overwriting only for the name the user TYPED;
/// when the chosen format changes that name (extension appended or replaced), the written file is a
/// different one it never asked about. Returns the changed path when something exists there (the
/// dialog must be shown again with that full name, so the native overwrite confirmation covers it),
/// `None` when `picked` is written as named or the changed path is free. `probe` answers whether a
/// path exists; an error is treated as "may exist" (asking again is the safe side) and logged by the
/// caller-provided probe.
pub(super) fn changed_name_needs_confirmation(picked: &Path, format: ImageSaveFormat, probe: impl FnOnce(&Path) -> bool) -> Option<PathBuf> {
    extension_enforced_path(picked, format).filter(|changed| probe(changed))
}

/// The «Сохранить как» dialog's filter for the chosen `format`: its extensions (`ImageSaveFormat`'s
/// one table) plus their uppercase copies, because GTK matches filter globs case-sensitively.
pub(super) fn save_as_filter_extensions(format: ImageSaveFormat) -> Vec<String> {
    let lowercase: Vec<&'static str> = format.extensions().collect();
    lowercase.iter().map(|ext| (*ext).to_owned()).chain(lowercase.iter().map(|ext| ext.to_ascii_uppercase())).collect()
}

/// The folder and file name the «Сохранить как» dialog starts with: the current target's (else the
/// opened file's) folder, and its stem with the chosen `format`'s canonical extension.
pub(super) fn save_as_suggestion(source_path: &Path, target: Option<&SaveTarget>, format: ImageSaveFormat) -> (Option<PathBuf>, String) {
    let base = target.map_or(source_path, |target| target.path.as_path());
    let dir = base.parent().map(Path::to_path_buf);
    let stem = base.file_stem().map_or_else(|| "image".to_owned(), |stem| stem.to_string_lossy().into_owned());
    (dir, format!("{stem}.{}", format.extension()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(path: &str) -> SaveTarget {
        SaveTarget { path: PathBuf::from(path), format: ImageSaveFormat::Png }
    }

    fn jpeg(path: &str) -> SaveTarget {
        SaveTarget { path: PathBuf::from(path), format: ImageSaveFormat::Jpeg }
    }

    /// «Сохранить как…» then `format` in the panel: the machine waits on the picker.
    fn save_as(machine: &mut SaveMachine, format: ImageSaveFormat) {
        assert_eq!(machine.request_save_as(), Step::Nothing);
        assert_eq!(machine.choose_format(format), Next::OpenPicker(format));
    }

    /// A desktop-shaped dialog answer (`SaveTarget::for_test`) writing `path`.
    fn dialog(path: &str) -> DialogTarget {
        DialogTarget::for_test(PathBuf::from(path))
    }

    /// Drives a machine from `Preparing` through a successful write with dispatch baseline
    /// `baseline`, and through the commit when the save has a dialog target.
    fn save_ok(machine: &mut SaveMachine, baseline: u64) -> Step {
        machine.dispatched(baseline);
        let step = machine.write_finished(Ok(()), baseline, false);
        if machine.take_commit().is_some() {
            return machine.commit_finished(Ok(()), baseline, false);
        }
        step
    }

    #[test]
    fn fresh_session_is_clean_and_any_action_makes_it_dirty() {
        let machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        assert!(!machine.is_dirty(0, false));
        assert!(machine.is_dirty(1, false));
        assert!(machine.is_dirty(0, true), "a deferred edit (typing text, PS layer) is unsaved work");
    }

    #[test]
    fn successful_save_adopts_the_dispatch_baseline() {
        let mut machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        machine.request_save();
        assert!(matches!(machine.phase(), Phase::Preparing { .. }));
        assert_eq!(save_ok(&mut machine, 7), Step::Nothing);
        assert_eq!(machine.phase(), &Phase::Idle);
        assert!(!machine.is_dirty(7, false));
        assert_eq!(machine.status().0, Some(&Status::Saved { name: "a.png".to_owned() }));
        assert_eq!(machine.take_retitle(), None, "an in-place save keeps the title");
    }

    #[test]
    fn edit_during_write_stays_dirty() {
        let mut machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        machine.request_save();
        machine.dispatched(7);
        // Two more actions land while the worker writes.
        assert_eq!(machine.write_finished(Ok(()), 9, false), Step::Nothing);
        assert!(machine.is_dirty(9, false));
        assert!(!machine.is_dirty(7, false));
    }

    #[test]
    fn failed_write_keeps_dirty_and_drops_the_close_action() {
        let mut machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        machine.request_save_then(PendingCloseAction::Exit);
        machine.dispatched(4);
        let err = SaveError::Write { path: PathBuf::from("/home/u/a.png"), reason: "denied".to_owned() };
        assert_eq!(machine.write_finished(Err(err.clone()), 4, false), Step::Nothing);
        assert!(machine.is_dirty(4, false), "the baseline must not move on failure");
        assert_eq!(machine.status().0, Some(&Status::Error(err)));
        // The close action is gone: a later plain save does not close the window.
        machine.request_save();
        assert_eq!(save_ok(&mut machine, 4), Step::Nothing);
    }

    #[test]
    fn save_then_close_closes_only_when_clean_after_the_write() {
        let mut machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        machine.request_save_then(PendingCloseAction::ReturnToLauncher);
        assert_eq!(save_ok(&mut machine, 3), Step::CloseNow(PendingCloseAction::ReturnToLauncher));

        machine.request_save_then(PendingCloseAction::Exit);
        machine.dispatched(5);
        assert_eq!(machine.write_finished(Ok(()), 6, false), Step::ReturnToExitDialog(PendingCloseAction::Exit));
    }

    #[test]
    fn save_as_updates_the_target_and_the_title() {
        let mut machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        save_as(&mut machine, ImageSaveFormat::WebpLossless);
        assert_eq!(machine.picker_closed(Some(dialog("/home/u/b.webp"))), Step::Nothing);
        assert_eq!(save_ok(&mut machine, 2), Step::Nothing);
        assert_eq!(machine.target(), Some(&SaveTarget { path: PathBuf::from("/home/u/b.webp"), format: ImageSaveFormat::WebpLossless }));
        assert_eq!(machine.take_retitle(), Some(PathBuf::from("/home/u/b.webp")));
        assert_eq!(machine.take_retitle(), None);
        // The next «Сохранить» writes the new target without asking.
        machine.request_save();
        assert!(matches!(machine.phase(), Phase::Preparing { target } if target.path == Path::new("/home/u/b.webp")));
    }

    #[test]
    fn failed_save_as_keeps_the_old_target() {
        let mut machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        save_as(&mut machine, ImageSaveFormat::Png);
        let _ = machine.picker_closed(Some(dialog("/home/u/b.png")));
        machine.dispatched(1);
        let _ = machine.write_finished(Err(SaveError::WorkerLost), 1, false);
        assert_eq!(machine.target(), Some(&png("/home/u/a.png")));
        assert_eq!(machine.take_retitle(), None);
    }

    #[test]
    fn jpeg_options_are_shown_once_per_target() {
        let mut machine = SaveMachine::new(Some(jpeg("/home/u/a.jpg")), 95, false, true);
        machine.request_save();
        assert!(matches!(machine.phase(), Phase::JpegOptions { quality: 95, .. }));
        if let Some(quality) = machine.jpeg_options_quality_mut() {
            *quality = 80;
        }
        assert_eq!(machine.confirm_jpeg_options(), Some(80));
        assert_eq!(save_ok(&mut machine, 1), Step::Nothing);
        // Second «Сохранить» to the same target: silent.
        machine.request_save();
        assert!(matches!(machine.phase(), Phase::Preparing { .. }));
        assert_eq!(save_ok(&mut machine, 2), Step::Nothing);
        // «Сохранить как» to a .jpg always shows the options, even for the confirmed path.
        save_as(&mut machine, ImageSaveFormat::Jpeg);
        let _ = machine.picker_closed(Some(dialog("/home/u/a.jpg")));
        assert!(matches!(machine.phase(), Phase::JpegOptions { quality: 80, .. }), "the confirmed quality is the new default");
        assert_eq!(machine.confirm_jpeg_options(), Some(80));
        assert_eq!(save_ok(&mut machine, 3), Step::Nothing);
        // A different JPEG target asks again on «Сохранить» after it became the target.
        save_as(&mut machine, ImageSaveFormat::Jpeg);
        let _ = machine.picker_closed(Some(dialog("/home/u/c.jpeg")));
        assert!(matches!(machine.phase(), Phase::JpegOptions { .. }));
    }

    #[test]
    fn cancelled_jpeg_options_write_nothing() {
        let mut machine = SaveMachine::new(Some(jpeg("/home/u/a.jpg")), 95, false, true);
        machine.request_save_then(PendingCloseAction::Exit);
        assert_eq!(machine.cancel_jpeg_options(), Step::ReturnToExitDialog(PendingCloseAction::Exit));
        assert_eq!(machine.phase(), &Phase::Idle);
        // Not confirmed: the next save asks again.
        machine.request_save();
        assert!(matches!(machine.phase(), Phase::JpegOptions { .. }));
    }

    #[test]
    fn no_in_place_target_makes_save_act_as_save_as() {
        // Animated or non-writable source (gif, bmp, mismatched extension): no in-place target.
        let mut machine = SaveMachine::new(None, 95, true, true);
        assert_eq!(machine.status().0, Some(&Status::AnimatedNotice));
        machine.request_save();
        assert_eq!(machine.phase(), &Phase::ChoosingFormat, "«Сохранить» without a target opens the format panel");
        assert_eq!(machine.choose_format(ImageSaveFormat::Png), Next::OpenPicker(ImageSaveFormat::Png));
        assert!(machine.picking_path());
        // A path without an extension gets the chosen format's. (The picker worker only delivers such
        // a path once `changed_name_needs_confirmation` found that `out.png` does not exist, or the
        // user confirmed that full name in the dialog; see below.)
        let _ = machine.picker_closed(Some(dialog("/home/u/out")));
        assert!(matches!(machine.phase(), Phase::Preparing { target } if target == &png("/home/u/out.png")));
    }

    /// Format panel: «Сохранить как…» opens it, choosing a format opens the picker filtered to it,
    /// «✕» / Escape / the toggle close it with no action and no status.
    #[test]
    fn format_panel_opens_chooses_and_cancels() {
        let mut machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        assert_eq!(machine.request_save_as(), Step::Nothing);
        assert!(machine.choosing_format());
        assert!(machine.busy(), "the open panel blocks «Сохранить»");
        machine.request_save();
        assert!(machine.choosing_format(), "«Сохранить» is ignored while the panel is open");
        assert_eq!(machine.choose_format(ImageSaveFormat::Jpeg), Next::OpenPicker(ImageSaveFormat::Jpeg));
        assert_eq!(machine.phase(), &Phase::PickingPath { format: ImageSaveFormat::Jpeg });
        assert_eq!(machine.choose_format(ImageSaveFormat::Png), Next::Nothing, "no second picker");
        assert_eq!(machine.request_save_as(), Step::Nothing, "the toggle is inert while the picker is open");
        assert!(machine.picking_path());
        // The picked name follows the CHOSEN format, and JPEG always shows its options.
        let _ = machine.picker_closed(Some(dialog("/home/u/b.png")));
        assert!(matches!(machine.phase(), Phase::JpegOptions { target, .. } if target == &jpeg("/home/u/b.jpg")));
        let _ = machine.cancel_jpeg_options();

        // «✕» / Escape: idle, nothing shown.
        let (_, revision) = machine.status();
        let _ = machine.request_save_as();
        assert_eq!(machine.cancel_format_choice(), Step::Nothing);
        assert_eq!(machine.phase(), &Phase::Idle);
        assert_eq!(machine.status().1, revision, "closing the panel sets no status");
        assert_eq!(machine.cancel_format_choice(), Step::Nothing, "a second close is a no-op");
        // The «Сохранить как…» button toggles it closed.
        let _ = machine.request_save_as();
        assert_eq!(machine.request_save_as(), Step::Nothing);
        assert_eq!(machine.phase(), &Phase::Idle);
        // Closing the window while it is open drops it.
        let _ = machine.request_save_as();
        machine.abandon_pending();
        assert_eq!(machine.phase(), &Phase::Idle);
    }

    /// Exit dialog «Сохранить» without an in-place target shows the format panel; closing it (by
    /// «✕» or by the toggle) returns to the exit dialog, as a cancelled picker does.
    #[test]
    fn exit_dialog_fallback_shows_the_panel_and_cancel_returns_to_it() {
        let mut machine = SaveMachine::new(None, 95, false, true);
        machine.request_save_then(PendingCloseAction::Exit);
        assert!(machine.choosing_format());
        assert_eq!(machine.cancel_format_choice(), Step::ReturnToExitDialog(PendingCloseAction::Exit));
        assert_eq!(machine.phase(), &Phase::Idle);

        machine.request_save_then(PendingCloseAction::ReturnToLauncher);
        assert_eq!(machine.request_save_as(), Step::ReturnToExitDialog(PendingCloseAction::ReturnToLauncher));

        // A save-then-close that reaches the picker and is cancelled there also returns.
        machine.request_save_then(PendingCloseAction::Exit);
        assert_eq!(machine.choose_format(ImageSaveFormat::WebpLossless), Next::OpenPicker(ImageSaveFormat::WebpLossless));
        assert_eq!(machine.picker_closed(None), Step::ReturnToExitDialog(PendingCloseAction::Exit));

        // The exit dialog opened over an already open panel: its «Сохранить» adopts the panel.
        let mut machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        let _ = machine.request_save_as();
        machine.request_save_then(PendingCloseAction::Exit);
        assert!(machine.choosing_format());
        assert_eq!(machine.cancel_format_choice(), Step::ReturnToExitDialog(PendingCloseAction::Exit));
    }

    #[test]
    fn picker_cancel_returns_to_the_exit_dialog() {
        let mut machine = SaveMachine::new(None, 95, false, true);
        machine.request_save_then(PendingCloseAction::ReturnToLauncher);
        assert_eq!(machine.choose_format(ImageSaveFormat::Png), Next::OpenPicker(ImageSaveFormat::Png));
        assert_eq!(machine.picker_closed(None), Step::ReturnToExitDialog(PendingCloseAction::ReturnToLauncher));
        assert_eq!(machine.phase(), &Phase::Idle);
        // A plain Save As cancelled outside the exit flow just goes idle.
        save_as(&mut machine, ImageSaveFormat::Png);
        assert_eq!(machine.picker_closed(None), Step::Nothing);
        assert_eq!(machine.status().0, Some(&Status::Cancelled));
    }

    #[test]
    fn cancel_while_preparing_drops_the_save_and_the_close_action() {
        let mut machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        machine.request_save_then(PendingCloseAction::Exit);
        machine.cancel_preparing();
        assert_eq!(machine.phase(), &Phase::Idle);
        assert_eq!(machine.status().0, Some(&Status::Cancelled));
        // A late dispatch / result for the cancelled save is ignored.
        machine.dispatched(3);
        assert_eq!(machine.phase(), &Phase::Idle);
        assert_eq!(machine.write_finished(Ok(()), 3, false), Step::Nothing);
        assert!(machine.is_dirty(3, false));
        // The close action did not survive.
        machine.request_save();
        assert_eq!(save_ok(&mut machine, 3), Step::Nothing);
    }

    #[test]
    fn requests_are_ignored_while_busy_but_save_then_adopts_the_running_save() {
        let mut machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        machine.request_save();
        machine.dispatched(1);
        machine.request_save();
        assert_eq!(machine.request_save_as(), Step::Nothing);
        assert!(machine.writing());
        machine.request_save_then(PendingCloseAction::Exit);
        assert_eq!(machine.write_finished(Ok(()), 1, false), Step::CloseNow(PendingCloseAction::Exit));
    }

    #[test]
    fn abandon_keeps_a_running_write() {
        let mut machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        machine.request_save();
        machine.abandon_pending();
        assert_eq!(machine.phase(), &Phase::Idle);
        machine.request_save();
        machine.dispatched(1);
        machine.abandon_pending();
        assert!(machine.writing());
    }

    #[test]
    fn transient_status_expires_only_for_its_revision() {
        let mut machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        machine.request_save();
        let _ = save_ok(&mut machine, 1);
        let (_, revision) = machine.status();
        machine.expire_status(revision.wrapping_sub(1));
        assert!(machine.status().0.is_some(), "a stale revision must not clear a newer status");
        machine.expire_status(revision);
        assert_eq!(machine.status().0, None);
        // Errors are not transient.
        machine.quality_not_persisted("disk full".to_owned());
        let (_, revision) = machine.status();
        machine.expire_status(revision);
        assert!(machine.status().0.is_some());
    }

    #[test]
    fn save_as_commits_the_dialog_target_after_the_write() {
        let mut machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        save_as(&mut machine, ImageSaveFormat::Png);
        let _ = machine.picker_closed(Some(dialog("/home/u/b.png")));
        assert_eq!(machine.take_commit(), None, "nothing to commit before the write");
        machine.dispatched(2);
        assert_eq!(machine.write_finished(Ok(()), 2, false), Step::Nothing);
        assert!(matches!(machine.phase(), Phase::Committing { target, baseline: 2 } if target == &png("/home/u/b.png")));
        assert!(machine.writing(), "a close waits for the commit");
        assert!(machine.is_dirty(2, false), "not saved before the commit succeeded");
        let committed = machine.take_commit();
        assert_eq!(committed.as_ref().map(DialogTarget::write_path), Some(Path::new("/home/u/b.png")));
        assert_eq!(machine.take_commit(), None, "the target is handed out once");
        assert_eq!(machine.commit_finished(Ok(()), 2, false), Step::Nothing);
        assert!(!machine.is_dirty(2, false));
        assert_eq!(machine.target(), Some(&png("/home/u/b.png")));
        // The remembered desktop target is rewritten in place: no dialog, no commit.
        machine.request_save();
        machine.dispatched(3);
        assert_eq!(machine.write_finished(Ok(()), 3, false), Step::Nothing);
        assert_eq!(machine.phase(), &Phase::Idle);
    }

    #[test]
    fn failed_commit_is_a_failed_save() {
        let mut machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        save_as(&mut machine, ImageSaveFormat::Png);
        machine.request_save_then(PendingCloseAction::Exit);
        let _ = machine.picker_closed(Some(dialog("/home/u/b.png")));
        machine.dispatched(4);
        let _ = machine.write_finished(Ok(()), 4, false);
        assert!(machine.take_commit().is_some());
        let err = SaveError::Commit("denied".to_owned());
        assert_eq!(machine.commit_finished(Err(err.clone()), 4, false), Step::Nothing, "the close action is dropped");
        assert_eq!(machine.phase(), &Phase::Idle);
        assert!(machine.is_dirty(4, false));
        assert_eq!(machine.status().0, Some(&Status::Error(err)));
        assert_eq!(machine.target(), Some(&png("/home/u/a.png")), "the old target stays");
        assert_eq!(machine.take_retitle(), None);
    }

    /// Android SAF shape: paths are not the user's files. No in-place target, the dialog's write
    /// path is written as named, and EVERY «Сохранить» goes through the dialog and its commit.
    #[test]
    fn without_in_place_writes_every_save_goes_through_a_committed_dialog_target() {
        let mut machine = SaveMachine::new(Some(png("/copy/a.png")), 95, false, false);
        assert_eq!(machine.target(), None, "the opened copy is never rewritten in place");
        for round in 1..=2 {
            machine.request_save();
            assert_eq!(machine.phase(), &Phase::ChoosingFormat, "round {round}");
            assert_eq!(machine.choose_format(ImageSaveFormat::Png), Next::OpenPicker(ImageSaveFormat::Png));
            let _ = machine.picker_closed(Some(dialog("/staging/out")));
            assert!(matches!(machine.phase(), Phase::Preparing { target } if target.path == Path::new("/staging/out")), "the staging name is kept");
            machine.dispatched(round);
            assert_eq!(machine.write_finished(Ok(()), round, false), Step::Nothing);
            assert!(machine.take_commit().is_some(), "round {round} commits");
            assert_eq!(machine.commit_finished(Ok(()), round, false), Step::Nothing);
            assert!(!machine.is_dirty(round, false));
            assert_eq!(machine.target(), None, "round {round}: nothing to rewrite in place");
        }
    }

    #[test]
    fn unavailable_picker_reports_its_message() {
        let mut machine = SaveMachine::new(None, 95, false, true);
        machine.request_save_then(PendingCloseAction::Exit);
        let _ = machine.choose_format(ImageSaveFormat::Png);
        machine.picker_unavailable("No dialogs here.".to_owned());
        assert_eq!(machine.phase(), &Phase::Idle);
        assert_eq!(machine.status().0, Some(&Status::Error(SaveError::PickerUnavailable("No dialogs here.".to_owned()))));
        machine.picker_unavailable("late".to_owned());
        assert_eq!(machine.status().0, Some(&Status::Error(SaveError::PickerUnavailable("No dialogs here.".to_owned()))), "ignored outside PickingPath");
    }

    #[test]
    fn a_cancelled_save_as_drops_its_dialog_target() {
        let mut machine = SaveMachine::new(Some(png("/home/u/a.png")), 95, false, true);
        save_as(&mut machine, ImageSaveFormat::Jpeg);
        let _ = machine.picker_closed(Some(dialog("/home/u/b.jpg")));
        let _ = machine.cancel_jpeg_options();
        // The next in-place save has nothing to commit.
        machine.request_save();
        machine.dispatched(1);
        assert_eq!(machine.write_finished(Ok(()), 1, false), Step::Nothing);
        assert_eq!(machine.phase(), &Phase::Idle);
        assert_eq!(machine.take_commit(), None);
    }

    /// The chosen format is authoritative: no / empty extension -> appended; one of its own
    /// extensions in any case or alias -> kept; another extension (supported or not) -> replaced.
    #[test]
    fn resolve_save_as_path_table() {
        let cases: [(&str, ImageSaveFormat, SaveTarget); 10] = [
            ("/d/x", ImageSaveFormat::Jpeg, jpeg("/d/x.jpg")),
            ("/d/x.", ImageSaveFormat::Png, png("/d/x.png")),
            ("/d/x.png", ImageSaveFormat::Png, png("/d/x.png")),
            ("/d/x.PNG", ImageSaveFormat::Png, png("/d/x.PNG")),
            ("/d/x.jpe", ImageSaveFormat::Jpeg, jpeg("/d/x.jpe")),
            ("/d/x.JPEG", ImageSaveFormat::Jpeg, jpeg("/d/x.JPEG")),
            ("/d/x.png", ImageSaveFormat::Jpeg, jpeg("/d/x.jpg")),
            ("/d/x.JPG", ImageSaveFormat::WebpLossless, SaveTarget { path: PathBuf::from("/d/x.webp"), format: ImageSaveFormat::WebpLossless }),
            ("/d/x.tiff", ImageSaveFormat::Png, png("/d/x.png")),
            ("/d/my.page.bmp", ImageSaveFormat::Png, png("/d/my.page.png")),
        ];
        for (picked, format, expected) in cases {
            assert_eq!(resolve_save_as_path(PathBuf::from(picked), format), expected, "{picked} as {format:?}");
            let changed = extension_enforced_path(Path::new(picked), format);
            assert_eq!(changed.is_none(), Path::new(picked) == expected.path, "{picked}: None iff written as named");
        }
    }

    /// Review M1, generalized: an answer whose name the chosen format CHANGES (appended or replaced
    /// extension) is asked again when the changed file EXISTS (the native overwrite check never saw
    /// that name); a free changed name, or an answer written as named, is not.
    #[test]
    fn changed_name_is_confirmed_only_when_it_exists() {
        let exists = |path: &Path| path == Path::new("/home/u/page.png");
        assert_eq!(changed_name_needs_confirmation(Path::new("/home/u/page"), ImageSaveFormat::Png, exists), Some(PathBuf::from("/home/u/page.png")));
        assert_eq!(changed_name_needs_confirmation(Path::new("/home/u/page."), ImageSaveFormat::Png, exists), Some(PathBuf::from("/home/u/page.png")));
        assert_eq!(changed_name_needs_confirmation(Path::new("/home/u/page.jpg"), ImageSaveFormat::Png, exists), Some(PathBuf::from("/home/u/page.png")), "replaced extension");
        assert_eq!(changed_name_needs_confirmation(Path::new("/home/u/other"), ImageSaveFormat::Png, exists), None);
        assert_eq!(changed_name_needs_confirmation(Path::new("/home/u/page"), ImageSaveFormat::Jpeg, exists), None, "page.jpg does not exist");
        let never_probed = |_: &Path| -> bool { panic!("a name written as typed needs no probe") };
        assert_eq!(changed_name_needs_confirmation(Path::new("/home/u/page.png"), ImageSaveFormat::Png, never_probed), None);
        assert_eq!(changed_name_needs_confirmation(Path::new("/home/u/page.JPEG"), ImageSaveFormat::Jpeg, never_probed), None);
    }

    /// Review L3: the dialog filter is the chosen format's extensions from the one table, with
    /// uppercase copies, and nothing else.
    #[test]
    fn save_as_filter_covers_the_chosen_format_in_both_cases() {
        for format in ImageSaveFormat::save_formats() {
            let filter = save_as_filter_extensions(format);
            for ext in format.extensions() {
                assert!(filter.iter().any(|item| item == ext), "{ext}");
                assert!(filter.iter().any(|item| *item == ext.to_ascii_uppercase()), "{ext} uppercase");
            }
            assert_eq!(filter.len(), 2 * format.extensions().count(), "{format:?}");
            assert!(filter.iter().all(|item| ImageSaveFormat::from_extension(item) == Some(format)), "{format:?}");
        }
    }

    #[test]
    fn save_as_suggestion_follows_the_current_target() {
        let source = Path::new("/home/u/pics/page.gif");
        assert_eq!(save_as_suggestion(source, None, ImageSaveFormat::Png), (Some(PathBuf::from("/home/u/pics")), "page.png".to_owned()));
        let target = jpeg("/home/u/out/final.jpg");
        assert_eq!(save_as_suggestion(source, Some(&target), ImageSaveFormat::Jpeg), (Some(PathBuf::from("/home/u/out")), "final.jpg".to_owned()));
        assert_eq!(save_as_suggestion(source, Some(&target), ImageSaveFormat::WebpLossless), (Some(PathBuf::from("/home/u/out")), "final.webp".to_owned()), "the chosen format names the extension");
    }
}
