/*
File: region_edit_v2/layers.rs

Purpose:
The visual layers of a region frame: a stack of N paintable mask layers plus one colour MARKS
layer, and the processed-result layer that holds a consumer's output until it is merged into the
clean overlay.

Main responsibilities:
- keep one L8 mask buffer per layer (only `0` and `255`) plus an O(1) set-pixel counter
- keep one straight-alpha RGBA marks buffer (the user's colour annotation) plus an O(1)
  non-transparent-pixel counter
- keep every preview in lockstep with its buffer, and re-upload only the dirty patch
- offer ONE ordered per-stroke undo history across mask strokes and marks strokes/rectangles,
  restoring buffer, counter and preview together
- paint the marks, then the mask layers in index order, into a screen rect

Key structures:
- `MaskLayerSpec`: what a consumer declares about one mask layer — its tint and its name
- `MaskStack`: geometry, the mask layers, the marks layer, the active index, the undo history
- `MaskLayer`: one layer's buffer, counter, preview, dirty box and texture (private)
- `MarksLayer`: the marks buffer, counter, preview, dirty box and texture (private)
- `ResultLayer`: the processed image plus its lazily created texture

Key functions:
- `MaskStack::paint_segment`, `set_active_from_alpha`, `clear_all`, `push_undo`, `undo`
- `MaskStack::paint_marks_segment`, `paint_marks_boxes`, `push_marks_undo`, `marks_rgba`
- `hollow_rect_boxes()`: the pixel boxes of an unfilled rectangle (pure)
- `MaskStack::ensure_textures`, `MaskStack::draw_marks`, `MaskStack::draw_masks`
- `ResultLayer::ensure_texture`, `ResultLayer::draw`

Notes:
Modelled on `Flux2SessionState` (`tools/ai_editor/engines/flux2_klein/session.rs`), with N mask
layers where that type has one.
Brush radius policy is deliberately NOT here: the frame owns its `ms_tools::MaskBrush`es and hands
`radius_px()` to the paint calls, so this file adds no fourth copy of the radius handling. The
disc and the segment walk are ONE rasterizer (`visit_disc`, `for_each_stamp`) shared by the mask
and the marks writers; `ms_tools`' colour-image brush is not reused because it stamps at a coarser
cadence and reports no dirty box.
Design and the decisions behind it: `dev-docs/region_edit_v2_plan.md`.
*/

use ms_log::runtime_log;
use eframe::egui;
use egui::{Color32, Pos2, Rect, TextureHandle, TextureOptions};

/// Fraction of a tint's own alpha that a set mask pixel gets in the preview.
///
/// Same value as `build_tinted_mask_preview` in `tools/base.rs`, so a v2 layer reads exactly like
/// the mask of the existing region editors instead of introducing a second translucency.
const MASK_PREVIEW_ALPHA_FACTOR: f32 = 0.45;

/// Snapshots kept per stack. The pinned API has no depth parameter, and an unbounded stack of
/// megapixel L8 buffers is a real memory risk, so the oldest snapshot is dropped past this depth.
const MAX_UNDO_STEPS: usize = 32;

/// Total bytes the undo history may hold. A marks snapshot costs 4 bytes per pixel, so a
/// large frame reaches the step bound only after hundreds of megabytes; the oldest snapshots
/// are dropped past this budget too, always keeping the most recent one. The budget is SHARED
/// by mask and marks snapshots (one history), so on a very large frame it also shortens the
/// mask history below `MAX_UNDO_STEPS` (16 steps of a 16 MP L8 layer).
const MAX_UNDO_BYTES: usize = 256 * 1024 * 1024;

/// UV rectangle covering a whole texture — every layer is painted 1:1 into the frame rect.
const UV_FULL: Rect = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));

/// Preview colour of a set pixel for a layer tinted with `tint`.
///
/// The tint's RGB is taken un-premultiplied (so a translucent tint is not darkened by a second
/// premultiplication) and its alpha is scaled by `MASK_PREVIEW_ALPHA_FACTOR`. A set pixel is never
/// fully transparent: the alpha floor is `1`.
fn preview_color(tint: Color32) -> Color32 {
    let [r, g, b, a] = tint.to_srgba_unmultiplied();
    let alpha = (f32::from(a) * MASK_PREVIEW_ALPHA_FACTOR)
        .round()
        .clamp(1.0, 255.0);
    // The clamp above pins the value to `1.0..=255.0`, so this cast is exact, not lossy.
    let alpha = alpha as u8;
    Color32::from_rgba_unmultiplied(r, g, b, alpha)
}

/// Half-open pixel box `(x0, y0, x1, y1)`: preview pixels awaiting upload, or the area a
/// marks rectangle fills (`hollow_rect_boxes`).
pub type DirtyBox = (usize, usize, usize, usize);

// ---------------------------------------------------------------------------------------
// One mask layer
// ---------------------------------------------------------------------------------------

/// One paintable mask layer: the L8 buffer, its set-pixel counter, the tinted preview kept in
/// lockstep with the buffer, the pending upload box and the GPU texture.
///
/// Invariants, all maintained by the writers in this file and relied on by `MaskStack`:
/// - `bytes` holds only `0` and `255`, which is what makes the counter maintainable in O(1);
/// - `set_px` equals the number of non-zero bytes;
/// - `preview.pixels[i]` is `TRANSPARENT` when `bytes[i] == 0` and `set_color` otherwise;
/// - `bytes.len() == preview.pixels.len() == w * h` of the owning stack.
struct MaskLayer {
    /// Preview colour of a set pixel, precomputed from the layer's tint.
    set_color: Color32,
    bytes: Vec<u8>,
    /// Non-zero byte count, kept in step with every write into `bytes`.
    ///
    /// The frame asks "is anything painted?" on EVERY frame (the answer is its lock state, D4 of
    /// the design), and answering it by scanning megapixel buffers would be pure waste.
    set_px: usize,
    preview: egui::ColorImage,
    texture: Option<TextureHandle>,
    dirty: Option<DirtyBox>,
}

// `TextureHandle` implements no `Debug`, so the derive is impossible here; this prints the
// diagnostics that matter (geometry, counter, upload state) and never the pixels.
impl std::fmt::Debug for MaskLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaskLayer")
            .field("set_color", &self.set_color)
            .field("len", &self.bytes.len())
            .field("set_px", &self.set_px)
            .field("has_texture", &self.texture.is_some())
            .field("dirty", &self.dirty)
            .finish()
    }
}

impl MaskLayer {
    /// A layer with no geometry yet. `MaskStack::resize` gives it its buffers.
    fn blank(set_color: Color32) -> Self {
        Self {
            set_color,
            bytes: Vec::new(),
            set_px: 0,
            preview: egui::ColorImage::filled([0, 0], Color32::TRANSPARENT),
            texture: None,
            dirty: None,
        }
    }

    /// Re-allocates the layer for `w * h == len` pixels and clears it.
    ///
    /// The texture is dropped rather than patched: its GPU size no longer matches, so the next
    /// `ensure_texture` must upload a fresh one.
    fn reset(&mut self, w: usize, h: usize, len: usize) {
        self.bytes.clear();
        self.bytes.resize(len, 0);
        self.set_px = 0;
        self.preview = egui::ColorImage::filled([w, h], Color32::TRANSPARENT);
        self.texture = None;
        self.dirty = None;
    }

    /// Clears every pixel, its set-pixel counter and its preview, and dirties the whole layer.
    fn clear(&mut self, w: usize, h: usize) {
        if self.bytes.is_empty() {
            return;
        }
        self.bytes.fill(0);
        self.set_px = 0;
        self.preview.pixels.fill(Color32::TRANSPARENT);
        self.mark_dirty(0, 0, w, h);
    }

    /// Writes `value` into every pixel within `radius` of `(cx, cy)`.
    ///
    /// `cx`, `cy` and `radius` are `i64` because the pointer may sit far outside the mask and the
    /// caller interpolates along a segment; keeping the centre in a wide type means the scan box
    /// below can be clamped without any wrapping arithmetic.
    fn stamp_disc(&mut self, w: usize, h: usize, cx: i64, cy: i64, radius: i64, value: u8) {
        let color = if value == 0 {
            Color32::TRANSPARENT
        } else {
            self.set_color
        };
        let (bytes, set_px, preview) = (&mut self.bytes, &mut self.set_px, &mut self.preview.pixels);
        let changed = visit_disc(w, h, cx, cy, radius, |idx| {
            if bytes[idx] == value {
                return false;
            }
            bytes[idx] = value;
            // The buffer holds only 0 and 255, so a change is always a 0 <-> 255 transition
            // and the counter follows it exactly.
            if value == 0 {
                *set_px = set_px.saturating_sub(1);
            } else {
                *set_px = set_px.saturating_add(1);
            }
            preview[idx] = color;
            true
        });
        if let Some((x0, y0, x1, y1)) = changed {
            self.mark_dirty(x0, y0, x1, y1);
        }
    }

    /// Rebuilds the whole preview from `bytes` and marks it for upload.
    ///
    /// Used by undo: deriving the preview from the restored buffer is what makes the two provably
    /// consistent, and is why a snapshot stores 1 byte per pixel instead of the preview's 4.
    fn rebuild_preview(&mut self, w: usize, h: usize) {
        let color = self.set_color;
        for (pixel, byte) in self.preview.pixels.iter_mut().zip(self.bytes.iter()) {
            *pixel = if *byte == 0 { Color32::TRANSPARENT } else { color };
        }
        self.mark_dirty(0, 0, w, h);
    }

    /// Grows the pending upload box to cover the half-open `(x0, y0, x1, y1)`.
    fn mark_dirty(&mut self, x0: usize, y0: usize, x1: usize, y1: usize) {
        grow_dirty(&mut self.dirty, (x0, y0, x1, y1));
    }

    /// Uploads the preview, patching only the dirty box when a texture already exists.
    ///
    /// A full re-upload of a megapixel overlay on every brush frame is the single most expensive
    /// thing a painting tool can do; `set_partial` keeps the cost proportional to the stroke.
    fn ensure_texture(&mut self, ctx: &egui::Context, idx: usize, w: usize) {
        upload_preview(ctx, &mut self.texture, &mut self.dirty, &self.preview, w, || format!("region-edit-v2-mask-{idx}"));
    }
}

/// Uploads `preview` into `texture`, patching only the `dirty` box when a texture already exists.
///
/// A full re-upload of a megapixel overlay on every brush frame is the single most expensive
/// thing a painting tool can do; `set_partial` keeps the cost proportional to the stroke. `w` is
/// the preview's row stride. `name` is called only when the texture is first created, so the
/// per-frame call allocates nothing.
fn upload_preview(
    ctx: &egui::Context,
    texture: &mut Option<TextureHandle>,
    dirty: &mut Option<DirtyBox>,
    preview: &egui::ColorImage,
    w: usize,
    name: impl FnOnce() -> String,
) {
    if preview.pixels.is_empty() {
        *texture = None;
        *dirty = None;
        return;
    }
    let Some(handle) = texture.as_mut() else {
        // NEAREST: a mask or a mark must show crisp per-pixel edges when the canvas is zoomed in.
        *texture = Some(ctx.load_texture(name(), preview.clone(), TextureOptions::NEAREST));
        *dirty = None;
        return;
    };
    let Some((x0, y0, x1, y1)) = dirty.take() else {
        return;
    };
    let (patch_w, patch_h) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
    if patch_w == 0 || patch_h == 0 {
        return;
    }
    let mut pixels = Vec::with_capacity(patch_w.saturating_mul(patch_h));
    for y in y0..y1 {
        let row = y * w;
        pixels.extend_from_slice(&preview.pixels[row + x0..row + x1]);
    }
    handle.set_partial([x0, y0], egui::ColorImage::new([patch_w, patch_h], pixels), TextureOptions::NEAREST);
}

/// Grows `dirty` to cover the half-open `(x0, y0, x1, y1)`; an empty box changes nothing.
fn grow_dirty(dirty: &mut Option<DirtyBox>, (x0, y0, x1, y1): DirtyBox) {
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    *dirty = Some(match *dirty {
        Some((px0, py0, px1, py1)) => (px0.min(x0), py0.min(y0), px1.max(x1), py1.max(y1)),
        None => (x0, y0, x1, y1),
    });
}

/// Calls `visit(idx)` for every pixel of a `w * h` buffer within `radius` of `(cx, cy)`
/// (`dx² + dy² <= r²`) and returns the clamped scan box when any call answered `true` ("this
/// pixel changed"), `None` otherwise.
///
/// The ONE disc rasterizer of this file, shared by the mask and the marks writers. `cx`, `cy`
/// and `radius` are `i64` because the pointer may sit far outside the buffer and the caller
/// interpolates along a segment; keeping the centre in a wide type means the scan box can be
/// clamped without any wrapping arithmetic. `idx` is always `< w * h`.
fn visit_disc(w: usize, h: usize, cx: i64, cy: i64, radius: i64, mut visit: impl FnMut(usize) -> bool) -> Option<DirtyBox> {
    if w == 0 || h == 0 || radius <= 0 {
        return None;
    }
    // Clamp the scan box into the buffer FIRST: every index below is then plain `usize`
    // arithmetic that provably cannot leave the buffer.
    let x0 = clamp_to_index(cx.saturating_sub(radius), w);
    let y0 = clamp_to_index(cy.saturating_sub(radius), h);
    let x1 = clamp_to_index(cx.saturating_add(radius).saturating_add(1), w);
    let y1 = clamp_to_index(cy.saturating_add(radius).saturating_add(1), h);
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    let radius_sq = radius.saturating_mul(radius);
    let x_base = index_to_i64(x0);
    let mut changed = false;
    for y in y0..y1 {
        let dy = index_to_i64(y) - cy;
        let dy_sq = dy.saturating_mul(dy);
        if dy_sq > radius_sq {
            continue;
        }
        let row = y * w;
        // `dx` is carried alongside `x` so the inner loop needs no per-pixel index-to-integer
        // conversion; the zip keeps the two exactly in step.
        for (dx, x) in (x_base - cx..).zip(x0..x1) {
            if dx.saturating_mul(dx) + dy_sq > radius_sq {
                continue;
            }
            changed |= visit(row + x);
        }
    }
    changed.then_some((x0, y0, x1, y1))
}

/// Walks one brush segment from `from` to `to` over a `w * h` buffer and calls
/// `stamp(cx, cy, radius)` once per disc that can reach it.
///
/// The ONE segment walk of this file, shared by the mask and the marks writers. `radius` is the
/// brush's; `0` is treated as `1`, and one wider than the buffer's longest side is capped to it,
/// which covers the buffer entirely and so does not change what gets painted. One stamp per pixel
/// of the longer axis: consecutive discs then always overlap, so no gaps appear however fast the
/// pointer moves; `stamp_window` drops only stamps that cannot reach the buffer.
fn for_each_stamp(from: (i32, i32), to: (i32, i32), radius: usize, w: usize, h: usize, mut stamp: impl FnMut(i64, i64, i64)) {
    if w == 0 || h == 0 {
        return;
    }
    let radius = index_to_i64(radius.clamp(1, w.max(h).max(1)));
    let (fx, fy) = (i64::from(from.0), i64::from(from.1));
    let (dx, dy) = (i64::from(to.0) - fx, i64::from(to.1) - fy);
    let steps = dx.abs().max(dy.abs());
    let Some((first, last)) = stamp_window((fx, fy), (dx, dy), steps, radius, w, h) else {
        return;
    };
    for step in first..=last {
        let (cx, cy) = if steps == 0 {
            (fx, fy)
        } else {
            (fx + interpolate(dx, step, steps), fy + interpolate(dy, step, steps))
        };
        stamp(cx, cy, radius);
    }
}

/// The half-open pixel boxes `(x0, y0, x1, y1)` of an UNFILLED axis-aligned rectangle with
/// corners `a` and `b` (inclusive pixel coordinates, in any order) inside a `w * h` buffer.
///
/// Both corners are clamped into the buffer first. The walls are `thickness` pixels thick
/// (`0` is treated as `1`) and lie INSIDE the rectangle; when two opposite walls would meet or
/// overlap (`2 * thickness >= ` the side) the whole rectangle is one filled box. A side of ONE
/// pixel (a drag along the other axis) is widened to `thickness`, centred on the line, so a
/// straight drag draws a line as thick as the walls. The boxes are
/// disjoint. Empty for a buffer with no pixels.
#[must_use]
pub fn hollow_rect_boxes(w: usize, h: usize, a: (i32, i32), b: (i32, i32), thickness: usize) -> Vec<DirtyBox> {
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let clamp_x = |v: i32| clamp_to_index(i64::from(v), w - 1);
    let clamp_y = |v: i32| clamp_to_index(i64::from(v), h - 1);
    let (ax, bx) = (clamp_x(a.0), clamp_x(b.0));
    let (ay, by) = (clamp_y(a.1), clamp_y(b.1));
    // Half-open: the far corner is inclusive, so its box ends one past it.
    let (x0, x1) = (ax.min(bx), ax.max(bx) + 1);
    let (y0, y1) = (ay.min(by), ay.max(by) + 1);
    let t = thickness.max(1);
    // A drag along one axis (zero width or height) is a straight line, not a 1-pixel sliver:
    // it gets the wall thickness across, centred on the line and clamped to the buffer.
    let widen = |lo: usize, hi: usize, limit: usize| {
        if hi - lo > 1 {
            return (lo, hi);
        }
        let start = lo.saturating_sub(t / 2).min(limit.saturating_sub(t));
        (start, start.saturating_add(t).min(limit))
    };
    let (x0, x1) = widen(x0, x1, w);
    let (y0, y1) = widen(y0, y1, h);
    let wide = t.saturating_mul(2);
    if wide >= x1 - x0 || wide >= y1 - y0 {
        return vec![(x0, y0, x1, y1)];
    }
    vec![
        (x0, y0, x1, y0 + t),
        (x0, y1 - t, x1, y1),
        (x0, y0 + t, x0 + t, y1 - t),
        (x1 - t, y0 + t, x1, y1 - t),
    ]
}

// ---------------------------------------------------------------------------------------
// The marks layer
// ---------------------------------------------------------------------------------------

/// The colour marks layer: a straight-alpha RGBA buffer, its non-transparent pixel counter, the
/// premultiplied preview kept in lockstep with it, the pending upload box and the GPU texture.
///
/// ALLOCATED LAZILY: a frame whose user never draws a mark must pay nothing for the layer — no
/// 8 bytes per pixel of buffers, no texture upload after every resize, no snapshot copy. The
/// layer is either UNALLOCATED (`rgba` and `preview` empty, `set_px == 0`, no texture) or
/// allocated at the stack's geometry by the first write (`ensure_allocated`); `resize` and
/// `clear_all` return it to unallocated.
///
/// Invariants of an allocated layer, maintained by the writers below:
/// - `rgba` holds 4 STRAIGHT-alpha bytes per pixel, and a pixel with alpha `0` is exactly
///   `[0, 0, 0, 0]` (one canonical "unmarked" value);
/// - `set_px` equals the number of pixels with a non-zero alpha;
/// - `preview.pixels[i]` is the premultiplied form of pixel `i`;
/// - `rgba.len() == 4 * preview.pixels.len() == 4 * w * h` of the owning stack.
struct MarksLayer {
    rgba: Vec<u8>,
    set_px: usize,
    preview: egui::ColorImage,
    texture: Option<TextureHandle>,
    dirty: Option<DirtyBox>,
}

// `TextureHandle` implements no `Debug`; print the geometry, the counter and the upload state.
impl std::fmt::Debug for MarksLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarksLayer")
            .field("len", &self.rgba.len())
            .field("set_px", &self.set_px)
            .field("has_texture", &self.texture.is_some())
            .field("dirty", &self.dirty)
            .finish()
    }
}

/// The canonical straight-alpha bytes of `color`: a fully transparent colour is `[0, 0, 0, 0]`.
fn straight_rgba(color: Color32) -> [u8; 4] {
    let rgba = color.to_srgba_unmultiplied();
    if rgba[3] == 0 { [0; 4] } else { rgba }
}

impl MarksLayer {
    /// An unallocated layer; the first marks write allocates it (`ensure_allocated`).
    fn blank() -> Self {
        Self {
            rgba: Vec::new(),
            set_px: 0,
            preview: egui::ColorImage::filled([0, 0], Color32::TRANSPARENT),
            texture: None,
            dirty: None,
        }
    }

    /// Returns the layer to UNALLOCATED: buffers, counter and texture are dropped. This is both
    /// the clear and the resize — an empty layer and an unallocated one mean the same thing, and
    /// a texture of the old geometry could not be patched anyway.
    fn release(&mut self) {
        *self = Self::blank();
    }

    /// Allocates an all-transparent `w * h` layer if it is not allocated yet. The texture is
    /// created on the next `ensure_textures`.
    fn ensure_allocated(&mut self, w: usize, h: usize) {
        if !self.rgba.is_empty() {
            return;
        }
        let Some(len) = w.checked_mul(h).and_then(|px| px.checked_mul(4)) else {
            return;
        };
        self.rgba = vec![0; len];
        self.set_px = 0;
        self.preview = egui::ColorImage::filled([w, h], Color32::TRANSPARENT);
        self.texture = None;
        self.dirty = None;
    }

    /// Writes `value` (canonical straight RGBA) into pixel `idx`; answers whether it changed.
    ///
    /// REPLACES the pixel, never blends: a mark drawn twice over itself must stay the colour
    /// the user picked, not accumulate towards opaque.
    fn put(&mut self, idx: usize, value: [u8; 4]) -> bool {
        let at = idx * 4;
        let Some(old) = self.rgba.get_mut(at..at + 4) else {
            return false;
        };
        if old == value {
            return false;
        }
        let was_set = old[3] != 0;
        old.copy_from_slice(&value);
        match (was_set, value[3] != 0) {
            (false, true) => self.set_px = self.set_px.saturating_add(1),
            (true, false) => self.set_px = self.set_px.saturating_sub(1),
            (false, false) | (true, true) => {}
        }
        self.preview.pixels[idx] = Color32::from_rgba_unmultiplied(value[0], value[1], value[2], value[3]);
        true
    }

    /// Writes `value` into every pixel within `radius` of `(cx, cy)` (`visit_disc`).
    fn stamp_disc(&mut self, w: usize, h: usize, cx: i64, cy: i64, radius: i64, value: [u8; 4]) {
        if let Some(changed) = visit_disc(w, h, cx, cy, radius, |idx| self.put(idx, value)) {
            grow_dirty(&mut self.dirty, changed);
        }
    }

    /// Writes `value` into every pixel of the half-open box, which must lie inside the layer.
    fn fill_box(&mut self, w: usize, (x0, y0, x1, y1): DirtyBox, value: [u8; 4]) {
        let mut changed = false;
        for y in y0..y1 {
            for x in x0..x1 {
                changed |= self.put(y * w + x, value);
            }
        }
        if changed {
            grow_dirty(&mut self.dirty, (x0, y0, x1, y1));
        }
    }

    /// Rebuilds the whole preview and the counter from `rgba` and marks it for upload — the undo
    /// path, which restores only the buffer. `rgba` must already hold `4 * w * h` bytes.
    fn rebuild_from_rgba(&mut self, w: usize, h: usize) {
        if self.preview.size != [w, h] {
            self.preview = egui::ColorImage::filled([w, h], Color32::TRANSPARENT);
            self.texture = None;
        }
        let mut set_px = 0usize;
        for (pixel, rgba) in self.preview.pixels.iter_mut().zip(self.rgba.chunks_exact(4)) {
            *pixel = Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3]);
            set_px += usize::from(rgba[3] != 0);
        }
        self.set_px = set_px;
        grow_dirty(&mut self.dirty, (0, 0, w, h));
    }
}

/// Clamps a pixel coordinate into `0..=limit` without a lossy cast.
fn clamp_to_index(value: i64, limit: usize) -> usize {
    if value <= 0 {
        return 0;
    }
    match usize::try_from(value) {
        Ok(value) => value.min(limit),
        Err(_) => limit,
    }
}

/// Widens a buffer index for the distance arithmetic. Indices come from buffers that fit in memory,
/// so the saturation branch is unreachable in practice and only keeps the conversion total.
fn index_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// `delta * step / steps`, the position of one stamp along a segment.
///
/// The multiply is done in `i128` because both factors span the full `i32` range and their product
/// would overflow `i64`. The quotient is bounded by `delta`, so the narrowing back always succeeds;
/// the fallback exists only to keep the conversion total and cannot be reached.
fn interpolate(delta: i64, step: i64, steps: i64) -> i64 {
    let scaled = i128::from(delta) * i128::from(step) / i128::from(steps);
    i64::try_from(scaled).unwrap_or(delta)
}

/// The inclusive range of stamp indices whose disc can still reach a `w * h` mask, or `None` when
/// no stamp of the segment can.
///
/// A stamp whose centre is further than `radius` outside the mask clips away pixel-for-pixel, so
/// narrowing the loop to this window cannot change what gets painted. It exists because the loop is
/// otherwise proportional to the segment's length in mask pixels, and a segment spanning the whole
/// `i32` range would step through billions of no-ops on the GUI thread.
fn stamp_window(
    from: (i64, i64),
    delta: (i64, i64),
    steps: i64,
    radius: i64,
    w: usize,
    h: usize,
) -> Option<(i64, i64)> {
    if steps <= 0 {
        return Some((0, 0));
    }
    let (x0, x1) = axis_window(from.0, delta.0, steps, -radius, index_to_i64(w) - 1 + radius)?;
    let (y0, y1) = axis_window(from.1, delta.1, steps, -radius, index_to_i64(h) - 1 + radius)?;
    let first = x0.max(y0);
    let last = x1.min(y1);
    (first <= last).then_some((first, last))
}

/// One axis of `stamp_window`: the step indices for which `f + delta * step / steps` stays within
/// `lo..=hi`, widened by one step on each side so a disc that only just reaches the mask is kept.
fn axis_window(f: i64, delta: i64, steps: i64, lo: i64, hi: i64) -> Option<(i64, i64)> {
    if delta == 0 {
        // The axis never moves: either the whole segment is in range on it, or none of it is.
        return (f >= lo && f <= hi).then_some((0, steps));
    }
    // Every magnitude here is bounded by the i32 coordinate range times `steps`, which f64 carries
    // with an absolute error far below one step; the one-step widening below absorbs it either way.
    let steps_f = steps as f64;
    let scale = steps_f / delta as f64;
    let a = (lo - f) as f64 * scale;
    let b = (hi - f) as f64 * scale;
    let (t0, t1) = if a <= b { (a, b) } else { (b, a) };
    // Clamping before the cast keeps both ends inside `0..=steps`, so neither conversion is lossy.
    let first = t0.floor().clamp(0.0, steps_f) as i64;
    let last = t1.ceil().clamp(0.0, steps_f) as i64;
    Some((
        first.saturating_sub(1).max(0),
        last.saturating_add(1).min(steps),
    ))
}

// ---------------------------------------------------------------------------------------
// The stack
// ---------------------------------------------------------------------------------------

/// What a consumer declares about one mask layer it wants painted.
///
/// It lives in the FRAMEWORK rather than in the consumer that reads it, because the frame owns
/// both halves: the tint drives `MaskStack`'s preview and the key names the layer in the frame's
/// own chrome. A consumer-side trait re-exports this type instead of defining a twin, so the two
/// descriptions of a layer can never drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaskLayerSpec {
    /// Preview colour of the layer inside the frame. Should be opaque: `MaskStack` scales its
    /// alpha to `MASK_PREVIEW_ALPHA_FACTOR` itself so the mask stays translucent over the page.
    pub tint: Color32,
    /// Catalog key of the layer's name, resolved at DRAW time so it follows a language switch.
    /// Never a literal user-visible string.
    pub label_key: &'static str,
}

/// One undo snapshot: the state of ONE layer before a stroke — a mask layer or the marks layer.
///
/// One enum in one history is what makes «Отменить мазок» undo the LATEST stroke whichever
/// layer it went into.
#[derive(Debug)]
enum UndoStep {
    /// Mask layer `layer` before a stroke or a generated fill.
    Mask { layer: usize, bytes: Vec<u8>, set_px: usize },
    /// The marks layer before a marks stroke or rectangle; the counter and the preview are
    /// re-derived from the bytes on undo. `None` is the marker for an empty (or unallocated)
    /// layer: snapshotting nothing must not cost 4 bytes per pixel.
    Marks { rgba: Option<Vec<u8>> },
}

impl UndoStep {
    /// Bytes this snapshot holds, for the history's memory budget.
    fn byte_len(&self) -> usize {
        match self {
            Self::Mask { bytes, .. } => bytes.len(),
            Self::Marks { rgba } => rgba.as_ref().map_or(0, Vec::len),
        }
    }
}

/// The frame's paintable layers: N independent L8 masks over the same geometry, one of them
/// active, plus one colour marks layer.
///
/// The layer count is fixed at construction (`tints.len()`); nothing here assumes two. Mask
/// painting always targets the active layer. The marks layer is NOT one of the N layers: it has
/// no spec, no index and never reaches `bytes(idx)`; it is owned here only so that `resize`,
/// `clear_all` and the one undo history cover it. `is_empty` answers for the MASK layers (what
/// «Обработать» needs); the frame's lock signal (D4 of the design) is `is_blank`, which also
/// counts the marks.
#[derive(Debug)]
pub struct MaskStack {
    w: usize,
    h: usize,
    layers: Vec<MaskLayer>,
    active: usize,
    marks: MarksLayer,
    /// Most recent snapshot last. Cleared whenever a snapshot could no longer be applied safely.
    undo: Vec<UndoStep>,
}

impl MaskStack {
    /// Creates a `w * h` stack with one layer per entry of `tints`, all empty.
    ///
    /// `tints[i]` is the preview colour of layer `i` and should be opaque; its alpha is scaled to
    /// `MASK_PREVIEW_ALPHA_FACTOR` so the mask stays translucent over the page. An empty `tints`
    /// is a programming error: it yields a stack with no layers, on which every operation is a
    /// no-op, and it is logged rather than panicking.
    #[must_use]
    pub fn new(w: usize, h: usize, tints: &[Color32]) -> Self {
        if tints.is_empty() {
            runtime_log::log_warn(
                "[region-edit-v2] MaskStack::new got no tints: the stack has no layers and every paint call is a no-op",
            );
        }
        let mut stack = Self {
            w: 0,
            h: 0,
            layers: tints
                .iter()
                .map(|tint| MaskLayer::blank(preview_color(*tint)))
                .collect(),
            active: 0,
            marks: MarksLayer::blank(),
            undo: Vec::new(),
        };
        // One allocation path for both construction and resize, so the geometry guard lives once.
        stack.resize(w, h);
        stack
    }

    /// Mask geometry in page pixels, `(width, height)`.
    #[must_use]
    pub fn size(&self) -> (usize, usize) {
        (self.w, self.h)
    }

    #[must_use]
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Index of the layer `paint_segment` and `push_undo` operate on.
    #[must_use]
    pub fn active(&self) -> usize {
        self.active
    }

    /// Selects the layer to paint into.
    ///
    /// An out-of-range `idx` is REFUSED: the active layer is left unchanged and nothing panics, so
    /// a stale index held by a panel can never redirect a stroke into a different layer.
    pub fn set_active(&mut self, idx: usize) {
        if idx < self.layers.len() {
            self.active = idx;
        }
    }

    /// Re-sizes the stack to `w * h` and CLEARS every layer and the marks, along with the undo
    /// history.
    ///
    /// Losing painted work here is legal only because of decision D4 of the design: a non-empty
    /// stack LOCKS the frame, and a locked frame can be neither moved nor resized. A resize can
    /// therefore never reach a stack that holds work — this clear is the consequence of that
    /// invariant, not a shortcut around it. The undo history goes too: its snapshots describe the
    /// previous geometry and could not be applied to the new buffers.
    pub fn resize(&mut self, w: usize, h: usize) {
        let (w, h, len) = match w.checked_mul(h) {
            Some(len) => (w, h, len),
            None => {
                // Unreachable for any real page rect; refuse the geometry instead of overflowing.
                runtime_log::log_error(format!(
                    "[region-edit-v2] mask size {w}x{h} overflows usize; the mask stack is left empty"
                ));
                (0, 0, 0)
            }
        };
        self.w = w;
        self.h = h;
        for layer in &mut self.layers {
            layer.reset(w, h, len);
        }
        self.marks.release();
        self.undo.clear();
    }

    /// Whether NO mask layer holds a painted pixel. O(1) per layer — the counters are maintained
    /// by the writers, never recomputed. The marks do not count: this is what «Обработать» asks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.layers.iter().all(|layer| layer.set_px == 0)
    }

    /// Whether the marks layer holds no visible pixel. O(1).
    #[must_use]
    pub fn marks_is_empty(&self) -> bool {
        self.marks.set_px == 0
    }

    /// Whether neither a mask layer nor the marks hold anything. O(1). This is the frame's lock
    /// signal: marks are the user's work exactly like a mask.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.is_empty() && self.marks_is_empty()
    }

    /// Marked (non-transparent) pixel count of the marks layer.
    #[must_use]
    pub fn marks_set_px(&self) -> usize {
        self.marks.set_px
    }

    /// The marks layer: 4 STRAIGHT-alpha bytes per pixel, row-major, `4 * w * h` bytes; an
    /// unmarked pixel is `[0, 0, 0, 0]`. EMPTY while the layer is unallocated, which implies
    /// `marks_is_empty()` — read the counter, not the length, to ask whether anything is drawn.
    #[must_use]
    pub fn marks_rgba(&self) -> &[u8] {
        &self.marks.rgba
    }

    /// Paints one brush segment from `from` to `to` into the MARKS layer with `color`, or
    /// erases it to transparent. Same coordinates, radius rule and stamping as `paint_segment`;
    /// a pixel is REPLACED by the colour (straight alpha), never blended.
    pub fn paint_marks_segment(&mut self, from: (i32, i32), to: (i32, i32), radius: usize, color: Color32, erase: bool) {
        let (w, h) = (self.w, self.h);
        let value = if erase { [0; 4] } else { straight_rgba(color) };
        if value[3] == 0 && self.marks.rgba.is_empty() {
            // Erasing (or painting transparency) over an unallocated layer changes nothing.
            return;
        }
        self.marks.ensure_allocated(w, h);
        let marks = &mut self.marks;
        for_each_stamp(from, to, radius, w, h, |cx, cy, r| marks.stamp_disc(w, h, cx, cy, r, value));
    }

    /// Fills the half-open `boxes` of the MARKS layer with `color`, or erases them to
    /// transparent. A box is clipped to the layer first, so a stale box cannot index past it.
    /// `hollow_rect_boxes` produces exactly what this consumes.
    pub fn paint_marks_boxes(&mut self, boxes: &[DirtyBox], color: Color32, erase: bool) {
        let (w, h) = (self.w, self.h);
        let value = if erase { [0; 4] } else { straight_rgba(color) };
        if value[3] == 0 && self.marks.rgba.is_empty() {
            return;
        }
        self.marks.ensure_allocated(w, h);
        for &(x0, y0, x1, y1) in boxes {
            let (x1, y1) = (x1.min(w), y1.min(h));
            if x0 < x1 && y0 < y1 {
                self.marks.fill_box(w, (x0, y0, x1, y1), value);
            }
        }
    }

    /// Snapshots the marks layer so the next marks stroke or rectangle can be undone. Call it
    /// once when the gesture begins; it shares the one history with the mask strokes.
    pub fn push_marks_undo(&mut self) {
        let rgba = (self.marks.set_px != 0).then(|| self.marks.rgba.clone());
        self.push_step(UndoStep::Marks { rgba });
    }

    /// Appends `step`, dropping the oldest snapshots past `MAX_UNDO_STEPS` or `MAX_UNDO_BYTES`
    /// — always keeping the one just pushed.
    fn push_step(&mut self, step: UndoStep) {
        self.undo.push(step);
        if self.undo.len() > MAX_UNDO_STEPS {
            self.undo.remove(0);
        }
        let mut total: usize = self.undo.iter().map(UndoStep::byte_len).sum();
        while total > MAX_UNDO_BYTES && self.undo.len() > 1 {
            total = total.saturating_sub(self.undo.remove(0).byte_len());
        }
    }

    /// Painted pixel count of layer `idx`, or `0` when `idx` is out of range.
    #[must_use]
    pub fn layer_set_px(&self, idx: usize) -> usize {
        self.layers.get(idx).map_or(0, |layer| layer.set_px)
    }

    /// The L8 buffer of layer `idx` (`0` or `255` per pixel, row-major), or an empty slice when
    /// `idx` is out of range. This is what a consumer puts on the wire.
    #[must_use]
    pub fn bytes(&self, idx: usize) -> &[u8] {
        self.layers
            .get(idx)
            .map_or(&[][..], |layer| layer.bytes.as_slice())
    }

    /// Paints one brush segment from `from` to `to` into the ACTIVE layer, writing `255` (paint) or
    /// `0` (erase). Coordinates are mask pixels and may lie outside the mask; the stroke is clipped.
    ///
    /// `radius` comes from the frame's `MaskBrush` — this type owns no radius policy. A radius of
    /// `0` is treated as `1`, and one wider than the mask's longest side is capped to it, which
    /// covers the mask entirely and so does not change the painted result.
    ///
    /// Cost is proportional to the segment's length in mask pixels; a segment whose bounding box
    /// misses the mask entirely costs nothing.
    pub fn paint_segment(&mut self, from: (i32, i32), to: (i32, i32), radius: usize, erase: bool) {
        let (w, h) = (self.w, self.h);
        let Some(layer) = self.layers.get_mut(self.active) else {
            return;
        };
        let value = if erase { 0u8 } else { 255u8 };
        for_each_stamp(from, to, radius, w, h, |cx, cy, r| layer.stamp_disc(w, h, cx, cy, r, value));
    }

    /// Replaces the ACTIVE layer with `alpha` — one byte per pixel, row-major, non-zero meaning
    /// "set" — and returns whether it was written.
    ///
    /// This is how a mask that came from somewhere other than the brush (a backend detector, for
    /// instance) enters the stack, and it deliberately goes through the same door a stroke does:
    /// the previous contents are SNAPSHOTTED first, so the whole fill is one undo step and erases
    /// like anything painted by hand. Nothing else changes — the other layers, the active index
    /// and the geometry are untouched.
    ///
    /// `alpha` is binarized to `0`/`255` on the way in, which is the layer's own invariant and
    /// what keeps the set-pixel counter (the frame's lock signal) exact.
    ///
    /// Returns `false`, having changed nothing and taken no snapshot, when `alpha` does not hold
    /// exactly `width * height` bytes or the active index is out of range: a mask of the wrong
    /// shape must be refused rather than stretched over the layer, because every consumer indexes
    /// both with the same stride.
    pub fn set_active_from_alpha(&mut self, alpha: &[u8]) -> bool {
        let (w, h) = (self.w, self.h);
        if alpha.len() != w.saturating_mul(h) {
            return false;
        }
        let Some(layer) = self.layers.get(self.active) else {
            return false;
        };
        if layer.bytes.len() != alpha.len() {
            // The stack's own invariant, restated where writing past it would corrupt the counter.
            runtime_log::log_error(format!(
                "[region-edit-v2] layer {} holds {} bytes but the stack is {w}x{h}; the generated mask is refused",
                self.active,
                layer.bytes.len()
            ));
            return false;
        }
        self.push_undo();
        let Some(layer) = self.layers.get_mut(self.active) else {
            return false;
        };
        let mut set_px = 0usize;
        for (dst, src) in layer.bytes.iter_mut().zip(alpha) {
            let value = if *src == 0 { 0u8 } else { 255u8 };
            *dst = value;
            set_px += usize::from(value != 0);
        }
        layer.set_px = set_px;
        layer.rebuild_preview(w, h);
        true
    }

    /// Clears EVERY mask layer and the marks, and drops the undo history.
    ///
    /// The history goes because a snapshot covers one layer only (`push_undo`) and could not
    /// restore the layers this just cleared; a partial resurrection would be worse than none.
    pub fn clear_all(&mut self) {
        let (w, h) = (self.w, self.h);
        for layer in &mut self.layers {
            layer.clear(w, h);
        }
        self.marks.release();
        self.undo.clear();
    }

    /// Snapshots the ACTIVE mask layer so the next stroke can be undone. Call it once when a
    /// stroke begins, not per pointer move. Past `MAX_UNDO_STEPS` (or `MAX_UNDO_BYTES`) the
    /// oldest snapshot is dropped.
    pub fn push_undo(&mut self) {
        let Some(layer) = self.layers.get(self.active) else {
            return;
        };
        let step = UndoStep::Mask { layer: self.active, bytes: layer.bytes.clone(), set_px: layer.set_px };
        self.push_step(step);
    }

    /// Restores the most recent snapshot and returns `true`; returns `false` when the history is
    /// empty, leaving the stack untouched.
    ///
    /// The snapshot is applied to the layer it was taken from — a mask layer, not necessarily the
    /// active one, or the marks. Buffer, counter and preview are restored together — a stale
    /// counter would silently corrupt the frame's lock state (D4), so the preview is rebuilt
    /// from the restored buffer rather than trusted to have been saved alongside it.
    pub fn undo(&mut self) -> bool {
        let Some(step) = self.undo.pop() else {
            return false;
        };
        let (w, h) = (self.w, self.h);
        match step {
            UndoStep::Mask { layer: idx, bytes, set_px } => {
                let Some(layer) = self.layers.get_mut(idx) else {
                    return false;
                };
                if bytes.len() != layer.bytes.len() {
                    // `resize` clears the history, so this cannot happen; report it instead of
                    // writing a buffer of the wrong length.
                    runtime_log::log_error(format!(
                        "[region-edit-v2] undo snapshot of layer {idx} has {} bytes but the layer holds {}; the snapshot is discarded",
                        bytes.len(),
                        layer.bytes.len()
                    ));
                    return false;
                }
                layer.bytes = bytes;
                layer.set_px = set_px;
                layer.rebuild_preview(w, h);
            }
            UndoStep::Marks { rgba: None } => self.marks.release(),
            UndoStep::Marks { rgba: Some(rgba) } => {
                let expected = w.saturating_mul(h).saturating_mul(4);
                if rgba.len() != expected {
                    // Same invariant as above: `resize` clears the history.
                    runtime_log::log_error(format!(
                        "[region-edit-v2] undo snapshot of the marks has {} bytes but a {w}x{h} layer needs {expected}; the snapshot is discarded",
                        rgba.len()
                    ));
                    return false;
                }
                self.marks.rgba = rgba;
                self.marks.rebuild_from_rgba(w, h);
            }
        }
        true
    }

    /// Creates or patches the texture of the marks and of every mask layer. Call once per frame
    /// before `draw`.
    pub fn ensure_textures(&mut self, ctx: &egui::Context) {
        let w = self.w;
        let marks = &mut self.marks;
        upload_preview(ctx, &mut marks.texture, &mut marks.dirty, &marks.preview, w, || "region-edit-v2-marks".to_string());
        for (idx, layer) in self.layers.iter_mut().enumerate() {
            layer.ensure_texture(ctx, idx, w);
        }
    }

    /// Paints only the marks layer into `rect` — the BOTTOM of the stack: the frame paints it,
    /// then a live marks-rectangle preview, then `draw_masks`.
    ///
    /// The ORDER IS THE LAYERING: egui exposes no blend mode, so overlays are stacked exactly the
    /// way `draw_mask_editor_image` stacks them in `tools/base.rs` — several `Painter::image` calls
    /// at the same rect, each drawn over the previous. The marks lie UNDER the masks (the masks
    /// are translucent, so a mark stays visible through them). Not drawn before it is uploaded.
    pub fn draw_marks(&self, painter: &egui::Painter, rect: Rect) {
        if let Some(texture) = self.marks.texture.as_ref() {
            painter.image(texture.id(), rect, UV_FULL, Color32::WHITE);
        }
    }

    /// Paints only the mask layers into `rect`, in index order, over the marks: a higher index
    /// sits on top. Layers whose texture has not been uploaded yet are skipped.
    pub fn draw_masks(&self, painter: &egui::Painter, rect: Rect) {
        for layer in &self.layers {
            let Some(texture) = layer.texture.as_ref() else {
                continue;
            };
            painter.image(texture.id(), rect, UV_FULL, Color32::WHITE);
        }
    }
}

// ---------------------------------------------------------------------------------------
// The processed result
// ---------------------------------------------------------------------------------------

/// A consumer's processed image, held until the tool merges it into the clean overlay.
///
/// It owns the pixels and its texture and nothing else: it does not know how to apply itself.
/// Applying belongs to the tool, which must first refuse a result whose `size` differs from the
/// frame rect — `CanvasView::replace_overlay_region_px` silently nearest-rescales and overwrites
/// alpha (D7 of the design).
pub struct ResultLayer {
    image: egui::ColorImage,
    texture: Option<TextureHandle>,
}

// `TextureHandle` implements no `Debug`; print the geometry and the upload state, never the pixels.
impl std::fmt::Debug for ResultLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResultLayer")
            .field("size", &self.image.size)
            .field("has_texture", &self.texture.is_some())
            .finish()
    }
}

impl ResultLayer {
    /// Takes ownership of a processed image. Nothing is uploaded until the layer is first drawn.
    #[must_use]
    pub fn new(image: egui::ColorImage) -> Self {
        Self {
            image,
            texture: None,
        }
    }

    /// `[width, height]` of the result in pixels — what the tool compares against the frame rect.
    #[must_use]
    pub fn size(&self) -> [usize; 2] {
        self.image.size
    }

    /// The processed pixels, for merging into the clean overlay.
    #[must_use]
    pub fn image(&self) -> &egui::ColorImage {
        &self.image
    }

    /// Uploads the image once. Later calls are no-ops: the image never changes after `new`, so
    /// there is no dirty box and no partial upload here.
    pub fn ensure_texture(&mut self, ctx: &egui::Context) {
        if self.texture.is_some() || self.image.pixels.is_empty() {
            return;
        }
        // LINEAR, unlike the masks: this is photographic content sitting inside a page that is
        // itself drawn with LINEAR, so a different filter would make the preview stand out.
        self.texture = Some(ctx.load_texture(
            "region-edit-v2-result",
            self.image.clone(),
            TextureOptions::LINEAR,
        ));
    }

    /// Paints the result into `rect`, uploading it on first use. `&mut self` is what allows that
    /// lazy upload without the caller threading a `Context` through the render pass.
    pub fn draw(&mut self, painter: &egui::Painter, rect: Rect) {
        self.ensure_texture(painter.ctx());
        let Some(texture) = self.texture.as_ref() else {
            return;
        };
        painter.image(texture.id(), rect, UV_FULL, Color32::WHITE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINT_A: Color32 = Color32::from_rgb(80, 200, 255);
    const TINT_B: Color32 = Color32::from_rgb(255, 120, 80);

    /// A two-layer stack, the geometry used by most tests.
    fn stack(w: usize, h: usize) -> MaskStack {
        MaskStack::new(w, h, &[TINT_A, TINT_B])
    }

    /// Paints the WHOLE of layer `idx`, the way a test that needs a full layer gets one now
    /// that no public fill exists: a disc of `max(w, h)` at the centre covers every corner,
    /// because half the diagonal is never larger than the longer side.
    fn fill_layer(stack: &mut MaskStack, idx: usize) {
        let (w, h) = stack.size();
        stack.set_active(idx);
        let (cx, cy) = (i32::try_from(w / 2).unwrap_or(0), i32::try_from(h / 2).unwrap_or(0));
        stack.paint_segment((cx, cy), (cx, cy), w.max(h), false);
    }

    /// Brute-force set-pixel count, the reference the O(1) counter is checked against.
    fn count_set(bytes: &[u8]) -> usize {
        bytes.iter().filter(|byte| **byte != 0).count()
    }

    /// A generated mask lands in the ACTIVE layer only, keeps its non-square stride, is
    /// binarized on the way in, and leaves the counter exact.
    #[test]
    fn a_generated_mask_fills_the_active_layer_at_the_right_stride() {
        let (w, h) = (5usize, 3usize);
        let mut stack = stack(w, h);
        stack.set_active(1);
        let mut alpha = vec![0u8; w * h];
        // Row 2, column 1: the two indices differ, so a transposed write lands elsewhere.
        alpha[2 * w + 1] = 17;
        assert!(stack.set_active_from_alpha(&alpha));

        assert_eq!(stack.bytes(1)[2 * w + 1], 255, "a non-zero byte becomes a set pixel");
        assert_eq!(count_set(stack.bytes(1)), 1);
        assert_eq!(stack.layer_set_px(1), 1, "the O(1) counter must agree");
        assert_eq!(stack.layer_set_px(0), 0, "the other layer is untouched");
        assert!(!stack.is_empty(), "the frame must see the stack as painted");
    }

    /// The fill is ONE undo step: undoing it restores the layer the brush left behind, counter
    /// included, so a generated mask erases exactly like a stroke.
    #[test]
    fn a_generated_mask_is_undone_in_one_step() {
        let mut stack = stack(4, 4);
        fill_layer(&mut stack, 0);
        let painted = stack.layer_set_px(0);
        assert!(painted > 0);

        assert!(stack.set_active_from_alpha(&[255u8; 16]));
        assert_eq!(stack.layer_set_px(0), 16);
        assert!(stack.undo());
        assert_eq!(stack.layer_set_px(0), painted);
        assert_eq!(count_set(stack.bytes(0)), painted);
    }

    /// A buffer of the wrong length is refused outright: nothing is written and no undo step is
    /// taken, so the refusal cannot be mistaken for an empty fill afterwards.
    #[test]
    fn a_generated_mask_of_the_wrong_length_is_refused() {
        let mut stack = stack(5, 3);
        assert!(!stack.set_active_from_alpha(&[255u8; 8]));
        assert_eq!(stack.layer_set_px(0), 0);
        assert!(stack.is_empty());
        assert!(!stack.undo(), "a refused fill must leave no undo step behind");
        // A transposed buffer has the right LENGTH, so it is accepted — the caller checks the
        // shape. This pins that the length check is the only guard here.
        assert!(stack.set_active_from_alpha(&[255u8; 15]));
    }

    /// Every counter agrees with a brute-force scan of its own buffer.
    fn assert_counters_exact(stack: &MaskStack) {
        for idx in 0..stack.layer_count() {
            assert_eq!(
                stack.layer_set_px(idx),
                count_set(stack.bytes(idx)),
                "counter of layer {idx} drifted from its buffer"
            );
        }
    }

    /// The preview is `TRANSPARENT` exactly where the buffer is zero, and the layer's tint colour
    /// everywhere else.
    fn assert_preview_matches_bytes(stack: &MaskStack) {
        for layer in &stack.layers {
            assert_eq!(layer.preview.pixels.len(), layer.bytes.len());
            for (idx, byte) in layer.bytes.iter().enumerate() {
                let expected = if *byte == 0 {
                    Color32::TRANSPARENT
                } else {
                    layer.set_color
                };
                assert_eq!(layer.preview.pixels[idx], expected, "preview pixel {idx}");
            }
        }
    }

    #[test]
    fn a_stroke_sets_pixels_and_the_counter_matches_the_buffer() {
        let mut stack = stack(32, 32);
        stack.paint_segment((8, 8), (20, 8), 3, false);
        assert!(stack.layer_set_px(0) > 0);
        assert_eq!(stack.layer_set_px(1), 0, "only the active layer is painted");
        assert_counters_exact(&stack);
        assert_preview_matches_bytes(&stack);
        // Only 0 and 255 ever reach the buffer; the counter's O(1) maintenance depends on it.
        assert!(stack.bytes(0).iter().all(|b| *b == 0 || *b == 255));
    }

    #[test]
    fn erasing_removes_pixels_and_the_counter_matches_the_buffer() {
        let mut stack = stack(32, 32);
        stack.paint_segment((16, 16), (16, 16), 6, false);
        let painted = stack.layer_set_px(0);
        assert!(painted > 0);
        stack.paint_segment((16, 16), (16, 16), 6, true);
        assert_eq!(stack.layer_set_px(0), 0);
        assert_counters_exact(&stack);
        assert_preview_matches_bytes(&stack);
    }

    #[test]
    fn a_stroke_clipped_at_the_border_writes_nothing_out_of_bounds() {
        let mut stack = stack(16, 12);
        let before = stack.bytes(0).len();
        // Starts and ends far outside the mask, in both directions, with an oversized radius.
        stack.paint_segment((-500, -500), (900, 700), 40, false);
        stack.paint_segment((15, 11), (16_000, 11), 9, false);
        stack.paint_segment((0, 0), (i32::MIN, i32::MIN), 5, true);
        stack.paint_segment((i32::MAX, i32::MIN), (i32::MIN, i32::MAX), 3, false);
        assert_eq!(stack.bytes(0).len(), before, "the buffer was resized");
        assert_counters_exact(&stack);
        assert_preview_matches_bytes(&stack);
    }

    /// The stamp window narrows the loop for segments that start and end far outside the mask; it
    /// must not drop a single stamp that would have landed on it.
    #[test]
    fn a_segment_crossing_the_mask_paints_the_whole_crossing() {
        let mut stack = stack(16, 9);
        stack.paint_segment((-1_000_000, 4), (1_000_000, 4), 1, false);
        // Radius 1 covers row 4 completely plus rows 3 and 5 minus their corners.
        for x in 0..16 {
            assert_eq!(stack.bytes(0)[4 * 16 + x], 255, "row 4, column {x}");
            assert_eq!(stack.bytes(0)[3 * 16 + x], 255, "row 3, column {x}");
            assert_eq!(stack.bytes(0)[5 * 16 + x], 255, "row 5, column {x}");
        }
        assert_eq!(stack.bytes(0)[2 * 16 + 8], 0, "row 2 is out of the brush");
        assert_counters_exact(&stack);
    }

    #[test]
    fn a_zero_length_segment_still_stamps_one_disc() {
        let mut stack = stack(16, 16);
        stack.paint_segment((8, 8), (8, 8), 1, false);
        assert_counters_exact(&stack);
        // A radius-1 disc covers the centre and its four neighbours.
        assert_eq!(stack.layer_set_px(0), 5);
        assert_eq!(stack.bytes(0)[8 * 16 + 8], 255);
    }

    /// The L8 mask the region editor's own brush would leave on the same geometry.
    ///
    /// `MaskBrush` is the shared radius/rasterisation policy this module deliberately does not
    /// duplicate; these two tests are what pins "the frame paints with the same brush".
    fn region_editor_mask(w: usize, h: usize, from: (i32, i32), to: (i32, i32), radius: usize, erase: bool) -> Vec<u8> {
        let mut brush = ms_tools::MaskBrush::default();
        brush.set_radius_px(radius);
        let mut mask = vec![if erase { 255u8 } else { 0u8 }; w * h];
        brush.paint_binary_mask_segment(&mut mask, w, h, from.0, from.1, to.0, to.1, erase);
        mask
    }

    /// A single click must set EXACTLY the pixels the region editor's brush sets: its disc
    /// fills `|dx| <= floor(sqrt(r^2 - dy^2))` and `stamp_disc` fills `dx^2 + dy^2 <= r^2`,
    /// which are the same set. Erasing is the same disc written with `0`.
    #[test]
    fn a_single_click_paints_and_erases_the_same_disc_as_the_region_editors_brush() {
        for radius in [1usize, 3, 8, 24] {
            let mut painted = stack(64, 64);
            painted.paint_segment((30, 30), (30, 30), radius, false);
            assert_eq!(painted.bytes(0), region_editor_mask(64, 64, (30, 30), (30, 30), radius, false), "paint, radius {radius}");

            let mut erased = stack(64, 64);
            fill_layer(&mut erased, 0);
            erased.paint_segment((30, 30), (30, 30), radius, true);
            assert_eq!(erased.bytes(0), region_editor_mask(64, 64, (30, 30), (30, 30), radius, true), "erase, radius {radius}");
            assert_counters_exact(&erased);
        }
    }

    /// Along a segment the frame stamps once per pixel of the longer axis while the region
    /// editor stamps every `0.45 * radius` px. The frame is therefore the DENSER of the two and
    /// can only cover more, never less — which is what "no gaps however fast the pointer moves"
    /// means. Checked on the segments where both cadences land on the same integer lattice
    /// (axis-aligned and 45 degrees), so a missing pixel could only come from the cadence.
    #[test]
    fn a_stroke_covers_everything_the_region_editors_brush_would_cover() {
        for (from, to) in [((10, 10), (50, 50)), ((50, 50), (10, 10)), ((10, 30), (50, 30)), ((30, 10), (30, 50))] {
            let mut stack = stack(64, 64);
            stack.paint_segment(from, to, 6, false);
            let reference = region_editor_mask(64, 64, from, to, 6, false);
            for (idx, (ours, theirs)) in stack.bytes(0).iter().zip(reference.iter()).enumerate() {
                assert!(*theirs == 0 || *ours == 255, "{from:?}->{to:?}: pixel {idx} is painted by the region editor but not by the frame");
            }
            assert_counters_exact(&stack);
        }
    }

    #[test]
    fn a_radius_of_zero_still_stamps_one_pixel() {
        let mut stack = stack(8, 8);
        stack.paint_segment((4, 4), (4, 4), 0, false);
        assert!(stack.layer_set_px(0) > 0, "radius 0 must behave as radius 1");
        assert_counters_exact(&stack);
    }

    #[test]
    fn a_stroke_touches_only_the_active_layer() {
        let mut stack = stack(8, 4);
        fill_layer(&mut stack, 1);
        assert_eq!(stack.layer_set_px(1), 32);
        assert_eq!(stack.layer_set_px(0), 0);
        assert!(stack.bytes(1).iter().all(|byte| *byte == 255));
        assert_counters_exact(&stack);
        assert_preview_matches_bytes(&stack);
    }

    #[test]
    fn clear_all_empties_every_layer() {
        let mut stack = stack(8, 8);
        fill_layer(&mut stack, 0);
        fill_layer(&mut stack, 1);
        assert!(!stack.is_empty());
        stack.clear_all();
        assert!(stack.is_empty());
        assert_eq!(stack.layer_set_px(0), 0);
        assert_eq!(stack.layer_set_px(1), 0);
        assert_counters_exact(&stack);
        assert_preview_matches_bytes(&stack);
    }

    #[test]
    fn is_empty_is_false_while_any_layer_holds_pixels() {
        let mut stack = stack(8, 8);
        assert!(stack.is_empty());
        stack.set_active(1);
        stack.paint_segment((4, 4), (4, 4), 2, false);
        stack.set_active(0);
        // Layer 0 — the active one — is still blank, but the stack is not empty.
        assert_eq!(stack.layer_set_px(0), 0);
        assert!(!stack.is_empty());
    }

    #[test]
    fn set_active_out_of_range_is_refused() {
        let mut stack = stack(4, 4);
        stack.set_active(1);
        stack.set_active(7);
        assert_eq!(stack.active(), 1, "an out-of-range index must be ignored");
        stack.paint_segment((2, 2), (2, 2), 1, false);
        assert!(stack.layer_set_px(1) > 0, "painting still hits layer 1");
        assert_counters_exact(&stack);
    }

    #[test]
    fn an_unknown_layer_index_reads_as_empty() {
        let stack = stack(4, 4);
        assert_eq!(stack.layer_set_px(9), 0);
        assert!(stack.bytes(9).is_empty());
    }

    #[test]
    fn resize_clears_and_re_sizes_every_layer() {
        let mut stack = stack(8, 8);
        fill_layer(&mut stack, 0);
        fill_layer(&mut stack, 1);
        stack.resize(5, 3);
        assert_eq!(stack.size(), (5, 3));
        assert!(stack.is_empty());
        for idx in 0..stack.layer_count() {
            assert_eq!(stack.bytes(idx).len(), 15);
        }
        assert_counters_exact(&stack);
        assert_preview_matches_bytes(&stack);
        // The stack stays usable at the new geometry.
        stack.paint_segment((2, 1), (2, 1), 1, false);
        assert_counters_exact(&stack);
    }

    #[test]
    fn undo_restores_buffer_counter_and_preview_exactly() {
        let mut stack = stack(24, 24);
        stack.paint_segment((4, 4), (12, 12), 2, false);
        let bytes_before = stack.bytes(0).to_vec();
        let set_px_before = stack.layer_set_px(0);
        let preview_before = stack.layers[0].preview.pixels.clone();

        stack.push_undo();
        stack.paint_segment((14, 4), (20, 20), 5, false);
        assert_ne!(stack.bytes(0), bytes_before.as_slice());

        assert!(stack.undo());
        assert_eq!(stack.bytes(0), bytes_before.as_slice());
        assert_eq!(stack.layer_set_px(0), set_px_before);
        assert_eq!(stack.layers[0].preview.pixels, preview_before);
        assert_counters_exact(&stack);
        assert_preview_matches_bytes(&stack);
    }

    #[test]
    fn undo_on_an_empty_history_changes_nothing() {
        let mut stack = stack(8, 8);
        stack.paint_segment((4, 4), (4, 4), 2, false);
        let bytes_before = stack.bytes(0).to_vec();
        let set_px_before = stack.layer_set_px(0);
        assert!(!stack.undo());
        assert_eq!(stack.bytes(0), bytes_before.as_slice());
        assert_eq!(stack.layer_set_px(0), set_px_before);
    }

    #[test]
    fn undo_of_a_stroke_on_one_layer_leaves_the_others_untouched() {
        let mut stack = stack(20, 20);
        stack.paint_segment((5, 5), (5, 5), 3, false);
        let layer0_bytes = stack.bytes(0).to_vec();
        let layer0_set_px = stack.layer_set_px(0);

        stack.set_active(1);
        stack.push_undo();
        stack.paint_segment((10, 10), (15, 15), 4, false);
        assert!(stack.layer_set_px(1) > 0);

        assert!(stack.undo());
        assert_eq!(stack.layer_set_px(1), 0);
        assert_eq!(stack.bytes(0), layer0_bytes.as_slice());
        assert_eq!(stack.layer_set_px(0), layer0_set_px);
        assert_counters_exact(&stack);
        assert_preview_matches_bytes(&stack);
    }

    #[test]
    fn undo_applies_to_the_snapshotted_layer_not_the_active_one() {
        let mut stack = stack(12, 12);
        stack.push_undo();
        stack.paint_segment((6, 6), (6, 6), 2, false);
        // The user switches layers before undoing; the snapshot must still find layer 0.
        stack.set_active(1);
        stack.paint_segment((3, 3), (3, 3), 2, false);
        let layer1_set_px = stack.layer_set_px(1);

        assert!(stack.undo());
        assert_eq!(stack.layer_set_px(0), 0);
        assert_eq!(stack.layer_set_px(1), layer1_set_px);
        assert_counters_exact(&stack);
    }

    #[test]
    fn resize_and_clear_all_drop_the_undo_history() {
        let mut stack = stack(8, 8);
        stack.push_undo();
        stack.resize(4, 4);
        assert!(
            !stack.undo(),
            "a snapshot of the previous geometry must not survive a resize"
        );

        stack.push_undo();
        stack.clear_all();
        assert!(
            !stack.undo(),
            "an active-layer snapshot cannot undo a multi-layer clear"
        );
    }

    #[test]
    fn the_undo_history_is_bounded() {
        let mut stack = stack(4, 4);
        for _ in 0..(MAX_UNDO_STEPS + 8) {
            stack.push_undo();
        }
        assert_eq!(stack.undo.len(), MAX_UNDO_STEPS);
    }

    #[test]
    fn a_stack_without_tints_has_no_layers_and_never_panics() {
        let mut stack = MaskStack::new(8, 8, &[]);
        assert_eq!(stack.layer_count(), 0);
        assert!(stack.is_empty());
        stack.set_active(0);
        stack.paint_segment((1, 1), (4, 4), 3, false);
        stack.push_undo();
        assert!(!stack.undo());
        assert!(stack.is_empty());
    }

    #[test]
    fn a_stack_with_no_geometry_never_panics() {
        let mut stack = stack(0, 0);
        stack.paint_segment((0, 0), (5, 5), 4, false);
        stack.clear_all();
        assert!(stack.is_empty());
        assert_eq!(stack.size(), (0, 0));
        assert!(stack.bytes(0).is_empty());
    }

    #[test]
    fn a_layer_tint_keeps_its_rgb_and_is_translucent_in_the_preview() {
        let stack = stack(2, 2);
        let color = stack.layers[0].set_color;
        let [r, g, b, a] = color.to_srgba_unmultiplied();
        assert_eq!([r, g, b], [80, 200, 255]);
        // 255 * 0.45, the same tinting rule as `build_tinted_mask_preview`.
        assert_eq!(a, 115);
        assert_ne!(stack.layers[0].set_color, stack.layers[1].set_color);
    }

    #[test]
    fn a_result_layer_reports_the_size_of_its_image() {
        let result = ResultLayer::new(egui::ColorImage::filled([7, 3], Color32::RED));
        assert_eq!(result.size(), [7, 3]);
        assert_eq!(result.image().pixels.len(), 21);
    }

    // -----------------------------------------------------------------------------------
    // The marks layer
    // -----------------------------------------------------------------------------------

    const MARK_RED: Color32 = Color32::from_rgb(230, 30, 30);
    const MARK_BLUE: Color32 = Color32::from_rgb(20, 60, 240);

    /// The straight RGBA of marks pixel `idx`.
    fn mark_at(stack: &MaskStack, idx: usize) -> [u8; 4] {
        let at = idx * 4;
        let mut px = [0u8; 4];
        px.copy_from_slice(&stack.marks_rgba()[at..at + 4]);
        px
    }

    /// The marks counter and preview agree with a brute-force scan of the marks buffer, and
    /// every unmarked pixel is the canonical `[0, 0, 0, 0]`.
    fn assert_marks_consistent(stack: &MaskStack) {
        let rgba = stack.marks_rgba();
        assert_eq!(rgba.len(), stack.marks.preview.pixels.len() * 4);
        let mut set = 0usize;
        for (idx, px) in rgba.chunks_exact(4).enumerate() {
            if px[3] == 0 {
                assert_eq!(px, [0, 0, 0, 0], "pixel {idx} is transparent but not canonical");
            } else {
                set += 1;
            }
            assert_eq!(stack.marks.preview.pixels[idx], Color32::from_rgba_unmultiplied(px[0], px[1], px[2], px[3]), "preview pixel {idx}");
        }
        assert_eq!(stack.marks_set_px(), set, "the marks counter drifted from its buffer");
    }

    /// A marks stroke paints the marks layer only: the mask layers stay empty, so «Обработать»
    /// still sees no mask while the frame sees work.
    #[test]
    fn a_marks_stroke_paints_the_marks_layer_only() {
        let mut stack = stack(32, 32);
        stack.paint_marks_segment((8, 8), (20, 8), 3, MARK_RED, false);
        assert!(stack.marks_set_px() > 0);
        assert!(stack.is_empty(), "no mask layer may be touched");
        assert!(!stack.is_blank(), "but the stack holds work");
        assert_eq!(mark_at(&stack, 8 * 32 + 14), [230, 30, 30, 255]);
        assert_marks_consistent(&stack);
        assert_counters_exact(&stack);
    }

    /// The same disc and the same segment walk as the mask brush: a marks stroke covers exactly
    /// the pixels a mask stroke of the same geometry covers.
    #[test]
    fn a_marks_stroke_covers_the_same_pixels_as_a_mask_stroke() {
        for (from, to, radius) in [((30, 30), (30, 30), 5usize), ((3, 50), (60, 10), 4), ((-20, 10), (90, 40), 9)] {
            let mut stack = stack(64, 64);
            stack.paint_segment(from, to, radius, false);
            stack.paint_marks_segment(from, to, radius, MARK_RED, false);
            for (idx, byte) in stack.bytes(0).iter().enumerate() {
                assert_eq!(*byte != 0, mark_at(&stack, idx)[3] != 0, "{from:?}->{to:?} r{radius}: pixel {idx}");
            }
            assert_marks_consistent(&stack);
        }
    }

    /// A mark REPLACES what is under it: a second colour over the first is exactly the second,
    /// and a translucent colour does not accumulate towards opaque over itself.
    #[test]
    fn a_mark_replaces_rather_than_blends() {
        let mut stack = stack(16, 16);
        stack.paint_marks_segment((8, 8), (8, 8), 3, MARK_RED, false);
        stack.paint_marks_segment((8, 8), (8, 8), 3, MARK_BLUE, false);
        assert_eq!(mark_at(&stack, 8 * 16 + 8), [20, 60, 240, 255]);
        let translucent = Color32::from_rgba_unmultiplied(200, 0, 0, 100);
        stack.paint_marks_segment((8, 8), (8, 8), 3, translucent, false);
        stack.paint_marks_segment((8, 8), (8, 8), 3, translucent, false);
        assert_eq!(mark_at(&stack, 8 * 16 + 8)[3], translucent.to_srgba_unmultiplied()[3]);
        assert_marks_consistent(&stack);
    }

    /// Erasing writes the canonical transparent pixel and brings the counter back to zero.
    #[test]
    fn erasing_marks_leaves_transparency() {
        let mut stack = stack(16, 16);
        stack.paint_marks_segment((8, 8), (8, 8), 4, MARK_RED, false);
        stack.paint_marks_segment((8, 8), (8, 8), 6, MARK_RED, true);
        assert!(stack.marks_is_empty());
        assert!(stack.marks_rgba().iter().all(|byte| *byte == 0));
        assert_marks_consistent(&stack);
    }

    /// ONE ordered history: «Отменить мазок» takes back the latest gesture, whichever layer it
    /// went into, in reverse order across mask and marks strokes.
    #[test]
    fn undo_is_ordered_across_mask_and_marks_strokes() {
        let mut stack = stack(24, 24);
        stack.push_undo();
        stack.paint_segment((4, 4), (4, 4), 2, false);
        let mask_after_first = stack.layer_set_px(0);
        stack.push_marks_undo();
        stack.paint_marks_segment((12, 12), (12, 12), 3, MARK_RED, false);
        let marks_after_second = stack.marks_set_px();
        stack.push_undo();
        stack.paint_segment((20, 20), (20, 20), 2, false);

        assert!(stack.undo(), "third: the second mask stroke");
        assert_eq!(stack.layer_set_px(0), mask_after_first);
        assert_eq!(stack.marks_set_px(), marks_after_second, "the marks are untouched by a mask undo");
        assert!(stack.undo(), "second: the marks stroke");
        assert!(stack.marks_is_empty());
        assert_eq!(stack.layer_set_px(0), mask_after_first, "the mask is untouched by a marks undo");
        assert!(stack.undo(), "first: the first mask stroke");
        assert!(stack.is_blank());
        assert!(!stack.undo(), "nothing left");
        assert_marks_consistent(&stack);
        assert_counters_exact(&stack);
    }

    /// `clear_all` and `resize` cover the marks too: they are the user's work exactly like a mask.
    #[test]
    fn clear_all_and_resize_clear_the_marks() {
        let mut stack = stack(8, 8);
        stack.paint_marks_segment((4, 4), (4, 4), 2, MARK_RED, false);
        stack.clear_all();
        assert!(stack.marks_is_empty() && stack.is_blank());
        assert_marks_consistent(&stack);

        stack.paint_marks_segment((4, 4), (4, 4), 2, MARK_RED, false);
        stack.resize(5, 3);
        assert!(stack.marks_is_empty());
        stack.paint_marks_segment((2, 1), (2, 1), 1, MARK_RED, false);
        assert_eq!(stack.marks_rgba().len(), 5 * 3 * 4, "re-allocated at the new geometry");
        assert_marks_consistent(&stack);
    }

    /// A marks snapshot is 4 bytes per pixel, so the history is also bounded by bytes; the most
    /// recent snapshot always survives.
    #[test]
    fn the_undo_history_is_bounded_by_bytes_too() {
        let mut stack = stack(4, 4);
        stack.push_step(UndoStep::Marks { rgba: Some(vec![0; MAX_UNDO_BYTES / 2 + 1]) });
        stack.push_step(UndoStep::Marks { rgba: Some(vec![0; MAX_UNDO_BYTES / 2 + 1]) });
        assert_eq!(stack.undo.len(), 1, "two half-budget snapshots cannot both be kept");
        stack.push_step(UndoStep::Marks { rgba: Some(vec![0; MAX_UNDO_BYTES + 1]) });
        assert_eq!(stack.undo.len(), 1, "an oversized snapshot is still kept alone");
    }

    // -----------------------------------------------------------------------------------
    // The hollow rectangle
    // -----------------------------------------------------------------------------------

    /// Rasterizes `boxes` into an L8 grid for the assertions below.
    fn rasterize(w: usize, h: usize, boxes: &[DirtyBox]) -> Vec<u8> {
        let mut grid = vec![0u8; w * h];
        for &(x0, y0, x1, y1) in boxes {
            for y in y0..y1 {
                for x in x0..x1 {
                    assert_eq!(grid[y * w + x], 0, "boxes overlap at {x};{y}");
                    grid[y * w + x] = 1;
                }
            }
        }
        grid
    }

    /// The walls lie INSIDE the dragged rectangle and are exactly `thickness` pixels thick; the
    /// interior stays empty. The corners may come in any order.
    #[test]
    fn a_hollow_rectangle_has_walls_of_the_given_thickness_inside_it() {
        let (w, h) = (20usize, 16usize);
        for (a, b) in [((2, 3), (13, 12)), ((13, 12), (2, 3)), ((13, 3), (2, 12))] {
            let grid = rasterize(w, h, &hollow_rect_boxes(w, h, a, b, 2));
            for y in 0..h {
                for x in 0..w {
                    let inside = (2..=13).contains(&x) && (3..=12).contains(&y);
                    let wall = inside && (!(4..=11).contains(&x) || !(5..=10).contains(&y));
                    assert_eq!(grid[y * w + x] == 1, wall, "{a:?}->{b:?}: pixel {x};{y}");
                }
            }
        }
    }

    /// Corners outside the buffer are clamped onto it, so the walls run along the buffer edges.
    #[test]
    fn a_hollow_rectangle_is_clamped_to_the_buffer() {
        let (w, h) = (10usize, 8usize);
        let boxes = hollow_rect_boxes(w, h, (-50, -50), (500, i32::MAX), 1);
        let grid = rasterize(w, h, &boxes);
        for y in 0..h {
            for x in 0..w {
                let wall = x == 0 || y == 0 || x == w - 1 || y == h - 1;
                assert_eq!(grid[y * w + x] == 1, wall, "pixel {x};{y}");
            }
        }
        assert!(hollow_rect_boxes(0, 0, (0, 0), (5, 5), 3).is_empty(), "no buffer, no boxes");
    }

    /// Walls that would meet fill the rectangle, and a zero thickness still draws one pixel.
    #[test]
    fn a_thick_or_thin_hollow_rectangle_degrades_sensibly() {
        // 6 x 10: walls of 3 meet across the 6 px width.
        assert_eq!(hollow_rect_boxes(32, 32, (4, 4), (9, 13), 3), vec![(4, 4, 10, 14)]);
        // 8 wide, walls of 3: 2 px of interior remain, so it stays hollow.
        assert_eq!(hollow_rect_boxes(32, 32, (4, 4), (11, 13), 3).len(), 4);
        // A degenerate drag (one pixel) is one filled pixel.
        assert_eq!(hollow_rect_boxes(32, 32, (7, 7), (7, 7), 1), vec![(7, 7, 8, 8)]);
        let thin = rasterize(16, 16, &hollow_rect_boxes(16, 16, (2, 2), (9, 9), 0));
        assert_eq!(thin.iter().filter(|px| **px == 1).count(), 28, "a 1-pixel outline of an 8x8 square");
    }

    /// A drag along one axis draws a line as thick as the walls, centred on the drag, instead
    /// of a 1-pixel sliver — and it stays inside the buffer at its edge.
    #[test]
    fn a_straight_drag_draws_a_line_of_wall_thickness() {
        assert_eq!(hollow_rect_boxes(32, 32, (10, 4), (10, 20), 4), vec![(8, 4, 12, 21)]);
        assert_eq!(hollow_rect_boxes(32, 32, (3, 6), (25, 6), 4), vec![(3, 4, 26, 8)]);
        assert_eq!(hollow_rect_boxes(32, 32, (0, 4), (0, 20), 4), vec![(0, 4, 4, 21)], "clamped at the left edge");
        assert_eq!(hollow_rect_boxes(32, 32, (31, 4), (31, 20), 4), vec![(28, 4, 32, 21)], "clamped at the right edge");
    }

    /// The marks layer costs nothing until the first mark: no buffer, no texture, and a
    /// snapshot of an empty layer is a marker rather than a copy. Resize and clear release it.
    #[test]
    fn the_marks_layer_is_allocated_lazily() {
        let mut stack = stack(64, 64);
        assert!(stack.marks_rgba().is_empty(), "nothing allocated before the first mark");
        stack.paint_marks_segment((5, 5), (5, 5), 3, MARK_RED, true);
        assert!(stack.marks_rgba().is_empty(), "erasing nothing allocates nothing");
        stack.push_marks_undo();
        assert_eq!(stack.undo.last().map(UndoStep::byte_len), Some(0), "an empty layer snapshots as a marker");

        stack.paint_marks_segment((5, 5), (5, 5), 3, MARK_RED, false);
        assert_eq!(stack.marks_rgba().len(), 64 * 64 * 4);
        assert_marks_consistent(&stack);
        assert!(stack.undo(), "undoing to the marker empties the layer");
        assert!(stack.marks_is_empty() && stack.marks_rgba().is_empty());

        stack.paint_marks_segment((5, 5), (5, 5), 3, MARK_RED, false);
        stack.push_marks_undo();
        stack.paint_marks_segment((20, 20), (20, 20), 3, MARK_BLUE, false);
        let painted_once = {
            let mut probe = MaskStack::new(64, 64, &[TINT_A]);
            probe.paint_marks_segment((5, 5), (5, 5), 3, MARK_RED, false);
            probe.marks_rgba().to_vec()
        };
        assert!(stack.undo());
        assert_eq!(stack.marks_rgba(), painted_once.as_slice(), "a real snapshot restores the bytes");
        assert_marks_consistent(&stack);

        stack.resize(32, 32);
        assert!(stack.marks_rgba().is_empty(), "a resize releases the layer");
        stack.paint_marks_segment((5, 5), (5, 5), 3, MARK_RED, false);
        stack.clear_all();
        assert!(stack.marks_rgba().is_empty(), "so does clearing");
    }

    /// The rectangle boxes land in the marks layer through `paint_marks_boxes`, which clips a
    /// box that reaches past the layer instead of indexing out of it.
    #[test]
    fn marks_boxes_are_painted_and_clipped() {
        let mut stack = stack(10, 10);
        stack.paint_marks_boxes(&hollow_rect_boxes(10, 10, (1, 1), (8, 8), 1), MARK_RED, false);
        assert_eq!(stack.marks_set_px(), 28);
        stack.paint_marks_boxes(&[(5, 5, 50, 50)], MARK_BLUE, false);
        assert_eq!(mark_at(&stack, 9 * 10 + 9), [20, 60, 240, 255]);
        stack.paint_marks_boxes(&[(0, 0, 10, 10)], MARK_RED, true);
        assert!(stack.marks_is_empty());
        assert_marks_consistent(&stack);
    }
}
