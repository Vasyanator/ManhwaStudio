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
- write a document as pretty JSON, creating the directory, and replace the file ATOMICALLY
  (unique sibling temp `.<name>.<pid>.<n>.tmp`, synced, renamed over it; `write_settings_json`,
  typed `SettingsWriteError`);
- drive the save from an engine's `poll`: at most one writer, never before the load landed,
  never while blocked, only after `SETTINGS_SAVE_DEBOUNCE` (700 ms) without a change (detected
  by value), a failed write restores `dirty` and retries after 2 s doubling to 60 s, and the
  GUI is woken per `wake_after` (`SettingsSaver`, `save_retry_delay`);
- flush a value still owed when the saver is dropped, on a detached worker that first waits for
  the write in flight (`impl Drop for SettingsSaver`).

Key structures:
- `SettingsProblem`, `SettingsFileRead`, `SettingsWriteError`, `SettingsSaver`

Key functions:
- `read_settings_file()`, `malformed_aside_path()`, `write_settings_json()`, `unix_now_secs()`,
  `save_retry_delay()`, `SettingsSaver::{poll, wake_after}`

Notes:
Every function that touches the disk runs on a worker thread (`ms_thread::spawn` in the engine,
in `SettingsSaver::poll` or in its `Drop`); nothing here blocks the GUI thread. The save gate itself is
`region_edit_v2::engine_settings::settings_save_due`, shared by every hosted engine.
*/

use crate::tools::region_edit_v2::engine_settings::settings_save_due;
use ms_thread as thread;
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Writes `document` to `path` as pretty JSON, creating the parent directory first, and
/// REPLACES the file atomically: the bytes go to a fresh sibling temp file
/// ([`settings_temp_path`]), are synced, and only then is the temp renamed over `path`. A
/// crash or a full disk mid-write therefore leaves the previous complete file (or the new
/// complete one), never a truncated document the next load would have to quarantine. Runs on
/// a worker thread.
///
/// The rename replaces an existing file on every target: `rename(2)` on unix, and on Windows
/// `std::fs::rename` is `MoveFileExW(.., MOVEFILE_REPLACE_EXISTING)` (std
/// `sys/fs/windows.rs` `rename`). The directory entry itself is not synced: after a power
/// loss either the old or the new complete file is there, both readable.
///
/// # Errors
/// [`SettingsWriteError`] naming the step that failed. On a failed write, sync or rename the
/// temp file is removed (a failed removal is logged) and `path` is left exactly as it was.
pub(in crate::tools::ai_editor::engines) fn write_settings_json<T: Serialize>(path: &Path, document: &T) -> Result<(), SettingsWriteError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(SettingsWriteError::CreateDir)?;
    }
    let raw = serde_json::to_string_pretty(document).map_err(SettingsWriteError::Serialize)?;
    replace_file_atomically(path, raw.as_bytes()).map_err(SettingsWriteError::Write)
}

/// Process-wide counter that makes every temp name of [`settings_temp_path`] unique, so two
/// writers (two engines, or an exit flush behind a write) never share a temp file.
static SETTINGS_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The `serial`-th temp name for a write of `path`: `<dir>/.<file name>.<pid>.<serial>.tmp`.
///
/// It can never be taken for a settings document or a quarantine copy: settings files are
/// opened by their exact name, aside copies are `<stem>.malformed-*.json`, and this name
/// starts with a dot and ends in `.tmp`.
#[must_use]
fn settings_temp_path(path: &Path, serial: u64) -> PathBuf {
    let name = path.file_name().map_or_else(|| "settings".into(), |name| name.to_string_lossy());
    path.with_file_name(format!(".{name}.{}.{serial}.tmp", std::process::id()))
}

/// Writes `bytes` into a fresh temp file beside `path` (reserved with `create_new`), syncs it
/// and renames it over `path`.
///
/// # Errors
/// The I/O error of the step that failed; the temp file is removed first, so nothing is left
/// behind and `path` is untouched.
fn replace_file_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let temp = settings_temp_path(path, SETTINGS_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed));
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    // Closed before the rename: Windows refuses to rename a file that is still open.
    drop(file);
    if let Err(err) = written.and_then(|()| fs::rename(&temp, path)) {
        if let Err(cleanup) = fs::remove_file(&temp) {
            ms_log::runtime_log::log_warn(format!(
                "[cleaning] could not remove the settings temp file {} ({cleanup})",
                temp.display()
            ));
        }
        return Err(err);
    }
    Ok(())
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

/// The quiet time a settings change must survive before its write starts: a dragged dial
/// changes the settings on every frame, and only the value it settles on is worth a disk
/// write. The same 700 ms as `ms_config::config_saver`'s private `SAVE_DEBOUNCE`, which a
/// drag of a GUI-driven config section waits for.
pub(in crate::tools::ai_editor::engines) const SETTINGS_SAVE_DEBOUNCE: Duration = Duration::from_millis(700);

/// How often the GUI is woken while a write is in flight, so its outcome (and a newer change
/// waiting behind it) is picked up without user input. A write of a small JSON file lands
/// well within it.
const SAVE_IN_FLIGHT_POLL: Duration = Duration::from_millis(100);

/// The settings writer of one engine: the change waiting out its quiet time, the write in
/// flight and the failure state.
///
/// A change is detected BY VALUE: while the engine's `dirty` is set, every poll compares the
/// settings with the last value it saw ([`Self::pending`]); a difference restarts the quiet
/// time ([`SETTINGS_SAVE_DEBOUNCE`]). So every place that raises `dirty` gets the debounce
/// without telling the saver anything.
///
/// Dropping the saver with a value still owed to the file (waiting out its quiet time, or
/// changed while the previous write was in flight) writes that value on a detached worker
/// thread — after the in-flight write ended, so the newest value is renamed last — and logs
/// a failure; the dropping thread never waits.
///
/// `write` is the function that writes the file — the engine's own saver in the product; a
/// test injects a failing or recording one (it must never write the user's real file).
pub(in crate::tools::ai_editor::engines) struct SettingsSaver<T: Clone + PartialEq + Send + 'static> {
    /// The settings write in flight; answers whether it succeeded.
    pub(in crate::tools::ai_editor::engines) rx: Option<Receiver<Result<(), String>>>,
    pub(in crate::tools::ai_editor::engines) write: fn(&T) -> Result<(), String>,
    /// Names the engine in the log lines (e.g. `"FLUX.2 klein"`).
    log_label: &'static str,
    /// The last failure, as the engine described it, shown at the top of the panel
    /// until a write succeeds.
    pub(in crate::tools::ai_editor::engines) error: Option<String>,
    /// When a failed write may be retried; `None` when nothing failed. Bounded backoff, so a
    /// read-only disk costs one attempt per interval, not one per frame.
    pub(in crate::tools::ai_editor::engines) retry_at: Option<Instant>,
    /// Consecutive failed writes; drives the backoff ([`save_retry_delay`]).
    pub(in crate::tools::ai_editor::engines) failures: u32,
    /// The value owed to the file: the settings as last seen while dirty, loaded and not
    /// blocked; `None` when nothing is owed. Moved into the write when it starts, and the
    /// payload of the teardown flush.
    pending: Option<T>,
    /// When [`Self::pending`] last changed: the start of its quiet time.
    changed_at: Option<Instant>,
}

impl<T: Clone + PartialEq + Send + 'static> SettingsSaver<T> {
    /// A saver with nothing owed, nothing in flight and nothing failed.
    pub(in crate::tools::ai_editor::engines) fn new(write: fn(&T) -> Result<(), String>, log_label: &'static str) -> Self {
        Self { rx: None, write, log_label, error: None, retry_at: None, failures: 0, pending: None, changed_at: None }
    }

    /// How long the GUI may sleep before the saver needs another poll at `now`: when the
    /// quiet time of a pending change ends, when a failed write may be retried, or shortly
    /// while a write is in flight. `None` when nothing waits (the engine then needs no
    /// wake-up for its settings). `Some(Duration::ZERO)` means "poll on the next frame".
    #[must_use]
    pub(in crate::tools::ai_editor::engines) fn wake_after(&self, now: Instant) -> Option<Duration> {
        if self.rx.is_some() {
            return Some(SAVE_IN_FLIGHT_POLL);
        }
        let quiet_end = self.changed_at.filter(|_| self.pending.is_some()).map(|at| at + SETTINGS_SAVE_DEBOUNCE);
        let due = match (quiet_end, self.retry_at) {
            (Some(quiet), Some(retry)) => Some(quiet.max(retry)),
            // A retry with no pending value cannot start; nothing to wake for.
            (None, _) => None,
            (Some(quiet), None) => Some(quiet),
        };
        due.map(|at| at.saturating_duration_since(now))
    }

    /// Drains the write in flight and starts the next one when it is due.
    ///
    /// Driven from the engine's `poll` and from nowhere else, which is why the host must poll
    /// the selected engine EVERY frame, panel visible or not (and wake it per
    /// [`Self::wake_after`]). Before the load landed and while blocked nothing is owed or
    /// written (the edits stay in memory, `dirty` kept). A dirty value starts its write only
    /// once it stayed unchanged for [`SETTINGS_SAVE_DEBOUNCE`], no write is in flight and no
    /// retry backoff runs. A FAILED write is not success: `dirty` is restored,
    /// `describe(err)` becomes the shown [`Self::error`], and the next attempt waits
    /// [`save_retry_delay`]. A successful write clears all of it. `settings` is cloned only
    /// when it differs from the value already owed.
    pub(in crate::tools::ai_editor::engines) fn poll(
        &mut self,
        now: Instant,
        gate: SaveGate<'_>,
        settings: &T,
        describe: impl FnOnce(&str) -> String,
    ) {
        let SaveGate { dirty, loaded, blocked } = gate;
        if let Some(rx) = self.rx.as_ref() {
            let outcome = match rx.try_recv() {
                Ok(outcome) => Some(outcome),
                Err(TryRecvError::Disconnected) => Some(Err(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string())),
                Err(TryRecvError::Empty) => None,
            };
            if let Some(outcome) = outcome {
                self.rx = None;
                self.finish_write(now, outcome, dirty, describe);
            }
        }
        if blocked || !loaded {
            // Never owed: an exit flush must not write a blocked file, nor the in-memory
            // defaults over a file whose load has not landed.
            self.pending = None;
            self.changed_at = None;
            return;
        }
        if !*dirty {
            self.pending = None;
            self.changed_at = None;
            return;
        }
        if self.pending.as_ref() != Some(settings) {
            self.pending = Some(settings.clone());
            self.changed_at = Some(now);
        }
        if self.rx.is_some() || self.retry_at.is_some_and(|at| now < at) {
            return;
        }
        if self.changed_at.is_some_and(|at| now.saturating_duration_since(at) < SETTINGS_SAVE_DEBOUNCE) {
            return;
        }
        if !settings_save_due(*dirty, loaded, self.rx.is_some()) {
            return;
        }
        let Some(settings) = self.pending.take() else {
            return;
        };
        *dirty = false;
        self.changed_at = None;
        let write = self.write;
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        thread::spawn(move || {
            // A failed send means the engine was dropped meanwhile: nobody waits for it.
            let _ = tx.send(write(&settings));
        });
    }

    /// Applies the outcome of the write that just ended (see [`Self::poll`]).
    fn finish_write(&mut self, now: Instant, outcome: Result<(), String>, dirty: &mut bool, describe: impl FnOnce(&str) -> String) {
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
                    "[cleaning] failed to save {} settings (attempt {}): {err}; retrying in {} s",
                    self.log_label,
                    self.failures,
                    delay.as_secs()
                ));
                self.error = Some(describe(&err));
            }
        }
    }
}

impl<T: Clone + PartialEq + Send + 'static> Drop for SettingsSaver<T> {
    /// The teardown flush: a value still owed to the file is written on a detached worker,
    /// never on the dropping (possibly GUI) thread. A write in flight is waited for ON THAT
    /// WORKER first, so the two renames cannot land out of order.
    fn drop(&mut self) {
        let Some(settings) = self.pending.take() else {
            return;
        };
        let in_flight = self.rx.take();
        let write = self.write;
        let log_label = self.log_label;
        ms_log::runtime_log::log_info(format!("[cleaning] writing the unsaved {log_label} settings on teardown"));
        thread::spawn(move || {
            // Only the ORDER matters here: whatever the earlier write answered (or a writer
            // that died without answering), the flushed value replaces it.
            if let Some(rx) = in_flight
                && rx.recv().is_err()
            {
                ms_log::runtime_log::log_warn(format!("[cleaning] the previous {log_label} settings writer ended without an answer; flushing anyway"));
            }
            if let Err(err) = write(&settings) {
                ms_log::runtime_log::log_error(format!("[cleaning] failed to write the unsaved {log_label} settings on teardown: {err}"));
            }
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

    /// The written document reads back, the directory is created on the way, an existing
    /// file is replaced, and no temp file is left beside it.
    #[test]
    fn a_written_document_reads_back() {
        let dir = std::env::temp_dir().join(format!("ms_engine_settings_file_write_{}", std::process::id()));
        let path = dir.join("nested").join("engine_settings.json");
        write_settings_json(&path, &serde_json::json!({ "steps": 3 })).expect("the write succeeds");
        assert_eq!(read_settings_file(&path, 1, "test", parse_steps).document, Some(3));
        write_settings_json(&path, &serde_json::json!({ "steps": 4 })).expect("the rewrite succeeds");
        assert_eq!(read_settings_file(&path, 1, "test", parse_steps).document, Some(4), "replaced");
        let entries: Vec<_> = std::fs::read_dir(dir.join("nested")).expect("list the dir").map(|entry| entry.expect("an entry").file_name()).collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("engine_settings.json")], "no temp file left behind");
        std::fs::remove_dir_all(&dir).expect("remove the scratch dir");
    }

    /// The temp name can never be read as a settings file or an aside copy.
    #[test]
    fn the_temp_name_is_neither_a_settings_file_nor_an_aside_copy() {
        let path = Path::new("/home/u/data/flux2_klein_settings.json");
        let temp = settings_temp_path(path, 7);
        let name = temp.file_name().and_then(|name| name.to_str()).expect("a UTF-8 name");
        assert_eq!(temp.parent(), path.parent(), "a sibling, so the rename stays on one filesystem");
        assert_eq!(name, format!(".flux2_klein_settings.json.{}.7.tmp", std::process::id()));
        assert!(name.starts_with('.') && !name.ends_with(".json") && !name.contains(".malformed-"));
        assert_ne!(settings_temp_path(path, 8), temp, "every write gets its own temp");
    }

    /// A failed replacement leaves the target exactly as it was and no temp behind: here the
    /// rename fails because the target name is a non-empty directory (portable failure).
    #[test]
    fn a_failed_rename_leaves_the_target_and_no_temp() {
        let dir = std::env::temp_dir().join(format!("ms_engine_settings_file_rename_{}", std::process::id()));
        let path = dir.join("engine_settings.json");
        std::fs::create_dir_all(&path).expect("occupy the target with a directory");
        std::fs::write(path.join("keep.txt"), "kept").expect("make it non-empty");
        let err = write_settings_json(&path, &serde_json::json!({ "steps": 3 })).expect_err("a directory cannot be replaced");
        assert!(matches!(err, SettingsWriteError::Write(_)), "{err:?}");
        assert_eq!(std::fs::read_to_string(path.join("keep.txt")).expect("the target is intact"), "kept");
        let entries: Vec<_> = std::fs::read_dir(&dir).expect("list the dir").map(|entry| entry.expect("an entry").file_name()).collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("engine_settings.json")], "the temp file was removed");
        std::fs::remove_dir_all(&dir).expect("remove the scratch dir");
    }

    /// A temp file that cannot even be created (a read-only directory) fails the write and
    /// leaves the old file intact. Skipped when the process can write there anyway (root).
    #[cfg(unix)]
    #[test]
    fn a_read_only_directory_fails_the_write_and_keeps_the_old_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ms_engine_settings_file_readonly_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        let path = dir.join("engine_settings.json");
        std::fs::write(&path, r#"{ "steps": 1 }"#).expect("the old file");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("make the dir read-only");
        let probe = dir.join("probe");
        let skip = std::fs::write(&probe, "x").is_ok();
        let result = if skip { None } else { Some(write_settings_json(&path, &serde_json::json!({ "steps": 3 }))) };
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("restore the permissions");
        match result {
            None => eprintln!("skipped: the read-only directory is writable for this user"),
            Some(result) => {
                assert!(matches!(result, Err(SettingsWriteError::Write(_))), "{result:?}");
                assert_eq!(read_settings_file(&path, 1, "test", parse_steps).document, Some(1), "the old file is intact");
                assert_eq!(std::fs::read_dir(&dir).expect("list the dir").count(), 1, "no temp file left behind");
            }
        }
        std::fs::remove_dir_all(&dir).expect("remove the scratch dir");
    }

    /// Polls `saver` at `now` until its write in flight answered (bounded).
    fn drain(saver: &mut SettingsSaver<u32>, now: Instant, dirty: &mut bool, settings: u32) {
        for _ in 0..2000 {
            saver.poll(now, SaveGate { dirty: &mut *dirty, loaded: true, blocked: false }, &settings, |err| err.to_string());
            if saver.rx.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("the settings writer never answered");
    }

    /// Waits (bounded) until `log` holds `expected`; a detached flush writes on its own.
    fn await_writes(log: &std::sync::Mutex<Vec<u32>>, expected: &[u32]) {
        for _ in 0..2000 {
            if log.lock().expect("the test log").as_slice() == expected {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(log.lock().expect("the test log").as_slice(), expected);
    }

    /// No write inside the quiet time; a change during it restarts it; a burst costs ONE
    /// write of the value it settled on; the wake-up names the end of the quiet time.
    #[test]
    fn a_burst_of_changes_is_written_once_after_the_quiet_time() {
        static WRITES: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
        fn record(value: &u32) -> Result<(), String> {
            WRITES.lock().map_err(|err| err.to_string())?.push(*value);
            Ok(())
        }
        let mut saver = SettingsSaver::new(record, "test");
        let mut dirty = true;
        let t0 = Instant::now();
        let half = SETTINGS_SAVE_DEBOUNCE / 2;
        fn gate(dirty: &mut bool) -> SaveGate<'_> {
            SaveGate { dirty, loaded: true, blocked: false }
        }
        assert_eq!(saver.wake_after(t0), None, "nothing seen yet");

        saver.poll(t0, gate(&mut dirty), &1, |err| err.to_string());
        assert!(saver.rx.is_none() && dirty, "no write inside the quiet time");
        assert_eq!(saver.wake_after(t0), Some(SETTINGS_SAVE_DEBOUNCE));

        // The dial moves on before the quiet time ended: it restarts.
        saver.poll(t0 + half, gate(&mut dirty), &2, |err| err.to_string());
        saver.poll(t0 + SETTINGS_SAVE_DEBOUNCE, gate(&mut dirty), &2, |err| err.to_string());
        assert!(saver.rx.is_none(), "the change restarted the quiet time");
        assert_eq!(saver.wake_after(t0 + SETTINGS_SAVE_DEBOUNCE), Some(half));

        let settled = t0 + half + SETTINGS_SAVE_DEBOUNCE;
        saver.poll(settled, gate(&mut dirty), &2, |err| err.to_string());
        assert!(saver.rx.is_some() && !dirty, "the settled value is written");
        assert_eq!(saver.wake_after(settled), Some(SAVE_IN_FLIGHT_POLL), "the GUI is woken for the answer");
        drain(&mut saver, settled, &mut dirty, 2);
        assert_eq!(WRITES.lock().expect("the test log").as_slice(), &[2], "one write, of the last value");
        assert_eq!(saver.wake_after(settled), None, "nothing waits any more");
        drop(saver);
        assert_eq!(WRITES.lock().expect("the test log").as_slice(), &[2], "nothing owed: no teardown write");
    }

    /// A failed write still retries after its backoff (not sooner, and the wake-up says so).
    #[test]
    fn a_failed_write_retries_after_the_backoff() {
        fn failing(_: &u32) -> Result<(), String> {
            Err("disk full".to_string())
        }
        let mut saver = SettingsSaver::new(failing, "test");
        let mut dirty = true;
        let t0 = Instant::now();
        let settled = t0 + SETTINGS_SAVE_DEBOUNCE;
        saver.poll(t0, SaveGate { dirty: &mut dirty, loaded: true, blocked: false }, &5, |err| err.to_string());
        saver.poll(settled, SaveGate { dirty: &mut dirty, loaded: true, blocked: false }, &5, |err| err.to_string());
        assert!(saver.rx.is_some());
        drain(&mut saver, settled, &mut dirty, 5);
        assert!(dirty && saver.failures == 1 && saver.error.as_deref() == Some("disk full"));
        assert_eq!(saver.wake_after(settled), Some(save_retry_delay(1)), "woken when the retry may run");
        saver.poll(settled + Duration::from_secs(1), SaveGate { dirty: &mut dirty, loaded: true, blocked: false }, &5, |err| err.to_string());
        assert!(saver.rx.is_none(), "no retry before the backoff");
        let retry = settled + save_retry_delay(1);
        saver.poll(retry, SaveGate { dirty: &mut dirty, loaded: true, blocked: false }, &5, |err| err.to_string());
        assert!(saver.rx.is_some(), "retried after the backoff");
        drain(&mut saver, retry, &mut dirty, 5);
        // Owed again after the second failure: the drop flush calls `failing`, which writes nothing.
    }

    /// Before the load and while blocked nothing is owed: no write, no wake-up, no flush.
    #[test]
    fn nothing_is_owed_before_the_load_or_while_blocked() {
        static WRITES: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
        fn record(value: &u32) -> Result<(), String> {
            WRITES.lock().map_err(|err| err.to_string())?.push(*value);
            Ok(())
        }
        let t0 = Instant::now();
        for (loaded, blocked) in [(false, false), (true, true)] {
            let mut saver = SettingsSaver::new(record, "test");
            let mut dirty = true;
            saver.poll(t0, SaveGate { dirty: &mut dirty, loaded, blocked }, &9, |err| err.to_string());
            saver.poll(t0 + SETTINGS_SAVE_DEBOUNCE, SaveGate { dirty: &mut dirty, loaded, blocked }, &9, |err| err.to_string());
            assert!(saver.rx.is_none() && dirty, "kept in memory, never written");
            assert_eq!(saver.wake_after(t0), None, "no wake-up loop for a write that cannot run");
        }
        // Every saver above was dropped; give a (wrong) detached flush time to show up.
        std::thread::sleep(Duration::from_millis(50));
        assert!(WRITES.lock().expect("the test log").is_empty());
    }

    /// Teardown flush: a value waiting out its quiet time is written after the drop, and one
    /// changed while a write was in flight lands AFTER that write.
    #[test]
    fn a_dropped_saver_flushes_the_value_it_still_owes() {
        static WRITES: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());
        fn slow_record(value: &u32) -> Result<(), String> {
            // Slow enough that the drop below happens while this write is in flight.
            std::thread::sleep(Duration::from_millis(50));
            WRITES.lock().map_err(|err| err.to_string())?.push(*value);
            Ok(())
        }
        let t0 = Instant::now();
        let mut saver = SettingsSaver::new(slow_record, "test");
        let mut dirty = true;
        saver.poll(t0, SaveGate { dirty: &mut dirty, loaded: true, blocked: false }, &1, |err| err.to_string());
        drop(saver);
        await_writes(&WRITES, &[1]);

        let mut saver = SettingsSaver::new(slow_record, "test");
        let mut dirty = true;
        let settled = t0 + SETTINGS_SAVE_DEBOUNCE;
        saver.poll(t0, SaveGate { dirty: &mut dirty, loaded: true, blocked: false }, &2, |err| err.to_string());
        saver.poll(settled, SaveGate { dirty: &mut dirty, loaded: true, blocked: false }, &2, |err| err.to_string());
        assert!(saver.rx.is_some(), "the write of 2 is in flight");
        dirty = true;
        saver.poll(settled, SaveGate { dirty: &mut dirty, loaded: true, blocked: false }, &3, |err| err.to_string());
        drop(saver);
        await_writes(&WRITES, &[1, 2, 3]);
    }
}
