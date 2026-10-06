"""
File: modules/ai_backend/ocr/test_paddle_vl_vendor.py

Purpose:
Pins the vendored PaddleOCR-VL code (`paddle_vl_vendor/`) to the exact upstream
bytes of `PaddlePaddle/PaddleOCR-VL-1.6` @ `c5630abae1d940eafe0697512a0325494b02ab42`.

Main responsibilities:
- every vendored file exists and has the published sha256, which catches a
  hand edit, an added header and a formatter run;
- no file other than the pinned ones, the package marker and the readme sits
  in the package (an unreviewed module would be executable code too).

Notes:
Hashes are taken over CRLF-normalized bytes so a Windows checkout with
`core.autocrlf` still verifies; the upstream files contain no CR at all.
"""

from __future__ import annotations

import hashlib
import unittest
from pathlib import Path

VENDOR_DIR = Path(__file__).resolve().parent / "paddle_vl_vendor"

PINNED_SHA256 = {
    "configuration_paddleocr_vl.py": "753dd93654c3a9c8c85a3eaee1e3092dd12591b0f2dce0305e1abfb7a41ff160",
    "modeling_paddleocr_vl.py": "c5013dff57ca8b87dc1de64d0fd839a44313de09d230a4fb2d08289d2cad5111",
    "processing_paddleocr_vl.py": "e29cb1e5f275f2bd3ce051bd5c9983a33894e693b2823a0e13d4c07c8c4f9e13",
    "image_processing_paddleocr_vl.py": "a4fa521b9cb16e207f94b7f2d16427771776dfc634420d319fc4916ee58049ec",
    "LICENSE": "b8c4d7deccd236af023af1c88c4d4e8f0fc2f41914e0fb23a3ec9678fb5a8456",
}
PROJECT_FILES = {"__init__.py", "MODULE_README.md"}


class VendoredFilesTests(unittest.TestCase):
    def test_every_vendored_file_matches_its_pin(self) -> None:
        for name, expected in PINNED_SHA256.items():
            with self.subTest(file=name):
                data = (VENDOR_DIR / name).read_bytes().replace(b"\r\n", b"\n")
                self.assertEqual(hashlib.sha256(data).hexdigest(), expected)

    def test_no_unpinned_file_in_the_package(self) -> None:
        present = {
            path.name
            for path in VENDOR_DIR.iterdir()
            if path.is_file() and path.suffix != ".pyc"
        }
        self.assertEqual(present, set(PINNED_SHA256) | PROJECT_FILES)


if __name__ == "__main__":
    unittest.main()
