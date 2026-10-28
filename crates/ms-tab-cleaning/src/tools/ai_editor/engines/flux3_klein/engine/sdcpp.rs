/*
File: cleaning/tools/ai_editor/engines/flux3_klein/engine/sdcpp.rs

Purpose:
The Stable-Diffusion.cpp half of `Flux3KleinEngine`: the files worker that resolves and
validates what sd.cpp loads for the selection, the run (a short PREP worker that builds the
generation request off the GUI thread, then the shared `SdCppRunJob` in the sd.cpp worker
process) and its answer as the same `EnginePoll::DoneRaw` the PyTorch run gives.

Main responsibilities:
- resolve the model files on a worker whenever the selection's key moved, on «Обновить» and
  after a catalog job (`poll_and_maybe_resolve_sdcpp_files`), reading the saved sd.cpp device with
  them; expose the guarded state (`sdcpp_files_state`) and the runtime readiness
  (`sdcpp_readiness`);
- build the request (`flux3_sdcpp_generate_request`, pure): init = the region, mask = the run's
  `mask_for_run` mask (sd.cpp's latent blend), strength 1.0, references = [region, user
  references…, marks reference?] each flattened over white and downscaled in Rust (Lanczos3) to
  «Ограничить референсы» (area ≤ side², the backend's `reference_max_pixels` rule), steps, the
  seed, cfg 1.0, the region size;
- start / poll / cancel the run (`start_sdcpp`, `poll_sdcpp_run`, `cancel_sdcpp_run`).

Key structures:
- `Flux3SdcppRun`, `Flux3SdcppPrepared`

Key functions:
- `flux3_sdcpp_generate_request()`, `flux3_sdcpp_capped_size()`, `flux3_sdcpp_raw_result()`,
  `draw_flux3_sdcpp_seed()`

Notes:
A third inherent `impl Flux3KleinEngine` block; the state lives in `mod.rs`. Decoding the
user's reference PNGs and the Lanczos resize are CPU work, so the prep runs on a worker; the job
itself must be started on the GUI thread (it claims the shared progress generation there), which
`poll_sdcpp_run` does when the prep answers. A blank seed is drawn HERE (not by sd.cpp), so the
finished line and «Повторить этот сид» can name it.
*/

use super::*;
use ms_sdcpp::{GenerateRequest, GrayImage, ModelFilesError, RgbImage};
use std::hash::BuildHasher;

/// What the prep worker hands back: the run's input for `SdCppRunJob::start` plus the run facts
/// the raw answer is post-processed with.
pub(in crate::tools::ai_editor::engines::flux3_klein) struct Flux3SdcppPrepared {
    /// Files, request and the engine's sd.cpp options.
    pub(in crate::tools::ai_editor::engines::flux3_klein) input: SdCppRunInput,
    /// The L8 mask the run sent (`mask_for_run`'s output), region-sized.
    pub(in crate::tools::ai_editor::engines::flux3_klein) mask: Vec<u8>,
    /// The whole-region mode of that mask.
    pub(in crate::tools::ai_editor::engines::flux3_klein) whole_region: bool,
    /// `[width, height]` of the region.
    pub(in crate::tools::ai_editor::engines::flux3_klein) size: [usize; 2],
}

/// The sd.cpp run in flight.
pub(in crate::tools::ai_editor::engines::flux3_klein) enum Flux3SdcppRun {
    /// The request is being built on a worker; `seed` is the one it carries.
    Preparing { rx: Receiver<Result<Flux3SdcppPrepared, String>>, seed: u64 },
    /// The job runs in the sd.cpp worker process.
    Running { job: SdCppRunJob, mask: Vec<u8>, whole_region: bool, size: [usize; 2], seed: u64 },
}

impl Flux3KleinEngine {
    /// The sd.cpp files state of the CURRENT selection (an answer about another one is none).
    pub(super) fn sdcpp_files_state(&self) -> Flux3SdcppFilesState {
        Flux3SdcppFilesState::of(self.sdcpp_files.as_ref(), &flux3_sdcpp_files_key(&self.settings))
    }

    /// The readiness of the sd.cpp runtime: the process-global status snapshot (one short lock),
    /// the `--no-ai` / platform gate and the saved device the files worker read.
    pub(super) fn sdcpp_readiness(&self) -> SdCppReadiness {
        SdCppReadiness::of(&ms_sdcpp::snapshot(), sdcpp_route_gate(), self.sdcpp_files.as_ref().map(|answer| &answer.device))
    }

    /// Drains the files worker and asks again — on sd.cpp only — when the selection's key
    /// differs from the last answer's, the process-global sd.cpp status revision moved since the
    /// last question, or a re-check was requested (`sdcpp_files_wanted`: «Обновить»,
    /// the end of a catalog job). One query at a time; a typed folder asks again after the
    /// answer in flight, never per frame.
    pub(super) fn poll_and_maybe_resolve_sdcpp_files(&mut self) {
        if let Some(rx) = self.sdcpp_files_rx.as_ref() {
            match rx.try_recv() {
                Ok(answer) => {
                    self.sdcpp_files = Some(answer);
                    self.sdcpp_files_rx = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    // A worker that ended without answering: recorded as an unreadable answer for
                    // the key it was asked about, so the same question is not re-asked every frame.
                    let key = flux3_sdcpp_files_key(&self.settings);
                    let path = PathBuf::from(&key.0);
                    ms_log::runtime_log::log_error(format!("[cleaning] {FLUX3_LOG_LABEL} sd.cpp files worker ended without an answer"));
                    let detail = "the files check ended without an answer".to_string();
                    self.sdcpp_files = Some(Flux3SdcppFilesAnswer {
                        key,
                        files: Err(Flux3SdcppFilesError::Files(ModelFilesError::Unreadable { role: ms_sdcpp::ComponentRole::Diffusion, path, detail })),
                        device: ms_config::SdCppDeviceChoice::Auto,
                    });
                    self.sdcpp_files_rx = None;
                }
            }
        }
        if self.settings.runtime() != ImageEditRuntime::SdCpp || self.sdcpp_files_rx.is_some() {
            return;
        }
        let key = flux3_sdcpp_files_key(&self.settings);
        let answered = self.sdcpp_files.as_ref().is_some_and(|answer| answer.key == key);
        // A refreshed sd.cpp status (a device or build saved in Settings, an install, a probe)
        // re-reads the saved device with the files, so a stale «device not found» clears itself.
        let revision = ms_sdcpp::snapshot().revision;
        let status_moved = self.sdcpp_status_revision != Some(revision);
        if answered && !self.sdcpp_files_wanted && !status_moved {
            return;
        }
        self.sdcpp_files_wanted = false;
        self.sdcpp_status_revision = Some(revision);
        let settings = self.settings.normalized();
        let (tx, rx) = mpsc::channel();
        self.sdcpp_files_rx = Some(rx);
        thread::spawn(move || {
            let files = resolve_flux3_sdcpp_files(&settings);
            let device = read_sdcpp_choice().device;
            // A failed send means the engine was dropped meanwhile: nobody waits.
            let _ = tx.send(Flux3SdcppFilesAnswer { key, files, device });
        });
    }

    /// Starts an sd.cpp run of `input` with the validated files of the current selection: the
    /// request is built on a prep worker (`poll_sdcpp_run` then starts the job). A blank seed is
    /// drawn here.
    ///
    /// # Errors
    /// The run gate's own line when the files are not (or no longer) validated for this
    /// selection — `start` checks the gate first, so this only guards the race.
    pub(super) fn start_sdcpp(&mut self, input: Flux3RunInput) -> Result<(), String> {
        let key = flux3_sdcpp_files_key(&self.settings);
        let files = match self.sdcpp_files.as_ref().filter(|answer| answer.key == key).map(|answer| &answer.files) {
            Some(Ok(files)) => files.clone(),
            Some(Err(err)) => return Err(err.message()),
            None => return Err(t!("cleaning.tools.flux3_klein.sdcpp_files_checking_status").to_string()),
        };
        let settings = self.settings.normalized();
        let seed = settings.seed.unwrap_or_else(draw_flux3_sdcpp_seed);
        ms_log::runtime_log::log_info(format!(
            "[cleaning] {FLUX3_LOG_LABEL} sd.cpp run requested: DiT {}, encoder {}, steps {}, seed {seed}, references cap {}",
            settings.run_dit_format().wire(),
            settings.encoder_format().wire(),
            settings.steps,
            settings.reference_max_side
        ));
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let prepared = flux3_sdcpp_generate_request(&input, &settings, seed).map(|request| Flux3SdcppPrepared {
                input: SdCppRunInput { files, request, options: settings.sdcpp.clone() },
                mask: input.mask,
                whole_region: input.whole_region,
                size: input.size,
            });
            // A failed send means the run was cancelled meanwhile: the request is unwanted.
            let _ = tx.send(prepared);
        });
        self.sdcpp_run = Some(Flux3SdcppRun::Preparing { rx, seed });
        self.run_status = None;
        Ok(())
    }

    /// Advances the sd.cpp run: a finished prep starts the job (on this GUI thread, which claims
    /// the progress bar), a finished job answers `DoneRaw` with the run's mask and mode. `Idle`
    /// without an sd.cpp run.
    pub(super) fn poll_sdcpp_run(&mut self) -> EnginePoll {
        let Some(run) = self.sdcpp_run.take() else {
            return EnginePoll::Idle;
        };
        match run {
            Flux3SdcppRun::Preparing { rx, seed } => match rx.try_recv() {
                Ok(Ok(prepared)) => {
                    let Flux3SdcppPrepared { input, mask, whole_region, size } = prepared;
                    let job = SdCppRunJob::start(input, &self.progress, FLUX3_LOG_LABEL);
                    self.sdcpp_run = Some(Flux3SdcppRun::Running { job, mask, whole_region, size, seed });
                    EnginePoll::Running
                }
                Ok(Err(err)) => self.fail_sdcpp_run(&err),
                Err(TryRecvError::Empty) => {
                    self.sdcpp_run = Some(Flux3SdcppRun::Preparing { rx, seed });
                    EnginePoll::Running
                }
                Err(TryRecvError::Disconnected) => self.fail_sdcpp_run(t!("cleaning.mask_editor.processing_thread_crashed_error")),
            },
            Flux3SdcppRun::Running { mut job, mask, whole_region, size, seed } => match job.poll() {
                None => {
                    self.sdcpp_run = Some(Flux3SdcppRun::Running { job, mask, whole_region, size, seed });
                    EnginePoll::Running
                }
                Some(Err(err)) => self.fail_sdcpp_run(&err),
                Some(Ok(image)) => match flux3_sdcpp_raw_result(image, mask, whole_region, size) {
                    Ok(raw) => {
                        ms_log::runtime_log::log_info(format!("[cleaning] {FLUX3_LOG_LABEL} sd.cpp run finished: seed {seed}, whole region {whole_region}"));
                        self.last_seed = Some(seed);
                        self.run_status = Some(flux3_run_done_status(seed, None));
                        EnginePoll::DoneRaw(raw)
                    }
                    Err(err) => self.fail_sdcpp_run(&err),
                },
            },
        }
    }

    /// Records a failed sd.cpp run (the line under the bar) and answers `Failed`.
    fn fail_sdcpp_run(&mut self, err: &str) -> EnginePoll {
        let message = tf!("cleaning.mask_editor.processing_error", err = err);
        self.run_status = Some(message.clone());
        EnginePoll::Failed(message)
    }

    /// Abandons the sd.cpp run: a prep is dropped (it touches nothing outside), a job is
    /// cancelled through the shared job (cancel flag, bar retired, receiver parked in
    /// `abandoned` until the worker process answered). Returns whether there was one.
    pub(super) fn cancel_sdcpp_run(&mut self) -> bool {
        let Some(run) = self.sdcpp_run.take() else {
            return false;
        };
        match run {
            Flux3SdcppRun::Preparing { .. } => ms_log::runtime_log::log_info(format!("[cleaning] {FLUX3_LOG_LABEL} sd.cpp run cancelled while its request was prepared")),
            Flux3SdcppRun::Running { job, .. } => job.cancel(&mut self.abandoned, FLUX3_LOG_LABEL),
        }
        self.run_status = Some(t!("cleaning.mask_editor.processing_cancelled_status").to_string());
        true
    }
}

/// The sd.cpp route's `--no-ai` / platform gate from its one owner
/// (`AiRouteKind::SdCpp.availability`), never the raw `LocalAiAvailability` field.
pub(super) fn sdcpp_route_gate() -> Result<(), ms_local_ai::local_ai::LocalAiOff> {
    ms_local_ai::local_ai::AiRouteKind::SdCpp.availability(&ms_local_ai::local_ai::process())
}

/// Builds the sd.cpp generation of one region edit. `settings` must be `normalized()`; `seed`
/// is the run's (pinned, or drawn by [`draw_flux3_sdcpp_seed`]).
///
/// init = the region (RGB over white), mask = `input.mask` (255 = repaint; sd.cpp blends in
/// latent space, the host composites afterwards), references = [region, user references in
/// order, marks reference?], each capped by «Ограничить референсы» ([`flux3_sdcpp_capped_size`]),
/// strength 1.0 and cfg 1.0 (FLUX.3 klein is distilled), the region's size, no sampler override.
///
/// # Errors
/// A localized message when a size does not fit `u32`, a buffer does not match its size, a
/// reference PNG cannot be decoded, or the seed exceeds sd.cpp's signed range.
pub(super) fn flux3_sdcpp_generate_request(input: &Flux3RunInput, settings: &Flux3KleinSettings, seed: u64) -> Result<GenerateRequest, String> {
    let refuse = |detail: String| {
        ms_log::runtime_log::log_warn(format!("[cleaning] {FLUX3_LOG_LABEL} sd.cpp request not built: {detail}"));
        tf!("cleaning.tools.flux3_klein.sdcpp_reference_error", err = detail)
    };
    let side = |px: usize| u32::try_from(px).map_err(|err| refuse(err.to_string()));
    let [width, height] = input.size;
    let (width, height) = (side(width)?, side(height)?);
    let seed = i64::try_from(seed).map_err(|_| tf!("cleaning.tools.flux3_klein.sdcpp_seed_range_error", max = FLUX3_SDCPP_SEED_MAX))?;
    let cap = settings.reference_max_side;
    let region_rgb = color_image_rgb(&input.region).map_err(refuse)?;
    let init = RgbImage::new(width, height, region_rgb.clone()).map_err(|err| refuse(err.to_string()))?;
    let mask = GrayImage::new(width, height, input.mask.clone()).map_err(|err| refuse(err.to_string()))?;
    let mut references = vec![capped_reference(width, height, region_rgb, cap).map_err(refuse)?];
    for png in &input.references {
        references.push(png_reference(png, cap).map_err(refuse)?);
    }
    if let Some(marks) = input.marks_reference.as_ref() {
        let [marks_width, marks_height] = marks.size;
        references.push(capped_reference(side(marks_width)?, side(marks_height)?, color_image_rgb(marks).map_err(refuse)?, cap).map_err(refuse)?);
    }
    Ok(GenerateRequest {
        prompt: settings.prompt.trim().to_string(),
        negative_prompt: String::new(),
        width,
        height,
        init_image: Some(init),
        mask: Some(mask),
        references,
        strength: 1.0,
        steps: settings.steps,
        cfg_scale: 1.0,
        seed,
        sample_method: None,
        scheduler: None,
        // The run job sets it from the engine's sd.cpp options.
        vae_tiling: false,
    })
}

/// The size a `width` x `height` reference is sent at under the cap `side` (`0` = no cap): kept
/// when its AREA is within `side²` (the backend's `reference_max_pixels` rule), else scaled
/// down keeping the aspect, each side `floor(sqrt(side² · w / h))`-style in integers so the
/// product never exceeds `side²`, never below 1 px and never above the original.
#[must_use]
pub(super) fn flux3_sdcpp_capped_size(width: u32, height: u32, side: u32) -> (u32, u32) {
    let budget = u64::from(side) * u64::from(side);
    let (w, h) = (u64::from(width), u64::from(height));
    if side == 0 || width == 0 || height == 0 || w * h <= budget {
        return (width, height);
    }
    // (nw·nh)² <= (budget·w/h)·(budget·h/w) = budget², so nw·nh <= budget — unless the 1 px
    // floor lifted the short side of an extreme strip; then the long side is shortened to
    // `budget / short` to stay within it. `budget · a` may overflow for a huge cap: it then
    // saturates, and the `min` with the original side bounds the result. Both sides stay
    // below 2^32, so `nw · nh` cannot overflow.
    let scaled = |a: u64, b: u64| budget.checked_mul(a).map_or(u64::MAX, |product| product / b).isqrt().max(1);
    let (mut nw, mut nh) = (scaled(w, h).min(w), scaled(h, w).min(h));
    if nw * nh > budget {
        if nw >= nh {
            nw = (budget / nh).max(1);
        } else {
            nh = (budget / nw).max(1);
        }
    }
    (u32::try_from(nw).unwrap_or(width), u32::try_from(nh).unwrap_or(height))
}

/// An RGB reference of `width` x `height` (`rgb` = its bytes) at its capped size, resized with
/// Lanczos3 when the cap applies.
fn capped_reference(width: u32, height: u32, rgb: Vec<u8>, cap: u32) -> Result<RgbImage, String> {
    let (target_width, target_height) = flux3_sdcpp_capped_size(width, height, cap);
    if (target_width, target_height) == (width, height) {
        return RgbImage::new(width, height, rgb).map_err(|err| err.to_string());
    }
    let full = image::RgbImage::from_raw(width, height, rgb).ok_or_else(|| format!("{width}x{height} buffer has the wrong length"))?;
    let resized = image::imageops::resize(&full, target_width, target_height, image::imageops::FilterType::Lanczos3);
    RgbImage::new(target_width, target_height, resized.into_raw()).map_err(|err| err.to_string())
}

/// A user reference (its wire PNG) as a capped RGB reference, alpha flattened over white like
/// the backend flattens it.
fn png_reference(png: &[u8], cap: u32) -> Result<RgbImage, String> {
    let decoded = image::load_from_memory(png).map_err(|err| err.to_string())?.to_rgba8();
    let (width, height) = decoded.dimensions();
    let mut rgb = Vec::new();
    ms_raster::rgba_over_white_to_rgb(decoded.as_raw(), &mut rgb).map_err(|err| err.to_string())?;
    capped_reference(width, height, rgb, cap)
}

/// The RGB bytes of an egui image, alpha flattened over white.
fn color_image_rgb(image: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let rgba: Vec<u8> = image.pixels.iter().flat_map(|pixel| pixel.to_srgba_unmultiplied()).collect();
    let mut rgb = Vec::new();
    ms_raster::rgba_over_white_to_rgb(&rgba, &mut rgb).map_err(|err| err.to_string())?;
    Ok(rgb)
}

/// The finished sd.cpp run as the host's raw result: the generated region with the SAME mask the
/// run sent and its mode — the pairing a PyTorch answer has (`sample_mask: None`: FLUX.3 has no
/// separate latent mask), so the host composites it inside `mask` with the live «Постобработка»
/// settings.
///
/// `image` arrives already converted on the job's worker (`SdCppRunJob::poll`); only its size is
/// checked here (a cheap comparison on the GUI thread).
///
/// # Errors
/// `cleaning.inpaint.size_mismatch_error` (logged with both sizes) when it is not exactly `size`.
pub(super) fn flux3_sdcpp_raw_result(image: egui::ColorImage, mask: Vec<u8>, whole_region: bool, size: [usize; 2]) -> Result<RawRegionResult, String> {
    if image.size != size {
        ms_log::runtime_log::log_error(format!("[cleaning] {FLUX3_LOG_LABEL} sd.cpp answered {}x{} for a {}x{} region", image.size[0], image.size[1], size[0], size[1]));
        return Err(t!("cleaning.inpaint.size_mismatch_error").to_string());
    }
    Ok(RawRegionResult { image, mask, sample_mask: None, whole_region })
}

/// A fresh random seed in `0..=FLUX3_SDCPP_SEED_MAX`: a newly keyed `RandomState` (OS-seeded,
/// re-keyed per instance) hashes a constant; the top bit is dropped so it fits sd.cpp's signed
/// seed.
#[must_use]
pub(super) fn draw_flux3_sdcpp_seed() -> u64 {
    std::collections::hash_map::RandomState::new().hash_one(0_u8) >> 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A region of `w` x `h`, one solid colour, and an all-repaint mask.
    fn input(w: usize, h: usize, references: Vec<Arc<Vec<u8>>>, marks: bool) -> Flux3RunInput {
        Flux3RunInput {
            region: egui::ColorImage::filled([w, h], Color32::from_rgb(10, 20, 30)),
            mask: vec![255; w * h],
            whole_region: true,
            size: [w, h],
            references,
            marks_reference: marks.then(|| egui::ColorImage::filled([w, h], Color32::RED)),
        }
    }

    /// A PNG of `w` x `h` in one colour (`encode_color_image_png_rgba`, the wire form).
    fn png(w: usize, h: usize, color: Color32) -> Arc<Vec<u8>> {
        Arc::new(encode_color_image_png_rgba(&egui::ColorImage::filled([w, h], color)).expect("encodes"))
    }

    /// The request: init and mask are the region and the run's mask; the references are the
    /// region, the user references in order and the marks last; strength and cfg 1, the settings'
    /// steps and the given seed.
    #[test]
    fn the_request_carries_the_region_mask_and_references_in_order() {
        let settings = Flux3KleinSettings { steps: 12, ..Flux3KleinSettings::default() }.normalized();
        let refs = vec![png(64, 32, Color32::GREEN), png(40, 40, Color32::BLUE)];
        let request = flux3_sdcpp_generate_request(&input(128, 128, refs, true), &settings, 77).expect("builds");
        assert_eq!((request.width, request.height), (128, 128));
        let init = request.init_image.as_ref().expect("init");
        assert_eq!(&init.pixels()[..3], &[10, 20, 30]);
        assert_eq!(request.mask.as_ref().map(|mask| mask.pixels().len()), Some(128 * 128));
        assert_eq!((request.strength, request.cfg_scale, request.steps, request.seed), (1.0, 1.0, 12, 77));
        let sizes: Vec<(u32, u32)> = request.references.iter().map(|image| (image.width(), image.height())).collect();
        assert_eq!(sizes, vec![(128, 128), (64, 32), (40, 40), (128, 128)], "region, user refs, marks");
        assert_eq!(&request.references[1].pixels()[..3], &[0, 255, 0]);
        assert_eq!(&request.references[2].pixels()[..3], &[0, 0, 255]);
        assert_eq!(&request.references[3].pixels()[..3], &[255, 0, 0], "the marks reference is last");
        assert_eq!(request.prompt, settings.prompt);
        assert!(request.negative_prompt.is_empty());
    }

    /// The reference cap downscales every reference (the region included) to area ≤ side²,
    /// keeping the aspect; the init image stays full size.
    #[test]
    fn the_reference_cap_downscales_every_reference() {
        let settings = Flux3KleinSettings { reference_max_side: 512, ..Flux3KleinSettings::default() }.normalized();
        let refs = vec![png(2048, 1024, Color32::GREEN), png(256, 256, Color32::BLUE)];
        let request = flux3_sdcpp_generate_request(&input(1024, 1024, refs, false), &settings, 1).expect("builds");
        let init = request.init_image.as_ref().expect("init");
        assert_eq!((init.width(), init.height()), (1024, 1024), "the init image is never capped");
        let sizes: Vec<(u32, u32)> = request.references.iter().map(|image| (image.width(), image.height())).collect();
        assert_eq!(sizes, vec![(512, 512), (724, 362), (256, 256)]);
        for (w, h) in sizes {
            assert!(u64::from(w) * u64::from(h) <= 512 * 512);
        }
    }

    /// The capped size: kept within the budget, scaled with its aspect above it, never zero.
    #[test]
    fn the_capped_size_keeps_the_area_within_the_budget() {
        assert_eq!(flux3_sdcpp_capped_size(2048, 2048, 0), (2048, 2048), "no cap");
        assert_eq!(flux3_sdcpp_capped_size(500, 500, 512), (500, 500));
        assert_eq!(flux3_sdcpp_capped_size(1024, 1024, 768), (768, 768));
        assert_eq!(flux3_sdcpp_capped_size(4096, 1024, 1024), (2048, 512));
        assert_eq!(flux3_sdcpp_capped_size(100_000, 1, 512), (100_000, 1), "a thin strip within the area budget is kept");
        assert_eq!(flux3_sdcpp_capped_size(1_000_000, 2, 512), (262_144, 1), "the 1 px floor shortens the long side to stay in budget");
        for (w, h, side) in [(3000, 1999, 512), (17, 9000, 768), (4096, 4096, 1024), (1_000_000, 2, 512), (3, 4_000_000, 768)] {
            let (nw, nh) = flux3_sdcpp_capped_size(w, h, side);
            assert!(u64::from(nw) * u64::from(nh) <= u64::from(side) * u64::from(side), "{w}x{h} -> {nw}x{nh}");
            assert!(nw >= 1 && nh >= 1 && nw <= w && nh <= h);
        }
    }

    /// A seed beyond sd.cpp's signed range is refused; a drawn seed always fits.
    #[test]
    fn the_seed_must_fit_sd_cpp() {
        let settings = Flux3KleinSettings::default().normalized();
        assert!(flux3_sdcpp_generate_request(&input(128, 128, Vec::new(), false), &settings, FLUX3_SDCPP_SEED_MAX + 1).is_err());
        assert!(flux3_sdcpp_generate_request(&input(128, 128, Vec::new(), false), &settings, FLUX3_SDCPP_SEED_MAX).is_ok());
        for _ in 0..32 {
            assert!(draw_flux3_sdcpp_seed() <= FLUX3_SDCPP_SEED_MAX);
        }
    }

    /// A broken reference PNG refuses the request instead of sending a run without it.
    #[test]
    fn a_broken_reference_refuses_the_request() {
        let settings = Flux3KleinSettings::default().normalized();
        assert!(flux3_sdcpp_generate_request(&input(128, 128, vec![Arc::new(vec![1, 2, 3])], false), &settings, 1).is_err());
    }

    /// The raw result pairs the generated region with the run's own mask and mode.
    #[test]
    fn the_raw_result_pairs_the_region_with_the_runs_mask() {
        let image = egui::ColorImage::filled([2, 2], Color32::from_rgb(9, 9, 9));
        let mask = vec![0, 255, 255, 0];
        let raw = flux3_sdcpp_raw_result(image.clone(), mask.clone(), false, [2, 2]).expect("same size");
        assert_eq!(raw.mask, mask);
        assert!(raw.sample_mask.is_none());
        assert!(!raw.whole_region);
        assert_eq!(raw.image.size, [2, 2]);
        assert!(flux3_sdcpp_raw_result(image, mask, true, [4, 1]).is_err(), "a wrong size is refused");
    }

    /// An engine on sd.cpp with validated files for a fictional manual root.
    /// Selecting the engine without PyTorch moves it to sd.cpp where sd.cpp is offered, saved
    /// like a click; with PyTorch, or while the switch is blocked, nothing moves.
    #[test]
    fn selecting_without_pytorch_switches_to_sdcpp() {
        let no_torch = AiCaps { backend: Some(true), torch: Some(false), ort: None, sdcpp: None, backend_starting: false };
        let torch = AiCaps { torch: Some(true), ..no_torch };

        let mut engine = Flux3KleinEngine::at_rest();
        engine.on_selected(&torch);
        assert_eq!(engine.settings.runtime(), ImageEditRuntime::PyTorch, "PyTorch is available");

        engine.on_selected(&no_torch);
        let expected = if sdcpp_route_available() { ImageEditRuntime::SdCpp } else { ImageEditRuntime::PyTorch };
        assert_eq!(engine.settings.runtime(), expected);
        if sdcpp_route_available() {
            assert!(engine.dirty, "the automatic switch is saved like a click");
        }
    }

    /// A selection before the settings load landed is decided AFTER it, on the file: the file's
    /// fields survive and only the runtime moves (where sd.cpp is offered); a file that already
    /// selects sd.cpp causes no switch and no write.
    #[test]
    fn a_selection_before_the_load_keeps_the_file() {
        let no_torch = AiCaps { backend: Some(false), torch: None, ort: None, sdcpp: None, backend_starting: false };
        let file = Flux3KleinSettings { seed: Some(77), steps: 30, ..Flux3KleinSettings::default() };
        assert_eq!(file.runtime(), ImageEditRuntime::PyTorch);

        let mut engine = Flux3KleinEngine::at_rest();
        let (tx, rx) = mpsc::channel();
        engine.settings_rx = Some(rx);
        engine.on_selected(&no_torch);
        assert!(engine.selection_pending, "no decision on unloaded settings");
        assert!(!engine.dirty, "nothing may outrank the file before it landed");
        tx.send(Flux3SettingsLoad { settings: file.clone(), problem: None }).expect("send");
        engine.poll_settings_load_with(|| no_torch);
        assert!(engine.settings_loaded && !engine.selection_pending);
        assert_eq!((engine.settings.seed, engine.settings.steps), (Some(77), 30), "the file's fields survive");
        let expected = if sdcpp_route_available() { ImageEditRuntime::SdCpp } else { ImageEditRuntime::PyTorch };
        assert_eq!(engine.settings.runtime(), expected);
        assert_eq!(engine.dirty, sdcpp_route_available(), "written once, only for the switch");

        let mut on_sdcpp = Flux3KleinEngine::at_rest();
        let (tx, rx) = mpsc::channel();
        on_sdcpp.settings_rx = Some(rx);
        on_sdcpp.on_selected(&no_torch);
        tx.send(Flux3SettingsLoad { settings: Flux3KleinSettings { runtime: "sdcpp".to_string(), ..file }, problem: None }).expect("send");
        on_sdcpp.poll_settings_load_with(|| no_torch);
        assert_eq!(on_sdcpp.settings.runtime(), ImageEditRuntime::SdCpp);
        assert!(!on_sdcpp.dirty, "nothing to write");
    }

    /// A loader that died without answering leaves the DEFAULTS in memory: the deferred
    /// selection is dropped, nothing switches and nothing is owed a write.
    #[test]
    fn a_selection_before_a_dead_loader_decides_nothing() {
        let no_torch = AiCaps { backend: Some(false), torch: None, ort: None, sdcpp: None, backend_starting: false };
        let mut engine = Flux3KleinEngine::at_rest();
        let (tx, rx) = mpsc::channel::<Flux3SettingsLoad>();
        engine.settings_rx = Some(rx);
        engine.on_selected(&no_torch);
        assert!(engine.selection_pending);
        drop(tx);
        engine.poll_settings_load_with(|| no_torch);
        assert!(engine.settings_loaded && !engine.selection_pending);
        assert_eq!(engine.settings.runtime(), ImageEditRuntime::PyTorch, "no decision on defaults");
        assert!(!engine.dirty, "nothing may be written over an unread file");
    }

    fn sdcpp_engine() -> Flux3KleinEngine {
        let mut engine = Flux3KleinEngine::at_rest();
        engine.settings.runtime = "sdcpp".to_string();
        engine.settings.source_mode = "manual".to_string();
        engine.settings.model_root = std::env::temp_dir().join(format!("ms_flux3_sdcpp_engine_{}", std::process::id())).to_string_lossy().to_string();
        let key = flux3_sdcpp_files_key(&engine.settings);
        let files = flux3_sdcpp_model_files(&engine.settings).expect("maps");
        engine.sdcpp_files = Some(Flux3SdcppFilesAnswer { key, files: Ok(files), device: ms_config::SdCppDeviceChoice::Auto });
        engine
    }

    /// The runtime decides the Torch requirement; sd.cpp's gate never asks for Torch but blocks
    /// until the runtime and the files are ready, and PyTorch without Torch names the sd.cpp way
    /// out where the platform has it.
    #[test]
    fn the_runtime_decides_the_torch_rule_and_the_gate() {
        let _locale = english();
        let mut engine = Flux3KleinEngine::at_rest();
        assert!(engine.requires_torch(), "PyTorch by default");
        assert_eq!(engine.ai_requirement(), Some(AiRequirement::TorchOrSdcpp), "the picker gate covers both runtimes");
        engine.set_torch_available(false);
        let expected = sdcpp_route_available().then(|| t!("cleaning.tools.sdcpp.torch_missing_hint").to_string());
        assert_eq!(engine.run_block_reason(), expected);
        engine.set_torch_available(true);
        assert!(engine.run_block_reason().is_none());
        let mut sdcpp = sdcpp_engine();
        assert!(!sdcpp.requires_torch());
        sdcpp.set_torch_available(false);
        // The process-global sd.cpp status is never refreshed in a test: the runtime line blocks
        // (checking / disabled), never the Torch rule.
        let reason = sdcpp.run_block_reason().expect("not ready in a test process");
        assert_ne!(reason, t!("cleaning.tools.sdcpp.torch_missing_hint"));
        assert_eq!(sdcpp.sdcpp_files_state(), Flux3SdcppFilesState::Ready);
        sdcpp.settings.text_encoder_format = "q8_0".to_string();
        assert_eq!(sdcpp.sdcpp_files_state(), Flux3SdcppFilesState::Checking, "another selection has no answer yet");
    }

    /// The runtime switch closes while any job of either runtime is in flight and reopens after.
    #[test]
    fn the_runtime_switch_is_closed_while_a_job_runs() {
        let mut engine = Flux3KleinEngine::at_rest();
        assert!(engine.runtime_switch_block_reason().is_none());
        let (_run_tx, run_rx) = mpsc::channel();
        engine.run_rx = Some(run_rx);
        assert!(engine.runtime_switch_block_reason().is_some(), "a PyTorch run");
        engine.run_rx = None;
        let (_cache_tx, cache_rx) = mpsc::channel();
        engine.prompt_cache.rx = Some(cache_rx);
        assert!(engine.runtime_switch_block_reason().is_some(), "a prompt-cache build");
        engine.prompt_cache.rx = None;
        let (_unload_tx, unload_rx) = mpsc::channel();
        engine.unload_rx = Some(unload_rx);
        assert!(engine.runtime_switch_block_reason().is_some(), "a component unload");
        engine.unload_rx = None;
        let (_prep_tx, prep_rx) = mpsc::channel();
        engine.sdcpp_run = Some(Flux3SdcppRun::Preparing { rx: prep_rx, seed: 1 });
        assert!(engine.runtime_switch_block_reason().is_some(), "an sd.cpp run");
        assert!(engine.switch_block_reason().is_some(), "the engine picker too");
        assert!(engine.cancel_run(), "a prep is cancelled");
        assert!(engine.sdcpp_run.is_none() && engine.run_status.is_some());
        assert!(engine.runtime_switch_block_reason().is_none(), "nothing in flight");
        // A cancelled run still ending keeps the engine picker AND the switch closed.
        let (abandoned_tx, abandoned_rx) = mpsc::channel::<()>();
        engine.abandoned.push(abandoned_rx);
        assert!(engine.switch_block_reason().is_some(), "the cancelled job still holds the sd.cpp worker");
        assert!(engine.runtime_switch_block_reason().is_some());
        drop(abandoned_tx);
        assert!(!engine.abandoned.poll());
        assert!(engine.switch_block_reason().is_none(), "reopens once it ended");
    }

    /// A moved sd.cpp status revision (a device saved in Settings, a probe) re-asks the files
    /// and the device even though the answer still names the current selection.
    #[test]
    fn a_status_refresh_re_reads_the_files_and_the_device() {
        let mut engine = sdcpp_engine();
        let revision = ms_sdcpp::snapshot().revision;
        engine.sdcpp_status_revision = Some(revision);
        engine.poll_and_maybe_resolve_sdcpp_files();
        assert!(engine.sdcpp_files_rx.is_none(), "same selection, same revision: nothing asked");
        engine.sdcpp_status_revision = Some(revision.wrapping_add(1));
        engine.poll_and_maybe_resolve_sdcpp_files();
        assert!(engine.sdcpp_files_rx.is_some(), "a moved revision re-asks");
        assert_eq!(engine.sdcpp_status_revision, Some(revision));
    }

    /// `start_sdcpp` builds the request on its prep worker: the files of the selection, the
    /// run's mask and mode, the references in order, the pinned seed; a failed prep answers
    /// `Failed`.
    #[test]
    fn an_sdcpp_start_prepares_the_request_off_the_gui_thread() {
        let mut engine = sdcpp_engine();
        engine.settings.seed = Some(5);
        let refs = vec![png(64, 32, Color32::GREEN)];
        let mut run_input = input(128, 128, refs, true);
        run_input.mask[0] = 0;
        run_input.whole_region = false;
        let sent_mask = run_input.mask.clone();
        engine.start_sdcpp(run_input).expect("validated files");
        assert!(engine.pipeline_busy());
        let Some(Flux3SdcppRun::Preparing { rx, seed }) = engine.sdcpp_run.take() else {
            panic!("the run starts with its prep");
        };
        assert_eq!(seed, 5);
        let prepared = rx.recv_timeout(std::time::Duration::from_secs(30)).expect("the prep answers").expect("builds");
        assert_eq!(prepared.mask, sent_mask, "the mask that travels back is the one sent");
        assert!(!prepared.whole_region);
        assert_eq!(prepared.input.files, flux3_sdcpp_model_files(&engine.settings).expect("maps"));
        assert_eq!(prepared.input.request.mask.as_ref().map(|mask| mask.pixels().to_vec()), Some(sent_mask));
        assert_eq!(prepared.input.request.references.len(), 3, "region, one user reference, marks");
        assert_eq!(prepared.input.request.seed, 5);
        // A prep that failed is the run's failure.
        let (tx, rx) = mpsc::channel();
        tx.send(Err("broken".to_string())).expect("send");
        engine.sdcpp_run = Some(Flux3SdcppRun::Preparing { rx, seed: 1 });
        let _locale = english();
        assert!(matches!(engine.poll_sdcpp_run(), EnginePoll::Failed(_)));
        assert!(engine.sdcpp_run.is_none());
    }

}
