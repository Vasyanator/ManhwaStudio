/*
File: tabs/ps_editor/layer_render.rs

Purpose:
GPU-side tiled texture cache for one editor layer. A page-sized RGBA buffer is split into fixed
tiles so large manhwa pages never exceed the GPU max texture size, and brush strokes only re-upload
the few tiles they touched.

Key structures:
- `TiledTexture`: per-layer tile grid with per-tile dirty flags and lazily uploaded handles.

Notes:
Tiles are uploaded with a per-frame budget so the initial upload of a tall page is spread across
frames and never stalls the GUI thread. Drawing maps each tile's image-space rect through the
`ViewTransform` and tints by the layer opacity.
*/

use super::layers::Layer;
use super::tools::DirtyRect;
use super::viewport::ViewTransform;
use eframe::egui;
use egui::epaint::Vertex;
use egui::{Color32, ColorImage, Mesh, Pos2, Shape, TextureHandle, TextureOptions, Vec2};

/// Tile side in image pixels. Kept well under common GPU limits.
const TILE_SIDE: usize = 1024;

/// Linear interpolation between `a` and `b` by `t` (used to walk a tile's UV sub-range).
fn uu_lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Tiled GPU cache for a single page-sized layer image.
pub struct TiledTexture {
    size: [usize; 2],
    cols: usize,
    rows: usize,
    textures: Vec<Option<TextureHandle>>,
    dirty: Vec<bool>,
    name: String,
}

impl TiledTexture {
    /// Creates a tile grid for a `size` (image px) layer with all tiles pending upload.
    #[must_use]
    pub fn new(size: [usize; 2], name: impl Into<String>) -> Self {
        let cols = size[0].div_ceil(TILE_SIDE).max(1);
        let rows = size[1].div_ceil(TILE_SIDE).max(1);
        let count = cols * rows;
        Self {
            size,
            cols,
            rows,
            textures: (0..count).map(|_| None).collect(),
            dirty: vec![true; count],
            name: name.into(),
        }
    }

    /// True when this tile grid was built for `size` (image px).
    #[must_use]
    pub fn matches_size(&self, size: [usize; 2]) -> bool {
        self.size == size
    }

    /// Marks every tile overlapping `rect` (inclusive image px) for re-upload.
    pub fn mark_dirty_rect(&mut self, rect: DirtyRect) {
        crate::trace_log!(
            crate::trace::cat::RENDER,
            "tile mark_dirty_rect layer={} rect=[{},{},{},{}]",
            self.name,
            rect.min_x,
            rect.min_y,
            rect.max_x,
            rect.max_y
        );
        let col0 = rect.min_x / TILE_SIDE;
        let col1 = (rect.max_x / TILE_SIDE).min(self.cols.saturating_sub(1));
        let row0 = rect.min_y / TILE_SIDE;
        let row1 = (rect.max_y / TILE_SIDE).min(self.rows.saturating_sub(1));
        for row in row0..=row1 {
            for col in col0..=col1 {
                self.dirty[row * self.cols + col] = true;
            }
        }
    }

    /// Marks every tile for re-upload (used after a non-axis-aligned edit like a cut).
    pub fn mark_all_dirty(&mut self) {
        crate::trace_log!(
            crate::trace::cat::RENDER,
            "tile mark_all_dirty layer={} tiles={}",
            self.name,
            self.dirty.len()
        );
        self.dirty.iter_mut().for_each(|d| *d = true);
    }

    /// Uploads up to `budget` dirty tiles from `image`, returning how many were uploaded.
    ///
    /// `image` must have dimensions equal to the layer size. Returns `0` when nothing is dirty so
    /// the caller can tell when a layer is fully resident.
    pub fn upload_budgeted(
        &mut self,
        ctx: &egui::Context,
        image: &ColorImage,
        budget: usize,
    ) -> usize {
        if image.size != self.size {
            return 0;
        }
        let mut uploaded = 0;
        for index in 0..self.textures.len() {
            if uploaded >= budget {
                break;
            }
            if !self.dirty[index] {
                continue;
            }
            let col = index % self.cols;
            let row = index / self.cols;
            let tile = self.crop_tile(image, col, row);
            match &mut self.textures[index] {
                Some(handle) => handle.set(tile, TextureOptions::LINEAR),
                slot @ None => {
                    *slot = Some(ctx.load_texture(
                        format!("{}_{col}_{row}", self.name),
                        tile,
                        TextureOptions::LINEAR,
                    ));
                }
            }
            self.dirty[index] = false;
            uploaded += 1;
        }
        // Only emit when tiles were actually uploaded this frame (skip the common 0-upload idle case).
        if uploaded > 0 {
            crate::trace_log!(
                crate::trace::cat::RENDER,
                "tile upload layer={} count={} budget={}",
                self.name,
                uploaded,
                budget
            );
        }
        uploaded
    }

    /// Draws all resident tiles through the layer's transform and `view`, tinted by `opacity`.
    ///
    /// Each tile is mapped layer-local → page (via `layer`'s transform) → screen and drawn as a
    /// textured quad mesh, so rotated/scaled layers render correctly. An axis-aligned (identity)
    /// layer produces the same result as a plain image blit.
    pub fn draw(&self, painter: &egui::Painter, view: &ViewTransform, opacity: f32, layer: &Layer) {
        let alpha = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
        let tint = Color32::from_white_alpha(alpha);
        for (index, slot) in self.textures.iter().enumerate() {
            let Some(handle) = slot else {
                continue;
            };
            let col = index % self.cols;
            let row = index / self.cols;
            let x0 = (col * TILE_SIDE) as f32;
            let y0 = (row * TILE_SIDE) as f32;
            let tw = (self.size[0] - col * TILE_SIDE).min(TILE_SIDE) as f32;
            let th = (self.size[1] - row * TILE_SIDE).min(TILE_SIDE) as f32;
            // Tile corners in layer-local px, mapped to screen through the layer transform + view.
            let corners = [
                Vec2::new(x0, y0),
                Vec2::new(x0 + tw, y0),
                Vec2::new(x0 + tw, y0 + th),
                Vec2::new(x0, y0 + th),
            ]
            .map(|local| view.world_to_screen(layer.local_to_world(local).to_pos2()));
            let uvs = [
                Pos2::new(0.0, 0.0),
                Pos2::new(1.0, 0.0),
                Pos2::new(1.0, 1.0),
                Pos2::new(0.0, 1.0),
            ];
            let mut mesh = Mesh::with_texture(handle.id());
            for (pos, uv) in corners.into_iter().zip(uvs) {
                mesh.vertices.push(Vertex {
                    pos,
                    uv,
                    color: tint,
                });
            }
            mesh.add_triangle(0, 1, 2);
            mesh.add_triangle(0, 2, 3);
            painter.add(Shape::mesh(mesh));
        }
    }

    /// Draws the layer warped through its mesh-deform grid (`layer.deform`), tinted by `opacity`.
    ///
    /// The deform grid's control points are absolute page px (row-major, cols×rows), and its UV
    /// spans the whole layer image `[0,1]×[0,1]` — identical to the deformed-text path in
    /// `PsTextLayer::draw`. Because the layer is tiled (each tile holds only a UV sub-rectangle of
    /// the image), each tile is rendered as its own sub-mesh: the tile's UV corners are mapped
    /// through the deform grid (bilinear sample of the control points) to page px, then to screen.
    /// Interior grid lines that cross the tile are inserted so the warp follows the mesh smoothly
    /// rather than only at tile corners. A single-tile layer (the common case) reproduces the text
    /// deform rendering exactly. When the grid is degenerate this falls back to the affine `draw`.
    pub fn draw_deform(
        &self,
        painter: &egui::Painter,
        view: &ViewTransform,
        opacity: f32,
        layer: &Layer,
    ) {
        let Some(grid) = layer.deform.as_ref() else {
            self.draw(painter, view, opacity, layer);
            return;
        };
        let (gc, gr) = (grid.cols, grid.rows);
        if gc < 2 || gr < 2 || grid.points_px.len() != gc * gr {
            self.draw(painter, view, opacity, layer);
            return;
        }
        if self.size[0] == 0 || self.size[1] == 0 {
            return;
        }
        let tint = Color32::from_white_alpha((opacity.clamp(0.0, 1.0) * 255.0).round() as u8);
        // Bilinear sample of the deform grid at full-image UV (u, v) in [0,1] -> page px.
        let sample = |u: f32, v: f32| -> Pos2 {
            let fu = (u.clamp(0.0, 1.0)) * (gc - 1) as f32;
            let fv = (v.clamp(0.0, 1.0)) * (gr - 1) as f32;
            let c0 = (fu.floor() as usize).min(gc - 2);
            let r0 = (fv.floor() as usize).min(gr - 2);
            let tu = fu - c0 as f32;
            let tv = fv - r0 as f32;
            let p = |c: usize, r: usize| {
                let q = grid.points_px[r * gc + c];
                Vec2::new(q[0], q[1])
            };
            let top = p(c0, r0) * (1.0 - tu) + p(c0 + 1, r0) * tu;
            let bot = p(c0, r0 + 1) * (1.0 - tu) + p(c0 + 1, r0 + 1) * tu;
            (top * (1.0 - tv) + bot * tv).to_pos2()
        };
        for (index, slot) in self.textures.iter().enumerate() {
            let Some(handle) = slot else {
                continue;
            };
            let col = index % self.cols;
            let row = index / self.cols;
            let x0 = (col * TILE_SIDE) as f32;
            let y0 = (row * TILE_SIDE) as f32;
            let tw = (self.size[0] - col * TILE_SIDE).min(TILE_SIDE) as f32;
            let th = (self.size[1] - row * TILE_SIDE).min(TILE_SIDE) as f32;
            // This tile covers full-image UV [uu0, uu1] x [vv0, vv1].
            let uu0 = x0 / self.size[0] as f32;
            let uu1 = (x0 + tw) / self.size[0] as f32;
            let vv0 = y0 / self.size[1] as f32;
            let vv1 = (y0 + th) / self.size[1] as f32;
            // Subdivide the tile to follow the deform grid: at least the grid resolution, clamped
            // so a single tile spanning the whole image uses the full cols x rows.
            let sub_c = ((gc as f32 * (uu1 - uu0)).ceil() as usize).max(1) + 1;
            let sub_r = ((gr as f32 * (vv1 - vv0)).ceil() as usize).max(1) + 1;
            let mut mesh = Mesh::with_texture(handle.id());
            for ir in 0..sub_r {
                let fv = ir as f32 / (sub_r - 1) as f32;
                let uv = uu_lerp(vv0, vv1, fv);
                for ic in 0..sub_c {
                    let fu = ic as f32 / (sub_c - 1) as f32;
                    let uu = uu_lerp(uu0, uu1, fu);
                    mesh.vertices.push(Vertex {
                        // Tile-local UV: the tile texture itself spans [0,1] over its own region.
                        pos: view.world_to_screen(sample(uu, uv)),
                        uv: Pos2::new(fu, fv),
                        color: tint,
                    });
                }
            }
            for ir in 0..(sub_r - 1) {
                for ic in 0..(sub_c - 1) {
                    let i0 = (ir * sub_c + ic) as u32;
                    let i2 = ((ir + 1) * sub_c + ic) as u32;
                    mesh.add_triangle(i0, i0 + 1, i2);
                    mesh.add_triangle(i2, i0 + 1, i2 + 1);
                }
            }
            painter.add(Shape::mesh(mesh));
        }
    }

    /// Copies a single tile region out of the full-page image.
    fn crop_tile(&self, image: &ColorImage, col: usize, row: usize) -> ColorImage {
        let x0 = col * TILE_SIDE;
        let y0 = row * TILE_SIDE;
        let tw = (self.size[0] - x0).min(TILE_SIDE);
        let th = (self.size[1] - y0).min(TILE_SIDE);
        let mut pixels = Vec::with_capacity(tw * th);
        for y in 0..th {
            let src_row = (y0 + y) * self.size[0] + x0;
            pixels.extend_from_slice(&image.pixels[src_row..src_row + tw]);
        }
        ColorImage::new([tw, th], pixels)
    }
}
