"""
File: modules/ai_backend/inpaint/flux2_klein/test_attention.py

Purpose:
Unit tests for `attention.py`: the `text_attention_in_mask` contract.

Main responsibilities:
- verify the token grid marks a token inside when ANY of its pixels is set, and
  refuses a mask that is not on the 16 px grid;
- verify a condition image's token count is exact or refused, never guessed;
- verify the early refusal of a mask with no set pixel, and the layout of one
  run (text, noisy, clean copy, reference) including whether the reference
  follows the region's grid, and the planner's refusal of a mask with no
  inside token;
- verify the device-byte formula, row padding included;
- verify the built mask blocks EXACTLY the documented pairs, keeps an open key in
  every row, and is a row-padded view (`AttentionMaskBuildTests`, real torch on
  the CPU; skipped where torch is absent);
- pin the layout to diffusers itself, not to this module's own assumption:
  `RealTransformerLayoutTests` runs a tiny randomly initialised
  `Flux2Transformer2DModel` (no download, no file) with the mask and two
  different text embeddings, and asserts the text reaches exactly the inside
  tokens (skipped where torch or diffusers is absent).
"""

from __future__ import annotations

import importlib.util
import logging
import unittest

import numpy as np

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import attention


def _mask(height: int, width: int, *pixels: tuple[int, int]) -> np.ndarray:
    """An L8 mask of `height x width` with exactly `pixels` (row, col) set."""
    mask = np.zeros((height, width), dtype=np.uint8)
    for row, col in pixels:
        mask[row, col] = 255
    return mask


class TokenGridTests(unittest.TestCase):
    def test_one_pixel_marks_exactly_its_own_token(self) -> None:
        # Pixel (17, 40) lies in token row 1, token column 2 of a 2 x 3 grid.
        grid = svc.token_grid_inside(_mask(32, 48, (17, 40)))
        expected = np.zeros((2, 3), dtype=bool)
        expected[1, 2] = True
        self.assertTrue(np.array_equal(grid, expected))

    def test_a_token_edge_pixel_is_enough(self) -> None:
        # The last pixel of token (0, 0) and the first of token (1, 1): ANY set
        # pixel counts, which is the one-token dilation the contract asks for.
        grid = svc.token_grid_inside(_mask(32, 32, (15, 15), (16, 16)))
        self.assertTrue(np.array_equal(grid, np.array([[True, False], [False, True]])))

    def test_an_empty_mask_has_no_inside_token(self) -> None:
        self.assertFalse(svc.token_grid_inside(_mask(32, 32)).any())

    def test_a_mask_off_the_token_grid_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            svc.token_grid_inside(np.zeros((30, 32), dtype=np.uint8))
        with self.assertRaises(ValueError):
            svc.token_grid_inside(np.zeros((32, 32, 1), dtype=np.uint8))


class ConditionTokensTests(unittest.TestCase):
    def test_the_count_is_the_token_grid_area(self) -> None:
        self.assertEqual(svc.condition_tokens(128, 64), 8 * 4)

    def test_a_size_the_pipeline_would_resize_is_refused(self) -> None:
        # Off the 16 px grid, or above 1 MP: diffusers would resize it, and the
        # count would then be a guess.
        for width, height in ((130, 64), (2048, 1024), (0, 64)):
            with self.subTest(size=(width, height)), self.assertRaises(ValueError):
                svc.condition_tokens(width, height)


class EarlyRefusalTests(unittest.TestCase):
    def test_an_empty_mask_is_refused_with_a_log_line(self) -> None:
        with (
            self.assertLogs(attention.log, level="WARNING") as captured,
            self.assertRaises(ValueError),
        ):
            svc.require_text_attention_target(_mask(32, 48))
        self.assertTrue(any("mask_shape=(32, 48)" in line for line in captured.output))

    def test_one_set_pixel_is_enough(self) -> None:
        svc.require_text_attention_target(_mask(32, 48, (31, 47)))


class PlanTests(unittest.TestCase):
    def test_the_layout_without_a_reference(self) -> None:
        layout = svc.plan_text_attention(
            _mask(32, 48, (0, 0)), text_tokens=7, reference_hw=None
        )
        self.assertEqual(
            (layout.text_tokens, layout.image_tokens, layout.reference_tokens), (7, 6, 0)
        )
        self.assertEqual(layout.sequence_length, 7 + 2 * 6)
        self.assertEqual(layout.inside_tokens, 1)
        self.assertTrue(np.array_equal(layout.inside, [True, False, False, False, False, False]))

    def test_the_reference_count_comes_from_its_own_size(self) -> None:
        layout = svc.plan_text_attention(
            _mask(32, 48, (0, 0)), text_tokens=7, reference_hw=(32, 48)
        )
        self.assertEqual(layout.reference_tokens, 6)
        self.assertEqual(layout.sequence_length, 7 + 2 * 6 + 6)

    def test_a_region_sized_reference_follows_the_region_grid(self) -> None:
        layout = svc.plan_text_attention(
            _mask(32, 48, (0, 0)), text_tokens=7, reference_hw=(32, 48)
        )
        self.assertIs(layout.reference_inside, layout.inside)

    def test_a_reference_of_another_size_stays_open_and_is_logged(self) -> None:
        # 64 x 32 px is 8 tokens against the region's 6: no token of it can be
        # matched to a region token, so none of them is cut off from the text.
        with self.assertLogs(attention.log, level=logging.INFO) as captured:
            layout = svc.plan_text_attention(
                _mask(32, 48, (0, 0)), text_tokens=7, reference_hw=(64, 32)
            )
        self.assertEqual(layout.reference_tokens, 8)
        self.assertIsNone(layout.reference_inside)
        self.assertEqual(len([line for line in captured.output if "not spatially" in line]), 1)

    def test_no_reference_has_no_reference_grid(self) -> None:
        layout = svc.plan_text_attention(
            _mask(32, 48, (0, 0)), text_tokens=7, reference_hw=None
        )
        self.assertIsNone(layout.reference_inside)

    def test_a_mask_with_no_inside_token_is_refused(self) -> None:
        # Every image query would be cut off from the text: the prompt would
        # reach nothing and the run would be a silent no-op.
        with self.assertRaises(ValueError):
            svc.plan_text_attention(_mask(32, 32), text_tokens=7, reference_hw=None)

    def test_an_empty_text_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            svc.plan_text_attention(_mask(32, 32, (0, 0)), text_tokens=0, reference_hw=None)


class MaskBytesTests(unittest.TestCase):
    def test_rows_are_padded_to_the_alignment(self) -> None:
        # S = 4 + 2*2 + 1 = 9 rows of 16 (9 rounded up to a multiple of 8) at 2 bytes.
        self.assertEqual(svc.text_attention_mask_bytes(4, 2, 1, "bfloat16"), 9 * 16 * 2)
        # An aligned S has no padding: 512 + 2*4096 = 8704.
        self.assertEqual(
            svc.text_attention_mask_bytes(512, 4096, 0, "float16"), 8704 * 8704 * 2
        )

    def test_the_layout_reports_the_same_figure(self) -> None:
        layout = svc.plan_text_attention(
            _mask(32, 48, (0, 0)), text_tokens=7, reference_hw=(32, 48)
        )
        self.assertEqual(
            layout.mask_bytes("bfloat16"), svc.text_attention_mask_bytes(7, 6, 6, "bfloat16")
        )

    def test_an_unknown_dtype_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            svc.text_attention_mask_bytes(4, 2, 1, "float32")


class AttentionMaskBuildTests(unittest.TestCase):
    """The mask itself, with real torch on the CPU."""

    def setUp(self) -> None:
        super().setUp()
        if importlib.util.find_spec("torch") is None:  # pragma: no cover - host-dependent
            self.skipTest("torch is not installed")

    def _expected_blocked(self, layout: svc.TextAttentionLayout) -> np.ndarray:
        """The documented blocked pairs, written out segment by segment."""
        text, image = layout.text_tokens, layout.image_tokens
        sequence = layout.sequence_length

        def outside(index: int) -> bool:
            # Noisy tokens and the region's clean copy share the region grid; an
            # aligned reference follows its own copy of it, a non-aligned one and
            # the text are never "outside".
            if text <= index < text + image:
                return not bool(layout.inside[index - text])
            if text + image <= index < text + 2 * image:
                return not bool(layout.inside[index - text - image])
            if index >= text + 2 * image and layout.reference_inside is not None:
                return not bool(layout.reference_inside[index - text - 2 * image])
            return False

        blocked = np.zeros((sequence, sequence), dtype=bool)
        for query in range(sequence):
            for key in range(sequence):
                query_is_text, key_is_text = query < text, key < text
                if query_is_text and not key_is_text and outside(key):
                    blocked[query, key] = True
                if not query_is_text and key_is_text and outside(query):
                    blocked[query, key] = True
        return blocked

    def _check(self, reference_hw: tuple[int, int] | None) -> svc.TextAttentionLayout:
        import torch

        # 2 x 2 tokens, the top-left one inside; 3 text tokens.
        layout = svc.plan_text_attention(
            _mask(32, 32, (3, 5)), text_tokens=3, reference_hw=reference_hw
        )
        mask = svc.build_text_attention_mask(layout, dtype_name="bfloat16", device="cpu")
        sequence = layout.sequence_length
        self.assertEqual(tuple(mask.shape), (sequence, sequence))
        self.assertEqual(mask.dtype, torch.bfloat16)
        # A view of a row-padded buffer, so SDPA does not copy it per call.
        alignment = svc.MASK_ROW_ALIGNMENT
        self.assertEqual(mask.stride(0), -(-sequence // alignment) * alignment)

        values = mask.float().numpy()
        minimum = float(torch.finfo(torch.bfloat16).min)
        blocked = values == minimum
        self.assertTrue(np.array_equal(blocked, self._expected_blocked(layout)))
        self.assertTrue(np.all(values[~blocked] == 0.0))
        # No softmax row is ever fully blocked.
        self.assertTrue((~blocked).any(axis=1).all())
        return layout

    def test_the_documented_pairs_are_blocked_without_a_reference(self) -> None:
        self._check(None)

    def test_an_aligned_reference_is_blocked_outside_the_mask(self) -> None:
        layout = self._check((32, 32))
        mask = svc.build_text_attention_mask(layout, dtype_name="bfloat16", device="cpu")
        values = mask.float().numpy()
        # Reference token 0 is inside (the set pixel is in token (0, 0)), token 3 is not.
        reference_start = layout.text_tokens + 2 * layout.image_tokens
        self.assertEqual(values[0, reference_start], 0.0)
        self.assertLess(values[0, reference_start + 3], 0.0)
        self.assertLess(values[reference_start + 3, 0], 0.0)

    def test_a_non_aligned_reference_stays_open_to_the_text(self) -> None:
        layout = self._check((32, 48))
        self.assertIsNone(layout.reference_inside)

    def test_an_unknown_dtype_is_refused(self) -> None:
        layout = svc.plan_text_attention(_mask(32, 32, (0, 0)), text_tokens=3, reference_hw=None)
        with self.assertRaises(ValueError):
            svc.build_text_attention_mask(layout, dtype_name="float32", device="cpu")


class RealTransformerLayoutTests(unittest.TestCase):
    """The mask against diffusers' real joint-sequence concatenation.

    Every other test here checks the mask against the layout this module
    ASSUMES (`[text | noisy | clean copy | reference]`). A diffusers upgrade that
    reorders the stream at the same sequence length would pass all of them and
    silently mis-apply the mask, because SDPA only notices a length change. This
    runs the real `Flux2Transformer2DModel` — one double-stream block, then one
    single-stream block, tiny dims, random weights, on the CPU — twice with the
    same image and two different text embeddings, and requires that exactly the
    tokens the mask leaves open to the text change.
    """

    #: 64 x 48 px region -> 4 x 3 tokens; token (1, 2) via one pixel, token (3, 0) fully.
    REGION_HW = (64, 48)
    TEXT_TOKENS = 5

    def setUp(self) -> None:
        super().setUp()
        try:
            import diffusers  # noqa: F401
            import torch  # noqa: F401
        except ImportError as exc:  # pragma: no cover - host-dependent
            self.skipTest(f"torch or diffusers is not importable: {exc}")

    def _changed_tokens(
        self, reference_hw: tuple[int, int] | None, *, double_blocks: int, single_blocks: int
    ) -> tuple[svc.TextAttentionLayout, np.ndarray]:
        """Per image token, whether a text change moved the transformer's output."""
        import torch
        from diffusers import Flux2Transformer2DModel

        height, width = self.REGION_HW
        region_mask = _mask(height, width, (17, 33))
        region_mask[48:64, 0:16] = 255
        layout = svc.plan_text_attention(
            region_mask, text_tokens=self.TEXT_TOKENS, reference_hw=reference_hw
        )
        torch.manual_seed(0)
        model = Flux2Transformer2DModel(
            in_channels=8,
            num_layers=double_blocks,
            num_single_layers=single_blocks,
            attention_head_dim=16,
            num_attention_heads=2,
            joint_attention_dim=24,
            axes_dims_rope=(4, 4, 4, 4),
            timestep_guidance_channels=32,
            guidance_embeds=False,
        ).eval()

        def grid_ids(time_index: int, grid_hw: tuple[int, int]) -> torch.Tensor:
            # The pipeline's `(t, h, w, l)` position ids for one image.
            return torch.cartesian_prod(
                torch.tensor([time_index]),
                torch.arange(grid_hw[0] // svc.TOKEN_PIXELS),
                torch.arange(grid_hw[1] // svc.TOKEN_PIXELS),
                torch.tensor([0]),
            )

        id_parts = [grid_ids(0, self.REGION_HW), grid_ids(10, self.REGION_HW)]
        if reference_hw is not None:
            id_parts.append(grid_ids(20, reference_hw))
        image_ids = torch.cat(id_parts)[None]
        text_ids = torch.cartesian_prod(
            torch.tensor([0]), torch.tensor([0]), torch.tensor([0]), torch.arange(self.TEXT_TOKENS)
        )[None]
        image_count = layout.sequence_length - layout.text_tokens
        hidden = torch.randn(1, image_count, 8)
        text = torch.randn(1, self.TEXT_TOKENS, 24)
        # The model runs in float32 for a clear signal; the bf16 minimum is a
        # finite float32 value, so the blocked positions carry over exactly.
        mask = svc.build_text_attention_mask(layout, dtype_name="bfloat16", device="cpu").float()
        common = {
            "hidden_states": hidden,
            "timestep": torch.tensor([0.5]),
            "img_ids": image_ids,
            "txt_ids": text_ids,
            "guidance": None,
            "joint_attention_kwargs": {"attention_mask": mask},
            "return_dict": False,
        }
        with torch.no_grad():
            first = model(encoder_hidden_states=text, **common)[0]
            second = model(encoder_hidden_states=text + 1.0, **common)[0]
        self.assertEqual(tuple(first.shape), (1, image_count, 8))
        changed = (first - second).abs().amax(dim=-1)[0].numpy() > 0.0
        return layout, changed

    def _assert_reach(self, reference_hw: tuple[int, int] | None, reference_open: str) -> None:
        for double_blocks, single_blocks in ((1, 0), (0, 1)):
            with self.subTest(double=double_blocks, single=single_blocks):
                layout, changed = self._changed_tokens(
                    reference_hw, double_blocks=double_blocks, single_blocks=single_blocks
                )
                inside = np.asarray(layout.inside, dtype=bool)
                if reference_open == "follows":
                    reference = inside
                elif reference_open == "all":
                    reference = np.ones(layout.reference_tokens, dtype=bool)
                else:
                    reference = np.zeros(0, dtype=bool)
                expected = np.concatenate([inside, inside, reference])
                self.assertTrue(np.array_equal(changed, expected), (changed, expected))

    def test_the_text_reaches_exactly_the_inside_tokens(self) -> None:
        self._assert_reach(None, "none")

    def test_an_aligned_reference_follows_the_mask(self) -> None:
        self._assert_reach(self.REGION_HW, "follows")

    def test_a_non_aligned_reference_is_fully_reached(self) -> None:
        self._assert_reach((32, 32), "all")


if __name__ == "__main__":
    unittest.main()
