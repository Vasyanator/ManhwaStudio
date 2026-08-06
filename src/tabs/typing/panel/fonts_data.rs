/*
File: panel/fonts_data.rs

Purpose:
Serde schema and disk I/O for the app-level per-font settings document
`fonts_data.json`, stored inside the app fonts directory (`resolve_fonts_dir()`).
This file is the single on-disk home for the user-imported system fonts, per-font
settings (display-name override + default parameter profile) and user-defined VIRTUAL
font groups. Font discovery never picks it up because it only scans `.ttf/.otf/.ttc`.

SCHEMA VERSION 2 — the font is named by its IDENTITY, never by a path:

```jsonc
{
  "version": 2,
  "system_fonts": [ { "font": "Roboto-Medium", "last_path": "/home/…/Roboto-Medium.ttf" } ],
  "fonts": { "CCWildWordsLower-Regular": { "display_name": "Разговор", "profile": { … } } },
  "virtual_groups": [ { "name": "Возлюбленная",
                        "members": [ { "font": "kCCAskForMercy-Regular", "alias": "Основа" } ] } ]
}
```

- `fonts` keys and `virtual_groups[].members[].font` are font IDENTITIES
  (`FontEntry::render_identity_name`), so moving or renaming a font FILE no longer drops
  its display name, its profile or its group membership.
- `system_fonts[].font` is the imported font's PostScript name (its UNSUFFIXED identity);
  `last_path` is a HINT only — the loader accepts it just when the file is still there and
  still claims that name. Otherwise the font is located BY NAME among the installed fonts
  (`fonts::locate_system_font_by_identity`) and the hint is rewritten. An entry that cannot
  be located either way stays in the document (it is never silently dropped) and is surfaced
  as an unavailable, removable row in the settings font list.
- Fields that carry no value are OMITTED (`display_name`, `profile`, `alias`, `last_path`,
  and the empty collections), so the document stays minimal.

VERSION 1 (LEGACY) IS READ FOREVER. A v1 document keys everything by FILE PATH
(`imported_system_fonts`, `font_settings`, and path keys inside `virtual_groups`). It is
decoded verbatim with `FontsData::pending_migration = true`; `font_settings_store` then
re-keys it to identities after the first successful font-list build (the `path → identity`
map does not exist any earlier) and rewrites the document as v2. The legacy keys are never
written back.

THE SCHEMA VERSION IS DECIDED BY CONTENT WHEN THE `version` FIELD IS ABSENT. A document
carrying `system_fonts`/`fonts` but no `version` is a v2 document (a hand edit, a truncated
write, an older writer); reading it as an empty v1 — which a `0` default made it do — threw
every key away on the next save. Both payload shapes are decoded and UNIONED, so a document
that somehow carries v1 AND v2 keys loses neither.

`pending_migration` IS PART OF THE PERSISTED v2 PAYLOAD. A deferred migration that could not
resolve every legacy key rewrites the document in the CURRENT schema but must stay pending,
or the next launch would read a v2 document, never retry, and the unresolved keys would be
frozen forever (the "will apply again" promise in the migration log has to be true).

Main responsibilities:
- define the versioned JSON schema (`version: 2`) and its serde mirror, plus the read-only
  v1 mirror;
- load the document as a typed `LoadOutcome` (`Missing` / `Loaded` / `Invalid`) so the
  caller can distinguish "first run" from "corrupt file" — a corrupt file must NOT be
  silently treated as empty, or the next mutation would overwrite (and destroy) it;
  an unknown future version is still parsed best-effort as `Loaded`;
- quarantine a corrupt document to `fonts_data.json.bad` (`quarantine_bad_file`), reporting
  whether the original was moved, copied, or is still the only surviving copy;
- save a full snapshot atomically and crash-durably (temp sibling written via an explicit
  `File` + `write_all` + `sync_all`, then rename; mirrors `locale_store::write_atomic`),
  creating the fonts directory if missing;
- guard that write with the document's own state: a NEWER schema version is never
  overwritten (its unknown fields would be dropped), and a file that changed since the
  caller's baseline fingerprint is reported as a CONFLICT together with the parsed on-disk
  document, so a second app instance's settings can be merged instead of clobbered.

Key types:
- `FontsData` (decoded in-memory form consumed by `font_settings_store`)
- `LoadOutcome` (Missing / Loaded / Invalid load result)
- `DocumentFingerprint` / `SaveBaseline` / `SaveError` (the write guard)
- `QuarantineOutcome` (what happened to a corrupt document)
- `SystemFontRef` (one imported system font: identity + last-known path hint)
- `FontSettingsRecord` (per-font settings: display-name override + default profile)
- `VirtualFontGroup` / `VirtualFontGroupMember` (user-defined virtual font groups; serde
  mirror AND decoded form; sanitized by `sanitize_virtual_groups` on load and save)

Key functions:
- `data_path` / `load_outcome` / `quarantine_bad_file` / `save_checked`

Notes:
`use super::*;` pulls in the parent `panel` module's imports (`Path`, `PathBuf`,
`fs`). The crash-safe write recipe and the fingerprint/baseline vocabulary live in
`panel/doc_store.rs`, shared with `presets_store` (`DocumentFingerprint` / `SaveBaseline`
are re-exported here under their historical names). Compiled
unconditionally (no wasm cfg gates): raw `std::fs`. A read/parse failure yields
`LoadOutcome::Invalid` with a `runtime_log` warning instead of degrading to empty, so
imported fonts + overrides are never silently wiped.
*/

use super::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Current on-disk schema version of `fonts_data.json` (identity-keyed).
pub(in crate::tabs::typing) const FONTS_DATA_VERSION: u32 = 2;

/// Last schema version that keyed everything by FILE PATH. A document at or below this
/// version is decoded with the legacy rules and flagged for the deferred re-key.
const LEGACY_FONTS_DATA_VERSION: u32 = 1;

/// File name of the per-font settings document inside the app fonts directory.
const FONTS_DATA_FILE_NAME: &str = "fonts_data.json";

/// `skip_serializing_if` predicate for a `bool` field that is only written when set.
/// Takes `&bool` because that is the signature serde's `skip_serializing_if` requires.
fn is_false(value: &bool) -> bool {
    !*value
}

/// Per-font user settings block stored under a font IDENTITY in `fonts_data.json`.
/// Fields are optional and skipped when empty so the document stays minimal.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct FontSettingsEntry {
    /// User display-name override. Absent/`None` means "use the font's own label".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    /// The font's DEFAULT parameter profile (a `text_params`-shaped object). Absent/`None`
    /// means "the font has no remembered parameters yet".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<Value>,
}

/// One imported system font as stored on disk: its PostScript name plus the last path it
/// was seen at. The path is a HINT (see the file header), never the key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SystemFontFileEntry {
    /// PostScript name (unsuffixed identity) of the imported font. Empty only for a v1
    /// document that has not been migrated yet.
    #[serde(default)]
    font: String,
    /// Last known file path of the font, as a string. Absent when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_path: Option<String>,
}

/// One member of a [`VirtualFontGroup`]: a reference to a real known font (a folder font
/// or an imported system font) by its IDENTITY, plus an optional per-group display alias.
/// The JSON member key is `"font"`.
///
/// Used directly as BOTH the serde mirror and the decoded form: the referenced font is
/// always a plain identity string, so no separate disk/runtime split is needed. In a v1
/// document this field holds the legacy PATH key instead; the store re-keys it once the
/// first font list is built.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::tabs::typing) struct VirtualFontGroupMember {
    /// IDENTITY of the referenced real font (`FontEntry::render_identity_name`).
    pub font: String,
    /// Optional per-group display alias. Absent/`None` means "use the font's own label".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

/// A user-defined VIRTUAL font group: a named, ordered set of real fonts referenced by
/// identity. Virtual groups exist purely in config (no real files on disk), unlike folder
/// groups discovered under `fonts/groups/`. Member order is user-significant (a `Vec`).
///
/// Used directly as BOTH the serde mirror and the decoded form (see [`VirtualFontGroupMember`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::tabs::typing) struct VirtualFontGroup {
    /// Group display name. Non-blank and unique case-insensitively across virtual groups
    /// (enforced by [`sanitize_virtual_groups`] on load/save and by the runtime store).
    #[serde(default)]
    pub name: String,
    /// Ordered group members; user order preserved.
    #[serde(default)]
    pub members: Vec<VirtualFontGroupMember>,
}

/// Serde mirror of the entire `fonts_data.json` document. Every field has a serde
/// default so a partial or future-version document still deserializes its known keys.
///
/// The two trailing fields are the READ-ONLY v1 mirror: they are parsed forever (a user may
/// open a years-old document) but never serialized again.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FontsDataFile {
    /// Schema version; see `FONTS_DATA_VERSION`. A newer version is warned about but the
    /// known fields are still parsed best-effort. `None` means the field was ABSENT, which
    /// is NOT the same as `0`: the version is then inferred from the payload shape by
    /// [`decode`], because reading a v2-shaped document as an empty v1 would destroy it on
    /// the next save. Always written by [`encode`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<u32>,
    /// Whether a deferred v1 → v2 re-key is still owed (see the file header). Written only
    /// when `true`, so a fully migrated document stays minimal.
    #[serde(default, skip_serializing_if = "is_false")]
    pending_migration: bool,
    /// Imported system fonts, keyed by name with a path hint (v2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    system_fonts: Vec<SystemFontFileEntry>,
    /// Per-font settings keyed by font IDENTITY (v2).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    fonts: BTreeMap<String, FontSettingsEntry>,
    /// User-defined virtual font groups. Sanitized on decode AND encode. Present in both
    /// schema versions; only the MEANING of `members[].font` changed (path → identity).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    virtual_groups: Vec<VirtualFontGroup>,
    /// v1 ONLY: user-imported system font FILE paths. Read forever, never written.
    #[serde(default, skip_serializing)]
    imported_system_fonts: Vec<String>,
    /// v1 ONLY: per-font settings keyed by the legacy PATH key. Read forever, never written.
    #[serde(default, skip_serializing)]
    font_settings: BTreeMap<String, FontSettingsEntry>,
}

/// One imported system font in the decoded runtime form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::tabs::typing) struct SystemFontRef {
    /// PostScript name (unsuffixed identity) of the font. EMPTY while a v1 document is
    /// still waiting for the deferred migration to learn the name.
    pub font: String,
    /// Last known file path. A HINT: accepted only when the file is still present and
    /// still claims `font` (see the file header).
    pub last_path: Option<PathBuf>,
}

/// Per-font settings in the decoded runtime form. Both fields are `None` when unset; a
/// record with two `None`s is dropped rather than stored.
#[derive(Debug, Clone, Default, PartialEq)]
pub(in crate::tabs::typing) struct FontSettingsRecord {
    /// User display-name override; blank values are normalized away on decode.
    pub display_name: Option<String>,
    /// The font's default parameter profile.
    pub profile: Option<Value>,
}

impl FontSettingsRecord {
    /// Whether the record carries nothing worth storing (both fields unset).
    #[must_use]
    pub(in crate::tabs::typing) fn is_empty(&self) -> bool {
        self.display_name.is_none() && self.profile.is_none()
    }
}

/// Decoded in-memory form of `fonts_data.json` consumed by `font_settings_store`.
///
/// This is the boundary type between disk I/O and the runtime store. `pending_migration`
/// marks a document that was read with the LEGACY v1 rules, i.e. one whose `fonts` keys,
/// `virtual_groups[].members[].font` values and `system_fonts[].font` values are still
/// path-derived and must be re-keyed once a font list exists.
#[derive(Debug, Clone, Default, PartialEq)]
pub(in crate::tabs::typing) struct FontsData {
    /// Imported system fonts, in stored order.
    pub system_fonts: Vec<SystemFontRef>,
    /// Per-font settings keyed by font IDENTITY (by legacy PATH key while
    /// `pending_migration` holds). Empty records are dropped.
    pub fonts: BTreeMap<String, FontSettingsRecord>,
    /// User-defined virtual font groups, sanitized (blank names/keys dropped, blank aliases
    /// normalized to `None`, duplicate members/groups removed, user order preserved).
    pub virtual_groups: Vec<VirtualFontGroup>,
    /// `true` while the deferred path → identity re-key is still owed: the document was read
    /// with the v1 (path-keyed) rules, or a previous re-key pass could not resolve every
    /// legacy reference and wrote the flag back so a later launch retries.
    pub pending_migration: bool,
}

/// Absolute (or fonts-dir-relative) path of the `fonts_data.json` document.
#[must_use]
pub(in crate::tabs::typing) fn data_path(fonts_dir: &Path) -> PathBuf {
    fonts_dir.join(FONTS_DATA_FILE_NAME)
}

/// Typed result of attempting to load `fonts_data.json`. The three cases must be handled
/// differently by the seeding logic: `Missing` is the normal first run (run the legacy
/// migration), `Loaded` carries a parsed document (use it as-is), and `Invalid` means the
/// file exists but is unreadable/malformed — it must be quarantined and treated as `Missing`
/// rather than degraded to empty, otherwise the next mutation would overwrite and destroy a
/// possibly-recoverable file.
#[derive(Debug)]
pub(in crate::tabs::typing) enum LoadOutcome {
    /// No `fonts_data.json` exists yet (normal first-run case).
    Missing,
    /// The document parsed successfully (best-effort for an unknown future version).
    Loaded {
        /// The decoded document.
        data: FontsData,
        /// Fingerprint of the exact bytes that were read — the caller's optimistic-concurrency
        /// baseline for its first save (see [`SaveBaseline`]).
        fingerprint: DocumentFingerprint,
    },
    /// The file exists but could not be read or parsed; the caller must quarantine it.
    Invalid,
}

/// The fingerprint/baseline vocabulary of the write guard. Both are OWNED by `doc_store`
/// and shared with `presets_store`: the "is the file still what I last read?" question, its
/// answer and the crash-safe write recipe are one mechanism, and two copies of it drift
/// (they already had). Re-exported under the historical names so every caller and test here
/// reads unchanged.
pub(in crate::tabs::typing) use super::doc_store::{DocumentFingerprint, SaveBaseline};

/// Why [`save_checked`] refused to (or could not) write.
#[derive(Debug)]
pub(in crate::tabs::typing) enum SaveError {
    /// Directory creation, serialization, or the atomic write itself failed. Carries a
    /// human-readable description including the path and the OS error.
    Io(String),
    /// The document on disk declares a schema version this build does not understand.
    /// Rewriting it as v2 would silently drop every field that version added, so the write
    /// is refused; the user keeps the newer file intact.
    NewerVersion {
        /// The version the on-disk document declares.
        found: u32,
    },
    /// The file no longer matches the caller's baseline — another instance of the app wrote
    /// it. Nothing was written.
    Conflict {
        /// The freshly parsed on-disk document, so the caller can MERGE it into its own
        /// state and retry. `None` when the conflicting file could not be parsed at all
        /// (then it must not be overwritten: it is the only copy of whatever it holds).
        disk: Option<Box<FontsData>>,
        /// Fingerprint of the conflicting on-disk bytes, i.e. the caller's new baseline
        /// once it has merged them in.
        fingerprint: DocumentFingerprint,
    },
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(f, "{message}"),
            Self::NewerVersion { found } => write!(
                f,
                "the document on disk declares schema version {found}, newer than the \
                 supported {FONTS_DATA_VERSION}; refusing to overwrite it (its extra fields \
                 would be lost)"
            ),
            Self::Conflict { disk, .. } => write!(
                f,
                "the document changed on disk since it was last read ({}); refusing to \
                 overwrite it",
                if disk.is_some() {
                    "another app instance wrote it"
                } else {
                    "and it can no longer be parsed"
                }
            ),
        }
    }
}

/// Result of trying to move a corrupt `fonts_data.json` out of the way.
#[derive(Debug)]
pub(in crate::tabs::typing) enum QuarantineOutcome {
    /// The corrupt document was RENAMED to `fonts_data.json.bad`; the original path is free
    /// and the next save may write it.
    Moved,
    /// The rename failed, but a COPY reached `fonts_data.json.bad`. The content is preserved,
    /// so overwriting the original is safe.
    Copied,
    /// Neither the rename nor the copy worked: the corrupt file is the ONLY copy of whatever
    /// the user had, so nothing may overwrite it.
    Failed {
        /// Why the rename failed.
        rename_error: String,
        /// Why the fallback copy failed.
        copy_error: String,
    },
}

/// 64-bit digest of `contents`; see `doc_store::fingerprint`, which owns the rule.
#[must_use]
fn document_fingerprint(contents: &str) -> DocumentFingerprint {
    super::doc_store::fingerprint(contents)
}

/// Loads `fonts_data.json` from `fonts_dir` into a typed [`LoadOutcome`]. A missing file is
/// `Missing`; a read or parse failure is `Invalid` (warned about, never silently emptied);
/// otherwise `Loaded` (a NEWER version is warned about and still parsed best-effort).
/// Never panics.
#[must_use]
pub(in crate::tabs::typing) fn load_outcome(fonts_dir: &Path) -> LoadOutcome {
    load_outcome_from_file(&data_path(fonts_dir))
}

/// Path-parameterized core of [`load_outcome`], split out so the read logic can be
/// unit-tested against a temp file instead of the real fonts directory.
fn load_outcome_from_file(path: &Path) -> LoadOutcome {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        // A missing file is the normal first-run case; anything else is a real read error.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return LoadOutcome::Missing,
        Err(err) => {
            crate::runtime_log::log_warn(format!(
                "typing: cannot read fonts_data.json; treating as corrupt (will quarantine). \
                 Path: {} Error: {err}",
                path.display()
            ));
            return LoadOutcome::Invalid;
        }
    };

    let file: FontsDataFile = match serde_json::from_str(&raw) {
        Ok(file) => file,
        Err(err) => {
            crate::runtime_log::log_warn(format!(
                "typing: malformed fonts_data.json; treating as corrupt (will quarantine). \
                 Path: {} Error: {err}",
                path.display()
            ));
            return LoadOutcome::Invalid;
        }
    };

    if file.version.is_some_and(|version| version > FONTS_DATA_VERSION) {
        // Forward compatible for READING: warn but keep the fields we understand. WRITING
        // over such a document is refused outright (`save_checked`), because a rewrite would
        // drop exactly the fields this branch could not parse.
        crate::runtime_log::log_warn(format!(
            "typing: fonts_data.json version {} is newer than the expected {}; parsing known \
             fields only, and this build will REFUSE to overwrite the file. Path: {}",
            file.version.unwrap_or_default(),
            FONTS_DATA_VERSION,
            path.display()
        ));
    }

    let data = decode(file);
    if data.pending_migration {
        crate::runtime_log::log_info(format!(
            "typing: fonts_data.json still owes the deferred path → identity re-key; it will \
             be re-keyed after the first font list is built and stays flagged until EVERY \
             legacy reference has resolved. Path: {}",
            path.display()
        ));
    }
    LoadOutcome::Loaded {
        data,
        fingerprint: document_fingerprint(&raw),
    }
}

/// Moves a corrupt `fonts_data.json` out of the way so the next mutation cannot overwrite —
/// and thereby destroy — a possibly-recoverable document.
///
/// Tries `rename` to `fonts_data.json.bad` first (overwriting an older quarantine); if that
/// fails, falls back to a `copy`, which preserves the content even when the original cannot
/// be unlinked. The outcome MUST be honored by the caller: on [`QuarantineOutcome::Failed`]
/// the corrupt file is still the only copy of the user's data, and persistence has to stay
/// off until it is dealt with.
pub(in crate::tabs::typing) fn quarantine_bad_file(fonts_dir: &Path) -> QuarantineOutcome {
    let path = data_path(fonts_dir);
    // `fonts_data.json` -> `fonts_data.json.bad`; `fs::rename` overwrites an older `.bad`.
    let bad = path.with_extension("json.bad");
    let rename_error = match fs::rename(&path, &bad) {
        Ok(()) => {
            crate::runtime_log::log_warn(format!(
                "typing: quarantined corrupt fonts_data.json to {}",
                bad.display()
            ));
            return QuarantineOutcome::Moved;
        }
        Err(err) => err.to_string(),
    };
    // The rename can fail while the bytes are perfectly readable (a cross-device `.bad`
    // target, a read-only directory entry, a Windows share lock). A copy is enough: it makes
    // a second, recoverable copy exist, which is the entire point of the quarantine.
    match fs::copy(&path, &bad) {
        Ok(_) => {
            crate::runtime_log::log_warn(format!(
                "typing: could not RENAME the corrupt fonts_data.json ({rename_error}); copied \
                 it to {} instead, so the original may be overwritten safely. Path: {}",
                bad.display(),
                path.display()
            ));
            QuarantineOutcome::Copied
        }
        Err(err) => {
            crate::runtime_log::log_error(format!(
                "typing: could not quarantine the corrupt fonts_data.json — neither rename \
                 ({rename_error}) nor copy ({err}) worked. It is the only copy of these \
                 settings, so saving per-font settings is DISABLED for this session; move or \
                 delete {} by hand to re-enable it.",
                path.display()
            ));
            QuarantineOutcome::Failed {
                rename_error,
                copy_error: err.to_string(),
            }
        }
    }
}

/// Sanitizes a list of virtual font groups, applied on BOTH decode and encode so the
/// on-disk and in-memory forms are always well-formed. Rules (order preserved throughout):
/// - drop groups whose trimmed name is empty;
/// - deduplicate groups by case-insensitive name (FIRST wins);
/// - within a group, drop members whose trimmed font key is empty;
/// - deduplicate members by font key within a group (FIRST wins);
/// - normalize blank/whitespace-only aliases to `None`.
///
/// Names, keys, and aliases are trimmed. Round-trip is lossless for already-sane data.
/// Member keys are compared VERBATIM here (not case-folded): identity case folding belongs
/// to the resolution side (`fonts::normalize_font_identity`), and folding it here would
/// silently drop a member a future rename might still distinguish.
#[must_use]
fn sanitize_virtual_groups(groups: Vec<VirtualFontGroup>) -> Vec<VirtualFontGroup> {
    let mut seen_names: HashSet<String> = HashSet::new();
    let mut out: Vec<VirtualFontGroup> = Vec::with_capacity(groups.len());
    for group in groups {
        let name = group.name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        // Case-insensitive group-name dedup; first occurrence wins.
        if !seen_names.insert(name.to_lowercase()) {
            continue;
        }
        let mut seen_keys: HashSet<String> = HashSet::new();
        let mut members: Vec<VirtualFontGroupMember> = Vec::with_capacity(group.members.len());
        for member in group.members {
            let font = member.font.trim().to_string();
            if font.is_empty() {
                continue;
            }
            // Duplicate member keys within one group collapse to the first entry.
            if !seen_keys.insert(font.clone()) {
                continue;
            }
            let alias = member
                .alias
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty());
            members.push(VirtualFontGroupMember { font, alias });
        }
        out.push(VirtualFontGroup { name, members });
    }
    out
}

/// Normalizes one decoded per-font settings record: a blank display-name override behaves
/// exactly like "no override", so it is dropped rather than stored.
fn decode_settings_entry(entry: FontSettingsEntry) -> FontSettingsRecord {
    FontSettingsRecord {
        display_name: entry
            .display_name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty()),
        profile: entry.profile,
    }
}

/// Converts a decoded settings map, dropping records that carry nothing.
fn decode_settings_map(
    entries: BTreeMap<String, FontSettingsEntry>,
) -> BTreeMap<String, FontSettingsRecord> {
    entries
        .into_iter()
        .filter_map(|(key, entry)| {
            let key = key.trim().to_string();
            if key.is_empty() {
                return None;
            }
            let record = decode_settings_entry(entry);
            (!record.is_empty()).then_some((key, record))
        })
        .collect()
}

/// Converts the serde mirror into the decoded runtime form, UNIONING both schema payloads.
///
/// Neither payload is ever discarded: a document that carries v2 keys AND leftover v1 keys
/// (a hand edit, a half-written file, a partially migrated one) keeps both, with the v2 form
/// winning on a key clash. The v1 half is what raises `pending_migration`.
///
/// VERSION INFERENCE. The `version` field decides when it is present. When it is ABSENT the
/// PAYLOAD decides: a document carrying `system_fonts`/`fonts` is v2. The old rule — serde's
/// `0` default, therefore "≤ 1", therefore v1 — read such a document as an EMPTY v1 and the
/// next save wrote that emptiness back, destroying every identity-keyed setting in it. With
/// nothing to go on at all (no version, no payload) the document is treated as legacy, which
/// is the harmless direction: a pending migration only ever re-keys and never drops.
fn decode(file: FontsDataFile) -> FontsData {
    let has_v2_payload = !file.system_fonts.is_empty() || !file.fonts.is_empty();
    let has_legacy_payload =
        !file.imported_system_fonts.is_empty() || !file.font_settings.is_empty();
    let declares_legacy = match file.version {
        Some(version) => version <= LEGACY_FONTS_DATA_VERSION,
        None => !has_v2_payload,
    };

    let mut system_fonts: Vec<SystemFontRef> = file
        .system_fonts
        .into_iter()
        .filter_map(|entry| {
            let font = entry.font.trim().to_string();
            let last_path = entry
                .last_path
                .map(|raw| raw.trim().to_string())
                .filter(|raw| !raw.is_empty())
                .map(PathBuf::from);
            // An entry naming neither a font nor a file references nothing at all.
            (!font.is_empty() || last_path.is_some()).then_some(SystemFontRef { font, last_path })
        })
        .collect();
    // v1 imported fonts are bare FILE PATHS with no name; a path a v2 entry already points at
    // is the same font recorded twice, so it is not added again.
    for raw in file.imported_system_fonts {
        let raw = raw.trim().to_string();
        if raw.is_empty() {
            continue;
        }
        let path = PathBuf::from(raw);
        if system_fonts
            .iter()
            .any(|entry| entry.last_path.as_deref() == Some(path.as_path()))
        {
            continue;
        }
        system_fonts.push(SystemFontRef {
            // The name is unknown until a loader parses the file; the migration fills it in.
            font: String::new(),
            last_path: Some(path),
        });
    }

    let mut fonts = decode_settings_map(file.fonts);
    for (key, record) in decode_settings_map(file.font_settings) {
        // The v2 form wins: a key present in both was already migrated.
        fonts.entry(key).or_insert(record);
    }

    FontsData {
        system_fonts,
        fonts,
        virtual_groups: sanitize_virtual_groups(file.virtual_groups),
        // A v2 document carries the flag explicitly (a migration that could not resolve
        // everything writes it back), so it survives the rewrite the migration itself
        // performs — without that, the next launch would read a v2 document, never retry,
        // and freeze the unresolved keys forever.
        pending_migration: declares_legacy || has_legacy_payload || file.pending_migration,
    }
}

/// Atomically writes a full snapshot of `data` to `fonts_data.json` in `fonts_dir`,
/// creating the fonts directory if it does not yet exist. Always writes schema
/// `FONTS_DATA_VERSION`, never the legacy form.
///
/// `baseline` is the state the caller believes the file is in; the write is refused when
/// reality disagrees (see [`SaveBaseline`] and [`SaveError`]). On success, returns the
/// fingerprint of the bytes just written — the caller's new baseline.
///
/// # Errors
/// [`SaveError::Io`] on directory creation, serialization, or atomic-write failure;
/// [`SaveError::NewerVersion`] when the on-disk document is from a future schema;
/// [`SaveError::Conflict`] when the file changed since `baseline`. Callers persist off the
/// GUI thread.
pub(in crate::tabs::typing) fn save_checked(
    fonts_dir: &Path,
    data: &FontsData,
    baseline: SaveBaseline,
) -> Result<DocumentFingerprint, SaveError> {
    // Create the fonts dir on demand so a first-ever save (e.g. one-time migration)
    // succeeds even when the app runs before any font is present.
    if let Err(err) = fs::create_dir_all(fonts_dir) {
        return Err(SaveError::Io(format!(
            "cannot create fonts directory {}: {err}",
            fonts_dir.display()
        )));
    }
    save_to_file(&data_path(fonts_dir), data, baseline)
}

/// Path-parameterized core of [`save_checked`], split out so the write recipe and its guard
/// can be unit-tested against a temp file. Assumes the parent directory already exists.
fn save_to_file(
    path: &Path,
    data: &FontsData,
    baseline: SaveBaseline,
) -> Result<DocumentFingerprint, SaveError> {
    guard_existing_document(path, baseline)?;
    let file = encode(data);
    let mut text = serde_json::to_string_pretty(&file)
        .map_err(|err| SaveError::Io(format!("cannot serialize fonts_data.json: {err}")))?;
    text.push('\n');
    let fingerprint = document_fingerprint(&text);
    write_atomic(path, &text).map_err(SaveError::Io)?;
    Ok(fingerprint)
}

/// Inspects the document currently at `path` and decides whether it may be replaced.
///
/// Two things make a replacement unacceptable, and both are silent data loss if allowed:
/// a document from a FUTURE schema (whose unknown fields this build cannot round-trip —
/// see the "choose one" note below), and a document that changed since the caller's
/// `baseline` (a second running app instance wrote it; overwriting drops whatever it added).
///
/// WHY REFUSING RATHER THAN PRESERVING UNKNOWN FIELDS. Carrying unknown keys through a
/// `#[serde(flatten)]` bag would let this build stamp `"version": 2` onto a payload whose
/// other half is v99 — a document that is neither, and whose unknown fields may reference
/// the very keys this build re-keys during migration. Refusing keeps the newer file exactly
/// as its writer left it, which is the only outcome that cannot corrupt it. The cost is that
/// settings changed in this session are not persisted, which is why the refusal is reported
/// as an error rather than swallowed.
///
/// A file that is ABSENT never blocks a write: there is nothing to lose.
fn guard_existing_document(path: &Path, baseline: SaveBaseline) -> Result<(), SaveError> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        // Nothing on disk: any baseline may proceed (a `Matching` baseline whose file
        // vanished has nothing left to preserve).
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(SaveError::Io(format!(
                "cannot read the existing {} before replacing it: {err}",
                path.display()
            )));
        }
    };
    let fingerprint = document_fingerprint(&raw);
    let parsed: Option<FontsDataFile> = serde_json::from_str(&raw).ok();
    if let Some(found) = parsed
        .as_ref()
        .and_then(|file| file.version)
        .filter(|version| *version > FONTS_DATA_VERSION)
    {
        return Err(SaveError::NewerVersion { found });
    }
    if baseline.accepts(fingerprint) {
        return Ok(());
    }
    Err(SaveError::Conflict {
        disk: parsed.map(|file| Box::new(decode(file))),
        fingerprint,
    })
}

/// Converts the decoded runtime form into the serde mirror for serialization, stamping the
/// current schema version. Records and fields carrying no value are dropped so the document
/// stays minimal (the "JSON slimming" rule of the identity plan).
fn encode(data: &FontsData) -> FontsDataFile {
    let system_fonts = data
        .system_fonts
        .iter()
        .map(|entry| SystemFontFileEntry {
            font: entry.font.clone(),
            last_path: entry
                .last_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        })
        .collect();
    let fonts = data
        .fonts
        .iter()
        .filter(|(_, record)| !record.is_empty())
        .map(|(key, record)| {
            (
                key.clone(),
                FontSettingsEntry {
                    display_name: record.display_name.clone(),
                    profile: record.profile.clone(),
                },
            )
        })
        .collect();
    FontsDataFile {
        version: Some(FONTS_DATA_VERSION),
        // Persisted so an INCOMPLETE deferred migration survives the rewrite it triggers;
        // see the file header.
        pending_migration: data.pending_migration,
        system_fonts,
        fonts,
        virtual_groups: sanitize_virtual_groups(data.virtual_groups.clone()),
        // The v1 mirror is never written back.
        imported_system_fonts: Vec::new(),
        font_settings: BTreeMap::new(),
    }
}

/// Atomically replaces `path` with `contents` through the shared `doc_store` recipe (sibling
/// temp + `write_all` + `sync_all` + close + `rename`).
///
/// `fonts_data.json` asks for [`doc_store::Durability::Contents`] only: nothing deletes a
/// data source once this returns (the legacy `user_config` keys are dropped by
/// `presets_store`, which uses the directory-durable mode), and a lost directory-entry flush
/// at worst loses a brand-new file that the next mutation rewrites. The write is frequent
/// (every debounced profile edit), so the extra directory fsync would be paid per keystroke
/// for a guarantee this document does not need.
fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    super::doc_store::write_atomic(path, contents, super::doc_store::Durability::Contents)
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Unique temp path so parallel tests never share a file.
    fn unique_temp_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("ms_fonts_data_{tag}_{nanos}.json"))
    }

    /// Unwraps a `Loaded` outcome or panics with a message naming the actual variant.
    fn expect_loaded(outcome: LoadOutcome) -> FontsData {
        match outcome {
            LoadOutcome::Loaded { data, .. } => data,
            LoadOutcome::Missing => panic!("expected Loaded, got Missing"),
            LoadOutcome::Invalid => panic!("expected Loaded, got Invalid"),
        }
    }

    /// Convenience constructor for an imported system font with a path hint.
    fn system_font(font: &str, last_path: &str) -> SystemFontRef {
        SystemFontRef {
            font: font.to_string(),
            last_path: Some(PathBuf::from(last_path)),
        }
    }

    /// Convenience constructor for a display-name-only settings record.
    fn named(display_name: &str) -> FontSettingsRecord {
        FontSettingsRecord {
            display_name: Some(display_name.to_string()),
            profile: None,
        }
    }

    #[test]
    fn round_trip_v2_through_temp_file() {
        let path = unique_temp_path("roundtrip_v2");
        let mut fonts = BTreeMap::new();
        fonts.insert("CCWildWordsLower-Regular".to_string(), named("Разговор"));
        fonts.insert(
            "kCCAskForMercy-Regular".to_string(),
            FontSettingsRecord {
                display_name: None,
                profile: Some(serde_json::json!({ "schema": 2, "font_size_px": 42.0 })),
            },
        );
        let data = FontsData {
            system_fonts: vec![
                system_font("Roboto-Medium", "/usr/share/fonts/Roboto-Medium.ttf"),
                SystemFontRef {
                    font: "NotoSans-Regular".to_string(),
                    last_path: None,
                },
            ],
            fonts,
            virtual_groups: vec![VirtualFontGroup {
                name: "Возлюбленная".to_string(),
                members: vec![member_alias("kCCAskForMercy-Regular", "Основа")],
            }],
            pending_migration: false,
        };
        save_to_file(&path, &data, SaveBaseline::Unchecked).expect("save must succeed");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert_eq!(loaded, data, "a v2 document must round-trip verbatim");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn saved_document_declares_v2_and_omits_unset_fields() {
        let path = unique_temp_path("slim_v2");
        let mut fonts = BTreeMap::new();
        fonts.insert("Comic-Regular".to_string(), named("Разговор"));
        let data = FontsData {
            system_fonts: Vec::new(),
            fonts,
            virtual_groups: vec![VirtualFontGroup {
                name: "G".to_string(),
                members: vec![member("Comic-Regular")],
            }],
            pending_migration: false,
        };
        save_to_file(&path, &data, SaveBaseline::Unchecked).expect("save must succeed");
        let raw = fs::read_to_string(&path).expect("read back");
        assert!(raw.contains("\"version\": 2"), "the document must declare v2");
        // Unset optionals and empty collections are omitted, never written as null/[].
        assert!(!raw.contains("profile"), "an unset profile must be omitted");
        assert!(!raw.contains("alias"), "an unset alias must be omitted");
        assert!(!raw.contains("last_path"), "no system fonts -> no path hints");
        assert!(
            !raw.contains("system_fonts"),
            "an empty system-font list must be omitted"
        );
        // The legacy mirror is never written back.
        assert!(!raw.contains("imported_system_fonts"));
        assert!(!raw.contains("font_settings"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_missing_outcome() {
        let path = unique_temp_path("missing");
        // Never created: load must report Missing (first run), not panic or Invalid.
        assert!(matches!(load_outcome_from_file(&path), LoadOutcome::Missing));
    }

    #[test]
    fn malformed_file_is_invalid_outcome() {
        let path = unique_temp_path("malformed");
        fs::write(&path, "{ this is : not json").expect("write malformed");
        // A corrupt file must be Invalid (so the caller quarantines it), NOT silently empty.
        assert!(matches!(load_outcome_from_file(&path), LoadOutcome::Invalid));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn unknown_version_still_parses_known_fields() {
        let path = unique_temp_path("future_version");
        // A future version with known v2 fields present must still yield those fields.
        let raw = r#"{
            "version": 99,
            "system_fonts": [ { "font": "A-Regular", "last_path": "/x/A.ttf" } ],
            "fonts": { "B-Regular": { "display_name": "Name" } },
            "unknown_future_key": 123
        }"#;
        fs::write(&path, raw).expect("write future-version doc");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert!(!loaded.pending_migration, "a v2-shaped document needs no migration");
        assert_eq!(
            loaded.system_fonts,
            vec![system_font("A-Regular", "/x/A.ttf")]
        );
        assert_eq!(
            loaded
                .fonts
                .get("B-Regular")
                .and_then(|record| record.display_name.as_deref()),
            Some("Name")
        );
        let _ = fs::remove_file(&path);
    }

    /// The user's REAL v1 document shape: one imported system font, path-keyed settings and
    /// two virtual groups whose members are path keys. Everything must survive the decode
    /// verbatim and be flagged for the deferred re-key.
    #[test]
    fn legacy_v1_document_decodes_verbatim_and_is_flagged_pending() {
        let path = unique_temp_path("legacy_v1");
        let raw = r#"{
            "version": 1,
            "imported_system_fonts": ["/home/u/.fonts/Roboto-Medium.ttf"],
            "font_settings": { "groups/ВВД/Мысли.ttf": { "display_name": "Мысли" } },
            "virtual_groups": [
                { "name": "Возлюбленная",
                  "members": [ { "font": "groups/ВВД/Основа.ttf", "alias": "Основа" },
                               { "font": "/home/u/.fonts/Roboto-Medium.ttf", "alias": "Сис" } ] },
                { "name": "Экшн", "members": [ { "font": "Comic.otf" } ] }
            ]
        }"#;
        fs::write(&path, raw).expect("write v1 doc");
        let loaded = expect_loaded(load_outcome_from_file(&path));

        assert!(loaded.pending_migration, "a v1 document must be flagged pending");
        assert_eq!(
            loaded.system_fonts,
            vec![SystemFontRef {
                // The name is not knowable from a v1 document; the migration learns it.
                font: String::new(),
                last_path: Some(PathBuf::from("/home/u/.fonts/Roboto-Medium.ttf")),
            }]
        );
        assert_eq!(
            loaded
                .fonts
                .get("groups/ВВД/Мысли.ttf")
                .and_then(|record| record.display_name.as_deref()),
            Some("Мысли"),
            "the legacy path key survives decode untouched"
        );
        assert_eq!(loaded.virtual_groups.len(), 2);
        assert_eq!(loaded.virtual_groups[0].members.len(), 2);
        assert_eq!(
            loaded.virtual_groups[0].members[1].font,
            "/home/u/.fonts/Roboto-Medium.ttf"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn v2_declaring_document_with_only_legacy_payload_is_read_as_legacy() {
        let path = unique_temp_path("legacy_payload_v2_header");
        // A half-written / hand-edited file: it claims v2 but carries only the v1 payload.
        // Reading it as an EMPTY v2 document would discard the user's data on the next save.
        let raw = r#"{
            "version": 2,
            "imported_system_fonts": ["/x/A.ttf"],
            "font_settings": { "B.ttf": { "display_name": "Name" } }
        }"#;
        fs::write(&path, raw).expect("write mixed doc");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert!(loaded.pending_migration);
        assert_eq!(loaded.system_fonts.len(), 1);
        assert!(loaded.fonts.contains_key("B.ttf"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn blank_override_is_dropped_on_load() {
        let path = unique_temp_path("blank_override");
        let raw = r#"{
            "version": 2,
            "fonts": { "A-Regular": { "display_name": "   " } }
        }"#;
        fs::write(&path, raw).expect("write blank override");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert!(
            loaded.fonts.is_empty(),
            "a record left with nothing but a whitespace-only override must not survive load"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn empty_string_paths_are_skipped_on_load() {
        let path = unique_temp_path("empty_paths");
        let raw = r#"{
            "version": 1,
            "imported_system_fonts": ["/x/A.ttf", ""],
            "font_settings": {}
        }"#;
        fs::write(&path, raw).expect("write doc with empty path");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert_eq!(
            loaded.system_fonts,
            vec![SystemFontRef {
                font: String::new(),
                last_path: Some(PathBuf::from("/x/A.ttf")),
            }]
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn profile_round_trips_verbatim() {
        let path = unique_temp_path("profile_roundtrip");
        let profile = serde_json::json!({
            "schema": 2,
            "font": "Comic-Regular",
            "font_size_px": 42.5,
            "effects": [ { "kind": "stroke", "width_px": 3 } ]
        });
        let mut fonts = BTreeMap::new();
        fonts.insert(
            "Comic-Regular".to_string(),
            FontSettingsRecord {
                display_name: None,
                profile: Some(profile.clone()),
            },
        );
        let data = FontsData {
            system_fonts: Vec::new(),
            fonts,
            virtual_groups: Vec::new(),
            pending_migration: false,
        };
        save_to_file(&path, &data, SaveBaseline::Unchecked).expect("save must succeed");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert_eq!(
            loaded.fonts.get("Comic-Regular").and_then(|r| r.profile.clone()),
            Some(profile),
            "a stored default profile must come back byte-for-byte equal"
        );
        let _ = fs::remove_file(&path);
    }

    /// Convenience constructor for a member with no alias.
    fn member(font: &str) -> VirtualFontGroupMember {
        VirtualFontGroupMember {
            font: font.to_string(),
            alias: None,
        }
    }

    /// Convenience constructor for a member with an alias.
    fn member_alias(font: &str, alias: &str) -> VirtualFontGroupMember {
        VirtualFontGroupMember {
            font: font.to_string(),
            alias: Some(alias.to_string()),
        }
    }

    #[test]
    fn virtual_groups_round_trip_with_aliases_and_order() {
        let path = unique_temp_path("vgroups_roundtrip");
        let groups = vec![
            VirtualFontGroup {
                name: "Экшн".to_string(),
                members: vec![
                    member_alias("MangaBold-Regular", "Жирный"),
                    member("Comic-Regular"),
                ],
            },
            VirtualFontGroup {
                name: "Диалоги".to_string(),
                members: vec![member("NotoSans-Regular")],
            },
        ];
        let data = FontsData {
            system_fonts: Vec::new(),
            fonts: BTreeMap::new(),
            virtual_groups: groups.clone(),
            pending_migration: false,
        };
        save_to_file(&path, &data, SaveBaseline::Unchecked).expect("save must succeed");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        // Group AND member order must survive the round-trip verbatim.
        assert_eq!(loaded.virtual_groups, groups);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn old_document_without_virtual_groups_loads_empty() {
        let path = unique_temp_path("vgroups_absent");
        let raw = r#"{
            "version": 1,
            "imported_system_fonts": [],
            "font_settings": {}
        }"#;
        fs::write(&path, raw).expect("write doc without virtual_groups");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert!(
            loaded.virtual_groups.is_empty(),
            "a document predating virtual groups must load with an empty vec"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn unknown_extra_json_fields_in_virtual_groups_still_parse() {
        let path = unique_temp_path("vgroups_extra_fields");
        // Unknown keys at the document and group/member level must be ignored, not fail.
        let raw = r#"{
            "version": 2,
            "virtual_groups": [
                {
                    "name": "G1",
                    "members": [ { "font": "A-Regular", "future_flag": true } ],
                    "future_group_key": 42
                }
            ],
            "unknown_future_key": 123
        }"#;
        fs::write(&path, raw).expect("write doc with extra fields");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert_eq!(loaded.virtual_groups.len(), 1);
        assert_eq!(loaded.virtual_groups[0].name, "G1");
        assert_eq!(loaded.virtual_groups[0].members, vec![member("A-Regular")]);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn sanitize_drops_blank_names_keys_and_dedups() {
        let input = vec![
            // Blank name -> dropped entirely.
            VirtualFontGroup {
                name: "   ".to_string(),
                members: vec![member("A-Regular")],
            },
            VirtualFontGroup {
                name: "  Keep  ".to_string(),
                members: vec![
                    member(""),                            // blank key -> dropped
                    member_alias("A-Regular", "   "),      // blank alias -> None
                    member("A-Regular"),                   // duplicate key -> dropped (first wins)
                    member_alias("B-Regular", "  Bee  "),  // alias trimmed
                ],
            },
            // Case-insensitive duplicate of "Keep" -> dropped (first wins).
            VirtualFontGroup {
                name: "keep".to_string(),
                members: vec![member("C-Regular")],
            },
        ];
        let out = sanitize_virtual_groups(input);
        assert_eq!(out.len(), 1, "blank + duplicate groups must be dropped");
        let group = &out[0];
        assert_eq!(group.name, "Keep", "the name must be trimmed");
        assert_eq!(
            group.members,
            vec![
                // First "A-Regular" survives with its blank alias normalized to None.
                member("A-Regular"),
                member_alias("B-Regular", "Bee"),
            ]
        );
    }

    /// Unique temp DIRECTORY so a test that needs the `.bad` sibling never collides.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("ms_fonts_data_{tag}_{nanos}"))
    }

    /// DEFECT 5. A document with a perfectly good v2 payload but NO `version` field must be
    /// read as v2. Serde's `0` default made it "≤ 1", i.e. legacy, and the legacy decoder read
    /// only the v1 keys — so the document came back EMPTY and the next save wrote that
    /// emptiness over the user's imported fonts, overrides, profiles and groups.
    #[test]
    fn a_versionless_v2_document_is_not_read_as_an_empty_legacy_one() {
        let path = unique_temp_path("versionless_v2");
        let raw = r#"{
            "system_fonts": [ { "font": "Roboto-Medium", "last_path": "/x/Roboto-Medium.ttf" } ],
            "fonts": { "Comic-Regular": { "display_name": "Разговор" } },
            "virtual_groups": [ { "name": "Экшн",
                                  "members": [ { "font": "Comic-Regular", "alias": "Крик" } ] } ]
        }"#;
        fs::write(&path, raw).expect("write versionless v2 doc");
        let loaded = expect_loaded(load_outcome_from_file(&path));

        assert!(
            !loaded.pending_migration,
            "a v2-shaped payload is v2 even without the version field"
        );
        assert_eq!(
            loaded.system_fonts,
            vec![system_font("Roboto-Medium", "/x/Roboto-Medium.ttf")],
            "the imported system font must survive"
        );
        assert_eq!(
            loaded
                .fonts
                .get("Comic-Regular")
                .and_then(|record| record.display_name.as_deref()),
            Some("Разговор"),
            "the display-name override must survive"
        );
        assert_eq!(loaded.virtual_groups.len(), 1);
        assert_eq!(loaded.virtual_groups[0].members.len(), 1);
        let _ = fs::remove_file(&path);
    }

    /// A document that carries BOTH payload shapes (a half-migrated or hand-edited file)
    /// loses neither half.
    #[test]
    fn a_mixed_v1_and_v2_document_keeps_both_payloads() {
        let path = unique_temp_path("mixed_payloads");
        let raw = r#"{
            "version": 2,
            "fonts": { "Comic-Regular": { "display_name": "Новый" } },
            "font_settings": { "groups/A/Old.ttf": { "display_name": "Старый" } },
            "imported_system_fonts": ["/x/Legacy.ttf"]
        }"#;
        fs::write(&path, raw).expect("write mixed doc");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert!(
            loaded.pending_migration,
            "the leftover legacy half still owes the re-key"
        );
        assert!(loaded.fonts.contains_key("Comic-Regular"));
        assert!(loaded.fonts.contains_key("groups/A/Old.ttf"));
        assert_eq!(loaded.system_fonts.len(), 1);
        let _ = fs::remove_file(&path);
    }

    /// DEFECT 1 (persistence half). A migration that could not resolve everything rewrites the
    /// document in the CURRENT schema, so the pending flag has to travel WITH it — otherwise
    /// the next launch reads a v2 document, never retries, and the unresolved keys are frozen
    /// while the log promises they "will apply again".
    #[test]
    fn a_pending_migration_survives_the_rewrite_it_triggers() {
        let path = unique_temp_path("pending_roundtrip");
        let mut fonts = BTreeMap::new();
        fonts.insert("groups/ВВД/Основа.ttf".to_string(), named("Основа"));
        let data = FontsData {
            system_fonts: Vec::new(),
            fonts,
            virtual_groups: Vec::new(),
            pending_migration: true,
        };
        save_to_file(&path, &data, SaveBaseline::Unchecked).expect("save must succeed");
        let raw = fs::read_to_string(&path).expect("read back");
        assert!(raw.contains("\"version\": 2"), "it is written as v2");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert!(
            loaded.pending_migration,
            "the rewritten document must still ask for the deferred re-key"
        );
        assert!(loaded.fonts.contains_key("groups/ВВД/Основа.ttf"));
        let _ = fs::remove_file(&path);
    }

    /// A FINISHED migration writes no flag at all, so a normal document stays minimal.
    #[test]
    fn a_finished_migration_writes_no_pending_flag() {
        let path = unique_temp_path("pending_absent");
        let mut fonts = BTreeMap::new();
        fonts.insert("Comic-Regular".to_string(), named("Разговор"));
        let data = FontsData {
            system_fonts: Vec::new(),
            fonts,
            virtual_groups: Vec::new(),
            pending_migration: false,
        };
        save_to_file(&path, &data, SaveBaseline::Unchecked).expect("save must succeed");
        let raw = fs::read_to_string(&path).expect("read back");
        assert!(!raw.contains("pending_migration"));
        let _ = fs::remove_file(&path);
    }

    /// DEFECT 6. A document from a FUTURE schema must never be rewritten as v2: everything the
    /// newer version added (here `font_collections`) would silently disappear. Reading it
    /// best-effort stays allowed; writing over it does not.
    #[test]
    fn a_newer_version_document_is_never_overwritten() {
        let path = unique_temp_path("future_version_write");
        let raw = r#"{
            "version": 99,
            "fonts": { "Comic-Regular": { "display_name": "Разговор" } },
            "font_collections": [ { "name": "Set", "fonts": ["Comic-Regular"] } ]
        }"#;
        fs::write(&path, raw).expect("write future-version doc");

        let mut fonts = BTreeMap::new();
        fonts.insert("Comic-Regular".to_string(), named("Renamed"));
        let data = FontsData {
            system_fonts: Vec::new(),
            fonts,
            virtual_groups: Vec::new(),
            pending_migration: false,
        };
        let error = save_to_file(&path, &data, SaveBaseline::Unchecked)
            .expect_err("writing over a newer schema must be refused");
        assert!(
            matches!(error, SaveError::NewerVersion { found: 99 }),
            "the refusal must name the version it found: {error:?}"
        );
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("font_collections"),
            "the newer document must be left exactly as its writer left it"
        );
        let _ = fs::remove_file(&path);
    }

    /// DEFECT 10. Instance A adds group G1 and writes; instance B, holding a snapshot from
    /// before that write, must NOT be able to rename its own document over it. The conflict
    /// is reported together with the on-disk content, which is what lets the caller merge.
    #[test]
    fn a_write_by_another_instance_is_detected_instead_of_clobbered() {
        let path = unique_temp_path("concurrent_write");
        let empty = FontsData::default();
        // Both instances start from the same document.
        let shared = save_to_file(&path, &empty, SaveBaseline::Unchecked).expect("initial save");

        // Instance A adds G1 and saves against the shared baseline.
        let a = FontsData {
            virtual_groups: vec![VirtualFontGroup {
                name: "G1".to_string(),
                members: vec![member("A-Regular")],
            }],
            ..FontsData::default()
        };
        save_to_file(&path, &a, SaveBaseline::Matching(shared)).expect("instance A saves");

        // Instance B still believes the file is the shared one, and adds G2.
        let b = FontsData {
            virtual_groups: vec![VirtualFontGroup {
                name: "G2".to_string(),
                members: vec![member("B-Regular")],
            }],
            ..FontsData::default()
        };
        let error = save_to_file(&path, &b, SaveBaseline::Matching(shared))
            .expect_err("a stale baseline must not overwrite");
        match error {
            SaveError::Conflict { disk, .. } => {
                let disk = disk.expect("the conflicting document parsed, so it is handed back");
                assert_eq!(
                    disk.virtual_groups
                        .iter()
                        .map(|group| group.name.as_str())
                        .collect::<Vec<_>>(),
                    vec!["G1"],
                    "the caller is handed exactly what the other instance wrote"
                );
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
        let after = fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("G1") && !after.contains("G2"),
            "instance A's group must still be on disk, untouched"
        );
        let _ = fs::remove_file(&path);
    }

    /// The baseline is not a blanket lock: a writer that is up to date replaces the file.
    #[test]
    fn a_matching_baseline_replaces_the_document() {
        let path = unique_temp_path("baseline_ok");
        let first = save_to_file(&path, &FontsData::default(), SaveBaseline::Unchecked)
            .expect("initial save");
        let data = FontsData {
            virtual_groups: vec![VirtualFontGroup {
                name: "G".to_string(),
                members: Vec::new(),
            }],
            ..FontsData::default()
        };
        let second =
            save_to_file(&path, &data, SaveBaseline::Matching(first)).expect("in-sync save");
        assert_ne!(first, second, "the new bytes get a new fingerprint");
        let after = fs::read_to_string(&path).expect("read back");
        assert!(after.contains("\"G\""));
        let _ = fs::remove_file(&path);
    }

    /// DUPLICATE JSON KEYS, part 1: a duplicated DOCUMENT-LEVEL field is a hard parse error
    /// for serde's derived struct reader ("duplicate field"), so the document is `Invalid`.
    ///
    /// That is the safe outcome and is pinned deliberately: `Invalid` means quarantine +
    /// first-run, never "read as empty and overwrite", so a hand-edited or concatenated file
    /// keeps its content in `fonts_data.json.bad` instead of being silently destroyed.
    #[test]
    fn a_duplicated_document_level_key_is_invalid_not_silently_merged() {
        let path = unique_temp_path("duplicate_doc_keys");
        let raw = r#"{
            "version": 2,
            "fonts": { "Comic-Regular": { "display_name": "первый" } },
            "fonts": { "Comic-Regular": { "display_name": "второй" } }
        }"#;
        fs::write(&path, raw).expect("write doc with a duplicated field");
        assert!(
            matches!(load_outcome_from_file(&path), LoadOutcome::Invalid),
            "a duplicated struct field must be reported as corrupt, so the file is \
             quarantined rather than treated as empty"
        );
        let _ = fs::remove_file(&path);
    }

    /// DUPLICATE JSON KEYS, part 2: a duplicated key INSIDE a map (`fonts`, and the same for
    /// a settings record's own fields) is not an error — the LAST occurrence wins. Pinned
    /// because "which value survives" decides which of the user's settings is kept.
    #[test]
    fn a_duplicated_map_key_resolves_to_the_last_occurrence() {
        let path = unique_temp_path("duplicate_map_keys");
        let raw = r#"{
            "version": 2,
            "fonts": {
                "Comic-Regular": { "display_name": "первый" },
                "Comic-Regular": { "display_name": "второй" }
            }
        }"#;
        fs::write(&path, raw).expect("write doc with a duplicated map key");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert_eq!(loaded.fonts.len(), 1, "the two entries collapse into one");
        assert_eq!(
            loaded
                .fonts
                .get("Comic-Regular")
                .and_then(|record| record.display_name.as_deref()),
            Some("второй"),
            "the last object written for a duplicated map key wins"
        );
        let _ = fs::remove_file(&path);
    }

    /// DEFECT 9. When the corrupt document can be neither renamed nor copied aside, the
    /// quarantine must SAY so — the caller has to disable persistence, because that file is
    /// the only copy of the user's settings and the next save would rename over it.
    #[test]
    fn a_quarantine_that_cannot_move_or_copy_reports_failure() {
        let dir = unique_temp_dir("quarantine_fail");
        fs::create_dir_all(&dir).expect("create temp dir");
        let path = data_path(&dir);
        fs::write(&path, "{ not json").expect("write corrupt file");
        // Make the `.bad` target a NON-EMPTY DIRECTORY: `rename` cannot replace it and
        // `copy` cannot write to it, on every platform we build for.
        let bad = path.with_extension("json.bad");
        fs::create_dir_all(&bad).expect("create the blocking directory");
        fs::write(bad.join("occupied"), b"x").expect("make it non-empty");

        let outcome = quarantine_bad_file(&dir);
        assert!(
            matches!(outcome, QuarantineOutcome::Failed { .. }),
            "neither rename nor copy can succeed here: {outcome:?}"
        );
        assert!(
            path.exists(),
            "the corrupt file must still be there — it is the only copy"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn quarantine_renames_corrupt_file_to_bad() {
        // Isolated temp dir so quarantine's fixed `.bad` sibling never collides.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("ms_fonts_data_quarantine_{nanos}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        let path = data_path(&dir);
        fs::write(&path, "{ not json").expect("write corrupt file");

        quarantine_bad_file(&dir);

        let bad = path.with_extension("json.bad");
        assert!(!path.exists(), "the corrupt file must be moved away");
        assert!(bad.exists(), "the corrupt file must land at fonts_data.json.bad");
        let _ = fs::remove_dir_all(&dir);
    }
}
