/*
File: crates/ms-launcher/src/open_image.rs

Purpose:
"Open image" flow of the main menu (single-image mode entry): the native file picker on a
worker thread, the launcher-side validation of the picked path, and the mapping of a pick
onto the launcher outcome. Native only — the wasm build has no picker and no button (D10 of
`dev-docs/single_image_mode_plan.md`).

Key items:
- `spawn_image_picker()`: runs the blocking `ms_platform::file_dialog::pick_blocking` and the
  validation on a worker; the GUI thread only polls the returned channel.
- `image_picker_request()`: the `OpenFile` request, its filter built from the ONE input-type
  table `ms_config::single_image::input_extensions()` (the dialog seam adds the uppercase
  spellings case-sensitive GTK globs need).
- `pick_result_from_dialog()`: maps a dialog result onto `ImagePickResult` (cancel, dialog
  failure, validated pick).
- `validate_picked_path()`: the launcher's only check — the path exists and is a file.
- `launcher_outcome_for_pick()`: pure mapping of an `ImagePickResult` onto
  `LauncherOutcome::OpenImage`.

Notes:
Decoding is deliberately NOT attempted here: the studio loading screen owns every decode
error (`SingleImageError::user_message`), so the launcher never duplicates that decision.
*/

use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

use ms_platform::file_dialog::{self, DialogKind, DialogResult, FileDialogRequest};

use crate::state::LauncherOutcome;

/// Result of one "Open image" pick, produced on the picker worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImagePickResult {
    /// The user closed the dialog without choosing a file.
    Cancelled,
    /// The chosen path exists and is a file (decodability is not checked).
    Selected(PathBuf),
    /// The chosen path failed launcher validation.
    Rejected(OpenImageRejection),
}

/// Why a picked path cannot be handed to the studio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenImageRejection {
    /// Nothing exists at the path (e.g. deleted between listing and confirming).
    NotFound(PathBuf),
    /// The path exists but is not a regular file (a directory, a device, ...).
    NotAFile(PathBuf),
    /// The file metadata could not be read; `error` is the OS error text for the log.
    Inaccessible { path: PathBuf, error: String },
    /// The file dialog itself failed (another dialog open, no picker on this platform, a
    /// backend failure). `message` is the localized status, `detail` the typed error for the log.
    Dialog { message: String, detail: String },
}

impl OpenImageRejection {
    /// Localized one-line status shown under the main menu.
    pub fn user_message(&self) -> String {
        match self {
            OpenImageRejection::NotFound(path) => tf!("launcher.main.open_image_not_found_error", path = path.display()),
            OpenImageRejection::NotAFile(path) => tf!("launcher.main.open_image_not_a_file_error", path = path.display()),
            OpenImageRejection::Inaccessible { path, .. } => tf!("launcher.main.open_image_access_error", path = path.display()),
            OpenImageRejection::Dialog { message, .. } => message.clone(),
        }
    }

    /// Diagnostic line for the session log (path plus OS error when there is one).
    pub fn log_message(&self) -> String {
        match self {
            OpenImageRejection::NotFound(path) => format!("[launcher-open-image] picked path does not exist; path='{}'", path.display()),
            OpenImageRejection::NotAFile(path) => format!("[launcher-open-image] picked path is not a file; path='{}'", path.display()),
            OpenImageRejection::Inaccessible { path, error } => format!(
                "[launcher-open-image] cannot read metadata of the picked path; path='{}'; error={error}; possible cause: permissions or a broken link",
                path.display()
            ),
            OpenImageRejection::Dialog { detail, .. } => format!("[launcher-open-image] file dialog failed; error={detail}"),
        }
    }
}

/// The single-file request of the "Open image" picker: one filter listing every input
/// extension in table order. Uppercase spellings are not listed here: the desktop dialog
/// backend adds them for the case-sensitive GTK / xdg-portal globs.
pub fn image_picker_request() -> FileDialogRequest {
    let extensions: Vec<&'static str> = ms_config::single_image::input_extensions().collect();
    FileDialogRequest::new(DialogKind::OpenFile).filter(t!("launcher.main.open_image_filter"), &extensions)
}

/// Maps a finished dialog onto the pick result: a cancel stays `Cancelled`, a dialog failure
/// becomes `Rejected(Dialog)`, a picked file goes through `validate_picked_path` (blocking
/// metadata read: call only off the GUI thread).
pub fn pick_result_from_dialog(result: DialogResult) -> ImagePickResult {
    match result {
        Ok(None) => ImagePickResult::Cancelled,
        Ok(Some(picked)) => match picked.into_file() {
            Some(file) => match validate_picked_path(file.path) {
                Ok(path) => ImagePickResult::Selected(path),
                Err(rejection) => ImagePickResult::Rejected(rejection),
            },
            // `OpenFile` always yields `Picked::Files` with one file (the seam's contract);
            // anything else is a backend bug, reported like a picker failure.
            None => ImagePickResult::Rejected(OpenImageRejection::Dialog {
                message: t!("launcher.main.open_image_picker_error").to_owned(),
                detail: "OpenFile dialog returned a result without a file".to_owned(),
            }),
        },
        Err(error) => ImagePickResult::Rejected(OpenImageRejection::Dialog { message: error.user_message(), detail: format!("{error:?}") }),
    }
}

/// Checks that `path` names an existing regular file (symlinks are followed). Blocking file
/// I/O: call only off the GUI thread. Errors: `NotFound` when nothing is there, `NotAFile` for
/// directories and other non-files, `Inaccessible` for any other metadata error.
pub fn validate_picked_path(path: PathBuf) -> Result<PathBuf, OpenImageRejection> {
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => Ok(path),
        Ok(_) => Err(OpenImageRejection::NotAFile(path)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(OpenImageRejection::NotFound(path)),
        Err(err) => Err(OpenImageRejection::Inaccessible { path, error: err.to_string() }),
    }
}

/// Maps a finished pick onto the launcher's exit intent: `Ok(None)` = cancelled (stay in the
/// launcher), `Ok(Some(OpenImage))` = close the launcher and open the image, `Err` = stay and
/// show the rejection.
pub fn launcher_outcome_for_pick(result: ImagePickResult) -> Result<Option<LauncherOutcome>, OpenImageRejection> {
    match result {
        ImagePickResult::Cancelled => Ok(None),
        ImagePickResult::Selected(path) => Ok(Some(LauncherOutcome::OpenImage(path))),
        ImagePickResult::Rejected(rejection) => Err(rejection),
    }
}

/// Starts the single-file picker plus validation on a named worker thread and returns
/// the receiver of its one result. The dialog opens at the OS default location (Windows and
/// GTK remember the last folder used): the projects root holds chapters, not loose pictures,
/// so it would be a worse start. A disconnected receiver without a value means the worker
/// could not be spawned or died; the failure is logged here and the caller shows a status.
pub fn spawn_image_picker() -> Receiver<ImagePickResult> {
    let (tx, rx) = mpsc::channel::<ImagePickResult>();
    let spawned = std::thread::Builder::new().name("launcher-open-image-picker".to_owned()).spawn(move || {
        let result = pick_result_from_dialog(file_dialog::pick_blocking(image_picker_request()));
        if tx.send(result).is_err() {
            // The launcher window closed while the dialog was open; nobody waits for the pick.
            ms_log::runtime_log::log_warn("[launcher-open-image] pick finished after the launcher stopped polling; result dropped");
        }
    });
    if let Err(err) = spawned {
        // The closure (and with it `tx`) was dropped, so the caller sees a disconnect.
        ms_log::runtime_log::log_error(format!("[launcher-open-image] failed to spawn the picker thread; error={err}"));
    }
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_one_open_file_filter_over_the_input_table() {
        let request = image_picker_request();
        assert_eq!(request.kind(), DialogKind::OpenFile);
        let [filter] = request.filters() else {
            panic!("expected exactly one filter, got {:?}", request.filters());
        };
        let expected: Vec<&str> = ms_config::single_image::input_extensions().collect();
        assert_eq!(filter.extensions(), expected.as_slice());
    }

    #[test]
    fn image_picker_request_never_widens_to_any_file() {
        // An extension unknown to the SAF MIME table widens the whole Android picker to `*/*`,
        // so every input extension must have a MIME type there.
        for extension in ms_config::single_image::input_extensions() {
            let single = FileDialogRequest::new(DialogKind::OpenFile).filter("x", &[extension]);
            assert_ne!(ms_platform::file_dialog::mime_types_for(single.filters()), vec!["*/*"], "{extension}");
        }
        assert_ne!(ms_platform::file_dialog::mime_types_for(image_picker_request().filters()), vec!["*/*"]);
    }

    #[test]
    fn dialog_results_map_onto_pick_results() {
        use ms_platform::file_dialog::{FileDialogError, Picked, PickedFile};

        assert_eq!(pick_result_from_dialog(Ok(None)), ImagePickResult::Cancelled);
        let ImagePickResult::Rejected(OpenImageRejection::Dialog { message, detail }) = pick_result_from_dialog(Err(FileDialogError::Busy)) else {
            panic!("a dialog error must become a Dialog rejection");
        };
        assert!(!message.is_empty());
        assert!(detail.contains("Busy"));

        let dir = tempfile::tempdir().expect("create temp dir");
        let file = dir.path().join("page.png");
        std::fs::write(&file, b"not decoded here").expect("write fixture file");
        let picked = Picked::Files(vec![PickedFile { path: file.clone(), display_name: "page.png".to_owned() }]);
        assert_eq!(pick_result_from_dialog(Ok(Some(picked))), ImagePickResult::Selected(file));
        let folder = Picked::Folder(dir.path().to_path_buf());
        assert!(matches!(pick_result_from_dialog(Ok(Some(folder))), ImagePickResult::Rejected(OpenImageRejection::Dialog { .. })));
    }

    #[test]
    fn outcome_mapping_covers_every_pick_result() {
        assert_eq!(launcher_outcome_for_pick(ImagePickResult::Cancelled), Ok(None));
        let path = PathBuf::from("/home/u/page.png");
        assert_eq!(launcher_outcome_for_pick(ImagePickResult::Selected(path.clone())), Ok(Some(LauncherOutcome::OpenImage(path.clone()))));
        let rejection = OpenImageRejection::NotAFile(path);
        assert_eq!(launcher_outcome_for_pick(ImagePickResult::Rejected(rejection.clone())), Err(rejection));
    }

    #[test]
    fn validation_accepts_files_and_rejects_directories_and_missing_paths() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let file = dir.path().join("page.png");
        std::fs::write(&file, b"not decoded here").expect("write fixture file");
        assert_eq!(validate_picked_path(file.clone()), Ok(file));
        assert_eq!(validate_picked_path(dir.path().to_path_buf()), Err(OpenImageRejection::NotAFile(dir.path().to_path_buf())));
        let missing = dir.path().join("missing.png");
        assert_eq!(validate_picked_path(missing.clone()), Err(OpenImageRejection::NotFound(missing)));
    }
}
