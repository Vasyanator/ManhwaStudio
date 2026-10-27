"""
File: modules/ai_backend/inpaint/flux3_klein/test_params.py

Purpose:
Unit tests for `flux3_klein/params.py`: request normalization (defaults and
every refusal), the reference default seed, and the `Flux3Options` defaults and
validation, including the DiT / encoder sources (`variant`, `dit_formats`,
`encoder`, the GGUF-encoder-is-TEXT-only rule).

Notes:
Torch- and PIL-free: references are stand-in objects, since `normalize_request`
only counts them.
"""

from __future__ import annotations

import hashlib
import unittest
from pathlib import Path
from typing import Any

from ...runtime.large_models.tiers import RamStep, Steps, VramStep
from . import params as p
from .config import ENCODER_BF16_SOURCE, MAX_REFERENCES, NATIVE_DIT_FORMATS, EncoderSource, GgufDit, SamplerConfig, WeightFormat

SAMPLER = SamplerConfig(steps=16, alpha=6.93, default_size=(1024, 1024))


def _request(**kwargs: Any) -> p.Flux3Request:
    """A valid request with `kwargs` overriding fields."""
    fields: dict[str, Any] = {"caption": "a cat on a roof"}
    fields.update(kwargs)
    return p.Flux3Request(**fields)


class DefaultSeedTests(unittest.TestCase):
    def test_it_is_the_reference_rule(self) -> None:
        caption = "a red fox, watercolour"
        expected = int(hashlib.sha256(caption.encode()).hexdigest(), 16) % 2**32
        self.assertEqual(p.default_seed(caption), expected)

    def test_it_is_deterministic_and_32_bit(self) -> None:
        self.assertEqual(p.default_seed("x"), p.default_seed("x"))
        self.assertNotEqual(p.default_seed("x"), p.default_seed("y"))
        self.assertLess(p.default_seed("кириллица"), 2**32)


class NormalizeRequestTests(unittest.TestCase):
    def test_defaults_come_from_the_caption_and_the_sampler(self) -> None:
        normalized = p.normalize_request(_request(), SAMPLER)
        self.assertEqual(normalized.seed, p.default_seed("a cat on a roof"))
        self.assertEqual(normalized.steps, 16)
        self.assertIsNone(normalized.width)
        self.assertIsNone(normalized.height)
        self.assertEqual(normalized.references, ())

    def test_explicit_values_are_kept(self) -> None:
        refs = (object(), object())
        normalized = p.normalize_request(
            _request(references=refs, width=64, height=4096, seed=0, steps=64), SAMPLER
        )
        self.assertEqual((normalized.width, normalized.height), (64, 4096))
        self.assertEqual((normalized.seed, normalized.steps), (0, 64))
        self.assertEqual(normalized.references, refs)

    def test_one_side_alone_is_allowed(self) -> None:
        normalized = p.normalize_request(_request(width=512), SAMPLER)
        self.assertEqual((normalized.width, normalized.height), (512, None))

    def test_invalid_requests_are_refused(self) -> None:
        cases: dict[str, dict[str, Any]] = {
            "empty caption": {"caption": ""},
            "blank caption": {"caption": "  \n"},
            "too many references": {"references": tuple(object() for _ in range(MAX_REFERENCES + 1))},
            "width not a multiple of 16": {"width": 1000},
            "width below 64": {"width": 48},
            "height above 4096": {"height": 4112},
            "bool height": {"height": True},
            "zero steps": {"steps": 0},
            "too many steps": {"steps": 65},
            "float steps": {"steps": 4.0},
            "negative seed": {"seed": -1},
            "seed above 64 bits": {"seed": 2**64},
        }
        for name, fields in cases.items():
            with self.subTest(name=name):
                with self.assertRaises(ValueError):
                    p.normalize_request(_request(**fields), SAMPLER)

    def test_the_reference_cap_itself_is_allowed(self) -> None:
        refs = tuple(object() for _ in range(MAX_REFERENCES))
        self.assertEqual(len(p.normalize_request(_request(references=refs), SAMPLER).references), MAX_REFERENCES)

    def test_sampler_steps_are_validated_too(self) -> None:
        with self.assertRaises(ValueError):
            p.normalize_request(_request(), SamplerConfig(steps=100))


class Flux3OptionsTests(unittest.TestCase):
    def test_defaults(self) -> None:
        options = p.Flux3Options(model_root=Path("/fictional/flux3"), variant=WeightFormat.FP8R)
        self.assertIs(options.encoder_mode, p.EncoderMode.TEXT)
        self.assertIs(options.fp8_compute, p.Fp8Compute.AUTO)
        self.assertIs(options.ae_dtype, p.AeDtype.FLOAT32)
        self.assertEqual(options.vl_ref_max_pixels, 512 * 512)
        self.assertIsNone(options.device)
        self.assertIsNone(options.reference_max_pixels)
        self.assertEqual((options.vram_step, options.ram_step), (VramStep.AUTO, RamStep.AUTO))
        self.assertEqual(options.steps, Steps(VramStep.AUTO, RamStep.AUTO))

    def test_reference_max_pixels_bounds(self) -> None:
        root = Path("/fictional/flux3")
        ok = p.Flux3Options(model_root=root, variant=WeightFormat.BF16, reference_max_pixels=p.MIN_REFERENCE_PIXELS)
        self.assertEqual(ok.reference_max_pixels, 64 * 64)
        for value in (p.MIN_REFERENCE_PIXELS - 1, 0, 1.5e6, True):
            with self.subTest(value=value):
                with self.assertRaises(ValueError):
                    p.Flux3Options(model_root=root, variant=WeightFormat.BF16, reference_max_pixels=value)  # type: ignore[arg-type]

    def test_the_format_and_the_steps_are_validated(self) -> None:
        root = Path("/fictional/flux3")
        options = p.Flux3Options(model_root=root, variant=WeightFormat.NVFP4, vram_step=VramStep.HALF, ram_step=RamStep.MIN)
        self.assertEqual(options.steps, Steps(VramStep.HALF, RamStep.MIN))
        refused = (
            {"variant": "nvfp4"},
            {"variant": None},
            {"variant": WeightFormat.BF16, "vram_step": "auto"},
            {"variant": WeightFormat.BF16, "ram_step": "auto"},
            # The axes are not interchangeable: a RAM step on the VRAM axis is refused.
            {"variant": WeightFormat.BF16, "vram_step": RamStep.MIN},
            {"variant": WeightFormat.BF16, "ram_step": VramStep.MIN},
        )
        for overrides in refused:
            with self.subTest(overrides=overrides):
                with self.assertRaises(ValueError):
                    p.Flux3Options(model_root=root, **overrides)  # type: ignore[arg-type]

    def test_the_removed_residency_options_are_gone(self) -> None:
        # B5: where the components live follows from the two resource steps alone.
        fields = set(p.Flux3Options.__dataclass_fields__)
        self.assertFalse({"encoder_policy", "dit_offload_during_ae", "profile"} & fields)
        self.assertFalse(hasattr(p, "EncoderPolicy"))

    def test_a_non_positive_pixel_cap_is_refused(self) -> None:
        for value in (0, -5, 1.5):
            with self.subTest(value=value):
                with self.assertRaises(ValueError):
                    p.Flux3Options(
                        model_root=Path("/fictional/flux3"), variant=WeightFormat.BF16, vl_ref_max_pixels=value  # type: ignore[arg-type]
                    )



class SourceTests(unittest.TestCase):
    """`Flux3Options.variant` / `dit_formats` / `encoder` and the `config` source types."""

    ROOT = Path("/fictional/flux3")
    Q8 = GgufDit("q8_0", Path("/fictional/flux3/transformer_gguf/q8.gguf"))
    ENC = EncoderSource("q8_0", Path("/fictional/flux3/text_encoder_gguf/enc.gguf"))

    def test_defaults_are_the_natives_and_the_bf16_encoder(self) -> None:
        options = p.Flux3Options(model_root=self.ROOT, variant=WeightFormat.NVFP4)
        self.assertEqual((options.dit_formats, options.encoder), (NATIVE_DIT_FORMATS, ENCODER_BF16_SOURCE))
        self.assertEqual(NATIVE_DIT_FORMATS, (WeightFormat.BF16, WeightFormat.FP8R, WeightFormat.NVFP4))

    def test_a_gguf_variant_without_an_offer_is_appended_to_the_natives(self) -> None:
        options = p.Flux3Options(model_root=self.ROOT, variant=self.Q8)
        self.assertEqual(options.dit_formats, (*NATIVE_DIT_FORMATS, self.Q8))
        self.assertEqual(options.variant.value, "q8_0")

    def test_an_explicit_offer_is_kept_in_order(self) -> None:
        offer = (self.Q8, WeightFormat.FP8R)
        self.assertEqual(p.Flux3Options(model_root=self.ROOT, variant=WeightFormat.FP8R, dit_formats=offer).dit_formats, offer)

    def test_refusals(self) -> None:
        other_path = GgufDit("q8_0", Path("/fictional/other.gguf"))
        refused: tuple[dict[str, Any], ...] = (
            {"variant": WeightFormat.BF16, "dit_formats": (self.Q8,)},
            {"variant": other_path, "dit_formats": (self.Q8,)},
            {"variant": WeightFormat.BF16, "dit_formats": (WeightFormat.BF16, WeightFormat.BF16)},
            {"variant": self.Q8, "dit_formats": (self.Q8, other_path)},
            {"variant": WeightFormat.BF16, "dit_formats": (WeightFormat.BF16, "fp8r")},
            {"variant": WeightFormat.BF16, "encoder": "bf16"},
        )
        for overrides in refused:
            with self.subTest(overrides=overrides), self.assertRaises(ValueError):
                p.Flux3Options(model_root=self.ROOT, **overrides)

    def test_a_gguf_encoder_serves_the_text_mode_only(self) -> None:
        options = p.Flux3Options(model_root=self.ROOT, variant=WeightFormat.BF16, encoder=self.ENC)
        self.assertTrue(options.encoder.is_gguf)
        with self.assertRaises(ValueError) as caught:
            p.Flux3Options(model_root=self.ROOT, variant=WeightFormat.BF16, encoder=self.ENC, encoder_mode=p.EncoderMode.VL_REFS)
        self.assertIn("q8_0", str(caught.exception))
        self.assertIs(p.Flux3Options(model_root=self.ROOT, variant=WeightFormat.BF16, encoder_mode=p.EncoderMode.VL_REFS).encoder, ENCODER_BF16_SOURCE)

    def test_source_construction_rules(self) -> None:
        for args in (("bf16", Path("/x.gguf")), ("Q8_0", Path("/x.gguf")), ("q8_0", Path("x.gguf")), ("", Path("/x.gguf"))):
            with self.subTest(dit=args), self.assertRaises(ValueError):
                GgufDit(*args)
        for args in (("bf16", Path("/x.gguf")), ("q8_0", None), ("q8_0", Path("x.gguf")), ("q 8", Path("/x.gguf"))):
            with self.subTest(encoder=args), self.assertRaises(ValueError):
                EncoderSource(*args)
        self.assertFalse(ENCODER_BF16_SOURCE.is_gguf)


if __name__ == "__main__":
    unittest.main()
