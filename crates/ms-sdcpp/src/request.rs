/*
File: crates/ms-sdcpp/src/request.rs

Purpose:
The model-family-agnostic request and report types of the sd.cpp runtime: checked image
buffers, which model family and files to load, how to create the context, one generation
request, the job that bundles them, run progress events, and the device / probe reports.

Key items:
- RgbImage / GrayImage / ImageError : length-checked 8-bit buffers (serde validates too)
- ModelFamily / ComponentRole / ModelFiles : what to load (FLUX.2 klein, FLUX.3 klein)
- DeviceSelection / FlashAttention / ContextOptions : context creation options
- GenerateRequest / GenerateJob : one generation and everything needed to run it
- RunStage / RunEvent : progress reported while a job runs
- DeviceInfo / ProbeReport : what a probe of an installed build found

Notes:
Pure data (serde). No I/O, no validation of model files (the family mapping and header probes
live with the bindings). Sizes are pixels; images are row-major, top row first, no padding.
*/

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::catalog::SdCppBuild;

/// A buffer length does not match its declared dimensions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    /// `width * height * channels` overflows `usize` or is not `actual`.
    #[error("image buffer of {actual} bytes does not match {width}x{height} with {channels} channel(s)")]
    LengthMismatch { width: u32, height: u32, channels: u8, actual: usize },
}

/// Checks that `len == width * height * channels` (overflow-safe).
fn check_len(width: u32, height: u32, channels: u8, len: usize) -> Result<(), ImageError> {
    let expected = usize::try_from(width).ok().and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h))).and_then(|area| area.checked_mul(usize::from(channels)));
    if expected == Some(len) { Ok(()) } else { Err(ImageError::LengthMismatch { width, height, channels, actual: len }) }
}

/// Serialized shape of an image before its length check.
#[derive(Deserialize)]
struct RawImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// An 8-bit RGB image, `width * height * 3` bytes, rows top to bottom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawImage")]
pub struct RgbImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl RgbImage {
    /// Wraps `pixels` as a `width` x `height` RGB image.
    ///
    /// # Errors
    /// [`ImageError::LengthMismatch`] when `pixels.len() != width * height * 3`.
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self, ImageError> {
        check_len(width, height, 3, pixels.len())?;
        Ok(Self { width, height, pixels })
    }

    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The RGB bytes.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Takes the RGB bytes.
    #[must_use]
    pub fn into_pixels(self) -> Vec<u8> {
        self.pixels
    }
}

impl TryFrom<RawImage> for RgbImage {
    type Error = ImageError;

    fn try_from(raw: RawImage) -> Result<Self, Self::Error> {
        Self::new(raw.width, raw.height, raw.pixels)
    }
}

/// An 8-bit single-channel image (a mask: 0 = keep, 255 = repaint), `width * height` bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawImage")]
pub struct GrayImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl GrayImage {
    /// Wraps `pixels` as a `width` x `height` single-channel image.
    ///
    /// # Errors
    /// [`ImageError::LengthMismatch`] when `pixels.len() != width * height`.
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self, ImageError> {
        check_len(width, height, 1, pixels.len())?;
        Ok(Self { width, height, pixels })
    }

    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The bytes.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Takes the bytes.
    #[must_use]
    pub fn into_pixels(self) -> Vec<u8> {
        self.pixels
    }
}

impl TryFrom<RawImage> for GrayImage {
    type Error = ImageError;

    fn try_from(raw: RawImage) -> Result<Self, Self::Error> {
        Self::new(raw.width, raw.height, raw.pixels)
    }
}

/// The model family a job loads; decides how its files map onto the context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelFamily {
    /// FLUX.2 klein (4B / 9B): diffusion transformer + VAE + Qwen3 text encoder.
    Flux2Klein,
    /// FLUX.3 klein: diffusion transformer + VAE + Qwen3-VL text encoder.
    Flux3Klein,
}

/// The role of one model file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ComponentRole {
    /// The diffusion transformer.
    Diffusion,
    /// The VAE.
    Vae,
    /// The LLM text encoder.
    Llm,
}

/// The files of one model: absolute paths keyed by role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFiles {
    /// The family the files belong to.
    pub family: ModelFamily,
    /// One absolute path per role (a single file, a GGUF, or a sharded index JSON).
    pub components: BTreeMap<ComponentRole, PathBuf>,
}

/// Which compute device the context uses.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DeviceSelection {
    /// sd.cpp's automatic placement.
    #[default]
    Auto,
    /// A device by the name `sd_list_devices` reports (`Vulkan1`), with its description for
    /// diagnostics. Never an index: index 0 is often an integrated GPU.
    Named { name: String, description: String },
}

/// Flash-attention mode of the context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FlashAttention {
    /// sd.cpp decides.
    #[default]
    Auto,
    /// Forced on.
    On,
    /// Forced off.
    Off,
}

/// How the context is created (`new_sd_ctx`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextOptions {
    /// The compute device.
    pub device: DeviceSelection,
    /// Run the text encoder on the CPU (`te=cpu`).
    pub text_encoder_on_cpu: bool,
    /// Keep weights in RAM and stream them to the device (`*=cpu` params backend).
    pub weights_in_ram: bool,
    /// Device memory budget in GiB; `None` = no limit.
    pub max_vram_gib: Option<f32>,
    /// Flash-attention mode.
    pub flash_attention: FlashAttention,
    /// CPU thread count; `None` = sd.cpp's default (physical cores).
    pub threads: Option<u32>,
}

/// One image generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerateRequest {
    /// Positive prompt.
    pub prompt: String,
    /// Negative prompt (empty = none).
    pub negative_prompt: String,
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
    /// Image-to-image source; `None` = text-to-image.
    pub init_image: Option<RgbImage>,
    /// Inpainting mask over `init_image` (255 = repaint).
    pub mask: Option<GrayImage>,
    /// Reference images (edit models), sent at their final size.
    pub references: Vec<RgbImage>,
    /// Denoising strength for image-to-image, 0..=1.
    pub strength: f32,
    /// Sampling steps.
    pub steps: u32,
    /// Classifier-free guidance scale.
    pub cfg_scale: f32,
    /// Seed (negative = random, as in sd.cpp).
    pub seed: i64,
    /// Sampler name; `None` = the model's default.
    pub sample_method: Option<String>,
    /// Scheduler name; `None` = the model's default.
    pub scheduler: Option<String>,
    /// Decode the VAE in tiles (less memory).
    pub vae_tiling: bool,
}

/// Everything one run needs: which build, which files, how to create the context, what to
/// generate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerateJob {
    /// The installed build to run.
    pub build: SdCppBuild,
    /// The model files.
    pub files: ModelFiles,
    /// Context creation options.
    pub context: ContextOptions,
    /// The generation.
    pub request: GenerateRequest,
}

/// Coarse stage of a running job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStage {
    /// The worker process is starting and loading the library.
    StartingWorker,
    /// The context is being created (model files read).
    LoadingModel,
    /// Sampling and decoding.
    Generating,
}

/// A progress event of a running job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunEvent {
    /// A new stage began.
    Stage(RunStage),
    /// Sampling step `step` of `steps` finished.
    Step { step: u32, steps: u32 },
}

/// One compute device sd.cpp reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Device name (`Vulkan1`, `CPU`), the key of [`DeviceSelection::Named`].
    pub name: String,
    /// Human-readable description (GPU model).
    pub description: String,
}

/// What a probe of an installed build found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeReport {
    /// `sd_version()`.
    pub sd_version: String,
    /// `sd_commit()`.
    pub sd_commit: String,
    /// The devices `sd_list_devices` reports.
    pub devices: Vec<DeviceInfo>,
    /// `sd_get_num_physical_cores()`.
    pub physical_cores: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_image_checks_length() {
        assert!(RgbImage::new(2, 3, vec![0; 18]).is_ok());
        assert_eq!(RgbImage::new(2, 3, vec![0; 17]), Err(ImageError::LengthMismatch { width: 2, height: 3, channels: 3, actual: 17 }));
        assert!(RgbImage::new(0, 0, Vec::new()).is_ok());
        assert!(RgbImage::new(u32::MAX, u32::MAX, vec![0; 3]).is_err());
    }

    #[test]
    fn gray_image_checks_length() {
        assert!(GrayImage::new(4, 2, vec![255; 8]).is_ok());
        assert!(GrayImage::new(4, 2, vec![255; 24]).is_err());
    }

    #[test]
    fn deserialization_rejects_a_bad_length() {
        let good: RgbImage = serde_json::from_str(r#"{"width":1,"height":1,"pixels":[1,2,3]}"#).expect("valid image");
        assert_eq!(good.pixels(), &[1, 2, 3]);
        assert!(serde_json::from_str::<RgbImage>(r#"{"width":1,"height":1,"pixels":[1,2]}"#).is_err());
        assert!(serde_json::from_str::<GrayImage>(r#"{"width":2,"height":1,"pixels":[1]}"#).is_err());
    }

    #[test]
    fn job_round_trips_through_json() {
        let mut components = BTreeMap::new();
        components.insert(ComponentRole::Diffusion, PathBuf::from("/m/dit.gguf"));
        components.insert(ComponentRole::Vae, PathBuf::from("/m/vae.safetensors"));
        components.insert(ComponentRole::Llm, PathBuf::from("/m/te.gguf"));
        let job = GenerateJob {
            build: SdCppBuild::Vulkan,
            files: ModelFiles { family: ModelFamily::Flux3Klein, components },
            context: ContextOptions {
                device: DeviceSelection::Named { name: "Vulkan1".into(), description: "GPU".into() },
                text_encoder_on_cpu: true,
                weights_in_ram: false,
                max_vram_gib: Some(8.0),
                flash_attention: FlashAttention::On,
                threads: None,
            },
            request: GenerateRequest {
                prompt: "p".into(),
                negative_prompt: String::new(),
                width: 2,
                height: 1,
                init_image: Some(RgbImage::new(2, 1, vec![0; 6]).expect("2x1 rgb")),
                mask: Some(GrayImage::new(2, 1, vec![0, 255]).expect("2x1 mask")),
                references: vec![],
                strength: 1.0,
                steps: 4,
                cfg_scale: 1.0,
                seed: 7,
                sample_method: None,
                scheduler: None,
                vae_tiling: false,
            },
        };
        let text = serde_json::to_string(&job).expect("serialize job");
        let back: GenerateJob = serde_json::from_str(&text).expect("deserialize job");
        assert_eq!(back, job);
    }
}
