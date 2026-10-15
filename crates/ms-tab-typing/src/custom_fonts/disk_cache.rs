/*
File: crates/ms-tab-typing/src/custom_fonts/disk_cache.rs

Purpose:
The on-disk cache of COMPILED full custom faces (decision D10):
`<app fonts dir>/custom_cache/<RecipeKey hex>.ttf`. Purely a load-time optimization — a
compiled face is a disposable derivative of its recipe, so any file here may be deleted at any
time and is regenerated on demand.

Key functions:
- `default_cache_dir`: the production directory, or `None` in a test process (no read, no write).
  Evaluated per call by the service, never stored.
- `load`: read + validate one face against its WHOLE key (`RecipeKey::verify_full_face`, the
  rule's one owner in `ms-custom-fonts`); a corrupt or foreign file is removed and reported as
  a miss.
- `store`: atomic write of one face.
- `prune`: removes faces nobody references any more, honouring the grace window.

Notes:
- Blocking file I/O: every function here runs on a worker, never on the GUI thread.
- A hit touches the file's mtime, so the grace rule of `prune` protects recently used faces
  (e.g. ad-hoc faces of chapters that are not open, which no "referenced" set can see).
- The font folder scanner must not list this directory as user fonts:
  `panel::fonts::should_skip_font_dir` skips `CACHE_DIR_NAME`.
- Several app processes may share the directory. Every removal (`load` of an invalid file,
  `prune`) first RENAMES the file to a unique `.aside` sibling, re-checks the moved file (same
  rejected bytes / still stale) and only then deletes it; a file that turns out to be a newer
  publication is put back without replacing anything published since (`restore`). So a valid
  face another process just published is never deleted. Residual: while a file is aside (a
  few syscalls) another process sees a miss and recompiles the face, costing time only. An
  `.aside` file left by a crash is removed by `prune` once it is older than the grace window.
*/

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use ms_custom_fonts::RecipeKey;

use crate::panel::font_settings_store::persistence_suppressed_by_tests;

/// Name of the cache directory inside the app fonts directory.
pub(crate) const CACHE_DIR_NAME: &str = "custom_cache";
/// File extension of a cached face.
const FACE_EXTENSION: &str = "ttf";
/// A file touched within this window is never pruned (14 days).
pub(crate) const PRUNE_GRACE: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// Suffix of a file moved aside for a checked removal.
const ASIDE_SUFFIX: &str = "aside";

/// The production cache directory, or `None` in a test process: tests never read or write the
/// real `fonts/custom_cache/` (same latch as `fonts_data.json`).
#[must_use]
pub(crate) fn default_cache_dir() -> Option<PathBuf> {
    if persistence_suppressed_by_tests() {
        return None;
    }
    Some(ms_config::storage_mode::app_fonts_dir().join(CACHE_DIR_NAME))
}

/// Path of the cached face for `key` inside `dir`.
#[must_use]
pub(crate) fn face_path(dir: &Path, key: &RecipeKey) -> PathBuf {
    dir.join(format!("{}.{FACE_EXTENSION}", key.hex()))
}

/// Why a cached face could not be read or written.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DiskCacheError {
    /// Reading the file failed for a reason other than "not found".
    #[error("could not read cached custom face '{path}': {source}")]
    Read {
        /// The cached file.
        path: PathBuf,
        /// OS error.
        source: io::Error,
    },
    /// Creating the cache directory failed.
    #[error("could not create the custom font cache directory '{path}': {source}")]
    CreateDir {
        /// The directory.
        path: PathBuf,
        /// OS error.
        source: io::Error,
    },
    /// The atomic write failed.
    #[error("could not write cached custom face '{path}': {reason}")]
    Write {
        /// The cached file.
        path: PathBuf,
        /// Writer diagnostic.
        reason: String,
    },
}

/// Result of `load`.
#[derive(Debug)]
pub(crate) enum LoadOutcome {
    /// A valid face (parses, carries this key's full-face names, `RecipeKey::verify_full_face`).
    Hit {
        /// The file bytes.
        bytes: Vec<u8>,
        /// The face's family name (name id 1; empty when it has none).
        family_name: String,
    },
    /// No file for the key.
    Miss,
    /// A file existed but was not a valid face for the key; it was deleted unless it had been
    /// replaced meanwhile (or the deletion failed — `removal_error`). Treated as a miss by the
    /// caller, after logging.
    Invalid {
        /// Why the file was rejected.
        reason: String,
        /// The deletion error, if the file could not be removed.
        removal_error: Option<io::Error>,
    },
}

/// Reads and validates the cached face of `key`.
///
/// A file is valid when it is the compiled full face of exactly this key
/// (`RecipeKey::verify_full_face`: PostScript name AND the full-key unique identifier). A hit
/// touches the file's mtime (best effort: a failure is logged, the hit still counts). An
/// invalid file is removed only while it still holds the rejected bytes (`discard_rejected`).
///
/// # Errors
/// `DiskCacheError::Read` for an I/O failure other than "not found".
pub(crate) fn load(dir: &Path, key: &RecipeKey) -> Result<LoadOutcome, DiskCacheError> {
    load_with(dir, key, &|bytes: &[u8]| {
        key.verify_full_face(bytes).map(|verified| verified.family_name).map_err(|mismatch| mismatch.to_string())
    })
}

/// `load` over an injected validator (`Ok(family name)` / `Err(reason)`); the seam that lets
/// tests exercise the removal protocol without compiling a real face.
fn load_with(dir: &Path, key: &RecipeKey, verify: &dyn Fn(&[u8]) -> Result<String, String>) -> Result<LoadOutcome, DiskCacheError> {
    let path = face_path(dir, key);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(LoadOutcome::Miss),
        Err(source) => return Err(DiskCacheError::Read { path, source }),
    };
    let family_name = match verify(&bytes) {
        Ok(family_name) => family_name,
        Err(reason) => return Ok(discard_rejected(&path, &bytes, reason, verify)),
    };
    touch_hit(&path);
    Ok(LoadOutcome::Hit { bytes, family_name })
}

/// Refreshes the mtime of a hit. The mtime only feeds `prune`'s grace window; a failed touch
/// costs at worst one recompilation after a prune, so it is logged but never fails a hit.
fn touch_hit(path: &Path) {
    if let Err(err) = touch(path) {
        ms_log::runtime_log::log_warn(format!(
            "typing: could not refresh the mtime of cached custom face '{}': {err}",
            path.display()
        ));
    }
}

/// Removes the file at `path` that was rejected with `reason` after reading `rejected` from
/// it — but never a file published there since by another process.
///
/// The file is moved aside atomically first, so what gets judged and deleted is exactly what
/// left `path`: the same bytes as `rejected` -> deleted (`Invalid`); different bytes that pass
/// `verify` (a concurrent publication) -> put back and returned as a `Hit`; different bytes
/// that fail as well -> deleted. A file that vanished meanwhile is someone else's removal.
fn discard_rejected(path: &Path, rejected: &[u8], reason: String, verify: &dyn Fn(&[u8]) -> Result<String, String>) -> LoadOutcome {
    let aside = match move_aside(path) {
        Ok(Some(aside)) => aside,
        Ok(None) => return LoadOutcome::Invalid { reason, removal_error: None },
        Err(err) => return LoadOutcome::Invalid { reason, removal_error: Some(err) },
    };
    let moved = match fs::read(&aside) {
        Ok(moved) => moved,
        Err(err) => {
            // Cannot judge what was moved: put it back rather than risk deleting a valid face.
            restore_logged(&aside, path);
            return LoadOutcome::Invalid { reason, removal_error: Some(err) };
        }
    };
    if moved != rejected
        && let Ok(family_name) = verify(&moved)
    {
        restore_logged(&aside, path);
        touch_hit(path);
        return LoadOutcome::Hit { bytes: moved, family_name };
    }
    LoadOutcome::Invalid { reason, removal_error: fs::remove_file(&aside).err() }
}

/// Atomically renames `path` to a unique `.aside` sibling. `Ok(None)` when `path` no longer
/// exists.
fn move_aside(path: &Path) -> io::Result<Option<PathBuf>> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let aside = path.with_file_name(format!(".{file_name}.{}.{unique}.{ASIDE_SUFFIX}", std::process::id()));
    match fs::rename(path, &aside) {
        Ok(()) => Ok(Some(aside)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Puts the file moved aside back at `path` unless a file was published there since (that
/// newer file wins), and removes the aside name.
fn restore(aside: &Path, path: &Path) -> io::Result<()> {
    match fs::hard_link(aside, path) {
        Ok(()) => fs::remove_file(aside),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => fs::remove_file(aside),
        // A file system without hard links: a plain rename, which could replace a file
        // published in the last few syscalls with this one (a face cached under the same key
        // name either way, so at worst one file is exchanged for an equivalent one).
        Err(_) => fs::rename(aside, path),
    }
}

/// `restore`, logging a failure (the cached face is then lost and recompiled on next use).
fn restore_logged(aside: &Path, path: &Path) {
    if let Err(err) = restore(aside, path) {
        ms_log::runtime_log::log_warn(format!(
            "typing: could not put cached custom face '{}' back from '{}': {err}",
            path.display(),
            aside.display()
        ));
    }
}

/// Sets the file's modification time to now.
fn touch(path: &Path) -> io::Result<()> {
    fs::File::options().write(true).open(path)?.set_modified(SystemTime::now())
}

/// Writes the face of `key` atomically (temp sibling + rename, contents fsynced), creating the
/// directory when needed.
///
/// # Errors
/// `DiskCacheError::CreateDir` / `DiskCacheError::Write`.
pub(crate) fn store(dir: &Path, key: &RecipeKey, bytes: &[u8]) -> Result<(), DiskCacheError> {
    fs::create_dir_all(dir).map_err(|source| DiskCacheError::CreateDir { path: dir.to_path_buf(), source })?;
    let path = face_path(dir, key);
    write_atomic(&path, bytes).map_err(|reason| DiskCacheError::Write { path, reason })
}

#[cfg(not(target_arch = "wasm32"))]
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    ms_docstore::write_bytes_atomic(path, bytes, ms_docstore::Durability::Contents).map_err(|err| err.to_string())
}

/// The web build has no atomic file writer and no fonts directory to cache into.
#[cfg(target_arch = "wasm32")]
fn write_atomic(_path: &Path, _bytes: &[u8]) -> Result<(), String> {
    Err("the custom font disk cache is not available in the web build".to_owned())
}

/// What `prune` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct PruneReport {
    /// Files deleted.
    pub(crate) removed: Vec<PathBuf>,
    /// Files kept (referenced, or touched within the grace window).
    pub(crate) kept: usize,
    /// Files that should have been deleted but could not be (path + OS error text).
    pub(crate) failures: Vec<(PathBuf, String)>,
}

/// Deletes every regular file in `dir` that is neither the face of a key in `keep` nor touched
/// within `grace` of `now`: unreferenced faces, leftover temp files of interrupted writes and
/// foreign names alike. Directories are left alone. A missing `dir` is an empty cache.
///
/// `keep` is the caller's union of the keys saved fonts reference and the keys this process
/// holds in memory or is compiling. The grace window protects faces that no "referenced" set
/// can see (ad-hoc fonts of chapters that are not open).
///
/// A removal is checked (`remove_if_still_stale`): a file another process republished or
/// touched after the listing is kept, never deleted on the strength of stale metadata.
///
/// # Errors
/// The `io::Error` of listing `dir` (other than "not found").
pub(crate) fn prune(dir: &Path, keep: &HashSet<RecipeKey>, now: SystemTime, grace: Duration) -> io::Result<PruneReport> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(PruneReport::default()),
        Err(err) => return Err(err),
    };
    let keep_names: HashSet<String> = keep.iter().map(|key| format!("{}.{FACE_EXTENSION}", key.hex())).collect();
    let mut report = PruneReport::default();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                report.failures.push((path, err.to_string()));
                continue;
            }
        };
        if !metadata.is_file() {
            continue;
        }
        let referenced = entry.file_name().to_str().is_some_and(|name| keep_names.contains(name));
        if referenced || is_recent(&metadata, now, grace) {
            report.kept += 1;
            continue;
        }
        match remove_if_still_stale(&path, now, grace) {
            Ok(StaleRemoval::Removed) => report.removed.push(path),
            Ok(StaleRemoval::Kept) => report.kept += 1,
            Ok(StaleRemoval::Vanished) => {}
            Err(err) => report.failures.push((path, err.to_string())),
        }
    }
    report.removed.sort();
    Ok(report)
}

/// The grace rule. An unreadable mtime counts as "recent": never delete what cannot be dated.
/// A modification time in the future (`duration_since` fails) is recent as well.
fn is_recent(metadata: &fs::Metadata, now: SystemTime, grace: Duration) -> bool {
    match metadata.modified() {
        Ok(modified) => now.duration_since(modified).ok().is_none_or(|age| age < grace),
        Err(_) => true,
    }
}

/// What `remove_if_still_stale` did.
#[derive(Debug, PartialEq, Eq)]
enum StaleRemoval {
    /// The file was still stale and is deleted.
    Removed,
    /// The file had been replaced or touched since the listing; it is back in place.
    Kept,
    /// The file was already gone.
    Vanished,
}

/// Deletes `path` only if the file is STILL stale when it is moved aside: the moved file's own
/// mtime is re-judged, so a face republished (fresh mtime, rename keeps it) or touched by a
/// load hit after the caller's listing is put back instead.
///
/// # Errors
/// The `io::Error` of the move or of the final deletion.
fn remove_if_still_stale(path: &Path, now: SystemTime, grace: Duration) -> io::Result<StaleRemoval> {
    let Some(aside) = move_aside(path)? else {
        return Ok(StaleRemoval::Vanished);
    };
    // Unreadable metadata counts as recent (`is_recent`'s rule): put the file back.
    let still_stale = fs::metadata(&aside).is_ok_and(|metadata| !is_recent(&metadata, now, grace));
    if still_stale {
        fs::remove_file(&aside)?;
        Ok(StaleRemoval::Removed)
    } else {
        restore(&aside, path)?;
        Ok(StaleRemoval::Kept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique scratch directory under the system temp dir, removed on drop.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("ms-custom-cache-{tag}-{}-{unique}", std::process::id()));
            // A leftover of a crashed earlier run with the same pid would only add files;
            // start from an empty directory.
            if path.exists() {
                fs::remove_dir_all(&path).expect("clear stale scratch dir");
            }
            fs::create_dir_all(&path).expect("create scratch dir");
            Self(path)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            if let Err(err) = fs::remove_dir_all(&self.0) {
                eprintln!("could not remove scratch dir {}: {err}", self.0.display());
            }
        }
    }

    /// A bundled `fonts/ui` font, or `None` (logged) when the checkout lacks it.
    fn bundled_font(relative: &str) -> Option<Vec<u8>> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fonts/ui").join(relative);
        match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(err) => {
                eprintln!("skipping: fixture font {} unavailable: {err}", path.display());
                None
            }
        }
    }

    fn key(byte: u8) -> RecipeKey {
        RecipeKey([byte; 32])
    }

    #[test]
    fn tests_never_get_the_real_cache_dir() {
        assert_eq!(default_cache_dir(), None);
    }

    /// A validator that accepts exactly `good` (family "Good").
    fn accepts(good: &'static [u8]) -> impl Fn(&[u8]) -> Result<String, String> {
        move |bytes: &[u8]| if bytes == good { Ok("Good".to_owned()) } else { Err("rejected".to_owned()) }
    }

    /// Names of the entries of `dir` (sorted).
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            fs::read_dir(dir).expect("list").map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn store_then_load_round_trips_a_valid_face() {
        let dir = ScratchDir::new("roundtrip");
        let cache = dir.0.join(CACHE_DIR_NAME);
        let verify = accepts(b"good face");
        assert!(matches!(load_with(&cache, &key(1), &verify).expect("load"), LoadOutcome::Miss));
        store(&cache, &key(1), b"good face").expect("store creates the directory");
        match load_with(&cache, &key(1), &verify).expect("load") {
            LoadOutcome::Hit { bytes: read, family_name } => {
                assert_eq!(read, b"good face");
                assert_eq!(family_name, "Good");
            }
            other => panic!("expected a hit, got {other:?}"),
        }
    }

    #[test]
    fn invalid_files_are_deleted() {
        let Some(bytes) = bundled_font("core/00-NotoSans-Regular.ttf") else { return };
        let dir = ScratchDir::new("invalid");
        store(&dir.0, &key(2), b"not a font").expect("store garbage");
        assert!(matches!(load(&dir.0, &key(2)).expect("load"), LoadOutcome::Invalid { removal_error: None, .. }));
        assert!(!face_path(&dir.0, &key(2)).exists());

        // A real font that is not this key's compiled face is rejected too.
        store(&dir.0, &key(3), &bytes).expect("store");
        assert!(matches!(load(&dir.0, &key(3)).expect("load"), LoadOutcome::Invalid { .. }));
        assert!(!face_path(&dir.0, &key(3)).exists());
        assert!(entries(&dir.0).is_empty(), "no aside file is left behind");
    }

    #[test]
    fn rejecting_stale_bytes_never_deletes_a_face_published_meanwhile() {
        let dir = ScratchDir::new("race");
        let path = face_path(&dir.0, &key(4));
        let verify = accepts(b"good face");
        // Process A read and rejected an old corrupt file; process B then atomically published
        // the valid face at the same path before A got to remove anything.
        fs::write(&path, b"good face").expect("B publishes");
        match discard_rejected(&path, b"old corrupt bytes", "rejected".to_owned(), &verify) {
            LoadOutcome::Hit { bytes, family_name } => {
                assert_eq!(bytes, b"good face");
                assert_eq!(family_name, "Good");
            }
            other => panic!("the published face must be kept and used, got {other:?}"),
        }
        assert_eq!(fs::read(&path).expect("the face is still cached"), b"good face");
        assert_eq!(entries(&dir.0), vec![format!("{}.{FACE_EXTENSION}", key(4).hex())], "no aside file is left behind");

        // The same rejected bytes still in place are removed.
        fs::write(&path, b"old corrupt bytes").expect("rewrite");
        assert!(matches!(
            discard_rejected(&path, b"old corrupt bytes", "rejected".to_owned(), &verify),
            LoadOutcome::Invalid { removal_error: None, .. }
        ));
        assert!(entries(&dir.0).is_empty());

        // A file that vanished meanwhile is someone else's removal.
        assert!(matches!(
            discard_rejected(&path, b"old corrupt bytes", "rejected".to_owned(), &verify),
            LoadOutcome::Invalid { removal_error: None, .. }
        ));
    }

    #[test]
    fn restore_never_replaces_a_newer_publication() {
        let dir = ScratchDir::new("restore");
        let path = face_path(&dir.0, &key(5));
        fs::write(&path, b"first").expect("write");
        let aside = move_aside(&path).expect("move").expect("present");
        assert!(!path.exists());
        fs::write(&path, b"newer").expect("a newer publication");
        restore(&aside, &path).expect("restore");
        assert_eq!(fs::read(&path).expect("read"), b"newer", "the newer file wins");
        assert!(!aside.exists());
    }

    #[test]
    fn prune_keeps_a_file_refreshed_after_the_listing() {
        let dir = ScratchDir::new("prune-race");
        let path = face_path(&dir.0, &key(6));
        // Listed as stale by the caller, then republished (fresh mtime) before the removal.
        fs::write(&path, b"fresh").expect("write");
        let now = SystemTime::now();
        assert_eq!(remove_if_still_stale(&path, now, PRUNE_GRACE).expect("checked removal"), StaleRemoval::Kept);
        assert_eq!(fs::read(&path).expect("kept"), b"fresh");
        assert_eq!(entries(&dir.0).len(), 1, "no aside file is left behind");

        let later = now + PRUNE_GRACE + Duration::from_secs(60);
        assert_eq!(remove_if_still_stale(&path, later, PRUNE_GRACE).expect("removal"), StaleRemoval::Removed);
        assert!(entries(&dir.0).is_empty());
        assert_eq!(remove_if_still_stale(&path, later, PRUNE_GRACE).expect("vanished"), StaleRemoval::Vanished);
    }

    #[test]
    fn prune_keeps_referenced_and_recent_files_and_removes_the_rest() {
        let dir = ScratchDir::new("prune");
        let referenced_old = face_path(&dir.0, &key(10));
        let unreferenced_old = face_path(&dir.0, &key(11));
        let unreferenced_recent = face_path(&dir.0, &key(12));
        let foreign_old = dir.0.join("notes.txt");
        let temp_old = dir.0.join(format!(".{}.ttf.4242.tmp", key(13).hex()));
        let subdir = dir.0.join("nested");
        for path in [&referenced_old, &unreferenced_old, &unreferenced_recent, &foreign_old, &temp_old] {
            fs::write(path, b"x").expect("write fixture file");
        }
        fs::create_dir_all(&subdir).expect("create nested dir");

        // Ages are judged against `now`; push "now" forward instead of back-dating files.
        let now = SystemTime::now() + PRUNE_GRACE + Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&unreferenced_recent)
            .and_then(|file| file.set_modified(now - Duration::from_secs(60)))
            .expect("touch recent file");

        let keep: HashSet<RecipeKey> = [key(10)].into_iter().collect();
        let report = prune(&dir.0, &keep, now, PRUNE_GRACE).expect("prune");
        let mut expected = vec![unreferenced_old.clone(), foreign_old.clone(), temp_old.clone()];
        expected.sort();
        assert_eq!(report.removed, expected);
        assert_eq!(report.kept, 2);
        assert!(report.failures.is_empty());
        assert!(referenced_old.exists() && unreferenced_recent.exists() && subdir.exists());
    }

    #[test]
    fn prune_of_a_missing_dir_is_empty() {
        let dir = ScratchDir::new("missing");
        let report = prune(&dir.0.join("absent"), &HashSet::new(), SystemTime::now(), PRUNE_GRACE).expect("prune");
        assert_eq!(report, PruneReport::default());
    }
}
