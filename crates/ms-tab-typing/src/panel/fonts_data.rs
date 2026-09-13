/*
File: panel/fonts_data.rs

Purpose:
Serde schema and disk I/O for the app-level per-font settings document
`fonts_data.json`, stored inside the app fonts directory (`resolve_fonts_dir()`).
This file is the single on-disk home for the user-imported system fonts, per-font
settings (display-name override + default parameter profile + user-defined custom kerning
pairs) and user-defined VIRTUAL font groups. Font discovery never picks it up because it
only scans `.ttf/.otf/.ttc`.

SCHEMA VERSION 3 — the font is named by its IDENTITY, never by a path:

```jsonc
{
  "version": 3,
  "system_fonts": [ { "font": "Roboto-Medium", "last_path": "/home/…/Roboto-Medium.ttf" } ],
  "fonts": { "CCWildWordsLower-Regular": {
               "display_name": "Разговор",
               "profile": { … },
               "custom_kerning": [ { "left": "A", "right": "V", "em": -40.0 } ] } },
  "virtual_groups": [ { "name": "Возлюбленная",
                        "members": [ { "font": "kCCAskForMercy-Regular", "alias": "Основа" } ] } ]
}
```

CUSTOM KERNING IS MEASURED IN THOUSANDTHS OF AN EM (`em`, ‰ em), never in font design
units. The renderer applies `delta_px = em / 1000.0 * font_size_px`, so the user's tuning is
independent of BOTH the rendered size and the font file's `units_per_em` — a font updated to
a different upem does not silently rescale it. `left`/`right` are single-character STRINGS; a
string that is not exactly one `char` is dropped on decode with a warning. A pair whose `em`
is `0.0` IS MEANINGFUL AND IS KEPT: it is how the user CANCELS a non-zero built-in pair, so
nothing here filters zeros out (the settings UI's built-in extractor drops them, which is a
different rule for a different list).

VERSION 3 IS A GATE, NOT A MIGRATION. `custom_kerning` is purely additive and a v2 (or v1)
document decodes exactly as before. The number was bumped because this document is REWRITTEN
IN FULL on every debounced profile edit and there is no `deny_unknown_fields`: an older build
would read a v3 document, silently drop the key it does not know, and write that loss back
within seconds. The bump makes such a build refuse the write (`SaveError::NewerVersion`) and
persist no font-settings change instead — the same trade `presets.json` version 2 makes.

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
map does not exist any earlier) and rewrites the document in the CURRENT schema. The
legacy keys are never written back.

THE SCHEMA VERSION IS DECIDED BY CONTENT WHEN THE `version` FIELD IS ABSENT. A document
carrying `system_fonts`/`fonts` but no `version` is an IDENTITY-KEYED (v2+) document (a hand
edit, a truncated write, an older writer); reading it as an empty v1 — which a `0` default
made it do — threw every key away on the next save. Both payload shapes are decoded and
UNIONED, so a document that somehow carries v1 AND identity-keyed keys loses neither.

`pending_migration` IS PART OF THE PERSISTED IDENTITY-KEYED PAYLOAD. A deferred migration
that could not resolve every legacy key rewrites the document in the CURRENT schema but must
stay pending, or the next launch would read an identity-keyed document, never retry, and the
unresolved keys would be frozen forever (the "will apply again" promise in the migration log
has to be true).

Main responsibilities:
- define the versioned JSON schema (`version: 3`) and its serde mirror, plus the read-only
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
- `CustomKerningPair` (one user-authored kerning override, in ‰ em; sanitized by
  `sanitize_custom_kerning` on load and save)
- `FontsData` (decoded in-memory form consumed by `font_settings_store`)
- `LoadOutcome` (Missing / Loaded / Invalid load result)
- `DocumentFingerprint` / `SaveBaseline` / `SaveError` (the write guard)
- `QuarantineOutcome` (what happened to a corrupt document)
- `SystemFontRef` (one imported system font: identity + last-known path hint)
- `FontSettingsRecord` (per-font settings: display-name override + default profile +
  custom kerning pairs)
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
///
/// Version 3 added `fonts.<identity>.custom_kerning`. It is a GATE, not a migration: the
/// field is purely additive and every older document still decodes unchanged, but an older
/// BUILD must refuse to rewrite a v3 document (see the file header) instead of dropping the
/// key it cannot see on the next debounced profile write.
pub(crate) const FONTS_DATA_VERSION: u32 = 3;

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

/// Serde mirror of one [`CustomKerningPair`]. JSON shape:
/// `{ "left": "A", "right": "V", "em": -40.0 }`.
///
/// `left`/`right` are single-character STRINGS rather than code points, so the document
/// stays readable and hand-editable; a value that is not exactly one `char` is dropped by
/// [`decode_custom_kerning`] with a warning. `em` is the advance delta in THOUSANDTHS OF AN
/// EM (see [`CustomKerningPair::offset_per_mille`]); it is written even when `0.0`, because
/// a zero pair is a deliberate cancellation of a built-in one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct CustomKerningEntry {
    /// Left-hand character of the pair, as a one-character string.
    #[serde(default)]
    left: String,
    /// Right-hand character of the pair, as a one-character string.
    #[serde(default)]
    right: String,
    /// Advance delta in thousandths of an em. Negative tightens, positive widens.
    #[serde(default)]
    em: f32,
}

/// One user-defined kerning pair override for a font, replacing whatever the font's own
/// `kern`/GPOS tables say for that pair. The font FILE is never modified.
///
/// `offset_per_mille` is the advance delta between `left` and `right` in THOUSANDTHS OF AN
/// EM (positive widens, negative tightens), so the tuning is independent of both the
/// rendered size and the font's `units_per_em`; the renderer applies
/// `delta_px = offset_per_mille / 1000.0 * font_size_px`. A value of `0.0` is MEANINGFUL —
/// it cancels a built-in pair — and is therefore never filtered out.
#[derive(Debug, Clone, PartialEq)]
pub struct CustomKerningPair {
    /// Left-hand character of the pair.
    pub left: char,
    /// Right-hand character of the pair.
    pub right: char,
    /// Advance delta in thousandths of an em. Finite by construction
    /// ([`sanitize_custom_kerning`] drops NaN/±∞); `0.0` cancels a built-in pair.
    pub offset_per_mille: f32,
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
    /// User-defined kerning pair overrides, in user order. Omitted when empty (v3+).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    custom_kerning: Vec<CustomKerningEntry>,
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
pub(crate) struct VirtualFontGroupMember {
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
pub(crate) struct VirtualFontGroup {
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
    /// [`decode`], because reading an identity-keyed document as an empty v1 would destroy
    /// it on the next save. Always written by [`encode`].
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
pub(crate) struct SystemFontRef {
    /// PostScript name (unsuffixed identity) of the font. EMPTY while a v1 document is
    /// still waiting for the deferred migration to learn the name.
    pub font: String,
    /// Last known file path. A HINT: accepted only when the file is still present and
    /// still claims `font` (see the file header).
    pub last_path: Option<PathBuf>,
}

/// Per-font settings in the decoded runtime form. A record carrying nothing at all is
/// dropped rather than stored (see [`FontSettingsRecord::is_empty`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct FontSettingsRecord {
    /// User display-name override; blank values are normalized away on decode.
    pub display_name: Option<String>,
    /// The font's default parameter profile.
    pub profile: Option<Value>,
    /// User-defined kerning pair overrides, in user order, sanitized
    /// ([`sanitize_custom_kerning`]). Empty means "the font uses only its built-in pairs".
    pub custom_kerning: Vec<CustomKerningPair>,
}

impl FontSettingsRecord {
    /// Whether the record carries nothing worth storing, i.e. every field is unset.
    ///
    /// `custom_kerning` counts: a record holding ONLY kerning overrides would otherwise be
    /// dropped by `mutate_font_record` the instant it was created.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.display_name.is_none() && self.profile.is_none() && self.custom_kerning.is_empty()
    }
}

/// Decoded in-memory form of `fonts_data.json` consumed by `font_settings_store`.
///
/// This is the boundary type between disk I/O and the runtime store. `pending_migration`
/// marks a document that was read with the LEGACY v1 rules, i.e. one whose `fonts` keys,
/// `virtual_groups[].members[].font` values and `system_fonts[].font` values are still
/// path-derived and must be re-keyed once a font list exists.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct FontsData {
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
pub(crate) fn data_path(fonts_dir: &Path) -> PathBuf {
    fonts_dir.join(FONTS_DATA_FILE_NAME)
}

/// Typed result of attempting to load `fonts_data.json`. The three cases must be handled
/// differently by the seeding logic: `Missing` is the normal first run (run the legacy
/// migration), `Loaded` carries a parsed document (use it as-is), and `Invalid` means the
/// file exists but is unreadable/malformed — it must be quarantined and treated as `Missing`
/// rather than degraded to empty, otherwise the next mutation would overwrite and destroy a
/// possibly-recoverable file.
#[derive(Debug)]
pub(crate) enum LoadOutcome {
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
pub(crate) use super::doc_store::{DocumentFingerprint, SaveBaseline};

/// Why [`save_checked`] refused to (or could not) write.
#[derive(Debug)]
pub(crate) enum SaveError {
    /// Directory creation, serialization, or the atomic write itself failed. Carries a
    /// human-readable description including the path and the OS error.
    Io(String),
    /// The document on disk declares a schema version this build does not understand.
    /// Rewriting it in the CURRENT schema would silently drop every field that version
    /// added, so the write is refused; the user keeps the newer file intact.
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
pub(crate) enum QuarantineOutcome {
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
pub(crate) fn load_outcome(fonts_dir: &Path) -> LoadOutcome {
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
            ms_log::runtime_log::log_warn(format!(
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
            ms_log::runtime_log::log_warn(format!(
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
        ms_log::runtime_log::log_warn(format!(
            "typing: fonts_data.json version {} is newer than the expected {}; parsing known \
             fields only, and this build will REFUSE to overwrite the file. Path: {}",
            file.version.unwrap_or_default(),
            FONTS_DATA_VERSION,
            path.display()
        ));
    }

    let data = decode(file);
    if data.pending_migration {
        ms_log::runtime_log::log_info(format!(
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
pub(crate) fn quarantine_bad_file(fonts_dir: &Path) -> QuarantineOutcome {
    let path = data_path(fonts_dir);
    // `fonts_data.json` -> `fonts_data.json.bad`; `fs::rename` overwrites an older `.bad`.
    let bad = path.with_extension("json.bad");
    let rename_error = match fs::rename(&path, &bad) {
        Ok(()) => {
            ms_log::runtime_log::log_warn(format!(
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
            ms_log::runtime_log::log_warn(format!(
                "typing: could not RENAME the corrupt fonts_data.json ({rename_error}); copied \
                 it to {} instead, so the original may be overwritten safely. Path: {}",
                bad.display(),
                path.display()
            ));
            QuarantineOutcome::Copied
        }
        Err(err) => {
            ms_log::runtime_log::log_error(format!(
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

/// Sanitizes a list of custom kerning pairs, applied on BOTH decode and encode so the
/// on-disk and in-memory forms are always well-formed. Rules (user order preserved):
/// - drop a pair whose `offset_per_mille` is not finite (NaN / ±∞ would serialize as JSON
///   `null` and come back as `0.0`, silently turning a broken value into a cancellation);
/// - deduplicate by `(left, right)`, FIRST wins — a later duplicate is a second opinion
///   about the same pair, and only one of them can ever be applied.
///
/// A pair whose offset is `0.0` IS KEPT. Zero is the user's way to CANCEL a non-zero
/// built-in pair, so it carries information that dropping it would destroy; the built-in
/// extractor in the settings UI drops zeros because it is describing the FONT, not the
/// user's overrides. Characters are compared verbatim: `A`/`a` are different pairs.
///
/// `pub(crate)` because `font_settings_store` must apply the SAME rule to an incoming list:
/// a mutator that stored an unsanitized list would make the in-memory state disagree with
/// what the document round-trips.
#[must_use]
pub(crate) fn sanitize_custom_kerning(pairs: Vec<CustomKerningPair>) -> Vec<CustomKerningPair> {
    let mut seen: HashSet<(char, char)> = HashSet::new();
    let mut out: Vec<CustomKerningPair> = Vec::with_capacity(pairs.len());
    for pair in pairs {
        if !pair.offset_per_mille.is_finite() {
            ms_log::runtime_log::log_warn(format!(
                "typing: fonts_data: dropping a custom kerning pair '{}{}' whose offset is not \
                 a finite number ({}); it cannot be persisted or applied.",
                pair.left, pair.right, pair.offset_per_mille
            ));
            continue;
        }
        if !seen.insert((pair.left, pair.right)) {
            continue;
        }
        out.push(pair);
    }
    out
}

/// The single `char` of `raw`, or `None` when it is empty or holds more than one.
///
/// A kerning pair addresses exactly two characters; anything else in that slot is a hand
/// edit or a foreign writer and cannot be applied to anything.
fn single_char(raw: &str) -> Option<char> {
    let mut chars = raw.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

/// Converts the serde mirror of a font's custom kerning list into the decoded runtime form,
/// dropping (with a warning) every entry whose `left`/`right` is not exactly one character,
/// then applying [`sanitize_custom_kerning`].
fn decode_custom_kerning(entries: Vec<CustomKerningEntry>) -> Vec<CustomKerningPair> {
    let pairs = entries
        .into_iter()
        .filter_map(|entry| {
            match (single_char(&entry.left), single_char(&entry.right)) {
                (Some(left), Some(right)) => Some(CustomKerningPair {
                    left,
                    right,
                    offset_per_mille: entry.em,
                }),
                _ => {
                    // A pair naming no single character addresses no glyph pair; it is kept
                    // out of the runtime form rather than guessed at.
                    ms_log::runtime_log::log_warn(format!(
                        "typing: fonts_data: dropping a custom kerning entry whose left/right \
                         is not exactly one character (left: {:?}, right: {:?}).",
                        entry.left, entry.right
                    ));
                    None
                }
            }
        })
        .collect();
    sanitize_custom_kerning(pairs)
}

/// Converts the decoded custom kerning list back into its serde mirror, sanitizing first so
/// a runtime list that was mutated in place cannot write a malformed document.
#[must_use]
fn encode_custom_kerning(pairs: &[CustomKerningPair]) -> Vec<CustomKerningEntry> {
    sanitize_custom_kerning(pairs.to_vec())
        .into_iter()
        .map(|pair| CustomKerningEntry {
            left: pair.left.to_string(),
            right: pair.right.to_string(),
            em: pair.offset_per_mille,
        })
        .collect()
}

/// Normalizes one decoded per-font settings record: a blank display-name override behaves
/// exactly like "no override", so it is dropped rather than stored; the custom kerning list
/// is sanitized (see [`decode_custom_kerning`]).
fn decode_settings_entry(entry: FontSettingsEntry) -> FontSettingsRecord {
    FontSettingsRecord {
        display_name: entry
            .display_name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty()),
        profile: entry.profile,
        custom_kerning: decode_custom_kerning(entry.custom_kerning),
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
/// Neither payload is ever discarded: a document that carries identity-keyed (v2+) keys AND
/// leftover v1 keys (a hand edit, a half-written file, a partially migrated one) keeps both,
/// with the identity-keyed form winning on a key clash. The v1 half is what raises
/// `pending_migration`.
///
/// VERSION INFERENCE. The `version` field decides when it is present. When it is ABSENT the
/// PAYLOAD decides: a document carrying `system_fonts`/`fonts` is identity-keyed. The old
/// rule — serde's `0` default, therefore "≤ 1", therefore v1 — read such a document as an
/// EMPTY v1 and the next save wrote that emptiness back, destroying every identity-keyed
/// setting in it. With nothing to go on at all (no version, no payload) the document is
/// treated as legacy, which is the harmless direction: a pending migration only ever re-keys
/// and never drops.
fn decode(file: FontsDataFile) -> FontsData {
    let has_identity_payload = !file.system_fonts.is_empty() || !file.fonts.is_empty();
    let has_legacy_payload =
        !file.imported_system_fonts.is_empty() || !file.font_settings.is_empty();
    let declares_legacy = match file.version {
        Some(version) => version <= LEGACY_FONTS_DATA_VERSION,
        None => !has_identity_payload,
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
        // The identity-keyed form wins: a key present in both was already migrated.
        fonts.entry(key).or_insert(record);
    }

    FontsData {
        system_fonts,
        fonts,
        virtual_groups: sanitize_virtual_groups(file.virtual_groups),
        // An identity-keyed document carries the flag explicitly (a migration that could
        // not resolve everything writes it back), so it survives the rewrite the migration
        // itself performs — without that, the next launch would read an identity-keyed
        // document, never retry, and freeze the unresolved keys forever.
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
pub(crate) fn save_checked(
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
/// `#[serde(flatten)]` bag would let this build stamp the CURRENT version onto a payload whose
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
                    custom_kerning: encode_custom_kerning(&record.custom_kerning),
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
            custom_kerning: Vec::new(),
        }
    }

    #[test]
    fn round_trip_current_schema_through_temp_file() {
        let path = unique_temp_path("roundtrip_v3");
        let mut fonts = BTreeMap::new();
        fonts.insert("CCWildWordsLower-Regular".to_string(), named("Разговор"));
        fonts.insert(
            "kCCAskForMercy-Regular".to_string(),
            FontSettingsRecord {
                display_name: None,
                profile: Some(serde_json::json!({ "schema": 2, "font_size_px": 42.0 })),
                custom_kerning: Vec::new(),
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
        assert_eq!(loaded, data, "a current-schema document must round-trip verbatim");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn saved_document_declares_v3_and_omits_unset_fields() {
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
        assert!(raw.contains("\"version\": 3"), "the document must declare v3");
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
                custom_kerning: Vec::new(),
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
        assert!(raw.contains("\"version\": 3"), "it is written in the current schema");
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

    /// DEFECT 6. A document from a FUTURE schema must never be rewritten in the current
    /// schema: everything the newer version added (here `font_collections`) would silently
    /// disappear. Reading it best-effort stays allowed; writing over it does not.
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

    /// Convenience constructor for one custom kerning pair.
    fn kern(left: char, right: char, offset_per_mille: f32) -> CustomKerningPair {
        CustomKerningPair {
            left,
            right,
            offset_per_mille,
        }
    }

    /// Custom kerning survives a save -> load cycle VERBATIM, user order included, and a
    /// record holding ONLY kerning is neither dropped as empty nor reordered.
    #[test]
    fn custom_kerning_round_trips_in_user_order() {
        let path = unique_temp_path("kerning_roundtrip");
        let mut fonts = BTreeMap::new();
        fonts.insert(
            "Comic-Regular".to_string(),
            FontSettingsRecord {
                display_name: None,
                profile: None,
                // Deliberately NOT sorted: the user's order is the stored order.
                custom_kerning: vec![
                    kern('V', 'A', -40.0),
                    kern('A', 'V', -40.0),
                    kern('Т', 'о', 12.5),
                ],
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
        assert_eq!(loaded, data, "custom kerning must round-trip verbatim");
        let _ = fs::remove_file(&path);
    }

    /// A pair of `0.0` is the user CANCELLING a built-in pair, so it must survive the whole
    /// round trip; dropping it (as the built-in extractor does) would silently restore the
    /// font's own kerning for that pair.
    #[test]
    fn a_zero_valued_custom_pair_survives_the_round_trip() {
        let path = unique_temp_path("kerning_zero");
        let mut fonts = BTreeMap::new();
        fonts.insert(
            "Comic-Regular".to_string(),
            FontSettingsRecord {
                display_name: None,
                profile: None,
                custom_kerning: vec![kern('A', 'V', 0.0)],
            },
        );
        let data = FontsData {
            system_fonts: Vec::new(),
            fonts,
            virtual_groups: Vec::new(),
            pending_migration: false,
        };
        save_to_file(&path, &data, SaveBaseline::Unchecked).expect("save must succeed");
        let raw = fs::read_to_string(&path).expect("read back");
        assert!(raw.contains("custom_kerning"), "a zero pair must still be written");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert_eq!(
            loaded.fonts.get("Comic-Regular").map(|record| record.custom_kerning.as_slice()),
            Some([kern('A', 'V', 0.0)].as_slice()),
            "a zero-valued pair must come back unchanged"
        );
        let _ = fs::remove_file(&path);
    }

    /// A pair character that is itself JSON syntax — the double quote and the backslash — must
    /// survive the round trip, and the document must stay parseable.
    ///
    /// Nothing here builds JSON by hand (`save_to_file` goes through `serde_json`), so the
    /// escaping is the serializer's job; this test is what keeps it that way. A future "cheaper"
    /// hand-rolled writer would corrupt the whole document on the first quote a user types, and
    /// the pair characters are free-form user input from a text field.
    /// The SPACE character is pinned alongside them because `single_char` deliberately does not
    /// trim: a space is a legitimate half of a kerning pair, and trimming it would drop the pair.
    #[test]
    fn pair_characters_that_are_json_syntax_survive_the_round_trip() {
        let path = unique_temp_path("kerning_json_syntax");
        let mut fonts = BTreeMap::new();
        fonts.insert(
            "Comic-Regular".to_string(),
            FontSettingsRecord {
                display_name: None,
                profile: None,
                custom_kerning: vec![
                    kern('"', '"', -30.0),
                    kern('\\', 'A', 15.0),
                    kern(' ', '"', -5.0),
                    kern('\n', 'A', 7.0),
                ],
            },
        );
        let data = FontsData {
            system_fonts: Vec::new(),
            fonts,
            virtual_groups: Vec::new(),
            pending_migration: false,
        };
        save_to_file(&path, &data, SaveBaseline::Unchecked).expect("save must succeed");
        let raw = fs::read_to_string(&path).expect("read back");
        // The serializer must have ESCAPED the quote rather than emitted it raw, or the
        // document below would not parse at all.
        assert!(raw.contains(r#""left": "\"""#), "the quote must be written escaped: {raw}");
        assert!(raw.contains(r#""left": "\\""#), "the backslash must be written escaped: {raw}");
        serde_json::from_str::<serde_json::Value>(&raw).expect("the document must stay valid JSON");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert_eq!(loaded, data, "every pair character must come back verbatim");
        let _ = fs::remove_file(&path);
    }

    /// An empty kerning list is OMITTED from the document, like every other unset field.
    #[test]
    fn an_empty_custom_kerning_list_is_not_written() {
        let path = unique_temp_path("kerning_omitted");
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
        assert!(
            !raw.contains("custom_kerning"),
            "an empty kerning list must be omitted, never written as []"
        );
        let _ = fs::remove_file(&path);
    }

    /// Decode-side sanitation: a `left`/`right` that is not exactly one character is dropped,
    /// a non-finite offset is dropped, a duplicate `(left, right)` keeps the FIRST entry, the
    /// surviving order is the document order, and a ZERO offset is kept.
    #[test]
    fn custom_kerning_is_sanitized_on_decode() {
        let path = unique_temp_path("kerning_sanitize");
        let raw = r#"{
            "version": 3,
            "fonts": { "Comic-Regular": { "custom_kerning": [
                { "left": "A", "right": "V", "em": -40.0 },
                { "left": "", "right": "V", "em": -10.0 },
                { "left": "AB", "right": "V", "em": -10.0 },
                { "left": "A", "right": "V", "em": 999.0 },
                { "left": "T", "right": "o", "em": 0.0 },
                { "left": "W", "right": "a", "em": 1e39 }
            ] } }
        }"#;
        fs::write(&path, raw).expect("write kerning doc");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert_eq!(
            loaded.fonts.get("Comic-Regular").map(|record| record.custom_kerning.as_slice()),
            // `1e39` is finite as JSON but overflows f32 to +inf, so it is dropped along
            // with the multi-char entries;
            // the second `A`/`V` loses to the first; the zero pair is kept.
            Some([kern('A', 'V', -40.0), kern('T', 'o', 0.0)].as_slice())
        );
        let _ = fs::remove_file(&path);
    }

    /// A v2 document (no `custom_kerning` anywhere) still decodes exactly as before: the new
    /// field simply defaults to empty and nothing about the record changes.
    #[test]
    fn a_v2_document_still_decodes_without_custom_kerning() {
        let path = unique_temp_path("v2_compat");
        let raw = r#"{
            "version": 2,
            "system_fonts": [ { "font": "Roboto-Medium", "last_path": "/x/Roboto-Medium.ttf" } ],
            "fonts": { "Comic-Regular": { "display_name": "Разговор", "profile": { "schema": 2 } } },
            "virtual_groups": [ { "name": "G", "members": [ { "font": "Comic-Regular" } ] } ]
        }"#;
        fs::write(&path, raw).expect("write v2 doc");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert!(!loaded.pending_migration, "a v2 document owes no re-key");
        assert_eq!(loaded.system_fonts, vec![system_font("Roboto-Medium", "/x/Roboto-Medium.ttf")]);
        let record = loaded.fonts.get("Comic-Regular").expect("the record must decode");
        assert_eq!(record.display_name.as_deref(), Some("Разговор"));
        assert!(record.profile.is_some(), "the v2 profile must survive the bump");
        assert!(record.custom_kerning.is_empty(), "a v2 document carries no kerning");
        assert_eq!(loaded.virtual_groups.len(), 1);
        let _ = fs::remove_file(&path);
    }

    /// A v1 (path-keyed) document is unaffected by the bump: it still decodes verbatim and is
    /// still flagged for the deferred re-key. The v1 mirror reuses `FontSettingsEntry`, so the
    /// new field is simply absent there.
    #[test]
    fn a_v1_document_still_decodes_after_the_version_bump() {
        let path = unique_temp_path("v1_compat");
        let raw = r#"{
            "version": 1,
            "imported_system_fonts": ["/home/u/.fonts/Roboto-Medium.ttf"],
            "font_settings": { "groups/ВВД/Мысли.ttf": { "display_name": "Мысли" } }
        }"#;
        fs::write(&path, raw).expect("write v1 doc");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert!(loaded.pending_migration, "a v1 document must still be flagged pending");
        let record = loaded
            .fonts
            .get("groups/ВВД/Мысли.ttf")
            .expect("the legacy path key must survive");
        assert_eq!(record.display_name.as_deref(), Some("Мысли"));
        assert!(record.custom_kerning.is_empty());
        let _ = fs::remove_file(&path);
    }
}
