/*
File: crates/ms-atomic-file/src/naming.rs

Purpose:
The one owner of the recipe's temp-file names: the per-process name (`temp_path_for`), the
per-call unique name (`unique_temp_path_for`) and the matcher every directory scan or copy uses
to skip crash leftovers (`is_temp_artifact`).

Notes:
The Python mirror (`docstore.py`, repo root) writes `.{name}.{pid}.tmp` too; the matcher must
keep accepting exactly that shape.
*/

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide serial that makes every [`unique_temp_path_for`] name distinct, so two
/// in-process writers of one target never share a temp file.
static UNIQUE_TEMP_SERIAL: AtomicU64 = AtomicU64::new(0);

/// The target's file name for a temp name; `document.json` when `path` has none.
fn file_name_of(path: &Path) -> String {
    path.file_name().map_or_else(|| "document.json".to_owned(), |name| name.to_string_lossy().into_owned())
}

/// The target's directory (`.` when `path` has no parent component).
fn parent_of(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new("."))
}

/// The sibling temp path the recipe writes before renaming: `.{file_name}.{pid}.tmp` in
/// the target's directory. Per-process so two concurrent processes never collide on it; the
/// `.` prefix hides it from casual directory listings. Within one process the caller must
/// serialize writers of the same target (the docstore's document lock does), or use
/// `AtomicWriteOptions::unique_temp`.
#[must_use]
pub fn temp_path_for(path: &Path) -> PathBuf {
    parent_of(path).join(format!(".{}.{}.tmp", file_name_of(path), std::process::id()))
}

/// A temp path no other call of this process returns: `.{file_name}.{pid}.{serial}.tmp`.
/// The recipe opens it with `create_new`, so even a leftover of the same name is never shared.
pub(crate) fn unique_temp_path_for(path: &Path) -> PathBuf {
    let serial = UNIQUE_TEMP_SERIAL.fetch_add(1, Ordering::Relaxed);
    parent_of(path).join(format!(".{}.{}.{serial}.tmp", file_name_of(path), std::process::id()))
}

/// Whether `path` names a temp file of the write recipe (`.{file_name}.{pid}.tmp` or the
/// unique `.{file_name}.{pid}.{serial}.tmp`, any pid) — a leftover of a crash between the temp
/// write and the rename — or the `SQLite` rollback journal of such a temp
/// (`.{file_name}.{pid}.tmp-journal`, left when a `.db` temp was being written). Directory
/// copies and scans use it to skip such files; it is the one owner of the pattern produced by
/// [`temp_path_for`]. A document's own `<name>.db-journal` is NOT a temp artifact (it may be
/// needed for crash recovery).
#[must_use]
pub fn is_temp_artifact(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else { return false };
    let name = name.strip_suffix("-journal").unwrap_or(name);
    let Some(inner) = name.strip_prefix('.').and_then(|rest| rest.strip_suffix(".tmp")) else { return false };
    // `inner` is `{file_name}.{pid}` (or `{file_name}.{pid}.{serial}`, whose `{file_name}.{pid}`
    // head then plays the file name): a non-empty file name and an all-digit last component.
    inner.rsplit_once('.').is_some_and(|(file_name, pid)| !file_name.is_empty() && !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()))
}
