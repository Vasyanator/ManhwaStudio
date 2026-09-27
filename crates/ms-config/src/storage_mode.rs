/*
File: storage_mode.rs

Purpose:
The global "data storage mode" setting (`General.storage_mode`): Prod keeps every global and
title-level document as a SQLite `<stem>.db`, Dev as a `<stem>.json` (same stem). This file
owns the typed value, its frozen config spelling, the startup probe that seeds
`ms_docstore::set_default_format` before any other document is touched, and the locations
of the two documents that live in the application fonts directory.

Key structures:
- StorageMode          : Prod (default) / Dev, with `as_config_str` / `from_config_str`
- StorageStartupProbe  : what the startup probe found (mode, user_config format, pending)

Key functions:
- storage_mode_from_user_settings() : read the mode out of a user-config value
- probe_storage_mode()              : pure probe of one user-config document
- init_storage_mode_at_startup()    : probe the real user_config + set the docstore default
- app_fonts_dir() / fonts_data_doc() / fonts_presets_doc()

Notes:
- The conversion DRIVER (which documents, in which order) lives in `ms-project`
  (`storage_mode::convert_globals`); this crate only knows the key and the probe.
- On wasm32 the storage is Dev-only (`DocFormat::Db` is unsupported there); the web entry
  never calls `init_storage_mode_at_startup`, so the docstore default stays JSON.
*/

use std::path::{Path, PathBuf};

use ms_docstore::{DocFormat, DocKind, DocRef};
use ms_log::runtime_log;
use serde_json::Value;

/// `General` key holding the storage mode. Frozen spelling (persisted; listed in
/// `dev-docs/i18n_exclusions.md`), values [`StorageMode::as_config_str`].
pub const GENERAL_STORAGE_MODE_KEY: &str = "storage_mode";

/// File name of the per-font settings document inside [`app_fonts_dir`].
pub const FONTS_DATA_FILE: &str = "fonts_data.json";
/// File name of the typing-preset document inside [`app_fonts_dir`].
pub const FONTS_PRESETS_FILE: &str = "presets.json";

/// Where global and title-level documents are stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum StorageMode {
    /// `SQLite` `<stem>.db` documents (the default; native only).
    #[default]
    Prod,
    /// Plain `<stem>.json` documents (human-readable; the only mode on the web build).
    Dev,
}

impl StorageMode {
    /// Every mode, in UI order.
    pub const ALL: [Self; 2] = [Self::Prod, Self::Dev];

    /// The persisted spelling (frozen: `"prod"` / `"dev"`).
    #[must_use]
    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::Prod => "prod",
            Self::Dev => "dev",
        }
    }

    /// Parses a persisted spelling (trimmed, case-insensitive); `None` for anything else.
    #[must_use]
    pub fn from_config_str(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "prod" => Some(Self::Prod),
            "dev" => Some(Self::Dev),
            _ => None,
        }
    }

    /// The document format this mode stores documents in.
    #[must_use]
    pub fn doc_format(self) -> DocFormat {
        match self {
            Self::Prod => DocFormat::Db,
            Self::Dev => DocFormat::Json,
        }
    }

    /// The mode whose documents have `format`.
    #[must_use]
    pub fn from_doc_format(format: DocFormat) -> Self {
        match format {
            DocFormat::Db => Self::Prod,
            DocFormat::Json => Self::Dev,
        }
    }
}

/// Reads `General.storage_mode` out of a user-config value. A missing value resolves
/// silently to the default ([`StorageMode::Prod`]); a present but non-string or unknown
/// value also resolves to Prod and logs a warning (mirrors `docstore.py`).
#[must_use]
pub fn storage_mode_from_user_settings(user_settings: &Value) -> StorageMode {
    let Some(raw) = user_settings.get("General").and_then(Value::as_object).and_then(|general| general.get(GENERAL_STORAGE_MODE_KEY)) else {
        return StorageMode::default();
    };
    if let Some(mode) = raw.as_str().and_then(StorageMode::from_config_str) {
        return mode;
    }
    let fallback = StorageMode::default();
    runtime_log::log_warn(format!("[storage-mode] unknown General.{GENERAL_STORAGE_MODE_KEY} value {raw}; using \"{}\"", fallback.as_config_str()));
    fallback
}

/// Result of probing the user-config document at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageStartupProbe {
    /// The mode the process runs in (its format becomes the docstore default).
    pub mode: StorageMode,
    /// The format the user-config document is stored in, `None` when it does not exist
    /// yet (fresh install) or could not be determined.
    pub user_config_format: Option<DocFormat>,
    /// Whether the global documents must be reconciled to `mode` (the user-config
    /// document, converted LAST by the driver, is still in the other format: an upgrade
    /// or an interrupted/partially failed conversion).
    pub reconciliation_pending: bool,
}

/// Probes the user-config document `doc` WITHOUT changing any global state.
///
/// - absent (fresh install) → [`StorageMode::Prod`], nothing pending (it is created in
///   the mode's format);
/// - readable → the mode it records (missing key ⇒ Prod); pending when exactly one file
///   exists and it is not in the mode's format. When BOTH files exist (a crash inside the
///   final user-config conversion) nothing is pending: the file of the mode's format is
///   authoritative once the default is set, and the docstore's locked access removes the
///   Value-equal leftover;
/// - unreadable/malformed → logged; the mode follows the format of the file that exists
///   (so nothing is converted around a document the user must repair), nothing pending.
#[must_use]
pub fn probe_storage_mode(doc: &DocRef) -> StorageStartupProbe {
    let detected = ms_docstore::actual_format(doc);
    let value = ms_docstore::read_value(doc);
    let (mode, user_config_format) = match (value, detected) {
        (Ok(None), _) => return StorageStartupProbe { mode: StorageMode::default(), user_config_format: None, reconciliation_pending: false },
        (Ok(Some(value)), Ok(format)) => (storage_mode_from_user_settings(&value), format),
        (Ok(Some(value)), Err(err)) => {
            runtime_log::log_warn(format!("[storage-mode] could not determine the format of {}; assuming no reconciliation is needed; error={err}", doc.stem().display()));
            return StorageStartupProbe { mode: storage_mode_from_user_settings(&value), user_config_format: None, reconciliation_pending: false };
        }
        (Err(err), format) => {
            let format = format.ok().flatten();
            let mode = format.map_or_else(StorageMode::default, StorageMode::from_doc_format);
            runtime_log::log_error(format!(
                "[storage-mode] the user config cannot be read; running in the mode of its current file and converting nothing.\nDocument: {}\nMode: {}\nError: {err}",
                doc.stem().display(),
                mode.as_config_str()
            ));
            return StorageStartupProbe { mode, user_config_format: format, reconciliation_pending: false };
        }
    };
    let both_exist = [DocFormat::Json, DocFormat::Db].iter().all(|format| doc.path_for(*format).is_file());
    let reconciliation_pending = !both_exist && user_config_format.is_some_and(|format| format != mode.doc_format());
    StorageStartupProbe { mode, user_config_format, reconciliation_pending }
}

/// Startup seed of the storage mode: probes the real user-config document and sets
/// `ms_docstore::set_default_format(mode)`. MUST run before any other document access of
/// the process (the "both files exist" rule of the store uses the default format), i.e.
/// before `mark_first_run_languages_if_needed`. Native only.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn init_storage_mode_at_startup() -> StorageStartupProbe {
    let probe = probe_storage_mode(&crate::user_config_doc());
    ms_docstore::set_default_format(probe.mode.doc_format());
    runtime_log::log_info(format!(
        "[storage-mode] mode={} user_config_format={} reconciliation_pending={}",
        probe.mode.as_config_str(),
        probe.user_config_format.map_or("absent", DocFormat::extension),
        probe.reconciliation_pending
    ));
    probe
}

/// The application fonts directory holding `fonts_data` and `presets`: `<cwd>/fonts` when
/// it is a directory, else `<exe dir>/fonts` when that is one, else the relative `fonts`.
/// The single owner of this rule: the typing tab (which owns those two documents'
/// contents) and the storage-mode conversion both resolve the directory here.
#[must_use]
pub fn app_fonts_dir() -> PathBuf {
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join("fonts");
        if candidate.is_dir() {
            return candidate;
        }
    }
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        let candidate = exe_dir.join("fonts");
        if candidate.is_dir() {
            return candidate;
        }
    }
    PathBuf::from("fonts")
}

/// The `fonts_data` document of the fonts directory `fonts_dir`.
#[must_use]
pub fn fonts_data_doc(fonts_dir: &Path) -> DocRef {
    DocRef::new(fonts_dir.join(FONTS_DATA_FILE), DocKind::FontsData)
}

/// The `presets` document of the fonts directory `fonts_dir`.
#[must_use]
pub fn fonts_presets_doc(fonts_dir: &Path) -> DocRef {
    DocRef::new(fonts_dir.join(FONTS_PRESETS_FILE), DocKind::Presets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn config_spelling_round_trips_and_is_frozen() {
        assert_eq!(StorageMode::Prod.as_config_str(), "prod");
        assert_eq!(StorageMode::Dev.as_config_str(), "dev");
        for mode in StorageMode::ALL {
            assert_eq!(StorageMode::from_config_str(mode.as_config_str()), Some(mode));
        }
        assert_eq!(StorageMode::from_config_str(" DEV "), Some(StorageMode::Dev));
        assert_eq!(StorageMode::from_config_str("sqlite"), None);
        assert_eq!(StorageMode::default(), StorageMode::Prod);
    }

    #[test]
    fn mode_maps_to_format_both_ways() {
        assert_eq!(StorageMode::Prod.doc_format(), DocFormat::Db);
        assert_eq!(StorageMode::Dev.doc_format(), DocFormat::Json);
        for mode in StorageMode::ALL {
            assert_eq!(StorageMode::from_doc_format(mode.doc_format()), mode);
        }
    }

    #[test]
    fn reading_the_mode_defaults_to_prod() {
        assert_eq!(storage_mode_from_user_settings(&json!({})), StorageMode::Prod);
        assert_eq!(storage_mode_from_user_settings(&json!({"General": {"storage_mode": 3}})), StorageMode::Prod);
        assert_eq!(storage_mode_from_user_settings(&json!({"General": {"storage_mode": "bogus"}})), StorageMode::Prod);
        assert_eq!(storage_mode_from_user_settings(&json!({"General": {"storage_mode": "dev"}})), StorageMode::Dev);
    }

    #[test]
    fn defaults_tree_carries_prod() {
        let defaults = crate::user_config_defaults();
        assert_eq!(defaults["General"][GENERAL_STORAGE_MODE_KEY], json!("prod"));
    }

    fn user_config_in(dir: &Path) -> DocRef {
        DocRef::new(dir.join(crate::USER_CONFIG_FILE), DocKind::UserConfig)
    }

    fn write_json(doc: &DocRef, value: &Value) -> Result<(), Box<dyn std::error::Error>> {
        std::fs::write(doc.path_for(DocFormat::Json), serde_json::to_vec_pretty(value)?)?;
        Ok(())
    }

    #[test]
    fn probe_fresh_install_is_prod_and_not_pending() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let probe = probe_storage_mode(&user_config_in(temp.path()));
        assert_eq!(probe, StorageStartupProbe { mode: StorageMode::Prod, user_config_format: None, reconciliation_pending: false });
        Ok(())
    }

    #[test]
    fn probe_upgrade_json_without_key_is_pending_prod() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let doc = user_config_in(temp.path());
        write_json(&doc, &json!({"General": {"theme": "dark"}}))?;
        let probe = probe_storage_mode(&doc);
        assert_eq!(probe, StorageStartupProbe { mode: StorageMode::Prod, user_config_format: Some(DocFormat::Json), reconciliation_pending: true });
        Ok(())
    }

    #[test]
    fn probe_json_in_dev_mode_is_settled() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let doc = user_config_in(temp.path());
        write_json(&doc, &json!({"General": {"storage_mode": "dev"}}))?;
        let probe = probe_storage_mode(&doc);
        assert_eq!(probe, StorageStartupProbe { mode: StorageMode::Dev, user_config_format: Some(DocFormat::Json), reconciliation_pending: false });
        Ok(())
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn probe_db_reads_the_mode_from_the_db() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let doc = user_config_in(temp.path());
        ms_docstore::write_whole_atomic(&doc, &json!({"General": {"storage_mode": "prod"}}), DocFormat::Db)?;
        assert_eq!(probe_storage_mode(&doc), StorageStartupProbe { mode: StorageMode::Prod, user_config_format: Some(DocFormat::Db), reconciliation_pending: false });
        // A `.db` that records Dev (the switch persisted the key, then the final
        // conversion failed or was interrupted) must be reconciled back to JSON.
        ms_docstore::write_whole_atomic(&doc, &json!({"General": {"storage_mode": "dev"}}), DocFormat::Db)?;
        assert_eq!(probe_storage_mode(&doc), StorageStartupProbe { mode: StorageMode::Dev, user_config_format: Some(DocFormat::Db), reconciliation_pending: true });
        Ok(())
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn probe_both_files_is_not_pending() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let doc = user_config_in(temp.path());
        let value = json!({"General": {"storage_mode": "prod"}});
        write_json(&doc, &value)?;
        ms_docstore::write_whole_atomic(&doc, &value, DocFormat::Db)?;
        let probe = probe_storage_mode(&doc);
        assert_eq!(probe.mode, StorageMode::Prod);
        assert!(!probe.reconciliation_pending);
        Ok(())
    }

    #[test]
    fn probe_malformed_follows_the_existing_file_and_converts_nothing() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let doc = user_config_in(temp.path());
        std::fs::write(doc.path_for(DocFormat::Json), b"{ not json")?;
        let probe = probe_storage_mode(&doc);
        assert_eq!(probe, StorageStartupProbe { mode: StorageMode::Dev, user_config_format: Some(DocFormat::Json), reconciliation_pending: false });
        // Nothing was rewritten.
        assert_eq!(std::fs::read(doc.path_for(DocFormat::Json))?, b"{ not json");
        Ok(())
    }
}
