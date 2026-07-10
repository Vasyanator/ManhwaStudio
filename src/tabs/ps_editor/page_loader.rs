/*
File: tabs/ps_editor/page_loader.rs

Purpose:
Background loader for the PS-like editor. Decodes the source page and reads the shared clean
overlay for a single page off the GUI thread, returning ready `ColorImage`s for the two base
layers.

Key structures:
- `PageLoadRequest` / `PageLoadResult` / `LoadedPage`: worker protocol.
- `spawn_page_loader_thread`: starts the worker and returns its channels + join handle.

Notes:
Source pixels are reused from `CleanOverlaysModel`'s page cache when present and decoded with
`image::open` otherwise (then stored back into the cache). The clean overlay is read via
`overlay_rgba`; an absent overlay yields a fully transparent clean layer of the source size. The
model lock is never held across image decode.

In addition to the two base-layer images, the worker decodes the page's persisted USER layer payload
(raster + text nodes + groups) off the GUI thread via `LayerDoc::decode_page_payload` — a PURE,
lock-free function (the worker holds no doc `Arc`, only the inputs). The decoded payload is returned in
`LoadedPage::layers` for the GUI thread to MOVE into the shared `LayerDoc` with a brief lock
(`insert_decoded_page`). The heavy multi-MB PNG decode therefore never runs under the doc lock.
*/

use crate::models::clean_overlays_model::CleanOverlaysModel;
use crate::models::layer_model::layer_doc::{DecodedPagePayload, LayerDoc};
use eframe::egui::{Color32, ColorImage};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use ms_thread as thread;

/// Request to load the base layers + persisted user-layer payload for one page.
///
/// `unsaved_layers_dir` / `layers_dir` are the staging (primary) and committed (fallback) layer dirs,
/// and `page_sizes` is the FULL chapter page-size map (`page_idx -> [w, h]`) required by the doc's
/// lock-free `decode_page_payload` for the legacy absolute-ribbon migration (a partial map corrupts
/// legacy geometry — see `LayerDoc::decode_page_payload`).
#[derive(Debug, Clone)]
pub struct PageLoadRequest {
    pub job_id: u64,
    pub page_idx: usize,
    pub page_path: PathBuf,
    pub unsaved_layers_dir: PathBuf,
    pub layers_dir: PathBuf,
    pub page_sizes: HashMap<usize, [usize; 2]>,
}

/// Decoded base layers for a page, plus the off-thread-decoded user-layer payload.
///
/// `layers` is `None` when the persisted-layer decode failed (the page still opens with its two base
/// layers; the failure is logged on the worker and does not fail the whole page load).
#[derive(Debug)]
pub struct LoadedPage {
    pub size: [usize; 2],
    pub source: ColorImage,
    pub clean: ColorImage,
    pub layers: Option<DecodedPagePayload>,
}

/// Result of a page load job; `outcome` carries a user-facing error string on failure.
#[derive(Debug)]
pub struct PageLoadResult {
    pub job_id: u64,
    pub page_idx: usize,
    pub outcome: Result<LoadedPage, String>,
}

/// Channels for talking to the page loader worker thread.
///
/// The worker thread is detached; it exits on its own once `request_tx` is dropped (the recv loop
/// then returns an error), so no join handle is retained.
pub struct PageLoaderHandles {
    pub request_tx: Sender<Option<PageLoadRequest>>,
    pub result_rx: Receiver<PageLoadResult>,
}

/// Spawns the page loader worker bound to the shared clean-overlay model.
///
/// Send `Some(request)` to load a page and `None` to stop the worker. Results arrive on
/// `result_rx`; the caller filters stale `job_id`s.
#[must_use]
pub fn spawn_page_loader_thread(
    overlays_model: Arc<Mutex<CleanOverlaysModel>>,
) -> PageLoaderHandles {
    let (request_tx, request_rx) = mpsc::channel::<Option<PageLoadRequest>>();
    let (result_tx, result_rx) = mpsc::channel::<PageLoadResult>();
    thread::spawn(move || {
        while let Ok(message) = request_rx.recv() {
            let Some(request) = message else {
                break;
            };
            let outcome = load_page(&overlays_model, &request);
            let _ = result_tx.send(PageLoadResult {
                job_id: request.job_id,
                page_idx: request.page_idx,
                outcome,
            });
        }
    });
    PageLoaderHandles {
        request_tx,
        result_rx,
    }
}

/// Loads the source + clean base layers for one page.
fn load_page(
    overlays_model: &Arc<Mutex<CleanOverlaysModel>>,
    request: &PageLoadRequest,
) -> Result<LoadedPage, String> {
    // Try the shared page cache first; decode off-lock only on a miss.
    let cached = overlays_model
        .lock()
        .ok()
        .and_then(|mut model| model.cached_page_rgba(request.page_idx));
    let source_rgba = match cached {
        Some(image) => image,
        None => {
            let decoded = image::open(&request.page_path)
                .map_err(|err| {
                    tf!("ps_editor.page_loader.open_error", path = request.page_path.display(), err = err)
                })?
                .to_rgba8();
            let decoded = Arc::new(decoded);
            if let Ok(mut model) = overlays_model.lock() {
                let _ = model.store_cached_page_rgba_arc(request.page_idx, Arc::clone(&decoded));
            }
            decoded
        }
    };

    let size = [source_rgba.width() as usize, source_rgba.height() as usize];
    if size[0] == 0 || size[1] == 0 {
        return Err(tf!("ps_editor.page_loader.zero_size", path = request.page_path.display()));
    }

    let clean_rgba = overlays_model
        .lock()
        .ok()
        .and_then(|model| model.overlay_rgba(request.page_idx));

    let source = rgba_to_color_image(&source_rgba);
    let clean = match clean_rgba {
        Some(overlay)
            if overlay.width() as usize == size[0] && overlay.height() as usize == size[1] =>
        {
            rgba_to_color_image(&overlay)
        }
        // No overlay (or a mismatched legacy size): start from a transparent clean layer.
        _ => ColorImage::filled(size, Color32::TRANSPARENT),
    };

    // Decode the persisted user-layer payload OFF the doc lock (the worker holds no doc `Arc`, only
    // the pure inputs). A failure does NOT fail the whole page load: the page still opens with its two
    // base layers, and the GUI thread leaves the doc page un-inserted (the per-frame doc projection
    // then shows just the base layers). The error is logged here so it is not silently swallowed.
    let layers = match LayerDoc::decode_page_payload(
        request.page_idx,
        &request.unsaved_layers_dir,
        Some(&request.layers_dir),
        &request.page_sizes,
    ) {
        Ok(payload) => Some(payload),
        Err(err) => {
            crate::runtime_log::log_warn(format!(
                "[ps_editor] page {} layer payload decode failed: {err}",
                request.page_idx
            ));
            None
        }
    };

    Ok(LoadedPage {
        size,
        source,
        clean,
        layers,
    })
}

/// Converts a straight-alpha `RgbaImage` to an egui `ColorImage`.
fn rgba_to_color_image(image: &image::RgbaImage) -> ColorImage {
    ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    )
}
