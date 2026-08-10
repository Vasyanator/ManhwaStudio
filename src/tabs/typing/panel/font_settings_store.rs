/*
File: panel/font_settings_store.rs

Purpose:
Process-global runtime store for the app-level per-font settings persisted in
`fonts/fonts_data.json` (via `super::fonts_data`). It owns the authoritative runtime
copy of three things: the user-imported system fonts, the per-font settings
(display-name override + default parameter profile), and the user-defined VIRTUAL font
groups. A single monotonic revision counter lets a GUI poller detect any change and
reload; every mutation snapshots the whole state and saves off-thread.

EVERYTHING IS KEYED BY FONT IDENTITY (`FontEntry::render_identity_name`), never by a file
path. A path survives only as `SystemFontRef::last_path`, the hint that tells the loader
where an imported system font's bytes were last seen.

DEFERRED v1 MIGRATION. A schema-1 `fonts_data.json` keys everything by path. Re-keying it
needs a `path → identity` map, which does not exist until the first font list has been
built — long after this store is seeded at startup. The store therefore seeds the legacy
document VERBATIM with `pending_migration = true`, and `fonts::run_pending_fonts_data_migration`
calls `migrate_legacy_font_keys` at the end of a font-list build (always off the GUI thread
for the combined list; the folder-only list build is cheap and in-memory here). Keys that
resolve are rewritten to identities; keys that resolve to nothing are KEPT VERBATIM and
logged, because a key is the only remaining clue about the font it meant.

Main responsibilities:
- own a thread-safe runtime-global state (imported system fonts + per-font settings +
  virtual groups + the pending-migration flag);
- seed it at startup from `fonts_data.json`, migrating the legacy
  `TextTab.imported_system_fonts` list on first run (`seed_imported_system_fonts_from_config`);
- expose the imported-fonts snapshot / mutate helpers, the display-name override and the
  per-font default profile get/set helpers;
- bump ONE shared monotonic revision on every real mutation so a poller can detect it;
- persist the full state off the GUI thread after any mutation, SERIALIZED via `save_lock`
  and snapshotted afresh inside the writer thread so concurrent mutations coalesce to the
  newest state and never race on the shared temp file.

Key functions:
- `imported_system_fonts` / `system_font_identity_for_path` / `learn_system_font_identity` /
  `set_system_font_path` (the hint follows a font located by name)
- `imported_fonts_revision`
- `add_imported_system_font` / `remove_imported_system_font` / `is_system_font_imported`
- BATCH mutators (`add_imported_system_fonts` / `add_virtual_group_members`): apply many
  entries under ONE write lock with ONE revision bump and ONE persist, so a bulk import does
  not send every open panel through a font reload per added font
- `font_display_name_override` / `set_font_display_name_override`
- `font_profile` / `set_font_profile`
- `virtual_groups` / `create_virtual_group` / `delete_virtual_group` / `rename_virtual_group`
- `add_virtual_group_member` / `remove_virtual_group_member` / `set_virtual_group_member_alias`
- `seed_imported_system_fonts_from_config` / `migrate_legacy_font_keys`

Notes:
`use super::*;` pulls in the parent `panel` module's types and imports (`PathBuf`,
`Path`, `HashSet`, `HashMap`, `thread` = `ms_thread`, `resolve_fonts_dir`, the `fonts_data`
module and the `presets_io` load helper used for the one-time migration). The store is a
plain `OnceLock<RwLock<StoreState>>`; it is not on any hot path, so no generation cache is
needed. Seeding sets the state directly WITHOUT bumping the revision or persisting (it is
the initial state, not a change). Display-name overrides, profiles, imported fonts and
virtual groups share the SAME revision, so a change to any of them reloads both the settings
font lists and the typing panels.
*/

use super::*;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

/// How long a DEBOUNCED save waits before writing, so a burst of per-font profile updates
/// (the panel rewrites a font's profile on every parameter change) collapses into one
/// document write instead of one fsync per keystroke. The cost of the delay is a bounded
/// loss window: a crash within it loses only the newest profile edit, which the user can
/// reproduce by touching the parameter again.
const PROFILE_SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(3);

/// Runtime-global per-font settings state. Not on a hot path.
#[derive(Default)]
struct StoreState {
    /// User-imported system fonts (identity + last-known path hint), first-seen order.
    system_fonts: Vec<fonts_data::SystemFontRef>,
    /// Per-font settings keyed by font IDENTITY (by the legacy PATH key while
    /// `pending_migration` holds). Empty records are never stored.
    fonts: BTreeMap<String, fonts_data::FontSettingsRecord>,
    /// User-defined virtual font groups, in user order. Group names are unique
    /// case-insensitively; members are unique by font identity within a group (enforced by
    /// the mutators). The store cannot see folder groups (filesystem) — a collision of a
    /// virtual name with a real folder-group name is handled at the UI / panel-merge level.
    virtual_groups: Vec<fonts_data::VirtualFontGroup>,
    /// `true` while a schema-1 document is waiting for the deferred re-key (see the file
    /// header). Cleared by `migrate_legacy_font_keys`.
    pending_migration: bool,
}

/// Runtime-global per-font settings state. Lazily created; not on a hot path.
fn store() -> &'static RwLock<StoreState> {
    static STORE: OnceLock<RwLock<StoreState>> = OnceLock::new();
    STORE.get_or_init(|| RwLock::new(StoreState::default()))
}

/// Monotonic revision bumped on every real mutation of the store, so a GUI poller can
/// cheaply detect changes. Seeding does not bump it.
fn revision() -> &'static AtomicU64 {
    static REVISION: AtomicU64 = AtomicU64::new(0);
    &REVISION
}

/// Increments the revision counter. Called only after a mutation actually changed state.
fn bump_revision() {
    revision().fetch_add(1, Ordering::Relaxed);
}

/// Snapshots the full store state as the `fonts_data::FontsData` disk model.
///
/// `pending_migration` IS carried into the snapshot. The document is always written in the
/// CURRENT schema, but a migration that could not resolve every legacy reference must stay
/// flagged: without the flag the rewritten (v2) document would never be retried, and the
/// unresolved keys — the only clue left about the fonts they meant — would be frozen forever
/// while the log claimed they "will apply again".
fn snapshot_data() -> fonts_data::FontsData {
    let guard = match store().read() {
        Ok(guard) => guard,
        // A poisoned lock still holds valid data; recover it rather than panicking.
        Err(poisoned) => poisoned.into_inner(),
    };
    fonts_data::FontsData {
        system_fonts: guard.system_fonts.clone(),
        fonts: guard.fonts.clone(),
        virtual_groups: guard.virtual_groups.clone(),
        pending_migration: guard.pending_migration,
    }
}

/// Serializes all off-thread `fonts_data.json` writers. Every writer thread must hold this
/// lock across its snapshot + save, so concurrent mutations can never rename the shared
/// PID-derived temp file over each other (lost saves / a stale snapshot winning last) and
/// never corrupt the target mid-write.
fn save_lock() -> &'static Mutex<()> {
    static SAVE_LOCK: Mutex<()> = Mutex::new(());
    &SAVE_LOCK
}

/// `true` while a DEBOUNCED writer is already scheduled, so a burst of profile updates
/// spawns exactly one writer thread instead of one per update. It doubles as the "a profile
/// edit is still owed to disk" flag that [`flush_pending_saves`] reads at app exit.
fn debounced_save_scheduled() -> &'static AtomicBool {
    static SCHEDULED: AtomicBool = AtomicBool::new(false);
    &SCHEDULED
}

/// `true` once a corrupt `fonts_data.json` could NOT be moved aside.
///
/// While it holds, EVERY save is refused: the corrupt file is the only surviving copy of
/// whatever the user had, and the first mutation's atomic rename would destroy it. Cleared
/// only by a fresh seed (i.e. a new process, or a test).
fn persistence_blocked() -> &'static AtomicBool {
    static BLOCKED: AtomicBool = AtomicBool::new(false);
    &BLOCKED
}

/// The state of `fonts_data.json` this process last read or wrote — the optimistic-concurrency
/// baseline handed to `fonts_data::save_checked`.
///
/// It is what makes a SECOND running instance of the app detectable: if the file no longer
/// hashes to this fingerprint, someone else wrote it, and blindly renaming our snapshot over
/// it would drop everything they added.
fn disk_baseline() -> &'static Mutex<fonts_data::SaveBaseline> {
    static BASELINE: OnceLock<Mutex<fonts_data::SaveBaseline>> = OnceLock::new();
    BASELINE.get_or_init(|| Mutex::new(fonts_data::SaveBaseline::Unchecked))
}

/// Reads the current save baseline.
fn current_baseline() -> fonts_data::SaveBaseline {
    match disk_baseline().lock() {
        Ok(guard) => *guard,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

/// Replaces the save baseline with the state the document is now known to be in.
fn set_baseline(baseline: fonts_data::SaveBaseline) {
    let mut guard = match disk_baseline().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = baseline;
}

/// How many times [`save_snapshot_now`] retries after merging a conflicting on-disk document.
/// One retry is enough for the case it exists for (another instance wrote once while we were
/// snapshotting); a second conflict means continuous concurrent writing, where refusing and
/// reporting beats spinning.
const SAVE_CONFLICT_RETRIES: usize = 1;

/// Takes the save lock, snapshots the store afresh and writes it. Shared by every writer
/// path. Errors are logged, not surfaced (best-effort save, matching the sibling font
/// writers) — but never swallowed, because each of them means a user setting did not reach
/// disk.
///
/// Three guards run before the bytes are replaced:
/// - a FAILED quarantine disables persistence entirely (see [`persistence_blocked`]);
/// - a document from a NEWER schema is never overwritten (`SaveError::NewerVersion`);
/// - a document that changed since our baseline (a second app instance) is MERGED into the
///   store and the save is retried, so that instance's additions survive instead of being
///   clobbered. Merging adds what we lack and keeps what we have; the accepted asymmetry is
///   that a DELETION performed by the other instance can come back, which is the same
///   "never destroy the last clue" bias the rest of this subsystem follows.
fn save_snapshot_now(fonts_dir: &Path) {
    // Hold the save lock across snapshot + save. Taking the snapshot HERE (not before
    // spawning) means whichever writer acquires the lock LAST observes the newest store
    // state, so the final on-disk document always reflects the latest mutation.
    let _guard = match save_lock().lock() {
        Ok(guard) => guard,
        // A poisoned lock still guards the same section; recover rather than panic.
        Err(poisoned) => poisoned.into_inner(),
    };
    if persistence_blocked().load(Ordering::Acquire) {
        crate::runtime_log::log_error(format!(
            "typing: not persisting fonts_data.json — a corrupt document at {} could not be \
             quarantined, and it is the only copy of these settings. Move or delete it by \
             hand and restart to re-enable saving.",
            fonts_data::data_path(fonts_dir).display()
        ));
        return;
    }
    for attempt in 0..=SAVE_CONFLICT_RETRIES {
        let data = snapshot_data();
        match fonts_data::save_checked(fonts_dir, &data, current_baseline()) {
            Ok(fingerprint) => {
                set_baseline(fonts_data::SaveBaseline::Matching(fingerprint));
                return;
            }
            Err(fonts_data::SaveError::Conflict { disk, fingerprint })
                if disk.is_some() && attempt < SAVE_CONFLICT_RETRIES =>
            {
                // `disk.is_some()` is guaranteed by the guard above; `else` cannot happen and
                // would only skip the merge, which the next arm reports.
                if let Some(disk) = disk {
                    crate::runtime_log::log_warn(
                        "typing: fonts_data.json changed on disk since this instance last read \
                         it (another running copy of the app); merging its content into this \
                         session's settings before saving, so nothing it added is lost.",
                    );
                    merge_disk_document(*disk);
                }
                set_baseline(fonts_data::SaveBaseline::Matching(fingerprint));
            }
            Err(err) => {
                crate::runtime_log::log_error(format!(
                    "typing: failed to persist fonts_data.json: {err}"
                ));
                return;
            }
        }
    }
}

/// Folds a document another process wrote back into this process's store: everything it has
/// and we lack is ADDED, everything we already hold is kept as-is.
///
/// See [`save_snapshot_now`] for why the merge is additive-only.
fn merge_disk_document(disk: fonts_data::FontsData) {
    let mut guard = match store().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    merge_disk_into_state(&mut guard, disk);
}

/// Pure core of [`merge_disk_document`], split out so the union rules can be unit-tested
/// without the process-global store.
///
/// Rules, all "ours wins, theirs is added":
/// - an imported system font we do not know by identity (or, for a not-yet-named legacy
///   entry, by path hint) is appended;
/// - a per-font record we do not have is inserted; one we do have is filled FIELD-WISE from
///   theirs only where ours is unset;
/// - a virtual group we do not have is appended; a group we share gains the members we lack,
///   and a member we hold without an alias picks up theirs.
fn merge_disk_into_state(state: &mut StoreState, disk: fonts_data::FontsData) {
    for entry in disk.system_fonts {
        let known = state.system_fonts.iter().any(|ours| {
            (!entry.font.trim().is_empty() && identities_equal(&ours.font, &entry.font))
                || (entry.last_path.is_some() && ours.last_path == entry.last_path)
        });
        if !known {
            state.system_fonts.push(entry);
        }
    }
    for (key, record) in disk.fonts {
        match find_record_key(state, &key) {
            None => {
                state.fonts.insert(key, record);
            }
            Some(existing_key) => {
                let ours = state.fonts.entry(existing_key).or_default();
                if ours.display_name.is_none() {
                    ours.display_name = record.display_name;
                }
                if ours.profile.is_none() {
                    ours.profile = record.profile;
                }
            }
        }
    }
    for group in disk.virtual_groups {
        let Some(ours) = state
            .virtual_groups
            .iter_mut()
            .find(|candidate| names_equal_ci(&candidate.name, &group.name))
        else {
            state.virtual_groups.push(group);
            continue;
        };
        for member in group.members {
            match ours
                .members
                .iter_mut()
                .find(|existing| identities_equal(&existing.font, &member.font))
            {
                None => ours.members.push(member),
                Some(existing) => {
                    if existing.alias.is_none() {
                        existing.alias = member.alias;
                    }
                }
            }
        }
    }
}

/// Persists the store to `fonts_data.json` off the GUI thread, immediately.
///
/// Under `#[cfg(test)]` the body early-returns before spawning, so unit tests never write to
/// disk; the save recipe itself is covered by `fonts_data`'s tests.
fn persist_off_thread() {
    // Tests never touch the real fonts dir; bail before spawning the writer thread.
    if cfg!(test) {
        return;
    }
    let fonts_dir = resolve_fonts_dir();
    let spawn_result = thread::Builder::new()
        .name("typing-save-fonts-data".to_string())
        .spawn(move || save_snapshot_now(&fonts_dir));
    // A failed spawn (e.g. resource exhaustion) would otherwise silently drop the save; log
    // it so a lost persistence is diagnosable instead of vanishing.
    if let Err(err) = spawn_result {
        crate::runtime_log::log_error(format!(
            "typing: failed to spawn fonts_data.json writer thread; change not persisted: {err}"
        ));
    }
}

/// Persists the store off the GUI thread after a short DEBOUNCE, coalescing a burst of
/// updates into one write (see `PROFILE_SAVE_DEBOUNCE`).
///
/// At most one debounced writer exists at a time. The flag is cleared BEFORE the snapshot
/// is taken, so a mutation landing during the write schedules a fresh writer and cannot be
/// lost; the worst case is two writes in a row, never a dropped one.
fn persist_off_thread_debounced() {
    // Already scheduled: that writer will pick up this mutation too.
    if debounced_save_scheduled().swap(true, Ordering::AcqRel) {
        return;
    }
    // Tests never touch the real fonts dir, but the SCHEDULED flag is still set above: it is
    // the "a write is owed" state `flush_pending_saves` acts on, and a test that could not
    // observe it could not cover the exit flush at all.
    if cfg!(test) {
        return;
    }
    let fonts_dir = resolve_fonts_dir();
    let spawn_result = thread::Builder::new()
        .name("typing-save-fonts-data-debounced".to_string())
        .spawn(move || {
            thread::sleep(PROFILE_SAVE_DEBOUNCE);
            debounced_save_scheduled().store(false, Ordering::Release);
            save_snapshot_now(&fonts_dir);
        });
    if let Err(err) = spawn_result {
        // Nothing is scheduled after a failed spawn; clear the flag so the NEXT mutation
        // can try again instead of believing a writer is pending forever.
        debounced_save_scheduled().store(false, Ordering::Release);
        crate::runtime_log::log_error(format!(
            "typing: failed to spawn debounced fonts_data.json writer thread; change not \
             persisted: {err}"
        ));
    }
}

/// Whether a DEBOUNCED save is still owed to disk (a profile edit inside the debounce
/// window). Test-only observer of the flag `flush_pending_saves` consumes at app exit.
#[cfg(test)]
#[must_use]
pub(in crate::tabs::typing) fn pending_debounced_save() -> bool {
    debounced_save_scheduled().load(Ordering::Acquire)
}

/// Writes a still-pending DEBOUNCED save immediately, on the CALLING thread. Returns whether
/// there was anything to flush.
///
/// This exists for app teardown: `set_font_profile` (called on every parameter edit) persists
/// through a `PROFILE_SAVE_DEBOUNCE`-long debounce, so closing the app within that window
/// used to lose the edit outright — the writer thread is detached and dies with the process.
/// It runs SYNCHRONOUSLY because at `on_exit` there is no GUI frame left to keep responsive
/// and no thread that would outlive the call; the work is one atomic write of a small JSON
/// document, the same one the debounced writer would have done.
///
/// Under `#[cfg(test)]` the flag is cleared but nothing is written (the process-global store
/// must never touch the real fonts dir from a test); the write recipe itself is covered by
/// `fonts_data`'s tests.
pub(in crate::tabs::typing) fn flush_pending_saves() -> bool {
    if !debounced_save_scheduled().swap(false, Ordering::AcqRel) {
        return false;
    }
    if cfg!(test) {
        return true;
    }
    save_snapshot_now(&resolve_fonts_dir());
    true
}

/// Removes duplicate imported system fonts while preserving first-seen order. Two entries
/// are the same when they name the same font AND the same path hint.
fn dedup_system_fonts(
    entries: Vec<fonts_data::SystemFontRef>,
) -> Vec<fonts_data::SystemFontRef> {
    let mut seen: HashSet<(String, Option<PathBuf>)> = HashSet::new();
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        if seen.insert((entry.font.clone(), entry.last_path.clone())) {
            out.push(entry);
        }
    }
    out
}

/// Case-insensitive identity comparison, mirroring `fonts::normalize_font_identity` — the
/// one normalization the panel, the provider and the renderer all share.
fn identities_equal(a: &str, b: &str) -> bool {
    fonts::normalize_font_identity(a) == fonts::normalize_font_identity(b)
}

/// Returns the last-known FILE PATHS of the imported system fonts, in stored order.
///
/// These are HINTS, not keys: the loader accepts one only when the file is still there and
/// still claims the recorded PostScript name (`system_font_identity_for_path`). An entry
/// with no path hint contributes nothing here — locating it by name is phase 6 of
/// `dev-docs/font_identity_postscript_plan.md`.
#[must_use]
pub(in crate::tabs::typing) fn imported_system_fonts() -> Vec<PathBuf> {
    let guard = match store().read() {
        Ok(guard) => guard,
        // A poisoned lock still holds valid data; recover it rather than panicking.
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .system_fonts
        .iter()
        .filter_map(|entry| entry.last_path.clone())
        .collect()
}

/// Returns the imported system fonts VERBATIM (identity + path hint), in stored order.
///
/// Unlike [`imported_system_fonts`], nothing is filtered: an entry whose file is missing,
/// unreadable, or no longer holds the font it was imported as is still returned, because it
/// still exists in `fonts_data.json` and the settings UI has to show it — and let the user
/// remove it. A list built only from loadable fonts is what made such an entry unremovable.
#[must_use]
pub(in crate::tabs::typing) fn imported_system_font_refs() -> Vec<fonts_data::SystemFontRef> {
    let guard = match store().read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.system_fonts.clone()
}

/// PostScript name recorded for the imported system font whose path hint is `path`, or
/// `None` when the path is unknown or its name has not been learned yet (an unmigrated v1
/// document). The loader compares it against the name the FILE actually claims.
#[must_use]
pub(in crate::tabs::typing) fn system_font_identity_for_path(path: &Path) -> Option<String> {
    let guard = match store().read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .system_fonts
        .iter()
        .find(|entry| entry.last_path.as_deref() == Some(path))
        .map(|entry| entry.font.clone())
        .filter(|font| !font.trim().is_empty())
}

/// Records the PostScript name of the imported system font at `path`, but only when it is
/// not known yet (i.e. the entry came from an unmigrated v1 document).
///
/// Returns whether anything changed. A learned name is persisted but does NOT bump the
/// revision: it is a re-encoding of what the file already said, not a user-visible change,
/// and bumping would send every open panel through a pointless font reload.
pub(in crate::tabs::typing) fn learn_system_font_identity(path: &Path, identity: &str) -> bool {
    let identity = identity.trim();
    if identity.is_empty() {
        return false;
    }
    let changed = {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard
            .system_fonts
            .iter_mut()
            .find(|entry| entry.last_path.as_deref() == Some(path) && entry.font.trim().is_empty())
        {
            None => false,
            Some(entry) => {
                entry.font = identity.to_string();
                true
            }
        }
    };
    if changed {
        persist_off_thread();
    }
    changed
}

/// Rewrites the recorded `last_path` HINT of the imported system font named `identity`
/// (matched case-insensitively) to `path`.
///
/// Called after the loader located that font BY NAME somewhere other than where it was last
/// seen, so the next launch resolves it from the hint again instead of scanning the system.
/// Returns whether anything changed.
///
/// Like [`learn_system_font_identity`], it persists but does NOT bump the revision: the font
/// is the same font, only the note about where its bytes live has been refreshed. Bumping
/// would send every open panel through another font reload immediately after the reload that
/// just relocated it, with nothing new to show.
pub(in crate::tabs::typing) fn set_system_font_path(identity: &str, path: PathBuf) -> bool {
    let identity = identity.trim();
    if identity.is_empty() {
        return false;
    }
    let changed = {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            // A poisoned lock still holds valid data; recover it rather than panicking.
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard
            .system_fonts
            .iter_mut()
            .find(|entry| identities_equal(&entry.font, identity))
        {
            None => false,
            Some(entry) => {
                if entry.last_path.as_deref() == Some(path.as_path()) {
                    false
                } else {
                    entry.last_path = Some(path);
                    true
                }
            }
        }
    };
    if changed {
        persist_off_thread();
    }
    changed
}

/// Returns the current revision, bumped on every mutation of the store.
#[must_use]
pub(in crate::tabs::typing) fn imported_fonts_revision() -> u64 {
    revision().load(Ordering::Relaxed)
}

/// Imports the system font named `identity` (its PostScript name), recording `path` as the
/// hint of where its bytes were last seen.
///
/// Returns `true` if it was added; on an add, bumps the revision and persists off-thread.
/// Returns `false` (no revision bump, no persist) when a font with that identity — or, for
/// a not-yet-migrated entry, that exact path — is already imported.
pub(in crate::tabs::typing) fn add_imported_system_font(identity: &str, path: PathBuf) -> bool {
    let identity = identity.trim().to_string();
    if identity.is_empty() {
        crate::runtime_log::log_warn(format!(
            "typing: refusing to import a system font with no usable PostScript name. Path: {}",
            path.display()
        ));
        return false;
    }
    {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let already = guard.system_fonts.iter().any(|entry| {
            identities_equal(&entry.font, &identity) || entry.last_path.as_ref() == Some(&path)
        });
        if already {
            return false;
        }
        guard.system_fonts.push(fonts_data::SystemFontRef {
            font: identity,
            last_path: Some(path),
        });
    }
    bump_revision();
    persist_off_thread();
    true
}

/// Imports SEVERAL system fonts in ONE mutation: one write-lock section, ONE revision bump
/// and ONE persist for the whole batch, however many fonts were added.
///
/// `fonts` is a slice of `(identity, path)` pairs, applied in the given order with exactly the
/// per-font rules of [`add_imported_system_font`]: a blank identity is refused (with a warning
/// naming the path), and a font already imported — by IDENTITY (case-insensitively) or by that
/// exact path hint — is SKIPPED, including one added earlier in the same batch. Returns how
/// many entries were really added.
///
/// Returns `0` without bumping the revision and without persisting when the batch adds nothing
/// (an empty slice, or every entry a duplicate). Bumping per font would make every open panel
/// reload once per imported font, which is the whole reason this batch form exists.
pub(in crate::tabs::typing) fn add_imported_system_fonts(fonts: &[(String, PathBuf)]) -> usize {
    // Blank identities are reported BEFORE the lock is taken: logging can block on the log
    // sink, and the store's write lock must not be held across it.
    for (identity, path) in fonts {
        if identity.trim().is_empty() {
            crate::runtime_log::log_warn(format!(
                "typing: refusing to import a system font with no usable PostScript name. Path: {}",
                path.display()
            ));
        }
    }
    let added = {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            // A poisoned lock still holds valid data; recover it rather than panicking.
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut added = 0usize;
        for (identity, path) in fonts {
            let identity = identity.trim();
            if identity.is_empty() {
                continue;
            }
            // Checked against the LIVE vector, so a pair duplicated inside the batch is caught
            // by the entry its own predecessor pushed.
            let already = guard.system_fonts.iter().any(|entry| {
                identities_equal(&entry.font, identity) || entry.last_path.as_ref() == Some(path)
            });
            if already {
                continue;
            }
            guard.system_fonts.push(fonts_data::SystemFontRef {
                font: identity.to_string(),
                last_path: Some(path.clone()),
            });
            added += 1;
        }
        added
    };
    if added > 0 {
        bump_revision();
        persist_off_thread();
    }
    added
}

/// Removes the imported system font named `identity` (case-insensitively). Returns `true`
/// if an entry was removed; on a removal, bumps the revision and persists off-thread.
/// Returns `false` (no revision bump, no persist) when nothing matched.
pub(in crate::tabs::typing) fn remove_imported_system_font(identity: &str) -> bool {
    let identity = identity.trim();
    if identity.is_empty() {
        return false;
    }
    let removed = {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let before = guard.system_fonts.len();
        guard
            .system_fonts
            .retain(|entry| !identities_equal(&entry.font, identity));
        guard.system_fonts.len() != before
    };
    if removed {
        bump_revision();
        persist_off_thread();
    }
    removed
}

/// Whether a system font with this IDENTITY is currently imported (case-insensitive).
#[must_use]
pub(in crate::tabs::typing) fn is_system_font_imported(identity: &str) -> bool {
    let identity = identity.trim();
    if identity.is_empty() {
        return false;
    }
    let guard = match store().read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .system_fonts
        .iter()
        .any(|entry| identities_equal(&entry.font, identity))
}

/// Returns the user display-name override for the font `identity`, or `None` when the font
/// has no override. The override is display-only; the font's render/inline-tag identity is
/// never affected.
#[must_use]
pub(in crate::tabs::typing) fn font_display_name_override(identity: &str) -> Option<String> {
    let guard = match store().read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    find_record(&guard, identity).and_then(|record| record.display_name.clone())
}

/// The stored key of the per-font record for `identity`, compared the way every identity in
/// the app is compared: case-insensitively (`fonts::normalize_font_identity`).
///
/// An EXACT `BTreeMap::get` would contradict that contract in the worst possible way — a
/// mutation arriving with different casing would create a SECOND record for the same font,
/// of which only one is ever active, and the other silently shadows the user's setting.
#[must_use]
fn find_record_key(state: &StoreState, identity: &str) -> Option<String> {
    let wanted = fonts::normalize_font_identity(identity);
    // Fast path: identities are stored with their original casing, so an exact hit is the
    // overwhelmingly common case and costs one lookup instead of a scan.
    if state.fonts.contains_key(identity) {
        return Some(identity.to_string());
    }
    state
        .fonts
        .keys()
        .find(|key| fonts::normalize_font_identity(key) == wanted)
        .cloned()
}

/// The per-font record for `identity`, matched case-insensitively (see [`find_record_key`]).
#[must_use]
fn find_record<'a>(
    state: &'a StoreState,
    identity: &str,
) -> Option<&'a fonts_data::FontSettingsRecord> {
    let wanted = fonts::normalize_font_identity(identity);
    state
        .fonts
        .get(identity)
        .or_else(|| {
            state
                .fonts
                .iter()
                .find(|(key, _)| fonts::normalize_font_identity(key) == wanted)
                .map(|(_, record)| record)
        })
}

/// Sets or clears the display-name override for the font `identity`.
///
/// `name = None` or a blank/whitespace-only string REMOVES the override. Returns `true`
/// when the stored state actually changed; on a real change bumps the shared revision and
/// persists off-thread. A no-op (setting the same value, or clearing an absent override)
/// returns `false` without bumping the revision or persisting.
pub(in crate::tabs::typing) fn set_font_display_name_override(
    identity: &str,
    name: Option<String>,
) -> bool {
    // A blank override behaves identically to "no override", so normalize it to a removal.
    let normalized = name
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let changed = mutate_font_record(identity, |record| {
        if record.display_name == normalized {
            return false;
        }
        record.display_name = normalized.clone();
        true
    });
    if changed {
        bump_revision();
        persist_off_thread();
    }
    changed
}

/// Returns the font's persisted DEFAULT parameter profile, or `None` when it has none.
#[must_use]
pub(in crate::tabs::typing) fn font_profile(identity: &str) -> Option<Value> {
    let guard = match store().read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    find_record(&guard, identity).and_then(|record| record.profile.clone())
}

/// Stores (or, with `None`, clears) the font's DEFAULT parameter profile — the parameters
/// the panel restores the next time this font is selected, in this session or a later run.
///
/// Returns whether the stored value changed. A change persists through the DEBOUNCED writer
/// (a profile is rewritten on every parameter edit, so an immediate fsync per edit would be
/// pure write amplification) and deliberately does NOT bump the revision: the profile is
/// panel state, and bumping would send every open panel through a font reload on each edit.
pub(in crate::tabs::typing) fn set_font_profile(identity: &str, profile: Option<Value>) -> bool {
    let changed = mutate_font_record(identity, |record| {
        if record.profile == profile {
            return false;
        }
        record.profile = profile.clone();
        true
    });
    if changed {
        persist_off_thread_debounced();
    }
    changed
}

/// Applies `edit` to the settings record of `identity`, creating it on demand and dropping
/// it again when the edit left it empty. Returns whatever `edit` reported.
///
/// Keeping the "create on demand / drop when empty" rule in ONE place is what guarantees the
/// document never accumulates records that carry nothing. The record is located
/// CASE-INSENSITIVELY (`find_record_key`) and mutated under its EXISTING key, so a caller
/// spelling the identity differently edits the one record instead of creating a second.
fn mutate_font_record(
    identity: &str,
    edit: impl FnOnce(&mut fonts_data::FontSettingsRecord) -> bool,
) -> bool {
    let identity = identity.trim();
    if identity.is_empty() {
        return false;
    }
    let mut guard = match store().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let key = find_record_key(&guard, identity).unwrap_or_else(|| identity.to_string());
    let record = guard.fonts.entry(key.clone()).or_default();
    let changed = edit(record);
    if record.is_empty() {
        guard.fonts.remove(&key);
    }
    changed
}

/// Case-insensitive name equality (Unicode-aware, so Cyrillic group names fold correctly).
fn names_equal_ci(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// Returns a snapshot clone of the virtual font groups, in user order. Consumed by the
/// typing create/edit panels (`create_state`), which inject these into the combobox group
/// list via `fonts::apply_virtual_groups` on every font (re)load.
#[must_use]
pub(in crate::tabs::typing) fn virtual_groups() -> Vec<fonts_data::VirtualFontGroup> {
    let guard = match store().read() {
        Ok(guard) => guard,
        // A poisoned lock still holds valid data; recover it rather than panicking.
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.virtual_groups.clone()
}

/// Returns every virtual group containing the font `identity`, as `(group name, per-group
/// alias)`. Cheap (in-memory scan); GUI-thread safe. Matching is case-insensitive, like
/// everywhere an identity is compared.
#[must_use]
pub(in crate::tabs::typing) fn virtual_groups_for_font(
    identity: &str,
) -> Vec<(String, Option<String>)> {
    let guard = match store().read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .virtual_groups
        .iter()
        .filter_map(|group| {
            group
                .members
                .iter()
                .find(|member| identities_equal(&member.font, identity))
                .map(|member| (group.name.clone(), member.alias.clone()))
        })
        .collect()
}

/// Creates an empty virtual font group named `name` (trimmed). Returns `true` on creation;
/// bumps the shared revision and persists off-thread only then. Returns `false` (no change)
/// when `name` is blank or case-insensitively duplicates an existing VIRTUAL group name.
///
/// NOTE: the store cannot see folder groups (they live on the filesystem under
/// `fonts/groups/`), so a collision of a virtual name with a real FOLDER-group name is NOT
/// rejected here. That is validated at the UI level and handled defensively when the panel
/// merges virtual and folder groups (other tasks).
pub(in crate::tabs::typing) fn create_virtual_group(name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return false;
    }
    let created = {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard
            .virtual_groups
            .iter()
            .any(|group| names_equal_ci(&group.name, name))
        {
            false
        } else {
            guard.virtual_groups.push(fonts_data::VirtualFontGroup {
                name: name.to_string(),
                members: Vec::new(),
            });
            true
        }
    };
    if created {
        bump_revision();
        persist_off_thread();
    }
    created
}

/// Deletes the virtual group whose name EXACTLY equals `name`. Returns `true` when a group
/// was removed (then bumps the revision and persists off-thread); `false` when none matched.
pub(in crate::tabs::typing) fn delete_virtual_group(name: &str) -> bool {
    let removed = {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let before = guard.virtual_groups.len();
        guard.virtual_groups.retain(|group| group.name != name);
        guard.virtual_groups.len() != before
    };
    if removed {
        bump_revision();
        persist_off_thread();
    }
    removed
}

/// Renames the virtual group named EXACTLY `old` to `new` (trimmed). Returns `true` on a real
/// rename (then bumps the revision and persists off-thread). Returns `false` when `new` is
/// blank, `old` does not exist, `new` equals the current name (no-op), or `new` collides
/// case-insensitively with a DIFFERENT existing group.
pub(in crate::tabs::typing) fn rename_virtual_group(old: &str, new: &str) -> bool {
    let new = new.trim();
    if new.is_empty() {
        return false;
    }
    let renamed = {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard.virtual_groups.iter().position(|group| group.name == old) {
            None => false,
            Some(idx) => {
                if guard.virtual_groups[idx].name == new {
                    // Unchanged (an exact case-only change is still allowed below).
                    false
                } else if guard
                    .virtual_groups
                    .iter()
                    .enumerate()
                    .any(|(other, group)| other != idx && names_equal_ci(&group.name, new))
                {
                    // A different group already owns this name (case-insensitively).
                    false
                } else {
                    guard.virtual_groups[idx].name = new.to_string();
                    true
                }
            }
        }
    };
    if renamed {
        bump_revision();
        persist_off_thread();
    }
    renamed
}

/// Adds the font `identity` to the virtual group named EXACTLY `group`. Returns `true` on a
/// real add (then bumps the revision and persists off-thread). Returns `false` when the group
/// is unknown, `identity` is blank, or the font is already a member.
pub(in crate::tabs::typing) fn add_virtual_group_member(group: &str, identity: &str) -> bool {
    let identity = identity.trim();
    if identity.is_empty() {
        return false;
    }
    let added = {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard
            .virtual_groups
            .iter_mut()
            .find(|candidate| candidate.name == group)
        {
            None => false,
            Some(candidate) => {
                if candidate
                    .members
                    .iter()
                    .any(|member| identities_equal(&member.font, identity))
                {
                    false
                } else {
                    candidate.members.push(fonts_data::VirtualFontGroupMember {
                        font: identity.to_string(),
                        alias: None,
                    });
                    true
                }
            }
        }
    };
    if added {
        bump_revision();
        persist_off_thread();
    }
    added
}

/// Adds SEVERAL fonts to the virtual group named EXACTLY `group` in ONE mutation: one
/// write-lock section, ONE revision bump and ONE persist for the whole batch.
///
/// `members` is a slice of `(identity, alias)` pairs appended in the given order, so the
/// caller's order becomes the group's member order. Per member: a blank identity is refused
/// (with a warning), a font that is ALREADY a member is SKIPPED and its existing alias is left
/// untouched (a batch import must never silently rewrite the aliases the user set), and a
/// blank/whitespace-only alias is stored as `None`, exactly like
/// [`set_virtual_group_member_alias`] normalizes it. Returns how many members were really
/// added.
///
/// Returns `0` without bumping the revision and without persisting when the group is unknown
/// (which is also logged) or the batch adds nothing.
pub(in crate::tabs::typing) fn add_virtual_group_members(
    group: &str,
    members: &[(String, Option<String>)],
) -> usize {
    // Collected under the lock, logged after it: the log sink can block, and the store's write
    // lock must not be held across it.
    let mut blank_identities = 0usize;
    let mut group_missing = false;
    let added = {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            // A poisoned lock still holds valid data; recover it rather than panicking.
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard
            .virtual_groups
            .iter_mut()
            .find(|candidate| candidate.name == group)
        {
            None => {
                group_missing = true;
                0usize
            }
            Some(candidate) => {
                let mut added = 0usize;
                for (identity, alias) in members {
                    let identity = identity.trim();
                    if identity.is_empty() {
                        blank_identities += 1;
                        continue;
                    }
                    // Checked against the LIVE member list, so a font repeated inside the batch
                    // is caught by the member its own predecessor pushed.
                    if candidate
                        .members
                        .iter()
                        .any(|member| identities_equal(&member.font, identity))
                    {
                        continue;
                    }
                    let alias = alias
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string);
                    candidate.members.push(fonts_data::VirtualFontGroupMember {
                        font: identity.to_string(),
                        alias,
                    });
                    added += 1;
                }
                added
            }
        }
    };
    if group_missing {
        crate::runtime_log::log_warn(format!(
            "typing: cannot add {} font(s) to the virtual group '{group}': no such group.",
            members.len()
        ));
    }
    if blank_identities > 0 {
        crate::runtime_log::log_warn(format!(
            "typing: skipped {blank_identities} member(s) of the virtual group '{group}' with no \
             usable PostScript name."
        ));
    }
    if added > 0 {
        bump_revision();
        persist_off_thread();
    }
    added
}

/// Removes the font `identity` from the virtual group named EXACTLY `group`. Returns `true`
/// when a member was removed (then bumps the revision and persists off-thread); `false` when
/// the group is unknown or the font was not a member.
pub(in crate::tabs::typing) fn remove_virtual_group_member(group: &str, identity: &str) -> bool {
    let removed = {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard
            .virtual_groups
            .iter_mut()
            .find(|candidate| candidate.name == group)
        {
            None => false,
            Some(candidate) => {
                let before = candidate.members.len();
                candidate
                    .members
                    .retain(|member| !identities_equal(&member.font, identity));
                candidate.members.len() != before
            }
        }
    };
    if removed {
        bump_revision();
        persist_off_thread();
    }
    removed
}

/// Sets or clears the per-group display alias of the font `identity` in the virtual group
/// named EXACTLY `group`. `alias = None` or a blank/whitespace-only string CLEARS the alias.
/// Returns `true` when the stored alias actually changed (then bumps the revision and
/// persists off-thread); `false` when the group/member is missing or the alias is unchanged.
pub(in crate::tabs::typing) fn set_virtual_group_member_alias(
    group: &str,
    identity: &str,
    alias: Option<&str>,
) -> bool {
    // A blank alias behaves identically to "no alias", so normalize it to a clear.
    let normalized = alias
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let changed = {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard
            .virtual_groups
            .iter_mut()
            .find(|candidate| candidate.name == group)
        {
            None => false,
            Some(candidate) => match candidate
                .members
                .iter_mut()
                .find(|member| identities_equal(&member.font, identity))
            {
                None => false,
                Some(member) => {
                    if member.alias == normalized {
                        false
                    } else {
                        member.alias = normalized;
                        true
                    }
                }
            },
        }
    };
    if changed {
        bump_revision();
        persist_off_thread();
    }
    changed
}

/// Everything a deferred v1 migration needs to translate legacy PATH keys into font
/// IDENTITIES. Built by `fonts::run_pending_fonts_data_migration` from a finished font list.
#[derive(Debug, Default)]
pub(in crate::tabs::typing) struct LegacyKeyResolution {
    /// Legacy `fonts_data` path key (fonts-dir-relative, else absolute) → font identity.
    /// Covers a font's PRIMARY file and every merged-duplicate `alt_path`.
    pub by_key: HashMap<String, String>,
    /// Font FILE path → that font's UNSUFFIXED identity (its PostScript name), used to fill
    /// in `system_fonts[].font`. Unsuffixed because `system_fonts` names a FILE's face, not
    /// a panel-list entry, and the `%hash` contest suffix is a property of the list.
    pub by_path: HashMap<PathBuf, String>,
    /// Normalized identities of every loaded font (both the list identity and its unsuffixed
    /// form). A key that is ALREADY one of these needs no translation — that is what keeps a
    /// second migration pass (the combined list re-running after a folder-only one) from
    /// reporting the keys the first pass just converted as "unresolved".
    pub identities: HashSet<String>,
}

impl LegacyKeyResolution {
    /// Whether this resolution can translate anything at all. An EMPTY resolution means the
    /// font list was empty (no fonts dir yet, a failed scan), and completing a migration
    /// against it would freeze every legacy key forever.
    #[must_use]
    fn is_empty(&self) -> bool {
        self.by_key.is_empty() && self.by_path.is_empty() && self.identities.is_empty()
    }

    /// Whether `key` is already the identity of a loaded font, i.e. needs no translation.
    #[must_use]
    fn is_current_identity(&self, key: &str) -> bool {
        self.identities.contains(&fonts::normalize_font_identity(key))
    }
}

/// Whether the store is still waiting for the deferred v1 re-key. Cheap; lets a font-list
/// build skip building the resolution maps in the (overwhelmingly common) v2 case.
#[must_use]
pub(in crate::tabs::typing) fn migration_pending() -> bool {
    let guard = match store().read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.pending_migration
}

/// Re-keys a pending schema-1 document from legacy PATH keys to font IDENTITIES.
///
/// `resolution` maps what a finished font list could resolve; it must contain NOTHING derived
/// from a placeholder entry (a font file that could not be read or parsed this run), because
/// such an entry's identity is a file-stem guess, not the font's real name — see
/// `fonts::run_pending_fonts_data_migration`.
///
/// Keys that resolve are rewritten to the font's identity; keys that resolve to nothing are
/// KEPT VERBATIM, logged once, and the migration STAYS PENDING so a later run — where the
/// font is readable again, or reinstalled — converts them. Returns whether anything changed
/// (and was therefore persisted). A no-op when no migration is pending, or when `resolution`
/// is empty.
///
/// COMPLETION RULE: the migration is finished only when EVERY legacy reference resolved.
/// "The list looked complete" is not a licence to finish it — a font that merely happened to
/// be unreadable during this launch would otherwise have its settings frozen (or, worse,
/// re-keyed to a stem-derived guess) permanently. The pending flag is persisted with the
/// document, so the retry survives a restart.
///
/// Does NOT bump the revision: re-keying is a re-encoding of the same settings, and a bump
/// would send every open panel into a redundant font reload right after startup.
pub(in crate::tabs::typing) fn migrate_legacy_font_keys(resolution: &LegacyKeyResolution) -> bool {
    if !migration_pending() || resolution.is_empty() {
        return false;
    }
    let mut unresolved: Vec<String> = Vec::new();
    let changed = {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut changed = false;

        // 1. Per-font settings. Two legacy keys can name one font (a merged duplicate had
        //    one key per copy), and the two records can carry DIFFERENT fields — one copy's
        //    key held the display name, the other's the profile. They are merged field-wise
        //    in `BTreeMap` order (so the outcome does not depend on iteration randomness);
        //    only a field that both records set and that actually differs is dropped, and
        //    that loss is warned about individually.
        let mut rekeyed: BTreeMap<String, fonts_data::FontSettingsRecord> = BTreeMap::new();
        for (key, record) in std::mem::take(&mut guard.fonts) {
            match resolution.by_key.get(&key) {
                Some(identity) => {
                    changed |= identity != &key;
                    match rekeyed.get_mut(identity) {
                        None => {
                            rekeyed.insert(identity.clone(), record);
                        }
                        Some(existing) => {
                            merge_settings_record(existing, record, &key, identity);
                        }
                    }
                }
                None => {
                    // A key that already IS a loaded font's identity was converted by an
                    // earlier pass (or never needed converting); it is not a loss.
                    if !resolution.is_current_identity(&key) {
                        unresolved.push(key.clone());
                    }
                    rekeyed.insert(key, record);
                }
            }
        }
        guard.fonts = rekeyed;

        // 2. Virtual-group members. Re-keying can make two members of one group collapse
        //    onto a single identity (again: a merged duplicate). The surviving member keeps
        //    the FIRST non-empty alias — dropping the second entry's alias when the first
        //    had none was a silent loss of a user-typed string.
        for group in &mut guard.virtual_groups {
            let mut positions: HashMap<String, usize> = HashMap::new();
            let mut members: Vec<fonts_data::VirtualFontGroupMember> =
                Vec::with_capacity(group.members.len());
            for mut member in std::mem::take(&mut group.members) {
                let legacy_key = member.font.clone();
                match resolution.by_key.get(&member.font) {
                    Some(identity) => {
                        changed |= identity != &member.font;
                        member.font = identity.clone();
                    }
                    None => {
                        if !resolution.is_current_identity(&member.font) {
                            unresolved.push(member.font.clone());
                        }
                    }
                }
                let normalized = fonts::normalize_font_identity(&member.font);
                match positions.get(&normalized) {
                    None => {
                        positions.insert(normalized, members.len());
                        members.push(member);
                    }
                    Some(&index) => {
                        changed = true;
                        merge_group_member(&mut members[index], member, &legacy_key, &group.name);
                    }
                }
            }
            group.members = members;
        }

        // 3. Imported system fonts: learn the PostScript name behind each path hint.
        for entry in &mut guard.system_fonts {
            if !entry.font.trim().is_empty() {
                continue;
            }
            match entry
                .last_path
                .as_ref()
                .and_then(|path| resolution.by_path.get(path))
            {
                Some(identity) => {
                    entry.font = identity.clone();
                    changed = true;
                }
                None => unresolved.push(
                    entry
                        .last_path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default(),
                ),
            }
        }

        // Finished ONLY when nothing was left over. Anything else keeps the flag (and the
        // flag is written to disk), so the next list build — and the next launch — retries.
        if unresolved.is_empty() {
            guard.pending_migration = false;
            changed = true;
        }
        changed
    };

    if !unresolved.is_empty() {
        crate::runtime_log::log_warn(format!(
            "typing: fonts_data migration could not resolve {} legacy reference(s) to a loaded \
             font (the font is not installed, or its file could not be read this run); they are \
             KEPT verbatim and the migration stays PENDING, so it is retried on the next font \
             list build and on the next launch: {:?}",
            unresolved.len(),
            unresolved
        ));
    }
    if changed {
        persist_off_thread();
    }
    changed
}

/// Folds the record of a second legacy key that named the same font into `target`.
///
/// A field only `later` carries is ADOPTED (the two legacy keys of a merged duplicate can
/// each hold half of the settings); a field both carry keeps `target`'s value, and the
/// dropped one is named in a warning so the user can see exactly what was discarded.
fn merge_settings_record(
    target: &mut fonts_data::FontSettingsRecord,
    later: fonts_data::FontSettingsRecord,
    later_key: &str,
    identity: &str,
) {
    match (&target.display_name, later.display_name) {
        (Some(kept), Some(dropped)) if *kept != dropped => {
            crate::runtime_log::log_warn(format!(
                "typing: fonts_data migration: legacy key '{later_key}' names the font \
                 '{identity}', which an earlier key already named; both set a display name, so \
                 '{dropped}' is DISCARDED and '{kept}' is kept."
            ));
        }
        (Some(_), _) => {}
        (None, later_name) => target.display_name = later_name,
    }
    match (&target.profile, later.profile) {
        (Some(_), Some(dropped)) => {
            crate::runtime_log::log_warn(format!(
                "typing: fonts_data migration: legacy key '{later_key}' names the font \
                 '{identity}', which an earlier key already named; both store a default \
                 parameter profile, so the later one is DISCARDED: {dropped}"
            ));
        }
        (Some(_), None) => {}
        (None, later_profile) => target.profile = later_profile,
    }
}

/// Folds a second group member that re-keyed onto an existing member's identity into it.
///
/// The surviving member adopts the later alias when it has none; a genuinely different alias
/// on both is reported before being discarded (logging is not saving — a user-typed alias may
/// not vanish without a word).
fn merge_group_member(
    target: &mut fonts_data::VirtualFontGroupMember,
    later: fonts_data::VirtualFontGroupMember,
    later_key: &str,
    group: &str,
) {
    match (&target.alias, later.alias) {
        (None, alias) => target.alias = alias,
        (Some(kept), Some(dropped)) if *kept != dropped => {
            crate::runtime_log::log_warn(format!(
                "typing: fonts_data migration: in group '{group}', legacy member '{later_key}' \
                 re-keys onto '{}', which is already a member with the alias '{kept}'; the \
                 second alias '{dropped}' is DISCARDED.",
                target.font
            ));
        }
        (Some(_), _) => {}
    }
}

/// Seeds the runtime-global store at startup from `fonts/fonts_data.json`, migrating the
/// legacy `TextTab.imported_system_fonts` list on first run.
///
/// The load outcome decides the path:
/// - `Loaded`: use the parsed document (v2 as-is; v1 verbatim + `pending_migration`).
/// - `Missing` (first run): run the one-time legacy `user_config` migration.
/// - `Invalid` (corrupt file): quarantine it to `fonts_data.json.bad` and then run the
///   legacy migration, so a corrupt file is neither trusted nor silently overwritten by the
///   next mutation (which would destroy the recoverable original).
///
/// Sets the state directly WITHOUT bumping the revision or persisting via the mutators — this
/// is the initial state, not a change, so a poller must not treat startup as a mutation.
pub fn seed_imported_system_fonts_from_config() {
    seed_from_fonts_dir(&resolve_fonts_dir());
}

/// Directory-parameterized core of [`seed_imported_system_fonts_from_config`], split out so
/// the load / quarantine / persistence-blocking decisions can be unit-tested against a temp
/// directory instead of the user's real `fonts/`.
pub(in crate::tabs::typing) fn seed_from_fonts_dir(fonts_dir: &Path) {
    let loaded = match fonts_data::load_outcome(fonts_dir) {
        fonts_data::LoadOutcome::Loaded { data, fingerprint } => {
            // The document we just read IS our concurrency baseline: a later save may replace
            // it only while it still hashes to this.
            set_baseline(fonts_data::SaveBaseline::Matching(fingerprint));
            data
        }
        fonts_data::LoadOutcome::Missing => {
            set_baseline(fonts_data::SaveBaseline::Absent);
            migrate_legacy_imported_fonts(fonts_dir)
        }
        fonts_data::LoadOutcome::Invalid => {
            // Move the corrupt document aside before proceeding, so the first mutation's save
            // cannot overwrite (and destroy) a possibly-recoverable file. When that FAILS the
            // corrupt file is the only copy left, so persistence is disabled for the session
            // rather than allowed to run over it.
            match fonts_data::quarantine_bad_file(fonts_dir) {
                // The corrupt file is gone; the next save creates a fresh document.
                fonts_data::QuarantineOutcome::Moved => {
                    set_baseline(fonts_data::SaveBaseline::Absent);
                }
                // The corrupt file is still in place but its content is preserved in the
                // `.bad` copy, so replacing it is safe — and its bytes are not our baseline.
                fonts_data::QuarantineOutcome::Copied => {
                    set_baseline(fonts_data::SaveBaseline::Unchecked);
                }
                fonts_data::QuarantineOutcome::Failed {
                    rename_error,
                    copy_error,
                } => {
                    persistence_blocked().store(true, Ordering::Release);
                    crate::runtime_log::log_error(format!(
                        "typing: per-font settings will NOT be saved this session — the corrupt \
                         {} could not be moved aside (rename: {rename_error}; copy: \
                         {copy_error}), and overwriting it would destroy the only copy of these \
                         settings.",
                        fonts_data::data_path(fonts_dir).display()
                    ));
                }
            }
            migrate_legacy_imported_fonts(fonts_dir)
        }
    };

    let mut guard = match store().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.system_fonts = dedup_system_fonts(loaded.system_fonts);
    guard.fonts = loaded.fonts;
    guard.virtual_groups = loaded.virtual_groups;
    guard.pending_migration = loaded.pending_migration;
}

/// One-time migration of the legacy `user_config.json` imported-fonts list into a fresh
/// `fonts_data.json`. Reads the legacy list via `presets_io`; if it is non-empty it is
/// written once to `fonts_data.json` (the legacy key is left in place, it simply stops being
/// read/written). Best-effort: a save failure is logged but the returned state is still used.
///
/// The legacy list holds PATHS only, so the result is flagged `pending_migration`: the
/// deferred pass learns each font's PostScript name from the first font-list build.
fn migrate_legacy_imported_fonts(fonts_dir: &Path) -> fonts_data::FontsData {
    let legacy: Vec<fonts_data::SystemFontRef> =
        presets_io::load_text_tab_imported_system_fonts()
            .into_iter()
            .map(|path| fonts_data::SystemFontRef {
                font: String::new(),
                last_path: Some(path),
            })
            .collect();
    let migrated = fonts_data::FontsData {
        system_fonts: dedup_system_fonts(legacy),
        fonts: BTreeMap::new(),
        virtual_groups: Vec::new(),
        pending_migration: true,
    };
    if !migrated.system_fonts.is_empty() && !persistence_blocked().load(Ordering::Acquire) {
        match fonts_data::save_checked(fonts_dir, &migrated, current_baseline()) {
            // The document we just wrote becomes the concurrency baseline.
            Ok(fingerprint) => set_baseline(fonts_data::SaveBaseline::Matching(fingerprint)),
            Err(err) => crate::runtime_log::log_warn(format!(
                "typing: failed to migrate imported system fonts into fonts_data.json: {err}"
            )),
        }
    }
    migrated
}

/// Serializes every test that touches the PROCESS-GLOBAL store, including the loader tests
/// in the sibling `panel::tests` module: those seed a real override and would otherwise race
/// this module's own tests, which reset the shared state. Test-only.
#[cfg(test)]
pub(in crate::tabs::typing) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static TEST_LOCK: Mutex<()> = Mutex::new(());
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Clears the shared state to a known-empty baseline for an isolated test. Only the state is
/// reset; the revision counter stays monotonic (tests assert relative deltas). The
/// PROCESS-GLOBAL persistence flags are reset too, or a test that blocked persistence (or
/// scheduled a debounced save) would leak that state into every later test in the binary.
/// Callers must hold [`test_lock`]. Test-only.
#[cfg(test)]
pub(in crate::tabs::typing) fn test_reset() {
    let mut guard = match store().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.system_fonts.clear();
    guard.fonts.clear();
    guard.virtual_groups.clear();
    guard.pending_migration = false;
    drop(guard);
    persistence_blocked().store(false, Ordering::Release);
    debounced_save_scheduled().store(false, Ordering::Release);
    set_baseline(fonts_data::SaveBaseline::Unchecked);
}

/// Whether persistence is currently disabled because a corrupt document could not be
/// quarantined. Test-only accessor for the seeding contract.
#[cfg(test)]
pub(in crate::tabs::typing) fn test_persistence_blocked() -> bool {
    persistence_blocked().load(Ordering::Acquire)
}

/// Installs `data` as the store state directly, exactly as a seed from a document would.
/// Callers must hold [`test_lock`]. Test-only; used by the loader tests in `panel::tests`,
/// which need a pending legacy document without touching the real fonts dir.
#[cfg(test)]
pub(in crate::tabs::typing) fn test_seed(data: fonts_data::FontsData) {
    let mut guard = match store().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.system_fonts = data.system_fonts;
    guard.fonts = data.fonts;
    guard.virtual_groups = data.virtual_groups;
    guard.pending_migration = data.pending_migration;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clears the shared state to a known-empty baseline for an isolated test.
    fn reset_store() {
        test_reset();
    }

    fn lock_tests() -> std::sync::MutexGuard<'static, ()> {
        test_lock()
    }

    /// Installs a legacy (v1) store state directly, as `seed_imported_system_fonts_from_config`
    /// would after reading a path-keyed document — without touching the real fonts dir.
    fn seed_legacy(data: fonts_data::FontsData) {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.system_fonts = data.system_fonts;
        guard.fonts = data.fonts;
        guard.virtual_groups = data.virtual_groups;
        guard.pending_migration = data.pending_migration;
    }

    #[test]
    fn add_dedups_and_reports_insertion() {
        let _lock = lock_tests();
        reset_store();
        let path = PathBuf::from("/fonts/a.ttf");
        assert!(
            add_imported_system_font("A-Regular", path.clone()),
            "first add succeeds"
        );
        assert!(
            !add_imported_system_font("A-Regular", path.clone()),
            "duplicate identity is rejected"
        );
        assert!(
            !add_imported_system_font("a-regular", PathBuf::from("/other/a.ttf")),
            "the identity comparison is case-insensitive"
        );
        assert_eq!(imported_system_fonts(), vec![path]);
    }

    #[test]
    fn remove_reports_presence() {
        let _lock = lock_tests();
        reset_store();
        let path = PathBuf::from("/fonts/b.ttf");
        add_imported_system_font("B-Regular", path);
        assert!(remove_imported_system_font("B-Regular"), "present -> removed");
        assert!(
            !remove_imported_system_font("B-Regular"),
            "absent -> not removed"
        );
        assert!(imported_system_fonts().is_empty());
    }

    #[test]
    fn is_imported_matches_by_identity_not_path() {
        let _lock = lock_tests();
        reset_store();
        add_imported_system_font("C-Regular", PathBuf::from("/fonts/c.ttf"));
        assert!(is_system_font_imported("c-regular"));
        assert!(!is_system_font_imported("D-Regular"));
        assert!(!is_system_font_imported("   "));
    }

    #[test]
    fn revision_increases_only_on_real_mutation() {
        let _lock = lock_tests();
        reset_store();
        let path = PathBuf::from("/fonts/c.ttf");
        let before = imported_fonts_revision();
        assert!(add_imported_system_font("C-Regular", path.clone()));
        let after_add = imported_fonts_revision();
        assert!(after_add > before, "add must bump the revision");
        // A rejected duplicate must NOT bump the revision.
        assert!(!add_imported_system_font("C-Regular", path));
        assert_eq!(
            imported_fonts_revision(),
            after_add,
            "rejected add must not bump the revision"
        );
        // A no-op remove of an absent font must NOT bump the revision.
        assert!(!remove_imported_system_font("Absent-Regular"));
        assert_eq!(
            imported_fonts_revision(),
            after_add,
            "no-op remove must not bump the revision"
        );
    }

    #[test]
    fn display_name_override_set_get_remove() {
        let _lock = lock_tests();
        reset_store();
        let identity = "CCWildWordsLower-Regular";
        assert_eq!(
            font_display_name_override(identity),
            None,
            "no override initially"
        );

        assert!(
            set_font_display_name_override(identity, Some("Мой шрифт".to_string())),
            "first set changes state"
        );
        assert_eq!(
            font_display_name_override(identity).as_deref(),
            Some("Мой шрифт")
        );

        // Setting the SAME value is a no-op.
        assert!(!set_font_display_name_override(
            identity,
            Some("Мой шрифт".to_string())
        ));

        // A blank value removes the override.
        assert!(set_font_display_name_override(identity, Some("   ".to_string())));
        assert_eq!(font_display_name_override(identity), None);

        // Clearing an already-absent override is a no-op.
        assert!(!set_font_display_name_override(identity, None));
    }

    #[test]
    fn override_mutation_bumps_the_shared_revision() {
        let _lock = lock_tests();
        reset_store();
        let identity = "A-Regular";
        let before = imported_fonts_revision();
        assert!(set_font_display_name_override(identity, Some("Name".to_string())));
        assert!(
            imported_fonts_revision() > before,
            "a display-name change must bump the same revision imported-fonts uses"
        );
    }

    #[test]
    fn font_profile_is_stored_and_cleared() {
        let _lock = lock_tests();
        reset_store();
        let identity = "Comic-Regular";
        assert_eq!(font_profile(identity), None, "no profile initially");

        let profile = serde_json::json!({ "schema": 2, "font_size_px": 42.0 });
        assert!(set_font_profile(identity, Some(profile.clone())));
        assert_eq!(font_profile(identity), Some(profile.clone()));
        assert!(
            !set_font_profile(identity, Some(profile)),
            "storing the same profile is a no-op"
        );

        // A profile and a display name live in the SAME record without disturbing each other.
        assert!(set_font_display_name_override(identity, Some("Разговор".to_string())));
        assert!(font_profile(identity).is_some(), "the profile survives a rename");

        assert!(set_font_profile(identity, None), "clearing removes the profile");
        assert_eq!(font_profile(identity), None);
        assert_eq!(
            font_display_name_override(identity).as_deref(),
            Some("Разговор"),
            "clearing the profile must not drop the display name sharing the record"
        );
    }

    #[test]
    fn profile_change_does_not_bump_the_revision() {
        let _lock = lock_tests();
        reset_store();
        let before = imported_fonts_revision();
        assert!(set_font_profile("X-Regular", Some(serde_json::json!({ "a": 1 }))));
        assert_eq!(
            imported_fonts_revision(),
            before,
            "a profile edit is panel state; bumping would force a font reload per keystroke"
        );
    }

    #[test]
    fn create_virtual_group_rejects_blank_and_ci_duplicate() {
        let _lock = lock_tests();
        reset_store();
        assert!(create_virtual_group("  Экшн  "), "first create trims and succeeds");
        assert!(!create_virtual_group("   "), "blank name rejected");
        assert!(
            !create_virtual_group("ЭКШН"),
            "case-insensitive duplicate rejected"
        );
        let groups = virtual_groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "Экшн", "stored name is trimmed");
    }

    #[test]
    fn rename_virtual_group_rejects_collision_and_no_op() {
        let _lock = lock_tests();
        reset_store();
        assert!(create_virtual_group("A"));
        assert!(create_virtual_group("B"));
        assert!(!rename_virtual_group("A", "  b  "), "CI collision with B rejected");
        assert!(!rename_virtual_group("A", "A"), "unchanged rename is a no-op");
        assert!(!rename_virtual_group("missing", "X"), "unknown source rejected");
        assert!(rename_virtual_group("A", "Alpha"), "distinct rename succeeds");
        let names: Vec<String> = virtual_groups().into_iter().map(|group| group.name).collect();
        assert_eq!(names, vec!["Alpha".to_string(), "B".to_string()]);
    }

    #[test]
    fn add_and_remove_virtual_group_member() {
        let _lock = lock_tests();
        reset_store();
        assert!(create_virtual_group("G"));
        assert!(
            !add_virtual_group_member("missing", "A-Regular"),
            "unknown group rejected"
        );
        assert!(!add_virtual_group_member("G", "   "), "blank identity rejected");
        assert!(add_virtual_group_member("G", "A-Regular"), "first add succeeds");
        assert!(
            !add_virtual_group_member("G", "a-regular"),
            "duplicate member rejected case-insensitively"
        );
        assert!(add_virtual_group_member("G", "B-Regular"), "second member succeeds");
        let members: Vec<String> = virtual_groups()
            .into_iter()
            .flat_map(|group| group.members)
            .map(|member| member.font)
            .collect();
        assert_eq!(members, vec!["A-Regular".to_string(), "B-Regular".to_string()]);
        assert!(
            remove_virtual_group_member("G", "A-Regular"),
            "present member removed"
        );
        assert!(
            !remove_virtual_group_member("G", "A-Regular"),
            "absent member -> false"
        );
        assert!(
            !remove_virtual_group_member("missing", "B-Regular"),
            "unknown group -> false"
        );
    }

    #[test]
    fn set_virtual_group_member_alias_set_clear_and_no_op() {
        let _lock = lock_tests();
        reset_store();
        assert!(create_virtual_group("G"));
        assert!(add_virtual_group_member("G", "A-Regular"));
        assert!(
            !set_virtual_group_member_alias("G", "Missing-Regular", Some("X")),
            "unknown member"
        );
        assert!(
            !set_virtual_group_member_alias("missing", "A-Regular", Some("X")),
            "unknown group"
        );
        assert!(
            set_virtual_group_member_alias("G", "A-Regular", Some("  Псевдоним  ")),
            "set trims"
        );
        assert_eq!(
            virtual_groups()[0].members[0].alias.as_deref(),
            Some("Псевдоним")
        );
        assert!(
            !set_virtual_group_member_alias("G", "A-Regular", Some("Псевдоним")),
            "setting the same alias is a no-op"
        );
        assert!(
            set_virtual_group_member_alias("G", "A-Regular", Some("   ")),
            "a blank alias clears it"
        );
        assert_eq!(virtual_groups()[0].members[0].alias, None);
        assert!(
            !set_virtual_group_member_alias("G", "A-Regular", None),
            "clearing an absent alias is a no-op"
        );
    }

    #[test]
    fn virtual_group_mutations_bump_revision_only_on_real_change() {
        let _lock = lock_tests();
        reset_store();
        let before = imported_fonts_revision();
        assert!(create_virtual_group("G"));
        let after_create = imported_fonts_revision();
        assert!(after_create > before, "create must bump the revision");
        // A rejected duplicate create must NOT bump.
        assert!(!create_virtual_group("g"));
        assert_eq!(imported_fonts_revision(), after_create, "rejected create must not bump");
        // A no-op alias set on a non-existent member must NOT bump.
        assert!(!set_virtual_group_member_alias("G", "absent", Some("X")));
        assert_eq!(imported_fonts_revision(), after_create, "no-op alias must not bump");
    }

    // ---- deferred v1 -> v2 migration -------------------------------------------------

    /// The user's REAL v1 document: two virtual groups with eight aliased members, one
    /// imported system font, and a display-name override — everything path-keyed. One
    /// authoritative pass must re-key all of it and finish the migration.
    #[test]
    fn deferred_migration_rekeys_the_real_v1_document() {
        let _lock = lock_tests();
        reset_store();
        let mut fonts = BTreeMap::new();
        fonts.insert(
            "groups/ВВД/Мысли.ttf".to_string(),
            fonts_data::FontSettingsRecord {
                display_name: Some("Мысли".to_string()),
                profile: None,
            },
        );
        let member = |key: &str, alias: &str| fonts_data::VirtualFontGroupMember {
            font: key.to_string(),
            alias: Some(alias.to_string()),
        };
        seed_legacy(fonts_data::FontsData {
            system_fonts: vec![fonts_data::SystemFontRef {
                font: String::new(),
                last_path: Some(PathBuf::from("/home/u/.fonts/Roboto-Medium.ttf")),
            }],
            fonts,
            virtual_groups: vec![
                fonts_data::VirtualFontGroup {
                    name: "Возлюбленная".to_string(),
                    members: vec![
                        member("groups/ВВД/Мысли.ttf", "Мысли"),
                        member("groups/ВВД/Основа.ttf", "Основа"),
                        member("groups/ВВД/Крик.ttf", "Крик"),
                        member("/home/u/.fonts/Roboto-Medium.ttf", "Сис"),
                    ],
                },
                fonts_data::VirtualFontGroup {
                    name: "Экшн".to_string(),
                    members: vec![
                        member("Comic.otf", "Разговор"),
                        member("groups/Империя/Мысли.ttf", "Мысли2"),
                        member("groups/Империя/Основа.ttf", "Основа2"),
                        member("groups/Империя/Крик.ttf", "Крик2"),
                    ],
                },
            ],
            pending_migration: true,
        });

        let mut resolution = LegacyKeyResolution::default();
        for (key, identity) in [
            ("groups/ВВД/Мысли.ttf", "CCWildWordsLower-Italic"),
            ("groups/ВВД/Основа.ttf", "CCWildWordsLower-Regular"),
            ("groups/ВВД/Крик.ttf", "CCWildWordsLower-Bold"),
            ("Comic.otf", "Comic-Regular"),
            ("groups/Империя/Мысли.ttf", "kCCAskForMercy-Italic"),
            ("groups/Империя/Основа.ttf", "kCCAskForMercy-Regular"),
            ("groups/Империя/Крик.ttf", "kCCAskForMercy-Bold"),
            ("/home/u/.fonts/Roboto-Medium.ttf", "Roboto-Medium"),
        ] {
            resolution
                .by_key
                .insert(key.to_string(), identity.to_string());
        }
        resolution.by_path.insert(
            PathBuf::from("/home/u/.fonts/Roboto-Medium.ttf"),
            "Roboto-Medium".to_string(),
        );

        assert!(migrate_legacy_font_keys(&resolution), "the pass changes state");
        assert!(!migration_pending(), "an authoritative pass finishes the migration");

        // Display-name override survives under the identity.
        assert_eq!(
            font_display_name_override("CCWildWordsLower-Italic").as_deref(),
            Some("Мысли")
        );
        assert_eq!(
            font_display_name_override("groups/ВВД/Мысли.ttf"),
            None,
            "the legacy path key is gone"
        );
        // All eight members re-keyed, order and aliases preserved.
        let groups = virtual_groups();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0]
                .members
                .iter()
                .map(|m| m.font.as_str())
                .collect::<Vec<_>>(),
            vec![
                "CCWildWordsLower-Italic",
                "CCWildWordsLower-Regular",
                "CCWildWordsLower-Bold",
                "Roboto-Medium",
            ]
        );
        assert_eq!(groups[0].members[3].alias.as_deref(), Some("Сис"));
        assert_eq!(
            groups[1]
                .members
                .iter()
                .map(|m| m.font.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Comic-Regular",
                "kCCAskForMercy-Italic",
                "kCCAskForMercy-Regular",
                "kCCAskForMercy-Bold",
            ]
        );
        // The imported system font learned its PostScript name and kept its path hint.
        let guard = store().read().unwrap_or_else(|p| p.into_inner());
        assert_eq!(guard.system_fonts.len(), 1);
        assert_eq!(guard.system_fonts[0].font, "Roboto-Medium");
        assert_eq!(
            guard.system_fonts[0].last_path.as_deref(),
            Some(Path::new("/home/u/.fonts/Roboto-Medium.ttf"))
        );
    }

    /// A SECOND pass (the combined list re-running after a folder-only one) must not report
    /// the keys the FIRST pass just converted as lost: a key that already IS a loaded font's
    /// identity needs no translation.
    #[test]
    fn an_already_migrated_key_is_not_treated_as_unresolved() {
        let _lock = lock_tests();
        reset_store();
        seed_legacy(fonts_data::FontsData {
            system_fonts: Vec::new(),
            fonts: BTreeMap::new(),
            virtual_groups: vec![fonts_data::VirtualFontGroup {
                name: "G".to_string(),
                members: vec![fonts_data::VirtualFontGroupMember {
                    // Already an identity (a previous pass converted it).
                    font: "Comic-Regular".to_string(),
                    alias: None,
                }],
            }],
            pending_migration: true,
        });
        let mut resolution = LegacyKeyResolution::default();
        resolution
            .identities
            .insert(fonts::normalize_font_identity("Comic-Regular"));

        // Nothing was left over, so even a NON-authoritative pass may finish the migration.
        assert!(migrate_legacy_font_keys(&resolution));
        assert!(!migration_pending());
        assert_eq!(virtual_groups()[0].members[0].font, "Comic-Regular");
    }

    #[test]
    fn unresolvable_legacy_member_is_kept_not_dropped() {
        let _lock = lock_tests();
        reset_store();
        seed_legacy(fonts_data::FontsData {
            system_fonts: Vec::new(),
            fonts: BTreeMap::new(),
            virtual_groups: vec![fonts_data::VirtualFontGroup {
                name: "G".to_string(),
                members: vec![
                    fonts_data::VirtualFontGroupMember {
                        font: "Known.ttf".to_string(),
                        alias: None,
                    },
                    fonts_data::VirtualFontGroupMember {
                        font: "Uninstalled.ttf".to_string(),
                        alias: Some("Псевдо".to_string()),
                    },
                ],
            }],
            pending_migration: true,
        });
        let mut resolution = LegacyKeyResolution::default();
        resolution
            .by_key
            .insert("Known.ttf".to_string(), "Known-Regular".to_string());

        assert!(migrate_legacy_font_keys(&resolution));
        let members = virtual_groups().remove(0).members;
        assert_eq!(members.len(), 2, "the unresolvable member must NOT be dropped");
        assert_eq!(members[0].font, "Known-Regular");
        assert_eq!(
            members[1].font, "Uninstalled.ttf",
            "an unresolvable key is kept verbatim — it is the only clue left"
        );
        assert_eq!(members[1].alias.as_deref(), Some("Псевдо"));
    }

    #[test]
    fn a_partial_list_leaves_the_migration_pending() {
        let _lock = lock_tests();
        reset_store();
        let mut fonts = BTreeMap::new();
        fonts.insert(
            "Folder.ttf".to_string(),
            fonts_data::FontSettingsRecord {
                display_name: Some("Папка".to_string()),
                profile: None,
            },
        );
        fonts.insert(
            "/sys/Imported.ttf".to_string(),
            fonts_data::FontSettingsRecord {
                display_name: Some("Система".to_string()),
                profile: None,
            },
        );
        seed_legacy(fonts_data::FontsData {
            system_fonts: Vec::new(),
            fonts,
            virtual_groups: Vec::new(),
            pending_migration: true,
        });

        // A FOLDER-ONLY list resolves the folder font but cannot see the imported one.
        let mut folder_only = LegacyKeyResolution::default();
        folder_only
            .by_key
            .insert("Folder.ttf".to_string(), "Folder-Regular".to_string());
        assert!(migrate_legacy_font_keys(&folder_only));
        assert!(
            migration_pending(),
            "an incomplete list must leave the migration pending for the combined pass"
        );
        assert_eq!(
            font_display_name_override("Folder-Regular").as_deref(),
            Some("Папка")
        );

        // The combined pass then finishes it. Like the real resolution builder, it also lists
        // every loaded identity — including the one the first pass already converted, which is
        // therefore not counted as unresolved.
        let mut combined = LegacyKeyResolution::default();
        combined
            .by_key
            .insert("/sys/Imported.ttf".to_string(), "Imported-Regular".to_string());
        for identity in ["Folder-Regular", "Imported-Regular"] {
            combined
                .identities
                .insert(fonts::normalize_font_identity(identity));
        }
        assert!(migrate_legacy_font_keys(&combined));
        assert!(!migration_pending());
        assert_eq!(
            font_display_name_override("Imported-Regular").as_deref(),
            Some("Система")
        );
    }

    #[test]
    fn an_empty_resolution_never_completes_the_migration() {
        let _lock = lock_tests();
        reset_store();
        seed_legacy(fonts_data::FontsData {
            system_fonts: Vec::new(),
            fonts: BTreeMap::new(),
            virtual_groups: vec![fonts_data::VirtualFontGroup {
                name: "G".to_string(),
                members: vec![fonts_data::VirtualFontGroupMember {
                    font: "A.ttf".to_string(),
                    alias: None,
                }],
            }],
            pending_migration: true,
        });
        // No fonts loaded at all (missing fonts dir, failed scan): completing here would
        // freeze every legacy key forever.
        assert!(!migrate_legacy_font_keys(&LegacyKeyResolution::default()));
        assert!(migration_pending());
        assert_eq!(virtual_groups()[0].members[0].font, "A.ttf");
    }

    #[test]
    fn migration_is_a_no_op_for_a_v2_document() {
        let _lock = lock_tests();
        reset_store();
        assert!(create_virtual_group("G"));
        assert!(add_virtual_group_member("G", "A-Regular"));
        let mut resolution = LegacyKeyResolution::default();
        resolution
            .by_key
            .insert("A-Regular".to_string(), "Something-Else".to_string());
        assert!(
            !migrate_legacy_font_keys(&resolution),
            "nothing is pending, so the pass must not touch a v2 store"
        );
        assert_eq!(virtual_groups()[0].members[0].font, "A-Regular");
    }

    #[test]
    fn learn_system_font_identity_fills_only_a_blank_name() {
        let _lock = lock_tests();
        reset_store();
        let path = PathBuf::from("/sys/Roboto-Medium.ttf");
        seed_legacy(fonts_data::FontsData {
            system_fonts: vec![fonts_data::SystemFontRef {
                font: String::new(),
                last_path: Some(path.clone()),
            }],
            fonts: BTreeMap::new(),
            virtual_groups: Vec::new(),
            pending_migration: true,
        });
        assert_eq!(system_font_identity_for_path(&path), None, "not learned yet");
        assert!(learn_system_font_identity(&path, "Roboto-Medium"));
        assert_eq!(
            system_font_identity_for_path(&path).as_deref(),
            Some("Roboto-Medium")
        );
        assert!(
            !learn_system_font_identity(&path, "Something-Else"),
            "a known name is never overwritten by a learn"
        );
        assert_eq!(
            system_font_identity_for_path(&path).as_deref(),
            Some("Roboto-Medium")
        );
    }

    // ---- defect fixes ----------------------------------------------------------------

    /// DEFECT 1. A legacy reference that no loaded font can resolve must leave the migration
    /// PENDING — even when the font list looked complete. The previous rule ("an authoritative
    /// list may declare it finished") froze the reference forever the moment a font file
    /// happened to be unreadable during one launch.
    #[test]
    fn an_unresolved_reference_keeps_the_migration_pending_on_any_list() {
        let _lock = lock_tests();
        reset_store();
        seed_legacy(fonts_data::FontsData {
            system_fonts: Vec::new(),
            fonts: BTreeMap::new(),
            virtual_groups: vec![fonts_data::VirtualFontGroup {
                name: "Возлюбленная".to_string(),
                members: vec![
                    fonts_data::VirtualFontGroupMember {
                        font: "groups/ВВД/Мысли.ttf".to_string(),
                        alias: Some("Мысли".to_string()),
                    },
                    fonts_data::VirtualFontGroupMember {
                        // Its file could not be read this run, so nothing resolves it.
                        font: "groups/ВВД/Основа.ttf".to_string(),
                        alias: Some("Основа".to_string()),
                    },
                ],
            }],
            pending_migration: true,
        });
        let mut resolution = LegacyKeyResolution::default();
        resolution.by_key.insert(
            "groups/ВВД/Мысли.ttf".to_string(),
            "CCWildWordsLower-Italic".to_string(),
        );

        assert!(migrate_legacy_font_keys(&resolution), "the pass re-keys what it can");
        assert!(
            migration_pending(),
            "one unresolved reference is enough to keep the migration pending"
        );
        let members = virtual_groups().remove(0).members;
        assert_eq!(members[0].font, "CCWildWordsLower-Italic");
        assert_eq!(
            members[1].font, "groups/ВВД/Основа.ttf",
            "the unresolved reference is kept VERBATIM — it is the only clue left"
        );
        assert_eq!(members[1].alias.as_deref(), Some("Основа"));

        // The next run reads the file successfully; the retry finishes the job.
        let mut retry = LegacyKeyResolution::default();
        retry.by_key.insert(
            "groups/ВВД/Основа.ttf".to_string(),
            "CCWildWordsLower-Regular".to_string(),
        );
        retry
            .identities
            .insert(fonts::normalize_font_identity("CCWildWordsLower-Italic"));
        assert!(migrate_legacy_font_keys(&retry));
        assert!(!migration_pending(), "nothing is left over now");
        assert_eq!(
            virtual_groups().remove(0).members[1].font,
            "CCWildWordsLower-Regular"
        );
    }

    /// DEFECT 3 (records). Two legacy keys of a merged duplicate can each hold HALF of a
    /// font's settings — one the display name, the other the profile. Keeping only the first
    /// record threw the other half away; they must be merged field-wise.
    #[test]
    fn two_legacy_keys_naming_one_font_merge_field_wise() {
        let _lock = lock_tests();
        reset_store();
        let mut fonts = BTreeMap::new();
        fonts.insert(
            "A-copy.ttf".to_string(),
            fonts_data::FontSettingsRecord {
                display_name: Some("Разговор".to_string()),
                profile: None,
            },
        );
        fonts.insert(
            "B-copy.ttf".to_string(),
            fonts_data::FontSettingsRecord {
                display_name: None,
                profile: Some(serde_json::json!({ "schema": 2, "font_size_px": 42.0 })),
            },
        );
        seed_legacy(fonts_data::FontsData {
            system_fonts: Vec::new(),
            fonts,
            virtual_groups: Vec::new(),
            pending_migration: true,
        });
        let mut resolution = LegacyKeyResolution::default();
        for key in ["A-copy.ttf", "B-copy.ttf"] {
            resolution
                .by_key
                .insert(key.to_string(), "Comic-Regular".to_string());
        }

        assert!(migrate_legacy_font_keys(&resolution));
        assert_eq!(
            font_display_name_override("Comic-Regular").as_deref(),
            Some("Разговор"),
            "the display name from the FIRST key survives"
        );
        assert_eq!(
            font_profile("Comic-Regular"),
            Some(serde_json::json!({ "schema": 2, "font_size_px": 42.0 })),
            "the profile stored under the SECOND key must not be thrown away with its record"
        );
        let guard = store().read().unwrap_or_else(|p| p.into_inner());
        assert_eq!(guard.fonts.len(), 1, "the two keys collapse into one record");
    }

    /// DEFECT 3 (aliases). Two members of one group re-keying onto the same identity must not
    /// silently lose the second member's alias when the first has none.
    #[test]
    fn a_collapsing_group_member_keeps_the_first_non_empty_alias() {
        let _lock = lock_tests();
        reset_store();
        seed_legacy(fonts_data::FontsData {
            system_fonts: Vec::new(),
            fonts: BTreeMap::new(),
            virtual_groups: vec![fonts_data::VirtualFontGroup {
                name: "Экшн".to_string(),
                members: vec![
                    fonts_data::VirtualFontGroupMember {
                        font: "Comic.ttf".to_string(),
                        alias: None,
                    },
                    fonts_data::VirtualFontGroupMember {
                        font: "Comic-copy.ttf".to_string(),
                        alias: Some("Крик".to_string()),
                    },
                ],
            }],
            pending_migration: true,
        });
        let mut resolution = LegacyKeyResolution::default();
        for key in ["Comic.ttf", "Comic-copy.ttf"] {
            resolution
                .by_key
                .insert(key.to_string(), "Comic-Regular".to_string());
        }

        assert!(migrate_legacy_font_keys(&resolution));
        let members = virtual_groups().remove(0).members;
        assert_eq!(members.len(), 1, "the two members collapse onto one identity");
        assert_eq!(members[0].font, "Comic-Regular");
        assert_eq!(
            members[0].alias.as_deref(),
            Some("Крик"),
            "the surviving member must adopt the alias the collapsed one carried"
        );
    }

    /// DEFECT 7. A profile edit persists through a multi-second debounce; closing the app
    /// inside that window used to lose it, because the debounced writer is a detached thread
    /// that dies with the process. The exit flush must find the pending write and consume it.
    #[test]
    fn a_pending_debounced_profile_save_is_flushed_at_exit() {
        let _lock = lock_tests();
        reset_store();
        assert!(
            !pending_debounced_save(),
            "a freshly reset store owes nothing"
        );
        assert!(set_font_profile(
            "Comic-Regular",
            Some(serde_json::json!({ "schema": 2, "font_size_px": 42.0 }))
        ));
        assert!(
            pending_debounced_save(),
            "a profile edit schedules a debounced write"
        );
        assert!(flush_pending_saves(), "the exit flush must find that write");
        assert!(
            !pending_debounced_save(),
            "the flush consumes the pending write"
        );
        assert!(
            !flush_pending_saves(),
            "a second flush has nothing left to do"
        );
    }

    /// DEFECT 8. Identity comparison is case-insensitive everywhere else, so a record must be
    /// found and MUTATED case-insensitively too. Exact map lookups let a differently-cased
    /// caller create a SECOND record for one font, of which only one is ever active.
    #[test]
    fn per_font_records_are_keyed_case_insensitively() {
        let _lock = lock_tests();
        reset_store();
        assert!(set_font_display_name_override(
            "Comic-Regular",
            Some("Разговор".to_string())
        ));
        assert_eq!(
            font_display_name_override("comic-regular").as_deref(),
            Some("Разговор"),
            "a differently-cased read must find the record"
        );

        assert!(set_font_profile(
            "COMIC-REGULAR",
            Some(serde_json::json!({ "schema": 2 }))
        ));
        {
            let guard = store().read().unwrap_or_else(|p| p.into_inner());
            assert_eq!(
                guard.fonts.len(),
                1,
                "a differently-cased write must edit the SAME record, not add a second"
            );
            assert!(
                guard.fonts.contains_key("Comic-Regular"),
                "the record keeps the casing it was first stored with"
            );
        }
        assert_eq!(
            font_display_name_override("Comic-Regular").as_deref(),
            Some("Разговор"),
            "the display name must survive the differently-cased profile write"
        );
        assert!(font_profile("Comic-Regular").is_some());
    }

    /// DEFECT 9. When the corrupt document could be neither renamed nor copied aside, seeding
    /// must DISABLE persistence: it is the only copy of the user's settings, and the first
    /// mutation's atomic rename would destroy it.
    #[test]
    fn a_failed_quarantine_disables_persistence() {
        let _lock = lock_tests();
        reset_store();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("ms_store_quarantine_fail_{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp fonts dir");
        let path = fonts_data::data_path(&dir);
        std::fs::write(&path, "{ not json").expect("write corrupt document");
        // Block BOTH the rename and the copy: a non-empty directory at the `.bad` target.
        let bad = path.with_extension("json.bad");
        std::fs::create_dir_all(&bad).expect("create the blocking directory");
        std::fs::write(bad.join("occupied"), b"x").expect("make it non-empty");

        seed_from_fonts_dir(&dir);
        assert!(
            test_persistence_blocked(),
            "an unquarantinable corrupt document must switch persistence off"
        );
        assert!(
            path.exists(),
            "the corrupt document is the only copy and must stay untouched"
        );

        // The block also survives a mutation attempt: nothing may write while it holds.
        save_snapshot_now(&dir);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap_or_default(),
            "{ not json",
            "a blocked save must not have replaced the corrupt document"
        );

        reset_store();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A quarantine that DOES work leaves persistence enabled — the block is the exception,
    /// not the rule.
    #[test]
    fn a_successful_quarantine_leaves_persistence_enabled() {
        let _lock = lock_tests();
        reset_store();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("ms_store_quarantine_ok_{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp fonts dir");
        std::fs::write(fonts_data::data_path(&dir), "{ not json").expect("write corrupt doc");

        seed_from_fonts_dir(&dir);
        assert!(!test_persistence_blocked());
        assert!(
            fonts_data::data_path(&dir).with_extension("json.bad").exists(),
            "the corrupt document was moved aside"
        );

        reset_store();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The batch import must add every NEW font, skip the ones already imported (by identity,
    /// case-insensitively, or by the exact path hint) including duplicates inside the batch
    /// itself, and cost exactly ONE revision bump — the reason the batch form exists.
    #[test]
    fn a_batch_import_adds_every_new_font_with_one_revision_bump() {
        let _lock = lock_tests();
        reset_store();
        assert!(add_imported_system_font(
            "Old-Regular",
            PathBuf::from("fonts/old.ttf")
        ));
        let before = imported_fonts_revision();
        let added = add_imported_system_fonts(&[
            ("New-Regular".to_string(), PathBuf::from("fonts/new.ttf")),
            // Already imported, differently cased -> skipped.
            ("old-regular".to_string(), PathBuf::from("fonts/other.ttf")),
            // Same font again inside the batch -> skipped.
            ("New-Regular".to_string(), PathBuf::from("fonts/copy.ttf")),
            // Same PATH hint as an entry we just added -> skipped.
            ("Third-Regular".to_string(), PathBuf::from("fonts/new.ttf")),
            ("Second-Regular".to_string(), PathBuf::from("fonts/two.ttf")),
        ]);
        assert_eq!(added, 2, "only the two genuinely new fonts are added");
        assert_eq!(
            imported_fonts_revision(),
            before + 1,
            "the whole batch costs ONE revision bump"
        );
        let guard = store().read().unwrap_or_else(|p| p.into_inner());
        let names: Vec<&str> = guard
            .system_fonts
            .iter()
            .map(|entry| entry.font.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["Old-Regular", "New-Regular", "Second-Regular"],
            "added in the order the caller passed, appended after what was there"
        );
    }

    /// A batch that adds nothing must not bump the revision: a bump forces every open panel
    /// through a font reload, and there would be nothing new to show.
    #[test]
    fn a_batch_import_that_adds_nothing_does_not_bump_the_revision() {
        let _lock = lock_tests();
        reset_store();
        assert!(add_imported_system_font(
            "Only-Regular",
            PathBuf::from("fonts/only.ttf")
        ));
        let before = imported_fonts_revision();
        assert_eq!(add_imported_system_fonts(&[]), 0, "an empty batch adds nothing");
        assert_eq!(
            add_imported_system_fonts(&[
                ("Only-Regular".to_string(), PathBuf::from("fonts/moved.ttf")),
                // Blank identity: refused, never stored.
                ("   ".to_string(), PathBuf::from("fonts/blank.ttf")),
            ]),
            0,
            "a duplicate and a blank identity add nothing"
        );
        assert_eq!(
            imported_fonts_revision(),
            before,
            "no add -> no revision bump"
        );
        assert_eq!(
            imported_system_fonts(),
            vec![PathBuf::from("fonts/only.ttf")],
            "the blank-identity entry must not have been stored"
        );
    }

    /// The batch group add must append the members in the caller's order, skip a font that is
    /// already a member WITHOUT touching its alias, normalize a blank alias to `None`, and
    /// bump the revision exactly once.
    #[test]
    fn a_batch_of_group_members_appends_in_order_and_keeps_existing_aliases() {
        let _lock = lock_tests();
        reset_store();
        assert!(create_virtual_group("Звуки"));
        assert!(add_virtual_group_member("Звуки", "Kept-Regular"));
        assert!(set_virtual_group_member_alias(
            "Звуки",
            "Kept-Regular",
            Some("Оставить")
        ));
        let before = imported_fonts_revision();
        let added = add_virtual_group_members(
            "Звуки",
            &[
                ("First-Regular".to_string(), Some("Первый".to_string())),
                // Already a member (differently cased): skipped, alias untouched.
                ("kept-regular".to_string(), Some("Перезапись".to_string())),
                ("Second-Regular".to_string(), Some("   ".to_string())),
                // Repeated inside the batch: skipped.
                ("first-regular".to_string(), None),
                // Blank identity: refused.
                ("  ".to_string(), None),
            ],
        );
        assert_eq!(added, 2, "only the two genuinely new members are added");
        assert_eq!(
            imported_fonts_revision(),
            before + 1,
            "the whole batch costs ONE revision bump"
        );
        let groups = virtual_groups();
        let members: Vec<(&str, Option<&str>)> = groups[0]
            .members
            .iter()
            .map(|member| (member.font.as_str(), member.alias.as_deref()))
            .collect();
        assert_eq!(
            members,
            vec![
                ("Kept-Regular", Some("Оставить")),
                ("First-Regular", Some("Первый")),
                ("Second-Regular", None),
            ],
            "existing member keeps its alias; new ones append in order, blank alias -> None"
        );
    }

    /// An unknown group and an empty batch both change nothing — and therefore must not bump
    /// the revision either.
    #[test]
    fn a_batch_of_group_members_into_no_group_changes_nothing() {
        let _lock = lock_tests();
        reset_store();
        assert!(create_virtual_group("Звуки"));
        let before = imported_fonts_revision();
        assert_eq!(
            add_virtual_group_members("Нет такой", &[("A-Regular".to_string(), None)]),
            0,
            "an unknown group adds nothing"
        );
        assert_eq!(
            add_virtual_group_members("Звуки", &[]),
            0,
            "an empty batch adds nothing"
        );
        assert_eq!(
            imported_fonts_revision(),
            before,
            "no add -> no revision bump"
        );
        assert!(
            virtual_groups()[0].members.is_empty(),
            "nothing was stored in the existing group"
        );
    }

    /// DEFECT 10 (merge rules). A second running instance added group G1 and a display name
    /// while this instance was editing; merging its document in must ADD what we lack and
    /// keep what we have, so neither instance's work disappears.
    #[test]
    fn merging_another_instances_document_adds_what_we_lack() {
        let mut state = StoreState {
            system_fonts: vec![fonts_data::SystemFontRef {
                font: "Ours-Regular".to_string(),
                last_path: Some(PathBuf::from("/x/ours.ttf")),
            }],
            fonts: BTreeMap::new(),
            virtual_groups: vec![fonts_data::VirtualFontGroup {
                name: "G2".to_string(),
                members: vec![fonts_data::VirtualFontGroupMember {
                    font: "B-Regular".to_string(),
                    alias: None,
                }],
            }],
            pending_migration: false,
        };
        state.fonts.insert(
            "Shared-Regular".to_string(),
            fonts_data::FontSettingsRecord {
                display_name: Some("наше".to_string()),
                profile: None,
            },
        );

        let mut theirs = BTreeMap::new();
        theirs.insert(
            "Shared-Regular".to_string(),
            fonts_data::FontSettingsRecord {
                display_name: Some("их".to_string()),
                profile: Some(serde_json::json!({ "schema": 2 })),
            },
        );
        theirs.insert(
            "Theirs-Regular".to_string(),
            fonts_data::FontSettingsRecord {
                display_name: Some("только их".to_string()),
                profile: None,
            },
        );
        merge_disk_into_state(
            &mut state,
            fonts_data::FontsData {
                system_fonts: vec![fonts_data::SystemFontRef {
                    font: "Theirs-Regular".to_string(),
                    last_path: Some(PathBuf::from("/x/theirs.ttf")),
                }],
                fonts: theirs,
                virtual_groups: vec![
                    fonts_data::VirtualFontGroup {
                        name: "G1".to_string(),
                        members: vec![fonts_data::VirtualFontGroupMember {
                            font: "A-Regular".to_string(),
                            alias: Some("Альфа".to_string()),
                        }],
                    },
                    fonts_data::VirtualFontGroup {
                        name: "G2".to_string(),
                        members: vec![fonts_data::VirtualFontGroupMember {
                            font: "B-Regular".to_string(),
                            alias: Some("Бета".to_string()),
                        }],
                    },
                ],
                pending_migration: false,
            },
        );

        // Their imported font is added; ours is untouched.
        assert_eq!(state.system_fonts.len(), 2);
        assert_eq!(state.system_fonts[1].font, "Theirs-Regular");
        // Their new record is added; the shared one keeps OUR display name but adopts the
        // profile we did not have.
        assert_eq!(
            state
                .fonts
                .get("Shared-Regular")
                .and_then(|record| record.display_name.as_deref()),
            Some("наше")
        );
        assert!(state.fonts["Shared-Regular"].profile.is_some());
        assert_eq!(
            state
                .fonts
                .get("Theirs-Regular")
                .and_then(|record| record.display_name.as_deref()),
            Some("только их")
        );
        // Their group is added; the shared group keeps its member and adopts the alias.
        let names: Vec<&str> = state
            .virtual_groups
            .iter()
            .map(|group| group.name.as_str())
            .collect();
        assert_eq!(names, vec!["G2", "G1"]);
        assert_eq!(
            state.virtual_groups[0].members[0].alias.as_deref(),
            Some("Бета"),
            "a member we hold without an alias picks up theirs"
        );
    }
}
