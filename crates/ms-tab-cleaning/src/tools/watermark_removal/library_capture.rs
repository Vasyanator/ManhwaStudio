/*
File: tools/watermark_removal/library_capture.rs

Purpose:
Library capture: the worker that turns a selected page region into a reference crop for the
watermark library, the grown capture rect and the scratch-crop files (path, staleness, sweep).

Key items:
- `run_library_capture()`
- `grow_page_rect()` (returns the parent's `GrownCrop`)
- `library_scratch_crop_path()`, `scratch_crop_is_stale()`, `sweep_stale_scratch_crops()`

Notes:
Runs on a worker thread; `LibraryCaptureRequest`, `GrownCrop` and the scratch-crop constants live
in `mod.rs`.
*/
use crate::tools::watermark_entry::{CropMargins, ReferenceRefusal, reference_crop_margin_px};
use crate::tools::watermark_library_window::{
    CapturedCrop, LibraryCaptureRefusal, thumbnail as library_window_thumbnail,
};
use crate::watermark_chapter::{
    PixelRect, SampleRejection, SampleVerdict, validate_calibration_sample,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use web_time::Duration;
use super::{GrownCrop, LibraryCaptureRequest, SCRATCH_CROP_MAX_AGE, SCRATCH_CROP_PREFIX};
use super::chapter_state::{decode_chapter_page, overlay_rect_to_page};


/// Cuts the armed selection out of its page and measures the background under it.
///
/// The written crop is the selection GROWN by a measurement ring, because the reference intake
/// derives the footprint by insetting the crop by that margin again: what the user dragged is
/// the mark, and the band around it is the evidence about the background. The ring is measured
/// here as well, on the page, so the panel knows straight away whether the crop can be
/// committed or has to wait for the user to state a level — the on-disk sample format has no
/// "level unknown" state.
///
/// The ring is CLIPPED at the page border rather than demanded on all four sides. A mark
/// stamped against the edge of the image — which is where the user's marks sit — has no
/// background on that side and never will, so a symmetric demand would make such a mark
/// permanently unsamplable. The engine's own measurement has always worked this way
/// (`validate_calibration_sample` clips the ring and admits it on a pixel COUNT), so nothing is
/// widened here; what is removed is a symmetry this lane invented on top of it. The real
/// margins are carried to the intake in [`CapturedCrop::margins`] so the footprint it derives is
/// the one that was cut, and the measurement's partialness travels with the level.
///
/// The three ring outcomes are NOT the same thing: a flat ring measures the level, a ring
/// refused as flat leaves the level to the user, and a ring that could not be measured at all
/// makes the crop unusable — no stated level can license it, so it is refused here instead of
/// being offered a colour control it could never satisfy.
///
/// # Errors
/// A [`LibraryCaptureRefusal`] — an already-localized message plus the tag that says what the
/// user could DO about it — for a page that cannot be decoded, a degenerate selection, a
/// footprint above `max_side`, a selection with too little background left around it to judge
/// flatness at all, and a scratch file that cannot be written.
pub(super) fn run_library_capture(request: LibraryCaptureRequest) -> Result<CapturedCrop, LibraryCaptureRefusal> {
    let LibraryCaptureRequest {
        page: page_task,
        rect,
        sample_params,
        max_side,
    } = request;
    let page = decode_chapter_page(&page_task.path)
        .map_err(LibraryCaptureRefusal::untagged)?;
    let overlay_size = page_task
        .overlay_size
        .unwrap_or([page.width() as usize, page.height() as usize]);
    let footprint = overlay_rect_to_page(rect, overlay_size, page.width(), page.height())
        .ok_or_else(|| {
            LibraryCaptureRefusal::tagged(
                t!("cleaning.region.invalid_selection_size_error").to_string(),
                ReferenceRefusal::TooSmall,
            )
        })?;
    if footprint.width > max_side || footprint.height > max_side {
        return Err(LibraryCaptureRefusal::tagged(
            tf!(
                "cleaning.tools.watermark.chapter.selection_too_large_error",
                width = footprint.width,
                height = footprint.height,
                limit = max_side
            ),
            ReferenceRefusal::TooSmall,
        ));
    }
    let margin = reference_crop_margin_px(&sample_params);
    let grown = grow_page_rect(footprint, margin, page.width(), page.height()).ok_or_else(|| {
        LibraryCaptureRefusal::tagged(
            t!("cleaning.region.invalid_selection_size_error").to_string(),
            ReferenceRefusal::TooSmall,
        )
    })?;
    let GrownCrop { outer, margins } = grown;
    let measured = match validate_calibration_sample(&page, footprint, &sample_params) {
        SampleVerdict::Calibration { level, .. } => Some(level),
        // The ring was measured and REFUSED as flat: the level is the one thing only the user
        // can supply, and the intake accepts the crop once they state it.
        SampleVerdict::TemplateOnly { .. } => None,
        // No ring could be measured at all. Stating a level cannot license this crop — the
        // intake refuses it whatever is asserted (`run_reference_intake`) — so the capture is
        // refused HERE, before a scratch file is written and before the panel offers a colour
        // control that could never lead anywhere.
        SampleVerdict::Unusable {
            reason: SampleRejection::BadRect,
        } => {
            return Err(LibraryCaptureRefusal::tagged(
                t!("cleaning.tools.watermark.chapter.sample_unusable_rect").to_string(),
                ReferenceRefusal::TooSmall,
            ));
        }
        // Too little background survived to judge flatness AT ALL. This is the floor a mark at
        // the page edge actually runs into, and it is a pixel count rather than a per-side
        // margin — so the message names the count, and names the border when that is what took
        // the pixels, instead of the generic "too close to the edge" it used to give.
        SampleVerdict::Unusable {
            reason: SampleRejection::RingTooSmall { pixels, needed },
        } => {
            let message = if margins.is_partial(margin) {
                tf!(
                    "cleaning.tools.watermark.chapter.library_capture_edge_ring_error",
                    pixels = pixels,
                    needed = needed
                )
            } else {
                tf!(
                    "cleaning.tools.watermark.chapter.sample_unusable_ring",
                    pixels = pixels,
                    needed = needed
                )
            };
            return Err(LibraryCaptureRefusal::tagged(
                message,
                ReferenceRefusal::TooSmall,
            ));
        }
    };
    let crop = image::imageops::crop_imm(&page, outer.x, outer.y, outer.width, outer.height)
        .to_image();
    let path = library_scratch_crop_path();
    crop.save(&path).map_err(|err| {
        LibraryCaptureRefusal::untagged(tf!(
            "cleaning.tools.watermark.chapter.library_image_write_error",
            path = path.display(),
            err = err
        ))
    })?;
    Ok(CapturedCrop {
        preview: library_window_thumbnail(&crop),
        path,
        measured,
        margins,
    })
}

/// Grows `rect` by up to `margin` on every side, taking what the page can give.
///
/// A side the page border truncates contributes fewer pixels — or none — instead of failing the
/// whole capture. This used to refuse, which made a mark stamped flush against the image edge
/// permanently unsamplable: the ring it needs does not exist there and never will, and the user
/// has such marks. The engine's measurement has always clipped its ring at the page and judged
/// the result on a PIXEL COUNT, so admitting a truncated ring here widens nothing; it only stops
/// this lane from demanding a symmetry the measurement never asked for. The asymmetry is not
/// hidden: [`GrownCrop::margins`] travels to the intake, which derives the footprint from it
/// instead of assuming a symmetric inset, and the ring's partialness is recorded with the level.
///
/// `None` only for a rect that is not inside the page at all, which is a bad selection rather
/// than a tight one.
pub(super) fn grow_page_rect(
    rect: PixelRect,
    margin: u32,
    page_width: u32,
    page_height: u32,
) -> Option<GrownCrop> {
    if u64::from(rect.x) + u64::from(rect.width) > u64::from(page_width)
        || u64::from(rect.y) + u64::from(rect.height) > u64::from(page_height)
        || rect.width == 0
        || rect.height == 0
    {
        return None;
    }
    let left = margin.min(rect.x);
    let top = margin.min(rect.y);
    // Cast-free: both sums are bounded by the page dimensions, which the check above proved.
    let right = margin.min(page_width - rect.x - rect.width);
    let bottom = margin.min(page_height - rect.y - rect.height);
    Some(GrownCrop {
        outer: PixelRect::new(
            rect.x - left,
            rect.y - top,
            rect.width + left + right,
            rect.height + top + bottom,
        ),
        margins: CropMargins {
            left,
            top,
            right,
            bottom,
        },
    })
}

/// A fresh path for one session scratch crop.
///
/// The crops are session state by construction — a crop with no stated background cannot be
/// persisted — so they live in the system temp directory, are named per process and per crop
/// so two runs cannot collide, and are deleted by the panel when the crop is committed or
/// discarded. A run that ends before the panel got to them is what `sweep_stale_scratch_crops`
/// cleans up.
fn library_scratch_crop_path() -> PathBuf {
    static NEXT_CROP: AtomicU64 = AtomicU64::new(1);
    let index = NEXT_CROP.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "{SCRATCH_CROP_PREFIX}{}-{index}.png",
        std::process::id()
    ))
}

/// True when `name` is a scratch crop that no longer belongs to anybody this sweep may assume
/// is alive: it carries the scratch prefix, it was not written by `current_pid`, and `age` has
/// passed [`SCRATCH_CROP_MAX_AGE`].
///
/// Pure so the rule can be tested without touching the filesystem. A name whose process id
/// does not parse is still one of ours by prefix and is aged out like any other; a file this
/// process wrote is NEVER swept, because its crops may still be waiting in the panel.
pub(super) fn scratch_crop_is_stale(name: &str, age: Duration, current_pid: u32) -> bool {
    let Some(rest) = name.strip_prefix(SCRATCH_CROP_PREFIX) else {
        return false;
    };
    if !rest.ends_with(".png") {
        return false;
    }
    let owner = rest.split_once('-').map(|(pid, _)| pid).unwrap_or(rest);
    if owner.parse::<u32>().is_ok_and(|pid| pid == current_pid) {
        return false;
    }
    age >= SCRATCH_CROP_MAX_AGE
}

/// Removes scratch crops that earlier runs left in the system temp directory.
///
/// The panel deletes a crop when it is committed or discarded, so the only crops this finds
/// are those a run was killed on top of. Without the sweep they accumulate for the life of the
/// machine; with it the directory holds at most one session's worth plus a day's grace for a
/// concurrently running instance. Runs on a worker thread: it is directory I/O.
pub(super) fn sweep_stale_scratch_crops() {
    let dir = std::env::temp_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) => {
            ms_log::runtime_log::log_warn(format!(
                "[cleaning] watermark scratch sweep could not read {}: {err}",
                dir.display()
            ));
            return;
        }
    };
    let now = std::time::SystemTime::now();
    let current_pid = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // A file whose age cannot be read counts as brand new, so an unreadable timestamp
        // never costs somebody their crop.
        let age = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .unwrap_or(Duration::ZERO);
        if !scratch_crop_is_stale(name, age, current_pid) {
            continue;
        }
        if let Err(err) = fs::remove_file(entry.path()) {
            ms_log::runtime_log::log_warn(format!(
                "[cleaning] watermark scratch sweep could not remove {}: {err}",
                entry.path().display()
            ));
        }
    }
}
