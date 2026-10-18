"""
File: modules/ai_backend/inpaint/flux3_klein/test_packing.py

Purpose:
Unit tests for `flux3_klein/packing.py`: `pack` spans and dtypes, rope-id
scaling, the single-reference canvas (4 MP cap, 64..4096 sides, reflect vs
symmetric padding, crop box), the multi-reference 1 MP floor and 16-grid crop,
the reference selection rule, reference token ids (l axis), image <-> tensor
conversion (`to_pil` clamps, then rounds half to even), and the bf16-rounded
deterministic noise.

Notes:
Real torch, numpy and PIL on the CPU; skipped where torch or PIL is absent.
Images are built in memory; nothing touches disk.
"""

from __future__ import annotations

import importlib.util
import unittest

if importlib.util.find_spec("torch") is None or importlib.util.find_spec("PIL") is None:
    raise unittest.SkipTest("torch / PIL не установлены")

import numpy as np
import torch
from PIL import Image

from . import packing as pk

CHANNELS = {"image": 16, "image_cond": 16, "text": 48}


def _pair(count: int, width: int, dtype: torch.dtype = torch.bfloat16) -> tuple[torch.Tensor, torch.Tensor]:
    """`count` tokens of `width` channels and their (0, 0, 0, i) ids."""
    ids = torch.zeros(count, 4)
    ids[:, 3] = torch.arange(count)
    return torch.randn(count, width).to(dtype), ids


def _image(width: int, height: int) -> Image.Image:
    """A deterministic RGB gradient image."""
    rng = np.random.default_rng(width * 7919 + height)
    return Image.fromarray(rng.integers(0, 256, size=(height, width, 3), dtype=np.uint8))


class PackTests(unittest.TestCase):
    def test_two_samples_spans_and_empty_cond(self) -> None:
        text_a, text_b = _pair(5, 48), _pair(3, 48)
        cond_b = _pair(4, 16)
        image_a, image_b = _pair(6, 16, torch.float32), _pair(6, 16, torch.float32)
        packed = pk.pack([(text_a, None, image_a), (text_b, cond_b, image_b)], CHANNELS)
        self.assertEqual(packed.num_samples, 2)
        self.assertEqual(packed.mode_indices["text"], [(0, 5), (5, 8)])
        self.assertEqual(packed.mode_indices["image_cond"], [(0, 0), (0, 4)])
        self.assertEqual(packed.mode_indices["image"], [(0, 6), (6, 12)])
        # Image noise keeps its float32 dtype; ids are float32; timesteps bf16 zeros.
        self.assertEqual(packed.data["image"].dtype, torch.float32)
        self.assertEqual(packed.data["text"].dtype, torch.bfloat16)
        for mode in pk.MODES:
            self.assertEqual(packed.ids[mode].dtype, torch.float32)
            self.assertEqual(packed.timesteps[mode].dtype, torch.bfloat16)
            self.assertEqual(packed.timesteps[mode].shape[0], packed.data[mode].shape[0])
            self.assertFalse(packed.timesteps[mode].any())

    def test_mode_without_tokens_is_an_empty_bf16_tensor(self) -> None:
        packed = pk.pack([(_pair(2, 48), None, _pair(4, 16))], CHANNELS)
        self.assertEqual(tuple(packed.data["image_cond"].shape), (0, 16))
        self.assertEqual(packed.data["image_cond"].dtype, torch.bfloat16)
        self.assertEqual(tuple(packed.ids["image_cond"].shape), (0, 4))

    def test_shape_mismatches_are_refused(self) -> None:
        with self.assertRaises(ValueError):
            pk.pack([(_pair(2, 40), None, _pair(4, 16))], CHANNELS)
        bad_ids = (torch.zeros(4, 16), torch.zeros(3, 4))
        with self.assertRaises(ValueError):
            pk.pack([(_pair(2, 48), None, bad_ids)], CHANNELS)
        with self.assertRaises(ValueError):
            pk.pack([], CHANNELS)
        with self.assertRaises(ValueError):
            pk.pack([(_pair(2, 48), None, _pair(4, 16))], {"image": 16, "text": 48})


class IdsTests(unittest.TestCase):
    def test_scaled_ids_scales_h_w_only_and_copies(self) -> None:
        ids = torch.tensor([[0, 2, 3, 0], [0, 4, 6, 0]])
        scaled = pk.scaled_ids(ids, 0.5)
        self.assertEqual(scaled.dtype, torch.float32)
        self.assertEqual(scaled.tolist(), [[0, 1, 1.5, 0], [0, 2, 3, 0]])
        float_ids = ids.float()
        pk.scaled_ids(float_ids, 0.5)
        self.assertEqual(float_ids.tolist(), [[0, 2, 3, 0], [0, 4, 6, 0]])

    def test_reference_tokens_sets_the_l_axis_per_reference(self) -> None:
        first = (torch.randn(4, 16), torch.tensor([[0, r, c, 0] for r in range(2) for c in range(2)]))
        second = (torch.randn(2, 16), torch.tensor([[0, 0, c, 0] for c in range(2)]))
        joined = pk.reference_tokens([first, second], 0.5)
        self.assertIsNotNone(joined)
        tokens, ids = joined
        self.assertEqual(tokens.dtype, torch.bfloat16)
        self.assertEqual(tuple(tokens.shape), (6, 16))
        self.assertEqual(ids[:4, 3].tolist(), [-1.0] * 4)
        self.assertEqual(ids[4:, 3].tolist(), [-2.0] * 2)
        self.assertEqual(ids[3, 1:3].tolist(), [0.5, 0.5])

    def test_no_references_give_none(self) -> None:
        self.assertIsNone(pk.reference_tokens([], 0.5))


class CanvasTests(unittest.TestCase):
    def test_small_image_is_not_upscaled_and_canvas_rounds_up(self) -> None:
        self.assertEqual(pk.reference_canvas(100, 50), ((100, 50), (112, 64)))

    def test_on_grid_image_keeps_its_size(self) -> None:
        self.assertEqual(pk.reference_canvas(640, 480), ((640, 480), (640, 480)))

    def test_large_image_fits_the_4mp_cap_on_the_grid(self) -> None:
        for width, height in ((4000, 3000), (3001, 2999), (2048, 2049)):
            with self.subTest(size=(width, height)):
                fit, canvas = pk.reference_canvas(width, height)
                self.assertLessEqual(canvas[0] * canvas[1], pk.MATCH_MAX_PIXELS)
                self.assertEqual(canvas[0] % 16, 0)
                self.assertEqual(canvas[1] % 16, 0)
                self.assertLessEqual(fit[0], canvas[0])
                self.assertLessEqual(fit[1], canvas[1])

    def test_sides_are_clamped_to_64_and_4096(self) -> None:
        fit, canvas = pk.reference_canvas(8000, 100)
        self.assertEqual(fit, (4096, 51))
        self.assertEqual(canvas, (4096, 64))

    def test_non_positive_size_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            pk.reference_canvas(0, 10)

    def test_reflect_padding_and_crop_box(self) -> None:
        source = _image(40, 30)
        padded, box = pk.prepare_reference(source)
        self.assertEqual(padded.size, (64, 64))
        self.assertEqual(box, (12, 17, 52, 47))
        expected = np.pad(np.asarray(source), ((17, 17), (12, 12), (0, 0)), mode="reflect")
        self.assertTrue(np.array_equal(np.asarray(padded), expected))

    def test_tiny_image_is_padded_symmetrically(self) -> None:
        source = _image(20, 10)
        padded, box = pk.prepare_reference(source)
        self.assertEqual(padded.size, (64, 64))
        self.assertEqual(box, (22, 27, 42, 37))
        expected = np.pad(np.asarray(source), ((27, 27), (22, 22), (0, 0)), mode="symmetric")
        self.assertTrue(np.array_equal(np.asarray(padded), expected))

    def test_on_grid_image_has_no_box(self) -> None:
        padded, box = pk.prepare_reference(_image(128, 64))
        self.assertIsNone(box)
        self.assertEqual(padded.size, (128, 64))

    def test_non_rgb_input_is_converted(self) -> None:
        padded, _ = pk.prepare_reference(_image(64, 64).convert("L"))
        self.assertEqual(padded.mode, "RGB")


class MultiReferenceTests(unittest.TestCase):
    def test_large_reference_floors_under_1mp_and_crops_to_grid(self) -> None:
        prepared = pk.prepare_multi_reference(_image(2000, 1000))
        width, height = prepared.size
        self.assertLessEqual(width * height, pk.MULTI_REFERENCE_MAX_PIXELS)
        # sqrt(2**20 / 2e6) * (2000, 1000) floored = (1448, 724) -> grid (1440, 720).
        self.assertEqual(prepared.size, (1440, 720))

    def test_small_reference_is_centre_cropped(self) -> None:
        source = _image(100, 50)
        prepared = pk.prepare_multi_reference(source)
        self.assertEqual(prepared.size, (96, 48))
        self.assertTrue(np.array_equal(np.asarray(prepared), np.asarray(source)[1:49, 2:98]))

    def test_reference_below_the_grid_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            pk.prepare_multi_reference(_image(10, 100))


class ReferenceCapTests(unittest.TestCase):
    """`max_pixels` (`Flux3Options.reference_max_pixels`) lowers the reference pixel caps."""

    def test_none_is_the_reference_rule(self) -> None:
        for size in ((1188, 789), (3000, 2000), (40, 30)):
            with self.subTest(size=size):
                self.assertEqual(pk.reference_canvas(*size, max_pixels=None), pk.reference_canvas(*size))

    def test_single_reference_is_downscaled_onto_the_grid_under_the_cap(self) -> None:
        cap = 512 * 512
        for width, height in ((1188, 789), (755, 810), (4000, 300), (300, 4000)):
            with self.subTest(size=(width, height)):
                fit, canvas = pk.reference_canvas(width, height, max_pixels=cap)
                self.assertLessEqual(canvas[0] * canvas[1], cap)
                self.assertEqual((canvas[0] % 16, canvas[1] % 16), (0, 0))
                self.assertAlmostEqual(fit[0] / fit[1], width / height, delta=0.05 * width / height + 0.05)
        # The kitchen photo: 3750 reference tokens -> under 1024.
        _fit, canvas = pk.reference_canvas(1188, 789, max_pixels=cap)
        self.assertLessEqual((canvas[0] // 16) * (canvas[1] // 16), 1024)

    def test_a_cap_never_upscales(self) -> None:
        self.assertEqual(pk.reference_canvas(100, 50, max_pixels=10**7), pk.reference_canvas(100, 50))

    def test_prepared_single_reference_and_box_follow_the_cap(self) -> None:
        prepared, box = pk.prepare_reference(_image(300, 200), max_pixels=128 * 128)
        self.assertLessEqual(prepared.width * prepared.height, 128 * 128)
        self.assertEqual((prepared.width % 16, prepared.height % 16), (0, 0))
        if box is not None:
            left, top, right, bottom = box
            self.assertLessEqual((right, bottom), prepared.size)
            self.assertGreaterEqual(min(left, top), 0)

    def test_multi_references_follow_the_cap(self) -> None:
        prepared = pk.prepare_references([_image(1000, 500), _image(300, 300)], None, None, max_pixels=200 * 100)
        for image in prepared.images:
            self.assertLessEqual(image.width * image.height, 200 * 100)
            self.assertEqual((image.width % 16, image.height % 16), (0, 0))

    def test_a_cap_below_the_smallest_canvas_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            pk.reference_canvas(100, 100, max_pixels=pk.MIN_REFERENCE_PIXELS - 1)
        with self.assertRaises(ValueError):
            pk.prepare_multi_reference(_image(100, 100), max_pixels=100)


class SelectionTests(unittest.TestCase):
    def test_no_references(self) -> None:
        self.assertEqual(pk.prepare_references([], None, None), pk.PreparedReferences(images=(), crop_box=None))

    def test_single_reference_keeps_box_only_without_size(self) -> None:
        self.assertEqual(pk.prepare_references([_image(40, 30)], None, None).crop_box, (12, 17, 52, 47))
        self.assertIsNone(pk.prepare_references([_image(40, 30)], 64, None).crop_box)
        self.assertIsNone(pk.prepare_references([_image(40, 30)], None, 64).crop_box)

    def test_several_references_use_the_multi_rule(self) -> None:
        prepared = pk.prepare_references([_image(100, 50), _image(40, 30)], None, None)
        self.assertIsNone(prepared.crop_box)
        self.assertEqual([image.size for image in prepared.images], [(96, 48), (32, 16)])

    def test_default_size(self) -> None:
        self.assertEqual(pk.default_size([], (1024, 1024)), (1024, 1024))
        self.assertEqual(pk.default_size([_image(96, 48)], (1024, 1024)), (96, 48))


class ConversionTests(unittest.TestCase):
    def test_tensor_round_trip(self) -> None:
        source = _image(16, 8)
        tensor = pk.to_tensor(source)
        self.assertEqual(tuple(tensor.shape), (3, 8, 16))
        self.assertGreaterEqual(float(tensor.min()), -1.0)
        self.assertLessEqual(float(tensor.max()), 1.0)
        back = np.asarray(pk.to_pil(tensor))
        # to_pil rounds, so the float32 round trip is exact.
        self.assertTrue(np.array_equal(back, np.asarray(source)))

    def test_to_pil_clamps_then_rounds_half_to_even(self) -> None:
        # 0.0 -> 127.5 -> 128 (half to even; truncation would give 127);
        # 10.6 levels -> 11 (truncation: 10); out-of-range values clamp to 0 / 255.
        values = torch.tensor([0.0, 10.6 / 127.5 - 1.0, -3.0, 3.0], dtype=torch.float32)
        tensor = values.view(1, 1, 4).expand(3, 1, 4)
        out = np.asarray(pk.to_pil(tensor))
        self.assertEqual(out.dtype, np.uint8)
        self.assertEqual(out[0, :, 0].tolist(), [128, 11, 0, 255])

    def test_to_tensor_refuses_non_rgb(self) -> None:
        with self.assertRaises(ValueError):
            pk.to_tensor(_image(8, 8).convert("L"))


class NoiseTests(unittest.TestCase):
    def test_noise_is_deterministic_bf16_valued_float32(self) -> None:
        first = pk.initial_noise(3, 4, 1234, 16)
        self.assertEqual(tuple(first.shape), (12, 16))
        self.assertEqual(first.dtype, torch.float32)
        self.assertTrue(torch.equal(first, pk.initial_noise(3, 4, 1234, 16)))
        self.assertTrue(torch.equal(first, first.to(torch.bfloat16).float()))
        self.assertFalse(torch.equal(first, pk.initial_noise(3, 4, 1235, 16)))

    def test_noise_matches_the_reference_draw(self) -> None:
        expected = torch.randn((12, 16), generator=torch.Generator().manual_seed(7)).to(torch.bfloat16).float()
        self.assertTrue(torch.equal(pk.initial_noise(3, 4, 7, 16), expected))


if __name__ == "__main__":
    unittest.main()
