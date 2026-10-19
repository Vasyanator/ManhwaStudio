/*
File: crates/ms-tab-translation/src/page_crop.rs

Purpose:
The one rule that turns a page-normalized selection rect (`uv_rect`, 0..1 of the page) into the
pixel rect cropped out of the decoded page. The OCR worker crops its input with it and the text
detector's region detection crops the same pixels with it, so an OCR selection and the detector
run on that selection see exactly the same image.

Key items:
- `PixelRect`: a non-empty pixel rect inside the page (`x`, `y`, `width`, `height`).
- `uv_rect_to_pixel_rect()`: uv -> pixel rect (floor of the near edges, ceil of the far edges,
  clamped into the page).
- `normalized_uv()`: orders and clamps a uv rect into `[left, top, right, bottom]` in 0..1.

Notes:
Pure and GUI-free; callers decode the page on their worker thread.
*/

/// A non-empty pixel rect inside a page: `x + width <= page width`, `y + height <= page height`,
/// `width > 0`, `height > 0` (guaranteed by [`uv_rect_to_pixel_rect`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PixelRect {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl PixelRect {
    /// The rect as exclusive page-pixel edges `[x1, y1, x2, y2]`.
    #[must_use]
    pub(crate) fn xyxy(self) -> [u32; 4] {
        // `uv_rect_to_pixel_rect` built `x + width` and `y + height` from edges inside the page,
        // so the sums cannot overflow; saturation only guards a hand-built value.
        [self.x, self.y, self.x.saturating_add(self.width), self.y.saturating_add(self.height)]
    }
}

/// Orders and clamps a uv rect `[u1, v1, u2, v2]` (corners in any order) into
/// `[left, top, right, bottom]`, each in `0.0..=1.0`.
#[must_use]
pub(crate) fn normalized_uv(uv: [f32; 4]) -> [f32; 4] {
    let left = uv[0].min(uv[2]).clamp(0.0, 1.0);
    let right = uv[0].max(uv[2]).clamp(0.0, 1.0);
    let top = uv[1].min(uv[3]).clamp(0.0, 1.0);
    let bottom = uv[1].max(uv[3]).clamp(0.0, 1.0);
    [left, top, right, bottom]
}

/// The pixel rect a page-normalized `uv_rect` selects on an `img_w` x `img_h` page: the near
/// edges are floored, the far edges ceiled (so a selection never loses a partially covered
/// pixel), the near edges stay inside the page and the far edges are clamped to it.
///
/// Returns `None` for an empty page or when the rect covers no whole pixel column or row (the
/// callers' "selection too small" case).
#[must_use]
pub(crate) fn uv_rect_to_pixel_rect(uv_rect: [f32; 4], img_w: u32, img_h: u32) -> Option<PixelRect> {
    if img_w == 0 || img_h == 0 {
        return None;
    }
    let [u1, v1, u2, v2] = normalized_uv(uv_rect);
    // The uv values are clamped to 0..=1, so every product lies in 0..=side and the float ->
    // int casts (saturating in Rust) cannot truncate a meaningful value; the u32 -> f32 widening
    // is exact for every side a page can have (the detector refuses pages above its pixel limit
    // long before 2^24). This is the exact arithmetic the OCR crop has always used.
    let x1 = ((u1 * img_w as f32).floor() as u32).min(img_w.saturating_sub(1));
    let y1 = ((v1 * img_h as f32).floor() as u32).min(img_h.saturating_sub(1));
    let x2 = ((u2 * img_w as f32).ceil() as u32).min(img_w);
    let y2 = ((v2 * img_h as f32).ceil() as u32).min(img_h);
    if x2 <= x1 || y2 <= y1 {
        return None;
    }
    Some(PixelRect { x: x1, y: y1, width: x2 - x1, height: y2 - y1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floors_near_edges_and_ceils_far_edges() {
        // 0.25 * 10 = 2.5 -> 2 ; 0.75 * 10 = 7.5 -> 8 ; 0.1 * 20 = 2 ; 0.55 * 20 = 11.
        let rect = uv_rect_to_pixel_rect([0.25, 0.1, 0.75, 0.55], 10, 20);
        assert_eq!(rect, Some(PixelRect { x: 2, y: 2, width: 6, height: 9 }));
        assert_eq!(rect.map(PixelRect::xyxy), Some([2, 2, 8, 11]));
    }

    #[test]
    fn corners_in_any_order_and_out_of_range_values_are_normalized() {
        let rect = uv_rect_to_pixel_rect([1.4, 0.9, -0.2, 0.5], 100, 50);
        assert_eq!(rect, Some(PixelRect { x: 0, y: 25, width: 100, height: 20 }));
    }

    #[test]
    fn full_page_and_page_edge_selections_stay_inside_the_page() {
        assert_eq!(uv_rect_to_pixel_rect([0.0, 0.0, 1.0, 1.0], 7, 3), Some(PixelRect { x: 0, y: 0, width: 7, height: 3 }));
        // A selection touching the far edge keeps its last column/row inside the page.
        assert_eq!(uv_rect_to_pixel_rect([0.95, 0.95, 1.0, 1.0], 10, 10), Some(PixelRect { x: 9, y: 9, width: 1, height: 1 }));
    }

    #[test]
    fn empty_page_or_degenerate_selection_is_none() {
        assert_eq!(uv_rect_to_pixel_rect([0.0, 0.0, 1.0, 1.0], 0, 10), None);
        assert_eq!(uv_rect_to_pixel_rect([0.5, 0.5, 0.5, 0.8], 10, 10), None, "zero width");
        // A point on the far corner is not degenerate: the near edges are kept inside the page,
        // so it selects the last pixel (the OCR crop's long-standing behaviour).
        assert_eq!(uv_rect_to_pixel_rect([1.0, 1.0, 1.0, 1.0], 10, 10), Some(PixelRect { x: 9, y: 9, width: 1, height: 1 }));
    }
}
