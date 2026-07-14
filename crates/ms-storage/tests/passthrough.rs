/*
File: tests/passthrough.rs

Purpose:
Tests for `PassthroughStorage`, the native identity-mapped backend. Unlike the
rooted backends it addresses absolute OS paths verbatim, so these tests use a
real temp directory and confirm the operations match `std::fs` semantics.
*/

use ms_storage::{PassthroughStorage, Storage, StorageError};

struct TempDirGuard(std::path::PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir() -> TempDirGuard {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let id = (u64::from(std::process::id()) << 20) ^ N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ms-passthrough-{id}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    TempDirGuard(dir)
}

#[test]
fn absolute_path_roundtrip() {
    let g = temp_dir();
    let s = PassthroughStorage::new();
    let file = g.0.join("sub").join("f.txt");
    let dir = g.0.join("sub");
    let file_s = file.to_string_lossy();
    let dir_s = dir.to_string_lossy();

    s.create_dir_all(dir_s.as_ref()).unwrap();
    s.write(file_s.as_ref(), b"payload").unwrap();
    assert!(s.exists(file_s.as_ref()));
    assert!(s.is_dir(dir_s.as_ref()));
    assert_eq!(s.read(file_s.as_ref()).unwrap(), b"payload");
    assert_eq!(s.metadata(file_s.as_ref()).unwrap().len, 7);

    // A real file written through the backend is visible to std::fs directly,
    // proving the identity mapping.
    assert_eq!(std::fs::read(&file).unwrap(), b"payload");
}

#[test]
fn read_prefix_is_bounded() {
    let g = temp_dir();
    let s = PassthroughStorage::new();
    let file = g.0.join("f.bin");
    let file_s = file.to_string_lossy();

    s.write(file_s.as_ref(), b"abcdef").unwrap();
    // Longer than max_len, exactly max_len, shorter than max_len.
    assert_eq!(s.read_prefix(file_s.as_ref(), 3).unwrap(), b"abc");
    assert_eq!(s.read_prefix(file_s.as_ref(), 6).unwrap(), b"abcdef");
    assert_eq!(s.read_prefix(file_s.as_ref(), 100).unwrap(), b"abcdef");

    let missing = g.0.join("does-not-exist");
    let err = s.read_prefix(missing.to_string_lossy().as_ref(), 3).unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "got {err:?}");

    // A directory must be typed `IsADirectory` even with `max_len == 0`, where the bounded
    // read never touches the file contents and `File::open` alone would succeed on Linux.
    let dir = g.0.join("subdir");
    s.create_dir_all(dir.to_string_lossy().as_ref()).unwrap();
    for max_len in [0, 3] {
        let err = s.read_prefix(dir.to_string_lossy().as_ref(), max_len).unwrap_err();
        assert!(matches!(err, StorageError::IsADirectory(_)), "max_len={max_len}: got {err:?}");
    }
}

#[test]
fn missing_maps_to_not_found() {
    let g = temp_dir();
    let s = PassthroughStorage::new();
    let missing = g.0.join("does-not-exist");
    let err = s.read(missing.to_string_lossy().as_ref()).unwrap_err();
    assert!(matches!(err, StorageError::NotFound(_)), "got {err:?}");
}
