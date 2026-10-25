/*
File: page_ops/plan/crop.rs

Purpose:
Crop geometry: `CropTreeRouting` and `CropGeometry` behaviour (what survives the
kept region) and the resolution of a crop request into its rotated canvas and
single rotating affine. The types themselves are declared in `plan.rs`.

Key functions:
- resolve_crop(): page size + rotation + rect -> validated `CropGeometry`.
- CropGeometry::keeps_*: the survival tests every remap of the page uses.

Notes:
A child of `plan.rs`; pure like its parent (no filesystem access).
*/

use super::split::{clip_half_plane, polygon_area};
use super::{CropGeometry, CropTreeRouting, PlacementMap, STITCH_MAX_TOTAL_PX};
use crate::crop_geometry::{PageRotation, RotatedPage};
use crate::{PageOpError, StitchPlacement};

impl CropTreeRouting {
    /// Builds a routing from the resolved sets (see
    /// `json_remap::crop_layer_routing`, the only production constructor).
    #[must_use]
    pub(crate) fn new(
        dropped_nodes: std::collections::HashSet<String>,
        dropped_files: std::collections::HashSet<String>,
    ) -> Self {
        Self {
            dropped_nodes,
            dropped_files,
        }
    }

    /// Whether the layer record `uid` falls entirely outside the kept region.
    #[must_use]
    pub(crate) fn is_dropped_node(&self, uid: &str) -> bool {
        self.dropped_nodes.contains(uid)
    }

    /// Whether `file` is referenced only by dropped records, so that it must be
    /// trashed instead of staying with the page.
    #[must_use]
    pub(crate) fn is_dropped_file(&self, file: &str) -> bool {
        self.dropped_files.contains(file)
    }
}

impl CropGeometry {
    /// Index of the cropped page, in the current order (also its new index).
    #[must_use]
    pub(crate) fn source_old_idx(&self) -> usize {
        self.source_old_idx
    }

    /// Pixel size `[w, h]` of the source page, as floats.
    #[must_use]
    pub(crate) fn page_size(&self) -> [f64; 2] {
        [f64::from(self.page_size[0]), f64::from(self.page_size[1])]
    }

    /// Pixel size `[w, h]` of the page AFTER the operation (the crop rect).
    #[must_use]
    pub(crate) fn new_size(&self) -> [u32; 2] {
        [self.rect[2], self.rect[3]]
    }

    /// The single affine of the operation: source page px -> new page px.
    #[must_use]
    pub(crate) fn placement(&self) -> &PlacementMap {
        &self.placement
    }

    /// The rotation applied before the crop.
    #[must_use]
    pub(crate) fn rotation(&self) -> PageRotation {
        self.placement.rotation()
    }

    /// Whether the rotation RESAMPLES (a non-zero fine angle) rather than
    /// permuting pixels.
    ///
    /// The single gate of every lossy degradation this operation applies: under
    /// a pure quarter turn an axis-aligned stored rectangle maps to an
    /// axis-aligned rectangle exactly and detection stays remappable, so none of
    /// them applies.
    #[must_use]
    pub(crate) fn resamples(&self) -> bool {
        self.placement.rotation().is_fine()
    }

    /// Whether the source-page point `(x, y)` (absolute page pixels) lands
    /// inside the kept region.
    ///
    /// Boundary-inclusive: a point exactly on the new page's edge is on the
    /// page, matching [`RotatedPage::validate_crop`]'s corner-based coordinates.
    #[must_use]
    pub(crate) fn keeps_point(&self, x: f64, y: f64) -> bool {
        let [w, h] = self.new_size();
        let (mx, my) = self.placement.map_point(x, y);
        (0.0..=f64::from(w)).contains(&mx) && (0.0..=f64::from(h)).contains(&my)
    }

    /// [`Self::keeps_point`] for a page-normalized `(u, v)` anchor.
    #[must_use]
    pub(crate) fn keeps_uv_point(&self, u: f64, v: f64) -> bool {
        let [w, h] = self.page_size();
        self.keeps_point(u * w, v * h)
    }

    /// Whether any part of a footprint made of several page-pixel polygon
    /// pieces survives the crop, or `None` when the footprint encloses no area
    /// at all (the caller must then fall back to a point test).
    ///
    /// The pieces are summed in ABSOLUTE area after clipping, exactly like
    /// [`SplitGeometry::part_for_polygon_group`](super::SplitGeometry::part_for_polygon_group): a deform mesh can be FOLDED,
    /// and the lobes of a self-intersecting ring cancel in a signed shoelace
    /// sum, so a single outer ring would answer for the wrong shape.
    #[must_use]
    pub(crate) fn keeps_polygon_group(&self, pieces: &[&[[f64; 2]]]) -> Option<bool> {
        let [w, h] = self.new_size();
        let (right, bottom) = (f64::from(w), f64::from(h));
        let mut total = 0.0_f64;
        let mut kept = 0.0_f64;
        for points in pieces {
            if points.len() < 3 {
                continue;
            }
            let mapped: Vec<[f64; 2]> = points
                .iter()
                .map(|point| {
                    let (x, y) = self.placement.map_point(point[0], point[1]);
                    [x, y]
                })
                .collect();
            total += polygon_area(&mapped);
            // Clip against the four edges of the kept region; the result's
            // absolute area is the part of this piece that survives.
            let clipped = clip_half_plane(&mapped, 0, 0.0, true);
            let clipped = clip_half_plane(&clipped, 0, right, false);
            let clipped = clip_half_plane(&clipped, 1, 0.0, true);
            let clipped = clip_half_plane(&clipped, 1, bottom, false);
            kept += polygon_area(&clipped);
        }
        (total > 0.0).then_some(kept > 0.0)
    }

    /// [`Self::keeps_polygon_group`] for a single simple ring.
    #[must_use]
    pub(crate) fn keeps_polygon(&self, points: &[[f64; 2]]) -> Option<bool> {
        self.keeps_polygon_group(&[points])
    }

    /// Whether an axis-aligned page-pixel rectangle `[x1, y1, x2, y2]` still
    /// overlaps the kept region; a degenerate rectangle degrades to its centre
    /// point, mirroring [`SplitGeometry::part_for_page_rect`](super::SplitGeometry::part_for_page_rect).
    #[must_use]
    pub(crate) fn keeps_page_rect(&self, rect: [f64; 4]) -> bool {
        let [x1, y1, x2, y2] = rect;
        let (left, right) = (x1.min(x2), x1.max(x2));
        let (top, bottom) = (y1.min(y2), y1.max(y2));
        let quad = [
            [left, top],
            [right, top],
            [right, bottom],
            [left, bottom],
        ];
        self.keeps_polygon(&quad)
            .unwrap_or_else(|| self.keeps_point((left + right) * 0.5, (top + bottom) * 0.5))
    }

    /// [`Self::keeps_page_rect`] for a page-normalized `[u1, v1, u2, v2]`.
    #[must_use]
    pub(crate) fn keeps_uv_rect(&self, rect: [f64; 4]) -> bool {
        let [w, h] = self.page_size();
        self.keeps_page_rect([rect[0] * w, rect[1] * h, rect[2] * w, rect[3] * h])
    }

    /// Dropped-layer routing of the tree rooted at `tree_rel`.
    #[must_use]
    pub(crate) fn routing(&self, tree_rel: &str) -> Option<&CropTreeRouting> {
        self.trees.get(tree_rel)
    }

    /// Drains the diagnostics collected while resolving what survives.
    pub(super) fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// Attaches a per-tree dropped-layer routing to a test geometry.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_routing(mut self, tree_rel: &str, routing: CropTreeRouting) -> Self {
        self.trees.insert(tree_rel.to_string(), routing);
        self
    }

    /// Builds a geometry directly, for tests of the JSON remaps (the normal
    /// path goes through `build_crop_geometry`, which needs a full snapshot).
    #[cfg(test)]
    pub(crate) fn for_tests(
        source_old_idx: usize,
        page_size: [u32; 2],
        quarter_turns: u8,
        angle_deg: f64,
        rect: [u32; 4],
    ) -> Self {
        resolve_crop(source_old_idx, page_size, quarter_turns, angle_deg, rect)
            .expect("valid test crop geometry")
    }
}

/// Resolves a crop request into its rotated canvas and its single affine.
///
/// `rect` is `[x, y, w, h]` in the ROTATED CANVAS' pixels; the resulting page
/// is exactly that size, so the placement has `scale = 1` and `dx = dy = 0` —
/// the same degenerate affine a split part uses, with a real rotation in front
/// of it.
///
/// # Errors
/// [`PageOpError::InvalidOp`] for a zero-sized page, `quarter_turns` outside
/// `0..=3`, a fine angle outside `(-45, 45)`, a rotated canvas that does not fit
/// `u32` or exceeds [`STITCH_MAX_TOTAL_PX`], or a crop rectangle that is empty
/// or leaves the canvas.
pub(super) fn resolve_crop(
    source_old_idx: usize,
    page_size: [u32; 2],
    quarter_turns: u8,
    angle_deg: f64,
    rect: [u32; 4],
) -> Result<CropGeometry, PageOpError> {
    let rotation = PageRotation::new(quarter_turns, angle_deg)?;
    let rotated = RotatedPage::new(page_size, rotation)?;
    let canvas = rotated.canvas_size();
    // Kept local: the plan needs the canvas only to bound the staging buffer and
    // to validate the rectangle — every consumer works in page or new-page
    // pixels, and re-deriving the canvas belongs to `RotatedPage`.
    // The rotated canvas becomes a staging RGBA buffer in the executor, so it
    // falls under the same memory bound as a stitched canvas.
    if u64::from(canvas[0]) * u64::from(canvas[1]) > STITCH_MAX_TOTAL_PX {
        return Err(PageOpError::InvalidOp(format!(
            "rotating page {source_old_idx} ({}x{}) by {} deg needs a {}x{} canvas, which \
             exceeds {STITCH_MAX_TOTAL_PX} pixels",
            page_size[0],
            page_size[1],
            rotation.total_degrees(),
            canvas[0],
            canvas[1]
        )));
    }
    rotated.validate_crop(rect, &format!("page {source_old_idx}"))?;
    let placement = PlacementMap::with_rotation(
        &StitchPlacement {
            page_idx: source_old_idx,
            crop: rect,
            scale: 1.0,
            dx: 0,
            dy: 0,
        },
        page_size,
        rotation,
        [rect[2], rect[3]],
    )?;
    Ok(CropGeometry {
        source_old_idx,
        page_size,
        rect,
        placement,
        trees: std::collections::BTreeMap::new(),
        warnings: Vec::new(),
    })
}
