/*
File: page_ops/crop_geometry.rs

Purpose:
Pure, GUI-free geometry of a page ROTATION: the quarter turn plus the fine
straightening angle that is applied to ONE page before a crop rectangle is
taken from it. It owns the canonical model - canvas size, the forward and
inverse point mappings, and crop-rectangle legality - so that the engine
(`plan.rs`) and the page-manager UI share one set of formulas instead of each
restating them.

Main responsibilities:
- validate a rotation request (quarter turns in `0..=3`, fine angle in
  `(-45, 45)` degrees);
- compute the ROTATED CANVAS: the axis-aligned bounding box of the rotated page
  rectangle, with the rotated page centred inside it;
- map points both ways between source-page pixels and rotated-canvas pixels;
- judge whether a crop rectangle fits the rotated canvas.

Key structures:
- QuarterTurns: the four exact clockwise 90-degree steps.
- PageRotation: quarter turns + fine angle, validated.
- RotatedPage: a validated (page size, rotation) pair carrying its canvas size
  and both point mappings.

Key functions:
- PageRotation::new(): validated rotation request.
- RotatedPage::new(): validated page + rotation, computes the canvas size.
- RotatedPage::map_point() / RotatedPage::unmap_point(): the two mappings.
- RotatedPage::validate_crop(): legality of a crop rect in canvas pixels.

Notes:
Coordinates are CORNER-based and continuous: the source page occupies
`[0, W] x [0, H]`, exactly like `PlacementMap`'s crop rectangle. The identity
rotation and every pure quarter turn are EXACT - they are derived from the
integer page size by swapping and subtracting, never from `cos`/`sin` of a
float - so a quarter-turn crop stays bit-exact. Only a non-zero fine angle
introduces floating-point rounding. No I/O and no pixel data here: moving the
pixels is `fs_exec`'s job.
*/


use super::PageOpError;

/// Exclusive bound on the fine straightening angle, in degrees.
///
/// A larger straightening angle is a quarter turn plus a smaller angle, so the
/// half-open range keeps the (quarter turns, angle) decomposition unique.
pub(crate) const MAX_FINE_ANGLE_DEG: f64 = 45.0;

/// Number of exact CLOCKWISE 90-degree steps applied to a page.
///
/// A dedicated enum rather than a raw `u8` so that every consumer's `match` is
/// exhaustive and an out-of-range value cannot reach the geometry at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuarterTurns {
    /// No turn.
    Zero,
    /// 90 degrees clockwise; the page's width and height swap.
    Cw90,
    /// 180 degrees.
    Cw180,
    /// 270 degrees clockwise (= 90 counter-clockwise); width and height swap.
    Cw270,
}

impl QuarterTurns {
    /// `steps` clockwise quarter turns, or `None` when `steps` is not `0..=3`.
    #[must_use]
    pub(crate) fn from_steps(steps: u8) -> Option<Self> {
        match steps {
            0 => Some(Self::Zero),
            1 => Some(Self::Cw90),
            2 => Some(Self::Cw180),
            3 => Some(Self::Cw270),
            _ => None,
        }
    }

    /// The number of clockwise quarter turns, `0..=3`.
    #[must_use]
    pub(crate) fn steps(self) -> u8 {
        match self {
            Self::Zero => 0,
            Self::Cw90 => 1,
            Self::Cw180 => 2,
            Self::Cw270 => 3,
        }
    }

    /// Whether the turn exchanges the page's width and height.
    #[must_use]
    pub(crate) fn swaps_axes(self) -> bool {
        match self {
            Self::Zero | Self::Cw180 => false,
            Self::Cw90 | Self::Cw270 => true,
        }
    }
}

/// A page rotation: exact quarter turns plus a fine straightening angle.
///
/// The angle is applied AFTER the quarter turns, about the centre of the
/// quarter-turned page, and is clockwise-positive in the image's y-down pixel
/// coordinates. Built only through [`PageRotation::new`] (or
/// [`PageRotation::IDENTITY`]), so a value of this type is always inside the
/// documented ranges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PageRotation {
    quarter: QuarterTurns,
    angle_deg: f64,
}

impl PageRotation {
    /// The rotation that changes nothing.
    pub(crate) const IDENTITY: Self = Self {
        quarter: QuarterTurns::Zero,
        angle_deg: 0.0,
    };

    /// Validates a rotation request.
    ///
    /// `quarter_turns` counts CLOCKWISE 90-degree steps and must be `0..=3`;
    /// `angle_deg` is the fine straightening angle, clockwise-positive, and
    /// must be finite and strictly inside `(-45, 45)`.
    ///
    /// # Errors
    /// [`PageOpError::InvalidOp`] when either value is outside its range.
    pub(crate) fn new(quarter_turns: u8, angle_deg: f64) -> Result<Self, PageOpError> {
        let Some(quarter) = QuarterTurns::from_steps(quarter_turns) else {
            return Err(PageOpError::InvalidOp(format!(
                "page rotation quarter_turns {quarter_turns} is outside 0..=3"
            )));
        };
        if !angle_deg.is_finite()
            || angle_deg <= -MAX_FINE_ANGLE_DEG
            || angle_deg >= MAX_FINE_ANGLE_DEG
        {
            return Err(PageOpError::InvalidOp(format!(
                "page rotation angle {angle_deg} deg is outside \
                 (-{MAX_FINE_ANGLE_DEG}, {MAX_FINE_ANGLE_DEG})"
            )));
        }
        Ok(Self { quarter, angle_deg })
    }

    /// The exact quarter-turn component.
    #[must_use]
    pub(crate) fn quarter(self) -> QuarterTurns {
        self.quarter
    }

    /// The fine straightening angle in degrees, clockwise-positive.
    #[must_use]
    pub(crate) fn angle_deg(self) -> f64 {
        self.angle_deg
    }

    /// Whether this rotation maps every point onto itself.
    ///
    /// True only for zero quarter turns AND an exactly zero angle: that is the
    /// case in which every mapping below is the identity bit-for-bit, which the
    /// pixel-identity operations rely on.
    #[must_use]
    pub(crate) fn is_identity(self) -> bool {
        matches!(self.quarter, QuarterTurns::Zero) && self.angle_deg == 0.0
    }

    /// Whether the fine straightening angle is non-zero, i.e. whether this
    /// rotation RESAMPLES pixels instead of permuting them.
    ///
    /// The single question every lossy-degradation decision keys on: an
    /// axis-aligned stored rectangle survives a pure quarter turn exactly and
    /// can only be bounding-boxed under a fine angle.
    #[must_use]
    pub(crate) fn is_fine(self) -> bool {
        self.angle_deg != 0.0
    }

    /// Total clockwise rotation applied to the page, in DEGREES.
    ///
    /// This is the value a STORED angle of a page-placed object must gain: a
    /// `layers.json` `transform.rotation` (after [`Self::total_radians`]) or a
    /// `text_info` `rotation_deg` keeps describing the same visual orientation
    /// only if it turns with the page. Exactly `0.0` for the identity.
    #[must_use]
    pub(crate) fn total_degrees(self) -> f64 {
        f64::from(self.quarter.steps()) * 90.0 + self.angle_deg
    }

    /// [`Self::total_degrees`] in RADIANS, the unit `layers.json` stores.
    #[must_use]
    pub(crate) fn total_radians(self) -> f64 {
        self.total_degrees().to_radians()
    }
}

/// One page plus the rotation applied to it, with the resulting canvas.
///
/// The ROTATED CANVAS is the axis-aligned bounding box of the rotated page
/// rectangle, its size rounded UP to whole pixels, with the rotated page
/// centred inside it. Every crop rectangle of a rotating operation lives in
/// this canvas' pixel space, not in the source page's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RotatedPage {
    /// Source page size `[w, h]` in pixels.
    page: [u32; 2],
    rotation: PageRotation,
    /// Page size after the quarter turns alone, before the fine angle.
    turned: [u32; 2],
    /// Rotated-canvas size `[w, h]` in pixels.
    canvas: [u32; 2],
    /// `cos` / `sin` of the fine angle; exactly `1.0` / `0.0` when it is zero,
    /// in which case neither is ever used (the mappings short-circuit).
    cos: f64,
    sin: f64,
}

impl RotatedPage {
    /// Validates a page size against a rotation and computes the canvas.
    ///
    /// `page_size` is `[w, h]` in pixels and must be non-empty.
    ///
    /// # Errors
    /// [`PageOpError::InvalidOp`] for a zero-sized page, or when the rotated
    /// canvas would not fit `u32`.
    pub(crate) fn new(page_size: [u32; 2], rotation: PageRotation) -> Result<Self, PageOpError> {
        if page_size[0] == 0 || page_size[1] == 0 {
            return Err(PageOpError::InvalidOp(format!(
                "cannot rotate a page of zero pixel size {}x{}",
                page_size[0], page_size[1]
            )));
        }
        let turned = if rotation.quarter.swaps_axes() {
            [page_size[1], page_size[0]]
        } else {
            page_size
        };
        // A zero angle keeps the canvas EXACTLY the quarter-turned page, which
        // is what makes a quarter-turn crop a lossless pixel permutation.
        if rotation.angle_deg == 0.0 {
            return Ok(Self {
                page: page_size,
                rotation,
                turned,
                canvas: turned,
                cos: 1.0,
                sin: 0.0,
            });
        }
        let (sin, cos) = rotation.angle_deg.to_radians().sin_cos();
        let w = f64::from(turned[0]);
        let h = f64::from(turned[1]);
        let (abs_cos, abs_sin) = (cos.abs(), sin.abs());
        let (Some(canvas_w), Some(canvas_h)) = (
            ceil_to_u32(w * abs_cos + h * abs_sin),
            ceil_to_u32(w * abs_sin + h * abs_cos),
        ) else {
            return Err(PageOpError::InvalidOp(format!(
                "a {}x{} page rotated by {} deg does not fit a u32 canvas",
                page_size[0], page_size[1], rotation.angle_deg
            )));
        };
        Ok(Self {
            page: page_size,
            rotation,
            turned,
            canvas: [canvas_w, canvas_h],
            cos,
            sin,
        })
    }

    /// The source page size `[w, h]` in pixels.
    #[must_use]
    pub(crate) fn page_size(&self) -> [u32; 2] {
        self.page
    }

    /// The rotated-canvas size `[w, h]` in pixels.
    #[must_use]
    pub(crate) fn canvas_size(&self) -> [u32; 2] {
        self.canvas
    }

    /// The rotation this page is mapped through.
    #[must_use]
    pub(crate) fn rotation(&self) -> PageRotation {
        self.rotation
    }

    /// Whether the mapping is the identity (see [`PageRotation::is_identity`]).
    #[must_use]
    pub(crate) fn is_identity(&self) -> bool {
        self.rotation.is_identity()
    }

    /// Maps a source-page pixel point to rotated-canvas pixels.
    ///
    /// Coordinates are corner-based and continuous: the page spans
    /// `[0, w] x [0, h]`. For an identity rotation the returned pair is the
    /// input bit-for-bit; for a pure quarter turn it is exact integer
    /// arithmetic on integer inputs.
    #[must_use]
    pub(crate) fn map_point(&self, x: f64, y: f64) -> (f64, f64) {
        let (tx, ty) = self.turn_point(x, y);
        if self.rotation.angle_deg == 0.0 {
            return (tx, ty);
        }
        // Rotate about the quarter-turned page's centre, then re-centre on the
        // canvas. In y-down pixel coordinates the standard rotation matrix
        // turns the content CLOCKWISE, which is the documented sign.
        let u = tx - f64::from(self.turned[0]) / 2.0;
        let v = ty - f64::from(self.turned[1]) / 2.0;
        (
            u * self.cos - v * self.sin + f64::from(self.canvas[0]) / 2.0,
            u * self.sin + v * self.cos + f64::from(self.canvas[1]) / 2.0,
        )
    }

    /// Maps a rotated-canvas pixel point back to source-page pixels.
    ///
    /// The exact inverse of [`RotatedPage::map_point`] (up to floating-point
    /// rounding when the fine angle is non-zero). The result may fall outside
    /// the page: the canvas corners are not covered by a rotated page.
    #[must_use]
    pub(crate) fn unmap_point(&self, x: f64, y: f64) -> (f64, f64) {
        if self.rotation.angle_deg == 0.0 {
            return self.unturn_point(x, y);
        }
        let u = x - f64::from(self.canvas[0]) / 2.0;
        let v = y - f64::from(self.canvas[1]) / 2.0;
        let tx = u * self.cos + v * self.sin + f64::from(self.turned[0]) / 2.0;
        let ty = -u * self.sin + v * self.cos + f64::from(self.turned[1]) / 2.0;
        self.unturn_point(tx, ty)
    }

    /// Checks that `rect` (`[x, y, w, h]`, rotated-canvas pixels) is a legal
    /// crop: non-empty and fully inside the canvas.
    ///
    /// This is the SINGLE legality rule for a crop rectangle; both
    /// `PlacementMap` and the UI preview call it instead of restating the
    /// bounds. `subject` names the thing being cropped in the error message
    /// (`"page 3"`, `"the crop request"`). For a non-rotating page the canvas
    /// is the page image itself.
    ///
    /// # Errors
    /// [`PageOpError::InvalidOp`] for a zero-sized rectangle or one that leaves
    /// the canvas (an overflowing `x + w` counts as leaving it).
    pub(crate) fn validate_crop(&self, rect: [u32; 4], subject: &str) -> Result<(), PageOpError> {
        let [x, y, w, h] = rect;
        if w == 0 || h == 0 {
            return Err(PageOpError::InvalidOp(format!(
                "crop [{x}, {y}, {w}, {h}] of {subject} is empty"
            )));
        }
        let right = x.checked_add(w);
        let bottom = y.checked_add(h);
        if right.is_none_or(|r| r > self.canvas[0]) || bottom.is_none_or(|b| b > self.canvas[1]) {
            return Err(PageOpError::InvalidOp(format!(
                "crop [{x}, {y}, {w}, {h}] of {subject} leaves its {}x{} source canvas",
                self.canvas[0], self.canvas[1]
            )));
        }
        Ok(())
    }

    /// Applies the quarter turns alone: source-page pixels -> quarter-turned
    /// page pixels. Exact for integer inputs (swap and subtract only).
    fn turn_point(&self, x: f64, y: f64) -> (f64, f64) {
        let w = f64::from(self.page[0]);
        let h = f64::from(self.page[1]);
        match self.rotation.quarter {
            QuarterTurns::Zero => (x, y),
            QuarterTurns::Cw90 => (h - y, x),
            QuarterTurns::Cw180 => (w - x, h - y),
            QuarterTurns::Cw270 => (y, w - x),
        }
    }

    /// Inverse of [`RotatedPage::turn_point`].
    fn unturn_point(&self, x: f64, y: f64) -> (f64, f64) {
        let w = f64::from(self.page[0]);
        let h = f64::from(self.page[1]);
        match self.rotation.quarter {
            QuarterTurns::Zero => (x, y),
            QuarterTurns::Cw90 => (y, h - x),
            QuarterTurns::Cw180 => (w - x, h - y),
            QuarterTurns::Cw270 => (w - y, x),
        }
    }
}

/// Rounds a finite, non-negative `f64` UP to a `u32`, or `None` when it is
/// negative, not finite, or above `u32::MAX`.
///
/// The `as` conversion is reached only for a value already proven to be a
/// non-negative integer inside `0 ..= u32::MAX`, where it is exact.
#[must_use]
fn ceil_to_u32(value: f64) -> Option<u32> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let ceiled = value.ceil();
    if ceiled > f64::from(u32::MAX) {
        return None;
    }
    Some(ceiled as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a rotation, panicking with the rejection reason on bad input.
    fn rot(quarter_turns: u8, angle_deg: f64) -> PageRotation {
        PageRotation::new(quarter_turns, angle_deg).expect("rotation is inside its ranges")
    }

    /// Builds a rotated page, panicking with the rejection reason.
    fn page(size: [u32; 2], quarter_turns: u8, angle_deg: f64) -> RotatedPage {
        RotatedPage::new(size, rot(quarter_turns, angle_deg)).expect("page rotation is valid")
    }

    #[test]
    fn rotation_rejects_out_of_range_requests() {
        assert!(PageRotation::new(4, 0.0).is_err());
        assert!(PageRotation::new(255, 0.0).is_err());
        assert!(PageRotation::new(0, 45.0).is_err());
        assert!(PageRotation::new(0, -45.0).is_err());
        assert!(PageRotation::new(0, f64::NAN).is_err());
        assert!(PageRotation::new(0, f64::INFINITY).is_err());
        assert!(PageRotation::new(3, 44.999).is_ok());
        assert!(PageRotation::new(0, -44.999).is_ok());
    }

    #[test]
    fn identity_is_only_zero_turns_and_zero_angle() {
        assert!(PageRotation::IDENTITY.is_identity());
        assert!(rot(0, 0.0).is_identity());
        assert!(!rot(2, 0.0).is_identity());
        assert!(!rot(0, 0.5).is_identity());
        assert_eq!(PageRotation::IDENTITY.quarter().steps(), 0);
        assert_eq!(rot(3, 1.0).quarter(), QuarterTurns::Cw270);
        assert!((rot(3, 1.0).angle_deg() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn total_angle_sums_the_quarter_turns_and_the_fine_angle() {
        // Exactly zero for the identity: a stored angle must not be rewritten
        // by a non-rotating placement.
        assert!(PageRotation::IDENTITY.total_degrees() == 0.0);
        assert!(PageRotation::IDENTITY.total_radians() == 0.0);
        assert!(!PageRotation::IDENTITY.is_fine());
        // A pure quarter turn is exact and is NOT a fine (resampling) rotation.
        assert!((rot(1, 0.0).total_degrees() - 90.0).abs() < f64::EPSILON);
        assert!((rot(3, 0.0).total_degrees() - 270.0).abs() < f64::EPSILON);
        assert!(!rot(2, 0.0).is_fine());
        // The fine angle adds on top, with its sign.
        assert!((rot(2, 7.5).total_degrees() - 187.5).abs() < 1e-12);
        assert!((rot(0, -7.5).total_degrees() + 7.5).abs() < 1e-12);
        assert!(rot(0, -7.5).is_fine());
        assert!((rot(1, 0.0).total_radians() - std::f64::consts::FRAC_PI_2).abs() < 1e-12);
    }

    #[test]
    fn quarter_turns_swap_the_canvas_only_on_odd_steps() {
        assert_eq!(page([100, 200], 0, 0.0).canvas_size(), [100, 200]);
        assert_eq!(page([100, 200], 1, 0.0).canvas_size(), [200, 100]);
        assert_eq!(page([100, 200], 2, 0.0).canvas_size(), [100, 200]);
        assert_eq!(page([100, 200], 3, 0.0).canvas_size(), [200, 100]);
    }

    #[test]
    fn identity_mapping_is_bit_exact() {
        let map = page([100, 200], 0, 0.0);
        assert!(map.is_identity());
        for (x, y) in [(0.0, 0.0), (37.25, 191.5), (100.0, 200.0), (-3.0, 7.5)] {
            assert_eq!(map.map_point(x, y), (x, y));
            assert_eq!(map.unmap_point(x, y), (x, y));
        }
    }

    #[test]
    fn quarter_turns_move_the_page_corners_clockwise() {
        // Page corners, clockwise from the top-left, of a 100x200 page.
        let corners = [(0.0, 0.0), (100.0, 0.0), (100.0, 200.0), (0.0, 200.0)];
        // 90 CW: top-left becomes the canvas' top-RIGHT corner (canvas 200x100).
        let cw90 = page([100, 200], 1, 0.0);
        assert_eq!(cw90.map_point(corners[0].0, corners[0].1), (200.0, 0.0));
        assert_eq!(cw90.map_point(corners[1].0, corners[1].1), (200.0, 100.0));
        assert_eq!(cw90.map_point(corners[2].0, corners[2].1), (0.0, 100.0));
        assert_eq!(cw90.map_point(corners[3].0, corners[3].1), (0.0, 0.0));
        // 180: top-left becomes bottom-right (canvas unchanged).
        let cw180 = page([100, 200], 2, 0.0);
        assert_eq!(cw180.map_point(0.0, 0.0), (100.0, 200.0));
        assert_eq!(cw180.map_point(100.0, 200.0), (0.0, 0.0));
        // 270 CW: top-left becomes the bottom-left (canvas 200x100).
        let cw270 = page([100, 200], 3, 0.0);
        assert_eq!(cw270.map_point(corners[0].0, corners[0].1), (0.0, 100.0));
        assert_eq!(cw270.map_point(corners[1].0, corners[1].1), (0.0, 0.0));
        assert_eq!(cw270.map_point(corners[2].0, corners[2].1), (200.0, 0.0));
        assert_eq!(cw270.map_point(corners[3].0, corners[3].1), (200.0, 100.0));
    }

    #[test]
    fn quarter_turn_round_trip_is_exact() {
        for steps in 0..=3u8 {
            let map = page([100, 200], steps, 0.0);
            for (x, y) in [(0.0, 0.0), (17.0, 3.0), (100.0, 200.0), (63.5, 128.25)] {
                let (cx, cy) = map.map_point(x, y);
                // Bit-exact, not approximate: a quarter turn is a pixel
                // permutation and must never drift.
                assert_eq!(map.unmap_point(cx, cy), (x, y), "steps {steps}");
            }
        }
    }

    #[test]
    fn quarter_turns_keep_every_page_point_inside_the_canvas() {
        for steps in 0..=3u8 {
            let map = page([100, 200], steps, 0.0);
            let [cw, ch] = map.canvas_size();
            for x in [0.0, 50.0, 100.0] {
                for y in [0.0, 100.0, 200.0] {
                    let (mx, my) = map.map_point(x, y);
                    assert!(
                        (0.0..=f64::from(cw)).contains(&mx) && (0.0..=f64::from(ch)).contains(&my),
                        "steps {steps}: ({x}, {y}) -> ({mx}, {my}) outside {cw}x{ch}"
                    );
                }
            }
        }
    }

    #[test]
    fn fine_angle_canvas_matches_the_hand_computed_bounding_box() {
        // 100x200 at 30 deg: 100*cos30 + 200*sin30 = 186.60 -> 187,
        //                    100*sin30 + 200*cos30 = 223.21 -> 224.
        assert_eq!(page([100, 200], 0, 30.0).canvas_size(), [187, 224]);
        // The sign of the angle does not change the bounding box.
        assert_eq!(page([100, 200], 0, -30.0).canvas_size(), [187, 224]);
        // A quarter turn first: the 200x100 turned page gives the transpose.
        assert_eq!(page([100, 200], 1, 30.0).canvas_size(), [224, 187]);
        // 40x40 at 44 deg: 40*(cos44 + sin44) = 56.56 -> 57 on both axes.
        assert_eq!(page([40, 40], 0, 44.0).canvas_size(), [57, 57]);
    }

    #[test]
    fn fine_angle_rotates_clockwise_about_the_centre() {
        let map = page([100, 200], 0, 30.0);
        let [cw, ch] = map.canvas_size();
        // The page centre lands on the canvas centre.
        let (mx, my) = map.map_point(50.0, 100.0);
        assert!((mx - f64::from(cw) / 2.0).abs() < 1e-9);
        assert!((my - f64::from(ch) / 2.0).abs() < 1e-9);
        // A point to the RIGHT of the centre moves DOWN under a clockwise
        // rotation of the content (y grows downwards).
        let (rx, ry) = map.map_point(100.0, 100.0);
        assert!(rx > mx, "{rx} should stay right of the centre");
        assert!(ry > my, "{ry} should move below the centre");
    }

    #[test]
    fn fine_angle_round_trip_returns_the_source_point() {
        for (steps, angle) in [(0u8, 12.5), (1, -7.25), (2, 44.0), (3, 0.75)] {
            let map = page([317, 511], steps, angle);
            for (x, y) in [(0.0, 0.0), (317.0, 511.0), (12.5, 480.0), (200.0, 3.5)] {
                let (cx, cy) = map.map_point(x, y);
                let (bx, by) = map.unmap_point(cx, cy);
                assert!(
                    (bx - x).abs() < 1e-6 && (by - y).abs() < 1e-6,
                    "steps {steps} angle {angle}: ({x}, {y}) -> ({cx}, {cy}) -> ({bx}, {by})"
                );
            }
        }
    }

    #[test]
    fn fine_angle_keeps_the_rotated_page_inside_the_canvas() {
        let map = page([317, 511], 1, 17.5);
        let [cw, ch] = map.canvas_size();
        for (x, y) in [(0.0, 0.0), (317.0, 0.0), (317.0, 511.0), (0.0, 511.0)] {
            let (mx, my) = map.map_point(x, y);
            assert!(
                mx >= -1e-9 && my >= -1e-9 && mx <= f64::from(cw) && my <= f64::from(ch),
                "corner ({x}, {y}) -> ({mx}, {my}) outside {cw}x{ch}"
            );
        }
    }

    #[test]
    fn crop_validation_accepts_the_boundaries_and_rejects_the_rest() {
        let map = page([100, 200], 1, 0.0);
        assert_eq!(map.canvas_size(), [200, 100]);
        assert!(map.validate_crop([0, 0, 200, 100], "the test page").is_ok());
        assert!(map.validate_crop([199, 99, 1, 1], "the test page").is_ok());
        assert!(map.validate_crop([0, 0, 0, 100], "the test page").is_err());
        assert!(map.validate_crop([0, 0, 200, 0], "the test page").is_err());
        assert!(map.validate_crop([1, 0, 200, 100], "the test page").is_err());
        assert!(map.validate_crop([0, 1, 200, 100], "the test page").is_err());
        assert!(map.validate_crop([u32::MAX, 0, 2, 2], "the test page").is_err());
        assert!(map.validate_crop([0, u32::MAX, 2, 2], "the test page").is_err());
    }

    #[test]
    fn zero_sized_pages_are_rejected() {
        assert!(RotatedPage::new([0, 10], PageRotation::IDENTITY).is_err());
        assert!(RotatedPage::new([10, 0], PageRotation::IDENTITY).is_err());
        assert!(RotatedPage::new([1, 1], PageRotation::IDENTITY).is_ok());
    }

    #[test]
    fn ceil_to_u32_saturates_nothing_and_rejects_the_impossible() {
        assert_eq!(ceil_to_u32(0.0), Some(0));
        assert_eq!(ceil_to_u32(0.25), Some(1));
        assert_eq!(ceil_to_u32(7.0), Some(7));
        assert_eq!(ceil_to_u32(f64::from(u32::MAX)), Some(u32::MAX));
        assert_eq!(ceil_to_u32(f64::from(u32::MAX) + 1.0), None);
        assert_eq!(ceil_to_u32(-0.5), None);
        assert_eq!(ceil_to_u32(f64::NAN), None);
    }
}
