"""
File: modules/ai_backend/inpaint/flux3_klein/test_real_processor.py

Purpose:
Opt-in checks of `flux3_klein/text_encoder.py` against the REAL Qwen3-VL
processor of a FLUX.3 klein package: the chat template, the reference
tokenization and padding rule (including truncation at 4096), the installed
image processor's pixel-cap kwarg, and the VL token count against
`vl_pixel_budget`.

Notes:
Runs only when `MS_FLUX3_DIR` names a package root with a `text_encoder/`
directory; otherwise every test skips. Loads the processor only (no weights),
CPU only, writes nothing.
"""

from __future__ import annotations

import logging
import math
import os
import unittest
from pathlib import Path
from typing import Any

from PIL import Image

from . import text_encoder as te
from .config import TEXT_MAX_TOKENS, TEXT_PAD_MULTIPLE

_ROOT = os.environ.get("MS_FLUX3_DIR")


def _processor_dir() -> Path | None:
    """`<MS_FLUX3_DIR>/text_encoder` when it exists."""
    if not _ROOT:
        return None
    directory = Path(_ROOT) / "text_encoder"
    return directory if directory.is_dir() else None


_DIR = _processor_dir()


def _image(width: int, height: int) -> Image.Image:
    """A flat RGB image; the processor's token count depends only on the size."""
    return Image.new("RGB", (width, height), (120, 80, 40))


def _caption_with_tokens(processor: Any, low: int, high: int) -> str:
    """A caption whose text-only template has between `low` and `high` tokens."""
    words = 1
    while True:
        caption = "слово " * words
        count = te.caption_token_count(processor, caption)
        if low <= count <= high:
            return caption
        if count > high:
            raise AssertionError(f"не удалось подобрать запрос на {low}..{high} токенов")
        words += max(1, (low - count) // 2)


@unittest.skipIf(_DIR is None, "MS_FLUX3_DIR не задан или в нём нет text_encoder/")
class RealProcessorTests(unittest.TestCase):
    processor: Any

    @classmethod
    def setUpClass(cls) -> None:
        assert _DIR is not None
        cls.processor = te.load_text_processor(_DIR)

    def test_tokenizer_setup(self) -> None:
        tokenizer = self.processor.tokenizer
        self.assertEqual(tokenizer.padding_side, "right")
        self.assertIsNotNone(tokenizer.pad_token_id)

    def test_chat_template(self) -> None:
        text = self.processor.apply_chat_template(
            [{"role": "user", "content": "a cat"}], tokenize=False, add_generation_prompt=True
        )
        self.assertEqual(text, "<|im_start|>user\na cat<|im_end|>\n<|im_start|>assistant\n")

    def _reference_tokens(self, caption: str) -> tuple[Any, Any, int]:
        """The reference `TextEncoder.encode` tokenization, verbatim (two tokenizer calls)."""
        text = self.processor.apply_chat_template(
            [{"role": "user", "content": caption}], tokenize=False, add_generation_prompt=True
        )
        tokenizer = self.processor.tokenizer
        real = tokenizer(text, return_tensors="pt", truncation=True, max_length=TEXT_MAX_TOKENS)["input_ids"].shape[1]
        length = min(math.ceil(real / TEXT_PAD_MULTIPLE) * TEXT_PAD_MULTIPLE, TEXT_MAX_TOKENS)
        inputs = tokenizer(text, return_tensors="pt", padding="max_length", truncation=True, max_length=length)
        return inputs["input_ids"], inputs["attention_mask"], real

    def test_tokenize_caption_matches_the_reference(self) -> None:
        for caption in ("a cat", "Кот в сапогах сидит на крыше, акварель. " * 30):
            batch = te.tokenize_caption(self.processor, caption)
            ids, mask, real = self._reference_tokens(caption)
            self.assertTrue(bool((batch.input_ids == ids).all()), caption[:20])
            self.assertTrue(bool((batch.attention_mask == mask).all()), caption[:20])
            self.assertEqual(batch.real_length, real)
            self.assertEqual(int(batch.attention_mask.sum()), real)
            self.assertEqual(batch.input_ids.shape[1] % TEXT_PAD_MULTIPLE, 0)
            pads = batch.input_ids[0, real:]
            self.assertTrue(bool((pads == self.processor.tokenizer.pad_token_id).all()))

    def test_a_long_caption_is_truncated_like_the_reference(self) -> None:
        caption = "слово " * 3000
        with self.assertLogs(te.log, logging.WARNING):
            batch = te.tokenize_caption(self.processor, caption)
        ids, mask, real = self._reference_tokens(caption)
        self.assertEqual(real, TEXT_MAX_TOKENS)
        self.assertEqual(batch.input_ids.shape, (1, TEXT_MAX_TOKENS))
        self.assertEqual(batch.real_length, TEXT_MAX_TOKENS)
        self.assertTrue(bool((batch.input_ids == ids).all()))

    def test_pixels_per_token_matches_the_image_processor(self) -> None:
        image_processor = self.processor.image_processor
        self.assertEqual((image_processor.patch_size * image_processor.merge_size) ** 2, te.VL_PIXELS_PER_TOKEN)

    def test_the_pixel_cap_kwarg_of_the_installed_processor(self) -> None:
        # Pins the transformers 4.57.1 behaviour `prepare_reference_batch`
        # relies on: `size` with both edges caps the image; a lone
        # `max_pixels` is silently ignored.
        text = self.processor.apply_chat_template(
            [{"role": "user", "content": [{"type": "image"}, {"type": "text", "text": "x"}]}],
            tokenize=False,
            add_generation_prompt=True,
        )
        image = _image(1024, 1024)
        capped = self.processor(
            text=[text], images=[image], return_tensors="pt", size={"shortest_edge": 4096, "longest_edge": 512 * 512}
        )
        self.assertEqual(capped["image_grid_thw"].tolist(), [[1, 32, 32]])
        ignored = self.processor(text=[text], images=[image], return_tensors="pt", max_pixels=512 * 512)
        self.assertEqual(ignored["image_grid_thw"].tolist(), [[1, 64, 64]])

    def test_one_reference_token_count(self) -> None:
        caption = "a red fox"
        text_only = te.caption_token_count(self.processor, caption)
        budget = te.vl_pixel_budget(text_only, 1, vl_ref_max_pixels=512 * 512)
        self.assertEqual(budget, 512 * 512)
        batch = te.prepare_reference_batch(self.processor, caption, [_image(1024, 1024)], vl_ref_max_pixels=512 * 512)
        image_tokens = budget // te.VL_PIXELS_PER_TOKEN
        self.assertEqual(batch.real_length, text_only + 2 + image_tokens)
        assert batch.image_grid_thw is not None and batch.pixel_values is not None
        self.assertEqual(batch.image_grid_thw.tolist(), [[1, 32, 32]])
        self.assertEqual(batch.input_ids.shape[1], te.padded_length(batch.real_length))
        self.assertEqual(int(batch.attention_mask.sum()), batch.real_length)
        self.assertEqual(int((batch.input_ids == self.processor.image_token_id).sum()), image_tokens)
        pads = batch.input_ids[0, batch.real_length :]
        self.assertTrue(bool((pads == self.processor.tokenizer.pad_token_id).all()))

    def test_three_references_share_the_sequence(self) -> None:
        caption = "a red fox"
        text_only = te.caption_token_count(self.processor, caption)
        budget = te.vl_pixel_budget(text_only, 3, vl_ref_max_pixels=4096 * 4096)
        references = [_image(2048, 2048), _image(1500, 900), _image(700, 1800)]
        batch = te.prepare_reference_batch(self.processor, caption, references, vl_ref_max_pixels=4096 * 4096)
        assert batch.image_grid_thw is not None
        per_image = [int(t * h * w) // 4 for t, h, w in batch.image_grid_thw.tolist()]
        self.assertTrue(all(tokens <= budget // te.VL_PIXELS_PER_TOKEN for tokens in per_image), per_image)
        self.assertEqual(batch.real_length, text_only + 3 * 2 + sum(per_image))
        self.assertLessEqual(batch.input_ids.shape[1], TEXT_MAX_TOKENS)

    def test_an_overflowing_reference_is_refused_not_truncated(self) -> None:
        # With ~4080 text tokens the budget is a few tokens, and a 32x6000
        # strip cannot be resized below 32 px wide, so it overshoots the cap.
        caption = _caption_with_tokens(self.processor, 4070, 4088)
        with self.assertRaisesRegex(ValueError, "токенов при пределе 4096"):
            te.prepare_reference_batch(self.processor, caption, [_image(32, 6000)], vl_ref_max_pixels=512 * 512)


if __name__ == "__main__":
    unittest.main()
