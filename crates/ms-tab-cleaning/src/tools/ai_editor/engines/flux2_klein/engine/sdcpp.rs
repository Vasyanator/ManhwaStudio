/*
File: cleaning/tools/ai_editor/engines/flux2_klein/engine/sdcpp.rs

Purpose:
The Stable-Diffusion.cpp half of the FLUX.2 klein engine's state: the sd.cpp status worker
(which three files sd.cpp would load and the saved sd.cpp device, both read off the GUI thread)
and the run in flight, in its two stages — preparing the request on a worker (validation, the
latent-mask dilation, the RGB copies), then the shared `SdCppRunJob`.

Key items:
- `Flux2SdcppState`: `poll_resolve`, `files_for`, `device`, `start`, `poll_run`, `cancel`,
  `busy`.
- `Flux2SdcppRun`: `Preparing` / `Running`.

Notes:
The progress generation is claimed when the job starts (on the GUI thread, in `poll_run`), so
the bar appears once the request is ready; a cancel while preparing parks the preparing worker
in `abandoned` and claims nothing. The answer is the host's `RawRegionResult`, exactly like the
PyTorch run's: the run's mask, the latent mask as the colour-match sample, the working mode.
*/

use super::*;
use ms_config::SdCppDeviceChoice;
use ms_sdcpp::ModelFiles;

/// What one sd.cpp status-worker run found for `key`.
pub(in crate::tools::ai_editor::engines::flux2_klein) struct Flux2SdcppResolved {
    key: Flux2SdcppFilesKey,
    files: Result<ModelFiles, Flux2SdcppFilesError>,
    device: SdCppDeviceChoice,
}

/// The host's raw result of a finished sd.cpp run, paired exactly like the PyTorch answer
/// (`Flux2SessionState::poll_run`): the decode, the run's mask, the latent mask as the
/// colour-match sample, the working mode.
///
/// # Errors
/// The run's own localized error.
pub(super) fn flux2_sdcpp_raw_result(answer: Result<egui::ColorImage, String>, mask: Vec<u8>, latent_mask: Vec<u8>, whole_region: bool) -> Result<RawRegionResult, String> {
    Ok(RawRegionResult { image: answer?, mask, sample_mask: Some(latent_mask), whole_region })
}

/// The two stages of an sd.cpp run.
pub(in crate::tools::ai_editor::engines::flux2_klein) enum Flux2SdcppRun {
    /// The request is being built on a worker (validation, latent mask, RGB copies).
    Preparing(Receiver<Result<(Flux2SdcppPrepared, ModelFiles), String>>),
    /// The shared job runs; the run facts wait for its answer.
    Running { job: SdCppRunJob, mask: Vec<u8>, latent_mask: Vec<u8>, whole_region: bool, seed: i64 },
}

/// The sd.cpp state of one engine.
#[derive(Default)]
pub(in crate::tools::ai_editor::engines::flux2_klein) struct Flux2SdcppState {
    /// The last status-worker answer (for whatever key it was asked about).
    resolved: Option<Flux2SdcppResolved>,
    resolve_rx: Option<Receiver<Flux2SdcppResolved>>,
    /// Ask again even when the key did not move (files may have appeared: a finished catalog
    /// job, the refresh button).
    resolve_wanted: bool,
    /// The `ms_sdcpp` status revision the device was last read under; a change re-reads it.
    seen_status_revision: Option<u64>,
    pub(super) run: Option<Flux2SdcppRun>,
    /// Cancelled runs whose workers have not ended (the service runs one generation at a time).
    pub(super) abandoned: AbandonedJobs,
}

impl Flux2SdcppState {
    /// Re-asks the status worker on the next `poll_resolve` even for an unchanged key.
    pub(super) fn want_resolve(&mut self) {
        self.resolve_wanted = true;
    }

    /// Drains the status worker and starts the next question when the settings' key moved, the
    /// sd.cpp status changed (the device may have been re-chosen) or a refresh was asked. One
    /// worker at a time; its answer is used only while its key is the current one.
    pub(super) fn poll_resolve(&mut self, settings: &Flux2KleinSettings, status_revision: u64) {
        if let Some(rx) = self.resolve_rx.as_ref() {
            match rx.try_recv() {
                Ok(resolved) => {
                    self.resolve_rx = None;
                    self.resolved = Some(resolved);
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.resolve_rx = None;
                    ms_log::runtime_log::log_warn(format!("[cleaning] {FLUX2_LOG_LABEL}: the sd.cpp file resolution ended without an answer; asking again"));
                    self.resolve_wanted = true;
                }
            }
        }
        let key = Flux2SdcppFilesKey::of(settings);
        let stale = self.resolved.as_ref().is_none_or(|resolved| resolved.key != key);
        let revision_moved = self.seen_status_revision != Some(status_revision);
        if !(stale || revision_moved || self.resolve_wanted) {
            return;
        }
        self.resolve_wanted = false;
        self.seen_status_revision = Some(status_revision);
        let settings = settings.clone();
        let root = config::side_models_dir();
        let (tx, rx) = mpsc::channel();
        self.resolve_rx = Some(rx);
        // Stats the model files and reads `user_config`: never on the GUI thread.
        thread::spawn(move || {
            let resolved = Flux2SdcppResolved { files: flux2_sdcpp_model_files(&settings, &root), device: read_sdcpp_choice().device, key };
            // A closed channel means the engine was dropped meanwhile: nobody reads it.
            let _ = tx.send(resolved);
        });
    }

    /// The resolved files of `settings`, `None` while the answer for its current key is not in.
    #[must_use]
    pub(super) fn files_for(&self, settings: &Flux2KleinSettings) -> Option<&Result<ModelFiles, Flux2SdcppFilesError>> {
        let key = Flux2SdcppFilesKey::of(settings);
        self.resolved.as_ref().filter(|resolved| resolved.key == key).map(|resolved| &resolved.files)
    }

    /// The saved sd.cpp device as last read, `None` before the first answer.
    #[must_use]
    pub(super) fn device(&self) -> Option<&SdCppDeviceChoice> {
        self.resolved.as_ref().map(|resolved| &resolved.device)
    }

    /// Whether an sd.cpp run is in flight (either stage) or a cancelled one still ends.
    #[must_use]
    pub(super) fn busy(&self) -> bool {
        self.run.is_some() || self.abandoned.busy()
    }

    /// Starts preparing a run on a worker with the NORMALIZED `settings` and the resolved
    /// `files`.
    ///
    /// # Errors
    /// The localized "already running" message while a run is in flight.
    pub(super) fn start(&mut self, input: Flux2RunInput, settings: Flux2KleinSettings, files: ModelFiles) -> Result<(), String> {
        if self.run.is_some() {
            return Err(t!("cleaning.mask_editor.processing_already_running_status").to_string());
        }
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let prepared = flux2_sdcpp_prepare(input, &settings).map(|prepared| (prepared, files));
            // The receiver is gone when the run was cancelled while preparing: nobody wants it.
            let _ = tx.send(prepared);
        });
        self.run = Some(Flux2SdcppRun::Preparing(rx));
        Ok(())
    }

    /// Advances the run: a prepared request starts the shared job (claiming `progress`), and
    /// the job's answer becomes the host's raw result. `options` are the engine's sd.cpp
    /// options at this moment; `status` receives the engine's line about a finished run.
    pub(super) fn poll_run(&mut self, options: &SdCppRunOptions, progress: &Arc<Mutex<EngineProgress>>, status: &mut Option<String>) -> Flux2RunPoll {
        let Some(run) = self.run.take() else {
            return Flux2RunPoll::Idle;
        };
        let outcome = match run {
            Flux2SdcppRun::Preparing(rx) => match rx.try_recv() {
                Ok(Ok((prepared, files))) => {
                    let Flux2SdcppPrepared { request, mask, latent_mask, whole_region } = prepared;
                    let seed = request.seed;
                    let job = SdCppRunJob::start(SdCppRunInput { files, request, options: options.clone() }, progress, FLUX2_LOG_LABEL);
                    self.run = Some(Flux2SdcppRun::Running { job, mask, latent_mask, whole_region, seed });
                    return Flux2RunPoll::Running;
                }
                Ok(Err(err)) => Err(err),
                Err(TryRecvError::Empty) => {
                    self.run = Some(Flux2SdcppRun::Preparing(rx));
                    return Flux2RunPoll::Running;
                }
                Err(TryRecvError::Disconnected) => Err(t!("cleaning.mask_editor.processing_thread_crashed_error").to_string()),
            },
            Flux2SdcppRun::Running { mut job, mask, latent_mask, whole_region, seed } => match job.poll() {
                None => {
                    self.run = Some(Flux2SdcppRun::Running { job, mask, latent_mask, whole_region, seed });
                    return Flux2RunPoll::Running;
                }
                Some(answer) => flux2_sdcpp_raw_result(answer, mask, latent_mask, whole_region).map(|raw| (raw, seed)),
            },
        };
        match outcome {
            Ok((raw, seed)) => {
                // Names the seed actually used (a blank seed was drawn in Rust), so the user can
                // pin it to repeat the run.
                *status = Some(tf!("cleaning.tools.flux2_klein.sdcpp_run_done_status", seed = seed));
                Flux2RunPoll::Done(raw)
            }
            Err(err) => {
                let message = tf!("cleaning.mask_editor.processing_error", err = err);
                *status = Some(message.clone());
                Flux2RunPoll::Failed(message)
            }
        }
    }

    /// Abandons the run in flight: a preparing worker is parked, a running job is cancelled
    /// (flag raised, bar retired, receiver parked). Returns whether there was a run.
    pub(super) fn cancel(&mut self) -> bool {
        match self.run.take() {
            None => false,
            Some(Flux2SdcppRun::Preparing(rx)) => {
                self.abandoned.push(rx);
                true
            }
            Some(Flux2SdcppRun::Running { job, .. }) => {
                job.cancel(&mut self.abandoned, FLUX2_LOG_LABEL);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A finished sd.cpp run answers the host exactly as a PyTorch run does: the raw image, the
    /// run's mask, the latent mask as `sample_mask`, the mode; an error stays the error.
    #[test]
    fn a_finished_run_pairs_like_the_pytorch_answer() {
        let image = egui::ColorImage::filled([2, 1], egui::Color32::WHITE);
        let raw = flux2_sdcpp_raw_result(Ok(image), vec![255, 0], vec![255, 255], false).expect("paired");
        assert_eq!(raw.image.size, [2, 1]);
        assert_eq!(raw.mask, vec![255, 0]);
        assert_eq!(raw.sample_mask, Some(vec![255, 255]));
        assert!(!raw.whole_region);
        assert_eq!(flux2_sdcpp_raw_result(Err("boom".to_string()), Vec::new(), Vec::new(), true).err(), Some("boom".to_string()));
    }

    /// Cancelling while the request is being prepared parks the preparing worker: the state
    /// stays busy until it ends, and a run that never started reports nothing.
    #[test]
    fn a_cancel_while_preparing_parks_the_worker() {
        let mut state = Flux2SdcppState::default();
        assert!(!state.cancel(), "nothing to cancel");
        let (tx, rx) = mpsc::channel();
        state.run = Some(Flux2SdcppRun::Preparing(rx));
        assert!(state.busy());
        assert!(state.cancel());
        assert!(state.run.is_none());
        assert!(state.busy(), "the parked worker keeps it busy");
        drop(tx);
        assert!(!state.abandoned.poll(), "a dropped sender settles it");
        assert!(!state.busy());
        let mut status = None;
        let progress = Arc::new(Mutex::new(EngineProgress::default()));
        assert!(matches!(state.poll_run(&SdCppRunOptions::default(), &progress, &mut status), Flux2RunPoll::Idle));
    }

    /// A preparing worker that fails reports the localized processing error and frees the state.
    #[test]
    fn a_failed_preparation_is_reported() {
        let mut state = Flux2SdcppState::default();
        let (tx, rx) = mpsc::channel();
        state.run = Some(Flux2SdcppRun::Preparing(rx));
        tx.send(Err("bad region".to_string())).expect("receiver alive");
        let mut status = None;
        let progress = Arc::new(Mutex::new(EngineProgress::default()));
        assert!(matches!(state.poll_run(&SdCppRunOptions::default(), &progress, &mut status), Flux2RunPoll::Failed(_)));
        assert!(status.is_some());
        assert!(!state.busy());
    }
}
