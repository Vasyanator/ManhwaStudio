"""
File: modules/ai_backend/inpaint/flux3_klein/test_real_headers_dit.py

Purpose:
Opt-in check of the real FLUX.3 klein DiT exports against this port: for every
variant present under `MS_FLUX3_DIR`, the meta-built `Flux3Dit` of that
variant's `config.json`, after the quantized-linear swap, must have exactly the
header's key set, shapes and (for quantized payloads) dtypes, and the header's
format must be the declared one.

Notes:
Reads only `config.json` files and safetensors HEADERS: no tensor byte is read
and no GPU is used. Skipped unless `MS_FLUX3_DIR` names the package root (the
directory holding `ae.safetensors` and `variants/`), and per variant when its
files are absent.
"""

from __future__ import annotations

import importlib.util
import os
import unittest
from pathlib import Path

from ...runtime.safetensors_stream import SAFETENSORS_DTYPES_WITH_F8, parse_tensor_spans, read_safetensors_header
from .config import WeightFormat, detect_weight_format, read_package_config, require_declared_format

_HAS_TORCH = importlib.util.find_spec("torch") is not None

if _HAS_TORCH:
    import torch

    from . import loading, quant
    from .dit import Flux3Dit

_ROOT_ENV = "MS_FLUX3_DIR"


def _variant_dir(root: Path, variant: WeightFormat) -> Path:
    """Where `variant`'s `model.safetensors` + `config.json` live (package layout, `config.py` header)."""
    return root if variant is WeightFormat.BF16 else root / "variants" / variant.value


@unittest.skipUnless(_HAS_TORCH, "torch is not installed")
class RealDitHeaderTests(unittest.TestCase):
    def setUp(self) -> None:
        value = os.environ.get(_ROOT_ENV)
        if not value:
            self.skipTest(f"{_ROOT_ENV} is not set")
        self.root = Path(value)
        if not self.root.is_dir():
            self.skipTest(f"{_ROOT_ENV}={value} is not a directory")

    def _check_variant(self, variant: WeightFormat) -> None:
        directory = _variant_dir(self.root, variant)
        dit_file, config_file = directory / "model.safetensors", directory / "config.json"
        if not (dit_file.is_file() and config_file.is_file()):
            self.skipTest(f"{variant.value}: {dit_file} or {config_file} is absent")
        config = read_package_config(config_file)
        header = read_safetensors_header(dit_file)
        spans = {span.name: span for span in parse_tensor_spans(header, source=dit_file, dtypes=SAFETENSORS_DTYPES_WITH_F8)}

        loading.require_flow_map(header)
        detected = detect_weight_format(header, source=dit_file)
        require_declared_format(config, detected)
        self.assertIs(detected, variant)

        with torch.device("meta"):
            model = Flux3Dit(config.dit)
        self.assertIs(quant.swap_quantized_linears(model, header.keys()), variant)
        expected = model.state_dict()
        self.assertEqual(sorted(set(expected) - set(spans)), [], "keys the model needs but the file lacks")
        self.assertEqual(sorted(set(spans) - set(expected)), [], "keys the file has but the model lacks")
        wrong_shapes = [
            f"{key}: file {spans[key].shape} model {tuple(value.shape)}"
            for key, value in expected.items()
            if spans[key].shape != tuple(value.shape)
        ]
        self.assertEqual(wrong_shapes, [])

        # The loader's cast rule must accept every quantized payload as stored.
        quantized = loading._quantized_buffer_dtypes(model)
        wrong_dtypes = [
            f"{key}: file {spans[key].torch_dtype_name} model {dtype}"
            for key, dtype in quantized.items()
            if getattr(torch, spans[key].torch_dtype_name) != dtype
        ]
        self.assertEqual(wrong_dtypes, [])
        others = {spans[key].dtype_token for key in spans if key not in quantized}
        self.assertLessEqual(others, {"BF16", "F16", "F32"})
        if variant is WeightFormat.BF16:
            self.assertEqual(quantized, {})

    def test_bf16(self) -> None:
        self._check_variant(WeightFormat.BF16)

    def test_fp8r(self) -> None:
        self._check_variant(WeightFormat.FP8R)

    def test_nvfp4(self) -> None:
        self._check_variant(WeightFormat.NVFP4)


if __name__ == "__main__":
    unittest.main()
