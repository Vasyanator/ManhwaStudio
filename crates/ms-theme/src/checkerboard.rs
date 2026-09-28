/*
File: crates/ms-theme/src/checkerboard.rs

Purpose:
The studio's transparency checkerboards: their palettes, cell sizes, a per-pixel colour rule for
code that composites into pixel buffers, and an egui painter.

Key structures:
- Checkerboard (palette + cell size; only the named presets exist)

Key constants:
- CANVAS: the board under transparent page content (ps-editor page, layer previews)
- INK_PREVIEW_DARK / INK_PREVIEW_LIGHT: boards behind rendered-text previews, picked by the ink's
  luminance (light ink -> dark board), plus their 1 px border colours

Key functions:
- paint() (CANVAS shorthand), Checkerboard::paint(), Checkerboard::color_at(),
  Checkerboard::tile_image()

Notes:
Painting is ONE textured rect per call: a 2x2-cell tile is uploaded once per preset per
`Context` (cached in the context's temp data) and repeated through `NEAREST_REPEAT` UVs, so the
cost never depends on the painted area. Cells are one texel per point, anchored at the painted
rect's top-left corner, and do not scale with any zoom.
*/

use egui::{Color32, ColorImage, CornerRadius, Painter, Pos2, Rect, TextureHandle, TextureOptions, Vec2};

/// A two-colour transparency checkerboard: the colour of the cell at the origin, the alternating
/// colour, and the cell side.
///
/// Only the named presets exist ([`CANVAS`], [`INK_PREVIEW_DARK`], [`INK_PREVIEW_LIGHT`]), so a
/// board is always one the studio has chosen and the cell side is never zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Checkerboard {
    origin: Color32,
    alternate: Color32,
    cell_px: u16,
}

/// Board under transparent page content (the ps-editor page, layer thumbnails).
///
/// Both greys sit far from black ink and white paper, so a transparent hole over the dark canvas
/// ground never reads as an ink blob.
pub const CANVAS: Checkerboard = Checkerboard { origin: Color32::from_rgb(84, 84, 88), alternate: Color32::from_rgb(64, 64, 68), cell_px: 10 };

/// Board behind a rendered-text preview whose ink is LIGHT (luminance >= 140 over white).
pub const INK_PREVIEW_DARK: Checkerboard = Checkerboard { origin: Color32::from_gray(64), alternate: Color32::from_gray(88), cell_px: 14 };

/// Board behind a rendered-text preview whose ink is DARK.
pub const INK_PREVIEW_LIGHT: Checkerboard = Checkerboard { origin: Color32::from_gray(232), alternate: Color32::from_gray(198), cell_px: 14 };

/// 1 px border drawn around an [`INK_PREVIEW_DARK`] tile.
pub const INK_PREVIEW_DARK_BORDER: Color32 = Color32::from_gray(115);

/// 1 px border drawn around an [`INK_PREVIEW_LIGHT`] tile.
pub const INK_PREVIEW_LIGHT_BORDER: Color32 = Color32::from_gray(150);

impl Checkerboard {
    /// Colour of the cell at the board's origin (top-left).
    #[must_use]
    pub const fn origin(self) -> Color32 {
        self.origin
    }

    /// Colour of the cells adjacent to the origin cell.
    #[must_use]
    pub const fn alternate(self) -> Color32 {
        self.alternate
    }

    /// Side of one cell, in pixels (or points, when painted on screen). Never zero.
    #[must_use]
    pub const fn cell_px(self) -> u16 {
        self.cell_px
    }

    /// Colour of pixel `(x, y)` of a board anchored at `(0, 0)`, for code that composites the
    /// board into a pixel buffer. Total over the whole `u32` range (no overflow).
    #[must_use]
    pub fn color_at(self, x: u32, y: u32) -> Color32 {
        // `cell_px` is non-zero for every preset (the only constructors).
        let cell = u32::from(self.cell_px);
        // Parity of the CELL index sum, via XOR so it cannot overflow.
        if ((x / cell) ^ (y / cell)) & 1 == 0 { self.origin } else { self.alternate }
    }

    /// One 2x2-cell period of the board (`2 * cell_px` pixels on a side), origin cell top-left.
    ///
    /// Repeating this tile reproduces [`Self::color_at`] exactly.
    #[must_use]
    pub fn tile_image(self) -> ColorImage {
        let side = usize::from(self.cell_px) * 2;
        let side_u32 = u32::from(self.cell_px) * 2;
        let pixels = (0..side_u32).flat_map(|y| (0..side_u32).map(move |x| self.color_at(x, y))).collect();
        ColorImage::new([side, side], pixels)
    }

    /// Paints the board over `rect` with corners rounded by `corner_radius`, as one textured rect.
    ///
    /// The pattern is anchored at `rect`'s top-left corner, one texel per point. Paints nothing
    /// for an empty, negative or non-finite `rect`; never panics. The tile texture is uploaded on
    /// the first call for this board on `painter`'s context and reused afterwards.
    pub fn paint(self, painter: &Painter, rect: Rect, corner_radius: impl Into<CornerRadius>) {
        let Some(uv) = self.uv_rect(rect.size()) else {
            return;
        };
        let texture = self.texture(painter.ctx());
        painter.add(egui::epaint::RectShape::filled(rect, corner_radius, Color32::WHITE).with_texture(texture.id(), uv));
    }

    /// UV rect tiling the texture over `painted_size` points at one texel per point, or `None`
    /// for a degenerate or non-finite size.
    fn uv_rect(self, painted_size: Vec2) -> Option<Rect> {
        if !(painted_size.x.is_finite() && painted_size.y.is_finite()) || painted_size.x <= 0.0 || painted_size.y <= 0.0 {
            return None;
        }
        let tile = f32::from(self.cell_px) * 2.0;
        Some(Rect::from_min_size(Pos2::ZERO, painted_size / tile))
    }

    /// The repeating tile texture of this board on `ctx`, uploaded on first use.
    ///
    /// The handle cached in `ctx`'s temp data keeps the GPU texture alive across frames.
    fn texture(self, ctx: &egui::Context) -> TextureHandle {
        let id = egui::Id::new(("ms_theme::checkerboard", self));
        if let Some(handle) = ctx.data(|data| data.get_temp::<TextureHandle>(id)) {
            return handle;
        }
        // The upload must NOT happen inside `data_mut`: both lock the same `Context`, so nesting
        // them would deadlock the GUI thread.
        let handle = ctx.load_texture("ms_theme_checkerboard", self.tile_image(), TextureOptions::NEAREST_REPEAT);
        ctx.data_mut(|data| data.insert_temp(id, handle.clone()));
        handle
    }
}

/// Paints the [`CANVAS`] board over `rect` with square corners. See [`Checkerboard::paint`].
pub fn paint(painter: &Painter, rect: Rect) {
    CANVAS.paint(painter, rect, CornerRadius::ZERO);
}

#[cfg(test)]
mod tests {
    use super::{CANVAS, INK_PREVIEW_DARK, INK_PREVIEW_LIGHT, paint};
    use egui::{Pos2, Rect, Vec2};

    #[test]
    fn color_at_switches_exactly_on_cell_boundaries() {
        let board = CANVAS;
        let c = u32::from(board.cell_px());
        assert_eq!(board.color_at(0, 0), board.origin());
        assert_eq!(board.color_at(c - 1, c - 1), board.origin());
        assert_eq!(board.color_at(c, 0), board.alternate());
        assert_eq!(board.color_at(0, c), board.alternate());
        assert_eq!(board.color_at(c, c), board.origin());
        assert_eq!(board.color_at(2 * c, 0), board.origin());
    }

    #[test]
    fn color_at_is_total_at_the_u32_edge() {
        for board in [CANVAS, INK_PREVIEW_DARK, INK_PREVIEW_LIGHT] {
            let color = board.color_at(u32::MAX, u32::MAX);
            assert!(color == board.origin() || color == board.alternate());
        }
    }

    #[test]
    fn tile_image_is_one_period_of_color_at() {
        let board = INK_PREVIEW_LIGHT;
        let tile = board.tile_image();
        let side = usize::from(board.cell_px()) * 2;
        assert_eq!(tile.size, [side, side]);
        for y in 0..side {
            for x in 0..side {
                let expected = board.color_at(u32::try_from(x).unwrap_or(u32::MAX), u32::try_from(y).unwrap_or(u32::MAX));
                assert_eq!(tile.pixels[y * side + x], expected, "pixel ({x}, {y})");
            }
        }
    }

    #[test]
    fn paint_adds_one_shape_and_nothing_for_a_degenerate_rect() {
        let ctx = egui::Context::default();
        let empty = ctx.run_ui(egui::RawInput::default(), |ui| {
            paint(ui.painter(), Rect::from_min_size(Pos2::ZERO, Vec2::ZERO));
            paint(ui.painter(), Rect::from_min_size(Pos2::ZERO, Vec2::new(f32::NAN, 10.0)));
        });
        let painted = ctx.run_ui(egui::RawInput::default(), |ui| {
            paint(ui.painter(), Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 50.0)));
        });
        let (empty_len, painted_len) = (empty.shapes.len(), painted.shapes.len());
        // Headless: no renderer consumes the font-atlas upload, and egui 0.36 panics when a
        // `TexturesDelta` is dropped unapplied — discard the deltas before any assert can unwind.
        empty.drop_without_applying_deltas();
        painted.drop_without_applying_deltas();
        assert_eq!(painted_len, empty_len + 1);
    }

    #[test]
    fn texture_is_uploaded_once_per_board() {
        let ctx = egui::Context::default();
        let first = CANVAS.texture(&ctx);
        let again = CANVAS.texture(&ctx);
        let other = INK_PREVIEW_DARK.texture(&ctx);
        assert_eq!(first.id(), again.id());
        assert_ne!(first.id(), other.id());
    }
}
