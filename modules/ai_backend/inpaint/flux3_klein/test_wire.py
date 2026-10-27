"""
File: modules/ai_backend/inpaint/flux3_klein/test_wire.py

Purpose:
Unit tests of `wire.py`: every `params` key with its default, strict types and
ranges, unknown keys and bad enums refused by name (the post-processing fields
retired by protocol 17 among them), the DiT / text-encoder format candidates
(`dit_formats`, `format`, `text_encoder_formats`, `text_encoder_format`: ids,
paths, duplicates named at their indexed `id`, the D6 refusal of `vl_refs`
with a GGUF encoder; the encoder candidates become
`Flux3Options.encoder_formats`, the encoder handle's rows, whose own
construction rules are pinned here too), the `.status` reading that validates every field even
without `source_root` (`normalize_flux3_status_params`), the prompt-library
fields (`prompt_library` = `flux3_klein` only, `prompt_cache_precision_order`)
and the library methods' reading that keeps them without a root
(`normalize_flux3_library_params`), and the
region frame rule (multiple of 16, side >= 128, 4 MP, aspect 4:1) with the area
cap equal to `packing.MATCH_MAX_PIXELS`.

Notes:
Torch is imported only by the area-cap equality check (through `packing`).
"""

from __future__ import annotations

import unittest
from pathlib import Path

from ...runtime.large_models.tiers import RamStep, VramStep
from . import wire
from .config import ENCODER_BF16_SOURCE, NATIVE_DIT_FORMATS, EncoderSource, GgufDit, WeightFormat
from .params import MAX_SEED, AeDtype, EncoderMode, Flux3Options, Fp8Compute
from .prompt_cache import LIBRARY_ID, Flux3PromptCacheRequest


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
        self.assertFalse(normalized.whole_region)
        self.assertEqual((options.dit_formats, options.encoder, normalized.text_encoder_formats), (NATIVE_DIT_FORMATS, ENCODER_BF16_SOURCE, (ENCODER_BF16_SOURCE,)))
        self.assertEqual(normalized.prompt_cache, Flux3PromptCacheRequest(None, ()))

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
            whole_region=True,
            prompt_library="flux3_klein",
            prompt_cache_precision_order=["bf16", "q8_0", "q4_k_m"],
        )
        options = normalized.options
        self.assertEqual((options.variant, options.encoder_mode, options.ae_dtype, options.fp8_compute), (WeightFormat.NVFP4, EncoderMode.VL_REFS, AeDtype.BFLOAT16, Fp8Compute.DEQUANT))
        self.assertEqual((options.reference_max_pixels, options.vram_step, options.ram_step, options.encoder_on_cpu), (512 * 512, VramStep.HALF, RamStep.MIN, False))
        self.assertEqual((normalized.prompt, normalized.steps, normalized.seed, normalized.whole_region), ("remove the text", 20, MAX_SEED, True))
        self.assertEqual(normalized.prompt_cache, Flux3PromptCacheRequest(LIBRARY_ID, ("bf16", "q8_0", "q4_k_m")))
        self.assertEqual(
            wire.FIELDS,
            {"source_root", "dit_formats", "format", "text_encoder_format", "text_encoder_formats", "encoder_mode", "ae_dtype", "fp8_compute", "reference_max_pixels", "prompt", "steps", "seed", "vram_step", "ram_step", "encoder_on_cpu", "whole_region", "prompt_library", "prompt_cache_precision_order"},
        )

    def test_the_post_processing_fields_are_unknown_keys(self) -> None:
        # Protocol 17: the client post-processes the raw answer, so a request
        # still carrying a post field is refused by name, never ignored.
        for field, value in (("mask_feather_px", 12), ("color_match", True), ("color_match_local_pct", 60), ("color_match_scale_px", 24)):
            with self.subTest(field=field), self.assertRaises(ValueError) as caught:
                self._normalize(**{field: value})
            self.assertIn(field, str(caught.exception))

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
            "reference_max_pixels": {"reference_max_pixels": 100},
            "encoder_on_cpu": {"encoder_on_cpu": "yes"},
            "whole_region": {"whole_region": "true"},
            "prompt": {"prompt": 5},
            "prompt_library": {"prompt_library": "flux2_klein_9b"},
            "prompt_cache_precision_order": {"prompt_cache_precision_order": "bf16"},
            "text_encoder_source": {"text_encoder_source": "stock"},
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

    def test_status_params_without_a_root_are_still_validated(self) -> None:
        # Only a missing root is the "unavailable" reading (`None`) ...
        for params in (None, {}, {"source_root": None}, {"source_root": ""}, {"source_root": "  "}, {"format": "bf16", "encoder_on_cpu": False}):
            with self.subTest(params=params):
                self.assertIsNone(wire.normalize_flux3_status_params(params))
        self.assertEqual(wire.normalize_flux3_status_params({"source_root": self.ROOT}).options.model_root, Path(self.ROOT))
        # ... every other malformed field is refused by name, root or not.
        cases = {
            "bogus": {"bogus": 1},
            "format": {"format": 8},
            "fp8_compute": {"fp8_compute": "fast"},
            "dit_formats": {"dit_formats": []},
            "dit_formats[1].id": {"dit_formats": [{"id": "bf16"}, {"id": "bf16"}]},
            "text_encoder_formats[0].path": {"text_encoder_formats": [{"id": "q8_0", "path": "relative.gguf"}]},
            "encoder_mode": {"text_encoder_formats": [{"id": "q8_0", "path": "/m/q8.gguf"}], "text_encoder_format": "q8_0", "encoder_mode": "vl_refs"},
            "steps": {"steps": 65},
            "whole_region": {"whole_region": "true"},
            "source_root": {"source_root": 5},
            "prompt_library": {"prompt_library": "Bad-Id"},
        }
        for field, params in cases.items():
            with self.subTest(field=field):
                with self.assertRaises(ValueError) as caught:
                    wire.normalize_flux3_status_params(params)
                self.assertIn(field, str(caught.exception))
        with self.assertRaises(ValueError):
            wire.normalize_flux3_status_params(["source_root"])  # type: ignore[arg-type]

    def test_the_library_reading_keeps_the_library_fields_without_a_root(self) -> None:
        cache, normalized = wire.normalize_flux3_library_params({"prompt_library": "flux3_klein", "prompt_cache_precision_order": ["q8_0"]})
        self.assertIsNone(normalized)
        self.assertEqual(cache, Flux3PromptCacheRequest(LIBRARY_ID, ("q8_0",)))
        cache, normalized = wire.normalize_flux3_library_params({"source_root": self.ROOT, "prompt_library": "flux3_klein"})
        self.assertIsNotNone(normalized)
        self.assertEqual(normalized.prompt_cache if normalized is not None else None, cache)
        for params in ({"prompt_library": "flux2_klein_4b"}, {"prompt_cache_precision_order": ["bf16", "bf16"]}, {"bogus": 1}):
            with self.subTest(params=params), self.assertRaises(ValueError):
                wire.normalize_flux3_library_params(params)


class FormatCandidateTests(unittest.TestCase):
    """D7: the offered DiT / encoder formats, the selection among them, D6."""

    ROOT = "/models/flux3"
    DIT = "/models/flux3/transformer_gguf/flux-3-image-klein-Q8_0.gguf"
    ENC = "/models/flux3/text_encoder_gguf/Qwen3VL-4B-Instruct-Q8_0.gguf"

    def _normalize(self, **params: object) -> wire.Flux3WireParams:
        return wire.normalize_flux3_params({"source_root": self.ROOT, **params}, require_prompt=False)

    def _refused(self, field: str, **params: object) -> None:
        with self.assertRaises(ValueError) as caught:
            self._normalize(**params)
        self.assertIn(field, str(caught.exception))

    def test_a_full_catalog_offer(self) -> None:
        offered = [{"id": "bf16"}, {"id": "fp8r", "path": None}, {"id": "q8_0", "path": self.DIT}, {"id": "nvfp4"}, {"id": "q4_k_m", "path": "/models/q4.gguf"}]
        normalized = self._normalize(dit_formats=offered, format="q8_0", text_encoder_formats=[{"id": "q8_0", "path": self.ENC}], text_encoder_format="q8_0")
        options = normalized.options
        q8 = GgufDit("q8_0", Path(self.DIT))
        self.assertEqual(options.variant, q8)
        self.assertEqual(options.variant.value, "q8_0")
        self.assertEqual(options.dit_formats, (WeightFormat.BF16, WeightFormat.FP8R, q8, WeightFormat.NVFP4, GgufDit("q4_k_m", Path("/models/q4.gguf"))))
        self.assertEqual(options.encoder, EncoderSource("q8_0", Path(self.ENC)))
        self.assertEqual(normalized.text_encoder_formats, (ENCODER_BF16_SOURCE, EncoderSource("q8_0", Path(self.ENC))))
        # The encoder handle's format rows are the same candidates, bf16 first.
        self.assertEqual(options.encoder_formats, normalized.text_encoder_formats)
        # The same id may name a DiT and an encoder format: separate lists.
        self.assertEqual(self._normalize(dit_formats=offered, format="nvfp4").options.variant, WeightFormat.NVFP4)

    def test_the_encoder_defaults_to_bf16_even_with_gguf_candidates(self) -> None:
        normalized = self._normalize(text_encoder_formats=[{"id": "q4_k_m", "path": self.ENC}])
        self.assertEqual(normalized.options.encoder, ENCODER_BF16_SOURCE)
        self.assertEqual(self._normalize(text_encoder_format="bf16").options.encoder, ENCODER_BF16_SOURCE)

    def test_the_selection_must_be_offered(self) -> None:
        self._refused("format", format="q8_0")
        self._refused("format", dit_formats=[{"id": "bf16"}])  # the fp8r default is not offered
        self._refused("format", format=8)
        self._refused("text_encoder_format", text_encoder_format="q8_0")
        self._refused("text_encoder_format", text_encoder_formats=[{"id": "q8_0", "path": self.ENC}], text_encoder_format="q4_k_m")

    def test_malformed_dit_candidates_are_refused_by_name(self) -> None:
        cases = {
            "dit_formats": [],
            "dit_formats[0]": ["bf16"],
            "dit_formats[1].id": [{"id": "bf16"}, {"id": "Q8_0", "path": self.DIT}],
            "dit_formats[0].path": [{"id": "fp8r", "path": "/models/x.safetensors"}],
            "dit_formats[2].path": [{"id": "bf16"}, {"id": "fp8r"}, {"id": "q8_0"}],
            "dit_formats[0].path ": [{"id": "q8_0", "path": "relative/x.gguf"}],
            "dit_formats[0].path  ": [{"id": "q8_0", "path": 5}],
            "dit_formats[0]  ": [{"id": "q8_0", "path": self.DIT, "sha256": "x"}],
            "dit_formats[2].id": [{"id": "fp8r"}, {"id": "bf16"}, {"id": "fp8r"}],
        }
        for field, value in cases.items():
            with self.subTest(field=field):
                self._refused(field.strip(), dit_formats=value)
        self._refused("dit_formats", dit_formats={"id": "bf16"})
        self._refused("dit_formats[0].id", dit_formats=[{"id": "x" * 33, "path": self.DIT}])

    def test_malformed_encoder_candidates_are_refused_by_name(self) -> None:
        cases = {
            "text_encoder_formats[0].id": [{"id": "bf16", "path": self.ENC}],
            "text_encoder_formats[0].path": [{"id": "q8_0"}],
            "text_encoder_formats[1].path": [{"id": "q8_0", "path": self.ENC}, {"id": "q4_k_m", "path": "q4.gguf"}],
            "text_encoder_formats[1].id": [{"id": "q8_0", "path": self.ENC}, {"id": "q8_0", "path": "/models/other.gguf"}],
        }
        for field, value in cases.items():
            with self.subTest(field=field):
                self._refused(field, text_encoder_formats=value)

    def test_vl_refs_with_a_gguf_encoder_is_refused_by_name(self) -> None:
        candidates = [{"id": "q8_0", "path": self.ENC}]
        for field in ("encoder_mode", "text_encoder_format"):
            with self.subTest(field=field):
                self._refused(field, text_encoder_formats=candidates, text_encoder_format="q8_0", encoder_mode="vl_refs")
        # The bf16 encoder keeps VL_REFS, whatever GGUF candidates are offered.
        self.assertIs(self._normalize(text_encoder_formats=candidates, encoder_mode="vl_refs").options.encoder_mode, EncoderMode.VL_REFS)

    def test_nulls_read_as_absent(self) -> None:
        normalized = self._normalize(dit_formats=None, text_encoder_formats=None, text_encoder_format=None)
        self.assertEqual((normalized.options.dit_formats, normalized.options.encoder), (NATIVE_DIT_FORMATS, ENCODER_BF16_SOURCE))
        self.assertEqual(normalized.options.encoder_formats, (ENCODER_BF16_SOURCE,))

    def test_the_options_encoder_formats_rules(self) -> None:
        # Direct callers (tests, the calibration tool) may omit the candidates:
        # bf16, plus a selected GGUF encoder appended last.
        root = Path(self.ROOT)
        gguf = EncoderSource("q8_0", Path(self.ENC))
        self.assertEqual(Flux3Options(model_root=root, variant=WeightFormat.FP8R).encoder_formats, (ENCODER_BF16_SOURCE,))
        self.assertEqual(Flux3Options(model_root=root, variant=WeightFormat.FP8R, encoder=gguf).encoder_formats, (ENCODER_BF16_SOURCE, gguf))
        for bad in ((ENCODER_BF16_SOURCE, ENCODER_BF16_SOURCE), (ENCODER_BF16_SOURCE, "q8_0"), (gguf,)):
            with self.subTest(bad=bad), self.assertRaises(ValueError) as caught:
                Flux3Options(model_root=root, variant=WeightFormat.FP8R, encoder_formats=bad)  # type: ignore[arg-type]
            self.assertIn("encoder", str(caught.exception))


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
