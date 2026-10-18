/*
File: cleaning/tools/ai_editor/engines/flux2_klein/session.rs

Purpose:
The per-session run state of the FLUX.2 klein engine: the channel a worker answers on
and the file-dialog requests the panel opens.

Main responsibilities:
- own `Flux2SessionState` — the run receiver and the cancelled workers still ending;
- poll the worker and report what arrived (`Flux2RunPoll`; a finished run is the RAW answer
  plus its run facts, `RawRegionResult`, which the host post-processes);
- write the one flag the backend reports back into the settings (`apply_backend_flags`:
  `vae_tiling` only — the memory steps are the user's and are never rewritten);
- build the file-dialog request of each picker purpose (`flux2_picker_request`); the engine
  opens it through the non-blocking `ms_platform::file_dialog::request`.

Key structures:
- `Flux2SessionState`, `Flux2RunPoll`, `Flux2PickerPurpose`

Key functions:
- `apply_backend_flags()`, `flux2_picker_request()`

Notes:
The painted mask is NOT stored here — it belongs to the host's `MaskStack` and arrives
in `EngineRunRequest::masks`; the working mode is decided by the shared `mask_for_run`
(`../shared/region_mask.rs`), because the backend refuses `whole_region = true` unless the
mask really is uniformly 255. The model-path pickers ask for a directory PATH used in place (`PickDirectoryPath`) or
an existing file; a platform without them answers the request with a typed error.
*/

use super::*;
use ms_platform::file_dialog::{DialogKind, FileDialogRequest};

// ---------------------------------------------------------------------------------------
// Per-session state
// ---------------------------------------------------------------------------------------

/// What one poll of the run channel found.
///
/// `Done` and `Failed` are TERMINAL and are produced exactly once: the receiver is dropped
/// before either is returned, so the next poll answers `Idle`.
pub(super) enum Flux2RunPoll {
    /// No run has been started, or the last one has already been reported.
    Idle,
    /// A run is still in flight.
    Running,
    /// The run finished: the RAW image, exactly the region the run started from, with the
    /// mask it ran under and the latent mask as the colour-match sample mask.
    Done(RawRegionResult),
    /// The run failed, with an already-localized message.
    Failed(String),
}

/// State of the engine's runs: the channel the worker answers on.
///
/// The painted MASK is deliberately not here and must not come back: the host's `MaskStack`
/// owns it and hands its bytes over in `EngineRunRequest::masks`. Neither is any region image:
/// the pending result, and what Отменить restores, are the host's.
#[derive(Default)]
pub(super) struct Flux2SessionState {
    pub(super) run_rx: Option<Receiver<Flux2JobResult>>,
    /// Cancelled operations (runs, downloads) whose workers have not ended yet; while one is
    /// running nothing new starts on the backend's pipeline (`Flux2KleinEngine::pipeline_busy`).
    pub(super) abandoned: AbandonedJobs,
}

impl Flux2SessionState {
    /// Starts a run on a worker thread. A second run is refused while one is in flight.
    ///
    /// `input.mask` is the run's L8 mask (dilated into the wire's latent mask on the worker) — the host's painted
    /// layer, or the solid one built for the whole-region mode — and `input.mask_size` is
    /// the region size it, `input.region` and the optional `input.reference` all describe.
    /// `input.whole_region` is the flag [`mask_for_run`] derived alongside that buffer and
    /// must not be re-derived here: the two have to agree, or the backend refuses the pair.
    /// The sizes are validated by the caller before it gets here, and again on the worker.
    ///
    /// # Errors
    /// Returns the localized "already running" message when a run is still in flight.
    pub(super) fn start_run(
        &mut self,
        input: Flux2RunInput,
        settings: &Flux2KleinSettings,
        progress: &Arc<Mutex<EngineProgress>>,
    ) -> Result<(), String> {
        if self.run_rx.is_some() {
            return Err(t!("cleaning.mask_editor.processing_already_running_status").to_string());
        }
        let settings = settings.normalized();
        // Claimed here rather than on the worker: the claim then happens in the order
        // the user pressed the button, whatever order the threads start in.
        let claim = begin_progress_generation(progress);
        let progress = Arc::clone(progress);
        let (tx, rx) = mpsc::channel::<Flux2JobResult>();
        thread::spawn(move || {
            let result = run_flux2_klein(&input, &settings, &progress, &claim);
            // A failed send means the receiver is gone: the run was cancelled (its receiver
            // sits in `abandoned`, which only waits for the disconnect) or the engine was
            // dropped. Either way nobody wants the answer, so there is nothing to report.
            let _ = tx.send(Flux2JobResult { mask: input.mask, whole_region: input.whole_region, result });
        });
        self.run_rx = Some(rx);
        Ok(())
    }

    /// Abandons the run in flight: its answer is discarded, its progress generation is
    /// retired so it can neither move nor stop the bar of whatever runs next, and the
    /// backend is told to stop instead of finishing a generation nobody will see.
    /// Returns `true` when there was a run to abandon.
    ///
    /// The receiver moves into `abandoned` first, so a result that lands meanwhile is never
    /// reported as a finished run; the engine stays busy until that worker ends.
    ///
    /// A cancel in the window before the request reaches the wire (no id yet) is STICKY: the
    /// claim's `CallControl` sends the backend cancel the moment the worker publishes the id.
    pub(super) fn cancel_run(&mut self, progress: &Arc<Mutex<EngineProgress>>) -> bool {
        let Some(rx) = self.run_rx.take() else {
            return false;
        };
        // The answer is never read, but the worker is waited for: the backend stops at its next
        // check, and a run started meanwhile would queue behind this one.
        self.abandoned.push(rx);
        retire_progress_generation(progress, FLUX2_LOG_LABEL);
        true
    }

    /// Polls the run channel and reports what it found, writing the user-facing line for
    /// the engine's own status slot into `status`.
    ///
    /// A finished run also writes the `vae_tiling` flag the backend actually used back
    /// into `settings` (setting `settings_changed`, which the caller turns into a
    /// background save), so a decode that had to be retried tiled starts tiled next time.
    /// A DEGRADED run is reported with the step pair it actually ran as; the user's own
    /// steps stay as they are (the backend learned a floor, which the panel shows).
    pub(super) fn poll_run(
        &mut self,
        settings: &mut Flux2KleinSettings,
        settings_changed: &mut bool,
        status: &mut Option<String>,
    ) -> Flux2RunPoll {
        let Some(rx) = self.run_rx.as_ref() else {
            return Flux2RunPoll::Idle;
        };
        match rx.try_recv() {
            Ok(job) => {
                self.run_rx = None;
                match job.result {
                    Ok(outcome) => {
                        if let Some(applied) = outcome.applied {
                            *settings_changed |= apply_backend_flags(settings, applied);
                        }
                        *status = Some(flux2_run_done_status(outcome.degraded.as_ref(), outcome.oom_recovered));
                        Flux2RunPoll::Done(RawRegionResult {
                            image: outcome.image,
                            mask: job.mask,
                            sample_mask: Some(outcome.latent_mask),
                            whole_region: job.whole_region,
                        })
                    }
                    Err(err) => {
                        let message = tf!("cleaning.mask_editor.processing_error", err = err);
                        *status = Some(message.clone());
                        Flux2RunPoll::Failed(message)
                    }
                }
            }
            Err(TryRecvError::Empty) => Flux2RunPoll::Running,
            Err(TryRecvError::Disconnected) => {
                self.run_rx = None;
                let message =
                    t!("cleaning.mask_editor.processing_thread_crashed_error").to_string();
                *status = Some(message.clone());
                Flux2RunPoll::Failed(message)
            }
        }
    }
}

/// Copies the `vae_tiling` flag the backend actually used into `settings`. Returns `true`
/// when it changed, i.e. when a background save is owed. Nothing else is written back: the
/// memory steps are the user's (the backend learns a floor instead) and fp8 is a quality
/// trade only the user makes.
pub(super) fn apply_backend_flags(settings: &mut Flux2KleinSettings, applied: Flux2AppliedFlags) -> bool {
    match applied.vae_tiling {
        Some(tiling) if tiling != settings.vae_tiling => {
            settings.vae_tiling = tiling;
            true
        }
        Some(_) | None => false,
    }
}

/// The engine's line about a finished run: a degraded run names the steps it ran as (that
/// is the one thing no other surface says), a recovered decode says so, else plain "done".
#[must_use]
pub(super) fn flux2_run_done_status(degraded: Option<&RunDegraded>, oom_recovered: bool) -> String {
    if let Some(degraded) = degraded {
        let vram = degraded.vram.map_or_else(|| t!("large_models.step.unknown_label").to_string(), |step| step.label().to_string());
        let ram = degraded.ram.map_or_else(|| t!("large_models.step.unknown_label").to_string(), |step| step.label().to_string());
        return tf!("cleaning.tools.flux2_klein.degraded_status", vram = vram, ram = ram);
    }
    if oom_recovered {
        return t!("cleaning.tools.flux2_klein.oom_recovered_status").to_string();
    }
    t!("cleaning.mask_editor.processing_done_status").to_string()
}

// ---------------------------------------------------------------------------------------
// File pickers
// ---------------------------------------------------------------------------------------

/// What a running native file dialog is picking a path for.
///
/// Five of the variants fill in a model path; the last two carry a prompt-cache entry in
/// or out of the library and do NOT touch the settings — they start an IPC call instead
/// (see [`Flux2KleinEngine::poll_picker`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flux2PickerPurpose {
    TextEncoderDir,
    TransformerFile,
    TransformerDir,
    VaeFile,
    VaeDir,
    /// Destination of `inpaint.flux2_klein.prompt_cache.export`.
    PromptCacheExport,
    /// Source of `inpaint.flux2_klein.prompt_cache.import`.
    PromptCacheImport,
}

/// The file-dialog request of `purpose`.
///
/// The model directories are used IN PLACE by the backend, so they are `PickDirectoryPath`
/// requests, never copied-in folders; the prompt-cache export is a `SaveFile` whose target the
/// engine commits once the backend has written it.
pub(super) fn flux2_picker_request(purpose: Flux2PickerPurpose) -> FileDialogRequest {
    match purpose {
        Flux2PickerPurpose::TextEncoderDir | Flux2PickerPurpose::TransformerDir | Flux2PickerPurpose::VaeDir => {
            FileDialogRequest::new(DialogKind::PickDirectoryPath)
        }
        Flux2PickerPurpose::TransformerFile | Flux2PickerPurpose::VaeFile => FileDialogRequest::new(DialogKind::OpenFile)
            .filter(t!("cleaning.tools.flux2_klein.weights_files_filter"), &["safetensors", "sft", "gguf"]),
        Flux2PickerPurpose::PromptCacheExport => FileDialogRequest::new(DialogKind::SaveFile)
            .title(t!("cleaning.tools.flux2_klein.prompt_cache_export_dialog_title"))
            .filter(t!("cleaning.tools.flux2_klein.prompt_cache_files_filter"), &[FLUX2_PROMPT_CACHE_EXTENSION])
            // The extension is part of the file's identity, so the suggested name already
            // carries it; the stem is a plain identifier and stays literal.
            .suggested_name(format!("prompt.{FLUX2_PROMPT_CACHE_EXTENSION}")),
        Flux2PickerPurpose::PromptCacheImport => FileDialogRequest::new(DialogKind::OpenFile)
            .title(t!("cleaning.tools.flux2_klein.prompt_cache_import_dialog_title"))
            .filter(t!("cleaning.tools.flux2_klein.prompt_cache_files_filter"), &[FLUX2_PROMPT_CACHE_EXTENSION]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boundary of the derived rule. One pixel is a mask — the threshold is "any byte
    /// above zero" and nothing softer, because a stray dot the user has not noticed must
    /// not silently become permission to regenerate the entire region.
    #[test]
    fn a_single_painted_pixel_is_already_a_mask() {
        for idx in [0usize, 5, 32 * 16 - 1] {
            let mut painted = vec![0u8; 32 * 16];
            painted[idx] = 255;
            let (sent, whole_region) = mask_for_run(&painted);
            assert!(!whole_region, "one pixel at {idx} is a mask, not an empty layer");
            assert_eq!(sent, painted, "and it travels exactly as painted");
        }
        // The other side of the same boundary: a layer that is entirely 255 is a painted
        // mask covering everything, so it is sent as one rather than re-derived as the
        // whole-region mode. The bytes on the wire are identical either way.
        let solid = vec![255u8; 32 * 16];
        assert_eq!(mask_for_run(&solid), (solid.clone(), false));
    }

    /// The whole rule, in one place: nothing painted is the whole-region mode and yields
    /// the solid mask the backend demands alongside the flag; anything painted is the
    /// masked mode and travels verbatim.
    #[test]
    fn an_empty_mask_becomes_a_solid_one_and_a_painted_one_travels_verbatim() {
        let empty = vec![0u8; 32 * 16];
        let (sent, whole_region) = mask_for_run(&empty);
        assert!(whole_region, "nothing painted means the whole region may change");
        // The bytes that actually reach the wire are checked, not the intention: the
        // buffer is encoded and decoded back exactly as `run_flux2_klein_pass` does it.
        let png = encode_mask_png_l8(&sent, 32, 16).expect("encode the solid mask");
        let decoded = image::load_from_memory(&png).expect("decode").to_luma8();
        assert_eq!(decoded.dimensions(), (32, 16));
        assert!(
            decoded.pixels().all(|pixel| pixel.0[0] == 255),
            "the backend refuses whole_region unless every mask byte is 255"
        );
        assert_eq!(empty, vec![0u8; 32 * 16], "the host's layer is never written through");

        // A real stroke: the painted layer decides, and it reaches the wire unchanged.
        let mut painted = vec![0u8; 32 * 16];
        for x in 2..14 {
            painted[4 * 32 + x] = 255;
        }
        let painted_px = painted.iter().filter(|value| **value > 0).count();
        assert!(painted_px > 0 && painted_px < painted.len(), "a real stroke");
        assert_eq!(mask_for_run(&painted), (painted.clone(), false));
    }

    /// Directories are kept as paths, weights and caches are files, the export is a save.
    #[test]
    fn picker_requests_match_their_purpose() {
        let cases = [
            (Flux2PickerPurpose::TextEncoderDir, DialogKind::PickDirectoryPath),
            (Flux2PickerPurpose::TransformerDir, DialogKind::PickDirectoryPath),
            (Flux2PickerPurpose::VaeDir, DialogKind::PickDirectoryPath),
            (Flux2PickerPurpose::TransformerFile, DialogKind::OpenFile),
            (Flux2PickerPurpose::VaeFile, DialogKind::OpenFile),
            (Flux2PickerPurpose::PromptCacheExport, DialogKind::SaveFile),
            (Flux2PickerPurpose::PromptCacheImport, DialogKind::OpenFile),
        ];
        for (purpose, kind) in cases {
            assert_eq!(flux2_picker_request(purpose).kind(), kind, "{purpose:?}");
        }
    }
}
