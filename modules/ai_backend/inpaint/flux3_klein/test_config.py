"""
File: modules/ai_backend/inpaint/flux3_klein/test_config.py

Purpose:
Unit tests for `flux3_klein/config.py`: parsing and validating `config.json`
(the official package's shape as a literal fixture), weight-format detection
from literal headers shaped like the bf16 / fp8r / nvfp4 exports, the declared
vs detected check, and file resolution / availability over a temp tree.

Notes:
Torch-free; never touches a real model file. Temp files are removed by the test.
"""

from __future__ import annotations

import copy
import json
import logging
import tempfile
import unittest
from pathlib import Path
from typing import Any

from . import config as cfg

#: The official package's `config.json` (bf16 root), verbatim as data.
OFFICIAL_CONFIG: dict[str, Any] = {
    "format_version": 1,
    "model": "flux-3-image-klein",
    "quantization": None,
    "torch_dtype": "bfloat16",
    "dit": {
        "hidden": 3072,
        "heads": 24,
        "depth": 5,
        "depth_single": 28,
        "mlp_ratio": 3.0,
        "axes_dim": [32, 32, 32, 32],
        "theta": 10000,
        "in_channels": {"image": 128, "image_cond": 128, "text": 20480},
        "out_modes": ["image"],
        "vec_in_dim": None,
        "image_rope_scale": 0.5,
        "flow_map": True,
    },
    "text_encoder": {
        "path": "text_encoder",
        "tokenizer": "text_encoder",
        "layers": [4, 8, 12, 16, 20, 24, 28, 32],
        "pad_multiple": 80,
        "max_tokens": 8192,
        "dtype": "bfloat16",
    },
    "vae": {"path": "ae.safetensors", "dtype": "float32"},
    "sampler": {"steps": 16, "guidance": None, "alpha": 6.93, "size": [1024, 1024]},
}


def _entry(dtype: str) -> dict[str, Any]:
    """A header entry; shapes and offsets do not matter to format detection."""
    return {"dtype": dtype, "shape": [1], "data_offsets": [0, 1]}


#: Header shapes of the three exports (a few keys each, real suffixes and dtypes).
BF16_HEADER = {
    "__metadata__": {"format": "pt"},
    "double_blocks.0.img_attn.qkv.weight": _entry("BF16"),
    "double_blocks.0.img_attn.norm.query_norm.scale": _entry("BF16"),
    cfg.FLOW_MAP_KEY: _entry("BF16"),
}
FP8R_HEADER = {
    "__metadata__": {"quantization": "fp8r"},
    "single_blocks.0.q_proj.weight": _entry("F8_E4M3"),
    "single_blocks.0.q_proj.weight_scale": _entry("F32"),
    "single_blocks.0.norm.scale": _entry("BF16"),
    "txt_in.weight": _entry("BF16"),
}
NVFP4_HEADER = {
    "__metadata__": {"quantization": "nvfp4"},
    "single_blocks.0.q_proj.weight": _entry("U8"),
    "single_blocks.0.q_proj.weight_scale": _entry("F8_E4M3"),
    "single_blocks.0.q_proj.weight_scale_2": _entry("F32"),
    "single_blocks.0.q_proj.input_scale": _entry("F32"),
    "single_blocks.0.norm.scale": _entry("BF16"),
    "emb_in.image.weight": _entry("BF16"),
}


#: Sentinel for "remove this key" in the invalid-config cases.
_DELETE = object()


class _TempCase(unittest.TestCase):
    """A `TestCase` with a temp directory that is removed afterwards."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)

    def write_config(self, data: dict[str, Any], name: str = "config.json") -> Path:
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(data), encoding="utf-8")
        return path


class ReadPackageConfigTests(_TempCase):
    def test_the_official_config_parses(self) -> None:
        with self.assertLogs(cfg.log, level=logging.INFO) as captured:
            parsed = cfg.read_package_config(self.write_config(OFFICIAL_CONFIG))
        dit = parsed.dit
        self.assertEqual((dit.hidden, dit.heads, dit.depth, dit.depth_single), (3072, 24, 5, 28))
        self.assertEqual(dit.head_dim, 128)
        self.assertEqual(dit.axes_dim, (32, 32, 32, 32))
        self.assertEqual(dit.mlp_ratio, 3.0)
        self.assertEqual(dit.theta, 10000.0)
        self.assertEqual(dict(dit.in_channels), {"image": 128, "image_cond": 128, "text": 20480})
        self.assertEqual(dit.image_rope_scale, 0.5)
        self.assertTrue(dit.flow_map)
        self.assertEqual(parsed.sampler, cfg.SamplerConfig(steps=16, alpha=6.93, default_size=(1024, 1024)))
        self.assertEqual(parsed.text_layers, cfg.TEXT_OUTPUT_LAYERS)
        self.assertEqual(parsed.pad_multiple, cfg.TEXT_PAD_MULTIPLE)
        self.assertEqual(parsed.declared_max_tokens, 8192)
        self.assertIsNone(parsed.quantization)
        self.assertIs(parsed.declared_format, cfg.WeightFormat.BF16)
        # The declared 8192 is ignored in favour of the reference cap, and said so.
        self.assertEqual(len(captured.records), 1)
        self.assertIn("8192", captured.records[0].getMessage())
        self.assertIn(str(cfg.TEXT_MAX_TOKENS), captured.records[0].getMessage())

    def test_in_channels_is_read_only(self) -> None:
        parsed = cfg.read_package_config(self.write_config(OFFICIAL_CONFIG))
        with self.assertRaises(TypeError):
            parsed.dit.in_channels["image"] = 1  # type: ignore[index]

    def test_quantized_variants_declare_their_format(self) -> None:
        for fmt in (cfg.WeightFormat.FP8R, cfg.WeightFormat.NVFP4):
            with self.subTest(fmt=fmt):
                data = copy.deepcopy(OFFICIAL_CONFIG)
                data["quantization"] = fmt.value
                parsed = cfg.read_package_config(self.write_config(data, f"{fmt.value}.json"))
                self.assertEqual(parsed.quantization, fmt.value)
                self.assertIs(parsed.declared_format, fmt)

    def test_a_missing_file_is_file_not_found(self) -> None:
        with self.assertRaises(FileNotFoundError):
            cfg.read_package_config(self.root / "absent.json")

    def test_malformed_json_is_a_value_error(self) -> None:
        path = self.root / "config.json"
        path.write_text("{not json", encoding="utf-8")
        with self.assertRaises(ValueError):
            cfg.read_package_config(path)

    def test_invalid_configs_are_refused_naming_the_field(self) -> None:
        def mutate(section: str | None, key: str, value: Any) -> dict[str, Any]:
            data = copy.deepcopy(OFFICIAL_CONFIG)
            target = data if section is None else data[section]
            if value is _DELETE:
                del target[key]
            else:
                target[key] = value
            return data

        cases = {
            "format_version": mutate(None, "format_version", 2),
            "quantization": mutate(None, "quantization", "int4"),
            "«dit»": mutate(None, "dit", None),
            "dit.hidden": mutate("dit", "hidden", 0),
            "не делится": mutate("dit", "heads", 7),
            "axes_dim": mutate("dit", "axes_dim", [32, 32, 32]),
            "in_channels": mutate("dit", "in_channels", {"image": 128, "text": 20480}),
            "out_modes": mutate("dit", "out_modes", ["image", "depth"]),
            "vec_in_dim": mutate("dit", "vec_in_dim", 768),
            "flow_map": mutate("dit", "flow_map", False),
            "dit.mlp_ratio": mutate("dit", "mlp_ratio", _DELETE),
            "sampler.guidance": mutate("sampler", "guidance", 3.5),
            "sampler.size": mutate("sampler", "size", [1024]),
            "sampler.steps": mutate("sampler", "steps", True),
            "text_encoder.layers": mutate("text_encoder", "layers", [4, 8]),
            "text_encoder.pad_multiple": mutate("text_encoder", "pad_multiple", 64),
            "text_encoder.max_tokens": mutate("text_encoder", "max_tokens", -1),
        }
        for needle, data in cases.items():
            with self.subTest(needle=needle):
                with self.assertRaises(ValueError) as caught:
                    cfg.read_package_config(self.write_config(data))
                self.assertIn(needle, str(caught.exception))

    def test_the_flow_map_refusal_names_the_base_model(self) -> None:
        data = copy.deepcopy(OFFICIAL_CONFIG)
        data["dit"]["flow_map"] = False
        with self.assertRaises(ValueError) as caught:
            cfg.DitConfig.from_json_dict(data["dit"])
        self.assertIn("CFG", str(caught.exception))

    def test_max_tokens_equal_to_the_cap_or_absent_logs_nothing(self) -> None:
        for value in (cfg.TEXT_MAX_TOKENS, None):
            with self.subTest(value=value):
                data = copy.deepcopy(OFFICIAL_CONFIG)
                data["text_encoder"]["max_tokens"] = value
                with self.assertNoLogs(cfg.log, level=logging.INFO):
                    parsed = cfg.read_package_config(self.write_config(data))
                self.assertEqual(parsed.declared_max_tokens, value)


class DetectWeightFormatTests(unittest.TestCase):
    def test_the_three_exports_are_recognized(self) -> None:
        self.assertIs(cfg.detect_weight_format(BF16_HEADER), cfg.WeightFormat.BF16)
        self.assertIs(cfg.detect_weight_format(FP8R_HEADER), cfg.WeightFormat.FP8R)
        self.assertIs(cfg.detect_weight_format(NVFP4_HEADER), cfg.WeightFormat.NVFP4)

    def test_norm_scale_keys_are_not_quantization_keys(self) -> None:
        header = {"a.norm.scale": _entry("BF16"), "a.weight": _entry("BF16")}
        self.assertIs(cfg.detect_weight_format(header), cfg.WeightFormat.BF16)

    def test_mixed_or_unknown_layouts_are_refused(self) -> None:
        cases = {
            "fp8 weights without scales": {"a.weight": _entry("F8_E4M3")},
            "scales without fp8 weights": {"a.weight": _entry("BF16"), "a.weight_scale": _entry("F32")},
            "packed u8 without scale_2": {"a.weight": _entry("U8"), "a.weight_scale": _entry("F8_E4M3")},
            "fp8 and nvfp4 mixed": {**NVFP4_HEADER, "b.weight": _entry("F8_E4M3")},
            "a stray 8-bit tensor": {**BF16_HEADER, "a.bias": _entry("F8_E5M2")},
            "fp8r with input scales": {**FP8R_HEADER, "x.input_scale": _entry("F32")},
        }
        for name, header in cases.items():
            with self.subTest(name=name):
                with self.assertRaises(ValueError):
                    cfg.detect_weight_format(header)

    def test_the_refusal_names_the_source(self) -> None:
        source = Path("/fictional/model.safetensors")
        with self.assertRaises(ValueError) as caught:
            cfg.detect_weight_format({"a.weight": _entry("F8_E4M3")}, source=source)
        self.assertIn(str(source), str(caught.exception))


class RequireDeclaredFormatTests(_TempCase):
    def test_agreement_passes_and_disagreement_names_both(self) -> None:
        data = copy.deepcopy(OFFICIAL_CONFIG)
        data["quantization"] = "fp8r"
        parsed = cfg.read_package_config(self.write_config(data))
        cfg.require_declared_format(parsed, cfg.WeightFormat.FP8R)
        with self.assertRaises(ValueError) as caught:
            cfg.require_declared_format(parsed, cfg.WeightFormat.NVFP4)
        self.assertIn("fp8r", str(caught.exception))
        self.assertIn("nvfp4", str(caught.exception))


class ResolveModelFilesTests(_TempCase):
    def _touch(self, *parts: str) -> None:
        path = self.root.joinpath(*parts)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"")

    def _full_package(self) -> None:
        self._touch("model.safetensors")
        self._touch("config.json")
        self._touch("ae.safetensors")
        (self.root / "text_encoder").mkdir()
        for variant in ("fp8r", "nvfp4"):
            self._touch("variants", variant, "model.safetensors")
            self._touch("variants", variant, "config.json")

    def test_every_variant_resolves_in_a_full_package(self) -> None:
        self._full_package()
        bf16 = cfg.resolve_model_files(self.root, cfg.WeightFormat.BF16)
        self.assertEqual(bf16.dit_file, self.root / "model.safetensors")
        self.assertEqual(bf16.dit_config, self.root / "config.json")
        self.assertEqual(bf16.ae_file, self.root / "ae.safetensors")
        self.assertEqual(bf16.text_encoder_dir, self.root / "text_encoder")
        nvfp4 = cfg.resolve_model_files(self.root, cfg.WeightFormat.NVFP4)
        self.assertEqual(nvfp4.dit_file, self.root / "variants" / "nvfp4" / "model.safetensors")
        self.assertEqual(nvfp4.dit_config, self.root / "variants" / "nvfp4" / "config.json")
        self.assertEqual(nvfp4.ae_file, self.root / "ae.safetensors")
        self.assertEqual(
            cfg.variant_availability(self.root), {fmt: None for fmt in cfg.WeightFormat}
        )

    def test_missing_files_are_named(self) -> None:
        self._full_package()
        (self.root / "model.safetensors").unlink()
        (self.root / "text_encoder").rmdir()
        with self.assertRaises(FileNotFoundError) as caught:
            cfg.resolve_model_files(self.root, cfg.WeightFormat.BF16)
        message = str(caught.exception)
        self.assertIn(str(self.root / "model.safetensors"), message)
        self.assertIn(str(self.root / "text_encoder"), message)

    def test_availability_reports_each_variant_separately(self) -> None:
        self._full_package()
        (self.root / "model.safetensors").unlink()
        (self.root / "variants" / "fp8r" / "config.json").unlink()
        availability = cfg.variant_availability(self.root)
        self.assertIsNone(availability[cfg.WeightFormat.NVFP4])
        self.assertIn("model.safetensors", availability[cfg.WeightFormat.BF16] or "")
        self.assertIn("config.json", availability[cfg.WeightFormat.FP8R] or "")


if __name__ == "__main__":
    unittest.main()
