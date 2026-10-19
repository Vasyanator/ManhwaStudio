/*
FILE OVERVIEW: crates/ms-tab-translation/src/text_detector/neurolate.rs
The NeuroLate cloud text detector: sends a page (or an OCR selection's crop) to NeuroLate in
`MASK` mode and turns the answer into the detector's types. The one owner of the NeuroLate
answer -> mask + blocks conversion, used by the detector mode here and by the NeuroLate OCR
engine's "create mask and text blocks" (`region_from_image`).

Key items:
- `NeurolateKey`: the API key read once per batch / region (redacted `Debug`).
- `detect_page()`: page file -> `TextDetectorPageResult` (worker side of the detector mode).
- `recognize_image()`: one RGB image -> PNG -> one paid `MASK` request -> its image result.
- `page_mode()` / `page_texts_from_image()`: a page detection with "recognize text right away"
  sends `MASK_AND_TEXT` and returns the block texts aligned with the finalized blocks
  (`finalize_paired`, `block_text`).
- `detection_from_image()`: an image result -> 0/255 mask at `size` + finalized blocks.
- `usable_block()` / `block_rect()` / `image_result()`: the shared block rule, block rectangle
  and answer -> image-0 step (also used by the OCR engine).
- `page_result_from_image()` / `region_from_image()`: the validated page / region forms.

Notes:
The one block rule is `usable_block` (no error, status absent or OK; an unknown status counts
as unusable): only usable blocks contribute a mask and a block here, and only usable blocks
contribute text to the NeuroLate OCR engine (`crate::ocr::neurolate`), which also shares
`block_rect` (reading-order rectangle) and `image_result` (image 0 + per-image error) from here.
An EMPTY region returns its own rectangle as the mask and a FAILED one nothing usable, so both
add neither mask nor block. An answer that should carry masks but does not (`modeApplied`
TEXT, or a usable block without polygons) is an error, never an empty result that would wipe
the stored mask. Polygons rasterize through `ms_text_detect::rasterize_polygons` (holes per polygon);
blocks pass `ms_text_detect::blocks::finalize_blocks` like every engine's, so NeuroLate's own
reading order is replaced by the common one. Masks are not dilated here for pages (the batch
driver dilates every page); a region mask is dilated at the region size like every region
engine's. A paid request is never retried; `cancel` (download Cancel, controller drop)
abandons the wait at once (the request may still be charged), the user's Stop never reaches it.
Worker-thread only: decode, PNG encode, the credential-store read and the HTTP call block.
*/

use super::pipeline::{self, DetectFailure};
use super::region_detect::{RegionDetectEngine, RegionDetection, offset_blocks};
use super::{NeurolateDetect, PageBlockTexts, TextDetectorPageResult, TextDetectorRect, dilate_mask_alpha};
use crate::page_crop::PixelRect;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder, RgbImage};
use ms_ai_api::neurolate::{self, NeurolateError, OcrBlock, OcrImageResult, OcrKind, OcrMode, OcrParams, OcrPolygon, OcrRequest, OcrResponse, TextStatus};
use ms_text_detect::mask::MaskError;
use ms_text_detect::{MaskPolygon, rasterize_polygons};
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// The largest coordinate magnitude every `f32` represents exactly (2^24).
const MAX_EXACT_F32_COORD: i32 = 1 << 24;

/// The NeuroLate API key, read on the worker once per batch or region. `Debug` never shows it.
pub(super) struct NeurolateKey(String);

impl fmt::Debug for NeurolateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NeurolateKey(<redacted>)")
    }
}

impl NeurolateKey {
    /// Reads the stored key (blocking credential-store I/O; worker thread only).
    ///
    /// # Errors
    /// The localized "no key" / credential-store message (`NeurolateError`'s `Display`).
    pub(super) fn read() -> Result<Self, String> {
        neurolate::read_key().map(Self).map_err(|error| {
            ms_log::runtime_log::log_error(format!("[text-detector] NeuroLate key unavailable: {error:?}"));
            error.to_string()
        })
    }
}

/// The request mode of a NeuroLate page detection: `MASK_AND_TEXT` when the block texts are
/// wanted too (one request, the texts come with the masks), else `MASK`.
#[must_use]
pub(super) fn page_mode(detect: NeurolateDetect) -> OcrMode {
    if detect.with_text { OcrMode::MaskAndText } else { OcrMode::Mask }
}

/// Detects text on the page file `path` with NeuroLate: decode (the detector's one page decode),
/// one request in [`page_mode`], then [`page_result_from_image`] and, with `detect.with_text`,
/// [`page_texts_from_image`] (aligned with the result's blocks). The mask is NOT dilated (the
/// batch driver does it).
///
/// # Errors
/// [`DetectFailure::Cancelled`] when `cancel` was set while waiting; else `Failed` with the
/// localized open / encode / NeuroLate / answer-mismatch message.
pub(super) fn detect_page(path: &Path, key: &NeurolateKey, detect: NeurolateDetect, cancel: &AtomicBool) -> Result<(TextDetectorPageResult, Option<PageBlockTexts>), DetectFailure> {
    let page = pipeline::decode_page_rgb(path)?;
    let source_size = [page.width(), page.height()];
    let mode = page_mode(detect);
    ms_log::runtime_log::log_info(format!("[text-detector] NeuroLate page {} ({}x{}), mode {}", path.display(), source_size[0], source_size[1], mode.wire()));
    // A whole page, whatever its size: without `kind` the server would read a page with both
    // sides up to 690 px as ONE block.
    let image = recognize_image_in(key, detect.params, mode, Some(OcrKind::Page), &page, cancel)?;
    let result = page_result_from_image(source_size, &image).map_err(|error| {
        ms_log::runtime_log::log_error(format!("[text-detector] NeuroLate answer for page {} rejected: {error}", path.display()));
        DetectFailure::Failed(error)
    })?;
    let texts = detect.with_text.then(|| page_texts_from_image(&image));
    Ok((result, texts))
}

/// Sends `image` to NeuroLate as one PNG in `MASK` mode (never retried) and returns the result
/// of that image. `cancel` abandons the wait (the client returns at once; the request may still
/// be charged).
///
/// # Errors
/// [`DetectFailure::Cancelled`] when `cancel` stopped the wait; else `Failed` with the localized
/// encode message, the client's message, or [`image_result`]'s error.
pub(super) fn recognize_image(key: &NeurolateKey, params: OcrParams, image: &RgbImage, cancel: &AtomicBool) -> Result<OcrImageResult, DetectFailure> {
    // A selection crop: the server's size default decides BLOCK vs PAGE.
    recognize_image_in(key, params, OcrMode::Mask, None, image, cancel)
}

/// [`recognize_image`] in `mode` with the request's `kind` (`None`: the server's size default).
///
/// # Errors
/// As [`recognize_image`].
fn recognize_image_in(key: &NeurolateKey, params: OcrParams, mode: OcrMode, kind: Option<OcrKind>, image: &RgbImage, cancel: &AtomicBool) -> Result<OcrImageResult, DetectFailure> {
    let png = encode_png(image)?;
    let request = OcrRequest { mode, kind, params, images_png: vec![png] };
    let abandon = || cancel.load(Ordering::Relaxed);
    match neurolate::recognize(&key.0, request, &abandon) {
        Ok(response) => image_result(response, "[text-detector]").map_err(DetectFailure::Failed),
        Err(NeurolateError::Cancelled) => Err(DetectFailure::Cancelled),
        // The client already logged the failure with its context.
        Err(error) => Err(DetectFailure::Failed(error.to_string())),
    }
}

/// Encodes an RGB image as PNG (NeuroLate reads PNG or JPEG only), straight from its buffer.
fn encode_png(image: &RgbImage) -> Result<Vec<u8>, DetectFailure> {
    let mut out = Vec::new();
    // `image.as_raw()` is exactly `w * h * 3` bytes by `RgbImage`'s own invariant, so the
    // encoder's length check cannot fail.
    PngEncoder::new(&mut out).write_image(image.as_raw(), image.width(), image.height(), ExtendedColorType::Rgb8).map_err(|error| {
        ms_log::runtime_log::log_error(format!("[text-detector] PNG encode of a {}x{} image for NeuroLate failed: {error}", image.width(), image.height()));
        DetectFailure::Failed(tf!("translation.text_detector.neurolate_encode_error", err = error))
    })?;
    Ok(out)
}

/// The result of the one image a request sent (image 0) out of `response`, checked for its
/// per-image error. `log_tag` prefixes the log line of a refusal (`"[ocr]"`, `"[text-detector]"`).
/// The one owner of this step for the detector and the OCR engine.
///
/// # Errors
/// The localized "no result" message when the answer lacks image 0, or the per-image failure.
pub(crate) fn image_result(response: OcrResponse, log_tag: &str) -> Result<OcrImageResult, String> {
    let request_id = response.request_id;
    let Some(image) = response.images.into_iter().find(|result| result.index == 0) else {
        ms_log::runtime_log::log_error(format!("{log_tag} NeuroLate answer {request_id} holds no result for image 0"));
        return Err(t!("translation.text_detector.neurolate_no_result_error").to_string());
    };
    image_error(&image).inspect_err(|error| ms_log::runtime_log::log_warn(format!("{log_tag} NeuroLate answer {request_id}: {error}")))?;
    Ok(image)
}

/// The localized failure of an image the service could not process, if any.
fn image_error(image: &OcrImageResult) -> Result<(), String> {
    match &image.error {
        Some(error) => Err(tf!("translation.text_detector.neurolate_image_failed_error", code = error.code, message = error.message)),
        None => Ok(()),
    }
}

/// The one block rule: whether a block counts at all — no per-block error and a region status
/// that is absent or OK. EMPTY / FAILED regions and any status this client does not know are
/// unusable. A usable block contributes its polygons to the mask (`detection_from_image`) and its
/// text to the OCR engine's result; an unusable one contributes neither.
#[must_use]
pub(crate) fn usable_block(block: &OcrBlock) -> bool {
    let status_usable = match &block.text_status {
        None | Some(TextStatus::Ok) => true,
        Some(TextStatus::Empty | TextStatus::Failed | TextStatus::Unknown(_)) => false,
    };
    block.error.is_none() && status_usable
}

/// Checks that an answer that had to carry masks does: refused when the service applied `TEXT`
/// mode, or when a usable block came without polygons (its mask would be silently missing and
/// the merge would clear the stored mask under it).
fn require_masks(image: &OcrImageResult) -> Result<(), String> {
    let text_only = image.mode_applied == Some(OcrMode::Text) || image.blocks.iter().any(|block| usable_block(block) && block.polygons.is_empty());
    if text_only {
        ms_log::runtime_log::log_warn(format!("[text-detector] NeuroLate answer without masks (mode applied {:?}, {} block(s)); not applied", image.mode_applied, image.blocks.len()));
        return Err(t!("translation.text_detector.neurolate_no_mask_error").to_string());
    }
    Ok(())
}

/// Checks that `image` answered successfully and for an image of `expected_size` (`[w, h]`, the
/// size that was sent): the polygons are image-absolute, so another size would misplace them.
fn validate_image(image: &OcrImageResult, expected_size: [u32; 2]) -> Result<(), String> {
    image_error(image)?;
    if [image.width, image.height] != expected_size {
        return Err(tf!(
            "translation.text_detector.neurolate_size_mismatch_error",
            w = image.width,
            h = image.height,
            expected_w = expected_size[0],
            expected_h = expected_size[1]
        ));
    }
    Ok(())
}

/// An API coordinate as an `f32` vertex coordinate. Clamped to +-2^24 first, where every `i32`
/// is exactly representable, so the cast below loses nothing; a coordinate that far outside the
/// image is clamped by the rasterizer anyway.
fn coord(value: i32) -> f32 {
    // Exact: |value| <= 2^24 after the clamp (see `MAX_EXACT_F32_COORD`).
    value.clamp(-MAX_EXACT_F32_COORD, MAX_EXACT_F32_COORD) as f32
}

fn ring(points: &[[i32; 2]]) -> Vec<(f32, f32)> {
    points.iter().map(|&[x, y]| (coord(x), coord(y))).collect()
}

fn mask_polygon(polygon: &OcrPolygon) -> MaskPolygon {
    MaskPolygon { outer: ring(&polygon.outer), holes: polygon.holes.iter().map(|hole| ring(hole)).collect() }
}

/// A block's rectangle: its `bbox`, else the extents of its outer rings; `None` when neither
/// gives a non-empty box.
pub(crate) fn block_rect(block: &OcrBlock) -> Option<TextDetectorRect> {
    if let Some(bbox) = block.bbox {
        return TextDetectorRect::from_xyxy(coord(bbox.x), coord(bbox.y), coord(bbox.x.saturating_add(bbox.width)), coord(bbox.y.saturating_add(bbox.height)));
    }
    let mut points = block.polygons.iter().flat_map(|polygon| polygon.outer.iter());
    let &[x0, y0] = points.next()?;
    let (min_x, min_y, max_x, max_y) = points.fold((x0, y0, x0, y0), |(min_x, min_y, max_x, max_y), &[x, y]| (min_x.min(x), min_y.min(y), max_x.max(x), max_y.max(y)));
    TextDetectorRect::from_xyxy(coord(min_x), coord(min_y), coord(max_x), coord(max_y))
}

/// The mask and blocks of one NeuroLate image result in that image's pixels: a 0/255 mask of
/// `size` (`[w, h]`; the empty `[0, 0]` form only for a zero-area size) rasterized from the
/// polygons of the blocks with a usable mask, and those blocks' rectangles, finalized
/// (`ms_text_detect` reading order and cap). Blocks without a usable mask (EMPTY / FAILED
/// regions, per-block errors) contribute nothing; a block whose box is degenerate keeps its
/// mask but adds no rectangle.
///
/// # Errors
/// The localized "mask too large" message when `size` exceeds the detector's pixel limit.
pub(crate) fn detection_from_image(image: &OcrImageResult, size: [u32; 2]) -> Result<(Vec<u8>, Vec<TextDetectorRect>), String> {
    let polygons: Vec<MaskPolygon> = mask_blocks(image).flat_map(|block| block.polygons.iter().map(mask_polygon)).collect();
    let mask = rasterize_polygons(size, &polygons).map_err(|error| match error {
        MaskError::TooLarge { width, height } => tf!("translation.text_detector.result_mask_too_large_error", w = width, h = height),
    })?;
    let blocks = detected_blocks(image).into_iter().map(|(rect, _block)| rect).collect();
    Ok((mask.alpha, blocks))
}

/// The blocks of an answer that make the detector mask: usable ([`usable_block`]) and with
/// polygons. A usable block without polygons has nothing to rasterize (TEXT-mode answers) and
/// adds no rectangle either, so mask and blocks stay in step.
fn mask_blocks(image: &OcrImageResult) -> impl Iterator<Item = &OcrBlock> {
    image.blocks.iter().filter(|block| usable_block(block) && !block.polygons.is_empty())
}

/// The detector blocks of an answer with the answer block each came from, finalized
/// ([`finalize_paired`]): exactly the rectangles `detection_from_image` returns, in its order.
fn detected_blocks(image: &OcrImageResult) -> Vec<(TextDetectorRect, &OcrBlock)> {
    finalize_paired(mask_blocks(image).filter_map(|block| block_rect(block).map(|rect| (rect, block))).collect())
}

/// The raw recognized text of each detector block of an answer, aligned with the blocks
/// `detection_from_image` / `page_result_from_image` return for it ([`block_text`]; `None` for a
/// block without text). Used for a `MASK_AND_TEXT` page detection.
#[must_use]
pub(crate) fn page_texts_from_image(image: &OcrImageResult) -> PageBlockTexts {
    detected_blocks(image).into_iter().map(|(_rect, block)| block_text(block).map(str::to_string)).collect()
}

/// The recognized text of a block: trimmed and non-empty, from a usable block only (the rule
/// the mask follows too). Shared by the page texts here and the NeuroLate OCR engine.
#[must_use]
pub(crate) fn block_text(block: &OcrBlock) -> Option<&str> {
    if !usable_block(block) {
        return None;
    }
    block.text.as_deref().map(str::trim).filter(|text| !text.is_empty())
}

/// `pairs` in the one block order: the rectangles finalized by
/// `ms_text_detect::blocks::finalize_blocks` (reading-order sort, then the block cap), each
/// carrying its payload. Equal rectangles keep their input order (the sort is stable and the
/// first remaining equal pair is taken), so the result is exactly `finalize_blocks` of the
/// rectangles with the payloads attached. Quadratic, but answers hold few blocks.
pub(crate) fn finalize_paired<T>(pairs: Vec<(TextDetectorRect, T)>) -> Vec<(TextDetectorRect, T)> {
    let mut order: Vec<TextDetectorRect> = pairs.iter().map(|(rect, _)| *rect).collect();
    ms_text_detect::blocks::finalize_blocks(&mut order);
    let mut remaining: Vec<Option<(TextDetectorRect, T)>> = pairs.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|rect| {
            let pos = remaining.iter().position(|entry| entry.as_ref().is_some_and(|(candidate, _)| *candidate == rect))?;
            remaining.get_mut(pos).and_then(Option::take)
        })
        .collect()
}

/// The detector page result of a whole-page answer for a page of `source_size`: validated
/// (no per-image error, the answered size equals the page size), then
/// [`detection_from_image`] at the page size. Not dilated.
///
/// # Errors
/// The localized per-image failure, size-mismatch, missing-masks (`require_masks`) or mask-limit
/// message.
pub(super) fn page_result_from_image(source_size: [u32; 2], image: &OcrImageResult) -> Result<TextDetectorPageResult, String> {
    validate_image(image, source_size)?;
    require_masks(image)?;
    let (mask_alpha, blocks) = detection_from_image(image, source_size)?;
    let mask_size = if mask_alpha.is_empty() { [0, 0] } else { source_size };
    Ok(TextDetectorPageResult { source_size, blocks, mask_size, mask_alpha })
}

/// The region detection of an answer for the crop `rect` (page pixels, the `page_crop` rule)
/// of page `page_idx` (`source_size`): validated against the crop size, the mask built at the
/// crop size and dilated by `mask_dilate_size` (clamped 0..=30, stopping at the crop border),
/// the blocks moved to page pixels. Shared by the detector's region command and the NeuroLate
/// OCR engine, whose answer for the same crop also carries the mask.
///
/// # Errors
/// The localized per-image failure, size-mismatch, missing-masks (`require_masks`) or mask-limit
/// message.
pub(crate) fn region_from_image(page_idx: usize, engine: RegionDetectEngine, source_size: [u32; 2], rect: PixelRect, image: &OcrImageResult, mask_dilate_size: i32) -> Result<RegionDetection, String> {
    let crop_size = [rect.width, rect.height];
    validate_image(image, crop_size)?;
    require_masks(image)?;
    let (mut mask_alpha, blocks) = detection_from_image(image, crop_size)?;
    dilate_mask_alpha(&mut mask_alpha, crop_size, mask_dilate_size);
    let blocks = offset_blocks(&blocks, rect.x, rect.y);
    Ok(RegionDetection { page_idx, engine, source_size, rect: rect.xyxy(), mask_alpha, blocks })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ms_ai_api::neurolate::protocol::parse_ocr_response;
    use ms_ai_api::neurolate::{JobError, NeurolateComicType, NeurolateLanguage, OcrBox};

    const OCR_PAGE: &str = include_str!("../../../ms-ai-api/fixtures/neurolate/ocr_page.json");
    const OCR_REGIONS_EMPTY: &str = include_str!("../../../ms-ai-api/fixtures/neurolate/ocr_regions_empty.json");

    const PARAMS: OcrParams = OcrParams { language: NeurolateLanguage::Ko, comic_type: NeurolateComicType::Webtoon };

    fn first_image(fixture: &str) -> OcrImageResult {
        let response = parse_ocr_response(200, fixture.as_bytes(), 1).unwrap_or_else(|error| panic!("fixture parses: {error:?}"));
        response.images.into_iter().next().unwrap_or_else(|| panic!("fixture has an image"))
    }

    fn mask_px(mask: &[u8], width: u32, x: u32, y: u32) -> u8 {
        let idx = usize::try_from(u64::from(y) * u64::from(width) + u64::from(x)).unwrap_or(usize::MAX);
        mask.get(idx).copied().unwrap_or_else(|| panic!("pixel ({x}, {y}) in the mask"))
    }

    /// Every block of the live page answer gives a mask with set pixels inside its bbox and a
    /// rectangle; the mask is strictly 0/255 at the page size; blocks are finalized.
    #[test]
    fn page_answer_becomes_mask_and_finalized_blocks() {
        let image = first_image(OCR_PAGE);
        let result = page_result_from_image([690, 1500], &image).unwrap_or_else(|error| panic!("valid page answer: {error}"));
        assert_eq!((result.source_size, result.mask_size), ([690, 1500], [690, 1500]));
        assert_eq!(result.mask_alpha.len(), 690 * 1500);
        assert!(result.mask_alpha.iter().all(|&px| px == 0 || px == 255));
        assert_eq!(result.blocks.len(), 3);
        let mut finalized = result.blocks.clone();
        ms_text_detect::blocks::finalize_blocks(&mut finalized);
        assert_eq!(finalized, result.blocks, "blocks are in the common reading order");
        for block in &image.blocks {
            let bbox = block.bbox.unwrap_or_else(|| panic!("fixture block has a bbox"));
            let rect = block_rect(block).unwrap_or_else(|| panic!("bbox is a valid rect"));
            assert!(result.blocks.contains(&rect), "{rect:?}");
            let set_inside = (bbox.y..bbox.y + bbox.height)
                .flat_map(|y| (bbox.x..bbox.x + bbox.width).map(move |x| (x, y)))
                .filter(|&(x, y)| mask_px(&result.mask_alpha, 690, u32::try_from(x).unwrap_or(0), u32::try_from(y).unwrap_or(0)) == 255)
                .count();
            assert!(set_inside > 0, "block {} has mask pixels inside its bbox", block.number);
        }
        // Far from every block the mask is clear.
        assert_eq!(mask_px(&result.mask_alpha, 690, 5, 5), 0);
    }

    /// An EMPTY region returns its own rectangle as the mask: it adds neither mask nor block.
    #[test]
    fn empty_region_contributes_nothing() {
        let image = first_image(OCR_REGIONS_EMPTY);
        let (mask, blocks) = detection_from_image(&image, [690, 1500]).unwrap_or_else(|error| panic!("valid answer: {error}"));
        assert_eq!(blocks.len(), 2);
        // The EMPTY block's region is 450,1000 200x200; no other block reaches it.
        for y in 1000..1200 {
            for x in 450..650 {
                assert_eq!(mask_px(&mask, 690, x, y), 0, "pixel ({x}, {y}) of the EMPTY region");
            }
        }
        assert!(mask.contains(&255));
    }

    /// A per-image error or an answer for another size is a page error, never a result.
    #[test]
    fn image_error_and_size_mismatch_are_refused() {
        let mut image = first_image(OCR_PAGE);
        assert!(page_result_from_image([690, 1501], &image).is_err(), "size mismatch");
        image.error = Some(JobError { code: "INVALID_IMAGE".to_string(), message: "bad".to_string() });
        assert!(page_result_from_image([690, 1500], &image).is_err(), "per-image error");
    }

    fn square_block(x: i32, y: i32, side: i32, bbox: Option<OcrBox>) -> OcrBlock {
        OcrBlock {
            number: 1,
            region: None,
            text: None,
            polygons: vec![OcrPolygon { outer: vec![[x, y], [x + side, y], [x + side, y + side], [x, y + side]], holes: Vec::new() }],
            bbox,
            text_status: None,
            error: None,
        }
    }

    fn image_of(width: u32, height: u32, blocks: Vec<OcrBlock>) -> OcrImageResult {
        OcrImageResult { index: 0, width, height, mode_applied: Some(OcrMode::Mask), blocks, error: None }
    }

    /// A crop answer: the mask has the crop size and is dilated there; the blocks move to page
    /// pixels; the rect is the crop's.
    #[test]
    fn region_answer_is_offset_to_page_pixels() {
        let image = image_of(10, 8, vec![square_block(2, 2, 3, Some(OcrBox { x: 2, y: 2, width: 3, height: 3 }))]);
        let rect = PixelRect { x: 100, y: 50, width: 10, height: 8 };
        let engine = RegionDetectEngine::NeuroLate(PARAMS);
        let region = region_from_image(4, engine, [400, 300], rect, &image, 0).unwrap_or_else(|error| panic!("valid region: {error}"));
        assert_eq!((region.page_idx, region.engine, region.source_size, region.rect), (4, engine, [400, 300], [100, 50, 110, 58]));
        assert_eq!(region.mask_alpha.len(), 80);
        let undilated = region.mask_alpha.iter().filter(|&&px| px == 255).count();
        assert!(undilated > 0);
        let expected = TextDetectorRect::from_xyxy(102.0, 52.0, 105.0, 55.0).unwrap_or_else(|| panic!("valid rect"));
        assert_eq!(region.blocks, vec![expected]);
        let dilated = region_from_image(4, engine, [400, 300], rect, &image, 1).unwrap_or_else(|error| panic!("valid region: {error}"));
        assert!(dilated.mask_alpha.iter().filter(|&&px| px == 255).count() > undilated, "dilated at the crop size");
        assert!(region_from_image(4, engine, [400, 300], PixelRect { width: 11, ..rect }, &image, 0).is_err(), "answer for another size");
    }

    /// Without a bbox the block's rectangle is its polygon extents; a block with an error or no
    /// polygon is skipped.
    #[test]
    fn block_rect_falls_back_to_polygon_extents() {
        let block = square_block(1, 2, 4, None);
        assert_eq!(block_rect(&block), TextDetectorRect::from_xyxy(1.0, 2.0, 5.0, 6.0));
        let failed = OcrBlock { error: Some(JobError { code: "X".to_string(), message: String::new() }), ..square_block(0, 0, 4, None) };
        let (mask, blocks) = detection_from_image(&image_of(8, 8, vec![failed, square_block(0, 0, 0, None)]), [8, 8]).unwrap_or_else(|error| panic!("{error}"));
        assert!(blocks.is_empty());
        assert!(mask.iter().all(|&px| px == 0));
    }

    /// The one block rule agrees with the client's `has_usable_mask` wherever a block has
    /// polygons, an unknown status included (unusable for mask and text alike).
    #[test]
    fn usable_block_is_the_mask_rule() {
        let statuses = [None, Some(TextStatus::Ok), Some(TextStatus::Empty), Some(TextStatus::Failed), Some(TextStatus::Unknown("PARTIAL".to_string()))];
        for status in statuses {
            for error in [None, Some(JobError { code: "X".to_string(), message: String::new() })] {
                let block = OcrBlock { text_status: status.clone(), error: error.clone(), ..square_block(0, 0, 4, None) };
                assert_eq!(usable_block(&block), block.has_usable_mask(), "{status:?} {error:?}");
            }
        }
        assert!(!usable_block(&OcrBlock { text_status: Some(TextStatus::Unknown("NEW".to_string())), ..square_block(0, 0, 4, None) }));
    }

    /// A MASK answer applied as TEXT, or a usable block without polygons, is refused for pages
    /// and regions alike (never an empty result over the stored mask).
    #[test]
    fn answers_without_masks_are_refused() {
        let mut image = first_image(OCR_PAGE);
        image.mode_applied = Some(OcrMode::Text);
        assert!(page_result_from_image([690, 1500], &image).is_err());
        let rect = PixelRect { x: 0, y: 0, width: 690, height: 1500 };
        assert!(region_from_image(0, RegionDetectEngine::NeuroLate(PARAMS), [690, 1500], rect, &image, 0).is_err());
        let mut image = first_image(OCR_PAGE);
        image.blocks[1].polygons.clear();
        assert!(page_result_from_image([690, 1500], &image).is_err());
        // An EMPTY region without polygons is not usable, so it does not trip the check.
        let mut regions = first_image(OCR_REGIONS_EMPTY);
        regions.blocks[2].polygons.clear();
        assert!(page_result_from_image([690, 1500], &regions).is_ok());
    }

    /// "Recognize text right away" switches the page request to `MASK_AND_TEXT`.
    #[test]
    fn page_mode_follows_the_with_text_choice() {
        assert_eq!(page_mode(NeurolateDetect { params: PARAMS, with_text: false }), OcrMode::Mask);
        assert_eq!(page_mode(NeurolateDetect { params: PARAMS, with_text: true }), OcrMode::MaskAndText);
    }

    /// The page texts are aligned with the finalized blocks of the page result, whatever order
    /// the server sent; an EMPTY region adds neither a block nor a text.
    #[test]
    fn page_texts_align_with_the_finalized_blocks() {
        let mut image = first_image(OCR_PAGE);
        image.blocks.reverse();
        let result = page_result_from_image([690, 1500], &image).unwrap_or_else(|error| panic!("{error}"));
        let texts = page_texts_from_image(&image);
        assert_eq!(texts.len(), result.blocks.len());
        for (rect, text) in result.blocks.iter().zip(&texts) {
            let source = image.blocks.iter().find(|block| block_rect(block) == Some(*rect)).unwrap_or_else(|| panic!("block for {rect:?}"));
            assert_eq!(text.as_deref(), source.text.as_deref().map(str::trim), "{rect:?}");
        }
        assert_eq!(texts.first().cloned().flatten().as_deref(), Some("안녕하세요! 오늘 날씨가 정말 좋네요."), "reading order, not server order");

        let regions = first_image(OCR_REGIONS_EMPTY);
        let texts = page_texts_from_image(&regions);
        assert_eq!(texts.len(), 2);
        assert!(texts.iter().all(Option::is_some));
    }

    /// `finalize_paired` is `finalize_blocks` with payloads attached; equal rectangles keep their
    /// input order.
    #[test]
    fn finalize_paired_keeps_payloads_with_their_rectangles() {
        let a = TextDetectorRect::from_xyxy(0.0, 10.0, 5.0, 15.0).unwrap_or_else(|| panic!("rect"));
        let b = TextDetectorRect::from_xyxy(0.0, 0.0, 5.0, 5.0).unwrap_or_else(|| panic!("rect"));
        let paired = finalize_paired(vec![(a, "a"), (b, "b"), (a, "a2")]);
        assert_eq!(paired, vec![(b, "b"), (a, "a"), (a, "a2")]);
    }

    #[test]
    fn coordinates_convert_exactly_and_clamp_far_outliers() {
        assert_eq!(coord(1234), 1234.0);
        assert_eq!(coord(-7), -7.0);
        assert_eq!(coord(i32::MAX), 16_777_216.0);
        assert_eq!(coord(i32::MIN), -16_777_216.0);
    }

    #[test]
    fn the_key_never_shows_in_debug() {
        assert_eq!(format!("{:?}", NeurolateKey("nlk_secret".to_string())), "NeurolateKey(<redacted>)");
    }
}
