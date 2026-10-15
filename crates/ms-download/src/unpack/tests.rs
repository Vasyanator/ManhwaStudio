/*
File: crates/ms-download/src/unpack/tests.rs

Purpose:
Unit tests of `zip_member_atomic`: verified extraction, hash and size mismatches (the
temporary file removed, the destination untouched), missing and directory members, an
already-matching destination kept without opening the archive, replacement of a differing
destination, and the busy guard on a shared destination.

Notes:
- Archives are written with `zip::ZipWriter` into self-removing temp directories.
*/

use std::io::Write as _;

use super::*;
use crate::test_fs::{content, len_u64, read, sha256_hex, temp_root, write};

/// Writes a deflate zip at `path` with the given (name, bytes) members and one directory.
fn make_zip(path: &Path, members: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap_or_else(|err| panic!("create {}: {err}", path.display()));
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    if let Err(err) = writer.add_directory("dir/", options) {
        panic!("add_directory: {err}");
    }
    for (name, bytes) in members {
        if let Err(err) = writer.start_file(*name, options) {
            panic!("start_file {name}: {err}");
        }
        if let Err(err) = writer.write_all(bytes) {
            panic!("write {name}: {err}");
        }
    }
    if let Err(err) = writer.finish() {
        panic!("finish: {err}");
    }
}

#[test]
fn member_is_extracted_and_verified() {
    let root = temp_root("ms-download-unpack-");
    let weights = content(250_000, 1);
    let sha = sha256_hex(&weights);
    let archive = root.path().join("craft.zip");
    make_zip(&archive, &[("craft.pth", &weights), ("other.txt", b"x")]);
    let dest = root.path().join("models/craft.pth");
    let result = zip_member_atomic(&archive, "craft.pth", &dest, Expected::SizeSha256 { size: len_u64(&weights), sha256: &sha });
    assert_eq!(result.ok(), Some(len_u64(&weights)));
    assert_eq!(read(&dest), weights);
    assert!(!root.path().join("models/craft.pth.unpack").exists());
}

#[test]
fn announced_size_extracts_the_declared_member() {
    let root = temp_root("ms-download-unpack-");
    let archive = root.path().join("a.zip");
    make_zip(&archive, &[("m.bin", b"hello")]);
    let dest = root.path().join("m.bin");
    assert_eq!(zip_member_atomic(&archive, "m.bin", &dest, Expected::Announced).ok(), Some(5));
    assert_eq!(read(&dest), b"hello");
}

#[test]
fn hash_mismatch_leaves_the_destination_untouched() {
    let root = temp_root("ms-download-unpack-");
    let weights = content(10_000, 2);
    let archive = root.path().join("a.zip");
    make_zip(&archive, &[("m.bin", &weights)]);
    let dest = root.path().join("m.bin");
    write(&dest, b"previous");
    let wrong = sha256_hex(b"something else");
    let result = zip_member_atomic(&archive, "m.bin", &dest, Expected::SizeSha256 { size: len_u64(&weights), sha256: &wrong });
    assert!(matches!(result, Err(DownloadError::HashMismatch { ref expected, .. }) if *expected == wrong), "{result:?}");
    assert_eq!(read(&dest), b"previous");
    assert!(!root.path().join("m.bin.unpack").exists());
}

#[test]
fn declared_size_mismatch_fails_before_writing() {
    let root = temp_root("ms-download-unpack-");
    let archive = root.path().join("a.zip");
    make_zip(&archive, &[("m.bin", b"12345")]);
    let dest = root.path().join("m.bin");
    let result = zip_member_atomic(&archive, "m.bin", &dest, Expected::Size(4));
    assert!(matches!(result, Err(DownloadError::SizeMismatch { expected: 4, actual: 5 })), "{result:?}");
    assert!(!dest.exists());
    assert!(!root.path().join("m.bin.unpack").exists());
}

#[test]
fn missing_and_directory_members_are_archive_errors() {
    let root = temp_root("ms-download-unpack-");
    let archive = root.path().join("a.zip");
    make_zip(&archive, &[("m.bin", b"1")]);
    let dest = root.path().join("m.bin");
    let missing = zip_member_atomic(&archive, "nope.bin", &dest, Expected::Announced);
    assert!(matches!(missing, Err(DownloadError::Archive { .. })), "{missing:?}");
    let directory = zip_member_atomic(&archive, "dir/", &dest, Expected::Announced);
    assert!(matches!(directory, Err(DownloadError::Archive { .. })), "{directory:?}");
    let not_zip = root.path().join("plain.zip");
    write(&not_zip, b"not a zip archive");
    let unreadable = zip_member_atomic(&not_zip, "m.bin", &dest, Expected::Announced);
    assert!(matches!(unreadable, Err(DownloadError::Archive { .. })), "{unreadable:?}");
}

#[test]
fn matching_destination_needs_no_archive() {
    let root = temp_root("ms-download-unpack-");
    let weights = content(4_000, 3);
    let sha = sha256_hex(&weights);
    let dest = root.path().join("shared.pth");
    write(&dest, &weights);
    // A second model sharing the member: the archive may already be gone.
    let result = zip_member_atomic(&root.path().join("missing.zip"), "shared.pth", &dest, Expected::SizeSha256 { size: len_u64(&weights), sha256: &sha });
    assert_eq!(result.ok(), Some(len_u64(&weights)));
}

#[test]
fn differing_destination_is_replaced() {
    let root = temp_root("ms-download-unpack-");
    let weights = content(4_000, 4);
    let sha = sha256_hex(&weights);
    let archive = root.path().join("a.zip");
    make_zip(&archive, &[("m.bin", &weights)]);
    let dest = root.path().join("m.bin");
    write(&dest, &content(4_000, 40));
    let result = zip_member_atomic(&archive, "m.bin", &dest, Expected::SizeSha256 { size: len_u64(&weights), sha256: &sha });
    assert_eq!(result.ok(), Some(len_u64(&weights)));
    assert_eq!(read(&dest), weights);
}

#[test]
fn concurrent_extraction_of_one_destination_is_busy() {
    let root = temp_root("ms-download-unpack-");
    let archive = root.path().join("a.zip");
    make_zip(&archive, &[("m.bin", b"abc")]);
    let dest = root.path().join("m.bin");
    let key = dest_key(&dest);
    let held = BusyGuard::try_acquire(&key).unwrap_or_else(|err| panic!("{err}"));
    let result = zip_member_atomic(&archive, "m.bin", &dest, Expected::Size(3));
    assert!(matches!(result, Err(DownloadError::Busy { key: ref busy }) if *busy == key), "{result:?}");
    drop(held);
    assert_eq!(zip_member_atomic(&archive, "m.bin", &dest, Expected::Size(3)).ok(), Some(3));
}

#[test]
fn malformed_pin_and_nameless_destination_are_rejected() {
    let root = temp_root("ms-download-unpack-");
    let archive = root.path().join("a.zip");
    make_zip(&archive, &[("m.bin", b"abc")]);
    let bad_pin = zip_member_atomic(&archive, "m.bin", &root.path().join("m.bin"), Expected::SizeSha256 { size: 3, sha256: "zz" });
    assert!(matches!(bad_pin, Err(DownloadError::InvalidRequest { .. })), "{bad_pin:?}");
    let nameless = zip_member_atomic(&archive, "m.bin", Path::new(""), Expected::Size(3));
    assert!(matches!(nameless, Err(DownloadError::InvalidRequest { .. })), "{nameless:?}");
}
