/*
File: panel/presets_store.rs

Purpose:
The SINGLE owner of `fonts/presets.json` — the create-panel preset document — and of the
one-shot migration that moved it out of `user_config.json`
(`dev-docs/font_identity_postscript_plan.md`, phase 5).

Main responsibilities:
- define the versioned schema (`version: 2`) and its serde mirror;
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
- `StoredDocument` (the whole decoded document: the presets plus the DEFAULT local set)
- `StoredPresets` (name -> `TypingCreatePreset`, the preset map of that document)
- `DefaultLocalSet` (the document's default local presets + the selected index)
- `LoadOutcome` (Missing / Loaded { document, fingerprint } / Invalid)
- `LegacyCreatePreset` (one preset exactly as an older build stored it)
- `SaveReport` (what a successful save merged in from another app instance: its presets, and
  the DEFAULT local presets appended to this snapshot — both halves merge ADDITIVELY, nothing
  on disk is ever superseded)
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

SCHEMA 2 added the second PARAMETER IDENTITY MODE (`dev-docs/local_presets_plan.md` §6):
each preset may carry `identity_mode` + `local_presets` + `selected_local_preset`, and the
document carries the same pair at top level as the DEFAULT local set. A `version: 1`
document decodes as v2 with those fields at their defaults — NOTHING migrates and nothing is
rewritten eagerly. The version was still bumped so an OLDER build refuses the document
(`PresetsStoreError::NewerVersion`) instead of silently dropping every local preset in it.

LOCAL PRESETS ARE ORDERED AND SELECTED BY INDEX, but IDENTIFIED BY A STABLE ID: the array
order is user data and is never sorted, names may repeat and may be empty, and
`selected_local_preset` is an index that is VALIDATED against the array length on read (out
of range -> `None`). Every row carries an `id` (`LocalPreset::id`) that is minted once and
persisted; a row from a document written before that field existed is given a DETERMINISTIC
id on read (`decode_local_preset_id` -> `legacy_local_preset_id`), so every instance agrees
on it. The two-instance merge of the DEFAULT set reconciles BY ID — same id means the same
logical row and OURS wins, an id seen only on disk is APPENDED (`merge_default_local_set`).
It never lets one instance's set supersede the other's, and it can no longer accumulate one
row per conflicting save the way the old (name, profile) key did.

PRESET NAMES ARE USER DATA AND ARE STORED VERBATIM. Nothing here trims, folds or otherwise
edits a name: `" Рао-кун "` and `"Рао-кун"` are two different presets, and silently
collapsing them (which trimming did) destroyed one of them without a word.
*/

use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Current on-disk schema version of `fonts/presets.json`. Bumped to 2 by the local-preset
/// feature so an older build refuses the document instead of dropping its local presets.
pub(super) const PRESETS_VERSION: u32 = 2;

/// File name of the create-preset document inside the app fonts directory.
const PRESETS_FILE_NAME: &str = "presets.json";

/// Legacy `TextTab` key that held the whole preset map inside `user_config.json`.
/// Read once by the migration, then deleted; never written again.
const LEGACY_CREATE_PRESETS_KEY: &str = "create_presets";

/// Dead `TextTab` key: no reader exists anywhere in `src/`. Deleted together with the
/// migrated preset map so the config stops carrying it forever.
const LEGACY_USE_SYSTEM_FONTS_KEY: &str = "use_system_fonts";

/// On-disk spelling of [`ParamIdentityMode::LocalPreset`]. The `Font` mode has no spelling
/// at all: it is the default and is OMITTED from the document.
const IDENTITY_MODE_LOCAL_PRESET: &str = "local_preset";

/// On-disk spelling of [`ParamIdentityMode::Font`]. Never written (the default is omitted),
/// but accepted on read, because an explicit `"font"` is the obvious hand edit.
const IDENTITY_MODE_FONT: &str = "font";

/// Decoded preset map of the document: preset name -> the preset itself.
pub(super) type StoredPresets = HashMap<String, TypingCreatePreset>;

/// The DEFAULT local-preset set of the document: the set that owns the panel's edits while
/// the panel is in [`ParamIdentityMode::LocalPreset`] mode and NO global preset is applied
/// (`dev-docs/local_presets_plan.md` §5).
///
/// The order of `local_presets` is USER DATA and is never sorted; `selected_local_preset`
/// indexes into it and is validated against its length on read.
#[derive(Debug, Clone, Default)]
pub(super) struct DefaultLocalSet {
    /// The local presets themselves, in user order.
    pub(super) local_presets: Vec<LocalPreset>,
    /// Index of the selected local preset, or `None` for "nothing selected".
    pub(super) selected_local_preset: Option<usize>,
}

/// The whole decoded `presets.json`: the named global presets plus the document-level
/// default local set. This is what [`load_outcome`] returns and what [`save`] writes; the
/// two halves live in ONE document, so they are loaded, merged and written as one unit.
#[derive(Debug, Clone, Default)]
pub(super) struct StoredDocument {
    /// Global presets by name.
    pub(super) presets: StoredPresets,
    /// The default local set (see [`DefaultLocalSet`]).
    pub(super) default_local: DefaultLocalSet,
}

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
    /// Parameter identity mode this preset was saved in, as the on-disk string. `None` —
    /// the omitted default — means [`ParamIdentityMode::Font`], which is also what an
    /// unrecognised string decodes to (with a warning naming it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity_mode: Option<String>,
    /// The preset's OWN local presets, in user order. Payload of the local-preset mode,
    /// omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    local_presets: Vec<LocalPresetFileEntry>,
    /// Index into `local_presets`; omitted when nothing was selected, and decoded as
    /// `None` when it does not address an existing entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selected_local_preset: Option<usize>,
}

/// One local preset as stored on disk: a VERBATIM name plus a full render-data snapshot.
///
/// Both fields are omitted when they carry nothing (an empty name, a null profile), by the
/// same document-slimming rule as everything else here; both decode back to the same value.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LocalPresetFileEntry {
    /// STABLE IDENTITY of the row ([`LocalPreset::id`]), as a hyphenated UUID. Always
    /// written by this build; absent in a document written before the field existed, and
    /// then re-minted DETERMINISTICALLY on read ([`decode_local_preset_id`]).
    ///
    /// Typed as a `String`, not a `Uuid`, on purpose: a hand-edited or truncated value must
    /// cost this ONE row a re-mint, not fail the whole document into `Invalid` and have the
    /// user's presets quarantined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    /// User-visible name, stored byte for byte — never trimmed, folded or deduplicated.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    name: String,
    /// Full render-data snapshot (`{"text_params": {…, "font": …}, "effects": […]}`).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    profile: Value,
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
    /// The DEFAULT local set of the document, in user order; omitted when empty. NOT keyed
    /// and NOT sorted — see [`DefaultLocalSet`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    local_presets: Vec<LocalPresetFileEntry>,
    /// Index into the top-level `local_presets`; omitted when absent, decoded as `None`
    /// when out of range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selected_local_preset: Option<usize>,
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
        /// The decoded document: the presets AND the default local set.
        document: StoredDocument,
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
    /// DEFAULT local presets that were on disk but not in the saved snapshot, in the order
    /// they were APPENDED to it (see the merge rule in [`save`]). Empty when the on-disk set
    /// held nothing this snapshot did not already carry. Like `merged_from_disk`, they are
    /// already part of the document that was just written and the caller must take them
    /// over, or its next snapshot would drop them again.
    pub(super) appended_default_local: Vec<LocalPreset>,
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
    /// Rewriting it as v2 would silently drop every field that newer version added.
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

impl PresetsStoreError {
    /// Whether re-attempting the very same save could ever succeed.
    ///
    /// The debounced DEFAULT-local-set writer re-arms itself only on a retryable failure
    /// (`local_presets::rearm_default_local_set_after_failed_save`): re-arming on a
    /// permanent one would rewrite, fail and re-log every debounce window for the rest of
    /// the session without ever getting the data to disk.
    ///
    /// PERMANENT: [`Self::PersistenceDisabled`] (the only copy of the document is corrupt
    /// and could not be moved aside — nothing may be written to that path for the rest of
    /// the session), [`Self::NewerVersion`] (this build can never write a document whose
    /// schema it does not understand) and [`Self::Serialize`] (the same snapshot would fail
    /// to serialize again).
    ///
    /// RETRYABLE: everything environmental — the directory, the read-back, the write itself,
    /// and a conflict with another app instance.
    #[must_use]
    pub(super) fn is_retryable(&self) -> bool {
        match self {
            Self::CreateDir { .. }
            | Self::ReadExisting { .. }
            | Self::Write(_)
            | Self::Conflict { .. } => true,
            Self::Serialize { .. }
            | Self::NewerVersion { .. }
            | Self::PersistenceDisabled { .. } => false,
        }
    }
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
        document: decode(file),
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
/// Names are taken VERBATIM (see the file header). Only a completely empty PRESET name is
/// dropped: it can address no preset in the combo and the user cannot have typed it. Local
/// preset names are NOT subject to that rule — they are addressed by index, so an empty one
/// is perfectly usable.
///
/// A `version: 1` document simply has none of the local-preset keys, so it decodes with
/// [`ParamIdentityMode::Font`], an empty set and no selection: the migration is the absence
/// of one.
fn decode(file: PresetsFile) -> StoredDocument {
    let presets = file
        .presets
        .into_iter()
        .filter(|(name, _)| !name.is_empty())
        .map(|(name, entry)| {
            let local_presets = decode_local_presets(entry.local_presets);
            let selected_local_preset = validate_selection(
                entry.selected_local_preset,
                local_presets.len(),
                &format!("preset '{name}'"),
            );
            (
                name,
                TypingCreatePreset {
                    font: entry.font,
                    font_profiles: entry.profiles.into_iter().collect(),
                    identity_mode: decode_identity_mode(entry.identity_mode.as_deref()),
                    local_presets,
                    selected_local_preset,
                },
            )
        })
        .collect();
    let local_presets = decode_local_presets(file.local_presets);
    let selected_local_preset = validate_selection(
        file.selected_local_preset,
        local_presets.len(),
        "the default local set",
    );
    StoredDocument {
        presets,
        default_local: DefaultLocalSet {
            local_presets,
            selected_local_preset,
        },
    }
}

/// Decodes one stored parameter identity mode.
///
/// An ABSENT value is the omitted default, [`ParamIdentityMode::Font`]. An UNRECOGNISED
/// string also decodes to `Font` — the safe half, since it owns the payload every build
/// understands — but is LOGGED with the offending value, because it is either a hand edit or
/// a document from a build this one does not know.
fn decode_identity_mode(stored: Option<&str>) -> ParamIdentityMode {
    match stored {
        None | Some(IDENTITY_MODE_FONT) => ParamIdentityMode::Font,
        Some(IDENTITY_MODE_LOCAL_PRESET) => ParamIdentityMode::LocalPreset,
        Some(unknown) => {
            crate::runtime_log::log_warn(format!(
                "typing presets: unknown identity_mode '{unknown}' in presets.json; falling \
                 back to '{IDENTITY_MODE_FONT}' (the font owns the parameters)."
            ));
            ParamIdentityMode::Font
        }
    }
}

/// Decodes an ORDERED array of local presets, preserving the stored order exactly — the
/// order is user data and the index into it is the panel's SELECTION key.
///
/// Each row keeps (or is given) its STABLE ID, which is what the cross-instance merge
/// reconciles by; see [`decode_local_preset_id`] for where an id comes from when the
/// document has none, and [`merge_default_local_set`] for what it is used for.
///
/// IDS ARE UNIQUE WITHIN THE ARRAY. A document that repeats one (only reachable by hand
/// editing, or by copying a row inside the file) would make the merge treat two rows as one;
/// the duplicate is given a fresh identity instead, with a warning.
fn decode_local_presets(stored: Vec<LocalPresetFileEntry>) -> Vec<LocalPreset> {
    let mut seen: HashSet<Uuid> = HashSet::with_capacity(stored.len());
    stored
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            let mut id = decode_local_preset_id(index, &entry);
            if !seen.insert(id) {
                crate::runtime_log::log_warn(format!(
                    "typing presets: local preset #{index} ('{}') repeats the id {id} of an \
                     earlier entry; giving it a fresh one so the two stay distinct.",
                    entry.name
                ));
                id = Uuid::new_v4();
                seen.insert(id);
            }
            LocalPreset::with_id(id, entry.name, entry.profile)
        })
        .collect()
}

/// The stable id of ONE stored local preset: the one in the document, or a deterministic
/// mint when the document has none.
///
/// A document written before the id existed carries no `id` at all, and a hand-edited one
/// may carry something that is not a UUID. Both mint through
/// [`legacy_local_preset_id`], which is a pure function of the row's position and content —
/// so a second app instance loading THE SAME document mints THE SAME id and the merge still
/// recognises the row. Anything non-deterministic here (a random id, a timestamp) would put
/// the unbounded-growth defect straight back.
fn decode_local_preset_id(index: usize, entry: &LocalPresetFileEntry) -> Uuid {
    match entry.id.as_deref() {
        Some(stored) => match Uuid::parse_str(stored) {
            Ok(id) => id,
            Err(err) => {
                crate::runtime_log::log_warn(format!(
                    "typing presets: local preset #{index} ('{}') carries the unusable id \
                     '{stored}' ({err}); minting the deterministic legacy id instead.",
                    entry.name
                ));
                legacy_local_preset_id(index, &entry.name, &entry.profile)
            }
        },
        None => legacy_local_preset_id(index, &entry.name, &entry.profile),
    }
}

/// Validates a stored `selected_local_preset` against the length of the list it indexes.
///
/// Returns `None` for an absent or OUT-OF-RANGE index, so no caller can ever index a list
/// with it. `owner` names the place for the log line ("preset 'X'" / "the default local
/// set"); an out-of-range index is a data anomaly and is logged rather than swallowed.
fn validate_selection(stored: Option<usize>, len: usize, owner: &str) -> Option<usize> {
    let selected = stored?;
    if selected < len {
        return Some(selected);
    }
    crate::runtime_log::log_warn(format!(
        "typing presets: selected_local_preset {selected} of {owner} addresses no entry \
         ({len} local preset(s) stored); treating it as no selection."
    ));
    None
}

/// Converts the decoded runtime form into the serde mirror, stamping the current version.
/// Names are written VERBATIM; a preset with a completely empty name is dropped.
///
/// Everything the local-preset feature added is OMITTED when it carries nothing, so a
/// document that uses only the font mode is byte-identical to what schema 1 wrote apart from
/// the version number.
fn encode(document: &StoredDocument) -> PresetsFile {
    PresetsFile {
        version: PRESETS_VERSION,
        presets: document
            .presets
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
                        identity_mode: encode_identity_mode(preset.identity_mode),
                        local_presets: encode_local_presets(&preset.local_presets),
                        selected_local_preset: preset.selected_local_preset,
                    },
                )
            })
            .collect(),
        local_presets: encode_local_presets(&document.default_local.local_presets),
        selected_local_preset: document.default_local.selected_local_preset,
    }
}

/// On-disk spelling of a parameter identity mode, or `None` for the default that is OMITTED.
fn encode_identity_mode(mode: ParamIdentityMode) -> Option<String> {
    match mode {
        ParamIdentityMode::Font => None,
        ParamIdentityMode::LocalPreset => Some(IDENTITY_MODE_LOCAL_PRESET.to_string()),
    }
}

/// Encodes local presets in ORDER; the array order is user data and is never sorted. The
/// stable id is ALWAYS written — a row that reached this build without one has already been
/// given a deterministic id on read, and persisting it is what stops it being re-minted.
fn encode_local_presets(local_presets: &[LocalPreset]) -> Vec<LocalPresetFileEntry> {
    local_presets
        .iter()
        .map(|preset| LocalPresetFileEntry {
            id: Some(preset.id().to_string()),
            name: preset.name.clone(),
            profile: preset.profile().clone(),
        })
        .collect()
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

/// Atomically writes a full snapshot of `document` to `fonts/presets.json`, creating the
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
/// MERGING THE DEFAULT LOCAL SET follows the same additive bias — see
/// [`merge_default_local_set`] for the rule and for what stands in for the missing merge
/// key.
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
    document: &StoredDocument,
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

    let mut snapshot = document.clone();
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
                for (name, preset) in disk.presets {
                    // Ours wins a name clash: this snapshot is what the user has on screen.
                    if !snapshot.presets.contains_key(&name) {
                        snapshot.presets.insert(name.clone(), preset.clone());
                        report.merged_from_disk.insert(name, preset);
                    }
                }
                merge_default_local_set(&path, &mut snapshot, disk.default_local, &mut report);
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

/// Reconciles the DEFAULT local set of a pending snapshot with the one another app instance
/// left on disk.
///
/// THE MERGE IS ADDITIVE, exactly like the preset half above and like `fonts_data`: an entry
/// present on disk but not in this snapshot is APPENDED to it, ours first, both sides in
/// user order. Nothing on disk is ever dropped — two app instances editing their own default
/// sets used to silently overwrite each other, with the loser only logged.
///
/// THE COMPARISON KEY IS THE STABLE ID ([`LocalPreset::id`]) AND NOTHING ELSE. Neither the
/// name nor the index is an identity — names may repeat or be empty
/// (`dev-docs/local_presets_plan.md` §3, D3) and the index is only the panel's cursor into
/// the list on screen. The rule is therefore:
/// - **same id → the same logical row, and OURS WINS.** We are the newer writer and the user
///   is looking at our version, so the disk version is dropped, not appended;
/// - **an id only on disk → APPENDED**, after ours, in disk order;
/// - **our rows keep their order**, so our selection index stays valid.
///
/// Comparing (name, profile) instead — which is what this did before — is what made the set
/// grow WITHOUT BOUND: two instances editing the same logical row produce two different
/// snapshots, neither recognises the other as itself, and every conflicting save appends one
/// more historical version that nothing will ever remove.
///
/// THE SELECTION INDEX STAYS OURS: it points into the live set the user is looking at, and
/// appending at the END cannot invalidate it. The only exception is a snapshot with NO local
/// presets at all, which has no selection to keep and takes the disk set whole.
///
/// THE ACCEPTED ASYMMETRY, the same one `fonts_data`'s merge carries: with no tombstone, a row
/// the OTHER instance deleted while we still hold it comes back on our next save. That is the
/// deliberate "never destroy the last clue" bias — the set is now bounded by the number of
/// LOGICAL rows either instance ever had, not by the number of conflicting saves, and a row
/// that returns can be deleted again, while one that was destroyed cannot be recovered.
///
/// Appending into `snapshot` and reporting in `report` happen together: the appended entries
/// are about to be written, so the caller must take them over as well.
fn merge_default_local_set(
    path: &Path,
    snapshot: &mut StoredDocument,
    disk: DefaultLocalSet,
    report: &mut SaveReport,
) {
    if disk.local_presets.is_empty() {
        return;
    }
    if snapshot.default_local.local_presets.is_empty() {
        // The whole set, selection included: an empty snapshot has nothing of its own to
        // preserve, and this is the case the additive rule exists for — a panel that never
        // entered local-preset mode would otherwise erase the set on its first save.
        report.appended_default_local = disk.local_presets.clone();
        snapshot.default_local = disk;
        return;
    }
    let appended: Vec<LocalPreset> = disk
        .local_presets
        .into_iter()
        .filter(|theirs| {
            !snapshot
                .default_local
                .local_presets
                .iter()
                .any(|ours| same_local_preset(ours, theirs))
        })
        .collect();
    if appended.is_empty() {
        return;
    }
    crate::runtime_log::log_info(format!(
        "typing presets: {} carried {} default local preset(s) this panel does not have \
         (another app instance wrote them); appending them after the {} on screen rather \
         than superseding them.",
        path.display(),
        appended.len(),
        snapshot.default_local.local_presets.len()
    ));
    snapshot
        .default_local
        .local_presets
        .extend(appended.iter().cloned());
    report.appended_default_local = appended;
}

/// Whether two local presets are the SAME LOGICAL ENTRY: same stable id. Their names and
/// snapshots may well differ — that is the normal case of two instances having edited the
/// same row, and it is precisely what must NOT produce two rows. See
/// [`merge_default_local_set`].
#[must_use]
pub(super) fn same_local_preset(a: &LocalPreset, b: &LocalPreset) -> bool {
    a.id() == b.id()
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
        disk: Option<StoredDocument>,
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

/// Serializes `document` and writes it over `path` durably. Returns the fingerprint of the
/// bytes just written — the caller's new baseline.
fn write_document(
    path: &Path,
    document: &StoredDocument,
) -> Result<doc_store::DocumentFingerprint, PresetsStoreError> {
    let file = encode(document);
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
    fn expect_loaded(outcome: LoadOutcome) -> StoredDocument {
        match outcome {
            LoadOutcome::Loaded { document, .. } => document,
            LoadOutcome::Missing => panic!("expected Loaded, got Missing"),
            LoadOutcome::Invalid => panic!("expected Loaded, got Invalid"),
        }
    }

    /// A document carrying only global presets — the shape every schema-1 test uses.
    fn document(presets: StoredPresets) -> StoredDocument {
        StoredDocument {
            presets,
            default_local: DefaultLocalSet::default(),
        }
    }

    fn preset(font: &str, profiles: &[(&str, Value)]) -> TypingCreatePreset {
        TypingCreatePreset {
            font: font.to_string(),
            font_profiles: profiles
                .iter()
                .map(|(key, value)| ((*key).to_string(), value.clone()))
                .collect(),
            ..TypingCreatePreset::default()
        }
    }

    /// One local preset with a distinguishable profile, so a test can see WHICH one it is.
    fn local(name: &str, marker: f32) -> LocalPreset {
        LocalPreset::new(
            name.to_string(),
            json!({"text_params": {"schema": 2, "font_size_px": marker}}),
        )
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
        save(&dir, &document(presets), next_save_ticket()).expect("save presets");

        let loaded = expect_loaded(load_outcome(&dir)).presets;
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
        save(&dir, &document(presets), next_save_ticket()).expect("save presets");

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

        let loaded = expect_loaded(load_outcome(&dir)).presets;
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
    /// omits everything unset — including every key schema 2 added. The three legacy
    /// `primary_font_*` keys are gone for good, and the version is now 2, so an older build
    /// refuses the document instead of dropping the local presets it cannot represent.
    #[test]
    fn written_document_is_version_two_with_a_single_font_key() {
        let dir = unique_temp_dir("shape");
        let mut presets = StoredPresets::new();
        presets.insert("A".to_string(), preset("Some-Font", &[]));
        save(&dir, &document(presets), next_save_ticket()).expect("save presets");

        let raw = fs::read_to_string(data_path(&dir)).expect("read written file");
        let value: Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(value.pointer("/version"), Some(&json!(2)));
        assert_eq!(value.pointer("/presets/A/font"), Some(&json!("Some-Font")));
        let entry = value
            .pointer("/presets/A")
            .and_then(Value::as_object)
            .expect("preset object");
        // `profiles` is omitted when empty; no legacy key is ever written, and neither is
        // any of the local-preset keys while the preset stays in the font mode.
        assert_eq!(entry.keys().collect::<Vec<_>>(), vec!["font"]);
        for legacy in [
            "primary_font_key",
            "primary_font_path",
            "primary_font_label",
        ] {
            assert!(!entry.contains_key(legacy), "'{legacy}' must not be written");
        }
        let root = value.as_object().expect("document object");
        assert_eq!(
            root.keys().collect::<Vec<_>>(),
            vec!["presets", "version"],
            "an unused default local set must not appear in the document at all"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The save is CRASH-DURABLE, not merely atomic: the containing directory is fsynced
    /// before it returns, because the caller deletes the presets from `user_config.json`
    /// right afterwards (defect 1 of the phase-5 review).
    #[test]
    fn a_successful_save_makes_the_directory_entry_durable() {
        let dir = unique_temp_dir("durable");
        save(&dir, &StoredDocument::default(), next_save_ticket()).expect("save presets");
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
        let err = save(&dir, &document(presets), next_save_ticket())
            .expect_err("the save must be refused");
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
        let err = save(&blocked, &StoredDocument::default(), next_save_ticket())
            .expect_err("save must fail");
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
        save(&dir, &document(newer), newer_ticket).expect("newer save");
        save(&dir, &document(older), older_ticket).expect("older save is a no-op, not an error");

        let loaded = expect_loaded(load_outcome(&dir)).presets;
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
        save(&dir, &document(ours.clone()), next_save_ticket()).expect("first save");

        // Instance B (a different process) writes a document A has never seen.
        let mut theirs = StoredPresets::new();
        theirs.insert("Наш".to_string(), preset("Alpha-Regular", &[]));
        theirs.insert("Чужой".to_string(), preset("Beta-Regular", &[]));
        let raw = serde_json::to_string_pretty(&encode(&document(theirs))).expect("serialize");
        fs::write(data_path(&dir), format!("{raw}\n")).expect("other instance writes");

        // Instance A saves again: the conflict is detected, merged and retried.
        ours.insert("Ещё наш".to_string(), preset("Gamma-Regular", &[]));
        let report = save(&dir, &document(ours), next_save_ticket()).expect("merged save");
        assert_eq!(
            report.merged_from_disk.keys().collect::<Vec<_>>(),
            vec!["Чужой"],
            "what the other instance added must be reported back to the panel"
        );

        let loaded = expect_loaded(load_outcome(&dir)).presets;
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
        let err = save(&dir, &StoredDocument::default(), next_save_ticket())
            .expect_err("a newer document must not be replaced");
        assert!(
            matches!(err, PresetsStoreError::NewerVersion { found: 99 }),
            "unexpected variant: {err:?}"
        );
        let raw = fs::read_to_string(data_path(&dir)).expect("read back");
        assert!(raw.contains("\"future\""), "the newer document is intact");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A SCHEMA-1 DOCUMENT decodes as schema 2 with every new field at its default, and is
    /// NOT rewritten by the mere act of reading it: there is no migration at all
    /// (`dev-docs/local_presets_plan.md` §6).
    #[test]
    fn a_version_one_document_decodes_as_version_two_with_defaults() {
        let dir = unique_temp_dir("v1_decode");
        let seed = json!({
            "version": 1,
            "presets": {
                "ВВД": {
                    "font": "d_CCShoutOut",
                    "profiles": {"d_CCShoutOut": {"schema": 2}}
                }
            }
        })
        .to_string();
        fs::write(data_path(&dir), &seed).expect("seed v1 document");

        let loaded = expect_loaded(load_outcome(&dir));
        let vvd = loaded.presets.get("ВВД").expect("the v1 preset survives");
        assert_eq!(vvd.font, "d_CCShoutOut");
        assert_eq!(vvd.identity_mode, ParamIdentityMode::Font);
        assert!(vvd.local_presets.is_empty());
        assert_eq!(vvd.selected_local_preset, None);
        assert!(loaded.default_local.local_presets.is_empty());
        assert_eq!(loaded.default_local.selected_local_preset, None);
        assert_eq!(
            fs::read_to_string(data_path(&dir)).expect("read back"),
            seed,
            "reading a v1 document must not rewrite it"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The local-preset payload round-trips losslessly: the ARRAY ORDER is preserved (a
    /// local preset is addressed by index), names are verbatim — including an empty one —
    /// and both the per-preset and the document-level selection come back.
    #[test]
    fn local_presets_round_trip_in_order_with_their_selection() {
        let dir = unique_temp_dir("local_round_trip");
        let mut presets = StoredPresets::new();
        presets.insert(
            "ВВД".to_string(),
            TypingCreatePreset {
                identity_mode: ParamIdentityMode::LocalPreset,
                local_presets: vec![local("Яркий", 1.0), local("", 2.0), local("Яркий", 3.0)],
                selected_local_preset: Some(2),
                ..TypingCreatePreset::default()
            },
        );
        let stored = StoredDocument {
            presets,
            default_local: DefaultLocalSet {
                local_presets: vec![local("По умолчанию", 4.0), local("Второй", 5.0)],
                selected_local_preset: Some(1),
            },
        };
        save(&dir, &stored, next_save_ticket()).expect("save presets");

        let raw = fs::read_to_string(data_path(&dir)).expect("read written file");
        let value: Value = serde_json::from_str(&raw).expect("valid JSON");
        assert_eq!(
            value.pointer("/presets/ВВД/identity_mode"),
            Some(&json!("local_preset"))
        );
        assert_eq!(
            value
                .pointer("/presets/ВВД/local_presets")
                .and_then(Value::as_array)
                .map(|rows| rows
                    .iter()
                    .map(|row| row.pointer("/name").and_then(Value::as_str).unwrap_or(""))
                    .collect::<Vec<_>>()),
            Some(vec!["Яркий", "", "Яркий"]),
            "the array order is user data and must be written as given"
        );
        // The document-slimming rule reaches into the rows too: an empty name is omitted.
        // The STABLE ID is not subject to it — it is always written, or the next load would
        // re-mint it and two instances would stop recognising the row.
        let unnamed = value
            .pointer("/presets/ВВД/local_presets/1")
            .and_then(Value::as_object)
            .expect("second row object");
        assert_eq!(unnamed.keys().collect::<Vec<_>>(), vec!["id", "profile"]);
        assert_eq!(value.pointer("/selected_local_preset"), Some(&json!(1)));

        let loaded = expect_loaded(load_outcome(&dir));
        let vvd = loaded.presets.get("ВВД").expect("preset survives");
        assert_eq!(vvd.identity_mode, ParamIdentityMode::LocalPreset);
        assert_eq!(vvd.selected_local_preset, Some(2));
        assert_eq!(
            vvd.local_presets
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Яркий", "", "Яркий"]
        );
        assert_eq!(vvd.local_presets[2].profile, local("Яркий", 3.0).profile);
        // Every row comes back with the identity it was written with: the id is what the
        // cross-instance merge reconciles by, so a round trip that re-minted it would be a
        // silent identity change.
        assert_eq!(
            vvd.local_presets
                .iter()
                .map(LocalPreset::id)
                .collect::<Vec<_>>(),
            stored
                .presets
                .get("ВВД")
                .expect("the preset we saved")
                .local_presets
                .iter()
                .map(LocalPreset::id)
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            loaded
                .default_local
                .local_presets
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>(),
            vec!["По умолчанию", "Второй"]
        );
        assert_eq!(loaded.default_local.selected_local_preset, Some(1));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A `selected_local_preset` that addresses no entry decodes to NO SELECTION, at both
    /// levels of the document: nothing may ever index a list with a stored number.
    #[test]
    fn an_out_of_range_selected_local_preset_decodes_to_no_selection() {
        let dir = unique_temp_dir("selection_range");
        fs::write(
            data_path(&dir),
            json!({
                "version": 2,
                "presets": {
                    "A": {
                        "identity_mode": "local_preset",
                        "local_presets": [{"name": "Один", "profile": {}}],
                        "selected_local_preset": 7
                    },
                    "B": {"selected_local_preset": 0}
                },
                "selected_local_preset": 3
            })
            .to_string(),
        )
        .expect("seed document");

        let loaded = expect_loaded(load_outcome(&dir));
        let a = loaded.presets.get("A").expect("preset A");
        assert_eq!(a.local_presets.len(), 1, "the entries themselves survive");
        assert_eq!(a.selected_local_preset, None);
        assert_eq!(
            loaded.presets.get("B").and_then(|b| b.selected_local_preset),
            None,
            "an index into an EMPTY list addresses nothing either"
        );
        assert_eq!(loaded.default_local.selected_local_preset, None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// An `identity_mode` this build does not know falls back to the FONT mode — the half
    /// every build understands — instead of failing the whole document. An explicit
    /// `"font"` is accepted, though it is never written.
    #[test]
    fn an_unknown_identity_mode_falls_back_to_the_font_mode() {
        let dir = unique_temp_dir("bad_identity_mode");
        fs::write(
            data_path(&dir),
            json!({
                "version": 2,
                "presets": {
                    "A": {"font": "Alpha-Regular", "identity_mode": "телепатия"},
                    "B": {"font": "Beta-Regular", "identity_mode": "font"},
                    "C": {"font": "Gamma-Regular", "identity_mode": "local_preset"}
                }
            })
            .to_string(),
        )
        .expect("seed document");

        let loaded = expect_loaded(load_outcome(&dir)).presets;
        assert_eq!(
            loaded.get("A").map(|preset| preset.identity_mode),
            Some(ParamIdentityMode::Font)
        );
        assert_eq!(
            loaded.get("B").map(|preset| preset.identity_mode),
            Some(ParamIdentityMode::Font)
        );
        assert_eq!(
            loaded.get("C").map(|preset| preset.identity_mode),
            Some(ParamIdentityMode::LocalPreset),
            "the known spelling must still decode"
        );
        // The font payload of the unknown-mode preset is untouched: nothing is dropped.
        assert_eq!(
            loaded.get("A").map(|preset| preset.font.as_str()),
            Some("Alpha-Regular")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// TWO APP INSTANCES, DEFAULT LOCAL SET. A snapshot that carries NO local presets never
    /// writes its emptiness over the set another instance saved: the set is adopted whole
    /// and reported back, exactly like a merged preset.
    #[test]
    fn an_empty_default_local_set_adopts_the_one_another_instance_wrote() {
        let dir = unique_temp_dir("adopt_default_local");
        let mut ours = StoredPresets::new();
        ours.insert("Наш".to_string(), preset("Alpha-Regular", &[]));
        save(&dir, &document(ours.clone()), next_save_ticket()).expect("first save");

        // Instance B writes a default local set this process has never seen.
        let theirs = StoredDocument {
            presets: StoredPresets::new(),
            default_local: DefaultLocalSet {
                local_presets: vec![local("Чужой", 9.0)],
                selected_local_preset: Some(0),
            },
        };
        let raw = serde_json::to_string_pretty(&encode(&theirs)).expect("serialize");
        fs::write(data_path(&dir), format!("{raw}\n")).expect("other instance writes");

        let report = save(&dir, &document(ours), next_save_ticket()).expect("merged save");
        assert_eq!(
            report
                .appended_default_local
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Чужой"],
            "the set must be reported so the panel can take it over",
        );

        let loaded = expect_loaded(load_outcome(&dir));
        assert_eq!(
            loaded
                .default_local
                .local_presets
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Чужой"],
            "the adopted set is part of the document that was just written"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The other half of the same rule, and the DEFECT it replaces: a non-empty snapshot set
    /// used to SUPERSEDE the on-disk one, so two app instances silently overwrote each
    /// other's default local set. The merge is additive — theirs is appended after ours,
    /// user order preserved on both sides, our selection index untouched.
    #[test]
    fn a_default_local_set_from_another_instance_is_appended_not_superseded() {
        let dir = unique_temp_dir("keep_default_local");
        let mut ours = StoredDocument::default();
        ours.default_local.local_presets = vec![local("Наш", 1.0)];
        ours.default_local.selected_local_preset = Some(0);
        save(&dir, &ours, next_save_ticket()).expect("first save");

        let theirs = StoredDocument {
            presets: StoredPresets::new(),
            default_local: DefaultLocalSet {
                local_presets: vec![local("Чужой", 9.0), local("Второй чужой", 8.0)],
                selected_local_preset: Some(1),
            },
        };
        let raw = serde_json::to_string_pretty(&encode(&theirs)).expect("serialize");
        fs::write(data_path(&dir), format!("{raw}\n")).expect("other instance writes");

        let report = save(&dir, &ours, next_save_ticket()).expect("merged save");
        assert_eq!(
            report
                .appended_default_local
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Чужой", "Второй чужой"],
            "what came from disk must be reported so the panel adopts it too",
        );
        let loaded = expect_loaded(load_outcome(&dir));
        assert_eq!(
            loaded
                .default_local
                .local_presets
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Наш", "Чужой", "Второй чужой"],
            "ours first, theirs appended, both in user order",
        );
        assert_eq!(
            loaded.default_local.selected_local_preset,
            Some(0),
            "the selection index is ours and appending at the end cannot invalidate it",
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// THE MERGE KEY IS THE STABLE ID. The row we wrote a moment ago comes back from disk
    /// EDITED BY THE OTHER INSTANCE — a different name and a different snapshot — and is
    /// still recognised as the same logical row: ours wins and nothing is appended. A row
    /// that merely shares a name has its own id and is a genuinely different row, so it is
    /// kept.
    ///
    /// The defect this replaces: with (name, profile) as the key, the edited row was
    /// unrecognisable and every conflicting save appended one more historical version of it
    /// (see `presets_grow_without_bound_...` below).
    #[test]
    fn a_default_local_preset_is_recognised_by_id_however_it_was_edited() {
        let dir = unique_temp_dir("dedup_default_local");
        let shared = local("Общий", 1.0);
        let mut ours = StoredDocument::default();
        ours.default_local.local_presets = vec![shared.clone(), local("Наш", 2.0)];
        save(&dir, &ours, next_save_ticket()).expect("first save");

        // The other instance holds the very same row — RENAMED and re-edited — plus a
        // DIFFERENT preset of its own that happens to carry the same name.
        let edited = LocalPreset::with_id(
            shared.id(),
            "Общий (у них)".to_string(),
            json!({"text_params": {"schema": 2, "font_size_px": 7.0}}),
        );
        let theirs = StoredDocument {
            presets: StoredPresets::new(),
            default_local: DefaultLocalSet {
                local_presets: vec![edited, local("Общий", 5.0)],
                selected_local_preset: None,
            },
        };
        let raw = serde_json::to_string_pretty(&encode(&theirs)).expect("serialize");
        fs::write(data_path(&dir), format!("{raw}\n")).expect("other instance writes");

        let report = save(&dir, &ours, next_save_ticket()).expect("merged save");
        assert_eq!(report.appended_default_local.len(), 1);
        let loaded = expect_loaded(load_outcome(&dir));
        assert_eq!(
            loaded
                .default_local
                .local_presets
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Общий", "Наш", "Общий"],
            "our version of the shared row wins, the same-named different one is appended",
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// REPEATED CONFLICTS DO NOT GROW THE SET. The regression test for the defect the stable
    /// id was introduced for: two instances take turns editing THE SAME logical row, each
    /// save conflicting with the other's. Under the old (name, profile) key every round
    /// appended one more version of that row and nothing ever removed it; under the id key
    /// the set stays exactly as long as it started.
    #[test]
    fn repeated_conflicting_saves_of_one_row_never_grow_the_default_local_set() {
        let dir = unique_temp_dir("bounded_default_local");
        let shared = local("Общий", 1.0);
        let mut ours = StoredDocument::default();
        ours.default_local.local_presets = vec![shared.clone()];
        ours.default_local.selected_local_preset = Some(0);
        save(&dir, &ours, next_save_ticket()).expect("first save");

        for round in 0..5 {
            // The other instance edits ITS copy of the same logical row and writes it.
            let theirs = StoredDocument {
                presets: StoredPresets::new(),
                default_local: DefaultLocalSet {
                    local_presets: vec![LocalPreset::with_id(
                        shared.id(),
                        format!("Общий {round}"),
                        json!({"text_params": {"schema": 2, "font_size_px": 10.0 + f64::from(round)}}),
                    )],
                    selected_local_preset: Some(0),
                },
            };
            let raw = serde_json::to_string_pretty(&encode(&theirs)).expect("serialize");
            fs::write(data_path(&dir), format!("{raw}\n")).expect("other instance writes");

            // We edit OUR copy of the same row and save into the conflict.
            ours.default_local.local_presets = vec![LocalPreset::with_id(
                shared.id(),
                format!("Наш {round}"),
                json!({"text_params": {"schema": 2, "font_size_px": 20.0 + f64::from(round)}}),
            )];
            let report = save(&dir, &ours, next_save_ticket()).expect("merged save");
            assert!(
                report.appended_default_local.is_empty(),
                "round {round}: the other instance's version of OUR row is not a new row",
            );
            let loaded = expect_loaded(load_outcome(&dir));
            assert_eq!(
                loaded
                    .default_local
                    .local_presets
                    .iter()
                    .map(|preset| preset.name.as_str())
                    .collect::<Vec<_>>(),
                vec![format!("Наш {round}").as_str()],
                "round {round}: one logical row stays one row, and ours is the version kept",
            );
            assert_eq!(loaded.default_local.selected_local_preset, Some(0));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// A document written BEFORE the stable id existed mints its ids DETERMINISTICALLY: the
    /// same document loaded twice (which is what two app instances do) yields the same ids,
    /// so the merge recognises the rows instead of appending them. Once saved, the minted ids
    /// are persisted and survive verbatim.
    #[test]
    fn a_pre_id_document_mints_the_same_ids_on_every_load() {
        let dir = unique_temp_dir("legacy_local_ids");
        // Hand-written v2 document without any `id` key — exactly what the build before this
        // field wrote.
        let raw = json!({
            "version": PRESETS_VERSION,
            "presets": {},
            "local_presets": [
                {"name": "Первый", "profile": {"text_params": {"schema": 2}}},
                {"name": "", "profile": {"text_params": {"schema": 2, "font_size_px": 9.0}}},
            ],
            "selected_local_preset": 1,
        });
        fs::write(
            data_path(&dir),
            serde_json::to_string_pretty(&raw).expect("serialize"),
        )
        .expect("seed legacy document");

        let first = expect_loaded(load_outcome(&dir)).default_local.local_presets;
        let second = expect_loaded(load_outcome(&dir)).default_local.local_presets;
        assert_eq!(
            first.iter().map(LocalPreset::id).collect::<Vec<_>>(),
            second.iter().map(LocalPreset::id).collect::<Vec<_>>(),
            "a pre-id document must mint the SAME ids on every load, or two instances would \
             never recognise each other's rows",
        );
        assert_ne!(
            first[0].id(),
            first[1].id(),
            "two rows of one document must not share an identity",
        );

        let document = StoredDocument {
            presets: StoredPresets::new(),
            default_local: DefaultLocalSet {
                local_presets: first.clone(),
                selected_local_preset: Some(1),
            },
        };
        save(&dir, &document, next_save_ticket()).expect("save the minted ids");
        assert_eq!(
            expect_loaded(load_outcome(&dir))
                .default_local
                .local_presets
                .iter()
                .map(LocalPreset::id)
                .collect::<Vec<_>>(),
            first.iter().map(LocalPreset::id).collect::<Vec<_>>(),
            "the minted ids are persisted, so nothing re-mints them again",
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The retryability classification the debounced writer re-arms on. A permanent failure
    /// must never re-arm, or the panel would rewrite and re-log every debounce window for
    /// the rest of the session.
    #[test]
    fn only_environmental_save_failures_are_retryable() {
        assert!(
            PresetsStoreError::CreateDir {
                dir: PathBuf::from("/nope"),
                reason: "denied".to_string(),
            }
            .is_retryable()
        );
        assert!(
            PresetsStoreError::ReadExisting {
                path: PathBuf::from("/nope"),
                reason: "denied".to_string(),
            }
            .is_retryable()
        );
        assert!(
            PresetsStoreError::Conflict {
                path: PathBuf::from("/nope"),
                parsable: true,
            }
            .is_retryable()
        );
        assert!(
            !PresetsStoreError::PersistenceDisabled {
                path: PathBuf::from("/nope"),
            }
            .is_retryable(),
            "nothing may be written to that path for the rest of the session",
        );
        assert!(
            !PresetsStoreError::NewerVersion { found: 99 }.is_retryable(),
            "this build can never write a schema it does not understand",
        );
        assert!(
            !PresetsStoreError::Serialize {
                reason: "NaN".to_string(),
            }
            .is_retryable(),
            "the same snapshot would fail to serialize again",
        );
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
