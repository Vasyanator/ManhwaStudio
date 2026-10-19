/*
File: crates/ms-text-detect/src/polygon_mask.rs

Purpose:
Turns text-region polygons with holes (as returned by cloud OCR services that answer a mask as
polygons instead of a bitmap) into the crate's 0/255 `BinaryMask`.

Key items:
- `MaskPolygon`: one outer ring plus the hole rings that belong to THIS polygon only.
- `rasterize_polygons()`: page-sized 0/255 mask = OR over polygons of (outer minus own holes).

Notes:
Holes are per polygon: a polygon's hole clears only that polygon's pixels, never a sibling
polygon's, so each polygon is rasterized into its own bbox-sized scratch and then ORed into the
page mask. Every ring goes through `ms_raster::fill_polygon_spans` in PAGE coordinates (the
scratch only offsets the span indices), so the sampling rule is exactly the primitive's: no
vertex is ever translated or rounded here. The scratch bbox is taken from the outer ring's own
spans, which makes it exact by construction. No logging, no I/O; wasm-buildable.
*/

use crate::mask::{BinaryMask, MaskError, exceeds_pixel_limit};
use crate::num::idx;

/// A filled region of a text mask: the inside of `outer` minus the inside of each of its `holes`.
///
/// Coordinates are page pixels, used exactly as given (the even-odd, scanline-centre sampling of
/// `ms_raster::fill_polygon_spans`); vertices may lie outside the page. A ring with fewer than
/// three points is ignored (an outer ring like that makes the whole polygon empty). Overlapping
/// holes are each cleared (holes are not combined by even-odd across rings).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MaskPolygon {
    /// The outer boundary, implicitly closed.
    pub outer: Vec<(f32, f32)>,
    /// Hole rings, implicitly closed; each clears only this polygon's pixels.
    pub holes: Vec<Vec<(f32, f32)>>,
}

/// Rasterizes `polygons` into a `size = [width, height]` 0/255 mask.
///
/// The result is the OR over all polygons of "inside the outer ring and inside none of that
/// polygon's own holes", so one polygon's hole never clears another polygon. A size with a zero
/// dimension yields the empty mask (`[0, 0]`, no bytes), matching [`BinaryMask`]'s zero-area
/// form; an empty polygon list yields an all-zero mask of `size`. Pure CPU work proportional to
/// the polygons' areas: callers run it off the GUI thread.
///
/// # Errors
/// [`MaskError::TooLarge`] when `size` has more than [`crate::mask::MAX_MASK_PIXELS`] pixels
/// (checked before anything is allocated).
pub fn rasterize_polygons(size: [u32; 2], polygons: &[MaskPolygon]) -> Result<BinaryMask, MaskError> {
    let [width, height] = size;
    if width == 0 || height == 0 {
        return Ok(BinaryMask { size: [0, 0], alpha: Vec::new() });
    }
    if exceeds_pixel_limit(width, height) {
        return Err(MaskError::TooLarge { width, height });
    }
    let (width, height) = (idx(width), idx(height));
    // `width * height <= MAX_MASK_PIXELS` was just checked, so neither the product nor any
    // in-bounds `y * width + x` below can overflow.
    let mut alpha = vec![0u8; width * height];
    // One scratch allocation reused by every polygon.
    let mut scratch: Vec<u8> = Vec::new();
    for polygon in polygons {
        let Some(bbox) = span_bbox(&polygon.outer, width, height) else {
            continue;
        };
        let scratch_width = bbox.x1 - bbox.x0 + 1;
        scratch.clear();
        scratch.resize(scratch_width * (bbox.y1 - bbox.y0 + 1), 0);
        fill_ring(&polygon.outer, width, height, &bbox, &mut scratch, 255);
        for hole in &polygon.holes {
            fill_ring(hole, width, height, &bbox, &mut scratch, 0);
        }
        for (row, scratch_row) in scratch.chunks_exact(scratch_width).enumerate() {
            let start = (bbox.y0 + row) * width + bbox.x0;
            // In bounds: the bbox lies inside the page (it comes from the primitive's clamped spans).
            if let Some(page_row) = alpha.get_mut(start..start + scratch_width) {
                for (dst, &src) in page_row.iter_mut().zip(scratch_row) {
                    *dst |= src;
                }
            }
        }
    }
    Ok(BinaryMask { size, alpha })
}

/// Inclusive page-pixel bounding box of a ring's filled spans.
struct SpanBox {
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
}

/// The inclusive bbox of every pixel `ring` fills inside a `width x height` page, or `None` when
/// it fills none (fewer than three points, degenerate, or fully outside the page).
fn span_bbox(ring: &[(f32, f32)], width: usize, height: usize) -> Option<SpanBox> {
    let mut bbox: Option<SpanBox> = None;
    ms_raster::fill_polygon_spans(ring, width, height, |y, x0, x1| match &mut bbox {
        None => bbox = Some(SpanBox { x0, x1, y0: y, y1: y }),
        Some(b) => {
            b.x0 = b.x0.min(x0);
            b.x1 = b.x1.max(x1);
            // Spans arrive in increasing `y`, but min/max keeps this independent of that order.
            b.y0 = b.y0.min(y);
            b.y1 = b.y1.max(y);
        }
    });
    bbox
}

/// Writes `value` into every pixel of `scratch` (the `bbox` window of the page, row-major) that
/// `ring` fills in page coordinates; spans outside the window are clipped to it.
fn fill_ring(ring: &[(f32, f32)], width: usize, height: usize, bbox: &SpanBox, scratch: &mut [u8], value: u8) {
    let scratch_width = bbox.x1 - bbox.x0 + 1;
    ms_raster::fill_polygon_spans(ring, width, height, |y, x0, x1| {
        // A hole may reach beyond its polygon's bbox; only the overlap can clear anything.
        if y < bbox.y0 || y > bbox.y1 || x1 < bbox.x0 || x0 > bbox.x1 {
            return;
        }
        let start = (y - bbox.y0) * scratch_width + (x0.max(bbox.x0) - bbox.x0);
        let end = (y - bbox.y0) * scratch_width + (x1.min(bbox.x1) - bbox.x0);
        // In bounds: the span was clipped to the window above.
        if let Some(run) = scratch.get_mut(start..=end) {
            run.fill(value);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<(f32, f32)> {
        vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
    }

    fn rasterize(size: [u32; 2], polygons: &[MaskPolygon]) -> BinaryMask {
        rasterize_polygons(size, polygons).unwrap_or_else(|err| panic!("in-bounds mask: {err}"))
    }

    /// Independent reference: the page-sized fill of one ring, straight from the primitive.
    fn ring_fill(ring: &[(f32, f32)], width: usize, height: usize) -> Vec<bool> {
        let mut out = vec![false; width * height];
        ms_raster::fill_polygon_spans(ring, width, height, |y, x0, x1| {
            for x in x0..=x1 {
                out[y * width + x] = true;
            }
        });
        out
    }

    fn at(mask: &BinaryMask, x: usize, y: usize) -> u8 {
        mask.alpha[y * idx(mask.size[0]) + x]
    }

    fn assert_strictly_binary(mask: &BinaryMask) {
        assert_eq!(mask.alpha.len(), idx(mask.size[0]) * idx(mask.size[1]));
        assert!(mask.alpha.iter().all(|&v| v == 0 || v == 255), "mask must be strictly 0/255");
    }

    #[test]
    fn square_with_square_hole_is_a_ring() {
        let polygon = MaskPolygon { outer: rect(2.0, 2.0, 17.0, 17.0), holes: vec![rect(6.0, 6.0, 13.0, 13.0)] };
        let mask = rasterize([20, 20], std::slice::from_ref(&polygon));
        assert_strictly_binary(&mask);
        let outer = ring_fill(&polygon.outer, 20, 20);
        let hole = ring_fill(&polygon.holes[0], 20, 20);
        for (i, &v) in mask.alpha.iter().enumerate() {
            assert_eq!(v == 255, outer[i] && !hole[i], "pixel {i} differs from outer minus hole");
        }
        assert_eq!(at(&mask, 3, 3), 255, "ring body is filled");
        assert_eq!(at(&mask, 9, 9), 0, "hole centre is cleared");
        assert_eq!(at(&mask, 0, 0), 0, "outside stays empty");
    }

    #[test]
    fn a_hole_never_clears_another_polygon() {
        // A's hole covers the whole of B; B must stay filled.
        let a = MaskPolygon { outer: rect(0.0, 0.0, 30.0, 30.0), holes: vec![rect(5.0, 5.0, 25.0, 25.0)] };
        let b = MaskPolygon { outer: rect(10.0, 10.0, 20.0, 20.0), holes: Vec::new() };
        for order in [[a.clone(), b.clone()], [b.clone(), a.clone()]] {
            let mask = rasterize([32, 32], &order);
            assert_strictly_binary(&mask);
            assert_eq!(at(&mask, 15, 15), 255, "B's interior survives A's hole");
            assert_eq!(at(&mask, 7, 7), 0, "A's hole outside B stays cleared");
            assert_eq!(at(&mask, 2, 2), 255, "A's ring body is filled");
        }
    }

    #[test]
    fn hole_reaching_beyond_its_polygon_is_clipped() {
        let polygon = MaskPolygon { outer: rect(0.0, 0.0, 10.0, 10.0), holes: vec![rect(5.0, -5.0, 40.0, 40.0)] };
        let other = MaskPolygon { outer: rect(20.0, 20.0, 30.0, 30.0), holes: Vec::new() };
        let mask = rasterize([32, 32], &[polygon, other]);
        assert_eq!(at(&mask, 2, 2), 255);
        assert_eq!(at(&mask, 7, 7), 0);
        assert_eq!(at(&mask, 25, 25), 255, "a sibling under the hole's overhang stays filled");
    }

    #[test]
    fn polygon_partly_outside_the_page_is_clipped() {
        let polygon = MaskPolygon { outer: rect(-10.0, -10.0, 5.0, 5.0), holes: vec![rect(-20.0, -20.0, 2.0, 2.0)] };
        let mask = rasterize([8, 8], std::slice::from_ref(&polygon));
        assert_strictly_binary(&mask);
        let outer = ring_fill(&polygon.outer, 8, 8);
        let hole = ring_fill(&polygon.holes[0], 8, 8);
        for (i, &v) in mask.alpha.iter().enumerate() {
            assert_eq!(v == 255, outer[i] && !hole[i], "pixel {i}");
        }
        assert_eq!(at(&mask, 4, 4), 255);
        assert_eq!(at(&mask, 0, 0), 0, "hole part inside the page is cleared");
        assert_eq!(at(&mask, 7, 7), 0);
    }

    #[test]
    fn polygon_fully_outside_the_page_adds_nothing() {
        let polygon = MaskPolygon { outer: rect(100.0, 100.0, 120.0, 120.0), holes: Vec::new() };
        let mask = rasterize([8, 8], &[polygon]);
        assert_eq!(mask.alpha, vec![0; 64]);
    }

    #[test]
    fn empty_list_is_all_zero() {
        let mask = rasterize([7, 3], &[]);
        assert_eq!(mask, BinaryMask { size: [7, 3], alpha: vec![0; 21] });
    }

    #[test]
    fn degenerate_rings_are_ignored() {
        let short_outer = MaskPolygon { outer: vec![(1.0, 1.0), (5.0, 5.0)], holes: Vec::new() };
        let collinear = MaskPolygon { outer: vec![(1.0, 1.0), (3.0, 3.0), (6.0, 6.0)], holes: Vec::new() };
        let empty = MaskPolygon::default();
        let mask = rasterize([8, 8], &[short_outer, collinear, empty]);
        assert_eq!(mask.alpha, vec![0; 64], "degenerate outer rings fill nothing");

        // A degenerate hole clears nothing.
        let filled = MaskPolygon { outer: rect(0.0, 0.0, 8.0, 8.0), holes: vec![vec![(2.0, 2.0), (4.0, 4.0)], Vec::new()] };
        let mask = rasterize([8, 8], std::slice::from_ref(&filled));
        let outer = ring_fill(&filled.outer, 8, 8);
        for (i, &v) in mask.alpha.iter().enumerate() {
            assert_eq!(v == 255, outer[i], "pixel {i}");
        }
    }

    #[test]
    fn non_finite_vertices_never_panic() {
        let polygon = MaskPolygon { outer: vec![(f32::NAN, 1.0), (f32::INFINITY, 2.0), (3.0, f32::NEG_INFINITY)], holes: vec![vec![(f32::NAN, f32::NAN); 3]] };
        let mask = rasterize([6, 6], &[polygon]);
        assert_strictly_binary(&mask);
    }

    #[test]
    fn zero_size_is_the_empty_mask() {
        let polygon = MaskPolygon { outer: rect(0.0, 0.0, 4.0, 4.0), holes: Vec::new() };
        for size in [[0, 0], [5, 0], [0, 5]] {
            let mask = rasterize(size, std::slice::from_ref(&polygon));
            assert_eq!(mask, BinaryMask { size: [0, 0], alpha: Vec::new() });
        }
    }

    #[test]
    fn pixel_limit_is_refused_before_allocation() {
        let err = rasterize_polygons([10_001, 10_000], &[]).expect_err("oversize mask");
        assert_eq!(err, MaskError::TooLarge { width: 10_001, height: 10_000 });
        let err = rasterize_polygons([u32::MAX, u32::MAX], &[]).expect_err("oversize mask");
        assert_eq!(err, MaskError::TooLarge { width: u32::MAX, height: u32::MAX });
    }
}
