/*
File: crates/ms-tab-typing/src/image_encode.rs

Purpose:
Pure, GUI-free encoding of a composed straight-RGBA8 page into the bytes of one image file
(PNG, JPEG or lossless WebP), plus the single owner of the rule deciding which opened-file
formats can be overwritten in place by «Сохранить» (single-image mode).

Key structures:
- `ImageSaveFormat`: the writable formats and their file extensions (one table, `SAVE_EXTENSIONS`,
  behind `from_extension`, the per-format dialog filter `extensions` and the format list
  `save_formats`).
- `ImageEncoding`: format + JPEG quality + `AlphaPolicy` + optional ICC profile to embed.
- `EncodeError`: typed encode failure with a technical (non-localized) `Display`.

Key functions:
- `encode_rgba()`: RGBA8 image -> encoded file bytes in memory (no I/O).
- `in_place_format()`: the in-place writability rule over primitive inputs;
  `ImageSaveFormat::in_place_for()` is its adapter over an `ms_project::SingleImageSession`.

Notes:
JPEG has no alpha, so it is always flattened over white by `ms_raster::rgba_over_white_to_rgb`
(the one owner of that rule, shared with the PDF export). Pixels are never colour converted; an
ICC profile is only re-embedded so colour-managed viewers keep showing the source's colours. A
format whose encoder rejects ICC embedding is logged and encoded without it. No localization
happens here: callers map `EncodeError` onto their own catalog keys. The project PNG export
(`tab/export.rs`) encodes through `encode_rgba` with `KeepRgba` and no ICC, which is byte-identical
to a bare `PngEncoder::new(..).write_image(.., Rgba8)`.
*/

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::{ExtendedColorType, ImageBuffer, ImageEncoder, ImageFormat, Rgba};
use std::ops::Deref;

/// Bytes per interleaved RGBA8 pixel.
const RGBA_CHANNELS: usize = 4;
/// Bytes per interleaved RGB8 pixel.
const RGB_CHANNELS: usize = 3;
/// Inclusive range of the JPEG quality knob (1 = smallest file, 100 = best).
const JPEG_QUALITY_RANGE: std::ops::RangeInclusive<u8> = 1..=100;

/// Every file extension (lowercase, without the dot) a save target may carry, with the format it
/// names. The ONE table behind [`ImageSaveFormat::from_extension`], the «Сохранить как» dialog's
/// filter ([`ImageSaveFormat::extensions`]) and the format list ([`ImageSaveFormat::save_formats`]),
/// so they can never disagree. Table order is the order formats are offered to the user.
const SAVE_EXTENSIONS: [(&str, ImageSaveFormat); 5] = [
    ("png", ImageSaveFormat::Png),
    ("jpg", ImageSaveFormat::Jpeg),
    ("jpeg", ImageSaveFormat::Jpeg),
    ("jpe", ImageSaveFormat::Jpeg),
    ("webp", ImageSaveFormat::WebpLossless),
];

/// A file format this module can write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageSaveFormat {
    /// PNG (lossless, alpha kept).
    Png,
    /// Baseline JPEG; alpha is flattened over white.
    Jpeg,
    /// WebP in its lossless VP8L mode (alpha kept); lossy WebP is never written.
    WebpLossless,
}

impl ImageSaveFormat {
    /// Maps a file extension (without the dot, ASCII case-insensitive) to the format it names:
    /// `png`, `jpg` / `jpeg` / `jpe`, `webp`. Any other extension, including an empty one, is
    /// `None` — the caller must reject it, never guess a format.
    #[must_use]
    pub fn from_extension(ext: &str) -> Option<Self> {
        SAVE_EXTENSIONS.iter().find(|(known, _)| known.eq_ignore_ascii_case(ext)).map(|(_, format)| *format)
    }

    /// Every writable format once, in `SAVE_EXTENSIONS` order (the order a format choice offers
    /// them in). Derived from the extension table, so a new row there is offered automatically.
    pub fn save_formats() -> impl Iterator<Item = Self> {
        SAVE_EXTENSIONS
            .iter()
            .enumerate()
            .filter(|(idx, (_, format))| SAVE_EXTENSIONS[..*idx].iter().all(|(_, earlier)| earlier != format))
            .map(|(_, (_, format))| *format)
    }

    /// Every extension [`Self::from_extension`] maps to `self`, lowercase and without the dot (for a
    /// file dialog filter; a case-sensitive dialog needs the caller to add uppercase copies).
    pub fn extensions(self) -> impl Iterator<Item = &'static str> {
        SAVE_EXTENSIONS.iter().filter(move |(_, format)| *format == self).map(|(ext, _)| *ext)
    }

    /// The format's canonical name for a UI choice ("PNG", "JPEG", "WebP"). Not localized: these are
    /// proper names of file formats, identical in every UI language.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::WebpLossless => "WebP",
        }
    }

    /// The canonical extension (without the dot) appended when a target path has none.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::WebpLossless => "webp",
        }
    }

    /// The format «Сохранить» may overwrite `session`'s opened file in, or `None` when the save
    /// must act as «Сохранить как». A thin adapter: maps the session's content format onto
    /// `image::ImageFormat` and delegates to [`in_place_format`], the single owner of the rule.
    #[must_use]
    pub fn in_place_for(session: &ms_project::SingleImageSession) -> Option<Self> {
        in_place_format(source_format_to_image_format(session.source_format), session.source_animated, session.extension_matches_content)
    }

    /// Short technical name used in `EncodeError` messages and logs.
    fn name(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::WebpLossless => "WebP (lossless)",
        }
    }
}

/// How an alpha channel is written by the formats that can store one (PNG, WebP). JPEG ignores
/// this: it is always flattened over white.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlphaPolicy {
    /// Always write RGBA8, even when every pixel is opaque.
    KeepRgba,
    /// Write RGB8 when every pixel has alpha 255 (lossless; matches an opaque source), RGBA8
    /// otherwise.
    DropIfOpaque,
}

/// Everything `encode_rgba` needs besides the pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageEncoding {
    /// Output file format.
    pub format: ImageSaveFormat,
    /// JPEG quality in `1..=100`; read only when `format` is `Jpeg`, where a value outside the
    /// range is rejected with `EncodeError::InvalidJpegQuality`.
    pub jpeg_quality: u8,
    /// Alpha handling for PNG / WebP.
    pub alpha: AlphaPolicy,
    /// Raw ICC profile bytes to embed unchanged, or `None` for no profile.
    pub icc_profile: Option<Vec<u8>>,
}

/// Why `encode_rgba` produced no bytes. `Display` is technical English for logs; user-facing
/// text is chosen by the caller.
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    /// The JPEG quality is outside `1..=100`.
    #[error("JPEG quality {0} is outside 1..=100")]
    InvalidJpegQuality(u8),
    /// The image's backing buffer is not exactly `width * height * 4` bytes (or that product
    /// overflows `usize`).
    #[error("RGBA8 buffer of {len} bytes does not match {width}x{height}")]
    BufferLength {
        /// Declared width in pixels.
        width: u32,
        /// Declared height in pixels.
        height: u32,
        /// Actual buffer length in bytes.
        len: usize,
    },
    /// Flattening over white for JPEG rejected the buffer.
    #[error("flattening RGBA8 over white failed: {0}")]
    OverWhite(#[from] ms_raster::RasterError),
    /// The format encoder rejected the image (dimensions, colour type, I/O into the buffer).
    #[error("{format} encoder failed for {width}x{height}: {source}", format = .format.name())]
    Encoder {
        /// Format being written.
        format: ImageSaveFormat,
        /// Image width in pixels.
        width: u32,
        /// Image height in pixels.
        height: u32,
        /// The encoder's own error.
        #[source]
        source: image::ImageError,
    },
}

/// Encodes a straight-RGBA8 image into the bytes of one `enc.format` file, in memory.
///
/// Generic over the pixel container so a caller holding a borrowed `&[u8]` page can wrap it in an
/// `ImageBuffer<Rgba<u8>, &[u8]>` without copying; an owned `RgbaImage` works as is.
/// PNG / WebP honour `enc.alpha`; JPEG is flattened over white at `enc.jpeg_quality`. The ICC
/// profile, if any, is embedded unchanged; an encoder that cannot embed one is logged and the
/// image is still written.
///
/// # Errors
/// `InvalidJpegQuality` for a JPEG quality outside `1..=100`; `BufferLength` when the backing
/// buffer is not exactly `width * height * 4` bytes; `OverWhite` if the over-white flatten rejects
/// the buffer; `Encoder` when the format encoder fails (e.g. zero or oversized dimensions).
pub fn encode_rgba<C>(rgba: &ImageBuffer<Rgba<u8>, C>, enc: &ImageEncoding) -> Result<Vec<u8>, EncodeError>
where
    C: Deref<Target = [u8]>,
{
    let (width, height) = rgba.dimensions();
    let raw: &[u8] = rgba.as_raw();
    // `ImageBuffer::from_raw` accepts a container LONGER than the image, and every
    // `ImageEncoder::write_image` panics on a length mismatch, so the exact length is checked here
    // instead of letting a malformed caller buffer panic the worker.
    let expected = usize::try_from(width).ok().zip(usize::try_from(height).ok()).and_then(|(w, h)| w.checked_mul(h)).and_then(|px| px.checked_mul(RGBA_CHANNELS));
    if expected != Some(raw.len()) {
        return Err(EncodeError::BufferLength { width, height, len: raw.len() });
    }
    let encoder_error = |source: image::ImageError| EncodeError::Encoder { format: enc.format, width, height, source };
    let mut out = Vec::new();
    match enc.format {
        ImageSaveFormat::Png => {
            let (pixels, color) = pixels_for_alpha_policy(raw, enc.alpha);
            let mut encoder = PngEncoder::new(&mut out);
            embed_icc(&mut encoder, enc);
            encoder.write_image(&pixels, width, height, color).map_err(encoder_error)?;
        }
        ImageSaveFormat::WebpLossless => {
            let (pixels, color) = pixels_for_alpha_policy(raw, enc.alpha);
            let mut encoder = WebPEncoder::new_lossless(&mut out);
            embed_icc(&mut encoder, enc);
            encoder.write_image(&pixels, width, height, color).map_err(encoder_error)?;
        }
        ImageSaveFormat::Jpeg => {
            if !JPEG_QUALITY_RANGE.contains(&enc.jpeg_quality) {
                return Err(EncodeError::InvalidJpegQuality(enc.jpeg_quality));
            }
            let mut rgb = Vec::new();
            ms_raster::rgba_over_white_to_rgb(raw, &mut rgb)?;
            let mut encoder = JpegEncoder::new_with_quality(&mut out, enc.jpeg_quality);
            embed_icc(&mut encoder, enc);
            encoder.write_image(&rgb, width, height, ExtendedColorType::Rgb8).map_err(encoder_error)?;
        }
    }
    Ok(out)
}

/// Returns the pixel bytes and colour type to hand the PNG / WebP encoder under `policy`:
/// borrowed RGBA8, or a freshly packed RGB8 copy when `DropIfOpaque` finds every alpha == 255.
fn pixels_for_alpha_policy(raw: &[u8], policy: AlphaPolicy) -> (std::borrow::Cow<'_, [u8]>, ExtendedColorType) {
    let fully_opaque = || raw.chunks_exact(RGBA_CHANNELS).all(|px| px[3] == u8::MAX);
    match policy {
        AlphaPolicy::DropIfOpaque if fully_opaque() => {
            let mut rgb = Vec::with_capacity(raw.len() / RGBA_CHANNELS * RGB_CHANNELS);
            for px in raw.chunks_exact(RGBA_CHANNELS) {
                // Indexing is safe: `chunks_exact(4)` yields slices of exactly four bytes.
                rgb.extend_from_slice(&px[..RGB_CHANNELS]);
            }
            (std::borrow::Cow::Owned(rgb), ExtendedColorType::Rgb8)
        }
        AlphaPolicy::DropIfOpaque | AlphaPolicy::KeepRgba => (std::borrow::Cow::Borrowed(raw), ExtendedColorType::Rgba8),
    }
}

/// Hands `enc.icc_profile` (if any) to `encoder`. An encoder that does not support ICC embedding
/// is logged with the format and the image is written without a profile (plan D2: a missing
/// profile must not block a save).
fn embed_icc<E: ImageEncoder>(encoder: &mut E, enc: &ImageEncoding) {
    let Some(profile) = enc.icc_profile.as_ref() else {
        return;
    };
    if let Err(err) = encoder.set_icc_profile(profile.clone()) {
        ms_log::runtime_log::log_warn(format!(
            "image_encode: ICC profile not embedded.\nFormat: {}\nProfile size: {} bytes\nError: {err}\nPossible cause: the encoder does not support ICC profiles; the image is written without one",
            enc.format.name(),
            profile.len()
        ));
    }
}

/// The single owner of the in-place writability rule (plan D7): which format «Сохранить» may
/// overwrite the opened file in, or `None` when the save must act as «Сохранить как».
///
/// - `content_format`: the opened file's format detected from its CONTENT
///   (`ImageReader::with_guessed_format`), not from its extension. `image::ImageFormat` is the
///   input because the opener already gets it from that call and any narrower source enum
///   (ms-project's `SourceImageFormat`) maps onto it one to one.
/// - `animated`: the source had more than one frame (APNG, animated WebP, multi-frame GIF); one
///   frame must never silently replace an animation.
/// - `extension_matches_content`: the path's extension names the same format as the content.
///
/// Writable in place: PNG -> `Png`, JPEG -> `Jpeg`, WebP -> `WebpLossless` (a lossy WebP becomes
/// lossless: larger, but no further loss). Every other format is read-only here.
#[must_use]
pub fn in_place_format(content_format: ImageFormat, animated: bool, extension_matches_content: bool) -> Option<ImageSaveFormat> {
    if animated || !extension_matches_content {
        return None;
    }
    match content_format {
        ImageFormat::Png => Some(ImageSaveFormat::Png),
        ImageFormat::Jpeg => Some(ImageSaveFormat::Jpeg),
        ImageFormat::WebP => Some(ImageSaveFormat::WebpLossless),
        // `ImageFormat` is `#[non_exhaustive]` (image-0.25 `src/io/format.rs`), so a wildcard is
        // required; every format not listed above, present or future, is not writable in place.
        _ => None,
    }
}

/// One-to-one mapping of a single-image source format onto the `image` crate's format, the
/// input [`in_place_format`] speaks.
fn source_format_to_image_format(format: ms_project::SourceImageFormat) -> ImageFormat {
    use ms_project::SourceImageFormat;
    match format {
        SourceImageFormat::Png => ImageFormat::Png,
        SourceImageFormat::Jpeg => ImageFormat::Jpeg,
        SourceImageFormat::WebP => ImageFormat::WebP,
        SourceImageFormat::Gif => ImageFormat::Gif,
        SourceImageFormat::Bmp => ImageFormat::Bmp,
        SourceImageFormat::Tiff => ImageFormat::Tiff,
        SourceImageFormat::Tga => ImageFormat::Tga,
        SourceImageFormat::Qoi => ImageFormat::Qoi,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ColorType, ImageDecoder, ImageReader, RgbaImage};
    use std::io::Cursor;

    /// Max per-component difference accepted after a JPEG round trip of flat colour blocks at
    /// quality 95 (chroma subsampling and DCT quantization; flat 8x8 blocks stay very close).
    const JPEG_TOLERANCE: u8 = 6;

    fn encoding(format: ImageSaveFormat, alpha: AlphaPolicy) -> ImageEncoding {
        ImageEncoding { format, jpeg_quality: 95, alpha, icc_profile: None }
    }

    /// 16x16 image with a 4x4 grid of flat colour blocks; `alpha` for every pixel.
    fn fixture(alpha: u8) -> RgbaImage {
        RgbaImage::from_fn(16, 16, |x, y| {
            let bx = u8::try_from(x / 4).unwrap_or(0);
            let by = u8::try_from(y / 4).unwrap_or(0);
            Rgba([bx * 60, by * 60, 200 - bx * 20, alpha])
        })
    }

    fn decode(bytes: &[u8]) -> (image::DynamicImage, ImageFormat) {
        let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().expect("cursor reads cannot fail");
        let format = reader.format().expect("encoded bytes have a recognizable signature");
        (reader.decode().expect("encoded bytes decode"), format)
    }

    /// The format list and the per-format extensions are both derived from `SAVE_EXTENSIONS`: every
    /// format is listed once, in table order, and each extension maps back to its own format.
    #[test]
    fn save_formats_and_extensions_follow_the_one_table() {
        let formats: Vec<ImageSaveFormat> = ImageSaveFormat::save_formats().collect();
        assert_eq!(formats, [ImageSaveFormat::Png, ImageSaveFormat::Jpeg, ImageSaveFormat::WebpLossless]);
        for format in formats {
            let extensions: Vec<&str> = format.extensions().collect();
            assert!(extensions.contains(&format.extension()), "{format:?} lists its canonical extension");
            assert!(extensions.iter().all(|ext| ImageSaveFormat::from_extension(ext) == Some(format)), "{format:?}");
        }
        assert_eq!(ImageSaveFormat::Jpeg.extensions().collect::<Vec<_>>(), ["jpg", "jpeg", "jpe"]);
        let total: usize = ImageSaveFormat::save_formats().map(|format| format.extensions().count()).sum();
        assert_eq!(total, SAVE_EXTENSIONS.len(), "every table row belongs to exactly one listed format");
    }

    #[test]
    fn png_round_trip_is_exact_and_keeps_alpha() {
        let mut img = fixture(255);
        img.put_pixel(3, 5, Rgba([10, 20, 30, 128]));
        let bytes = encode_rgba(&img, &encoding(ImageSaveFormat::Png, AlphaPolicy::DropIfOpaque)).expect("encode");
        let (decoded, format) = decode(&bytes);
        assert_eq!(format, ImageFormat::Png);
        assert_eq!(decoded.color(), ColorType::Rgba8);
        assert_eq!(decoded.to_rgba8(), img);
    }

    #[test]
    fn webp_lossless_round_trip_is_exact() {
        let mut img = fixture(255);
        img.put_pixel(0, 0, Rgba([1, 2, 3, 0]));
        img.put_pixel(1, 0, Rgba([9, 8, 7, 77]));
        let bytes = encode_rgba(&img, &encoding(ImageSaveFormat::WebpLossless, AlphaPolicy::KeepRgba)).expect("encode");
        let (decoded, format) = decode(&bytes);
        assert_eq!(format, ImageFormat::WebP);
        let decoded = decoded.to_rgba8();
        // A fully transparent pixel's colour may be canonicalized by a lossless encoder; only
        // its alpha is meaningful.
        assert_eq!(decoded.get_pixel(0, 0)[3], 0);
        for (x, y, px) in img.enumerate_pixels().skip(1) {
            assert_eq!(decoded.get_pixel(x, y), px, "pixel ({x},{y})");
        }
    }

    #[test]
    fn jpeg_round_trip_within_tolerance() {
        let img = fixture(255);
        let bytes = encode_rgba(&img, &encoding(ImageSaveFormat::Jpeg, AlphaPolicy::KeepRgba)).expect("encode");
        let (decoded, format) = decode(&bytes);
        assert_eq!(format, ImageFormat::Jpeg);
        assert_eq!(decoded.color(), ColorType::Rgb8);
        let decoded = decoded.to_rgb8();
        // Compare block centres, away from the block edges where chroma subsampling blends
        // neighbouring colours.
        for (x, y) in [(1u32, 1u32), (5, 1), (9, 6), (14, 13), (2, 10)] {
            let want = img.get_pixel(x, y);
            let got = decoded.get_pixel(x, y);
            for c in 0..3 {
                assert!(want[c].abs_diff(got[c]) <= JPEG_TOLERANCE, "({x},{y}) channel {c}: want {} got {}", want[c], got[c]);
            }
        }
    }

    #[test]
    fn jpeg_half_transparent_pixel_is_flattened_over_white() {
        let img = RgbaImage::from_pixel(16, 16, Rgba([0, 100, 200, 128]));
        let bytes = encode_rgba(&img, &encoding(ImageSaveFormat::Jpeg, AlphaPolicy::KeepRgba)).expect("encode");
        let decoded = decode(&bytes).0.to_rgb8();
        // (c * 128 + 255 * 127 + 127) / 255, the ms-raster over-white rule.
        let expected = [127u8, 177, 227];
        let got = decoded.get_pixel(8, 8);
        for c in 0..3 {
            assert!(expected[c].abs_diff(got[c]) <= JPEG_TOLERANCE, "channel {c}: want {} got {}", expected[c], got[c]);
        }
    }

    #[test]
    fn drop_if_opaque_writes_rgb8_only_when_every_alpha_is_255() {
        for format in [ImageSaveFormat::Png, ImageSaveFormat::WebpLossless] {
            let opaque = fixture(255);
            let bytes = encode_rgba(&opaque, &encoding(format, AlphaPolicy::DropIfOpaque)).expect("encode");
            let (decoded, _) = decode(&bytes);
            assert_eq!(decoded.color(), ColorType::Rgb8, "{format:?} opaque");
            assert_eq!(decoded.to_rgba8(), opaque, "{format:?} opaque pixels");

            let mut one_translucent = fixture(255);
            one_translucent.put_pixel(15, 15, Rgba([5, 6, 7, 254]));
            let bytes = encode_rgba(&one_translucent, &encoding(format, AlphaPolicy::DropIfOpaque)).expect("encode");
            assert_eq!(decode(&bytes).0.color(), ColorType::Rgba8, "{format:?} one translucent pixel");

            let bytes = encode_rgba(&opaque, &encoding(format, AlphaPolicy::KeepRgba)).expect("encode");
            assert_eq!(decode(&bytes).0.color(), ColorType::Rgba8, "{format:?} KeepRgba");
        }
    }

    #[test]
    fn png_icc_profile_round_trips() {
        // Opaque bytes: the encoder embeds the profile verbatim and never parses it.
        let profile: Vec<u8> = (0u8..=200).collect();
        let enc = ImageEncoding { icc_profile: Some(profile.clone()), ..encoding(ImageSaveFormat::Png, AlphaPolicy::KeepRgba) };
        let bytes = encode_rgba(&fixture(255), &enc).expect("encode");
        let mut decoder = image::codecs::png::PngDecoder::new(Cursor::new(&bytes)).expect("png header");
        assert_eq!(decoder.icc_profile().expect("iccp chunk reads"), Some(profile));
    }

    #[test]
    fn jpeg_quality_outside_range_is_rejected() {
        for quality in [0u8, 101] {
            let enc = ImageEncoding { jpeg_quality: quality, ..encoding(ImageSaveFormat::Jpeg, AlphaPolicy::KeepRgba) };
            assert!(matches!(encode_rgba(&fixture(255), &enc), Err(EncodeError::InvalidJpegQuality(q)) if q == quality));
        }
    }

    #[test]
    fn oversized_container_is_rejected_instead_of_panicking() {
        let raw = vec![0u8; 2 * 2 * 4 + 1];
        let img = ImageBuffer::<Rgba<u8>, &[u8]>::from_raw(2, 2, raw.as_slice()).expect("from_raw accepts a longer container");
        assert!(matches!(encode_rgba(&img, &encoding(ImageSaveFormat::Png, AlphaPolicy::KeepRgba)), Err(EncodeError::BufferLength { width: 2, height: 2, len: 17 })));
    }

    #[test]
    fn zero_sized_image_is_an_encoder_error() {
        let img = RgbaImage::new(0, 0);
        assert!(matches!(encode_rgba(&img, &encoding(ImageSaveFormat::Png, AlphaPolicy::KeepRgba)), Err(EncodeError::Encoder { format: ImageSaveFormat::Png, .. })));
    }

    #[test]
    fn extension_mapping() {
        assert_eq!(ImageSaveFormat::from_extension("PNG"), Some(ImageSaveFormat::Png));
        assert_eq!(ImageSaveFormat::from_extension("jpg"), Some(ImageSaveFormat::Jpeg));
        assert_eq!(ImageSaveFormat::from_extension("JPEG"), Some(ImageSaveFormat::Jpeg));
        assert_eq!(ImageSaveFormat::from_extension("jpe"), Some(ImageSaveFormat::Jpeg));
        assert_eq!(ImageSaveFormat::from_extension("webp"), Some(ImageSaveFormat::WebpLossless));
        for unsupported in ["", "gif", "bmp", "tiff", ".png", "png "] {
            assert_eq!(ImageSaveFormat::from_extension(unsupported), None, "{unsupported:?}");
        }
        for format in [ImageSaveFormat::Png, ImageSaveFormat::Jpeg, ImageSaveFormat::WebpLossless] {
            assert_eq!(ImageSaveFormat::from_extension(format.extension()), Some(format));
            assert!(format.extensions().any(|ext| ext == format.extension()), "{format:?}: the canonical extension is offered");
        }
        let all: Vec<&str> = ImageSaveFormat::save_formats().flat_map(ImageSaveFormat::extensions).collect();
        assert_eq!(all, ["png", "jpg", "jpeg", "jpe", "webp"]);
        assert!(all.iter().all(|ext| ImageSaveFormat::from_extension(ext).is_some() && *ext == ext.to_ascii_lowercase()));
    }

    fn session(source_format: ms_project::SourceImageFormat, animated: bool, extension_matches_content: bool) -> ms_project::SingleImageSession {
        ms_project::SingleImageSession {
            source_path: std::path::PathBuf::from("/home/u/pictures/page.img"),
            source_format,
            extension_matches_content,
            source_animated: animated,
            exif_orientation_applied: false,
            icc_profile: None,
            scratch_root: std::path::PathBuf::from("/home/u/scratch"),
        }
    }

    #[test]
    fn in_place_for_session_table() {
        use ms_project::SourceImageFormat as S;
        let cases = [
            (S::Png, false, true, Some(ImageSaveFormat::Png)),
            (S::Jpeg, false, true, Some(ImageSaveFormat::Jpeg)),
            (S::WebP, false, true, Some(ImageSaveFormat::WebpLossless)),
            (S::Png, true, true, None),
            (S::WebP, true, true, None),
            (S::Gif, false, true, None),
            (S::Bmp, false, true, None),
            (S::Tiff, false, true, None),
            (S::Tga, false, true, None),
            (S::Qoi, false, true, None),
            (S::Jpeg, false, false, None),
        ];
        for (format, animated, ext_ok, want) in cases {
            assert_eq!(ImageSaveFormat::in_place_for(&session(format, animated, ext_ok)), want, "{format:?} animated={animated} ext_ok={ext_ok}");
        }
    }

    #[test]
    fn in_place_format_table() {
        let cases = [
            (ImageFormat::Png, false, true, Some(ImageSaveFormat::Png)),
            (ImageFormat::Jpeg, false, true, Some(ImageSaveFormat::Jpeg)),
            (ImageFormat::WebP, false, true, Some(ImageSaveFormat::WebpLossless)),
            // APNG and animated WebP: writable formats, but one frame must not replace an animation.
            (ImageFormat::Png, true, true, None),
            (ImageFormat::WebP, true, true, None),
            (ImageFormat::Gif, false, true, None),
            (ImageFormat::Gif, true, true, None),
            (ImageFormat::Bmp, false, true, None),
            (ImageFormat::Tiff, false, true, None),
            (ImageFormat::Tga, false, true, None),
            (ImageFormat::Qoi, false, true, None),
            // A writable content format behind a mismatched extension.
            (ImageFormat::Png, false, false, None),
            (ImageFormat::Jpeg, false, false, None),
        ];
        for (content, animated, ext_ok, want) in cases {
            assert_eq!(in_place_format(content, animated, ext_ok), want, "{content:?} animated={animated} ext_ok={ext_ok}");
        }
    }
}
