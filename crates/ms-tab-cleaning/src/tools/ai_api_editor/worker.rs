/*
File: ai_api_editor/worker.rs

Purpose:
One cloud edit run on its own worker thread: read the provider's API key from the credential
store, convert the region (and the user's marks, when they travel as a reference) to the
pipeline's RGBA rasters, call `ms_ai_api::image_edit::
run_image_edit` (which owns the HTTP exchange and the size contract), and turn the edited
raster back into an `egui::ColorImage` of exactly the region's size.

Key structures:
- `RunJob`: everything one run needs, snapshotted on the GUI thread
- `WorkerEvent`: what the worker reports (progress stages, then exactly one result)

Key functions:
- `spawn_run()`: starts the worker and returns its event channel
- `marks_reference()`: the run's `RunMarks` as the pipeline's reference raster

Notes:
Every blocking step (key-store read, network, decode, composite) happens here, never on the GUI
thread. A closed event channel (the engine dropped its receiver) raises the run's cancel flag
(`stage_sink`), so a run nobody reads stops at the pipeline's next check. The key is read here and handed straight to the pipeline; it is never logged, stored or
sent back. The prompt is never logged here (the pipeline logs its length only).
*/

use ms_ai_api::image_edit::{CancelFlag, EndpointChoice, ImageEditError, ImageEditProvider, ImageEditRequest, ImageEditStage, MaskBlend, RgbaRegion, read_key, run_image_edit};
use crate::tools::region_edit_v2::engine::RunMarks;
use eframe::egui;
use ms_thread as thread;
use std::sync::mpsc::{self, Receiver, Sender};

/// Everything one run needs, captured when «Обработать» starts it.
#[derive(Debug)]
pub(super) struct RunJob {
    /// The provider to call.
    pub provider: ImageEditProvider,
    /// The selected model id.
    pub model_id: String,
    /// The selected endpoint (also addresses the key slot).
    pub endpoint: EndpointChoice,
    /// The edit instruction.
    pub prompt: String,
    /// The source region (page crop composited with the clean overlay), exactly the frame size.
    pub region: egui::ColorImage,
    /// The painted mask (`width * height` bytes, 0/255), `None` when nothing is painted.
    pub mask: Option<Vec<u8>>,
    /// The user's marks as the host packaged them; a `Reference` or `Layer` is sent as the
    /// model's reference image (the engine admits them only for a model that takes one).
    pub marks: RunMarks,
    /// How the edit is blended back inside the mask.
    pub blend: MaskBlend,
    /// The upscale factor from `geometry::upscale_factor_for`.
    pub upscale: u8,
}

/// What the worker reports, in order: any number of `Stage`s, then exactly one `Finished`.
#[derive(Debug)]
pub(super) enum WorkerEvent {
    /// The pipeline reached a new stage.
    Stage(ImageEditStage),
    /// The run ended: the edited region (exactly the source size) or why it failed.
    Finished(Result<egui::ColorImage, ImageEditError>),
}

/// Starts `job` on a worker thread; `cancel` is shared with the engine.
#[must_use]
pub(super) fn spawn_run(job: RunJob, cancel: CancelFlag) -> Receiver<WorkerEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = run(job, &cancel, &tx);
        // A send fails only when the engine dropped the receiver (cancel, tool switch, engine
        // dropped): the run is over either way and nobody is left to tell.
        let _detached = tx.send(WorkerEvent::Finished(result));
    });
    rx
}

/// The worker body: key, conversion, pipeline, conversion back.
fn run(job: RunJob, cancel: &CancelFlag, tx: &Sender<WorkerEvent>) -> Result<egui::ColorImage, ImageEditError> {
    cancel.check()?;
    let [width, height] = job.region.size;
    let image = color_image_to_region(&job.region)?;
    let reference = marks_reference(job.marks)?;
    // Blocking credential-store read: this is why the key is read here and not by the panel.
    let key = read_key(job.provider, &job.endpoint)?;
    cancel.check()?;
    let request = ImageEditRequest {
        provider: job.provider,
        model_id: job.model_id,
        endpoint: job.endpoint,
        prompt: job.prompt,
        image,
        reference,
        mask: job.mask,
        blend: job.blend,
        upscale: job.upscale,
    };
    let outcome = run_image_edit(&request, &key, cancel, stage_sink(tx, cancel))?;
    region_to_color_image(outcome.image, width, height)
}

/// The pipeline's stage callback: forwards each stage to the engine. A closed channel means the
/// engine dropped the receiver (cancel, tool switch, engine dropped): nobody will read this run
/// any more, so the sink raises `cancel` and the pipeline stops at its next check, sending the
/// provider's cancel request instead of polling and downloading for nobody.
fn stage_sink<'a>(tx: &'a Sender<WorkerEvent>, cancel: &'a CancelFlag) -> impl FnMut(ImageEditStage) + 'a {
    move |stage| {
        if tx.send(WorkerEvent::Stage(stage)).is_err() {
            cancel.cancel();
        }
    }
}

/// The region as the pipeline's straight-alpha RGBA raster.
///
/// # Errors
/// `ShapeMismatch` when a side does not fit `u32` or the buffer disagrees with the size.
fn color_image_to_region(image: &egui::ColorImage) -> Result<RgbaRegion, ImageEditError> {
    let [width, height] = image.size;
    let side = |value: usize| u32::try_from(value).map_err(|_| ImageEditError::ShapeMismatch { detail: format!("region side {value} does not fit u32") });
    let pixels: Vec<u8> = image.pixels.iter().flat_map(|px| px.to_srgba_unmultiplied()).collect();
    RgbaRegion::new(side(width)?, side(height)?, pixels)
}

/// The reference raster `marks` travels as: `Reference` (the region with the marks composited,
/// opaque) un-premultiplied, `Layer` (the marks alone) with its straight alpha untouched, `None`
/// as no reference. The pipeline encodes it (RGB when opaque, RGBA otherwise) at the sent size.
///
/// # Errors
/// `ShapeMismatch` when a side does not fit `u32` or a buffer disagrees with its size.
fn marks_reference(marks: RunMarks) -> Result<Option<RgbaRegion>, ImageEditError> {
    match marks {
        RunMarks::None => Ok(None),
        RunMarks::Reference(image) => color_image_to_region(&image).map(Some),
        RunMarks::Layer(layer) => {
            let (width, height) = layer.dimensions();
            RgbaRegion::new(width, height, layer.into_raw()).map(Some)
        }
    }
}

/// The edited raster as a `ColorImage`, refused unless it is exactly `width × height`.
///
/// # Errors
/// `SizeContractViolated` for any other size (the host would refuse it too; this names it).
fn region_to_color_image(region: RgbaRegion, width: usize, height: usize) -> Result<egui::ColorImage, ImageEditError> {
    let got = (region.width(), region.height());
    let matches = usize::try_from(got.0).is_ok_and(|w| w == width) && usize::try_from(got.1).is_ok_and(|h| h == height);
    if !matches {
        let expected = (u32::try_from(width).unwrap_or(u32::MAX), u32::try_from(height).unwrap_or(u32::MAX));
        return Err(ImageEditError::SizeContractViolated { expected, detail: format!("the pipeline returned {}x{}", got.0, got.1) });
    }
    Ok(egui::ColorImage::from_rgba_unmultiplied([width, height], region.pixels()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conversion pair is lossless for opaque pixels (the page crop is opaque), and the
    /// way back refuses any size but the region's.
    #[test]
    fn region_conversion_round_trips_and_checks_the_size() {
        let pixels: Vec<egui::Color32> = (0u8..6).map(|v| egui::Color32::from_rgb(v, v.wrapping_mul(40), 255 - v)).collect();
        let image = egui::ColorImage::new([3, 2], pixels);
        let region = color_image_to_region(&image).expect("a 3x2 region converts");
        assert_eq!((region.width(), region.height()), (3, 2));
        let back = region_to_color_image(region.clone(), 3, 2).expect("the same size converts back");
        assert_eq!(back.pixels, image.pixels);
        assert!(matches!(region_to_color_image(region, 2, 3), Err(ImageEditError::SizeContractViolated { .. })));
    }

    /// Each marks form becomes its reference raster: the composited copy opaque, the layer with
    /// its alpha byte for byte, no marks no reference.
    #[test]
    fn marks_become_the_reference_raster() {
        assert!(matches!(marks_reference(RunMarks::None), Ok(None)));
        let composited = egui::ColorImage::new([2, 1], vec![egui::Color32::from_rgb(200, 10, 10), egui::Color32::from_rgb(1, 2, 3)]);
        let reference = marks_reference(RunMarks::Reference(composited)).expect("converts").expect("a reference");
        assert_eq!((reference.width(), reference.height()), (2, 1));
        assert_eq!(reference.pixels(), &[200, 10, 10, 255, 1, 2, 3, 255]);
        let layer = image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 128, 0, 0, 0, 0]).expect("2x1 layer");
        let reference = marks_reference(RunMarks::Layer(layer)).expect("converts").expect("a reference");
        assert_eq!(reference.pixels(), &[255, 0, 0, 128, 0, 0, 0, 0], "straight alpha is kept");
    }

    /// A stage forwarded to a live engine leaves the run alone; once the receiver is gone the
    /// sink raises the cancel flag, so the pipeline stops at its next check.
    #[test]
    fn a_closed_channel_cancels_the_run() {
        let cancel = CancelFlag::new();
        let (tx, rx) = mpsc::channel();
        stage_sink(&tx, &cancel)(ImageEditStage::Sending);
        assert!(matches!(rx.try_recv(), Ok(WorkerEvent::Stage(ImageEditStage::Sending))));
        assert!(!cancel.is_cancelled());
        drop(rx);
        stage_sink(&tx, &cancel)(ImageEditStage::Waiting { polls: 1 });
        assert!(cancel.is_cancelled());
    }
}
