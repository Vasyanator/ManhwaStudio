//! Host tests of the file-dialog seam: request normalization, kind -> `Picked` mapping, the
//! `PendingDialog` poll contract, single-flight and the error messages. The backend is
//! injected through `request_with`, each test with its own in-flight flag, so no real dialog
//! opens and tests never contend for the process-wide flag.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::*;
use crate::test_catalog::{assert_key_in_shipped_catalogs, install_english};

/// A backend that keeps the completion so the test decides when (and whether) it finishes.
fn holding_backend(slot: &Arc<Mutex<Option<Completion>>>) -> impl FnOnce(FileDialogRequest, Completion) -> Result<Option<DropGuard>, FileDialogError> {
    let slot = Arc::clone(slot);
    move |_request, completion| {
        *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(completion);
        Ok(None)
    }
}

fn take_completion(slot: &Arc<Mutex<Option<Completion>>>) -> Completion {
    slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take().unwrap_or_else(|| panic!("backend did not receive a completion"))
}

#[test]
fn filter_normalizes_extensions() {
    let request = FileDialogRequest::new(DialogKind::OpenFiles)
        .filter("Images", &[".PNG", " jpg ", "png", "..JpEg", ""])
        .filter("Nothing", &["", "."]);
    assert_eq!(request.filters().len(), 1, "a filter without extensions is dropped");
    assert_eq!(request.filters()[0].label(), "Images");
    assert_eq!(request.filters()[0].extensions(), ["png", "jpg", "jpeg"]);
    assert_eq!(request.kind(), DialogKind::OpenFiles);
}

#[test]
fn filter_accepts_owned_strings() {
    let owned = vec!["PSD".to_owned()];
    let request = FileDialogRequest::new(DialogKind::OpenFile).filter("Photoshop".to_owned(), &owned);
    assert_eq!(request.filters()[0].extensions(), ["psd"]);
}

#[test]
fn kind_maps_to_matching_picked_variant() {
    let a = PathBuf::from("/home/u/a.png");
    let b = PathBuf::from("/home/u/b.png");
    let dir = PathBuf::from("/home/u/out");

    let single = picked_from_paths(DialogKind::OpenFile, vec![a.clone()]).and_then(Picked::into_file);
    assert_eq!(single, Some(PickedFile { path: a.clone(), display_name: "a.png".to_owned() }));

    let many = picked_from_paths(DialogKind::OpenFiles, vec![a.clone(), b.clone()]).and_then(Picked::into_files);
    assert_eq!(many.map(|files| files.into_iter().map(|file| file.path).collect::<Vec<_>>()), Some(vec![a.clone(), b]));

    assert_eq!(picked_from_paths(DialogKind::OpenFolder, vec![dir.clone()]).and_then(Picked::into_folder), Some(dir.clone()));
    assert_eq!(picked_from_paths(DialogKind::PickDirectoryPath, vec![dir.clone()]).and_then(Picked::into_directory_path), Some(dir.clone()));

    let save = picked_from_paths(DialogKind::SaveFile, vec![a.clone()]).and_then(Picked::into_save_target);
    assert_eq!(save.as_ref().map(SaveTarget::write_path), Some(a.as_path()));
    assert_eq!(save.as_ref().map(SaveTarget::display_name), Some("a.png"));

    let folder = picked_from_paths(DialogKind::SaveIntoFolder, vec![dir.clone()]).and_then(Picked::into_folder_target);
    assert_eq!(folder.as_ref().map(FolderTarget::write_dir), Some(dir.as_path()));
    assert_eq!(folder.as_ref().map(FolderTarget::display_name), Some("out"));

    // A wrong-variant accessor answers `None` instead of guessing.
    assert_eq!(picked_from_paths(DialogKind::OpenFolder, vec![dir]).and_then(Picked::into_file), None);
}

#[test]
fn empty_paths_are_a_cancel() {
    for kind in [DialogKind::OpenFile, DialogKind::OpenFiles, DialogKind::OpenFolder, DialogKind::SaveFile, DialogKind::SaveIntoFolder, DialogKind::PickDirectoryPath] {
        assert!(picked_from_paths(kind, Vec::new()).is_none(), "{kind:?}");
    }
}

#[test]
fn poll_yields_result_exactly_once_and_wakes() {
    static FLAG: AtomicBool = AtomicBool::new(false);
    let slot = Arc::new(Mutex::new(None));
    let wake_count = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&wake_count);
    let waker: Waker = Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    let mut pending = request_with(&FLAG, FileDialogRequest::new(DialogKind::OpenFolder), Some(waker), holding_backend(&slot));
    assert!(pending.poll().is_none(), "still open");
    assert!(pending.poll().is_none(), "still open");
    assert_eq!(wake_count.load(Ordering::SeqCst), 0);

    take_completion(&slot).finish(Ok(None));
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);
    assert!(matches!(pending.poll(), Some(Ok(None))));
    assert!(pending.poll().is_none(), "the result is delivered only once");
    assert!(!FLAG.load(Ordering::SeqCst), "finishing released the in-flight flag");
}

#[test]
fn dropped_completion_is_worker_lost() {
    static FLAG: AtomicBool = AtomicBool::new(false);
    let slot = Arc::new(Mutex::new(None));
    let mut pending = request_with(&FLAG, FileDialogRequest::new(DialogKind::OpenFile), None, holding_backend(&slot));
    drop(take_completion(&slot));
    assert!(matches!(pending.poll(), Some(Err(FileDialogError::WorkerLost))));
    assert!(pending.poll().is_none());
    assert!(!FLAG.load(Ordering::SeqCst), "a dropped completion releases the in-flight flag");
}

#[test]
fn second_request_while_open_is_busy() {
    static FLAG: AtomicBool = AtomicBool::new(false);
    let slot = Arc::new(Mutex::new(None));
    let mut first = request_with(&FLAG, FileDialogRequest::new(DialogKind::OpenFile), None, holding_backend(&slot));

    let launched = Arc::new(AtomicBool::new(false));
    let launched_flag = Arc::clone(&launched);
    let mut second = request_with(&FLAG, FileDialogRequest::new(DialogKind::SaveFile), None, move |_request, _completion| {
        launched_flag.store(true, Ordering::SeqCst);
        Ok(None)
    });
    assert!(matches!(second.poll(), Some(Err(FileDialogError::Busy))));
    assert!(!launched.load(Ordering::SeqCst), "a busy request never reaches the backend");

    take_completion(&slot).finish(Ok(None));
    assert!(matches!(first.poll(), Some(Ok(None))));

    let mut third = request_with(&FLAG, FileDialogRequest::new(DialogKind::OpenFile), None, holding_backend(&slot));
    assert!(third.poll().is_none(), "the flag is free again");
    drop(take_completion(&slot));
}

#[test]
fn backend_launch_failure_resolves_immediately_and_frees_the_flag() {
    static FLAG: AtomicBool = AtomicBool::new(false);
    let mut pending = request_with(&FLAG, FileDialogRequest::new(DialogKind::OpenFile), None, |_request, completion| {
        drop(completion);
        Err(FileDialogError::Backend { detail: "spawn failed".to_owned() })
    });
    assert!(matches!(pending.poll(), Some(Err(FileDialogError::Backend { .. }))));
    assert!(!FLAG.load(Ordering::SeqCst));
}

#[test]
fn ready_handle_for_dependents_tests() {
    let mut pending = PendingDialog::ready(Ok(Some(Picked::Folder(PathBuf::from("/home/u/in")))));
    assert!(matches!(pending.poll(), Some(Ok(Some(Picked::Folder(_))))));
    assert!(pending.poll().is_none());
    assert_eq!(format!("{pending:?}"), "PendingDialog { state: \"finished\", .. }");
}

#[test]
#[cfg(target_os = "linux")]
fn every_kind_is_allowed_on_linux() {
    for kind in [DialogKind::OpenFile, DialogKind::OpenFiles, DialogKind::OpenFolder, DialogKind::SaveFile, DialogKind::SaveIntoFolder, DialogKind::PickDirectoryPath] {
        assert!(required_feature_check(kind).is_ok(), "{kind:?}");
    }
}

#[test]
fn desktop_targets_commit_as_noop() {
    let dir = std::env::temp_dir().join(format!("ms-platform-target-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap_or_else(|err| panic!("create {}: {err}", dir.display()));

    let folder = FolderTarget::for_test(dir.clone());
    assert!(!folder.has_entries().unwrap_or(true), "a fresh folder is empty");
    let file = dir.join("out.txt");
    std::fs::write(&file, b"x").unwrap_or_else(|err| panic!("write {}: {err}", file.display()));
    assert!(folder.has_entries().unwrap_or(false));
    assert!(folder.commit().is_ok());

    let target = SaveTarget::for_test(file.clone());
    assert_eq!(target.write_path(), file.as_path());
    assert!(target.commit().is_ok());
    assert_eq!(std::fs::read(&file).unwrap_or_default(), b"x", "commit leaves the written file in place");

    std::fs::remove_dir_all(&dir).unwrap_or_else(|err| panic!("cleanup {}: {err}", dir.display()));
    let missing = FolderTarget::for_test(dir);
    assert!(matches!(missing.has_entries(), Err(FileDialogError::TargetIo { .. })));
}

#[test]
fn every_message_key_is_shipped_and_interpolated() {
    install_english();
    let errors = [
        FileDialogError::Busy,
        FileDialogError::Backend { detail: "d1".to_owned() },
        FileDialogError::CopyIn { name: "n2".to_owned(), detail: "d2".to_owned() },
        FileDialogError::Commit { detail: "d3".to_owned() },
        FileDialogError::TargetIo { detail: "d4".to_owned() },
        FileDialogError::WorkerLost,
    ];
    for error in &errors {
        assert_key_in_shipped_catalogs(error.message_key());
        assert!(!error.user_message().is_empty());
        assert_eq!(error.to_string(), error.user_message());
    }
    assert!(errors[1].user_message().contains("d1"));
    assert!(errors[2].user_message().contains("n2") && errors[2].user_message().contains("d2"));
    assert!(errors[4].user_message().contains("d4"));
    let unavailable = Unavailable::new(PlatformFeature::FilePicker, crate::UnavailableReason::BridgeMissing);
    assert_eq!(FileDialogError::from(unavailable).user_message(), unavailable.user_message());
}
