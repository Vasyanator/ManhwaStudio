/*
File: lock.rs

Purpose:
Process-wide registry of per-document write locks. Every write and every read-modify-write
of a document runs under the lock of its STEM (the path without `.json`/`.db`), so two
threads of this process can never interleave their RMW cycles on one document. Reads that
are not part of an RMW stay unlocked: the atomic rename guarantees they see an untorn file.

Key functions:
- with_document_lock(): run a closure under the lock of one stem (blocking, non-reentrant).

Notes:
- The lock is NOT reentrant: acquiring it again on the same thread while holding it
  deadlocks. Multi-step callers use `with_lock` and the `LockedDoc` methods, which never
  re-lock.
- Poison is recovered (`PoisonError::into_inner`): the guarded unit is `()`, so a panic in
  one holder cannot leave shared state half-updated here; the document on disk is protected
  by the atomic write, not by the mutex.
- Keys are normalized on native so two spellings of one path share a lock: the nearest
  EXISTING ancestor is canonicalized and the missing tail appended verbatim. This stays
  stable when the missing directories are created later (the canonical form of a new
  plain directory is its canonical parent plus its name). On wasm the stem is used as is.
- Unused entries are pruned when the registry grows, so a long session over many chapters
  does not accumulate one mutex per document ever touched.
*/

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// Registry size above which entries nobody holds are pruned on the next acquisition.
const PRUNE_THRESHOLD: usize = 64;

/// Shared per-stem mutex map. The outer mutex is held only for the lookup, never while a
/// document lock is held or awaited.
fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<()>>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Runs `f` while holding the process-local lock of the document whose extension-less path
/// is `stem`, and returns its result. Blocks until the lock is free. Non-reentrant: `f`
/// must not lock the same document again (see the file header). A panic in `f` releases
/// the lock (poison is recovered by the next holder).
pub(crate) fn with_document_lock<R>(stem: &Path, f: impl FnOnce() -> R) -> R {
    let key = normalize_key(stem);
    let mutex = {
        let mut map = registry().lock().unwrap_or_else(PoisonError::into_inner);
        if map.len() > PRUNE_THRESHOLD {
            // Safe: holders clone the Arc while the registry lock is held, so a count of 1
            // means nobody holds or awaits this entry; a later caller simply recreates it.
            map.retain(|_, entry| Arc::strong_count(entry) > 1);
        }
        Arc::clone(map.entry(key).or_insert_with(|| Arc::new(Mutex::new(()))))
    };
    // The registry lock is released above, BEFORE waiting on the document lock, so a slow
    // holder of one document never blocks access to another. `_guard` is held only for its
    // drop effect: it releases the document lock when `f` returns or unwinds.
    let _guard = mutex.lock().unwrap_or_else(PoisonError::into_inner);
    f()
}

/// Registry key for `stem`: see the file header ("Keys are normalized").
#[cfg(not(target_arch = "wasm32"))]
fn normalize_key(stem: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = stem;
    loop {
        if let Ok(canonical) = std::fs::canonicalize(cursor) {
            return tail.iter().rev().fold(canonical, |acc, part| acc.join(part));
        }
        match (cursor.parent(), cursor.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                cursor = parent;
            }
            // No existing ancestor (relative path in a missing tree, or `..` tails):
            // fall back to the raw stem, which is still a consistent key for one spelling.
            _ => return stem.to_path_buf(),
        }
    }
}

/// wasm32: the in-memory store has no aliases worth normalizing.
#[cfg(target_arch = "wasm32")]
fn normalize_key(stem: &Path) -> PathBuf {
    stem.to_path_buf()
}
