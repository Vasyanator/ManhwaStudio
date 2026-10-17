/*
File: cleaning/tools/ai_editor/engines/shared/settings_file.rs

Purpose:
The model-agnostic rules of an engine's own JSON settings file: read it on a worker and MOVE A
DAMAGED ONE ASIDE instead of ever overwriting it, write it back, and keep a failed write
pending with a bounded retry backoff. The engine owns its document type, its path, its
migrations and every user-visible sentence; this file owns only what is the same for all of
them.

Main responsibilities:
- read a settings file into the engine's document (`read_settings_file`): missing -> first
  run, damaged -> quarantined to the first FREE `<stem>.malformed-<unix_secs>[-<n>].json`
  (reserved with `create_new`, never replacing an earlier copy) and defaults, unreadable or
  unquarantinable -> defaults with saving BLOCKED (`SettingsProblem`);
- write a document as pretty JSON, creating the directory (`write_settings_json`, typed
  `SettingsWriteError`);
- drive the save from an engine's `poll`: at most one writer, never before the load landed,
  never while blocked, and a failed write restores `dirty` and retries after 2 s doubling to
  60 s (`SettingsSaver`, `save_retry_delay`).

Key structures:
- `SettingsProblem`, `SettingsFileRead`, `SettingsWriteError`, `SettingsSaver`

Key functions:
- `read_settings_file()`, `malformed_aside_path()`, `write_settings_json()`, `unix_now_secs()`,
  `save_retry_delay()`

Notes:
Every function that touches the disk runs on a worker thread (`ms_thread::spawn` in the engine
or in `SettingsSaver::poll`); nothing here blocks the GUI thread. The save gate itself is
`region_edit_v2::engine_settings::settings_save_due`, shared by every hosted engine.
*/

use crate::tools::region_edit_v2::engine_settings::settings_save_due;
use ms_thread as thread;
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use web_time::{Duration, Instant};

/// Why the settings file could not be used as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tools::ai_editor::engines) enum SettingsProblem {
    /// The file was damaged and has been moved aside to this path; defaults are in use and
    /// saving is allowed (the damaged copy survives under its new name).
    MovedAside(PathBuf),
    /// The file was damaged or unreadable and could NOT be moved aside. Defaults are in use
    /// and SAVING IS BLOCKED, so the file is never overwritten; the I/O reason.
    SaveBlocked(String),
}

/// What reading a settings file produced: the parsed document (`None` = use the engine's
/// defaults: a first run, or a damaged file) and what was wrong with the file, if anything.
#[derive(Debug, Clone)]
pub(in crate::tools::ai_editor::engines) struct SettingsFileRead<D> {
    pub(in crate::tools::ai_editor::engines) document: Option<D>,
    pub(in crate::tools::ai_editor::engines) problem: Option<SettingsProblem>,
}

/// The current Unix time in seconds, the stamp of a quarantined file's aside name; `0` on a
/// clock before the epoch (the name then still is unique through its attempt suffix).
#[must_use]
pub(in crate::tools::ai_editor::engines) fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Reads the settings file at `path` into the engine's document. Runs on a worker thread.
///
/// A missing file is the normal first run (`document: None`, no problem). A file that is not
/// a readable settings document — not UTF-8, not JSON, or refused by `parse` — is NEVER
/// silently replaced: it is moved aside to the first free
/// `<stem>.malformed-<unix_secs>[-<n>].json` beside it and defaults load; if that fails — or
/// the file exists but cannot be read at all — defaults load with saving BLOCKED, so the next
/// save cannot overwrite it. Both cases are logged (under `log_label`, e.g. `"FLUX.2 klein"`)
/// and reported in [`SettingsFileRead::problem`] for the panel.
pub(in crate::tools::ai_editor::engines) fn read_settings_file<D>(
    path: &Path,
    unix_secs: u64,
    log_label: &str,
    parse: impl FnOnce(&Value) -> Result<D, serde_json::Error>,
) -> SettingsFileRead<D> {
    let defaults = |problem| SettingsFileRead { document: None, problem };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return defaults(None),
        Err(err) => {
            ms_log::runtime_log::log_error(format!(
                "[cleaning] {log_label} settings {} exist but cannot be read ({err}); defaults are in use and saving is blocked so the file is not overwritten",
                path.display()
            ));
            return defaults(Some(SettingsProblem::SaveBlocked(err.to_string())));
        }
    };
    // Not UTF-8 is as damaged as not JSON: it is moved aside like any other bad document.
    let parsed = std::str::from_utf8(&bytes)
        .map_err(|err| err.to_string())
        .and_then(|raw| serde_json::from_str::<Value>(raw).map_err(|err| err.to_string()))
        .and_then(|value| parse(&value).map_err(|err| err.to_string()));
    match parsed {
        Ok(document) => SettingsFileRead { document: Some(document), problem: None },
        Err(problem) => match quarantine_settings_file(path, &bytes, unix_secs) {
            Ok(aside) => {
                ms_log::runtime_log::log_warn(format!(
                    "[cleaning] {log_label} settings {} are damaged ({problem}); moved aside to {} and defaults loaded",
                    path.display(),
                    aside.display()
                ));
                defaults(Some(SettingsProblem::MovedAside(aside)))
            }
            Err(err) => {
                ms_log::runtime_log::log_error(format!(
                    "[cleaning] {log_label} settings {} are damaged ({problem}) and could not be moved aside ({err}); defaults are in use and saving is blocked",
                    path.display()
                ));
                defaults(Some(SettingsProblem::SaveBlocked(err.to_string())))
            }
        },
    }
}

/// The `attempt`-th candidate name of a damaged settings file's aside copy:
/// `<dir>/<stem>.malformed-<unix_secs>.json`, then `…-<unix_secs>-<attempt>.json`. A path
/// without a file stem (not a real settings path) uses the stem `settings`.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn malformed_aside_path(path: &Path, unix_secs: u64, attempt: u32) -> PathBuf {
    let stem = path.file_stem().map_or_else(|| "settings".into(), |stem| stem.to_string_lossy());
    if attempt == 0 {
        path.with_file_name(format!("{stem}.malformed-{unix_secs}.json"))
    } else {
        path.with_file_name(format!("{stem}.malformed-{unix_secs}-{attempt}.json"))
    }
}

/// How many aside names are tried before quarantine gives up (and saving is blocked).
const MALFORMED_ASIDE_ATTEMPTS: u32 = 1000;

/// Moves the damaged settings file at `path` (whose content is `bytes`) aside WITHOUT EVER
/// REPLACING AN EXISTING FILE, and returns where it went.
///
/// A rename onto an existing name replaces it on every platform `std` supports, so two
/// quarantines in the same second would destroy the first copy. Instead the aside name is
/// RESERVED atomically with `create_new` (the first free one of `malformed_aside_path`'s
/// candidates), the bytes are written and synced there, and only then is the original
/// removed.
///
/// # Errors
/// The I/O error when no free name was found, the copy could not be written (the partial
/// copy is removed), or the original could not be removed (the copy is kept: it is a
/// faithful duplicate). The caller then blocks saving.
fn quarantine_settings_file(path: &Path, bytes: &[u8], unix_secs: u64) -> std::io::Result<PathBuf> {
    use std::io::Write;
    for attempt in 0..MALFORMED_ASIDE_ATTEMPTS {
        let aside = malformed_aside_path(path, unix_secs, attempt);
        let mut file = match fs::OpenOptions::new().write(true).create_new(true).open(&aside) {
            Ok(file) => file,
            // Taken (a file, a directory, anything): try the next name, never replace it.
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };
        if let Err(err) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            drop(file);
            if let Err(cleanup) = fs::remove_file(&aside) {
                ms_log::runtime_log::log_warn(format!(
                    "[cleaning] could not remove the partial aside copy {} ({cleanup})",
                    aside.display()
                ));
            }
            return Err(err);
        }
        fs::remove_file(path)?;
        return Ok(aside);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("no free aside name after {MALFORMED_ASIDE_ATTEMPTS} attempts"),
    ))
}

/// Why a settings write failed. The engine turns it into its own user-facing sentence.
#[derive(Debug, thiserror::Error)]
pub(in crate::tools::ai_editor::engines) enum SettingsWriteError {
    /// The directory the file lives in could not be created.
    #[error("{0}")]
    CreateDir(std::io::Error),
    /// The document could not be serialized.
    #[error("{0}")]
    Serialize(serde_json::Error),
    /// The file could not be written.
    #[error("{0}")]
    Write(std::io::Error),
}

/// Writes `document` to `path` as pretty JSON, creating the parent directory first. Runs on a
/// worker thread.
///
/// # Errors
/// [`SettingsWriteError`] naming the step that failed.
pub(in crate::tools::ai_editor::engines) fn write_settings_json<T: Serialize>(path: &Path, document: &T) -> Result<(), SettingsWriteError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(SettingsWriteError::CreateDir)?;
    }
    let raw = serde_json::to_string_pretty(document).map_err(SettingsWriteError::Serialize)?;
    fs::write(path, raw).map_err(SettingsWriteError::Write)
}

/// How long to wait before retrying the settings write after `failures` consecutive
/// failures (`>= 1`): 2 s, doubling, capped at 60 s.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn save_retry_delay(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(5);
    Duration::from_secs((2_u64 << exponent).min(60))
}

/// What the save gate reads from the engine for one poll.
#[derive(Debug)]
pub(in crate::tools::ai_editor::engines) struct SaveGate<'a> {
    /// Raised by every parameter change; cleared when a write starts, restored when it fails.
    pub(in crate::tools::ai_editor::engines) dirty: &'a mut bool,
    /// The initial load has landed: writing before it would overwrite the user's file with
    /// the in-memory defaults.
    pub(in crate::tools::ai_editor::engines) loaded: bool,
    /// The file could not be read or moved aside: it is NEVER written this session.
    pub(in crate::tools::ai_editor::engines) blocked: bool,
}

/// The settings writer of one engine: the write in flight and the failure state.
///
/// `write` is the function that writes the file — the engine's own saver in the product; a
/// test injects a failing one (it must never write the user's real file).
pub(in crate::tools::ai_editor::engines) struct SettingsSaver<T> {
    /// The settings write in flight; answers whether it succeeded.
    pub(in crate::tools::ai_editor::engines) rx: Option<Receiver<Result<(), String>>>,
    pub(in crate::tools::ai_editor::engines) write: fn(&T) -> Result<(), String>,
    /// The last write's failure, as the engine described it, shown at the top of the panel
    /// until a write succeeds.
    pub(in crate::tools::ai_editor::engines) error: Option<String>,
    /// When a failed write may be retried; `None` when nothing failed. Bounded backoff, so a
    /// read-only disk costs one attempt per interval, not one per frame.
    pub(in crate::tools::ai_editor::engines) retry_at: Option<Instant>,
    /// Consecutive failed writes; drives the backoff ([`save_retry_delay`]).
    pub(in crate::tools::ai_editor::engines) failures: u32,
}

impl<T: Clone + Send + 'static> SettingsSaver<T> {
    /// A saver with nothing in flight and nothing failed.
    pub(in crate::tools::ai_editor::engines) fn new(write: fn(&T) -> Result<(), String>) -> Self {
        Self { rx: None, write, error: None, retry_at: None, failures: 0 }
    }

    /// Whether a retry is pending, i.e. the GUI must be woken when its backoff elapses.
    pub(in crate::tools::ai_editor::engines) fn retry_pending(&self) -> bool {
        self.retry_at.is_some()
    }

    /// Drains the write in flight and starts the next one when it is due.
    ///
    /// Driven from the engine's `poll` and from nowhere else, which is why the host must poll
    /// the selected engine EVERY frame, panel visible or not. A blocked file is never written
    /// (the edits stay in memory, `dirty` kept). A FAILED write is not success: `dirty` is
    /// restored, `describe(err)` becomes the shown [`Self::error`], and the next attempt waits
    /// [`save_retry_delay`]. A successful write clears all of it. `settings` is cloned onto
    /// the writer thread only when a write actually starts; `log_label` names the engine in the
    /// failure log.
    pub(in crate::tools::ai_editor::engines) fn poll(
        &mut self,
        now: Instant,
        gate: SaveGate<'_>,
        settings: &T,
        log_label: &str,
        describe: impl FnOnce(&str) -> String,
    ) {
        let SaveGate { dirty, loaded, blocked } = gate;
        if let Some(rx) = self.rx.as_ref() {
            let outcome = match rx.try_recv() {
                Ok(outcome) => outcome,
                Err(TryRecvError::Disconnected) => Err(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string()),
                Err(TryRecvError::Empty) => return,
            };
            self.rx = None;
            match outcome {
                Ok(()) => {
                    self.error = None;
                    self.retry_at = None;
                    self.failures = 0;
                }
                Err(err) => {
                    *dirty = true;
                    self.failures = self.failures.saturating_add(1);
                    let delay = save_retry_delay(self.failures);
                    self.retry_at = Some(now + delay);
                    ms_log::runtime_log::log_warn(format!(
                        "[cleaning] failed to save {log_label} settings (attempt {}): {err}; retrying in {} s",
                        self.failures,
                        delay.as_secs()
                    ));
                    self.error = Some(describe(&err));
                }
            }
        }
        if blocked {
            return;
        }
        if self.retry_at.is_some_and(|at| now < at) {
            return;
        }
        if !settings_save_due(*dirty, loaded, self.rx.is_some()) {
            return;
        }
        *dirty = false;
        let settings = settings.clone();
        let write = self.write;
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        thread::spawn(move || {
            // A failed send means the engine was dropped meanwhile: nobody waits for it.
            let _ = tx.send(write(&settings));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A document type for the tests: any JSON object with an optional `steps` number.
    fn parse_steps(value: &Value) -> Result<u64, serde_json::Error> {
        #[derive(serde::Deserialize)]
        struct Doc {
            #[serde(default)]
            steps: u64,
        }
        serde_json::from_value::<Doc>(value.clone()).map(|doc| doc.steps)
    }

    /// A damaged file is MOVED ASIDE, never overwritten; a missing one is a first run; a
    /// readable one loads. Scratch files live under the OS temp dir.
    #[test]
    fn a_damaged_settings_file_is_moved_aside() {
        let dir = std::env::temp_dir().join(format!("ms_engine_settings_file_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        let path = dir.join("flux2_klein_settings.json");

        let missing = read_settings_file(&path, 7, "test", parse_steps);
        assert!(missing.problem.is_none() && missing.document.is_none());

        for damaged in ["{ not json", "", "[1, 2]", "{\"steps\": \"many\"}"] {
            std::fs::write(&path, damaged).expect("write the damaged file");
            let read = read_settings_file(&path, 42, "test", parse_steps);
            let aside = malformed_aside_path(&path, 42, 0);
            assert_eq!(read.problem, Some(SettingsProblem::MovedAside(aside.clone())), "{damaged:?}");
            assert!(read.document.is_none());
            assert!(!path.exists(), "the damaged file left its place");
            assert_eq!(std::fs::read_to_string(&aside).expect("the aside copy"), damaged, "kept byte for byte");
            std::fs::remove_file(&aside).expect("remove the aside copy");
        }
        assert_eq!(malformed_aside_path(&path, 42, 0).file_name().and_then(|n| n.to_str()), Some("flux2_klein_settings.malformed-42.json"));
        assert_eq!(malformed_aside_path(&path, 42, 3).file_name().and_then(|n| n.to_str()), Some("flux2_klein_settings.malformed-42-3.json"));

        std::fs::write(&path, r#"{ "steps": 6 }"#).expect("write a valid file");
        let valid = read_settings_file(&path, 1, "test", parse_steps);
        assert!(valid.problem.is_none());
        assert_eq!(valid.document, Some(6));
        std::fs::remove_file(&path).expect("remove the valid file");

        std::fs::remove_dir_all(&dir).expect("remove the scratch dir");
    }

    /// Two quarantines in the same second: the earlier aside copy (a REGULAR FILE at the exact
    /// first name) survives byte for byte, and the new one gets the next free name.
    #[test]
    fn a_quarantine_never_replaces_an_earlier_aside_copy() {
        let dir = std::env::temp_dir().join(format!("ms_engine_settings_file_collision_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        let path = dir.join("flux2_klein_settings.json");
        let first = malformed_aside_path(&path, 9, 0);
        std::fs::write(&first, "the earlier damaged file").expect("pre-create the first aside");
        std::fs::write(&path, "{ broken now").expect("write the damaged file");

        let read = read_settings_file(&path, 9, "test", parse_steps);
        let second = malformed_aside_path(&path, 9, 1);
        assert_eq!(read.problem, Some(SettingsProblem::MovedAside(second.clone())));
        assert_eq!(std::fs::read_to_string(&first).expect("the earlier copy"), "the earlier damaged file");
        assert_eq!(std::fs::read_to_string(&second).expect("the new copy"), "{ broken now");
        assert!(!path.exists());

        // A settings "file" that cannot be read at all (here a directory) blocks saving.
        std::fs::create_dir_all(&path).expect("occupy the settings name with a directory");
        let blocked = read_settings_file(&path, 9, "test", parse_steps);
        assert!(matches!(blocked.problem, Some(SettingsProblem::SaveBlocked(_))), "{:?}", blocked.problem);
        assert!(path.is_dir(), "nothing touched it");
        std::fs::remove_dir_all(&dir).expect("remove the scratch dir");
    }

    #[test]
    fn the_save_retry_backs_off_and_is_bounded() {
        assert_eq!(save_retry_delay(1), Duration::from_secs(2));
        assert_eq!(save_retry_delay(2), Duration::from_secs(4));
        assert_eq!(save_retry_delay(5), Duration::from_secs(32));
        assert_eq!(save_retry_delay(6), Duration::from_secs(60));
        assert_eq!(save_retry_delay(u32::MAX), Duration::from_secs(60));
    }

    /// The written document reads back, and the directory is created on the way.
    #[test]
    fn a_written_document_reads_back() {
        let dir = std::env::temp_dir().join(format!("ms_engine_settings_file_write_{}", std::process::id()));
        let path = dir.join("nested").join("engine_settings.json");
        write_settings_json(&path, &serde_json::json!({ "steps": 3 })).expect("the write succeeds");
        assert_eq!(read_settings_file(&path, 1, "test", parse_steps).document, Some(3));
        std::fs::remove_dir_all(&dir).expect("remove the scratch dir");
    }
}
