/*
File: crates/ms-backend-ipc/src/textdetector.rs

Purpose:
Rust side of the forward-only text-detector wire contract
(`textdetector.{ctd,paddle,surya}.forward`, `modules/ai_backend/ipc/PROTOCOL.md` §5.3):
the per-engine facts, the request builder and the response decoder. Pure functions over
bytes and `serde_json::Value`; no I/O, no image types, no detection logic.

Key structures:
- ForwardEngine     : the three engines and their wire facts (method, align, stride, channels).
- ForwardMaps       : a validated response — `n` tiles x channels of `u8` maps.
- ForwardWireError  : every way a request or response breaks the contract.

Key functions:
- build_forward_request()   : equal-size RGB tiles -> (header fields, blob).
- decode_forward_response() : response header + blob -> ForwardMaps.
- max_tiles_per_request()   : how many tiles of one size fit a share of one frame in both
                              directions (the only owner of that rule).

Notes:
The Python mirror of the engine table is `FORWARD_SPECS` in
`modules/ai_backend/ipc/handlers/textdetector.py`; both sides validate. Callers
(`ms-tab-translation`) wrap the decoded bytes into their own probability-map type.
*/

use serde_json::{Value, json};

use crate::protocol::{
    MAX_BLOB_BYTES, METHOD_TEXTDETECTOR_CTD_FORWARD, METHOD_TEXTDETECTOR_PADDLE_FORWARD,
    METHOD_TEXTDETECTOR_SURYA_FORWARD,
};

/// Bytes per input pixel: the request carries RGB u8.
const RGB_BYTES: usize = 3;

/// One forward-only detector engine served by the Python backend.
///
/// Every wire fact of an engine is a method here, so the request builder, the response
/// decoder and any batch sizing read the same table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ForwardEngine {
    /// ComicTextDetector (Torch): maps `[seg, shrink]` at the tile resolution.
    Ctd,
    /// PP-OCR detection (ONNX): one DB probability map at the tile resolution.
    Paddle,
    /// Surya segformer (Torch): one text heatmap at a quarter of the tile resolution.
    Surya,
}

impl ForwardEngine {
    /// The IPC method name (`protocol::METHOD_TEXTDETECTOR_*_FORWARD`).
    #[must_use]
    pub const fn method(self) -> &'static str {
        match self {
            Self::Ctd => METHOD_TEXTDETECTOR_CTD_FORWARD,
            Self::Paddle => METHOD_TEXTDETECTOR_PADDLE_FORWARD,
            Self::Surya => METHOD_TEXTDETECTOR_SURYA_FORWARD,
        }
    }

    /// The `engine` string the response header carries.
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Ctd => "ctd",
            Self::Paddle => "paddle",
            Self::Surya => "surya",
        }
    }

    /// Both tile sides must be a positive multiple of this (the network's input stride).
    #[must_use]
    pub const fn align(self) -> u32 {
        match self {
            Self::Ctd => 64,
            Self::Paddle => 32,
            Self::Surya => 4,
        }
    }

    /// Each map side is the tile side divided by this.
    #[must_use]
    pub const fn map_stride(self) -> u32 {
        match self {
            Self::Ctd | Self::Paddle => 1,
            Self::Surya => 4,
        }
    }

    /// Map names in blob order; one map per name per tile.
    #[must_use]
    pub const fn channel_names(self) -> &'static [&'static str] {
        match self {
            Self::Ctd => &["seg", "shrink"],
            Self::Paddle => &["prob"],
            Self::Surya => &["text"],
        }
    }

    /// Number of maps per tile (`channel_names().len()`).
    #[must_use]
    pub const fn channel_count(self) -> usize {
        self.channel_names().len()
    }
}

/// A request or response that breaks the forward wire contract.
///
/// Messages are technical (logs, developer diagnostics); a caller that shows the failure to
/// the user wraps it in a localized string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ForwardWireError {
    /// A request needs at least one tile.
    #[error("{engine}: a forward request needs at least one tile")]
    NoTiles {
        /// Engine wire name.
        engine: &'static str,
    },
    /// A tile side is zero or not a multiple of the engine alignment.
    #[error("{engine}: tile size {width}x{height} must be a positive multiple of {align}")]
    Misaligned {
        /// Engine wire name.
        engine: &'static str,
        /// Tile width in pixels.
        width: u32,
        /// Tile height in pixels.
        height: u32,
        /// Required multiple.
        align: u32,
    },
    /// A tile buffer does not hold exactly `width * height * 3` bytes.
    #[error("{engine}: tile {index} has {got} bytes, expected {expected} (RGB u8)")]
    TileLength {
        /// Engine wire name.
        engine: &'static str,
        /// Index of the offending tile.
        index: usize,
        /// Required byte length.
        expected: usize,
        /// Actual byte length.
        got: usize,
    },
    /// A byte count overflowed `usize`.
    #[error("{engine}: byte count overflows for {n} tiles of {width}x{height}")]
    SizeOverflow {
        /// Engine wire name.
        engine: &'static str,
        /// Tile count.
        n: usize,
        /// Tile width in pixels.
        width: u32,
        /// Tile height in pixels.
        height: u32,
    },
    /// The request or the response it implies exceeds `MAX_BLOB_BYTES`.
    #[error("{engine}: the {direction} blob would be {bytes} bytes, over MAX_BLOB_BYTES {max}")]
    BlobTooLarge {
        /// Engine wire name.
        engine: &'static str,
        /// `"request"` or `"response"`.
        direction: &'static str,
        /// Required blob size.
        bytes: usize,
        /// The frame limit.
        max: usize,
    },
    /// A response header field is missing, mistyped or disagrees with the request.
    #[error("{engine}: response field '{field}' is {got}, expected {expected}")]
    HeaderField {
        /// Engine wire name.
        engine: &'static str,
        /// Header key.
        field: &'static str,
        /// What the request implies.
        expected: String,
        /// What arrived (JSON text, or `missing`).
        got: String,
    },
    /// The response blob length does not match the header.
    #[error("{engine}: response blob is {got} bytes, expected {expected}")]
    ResponseLength {
        /// Engine wire name.
        engine: &'static str,
        /// `n * channels * map_height * map_width`.
        expected: usize,
        /// Actual blob length.
        got: usize,
    },
}

/// Byte sizes of one tile in both directions, after checking the tile size.
///
/// Returns `(request_bytes, response_bytes)` for ONE tile of `width x height`.
///
/// # Errors
/// [`ForwardWireError::Misaligned`] for a zero or misaligned side,
/// [`ForwardWireError::SizeOverflow`] when a product overflows `usize`.
pub fn tile_bytes(engine: ForwardEngine, width: u32, height: u32) -> Result<(usize, usize), ForwardWireError> {
    let align = engine.align();
    if width == 0 || height == 0 || !width.is_multiple_of(align) || !height.is_multiple_of(align) {
        return Err(ForwardWireError::Misaligned { engine: engine.wire_name(), width, height, align });
    }
    let overflow = || ForwardWireError::SizeOverflow { engine: engine.wire_name(), n: 1, width, height };
    let w = usize::try_from(width).map_err(|_| overflow())?;
    let h = usize::try_from(height).map_err(|_| overflow())?;
    let stride = usize::try_from(engine.map_stride()).map_err(|_| overflow())?;
    let request = w.checked_mul(h).and_then(|p| p.checked_mul(RGB_BYTES)).ok_or_else(overflow)?;
    // `align` is a multiple of `map_stride` for every engine, so the division is exact.
    let response = (w / stride).checked_mul(h / stride).and_then(|p| p.checked_mul(engine.channel_count())).ok_or_else(overflow)?;
    Ok((request, response))
}

/// The largest tile count of one size whose request AND response both fit
/// `budget_percent` % of `MAX_BLOB_BYTES` — the one owner of the "tiles per frame" rule.
///
/// `budget_percent` is the share of the frame limit a batch may use: 100 fills the frame
/// exactly, a smaller value keeps a safety margin below it. Values above 100 are treated as
/// 100, because `MAX_BLOB_BYTES` is the hard ceiling the request builder enforces anyway.
/// Returns 0 when not even one tile fits the budget (the caller must use smaller tiles or
/// send one tile and let [`build_forward_request`] report the precise error).
///
/// # Errors
/// As [`tile_bytes`].
pub fn max_tiles_per_request(engine: ForwardEngine, width: u32, height: u32, budget_percent: u8) -> Result<usize, ForwardWireError> {
    let (request, response) = tile_bytes(engine, width, height)?;
    // Divide first: `MAX_BLOB_BYTES / 100 * p` cannot overflow and loses < 100 bytes.
    let budget = MAX_BLOB_BYTES / 100 * usize::from(budget_percent.min(100));
    // `request` is non-zero after the size check, so the division is defined.
    Ok(budget / request.max(response))
}

/// Builds the request header fields and blob for `n = tiles.len()` RGB tiles.
///
/// Every tile must be `width x height` RGB u8, row-major (`width * height * 3` bytes). The
/// blob is the tiles concatenated in order (tile-major); the returned header fields are
/// `{n, width, height}`, to be merged by `protocol::request_header` / `BackendClient::call`
/// with `engine.method()`.
///
/// # Errors
/// [`ForwardWireError::NoTiles`], [`ForwardWireError::Misaligned`],
/// [`ForwardWireError::TileLength`], [`ForwardWireError::SizeOverflow`], and
/// [`ForwardWireError::BlobTooLarge`] when the request or the response it implies exceeds
/// `MAX_BLOB_BYTES`.
pub fn build_forward_request(engine: ForwardEngine, width: u32, height: u32, tiles: &[&[u8]]) -> Result<(Value, Vec<u8>), ForwardWireError> {
    let n = tiles.len();
    if n == 0 {
        return Err(ForwardWireError::NoTiles { engine: engine.wire_name() });
    }
    let (tile_request, tile_response) = tile_bytes(engine, width, height)?;
    let overflow = || ForwardWireError::SizeOverflow { engine: engine.wire_name(), n, width, height };
    let request_bytes = tile_request.checked_mul(n).ok_or_else(overflow)?;
    let response_bytes = tile_response.checked_mul(n).ok_or_else(overflow)?;
    for (direction, bytes) in [("request", request_bytes), ("response", response_bytes)] {
        if bytes > MAX_BLOB_BYTES {
            return Err(ForwardWireError::BlobTooLarge { engine: engine.wire_name(), direction, bytes, max: MAX_BLOB_BYTES });
        }
    }
    if let Some((index, tile)) = tiles.iter().enumerate().find(|(_, tile)| tile.len() != tile_request) {
        return Err(ForwardWireError::TileLength { engine: engine.wire_name(), index, expected: tile_request, got: tile.len() });
    }
    let mut blob = Vec::with_capacity(request_bytes);
    for tile in tiles {
        blob.extend_from_slice(tile);
    }
    Ok((json!({ "n": n, "width": width, "height": height }), blob))
}

/// A validated forward response: `n` tiles, each with `channel_count()` maps of
/// `map_width x map_height` u8 values (`round(clip(p, 0, 1) * 255)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardMaps {
    engine: ForwardEngine,
    n: usize,
    map_width: u32,
    map_height: u32,
    data: Vec<u8>,
}

impl ForwardMaps {
    /// The engine that produced the maps.
    #[must_use]
    pub const fn engine(&self) -> ForwardEngine {
        self.engine
    }

    /// Number of tiles.
    #[must_use]
    pub const fn tile_count(&self) -> usize {
        self.n
    }

    /// Map size `[width, height]` in map pixels (the tile size divided by `map_stride`).
    #[must_use]
    pub const fn map_size(&self) -> [u32; 2] {
        [self.map_width, self.map_height]
    }

    /// The row-major map of `channel` for `tile`, or `None` when either index is out of range.
    #[must_use]
    pub fn map(&self, tile: usize, channel: usize) -> Option<&[u8]> {
        let channels = self.engine.channel_count();
        if tile >= self.n || channel >= channels {
            return None;
        }
        // In range by construction: the decoder checked `data.len() == n * channels * plane`.
        let plane = self.data.len() / (self.n * channels);
        let start = (tile * channels + channel) * plane;
        self.data.get(start..start + plane)
    }

    /// The whole blob: tile-major, then channel-major, then row-major.
    #[must_use]
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

/// Validates a forward response against its request and returns the maps.
///
/// `n`, `width` and `height` are the values the request was built with. The header must
/// carry `engine == engine.wire_name()`, the same `n`, `map_width`/`map_height` equal to the
/// tile size divided by `map_stride`, and `channels` equal to `engine.channel_names()`; the
/// blob must be exactly `n * channels * map_height * map_width` bytes.
///
/// # Errors
/// [`ForwardWireError::Misaligned`] / [`ForwardWireError::SizeOverflow`] for request values
/// that could never have been sent, [`ForwardWireError::HeaderField`] for a header mismatch,
/// [`ForwardWireError::ResponseLength`] for a blob of the wrong size.
pub fn decode_forward_response(engine: ForwardEngine, n: usize, width: u32, height: u32, header: &Value, blob: Vec<u8>) -> Result<ForwardMaps, ForwardWireError> {
    let (_, tile_response) = tile_bytes(engine, width, height)?;
    let expected_len = tile_response.checked_mul(n).ok_or(ForwardWireError::SizeOverflow { engine: engine.wire_name(), n, width, height })?;
    let map_width = width / engine.map_stride();
    let map_height = height / engine.map_stride();
    let channels: Vec<Value> = engine.channel_names().iter().map(|name| json!(name)).collect();
    let expectations: [(&'static str, Value); 5] = [
        ("engine", json!(engine.wire_name())),
        ("n", json!(n)),
        ("map_width", json!(map_width)),
        ("map_height", json!(map_height)),
        ("channels", Value::Array(channels)),
    ];
    for (field, expected) in expectations {
        let got = header.get(field);
        if got != Some(&expected) {
            return Err(ForwardWireError::HeaderField {
                engine: engine.wire_name(),
                field,
                expected: expected.to_string(),
                got: got.map_or_else(|| "missing".to_string(), Value::to_string),
            });
        }
    }
    if blob.len() != expected_len {
        return Err(ForwardWireError::ResponseLength { engine: engine.wire_name(), expected: expected_len, got: blob.len() });
    }
    Ok(ForwardMaps { engine, n, map_width, map_height, data: blob })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [ForwardEngine; 3] = [ForwardEngine::Ctd, ForwardEngine::Paddle, ForwardEngine::Surya];

    /// A response header exactly as the Python handler emits it.
    fn response_header(engine: ForwardEngine, n: usize, width: u32, height: u32) -> Value {
        json!({
            "engine": engine.wire_name(),
            "n": n,
            "map_width": width / engine.map_stride(),
            "map_height": height / engine.map_stride(),
            "channels": engine.channel_names(),
        })
    }

    #[test]
    fn engine_table_matches_the_protocol() {
        assert_eq!(ForwardEngine::Ctd.method(), "textdetector.ctd.forward");
        assert_eq!(ForwardEngine::Paddle.method(), "textdetector.paddle.forward");
        assert_eq!(ForwardEngine::Surya.method(), "textdetector.surya.forward");
        assert_eq!([64, 32, 4], ALL.map(ForwardEngine::align));
        assert_eq!([1, 1, 4], ALL.map(ForwardEngine::map_stride));
        assert_eq!(ForwardEngine::Ctd.channel_names(), ["seg", "shrink"]);
        assert_eq!(ForwardEngine::Paddle.channel_names(), ["prob"]);
        assert_eq!(ForwardEngine::Surya.channel_names(), ["text"]);
        for engine in ALL {
            assert!(engine.align().is_multiple_of(engine.map_stride()), "{engine:?}: align must be a multiple of the stride");
        }
    }

    #[test]
    fn request_concatenates_tiles_in_order() -> Result<(), ForwardWireError> {
        let a = vec![1_u8; 64 * 64 * 3];
        let b = vec![2_u8; 64 * 64 * 3];
        let (header, blob) = build_forward_request(ForwardEngine::Ctd, 64, 64, &[&a, &b])?;
        assert_eq!(header, json!({ "n": 2, "width": 64, "height": 64 }));
        assert_eq!(blob.len(), 2 * 64 * 64 * 3);
        assert!(blob[..a.len()].iter().all(|&v| v == 1));
        assert!(blob[a.len()..].iter().all(|&v| v == 2));
        Ok(())
    }

    #[test]
    fn request_validation_errors() {
        let tile = vec![0_u8; 32 * 32 * 3];
        assert_eq!(build_forward_request(ForwardEngine::Paddle, 32, 32, &[]), Err(ForwardWireError::NoTiles { engine: "paddle" }));
        assert!(matches!(build_forward_request(ForwardEngine::Ctd, 32, 32, &[&tile]), Err(ForwardWireError::Misaligned { align: 64, .. })));
        assert!(matches!(build_forward_request(ForwardEngine::Surya, 0, 32, &[&tile]), Err(ForwardWireError::Misaligned { .. })));
        assert_eq!(
            build_forward_request(ForwardEngine::Paddle, 32, 32, &[&tile, &tile[1..]]),
            Err(ForwardWireError::TileLength { engine: "paddle", index: 1, expected: 32 * 32 * 3, got: 32 * 32 * 3 - 1 })
        );
    }

    #[test]
    fn request_over_the_frame_limit_is_refused_before_copying() {
        // 2048x2048 RGB is 12 MiB per tile: three tiles exceed 32 MiB. Length checks come
        // after the size check, so empty slices suffice to prove nothing is copied.
        let empty: &[u8] = &[];
        let result = build_forward_request(ForwardEngine::Ctd, 2048, 2048, &[empty, empty, empty]);
        assert!(matches!(result, Err(ForwardWireError::BlobTooLarge { direction: "request", .. })), "{result:?}");
    }

    #[test]
    fn max_tiles_respects_both_directions() -> Result<(), ForwardWireError> {
        // CTD 1280x1280: request 4.69 MiB, response 3.13 MiB per tile -> 6 tiles.
        assert_eq!(max_tiles_per_request(ForwardEngine::Ctd, 1280, 1280, 100)?, 6);
        // Surya 1200x1200: request 4.12 MiB per tile -> 7 tiles (response is 1/48 of it).
        assert_eq!(max_tiles_per_request(ForwardEngine::Surya, 1200, 1200, 100)?, 7);
        for engine in ALL {
            let side = 1024;
            let n = max_tiles_per_request(engine, side, side, 100)?;
            let tile = vec![0_u8; 1024 * 1024 * 3];
            let tiles: Vec<&[u8]> = (0..n).map(|_| tile.as_slice()).collect();
            assert!(build_forward_request(engine, side, side, &tiles).is_ok(), "{engine:?}: {n} tiles must fit");
            let mut over = tiles.clone();
            over.push(&tile);
            assert!(matches!(build_forward_request(engine, side, side, &over), Err(ForwardWireError::BlobTooLarge { .. })), "{engine:?}");
        }
        Ok(())
    }

    #[test]
    fn max_tiles_budget_percent_keeps_a_margin() -> Result<(), ForwardWireError> {
        // Surya 1200x1200 (4.12 MiB request): 7 fill the whole frame, a 90 % budget leaves 6.
        assert_eq!(max_tiles_per_request(ForwardEngine::Surya, 1200, 1200, 90)?, 6);
        // CTD 2048x2048 (12 MiB request): 2 fit at both 100 % and 90 %.
        assert_eq!(max_tiles_per_request(ForwardEngine::Ctd, 2048, 2048, 90)?, 2);
        // The percent is capped at the frame limit itself.
        assert_eq!(max_tiles_per_request(ForwardEngine::Surya, 1200, 1200, 255)?, 7);
        // A zero budget fits nothing; a tile over the budget yields 0, not an error.
        assert_eq!(max_tiles_per_request(ForwardEngine::Paddle, 960, 960, 0)?, 0);
        assert_eq!(max_tiles_per_request(ForwardEngine::Ctd, 4096, 4096, 90)?, 0);
        // A misaligned tile is still refused before any budget arithmetic.
        assert!(max_tiles_per_request(ForwardEngine::Ctd, 100, 64, 90).is_err());
        Ok(())
    }

    #[test]
    fn response_round_trip_per_engine() -> Result<(), ForwardWireError> {
        for engine in ALL {
            let (n, width, height) = (2, 128, 64);
            let channels = engine.channel_count();
            let (_, tile_response) = tile_bytes(engine, width, height)?;
            let plane = tile_response / channels;
            // Value encodes (tile, channel) so the layout is checkable.
            let blob: Vec<u8> = (0..n * channels).flat_map(|i| std::iter::repeat_n(u8::try_from(i).unwrap_or(u8::MAX), plane)).collect();
            let maps = decode_forward_response(engine, n, width, height, &response_header(engine, n, width, height), blob)?;
            assert_eq!(maps.engine(), engine);
            assert_eq!(maps.tile_count(), n);
            assert_eq!(maps.map_size(), [width / engine.map_stride(), height / engine.map_stride()]);
            for tile in 0..n {
                for channel in 0..channels {
                    let Some(map) = maps.map(tile, channel) else { panic!("{engine:?}: map ({tile}, {channel}) missing") };
                    assert_eq!(map.len(), plane);
                    let value = u8::try_from(tile * channels + channel).unwrap_or(u8::MAX);
                    assert!(map.iter().all(|&v| v == value), "{engine:?} tile {tile} channel {channel}");
                }
            }
            assert!(maps.map(n, 0).is_none());
            assert!(maps.map(0, channels).is_none());
        }
        Ok(())
    }

    #[test]
    fn response_header_mismatches_are_rejected() {
        let engine = ForwardEngine::Surya;
        let blob = || vec![0_u8; 16 * 16];
        let good = response_header(engine, 1, 64, 64);
        for (field, bad) in [
            ("engine", json!("ctd")),
            ("n", json!(2)),
            ("map_width", json!(64)),
            ("map_height", json!("16")),
            ("channels", json!(["prob"])),
        ] {
            let mut header = good.clone();
            header[field] = bad;
            let result = decode_forward_response(engine, 1, 64, 64, &header, blob());
            assert!(matches!(result, Err(ForwardWireError::HeaderField { field: f, .. }) if f == field), "{field}: {result:?}");
        }
        let mut missing = good.clone();
        if let Some(obj) = missing.as_object_mut() {
            obj.remove("channels");
        }
        assert!(matches!(decode_forward_response(engine, 1, 64, 64, &missing, blob()), Err(ForwardWireError::HeaderField { field: "channels", .. })));
    }

    #[test]
    fn response_blob_length_is_exact() {
        let engine = ForwardEngine::Ctd;
        let header = response_header(engine, 1, 64, 64);
        let result = decode_forward_response(engine, 1, 64, 64, &header, vec![0; 2 * 64 * 64 - 1]);
        assert_eq!(result, Err(ForwardWireError::ResponseLength { engine: "ctd", expected: 2 * 64 * 64, got: 2 * 64 * 64 - 1 }));
    }
}
