/*
File: locale_store.rs

Purpose:
Native-only (`#[cfg(not(target_arch = "wasm32"))]`) on-disk layer for the UI
localization catalog. It materializes the catalogs that `ms-i18n` embeds
(`embedded_locales()`) into an editable `locale/` folder next to the executable
(`config::data_dir().join("locale")`), reconciles each file with its reference on
every launch, and then loads the active UI locale from disk (falling back to the
embedded catalog) and installs it into the `ms-i18n` runtime.

`ms-i18n` is the in-memory layer (parsed `Catalog`, wait-free `lookup`); this file
is the disk layer. On wasm there is no folder next to an executable, so the whole
module is compiled out and the web entry point installs the embedded catalog
directly (see `web_entry.rs`).

Reconcile contract (per embedded locale `<tag>.json`):
1. absent      -> write the embedded source verbatim;
2. present     -> add only the keys the user's file lacks, taking values from the
                  embedded catalog. Existing user values are NEVER overwritten and
                  user-only (obsolete) keys are NEVER deleted.
For a `locale/<tag>.json` whose tag is NOT an embedded locale (a user-authored
custom language, e.g. `de.json`), missing keys are backfilled from `en.json` (the
English reference), not from the tag's own language.

Key structures:
- LocaleStoreError : typed I/O / parse errors (thiserror)
- ReconcileOutcome : the reconciled map plus whether it changed

Key functions:
- reconcile_locale_map    : the PURE reconcile over two JSON maps (no filesystem)
- reconcile_disk_catalog  : best-effort disk unpack/reconcile at startup
- install_ui_locale       : load the active locale from disk and install it
- ui_locale_tag_from_user_settings : read `General.ui_language` -> `LocaleTag`

Identity vs. plural rules (see `ms-i18n`):
`General.ui_language` stores a raw OPEN tag string, resolved to an `ms_i18n::LocaleTag`.
ANY valid tag with a `locale/<tag>.json` on disk loads, custom languages included.
A tag with no hand-written CLDR plural rules uses English plural rules; that fallback
is REPORTED once here at install time (via `ms_i18n::plural_rules_for_tag`), never
silently.

Reference locale is ENGLISH, not Russian:
Missing keys in any non-English catalog fall back to English, and every error path
(absent/invalid `ui_language`, a tag with neither a disk file nor an embedded
catalog) installs ENGLISH. Russian is only the shipped DEFAULT config value
(`config::user_config_defaults()` writes `"ru"`), so a normal install shows Russian;
the error paths do not.

Failure policy (WHY it is acceptable):
The `locale/` directory may be unwritable (Program Files without elevation,
read-only media, a file locked by an editor). This is a DOCUMENTED, BOUNDED
degradation, not a "silent fallback to incorrect behavior" (CLAUDE.md §14): the
application stays fully functional using the compiled-in embedded catalog; only
on-disk editing of translations is unavailable. Every failure is logged via
`crate::runtime_log` with the path, the operation, and the OS/serde error, then
startup continues with the embedded catalog. A corrupt/unparseable user file is
left UNTOUCHED on disk (so the user can fix it by hand) and its tag falls back to
the embedded catalog (English when no embedded catalog exists for the tag).
*/

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::config;
use crate::runtime_log;
use ms_i18n::{Catalog, LocaleTag};

/// Subdirectory (under `config::data_dir()`) holding the editable locale files.
const LOCALE_DIR_NAME: &str = "locale";

/// Reserved, non-translatable metadata key. It is preserved verbatim and never
/// backfilled from a reference catalog (a reference's `_meta.name` is in the
/// reference's own language, not the target's).
const META_KEY: &str = "_meta";

/// `General.ui_language` config key (a locale tag string).
const UI_LANGUAGE_KEY: &str = "ui_language";

/// Typed errors for the locale disk layer.
///
/// `Directory` is the only whole-operation-fatal variant (the `locale/` folder is
/// unreachable); the rest are per-file and are logged-and-skipped by the
/// orchestration so one bad file never aborts the others.
#[derive(Debug, thiserror::Error)]
pub enum LocaleStoreError {
    /// The `locale/` directory itself could not be created or enumerated.
    #[error("locale directory {path} is unavailable (failed to {operation}): {source}")]
    Directory {
        path: PathBuf,
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    /// A single locale file could not be read or written.
    #[error("failed to {operation} locale file {path}: {source}")]
    File {
        path: PathBuf,
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    /// A user locale file is not valid JSON. The file is left untouched on disk.
    #[error("locale file {path} is not valid JSON: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// A user locale file parsed but its root is not a JSON object.
    #[error("locale file {path} root is not a JSON object")]
    NotObject { path: PathBuf },
    /// `ms-i18n` rejected a file's contents as an invalid catalog (bad plural
    /// object, non-string/non-object value, …). Carries the typed message.
    #[error("locale file {path} is not a valid catalog: {message}")]
    Catalog { path: PathBuf, message: String },
}

/// The result of reconciling a user map against a reference map.
///
/// `changed` is `true` iff at least one missing key was added; it is the sole
/// signal used to decide whether the file must be rewritten, so an unchanged file
/// is never rewritten (and its mtime never bumped).
#[derive(Debug)]
pub struct ReconcileOutcome {
    /// The reconciled map (user entries preserved, missing reference keys added).
    pub map: Map<String, Value>,
    /// Whether any key was added relative to the user map.
    pub changed: bool,
}

/// Reconciles a user locale map against a `reference` map — the PURE core, with no
/// filesystem access.
///
/// Adds every key present in `reference` but absent in `user`, copying the
/// reference value verbatim. Because values are copied whole, a nested plural
/// object (`{"one":…,"other":…}`) is treated as a single unit: it is copied
/// atomically when the key is absent, and left entirely alone (even with extra
/// CLDR categories) when the key is already present. Existing user values are
/// NEVER overwritten and user-only keys are NEVER deleted.
///
/// The reserved `_meta` key is never backfilled from `reference`; the user's
/// `_meta` (if any) is carried through unchanged.
///
/// Returns the reconciled map and whether it changed. `changed == false` means the
/// caller must not rewrite the file.
#[must_use]
pub fn reconcile_locale_map(
    user: &Map<String, Value>,
    reference: &Map<String, Value>,
) -> ReconcileOutcome {
    // Start from the user's map so every existing key/value (including any user
    // `_meta` and any obsolete user-only keys) is preserved as-is.
    let mut map = user.clone();
    let mut changed = false;

    for (key, value) in reference {
        // `_meta` is reserved metadata in the reference's own language; never copy
        // it into a target locale.
        if key == META_KEY {
            continue;
        }
        if !map.contains_key(key) {
            // Copy the whole value: for a plural object this keeps it atomic.
            map.insert(key.clone(), value.clone());
            changed = true;
        }
    }

    ReconcileOutcome { map, changed }
}

/// Serializes a locale map to pretty JSON with a stable, readable key order:
/// `_meta` first, then the remaining keys sorted lexicographically. A trailing
/// newline is appended so a `git diff` stays clean.
///
/// The explicit `_meta`-first + sorted insertion is correct whether
/// `serde_json::Map` is a `BTreeMap` (order derived from the keys) or an
/// `IndexMap` (insertion order preserved): locale keys are lowercase dotted
/// identifiers, so `_meta` (leading `_`) precedes them under both.
fn serialize_locale_map(map: &Map<String, Value>) -> Result<String, serde_json::Error> {
    let mut ordered = Map::new();
    if let Some(meta) = map.get(META_KEY) {
        ordered.insert(META_KEY.to_owned(), meta.clone());
    }
    let mut keys: Vec<&String> = map.keys().filter(|k| k.as_str() != META_KEY).collect();
    keys.sort();
    for key in keys {
        if let Some(value) = map.get(key) {
            ordered.insert(key.clone(), value.clone());
        }
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(ordered))?;
    text.push('\n');
    Ok(text)
}

/// Atomically replaces `path` with `contents`: write a sibling temp file, then
/// `rename` it over the target (an atomic replace on both Unix and Windows).
///
/// The temp file lives in the SAME directory as `path` so the final `rename` never
/// crosses a filesystem boundary.
///
/// # Errors
/// [`LocaleStoreError::File`] if the temp write or the rename fails, carrying the
/// path, the failed operation, and the OS error.
fn write_atomic(path: &Path, contents: &str) -> Result<(), LocaleStoreError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_stem = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "locale".to_owned());
    // Per-process temp name keeps two concurrent processes from colliding on the
    // same temp path; a `.` prefix hides it from casual directory listings.
    let temp = parent.join(format!(".{file_stem}.{}.tmp", std::process::id()));

    fs::write(&temp, contents).map_err(|source| LocaleStoreError::File {
        path: temp.clone(),
        operation: "write temporary file",
        source,
    })?;

    fs::rename(&temp, path).map_err(|source| {
        // Best-effort cleanup of the orphaned temp file on the error path; the
        // rename failure below is the error we actually report. Justified ignore
        // per CLAUDE.md §7 (a failed cleanup cannot mask the real error).
        let _ = fs::remove_file(&temp);
        LocaleStoreError::File {
            path: path.to_path_buf(),
            operation: "rename temporary file over target",
            source,
        }
    })?;
    Ok(())
}

/// Parses a JSON source string into its top-level object map.
///
/// # Errors
/// [`LocaleStoreError::Parse`] on invalid JSON, [`LocaleStoreError::NotObject`] if
/// the root is not an object. `path` is only used for error context.
fn parse_object(path: &Path, source: &str) -> Result<Map<String, Value>, LocaleStoreError> {
    let value: Value =
        serde_json::from_str(source).map_err(|source| LocaleStoreError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    match value {
        Value::Object(map) => Ok(map),
        Value::Null
        | Value::Bool(_)
        | Value::Number(_)
        | Value::String(_)
        | Value::Array(_) => Err(LocaleStoreError::NotObject {
            path: path.to_path_buf(),
        }),
    }
}

/// Reconciles one on-disk locale file against `reference`, rewriting it only if a
/// key was added.
///
/// Returns `true` if the file was rewritten. The file is read, reconciled in
/// memory, and only rewritten when [`reconcile_locale_map`] reports a change, so an
/// already-complete file is left byte-for-byte intact.
///
/// # Errors
/// [`LocaleStoreError::File`] on a read/write failure, [`LocaleStoreError::Parse`]
/// or [`LocaleStoreError::NotObject`] if the user file is corrupt. On a parse error
/// the file is NOT rewritten (left untouched for manual repair).
fn reconcile_existing_file(
    path: &Path,
    reference: &Map<String, Value>,
) -> Result<bool, LocaleStoreError> {
    let raw = fs::read_to_string(path).map_err(|source| LocaleStoreError::File {
        path: path.to_path_buf(),
        operation: "read",
        source,
    })?;
    let user_map = parse_object(path, &raw)?;
    let outcome = reconcile_locale_map(&user_map, reference);
    if !outcome.changed {
        return Ok(false);
    }
    let text = serialize_locale_map(&outcome.map).map_err(|source| LocaleStoreError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    write_atomic(path, &text)?;
    Ok(true)
}

/// The set of embedded locale tags (`en`, `ru`, …), for classifying which on-disk
/// files are custom (user-authored) languages.
fn embedded_tags() -> Vec<&'static str> {
    ms_i18n::embedded_locales()
        .iter()
        .map(|(tag, _)| *tag)
        .collect()
}

/// Reconciles the whole `locale/` directory at `dir` (explicit path so it is
/// testable against a temp dir).
///
/// Ensures the directory exists, then: for every embedded locale writes the source
/// verbatim when absent or reconciles it against the embedded catalog when present;
/// and for every non-embedded `*.json` file backfills missing keys from `en.json`.
/// Per-file failures are logged and skipped so one bad file never aborts the rest.
///
/// # Errors
/// [`LocaleStoreError::Directory`] only when the directory itself cannot be created
/// or enumerated (the whole-layer-unavailable case the caller degrades on).
fn reconcile_dir_at(dir: &Path) -> Result<(), LocaleStoreError> {
    fs::create_dir_all(dir).map_err(|source| LocaleStoreError::Directory {
        path: dir.to_path_buf(),
        operation: "create",
        source,
    })?;

    // English reference map, used to backfill custom-language files. Parsed once.
    let en_reference = embedded_reference_map("en");

    // Embedded-locale pass: verbatim on absence, reconcile on presence.
    for (tag, source) in ms_i18n::embedded_locales() {
        let path = dir.join(format!("{tag}.json"));
        if path.exists() {
            match parse_object(&path, source) {
                Ok(reference) => log_file_reconcile(&path, &reference),
                Err(err) => runtime_log::log_error(format!(
                    "locale: embedded source for '{tag}' failed to parse (build bug): {err}"
                )),
            }
        } else if let Err(err) = write_atomic(&path, source) {
            // Unwritable target file: keep using the embedded catalog for this tag.
            runtime_log::log_warn(format!(
                "locale: could not write embedded '{tag}' to disk, using embedded catalog: {err}"
            ));
        }
    }

    // Custom-language pass: every `*.json` whose tag is not embedded is backfilled
    // from the English reference.
    let Some(en_reference) = en_reference else {
        // Without a usable English reference we cannot backfill custom files; the
        // embedded pass above already ran, so just stop here.
        return Ok(());
    };
    let embedded = embedded_tags();
    let entries = fs::read_dir(dir).map_err(|source| LocaleStoreError::Directory {
        path: dir.to_path_buf(),
        operation: "read",
        source,
    })?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                runtime_log::log_warn(format!("locale: skipping unreadable directory entry: {err}"));
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(tag) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if embedded.contains(&tag) {
            continue;
        }
        log_file_reconcile(&path, &en_reference);
    }

    Ok(())
}

/// Runs [`reconcile_existing_file`] and logs its outcome; used by the directory
/// orchestration so a per-file error is surfaced without aborting the pass.
fn log_file_reconcile(path: &Path, reference: &Map<String, Value>) {
    match reconcile_existing_file(path, reference) {
        Ok(true) => runtime_log::log_info(format!(
            "locale: reconciled {} (added missing keys)",
            path.display()
        )),
        Ok(false) => {}
        // A parse error leaves the file untouched (see `reconcile_existing_file`);
        // log it so the user can fix the file by hand.
        Err(err) => runtime_log::log_warn(format!("locale: {err}")),
    }
}

/// Parses an embedded locale source into its object map by tag, or `None` if the
/// tag is not embedded / its source fails to parse (a build bug, logged).
fn embedded_reference_map(tag: &str) -> Option<Map<String, Value>> {
    let source = ms_i18n::embedded_locales()
        .iter()
        .find(|(embedded_tag, _)| *embedded_tag == tag)
        .map(|(_, source)| *source)?;
    match parse_object(Path::new(tag), source) {
        Ok(map) => Some(map),
        Err(err) => {
            runtime_log::log_error(format!(
                "locale: embedded reference '{tag}' failed to parse (build bug): {err}"
            ));
            None
        }
    }
}

/// Unpacks and reconciles the on-disk `locale/` folder at startup (best-effort).
///
/// Uses `config::data_dir().join("locale")`. Never fails the caller: a directory
/// that is unavailable is logged and the app keeps using the embedded catalog. Run
/// once, pre-window, before any locale is installed.
pub fn reconcile_disk_catalog() {
    let dir = config::data_dir().join(LOCALE_DIR_NAME);
    if let Err(err) = reconcile_dir_at(&dir) {
        // Bounded degradation: on-disk locale editing is unavailable; the embedded
        // catalog still drives the UI. See the file header's failure policy.
        runtime_log::log_warn(format!(
            "locale: on-disk catalog unavailable, using embedded catalog: {err}"
        ));
    }
}

/// Reads `General.ui_language` and resolves it to a [`LocaleTag`].
///
/// An absent, blank, or syntactically invalid tag resolves to ENGLISH (the
/// reference locale, NOT Russian) and emits a `runtime_log` warning. A valid tag
/// (including a custom tag such as `de`, or a reserved tag such as `es` whose
/// catalog is not embedded) is returned as-is; disk-loading then decides whether an
/// on-disk catalog exists for it. Russian is the shipped default config VALUE, not
/// the error fallback.
#[must_use]
pub fn ui_locale_tag_from_user_settings(user_settings: &Value) -> LocaleTag {
    let raw = user_settings
        .get("General")
        .and_then(Value::as_object)
        .and_then(|general| general.get(UI_LANGUAGE_KEY))
        .and_then(Value::as_str)
        .map(str::trim);

    match raw {
        Some(tag) if !tag.is_empty() => match LocaleTag::parse(tag) {
            Ok(locale_tag) => locale_tag,
            Err(err) => {
                runtime_log::log_warn(format!(
                    "locale: invalid ui_language '{tag}' ({err}), defaulting to English"
                ));
                LocaleTag::english()
            }
        },
        Some(_) | None => {
            runtime_log::log_warn("locale: ui_language absent, defaulting to English");
            LocaleTag::english()
        }
    }
}

/// Loads a catalog for `tag` from the on-disk `locale/` folder, attaching the
/// on-disk English catalog as the fallback for non-English tags.
///
/// # Errors
/// Propagates read/parse errors so the caller can fall back to the embedded
/// catalog for that tag.
fn load_catalog_from_disk(dir: &Path, tag: &LocaleTag) -> Result<Catalog, LocaleStoreError> {
    let catalog = load_single_catalog(dir, tag)?;
    if tag.is_english() {
        return Ok(catalog);
    }
    // Non-English tags resolve misses through the on-disk English reference.
    let en = load_single_catalog(dir, &LocaleTag::english())?;
    Ok(catalog.with_fallback(std::sync::Arc::new(en)))
}

/// Loads one on-disk locale file (`<tag>.json`) into a [`Catalog`] (no fallback
/// attached).
///
/// # Errors
/// [`LocaleStoreError::File`] if the file cannot be read, or
/// [`LocaleStoreError::Catalog`] if `ms-i18n` rejects its contents.
fn load_single_catalog(dir: &Path, tag: &LocaleTag) -> Result<Catalog, LocaleStoreError> {
    let path = dir.join(format!("{}.json", tag.as_str()));
    let raw = fs::read_to_string(&path).map_err(|source| LocaleStoreError::File {
        path: path.clone(),
        operation: "read",
        source,
    })?;
    // `ms-i18n` owns catalog validation; map its typed error into ours for logging.
    Catalog::from_json_str(tag, &raw).map_err(|err| LocaleStoreError::Catalog {
        path,
        message: err.to_string(),
    })
}

/// Installs the active UI locale: load it from disk, or fall back to the embedded
/// catalog on any failure. Best-effort; never fails the caller.
///
/// Reads `General.ui_language` from `user_settings`, loads that tag's catalog
/// (with English fallback) from the on-disk `locale/` folder, and installs it into
/// the `ms-i18n` runtime. On any disk error it installs the embedded catalog for
/// the same tag, and if even that has no embedded catalog it installs ENGLISH.
///
/// Before installing, it reports the tag's plural-rule resolution ONCE: a tag with
/// no hand-written CLDR rules (a custom language) uses English plural rules, and
/// that fallback is logged here rather than being silent (`ms-i18n` emits no logs).
pub fn install_ui_locale(user_settings: &Value) {
    let tag = ui_locale_tag_from_user_settings(user_settings);
    report_plural_rule_fallback(&tag);
    let dir = config::data_dir().join(LOCALE_DIR_NAME);

    match load_catalog_from_disk(&dir, &tag) {
        Ok(catalog) => {
            ms_i18n::install(catalog);
            runtime_log::log_info(format!("locale: installed '{tag}' from disk"));
        }
        Err(err) => {
            runtime_log::log_warn(format!(
                "locale: could not load '{tag}' from disk, using embedded catalog: {err}"
            ));
            install_embedded_fallback(&tag);
        }
    }
}

/// Logs — once, at install time — when `tag` has no hand-written CLDR plural rules
/// and therefore uses English plural rules. Keeps the plural-fallback observable
/// (CLAUDE.md §14) without touching the hot `tp!` path.
fn report_plural_rule_fallback(tag: &LocaleTag) {
    if ms_i18n::plural_rules_for_tag(tag).fell_back_to_english() {
        runtime_log::log_info(format!(
            "locale: tag '{tag}' has no hand-written plural rules; using English plural rules"
        ));
    }
}

/// Installs the embedded catalog for `tag`, falling back to English if `tag` has no
/// embedded catalog. Used when the on-disk catalog is unavailable.
fn install_embedded_fallback(tag: &LocaleTag) {
    match ms_i18n::set_locale(tag) {
        Ok(()) => {}
        Err(err) => {
            runtime_log::log_warn(format!(
                "locale: no embedded catalog for '{tag}' ({err}), installing English"
            ));
            if let Err(en_err) = ms_i18n::set_locale(&LocaleTag::english()) {
                // English is always embedded; this only fails on a build bug.
                runtime_log::log_error(format!(
                    "locale: failed to install English embedded catalog: {en_err}"
                ));
            }
        }
    }
}

/// Serializes every test in this binary that drives the process-global active
/// catalog (`ms-i18n`'s `ACTIVE` slot, shared across the whole test binary). Any
/// test that installs a locale or asserts on a `t!`/`tf!`/`tp!` result must hold
/// this so a concurrent test's catalog swap cannot be observed mid-assertion.
#[cfg(test)]
pub(crate) static GLOBAL_LOCALE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Builds a `serde_json` object map from a JSON literal (test helper).
    fn map_of(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("test fixture is not a JSON object: {other}"),
        }
    }

    #[test]
    fn absent_key_is_added_from_reference() {
        let user = map_of(json!({ "a": "user-a" }));
        let reference = map_of(json!({ "a": "ref-a", "b": "ref-b" }));
        let outcome = reconcile_locale_map(&user, &reference);
        assert!(outcome.changed);
        // Existing value untouched; missing key taken from the reference.
        assert_eq!(outcome.map.get("a"), Some(&json!("user-a")));
        assert_eq!(outcome.map.get("b"), Some(&json!("ref-b")));
    }

    #[test]
    fn existing_values_are_never_overwritten() {
        let user = map_of(json!({ "a": "user-a", "b": "user-b" }));
        let reference = map_of(json!({ "a": "ref-a", "b": "ref-b" }));
        let outcome = reconcile_locale_map(&user, &reference);
        assert!(!outcome.changed);
        assert_eq!(outcome.map.get("a"), Some(&json!("user-a")));
        assert_eq!(outcome.map.get("b"), Some(&json!("user-b")));
    }

    #[test]
    fn obsolete_user_keys_survive() {
        let user = map_of(json!({ "a": "user-a", "old": "user-old" }));
        let reference = map_of(json!({ "a": "ref-a" }));
        let outcome = reconcile_locale_map(&user, &reference);
        // `old` is not in the reference but must not be deleted.
        assert!(!outcome.changed);
        assert_eq!(outcome.map.get("old"), Some(&json!("user-old")));
    }

    #[test]
    fn user_meta_is_preserved_and_not_backfilled() {
        let user = map_of(json!({ "_meta": { "name": "Deutsch" }, "a": "user-a" }));
        let reference = map_of(json!({ "_meta": { "name": "English" }, "a": "ref-a", "b": "ref-b" }));
        let outcome = reconcile_locale_map(&user, &reference);
        // The user's `_meta` stays; the reference's `_meta` is never copied.
        assert_eq!(outcome.map.get("_meta"), Some(&json!({ "name": "Deutsch" })));
        assert!(outcome.changed);
    }

    #[test]
    fn absent_meta_is_not_added_from_reference() {
        let user = map_of(json!({ "a": "user-a" }));
        let reference = map_of(json!({ "_meta": { "name": "English" }, "a": "ref-a" }));
        let outcome = reconcile_locale_map(&user, &reference);
        // `_meta` is reserved: never backfilled, even when the user lacks it.
        assert!(!outcome.map.contains_key("_meta"));
        assert!(!outcome.changed);
    }

    #[test]
    fn absent_plural_object_is_copied_whole() {
        let user = map_of(json!({ "a": "user-a" }));
        let reference = map_of(json!({
            "a": "ref-a",
            "count": { "one": "{n} item", "other": "{n} items" }
        }));
        let outcome = reconcile_locale_map(&user, &reference);
        assert!(outcome.changed);
        assert_eq!(
            outcome.map.get("count"),
            Some(&json!({ "one": "{n} item", "other": "{n} items" }))
        );
    }

    #[test]
    fn present_plural_object_is_left_alone_even_with_extra_categories() {
        let user = map_of(json!({
            "count": { "one": "1", "few": "few", "many": "many", "other": "other" }
        }));
        let reference = map_of(json!({
            "count": { "one": "{n} item", "other": "{n} items" }
        }));
        let outcome = reconcile_locale_map(&user, &reference);
        // The user's plural object (with extra `few`/`many`) is not merged.
        assert!(!outcome.changed);
        assert_eq!(
            outcome.map.get("count"),
            Some(&json!({ "one": "1", "few": "few", "many": "many", "other": "other" }))
        );
    }

    #[test]
    fn serialized_output_is_sorted_with_meta_first_and_stable() {
        let map = map_of(json!({
            "zeta": "z",
            "_meta": { "name": "X" },
            "alpha": "a"
        }));
        let text = serialize_locale_map(&map).expect("serialize");
        let meta_pos = text.find("\"_meta\"").expect("meta present");
        let alpha_pos = text.find("\"alpha\"").expect("alpha present");
        let zeta_pos = text.find("\"zeta\"").expect("zeta present");
        assert!(meta_pos < alpha_pos, "_meta must come first");
        assert!(alpha_pos < zeta_pos, "keys must be sorted");
        // Stable: serializing again yields identical bytes.
        assert_eq!(text, serialize_locale_map(&map).expect("serialize again"));
    }

    #[test]
    fn absent_file_is_written_verbatim() {
        let dir = tempdir();
        reconcile_dir_at(&dir).expect("reconcile");
        let en_path = dir.join("en.json");
        let written = fs::read_to_string(&en_path).expect("en.json written");
        // The embedded source is written byte-for-byte (verbatim).
        let embedded = ms_i18n::embedded_locales()
            .iter()
            .find(|(tag, _)| *tag == "en")
            .map(|(_, source)| *source)
            .expect("en embedded");
        assert_eq!(written, embedded);
        cleanup(&dir);
    }

    #[test]
    fn unchanged_file_is_not_rewritten() {
        let dir = tempdir();
        // First pass writes the files.
        reconcile_dir_at(&dir).expect("first reconcile");
        let ru_path = dir.join("ru.json");
        let mtime_before = fs::metadata(&ru_path).and_then(|m| m.modified()).expect("mtime");
        // Second pass must not rewrite an already-complete file.
        reconcile_dir_at(&dir).expect("second reconcile");
        let mtime_after = fs::metadata(&ru_path).and_then(|m| m.modified()).expect("mtime");
        assert_eq!(mtime_before, mtime_after, "unchanged file must not be rewritten");
        cleanup(&dir);
    }

    #[test]
    fn present_file_missing_keys_gets_exactly_those_keys() {
        let dir = tempdir();
        fs::create_dir_all(&dir).expect("mkdir");
        let ru_path = dir.join("ru.json");
        // A partial ru file: only one key plus a custom _meta and an obsolete key.
        fs::write(
            &ru_path,
            r#"{ "_meta": { "name": "MyRu" }, "app.tab.translation": "Мой перевод", "obsolete": "keep" }"#,
        )
        .expect("write");
        reconcile_dir_at(&dir).expect("reconcile");
        let raw = fs::read_to_string(&ru_path).expect("read");
        let value: Value = serde_json::from_str(&raw).expect("parse");
        let obj = value.as_object().expect("object");
        // Existing value untouched, obsolete key kept, custom _meta preserved.
        assert_eq!(obj.get("app.tab.translation"), Some(&json!("Мой перевод")));
        assert_eq!(obj.get("obsolete"), Some(&json!("keep")));
        assert_eq!(obj.get("_meta"), Some(&json!({ "name": "MyRu" })));
        // A key missing from the partial file was backfilled from embedded ru.
        assert!(obj.contains_key("app.tab.settings"));
        cleanup(&dir);
    }

    #[test]
    fn custom_language_backfills_from_english_not_native() {
        let dir = tempdir();
        fs::create_dir_all(&dir).expect("mkdir");
        let de_path = dir.join("de.json");
        fs::write(&de_path, r#"{ "_meta": { "name": "Deutsch" } }"#).expect("write");
        reconcile_dir_at(&dir).expect("reconcile");
        let raw = fs::read_to_string(&de_path).expect("read");
        let value: Value = serde_json::from_str(&raw).expect("parse");
        let obj = value.as_object().expect("object");
        // Backfill comes from en.json ("Translation"), never from ru.json.
        assert_eq!(obj.get("app.tab.translation"), Some(&json!("Translation")));
        cleanup(&dir);
    }

    #[test]
    fn corrupt_file_is_left_untouched_and_errors() {
        let dir = tempdir();
        fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("ru.json");
        let corrupt = "{ this is not json";
        fs::write(&path, corrupt).expect("write");
        let reference = embedded_reference_map("ru").expect("ru embedded");
        let result = reconcile_existing_file(&path, &reference);
        assert!(matches!(result, Err(LocaleStoreError::Parse { .. })));
        // The corrupt file is left byte-for-byte intact for manual repair.
        assert_eq!(fs::read_to_string(&path).expect("read"), corrupt);
        cleanup(&dir);
    }

    #[test]
    fn load_catalog_from_disk_resolves_and_falls_back() {
        let dir = tempdir();
        reconcile_dir_at(&dir).expect("reconcile");
        let ru = LocaleTag::parse("ru").expect("ru tag");
        let catalog = load_catalog_from_disk(&dir, &ru).expect("load ru");
        assert_eq!(catalog.tag().as_str(), "ru");
        cleanup(&dir);
    }

    #[test]
    fn absent_tag_resolves_to_english_not_russian() {
        // A blank / missing ui_language must resolve to English (the reference),
        // NOT Russian (which is only the shipped default config value).
        let empty = json!({});
        assert_eq!(
            ui_locale_tag_from_user_settings(&empty).as_str(),
            "en"
        );
        let blank = json!({ "General": { "ui_language": "   " } });
        assert_eq!(
            ui_locale_tag_from_user_settings(&blank).as_str(),
            "en"
        );
        // An invalid tag also resolves to English.
        let invalid = json!({ "General": { "ui_language": "../etc/passwd" } });
        assert_eq!(
            ui_locale_tag_from_user_settings(&invalid).as_str(),
            "en"
        );
        // A valid custom tag is passed through verbatim.
        let custom = json!({ "General": { "ui_language": "de" } });
        assert_eq!(
            ui_locale_tag_from_user_settings(&custom).as_str(),
            "de"
        );
    }

    /// Writes the embedded English source to `<dir>/en.json` verbatim (the disk
    /// English reference the custom-tag fallback chain resolves through).
    fn write_embedded_en(dir: &Path) {
        let en_source = ms_i18n::embedded_locales()
            .iter()
            .find(|(tag, _)| *tag == "en")
            .map(|(_, source)| *source)
            .expect("en embedded");
        fs::write(dir.join("en.json"), en_source).expect("write en.json");
    }

    #[test]
    fn custom_tag_installs_and_missing_key_falls_back_to_english() {
        let _guard = GLOBAL_LOCALE_LOCK.lock().expect("lock");
        let dir = tempdir();
        fs::create_dir_all(&dir).expect("mkdir");
        write_embedded_en(&dir);
        // A minimal `de.json` translating exactly ONE known key; everything else is
        // intentionally absent so the catalog's English fallback must supply it.
        fs::write(
            dir.join("de.json"),
            r#"{ "_meta": { "name": "Deutsch" }, "app.tab.settings": "Einstellungen" }"#,
        )
        .expect("write de.json");

        let de = LocaleTag::parse("de").expect("de tag");
        let catalog = load_catalog_from_disk(&dir, &de).expect("load de");
        ms_i18n::install(catalog);

        // The German value is returned for the translated key.
        assert_eq!(ms_i18n::lookup("app.tab.settings"), Some("Einstellungen"));
        // A key missing from de.json falls back to the English value.
        assert_eq!(ms_i18n::lookup("app.tab.translation"), Some("Translation"));
        cleanup(&dir);
    }

    #[test]
    fn absent_tag_install_falls_back_to_english_runtime() {
        let _guard = GLOBAL_LOCALE_LOCK.lock().expect("lock");
        // A tag with neither a disk file nor an embedded catalog installs English.
        let dir = tempdir(); // never created on disk -> load fails
        let xx = LocaleTag::parse("xx").expect("xx tag");
        assert!(load_catalog_from_disk(&dir, &xx).is_err());
        install_embedded_fallback(&xx);
        // English strings are active (not Russian "Настройки").
        assert_eq!(ms_i18n::lookup("app.tab.settings"), Some("Settings"));
    }

    #[test]
    fn corrupt_custom_file_installs_english_and_leaves_bytes_untouched() {
        let _guard = GLOBAL_LOCALE_LOCK.lock().expect("lock");
        let dir = tempdir();
        fs::create_dir_all(&dir).expect("mkdir");
        write_embedded_en(&dir);
        let de_path = dir.join("de.json");
        let corrupt = "{ this is not valid json";
        fs::write(&de_path, corrupt).expect("write corrupt de.json");

        let de = LocaleTag::parse("de").expect("de tag");
        // Loading a corrupt catalog fails (mapped to a typed Catalog error).
        assert!(matches!(
            load_catalog_from_disk(&dir, &de),
            Err(LocaleStoreError::Catalog { .. })
        ));
        // The install path then falls back to English.
        install_embedded_fallback(&de);
        assert_eq!(ms_i18n::lookup("app.tab.settings"), Some("Settings"));
        // The corrupt file is left byte-for-byte intact for manual repair.
        assert_eq!(fs::read_to_string(&de_path).expect("read"), corrupt);
        cleanup(&dir);
    }

    #[test]
    fn plural_fallback_is_observable_for_custom_tag() {
        // The English plural fallback for a custom tag is an assertable marker, not
        // a silent default (mirrors `report_plural_rule_fallback`).
        let de = LocaleTag::parse("de").expect("de tag");
        assert!(ms_i18n::plural_rules_for_tag(&de).fell_back_to_english());
        let ru = LocaleTag::parse("ru").expect("ru tag");
        assert!(!ms_i18n::plural_rules_for_tag(&ru).fell_back_to_english());
    }

    /// Creates a unique temp directory path for a test (not yet created on disk).
    fn tempdir() -> PathBuf {
        let unique = format!(
            "ms_locale_store_test_{}_{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        std::env::temp_dir().join(unique)
    }

    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Removes a temp directory tree, ignoring errors (best-effort test cleanup).
    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }
}
