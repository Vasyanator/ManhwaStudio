/*
File: src/canvas/view_transform.rs

Purpose:
Shadow-only world<->screen affine transform for the canvas. It captures the
same mapping the `ScrollArea`-based page layout already produces
(`screen = world * scale + translation`), but nothing consumes it yet: future
camera-based increments will make it authoritative and replace the implicit
`ScrollArea` zoom/scroll math.

Key structures:
- DVec2: minimal f64 2D vector (no external dependency; see note below).
- ViewTransform: affine map between content/world pixels and egui screen points.

Key functions:
- ViewTransform::world_to_screen / screen_to_world
- ViewTransform::world_rect_to_screen
- ViewTransform::with_anchor
- ViewTransform::clamp_translation

Notes:
- The project has no `glam` dependency (verified against Cargo.toml / Cargo.lock),
  and the task forbids adding one, so a local `DVec2` is used for the f64 offset.
- World space = content-centered source-image pixels (the unscaled
  `page_world_rects` strip). Screen space = egui points. Internal math is f64;
  values are downcast to f32 only at the egui boundary.
- `scale` is clamped to `MIN_SCALE..=MAX_SCALE` wherever a scale is set, matching
  the canvas zoom clamp.
*/

use eframe::egui;
use egui::{Pos2, Rect};

/// Minimum allowed view scale, mirroring the canvas zoom clamp lower bound.
pub(crate) const MIN_SCALE: f32 = 0.2;
/// Maximum allowed view scale, mirroring the canvas zoom clamp upper bound.
pub(crate) const MAX_SCALE: f32 = 5.0;

/// Minimal f64 2D vector used for the screen-point translation offset.
///
/// A dedicated type keeps the translation in f64 until the egui boundary, where
/// it is downcast to f32. It exists only because the project intentionally has
/// no `glam` dependency.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DVec2 {
    pub x: f64,
    pub y: f64,
}

impl DVec2 {
    /// The zero vector.
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    /// Constructs a vector from its components.
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Affine world<->screen transform: `screen = world * scale + translation`.
///
/// `scale` is the uniform zoom (f32, clamped to `MIN_SCALE..=MAX_SCALE`).
/// `translation` is an f64 screen-point offset (the screen position of the world
/// origin). Internal mapping math is performed in f64 and downcast to f32 only at
/// the egui boundary.
///
/// This type is currently shadow-only: it is computed from the existing
/// `ScrollArea` layout but not yet read by any rendering path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewTransform {
    pub scale: f32,
    pub translation: DVec2,
}

impl Default for ViewTransform {
    fn default() -> Self {
        Self {
            scale: 1.0,
            translation: DVec2::ZERO,
        }
    }
}

impl ViewTransform {
    /// Builds a transform with the given scale (clamped) and translation offset.
    #[must_use]
    pub fn new(scale: f32, translation: DVec2) -> Self {
        Self {
            scale: scale.clamp(MIN_SCALE, MAX_SCALE),
            translation,
        }
    }

    /// Maps a world/content-pixel point to a screen point.
    ///
    /// Applies `screen = world * scale + translation`. The downcast to f32 is the
    /// single allowed lossy step at the egui boundary; screen points are f32.
    #[must_use]
    pub fn world_to_screen(&self, world: Pos2) -> Pos2 {
        let scale = f64::from(self.scale);
        let sx = f64::from(world.x) * scale + self.translation.x;
        let sy = f64::from(world.y) * scale + self.translation.y;
        // Allowed boundary downcast: egui screen coordinates are f32.
        egui::pos2(sx as f32, sy as f32)
    }

    /// Maps a screen point back to a world/content-pixel point.
    ///
    /// Inverts `world = (screen - translation) / scale`. `scale` is clamped to at
    /// least `MIN_SCALE`, so the division never hits zero.
    #[must_use]
    pub fn screen_to_world(&self, screen: Pos2) -> Pos2 {
        let scale = f64::from(self.scale.clamp(MIN_SCALE, MAX_SCALE));
        let wx = (f64::from(screen.x) - self.translation.x) / scale;
        let wy = (f64::from(screen.y) - self.translation.y) / scale;
        // Allowed boundary downcast: egui screen/world points are f32 here.
        egui::pos2(wx as f32, wy as f32)
    }

    /// Maps a world rectangle to its screen rectangle using the same formula as
    /// `reserve_canvas_page_frame` (`screen = world * scale + translation`).
    #[must_use]
    pub fn world_rect_to_screen(&self, world: Rect) -> Rect {
        Rect::from_min_max(
            self.world_to_screen(world.min),
            self.world_to_screen(world.max),
        )
    }

    /// Returns a transform with `new_scale` (clamped) that keeps `world_anchor`
    /// mapped to `screen_anchor`.
    ///
    /// Used by directed zoom: the point under the cursor stays put while the
    /// scale changes. Derived from `screen_anchor = world_anchor * scale +
    /// translation`, solving for `translation`.
    #[must_use]
    pub fn with_anchor(&self, new_scale: f32, world_anchor: Pos2, screen_anchor: Pos2) -> Self {
        let scale = new_scale.clamp(MIN_SCALE, MAX_SCALE);
        let scale_f64 = f64::from(scale);
        let translation = DVec2::new(
            f64::from(screen_anchor.x) - f64::from(world_anchor.x) * scale_f64,
            f64::from(screen_anchor.y) - f64::from(world_anchor.y) * scale_f64,
        );
        Self { scale, translation }
    }

    /// Returns a copy whose translation is clamped so the scaled content stays
    /// within scroll bounds relative to `viewport_rect`.
    ///
    /// `content_world_rect` is the full content strip in world pixels;
    /// `viewport_rect` is the visible region in screen points. Per axis: if the
    /// scaled content is larger than the viewport, the translation is clamped so
    /// the content edges cannot move inside the viewport edges (no empty gutter);
    /// if it is smaller, the content is centered. This documents the future
    /// camera scroll-bound contract; it is not consumed yet.
    #[must_use]
    pub fn clamp_translation(&self, content_world_rect: Rect, viewport_rect: Rect) -> Self {
        let scale = f64::from(self.scale.clamp(MIN_SCALE, MAX_SCALE));

        let clamp_axis = |content_min: f32,
                          content_extent: f32,
                          viewport_min: f32,
                          viewport_extent: f32,
                          translation: f64|
         -> f64 {
            let scaled_extent = f64::from(content_extent.max(0.0)) * scale;
            let vp_min = f64::from(viewport_min);
            let vp_extent = f64::from(viewport_extent.max(0.0));
            // Screen position of the content's min edge under the current translation.
            let content_screen_min = f64::from(content_min) * scale + translation;
            if scaled_extent <= vp_extent {
                // Content fits: center it in the viewport, so the translation is fixed.
                let centered_screen_min = vp_min + (vp_extent - scaled_extent) * 0.5;
                translation + (centered_screen_min - content_screen_min)
            } else {
                // Content overflows: keep both edges outside the viewport edges.
                // max translation keeps content_min at viewport_min (cannot scroll past start);
                // min translation keeps content_max at viewport_max (cannot scroll past end).
                let max_translation = vp_min - f64::from(content_min) * scale;
                let min_translation =
                    (vp_min + vp_extent) - f64::from(content_min) * scale - scaled_extent;
                translation.clamp(min_translation, max_translation)
            }
        };

        let translation = DVec2::new(
            clamp_axis(
                content_world_rect.min.x,
                content_world_rect.width(),
                viewport_rect.min.x,
                viewport_rect.width(),
                self.translation.x,
            ),
            clamp_axis(
                content_world_rect.min.y,
                content_world_rect.height(),
                viewport_rect.min.y,
                viewport_rect.height(),
                self.translation.y,
            ),
        );

        Self {
            scale: self.scale.clamp(MIN_SCALE, MAX_SCALE),
            translation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-3;

    fn approx_pos(a: Pos2, b: Pos2) -> bool {
        (a.x - b.x).abs() <= EPS && (a.y - b.y).abs() <= EPS
    }

    #[test]
    fn round_trip_screen_world_screen() {
        // screen_to_world(world_to_screen(p)) must recover p across scales.
        for scale in [0.2_f32, 1.0, 5.0] {
            let view = ViewTransform::new(scale, DVec2::new(123.5, -42.25));
            for p in [
                Pos2::new(0.0, 0.0),
                Pos2::new(640.0, 900.0),
                Pos2::new(-37.0, 512.0),
            ] {
                let back = view.screen_to_world(view.world_to_screen(p));
                assert!(
                    approx_pos(back, p),
                    "round trip failed at scale {scale}: {p:?} -> {back:?}"
                );
            }
        }
    }

    #[test]
    fn world_rect_to_screen_matches_reserve_frame_formula() {
        // Mirror reserve_canvas_page_frame: image_rect = world * scale + translation,
        // where translation = image_rect.left_top() - world.left_top() * scale.
        let scale = 2.5_f32;
        let world = Rect::from_min_size(egui::pos2(100.0, 250.0), egui::vec2(800.0, 1200.0));
        // Pick an arbitrary screen top-left, derive translation as the canvas does.
        let image_left_top = egui::pos2(40.0, 16.0);
        let translation = DVec2::new(
            f64::from(image_left_top.x) - f64::from(world.min.x) * f64::from(scale),
            f64::from(image_left_top.y) - f64::from(world.min.y) * f64::from(scale),
        );
        let view = ViewTransform::new(scale, translation);
        let screen = view.world_rect_to_screen(world);

        // Expected via the explicit `world * scale + translation` formula.
        let expect_min = image_left_top;
        let expect_max = egui::pos2(
            (f64::from(world.max.x) * f64::from(scale) + translation.x) as f32,
            (f64::from(world.max.y) * f64::from(scale) + translation.y) as f32,
        );
        assert!(approx_pos(screen.min, expect_min));
        assert!(approx_pos(screen.max, expect_max));
        // Size must equal world size * scale.
        assert!((screen.width() - world.width() * scale).abs() <= 1e-2);
        assert!((screen.height() - world.height() * scale).abs() <= 1e-2);
    }

    #[test]
    fn with_anchor_keeps_anchor_fixed_and_sets_scale() {
        let view = ViewTransform::new(1.0, DVec2::new(10.0, 20.0));
        let world_anchor = Pos2::new(300.0, 450.0);
        let screen_anchor = view.world_to_screen(world_anchor);
        for new_scale in [0.2_f32, 1.0, 3.3, 5.0] {
            let zoomed = view.with_anchor(new_scale, world_anchor, screen_anchor);
            assert!((zoomed.scale - new_scale).abs() <= EPS, "scale not set");
            let mapped = zoomed.world_to_screen(world_anchor);
            assert!(
                approx_pos(mapped, screen_anchor),
                "anchor moved at scale {new_scale}: {mapped:?} != {screen_anchor:?}"
            );
        }
    }

    #[test]
    fn with_anchor_clamps_scale() {
        let view = ViewTransform::default();
        let zoomed = view.with_anchor(100.0, Pos2::ZERO, Pos2::ZERO);
        assert!((zoomed.scale - MAX_SCALE).abs() <= EPS);
        let zoomed = view.with_anchor(0.001, Pos2::ZERO, Pos2::ZERO);
        assert!((zoomed.scale - MIN_SCALE).abs() <= EPS);
    }

    #[test]
    fn clamp_translation_centers_small_content() {
        // Content smaller than viewport on both axes => centered.
        let scale = 1.0_f32;
        let view = ViewTransform::new(scale, DVec2::new(9999.0, -9999.0));
        let content = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 100.0));
        let viewport = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 400.0));
        let clamped = view.clamp_translation(content, viewport);
        // Centered: content min maps to (400-100)/2 = 150 on each axis.
        let mapped_min = clamped.world_to_screen(content.min);
        assert!(approx_pos(mapped_min, egui::pos2(150.0, 150.0)));
    }

    #[test]
    fn clamp_translation_bounds_large_content() {
        // Content larger than viewport => translation clamped to keep edges out.
        let scale = 1.0_f32;
        let content = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1000.0, 1000.0));
        let viewport = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 400.0));

        // Push content far to the right (positive translation): clamp to 0 (start aligned).
        let view = ViewTransform::new(scale, DVec2::new(500.0, 0.0));
        let clamped = view.clamp_translation(content, viewport);
        assert!(
            clamped.translation.x.abs() <= f64::from(EPS),
            "start-aligned translation should clamp to 0, got {}",
            clamped.translation.x
        );

        // Push content far to the left: clamp so content_max sits at viewport_max.
        let view = ViewTransform::new(scale, DVec2::new(-5000.0, 0.0));
        let clamped = view.clamp_translation(content, viewport);
        // content_max screen x = 1000 * 1 + translation = viewport right (400).
        let expected: f64 = 400.0 - 1000.0;
        assert!(
            (clamped.translation.x - expected).abs() <= f64::from(EPS),
            "end-aligned translation expected {expected}, got {}",
            clamped.translation.x
        );
    }

    #[test]
    fn world_rect_to_screen_horizontal_centering_contract() {
        // Complementary to the scene centering test: a page world rect that is
        // horizontally centered in content world width maps to a screen rect whose
        // left/right margins are equal (centering is preserved by the transform).
        let content_world_width = 1400.0_f32;
        let page_width = 800.0_f32;
        let page_left = (content_world_width - page_width) * 0.5;
        let world = Rect::from_min_size(egui::pos2(page_left, 0.0), egui::vec2(page_width, 1000.0));
        let scale = 1.5_f32;
        let view = ViewTransform::new(scale, DVec2::new(7.0, 3.0));
        let content_rect = Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(content_world_width, 1000.0),
        );
        let content_screen = view.world_rect_to_screen(content_rect);
        let page_screen = view.world_rect_to_screen(world);
        let left_margin = page_screen.min.x - content_screen.min.x;
        let right_margin = content_screen.max.x - page_screen.max.x;
        assert!(
            (left_margin - right_margin).abs() <= 1e-2,
            "page must stay centered in content after transform: {left_margin} vs {right_margin}"
        );
    }
}
