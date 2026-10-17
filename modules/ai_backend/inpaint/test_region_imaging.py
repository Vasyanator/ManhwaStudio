"""
File: modules/ai_backend/inpaint/test_region_imaging.py

Purpose:
Unit tests for `region_imaging.py`: the post-processing contract that every pixel
outside the mask comes back byte-identical, the color match against the ring
around the mask, and the feather that joins the two.

Main responsibilities:
- verify the composite leaves every pixel outside the mask untouched;
- verify the color match aligns the generated region to the ring around it,
  keeps `COLOR_MATCH_MARGIN_PX` clear of the contour, falls back to the bare
  mask for a thin ring and refuses to act on too few samples;
- verify the local CIELAB match pulls a centre drift towards the surroundings,
  is an identity at strength 0, leaves a fully masked region and every pixel
  outside the mask alone, and agrees with its cv2-free fallback;
- verify the feather and the mask distance transform survive the absence of cv2.
"""

from __future__ import annotations

import builtins
import unittest
from unittest.mock import patch

import numpy as np

from modules.ai_backend.inpaint import region_imaging as ri


def _without_cv2(name: str, *args: object, **kwargs: object) -> object:
    """`builtins.__import__` stand-in that makes `import cv2` fail (OpenCV is optional)."""
    if name == "cv2":
        raise ImportError("cv2 is unavailable in this test")
    return _REAL_IMPORT(name, *args, **kwargs)


_REAL_IMPORT = builtins.__import__


# ---------------------------------------------------------------------------
# Post-processing
# ---------------------------------------------------------------------------
class PostProcessingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.original = np.full((64, 64, 3), 100, dtype=np.uint8)
        self.generated = np.full((64, 64, 3), 200, dtype=np.uint8)
        self.mask = np.zeros((64, 64), dtype=np.uint8)
        self.mask[20:44, 20:44] = 255

    def test_pixels_outside_the_mask_are_byte_identical(self) -> None:
        composed = ri.composite_over_region(self.original, self.generated, self.mask, 6)
        outside = self.mask == 0
        self.assertTrue(np.array_equal(composed[outside], self.original[outside]))

    def test_the_centre_of_the_mask_takes_the_generated_pixels(self) -> None:
        composed = ri.composite_over_region(self.original, self.generated, self.mask, 4)
        # The inward feather only fades the rim, so the middle is (nearly) all
        # generated content.
        self.assertGreater(int(composed[31, 31, 0]), 190)

    def test_a_zero_feather_is_a_hard_edge(self) -> None:
        composed = ri.composite_over_region(self.original, self.generated, self.mask, 0)
        inside = self.mask > 0
        self.assertTrue(np.array_equal(composed[inside], self.generated[inside]))

    def test_a_mask_thinner_than_the_feather_is_not_blended_away(self) -> None:
        thin = np.zeros((64, 64), dtype=np.uint8)
        thin[32, 20:44] = 255
        composed = ri.composite_over_region(self.original, self.generated, thin, 12)
        self.assertTrue(np.array_equal(composed[32, 30], self.generated[32, 30]))

    def test_the_feather_reaches_full_strength_at_exactly_feather_px(self) -> None:
        # `mask_feather_px` is the ramp WIDTH: zero weight on the contour, full
        # weight `feather_px` pixels in. The predecessor spread a nominal 6 over
        # ~22 px and never reached 1.0 at all for a wide feather.
        for feather in (4, 6, 12):
            with self.subTest(feather=feather):
                alpha = ri.feather_mask_inwards(self.mask, feather)
                self.assertEqual(int(alpha[self.mask == 0].max()), 0)
                # Row 32 crosses the mask at columns 20..43; column 20+k is k+1 px
                # from the outside, so the ramp is complete at 20 + feather - 1.
                self.assertEqual(int(alpha[32, 20 + feather - 1]), 255)
                self.assertLess(int(alpha[32, 20]), 255)

    def test_the_region_border_is_a_contour_the_feather_ramps_from(self) -> None:
        # A mask painted up to the region edge used to meet the untouched page
        # with a hard step, because neither `distanceTransform` nor an erosion
        # sees a contour at the array boundary. The region is a window onto a
        # larger page, so its border is one.
        mask = np.zeros((64, 64), dtype=np.uint8)
        mask[:, :32] = 255  # touches the top, left and bottom borders
        distance = ri.mask_distance_inside(mask)
        self.assertEqual(float(distance[0, 0]), 1.0)
        self.assertEqual(float(distance[63, 0]), 1.0)
        self.assertGreater(float(distance[32, 8]), 1.0)
        alpha = ri.feather_mask_inwards(mask, 8)
        self.assertLess(int(alpha[0, 0]), 32)
        self.assertEqual(int(alpha[32, 8]), 255)
        # Still exactly zero outside the mask: the composite's core contract.
        self.assertEqual(int(alpha[32, 40]), 0)

    def test_blending_a_region_with_itself_is_an_exact_identity(self) -> None:
        # A truncating blend biased every partially blended pixel one level
        # darker, which is a mask-shaped dark patch with a hard contour — part of
        # the seam users reported. Rounding is what makes this hold.
        for feather in (0, 3, 6, 12, 32):
            with self.subTest(feather=feather):
                composed = ri.composite_over_region(
                    self.original, self.original.copy(), self.mask, feather
                )
                self.assertTrue(np.array_equal(composed, self.original))

    def test_the_feather_is_the_same_without_cv2(self) -> None:
        # OpenCV is optional for this backend, so the erosion fallback of
        # `mask_distance_inside` must produce the same ramp, not a coarser one.
        with_cv2 = ri.feather_mask_inwards(self.mask, 6)
        with patch.object(builtins, "__import__", _without_cv2):
            fallback = ri.feather_mask_inwards(self.mask, 6)
        self.assertTrue(np.array_equal(with_cv2, fallback))

    def test_color_match_aligns_the_generated_region_to_the_ring(self) -> None:
        rng = np.random.default_rng(7)
        original = rng.integers(80, 160, size=(64, 64, 3), dtype=np.uint8)
        # The generated window came back uniformly brighter, as a VAE round trip
        # tends to leave it.
        generated = np.clip(original.astype(np.int16) + 30, 0, 255).astype(np.uint8)
        matched = ri.match_color_outside_mask(generated, original, self.mask)
        outside = self.mask == 0
        self.assertLess(
            abs(float(matched[outside].mean()) - float(original[outside].mean())), 1.0
        )

    def test_color_match_is_skipped_when_the_ring_is_too_small(self) -> None:
        full = np.full((64, 64), 255, dtype=np.uint8)
        matched = ri.match_color_outside_mask(self.generated, self.original, full)
        self.assertIs(matched, self.generated)

    def test_color_match_keeps_the_margin_clear_of_a_text_outline(self) -> None:
        # The original keeps a bright outline just outside the mask that the
        # model painted over; the fit must ignore it, not chase it.
        original = np.full((96, 96, 3), 100, dtype=np.uint8)
        mask = np.zeros((96, 96), dtype=np.uint8)
        mask[32:64, 32:64] = 255
        outline = (ri.dilate_mask(mask, 3) > 0) & (mask == 0)
        original[outline] = 250
        generated = np.full((96, 96, 3), 120, dtype=np.uint8)
        matched = ri.match_color_outside_mask(generated, original, mask)
        # Every sampled pixel says "+20"; the outline would have pulled the
        # target mean up and rescaled by its spread.
        self.assertTrue(np.array_equal(matched[0:10, 0:10], original[0:10, 0:10]))
        self.assertGreater(ri.COLOR_MATCH_MARGIN_PX, 3)

    def test_color_match_falls_back_to_the_bare_mask_for_a_thin_ring(self) -> None:
        # A 4 px ring around a 56 px mask: the 6 px margin leaves nothing, the
        # ring itself (960 px) is still a meaningful sample.
        rng = np.random.default_rng(3)
        original = rng.integers(80, 160, size=(64, 64, 3), dtype=np.uint8)
        generated = np.clip(original.astype(np.int16) + 30, 0, 255).astype(np.uint8)
        mask = np.zeros((64, 64), dtype=np.uint8)
        mask[4:60, 4:60] = 255
        self.assertFalse((ri.dilate_mask(mask, ri.COLOR_MATCH_MARGIN_PX) == 0).any())
        matched = ri.match_color_outside_mask(generated, original, mask)
        outside = mask == 0
        self.assertIsNot(matched, generated)
        self.assertLess(abs(float(matched[outside].mean()) - float(original[outside].mean())), 1.0)


# ---------------------------------------------------------------------------
# Local CIELAB color match
# ---------------------------------------------------------------------------
def _drift_scene(size: int) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """A smooth noisy gradient, a centred square mask, and a generated region
    whose warm offset grows from zero at the mask contour to its maximum at the
    centre — the failure mode a global match cannot fix."""
    yy, xx = np.mgrid[0:size, 0:size].astype(np.float32) / size
    base = np.stack((60 + 80 * xx, 90 + 60 * yy, 70 + 40 * (xx + yy)), axis=2)
    rng = np.random.default_rng(1)
    original = np.clip(base + rng.normal(0.0, 6.0, base.shape), 0, 255).astype(np.uint8)
    mask = np.zeros((size, size), dtype=np.uint8)
    quarter = size // 4
    mask[quarter : size - quarter, quarter : size - quarter] = 255
    depth = ri.mask_distance_inside(mask)
    warm = (depth / depth.max())[..., None] * np.asarray([18.0, 4.0, -6.0], dtype=np.float32)
    generated = np.clip(original.astype(np.float32) + warm, 0, 255).astype(np.uint8)
    return original, generated, mask


class LocalColorMatchTests(unittest.TestCase):
    def setUp(self) -> None:
        self.original, self.generated, self.mask = _drift_scene(256)
        centre = np.zeros(self.mask.shape, dtype=bool)
        centre[112:144, 112:144] = True
        self.centre = centre

    def _centre_drift(self, image: np.ndarray) -> np.ndarray:
        lab = ri._srgb_u8_to_lab(image)
        source = ri._srgb_u8_to_lab(self.original)
        return lab[self.centre].mean(axis=0) - source[self.centre].mean(axis=0)

    def test_it_pulls_a_centre_drift_towards_the_surroundings(self) -> None:
        before = np.abs(self._centre_drift(self.generated))
        partial = ri.match_color_local(self.original, self.generated, self.mask, strength=0.6, scale_px=8)
        full = ri.match_color_local(self.original, self.generated, self.mask, strength=1.0, scale_px=8)
        after_partial = np.abs(self._centre_drift(partial))
        after_full = np.abs(self._centre_drift(full))
        # a* and b* carry most of the warm offset; each must shrink clearly, and
        # full strength must get closer than 60 %.
        for channel in (1, 2):
            with self.subTest(channel=channel):
                self.assertLess(after_partial[channel], 0.7 * before[channel])
                self.assertLess(after_full[channel], after_partial[channel])
                self.assertLess(after_full[channel], 0.35 * before[channel])

    def test_it_changes_only_pixels_inside_the_mask(self) -> None:
        out = ri.match_color_local(self.original, self.generated, self.mask, strength=0.6, scale_px=24)
        self.assertEqual(out.dtype, np.uint8)
        self.assertEqual(out.shape, self.generated.shape)
        outside = self.mask == 0
        self.assertTrue(np.array_equal(out[outside], self.generated[outside]))
        self.assertFalse(np.array_equal(out[~outside], self.generated[~outside]))

    def test_the_composed_result_is_byte_identical_outside_the_mask(self) -> None:
        corrected = ri.match_color_outside_mask(self.generated, self.original, self.mask)
        local = ri.match_color_local(self.original, corrected, self.mask, strength=0.6, scale_px=24)
        composed = ri.composite_over_region(self.original, local, self.mask, 12)
        outside = self.mask == 0
        self.assertTrue(np.array_equal(composed[outside], self.original[outside]))

    def test_strength_zero_is_the_identity(self) -> None:
        for strength in (0.0, -0.5):
            with self.subTest(strength=strength):
                out = ri.match_color_local(self.original, self.generated, self.mask, strength=strength, scale_px=24)
                self.assertIs(out, self.generated)

    def test_a_fully_masked_region_is_left_unchanged(self) -> None:
        full = np.full(self.mask.shape, 255, dtype=np.uint8)
        with self.assertLogs(ri.log, level="DEBUG") as captured:
            out = ri.match_color_local(self.original, self.generated, full, strength=0.6, scale_px=24)
        self.assertIs(out, self.generated)
        self.assertTrue(any("skipping the local color match" in line for line in captured.output))

    def test_an_empty_mask_is_left_unchanged(self) -> None:
        empty = np.zeros(self.mask.shape, dtype=np.uint8)
        out = ri.match_color_local(self.original, self.generated, empty, strength=0.6, scale_px=24)
        self.assertIs(out, self.generated)

    def test_a_tiny_region_and_flat_channels_stay_finite(self) -> None:
        # 24x24 with the widest scale: every grid collapses to a cell or two.
        original = np.zeros((24, 24, 3), dtype=np.uint8)
        original[..., 0] = 120  # flat channels everywhere: std floors, no NaN
        generated = original.copy()
        generated[8:16, 8:16] = (140, 10, 10)
        mask = np.zeros((24, 24), dtype=np.uint8)
        mask[8:16, 8:16] = 255
        for scale in (8, 96):
            with self.subTest(scale=scale):
                out = ri.match_color_local(original, generated, mask, strength=1.0, scale_px=scale)
                self.assertEqual(out.dtype, np.uint8)
                # The flat surroundings are the target: the patch moves towards
                # them (only partly — its own local mean is diluted by the ring).
                before = np.abs(generated[12, 12].astype(int) - original[12, 12].astype(int)).sum()
                after = np.abs(out[12, 12].astype(int) - original[12, 12].astype(int)).sum()
                self.assertLess(after, before)
                self.assertTrue(np.array_equal(out[mask == 0], generated[mask == 0]))

    def test_a_matching_region_without_structure_is_left_alone(self) -> None:
        # Generated == original over plain noise: target and local statistics
        # agree, nothing to move. (Over a GRADIENT the match does move pixels:
        # the target inside the mask is a fill from the ring, not the true
        # content, which is why the strength stays below 1 by default.)
        rng = np.random.default_rng(2)
        noise = np.clip(100.0 + rng.normal(0.0, 6.0, size=(256, 256, 3)), 0, 255).astype(np.uint8)
        out = ri.match_color_local(noise, noise.copy(), self.mask, strength=1.0, scale_px=24)
        self.assertLessEqual(int(np.abs(out.astype(int) - noise.astype(int)).max()), 1)

    def test_the_cv2_free_fallback_agrees(self) -> None:
        for scale in (8, 24, 96):
            with self.subTest(scale=scale):
                with_cv2 = ri.match_color_local(self.original, self.generated, self.mask, strength=0.6, scale_px=scale)
                with patch.object(builtins, "__import__", _without_cv2):
                    fallback = ri.match_color_local(self.original, self.generated, self.mask, strength=0.6, scale_px=scale)
                difference = np.abs(with_cv2.astype(int) - fallback.astype(int))
                self.assertLessEqual(int(difference.max()), 1)
                self.assertLess(float(difference.mean()), 0.05)

    def test_far_from_the_ring_the_target_converges_to_the_global_one(self) -> None:
        # cv2 truncates each Gaussian at 4 sigma, so beyond the coarsest
        # level's reach every level has zero weight; the cascade must then
        # fall back to the global target smoothly, not to "no correction" with
        # a step (a seam inside the mask) and a 0.8 contrast cut. One level
        # instead of five brings that reach (~40 px at scale 8) inside a small,
        # fast region; the real cascade hits it ~67·s from the ring.
        rng = np.random.default_rng(0)
        size, ring = 320, 16
        mask = np.zeros((size, size), dtype=np.uint8)
        mask[ring:-ring, ring:-ring] = 255
        # Bins of 16x48 px keep the noise of a bin mean near 0.2 L.
        column = slice(136, 184)
        bins = range(ring, size // 2, 16)
        cases = {"darker": (140, 110), "matching": (120, 120)}
        with patch.object(ri, "LOCAL_MATCH_LEVELS", 1):
            for name, (source_level, generated_level) in cases.items():
                with self.subTest(case=name):
                    original = np.clip(rng.normal(source_level, 20, (size, size, 3)), 0, 255).astype(np.uint8)
                    generated = np.clip(rng.normal(generated_level, 20, (size, size, 3)), 0, 255).astype(np.uint8)
                    out = ri.match_color_local(original, generated, mask, strength=1.0, scale_px=8)
                    lightness = ri._srgb_u8_to_lab(out)[..., 0]
                    target = ri._srgb_u8_to_lab(original)[..., 0]
                    profile = [float(lightness[row : row + 16, column].mean()) for row in bins]
                    steps = np.abs(np.diff(profile))
                    self.assertLess(float(steps.max()), 1.5, profile)
                    self.assertLess(abs(profile[-1] - float(target.mean())), 1.0)
                    centre = lightness[140:180, 140:180]
                    produced = ri._srgb_u8_to_lab(generated)[..., 0][140:180, 140:180]
                    self.assertGreater(float(centre.std()), 0.95 * float(produced.std()))

    def test_the_rim_is_corrected_as_fully_as_the_centre(self) -> None:
        # The corrected image's local statistics come from INSIDE the mask only:
        # a plain Gaussian mixed the drifted rim with the matching outside, saw
        # half the drift there and left a band along the contour (residual ΔL
        # 2.6 at the contour, 1.1 at 20 px for scale 24, 0 at the centre).
        rng = np.random.default_rng(0)
        size, low, high = 320, 80, 240
        original = np.clip(60 + rng.normal(0, 4, (size, size, 3)), 0, 255).astype(np.uint8)
        mask = np.zeros((size, size), dtype=np.uint8)
        mask[low:high, low:high] = 255
        generated = original.astype(np.int16)
        generated[mask > 0] += 15
        generated = np.clip(generated, 0, 255).astype(np.uint8)
        source = ri._srgb_u8_to_lab(original)[..., 0]
        columns = slice(120, 200)
        for scale in (8, 24):
            with self.subTest(scale=scale):
                out = ri._srgb_u8_to_lab(ri.match_color_local(original, generated, mask, strength=1.0, scale_px=scale))[..., 0]

                def residual(depth: int) -> float:
                    row = low + depth
                    return float(out[row, columns].mean() - source[row, columns].mean())

                centre = residual((high - low) // 2)
                for depth in (0, 2, 5, 10, 20):
                    self.assertLessEqual(abs(residual(depth)), 0.5, depth)
                    self.assertLessEqual(abs(residual(depth) - centre), 0.5, depth)

    def test_a_clean_fill_on_line_art_is_not_dragged_towards_the_ink(self) -> None:
        # Flat fill + 6 px ink strokes every 40 px + white page: a plain mean of
        # the ring is a darker, greyer fill and dragged the model's clean fill
        # into a grey patch the shape of the mask; the robust estimate keeps the
        # fill as the local colour.
        orange = np.asarray([214, 150, 95], dtype=np.uint8)
        original = np.tile(orange, (512, 512, 1))
        for row in range(0, 512, 40):
            original[row : row + 6] = (25, 15, 10)
        original[:60] = 255
        mask = np.zeros((512, 512), dtype=np.uint8)
        mask[150:400, 120:400] = 255
        generated = original.copy()
        generated[mask > 0] = orange
        before = ri._srgb_u8_to_lab(generated)
        for scale in (8, 24):
            for strength in (0.6, 1.0):
                with self.subTest(scale=scale, strength=strength):
                    out = ri._srgb_u8_to_lab(ri.match_color_local(original, generated, mask, strength=strength, scale_px=scale))
                    delta_e = np.sqrt(((out - before) ** 2).sum(axis=-1))[mask > 0]
                    self.assertLess(float(delta_e.mean()), 1.0)

    def test_each_rim_converges_to_its_own_surface(self) -> None:
        # Dark-green grass on the left, red-brown ground on the right, the
        # boundary through the mask, a small warm drift inside it. Pinned: the
        # rim next to each surface is pulled to THAT surface (strength 0.6
        # leaves 40 % of the ~3 ΔE drift). Deliberately NOT pinned: the interior
        # next to the surface boundary, where the coarse target blends both
        # surfaces (a documented limit, see `match_color_local`).
        rng = np.random.default_rng(6)
        size = 320
        base = np.empty((size, size, 3), dtype=np.float32)
        base[:, :160] = (40, 70, 35)
        base[:, 160:] = (150, 80, 55)
        base[:, :160] += rng.normal(0, 7, (size, 160, 3))
        base[:, 160:] += rng.normal(0, 4, (size, 160, 3))
        original = np.clip(base, 0, 255).astype(np.uint8)
        mask = np.zeros((size, size), dtype=np.uint8)
        mask[80:240, 80:240] = 255
        drift = np.asarray([6, 2, -2], dtype=np.int16) * (mask[..., None] > 0)
        generated = np.clip(original.astype(np.int16) + drift, 0, 255).astype(np.uint8)
        source = ri._srgb_u8_to_lab(original)
        rows = slice(120, 200)

        def rim_delta_e(image_lab: np.ndarray, columns: slice) -> float:
            return float(np.linalg.norm((image_lab[rows, columns] - source[rows, columns]).reshape(-1, 3).mean(axis=0)))

        rims = {"grass": slice(81, 90), "ground": slice(230, 239)}
        for scale in (8, 24):
            for strength, limit in ((0.6, 1.8), (1.0, 0.75)):
                out = ri._srgb_u8_to_lab(ri.match_color_local(original, generated, mask, strength=strength, scale_px=scale))
                for name, columns in rims.items():
                    with self.subTest(scale=scale, strength=strength, rim=name):
                        self.assertLess(rim_delta_e(out, columns), limit)

    def test_a_non_finite_strength_is_refused(self) -> None:
        for strength in (float("nan"), float("inf")):
            with self.subTest(strength=strength), self.assertRaises(ValueError):
                ri.match_color_local(self.original, self.generated, self.mask, strength=strength, scale_px=24)

    def test_a_mask_on_the_region_border_matches_a_full_resolution_run(self) -> None:
        # The coarse grid used to mirror about its first cell, not the first
        # pixel: up to 9 levels off along a border the mask touches at s = 96.
        rng = np.random.default_rng(4)
        yy, xx = np.mgrid[0:200, 0:260].astype(np.float32)
        base = np.stack((80 + 60 * np.sin(xx / 90) + 0.05 * yy, 100 + 50 * np.cos(yy / 70), 60 + 0.08 * xx), axis=-1)
        original = np.clip(base + rng.normal(0, 8, base.shape), 0, 255).astype(np.uint8)
        generated = np.clip(original.astype(int) - 15, 0, 255).astype(np.uint8)
        mask = np.zeros((200, 260), dtype=np.uint8)
        mask[40:160, 0:120] = 255
        on_grid = ri.match_color_local(original, generated, mask, strength=1.0, scale_px=96)
        with patch.object(ri, "LOCAL_MATCH_GRID_SIGMA", 1e9):  # no grid: full resolution
            direct = ri.match_color_local(original, generated, mask, strength=1.0, scale_px=96)
        self.assertLessEqual(int(np.abs(on_grid.astype(int) - direct.astype(int)).max()), 3)

    def test_bad_inputs_are_refused(self) -> None:
        with self.assertRaises(ValueError):
            ri.match_color_local(self.original, self.generated[:-1], self.mask, strength=0.6, scale_px=24)
        with self.assertRaises(ValueError):
            ri.match_color_local(self.original, self.generated, self.mask[:-1], strength=0.6, scale_px=24)
        with self.assertRaises(ValueError):
            ri.match_color_local(self.original, self.generated, self.mask, strength=0.6, scale_px=0)

    def test_lab_conversion_matches_reference_values_and_round_trips(self) -> None:
        pixels = np.asarray([[[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255], [0, 0, 0]]], dtype=np.uint8)
        expected = np.asarray([[53.24, 80.09, 67.20], [87.73, -86.18, 83.18], [32.30, 79.19, -107.86], [100.0, 0.0, 0.0], [0.0, 0.0, 0.0]])
        np.testing.assert_allclose(ri._srgb_u8_to_lab(pixels)[0], expected, atol=0.02)
        rng = np.random.default_rng(0)
        colours = rng.integers(0, 256, size=(128, 128, 3), dtype=np.uint8)
        self.assertTrue(np.array_equal(ri._lab_to_srgb_u8(ri._srgb_u8_to_lab(colours)), colours))

    def test_the_low_pass_matches_a_full_resolution_gaussian(self) -> None:
        # `_smooth` blurs a large sigma on a coarser grid; the fields are smooth,
        # so the shortcut must stay close to the direct blur.
        # Away from the border (where the coarse grid reflects about its first
        # cell rather than the first pixel) the two must agree closely.
        rng = np.random.default_rng(5)
        field = ri._gaussian_blur(rng.normal(0.0, 100.0, size=(200, 184, 2)).astype(np.float32), 6.0)
        for sigma in (8.0, 20.0):
            with self.subTest(sigma=sigma):
                direct = ri._gaussian_blur(field, sigma)
                shortcut = ri._smooth(field, sigma)
                edge = int(2 * sigma)
                error = np.abs(direct - shortcut)[edge:-edge, edge:-edge]
                self.assertLess(float(error.max()), 0.05 * float(direct.std()))

if __name__ == "__main__":
    unittest.main()
