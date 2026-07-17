/*
File: crates/ms-gifs/src/error.rs

Purpose:
Defines typed failures for embedded animated WebP validation and decoding, with
the stable hint name retained in every variant for actionable diagnostics.
*/

use image_webp::DecodingError;

/// Failures produced while validating or decoding an embedded hint animation.
#[derive(Debug, thiserror::Error)]
pub enum GifError {
    /// The embedded file is valid WebP data but has no animation frames.
    #[error("hint \"{name}\" is not an animated WebP")]
    NotAnimated {
        /// Stable identity of the invalid hint.
        name: &'static str,
    },

    /// The WebP parser or frame decoder rejected the embedded data.
    #[error("failed to decode hint \"{name}\": {source}")]
    Decoder {
        /// Stable identity of the hint that failed decoding.
        name: &'static str,
        /// Underlying WebP decoding failure.
        #[source]
        source: DecodingError,
    },

    /// The decoder dimensions or output buffer size cannot represent RGBA frames safely.
    #[error("hint \"{name}\" reported an unusable frame layout ({width}x{height})")]
    UnusableLayout {
        /// Stable identity of the malformed hint.
        name: &'static str,
        /// Reported canvas width in pixels.
        width: u32,
        /// Reported canvas height in pixels.
        height: u32,
    },

    /// The animated decoder reached its terminator before yielding any frame.
    #[error("hint \"{name}\" decoded to zero frames")]
    NoFrames {
        /// Stable identity of the empty hint.
        name: &'static str,
    },

    /// The caller supplied a frame output buffer with the wrong byte length.
    #[error("hint \"{name}\" requires a {expected}-byte frame buffer, received {actual} bytes")]
    BufferLen {
        /// Stable identity of the hint being played.
        name: &'static str,
        /// Required non-premultiplied RGBA8 buffer length.
        expected: usize,
        /// Supplied buffer length.
        actual: usize,
    },
}
