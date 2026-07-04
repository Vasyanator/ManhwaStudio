/*
File: tests/backends.rs

Purpose:
Contract tests that run the SAME scenarios against both `NativeStorage` (in a
temp dir) and `MemStorage`, proving the two backends behave identically. This is
the guarantee the migration relies on: code written against `Storage` works the
same on desktop and web.
*/

use std::sync::Arc;

use ms_storage::{MemStorage, NativeStorage, Storage, StorageError};

/// Builds the two backends under test; the native one is rooted in a unique
/// temp directory that is cleaned up when the returned guard drops.
fn backends() -> (Vec<Arc<dyn Storage>>, TempDirGuard) {
    let dir = std::env::temp_dir().join(format!("ms-storage-test-{}", unique_id()));
    std::fs::create_dir_all(&dir).expect("create temp root");
    let native: Arc<dyn Storage> = Arc::new(NativeStorage::new(dir.clone()));
    let mem: Arc<dyn Storage> = Arc::new(MemStorage::new());
    (vec![native, mem], TempDirGuard(dir))
}

/// Process-unique suffix without pulling in `rand`/time crates.
fn unique_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let pid = u64::from(std::process::id());
    (pid << 20) ^ N.fetch_add(1, Ordering::Relaxed)
}

struct TempDirGuard(std::path::PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn write_read_roundtrip() {
    let (stores, _g) = backends();
    for s in &stores {
        s.create_dir_all("a/b").unwrap();
        s.write("a/b/f.txt", b"hello").unwrap();
        assert_eq!(s.read("a/b/f.txt").unwrap(), b"hello");
        assert_eq!(s.read_to_string("a/b/f.txt").unwrap(), "hello");
        assert!(s.exists("a/b/f.txt"));
        assert!(s.is_dir("a/b"));
        assert!(!s.is_dir("a/b/f.txt"));
        assert_eq!(s.metadata("a/b/f.txt").unwrap().len, 5);
        assert!(s.metadata("a/b").unwrap().is_dir);
    }
}

#[test]
fn write_without_parent_fails() {
    let (stores, _g) = backends();
    for s in &stores {
        let err = s.write("missing/f.txt", b"x").unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)), "got {err:?}");
    }
}

#[test]
fn read_missing_is_not_found() {
    let (stores, _g) = backends();
    for s in &stores {
        assert!(matches!(s.read("nope").unwrap_err(), StorageError::NotFound(_)));
        assert!(!s.exists("nope"));
    }
}

#[test]
fn read_dir_lists_children_sorted() {
    let (stores, _g) = backends();
    for s in &stores {
        s.create_dir_all("d").unwrap();
        s.write("d/b.txt", b"1").unwrap();
        s.write("d/a.txt", b"2").unwrap();
        s.create_dir_all("d/sub").unwrap();
        let mut names: Vec<_> = s.read_dir("d").unwrap().into_iter().map(|e| e.name).collect();
        names.sort();
        assert_eq!(names, vec!["a.txt", "b.txt", "sub"]);
    }
}

#[test]
fn remove_file_and_dir() {
    let (stores, _g) = backends();
    for s in &stores {
        s.create_dir_all("d/sub").unwrap();
        s.write("d/f.txt", b"x").unwrap();
        s.remove_file("d/f.txt").unwrap();
        assert!(!s.exists("d/f.txt"));
        s.remove_dir_all("d").unwrap();
        assert!(!s.exists("d"));
    }
}

#[test]
fn rename_moves_entry() {
    let (stores, _g) = backends();
    for s in &stores {
        s.create_dir_all("src").unwrap();
        s.create_dir_all("dst").unwrap();
        s.write("src/f.txt", b"data").unwrap();
        s.rename("src/f.txt", "dst/g.txt").unwrap();
        assert!(!s.exists("src/f.txt"));
        assert_eq!(s.read("dst/g.txt").unwrap(), b"data");
    }
}

#[test]
fn escape_is_rejected_on_both() {
    let (stores, _g) = backends();
    for s in &stores {
        assert!(matches!(s.read("../secret"), Err(StorageError::Escape(_))));
        assert!(matches!(s.write("../x", b"y"), Err(StorageError::Escape(_))));
    }
}

#[test]
fn overwrite_truncates() {
    let (stores, _g) = backends();
    for s in &stores {
        s.write("f", b"longvalue").unwrap();
        s.write("f", b"hi").unwrap();
        assert_eq!(s.read("f").unwrap(), b"hi");
    }
}

#[test]
fn mem_clone_shares_state() {
    // Specific to MemStorage: cloned handles must observe each other's writes,
    // matching how the app shares one Arc across worker threads.
    let a = MemStorage::new();
    let b = a.clone();
    a.write("x", b"v").unwrap();
    assert_eq!(b.read("x").unwrap(), b"v");
}
