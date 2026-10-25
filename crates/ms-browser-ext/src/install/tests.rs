/*
File: crates/ms-browser-ext/src/install/tests.rs

Purpose:
Contract tests of the unpack: fresh install, upgrade (stale files removed, foreign files and
`bridge.json` kept, identical files untouched), stamp written last (a failed install leaves
the old stamp or none), path validation before any disk access, corrupt stamps, and the
inter-process install lock (an installer waits for the lock's holder).
All scratch data lives in self-removing temp directories.
*/

use std::fs;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::{INSTALL_LOCK_FILE_NAME, InstallError, install, install_dir, installed_build_id};
use crate::bundle::Bundle;
use crate::names::STAMP_FILE_NAME;

const OLD: Bundle = Bundle {
    build_id: "00000000000000000000000000000001",
    files: &[("background.js", b"old bg"), ("icons/icon-16.png", b"png16"), ("legacy.js", b"gone soon"), ("manifest.json", b"{}")],
};

const NEW: Bundle = Bundle {
    build_id: "00000000000000000000000000000002",
    files: &[("background.js", b"new bg"), ("icons/icon-16.png", b"png16"), ("manifest.json", b"{}"), ("modules/translate/driver.js", b"driver")],
};

fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("ms-browser-ext-test-").tempdir().unwrap_or_else(|err| panic!("{err:?}"))
}

fn read(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|err| panic!("{}: {err:?}", path.display()))
}

#[test]
fn install_dir_is_under_the_data_dir() {
    assert_eq!(install_dir(Path::new("/home/u/ms")), Path::new("/home/u/ms").join("browser_extension").join("chromium"));
}

#[test]
fn nothing_installed_reads_as_none() {
    let temp = scratch();
    assert_eq!(installed_build_id(&temp.path().join("missing")).unwrap_or_else(|err| panic!("{err:?}")), None);
    assert_eq!(installed_build_id(temp.path()).unwrap_or_else(|err| panic!("{err:?}")), None);
}

#[test]
fn fresh_install_writes_every_file_and_the_stamp() {
    let temp = scratch();
    let dir = temp.path().join("ext");
    let report = install(&OLD, &dir).unwrap_or_else(|err| panic!("{err:?}"));
    assert_eq!((report.written, report.unchanged, report.removed), (4, 0, 0));
    assert_eq!(report.previous_build_id, None);
    assert!(!report.previous_stamp_corrupt);
    assert_eq!(read(&dir.join("icons").join("icon-16.png")), b"png16");
    assert_eq!(installed_build_id(&dir).unwrap_or_else(|err| panic!("{err:?}")).as_deref(), Some(OLD.build_id));
    let leftovers: Vec<_> = fs::read_dir(&dir).unwrap_or_else(|err| panic!("{err:?}")).filter_map(Result::ok).filter(|e| ms_atomic_file::is_temp_artifact(&e.path())).collect();
    assert!(leftovers.is_empty(), "no temporary file survives");
}

#[test]
fn upgrade_replaces_changed_removes_stale_and_keeps_foreign_files() {
    let temp = scratch();
    let dir = temp.path().join("ext");
    install(&OLD, &dir).unwrap_or_else(|err| panic!("{err:?}"));
    fs::write(dir.join("bridge.json"), b"{\"secret\":\"x\"}").unwrap_or_else(|err| panic!("{err:?}"));
    fs::write(dir.join("user-notes.txt"), b"mine").unwrap_or_else(|err| panic!("{err:?}"));

    let report = install(&NEW, &dir).unwrap_or_else(|err| panic!("{err:?}"));
    assert_eq!(report.previous_build_id.as_deref(), Some(OLD.build_id));
    assert_eq!((report.written, report.unchanged, report.removed), (2, 2, 1));
    assert_eq!(read(&dir.join("background.js")), b"new bg");
    assert_eq!(read(&dir.join("modules").join("translate").join("driver.js")), b"driver");
    assert!(!dir.join("legacy.js").exists(), "a file only the old bundle had is removed");
    assert_eq!(read(&dir.join("bridge.json")), b"{\"secret\":\"x\"}", "the bridge's pairing file survives");
    assert_eq!(read(&dir.join("user-notes.txt")), b"mine", "foreign files survive");
    assert_eq!(installed_build_id(&dir).unwrap_or_else(|err| panic!("{err:?}")).as_deref(), Some(NEW.build_id));

    let again = install(&NEW, &dir).unwrap_or_else(|err| panic!("{err:?}"));
    assert_eq!((again.written, again.unchanged, again.removed), (0, 4, 0), "a repeated install writes nothing");
}

#[test]
fn a_failed_install_leaves_the_previous_stamp() {
    let temp = scratch();
    let dir = temp.path().join("ext");
    install(&OLD, &dir).unwrap_or_else(|err| panic!("{err:?}"));
    // A directory where the new bundle wants a file makes its rename fail mid-install.
    fs::create_dir_all(dir.join("modules").join("translate").join("driver.js").join("blocker")).unwrap_or_else(|err| panic!("{err:?}"));
    let result = install(&NEW, &dir);
    assert!(matches!(result, Err(InstallError::Io { .. })), "{result:?}");
    assert_eq!(installed_build_id(&dir).unwrap_or_else(|err| panic!("{err:?}")).as_deref(), Some(OLD.build_id), "the stamp still names the old bundle");
}

#[test]
fn a_failed_fresh_install_has_no_stamp() {
    let temp = scratch();
    let dir = temp.path().join("ext");
    fs::create_dir_all(dir.join("manifest.json").join("blocker")).unwrap_or_else(|err| panic!("{err:?}"));
    assert!(install(&NEW, &dir).is_err());
    assert!(!dir.join(STAMP_FILE_NAME).exists(), "an incomplete folder never carries a stamp");
    assert_eq!(installed_build_id(&dir).unwrap_or_else(|err| panic!("{err:?}")), None);
}

/// A bundle's `(relative path, content)` list.
type FileList = &'static [(&'static str, &'static [u8])];

/// (expected offending path, bundle file list) per invalid bundle; `const` so the lists are `'static`.
const INVALID_BUNDLES: &[(&str, FileList)] = &[
    ("../escape.js", &[("../escape.js", b"x")]),
    ("/abs.js", &[("/abs.js", b"x")]),
    ("a\\b.js", &[("a\\b.js", b"x")]),
    ("c:x.js", &[("c:x.js", b"x")]),
    ("a//b.js", &[("a//b.js", b"x")]),
    (STAMP_FILE_NAME, &[(STAMP_FILE_NAME, b"x")]),
    (INSTALL_LOCK_FILE_NAME, &[(INSTALL_LOCK_FILE_NAME, b"x")]),
    ("dup.js", &[("dup.js", b"x"), ("dup.js", b"y")]),
];

#[test]
fn invalid_bundle_paths_are_refused_before_touching_disk() {
    let temp = scratch();
    let dir = temp.path().join("ext");
    for &(path, files) in INVALID_BUNDLES {
        let bundle = Bundle { build_id: "bad", files };
        let result = install(&bundle, &dir);
        assert!(matches!(&result, Err(InstallError::InvalidBundlePath { path: p, .. }) if p == path), "{path}: {result:?}");
    }
    assert!(!dir.exists(), "validation happens before the folder is created");
}

#[test]
fn a_corrupt_stamp_is_reported_and_replaced() {
    let temp = scratch();
    let dir = temp.path().join("ext");
    fs::create_dir_all(&dir).unwrap_or_else(|err| panic!("{err:?}"));
    fs::write(dir.join(STAMP_FILE_NAME), b"{not json").unwrap_or_else(|err| panic!("{err:?}"));
    assert!(matches!(installed_build_id(&dir), Err(InstallError::StampCorrupt { .. })));
    let report = install(&OLD, &dir).unwrap_or_else(|err| panic!("{err:?}"));
    assert!(report.previous_stamp_corrupt);
    assert_eq!(installed_build_id(&dir).unwrap_or_else(|err| panic!("{err:?}")).as_deref(), Some(OLD.build_id));
}

#[test]
fn a_hostile_stamp_cannot_delete_outside_the_folder() {
    let temp = scratch();
    let dir = temp.path().join("ext");
    fs::create_dir_all(&dir).unwrap_or_else(|err| panic!("{err:?}"));
    let outside = temp.path().join("precious.txt");
    fs::write(&outside, b"keep").unwrap_or_else(|err| panic!("{err:?}"));
    fs::write(dir.join(STAMP_FILE_NAME), br#"{"build_id":"x","files":["../precious.txt","/etc/hostname"]}"#).unwrap_or_else(|err| panic!("{err:?}"));
    let report = install(&OLD, &dir).unwrap_or_else(|err| panic!("{err:?}"));
    assert_eq!(report.removed, 0);
    assert_eq!(read(&outside), b"keep");
}

#[test]
fn an_install_waits_for_the_holder_of_the_folder_lock() {
    let temp = scratch();
    let dir = temp.path().join("ext");
    fs::create_dir_all(&dir).unwrap_or_else(|err| panic!("{err:?}"));
    // An independent open + lock of the lock file: exactly what another app process holds while
    // it installs.
    let holder = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join(INSTALL_LOCK_FILE_NAME)).unwrap_or_else(|err| panic!("{err:?}"));
    holder.lock().unwrap_or_else(|err| panic!("{err:?}"));
    let (done_tx, done_rx) = mpsc::channel();
    let installer = {
        let dir = dir.clone();
        thread::spawn(move || {
            let report = install(&NEW, &dir);
            done_tx.send(()).unwrap_or_else(|err| panic!("{err:?}"));
            report
        })
    };
    assert!(done_rx.recv_timeout(Duration::from_millis(300)).is_err(), "the install did not wait for the lock");
    assert!(!dir.join(STAMP_FILE_NAME).exists(), "nothing was stamped while the other installer held the folder");
    holder.unlock().unwrap_or_else(|err| panic!("{err:?}"));
    let report = installer.join().unwrap_or_else(|err| panic!("{err:?}")).unwrap_or_else(|err| panic!("{err:?}"));
    assert_eq!(report.build_id, NEW.build_id);
    assert_eq!(installed_build_id(&dir).unwrap_or_else(|err| panic!("{err:?}")).as_deref(), Some(NEW.build_id));
}

#[test]
fn concurrent_installs_of_two_builds_leave_one_complete_build() {
    let temp = scratch();
    let dir = temp.path().join("ext");
    for _round in 0..5 {
        let workers: Vec<_> = [OLD, NEW]
            .into_iter()
            .map(|bundle| {
                let dir = dir.clone();
                thread::spawn(move || install(&bundle, &dir).map(|report| report.build_id))
            })
            .collect();
        for worker in workers {
            worker.join().unwrap_or_else(|err| panic!("{err:?}")).unwrap_or_else(|err| panic!("{err:?}"));
        }
        // Whichever ran last, the stamp names a build whose every file is on disk byte for byte.
        let stamped = installed_build_id(&dir).unwrap_or_else(|err| panic!("{err:?}")).unwrap_or_else(|| panic!("no stamp"));
        let bundle = if stamped == OLD.build_id { OLD } else { NEW };
        for (path, bytes) in bundle.files {
            assert_eq!(read(&super::target_path(&dir, path)), *bytes, "{path} of build {stamped}");
        }
    }
}
