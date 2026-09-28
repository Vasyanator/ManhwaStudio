/*
File: tabs/ps_editor/layer_render.rs

Purpose:
GPU-side painting helpers for the PS editor canvas: the per-layer tiled texture cache, and the
transparency checkerboard painted under the page. A page-sized RGBA buffer is split into fixed
tiles so large manhwa pages never exceed the GPU max texture size, and brush strokes only re-upload
the few tiles they touched.

Key structures:
- `TiledTexture`: per-layer tile grid with per-tile dirty flags and lazily uploaded handles.

Key functions:
- `draw_page_checkerboard`: transparency checkerboard covering the page rect, drawn beneath every
  layer so a transparent hole reads as "no pixels" instead of as dark canvas ground.

Notes:
Each grid carries the `TextureOptions` its tiles are uploaded with, driven by the «Сглаживание»
checkbox of the «PS редактор» tab. Flipping it re-uploads (`set_options` -> `mark_all_dirty`)
instead of keeping a second handle per tile: this cache is not registered with `memory_manager`,
so dual handles would be untracked, unevictable GPU memory.
Tiles are uploaded with a per-frame budget so the initial upload of a tall page is spread across
frames and never stalls the GUI thread. Drawing maps each tile's image-space rect through the
`ViewTransform` and tints by the layer opacity. The checkerboard is the studio's
`ms_theme::checkerboard::CANVAS` preset, painted as ONE textured quad, never a per-cell loop: a page
can be ~800x19000 px and a loop would emit thousands of shapes every frame.
*/

use super::layers::Layer;
use super::tools::DirtyRect;
use super::viewport::ViewTransform;
use eframe::egui;
use egui::epaint::Vertex;
use egui::{Color32, ColorImage, Mesh, Pos2, Rect, Shape, TextureHandle, TextureOptions, Vec2};

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
    /// Sampling mode every tile of this grid is uploaded with. See [`TiledTexture::set_options`]
    /// for why the mode lives here instead of at draw time.
    options: TextureOptions,
}

impl TiledTexture {
    /// Creates a tile grid for a `size` (image px) layer with all tiles pending upload.
    ///
    /// Tiles start in the smoothed (`TextureOptions::LINEAR`) sampling mode, which is what the PS
    /// editor's «Сглаживание» checkbox defaults to; [`TiledTexture::set_options`] switches it.
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
            options: TextureOptions::LINEAR,
        }
    }

    /// Switches the sampling mode every tile is uploaded with. Returns `true` when the mode
    /// actually changed, in which case all tiles were marked dirty for re-upload.
    ///
    /// # Why a re-upload and not a second `TextureHandle`
    /// In egui 0.35 filtering is a property of the `TextureId`, not of the mesh
    /// (`TextureOptions` travels only with an `ImageDelta`), so one handle cannot be drawn both
    /// smoothed and un-smoothed — a switch costs either a second handle per tile or a re-upload.
    /// The canvas affords dual handles (`app.rs::TextureTile`) because its source-page cache
    /// retains the tile bytes AND is registered with `memory_manager` as an evictable
    /// `CacheResourceKind`. The PS editor's `render_cache` is neither: a dual-handle grid would be
    /// untracked, unevictable GPU memory — roughly +122 MB for the two base layers of a tall
    /// webtoon page. Toggling is rare, so the cheap steady state wins over the cheap transition;
    /// the existing `upload_budgeted` sweep re-uploads over a few frames.
    ///
    /// Marking dirty only on a real change is load-bearing: an unconditional `mark_all_dirty` would
    /// re-upload the whole page every frame, since this is called once per frame per layer.
    pub fn set_options(&mut self, options: TextureOptions) -> bool {
        if self.options == options {
            return false;
        }
        self.options = options;
        self.mark_all_dirty();
        true
    }

    /// True when this tile grid was built for `size` (image px).
    #[must_use]
    pub fn matches_size(&self, size: [usize; 2]) -> bool {
        self.size == size
    }

    /// Marks every tile overlapping `rect` (inclusive image px) for re-upload.
    pub fn mark_dirty_rect(&mut self, rect: DirtyRect) {
        ms_log::trace_log!(
            ms_log::trace::cat::RENDER,
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
        ms_log::trace_log!(
            ms_log::trace::cat::RENDER,
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
                Some(handle) => handle.set(tile, self.options),
                slot @ None => {
                    *slot = Some(ctx.load_texture(
                        format!("{}_{col}_{row}", self.name),
                        tile,
                        self.options,
                    ));
                }
            }
            self.dirty[index] = false;
            uploaded += 1;
        }
        // Only emit when tiles were actually uploaded this frame (skip the common 0-upload idle case).
        if uploaded > 0 {
            ms_log::trace_log!(
                ms_log::trace::cat::RENDER,
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

/// Screen-space rect of the whole page, or `Rect::ZERO` when `page_size` (image px) has a zero side.
fn page_rect_on_screen(view: &ViewTransform, page_size: [usize; 2]) -> Rect {
    if page_size[0] == 0 || page_size[1] == 0 {
        return Rect::ZERO;
    }
    // `as f32` is exact here: a page dimension comes from a decoded image and stays far below
    // f32's 2^24 exact-integer limit, and the whole viewport transform is f32 anyway.
    let size = Vec2::new(page_size[0] as f32, page_size[1] as f32);
    view.world_rect_to_screen(Rect::from_min_size(Pos2::ZERO, size))
}

/// Paints the transparency checkerboard over the page rectangle, under every layer.
///
/// `page_size` is the page in IMAGE pixels (`LayerStack::size`); `view` maps it to screen, so the
/// board covers exactly the page and nothing of the surrounding canvas ground — no clip rect is
/// needed. The board is the studio's `ms_theme::checkerboard::CANVAS` preset, anchored at the
/// page's top-left corner: squares are SCREEN-constant and do not zoom with the page, and the whole
/// board is one textured quad. Paints nothing for a zero-sized page or a degenerate/non-finite
/// screen rect, and never panics. Call it before drawing the base layers, on the same painter.
pub(super) fn draw_page_checkerboard(painter: &egui::Painter, view: &ViewTransform, page_size: [usize; 2]) {
    let rect = page_rect_on_screen(view, page_size);
    ms_theme::checkerboard::CANVAS.paint(painter, rect, egui::CornerRadius::ZERO);
}

#[cfg(test)]
mod tests {
    use super::{TiledTexture, ViewTransform, draw_page_checkerboard, page_rect_on_screen};
    use eframe::egui;
    use egui::{ColorImage, Pos2, Rect, TextureOptions, Vec2};

    /// A view whose viewport is 400x400 points, with the page origin at its centre offset.
    fn view(zoom: f32) -> ViewTransform {
        ViewTransform {
            viewport_rect: Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 400.0)),
            zoom,
            center_world: Vec2::ZERO,
        }
    }

    #[test]
    fn page_rect_is_empty_for_a_zero_sized_page() {
        for size in [[0, 100], [100, 0], [0, 0]] {
            assert_eq!(page_rect_on_screen(&view(1.0), size), Rect::ZERO);
        }
    }

    #[test]
    fn page_rect_scales_with_zoom() {
        // The CANVAS board is painted at one texel per SCREEN point over this rect, so a page twice
        // as large on screen shows twice as many squares, i.e. an unchanged square size.
        let width = |zoom: f32| page_rect_on_screen(&view(zoom), [100, 100]).width();
        assert!((width(2.0) - width(1.0) * 2.0).abs() < 1e-3);
        assert!((width(0.5) - width(1.0) * 0.5).abs() < 1e-3);
    }

    #[test]
    fn page_checkerboard_paints_one_quad_and_nothing_for_a_degenerate_page() {
        let ctx = egui::Context::default();
        let empty = ctx.run_ui(egui::RawInput::default(), |ui| {
            draw_page_checkerboard(ui.painter(), &view(1.0), [0, 0]);
        });
        let painted = ctx.run_ui(egui::RawInput::default(), |ui| {
            draw_page_checkerboard(ui.painter(), &view(1.0), [100, 200]);
        });
        let (empty_len, painted_len) = (empty.shapes.len(), painted.shapes.len());
        // Headless: no renderer consumes the font-atlas upload, and egui 0.36 panics when a
        // `TexturesDelta` is dropped unapplied — discard the deltas before any assert can unwind.
        empty.drop_without_applying_deltas();
        painted.drop_without_applying_deltas();
        assert_eq!(
            painted_len,
            empty_len + 1,
            "a real page adds exactly one textured quad; a zero-sized page adds none"
        );
    }

    /// `set_options` must mark tiles dirty ONLY on a real change: it is called once per frame per
    /// layer, so an unconditional invalidation would re-upload the whole page every frame.
    #[test]
    fn set_options_marks_dirty_only_on_a_real_change() {
        let ctx = egui::Context::default();
        let size = [2048, 2048];
        let image = ColorImage::filled(size, egui::Color32::WHITE);
        let mut cache = TiledTexture::new(size, "test_layer");
        // Four tiles at TILE_SIDE = 1024; the budget is large enough to make the grid fully resident.
        assert_eq!(cache.upload_budgeted(&ctx, &image, 16), 4);
        assert_eq!(cache.upload_budgeted(&ctx, &image, 16), 0, "nothing left dirty");

        assert!(!cache.set_options(TextureOptions::LINEAR), "already LINEAR");
        assert_eq!(cache.upload_budgeted(&ctx, &image, 16), 0, "a no-op must not re-upload");

        assert!(cache.set_options(TextureOptions::NEAREST), "a real change");
        assert_eq!(cache.upload_budgeted(&ctx, &image, 16), 4, "every tile re-uploads");
        assert_eq!(cache.upload_budgeted(&ctx, &image, 16), 0, "and then settles again");
    }
}
