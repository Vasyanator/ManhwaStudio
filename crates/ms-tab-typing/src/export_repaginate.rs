/*
FILE HEADER (crates/ms-tab-typing/src/export_repaginate.rs)

Purpose:
Pure re-pagination ("перенарезка") engine of the `Текст` tab export. After every page of a
chapter has been composed (source raster + clean overlay + text layers), the export may be
asked NOT to save the pages one-to-one: consecutive pages are stitched into a vertical
ribbon and the ribbon is re-sliced into pages of a chosen height.

Rules this module implements (they are product decisions, not defaults):
- Pages are NEVER rescaled. Only pages of the SAME pixel width may share a ribbon, so a
  width change starts a new ribbon (`group_pages_into_ribbons`).
- The target slice height comes either from an aspect ratio applied to the ribbon's own
  width (`TypingRepaginateMode::Ratio`) or from a fixed pixel height
  (`TypingRepaginateMode::FixedHeight`).
- The LAST slice of a ribbon stays SHORTER than the target. It is never padded, never
  stretched, and never merged into the next ribbon.

Main responsibilities:
- the persisted/UI settings of the feature (`TypingRepaginateSettings`) and the single
  place that turns them into a concrete target height (`target_height_px`);
- grouping composed pages into same-width ribbons;
- the streaming slicer (`RibbonSlicer`) that turns a ribbon into output pages while
  keeping peak memory bounded;
- the output file name of a re-paginated page (`repaginated_page_file_name`).

Key structures:
- TypingRepaginateMode, TypingRepaginateSettings
- RibbonSlicer, SlicedPage
- RepaginateError

Key functions:
- TypingRepaginateSettings::target_height_px()
- group_pages_into_ribbons()
- RibbonSlicer::push_page(), RibbonSlicer::finish()
- repaginated_page_file_name()

Notes:
This module is deliberately GUI-free and I/O-free: no egui, no `ms_storage`, no file
system, no logging of pixel data. It is pure arithmetic over straight RGBA8 buffers, which
is what makes it unit-testable and safe to run on an export worker thread.
*/

use std::num::NonZeroU32;
use std::ops::Range;

use thiserror::Error;

/// Bytes per pixel of every buffer this module handles: straight (non-premultiplied) RGBA8,
/// the same layout the tab's page compositor produces.
const BYTES_PER_PIXEL: u64 = 4;

/// Errors of the re-pagination engine.
///
/// Every variant describes a caller mistake, not a recoverable runtime condition: the
/// slicer is fed by the export worker, which knows each composed page's exact dimensions.
/// They exist so that a malformed buffer surfaces as a typed error instead of a panic
/// inside an export thread.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum RepaginateError {
    /// The pushed buffer's length does not match `width_px * height_px * 4`.
    ///
    /// `expected` and `actual` are byte counts; `width_px` is the ribbon's width (the
    /// slicer's own), `height_px` the height the caller declared for the pushed page.
    #[error("re-pagination: page buffer length {actual} B does not match {width_px}x{height_px} RGBA ({expected} B)")]
    BufferLengthMismatch { expected: u64, actual: u64, width_px: u32, height_px: u32 },
    /// A byte count needed by the slicer does not fit in `usize` on this target.
    ///
    /// Only reachable on a 32-bit target (the app also builds for `wasm32`), where a single
    /// ribbon page can legitimately exceed `usize::MAX` bytes. `bytes` is the count that
    /// failed to convert.
    #[error("re-pagination: {bytes} B does not fit in this target's address space")]
    SizeNotAddressable { bytes: u64 },
}

/// How the target height of a re-paginated page is derived.
///
/// Project-owned enum: every `match` on it must stay exhaustive (no `_` arm), so that a
/// future mode forces every call site to be reconsidered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypingRepaginateMode {
    /// Target height = ribbon width * `ratio_height` / `ratio_width` (e.g. 9:16).
    Ratio,
    /// Target height = `fixed_height_px`, independent of the ribbon's width.
    FixedHeight,
}

/// User-facing settings of the export re-pagination feature.
///
/// The three numeric fields are kept independently of `mode` on purpose: switching the mode
/// back and forth in the UI must not destroy the other mode's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TypingRepaginateSettings {
    /// When `false` the export writes composed pages one-to-one and this module is unused.
    pub(crate) enabled: bool,
    /// Which of the two height sources below is in effect.
    pub(crate) mode: TypingRepaginateMode,
    /// Width term of the aspect ratio, in arbitrary units (only the ratio matters). Must be >= 1.
    pub(crate) ratio_width: u32,
    /// Height term of the aspect ratio, in the same units as `ratio_width`. Must be >= 1.
    pub(crate) ratio_height: u32,
    /// Target page height in pixels for [`TypingRepaginateMode::FixedHeight`]. Must be >= 1.
    pub(crate) fixed_height_px: u32,
}

impl Default for TypingRepaginateSettings {
    /// Disabled, with a vertical 9:16 ratio and a 2000 px fixed height — the webtoon-page
    /// shape the feature was requested for, so that merely enabling the checkbox already
    /// produces a sane result.
    fn default() -> Self {
        Self { enabled: false, mode: TypingRepaginateMode::Ratio, ratio_width: 9, ratio_height: 16, fixed_height_px: 2000 }
    }
}

impl TypingRepaginateSettings {
    /// Resolves the target slice height, in pixels, for a ribbon `ribbon_width_px` wide.
    ///
    /// `Ratio` computes `ribbon_width_px * ratio_height / ratio_width` rounded to NEAREST
    /// (halves up) in 64-bit arithmetic; `FixedHeight` ignores the width and returns
    /// `fixed_height_px`.
    ///
    /// Returns `None` when the result would be meaningless: a zero ribbon width, a zero
    /// ratio term, a zero fixed height, or a height that does not fit in `u32`. `None` means
    /// the settings CANNOT yield a usable height for this ribbon, and the caller MUST surface
    /// it as an error (`typing.errors.export_repaginate_height_error`). It is not a licence to
    /// fall back to any other pagination: silently writing the composed pages one-to-one — or
    /// substituting a default height — would hand the user a chapter cut to a size they never
    /// chose, under a run they asked to re-paginate.
    #[must_use]
    pub(crate) fn target_height_px(&self, ribbon_width_px: u32) -> Option<NonZeroU32> {
        // A zero-width ribbon has no pages worth slicing in either mode, so it is rejected
        // before the mode is even consulted.
        if ribbon_width_px == 0 {
            return None;
        }
        let height = match self.mode {
            TypingRepaginateMode::Ratio => {
                if self.ratio_width == 0 || self.ratio_height == 0 {
                    return None;
                }
                let width = u64::from(ribbon_width_px);
                let ratio_width = u64::from(self.ratio_width);
                // Round to nearest by adding half the divisor before the truncating division.
                // All three operands are <= u32::MAX, so the product and the sum stay far
                // inside u64 and no checked arithmetic is needed here.
                let scaled = width * u64::from(self.ratio_height) + ratio_width / 2;
                u32::try_from(scaled / ratio_width).ok()?
            }
            TypingRepaginateMode::FixedHeight => self.fixed_height_px,
        };
        NonZeroU32::new(height)
    }
}

/// Groups composed pages into ribbons of consecutive pages that share one pixel width.
///
/// `widths` holds the pages' widths in export order. The returned ranges index `widths`,
/// are non-empty, contiguous, and cover the whole slice in order; a width change starts a
/// new ribbon because pages of different widths are never rescaled to fit one another.
/// An empty input yields an empty output.
#[must_use]
pub(crate) fn group_pages_into_ribbons(widths: &[u32]) -> Vec<Range<usize>> {
    let mut ribbons: Vec<Range<usize>> = Vec::new();
    let mut start = 0usize;
    for (index, width) in widths.iter().enumerate() {
        // `start < index` guarantees the previous element exists, so the comparison below
        // never needs a bounds check of its own.
        if index > start && *width != widths[start] {
            ribbons.push(start..index);
            start = index;
        }
    }
    if start < widths.len() {
        ribbons.push(start..widths.len());
    }
    ribbons
}

/// One output page produced by the slicer: straight RGBA8, row-major, `width_px` wide.
///
/// `rgba.len()` always equals `width_px * height_px * 4`. `height_px` equals the slicer's
/// target height for every page except a ribbon's last one, which is SHORTER (never padded).
pub(crate) struct SlicedPage {
    /// Straight (non-premultiplied) RGBA8 pixels, row-major, top row first.
    pub(crate) rgba: Vec<u8>,
    /// Page width in pixels — always the ribbon's width.
    pub(crate) width_px: u32,
    /// Page height in pixels.
    pub(crate) height_px: u32,
}

impl std::fmt::Debug for SlicedPage {
    /// Prints the dimensions and the buffer SIZE only: the derived form would dump several
    /// hundred megabytes of pixels into a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlicedPage").field("width_px", &self.width_px).field("height_px", &self.height_px).field("rgba_len", &self.rgba.len()).finish()
    }
}

/// Streaming re-slicer of ONE ribbon.
///
/// Pages of the ribbon are pushed in export order; every time enough rows have accumulated
/// the slicer hands back finished output pages, and [`RibbonSlicer::finish`] releases the
/// shorter tail. Pushing a page of a different width is a caller error and is rejected by
/// the buffer-length check (see [`RibbonSlicer::push_page`]).
///
/// MEMORY CONTRACT — the whole reason this type exists instead of a `concat` + `chunks`
/// one-liner: a chapter can be 40+ pages of 800x3000 RGBA (~180 MB) or 20 ribbon pages
/// (~640 MB), so the stitched ribbon must never be materialized. The pending buffer is
/// drained as slices are emitted and therefore always holds LESS than one target page at
/// rest; peak usage is bounded by roughly one target page plus the page being pushed.
pub(crate) struct RibbonSlicer {
    /// Ribbon width in pixels; every pushed page and every emitted page has this width.
    width_px: NonZeroU32,
    /// Target height of a full output page, in pixels.
    target_height_px: NonZeroU32,
    /// Bytes of one pixel row (`width_px * 4`), kept in `u64` so that the 32-bit targets
    /// (`wasm32`) convert it explicitly instead of overflowing.
    row_bytes: u64,
    /// Bytes of one full output page (`row_bytes * target_height_px`).
    target_bytes: u64,
    /// Rows accumulated but not yet emitted. Always shorter than `target_bytes`, and always
    /// a whole number of rows.
    pending: Vec<u8>,
}

impl std::fmt::Debug for RibbonSlicer {
    /// Prints the geometry and how many bytes are pending, never the pending pixels.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RibbonSlicer").field("width_px", &self.width_px).field("target_height_px", &self.target_height_px).field("pending_bytes", &self.pending.len()).finish()
    }
}

impl RibbonSlicer {
    /// Creates a slicer for a ribbon `width_px` pixels wide, cutting pages of
    /// `target_height_px` pixels.
    ///
    /// Both dimensions are non-zero by type, so construction cannot fail; the byte counts
    /// derived here stay in `u64` and are converted to `usize` only where they are used.
    #[must_use]
    pub(crate) fn new(width_px: NonZeroU32, target_height_px: NonZeroU32) -> Self {
        // Both products are bounded by u32::MAX * u32::MAX * 4 < u64::MAX, so u64 cannot
        // overflow here; addressability is checked at use sites instead.
        let row_bytes = u64::from(width_px.get()) * BYTES_PER_PIXEL;
        let target_bytes = row_bytes * u64::from(target_height_px.get());
        Self { width_px, target_height_px, row_bytes, target_bytes, pending: Vec::new() }
    }

    /// Appends one composed page of the ribbon and returns every output page that can now
    /// be emitted.
    ///
    /// `rgba` must be straight RGBA8, row-major, exactly `self.width_px` wide and
    /// `height_px` tall; a `height_px` of zero is accepted and contributes nothing. The
    /// returned pages are full-height and in ribbon order — pushing one tall page can emit
    /// several of them, and pushing a short page usually emits none.
    ///
    /// # Errors
    /// [`RepaginateError::BufferLengthMismatch`] when `rgba.len()` is not
    /// `width_px * height_px * 4` — which is also what catches a page of the wrong width.
    /// [`RepaginateError::SizeNotAddressable`] when a required byte count does not fit in
    /// `usize` on this target (32-bit only). On either error the slicer is left untouched.
    pub(crate) fn push_page(&mut self, rgba: &[u8], height_px: u32) -> Result<Vec<SlicedPage>, RepaginateError> {
        let expected = self.row_bytes * u64::from(height_px);
        let actual = u64::try_from(rgba.len()).unwrap_or(u64::MAX);
        if actual != expected {
            return Err(RepaginateError::BufferLengthMismatch { expected, actual, width_px: self.width_px.get(), height_px });
        }
        let target_bytes = usize::try_from(self.target_bytes).map_err(|_| RepaginateError::SizeNotAddressable { bytes: self.target_bytes })?;
        // Reserving the exact tail keeps the append to a single growth step; the buffer is
        // drained again below, so this does not accumulate across pushes.
        self.pending.reserve(rgba.len());
        self.pending.extend_from_slice(rgba);
        let mut emitted: Vec<SlicedPage> = Vec::new();
        // Drain full pages out of the pending buffer one at a time: after the loop `pending`
        // again holds less than one target page, which is the memory contract of this type.
        while self.pending.len() >= target_bytes {
            let page: Vec<u8> = self.pending.drain(..target_bytes).collect();
            emitted.push(SlicedPage { rgba: page, width_px: self.width_px.get(), height_px: self.target_height_px.get() });
        }
        Ok(emitted)
    }

    /// Emits the ribbon's tail — the rows left over after the last full page.
    ///
    /// The tail is SHORTER than the target height and is never padded. Returns `None` when
    /// nothing is pending (an empty ribbon, or one whose length was an exact multiple of the
    /// target). The slicer is emptied, so a second call returns `None`.
    #[must_use]
    pub(crate) fn finish(&mut self) -> Option<SlicedPage> {
        if self.pending.is_empty() {
            return None;
        }
        // A non-empty `pending` can only exist after a successful `push_page`, which already
        // proved `target_bytes` (>= `row_bytes`) addressable; the fallible conversion is kept
        // instead of an `expect` because this module must not panic (CLAUDE.md §7, §11).
        let row_bytes = usize::try_from(self.row_bytes).ok()?;
        let rgba = std::mem::take(&mut self.pending);
        let height_px = u32::try_from(rgba.len() / row_bytes).ok()?;
        Some(SlicedPage { rgba, width_px: self.width_px.get(), height_px })
    }
}

/// Builds the file name of a re-paginated output page: `"{base} {NNN}.png"`.
///
/// `base` is the chapter/page name stem and arrives ALREADY SANITIZED for the filesystem;
/// when it is empty the name degrades to just the number. `index` is 0-based and is written
/// 1-based. `total` is the number of pages the run will write and only selects the zero
/// padding: at least three digits, widened when `total` needs more (1000 pages produce
/// `0001`). An `index` beyond `total` still yields a valid name, merely a wider one.
#[must_use]
pub(crate) fn repaginated_page_file_name(base: &str, index: usize, total: usize) -> String {
    // `total.max(1)` keeps a degenerate empty run at the three-digit minimum instead of
    // measuring the string "0".
    let width = total.max(1).to_string().len().max(3);
    let number = index.saturating_add(1);
    if base.is_empty() { format!("{number:0width$}.png") } else { format!("{base} {number:0width$}.png") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a page whose every pixel encodes its ABSOLUTE row index inside the ribbon, so
    /// that a slice's content can be checked against the offset it should have come from.
    fn gradient_page(width_px: u32, height_px: u32, first_row: u32) -> Vec<u8> {
        let mut rgba = Vec::new();
        for row in 0..height_px {
            let value = u8::try_from((first_row + row) % 256).unwrap();
            for _ in 0..width_px {
                rgba.extend_from_slice(&[value, value, value, 255]);
            }
        }
        rgba
    }

    /// Shorthand for a test dimension. Panicking on zero is the intended failure signal
    /// inside tests, where a wrong literal is a bug in the test itself.
    fn nz(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    #[test]
    fn default_settings_are_a_disabled_vertical_9_16_page() {
        let settings = TypingRepaginateSettings::default();
        assert!(!settings.enabled);
        assert_eq!(settings.mode, TypingRepaginateMode::Ratio);
        assert_eq!(settings.ratio_width, 9);
        assert_eq!(settings.ratio_height, 16);
        assert_eq!(settings.fixed_height_px, 2000);
    }

    #[test]
    fn ratio_height_is_exact_when_the_division_is_exact() {
        let settings = TypingRepaginateSettings { mode: TypingRepaginateMode::Ratio, ratio_width: 9, ratio_height: 16, ..TypingRepaginateSettings::default() };
        assert_eq!(settings.target_height_px(900).map(NonZeroU32::get), Some(1600));
    }

    #[test]
    fn ratio_height_rounds_to_nearest() {
        let settings = TypingRepaginateSettings { mode: TypingRepaginateMode::Ratio, ratio_width: 9, ratio_height: 16, ..TypingRepaginateSettings::default() };
        // 800 * 16 / 9 = 1422.22… -> 1422 (rounds down).
        assert_eq!(settings.target_height_px(800).map(NonZeroU32::get), Some(1422));
        // 3 * 5 / 2 = 7.5 -> 8 (a half rounds up).
        let halves = TypingRepaginateSettings { mode: TypingRepaginateMode::Ratio, ratio_width: 2, ratio_height: 5, ..TypingRepaginateSettings::default() };
        assert_eq!(halves.target_height_px(3).map(NonZeroU32::get), Some(8));
    }

    #[test]
    fn ratio_height_rejects_zero_operands_and_zero_results() {
        let settings = TypingRepaginateSettings { mode: TypingRepaginateMode::Ratio, ratio_width: 9, ratio_height: 16, ..TypingRepaginateSettings::default() };
        assert_eq!(settings.target_height_px(0), None);
        let zero_width = TypingRepaginateSettings { ratio_width: 0, ..settings };
        assert_eq!(zero_width.target_height_px(800), None);
        let zero_height = TypingRepaginateSettings { ratio_height: 0, ..settings };
        assert_eq!(zero_height.target_height_px(800), None);
        // A ratio so flat that the rounded result is zero must not become a 0-px page.
        let flat = TypingRepaginateSettings { ratio_width: 1000, ratio_height: 1, ..settings };
        assert_eq!(flat.target_height_px(100), None);
    }

    #[test]
    fn ratio_height_rejects_a_result_wider_than_u32() {
        let settings = TypingRepaginateSettings { mode: TypingRepaginateMode::Ratio, ratio_width: 1, ratio_height: u32::MAX, ..TypingRepaginateSettings::default() };
        assert_eq!(settings.target_height_px(u32::MAX), None);
    }

    #[test]
    fn fixed_height_ignores_the_ribbon_width_but_not_a_zero_height() {
        let settings = TypingRepaginateSettings { mode: TypingRepaginateMode::FixedHeight, fixed_height_px: 2000, ..TypingRepaginateSettings::default() };
        assert_eq!(settings.target_height_px(800).map(NonZeroU32::get), Some(2000));
        assert_eq!(settings.target_height_px(5).map(NonZeroU32::get), Some(2000));
        assert_eq!(settings.target_height_px(0), None);
        let zero = TypingRepaginateSettings { fixed_height_px: 0, ..settings };
        assert_eq!(zero.target_height_px(800), None);
    }

    #[test]
    fn grouping_handles_empty_single_run_and_alternating_widths() {
        assert!(group_pages_into_ribbons(&[]).is_empty());
        assert_eq!(group_pages_into_ribbons(&[800]), vec![0..1]);
        assert_eq!(group_pages_into_ribbons(&[800, 800, 800]), vec![0..3]);
        assert_eq!(group_pages_into_ribbons(&[800, 700, 800]), vec![0..1, 1..2, 2..3]);
        assert_eq!(group_pages_into_ribbons(&[800, 800, 700, 700, 700, 900]), vec![0..2, 2..5, 5..6]);
    }

    #[test]
    fn exact_multiple_slices_leave_no_tail() {
        let mut slicer = RibbonSlicer::new(nz(4), nz(10));
        let emitted = slicer.push_page(&gradient_page(4, 20, 0), 20).unwrap();
        assert_eq!(emitted.len(), 2);
        assert_eq!(emitted[0].height_px, 10);
        assert_eq!(emitted[1].height_px, 10);
        assert!(slicer.finish().is_none());
    }

    #[test]
    fn one_push_can_emit_several_slices_with_the_right_pixels() {
        let mut slicer = RibbonSlicer::new(nz(2), nz(3));
        let emitted = slicer.push_page(&gradient_page(2, 7, 0), 7).unwrap();
        assert_eq!(emitted.len(), 2);
        // Row 0 of the second slice must be ribbon row 3, and its last row ribbon row 5.
        let second = &emitted[1];
        assert_eq!(second.width_px, 2);
        assert_eq!(second.height_px, 3);
        let row_bytes = 2 * 4;
        assert_eq!(&second.rgba[..row_bytes], &[3, 3, 3, 255, 3, 3, 3, 255]);
        assert_eq!(&second.rgba[row_bytes * 2..row_bytes * 3], &[5, 5, 5, 255, 5, 5, 5, 255]);
        // The tail is the single leftover row 6, shorter than the 3-row target.
        let tail = slicer.finish().unwrap();
        assert_eq!(tail.height_px, 1);
        assert_eq!(&tail.rgba[..], &[6, 6, 6, 255, 6, 6, 6, 255]);
    }

    #[test]
    fn slices_span_the_boundary_between_two_pushed_pages() {
        let mut slicer = RibbonSlicer::new(nz(1), nz(4));
        // Rows 0..3 on the first page, rows 3..6 on the second: the first slice must contain
        // one row taken from the SECOND push.
        assert!(slicer.push_page(&gradient_page(1, 3, 0), 3).unwrap().is_empty());
        let emitted = slicer.push_page(&gradient_page(1, 3, 3), 3).unwrap();
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].rgba, vec![0, 0, 0, 255, 1, 1, 1, 255, 2, 2, 2, 255, 3, 3, 3, 255]);
        let tail = slicer.finish().unwrap();
        assert_eq!(tail.height_px, 2);
        assert_eq!(tail.rgba, vec![4, 4, 4, 255, 5, 5, 5, 255]);
    }

    #[test]
    fn a_ribbon_shorter_than_one_target_yields_a_single_short_page() {
        let mut slicer = RibbonSlicer::new(nz(3), nz(100));
        assert!(slicer.push_page(&gradient_page(3, 12, 0), 12).unwrap().is_empty());
        let tail = slicer.finish().unwrap();
        assert_eq!((tail.width_px, tail.height_px), (3, 12));
        assert_eq!(tail.rgba.len(), 3 * 12 * 4);
        assert!(slicer.finish().is_none());
    }

    #[test]
    fn an_empty_ribbon_and_a_zero_height_page_emit_nothing() {
        let mut slicer = RibbonSlicer::new(nz(8), nz(16));
        assert!(slicer.finish().is_none());
        assert!(slicer.push_page(&[], 0).unwrap().is_empty());
        assert!(slicer.finish().is_none());
    }

    #[test]
    fn a_wrong_buffer_length_is_a_typed_error_and_leaves_the_slicer_untouched() {
        let mut slicer = RibbonSlicer::new(nz(4), nz(10));
        let err = slicer.push_page(&gradient_page(4, 5, 0), 6).unwrap_err();
        assert_eq!(err, RepaginateError::BufferLengthMismatch { expected: 4 * 6 * 4, actual: 4 * 5 * 4, width_px: 4, height_px: 6 });
        // A page of the WRONG WIDTH is caught by the same check.
        let wrong_width = slicer.push_page(&gradient_page(5, 5, 0), 5).unwrap_err();
        assert!(matches!(wrong_width, RepaginateError::BufferLengthMismatch { .. }));
        assert!(slicer.finish().is_none());
    }

    #[test]
    fn page_names_pad_to_three_digits_and_widen_with_the_total() {
        assert_eq!(repaginated_page_file_name("Глава 1", 0, 12), "Глава 1 001.png");
        assert_eq!(repaginated_page_file_name("Глава 1", 11, 12), "Глава 1 012.png");
        assert_eq!(repaginated_page_file_name("page", 999, 1000), "page 1000.png");
        assert_eq!(repaginated_page_file_name("page", 0, 1000), "page 0001.png");
        assert_eq!(repaginated_page_file_name("", 4, 10), "005.png");
        assert_eq!(repaginated_page_file_name("x", 0, 0), "x 001.png");
    }
}
