/*
File: page_ops/plan/split.rs

Purpose:
Split geometry: `SplitTreeRouting` and `SplitGeometry` behaviour (part lookup
for points, rectangles and polygons by exact overlap area), the pure part
resolution of a split request and the one legality rule of its `order` /
`deleted` arrays. The types themselves are declared in `plan.rs`.

Key functions:
- resolve_split_parts(): cut list + routing -> validated part affines.
- validate_split_routing(): shared by the index math and the geometry builder.
- polygon_area() / clip_half_plane(): exact-area helpers, also used by the crop.

Notes:
A child of `plan.rs`; pure like its parent (no filesystem access).
*/

use super::{PlacementMap, SplitGeometry, SplitTreeRouting};
use crate::{PageOpError, SplitAxis, StitchPlacement};

impl SplitTreeRouting {
    /// Builds a routing from the resolved tables (see
    /// `json_remap::split_layer_routing`, the only production constructor).
    #[must_use]
    pub(crate) fn new(
        node_part: std::collections::HashMap<String, usize>,
        file_new_idx: std::collections::HashMap<String, usize>,
        deleted_files: std::collections::HashSet<String>,
    ) -> Self {
        Self {
            node_part,
            file_new_idx,
            deleted_files,
        }
    }

    /// Geometric part of the layer node `uid`, or `None` when the uid does not
    /// belong to the split page.
    #[must_use]
    pub(crate) fn node_part(&self, uid: &str) -> Option<usize> {
        self.node_part.get(uid).copied()
    }

    /// New page index of the layer PNG `file`, or `None` when the file is not
    /// owned by a node of the split page.
    #[must_use]
    pub(crate) fn file_new_idx(&self, file: &str) -> Option<usize> {
        self.file_new_idx.get(file).copied()
    }

    /// Whether `file` is referenced only by layer records of DELETED parts, so
    /// that it must be trashed instead of following a part.
    #[must_use]
    pub(crate) fn is_deleted_file(&self, file: &str) -> bool {
        self.deleted_files.contains(file)
    }
}

impl SplitGeometry {
    /// Index of the page being cut, in the current order.
    #[must_use]
    pub(crate) fn source_old_idx(&self) -> usize {
        self.source_old_idx
    }

    /// Pixel size `[w, h]` of the page being cut.
    #[must_use]
    pub(crate) fn page_size(&self) -> [f64; 2] {
        [f64::from(self.page_size[0]), f64::from(self.page_size[1])]
    }

    /// Geometric part containing a page-normalized `(u, v)` anchor.
    #[must_use]
    pub(crate) fn part_for_uv_point(&self, u: f64, v: f64) -> usize {
        let [w, h] = self.page_size();
        self.part_for_point(u * w, v * h)
    }

    /// Geometric part holding the largest share of a page-normalized
    /// `[u1, v1, u2, v2]` rectangle.
    #[must_use]
    pub(crate) fn part_for_uv_rect(&self, rect: [f64; 4]) -> usize {
        let [w, h] = self.page_size();
        self.part_for_page_rect([rect[0] * w, rect[1] * h, rect[2] * w, rect[3] * h])
    }

    /// Number of geometric parts (`cuts.len() + 1`).
    #[must_use]
    pub(crate) fn part_count(&self) -> usize {
        self.parts.len()
    }

    /// Affine of geometric part `part`.
    #[must_use]
    pub(crate) fn placement(&self, part: usize) -> Option<&PlacementMap> {
        self.parts.get(part)
    }

    /// Pixel size `[w, h]` of geometric part `part`.
    #[must_use]
    pub(crate) fn part_size(&self, part: usize) -> Option<[u32; 2]> {
        self.part_sizes.get(part).copied()
    }

    /// New-order index of geometric part `part`, or `None` when the part was
    /// DELETED by the request (or `part` is out of range).
    ///
    /// `None` is the engine-wide signal "nothing of this part becomes a page":
    /// every entry routed to it must be archived or trashed as
    /// [`super::PageOpKind::Delete`](crate::PageOpKind::Delete) would do it, never relocated onto a
    /// surviving neighbour.
    #[must_use]
    pub(crate) fn part_new_idx(&self, part: usize) -> Option<usize> {
        self.part_new_idx.get(part).copied().flatten()
    }

    /// Whether geometric part `part` was deleted by the request. An out-of-range
    /// part is not "deleted" — it does not exist.
    #[must_use]
    pub(crate) fn is_deleted_part(&self, part: usize) -> bool {
        matches!(self.part_new_idx.get(part), Some(None))
    }

    /// Number of parts that survive as pages (at least 1).
    #[must_use]
    pub(crate) fn kept_count(&self) -> usize {
        self.part_new_idx.iter().filter(|idx| idx.is_some()).count()
    }

    /// Geometric part that keeps the source page's index: the fallback target
    /// for anything of the cut page that could not be routed to a part.
    #[must_use]
    pub(crate) fn first_kept_part(&self) -> usize {
        self.first_kept_part
    }

    /// New-order index of [`Self::first_kept_part`]. Equal to the source page's
    /// own index: the surviving parts occupy the run starting there.
    #[must_use]
    pub(crate) fn first_kept_new_idx(&self) -> usize {
        self.source_old_idx
    }

    /// Layer routing of the tree rooted at `tree_rel`.
    #[must_use]
    pub(crate) fn routing(&self, tree_rel: &str) -> Option<&SplitTreeRouting> {
        self.trees.get(tree_rel)
    }

    /// Geometric part containing the page-pixel point `(x, y)`.
    ///
    /// Containment follows the part CROP rectangles exactly: a point sitting
    /// on a cut line belongs to the part that owns that pixel row/column, i.e.
    /// the lower / right one. Points outside the page clamp to the first or
    /// last part.
    #[must_use]
    pub(crate) fn part_for_point(&self, x: f64, y: f64) -> usize {
        let coord = match self.axis {
            SplitAxis::Horizontal => y,
            SplitAxis::Vertical => x,
        };
        // `bounds` has part_count + 1 entries; the last one is the page extent.
        for part in 0..self.parts.len() {
            if coord < self.bounds[part + 1] {
                return part;
            }
        }
        self.parts.len().saturating_sub(1)
    }

    /// Geometric part holding the LARGEST share of a polygon's area, in page
    /// pixels, or `None` when the polygon encloses no area.
    ///
    /// This is the exact form of the "the part holding 51% of it" rule: the
    /// polygon is clipped against each part's slab and compared by shoelace
    /// area, so a rotated or deformed layer is judged by what it really covers
    /// rather than by its bounding box. An exact tie resolves to the SMALLER
    /// part index — the top part for horizontal cuts, the left part for
    /// vertical ones — which is geometric position, not user order.
    ///
    /// The caller must pass a SIMPLE (non-self-intersecting) ring here; a
    /// footprint that can fold — a deform mesh — must go through
    /// [`Self::part_for_polygon_group`] instead, one piece per grid cell.
    #[must_use]
    pub(crate) fn part_for_polygon(&self, points: &[[f64; 2]]) -> Option<usize> {
        self.part_for_polygon_group(&[points])
    }

    /// Geometric part holding the largest share of a footprint made of SEVERAL
    /// pieces, whose per-part clipped areas are summed in ABSOLUTE value.
    ///
    /// This is the fold-correct form of [`Self::part_for_polygon`]. A signed
    /// shoelace sum over one outer ring cancels the lobes of a self-intersecting
    /// (folded) footprint, so the clipped area is not the filled area and the
    /// layer can route to the wrong part. A deform mesh is a regular grid of
    /// quads and a user CAN fold it in the typing tab, so its area is measured
    /// cell by cell: each cell is clipped against the part slab and its absolute
    /// area added, which stays correct however the cells overlap.
    ///
    /// Pieces with fewer than three points contribute nothing. Ties resolve
    /// exactly as in [`Self::part_for_polygon`].
    #[must_use]
    pub(crate) fn part_for_polygon_group(&self, pieces: &[&[[f64; 2]]]) -> Option<usize> {
        let axis_index = match self.axis {
            SplitAxis::Horizontal => 1,
            SplitAxis::Vertical => 0,
        };
        let mut areas = vec![0.0_f64; self.parts.len()];
        for points in pieces {
            if points.len() < 3 {
                continue;
            }
            for (part, area) in areas.iter_mut().enumerate() {
                let lower = clip_half_plane(points, axis_index, self.bounds[part], true);
                let slab = clip_half_plane(&lower, axis_index, self.bounds[part + 1], false);
                *area += polygon_area(&slab);
            }
        }
        let mut best: Option<(usize, f64)> = None;
        for (part, area) in areas.into_iter().enumerate() {
            // Strictly greater keeps the FIRST (topmost/leftmost) part on a tie.
            if best.is_none_or(|(_, best_area)| area > best_area) {
                best = Some((part, area));
            }
        }
        best.filter(|(_, area)| *area > 0.0).map(|(part, _)| part)
    }

    /// Geometric part holding the largest share of an axis-aligned page-pixel
    /// rectangle `[x1, y1, x2, y2]`; falls back to the rectangle's centre point
    /// when the rectangle is degenerate.
    #[must_use]
    pub(crate) fn part_for_page_rect(&self, rect: [f64; 4]) -> usize {
        let [x1, y1, x2, y2] = rect;
        let (left, right) = (x1.min(x2), x1.max(x2));
        let (top, bottom) = (y1.min(y2), y1.max(y2));
        let quad = [
            [left, top],
            [right, top],
            [right, bottom],
            [left, bottom],
        ];
        self.part_for_polygon(&quad).unwrap_or_else(|| {
            self.part_for_point((left + right) * 0.5, (top + bottom) * 0.5)
        })
    }

    /// Drains the diagnostics collected while resolving the routing.
    pub(super) fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    /// Attaches a per-tree layer routing to a test geometry.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_routing(mut self, tree_rel: &str, routing: SplitTreeRouting) -> Self {
        self.trees.insert(tree_rel.to_string(), routing);
        self
    }

    /// Builds a geometry directly, for tests of the JSON remaps (the normal
    /// path goes through `build_split_geometry`, which needs a full snapshot).
    ///
    /// `deleted` has one flag per part, exactly like the request field.
    #[cfg(test)]
    pub(crate) fn for_tests(
        source_old_idx: usize,
        axis: SplitAxis,
        page_size: [u32; 2],
        cuts: &[u32],
        order: &[usize],
        deleted: &[bool],
    ) -> Self {
        resolve_split_parts(source_old_idx, axis, page_size, cuts, order, deleted)
            .expect("valid test split geometry")
    }
}

/// Resolves the pure part geometry of a split (no tree routing yet).
///
/// `order[k]` is the position of geometric part `k` among ALL parts, deleted
/// ones included, and `deleted[k]` drops part `k` instead of turning it into a
/// page (see [`super::PageOpKind::Split`](crate::PageOpKind::Split)). The surviving parts are compacted
/// here: ranked by their `order` value among themselves, they take the
/// consecutive new indices `source_old_idx ..`, so the deleted parts leave no
/// gap in the new page order.
///
/// # Errors
/// [`PageOpError::InvalidOp`] for a zero-sized page, a cut list that is not
/// strictly increasing strictly inside the page, an `order` that is not a
/// permutation of the parts, a `deleted` of the wrong length, a request that
/// deletes every part, or a part that fails [`PlacementMap::new`].
pub(super) fn resolve_split_parts(
    source_old_idx: usize,
    axis: SplitAxis,
    page_size: [u32; 2],
    cuts: &[u32],
    order: &[usize],
    deleted: &[bool],
) -> Result<SplitGeometry, PageOpError> {
    if page_size[0] == 0 || page_size[1] == 0 {
        return Err(PageOpError::InvalidOp(format!(
            "split page {source_old_idx} has a zero pixel size {}x{}",
            page_size[0], page_size[1]
        )));
    }
    let extent = match axis {
        SplitAxis::Horizontal => page_size[1],
        SplitAxis::Vertical => page_size[0],
    };
    let mut previous = 0u32;
    for cut in cuts {
        // Strictly increasing and strictly inside the page is exactly the
        // condition "every part is at least 1 px".
        if *cut <= previous || *cut >= extent {
            return Err(PageOpError::InvalidOp(format!(
                "split cuts {cuts:?} of page {source_old_idx} are not strictly increasing \
                 inside (0, {extent}) along the cut axis"
            )));
        }
        previous = *cut;
    }
    let part_count = cuts.len() + 1;
    validate_split_routing(part_count, order, deleted)?;

    let mut bounds = Vec::with_capacity(part_count + 1);
    bounds.push(0u32);
    bounds.extend_from_slice(cuts);
    bounds.push(extent);

    // D5: the survivors take consecutive indices starting at the source page's
    // own, ranked by their `order` value among THEMSELVES — the deleted parts
    // keep their position in `order` but consume no index.
    let mut rank_of_part = vec![None; part_count];
    let mut survivors: Vec<usize> = (0..part_count).filter(|part| !deleted[*part]).collect();
    survivors.sort_by_key(|part| order[*part]);
    for (rank, part) in survivors.iter().enumerate() {
        rank_of_part[*part] = Some(source_old_idx + rank);
    }
    // Checked non-empty by `validate_split_routing`.
    let first_kept_part = survivors.first().copied().unwrap_or(0);

    let mut parts = Vec::with_capacity(part_count);
    let mut part_sizes = Vec::with_capacity(part_count);
    let mut part_new_idx = Vec::with_capacity(part_count);
    for part in 0..part_count {
        let start = bounds[part];
        let length = bounds[part + 1] - start;
        let (crop, canvas) = match axis {
            SplitAxis::Horizontal => (
                [0, start, page_size[0], length],
                [page_size[0], length],
            ),
            SplitAxis::Vertical => (
                [start, 0, length, page_size[1]],
                [length, page_size[1]],
            ),
        };
        let placement = StitchPlacement {
            page_idx: source_old_idx,
            crop,
            scale: 1.0,
            dx: 0,
            dy: 0,
        };
        parts.push(PlacementMap::new(&placement, page_size, canvas)?);
        part_sizes.push(canvas);
        part_new_idx.push(rank_of_part[part]);
    }

    Ok(SplitGeometry {
        source_old_idx,
        page_size,
        axis,
        bounds: bounds.into_iter().map(f64::from).collect(),
        parts,
        part_sizes,
        part_new_idx,
        first_kept_part,
        trees: std::collections::BTreeMap::new(),
        warnings: Vec::new(),
    })
}

/// Validates the per-part routing arrays of a split and returns how many parts
/// SURVIVE as pages.
///
/// `order` must be a permutation of `0..part_count` over ALL parts (deleted
/// ones keep their position, see [`super::PageOpKind::Split`](crate::PageOpKind::Split)) and `deleted`
/// must have one flag per part. Shared by [`resolve_split_parts`] and
/// [`split_permutation`](super::permutation::split_permutation), which validate the same request from two directions
/// (geometry and index math) and must never disagree about what is legal.
///
/// # Errors
/// [`PageOpError::InvalidOp`] for a length mismatch of either array, an order
/// position out of range or repeated, or a request that deletes every part —
/// that request is a [`super::PageOpKind::Delete`](crate::PageOpKind::Delete) of the page and must go
/// through it, so that "a chapter keeps at least one page" stays one rule in
/// one place.
pub(super) fn validate_split_routing(
    part_count: usize,
    order: &[usize],
    deleted: &[bool],
) -> Result<usize, PageOpError> {
    if order.len() != part_count {
        return Err(PageOpError::InvalidOp(format!(
            "split order has {} entr(ies) for {part_count} part(s)",
            order.len()
        )));
    }
    if deleted.len() != part_count {
        return Err(PageOpError::InvalidOp(format!(
            "split deletion flags have {} entr(ies) for {part_count} part(s)",
            deleted.len()
        )));
    }
    let mut seen = vec![false; part_count];
    for position in order {
        let slot = seen.get_mut(*position).ok_or_else(|| {
            PageOpError::InvalidOp(format!(
                "split order position {position} is out of range for {part_count} part(s)"
            ))
        })?;
        if *slot {
            return Err(PageOpError::InvalidOp(format!(
                "split order {order:?} lists position {position} more than once"
            )));
        }
        *slot = true;
    }
    let kept = deleted.iter().filter(|flag| !**flag).count();
    if kept == 0 {
        return Err(PageOpError::InvalidOp(
            "split cannot delete every part: use a page delete instead".to_string(),
        ));
    }
    Ok(kept)
}

/// Shoelace area of a simple polygon, in the units of its coordinates. An
/// empty or degenerate ring yields 0.
#[must_use]
pub(super) fn polygon_area(points: &[[f64; 2]]) -> f64 {
    if points.len() < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for (index, point) in points.iter().enumerate() {
        let next = points[(index + 1) % points.len()];
        sum += point[0] * next[1] - next[0] * point[1];
    }
    (sum * 0.5).abs()
}

/// Sutherland-Hodgman clip of a polygon against ONE axis-aligned half-plane.
///
/// `axis_index` selects the coordinate (0 = x, 1 = y); `keep_greater` keeps the
/// side `coord >= limit`, otherwise `coord <= limit`. The clip region is convex
/// (a half-plane), so the signed area of the result is exact even for a concave
/// subject polygon such as a deform mesh's boundary ring — the degenerate edges
/// the algorithm may introduce there contribute zero area.
#[must_use]
pub(super) fn clip_half_plane(
    points: &[[f64; 2]],
    axis_index: usize,
    limit: f64,
    keep_greater: bool,
) -> Vec<[f64; 2]> {
    if points.is_empty() {
        return Vec::new();
    }
    let inside = |point: &[f64; 2]| {
        if keep_greater {
            point[axis_index] >= limit
        } else {
            point[axis_index] <= limit
        }
    };
    let intersect = |a: &[f64; 2], b: &[f64; 2]| {
        let delta = b[axis_index] - a[axis_index];
        // Both endpoints on the boundary line: no unique crossing, take `b`.
        if delta.abs() <= f64::EPSILON {
            return *b;
        }
        let t = (limit - a[axis_index]) / delta;
        [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
    };
    let mut out = Vec::with_capacity(points.len() + 4);
    for (index, current) in points.iter().enumerate() {
        let previous = &points[(index + points.len() - 1) % points.len()];
        let (current_in, previous_in) = (inside(current), inside(previous));
        if current_in {
            if !previous_in {
                out.push(intersect(previous, current));
            }
            out.push(*current);
        } else if previous_in {
            out.push(intersect(previous, current));
        }
    }
    out
}
