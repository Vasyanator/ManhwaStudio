/*
File: crates/ms-ai-api/src/image_edit/pipeline.rs

Purpose:
The single owner of size on the API side: the pure halves of an image-edit run around the
HTTP exchange. `prepare` validates the request, applies the caller's integer upscale `k`,
encodes the image and the native mask and resolves the endpoint; `finish` decodes the
provider's answer, demands exactly `(k * W, k * H)`, box-downscales by `k`, composites inside
the feathered mask and returns exactly `W x H` with alpha 255.

Key structures:
- PreparedCall

Key functions:
- prepare()
- finish()
- run_image_edit(): the whole run, prepare -> executor -> finish (native; a wasm stub).

Notes:
No resampling except the integer pair `ms_raster::upscale_replicate` / `downscale_box`, which
are exact inverses: pixels the model left alone survive bit for bit. `k` comes from the caller
(the cleaning frame's `upscale_factor_for` owns that decision); this module only checks it is
within the offer's `max_upscale`. A wrong output size is a typed `SizeMismatch`, never a
resize. `run_image_edit` joins the halves through the adapter of the provider's shape and the
native executor (worker threads only); it logs one `runtime_log` line per run (provider, model,
sizes, `k`, elapsed, outcome; never the key, the prompt only by length) and the prompt text
only to the opt-in trace log.
*/

use super::catalog::{MaskSupport, ModelOffer, SizeParamStyle};
use super::codec::{decode_rgba, encode_rgb_png};
use super::composite::composite_feathered;
use super::error::ImageEditError;
use super::protocol::EditCall;
use super::provider::EndpointKind;
use super::request::{EndpointChoice, ImageEditOutcome, ImageEditRequest, RgbaRegion, pixel_count};
use crate::service::AiApiService;
use crate::target::AiApiTarget;
#[cfg(not(target_arch = "wasm32"))]
use super::adapters::protocol_for;
#[cfg(not(target_arch = "wasm32"))]
use super::catalog::lookup;
#[cfg(not(target_arch = "wasm32"))]
use super::executor::{ExecutorLimits, HttpTransport, RunContext, UreqTransport, execute};
use super::request::{CancelFlag, ImageEditStage};

/// The provider call of a run plus what `finish` needs to map the answer back.
#[derive(Debug, Clone)]
pub struct PreparedCall {
    /// What the adapter sends.
    pub call: EditCall,
    /// The applied upscale factor `k`.
    pub upscale: u8,
    /// The source region size `(W, H)`.
    pub source_size: (u32, u32),
}

/// Validates `request` against `offer` and builds the provider call (steps 1-4 of the run:
/// validate, upscale, encode, size and endpoint).
///
/// # Errors
/// - `EmptyPrompt` for a blank prompt.
/// - `UnknownModel` when `offer` is not the request's provider / model.
/// - `ShapeMismatch` for a mask of the wrong length or an upscaled size that overflows.
/// - `UpscaleNotAllowed` when `request.upscale` is outside `1..=offer.rule.max_upscale`.
/// - `SizeNotOffered` when the offer states its size by table labels and `(k * W, k * H)` is
///   not an entry.
/// - `InvalidEndpoint` when the endpoint choice does not fit the provider.
/// - `Encode` when PNG encoding fails.
pub fn prepare(request: &ImageEditRequest, offer: &ModelOffer) -> Result<PreparedCall, ImageEditError> {
    let model_id = request.model_id.trim();
    if offer.provider != request.provider || model_id.is_empty() || !(offer.model_id.is_empty() || offer.model_id == model_id) {
        return Err(ImageEditError::UnknownModel { model_id: model_id.to_string() });
    }
    if request.prompt.trim().is_empty() {
        return Err(ImageEditError::EmptyPrompt);
    }
    let max = offer.rule.effective_max_upscale();
    if request.upscale == 0 || request.upscale > max {
        return Err(ImageEditError::UpscaleNotAllowed { k: request.upscale, max });
    }
    let image = &request.image;
    let (width, height) = (image.width(), image.height());
    let count = pixel_count(width, height)?;
    if let Some(mask) = &request.mask
        && mask.len() != count
    {
        return Err(ImageEditError::ShapeMismatch { detail: format!("mask of {width}x{height} must be {count} bytes, got {}", mask.len()) });
    }
    let k = request.upscale;
    let overflow = || ImageEditError::ShapeMismatch { detail: format!("{width}x{height} upscaled by {k} overflows") };
    let sent_width = width.checked_mul(u32::from(k)).ok_or_else(overflow)?;
    let sent_height = height.checked_mul(u32::from(k)).ok_or_else(overflow)?;
    let size_entry = match offer.size_param {
        SizeParamStyle::AspectTier(entries) => Some(*entries.iter().find(|entry| (entry.width, entry.height) == (sent_width, sent_height)).ok_or(ImageEditError::SizeNotOffered { width: sent_width, height: sent_height })?),
        SizeParamStyle::WxH | SizeParamStyle::WStarH | SizeParamStyle::WidthHeight | SizeParamStyle::ImageSizeObject | SizeParamStyle::None => None,
    };
    let (base_url, region_id) = resolve_endpoint(request)?;

    let (w, h, factor) = (to_usize(width)?, to_usize(height)?, usize::from(k));
    let rgb: Vec<u8> = image.pixels().chunks_exact(4).flat_map(|pixel| [pixel[0], pixel[1], pixel[2]]).collect();
    let raster = |error: ms_raster::RasterError| ImageEditError::ShapeMismatch { detail: error.to_string() };
    let sent_rgb = ms_raster::upscale_replicate(&rgb, w, h, 3, factor).map_err(raster)?;
    let image_png = encode_rgb_png(&sent_rgb, sent_width, sent_height)?;
    let painted = request.mask.as_deref().filter(|mask| mask.iter().any(|&value| value != 0));
    let native_mask = match (offer.mask, painted) {
        (MaskSupport::None, _) | (MaskSupport::Soft | MaskSupport::Hard, None) => None,
        (MaskSupport::Soft | MaskSupport::Hard | MaskSupport::HardRequired, Some(mask)) => {
            let normalized: Vec<u8> = mask.iter().map(|&value| if value != 0 { 255 } else { 0 }).collect();
            Some(ms_raster::upscale_replicate(&normalized, w, h, 1, factor).map_err(raster)?)
        }
        // The model requires a mask and the whole region may change: send it all editable.
        (MaskSupport::HardRequired, None) => Some(vec![255; pixel_count(sent_width, sent_height)?]),
    };
    let model_id = if offer.model_id.is_empty() { model_id.to_string() } else { offer.model_id.to_string() };
    let call = EditCall {
        provider: request.provider,
        model_id,
        base_url,
        region_id,
        prompt: request.prompt.clone(),
        image_png,
        mask: native_mask,
        width: sent_width,
        height: sent_height,
        size_param: offer.size_param,
        size_entry,
    };
    Ok(PreparedCall { call, upscale: k, source_size: (width, height) })
}

/// Maps the provider's image `bytes` back onto the source region (steps 6-9 of the run:
/// decode and size check, downscale, composite, finish).
///
/// # Errors
/// - `Decode` when the bytes are not a decodable image within the decoder limits.
/// - `SizeMismatch` when the decoded size is not exactly the sent size (never resampled).
/// - `ShapeMismatch` when `prepared` does not belong to `request`.
/// - `SizeContractViolated` when the finished image is not the source size (a bug guard).
pub fn finish(request: &ImageEditRequest, prepared: &PreparedCall, bytes: &[u8]) -> Result<ImageEditOutcome, ImageEditError> {
    let source = &request.image;
    let source_size = (source.width(), source.height());
    if prepared.source_size != source_size {
        return Err(ImageEditError::ShapeMismatch { detail: format!("prepared call for {:?} finished with a {source_size:?} source", prepared.source_size) });
    }
    let expected = (prepared.call.width, prepared.call.height);
    let (got_width, got_height, edited) = decode_rgba(bytes)?;
    if (got_width, got_height) != expected {
        return Err(ImageEditError::SizeMismatch { expected, got: (got_width, got_height) });
    }
    let raster = |error: ms_raster::RasterError| ImageEditError::ShapeMismatch { detail: error.to_string() };
    let edited = ms_raster::downscale_box(&edited, to_usize(got_width)?, to_usize(got_height)?, 4, usize::from(prepared.upscale)).map_err(raster)?;
    let composited = composite_feathered(source, &edited, request.mask.as_deref(), request.blend)?;
    // The region is rebuilt at the source size, so a wrong buffer length is the only way the
    // finished image could disagree with it.
    let finished = RgbaRegion::new(source_size.0, source_size.1, composited).map_err(|error| ImageEditError::SizeContractViolated { expected: source_size, detail: error.to_string() })?;
    Ok(ImageEditOutcome { image: finished, upscale: prepared.upscale, sent_size: expected })
}

/// Trace-log category of the image-edit prompt (opt-in `--trace` only).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) const TRACE_CATEGORY: &str = "IMAGE_EDIT";

/// Runs one image edit end to end (steps 1-9): validates and prepares the call, sends it
/// through the adapter of the provider's API shape and the native executor, and maps the answer
/// back to exactly the source size. `key` is the provider's API key (read on the worker from
/// `image_edit::keys::read_key`; may be empty only for a provider that requires none).
/// `on_stage` receives the progress stages. Blocking network I/O: worker threads only.
///
/// # Errors
/// `KeyMissing` for a required key that is empty; `UnknownModel` and every `prepare` /
/// `finish` error; the executor's and the adapter's errors (`Cancelled`, `Timeout`, `Network`,
/// provider failures, `SizeMismatch` for an answer of another size); `RequestBuild` when the
/// adapter cannot express the call (a size style its shape has no field for, a missing
/// required mask).
#[cfg(not(target_arch = "wasm32"))]
pub fn run_image_edit(request: &ImageEditRequest, key: &str, cancel: &CancelFlag, mut on_stage: impl FnMut(ImageEditStage)) -> Result<ImageEditOutcome, ImageEditError> {
    let limits = ExecutorLimits::default();
    run_with(&UreqTransport::new(&limits), &limits, request, &RunInputs { key, cancel }, &mut on_stage)
}

/// Web stub: the browser build has no HTTP executor.
///
/// # Errors
/// Always `ImageEditError::WebUnavailable`.
#[cfg(target_arch = "wasm32")]
pub fn run_image_edit(_request: &ImageEditRequest, _key: &str, _cancel: &CancelFlag, _on_stage: impl FnMut(ImageEditStage)) -> Result<ImageEditOutcome, ImageEditError> {
    Err(ImageEditError::WebUnavailable)
}

/// The per-run inputs besides the request.
#[cfg(not(target_arch = "wasm32"))]
struct RunInputs<'a> {
    key: &'a str,
    cancel: &'a CancelFlag,
}

/// `run_image_edit` over an explicit transport and limits (tests use a scripted transport),
/// plus the one summary log line of the run.
#[cfg(not(target_arch = "wasm32"))]
fn run_with(transport: &dyn HttpTransport, limits: &ExecutorLimits, request: &ImageEditRequest, inputs: &RunInputs<'_>, on_stage: &mut dyn FnMut(ImageEditStage)) -> Result<ImageEditOutcome, ImageEditError> {
    let started = std::time::Instant::now();
    let mut sent = None;
    let result = run_steps(transport, limits, request, inputs, on_stage, &mut sent);
    let returned = match &result {
        Ok(outcome) => Some(outcome.sent_size),
        Err(ImageEditError::SizeMismatch { got, .. }) => Some(*got),
        Err(_) => None,
    };
    let size_text = |size: Option<(u32, u32)>| size.map_or_else(|| "-".to_string(), |(width, height)| format!("{width}x{height}"));
    let line = format!(
        "[AI API/image_edit] run provider={} model={} region={}x{} k={} sent={} returned={} prompt_chars={} elapsed_ms={} outcome={}",
        request.provider.key(),
        request.model_id.trim(),
        request.image.width(),
        request.image.height(),
        request.upscale,
        size_text(sent),
        size_text(returned),
        request.prompt.chars().count(),
        started.elapsed().as_millis(),
        result.as_ref().map_or_else(|error| format!("{error:?}"), |_| "ok".to_string()),
    );
    if result.is_ok() { ms_log::runtime_log::log_info(line) } else { ms_log::runtime_log::log_warn(line) }
    result
}

/// The steps of a run; `sent` receives the sent size once the call is prepared.
#[cfg(not(target_arch = "wasm32"))]
fn run_steps(transport: &dyn HttpTransport, limits: &ExecutorLimits, request: &ImageEditRequest, inputs: &RunInputs<'_>, on_stage: &mut dyn FnMut(ImageEditStage), sent: &mut Option<(u32, u32)>) -> Result<ImageEditOutcome, ImageEditError> {
    inputs.cancel.check()?;
    if request.provider.requires_key() && inputs.key.trim().is_empty() {
        return Err(ImageEditError::KeyMissing);
    }
    on_stage(ImageEditStage::Preparing);
    let offer = lookup(request.provider, &request.model_id)?;
    let protocol = protocol_for(request.provider.info().shape);
    let prepared = prepare(request, offer)?;
    *sent = Some((prepared.call.width, prepared.call.height));
    ms_log::trace_log!(TRACE_CATEGORY, "image edit {} {} prompt: {}", request.provider.key(), prepared.call.model_id, request.prompt);
    let bytes = execute(transport, limits, protocol, &RunContext { call: &prepared.call, key: inputs.key, cancel: inputs.cancel }, on_stage)?;
    // A cancel that arrived during the last request drops its (possibly billed) answer.
    inputs.cancel.check()?;
    on_stage(ImageEditStage::Compositing);
    finish(request, &prepared, &bytes)
}

/// The API base URL and region id `request.endpoint` selects at its provider.
fn resolve_endpoint(request: &ImageEditRequest) -> Result<(String, Option<&'static str>), ImageEditError> {
    let invalid = |detail: String| ImageEditError::InvalidEndpoint { detail };
    match (request.provider.info().endpoint, &request.endpoint) {
        (EndpointKind::Fixed(url), EndpointChoice::Default) => Ok((url.to_string(), None)),
        (EndpointKind::Regions(regions), EndpointChoice::Default) => regions.first().map(|region| (region.base_url.to_string(), Some(region.id))).ok_or_else(|| invalid("provider has no region".to_string())),
        (EndpointKind::Regions(regions), EndpointChoice::Region(id)) => {
            regions.iter().find(|region| region.id == *id).map(|region| (region.base_url.to_string(), Some(region.id))).ok_or_else(|| invalid(format!("unknown region {id:?}")))
        }
        (EndpointKind::UserBaseUrl, EndpointChoice::BaseUrl(url)) => {
            let target = AiApiTarget::new(AiApiService::OpenAiCompatible, url).map_err(|error| invalid(error.to_string()))?;
            let normalized = target.endpoint().ok_or_else(|| invalid("compatible target without a base URL".to_string()))?;
            Ok((normalized.trim_end_matches('/').to_string(), None))
        }
        (EndpointKind::UserBaseUrl, EndpointChoice::Default) => Err(invalid(crate::error::AiApiError::BaseUrlMissing { service: AiApiService::OpenAiCompatible }.to_string())),
        (EndpointKind::Fixed(_), EndpointChoice::Region(_) | EndpointChoice::BaseUrl(_)) | (EndpointKind::Regions(_), EndpointChoice::BaseUrl(_)) | (EndpointKind::UserBaseUrl, EndpointChoice::Region(_)) => {
            Err(invalid(format!("endpoint {:?} does not fit provider {}", request.endpoint, request.provider.key())))
        }
    }
}

/// `u32` to `usize` (lossless on every supported target).
fn to_usize(value: u32) -> Result<usize, ImageEditError> {
    usize::try_from(value).map_err(|_| ImageEditError::ShapeMismatch { detail: format!("value {value} does not fit usize") })
}

#[cfg(test)]
mod tests {
    use super::{PreparedCall, finish, prepare};
    use crate::image_edit::catalog::{ModelOffer, lookup};
    use crate::image_edit::codec::{decode_rgba, encode_rgb_png};
    use crate::image_edit::error::ImageEditError;
    use crate::image_edit::provider::ImageEditProvider;
    use crate::image_edit::request::{EndpointChoice, ImageEditRequest, MaskBlend, RgbaRegion};

    const W: u32 = 24;
    const H: u32 = 10;

    /// A deterministic opaque test pattern.
    fn pattern(width: u32, height: u32) -> RgbaRegion {
        let mut pixels = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let r = u8::try_from((x * 7 + y * 13) % 256).unwrap_or(0);
                let g = u8::try_from((x * 31 + y * 3) % 256).unwrap_or(0);
                let b = u8::try_from((x * y + 5) % 256).unwrap_or(0);
                pixels.extend_from_slice(&[r, g, b, 255]);
            }
        }
        RgbaRegion::new(width, height, pixels).unwrap_or_else(|error| panic!("{error:?}"))
    }

    fn offer(provider: ImageEditProvider, id: &str) -> &'static ModelOffer {
        lookup(provider, id).unwrap_or_else(|error| panic!("{error:?}"))
    }

    fn request(provider: ImageEditProvider, model_id: &str, mask: Option<Vec<u8>>, upscale: u8) -> ImageEditRequest {
        ImageEditRequest { provider, model_id: model_id.to_string(), endpoint: EndpointChoice::Default, prompt: "remove the text".to_string(), image: pattern(W, H), mask, blend: MaskBlend { dilate_px: 2, feather_px: 1 }, upscale }
    }

    fn prepared(request: &ImageEditRequest, offer: &ModelOffer) -> PreparedCall {
        prepare(request, offer).unwrap_or_else(|error| panic!("{error:?}"))
    }

    /// A "provider" that inverts every pixel of the image it was sent, at the sent size.
    fn invert_provider(call_png: &[u8]) -> Vec<u8> {
        let (width, height, rgba) = decode_rgba(call_png).unwrap_or_else(|error| panic!("{error:?}"));
        let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|pixel| [255 - pixel[0], 255 - pixel[1], 255 - pixel[2]]).collect();
        encode_rgb_png(&rgb, width, height).unwrap_or_else(|error| panic!("{error:?}"))
    }

    fn pixel(region: &RgbaRegion, x: u32, y: u32) -> [u8; 4] {
        let index = usize::try_from((y * region.width() + x) * 4).unwrap_or(0);
        let p = &region.pixels()[index..index + 4];
        [p[0], p[1], p[2], p[3]]
    }

    #[test]
    fn identity_provider_round_trips_bit_exactly_for_k_1_and_2() {
        for k in [1u8, 2, 4] {
            let offer = offer(ImageEditProvider::OpenAi, "gpt-image-2");
            let request = request(ImageEditProvider::OpenAi, "gpt-image-2", None, k);
            let prepared = prepared(&request, offer);
            assert_eq!((prepared.call.width, prepared.call.height), (W * u32::from(k), H * u32::from(k)));
            let outcome = finish(&request, &prepared, &prepared.call.image_png).unwrap_or_else(|error| panic!("k={k}: {error:?}"));
            assert_eq!(outcome.image, request.image, "k={k}");
            assert_eq!(outcome.sent_size, (W * u32::from(k), H * u32::from(k)));
        }
    }

    #[test]
    fn pixels_outside_the_mask_reach_are_the_source_for_k_1_and_2() {
        let mut mask = vec![0u8; usize::try_from(W * H).unwrap_or(0)];
        let painted = (5u32, 4u32);
        mask[usize::try_from(painted.1 * W + painted.0).unwrap_or(0)] = 9;
        for k in [1u8, 2] {
            let offer = offer(ImageEditProvider::OpenAi, "gpt-image-2");
            let request = request(ImageEditProvider::OpenAi, "gpt-image-2", Some(mask.clone()), k);
            let prepared = prepared(&request, offer);
            let outcome = finish(&request, &prepared, &invert_provider(&prepared.call.image_png)).unwrap_or_else(|error| panic!("k={k}: {error:?}"));
            let reach = request.blend.dilate_px + request.blend.feather_px;
            for y in 0..H {
                for x in 0..W {
                    let source = pixel(&request.image, x, y);
                    let got = pixel(&outcome.image, x, y);
                    assert_eq!(got[3], 255);
                    if x.abs_diff(painted.0) > reach || y.abs_diff(painted.1) > reach {
                        assert_eq!(got, source, "k={k} ({x},{y}) outside the reach must be untouched");
                    }
                }
            }
            // The painted pixel itself takes the edit exactly (dilate >= feather).
            let source = pixel(&request.image, painted.0, painted.1);
            assert_eq!(pixel(&outcome.image, painted.0, painted.1), [255 - source[0], 255 - source[1], 255 - source[2], 255], "k={k}");
        }
    }

    #[test]
    fn empty_mask_replaces_the_whole_region_with_alpha_255() {
        let mut image = pattern(W, H);
        // A translucent source must still come back opaque.
        let pixels: Vec<u8> = image.pixels().chunks_exact(4).flat_map(|p| [p[0], p[1], p[2], 40]).collect();
        image = RgbaRegion::new(W, H, pixels).unwrap_or_else(|error| panic!("{error:?}"));
        for mask in [None, Some(vec![0u8; usize::try_from(W * H).unwrap_or(0)])] {
            let offer = offer(ImageEditProvider::OpenAi, "gpt-image-2");
            let mut request = request(ImageEditProvider::OpenAi, "gpt-image-2", mask, 2);
            request.image = image.clone();
            let prepared = prepared(&request, offer);
            assert!(prepared.call.mask.is_none(), "an unpainted Soft mask is not sent");
            let outcome = finish(&request, &prepared, &invert_provider(&prepared.call.image_png)).unwrap_or_else(|error| panic!("{error:?}"));
            for (got, source) in outcome.image.pixels().chunks_exact(4).zip(image.pixels().chunks_exact(4)) {
                assert_eq!(got, [255 - source[0], 255 - source[1], 255 - source[2], 255]);
            }
        }
    }

    #[test]
    fn a_wrong_output_size_is_a_size_mismatch_never_a_resample() {
        let offer = offer(ImageEditProvider::OpenAi, "gpt-image-2");
        for k in [1u8, 2] {
            let request = request(ImageEditProvider::OpenAi, "gpt-image-2", None, k);
            let prepared = prepared(&request, offer);
            let (sw, sh) = (prepared.call.width, prepared.call.height);
            for (width, height) in [(sw + 1, sh), (sw, sh - 1), (sw * 2, sh * 2)] {
                let bytes = encode_rgb_png(&vec![0; usize::try_from(width * height * 3).unwrap_or(0)], width, height).unwrap_or_default();
                match finish(&request, &prepared, &bytes) {
                    Err(ImageEditError::SizeMismatch { expected, got }) => {
                        assert_eq!(expected, (sw, sh));
                        assert_eq!(got, (width, height));
                    }
                    other => panic!("k={k} {width}x{height}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn native_masks_follow_the_offer() {
        let count = usize::try_from(W * H).unwrap_or(0);
        let mut mask = vec![0u8; count];
        mask[3] = 1;
        // Soft: painted -> sent normalized and upscaled.
        let soft = prepared(&request(ImageEditProvider::OpenAi, "gpt-image-2", Some(mask.clone()), 2), offer(ImageEditProvider::OpenAi, "gpt-image-2"));
        let sent = soft.call.mask.unwrap_or_default();
        assert_eq!(sent.len(), count * 4);
        // One painted source pixel becomes a k x k block of editable pixels.
        let editable: usize = sent.iter().map(|&value| usize::from(value == 255)).sum();
        assert_eq!(editable, 4);
        assert!(sent.iter().all(|&value| value == 0 || value == 255));
        // None: never sent.
        let none = prepared(&request(ImageEditProvider::Bfl, "flux-2-pro", Some(mask.clone()), 1), offer(ImageEditProvider::Bfl, "flux-2-pro"));
        assert!(none.call.mask.is_none());
        // HardRequired with nothing painted: a full editable mask.
        let fill = prepared(&request(ImageEditProvider::Bfl, "flux-pro-1.0-fill", None, 1), offer(ImageEditProvider::Bfl, "flux-pro-1.0-fill"));
        assert_eq!(fill.call.mask, Some(vec![255; count]));
        assert_eq!(fill.call.base_url, "https://api.bfl.ai");
        assert_eq!(fill.call.region_id, Some("global"));
    }

    #[test]
    fn request_validation_errors() {
        let gpt = offer(ImageEditProvider::OpenAi, "gpt-image-2");
        let mut blank = request(ImageEditProvider::OpenAi, "gpt-image-2", None, 1);
        blank.prompt = " \n".to_string();
        assert!(matches!(prepare(&blank, gpt), Err(ImageEditError::EmptyPrompt)));
        assert!(matches!(prepare(&request(ImageEditProvider::OpenAi, "gpt-image-2", None, 0), gpt), Err(ImageEditError::UpscaleNotAllowed { k: 0, max: 4 })));
        assert!(matches!(prepare(&request(ImageEditProvider::OpenAi, "gpt-image-2", None, 5), gpt), Err(ImageEditError::UpscaleNotAllowed { k: 5, max: 4 })));
        let ideogram = offer(ImageEditProvider::Ideogram, "ideogram-4-5");
        assert!(matches!(prepare(&request(ImageEditProvider::Ideogram, "ideogram-4-5", None, 2), ideogram), Err(ImageEditError::UpscaleNotAllowed { k: 2, max: 1 })));
        assert!(matches!(prepare(&request(ImageEditProvider::OpenAi, "gpt-image-2", Some(vec![0; 5]), 1), gpt), Err(ImageEditError::ShapeMismatch { .. })));
        assert!(matches!(prepare(&request(ImageEditProvider::OpenAi, "gpt-image-1.5", None, 1), gpt), Err(ImageEditError::UnknownModel { .. })));
        assert!(matches!(prepare(&request(ImageEditProvider::Fal, "gpt-image-2", None, 1), gpt), Err(ImageEditError::UnknownModel { .. })));
    }

    #[test]
    fn table_offers_need_a_table_size_and_carry_its_labels() {
        let flash = offer(ImageEditProvider::Gemini, "gemini-3.1-flash-image");
        assert!(matches!(prepare(&request(ImageEditProvider::Gemini, "gemini-3.1-flash-image", None, 1), flash), Err(ImageEditError::SizeNotOffered { width: W, height: H })));
        let mut table_sized = request(ImageEditProvider::Gemini, "gemini-3.1-flash-image", None, 2);
        // 300x224 upscaled by 2 is the 4:3 @ 512 entry, 600x448.
        table_sized.image = pattern(300, 224);
        let call = prepared(&table_sized, flash).call;
        let entry = call.size_entry.map(|entry| (entry.aspect, entry.tier, entry.width, entry.height));
        assert_eq!(entry, Some(("4:3", "512", 600, 448)));
    }

    #[test]
    fn endpoints_resolve_per_provider() {
        let bfl = offer(ImageEditProvider::Bfl, "flux-2-pro");
        let mut eu = request(ImageEditProvider::Bfl, "flux-2-pro", None, 1);
        eu.endpoint = EndpointChoice::Region("eu");
        assert_eq!(prepared(&eu, bfl).call.base_url, "https://api.eu.bfl.ai");
        eu.endpoint = EndpointChoice::Region("mars");
        assert!(matches!(prepare(&eu, bfl), Err(ImageEditError::InvalidEndpoint { .. })));
        eu.endpoint = EndpointChoice::BaseUrl("https://example.com".to_string());
        assert!(matches!(prepare(&eu, bfl), Err(ImageEditError::InvalidEndpoint { .. })));

        let own = offer(ImageEditProvider::OpenAiCompatible, "sd-model");
        let mut local = request(ImageEditProvider::OpenAiCompatible, " sd-model ", None, 1);
        assert!(matches!(prepare(&local, own), Err(ImageEditError::InvalidEndpoint { .. })));
        local.endpoint = EndpointChoice::BaseUrl("http://127.0.0.1:1234".to_string());
        let call = prepared(&local, own).call;
        assert_eq!(call.base_url, "http://127.0.0.1:1234/v1");
        assert_eq!(call.model_id, "sd-model");
        local.endpoint = EndpointChoice::BaseUrl("ftp://x".to_string());
        assert!(matches!(prepare(&local, own), Err(ImageEditError::InvalidEndpoint { .. })));
    }

    /// End-to-end runs over a scripted transport (no network): adapter + executor + finish.
    #[cfg(not(target_arch = "wasm32"))]
    mod run {
        use super::{H, W, prepared, request};
        use crate::encoding::base64_encode;
        use crate::image_edit::catalog::lookup;
        use crate::image_edit::codec::encode_rgb_png;
        use crate::image_edit::error::ImageEditError;
        use crate::image_edit::executor::tests::{FakeTransport, instant_limits, resp};
        use crate::image_edit::pipeline::{RunInputs, run_with};
        use crate::image_edit::provider::ImageEditProvider;
        use crate::image_edit::request::{CancelFlag, ImageEditRequest, ImageEditStage};

        fn images_answer(png: &[u8]) -> String {
            format!("{{\"data\":[{{\"b64_json\":\"{}\"}}]}}", base64_encode(png))
        }

        fn run(transport: &FakeTransport, request: &ImageEditRequest, key: &str, cancel: &CancelFlag) -> (Result<crate::image_edit::request::ImageEditOutcome, ImageEditError>, Vec<ImageEditStage>) {
            let mut stages = Vec::new();
            let result = run_with(transport, &instant_limits(), request, &RunInputs { key, cancel }, &mut |stage| stages.push(stage));
            (result, stages)
        }

        #[test]
        fn an_exact_size_answer_round_trips_to_the_source_size() {
            let request = request(ImageEditProvider::OpenAi, "gpt-image-2", None, 2);
            let offer = lookup(ImageEditProvider::OpenAi, "gpt-image-2").unwrap_or_else(|error| panic!("{error:?}"));
            // The identity "provider" answers with exactly the image it was sent.
            let sent_png = prepared(&request, offer).call.image_png;
            let transport = FakeTransport::new(vec![Ok(resp(200, images_answer(&sent_png).as_bytes()))]);
            let (result, stages) = run(&transport, &request, "sk-test", &CancelFlag::new());
            let outcome = result.unwrap_or_else(|error| panic!("{error:?}"));
            assert_eq!(outcome.image, request.image);
            assert_eq!((outcome.image.width(), outcome.image.height()), (W, H));
            assert_eq!(outcome.sent_size, (2 * W, 2 * H));
            assert_eq!(stages, [ImageEditStage::Preparing, ImageEditStage::Sending, ImageEditStage::Compositing]);
            let sent = transport.sent.borrow();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].url, "https://api.openai.com/v1/images/edits");
        }

        #[test]
        fn a_wrong_size_answer_is_a_size_mismatch() {
            let request = request(ImageEditProvider::OpenAi, "gpt-image-2", None, 1);
            let other = encode_rgb_png(&vec![0; usize::try_from((W + 16) * H * 3).unwrap_or(0)], W + 16, H).unwrap_or_default();
            let transport = FakeTransport::new(vec![Ok(resp(200, images_answer(&other).as_bytes()))]);
            let (result, _) = run(&transport, &request, "sk-test", &CancelFlag::new());
            assert!(matches!(result, Err(ImageEditError::SizeMismatch { expected, got }) if expected == (W, H) && got == (W + 16, H)));
        }

        #[test]
        fn a_missing_required_key_sends_nothing() {
            let request = request(ImageEditProvider::OpenAi, "gpt-image-2", None, 1);
            let transport = FakeTransport::new(Vec::new());
            let (result, _) = run(&transport, &request, "  ", &CancelFlag::new());
            assert!(matches!(result, Err(ImageEditError::KeyMissing)));
            assert!(transport.sent.borrow().is_empty());
        }

        #[test]
        fn a_cancel_during_the_request_drops_its_answer() {
            let request = request(ImageEditProvider::OpenAi, "gpt-image-2", None, 1);
            let offer = lookup(ImageEditProvider::OpenAi, "gpt-image-2").unwrap_or_else(|error| panic!("{error:?}"));
            let sent_png = prepared(&request, offer).call.image_png;
            let cancel = CancelFlag::new();
            let mut transport = FakeTransport::new(vec![Ok(resp(200, images_answer(&sent_png).as_bytes()))]);
            transport.cancel_after = Some((1, cancel.clone()));
            let (result, _) = run(&transport, &request, "sk-test", &cancel);
            assert!(matches!(result, Err(ImageEditError::Cancelled)));
        }
    }
}
