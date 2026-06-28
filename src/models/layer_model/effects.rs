/*
File: models/layer_model/effects.rs

Purpose:
Shared seam for the "effects render" (render type 2): apply a post-effects chain over a base image.
The effect engine itself lives in the typing tab's render pipeline (`render_next`); this module
re-exports the reusable, state-free entry point and adds an egui-`ColorImage` bridge so the PS
editor (and any future caller) can run effects on an arbitrary raster layer — not just text.

Conventions (verified against the pipeline):
- The effects JSON is the same contract the typing tab stores (`[{"type":"stroke", ...}, ...]`).
- Both the input buffer and the output are STRAIGHT (unmultiplied) alpha, matching `ColorImage`.
- Effects may ENLARGE the output (e.g. shadow/glow pad the canvas). The returned image can therefore
  be bigger than the input; a caller that positions a layer by its center must recenter accordingly.
- `apply_effects_to_image` is pure (no global/font/GPU state) and safe to call on any thread, but a
  heavy effect on a large image can take tens of ms — run it off the GUI thread when that matters.
*/

use crate::tabs::typing::render_next::pipeline::apply_effects_to_image;
use eframe::egui::ColorImage;

/// Applies an effects chain to a `ColorImage`, returning the rendered result and the **content
/// origin**: the pixel offset `[x, y]` of the original content's top-left within the result.
///
/// `effects_json` is the typing-tab effects contract; an empty or blank string returns the image
/// unchanged (origin `[0, 0]`). The result may be larger than `image` when an effect expands the
/// canvas (shadow/glow); the origin lets a center-anchored caller recenter so the content stays put.
pub fn apply_effects_to_color_image(
    image: &ColorImage,
    effects_json: &str,
) -> Result<(ColorImage, [i32; 2]), String> {
    let [w, h] = image.size;
    let mut straight = Vec::with_capacity(w * h * 4);
    for px in &image.pixels {
        straight.extend_from_slice(&px.to_srgba_unmultiplied());
    }
    let rendered = apply_effects_to_image(straight, w as u32, h as u32, effects_json, None)?;
    let out = ColorImage::from_rgba_unmultiplied(
        [rendered.width as usize, rendered.height as usize],
        &rendered.rgba,
    );
    Ok((
        out,
        [
            rendered.content_origin_x as i32,
            rendered.content_origin_y as i32,
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Color32;

    /// A small transparent image with a single opaque dot in the center.
    fn dot(size: usize) -> ColorImage {
        let mut img = ColorImage::filled([size, size], Color32::TRANSPARENT);
        img.pixels[(size / 2) * size + size / 2] = Color32::WHITE;
        img
    }

    #[test]
    fn empty_effects_round_trip_is_unchanged() {
        let img = dot(5);
        let (out, origin) = apply_effects_to_color_image(&img, "").unwrap();
        assert_eq!(out.size, img.size);
        assert_eq!(origin, [0, 0]);
    }

    #[test]
    fn stroke_outlines_without_growing_canvas() {
        let img = dot(7);
        let (out, _origin) =
            apply_effects_to_color_image(&img, r#"[{"type":"stroke","width":1.0,"color":[0,0,0,255]}]"#)
                .unwrap();
        // Stroke does not expand the canvas.
        assert_eq!(out.size, [7, 7]);
        // A neighbor of the center dot, transparent before, is now part of the outline.
        let n = 7;
        let neighbor = out.pixels[(n / 2) * n + (n / 2 - 1)];
        assert!(neighbor.a() > 0, "stroke should outline the dot's neighbors");
    }

    #[test]
    fn shadow_can_enlarge_the_canvas() {
        let img = dot(7);
        let (out, origin) = apply_effects_to_color_image(
            &img,
            r#"[{"type":"shadow","offset_x":5,"offset_y":5,"blur":2.0,"color":[0,0,0,200]}]"#,
        )
        .unwrap();
        assert!(
            out.size[0] > 7 || out.size[1] > 7,
            "shadow with offset+blur should expand the canvas, got {:?}",
            out.size
        );
        // The original content is offset inside the enlarged canvas, and still fits.
        assert!(origin[0] >= 0 && origin[1] >= 0);
        assert!(origin[0] as usize + 7 <= out.size[0] && origin[1] as usize + 7 <= out.size[1]);
    }
}
