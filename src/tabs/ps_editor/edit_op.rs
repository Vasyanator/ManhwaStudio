/*
File: tabs/ps_editor/edit_op.rs

Purpose:
Undo/redo operations for the PS-like editor, built on the generic `ms-actions`
engine. Part A added the raster brush-stroke op; part B1 adds structural (add/delete
a whole raster layer) and metadata (visibility / opacity / transform / deform) ops.
Cut/clip, merge-down and z-reorder undo are NOT here (later parts).

Direction convention (both structural and metadata ops):
- `apply` always drives the target toward the op's RECORDED end state — a `FieldPatch`
  applies its `after` value; a `LayerLifecycle` realizes its `dir` (Added ⇒ present,
  Removed ⇒ absent). A freshly RECORDED op is the forward edit, so `record` pushes it
  as-is (the mutation already happened live), `undo` runs `inverse()`, `redo` re-runs
  the original. `inverse()` swaps a `FieldPatch`'s before/after and flips a
  `LayerLifecycle`'s dir — matching the engine's Koharu-style contract.

Key structures:
- `PsEditOp`: one reversible PS-editor edit — `RasterPixels` (tiled+zstd `RasterDiff`),
  `LayerLifecycle` (add/delete a whole raster layer, retaining its pixels for re-add),
  and `FieldPatch` (a single metadata/geometry field's before+after).
- `LifecycleDir`: `Added` / `Removed` — the direction a `LayerLifecycle` realizes.
- `LayerFieldPatch`: one metadata/geometry field (visibility / opacity / transform /
  deform), each carrying `before` + `after`.
- `PsEditOpError`: typed, panic-free failure surface (target not resident + a
  wrapped `RasterDiffError`).

Key functions:
- `apply_raster_diff_to_layer`: the pure, testable pixel-mutation core — applies a
  `RasterDiff` to a `Layer`'s `image` (and mirrors the change into `base_image`,
  preserving the editable-raster `base_image == image` invariant), returning the
  changed rects. Unit-tested here without any GUI struct.
- `copy_region_premul`: extract a region-local premultiplied-RGBA8 buffer from a
  `ColorImage` for building a bounded region diff.

Alpha convention:
`Layer.image`/`base_image` are `egui::ColorImage` = premultiplied `Color32`. The
`RasterDiff` signed-delta round-trip is correct for ANY consistent RGBA8 buffer, so
these ops build the diff from and apply it to the premultiplied pixel bytes directly
(`ColorImage::as_raw`/`as_raw_mut`). No separate straight-alpha buffer exists here
(unlike the clean-overlay model), so premultiplied bytes are used throughout.
*/

use std::fmt;
use std::sync::Arc;

use eframe::egui::ColorImage;
use ms_actions::{ApplyDirection, DirtyRect, RasterDiff, RasterDiffError, ReversibleAction};

use super::PsEditorTabState;
use super::layers::{Layer, LayerTransform};
use crate::models::layer_model::manifest::DeformRec;

/// One reversible PS-editor edit.
///
/// The payload lives behind an `Arc` so `inverse()` (every undo) and the redo path
/// share it instead of deep-cloning the compressed tiles. A freshly RECORDED op has
/// `dir == Forward`, matching the engine: `ActionHistory::undo` runs `inverse()` (a
/// `Reverse` op = subtract the delta = restore pre-edit pixels), and `redo`
/// re-applies the original `Forward` op (add the delta).
#[derive(Debug, Clone)]
pub(crate) enum PsEditOp {
    /// A raster brush stroke recorded as a tiled+zstd reversible pixel delta against
    /// the layer identified by `layer_uid` on `page_idx`.
    RasterPixels {
        /// Page the diff was built against; the op is only valid while that page's
        /// stack is resident (the history is cleared on page switch).
        page_idx: usize,
        /// Stable doc uid of the target raster layer.
        layer_uid: String,
        /// The reversible tiled delta (premultiplied RGBA8).
        diff: Arc<RasterDiff>,
        /// Whether `apply` runs the delta Forward (redo/add) or Reverse (undo/subtract).
        dir: ApplyDirection,
        /// Human-readable label for history/logging.
        label: String,
    },
    /// Adds or deletes a whole raster layer. The full `layer` (including pixels) is retained so an
    /// undo/redo can re-materialize it; `z` restores its exact stack position on re-add.
    LayerLifecycle {
        /// Page the layer belongs to; only valid while that page is resident.
        page_idx: usize,
        /// The full layer (metadata + base/display pixels) to re-add on the `Added` direction.
        layer: Box<Layer>,
        /// The layer's unified Z at record time, used to re-insert it at its exact prior position.
        z: u32,
        /// Whether `apply` realizes the layer PRESENT (`Added`) or ABSENT (`Removed`).
        dir: LifecycleDir,
    },
    /// A single metadata/geometry field change on the raster identified by `layer_uid`.
    FieldPatch {
        /// Page the layer belongs to; only valid while that page is resident.
        page_idx: usize,
        /// Stable doc uid of the target raster layer.
        layer_uid: String,
        /// The field's before + after values; `apply` drives the layer to `after`.
        field: LayerFieldPatch,
    },
}

/// The direction a [`PsEditOp::LayerLifecycle`] realizes. A recorded ADD carries `Added` (undo →
/// `inverse` → `Removed` → removes it; redo → `Added` → re-adds); a recorded DELETE carries `Removed`
/// (undo re-adds, redo removes). `inverse` flips the direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleDir {
    Added,
    Removed,
}

impl LifecycleDir {
    /// The opposite direction (used by `LayerLifecycle::inverse`).
    fn flipped(self) -> Self {
        match self {
            LifecycleDir::Added => LifecycleDir::Removed,
            LifecycleDir::Removed => LifecycleDir::Added,
        }
    }
}

/// One reversible metadata/geometry field change on a raster layer, carrying the value `before` the
/// edit and the value `after` it. `apply` always drives the layer to `after`; [`Self::inverted`]
/// swaps the two so an undo drives it back to `before`.
#[derive(Debug, Clone)]
pub(crate) enum LayerFieldPatch {
    Visibility { before: bool, after: bool },
    Opacity { before: f32, after: f32 },
    Transform { before: LayerTransform, after: LayerTransform },
    Deform { before: Option<DeformRec>, after: Option<DeformRec> },
}

impl LayerFieldPatch {
    /// The same field with `before`/`after` swapped (the undo of this patch).
    fn inverted(&self) -> Self {
        match self {
            LayerFieldPatch::Visibility { before, after } => LayerFieldPatch::Visibility {
                before: *after,
                after: *before,
            },
            LayerFieldPatch::Opacity { before, after } => LayerFieldPatch::Opacity {
                before: *after,
                after: *before,
            },
            LayerFieldPatch::Transform { before, after } => LayerFieldPatch::Transform {
                before: *after,
                after: *before,
            },
            LayerFieldPatch::Deform { before, after } => LayerFieldPatch::Deform {
                before: after.clone(),
                after: before.clone(),
            },
        }
    }
}

/// Applies a [`LayerFieldPatch`]'s `after` value to `layer`. The pure metadata/geometry core of a
/// [`PsEditOp::FieldPatch`] apply, extracted so it can be unit-tested without the GUI tab and reused
/// as the no-doc local fallback in `apply_ps_field_patch`. Touches only the field the patch names.
pub(crate) fn apply_field_patch_to_layer(layer: &mut Layer, field: &LayerFieldPatch) {
    match field {
        LayerFieldPatch::Visibility { after, .. } => layer.visible = *after,
        LayerFieldPatch::Opacity { after, .. } => layer.opacity = *after,
        LayerFieldPatch::Transform { after, .. } => layer.transform = *after,
        LayerFieldPatch::Deform { after, .. } => layer.deform = after.clone(),
    }
}

impl ReversibleAction for PsEditOp {
    type Ctx = PsEditorTabState;
    type Err = PsEditOpError;

    fn apply(&mut self, ctx: &mut Self::Ctx) -> Result<(), Self::Err> {
        // A real `match` (no `_ =>`) so any future variant must be handled here.
        match self {
            PsEditOp::RasterPixels {
                page_idx,
                layer_uid,
                diff,
                dir,
                label: _,
            } => ctx.apply_ps_raster_edit(*page_idx, layer_uid, diff.as_ref(), *dir),
            PsEditOp::LayerLifecycle {
                page_idx,
                layer,
                z,
                dir,
            } => ctx.apply_ps_layer_lifecycle(*page_idx, layer, *z, *dir),
            PsEditOp::FieldPatch {
                page_idx,
                layer_uid,
                field,
            } => ctx.apply_ps_field_patch(*page_idx, layer_uid, field),
        }
    }

    fn inverse(&self) -> Self {
        match self {
            PsEditOp::RasterPixels {
                page_idx,
                layer_uid,
                diff,
                dir,
                label,
            } => PsEditOp::RasterPixels {
                page_idx: *page_idx,
                layer_uid: layer_uid.clone(),
                diff: Arc::clone(diff),
                dir: match dir {
                    ApplyDirection::Forward => ApplyDirection::Reverse,
                    ApplyDirection::Reverse => ApplyDirection::Forward,
                },
                label: label.clone(),
            },
            PsEditOp::LayerLifecycle {
                page_idx,
                layer,
                z,
                dir,
            } => PsEditOp::LayerLifecycle {
                page_idx: *page_idx,
                layer: layer.clone(),
                z: *z,
                dir: dir.flipped(),
            },
            PsEditOp::FieldPatch {
                page_idx,
                layer_uid,
                field,
            } => PsEditOp::FieldPatch {
                page_idx: *page_idx,
                layer_uid: layer_uid.clone(),
                field: field.inverted(),
            },
        }
    }

    fn label(&self) -> &str {
        match self {
            PsEditOp::RasterPixels { label, .. } => label,
            PsEditOp::LayerLifecycle { dir, .. } => match dir {
                LifecycleDir::Added => t!("ps_editor.edit_op.add_layer"),
                LifecycleDir::Removed => t!("ps_editor.edit_op.delete_layer"),
            },
            PsEditOp::FieldPatch { field, .. } => match field {
                LayerFieldPatch::Visibility { .. } => t!("ps_editor.edit_op.layer_visibility"),
                LayerFieldPatch::Opacity { .. } => t!("ps_editor.edit_op.layer_opacity"),
                LayerFieldPatch::Transform { .. } => t!("ps_editor.edit_op.layer_transform"),
                LayerFieldPatch::Deform { .. } => t!("ps_editor.edit_op.layer_deform"),
            },
        }
    }

    fn weight(&self) -> usize {
        // Drives the history byte budget.
        match self {
            // The retained cost is the compressed tile payload (the `Arc`/String overhead is
            // negligible).
            PsEditOp::RasterPixels { diff, .. } => diff.compressed_len(),
            // A retained layer's uncompressed display pixels dominate its cost (a deleted large layer
            // must count against the budget). `Color32` is 4 bytes/pixel.
            PsEditOp::LayerLifecycle { layer, .. } => layer.image.pixels.len().saturating_mul(4),
            // Negligible: a few scalars / a small mesh.
            PsEditOp::FieldPatch { .. } => 0,
        }
    }
}

/// Error raised while applying a [`PsEditOp`] to a [`PsEditorTabState`].
///
/// Kept typed and panic-free: the target layer/page not being resident and a raster
/// apply failure (size mismatch / corrupt payload) are the only failure modes, and
/// both are surfaced to the caller (which treats them as "nothing changed") rather
/// than aborting.
#[derive(Debug)]
pub(crate) enum PsEditOpError {
    /// The target page/layer is not resident (no stack, wrong page, or the uid left
    /// the stack), so the diff cannot be applied.
    NotResident {
        /// The page index the op targeted.
        page_idx: usize,
    },
    /// The underlying `RasterDiff` operation failed (size mismatch, corrupt payload,
    /// dimension overflow, ...).
    Raster(RasterDiffError),
}

impl fmt::Display for PsEditOpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PsEditOpError::NotResident { page_idx } => {
                write!(f, "ps_editor page {page_idx} is not resident for undo/redo")
            }
            PsEditOpError::Raster(err) => write!(f, "raster diff failed: {err}"),
        }
    }
}

impl std::error::Error for PsEditOpError {}

impl From<RasterDiffError> for PsEditOpError {
    fn from(err: RasterDiffError) -> Self {
        PsEditOpError::Raster(err)
    }
}

/// Applies `diff` in `dir` to `layer.image` and mirrors the changed pixels into
/// `layer.base_image`, preserving the editable-raster `base_image == image`
/// invariant. Returns the changed rects (image coordinates) for tile invalidation.
///
/// This is the pure pixel core of a [`PsEditOp::RasterPixels`] apply, extracted so it
/// can be unit-tested without the GUI tab struct. Works on the premultiplied
/// `Color32` bytes directly.
///
/// # Errors
/// Returns [`RasterDiffError`] if the diff cannot be applied (buffer length / image
/// size mismatch, corrupt payload, or dimensions that overflow `u32`). Never panics.
pub(crate) fn apply_raster_diff_to_layer(
    layer: &mut Layer,
    diff: &RasterDiff,
    dir: ApplyDirection,
) -> Result<Vec<DirtyRect>, RasterDiffError> {
    let size = layer.image.size;
    let image_size = [
        u32::try_from(size[0]).map_err(|_| RasterDiffError::DimensionOverflow)?,
        u32::try_from(size[1]).map_err(|_| RasterDiffError::DimensionOverflow)?,
    ];
    // Apply to the display buffer (primary). A page/layer resized since the edit
    // surfaces as a `RasterDiff` size error here rather than a panic.
    let dirty = diff.apply(layer.image.as_raw_mut(), image_size, dir)?;
    // Keep base_image byte-consistent with image over the changed rects: an editable
    // raster requires base_image == image, and both were equal before the op.
    mirror_rects_image_to_base(layer, &dirty);
    Ok(dirty)
}

/// Copies `layer.image` pixels into `layer.base_image` over each `dirty` rect,
/// re-establishing `base_image == image`. Out-of-bounds rects are clamped; a
/// base/image size mismatch (never expected for an editable raster) is a no-op.
fn mirror_rects_image_to_base(layer: &mut Layer, dirty: &[DirtyRect]) {
    let Layer {
        image, base_image, ..
    } = layer;
    if image.size != base_image.size {
        return;
    }
    let w = image.size[0];
    let h = image.size[1];
    for rect in dirty {
        let ox = rect.origin_px[0] as usize;
        let oy = rect.origin_px[1] as usize;
        let rw = rect.size_px[0] as usize;
        let rh = rect.size_px[1] as usize;
        let cols = rw.min(w.saturating_sub(ox));
        if cols == 0 {
            continue;
        }
        for row in 0..rh {
            let y = oy + row;
            if y >= h {
                break;
            }
            let start = y * w + ox;
            let (Some(src), Some(dst)) = (
                image.pixels.get(start..start + cols),
                base_image.pixels.get_mut(start..start + cols),
            ) else {
                continue;
            };
            dst.copy_from_slice(src);
        }
    }
}

/// Extracts a region-local, row-major premultiplied-RGBA8 buffer (`w*h*4` bytes)
/// from `image` at `(x, y)`. Used to build a bounded region [`RasterDiff`] over just
/// a brush stroke's dirty union. Out-of-bounds rows/cols read as transparent zeros.
pub(crate) fn copy_region_premul(
    image: &ColorImage,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
) -> Vec<u8> {
    let iw = image.size[0];
    let ih = image.size[1];
    let raw = image.as_raw();
    let mut out = vec![0u8; w.saturating_mul(h).saturating_mul(4)];
    let cols = w.min(iw.saturating_sub(x));
    if cols == 0 {
        return out;
    }
    for row in 0..h {
        let sy = y + row;
        if sy >= ih {
            break;
        }
        let src = (sy * iw + x) * 4;
        let dst = row * w * 4;
        let (Some(src_slice), Some(dst_slice)) = (
            raw.get(src..src + cols * 4),
            out.get_mut(dst..dst + cols * 4),
        ) else {
            continue;
        };
        dst_slice.copy_from_slice(src_slice);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tabs::ps_editor::layers::LayerStack;
    use eframe::egui::Color32;

    /// Builds a stack with one editable raster layer of `size`, returning its id.
    fn raster_stack(size: [usize; 2]) -> (LayerStack, u64) {
        let img = ColorImage::filled(size, Color32::TRANSPARENT);
        let mut stack = LayerStack::new(0, size, img.clone(), img);
        let id = stack.add_raster_layer();
        (stack, id)
    }

    /// Builds a Forward diff capturing the change from the layer's `base_image`
    /// (before) to its `image` (after) — the exact recording done at brush commit.
    fn diff_from_layer(layer: &Layer) -> RasterDiff {
        let size = [layer.image.size[0] as u32, layer.image.size[1] as u32];
        RasterDiff::from_rgba(layer.base_image.as_raw(), layer.image.as_raw(), size, 1024)
            .expect("diff builds from equal-sized buffers")
    }

    #[test]
    fn undo_and_redo_restore_both_image_and_base() {
        let size = [8, 8];
        let (mut stack, id) = raster_stack(size);
        let layer = stack.layer_mut(id).expect("raster resident");

        // Pre-stroke: base_image == image == fully transparent.
        assert!(layer.image.pixels.iter().all(|p| *p == Color32::TRANSPARENT));

        // Paint a couple of pixels into `image` only (mirrors the live brush), so
        // base_image still holds the pre-stroke "before".
        layer.image.pixels[0] = Color32::RED;
        layer.image.pixels[9] = Color32::RED;
        let diff = diff_from_layer(layer);
        assert!(!diff.is_empty(), "a real change must produce a non-empty diff");

        // Undo (Reverse): both image and base_image return to transparent.
        let dirty = apply_raster_diff_to_layer(layer, &diff, ApplyDirection::Reverse)
            .expect("reverse applies");
        assert!(!dirty.is_empty());
        assert!(
            layer.image.pixels.iter().all(|p| *p == Color32::TRANSPARENT),
            "image restored to pre-stroke"
        );
        assert!(
            layer
                .base_image
                .pixels
                .iter()
                .all(|p| *p == Color32::TRANSPARENT),
            "base_image restored to pre-stroke"
        );

        // Redo (Forward): both image and base_image reproduce the painted pixels.
        apply_raster_diff_to_layer(layer, &diff, ApplyDirection::Forward).expect("forward applies");
        assert_eq!(layer.image.pixels[0], Color32::RED);
        assert_eq!(layer.image.pixels[9], Color32::RED);
        assert_eq!(
            layer.base_image.pixels[0], layer.image.pixels[0],
            "base tracks image after redo"
        );
        assert_eq!(layer.base_image.pixels[9], layer.image.pixels[9]);
    }

    #[test]
    fn region_diff_round_trips_through_the_stroke_union() {
        // Mirrors the region-bounded recording path: build the diff from a region of
        // base_image (before) and image (after), then undo/redo through it.
        let size = [16, 16];
        let (mut stack, id) = raster_stack(size);
        let layer = stack.layer_mut(id).expect("raster resident");
        // Paint inside a 4x4 union at origin (5,6).
        for row in 6..10 {
            for col in 5..9 {
                layer.image.pixels[row * size[0] + col] = Color32::BLUE;
            }
        }
        let before = copy_region_premul(&layer.base_image, 5, 6, 4, 4);
        let after = copy_region_premul(&layer.image, 5, 6, 4, 4);
        let diff = RasterDiff::from_region_pixels(
            &before,
            &after,
            [5, 6],
            [4, 4],
            [size[0] as u32, size[1] as u32],
            1024,
        )
        .expect("region diff builds");
        assert!(!diff.is_empty());

        apply_raster_diff_to_layer(layer, &diff, ApplyDirection::Reverse).expect("reverse");
        assert!(layer.image.pixels.iter().all(|p| *p == Color32::TRANSPARENT));
        assert!(layer.base_image.pixels.iter().all(|p| *p == Color32::TRANSPARENT));

        apply_raster_diff_to_layer(layer, &diff, ApplyDirection::Forward).expect("forward");
        assert_eq!(layer.image.pixels[6 * size[0] + 5], Color32::BLUE);
        assert_eq!(layer.base_image.pixels[6 * size[0] + 5], Color32::BLUE);
    }

    #[test]
    fn empty_diff_is_a_no_op() {
        // No pixels changed: the diff is empty and applying it touches nothing.
        let size = [4, 4];
        let (mut stack, id) = raster_stack(size);
        let layer = stack.layer_mut(id).expect("raster resident");
        let diff = diff_from_layer(layer);
        assert!(diff.is_empty(), "identical before/after produces an empty diff");
        let dirty =
            apply_raster_diff_to_layer(layer, &diff, ApplyDirection::Reverse).expect("no-op applies");
        assert!(dirty.is_empty());
        assert!(layer.image.pixels.iter().all(|p| *p == Color32::TRANSPARENT));
    }

    /// Pure `LayerStack`-level realization of a `LayerLifecycle` op, mirroring the doc-driven
    /// `apply_ps_layer_lifecycle` but without the GUI tab: `Added` re-inserts `layer` (preserving its
    /// uid + pixels), `Removed` deletes the resident layer with the same uid. Test-only helper.
    fn apply_lifecycle_to_stack(stack: &mut LayerStack, layer: &Layer, dir: LifecycleDir) {
        match dir {
            LifecycleDir::Removed => {
                if let Some(id) = stack
                    .layers()
                    .iter()
                    .find(|l| l.uid == layer.uid)
                    .map(|l| l.id)
                {
                    stack.remove_layer(id);
                }
            }
            LifecycleDir::Added => {
                let id = stack.add_raster_layer_image(
                    layer.name.clone(),
                    layer.image.clone(),
                    layer.transform,
                );
                if let Some(l) = stack.layer_mut(id) {
                    l.uid = layer.uid;
                    l.base_image = layer.base_image.clone();
                    l.visible = layer.visible;
                    l.opacity = layer.opacity;
                }
            }
        }
    }

    #[test]
    fn field_patch_round_trips_visibility_opacity_transform() {
        let size = [4, 4];
        let (mut stack, id) = raster_stack(size);
        let layer = stack.layer_mut(id).expect("raster resident");
        layer.visible = true;
        layer.opacity = 1.0;
        let t0 = layer.transform;
        let t1 = LayerTransform {
            center: eframe::egui::Vec2::new(10.0, 20.0),
            rotation: 0.5,
            scale: 2.0,
        };

        // Visibility: forward drives to `after=false`, inverse drives back to `before=true`.
        let vis = LayerFieldPatch::Visibility {
            before: true,
            after: false,
        };
        apply_field_patch_to_layer(layer, &vis);
        assert!(!layer.visible, "forward applies `after`");
        apply_field_patch_to_layer(layer, &vis.inverted());
        assert!(layer.visible, "inverse restores `before`");

        // Opacity.
        let op = LayerFieldPatch::Opacity {
            before: 1.0,
            after: 0.25,
        };
        apply_field_patch_to_layer(layer, &op);
        assert!((layer.opacity - 0.25).abs() < 1e-6);
        apply_field_patch_to_layer(layer, &op.inverted());
        assert!((layer.opacity - 1.0).abs() < 1e-6);

        // Transform.
        let tp = LayerFieldPatch::Transform {
            before: t0,
            after: t1,
        };
        apply_field_patch_to_layer(layer, &tp);
        assert_eq!(layer.transform, t1, "forward applies `after`");
        apply_field_patch_to_layer(layer, &tp.inverted());
        assert_eq!(layer.transform, t0, "inverse restores `before`");
    }

    #[test]
    fn field_patch_round_trips_deform() {
        let size = [4, 4];
        let (mut stack, id) = raster_stack(size);
        let layer = stack.layer_mut(id).expect("raster resident");
        assert!(layer.deform.is_none());
        let grid = DeformRec {
            cols: 2,
            rows: 2,
            points_px: vec![[0.0, 0.0], [4.0, 0.0], [0.0, 4.0], [4.0, 4.0]],
        };
        let patch = LayerFieldPatch::Deform {
            before: None,
            after: Some(grid.clone()),
        };
        apply_field_patch_to_layer(layer, &patch);
        let applied = layer.deform.as_ref().expect("deform set by forward");
        assert_eq!(applied.points_px, grid.points_px);
        apply_field_patch_to_layer(layer, &patch.inverted());
        assert!(layer.deform.is_none(), "inverse clears the deform");
    }

    #[test]
    fn layer_lifecycle_round_trips_add_and_delete() {
        // Delete → undo re-adds with the SAME uid and pixels intact.
        let size = [4, 4];
        let (mut stack, id) = raster_stack(size);
        // Paint a marker pixel so we can verify pixel retention across the round-trip.
        {
            let layer = stack.layer_mut(id).expect("raster resident");
            layer.image.pixels[5] = Color32::RED;
            layer.base_image.pixels[5] = Color32::RED;
        }
        let captured = stack.layer(id).expect("resident").clone();
        let uid = captured.uid;
        assert_eq!(stack.raster_count(), 1);

        // Forward DELETE (dir = Removed).
        apply_lifecycle_to_stack(&mut stack, &captured, LifecycleDir::Removed);
        assert_eq!(stack.raster_count(), 0, "delete removes the raster");

        // Undo of a delete = inverse = Added → re-adds the layer with its pixels.
        apply_lifecycle_to_stack(&mut stack, &captured, LifecycleDir::Added);
        assert_eq!(stack.raster_count(), 1);
        let re = stack
            .layers()
            .iter()
            .find(|l| l.uid == uid)
            .expect("re-added with same uid");
        assert_eq!(re.image.pixels[5], Color32::RED, "pixels survive the round-trip");

        // Redo of the delete = Removed again.
        apply_lifecycle_to_stack(&mut stack, &captured, LifecycleDir::Removed);
        assert_eq!(stack.raster_count(), 0);
    }

    #[test]
    fn lifecycle_inverse_flips_dir_and_weight_counts_retained_pixels() {
        let size = [8, 8];
        let (stack, id) = raster_stack(size);
        let layer = stack.layer(id).expect("resident").clone();
        let op = PsEditOp::LayerLifecycle {
            page_idx: 0,
            layer: Box::new(layer),
            z: 3,
            dir: LifecycleDir::Removed,
        };
        // `inverse` flips Removed → Added, preserving page/z.
        match op.inverse() {
            PsEditOp::LayerLifecycle { dir, z, page_idx, .. } => {
                assert_eq!(dir, LifecycleDir::Added);
                assert_eq!(z, 3);
                assert_eq!(page_idx, 0);
            }
            PsEditOp::RasterPixels { .. } | PsEditOp::FieldPatch { .. } => {
                panic!("inverse of a lifecycle op must stay a lifecycle op")
            }
        }
        // Weight reflects the retained display pixels (8*8*4 bytes), so a big deleted layer is budgeted.
        assert_eq!(op.weight(), 8 * 8 * 4);
        // A field patch is negligible.
        let fp = PsEditOp::FieldPatch {
            page_idx: 0,
            layer_uid: "x".to_string(),
            field: LayerFieldPatch::Visibility {
                before: true,
                after: false,
            },
        };
        assert_eq!(fp.weight(), 0);
    }
}
