/*
File: crates/ms-ai-api/src/image_edit/request.rs

Purpose:
The inputs and outputs of one image-edit run: the validated RGBA region, the request (provider,
model, endpoint choice, prompt, mask, blend, upscale factor), the finished outcome, the
progress stages and the cancel flag shared with the worker.

Key structures:
- RgbaRegion (validated constructor)
- ImageEditRequest, EndpointChoice, MaskBlend
- ImageEditOutcome, ImageEditStage, CancelFlag

Notes:
Pure and target-neutral. The mask convention is the cleaning tool's: one byte per pixel,
nonzero = "may change here"; `None` or an all-zero mask means the whole region.
*/

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::error::ImageEditError;
use super::provider::ImageEditProvider;

/// An RGBA8 raster (straight alpha, row-major) whose buffer length is checked against its
/// size at construction, so every holder can rely on `pixels.len() == width * height * 4`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaRegion {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl RgbaRegion {
    /// Wraps `pixels` as a `width x height` RGBA8 raster.
    ///
    /// # Errors
    /// `ImageEditError::ShapeMismatch` for a zero side, a size whose byte length overflows, or
    /// a buffer of another length.
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self, ImageEditError> {
        let expected = rgba_len(width, height)?;
        if pixels.len() != expected {
            return Err(ImageEditError::ShapeMismatch { detail: format!("RGBA buffer of {width}x{height} must be {expected} bytes, got {}", pixels.len()) });
        }
        Ok(Self { width, height, pixels })
    }

    /// Width in pixels (>= 1).
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels (>= 1).
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The RGBA8 bytes, `width * height * 4` long.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Consumes the region, returning its RGBA8 bytes.
    #[must_use]
    pub fn into_pixels(self) -> Vec<u8> {
        self.pixels
    }
}

/// `width * height` as `usize`, rejecting a zero side and overflow.
///
/// # Errors
/// `ImageEditError::ShapeMismatch`.
pub(crate) fn pixel_count(width: u32, height: u32) -> Result<usize, ImageEditError> {
    let bad = || ImageEditError::ShapeMismatch { detail: format!("raster size {width}x{height} is empty or too large") };
    if width == 0 || height == 0 {
        return Err(bad());
    }
    let width = usize::try_from(width).map_err(|_| bad())?;
    let height = usize::try_from(height).map_err(|_| bad())?;
    width.checked_mul(height).ok_or_else(bad)
}

/// Byte length of a `width x height` RGBA8 buffer.
///
/// # Errors
/// `ImageEditError::ShapeMismatch` as `pixel_count`.
pub(crate) fn rgba_len(width: u32, height: u32) -> Result<usize, ImageEditError> {
    pixel_count(width, height)?.checked_mul(4).ok_or_else(|| ImageEditError::ShapeMismatch { detail: format!("RGBA buffer of {width}x{height} overflows") })
}

/// Which of the provider's endpoints to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointChoice {
    /// The provider's single base URL (or its first region).
    Default,
    /// A region of a provider with a region list, by its frozen id.
    Region(&'static str),
    /// The user's own server base URL (as typed; normalized by the executor).
    BaseUrl(String),
}

/// How the edited pixels are blended back inside the mask: the mask is dilated by
/// `dilate_px`, then box-blurred by `feather_px` into the blend alpha. Pixels farther than
/// `dilate_px + feather_px` from every painted pixel stay bit-identical to the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaskBlend {
    /// Square dilation radius of the painted mask, in source pixels.
    pub dilate_px: u32,
    /// Box-blur radius of the dilated mask, in source pixels.
    pub feather_px: u32,
}

impl Default for MaskBlend {
    fn default() -> Self {
        Self { dilate_px: 4, feather_px: 4 }
    }
}

/// One image-edit run, as the cleaning tool hands it to the worker.
#[derive(Debug, Clone)]
pub struct ImageEditRequest {
    /// The provider to call.
    pub provider: ImageEditProvider,
    /// The model id (see `catalog::lookup`).
    pub model_id: String,
    /// Which endpoint of the provider.
    pub endpoint: EndpointChoice,
    /// The edit instruction; must not be blank. Never logged (length only).
    pub prompt: String,
    /// The source region.
    pub image: RgbaRegion,
    /// `width * height` bytes, nonzero = may change; `None` or all zero = the whole region.
    pub mask: Option<Vec<u8>>,
    /// How the result is blended back.
    pub blend: MaskBlend,
    /// The integer upscale `k` the caller chose (the cleaning frame's `upscale_factor_for`);
    /// `1..=offer.rule.max_upscale`. The pipeline never re-decides it.
    pub upscale: u8,
}

/// A finished run.
#[derive(Debug, Clone)]
pub struct ImageEditOutcome {
    /// The edited region: exactly the source size, alpha 255 everywhere.
    pub image: RgbaRegion,
    /// The upscale factor that was applied.
    pub upscale: u8,
    /// The size sent to (and returned by) the provider, `(k * W, k * H)`.
    pub sent_size: (u32, u32),
}

/// Progress of a run, reported to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageEditStage {
    /// Validating, upscaling and encoding.
    Preparing,
    /// The paid request is being sent.
    Sending,
    /// Waiting for an asynchronous provider; `polls` status requests so far.
    Waiting { polls: u32 },
    /// Downloading the result.
    Downloading,
    /// Decoding, downscaling and compositing.
    Compositing,
}

/// A cancel request shared between the UI and the worker. Cloning shares the flag. Checked
/// between steps: an HTTP call already in flight is not aborted (its result is dropped and may
/// still be billed).
#[derive(Debug, Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    /// A flag that is not cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation (idempotent).
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// `Err(Cancelled)` once cancellation was requested, for `?` between steps.
    ///
    /// # Errors
    /// `ImageEditError::Cancelled`.
    pub fn check(&self) -> Result<(), ImageEditError> {
        if self.is_cancelled() { Err(ImageEditError::Cancelled) } else { Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use super::{CancelFlag, ImageEditError, RgbaRegion, rgba_len};

    #[test]
    fn rgba_region_checks_its_shape() {
        assert!(RgbaRegion::new(2, 3, vec![0; 24]).is_ok());
        assert!(matches!(RgbaRegion::new(2, 3, vec![0; 23]), Err(ImageEditError::ShapeMismatch { .. })));
        assert!(matches!(RgbaRegion::new(0, 3, Vec::new()), Err(ImageEditError::ShapeMismatch { .. })));
        // (2^32 - 1)^2 * 4 bytes overflows `usize` on every supported target.
        assert!(matches!(rgba_len(u32::MAX, u32::MAX), Err(ImageEditError::ShapeMismatch { .. })));
    }

    #[test]
    fn cancel_flag_is_shared_by_clones() {
        let flag = CancelFlag::new();
        let worker = flag.clone();
        assert!(worker.check().is_ok());
        flag.cancel();
        assert!(worker.is_cancelled());
        assert!(matches!(worker.check(), Err(ImageEditError::Cancelled)));
    }
}
