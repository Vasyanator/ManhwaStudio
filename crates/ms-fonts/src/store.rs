/*
File: crates/ms-fonts/src/store.rs

Purpose:
Process-wide store of font bytes: every file of the bundled stack is read at most once
and its bytes then live for the rest of the process, shared by every consumer.

Main responsibilities:
- read a stack font exactly once per FILE (identity = canonical path, not the spelling
  a caller happened to use);
- extend the bytes to `'static` so both `epaint` (`FontData::from_static`) and `fontdb`
  (`Source::Binary`) can borrow the same copy instead of each keeping their own.

Key functions:
- `bytes`: the bytes of one stack font.

Notes:
The store deliberately has no eviction. Neither of the two consumers can unload a font:
`epaint` has no `remove_font`, and the cosmic-text font cache has no eviction either
(`cosmic-text-0.14.2/src/font/system.rs:91`). The real lifetime of these bytes is
therefore the lifetime of the process, and pretending otherwise would only add a second
copy of ~19 MB in `epaint`.
*/

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use ms_log::runtime_log;

use crate::manifest::StackFont;

/// Cache of the bytes already read this process.
///
/// The identity of a file is its CANONICAL path, so `f.ttf`, `./f.ttf`, an absolute path
/// and a symlink to the same file share one `'static` copy. Every SPELLING a caller has
/// asked for is additionally kept as an alias entry pointing at that same slice, so a
/// repeated call costs one hash lookup and no `canonicalize` syscall.
type ByteCache = HashMap<PathBuf, &'static [u8]>;

/// The store. Created on first use; never cleared.
static FONT_BYTES: OnceLock<Mutex<ByteCache>> = OnceLock::new();

/// Returns the bytes of `font`, reading the file at most once per process.
///
/// A second call for the same FILE returns the very same slice — same address, no second
/// read — even when the two calls spell the path differently (relative vs absolute, a
/// symlink, a `..` component). A consumer may therefore call this freely instead of
/// caching the result itself.
///
/// Returns `None` when the file cannot be read; the reason is logged and the font is
/// simply absent for the rest of the session. Reading is blocking I/O of up to tens of
/// megabytes and must not happen on the GUI thread.
#[must_use]
pub fn bytes(font: &StackFont) -> Option<&'static [u8]> {
    bytes_at_path(font.path.as_path())
}

/// [`bytes`] by path; the store is keyed by file identity, not by `StackFont` identity.
pub(crate) fn bytes_at_path(path: &Path) -> Option<&'static [u8]> {
    let cache = FONT_BYTES.get_or_init(|| Mutex::new(ByteCache::new()));

    // The lock is dropped before the read: reading a 20 MB font is I/O and no lock may be
    // held across it. Two threads racing on the same file therefore read it twice; the
    // loser's copy is dropped below, so the cached slice — and its address — stays unique.
    {
        let cached = lock(cache);
        if let Some(found) = cached.get(path) {
            return Some(found);
        }
    }

    // The spelling is unknown to the store; resolve it to the file's identity before
    // deciding to read, so a second spelling of an already-read file does not leak a
    // second process-lifetime copy of it.
    let (key, canonicalize_error) = canonical_key(path);
    {
        let mut cached = lock(cache);
        if let Some(found) = cached.get(&key).copied() {
            // Known file under a new spelling: alias the spelling, do not read.
            alias(&mut cached, path, &key, found);
            return Some(found);
        }
    }

    let data = match fs::read(path) {
        Ok(data) => data,
        Err(err) => {
            runtime_log::log_warn(format!(
                "[ms_fonts] cannot read font file '{}': {err}; the font is unavailable for \
                 the rest of this session",
                path.display()
            ));
            return None;
        }
    };
    let len = data.len();
    // Only worth reporting when the file IS readable: a canonicalize failure on an
    // unreadable file says nothing the read error above does not already say, and would
    // double every line for a missing font. Logged once per spelling — the alias below
    // makes every later call hit the fast path.
    if let Some(err) = canonicalize_error {
        runtime_log::log_warn(format!(
            "[ms_fonts] font file '{}' is readable but its path cannot be canonicalized: \
             {err}; its bytes are cached under the path as written, so another spelling of \
             the same file would be read a second time",
            path.display()
        ));
    }

    let mut cached = lock(cache);
    let shared = match cached.entry(key.clone()) {
        // Another thread won the race while this one was reading; its slice is the
        // canonical one and the bytes just read are dropped.
        Entry::Occupied(entry) => *entry.get(),
        Entry::Vacant(entry) => {
            // Leaking IS the real lifetime here, not a lost allocation: see the file
            // header. Bounded by the number of files in `fonts/ui`, each read once.
            let leaked: &'static [u8] = Box::leak(data.into_boxed_slice());
            entry.insert(leaked);
            runtime_log::log_info(format!(
                "[ms_fonts] read font '{}' ({len} bytes); it is now shared for the lifetime \
                 of the process",
                path.display()
            ));
            leaked
        }
    };
    alias(&mut cached, path, &key, shared);
    Some(shared)
}

/// Resolves `path` to the cache key identifying the FILE, plus the error that prevented it.
///
/// The key is the canonical path. When canonicalization fails (missing file, unreadable
/// parent directory, a platform that refuses it) the path as written is used instead and
/// the error is handed back so the caller can report it only where it is meaningful; the
/// store then degrades to spelling-keyed caching rather than losing the font.
fn canonical_key(path: &Path) -> (PathBuf, Option<std::io::Error>) {
    match fs::canonicalize(path) {
        Ok(canonical) => (canonical, None),
        Err(err) => (path.to_path_buf(), Some(err)),
    }
}

/// Records `spelling` as another name for the file already cached under `key`.
///
/// Keeps the no-syscall fast path working for whatever spelling a caller uses. A no-op
/// when the spelling already IS the key.
fn alias(cached: &mut ByteCache, spelling: &Path, key: &Path, shared: &'static [u8]) {
    if spelling != key {
        cached.insert(spelling.to_path_buf(), shared);
    }
}

/// Locks the store, recovering the map from a poisoned mutex.
///
/// The guarded sections only look up and insert into a map, so a panic elsewhere cannot
/// leave it half-updated; continuing with the recovered map is preferable to failing every
/// later font read for the rest of the process.
fn lock(cache: &'static Mutex<ByteCache>) -> MutexGuard<'static, ByteCache> {
    match cache.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Tier;

    #[test]
    fn the_same_file_is_read_once_and_shared() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("00-Test.ttf");
        fs::write(&path, b"font bytes; the store never parses them")?;

        let font = StackFont {
            order: 0,
            path: path.clone(),
            family_name: "Ms Test Sans",
            tier: Tier::Core,
        };

        let first = bytes(&font).expect("the file was just written");
        let second = bytes(&font).expect("the second call must hit the cache");

        // Same address, not merely equal contents: the second call must not have read or
        // allocated anything.
        assert_eq!(first.as_ptr(), second.as_ptr());
        assert_eq!(first.len(), second.len());
        assert_eq!(first, b"font bytes; the store never parses them");
        Ok(())
    }

    #[test]
    fn deleting_the_file_after_the_first_read_does_not_invalidate_the_slice() -> Result<(), std::io::Error>
    {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("01-Test.ttf");
        fs::write(&path, b"cached for the whole process")?;

        let first = bytes_at_path(&path).expect("the file was just written");
        fs::remove_file(&path)?;
        let second = bytes_at_path(&path).expect("the cache outlives the file");

        assert_eq!(first.as_ptr(), second.as_ptr());
        Ok(())
    }

    /// Two spellings of ONE physical file must share one process-lifetime copy: the store
    /// promises "read once per file", and a per-spelling cache would leak a second
    /// `Box::leak`ed buffer (tens of megabytes for the big `ext` fonts) per spelling.
    ///
    /// The second spelling routes through a real subdirectory and `..` rather than a
    /// symlink, so the test means the same thing on Linux and on Windows.
    #[test]
    fn two_spellings_of_one_file_share_one_copy() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("02-Test.ttf");
        fs::write(&path, b"one file, two names")?;
        let sub = dir.path().join("sub");
        fs::create_dir(&sub)?;
        // `Path` normalizes away `.` but never `..`, so this really is a DIFFERENT key.
        let detoured = sub.join("..").join("02-Test.ttf");
        assert_ne!(detoured, path, "the two spellings must not compare equal");

        let font = |path: PathBuf| StackFont {
            order: 0,
            path,
            family_name: "Ms Test Sans",
            tier: Tier::Core,
        };
        let first = bytes(&font(path)).expect("the file was just written");
        let second = bytes(&font(detoured)).expect("the detoured spelling names the same file");

        // Same address: the second spelling reused the first read instead of leaking a
        // second copy of the same file.
        assert_eq!(first.as_ptr(), second.as_ptr());
        assert_eq!(first.len(), second.len());
        Ok(())
    }

    #[test]
    fn a_missing_file_yields_none_instead_of_panicking() {
        let missing = Path::new("/definitely/not/a/font/00-Missing.ttf");
        assert_eq!(bytes_at_path(missing), None);
    }
}
