/*
File: tabs/ps_editor/viewport.rs

Purpose:
Standalone pan/zoom viewport for the PS-like editor. This editor is intentionally NOT a
`CanvasView`; it owns its own camera so the rest of the canvas engine stays untouched.

Key structures:
- `PsViewport`: persistent camera state (zoom + world point shown at the viewport center).
- `ViewTransform`: per-frame immutable snapshot used to convert between image (world) pixel
  coordinates and screen coordinates.

Notes:
World coordinates are image pixel coordinates: the top-left of the page is `(0, 0)` and the
bottom-right is `(width, height)`. Screen coordinates are egui logical points inside the
allocated viewport rect.
*/

use eframe::egui;
use egui::{Pos2, Rect, Vec2};

/// Minimum and maximum zoom factors (screen pixels per image pixel).
const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 32.0;
/// Multiplicative zoom step applied per wheel notch.
const WHEEL_ZOOM_STEP: f32 = 1.0015;

/// Immutable per-frame mapping between image (world) pixels and screen points.
///
/// Construct it once per frame from a `PsViewport` and the allocated viewport rect, then use it
/// for hit-testing, cursor drawing, and tile placement. It is cheap to copy.
#[derive(Debug, Clone, Copy)]
pub struct ViewTransform {
    pub viewport_rect: Rect,
    pub zoom: f32,
    /// World (image px) point displayed at the center of `viewport_rect`.
    pub center_world: Vec2,
}

impl ViewTransform {
    /// Maps an image-space point to its on-screen position.
    #[must_use]
    pub fn world_to_screen(&self, world: Pos2) -> Pos2 {
        self.viewport_rect.center() + (world.to_vec2() - self.center_world) * self.zoom
    }

    /// Maps a screen point back to image-space coordinates (may fall outside the page bounds).
    #[must_use]
    pub fn screen_to_world(&self, screen: Pos2) -> Pos2 {
        (((screen - self.viewport_rect.center()) / self.zoom) + self.center_world).to_pos2()
    }

    /// Screen-space rect covering an image-space rect.
    #[must_use]
    pub fn world_rect_to_screen(&self, world: Rect) -> Rect {
        Rect::from_min_max(
            self.world_to_screen(world.min),
            self.world_to_screen(world.max),
        )
    }
}

/// Persistent camera for the PS-like editor.
///
/// Holds the zoom factor and the world point anchored to the viewport center. Input handling is
/// explicit (`handle_input`) so the editor can gate it when the pointer is over floating UI.
#[derive(Debug, Clone)]
pub struct PsViewport {
    zoom: f32,
    center_world: Vec2,
    initialized: bool,
}

impl Default for PsViewport {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            center_world: Vec2::ZERO,
            initialized: false,
        }
    }
}

impl PsViewport {
    /// Builds the per-frame transform for the given allocated viewport rect.
    #[must_use]
    pub fn transform(&self, viewport_rect: Rect) -> ViewTransform {
        ViewTransform {
            viewport_rect,
            zoom: self.zoom,
            center_world: self.center_world,
        }
    }

    #[must_use]
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// World (image px) point currently anchored to the viewport center.
    #[must_use]
    pub fn center_world(&self) -> Vec2 {
        self.center_world
    }

    /// Sets the camera directly from an external view (e.g. synced from `CanvasView`).
    ///
    /// `zoom` is clamped to this viewport's own limits, so a canvas zoom outside the PS editor's
    /// range is honored as far as possible ("в доступных пределах"). Marks the camera initialized
    /// so a subsequent `fit_page_if_needed` does not override the synced position.
    pub fn set_camera(&mut self, zoom: f32, center_world: Vec2) {
        self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        self.center_world = center_world;
        self.initialized = true;
    }

    /// Resets the camera so a freshly loaded page is fit on the next `fit_page` call.
    pub fn invalidate(&mut self) {
        self.initialized = false;
    }

    /// Fits a page of `page_size` (image px) into `viewport_rect`, centering it.
    ///
    /// Always runs when the camera has not been initialized for the current page; otherwise it is
    /// a no-op so user pan/zoom is preserved across frames.
    pub fn fit_page_if_needed(&mut self, viewport_rect: Rect, page_size: [usize; 2]) {
        if self.initialized {
            return;
        }
        self.fit_page(viewport_rect, page_size);
        self.initialized = true;
    }

    /// Fits and centers a page regardless of the initialized flag (used by the "Вписать" button).
    pub fn fit_page(&mut self, viewport_rect: Rect, page_size: [usize; 2]) {
        let w = page_size[0].max(1) as f32;
        let h = page_size[1].max(1) as f32;
        let margin = 0.97;
        let zoom_x = viewport_rect.width() / w;
        let zoom_y = viewport_rect.height() / h;
        self.zoom = (zoom_x.min(zoom_y) * margin).clamp(MIN_ZOOM, MAX_ZOOM);
        self.center_world = Vec2::new(w * 0.5, h * 0.5);
        self.initialized = true;
    }

    /// Sets 1:1 zoom (one image pixel per screen point) keeping the page centered.
    pub fn reset_to_actual_size(&mut self, page_size: [usize; 2]) {
        self.zoom = 1.0;
        self.center_world = Vec2::new(
            page_size[0].max(1) as f32 * 0.5,
            page_size[1].max(1) as f32 * 0.5,
        );
        self.initialized = true;
    }

    /// Applies wheel-zoom anchored on `anchor_screen` and middle/space drag panning.
    ///
    /// `wheel_delta_y` is the raw scroll delta; `pan_delta` is the drag delta in screen points to
    /// apply this frame (already gated by the caller's button/key logic).
    pub fn handle_input(
        &mut self,
        viewport_rect: Rect,
        anchor_screen: Option<Pos2>,
        wheel_delta_y: f32,
        pan_delta: Vec2,
    ) {
        if pan_delta != Vec2::ZERO && self.zoom > f32::EPSILON {
            self.center_world -= pan_delta / self.zoom;
        }

        if wheel_delta_y.abs() > f32::EPSILON {
            let anchor = anchor_screen.unwrap_or_else(|| viewport_rect.center());
            let before = self.transform(viewport_rect).screen_to_world(anchor);
            let factor = WHEEL_ZOOM_STEP.powf(wheel_delta_y);
            self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
            // Keep the world point under the cursor stationary while zooming.
            let after = self.transform(viewport_rect).screen_to_world(anchor);
            self.center_world += before.to_vec2() - after.to_vec2();
        }
    }
}
