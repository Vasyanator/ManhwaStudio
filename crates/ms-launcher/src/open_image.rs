/*
File: crates/ms-launcher/src/open_image.rs

Purpose:
"Open image" flow of the main menu (image-set mode entry): the native multi-file picker on a
worker thread, the natural sort of the pick, the launcher-side validation of the picked paths,
and the mapping of a pick onto the launcher outcome. Native only — the wasm build has no picker
and no button (D10 of `dev-docs/single_image_mode_plan.md`).

Key items:
- `spawn_image_picker()`: runs the blocking `ms_platform::file_dialog::pick_blocking` and the
  validation on a worker; the GUI thread only polls the returned channel.
- `image_picker_request()`: the `OpenFiles` request, its filter built from the ONE input-type
  table `ms_config::single_image::input_extensions()` (the dialog seam adds the uppercase
  spellings case-sensitive GTK globs need).
- `pick_result_from_dialog()`: maps a dialog result onto `ImagePickResult` (cancel, dialog
  failure, sorted and validated pick).
- `selection_from_files()`: the page order (`natural_sort::natural_name_cmp` of the names the
  user saw, full path as tie-break) and the "at least one usable file" rule.
- `validate_picked_path()`: the launcher's only per-file check — the path exists and is a file.
- `launcher_outcome_for_pick()`: pure mapping of an `ImagePickResult` onto
  `LauncherOutcome::OpenImages`.

Notes:
Decoding is deliberately NOT attempted here: the studio open owns every decode error
(`SingleImageError::user_message`), so the launcher never duplicates that decision. For the
same reason a pick with at least one usable file goes to the studio WHOLE: the studio lists the
unusable ones with the undecodable ones in its skipped-files notice (one place, and the launcher
is gone by then). Only a pick with no usable file stays here with a status. One picked file
behaves exactly as the single-file picker did.
*/

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};

use ms_platform::file_dialog::{self, DialogKind, DialogResult, FileDialogRequest, PickedFile};

use crate::state::LauncherOutcome;

/// Result of one "Open image" pick, produced on the picker worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImagePickResult {
    /// The user closed the dialog without choosing a file.
    Cancelled,
    /// The chosen paths in page order (never empty); at least one exists and is a file
    /// (decodability is not checked, nor are the others: the studio lists what it skips).
    Selected(Vec<PathBuf>),
    /// The dialog failed, or no chosen path passed launcher validation.
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
    /// Several files were picked and none passed validation; one rejection per file, in page
    /// order (each is a `NotFound`, `NotAFile` or `Inaccessible`).
    NoneUsable(Vec<OpenImageRejection>),
}

impl OpenImageRejection {
    /// Localized one-line status shown under the main menu.
    pub fn user_message(&self) -> String {
        match self {
            OpenImageRejection::NotFound(path) => tf!("launcher.main.open_image_not_found_error", path = path.display()),
            OpenImageRejection::NotAFile(path) => tf!("launcher.main.open_image_not_a_file_error", path = path.display()),
            OpenImageRejection::Inaccessible { path, .. } => tf!("launcher.main.open_image_access_error", path = path.display()),
            OpenImageRejection::Dialog { message, .. } => message.clone(),
            OpenImageRejection::NoneUsable(rejections) => {
                let names = rejections.iter().filter_map(OpenImageRejection::path).map(display_name_of).collect::<Vec<_>>().join(", ");
                tf!("launcher.main.open_images_rejected_error", files = names)
            }
        }
    }

    /// The picked path this rejection is about; `None` for a dialog failure or a whole pick.
    fn path(&self) -> Option<&Path> {
        match self {
            OpenImageRejection::NotFound(path) | OpenImageRejection::NotAFile(path) | OpenImageRejection::Inaccessible { path, .. } => Some(path),
            OpenImageRejection::Dialog { .. } | OpenImageRejection::NoneUsable(_) => None,
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
            OpenImageRejection::NoneUsable(rejections) => format!(
                "[launcher-open-image] none of the {} picked files can be opened:\n{}",
                rejections.len(),
                rejections.iter().map(OpenImageRejection::log_message).collect::<Vec<_>>().join("\n")
            ),
        }
    }
}

/// The last path component, or the whole path when it has none.
fn display_name_of(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// The multi-file request of the "Open image" picker: one filter listing every input extension
/// in table order. Uppercase spellings are not listed here: the desktop dialog backend adds them
/// for the case-sensitive GTK / xdg-portal globs.
pub fn image_picker_request() -> FileDialogRequest {
    let extensions: Vec<&'static str> = ms_config::single_image::input_extensions().collect();
    FileDialogRequest::new(DialogKind::OpenFiles).filter(t!("launcher.main.open_image_filter"), &extensions)
}

/// Maps a finished dialog onto the pick result: a cancel stays `Cancelled`, a dialog failure
/// becomes `Rejected(Dialog)`, picked files go through [`selection_from_files`] (blocking
/// metadata reads: call only off the GUI thread).
pub fn pick_result_from_dialog(result: DialogResult) -> ImagePickResult {
    match result {
        Ok(None) => ImagePickResult::Cancelled,
        Ok(Some(picked)) => match picked.into_files() {
            Some(files) if !files.is_empty() => selection_from_files(files),
            // `OpenFiles` always yields `Picked::Files` with at least one file (the seam's
            // contract); anything else is a backend bug, reported like a picker failure.
            Some(_) | None => ImagePickResult::Rejected(OpenImageRejection::Dialog {
                message: t!("launcher.main.open_image_picker_error").to_owned(),
                detail: "OpenFiles dialog returned a result without a file".to_owned(),
            }),
        },
        Err(error) => ImagePickResult::Rejected(OpenImageRejection::Dialog { message: error.user_message(), detail: format!("{error:?}") }),
    }
}

/// Orders picked `files` into pages and applies the launcher's rule (blocking metadata reads):
/// - order: `natural_sort::natural_name_cmp` of the name the user saw in the picker
///   (`PickedFile::display_name`; on Android the path is a copy), the full path as tie-break;
/// - one file: `Selected` when it validates, else `Rejected` with its own rejection (the
///   single-file behaviour);
/// - several: `Selected` with EVERY path, in order, when at least one validates (the unusable
///   ones are logged here and listed by the studio as skipped); `Rejected(NoneUsable)` otherwise;
/// - no file: a `Dialog` rejection (the dialog seam never returns one).
pub fn selection_from_files(mut files: Vec<PickedFile>) -> ImagePickResult {
    files.sort_by(|left, right| crate::natural_sort::natural_name_cmp(&left.display_name, &right.display_name).then_with(|| left.path.cmp(&right.path)));
    let paths: Vec<PathBuf> = files.into_iter().map(|file| file.path).collect();
    let rejections: Vec<OpenImageRejection> = paths.iter().filter_map(|path| validate_picked_path(path.clone()).err()).collect();
    if paths.is_empty() {
        return ImagePickResult::Rejected(OpenImageRejection::Dialog {
            message: t!("launcher.main.open_image_picker_error").to_owned(),
            detail: "the picker returned an empty file list".to_owned(),
        });
    }
    if rejections.len() < paths.len() {
        for rejection in &rejections {
            ms_log::runtime_log::log_warn(format!("{}; the studio lists it as skipped", rejection.log_message()));
        }
        return ImagePickResult::Selected(paths);
    }
    // Every path was rejected: one file keeps its own message, several are listed together.
    match <[OpenImageRejection; 1]>::try_from(rejections) {
        Ok([only]) => ImagePickResult::Rejected(only),
        Err(several) => ImagePickResult::Rejected(OpenImageRejection::NoneUsable(several)),
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
/// launcher), `Ok(Some(OpenImages))` = close the launcher and open the set, `Err` = stay and
/// show the rejection. An empty `Selected` (never built by [`selection_from_files`]) is a
/// picker failure: `OpenImages` must not be empty.
pub fn launcher_outcome_for_pick(result: ImagePickResult) -> Result<Option<LauncherOutcome>, OpenImageRejection> {
    match result {
        ImagePickResult::Cancelled => Ok(None),
        ImagePickResult::Selected(paths) if paths.is_empty() => Err(OpenImageRejection::Dialog {
            message: t!("launcher.main.open_image_picker_error").to_owned(),
            detail: "an empty selection reached the outcome mapping".to_owned(),
        }),
        ImagePickResult::Selected(paths) => Ok(Some(LauncherOutcome::OpenImages(paths))),
        ImagePickResult::Rejected(rejection) => Err(rejection),
    }
}

/// Starts the multi-file picker plus sort and validation on a named worker thread and returns
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
    fn request_is_one_open_files_filter_over_the_input_table() {
        let request = image_picker_request();
        assert_eq!(request.kind(), DialogKind::OpenFiles);
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

    /// A picked file as the dialog seam reports it (display name = the path's file name).
    fn picked(path: &Path) -> PickedFile {
        PickedFile { path: path.to_path_buf(), display_name: display_name_of(path) }
    }

    #[test]
    fn dialog_results_map_onto_pick_results() {
        use ms_platform::file_dialog::{FileDialogError, Picked};

        assert_eq!(pick_result_from_dialog(Ok(None)), ImagePickResult::Cancelled);
        let ImagePickResult::Rejected(OpenImageRejection::Dialog { message, detail }) = pick_result_from_dialog(Err(FileDialogError::Busy)) else {
            panic!("a dialog error must become a Dialog rejection");
        };
        assert!(!message.is_empty());
        assert!(detail.contains("Busy"));

        let dir = tempfile::tempdir().expect("create temp dir");
        let file = dir.path().join("page.png");
        std::fs::write(&file, b"not decoded here").expect("write fixture file");
        let one = Picked::Files(vec![picked(&file)]);
        assert_eq!(pick_result_from_dialog(Ok(Some(one))), ImagePickResult::Selected(vec![file]));
        let folder = Picked::Folder(dir.path().to_path_buf());
        assert!(matches!(pick_result_from_dialog(Ok(Some(folder))), ImagePickResult::Rejected(OpenImageRejection::Dialog { .. })));
        assert!(matches!(pick_result_from_dialog(Ok(Some(Picked::Files(Vec::new())))), ImagePickResult::Rejected(OpenImageRejection::Dialog { .. })));
    }

    #[test]
    fn a_multi_pick_is_natural_sorted_with_the_path_as_tie_break() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let sub = dir.path().join("b");
        std::fs::create_dir(&sub).expect("create sub dir");
        let files = [dir.path().join("p10.png"), sub.join("p2.png"), dir.path().join("p2.png"), dir.path().join("P3.png")];
        for file in &files {
            std::fs::write(file, b"not decoded here").expect("write fixture file");
        }
        let [p10, sub_p2, p2, upper_p3] = files;
        let result = selection_from_files(vec![picked(&p10), picked(&sub_p2), picked(&p2), picked(&upper_p3)]);
        // Case-insensitive: `p2 < P3 < p10`; equal names fall back to the full path.
        let mut expected_p2 = [p2, sub_p2];
        expected_p2.sort();
        let [first_p2, second_p2] = expected_p2;
        assert_eq!(result, ImagePickResult::Selected(vec![first_p2, second_p2, upper_p3, p10]));
    }

    #[test]
    fn a_multi_pick_keeps_unusable_files_while_one_is_usable() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let good = dir.path().join("a1.png");
        std::fs::write(&good, b"not decoded here").expect("write fixture file");
        let missing = dir.path().join("a2.png");
        let folder = dir.path().join("a3.png");
        std::fs::create_dir(&folder).expect("create folder named like an image");
        let result = selection_from_files(vec![picked(&folder), picked(&missing), picked(&good)]);
        assert_eq!(result, ImagePickResult::Selected(vec![good, missing, folder]), "every path goes on, in page order");
    }

    #[test]
    fn a_pick_without_a_usable_file_stays_in_the_launcher() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let missing = dir.path().join("gone.png");
        // One file: its own rejection, exactly as the single-file picker reported it.
        assert_eq!(selection_from_files(vec![picked(&missing)]), ImagePickResult::Rejected(OpenImageRejection::NotFound(missing.clone())));
        let folder = dir.path().join("folder.png");
        std::fs::create_dir(&folder).expect("create folder named like an image");
        let ImagePickResult::Rejected(rejection) = selection_from_files(vec![picked(&missing), picked(&folder)]) else {
            panic!("no usable file must stay in the launcher");
        };
        assert_eq!(rejection, OpenImageRejection::NoneUsable(vec![OpenImageRejection::NotAFile(folder), OpenImageRejection::NotFound(missing)]));
        assert!(!rejection.user_message().is_empty());
        assert!(rejection.log_message().contains("gone.png"));
    }

    #[test]
    fn outcome_mapping_covers_every_pick_result() {
        assert_eq!(launcher_outcome_for_pick(ImagePickResult::Cancelled), Ok(None));
        let paths = vec![PathBuf::from("/home/u/p2.png"), PathBuf::from("/home/u/p10.png")];
        assert_eq!(launcher_outcome_for_pick(ImagePickResult::Selected(paths.clone())), Ok(Some(LauncherOutcome::OpenImages(paths.clone()))));
        assert!(matches!(launcher_outcome_for_pick(ImagePickResult::Selected(Vec::new())), Err(OpenImageRejection::Dialog { .. })), "OpenImages is never empty");
        let rejection = OpenImageRejection::NotAFile(paths[0].clone());
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
