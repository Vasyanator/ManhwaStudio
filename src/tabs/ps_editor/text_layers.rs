/*
File: tabs/ps_editor/text_layers.rs

Purpose:
Read-only display of the typing tab's text/image overlays inside the PS editor, as "text layers".
The overlays are nodes in the shared `layers.json` (kind = text), with their pixels + geometry in
`text_info.json`. This module loads those nodes, resolves each payload, and renders the overlay
image at its page-space placement through the PS viewport.

Why separate from `LayerStack`:
These layers are owned by the typing tab; here they are display-only. Keeping them out of
`LayerStack` means the raster invariants and tools (paint/cut/merge/reorder/stash) are untouched.
Moving/transforming them from the PS side (which must write geometry back to `text_info.json` and
signal the typing tab to reload) is a later step.
*/

use super::layers::LayerTransform;
use super::viewport::ViewTransform;
use crate::models::layer_model::manifest::DeformRec;
use crate::models::layer_model::persist;
use eframe::egui::{self, Color32, ColorImage, Pos2, Vec2};
use std::path::Path;

/// A typing-tab overlay shown read-only in the PS editor.
pub struct PsTextLayer {
    pub uid: String,
    pub name: String,
    pub visible: bool,
    /// Text group (Группа текста N) this overlay belongs to (typing tab's axis).
    pub layer_idx: u32,
    /// Unified PS-tree group membership (the `GroupRec.uid` it belongs to), independent of
    /// `layer_idx`. `None` when the overlay is not in any PS group.
    pub group_uid: Option<String>,
    /// Pinned overlays render as their own band at an explicit Z (not auto page-Y ordered).
    pub pinned: bool,
    /// The pin was set implicitly by PS grouping (vs. an explicit user pin).
    pub pinned_by_group: bool,
    /// Raw text string of the overlay (from the node's `render_data.text_params.text`), used to build
    /// a row preview label in the PS layers panel. Empty when the node carries no text.
    pub text_content: String,
    image: ColorImage,
    transform: LayerTransform,
    /// Perspective/bend deformation grid in page pixels (`cols`×`rows` control points), when the
    /// overlay was deformed in the typing tab. Absolute page positions — when present they drive
    /// rendering instead of the affine `transform`. Canonical `DeformRec` (shared with the layer
    /// model and the typing tab).
    deform: Option<DeformRec>,
    texture: Option<egui::TextureHandle>,
}

impl PsTextLayer {
    /// Builds a `PsTextLayer` directly from a `LayerDoc` Text node's projected fields. Pin / text-group
    /// (`layer_idx`) metadata is not carried by the doc node, so the caller supplies it (preserved from
    /// the prior projection or derived from the bands); `texture` is supplied so the GPU handle can be
    /// reused across re-projections when the node's pixels are unchanged. `text_content` is the overlay's
    /// raw text (from the node's `render_data`), used to build the panel row preview; empty when absent.
    #[allow(clippy::too_many_arguments)]
    pub fn from_doc_node(
        uid: String,
        name: String,
        visible: bool,
        layer_idx: u32,
        group_uid: Option<String>,
        pinned: bool,
        pinned_by_group: bool,
        text_content: String,
        image: ColorImage,
        transform: LayerTransform,
        deform: Option<DeformRec>,
        texture: Option<egui::TextureHandle>,
    ) -> Self {
        Self {
            uid,
            name,
            visible,
            layer_idx,
            group_uid,
            pinned,
            pinned_by_group,
            text_content,
            image,
            transform,
            deform,
            texture,
        }
    }

    /// Builds a skeletal `PsTextLayer` carrying only the PS-owned metadata read from a `layers.json`
    /// text node (pin / pinned_by_group / text-group `layer_idx` / unified `group_uid`). The image and
    /// geometry are placeholders, filled by the subsequent `sync_view_from_doc` projection from the doc
    /// (the source of truth). Used so PS gets pin metadata on page-load WITHOUT reading `text_info.json`.
    /// `text_content` starts empty (render_data is not available on this skeletal path); the projection
    /// fills it from the doc node.
    pub fn meta_from_node(
        uid: String,
        name: String,
        layer_idx: u32,
        group_uid: Option<String>,
        pinned: bool,
        pinned_by_group: bool,
    ) -> Self {
        Self {
            uid,
            name,
            visible: true,
            layer_idx,
            group_uid,
            pinned,
            pinned_by_group,
            text_content: String::new(),
            image: ColorImage::filled([1, 1], Color32::TRANSPARENT),
            transform: LayerTransform {
                center: Vec2::ZERO,
                rotation: 0.0,
                scale: 1.0,
            },
            deform: None,
            texture: None,
        }
    }

    /// The overlay's stable uid (durable identity across reloads/projections).
    pub fn uid(&self) -> &str {
        &self.uid
    }

    /// Takes the GPU texture handle out (used to carry it across a re-projection by uid).
    pub fn take_texture(&mut self) -> Option<egui::TextureHandle> {
        self.texture.take()
    }

    /// Center of the overlay in page (world) pixels.
    pub fn center(&self) -> Vec2 {
        self.transform.center
    }

    /// Translates the overlay by `delta` page pixels (used for drag-to-move in the PS editor).
    pub fn translate(&mut self, delta: Vec2) {
        self.transform.center += delta;
    }

    /// True when the overlay carries a deform grid (perspective/bend). Such overlays are positioned
    /// by absolute mesh points, so PS affine drag/rotate/scale doesn't apply — edit them in typing.
    pub fn has_deform(&self) -> bool {
        self.deform.is_some()
    }

    /// Rotation in radians.
    pub fn rotation(&self) -> f32 {
        self.transform.rotation
    }

    /// Uniform scale factor.
    pub fn scale(&self) -> f32 {
        self.transform.scale
    }

    /// Rotates the overlay about its center by `delta` radians.
    pub fn rotate_by(&mut self, delta: f32) {
        self.transform.rotation += delta;
    }

    /// Multiplies the overlay's scale by `factor` (clamped to a small minimum).
    pub fn scale_by(&mut self, factor: f32) {
        self.transform.scale = (self.transform.scale * factor).max(0.02);
    }

    /// The overlay's placement, for rasterizing it into an owned raster layer.
    pub fn transform(&self) -> LayerTransform {
        self.transform
    }

    /// The overlay's rendered pixels, for rasterizing.
    pub fn image(&self) -> &ColorImage {
        &self.image
    }

    /// True when page-space point `world` falls inside the overlay's (transformed) image rect.
    pub fn contains_world(&self, world: Vec2) -> bool {
        let size = Vec2::new(self.image.size[0] as f32, self.image.size[1] as f32);
        let local_center = size * 0.5;
        let scale = if self.transform.scale.abs() < f32::EPSILON {
            f32::EPSILON
        } else {
            self.transform.scale
        };
        let (sin, cos) = (-self.transform.rotation).sin_cos();
        let d = world - self.transform.center;
        let unrotated = Vec2::new(d.x * cos - d.y * sin, d.x * sin + d.y * cos) / scale;
        let local = local_center + unrotated;
        local.x >= 0.0 && local.y >= 0.0 && local.x < size.x && local.y < size.y
    }

    /// The image's four corners in page (world) pixels, through the layer transform.
    fn world_corners(&self) -> [Pos2; 4] {
        let size = Vec2::new(self.image.size[0] as f32, self.image.size[1] as f32);
        let local_center = size * 0.5;
        let (sin, cos) = self.transform.rotation.sin_cos();
        let place = |lx: f32, ly: f32| {
            let v = (Vec2::new(lx, ly) - local_center) * self.transform.scale;
            let rotated = Vec2::new(v.x * cos - v.y * sin, v.x * sin + v.y * cos);
            (self.transform.center + rotated).to_pos2()
        };
        [
            place(0.0, 0.0),
            place(size.x, 0.0),
            place(size.x, size.y),
            place(0.0, size.y),
        ]
    }

    /// The overlay's page-space outline, used to build a canvas selection ("выделить слой полностью")
    /// for a text layer. For a deformed overlay (perspective/bend) it returns the axis-aligned bbox over
    /// the deform control points as a 4-point rectangle; otherwise the (possibly rotated/scaled) affine
    /// image quad. Empty deform points fall back to the affine quad.
    pub fn footprint_polygon(&self) -> Vec<(f32, f32)> {
        if let Some(grid) = &self.deform
            && !grid.points_px.is_empty()
        {
            let mut min_x = f32::INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for p in &grid.points_px {
                min_x = min_x.min(p[0]);
                min_y = min_y.min(p[1]);
                max_x = max_x.max(p[0]);
                max_y = max_y.max(p[1]);
            }
            return vec![
                (min_x, min_y),
                (max_x, min_y),
                (max_x, max_y),
                (min_x, max_y),
            ];
        }
        self.world_corners().iter().map(|p| (p.x, p.y)).collect()
    }

    /// Draws the overlay through the viewport, lazily uploading its texture. When a deform grid is
    /// present (perspective/bend from the typing tab), it renders the textured `cols`×`rows` mesh;
    /// otherwise a simple affine quad. `opacity` (0..=1, from the unified group fold) modulates the
    /// whole overlay via the mesh vertex tint.
    pub fn draw(
        &mut self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        view: &ViewTransform,
        opacity: f32,
    ) {
        if !self.visible {
            return;
        }
        let tint = Color32::from_white_alpha((opacity.clamp(0.0, 1.0) * 255.0).round() as u8);
        let image = self.image.clone();
        let uid = self.uid.clone();
        let texture_id = self
            .texture
            .get_or_insert_with(|| {
                ctx.load_texture(format!("ps_text_{uid}"), image, egui::TextureOptions::LINEAR)
            })
            .id();

        let mut mesh = egui::Mesh::with_texture(texture_id);
        if let Some(grid) = &self.deform {
            // Deform grid: control points are absolute page pixels; map each to screen and stitch
            // the cols×rows grid into triangles with bilinear UVs.
            let (cols, rows) = (grid.cols, grid.rows);
            for row in 0..rows {
                let t = row as f32 / (rows - 1) as f32;
                for col in 0..cols {
                    let s = col as f32 / (cols - 1) as f32;
                    let p = grid.points_px[row * cols + col];
                    mesh.vertices.push(egui::epaint::Vertex {
                        pos: view.world_to_screen(Pos2::new(p[0], p[1])),
                        uv: Pos2::new(s, t),
                        color: tint,
                    });
                }
            }
            for row in 0..(rows - 1) {
                for col in 0..(cols - 1) {
                    let i0 = (row * cols + col) as u32;
                    let i2 = ((row + 1) * cols + col) as u32;
                    mesh.add_triangle(i0, i0 + 1, i2);
                    mesh.add_triangle(i2, i0 + 1, i2 + 1);
                }
            }
        } else {
            let corners = self.world_corners();
            let uv = [
                Pos2::new(0.0, 0.0),
                Pos2::new(1.0, 0.0),
                Pos2::new(1.0, 1.0),
                Pos2::new(0.0, 1.0),
            ];
            for i in 0..4 {
                mesh.vertices.push(egui::epaint::Vertex {
                    pos: view.world_to_screen(corners[i]),
                    uv: uv[i],
                    color: Color32::WHITE,
                });
            }
            mesh.indices = vec![0, 1, 2, 0, 2, 3];
        }
        painter.add(egui::Shape::mesh(mesh));
    }
}

/// Loads the typing tab's overlays to display for one page, driven by `text_info.json` (the actual
/// overlays) so text shows even before any `layers.json` text nodes exist (fresh open / legacy
/// chapter). The PS-owned pin state is taken from the `layers.json` nodes when present; band Z /
/// group come from the unified bands at composite time. Directories are tried in order (unsaved
/// staging, committed `layers/`, legacy `text_images/`) for both the JSON and the PNGs.
/// Loads PS-owned text-node METADATA for a page from `layers.json` ONLY (no `text_info.json`): one
/// skeletal `PsTextLayer` per text node carrying pin / pinned_by_group / text-group `layer_idx` /
/// unified `group_uid`. The image + geometry are placeholders that `sync_view_from_doc` fills from the
/// shared doc (the source of truth for text now that the typing tab no longer writes `text_info.json`).
/// Returns an empty list for a chapter whose text still lives only in legacy `text_info.json` (no
/// layers.json text nodes yet) — those nodes are materialized by the doc projection with default pins.
pub fn load_page_text_layer_meta(
    unsaved_layers_dir: &Path,
    layers_dir: &Path,
    page_idx: usize,
) -> Vec<PsTextLayer> {
    persist::load_page_text_nodes(unsaved_layers_dir, Some(layers_dir), page_idx)
        .unwrap_or_default()
        .into_iter()
        .map(|n| {
            PsTextLayer::meta_from_node(
                n.uid,
                n.name,
                n.layer_idx,
                n.group_uid,
                n.pinned,
                n.pinned_by_group,
            )
        })
        .collect()
}


// NOTE: PS no longer reads or writes `text_info.json`. Text layers are projected from the shared
// `LayerDoc` (`sync_view_from_doc`), with PS-owned pin/group metadata seeded from `layers.json` text
// nodes via `load_page_text_layer_meta`. The former `text_info.json` readers/writers
// (`read_text_info_array` / `load_png` / `persist_overlay_transform` / `delete_overlay`) were removed
// across Phases A4–A5 and D. The doc's `decode_page_payload` is the only reader of the legacy file, and
// only for un-migrated chapters.

