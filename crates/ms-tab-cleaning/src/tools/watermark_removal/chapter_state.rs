/*
File: tools/watermark_removal/chapter_state.rs

Purpose:
Chapter-mode state behaviour and its small helpers: mark / catalog / state methods, page
decode, overlay <-> page geometry, crops, histogram bins, kind refit and the mark previews.

Key items:
- `impl ChapterMark`, `impl ChapterCatalog`, `impl ChapterState`
- `decode_chapter_page()`, `overlay_rect_to_page()`, `page_rect_to_overlay()`, `crop_page()`
- `build_chapter_preview()`, `refresh_chapter_preview()`, `next_library_arm()`

Notes:
Every function here is pure or worker-side; nothing touches egui state.
*/
use crate::tools::watermark_library::StoredAlphaAssumption;
use crate::tools::watermark_library_window::{LibraryArm, LibraryPanelRequest};
use ms_canvas::OverlayRectPx;
use crate::watermark_chapter::{ModelFitError, PixelRect, WatermarkKind, WatermarkModel};
use eframe::egui;
use egui::Color32;
use image::RgbaImage;
use ms_thread as thread;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use super::{
    CHAPTER_HISTOGRAM_BINS, ChapterCatalog, ChapterEvent, ChapterJobLabel, ChapterMark,
    ChapterPreview, ChapterState,
};


impl ChapterMark {
    /// A mark with no samples yet.
    pub(super) fn new(name: String, template_crop: RgbaImage, page_width: u32) -> Self {
        Self {
            name,
            template_crop,
            crops: Vec::new(),
            last_verdict: None,
            template_only: 0,
            occurrences: 0,
            unverified: 0,
            gain_mean: 0.0,
            snr_mean: 0.0,
            histogram: [0; CHAPTER_HISTOGRAM_BINS],
            page_width,
            library_entry: None,
            matches: Vec::new(),
            adopted_entry: None,
            pinned_entry: None,
            parked: None,
            alpha_assumption: StoredAlphaAssumption::FromDeposit,
            preview: None,
            preview_revision: 0,
        }
    }

    /// Clears the per-scan counters before a new scan fills them in.
    pub(super) fn reset_scan_stats(&mut self) {
        self.occurrences = 0;
        self.unverified = 0;
        self.gain_mean = 0.0;
        self.snr_mean = 0.0;
        self.histogram = [0; CHAPTER_HISTOGRAM_BINS];
    }
}

impl ChapterCatalog {
    pub(super) fn len(&self) -> usize {
        self.kinds.len().min(self.marks.len())
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends one mark and its kind, keeping the alignment.
    pub(super) fn push(&mut self, kind: WatermarkKind, mark: ChapterMark) {
        self.kinds.push(kind);
        self.marks.push(mark);
    }

    /// Removes one mark and its kind.
    pub(super) fn remove(&mut self, index: usize) {
        if index >= self.len() {
            return;
        }
        self.kinds.remove(index);
        self.marks.remove(index);
    }

    /// Index of the mark with this literal id.
    pub(super) fn index_of(&self, mark_id: &str) -> Option<usize> {
        self.kinds.iter().position(|kind| kind.id() == mark_id)
    }
}

impl ChapterState {
    /// True while a worker owns a copy of the catalog.
    pub(super) fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Drops everything that describes the previous chapter scan. Called whenever the
    /// catalog changes, because hits reference marks and models that no longer exist.
    pub(super) fn invalidate_scan(&mut self) {
        self.hits.clear();
        self.scan_report = None;
        self.apply_report = None;
    }

    /// Starts a worker, refusing a second job while one is in flight.
    pub(super) fn start_job<F>(&mut self, label: ChapterJobLabel, work: F) -> bool
    where
        F: FnOnce(&Sender<ChapterEvent>) + Send + 'static,
    {
        if self.busy() {
            self.status =
                Some(t!("cleaning.mask_editor.processing_already_running_status").to_string());
            return false;
        }
        let (tx, rx) = mpsc::channel::<ChapterEvent>();
        self.rx = Some(rx);
        self.job = Some(label);
        self.progress = None;
        thread::spawn(move || {
            work(&tx);
        });
        true
    }
}

/// Decodes one chapter page into RGBA8.
///
/// # Errors
/// A user-facing message naming the file when the image cannot be opened or decoded.
pub(super) fn decode_chapter_page(path: &Path) -> Result<RgbaImage, String> {
    image::open(path)
        .map(|image| image.to_rgba8())
        .map_err(|err| {
            tf!(
                "cleaning.region.open_page_error",
                page_path = path.display(),
                err = err
            )
        })
}

/// Wraps an engine failure into the tool's user-facing message.
pub(super) fn chapter_engine_error(err: impl std::fmt::Display) -> String {
    tf!("cleaning.tools.watermark.chapter.engine_error", err = err)
}

/// Maps a value from one pixel grid onto another, proportionally.
///
/// Both grids describe the same page, so the ratio is exact whenever the two sizes are
/// equal (the normal case: an overlay is created at the page's own pixel size).
fn scale_axis(value: u64, from: u64, to: u64) -> u64 {
    if from == 0 {
        return 0;
    }
    (value * to + from / 2) / from
}

/// Maps an overlay-space rect onto the decoded page's own pixel grid.
///
/// Returns `None` for a degenerate rect or one that lands outside the page.
pub(super) fn overlay_rect_to_page(
    rect: OverlayRectPx,
    overlay_size: [usize; 2],
    page_width: u32,
    page_height: u32,
) -> Option<PixelRect> {
    let (ow, oh) = (overlay_size[0] as u64, overlay_size[1] as u64);
    let (pw, ph) = (u64::from(page_width), u64::from(page_height));
    if ow == 0 || oh == 0 || pw == 0 || ph == 0 {
        return None;
    }
    let x0 = scale_axis(rect.x as u64, ow, pw).min(pw);
    let y0 = scale_axis(rect.y as u64, oh, ph).min(ph);
    let x1 = scale_axis((rect.x + rect.w) as u64, ow, pw).min(pw);
    let y1 = scale_axis((rect.y + rect.h) as u64, oh, ph).min(ph);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(PixelRect::new(
        u32::try_from(x0).ok()?,
        u32::try_from(y0).ok()?,
        u32::try_from(x1 - x0).ok()?,
        u32::try_from(y1 - y0).ok()?,
    ))
}

/// Maps a decoded-page rect back into overlay space.
///
/// Returns `None` for a degenerate result, so a patch is never applied to an empty rect.
pub(super) fn page_rect_to_overlay(
    rect: PixelRect,
    page_size: [usize; 2],
    overlay_size: [usize; 2],
) -> Option<OverlayRectPx> {
    let (pw, ph) = (page_size[0] as u64, page_size[1] as u64);
    let (ow, oh) = (overlay_size[0] as u64, overlay_size[1] as u64);
    if pw == 0 || ph == 0 || ow == 0 || oh == 0 {
        return None;
    }
    let x0 = scale_axis(u64::from(rect.x), pw, ow).min(ow);
    let y0 = scale_axis(u64::from(rect.y), ph, oh).min(oh);
    let x1 = scale_axis(rect.right(), pw, ow).min(ow);
    let y1 = scale_axis(rect.bottom(), ph, oh).min(oh);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(OverlayRectPx {
        x: usize::try_from(x0).ok()?,
        y: usize::try_from(y0).ok()?,
        w: usize::try_from(x1 - x0).ok()?,
        h: usize::try_from(y1 - y0).ok()?,
    })
}

/// Cuts `rect` out of `page`. The rect must already be validated against the page.
pub(super) fn crop_page(page: &RgbaImage, rect: PixelRect) -> RgbaImage {
    image::imageops::crop_imm(page, rect.x, rect.y, rect.width, rect.height).to_image()
}

/// Histogram bin of a background luma.
pub(super) fn chapter_histogram_bin(luma: f32) -> usize {
    if !luma.is_finite() || luma <= 0.0 {
        return 0;
    }
    // Cast justification: the quotient is clamped into `0..CHAPTER_HISTOGRAM_BINS`, a
    // single-digit range.
    let bin = (luma / 256.0 * CHAPTER_HISTOGRAM_BINS as f32) as usize;
    bin.min(CHAPTER_HISTOGRAM_BINS - 1)
}

/// Refits a kind, treating a graded refusal as the normal state it is.
///
/// A refusal stores its verdict in the kind and drops the model, which is exactly what the
/// UI reports; only an invalid input is a real failure.
///
/// # Errors
/// A user-facing message for [`ModelFitError::Invalid`].
pub(super) fn refit_chapter_kind(kind: &mut WatermarkKind) -> Result<(), String> {
    match kind.refit() {
        Ok(()) | Err(ModelFitError::Refused(_)) => Ok(()),
        Err(ModelFitError::Invalid(err)) => Err(chapter_engine_error(err)),
    }
}

/// Builds the alpha-map and imprint previews of a fitted model.
///
/// The imprint is `W = c/alpha`, defined only where the mark is opaque enough for the
/// division to mean anything; elsewhere the preview is transparent rather than showing a
/// number the data does not contain.
pub(super) fn build_chapter_preview(model: &WatermarkModel) -> ChapterPreview {
    let (width, height) = (model.width() as usize, model.height() as usize);
    let mut alpha = egui::ColorImage::filled([width, height], Color32::TRANSPARENT);
    let mut imprint = egui::ColorImage::filled([width, height], Color32::TRANSPARENT);
    let (c, s) = (model.c(), model.s());
    for pixel in 0..width * height {
        let base = pixel * 3;
        let mut peak = 0.0f32;
        let mut colour = [0u8; 3];
        for channel in 0..3 {
            let opacity = (1.0 - s[base + channel]).clamp(0.0, 1.0);
            peak = peak.max(opacity);
            // Below this the division is dominated by noise, so the mark's own colour is
            // simply not measured there.
            colour[channel] = if opacity > 0.02 {
                // Cast justification: the value is clamped to 0..=255 and rounded.
                (c[base + channel] / opacity).clamp(0.0, 255.0).round() as u8
            } else {
                0
            };
        }
        // Cast justification: `peak` is clamped to 0..=1 before scaling.
        let grey = (peak * 255.0).round() as u8;
        alpha.pixels[pixel] = Color32::from_rgb(grey, grey, grey);
        if peak > 0.02 {
            imprint.pixels[pixel] =
                Color32::from_rgba_unmultiplied(colour[0], colour[1], colour[2], 255);
        }
    }
    ChapterPreview { alpha, imprint }
}

/// Refreshes one mark's preview from its kind, bumping the revision so the GUI re-uploads.
pub(super) fn refresh_chapter_preview(catalog: &mut ChapterCatalog, index: usize) {
    let Some(kind) = catalog.kinds.get(index) else {
        return;
    };
    let preview = kind.model().map(build_chapter_preview);
    let Some(mark) = catalog.marks.get_mut(index) else {
        return;
    };
    mark.preview = preview;
    mark.preview_revision = mark.preview_revision.wrapping_add(1);
}

/// The reservation left after one library-panel request is applied.
///
/// Exactly one reservation exists at a time, so arming a second purpose REPLACES the first
/// instead of adding to it: that is what makes «+ новый» and «+ Выделить новый» mutually
/// exclusive without either button knowing the other exists. Choosing which entry the chapter
/// hunts says nothing about the next canvas selection and therefore leaves it alone.
pub(super) fn next_library_arm(
    current: Option<LibraryArm>,
    request: &LibraryPanelRequest,
) -> Option<LibraryArm> {
    match request {
        LibraryPanelRequest::Arm(arm) => Some(arm.clone()),
        LibraryPanelRequest::Disarm => None,
        LibraryPanelRequest::SelectEntry(_) | LibraryPanelRequest::UnselectEntry(_) => current,
    }
}
