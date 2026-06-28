/*
File: tabs/ps_editor/layers.rs

Purpose:
Layer model for the PS-like editor. Holds an ordered stack of layers for a single page plus the
invariants that distinguish the two locked base layers (source + clean) from user raster layers.

Key structures:
- `LayerId`: stable per-session identifier for a layer.
- `LayerKind`: `Source` / `Clean` / `Raster`.
- `LayerTransform`: per-layer affine placement (center/rotation/scale) in page space.
- `Layer`: name, kind, visibility, opacity, RGBA buffer, and its `LayerTransform`.
- `LayerStack`: ordered layers (index 0 = bottom) plus the active layer and id allocator.

Notes:
The stack always begins with `Source` (bottom) and `Clean`, both locked: they can be hidden but
never deleted, reordered, painted on, or transformed. Any number of `Raster` layers stack above
them. Base layers are page-sized with the identity transform; raster layers may be smaller than
the page ("incomplete") and carry an arbitrary transform so they can be moved/rotated/scaled.
*/

use eframe::egui;
use eframe::egui::{ColorImage, Pos2, Vec2};

/// Stable per-session layer identifier. The durable identity is `Layer::uid`.
pub type LayerId = u64;

/// Per-session group identifier. The durable identity is `LayerGroup::uid`.
pub type GroupId = u64;

/// A layer group (folder). Phase-2 groups are single-level: a raster layer belongs to at most one
/// group, base layers never belong to one. A hidden group hides its members and its opacity
/// multiplies each member's opacity at composite time.
#[derive(Debug, Clone)]
pub struct LayerGroup {
    pub id: GroupId,
    pub uid: uuid::Uuid,
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    /// Panel-only: children hidden in the unified layers tree. Does not affect compositing.
    pub collapsed: bool,
}

/// Rotates `v` by `angle` radians (clockwise in image space, where +y is down).
fn rotate_vec(v: Vec2, angle: f32) -> Vec2 {
    let (s, c) = angle.sin_cos();
    Vec2::new(v.x * c - v.y * s, v.x * s + v.y * c)
}

/// Affine placement of a layer's intrinsic image in page (world) pixel space.
///
/// A layer image of size `[w, h]` is positioned by anchoring its center at `center` (page px),
/// then applying a uniform `scale` and a `rotation` about that center. Identity (`center =
/// image_center`, `rotation = 0`, `scale = 1`) places the image 1:1 over the page, matching the
/// pre-transform behavior. Base layers always keep the identity transform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerTransform {
    /// World (page px) position of the layer image's center.
    pub center: Vec2,
    /// Rotation in radians about the center.
    pub rotation: f32,
    /// Uniform scale factor.
    pub scale: f32,
}

impl LayerTransform {
    /// Identity transform for an image of `size` (image px): centered, unrotated, unscaled.
    #[must_use]
    pub fn identity_for(size: [usize; 2]) -> Self {
        Self {
            center: Vec2::new(size[0] as f32 * 0.5, size[1] as f32 * 0.5),
            rotation: 0.0,
            scale: 1.0,
        }
    }

    /// True when this transform places the image 1:1 over the page (no rotation/scale/offset).
    #[must_use]
    pub fn is_identity_for(&self, size: [usize; 2]) -> bool {
        *self == Self::identity_for(size)
    }
}

/// Distinguishes the read-only base layers from editable raster layers.
///
/// `Source` mirrors the page source image; `Clean` mirrors the shared clean overlay. Both are
/// locked. `Raster` layers are user-owned, editable, and removable. Text overlays are NOT
/// `LayerStack` layers — they live as the doc's Text nodes and the tab's `text_layers` projection,
/// and become rasters only when baked.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum LayerKind {
    Source,
    Clean,
    Raster,
}

impl LayerKind {
    /// Base layers are locked: hideable but not editable, movable, or deletable.
    #[must_use]
    pub fn is_base(self) -> bool {
        matches!(self, LayerKind::Source | LayerKind::Clean)
    }
}

/// A single layer: metadata plus its RGBA buffer and page-space placement.
#[derive(Debug, Clone)]
pub struct Layer {
    pub id: LayerId,
    /// Stable cross-session identifier persisted to disk and used for cross-tab references. Base
    /// layers carry one for uniformity but are never serialized.
    pub uid: uuid::Uuid,
    pub name: String,
    pub kind: LayerKind,
    pub visible: bool,
    /// Layer opacity in `0.0..=1.0`, applied at composite time.
    pub opacity: f32,
    /// RGBA DISPLAY image. Base layers are page-sized; raster layers may be smaller ("incomplete").
    /// Equals `base_image` when `effects` is empty, otherwise the rendered (post-effects) result.
    pub image: ColorImage,
    /// Pre-effects (base) pixels. The non-destructive effects chain renders from these, so applying
    /// or clearing effects is fully reversible. Equals `image` when `effects` is empty.
    pub base_image: ColorImage,
    /// Non-destructive post-effects chain (typing-tab effects contract). Empty = no effects, in which
    /// case `image == base_image`. Mirrors the typing tab's reversible effects storage.
    pub effects: Vec<serde_json::Value>,
    /// Placement of `image` in page space. Always identity for base layers.
    pub transform: LayerTransform,
    /// Optional mesh-deform grid (cols×rows control points in absolute page px, row-major, ≥2×2).
    /// When `Some`, the layer is positioned and rendered through this mesh and its affine
    /// `transform` does not apply (mirrors the deformed-text rule). `None` = plain affine raster.
    /// Always `None` for base layers.
    pub deform: Option<crate::models::layer_model::manifest::DeformRec>,
    /// Group membership, if any. Always `None` for base layers.
    pub group: Option<GroupId>,
    /// True when this raster's *base* pixels were edited this session (paint / cut / merge / effects
    /// bake), so a save must rewrite its base PNG and drop any non-destructive effects chain. False
    /// for a freshly-loaded layer, so a save preserves another tab's effects. See
    /// `models::layer_model::persist::save_page_rasters`.
    pub pixels_dirty: bool,
}

impl Layer {
    /// True when the layer accepts direct base-pixel edits (paint / cut / merge) *right now*. A
    /// raster showing a non-destructive effects chain must be baked first (flatten the rendered
    /// display into the base and clear the chain), so it is not directly editable until then.
    #[must_use]
    pub fn can_edit_pixels(&self) -> bool {
        matches!(self.kind, LayerKind::Raster) && self.effects.is_empty()
    }

    /// True when the layer can be freely moved/rotated/scaled (raster and text layers — the shared
    /// transform mechanism; base layers are locked).
    #[must_use]
    pub fn is_transformable(&self) -> bool {
        !self.kind.is_base()
    }

    /// Image size in pixels as a float vector.
    #[must_use]
    pub fn image_size(&self) -> Vec2 {
        Vec2::new(self.image.size[0] as f32, self.image.size[1] as f32)
    }

    /// Maps a layer-local pixel point to its page (world) position through the transform.
    #[must_use]
    pub fn local_to_world(&self, local: Vec2) -> Vec2 {
        let local_center = self.image_size() * 0.5;
        self.transform.center
            + rotate_vec((local - local_center) * self.transform.scale, self.transform.rotation)
    }

    /// Maps a page (world) point back to layer-local pixel coordinates.
    #[must_use]
    pub fn world_to_local(&self, world: Vec2) -> Vec2 {
        let local_center = self.image_size() * 0.5;
        let scale = if self.transform.scale.abs() < f32::EPSILON {
            f32::EPSILON
        } else {
            self.transform.scale
        };
        local_center + rotate_vec(world - self.transform.center, -self.transform.rotation) / scale
    }

    /// Builds an identity `cols`×`rows` deform grid (≥2×2) spanning this layer's current affine
    /// footprint in page px: a bilinear lerp across the four `world_corners()`. Dragging the
    /// resulting handles starts from exactly the affine placement, so entering deform mode is a
    /// no-op until the user moves a point. Row-major order (matching `DeformRec`).
    #[must_use]
    pub fn identity_deform_grid(
        &self,
        cols: usize,
        rows: usize,
    ) -> crate::models::layer_model::manifest::DeformRec {
        let cols = cols.max(2);
        let rows = rows.max(2);
        let c = self.world_corners(); // tl, tr, br, bl
        let mut points_px = Vec::with_capacity(cols * rows);
        for r in 0..rows {
            let tv = r as f32 / (rows - 1) as f32;
            // Left and right edge points at this row (tl->bl and tr->br).
            let left = c[0].to_vec2() * (1.0 - tv) + c[3].to_vec2() * tv;
            let right = c[1].to_vec2() * (1.0 - tv) + c[2].to_vec2() * tv;
            for col in 0..cols {
                let tu = col as f32 / (cols - 1) as f32;
                let p = left * (1.0 - tu) + right * tu;
                points_px.push([p.x, p.y]);
            }
        }
        crate::models::layer_model::manifest::DeformRec {
            cols,
            rows,
            points_px,
        }
    }

    /// The four image corners in page space (top-left, top-right, bottom-right, bottom-left).
    #[must_use]
    pub fn world_corners(&self) -> [Pos2; 4] {
        let size = self.image_size();
        [
            self.local_to_world(Vec2::ZERO).to_pos2(),
            self.local_to_world(Vec2::new(size.x, 0.0)).to_pos2(),
            self.local_to_world(Vec2::new(size.x, size.y)).to_pos2(),
            self.local_to_world(Vec2::new(0.0, size.y)).to_pos2(),
        ]
    }
}

/// Ordered, single-page layer stack with base-layer invariants.
///
/// `layers[0]` is the bottom (source); `layers[1]` is the clean layer. Raster layers occupy
/// indices `>= 2`. The stack is rebuilt whenever the active page changes.
#[derive(Debug, Clone)]
pub struct LayerStack {
    page_idx: usize,
    size: [usize; 2],
    layers: Vec<Layer>,
    groups: Vec<LayerGroup>,
    active: LayerId,
    next_id: LayerId,
    next_group_id: GroupId,
}

impl LayerStack {
    /// Builds a stack with the two locked base layers from their RGBA buffers.
    ///
    /// Both buffers must have dimensions equal to `size`. The clean layer starts active only if no
    /// raster layer exists yet; callers usually re-add session raster layers afterwards.
    #[must_use]
    pub fn new(page_idx: usize, size: [usize; 2], source: ColorImage, clean: ColorImage) -> Self {
        let source_layer = Layer {
            id: 0,
            uid: uuid::Uuid::new_v4(),
            name: "Исходник".to_string(),
            kind: LayerKind::Source,
            visible: true,
            opacity: 1.0,
            transform: LayerTransform::identity_for(source.size),
            base_image: source.clone(),
            image: source,
            effects: Vec::new(),
            deform: None,
            group: None,
            pixels_dirty: false,
        };
        let clean_layer = Layer {
            id: 1,
            uid: uuid::Uuid::new_v4(),
            name: "Клин".to_string(),
            kind: LayerKind::Clean,
            visible: true,
            opacity: 1.0,
            transform: LayerTransform::identity_for(clean.size),
            base_image: clean.clone(),
            image: clean,
            effects: Vec::new(),
            deform: None,
            group: None,
            pixels_dirty: false,
        };
        Self {
            page_idx,
            size,
            layers: vec![source_layer, clean_layer],
            groups: Vec::new(),
            active: 1,
            next_id: 2,
            next_group_id: 1,
        }
    }

    #[must_use]
    pub fn page_idx(&self) -> usize {
        self.page_idx
    }

    #[must_use]
    pub fn size(&self) -> [usize; 2] {
        self.size
    }

    /// Layers ordered bottom-to-top (index 0 = bottom).
    #[must_use]
    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// Clears the `pixels_dirty` flag on every raster layer. Call after the stack's rasters have
    /// been persisted (their base PNGs are now on disk), so a later save treats them as clean and
    /// does not rewrite their bases — which would otherwise clobber a non-destructive effects chain
    /// another tab (e.g. the typing tab) added on top in the meantime.
    pub fn mark_rasters_persisted(&mut self) {
        for layer in &mut self.layers {
            if layer.kind == LayerKind::Raster {
                layer.pixels_dirty = false;
            }
        }
    }

    #[must_use]
    pub fn active_id(&self) -> LayerId {
        self.active
    }

    /// Selects the active layer if `id` exists.
    pub fn set_active(&mut self, id: LayerId) {
        if self.layers.iter().any(|layer| layer.id == id) {
            self.active = id;
        }
    }

    /// Mutable access to the active layer when its base pixels are directly editable right now.
    ///
    /// Returns `None` for a locked base layer (so painting tools cannot mutate the source/clean
    /// buffers) *and* for a raster still showing a non-destructive effects chain (it must be baked
    /// first). Uses `can_edit_pixels` so the brush refuses to paint into an effected raster.
    pub fn active_editable_mut(&mut self) -> Option<&mut Layer> {
        let active = self.active;
        self.layers
            .iter_mut()
            .find(|layer| layer.id == active && layer.can_edit_pixels())
    }

    pub fn layer(&self, id: LayerId) -> Option<&Layer> {
        self.layers.iter().find(|layer| layer.id == id)
    }

    pub fn layer_mut(&mut self, id: LayerId) -> Option<&mut Layer> {
        self.layers.iter_mut().find(|layer| layer.id == id)
    }

    /// Adds a transparent raster layer on top, makes it active, and returns its id.
    pub fn add_raster_layer(&mut self) -> LayerId {
        let id = self.next_id;
        self.next_id += 1;
        let raster_index = self.raster_count() + 1;
        let layer = Layer {
            id,
            uid: uuid::Uuid::new_v4(),
            name: format!("Слой {raster_index}"),
            kind: LayerKind::Raster,
            visible: true,
            opacity: 1.0,
            transform: LayerTransform::identity_for(self.size),
            base_image: ColorImage::filled(self.size, egui::Color32::TRANSPARENT),
            image: ColorImage::filled(self.size, egui::Color32::TRANSPARENT),
            effects: Vec::new(),
            deform: None,
            group: None,
            pixels_dirty: true,
        };
        self.layers.push(layer);
        self.active = id;
        id
    }

    /// Adds a raster layer on top from an existing image (any size — layers may be "incomplete")
    /// placed by `transform`, makes it active, and returns its id.
    pub fn add_raster_layer_image(
        &mut self,
        name: String,
        image: ColorImage,
        transform: LayerTransform,
    ) -> LayerId {
        let id = self.next_id;
        self.next_id += 1;
        self.layers.push(Layer {
            id,
            uid: uuid::Uuid::new_v4(),
            name,
            kind: LayerKind::Raster,
            visible: true,
            opacity: 1.0,
            transform,
            base_image: image.clone(),
            image,
            effects: Vec::new(),
            deform: None,
            group: None,
            // Loaders (`load_persisted_into_stack` / merge) reset this to false after adding; the
            // clip/paste path marks it dirty. New PS-owned content is dirty by virtue of being new.
            pixels_dirty: false,
        });
        self.active = id;
        id
    }

    /// Mutable access to the active layer when it is a transformable raster layer.
    pub fn active_transformable_mut(&mut self) -> Option<&mut Layer> {
        let active = self.active;
        self.layers
            .iter_mut()
            .find(|layer| layer.id == active && layer.is_transformable())
    }

    /// Removes a raster layer by id. Base layers are never removed.
    ///
    /// Returns `true` when a layer was removed. The active selection falls back to the topmost
    /// remaining layer.
    pub fn remove_layer(&mut self, id: LayerId) -> bool {
        let Some(index) = self.layers.iter().position(|layer| layer.id == id) else {
            return false;
        };
        if self.layers[index].kind.is_base() {
            return false;
        }
        self.layers.remove(index);
        if self.active == id {
            self.active = self.layers.last().map(|layer| layer.id).unwrap_or(1);
        }
        true
    }

    /// All groups defined on this page.
    #[must_use]
    pub fn groups(&self) -> &[LayerGroup] {
        &self.groups
    }

    #[must_use]
    pub fn group(&self, id: GroupId) -> Option<&LayerGroup> {
        self.groups.iter().find(|g| g.id == id)
    }

    pub fn group_mut(&mut self, id: GroupId) -> Option<&mut LayerGroup> {
        self.groups.iter_mut().find(|g| g.id == id)
    }

    /// Creates a new empty group and returns its id.
    pub fn add_group(&mut self, name: String) -> GroupId {
        let id = self.next_group_id;
        self.next_group_id += 1;
        self.groups.push(LayerGroup {
            id,
            uid: uuid::Uuid::new_v4(),
            name,
            visible: true,
            opacity: 1.0,
            collapsed: false,
        });
        id
    }

    /// Finds a group by its stable uid string.
    #[must_use]
    pub fn group_by_uid(&self, uid: &str) -> Option<&LayerGroup> {
        self.groups.iter().find(|g| g.uid.to_string() == uid)
    }

    /// Creates a new group carrying an explicit stable `uid` (so it matches the on-disk `GroupRec`),
    /// and returns its session id. Used when mirroring a persisted grouping edit into the stack.
    pub fn add_group_with_uid(&mut self, name: String, uid: uuid::Uuid) -> GroupId {
        let id = self.add_group(name);
        if let Some(group) = self.group_mut(id) {
            group.uid = uid;
        }
        id
    }

    /// Resolves the stable uid of the group a layer belongs to, if any.
    #[must_use]
    pub fn layer_group_uid(&self, layer_id: LayerId) -> Option<String> {
        self.layer(layer_id)
            .and_then(|l| l.group)
            .and_then(|gid| self.group(gid).map(|g| g.uid.to_string()))
    }

    /// Removes a group, ungrouping its members (their pixels and order are untouched).
    pub fn remove_group(&mut self, id: GroupId) {
        for layer in &mut self.layers {
            if layer.group == Some(id) {
                layer.group = None;
            }
        }
        self.groups.retain(|g| g.id != id);
    }

    /// Assigns a raster layer to a group (or `None` to ungroup). Base layers are never grouped and
    /// an unknown group id is ignored.
    pub fn set_layer_group(&mut self, layer_id: LayerId, group: Option<GroupId>) {
        if let Some(group) = group
            && !self.groups.iter().any(|g| g.id == group)
        {
            return;
        }
        if let Some(layer) = self.layers.iter_mut().find(|l| l.id == layer_id)
            && !layer.kind.is_base()
        {
            layer.group = group;
        }
    }

    /// Effective visibility at composite time, factoring in the layer's group.
    #[must_use]
    pub fn layer_visible(&self, layer: &Layer) -> bool {
        layer.visible
            && layer
                .group
                .and_then(|g| self.group(g))
                .is_none_or(|g| g.visible)
    }

    /// Effective opacity at composite time: the layer opacity scaled by its group's opacity.
    #[must_use]
    pub fn layer_opacity(&self, layer: &Layer) -> f32 {
        let group_opacity = layer
            .group
            .and_then(|g| self.group(g))
            .map_or(1.0, |g| g.opacity);
        layer.opacity * group_opacity
    }

    /// Reorders the raster layers (indices `>= 2`) to match `order` (bottom-to-top), leaving the base
    /// layers in place. Ids in `order` that are not raster layers are ignored; raster layers absent
    /// from `order` are appended on top in their current relative order (defensive — callers pass the
    /// full set). Used to project the unified `LayerDoc` z order onto the stack.
    pub fn reorder_rasters(&mut self, order: &[LayerId]) {
        // Pull out the raster layers, keyed by id.
        let mut rasters: Vec<Layer> = Vec::new();
        self.layers.retain(|l| {
            if l.kind == LayerKind::Raster {
                rasters.push(l.clone());
                false
            } else {
                true
            }
        });
        let mut by_id: std::collections::HashMap<LayerId, Layer> =
            rasters.into_iter().map(|l| (l.id, l)).collect();
        for id in order {
            if let Some(layer) = by_id.remove(id) {
                self.layers.push(layer);
            }
        }
        // Any rasters not named in `order` keep their relative order on top.
        let mut leftover: Vec<Layer> = by_id.into_values().collect();
        leftover.sort_by_key(|l| l.id);
        self.layers.extend(leftover);
    }

    /// Number of editable raster layers in the stack.
    #[must_use]
    pub fn raster_count(&self) -> usize {
        self.layers
            .iter()
            .filter(|layer| layer.kind == LayerKind::Raster)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Color32;

    fn stack() -> LayerStack {
        let size = [4, 4];
        let img = ColorImage::filled(size, Color32::TRANSPARENT);
        LayerStack::new(0, size, img.clone(), img)
    }

    #[test]
    fn base_layers_are_locked_and_undeletable() {
        let mut s = stack();
        assert_eq!(s.layers().len(), 2);
        assert!(!s.remove_layer(0), "source must not be removable");
        assert!(!s.remove_layer(1), "clean must not be removable");
        assert_eq!(s.layers().len(), 2);
        // Base layers report not editable so brush tools skip them.
        assert!(!s.layers()[0].can_edit_pixels());
        assert!(!s.layers()[1].can_edit_pixels());
    }

    #[test]
    fn identity_deform_grid_spans_the_affine_footprint() {
        // A raster placed by an affine transform; its identity 3x3 grid must reproduce the four
        // affine corners exactly (entering deform mode is a no-op until a handle is dragged).
        let mut s = stack();
        let id = s.add_raster_layer_image(
            "r".into(),
            ColorImage::filled([10, 6], Color32::WHITE),
            LayerTransform { center: Vec2::new(100.0, 50.0), rotation: 0.3, scale: 2.0 },
        );
        let layer = s.layer(id).unwrap();
        let corners = layer.world_corners(); // tl, tr, br, bl
        let grid = layer.identity_deform_grid(3, 3);
        assert_eq!(grid.cols, 3);
        assert_eq!(grid.rows, 3);
        assert_eq!(grid.points_px.len(), 9);
        let near = |p: [f32; 2], q: Pos2| (p[0] - q.x).abs() < 1e-3 && (p[1] - q.y).abs() < 1e-3;
        // Grid corners (row-major 3x3): idx 0=tl, 2=tr, 8=br, 6=bl.
        assert!(near(grid.points_px[0], corners[0]), "top-left matches");
        assert!(near(grid.points_px[2], corners[1]), "top-right matches");
        assert!(near(grid.points_px[8], corners[2]), "bottom-right matches");
        assert!(near(grid.points_px[6], corners[3]), "bottom-left matches");
        // Center point is the average of the four corners.
        let cx = (corners[0].x + corners[1].x + corners[2].x + corners[3].x) / 4.0;
        let cy = (corners[0].y + corners[1].y + corners[2].y + corners[3].y) / 4.0;
        assert!((grid.points_px[4][0] - cx).abs() < 1e-3, "center x");
        assert!((grid.points_px[4][1] - cy).abs() < 1e-3, "center y");
        // Minimum 2x2 is enforced.
        assert_eq!(layer.identity_deform_grid(1, 1).cols, 2);
    }

    #[test]
    fn raster_layers_add_remove_and_become_active() {
        let mut s = stack();
        let id = s.add_raster_layer();
        assert_eq!(s.active_id(), id);
        assert_eq!(s.layers().len(), 3);
        assert!(s.active_editable_mut().is_some());
        assert!(s.remove_layer(id));
        assert_eq!(s.layers().len(), 2);
        // After deleting the only raster layer, active is a locked base layer again.
        assert!(s.active_editable_mut().is_none());
    }

    #[test]
    fn mark_rasters_persisted_clears_dirty_so_a_later_save_keeps_effects() {
        // Regression: a freshly added/cut raster is `pixels_dirty`. Once it has been flushed to disk,
        // a later flush (e.g. the project-save flush) must treat it as clean — otherwise it rewrites
        // the base PNG and clobbers a non-destructive effects chain the typing tab added in between.
        let mut s = stack();
        let a = s.add_raster_layer();
        assert!(s.layer(a).unwrap().pixels_dirty, "new raster starts dirty");

        s.mark_rasters_persisted();
        assert!(
            !s.layer(a).unwrap().pixels_dirty,
            "raster is clean after its base was persisted"
        );
    }

    #[test]
    fn can_edit_pixels_requires_a_raster_with_no_effects() {
        let mut s = stack();
        // Base layers are never directly editable.
        assert!(!s.layers()[0].can_edit_pixels(), "source");
        assert!(!s.layers()[1].can_edit_pixels(), "clean");

        let id = s.add_raster_layer();
        // A fresh raster has an empty effects chain → directly editable.
        assert!(s.layer(id).unwrap().can_edit_pixels());

        // Giving it a non-empty effects chain blocks direct edits until it is baked.
        s.layer_mut(id).unwrap().effects = vec![serde_json::json!({"type": "shadow"})];
        assert!(!s.layer(id).unwrap().can_edit_pixels());
    }

    #[test]
    fn reorder_rasters_applies_doc_order_and_keeps_base_layers_first() {
        let mut s = stack();
        let a = s.add_raster_layer();
        let b = s.add_raster_layer();
        let c = s.add_raster_layer();
        // Stack: [source, clean, a, b, c]. Reorder rasters to c, a, b (bottom-to-top).
        s.reorder_rasters(&[c, a, b]);
        let ids: Vec<LayerId> = s.layers().iter().map(|l| l.id).collect();
        assert_eq!(ids, vec![0, 1, c, a, b], "base layers stay first; rasters follow doc order");

        // An order omitting a raster appends it on top in id order (defensive).
        s.reorder_rasters(&[b, a]);
        let ids: Vec<LayerId> = s.layers().iter().map(|l| l.id).collect();
        assert_eq!(ids[0..2], [0, 1], "base layers still first");
        assert_eq!(ids[2], b);
        assert_eq!(ids[3], a);
        assert_eq!(ids[4], c, "omitted raster appended on top");
    }

    #[test]
    fn groups_assign_resolve_and_ungroup_on_remove() {
        let mut s = stack();
        let a = s.add_raster_layer();
        let g = s.add_group("G".to_string());
        s.set_layer_group(a, Some(g));
        // Base layers can never be grouped.
        s.set_layer_group(0, Some(g));
        assert_eq!(s.layer(0).unwrap().group, None);
        assert_eq!(s.layer(a).unwrap().group, Some(g));

        // Group opacity multiplies the layer opacity; group visibility gates it.
        s.group_mut(g).unwrap().opacity = 0.5;
        {
            let la = s.layer(a).unwrap();
            assert!((s.layer_opacity(la) - 0.5).abs() < 1e-6);
            assert!(s.layer_visible(la));
        }
        s.group_mut(g).unwrap().visible = false;
        {
            let la = s.layer(a).unwrap();
            assert!(!s.layer_visible(la));
        }

        // Removing the group ungroups its members without touching them.
        s.remove_group(g);
        assert_eq!(s.layer(a).unwrap().group, None);
        assert!(s.group(g).is_none());
    }
}
