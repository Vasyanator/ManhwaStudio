/*
File: crates/ms-sdcpp/src/client/mod.rs

Purpose:
The app side of the sd.cpp worker: one process-wide `SdCppService` that runs a generation in
a long-lived worker process (the model stays loaded between runs while files and context
options stay the same), probes an installed build in a separate short-lived worker, cancels
cooperatively and then by force, maps a dead worker to `WorkerCrashed`, and unloads.

Key items:
- service()                : the process-wide service (real launcher, installed packages)
- SdCppService::new()      : a service over any launcher / package resolver (tests, smoke)
- generate() / probe() / unload() / unload_build() / is_worker_running()
- ActiveRun                : the running generation, stoppable from another thread
- CANCEL_GRACE, SHUTDOWN_WAIT

Notes:
- Every entry point blocks: worker threads only, never the GUI thread.
- One generation at a time: a second concurrent `generate` gets `GenerationBusy`.
- The model files are validated (`families::validate_model_files`) before any worker sees
  them: a mis-paired encoder would abort the library.
- Unload = cancel, kill after the grace period, reap. `unload_build` (the before-remove hook)
  returns only once no process of that build is left, or fails with `WorkerStillRunning`, so a
  package is never removed under a live worker.
- The global service publishes `worker_running` and probe results into `status`.
*/

pub mod spawn;
mod transport;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use ms_log::runtime_log;
use ms_platform::PlatformFeature;

use crate::catalog::{SdCppBuild, builds_here, row_here};
use crate::error::SdCppError;
use crate::families::validate_model_files;
use crate::protocol::{Event, GenerateMessage, LogLevel, Request, WORKER_PROTOCOL, split_request};
use crate::request::{GenerateJob, ProbeReport, RgbImage, RunEvent, RunStage};

use spawn::{ProcessLauncher, WorkerLauncher, WorkerPipes};
use transport::{LiveWorker, Received};

/// How long a worker may take to stop after `Cancel` before it is killed.
pub const CANCEL_GRACE: Duration = Duration::from_secs(15);
/// How long a worker may take to exit after `Shutdown` before it is killed.
pub const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
/// How long the library load (`Init`) and a probe may take.
const INIT_TIMEOUT: Duration = Duration::from_secs(120);
/// Poll interval of the event loops (cancel flag checks).
const POLL: Duration = Duration::from_millis(100);
/// Slack on top of grace + shutdown when waiting for a stopped generation to end.
const STOP_SLACK: Duration = Duration::from_secs(10);

/// Resolves the installed package directory of a build.
pub type PackageResolver = Box<dyn Fn(SdCppBuild) -> Result<PathBuf, SdCppError> + Send + Sync>;

/// The running generation, as `unload` / the before-remove hook see it.
struct ActiveRun {
    build: SdCppBuild,
    /// Set from outside: stop this run (cancel, kill after grace, never keep the worker).
    stop: AtomicBool,
    /// Set (under the lock) once the run's worker is reaped or parked.
    done: Mutex<bool>,
    finished: Condvar,
}

impl ActiveRun {
    fn new(build: SdCppBuild) -> Self {
        Self { build, stop: AtomicBool::new(false), done: Mutex::new(false), finished: Condvar::new() }
    }

    fn finish(&self) {
        *self.done.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.finished.notify_all();
    }

    /// Waits up to `timeout` for [`Self::finish`]; `true` when the run finished.
    fn wait_done(&self, timeout: Duration) -> bool {
        let done = self.done.lock().unwrap_or_else(PoisonError::into_inner);
        let (done, _) = self.finished.wait_timeout_while(done, timeout, |done| !*done).unwrap_or_else(PoisonError::into_inner);
        *done
    }
}

/// The sd.cpp worker client.
pub struct SdCppService {
    launcher: Box<dyn WorkerLauncher>,
    packages: PackageResolver,
    /// The idle worker (model loaded) kept for the next generation.
    slot: Mutex<Option<LiveWorker>>,
    generation: Mutex<()>,
    active: Mutex<Option<Arc<ActiveRun>>>,
    publish_status: bool,
    cancel_grace: Duration,
}

/// The installed package directory of `build`, or why it cannot run.
///
/// # Errors
/// `Unavailable`, `NoBuildForPlatform`, `NoBuildHere`, `NotInstalled`.
fn installed_package_dir(build: SdCppBuild) -> Result<PathBuf, SdCppError> {
    PlatformFeature::SdCppRuntime.require()?;
    if builds_here().is_empty() {
        return Err(SdCppError::NoBuildForPlatform);
    }
    row_here(build).ok_or(SdCppError::NoBuildHere { build })?;
    if !crate::install::is_installed(build) {
        return Err(SdCppError::NotInstalled { build });
    }
    crate::install::package_dir(build).ok_or(SdCppError::NoBuildHere { build })
}

/// A launcher that always fails (the executable path could not be determined).
struct FailingLauncher(SdCppError);

impl WorkerLauncher for FailingLauncher {
    fn launch(&self) -> Result<WorkerPipes, SdCppError> {
        Err(self.0.clone())
    }
}

/// The process-wide service: workers are this executable with `--sdcpp-worker`, packages
/// come from `install`. Created on first use, which also registers the before-remove hook
/// (before that, no worker of this process can exist).
pub fn service() -> &'static SdCppService {
    static SERVICE: OnceLock<SdCppService> = OnceLock::new();
    SERVICE.get_or_init(|| {
        let launcher: Box<dyn WorkerLauncher> = match ProcessLauncher::current_exe() {
            Ok(launcher) => Box::new(launcher),
            Err(err) => Box::new(FailingLauncher(err)),
        };
        crate::install::set_before_remove_hook(|build| service().unload_build(build));
        let mut service = SdCppService::new(launcher, Box::new(installed_package_dir));
        service.publish_status = true;
        service
    })
}

/// Whether a failed run leaves the worker usable (it answered with a typed error).
const fn worker_survives(err: &SdCppError) -> bool {
    match err {
        SdCppError::GenerationCancelled | SdCppError::Generation { .. } | SdCppError::ContextCreate { .. } | SdCppError::DeviceNotFound { .. } | SdCppError::OutputSize { .. } => true,
        SdCppError::Unavailable(_)
        | SdCppError::NoBuildHere { .. }
        | SdCppError::NotInstalled { .. }
        | SdCppError::Requirement(_)
        | SdCppError::Package { .. }
        | SdCppError::Cancelled
        | SdCppError::Busy
        | SdCppError::GenerationBusy
        | SdCppError::NoBuildForPlatform
        | SdCppError::WorkerStillRunning
        | SdCppError::Files(_)
        | SdCppError::LibraryLoad { .. }
        | SdCppError::AbiMismatch { .. }
        | SdCppError::Protocol { .. }
        | SdCppError::WorkerSpawn { .. }
        | SdCppError::WorkerCrashed { .. } => false,
    }
}

impl SdCppService {
    /// A service over `launcher`, resolving package directories with `packages`. It does not
    /// publish into the process-global `status` (only [`service`] does).
    #[must_use]
    pub fn new(launcher: Box<dyn WorkerLauncher>, packages: PackageResolver) -> Self {
        Self { launcher, packages, slot: Mutex::new(None), generation: Mutex::new(()), active: Mutex::new(None), publish_status: false, cancel_grace: CANCEL_GRACE }
    }

    /// Whether a generation is in flight right now (registered from before file validation
    /// until its worker answers or is reaped); a worker may also be alive and idle without
    /// one, see [`Self::is_worker_running`]. One short lock, cheap enough per frame.
    #[must_use]
    pub fn is_generating(&self) -> bool {
        self.active.lock().unwrap_or_else(PoisonError::into_inner).is_some()
    }

    /// Whether a worker process is alive (idle with a loaded model, or generating).
    #[must_use]
    pub fn is_worker_running(&self) -> bool {
        self.active.lock().unwrap_or_else(PoisonError::into_inner).is_some() || self.slot.lock().unwrap_or_else(PoisonError::into_inner).is_some()
    }

    fn publish_running(&self) {
        if self.publish_status {
            crate::status::publish_worker_running(self.is_worker_running());
        }
    }

    /// Runs `job` and returns the generated image. Blocking; reports stages and steps through
    /// `events` on this thread. Setting `cancel` (or an `unload` from another thread) sends
    /// `Cancel`; a worker that has not stopped after [`CANCEL_GRACE`] is killed. The worker
    /// (and its loaded model) is kept for the next run unless it crashed, was killed, or the
    /// run was stopped by an unload.
    ///
    /// # Errors
    /// `GenerationBusy` (another generation runs), `Unavailable` / `NoBuildForPlatform` /
    /// `NoBuildHere` / `NotInstalled`, `Files` (validation), `WorkerSpawn`, `LibraryLoad`,
    /// `AbiMismatch`, `DeviceNotFound`, `ContextCreate`, `Generation`, `OutputSize`,
    /// `GenerationCancelled`, `WorkerCrashed` (with the worker's stderr tail), `Protocol`.
    ///
    /// # Threading
    /// Blocks for the whole run: worker thread only.
    pub fn generate(&self, job: &GenerateJob, cancel: &AtomicBool, events: &mut dyn FnMut(RunEvent)) -> Result<RgbImage, SdCppError> {
        let _generation = match self.generation.try_lock() {
            Ok(guard) => guard,
            // A poisoned lock means an earlier generation panicked; the slot itself is sound.
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return Err(SdCppError::GenerationBusy),
        };
        // Registered before anything else, so an unload during validation or worker start
        // already sees (and stops) this run.
        let run = Arc::new(ActiveRun::new(job.build));
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) = Some(run.clone());
        let result = self.generate_registered(job, cancel, &run, events);
        // Cleared before `finish`: a waiter that wakes up finds no run of this build.
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) = None;
        run.finish();
        self.publish_running();
        result
    }

    /// The body of [`Self::generate`] for a registered `run`. Every worker it started is
    /// parked in the slot, shut down or killed (and reaped) before it returns.
    fn generate_registered(&self, job: &GenerateJob, cancel: &AtomicBool, run: &ActiveRun, events: &mut dyn FnMut(RunEvent)) -> Result<RgbImage, SdCppError> {
        let stopped = || cancel.load(Ordering::SeqCst) || run.stop.load(Ordering::SeqCst);
        let package_dir = (self.packages)(job.build)?;
        let files = validate_model_files(&job.files)?;
        if stopped() {
            return Err(SdCppError::GenerationCancelled);
        }
        let mut reusable = self.slot.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(stale) = reusable.take_if(|worker| worker.build != job.build || worker.has_exited()) {
            drop(stale);
        }
        let mut worker = if let Some(worker) = reusable {
            worker
        } else {
            events(RunEvent::Stage(RunStage::StartingWorker));
            self.start_worker(job.build, &package_dir, &stopped, SdCppError::GenerationCancelled)?
        };
        self.publish_running();

        let result = self.run_generation(&mut worker, &files, job, &stopped, events);
        let keep = match &result {
            Ok(_) => true,
            Err(err) => worker_survives(err) && !worker.has_exited(),
        };
        if keep && !run.stop.load(Ordering::SeqCst) {
            *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(worker);
        } else if keep {
            worker.shutdown(SHUTDOWN_WAIT);
        } else {
            // Dropping kills and reaps a process that is still running.
            drop(worker);
        }
        result
    }

    /// Sends one generation to `worker` and waits for its outcome.
    fn run_generation(&self, worker: &mut LiveWorker, files: &crate::request::ModelFiles, job: &GenerateJob, stopped: &dyn Fn() -> bool, events: &mut dyn FnMut(RunEvent)) -> Result<RgbImage, SdCppError> {
        let (header, images) = split_request(&job.request);
        let message = GenerateMessage { files: files.clone(), context: job.context.clone(), request: header, images: images.iter().map(|(slot, _)| *slot).collect() };
        send_or_crash(worker, &Request::Generate(Box::new(message)), &[])?;
        for (slot, bytes) in images {
            // A half-sent request cannot be cancelled cooperatively: the worker waits for the
            // remaining frames, so it is killed.
            if stopped() {
                worker.kill();
                return Err(SdCppError::GenerationCancelled);
            }
            send_or_crash(worker, &Request::Image { slot }, bytes)?;
        }
        let mut cancel_deadline: Option<Instant> = None;
        loop {
            if cancel_deadline.is_none() && stopped() {
                runtime_log::log_info("[sdcpp] cancelling the generation");
                send_or_crash(worker, &Request::Cancel, &[])?;
                cancel_deadline = Some(Instant::now() + self.cancel_grace);
            }
            if cancel_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                runtime_log::log_warn("[sdcpp] the worker ignored Cancel; killing it");
                worker.kill();
                return Err(SdCppError::GenerationCancelled);
            }
            match worker.recv(POLL) {
                Received::Timeout => {}
                Received::Closed(detail) => return Err(worker.crash_error(&detail)),
                Received::Event(event, blob) => match event {
                    Event::Stage { stage } => events(RunEvent::Stage(stage)),
                    Event::Step { step, steps } => events(RunEvent::Step { step, steps }),
                    Event::Log { level, text } => log_worker_line(level, &text),
                    Event::Result { width, height } => {
                        if (width, height) != (job.request.width, job.request.height) {
                            return Err(SdCppError::OutputSize { expected: (job.request.width, job.request.height), found: (width, height) });
                        }
                        return RgbImage::new(width, height, blob).map_err(|err| SdCppError::Protocol { detail: err.to_string() });
                    }
                    Event::Error { error } => return Err(error.into()),
                    Event::Cancelled => return Err(SdCppError::GenerationCancelled),
                    other @ (Event::Ready { .. } | Event::ProbeResult { .. } | Event::Unloaded) => return Err(SdCppError::Protocol { detail: format!("unexpected event during a generation: {other:?}") }),
                },
            }
        }
    }

    /// Launches a worker for `build` and initializes it with `package_dir`; `stopped` kills it
    /// and returns `cancelled`.
    fn start_worker(&self, build: SdCppBuild, package_dir: &std::path::Path, stopped: &dyn Fn() -> bool, cancelled: SdCppError) -> Result<LiveWorker, SdCppError> {
        runtime_log::log_info(format!("[sdcpp] starting a worker for build '{}' ({})", build.wire(), package_dir.display()));
        let mut worker = LiveWorker::start(build, self.launcher.launch()?)?;
        send_or_crash(&mut worker, &Request::Init { protocol: WORKER_PROTOCOL, package_dir: package_dir.to_path_buf(), build }, &[])?;
        let event = wait_for_answer(&mut worker, stopped, cancelled, "Init")?;
        match event {
            Event::Ready { protocol, sd_version, sd_commit } if protocol == WORKER_PROTOCOL => {
                runtime_log::log_info(format!("[sdcpp] worker ready: sd.cpp {sd_version} ({sd_commit})"));
                Ok(worker)
            }
            Event::Ready { protocol, .. } => Err(SdCppError::Protocol { detail: format!("worker protocol {protocol}, expected {WORKER_PROTOCOL}") }),
            Event::Error { error } => Err(error.into()),
            other => Err(SdCppError::Protocol { detail: format!("unexpected answer to Init: {other:?}") }),
        }
    }

    /// Probes `build` in a separate short-lived worker (a running generation is not
    /// disturbed): versions, devices, cores. The global service publishes the result into
    /// `status`.
    ///
    /// # Errors
    /// As [`Self::generate`] for package, spawn, load and crash failures; `Cancelled`.
    ///
    /// # Threading
    /// Blocks: worker thread only.
    pub fn probe(&self, build: SdCppBuild, cancel: &AtomicBool) -> Result<ProbeReport, SdCppError> {
        let result = self.probe_inner(build, cancel);
        if let Err(err) = &result {
            runtime_log::log_warn(format!("[sdcpp] probe of build '{}' failed: {err:?}", build.wire()));
        }
        if self.publish_status {
            crate::status::publish_probe(build, result.clone());
        }
        result
    }

    fn probe_inner(&self, build: SdCppBuild, cancel: &AtomicBool) -> Result<ProbeReport, SdCppError> {
        let package_dir = (self.packages)(build)?;
        let stopped = || cancel.load(Ordering::SeqCst);
        let mut worker = self.start_worker(build, &package_dir, &stopped, SdCppError::Cancelled)?;
        send_or_crash(&mut worker, &Request::Probe, &[])?;
        let answer = wait_for_answer(&mut worker, &stopped, SdCppError::Cancelled, "Probe");
        worker.shutdown(SHUTDOWN_WAIT);
        match answer? {
            Event::ProbeResult { report } => Ok(report),
            Event::Error { error } => Err(error.into()),
            other => Err(SdCppError::Protocol { detail: format!("unexpected answer to Probe: {other:?}") }),
        }
    }

    /// Stops every worker of this service: a running generation is cancelled (killed after
    /// the grace period) and its worker reaped, the idle worker is shut down (killed after
    /// [`SHUTDOWN_WAIT`]). A failure to stop is logged.
    ///
    /// # Threading
    /// May block for the grace period plus the shutdown wait: worker thread only.
    pub fn unload(&self) {
        for build in SdCppBuild::ALL {
            if let Err(err) = self.unload_build(build) {
                runtime_log::log_error(format!("[sdcpp] unload of build '{}' failed: {err:?}", build.wire()));
            }
        }
    }

    /// Stops every worker of `build` and returns only once none is left: a running generation
    /// of that build is cancelled, killed after the grace period and reaped; then the idle
    /// worker of that build is shut down. Workers of other builds are untouched. This is the
    /// before-remove hook of `install::remove`.
    ///
    /// # Errors
    /// `WorkerStillRunning` when the running generation did not end in time (e.g. a request
    /// write blocked on a stalled pipe); the caller must not touch the package then.
    ///
    /// # Threading
    /// Blocks: worker thread only.
    pub fn unload_build(&self, build: SdCppBuild) -> Result<(), SdCppError> {
        let run = self.active.lock().unwrap_or_else(PoisonError::into_inner).clone().filter(|run| run.build == build);
        if let Some(run) = run {
            runtime_log::log_info(format!("[sdcpp] stopping the running generation of build '{}'", build.wire()));
            run.stop.store(true, Ordering::SeqCst);
            if !run.wait_done(self.cancel_grace + SHUTDOWN_WAIT + STOP_SLACK) {
                runtime_log::log_error(format!("[sdcpp] the generation of build '{}' did not stop in time", build.wire()));
                return Err(SdCppError::WorkerStillRunning);
            }
        }
        // After the run ended: it may have parked its worker just before the stop request.
        let idle = self.slot.lock().unwrap_or_else(PoisonError::into_inner).take_if(|worker| worker.build == build);
        if let Some(worker) = idle {
            runtime_log::log_info(format!("[sdcpp] unloading: stopping the worker of build '{}'", build.wire()));
            worker.shutdown(SHUTDOWN_WAIT);
        }
        self.publish_running();
        Ok(())
    }
}

/// Sends a request; a write failure means the worker's stdin closed, i.e. the process is
/// gone, so it is reported as the crash it is.
fn send_or_crash(worker: &mut LiveWorker, request: &Request, blob: &[u8]) -> Result<(), SdCppError> {
    match worker.send(request, blob) {
        Ok(()) => Ok(()),
        Err(SdCppError::Protocol { detail }) => Err(worker.crash_error(&detail)),
        Err(other) => Err(other),
    }
}

/// Waits for the answer to a request (skipping log lines). `stopped` kills the worker and
/// returns `cancelled`; [`INIT_TIMEOUT`] kills it and returns `Protocol`.
fn wait_for_answer(worker: &mut LiveWorker, stopped: &dyn Fn() -> bool, cancelled: SdCppError, what: &str) -> Result<Event, SdCppError> {
    let deadline = Instant::now() + INIT_TIMEOUT;
    loop {
        if stopped() {
            worker.kill();
            return Err(cancelled);
        }
        if Instant::now() >= deadline {
            worker.kill();
            return Err(SdCppError::Protocol { detail: format!("the worker did not answer {what} within {} s", INIT_TIMEOUT.as_secs()) });
        }
        match worker.recv(POLL) {
            Received::Timeout => {}
            Received::Closed(detail) => return Err(worker.crash_error(&detail)),
            Received::Event(Event::Log { level, text }, _) => log_worker_line(level, &text),
            Received::Event(event, _) => return Ok(event),
        }
    }
}

/// Logs a library log line forwarded by the worker.
fn log_worker_line(level: LogLevel, text: &str) {
    match level {
        LogLevel::Info => runtime_log::log_info(format!("[sdcpp] {text}")),
        LogLevel::Warn => runtime_log::log_warn(format!("[sdcpp] {text}")),
        LogLevel::Error => runtime_log::log_error(format!("[sdcpp] {text}")),
    }
}

#[cfg(test)]
mod tests;
