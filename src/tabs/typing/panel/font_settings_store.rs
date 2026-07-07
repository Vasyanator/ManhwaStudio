/*
File: panel/font_settings_store.rs

Purpose:
Process-global store of the user-imported system font FILE paths (a `Vec<PathBuf>`).
The typing tab lets the user manually import individual font files; this module owns the
authoritative runtime copy of that list, a revision counter a GUI poller can watch, and
the off-thread persistence to `TextTab.imported_system_fonts`.

Main responsibilities:
- own a thread-safe runtime-global list of imported system font paths;
- seed it from `user_config.json` at startup (`seed_imported_system_fonts_from_config`);
- expose snapshot / mutate helpers (`imported_system_fonts`, `add_/remove_...`);
- bump a monotonic revision on every real mutation so a poller can detect changes;
- persist the list off the GUI thread after any mutation.

Key functions:
- `imported_system_fonts` / `imported_fonts_revision` (read access)
- `add_imported_system_font` / `remove_imported_system_font`
- `seed_imported_system_fonts_from_config`

Notes:
`use super::*;` pulls in the parent `panel` module's types and imports (`PathBuf`, `Path`,
`HashSet`, `thread` = `ms_thread`, and the glob-imported `presets_io` load/save helpers).
The store is a plain `OnceLock<RwLock<Vec<PathBuf>>>`; it is not on any hot path, so no
generation cache is needed. Mutators dedup by exact `PathBuf` equality (first-seen order
preserved) and persist off the GUI thread; seeding sets the list directly without bumping
the revision or persisting (it is the initial state, not a change).
*/

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};

/// Runtime-global list of user-imported system font FILE paths. Lazily created; not on a
/// hot path. Order is first-seen; duplicates are removed by the mutators.
fn store() -> &'static RwLock<Vec<PathBuf>> {
    static STORE: OnceLock<RwLock<Vec<PathBuf>>> = OnceLock::new();
    STORE.get_or_init(|| RwLock::new(Vec::new()))
}

/// Monotonic revision bumped on every real mutation of the imported-fonts list, so a GUI
/// poller can cheaply detect changes. Starts at 0; seeding does not bump it.
fn revision() -> &'static AtomicU64 {
    static REVISION: AtomicU64 = AtomicU64::new(0);
    &REVISION
}

/// Increments the revision counter. Called only after a mutation actually changed the list.
fn bump_revision() {
    revision().fetch_add(1, Ordering::Relaxed);
}

/// Removes exact-duplicate paths while preserving first-seen order.
fn dedup_preserve_order(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        if seen.insert(path.clone()) {
            out.push(path);
        }
    }
    out
}

/// Snapshots the current list and persists it off the GUI thread. Errors are logged, not
/// surfaced (best-effort save, matching the other TextTab preset writers).
///
/// Always compiled so `presets_io::save_text_tab_imported_system_fonts` type-checks in every
/// profile. Under `#[cfg(test)]` the body early-returns before spawning, so unit tests never
/// write the real user config; the save recipe itself is covered by `presets_io`'s temp-file
/// tests.
fn persist_off_thread() {
    // Tests never touch the real user config; bail before spawning the writer thread.
    if cfg!(test) {
        return;
    }
    let paths = imported_system_fonts();
    let _ = thread::Builder::new()
        .name("typing-save-imported-fonts".to_string())
        .spawn(move || {
            if let Err(err) = presets_io::save_text_tab_imported_system_fonts(&paths) {
                crate::runtime_log::log_error(format!(
                    "typing: failed to persist imported system fonts: {err}"
                ));
            }
        });
}

/// Returns a snapshot clone of the imported system font paths, in first-seen order.
#[must_use]
pub(in crate::tabs::typing) fn imported_system_fonts() -> Vec<PathBuf> {
    let guard = match store().read() {
        Ok(guard) => guard,
        // A poisoned lock still holds valid data; recover it rather than panicking.
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.clone()
}

/// Returns the current revision, bumped on every mutation of the imported-fonts list.
#[must_use]
pub(in crate::tabs::typing) fn imported_fonts_revision() -> u64 {
    revision().load(Ordering::Relaxed)
}

/// Adds `path` to the list if it is not already present (exact `PathBuf` equality).
/// Returns `true` if it was added; on an add, bumps the revision and persists off-thread.
/// Returns `false` (no revision bump, no persist) when the path is already present.
pub(in crate::tabs::typing) fn add_imported_system_font(path: PathBuf) -> bool {
    {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.iter().any(|existing| existing == &path) {
            return false;
        }
        guard.push(path);
    }
    bump_revision();
    persist_off_thread();
    true
}

/// Removes `path` from the list if present. Returns `true` if it was removed; on a removal,
/// bumps the revision and persists off-thread. Returns `false` (no revision bump, no
/// persist) when the path was not present.
pub(in crate::tabs::typing) fn remove_imported_system_font(path: &Path) -> bool {
    let removed = {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let before = guard.len();
        guard.retain(|existing| existing.as_path() != path);
        guard.len() != before
    };
    if removed {
        bump_revision();
        persist_off_thread();
    }
    removed
}

/// Seeds the runtime-global store from `TextTab.imported_system_fonts` at startup.
/// Best-effort: a missing/malformed config yields an empty list. Sets the list directly
/// WITHOUT bumping the revision or persisting — this is the initial state, not a change,
/// so a poller must not treat startup as a spurious mutation.
pub fn seed_imported_system_fonts_from_config() {
    let loaded = dedup_preserve_order(presets_io::load_text_tab_imported_system_fonts());
    let mut guard = match store().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = loaded;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize tests that mutate the process-global store; parallel tests would otherwise
    // race on the shared list. Revision assertions are relative (after > before), so the
    // shared monotonic counter does not make tests order-dependent.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Clears the shared list to a known-empty baseline for an isolated test. Only the list
    /// is reset; the revision counter stays monotonic (tests assert relative increments).
    fn reset_store() {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.clear();
    }

    fn lock_tests() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn add_dedups_and_reports_insertion() {
        let _lock = lock_tests();
        reset_store();
        let path = PathBuf::from("/fonts/a.ttf");
        assert!(add_imported_system_font(path.clone()), "first add succeeds");
        assert!(
            !add_imported_system_font(path.clone()),
            "duplicate add is rejected"
        );
        assert_eq!(imported_system_fonts(), vec![path]);
    }

    #[test]
    fn remove_reports_presence() {
        let _lock = lock_tests();
        reset_store();
        let path = PathBuf::from("/fonts/b.ttf");
        add_imported_system_font(path.clone());
        assert!(remove_imported_system_font(&path), "present -> removed");
        assert!(
            !remove_imported_system_font(&path),
            "absent -> not removed"
        );
        assert!(imported_system_fonts().is_empty());
    }

    #[test]
    fn revision_increases_only_on_real_mutation() {
        let _lock = lock_tests();
        reset_store();
        let path = PathBuf::from("/fonts/c.ttf");
        let before = imported_fonts_revision();
        assert!(add_imported_system_font(path.clone()));
        let after_add = imported_fonts_revision();
        assert!(after_add > before, "add must bump the revision");
        // A rejected duplicate must NOT bump the revision.
        assert!(!add_imported_system_font(path.clone()));
        assert_eq!(
            imported_fonts_revision(),
            after_add,
            "rejected add must not bump the revision"
        );
        // A no-op remove of an absent path must NOT bump the revision.
        assert!(!remove_imported_system_font(&PathBuf::from("/fonts/absent.ttf")));
        assert_eq!(
            imported_fonts_revision(),
            after_add,
            "no-op remove must not bump the revision"
        );
    }
}
