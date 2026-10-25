/*
File: page_ops/plan/placement.rs

Purpose:
Behaviour of the stitch placement primitive: the `PlacementMap` affine (rotation,
crop, uniform scale, translation) shared by the stitch, the split and the crop,
and `StitchGeometry`'s accessors. The types themselves are declared in `plan.rs`.

Key functions:
- PlacementMap::new / with_rotation: validated construction of one page's map.
- PlacementMap::map_uv / map_point / map_extent / crop_rect: the per-space
  mappings every remap goes through.
- StitchGeometry::placement / layer_idx_offset: the stitch's per-page view.

Notes:
A child of `plan.rs`; pure like its parent (no filesystem access).
*/

use super::{PlacementMap, STITCH_MAX_OFFSET_PX, STITCH_MAX_SCALE, StitchGeometry};
use crate::crop_geometry::{PageRotation, RotatedPage};
use crate::{PageOpError, StitchPlacement};

impl PlacementMap {
    /// Validates one NON-ROTATING placement against its page and the canvas and
    /// returns the affine. See [`PlacementMap::with_rotation`] for the full
    /// contract; this is that function with [`PageRotation::IDENTITY`], which is
    /// what both pixel-identity operations request.
    ///
    /// # Errors
    /// As [`PlacementMap::with_rotation`].
    pub(crate) fn new(
        placement: &StitchPlacement,
        page_size: [u32; 2],
        canvas: [u32; 2],
    ) -> Result<Self, PageOpError> {
        Self::with_rotation(placement, page_size, PageRotation::IDENTITY, canvas)
    }

    /// Validates one placement of a page ROTATED by `rotation` against the
    /// canvas and returns the affine.
    ///
    /// `page_size` is the source page's own pixel size and `canvas` the output
    /// page's size. `placement.crop` is read in the pixel space of the ROTATED
    /// canvas of `page_size` under `rotation` — identical to the page's own
    /// space when the rotation is the identity. Validation is total: a returned
    /// map is guaranteed to place a non-empty rectangle fully inside the canvas
    /// from a non-empty crop fully inside its source space.
    ///
    /// # Errors
    /// [`PageOpError::InvalidOp`] for a zero-sized page or crop, a crop leaving
    /// the rotated canvas, a rotation whose canvas does not fit `u32`, a
    /// non-finite / out-of-range `scale`, an offset beyond
    /// `STITCH_MAX_OFFSET_PX`, or a placed rectangle leaving the canvas.
    pub(crate) fn with_rotation(
        placement: &StitchPlacement,
        page_size: [u32; 2],
        rotation: PageRotation,
        canvas: [u32; 2],
    ) -> Result<Self, PageOpError> {
        let idx = placement.page_idx;
        if page_size[0] == 0 || page_size[1] == 0 {
            return Err(PageOpError::InvalidOp(format!(
                "source page {idx} has a zero pixel size {}x{}",
                page_size[0], page_size[1]
            )));
        }
        let source = RotatedPage::new(page_size, rotation)?;
        let [cx, cy, cw, ch] = placement.crop;
        // The single crop-legality rule lives on `RotatedPage`; it is the same
        // one the UI preview uses, so the two cannot disagree.
        source.validate_crop(placement.crop, &format!("page {idx}"))?;
        if !placement.scale.is_finite()
            || placement.scale <= 0.0
            || placement.scale > STITCH_MAX_SCALE
        {
            return Err(PageOpError::InvalidOp(format!(
                "placement scale {} of page {idx} is outside (0, {STITCH_MAX_SCALE}]",
                placement.scale
            )));
        }
        // Range test rather than `abs()`: `i64::MIN.abs()` panics, and this
        // value comes straight from the UI.
        let offset_ok = -STITCH_MAX_OFFSET_PX..=STITCH_MAX_OFFSET_PX;
        if !offset_ok.contains(&placement.dx) || !offset_ok.contains(&placement.dy) {
            return Err(PageOpError::InvalidOp(format!(
                "placement offset ({}, {}) of page {idx} exceeds +-{STITCH_MAX_OFFSET_PX} px",
                placement.dx, placement.dy
            )));
        }
        // The bound above guarantees both offsets fit `i32`, so the conversion
        // to f64 is exact and needs no lossy `as`.
        let (Ok(dx_i32), Ok(dy_i32)) = (
            i32::try_from(placement.dx),
            i32::try_from(placement.dy),
        ) else {
            return Err(PageOpError::InvalidOp(format!(
                "placement offset ({}, {}) of page {idx} is not representable",
                placement.dx, placement.dy
            )));
        };
        let scale = f64::from(placement.scale);
        let dx = f64::from(dx_i32);
        let dy = f64::from(dy_i32);
        let placed_w = round_to_u32(f64::from(cw) * scale);
        let placed_h = round_to_u32(f64::from(ch) * scale);
        if placed_w == 0 || placed_h == 0 {
            return Err(PageOpError::InvalidOp(format!(
                "placement of page {idx} rounds to an empty rectangle"
            )));
        }
        if dx < 0.0
            || dy < 0.0
            || dx + f64::from(placed_w) > f64::from(canvas[0])
            || dy + f64::from(placed_h) > f64::from(canvas[1])
        {
            return Err(PageOpError::InvalidOp(format!(
                "placement of page {idx} ({placed_w}x{placed_h} at \
                 ({}, {})) leaves the {}x{} canvas",
                placement.dx, placement.dy, canvas[0], canvas[1]
            )));
        }
        // `dx`/`dy` are >= 0 and bounded by the canvas here, so the placed
        // origin is a valid u32.
        let placed_x = round_to_u32(dx);
        let placed_y = round_to_u32(dy);
        Ok(Self {
            source,
            crop_x: f64::from(cx),
            crop_y: f64::from(cy),
            crop_w: f64::from(cw),
            crop_h: f64::from(ch),
            scale,
            dx,
            dy,
            canvas_w: f64::from(canvas[0]),
            canvas_h: f64::from(canvas[1]),
            placed: [placed_x, placed_y, placed_w, placed_h],
        })
    }

    /// Maps an absolute POINT in the source page's pixels to canvas pixels.
    ///
    /// The one coordinate mapping of the placement: the rotation makes X depend
    /// on Y and vice versa, so the two axes can never be mapped apart (see
    /// [`PlacementMap::map_x_without_y`] for the only exception and its price).
    #[must_use]
    pub(crate) fn map_point(&self, x: f64, y: f64) -> (f64, f64) {
        let (rx, ry) = self.source.map_point(x, y);
        (
            (rx - self.crop_x) * self.scale + self.dx,
            (ry - self.crop_y) * self.scale + self.dy,
        )
    }

    /// Maps a page-normalized POINT (`u`, `v` over the source page's width and
    /// height) to a canvas-normalized point.
    #[must_use]
    pub(crate) fn map_uv(&self, u: f64, v: f64) -> (f64, f64) {
        let [page_w, page_h] = self.source.page_size();
        let (x, y) = self.map_point(u * f64::from(page_w), v * f64::from(page_h));
        (x / self.canvas_w, y / self.canvas_h)
    }

    /// Maps an absolute page-pixel X WITHOUT its Y, or `None` when the
    /// placement rotates.
    ///
    /// Only a half-specified stored point (a detection block carrying `x1` but
    /// no `y1`, a `img_u` with no `img_v`) needs this. A non-rotating placement
    /// is separable, so the answer is exact; a rotating one cannot map one axis
    /// without the other, and the caller must refuse or gate out such a
    /// document instead of guessing the missing coordinate.
    #[must_use]
    pub(crate) fn map_x_without_y(&self, x: f64) -> Option<f64> {
        self.source
            .is_identity()
            .then_some((x - self.crop_x) * self.scale + self.dx)
    }

    /// Maps an absolute page-pixel Y WITHOUT its X, or `None` when the
    /// placement rotates. See [`PlacementMap::map_x_without_y`].
    #[must_use]
    pub(crate) fn map_y_without_x(&self, y: f64) -> Option<f64> {
        self.source
            .is_identity()
            .then_some((y - self.crop_y) * self.scale + self.dy)
    }

    /// Maps a page-normalized U WITHOUT its V, or `None` when the placement
    /// rotates. See [`PlacementMap::map_x_without_y`].
    #[must_use]
    pub(crate) fn map_u_without_v(&self, u: f64) -> Option<f64> {
        let page_w = f64::from(self.source.page_size()[0]);
        self.map_x_without_y(u * page_w)
            .map(|x| x / self.canvas_w)
    }

    /// Maps a page-normalized V WITHOUT its U, or `None` when the placement
    /// rotates. See [`PlacementMap::map_x_without_y`].
    #[must_use]
    pub(crate) fn map_v_without_u(&self, v: f64) -> Option<f64> {
        let page_h = f64::from(self.source.page_size()[1]);
        self.map_y_without_x(v * page_h)
            .map(|y| y / self.canvas_h)
    }

    /// Maps a page-pixel MAGNITUDE — a length, a half-extent, or a stored
    /// page-size MULTIPLIER such as a layer transform's `scale` — to canvas
    /// pixels.
    ///
    /// Not a coordinate: it has no origin and no direction, so neither the
    /// crop's translation nor the rotation applies to it (a rotation is rigid
    /// and preserves lengths). Only the uniform scale does. A layer image sized
    /// by such a multiplier is never resampled, so its on-page extent must
    /// follow the placement.
    #[must_use]
    pub(crate) fn map_extent(&self, magnitude: f64) -> f64 {
        magnitude * self.scale
    }

    /// Maps an axis-aligned rectangle of ABSOLUTE page pixels, given as
    /// `[x1, y1, x2, y2]` (two opposite corners).
    ///
    /// An axis-aligned rectangle is not closed under rotation, so this is the
    /// one place where the mapping is allowed to be an approximation, and only
    /// when the placement actually rotates: the result is then the axis-aligned
    /// BOUNDING BOX of the mapped quad, normalized to `x1 <= x2, y1 <= y2`.
    /// Without a rotation the two given corners are mapped as they stand, which
    /// is exact and preserves the orientation of a stored inverted rectangle.
    #[must_use]
    pub(crate) fn map_px_rect(&self, rect: [f64; 4]) -> [f64; 4] {
        self.map_rect(rect, false)
    }

    /// Maps an axis-aligned page-normalized rectangle `[u1, v1, u2, v2]`.
    /// Same contract as [`PlacementMap::map_px_rect`].
    #[must_use]
    pub(crate) fn map_uv_rect(&self, rect: [f64; 4]) -> [f64; 4] {
        self.map_rect(rect, true)
    }

    /// Shared body of the two rectangle mappings; `normalized` selects the uv
    /// space over absolute page pixels.
    fn map_rect(&self, rect: [f64; 4], normalized: bool) -> [f64; 4] {
        let [a1, b1, a2, b2] = rect;
        let map = |x: f64, y: f64| {
            if normalized {
                self.map_uv(x, y)
            } else {
                self.map_point(x, y)
            }
        };
        let (x1, y1) = map(a1, b1);
        let (x2, y2) = map(a2, b2);
        if !self.rotates() {
            return [x1, y1, x2, y2];
        }
        // The other two corners only exist once the map rotates; before that
        // they are redundant and computing them would cost precision for
        // nothing.
        let (x3, y3) = map(a1, b2);
        let (x4, y4) = map(a2, b1);
        let xs = [x1, x2, x3, x4];
        let ys = [y1, y2, y3, y4];
        [
            xs.iter().copied().fold(f64::INFINITY, f64::min),
            ys.iter().copied().fold(f64::INFINITY, f64::min),
            xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            ys.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        ]
    }

    /// Whether this placement rotates its source page.
    ///
    /// The one question consumers currently need to ask: a `true` here means
    /// `crop_rect()` is read in the rotated canvas and the pixels must be
    /// rotated before being cropped. The rotation ITSELF is not exposed until an
    /// operation carries one.
    #[must_use]
    pub(crate) fn rotates(&self) -> bool {
        !self.source.is_identity()
    }

    /// The rotation this placement applies to its source page before cropping.
    ///
    /// Needed by the two consumers that must reproduce it outside this type: the
    /// pixel executor (which turns it into a [`ComposeSource::rotation`](super::ComposeSource::rotation) recipe)
    /// and the document remaps (which add its angle to every STORED angle of a
    /// page-placed object). [`PageRotation::IDENTITY`] for a stitch and a split.
    #[must_use]
    pub(crate) fn rotation(&self) -> PageRotation {
        self.source.rotation()
    }

    /// Total clockwise rotation of the placement in RADIANS, `0.0` when it does
    /// not rotate. See [`PageRotation::total_radians`].
    #[must_use]
    pub(crate) fn rotation_radians(&self) -> f64 {
        self.source.rotation().total_radians()
    }

    /// Total clockwise rotation of the placement in DEGREES, `0.0` when it does
    /// not rotate. See [`PageRotation::total_degrees`].
    #[must_use]
    pub(crate) fn rotation_degrees(&self) -> f64 {
        self.source.rotation().total_degrees()
    }

    /// The placed rectangle in canvas pixels, `[x, y, w, h]`.
    ///
    /// Exact and axis-aligned for every placement: the rotation happens BEFORE
    /// the crop, so what is placed is always an upright rectangle.
    #[must_use]
    pub(crate) fn placed_rect(&self) -> [u32; 4] {
        self.placed
    }

    /// The crop rectangle `[x, y, w, h]` in the placement's SOURCE space: the
    /// rotated canvas of the source page, which is the page's own pixel space
    /// whenever the placement does not rotate.
    ///
    /// A consumer that copies pixels (`ComposeSource`) must therefore rotate the
    /// page into that canvas first when [`PlacementMap::rotates`] is true; for
    /// the stitch and the split it is false and the rect indexes the page image
    /// directly.
    #[must_use]
    pub(crate) fn crop_rect(&self) -> [u32; 4] {
        [
            round_to_u32(self.crop_x),
            round_to_u32(self.crop_y),
            round_to_u32(self.crop_w),
            round_to_u32(self.crop_h),
        ]
    }
}

/// Rounds a non-negative, finite f64 to `u32`, saturating at both ends.
///
/// The `as` conversion is reached only for a value already proven finite and
/// inside `0 ..= u32::MAX`, where it is exact.
#[must_use]
fn round_to_u32(value: f64) -> u32 {
    let rounded = value.round();
    if !rounded.is_finite() || rounded <= 0.0 {
        return 0;
    }
    if rounded >= f64::from(u32::MAX) {
        return u32::MAX;
    }
    rounded as u32
}

impl StitchGeometry {
    /// The affine of `old_idx`, or `None` when that page is not stitched.
    #[must_use]
    pub(crate) fn placement(&self, old_idx: usize) -> Option<&PlacementMap> {
        self.placements.get(&old_idx)
    }

    /// Builds a geometry directly, for tests of the JSON remaps (the normal
    /// path goes through `build_stitch_geometry`, which needs a full snapshot).
    #[cfg(test)]
    pub(crate) fn for_tests(
        placements: Vec<(usize, PlacementMap)>,
        layer_idx_offsets: Vec<(usize, u32)>,
        primary_new: usize,
        canvas: [u32; 2],
    ) -> Self {
        Self {
            placements: placements.into_iter().collect(),
            layer_idx_offsets: layer_idx_offsets.into_iter().collect(),
            primary_new,
            canvas,
            background: [0, 0, 0, 0],
        }
    }

    /// Number of merged source pages.
    #[must_use]
    pub(crate) fn source_count(&self) -> usize {
        self.placements.len()
    }

    /// `layer_idx` offset of a source page; 0 for any other page.
    #[must_use]
    pub(crate) fn layer_idx_offset(&self, old_idx: usize) -> u32 {
        self.layer_idx_offsets.get(&old_idx).copied().unwrap_or(0)
    }
}
