/*
File: crates/ms-models/src/page_view.rs

Purpose:
Source-page view model shared by the app shell, the canvas and the tabs: page geometry with
its load state, and the tiled GPU residency of a decoded source page.

Key structures:
- `PageImageInfo`: page pixel size plus `SourcePageLoadState`, independent from GPU residency.
- `SourcePageLoadState`: whether a source page is still decoding, available, or failed.
- `PageTexture` / `TextureTile`: tiled GPU textures for one page, each tile keeping its decoded
  RGBA bytes so a dropped texture can be re-uploaded without decoding again.

Notes:
These types live in `ms-models` (not in `app.rs`) so that `canvas` and `tabs` do not have to
reference the app shell upwards. Producing and evicting the textures stays in `app.rs`.
*/

use eframe::egui;
use std::sync::Arc;

pub struct PageTexture {
    pub tiles: Vec<TextureTile>,
    pub linear_last_used_frame: u64,
    pub nearest_last_used_frame: u64,
}

pub struct TextureTile {
    pub linear_texture: Option<egui::TextureHandle>,
    pub nearest_texture: Option<egui::TextureHandle>,
    pub origin_px: egui::Vec2,
    pub size_px: egui::Vec2,
    pub rgba: Arc<[u8]>,
}

impl PageTexture {
    #[must_use]
    pub fn estimated_linear_gpu_bytes(&self) -> u64 {
        self.tiles
            .iter()
            .filter(|tile| tile.linear_texture.is_some())
            .map(|tile| u64::try_from(tile.rgba.len()).unwrap_or(u64::MAX))
            .sum()
    }

    #[must_use]
    pub fn estimated_nearest_gpu_bytes(&self) -> u64 {
        self.tiles
            .iter()
            .filter(|tile| tile.nearest_texture.is_some())
            .map(|tile| u64::try_from(tile.rgba.len()).unwrap_or(u64::MAX))
            .sum()
    }

    pub fn drop_nearest_textures(&mut self) {
        for tile in &mut self.tiles {
            tile.nearest_texture = None;
        }
        self.nearest_last_used_frame = 0;
    }

    pub fn drop_linear_textures(&mut self) {
        for tile in &mut self.tiles {
            tile.linear_texture = None;
        }
        self.linear_last_used_frame = 0;
    }
}

#[derive(Debug, Clone)]
pub struct PageImageInfo {
    pub width_px: u32,
    pub height_px: u32,
    pub load_state: SourcePageLoadState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePageLoadState {
    Loading,
    Available,
    Failed,
}
