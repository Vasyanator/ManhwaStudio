/*
File: panel/presets_store.rs

Purpose:
The SINGLE owner of `fonts/presets.json` — the create-panel preset document — and of the
one-shot migration that moved it out of `user_config.json`
(`dev-docs/font_identity_postscript_plan.md`, phase 5).

Main responsibilities:
- define the versioned schema (`version: 1`) and its serde mirror;
- load the document as a typed `LoadOutcome` (`Missing` / `Loaded` / `Invalid`) so a corrupt
  file is never silently degraded to empty (which the next save would then overwrite);
- quarantine a corrupt document to `presets.json.bad` (rename, else copy), and DISABLE saving
  for the session when neither worked — that document is then the only copy of the user's
  presets and the atomic write's final rename would destroy it;
- save a full snapshot ATOMICALLY and CRASH-DURABLY through the shared `doc_store` recipe
  (temp sibling + `write_all` + `sync_all` + rename + DIRECTORY fsync), reporting a TYPED
  error instead of swallowing it;
- guard that save with the same optimistic concurrency `fonts_data.json` uses: a document
  from a NEWER schema is never overwritten, and a document a SECOND app instance changed is
  merged in and retried once instead of being clobbered;
- read the LEGACY `user_config.TextTab.create_presets` payload for the migration and delete
  the migrated (and dead) `TextTab` keys afterwards — the imported-fonts key only once
  `fonts_data.json` demonstrably holds that list.

Key types:
- `StoredPresets` (name -> `TypingCreatePreset`, the decoded document)
- `LoadOutcome` (Missing / Loaded { presets, fingerprint } / Invalid)
- `LegacyCreatePreset` (one preset exactly as an older build stored it)
- `SaveReport` (what a successful save merged in from another app instance)
- `QuarantineOutcome` (what happened to a corrupt document: moved / copied / failed)
- `PresetsStoreError` (typed save failure: directory, serialization, write, version,
  persistence disabled, conflict)

Key functions:
- `data_path` / `load_outcome` / `quarantine_bad_file` / `set_baseline` / `next_save_ticket`
  / `save`
- `load_legacy_presets` / `drop_migrated_user_config_keys`

Notes:
`use super::*;` pulls in the parent `panel` module's imports (`fs`, `Path`, `PathBuf`,
`HashMap`, `Value`). The write recipe and the fingerprint/baseline vocabulary are NOT
duplicated here: they belong to `doc_store`, shared with `fonts_data`. The RESOLUTION of
legacy font references to identities is deliberately NOT here either: it needs the panel's
font list and lives in `create_presets::migrate_legacy_presets`.

PRESET NAMES ARE USER DATA AND ARE STORED VERBATIM. Nothing here trims, folds or otherwise
edits a name: `" Рао-кун "` and `"Рао-кун"` are two different presets, and silently
collapsing them (which trimming did) destroyed one of them without a word.
*/

use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Current on-disk schema version of `fonts/presets.json`.
pub(super) const PRESETS_VERSION: u32 = 1;

/// File name of the create-preset document inside the app fonts directory.
const PRESETS_FILE_NAME: &str = "presets.json";

/// Legacy `TextTab` key that held the whole preset map inside `user_config.json`.
/// Read once by the migration, then deleted; never written again.
const LEGACY_CREATE_PRESETS_KEY: &str = "create_presets";

/// Dead `TextTab` key: no reader exists anywhere in `src/`. Deleted together with the
/// migrated preset map so the config stops carrying it forever.
const LEGACY_USE_SYSTEM_FONTS_KEY: &str = "use_system_fonts";

/// Decoded document: preset name -> the preset itself.
pub(super) type StoredPresets = HashMap<String, TypingCreatePreset>;

/// One preset as stored on disk. Unset fields are omitted so the document stays minimal
/// (the "JSON slimming" rule of the identity plan).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PresetFileEntry {
    /// Font IDENTITY the preset selects. Empty when the preset names no font at all, or
    /// when the migration could not resolve the legacy reference — in that case the legacy
    /// string is kept VERBATIM here so the user can still repair it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    font: String,
    /// Per-font parameter overrides of THIS preset, keyed by font identity. Independent of
    /// `fonts_data.fonts.<identity>.profile`, which is the font's DEFAULT profile
    /// ("variant A" of `dev-docs/font_identity_postscript_plan.md`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    profiles: BTreeMap<String, Value>,
}

/// Serde mirror of the whole `presets.json` document. Every field has a serde default so a
/// partial or future-version document still deserializes its known keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PresetsFile {
    /// Schema version; see [`PRESETS_VERSION`]. A newer version is warned about on read and
    /// REFUSED on write (its unknown fields would be dropped).
    #[serde(default)]
    version: u32,
    /// Presets by name. `BTreeMap` so the file is byte-stable across saves.
    #[serde(default)]
    presets: BTreeMap<String, PresetFileEntry>,
}

/// Result of reading `presets.json`, mirroring `fonts_data::LoadOutcome`.
///
/// `Invalid` must NOT be collapsed into "empty": the next save would then overwrite a
/// possibly-recoverable document with an empty one.
#[derive(Debug)]
pub(super) enum LoadOutcome {
    /// No document yet (first run, or the file was never migrated out of `user_config`).
    Missing,
    /// Successfully parsed document.
    Loaded {
        /// The decoded presets.
        presets: StoredPresets,
        /// Fingerprint of the exact bytes read — this reader's optimistic-concurrency
        /// baseline for its first save.
        fingerprint: doc_store::DocumentFingerprint,
    },
    /// The file exists but could not be read or parsed.
    Invalid,
}

/// One create preset exactly as an older build stored it in
/// `user_config.TextTab.create_presets` — three competing font references plus a profile
/// map keyed by whatever string that build used (in practice an absolute FILE PATH).
///
/// Purely a transport type for the migration; nothing resolves here.
#[derive(Debug, Clone, Default)]
pub(super) struct LegacyCreatePreset {
    /// Historical primary key: the font's PATH on old data, its identity on late data.
    pub(super) primary_font_key: String,
    /// Historical companion: the primary font's file path.
    pub(super) primary_font_path: Option<String>,
    /// Historical companion: the primary font's label (a file stem on old data, the
    /// identity on late data).
    pub(super) primary_font_label: Option<String>,
    /// Per-font profiles keyed by the legacy string.
    pub(super) font_profiles: HashMap<String, Value>,
}

/// One legacy preset together with its name, as handed to the migration.
pub(super) type LegacyPresetEntry = (String, LegacyCreatePreset);

/// What a successful [`save`] had to reconcile.
#[derive(Debug, Default)]
pub(super) struct SaveReport {
    /// Presets that were on disk but NOT in the saved snapshot — another app instance wrote
    /// them. They are already part of the document that was just written; the caller adopts
    /// them into its own state so its next snapshot does not drop them again.
    pub(super) merged_from_disk: StoredPresets,
}

/// Typed failure of a `presets.json` save. Every variant names the file it was working on
/// and the OS reason, so the log line and the user-facing message carry the same facts.
#[derive(Debug)]
pub(super) enum PresetsStoreError {
    /// The fonts directory could not be created.
    CreateDir {
        /// The directory that could not be created.
        dir: PathBuf,
        /// OS reason.
        reason: String,
    },
    /// The document could not be serialized to JSON (a non-string map key or a NaN float).
    Serialize {
        /// Serializer reason.
        reason: String,
    },
    /// The existing document could not be read before replacing it, so it is not known what
    /// would be destroyed. Nothing was written.
    ReadExisting {
        /// The document that could not be read.
        path: PathBuf,
        /// OS reason.
        reason: String,
    },
    /// The atomic write itself failed; see [`doc_store::AtomicWriteError`].
    Write(doc_store::AtomicWriteError),
    /// The document on disk declares a schema version this build does not understand.
    /// Rewriting it as v1 would silently drop every field that version added.
    NewerVersion {
        /// The version the on-disk document declares.
        found: u32,
    },
    /// A corrupt document at this path could not be quarantined, so it is the only copy of
    /// the user's presets and saving is disabled for the session. Nothing was written.
    PersistenceDisabled {
        /// The corrupt document that is still the only copy.
        path: PathBuf,
    },
    /// Another app instance keeps rewriting the document (or replaced it with something
    /// unparsable): the merge-and-retry did not converge. Nothing was written.
    Conflict {
        /// The contested document.
        path: PathBuf,
        /// Whether the on-disk document could be parsed at all.
        parsable: bool,
    },
}

impl std::fmt::Display for PresetsStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateDir { dir, reason } => {
                write!(f, "cannot create fonts directory {}: {reason}", dir.display())
            }
            Self::Serialize { reason } => write!(f, "cannot serialize presets.json: {reason}"),
            Self::ReadExisting { path, reason } => write!(
                f,
                "cannot read the existing {} before replacing it: {reason}",
                path.display()
            ),
            Self::Write(err) => write!(f, "{err}"),
            Self::NewerVersion { found } => write!(
                f,
                "presets.json on disk declares version {found}, newer than the supported \
                 {PRESETS_VERSION}; refusing to overwrite it (its extra fields would be lost)"
            ),
            Self::PersistenceDisabled { path } => write!(
                f,
                "the corrupt {} could not be moved aside and is the only copy of the saved \
                 presets; saving is disabled until it is moved or deleted by hand",
                path.display()
            ),
            Self::Conflict { path, parsable } => write!(
                f,
                "{} keeps changing under us ({}); refusing to overwrite it",
                path.display(),
                if *parsable {
                    "another app instance is writing it"
                } else {
                    "and it can no longer be parsed"
                }
            ),
        }
    }
}

impl std::error::Error for PresetsStoreError {}

/// Path of the `presets.json` document inside `fonts_dir`.
#[must_use]
pub(super) fn data_path(fonts_dir: &Path) -> PathBuf {
    fonts_dir.join(PRESETS_FILE_NAME)
}

/// Reads `fonts/presets.json`, distinguishing "not there yet" from "corrupt".
pub(super) fn load_outcome(fonts_dir: &Path) -> LoadOutcome {
    load_outcome_from_file(&data_path(fonts_dir))
}

/// Path-parameterized core of [`load_outcome`], split out so the read logic can be
/// unit-tested against a temp file instead of the real fonts directory.
fn load_outcome_from_file(path: &Path) -> LoadOutcome {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        // A missing file is the normal pre-migration case; anything else is a read error.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return LoadOutcome::Missing,
        Err(err) => {
            crate::runtime_log::log_warn(format!(
                "typing: cannot read presets.json; treating as corrupt (will quarantine). \
                 Path: {} Error: {err}",
                path.display()
            ));
            return LoadOutcome::Invalid;
        }
    };
    let file: PresetsFile = match serde_json::from_str(&raw) {
        Ok(file) => file,
        Err(err) => {
            crate::runtime_log::log_warn(format!(
                "typing: malformed presets.json; treating as corrupt (will quarantine). \
                 Path: {} Error: {err}",
                path.display()
            ));
            return LoadOutcome::Invalid;
        }
    };
    if file.version > PRESETS_VERSION {
        crate::runtime_log::log_warn(format!(
            "typing: presets.json version {} is newer than the expected {PRESETS_VERSION}; \
             parsing known fields only, and this build will never overwrite it. Path: {}",
            file.version,
            path.display()
        ));
    }
    LoadOutcome::Loaded {
        presets: decode(file),
        fingerprint: doc_store::fingerprint(&raw),
    }
}

/// What happened to a corrupt `presets.json` that [`quarantine_bad_file`] tried to move
/// aside. Mirrors `fonts_data::QuarantineOutcome`, because the two documents run the same
/// risk: the corrupt file may be the only copy of what the user had.
#[derive(Debug)]
pub(super) enum QuarantineOutcome {
    /// The corrupt document was RENAMED to `presets.json.bad`; the original path is free and
    /// the next save may create a fresh document there.
    Moved,
    /// The rename failed, but a COPY reached `presets.json.bad`. The content is preserved, so
    /// overwriting the original is safe.
    Copied,
    /// Neither the rename nor the copy worked: the corrupt document is the ONLY copy of the
    /// user's presets, so persistence for this file is DISABLED for the session (see
    /// [`quarantine_bad_file`]).
    ///
    /// Carries no reasons: both OS errors are already logged by `quarantine_bad_file`, and
    /// the caller's only remaining decision is the concurrency baseline, which does not
    /// depend on WHY the quarantine failed.
    Failed,
}

/// Moves a corrupt `presets.json` aside to `presets.json.bad` so the next save cannot destroy
/// a recoverable document: `rename`, else `copy`, else [`QuarantineOutcome::Failed`].
///
/// SIDE EFFECT ON FAILURE, deliberately kept here rather than left to the caller: when
/// neither step worked, this file is the only surviving copy of the user's presets, so
/// persistence for `fonts_dir/presets.json` is switched OFF for the rest of the session
/// ([`save`] then returns [`PresetsStoreError::PersistenceDisabled`] without touching the
/// bytes). A caller that forgot to do this would have the next save rename its snapshot over
/// the only copy — which is exactly what used to happen.
///
/// Every outcome is logged; nothing is propagated as an error, because the caller's job here
/// is only to pick the concurrency baseline for the next write.
pub(super) fn quarantine_bad_file(fonts_dir: &Path) -> QuarantineOutcome {
    let path = data_path(fonts_dir);
    let bad = path.with_extension("json.bad");
    let rename_error = match fs::rename(&path, &bad) {
        Ok(()) => {
            crate::runtime_log::log_warn(format!(
                "typing: quarantined corrupt presets.json to {}",
                bad.display()
            ));
            return QuarantineOutcome::Moved;
        }
        Err(err) => err.to_string(),
    };
    // The rename can fail while the bytes are perfectly readable (a cross-device `.bad`
    // target, a read-only directory entry, a Windows share lock). A copy is enough: a second,
    // recoverable copy exists, which is the entire point of the quarantine.
    match fs::copy(&path, &bad) {
        Ok(_) => {
            crate::runtime_log::log_warn(format!(
                "typing: could not RENAME the corrupt presets.json ({rename_error}); copied it \
                 to {} instead, so the original may be overwritten safely. Path: {}",
                bad.display(),
                path.display()
            ));
            QuarantineOutcome::Copied
        }
        Err(err) => {
            block_persistence(fonts_dir);
            crate::runtime_log::log_error(format!(
                "typing: could not quarantine the corrupt presets.json — neither rename \
                 ({rename_error}) nor copy ({err}) worked. It is the only copy of the saved \
                 create presets, so saving presets is DISABLED for this session; move or \
                 delete {} by hand to re-enable it.",
                path.display()
            ));
            QuarantineOutcome::Failed
        }
    }
}

/// Converts the serde mirror into the decoded runtime form.
///
/// Names are taken VERBATIM (see the file header). Only a completely empty name is dropped:
/// it can address no preset in the combo and the user cannot have typed it.
fn decode(file: PresetsFile) -> StoredPresets {
    file.presets
        .into_iter()
        .filter(|(name, _)| !name.is_empty())
        .map(|(name, entry)| {
            (
                name,
                TypingCreatePreset {
                    font: entry.font,
                    font_profiles: entry.profiles.into_iter().collect(),
                },
            )
        })
        .collect()
}

/// Converts the decoded runtime form into the serde mirror, stamping the current version.
/// Names are written VERBATIM; a preset with a completely empty name is dropped.
fn encode(presets: &StoredPresets) -> PresetsFile {
    PresetsFile {
        version: PRESETS_VERSION,
        presets: presets
            .iter()
            .filter(|(name, _)| !name.is_empty())
            .map(|(name, preset)| {
                (
                    name.clone(),
                    PresetFileEntry {
                        font: preset.font.clone(),
                        profiles: preset
                            .font_profiles
                            .iter()
                            .map(|(key, profile)| (key.clone(), profile.clone()))
                            .collect(),
                    },
                )
            })
            .collect(),
    }
}

/// Per-target-file writer state: the newest snapshot already written and what this process
/// believes is on disk.
#[derive(Debug, Default)]
struct TargetState {
    /// Highest [`next_save_ticket`] value already written to this path.
    ticket: u64,
    /// Optimistic-concurrency expectation for the next write to this path.
    baseline: doc_store::SaveBaseline,
    /// `true` once a corrupt document at this path could NOT be quarantined; while it holds,
    /// every [`save`] to this path is refused (see [`block_persistence`]).
    blocked: bool,
}

/// Serializes every `presets.json` writer in this process AND remembers, PER TARGET FILE,
/// the highest ticket already written plus the concurrency baseline.
///
/// Two writers would otherwise create, truncate and rename the SAME per-process temp file
/// concurrently and could leave a half-written document renamed over the real one (mirrors
/// `font_settings_store`'s save lock). The per-path ticket is what additionally keeps a slow
/// writer from putting an older snapshot back over a newer one; the per-path baseline is
/// what keeps a SECOND RUNNING APP INSTANCE from being clobbered. Both are keyed by path
/// rather than global so two different documents (production has one, tests have many)
/// never supersede each other.
fn save_state() -> &'static std::sync::Mutex<HashMap<PathBuf, TargetState>> {
    static SAVE_STATE: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, TargetState>>> =
        std::sync::OnceLock::new();
    SAVE_STATE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Ticket dispenser ordering concurrent saves; see [`next_save_ticket`].
static SAVE_TICKETS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Takes the ticket for one save, in the order the SNAPSHOTS were made.
///
/// Callers snapshot the presets on the GUI thread and hand the work to a worker, so the order
/// in which workers reach the file is not the order of the edits. The ticket restores it:
/// [`save`] discards a snapshot older than what is already on disk.
#[must_use]
pub(super) fn next_save_ticket() -> u64 {
    SAVE_TICKETS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Records what this process now believes is on disk at `fonts_dir/presets.json`.
///
/// Called by the seeding read: the bytes just read ARE the baseline of the first save, so a
/// document another instance writes in the meantime is detected instead of overwritten.
pub(super) fn set_baseline(fonts_dir: &Path, baseline: doc_store::SaveBaseline) {
    let path = data_path(fonts_dir);
    let mut state = save_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.entry(path).or_default().baseline = baseline;
}

/// Refuses every further [`save`] to `fonts_dir/presets.json` in this process.
///
/// Set exactly once, by [`quarantine_bad_file`] when a corrupt document could be neither
/// renamed nor copied aside: that document is then the only copy of the user's presets, and
/// the atomic write's final `rename` would destroy it. There is no way to clear it short of
/// restarting the app after moving the file by hand — which is what the error message says.
fn block_persistence(fonts_dir: &Path) {
    let path = data_path(fonts_dir);
    let mut state = save_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.entry(path).or_default().blocked = true;
}

/// Atomically writes a full snapshot of `presets` to `fonts/presets.json`, creating the
/// fonts directory if it does not yet exist.
///
/// `ticket` comes from [`next_save_ticket`], taken where the snapshot was made: a snapshot
/// older than the one already written is silently skipped (reported as an empty `Ok`,
/// because nothing is wrong and nothing was lost — a newer state of the same data is on
/// disk).
///
/// CONCURRENCY. The write is guarded by the document's own state, exactly as
/// `fonts_data::save_checked` guards its own: a NEWER schema version is refused, and a
/// document that changed since this process's baseline (a second running app instance wrote
/// it) is PARSED, MERGED INTO the snapshot — additively: theirs is added, ours is kept — and
/// the write is retried once. What was merged in comes back in [`SaveReport`] so the caller
/// can adopt it; otherwise its next snapshot would drop those presets again.
///
/// DURABILITY. The containing directory is fsynced before this returns
/// ([`doc_store::Durability::ContentsAndDirectory`]), because the caller DELETES the presets
/// from `user_config.json` once this succeeded: without the directory flush a power loss in
/// that window could leave neither document.
///
/// # Errors
/// Returns a [`PresetsStoreError`] on directory creation, serialization, read-back, write,
/// version or conflict failure. The previous document is left untouched in every failure
/// case. Callers must run this off the GUI thread and must SURFACE the error — the preset
/// the user just saved is otherwise lost without a word.
pub(super) fn save(
    fonts_dir: &Path,
    presets: &StoredPresets,
    ticket: u64,
) -> Result<SaveReport, PresetsStoreError> {
    let path = data_path(fonts_dir);
    // A poisoned lock still guards the same section; recover rather than panic. Held across
    // the whole write, so two writers cannot share the temp file.
    let mut state = save_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let target = state.entry(path.clone()).or_default();
    // A corrupt document that could not be moved aside is the ONLY copy of the user's
    // presets; the atomic write ends in a `rename` over it, so nothing may be written at all.
    if target.blocked {
        return Err(PresetsStoreError::PersistenceDisabled { path });
    }
    if ticket <= target.ticket {
        return Ok(SaveReport::default());
    }
    if let Err(err) = fs::create_dir_all(fonts_dir) {
        return Err(PresetsStoreError::CreateDir {
            dir: fonts_dir.to_path_buf(),
            reason: err.to_string(),
        });
    }

    let mut snapshot = presets.clone();
    let mut report = SaveReport::default();
    // Two attempts: the first may discover another instance's document, which is merged in;
    // the second writes the merged result. A conflict on the retry means the other instance
    // is writing continuously — reported rather than fought over.
    for attempt in 0..2 {
        match inspect_existing(&path, target.baseline)? {
            ExistingState::Replaceable => {}
            ExistingState::Conflict { disk, fingerprint } => {
                let parsable = disk.is_some();
                // Only the FIRST attempt merges: a second conflict means the other instance
                // is writing continuously, and retrying forever would be a livelock.
                let (Some(disk), 0) = (disk, attempt) else {
                    return Err(PresetsStoreError::Conflict { path, parsable });
                };
                for (name, preset) in disk {
                    // Ours wins a name clash: this snapshot is what the user has on screen.
                    if !snapshot.contains_key(&name) {
                        snapshot.insert(name.clone(), preset.clone());
                        report.merged_from_disk.insert(name, preset);
                    }
                }
                crate::runtime_log::log_info(format!(
                    "typing presets: {} changed under us (another app instance); merged {} \
                     preset(s) from disk and retrying the save.",
                    path.display(),
                    report.merged_from_disk.len()
                ));
                target.baseline = doc_store::SaveBaseline::Matching(fingerprint);
                continue;
            }
        }
        let fingerprint = write_document(&path, &snapshot)?;
        target.baseline = doc_store::SaveBaseline::Matching(fingerprint);
        target.ticket = ticket;
        return Ok(report);
    }
    Err(PresetsStoreError::Conflict {
        path,
        parsable: true,
    })
}

/// What [`inspect_existing`] found in front of a pending write.
#[derive(Debug)]
enum ExistingState {
    /// Nothing is there, or what is there is exactly what the caller expected.
    Replaceable,
    /// The document changed since the caller's baseline.
    Conflict {
        /// The freshly parsed on-disk document, or `None` when it cannot be parsed at all
        /// (then it must not be overwritten: it is the only copy of whatever it holds).
        disk: Option<StoredPresets>,
        /// Fingerprint of the on-disk bytes — the caller's new baseline once merged.
        fingerprint: doc_store::DocumentFingerprint,
    },
}

/// Inspects the document currently at `path` and decides whether it may be replaced.
///
/// Mirrors `fonts_data::guard_existing_document`: a FUTURE schema version is refused
/// outright (this build cannot round-trip its unknown fields), and a document that no longer
/// matches `baseline` is reported as a conflict together with its parsed content so the
/// caller can merge rather than clobber. A file that is ABSENT never blocks a write.
///
/// # Errors
/// [`PresetsStoreError::ReadExisting`] when the file exists but cannot be read, and
/// [`PresetsStoreError::NewerVersion`] for a future schema.
fn inspect_existing(
    path: &Path,
    baseline: doc_store::SaveBaseline,
) -> Result<ExistingState, PresetsStoreError> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        // Nothing on disk: any baseline may proceed (a `Matching` baseline whose file
        // vanished has nothing left to preserve).
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ExistingState::Replaceable);
        }
        Err(err) => {
            return Err(PresetsStoreError::ReadExisting {
                path: path.to_path_buf(),
                reason: err.to_string(),
            });
        }
    };
    let parsed: Option<PresetsFile> = serde_json::from_str(&raw).ok();
    if let Some(found) = parsed
        .as_ref()
        .map(|file| file.version)
        .filter(|version| *version > PRESETS_VERSION)
    {
        return Err(PresetsStoreError::NewerVersion { found });
    }
    let fingerprint = doc_store::fingerprint(&raw);
    if baseline.accepts(fingerprint) {
        return Ok(ExistingState::Replaceable);
    }
    Ok(ExistingState::Conflict {
        disk: parsed.map(decode),
        fingerprint,
    })
}

/// Serializes `presets` and writes them over `path` durably. Returns the fingerprint of the
/// bytes just written — the caller's new baseline.
fn write_document(
    path: &Path,
    presets: &StoredPresets,
) -> Result<doc_store::DocumentFingerprint, PresetsStoreError> {
    let file = encode(presets);
    let mut text =
        serde_json::to_string_pretty(&file).map_err(|err| PresetsStoreError::Serialize {
            reason: err.to_string(),
        })?;
    text.push('\n');
    let fingerprint = doc_store::fingerprint(&text);
    doc_store::write_atomic(path, &text, doc_store::Durability::ContentsAndDirectory)
        .map_err(PresetsStoreError::Write)?;
    Ok(fingerprint)
}

/// Reads the LEGACY `user_config.TextTab.create_presets` map, if any.
///
/// Returns the presets exactly as stored, with no resolution whatsoever — re-keying the
/// font references needs the panel's font list and happens in
/// `create_presets::migrate_legacy_presets`. An unreadable or malformed config yields an
/// empty vector: there is then simply nothing to migrate, and the file is never rewritten
/// on that path, so a malformed config cannot be destroyed by the migration.
///
/// Names are taken VERBATIM, like everywhere else here. Entries are returned NAME-SORTED so
/// the migration log and the produced document are reproducible.
#[must_use]
pub(super) fn load_legacy_presets() -> Vec<LegacyPresetEntry> {
    load_legacy_presets_from(&config::user_config_path())
}

/// Path-parameterized core of [`load_legacy_presets`], split out so the migration can be
/// tested against a temp config instead of the real `user_config.json`.
#[must_use]
fn load_legacy_presets_from(user_settings_file: &Path) -> Vec<LegacyPresetEntry> {
    let Ok(raw) = fs::read_to_string(user_settings_file) else {
        return Vec::new();
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let Some(presets_obj) = payload
        .get("TextTab")
        .and_then(Value::as_object)
        .and_then(|text_tab| text_tab.get(LEGACY_CREATE_PRESETS_KEY))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    let mut out: Vec<LegacyPresetEntry> = presets_obj
        .iter()
        .filter_map(|(name, raw_preset)| {
            let obj = raw_preset.as_object()?;
            let read_str = |key: &str| {
                obj.get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
            };
            let font_profiles = obj
                .get("font_profiles")
                .and_then(Value::as_object)
                .map(|profiles| {
                    profiles
                        .iter()
                        .map(|(font_key, profile)| (font_key.clone(), profile.clone()))
                        .collect::<HashMap<String, Value>>()
                })
                .unwrap_or_default();
            Some((
                name.clone(),
                LegacyCreatePreset {
                    primary_font_key: read_str("primary_font_key").unwrap_or_default(),
                    primary_font_path: read_str("primary_font_path"),
                    primary_font_label: read_str("primary_font_label"),
                    font_profiles,
                },
            ))
        })
        .filter(|(name, _)| !name.is_empty())
        .collect();
    out.sort_by(|(left, _), (right, _)| left.cmp(right));
    out
}

/// Deletes the `TextTab` keys this phase made obsolete, after a SUCCESSFUL migration:
/// `create_presets` (now `fonts/presets.json`), the dead `use_system_fonts`, and — only when
/// the legacy `imported_system_fonts` list is demonstrably taken over by `fonts_data.json` —
/// that list too.
///
/// Both paths are explicit so the whole decision can be tested against temp files; the
/// production caller passes `config::user_config_path()`.
///
/// Returns the keys actually removed (empty when there was nothing to do, in which case the
/// config file is NOT rewritten).
///
/// # Errors
/// Returns a human-readable error string when the config cannot be read, parsed or written;
/// malformed existing JSON is reported and never overwritten
/// (`config::update_user_config_file`).
pub(super) fn drop_migrated_user_config_keys(
    fonts_dir: &Path,
    user_settings_file: &Path,
) -> Result<Vec<String>, String> {
    let mut doomed: Vec<&str> = vec![LEGACY_CREATE_PRESETS_KEY, LEGACY_USE_SYSTEM_FONTS_KEY];
    if legacy_imports_are_taken_over(fonts_dir, user_settings_file) {
        doomed.push(crate::config::TEXT_TAB_IMPORTED_SYSTEM_FONTS_KEY);
    }
    // Nothing to remove -> do not rewrite the file at all. Checked BEFORE taking the
    // config transaction so a launch with an already-clean config touches no disk.
    let present = present_text_tab_keys(user_settings_file, &doomed);
    if present.is_empty() {
        return Ok(Vec::new());
    }
    let removed = present.clone();
    config::update_user_config_file(user_settings_file, move |root| {
        let Some(text_tab) = root
            .as_object_mut()
            .and_then(|root_obj| root_obj.get_mut("TextTab"))
            .and_then(Value::as_object_mut)
        else {
            // The section vanished between the probe and the transaction: nothing to do.
            return Ok(());
        };
        for key in &present {
            text_tab.remove(key.as_str());
        }
        Ok(())
    })
    .map_err(|err| err.to_string())?;
    Ok(removed)
}

/// Whether the legacy `TextTab.imported_system_fonts` list may be deleted, i.e. whether
/// `fonts/fonts_data.json` demonstrably CONTAINS it.
///
/// THE EXISTENCE OF `fonts_data.json` IS NOT THAT PROOF, which is what this used to check.
/// A valid but EMPTY document (`{"version":2,"system_fonts":[]}` — a hand edit, a restore
/// from a machine without imported fonts, a half-finished first run) makes
/// `font_settings_store::seed_from_fonts_dir` take its `Loaded` branch, so the legacy list is
/// never consumed by `migrate_legacy_imported_fonts`; deleting the key then loses every
/// imported font the user had, from BOTH stores at once.
///
/// The evidence used instead is the CONTENT: every legacy path must be the `last_path` hint
/// of some entry in the document. That is exactly what a completed migration produces, and
/// it needs no second persistence mechanism beside `fonts_data`'s own document. The accepted
/// FALSE NEGATIVE is a user who imported fonts and then removed them all again: the legacy
/// key is then kept forever (harmless — nothing reads it while `fonts_data.json` exists),
/// and the alternative would be deleting fonts that were never migrated.
///
/// An empty (or absent) legacy list is trivially taken over: there is nothing to lose.
#[must_use]
fn legacy_imports_are_taken_over(fonts_dir: &Path, user_settings_file: &Path) -> bool {
    let legacy = load_imported_system_fonts_from(user_settings_file);
    if legacy.is_empty() {
        return true;
    }
    let fonts_data::LoadOutcome::Loaded { data, .. } = fonts_data::load_outcome(fonts_dir) else {
        // Missing or corrupt: nothing has taken the list over, and a corrupt document may
        // still be quarantined and rebuilt FROM this very list.
        return false;
    };
    legacy.iter().all(|path| {
        data.system_fonts
            .iter()
            .any(|entry| entry.last_path.as_deref() == Some(path.as_path()))
    })
}

/// Which of `keys` are currently present under `TextTab` in the config at
/// `user_settings_file`. A missing or malformed file yields an empty list.
#[must_use]
fn present_text_tab_keys(user_settings_file: &Path, keys: &[&str]) -> Vec<String> {
    let Ok(raw) = fs::read_to_string(user_settings_file) else {
        return Vec::new();
    };
    let Ok(payload) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let Some(text_tab) = payload.get("TextTab").and_then(Value::as_object) else {
        return Vec::new();
    };
    keys.iter()
        .filter(|key| text_tab.contains_key(**key))
        .map(|key| (*key).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Unique temp directory so parallel tests never share a file and the user's real
    /// `fonts/` and `user_config.json` are never touched.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("ms_presets_{tag}_{nanos}"));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    /// Unwraps a `Loaded` outcome or panics naming the actual variant.
    fn expect_loaded(outcome: LoadOutcome) -> StoredPresets {
        match outcome {
            LoadOutcome::Loaded { presets, .. } => presets,
            LoadOutcome::Missing => panic!("expected Loaded, got Missing"),
            LoadOutcome::Invalid => panic!("expected Loaded, got Invalid"),
        }
    }

    fn preset(font: &str, profiles: &[(&str, Value)]) -> TypingCreatePreset {
        TypingCreatePreset {
            font: font.to_string(),
            font_profiles: profiles
                .iter()
                .map(|(key, value)| ((*key).to_string(), value.clone()))
                .collect(),
        }
    }

    /// The document round-trips: one font key per preset, per-preset profiles preserved.
    #[test]
    fn presets_round_trip_through_the_file() {
        let dir = unique_temp_dir("round_trip");
        let mut presets = StoredPresets::new();
        presets.insert(
            "ВВД".to_string(),
            preset(
                "d_CCShoutOut",
                &[("d_CCShoutOut", json!({"schema": 2, "font_size_px": 42.0}))],
            ),
        );
        presets.insert("Пустой".to_string(), preset("", &[]));
        save(&dir, &presets, next_save_ticket()).expect("save presets");

        let loaded = expect_loaded(load_outcome(&dir));
        assert_eq!(loaded.len(), 2);
        let vvd = loaded.get("ВВД").expect("preset ВВД survives");
        assert_eq!(vvd.font, "d_CCShoutOut");
        assert_eq!(
            vvd.font_profiles.get("d_CCShoutOut"),
            Some(&json!({"schema": 2, "font_size_px": 42.0}))
        );
        assert_eq!(loaded.get("Пустой").map(|p| p.font.as_str()), Some(""));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A preset name is USER DATA: it is stored byte for byte, and two names that differ
    /// only in surrounding spaces are two presets. Trimming them (which the store did)
    /// silently merged them and destroyed one — defect 3 of the phase-5 review.
    #[test]
    fn preset_names_are_stored_verbatim_and_never_collapsed() {
        let dir = unique_temp_dir("verbatim_names");
        let mut presets = StoredPresets::new();
        presets.insert(" Рао-кун ".to_string(), preset("Alpha-Regular", &[]));
        presets.insert("Рао-кун".to_string(), preset("Beta-Regular", &[]));
        save(&dir, &presets, next_save_ticket()).expect("save presets");

        let raw = fs::read_to_string(data_path(&dir)).expect("read written file");
        let value: Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(
            value.pointer("/presets/ Рао-кун /font"),
            Some(&json!("Alpha-Regular")),
            "the padded name must be written exactly as the user typed it"
        );
        assert_eq!(
            value.pointer("/presets/Рао-кун/font"),
            Some(&json!("Beta-Regular"))
        );

        let loaded = expect_loaded(load_outcome(&dir));
        assert_eq!(loaded.len(), 2, "both presets must survive a round trip");
        assert_eq!(
            loaded.get(" Рао-кун ").map(|p| p.font.as_str()),
            Some("Alpha-Regular")
        );
        assert_eq!(
            loaded.get("Рао-кун").map(|p| p.font.as_str()),
            Some("Beta-Regular")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The written document carries exactly ONE font key per preset (the identity), and
    /// omits everything unset. The three legacy `primary_font_*` keys are gone for good.
    #[test]
    fn written_document_is_version_one_with_a_single_font_key() {
        let dir = unique_temp_dir("shape");
        let mut presets = StoredPresets::new();
        presets.insert("A".to_string(), preset("Some-Font", &[]));
        save(&dir, &presets, next_save_ticket()).expect("save presets");

        let raw = fs::read_to_string(data_path(&dir)).expect("read written file");
        let value: Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(value.pointer("/version"), Some(&json!(1)));
        assert_eq!(value.pointer("/presets/A/font"), Some(&json!("Some-Font")));
        let entry = value
            .pointer("/presets/A")
            .and_then(Value::as_object)
            .expect("preset object");
        // `profiles` is omitted when empty; no legacy key is ever written.
        assert_eq!(entry.keys().collect::<Vec<_>>(), vec!["font"]);
        for legacy in [
            "primary_font_key",
            "primary_font_path",
            "primary_font_label",
        ] {
            assert!(!entry.contains_key(legacy), "'{legacy}' must not be written");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// The save is CRASH-DURABLE, not merely atomic: the containing directory is fsynced
    /// before it returns, because the caller deletes the presets from `user_config.json`
    /// right afterwards (defect 1 of the phase-5 review).
    #[test]
    fn a_successful_save_makes_the_directory_entry_durable() {
        let dir = unique_temp_dir("durable");
        save(&dir, &StoredPresets::new(), next_save_ticket()).expect("save presets");
        let steps = doc_store::recorded_steps(&data_path(&dir));
        assert_eq!(
            steps,
            vec![
                doc_store::WriteStep::Renamed,
                doc_store::WriteStep::DirectoryDurable
            ],
            "presets.json must be durable before the legacy source may be deleted"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A corrupt document is neither trusted nor silently emptied: it reports `Invalid`,
    /// which the caller turns into a quarantine.
    #[test]
    fn a_corrupt_document_is_invalid_and_quarantinable() {
        let dir = unique_temp_dir("corrupt");
        fs::write(data_path(&dir), "{ not json").expect("seed corrupt file");
        assert!(matches!(load_outcome(&dir), LoadOutcome::Invalid));
        assert!(
            matches!(quarantine_bad_file(&dir), QuarantineOutcome::Moved),
            "the corrupt file must be moved"
        );
        assert!(!data_path(&dir).exists(), "the corrupt file is moved aside");
        assert!(dir.join("presets.json.bad").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A corrupt document that could be neither renamed nor copied aside is the ONLY copy of
    /// the user's presets, so persistence is disabled for the session instead of the next
    /// save renaming its snapshot over it.
    ///
    /// The quarantine is made to fail by putting a non-empty DIRECTORY where
    /// `presets.json.bad` would go: both `fs::rename` and `fs::copy` refuse a directory
    /// target on every supported platform.
    #[test]
    fn a_failed_quarantine_disables_saving_instead_of_destroying_the_only_copy() {
        let dir = unique_temp_dir("quarantine_failed");
        let corrupt = "{ not json — the user's only presets";
        fs::write(data_path(&dir), corrupt).expect("seed corrupt file");
        let blocker = dir.join("presets.json.bad");
        fs::create_dir_all(&blocker).expect("seed blocking directory");
        fs::write(blocker.join("occupant"), "in the way").expect("seed blocker child");

        assert!(matches!(load_outcome(&dir), LoadOutcome::Invalid));
        assert!(
            matches!(quarantine_bad_file(&dir), QuarantineOutcome::Failed),
            "neither rename nor copy can target a non-empty directory"
        );

        let mut presets = StoredPresets::new();
        presets.insert("Новый".to_string(), preset("Alpha-Regular", &[]));
        let err = save(&dir, &presets, next_save_ticket()).expect_err("the save must be refused");
        assert!(
            matches!(err, PresetsStoreError::PersistenceDisabled { .. }),
            "unexpected variant: {err:?}"
        );
        assert!(err.to_string().contains(&data_path(&dir).display().to_string()));
        assert_eq!(
            fs::read_to_string(data_path(&dir)).expect("the corrupt file must still be there"),
            corrupt,
            "the only copy of the user's presets must survive untouched"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A save into an unwritable location REPORTS the failure instead of losing it — the
    /// defect this phase closes (`let _ = save_text_tab_create_presets(..)`).
    #[test]
    fn a_failed_save_returns_a_typed_error() {
        let dir = unique_temp_dir("save_error");
        // A regular FILE where the fonts directory is expected: `create_dir_all` fails.
        let blocked = dir.join("blocked");
        fs::write(&blocked, "not a directory").expect("seed blocker file");
        let err =
            save(&blocked, &StoredPresets::new(), next_save_ticket()).expect_err("save must fail");
        assert!(
            matches!(err, PresetsStoreError::CreateDir { .. }),
            "unexpected variant: {err:?}"
        );
        // The message names the directory, so the log and the user message agree.
        assert!(err.to_string().contains(&blocked.display().to_string()));
        let _ = fs::remove_dir_all(&dir);
    }

    /// Concurrent writers are serialized AND ordered: a snapshot older than the one already
    /// on disk is skipped instead of resurrecting stale presets over newer ones. (Without the
    /// lock the two writers would also fight over the same per-process temp file.)
    #[test]
    fn an_older_snapshot_never_overwrites_a_newer_one() {
        let dir = unique_temp_dir("save_order");
        let mut older = StoredPresets::new();
        older.insert("A".to_string(), preset("Old-Font", &[]));
        let mut newer = StoredPresets::new();
        newer.insert("A".to_string(), preset("New-Font", &[]));
        // Tickets are taken in snapshot order; the writers reach the file out of order.
        let older_ticket = next_save_ticket();
        let newer_ticket = next_save_ticket();
        save(&dir, &newer, newer_ticket).expect("newer save");
        save(&dir, &older, older_ticket).expect("older save is a no-op, not an error");

        let loaded = expect_loaded(load_outcome(&dir));
        assert_eq!(
            loaded.get("A").map(|preset| preset.font.as_str()),
            Some("New-Font")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// TWO APP INSTANCES. The second instance's presets are merged into the document
    /// instead of being clobbered, and the merge is reported so the saving panel can adopt
    /// them (defect 5 of the phase-5 review: the store had process-local statics only).
    #[test]
    fn a_second_app_instance_is_merged_instead_of_overwritten() {
        let dir = unique_temp_dir("two_instances");
        // Instance A saves and remembers its baseline.
        let mut ours = StoredPresets::new();
        ours.insert("Наш".to_string(), preset("Alpha-Regular", &[]));
        save(&dir, &ours, next_save_ticket()).expect("first save");

        // Instance B (a different process) writes a document A has never seen.
        let mut theirs = StoredPresets::new();
        theirs.insert("Наш".to_string(), preset("Alpha-Regular", &[]));
        theirs.insert("Чужой".to_string(), preset("Beta-Regular", &[]));
        let raw = serde_json::to_string_pretty(&encode(&theirs)).expect("serialize");
        fs::write(data_path(&dir), format!("{raw}\n")).expect("other instance writes");

        // Instance A saves again: the conflict is detected, merged and retried.
        ours.insert("Ещё наш".to_string(), preset("Gamma-Regular", &[]));
        let report = save(&dir, &ours, next_save_ticket()).expect("merged save");
        assert_eq!(
            report.merged_from_disk.keys().collect::<Vec<_>>(),
            vec!["Чужой"],
            "what the other instance added must be reported back to the panel"
        );

        let loaded = expect_loaded(load_outcome(&dir));
        let mut names: Vec<&str> = loaded.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["Ещё наш", "Наш", "Чужой"]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A document from a FUTURE schema is never overwritten: its unknown fields cannot be
    /// round-tripped by this build, so refusing is the only outcome that cannot corrupt it.
    #[test]
    fn a_newer_document_is_refused_rather_than_downgraded() {
        let dir = unique_temp_dir("newer");
        fs::write(
            data_path(&dir),
            json!({"version": 99, "presets": {}, "future": true}).to_string(),
        )
        .expect("seed newer document");
        let err = save(&dir, &StoredPresets::new(), next_save_ticket())
            .expect_err("a newer document must not be replaced");
        assert!(
            matches!(err, PresetsStoreError::NewerVersion { found: 99 }),
            "unexpected variant: {err:?}"
        );
        let raw = fs::read_to_string(data_path(&dir)).expect("read back");
        assert!(raw.contains("\"future\""), "the newer document is intact");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The legacy reader takes the payload verbatim, in the REAL user shape: absolute
    /// path keys, some of them outside the project.
    #[test]
    fn legacy_presets_are_read_verbatim_and_name_sorted() {
        let dir = unique_temp_dir("legacy_read");
        let config = dir.join("user_config.json");
        let seed = json!({
            "TextTab": {
                "create_presets": {
                    "звук": {
                        "primary_font_key": "/home/u/Desktop/MangaFucker/fonts/звук.otf",
                        "primary_font_path": "/home/u/Desktop/MangaFucker/fonts/звук.otf",
                        "primary_font_label": "звук",
                        "font_profiles": {
                            "/home/u/Desktop/MangaFucker/fonts/Дёрганный.ttf": {"text_params": {}}
                        }
                    },
                    "ВВД": {
                        "primary_font_key": "/proj/fonts/groups/ВВД/Крик.ttf",
                        "primary_font_label": "Крик",
                        "font_profiles": {}
                    },
                    " ВВД ": {
                        "primary_font_key": "/proj/fonts/groups/ВВД/Крик.ttf",
                        "font_profiles": {}
                    }
                }
            }
        });
        fs::write(&config, serde_json::to_string(&seed).expect("serialize seed"))
            .expect("write seed");

        let legacy = load_legacy_presets_from(&config);
        assert_eq!(
            legacy
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec![" ВВД ", "ВВД", "звук"],
            "entries are name-sorted, and a padded name is a DIFFERENT preset"
        );
        let sound = &legacy[2].1;
        assert_eq!(
            sound.primary_font_path.as_deref(),
            Some("/home/u/Desktop/MangaFucker/fonts/звук.otf")
        );
        assert_eq!(sound.primary_font_label.as_deref(), Some("звук"));
        assert_eq!(sound.font_profiles.len(), 1);
        assert!(
            sound
                .font_profiles
                .contains_key("/home/u/Desktop/MangaFucker/fonts/Дёрганный.ttf"),
            "an out-of-project path key is kept verbatim by the reader"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Writes a `fonts_data.json` v2 document listing `paths` as imported system fonts.
    fn seed_fonts_data(fonts_dir: &Path, paths: &[&str]) {
        let entries: Vec<Value> = paths
            .iter()
            .map(|path| json!({ "font": "", "last_path": path }))
            .collect();
        fs::write(
            fonts_data::data_path(fonts_dir),
            json!({ "version": 2, "system_fonts": entries }).to_string(),
        )
        .expect("seed fonts_data.json");
    }

    /// After a successful migration the three obsolete keys leave `user_config.json`, and
    /// nothing else in the file is touched.
    #[test]
    fn migrated_keys_are_deleted_and_the_rest_of_the_config_survives() {
        let dir = unique_temp_dir("legacy_drop");
        let config = dir.join("user_config.json");
        let seed = json!({
            "General": {"theme": "dark"},
            "TextTab": {
                "create_presets": {"A": {}},
                "use_system_fonts": true,
                "imported_system_fonts": ["/fonts/X.ttf"],
                "formula_presets": {"Волна": {}},
                "effect_defaults": {"stroke": {}}
            }
        });
        fs::write(&config, serde_json::to_string(&seed).expect("serialize seed"))
            .expect("write seed");
        // The imported font IS in `fonts_data.json`: the list has been taken over.
        seed_fonts_data(&dir, &["/fonts/X.ttf"]);

        let removed =
            drop_migrated_user_config_keys(&dir, &config).expect("cleanup must succeed");
        assert_eq!(removed.len(), 3, "all three legacy keys were present");
        let after: Value = serde_json::from_str(&fs::read_to_string(&config).expect("read config"))
            .expect("valid JSON");
        let text_tab = after
            .get("TextTab")
            .and_then(Value::as_object)
            .expect("TextTab survives");
        for gone in ["create_presets", "use_system_fonts", "imported_system_fonts"] {
            assert!(!text_tab.contains_key(gone), "'{gone}' must be deleted");
        }
        // What still belongs to `user_config` stays.
        assert!(text_tab.contains_key("formula_presets"));
        assert!(text_tab.contains_key("effect_defaults"));
        assert_eq!(after.pointer("/General/theme"), Some(&json!("dark")));

        // Idempotent: a second run finds nothing and rewrites nothing.
        let before = fs::read_to_string(&config).expect("read config");
        assert!(
            drop_migrated_user_config_keys(&dir, &config)
                .expect("second cleanup")
                .is_empty()
        );
        assert_eq!(fs::read_to_string(&config).expect("read config"), before);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The imported-fonts key is kept while `fonts_data.json` does not exist yet, so the
    /// user's imported system fonts cannot be lost before they have been migrated.
    #[test]
    fn imported_system_fonts_key_survives_until_fonts_data_exists() {
        let dir = unique_temp_dir("legacy_gate");
        let config = dir.join("user_config.json");
        let seed = json!({
            "TextTab": {
                "create_presets": {"A": {}},
                "imported_system_fonts": ["/fonts/X.ttf"]
            }
        });
        fs::write(&config, serde_json::to_string(&seed).expect("serialize seed"))
            .expect("write seed");

        let removed =
            drop_migrated_user_config_keys(&dir, &config).expect("cleanup must succeed");
        assert_eq!(removed, vec!["create_presets".to_string()]);
        let after: Value = serde_json::from_str(&fs::read_to_string(&config).expect("read config"))
            .expect("valid JSON");
        assert!(
            after.pointer("/TextTab/imported_system_fonts").is_some(),
            "the imported-fonts list must survive until fonts_data.json exists"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A VALID BUT EMPTY `fonts_data.json` is NOT proof that the legacy imported-fonts list
    /// was migrated — `font_settings_store` never runs its legacy migration when a document
    /// is present, so deleting the key would drop the user's imported fonts from both
    /// stores at once (defect 2 of the phase-5 review).
    #[test]
    fn an_empty_fonts_data_does_not_authorize_dropping_the_imported_fonts_key() {
        let dir = unique_temp_dir("empty_fonts_data");
        let config = dir.join("user_config.json");
        fs::write(
            &config,
            json!({
                "TextTab": {
                    "create_presets": {"A": {}},
                    "imported_system_fonts": ["/fonts/MyFont.ttf"]
                }
            })
            .to_string(),
        )
        .expect("write seed");
        seed_fonts_data(&dir, &[]);

        let removed =
            drop_migrated_user_config_keys(&dir, &config).expect("cleanup must succeed");
        assert_eq!(
            removed,
            vec!["create_presets".to_string()],
            "only the presets key may go; the unmigrated font list must stay"
        );
        let after: Value = serde_json::from_str(&fs::read_to_string(&config).expect("read config"))
            .expect("valid JSON");
        assert_eq!(
            after.pointer("/TextTab/imported_system_fonts"),
            Some(&json!(["/fonts/MyFont.ttf"])),
            "the only surviving record of the imported font must not be deleted"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// One legacy path still missing from `fonts_data.json` blocks the deletion of the
    /// whole key: the list is all-or-nothing evidence.
    #[test]
    fn a_partially_migrated_imported_font_list_is_kept() {
        let dir = unique_temp_dir("partial_fonts_data");
        let config = dir.join("user_config.json");
        fs::write(
            &config,
            json!({
                "TextTab": {
                    "create_presets": {"A": {}},
                    "imported_system_fonts": ["/fonts/X.ttf", "/fonts/Y.ttf"]
                }
            })
            .to_string(),
        )
        .expect("write seed");
        seed_fonts_data(&dir, &["/fonts/X.ttf"]);

        let removed =
            drop_migrated_user_config_keys(&dir, &config).expect("cleanup must succeed");
        assert_eq!(removed, vec!["create_presets".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }
}
