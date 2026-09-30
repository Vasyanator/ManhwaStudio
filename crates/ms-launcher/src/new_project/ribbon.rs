/*
File: src/launcher/new_project/ribbon.rs

Purpose:
Ribbon page model and image-to-tile conversion for the New Project launcher window.

Main responsibilities:
- hold the current imported source path and ribbon pages;
- convert decoded source images into tiled previews (`RibbonTiles`, via `egui-large-image`);
- paint a tiled preview with culled, per-frame budgeted texture uploads (`paint_ribbon_tiles`);
- preserve original images and crop metadata for non-destructive page trimming;
- keep the rendering data independent from source import logic.

Key structures:
- RibbonState
- RibbonPage
- RibbonTiles
- ImportedImage

Key functions:
- build_ribbon_pages() / build_ribbon_tiles() — CPU split (worker-safe)
- ribbon_upload_budget() / paint_ribbon_tiles() — GUI-thread upload + draw

Notes:
Source selection and background import live in `open_source.rs`. This module only owns
the ribbon view-model and the conversion pipeline from decoded images to tiles. Tile
geometry, CPU split, upload budgeting and culled drawing are delegated to the
`egui-large-image` crate; this file only fixes the ribbon's policy (tile side, budget,
cull margin, placeholder).
*/

use egui_large_image::{
    Alpha, Placement, PreparedTiles, TiledTexture, UploadBudget, UploadScope,
};
use image::{DynamicImage, RgbaImage};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Side of a ribbon tile in pixels. Kept at the historical 2048 (the old strip height, and
/// `egui_large_image::PORTABLE_TILE_SIDE`): changing it moves the clamp-to-edge seams and so
/// changes the rendered pixels. Splits run on worker threads, where no `egui::Context` exists
/// to ask for the backend limit.
const RIBBON_TILE_SIDE: usize = egui_large_image::PORTABLE_TILE_SIDE;

/// Per-frame upload allowance of one ribbon surface (the ribbon scroll area, or the crop
/// editor): at most this many tiles ...
const RIBBON_UPLOAD_TILES_PER_FRAME: usize = 4;
/// ... and this many bytes (a full 2048x2048 RGBA tile is 16 MiB). Same values as the studio's
/// source-page upload budget.
const RIBBON_UPLOAD_BYTES_PER_FRAME: usize = 24 * 1024 * 1024;

/// Screen margin, in points, around the clip rect within which tiles are uploaded and drawn,
/// so tiles about to scroll into view are usually resident already.
const RIBBON_CULL_MARGIN: f32 = 128.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RibbonCrop {
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
}

pub struct ImportedImage {
    pub name: String,
    pub image: DynamicImage,
}

/// Tiled preview of one ribbon image: CPU tiles split once, GPU textures created lazily on the
/// first paint and uploaded tile by tile under a per-frame budget.
///
/// `prepared` is `None` only when the split (or the texture set) failed (logged); such a preview
/// draws nothing.
#[derive(Debug)]
pub struct RibbonTiles {
    prepared: Option<PreparedTiles>,
    texture: Option<TiledTexture>,
}

impl Clone for RibbonTiles {
    /// Shares the CPU tiles (`Arc`s, no pixel copy) but not the GPU textures: `TiledTexture` has
    /// a single owner, so a clone (e.g. the original-page snapshot restored later) creates its
    /// own textures lazily on its first paint and re-uploads them under the ribbon budget.
    fn clone(&self) -> Self {
        Self { prepared: self.prepared.clone(), texture: None }
    }
}

#[derive(Clone)]
pub struct RibbonPage {
    pub name: String,
    pub original_size: [usize; 2],
    full_image: Arc<RgbaImage>,
    source_image: Arc<RgbaImage>,
    crop: Option<RibbonCrop>,
    pub tiles: RibbonTiles,
}

pub struct RibbonState {
    loaded_source: Option<PathBuf>,
    pages: Vec<RibbonPage>,
    original_pages: Vec<RibbonPage>,
}

#[derive(Debug)]
pub enum RibbonMergeError {
    MissingPage,
    WidthMismatch {
        first_name: String,
        first_width: usize,
        second_name: String,
        second_width: usize,
    },
}

impl RibbonState {
    pub fn new() -> Self {
        Self {
            loaded_source: None,
            pages: Vec::new(),
            original_pages: Vec::new(),
        }
    }

    pub fn loaded_source(&self) -> Option<&Path> {
        self.loaded_source.as_deref()
    }

    pub fn pages(&self) -> &[RibbonPage] {
        &self.pages
    }

    pub fn pages_mut(&mut self) -> &mut [RibbonPage] {
        self.pages.as_mut_slice()
    }

    pub fn replace_source(&mut self, source_path: PathBuf, pages: Vec<RibbonPage>) {
        self.loaded_source = Some(source_path);
        self.original_pages = pages.clone();
        self.pages = pages;
    }

    pub fn replace_current(&mut self, pages: Vec<RibbonPage>) {
        self.pages = pages;
    }

    pub fn insert_pages(
        &mut self,
        source_path: PathBuf,
        insert_at: usize,
        pages: Vec<RibbonPage>,
    ) -> Range<usize> {
        self.loaded_source = Some(source_path);
        let insert_at = insert_at.min(self.pages.len());
        let inserted_len = pages.len();
        let original_pages = pages.clone();
        self.pages.splice(insert_at..insert_at, pages);
        let original_insert_at = insert_at.min(self.original_pages.len());
        self.original_pages
            .splice(original_insert_at..original_insert_at, original_pages);
        insert_at..insert_at + inserted_len
    }

    pub fn can_restore_original(&self) -> bool {
        !self.original_pages.is_empty()
            && (self.pages.len() != self.original_pages.len()
                || self
                    .pages
                    .iter()
                    .zip(self.original_pages.iter())
                    .any(|(current, original)| {
                        current.original_size != original.original_size
                            || current.name != original.name
                    }))
    }

    pub fn restore_original(&mut self) -> bool {
        if self.original_pages.is_empty() {
            return false;
        }
        self.pages = self.original_pages.clone();
        true
    }

    pub fn remove_page(&mut self, index: usize) -> Option<RibbonPage> {
        if index < self.pages.len() {
            Some(self.pages.remove(index))
        } else {
            None
        }
    }

    pub fn move_page_up(&mut self, index: usize) -> bool {
        if index == 0 || index >= self.pages.len() {
            return false;
        }
        self.pages.swap(index - 1, index);
        true
    }

    pub fn move_page_down(&mut self, index: usize) -> bool {
        if index + 1 >= self.pages.len() {
            return false;
        }
        self.pages.swap(index, index + 1);
        true
    }

    pub fn merge_with_next(&mut self, index: usize) -> Result<(), RibbonMergeError> {
        if index + 1 >= self.pages.len() {
            return Err(RibbonMergeError::MissingPage);
        }
        let first = &self.pages[index];
        let second = &self.pages[index + 1];
        if first.original_size[0] != second.original_size[0] {
            return Err(RibbonMergeError::WidthMismatch {
                first_name: first.name.clone(),
                first_width: first.original_size[0],
                second_name: second.name.clone(),
                second_width: second.original_size[0],
            });
        }

        let first_image = first.full_image.as_ref();
        let second_image = second.full_image.as_ref();
        let width = first_image.width();
        let height = first_image.height().saturating_add(second_image.height());
        let mut merged = RgbaImage::new(width, height);
        image::imageops::overlay(&mut merged, first_image, 0, 0);
        image::imageops::overlay(
            &mut merged,
            second_image,
            0,
            i64::from(first_image.height()),
        );
        let merged_name = format!("{}_{}", first.name, second.name);
        let merged_page = build_ribbon_page(merged_name, DynamicImage::ImageRgba8(merged));
        self.pages.splice(index..=index + 1, [merged_page]);
        Ok(())
    }

    pub fn apply_crop(&mut self, index: usize, crop: RibbonCrop) -> bool {
        let Some(page) = self.pages.get_mut(index) else {
            return false;
        };
        page.apply_crop(crop)
    }

    pub fn clear(&mut self) {
        self.loaded_source = None;
        self.pages.clear();
        self.original_pages.clear();
    }
}

impl RibbonPage {
    pub fn full_image(&self) -> Arc<RgbaImage> {
        Arc::clone(&self.full_image)
    }

    pub fn source_image(&self) -> Arc<RgbaImage> {
        Arc::clone(&self.source_image)
    }

    pub fn crop(&self) -> Option<RibbonCrop> {
        self.crop
    }

    pub fn source_size(&self) -> [usize; 2] {
        [
            usize::try_from(self.source_image.width()).unwrap_or(usize::MAX),
            usize::try_from(self.source_image.height()).unwrap_or(usize::MAX),
        ]
    }

    fn apply_crop(&mut self, crop: RibbonCrop) -> bool {
        let normalized_crop = normalize_crop(
            crop,
            usize::try_from(self.source_image.width()).unwrap_or(usize::MAX),
            usize::try_from(self.source_image.height()).unwrap_or(usize::MAX),
        );
        if let Some(normalized_crop) = normalized_crop {
            self.crop = (!is_full_image_crop(
                normalized_crop,
                usize::try_from(self.source_image.width()).unwrap_or(usize::MAX),
                usize::try_from(self.source_image.height()).unwrap_or(usize::MAX),
            ))
            .then_some(normalized_crop);
            let rendered = render_page_image(self.source_image.as_ref(), self.crop);
            self.original_size = [
                usize::try_from(rendered.width()).unwrap_or(usize::MAX),
                usize::try_from(rendered.height()).unwrap_or(usize::MAX),
            ];
            self.full_image = Arc::new(rendered);
            self.tiles = build_ribbon_tiles(self.full_image.as_ref());
            true
        } else {
            false
        }
    }
}

pub fn build_ribbon_pages(images: Vec<ImportedImage>) -> Vec<RibbonPage> {
    images
        .into_iter()
        .map(|image| build_ribbon_page(image.name, image.image))
        .collect()
}

fn build_ribbon_page(name: String, image: DynamicImage) -> RibbonPage {
    let source_image = Arc::new(image.to_rgba8());
    let full_image = Arc::clone(&source_image);
    let original_size = [
        usize::try_from(full_image.width()).unwrap_or(usize::MAX),
        usize::try_from(full_image.height()).unwrap_or(usize::MAX),
    ];
    RibbonPage {
        name,
        original_size,
        full_image: Arc::clone(&full_image),
        source_image,
        crop: None,
        tiles: build_ribbon_tiles(full_image.as_ref()),
    }
}

/// Splits `image` into `RIBBON_TILE_SIDE` tiles for the ribbon preview (row-major grid; pages
/// wider than the side get several columns). CPU-bound: call it off the GUI thread where the
/// caller can. A split failure (cannot happen for a well-formed `RgbaImage`) is logged and
/// yields a preview that draws nothing.
pub fn build_ribbon_tiles(image: &RgbaImage) -> RibbonTiles {
    let size = [
        usize::try_from(image.width()).unwrap_or(usize::MAX),
        usize::try_from(image.height()).unwrap_or(usize::MAX),
    ];
    let prepared = match PreparedTiles::from_rgba(image.as_raw(), size, Alpha::Unmultiplied, RIBBON_TILE_SIDE) {
        Ok(prepared) => Some(prepared),
        Err(err) => {
            ms_log::runtime_log::log_error(format!(
                "[launcher.new_project.ribbon] failed to split a {}x{} page into preview tiles: {err}. \
                 The page preview stays empty; the page pixels are unaffected.",
                size[0], size[1]
            ));
            None
        }
    };
    RibbonTiles { prepared, texture: None }
}

/// A fresh upload budget for one ribbon surface for the current frame. Create one per frame and
/// share it across every page painted on that surface.
#[must_use]
pub fn ribbon_upload_budget() -> UploadBudget {
    UploadBudget::new(RIBBON_UPLOAD_TILES_PER_FRAME, RIBBON_UPLOAD_BYTES_PER_FRAME)
}

/// Uploads (within `budget`) and draws the tiles of `tiles` that are on screen.
///
/// The image's pixel `(0, 0)` sits at `image_rect.min` and each pixel is `scale` points wide.
/// Only tiles meeting the clip rect expanded by `RIBBON_CULL_MARGIN` are uploaded and drawn;
/// visible tiles still waiting for their texture get a neutral `faint_bg_color` placeholder, and
/// a repaint is requested until they arrive. Textures are named `"{texture_prefix}-{tile}"` and
/// sampled `TextureOptions::LINEAR`; the prefix is fixed when the first paint creates them.
/// GUI thread only.
pub fn paint_ribbon_tiles(
    ui: &egui::Ui,
    image_rect: egui::Rect,
    scale: f32,
    tiles: &mut RibbonTiles,
    texture_prefix: &str,
    budget: &mut UploadBudget,
) {
    let Some(prepared) = tiles.prepared.as_mut() else {
        return;
    };
    if tiles.texture.is_none() {
        match TiledTexture::new(*prepared.grid(), texture_prefix, egui::TextureOptions::LINEAR) {
            Ok(texture) => tiles.texture = Some(texture),
            Err(err) => {
                // Unreachable for grids `PreparedTiles` accepted (same tile-count cap); dropping
                // the CPU tiles keeps a broken preview from retrying and logging every frame.
                let [width, height] = prepared.grid().image_size();
                ms_log::runtime_log::log_error(format!(
                    "[launcher.new_project.ribbon] failed to create preview textures for a {width}x{height} page: {err}. \
                     The page preview stays empty; the page pixels are unaffected."
                ));
                tiles.prepared = None;
                return;
            }
        }
    }
    let Some(texture) = tiles.texture.as_mut() else {
        return;
    };
    let cull = ui.clip_rect().expand(RIBBON_CULL_MARGIN);
    let placement = Placement::scaled(image_rect.min, scale);
    let report = texture.upload(ui.ctx(), prepared, budget, UploadScope::visible(placement, cull));
    let painter = ui.painter();
    // Uploads are culled and budgeted, so a tile may show up a few frames after it scrolls into
    // view; mark where it will appear instead of leaving a hole in the ribbon.
    for (index, screen) in texture.visible_tiles(placement, cull) {
        if texture.texture(index).is_none() {
            painter.rect_filled(screen, 0.0, ui.visuals().faint_bg_color);
        }
    }
    texture.paint(painter, placement, cull, egui::Color32::WHITE);
    // Only budget deferrals need another frame: `PreparedTiles` hold every tile and never answer
    // `NotReady`, so `awaiting_source` is always 0 here, and rejected tiles are settled.
    if report.wants_repaint() {
        ui.ctx().request_repaint();
    }
}

fn normalize_crop(
    crop: RibbonCrop,
    source_width: usize,
    source_height: usize,
) -> Option<RibbonCrop> {
    if source_width == 0 || source_height == 0 {
        return None;
    }
    let left = crop.left.min(source_width.saturating_sub(1));
    let top = crop.top.min(source_height.saturating_sub(1));
    let max_width = source_width.saturating_sub(left);
    let max_height = source_height.saturating_sub(top);
    let width = crop.width.clamp(1, max_width.max(1));
    let height = crop.height.clamp(1, max_height.max(1));
    Some(RibbonCrop {
        left,
        top,
        width,
        height,
    })
}

fn is_full_image_crop(crop: RibbonCrop, source_width: usize, source_height: usize) -> bool {
    crop.left == 0 && crop.top == 0 && crop.width == source_width && crop.height == source_height
}

fn render_page_image(source_image: &RgbaImage, crop: Option<RibbonCrop>) -> RgbaImage {
    let Some(crop) = crop else {
        return source_image.clone();
    };
    let left = u32::try_from(crop.left).unwrap_or(u32::MAX);
    let top = u32::try_from(crop.top).unwrap_or(u32::MAX);
    let width = u32::try_from(crop.width).unwrap_or(u32::MAX);
    let height = u32::try_from(crop.height).unwrap_or(u32::MAX);
    image::imageops::crop_imm(source_image, left, top, width, height).to_image()
}

#[cfg(test)]
mod tests {
    use super::{ImportedImage, RibbonCrop, RibbonState, build_ribbon_pages, build_ribbon_tiles};
    use image::{DynamicImage, Rgba, RgbaImage};
    use std::path::PathBuf;

    fn sample_image() -> DynamicImage {
        let mut image = RgbaImage::new(8, 6);
        for y in 0..6 {
            for x in 0..8 {
                image.put_pixel(x, y, Rgba([x as u8, y as u8, 0, 255]));
            }
        }
        DynamicImage::ImageRgba8(image)
    }

    /// Tile geometry `(cols, rows)` and tile sizes of a ribbon preview for a `width x height` page.
    fn preview_grid(width: u32, height: u32) -> (usize, usize, Vec<[usize; 2]>) {
        let tiles = build_ribbon_tiles(&RgbaImage::new(width, height));
        let prepared = tiles.prepared.expect("a well-formed image always splits");
        let grid = *prepared.grid();
        let sizes = grid.tiles().map(|(_, rect)| [rect.width, rect.height]).collect();
        (grid.cols(), grid.rows(), sizes)
    }

    #[test]
    fn cloned_preview_shares_cpu_tiles_but_not_textures() {
        let mut tiles = build_ribbon_tiles(&RgbaImage::new(64, 64));
        let grid = *tiles.prepared.as_ref().expect("a well-formed image always splits").grid();
        tiles.texture = Some(
            egui_large_image::TiledTexture::new(grid, "clone-test", egui::TextureOptions::LINEAR)
                .expect("a 1-tile grid is within the tile cap"),
        );
        let clone = tiles.clone();
        assert!(clone.texture.is_none(), "a clone creates its own textures on first paint");
        let original_tile = tiles.prepared.as_ref().and_then(|p| p.tile(0));
        let cloned_tile = clone.prepared.as_ref().and_then(|p| p.tile(0));
        assert!(
            matches!((original_tile, cloned_tile), (Some(a), Some(b)) if std::sync::Arc::ptr_eq(a, b)),
            "CPU tiles are shared, not copied"
        );
    }

    #[test]
    fn narrow_page_splits_into_2048_px_strips() {
        let (cols, rows, sizes) = preview_grid(800, 5000);
        assert_eq!((cols, rows), (1, 3));
        assert_eq!(sizes, vec![[800, 2048], [800, 2048], [800, 904]]);
    }

    #[test]
    fn page_wider_than_tile_side_splits_into_columns() {
        let (cols, rows, sizes) = preview_grid(2500, 3000);
        assert_eq!((cols, rows), (2, 2));
        assert_eq!(sizes, vec![[2048, 2048], [452, 2048], [2048, 952], [452, 952]]);
    }

    #[test]
    fn crop_rebuilds_preview_tiles_for_the_cropped_size() {
        let mut pages = build_ribbon_pages(vec![ImportedImage {
            name: "page".to_string(),
            image: sample_image(),
        }]);
        let page = pages.first_mut().expect("page should exist");
        assert!(page.apply_crop(RibbonCrop { left: 1, top: 1, width: 5, height: 3 }));
        let prepared = page.tiles.prepared.as_ref().expect("cropped page splits");
        assert_eq!(prepared.grid().image_size(), [5, 3]);
        assert!(page.tiles.texture.is_none(), "textures of the old size must not be reused");
    }

    #[test]
    fn apply_crop_updates_rendered_page_and_keeps_crop_metadata() {
        let mut pages = build_ribbon_pages(vec![ImportedImage {
            name: "page".to_string(),
            image: sample_image(),
        }]);
        let page = pages.first_mut().expect("page should exist");

        let changed = page.apply_crop(RibbonCrop {
            left: 2,
            top: 1,
            width: 3,
            height: 4,
        });

        assert!(changed);
        assert_eq!(
            page.crop(),
            Some(RibbonCrop {
                left: 2,
                top: 1,
                width: 3,
                height: 4,
            })
        );
        assert_eq!(page.original_size, [3, 4]);
        assert_eq!(page.full_image().width(), 3);
        assert_eq!(page.full_image().height(), 4);
    }

    #[test]
    fn full_image_crop_clears_crop_metadata_and_keeps_original_size() {
        let mut pages = build_ribbon_pages(vec![ImportedImage {
            name: "page".to_string(),
            image: sample_image(),
        }]);
        let page = pages.first_mut().expect("page should exist");

        let changed = page.apply_crop(RibbonCrop {
            left: 0,
            top: 0,
            width: 8,
            height: 6,
        });

        assert!(changed);
        assert_eq!(page.crop(), None);
        assert_eq!(page.original_size, [8, 6]);
        assert_eq!(page.full_image().width(), 8);
        assert_eq!(page.full_image().height(), 6);
    }

    #[test]
    fn insert_pages_inserts_into_current_and_original_sequences() {
        let mut ribbon = RibbonState::new();
        ribbon.replace_source(
            PathBuf::from("initial"),
            build_ribbon_pages(vec![
                ImportedImage {
                    name: "a".to_string(),
                    image: sample_image(),
                },
                ImportedImage {
                    name: "b".to_string(),
                    image: sample_image(),
                },
            ]),
        );

        let inserted = ribbon.insert_pages(
            PathBuf::from("extra"),
            1,
            build_ribbon_pages(vec![ImportedImage {
                name: "x".to_string(),
                image: sample_image(),
            }]),
        );

        assert_eq!(inserted, 1..2);
        let names: Vec<_> = ribbon
            .pages()
            .iter()
            .map(|page| page.name.as_str())
            .collect();
        assert_eq!(names, vec!["a", "x", "b"]);
        assert!(ribbon.restore_original());
        let restored_names: Vec<_> = ribbon
            .pages()
            .iter()
            .map(|page| page.name.as_str())
            .collect();
        assert_eq!(restored_names, vec!["a", "x", "b"]);
    }
}
