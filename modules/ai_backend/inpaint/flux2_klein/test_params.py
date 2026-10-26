"""
File: modules/ai_backend/inpaint/flux2_klein/test_params.py

Purpose:
Unit tests for `params.py`: the wire contract of `normalize_flux2_klein_params`
and the region contract of `validate_region_size`.

Main responsibilities:
- verify every enum/path error and every numeric clamp of the wire contract,
  the refusal by name of every retired field (the placement wire, protocol 14;
  the region post-processing, protocol 17), and the opt-in
  `text_attention_in_mask` that `whole_region` forces off;
- verify a region that the pipeline would silently resize or crop is refused
  with concrete numbers instead;
- verify `effective_steps` matches what the pipeline will actually run;
- verify the weight-format fields of protocol 21 (`transformer_format`,
  `transformer_formats`, and the encoder's `text_encoder_format` /
  `text_encoder_formats`): the derived id per container (an absent encoder
  derives `bf16`), the opaque-id shape, the candidate list's contract and
  every refusal naming its field.
"""

from __future__ import annotations

import unittest
from pathlib import Path

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import params as params_module

from ._test_fixtures import _TempTreeCase, _write_tiny_gguf


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
        # A stale client must get a clear error, never a silently ignored
        # choice; even an explicit `null` names the field and its protocol.
        self.assertEqual(
            {field: protocol for field, (protocol, _replacement) in svc.RETIRED_FIELDS.items()},
            {
                "placement": 14,
                "profile": 14,
                "low_cpu_mem_usage": 14,
                "vae_slicing": 14,
                "unload_transformer_before_vae": 14,
                "unload_text_encoder_after_encode": 14,
                "mask_dilate_px": 17,
                "mask_feather_px": 17,
                "color_match": 17,
                "color_match_local_pct": 17,
                "color_match_scale_px": 17,
            },
        )
        for field, (protocol, replacement) in svc.RETIRED_FIELDS.items():
            for value in ("full_gpu", True, None):
                with self.subTest(field=field, value=value):
                    with self.assertRaises(ValueError) as caught:
                        svc.normalize_flux2_klein_params(self.params(**{field: value}))
                    self.assertIn(field, str(caught.exception))
                    self.assertIn(f"протокол {protocol}", str(caught.exception))
                    self.assertIn(replacement, str(caught.exception))

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
        self.assertEqual(out["max_sequence_length"], 512)
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
                "prompt_library",
                "text_encoder_source",
                "prompt_cache_precision_order",
                "text_encoder_path",
                "transformer_path",
                "vae_path",
                "transformer_format",
                "transformer_formats",
                "text_encoder_format",
                "text_encoder_formats",
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

    def test_numeric_clamping(self) -> None:
        out = svc.normalize_flux2_klein_params(
            self.params(
                steps=9999,
                guidance_scale=-3.0,
                strength=0.0,
                max_sequence_length=4096,
            )
        )
        self.assertEqual(out["steps"], 50)
        self.assertAlmostEqual(out["guidance_scale"], 1.0)
        self.assertAlmostEqual(out["strength"], 0.25)
        self.assertEqual(out["max_sequence_length"], 512)

    def test_seed_is_kept_and_null_means_random(self) -> None:
        self.assertEqual(svc.normalize_flux2_klein_params(self.params(seed=7))["seed"], 7)
        self.assertIsNone(svc.normalize_flux2_klein_params(self.params(seed=None))["seed"])


def _has_gguf() -> bool:
    try:
        import gguf  # noqa: F401 - availability probe
    except ImportError:
        return False
    return True


@unittest.skipUnless(_has_gguf(), "needs the gguf package (fixture writer)")
class WeightFormatParamsTests(_TempTreeCase):
    """The protocol-21 weight-format fields of the transformer (D5 matrix)."""

    def setUp(self) -> None:
        super().setUp()
        self.gguf = str(_write_tiny_gguf(self.root / "transformer_gguf" / "klein-q8.gguf"))

    def _normalized(self, **overrides: object) -> dict[str, object]:
        return svc.normalize_flux2_klein_params(self.params(**overrides))

    def test_absent_fields_derive_the_id_from_the_container(self) -> None:
        folder = self._normalized()
        self.assertEqual((folder["transformer_format"], folder["transformer_formats"]), ("bf16", ()))
        gguf = self._normalized(transformer_path=self.gguf)
        self.assertEqual((gguf["transformer_format"], gguf["transformer_formats"]), ("gguf", ()))
        single = self.root / "single.safetensors"
        single.write_bytes((Path(self.paths["transformer_path"]) / "diffusion_pytorch_model.safetensors").read_bytes())
        self.assertEqual(self._normalized(transformer_path=str(single))["transformer_format"], "bf16")
        self.assertEqual(self._normalized(transformer_format=None)["transformer_format"], "bf16", "null reads as absent")

    def test_a_named_format_is_opaque(self) -> None:
        out = self._normalized(transformer_path=self.gguf, transformer_format="q8_0")
        self.assertEqual((out["transformer_format"], out["transformer_formats"]), ("q8_0", ()))
        # Nothing checks the id against the container: the client owns the names.
        self.assertEqual(self._normalized(transformer_format="anything_1")["transformer_format"], "anything_1")

    def test_a_candidate_list_is_kept_in_order_as_pairs(self) -> None:
        formats = [{"id": "bf16", "path": self.paths["transformer_path"]}, {"id": "q8_0", "path": self.gguf}, {"id": "q4_k_m", "path": str(self.root / "missing.gguf")}]
        out = self._normalized(transformer_path=self.gguf, transformer_format="q8_0", transformer_formats=formats)
        self.assertEqual(out["transformer_format"], "q8_0")
        self.assertEqual(out["transformer_formats"], (("bf16", self.paths["transformer_path"]), ("q8_0", self.gguf), ("q4_k_m", str(self.root / "missing.gguf"))))
        derived = self._normalized(transformer_formats=[{"id": "bf16", "path": self.paths["transformer_path"]}])
        self.assertEqual(derived["transformer_format"], "bf16", "an absent id is derived, then looked up")

    def test_bad_ids_are_refused_by_field(self) -> None:
        for bad in ("Q8_0", "", "q8-0", "x" * 33, 8, True, ["q8_0"]):
            with self.subTest(bad=bad), self.assertRaisesRegex(ValueError, "transformer_format"):
                self._normalized(transformer_format=bad)

    def test_bad_lists_are_refused_by_field(self) -> None:
        good = {"id": "bf16", "path": self.paths["transformer_path"]}
        for bad, field in (
            ([], "transformer_formats"),
            ({"id": "bf16"}, "transformer_formats"),
            ("bf16", "transformer_formats"),
            ([good, "q8_0"], r"transformer_formats\[1\]"),
            ([good, {"id": "q8_0"}], r"transformer_formats\[1\]"),
            ([good, {"id": "q8_0", "path": self.gguf, "size": 1}], r"transformer_formats\[1\]"),
            ([good, {"id": "Q8", "path": self.gguf}], r"transformer_formats\[1\]\.id"),
            ([good, {"id": "q8_0", "path": ""}], r"transformer_formats\[1\]\.path"),
            ([good, {"id": "q8_0", "path": 5}], r"transformer_formats\[1\]\.path"),
            ([good, dict(good)], "дважды"),
        ):
            with self.subTest(bad=bad), self.assertRaisesRegex(ValueError, field):
                self._normalized(transformer_formats=bad)

    def test_the_list_must_carry_the_selected_file(self) -> None:
        with self.assertRaisesRegex(ValueError, "transformer_format"):
            self._normalized(transformer_format="q8_0", transformer_formats=[{"id": "bf16", "path": self.paths["transformer_path"]}])
        # The selected id with another path: the list may not redirect the request.
        with self.assertRaisesRegex(ValueError, "transformer_path"):
            self._normalized(transformer_format="bf16", transformer_formats=[{"id": "bf16", "path": self.gguf}])
        with self.assertRaisesRegex(ValueError, "transformer_path"):
            self._normalized(transformer_format="bf16", transformer_formats=[{"id": "bf16", "path": self.paths["transformer_path"] + "/"}])

    def test_derived_format_reads_the_container_not_the_name(self) -> None:
        disguised = self.root / "weights.safetensors"
        disguised.write_bytes(Path(self.gguf).read_bytes())
        self.assertEqual(params_module.derived_format(str(disguised)), "gguf")
        self.assertEqual(params_module.derived_format(self.paths["transformer_path"]), "bf16")
        with self.assertRaises(ValueError):
            params_module.derived_format(str(self.root / "absent.gguf"))


class EncoderWeightFormatParamsTests(_TempTreeCase):
    """The protocol-21 weight-format fields of the text encoder: the transformer's contract, on an optional path."""

    def setUp(self) -> None:
        super().setUp()
        # Any GGUF container derives `gguf`; the encoder's own content is not read here.
        self.gguf = str(_write_tiny_gguf(self.root / "text_encoder_gguf" / "qwen3-q8.gguf"))

    def _normalized(self, **overrides: object) -> dict[str, object]:
        return svc.normalize_flux2_klein_params(self.params(**overrides))

    def test_absent_fields_derive_the_id_from_the_container(self) -> None:
        folder = self._normalized()
        self.assertEqual((folder["text_encoder_format"], folder["text_encoder_formats"]), ("bf16", ()))
        self.assertEqual(self._normalized(text_encoder_path=self.gguf)["text_encoder_format"], "gguf")

    def test_an_absent_encoder_derives_bf16_and_is_not_an_error(self) -> None:
        for path in ("", str(self.root / "elsewhere" / "qwen3.gguf")):
            with self.subTest(path=path):
                out = self._normalized(text_encoder_path=path)
                self.assertEqual((out["text_encoder_format"], out["text_encoder_formats"]), ("bf16", ()))

    def test_a_candidate_list_follows_the_transformer_contract(self) -> None:
        formats = [{"id": "bf16", "path": self.paths["text_encoder_path"]}, {"id": "q8_0", "path": self.gguf}]
        out = self._normalized(text_encoder_path=self.gguf, text_encoder_format="q8_0", text_encoder_formats=formats)
        self.assertEqual((out["text_encoder_format"], out["text_encoder_formats"]), ("q8_0", (("bf16", self.paths["text_encoder_path"]), ("q8_0", self.gguf))))
        with self.assertRaisesRegex(ValueError, "text_encoder_path"):
            self._normalized(text_encoder_format="q8_0", text_encoder_formats=formats)
        with self.assertRaisesRegex(ValueError, "text_encoder_format"):
            self._normalized(text_encoder_format="Q8")
        with self.assertRaisesRegex(ValueError, r"text_encoder_formats\[1\]\.path"):
            self._normalized(text_encoder_formats=[formats[0], {"id": "q8_0", "path": ""}])


# ---------------------------------------------------------------------------
# Region validation
# ---------------------------------------------------------------------------
class PromptLibraryParamsTests(_TempTreeCase):
    """Protocol 23: `prompt_library`, `text_encoder_source`, `prompt_cache_precision_order`."""

    def test_absent_fields_mean_no_library_and_no_preference(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params())
        self.assertEqual((normalized["prompt_library"], normalized["text_encoder_source"], normalized["prompt_cache_precision_order"]), (None, "", ()))
        self.assertEqual(params_module.prompt_library_fields({"prompt_library": None, "text_encoder_source": None, "prompt_cache_precision_order": None}), (None, "", ()))
        self.assertEqual(params_module.prompt_library_fields(None), (None, "", ()))

    def test_valid_fields_are_kept(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params(prompt_library="flux2_klein_9b", text_encoder_source="uncensored", prompt_cache_precision_order=["bf16", "q8_0", "fp8"]))
        self.assertEqual((normalized["prompt_library"], normalized["text_encoder_source"], normalized["prompt_cache_precision_order"]), ("flux2_klein_9b", "uncensored", ("bf16", "q8_0", "fp8")))
        self.assertEqual(params_module.prompt_library_fields({"text_encoder_source": ""})[1], "", "manual mode may send an empty source")

    def test_a_bad_field_is_refused_by_name(self) -> None:
        for field, value in (
            ("prompt_library", "Flux-9B"),
            ("prompt_library", ""),
            ("prompt_library", 9),
            ("prompt_library", "x" * 41),
            ("text_encoder_source", "Stock"),
            ("text_encoder_source", 1),
            ("prompt_cache_precision_order", "bf16"),
            ("prompt_cache_precision_order", ["bf16", "BF16"]),
            ("prompt_cache_precision_order", ["bf16", "bf16"]),
        ):
            with self.subTest(field=field, value=value), self.assertRaisesRegex(ValueError, field):
                svc.normalize_flux2_klein_params(self.params(**{field: value}))


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
