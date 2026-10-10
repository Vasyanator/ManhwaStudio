/*
File: file_dialog/desktop.rs

Purpose:
Desktop file-dialog backend: runs the blocking rfd dialog on a named `file-dialog` thread
and reports through the request's `Completion`.

Key functions:
- launch(): spawns the worker (the `request()` dispatch target).
- run(): builds and shows the rfd dialog for one request.

Notes:
`OpenFolder`, `SaveIntoFolder` and `PickDirectoryPath` all map to `pick_folder`; the targets'
`commit()` is a no-op here because the chosen path IS the destination. On macOS rfd forwards a
dialog opened off the main thread to the main thread itself.
*/

use std::path::PathBuf;

use super::{Completion, DialogKind, DropGuard, FileDialogError, FileDialogRequest, Picked, picked_from_paths};

/// Starts `request` on a detached `file-dialog` thread. A panic inside rfd drops the completion,
/// which the requester sees as `WorkerLost`.
///
/// # Errors
/// [`FileDialogError::Backend`] when the thread cannot be spawned.
pub(super) fn launch(request: FileDialogRequest, completion: Completion) -> Result<Option<DropGuard>, FileDialogError> {
    std::thread::Builder::new()
        .name("file-dialog".to_owned())
        .spawn(move || {
            // rfd reports no errors, only a cancel (`None`).
            completion.finish(Ok(run(&request)));
        })
        // Detached on purpose: the result travels through the completion, never `join`.
        .map(|_detached| None)
        .map_err(|err| FileDialogError::Backend { detail: err.to_string() })
}

/// Shows the dialog and blocks until it closes; `None` = cancelled.
fn run(request: &FileDialogRequest) -> Option<Picked> {
    let mut dialog = rfd::FileDialog::new();
    if let Some(title) = &request.title {
        dialog = dialog.set_title(title.as_str());
    }
    for filter in &request.filters {
        dialog = dialog.add_filter(filter.label.as_str(), &case_variants(&filter.extensions));
    }
    if let Some(dir) = &request.start_dir {
        dialog = dialog.set_directory(dir);
    }
    if request.kind == DialogKind::SaveFile
        && let Some(name) = &request.suggested_name
    {
        dialog = dialog.set_file_name(name.as_str());
    }
    let paths: Vec<PathBuf> = match request.kind {
        DialogKind::OpenFile => dialog.pick_file().into_iter().collect(),
        DialogKind::OpenFiles => dialog.pick_files().unwrap_or_default(),
        DialogKind::OpenFolder | DialogKind::SaveIntoFolder | DialogKind::PickDirectoryPath => dialog.pick_folder().into_iter().collect(),
        DialogKind::SaveFile => dialog.save_file().into_iter().collect(),
    };
    picked_from_paths(request.kind, paths)
}

/// Each normalized (lowercase) extension followed by its uppercase spelling, so the
/// case-sensitive GTK / xdg-portal globs do not hide `PHOTO.JPG`; Windows and macOS match
/// case-insensitively, where the copies are harmless.
fn case_variants(extensions: &[String]) -> Vec<String> {
    let mut variants = Vec::with_capacity(extensions.len() * 2);
    for extension in extensions {
        variants.push(extension.clone());
        let upper = extension.to_uppercase();
        if upper != *extension {
            variants.push(upper);
        }
    }
    variants
}

#[cfg(test)]
mod tests {
    use super::case_variants;

    #[test]
    fn case_variants_add_uppercase_once() {
        let extensions = vec!["png".to_owned(), "7z".to_owned()];
        assert_eq!(case_variants(&extensions), vec!["png", "PNG", "7z", "7Z"]);
        let digits_only = vec!["001".to_owned()];
        assert_eq!(case_variants(&digits_only), vec!["001"]);
    }
}
