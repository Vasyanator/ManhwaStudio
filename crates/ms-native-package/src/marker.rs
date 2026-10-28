/*
File: crates/ms-native-package/src/marker.rs

Purpose:
The completion marker of a package directory: its text (recipe header + one line per
published member), the trust check, its durable removal and its atomic publication.

Key functions:
- recipe_header()   : format line + `<key> <value>` fields + `source <url> <sha256>` lines
- marker_text()     : the header followed by `member <file name> <size> <sha256>` lines
- marker_matches()  : the "installed" rule (marker == recipe, every member at its size)
- remove_marker()   : remove + fsync the directory, BEFORE any member is replaced
- write_marker()    : `ms_atomic_file::write_atomic` with `ContentsAndDirectory`

Notes:
The byte format is a persistence contract shared with caches written by earlier program
versions (e.g. `ms-onnx-runtime cache v1`): never change it; a consumer that needs another
meaning bumps ITS format line.
*/

use std::fs;
use std::path::Path;

use crate::error::{PackageError, PackageIoOp};
use crate::{PackageLayout, PackageRecipe};

/// One member recorded in the completion marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublishedMember {
    /// File name inside the package directory.
    pub(crate) file_name: String,
    /// Exact byte size at publication.
    pub(crate) size: u64,
    /// Lowercase sha256 at publication (diagnostic: the fast path checks sizes only, so a
    /// probe never hashes hundreds of MB).
    pub(crate) sha256: String,
}

/// The recipe header of the marker: `marker_format`, then `<key> <value>` per recipe field in
/// order, then `source <url> <sha256>` per source in fetch order. A marker is trusted only
/// when it starts with exactly this text, so any recipe change (re-pin, version bump, new
/// source) makes the package stale.
pub(crate) fn recipe_header(layout: &PackageLayout<'_>, recipe: &PackageRecipe<'_>) -> String {
    let fields = recipe.fields.iter().map(|(key, value)| format!("{key} {value}\n"));
    let sources = recipe.sources.iter().map(|source| format!("source {} {}\n", source.url, source.sha256));
    std::iter::once(format!("{}\n", layout.marker_format)).chain(fields).chain(sources).collect()
}

/// The full marker: `header` followed by one `member <name> <size> <sha256>` line per
/// published member, in publication order.
pub(crate) fn marker_text(header: &str, members: &[PublishedMember]) -> String {
    let lines = members.iter().map(|member| format!("member {} {} {}\n", member.file_name, member.size, member.sha256));
    std::iter::once(header.to_owned()).chain(lines).collect()
}

/// Whether the package in `layout.dir` is complete for `header`: the marker exists, starts
/// with exactly `header`, lists exactly `expected_names` (as a multiset, any order) and each
/// listed file is a regular file of the recorded size. Every doubt (missing, unreadable or
/// malformed marker) answers `false`, which rebuilds the package. Stat-only, no hashing.
pub(crate) fn marker_matches(layout: &PackageLayout<'_>, header: &str, expected_names: &[&str], log_tag: &str) -> bool {
    let marker_path = layout.dir.join(layout.marker_file);
    let text = match fs::read_to_string(&marker_path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
        Err(err) => {
            ms_log::runtime_log::log_warn(format!("{log_tag} cannot read completion marker '{}' ({err}); rebuilding the package", marker_path.display()));
            return false;
        }
    };
    let Some(members) = text.strip_prefix(header) else { return false };
    let mut recorded: Vec<(&str, u64)> = Vec::new();
    for line in members.lines() {
        // `member <name> <size> <sha256>`; names never contain spaces (plain file names).
        let mut fields = line.split(' ');
        let (Some("member"), Some(name), Some(size), Some(_sha256), None) = (fields.next(), fields.next(), fields.next(), fields.next(), fields.next()) else {
            return false;
        };
        let Ok(size) = size.parse::<u64>() else { return false };
        recorded.push((name, size));
    }
    let mut expected: Vec<&str> = expected_names.to_vec();
    expected.sort_unstable();
    let mut recorded_names: Vec<&str> = recorded.iter().map(|(name, _)| *name).collect();
    recorded_names.sort_unstable();
    if recorded_names != expected {
        return false;
    }
    recorded.iter().all(|(name, size)| fs::metadata(layout.dir.join(name)).is_ok_and(|meta| meta.is_file() && meta.len() == *size))
}

/// Removes the completion marker of `layout.dir` (if any) and fsyncs the directory, so the
/// removal is durable before any member is replaced or removed.
///
/// # Errors
/// [`PackageError::Io`] (`RemoveMarker`) when the marker exists but cannot be removed;
/// [`PackageError::SyncDir`] on the fsync.
pub(crate) fn remove_marker(layout: &PackageLayout<'_>) -> Result<(), PackageError> {
    let marker_path = layout.dir.join(layout.marker_file);
    match fs::remove_file(&marker_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(PackageError::Io { op: PackageIoOp::RemoveMarker, path: marker_path, source }),
    }
    sync_dir(layout.dir)
}

/// Publishes the completion marker `text` atomically (`ms_atomic_file::write_atomic` with
/// `Durability::ContentsAndDirectory`: hidden sibling temp, fsync, rename with the Windows
/// sharing-violation retry, fsync of the directory; the temp is removed on failure).
///
/// # Errors
/// [`PackageError::WriteMarker`] on any write, rename or directory-fsync failure (the package
/// then stays untrusted and the next call rebuilds it).
pub(crate) fn write_marker(layout: &PackageLayout<'_>, text: &str) -> Result<(), PackageError> {
    let marker_path = layout.dir.join(layout.marker_file);
    ms_atomic_file::write_atomic(&marker_path, text.as_bytes(), ms_atomic_file::Durability::ContentsAndDirectory).map_err(|source| PackageError::WriteMarker {
        path: marker_path,
        dir: layout.dir.to_path_buf(),
        source: Box::new(source),
    })
}

/// Fsyncs directory `dir` so the renames / removals inside it are durable
/// (`ms_atomic_file::sync_directory`: unix; a documented no-op elsewhere).
///
/// # Errors
/// [`PackageError::SyncDir`] naming `dir`.
pub(crate) fn sync_dir(dir: &Path) -> Result<(), PackageError> {
    ms_atomic_file::sync_directory(dir).map_err(|source| PackageError::SyncDir { dir: dir.to_path_buf(), source: Box::new(source) })
}
