/*
File: panel/font_settings_store.rs

Purpose:
Process-global runtime store for the app-level per-font settings persisted in
`fonts/fonts_data.json` (via `super::fonts_data`). It owns the authoritative runtime
copy of three things: the user-imported system font FILE paths, the per-font
display-name overrides, and the user-defined VIRTUAL font groups. A single monotonic
revision counter lets a GUI poller detect any change and reload; every mutation
snapshots the whole state and saves off-thread.

Main responsibilities:
- own a thread-safe runtime-global state (imported paths + display-name overrides);
- seed it at startup from `fonts_data.json`, migrating the legacy
  `TextTab.imported_system_fonts` list on first run (`seed_imported_system_fonts_from_config`);
- expose the imported-fonts snapshot / mutate helpers and the display-name override
  get/set helpers;
- bump ONE shared monotonic revision on every real mutation so a poller can detect it;
- persist the full state off the GUI thread after any mutation, SERIALIZED via `save_lock`
  and snapshotted afresh inside the writer thread so concurrent mutations coalesce to the
  newest state and never race on the shared temp file.

Key functions:
- `imported_system_fonts` / `imported_fonts_revision`
- `add_imported_system_font` / `remove_imported_system_font`
- `font_display_name_override` / `set_font_display_name_override`
- `virtual_groups` / `create_virtual_group` / `delete_virtual_group` / `rename_virtual_group`
- `add_virtual_group_member` / `remove_virtual_group_member` / `set_virtual_group_member_alias`
- `seed_imported_system_fonts_from_config`

Notes:
`use super::*;` pulls in the parent `panel` module's types and imports (`PathBuf`,
`Path`, `HashSet`, `thread` = `ms_thread`, `resolve_fonts_dir`, the `fonts_data` module
and the `presets_io` load helper used for the one-time migration). The store is a plain
`OnceLock<RwLock<StoreState>>`; it is not on any hot path, so no generation cache is
needed. Mutators dedup imported paths by exact `PathBuf` equality (first-seen order
preserved) and persist off the GUI thread; seeding sets the state directly WITHOUT
bumping the revision or persisting (it is the initial state, not a change). Display-name
overrides and imported paths share the SAME revision, so a change to either reloads both
the settings font lists and the typing panels.
*/

use super::*;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

/// Runtime-global per-font settings state. Not on a hot path.
#[derive(Default)]
struct StoreState {
    /// User-imported system font FILE paths, first-seen order, deduped by the mutators.
    imported: Vec<PathBuf>,
    /// Display-name overrides keyed by `fonts_data::font_settings_key`. Blank values are
    /// never stored (a blank set removes the entry).
    overrides: BTreeMap<String, String>,
    /// User-defined virtual font groups, in user order. Group names are unique
    /// case-insensitively; members are unique by font key within a group (enforced by
    /// the mutators). The store cannot see folder groups (filesystem) — a collision of a
    /// virtual name with a real folder-group name is handled at the UI / panel-merge level.
    virtual_groups: Vec<fonts_data::VirtualFontGroup>,
}

/// Runtime-global per-font settings state. Lazily created; not on a hot path.
fn store() -> &'static RwLock<StoreState> {
    static STORE: OnceLock<RwLock<StoreState>> = OnceLock::new();
    STORE.get_or_init(|| RwLock::new(StoreState::default()))
}

/// Monotonic revision bumped on every real mutation of the store (imported paths OR
/// display-name overrides), so a GUI poller can cheaply detect changes. Seeding does
/// not bump it.
fn revision() -> &'static AtomicU64 {
    static REVISION: AtomicU64 = AtomicU64::new(0);
    &REVISION
}

/// Increments the revision counter. Called only after a mutation actually changed state.
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

/// Snapshots the full store state as the `fonts_data::FontsData` disk model.
fn snapshot_data() -> fonts_data::FontsData {
    let guard = match store().read() {
        Ok(guard) => guard,
        // A poisoned lock still holds valid data; recover it rather than panicking.
        Err(poisoned) => poisoned.into_inner(),
    };
    fonts_data::FontsData {
        imported_system_fonts: guard.imported.clone(),
        display_name_overrides: guard.overrides.clone(),
        virtual_groups: guard.virtual_groups.clone(),
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

/// Persists the store to `fonts_data.json` off the GUI thread. The writer thread takes the
/// FRESH snapshot INSIDE `save_lock`, so writers run one at a time and the last completed
/// write always reflects the newest state (coalescing-by-fresh-snapshot). Errors are logged,
/// not surfaced (best-effort save, matching the sibling font writers).
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
        .spawn(move || {
            // Hold the save lock across snapshot + save. Taking the snapshot here (not before
            // spawning) means whichever writer acquires the lock LAST observes the newest
            // store state, so the final on-disk document always reflects the latest mutation.
            let _guard = match save_lock().lock() {
                Ok(guard) => guard,
                // A poisoned lock still guards the same section; recover rather than panic.
                Err(poisoned) => poisoned.into_inner(),
            };
            let data = snapshot_data();
            if let Err(err) = fonts_data::save(&fonts_dir, &data) {
                crate::runtime_log::log_error(format!(
                    "typing: failed to persist fonts_data.json: {err}"
                ));
            }
        });
    // A failed spawn (e.g. resource exhaustion) would otherwise silently drop the save; log
    // it so a lost persistence is diagnosable instead of vanishing.
    if let Err(err) = spawn_result {
        crate::runtime_log::log_error(format!(
            "typing: failed to spawn fonts_data.json writer thread; change not persisted: {err}"
        ));
    }
}

/// Returns a snapshot clone of the imported system font paths, in first-seen order.
#[must_use]
pub(in crate::tabs::typing) fn imported_system_fonts() -> Vec<PathBuf> {
    let guard = match store().read() {
        Ok(guard) => guard,
        // A poisoned lock still holds valid data; recover it rather than panicking.
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.imported.clone()
}

/// Returns the current revision, bumped on every mutation of the store (imported paths
/// or display-name overrides).
#[must_use]
pub(in crate::tabs::typing) fn imported_fonts_revision() -> u64 {
    revision().load(Ordering::Relaxed)
}

/// Adds `path` to the imported list if not already present (exact `PathBuf` equality).
/// Returns `true` if it was added; on an add, bumps the revision and persists off-thread.
/// Returns `false` (no revision bump, no persist) when the path is already present.
pub(in crate::tabs::typing) fn add_imported_system_font(path: PathBuf) -> bool {
    {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.imported.iter().any(|existing| existing == &path) {
            return false;
        }
        guard.imported.push(path);
    }
    bump_revision();
    persist_off_thread();
    true
}

/// Removes `path` from the imported list if present. Returns `true` if it was removed;
/// on a removal, bumps the revision and persists off-thread. Returns `false` (no revision
/// bump, no persist) when the path was not present.
pub(in crate::tabs::typing) fn remove_imported_system_font(path: &Path) -> bool {
    let removed = {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let before = guard.imported.len();
        guard.imported.retain(|existing| existing.as_path() != path);
        guard.imported.len() != before
    };
    if removed {
        bump_revision();
        persist_off_thread();
    }
    removed
}

/// Returns the user display-name override for `key` (a `fonts_data::font_settings_key`),
/// or `None` when the font has no override. The override is display-only; the font's
/// render/inline-tag identity is never affected.
#[must_use]
pub(in crate::tabs::typing) fn font_display_name_override(key: &str) -> Option<String> {
    let guard = match store().read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.overrides.get(key).cloned()
}

/// Sets or clears the display-name override for `key` (a `fonts_data::font_settings_key`).
///
/// `name = None` or a blank/whitespace-only string REMOVES the override. Returns `true`
/// when the stored state actually changed; on a real change bumps the shared revision and
/// persists off-thread. A no-op (setting the same value, or clearing an absent override)
/// returns `false` without bumping the revision or persisting.
pub(in crate::tabs::typing) fn set_font_display_name_override(
    key: &str,
    name: Option<String>,
) -> bool {
    // A blank override behaves identically to "no override", so normalize it to a removal.
    let normalized = name.filter(|value| !value.trim().is_empty());
    let changed = {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match normalized {
            Some(value) => {
                if guard.overrides.get(key) == Some(&value) {
                    false
                } else {
                    guard.overrides.insert(key.to_string(), value);
                    true
                }
            }
            None => guard.overrides.remove(key).is_some(),
        }
    };
    if changed {
        bump_revision();
        persist_off_thread();
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

/// Adds font `key` (a `fonts_data::font_settings_key`) to the virtual group named EXACTLY
/// `group`. Returns `true` on a real add (then bumps the revision and persists off-thread).
/// Returns `false` when the group is unknown, `key` is blank, or the font is already a member.
pub(in crate::tabs::typing) fn add_virtual_group_member(group: &str, key: &str) -> bool {
    let key = key.trim();
    if key.is_empty() {
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
                if candidate.members.iter().any(|member| member.font == key) {
                    false
                } else {
                    candidate.members.push(fonts_data::VirtualFontGroupMember {
                        font: key.to_string(),
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

/// Removes font `key` from the virtual group named EXACTLY `group`. Returns `true` when a
/// member was removed (then bumps the revision and persists off-thread); `false` when the
/// group is unknown or the font was not a member.
pub(in crate::tabs::typing) fn remove_virtual_group_member(group: &str, key: &str) -> bool {
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
                candidate.members.retain(|member| member.font != key);
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

/// Sets or clears the per-group display alias of font `key` in the virtual group named
/// EXACTLY `group`. `alias = None` or a blank/whitespace-only string CLEARS the alias.
/// Returns `true` when the stored alias actually changed (then bumps the revision and
/// persists off-thread); `false` when the group/member is missing or the alias is unchanged.
pub(in crate::tabs::typing) fn set_virtual_group_member_alias(
    group: &str,
    key: &str,
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
            Some(candidate) => match candidate.members.iter_mut().find(|member| member.font == key) {
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

/// Seeds the runtime-global store at startup from `fonts/fonts_data.json`, migrating the
/// legacy `TextTab.imported_system_fonts` list on first run.
///
/// The load outcome decides the path:
/// - `Loaded`: use the parsed imported paths + display-name overrides.
/// - `Missing` (first run): run the one-time legacy migration.
/// - `Invalid` (corrupt file): quarantine it to `fonts_data.json.bad` and then run the
///   legacy migration, so a corrupt file is neither trusted nor silently overwritten by the
///   next mutation (which would destroy the recoverable original).
///
/// Sets the state directly WITHOUT bumping the revision or persisting via the mutators — this
/// is the initial state, not a change, so a poller must not treat startup as a mutation.
pub fn seed_imported_system_fonts_from_config() {
    let fonts_dir = resolve_fonts_dir();
    let loaded = match fonts_data::load_outcome(&fonts_dir) {
        fonts_data::LoadOutcome::Loaded(data) => data,
        fonts_data::LoadOutcome::Missing => migrate_legacy_imported_fonts(&fonts_dir),
        fonts_data::LoadOutcome::Invalid => {
            // Move the corrupt document aside before proceeding, so the first mutation's save
            // cannot overwrite (and destroy) a possibly-recoverable file.
            fonts_data::quarantine_bad_file(&fonts_dir);
            migrate_legacy_imported_fonts(&fonts_dir)
        }
    };

    let mut guard = match store().write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.imported = dedup_preserve_order(loaded.imported_system_fonts);
    guard.overrides = loaded.display_name_overrides;
    guard.virtual_groups = loaded.virtual_groups;
}

/// One-time migration of the legacy `user_config.json` imported-fonts list into a fresh
/// `fonts_data.json`. Reads the legacy list via `presets_io`; if it is non-empty it is
/// written once to `fonts_data.json` (the legacy key is left in place, it simply stops being
/// read/written). Best-effort: a save failure is logged but the returned state is still used.
fn migrate_legacy_imported_fonts(fonts_dir: &Path) -> fonts_data::FontsData {
    let legacy = dedup_preserve_order(presets_io::load_text_tab_imported_system_fonts());
    let migrated = fonts_data::FontsData {
        imported_system_fonts: legacy,
        display_name_overrides: BTreeMap::new(),
        virtual_groups: Vec::new(),
    };
    if !migrated.imported_system_fonts.is_empty()
        && let Err(err) = fonts_data::save(fonts_dir, &migrated)
    {
        crate::runtime_log::log_warn(format!(
            "typing: failed to migrate imported system fonts into fonts_data.json: {err}"
        ));
    }
    migrated
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize tests that mutate the process-global store; parallel tests would otherwise
    // race on the shared state. Revision assertions are relative (after > before), so the
    // shared monotonic counter does not make tests order-dependent.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Clears the shared state to a known-empty baseline for an isolated test. Only the
    /// state is reset; the revision counter stays monotonic (tests assert relative deltas).
    fn reset_store() {
        let mut guard = match store().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.imported.clear();
        guard.overrides.clear();
        guard.virtual_groups.clear();
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

    #[test]
    fn display_name_override_set_get_remove() {
        let _lock = lock_tests();
        reset_store();
        let key = "groups/Manga/Bold.ttf";
        assert_eq!(font_display_name_override(key), None, "no override initially");

        assert!(
            set_font_display_name_override(key, Some("Мой шрифт".to_string())),
            "first set changes state"
        );
        assert_eq!(
            font_display_name_override(key).as_deref(),
            Some("Мой шрифт")
        );

        // Setting the SAME value is a no-op.
        assert!(!set_font_display_name_override(key, Some("Мой шрифт".to_string())));

        // A blank value removes the override.
        assert!(set_font_display_name_override(key, Some("   ".to_string())));
        assert_eq!(font_display_name_override(key), None);

        // Clearing an already-absent override is a no-op.
        assert!(!set_font_display_name_override(key, None));
    }

    #[test]
    fn override_mutation_bumps_the_shared_revision() {
        let _lock = lock_tests();
        reset_store();
        let key = "A.ttf";
        let before = imported_fonts_revision();
        assert!(set_font_display_name_override(key, Some("Name".to_string())));
        assert!(
            imported_fonts_revision() > before,
            "a display-name change must bump the same revision imported-fonts uses"
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
        assert!(!add_virtual_group_member("missing", "A.ttf"), "unknown group rejected");
        assert!(!add_virtual_group_member("G", "   "), "blank key rejected");
        assert!(add_virtual_group_member("G", "A.ttf"), "first add succeeds");
        assert!(!add_virtual_group_member("G", "A.ttf"), "duplicate member rejected");
        assert!(add_virtual_group_member("G", "B.ttf"), "second member succeeds");
        let members: Vec<String> = virtual_groups()
            .into_iter()
            .flat_map(|group| group.members)
            .map(|member| member.font)
            .collect();
        assert_eq!(members, vec!["A.ttf".to_string(), "B.ttf".to_string()]);
        assert!(remove_virtual_group_member("G", "A.ttf"), "present member removed");
        assert!(!remove_virtual_group_member("G", "A.ttf"), "absent member -> false");
        assert!(!remove_virtual_group_member("missing", "B.ttf"), "unknown group -> false");
    }

    #[test]
    fn set_virtual_group_member_alias_set_clear_and_no_op() {
        let _lock = lock_tests();
        reset_store();
        assert!(create_virtual_group("G"));
        assert!(add_virtual_group_member("G", "A.ttf"));
        assert!(!set_virtual_group_member_alias("G", "missing.ttf", Some("X")), "unknown member");
        assert!(!set_virtual_group_member_alias("missing", "A.ttf", Some("X")), "unknown group");
        assert!(set_virtual_group_member_alias("G", "A.ttf", Some("  Псевдоним  ")), "set trims");
        assert_eq!(
            virtual_groups()[0].members[0].alias.as_deref(),
            Some("Псевдоним")
        );
        assert!(
            !set_virtual_group_member_alias("G", "A.ttf", Some("Псевдоним")),
            "setting the same alias is a no-op"
        );
        assert!(
            set_virtual_group_member_alias("G", "A.ttf", Some("   ")),
            "a blank alias clears it"
        );
        assert_eq!(virtual_groups()[0].members[0].alias, None);
        assert!(
            !set_virtual_group_member_alias("G", "A.ttf", None),
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
        assert!(!set_virtual_group_member_alias("G", "absent.ttf", Some("X")));
        assert_eq!(imported_fonts_revision(), after_create, "no-op alias must not bump");
    }
}
