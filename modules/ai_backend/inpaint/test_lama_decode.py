"""
File: modules/ai_backend/inpaint/test_lama_decode.py

Purpose:
Pins the image / mask decoders of `inpaint/lama.py`: a cv2 decode that raises
falls back to PIL and is logged (WARNING, with the byte count), never dropped
silently (STATE S-094).

Notes:
- cv2 is replaced by a stub whose `imdecode` raises, so the PIL branch runs
  for real on a tiny in-memory PNG; no torch, no weights.
"""

from __future__ import annotations

import io
import types
import unittest
from unittest.mock import patch

from PIL import Image

from modules.ai_backend.inpaint import lama
from modules.ai_backend.runtime.model_manager import SmallModelManager


def _png(mode: str, color: object) -> bytes:
    with io.BytesIO() as buffer:
        Image.new(mode, (4, 3), color).save(buffer, format="PNG")
        return buffer.getvalue()


def _raising_cv2() -> types.SimpleNamespace:
    def imdecode(_buffer: object, _flags: object) -> object:
        raise ValueError("cv2 refused the bytes")

    return types.SimpleNamespace(imdecode=imdecode, IMREAD_UNCHANGED=-1)


class Cv2FallbackTests(unittest.TestCase):
    def test_the_rgb_decoder_logs_and_falls_back_to_pil(self) -> None:
        service = lama.LamaInpaintService(SmallModelManager())
        data = _png("RGB", (10, 20, 30))
        with patch.object(lama, "_maybe_cv2", _raising_cv2), self.assertLogs(lama.log, level="WARNING") as logs:
            rgb = service._decode_image_rgb(data)
        self.assertEqual(rgb.shape, (3, 4, 3))
        self.assertEqual(rgb[0, 0].tolist(), [10, 20, 30])
        self.assertTrue(any(f"({len(data)} bytes)" in line for line in logs.output))

    def test_the_any_decoder_logs_and_falls_back_to_pil(self) -> None:
        data = _png("L", 200)
        with patch.object(lama, "_maybe_cv2", _raising_cv2), self.assertLogs(lama.log, level="WARNING") as logs:
            mask = lama._decode_image_any(data)
        self.assertEqual(mask.shape, (3, 4))
        self.assertEqual(int(mask[0, 0]), 200)
        self.assertTrue(any("falling back to PIL" in line for line in logs.output))


if __name__ == "__main__":
    unittest.main()
