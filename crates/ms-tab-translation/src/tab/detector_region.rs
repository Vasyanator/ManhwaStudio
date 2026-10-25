/*
FILE OVERVIEW: crates/ms-tab-translation/src/tab/detector_region.rs
The Translation tab's side of "an OCR selection also creates the detector mask and text blocks"
and of what the detector's Save button writes.

Main items:
- `TranslationTabState::start_selection_region_detection`: the trigger of a selection OCR
  (quick rect, the advanced window's Create bubble on its ORIGINAL page selection). Runs only
  when `RegionDetectEngine::for_ocr` says so (PaddleOCR / Surya / NeuroLate with that engine's
  own parameter on), starts the chapter's stored-detection load if it has not run, checks the
  detector route like a page run, and refuses with a toast and a status line while the detector
  is busy (nothing is queued in the controller).
- `ocr_region_want_for` / `receive_ocr_region` / `drop_ocr_region`: a quick NeuroLate selection
  takes its region from the OCR answer itself (one `MASK_AND_TEXT` request): the OCR request
  carries an `OcrRegionWant` (none while the detector is busy or unavailable: the OCR then runs
  in `TEXT` mode and the start above shows the usual refusal), the request id waits in
  `pending_ocr_regions`, and the answer's region enters the SAME `receive_region_detection`
  path. A failed or stopped OCR, Clear and a replacing page batch drop the wait; while it is
  awaited no page batch starts (`detector_busy_for_batches`). The advanced
  window's Create bubble never rides: it runs the detector (NeuroLate: a separate `MASK`
  request).
- `receive_region_detection` / `replay_pending_region_detections`: a result that arrives while
  the stored detection is loading (or being saved), or before it ever loaded (a load that died
  is retried by the next dispatch or panel draw), waits in `pending_region_detections` and is
  applied in arrival order once the load succeeded, so it never replaces a stored page the load
  has not delivered yet. Clear and a replacing page batch drop the waiting results.
- `apply_region_detection` / `region_detection_failed`: a result is merged into BOTH detector
  stores, replacing only the selection's rect. The tab's `text_detector_results` takes it through
  `text_detector::merge_region_into_page` (mask + the blocks centred in the rect, then finalized);
  the shared `TextMaskModel` merges the same region into its CURRENT mask (brush edits kept) and
  receives the tab's finalized block set (`TextMaskModel::merge_region`), so both stores hold
  the same blocks. A line-edit selection or drag on that page is dropped (the merge reorders the
  blocks it indexes).
- Legacy pages (stored blocks, no stored mask) show a mask rasterized from their resolved blocks;
  a region merge first freezes that shown raster into the result (`rasterize_resolved_blocks`),
  so the page looks the same outside the selection afterwards. The model then receives the
  resolved boxes that raster was built from (merged with the region by `merge_region_blocks`),
  so its boxes describe its mask; the tab's store keeps the raw lines.
- `detection_pages_for_save`: the pages the Save button writes (see its doc).
- `detector_overlay_visible_for`: the one owner of whether the detector mask and blocks are
  drawn — while the Detector tool is switched on, or while the selected OCR engine's "create mask
  and text blocks" parameter is on (display only; the edit modes stay with the tool).

Notes:
GUI thread: no disk access here (the worker sends the page size along with the result; the
stored-detection load runs on its own worker).
*/

use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui;
use ms_models::text_mask_model::TextMaskModel;
use ms_project::ProjectData;
use ms_theme::Severity;

use super::{TranslationTabState, TranslationTool, detector_rects_to_blocks_px, rasterize_resolved_blocks};
use crate::panels::ocr::OcrPanelOptions;
use crate::ocr::{DetectMaskFlags, OcrEngine, OcrRegionWant, OcrRuntimeOptions};
use crate::panels::text_detector::TextDetectorAlgorithm;
use crate::text_detector::{
    RegionDetectEngine, RegionDetectRequest, RegionDetection, SURYA_DETECTOR_SPECS, TextDetectorPageResult, TextDetectorRect, empty_page_result, merge_region_blocks,
    merge_region_into_page,
};

/// Whether the text detector's mask and blocks are drawn on the canvas: while the Detector
/// tool is switched on (`detector_tool_on`, the tab's tool STATE — never the dock's output, which
/// runs after the canvas draw), and while the selected OCR engine's "create mask and text blocks" parameter is
/// on (PaddleOCR / Surya from `ocr_options`, NeuroLate from `neurolate_detect_mask`, the tab's
/// `NeurolateSettings::detect_mask`; the rule is `RegionDetectEngine::selection_detects`).
/// Display only: the interactive detector modes (mask brush, lines edit) stay tied to the tool,
/// which turns them off when it is switched off. The one owner of the overlay's visibility.
pub(super) fn detector_overlay_visible_for(detector_tool_on: bool, ocr_options: &OcrPanelOptions, neurolate_detect_mask: bool) -> bool {
    let flags = DetectMaskFlags { paddle: ocr_options.paddle_detect_mask, surya: ocr_options.surya_detect_mask, neurolate: neurolate_detect_mask };
    detector_tool_on || RegionDetectEngine::selection_detects(ocr_options.engine, flags)
}

/// The page selection an OCR ran on: what its region detection crops.
#[derive(Debug, Clone)]
pub(super) struct OcrSelection {
    pub(super) page_idx: usize,
    pub(super) page_path: PathBuf,
    /// Page-normalized selection (`OcrRecognizeRequest::uv_rect`).
    pub(super) uv_rect: [f32; 4],
}

/// How long the region-detection toasts stay up, seconds.
const REGION_TOAST_SECS: f64 = 3.0;

/// The detector panel algorithm of a region engine: its availability rule and its title.
fn region_algorithm(engine: RegionDetectEngine) -> TextDetectorAlgorithm {
    match engine {
        RegionDetectEngine::PaddleOcr => TextDetectorAlgorithm::PaddleOcr,
        RegionDetectEngine::Surya => TextDetectorAlgorithm::Surya,
        RegionDetectEngine::NeuroLate(_) => TextDetectorAlgorithm::NeuroLate,
    }
}

impl TranslationTabState {
    /// [`detector_overlay_visible_for`] of this tab's Detector tool state and OCR options.
    pub(super) fn detector_overlay_visible(&self) -> bool {
        detector_overlay_visible_for(self.tools.is_on(TranslationTool::TextDetector), &self.ocr_panel_options, self.neurolate_settings.detect_mask)
    }

    /// Starts the detector of the OCR `engine` on `selection` when `options` turn that engine's
    /// "create mask and text blocks" parameter on; otherwise does nothing. Starts the chapter's stored-detection load
    /// first when it has not run (its result is applied before the region's). A refused start
    /// (route unavailable, detector busy, worker gone) sets the detector status line and shows a
    /// toast, never silent.
    ///
    /// `carried_by` is the id of the dispatched OCR request whose answer carries the region
    /// (its request had an [`OcrRegionWant`] from [`Self::ocr_region_want_for`]): then no
    /// detector runs, the id waits in `pending_ocr_regions` for [`Self::receive_ocr_region`].
    pub(super) fn start_selection_region_detection(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        engine: OcrEngine,
        options: &OcrRuntimeOptions,
        selection: OcrSelection,
        carried_by: Option<u64>,
    ) {
        let Some(region_engine) = RegionDetectEngine::for_ocr(engine, options) else {
            return;
        };
        let algorithm = region_algorithm(region_engine);
        if let Some(request_id) = carried_by {
            // Checked by `ocr_region_want_for` before the request left; the stored detection
            // must still be in the stores before the answer's region is merged.
            self.ensure_text_detection_storage_loaded(project);
            self.pending_ocr_regions.insert(request_id);
            self.set_text_detector_status(tf!("translation.tab.region_detecting_status", engine = algorithm.title()), Severity::Info);
            return;
        }
        if let Err(reason) = self.detector_algorithm_availability(algorithm) {
            self.report_region_refusal(ctx, reason.status_text(algorithm), Severity::Error);
            return;
        }
        if self.text_detector_controller.is_busy() {
            self.report_region_refusal(ctx, t!("translation.tab.region_detect_busy_status").to_string(), Severity::Warning);
            return;
        }
        // The stored detection must be in the stores before a region is merged into them; the
        // load runs on its own worker and a result arriving first waits for it.
        self.ensure_text_detection_storage_loaded(project);
        let request = RegionDetectRequest {
            page_idx: selection.page_idx,
            page_path: selection.page_path,
            uv_rect: selection.uv_rect,
            engine: region_engine,
            mask_dilate_size: self.text_detector_options.mask_dilate_size,
        };
        match self.text_detector_controller.start_region_detection(request) {
            Ok(()) => {
                self.set_text_detector_status(tf!("translation.tab.region_detecting_status", engine = algorithm.title()), Severity::Info);
            }
            Err(error) => {
                self.report_region_refusal(ctx, tf!("translation.tab.region_detect_error", error = error), Severity::Error);
            }
        }
    }

    /// The region a quick selection's OCR request with `engine` / `options` on page `page_idx`
    /// must also return (`RegionDetectEngine::ocr_region_want`: NeuroLate with its parameter on),
    /// or `None` — always for other engines, and while the region could not start now (its route
    /// unavailable, the detector busy with a batch or another region): the OCR then reads text
    /// only and [`Self::start_selection_region_detection`] reports the refusal as for any engine.
    pub(super) fn ocr_region_want_for(&self, engine: OcrEngine, options: &OcrRuntimeOptions, page_idx: usize) -> Option<OcrRegionWant> {
        let want = RegionDetectEngine::ocr_region_want(engine, options, page_idx, self.text_detector_options.mask_dilate_size)?;
        let algorithm = RegionDetectEngine::for_ocr(engine, options).map(region_algorithm)?;
        let can_start = self.detector_algorithm_availability(algorithm).is_ok() && !self.text_detector_controller.is_busy();
        can_start.then_some(want)
    }

    /// Takes the region part of a finished OCR answer (`OcrControllerEvent::Recognized::region`)
    /// for request `request_id`: when that request's region is awaited (`pending_ocr_regions`), an
    /// `Ok` region goes through [`Self::receive_region_detection`] (the one wait / merge path) and
    /// an `Err` or a missing region is reported as a region failure. A region nobody waits for
    /// any more (dropped by Clear or a replacing batch) is discarded with a log line.
    pub(super) fn receive_ocr_region(&mut self, ctx: &egui::Context, request_id: u64, region: Option<Result<RegionDetection, String>>) {
        if !self.pending_ocr_regions.remove(&request_id) {
            if region.is_some() {
                ms_log::runtime_log::log_info(format!("[text-detector] region of OCR request {request_id} discarded: no longer awaited (cleared or replaced)"));
            }
            return;
        }
        match region {
            Some(Ok(region)) => self.receive_region_detection(ctx, region),
            // A cloud region has no model files to re-probe: the plain region failure report.
            Some(Err(error)) => self.report_region_refusal(ctx, tf!("translation.tab.region_detect_error", error = error), Severity::Error),
            None => {
                ms_log::runtime_log::log_error(format!("[text-detector] OCR request {request_id} answered without the awaited region (dispatch bug)"));
                let error = t!("translation.text_detector.neurolate_no_result_error");
                self.report_region_refusal(ctx, tf!("translation.tab.region_detect_error", error = error), Severity::Error);
            }
        }
    }

    /// The OCR request `request_id` ended without an answer (failed or stopped): when its region
    /// was awaited, the wait ends and the detector status line says why (`status`; the OCR's own
    /// toast already told the user, so no second one).
    pub(super) fn drop_ocr_region(&mut self, request_id: u64, status: String, severity: Severity) {
        if self.pending_ocr_regions.remove(&request_id) {
            self.set_text_detector_status(status, severity);
        }
    }

    /// Whether a page batch must not start now (the panel disables its detect / save / block-OCR
    /// buttons and the batch starters refuse): the detector worker runs a batch or a region, OR a
    /// quick selection's region is still awaited from its OCR answer (`pending_ocr_regions`). The
    /// second keeps the ordering rule the controller gives detector regions — a region and a
    /// batch never overlap, so a batch can neither overwrite a paid region nor drop it — until the
    /// carried answer is merged or dropped (Clear, which stays enabled, also drops it).
    pub(super) fn detector_busy_for_batches(&self) -> bool {
        self.text_detector_controller.is_busy() || !self.pending_ocr_regions.is_empty()
    }

    /// Shows a region-detection refusal or failure on the detector status line and as a toast.
    fn report_region_refusal(&mut self, ctx: &egui::Context, text: String, severity: Severity) {
        self.set_text_detector_status(text.clone(), severity);
        self.push_toast(ctx, text, severity, REGION_TOAST_SECS);
    }

    /// Whether a region result must wait: a storage job runs, or the chapter's stored detection
    /// has not been loaded (its load died; the next region dispatch or detector panel draw
    /// retries it). A session without chapter storage has nothing to wait for.
    fn region_merge_must_wait(&self) -> bool {
        self.text_detection_storage_busy || (self.session_caps.chapter_storage && self.text_detection_storage_loaded_for.is_none())
    }

    /// Takes a finished region detection: applied now, or held in `pending_region_detections`
    /// while the stored detection is not in the stores yet (`region_merge_must_wait`).
    pub(super) fn receive_region_detection(&mut self, ctx: &egui::Context, region: RegionDetection) {
        if self.region_merge_must_wait() {
            ms_log::runtime_log::log_info(format!(
                "[text-detector] region result of page {} held until the stored detection job ends",
                region.page_idx
            ));
            self.pending_region_detections.push(region);
            return;
        }
        self.apply_region_detection(ctx, region);
    }

    /// Applies the held region detections in arrival order once the stored detection is in the
    /// stores and no storage job runs (called every frame before the detector events; a no-op
    /// while none is held).
    pub(super) fn replay_pending_region_detections(&mut self, ctx: &egui::Context) {
        if self.region_merge_must_wait() || self.pending_region_detections.is_empty() {
            return;
        }
        for region in std::mem::take(&mut self.pending_region_detections) {
            self.apply_region_detection(ctx, region);
        }
    }

    /// Applies a finished region detection to both detector stores (see the file header): the
    /// selection's rect is replaced, the rest of the page is kept. A page with no detection
    /// yet gets one. A merge the stores refuse (a result that does not fit the page) is logged
    /// and reported; neither store is then changed.
    fn apply_region_detection(&mut self, ctx: &egui::Context, region: RegionDetection) {
        let page_idx = region.page_idx;
        let frozen_boxes = self.freeze_resolved_mask_before_region(page_idx, region.source_size);
        let had_result = self.text_detector_results.contains_key(&page_idx);
        let mut page = self.text_detector_results.remove(&page_idx).unwrap_or_else(|| empty_page_result(region.source_size));
        let merged = merge_region_into_page(&mut page, &region);
        // The model's boxes describe its mask. Outside the rect a frozen legacy page's mask is the
        // raster of its RESOLVED boxes, so the model gets those (merged by the same block rule),
        // while the tab's store keeps the raw lines the user edits.
        let page_blocks = merged.is_ok().then(|| match frozen_boxes {
            Some(mut boxes) => {
                merge_region_blocks(&mut boxes, &region);
                detector_rects_to_blocks_px(&boxes)
            }
            None => detector_rects_to_blocks_px(&page.blocks),
        });
        // `merge_region_into_page` leaves `page` unchanged on error, so the old entry goes back.
        if merged.is_ok() || had_result {
            self.text_detector_results.insert(page_idx, page);
        }
        let Some(page_blocks) = page_blocks else {
            if let Err(error) = merged {
                ms_log::runtime_log::log_error(format!(
                    "[text-detector] region result of page {page_idx} not applied: {error} (rect {:?}, page {:?}, mask {} bytes)",
                    region.rect,
                    region.source_size,
                    region.mask_alpha.len()
                ));
            }
            self.report_region_refusal(ctx, tf!("translation.tab.region_merge_error", page_idx = page_idx), Severity::Error);
            return;
        };
        // The merge re-sorts the page's blocks: an index into them is stale now.
        self.drop_line_edit_state_on_page(page_idx);
        self.text_detector_mask_textures.remove(&page_idx);
        self.merge_region_into_text_mask_model(&region, page_blocks);
        self.set_text_detector_status(tf!("translation.tab.region_detected_status", blocks = region.blocks.len()), Severity::Success);
    }

    /// A legacy result (blocks, no stored mask) is shown as its rasterized resolved blocks. Before
    /// a region is merged into it, that shown raster becomes the result's mask (and the shared
    /// model's, through the same materialization the display uses), so the page looks the same
    /// outside the selection afterwards. A page whose model mask was hand-edited keeps that mask.
    ///
    /// Returns the boxes the frozen raster was built from (the resolved, expanded / merged
    /// lines), or `None` when nothing was frozen or those boxes are unknown (a hand-edited model
    /// mask).
    fn freeze_resolved_mask_before_region(&mut self, page_idx: usize, source_size: [u32; 2]) -> Option<Vec<TextDetectorRect>> {
        let needs_freeze = self
            .text_detector_results
            .get(&page_idx)
            .is_some_and(|result| result.source_size == source_size && result.mask_alpha.is_empty() && !result.blocks.is_empty());
        if !needs_freeze {
            return None;
        }
        self.materialize_text_mask_page_from_blocks_if_missing(page_idx);
        let shown = self.text_mask_page_snapshot(page_idx).filter(|page| page.source_size == source_size && mask_is_well_formed(page.mask_size, &page.mask_alpha));
        let result = self.text_detector_results.get_mut(&page_idx)?;
        if let Some(shown) = shown {
            result.mask_size = shown.mask_size;
            result.mask_alpha = shown.mask_alpha;
            // The materialized page stores the resolved boxes its raster was built from.
            shown.blocks.map(|blocks| blocks.iter().filter_map(px_block_to_rect).collect())
        } else if let Some((mask_alpha, resolved)) = rasterize_resolved_blocks(result, &self.text_detector_options) {
            // No shared model (or no shown page): the same raster the display would build.
            result.mask_size = result.source_size;
            result.mask_alpha = mask_alpha;
            Some(resolved)
        } else {
            None
        }
    }

    /// Mirrors a region into the shared `TextMaskModel`: merged into the model's current page
    /// (brush edits kept outside the rect) with the tab's finalized `page_blocks` when it has
    /// one, else the whole merged result of the tab's store is mirrored (the model must never
    /// hold only the region of a known page).
    fn merge_region_into_text_mask_model(&mut self, region: &RegionDetection, page_blocks: Option<Vec<[i32; 4]>>) {
        let Some(model) = self.text_mask_model.clone() else {
            return;
        };
        let merged = match model.lock() {
            Ok(mut model) if model.page(region.page_idx).is_some_and(|page| page.source_size == region.source_size) => {
                Some(model.merge_region(region.page_idx, region.source_size, region.rect, &region.mask_alpha, page_blocks))
            }
            Ok(_) => None,
            Err(error) => {
                ms_log::runtime_log::log_error(format!("[text-detector] text mask model lock poisoned, region of page {} not mirrored: {error}", region.page_idx));
                return;
            }
        };
        match merged {
            Some(Ok(())) => {}
            Some(Err(error)) => {
                // The tab's store took the region; mirror its whole page so both agree.
                ms_log::runtime_log::log_error(format!(
                    "[text-detector] region of page {} not merged into the shared mask ({error}); mirroring the whole page",
                    region.page_idx
                ));
                self.sync_text_mask_page_from_result(region.page_idx);
            }
            None => self.sync_text_mask_page_from_result(region.page_idx),
        }
    }

    /// A region detection failed: status + toast, and for Surya a re-probe of its checkpoint so
    /// the detector panel draws the download button the error names.
    pub(super) fn region_detection_failed(&mut self, ctx: &egui::Context, engine: RegionDetectEngine, error: String) {
        match engine {
            RegionDetectEngine::Surya => self.ocr_model_downloads.request_bundle_status(&SURYA_DETECTOR_SPECS),
            RegionDetectEngine::PaddleOcr | RegionDetectEngine::NeuroLate(_) => {}
        }
        self.report_region_refusal(ctx, tf!("translation.tab.region_detect_error", error = error), Severity::Error);
    }

    /// Whether the shared model holds a page the tab's store does not (a page painted by hand,
    /// never detected): the Save button's second reason to be enabled. Short lock, no copy.
    pub(super) fn has_hand_painted_only_pages(&self) -> bool {
        let Some(model) = self.text_mask_model.as_ref() else {
            return false;
        };
        model.lock().is_ok_and(|model| model.pages().any(|(page_idx, _)| !self.text_detector_results.contains_key(&page_idx)))
    }
}

/// A model box (`[x1, y1, x2, y2]`, source pixels) as a detector rect; `None` when degenerate.
fn px_block_to_rect(block: &[i32; 4]) -> Option<TextDetectorRect> {
    // i32 -> f32 is exact for every page coordinate the detector accepts (its pixel limit keeps
    // sides far below 2^24); the boxes came from such rects (`detector_rects_to_blocks_px`).
    let [x1, y1, x2, y2] = block.map(|value| value as f32);
    TextDetectorRect::from_xyxy(x1, y1, x2, y2)
}

/// Whether a mask buffer holds exactly one byte per pixel of a non-empty `mask_size`.
fn mask_is_well_formed(mask_size: [u32; 2], mask_alpha: &[u8]) -> bool {
    !mask_alpha.is_empty() && usize::try_from(u64::from(mask_size[0]) * u64::from(mask_size[1])).is_ok_and(|len| len == mask_alpha.len())
}

/// The pages the detector's Save button writes, sorted by page index. Called with the shared
/// model locked: it copies only the masks it writes.
///
/// - A detected page keeps its blocks (line edits included). Its mask is the one the user sees:
///   the model page's (brush edits and region merges included) when it describes the same
///   source size and is well-formed — except for a legacy page whose result has no mask and
///   whose model page is the untouched display raster of its blocks (boxes still known): that
///   page is written exactly as before, with no mask.
/// - A page only the model has (painted by hand, never detected) is written with no blocks
///   when its mask has a set pixel.
pub(super) fn detection_pages_for_save(results: &HashMap<usize, TextDetectorPageResult>, shown: Option<&TextMaskModel>) -> Vec<(usize, TextDetectorPageResult)> {
    let mut pages: Vec<(usize, TextDetectorPageResult)> = results
        .iter()
        .map(|(&page_idx, result)| {
            let shown_page = shown.and_then(|model| model.page(page_idx)).filter(|page| {
                page.source_size == result.source_size
                    && mask_is_well_formed(page.mask_size, &page.mask_alpha)
                    // A mask-less legacy result whose model page still carries boxes shows the
                    // untouched raster of those boxes: not a user mask, never frozen by Save.
                    && !(result.mask_alpha.is_empty() && page.blocks.is_some())
            });
            let (mask_size, mask_alpha) = match shown_page {
                Some(page) => (page.mask_size, page.mask_alpha.clone()),
                None => (result.mask_size, result.mask_alpha.clone()),
            };
            (page_idx, TextDetectorPageResult { source_size: result.source_size, blocks: result.blocks.clone(), mask_size, mask_alpha })
        })
        .collect();
    if let Some(model) = shown {
        pages.extend(
            model
                .pages()
                .filter(|(page_idx, page)| {
                    !results.contains_key(page_idx) && mask_is_well_formed(page.mask_size, &page.mask_alpha) && page.mask_alpha.iter().any(|&px| px != 0)
                })
                .map(|(page_idx, page)| {
                    (
                        page_idx,
                        TextDetectorPageResult { source_size: page.source_size, blocks: Vec::new(), mask_size: page.mask_size, mask_alpha: page.mask_alpha.clone() },
                    )
                }),
        );
    }
    pages.sort_by_key(|(page_idx, _)| *page_idx);
    pages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_detector::TextDetectorRect;
    use std::sync::{Arc, Mutex};

    fn rect(x1: f32, y1: f32, x2: f32, y2: f32) -> TextDetectorRect {
        TextDetectorRect::from_xyxy(x1, y1, x2, y2).unwrap_or_else(|| panic!("valid test rect {x1} {y1} {x2} {y2}"))
    }

    fn result(source_size: [u32; 2], mask: Vec<u8>) -> TextDetectorPageResult {
        TextDetectorPageResult { source_size, blocks: Vec::new(), mask_size: source_size, mask_alpha: mask }
    }

    /// One model page for `model_with`: index, size (source = mask), mask, boxes.
    type ModelPage = (usize, [u32; 2], Vec<u8>, Option<Vec<[i32; 4]>>);

    fn model_with(pages: &[ModelPage]) -> TextMaskModel {
        let mut model = TextMaskModel::new();
        for (page_idx, size, mask, blocks) in pages {
            model.set_page_with_blocks(*page_idx, *size, *size, mask.clone(), blocks.clone());
        }
        model
    }

    /// The saved mask is the one the user sees (the model's, brush edits included).
    #[test]
    fn save_takes_the_shown_mask_of_a_detected_page() {
        let results = HashMap::from([(0, result([2, 1], vec![0, 0]))]);
        let model = model_with(&[(0, [2, 1], vec![255, 0], None)]);
        let pages = detection_pages_for_save(&results, Some(&model));
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].1.mask_alpha, vec![255, 0]);
    }

    /// A model page for another source size, or a malformed one, never replaces the result's mask.
    #[test]
    fn save_keeps_the_result_mask_when_the_shown_page_does_not_match() {
        let results = HashMap::from([(0, result([2, 1], vec![0, 255])), (1, result([2, 1], vec![255, 0]))]);
        let mut model = model_with(&[(0, [4, 1], vec![255; 4], None)]);
        model.set_page(1, [2, 1], [2, 1], vec![255]);
        let pages = detection_pages_for_save(&results, Some(&model));
        assert_eq!(pages[0].1.mask_alpha, vec![0, 255]);
        assert_eq!(pages[1].1.mask_alpha, vec![255, 0]);
    }

    /// A legacy page (blocks, no stored mask) whose model page is the untouched display raster
    /// is saved as before (no mask); once hand-edited (boxes unknown) its mask is saved.
    #[test]
    fn save_never_freezes_the_display_raster_of_an_untouched_legacy_page() {
        let legacy = TextDetectorPageResult { source_size: [2, 1], blocks: vec![rect(0.0, 0.0, 1.0, 1.0)], mask_size: [2, 1], mask_alpha: Vec::new() };
        let results = HashMap::from([(0, legacy)]);
        let untouched = model_with(&[(0, [2, 1], vec![255, 0], Some(vec![[0, 0, 1, 1]]))]);
        assert!(detection_pages_for_save(&results, Some(&untouched))[0].1.mask_alpha.is_empty());
        let edited = model_with(&[(0, [2, 1], vec![255, 255], None)]);
        assert_eq!(detection_pages_for_save(&results, Some(&edited))[0].1.mask_alpha, vec![255, 255]);
    }

    /// A hand-painted page with no detection is saved (no blocks); an all-zero one is not.
    #[test]
    fn save_adds_hand_painted_pages_and_skips_empty_ones() {
        let results = HashMap::new();
        let model = model_with(&[(3, [2, 1], vec![0, 255], None), (1, [2, 1], vec![0, 0], None)]);
        let pages = detection_pages_for_save(&results, Some(&model));
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].0, 3);
        assert!(pages[0].1.blocks.is_empty());
        assert!(detection_pages_for_save(&results, None).is_empty());
    }

    #[test]
    fn region_engines_map_to_their_detector_algorithms() {
        assert_eq!(region_algorithm(RegionDetectEngine::PaddleOcr), TextDetectorAlgorithm::PaddleOcr);
        assert_eq!(region_algorithm(RegionDetectEngine::Surya), TextDetectorAlgorithm::Surya);
        let params = ms_ai_api::neurolate::OcrParams {
            language: ms_ai_api::neurolate::NeurolateLanguage::Ko,
            comic_type: ms_ai_api::neurolate::NeurolateComicType::Webtoon,
        };
        assert_eq!(region_algorithm(RegionDetectEngine::NeuroLate(params)), TextDetectorAlgorithm::NeuroLate);
    }

    fn tab_with_model() -> (TranslationTabState, Arc<Mutex<TextMaskModel>>) {
        let mut tab = TranslationTabState::default();
        let model = Arc::new(Mutex::new(TextMaskModel::new()));
        tab.text_mask_model = Some(Arc::clone(&model));
        (tab, model)
    }

    /// A 6x4 page detected once: a full mask and blocks left, middle and right.
    fn stored_page() -> TextDetectorPageResult {
        TextDetectorPageResult {
            source_size: [6, 4],
            blocks: vec![rect(0.0, 0.0, 1.0, 1.0), rect(2.0, 1.0, 4.0, 3.0), rect(4.5, 0.0, 6.0, 1.0)],
            mask_size: [6, 4],
            mask_alpha: vec![255; 24],
        }
    }

    fn region_on_page_0(mask_alpha: Vec<u8>) -> RegionDetection {
        RegionDetection {
            page_idx: 0,
            engine: RegionDetectEngine::PaddleOcr,
            source_size: [6, 4],
            rect: [2, 1, 4, 3],
            mask_alpha,
            blocks: vec![rect(2.0, 2.0, 3.0, 3.0)],
        }
    }

    /// A region result that arrives while the stored detection is still loading waits; once the
    /// load has delivered the page, the region replaces only its rect in both stores.
    #[test]
    fn region_result_before_the_stored_load_waits_and_merges_after_it() {
        let (mut tab, model) = tab_with_model();
        let ctx = egui::Context::default();
        tab.text_detection_storage_busy = true;
        tab.receive_region_detection(&ctx, region_on_page_0(vec![0, 255, 0, 0]));
        tab.replay_pending_region_detections(&ctx);
        assert!(tab.text_detector_results.is_empty(), "held while the load runs");

        tab.apply_loaded_detection_pages(vec![(0, stored_page())]);
        tab.text_detection_storage_busy = false;
        tab.text_detection_storage_loaded_for = Some(PathBuf::from("/home/u/title/chapter"));
        tab.replay_pending_region_detections(&ctx);

        let page = tab.text_detector_results.get(&0).expect("page kept");
        assert_eq!(page.blocks, vec![rect(0.0, 0.0, 1.0, 1.0), rect(4.5, 0.0, 6.0, 1.0), rect(2.0, 2.0, 3.0, 3.0)]);
        assert_eq!(page.mask_alpha.iter().filter(|&&px| px == 0).count(), 3, "only the rect changed");
        let model = model.lock().expect("model lock");
        let shown = model.page(0).expect("model page");
        assert_eq!(shown.mask_alpha, page.mask_alpha, "both stores hold the same mask");
        assert_eq!(shown.blocks, detector_rects_to_blocks_px(&page.blocks), "both stores hold the same blocks");
        assert!(tab.pending_region_detections.is_empty());
    }

    /// The tab merge and the model merge give the same mask, a scaled (half-size) stored mask
    /// included, and a line-edit selection on the page is dropped.
    #[test]
    fn tab_and_model_merges_agree_on_a_scaled_mask_and_drop_the_line_selection() {
        let (mut tab, model) = tab_with_model();
        let ctx = egui::Context::default();
        let scaled = TextDetectorPageResult { source_size: [6, 4], blocks: Vec::new(), mask_size: [3, 2], mask_alpha: vec![255; 6] };
        tab.apply_loaded_detection_pages(vec![(0, scaled)]);
        tab.text_detection_storage_loaded_for = Some(PathBuf::from("/home/u/title/chapter"));
        tab.text_detector_line_selection = Some(super::super::TextDetectorLineSelection { page_idx: 0, line_idx: 0 });
        tab.receive_region_detection(&ctx, region_on_page_0(vec![0, 0, 0, 0]));
        let page = tab.text_detector_results.get(&0).expect("page kept");
        assert_eq!(page.mask_size, [3, 2]);
        // Mask pixel (1, 0) centres at source (3, 1) and (1, 1) at (3, 3): only the first is in
        // `[2, 1, 4, 3]`.
        assert_eq!(page.mask_alpha, vec![255, 0, 255, 255, 255, 255]);
        assert_eq!(model.lock().expect("model lock").page(0).map(|shown| shown.mask_alpha.clone()), Some(page.mask_alpha.clone()));
        assert!(tab.text_detector_line_selection.is_none());
    }

    /// A legacy page (blocks, no stored mask) keeps its shown raster outside the selection.
    #[test]
    fn region_on_a_legacy_page_keeps_its_shown_raster_outside() {
        let (mut tab, model) = tab_with_model();
        let ctx = egui::Context::default();
        let legacy = TextDetectorPageResult { source_size: [6, 4], blocks: vec![rect(0.0, 0.0, 1.0, 1.0)], mask_size: [6, 4], mask_alpha: Vec::new() };
        tab.apply_loaded_detection_pages(vec![(0, legacy)]);
        tab.text_detection_storage_loaded_for = Some(PathBuf::from("/home/u/title/chapter"));
        let expected_outside = rasterize_resolved_blocks(tab.text_detector_results.get(&0).expect("legacy page"), &tab.text_detector_options)
            .map(|(mask, _)| mask)
            .expect("legacy raster");
        tab.receive_region_detection(&ctx, region_on_page_0(Vec::new()));
        let page = tab.text_detector_results.get(&0).expect("page kept");
        for y in 0..4usize {
            for x in 0..6usize {
                let inside = (2..4).contains(&x) && (1..3).contains(&y);
                let idx = y * 6 + x;
                let expected = if inside { 0 } else { expected_outside[idx] };
                assert_eq!(page.mask_alpha[idx], expected, "pixel ({x}, {y})");
            }
        }
        assert_eq!(model.lock().expect("model lock").page(0).map(|shown| shown.mask_alpha.clone()), Some(page.mask_alpha.clone()));
    }

    /// After a region merge onto a legacy page the model's boxes are the RESOLVED (expanded)
    /// boxes its raster was built from outside the rect plus the region's, merged by the same
    /// block rule; the tab's store keeps the raw lines.
    #[test]
    fn region_on_a_legacy_page_hands_the_model_its_resolved_boxes() {
        let (mut tab, model) = tab_with_model();
        let ctx = egui::Context::default();
        tab.text_detector_options.block_expand_px = 2;
        let legacy = TextDetectorPageResult { source_size: [6, 4], blocks: vec![rect(0.0, 0.0, 1.0, 1.0)], mask_size: [6, 4], mask_alpha: Vec::new() };
        let (_, resolved) = rasterize_resolved_blocks(&legacy, &tab.text_detector_options).expect("legacy raster");
        tab.apply_loaded_detection_pages(vec![(0, legacy)]);
        tab.text_detection_storage_loaded_for = Some(PathBuf::from("/home/u/title/chapter"));
        let region = region_on_page_0(Vec::new());
        let mut expected = resolved;
        merge_region_blocks(&mut expected, &region);
        tab.receive_region_detection(&ctx, region);
        let page = tab.text_detector_results.get(&0).expect("page kept");
        assert_eq!(page.blocks, vec![rect(0.0, 0.0, 1.0, 1.0), rect(2.0, 2.0, 3.0, 3.0)], "raw lines in the tab");
        let shown = model.lock().expect("model lock").page(0).and_then(|page| page.blocks.clone());
        assert_eq!(shown, detector_rects_to_blocks_px(&expected));
        assert_eq!(shown, Some(vec![[0, 0, 3, 3], [2, 2, 3, 3]]), "the expanded box, not the raw one");
    }

    /// A load that died leaves the stored detection unloaded: held regions keep waiting (never
    /// applied onto empty pages) until a load succeeds.
    #[test]
    fn held_regions_wait_through_a_dead_load_until_a_load_succeeds() {
        let (mut tab, _model) = tab_with_model();
        let ctx = egui::Context::default();
        tab.text_detection_storage_busy = true;
        tab.receive_region_detection(&ctx, region_on_page_0(vec![0, 255, 0, 0]));
        // The load worker died: idle, but nothing was loaded.
        tab.text_detection_storage_busy = false;
        tab.replay_pending_region_detections(&ctx);
        assert!(tab.text_detector_results.is_empty(), "not applied onto an unloaded page");
        assert_eq!(tab.pending_region_detections.len(), 1);
        // The retried load succeeds.
        tab.apply_loaded_detection_pages(vec![(0, stored_page())]);
        tab.text_detection_storage_loaded_for = Some(PathBuf::from("/home/u/title/chapter"));
        tab.replay_pending_region_detections(&ctx);
        let page = tab.text_detector_results.get(&0).expect("page kept");
        assert_eq!(page.blocks.len(), 3, "stored blocks outside the rect plus the region's");
        assert!(tab.pending_region_detections.is_empty());
    }

    /// Clear (and a replacing page batch) drops region results still waiting for a load.
    #[test]
    fn clear_drops_held_region_results() {
        let (mut tab, _model) = tab_with_model();
        let ctx = egui::Context::default();
        tab.text_detection_storage_busy = true;
        tab.receive_region_detection(&ctx, region_on_page_0(vec![255; 4]));
        tab.clear_detector_result_stores();
        tab.text_detection_storage_busy = false;
        tab.text_detection_storage_loaded_for = Some(PathBuf::from("/home/u/title/chapter"));
        tab.replay_pending_region_detections(&ctx);
        assert!(tab.text_detector_results.is_empty());
        assert!(tab.pending_region_detections.is_empty());
    }

    /// The overlay shows with the Detector tool on, or while the selected engine's own
    /// "create mask" parameter is on; another engine's flag or another tool does not.
    #[test]
    fn detector_overlay_visibility_follows_the_tool_or_the_engine_parameter() {
        let options = |engine: OcrEngine, paddle: bool, surya: bool| OcrPanelOptions { engine, paddle_detect_mask: paddle, surya_detect_mask: surya, ..Default::default() };
        assert!(detector_overlay_visible_for(true, &options(OcrEngine::MangaOcr, false, false), false));
        assert!(!detector_overlay_visible_for(false, &options(OcrEngine::MangaOcr, true, true), true));
        assert!(detector_overlay_visible_for(false, &options(OcrEngine::PaddleOcr, true, false), false));
        assert!(!detector_overlay_visible_for(false, &options(OcrEngine::PaddleOcr, false, true), true));
        assert!(detector_overlay_visible_for(false, &options(OcrEngine::Surya, false, true), false));
        assert!(!detector_overlay_visible_for(false, &options(OcrEngine::Surya, true, false), true));
        assert!(detector_overlay_visible_for(false, &options(OcrEngine::NeuroLate, false, false), true));
        assert!(!detector_overlay_visible_for(false, &options(OcrEngine::NeuroLate, true, true), false));
    }

    /// The OCR runtime options of `engine` with NeuroLate's "create mask" set to `detect_mask`.
    fn ocr_options_for(engine: OcrEngine, detect_mask: bool) -> OcrRuntimeOptions {
        let settings = crate::neurolate::NeurolateSettings { detect_mask, ..Default::default() };
        super::super::build_ocr_runtime_options(&OcrPanelOptions { engine, ..Default::default() }, "ko", &settings, None)
    }

    /// A quick NeuroLate selection asks its OCR request for the region (idle detector); other
    /// engines, and NeuroLate with its parameter off, never do.
    #[test]
    fn only_a_neurolate_selection_rides_on_its_ocr_request() {
        let tab = TranslationTabState::default();
        let want = tab.ocr_region_want_for(OcrEngine::NeuroLate, &ocr_options_for(OcrEngine::NeuroLate, true), 4);
        assert_eq!(want, Some(OcrRegionWant { page_idx: 4, mask_dilate_size: tab.text_detector_options.mask_dilate_size }));
        assert_eq!(tab.ocr_region_want_for(OcrEngine::NeuroLate, &ocr_options_for(OcrEngine::NeuroLate, false), 4), None);
        for engine in [OcrEngine::PaddleOcr, OcrEngine::Surya, OcrEngine::MangaOcr, OcrEngine::AiApi] {
            assert_eq!(tab.ocr_region_want_for(engine, &ocr_options_for(engine, true), 4), None, "{engine:?}");
        }
    }

    /// A page path that does not exist (the detector worker fails on it without touching disk
    /// further).
    fn absent_page() -> PathBuf {
        std::env::temp_dir().join(format!("ms_tab_translation_absent_page_{}.png", std::process::id()))
    }

    /// Occupies the tab's detector controller with a Classic batch on an absent page: busy at
    /// once (the worker only fails to decode it; no model, backend or network).
    fn occupy_detector(tab: &mut TranslationTabState) {
        assert!(!absent_page().exists());
        tab.text_detector_controller
            .start_detection(vec![(0, absent_page())], false, crate::text_detector::TextDetectorRunMode::Classic, 0)
            .unwrap_or_else(|error| panic!("detector starts: {error}"));
        assert!(tab.text_detector_controller.is_busy());
    }

    /// A minimal `ProjectData` whose every path is the fictional `/home/u/title/chapter` (nothing
    /// here reads or writes it).
    fn fictional_project() -> ProjectData {
        let dir = PathBuf::from("/home/u/title/chapter");
        ProjectData {
            project_dir: dir.clone(),
            image_dir: dir.clone(),
            pages: Vec::new(),
            bubbles: Arc::new(Vec::new()),
            paths: ms_project::ProjectPaths {
                project_dir: dir.clone(),
                title_dir: dir.clone(),
                notes_file: dir.clone(),
                char_favorites_file: dir.clone(),
                color_presets_file: dir.clone(),
                bubbles_file: dir.clone(),
                src_dir: dir.clone(),
                clean_layers_dir: dir.clone(),
                cleaned_dir: dir.clone(),
                alt_vers_dir: dir.clone(),
                saved_dir: dir.clone(),
                image_bubbles_dir: dir.clone(),
                text_images_dir: dir.clone(),
                layers_dir: dir.clone(),
                text_detection_dir: dir.clone(),
                characters_dir: dir.clone(),
                terms_file: dir.clone(),
                settings_file: dir.clone(),
                unsaved_dir: dir.clone(),
                unsaved_bubbles_file: dir.clone(),
                unsaved_clean_layers_dir: dir.clone(),
                unsaved_image_bubbles_dir: dir.clone(),
                unsaved_text_images_dir: dir.clone(),
                unsaved_layers_dir: dir,
            },
            comic_type: None,
            canvas_settings: ms_project::CanvasSettings::default(),
            settings_data: serde_json::Value::Null,
            session: Default::default(),
        }
    }

    /// While the detector is busy a NeuroLate selection asks for NO region: its OCR is the one
    /// paid call (TEXT), and the start path then shows the usual busy refusal.
    #[test]
    fn a_busy_detector_wants_no_region_so_one_selection_is_one_call() {
        let mut tab = TranslationTabState::default();
        occupy_detector(&mut tab);
        let options = ocr_options_for(OcrEngine::NeuroLate, true);
        assert_eq!(tab.ocr_region_want_for(OcrEngine::NeuroLate, &options, 0), None);
    }

    /// A carried region only records its request id (and the "detecting" status): no detector
    /// run starts. A selection without a carrier takes today's detector path, refused while busy.
    #[test]
    fn a_carried_region_starts_no_detector_run() {
        let mut tab = TranslationTabState::default();
        let ctx = egui::Context::default();
        let project = fictional_project();
        // The stored-detection load is not started here (a job is "running").
        tab.text_detection_storage_busy = true;
        let options = ocr_options_for(OcrEngine::NeuroLate, true);
        let selection = OcrSelection { page_idx: 0, page_path: absent_page(), uv_rect: [0.0, 0.0, 1.0, 1.0] };
        tab.start_selection_region_detection(&ctx, &project, OcrEngine::NeuroLate, &options, selection.clone(), Some(11));
        assert_eq!(tab.pending_ocr_regions, std::collections::HashSet::from([11]));
        assert!(!tab.text_detector_controller.is_busy(), "no detector run for a carried region");
        assert_eq!(tab.text_detector_status_severity, Severity::Info);

        let mut busy = TranslationTabState::default();
        occupy_detector(&mut busy);
        busy.text_detection_storage_busy = true;
        busy.start_selection_region_detection(&ctx, &project, OcrEngine::NeuroLate, &options, selection, None);
        assert!(busy.pending_ocr_regions.is_empty());
        assert_eq!(busy.text_detector_status_severity, Severity::Warning, "busy refusal");
    }

    /// An awaited OCR region blocks page batches like a running region detection; once it is
    /// merged or dropped, a batch may start again.
    #[test]
    fn an_awaited_ocr_region_blocks_page_batches() {
        let mut tab = TranslationTabState::default();
        let project = fictional_project();
        assert!(!tab.detector_busy_for_batches());
        tab.pending_ocr_regions.insert(3);
        assert!(tab.detector_busy_for_batches());
        tab.set_text_detector_status("before", Severity::Info);
        tab.start_text_detection_for_all_pages(&project);
        assert_eq!(tab.text_detector_status, "before", "the batch start was refused");
        assert!(!tab.text_detector_controller.is_busy());

        tab.drop_ocr_region(3, "stopped".to_string(), Severity::Info);
        assert!(!tab.detector_busy_for_batches());
        // Not refused by the gate any more: this project has no pages, so the controller itself
        // answers (an error status), which proves the start went through.
        tab.start_text_detection_for_all_pages(&project);
        assert_eq!(tab.text_detector_status_severity, Severity::Error);
    }

    fn neurolate_region_on_page_0() -> RegionDetection {
        let params = ms_ai_api::neurolate::OcrParams { language: ms_ai_api::neurolate::NeurolateLanguage::Ko, comic_type: ms_ai_api::neurolate::NeurolateComicType::Webtoon };
        RegionDetection { engine: RegionDetectEngine::NeuroLate(params), ..region_on_page_0(vec![0, 255, 0, 0]) }
    }

    /// An OCR-carried region waits like a detector's while the stored detection loads, is
    /// merged once by the same path, and ends the wait of its request.
    #[test]
    fn ocr_carried_region_waits_for_the_stored_load_and_merges_once() {
        let (mut tab, model) = tab_with_model();
        let ctx = egui::Context::default();
        tab.pending_ocr_regions.insert(7);
        tab.text_detection_storage_busy = true;
        tab.receive_ocr_region(&ctx, 7, Some(Ok(neurolate_region_on_page_0())));
        assert!(tab.pending_ocr_regions.is_empty(), "the request's wait ended");
        assert!(tab.text_detector_results.is_empty(), "held while the load runs");
        assert_eq!(tab.pending_region_detections.len(), 1);

        tab.apply_loaded_detection_pages(vec![(0, stored_page())]);
        tab.text_detection_storage_busy = false;
        tab.text_detection_storage_loaded_for = Some(PathBuf::from("/home/u/title/chapter"));
        tab.replay_pending_region_detections(&ctx);
        tab.replay_pending_region_detections(&ctx);
        let page = tab.text_detector_results.get(&0).expect("page kept");
        assert_eq!(page.blocks, vec![rect(0.0, 0.0, 1.0, 1.0), rect(4.5, 0.0, 6.0, 1.0), rect(2.0, 2.0, 3.0, 3.0)], "merged once");
        assert_eq!(page.mask_alpha.iter().filter(|&&px| px == 0).count(), 3, "only the rect changed");
        assert_eq!(model.lock().expect("model lock").page(0).map(|shown| shown.mask_alpha.clone()), Some(page.mask_alpha.clone()));
        // A second answer for the same request id is no longer awaited.
        tab.receive_ocr_region(&ctx, 7, Some(Ok(neurolate_region_on_page_0())));
        assert!(tab.pending_region_detections.is_empty());
    }

    /// A failed or stopped OCR, and an unusable region, end the wait and say so on the detector
    /// status line; nothing is merged.
    #[test]
    fn failed_cancelled_or_unusable_ocr_regions_clear_the_wait_and_the_status() {
        let (mut tab, _model) = tab_with_model();
        let ctx = egui::Context::default();
        tab.text_detection_storage_loaded_for = Some(PathBuf::from("/home/u/title/chapter"));
        tab.pending_ocr_regions.extend([1, 2, 3]);
        tab.drop_ocr_region(1, "failed".to_string(), Severity::Error);
        assert_eq!((tab.text_detector_status.as_str(), tab.text_detector_status_severity), ("failed", Severity::Error));
        tab.drop_ocr_region(2, "stopped".to_string(), Severity::Info);
        assert_eq!((tab.text_detector_status.as_str(), tab.text_detector_status_severity), ("stopped", Severity::Info));
        tab.receive_ocr_region(&ctx, 3, Some(Err("bad size".to_string())));
        assert_eq!(tab.text_detector_status_severity, Severity::Error);
        assert!(tab.text_detector_status.contains("bad size"));
        assert!(tab.pending_ocr_regions.is_empty());
        assert!(tab.text_detector_results.is_empty() && tab.pending_region_detections.is_empty());
        // A request whose region nobody awaits leaves the status alone.
        tab.drop_ocr_region(9, "other".to_string(), Severity::Warning);
        assert_ne!(tab.text_detector_status, "other");
    }

    /// Clear drops the awaited OCR regions: their answers are discarded, never merged into the
    /// cleared stores.
    #[test]
    fn clear_drops_awaited_ocr_regions() {
        let (mut tab, _model) = tab_with_model();
        let ctx = egui::Context::default();
        tab.text_detection_storage_loaded_for = Some(PathBuf::from("/home/u/title/chapter"));
        tab.pending_ocr_regions.insert(5);
        tab.clear_detector_result_stores();
        tab.receive_ocr_region(&ctx, 5, Some(Ok(neurolate_region_on_page_0())));
        assert!(tab.text_detector_results.is_empty());
        assert!(tab.pending_region_detections.is_empty());
    }
}
