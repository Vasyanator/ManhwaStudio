/*
File: cleaning/tools/ai_editor/engines/flux2_klein/session.rs

Purpose:
The per-session run state of the FLUX.2 klein engine: the channel a worker answers on,
the per-RUN undo stack, the derivation of the mask that actually goes on the wire, and
the file pickers the panel opens.

Main responsibilities:
- own `Flux2SessionState` — the run receiver, the undo history and their bounds;
- poll the worker and report what arrived (`Flux2RunPoll`);
- decide the working mode from the painted mask (`mask_for_run`): a painted buffer
  travels verbatim with `whole_region = false`, an empty one becomes a SOLID mask;
- write the flags the backend reports back into the settings (`apply_backend_flags`);
- open a native file/directory picker off the GUI thread (`spawn_flux2_picker`).

Key structures:
- `Flux2SessionState`, `Flux2RunPoll`, `Flux2PickerPurpose`

Key functions:
- `mask_for_run()`, `apply_backend_flags()`, `spawn_flux2_picker()`

Notes:
The painted mask is NOT stored here — it belongs to the host's `MaskStack` and arrives
in `EngineRunRequest::masks`. `mask_for_run` is the ONLY place the working mode is
decided; the backend refuses `whole_region = true` unless the mask really is uniformly
255. `spawn_flux2_picker` has two cfg variants: a real picker off-thread on desktop, and
an immediately-closed channel on wasm32.
*/

use super::*;

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
    /// The run finished; the image is exactly the region the run started from.
    Done(egui::ColorImage),
    /// The run failed, with an already-localized message.
    Failed(String),
}

/// State of the engine's runs: the channel the worker answers on and the region images
/// that preceded the runs already reported.
///
/// The painted MASK is deliberately not here and must not come back: the host's `MaskStack`
/// owns it and hands its bytes over in `EngineRunRequest::masks`.
#[derive(Default)]
pub(super) struct Flux2SessionState {
    /// Region images from before each finished run, most recent last, bounded by
    /// [`FLUX2_UNDO_LIMIT`].
    pub(super) undo_stack: Vec<egui::ColorImage>,
    pub(super) run_rx: Option<Receiver<Flux2JobResult>>,
}

impl Flux2SessionState {
    /// Drops the run history. The caller is responsible for the run in flight
    /// ([`Self::cancel_run`]) — dropping the receiver alone would leave the backend
    /// generating an image nothing can receive.
    pub(super) fn clear(&mut self) {
        self.undo_stack.clear();
    }

    /// Starts a run on a worker thread. A second run is refused while one is in flight.
    ///
    /// `mask` is the L8 buffer that actually goes on the wire — the host's painted layer,
    /// or the solid one built for the whole-region mode — and `mask_size` is the region size
    /// both it and `region` describe. `whole_region` is the flag [`mask_for_run`] derived
    /// alongside that buffer and must not be re-derived here: the two have to agree, or the
    /// backend refuses the pair. The sizes are validated by the caller before it gets here.
    ///
    /// # Errors
    /// Returns the localized "already running" message when a run is still in flight.
    pub(super) fn start_run(
        &mut self,
        region: egui::ColorImage,
        mask: Vec<u8>,
        whole_region: bool,
        mask_size: [usize; 2],
        settings: &Flux2KleinSettings,
        progress: &Arc<Mutex<Flux2Progress>>,
    ) -> Result<(), String> {
        if self.run_rx.is_some() {
            return Err(t!("cleaning.mask_editor.processing_already_running_status").to_string());
        }
        let settings = settings.normalized();
        // Claimed here rather than on the worker: the claim then happens in the order
        // the user pressed the button, whatever order the threads start in.
        let generation = begin_progress_generation(progress);
        let progress = Arc::clone(progress);
        let (tx, rx) = mpsc::channel::<Flux2JobResult>();
        thread::spawn(move || {
            let result = run_flux2_klein(
                &region,
                &mask,
                whole_region,
                mask_size,
                &settings,
                &progress,
                generation,
            );
            let _ = tx.send(Flux2JobResult {
                source: region,
                result,
            });
        });
        self.run_rx = Some(rx);
        Ok(())
    }

    /// Abandons the run in flight: its answer is discarded, its progress generation is
    /// retired so it can neither move nor stop the bar of whatever runs next, and the
    /// backend is told to stop instead of finishing a generation nobody will see.
    /// Returns `true` when there was a run to abandon.
    ///
    /// A no-op detail that matters: `run_rx` is dropped first, so a result that lands
    /// between the two statements is discarded rather than reported as a finished run.
    ///
    /// During the short window before the request reaches the wire (the region and mask
    /// are still being encoded) there is no id to cancel yet; the run is detached all
    /// the same and its answer is dropped, the backend simply finishes it.
    pub(super) fn cancel_run(&mut self, progress: &Arc<Mutex<Flux2Progress>>) -> bool {
        if self.run_rx.is_none() {
            return false;
        }
        self.run_rx = None;
        if let Some(id) = retire_progress_generation(progress) {
            spawn_flux2_cancel(id);
        }
        true
    }

    /// Polls the run channel and reports what it found, writing the user-facing line for
    /// the engine's own status slot into `status`.
    ///
    /// A finished run also writes the memory flags the backend actually used back into
    /// `settings` (setting `settings_changed`, which the caller turns into a background
    /// save), so a run that had to recover from an out-of-memory failure makes the next
    /// one take the cheap path from the start.
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
                        self.push_undo(job.source);
                        if let Some(applied) = outcome.applied {
                            *settings_changed |= apply_backend_flags(settings, applied);
                        }
                        *status = Some(if outcome.oom_recovered {
                            t!("cleaning.tools.flux2_klein.oom_recovered_status").to_string()
                        } else {
                            t!("cleaning.mask_editor.processing_done_status").to_string()
                        });
                        Flux2RunPoll::Done(outcome.image)
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

    /// Pushes one pre-run region image onto the undo stack, dropping the oldest entry
    /// once [`FLUX2_UNDO_LIMIT`] is reached.
    ///
    /// The bound is what keeps a long session from accumulating megabytes of history
    /// nobody will walk back to; the entries lost are always the oldest.
    pub(super) fn push_undo(&mut self, image: egui::ColorImage) {
        if self.undo_stack.len() >= FLUX2_UNDO_LIMIT {
            self.undo_stack.remove(0);
        }
        self.undo_stack.push(image);
    }
}

/// The L8 mask a run actually puts on the wire, region-sized either way, together with the
/// `whole_region` flag that must accompany it.
///
/// This is the ONE place the working mode is decided, and it is DERIVED, never chosen: an
/// empty `painted` layer means "edit the whole region", anything painted means "edit only
/// what is under it". A single painted pixel is already a mask — the threshold is "any byte
/// above zero" and nothing softer, so a stray dot cannot be mistaken for an empty layer.
///
/// `painted` is the host's mask layer, exactly `w * h` bytes. In the whole-region case the
/// model may change every pixel and the backend proves that by REQUIRING a solid mask
/// alongside the flag, so a fresh all-`255` buffer is built here rather than the host's
/// layer being overwritten — the host's `MaskStack` is never written to by the engine at
/// all, which is what keeps the user's painting theirs.
#[must_use]
pub(super) fn mask_for_run(painted: &[u8]) -> (Vec<u8>, bool) {
    if painted.iter().any(|value| *value > 0) {
        (painted.to_vec(), false)
    } else {
        (vec![255u8; painted.len()], true)
    }
}

/// Copies the flags the backend actually used into `settings`. Returns `true` when
/// anything changed, i.e. when a background save is owed.
///
/// The memory preset is deliberately NOT re-pinned here: if the recovered combination
/// matches no preset, the picker moves itself to «Пользовательский», which is the
/// honest report of what is now in effect.
pub(super) fn apply_backend_flags(settings: &mut Flux2KleinSettings, applied: Flux2AppliedFlags) -> bool {
    let mut changed = false;
    for (field, value) in [
        (
            &mut settings.unload_transformer_before_vae,
            applied.unload_transformer_before_vae,
        ),
        (&mut settings.vae_tiling, applied.vae_tiling),
        (&mut settings.vae_slicing, applied.vae_slicing),
        (
            &mut settings.unload_text_encoder_after_encode,
            applied.unload_text_encoder_after_encode,
        ),
        (&mut settings.text_encoder_fp8, applied.text_encoder_fp8),
    ] {
        if *field != value {
            *field = value;
            changed = true;
        }
    }
    changed
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

/// Spawns the blocking native file dialog for `purpose` on a worker thread.
///
/// `None` means the user cancelled. Never called on the GUI thread's own stack: `rfd`
/// blocks until the dialog closes.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn spawn_flux2_picker(purpose: Flux2PickerPurpose) -> Receiver<Option<PathBuf>> {
    let (tx, rx) = mpsc::channel::<Option<PathBuf>>();
    thread::spawn(move || {
        let picked = match purpose {
            Flux2PickerPurpose::TextEncoderDir
            | Flux2PickerPurpose::TransformerDir
            | Flux2PickerPurpose::VaeDir => rfd::FileDialog::new().pick_folder(),
            Flux2PickerPurpose::TransformerFile | Flux2PickerPurpose::VaeFile => {
                rfd::FileDialog::new()
                    .add_filter(
                        t!("cleaning.tools.flux2_klein.weights_files_filter"),
                        &["safetensors", "sft", "gguf"],
                    )
                    .pick_file()
            }
            Flux2PickerPurpose::PromptCacheExport => rfd::FileDialog::new()
                .set_title(t!(
                    "cleaning.tools.flux2_klein.prompt_cache_export_dialog_title"
                ))
                .add_filter(
                    t!("cleaning.tools.flux2_klein.prompt_cache_files_filter"),
                    &[FLUX2_PROMPT_CACHE_EXTENSION],
                )
                // The extension is part of the file's identity, so the suggested name
                // already carries it; the stem is a plain identifier and stays literal.
                .set_file_name(format!("prompt.{FLUX2_PROMPT_CACHE_EXTENSION}"))
                .save_file(),
            Flux2PickerPurpose::PromptCacheImport => rfd::FileDialog::new()
                .set_title(t!(
                    "cleaning.tools.flux2_klein.prompt_cache_import_dialog_title"
                ))
                .add_filter(
                    t!("cleaning.tools.flux2_klein.prompt_cache_files_filter"),
                    &[FLUX2_PROMPT_CACHE_EXTENSION],
                )
                .pick_file(),
        };
        let _ = tx.send(picked);
    });
    rx
}

/// Web fallback: the browser build has no native file dialog (`rfd` is native-only),
/// so the pick resolves immediately as cancelled and the dropped capability is logged.
#[cfg(target_arch = "wasm32")]
pub(super) fn spawn_flux2_picker(_purpose: Flux2PickerPurpose) -> Receiver<Option<PathBuf>> {
    let (tx, rx) = mpsc::channel::<Option<PathBuf>>();
    ms_log::runtime_log::log_warn("[cleaning] FLUX.2 klein file picker unavailable on web build");
    let _ = tx.send(None);
    rx
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

    #[test]
    fn the_undo_stack_is_bounded() {
        let mut session = Flux2SessionState::default();
        for index in 0..(FLUX2_UNDO_LIMIT + 3) {
            // A distinguishable one-pixel entry: the width encodes the push order.
            session.push_undo(egui::ColorImage::filled([index + 1, 1], Color32::WHITE));
        }
        assert_eq!(session.undo_stack.len(), FLUX2_UNDO_LIMIT);
        // The oldest entries are the ones dropped, so the most recent run is always
        // the one «Вернуть» restores.
        assert_eq!(
            session.undo_stack[FLUX2_UNDO_LIMIT - 1].size,
            [FLUX2_UNDO_LIMIT + 3, 1]
        );
        assert_eq!(session.undo_stack[0].size, [4, 1]);
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
}
