/*
File: src/studio/session/project/staging_probe.rs

Purpose:
`StagingDirProbe`, the off-GUI-thread "does the chapter's staging directory exist?" check behind
`ProjectSession::refresh_unsaved_changes_cache`. The GUI thread only sends a request and polls the
answer; the stat runs on one long-lived `ms_thread` worker.

Key structures:
- `StagingDirProbe`: request channel, answer channel, generation and the in-flight flag.

Key functions:
- `StagingDirProbe::new`, `request`, `poll`, `invalidate`.

Notes:
Why a probe at all, instead of answering from `StagingWriters` / `AutosaveGate` facts: the three
gated writers are not the only staging writers. The translation tab and the canvas write files
under `ProjectPaths::unsaved_dir` directly (`machine_translation.rs`, `ms-canvas/src/lib.rs`
image-bubble paths), and a layer write leaves `LayerDoc::has_pending_saves` the moment its ack is
polled, so "no writer holds anything" does not imply "nothing was staged". The directory itself is
the only complete witness.

Generations: an answer counts only for the generation it was asked in. `invalidate` (a save or a
discard just cleared the session's unsaved flag) bumps it, so an answer to a question asked before
the merge deleted the directory can never re-latch "unsaved" afterwards. One request is in flight
at a time; the worker exits when the probe is dropped (its request sender closes).
*/

use crate::runtime_log;
use ms_thread as thread;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

/// The staging-directory probe of one project session (see the file header).
#[derive(Debug)]
pub(super) struct StagingDirProbe {
    /// Directory the worker stats (`ProjectPaths::unsaved_dir`).
    unsaved_dir: PathBuf,
    /// The worker's request end, spawned on the first [`Self::request`]; `None` before that.
    worker: Option<ProbeWorker>,
    /// Generation of the current question; answers of older generations are ignored.
    generation: u64,
    /// A request was sent and its answer has not been polled yet.
    in_flight: bool,
    /// The worker thread could not be started; the probe stays silent (logged once).
    unavailable: bool,
}

/// The two channel ends to the probe worker.
#[derive(Debug)]
struct ProbeWorker {
    requests: Sender<u64>,
    answers: Receiver<(u64, bool)>,
}

impl StagingDirProbe {
    /// A probe of `unsaved_dir`. Spawns nothing and touches no file until the first request.
    pub(super) fn new(unsaved_dir: PathBuf) -> Self {
        Self { unsaved_dir, worker: None, generation: 0, in_flight: false, unavailable: false }
    }

    /// Asks the worker whether the directory exists now, unless a request is already in flight.
    /// The first call starts the worker; a refused thread is logged once and the probe then never
    /// answers (the session's other unsaved facts still apply). Cheap, no I/O on the caller.
    pub(super) fn request(&mut self) {
        if self.in_flight || self.unavailable {
            return;
        }
        if self.worker.is_none() {
            match spawn_probe_worker(self.unsaved_dir.clone()) {
                Ok(worker) => self.worker = Some(worker),
                Err(err) => {
                    self.unavailable = true;
                    runtime_log::log_error(format!(
                        "[unsaved] the staging-directory probe thread could not be started: {err}. Possible cause: the OS thread limit or low memory. A staging directory written outside the gated writers is not detected for this session.\nPath: {}",
                        self.unsaved_dir.display()
                    ));
                    return;
                }
            }
        }
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        if worker.requests.send(self.generation).is_err() {
            self.mark_worker_lost();
            return;
        }
        self.in_flight = true;
    }

    /// Takes the worker's answer if it arrived: `Some(exists)` for an answer to the CURRENT
    /// generation, `None` while waiting, for a stale answer, or when no request is in flight.
    pub(super) fn poll(&mut self) -> Option<bool> {
        let worker = self.worker.as_ref()?;
        match worker.answers.try_recv() {
            Ok((generation, exists)) => {
                self.in_flight = false;
                (generation == self.generation).then_some(exists)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.mark_worker_lost();
                None
            }
        }
    }

    /// Discards every answer to a question asked before this call (the unsaved flag was just
    /// cleared by a save or a discard, so an older "exists" would be a false positive).
    pub(super) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// The worker is gone (it never exits on its own while the probe lives): log it and stop asking.
    fn mark_worker_lost(&mut self) {
        self.worker = None;
        self.in_flight = false;
        self.unavailable = true;
        runtime_log::log_error(format!(
            "[unsaved] the staging-directory probe thread ended unexpectedly; a staging directory written outside the gated writers is not detected for the rest of this session.\nPath: {}",
            self.unsaved_dir.display()
        ));
    }
}

/// Starts the probe worker: it answers each request (a generation) with that generation and
/// whether `unsaved_dir` exists ([`staging_present`]: an inspection error other than "not found"
/// reads as PRESENT), until the request sender is dropped or the answer receiver is gone.
fn spawn_probe_worker(unsaved_dir: PathBuf) -> std::io::Result<ProbeWorker> {
    let (requests, request_rx) = mpsc::channel::<u64>();
    let (answer_tx, answers) = mpsc::channel::<(u64, bool)>();
    // Detached: it ends with the probe (closed request channel); nothing ever joins it.
    let _detached = thread::Builder::new().name("staging-dir-probe".to_owned()).spawn(move || {
        let mut error_logged = false;
        while let Ok(generation) = request_rx.recv() {
            let present = staging_present(&unsaved_dir, unsaved_dir.try_exists(), &mut error_logged);
            if answer_tx.send((generation, present)).is_err() {
                break;
            }
        }
    })?;
    Ok(ProbeWorker { requests, answers })
}

/// Reads one `Path::try_exists` answer for `unsaved_dir`: present or absent as reported, and an
/// inspection error (permission, a flaky network filesystem; "not found" is already `Ok(false)`)
/// as PRESENT — the conservative direction, since a staging directory that may exist must keep
/// the exit dialog. The first such error of a worker is logged (`error_logged` latches it).
fn staging_present(unsaved_dir: &Path, inspected: std::io::Result<bool>, error_logged: &mut bool) -> bool {
    match inspected {
        Ok(present) => present,
        Err(err) => {
            if !*error_logged {
                *error_logged = true;
                runtime_log::log_warn(format!(
                    "[unsaved] could not inspect the staging directory; the session is treated as unsaved. Possible cause: missing permissions or an unavailable drive.\nPath: {}\nError: {err}",
                    unsaved_dir.display()
                ));
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{StagingDirProbe, staging_present};
    use web_time::{Duration, Instant};

    /// Polls `probe` until it answers or two seconds pass (the worker answers in microseconds).
    fn answer(probe: &mut StagingDirProbe) -> Option<bool> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Some(exists) = probe.poll() {
                return Some(exists);
            }
            if !probe.in_flight {
                return None;
            }
            std::thread::yield_now();
        }
        None
    }

    /// The probe answers whether the directory exists, one request at a time, and follows the
    /// directory as it appears.
    #[test]
    fn the_probe_follows_the_staging_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let staging = dir.path().join("Chapter_unsaved");
        let mut probe = StagingDirProbe::new(staging.clone());
        assert_eq!(probe.poll(), None, "nothing asked, nothing answered");
        probe.request();
        assert_eq!(answer(&mut probe), Some(false));
        std::fs::create_dir_all(&staging).expect("create staging dir");
        probe.request();
        assert_eq!(answer(&mut probe), Some(true));
    }

    /// An answer to a question asked before `invalidate` is dropped, and the next request is
    /// answered normally.
    #[test]
    fn an_answer_from_before_invalidate_is_ignored() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut probe = StagingDirProbe::new(dir.path().to_path_buf());
        probe.request();
        probe.invalidate();
        assert_eq!(answer(&mut probe), None, "the stale 'exists' must not count");
        assert!(!probe.in_flight, "the stale answer still ends the request");
        probe.request();
        assert_eq!(answer(&mut probe), Some(true));
    }

    /// An inspection error counts as present (unsaved), logged once; plain answers pass through.
    #[test]
    fn an_inspection_error_reads_as_present() {
        let dir = std::path::Path::new("/home/u/Title/Chapter_unsaved");
        let mut logged = false;
        assert!(staging_present(dir, Ok(true), &mut logged));
        assert!(!staging_present(dir, Ok(false), &mut logged));
        assert!(!logged);
        let denied = || Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(staging_present(dir, denied(), &mut logged), "unknown is treated as unsaved");
        assert!(logged);
        assert!(staging_present(dir, denied(), &mut logged));
    }
}
