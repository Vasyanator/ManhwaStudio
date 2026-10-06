"""
File: modules/ai_backend/ocr/test_paddle_vl_real.py

Purpose:
Opt-in real-weight check of PaddleOCR-VL through the vendored code: the
checkpoint loads with no missing/unexpected keys, and greedy output with the
K/V cache (`use_cache=True`, the production setting) is identical to the
uncached output on five synthetic crops.

Notes:
Runs only when `MS_PADDLE_VL_DIR` names a downloaded variant directory
(`ManhwaStudio_AI_Models/side_models/PaddleOCR-VL/<variant>`); otherwise it
skips cleanly. `MS_PADDLE_VL_DEVICE` picks the torch device (default `cpu`).
Timings of both modes are logged at INFO (`pytest -o log_cli=true
-o log_cli_level=INFO` shows them). Crops are rendered with the bundled UI
fonts, so nothing outside the repository and the model directory is read.
"""

from __future__ import annotations

import logging
import os
import time
import unittest
from pathlib import Path

from modules.ai_backend.ocr import paddle_vl as service_module
from modules.ai_backend.runtime.paths import program_root

log = logging.getLogger(__name__)

_CROPS = (
    ("core/01-SourceHanSansK-Regular.otf", "내가 반드시 지켜줄게!"),
    ("core/01-SourceHanSansK-Regular.otf", "私がこの手で守る！！"),
    ("core/01-SourceHanSansK-Regular.otf", "我一定会保护你的！"),
    ("bold/00-NotoSans-Bold.ttf", "I WON'T LET YOU GO"),
    ("core/00-NotoSans-Regular.ttf", "Я тебя защищу!"),
)


def _render(font_file: str, text: str):
    from PIL import Image, ImageDraw, ImageFont

    font = ImageFont.truetype(str(program_root() / "fonts" / "ui" / font_file), 32)
    width = int(font.getlength(text)) + 40
    image = Image.new("RGB", (width, 80), "white")
    ImageDraw.Draw(image).text((20, 20), text, font=font, fill="black")
    return image


@unittest.skipUnless(os.environ.get("MS_PADDLE_VL_DIR"), "set MS_PADDLE_VL_DIR to run the real-weight PaddleOCR-VL check")
class KvCacheParityTests(unittest.TestCase):
    def test_cached_and_uncached_greedy_output_match(self) -> None:
        model_dir = Path(os.environ["MS_PADDLE_VL_DIR"]).resolve()
        device = os.environ.get("MS_PADDLE_VL_DEVICE", "cpu")
        model, processor = service_module._load_model_and_processor(model_dir, device)
        generate = service_module.PaddleVlOcrService._generate_text
        for font_file, text in _CROPS:
            with self.subTest(text=text):
                image = _render(font_file, text)
                started = time.monotonic()
                cached = generate(model, processor, image, use_cache=True)
                cached_s = time.monotonic() - started
                started = time.monotonic()
                uncached = generate(model, processor, image, use_cache=False)
                uncached_s = time.monotonic() - started
                log.info(
                    "PaddleOCR-VL parity: dir=%s device=%s input=%r output=%r cached=%.2fs uncached=%.2fs",
                    model_dir,
                    device,
                    text,
                    cached,
                    cached_s,
                    uncached_s,
                )
                self.assertEqual(cached, uncached)


if __name__ == "__main__":
    unittest.main()
