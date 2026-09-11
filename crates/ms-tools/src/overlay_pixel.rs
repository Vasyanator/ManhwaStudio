/*
File: tools/overlay_pixel.rs

Purpose:
Solves the clean-overlay pixel that reproduces a desired FINAL colour over a known, opaque
backdrop pixel. Shared by every tool that writes into a clean overlay: the cleaning tab's brushes
and stamp, and the patch tool's host-side store step.

Main responsibilities:
- Turn `(base, final_color, coverage)` into the un-premultiplied overlay pixel whose composite
  over `base` shows `final_color`, at an alpha of at least `coverage`.

Key functions:
- `overlay_pixel_for_final_color()`.

Notes:
The DENSE (not minimum-alpha) solution is deliberate; the declaration comment carries the full
rationale, which is a property of the renderer, not of any one tool.
*/
use eframe::egui;
use egui::Color32;

/// Solves for the clean-overlay pixel that shows `final_color` when composited over
/// the original page pixel `base`, at an alpha of at least `coverage`.
///
/// `coverage` is the brush dab strength in `0..=1` (values outside are clamped); it is
/// the LOWER bound on the returned alpha, not the alpha itself.
///
/// # Why not the minimum alpha
/// For a target colour `f` over an opaque original `o`, every overlay alpha `a` in
/// `[alpha_min, 1]` reproduces `f` exactly, with `p(a) = o + (f - o) / a`: `p` moves
/// monotonically from the clamp boundary at `alpha_min` to `p(1) = f`, so it stays inside
/// `[0, 1]` for the whole range. `alpha_min` — the smallest representable alpha, still
/// computed below — is exact only under pixel-perfect 1:1 compositing. On the canvas the
/// page and the clean overlay are two SEPARATE textured quads, both sampled with
/// `TextureOptions::LINEAR` (`crates/ms-canvas/src/overlay_runtime.rs`), so colour and alpha are
/// filtered INDEPENDENTLY and their product term is lost: a minimum-alpha solution over
/// black text on white paper is a text-shaped opaque stencil on a transparent field, and
/// at glyph coverage `w` the screen shows `w + (1 - w)^2` instead of `1.0` — a ghost of
/// the letters peaking at 0.25 error. Lifting alpha to the dab coverage keeps the composite
/// mathematically exact and makes the committed overlay DENSE over the brushed area (like
/// `zamazka`'s) instead of sparse; at hardness 100 % `coverage == 1.0`, so the patch is
/// fully opaque and immune to independent resampling.
///
/// Returns `Color32::TRANSPARENT` when `final_color` is fully transparent, and when the
/// solved alpha is below one 8-bit step — which, since the alpha is at least `coverage`,
/// can only happen where the dab itself contributes nothing.
#[must_use]
pub fn overlay_pixel_for_final_color(base: Color32, final_color: Color32, coverage: f32) -> Color32 {
    let [br, bg, bb, _] = base.to_srgba_unmultiplied();
    let [fr, fg, fb, fa] = final_color.to_srgba_unmultiplied();
    if fa == 0 {
        return Color32::TRANSPARENT;
    }
    let b = [br as f32 / 255.0, bg as f32 / 255.0, bb as f32 / 255.0];
    let f = [fr as f32 / 255.0, fg as f32 / 255.0, fb as f32 / 255.0];
    let mut alpha: f32 = 0.0;
    for channel in 0..3 {
        let diff = (f[channel] - b[channel]).abs();
        if diff <= (1.0 / 255.0) {
            continue;
        }
        let needed = if f[channel] < b[channel] {
            diff / b[channel].max(f32::EPSILON)
        } else {
            diff / (1.0 - b[channel]).max(f32::EPSILON)
        };
        alpha = alpha.max(needed.clamp(0.0, 1.0));
    }
    // `alpha` is the representability floor; the dab coverage raises it, never lowers it.
    alpha = alpha.max(coverage.clamp(0.0, 1.0));
    if alpha <= (1.0 / 255.0) {
        return Color32::TRANSPARENT;
    }
    let out = [
        ((f[0] - b[0] * (1.0 - alpha)) / alpha).clamp(0.0, 1.0),
        ((f[1] - b[1] * (1.0 - alpha)) / alpha).clamp(0.0, 1.0),
        ((f[2] - b[2] * (1.0 - alpha)) / alpha).clamp(0.0, 1.0),
    ];
    Color32::from_rgba_unmultiplied(
        (out[0] * 255.0).round().clamp(0.0, 255.0) as u8,
        (out[1] * 255.0).round().clamp(0.0, 255.0) as u8,
        (out[2] * 255.0).round().clamp(0.0, 255.0) as u8,
        (alpha * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tolerance of one 8-bit step, expressed in the normalized `0..=1` range.
    const ONE_STEP: f32 = 1.0 / 255.0;

    /// Composites an un-premultiplied overlay pixel over an opaque base and returns the
    /// three normalized sRGB channels, i.e. what the renderer shows at 1:1 sampling.
    fn composite_over(overlay: Color32, base: Color32) -> [f32; 3] {
        let [orr, og, ob, oa] = overlay.to_srgba_unmultiplied();
        let [br, bg, bb, _] = base.to_srgba_unmultiplied();
        let a = f32::from(oa) / 255.0;
        [
            (f32::from(orr) / 255.0) * a + (f32::from(br) / 255.0) * (1.0 - a),
            (f32::from(og) / 255.0) * a + (f32::from(bg) / 255.0) * (1.0 - a),
            (f32::from(ob) / 255.0) * a + (f32::from(bb) / 255.0) * (1.0 - a),
        ]
    }

    /// Normalized sRGB channels of a colour, ignoring its alpha.
    fn channels(color: Color32) -> [f32; 3] {
        let [r, g, b, _] = color.to_srgba_unmultiplied();
        [
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
        ]
    }

    #[test]
    fn overlay_pixel_at_full_coverage_is_opaque_and_equals_final_color() {
        // The ghosting regression: over a white page the minimum-alpha solution was
        // fully transparent, so the page showed through the resampled overlay.
        let over_white = overlay_pixel_for_final_color(Color32::WHITE, Color32::WHITE, 1.0);
        assert_eq!(over_white, Color32::from_rgba_unmultiplied(255, 255, 255, 255));

        let over_black = overlay_pixel_for_final_color(Color32::BLACK, Color32::WHITE, 1.0);
        assert_eq!(over_black, Color32::from_rgba_unmultiplied(255, 255, 255, 255));

        // A non-neutral target must survive full coverage unchanged as well.
        let target = Color32::from_rgb(37, 180, 90);
        let solved = overlay_pixel_for_final_color(Color32::from_rgb(200, 40, 10), target, 1.0);
        assert_eq!(solved.a(), 255);
        assert_eq!(solved.to_srgba_unmultiplied(), target.to_srgba_unmultiplied());
    }

    #[test]
    fn overlay_pixel_at_partial_coverage_still_reproduces_final_color() {
        let cases = [
            (Color32::WHITE, Color32::from_rgb(128, 128, 128), 0.5_f32),
            (Color32::WHITE, Color32::from_rgb(200, 190, 210), 0.25_f32),
            (Color32::BLACK, Color32::from_rgb(64, 32, 96), 0.35_f32),
            // Coverage below the representability floor must not weaken the solution.
            (Color32::WHITE, Color32::BLACK, 0.2_f32),
        ];
        for (base, desired_final, coverage) in cases {
            let overlay = overlay_pixel_for_final_color(base, desired_final, coverage);
            let alpha = f32::from(overlay.a()) / 255.0;
            assert!(alpha + ONE_STEP >= coverage, "alpha {alpha} fell below coverage {coverage}");
            let shown = composite_over(overlay, base);
            let want = channels(desired_final);
            for channel in 0..3 {
                // One step for the alpha quantization plus one for the colour quantization.
                assert!((shown[channel] - want[channel]).abs() <= 2.0 * ONE_STEP, "channel {channel}: {shown:?} vs {want:?} (coverage {coverage})");
            }
        }
    }

    #[test]
    fn overlay_pixel_for_transparent_final_color_is_transparent() {
        assert_eq!(overlay_pixel_for_final_color(Color32::WHITE, Color32::TRANSPARENT, 1.0), Color32::TRANSPARENT);
    }

    #[test]
    fn overlay_pixel_without_coverage_or_difference_is_transparent() {
        // Nothing to paint and nothing to represent: the pixel stays untouched.
        assert_eq!(overlay_pixel_for_final_color(Color32::WHITE, Color32::WHITE, 0.0), Color32::TRANSPARENT);
    }
}
