/*
File: src/launcher/new_project/ribbon.rs

Purpose:
Ribbon page model and image-to-tile conversion for the New Project launcher window.

Main responsibilities:
- hold the current imported source path and ribbon pages;
- convert decoded source images into tiled egui-friendly ribbon pages;
- preserve original images and crop metadata for non-destructive page trimming;
- keep the rendering data independent from source import logic.

Key structures:
- RibbonState
- RibbonPage
- RibbonTile
- ImportedImage

Notes:
Source selection and background import live in `open_source.rs`. This module only owns
the ribbon view-model and the conversion pipeline from decoded images to tiles.
*/

use egui::{ColorImage, TextureHandle};
use image::{DynamicImage, GenericImageView, RgbaImage};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const RIBBON_TILE_MAX_HEIGHT: u32 = 2048;

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

#[derive(Clone)]
pub struct RibbonTile {
    pub origin_px: [usize; 2],
    pub size: [usize; 2],
    pub color_image: ColorImage,
    pub texture: Option<TextureHandle>,
}

#[derive(Clone)]
pub struct RibbonPage {
    pub name: String,
    pub original_size: [usize; 2],
    full_image: Arc<RgbaImage>,
    source_image: Arc<RgbaImage>,
    crop: Option<RibbonCrop>,
    pub tiles: Vec<RibbonTile>,
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
            self.tiles = split_image_into_tiles(self.full_image.as_ref());
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
        tiles: split_image_into_tiles(full_image.as_ref()),
    }
}

pub fn build_ribbon_tiles(image: &RgbaImage) -> Vec<RibbonTile> {
    split_image_into_tiles(image)
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

fn split_image_into_tiles(image: &RgbaImage) -> Vec<RibbonTile> {
    let (width, height) = image.dimensions();
    let mut tiles = Vec::new();
    let mut y = 0u32;
    while y < height {
        let tile_height = (height - y).min(RIBBON_TILE_MAX_HEIGHT);
        let tile = image.view(0, y, width, tile_height).to_image();
        let tile_size = [tile.width() as usize, tile.height() as usize];
        tiles.push(RibbonTile {
            origin_px: [0, y as usize],
            size: tile_size,
            color_image: ColorImage::from_rgba_unmultiplied(tile_size, tile.as_raw().as_slice()),
            texture: None,
        });
        y += tile_height;
    }
    tiles
}

#[cfg(test)]
mod tests {
    use super::{ImportedImage, RibbonCrop, RibbonState, build_ribbon_pages};
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
