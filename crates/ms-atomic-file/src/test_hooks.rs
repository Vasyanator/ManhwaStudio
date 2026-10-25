/*
File: crates/ms-atomic-file/src/test_hooks.rs

Purpose:
Test observability of the write recipe: the per-path journal of the steps taken (`WriteStep`,
`recorded_steps`) and the per-thread fault injector (`FaultPoint`, `arm_fault`). Both exist only
under `cfg(test)` or the `test-support` feature; in a production build `record_step` and
`injected_fault` compile to nothing.

Notes:
The journal is keyed by path so tests running in parallel over their own temp directories never
see each other's entries; the fault is armed per thread so an injecting test cannot break a
parallel one. Dependent crates' tests (via `ms-docstore`'s forwarded feature) share this state.
*/

use std::path::Path;
#[cfg(any(test, feature = "test-support"))]
use std::path::PathBuf;

/// One observable step of the write recipe. The ORDER of these steps is the contract
/// (`TempClosed` before `TempRemoved`, `Renamed` before `DirectoryDurable`) and a unit test
/// cannot observe it any other way; outside tests recording them is compiled out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStep {
    /// The temp file's handle has been closed.
    TempClosed,
    /// An orphaned temp file was removed after a failure.
    TempRemoved,
    /// The temp file was renamed over the target.
    Renamed,
    /// The containing directory is known-durable (fsynced on Unix; see `sync_directory`).
    DirectoryDurable,
}

/// Where an injected I/O failure strikes. The temp-write failure path — and the cleanup
/// ordering it must honor — is otherwise unreachable on a healthy filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultPoint {
    /// `write_all` into the temp file fails.
    TempWrite,
    /// `sync_all` of the temp file fails.
    TempSync,
}

/// Journal of the write steps taken per path, in order. Test-only.
#[cfg(any(test, feature = "test-support"))]
fn journal() -> &'static std::sync::Mutex<Vec<(PathBuf, WriteStep)>> {
    static JOURNAL: std::sync::OnceLock<std::sync::Mutex<Vec<(PathBuf, WriteStep)>>> = std::sync::OnceLock::new();
    JOURNAL.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Records one write step in the test journal.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn record_step(path: &Path, step: WriteStep) {
    let mut entries = journal().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    entries.push((path.to_path_buf(), step));
}

/// Production build: the journal does not exist, so recording is nothing at all.
#[cfg(not(any(test, feature = "test-support")))]
pub(crate) fn record_step(_path: &Path, _step: WriteStep) {}

/// The steps recorded for `path`, in order. Test-only (`test-support` for dependents).
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn recorded_steps(path: &Path) -> Vec<WriteStep> {
    let entries = journal().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    entries.iter().filter(|(recorded, _)| recorded == path).map(|(_, step)| *step).collect()
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Fault armed for THIS thread only, so an injecting test cannot break a parallel one.
    #[cfg_attr(target_os = "android", expect(clippy::missing_const_for_thread_local, reason = "clippy false positive on targets without native TLS: std's os-TLS thread_local wraps the const initializer in a plain init fn"))]
    static ARMED_FAULT: std::cell::Cell<Option<FaultPoint>> = const { std::cell::Cell::new(None) };
}

/// Arms (or with `None` disarms) an injected write failure for the current thread.
/// Test-only (`test-support` for dependents).
#[cfg(any(test, feature = "test-support"))]
pub fn arm_fault(point: Option<FaultPoint>) {
    ARMED_FAULT.with(|armed| armed.set(point));
}

/// The injected failure for `point`, if one is armed on this thread. Test-only.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn injected_fault(point: FaultPoint) -> Option<std::io::Result<()>> {
    ARMED_FAULT.with(|armed| {
        armed
            .get()
            .filter(|armed| *armed == point)
            .map(|point| Err(std::io::Error::other(format!("injected {point:?} failure"))))
    })
}

/// Production build: nothing can be injected.
#[cfg(not(any(test, feature = "test-support")))]
pub(crate) fn injected_fault(_point: FaultPoint) -> Option<std::io::Result<()>> {
    None
}
