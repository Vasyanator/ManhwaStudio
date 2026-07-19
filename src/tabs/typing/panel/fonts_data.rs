/*
File: panel/fonts_data.rs

Purpose:
Serde schema and disk I/O for the app-level per-font settings document
`fonts_data.json`, stored inside the app fonts directory (`resolve_fonts_dir()`).
This file is the single on-disk home for the user-imported system font FILE paths
and per-font UI settings (currently a display-name override). Font discovery never
picks it up because it only scans `.ttf/.otf/.ttc`.

Main responsibilities:
- define the versioned JSON schema (`version: 1`) and its serde mirror;
- load the document as a typed `LoadOutcome` (`Missing` / `Loaded` / `Invalid`) so the
  caller can distinguish "first run" from "corrupt file" — a corrupt file must NOT be
  silently treated as empty, or the next mutation would overwrite (and destroy) it;
  an unknown future version is still parsed best-effort as `Loaded`;
- quarantine a corrupt document to `fonts_data.json.bad` (`quarantine_bad_file`);
- save a full snapshot atomically and crash-durably (temp sibling written via an explicit
  `File` + `write_all` + `sync_all`, then rename; mirrors `locale_store::write_atomic`),
  creating the fonts directory if missing;
- compute the stable per-font KEY (`font_settings_key`): a forward-slash relative
  path when the font lives under the fonts directory, otherwise the absolute path.

Key types:
- `FontsData` (decoded in-memory form consumed by `font_settings_store`)
- `LoadOutcome` (Missing / Loaded / Invalid load result)
- `FontSettingsEntry` (per-font settings block: display-name override)

Key functions:
- `font_settings_key` (pure key derivation, unit-tested)
- `data_path` / `load_outcome` / `quarantine_bad_file` / `save`

Notes:
`use super::*;` pulls in the parent `panel` module's imports (`Path`, `PathBuf`,
`fs`); `std::io::Write` is imported here for the durable temp write. Compiled
unconditionally (no wasm cfg gates): raw `std::fs`. A read/parse failure yields
`LoadOutcome::Invalid` with a `runtime_log` warning instead of degrading to empty, so
imported fonts + overrides are never silently wiped. The store persists
`imported_system_fonts` paths as strings via `to_string_lossy`, mirroring the legacy
`presets_io` writer this document supersedes.
*/

use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;

/// Current on-disk schema version of `fonts_data.json`.
pub(in crate::tabs::typing) const FONTS_DATA_VERSION: u32 = 1;

/// File name of the per-font settings document inside the app fonts directory.
const FONTS_DATA_FILE_NAME: &str = "fonts_data.json";

/// Per-font user settings block stored under a font key in `fonts_data.json`.
/// Fields are optional and skipped when empty so the document stays minimal.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct FontSettingsEntry {
    /// User display-name override. Absent/`None` means "use the font's own label".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
}

/// Serde mirror of the entire `fonts_data.json` document. Every field has a serde
/// default so a partial or future-version document still deserializes its known keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FontsDataFile {
    /// Schema version; see `FONTS_DATA_VERSION`. A mismatch is warned about but the
    /// known fields are still parsed best-effort.
    #[serde(default)]
    version: u32,
    /// User-imported system font FILE paths, stored as strings (`to_string_lossy`).
    #[serde(default)]
    imported_system_fonts: Vec<String>,
    /// Per-font settings keyed by `font_settings_key`.
    #[serde(default)]
    font_settings: BTreeMap<String, FontSettingsEntry>,
}

/// Decoded in-memory form of `fonts_data.json` consumed by `font_settings_store`.
///
/// This is the boundary type between disk I/O and the runtime store: paths are real
/// `PathBuf`s and only non-empty display-name overrides are retained.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::tabs::typing) struct FontsData {
    /// User-imported system font FILE paths, in stored order.
    pub imported_system_fonts: Vec<PathBuf>,
    /// Display-name overrides keyed by `font_settings_key`. Empty values are dropped.
    pub display_name_overrides: BTreeMap<String, String>,
}

/// Absolute (or fonts-dir-relative) path of the `fonts_data.json` document.
#[must_use]
pub(in crate::tabs::typing) fn data_path(fonts_dir: &Path) -> PathBuf {
    fonts_dir.join(FONTS_DATA_FILE_NAME)
}

/// Derives the stable per-font settings KEY for `path`.
///
/// When `path` lives under `fonts_dir` the key is the RELATIVE path with
/// forward-slash separators (so the same font keys identically on Linux and
/// Windows). Otherwise — e.g. an imported system font living elsewhere on disk —
/// the key is the absolute path string as-is. This key is independent of the
/// render/inline-tag `label`, so a display-name override never changes rendering.
#[must_use]
pub(in crate::tabs::typing) fn font_settings_key(fonts_dir: &Path, path: &Path) -> String {
    match path.strip_prefix(fonts_dir) {
        // Normalize separators so a folder font keys the same across platforms.
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        // Outside the fonts dir: keep the absolute path verbatim as the key.
        Err(_) => path.to_string_lossy().into_owned(),
    }
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
    Loaded(FontsData),
    /// The file exists but could not be read or parsed; the caller must quarantine it.
    Invalid,
}

/// Loads `fonts_data.json` from `fonts_dir` into a typed [`LoadOutcome`]. A missing file is
/// `Missing`; a read or parse failure is `Invalid` (warned about, never silently emptied);
/// otherwise `Loaded` (an unexpected version is warned about and still parsed best-effort).
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

    if file.version != FONTS_DATA_VERSION {
        // Forward/backward compatible: warn but keep the fields we understand.
        crate::runtime_log::log_warn(format!(
            "typing: fonts_data.json version {} != expected {}; parsing known fields only. Path: {}",
            file.version,
            FONTS_DATA_VERSION,
            path.display()
        ));
    }

    LoadOutcome::Loaded(decode(file))
}

/// Best-effort quarantine of a corrupt `fonts_data.json`: renames it to `fonts_data.json.bad`
/// (overwriting any older quarantine) so the next mutation does not overwrite — and thereby
/// destroy — a possibly-recoverable document. Failures are logged, not surfaced: quarantine
/// is a diagnostic aid, not a correctness requirement (the caller proceeds as first-run).
pub(in crate::tabs::typing) fn quarantine_bad_file(fonts_dir: &Path) {
    let path = data_path(fonts_dir);
    // `fonts_data.json` -> `fonts_data.json.bad`; `fs::rename` overwrites an older `.bad`.
    let bad = path.with_extension("json.bad");
    match fs::rename(&path, &bad) {
        Ok(()) => crate::runtime_log::log_warn(format!(
            "typing: quarantined corrupt fonts_data.json to {}",
            bad.display()
        )),
        Err(err) => crate::runtime_log::log_warn(format!(
            "typing: could not quarantine corrupt fonts_data.json. Path: {} Error: {err}",
            path.display()
        )),
    }
}

/// Converts the serde mirror into the decoded runtime form: string paths become
/// `PathBuf`s (empty strings skipped) and only non-empty display-name overrides are kept.
fn decode(file: FontsDataFile) -> FontsData {
    let imported_system_fonts = file
        .imported_system_fonts
        .into_iter()
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from)
        .collect();
    let display_name_overrides = file
        .font_settings
        .into_iter()
        .filter_map(|(key, entry)| {
            let name = entry.display_name?;
            // Drop blank overrides so they behave identically to "no override".
            if name.trim().is_empty() {
                None
            } else {
                Some((key, name))
            }
        })
        .collect();
    FontsData {
        imported_system_fonts,
        display_name_overrides,
    }
}

/// Atomically writes a full snapshot of `data` to `fonts_data.json` in `fonts_dir`,
/// creating the fonts directory if it does not yet exist.
///
/// # Errors
/// Returns a human-readable error string on directory creation, serialization, or
/// atomic-write failure. Callers persist off the GUI thread.
pub(in crate::tabs::typing) fn save(fonts_dir: &Path, data: &FontsData) -> Result<(), String> {
    // Create the fonts dir on demand so a first-ever save (e.g. one-time migration)
    // succeeds even when the app runs before any font is present.
    if let Err(err) = fs::create_dir_all(fonts_dir) {
        return Err(format!(
            "cannot create fonts directory {}: {err}",
            fonts_dir.display()
        ));
    }
    save_to_file(&data_path(fonts_dir), data)
}

/// Path-parameterized core of [`save`], split out so the write recipe can be
/// unit-tested against a temp file. Assumes the parent directory already exists.
fn save_to_file(path: &Path, data: &FontsData) -> Result<(), String> {
    let file = encode(data);
    let mut text = serde_json::to_string_pretty(&file)
        .map_err(|err| format!("cannot serialize fonts_data.json: {err}"))?;
    text.push('\n');
    write_atomic(path, &text)
}

/// Converts the decoded runtime form into the serde mirror for serialization,
/// stamping the current schema version and dropping blank display-name overrides.
fn encode(data: &FontsData) -> FontsDataFile {
    let imported_system_fonts = data
        .imported_system_fonts
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let font_settings = data
        .display_name_overrides
        .iter()
        .filter(|(_, name)| !name.trim().is_empty())
        .map(|(key, name)| {
            (
                key.clone(),
                FontSettingsEntry {
                    display_name: Some(name.clone()),
                },
            )
        })
        .collect();
    FontsDataFile {
        version: FONTS_DATA_VERSION,
        imported_system_fonts,
        font_settings,
    }
}

/// Atomically replaces `path` with `contents`: write a sibling temp file, fsync its
/// contents, then `rename` it over the target (an atomic replace on both Unix and Windows).
/// The temp file lives in the SAME directory as `path` so the final rename never crosses a
/// filesystem boundary. Mirrors `locale_store::write_atomic`.
///
/// The temp write goes through an explicit `File` + `write_all` + `sync_all` so the data
/// pages are on stable storage BEFORE the rename: without the fsync a crash just after the
/// rename could leave a renamed-but-empty (or partially written) file, since the rename can
/// become durable while the contents are still only in the page cache (precedent: the ORT
/// load-guard writers in `src/tabs/settings/mod.rs`). We deliberately do NOT fsync the parent
/// directory: `fonts_data.json` is not crash-critical (a lost directory-entry flush at worst
/// loses a brand-new file, which the next mutation rewrites), so the recipe stays simple.
fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| FONTS_DATA_FILE_NAME.to_owned());
    // Per-process temp name keeps two concurrent processes from colliding on the
    // same temp path; a `.` prefix hides it from casual directory listings.
    let temp = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));

    // Scope the file handle so it is closed before the rename. Any failure removes the
    // orphaned temp before reporting (a failed cleanup cannot mask the real error — §7).
    {
        let mut file = fs::File::create(&temp)
            .map_err(|err| format!("cannot create temp file {}: {err}", temp.display()))?;
        file.write_all(contents.as_bytes()).map_err(|err| {
            let _ = fs::remove_file(&temp);
            format!("cannot write temp file {}: {err}", temp.display())
        })?;
        file.sync_all().map_err(|err| {
            let _ = fs::remove_file(&temp);
            format!("cannot fsync temp file {}: {err}", temp.display())
        })?;
    }

    fs::rename(&temp, path).map_err(|err| {
        // Best-effort cleanup of the orphaned temp file; the rename failure is the
        // error we actually report (a failed cleanup cannot mask it) — CLAUDE.md §7.
        let _ = fs::remove_file(&temp);
        format!("cannot rename temp file over {}: {err}", path.display())
    })
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

    #[test]
    fn key_is_relative_and_slash_normalized_under_fonts_dir() {
        let fonts_dir = Path::new("/app/fonts");
        let path = fonts_dir.join("groups").join("Manga").join("Bold.ttf");
        assert_eq!(font_settings_key(fonts_dir, &path), "groups/Manga/Bold.ttf");
        // A root-level font keys by its bare file name.
        assert_eq!(
            font_settings_key(fonts_dir, &fonts_dir.join("Comic.otf")),
            "Comic.otf"
        );
    }

    #[test]
    fn key_is_absolute_when_outside_fonts_dir() {
        let fonts_dir = Path::new("/app/fonts");
        let outside = Path::new("/usr/share/fonts/NotoSans.ttf");
        assert_eq!(
            font_settings_key(fonts_dir, outside),
            "/usr/share/fonts/NotoSans.ttf"
        );
    }

    /// Unwraps a `Loaded` outcome or panics with a message naming the actual variant.
    fn expect_loaded(outcome: LoadOutcome) -> FontsData {
        match outcome {
            LoadOutcome::Loaded(data) => data,
            LoadOutcome::Missing => panic!("expected Loaded, got Missing"),
            LoadOutcome::Invalid => panic!("expected Loaded, got Invalid"),
        }
    }

    #[test]
    fn round_trip_through_temp_file() {
        let path = unique_temp_path("roundtrip");
        let mut overrides = BTreeMap::new();
        overrides.insert("groups/Manga/Bold.ttf".to_string(), "Мой шрифт".to_string());
        let data = FontsData {
            imported_system_fonts: vec![
                PathBuf::from("/usr/share/fonts/NotoSans.ttf"),
                PathBuf::from("/usr/share/fonts/Roboto.otf"),
            ],
            display_name_overrides: overrides,
        };
        save_to_file(&path, &data).expect("save must succeed");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert_eq!(loaded, data);
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
        // A future version with known fields present must still yield those fields as Loaded.
        let raw = r#"{
            "version": 99,
            "imported_system_fonts": ["/x/A.ttf"],
            "font_settings": { "B.ttf": { "display_name": "Name" } },
            "unknown_future_key": 123
        }"#;
        fs::write(&path, raw).expect("write future-version doc");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert_eq!(loaded.imported_system_fonts, vec![PathBuf::from("/x/A.ttf")]);
        assert_eq!(
            loaded.display_name_overrides.get("B.ttf").map(String::as_str),
            Some("Name")
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn blank_override_is_dropped_on_load() {
        let path = unique_temp_path("blank_override");
        let raw = r#"{
            "version": 1,
            "imported_system_fonts": [],
            "font_settings": { "A.ttf": { "display_name": "   " } }
        }"#;
        fs::write(&path, raw).expect("write blank override");
        let loaded = expect_loaded(load_outcome_from_file(&path));
        assert!(
            loaded.display_name_overrides.is_empty(),
            "a whitespace-only override must not survive load"
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
        assert_eq!(loaded.imported_system_fonts, vec![PathBuf::from("/x/A.ttf")]);
        let _ = fs::remove_file(&path);
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
