/*
File: tabs/ps_editor/selection.rs

Purpose:
Image-space selection mask for the PS-like editor. A selection constrains painting to the marked
region (like a Photoshop marquee). Geometry is built by the selection tools; brushing reads it.

Key structures:
- `Selection`: page-sized binary mask (`0` = outside, `255` = inside) plus a tight bounding box.

Notes:
The mask uses image pixel coordinates. An empty selection (no pixels set) is represented by
`None` at the call site, never by an all-zero mask, so "no selection" means "paint everywhere".
*/

/// Inclusive integer bounding box in image pixel coordinates.
#[derive(Debug, Clone, Copy)]
pub struct SelectionBounds {
    pub min_x: usize,
    pub min_y: usize,
    pub max_x: usize,
    pub max_y: usize,
}

/// Binary selection mask over a single page.
#[derive(Debug, Clone)]
pub struct Selection {
    width: usize,
    height: usize,
    mask: Vec<u8>,
    bounds: Option<SelectionBounds>,
    /// Closed boundary loops in image-pixel coordinates, used to draw the marching-ants marquee.
    /// Empty when nothing is selected; one loop for a rectangle/lasso.
    outline: Vec<Vec<(f32, f32)>>,
}

impl Selection {
    /// Creates an empty selection sized to the page.
    #[must_use]
    pub fn empty(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            mask: vec![0u8; width.saturating_mul(height)],
            bounds: None,
            outline: Vec::new(),
        }
    }

    /// Tight bounding box of the selection, or `None` when nothing is selected.
    #[must_use]
    pub fn bounds(&self) -> Option<SelectionBounds> {
        self.bounds
    }

    /// Closed boundary loops (image-pixel coords) for drawing the selection marquee.
    #[must_use]
    pub fn outline_loops(&self) -> &[Vec<(f32, f32)>] {
        &self.outline
    }

    /// True when at least one pixel is selected.
    #[must_use]
    pub fn any(&self) -> bool {
        self.bounds.is_some()
    }

    /// Returns whether the image pixel `(x, y)` is inside the selection.
    #[must_use]
    pub fn contains(&self, x: usize, y: usize) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        self.mask[y * self.width + x] != 0
    }

    /// Replaces the selection with an axis-aligned rectangle (inclusive of `min`, exclusive of
    /// `max`). Coordinates are clamped to the page; a degenerate rect clears the selection.
    pub fn set_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.mask.iter_mut().for_each(|v| *v = 0);
        self.bounds = None;
        self.outline.clear();
        let min_x = x0.min(x1).max(0);
        let min_y = y0.min(y1).max(0);
        let max_x = x0.max(x1).min(self.width as i32);
        let max_y = y0.max(y1).min(self.height as i32);
        if max_x <= min_x || max_y <= min_y {
            return;
        }
        // Boundary loop along the pixel edges (max coords are exclusive, i.e. the pixel boundary).
        let (fx0, fy0, fx1, fy1) = (min_x as f32, min_y as f32, max_x as f32, max_y as f32);
        self.outline = vec![vec![
            (fx0, fy0),
            (fx1, fy0),
            (fx1, fy1),
            (fx0, fy1),
            (fx0, fy0),
        ]];
        let (min_x, min_y) = (min_x as usize, min_y as usize);
        let (max_x, max_y) = (max_x as usize, max_y as usize);
        for y in min_y..max_y {
            let row = y * self.width;
            self.mask[row + min_x..row + max_x]
                .iter_mut()
                .for_each(|v| *v = 255);
        }
        self.bounds = Some(SelectionBounds {
            min_x,
            min_y,
            max_x: max_x - 1,
            max_y: max_y - 1,
        });
    }

    /// Replaces the selection with the interior of a closed polygon (even-odd fill).
    ///
    /// `points` are image-space vertices; the polygon is implicitly closed. Fewer than three
    /// points clears the selection.
    pub fn set_polygon(&mut self, points: &[(f32, f32)]) {
        self.mask.iter_mut().for_each(|v| *v = 0);
        self.bounds = None;
        self.outline.clear();
        if points.len() < 3 {
            return;
        }
        let min_y = points
            .iter()
            .map(|p| p.1.floor() as i32)
            .min()
            .unwrap_or(0)
            .clamp(0, self.height as i32);
        let max_y = points
            .iter()
            .map(|p| p.1.ceil() as i32)
            .max()
            .unwrap_or(0)
            .clamp(0, self.height as i32);
        let mut bounds: Option<SelectionBounds> = None;
        let mut crossings: Vec<f32> = Vec::new();
        for y in min_y..max_y {
            // Scanline center; test edges crossing this horizontal line (even-odd rule).
            let yc = y as f32 + 0.5;
            crossings.clear();
            for i in 0..points.len() {
                let (x0, y0) = points[i];
                let (x1, y1) = points[(i + 1) % points.len()];
                if (y0 <= yc && y1 > yc) || (y1 <= yc && y0 > yc) {
                    let t = (yc - y0) / (y1 - y0);
                    crossings.push(x0 + t * (x1 - x0));
                }
            }
            crossings.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let row = y as usize * self.width;
            for pair in crossings.chunks_exact(2) {
                let sx = pair[0].ceil().max(0.0) as i32;
                let ex = pair[1].floor().min(self.width as f32 - 1.0) as i32;
                if ex < sx {
                    continue;
                }
                for px in sx..=ex {
                    self.mask[row + px as usize] = 255;
                }
                Self::grow_bounds(&mut bounds, sx as usize, ex as usize, y as usize);
            }
        }
        self.bounds = bounds;
        // Draw the lasso path itself as the marquee when it enclosed any pixels.
        if self.bounds.is_some() {
            let mut loop_pts = points.to_vec();
            if let Some(&first) = points.first() {
                loop_pts.push(first);
            }
            self.outline = vec![loop_pts];
        }
    }

    fn grow_bounds(bounds: &mut Option<SelectionBounds>, sx: usize, ex: usize, y: usize) {
        match bounds {
            Some(b) => {
                b.min_x = b.min_x.min(sx);
                b.max_x = b.max_x.max(ex);
                b.min_y = b.min_y.min(y);
                b.max_y = b.max_y.max(y);
            }
            None => {
                *bounds = Some(SelectionBounds {
                    min_x: sx,
                    min_y: y,
                    max_x: ex,
                    max_y: y,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_selection_marks_interior_and_bounds() {
        let mut sel = Selection::empty(10, 10);
        sel.set_rect(2, 3, 6, 7);
        assert!(sel.any());
        assert!(sel.contains(2, 3));
        assert!(sel.contains(5, 6));
        assert!(!sel.contains(6, 7), "max edge is exclusive");
        assert!(!sel.contains(0, 0));
        // Interior corners are set; the exclusive max edge is not.
        assert!(sel.contains(2, 6));
        assert!(sel.contains(5, 3));
    }

    #[test]
    fn degenerate_rect_clears_selection() {
        let mut sel = Selection::empty(10, 10);
        sel.set_rect(4, 4, 4, 9);
        assert!(!sel.any());
    }

    #[test]
    fn polygon_triangle_fills_interior() {
        let mut sel = Selection::empty(10, 10);
        // Triangle covering the lower-left area.
        sel.set_polygon(&[(1.0, 1.0), (8.0, 1.0), (1.0, 8.0)]);
        assert!(sel.any());
        assert!(sel.contains(2, 2));
        assert!(!sel.contains(7, 7));
    }
}
