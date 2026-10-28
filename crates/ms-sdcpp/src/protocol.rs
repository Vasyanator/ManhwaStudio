/*
File: crates/ms-sdcpp/src/protocol.rs

Purpose:
The wire between the app and the sd.cpp worker process: typed requests (app -> worker) and
events (worker -> app), each one frame of `ms_backend_ipc::frame`
(`[u32 BE header_len][header JSON][u32 BE blob_len][blob]`, header <= 1 MiB, blob <= 32 MiB),
plus the split of a `GenerateRequest` into a header and one image frame per image.

Key items:
- WORKER_PROTOCOL           : the version both sides compare in `Init` / `Ready`
- Request / Event            : the messages (internally tagged by `type`, so a header is
                               always a JSON object)
- GenerateHeader / ImageSlot / SlotKind : a generation without its pixels, and where each
                               image frame belongs
- WireError                  : the worker-side `SdCppError` variants, lossless both ways
- write_message() / read_message() : one message + blob per frame
- split_request() / assemble_request() : `GenerateRequest` <-> header + image frames

Notes:
- Both ends are the same binary (the worker is `current_exe() --sdcpp-worker`), so the
  protocol number guards only against a stale worker of another build being reused; any
  change of a message's meaning bumps `WORKER_PROTOCOL`.
- An image travels as raw bytes (RGB for init / references / the result, L8 for the mask),
  one per `Image` frame after `Generate`, each <= 32 MiB.
*/

use std::io::{Read, Write};
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::catalog::SdCppBuild;
use crate::error::{LoadProblem, SdCppError};
use crate::request::{ContextOptions, GenerateRequest, GrayImage, ModelFiles, ProbeReport, RgbImage, RunStage};

/// Version of this wire protocol.
pub const WORKER_PROTOCOL: u32 = 1;

/// Where an image frame belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotKind {
    /// `GenerateRequest::init_image` (RGB).
    Init,
    /// `GenerateRequest::mask` (L8).
    Mask,
    /// `GenerateRequest::references[index]` (RGB).
    Reference { index: u32 },
}

impl SlotKind {
    /// Bytes per pixel of this slot's image.
    #[must_use]
    pub const fn channels(self) -> u8 {
        match self {
            Self::Init | Self::Reference { .. } => 3,
            Self::Mask => 1,
        }
    }
}

/// One image frame's place and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSlot {
    /// Which image.
    pub kind: SlotKind,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// A `GenerateRequest` without its pixels (same fields, same meaning).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerateHeader {
    pub prompt: String,
    pub negative_prompt: String,
    pub width: u32,
    pub height: u32,
    pub strength: f32,
    pub steps: u32,
    pub cfg_scale: f32,
    pub seed: i64,
    pub sample_method: Option<String>,
    pub scheduler: Option<String>,
    pub vae_tiling: bool,
}

/// The payload of `Request::Generate`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerateMessage {
    /// The (validated) model files.
    pub files: ModelFiles,
    /// Context creation options.
    pub context: ContextOptions,
    /// The generation without its pixels.
    pub request: GenerateHeader,
    /// The image frames that follow, in order.
    pub images: Vec<ImageSlot>,
}

/// App -> worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    /// Load the package in `package_dir` (build `build`); answered by `Ready` or `Error`.
    Init { protocol: u32, package_dir: PathBuf, build: SdCppBuild },
    /// Report version, commit, devices and cores; answered by `ProbeResult`.
    Probe,
    /// Generate one image; followed by one `Image` frame per entry of `images`, in order.
    /// Boxed: it is much larger than every other request.
    Generate(Box<GenerateMessage>),
    /// One image of the preceding `Generate` (the frame's blob holds its bytes).
    Image { slot: ImageSlot },
    /// Cancel the running generation; answered by `Cancelled` (or its normal end).
    Cancel,
    /// Free the context (the model); answered by `Unloaded`.
    Unload,
    /// Exit the worker.
    Shutdown,
}

/// Severity of a forwarded library log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogLevel {
    /// `SD_LOG_INFO`.
    Info,
    /// `SD_LOG_WARN`.
    Warn,
    /// `SD_LOG_ERROR`.
    Error,
}

/// The worker-side failures, lossless twins of the matching `SdCppError` variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code")]
pub enum WireError {
    /// `SdCppError::LibraryLoad`.
    LibraryLoad { problem: LoadProblem, detail: String },
    /// `SdCppError::AbiMismatch`.
    AbiMismatch { expected: String, found: String },
    /// `SdCppError::DeviceNotFound`.
    DeviceNotFound { name: String },
    /// `SdCppError::ContextCreate`.
    ContextCreate { detail: String },
    /// `SdCppError::Generation`.
    Generation { detail: String },
    /// `SdCppError::OutputSize`.
    OutputSize { expected: (u32, u32), found: (u32, u32) },
    /// `SdCppError::Protocol` (also every app-side error a worker should never produce).
    Protocol { detail: String },
}

impl WireError {
    /// The wire form of `err`. App-side variants (install, files, platform, …) are not
    /// expected from the worker; they become `Protocol` with their English debug text.
    #[must_use]
    pub fn from_error(err: &SdCppError) -> Self {
        match err {
            SdCppError::LibraryLoad { problem, detail } => Self::LibraryLoad { problem: problem.clone(), detail: detail.clone() },
            SdCppError::AbiMismatch { expected, found } => Self::AbiMismatch { expected: expected.clone(), found: found.clone() },
            SdCppError::DeviceNotFound { name } => Self::DeviceNotFound { name: name.clone() },
            SdCppError::ContextCreate { detail } => Self::ContextCreate { detail: detail.clone() },
            SdCppError::Generation { detail } => Self::Generation { detail: detail.clone() },
            SdCppError::OutputSize { expected, found } => Self::OutputSize { expected: *expected, found: *found },
            SdCppError::Protocol { detail } => Self::Protocol { detail: detail.clone() },
            SdCppError::Unavailable(_)
            | SdCppError::NoBuildHere { .. }
            | SdCppError::NotInstalled { .. }
            | SdCppError::Requirement(_)
            | SdCppError::Package { .. }
            | SdCppError::Cancelled
            | SdCppError::Busy
            | SdCppError::GenerationCancelled
            | SdCppError::GenerationBusy
            | SdCppError::NoBuildForPlatform
            | SdCppError::WorkerStillRunning
            | SdCppError::Files(_)
            | SdCppError::WorkerSpawn { .. }
            | SdCppError::WorkerCrashed { .. } => Self::Protocol { detail: format!("unexpected worker error {err:?}") },
        }
    }
}

impl From<WireError> for SdCppError {
    fn from(err: WireError) -> Self {
        match err {
            WireError::LibraryLoad { problem, detail } => Self::LibraryLoad { problem, detail },
            WireError::AbiMismatch { expected, found } => Self::AbiMismatch { expected, found },
            WireError::DeviceNotFound { name } => Self::DeviceNotFound { name },
            WireError::ContextCreate { detail } => Self::ContextCreate { detail },
            WireError::Generation { detail } => Self::Generation { detail },
            WireError::OutputSize { expected, found } => Self::OutputSize { expected, found },
            WireError::Protocol { detail } => Self::Protocol { detail },
        }
    }
}

/// Worker -> app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    /// The libraries are loaded (answer to `Init`).
    Ready { protocol: u32, sd_version: String, sd_commit: String },
    /// Answer to `Probe`.
    ProbeResult { report: ProbeReport },
    /// A generation stage began.
    Stage { stage: RunStage },
    /// Sampling step `step` of `steps` finished.
    Step { step: u32, steps: u32 },
    /// A library log line.
    Log { level: LogLevel, text: String },
    /// The generated image (`width` x `height` RGB) is this frame's blob.
    Result { width: u32, height: u32 },
    /// A request failed.
    Error { error: WireError },
    /// The generation was cancelled.
    Cancelled,
    /// The context was freed.
    Unloaded,
}

/// Writes `message` with `blob` as one frame.
///
/// # Errors
/// A message when serialization fails, the header or blob exceeds its frame limit, or the
/// write fails.
pub fn write_message<M: Serialize, W: Write>(writer: &mut W, message: &M, blob: &[u8]) -> Result<(), String> {
    let header = serde_json::to_value(message).map_err(|err| format!("serialize worker message: {err}"))?;
    if !header.is_object() {
        return Err("worker message header is not a JSON object".to_owned());
    }
    ms_backend_ipc::write_frame(writer, &header, blob)
}

/// Reads one frame as a message plus its blob.
///
/// # Errors
/// A message on a read failure, EOF, a frame over its limits, or a header that is not a
/// message of type `M`.
pub fn read_message<M: DeserializeOwned, R: Read>(reader: &mut R) -> Result<(M, Vec<u8>), String> {
    let frame = ms_backend_ipc::read_frame(reader)?;
    let message = serde_json::from_value(frame.header).map_err(|err| format!("decode worker message: {err}"))?;
    Ok((message, frame.blob))
}

/// Splits `request` into its header and its image frames (init, mask, references in order).
#[must_use]
pub fn split_request(request: &GenerateRequest) -> (GenerateHeader, Vec<(ImageSlot, &[u8])>) {
    let header = GenerateHeader {
        prompt: request.prompt.clone(),
        negative_prompt: request.negative_prompt.clone(),
        width: request.width,
        height: request.height,
        strength: request.strength,
        steps: request.steps,
        cfg_scale: request.cfg_scale,
        seed: request.seed,
        sample_method: request.sample_method.clone(),
        scheduler: request.scheduler.clone(),
        vae_tiling: request.vae_tiling,
    };
    let mut images: Vec<(ImageSlot, &[u8])> = Vec::new();
    if let Some(init) = &request.init_image {
        images.push((ImageSlot { kind: SlotKind::Init, width: init.width(), height: init.height() }, init.pixels()));
    }
    if let Some(mask) = &request.mask {
        images.push((ImageSlot { kind: SlotKind::Mask, width: mask.width(), height: mask.height() }, mask.pixels()));
    }
    for (index, reference) in request.references.iter().enumerate() {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        images.push((ImageSlot { kind: SlotKind::Reference { index }, width: reference.width(), height: reference.height() }, reference.pixels()));
    }
    (header, images)
}

/// Rebuilds a `GenerateRequest` from its header and the received image frames.
///
/// # Errors
/// A message when a frame's length does not match its slot, a slot repeats, or the
/// references are not numbered `0..n` in order.
pub fn assemble_request(header: GenerateHeader, images: Vec<(ImageSlot, Vec<u8>)>) -> Result<GenerateRequest, String> {
    let mut init_image = None;
    let mut mask = None;
    let mut references = Vec::new();
    for (slot, bytes) in images {
        match slot.kind {
            SlotKind::Init if init_image.is_none() => init_image = Some(RgbImage::new(slot.width, slot.height, bytes).map_err(|err| format!("init image: {err}"))?),
            SlotKind::Mask if mask.is_none() => mask = Some(GrayImage::new(slot.width, slot.height, bytes).map_err(|err| format!("mask: {err}"))?),
            SlotKind::Reference { index } if usize::try_from(index).is_ok_and(|index| index == references.len()) => {
                references.push(RgbImage::new(slot.width, slot.height, bytes).map_err(|err| format!("reference {index}: {err}"))?);
            }
            SlotKind::Init | SlotKind::Mask | SlotKind::Reference { .. } => return Err(format!("unexpected image slot {slot:?}")),
        }
    }
    Ok(GenerateRequest {
        prompt: header.prompt,
        negative_prompt: header.negative_prompt,
        width: header.width,
        height: header.height,
        init_image,
        mask,
        references,
        strength: header.strength,
        steps: header.steps,
        cfg_scale: header.cfg_scale,
        seed: header.seed,
        sample_method: header.sample_method,
        scheduler: header.scheduler,
        vae_tiling: header.vae_tiling,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Cursor;

    use super::*;
    use crate::request::{ComponentRole, DeviceInfo, DeviceSelection, FlashAttention, ModelFamily};

    fn round_trip<M: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(message: &M, blob: &[u8]) {
        let mut wire = Vec::new();
        write_message(&mut wire, message, blob).expect("write");
        let (back, back_blob): (M, Vec<u8>) = read_message(&mut Cursor::new(wire)).expect("read");
        assert_eq!(&back, message);
        assert_eq!(back_blob, blob);
    }

    fn request() -> GenerateRequest {
        GenerateRequest {
            prompt: "remove text".into(),
            negative_prompt: String::new(),
            width: 4,
            height: 2,
            init_image: Some(RgbImage::new(4, 2, (0..24).collect()).expect("init")),
            mask: Some(GrayImage::new(4, 2, vec![0, 255, 0, 255, 0, 255, 0, 255]).expect("mask")),
            references: vec![RgbImage::new(1, 1, vec![1, 2, 3]).expect("ref0"), RgbImage::new(2, 1, vec![4; 6]).expect("ref1")],
            strength: 0.9,
            steps: 4,
            cfg_scale: 1.0,
            seed: 42,
            sample_method: Some("euler".into()),
            scheduler: None,
            vae_tiling: true,
        }
    }

    #[test]
    fn every_request_round_trips() {
        let files = ModelFiles { family: ModelFamily::Flux2Klein, components: BTreeMap::from([(ComponentRole::Diffusion, PathBuf::from("/m/d.gguf"))]) };
        let context = ContextOptions { device: DeviceSelection::Named { name: "Vulkan1".into(), description: "GPU".into() }, text_encoder_on_cpu: true, weights_in_ram: false, max_vram_gib: Some(6.5), flash_attention: FlashAttention::Auto, threads: Some(6) };
        let generate = request();
        let (header, images) = split_request(&generate);
        let slots: Vec<ImageSlot> = images.iter().map(|(slot, _)| *slot).collect();
        let requests = [
            Request::Init { protocol: WORKER_PROTOCOL, package_dir: PathBuf::from("/data/sdcpp/vulkan/tag"), build: SdCppBuild::Vulkan },
            Request::Probe,
            Request::Generate(Box::new(GenerateMessage { files, context, request: header, images: slots.clone() })),
            Request::Image { slot: slots[0] },
            Request::Cancel,
            Request::Unload,
            Request::Shutdown,
        ];
        for message in &requests {
            round_trip(message, b"");
        }
    }

    #[test]
    fn every_event_round_trips() {
        let events = [
            Event::Ready { protocol: WORKER_PROTOCOL, sd_version: "v".into(), sd_commit: "be48649".into() },
            Event::ProbeResult { report: ProbeReport { sd_version: "v".into(), sd_commit: "c".into(), devices: vec![DeviceInfo { name: "CPU".into(), description: String::new() }], physical_cores: 6 } },
            Event::Stage { stage: RunStage::LoadingModel },
            Event::Step { step: 2, steps: 4 },
            Event::Log { level: LogLevel::Warn, text: "w".into() },
            Event::Result { width: 1, height: 1 },
            Event::Error { error: WireError::OutputSize { expected: (4, 4), found: (2, 2) } },
            Event::Error { error: WireError::LibraryLoad { problem: LoadProblem::MissingSystemLibrary { library: "libgomp.so.1".into() }, detail: "d".into() } },
            Event::Cancelled,
            Event::Unloaded,
        ];
        for message in &events {
            round_trip(message, &[1, 2, 3]);
        }
    }

    #[test]
    fn generate_request_splits_and_reassembles() {
        let original = request();
        let (header, images) = split_request(&original);
        assert_eq!(images.len(), 4);
        let received: Vec<(ImageSlot, Vec<u8>)> = images.into_iter().map(|(slot, bytes)| (slot, bytes.to_vec())).collect();
        assert_eq!(assemble_request(header.clone(), received.clone()).expect("assemble"), original);

        let mut wrong_len = received.clone();
        wrong_len[0].1.pop();
        assert!(assemble_request(header.clone(), wrong_len).is_err());
        let mut out_of_order = received.clone();
        out_of_order.swap(2, 3);
        assert!(assemble_request(header.clone(), out_of_order).is_err());
        let mut repeated = received;
        repeated.push(repeated[1].clone());
        assert!(assemble_request(header, repeated).is_err());
    }

    #[test]
    fn a_32_mib_image_fits_and_one_byte_more_is_refused() {
        let limit = ms_backend_ipc::protocol::MAX_BLOB_BYTES;
        let slot = ImageSlot { kind: SlotKind::Mask, width: 4096, height: 8192 };
        assert_eq!(usize::try_from(u64::from(slot.width) * u64::from(slot.height)).expect("fits"), limit);
        round_trip(&Request::Image { slot }, &vec![7_u8; limit]);
        let mut sink = Vec::new();
        assert!(write_message(&mut sink, &Request::Image { slot }, &vec![0_u8; limit + 1]).is_err());
    }

    #[test]
    fn wire_errors_convert_losslessly() {
        let errors = [
            SdCppError::LibraryLoad { problem: LoadProblem::Other, detail: "d".into() },
            SdCppError::AbiMismatch { expected: "a".into(), found: "b".into() },
            SdCppError::DeviceNotFound { name: "Vulkan1".into() },
            SdCppError::ContextCreate { detail: "d".into() },
            SdCppError::Generation { detail: "d".into() },
            SdCppError::OutputSize { expected: (1, 2), found: (3, 4) },
            SdCppError::Protocol { detail: "d".into() },
        ];
        for err in errors {
            assert_eq!(SdCppError::from(WireError::from_error(&err)), err);
        }
        assert!(matches!(WireError::from_error(&SdCppError::Busy), WireError::Protocol { .. }));
    }

    #[test]
    fn a_header_of_another_type_is_refused() {
        let mut wire = Vec::new();
        write_message(&mut wire, &Event::Cancelled, b"").expect("write");
        assert!(read_message::<Request, _>(&mut Cursor::new(wire)).is_err());
    }
}
