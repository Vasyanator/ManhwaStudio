"""
File: modules/ai_backend/runtime/test_gguf_layers.py

Purpose:
Unit tests for `runtime.gguf_layers`: numeric parity of `dequantize` with
diffusers (bit-exact) and with `gguf.quants.dequantize` (fp16 tolerance), the
`GgufLinear` / `GgufEmbedding` forwards, their survival through the
large-model manager's entry handling (`TensorSlot.set`, `packed_view`, `meta`)
and a `state_dict` round trip, and every refusal of `swap_gguf_modules`.

Notes:
- Quantized blocks are random bytes with every fp16 scale field overwritten by
  a finite value, built inline (no fixture files, no GPU, no network).
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

_MODULE_DIR = Path(__file__).resolve().parent
# `modules/ai_backend/runtime` -> parents[2] = repo root.
_PROJECT_ROOT = _MODULE_DIR.parents[2]
if str(_PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(_PROJECT_ROOT))

import gguf  # noqa: E402
import numpy as np  # noqa: E402
import torch  # noqa: E402
import torch.nn.functional as F  # noqa: E402
from diffusers.quantizers.gguf.utils import GGUFLinear, GGUFParameter, dequantize_gguf_tensor  # noqa: E402
from gguf import quants  # noqa: E402
from torch import nn  # noqa: E402

from modules.ai_backend.runtime import gguf_layers  # noqa: E402
from modules.ai_backend.runtime.gguf_layers import (  # noqa: E402
    GgufEmbedding,
    GgufLinear,
    bytes_per_row,
    dequantizable_types,
    dequantize,
    require_dequantizable,
    swap_gguf_modules,
)
from modules.ai_backend.runtime.large_models.loader import TensorSlot, packed_layout, packed_view  # noqa: E402

Q = gguf.GGMLQuantizationType

#: Byte offsets of every fp16 scale field inside one block, per type
#: (ggml block layouts: Q8_0/Q4_0/Q5_0 `d` first; Q4_K/Q5_K `d`, `dmin` first;
#: Q6_K `d` last after ql[128], qh[64], scales[16]).
_FP16_FIELDS: dict[int, tuple[int, ...]] = {
    Q.Q8_0: (0,),
    Q.Q4_0: (0,),
    Q.Q5_0: (0,),
    Q.Q4_K: (0, 2),
    Q.Q5_K: (0, 2),
    Q.Q6_K: (208,),
}


def _random_rows(quant_type: int, rows: int, row_elements: int, seed: int = 0) -> torch.Tensor:
    """Random packed rows `(rows, bytes_per_row)` with finite fp16 scales in [-0.5, 0.5]."""
    rng = np.random.default_rng(seed)
    block_size, type_size = gguf.GGML_QUANT_SIZES[quant_type]
    n_blocks = rows * row_elements // block_size
    blocks = rng.integers(0, 256, size=(n_blocks, type_size), dtype=np.uint8)
    for offset in _FP16_FIELDS[quant_type]:
        scales = rng.uniform(-0.5, 0.5, size=n_blocks).astype(np.float16)
        blocks[:, offset : offset + 2] = scales.view(np.uint8).reshape(n_blocks, 2)
    return torch.from_numpy(blocks.reshape(rows, -1).copy())


class DequantizeParityTests(unittest.TestCase):
    def test_bit_exact_with_diffusers_and_close_to_gguf_numpy(self) -> None:
        for quant_type in _FP16_FIELDS:
            with self.subTest(quant_type=Q(quant_type).name):
                row_elements = 512
                rows = _random_rows(quant_type, 6, row_elements, seed=int(quant_type))
                ours = dequantize(rows, quant_type, row_elements=row_elements)
                theirs = dequantize_gguf_tensor(GGUFParameter(rows.clone(), quant_type=quant_type))
                self.assertEqual(ours.dtype, theirs.dtype)
                self.assertEqual(tuple(ours.shape), (6, row_elements))
                self.assertTrue(torch.equal(ours, theirs))
                reference = torch.from_numpy(quants.dequantize(rows.numpy(), quant_type))
                self.assertTrue(torch.isfinite(reference).all())
                scale = float(reference.abs().max())
                torch.testing.assert_close(ours.float(), reference, rtol=1e-2, atol=1e-3 * scale)

    def test_leading_dims_are_kept(self) -> None:
        rows = _random_rows(Q.Q8_0, 6, 64)
        flat = dequantize(rows, Q.Q8_0, row_elements=64)
        nested = dequantize(rows.reshape(2, 3, -1), Q.Q8_0, row_elements=64)
        self.assertTrue(torch.equal(nested.reshape(6, 64), flat))

    def test_refuses_wrong_dtype_and_row_length(self) -> None:
        rows = _random_rows(Q.Q8_0, 2, 64)
        with self.assertRaisesRegex(ValueError, "uint8"):
            dequantize(rows.to(torch.int16), Q.Q8_0, row_elements=64)
        with self.assertRaisesRegex(ValueError, "do not end in"):
            dequantize(rows, Q.Q8_0, row_elements=96)

    def test_bytes_per_row(self) -> None:
        self.assertEqual(bytes_per_row(Q.Q8_0, 64), 68)
        self.assertEqual(bytes_per_row(Q.Q4_K, 512), 288)
        self.assertEqual(bytes_per_row(Q.Q6_K, 256), 210)
        with self.assertRaisesRegex(ValueError, "block size 32"):
            bytes_per_row(Q.Q8_0, 40)

    def test_supported_set_and_iq2_refusal(self) -> None:
        supported = dequantizable_types()
        for quant_type in _FP16_FIELDS:
            self.assertIn(int(quant_type), supported)
        self.assertNotIn(int(Q.IQ2_XXS), supported)
        with self.assertRaisesRegex(ValueError, r"blk\.0\.w.*IQ2_XXS.*Q8_0"):
            require_dequantizable(Q.IQ2_XXS, where="blk.0.w")
        with self.assertRaisesRegex(ValueError, "unknown\\(9999\\)"):
            require_dequantizable(9999, where="x")


def _filled_linear(quant_type: int, in_features: int, out_features: int, dtype: torch.dtype, seed: int = 1) -> GgufLinear:
    layer = GgufLinear(in_features, out_features, quant_type=quant_type, compute_dtype=dtype, bias=True)
    layer.weight.copy_(_random_rows(quant_type, out_features, in_features, seed=seed))
    with torch.no_grad():
        layer.bias.copy_(torch.randn(out_features, generator=torch.Generator().manual_seed(seed)).to(dtype))
    return layer


class GgufLinearTests(unittest.TestCase):
    def test_forward_matches_diffusers_forward_native(self) -> None:
        for dtype in (torch.bfloat16, torch.float16):
            for quant_type in (Q.Q8_0, Q.Q4_K):
                with self.subTest(dtype=dtype, quant_type=Q(quant_type).name):
                    layer = _filled_linear(quant_type, 256, 12, dtype)
                    x = torch.randn(3, 5, 256, generator=torch.Generator().manual_seed(7)).to(dtype)
                    ours = layer(x)
                    reference = GGUFLinear(256, 12, bias=True, compute_dtype=dtype)
                    reference.weight = GGUFParameter(layer.weight.clone(), quant_type=quant_type)
                    reference.bias = nn.Parameter(layer.bias.detach().clone(), requires_grad=False)
                    expected = reference.forward_native(x)
                    self.assertEqual(ours.dtype, dtype)
                    self.assertEqual(tuple(ours.shape), (3, 5, 12))
                    self.assertTrue(torch.equal(ours, expected))

    def test_bias_free_layer(self) -> None:
        layer = GgufLinear(64, 4, quant_type=Q.Q8_0, compute_dtype=torch.bfloat16, bias=False)
        layer.weight.copy_(_random_rows(Q.Q8_0, 4, 64))
        self.assertIsNone(layer.bias)
        self.assertEqual(set(layer.state_dict()), {"weight"})
        x = torch.randn(2, 64).to(torch.bfloat16)
        weight = dequantize(layer.weight, Q.Q8_0, row_elements=64).to(torch.bfloat16)
        self.assertTrue(torch.equal(layer(x), F.linear(x, weight)))
        self.assertIn("Q8_0", repr(layer))

    def test_state_dict_round_trip_keeps_weight_buffer_and_quant_type(self) -> None:
        layer = _filled_linear(Q.Q5_K, 256, 6, torch.bfloat16)
        state = layer.state_dict()
        self.assertEqual(set(state), {"weight", "bias"})
        self.assertEqual(state["weight"].dtype, torch.uint8)
        clone = GgufLinear(256, 6, quant_type=Q.Q5_K, compute_dtype=torch.bfloat16, bias=True)
        clone.load_state_dict(state, strict=True)
        self.assertEqual(clone.quant_type, int(Q.Q5_K))
        x = torch.randn(2, 256).to(torch.bfloat16)
        self.assertTrue(torch.equal(clone(x), layer(x)))

    def test_survives_tensor_slot_set_packed_view_and_meta(self) -> None:
        layer = _filled_linear(Q.Q6_K, 256, 5, torch.bfloat16)
        x = torch.randn(2, 256).to(torch.bfloat16)
        expected = layer(x)
        slots = [
            TensorSlot("weight", layer, "weight", False, tuple(layer.weight.shape), torch.uint8),
            TensorSlot("bias", layer, "bias", True, tuple(layer.bias.shape), torch.bfloat16),
        ]
        originals = [slot.get().detach().clone() for slot in slots]
        offsets, total = packed_layout(slots)
        buffer = torch.zeros(total, dtype=torch.uint8)
        # Park on meta first, as a DISK unit is, then restore into a packed host buffer.
        for slot in slots:
            slot.set(torch.empty(slot.shape, dtype=slot.dtype, device="meta"))
        self.assertTrue(layer.weight.is_meta)
        for slot, offset, original in zip(slots, offsets, originals):
            view = packed_view(buffer, offset, slot)
            view.copy_(original)
            slot.set(view)
        self.assertIn("weight", layer._buffers)
        self.assertEqual(layer.weight.dtype, torch.uint8)
        self.assertEqual(layer.quant_type, int(Q.Q6_K))
        self.assertTrue(torch.equal(layer(x), expected))


class GgufEmbeddingTests(unittest.TestCase):
    def _table(self, quant_type: int = Q.Q8_0) -> GgufEmbedding:
        table = GgufEmbedding(40, 64, quant_type=quant_type, compute_dtype=torch.bfloat16, padding_idx=0)
        table.weight.copy_(_random_rows(quant_type, 40, 64, seed=3))
        return table

    def test_matches_f_embedding_with_repeats(self) -> None:
        for quant_type in (Q.Q8_0, Q.Q4_0, Q.Q5_0):
            with self.subTest(quant_type=Q(quant_type).name):
                table = self._table(quant_type)
                ids = torch.tensor([[3, 3, 39, 0], [7, 3, 12, 39]])
                full = dequantize(table.weight, quant_type, row_elements=64).to(torch.bfloat16)
                out = table(ids)
                self.assertEqual(tuple(out.shape), (2, 4, 64))
                self.assertEqual(out.dtype, torch.bfloat16)
                self.assertTrue(torch.equal(out, F.embedding(ids, full)))

    def test_only_looked_up_rows_are_dequantized(self) -> None:
        table = self._table()
        seen: list[tuple[int, ...]] = []
        real = gguf_layers.dequantize

        def recording(rows: torch.Tensor, quant_type: int, *, row_elements: int) -> torch.Tensor:
            seen.append(tuple(rows.shape))
            return real(rows, quant_type, row_elements=row_elements)

        gguf_layers.dequantize = recording
        try:
            table(torch.tensor([5, 5, 9]))
        finally:
            gguf_layers.dequantize = real
        self.assertEqual(seen, [(3, bytes_per_row(Q.Q8_0, 64))])

    def test_survives_tensor_slot_set_and_state_dict(self) -> None:
        table = self._table()
        ids = torch.tensor([1, 2, 2])
        expected = table(ids)
        slot = TensorSlot("weight", table, "weight", False, tuple(table.weight.shape), torch.uint8)
        offsets, total = packed_layout([slot])
        buffer = torch.zeros(total, dtype=torch.uint8)
        view = packed_view(buffer, offsets[0], slot)
        view.copy_(slot.get())
        slot.set(view)
        self.assertTrue(torch.equal(table(ids), expected))
        clone = GgufEmbedding(40, 64, quant_type=Q.Q8_0, compute_dtype=torch.bfloat16)
        clone.load_state_dict(table.state_dict(), strict=True)
        self.assertTrue(torch.equal(clone(ids), expected))


class _Toy(nn.Module):
    def __init__(self) -> None:
        super().__init__()
        self.embed = nn.Embedding(10, 64, padding_idx=1)
        self.blocks = nn.ModuleList([nn.Linear(64, 32), nn.Linear(32, 64, bias=False)])
        self.norm = nn.LayerNorm(64)
        self.odd = nn.Linear(40, 8)


class SwapTests(unittest.TestCase):
    def _meta_toy(self) -> _Toy:
        with torch.device("meta"):
            return _Toy()

    def test_swaps_linear_and_embedding_on_meta(self) -> None:
        toy = self._meta_toy()
        count = swap_gguf_modules(
            toy,
            {"embed.weight": Q.Q8_0, "blocks.0.weight": Q.Q4_0, "blocks.1.weight": Q.Q8_0},
            compute_dtype=torch.bfloat16,
        )
        self.assertEqual(count, 3)
        self.assertIsInstance(toy.embed, GgufEmbedding)
        self.assertEqual(toy.embed.padding_idx, 1)
        self.assertIsInstance(toy.blocks[0], GgufLinear)
        self.assertIsInstance(toy.blocks[1], GgufLinear)
        self.assertIsNone(toy.blocks[1].bias)
        self.assertTrue(toy.blocks[0].weight.is_meta)
        self.assertEqual(tuple(toy.blocks[0].weight.shape), (32, bytes_per_row(Q.Q4_0, 64)))
        self.assertEqual(toy.blocks[0].bias.dtype, torch.bfloat16)
        self.assertIsInstance(toy.odd, nn.Linear)
        keys = set(toy.state_dict())
        self.assertIn("blocks.0.weight", keys)
        self.assertIn("embed.weight", keys)

    def test_refusals_leave_the_module_untouched(self) -> None:
        cases = {
            "missing": ({"blocks.0.weight": Q.Q8_0, "nope.weight": Q.Q8_0}, "no submodule 'nope'"),
            "type": ({"blocks.0.weight": Q.Q8_0, "norm.weight": Q.Q8_0}, "LayerNorm"),
            "blocks": ({"blocks.0.weight": Q.Q8_0, "odd.weight": Q.Q8_0}, "in_features=40"),
            "iq2": ({"blocks.0.weight": Q.Q8_0, "blocks.1.weight": Q.IQ2_XXS}, "IQ2_XXS"),
            "suffix": ({"blocks.0.bias": Q.Q8_0}, "does not name a submodule weight"),
            "root": ({"weight": Q.Q8_0}, "does not name a submodule weight"),
        }
        for name, (mapping, message) in cases.items():
            with self.subTest(case=name):
                toy = self._meta_toy()
                with self.assertRaisesRegex(ValueError, message):
                    swap_gguf_modules(toy, mapping, compute_dtype=torch.bfloat16)
                self.assertIsInstance(toy.blocks[0], nn.Linear)

    def test_refuses_embedding_with_max_norm(self) -> None:
        holder = nn.Module()
        holder.table = nn.Embedding(4, 32, max_norm=1.0, device="meta")
        with self.assertRaisesRegex(ValueError, "max_norm"):
            swap_gguf_modules(holder, {"table.weight": Q.Q8_0}, compute_dtype=torch.bfloat16)


if __name__ == "__main__":
    unittest.main()
