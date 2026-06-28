"""
File: modules/ai_backend/test_sdxl_inpaint_service.py

Purpose:
Unit tests for SDXL inpaint parameter normalization and sampler mapping.

Main responsibilities:
- verify mode/model_path/sampler validation raises clear errors;
- verify numeric clamping of steps/cfg/denoise/mask parameters;
- verify the four-channel denoise cap keeps the LaMa prefill meaningful;
- verify sampler names map to the expected diffusers scheduler config.

These tests cover the pure-Python contract only; they do not load torch,
diffusers, or any model weights.
"""

from __future__ import annotations

import unittest

from modules.ai_backend import sdxl_inpaint_service as svc


class NormalizeSdxlParamsTests(unittest.TestCase):
    def _base(self, **overrides: object) -> dict[str, object]:
        params: dict[str, object] = {
            "mode": "nine_channel",
            "model_path": "/models/sdxl-inpaint.safetensors",
            "sampler": "Euler",
        }
        params.update(overrides)
        return params

    def test_valid_params_pass_through(self) -> None:
        out = svc.normalize_sdxl_params(self._base(steps=40, cfg_scale=6.5))
        self.assertEqual(out["mode"], "nine_channel")
        self.assertEqual(out["model_path"], "/models/sdxl-inpaint.safetensors")
        self.assertEqual(out["steps"], 40)
        self.assertAlmostEqual(out["cfg_scale"], 6.5)

    def test_invalid_mode_raises(self) -> None:
        with self.assertRaises(ValueError):
            svc.normalize_sdxl_params(self._base(mode="bogus"))

    def test_empty_model_path_raises(self) -> None:
        with self.assertRaises(ValueError):
            svc.normalize_sdxl_params(self._base(model_path="   "))

    def test_invalid_sampler_raises(self) -> None:
        with self.assertRaises(ValueError):
            svc.normalize_sdxl_params(self._base(sampler="NopeSampler"))

    def test_numeric_clamping(self) -> None:
        out = svc.normalize_sdxl_params(
            self._base(
                steps=9999,
                cfg_scale=999.0,
                denoise_strength=5.0,
                mask_blur=-10,
                mask_dilation=999,
            )
        )
        self.assertEqual(out["steps"], 150)
        self.assertEqual(out["cfg_scale"], 30.0)
        self.assertEqual(out["denoise_strength"], 1.0)
        self.assertEqual(out["mask_blur"], 0)
        self.assertEqual(out["mask_dilation"], 64)

    def test_four_channel_denoise_capped_below_one(self) -> None:
        # Strength 1.0 on a 4-channel model would re-noise the hole to pure noise
        # and discard the LaMa prefill, so it must be capped below 1.0.
        out = svc.normalize_sdxl_params(
            self._base(mode="four_channel", denoise_strength=1.0)
        )
        self.assertLess(out["denoise_strength"], 1.0)

    def test_nine_channel_keeps_full_denoise(self) -> None:
        out = svc.normalize_sdxl_params(self._base(denoise_strength=1.0))
        self.assertEqual(out["denoise_strength"], 1.0)

    def test_seed_default_is_random_sentinel(self) -> None:
        out = svc.normalize_sdxl_params(self._base())
        self.assertEqual(out["seed"], -1)


class ResolveSchedulerConfigTests(unittest.TestCase):
    def test_known_sampler_returns_class_and_kwargs(self) -> None:
        class_name, kwargs = svc.resolve_scheduler_config("DPM++ 2M Karras")
        self.assertEqual(class_name, "DPMSolverMultistepScheduler")
        self.assertTrue(kwargs.get("use_karras_sigmas"))

    def test_returned_kwargs_are_isolated_copies(self) -> None:
        _, kwargs = svc.resolve_scheduler_config("DPM++ 2M")
        kwargs["mutated"] = True
        _, kwargs_again = svc.resolve_scheduler_config("DPM++ 2M")
        self.assertNotIn("mutated", kwargs_again)

    def test_unknown_sampler_raises(self) -> None:
        with self.assertRaises(ValueError):
            svc.resolve_scheduler_config("Unknown")

    def test_all_rust_samplers_are_supported(self) -> None:
        # Mirror of SDXL_SAMPLERS in src/tabs/cleaning/tools/sdxl.rs.
        rust_samplers = [
            "Euler",
            "Euler a",
            "DPM++ 2M",
            "DPM++ 2M Karras",
            "DPM++ SDE Karras",
            "DDIM",
            "UniPC",
            "Heun",
        ]
        for sampler in rust_samplers:
            with self.subTest(sampler=sampler):
                class_name, _ = svc.resolve_scheduler_config(sampler)
                self.assertTrue(class_name.endswith("Scheduler"))


try:
    import numpy as _np_for_tests
except Exception:
    _np_for_tests = None


@unittest.skipIf(_np_for_tests is None, "numpy is required")
class MatchVaeRoundtripTests(unittest.TestCase):
    def test_offset_compensates_uniform_darkening(self) -> None:
        np = _np_for_tests
        original = np.full((16, 16, 3), 200, dtype=np.uint8)
        # Generated is uniformly 20 darker everywhere (the VAE roundtrip shift).
        generated = np.full((16, 16, 3), 180.0, dtype=np.float32)
        # Mask the central 4x4 block; the rest is unmasked context.
        alpha = np.zeros((16, 16), dtype=np.float32)
        alpha[6:10, 6:10] = 1.0
        corrected = svc._match_vae_roundtrip(generated, original, alpha)
        # The masked patch should be lifted back toward the original brightness.
        self.assertAlmostEqual(float(corrected[7, 7, 0]), 200.0, delta=1.0)

    def test_skips_when_too_few_unmasked_pixels(self) -> None:
        np = _np_for_tests
        original = np.full((8, 8, 3), 200, dtype=np.uint8)
        generated = np.full((8, 8, 3), 180.0, dtype=np.float32)
        alpha = np.ones((8, 8), dtype=np.float32)  # everything masked
        corrected = svc._match_vae_roundtrip(generated, original, alpha)
        # No reliable context -> generated returned unchanged.
        self.assertTrue(np.allclose(corrected, generated))


if __name__ == "__main__":
    unittest.main()
