/*
File: crates/ms-gifs/src/lib.rs

Purpose:
Defines the GUI-free public API for embedded animated WebP hints and streams
fully composited, non-premultiplied RGBA frames with frame-count-independent memory.

Main responsibilities:
- expose stable hint identities and embedded bytes;
- validate decoder preconditions before frame playback;
- provide sequential looping playback with RGB-to-RGBA normalization.
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

/// Sequential, looping decoder for one embedded hint.
///
/// Holds a single compositing canvas, so its memory does NOT grow with the
/// animation's frame count.
pub struct Player {
    hint: Hint,
    decoder: WebPDecoder<Cursor<&'static [u8]>>,
    width: u32,
    height: u32,
    frame_buffer_len: usize,
    frame_count: u32,
    decoder_buffer_len: usize,
    rgb_scratch: Vec<u8>,
}

impl Player {
    /// Opens `hint` for playback. Decodes no frame yet (cheap).
    ///
    /// # Errors
    /// Returns [`GifError::NotAnimated`] for a static WebP, [`GifError::Decoder`]
    /// for parser failures, [`GifError::UnusableLayout`] for unsafe dimensions or
    /// buffer sizes, and [`GifError::NoFrames`] when no animation frame is declared.
    pub fn open(hint: Hint) -> Result<Player, GifError> {
        let name = hint.name();
        let decoder = WebPDecoder::new(Cursor::new(hint.bytes()))
            .map_err(|source| GifError::Decoder { name, source })?;

        if !decoder.is_animated() {
            return Err(GifError::NotAnimated { name });
        }

        let (width, height) = decoder.dimensions();
        let pixel_count = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .filter(|count| *count > 0)
            .ok_or(GifError::UnusableLayout {
                name,
                width,
                height,
            })?;
        let bytes_per_pixel = if decoder.has_alpha() { 4 } else { 3 };
        let expected_decoder_len = pixel_count.checked_mul(bytes_per_pixel).ok_or(
            GifError::UnusableLayout {
                name,
                width,
                height,
            },
        )?;
        let decoder_buffer_len = decoder
            .output_buffer_size()
            .filter(|length| *length == expected_decoder_len)
            .ok_or(GifError::UnusableLayout {
                name,
                width,
                height,
            })?;
        let frame_buffer_len = pixel_count.checked_mul(4).ok_or(GifError::UnusableLayout {
            name,
            width,
            height,
        })?;
        let frame_count = decoder.num_frames();
        if frame_count == 0 {
            return Err(GifError::NoFrames { name });
        }
        let rgb_scratch = if decoder.has_alpha() {
            Vec::new()
        } else {
            vec![0; decoder_buffer_len]
        };

        Ok(Player {
            hint,
            decoder,
            width,
            height,
            frame_buffer_len,
            frame_count,
            decoder_buffer_len,
            rgb_scratch,
        })
    }

    /// The hint this player plays.
    #[must_use]
    pub fn hint(&self) -> Hint {
        self.hint
    }

    /// Canvas width in pixels; identical for every frame. Always non-zero.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Canvas height in pixels; identical for every frame. Always non-zero.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Exact required length of the `out` buffer for [`Player::next_frame`]:
    /// `width * height * 4`.
    #[must_use]
    pub fn frame_buffer_len(&self) -> usize {
        self.frame_buffer_len
    }

    /// Number of frames in one loop. Always at least one.
    #[must_use]
    pub fn frame_count(&self) -> u32 {
        self.frame_count
    }

    /// Decodes the next frame into `out` as non-premultiplied RGBA8 and returns
    /// that frame's display duration. Loops automatically after the last frame.
    ///
    /// `out.len()` must equal [`Player::frame_buffer_len`].
    ///
    /// # Errors
    /// Returns [`GifError::BufferLen`] for a wrongly sized output buffer,
    /// [`GifError::Decoder`] for frame decoding failures, and [`GifError::NoFrames`]
    /// if the decoder remains exhausted immediately after being reset.
    pub fn next_frame(&mut self, out: &mut [u8]) -> Result<Duration, GifError> {
        if out.len() != self.frame_buffer_len {
            return Err(GifError::BufferLen {
                name: self.hint.name(),
                expected: self.frame_buffer_len,
                actual: out.len(),
            });
        }

        let first_attempt = if self.decoder_buffer_len == self.frame_buffer_len {
            self.decoder.read_frame(out)
        } else {
            self.decoder.read_frame(&mut self.rgb_scratch)
        };
        let delay_ms = match first_attempt {
            Ok(delay_ms) => delay_ms,
            Err(DecodingError::NoMoreFrames) => {
                self.decoder.reset_animation();
                let retry = if self.decoder_buffer_len == self.frame_buffer_len {
                    self.decoder.read_frame(out)
                } else {
                    self.decoder.read_frame(&mut self.rgb_scratch)
                };
                match retry {
                    Ok(delay_ms) => delay_ms,
                    Err(DecodingError::NoMoreFrames) => {
                        return Err(GifError::NoFrames {
                            name: self.hint.name(),
                        });
                    }
                    Err(source) => {
                        return Err(GifError::Decoder {
                            name: self.hint.name(),
                            source,
                        });
                    }
                }
            }
            Err(source) => {
                return Err(GifError::Decoder {
                    name: self.hint.name(),
                    source,
                });
            }
        };

        if self.decoder_buffer_len != self.frame_buffer_len {
            for (rgb, rgba) in self.rgb_scratch.chunks_exact(3).zip(out.chunks_exact_mut(4)) {
                rgba[..3].copy_from_slice(rgb);
                rgba[3] = 255;
            }
        }

        Ok(Duration::from_millis(u64::from(delay_ms)))
    }
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
        let error = Player::open(Hint::new("test.static", static_webp()))
            .err()
            .expect("a static WebP must not be accepted as an animation");
        assert!(matches!(error, GifError::NotAnimated { .. }));
    }

    #[test]
    fn garbage_bytes_are_rejected_without_panicking() {
        let error = Player::open(Hint::new("test.garbage", b"not a WebP file"))
            .err()
            .expect("garbage bytes must fail decoder construction");
        assert!(matches!(error, GifError::Decoder { .. }));
    }

    #[test]
    fn empty_bytes_are_rejected_without_panicking() {
        let error = Player::open(Hint::new("test.empty", b""))
            .err()
            .expect("empty bytes must fail decoder construction");
        assert!(matches!(error, GifError::Decoder { .. }));
    }

    #[test]
    fn wrong_frame_buffer_length_is_rejected_without_panicking() {
        let mut player = Player::open(typing::KERNING)
            .expect("the embedded animation must open for buffer validation");
        let mut buffer = vec![0; player.frame_buffer_len() - 1];
        let error = player
            .next_frame(&mut buffer)
            .expect_err("a wrongly sized output buffer must fail");
        assert!(matches!(
            error,
            GifError::BufferLen {
                expected,
                actual,
                ..
            } if expected == player.frame_buffer_len() && actual == buffer.len()
        ));
    }
}
