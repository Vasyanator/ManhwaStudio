"""
File: modules/ai_backend/inpaint/test_region_imaging.py

Purpose:
Unit tests for `region_imaging.py`: the region editors' wire codecs.

Main responsibilities:
- verify the region decodes to RGB (an RGBA region's alpha is dropped);
- verify the mask is strictly L8, binarized to 0/255 and refused on a size
  mismatch;
- verify `require_solid_mask` accepts only a fully set mask;
- verify `encode_png_rgb` round-trips through `decode_image_rgb`.
"""

from __future__ import annotations

import io
import unittest

import numpy as np
from PIL import Image

from modules.ai_backend.inpaint import region_imaging as ri


def _png(arr: np.ndarray, mode: str) -> bytes:
    """Encode `arr` as a PNG of the given PIL mode."""
    with io.BytesIO() as buf:
        Image.fromarray(arr, mode).save(buf, format="PNG")
        return buf.getvalue()


class CodecTests(unittest.TestCase):
    def test_an_rgba_region_decodes_to_rgb_without_its_alpha(self) -> None:
        rgba = np.zeros((4, 5, 4), dtype=np.uint8)
        rgba[..., 0] = 10
        rgba[..., 1] = 20
        rgba[..., 2] = 30
        rgba[..., 3] = 255
        decoded = ri.decode_image_rgb(_png(rgba, "RGBA"))
        self.assertEqual(decoded.shape, (4, 5, 3))
        self.assertEqual(decoded.dtype, np.uint8)
        self.assertTrue(np.all(decoded == np.array([10, 20, 30], dtype=np.uint8)))

    def test_the_mask_is_binarized(self) -> None:
        mask = np.array([[0, 1, 128], [255, 0, 7]], dtype=np.uint8)
        decoded = ri.decode_mask(_png(mask, "L"), expected_hw=(2, 3))
        self.assertTrue(np.array_equal(decoded, np.array([[0, 255, 255], [255, 0, 255]], dtype=np.uint8)))

    def test_a_mask_that_is_not_l8_is_refused(self) -> None:
        rgb = np.full((2, 3, 3), 255, dtype=np.uint8)
        with self.assertRaises(ValueError):
            ri.decode_mask(_png(rgb, "RGB"), expected_hw=(2, 3))

    def test_a_mask_of_another_size_is_refused(self) -> None:
        mask = np.full((2, 3), 255, dtype=np.uint8)
        with self.assertRaises(ValueError):
            ri.decode_mask(_png(mask, "L"), expected_hw=(3, 2))

    def test_require_solid_mask_accepts_only_a_full_mask(self) -> None:
        ri.require_solid_mask(np.full((8, 8), 255, dtype=np.uint8))
        holed = np.full((8, 8), 255, dtype=np.uint8)
        holed[3, 4] = 0
        with self.assertRaises(ValueError):
            ri.require_solid_mask(holed)

    def test_encode_png_rgb_round_trips(self) -> None:
        rng = np.random.default_rng(7)
        image = rng.integers(0, 256, size=(6, 9, 3), dtype=np.uint8)
        png = ri.encode_png_rgb(image)
        with Image.open(io.BytesIO(png)) as img:
            self.assertEqual(img.mode, "RGB")
        self.assertTrue(np.array_equal(ri.decode_image_rgb(png), image))


if __name__ == "__main__":
    unittest.main()
