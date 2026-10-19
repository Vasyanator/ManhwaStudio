/*
FILE OVERVIEW: crates/ms-tab-translation/src/text_detector/region_detect.rs
Region detection for an OCR selection: the detector of the selected OCR engine (PaddleOCR or
Surya) runs on exactly the page pixels the OCR crop reads, and its mask and blocks replace the
page's detector result inside the selection.

Key items:
- `RegionDetectEngine`: the OCR engines that are also text detectors, and the one rule of when a
  selection OCR also detects (`for_ocr`: engine + that engine's own "create mask" parameter).
- `RegionDetectRequest`: what the tab asks the detector worker for (page, uv selection, engine,
  dilation).
- `RegionDetection`: the worker's answer — the selection's pixel rect, its mask at the rect size
  and its blocks in PAGE pixels.
- `run_region_detection()`: worker side (decode, `page_crop` rect, crop, `pipeline::detect_image`,
  Rust dilation, offset to page pixels).
- `merge_region_into_page()` / `empty_page_result()`: pure "replace inside the rect" merge into a
  page result.

Notes:
The mask raster rule of the merge has one owner, `ms_models::text_mask_model::replace_mask_region`
(also used by `TextMaskModel::merge_region` for the shared model). The block rule lives only here
(`region_contains_block_centre` + `finalize_blocks`): the shared model receives the merged,
finalized block set of the tab's store instead of re-deciding it. Dilation is applied to the
crop, so it stops at the selection border. Worker-thread only, except the pure merge.
*/

use super::pipeline::{self, DetectFailure, DetectorRoute};
use super::{TextDetectorPageResult, TextDetectorPaddleOcrOptions, TextDetectorRect, TextDetectorRunMode, TextDetectorSuryaOptions, dilate_mask_alpha};
use crate::ocr::{OcrEngine, OcrRuntimeOptions};
use crate::page_crop;
use ms_models::text_mask_model::{RegionMergeError, replace_mask_region};
use ms_sysprobe::ai_models::EnsureCtx;
use ms_text_detect::{DetectParams, EngineKind};
use std::path::PathBuf;

/// The OCR engines whose text detector a selection OCR can also run. `pub` only because the
/// public `TextDetectorControllerEvent` carries it; nothing outside the crate uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionDetectEngine {
    PaddleOcr,
    Surya,
}

impl RegionDetectEngine {
    /// The detector a selection OCR with `engine` also runs: `Some` only for PaddleOCR and Surya
    /// with that engine's own "create mask and text blocks" parameter on. The one owner of the
    /// trigger rule.
    #[must_use]
    pub(crate) fn for_ocr(engine: OcrEngine, options: &OcrRuntimeOptions) -> Option<Self> {
        Self::from_flags(engine, options.paddle_detect_mask, options.surya_detect_mask)
    }

    /// The rule of [`Self::for_ocr`] over the two engine parameters themselves (the panel's
    /// `OcrPanelOptions` hold the same flags): `Some` only for PaddleOCR / Surya with their own
    /// flag on.
    #[must_use]
    pub(crate) fn from_flags(engine: OcrEngine, paddle_detect_mask: bool, surya_detect_mask: bool) -> Option<Self> {
        match engine {
            OcrEngine::PaddleOcr => paddle_detect_mask.then_some(Self::PaddleOcr),
            OcrEngine::Surya => surya_detect_mask.then_some(Self::Surya),
            OcrEngine::MangaOcr | OcrEngine::EasyOcr | OcrEngine::PaddleVl | OcrEngine::Baberu | OcrEngine::AiApi => None,
        }
    }

    /// The page-detector mode of the same engine (backend gate and model ensure reuse it).
    #[must_use]
    pub(crate) fn run_mode(self) -> TextDetectorRunMode {
        match self {
            Self::PaddleOcr => TextDetectorRunMode::PaddleOcr(TextDetectorPaddleOcrOptions::default()),
            Self::Surya => TextDetectorRunMode::Surya(TextDetectorSuryaOptions),
        }
    }

    fn engine_kind(self) -> EngineKind {
        match self {
            Self::PaddleOcr => EngineKind::Paddle,
            Self::Surya => EngineKind::Surya,
        }
    }
}

/// A region detection the tab asks the detector worker for.
#[derive(Debug, Clone)]
pub(crate) struct RegionDetectRequest {
    pub(crate) page_idx: usize,
    pub(crate) page_path: PathBuf,
    /// The OCR selection, page-normalized (`OcrRecognizeRequest::uv_rect`).
    pub(crate) uv_rect: [f32; 4],
    pub(crate) engine: RegionDetectEngine,
    /// Square dilation radius of the region mask (the detector panel's setting; clamped 0..=30).
    pub(crate) mask_dilate_size: i32,
}

/// The detector's answer for one selection. `pub` only because the public
/// `TextDetectorControllerEvent` carries it; nothing outside the crate uses it.
#[derive(Debug, Clone, PartialEq)]
pub struct RegionDetection {
    pub page_idx: usize,
    pub engine: RegionDetectEngine,
    /// Size `[w, h]` of the decoded page.
    pub source_size: [u32; 2],
    /// The selection in page pixels, `[x1, y1, x2, y2]` with exclusive far edges (`page_crop`).
    pub rect: [u32; 4],
    /// 0/255 mask at the rect's size (`x2 - x1` by `y2 - y1`), dilated; empty when the engine
    /// returned no mask.
    pub mask_alpha: Vec<u8>,
    /// Detected blocks in PAGE pixels, in reading order.
    pub blocks: Vec<TextDetectorRect>,
}

/// Runs `request.engine`'s detector on the selection of the page file. Decodes the page, cuts the
/// selection with the OCR crop rule (`page_crop::uv_rect_to_pixel_rect`), detects on the crop
/// (`route` decides PaddleOCR native vs backend, as for pages), dilates the crop mask and offsets
/// the blocks to page pixels. Worker-thread only (decode, inference / IPC, model ensure).
///
/// # Errors
/// [`DetectFailure::Cancelled`] when `ctx.cancel` stopped a model download, else `Failed` with
/// the localized open / empty-page / selection-too-small message or `detect_image`'s error.
pub(super) fn run_region_detection(request: &RegionDetectRequest, route: DetectorRoute, ctx: &mut EnsureCtx<'_>) -> Result<RegionDetection, DetectFailure> {
    let page = pipeline::decode_page_rgb(&request.page_path)?;
    let source_size = [page.width(), page.height()];
    let Some(rect) = page_crop::uv_rect_to_pixel_rect(request.uv_rect, source_size[0], source_size[1]) else {
        return Err(DetectFailure::Failed(t!("translation.ocr.selection_too_small_error").to_string()));
    };
    let crop = image::imageops::crop_imm(&page, rect.x, rect.y, rect.width, rect.height).to_image();
    ms_log::runtime_log::log_info(format!(
        "[text-detector] region {:?} on page {} ({}): rect {:?}",
        request.engine,
        request.page_idx,
        request.page_path.display(),
        rect.xyxy()
    ));
    let detection = pipeline::detect_image(request.engine.engine_kind(), &crop, &DetectParams::default(), route, ctx)?;
    let mut mask_alpha = detection.mask.alpha;
    dilate_mask_alpha(&mut mask_alpha, detection.mask.size, request.mask_dilate_size);
    let blocks = offset_blocks(&detection.blocks, rect.x, rect.y);
    Ok(RegionDetection { page_idx: request.page_idx, engine: request.engine, source_size, rect: rect.xyxy(), mask_alpha, blocks })
}

/// Whether a block's centre lies inside the region rect (`[x1, y1, x2, y2]`, exclusive far
/// edges): such blocks are replaced by the region's own blocks.
fn region_contains_block_centre(rect: [u32; 4], block: &TextDetectorRect) -> bool {
    let cx = (f64::from(block.x1) + f64::from(block.x2)) * 0.5;
    let cy = (f64::from(block.y1) + f64::from(block.y2)) * 0.5;
    cx >= f64::from(rect[0]) && cx < f64::from(rect[2]) && cy >= f64::from(rect[1]) && cy < f64::from(rect[3])
}

/// Moves crop-pixel blocks to page pixels (the crop's top-left is `(dx, dy)`).
fn offset_blocks(blocks: &[TextDetectorRect], dx: u32, dy: u32) -> Vec<TextDetectorRect> {
    // u32 -> f32 is exact for every page side the detector accepts (its pixel limit keeps sides
    // far below 2^24), which is the space the blocks already live in.
    let (dx, dy) = (dx as f32, dy as f32);
    blocks
        .iter()
        .filter_map(|block| TextDetectorRect::from_xyxy(block.x1 + dx, block.y1 + dy, block.x2 + dx, block.y2 + dy))
        .collect()
}

/// Merges a region detection into the page's detector result in place: inside `region.rect`
/// the mask is replaced by the region's mask and the blocks centred in the rect by the region's
/// blocks; outside it nothing changes; the blocks are then finalized (`ms_text_detect` order and
/// cap). A page with no detection yet is an [`empty_page_result`].
///
/// A result for another source size (the page image changed) restarts from an empty
/// source-size mask with no blocks; a result without a mask (empty `mask_alpha`) gets a
/// source-size mask holding only the region; a mask stored at another raster size is merged
/// through the pixel-centre mapping of `replace_mask_region`.
///
/// # Errors
/// [`RegionMergeError`] when the region does not fit the page or a buffer length is wrong;
/// `page` is then unchanged.
pub(crate) fn merge_region_into_page(page: &mut TextDetectorPageResult, region: &RegionDetection) -> Result<(), RegionMergeError> {
    if page.source_size == region.source_size && !page.mask_alpha.is_empty() {
        // Validated before any write: an error leaves the mask untouched.
        replace_mask_region(&mut page.mask_alpha, page.mask_size, page.source_size, region.rect, &region.mask_alpha)?;
    } else {
        let len = usize::try_from(u64::from(region.source_size[0]) * u64::from(region.source_size[1]))
            .map_err(|_| RegionMergeError::RectOutsidePage { rect: region.rect, source_size: region.source_size })?;
        let mut mask_alpha = vec![0u8; len];
        replace_mask_region(&mut mask_alpha, region.source_size, region.source_size, region.rect, &region.mask_alpha)?;
        if page.source_size != region.source_size {
            page.source_size = region.source_size;
            page.blocks.clear();
        }
        page.mask_size = region.source_size;
        page.mask_alpha = mask_alpha;
    }
    page.blocks.retain(|block| !region_contains_block_centre(region.rect, block));
    page.blocks.extend_from_slice(&region.blocks);
    ms_text_detect::blocks::finalize_blocks(&mut page.blocks);
    Ok(())
}

/// The result of a page with no detection yet: no blocks and no mask (`merge_region_into_page`
/// allocates the mask).
#[must_use]
pub(crate) fn empty_page_result(source_size: [u32; 2]) -> TextDetectorPageResult {
    TextDetectorPageResult { source_size, blocks: Vec::new(), mask_size: [0, 0], mask_alpha: Vec::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x1: f32, y1: f32, x2: f32, y2: f32) -> TextDetectorRect {
        TextDetectorRect::from_xyxy(x1, y1, x2, y2).unwrap_or_else(|| panic!("valid test rect {x1} {y1} {x2} {y2}"))
    }

    fn region(rect_px: [u32; 4], mask_alpha: Vec<u8>, blocks: Vec<TextDetectorRect>) -> RegionDetection {
        RegionDetection { page_idx: 0, engine: RegionDetectEngine::PaddleOcr, source_size: [6, 4], rect: rect_px, mask_alpha, blocks }
    }

    fn full_page() -> TextDetectorPageResult {
        TextDetectorPageResult {
            source_size: [6, 4],
            blocks: vec![rect(0.0, 0.0, 1.0, 1.0), rect(2.0, 1.0, 4.0, 3.0), rect(4.5, 0.0, 6.0, 1.0)],
            mask_size: [6, 4],
            mask_alpha: vec![255; 24],
        }
    }

    /// Inside `[2, 1, 4, 3]` the mask becomes the region's and the block centred there is
    /// replaced; the mask and blocks outside stay; blocks end in reading order.
    #[test]
    fn merge_replaces_inside_and_keeps_outside() {
        let mut page = full_page();
        let new_block = rect(2.0, 2.0, 3.0, 3.0);
        merge_region_into_page(&mut page, &region([2, 1, 4, 3], vec![0, 255, 0, 0], vec![new_block])).expect("valid merge");
        let row = |y: usize| page.mask_alpha[y * 6..y * 6 + 6].to_vec();
        assert_eq!(row(0), vec![255; 6]);
        assert_eq!(row(1), vec![255, 255, 0, 255, 255, 255]);
        assert_eq!(row(2), vec![255, 255, 0, 0, 255, 255]);
        assert_eq!(row(3), vec![255; 6]);
        assert_eq!(page.blocks, vec![rect(0.0, 0.0, 1.0, 1.0), rect(4.5, 0.0, 6.0, 1.0), new_block]);
    }

    /// No text found in the selection: its rect is cleared and its blocks removed.
    #[test]
    fn empty_region_result_clears_the_selection() {
        let mut page = full_page();
        merge_region_into_page(&mut page, &region([2, 1, 4, 3], Vec::new(), Vec::new())).expect("valid merge");
        assert_eq!(page.mask_alpha.iter().filter(|&&px| px == 0).count(), 4);
        assert_eq!(page.blocks.len(), 2);
    }

    /// A selection on the page's far corner (the `page_crop` clamp keeps it inside) merges at
    /// the edge; a page without a detection gets a source-size mask with only the region.
    #[test]
    fn merge_at_the_page_edge_into_a_page_without_detection() {
        let rect_px = page_crop::uv_rect_to_pixel_rect([0.9, 0.8, 1.2, 1.5], 6, 4).map(page_crop::PixelRect::xyxy);
        assert_eq!(rect_px, Some([5, 3, 6, 4]));
        let mut page = empty_page_result([6, 4]);
        merge_region_into_page(&mut page, &region([5, 3, 6, 4], vec![255], vec![rect(5.0, 3.0, 6.0, 4.0)])).expect("valid merge");
        assert_eq!(page.mask_size, [6, 4]);
        assert_eq!(page.mask_alpha.iter().position(|&px| px != 0), Some(23));
        assert_eq!(page.mask_alpha.iter().filter(|&&px| px != 0).count(), 1);
        assert_eq!(page.blocks, vec![rect(5.0, 3.0, 6.0, 4.0)]);
    }

    /// A result stored for another page size (the image changed) restarts from the region.
    #[test]
    fn merge_into_a_stale_page_restarts_it() {
        let mut page = TextDetectorPageResult { source_size: [3, 3], blocks: vec![rect(0.0, 0.0, 1.0, 1.0)], mask_size: [3, 3], mask_alpha: vec![255; 9] };
        merge_region_into_page(&mut page, &region([0, 0, 1, 1], vec![255], Vec::new())).expect("valid merge");
        assert_eq!((page.source_size, page.mask_size), ([6, 4], [6, 4]));
        assert!(page.blocks.is_empty());
        assert_eq!(page.mask_alpha.iter().filter(|&&px| px != 0).count(), 1);
    }

    /// A malformed region (rect outside the page, wrong mask length) leaves the page unchanged.
    #[test]
    fn invalid_region_leaves_the_page_unchanged() {
        let mut page = full_page();
        let before = page.clone();
        assert!(merge_region_into_page(&mut page, &region([4, 0, 7, 2], Vec::new(), Vec::new())).is_err());
        assert!(merge_region_into_page(&mut page, &region([0, 0, 2, 2], vec![255; 3], Vec::new())).is_err());
        assert_eq!(page.mask_alpha, before.mask_alpha);
        assert_eq!(page.blocks, before.blocks);
    }

    #[test]
    fn region_centre_rule_uses_exclusive_far_edges() {
        assert!(region_contains_block_centre([0, 0, 10, 10], &rect(2.0, 2.0, 4.0, 4.0)));
        assert!(!region_contains_block_centre([0, 0, 10, 10], &rect(8.0, 8.0, 12.0, 12.0)), "centre on the far edge");
        assert!(region_contains_block_centre([0, 0, 10, 10], &rect(-4.0, -4.0, 4.0, 4.0)), "centre at the near corner");
    }

    #[test]
    fn blocks_are_offset_to_page_pixels() {
        assert_eq!(offset_blocks(&[rect(1.0, 2.0, 3.0, 4.0)], 10, 20), vec![rect(11.0, 22.0, 13.0, 24.0)]);
    }

    /// Only PaddleOCR and Surya detect, each by its own parameter.
    #[test]
    fn only_paddle_and_surya_detect_by_their_own_parameter() {
        let options = |paddle: bool, surya: bool| OcrRuntimeOptions {
            manga_model: String::new(),
            paddle_lang: String::new(),
            paddle_vl_script: String::new(),
            paddle_vl_model: ms_sysprobe::ai_models::external_catalog::PaddleVlVariant::default(),
            easy_langs: String::new(),
            surya_task_name: String::new(),
            surya_recognize_math: false,
            surya_sort_lines: false,
            surya_drop_repeated_text: false,
            surya_max_sliding_window: 0,
            surya_max_tokens: 0,
            paddle_detect_mask: paddle,
            surya_detect_mask: surya,
            ai_api_service: ms_ai_api::AiApiService::OpenAi,
            ai_api_base_url: String::new(),
            ai_api_model: String::new(),
            ai_api_system_instruction: String::new(),
            ai_api_source_lang: String::new(),
        };
        assert_eq!(RegionDetectEngine::for_ocr(OcrEngine::PaddleOcr, &options(true, false)), Some(RegionDetectEngine::PaddleOcr));
        assert_eq!(RegionDetectEngine::for_ocr(OcrEngine::PaddleOcr, &options(false, true)), None);
        assert_eq!(RegionDetectEngine::for_ocr(OcrEngine::Surya, &options(false, true)), Some(RegionDetectEngine::Surya));
        assert_eq!(RegionDetectEngine::for_ocr(OcrEngine::Surya, &options(true, false)), None);
        for engine in [OcrEngine::MangaOcr, OcrEngine::EasyOcr, OcrEngine::PaddleVl, OcrEngine::Baberu, OcrEngine::AiApi] {
            assert_eq!(RegionDetectEngine::for_ocr(engine, &options(true, true)), None, "{engine:?}");
        }
    }
}
