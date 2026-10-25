/*
File: crates/ms-atomic-file/src/tests.rs

Purpose:
Contract tests of the atomic write recipe: step order (journal), failure cleanup (fault
injector), durability modes, the identical-bytes skip, every `AtomicWriteOptions` field, the
Windows sharing-violation retry bound, the temp-name matcher (including the Python mirror's
shape) and the `io::Error` conversion. Every test works inside its own `tempfile` directory.
*/

use std::fs;
use std::path::{Path, PathBuf};

use crate::naming::unique_temp_path_for;
use crate::recipe::{is_transient_sharing_error, retry_transient};
use crate::{AtomicWriteError, AtomicWriteOptions, Durability, FaultPoint, WriteStep, arm_fault, is_temp_artifact, recorded_steps, rename_with_retry, sync_directory, temp_path_for, write_atomic, write_atomic_with, write_temp_file};

/// Names of every recipe temp artifact left in `dir`.
fn temp_leftovers(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir).expect("list dir").map(|entry| entry.expect("dir entry").path()).filter(|path| is_temp_artifact(path)).collect()
}

/// A durable write leaves the directory entry fsynced BEFORE it returns, so a caller
/// may delete the source it just migrated.
#[test]
fn a_durable_write_fsyncs_the_directory_after_the_rename() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("doc.json");
    write_atomic(&path, b"{}\n", Durability::ContentsAndDirectory).expect("write");
    assert_eq!(fs::read_to_string(&path).expect("read back"), "{}\n");
    assert_eq!(
        recorded_steps(&path),
        vec![WriteStep::Renamed, WriteStep::DirectoryDurable],
        "the directory must be made durable, and only after the rename"
    );
}

/// The cheap mode is unchanged: contents only, no directory fsync.
#[test]
fn a_contents_only_write_does_not_touch_the_directory() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("doc.json");
    write_atomic(&path, b"{}\n", Durability::Contents).expect("write");
    assert_eq!(recorded_steps(&path), vec![WriteStep::Renamed]);
}

/// A failed temp write closes the handle BEFORE removing the orphan (on Windows
/// `remove_file` on an open handle fails) and leaves the previous file untouched.
#[test]
fn a_failed_temp_write_closes_the_handle_before_removing_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("doc.json");
    fs::write(&path, "previous\n").expect("seed previous file");
    let temp = temp_path_for(&path);

    arm_fault(Some(FaultPoint::TempWrite));
    let err = write_atomic(&path, b"new\n", Durability::ContentsAndDirectory).expect_err("the injected failure must be reported");
    arm_fault(None);

    assert!(matches!(err, AtomicWriteError::TempWrite { .. }), "{err:?}");
    assert_eq!(recorded_steps(&temp), vec![WriteStep::TempClosed, WriteStep::TempRemoved], "the handle must be closed before the orphan is removed");
    assert!(!temp.exists(), "no orphaned temp file may survive");
    assert_eq!(fs::read_to_string(&path).expect("read back"), "previous\n", "a failed write must leave the previous file intact");
}

/// The same holds when the fsync of the temp file fails.
#[test]
fn a_failed_temp_fsync_is_reported_and_cleaned_up() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("doc.json");
    let temp = temp_path_for(&path);

    arm_fault(Some(FaultPoint::TempSync));
    let err = write_atomic(&path, b"new\n", Durability::Contents).expect_err("the injected failure must be reported");
    arm_fault(None);

    assert!(matches!(err, AtomicWriteError::TempWrite { .. }), "{err:?}");
    assert_eq!(recorded_steps(&temp), vec![WriteStep::TempClosed, WriteStep::TempRemoved]);
    assert!(!path.exists(), "nothing may be created by a failed write");
}

/// `Durability::None` still replaces atomically and never touches the directory.
#[test]
fn an_unsynced_write_is_still_an_atomic_replace() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("doc.json");
    fs::write(&path, "previous\n").expect("seed");
    // An armed fsync fault must not fire: this mode never fsyncs.
    arm_fault(Some(FaultPoint::TempSync));
    let result = write_atomic(&path, b"new\n", Durability::None);
    arm_fault(None);
    result.expect("an unsynced write never reaches the fsync step");
    assert_eq!(fs::read_to_string(&path).expect("read back"), "new\n");
    assert_eq!(recorded_steps(&path), vec![WriteStep::Renamed]);
    assert!(!temp_path_for(&path).exists());
}

/// Identical bytes: nothing is rewritten (same inode, no temp, no rename step), even in
/// the durable modes, whose durability is applied to the existing file instead.
#[test]
fn identical_bytes_keep_the_existing_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    for (index, durability) in [Durability::None, Durability::Contents, Durability::ContentsAndDirectory].into_iter().enumerate() {
        let path = dir.path().join(format!("doc{index}.json"));
        write_atomic(&path, b"{\"a\": 1}\n", durability).expect("first write");
        let before = fs::metadata(&path).expect("stat");
        let steps_before = recorded_steps(&path);
        // An armed fault proves no temp file is even created.
        arm_fault(Some(FaultPoint::TempWrite));
        let result = write_atomic(&path, b"{\"a\": 1}\n", durability);
        arm_fault(None);
        result.expect("an identical write is a no-op");
        let after = fs::metadata(&path).expect("stat");
        assert_eq!(after.modified().expect("mtime"), before.modified().expect("mtime"), "{durability:?}: file untouched");
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::MetadataExt::ino(&after), std::os::unix::fs::MetadataExt::ino(&before), "{durability:?}: no rename");
        let mut expected = steps_before;
        if durability == Durability::ContentsAndDirectory {
            expected.push(WriteStep::DirectoryDurable);
        }
        assert_eq!(recorded_steps(&path), expected, "{durability:?}: no Renamed step, directory durability still applied");
        assert!(!temp_path_for(&path).exists());
        // Same length, different bytes: the full recipe runs.
        write_atomic(&path, b"{\"a\": 2}\n", durability).expect("changed write");
        assert_eq!(fs::read_to_string(&path).expect("read"), "{\"a\": 2}\n");
    }
}

/// `skip_if_identical: false` rewrites identical bytes through the full recipe.
#[test]
fn disabling_the_identical_skip_always_rewrites() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("marker.json");
    let options = AtomicWriteOptions { skip_if_identical: false, ..AtomicWriteOptions::new(Durability::Contents) };
    write_atomic_with(&path, b"same", &options).expect("first write");
    write_atomic_with(&path, b"same", &options).expect("second write");
    assert_eq!(recorded_steps(&path), vec![WriteStep::Renamed, WriteStep::Renamed], "both writes must rename");
    assert_eq!(fs::read(&path).expect("read back"), b"same");
}

/// Transient failures are retried up to the bound; the last error is returned.
#[test]
fn transient_rename_failures_are_retried_within_the_bound() {
    let transient = |err: &std::io::Error| err.kind() == std::io::ErrorKind::WouldBlock;
    let zero = std::time::Duration::ZERO;

    let mut calls = 0;
    let result = retry_transient(5, zero, || {
        calls += 1;
        if calls < 3 { Err(std::io::Error::from(std::io::ErrorKind::WouldBlock)) } else { Ok(()) }
    }, transient);
    assert!(result.is_ok());
    assert_eq!(calls, 3, "success on the third attempt ends the loop");

    let mut calls = 0;
    let result = retry_transient(5, zero, || {
        calls += 1;
        Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
    }, transient);
    assert_eq!(result.expect_err("persistent failure").kind(), std::io::ErrorKind::WouldBlock);
    assert_eq!(calls, 5, "never more than the bound");

    let mut calls = 0;
    let result = retry_transient(5, zero, || {
        calls += 1;
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    }, transient);
    assert!(result.is_err());
    assert_eq!(calls, 1, "a non-transient error is final");
}

/// The production classifier: on Windows exactly the sharing-violation codes 5 / 32 / 33 are
/// transient; elsewhere nothing is.
#[test]
fn only_windows_sharing_violations_are_transient() {
    for code in [5, 32, 33] {
        assert_eq!(is_transient_sharing_error(&std::io::Error::from_raw_os_error(code)), cfg!(windows), "os error {code}");
    }
    for code in [2, 3, 17, 112] {
        assert!(!is_transient_sharing_error(&std::io::Error::from_raw_os_error(code)), "os error {code} is final");
    }
    assert!(!is_transient_sharing_error(&std::io::Error::other("no os code")));
}

/// `rename_with_retry` replaces an existing target and reports a final failure unchanged.
#[test]
fn rename_with_retry_replaces_and_reports() {
    let dir = tempfile::tempdir().expect("temp dir");
    let staged = dir.path().join("staged.bin");
    let target = dir.path().join("target.bin");
    fs::write(&staged, b"new").expect("seed staged");
    fs::write(&target, b"old").expect("seed target");
    rename_with_retry(&staged, &target).expect("rename");
    assert_eq!(fs::read(&target).expect("read back"), b"new");
    assert!(!staged.exists());
    let err = rename_with_retry(&staged, &target).expect_err("a missing source must fail");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

/// The temp-name predicate recognizes exactly the recipe's temp names, including the
/// Python mirror's `.{name}.{pid}.tmp` and the unique `.{name}.{pid}.{serial}.tmp`.
#[test]
fn temp_artifacts_are_recognized_by_the_recipe_pattern() {
    let target = Path::new("/x/chapter/translation_bubbles.json");
    assert!(is_temp_artifact(&temp_path_for(target)));
    assert!(is_temp_artifact(&unique_temp_path_for(target)));
    assert!(is_temp_artifact(Path::new("layers/.layers.json.4242.tmp")), "the Python docstore.py shape");
    assert!(is_temp_artifact(Path::new("layers/.layers.json.4242.17.tmp")), "the unique shape");
    assert!(!is_temp_artifact(Path::new("layers/layers.json")));
    assert!(!is_temp_artifact(Path::new(".layers.json.tmp")), "pid is required");
    assert!(!is_temp_artifact(Path::new(".layers.json.12a.tmp")), "pid is digits");
    assert!(!is_temp_artifact(Path::new("..1.tmp")), "file name is required");
    assert!(!is_temp_artifact(Path::new("page.tmp")));
    assert!(is_temp_artifact(Path::new("layers/.layers.db.77.tmp")));
    assert!(is_temp_artifact(Path::new("layers/.layers.db.77.tmp-journal")), "the journal of a .db temp");
    assert!(!is_temp_artifact(Path::new("layers/layers.db-journal")), "a document's own journal is needed for recovery");
}

/// The unique temp names never repeat and stay beside the target.
#[test]
fn unique_temp_names_differ_per_call() {
    let target = Path::new("/x/settings/flux2.json");
    let first = unique_temp_path_for(target);
    let second = unique_temp_path_for(target);
    assert_ne!(first, second);
    assert_eq!(first.parent(), target.parent());
    assert_ne!(first, temp_path_for(target));
}

/// A unique-temp write replaces the file, leaves no temp and still cleans up on failure.
#[test]
fn unique_temp_writes_replace_and_clean_up() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("settings.json");
    let options = AtomicWriteOptions { unique_temp: true, ..AtomicWriteOptions::new(Durability::Contents) };
    write_atomic_with(&path, b"one", &options).expect("first write");
    write_atomic_with(&path, b"two", &options).expect("second write");
    assert_eq!(fs::read(&path).expect("read back"), b"two");
    assert_eq!(temp_leftovers(dir.path()), [] as [PathBuf; 0]);

    arm_fault(Some(FaultPoint::TempWrite));
    let result = write_atomic_with(&path, b"three", &options);
    arm_fault(None);
    assert!(matches!(result, Err(AtomicWriteError::TempWrite { .. })), "{result:?}");
    assert_eq!(fs::read(&path).expect("read back"), b"two", "the previous file stays");
    assert_eq!(temp_leftovers(dir.path()), [] as [PathBuf; 0], "the failed unique temp is removed");
}

/// Concurrent in-process writers of one target never share a temp: every write succeeds and
/// the file ends up holding one complete payload.
#[test]
fn unique_temp_writes_from_several_threads_do_not_collide() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("shared.json");
    let options = AtomicWriteOptions { unique_temp: true, skip_if_identical: false, ..AtomicWriteOptions::new(Durability::None) };
    let payloads: Vec<Vec<u8>> = (0..8u8).map(|index| vec![index; 4096]).collect();
    std::thread::scope(|scope| {
        for payload in &payloads {
            let path = &path;
            scope.spawn(move || {
                for _ in 0..20 {
                    write_atomic_with(path, payload, &options).expect("concurrent write");
                }
            });
        }
    });
    let final_bytes = fs::read(&path).expect("read back");
    assert!(payloads.contains(&final_bytes), "the file holds one whole payload");
    assert_eq!(temp_leftovers(dir.path()), [] as [PathBuf; 0]);
}

/// `create_parent` creates missing ancestors; without it a missing parent is an error that
/// creates nothing and leaves no temp.
#[test]
fn create_parent_creates_missing_directories() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("a").join("b").join("file.json");
    let options = AtomicWriteOptions { create_parent: true, ..AtomicWriteOptions::new(Durability::ContentsAndDirectory) };
    write_atomic_with(&path, b"x", &options).expect("write with parent creation");
    assert_eq!(fs::read(&path).expect("read back"), b"x");
}

/// A parent that cannot be created (a FILE sits on its path) fails as `TempWrite` naming the
/// directory, and creates nothing.
#[test]
fn create_parent_failure_names_the_directory() {
    let dir = tempfile::tempdir().expect("temp dir");
    let blocker = dir.path().join("blocker");
    fs::write(&blocker, b"file, not a directory").expect("seed blocker");
    let path = blocker.join("sub").join("file.json");
    let options = AtomicWriteOptions { create_parent: true, ..AtomicWriteOptions::new(Durability::Contents) };
    let err = write_atomic_with(&path, b"x", &options).expect_err("a file in the way must fail");
    match &err {
        AtomicWriteError::TempWrite { path: failed, .. } => assert_eq!(failed, &blocker.join("sub")),
        other => panic!("unexpected error {other:?}"),
    }
    assert_eq!(fs::read(&blocker).expect("blocker intact"), b"file, not a directory");
}

/// `unix_mode` sets the new file's mode — also when a leftover temp with a wider mode exists
/// — and an identical file with the wrong mode is rewritten rather than kept.
#[cfg(unix)]
#[test]
fn unix_mode_is_applied_and_forced_on_a_leftover_temp() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("bridge.json");
    let leftover = temp_path_for(&path);
    fs::write(&leftover, b"stale").expect("seed leftover temp");
    fs::set_permissions(&leftover, fs::Permissions::from_mode(0o644)).expect("chmod leftover");

    let options = AtomicWriteOptions { unix_mode: Some(0o600), ..AtomicWriteOptions::new(Durability::Contents) };
    write_atomic_with(&path, b"secret", &options).expect("write");
    let mode = |path: &Path| fs::metadata(path).expect("stat").permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert!(!leftover.exists(), "the leftover temp was reused and renamed");

    // Same bytes, wrong mode: the identical skip must not keep it.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("widen mode");
    write_atomic_with(&path, b"secret", &options).expect("rewrite");
    assert_eq!(mode(&path), 0o600, "the mode is restored");
}

/// The public byte API replaces an existing file's bytes and leaves no temp behind.
#[test]
fn write_atomic_replaces_an_existing_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("page.png");
    fs::write(&path, b"old image bytes").expect("seed");
    write_atomic(&path, b"new", Durability::Contents).expect("replace");
    assert_eq!(fs::read(&path).expect("read back"), b"new");
    assert_eq!(temp_leftovers(dir.path()), [] as [PathBuf; 0]);
}

/// The public byte API creates a file that did not exist (its parent does).
#[test]
fn write_atomic_creates_a_new_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("fresh.webp");
    write_atomic(&path, b"\x00\x01binary", Durability::ContentsAndDirectory).expect("create");
    assert_eq!(fs::read(&path).expect("read back"), b"\x00\x01binary");
    assert_eq!(temp_leftovers(dir.path()), [] as [PathBuf; 0]);
}

/// A missing parent directory is an error; nothing is created, no temp is left.
#[test]
fn write_atomic_requires_an_existing_parent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir.path().join("missing");
    let path = missing.join("image.png");
    let err = write_atomic(&path, b"bytes", Durability::Contents).expect_err("a missing parent must fail");
    assert!(matches!(err, AtomicWriteError::TempWrite { kind: std::io::ErrorKind::NotFound, .. }), "{err:?}");
    assert!(!missing.exists(), "the parent must not be created");
    assert_eq!(temp_leftovers(dir.path()), [] as [PathBuf; 0]);
}

/// A failed rename (the target is a directory) leaves the old entry intact and removes
/// the temp file.
#[test]
fn write_atomic_failure_keeps_the_old_entry() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("image.png");
    fs::create_dir(&path).expect("target directory");
    let marker = path.join("keep.txt");
    fs::write(&marker, b"old").expect("seed marker");
    let err = write_atomic(&path, b"new", Durability::Contents).expect_err("renaming over a directory must fail");
    assert!(matches!(err, AtomicWriteError::Rename { .. }), "{err:?}");
    assert!(path.is_dir(), "the old entry must survive");
    assert_eq!(fs::read(&marker).expect("read marker"), b"old");
    assert!(temp_leftovers(dir.path()).is_empty(), "the temp must be removed");
}

/// A directory the process may not write into: the error carries `PermissionDenied`, so a
/// caller can tell it from other failures without parsing the OS text. Skipped (with a
/// note) when the process can write anyway, e.g. running as root.
#[cfg(unix)]
#[test]
fn write_atomic_reports_permission_denied_kind() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("temp dir");
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).expect("create dir");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).expect("chmod read-only");
    let probe = locked.join("probe");
    if fs::write(&probe, b"x").is_ok() {
        eprintln!("skipped: this process can write into a read-only directory (running as root?)");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod back");
        return;
    }
    let result = write_atomic(&locked.join("image.png"), b"bytes", Durability::Contents);
    // Restore write access first so the temp dir can be removed whatever the assertions do.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod back");
    let err = result.expect_err("a read-only directory must refuse the write");
    assert!(matches!(err, AtomicWriteError::TempWrite { kind: std::io::ErrorKind::PermissionDenied, .. }), "{err:?}");
    assert_eq!(std::io::Error::from(err).kind(), std::io::ErrorKind::PermissionDenied, "the io conversion keeps the kind");
}

/// `write_temp_file` writes and closes the temp, and leaves cleanup of a failed one to the
/// caller (a staged temp may have companions).
#[test]
fn write_temp_file_writes_and_leaves_cleanup_to_the_caller() {
    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("doc.db");
    let temp = temp_path_for(&target);
    write_temp_file(&temp, b"staged", true).expect("write temp");
    assert_eq!(fs::read(&temp).expect("read temp"), b"staged");
    assert_eq!(recorded_steps(&temp), vec![WriteStep::TempClosed]);

    arm_fault(Some(FaultPoint::TempWrite));
    let result = write_temp_file(&temp, b"again", true);
    arm_fault(None);
    match result {
        Err(AtomicWriteError::TempWrite { path, .. }) => assert_eq!(path, temp),
        other => panic!("unexpected result {other:?}"),
    }
    assert!(temp.exists(), "write_temp_file never removes the temp itself");
}

/// `sync_directory` succeeds on an existing directory and reports `DirSync` for a missing one
/// (Unix; a documented no-op elsewhere).
#[test]
fn sync_directory_reports_dir_sync() {
    let dir = tempfile::tempdir().expect("temp dir");
    sync_directory(dir.path()).expect("an existing directory syncs");
    let missing = dir.path().join("missing");
    let result = sync_directory(&missing);
    if cfg!(unix) {
        match result {
            Err(err @ AtomicWriteError::DirSync { .. }) => assert_eq!(std::io::Error::from(err).kind(), std::io::ErrorKind::Other),
            other => panic!("unexpected result {other:?}"),
        }
    } else {
        assert!(result.is_ok(), "non-Unix directory sync is a no-op");
    }
}
