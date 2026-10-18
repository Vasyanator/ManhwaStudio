/*
File: crates/ms-region-postprocess/src/edge_band.rs

Purpose:
The colour-match reference of a WHOLE-REGION run (nothing painted, solid mask): the outer frame
of the region, where the regenerated picture still has to meet the page it is pasted into, so
generated and original are supposed to agree there.

Key items:
- `EdgeBand`: the frame of one region (depth, membership, sample count, bool mask).
- `EDGE_BAND_PX` / `EDGE_BAND_SIDE_DIVISOR`: the frame depth and its cap.

Notes:
For a solid mask the inside distance is `min(x + 1, y + 1, w - x, h - y)`, so the band is a plain
rectangle frame: membership is decided from the coordinates, no distance transform. Pure
geometry, no allocation except `to_mask`.
*/

/// Depth of the edge band, px: equals the default feather, and is thin on purpose — a model
/// whose drift grows away from the border biases a wide band's fit (measured: a 24 px band
/// doubled the residual of the 128x96 fixture case against an 8 px one).
pub(crate) const EDGE_BAND_PX: usize = 12;

/// The band is at most `shorter side / EDGE_BAND_SIDE_DIVISOR` deep (a quarter), so a small
/// region keeps an interior the band does not describe.
pub(crate) const EDGE_BAND_SIDE_DIVISOR: usize = 4;

/// The outer frame of a `width x height` region, `depth` px deep on every side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EdgeBand {
    width: usize,
    height: usize,
    depth: usize,
}

impl EdgeBand {
    /// The band of a `width x height` region: `min(EDGE_BAND_PX, min(width, height) / 4)` deep
    /// (0, an empty band, for a region under 4 px on a side).
    pub(crate) fn new(width: usize, height: usize) -> Self {
        let depth = EDGE_BAND_PX.min(width.min(height) / EDGE_BAND_SIDE_DIVISOR);
        Self { width, height, depth }
    }

    /// Region width, px.
    pub(crate) fn width(&self) -> usize {
        self.width
    }

    /// Band depth, px.
    #[cfg(test)]
    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    /// Whether pixel `(x, y)` lies in the band.
    pub(crate) fn contains(&self, x: usize, y: usize) -> bool {
        // `depth <= side / 4`, so `side - depth` never underflows.
        x < self.depth || y < self.depth || x >= self.width - self.depth || y >= self.height - self.depth
    }

    /// Whether row-major pixel `index` lies in the band (`width > 0`).
    pub(crate) fn contains_index(&self, index: usize) -> bool {
        self.contains(index % self.width.max(1), index / self.width.max(1))
    }

    /// Number of band pixels: the region minus its interior rectangle.
    pub(crate) fn count(&self) -> usize {
        let inner = (self.width - 2 * self.depth) * (self.height - 2 * self.depth);
        self.width * self.height - inner
    }

    /// The band as one `bool` per pixel, row-major (the local stage's sample weight).
    pub(crate) fn to_mask(self) -> Vec<bool> {
        (0..self.width * self.height).map(|index| self.contains_index(index)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_band_is_a_capped_frame() {
        let band = EdgeBand::new(128, 96);
        assert_eq!(band.depth(), 12);
        assert_eq!(band.count(), 128 * 96 - 104 * 72);
        assert_eq!(band.to_mask().iter().filter(|&&on| on).count(), band.count());
        assert!(band.contains(0, 50) && band.contains(11, 50) && !band.contains(12, 50) && band.contains(116, 50) && !band.contains(115, 50));
        assert!(band.contains(60, 84) && !band.contains(60, 83));
        // A quarter of the shorter side caps it.
        assert_eq!(EdgeBand::new(200, 20).depth(), 5);
        assert_eq!(EdgeBand::new(3, 300).count(), 0, "under 4 px a side there is no band");
    }
}
