"""
File: modules/ai_backend/inpaint/flux2_klein/test_params.py

Purpose:
Unit tests for `params.py`: the wire contract of `normalize_flux2_klein_params`
and the region contract of `validate_region_size`.

Main responsibilities:
- verify every enum/path error and every numeric clamp of the wire contract,
  including the placement-dependent default of `unload_transformer_before_vae`;
- verify a region that the pipeline would silently resize or crop is refused
  with concrete numbers instead;
- verify `effective_steps` matches what the pipeline will actually run.
"""

from __future__ import annotations

import unittest

from modules.ai_backend.inpaint import flux2_klein as svc

from ._test_fixtures import _TempTreeCase


# ---------------------------------------------------------------------------
# Parameter normalization
# ---------------------------------------------------------------------------
class NormalizeParamsTests(_TempTreeCase):
    def test_missing_path_raises(self) -> None:
        params = self.params()
        del params["vae_path"]
        with self.assertRaises(ValueError) as caught:
            svc.normalize_flux2_klein_params(params)
        self.assertIn("vae_path", str(caught.exception))

    def test_absent_path_raises(self) -> None:
        with self.assertRaises(ValueError) as caught:
            svc.normalize_flux2_klein_params(self.params(vae_path=str(self.root / "nope")))
        self.assertIn("не найден", str(caught.exception))

    def test_unknown_placement_raises(self) -> None:
        with self.assertRaises(ValueError):
            svc.normalize_flux2_klein_params(self.params(placement="teleport"))

    def test_unknown_dtype_raises(self) -> None:
        with self.assertRaises(ValueError):
            svc.normalize_flux2_klein_params(self.params(dtype="float64"))

    def test_defaults(self) -> None:
        out = svc.normalize_flux2_klein_params(self.params())
        self.assertEqual(out["steps"], 4)
        self.assertAlmostEqual(out["guidance_scale"], 1.0)
        self.assertAlmostEqual(out["strength"], 1.0)
        self.assertEqual(out["placement"], "full_gpu")
        self.assertEqual(out["dtype"], "bfloat16")
        self.assertEqual(out["mask_dilate_px"], 16)
        self.assertEqual(out["mask_feather_px"], 12)
        self.assertEqual(out["max_sequence_length"], 512)
        self.assertTrue(out["color_match"])
        self.assertFalse(out["whole_region"])
        self.assertFalse(out["unload_text_encoder_after_encode"])
        self.assertFalse(out["text_encoder_fp8"])
        self.assertIsNone(out["seed"])

    def test_every_key_of_the_wire_contract_is_present(self) -> None:
        # `normalize_flux2_klein_params` is what the rest of the module reads, so
        # a key it forgets is an unguarded `KeyError` deep inside a run rather
        # than a request error at the boundary.
        self.assertEqual(
            set(svc.normalize_flux2_klein_params(self.params())),
            {
                "text_encoder_path",
                "transformer_path",
                "vae_path",
                "prompt",
                "steps",
                "guidance_scale",
                "strength",
                "seed",
                "placement",
                "dtype",
                "low_cpu_mem_usage",
                "vae_tiling",
                "vae_slicing",
                "unload_transformer_before_vae",
                "unload_text_encoder_after_encode",
                "text_encoder_fp8",
                "mask_dilate_px",
                "mask_feather_px",
                "color_match",
                "whole_region",
                "max_sequence_length",
            },
        )

    def test_numeric_clamping(self) -> None:
        out = svc.normalize_flux2_klein_params(
            self.params(
                steps=9999,
                guidance_scale=-3.0,
                strength=0.0,
                mask_dilate_px=999,
                mask_feather_px=-4,
                max_sequence_length=4096,
            )
        )
        self.assertEqual(out["steps"], 50)
        self.assertAlmostEqual(out["guidance_scale"], 1.0)
        self.assertAlmostEqual(out["strength"], 0.25)
        self.assertEqual(out["mask_dilate_px"], 64)
        self.assertEqual(out["mask_feather_px"], 0)
        self.assertEqual(out["max_sequence_length"], 512)

    def test_unload_before_vae_default_depends_on_placement(self) -> None:
        # The VAE peak lands on top of the resident transformer everywhere except
        # `full_gpu`, whose whole point is that nothing leaves the GPU.
        self.assertFalse(
            svc.normalize_flux2_klein_params(self.params(placement="full_gpu"))[
                "unload_transformer_before_vae"
            ]
        )
        for placement in ("encoder_cpu", "model_cpu_offload", "sequential_cpu_offload"):
            with self.subTest(placement=placement):
                self.assertTrue(
                    svc.normalize_flux2_klein_params(self.params(placement=placement))[
                        "unload_transformer_before_vae"
                    ]
                )

    def test_explicit_unload_flag_wins_over_the_default(self) -> None:
        out = svc.normalize_flux2_klein_params(
            self.params(placement="full_gpu", unload_transformer_before_vae=True)
        )
        self.assertTrue(out["unload_transformer_before_vae"])

    def test_seed_is_kept_and_null_means_random(self) -> None:
        self.assertEqual(svc.normalize_flux2_klein_params(self.params(seed=7))["seed"], 7)
        self.assertIsNone(svc.normalize_flux2_klein_params(self.params(seed=None))["seed"])


# ---------------------------------------------------------------------------
# Region validation
# ---------------------------------------------------------------------------
class RegionValidationTests(unittest.TestCase):
    def test_accepts_a_valid_region(self) -> None:
        svc.validate_region_size(512, 512)

    def test_rejects_a_non_multiple_of_16(self) -> None:
        with self.assertRaises(ValueError) as caught:
            svc.validate_region_size(500, 512)
        self.assertIn("496", str(caught.exception))

    def test_rejects_a_too_small_side(self) -> None:
        with self.assertRaises(ValueError):
            svc.validate_region_size(112, 512)

    def test_rejects_too_large_an_area(self) -> None:
        with self.assertRaises(ValueError) as caught:
            svc.validate_region_size(1536, 1024)
        self.assertIn("1048576", str(caught.exception))

    def test_rejects_an_extreme_aspect_ratio(self) -> None:
        with self.assertRaises(ValueError) as caught:
            svc.validate_region_size(1280, 128)
        self.assertIn("Соотношение сторон", str(caught.exception))

    def test_accepts_exactly_eight_to_one(self) -> None:
        svc.validate_region_size(1024, 128)


class EffectiveStepsTests(unittest.TestCase):
    def test_full_strength_keeps_every_step(self) -> None:
        self.assertEqual(svc.effective_steps(4, 1.0), 4)

    def test_the_quantization_is_coarse_at_four_steps(self) -> None:
        # 4 - int(4 - 3.2) == 4: strength 0.8 still runs all four steps.
        self.assertEqual(svc.effective_steps(4, 0.8), 4)

    def test_it_never_drops_below_one_step(self) -> None:
        self.assertEqual(svc.effective_steps(4, 0.25), 1)

if __name__ == "__main__":
    unittest.main()
