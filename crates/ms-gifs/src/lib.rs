/*
File: crates/ms-gifs/src/lib.rs

Purpose:
Defines the GUI-free public API for embedded animated WebP hints and decodes
those assets into fully composited, non-premultiplied RGBA animation frames.

Main responsibilities:
- expose stable hint identities and embedded bytes;
- decode animated WebP data without allowing decoder precondition panics;
- normalize RGB decoder output to RGBA for consumers.
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]

use std::io::Cursor;
use std::time::Duration;

use image_webp::{DecodingError, WebPDecoder};

mod error;
pub mod typing;

pub use error::GifError;

/// A single embedded animated hint asset with a stable process-independent identity.
#[derive(Debug, Clone, Copy)]
pub struct Hint {
    name: &'static str,
    bytes: &'static [u8],
}

impl PartialEq for Hint {
    /// Compares stable hint identities without inspecting the embedded asset bytes.
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Hint {}

impl Hint {
    /// Creates a hint whose name remains its stable cache key across releases.
    pub(crate) const fn new(name: &'static str, bytes: &'static [u8]) -> Self {
        Self { name, bytes }
    }

    /// Returns the stable, globally unique cache key for this embedded hint.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// Returns the complete embedded WebP file without allocation or decoding.
    #[must_use]
    pub const fn bytes(self) -> &'static [u8] {
        self.bytes
    }
}

/// One fully composited animation frame in non-premultiplied RGBA8 format.
#[derive(Debug)]
pub struct Frame {
    /// Pixel data containing exactly `Animation::width * Animation::height * 4` bytes.
    pub rgba: Vec<u8>,
    /// Display duration reported by the WebP frame, with millisecond precision.
    pub delay: Duration,
}

/// A decoded animation whose dimensions apply to every frame and whose frame list is non-empty.
#[derive(Debug)]
pub struct Animation {
    /// Canvas width in pixels; always non-zero.
    pub width: u32,
    /// Canvas height in pixels; always non-zero.
    pub height: u32,
    /// Fully composited frames in playback order; never empty after successful decoding.
    pub frames: Vec<Frame>,
}

/// Decodes an embedded hint into fully composited, non-premultiplied RGBA8 frames.
///
/// This operation is expensive and may allocate tens of megabytes for roughly one
/// hundred frames. Callers must run it off the GUI thread. Invalid, static, empty,
/// or dimensionally inconsistent assets return [`GifError`] without panicking.
///
/// # Errors
/// Returns [`GifError::NotAnimated`] for a static WebP, [`GifError::Decoder`] for
/// parser or frame failures, [`GifError::UnusableLayout`] for unsafe dimensions or
/// buffer sizes, and [`GifError::NoFrames`] if no frame was produced.
pub fn decode(hint: Hint) -> Result<Animation, GifError> {
    let name = hint.name();
    let mut decoder = WebPDecoder::new(Cursor::new(hint.bytes()))
        .map_err(|source| GifError::Decoder { name, source })?;

    if !decoder.is_animated() {
        return Err(GifError::NotAnimated { name });
    }

    let (width, height) = decoder.dimensions();
    let pixel_count = usize::try_from(width)
        .ok()
        .and_then(|width| usize::try_from(height).ok().and_then(|height| width.checked_mul(height)))
        .filter(|count| *count > 0)
        .ok_or(GifError::UnusableLayout { name, width, height })?;
    let bytes_per_pixel = if decoder.has_alpha() { 4 } else { 3 };
    let expected_output_len = pixel_count
        .checked_mul(bytes_per_pixel)
        .ok_or(GifError::UnusableLayout { name, width, height })?;
    let output_len = decoder
        .output_buffer_size()
        .filter(|output_len| *output_len == expected_output_len)
        .ok_or(GifError::UnusableLayout { name, width, height })?;
    let rgba_len = pixel_count
        .checked_mul(4)
        .ok_or(GifError::UnusableLayout { name, width, height })?;
    let declared_frames = usize::try_from(decoder.num_frames())
        .map_err(|_| GifError::UnusableLayout { name, width, height })?;
    let mut frames = Vec::with_capacity(declared_frames);

    loop {
        let mut frame_buffer = vec![0; output_len];
        match decoder.read_frame(&mut frame_buffer) {
            Ok(delay_ms) => {
                let rgba = if bytes_per_pixel == 4 {
                    frame_buffer
                } else {
                    let mut rgba = Vec::with_capacity(rgba_len);
                    frame_buffer.chunks_exact(3).for_each(|rgb| {
                        rgba.extend_from_slice(rgb);
                        rgba.push(255);
                    });
                    rgba
                };
                frames.push(Frame {
                    rgba,
                    delay: Duration::from_millis(u64::from(delay_ms)),
                });
            }
            Err(DecodingError::NoMoreFrames) => break,
            Err(source) => return Err(GifError::Decoder { name, source }),
        }
    }

    if frames.is_empty() {
        return Err(GifError::NoFrames { name });
    }

    Ok(Animation { width, height, frames })
}

/// Returns every embedded hint in a stable order without decoding the assets.
#[must_use]
pub fn all() -> &'static [Hint] {
    &typing::ALL
}

#[cfg(test)]
mod tests {
    use image_webp::{ColorType, WebPEncoder};

    use super::*;

    /// Encodes a minimal static WebP and gives it test-lifetime static storage.
    fn static_webp() -> &'static [u8] {
        let mut bytes = Vec::new();
        WebPEncoder::new(&mut bytes)
            .encode(&[17, 34, 51], 1, 1, ColorType::Rgb8)
            .expect("the fixed 1x1 RGB fixture must encode");
        Box::leak(bytes.into_boxed_slice())
    }

    #[test]
    fn static_webp_is_rejected_without_panicking() {
        let error = decode(Hint::new("test.static", static_webp()))
            .expect_err("a static WebP must not be accepted as an animation");
        assert!(matches!(error, GifError::NotAnimated { .. }));
    }

    #[test]
    fn garbage_bytes_are_rejected_without_panicking() {
        let error = decode(Hint::new("test.garbage", b"not a WebP file"))
            .expect_err("garbage bytes must fail decoder construction");
        assert!(matches!(error, GifError::Decoder { .. }));
    }

    #[test]
    fn empty_bytes_are_rejected_without_panicking() {
        let error = decode(Hint::new("test.empty", b""))
            .expect_err("empty bytes must fail decoder construction");
        assert!(matches!(error, GifError::Decoder { .. }));
    }
}
