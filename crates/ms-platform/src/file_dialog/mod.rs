/*
File: file_dialog/mod.rs

Purpose:
The one file-dialog seam of the workspace. A request never blocks the caller: it returns a
`PendingDialog` that the GUI polls once per frame; workers may use `pick_blocking`.

Key structures:
- DialogKind: the six request kinds; each encodes its Android (SAF) semantics.
- FileDialogRequest: builder (title, filters, start dir, suggested name).
- Picked / PickedFile / SaveTarget / FolderTarget: the result; the variant always matches
  the kind (see `picked_from_paths`).
- PendingDialog: poll handle (`Some` exactly once).
- FileDialogError: typed error with a localized `user_message()`.

Key functions:
- request(): GUI-safe start; pick_blocking(): worker-only blocking form.
- mime_types_for(): filter -> MIME list for SAF (`mime.rs`).

Notes:
One dialog in flight process-wide (`FileDialogError::Busy`). Backends: `desktop.rs` (rfd on a
`file-dialog` thread), `android.rs` (SAF bridge; until the Kotlin bridge lands every request
resolves to `Unavailable { FilePicker, BridgeMissing }`), `unavailable.rs` (wasm). Every
outcome is logged here at info level with the kind and file names only, never full paths.
*/

mod mime;

#[cfg(target_os = "android")]
mod android;
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
mod desktop;
#[cfg(target_arch = "wasm32")]
mod unavailable;

#[cfg(target_os = "android")]
use android as backend;
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
use desktop as backend;
#[cfg(target_arch = "wasm32")]
use unavailable as backend;

pub use mime::mime_types_for;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use ms_log::runtime_log;

use crate::{PlatformFeature, Unavailable};

/// Repaint callback a backend calls once the dialog resolves (`move || ctx.request_repaint()`).
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// What a finished dialog produced: `Ok(None)` = the user cancelled.
pub type DialogResult = Result<Option<Picked>, FileDialogError>;

/// Owned value whose drop withdraws a backend registration (Android: the pending request id).
type DropGuard = Box<dyn Send>;

/// The kind of dialog. The order is part of the Android bridge contract (`kind` is passed to
/// Kotlin as its index), so variants are only ever appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DialogKind {
    /// One existing file, readable at a local path (Android copies it into app storage).
    OpenFile,
    /// One or more existing files, readable at local paths (copied in on Android).
    OpenFiles,
    /// An existing folder, readable at a local path (copied in recursively on Android).
    OpenFolder,
    /// A new file: write the complete file to `SaveTarget::write_path`, then `commit()`.
    SaveFile,
    /// A destination folder: write into `FolderTarget::write_dir`, then `commit()`.
    SaveIntoFolder,
    /// A directory PATH kept and used in place. Needs `PersistentDirectoryPicker`.
    PickDirectoryPath,
}

/// One named filter: a label and its extensions (lowercase, without the dot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFilter {
    label: String,
    extensions: Vec<String>,
}

impl FileFilter {
    /// The user-visible filter label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Normalized extensions: lowercase, no leading dot, non-empty, no duplicates.
    #[must_use]
    pub fn extensions(&self) -> &[String] {
        &self.extensions
    }
}

/// A file-dialog request, built with the chained setters below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDialogRequest {
    kind: DialogKind,
    title: Option<String>,
    filters: Vec<FileFilter>,
    start_dir: Option<PathBuf>,
    suggested_name: Option<String>,
}

impl FileDialogRequest {
    /// A request of `kind` with no title, filters, start dir or suggested name.
    #[must_use]
    pub fn new(kind: DialogKind) -> Self {
        Self { kind, title: None, filters: Vec::new(), start_dir: None, suggested_name: None }
    }

    /// Dialog window title.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Adds a filter. Extensions are normalized (trimmed, leading dots stripped, lowercased,
    /// deduplicated); the desktop backend also matches their uppercase spelling, because the
    /// GTK / xdg-portal globs are case-sensitive. A filter left with no extension is dropped
    /// with a warning (a caller bug, never user input).
    #[must_use]
    pub fn filter(mut self, label: impl Into<String>, extensions: &[impl AsRef<str>]) -> Self {
        let label = label.into();
        let mut normalized: Vec<String> = Vec::with_capacity(extensions.len());
        for raw in extensions {
            let extension = raw.as_ref().trim().trim_start_matches('.').to_lowercase();
            if !extension.is_empty() && !normalized.contains(&extension) {
                normalized.push(extension);
            }
        }
        if normalized.is_empty() {
            runtime_log::log_warn(format!("[file-dialog] filter '{label}' has no usable extension; ignored"));
        } else {
            self.filters.push(FileFilter { label, extensions: normalized });
        }
        self
    }

    /// Initial directory. Honoured by the desktop backend only; SAF ignores it.
    #[must_use]
    pub fn start_dir(mut self, dir: &Path) -> Self {
        self.start_dir = Some(dir.to_path_buf());
        self
    }

    /// Preselected file name. Used for [`DialogKind::SaveFile`] only; ignored otherwise.
    #[must_use]
    pub fn suggested_name(mut self, name: impl Into<String>) -> Self {
        self.suggested_name = Some(name.into());
        self
    }

    /// The request kind.
    #[must_use]
    pub fn kind(&self) -> DialogKind {
        self.kind
    }

    /// The filters, in the order they were added.
    #[must_use]
    pub fn filters(&self) -> &[FileFilter] {
        &self.filters
    }
}

/// A picked input file, readable at a local path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickedFile {
    /// Readable local file (on Android a copy inside app storage).
    pub path: PathBuf,
    /// The name the user saw in the picker.
    pub display_name: String,
}

/// A save destination for one file. Write the COMPLETE file to [`Self::write_path`] first, then
/// call [`Self::commit`]. Opaque so the Android variant (staging path + document URI) can be
/// added without touching callers.
#[derive(Debug, PartialEq, Eq)]
pub struct SaveTarget {
    path: PathBuf,
    display_name: String,
}

impl SaveTarget {
    /// A target whose write path is the destination itself (desktop backend).
    #[cfg(any(not(any(target_arch = "wasm32", target_os = "android")), test))]
    fn local(path: PathBuf) -> Self {
        Self { display_name: display_name_of(&path), path }
    }

    /// A desktop-shaped target at `path`, for dependents' tests.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn for_test(path: PathBuf) -> Self {
        Self { display_name: display_name_of(&path), path }
    }

    /// Where the caller writes the file (desktop: the final path).
    #[must_use]
    pub fn write_path(&self) -> &Path {
        &self.path
    }

    /// The chosen file name, for status messages.
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Publishes the written file to the chosen destination. Desktop: no-op (the file already
    /// is the destination). Android (bridge pending): streams the file to the document URI,
    /// which BLOCKS, so call it on the worker that finished the write.
    ///
    /// # Errors
    /// [`FileDialogError::Commit`] when the destination could not be written.
    pub fn commit(self) -> Result<(), FileDialogError> {
        Ok(())
    }
}

/// A destination folder. Write into [`Self::write_dir`], then call [`Self::commit`]. Opaque for
/// the same reason as [`SaveTarget`].
#[derive(Debug, PartialEq, Eq)]
pub struct FolderTarget {
    dir: PathBuf,
    display_name: String,
}

impl FolderTarget {
    /// A target whose write dir is the destination itself (desktop backend).
    #[cfg(any(not(any(target_arch = "wasm32", target_os = "android")), test))]
    fn local(dir: PathBuf) -> Self {
        Self { display_name: display_name_of(&dir), dir }
    }

    /// A desktop-shaped target at `dir`, for dependents' tests.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn for_test(dir: PathBuf) -> Self {
        Self { display_name: display_name_of(&dir), dir }
    }

    /// Where the caller writes (desktop: the chosen folder itself).
    #[must_use]
    pub fn write_dir(&self) -> &Path {
        &self.dir
    }

    /// The chosen folder name, for status messages.
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Whether the chosen destination already holds entries (an overwrite-confirmation input).
    /// Blocking directory read: a short one on desktop, a JVM round trip on Android.
    ///
    /// # Errors
    /// [`FileDialogError::TargetIo`] when the folder cannot be listed.
    pub fn has_entries(&self) -> Result<bool, FileDialogError> {
        std::fs::read_dir(&self.dir)
            .map(|mut entries| entries.next().is_some())
            .map_err(|err| FileDialogError::TargetIo { detail: err.to_string() })
    }

    /// Publishes the written folder. Desktop: no-op. Android (bridge pending): copies the
    /// staging folder into the document tree, which BLOCKS (worker only).
    ///
    /// # Errors
    /// [`FileDialogError::Commit`] when the destination could not be written.
    pub fn commit(self) -> Result<(), FileDialogError> {
        Ok(())
    }
}

/// A dialog result. The variant always matches the request kind: `OpenFile` / `OpenFiles` ->
/// `Files` (exactly one / at least one), `OpenFolder` -> `Folder`, `SaveFile` -> `SaveTarget`,
/// `SaveIntoFolder` -> `FolderTarget`, `PickDirectoryPath` -> `DirectoryPath`.
#[derive(Debug, PartialEq, Eq)]
pub enum Picked {
    /// Picked input files, readable locally.
    Files(Vec<PickedFile>),
    /// A picked input folder, readable locally.
    Folder(PathBuf),
    /// A file save destination.
    SaveTarget(SaveTarget),
    /// A folder save destination.
    FolderTarget(FolderTarget),
    /// A directory path used in place.
    DirectoryPath(PathBuf),
}

impl Picked {
    /// The single file of an `OpenFile` result; `None` for any other variant.
    #[must_use]
    pub fn into_file(self) -> Option<PickedFile> {
        match self {
            Picked::Files(files) => files.into_iter().next(),
            Picked::Folder(_) | Picked::SaveTarget(_) | Picked::FolderTarget(_) | Picked::DirectoryPath(_) => None,
        }
    }

    /// The files of an `OpenFile` / `OpenFiles` result; `None` for any other variant.
    #[must_use]
    pub fn into_files(self) -> Option<Vec<PickedFile>> {
        match self {
            Picked::Files(files) => Some(files),
            Picked::Folder(_) | Picked::SaveTarget(_) | Picked::FolderTarget(_) | Picked::DirectoryPath(_) => None,
        }
    }

    /// The folder of an `OpenFolder` result; `None` for any other variant.
    #[must_use]
    pub fn into_folder(self) -> Option<PathBuf> {
        match self {
            Picked::Folder(dir) => Some(dir),
            Picked::Files(_) | Picked::SaveTarget(_) | Picked::FolderTarget(_) | Picked::DirectoryPath(_) => None,
        }
    }

    /// The target of a `SaveFile` result; `None` for any other variant.
    #[must_use]
    pub fn into_save_target(self) -> Option<SaveTarget> {
        match self {
            Picked::SaveTarget(target) => Some(target),
            Picked::Files(_) | Picked::Folder(_) | Picked::FolderTarget(_) | Picked::DirectoryPath(_) => None,
        }
    }

    /// The target of a `SaveIntoFolder` result; `None` for any other variant.
    #[must_use]
    pub fn into_folder_target(self) -> Option<FolderTarget> {
        match self {
            Picked::FolderTarget(target) => Some(target),
            Picked::Files(_) | Picked::Folder(_) | Picked::SaveTarget(_) | Picked::DirectoryPath(_) => None,
        }
    }

    /// The path of a `PickDirectoryPath` result; `None` for any other variant.
    #[must_use]
    pub fn into_directory_path(self) -> Option<PathBuf> {
        match self {
            Picked::DirectoryPath(dir) => Some(dir),
            Picked::Files(_) | Picked::Folder(_) | Picked::SaveTarget(_) | Picked::FolderTarget(_) => None,
        }
    }

    /// File names for the outcome log line (never full paths).
    fn log_names(&self) -> String {
        match self {
            Picked::Files(files) => files.iter().map(|file| file.display_name.as_str()).collect::<Vec<_>>().join(", "),
            Picked::Folder(dir) | Picked::DirectoryPath(dir) => display_name_of(dir),
            Picked::SaveTarget(target) => target.display_name.clone(),
            Picked::FolderTarget(target) => target.display_name.clone(),
        }
    }
}

/// A file-dialog failure. `Display` is the localized [`Self::user_message`].
#[derive(Debug)]
pub enum FileDialogError {
    /// The platform (or this build) has no dialog for the request.
    Unavailable(Unavailable),
    /// Another dialog is already open in this process.
    Busy,
    /// The dialog backend failed (e.g. its worker thread could not start).
    Backend { detail: String },
    /// A picked input could not be copied into app storage (Android).
    CopyIn { name: String, detail: String },
    /// A save target could not be published to its destination (Android).
    Commit { detail: String },
    /// The chosen destination folder could not be listed.
    TargetIo { detail: String },
    /// The dialog worker ended without reporting a result.
    WorkerLost,
}

impl FileDialogError {
    /// The i18n key of the message (the feature/reason sentences for `Unavailable`).
    const fn message_key(&self) -> &'static str {
        match self {
            FileDialogError::Unavailable(unavailable) => unavailable.feature.message_key(),
            FileDialogError::Busy => "platform.file_dialog.busy_error",
            FileDialogError::Backend { .. } => "platform.file_dialog.backend_error",
            FileDialogError::CopyIn { .. } => "platform.file_dialog.copy_in_error",
            FileDialogError::Commit { .. } => "platform.file_dialog.commit_error",
            FileDialogError::TargetIo { .. } => "platform.file_dialog.target_io_error",
            FileDialogError::WorkerLost => "platform.file_dialog.worker_lost_error",
        }
    }

    /// Localized message for a status line or toast.
    #[must_use]
    pub fn user_message(&self) -> String {
        let template = ms_i18n::resolve_key(self.message_key());
        match self {
            FileDialogError::Unavailable(unavailable) => unavailable.user_message(),
            FileDialogError::Busy | FileDialogError::WorkerLost => template.to_owned(),
            FileDialogError::Backend { detail } | FileDialogError::Commit { detail } | FileDialogError::TargetIo { detail } => {
                ms_i18n::interpolate(template, &[("detail", detail as &dyn fmt::Display)])
            }
            FileDialogError::CopyIn { name, detail } => {
                ms_i18n::interpolate(template, &[("name", name as &dyn fmt::Display), ("detail", detail as &dyn fmt::Display)])
            }
        }
    }
}

impl fmt::Display for FileDialogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.user_message())
    }
}

impl std::error::Error for FileDialogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FileDialogError::Unavailable(unavailable) => Some(unavailable),
            FileDialogError::Busy
            | FileDialogError::Backend { .. }
            | FileDialogError::CopyIn { .. }
            | FileDialogError::Commit { .. }
            | FileDialogError::TargetIo { .. }
            | FileDialogError::WorkerLost => None,
        }
    }
}

impl From<Unavailable> for FileDialogError {
    fn from(unavailable: Unavailable) -> Self {
        FileDialogError::Unavailable(unavailable)
    }
}

/// Poll handle of a requested dialog. Store it in UI state and call [`Self::poll`] each frame;
/// dropping it abandons the result (the open dialog itself stays until the user closes it).
pub struct PendingDialog {
    state: PendingState,
    // Held only for its drop effect: withdraws the backend registration (Android request id).
    _registration: Option<DropGuard>,
}

enum PendingState {
    Ready(DialogResult),
    Waiting(Receiver<DialogResult>),
    Finished,
}

impl PendingDialog {
    fn resolved(result: DialogResult) -> Self {
        Self { state: PendingState::Ready(result), _registration: None }
    }

    /// An already-resolved handle, for dependents' tests of their pending-dialog state machine.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn ready(result: DialogResult) -> Self {
        Self::resolved(result)
    }

    /// Non-blocking check. `None` while the dialog is open (and after the result was taken);
    /// `Some(result)` exactly once. A worker that vanished yields
    /// `Some(Err(FileDialogError::WorkerLost))`.
    pub fn poll(&mut self) -> Option<DialogResult> {
        match std::mem::replace(&mut self.state, PendingState::Finished) {
            PendingState::Ready(result) => Some(result),
            PendingState::Waiting(receiver) => match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => {
                    self.state = PendingState::Waiting(receiver);
                    None
                }
                Err(TryRecvError::Disconnected) => Some(Err(FileDialogError::WorkerLost)),
            },
            PendingState::Finished => None,
        }
    }

    /// Blocks until the dialog resolves (worker threads only).
    fn wait(mut self) -> DialogResult {
        match std::mem::replace(&mut self.state, PendingState::Finished) {
            PendingState::Ready(result) => result,
            PendingState::Waiting(receiver) => receiver.recv().unwrap_or(Err(FileDialogError::WorkerLost)),
            // `wait` is only called on a fresh handle (`pick_blocking`), so the result was never taken.
            PendingState::Finished => Err(FileDialogError::WorkerLost),
        }
    }
}

impl fmt::Debug for PendingDialog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match self.state {
            PendingState::Ready(_) => "ready",
            PendingState::Waiting(_) => "waiting",
            PendingState::Finished => "finished",
        };
        f.debug_struct("PendingDialog").field("state", &state).finish_non_exhaustive()
    }
}

/// The process-wide "one dialog in flight" flag.
static DIALOG_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Holds the in-flight flag; dropping it lets the next request through.
struct BusyGuard(&'static AtomicBool);

impl BusyGuard {
    fn acquire(flag: &'static AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).ok().map(|_| Self(flag))
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The backend's half of a request: delivers the one result to the [`PendingDialog`].
pub(crate) struct Completion {
    kind: DialogKind,
    sender: Sender<DialogResult>,
    busy: BusyGuard,
    waker: Option<Waker>,
}

impl Completion {
    /// Logs the outcome, releases the in-flight flag (before delivery, so the requester can
    /// immediately open the next dialog), delivers the result and wakes the requester.
    pub(crate) fn finish(self, result: DialogResult) {
        let Completion { kind, sender, busy, waker } = self;
        log_outcome(kind, &result);
        drop(busy);
        if sender.send(result).is_err() {
            runtime_log::log_info(format!("[file-dialog] {kind:?} finished after its requester was dropped; result discarded"));
        }
        if let Some(waker) = waker {
            waker();
        }
    }
}

/// Starts a dialog without blocking (safe on the GUI thread). `waker` is called once when the
/// result is ready. Unavailability and `Busy` resolve immediately (the waker is still called).
#[must_use]
pub fn request(request: FileDialogRequest, waker: Option<Waker>) -> PendingDialog {
    request_with(&DIALOG_IN_FLIGHT, request, waker, backend::launch)
}

/// Opens a dialog and blocks until it closes. Worker threads only (never the GUI thread).
///
/// # Errors
/// Any [`FileDialogError`]; `Ok(None)` means the user cancelled.
pub fn pick_blocking(request: FileDialogRequest) -> DialogResult {
    self::request(request, None).wait()
}

/// [`request`] with an injectable in-flight flag and backend (tests drive both).
fn request_with(
    flag: &'static AtomicBool,
    request: FileDialogRequest,
    waker: Option<Waker>,
    launch: impl FnOnce(FileDialogRequest, Completion) -> Result<Option<DropGuard>, FileDialogError>,
) -> PendingDialog {
    let kind = request.kind;
    let busy = match required_feature_check(kind) {
        Ok(()) => BusyGuard::acquire(flag).ok_or(FileDialogError::Busy),
        Err(unavailable) => Err(unavailable.into()),
    };
    let busy = match busy {
        Ok(busy) => busy,
        Err(error) => return resolve_now(kind, error, waker),
    };
    let (sender, receiver) = mpsc::channel();
    let completion = Completion { kind, sender, busy, waker: waker.clone() };
    match launch(request, completion) {
        Ok(registration) => PendingDialog { state: PendingState::Waiting(receiver), _registration: registration },
        // The backend dropped the completion, which already released the in-flight flag.
        Err(error) => resolve_now(kind, error, waker),
    }
}

/// A request that failed before (or while) reaching its backend: logged, woken, resolved.
fn resolve_now(kind: DialogKind, error: FileDialogError, waker: Option<Waker>) -> PendingDialog {
    runtime_log::log_info(format!("[file-dialog] {kind:?} rejected: {error:?}"));
    if let Some(waker) = waker {
        waker();
    }
    PendingDialog::resolved(Err(error))
}

/// Every kind needs `FilePicker`; a path used in place also needs `PersistentDirectoryPicker`.
fn required_feature_check(kind: DialogKind) -> Result<(), Unavailable> {
    PlatformFeature::FilePicker.require()?;
    match kind {
        DialogKind::PickDirectoryPath => PlatformFeature::PersistentDirectoryPicker.require(),
        DialogKind::OpenFile | DialogKind::OpenFiles | DialogKind::OpenFolder | DialogKind::SaveFile | DialogKind::SaveIntoFolder => Ok(()),
    }
}

/// Maps the local paths a backend returned to the `Picked` variant of `kind`. An empty list is
/// a cancel (`None`); kinds that pick one item keep the first path.
/// Compiled where a backend returns plain paths (desktop) and for tests; the SAF backend builds
/// its targets from staging paths + URIs instead.
#[cfg(any(not(any(target_arch = "wasm32", target_os = "android")), test))]
fn picked_from_paths(kind: DialogKind, paths: Vec<PathBuf>) -> Option<Picked> {
    let mut paths = paths.into_iter();
    let picked = match kind {
        DialogKind::OpenFile => Picked::Files(vec![picked_file(paths.next()?)]),
        DialogKind::OpenFiles => {
            let files: Vec<PickedFile> = paths.map(picked_file).collect();
            if files.is_empty() {
                return None;
            }
            Picked::Files(files)
        }
        DialogKind::OpenFolder => Picked::Folder(paths.next()?),
        DialogKind::SaveFile => Picked::SaveTarget(SaveTarget::local(paths.next()?)),
        DialogKind::SaveIntoFolder => Picked::FolderTarget(FolderTarget::local(paths.next()?)),
        DialogKind::PickDirectoryPath => Picked::DirectoryPath(paths.next()?),
    };
    Some(picked)
}

#[cfg(any(not(any(target_arch = "wasm32", target_os = "android")), test))]
fn picked_file(path: PathBuf) -> PickedFile {
    let display_name = display_name_of(&path);
    PickedFile { path, display_name }
}

/// The last path component, or the whole path for a root such as `/` or `C:\`.
fn display_name_of(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

fn log_outcome(kind: DialogKind, result: &DialogResult) {
    match result {
        Ok(None) => runtime_log::log_info(format!("[file-dialog] {kind:?} cancelled")),
        Ok(Some(picked)) => runtime_log::log_info(format!("[file-dialog] {kind:?} picked: {}", picked.log_names())),
        Err(error) => runtime_log::log_warn(format!("[file-dialog] {kind:?} failed: {error:?}")),
    }
}

#[cfg(test)]
mod tests;
