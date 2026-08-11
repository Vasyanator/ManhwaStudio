/*
File: src/config_saver.rs

Purpose:
The one debouncing, retrying writer thread every self-owned section of
`user_config.json` is written through. Owns the DURABILITY POLICY of those
sections — coalescing, retry with backoff, final attempt on shutdown — so the
sections cannot drift into differing policies.

Main responsibilities:
- keep every config write off the GUI thread and off the disk until a gesture settles;
- never drop a payload a failed write still owes the disk;
- report a payload that is lost anyway as an error naming the cause, the path and the context.

Key structures:
- `SaverPayload`: what a section hands over, and how a newer value folds over an older one.
- `SaverError`: a write failure and whether repeating it could succeed.
- `SaverLabels`: the subsystem strings the log lines are built from.
- `SaverTiming` / `HeldPayload`: the loop's delays and the payload a failed write still owes.
- `ConfigSaver`: the writer thread and its handle.

Key functions:
- `ConfigSaver::spawn`, `ConfigSaver::store`, `ConfigSaver::flush_and_join`
- `run_saver_loop`, `wait_for_wake`, `fold_save_message`

Notes:
Consumers: `window_geometry.rs` (the `Window` section) and
`widgets/panel_dock/persist.rs` (the `PanelLayout` section). Each keeps its own
serde mirror, its own typed error and its own `write` step; only the thread, the
debounce and the retry live here.

DURABILITY. The saver is the LAST owner of a payload: its feeders clear their
dirty state when they hand one over, so a payload the saver drops is gone from
the whole process. A failed write therefore holds its payload and retries it with
a doubling, capped backoff, folding any newer payload over it; the shutdown — and
a disconnected channel — makes one final attempt, and a payload that is lost
anyway is logged as lost.

The write step itself goes through `config::update_user_config_file`, the single
locked read-modify-write border of `user_config.json`; this module only decides
WHEN it runs and what happens when it fails.
*/

use std::fmt::Display;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use ms_thread as thread;
use thread::JoinHandle;

use crate::config;
use crate::runtime_log;

/// How long the writer thread coalesces incoming payloads before writing.
///
/// A window drag or a panel drag produces a payload per frame, so a write per change would be
/// a write per frame. Only the last payload of a burst reaches the disk.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(700);

/// How long the writer waits before the FIRST retry of a payload whose write failed, and the
/// ceiling that delay doubles up to.
///
/// A write can fail for a reason that goes away on its own — the file held open by a backup
/// tool, a full or momentarily read-only volume — while the feeder has already forgotten the
/// payload, so the saver is the only place it still exists. The delays are long enough that a
/// permanently failing write costs one attempt per minute instead of a hot loop.
const RETRY_FIRST_BACKOFF: Duration = Duration::from_secs(2);
const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(60);

/// What a config section hands to its saver.
///
/// [`SaverPayload::coalesce`] is the section's ONE coalescing rule: it is applied both inside
/// the debounce window and over a payload a failed write still holds, so "the newest value
/// wins" means the same thing on both paths. It must be equivalent to writing `self` and then
/// `newer`, or the retry queue would produce a state no sequence of writes could.
pub trait SaverPayload: Send + Sized + 'static {
    /// Folds `newer` over `self`, keeping whatever `newer` says nothing about.
    fn coalesce(&mut self, newer: Self);
}

/// A write failure the saver has to decide about.
pub trait SaverError: Display {
    /// Whether repeating the same write later could succeed.
    ///
    /// `false` means the failure is a property of the situation, not of the moment (a refusal
    /// to overwrite a section written by a newer build, for instance): retrying it would only
    /// burn attempts and log lines.
    fn is_retryable(&self) -> bool;
}

/// The subsystem strings a saver's log lines are built from.
///
/// Never localized: log text (`dev-docs/i18n_exclusions.md`).
#[derive(Debug, Clone, Copy)]
pub struct SaverLabels {
    /// Prefix every line starts with, e.g. `"[window-geometry]"`.
    pub tag: &'static str,
    /// What is being written, as a noun phrase, e.g. `"the window geometry"`.
    pub subject: &'static str,
    /// OS thread name, e.g. `"window-geometry-saver"`.
    pub thread_name: &'static str,
}

/// The delays the writer loop runs on. A parameter rather than three constants read directly,
/// so the retry behaviour is testable in milliseconds.
#[derive(Debug, Clone, Copy)]
pub struct SaverTiming {
    /// How long a burst of payloads is coalesced before it is written.
    pub debounce: Duration,
    /// Delay before the first retry of a failed write.
    pub first_backoff: Duration,
    /// Ceiling the retry delay doubles up to.
    pub max_backoff: Duration,
}

impl SaverTiming {
    /// The timing the studio runs on.
    pub const PRODUCTION: Self = Self {
        debounce: SAVE_DEBOUNCE,
        first_backoff: RETRY_FIRST_BACKOFF,
        max_backoff: RETRY_MAX_BACKOFF,
    };
}

/// Message queue of a writer thread.
#[derive(Debug)]
pub enum SaveMessage<T> {
    /// A newer payload, folded over whatever is pending.
    Snapshot(T),
    /// Write whatever is pending and stop (app exit).
    Shutdown,
}

/// Folds one message into the pending payload, returning `true` when the burst must end now.
///
/// Pure, so the coalescing rule ("the newest value wins, shutdown ends the burst") is testable
/// without threads or timing.
fn fold_save_message<T: SaverPayload>(pending: &mut Option<T>, message: SaveMessage<T>) -> bool {
    match message {
        SaveMessage::Snapshot(payload) => {
            coalesce_into(pending, payload);
            false
        }
        SaveMessage::Shutdown => true,
    }
}

/// Folds `payload` over `pending`, or installs it when there is nothing pending yet.
fn coalesce_into<T: SaverPayload>(pending: &mut Option<T>, payload: T) {
    match pending.as_mut() {
        Some(pending) => pending.coalesce(payload),
        None => *pending = Some(payload),
    }
}

/// A payload whose write failed and which the saver still owes the disk.
#[derive(Debug)]
struct HeldPayload<T> {
    /// The value still owed to the disk.
    payload: T,
    /// When the next attempt is due.
    due: Instant,
    /// Delay to use for the attempt after the one that is due (already doubled).
    backoff: Duration,
    /// How many attempts at this held payload have failed so far.
    failures: u32,
}

/// What woke the writer loop up.
#[derive(Debug)]
enum Wake<T> {
    /// A message arrived from the GUI thread.
    Message(SaveMessage<T>),
    /// The retry of a held payload fell due with no message in between.
    RetryDue,
    /// Every sender is gone: no further message can arrive, so whatever is held gets one last
    /// attempt and the thread ends.
    Disconnected,
}

/// Blocks until a message arrives, or until `retry_due` (when there is one).
fn wait_for_wake<T>(rx: &Receiver<SaveMessage<T>>, retry_due: Option<Instant>) -> Wake<T> {
    match retry_due {
        Some(due) => match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Ok(message) => Wake::Message(message),
            Err(RecvTimeoutError::Timeout) => Wake::RetryDue,
            Err(RecvTimeoutError::Disconnected) => Wake::Disconnected,
        },
        None => match rx.recv() {
            Ok(message) => Wake::Message(message),
            Err(_) => Wake::Disconnected,
        },
    }
}

/// Body of a writer thread: debounce, write, and retry what could not be written.
///
/// `write` is the disk step, injected so the loop's policy can be tested without a config file;
/// it receives the config path (already resolved once, on this thread) and the payload. `path`
/// also appears in the log lines, so a failure names the file it was about.
///
/// Policy, in order:
/// * a burst of payloads is coalesced for `timing.debounce` and written once;
/// * a failed write does NOT discard its payload — it is held and retried after a delay that
///   starts at `timing.first_backoff` and doubles up to `timing.max_backoff`, so a permanent
///   failure never becomes a hot loop;
/// * a payload that arrives while one is held is folded OVER it
///   ([`SaverPayload::coalesce`]), so the newest value wins under exactly the rule the debounce
///   window uses;
/// * `Shutdown` (and a disconnected channel) makes one final attempt at whatever is held, so an
///   app's `on_exit` cannot leave a held payload behind;
/// * an error that is not retryable, or one that survives the final attempt, is logged with its
///   cause and the target path — the change is lost at that point and the user must be able to
///   find out why.
pub fn run_saver_loop<T, E, W>(
    rx: &Receiver<SaveMessage<T>>,
    path: &Path,
    labels: SaverLabels,
    timing: SaverTiming,
    mut write: W,
) where
    T: SaverPayload,
    E: SaverError,
    W: FnMut(&Path, &T) -> Result<(), E>,
{
    let mut held: Option<HeldPayload<T>> = None;
    loop {
        let mut pending: Option<T> = None;
        let mut stop = false;
        match wait_for_wake(rx, held.as_ref().map(|state| state.due)) {
            Wake::Message(first) => {
                stop = fold_save_message(&mut pending, first);
                // Coalesce the rest of the burst: one write per settled gesture, not per frame.
                let deadline = Instant::now() + timing.debounce;
                while !stop {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        break;
                    };
                    match rx.recv_timeout(remaining) {
                        Ok(message) => stop = fold_save_message(&mut pending, message),
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => stop = true,
                    }
                }
            }
            Wake::RetryDue => {}
            Wake::Disconnected => stop = true,
        }

        // The held payload is the OLDER state, so it is the base the fresh one is folded over.
        let (mut attempt, backoff, mut failures) = match held.take() {
            Some(state) => (Some(state.payload), state.backoff, state.failures),
            None => (None, timing.first_backoff, 0),
        };
        if let Some(fresh) = pending {
            coalesce_into(&mut attempt, fresh);
        }

        if let Some(payload) = attempt {
            match write(path, &payload) {
                Ok(()) => {
                    if failures > 0 {
                        runtime_log::log_info(format!(
                            "{} {} reached {} after {failures} failed attempt(s)",
                            labels.tag,
                            labels.subject,
                            path.display()
                        ));
                    }
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    if stop || !error.is_retryable() {
                        runtime_log::log_error(format!(
                            "{} giving up on writing {} after {failures} attempt(s); the change \
                             made in this session is LOST. Path: {}. Error: {error}",
                            labels.tag,
                            labels.subject,
                            path.display()
                        ));
                    } else {
                        runtime_log::log_warn(format!(
                            "{} failed to write {} (attempt {failures}); retrying in {} ms. \
                             Path: {}. Error: {error}",
                            labels.tag,
                            labels.subject,
                            backoff.as_millis(),
                            path.display()
                        ));
                        held = Some(HeldPayload {
                            payload,
                            due: Instant::now() + backoff,
                            // Saturating, so no configured delay can overflow the doubling into
                            // a panic.
                            backoff: backoff.saturating_mul(2).min(timing.max_backoff),
                            failures,
                        });
                    }
                }
            }
        }

        if stop {
            break;
        }
    }
}

/// Handle of the writer thread that owns every write of one `user_config.json` section.
///
/// Dropping a saver without [`ConfigSaver::flush_and_join`] disconnects the channel; the loop
/// then makes its final attempt at a held payload but loses whatever was still inside the
/// debounce window, so an app's `on_exit` must flush it.
#[derive(Debug)]
pub struct ConfigSaver<T: SaverPayload> {
    /// Sender into the writer thread; `None` once the thread is gone.
    tx: Option<Sender<SaveMessage<T>>>,
    /// Writer thread handle, joined by [`ConfigSaver::flush_and_join`].
    join: Option<JoinHandle<()>>,
    /// Strings the saver's own log lines are built from.
    labels: SaverLabels,
}

impl<T: SaverPayload> ConfigSaver<T> {
    /// Spawns the writer thread on [`SaverTiming::PRODUCTION`].
    ///
    /// `write` runs on that thread and receives the resolved `user_config.json` path; it is the
    /// section's own read-modify-write step. A thread that cannot be spawned is logged and
    /// leaves the saver inert: the program still runs, it just stops persisting this section.
    ///
    /// The timing is fixed here rather than exposed: the loop's policy is tested through
    /// [`run_saver_loop`] directly (`test_harness`), so a per-caller timing would be surface
    /// nothing uses.
    #[must_use]
    pub fn spawn<E, W>(labels: SaverLabels, write: W) -> Self
    where
        E: SaverError,
        W: FnMut(&Path, &T) -> Result<(), E> + Send + 'static,
    {
        let timing = SaverTiming::PRODUCTION;
        let (tx, rx) = mpsc::channel::<SaveMessage<T>>();
        let spawn_result = thread::Builder::new()
            .name(labels.thread_name.to_owned())
            .spawn(move || {
                // Resolved once, on the writer thread: the GUI thread must not pay for it, and
                // the loop needs it for its log lines as much as the write step does.
                let path = config::user_config_path();
                run_saver_loop(&rx, &path, labels, timing, write);
            });
        match spawn_result {
            Ok(join) => Self {
                tx: Some(tx),
                join: Some(join),
                labels,
            },
            Err(err) => {
                runtime_log::log_error(format!(
                    "{} failed to spawn the {} thread; {} will not be remembered this session: \
                     {err}",
                    labels.tag, labels.thread_name, labels.subject
                ));
                Self {
                    tx: None,
                    join: None,
                    labels,
                }
            }
        }
    }

    /// Queues one payload. Never blocks and never touches the disk.
    pub fn store(&mut self, payload: T) {
        self.send(SaveMessage::Snapshot(payload));
    }

    /// Sends a message, dropping the sender if the thread has gone away.
    fn send(&mut self, message: SaveMessage<T>) {
        let Some(tx) = self.tx.as_ref() else {
            return;
        };
        if let Err(err) = tx.send(message) {
            runtime_log::log_error(format!(
                "{} the {} thread is gone; {} is no longer persisted: {err}",
                self.labels.tag, self.labels.thread_name, self.labels.subject
            ));
            self.tx = None;
        }
    }

    /// Writes the pending payload and joins the writer thread. Called from an app's `on_exit`,
    /// so a change made in the last moments before closing is not lost inside the debounce
    /// window. Idempotent.
    ///
    /// The shutdown also makes the final attempt at a payload an earlier write failed on (see
    /// [`run_saver_loop`]), which is the last moment the process still holds it; a failure there
    /// is logged as a lost change.
    pub fn flush_and_join(&mut self) {
        self.send(SaveMessage::Shutdown);
        self.tx = None;
        if let Some(join) = self.join.take()
            && join.join().is_err()
        {
            runtime_log::log_error(format!(
                "{} the {} thread panicked; the last {} may not have been written",
                self.labels.tag, self.labels.thread_name, self.labels.subject
            ));
        }
    }
}

/// Test-only driver for [`run_saver_loop`], shared by every section's own tests.
///
/// Every assertion is driven off recorded attempts rather than off elapsed time: a test steps
/// the loop attempt by attempt and never sleeps on a guess.
#[cfg(test)]
pub mod test_harness {
    use std::sync::{Arc, Mutex};

    use super::{
        Duration, Path, Receiver, SaveMessage, SaverError, SaverPayload, SaverTiming, Sender, mpsc,
        run_saver_loop,
    };

    /// Timing that keeps the retry tests in the millisecond range while leaving the debounce
    /// short enough not to dominate them.
    pub const FAST_TIMING: SaverTiming = SaverTiming {
        debounce: Duration::from_millis(5),
        first_backoff: Duration::from_millis(5),
        max_backoff: Duration::from_millis(20),
    };

    /// Timing whose backoff is longer than any test can take, so a second attempt can only come
    /// from a message or from the shutdown, never from an elapsed timer.
    pub const NO_TIMER_TIMING: SaverTiming = SaverTiming {
        debounce: Duration::from_millis(5),
        first_backoff: Duration::from_secs(3600),
        max_backoff: Duration::from_secs(3600),
    };

    /// How long a test waits for one attempt before declaring the loop stalled.
    const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

    /// Records every payload the injected writer was asked to write and lets the test wait for
    /// each attempt instead of sleeping on a guess.
    #[derive(Debug)]
    struct AttemptLog<T> {
        /// Every payload handed to the writer, in order.
        attempts: Mutex<Vec<T>>,
        /// One message per finished attempt, so a test can step the loop.
        done: Sender<()>,
    }

    impl<T: Clone> AttemptLog<T> {
        /// Records one attempt and wakes whoever waits for it.
        fn record(&self, payload: &T) {
            let mut attempts = match self.attempts.lock() {
                Ok(attempts) => attempts,
                Err(poisoned) => poisoned.into_inner(),
            };
            attempts.push(payload.clone());
            drop(attempts);
            if self.done.send(()).is_err() {
                unreachable!("the test outlives the writer loop");
            }
        }

        /// Snapshot of what has been attempted so far.
        fn snapshots(&self) -> Vec<T> {
            match self.attempts.lock() {
                Ok(attempts) => attempts.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }
    }

    /// One writer loop running on its own thread with an injected write step.
    #[derive(Debug)]
    pub struct LoopHarness<T> {
        /// Sender into the loop under test; `None` once the test disconnected it.
        tx: Option<Sender<SaveMessage<T>>>,
        /// What the injected writer recorded.
        log: Arc<AttemptLog<T>>,
        /// One message per finished attempt.
        done: Receiver<()>,
        /// The loop's thread.
        join: std::thread::JoinHandle<()>,
    }

    impl<T: SaverPayload + Clone> LoopHarness<T> {
        /// Starts [`run_saver_loop`] with an injected writer whose verdict is asked per attempt
        /// number (1-based). No disk is touched: the path is a log-only label.
        pub fn start<E, V>(timing: SaverTiming, mut verdict: V) -> Self
        where
            E: SaverError,
            V: FnMut(u32) -> Result<(), E> + Send + 'static,
        {
            let (tx, rx) = mpsc::channel::<SaveMessage<T>>();
            let (done_tx, done) = mpsc::channel::<()>();
            let log = Arc::new(AttemptLog {
                attempts: Mutex::new(Vec::new()),
                done: done_tx,
            });
            let writer_log = Arc::clone(&log);
            let labels = super::SaverLabels {
                tag: "[config-saver-test]",
                subject: "the test payload",
                thread_name: "config-saver-test",
            };
            let join = match std::thread::Builder::new()
                .name("config-saver-test".to_owned())
                .spawn(move || {
                    let mut attempt = 0u32;
                    run_saver_loop(
                        &rx,
                        Path::new("/nonexistent/user_config.json"),
                        labels,
                        timing,
                        |_path, payload| {
                            attempt = attempt.saturating_add(1);
                            writer_log.record(payload);
                            verdict(attempt)
                        },
                    );
                }) {
                Ok(join) => join,
                Err(error) => unreachable!("the test writer thread spawns: {error}"),
            };
            Self {
                tx: Some(tx),
                log,
                done,
                join,
            }
        }

        /// Queues one payload.
        pub fn store(&self, payload: T) {
            self.send(SaveMessage::Snapshot(payload));
        }

        /// Asks the loop to write what is pending and stop.
        pub fn shutdown(&self) {
            self.send(SaveMessage::Shutdown);
        }

        /// Sends one message into the loop.
        fn send(&self, message: SaveMessage<T>) {
            let Some(tx) = self.tx.as_ref() else {
                unreachable!("the test disconnected the sender before sending");
            };
            if let Err(error) = tx.send(message) {
                unreachable!("the writer loop is still running: {error}");
            }
        }

        /// Drops the sender, which is what a saver dropped without `flush_and_join` does to the
        /// loop.
        pub fn disconnect(&mut self) {
            self.tx = None;
        }

        /// Blocks until one more attempt is recorded, failing on a stall.
        pub fn await_attempt(&self) {
            match self.done.recv_timeout(ATTEMPT_TIMEOUT) {
                Ok(()) => {}
                Err(error) => unreachable!("the writer loop must attempt a write: {error}"),
            }
        }

        /// Joins the loop's thread and returns every payload the writer was asked to write, in
        /// order. Fails the test if the loop panicked.
        pub fn join_and_take_attempts(mut self) -> Vec<T> {
            self.tx = None;
            if self.join.join().is_err() {
                unreachable!("the writer loop must not panic");
            }
            self.log.snapshots()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_harness::{FAST_TIMING, LoopHarness, NO_TIMER_TIMING};
    use super::{RETRY_FIRST_BACKOFF, RETRY_MAX_BACKOFF, SaverError, SaverPayload};

    /// A payload of two independent fields, each of which only a `Some` overwrites — the same
    /// shape both real sections have (a value the newest message says nothing about is still
    /// owed to the disk).
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    struct TestPayload {
        a: Option<u32>,
        b: Option<u32>,
    }

    impl SaverPayload for TestPayload {
        fn coalesce(&mut self, newer: Self) {
            if newer.a.is_some() {
                self.a = newer.a;
            }
            if newer.b.is_some() {
                self.b = newer.b;
            }
        }
    }

    /// A failure whose retryability the test chooses.
    #[derive(Debug)]
    struct TestError {
        retryable: bool,
    }

    impl std::fmt::Display for TestError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "test write failure (retryable={})", self.retryable)
        }
    }

    impl SaverError for TestError {
        fn is_retryable(&self) -> bool {
            self.retryable
        }
    }

    /// Fails the first `n` attempts with a retryable error and lets every later one succeed.
    fn fail_first(n: u32) -> impl FnMut(u32) -> Result<(), TestError> + Send + 'static {
        move |attempt| {
            if attempt <= n {
                Err(TestError { retryable: true })
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn a_failed_write_is_retried_until_it_succeeds() {
        // The regression this loop exists for: the feeder forgets a payload when it hands it
        // over, so a payload dropped on a transient failure is gone from the whole process.
        let harness = LoopHarness::start(FAST_TIMING, fail_first(2));
        let payload = TestPayload {
            a: Some(1),
            b: Some(2),
        };
        harness.store(payload.clone());

        // Two failures and the success that follows them, with no further input from the GUI
        // thread.
        harness.await_attempt();
        harness.await_attempt();
        harness.await_attempt();

        harness.shutdown();
        let attempts = harness.join_and_take_attempts();
        assert_eq!(attempts, vec![payload.clone(), payload.clone(), payload]);
    }

    #[test]
    fn a_shutdown_makes_the_final_attempt_at_a_held_payload() {
        let harness = LoopHarness::start(NO_TIMER_TIMING, fail_first(1));
        let payload = TestPayload {
            a: Some(7),
            b: None,
        };
        harness.store(payload.clone());
        harness.await_attempt();

        harness.shutdown();
        harness.await_attempt();
        assert_eq!(
            harness.join_and_take_attempts(),
            vec![payload.clone(), payload]
        );
    }

    #[test]
    fn dropping_the_sender_still_makes_the_final_attempt() {
        // A saver dropped without `flush_and_join` disconnects the channel; the held payload
        // must not go with it.
        let mut harness = LoopHarness::start(NO_TIMER_TIMING, fail_first(1));
        harness.store(TestPayload {
            a: Some(3),
            b: None,
        });
        harness.await_attempt();

        harness.disconnect();
        harness.await_attempt();
        assert_eq!(harness.join_and_take_attempts().len(), 2);
    }

    #[test]
    fn a_newer_payload_is_folded_over_the_held_one() {
        let harness = LoopHarness::start(NO_TIMER_TIMING, fail_first(1));
        harness.store(TestPayload {
            a: Some(1),
            b: Some(2),
        });
        harness.await_attempt();

        // The newer payload says nothing about `b`, which the failed write still owes.
        harness.store(TestPayload {
            a: Some(9),
            b: None,
        });
        harness.await_attempt();
        harness.shutdown();

        let attempts = harness.join_and_take_attempts();
        assert_eq!(
            attempts,
            vec![
                TestPayload {
                    a: Some(1),
                    b: Some(2)
                },
                TestPayload {
                    a: Some(9),
                    b: Some(2)
                },
            ]
        );
    }

    #[test]
    fn a_non_retryable_failure_is_never_repeated() {
        // Nothing is held, so the shutdown writes nothing at all.
        let harness = LoopHarness::<TestPayload>::start(FAST_TIMING, |_| {
            Err(TestError { retryable: false })
        });
        harness.store(TestPayload {
            a: Some(1),
            b: None,
        });
        harness.await_attempt();

        harness.shutdown();
        assert_eq!(harness.join_and_take_attempts().len(), 1);
    }

    #[test]
    fn the_retry_backoff_grows_and_stops_at_the_ceiling() {
        // The production ceiling must bound the doubling, or a permanently failing write would
        // drift into an ever longer silence.
        assert!(RETRY_FIRST_BACKOFF <= RETRY_MAX_BACKOFF);
        let mut backoff = RETRY_FIRST_BACKOFF;
        for _ in 0..16 {
            backoff = backoff.saturating_mul(2).min(RETRY_MAX_BACKOFF);
        }
        assert_eq!(backoff, RETRY_MAX_BACKOFF);
    }
}
