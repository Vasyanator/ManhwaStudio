/*
File: src/studio/session/image_set/machine.rs

Purpose:
The pure state machine of image-set (single-image) saving: which file each page is written to,
whether the session is dirty, the phases of one save (choose a format -> pick a file or a folder ->
JPEG options -> one flatten per page -> commit) and the status the top bar shows. No egui, no
threads, no I/O — `controller.rs` feeds it events (button presses, picker results, readiness, the
flatten results) and performs the side effects it asks for, which is what makes every transition
unit-testable.

Key structures:
- `SaveTarget`: a path plus the format it is written in.
- `PageSource`: what the machine is told about one opened page (in-place target, opened digest).
- `PageJob`: one flatten the controller must prepare and dispatch.
- `SaveMachine`: per-page targets and written digests, dirty baseline, current `Phase`, the
  running save (`Run`), JPEG quality, status.
- `Phase`, `AfterJpeg`, `Commit`, `PageResult`, `Status`, `SaveError`, `Next`, `Step`.

Key functions:
- `SaveMachine::request_save` / `request_save_as` / `request_save_then`
- `choose_format`, `cancel_format_choice`, `picker_closed`, `folder_picked`, `confirm_folder`
- `confirm_jpeg_options`, `cancel_jpeg_options`, `stop`
- `dispatched`, `dispatch_refused`, `write_finished`, `take_commit`, `commit_finished`
- `folder_file_names()`: page stems -> unique file names inside a chosen folder.
- `resolve_save_as_path()` / `extension_enforced_path()`: picked path -> target; the CHOSEN format
  is authoritative, the extension is appended or replaced to match it.

Notes:
Dirty (plan D6) = the session's monotonic EDIT STAMP differs from the baseline captured at the last
fully successful save, or an edit is still deferred (not yet counted by the stamp). The controller
defines both inputs (`controller::edit_stamp` / `deferred_edits`); this machine only compares. The
baseline is captured at the FIRST dispatch of a save (after the save's own flushes) and adopted
only when EVERY page ended written or unchanged: an edit made while pages are being written keeps
the session dirty, and a save with a failed, stopped or cancelled page changes nothing but the
status and the pages that did reach their files.

One page (the one-image session) keeps the single-file flow: «Сохранить» rewrites the page's
in-place target (or acts as «Сохранить как» without one), «Сохранить как» picks ONE file. Two or
more pages (an image set): «Сохранить» runs one pass over the pages in order — each page with an
in-place target is flattened into it unless its pixels still equal what was last written there
(`PageSave::written`, a `PixelDigest`; such a page is skipped without being encoded), each page
without one is only composed — and the changed pages that have no writable file (no target, or the
OS refused the write in place) are then written into ONE folder the user chooses; «Сохранить как»
writes every page into a chosen folder. A failing page does not stop the others.

Dialog targets (plan WP1d): a file or folder dialog answer is an `ms_platform` target. The machine
writes its write path (dir), keeps the dialog target as the run's `Commit` and, after the last
flatten of that stage, enters `Committing` until the controller's commit worker reports
(`take_commit` -> `commit_finished`); a commit failure fails every page written there. Pages
written through a dialog target adopt their new digest (and, where paths are written in place,
their new target) only after the commit succeeded. Whether a written path may later be rewritten
IN PLACE without a dialog (`writes_in_place`) is the caller's platform answer: on desktop the
dialog's write path IS the chosen file, so the opened files and the last «Сохранить как» targets
are «Сохранить» targets; where it is not (Android SAF: staging files published to documents),
there is no in-place target at all and changed pages always go through a dialog, so a copied path
is never written without its commit.
*/

use crate::studio::PendingCloseAction;
use crate::project::PixelDigest;
use crate::tabs::typing::image_encode::ImageSaveFormat;
use crate::tabs::typing::{FlattenDestination, FlattenToFileError};
use ms_platform::file_dialog::{FolderTarget, SaveTarget as DialogTarget};
use std::collections::{HashSet, VecDeque};
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
        file_display_name(&self.path)
    }
}

/// What the machine is told about one opened page (index = `Page::idx`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PageSource {
    /// The opened file (names the page in statuses and in a folder).
    pub source_path: PathBuf,
    /// `ImageSaveFormat::in_place_for` the opened file with its path; dropped by the machine where
    /// paths are not written in place.
    pub in_place: Option<SaveTarget>,
    /// Digest of the page as opened (`SourceImageFacts::opened_digest`).
    pub opened_digest: PixelDigest,
    /// Only the first frame of an animated file was opened.
    pub animated: bool,
}

/// Save state of one page.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PageSave {
    /// Where «Сохранить» rewrites this page in place; `None` = the page needs a destination.
    target: Option<SaveTarget>,
    /// Digest of the pixels last written to `target` (initially the opened file's): a page whose
    /// composite still has this digest is unchanged and is not written again.
    written: PixelDigest,
    /// Status / dialog name of the page: the opened file's name.
    name: String,
    /// Stem of the opened file, the base of the page's file name inside a folder.
    stem: String,
}

/// One flatten the controller must prepare (`prepare_flatten_to_file(page_idx)`) and dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PageJob {
    /// `Page::idx` of the page.
    pub page_idx: usize,
    /// A file to write, or compose only (an in-place pass over a page without a target).
    pub destination: FlattenDestination,
    /// The encoder of a written file (unused for compose-only).
    pub format: ImageSaveFormat,
    /// The digest that makes the page count as unchanged (nothing encoded or written).
    pub unchanged_if: Option<PixelDigest>,
}

/// What a JPEG options confirmation goes on with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AfterJpeg {
    /// Write the one page into this file (one-page «Сохранить» / «Сохранить как»).
    File(SaveTarget),
    /// Run the in-place pass over every page of an image set.
    InPlacePass,
    /// Write the folder stage's pages as JPEG.
    Folder,
}

/// Where one save currently is. Every phase but `Idle` blocks a new save request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Phase {
    /// No save in progress.
    Idle,
    /// The format panel under «Сохранить как…» is open: the user picks the format the file(s) are
    /// written in (or closes the panel). [`SaveMachine::format_panel_note`] says what it is for.
    ChoosingFormat,
    /// The native «Сохранить как» file dialog is open (on a worker thread), filtered to `format`.
    PickingPath {
        /// The format chosen in the panel; it decides the written extension (see
        /// [`extension_enforced_path`]).
        format: ImageSaveFormat,
    },
    /// The native folder dialog is open (on a worker thread); pages are written in `format`.
    PickingFolder {
        /// The format chosen in the panel.
        format: ImageSaveFormat,
    },
    /// The chosen folder already holds entries: the user confirms that same-named files are
    /// replaced (the folder is the run's pending [`Commit::Folder`]).
    ConfirmFolder {
        /// The format chosen in the panel.
        format: ImageSaveFormat,
    },
    /// The «Параметры JPEG» dialog is shown; `quality` is what its slider edits.
    JpegOptions {
        /// What a confirmation goes on with.
        next: AfterJpeg,
        /// Quality being edited, `1..=100`.
        quality: u8,
    },
    /// Waiting for `TypingTabState::prepare_flatten_to_file(job.page_idx)` to report `Ready`.
    Preparing {
        /// The flatten to dispatch.
        job: PageJob,
    },
    /// The flatten worker runs `job`.
    Writing {
        /// The flatten in flight.
        job: PageJob,
    },
    /// The stage's files were written; the commit worker publishes them to the dialog target. Like
    /// `Writing`, it cannot be cancelled.
    Committing,
}

/// The dialog target a stage's files are published to after they were written.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Commit {
    /// One file (one-page «Сохранить как»).
    File(DialogTarget),
    /// A folder (image-set folder stage).
    Folder(FolderTarget),
}

/// What a successful flatten did, as the machine needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PageResult {
    /// The page was encoded and written; the digest of what was written.
    Written(PixelDigest),
    /// The composite equals the job's `unchanged_if`: nothing was written.
    Unchanged,
    /// Compose-only on a changed page: nothing was written, the page needs a destination.
    Composed,
}

/// Why a save (or one page of it) did not happen. Typed so the localized text is chosen in one
/// place (`controller::error_text`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SaveError {
    /// The native file dialog could not be shown (its worker ended without an answer).
    PickerFailed,
    /// No dialog could be shown (another one is open, none on this platform, backend failure);
    /// the dialog layer's localized message.
    PickerUnavailable(String),
    /// The file(s) were written but could not be published to the chosen destination; the dialog
    /// layer's localized message.
    Commit(String),
    /// The typing tab refused the flatten (another flatten or an export is running).
    Busy,
    /// The scratch chapter has no such page.
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
        /// The OS refused it for lack of permission / a read-only file system: an image set
        /// writes such a page into the chosen folder instead.
        permission_denied: bool,
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
            FlattenToFileError::Write { path, reason, permission_denied } => Self::Write { path: path.clone(), reason: reason.clone(), permission_denied: *permission_denied },
            FlattenToFileError::WorkerLost => Self::WorkerLost,
        }
    }

    /// A refused in-place write that another location may accept.
    fn is_permission_denied(&self) -> bool {
        matches!(self, Self::Write { permission_denied: true, .. })
    }
}

/// The top-bar status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Status {
    /// Shown on open when `pages` pages are animated (only their first frame was opened; they have
    /// no in-place target).
    AnimatedNotice {
        /// Number of animated pages.
        pages: usize,
    },
    /// One-page save: waiting for the composite inputs.
    Preparing,
    /// One-page save: the file is being written.
    Writing {
        /// Display name of the target.
        name: String,
    },
    /// Image-set save: `done` of the stage's `total` pages processed.
    Progress {
        /// Pages finished in this stage.
        done: usize,
        /// Pages of this stage.
        total: usize,
    },
    /// Image-set save: the in-place pass left `pages` changed pages without a file; the format
    /// panel asks for a folder.
    NeedsFolder {
        /// Pages to be written into the folder.
        pages: usize,
    },
    /// One-page save succeeded (transient).
    Saved {
        /// Display name of the written file.
        name: String,
    },
    /// One-page save: the page equals what its file holds; nothing was written (transient).
    Unchanged {
        /// Display name of the file.
        name: String,
    },
    /// Image-set save: every page is written or unchanged (transient).
    SetSaved {
        /// Pages written.
        written: usize,
        /// Pages skipped as unchanged.
        unchanged: usize,
    },
    /// Image-set save: some pages failed (the others were written); stays until the next save.
    Partial {
        /// Pages written.
        written: usize,
        /// Display name and error of every failed page, in page order.
        failed: Vec<(String, SaveError)>,
    },
    /// Image-set save: stopped or cancelled after some pages were written; stays until the next save.
    Stopped {
        /// Pages written.
        written: usize,
        /// Changed pages that were not written.
        unsaved: usize,
    },
    /// The user cancelled a save (transient).
    Cancelled,
    /// The last save (or quality persistence) failed; stays until the next save attempt.
    Error(SaveError),
}

impl Status {
    /// Transient statuses disappear after a few seconds; the others stay until replaced.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Saved { .. } | Self::Unchanged { .. } | Self::SetSaved { .. } | Self::Cancelled => true,
            Self::AnimatedNotice { .. } | Self::Preparing | Self::Writing { .. } | Self::Progress { .. } | Self::NeedsFolder { .. } | Self::Partial { .. } | Self::Stopped { .. } | Self::Error(_) => false,
        }
    }
}

/// What the open format panel is for (its header line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FormatNote {
    /// One file (one-page session): no extra line.
    File,
    /// «Сохранить как» of an image set: every page goes into a folder.
    AllPages(usize),
    /// «Сохранить» of an image set: this many changed pages have no file to save into.
    Homeless(usize),
}

/// A side effect the controller must start after a format was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(super) enum Next {
    /// Nothing to start.
    Nothing,
    /// Spawn the native «Сохранить как» file dialog, filtered to this format.
    OpenPicker(ImageSaveFormat),
    /// Spawn the native folder dialog for pages written in this format.
    OpenFolderPicker(ImageSaveFormat),
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

/// The stage of a run that its jobs belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// No flatten dispatched yet (the run is in its dialogs).
    Dialogs,
    /// «Сохранить» over the pages' own files (compose-only for pages without one).
    InPlace,
    /// One-page «Сохранить как» into a picked file.
    File,
    /// Pages written into a picked folder.
    Folder,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunEnd {
    /// Every stage ran to its end.
    Completed,
    /// The user backed out in a dialog (format panel, picker, overwrite or JPEG options).
    Cancelled {
        /// Show «Сохранение отменено» (a closed format panel sets no status).
        show_status: bool,
    },
    /// The user stopped the pass.
    Stopped,
}

/// One save from its request to its end (see the file header).
#[derive(Debug)]
struct Run {
    /// A «Сохранить как» run (folder or file), not a «Сохранить».
    save_as: bool,
    stage: Stage,
    /// Jobs of the current stage not yet dispatched, in page order.
    jobs: VecDeque<PageJob>,
    /// Pages of the current stage, and how many of them finished.
    stage_total: usize,
    stage_done: usize,
    /// The save's flushes are due before the next dispatch (once per stage).
    flush_due: bool,
    /// Edit stamp at the run's first dispatch.
    baseline: Option<u64>,
    /// Pages written to their own files (in place) in this run.
    written_in_place: usize,
    /// Pages written through the dialog target and committed.
    written_committed: usize,
    unchanged: usize,
    /// Changed pages that still need a destination (page order).
    homeless: Vec<usize>,
    /// Pages that failed, with why (page order of failure).
    failed: Vec<(usize, SaveError)>,
    /// Stop after the page being written.
    stop_requested: bool,
    /// The dialog target of the current stage.
    commit: Option<Commit>,
    /// Pages written through `commit`, applied when it succeeds: page, target, written digest.
    uncommitted: Vec<(usize, SaveTarget, PixelDigest)>,
    /// What the format panel is for while it is open.
    note: FormatNote,
    /// Name of the file written last (the one-page success status).
    last_written_name: Option<String>,
}

impl Run {
    fn new(save_as: bool, note: FormatNote) -> Self {
        Self {
            save_as,
            stage: Stage::Dialogs,
            jobs: VecDeque::new(),
            stage_total: 0,
            stage_done: 0,
            flush_due: false,
            baseline: None,
            written_in_place: 0,
            written_committed: 0,
            unchanged: 0,
            homeless: Vec::new(),
            failed: Vec::new(),
            stop_requested: false,
            commit: None,
            uncommitted: Vec::new(),
            note,
            last_written_name: None,
        }
    }

    fn written(&self) -> usize {
        self.written_in_place + self.written_committed
    }
}

/// Pure image-set save state (see the file header).
#[derive(Debug)]
pub(super) struct SaveMachine {
    /// One entry per page, index = `Page::idx` (at least one for a real session).
    pages: Vec<PageSave>,
    /// Edit stamp at the last fully successful save (0 = the freshly opened state).
    saved_baseline: u64,
    phase: Phase,
    /// The save in progress (`Some` exactly while `phase != Idle`).
    run: Option<Run>,
    /// JPEG quality of the session (pre-filled from `user_config`, updated on confirm).
    jpeg_quality: u8,
    /// JPEG targets whose options were confirmed this session; «Сохранить» to them is silent.
    jpeg_confirmed: HashSet<PathBuf>,
    status: Option<Status>,
    /// Bumped on every status change, so the controller can time transient statuses.
    status_revision: u64,
    /// The close action to run once the current save succeeds (save-then-close from the exit dialog).
    after_save: Option<PendingCloseAction>,
    /// Whether a written path may be rewritten in place by a later «Сохранить» (see the header).
    writes_in_place: bool,
    /// Set when a successful save moved page 0's target to a new path (the window title follows).
    retitle_to: Option<PathBuf>,
}

impl SaveMachine {
    /// A fresh session over `pages` (page order). `writes_in_place` = paths the platform hands out
    /// are the user's own files (see the file header); without it every `in_place` is dropped.
    /// Animated pages show the first-frame notice.
    pub fn new(pages: Vec<PageSource>, jpeg_quality: u8, writes_in_place: bool) -> Self {
        let animated = pages.iter().filter(|page| page.animated).count();
        let pages = pages
            .into_iter()
            .map(|page| PageSave {
                target: page.in_place.filter(|_| writes_in_place),
                written: page.opened_digest,
                name: file_display_name(&page.source_path),
                stem: page.source_path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).filter(|stem| !stem.is_empty()).unwrap_or_else(|| "image".to_owned()),
            })
            .collect();
        let mut machine = Self {
            pages,
            saved_baseline: 0,
            phase: Phase::Idle,
            run: None,
            jpeg_quality,
            jpeg_confirmed: HashSet::new(),
            status: None,
            status_revision: 0,
            after_save: None,
            writes_in_place,
            retitle_to: None,
        };
        if animated > 0 {
            machine.set_status(Status::AnimatedNotice { pages: animated });
        }
        machine
    }

    /// Dirty = the edit stamp moved since the last fully successful save, or an edit is still
    /// deferred (plan D6; see the file header).
    pub fn is_dirty(&self, edit_stamp: u64, deferred_edits: bool) -> bool {
        edit_stamp != self.saved_baseline || deferred_edits
    }

    /// Number of pages of the session.
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// A one-page session (the single-file save flow).
    fn single(&self) -> bool {
        self.pages.len() == 1
    }

    /// The current phase (tests; the controller asks the narrower questions below).
    #[cfg(test)]
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

    /// What the open format panel is for (`None` while it is closed).
    pub fn format_panel_note(&self) -> Option<FormatNote> {
        if self.choosing_format() { self.run.as_ref().map(|run| run.note) } else { None }
    }

    /// A native file or folder dialog is open; it cannot be closed from code.
    pub fn picking_path(&self) -> bool {
        matches!(self.phase, Phase::PickingPath { .. } | Phase::PickingFolder { .. })
    }

    /// A file is being written or committed right now (cannot be cancelled).
    pub fn writing(&self) -> bool {
        matches!(self.phase, Phase::Writing { .. } | Phase::Committing)
    }

    /// An image-set pass is preparing or writing pages (the top bar offers «Стоп»).
    pub fn pass_running(&self) -> bool {
        !self.single() && matches!(self.phase, Phase::Preparing { .. } | Phase::Writing { .. })
    }

    /// The «Сохранить» target of page 0, if any (the one-page session's file).
    pub fn target(&self) -> Option<&SaveTarget> {
        self.pages.first().and_then(|page| page.target.as_ref())
    }

    /// The session's JPEG quality (pre-filled from `user_config`, or the last confirmed one).
    pub fn jpeg_quality(&self) -> u8 {
        self.jpeg_quality
    }

    /// The flatten the controller must prepare / dispatch now (`Phase::Preparing`).
    pub fn job_to_prepare(&self) -> Option<&PageJob> {
        match &self.phase {
            Phase::Preparing { job } => Some(job),
            Phase::Idle | Phase::ChoosingFormat | Phase::PickingPath { .. } | Phase::PickingFolder { .. } | Phase::ConfirmFolder { .. } | Phase::JpegOptions { .. } | Phase::Writing { .. } | Phase::Committing => None,
        }
    }

    /// The save's flushes (PS editor layers, typing text) must run before the next dispatch: once
    /// per stage, so edits made while a dialog was open reach the composite of the next stage.
    pub fn flush_due(&self) -> bool {
        self.run.as_ref().is_some_and(|run| run.flush_due)
    }

    /// The display name of the folder awaiting the overwrite confirmation.
    pub fn confirm_folder_name(&self) -> Option<&str> {
        if let (Phase::ConfirmFolder { .. }, Some(Commit::Folder(folder))) = (&self.phase, self.run.as_ref().and_then(|run| run.commit.as_ref())) { Some(folder.display_name()) } else { None }
    }

    /// The display name of page `page_idx` (its opened file's name).
    pub fn page_name(&self, page_idx: usize) -> Option<&str> {
        self.pages.get(page_idx).map(|page| page.name.as_str())
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

    /// «Сохранить». One page: write its target, or act as «Сохранить как» without one (the format
    /// panel opens). An image set: the in-place pass over every page. A JPEG target whose options
    /// were not confirmed this session shows the options first (for a set: any page's). Ignored
    /// while busy.
    pub fn request_save(&mut self) {
        if self.busy() {
            return;
        }
        if self.single() {
            match self.target().cloned() {
                None => self.open_format_panel(false),
                Some(target) => {
                    self.run = Some(Run::new(false, FormatNote::File));
                    if target.format == ImageSaveFormat::Jpeg && !self.jpeg_confirmed.contains(&target.path) {
                        self.phase = Phase::JpegOptions { next: AfterJpeg::File(target), quality: self.jpeg_quality };
                    } else {
                        self.start_in_place_pass();
                    }
                }
            }
            return;
        }
        self.run = Some(Run::new(false, FormatNote::Homeless(0)));
        let unconfirmed_jpeg = self.pages.iter().filter_map(|page| page.target.as_ref()).any(|target| target.format == ImageSaveFormat::Jpeg && !self.jpeg_confirmed.contains(&target.path));
        if unconfirmed_jpeg {
            self.phase = Phase::JpegOptions { next: AfterJpeg::InPlacePass, quality: self.jpeg_quality };
        } else {
            self.start_in_place_pass();
        }
    }

    /// «Сохранить как…» (button or hotkey) TOGGLES the format panel: opens it when idle (one page:
    /// for a file; a set: for a folder taking every page), closes it like
    /// [`Self::cancel_format_choice`] when it is open. Ignored during any other save phase.
    pub fn request_save_as(&mut self) -> Step {
        match self.phase {
            Phase::Idle => {
                self.open_format_panel(true);
                Step::Nothing
            }
            Phase::ChoosingFormat => self.cancel_format_choice(),
            Phase::PickingPath { .. } | Phase::PickingFolder { .. } | Phase::ConfirmFolder { .. } | Phase::JpegOptions { .. } | Phase::Preparing { .. } | Phase::Writing { .. } | Phase::Committing => Step::Nothing,
        }
    }

    /// «Сохранить» from the exit dialog: remembers `action` for after a fully successful save. A
    /// save already in progress (the format panel included) keeps going and adopts the action.
    pub fn request_save_then(&mut self, action: PendingCloseAction) {
        self.after_save = Some(action);
        if self.busy() {
            return;
        }
        self.request_save();
    }

    /// A format was chosen in the panel: the file dialog (one page) or the folder dialog (a set)
    /// opens; the written files carry the format's extension. Ignored unless the panel is open.
    pub fn choose_format(&mut self, format: ImageSaveFormat) -> Next {
        if self.phase != Phase::ChoosingFormat {
            return Next::Nothing;
        }
        if self.single() {
            self.phase = Phase::PickingPath { format };
            Next::OpenPicker(format)
        } else {
            self.phase = Phase::PickingFolder { format };
            Next::OpenFolderPicker(format)
        }
    }

    /// The panel was closed without a choice («✕», Escape, or the «Сохранить как…» toggle): no new
    /// file is written and, unless an earlier stage of the save already wrote pages, no status is
    /// shown; a save-then-close returns to the exit dialog, as a cancelled file dialog does.
    pub fn cancel_format_choice(&mut self) -> Step {
        if self.phase != Phase::ChoosingFormat {
            return Step::Nothing;
        }
        self.end_run(RunEnd::Cancelled { show_status: false }, 0, false)
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
            return self.end_run(RunEnd::Cancelled { show_status: true }, 0, false);
        };
        let written = picked.write_path().to_path_buf();
        let target = if self.writes_in_place { resolve_save_as_path(written, format) } else { SaveTarget { path: written, format } };
        if let Some(run) = self.run.as_mut() {
            run.commit = Some(Commit::File(picked));
        }
        if target.format == ImageSaveFormat::Jpeg {
            self.phase = Phase::JpegOptions { next: AfterJpeg::File(target), quality: self.jpeg_quality };
        } else {
            self.start_file_stage(target);
        }
        Step::Nothing
    }

    /// The folder dialog closed: `None` = cancelled (a save-then-close returns to the exit dialog);
    /// otherwise the folder and whether it already holds entries (an unreadable folder counts as
    /// holding some). A non-empty folder asks for the overwrite confirmation first.
    pub fn folder_picked(&mut self, picked: Option<(FolderTarget, bool)>) -> Step {
        let Phase::PickingFolder { format } = self.phase else {
            return Step::Nothing;
        };
        let Some((folder, has_entries)) = picked else {
            return self.end_run(RunEnd::Cancelled { show_status: true }, 0, false);
        };
        if let Some(run) = self.run.as_mut() {
            run.commit = Some(Commit::Folder(folder));
        }
        if has_entries {
            self.phase = Phase::ConfirmFolder { format };
        } else {
            self.proceed_to_folder(format);
        }
        Step::Nothing
    }

    /// The overwrite confirmation answered: `true` writes into the folder, `false` cancels the
    /// folder stage like a cancelled dialog.
    pub fn confirm_folder(&mut self, replace: bool) -> Step {
        let Phase::ConfirmFolder { format } = self.phase else {
            return Step::Nothing;
        };
        if replace {
            self.proceed_to_folder(format);
            return Step::Nothing;
        }
        self.end_run(RunEnd::Cancelled { show_status: true }, 0, false)
    }

    /// A file / folder dialog's worker ended without an answer.
    pub fn picker_failed(&mut self) {
        if self.picking_path() {
            self.fail_run(SaveError::PickerFailed);
        }
    }

    /// No dialog could be shown; `message` is the dialog layer's localized explanation.
    pub fn picker_unavailable(&mut self, message: String) {
        if self.picking_path() {
            self.fail_run(SaveError::PickerUnavailable(message));
        }
    }

    /// The quality the JPEG options slider edits, while that dialog is shown.
    pub fn jpeg_options_quality_mut(&mut self) -> Option<&mut u8> {
        match &mut self.phase {
            Phase::JpegOptions { quality, .. } => Some(quality),
            Phase::Idle | Phase::ChoosingFormat | Phase::PickingPath { .. } | Phase::PickingFolder { .. } | Phase::ConfirmFolder { .. } | Phase::Preparing { .. } | Phase::Writing { .. } | Phase::Committing => None,
        }
    }

    /// «Сохранить» in the JPEG options: adopts the quality for the session, marks the JPEG targets
    /// it covers as confirmed and starts writing. Returns the quality to persist to `user_config`.
    pub fn confirm_jpeg_options(&mut self) -> Option<u8> {
        let Phase::JpegOptions { next, quality } = self.phase.clone() else {
            return None;
        };
        self.jpeg_quality = quality;
        match next {
            AfterJpeg::File(target) => {
                self.jpeg_confirmed.insert(target.path.clone());
                if self.run.as_ref().is_some_and(|run| run.save_as) {
                    self.start_file_stage(target);
                } else {
                    self.start_in_place_pass();
                }
            }
            AfterJpeg::InPlacePass => {
                let paths: Vec<PathBuf> = self.pages.iter().filter_map(|page| page.target.as_ref()).filter(|target| target.format == ImageSaveFormat::Jpeg).map(|target| target.path.clone()).collect();
                self.jpeg_confirmed.extend(paths);
                self.start_in_place_pass();
            }
            // The folder files are marked confirmed when their commit moves the targets there.
            AfterJpeg::Folder => self.start_folder_stage(ImageSaveFormat::Jpeg),
        }
        Some(quality)
    }

    /// «Отмена» in the JPEG options: nothing more is written; a save-then-close returns to the exit
    /// dialog.
    pub fn cancel_jpeg_options(&mut self) -> Step {
        if !matches!(self.phase, Phase::JpegOptions { .. }) {
            return Step::Nothing;
        }
        self.end_run(RunEnd::Cancelled { show_status: true }, 0, false)
    }

    /// The user stopped the save («Отмена» while a one-page save prepares, «Стоп» during an
    /// image-set pass). A page still waiting for its inputs (they may never arrive if a loader
    /// worker died) is not written; a page being written finishes first and the save then ends.
    /// Files already written stay written (a folder stage still commits them); a pending close
    /// action is dropped (the window stays).
    pub fn stop(&mut self, edit_stamp_now: u64, deferred_edits_now: bool) -> Step {
        match self.phase {
            Phase::Preparing { .. } => {
                if let Some(run) = self.run.as_mut() {
                    run.stop_requested = true;
                    run.jobs.clear();
                }
                self.finish_stage(edit_stamp_now, deferred_edits_now)
            }
            Phase::Writing { .. } => {
                if let Some(run) = self.run.as_mut() {
                    run.stop_requested = true;
                }
                Step::Nothing
            }
            Phase::Idle | Phase::ChoosingFormat | Phase::PickingPath { .. } | Phase::PickingFolder { .. } | Phase::ConfirmFolder { .. } | Phase::JpegOptions { .. } | Phase::Committing => Step::Nothing,
        }
    }

    /// Abandons whatever has not started writing yet (the window is closing without saving). A
    /// write or commit in flight cannot be stopped and is left to finish; no further page starts.
    pub fn abandon_pending(&mut self) {
        self.after_save = None;
        match self.phase {
            Phase::ChoosingFormat | Phase::PickingPath { .. } | Phase::PickingFolder { .. } | Phase::ConfirmFolder { .. } | Phase::JpegOptions { .. } | Phase::Preparing { .. } => {
                self.phase = Phase::Idle;
                self.run = None;
            }
            // The page in flight finishes; no further page or stage starts.
            Phase::Writing { .. } => {
                if let Some(run) = self.run.as_mut() {
                    run.stop_requested = true;
                }
            }
            Phase::Idle | Phase::Committing => {}
        }
    }

    /// The flatten of the preparing job was dispatched with the edit stamp `baseline` (captured
    /// after the save's own flushes); the run keeps the FIRST dispatch's stamp.
    pub fn dispatched(&mut self, baseline: u64) {
        let Phase::Preparing { job } = self.phase.clone() else {
            return;
        };
        if let Some(run) = self.run.as_mut() {
            run.baseline.get_or_insert(baseline);
            run.flush_due = false;
        }
        if self.single()
            && let FlattenDestination::File(path) = &job.destination
        {
            self.set_status(Status::Writing { name: file_display_name(path) });
        }
        self.phase = Phase::Writing { job };
    }

    /// The typing tab refused the dispatch: nothing was written for that page. One page: the save
    /// fails; a set records the page as failed and goes on.
    pub fn dispatch_refused(&mut self, err: SaveError, edit_stamp_now: u64, deferred_edits_now: bool) -> Step {
        let Phase::Preparing { job } = self.phase.clone() else {
            return Step::Nothing;
        };
        if self.single() {
            self.fail_run(err);
            return Step::Nothing;
        }
        self.page_failed(job.page_idx, err);
        self.advance(edit_stamp_now, deferred_edits_now)
    }

    /// The flatten worker reported for the job being written. In-place writes adopt their digest at
    /// once; writes through a dialog target wait for its commit; a compose-only page that changed
    /// (or, in a set, a page whose in-place write the OS refused) waits for a folder; any other
    /// failure fails that page (one page: the save) and the pass goes on. The run ends after the
    /// last page (see [`Self::commit_finished`] for what a fully successful save does).
    pub fn write_finished(&mut self, result: Result<PageResult, SaveError>, edit_stamp_now: u64, deferred_edits_now: bool) -> Step {
        let Phase::Writing { job } = self.phase.clone() else {
            return Step::Nothing;
        };
        let single = self.single();
        let Some(run) = self.run.as_mut() else {
            self.phase = Phase::Idle;
            return Step::Nothing;
        };
        match result {
            Ok(PageResult::Written(digest)) => match (&job.destination, run.stage) {
                (FlattenDestination::File(path), Stage::File | Stage::Folder) => run.uncommitted.push((job.page_idx, SaveTarget { path: path.clone(), format: job.format }, digest)),
                // In place (compose-only never reports `Written`): the page's own file now holds it.
                (FlattenDestination::File(_), Stage::InPlace | Stage::Dialogs) | (FlattenDestination::ComposeOnly, _) => {
                    run.written_in_place += 1;
                    if let FlattenDestination::File(path) = &job.destination {
                        run.last_written_name = Some(file_display_name(path));
                    }
                    if let Some(page) = self.pages.get_mut(job.page_idx) {
                        page.written = digest;
                    }
                }
            },
            Ok(PageResult::Unchanged) => run.unchanged += 1,
            Ok(PageResult::Composed) => run.homeless.push(job.page_idx),
            Err(err) if !single && run.stage == Stage::InPlace && err.is_permission_denied() => run.homeless.push(job.page_idx),
            Err(err) => {
                if single {
                    self.fail_run(err);
                    return Step::Nothing;
                }
                run.failed.push((job.page_idx, err));
            }
        }
        run.stage_done += 1;
        self.advance(edit_stamp_now, deferred_edits_now)
    }

    /// The dialog target to commit, once (`Some` only in `Committing`). The controller commits it
    /// on a worker and reports through [`Self::commit_finished`].
    pub fn take_commit(&mut self) -> Option<Commit> {
        if self.phase == Phase::Committing { self.run.as_mut().and_then(|run| run.commit.take()) } else { None }
    }

    /// The commit worker reported. On success the stage's pages adopt their written digest and,
    /// where paths are rewritten in place, their new target (a «Сохранить как» moves them; elsewhere
    /// the next «Сохранить» asks for a destination again). A failure fails every page of the stage
    /// (one page: the save). Then the run ends: a fully successful one adopts the dispatch baseline
    /// and runs a pending close action — unless the session became dirty again meanwhile
    /// (`edit_stamp_now` / `deferred_edits_now`), which asks again instead.
    pub fn commit_finished(&mut self, result: Result<(), SaveError>, edit_stamp_now: u64, deferred_edits_now: bool) -> Step {
        if self.phase != Phase::Committing {
            return Step::Nothing;
        }
        let uncommitted = self.run.as_mut().map(|run| std::mem::take(&mut run.uncommitted)).unwrap_or_default();
        match result {
            Ok(()) => {
                let count = uncommitted.len();
                let last_name = uncommitted.last().map(|(_, target, _)| target.display_name());
                for (page_idx, target, digest) in uncommitted {
                    self.adopt_committed(page_idx, target, digest);
                }
                if let Some(run) = self.run.as_mut() {
                    run.written_committed += count;
                    if last_name.is_some() {
                        run.last_written_name = last_name;
                    }
                }
            }
            Err(err) => {
                if self.single() {
                    self.fail_run(err);
                    return Step::Nothing;
                }
                if let Some(run) = self.run.as_mut() {
                    run.failed.extend(uncommitted.into_iter().map(|(page_idx, _, _)| (page_idx, err.clone())));
                }
            }
        }
        let end = if self.run.as_ref().is_some_and(|run| run.stop_requested) { RunEnd::Stopped } else { RunEnd::Completed };
        self.end_run(end, edit_stamp_now, deferred_edits_now)
    }

    /// Reports a failed quality persistence (the save itself is unaffected).
    pub fn quality_not_persisted(&mut self, err: String) {
        self.set_status(Status::Error(SaveError::QualityNotPersisted(err)));
    }

    /// Returns (once) page 0's new target path after a save moved it, for the window title.
    pub fn take_retitle(&mut self) -> Option<PathBuf> {
        self.retitle_to.take()
    }

    /// Opens the format panel for a new run: a file for one page, a folder for a set.
    fn open_format_panel(&mut self, save_as: bool) {
        let note = if self.single() { FormatNote::File } else { FormatNote::AllPages(self.pages.len()) };
        self.run = Some(Run::new(save_as || self.single(), note));
        self.phase = Phase::ChoosingFormat;
    }

    /// A folder was accepted: JPEG shows its options (always, for a folder), else the pages go.
    fn proceed_to_folder(&mut self, format: ImageSaveFormat) {
        if format == ImageSaveFormat::Jpeg {
            self.phase = Phase::JpegOptions { next: AfterJpeg::Folder, quality: self.jpeg_quality };
        } else {
            self.start_folder_stage(format);
        }
    }

    /// The in-place stage: every page in order, into its target unless unchanged, compose-only
    /// without a target.
    fn start_in_place_pass(&mut self) {
        let jobs: VecDeque<PageJob> = self
            .pages
            .iter()
            .enumerate()
            .map(|(page_idx, page)| match &page.target {
                Some(target) => PageJob { page_idx, destination: FlattenDestination::File(target.path.clone()), format: target.format, unchanged_if: Some(page.written) },
                None => PageJob { page_idx, destination: FlattenDestination::ComposeOnly, format: ImageSaveFormat::Png, unchanged_if: Some(page.written) },
            })
            .collect();
        self.start_stage(Stage::InPlace, jobs);
    }

    /// The one-page «Сохранить как» stage: the page into the picked file, always written.
    fn start_file_stage(&mut self, target: SaveTarget) {
        let job = PageJob { page_idx: 0, destination: FlattenDestination::File(target.path), format: target.format, unchanged_if: None };
        self.start_stage(Stage::File, VecDeque::from([job]));
    }

    /// The folder stage: every page («Сохранить как») or the pages left without a file
    /// («Сохранить»), always written, under unique names inside the folder's write dir.
    fn start_folder_stage(&mut self, format: ImageSaveFormat) {
        let Some(run) = self.run.as_mut() else {
            return;
        };
        let Some(Commit::Folder(folder)) = &run.commit else {
            return;
        };
        let dir = folder.write_dir().to_path_buf();
        let pages: Vec<usize> = if run.save_as { (0..self.pages.len()).collect() } else { std::mem::take(&mut run.homeless) };
        let stems: Vec<&str> = pages.iter().filter_map(|page_idx| self.pages.get(*page_idx)).map(|page| page.stem.as_str()).collect();
        let names = folder_file_names(&stems, format);
        let jobs = pages.into_iter().zip(names).map(|(page_idx, name)| PageJob { page_idx, destination: FlattenDestination::File(dir.join(name)), format, unchanged_if: None }).collect();
        self.start_stage(Stage::Folder, jobs);
    }

    fn start_stage(&mut self, stage: Stage, mut jobs: VecDeque<PageJob>) {
        let single = self.single();
        let Some(run) = self.run.as_mut() else {
            return;
        };
        run.stage = stage;
        run.stage_total = jobs.len();
        run.stage_done = 0;
        run.flush_due = true;
        let total = jobs.len();
        match jobs.pop_front() {
            Some(job) => {
                run.jobs = jobs;
                self.phase = Phase::Preparing { job };
                self.set_status(if single { Status::Preparing } else { Status::Progress { done: 0, total } });
            }
            // No page to write (a session always has one, and a folder stage has at least one page
            // left without a file): nothing can be saved.
            None => self.fail_run(SaveError::NoPage),
        }
    }

    /// Records a failed page of a set.
    fn page_failed(&mut self, page_idx: usize, err: SaveError) {
        if let Some(run) = self.run.as_mut() {
            run.failed.push((page_idx, err));
            run.stage_done += 1;
        }
    }

    /// After a page finished: the next job, or the end of the stage.
    fn advance(&mut self, edit_stamp_now: u64, deferred_edits_now: bool) -> Step {
        let single = self.single();
        let Some(run) = self.run.as_mut() else {
            self.phase = Phase::Idle;
            return Step::Nothing;
        };
        let next = if run.stop_requested { None } else { run.jobs.pop_front() };
        let progress = Status::Progress { done: run.stage_done, total: run.stage_total };
        match next {
            Some(job) => {
                self.phase = Phase::Preparing { job };
                if !single {
                    self.set_status(progress);
                }
                Step::Nothing
            }
            None => self.finish_stage(edit_stamp_now, deferred_edits_now),
        }
    }

    /// The current stage has no job left: commit what it wrote through a dialog target, hand the
    /// pages left without a file to a folder, or end the run.
    fn finish_stage(&mut self, edit_stamp_now: u64, deferred_edits_now: bool) -> Step {
        let Some(run) = self.run.as_mut() else {
            self.phase = Phase::Idle;
            return Step::Nothing;
        };
        if !run.uncommitted.is_empty() && run.commit.is_some() {
            self.phase = Phase::Committing;
            return Step::Nothing;
        }
        // Nothing reached the dialog target (every page failed or the stage was stopped before its
        // first write): there is nothing to publish.
        run.commit = None;
        if run.stage == Stage::InPlace && !run.homeless.is_empty() && !run.stop_requested {
            let pages = run.homeless.len();
            run.note = FormatNote::Homeless(pages);
            self.phase = Phase::ChoosingFormat;
            self.set_status(Status::NeedsFolder { pages });
            return Step::Nothing;
        }
        let end = if run.stop_requested { RunEnd::Stopped } else { RunEnd::Completed };
        self.end_run(end, edit_stamp_now, deferred_edits_now)
    }

    /// A page written through a dialog target was committed: its digest, and where paths are
    /// rewritten in place its target (page 0's move retitles the window; a JPEG file written there
    /// had its options confirmed).
    fn adopt_committed(&mut self, page_idx: usize, target: SaveTarget, digest: PixelDigest) {
        let writes_in_place = self.writes_in_place;
        let Some(page) = self.pages.get_mut(page_idx) else {
            return;
        };
        page.written = digest;
        let moved = page.target.as_ref().map(|current| &current.path) != Some(&target.path);
        if page_idx == 0 && moved {
            self.retitle_to = Some(target.path.clone());
        }
        if writes_in_place {
            if target.format == ImageSaveFormat::Jpeg {
                self.jpeg_confirmed.insert(target.path.clone());
            }
            page.target = Some(target);
        }
    }

    /// Ends the run with `err` (a one-page save, or a dialog failure): pending close action and
    /// dialog target dropped (the window stays open with the error).
    fn fail_run(&mut self, err: SaveError) {
        self.phase = Phase::Idle;
        self.run = None;
        self.after_save = None;
        self.set_status(Status::Error(err));
    }

    /// Ends the run: a complete one — not cancelled, every page of its last stage processed (a
    /// stop during the last page still completes), no page failed and none left without a file —
    /// adopts the baseline and reports success; otherwise the session stays dirty and the status
    /// says what happened.
    fn end_run(&mut self, end: RunEnd, edit_stamp_now: u64, deferred_edits_now: bool) -> Step {
        self.phase = Phase::Idle;
        let Some(run) = self.run.take() else {
            return Step::Nothing;
        };
        let processed = run.stage != Stage::Dialogs && run.stage_done == run.stage_total;
        let complete = matches!(end, RunEnd::Completed | RunEnd::Stopped) && processed && run.failed.is_empty() && run.homeless.is_empty();
        if complete {
            if let Some(baseline) = run.baseline {
                self.saved_baseline = baseline;
            }
            let status = if !self.single() {
                Status::SetSaved { written: run.written(), unchanged: run.unchanged }
            } else if let Some(name) = run.last_written_name.clone().filter(|_| run.written() > 0) {
                Status::Saved { name }
            } else {
                let name = self.pages.first().map(|page| page.target.as_ref().map_or_else(|| page.name.clone(), SaveTarget::display_name)).unwrap_or_default();
                Status::Unchanged { name }
            };
            self.set_status(status);
            return match self.after_save.take() {
                None => Step::Nothing,
                Some(action) if self.is_dirty(edit_stamp_now, deferred_edits_now) => Step::ReturnToExitDialog(action),
                Some(action) => Step::CloseNow(action),
            };
        }
        let written = run.written();
        let status = if !run.failed.is_empty() {
            let failed = run.failed.iter().map(|(page_idx, err)| (self.page_name(*page_idx).unwrap_or_default().to_owned(), err.clone())).collect();
            Some(Status::Partial { written, failed })
        } else if written > 0 {
            let unsaved = self.pages.len().saturating_sub(written + run.unchanged);
            Some(Status::Stopped { written, unsaved })
        } else {
            match end {
                RunEnd::Cancelled { show_status: false } => None,
                RunEnd::Cancelled { show_status: true } | RunEnd::Stopped | RunEnd::Completed => Some(Status::Cancelled),
            }
        };
        if let Some(status) = status {
            self.set_status(status);
        }
        match (end, self.after_save.take()) {
            (RunEnd::Cancelled { .. }, Some(action)) => Step::ReturnToExitDialog(action),
            (RunEnd::Cancelled { .. } | RunEnd::Stopped | RunEnd::Completed, _) => Step::Nothing,
        }
    }

    fn set_status(&mut self, status: Status) {
        self.status = Some(status);
        self.status_revision = self.status_revision.wrapping_add(1);
    }
}

/// The name of `path` for statuses (the whole path when it has no file-name component).
fn file_display_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// File names for pages written into one folder: `{stem}.{ext}` in `format`'s canonical
/// extension, in the given (page) order. Names that collide — compared case-insensitively, since
/// the target file system may be — get ` (2)`, ` (3)`, … in page order, skipping any number whose
/// name is taken too, so every returned name is unique. Pure.
pub(super) fn folder_file_names(stems: &[&str], format: ImageSaveFormat) -> Vec<String> {
    let extension = format.extension();
    let mut taken: HashSet<String> = HashSet::with_capacity(stems.len());
    stems
        .iter()
        .map(|stem| {
            let mut name = format!("{stem}.{extension}");
            let mut number = 2_usize;
            while !taken.insert(name.to_lowercase()) {
                name = format!("{stem} ({number}).{extension}");
                number += 1;
            }
            name
        })
        .collect()
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

    /// A test digest; different `n` give different digests.
    fn digest(n: u8) -> PixelDigest {
        PixelDigest::of_rgba8(1, 1, &[n, 0, 0, 255])
    }

    /// A one-page session whose in-place target is `in_place` (opened digest `digest(0)`).
    fn one(in_place: Option<SaveTarget>, animated: bool, writes_in_place: bool) -> SaveMachine {
        let source_path = in_place.as_ref().map_or_else(|| PathBuf::from("/home/u/a.gif"), |target| target.path.clone());
        SaveMachine::new(vec![PageSource { source_path, in_place, opened_digest: digest(0), animated }], 95, writes_in_place)
    }

    /// A successful flatten that wrote the page (with a changed digest).
    fn written() -> Result<PageResult, SaveError> {
        Ok(PageResult::Written(digest(1)))
    }

    /// The file the preparing job writes (`None` while not preparing or compose-only).
    fn preparing_path(machine: &SaveMachine) -> Option<PathBuf> {
        match &machine.job_to_prepare()?.destination {
            FlattenDestination::File(path) => Some(path.clone()),
            FlattenDestination::ComposeOnly => None,
        }
    }

    /// A desktop-shaped dialog answer (`SaveTarget::for_test`) writing `path`.
    fn dialog(path: &str) -> DialogTarget {
        DialogTarget::for_test(PathBuf::from(path))
    }

    /// Drives a machine from `Preparing` through a successful write with dispatch baseline
    /// `baseline`, and through the commit when the save has a dialog target.
    fn save_ok(machine: &mut SaveMachine, baseline: u64) -> Step {
        machine.dispatched(baseline);
        let step = machine.write_finished(written(), baseline, false);
        if machine.take_commit().is_some() {
            return machine.commit_finished(Ok(()), baseline, false);
        }
        step
    }

    #[test]
    fn fresh_session_is_clean_and_any_action_makes_it_dirty() {
        let machine = one(Some(png("/home/u/a.png")), false, true);
        assert!(!machine.is_dirty(0, false));
        assert!(machine.is_dirty(1, false));
        assert!(machine.is_dirty(0, true), "a deferred edit (typing text, PS layer) is unsaved work");
    }

    #[test]
    fn successful_save_adopts_the_dispatch_baseline() {
        let mut machine = one(Some(png("/home/u/a.png")), false, true);
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
        let mut machine = one(Some(png("/home/u/a.png")), false, true);
        machine.request_save();
        machine.dispatched(7);
        // Two more actions land while the worker writes.
        assert_eq!(machine.write_finished(written(), 9, false), Step::Nothing);
        assert!(machine.is_dirty(9, false));
        assert!(!machine.is_dirty(7, false));
    }

    #[test]
    fn failed_write_keeps_dirty_and_drops_the_close_action() {
        let mut machine = one(Some(png("/home/u/a.png")), false, true);
        machine.request_save_then(PendingCloseAction::Exit);
        machine.dispatched(4);
        let err = SaveError::Write { path: PathBuf::from("/home/u/a.png"), reason: "denied".to_owned(), permission_denied: false };
        assert_eq!(machine.write_finished(Err(err.clone()), 4, false), Step::Nothing);
        assert!(machine.is_dirty(4, false), "the baseline must not move on failure");
        assert_eq!(machine.status().0, Some(&Status::Error(err)));
        // The close action is gone: a later plain save does not close the window.
        machine.request_save();
        assert_eq!(save_ok(&mut machine, 4), Step::Nothing);
    }

    #[test]
    fn save_then_close_closes_only_when_clean_after_the_write() {
        let mut machine = one(Some(png("/home/u/a.png")), false, true);
        machine.request_save_then(PendingCloseAction::ReturnToLauncher);
        assert_eq!(save_ok(&mut machine, 3), Step::CloseNow(PendingCloseAction::ReturnToLauncher));

        machine.request_save_then(PendingCloseAction::Exit);
        machine.dispatched(5);
        assert_eq!(machine.write_finished(written(), 6, false), Step::ReturnToExitDialog(PendingCloseAction::Exit));
    }

    #[test]
    fn save_as_updates_the_target_and_the_title() {
        let mut machine = one(Some(png("/home/u/a.png")), false, true);
        save_as(&mut machine, ImageSaveFormat::WebpLossless);
        assert_eq!(machine.picker_closed(Some(dialog("/home/u/b.webp"))), Step::Nothing);
        assert_eq!(save_ok(&mut machine, 2), Step::Nothing);
        assert_eq!(machine.target(), Some(&SaveTarget { path: PathBuf::from("/home/u/b.webp"), format: ImageSaveFormat::WebpLossless }));
        assert_eq!(machine.take_retitle(), Some(PathBuf::from("/home/u/b.webp")));
        assert_eq!(machine.take_retitle(), None);
        // The next «Сохранить» writes the new target without asking.
        machine.request_save();
        assert!(matches!(machine.phase(), Phase::Preparing { .. }) && preparing_path(&machine).as_deref() == Some(Path::new("/home/u/b.webp")));
    }

    #[test]
    fn failed_save_as_keeps_the_old_target() {
        let mut machine = one(Some(png("/home/u/a.png")), false, true);
        save_as(&mut machine, ImageSaveFormat::Png);
        let _ = machine.picker_closed(Some(dialog("/home/u/b.png")));
        machine.dispatched(1);
        let _ = machine.write_finished(Err(SaveError::WorkerLost), 1, false);
        assert_eq!(machine.target(), Some(&png("/home/u/a.png")));
        assert_eq!(machine.take_retitle(), None);
    }

    #[test]
    fn jpeg_options_are_shown_once_per_target() {
        let mut machine = one(Some(jpeg("/home/u/a.jpg")), false, true);
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
        let mut machine = one(Some(jpeg("/home/u/a.jpg")), false, true);
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
        let mut machine = one(None, true, true);
        assert_eq!(machine.status().0, Some(&Status::AnimatedNotice { pages: 1 }));
        machine.request_save();
        assert_eq!(machine.phase(), &Phase::ChoosingFormat, "«Сохранить» without a target opens the format panel");
        assert_eq!(machine.choose_format(ImageSaveFormat::Png), Next::OpenPicker(ImageSaveFormat::Png));
        assert!(machine.picking_path());
        // A path without an extension gets the chosen format's. (The picker worker only delivers such
        // a path once `changed_name_needs_confirmation` found that `out.png` does not exist, or the
        // user confirmed that full name in the dialog; see below.)
        let _ = machine.picker_closed(Some(dialog("/home/u/out")));
        assert_eq!(preparing_path(&machine), Some(PathBuf::from("/home/u/out.png")));
    }

    /// Format panel: «Сохранить как…» opens it, choosing a format opens the picker filtered to it,
    /// «✕» / Escape / the toggle close it with no action and no status.
    #[test]
    fn format_panel_opens_chooses_and_cancels() {
        let mut machine = one(Some(png("/home/u/a.png")), false, true);
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
        assert!(matches!(machine.phase(), Phase::JpegOptions { next: AfterJpeg::File(target), .. } if target == &jpeg("/home/u/b.jpg")));
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
        let mut machine = one(None, false, true);
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
        let mut machine = one(Some(png("/home/u/a.png")), false, true);
        let _ = machine.request_save_as();
        machine.request_save_then(PendingCloseAction::Exit);
        assert!(machine.choosing_format());
        assert_eq!(machine.cancel_format_choice(), Step::ReturnToExitDialog(PendingCloseAction::Exit));
    }

    #[test]
    fn picker_cancel_returns_to_the_exit_dialog() {
        let mut machine = one(None, false, true);
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
        let mut machine = one(Some(png("/home/u/a.png")), false, true);
        machine.request_save_then(PendingCloseAction::Exit);
        assert_eq!(machine.stop(0, false), Step::Nothing);
        assert_eq!(machine.phase(), &Phase::Idle);
        assert_eq!(machine.status().0, Some(&Status::Cancelled));
        // A late dispatch / result for the cancelled save is ignored.
        machine.dispatched(3);
        assert_eq!(machine.phase(), &Phase::Idle);
        assert_eq!(machine.write_finished(written(), 3, false), Step::Nothing);
        assert!(machine.is_dirty(3, false));
        // The close action did not survive.
        machine.request_save();
        assert_eq!(save_ok(&mut machine, 3), Step::Nothing);
    }

    #[test]
    fn requests_are_ignored_while_busy_but_save_then_adopts_the_running_save() {
        let mut machine = one(Some(png("/home/u/a.png")), false, true);
        machine.request_save();
        machine.dispatched(1);
        machine.request_save();
        assert_eq!(machine.request_save_as(), Step::Nothing);
        assert!(machine.writing());
        machine.request_save_then(PendingCloseAction::Exit);
        assert_eq!(machine.write_finished(written(), 1, false), Step::CloseNow(PendingCloseAction::Exit));
    }

    #[test]
    fn abandon_keeps_a_running_write() {
        let mut machine = one(Some(png("/home/u/a.png")), false, true);
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
        let mut machine = one(Some(png("/home/u/a.png")), false, true);
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
        let mut machine = one(Some(png("/home/u/a.png")), false, true);
        save_as(&mut machine, ImageSaveFormat::Png);
        let _ = machine.picker_closed(Some(dialog("/home/u/b.png")));
        assert_eq!(machine.take_commit(), None, "nothing to commit before the write");
        machine.dispatched(2);
        assert_eq!(machine.write_finished(written(), 2, false), Step::Nothing);
        assert_eq!(machine.phase(), &Phase::Committing);
        assert!(machine.writing(), "a close waits for the commit");
        assert!(machine.is_dirty(2, false), "not saved before the commit succeeded");
        let committed = machine.take_commit();
        assert!(matches!(&committed, Some(Commit::File(target)) if target.write_path() == Path::new("/home/u/b.png")), "{committed:?}");
        assert_eq!(machine.take_commit(), None, "the target is handed out once");
        assert_eq!(machine.commit_finished(Ok(()), 2, false), Step::Nothing);
        assert!(!machine.is_dirty(2, false));
        assert_eq!(machine.target(), Some(&png("/home/u/b.png")));
        // The remembered desktop target is rewritten in place: no dialog, no commit.
        machine.request_save();
        machine.dispatched(3);
        assert_eq!(machine.write_finished(written(), 3, false), Step::Nothing);
        assert_eq!(machine.phase(), &Phase::Idle);
    }

    #[test]
    fn failed_commit_is_a_failed_save() {
        let mut machine = one(Some(png("/home/u/a.png")), false, true);
        save_as(&mut machine, ImageSaveFormat::Png);
        machine.request_save_then(PendingCloseAction::Exit);
        let _ = machine.picker_closed(Some(dialog("/home/u/b.png")));
        machine.dispatched(4);
        let _ = machine.write_finished(written(), 4, false);
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
        let mut machine = one(Some(png("/copy/a.png")), false, false);
        assert_eq!(machine.target(), None, "the opened copy is never rewritten in place");
        for round in 1..=2 {
            machine.request_save();
            assert_eq!(machine.phase(), &Phase::ChoosingFormat, "round {round}");
            assert_eq!(machine.choose_format(ImageSaveFormat::Png), Next::OpenPicker(ImageSaveFormat::Png));
            let _ = machine.picker_closed(Some(dialog("/staging/out")));
            assert_eq!(preparing_path(&machine), Some(PathBuf::from("/staging/out")), "the staging name is kept");
            machine.dispatched(round);
            assert_eq!(machine.write_finished(written(), round, false), Step::Nothing);
            assert!(machine.take_commit().is_some(), "round {round} commits");
            assert_eq!(machine.commit_finished(Ok(()), round, false), Step::Nothing);
            assert!(!machine.is_dirty(round, false));
            assert_eq!(machine.target(), None, "round {round}: nothing to rewrite in place");
        }
    }

    #[test]
    fn unavailable_picker_reports_its_message() {
        let mut machine = one(None, false, true);
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
        let mut machine = one(Some(png("/home/u/a.png")), false, true);
        save_as(&mut machine, ImageSaveFormat::Jpeg);
        let _ = machine.picker_closed(Some(dialog("/home/u/b.jpg")));
        let _ = machine.cancel_jpeg_options();
        // The next in-place save has nothing to commit.
        machine.request_save();
        machine.dispatched(1);
        assert_eq!(machine.write_finished(written(), 1, false), Step::Nothing);
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

    /// An image set: `(source path, in-place target)` per page, opened digest `digest(0)` each.
    fn set(pages: &[(&str, Option<SaveTarget>)], writes_in_place: bool) -> SaveMachine {
        let pages = pages.iter().map(|(path, in_place)| PageSource { source_path: PathBuf::from(path), in_place: in_place.clone(), opened_digest: digest(0), animated: false }).collect();
        SaveMachine::new(pages, 95, writes_in_place)
    }

    /// Three PNG pages written in place.
    fn three_in_place() -> SaveMachine {
        set(&[("/home/u/a.png", Some(png("/home/u/a.png"))), ("/home/u/b.png", Some(png("/home/u/b.png"))), ("/home/u/c.png", Some(png("/home/u/c.png")))], true)
    }

    /// Finishes the preparing job with `result` (dispatch stamp `stamp`), checking it is `page`.
    fn finish_page(machine: &mut SaveMachine, page: usize, result: Result<PageResult, SaveError>, stamp: u64) -> Step {
        assert_eq!(machine.job_to_prepare().map(|job| job.page_idx), Some(page), "{:?}", machine.phase());
        machine.dispatched(stamp);
        machine.write_finished(result, stamp, false)
    }

    /// Commits the run's folder target successfully.
    fn commit_folder(machine: &mut SaveMachine, stamp: u64) -> Step {
        assert!(matches!(machine.take_commit(), Some(Commit::Folder(_))), "{:?}", machine.phase());
        machine.commit_finished(Ok(()), stamp, false)
    }

    #[test]
    fn set_save_writes_only_the_changed_page_and_adopts_the_baseline() {
        let mut machine = three_in_place();
        machine.request_save();
        assert_eq!(machine.job_to_prepare().map(|job| (job.unchanged_if, job.destination.clone())), Some((Some(digest(0)), FlattenDestination::File(PathBuf::from("/home/u/a.png")))));
        assert!(machine.flush_due(), "the save's flushes run before the first dispatch");
        assert_eq!(finish_page(&mut machine, 0, Ok(PageResult::Unchanged), 5), Step::Nothing);
        assert!(!machine.flush_due(), "flushed once per stage");
        assert_eq!(machine.status().0, Some(&Status::Progress { done: 1, total: 3 }));
        assert_eq!(finish_page(&mut machine, 1, Ok(PageResult::Written(digest(7))), 6), Step::Nothing);
        assert_eq!(finish_page(&mut machine, 2, Ok(PageResult::Unchanged), 6), Step::Nothing);
        assert_eq!(machine.phase(), &Phase::Idle);
        assert_eq!(machine.status().0, Some(&Status::SetSaved { written: 1, unchanged: 2 }));
        assert!(!machine.is_dirty(5, false), "the FIRST dispatch's stamp is the baseline");
        // The written page now compares against what was written.
        machine.request_save();
        let _ = finish_page(&mut machine, 0, Ok(PageResult::Unchanged), 5);
        assert_eq!(machine.job_to_prepare().and_then(|job| job.unchanged_if), Some(digest(7)));
    }

    #[test]
    fn one_failed_page_does_not_stop_the_others_and_keeps_the_session_dirty() {
        let mut machine = three_in_place();
        machine.request_save_then(PendingCloseAction::Exit);
        let _ = finish_page(&mut machine, 0, written(), 2);
        let _ = finish_page(&mut machine, 1, Err(SaveError::Encode("boom".to_owned())), 2);
        assert_eq!(finish_page(&mut machine, 2, written(), 2), Step::Nothing, "a failed save never closes the window");
        assert_eq!(machine.status().0, Some(&Status::Partial { written: 2, failed: vec![("b.png".to_owned(), SaveError::Encode("boom".to_owned()))] }));
        assert!(machine.is_dirty(2, false));
        // A refused dispatch fails only that page as well.
        machine.request_save();
        machine.dispatched(3);
        let _ = machine.write_finished(written(), 3, false);
        assert_eq!(machine.dispatch_refused(SaveError::Busy, 3, false), Step::Nothing);
        assert_eq!(machine.job_to_prepare().map(|job| job.page_idx), Some(2));
    }

    #[test]
    fn changed_pages_without_a_file_go_to_one_folder_and_cancel_keeps_them_dirty() {
        let mut machine = set(&[("/home/u/a.png", Some(png("/home/u/a.png"))), ("/home/u/b.gif", None), ("/home/u/c.gif", None)], true);
        machine.request_save_then(PendingCloseAction::Exit);
        assert_eq!(machine.job_to_prepare().map(|job| job.destination.clone()), Some(FlattenDestination::File(PathBuf::from("/home/u/a.png"))));
        let _ = finish_page(&mut machine, 0, Ok(PageResult::Written(digest(4))), 1);
        assert_eq!(machine.job_to_prepare().map(|job| (job.destination.clone(), job.unchanged_if)), Some((FlattenDestination::ComposeOnly, Some(digest(0)))));
        let _ = finish_page(&mut machine, 1, Ok(PageResult::Composed), 1);
        assert_eq!(finish_page(&mut machine, 2, Ok(PageResult::Unchanged), 1), Step::Nothing);
        assert!(machine.choosing_format());
        assert_eq!(machine.format_panel_note(), Some(FormatNote::Homeless(1)));
        assert_eq!(machine.status().0, Some(&Status::NeedsFolder { pages: 1 }));
        // Cancel: back to the exit dialog; page 0 stays written, the session stays dirty.
        assert_eq!(machine.cancel_format_choice(), Step::ReturnToExitDialog(PendingCloseAction::Exit));
        assert_eq!(machine.status().0, Some(&Status::Stopped { written: 1, unsaved: 1 }));
        assert!(machine.is_dirty(1, false));

        // Again, through to the folder: only the homeless page is written there.
        machine.request_save();
        let _ = finish_page(&mut machine, 0, Ok(PageResult::Unchanged), 2);
        let _ = finish_page(&mut machine, 1, Ok(PageResult::Composed), 2);
        let _ = finish_page(&mut machine, 2, Ok(PageResult::Unchanged), 2);
        assert_eq!(machine.choose_format(ImageSaveFormat::Png), Next::OpenFolderPicker(ImageSaveFormat::Png));
        assert!(machine.picking_path());
        assert_eq!(machine.folder_picked(Some((FolderTarget::for_test(PathBuf::from("/out")), false))), Step::Nothing);
        assert!(machine.flush_due(), "a new stage flushes again");
        assert_eq!(machine.job_to_prepare().map(|job| (job.destination.clone(), job.unchanged_if)), Some((FlattenDestination::File(PathBuf::from("/out/b.png")), None)));
        assert_eq!(finish_page(&mut machine, 1, Ok(PageResult::Written(digest(9))), 2), Step::Nothing);
        assert!(machine.writing(), "a close waits for the folder commit");
        assert!(machine.is_dirty(2, false), "not saved before the commit");
        assert_eq!(commit_folder(&mut machine, 2), Step::Nothing);
        assert_eq!(machine.status().0, Some(&Status::SetSaved { written: 1, unchanged: 2 }));
        assert!(!machine.is_dirty(2, false));
        assert_eq!(machine.take_retitle(), None, "page 0 did not move");
        // Desktop: the page's target moved into the folder.
        machine.request_save();
        let _ = finish_page(&mut machine, 0, Ok(PageResult::Unchanged), 2);
        assert_eq!(machine.job_to_prepare().map(|job| (job.destination.clone(), job.unchanged_if)), Some((FlattenDestination::File(PathBuf::from("/out/b.png")), Some(digest(9)))));
    }

    #[test]
    fn a_refused_in_place_write_is_rerouted_to_the_folder() {
        let mut machine = three_in_place();
        machine.request_save();
        let denied = SaveError::Write { path: PathBuf::from("/home/u/a.png"), reason: "read-only".to_owned(), permission_denied: true };
        let _ = finish_page(&mut machine, 0, Err(denied), 1);
        let _ = finish_page(&mut machine, 1, Ok(PageResult::Unchanged), 1);
        let _ = finish_page(&mut machine, 2, Err(SaveError::Write { path: PathBuf::from("/home/u/c.png"), reason: "disk full".to_owned(), permission_denied: false }), 1);
        assert_eq!(machine.format_panel_note(), Some(FormatNote::Homeless(1)), "only the permission-denied page needs a folder");
        let _ = machine.choose_format(ImageSaveFormat::WebpLossless);
        let _ = machine.folder_picked(Some((FolderTarget::for_test(PathBuf::from("/out")), false)));
        let _ = finish_page(&mut machine, 0, written(), 1);
        let _ = commit_folder(&mut machine, 1);
        assert!(matches!(machine.status().0, Some(Status::Partial { written: 1, failed }) if failed.len() == 1 && failed[0].0 == "c.png"));
        assert!(machine.is_dirty(1, false));
        assert_eq!(machine.take_retitle(), Some(PathBuf::from("/out/a.webp")), "page 0 moved");
        // A one-page session reports a refused write as an error (no folder for one file).
        let mut single = one(Some(png("/home/u/a.png")), false, true);
        single.request_save();
        single.dispatched(1);
        let denied = SaveError::Write { path: PathBuf::from("/home/u/a.png"), reason: "read-only".to_owned(), permission_denied: true };
        let _ = single.write_finished(Err(denied.clone()), 1, false);
        assert_eq!(single.status().0, Some(&Status::Error(denied)));
    }

    #[test]
    fn set_save_as_writes_every_page_into_the_folder_and_moves_the_targets() {
        let mut machine = set(&[("/home/u/p.png", Some(png("/home/u/p.png"))), ("/home/u/x/P.jpg", Some(jpeg("/home/u/x/P.jpg"))), ("/home/u/c.gif", None)], true);
        assert_eq!(machine.request_save_as(), Step::Nothing);
        assert_eq!(machine.format_panel_note(), Some(FormatNote::AllPages(3)));
        assert_eq!(machine.choose_format(ImageSaveFormat::Jpeg), Next::OpenFolderPicker(ImageSaveFormat::Jpeg));
        let _ = machine.folder_picked(Some((FolderTarget::for_test(PathBuf::from("/out")), true)));
        assert_eq!(machine.phase(), &Phase::ConfirmFolder { format: ImageSaveFormat::Jpeg }, "a non-empty folder is confirmed first");
        assert_eq!(machine.confirm_folder_name(), Some("out"));
        let _ = machine.confirm_folder(true);
        assert!(matches!(machine.phase(), Phase::JpegOptions { next: AfterJpeg::Folder, .. }), "a folder JPEG write always asks");
        assert_eq!(machine.confirm_jpeg_options(), Some(95));
        let expected = ["/out/p.jpg", "/out/P (2).jpg", "/out/c.jpg"];
        for (page, path) in expected.iter().enumerate() {
            assert_eq!(machine.job_to_prepare().map(|job| (job.destination.clone(), job.unchanged_if, job.format)), Some((FlattenDestination::File(PathBuf::from(path)), None, ImageSaveFormat::Jpeg)), "page {page}");
            let _ = finish_page(&mut machine, page, Ok(PageResult::Written(digest(10))), 4);
        }
        assert_eq!(commit_folder(&mut machine, 4), Step::Nothing);
        assert_eq!(machine.status().0, Some(&Status::SetSaved { written: 3, unchanged: 0 }));
        assert!(!machine.is_dirty(4, false));
        assert_eq!(machine.take_retitle(), Some(PathBuf::from("/out/p.jpg")));
        // Every page now saves into its folder file, silently (the JPEG options were confirmed).
        machine.request_save();
        assert_eq!(preparing_path(&machine), Some(PathBuf::from("/out/p.jpg")));
    }

    #[test]
    fn stop_ends_the_pass_after_the_current_page() {
        let mut machine = three_in_place();
        machine.request_save_then(PendingCloseAction::Exit);
        assert!(machine.pass_running());
        machine.dispatched(1);
        assert_eq!(machine.stop(1, false), Step::Nothing);
        assert!(machine.writing(), "the page in flight finishes");
        assert_eq!(machine.write_finished(written(), 1, false), Step::Nothing, "a stopped save drops the close action");
        assert_eq!(machine.phase(), &Phase::Idle);
        assert_eq!(machine.status().0, Some(&Status::Stopped { written: 1, unsaved: 2 }));
        assert!(machine.is_dirty(1, false));
        // Stopped while a page still prepares: nothing more is dispatched.
        machine.request_save();
        let _ = finish_page(&mut machine, 0, Ok(PageResult::Unchanged), 2);
        assert_eq!(machine.stop(2, false), Step::Nothing);
        assert_eq!(machine.phase(), &Phase::Idle);
        assert!(machine.is_dirty(2, false), "pages 1 and 2 were never composed");
        // A stop during the LAST page still completes the save.
        machine.request_save();
        let _ = finish_page(&mut machine, 0, Ok(PageResult::Unchanged), 3);
        let _ = finish_page(&mut machine, 1, Ok(PageResult::Unchanged), 3);
        machine.dispatched(3);
        let _ = machine.stop(3, false);
        let _ = machine.write_finished(Ok(PageResult::Unchanged), 3, false);
        assert!(!machine.is_dirty(3, false));
    }

    #[test]
    fn set_save_then_close_closes_on_success_and_returns_on_cancel() {
        let mut machine = three_in_place();
        machine.request_save_then(PendingCloseAction::ReturnToLauncher);
        let _ = finish_page(&mut machine, 0, Ok(PageResult::Unchanged), 0);
        let _ = finish_page(&mut machine, 1, written(), 0);
        assert_eq!(finish_page(&mut machine, 2, Ok(PageResult::Unchanged), 0), Step::CloseNow(PendingCloseAction::ReturnToLauncher));

        // Cancelling the folder dialog or the overwrite confirmation returns to the exit dialog.
        let mut machine = set(&[("/home/u/a.gif", None), ("/home/u/b.gif", None)], true);
        machine.request_save_then(PendingCloseAction::Exit);
        let _ = finish_page(&mut machine, 0, Ok(PageResult::Composed), 1);
        let _ = finish_page(&mut machine, 1, Ok(PageResult::Composed), 1);
        let _ = machine.choose_format(ImageSaveFormat::Png);
        assert_eq!(machine.folder_picked(None), Step::ReturnToExitDialog(PendingCloseAction::Exit));
        assert_eq!(machine.status().0, Some(&Status::Cancelled));
        machine.request_save_then(PendingCloseAction::Exit);
        let _ = finish_page(&mut machine, 0, Ok(PageResult::Composed), 1);
        let _ = finish_page(&mut machine, 1, Ok(PageResult::Composed), 1);
        let _ = machine.choose_format(ImageSaveFormat::Png);
        let _ = machine.folder_picked(Some((FolderTarget::for_test(PathBuf::from("/out")), true)));
        assert_eq!(machine.confirm_folder(false), Step::ReturnToExitDialog(PendingCloseAction::Exit));
        assert_eq!(machine.take_commit(), None, "the folder is dropped");
        assert!(machine.is_dirty(1, false));
    }

    #[test]
    fn set_jpeg_options_are_asked_once_per_session_before_an_in_place_pass() {
        let mut machine = set(&[("/home/u/a.jpg", Some(jpeg("/home/u/a.jpg"))), ("/home/u/b.png", Some(png("/home/u/b.png")))], true);
        machine.request_save();
        assert!(matches!(machine.phase(), Phase::JpegOptions { next: AfterJpeg::InPlacePass, .. }));
        assert_eq!(machine.cancel_jpeg_options(), Step::Nothing);
        assert_eq!(machine.status().0, Some(&Status::Cancelled));
        machine.request_save();
        assert_eq!(machine.confirm_jpeg_options(), Some(95));
        let _ = finish_page(&mut machine, 0, written(), 1);
        let _ = finish_page(&mut machine, 1, written(), 1);
        machine.request_save();
        assert!(matches!(machine.phase(), Phase::Preparing { .. }), "confirmed for the session");
    }

    /// Android shape: no in-place targets; changed pages go to a folder, the folder's commit
    /// adopts their digests (an unchanged page is then skipped) but no target is kept.
    #[test]
    fn without_in_place_writes_changed_pages_always_go_through_a_folder() {
        let mut machine = set(&[("/copy/a.png", Some(png("/copy/a.png"))), ("/copy/b.png", Some(png("/copy/b.png")))], false);
        machine.request_save();
        assert_eq!(machine.job_to_prepare().map(|job| job.destination.clone()), Some(FlattenDestination::ComposeOnly));
        let _ = finish_page(&mut machine, 0, Ok(PageResult::Composed), 1);
        let _ = finish_page(&mut machine, 1, Ok(PageResult::Unchanged), 1);
        let _ = machine.choose_format(ImageSaveFormat::Png);
        let _ = machine.folder_picked(Some((FolderTarget::for_test(PathBuf::from("/staging")), false)));
        let _ = finish_page(&mut machine, 0, Ok(PageResult::Written(digest(3))), 1);
        let _ = commit_folder(&mut machine, 1);
        assert!(!machine.is_dirty(1, false));
        machine.request_save();
        assert_eq!(machine.job_to_prepare().map(|job| (job.destination.clone(), job.unchanged_if)), Some((FlattenDestination::ComposeOnly, Some(digest(3)))), "no target kept, the written digest is");
    }

    #[test]
    fn a_failed_folder_commit_fails_its_pages() {
        let mut machine = set(&[("/home/u/a.gif", None), ("/home/u/b.png", Some(png("/home/u/b.png")))], true);
        machine.request_save();
        let _ = finish_page(&mut machine, 0, Ok(PageResult::Composed), 1);
        let _ = finish_page(&mut machine, 1, written(), 1);
        let _ = machine.choose_format(ImageSaveFormat::Png);
        let _ = machine.folder_picked(Some((FolderTarget::for_test(PathBuf::from("/out")), false)));
        let _ = finish_page(&mut machine, 0, written(), 1);
        assert!(machine.take_commit().is_some());
        let err = SaveError::Commit("denied".to_owned());
        assert_eq!(machine.commit_finished(Err(err.clone()), 1, false), Step::Nothing);
        assert_eq!(machine.status().0, Some(&Status::Partial { written: 1, failed: vec![("a.gif".to_owned(), err)] }));
        assert!(machine.is_dirty(1, false));
        assert_eq!(machine.take_retitle(), None);
    }

    #[test]
    fn folder_file_names_resolve_collisions_case_insensitively_in_page_order() {
        assert_eq!(folder_file_names(&["a", "A", "a (2)", "b", "a"], ImageSaveFormat::Png), ["a.png", "A (2).png", "a (2) (2).png", "b.png", "a (3).png"]);
        assert_eq!(folder_file_names(&["page", "page"], ImageSaveFormat::Jpeg), ["page.jpg", "page (2).jpg"]);
        assert!(folder_file_names(&[], ImageSaveFormat::WebpLossless).is_empty());
    }

    #[test]
    fn animated_pages_are_counted_in_the_open_notice() {
        let pages = vec![
            PageSource { source_path: PathBuf::from("/home/u/a.gif"), in_place: None, opened_digest: digest(0), animated: true },
            PageSource { source_path: PathBuf::from("/home/u/b.webp"), in_place: None, opened_digest: digest(0), animated: true },
            PageSource { source_path: PathBuf::from("/home/u/c.png"), in_place: Some(png("/home/u/c.png")), opened_digest: digest(0), animated: false },
        ];
        let machine = SaveMachine::new(pages, 95, true);
        assert_eq!(machine.status().0, Some(&Status::AnimatedNotice { pages: 2 }));
        assert_eq!(machine.page_count(), 3);
        assert_eq!(machine.page_name(1), Some("b.webp"));
    }
}
