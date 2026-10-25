/*
File: watermark_chapter/template.rs

Purpose:
Compositing-operator implementations, `MarkTemplate` behaviour (construction, NCC
reference, anchor columns) and the FOOTPRINT measurement: trimmed footprints from a model or a
template and the untrimmed mark extent.

Key items:
- `impl CompositingOperator for AlphaBlend`, `alpha_blend_operator()`, `operator_from_id()`
- `impl MarkTemplate`
- `trimmed_footprint_from_model()`, `trimmed_footprint_from_template()`,
  `mark_extent_from_template()`

Notes:
Types (`MarkTemplate`, `CorrelationRef`, `AlphaBlend`) and the footprint thresholds are declared in
`mod.rs`; the footprint contract is stated in its header.
*/
use std::sync::Arc;
use image::RgbaImage;
use super::{
    ANCHOR_CLUSTER_RADIUS_PX, AlphaBlend, CompositingOperator, CorrelationRef,
    FOOTPRINT_TRIM_MIN_ALPHA, FOOTPRINT_TRIM_MIN_DEVIATION_LSB, FOOTPRINT_TRIM_SAFETY_PX,
    MarkTemplate, PixelRect, RING_WIDTH_PX, WatermarkError, WatermarkModel, count_f32, luma_of,
    validate_page, validate_rect,
};


impl CompositingOperator for AlphaBlend {
    fn id(&self) -> &'static str {
        "alpha_blend"
    }

    fn to_linear(&self, value: f32) -> f32 {
        value
    }

    fn to_observed(&self, linear: f32) -> f32 {
        linear
    }
}

/// The default operator as a shareable handle.
#[must_use]
pub(crate) fn alpha_blend_operator() -> Arc<dyn CompositingOperator> {
    Arc::new(AlphaBlend)
}

/// Resolve a persisted [`CompositingOperator::id`] back into an operator.
///
/// `None` for an id this build does not implement. The caller MUST refuse rather than fall back
/// on alpha blending: a multiply or gamma mark composited under an alpha-blend assumption is
/// confidently wrong, not approximately right, which is the whole reason the operator is a trait
/// and its id is persisted.
#[must_use]
pub(crate) fn operator_from_id(id: &str) -> Option<Arc<dyn CompositingOperator>> {
    match id {
        "alpha_blend" => Some(alpha_blend_operator()),
        _ => None,
    }
}

impl MarkTemplate {
    /// Cut a template out of `rect` on `page`. The rect's x becomes the initial anchor set — one
    /// column, the one the user pointed at; [`MarkTemplate::set_anchors`] widens it to whatever
    /// the data turns out to hold.
    ///
    /// # Errors
    /// [`WatermarkError::RectOutOfPage`] / [`WatermarkError::EmptyRect`] for a bad rect,
    /// [`WatermarkError::FlatTemplate`] when the patch has no contrast to correlate on.
    pub fn from_page(page: &RgbaImage, rect: PixelRect) -> Result<Self, WatermarkError> {
        let (pw, _ph) = validate_page(page)?;
        validate_rect(page, rect)?;
        let raw = page.as_raw();
        let (tw, th) = (rect.width as usize, rect.height as usize);
        let mut centered = Vec::with_capacity(tw * th);
        for row in 0..th {
            let base = ((rect.y as usize + row) * pw + rect.x as usize) * 4;
            for px in raw[base..base + tw * 4].as_chunks::<4>().0 {
                centered.push(luma_of(px[0], px[1], px[2]));
            }
        }
        let mean = centered.iter().sum::<f32>() / count_f32(centered.len());
        for value in &mut centered {
            *value -= mean;
        }
        let norm = centered.iter().map(|v| v * v).sum::<f32>().sqrt();
        if !norm.is_finite() || norm <= f32::EPSILON {
            return Err(WatermarkError::FlatTemplate {
                width: rect.width,
                height: rect.height,
            });
        }
        Ok(Self {
            width: rect.width,
            height: rect.height,
            anchors: vec![rect.x],
            centered,
            norm,
        })
    }

    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Page columns occurrences of this mark are expected at, ascending and deduplicated.
    #[must_use]
    pub fn anchors(&self) -> &[u32] {
        &self.anchors
    }

    /// Replace the anchor set with one discovered from the data.
    ///
    /// Values are sorted and columns within `ANCHOR_CLUSTER_RADIUS_PX` of one another are merged,
    /// so a caller may hand in raw per-occurrence x positions.
    ///
    /// # Errors
    /// [`WatermarkError::EmptyAnchorSet`] for an empty input — a template with no anchor would
    /// turn detection into a full-width scan, which is neither affordable nor precise.
    pub fn set_anchors(&mut self, anchors: &[u32]) -> Result<(), WatermarkError> {
        let merged = cluster_columns(anchors, ANCHOR_CLUSTER_RADIUS_PX);
        if merged.is_empty() {
            return Err(WatermarkError::EmptyAnchorSet);
        }
        self.anchors = merged;
        Ok(())
    }

    /// Stable literal key of the anchor SET, for persistence and cache lookup.
    ///
    /// A model belongs to a source, and a source is identified by where it stamps: two chapters
    /// of the same publisher with different anchor sets are different layouts and must not share
    /// a cached model. Never localized, never parsed back.
    #[must_use]
    pub fn anchor_key(&self) -> String {
        self.anchors
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// True when `x` sits within `tolerance` of one of the anchor columns. This is half of the
    /// accept rule, not merely a scan restriction: see `ANCHOR_TOLERANCE_PX`.
    pub(super) fn is_on_anchor(&self, x: u32, tolerance: u32) -> bool {
        self.anchors
            .iter()
            .any(|&anchor| x.abs_diff(anchor) <= tolerance)
    }

    /// This template as a bare correlation reference.
    pub(super) fn correlation_ref(&self) -> CorrelationRef<'_> {
        CorrelationRef {
            width: self.width as usize,
            height: self.height as usize,
            centered: &self.centered,
            norm: self.norm,
        }
    }
}

/// Sort `columns`, merge everything within `radius` into one representative (the rounded mean of
/// the cluster) and return the result ascending. Empty input yields an empty result.
fn cluster_columns(columns: &[u32], radius: u32) -> Vec<u32> {
    let mut sorted: Vec<u32> = columns.to_vec();
    sorted.sort_unstable();
    let mut out: Vec<u32> = Vec::new();
    let mut cluster: Vec<u32> = Vec::new();
    for &column in &sorted {
        match cluster.last() {
            Some(&last) if column - last <= radius => cluster.push(column),
            Some(_) => {
                out.push(mean_column(&cluster));
                cluster.clear();
                cluster.push(column);
            }
            None => cluster.push(column),
        }
    }
    if !cluster.is_empty() {
        out.push(mean_column(&cluster));
    }
    out.dedup();
    out
}

/// Rounded mean of a non-empty column cluster. `u64` sums keep the mean exact for any page width.
pub(super) fn mean_column(cluster: &[u32]) -> u32 {
    let count = cluster.len() as u64;
    let sum: u64 = cluster.iter().map(|&value| u64::from(value)).sum();
    // Cast justification: the mean of values that all fit u32 fits u32.
    ((sum + count / 2) / count) as u32
}

/// Expand the inclusive pixel box `(min_x, min_y)..=(max_x, max_y)` by
/// [`FOOTPRINT_TRIM_SAFETY_PX`] and clamp it into a `width` x `height` footprint.
///
/// Both dimensions must be non-zero — every caller has already validated that through
/// [`validate_page`] or [`WatermarkModel::from_parts`]. All arithmetic saturates, so the grown box
/// can never leave the footprint and no subtraction can underflow.
fn grown_extent(min_x: u32, min_y: u32, max_x: u32, max_y: u32, width: u32, height: u32) -> PixelRect {
    let x0 = min_x.saturating_sub(FOOTPRINT_TRIM_SAFETY_PX);
    let y0 = min_y.saturating_sub(FOOTPRINT_TRIM_SAFETY_PX);
    let x1 = max_x
        .saturating_add(FOOTPRINT_TRIM_SAFETY_PX)
        .min(width.saturating_sub(1));
    let y1 = max_y
        .saturating_add(FOOTPRINT_TRIM_SAFETY_PX)
        .min(height.saturating_sub(1));
    PixelRect::new(
        x0,
        y0,
        x1.saturating_sub(x0).saturating_add(1),
        y1.saturating_sub(y0).saturating_add(1),
    )
}

/// The sub-rectangle of a fitted model's footprint the mark actually deposits in, grown by
/// [`FOOTPRINT_TRIM_SAFETY_PX`] of background and clamped to the footprint.
///
/// A detector's box is not the mark: it is the rectangle the detector happened to hand over, and
/// outside the deposit the model is `c = 0`, `s = 1` — fully transparent, carrying no information
/// and no removable signal. Every pixel of that padding still costs a stored plane entry and, far
/// worse, is demanded of every future reference crop. This is the measurement that says where the
/// mark really ends.
///
/// The returned rect is in FOOTPRINT-LOCAL coordinates: its origin is the footprint's own top
/// left. A rect equal to `(0, 0, width, height)` means the footprint is already tight.
///
/// # Errors
/// [`WatermarkError::BufferLength`] if a plane is not `width*height*3` long (revalidated here
/// rather than assumed, CLAUDE.md §11), and [`WatermarkError::FlatTemplate`] when NO pixel reaches
/// the floor at all — a model that deposits nowhere. The caller must keep its footprint rather
/// than trim to nothing.
pub(crate) fn trimmed_footprint_from_model(model: &WatermarkModel) -> Result<PixelRect, WatermarkError> {
    let (width, height) = (model.width(), model.height());
    let (w, h) = (width as usize, height as usize);
    let expected = w.saturating_mul(h).saturating_mul(3);
    let (c, s) = (model.c(), model.s());
    if c.len() != expected {
        return Err(WatermarkError::BufferLength {
            what: "model c plane",
            len: c.len(),
            expected,
        });
    }
    if s.len() != expected {
        return Err(WatermarkError::BufferLength {
            what: "model s plane",
            len: s.len(),
            expected,
        });
    }
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            let base = (y * w + x) * 3;
            // Either half is enough. `1 - s` is the opacity, which is what a removal divides by;
            // `c` is the additive deposit, which a mark of a very dark colour carries almost
            // none of while still being opaque, and vice versa. Testing both is what keeps a
            // degenerate-but-real mark inside the box.
            let marked = (0..3).any(|channel| {
                let index = base + channel;
                1.0 - s[index] >= FOOTPRINT_TRIM_MIN_ALPHA
                    || c[index] >= FOOTPRINT_TRIM_MIN_DEVIATION_LSB
            });
            if !marked {
                continue;
            }
            any = true;
            // Cast justification: `x < w` and `y < h`, both of which came from `u32` dimensions.
            let (px, py) = (x as u32, y as u32);
            min_x = min_x.min(px);
            min_y = min_y.min(py);
            max_x = max_x.max(px);
            max_y = max_y.max(py);
        }
    }
    if !any {
        return Err(WatermarkError::FlatTemplate { width, height });
    }
    Ok(grown_extent(min_x, min_y, max_x, max_y, width, height))
}

/// The sub-rectangle of `footprint` the mark occupies inside a stored TEMPLATE crop, grown by
/// [`FOOTPRINT_TRIM_SAFETY_PX`] of background and clamped to the footprint.
///
/// This is the template-only proxy for [`trimmed_footprint_from_model`], for an entry that has no
/// samples and therefore no `c`/`s`: the mark is wherever the crop differs from the flat level of
/// the background it was cut on. It is SOUND exactly when the padding around the mark is that flat
/// background — which is the case the trim exists for, a detector box grown loosely around a mark
/// stamped on empty page. It is NOT sound as a statement about faint deposit on a busy background,
/// and it does not pretend to be: on such a crop the background itself deviates by more than
/// [`FOOTPRINT_TRIM_MIN_DEVIATION_LSB`] everywhere, the extent comes back as the whole footprint,
/// and nothing is trimmed. The level is the plain mean of the [`RING_WIDTH_PX`]-wide frame just
/// inside `footprint`; a mark that reaches that frame pulls the level towards itself, which again
/// widens the extent rather than narrowing it. Alpha is ignored, as everywhere else in this
/// engine.
///
/// The returned rect is in FOOTPRINT-LOCAL coordinates, as [`trimmed_footprint_from_model`]'s is.
///
/// # Errors
/// [`WatermarkError::PageGeometry`] / [`WatermarkError::EmptyRect`] / [`WatermarkError::RectOutOfPage`]
/// for unusable geometry, and [`WatermarkError::FlatTemplate`] when no pixel of the crop differs
/// from its border level — a template that holds no mark at all. The caller must keep its
/// footprint rather than trim to nothing.
pub(crate) fn trimmed_footprint_from_template(
    template: &RgbaImage,
    footprint: PixelRect,
) -> Result<PixelRect, WatermarkError> {
    let raw = mark_extent_from_template(template, footprint)?;
    // Inclusive box: `raw` is half-open, `grown_extent` takes the last pixel still inside it.
    // `raw.width`/`raw.height` are non-zero whenever `mark_extent_from_template` returns Ok.
    Ok(grown_extent(
        raw.x,
        raw.y,
        raw.x + raw.width - 1,
        raw.y + raw.height - 1,
        footprint.width,
        footprint.height,
    ))
}

/// The RAW rectangle of `footprint` whose pixels deviate from the crop's own border level — the
/// mark's measured extent with NO safety border added.
///
/// This is the measurement [`trimmed_footprint_from_template`] grows by
/// [`FOOTPRINT_TRIM_SAFETY_PX`]; both share one rule, and the raw form exists because two callers
/// need the unpadded box itself:
/// - deriving a NEW entry's footprint jointly from several reference crops, where every crop's own
///   safety border would be added to a union that then no longer states what was measured;
/// - detecting a crop that CUT the mark. A raw box touching the crop's own border means the mark
///   continues outside the crop; the GROWN box touches the border routinely and says nothing.
///
/// The returned rect is in FOOTPRINT-LOCAL coordinates and is never empty.
///
/// # Errors
/// [`WatermarkError::PageGeometry`] / [`WatermarkError::EmptyRect`] / [`WatermarkError::RectOutOfPage`]
/// for unusable geometry, and [`WatermarkError::FlatTemplate`] when no pixel of the crop differs
/// from its border level — a crop that holds no mark at all.
pub(crate) fn mark_extent_from_template(
    template: &RgbaImage,
    footprint: PixelRect,
) -> Result<PixelRect, WatermarkError> {
    let (pw, _ph) = validate_page(template)?;
    validate_rect(template, footprint)?;
    let raw = template.as_raw();
    let (fw, fh) = (footprint.width as usize, footprint.height as usize);
    let (ox, oy) = (footprint.x as usize, footprint.y as usize);
    let ring = RING_WIDTH_PX as usize;
    let on_frame = |x: usize, y: usize| -> bool {
        x < ring || y < ring || fw.saturating_sub(x) <= ring || fh.saturating_sub(y) <= ring
    };
    let mut sums = [0f64; 3];
    let mut frame_pixels = 0usize;
    for y in 0..fh {
        for x in 0..fw {
            if !on_frame(x, y) {
                continue;
            }
            let base = ((oy + y) * pw + ox + x) * 4;
            for channel in 0..3 {
                sums[channel] += f64::from(raw[base + channel]);
            }
            frame_pixels += 1;
        }
    }
    if frame_pixels == 0 {
        // Unreachable for a non-empty rect — `on_frame` is true for every pixel of a rect
        // narrower than two ring widths — but a zero divisor must never be assumed away.
        return Err(WatermarkError::EmptyRect { rect: footprint });
    }
    // Cast justification: a mean of 0..=255 values, exactly representable in f32.
    let level: [f32; 3] = std::array::from_fn(|channel| (sums[channel] / frame_pixels as f64) as f32);

    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut any = false;
    for y in 0..fh {
        for x in 0..fw {
            let base = ((oy + y) * pw + ox + x) * 4;
            let marked = (0..3).any(|channel| {
                (f32::from(raw[base + channel]) - level[channel]).abs()
                    > FOOTPRINT_TRIM_MIN_DEVIATION_LSB
            });
            if !marked {
                continue;
            }
            any = true;
            // Cast justification: `x < fw` and `y < fh`, both from `u32` rect dimensions.
            let (px, py) = (x as u32, y as u32);
            min_x = min_x.min(px);
            min_y = min_y.min(py);
            max_x = max_x.max(px);
            max_y = max_y.max(py);
        }
    }
    if !any {
        return Err(WatermarkError::FlatTemplate {
            width: footprint.width,
            height: footprint.height,
        });
    }
    // Inclusive box -> half-open rect. Both `max_*` are >= their `min_*`, so neither
    // subtraction underflows and neither dimension is zero.
    Ok(PixelRect::new(min_x, min_y, max_x - min_x + 1, max_y - min_y + 1))
}
