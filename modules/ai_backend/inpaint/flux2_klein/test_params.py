"""
File: modules/ai_backend/inpaint/flux2_klein/test_params.py

Purpose:
Unit tests for `params.py`: the wire contract of `normalize_flux2_klein_params`
and the region contract of `validate_region_size`.

Main responsibilities:
- verify every enum/path error and every numeric clamp of the wire contract,
  the refusal by name of every field of the retired placement wire, and the
  opt-in `text_attention_in_mask` and the local color match fields that
  `whole_region` forces off;
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

    def test_unknown_step_ids_raise(self) -> None:
        for field, value in (("vram_step", "turbo"), ("ram_step", "keep_forever"), ("vram_step", "keep")):
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                svc.normalize_flux2_klein_params(self.params(**{field: value}))

    def test_absent_or_empty_steps_mean_auto(self) -> None:
        for extra in ({}, {"vram_step": None, "ram_step": None}, {"vram_step": "", "ram_step": " "}):
            with self.subTest(extra=extra):
                out = svc.normalize_flux2_klein_params(self.params(**extra))
                self.assertEqual((out["vram_step"], out["ram_step"]), (None, None))
        out = svc.normalize_flux2_klein_params(self.params(vram_step=" half ", ram_step="run"))
        self.assertEqual((out["vram_step"], out["ram_step"]), ("half", "run"))

    def test_every_retired_field_is_refused_by_name(self) -> None:
        # Protocol 14: a stale client must get a clear error, never a silently
        # ignored memory choice; even an explicit `null` names the field.
        self.assertEqual(
            set(svc.RETIRED_FIELDS),
            {"placement", "profile", "low_cpu_mem_usage", "vae_slicing", "unload_transformer_before_vae", "unload_text_encoder_after_encode"},
        )
        for field in svc.RETIRED_FIELDS:
            for value in ("full_gpu", True, None):
                with self.subTest(field=field, value=value):
                    with self.assertRaises(ValueError) as caught:
                        svc.normalize_flux2_klein_params(self.params(**{field: value}))
                    self.assertIn(field, str(caught.exception))
                    self.assertIn(svc.RETIRED_FIELDS[field], str(caught.exception))

    def test_unknown_dtype_raises(self) -> None:
        with self.assertRaises(ValueError):
            svc.normalize_flux2_klein_params(self.params(dtype="float64"))

    def test_defaults(self) -> None:
        out = svc.normalize_flux2_klein_params(self.params())
        self.assertEqual(out["steps"], 4)
        self.assertAlmostEqual(out["guidance_scale"], 1.0)
        self.assertAlmostEqual(out["strength"], 1.0)
        self.assertEqual((out["vram_step"], out["ram_step"]), (None, None))
        self.assertTrue(out["vae_tiling"])
        self.assertEqual(out["dtype"], "bfloat16")
        self.assertEqual(out["mask_dilate_px"], 16)
        self.assertEqual(out["mask_feather_px"], 12)
        self.assertEqual(out["max_sequence_length"], 512)
        self.assertTrue(out["color_match"])
        self.assertEqual(out["color_match_local_pct"], 60)
        self.assertEqual(out["color_match_scale_px"], 24)
        self.assertFalse(out["whole_region"])
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
                "vram_step",
                "ram_step",
                "dtype",
                "vae_tiling",
                "text_encoder_fp8",
                "encoder_on_cpu",
                "mask_dilate_px",
                "mask_feather_px",
                "color_match",
                "color_match_local_pct",
                "color_match_scale_px",
                "whole_region",
                "max_sequence_length",
                "text_attention_in_mask",
            },
        )

    def test_encoder_on_cpu_is_on_unless_turned_off(self) -> None:
        self.assertIs(svc.normalize_flux2_klein_params(self.params())["encoder_on_cpu"], True)
        self.assertIs(svc.normalize_flux2_klein_params(self.params(encoder_on_cpu=False))["encoder_on_cpu"], False)

    def test_text_attention_in_mask_is_off_unless_asked_for(self) -> None:
        # A request written before the field existed must run exactly as before.
        self.assertFalse(
            svc.normalize_flux2_klein_params(self.params())["text_attention_in_mask"]
        )
        for raw, expected in ((True, True), ("true", True), (1, True), ("on", True),
                              (False, False), ("0", False), ("maybe", False), (None, False)):
            with self.subTest(raw=raw):
                out = svc.normalize_flux2_klein_params(self.params(text_attention_in_mask=raw))
                self.assertIs(out["text_attention_in_mask"], expected)

    def test_whole_region_forces_text_attention_in_mask_off_and_says_so(self) -> None:
        # Every token is inside a solid mask, so the attention mask would block
        # nothing and still cost S² bytes; the effective value must be false.
        with self.assertLogs(svc.log, level="INFO") as captured:
            out = svc.normalize_flux2_klein_params(
                self.params(whole_region=True, text_attention_in_mask=True)
            )
        self.assertFalse(out["text_attention_in_mask"])
        self.assertTrue(any("text_attention_in_mask" in line for line in captured.output))

    def test_the_local_color_match_fields_clamp_to_their_ranges(self) -> None:
        for raw, expected in ((-5, 0), (0, 0), (35, 35), (100, 100), (250, 100), ("x", 60), (None, 60), (True, 60)):
            with self.subTest(pct=raw):
                self.assertEqual(svc.normalize_flux2_klein_params(self.params(color_match_local_pct=raw))["color_match_local_pct"], expected)
        for raw, expected in ((1, 8), (8, 8), (40, 40), (96, 96), (500, 96), (None, 24)):
            with self.subTest(scale=raw):
                self.assertEqual(svc.normalize_flux2_klein_params(self.params(color_match_scale_px=raw))["color_match_scale_px"], expected)

    def test_whole_region_switches_the_local_color_match_off_and_says_so(self) -> None:
        with self.assertLogs(svc.log, level="INFO") as captured:
            out = svc.normalize_flux2_klein_params(
                self.params(whole_region=True, color_match=False, color_match_local_pct=80, color_match_scale_px=40)
            )
        self.assertEqual(out["color_match_local_pct"], 0)
        # No "off" value exists for the scale; it is left as sent and never read.
        self.assertEqual(out["color_match_scale_px"], 40)
        self.assertTrue(any("color_match_local_pct" in line for line in captured.output))

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
