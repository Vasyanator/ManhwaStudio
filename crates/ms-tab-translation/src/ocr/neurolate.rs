/*
FILE OVERVIEW: crates/ms-tab-translation/src/ocr/neurolate.rs
The OCR worker step of the NeuroLate cloud engine (`OcrEngine::NeuroLate`): one recognize
request -> the crop (the shared `page_crop` rule) -> ONE paid NeuroLate request -> the
recognized text and, for a quick selection with "create mask and text blocks" on, the detector
region of the same answer.

Key functions:
- `run_neurolate_ocr()`: the whole step on the OCR worker (key read, crop, request, answer).
- `request_mode()`: `MASK_AND_TEXT` exactly when the region is wanted and the crop has a page
  placement (no advanced-window override), else `TEXT`.
- `recognized_text()`: the blocks' texts in the common reading order, through the one line-join
  owner `super::assemble_line_ocr_result`.
- `region_outcome()`: the region through the ONE NeuroLate answer -> mask + blocks owner,
  `text_detector::neurolate::region_from_image`.

Notes:
A paid request is sent at most once (the client never retries; the tab skips its block-OCR
retry for this engine). The request runs under the controller's `GenerationTracker` run, so
the panel's Stop and the controller drop abandon the wait at once (`OcrFailure::Cancelled`; the
request may still be charged and finishes detached). Which blocks count, their reading-order
rectangle and the image-0 / per-image-error step are shared with the detector
(`text_detector::neurolate::{usable_block, block_text, block_rect, finalize_paired, image_result}`): an unusable block (error,
EMPTY / FAILED / unknown status) contributes neither text here nor a mask there; a usable block
without text contributes no text. Worker-thread only: credential-store read,
PNG encode and the HTTP wait block.
*/

use super::{CropPlacement, OcrFailure, OcrRecognizeRequest, OcrRecognizeResult, OcrRegionWant, PageImageCache, assemble_line_ocr_result, crop_image_with_placement, encode_png};
use crate::text_detector::neurolate::{block_rect, block_text, finalize_paired, image_result, region_from_image};
use crate::text_detector::{RegionDetectEngine, RegionDetection, TextDetectorRect};
use ms_ai_api::GenerationTracker;
use ms_ai_api::neurolate::{self, NeurolateError, OcrBlock, OcrImageResult, OcrMode, OcrParams, OcrRequest};

/// The region part of a NeuroLate answer: `Some` exactly when the request wanted one (`Err` =
/// the localized reason it is unusable; the text is published anyway).
pub(super) type RegionOutcome = Option<Result<RegionDetection, String>>;

/// Recognizes `request` with NeuroLate: begins a generation run (so the panel shows the wait and
/// its Stop), reads the key, crops the selection (or decodes the override), sends ONE request in
/// [`request_mode`] and turns the answer into the text ([`recognized_text`]) and, when
/// `request.region` is set, the region ([`region_outcome`]). Post-processing is the caller's.
///
/// # Errors
/// [`OcrFailure::Cancelled`] when the controller is closed or the run was stopped (before or
/// during the wait); else `Failed` with the localized key / crop / encode / request message, the
/// "no result" message, or the per-image failure.
pub(super) fn run_neurolate_ocr(
    request: &OcrRecognizeRequest,
    page_cache: &mut PageImageCache,
    generation: &GenerationTracker,
) -> Result<(OcrRecognizeResult, RegionOutcome), OcrFailure> {
    // The controller is gone: skip the crop and the (possibly blocking) keyring read.
    if generation.is_closed() {
        return Err(OcrFailure::Cancelled);
    }
    let run = generation.begin(request.request_id);
    let key = neurolate::read_key().map_err(|error| {
        ms_log::runtime_log::log_warn(format!("[ocr] NeuroLate request {} refused: {error:?}", request.request_id));
        OcrFailure::Failed(error.to_string())
    })?;
    let (crop, placement) = crop_image_with_placement(request, page_cache)?;
    let mode = request_mode(request.region, placement);
    ms_log::runtime_log::log_info(format!(
        "[ocr] NeuroLate request {}: mode {}, image {}x{}, region wanted {}, page {}",
        request.request_id,
        mode.wire(),
        crop.width(),
        crop.height(),
        request.region.is_some(),
        request.page_path.display()
    ));
    let png = encode_png(crop)?;
    let params = request.options.neurolate;
    let abandon = || run.is_cancelled();
    // `kind: None`: a selection crop, so the server's size default (BLOCK up to 690 px a side)
    // decides how it is read.
    let response = match neurolate::recognize(&key, OcrRequest { mode, kind: None, params, images_png: vec![png] }, &abandon) {
        Ok(response) => response,
        Err(NeurolateError::Cancelled) => return Err(OcrFailure::Cancelled),
        // The client already logged the failure with its context.
        Err(error) => return Err(OcrFailure::Failed(error.to_string())),
    };
    let request_id = response.request_id.clone();
    let image = image_result(response, "[ocr]").map_err(OcrFailure::Failed)?;
    let result = recognized_text(&image, request.join_newlines, request.reflect_strings);
    let region = region_outcome(request.region, placement, params, &image);
    if let Some(Err(error)) = &region {
        ms_log::runtime_log::log_warn(format!("[ocr] NeuroLate answer {request_id}: region unusable: {error}"));
    }
    Ok((result, region))
}

/// The mode of a request: `MASK_AND_TEXT` (text plus masks, the region's source) only when the
/// region is wanted AND the crop lies on a page (`placement`); an override image (the advanced
/// window's edited preview) has no page mapping, so it is read in `TEXT` mode like block OCR.
fn request_mode(region: Option<OcrRegionWant>, placement: Option<CropPlacement>) -> OcrMode {
    if region.is_some() && placement.is_some() { OcrMode::MaskAndText } else { OcrMode::Text }
}

/// The texts of `blocks` (`block_text`, the detector's rule) in the one block order of their
/// rectangles (`block_rect`: bbox, else polygon extents — the rectangle the detector stores for
/// the same block — ordered by `finalize_paired`); blocks without any rectangle follow in the
/// server's order.
fn texts_in_reading_order(blocks: &[OcrBlock]) -> Vec<&str> {
    let mut placed: Vec<(TextDetectorRect, &str)> = Vec::new();
    let mut unplaced: Vec<&str> = Vec::new();
    for block in blocks {
        let Some(text) = block_text(block) else {
            continue;
        };
        match block_rect(block) {
            Some(rect) => placed.push((rect, text)),
            None => unplaced.push(text),
        }
    }
    let mut texts: Vec<&str> = finalize_paired(placed).into_iter().map(|(_rect, text)| text).collect();
    texts.extend(unplaced);
    texts
}

/// The recognized result of an answer: the block texts in reading order, one line each, through
/// the shared line join (`join_newlines` picks the separator, `reflect_strings` reverses the
/// order, blank lines are dropped).
fn recognized_text(image: &OcrImageResult, join_newlines: bool, reflect_strings: bool) -> OcrRecognizeResult {
    let text = texts_in_reading_order(&image.blocks).join("\n");
    assemble_line_ocr_result(&text, join_newlines, reflect_strings)
}

/// The region of an answer: `None` when none was wanted; else the region of the crop at
/// `placement` built by the one conversion owner (`region_from_image`: usable masks only, dilated
/// at the crop size, blocks in page pixels), or the localized reason it cannot be built (no page
/// placement, an answer for another size, the mask limit).
fn region_outcome(want: Option<OcrRegionWant>, placement: Option<CropPlacement>, params: OcrParams, image: &OcrImageResult) -> RegionOutcome {
    let want = want?;
    let Some(placement) = placement else {
        // The tab never asks for a region on an override image; answer the wait instead of
        // leaving it pending.
        ms_log::runtime_log::log_error("[ocr] NeuroLate region wanted for an image without a page placement (dispatch bug)");
        return Some(Err(t!("translation.text_detector.neurolate_no_result_error").to_string()));
    };
    Some(region_from_image(want.page_idx, RegionDetectEngine::NeuroLate(params), placement.source_size, placement.rect, image, want.mask_dilate_size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page_crop::PixelRect;
    use ms_ai_api::neurolate::protocol::parse_ocr_response;
    use ms_ai_api::neurolate::{JobError, NeurolateComicType, NeurolateLanguage, OcrBox, OcrPolygon, TextStatus};

    const OCR_PAGE: &str = include_str!("../../../ms-ai-api/fixtures/neurolate/ocr_page.json");
    const OCR_REGIONS_EMPTY: &str = include_str!("../../../ms-ai-api/fixtures/neurolate/ocr_regions_empty.json");

    const PARAMS: OcrParams = OcrParams { language: NeurolateLanguage::Ko, comic_type: NeurolateComicType::Webtoon };
    const WANT: OcrRegionWant = OcrRegionWant { page_idx: 2, mask_dilate_size: 0 };

    fn first_image(fixture: &str) -> OcrImageResult {
        let response = parse_ocr_response(200, fixture.as_bytes(), 1).unwrap_or_else(|error| panic!("fixture parses: {error:?}"));
        response.images.into_iter().next().unwrap_or_else(|| panic!("fixture has an image"))
    }

    fn whole_page() -> CropPlacement {
        CropPlacement { rect: PixelRect { x: 0, y: 0, width: 690, height: 1500 }, source_size: [690, 1500] }
    }

    const KO_1: &str = "안녕하세요! 오늘 날씨가 정말 좋네요.";
    const KO_2: &str = "어디 가는 거야? 같이 가자!";
    const JA_3: &str = "こんにちは! 元気ですか?";

    #[test]
    fn mode_is_mask_and_text_only_for_a_wanted_region_on_a_page_crop() {
        assert_eq!(request_mode(Some(WANT), Some(whole_page())), OcrMode::MaskAndText);
        assert_eq!(request_mode(None, Some(whole_page())), OcrMode::Text, "block OCR / no region");
        assert_eq!(request_mode(Some(WANT), None), OcrMode::Text, "override image");
        assert_eq!(request_mode(None, None), OcrMode::Text);
    }

    /// The page answer's blocks join in reading order: line breaks kept or a space, and the
    /// reflected order reversed.
    #[test]
    fn block_texts_join_in_reading_order_through_the_line_join() {
        let image = first_image(OCR_PAGE);
        let kept = recognized_text(&image, true, false);
        assert_eq!(kept.lines, vec![KO_1, KO_2, JA_3]);
        assert_eq!(kept.text, format!("{KO_1}\n{KO_2}\n{JA_3}"));
        assert_eq!(recognized_text(&image, false, false).text, format!("{KO_1} {KO_2} {JA_3}"));
        let reflected = recognized_text(&image, true, true);
        assert_eq!(reflected.lines, vec![JA_3, KO_2, KO_1]);
    }

    /// The order is the boxes' reading order, not the server's.
    #[test]
    fn server_order_is_replaced_by_the_box_order() {
        let mut image = first_image(OCR_PAGE);
        image.blocks.reverse();
        assert_eq!(recognized_text(&image, true, false).lines, vec![KO_1, KO_2, JA_3]);
    }

    /// An EMPTY region (text null, its rectangle as mask) adds no text; neither does a block
    /// with an error, a FAILED or unknown status (the block the mask skips too) or blank text.
    /// A block with neither bbox nor polygons comes last; one with polygons only is ordered by
    /// their extents, like the detector's block.
    #[test]
    fn empty_failed_and_blank_blocks_contribute_no_text() {
        let image = first_image(OCR_REGIONS_EMPTY);
        assert_eq!(recognized_text(&image, true, false).lines, vec![KO_1, KO_2]);
        let block = |text: &str, status: Option<TextStatus>, error: Option<JobError>, y: i32| OcrBlock {
            number: 1,
            region: None,
            text: Some(text.to_string()),
            polygons: if y >= 0 { vec![OcrPolygon { outer: vec![[0, y], [4, y], [4, y + 4]], holes: Vec::new() }] } else { Vec::new() },
            bbox: (y >= 10).then_some(OcrBox { x: 0, y, width: 4, height: 4 }),
            text_status: status,
            error,
        };
        let image = OcrImageResult {
            index: 0,
            width: 10,
            height: 10,
            mode_applied: Some(OcrMode::Text),
            blocks: vec![
                block("unplaced", None, None, -1),
                block("failed", Some(TextStatus::Failed), None, 0),
                block("errored", None, Some(JobError { code: "X".to_string(), message: String::new() }), 1),
                block("   ", Some(TextStatus::Ok), None, 2),
                block("unknown", Some(TextStatus::Unknown("PARTIAL".to_string())), None, 3),
                block("boxed", Some(TextStatus::Ok), None, 12),
                block("polygon only", None, None, 4),
            ],
            error: None,
        };
        assert_eq!(recognized_text(&image, true, false).lines, vec!["polygon only", "boxed", "unplaced"]);
        // The same blocks in the detector: the unknown-status block adds no mask either.
        let (_mask, rects) = crate::text_detector::neurolate::detection_from_image(&image, [10, 20]).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(rects.len(), 3, "the usable blocks with polygons (blank text, boxed, polygon only); never the unknown-status one");
    }

    /// A wanted region comes from the same answer (the EMPTY block's rectangle never enters
    /// the mask); no region is built when none was wanted; a wanted region without a page
    /// placement is answered with an error instead of staying pending.
    #[test]
    fn region_is_built_from_the_same_answer_only_when_wanted() {
        let image = first_image(OCR_REGIONS_EMPTY);
        assert_eq!(region_outcome(None, Some(whole_page()), PARAMS, &image), None);
        let region = region_outcome(Some(WANT), Some(whole_page()), PARAMS, &image)
            .unwrap_or_else(|| panic!("region wanted"))
            .unwrap_or_else(|error| panic!("valid region: {error}"));
        assert_eq!((region.page_idx, region.engine, region.rect, region.source_size), (2, RegionDetectEngine::NeuroLate(PARAMS), [0, 0, 690, 1500], [690, 1500]));
        assert_eq!(region.blocks.len(), 2, "the EMPTY region adds no block");
        assert_eq!(region.mask_alpha[1100 * 690 + 550], 0, "inside the EMPTY region");
        assert!(matches!(region_outcome(Some(WANT), None, PARAMS, &image), Some(Err(_))));
        let wrong_size = CropPlacement { rect: PixelRect { x: 0, y: 0, width: 300, height: 200 }, source_size: [690, 1500] };
        assert!(matches!(region_outcome(Some(WANT), Some(wrong_size), PARAMS, &image), Some(Err(_))), "answer for another size");
    }

    /// A `MASK_AND_TEXT` answer the service applied as `TEXT` (or without polygons) yields an
    /// error region, never an empty one that would clear the stored mask; the text stays.
    #[test]
    fn a_text_only_answer_is_a_region_error_not_an_empty_region() {
        let mut image = first_image(OCR_PAGE);
        image.mode_applied = Some(OcrMode::Text);
        assert!(matches!(region_outcome(Some(WANT), Some(whole_page()), PARAMS, &image), Some(Err(_))), "TEXT applied");
        assert_eq!(recognized_text(&image, true, false).lines, vec![KO_1, KO_2, JA_3], "text still read");
        let mut image = first_image(OCR_PAGE);
        for block in &mut image.blocks {
            block.polygons.clear();
        }
        assert!(matches!(region_outcome(Some(WANT), Some(whole_page()), PARAMS, &image), Some(Err(_))), "no polygons");
        // An answer with no blocks at all is a real "no text here" region.
        let mut empty = first_image(OCR_PAGE);
        empty.blocks.clear();
        let region = region_outcome(Some(WANT), Some(whole_page()), PARAMS, &empty).unwrap_or_else(|| panic!("wanted")).unwrap_or_else(|error| panic!("{error}"));
        assert!(region.blocks.is_empty());
    }
}
