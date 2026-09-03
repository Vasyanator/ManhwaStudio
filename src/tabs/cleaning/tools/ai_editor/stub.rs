/*
File: ai_editor/stub.rs

Purpose:
The STEP-1 PLACEHOLDER processing of the «ИИ-редактор области» tool. It is not an
approximation of the future model and does not pretend to be one: it paints the mask layers
into the region as flat colours so that the whole path — paint, process, preview, apply,
undo — can be exercised for real before any AI backend exists (D9 of
`dev-docs/region_edit_v2_plan.md`). The panel says so in words the user can read.

Main responsibilities:
- turn the CURRENT clean-overlay pixels of the frame plus the mask layers into a result image
- refuse, with a named reason, every input whose geometry does not match the region

Key structures:
- `StubError`: why a placeholder run could not produce a result

Key functions:
- `build_stub_result()`: the whole placeholder, a pure function over its inputs

Notes:
Deliberately GUI-free and allocation-bounded: one image clone plus one pass per mask layer.
It runs on the GUI thread because it is O(region pixels) with no I/O and no decode — see the
module readme. The real consumer will run on a worker thread and this file will go away with
it.
*/

use super::super::region_edit_v2::layers::MaskStack;
use egui::{Color32, ColorImage};

/// Why the placeholder processing refused to produce a result.
///
/// Every variant is a geometry disagreement between the frame, its mask stack and the
/// captured clean-overlay chunk. None of them is recoverable by rescaling: a rescale is
/// exactly what D7 forbids, so the run fails loudly instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(super) enum StubError {
    /// The region has no pixels, so there is nothing to process.
    #[error("the region is empty ({w}x{h})")]
    EmptyRegion { w: usize, h: usize },
    /// The captured clean-overlay chunk does not cover the region exactly.
    #[error("the clean-overlay chunk is {chunk_w}x{chunk_h}, the region is {region_w}x{region_h}")]
    ChunkSize {
        chunk_w: usize,
        chunk_h: usize,
        region_w: usize,
        region_h: usize,
    },
    /// One fill colour per mask layer is required; the caller gave a different number.
    #[error("{fills} fill colours for {layers} mask layers")]
    FillCount { fills: usize, layers: usize },
    /// A mask layer's buffer does not hold exactly one byte per region pixel.
    #[error("mask layer {layer} holds {len} bytes, the region needs {expected}")]
    LayerBuffer {
        layer: usize,
        len: usize,
        expected: usize,
    },
}

/// Builds the placeholder result for one region.
///
/// The region geometry is the MASK STACK's — the frame keeps it equal to its own `rect_px` —
/// and `chunk` must be the clean-overlay pixels under exactly that region. The result starts
/// as those pixels and every set mask pixel of layer `i` is overwritten with `fills[i]`;
/// layers are applied in ASCENDING index order, so a pixel set in several layers takes the
/// colour of the HIGHEST one, which is also the order the frame paints its previews in.
///
/// The returned image is always exactly the region size, which is what makes the tool's apply
/// check (D7) pass for a stub result rather than reject it.
///
/// # Errors
/// [`StubError::EmptyRegion`] for a zero-sized region, [`StubError::ChunkSize`] when the
/// chunk does not cover the region exactly, [`StubError::FillCount`] when the fill table does
/// not have one entry per layer, and [`StubError::LayerBuffer`] when a layer's buffer is not
/// one byte per region pixel.
pub(super) fn build_stub_result(
    chunk: &ColorImage,
    masks: &MaskStack,
    fills: &[Color32],
) -> Result<ColorImage, StubError> {
    let (w, h) = masks.size();
    if w == 0 || h == 0 {
        return Err(StubError::EmptyRegion { w, h });
    }
    if fills.len() != masks.layer_count() {
        return Err(StubError::FillCount {
            fills: fills.len(),
            layers: masks.layer_count(),
        });
    }
    if chunk.size != [w, h] {
        return Err(StubError::ChunkSize {
            chunk_w: chunk.size[0],
            chunk_h: chunk.size[1],
            region_w: w,
            region_h: h,
        });
    }
    let expected = w.saturating_mul(h);
    for layer in 0..fills.len() {
        let len = masks.bytes(layer).len();
        if len != expected {
            return Err(StubError::LayerBuffer {
                layer,
                len,
                expected,
            });
        }
    }

    let mut out = chunk.clone();
    for (layer, fill) in fills.iter().enumerate() {
        // Lengths are equal by the check above, so `zip` cannot silently truncate a layer.
        for (pixel, byte) in out.pixels.iter_mut().zip(masks.bytes(layer)) {
            if *byte != 0 {
                *pixel = *fill;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILL_A: Color32 = Color32::WHITE;
    const FILL_B: Color32 = Color32::from_rgb(128, 128, 128);
    const BASE: Color32 = Color32::from_rgb(10, 20, 30);

    /// A two-layer stack of `w * h` pixels with the tints the tool uses.
    fn masks(w: usize, h: usize) -> MaskStack {
        MaskStack::new(w, h, &[FILL_A, FILL_B])
    }

    /// A base chunk of one uniform colour, standing in for the clean overlay under the frame.
    fn chunk(w: usize, h: usize) -> ColorImage {
        ColorImage::filled([w, h], BASE)
    }

    /// Paints one brush dot of the smallest radius into layer `layer` at `(x, y)`.
    ///
    /// The dot is a disc, not a single pixel: radius 1 covers everything within a Euclidean
    /// distance of 1, so on one row it sets `x - 1 ..= x + 1`. The tests below leave gaps
    /// wider than that between the dots they place.
    fn paint_dot(stack: &mut MaskStack, layer: usize, x: i32, y: i32) {
        stack.set_active(layer);
        stack.paint_segment((x, y), (x, y), 1, false);
    }

    #[test]
    fn an_unpainted_region_comes_back_unchanged() {
        let stack = masks(4, 3);
        let out = build_stub_result(&chunk(4, 3), &stack, &[FILL_A, FILL_B])
            .expect("an unpainted stack is a valid input");
        assert_eq!(out.size, [4, 3]);
        assert!(out.pixels.iter().all(|px| *px == BASE));
    }

    #[test]
    fn each_layer_paints_its_own_fill_colour() {
        let mut stack = masks(8, 1);
        paint_dot(&mut stack, 0, 1, 0);
        paint_dot(&mut stack, 1, 5, 0);
        let out = build_stub_result(&chunk(8, 1), &stack, &[FILL_A, FILL_B])
            .expect("a painted stack is a valid input");
        assert_eq!(out.pixels[1], FILL_A);
        assert_eq!(out.pixels[5], FILL_B);
        assert_eq!(out.pixels[3], BASE, "an unpainted pixel keeps the clean overlay");
        assert_eq!(out.pixels[7], BASE, "an unpainted pixel keeps the clean overlay");
    }

    #[test]
    fn the_highest_layer_wins_where_two_layers_overlap() {
        let mut stack = masks(8, 1);
        paint_dot(&mut stack, 0, 3, 0);
        paint_dot(&mut stack, 1, 3, 0);
        let out = build_stub_result(&chunk(8, 1), &stack, &[FILL_A, FILL_B])
            .expect("an overlapping stack is a valid input");
        assert_eq!(out.pixels[3], FILL_B);
    }

    #[test]
    fn the_result_is_opaque_where_the_mask_is_set() {
        let mut stack = masks(8, 1);
        paint_dot(&mut stack, 0, 1, 0);
        let transparent = ColorImage::filled([8, 1], Color32::TRANSPARENT);
        let out = build_stub_result(&transparent, &stack, &[FILL_A, FILL_B])
            .expect("a transparent chunk is a valid input");
        assert_eq!(out.pixels[1].a(), 255);
        assert_eq!(out.pixels[7].a(), 0, "an unpainted pixel keeps the overlay's alpha");
    }

    #[test]
    fn a_chunk_of_the_wrong_size_is_refused_rather_than_rescaled() {
        let stack = masks(4, 4);
        let error = build_stub_result(&chunk(3, 4), &stack, &[FILL_A, FILL_B])
            .expect_err("a chunk that does not cover the region must be refused");
        assert_eq!(
            error,
            StubError::ChunkSize {
                chunk_w: 3,
                chunk_h: 4,
                region_w: 4,
                region_h: 4,
            }
        );
    }

    #[test]
    fn a_fill_table_that_does_not_match_the_layer_count_is_refused() {
        let stack = masks(2, 2);
        let error = build_stub_result(&chunk(2, 2), &stack, &[FILL_A])
            .expect_err("one fill for two layers must be refused");
        assert_eq!(error, StubError::FillCount { fills: 1, layers: 2 });
    }

    #[test]
    fn an_empty_region_is_refused() {
        let stack = masks(0, 0);
        let error = build_stub_result(&ColorImage::filled([0, 0], BASE), &stack, &[FILL_A, FILL_B])
            .expect_err("a zero-sized region must be refused");
        assert_eq!(error, StubError::EmptyRegion { w: 0, h: 0 });
    }
}
