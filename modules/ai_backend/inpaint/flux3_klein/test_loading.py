"""
File: modules/ai_backend/inpaint/flux3_klein/test_loading.py

Purpose:
Unit tests for `flux3_klein/loading.py`: synthetic bf16 / fp8r / nvfp4
checkpoints of the tiny DiT are written to a temp directory, loaded on the CPU,
and their forward compared with a bf16 reference module holding the
dequantized weights; plus every refusal (CFG checkpoint, missing / unexpected
key, declared vs detected vs requested format, mis-typed quantized payload,
explicit `SCALED_MM` that cannot run) and the progress callback. The rule
conversion of the large-model spec (`convert_dit_tensor`) and the meta build
are pinned in `test_large_model.py`.

Notes:
Real torch + safetensors on the CPU; skipped where either is absent. Temp files
live under the system temp directory and are removed by each test.
"""

from __future__ import annotations

import copy
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

from .config import ModelFiles, WeightFormat, read_package_config
from .params import Fp8Compute

_HAS_TORCH = importlib.util.find_spec("torch") is not None and importlib.util.find_spec("safetensors") is not None

if _HAS_TORCH:
    import torch
    from safetensors.torch import save_file
    from torch import nn

    from . import loading, quant
    from .dit import Flux3Dit
    from .test_dit import make_pack, tiny_dit_config


def _config_json(quantization: str | None) -> dict[str, Any]:
    """A `config.json` for the tiny DiT (`tiny_dit_config`)."""
    return {
        "format_version": 1,
        "quantization": quantization,
        "dit": {
            "hidden": 64,
            "heads": 2,
            "depth": 1,
            "depth_single": 2,
            "mlp_ratio": 2.0,
            "axes_dim": [8, 8, 8, 8],
            "theta": 10000,
            "in_channels": {"image": 16, "image_cond": 16, "text": 48},
            "out_modes": ["image"],
            "vec_in_dim": None,
            "image_rope_scale": 0.5,
            "flow_map": True,
        },
        "text_encoder": {"layers": [4, 8, 12, 16, 20, 24, 28, 32], "pad_multiple": 80, "max_tokens": 8192},
        "sampler": {"steps": 16, "guidance": None, "alpha": 6.93, "size": [1024, 1024]},
    }


def _source_model() -> Flux3Dit:
    """The tiny DiT in bf16 with seeded weights large enough to matter."""
    torch.manual_seed(11)
    model = Flux3Dit(tiny_dit_config())
    with torch.no_grad():
        for parameter in model.parameters():
            parameter.normal_(std=0.2)
    return model.to(torch.bfloat16).eval()


def _block_linears(model: nn.Module, prefix: str) -> list[str]:
    """Names of the `nn.Linear`s under `prefix` (e.g. `single_blocks.`)."""
    return [name for name, child in model.named_modules() if name.startswith(prefix) and isinstance(child, nn.Linear)]


def _fp8r_checkpoint(model: Flux3Dit) -> tuple[dict[str, torch.Tensor], Flux3Dit]:
    """fp8r state of `model` (every block linear quantized) and the bf16 reference holding the dequantized weights."""
    state = {key: value.clone() for key, value in model.state_dict().items()}
    reference = copy.deepcopy(model)
    for name in _block_linears(model, "content_mode_blocks.") + _block_linears(model, "single_blocks."):
        weight, scale = quant.quantize_rows(state[f"{name}.weight"].float())
        state[f"{name}.weight"], state[f"{name}.weight_scale"] = weight, scale
        reference.get_submodule(name).weight.data = quant.dequantize_fp8_rows(weight, scale)
    return state, reference


def _nvfp4_checkpoint(model: Flux3Dit) -> tuple[dict[str, torch.Tensor], Flux3Dit]:
    """nvfp4 state of `model` (single-block linears replaced by random NVFP4 weights) and its bf16 reference."""
    generator = torch.Generator().manual_seed(12)
    state = {key: value.clone() for key, value in model.state_dict().items()}
    reference = copy.deepcopy(model)
    for name in _block_linears(model, "single_blocks."):
        linear = model.get_submodule(name)
        out_features, in_features = linear.out_features, linear.in_features
        packed = torch.randint(0, 256, (out_features, in_features // 2), generator=generator, dtype=torch.uint8)
        block_scale = (torch.rand(out_features, in_features // 16, generator=generator) * 0.1 + 0.02).to(torch.float8_e4m3fn)
        global_scale = torch.tensor(0.5)
        state[f"{name}.weight"] = packed
        state[f"{name}.weight_scale"] = block_scale
        state[f"{name}.weight_scale_2"] = global_scale
        state[f"{name}.input_scale"] = torch.tensor(1.0)
        reference.get_submodule(name).weight.data = quant.dequantize_nvfp4(packed, block_scale, global_scale)
    return state, reference


@unittest.skipUnless(_HAS_TORCH, "torch or safetensors is not installed")
class LoadDitTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="flux3_dit_load_")
        self.root = Path(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def _write(self, state: dict[str, torch.Tensor], quantization: str | None) -> tuple[ModelFiles, Any]:
        """Write `state` + a matching `config.json`; return the files and the parsed config."""
        dit_file = self.root / "model.safetensors"
        config_file = self.root / "config.json"
        save_file({key: value.contiguous() for key, value in state.items()}, str(dit_file))
        config_file.write_text(json.dumps(_config_json(quantization)), encoding="utf-8")
        files = ModelFiles(
            dit_file=dit_file,
            dit_config=config_file,
            ae_file=self.root / "ae.safetensors",
            text_encoder_dir=self.root / "text_encoder",
        )
        return files, read_package_config(config_file)

    def _forward(self, model: nn.Module) -> torch.Tensor:
        pack = make_pack(tiny_dit_config(), [{"image": 6, "image_cond": 4, "text": 5}], dtype=torch.bfloat16, seed=4)
        with torch.inference_mode():
            return model(pack, torch.tensor([0.25]))

    def test_bf16_checkpoint_reproduces_the_source_model(self) -> None:
        model = _source_model()
        files, config = self._write(model.state_dict(), None)
        progress: list[tuple[int, int, str]] = []
        loaded = loading.load_dit(
            files,
            config,
            variant=WeightFormat.BF16,
            device="cpu",
            fp8_compute=Fp8Compute.AUTO,
            progress=lambda done, total, key: progress.append((done, total, key)),
        )
        self.assertIs(loaded.format, WeightFormat.BF16)
        self.assertIsNone(loaded.fp8_compute_applied)
        self.assertFalse(loaded.dit.training)
        self.assertGreater(loaded.bytes_read, 0)
        self.assertEqual(progress[-1][0], progress[-1][1])
        self.assertTrue(torch.equal(self._forward(loaded.dit), self._forward(model)))

    def test_float32_tensors_become_bf16(self) -> None:
        model = _source_model()
        files, config = self._write({k: v.float() for k, v in model.state_dict().items()}, None)
        loaded = loading.load_dit(files, config, variant=WeightFormat.BF16, device="cpu", fp8_compute=Fp8Compute.AUTO)
        self.assertEqual({p.dtype for p in loaded.dit.parameters()}, {torch.bfloat16})

    def test_fp8r_dequant_matches_the_dequantized_reference(self) -> None:
        state, reference = _fp8r_checkpoint(_source_model())
        files, config = self._write(state, "fp8r")
        loaded = loading.load_dit(files, config, variant=WeightFormat.FP8R, device="cpu", fp8_compute=Fp8Compute.DEQUANT)
        self.assertIs(loaded.fp8_compute_applied, Fp8Compute.DEQUANT)
        layer = loaded.dit.get_submodule("single_blocks.0.q_proj")
        self.assertIsInstance(layer, quant.Fp8RowwiseLinear)
        self.assertEqual((layer.weight.dtype, layer.weight_scale.dtype), (torch.float8_e4m3fn, torch.float32))
        self.assertIsInstance(loaded.dit.get_submodule("txt_in"), nn.Linear)
        self.assertTrue(torch.equal(self._forward(loaded.dit), self._forward(reference)))

    def test_fp8r_scaled_mm_stays_close_to_the_reference(self) -> None:
        if not quant.probe_scaled_mm("cpu").supported:
            self.skipTest("rowwise _scaled_mm is not supported on this CPU build")
        state, reference = _fp8r_checkpoint(_source_model())
        files, config = self._write(state, "fp8r")
        loaded = loading.load_dit(files, config, variant=WeightFormat.FP8R, device="cpu", fp8_compute=Fp8Compute.AUTO)
        self.assertIs(loaded.fp8_compute_applied, Fp8Compute.SCALED_MM)
        fast, expected = self._forward(loaded.dit).float(), self._forward(reference).float()
        self.assertLess(float((fast - expected).norm() / expected.norm()), 0.1)

    def test_nvfp4_matches_the_dequantized_reference(self) -> None:
        state, reference = _nvfp4_checkpoint(_source_model())
        files, config = self._write(state, "nvfp4")
        loaded = loading.load_dit(files, config, variant=WeightFormat.NVFP4, device="cpu", fp8_compute=Fp8Compute.AUTO)
        self.assertIs(loaded.format, WeightFormat.NVFP4)
        self.assertIsNone(loaded.fp8_compute_applied)
        layer = loaded.dit.get_submodule("single_blocks.1.mlp_in")
        self.assertIsInstance(layer, quant.Nvfp4Linear)
        self.assertEqual(layer.weight.dtype, torch.uint8)
        self.assertTrue(torch.equal(self._forward(loaded.dit), self._forward(reference)))

    def _load(self, files: ModelFiles, config: Any, variant: WeightFormat = WeightFormat.BF16) -> Any:
        return loading.load_dit(files, config, variant=variant, device="cpu", fp8_compute=Fp8Compute.DEQUANT)

    def test_cfg_checkpoint_is_refused(self) -> None:
        state = {k: v for k, v in _source_model().state_dict().items() if not k.startswith("ref_time_in.")}
        files, config = self._write(state, None)
        with self.assertRaisesRegex(ValueError, "CFG"):
            self._load(files, config)

    def test_missing_key_is_refused(self) -> None:
        state = dict(_source_model().state_dict())
        del state["txt_in.weight"]
        files, config = self._write(state, None)
        with self.assertRaisesRegex(ValueError, "txt_in.weight"):
            self._load(files, config)

    def test_unexpected_key_is_refused(self) -> None:
        state = dict(_source_model().state_dict())
        state["repa_projectors.0.weight"] = torch.zeros(2, 2, dtype=torch.bfloat16)
        files, config = self._write(state, None)
        with self.assertRaisesRegex(ValueError, "repa_projectors"):
            self._load(files, config)

    def test_requested_variant_must_match_the_file(self) -> None:
        files, config = self._write(_source_model().state_dict(), None)
        with self.assertRaisesRegex(ValueError, "fp8r"):
            self._load(files, config, WeightFormat.FP8R)

    def test_declared_format_must_match_the_file(self) -> None:
        files, config = self._write(_source_model().state_dict(), "fp8r")
        with self.assertRaisesRegex(ValueError, "config.json"):
            self._load(files, config, WeightFormat.FP8R)

    def test_quantized_payload_in_the_wrong_dtype_is_refused(self) -> None:
        state, _ = _fp8r_checkpoint(_source_model())
        state["single_blocks.0.q_proj.weight_scale"] = state["single_blocks.0.q_proj.weight_scale"].to(torch.bfloat16)
        files, config = self._write(state, "fp8r")
        with self.assertRaisesRegex(ValueError, "single_blocks.0.q_proj.weight_scale"):
            self._load(files, config, WeightFormat.FP8R)

    def test_explicit_scaled_mm_that_cannot_run_fails_before_reading(self) -> None:
        state, _ = _fp8r_checkpoint(_source_model())
        files, config = self._write(state, "fp8r")
        failing = quant.ScaledMmProbe(supported=False, reason="нет ядра", rel_error=None)
        with mock.patch.object(quant, "probe_scaled_mm", return_value=failing), mock.patch.object(
            loading, "fill_module_from_safetensors"
        ) as fill:
            with self.assertRaises(RuntimeError):
                loading.load_dit(files, config, variant=WeightFormat.FP8R, device="cpu", fp8_compute=Fp8Compute.SCALED_MM)
        fill.assert_not_called()


if __name__ == "__main__":
    unittest.main()
