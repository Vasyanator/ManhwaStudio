/*
File: ai_editor/engines/run_job.rs

Purpose:
The run in flight of an engine whose run may DOWNLOAD an app-managed model before it calls the
backend (`lama`, and `sdxl` through its LaMa prefill checkpoint): the worker thread, its
message channel and its cancel flag, in one owner.

Key items:
- `EngineRunMessage`: what the worker sends — model-download events, then the answer.
- `EngineRunJob`: spawns the worker (`spawn`), drains it (`try_finished`, keeping the latest
  download event for `download`), and on DROP sets the cancel flag the worker's model download
  polls.

Notes:
Dropping the job IS the cancel: `AiEngine::cancel` takes it out of its slot and the engine's
own drop (host teardown) drops it, so a download stops instead of finishing for nobody. The
backend call that follows cannot be stopped from here; its answer is discarded on arrival.
The engines draw `download` in their `AiEngine::draw_progress` through the shared
`ms_tab_translation::model_download_status` widget; the host's Cancel drops the job.
*/

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use eframe::egui;
use ms_sysprobe::ai_models::{EnsureCtx, ModelDownloadEvent};
use ms_tab_translation::model_download_status::ModelDownloadStatus;
use ms_thread as thread;

/// One message of an engine run's worker.
#[derive(Debug)]
pub(super) enum EngineRunMessage {
    /// A (thinned) model-download event of the run; retries are progress, not failures.
    ModelDownload(ModelDownloadEvent),
    /// The run's answer; the last message of a job.
    Finished(Result<egui::ColorImage, String>),
}

/// An engine run in flight: its channel, its cancel flag (set on drop) and the latest
/// download event drained from it.
#[derive(Debug)]
pub(super) struct EngineRunJob {
    rx: Receiver<EngineRunMessage>,
    cancel: Arc<AtomicBool>,
    download: ModelDownloadStatus,
}

impl EngineRunJob {
    /// Runs `work` on a worker thread with a download context whose cancel flag this job owns
    /// and whose events go into the job's channel.
    pub(super) fn spawn<W>(work: W) -> Self
    where
        W: FnOnce(&mut EnsureCtx<'_>) -> Result<egui::ColorImage, String> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        thread::spawn(move || {
            let events = tx.clone();
            let mut forward = |event: &ModelDownloadEvent| {
                // A gone receiver means the job was dropped, which also set the cancel flag.
                let _ = events.send(EngineRunMessage::ModelDownload(event.clone()));
            };
            let result = work(&mut EnsureCtx { cancel: &worker_cancel, progress: &mut forward });
            // The receiver is gone when the engine abandoned the run; nothing to report.
            let _ = tx.send(EngineRunMessage::Finished(result));
        });
        Self { rx, cancel, download: ModelDownloadStatus::default() }
    }

    /// Drains download events (the latest is kept for [`Self::download`]) and returns the
    /// answer once it arrived; `Empty` while the run is still going, `Disconnected` when the
    /// worker died without an answer.
    pub(super) fn try_finished(&mut self) -> Result<Result<egui::ColorImage, String>, TryRecvError> {
        loop {
            match self.rx.try_recv()? {
                EngineRunMessage::ModelDownload(event) => self.download.record(event),
                EngineRunMessage::Finished(result) => return Ok(result),
            }
        }
    }

    /// The model download the run is performing, if any (for the engine's `draw_progress`).
    pub(super) fn download(&self) -> Option<&ModelDownloadEvent> {
        self.download.current()
    }

    /// A job driven by the test through the returned sender (no worker thread), plus its
    /// cancel flag so the test can observe the drop.
    #[cfg(test)]
    pub(super) fn for_test() -> (Self, mpsc::Sender<EngineRunMessage>, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        (Self { rx, cancel: Arc::clone(&cancel), download: ModelDownloadStatus::default() }, tx, cancel)
    }
}

impl Drop for EngineRunJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Download events are drained without ending the run and the latest is kept; the answer
    /// ends it; dropping the job raises the cancel flag a running download polls.
    #[test]
    fn events_are_drained_the_answer_ends_the_run_and_drop_cancels() {
        let (mut job, tx, cancel) = EngineRunJob::for_test();
        let event = ModelDownloadEvent {
            file: "Torch/LaMa/config.yaml".to_string(),
            file_index: 0,
            file_count: 2,
            phase: ms_sysprobe::ai_models::ModelDownloadPhase::Downloading,
            staged: 1,
            total: None,
        };
        tx.send(EngineRunMessage::ModelDownload(event.clone())).expect("the receiver is alive");
        assert!(matches!(job.try_finished(), Err(TryRecvError::Empty)));
        assert_eq!(job.download(), Some(&event), "the drained event is kept for the progress");
        tx.send(EngineRunMessage::Finished(Err("boom".to_string()))).expect("the receiver is alive");
        assert!(matches!(job.try_finished(), Ok(Err(text)) if text == "boom"));
        assert!(!cancel.load(Ordering::Relaxed));
        drop(job);
        assert!(cancel.load(Ordering::Relaxed), "dropping the job must cancel its download");
    }

    /// A real worker: `spawn` hands `work` a context carrying the job's flag.
    #[test]
    fn spawn_gives_the_work_the_jobs_cancel_flag() {
        let mut job = EngineRunJob::spawn(|ctx| {
            if ctx.cancel.load(Ordering::Relaxed) {
                Err("cancelled before start".to_string())
            } else {
                Ok(egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE]))
            }
        });
        let answer = loop {
            match job.try_finished() {
                Ok(answer) => break answer,
                Err(TryRecvError::Empty) => std::thread::yield_now(),
                Err(TryRecvError::Disconnected) => panic!("the worker died"),
            }
        };
        assert_eq!(answer.map(|image| image.size), Ok([1, 1]));
    }
}
