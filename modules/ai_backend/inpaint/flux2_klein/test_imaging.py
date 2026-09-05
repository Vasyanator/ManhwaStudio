"""
File: modules/ai_backend/inpaint/flux2_klein/test_imaging.py

Purpose:
Unit tests for `imaging.py`: the post-processing contract that every pixel
outside the mask comes back byte-identical, the color match against the ring
around the mask, and the feather that joins the two.

Main responsibilities:
- verify the composite leaves every pixel outside the mask untouched;
- verify the color match aligns the generated region to the ring around it and
  refuses to act on too few samples;
- verify the feather and the mask distance transform survive the absence of cv2.
"""

from __future__ import annotations

import builtins
import unittest
from unittest.mock import patch

import numpy as np

from modules.ai_backend.inpaint import flux2_klein as svc


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
        composed = svc._composite_over_region(self.original, self.generated, self.mask, 6)
        outside = self.mask == 0
        self.assertTrue(np.array_equal(composed[outside], self.original[outside]))

    def test_the_centre_of_the_mask_takes_the_generated_pixels(self) -> None:
        composed = svc._composite_over_region(self.original, self.generated, self.mask, 4)
        # The inward feather only fades the rim, so the middle is (nearly) all
        # generated content.
        self.assertGreater(int(composed[31, 31, 0]), 190)

    def test_a_zero_feather_is_a_hard_edge(self) -> None:
        composed = svc._composite_over_region(self.original, self.generated, self.mask, 0)
        inside = self.mask > 0
        self.assertTrue(np.array_equal(composed[inside], self.generated[inside]))

    def test_a_mask_thinner_than_the_feather_is_not_blended_away(self) -> None:
        thin = np.zeros((64, 64), dtype=np.uint8)
        thin[32, 20:44] = 255
        composed = svc._composite_over_region(self.original, self.generated, thin, 12)
        self.assertTrue(np.array_equal(composed[32, 30], self.generated[32, 30]))

    def test_the_feather_reaches_full_strength_at_exactly_feather_px(self) -> None:
        # `mask_feather_px` is the ramp WIDTH: zero weight on the contour, full
        # weight `feather_px` pixels in. The predecessor spread a nominal 6 over
        # ~22 px and never reached 1.0 at all for a wide feather.
        for feather in (4, 6, 12):
            with self.subTest(feather=feather):
                alpha = svc._feather_mask_inwards(self.mask, feather)
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
        distance = svc._mask_distance_inside(mask)
        self.assertEqual(float(distance[0, 0]), 1.0)
        self.assertEqual(float(distance[63, 0]), 1.0)
        self.assertGreater(float(distance[32, 8]), 1.0)
        alpha = svc._feather_mask_inwards(mask, 8)
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
                composed = svc._composite_over_region(
                    self.original, self.original.copy(), self.mask, feather
                )
                self.assertTrue(np.array_equal(composed, self.original))

    def test_the_feather_is_the_same_without_cv2(self) -> None:
        # OpenCV is optional for this backend, so the erosion fallback of
        # `_mask_distance_inside` must produce the same ramp, not a coarser one.
        with_cv2 = svc._feather_mask_inwards(self.mask, 6)
        real_import = builtins.__import__

        def without_cv2(name: str, *args: object, **kwargs: object) -> object:
            if name == "cv2":
                raise ImportError("cv2 is unavailable in this test")
            return real_import(name, *args, **kwargs)

        with patch.object(builtins, "__import__", without_cv2):
            fallback = svc._feather_mask_inwards(self.mask, 6)
        self.assertTrue(np.array_equal(with_cv2, fallback))

    def test_color_match_aligns_the_generated_region_to_the_ring(self) -> None:
        rng = np.random.default_rng(7)
        original = rng.integers(80, 160, size=(64, 64, 3), dtype=np.uint8)
        # The generated window came back uniformly brighter, as a VAE round trip
        # tends to leave it.
        generated = np.clip(original.astype(np.int16) + 30, 0, 255).astype(np.uint8)
        matched = svc._match_color_outside_mask(generated, original, self.mask)
        outside = self.mask == 0
        self.assertLess(
            abs(float(matched[outside].mean()) - float(original[outside].mean())), 1.0
        )

    def test_color_match_is_skipped_when_the_ring_is_too_small(self) -> None:
        full = np.full((64, 64), 255, dtype=np.uint8)
        matched = svc._match_color_outside_mask(self.generated, self.original, full)
        self.assertIs(matched, self.generated)

if __name__ == "__main__":
    unittest.main()
