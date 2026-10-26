"""
File: modules/ai_backend/inpaint/flux2_klein/test_conversion.py

Purpose:
Unit tests for `conversion.py`: the per-key BFL / diffusers layout decision on
the klein key vocabulary, diffusers' converter applied to one tensor (key set
and values of a fused qkv split), the same converter on block-quantized
GGUF rows (`uint8 (rows, bytes_per_row)`: the qkv row split and the adaLN row
swap must commute with dequantization), and the refusal of a
`model.diffusion_model.`-prefixed checkpoint.

Notes:
The converter tests need torch and diffusers and skip without them. No GPU,
no files.
"""

from __future__ import annotations

import unittest
from pathlib import Path

from modules.ai_backend.inpaint.flux2_klein import conversion

#: Every top-level key shape of the real klein 9B BFL header (one per kind).
_BFL_KEYS = (
    "double_blocks.0.img_attn.qkv.weight",
    "double_blocks.0.txt_mlp.2.weight",
    "single_blocks.3.linear1.weight",
    "double_stream_modulation_img.lin.weight",
    "single_stream_modulation.lin.weight",
    "final_layer.adaLN_modulation.1.weight",
    "img_in.weight",
    "txt_in.weight",
    "time_in.in_layer.weight",
)
_DIFFUSERS_KEYS = (
    "transformer_blocks.0.attn.to_q.weight",
    "single_transformer_blocks.3.attn.to_qkv_mlp_proj.weight",
    "double_stream_modulation_img.linear.weight",
    "norm_out.linear.weight",
    "x_embedder.weight",
    "context_embedder.weight",
    "time_guidance_embed.timestep_embedder.linear_1.weight",
)


def _has_diffusers() -> bool:
    try:
        import diffusers  # noqa: F401 - availability probe
        import torch  # noqa: F401 - availability probe
    except ImportError:
        return False
    return True


class LayoutTests(unittest.TestCase):
    def test_the_two_vocabularies_never_overlap(self) -> None:
        for key in _BFL_KEYS:
            with self.subTest(key=key):
                self.assertTrue(conversion.is_bfl_key(key))
        for key in _DIFFUSERS_KEYS:
            with self.subTest(key=key):
                self.assertFalse(conversion.is_bfl_key(key))

    def test_the_marker_is_bfl(self) -> None:
        self.assertTrue(conversion.is_bfl_key(conversion.FLUX2_BFL_MARKER_KEY))

    def test_a_prefixed_checkpoint_is_refused(self) -> None:
        with self.assertRaisesRegex(ValueError, "model.diffusion_model."):
            conversion.require_known_layout(["model.diffusion_model.img_in.weight"], source=Path("x.safetensors"))
        conversion.require_known_layout(_BFL_KEYS, source=Path("x.safetensors"))


@unittest.skipUnless(_has_diffusers(), "needs torch and diffusers")
class ConverterTests(unittest.TestCase):
    def test_every_bfl_key_converts_into_diffusers_keys(self) -> None:
        import torch

        for key in _BFL_KEYS:
            with self.subTest(key=key):
                produced = conversion.convert_bfl_tensor(key, torch.empty(6, 6, device="meta"))
                self.assertTrue(produced)
                self.assertTrue(all(not conversion.is_bfl_key(name) for name in produced))

    def test_a_fused_qkv_splits_by_value(self) -> None:
        import torch

        fused = torch.arange(12.0).reshape(6, 2)
        produced = conversion.convert_bfl_tensor("double_blocks.1.txt_attn.qkv.weight", fused)
        self.assertEqual(list(produced), ["transformer_blocks.1.attn.add_q_proj.weight", "transformer_blocks.1.attn.add_k_proj.weight", "transformer_blocks.1.attn.add_v_proj.weight"])
        self.assertTrue(torch.equal(produced["transformer_blocks.1.attn.add_v_proj.weight"], fused[4:6]))


def _has_gguf_layers() -> bool:
    try:
        import gguf  # noqa: F401 - availability probe

        from modules.ai_backend.runtime import gguf_layers  # noqa: F401 - availability probe (torch + diffusers)
    except ImportError:
        return False
    return True


@unittest.skipUnless(_has_gguf_layers(), "needs torch, diffusers and gguf")
class QuantizedRowConversionTests(unittest.TestCase):
    """diffusers' converter on GGUF `uint8` rows: only whole-row operations, so the bytes stay valid blocks."""

    def _rows(self, name: str, rows: int, row_elements: int = 64) -> tuple[object, object]:
        import torch

        from modules.ai_backend.runtime._gguf_test_fixtures import Q, make_tensor

        fixture = make_tensor(name, Q.Q8_0, (rows, row_elements), seed=rows)
        return torch.from_numpy(fixture.raw.copy()), fixture

    def test_a_quantized_fused_qkv_splits_into_whole_row_slices(self) -> None:
        import torch

        from modules.ai_backend.runtime.gguf_layers import dequantize

        packed, fixture = self._rows("double_blocks.0.img_attn.qkv.weight", 3 * 4)
        produced = conversion.convert_bfl_tensor("double_blocks.0.img_attn.qkv.weight", packed)
        self.assertEqual(list(produced), ["transformer_blocks.0.attn.to_q.weight", "transformer_blocks.0.attn.to_k.weight", "transformer_blocks.0.attn.to_v.weight"])
        dequantized = dequantize(packed, int(fixture.qtype), row_elements=64)
        for index, value in enumerate(produced.values()):
            with self.subTest(index=index):
                self.assertEqual((value.dtype, tuple(value.shape)), (torch.uint8, (4, packed.shape[1])))
                self.assertTrue(torch.equal(value, packed[4 * index : 4 * (index + 1)]))
                self.assertTrue(torch.equal(dequantize(value.contiguous(), int(fixture.qtype), row_elements=64), dequantized[4 * index : 4 * (index + 1)]))

    def test_a_quantized_final_adaln_swaps_whole_row_halves(self) -> None:
        import torch

        from modules.ai_backend.runtime.gguf_layers import dequantize

        packed, fixture = self._rows("final_layer.adaLN_modulation.1.weight", 6)
        produced = conversion.convert_bfl_tensor("final_layer.adaLN_modulation.1.weight", packed)
        swapped = produced["norm_out.linear.weight"]
        self.assertEqual(swapped.dtype, torch.uint8)
        self.assertTrue(torch.equal(swapped, torch.cat([packed[3:], packed[:3]])))
        dequantized = dequantize(packed, int(fixture.qtype), row_elements=64)
        on_values = conversion.convert_bfl_tensor("final_layer.adaLN_modulation.1.weight", dequantized)["norm_out.linear.weight"]
        self.assertTrue(torch.equal(dequantize(swapped, int(fixture.qtype), row_elements=64), on_values))

    def test_renamed_quantized_tensors_keep_their_bytes(self) -> None:
        import torch

        packed, _fixture = self._rows("single_blocks.2.linear1.weight", 5)
        produced = conversion.convert_bfl_tensor("single_blocks.2.linear1.weight", packed)
        self.assertEqual(list(produced), ["single_transformer_blocks.2.attn.to_qkv_mlp_proj.weight"])
        self.assertTrue(torch.equal(produced["single_transformer_blocks.2.attn.to_qkv_mlp_proj.weight"], packed))


if __name__ == "__main__":
    unittest.main()
