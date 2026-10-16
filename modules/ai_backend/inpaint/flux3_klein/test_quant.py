"""
File: modules/ai_backend/inpaint/flux3_klein/test_quant.py

Purpose:
Unit tests for `flux3_klein/quant.py`: fp8 rowwise and NVFP4 dequantization
(hand-built nibbles), the quantized linears' forward paths and refusals, the
`_scaled_mm` probe / compute resolution (never silent, never raising), and
the linear swap's format precedence.

Notes:
Real torch on the CPU (torch 2.12 has a CPU `_scaled_mm`; the SCALED_MM
comparisons skip where the probe says unsupported). Skipped where torch is
absent. No files are written.
"""

from __future__ import annotations

import importlib.util
import logging
import unittest
from unittest import mock

_HAS_TORCH = importlib.util.find_spec("torch") is not None

if _HAS_TORCH:
    import torch
    import torch.nn.functional as F
    from torch import nn

    from . import quant
    from .config import WeightFormat
    from .params import Fp8Compute


def _fp8_layer(in_features: int, out_features: int, seed: int = 0) -> quant.Fp8RowwiseLinear:
    """An `Fp8RowwiseLinear` holding a rowwise-quantized random weight."""
    generator = torch.Generator().manual_seed(seed)
    layer = quant.Fp8RowwiseLinear(in_features, out_features)
    layer.weight, layer.weight_scale = quant.quantize_rows(torch.randn(out_features, in_features, generator=generator))
    return layer


def _rel_error(a: torch.Tensor, b: torch.Tensor) -> float:
    """Relative L2 error of `a` against `b`."""
    return float((a.float() - b.float()).norm() / b.float().norm())


@unittest.skipUnless(_HAS_TORCH, "torch is not installed")
class DequantizeTests(unittest.TestCase):
    def test_quantize_rows_round_trips_within_e4m3_precision(self) -> None:
        value = torch.randn(8, 32, generator=torch.Generator().manual_seed(3))
        quantized, scale = quant.quantize_rows(value)
        self.assertEqual(quantized.dtype, torch.float8_e4m3fn)
        self.assertEqual(tuple(scale.shape), (8,))
        restored = quant.dequantize_fp8_rows(quantized, scale, torch.float32)
        self.assertLess(_rel_error(restored, value), 0.05)

    def test_fp8_rows_follow_the_manifest_convention(self) -> None:
        weight = torch.tensor([[1.0, -2.0], [0.5, 4.0]]).to(torch.float8_e4m3fn)
        scale = torch.tensor([2.0, 0.25])
        out = quant.dequantize_fp8_rows(weight, scale, torch.float32)
        self.assertTrue(torch.equal(out, torch.tensor([[2.0, -4.0], [0.125, 1.0]])))
        self.assertEqual(quant.dequantize_fp8_rows(weight, scale).dtype, torch.bfloat16)

    def test_fp8_rows_refuse_a_mismatched_scale(self) -> None:
        weight = torch.zeros(3, 4).to(torch.float8_e4m3fn)
        with self.assertRaises(ValueError):
            quant.dequantize_fp8_rows(weight, torch.ones(4))

    def test_nvfp4_unpacks_hand_built_nibbles(self) -> None:
        # Byte 0x21: low nibble 1 (+0.5) is element 0, high nibble 2 (+1.0) element 1.
        # Byte 0x9F: low 0xF (-6.0), high 0x9 (-0.5). Codes 0x7 = +6, 0x8 = -0.
        row = [0x21, 0x9F, 0x87, 0x43, 0x65, 0x00, 0x00, 0x00]
        packed = torch.tensor([row, row], dtype=torch.uint8)
        block_scale = torch.tensor([[2.0], [0.5]]).to(torch.float8_e4m3fn)
        global_scale = torch.tensor(0.5)
        out = quant.dequantize_nvfp4(packed, block_scale, global_scale, dtype=torch.float32)
        codes = [0.5, 1.0, -6.0, -0.5, 6.0, -0.0, 1.5, 2.0, 3.0, 4.0, 0, 0, 0, 0, 0, 0]
        expected = torch.tensor([[c * 2.0 * 0.5 for c in codes], [c * 0.5 * 0.5 for c in codes]])
        self.assertTrue(torch.equal(out, expected))

    def test_nvfp4_block_scale_applies_per_16_elements(self) -> None:
        packed = torch.full((1, 16), 0x22, dtype=torch.uint8)  # every element +1.0, K = 32
        block_scale = torch.tensor([[1.0, 4.0]]).to(torch.float8_e4m3fn)
        out = quant.dequantize_nvfp4(packed, block_scale, torch.tensor(1.0), dtype=torch.float32)
        self.assertTrue(torch.equal(out, torch.tensor([[1.0] * 16 + [4.0] * 16])))

    def test_nvfp4_chunking_matches_one_pass(self) -> None:
        generator = torch.Generator().manual_seed(5)
        packed = torch.randint(0, 256, (40, 32), generator=generator, dtype=torch.uint8)
        block_scale = (torch.rand(40, 4, generator=generator) + 0.1).to(torch.float8_e4m3fn)
        whole = quant.dequantize_nvfp4(packed, block_scale, torch.tensor(0.03))
        with mock.patch.object(quant, "_CHUNK_ELEMENTS", 64 * 3):  # 3 rows per chunk
            chunked = quant.dequantize_nvfp4(packed, block_scale, torch.tensor(0.03))
        self.assertTrue(torch.equal(whole, chunked))

    def test_nvfp4_refuses_bad_shapes(self) -> None:
        packed = torch.zeros(2, 8, dtype=torch.uint8)
        good_scale = torch.ones(2, 1).to(torch.float8_e4m3fn)
        with self.assertRaises(ValueError):
            quant.dequantize_nvfp4(packed, torch.ones(2, 2).to(torch.float8_e4m3fn), torch.tensor(1.0))
        with self.assertRaises(ValueError):
            quant.dequantize_nvfp4(packed.to(torch.int8), good_scale, torch.tensor(1.0))
        with self.assertRaises(ValueError):
            quant.dequantize_nvfp4(packed, good_scale, torch.ones(2))


@unittest.skipUnless(_HAS_TORCH, "torch is not installed")
class QuantizedLinearTests(unittest.TestCase):
    def test_auto_compute_is_refused_at_forward(self) -> None:
        layer = _fp8_layer(32, 16)
        with self.assertRaises(RuntimeError):
            layer(torch.randn(4, 32))

    def test_dequant_forward_is_a_bf16_linear_over_the_dequantized_weight(self) -> None:
        layer = _fp8_layer(32, 16)
        layer.compute = Fp8Compute.DEQUANT
        x = torch.randn(2, 3, 32)
        out = layer(x)
        self.assertEqual(out.dtype, torch.bfloat16)
        self.assertEqual(tuple(out.shape), (2, 3, 16))
        expected = F.linear(x.to(torch.bfloat16), quant.dequantize_fp8_rows(layer.weight, layer.weight_scale))
        self.assertTrue(torch.equal(out, expected))

    def test_scaled_mm_matches_dequant_within_tolerance_with_padding(self) -> None:
        if not quant.probe_scaled_mm("cpu").supported:
            self.skipTest("rowwise _scaled_mm is not supported on this CPU build")
        # 40 x 24 is not aligned to 256 / 128: exercises the reference padding.
        layer = _fp8_layer(40, 24, seed=7)
        x = torch.randn(5, 40, generator=torch.Generator().manual_seed(8))
        layer.compute = Fp8Compute.DEQUANT
        reference = layer(x)
        layer.compute = Fp8Compute.SCALED_MM
        fast = layer(x)
        self.assertEqual(fast.dtype, torch.bfloat16)
        self.assertEqual(tuple(fast.shape), (5, 24))
        self.assertLess(_rel_error(fast, reference), 0.05)

    def test_empty_input_gives_an_empty_bf16_output(self) -> None:
        layer = _fp8_layer(32, 16)
        layer.compute = Fp8Compute.SCALED_MM
        out = layer(torch.zeros(0, 32))
        self.assertEqual((tuple(out.shape), out.dtype), ((0, 16), torch.bfloat16))

    def test_nvfp4_forward_uses_the_dequantized_weight(self) -> None:
        generator = torch.Generator().manual_seed(9)
        layer = quant.Nvfp4Linear(32, 8)
        layer.weight = torch.randint(0, 256, (8, 16), generator=generator, dtype=torch.uint8)
        layer.weight_scale = (torch.rand(8, 2, generator=generator) + 0.5).to(torch.float8_e4m3fn)
        layer.weight_scale_2 = torch.tensor(0.02)
        layer.input_scale = torch.tensor(1.0)
        x = torch.randn(3, 32, generator=generator)
        expected = F.linear(
            x.to(torch.bfloat16), quant.dequantize_nvfp4(layer.weight, layer.weight_scale, layer.weight_scale_2)
        )
        self.assertTrue(torch.equal(layer(x), expected))

    def test_nvfp4_refuses_in_features_off_the_block_grid(self) -> None:
        with self.assertRaises(ValueError):
            quant.Nvfp4Linear(24, 8)

    def test_set_fp8_compute_counts_layers_and_refuses_auto(self) -> None:
        model = nn.Sequential(_fp8_layer(16, 16), nn.Linear(16, 16), _fp8_layer(16, 16, seed=1))
        self.assertEqual(quant.set_fp8_compute(model, Fp8Compute.DEQUANT), 2)
        self.assertIs(model[0].compute, Fp8Compute.DEQUANT)
        with self.assertRaises(ValueError):
            quant.set_fp8_compute(model, Fp8Compute.AUTO)


@unittest.skipUnless(_HAS_TORCH, "torch is not installed")
class ProbeAndResolveTests(unittest.TestCase):
    def test_probe_never_raises_on_an_unusable_device(self) -> None:
        probe = quant.probe_scaled_mm("no_such_device:3")
        self.assertFalse(probe.supported)
        self.assertTrue(probe.reason)
        self.assertIsNone(probe.rel_error)

    def test_probe_is_cached_per_device_string(self) -> None:
        self.assertIs(quant.probe_scaled_mm("cpu"), quant.probe_scaled_mm("cpu"))

    def test_a_definitive_failure_is_cached(self) -> None:
        self.addCleanup(quant._PROBE_CACHE.pop, "no_such_device:7", None)
        first = quant.probe_scaled_mm("no_such_device:7")
        self.assertFalse(first.transient)
        self.assertIs(quant.probe_scaled_mm("no_such_device:7"), first)

    def test_an_out_of_memory_failure_is_not_cached(self) -> None:
        key = "cpu:0"
        self.addCleanup(quant._PROBE_CACHE.pop, key, None)
        quant._PROBE_CACHE.pop(key, None)
        with mock.patch.object(quant, "quantize_rows", side_effect=torch.OutOfMemoryError("HIP out of memory")):
            with self.assertLogs(quant.log, "WARNING"):
                transient = quant.probe_scaled_mm(key)
        self.assertFalse(transient.supported)
        self.assertTrue(transient.transient)
        self.assertNotIn(key, quant._PROBE_CACHE)
        again = quant.probe_scaled_mm(key)
        self.assertFalse(again.transient)
        self.assertIsNotNone(again.rel_error if again.supported else again.reason)
        self.assertIs(quant.probe_scaled_mm(key), again)

    def test_auto_with_a_failing_probe_warns_and_dequantizes(self) -> None:
        failing = quant.ScaledMmProbe(supported=False, reason="нет ядра", rel_error=None)
        with mock.patch.object(quant, "probe_scaled_mm", return_value=failing):
            with self.assertLogs(quant.log, level=logging.WARNING) as logs:
                self.assertIs(quant.resolve_fp8_compute(Fp8Compute.AUTO, "cuda:0"), Fp8Compute.DEQUANT)
        self.assertIn("нет ядра", logs.output[0])

    def test_auto_with_a_passing_probe_takes_scaled_mm(self) -> None:
        passing = quant.ScaledMmProbe(supported=True, reason="", rel_error=0.02)
        with mock.patch.object(quant, "probe_scaled_mm", return_value=passing):
            self.assertIs(quant.resolve_fp8_compute(Fp8Compute.AUTO, "cuda:0"), Fp8Compute.SCALED_MM)

    def test_explicit_scaled_mm_with_a_failing_probe_is_refused(self) -> None:
        failing = quant.ScaledMmProbe(supported=False, reason="нет ядра", rel_error=None)
        with mock.patch.object(quant, "probe_scaled_mm", return_value=failing):
            with self.assertRaises(RuntimeError) as caught:
                quant.resolve_fp8_compute(Fp8Compute.SCALED_MM, "cuda:0")
        self.assertIn("нет ядра", str(caught.exception))

    def test_dequant_never_probes(self) -> None:
        with mock.patch.object(quant, "probe_scaled_mm") as probe:
            self.assertIs(quant.resolve_fp8_compute(Fp8Compute.DEQUANT, "cuda:0"), Fp8Compute.DEQUANT)
        probe.assert_not_called()


@unittest.skipUnless(_HAS_TORCH, "torch is not installed")
class SwapTests(unittest.TestCase):
    def _model(self) -> nn.Module:
        with torch.device("meta"):
            return nn.ModuleDict({"a": nn.Linear(32, 16, bias=False), "b": nn.Linear(32, 16, bias=False)})

    def test_nvfp4_takes_precedence_over_fp8(self) -> None:
        model = self._model()
        keys = {"a.weight", "a.weight_scale", "a.weight_scale_2", "a.input_scale", "b.weight"}
        self.assertIs(quant.swap_quantized_linears(model, keys), WeightFormat.NVFP4)
        self.assertIsInstance(model["a"], quant.Nvfp4Linear)
        self.assertIsInstance(model["b"], nn.Linear)
        self.assertTrue(model["a"].weight.is_meta)

    def test_fp8_scales_swap_to_rowwise_layers(self) -> None:
        model = self._model()
        self.assertIs(quant.swap_quantized_linears(model, {"a.weight_scale", "b.weight_scale"}), WeightFormat.FP8R)
        self.assertIsInstance(model["a"], quant.Fp8RowwiseLinear)
        self.assertIsInstance(model["b"], quant.Fp8RowwiseLinear)

    def test_no_scale_key_is_bf16_and_changes_nothing(self) -> None:
        model = self._model()
        self.assertIs(quant.swap_quantized_linears(model, {"a.weight", "b.weight"}), WeightFormat.BF16)
        self.assertIsInstance(model["a"], nn.Linear)

    def test_a_biased_quantized_linear_is_refused(self) -> None:
        with torch.device("meta"):
            model = nn.ModuleDict({"a": nn.Linear(32, 16)})
        with self.assertRaises(ValueError):
            quant.swap_quantized_linears(model, {"a.weight_scale"})


if __name__ == "__main__":
    unittest.main()
