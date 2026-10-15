/*
File: src/tabs/settings/hotkeys/writer.rs

Purpose:
Persists the pane's binding changes (`ms_input::OverrideEntry`) into `user_config` -> `Hotkeys`
off the GUI thread, in the order they were made, and reports failures back to the pane.

Key structures:
- `OverrideWriter`: the pane-side handle (lazily spawned worker + failure channel).
- `WriteFailure`: one failed write, for the pane's error line.

Key functions:
- `OverrideWriter::submit`, `OverrideWriter::drain_failures`.

Notes:
- ONE worker thread per pane, fed through a channel: `write_override` is a read-modify-write
  of one id, so two writes of the same id on two threads could land in the wrong order and
  persist a stale list. The worker drains whatever queued up and keeps only the latest entry
  per id of that batch.
- The worker exits when the pane (and with it the sender) is dropped, after the queued writes.
- A failed spawn is logged and reported; the next change tries to spawn again.
*/

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

use ms_input::{OverrideEntry, PersistError, write_override};
use ms_thread as thread;

use crate::runtime_log;

/// Why a write did not land (the pane words it; the technical detail is in the log).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteFailureKind {
    /// The stored `Hotkeys` section has a newer format; it was left untouched.
    NewerFormat,
    /// The document could not be read, was malformed or could not be written.
    Io,
    /// The worker thread could not be started.
    Spawn,
}

/// One failed write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WriteFailure {
    pub(super) action: ms_input::ActionId,
    pub(super) kind: WriteFailureKind,
}

#[derive(Debug)]
struct Worker {
    entries: Sender<OverrideEntry>,
    failures: Receiver<WriteFailure>,
}

/// The pane-side handle of the override writer.
#[derive(Debug)]
pub(super) struct OverrideWriter {
    path: PathBuf,
    worker: Option<Worker>,
    /// Failures detected on the GUI side (spawn, dead worker) waiting for `drain_failures`.
    local_failures: Vec<WriteFailure>,
}

impl OverrideWriter {
    /// A writer for the `user_config` document at `path` (no thread until the first change).
    pub(super) fn new(path: PathBuf) -> Self {
        Self { path, worker: None, local_failures: Vec::new() }
    }

    /// Queues `entry` for writing; `ctx` is repainted when a failure is reported. Never blocks.
    pub(super) fn submit(&mut self, ctx: &egui::Context, entry: OverrideEntry) {
        let action = entry.id;
        if self.worker.is_none() {
            self.worker = self.spawn(ctx);
        }
        let Some(worker) = &self.worker else {
            self.local_failures.push(WriteFailure { action, kind: WriteFailureKind::Spawn });
            return;
        };
        if let Err(mpsc::SendError(entry)) = worker.entries.send(entry) {
            // The worker ended (it only does so after a panic, since this side holds the
            // sender): report this change and start a fresh worker on the next one.
            runtime_log::log_warn(format!("[settings::hotkeys] override writer is gone; change not saved; action={}", entry.id.0));
            self.worker = None;
            self.local_failures.push(WriteFailure { action, kind: WriteFailureKind::Io });
        }
    }

    /// Every failure reported since the last call, oldest first.
    pub(super) fn drain_failures(&mut self) -> Vec<WriteFailure> {
        let mut failures = std::mem::take(&mut self.local_failures);
        if let Some(worker) = &self.worker {
            failures.extend(worker.failures.try_iter());
        }
        failures
    }

    fn spawn(&self, ctx: &egui::Context) -> Option<Worker> {
        let (entries_tx, entries_rx) = mpsc::channel::<OverrideEntry>();
        let (failures_tx, failures_rx) = mpsc::channel::<WriteFailure>();
        let path = self.path.clone();
        let ctx = ctx.clone();
        let spawned = thread::Builder::new().name("settings-hotkeys-writer".to_owned()).spawn(move || run_worker(&path, &entries_rx, &failures_tx, &ctx));
        match spawned {
            // Detached on purpose: the worker ends by itself once the pane drops the sender, and
            // nothing ever waits for it (a join on the GUI thread would block).
            Ok(_detached) => Some(Worker { entries: entries_tx, failures: failures_rx }),
            Err(err) => {
                runtime_log::log_error(format!("[settings::hotkeys] failed to start the hotkey override writer; path={}; cause={err}", self.path.display()));
                None
            }
        }
    }
}

/// The worker loop: blocks for the next entry, drains the queue, writes the latest entry per id
/// in first-queued order, reports failures. Ends when the pane drops its sender.
fn run_worker(path: &std::path::Path, entries: &Receiver<OverrideEntry>, failures: &Sender<WriteFailure>, ctx: &egui::Context) {
    while let Ok(first) = entries.recv() {
        let mut batch: Vec<OverrideEntry> = vec![first];
        for entry in entries.try_iter() {
            // Latest wins per id; keep the id's first position so independent ids stay ordered.
            if let Some(slot) = batch.iter_mut().find(|queued| queued.id == entry.id) {
                *slot = entry;
            } else {
                batch.push(entry);
            }
        }
        for entry in &batch {
            let Err(err) = write_override(path, entry) else { continue };
            let kind = match &err {
                PersistError::NewerFormat { .. } => WriteFailureKind::NewerFormat,
                PersistError::UserConfig { .. } => WriteFailureKind::Io,
            };
            runtime_log::log_warn(format!(
                "[settings::hotkeys] failed to save hotkey override; action={}; path={}; cause={err}; possible cause: user config unreadable, malformed or not writable",
                entry.id.0,
                path.display()
            ));
            if failures.send(WriteFailure { action: entry.id, kind }).is_err() {
                runtime_log::log_info(format!("[settings::hotkeys] settings pane closed before a save failure could be shown; action={}", entry.id.0));
            }
            ctx.request_repaint();
        }
    }
}
