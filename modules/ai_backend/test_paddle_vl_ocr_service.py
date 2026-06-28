"""
File: modules/ai_backend/test_paddle_vl_ocr_service.py

Purpose:
Unit tests for PaddleOCR-VL OCR text post-processing contracts.

Main responsibilities:
- verify line splitting trims whitespace and drops empty lines;
- verify `join_newlines=False` collapses lines with spaces;
- verify `reflect_strings=True` reverses line order for right-to-left columns.

Notes:
The model-loading path needs PyTorch/Transformers and network weights, so only
the pure text formatter is covered here.
"""

from __future__ import annotations

import unittest

from modules.ai_backend.paddle_vl_ocr_service import _format_recognition_lines


class FormatRecognitionLinesTests(unittest.TestCase):
    def test_splits_and_trims_lines(self) -> None:
        result = _format_recognition_lines(
            "  first \r\n\n second  \n",
            join_newlines=True,
            reflect_strings=False,
        )
        self.assertEqual(result["lines"], ["first", "second"])
        self.assertEqual(result["text"], "first\nsecond")

    def test_join_newlines_false_uses_spaces(self) -> None:
        result = _format_recognition_lines(
            "a\nb\nc",
            join_newlines=False,
            reflect_strings=False,
        )
        self.assertEqual(result["text"], "a b c")

    def test_reflect_strings_reverses_order(self) -> None:
        result = _format_recognition_lines(
            "top\nmiddle\nbottom",
            join_newlines=True,
            reflect_strings=True,
        )
        self.assertEqual(result["lines"], ["bottom", "middle", "top"])
        self.assertEqual(result["text"], "bottom\nmiddle\ntop")

    def test_empty_text_yields_empty_result(self) -> None:
        result = _format_recognition_lines(
            "",
            join_newlines=True,
            reflect_strings=False,
        )
        self.assertEqual(result["lines"], [])
        self.assertEqual(result["text"], "")


if __name__ == "__main__":
    unittest.main()
