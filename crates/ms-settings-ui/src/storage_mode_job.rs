/*
File: storage_mode_job.rs

Purpose:
The ONE process-wide storage-mode conversion job. Both the startup reconciliation (the
user_config sentinel says a previous switch did not finish) and the General pane's Dev/Prod
switch run `ms_project::storage_mode::convert_globals` through here, on a named worker, so
that two drivers can never run at once in opposite directions, and so that every surface
(launcher main page, launcher settings, studio settings) observes the same progress. The
launcher's per-chapter conversion registers here too (`ChapterConversionLease`): a global job
and a chapter conversion exclude each other, because the global job flips the docstore
default format that is the chapter conversion's target.

Key structures:
- ConversionRequest  : target mode + the three roots the driver needs
- JobOrigin          : startup reconciliation vs. an explicit user switch
- ConversionJobState : Idle / Pending / Running {done,total} / Finished {outcome}
- StartConversionError
- ChapterConversionLease : RAII registration of a running chapter conversion

Key functions:
- set_pending_reconciliation() / start_pending_reconciliation()
- start_conversion()
- begin_chapter_conversion() / chapter_conversion_running()
- conversion_job_state() / last_finished_job()
- backend_autostart_blocked() : the AI backend autostart gate (anything in the slot)
- failed_document_path()  : a failed stem -> the real file path (worker-side I/O)

Notes:
The slot is process-global mutable state on purpose: the documents it converts are
process-global, and it is the exclusion mechanism between the two starters. The mutex is
held only to read/replace the small state value, never across I/O. GUI code polls
`conversion_job_state()` per frame (cheap) and requests a repaint while a job runs.
*/

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ms_config::StorageMode;
use ms_docstore::DocFormat;
use ms_log::runtime_log;
use ms_project::storage_mode::GlobalConvertOutcome;
use ms_thread as thread;

/// Everything one driver run needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionRequest {
    /// The mode every global/title document is converted to.
    pub target: StorageMode,
    /// The projects root whose titles are converted.
    pub projects_root: PathBuf,
    /// The application fonts directory (`fonts_data`, `presets`).
    pub fonts_dir: PathBuf,
    /// The user-config document (converted last; the startup sentinel).
    pub user_config_path: PathBuf,
}

impl ConversionRequest {
    /// A request for `target` over the process's real locations: the projects root recorded
    /// in `user_settings`, the application fonts dir and the data-root user config.
    #[must_use]
    pub fn for_current_install(target: StorageMode, user_settings: &serde_json::Value) -> Self {
        Self {
            target,
            projects_root: ms_config::projects_root_from_user_settings(user_settings),
            fonts_dir: ms_config::storage_mode::app_fonts_dir(),
            user_config_path: ms_config::user_config_path(),
        }
    }
}

/// Who started a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOrigin {
    /// Startup found the user-config sentinel in the other format.
    StartupReconciliation,
    /// The user chose a mode in the General pane.
    UserSwitch,
}

/// Observable state of the process-wide job. `id`s increase monotonically per start.
#[derive(Debug, Clone, Default)]
pub enum ConversionJobState {
    /// Nothing ran yet in this process.
    #[default]
    Idle,
    /// A startup reconciliation is due and waits for [`start_pending_reconciliation`].
    Pending {
        /// The run to start.
        request: ConversionRequest,
    },
    /// A driver is running on its worker.
    Running {
        /// Job id.
        id: u64,
        /// Who started it.
        origin: JobOrigin,
        /// Target mode.
        target: StorageMode,
        /// Documents finished so far.
        done: usize,
        /// Documents in total (0 until the driver has enumerated them).
        total: usize,
    },
    /// The last job ended (successfully or with per-document failures).
    Finished {
        /// Job id.
        id: u64,
        /// Who started it.
        origin: JobOrigin,
        /// Target mode.
        target: StorageMode,
        /// The driver's report.
        outcome: Arc<GlobalConvertOutcome>,
    },
}

impl ConversionJobState {
    /// Whether a driver is running right now.
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

/// Why a job did not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartConversionError {
    /// Another conversion is still running; wait for it.
    AlreadyRunning,
    /// The worker thread could not be spawned (OS error text).
    Spawn(String),
    /// A launcher chapter conversion is running; a global switch would change its target
    /// format under it. Wait for it.
    ChapterConversionRunning,
}

/// The driver a worker runs: the real `convert_globals` in production, a fake in tests.
type Driver = Box<dyn FnOnce(&ConversionRequest, &dyn Fn(usize, usize)) -> GlobalConvertOutcome + Send>;

struct Slot {
    state: ConversionJobState,
    next_id: u64,
    /// Live [`ChapterConversionLease`]s (chapter conversions in flight).
    chapter_conversions: usize,
}

static JOB: Mutex<Slot> = Mutex::new(Slot { state: ConversionJobState::Idle, next_id: 1, chapter_conversions: 0 });

/// Locks the slot; a poisoned lock (a panic while holding it — only ever a small state
/// assignment) is recovered, the state is still a valid value.
fn slot() -> MutexGuard<'static, Slot> {
    JOB.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Snapshot of the job state.
#[must_use]
pub fn conversion_job_state() -> ConversionJobState {
    slot().state.clone()
}

/// A finished job's identity and report, as polled per frame by surfaces that only care
/// about outcomes (no path clones of a pending request).
#[derive(Debug, Clone)]
pub struct FinishedJob {
    /// Job id.
    pub id: u64,
    /// Who started it.
    pub origin: JobOrigin,
    /// The driver's report.
    pub outcome: Arc<GlobalConvertOutcome>,
}

/// The last finished job, if the slot currently holds one (cheap: an `Arc` clone).
#[must_use]
pub fn last_finished_job() -> Option<FinishedJob> {
    match &slot().state {
        ConversionJobState::Finished { id, origin, outcome, .. } => Some(FinishedJob { id: *id, origin: *origin, outcome: Arc::clone(outcome) }),
        ConversionJobState::Idle | ConversionJobState::Pending { .. } | ConversionJobState::Running { .. } => None,
    }
}

/// Registration of one running launcher chapter conversion. While any lease lives, a global
/// switch (and the startup reconciliation) cannot start; dropping it releases the
/// registration. Move it into the conversion worker and drop it before reporting the result.
#[derive(Debug)]
#[must_use = "dropping the lease immediately releases the registration"]
pub struct ChapterConversionLease {
    _private: (),
}

impl Drop for ChapterConversionLease {
    fn drop(&mut self) {
        let mut slot = slot();
        slot.chapter_conversions = slot.chapter_conversions.saturating_sub(1);
    }
}

/// Registers a chapter conversion that is about to start.
///
/// # Errors
/// [`StartConversionError::AlreadyRunning`] while the global conversion job runs (it would
/// flip the chapter's target format mid-way).
pub fn begin_chapter_conversion() -> Result<ChapterConversionLease, StartConversionError> {
    let mut slot = slot();
    if slot.state.is_running() {
        return Err(StartConversionError::AlreadyRunning);
    }
    slot.chapter_conversions += 1;
    Ok(ChapterConversionLease { _private: () })
}

/// Whether a launcher chapter conversion is registered right now (the General pane disables
/// the switch meanwhile).
#[must_use]
pub fn chapter_conversion_running() -> bool {
    slot().chapter_conversions > 0
}

/// Whether the Python AI backend must not autostart right now: a startup reconciliation is
/// pending or any global conversion runs (the backend reads and writes `user_config` itself,
/// and a write landing mid-conversion is lost — KG-016), or a launcher chapter conversion is
/// registered. Cheap (no clone of the pending request): safe to poll on a worker tick.
#[must_use]
pub fn backend_autostart_blocked() -> bool {
    let slot = slot();
    let job_active = match &slot.state {
        ConversionJobState::Pending { .. } | ConversionJobState::Running { .. } => true,
        ConversionJobState::Idle | ConversionJobState::Finished { .. } => false,
    };
    job_active || slot.chapter_conversions > 0
}

/// Records a startup reconciliation to start later (see [`start_pending_reconciliation`]).
/// Ignored when a job already ran or runs in this process.
pub fn set_pending_reconciliation(request: ConversionRequest) {
    let mut slot = slot();
    if matches!(slot.state, ConversionJobState::Idle) {
        slot.state = ConversionJobState::Pending { request };
    }
}

/// Starts the pending startup reconciliation, if any. Returns the job id when one started.
/// The Pending -> Running transition happens in ONE critical section, so a user switch that
/// superseded the pending run (even one that already finished) always wins. While a chapter
/// conversion is registered the run stays pending (a later call starts it); a spawn failure
/// is logged and the sentinel makes the next process start retry.
pub fn start_pending_reconciliation() -> Option<u64> {
    start_pending_with_driver(production_driver())
}

/// [`start_pending_reconciliation`] with an injectable driver (tests).
fn start_pending_with_driver(driver: Driver) -> Option<u64> {
    let origin = JobOrigin::StartupReconciliation;
    let (id, request) = {
        let mut slot = slot();
        let request = match &slot.state {
            ConversionJobState::Pending { request } => request.clone(),
            ConversionJobState::Idle | ConversionJobState::Running { .. } | ConversionJobState::Finished { .. } => return None,
        };
        if slot.chapter_conversions > 0 {
            runtime_log::log_info("[storage-mode] startup reconciliation deferred: a chapter conversion is running");
            return None;
        }
        (claim_running(&mut slot, origin, request.target), request)
    };
    match spawn_job(id, origin, request, driver) {
        Ok(()) => Some(id),
        Err(err) => {
            runtime_log::log_error(format!("[storage-mode] could not start the startup reconciliation; it re-runs at the next start; error={err:?}"));
            None
        }
    }
}

/// Starts a conversion of every global/title document to `request.target` on a named
/// worker and returns its id. A pending (not yet started) reconciliation is superseded.
///
/// # Errors
/// [`StartConversionError::AlreadyRunning`] while another job runs;
/// [`StartConversionError::ChapterConversionRunning`] while a launcher chapter conversion
/// runs; [`StartConversionError::Spawn`] when the worker thread cannot be created.
pub fn start_conversion(origin: JobOrigin, request: ConversionRequest) -> Result<u64, StartConversionError> {
    start_with_driver(origin, request, production_driver())
}

/// The real driver: `convert_globals`, then the failed stems mapped to the files the user
/// has to repair (still on the worker: the mapping stats the disk).
fn production_driver() -> Driver {
    Box::new(|request, progress| {
        let mut outcome = ms_project::storage_mode::convert_globals(request.target, &request.projects_root, &request.fonts_dir, &request.user_config_path, progress);
        let target = request.target.doc_format();
        for (path, _reason) in &mut outcome.failed {
            *path = failed_document_path(path, target);
        }
        outcome
    })
}

/// [`start_conversion`] with an injectable driver (tests).
fn start_with_driver(origin: JobOrigin, request: ConversionRequest, driver: Driver) -> Result<u64, StartConversionError> {
    let id = {
        let mut slot = slot();
        if slot.state.is_running() {
            return Err(StartConversionError::AlreadyRunning);
        }
        if slot.chapter_conversions > 0 {
            return Err(StartConversionError::ChapterConversionRunning);
        }
        claim_running(&mut slot, origin, request.target)
    };
    spawn_job(id, origin, request, driver).map(|()| id)
}

/// Allocates the next job id and marks the slot `Running`. Caller holds the slot lock and
/// has checked that the transition is allowed.
fn claim_running(slot: &mut Slot, origin: JobOrigin, target: StorageMode) -> u64 {
    let id = slot.next_id;
    slot.next_id += 1;
    slot.state = ConversionJobState::Running { id, origin, target, done: 0, total: 0 };
    id
}

/// Runs `driver` for the already-claimed job `id` on a named worker. On a spawn failure
/// the claim is rolled back to `Idle` so the UI can offer the switch again.
fn spawn_job(id: u64, origin: JobOrigin, request: ConversionRequest, driver: Driver) -> Result<(), StartConversionError> {
    let target = request.target;
    let thread_name = match origin {
        JobOrigin::StartupReconciliation => "docstore-reconcile",
        JobOrigin::UserSwitch => "docstore-convert",
    };
    let spawned = thread::Builder::new().name(thread_name.to_string()).spawn(move || {
        let progress = |done: usize, total: usize| {
            let mut slot = slot();
            if let ConversionJobState::Running { id: running, done: slot_done, total: slot_total, .. } = &mut slot.state
                && *running == id
            {
                *slot_done = done;
                *slot_total = total;
            }
        };
        let outcome = driver(&request, &progress);
        slot().state = ConversionJobState::Finished { id, origin, target, outcome: Arc::new(outcome) };
    });
    match spawned {
        Ok(handle) => {
            // Detached on purpose: completion is observed through the slot, not a join.
            drop(handle);
            runtime_log::log_info(format!("[storage-mode] job {id} ({origin:?}) started: target={}", target.as_config_str()));
            Ok(())
        }
        Err(err) => {
            // Roll back so the UI can offer the switch again.
            let mut slot = slot();
            if matches!(slot.state, ConversionJobState::Running { id: running, .. } if running == id) {
                slot.state = ConversionJobState::Idle;
            }
            runtime_log::log_error(format!("[storage-mode] failed to spawn the {thread_name} worker; error={err}"));
            Err(StartConversionError::Spawn(err.to_string()))
        }
    }
}

/// Maps a document reported as failed to the file the user should look at: a stem (the
/// docstore reports `<dir>/terms`) becomes the existing `<stem>.<ext>`, preferring the
/// source format (a failed document stays there) over `target`; a path that already names
/// an existing file or directory, or that matches nothing on disk, is returned unchanged.
/// Stats the disk: call it on a worker, never per frame.
#[must_use]
pub fn failed_document_path(path: &Path, target: DocFormat) -> PathBuf {
    if path.exists() {
        return path.to_path_buf();
    }
    let source = match target {
        DocFormat::Json => DocFormat::Db,
        DocFormat::Db => DocFormat::Json,
    };
    [source, target]
        .into_iter()
        .map(|format| {
            let mut name = path.as_os_str().to_os_string();
            name.push(".");
            name.push(format.extension());
            PathBuf::from(name)
        })
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// The slot is process-global: tests touching it run one at a time.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn request(target: StorageMode) -> ConversionRequest {
        ConversionRequest { target, projects_root: PathBuf::from("p"), fonts_dir: PathBuf::from("f"), user_config_path: PathBuf::from("u") }
    }

    fn reset() {
        let mut slot = slot();
        slot.state = ConversionJobState::Idle;
        slot.chapter_conversions = 0;
    }

    fn instant() -> Driver {
        Box::new(|_, _| GlobalConvertOutcome::default())
    }

    fn wait_finished(id: u64) -> Result<Arc<GlobalConvertOutcome>, String> {
        for _ in 0..500 {
            if let ConversionJobState::Finished { id: done, outcome, .. } = conversion_job_state()
                && done == id
            {
                return Ok(outcome);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err("job did not finish".into())
    }

    #[test]
    fn a_second_job_is_refused_while_one_runs_and_progress_is_visible() -> Result<(), String> {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        reset();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (reported_tx, reported_rx) = mpsc::channel::<()>();
        let id = start_with_driver(
            JobOrigin::UserSwitch,
            request(StorageMode::Dev),
            Box::new(move |_, progress| {
                progress(1, 3);
                // A closed channel only means the test body already failed and returned.
                reported_tx.send(()).ok();
                release_rx.recv().ok();
                GlobalConvertOutcome { converted: 3, mode_persisted: true, sentinel_settled: true, ..GlobalConvertOutcome::default() }
            }),
        )
        .map_err(|err| format!("{err:?}"))?;
        reported_rx.recv().map_err(|err| err.to_string())?;
        match conversion_job_state() {
            ConversionJobState::Running { id: running, done, total, target, .. } => assert_eq!((running, done, total, target), (id, 1, 3, StorageMode::Dev)),
            other => return Err(format!("expected Running, got {other:?}")),
        }
        assert_eq!(start_with_driver(JobOrigin::UserSwitch, request(StorageMode::Prod), Box::new(|_, _| GlobalConvertOutcome::default())), Err(StartConversionError::AlreadyRunning));
        release_tx.send(()).map_err(|err| err.to_string())?;
        let outcome = wait_finished(id)?;
        assert!(outcome.is_complete());
        reset();
        Ok(())
    }

    #[test]
    fn pending_reconciliation_starts_once_and_a_user_switch_supersedes_it() -> Result<(), String> {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        reset();
        set_pending_reconciliation(request(StorageMode::Prod));
        assert!(matches!(conversion_job_state(), ConversionJobState::Pending { .. }));
        // A user switch replaces the pending run.
        let id = start_with_driver(JobOrigin::UserSwitch, request(StorageMode::Dev), Box::new(|_, _| GlobalConvertOutcome::default())).map_err(|err| format!("{err:?}"))?;
        let _outcome = wait_finished(id)?;
        assert_eq!(start_pending_reconciliation(), None, "nothing is pending any more");
        // A pending run is only recorded on a fresh slot.
        set_pending_reconciliation(request(StorageMode::Prod));
        assert!(matches!(conversion_job_state(), ConversionJobState::Finished { .. }));
        reset();
        Ok(())
    }

    #[test]
    fn a_pending_reconciliation_never_undoes_a_user_switch_that_superseded_it() -> Result<(), String> {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        // Race the two starters repeatedly. Whatever the interleaving, a user switch that
        // was accepted must be the LAST job to run: either the reconciliation claimed the
        // slot first (and the switch was refused or ran after it), or the switch superseded
        // the pending run and the reconciliation does not start at all.
        for _ in 0..200 {
            reset();
            set_pending_reconciliation(request(StorageMode::Prod));
            let user = std::thread::spawn(|| start_with_driver(JobOrigin::UserSwitch, request(StorageMode::Dev), instant()));
            let reconcile = start_pending_with_driver(instant());
            let user = user.join().map_err(|_| "user-switch thread panicked".to_string())?;
            match (user, reconcile) {
                (Ok(user_id), Some(reconcile_id)) => assert!(reconcile_id < user_id, "the reconciliation ran after the user switch"),
                (Ok(_), None) | (Err(StartConversionError::AlreadyRunning), Some(_)) => {}
                other => return Err(format!("unexpected interleaving outcome {other:?}")),
            }
            // Let the last job settle before the next round.
            for _ in 0..500 {
                if !conversion_job_state().is_running() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        // Deterministic core: after a finished user switch nothing is pending any more.
        reset();
        set_pending_reconciliation(request(StorageMode::Prod));
        let id = start_with_driver(JobOrigin::UserSwitch, request(StorageMode::Dev), instant()).map_err(|err| format!("{err:?}"))?;
        let _outcome = wait_finished(id)?;
        assert_eq!(start_pending_with_driver(instant()), None);
        assert!(matches!(conversion_job_state(), ConversionJobState::Finished { id: done, origin: JobOrigin::UserSwitch, .. } if done == id));
        reset();
        Ok(())
    }

    #[test]
    fn a_chapter_conversion_and_a_global_job_exclude_each_other() -> Result<(), String> {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        reset();
        // A running chapter conversion blocks a user switch and defers the reconciliation.
        let lease = begin_chapter_conversion().map_err(|err| format!("{err:?}"))?;
        assert!(chapter_conversion_running());
        assert_eq!(start_with_driver(JobOrigin::UserSwitch, request(StorageMode::Dev), instant()), Err(StartConversionError::ChapterConversionRunning));
        set_pending_reconciliation(request(StorageMode::Prod));
        assert_eq!(start_pending_with_driver(instant()), None);
        assert!(matches!(conversion_job_state(), ConversionJobState::Pending { .. }), "the reconciliation stays pending");
        drop(lease);
        assert!(!chapter_conversion_running());
        let reconcile = start_pending_with_driver(instant()).ok_or("the released reconciliation must start")?;
        let _outcome = wait_finished(reconcile)?;

        // A running global job refuses a chapter conversion.
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let id = start_with_driver(
            JobOrigin::UserSwitch,
            request(StorageMode::Dev),
            Box::new(move |_, _| {
                release_rx.recv().ok();
                GlobalConvertOutcome::default()
            }),
        )
        .map_err(|err| format!("{err:?}"))?;
        assert_eq!(begin_chapter_conversion().map(drop), Err(StartConversionError::AlreadyRunning));
        assert!(!chapter_conversion_running());
        release_tx.send(()).map_err(|err| err.to_string())?;
        let _outcome = wait_finished(id)?;
        // Two chapter conversions may overlap (different chapters); both must end.
        let first = begin_chapter_conversion().map_err(|err| format!("{err:?}"))?;
        let second = begin_chapter_conversion().map_err(|err| format!("{err:?}"))?;
        drop(first);
        assert!(chapter_conversion_running());
        drop(second);
        assert!(!chapter_conversion_running());
        reset();
        Ok(())
    }

    #[test]
    fn backend_autostart_is_blocked_while_anything_is_in_the_slot() -> Result<(), String> {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        reset();
        assert!(!backend_autostart_blocked(), "an idle slot lets the backend start");
        // A pending startup reconciliation blocks (the backend would race its user_config).
        set_pending_reconciliation(request(StorageMode::Prod));
        assert!(backend_autostart_blocked());
        // A running job blocks; its end (Finished, even with failures) releases.
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let id = start_pending_with_driver(Box::new(move |_, _| {
            release_rx.recv().ok();
            GlobalConvertOutcome::default()
        }))
        .ok_or("the reconciliation must start")?;
        assert!(backend_autostart_blocked());
        release_tx.send(()).map_err(|err| err.to_string())?;
        let _outcome = wait_finished(id)?;
        assert!(!backend_autostart_blocked(), "a finished job releases the gate");
        // A registered chapter conversion blocks until its lease drops.
        let lease = begin_chapter_conversion().map_err(|err| format!("{err:?}"))?;
        assert!(backend_autostart_blocked());
        drop(lease);
        assert!(!backend_autostart_blocked());
        reset();
        Ok(())
    }

    #[test]
    fn a_failed_stem_maps_to_the_file_that_exists() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let stem = dir.path().join("terms");
        // Nothing on disk: the stem is kept.
        assert_eq!(failed_document_path(&stem, DocFormat::Db), stem);
        // Only the target exists.
        std::fs::write(dir.path().join("terms.db"), b"")?;
        assert_eq!(failed_document_path(&stem, DocFormat::Db), dir.path().join("terms.db"));
        // Both exist: the source format (where a failed document stays) wins.
        std::fs::write(dir.path().join("terms.json"), b"{}")?;
        assert_eq!(failed_document_path(&stem, DocFormat::Db), dir.path().join("terms.json"));
        assert_eq!(failed_document_path(&stem, DocFormat::Json), dir.path().join("terms.db"));
        // A path naming an existing file or directory is already concrete.
        let full = dir.path().join("terms.json");
        assert_eq!(failed_document_path(&full, DocFormat::Db), full);
        assert_eq!(failed_document_path(dir.path(), DocFormat::Db), dir.path().to_path_buf());
        Ok(())
    }
}
