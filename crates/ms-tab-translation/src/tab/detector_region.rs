/*
FILE OVERVIEW: crates/ms-tab-translation/src/tab/detector_region.rs
The Translation tab's side of "an OCR selection also creates the detector mask and text blocks"
and of what the detector's Save button writes.

Main items:
- `TranslationTabState::start_selection_region_detection`: the trigger of a selection OCR
  (quick rect, the advanced window's Create bubble on its ORIGINAL page selection). Runs only
  when `RegionDetectEngine::for_ocr` says so (PaddleOCR / Surya with that engine's own
  parameter on), starts the chapter's stored-detection load if it has not run, checks the
  detector route like a page run, and refuses with a toast and a status line while the detector
  is busy (nothing is queued in the controller).
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
  so the page looks the same outside the selection afterwards.
- `detection_pages_for_save`: the pages the Save button writes (see its doc).
- `detector_overlay_visible_for`: the one owner of whether the detector mask and blocks are
  drawn — with the Detector panel open, or while the selected OCR engine's "create mask and text
  blocks" parameter is on (display only; the edit modes stay with the panel).

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

use super::{TranslationPanel, TranslationTabState, detector_rects_to_blocks_px, rasterize_resolved_blocks};
use crate::panels::ocr::OcrPanelOptions;
use crate::ocr::{OcrEngine, OcrRuntimeOptions};
use crate::panels::text_detector::TextDetectorAlgorithm;
use crate::text_detector::{
    RegionDetectEngine, RegionDetectRequest, RegionDetection, SURYA_DETECTOR_SPECS, TextDetectorPageResult, empty_page_result,
    merge_region_into_page,
};

/// Whether the text detector's mask and blocks are drawn on the canvas: while the Detector
/// panel is open, and while the selected OCR engine's "create mask and text blocks" parameter is
/// on (PaddleOCR / Surya, `RegionDetectEngine::from_flags`). Display only: the interactive
/// detector modes (mask brush, lines edit) stay tied to the panel, which turns them off when it
/// closes. The one owner of the overlay's visibility.
pub(super) fn detector_overlay_visible_for(active_panel: TranslationPanel, ocr_options: &OcrPanelOptions) -> bool {
    active_panel == TranslationPanel::TextDetector
        || RegionDetectEngine::from_flags(ocr_options.engine, ocr_options.paddle_detect_mask, ocr_options.surya_detect_mask).is_some()
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
    }
}

impl TranslationTabState {
    /// [`detector_overlay_visible_for`] of this tab's current panel and OCR options.
    pub(super) fn detector_overlay_visible(&self) -> bool {
        detector_overlay_visible_for(self.active_panel, &self.ocr_panel_options)
    }

    /// Starts the detector of the OCR `engine` on `selection` when `options` turn that engine's
    /// "create mask and text blocks" parameter on; otherwise does nothing. Starts the chapter's stored-detection load
    /// first when it has not run (its result is applied before the region's). A refused start
    /// (route unavailable, detector busy, worker gone) sets the detector status line and shows a
    /// toast, never silent.
    pub(super) fn start_selection_region_detection(
        &mut self,
        ctx: &egui::Context,
        project: &ProjectData,
        engine: OcrEngine,
        options: &OcrRuntimeOptions,
        selection: OcrSelection,
    ) {
        let Some(region_engine) = RegionDetectEngine::for_ocr(engine, options) else {
            return;
        };
        let algorithm = region_algorithm(region_engine);
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
        self.freeze_resolved_mask_before_region(page_idx, region.source_size);
        let had_result = self.text_detector_results.contains_key(&page_idx);
        let mut page = self.text_detector_results.remove(&page_idx).unwrap_or_else(|| empty_page_result(region.source_size));
        let merged = merge_region_into_page(&mut page, &region);
        let page_blocks = merged.is_ok().then(|| detector_rects_to_blocks_px(&page.blocks));
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
        let line_edit_on_page = self.text_detector_line_selection.is_some_and(|selection| selection.page_idx == page_idx)
            || self.text_detector_line_drag_state.is_some_and(|drag| drag.selection.page_idx == page_idx);
        if line_edit_on_page {
            self.clear_text_detector_line_edit_state();
        }
        self.text_detector_mask_textures.remove(&page_idx);
        self.merge_region_into_text_mask_model(&region, page_blocks);
        self.set_text_detector_status(tf!("translation.tab.region_detected_status", blocks = region.blocks.len()), Severity::Success);
    }

    /// A legacy result (blocks, no stored mask) is shown as its rasterized resolved blocks. Before
    /// a region is merged into it, that shown raster becomes the result's mask (and the shared
    /// model's, through the same materialization the display uses), so the page looks the same
    /// outside the selection afterwards. A page whose model mask was hand-edited keeps that mask.
    fn freeze_resolved_mask_before_region(&mut self, page_idx: usize, source_size: [u32; 2]) {
        let needs_freeze = self
            .text_detector_results
            .get(&page_idx)
            .is_some_and(|result| result.source_size == source_size && result.mask_alpha.is_empty() && !result.blocks.is_empty());
        if !needs_freeze {
            return;
        }
        self.materialize_text_mask_page_from_blocks_if_missing(page_idx);
        let shown = self.text_mask_page_snapshot(page_idx).filter(|page| page.source_size == source_size && mask_is_well_formed(page.mask_size, &page.mask_alpha));
        let Some(result) = self.text_detector_results.get_mut(&page_idx) else {
            return;
        };
        if let Some(shown) = shown {
            result.mask_size = shown.mask_size;
            result.mask_alpha = shown.mask_alpha;
        } else if let Some((mask_alpha, _)) = rasterize_resolved_blocks(result, &self.text_detector_options) {
            // No shared model (or no shown page): the same raster the display would build.
            result.mask_size = result.source_size;
            result.mask_alpha = mask_alpha;
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
        if engine == RegionDetectEngine::Surya {
            self.ocr_model_downloads.request_bundle_status(&SURYA_DETECTOR_SPECS);
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

    /// The overlay shows with the Detector panel open, or while the selected engine's own
    /// "create mask" parameter is on; another engine's flag or another panel does not.
    #[test]
    fn detector_overlay_visibility_follows_the_panel_or_the_engine_parameter() {
        let options = |engine: OcrEngine, paddle: bool, surya: bool| OcrPanelOptions { engine, paddle_detect_mask: paddle, surya_detect_mask: surya, ..Default::default() };
        assert!(detector_overlay_visible_for(TranslationPanel::TextDetector, &options(OcrEngine::MangaOcr, false, false)));
        assert!(!detector_overlay_visible_for(TranslationPanel::None, &options(OcrEngine::MangaOcr, true, true)));
        assert!(detector_overlay_visible_for(TranslationPanel::Ocr, &options(OcrEngine::PaddleOcr, true, false)));
        assert!(!detector_overlay_visible_for(TranslationPanel::Ocr, &options(OcrEngine::PaddleOcr, false, true)));
        assert!(detector_overlay_visible_for(TranslationPanel::None, &options(OcrEngine::Surya, false, true)));
        assert!(!detector_overlay_visible_for(TranslationPanel::Bubbles, &options(OcrEngine::Surya, true, false)));
    }
}
