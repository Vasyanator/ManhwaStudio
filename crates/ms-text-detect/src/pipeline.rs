/*
File: crates/ms-text-detect/src/pipeline.rs

Purpose:
The detection pipeline: resize the page once per the plan, cut tiles, batch them through a
runner, validate and stitch the maps, run the engine postprocess once and finalize the blocks.

Key items:
- `run_detection()`: plan + page + runner -> `Detection` (blocks in source px, 0/255 mask).
- `forward_and_stitch()`: the same up to the stitched scaled-space maps (postprocess parity
  tests and diagnostics use it).
- `Detection`, `DetectionStats`, `DetectError`.

Notes:
The engine postprocess is chosen ONCE, before any forward pass, by `postprocess_for`: Paddle uses
`db::boxes_from_bitmap` on the stitched u8 map plus `glyph_mask` on the full-resolution page.
CTD uses `ctd::postprocess` on the stitched `[seg, shrink]` maps and the source page; Surya uses
`surya::postprocess` on the stitched heatmap. Classic never runs through a runner and returns
`DetectError::Unsupported` before the runner is called (never a fallback).

Memory bound (one page): the source page (3 B/px, caller-owned) + the scaled page (3 B/scaled px,
absent at scale 1) + one batch of tile inputs and outputs + all tile maps of the page (about
`C * scaled_px * overlap factor` bytes) + the stitched maps (`C` B/scaled px) + the source-size
glyph mask and binary mask (2 B/px). An 800x12000 CTD page at scale 1 needs roughly 125 MB; pages
above `MAX_MASK_PIXELS` are refused by the plan.
*/

use std::borrow::Cow;

use image::imageops::{self, FilterType};
use image::{GrayImage, Rgb, RgbImage};

use crate::blocks::{DetectRect, finalize_blocks};
use crate::db::{block_from_quad, boxes_from_bitmap};
use crate::glyph_mask::build_glyph_mask;
use crate::mask::{BinaryMask, MaskError, binary_alpha_from_gray};
use crate::num::idx;
use crate::plan::{DetectionPlan, EngineKind, PlanError, ResizeFilter, TileRect};
use crate::runner::{ProbMap, ProbMapError, ProbMapRunner, RunnerError};
use crate::stitch::stitch_tiles;

/// Counters of one detection run (for the caller's log).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetectionStats {
    /// Tiles sent to the runner.
    pub tiles: u32,
    /// `forward` calls made.
    pub batches: u32,
}

/// The result of one page detection.
#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    /// Source page size `[w, h]`.
    pub source_size: [u32; 2],
    /// Blocks in source pixels, finalized (reading order, capped).
    pub blocks: Vec<DetectRect>,
    /// Source-size 0/255 text mask.
    pub mask: BinaryMask,
    /// Run counters.
    pub stats: DetectionStats,
}

/// Why a detection run failed. Every variant except `Runner` is technical (English); callers map
/// them onto their own localized messages. `Runner` carries the runner's localized text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DetectError {
    /// No plan could be made for the page.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// The forward pass failed.
    #[error(transparent)]
    Runner(#[from] RunnerError),
    /// The engine's postprocess is not available in this build of the pipeline.
    #[error("text detection postprocess for {engine:?} is not available")]
    Unsupported {
        /// The engine that was requested.
        engine: EngineKind,
    },
    /// The page passed in is not the size the plan was made for.
    #[error("page is {got:?}, the detection plan expects {expected:?}")]
    PageSize {
        /// Plan source size.
        expected: [u32; 2],
        /// Actual page size.
        got: [u32; 2],
    },
    /// The runner returned a different number of tile results than tiles sent.
    #[error("runner returned {got} tile results for {expected} tiles")]
    MapCount {
        /// Tiles sent.
        expected: usize,
        /// Results received.
        got: usize,
    },
    /// A tile result has the wrong number of channels.
    #[error("tile {tile}: runner returned {got} channels, expected {expected}")]
    ChannelCount {
        /// Tile index in plan order.
        tile: usize,
        /// Engine channel count.
        expected: usize,
        /// Channels received.
        got: usize,
    },
    /// A map has the wrong size.
    #[error("tile {tile} channel {channel}: map is {got:?}, expected {expected:?}")]
    MapShape {
        /// Tile index in plan order.
        tile: usize,
        /// Channel index.
        channel: usize,
        /// Expected `[w, h]`.
        expected: [u32; 2],
        /// Received `[w, h]`.
        got: [u32; 2],
    },
    /// An internal map buffer did not match its size.
    #[error(transparent)]
    Map(#[from] ProbMapError),
    /// The result mask exceeds the pixel limit.
    #[error(transparent)]
    Mask(#[from] MaskError),
}

/// An engine postprocess: stitched scaled-space maps (one per channel) -> source-px blocks and
/// the source-size mask.
type Postprocess = fn(&DetectionPlan, &RgbImage, &[ProbMap]) -> Result<(Vec<DetectRect>, BinaryMask), DetectError>;

/// The one place that decides which postprocess an engine runs, checked before any forward pass.
fn postprocess_for(engine: EngineKind) -> Result<Postprocess, DetectError> {
    match engine {
        EngineKind::Paddle => Ok(paddle_postprocess),
        EngineKind::Ctd => Ok(crate::ctd::postprocess),
        EngineKind::Surya => Ok(crate::surya::postprocess),
        // Classic has its own pipeline in the translation tab and never runs through a runner.
        EngineKind::Classic => Err(DetectError::Unsupported { engine }),
    }
}

/// Detects text on `page` (the full-resolution source, `plan.source_size()`) with `runner`.
///
/// Blocks come back in source pixels, finalized; the mask is source-size 0/255.
///
/// # Errors
/// [`DetectError::Unsupported`] (before any forward pass) for an engine without a postprocess;
/// [`DetectError::PageSize`] when `page` does not match the plan; the runner's error; the map
/// validation errors of [`forward_and_stitch`]; [`DetectError::Mask`] for an oversized mask.
pub fn run_detection(plan: &DetectionPlan, page: &RgbImage, runner: &mut dyn ProbMapRunner) -> Result<Detection, DetectError> {
    let postprocess = postprocess_for(plan.engine())?;
    let (maps, stats) = forward_and_stitch(plan, page, runner)?;
    let (mut blocks, mask) = postprocess(plan, page, &maps)?;
    finalize_blocks(&mut blocks);
    Ok(Detection { source_size: plan.source_size(), blocks, mask, stats })
}

/// Runs the plan up to the stitched maps: one resize, tiles in batches of
/// `runner.max_batch(tile_input)`, map validation, Surya maps upsampled x4 into tile space, and
/// stitching. Returns one scaled-space map per channel (`plan.scaled_size()`) and the counters.
///
/// # Errors
/// [`DetectError::PageSize`] when `page` does not match the plan; [`DetectError::Runner`];
/// [`DetectError::MapCount`], [`DetectError::ChannelCount`], [`DetectError::MapShape`] for a
/// malformed runner result.
pub fn forward_and_stitch(plan: &DetectionPlan, page: &RgbImage, runner: &mut dyn ProbMapRunner) -> Result<(Vec<ProbMap>, DetectionStats), DetectError> {
    let got = [page.width(), page.height()];
    if got != plan.source_size() {
        return Err(DetectError::PageSize { expected: plan.source_size(), got });
    }
    let [scaled_w, scaled_h] = plan.scaled_size();
    let scaled: Cow<'_, RgbImage> = if plan.scaled_size() == plan.source_size() {
        Cow::Borrowed(page)
    } else {
        Cow::Owned(imageops::resize(page, scaled_w, scaled_h, filter_type(plan.filter())))
    };
    let tile_input = plan.tile_input();
    let map_size = plan.map_size();
    let channels = plan.channels();
    let batch = runner.max_batch(tile_input).get();
    let tiles = plan.tiles();
    let mut per_channel: Vec<Vec<ProbMap>> = (0..channels).map(|_| Vec::with_capacity(tiles.len())).collect();
    let mut batches = 0_u32;
    for (chunk_index, chunk) in tiles.chunks(batch).enumerate() {
        let inputs: Vec<RgbImage> = chunk.iter().map(|rect| cut_tile(&scaled, rect, tile_input, plan.pad_rgb())).collect();
        let outputs = runner.forward(&inputs)?;
        batches = batches.saturating_add(1);
        if outputs.len() != chunk.len() {
            return Err(DetectError::MapCount { expected: chunk.len(), got: outputs.len() });
        }
        for (offset, tile_maps) in outputs.into_iter().enumerate() {
            let tile = chunk_index * batch + offset;
            if tile_maps.maps.len() != channels {
                return Err(DetectError::ChannelCount { tile, expected: channels, got: tile_maps.maps.len() });
            }
            for ((channel, map), store) in tile_maps.maps.into_iter().enumerate().zip(per_channel.iter_mut()) {
                if map.size() != map_size {
                    return Err(DetectError::MapShape { tile, channel, expected: map_size, got: map.size() });
                }
                store.push(to_tile_space(map, tile_input)?);
            }
        }
    }
    drop(scaled);
    let stitched = per_channel
        .iter()
        .map(|maps| {
            let refs: Vec<&ProbMap> = maps.iter().collect();
            stitch_tiles(plan, &refs)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let stats = DetectionStats { tiles: u32::try_from(tiles.len()).unwrap_or(u32::MAX), batches };
    Ok((stitched, stats))
}

/// The `image` filter of a plan filter.
fn filter_type(filter: ResizeFilter) -> FilterType {
    match filter {
        ResizeFilter::Triangle => FilterType::Triangle,
        ResizeFilter::Lanczos3 => FilterType::Lanczos3,
    }
}

/// Copies `rect` of the scaled page into the top-left corner of a `tile_input` image filled
/// with `pad`. The plan guarantees `rect` lies inside the scaled page and fits `tile_input`.
fn cut_tile(scaled: &RgbImage, rect: &TileRect, tile_input: [u32; 2], pad: [u8; 3]) -> RgbImage {
    let mut tile = RgbImage::from_pixel(tile_input[0], tile_input[1], Rgb(pad));
    let src_stride = idx(scaled.width()) * 3;
    let dst_stride = idx(tile_input[0]) * 3;
    let row_bytes = idx(rect.w) * 3;
    let src = scaled.as_raw();
    let dst: &mut [u8] = &mut tile;
    for row in 0..idx(rect.h) {
        let from = (idx(rect.y) + row) * src_stride + idx(rect.x) * 3;
        let to = row * dst_stride;
        dst[to..to + row_bytes].copy_from_slice(&src[from..from + row_bytes]);
    }
    tile
}

/// Brings a validated runner map (`map_size`) to tile space: identity at stride 1, a bilinear
/// (Triangle) upsample for Surya's quarter-resolution heatmap.
fn to_tile_space(map: ProbMap, tile_input: [u32; 2]) -> Result<ProbMap, DetectError> {
    if map.size() == tile_input {
        return Ok(map);
    }
    let [w, h] = map.size();
    let gray = GrayImage::from_raw(w, h, map.into_data()).ok_or(DetectError::MapShape { tile: 0, channel: 0, expected: [w, h], got: [w, h] })?;
    let up = imageops::resize(&gray, tile_input[0], tile_input[1], FilterType::Triangle);
    Ok(ProbMap::new(tile_input[0], tile_input[1], up.into_raw())?)
}

/// Paddle postprocess: DB boxes on the stitched u8 map, rescaled to source px, then the glyph
/// mask on the full-resolution page (same rules as the native OCR detection path).
fn paddle_postprocess(plan: &DetectionPlan, page: &RgbImage, maps: &[ProbMap]) -> Result<(Vec<DetectRect>, BinaryMask), DetectError> {
    let [src_w, src_h] = plan.source_size();
    let prob = maps.first().ok_or(DetectError::ChannelCount { tile: 0, expected: 1, got: 0 })?;
    let quads = boxes_from_bitmap(prob.data(), idx(prob.width()), idx(prob.height()), src_w, src_h);
    let blocks = quads
        .iter()
        .filter_map(|quad| {
            let [x1, y1, x2, y2] = block_from_quad(quad, src_w, src_h);
            DetectRect::from_xyxy(x1, y1, x2, y2)
        })
        .collect();
    let mask = binary_alpha_from_gray(build_glyph_mask(page, &quads))?;
    Ok((blocks, mask))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::*;
    use crate::plan::{DetectParams, plan_detection};
    use crate::runner::TileMaps;

    fn plan(engine: EngineKind, w: u32, h: u32) -> DetectionPlan {
        plan_detection(engine, [w, h], &DetectParams::default()).unwrap_or_else(|err| panic!("{err}"))
    }

    /// Fake runner: every channel map is the red channel of the tile (sub-sampled by `stride`),
    /// optionally corrupted by `tamper`. Records the batch sizes and checks the tile padding.
    struct RedRunner {
        batch: usize,
        channels: usize,
        stride: u32,
        pad: [u8; 3],
        valid: Vec<[u32; 2]>,
        calls: Vec<usize>,
        tamper: fn(&mut Vec<TileMaps>),
    }

    impl RedRunner {
        fn new(plan: &DetectionPlan, batch: usize) -> Self {
            Self { batch, channels: plan.channels(), stride: plan.map_stride(), pad: plan.pad_rgb(), valid: plan.tiles().iter().map(|t| [t.w, t.h]).collect(), calls: Vec::new(), tamper: |_| {} }
        }
    }

    impl ProbMapRunner for RedRunner {
        fn max_batch(&self, _tile_input: [u32; 2]) -> NonZeroUsize {
            NonZeroUsize::new(self.batch).unwrap_or(NonZeroUsize::MIN)
        }

        fn forward(&mut self, tiles: &[RgbImage]) -> Result<Vec<TileMaps>, RunnerError> {
            let first = self.calls.iter().sum::<usize>();
            self.calls.push(tiles.len());
            let mut out = Vec::new();
            for (k, tile) in tiles.iter().enumerate() {
                let [vw, vh] = self.valid[first + k];
                for (x, y, px) in tile.enumerate_pixels() {
                    if x >= vw || y >= vh {
                        assert_eq!(px.0, self.pad, "padding at ({x}, {y})");
                    }
                }
                let (mw, mh) = (tile.width() / self.stride, tile.height() / self.stride);
                let data: Vec<u8> = (0..mh).flat_map(|y| (0..mw).map(move |x| (x, y))).map(|(x, y)| tile.get_pixel(x * self.stride, y * self.stride).0[0]).collect();
                let map = ProbMap::new(mw, mh, data).map_err(|err| RunnerError { message: err.to_string() })?;
                out.push(TileMaps { maps: vec![map; self.channels] });
            }
            (self.tamper)(&mut out);
            Ok(out)
        }
    }

    /// Fake model of the Paddle end-to-end test: probability 1 on dark page pixels, 0 on light
    /// ones and on the black padding.
    struct DarkRunner;

    impl ProbMapRunner for DarkRunner {
        fn max_batch(&self, _tile_input: [u32; 2]) -> NonZeroUsize {
            NonZeroUsize::MIN
        }

        fn forward(&mut self, tiles: &[RgbImage]) -> Result<Vec<TileMaps>, RunnerError> {
            Ok(tiles
                .iter()
                .map(|t| {
                    let data = t.pixels().map(|px| if px.0[0] < 128 && px.0 != [0, 0, 0] { 255 } else { 0 }).collect();
                    let map = ProbMap::new(t.width(), t.height(), data).unwrap_or_else(|err| panic!("{err}"));
                    TileMaps { maps: vec![map] }
                })
                .collect())
        }
    }

    /// A runner whose forward pass always fails.
    struct Failing;

    impl ProbMapRunner for Failing {
        fn max_batch(&self, _tile_input: [u32; 2]) -> NonZeroUsize {
            NonZeroUsize::MIN
        }

        fn forward(&mut self, _tiles: &[RgbImage]) -> Result<Vec<TileMaps>, RunnerError> {
            Err(RunnerError { message: "backend down".to_owned() })
        }
    }

    /// A malformed-output case: how to corrupt the runner result, and which error must follow.
    type TamperCase = (fn(&mut Vec<TileMaps>), fn(&DetectError) -> bool);

    /// A page whose red channel encodes position and whose other channels are noise.
    fn position_page(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| Rgb([u8::try_from((x * 7 + y * 3) % 251).unwrap_or(0), u8::try_from((x * y) % 256).unwrap_or(0), 9]))
    }

    #[test]
    fn stitched_map_equals_the_page_function_at_scale_one() {
        let p = plan(EngineKind::Paddle, 960, 3000);
        assert!(p.scale().iter().all(|&s| (s - 1.0).abs() < f64::EPSILON));
        assert!(p.tiles().len() > 1);
        let page = position_page(960, 3000);
        let mut runner = RedRunner::new(&p, 3);
        let (maps, stats) = forward_and_stitch(&p, &page, &mut runner).unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(maps.len(), 1);
        let red: Vec<u8> = page.pixels().map(|px| px.0[0]).collect();
        assert_eq!(maps[0].data(), red.as_slice());
        assert_eq!(stats.tiles, u32::try_from(p.tiles().len()).unwrap_or(0));
        assert_eq!(runner.calls.iter().sum::<usize>(), p.tiles().len());
        assert!(runner.calls.iter().all(|&n| n <= 3));
        assert_eq!(stats.batches, u32::try_from(runner.calls.len()).unwrap_or(0));
    }

    #[test]
    fn surya_quarter_maps_are_upsampled_and_stitched() {
        let p = plan(EngineKind::Surya, 1200, 3000);
        let page = RgbImage::from_pixel(1200, 3000, Rgb([180, 0, 0]));
        let mut runner = RedRunner::new(&p, 8);
        let (maps, _) = forward_and_stitch(&p, &page, &mut runner).unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(maps[0].size(), [1200, 3000]);
        assert!(maps[0].data().iter().all(|&v| v == 180));
    }

    #[test]
    fn paddle_run_finds_a_dark_text_block_on_a_tiled_page() {
        // A white strip with one dark text-like bar far down: the fake model marks dark pixels.
        let (w, h) = (900_u32, 4000_u32);
        let page = RgbImage::from_fn(w, h, |x, y| if (100..400).contains(&x) && (2500..2540).contains(&y) { Rgb([10, 10, 10]) } else { Rgb([250, 250, 250]) });
        let p = plan(EngineKind::Paddle, w, h);
        assert!(p.grid()[1] > 1);
        let detection = run_detection(&p, &page, &mut DarkRunner).unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(detection.source_size, [w, h]);
        assert_eq!(detection.blocks.len(), 1, "{:?}", detection.blocks);
        let b = detection.blocks[0];
        assert!(b.x1 <= 100.0 && b.x2 >= 399.0 && b.y1 <= 2500.0 && b.y2 >= 2539.0, "{b:?}");
        assert!(b.x1 > 50.0 && b.x2 < 450.0 && b.y1 > 2450.0 && b.y2 < 2590.0, "{b:?}");
        assert_eq!(detection.mask.size, [w, h]);
        let at = |x: u32, y: u32| detection.mask.alpha[idx(y * w + x)];
        assert_eq!(at(200, 2520), 255);
        assert_eq!(at(200, 1000), 0);
    }

    #[test]
    fn classic_is_unsupported_before_any_forward_pass() {
        let p = plan(EngineKind::Classic, 400, 400);
        let page = RgbImage::new(400, 400);
        let mut runner = RedRunner::new(&p, 1);
        assert_eq!(run_detection(&p, &page, &mut runner), Err(DetectError::Unsupported { engine: EngineKind::Classic }));
        assert!(runner.calls.is_empty(), "Classic called the runner");
    }

    #[test]
    fn malformed_runner_output_is_rejected() {
        let p = plan(EngineKind::Paddle, 960, 3000);
        let page = position_page(960, 3000);
        let cases: [TamperCase; 3] = [
            (|out| {
                out.pop();
            }, |e| matches!(e, DetectError::MapCount { .. })),
            (|out| out[0].maps.clear(), |e| matches!(e, DetectError::ChannelCount { tile: 0, expected: 1, got: 0 })),
            (|out| out[1].maps[0] = ProbMap::new(2, 2, vec![0; 4]).unwrap_or_else(|err| panic!("{err}")), |e| matches!(e, DetectError::MapShape { tile: 1, channel: 0, got: [2, 2], .. })),
        ];
        for (tamper, expected) in cases {
            let mut runner = RedRunner::new(&p, 2);
            runner.tamper = tamper;
            let err = run_detection(&p, &page, &mut runner).expect_err("malformed output");
            assert!(expected(&err), "{err:?}");
        }
    }

    #[test]
    fn page_size_mismatch_and_runner_errors_propagate() {
        let p = plan(EngineKind::Paddle, 500, 400);
        let mut runner = RedRunner::new(&p, 1);
        assert_eq!(run_detection(&p, &RgbImage::new(400, 500), &mut runner), Err(DetectError::PageSize { expected: [500, 400], got: [400, 500] }));
        let err = run_detection(&p, &RgbImage::new(500, 400), &mut Failing).expect_err("runner failure");
        assert_eq!(err.to_string(), "backend down");
    }
}
