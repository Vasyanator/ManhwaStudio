/*
File: cleaning/tools/ai_editor/engines/flux2_klein/sdcpp_request.rs

Purpose:
Turns one FLUX.2 klein run into a Stable-Diffusion.cpp generation: the same validation and the
SAME latent mask the PyTorch wire builds (`validate_flux2_run_input`, `flux2_latent_mask` in
`wire.rs`), the region as init image and first reference, the marks reference second, the
strength, steps and seed rule of the settings, guidance 1.0. (The answer is converted back
into a `ColorImage` by the shared run job, on its worker.)

Key items:
- `Flux2SdcppPrepared`: the request plus the run facts `RawRegionResult` needs back.
- `flux2_sdcpp_prepare()`: worker only (the dilation and the RGB copy are 1 MP passes).
- `FLUX2_SDCPP_SEED_MAX`, `flux2_sdcpp_seed_block_reason()`, `flux2_sdcpp_run_seed()`.

Notes:
FLUX.2 klein is distilled: classifier-free guidance is fixed at 1.0 here (the PyTorch side's
`guidance_scale` dial is hidden on sd.cpp). The RGB of a region pixel is its unmultiplied colour
without alpha — what the PyTorch wire's RGBA PNG becomes after the backend's `convert("RGB")`.
*/

use super::*;
use ms_sdcpp::{GenerateRequest, GrayImage, RgbImage};

/// Guidance of every sd.cpp run: the klein checkpoints are guidance-distilled.
const FLUX2_SDCPP_CFG_SCALE: f32 = 1.0;

/// One sd.cpp run, ready for `SdCppRunJob`: the request and what the finished run hands the
/// host (`RawRegionResult`): the run's mask, its latent mask (the colour-match sample) and the
/// working mode.
pub(super) struct Flux2SdcppPrepared {
    pub(super) request: GenerateRequest,
    pub(super) mask: Vec<u8>,
    pub(super) latent_mask: Vec<u8>,
    pub(super) whole_region: bool,
}

/// The largest seed sd.cpp takes: its seed is a signed 64-bit integer (`i64::MAX`), and a
/// negative one means "random".
pub(super) const FLUX2_SDCPP_SEED_MAX: u64 = 0x7FFF_FFFF_FFFF_FFFF;

/// Why the pinned seed cannot run on sd.cpp: it is above [`FLUX2_SDCPP_SEED_MAX`]. Refused,
/// never folded into range — that would silently run another seed than the one shown.
#[must_use]
pub(super) fn flux2_sdcpp_seed_block_reason(settings: &Flux2KleinSettings) -> Option<String> {
    (settings.use_seed && settings.seed > FLUX2_SDCPP_SEED_MAX).then(|| tf!("cleaning.tools.flux2_klein.sdcpp_seed_range_error", max = FLUX2_SDCPP_SEED_MAX))
}

/// The seed an sd.cpp run uses: the pinned one when «Фиксированный сид» is on, else one drawn
/// HERE in `0..=FLUX2_SDCPP_SEED_MAX` (not sd.cpp's own `-1`), so the finished line can name
/// the seed actually used.
///
/// # Errors
/// [`flux2_sdcpp_seed_block_reason`]'s refusal for a pinned seed out of range.
pub(super) fn flux2_sdcpp_run_seed(settings: &Flux2KleinSettings) -> Result<i64, String> {
    if let Some(reason) = flux2_sdcpp_seed_block_reason(settings) {
        return Err(reason);
    }
    // A random draw is shifted into the non-negative range; a pinned seed is already in range.
    let seed = if settings.use_seed { settings.seed } else { ms_widgets::random_seed() >> 1 };
    i64::try_from(seed).map_err(|_| tf!("cleaning.tools.flux2_klein.sdcpp_seed_range_error", max = FLUX2_SDCPP_SEED_MAX))
}

/// The unmultiplied RGB of `image`, as an sd.cpp image.
fn rgb_of(image: &egui::ColorImage) -> Result<RgbImage, String> {
    let side = |px: usize| u32::try_from(px).map_err(|_| t!("cleaning.inpaint.size_mismatch_error").to_string());
    let (width, height) = (side(image.size[0])?, side(image.size[1])?);
    let mut pixels = Vec::with_capacity(image.pixels.len().saturating_mul(3));
    for pixel in &image.pixels {
        let [r, g, b, _alpha] = pixel.to_srgba_unmultiplied();
        pixels.extend_from_slice(&[r, g, b]);
    }
    RgbImage::new(width, height, pixels).map_err(|err| {
        ms_log::runtime_log::log_error(format!("[cleaning] {FLUX2_LOG_LABEL}: sd.cpp region image: {err}"));
        t!("cleaning.inpaint.size_mismatch_error").to_string()
    })
}

/// Builds the sd.cpp request of one run from the run's pixels and the NORMALIZED settings,
/// validating exactly as the PyTorch pass does and building THE same latent mask
/// (`flux2_latent_mask`): init image = the region, mask = the latent mask (255 = repaint),
/// references = `[region, marks?]`, the settings' strength / steps / the run seed
/// ([`flux2_sdcpp_run_seed`]), guidance 1.0, the
/// region's size.
///
/// # Errors
/// The localized refusals of `validate_flux2_run_input` and `flux2_latent_mask`.
///
/// # Threading
/// A 1 MP dilation and two RGB copies: worker threads only.
pub(super) fn flux2_sdcpp_prepare(input: Flux2RunInput, settings: &Flux2KleinSettings) -> Result<Flux2SdcppPrepared, String> {
    let [width, height] = validate_flux2_run_input(&input)?;
    let seed = flux2_sdcpp_run_seed(settings)?;
    let latent_mask = flux2_latent_mask(&input.mask, width, height, input.whole_region, settings.mask_dilate_px)?;
    let region = rgb_of(&input.region)?;
    let mut references = vec![region.clone()];
    if let Some(marks) = input.reference.as_ref() {
        references.push(rgb_of(marks)?);
    }
    let mask = GrayImage::new(region.width(), region.height(), latent_mask.clone()).map_err(|err| {
        ms_log::runtime_log::log_error(format!("[cleaning] {FLUX2_LOG_LABEL}: sd.cpp latent mask: {err}"));
        t!("cleaning.inpaint.size_mismatch_error").to_string()
    })?;
    let request = GenerateRequest {
        prompt: settings.prompt.clone(),
        negative_prompt: String::new(),
        width: region.width(),
        height: region.height(),
        init_image: Some(region),
        mask: Some(mask),
        references,
        strength: settings.strength,
        steps: settings.steps,
        cfg_scale: FLUX2_SDCPP_CFG_SCALE,
        seed,
        sample_method: None,
        scheduler: None,
        // Overwritten from the engine's sd.cpp options by the shared run job.
        vae_tiling: false,
    };
    Ok(Flux2SdcppPrepared { request, mask: input.mask, latent_mask, whole_region: input.whole_region })
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::Color32;

    /// A 128x128 region with a painted square and an optional marks reference.
    fn input(reference: bool, painted: bool) -> Flux2RunInput {
        let size = [128, 128];
        let mut mask = vec![0u8; 128 * 128];
        if painted {
            for y in 40..60 {
                for x in 40..60 {
                    mask[y * 128 + x] = 255;
                }
            }
        }
        let (mask, whole_region) = mask_for_run(&mask);
        Flux2RunInput {
            region: egui::ColorImage::filled(size, Color32::from_rgb(10, 20, 30)),
            reference: reference.then(|| egui::ColorImage::filled(size, Color32::from_rgb(200, 0, 0))),
            mask,
            whole_region,
            mask_size: size,
        }
    }

    /// The sd.cpp mask is byte-for-byte the latent mask the PyTorch wire has always sent, computed
    /// HERE independently of `flux2_latent_mask`: a painted run is the cv2 ellipse dilation of
    /// the painted mask by `mask_dilate_px` (the pre-extraction rule, inlined), and a
    /// whole-region run is the solid mask unchanged (radius 0, a hand-built all-255 fixture).
    /// It is also what comes back as the sample mask.
    #[test]
    fn the_mask_equals_the_pytorch_latent_mask() {
        let radius = 9;
        let settings = Flux2KleinSettings { mask_dilate_px: radius, ..Flux2KleinSettings::default() }.normalized();

        let painted = input(false, true);
        let expected_painted = dilate_ellipse(&painted.mask, 128, 128, radius).expect("dilation");
        // The growth is real: the dilated square is strictly larger than the painted one.
        assert!(expected_painted.iter().filter(|value| **value > 0).count() > 20 * 20);
        let prepared = flux2_sdcpp_prepare(input(false, true), &settings).expect("prepared");
        assert_eq!(prepared.request.mask.as_ref().map(GrayImage::pixels), Some(expected_painted.as_slice()));
        assert_eq!(prepared.latent_mask, expected_painted);
        assert!(!prepared.whole_region);
        assert_eq!(prepared.mask, painted.mask, "the run's own mask is kept for the host");

        let solid = vec![255u8; 128 * 128];
        let prepared = flux2_sdcpp_prepare(input(false, false), &settings).expect("prepared");
        assert!(prepared.whole_region);
        assert_eq!(prepared.request.mask.as_ref().map(GrayImage::pixels), Some(solid.as_slice()), "whole region: no growth");
        assert_eq!(prepared.latent_mask, solid);
    }

    /// References are the region first, then the marks; the init image is the region.
    #[test]
    fn references_are_the_region_then_the_marks() {
        let settings = Flux2KleinSettings::default().normalized();
        let prepared = flux2_sdcpp_prepare(input(true, true), &settings).expect("prepared");
        let request = &prepared.request;
        assert_eq!(request.references.len(), 2);
        assert_eq!(&request.references[0].pixels()[..3], &[10, 20, 30]);
        assert_eq!(&request.references[1].pixels()[..3], &[200, 0, 0]);
        assert_eq!(request.init_image.as_ref(), Some(&request.references[0]));
        assert_eq!((request.width, request.height), (128, 128));
        assert!((request.cfg_scale - 1.0).abs() < f32::EPSILON);
        assert_eq!(request.steps, settings.steps);
        let without = flux2_sdcpp_prepare(input(false, true), &settings).expect("prepared");
        assert_eq!(without.request.references.len(), 1);
    }

    /// The PyTorch pass's refusals apply unchanged.
    #[test]
    fn a_bad_region_is_refused_like_the_pytorch_pass() {
        let settings = Flux2KleinSettings::default().normalized();
        let mut bad = input(false, true);
        bad.mask.pop();
        assert!(flux2_sdcpp_prepare(bad, &settings).is_err());
        let mut off_grid = input(false, true);
        off_grid.region = egui::ColorImage::filled([120, 128], Color32::WHITE);
        off_grid.mask = vec![255; 120 * 128];
        off_grid.mask_size = [120, 128];
        assert!(flux2_sdcpp_prepare(off_grid, &settings).is_err());
    }

    /// A pinned seed is used as is; a blank one is drawn in range (never `-1`, so the done line
    /// can name it); a pinned seed above `i64::MAX` is refused, never folded.
    #[test]
    fn the_seed_rule() {
        let random = Flux2KleinSettings { use_seed: false, seed: u64::MAX, ..Flux2KleinSettings::default() };
        assert!(flux2_sdcpp_run_seed(&random).is_ok_and(|seed| seed >= 0), "a blank seed is drawn in range");
        assert!(flux2_sdcpp_seed_block_reason(&random).is_none(), "an unpinned stored value is irrelevant");
        let pinned = Flux2KleinSettings { use_seed: true, seed: 5, ..Flux2KleinSettings::default() };
        assert_eq!(flux2_sdcpp_run_seed(&pinned), Ok(5));
        let max = Flux2KleinSettings { use_seed: true, seed: FLUX2_SDCPP_SEED_MAX, ..Flux2KleinSettings::default() };
        assert_eq!(flux2_sdcpp_run_seed(&max), Ok(i64::MAX));
        let huge = Flux2KleinSettings { use_seed: true, seed: FLUX2_SDCPP_SEED_MAX + 1, ..Flux2KleinSettings::default() };
        assert!(flux2_sdcpp_seed_block_reason(&huge).is_some());
        assert!(flux2_sdcpp_run_seed(&huge).is_err());
        let mut run = input(false, true);
        run.mask_size = [128, 128];
        assert!(flux2_sdcpp_prepare(run, &huge.normalized()).is_err(), "the worker refuses it too");
    }
}
