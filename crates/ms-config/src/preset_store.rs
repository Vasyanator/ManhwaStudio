/*
File: crates/ms-config/src/preset_store.rs

Purpose:
The GUI-free machinery every named-preset store of the app shares: the ordered off-thread
writer that persists preset operations, and the allocator of a free default preset name.

Key structures:
- `OrderedWriter<Job, Outcome>`: one lazily started writer thread that runs every submitted job
  IN ARRIVAL ORDER and answers each with an outcome the owner drains on the GUI thread.
- `SubmitError<Job>`: why a job could not be handed to the thread (spawn failure, dead thread);
  it carries the job back so the owner can report what was lost.

Key functions:
- `OrderedWriter::new` / `submit` / `try_recv_outcome` / `recv_outcome_timeout`
- `free_default_name()`: the lowest free 1-based numbered default name.

Notes:
- Consumers: the typing tab's formula presets, the translation tab's composition presets and
  Settings › «Лента и пузыри» layout presets. Each keeps its own job type, its own write step
  (the runner) and its own bookkeeping of failed entries; only the thread, the ordering, the
  outcome channel and the restart rule live here.
- ORDER: one thread per writer, one FIFO queue, so a quick save-then-delete never lands
  reversed. Each job is meant to be its own read-modify-write (so another app instance's entries
  survive).
- LIFETIME: the thread starts on the first `submit`; a thread that died is replaced by the next
  `submit` after the one that found it gone. Dropping the writer closes the queue and JOINS the
  thread, so every queued job reaches the disk — drop it on a worker (the studio's teardown),
  never on the GUI thread while jobs may be queued.
- The outcome channel belongs to the writer, not to the thread, so outcomes of a replaced thread
  are still delivered.
*/

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use ms_log::runtime_log;
use ms_thread::JoinHandle;

/// Why [`OrderedWriter::submit`] could not hand a job to the writer thread. The job comes back
/// unrun, so the owner can name what was not saved.
#[derive(Debug)]
pub enum SubmitError<Job> {
    /// The writer thread could not be started (an OS error). The next `submit` tries again.
    Spawn { job: Job, error: std::io::Error },
    /// The writer thread is gone (it panicked). The next `submit` starts a fresh one.
    WorkerGone { job: Job },
}

/// The running thread of an [`OrderedWriter`].
struct Worker<Job> {
    /// `None` only while dropping (closing the queue ends the thread's loop).
    tx: Option<Sender<Job>>,
    handle: Option<JoinHandle<()>>,
}

/// One ordered writer thread with an outcome channel. See the file header for the ordering and
/// lifetime contract.
pub struct OrderedWriter<Job, Outcome> {
    /// OS thread name, also the log label of this writer.
    thread_name: &'static str,
    /// Runs one job on the writer thread (blocking I/O allowed) and produces its answer.
    run: fn(Job) -> Outcome,
    /// Started on the first submit; replaced after it died.
    worker: Option<Worker<Job>>,
    outcomes_tx: Sender<Outcome>,
    outcomes_rx: Receiver<Outcome>,
}

impl<Job, Outcome> std::fmt::Debug for OrderedWriter<Job, Outcome> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrderedWriter").field("thread_name", &self.thread_name).field("started", &self.worker.is_some()).finish_non_exhaustive()
    }
}

impl<Job: Send + 'static, Outcome: Send + 'static> OrderedWriter<Job, Outcome> {
    /// A writer whose thread (named `thread_name`) will run `run` once per submitted job. No
    /// thread is started until the first [`Self::submit`].
    #[must_use]
    pub fn new(thread_name: &'static str, run: fn(Job) -> Outcome) -> Self {
        let (outcomes_tx, outcomes_rx) = mpsc::channel();
        Self { thread_name, run, worker: None, outcomes_tx, outcomes_rx }
    }

    /// Queues `job` behind every job submitted before it, starting the thread if needed.
    ///
    /// # Errors
    /// [`SubmitError::Spawn`] when the thread cannot be started, [`SubmitError::WorkerGone`] when
    /// it died; both return the job unrun and are logged here. The next call retries with a fresh
    /// thread.
    pub fn submit(&mut self, job: Job) -> Result<(), SubmitError<Job>> {
        if self.worker.is_none() {
            match self.spawn() {
                Ok(worker) => self.worker = Some(worker),
                Err(error) => {
                    runtime_log::log_error(format!("[preset_store] could not start the writer thread {}; error={error}", self.thread_name));
                    return Err(SubmitError::Spawn { job, error });
                }
            }
        }
        let Some(tx) = self.worker.as_ref().and_then(|worker| worker.tx.as_ref()) else {
            // Unreachable: the worker was installed above and only `Drop` takes its sender.
            return Err(SubmitError::WorkerGone { job });
        };
        match tx.send(job) {
            Ok(()) => Ok(()),
            Err(mpsc::SendError(job)) => {
                runtime_log::log_error(format!("[preset_store] the writer thread {} is gone; a queued operation was not saved", self.thread_name));
                // A fresh thread is started by the next submit; the dead one is reaped here.
                if let Some(dead) = self.worker.take() {
                    join_worker(self.thread_name, dead);
                }
                Err(SubmitError::WorkerGone { job })
            }
        }
    }

    /// The next outcome the thread produced, or `None` when there is none yet. Never blocks.
    #[must_use]
    pub fn try_recv_outcome(&self) -> Option<Outcome> {
        // The writer holds a sender itself, so the channel cannot disconnect while `self` lives;
        // an `Err` means "nothing more to read".
        self.outcomes_rx.try_recv().ok()
    }

    /// Waits up to `timeout` for the next outcome (for tests and worker-side callers; never the
    /// GUI thread).
    ///
    /// # Errors
    /// [`RecvTimeoutError::Timeout`] when no outcome arrived in time.
    pub fn recv_outcome_timeout(&self, timeout: Duration) -> Result<Outcome, RecvTimeoutError> {
        self.outcomes_rx.recv_timeout(timeout)
    }

    /// Starts the thread over a fresh queue, answering on the writer's outcome channel.
    fn spawn(&self) -> std::io::Result<Worker<Job>> {
        let (tx, rx) = mpsc::channel::<Job>();
        let outcomes = self.outcomes_tx.clone();
        let run = self.run;
        let thread_name = self.thread_name;
        let handle = ms_thread::Builder::new().name(thread_name.to_owned()).spawn(move || {
            while let Ok(job) = rx.recv() {
                // The owner outlives the thread (it joins it on drop), so a send can only fail
                // during that drop, when nobody would read the answer anyway.
                if outcomes.send(run(job)).is_err() {
                    runtime_log::log_warn(format!("[preset_store] an outcome of the writer thread {thread_name} had no reader (its owner is closing)"));
                }
            }
        })?;
        Ok(Worker { tx: Some(tx), handle: Some(handle) })
    }
}

impl<Job, Outcome> Drop for OrderedWriter<Job, Outcome> {
    /// Closes the queue and waits for the queued jobs, so an operation made just before the owner
    /// closes still reaches the disk.
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            join_worker(self.thread_name, worker);
        }
    }
}

/// Closes `worker`'s queue and waits for its thread, logging a panic.
fn join_worker<Job>(thread_name: &str, mut worker: Worker<Job>) {
    drop(worker.tx.take());
    if let Some(handle) = worker.handle.take()
        && handle.join().is_err()
    {
        runtime_log::log_error(format!("[preset_store] the writer thread {thread_name} panicked; the last queued preset operations may not be saved"));
    }
}

/// The default name for a new preset: `candidate(index)` for the lowest 1-based index that
/// `is_taken` rejects, so a hole left by a rename or a deletion is reused.
///
/// `taken_count` is how many names the store holds. Both searches are BOUNDED by it (one more
/// candidate than there are names — the pigeonhole bound), never open-ended: a catalog entry that
/// lost its `{index}` placeholder makes every candidate the same string, and the search then
/// degrades to a `"<base> (<n>)"` suffix instead of hanging the GUI thread.
#[must_use]
pub fn free_default_name(taken_count: usize, candidate: impl Fn(usize) -> String, is_taken: impl Fn(&str) -> bool) -> String {
    let bound = taken_count.saturating_add(1);
    for index in 1..=bound {
        let name = candidate(index);
        if !is_taken(&name) {
            return name;
        }
    }
    let base = candidate(bound);
    (2..=bound.saturating_add(1))
        .map(|suffix| format!("{base} ({suffix})"))
        .find(|name| !is_taken(name))
        // Unreachable by the same bound: `taken_count + 1` candidates against `taken_count` names.
        .unwrap_or_else(|| format!("{base} ({})", bound.saturating_add(1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU32, Ordering};

    const TIMEOUT: Duration = Duration::from_secs(30);

    fn double(job: u32) -> u32 {
        job * 2
    }

    #[test]
    fn jobs_run_in_submission_order_and_every_one_is_answered() -> Result<(), String> {
        let mut writer = OrderedWriter::new("test-ordered-writer", double);
        for job in 1..=50 {
            writer.submit(job).map_err(|err| format!("{err:?}"))?;
        }
        let answers: Vec<u32> = (0..50).map(|_| writer.recv_outcome_timeout(TIMEOUT)).collect::<Result<_, _>>().map_err(|err| err.to_string())?;
        assert_eq!(answers, (1..=50).map(|job| job * 2).collect::<Vec<_>>());
        assert_eq!(writer.try_recv_outcome(), None, "nothing more");
        Ok(())
    }

    fn panic_on_zero(job: u32) -> u32 {
        assert_ne!(job, 0, "test writer told to die");
        job
    }

    #[test]
    fn a_dead_thread_returns_the_job_and_the_next_submit_restarts() -> Result<(), String> {
        let mut writer = OrderedWriter::new("test-ordered-writer-dies", panic_on_zero);
        writer.submit(0).map_err(|err| format!("{err:?}"))?;
        // The thread panics on job 0; keep submitting until a submit finds it gone.
        let mut lost = None;
        for _ in 0..1000 {
            match writer.submit(7) {
                Ok(()) => std::thread::sleep(Duration::from_millis(5)),
                Err(SubmitError::WorkerGone { job }) => {
                    lost = Some(job);
                    break;
                }
                Err(SubmitError::Spawn { error, .. }) => return Err(error.to_string()),
            }
        }
        assert_eq!(lost, Some(7), "the job comes back unrun");
        writer.submit(9).map_err(|err| format!("{err:?}"))?;
        assert_eq!(writer.recv_outcome_timeout(TIMEOUT), Ok(9), "a fresh thread answers on the same channel");
        Ok(())
    }

    static SLOW_JOBS_DONE: AtomicU32 = AtomicU32::new(0);

    fn slow_count(job: u32) -> u32 {
        std::thread::sleep(Duration::from_millis(20));
        SLOW_JOBS_DONE.fetch_add(1, Ordering::SeqCst);
        job
    }

    #[test]
    fn drop_waits_for_the_queued_jobs() -> Result<(), String> {
        let mut writer = OrderedWriter::new("test-ordered-writer-drop", slow_count);
        for job in 0..5 {
            writer.submit(job).map_err(|err| format!("{err:?}"))?;
        }
        drop(writer);
        assert_eq!(SLOW_JOBS_DONE.load(Ordering::SeqCst), 5, "the drop joined the thread after every queued job");
        Ok(())
    }

    #[test]
    fn the_lowest_free_index_is_reused() {
        let taken: BTreeSet<String> = ["P 1", "P 3"].into_iter().map(str::to_owned).collect();
        let name = free_default_name(taken.len(), |index| format!("P {index}"), |name| taken.contains(name));
        assert_eq!(name, "P 2");
        let none_taken = free_default_name(0, |index| format!("P {index}"), |_| false);
        assert_eq!(none_taken, "P 1");
    }

    #[test]
    fn a_catalog_without_the_index_placeholder_degrades_to_a_suffix() {
        let taken: BTreeSet<String> = ["Preset", "Preset (2)"].into_iter().map(str::to_owned).collect();
        let name = free_default_name(taken.len(), |_| "Preset".to_owned(), |name| taken.contains(name));
        assert_eq!(name, "Preset (3)");
    }
}
