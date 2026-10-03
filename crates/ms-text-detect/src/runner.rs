/*
File: crates/ms-text-detect/src/runner.rs

Purpose:
The runner seam: the trait through which a caller supplies the model forward pass (tiles in,
per-channel probability maps out), plus the map and error types it exchanges.

Key items:
- `ProbMap`: validated row-major `u8` probability map (`round(p * 255)`), re-exported at the
  crate root; produced by the native Paddle forward and the IPC runners.
- `TileMaps`: the channel maps of one tile, in the engine's channel order.
- `ProbMapRunner`: `max_batch` + `forward`.
- `RunnerError`: a forward failure with an already localized, user-facing message.

Notes:
Runners own threading, logging, cancellation and transport; this crate only validates what they
return (count, channel count, map size) in `pipeline`.
*/

use std::num::NonZeroUsize;

use image::RgbImage;

/// Why a [`ProbMap`] could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProbMapError {
    /// Width or height is zero.
    #[error("probability map has an empty size {width}x{height}")]
    Empty {
        /// Map width.
        width: u32,
        /// Map height.
        height: u32,
    },
    /// The buffer length is not `width * height` (overflow included).
    #[error("probability map {width}x{height} has {len} bytes")]
    LengthMismatch {
        /// Map width.
        width: u32,
        /// Map height.
        height: u32,
        /// Actual buffer length.
        len: usize,
    },
}

/// A probability map: `width * height` row-major bytes, each `round(clamp(p, 0, 1) * 255)`.
///
/// Built only through [`ProbMap::new`], so the size is non-zero and matches the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbMap {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl ProbMap {
    /// Wraps a row-major buffer (consumed, no copy).
    ///
    /// # Errors
    /// [`ProbMapError::Empty`] for a zero dimension, [`ProbMapError::LengthMismatch`] when
    /// `data.len() != width * height`.
    pub fn new(width: u32, height: u32, data: Vec<u8>) -> Result<Self, ProbMapError> {
        if width == 0 || height == 0 {
            return Err(ProbMapError::Empty { width, height });
        }
        let expected = usize::try_from(width).ok().and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)));
        if expected != Some(data.len()) {
            return Err(ProbMapError::LengthMismatch { width, height, len: data.len() });
        }
        Ok(Self { width, height, data })
    }

    /// Map width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Map height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// `[width, height]`.
    #[must_use]
    pub fn size(&self) -> [u32; 2] {
        [self.width, self.height]
    }

    /// The row-major bytes.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Consumes the map, returning its bytes.
    #[must_use]
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

/// The probability maps a runner returns for ONE tile, one per channel in the engine's order
/// (CTD: `[seg, shrink]`; Paddle, Surya: one map). Each map is `DetectionPlan::map_size()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileMaps {
    /// Channel maps.
    pub maps: Vec<ProbMap>,
}

/// A forward-pass failure. `message` is user-facing and already localized by the runner (the
/// runner also logs the technical detail); this crate passes it through unchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct RunnerError {
    /// Localized, user-facing text.
    pub message: String,
}

/// The model forward pass of one engine, supplied by the caller (native ONNX or the Python
/// backend over IPC).
pub trait ProbMapRunner {
    /// Largest number of tiles of size `tile_input` (`[w, h]`) one [`forward`](Self::forward) call
    /// may receive (transport frame budget, device memory).
    fn max_batch(&self, tile_input: [u32; 2]) -> NonZeroUsize;

    /// Runs the model on `tiles` (all the same size, a multiple of the engine alignment, padded
    /// with the plan's colour) and returns one [`TileMaps`] per tile, in order.
    ///
    /// # Errors
    /// A [`RunnerError`] with a user-facing message when the forward pass fails.
    fn forward(&mut self, tiles: &[RgbImage]) -> Result<Vec<TileMaps>, RunnerError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prob_map_validates_size_and_length() {
        assert_eq!(ProbMap::new(0, 3, Vec::new()), Err(ProbMapError::Empty { width: 0, height: 3 }));
        assert_eq!(ProbMap::new(2, 3, vec![0; 5]), Err(ProbMapError::LengthMismatch { width: 2, height: 3, len: 5 }));
        assert_eq!(ProbMap::new(u32::MAX, u32::MAX, vec![0; 1]), Err(ProbMapError::LengthMismatch { width: u32::MAX, height: u32::MAX, len: 1 }));
        let map = ProbMap::new(2, 3, vec![7; 6]).unwrap_or_else(|err| panic!("{err}"));
        assert_eq!((map.width(), map.height(), map.size()), (2, 3, [2, 3]));
        assert_eq!(map.data(), &[7; 6]);
        assert_eq!(map.into_data(), vec![7; 6]);
    }
}
