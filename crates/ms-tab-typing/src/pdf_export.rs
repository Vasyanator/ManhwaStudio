/*
FILE HEADER (crates/ms-tab-typing/src/pdf_export.rs)

Purpose:
Builds the multi-page PDF emitted by the typing tab's `Pdf` export format. A PDF produced
here is deliberately trivial in structure: N pages, each page carrying exactly ONE full-page
raster image and nothing else — no text objects, no embedded fonts, no vector content. The
text was already typeset and rasterized by the export pipeline; this module only wraps the
finished pixels in the container format.

Layer:
A GUI-free, I/O-free leaf of the typing tab. It returns the finished document as bytes and
never touches the filesystem — writing them out through `ms_storage` belongs to the caller,
which also owns the background thread the whole export runs on (`CLAUDE.md` §5).

Main responsibilities:
- accumulate pages incrementally, so peak memory is the size of the FINISHED PDF rather than
  the sum of the decoded pages: the source RGBA of a page is borrowed, consumed and released
  before `push_page` returns;
- composite straight (un-premultiplied) RGBA8 over an opaque white backdrop and embed it
  losslessly as a `/DeviceRGB`, 8-bit, `/FlateDecode` image XObject;
- map pixel dimensions onto a legal PDF page box, including the format's 14400 pt side limit
  that every stitched webtoon page exceeds.

Key structures:
- TypingPdfBuilder — the incremental document builder.
- PdfExportError — the typed failure set (`CLAUDE.md` §7: no panics on bad input).
- PageBoxPt — a validated page box in PDF points.

Key functions:
- TypingPdfBuilder::push_page(), TypingPdfBuilder::finish()
- page_box_pt() — pixel dimensions to page box, including the 14400 pt clamp.
- append_rgb_over_white() / composite_component_over_white() — RGBA to RGB over white.

Notes:
Object numbering is owned entirely by the builder (catalog = 1, page tree = 2, then three
consecutive ids per page). `pdf_writer::Pdf::finish` PANICS when one indirect id is written
twice, so no id may ever come from outside this module.
*/

use std::io::Write;

use flate2::Compression;
use flate2::write::ZlibEncoder;
use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect, Ref};

/// Hard upper bound on either side of a PDF page box, in points (1 pt = 1/72 in).
///
/// The PDF specification caps a page at 200 in = 14400 pt per side; viewers reject or clip
/// anything larger. See [`page_box_pt`] for how oversized rasters are fitted into it.
const PDF_MAX_PAGE_SIDE_PT: f64 = 14_400.0;

/// Indirect object id of the document catalog.
const CATALOG_ID: i32 = 1;

/// Indirect object id of the page tree node all pages hang off.
const PAGE_TREE_ID: i32 = 2;

/// First indirect object id available to page objects.
const FIRST_PAGE_OBJECT_ID: i32 = 3;

/// Indirect objects each page contributes: the page dict, its image XObject, its content stream.
const OBJECTS_PER_PAGE: i32 = 3;

/// Largest page count the builder's object numbering can address.
const MAX_PAGES: i32 = (i32::MAX - FIRST_PAGE_OBJECT_ID + 1) / OBJECTS_PER_PAGE;

/// Resource name under which each page's content stream addresses its single image.
///
/// The name is page-local (it lives in that page's own `/Resources`), so the same spelling on
/// every page addresses a different XObject and no uniquing is needed.
const IMAGE_RESOURCE_NAME: Name<'static> = Name(b"Im0");

/// Everything that can stop a typing-tab PDF export, with the page geometry that caused it.
///
/// Every variant is a rejected INPUT: the module validates instead of panicking, because the
/// page sizes and buffers come from the export pipeline at runtime (`CLAUDE.md` §7, §11).
#[derive(Debug, thiserror::Error)]
pub(crate) enum PdfExportError {
    /// A page was pushed with a zero width or height; a PDF page box cannot be empty.
    #[error("cannot export a {width}x{height} px page to PDF: both sides must be non-zero")]
    ZeroSizedPage { width: u32, height: u32 },
    /// The RGBA buffer length did not match `width * height * 4`.
    #[error("cannot export a {width}x{height} px page to PDF: expected {expected} RGBA bytes, got {actual}")]
    BufferLength { width: u32, height: u32, expected: usize, actual: usize },
    /// The page cannot be addressed on this target: `width * height * 4` overflows `usize`, or a
    /// side exceeds `i32::MAX`. Reachable on the 32-bit wasm build long before it is on native.
    #[error("cannot export a {width}x{height} px page to PDF: its size does not fit this target's address space")]
    PageTooLarge { width: u32, height: u32 },
    /// More pages were pushed than the PDF indirect-reference numbering can address.
    #[error("cannot export more than {max} pages into a single PDF")]
    TooManyPages { max: i32 },
    /// The zlib encoder behind the `/FlateDecode` image stream failed.
    #[error("cannot compress the image stream of a {width}x{height} px page: {source}")]
    Compression { width: u32, height: u32, #[source] source: std::io::Error },
    /// [`TypingPdfBuilder::finish`] was called before any page was pushed.
    #[error("cannot write a PDF with no pages")]
    NoPages,
}

/// A validated PDF page box, in points (1 pt = 1/72 in).
///
/// Both sides are strictly positive and never exceed [`PDF_MAX_PAGE_SIDE_PT`]. Produced only
/// by [`page_box_pt`]; the type exists so that the geometry can be asserted on its own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PageBoxPt {
    /// Page width in points.
    pub(crate) width: f64,
    /// Page height in points.
    pub(crate) height: f64,
}

/// Incremental multi-page PDF builder: one full-page raster per page.
///
/// Pages are serialized into the document as they arrive, so the builder retains only the
/// growing PDF buffer and one `Ref` per page — never the source rasters. Create it with
/// [`TypingPdfBuilder::new`], append pages with [`TypingPdfBuilder::push_page`], and take the
/// finished bytes with [`TypingPdfBuilder::finish`].
#[derive(Debug)]
pub(crate) struct TypingPdfBuilder {
    /// The document under construction. Page, image and content objects are written into it
    /// immediately; the catalog and the page tree are written last, by `finish`.
    pdf: Pdf,
    /// Page object ids in document order, needed for the page tree's `/Kids`.
    page_ids: Vec<Ref>,
    /// Next free indirect object id. Advances by [`OBJECTS_PER_PAGE`] per page.
    next_object_id: i32,
}

impl TypingPdfBuilder {
    /// Creates an empty builder holding a PDF 1.7 header and nothing else.
    #[must_use]
    pub(crate) fn new() -> Self { Self { pdf: Pdf::new(), page_ids: Vec::new(), next_object_id: FIRST_PAGE_OBJECT_ID } }

    /// Number of pages appended so far.
    #[must_use]
    pub(crate) fn page_count(&self) -> usize { self.page_ids.len() }

    /// Appends one page whose single content object is `rgba` drawn to fill it.
    ///
    /// `rgba` is straight (un-premultiplied) RGBA8, row-major, exactly
    /// `width_px * height_px * 4` bytes long. It is only borrowed: by the time this returns,
    /// the page has been composited, compressed and written into the document, and nothing
    /// derived from `rgba` is retained. Page sizes may differ from page to page.
    ///
    /// The page box follows [`page_box_pt`] (1 px = 1 pt, clamped to the format's 14400 pt
    /// limit), and the image is embedded losslessly — see [`compress_rgb_over_white`].
    ///
    /// # Errors
    /// [`PdfExportError::ZeroSizedPage`] when either side is zero;
    /// [`PdfExportError::BufferLength`] when `rgba.len()` does not match the dimensions;
    /// [`PdfExportError::PageTooLarge`] when the page cannot be addressed on this target;
    /// [`PdfExportError::TooManyPages`] when the object numbering is exhausted;
    /// [`PdfExportError::Compression`] when the zlib encoder fails.
    pub(crate) fn push_page(&mut self, rgba: &[u8], width_px: u32, height_px: u32) -> Result<(), PdfExportError> {
        if width_px == 0 || height_px == 0 { return Err(PdfExportError::ZeroSizedPage { width: width_px, height: height_px }); }

        let too_large = || PdfExportError::PageTooLarge { width: width_px, height: height_px };
        // Checked throughout: on wasm32 `usize` is 32 bits, so a page well below `u32::MAX` per
        // side can still overflow the byte count (`CLAUDE.md` §11).
        let width = usize::try_from(width_px).map_err(|_| too_large())?;
        let height = usize::try_from(height_px).map_err(|_| too_large())?;
        let row_rgba_len = width.checked_mul(4).ok_or_else(too_large)?;
        let expected_len = row_rgba_len.checked_mul(height).ok_or_else(too_large)?;
        if rgba.len() != expected_len { return Err(PdfExportError::BufferLength { width: width_px, height: height_px, expected: expected_len, actual: rgba.len() }); }
        // `pdf-writer` spells image dimensions as i32, which on a 64-bit host is the tighter bound.
        let width_i32 = i32::try_from(width_px).map_err(|_| too_large())?;
        let height_i32 = i32::try_from(height_px).map_err(|_| too_large())?;

        // Guarding the page COUNT here is what makes the id arithmetic below total: with fewer
        // than `MAX_PAGES` pages written, `next_object_id + OBJECTS_PER_PAGE` stays within i32.
        let page_count = i32::try_from(self.page_ids.len()).map_err(|_| PdfExportError::TooManyPages { max: MAX_PAGES })?;
        if page_count >= MAX_PAGES { return Err(PdfExportError::TooManyPages { max: MAX_PAGES }); }

        let stream = compress_rgb_over_white(rgba, row_rgba_len).map_err(|source| PdfExportError::Compression { width: width_px, height: height_px, source })?;

        let page_id = Ref::new(self.next_object_id);
        let image_id = Ref::new(self.next_object_id + 1);
        let content_id = Ref::new(self.next_object_id + 2);
        let page_box = page_box_pt(width_px, height_px);
        let box_width = pt_to_f32(page_box.width);
        let box_height = pt_to_f32(page_box.height);

        let mut page = self.pdf.page(page_id);
        page.media_box(Rect::new(0.0, 0.0, box_width, box_height));
        page.parent(Ref::new(PAGE_TREE_ID));
        page.contents(content_id);
        page.resources().x_objects().pair(IMAGE_RESOURCE_NAME, image_id);
        page.finish();

        let mut image = self.pdf.image_xobject(image_id, &stream);
        image.filter(Filter::FlateDecode);
        image.width(width_i32);
        image.height(height_i32);
        image.color_space().device_rgb();
        image.bits_per_component(8);
        image.finish();

        // A PDF image XObject is painted into the UNIT SQUARE of user space, with sample (0,0)
        // at its upper-left corner. Scaling that square to the page box therefore places the
        // first pixel row at the TOP of the page with no vertical flip: the matrix is
        // `[w 0 0 h 0 0]`, not `[w 0 0 -h 0 h]`.
        let mut content = Content::new();
        content.save_state();
        content.transform([box_width, 0.0, 0.0, box_height, 0.0, 0.0]);
        content.x_object(IMAGE_RESOURCE_NAME);
        content.restore_state();
        self.pdf.stream(content_id, &content.finish());

        self.page_ids.push(page_id);
        self.next_object_id += OBJECTS_PER_PAGE;
        Ok(())
    }

    /// Serializes the document, writing the catalog and the page tree over the pages already
    /// emitted, and returns the complete PDF bytes.
    ///
    /// # Errors
    /// Returns [`PdfExportError::NoPages`] when no page was pushed: a PDF with an empty page
    /// tree is invalid, and silently emitting one would hand the user a broken file.
    pub(crate) fn finish(self) -> Result<Vec<u8>, PdfExportError> {
        let Self { mut pdf, page_ids, next_object_id: _ } = self;
        if page_ids.is_empty() { return Err(PdfExportError::NoPages); }
        // Bounded by the `MAX_PAGES` guard in `push_page`, so this conversion cannot fail.
        let count = i32::try_from(page_ids.len()).map_err(|_| PdfExportError::TooManyPages { max: MAX_PAGES })?;

        // Written LAST on purpose: `Pdf::catalog` only records the root id for the trailer, and
        // object order inside the file body is irrelevant to a PDF reader (the xref table maps
        // ids to offsets). Deferring these two is what lets pages stream out one at a time.
        pdf.catalog(Ref::new(CATALOG_ID)).pages(Ref::new(PAGE_TREE_ID));
        pdf.pages(Ref::new(PAGE_TREE_ID)).kids(page_ids.iter().copied()).count(count);
        Ok(pdf.finish())
    }
}

/// Maps a page's pixel dimensions onto a legal PDF page box.
///
/// The natural mapping is 1 px = 1 pt (72 dpi) and is kept whenever it fits. A stitched
/// webtoon page is many times taller than the format's [`PDF_MAX_PAGE_SIDE_PT`] limit, so when
/// either side would exceed it BOTH sides are scaled by a single factor that lands the longer
/// side exactly on the limit. No pixels are lost and the aspect ratio is preserved — the very
/// same image is simply placed on a physically smaller page, which a viewer zooms as usual.
///
/// Truncating the box, or dropping such a page, would silently lose content, which
/// `CLAUDE.md` §14 forbids.
#[must_use]
fn page_box_pt(width_px: u32, height_px: u32) -> PageBoxPt {
    // `f64::from` is lossless for every u32, and the products below stay under 2^53, so the
    // whole computation is exact for the scales that matter.
    let width = f64::from(width_px);
    let height = f64::from(height_px);
    let longest = width.max(height);
    if longest <= PDF_MAX_PAGE_SIDE_PT { return PageBoxPt { width, height }; }
    // Multiplying before dividing keeps an exact result for the common case where the limit
    // divides evenly (e.g. 800x40000 px -> 288 x 14400 pt).
    PageBoxPt { width: width * PDF_MAX_PAGE_SIDE_PT / longest, height: height * PDF_MAX_PAGE_SIDE_PT / longest }
}

/// Narrows a page-box measurement in points to the `f32` the PDF writer's API takes.
///
/// This is the only numeric cast in the module and it is forced by the format's own API:
/// `pdf_writer::Rect::new` and `Content::transform` are `f32`-only. The argument always comes
/// from [`page_box_pt`] and is therefore in `[0, 14400]`, where one `f32` ulp is about
/// 0.001 pt (roughly 0.4 µm on paper) — orders of magnitude below what any renderer or printer
/// resolves, so no visible precision is lost.
#[must_use]
fn pt_to_f32(pt: f64) -> f32 { pt as f32 }

/// Composites one straight-RGBA8 span over opaque white and appends it to `dst` as RGB8.
///
/// `rgba` must be a whole number of 4-byte pixels; a trailing partial pixel is ignored, which
/// cannot happen for callers inside this module because `push_page` validates the buffer
/// length first.
fn append_rgb_over_white(rgba: &[u8], dst: &mut Vec<u8>) {
    for pixel in rgba.chunks_exact(4) {
        // Indexing is safe: `chunks_exact(4)` yields slices of exactly four bytes.
        let alpha = pixel[3];
        dst.push(composite_component_over_white(pixel[0], alpha));
        dst.push(composite_component_over_white(pixel[1], alpha));
        dst.push(composite_component_over_white(pixel[2], alpha));
    }
}

/// Composites one 8-bit colour component over an opaque WHITE backdrop.
///
/// Returns `round(component * alpha / 255 + 255 * (255 - alpha) / 255)`, evaluated in integer
/// arithmetic.
///
/// White, not black and not "drop the alpha": a PDF page has no transparency backdrop of its
/// own — anything the content stream does not paint is simply the paper, which every viewer
/// and every printer shows as white. Compositing over black (or discarding alpha outright)
/// would ring every antialiased glyph edge and every soft glow with a dark fringe.
#[must_use]
fn composite_component_over_white(component: u8, alpha: u8) -> u8 {
    let alpha = u32::from(alpha);
    // Largest possible numerator is 255 * 255 = 65025, so u32 cannot overflow; `+ 127` makes
    // the division by 255 round to nearest instead of truncating.
    let weighted = u32::from(component) * alpha + 255 * (255 - alpha);
    let rounded = (weighted + 127) / 255;
    // `rounded` is provably <= 255; the fallback exists only to keep this path panic-free.
    u8::try_from(rounded).unwrap_or(u8::MAX)
}

/// Composites `rgba` over white and returns the zlib-compressed RGB8 samples for a
/// `/FlateDecode` image stream.
///
/// `row_rgba_len` is the source stride in bytes (`width * 4`) and must divide `rgba.len()`.
///
/// LOSSLESS ON PURPOSE — do not "optimize" this into a JPEG (`/DCTDecode`). These pages are the
/// output of a typesetting program: hard strokes, outlines and glows over flat fills are
/// exactly the content DCT ringing is worst at, and the halo it leaves around every letter is a
/// visible defect the user never asked for. `/FlateDecode` costs file size and nothing else.
///
/// The conversion streams ROW BY ROW through one reused buffer, so the extra allocation is a
/// single row rather than a full RGB copy of the page (96 MB for an 800x40000 px page).
///
/// # Errors
/// Propagates the zlib encoder's `io::Error`. The sink is an in-memory `Vec`, so in practice
/// this only fires on allocation failure.
fn compress_rgb_over_white(rgba: &[u8], row_rgba_len: usize) -> std::io::Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    let mut row_rgb: Vec<u8> = Vec::with_capacity(row_rgba_len / 4 * 3);
    for row in rgba.chunks_exact(row_rgba_len) {
        row_rgb.clear();
        append_rgb_over_white(row, &mut row_rgb);
        encoder.write_all(&row_rgb)?;
    }
    encoder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an opaque RGBA page filled with one colour.
    fn solid_rgba(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
        let pixels = usize::try_from(width).unwrap_or(0) * usize::try_from(height).unwrap_or(0);
        let mut out = Vec::with_capacity(pixels * 4);
        for _ in 0..pixels { out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]); }
        out
    }

    /// Counts non-overlapping occurrences of `needle` in `haystack`.
    fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
        if needle.is_empty() || haystack.len() < needle.len() { return 0; }
        haystack.windows(needle.len()).filter(|window| *window == needle).count()
    }

    #[test]
    fn single_page_document_is_a_well_formed_pdf() {
        let mut builder = TypingPdfBuilder::new();
        assert!(builder.push_page(&solid_rgba(4, 3, [10, 20, 30]), 4, 3).is_ok());
        assert_eq!(builder.page_count(), 1);
        let bytes = builder.finish().unwrap_or_default();
        assert!(bytes.starts_with(b"%PDF-"), "document must open with the PDF header");
        assert!(bytes.ends_with(b"%%EOF"), "document must close with the EOF marker");
        assert_eq!(count_occurrences(&bytes, b"/Image"), 1, "one image XObject per page");
    }

    #[test]
    fn two_page_document_carries_both_pages() {
        let mut builder = TypingPdfBuilder::new();
        assert!(builder.push_page(&solid_rgba(4, 3, [10, 20, 30]), 4, 3).is_ok());
        // Deliberately a DIFFERENT size: page boxes vary from page to page.
        assert!(builder.push_page(&solid_rgba(7, 2, [200, 100, 50]), 7, 2).is_ok());
        assert_eq!(builder.page_count(), 2);
        let bytes = builder.finish().unwrap_or_default();
        assert!(bytes.starts_with(b"%PDF-"));
        assert!(bytes.ends_with(b"%%EOF"));
        assert_eq!(count_occurrences(&bytes, b"/Image"), 2, "one image XObject per page");
    }

    #[test]
    fn empty_builder_refuses_to_finish() {
        let builder = TypingPdfBuilder::new();
        assert!(matches!(builder.finish(), Err(PdfExportError::NoPages)));
    }

    #[test]
    fn page_box_is_one_point_per_pixel_when_it_fits() {
        let page_box = page_box_pt(600, 800);
        assert!((page_box.width - 600.0).abs() < 1e-9);
        assert!((page_box.height - 800.0).abs() < 1e-9);

        // Exactly on the limit is still unscaled.
        let at_limit = page_box_pt(14_400, 14_400);
        assert!((at_limit.width - 14_400.0).abs() < 1e-9);
        assert!((at_limit.height - 14_400.0).abs() < 1e-9);
    }

    #[test]
    fn oversized_page_is_scaled_to_the_fourteen_thousand_four_hundred_point_limit() {
        // A stitched webtoon page: the height blows through the format limit by 2.7x.
        let page_box = page_box_pt(800, 40_000);
        assert!((page_box.height - 14_400.0).abs() < 1e-9, "longer side lands exactly on the limit");
        assert!((page_box.width - 288.0).abs() < 1e-9, "shorter side is scaled by the same factor");
        // The aspect ratio survives, which is what "no pixels are lost" means here.
        assert!((page_box.width / page_box.height - 800.0 / 40_000.0).abs() < 1e-12);

        // The same clamp applies to the horizontal direction.
        let wide = page_box_pt(40_000, 800);
        assert!((wide.width - 14_400.0).abs() < 1e-9);
        assert!((wide.height - 288.0).abs() < 1e-9);
    }

    #[test]
    fn oversized_page_still_produces_a_document() {
        // 1 px tall keeps the test cheap while still tripping the horizontal clamp.
        let mut builder = TypingPdfBuilder::new();
        assert!(builder.push_page(&solid_rgba(20_000, 1, [0, 0, 0]), 20_000, 1).is_ok());
        let bytes = builder.finish().unwrap_or_default();
        assert!(bytes.ends_with(b"%%EOF"));
    }

    #[test]
    fn zero_dimensions_are_rejected() {
        let mut builder = TypingPdfBuilder::new();
        assert!(matches!(builder.push_page(&[], 0, 3), Err(PdfExportError::ZeroSizedPage { width: 0, height: 3 })));
        assert!(matches!(builder.push_page(&[], 4, 0), Err(PdfExportError::ZeroSizedPage { width: 4, height: 0 })));
        assert_eq!(builder.page_count(), 0, "a rejected page must not consume an object id");
    }

    #[test]
    fn buffer_length_mismatch_is_rejected() {
        let mut builder = TypingPdfBuilder::new();
        let short = solid_rgba(4, 3, [0, 0, 0]);
        let error = builder.push_page(&short[..short.len() - 4], 4, 3);
        match error {
            Err(PdfExportError::BufferLength { width, height, expected, actual }) => {
                assert_eq!((width, height), (4, 3));
                assert_eq!(expected, 48);
                assert_eq!(actual, 44);
            }
            Err(other) => panic!("expected a buffer-length error, got {other}"),
            Ok(()) => panic!("a short buffer must not be accepted"),
        }
        assert_eq!(builder.page_count(), 0);
    }

    #[test]
    fn alpha_is_composited_over_white() {
        // Opaque pixels pass through untouched.
        let mut opaque = Vec::new();
        append_rgb_over_white(&[10, 20, 30, 255], &mut opaque);
        assert_eq!(opaque, vec![10, 20, 30]);

        // Fully transparent pixels become paper white.
        let mut clear = Vec::new();
        append_rgb_over_white(&[10, 20, 30, 0], &mut clear);
        assert_eq!(clear, vec![255, 255, 255]);

        // Half-transparent red: red stays saturated, the other channels rise halfway to white.
        let mut half = Vec::new();
        append_rgb_over_white(&[255, 0, 0, 128], &mut half);
        assert_eq!(half, vec![255, 127, 127]);

        // Two pixels in one call keep their order.
        let mut pair = Vec::new();
        append_rgb_over_white(&[0, 0, 0, 255, 255, 255, 255, 0], &mut pair);
        assert_eq!(pair, vec![0, 0, 0, 255, 255, 255]);
    }

    #[test]
    fn compressed_stream_round_trips_to_rgb_over_white() {
        use std::io::Read;

        // Two rows of two pixels, mixing opacities so the row loop is actually exercised.
        let rgba: Vec<u8> = vec![255, 0, 0, 255, 0, 255, 0, 0, 0, 0, 255, 128, 9, 9, 9, 255];
        let compressed = compress_rgb_over_white(&rgba, 8).unwrap_or_default();
        let mut decoded = Vec::new();
        let read = flate2::read::ZlibDecoder::new(compressed.as_slice()).read_to_end(&mut decoded);
        assert!(read.is_ok(), "the emitted stream must be valid zlib");
        // Row 0: opaque red, then fully transparent green -> paper white.
        // Row 1: half-transparent blue -> blue stays saturated and R/G rise halfway, then opaque grey.
        assert_eq!(decoded, vec![255, 0, 0, 255, 255, 255, 127, 127, 255, 9, 9, 9]);
    }
}
