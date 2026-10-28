/*
File: crates/ms-download/src/unpack/tests.rs

Purpose:
Unit tests of `zip_member_atomic`: verified extraction, hash and size mismatches (the
temporary file removed, the destination untouched), missing and directory members, an
already-matching destination kept without opening the archive, replacement of a differing
destination, and the busy guard on a shared destination. Unit tests of `stage_members`:
zip / tar.gz / tar.zst staging with sizes and digests in `wanted` order, the all-or-nothing
rule (a missing member removes every temporary), `./` tar-key normalization with symlink and
directory entries skipped, and the open / read error variants.

Notes:
- Archives are written with `zip::ZipWriter` / `tar::Builder` into self-removing temp
  directories.
*/

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

/// The `(archive path, destination)` pairs as `WantedMember`s.
fn wanted<'a>(pairs: &'a [(&'a str, PathBuf)]) -> Vec<WantedMember<'a>> {
    pairs.iter().map(|(archive_path, dest)| WantedMember { archive_path, dest }).collect()
}

/// No `.tmp` staging file anywhere in `dir` (a missing `dir` has none).
fn assert_no_staging(dir: &Path) {
    let Ok(items) = std::fs::read_dir(dir) else { return };
    for item in items {
        let name = item.unwrap_or_else(|err| panic!("dir entry: {err}")).file_name();
        assert!(!name.to_string_lossy().ends_with(".tmp"), "leftover temporary {name:?}");
    }
}

/// A tar stream holding `./pkg/lib.so` (regular file), `./pkg/link.so` (symlink to it), a
/// `pkg/dir/` directory entry and `pkg/other.txt`.
fn tar_bytes(lib: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Directory);
    header.set_size(0);
    header.set_mode(0o755);
    builder.append_data(&mut header, "pkg/dir/", std::io::empty()).unwrap_or_else(|err| panic!("dir entry: {err}"));
    let mut header = tar::Header::new_gnu();
    header.set_size(len_u64(lib));
    header.set_mode(0o644);
    builder.append_data(&mut header, "./pkg/lib.so", lib).unwrap_or_else(|err| panic!("file entry: {err}"));
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    builder.append_link(&mut header, "./pkg/link.so", "lib.so").unwrap_or_else(|err| panic!("symlink entry: {err}"));
    let mut header = tar::Header::new_gnu();
    header.set_size(5);
    header.set_mode(0o644);
    builder.append_data(&mut header, "pkg/other.txt", b"other".as_slice()).unwrap_or_else(|err| panic!("other entry: {err}"));
    builder.into_inner().unwrap_or_else(|err| panic!("finish tar: {err}"))
}

#[test]
fn zip_members_are_staged_in_wanted_order() {
    let root = temp_root("ms-download-stage-");
    let archive = root.path().join("pkg.zip");
    let lib = content(70_000, 5);
    make_zip(&archive, &[("capi/side.dll", b"side"), ("capi/lib.dll", &lib), ("capi/big.pdb", b"skip")]);
    let out = root.path().join("out");
    let pairs = [("capi/lib.dll", out.join("lib.dll")), ("capi/side.dll", out.join("side.dll"))];
    let staged = stage_members(&archive, ArchiveKind::Zip, &wanted(&pairs)).unwrap_or_else(|err| panic!("stage: {err}"));
    assert_eq!(
        staged,
        vec![
            StagedMember { archive_path: "capi/lib.dll".to_owned(), size: len_u64(&lib), sha256: sha256_hex(&lib) },
            StagedMember { archive_path: "capi/side.dll".to_owned(), size: 4, sha256: sha256_hex(b"side") },
        ]
    );
    // Staged, not published: only the `.tmp` siblings exist, unlisted members are untouched.
    assert_eq!(read(&out.join("lib.dll.tmp")), lib);
    assert_eq!(read(&out.join("side.dll.tmp")), b"side");
    assert!(!out.join("lib.dll").exists() && !out.join("side.dll").exists());
    assert!(!out.join("big.pdb.tmp").exists());
}

#[test]
fn extraction_stages_every_member_before_publishing_any() {
    let root = temp_root("ms-download-stage-");
    let archive = root.path().join("fixture.zip");
    make_zip(&archive, &[("capi/onnxruntime.dll", b"primary"), ("capi/dxil.dll", b"dxil")]);
    let out = root.path().join("out");

    // One wanted member is absent: nothing may remain staged, no destination is touched.
    let incomplete = [("capi/dxil.dll", out.join("dxil.dll")), ("capi/absent.dll", out.join("absent.dll"))];
    let err = stage_members(&archive, ArchiveKind::Zip, &wanted(&incomplete)).err();
    assert!(matches!(&err, Some(UnpackError::MemberMissing { member }) if member == "capi/absent.dll"), "{err:?}");
    assert!(!out.join("dxil.dll").exists(), "a member was published before the set was complete");
    assert_no_staging(&out);

    let complete = [("capi/dxil.dll", out.join("dxil.dll")), ("capi/onnxruntime.dll", out.join("onnxruntime.dll"))];
    let staged = stage_members(&archive, ArchiveKind::Zip, &wanted(&complete)).unwrap_or_else(|err| panic!("stage: {err}"));
    assert_eq!(staged.len(), 2);
    assert_eq!(read(&staging_path(&out.join("onnxruntime.dll"))), b"primary");
    assert_eq!(read(&staging_path(&out.join("dxil.dll"))), b"dxil");
}

#[test]
fn tar_gz_keys_are_normalized_and_links_skipped() {
    let root = temp_root("ms-download-stage-");
    let lib = content(3_000, 6);
    let archive = root.path().join("pkg.tgz");
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&tar_bytes(&lib)).unwrap_or_else(|err| panic!("gzip: {err}"));
    write(&archive, &encoder.finish().unwrap_or_else(|err| panic!("gzip finish: {err}")));
    let out = root.path().join("out");
    // `pkg/lib.so` matches the `./`-prefixed entry; `pkg/dir/` is only a directory entry.
    let pairs = [("pkg/lib.so", out.join("lib.so"))];
    let staged = stage_members(&archive, ArchiveKind::TarGz, &wanted(&pairs)).unwrap_or_else(|err| panic!("stage: {err}"));
    assert_eq!(staged[0].size, len_u64(&lib));
    assert_eq!(read(&out.join("lib.so.tmp")), lib);

    // A wanted path that only exists as a symlink (or a directory) is never staged.
    let links = [("pkg/link.so", out.join("link.so"))];
    let err = stage_members(&archive, ArchiveKind::TarGz, &wanted(&links)).err();
    assert!(matches!(&err, Some(UnpackError::MemberMissing { member }) if member == "pkg/link.so"), "{err:?}");
    let dirs = [("pkg/dir", out.join("dir"))];
    let err = stage_members(&archive, ArchiveKind::TarGz, &wanted(&dirs)).err();
    assert!(matches!(&err, Some(UnpackError::MemberMissing { .. })), "{err:?}");
    assert!(!out.join("link.so.tmp").exists() && !out.join("dir.tmp").exists());
}

#[test]
fn tar_zst_members_are_staged() {
    let root = temp_root("ms-download-stage-");
    let lib = content(2_000, 7);
    let archive = root.path().join("pkg.tar.zst");
    write(&archive, &zstd::stream::encode_all(tar_bytes(&lib).as_slice(), 1).unwrap_or_else(|err| panic!("zstd: {err}")));
    let out = root.path().join("out");
    let pairs = [("pkg/other.txt", out.join("other.txt")), ("pkg/lib.so", out.join("lib.so"))];
    let staged = stage_members(&archive, ArchiveKind::TarZst, &wanted(&pairs)).unwrap_or_else(|err| panic!("stage: {err}"));
    assert_eq!(staged.iter().map(|member| member.archive_path.as_str()).collect::<Vec<_>>(), vec!["pkg/other.txt", "pkg/lib.so"]);
    assert_eq!(staged[0].sha256, sha256_hex(b"other"));
    assert_eq!(read(&out.join("lib.so.tmp")), lib);
}

#[test]
fn unreadable_archives_are_typed_errors() {
    let root = temp_root("ms-download-stage-");
    let out = root.path().join("out");
    let pairs = [("m.bin", out.join("m.bin"))];
    let missing = stage_members(&root.path().join("absent.zip"), ArchiveKind::Zip, &wanted(&pairs)).err();
    assert!(matches!(missing, Some(UnpackError::OpenArchive { .. })), "{missing:?}");
    let junk = root.path().join("junk.bin");
    write(&junk, b"not an archive at all");
    let not_zip = stage_members(&junk, ArchiveKind::Zip, &wanted(&pairs)).err();
    assert!(matches!(not_zip, Some(UnpackError::ZipRead { .. })), "{not_zip:?}");
    let not_zstd = stage_members(&junk, ArchiveKind::TarZst, &wanted(&pairs)).err();
    assert!(matches!(not_zstd, Some(UnpackError::TarRead { .. } | UnpackError::TarEntry { .. } | UnpackError::ZstdOpen { .. })), "{not_zstd:?}");
    assert_no_staging(&out);
}

#[test]
fn staging_path_is_a_tmp_sibling() {
    assert_eq!(staging_path(Path::new("/cache/lib.so.1")), PathBuf::from("/cache/lib.so.1.tmp"));
}
