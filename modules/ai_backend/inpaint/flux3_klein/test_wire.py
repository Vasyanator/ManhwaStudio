"""
File: modules/ai_backend/inpaint/flux3_klein/test_wire.py

Purpose:
Unit tests of `wire.py`: every `params` key with its default, strict types and
ranges, unknown keys and bad enums refused by name, `whole_region` settling
`color_match`, and the region frame rule (multiple of 16, side >= 128, 4 MP,
aspect 4:1) with the area cap equal to `packing.MATCH_MAX_PIXELS`.

Notes:
Torch is imported only by the area-cap equality check (through `packing`).
"""

from __future__ import annotations

import unittest
from pathlib import Path

from ...runtime.large_models.tiers import RamStep, VramStep
from . import wire
from .config import WeightFormat
from .params import MAX_SEED, AeDtype, EncoderMode, Fp8Compute


class NormalizeTests(unittest.TestCase):
    ROOT = "/models/flux3"

    def _normalize(self, require_prompt: bool = False, **params: object) -> wire.Flux3WireParams:
        return wire.normalize_flux3_params({"source_root": self.ROOT, **params}, require_prompt=require_prompt)

    def test_the_defaults(self) -> None:
        normalized = self._normalize()
        options = normalized.options
        self.assertEqual(options.model_root, Path(self.ROOT))
        self.assertEqual((options.variant, options.encoder_mode, options.fp8_compute, options.ae_dtype), (WeightFormat.FP8R, EncoderMode.TEXT, Fp8Compute.AUTO, AeDtype.FLOAT32))
        self.assertEqual((options.vram_step, options.ram_step), (VramStep.AUTO, RamStep.AUTO))
        self.assertIsNone(options.reference_max_pixels)
        self.assertIsNone(options.device)
        self.assertTrue(options.encoder_on_cpu)
        self.assertEqual((normalized.prompt, normalized.steps, normalized.seed), ("", None, None))
        self.assertEqual((normalized.mask_feather_px, normalized.color_match, normalized.whole_region), (12, True, False))

    def test_every_key(self) -> None:
        normalized = self._normalize(
            True,
            format="nvfp4",
            encoder_mode="vl_refs",
            ae_dtype="bfloat16",
            fp8_compute="dequant",
            reference_max_pixels=512 * 512,
            prompt="  remove the text  ",
            steps=20,
            seed=MAX_SEED,
            vram_step="half",
            ram_step="min",
            encoder_on_cpu=False,
            mask_feather_px=0,
            color_match=False,
            whole_region=False,
        )
        options = normalized.options
        self.assertEqual((options.variant, options.encoder_mode, options.ae_dtype, options.fp8_compute), (WeightFormat.NVFP4, EncoderMode.VL_REFS, AeDtype.BFLOAT16, Fp8Compute.DEQUANT))
        self.assertEqual((options.reference_max_pixels, options.vram_step, options.ram_step, options.encoder_on_cpu), (512 * 512, VramStep.HALF, RamStep.MIN, False))
        self.assertEqual((normalized.prompt, normalized.steps, normalized.seed, normalized.mask_feather_px, normalized.color_match), ("remove the text", 20, MAX_SEED, 0, False))
        self.assertEqual(wire.FIELDS, {"source_root", "format", "encoder_mode", "ae_dtype", "fp8_compute", "reference_max_pixels", "prompt", "steps", "seed", "vram_step", "ram_step", "encoder_on_cpu", "mask_feather_px", "color_match", "whole_region"})

    def test_whole_region_turns_the_color_match_off(self) -> None:
        normalized = self._normalize(whole_region=True, color_match=True)
        self.assertTrue(normalized.whole_region)
        self.assertFalse(normalized.color_match)

    def test_nulls_read_as_absent(self) -> None:
        normalized = self._normalize(format=None, seed=None, steps=None, encoder_on_cpu=None, prompt=None)
        self.assertEqual((normalized.options.variant, normalized.seed, normalized.steps, normalized.options.encoder_on_cpu, normalized.prompt), (WeightFormat.FP8R, None, None, True, ""))

    def test_refusals_name_the_field(self) -> None:
        cases = {
            "bogus": {"bogus": 1},
            "format": {"format": "fp16"},
            "encoder_mode": {"encoder_mode": "vl"},
            "ae_dtype": {"ae_dtype": 32},
            "fp8_compute": {"fp8_compute": "fast"},
            "vram_step": {"vram_step": "huge"},
            "ram_step": {"ram_step": True},
            "steps": {"steps": 0},
            "seed": {"seed": -1},
            "mask_feather_px": {"mask_feather_px": 33},
            "reference_max_pixels": {"reference_max_pixels": 100},
            "encoder_on_cpu": {"encoder_on_cpu": "yes"},
            "color_match": {"color_match": 1},
            "whole_region": {"whole_region": "true"},
            "prompt": {"prompt": 5},
        }
        for field, params in cases.items():
            with self.subTest(field=field):
                with self.assertRaises(ValueError) as caught:
                    self._normalize(**params)
                self.assertIn(field, str(caught.exception))
        for steps in (True, 2.0, 65):
            with self.subTest(steps=steps), self.assertRaises(ValueError):
                self._normalize(steps=steps)

    def test_the_root_and_the_prompt(self) -> None:
        for params in ({}, {"source_root": ""}, {"source_root": "  "}, {"source_root": 5}):
            with self.subTest(params=params), self.assertRaises(ValueError):
                wire.normalize_flux3_params(params, require_prompt=False)
        with self.assertRaises(ValueError):
            wire.normalize_flux3_params(None, require_prompt=False)
        with self.assertRaises(ValueError):
            self._normalize(True, prompt="   ")
        self.assertEqual(self._normalize(True, prompt="x").prompt, "x")
        with self.assertRaises(ValueError):
            wire.normalize_flux3_params(["source_root"], require_prompt=False)  # type: ignore[arg-type]


class RegionSizeTests(unittest.TestCase):
    def test_accepted(self) -> None:
        for width, height in ((128, 128), (512, 512), (2048, 2048), (4096, 1024), (1024, 4096), (128, 512)):
            with self.subTest(size=(width, height)):
                wire.validate_region_size(width, height)

    def test_refused(self) -> None:
        for width, height in ((120, 128), (128, 112), (130, 128), (2064, 2048), (4096, 1040), (128, 528), (4112, 1024)):
            with self.subTest(size=(width, height)), self.assertRaises(ValueError):
                wire.validate_region_size(width, height)

    def test_the_area_cap_is_the_single_reference_canvas_cap(self) -> None:
        from . import packing

        self.assertEqual(wire.REGION_MAX_PIXELS, packing.MATCH_MAX_PIXELS)
        self.assertEqual(wire.MAX_EXTRA_REFERENCES, 9)


if __name__ == "__main__":
    unittest.main()
