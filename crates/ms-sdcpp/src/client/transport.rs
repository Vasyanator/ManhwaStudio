/*
File: crates/ms-sdcpp/src/client/transport.rs

Purpose:
One live worker process as the client sees it: the request pipe, a reader thread that decodes
event frames into a channel, a stderr drain thread that logs every line and keeps the last
64 for crash reports, and the process handle (crash description, kill).

Key items:
- LiveWorker        : start / send / recv / crash_error / shutdown / kill
- Received          : one `recv` outcome (event, timeout, channel closed)
- STDERR_TAIL_LINES : how many stderr lines a crash report carries

Notes:
Dropping a `LiveWorker` kills and reaps the process if it is still running, so a worker is
never left behind. The reader and drain threads end when the process's pipes close.
*/

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ms_log::runtime_log;

use crate::catalog::SdCppBuild;
use crate::error::SdCppError;
use crate::protocol::{self, Event, Request};

use super::spawn::{WorkerChild, WorkerPipes};

/// stderr lines kept for a crash report.
pub(crate) const STDERR_TAIL_LINES: usize = 64;
/// How long a closed worker is given to report its exit status.
const EXIT_STATUS_WAIT: Duration = Duration::from_secs(2);

/// What the reader thread delivers.
enum Output {
    Event(Event, Vec<u8>),
    Closed(String),
}

/// One outcome of [`LiveWorker::recv`].
pub(crate) enum Received {
    /// An event frame and its blob.
    Event(Event, Vec<u8>),
    /// Nothing within the timeout.
    Timeout,
    /// The worker's stdout closed or carried garbage (`detail`): it is gone.
    Closed(String),
}

/// A running worker process.
pub(crate) struct LiveWorker {
    /// The build the worker loaded.
    pub(crate) build: SdCppBuild,
    stdin: Box<dyn Write + Send>,
    events: mpsc::Receiver<Output>,
    child: Box<dyn WorkerChild>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    finished: bool,
}

impl LiveWorker {
    /// Wraps launched pipes, starting the reader and stderr drain threads.
    ///
    /// # Errors
    /// `WorkerSpawn` when a thread cannot be started (the process is killed).
    pub(crate) fn start(build: SdCppBuild, pipes: WorkerPipes) -> Result<Self, SdCppError> {
        let WorkerPipes { stdin, stdout, stderr, mut child } = pipes;
        let (tx, events) = mpsc::channel();
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let spawn_failed = |err: std::io::Error, child: &mut Box<dyn WorkerChild>| {
            if let Err(kill_err) = child.kill() {
                runtime_log::log_warn(format!("[sdcpp] cannot kill a worker after a thread failure: {kill_err}"));
            }
            SdCppError::WorkerSpawn { detail: format!("thread: {err}") }
        };
        if let Err(err) = ms_thread::Builder::new().name("sdcpp-client-reader".to_owned()).spawn(move || read_events(stdout, &tx)) {
            return Err(spawn_failed(err, &mut child));
        }
        if let Some(stderr) = stderr {
            let tail = stderr_tail.clone();
            if let Err(err) = ms_thread::Builder::new().name("sdcpp-client-stderr".to_owned()).spawn(move || drain_stderr(stderr, &tail)) {
                return Err(spawn_failed(err, &mut child));
            }
        }
        Ok(Self { build, stdin, events, child, stderr_tail, finished: false })
    }

    /// Sends one request frame.
    ///
    /// # Errors
    /// `Protocol` with the codec's message (the worker's stdin is closed: it is gone).
    pub(crate) fn send(&mut self, request: &Request, blob: &[u8]) -> Result<(), SdCppError> {
        protocol::write_message(&mut self.stdin, request, blob).map_err(|detail| SdCppError::Protocol { detail })
    }

    /// Waits up to `timeout` for the next event.
    pub(crate) fn recv(&mut self, timeout: Duration) -> Received {
        match self.events.recv_timeout(timeout) {
            Ok(Output::Event(event, blob)) => Received::Event(event, blob),
            Ok(Output::Closed(detail)) => Received::Closed(detail),
            Err(RecvTimeoutError::Timeout) => Received::Timeout,
            Err(RecvTimeoutError::Disconnected) => Received::Closed("the event reader ended".to_owned()),
        }
    }

    /// Whether the process has exited (a dead worker is never reused).
    pub(crate) fn has_exited(&mut self) -> bool {
        if self.finished {
            return true;
        }
        match self.child.try_wait() {
            Ok(Some(exit)) => {
                runtime_log::log_info(format!("[sdcpp] worker exited: {exit}"));
                self.finished = true;
                true
            }
            Ok(None) => false,
            Err(err) => {
                runtime_log::log_warn(format!("[sdcpp] cannot query the worker status: {err}"));
                false
            }
        }
    }

    /// The error of a worker whose event channel closed: waits briefly for its exit status
    /// (killing it if it does not end) and returns `WorkerCrashed` with the stderr tail.
    pub(crate) fn crash_error(&mut self, detail: &str) -> SdCppError {
        let deadline = Instant::now() + EXIT_STATUS_WAIT;
        let exit = loop {
            match self.child.try_wait() {
                Ok(Some(exit)) => break exit,
                Ok(None) if Instant::now() < deadline => ms_thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    self.kill();
                    break "killed after its output closed".to_owned();
                }
                Err(err) => break format!("unknown ({err})"),
            }
        };
        self.finished = true;
        // Give the drain thread a moment to collect the last lines the process wrote.
        ms_thread::sleep(Duration::from_millis(50));
        let tail = self.stderr_tail.lock().unwrap_or_else(PoisonError::into_inner).iter().cloned().collect::<Vec<_>>().join("\n");
        runtime_log::log_error(format!("[sdcpp] worker crashed ({exit}); channel: {detail}"));
        SdCppError::WorkerCrashed { exit, stderr_tail: tail }
    }

    /// Asks the worker to exit, waits up to `wait`, then kills it.
    pub(crate) fn shutdown(mut self, wait: Duration) {
        if let Err(err) = self.send(&Request::Shutdown, &[]) {
            runtime_log::log_info(format!("[sdcpp] worker already gone at shutdown: {err}"));
        }
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            if self.has_exited() {
                return;
            }
            ms_thread::sleep(Duration::from_millis(20));
        }
        runtime_log::log_warn("[sdcpp] worker did not exit after Shutdown; killing it");
        self.kill();
    }

    /// Kills and reaps the process (logged on failure).
    pub(crate) fn kill(&mut self) {
        if self.finished {
            return;
        }
        if let Err(err) = self.child.kill() {
            runtime_log::log_warn(format!("[sdcpp] cannot kill the worker: {err}"));
        }
        self.finished = true;
    }
}

impl Drop for LiveWorker {
    fn drop(&mut self) {
        if !self.has_exited() {
            self.kill();
        }
    }
}

/// The reader thread: decodes event frames until the pipe closes.
fn read_events(stdout: Box<dyn Read + Send>, tx: &mpsc::Sender<Output>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match protocol::read_message::<Event, _>(&mut reader) {
            Ok((event, blob)) => {
                if tx.send(Output::Event(event, blob)).is_err() {
                    return;
                }
            }
            Err(detail) => {
                // The receiver may already be gone (the worker was discarded).
                if tx.send(Output::Closed(detail)).is_err() {
                    runtime_log::log_info("[sdcpp] worker output closed after its client was dropped");
                }
                return;
            }
        }
    }
}

/// The stderr drain thread: logs every line and keeps the last [`STDERR_TAIL_LINES`].
fn drain_stderr(stderr: Box<dyn Read + Send>, tail: &Mutex<VecDeque<String>>) {
    for line in BufReader::new(stderr).lines() {
        let Ok(line) = line else {
            return;
        };
        runtime_log::log_info(format!("[sdcpp] {line}"));
        let mut tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
}
