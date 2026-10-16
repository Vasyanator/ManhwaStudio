"""
File: modules/ai_backend/ocr/test_lease_hints.py

Purpose:
Pins the pre-load size hints the OCR services build for the residency arbiter
(the Baberu split hint and its spy test live in `test_baberu.py`).

Main responsibilities:
- MangaOCR: the ONNX hint is the encoder + decoder files (x1.25) on the EP's
  device; the PyTorch hint names only the device (its Hugging Face cache files
  are not resolved before the load);
- EasyOCR: the hint is the weight directory on the reader's device (`cpu` when
  EasyOCR gets no GPU); a relative or empty directory is never sized;
- Surya OCR: the same directory rule for its two leases' directories, and a
  refused detector lease releases (and aborts) the foundation lease already taken.

Notes:
- Pure calls of each service's hint builder: no lease, no torch, no weights, no
  GPU. Scratch files live in a temp directory removed by each test.
"""

from __future__ import annotations

import io
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

from PIL import Image

from modules.ai_backend.engines.paddle_onnx import ProviderSettings
from modules.ai_backend.ocr import easy, surya
from modules.ai_backend.runtime.memory_budget import MIB
from modules.ai_backend.runtime.model_manager import SmallModelManager
from modules.ai_backend.runtime.residency.holdings import FootprintHint, Runtime


class _FakeDeviceService:
    """`AiDeviceService.get_state` stand-in with a fixed torch device."""

    def __init__(self, selected_device: str) -> None:
        self.selected_device = selected_device

    def get_state(self) -> dict[str, Any]:
        return {"selected_device": self.selected_device}


class _TempDirTestCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="ms-ocr-hints-")
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)

    def _write(self, relative: str, size: int) -> Path:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"x" * size)
        return path


class MangaLeaseHintTests(_TempDirTestCase):
    def setUp(self) -> None:
        super().setUp()
        try:
            from modules.ai_backend.ocr import manga
        except ImportError as exc:  # pragma: no cover - environment specific (jaconv)
            raise unittest.SkipTest(f"modules.ai_backend.ocr.manga is not importable: {exc}") from exc
        self.manga = manga

    def _service(self, selected_device: str = "cpu") -> Any:
        return self.manga.MangaOcrService(SmallModelManager(), _FakeDeviceService(selected_device))

    def test_onnx_hint_is_the_encoder_and_decoder_on_the_ep_device(self) -> None:
        self._write(f"base/{self.manga.ENCODER_FILE_NAME}", 400)
        self._write(f"base/{self.manga.DECODER_FILE_NAME}", 400)
        self._write("base/tokenizer.json", 99)
        service = self._service()
        service._model_dir = lambda _name: self.root / "base"
        hint = service._footprint_hint("base_onnx", ProviderSettings("MIGraphXExecutionProvider", "0"))
        self.assertEqual(hint, FootprintHint(device="cuda:0", device_bytes=1000, host_bytes=256 * MIB, runtime=Runtime.ONNX))

    def test_torch_hint_names_only_the_device(self) -> None:
        self.assertEqual(self._service("cuda")._footprint_hint("base_torch", ProviderSettings("CPUExecutionProvider")), FootprintHint(device="cuda"))
        self.assertEqual(self._service("cpu")._footprint_hint("base_torch", ProviderSettings("CPUExecutionProvider")), FootprintHint(device="cpu"))


class EasyLeaseHintTests(_TempDirTestCase):
    def test_hint_is_the_weight_directory_on_the_reader_device(self) -> None:
        self._write("craft_mlt_25k.pth", 500)
        self._write("korean_g2.pth", 100)
        hint = easy.EasyOcrService._footprint_hint("cuda:0", str(self.root))
        self.assertEqual(hint, FootprintHint(device="cuda:0", device_bytes=600, host_bytes=256 * MIB))

    def test_a_device_easyocr_cannot_use_is_host_accounted(self) -> None:
        self._write("craft_mlt_25k.pth", 500)
        self.assertEqual(easy.EasyOcrService._footprint_hint("mps", str(self.root)), FootprintHint(device="cpu", device_bytes=0, host_bytes=500))

    def test_relative_or_empty_directory_is_never_sized(self) -> None:
        for bad in ("", "relative/dir"):
            with self.subTest(bad=bad):
                self.assertEqual(easy.EasyOcrService._footprint_hint("cpu", bad), FootprintHint(device="cpu"))


class SuryaLeaseHintTests(_TempDirTestCase):
    def test_hint_is_the_directory_and_a_relative_one_is_never_sized(self) -> None:
        self._write("model.safetensors", 700)
        self.assertEqual(surya._dir_hint(str(self.root), "cpu"), FootprintHint(device="cpu", device_bytes=0, host_bytes=700))
        self.assertEqual(surya._dir_hint("", "cuda"), FootprintHint(device="cuda"))


class SuryaLeaseLeakTests(_TempDirTestCase):
    def test_a_refused_detector_lease_releases_the_foundation_lease(self) -> None:
        manager = SmallModelManager(max_loaded_models=4)
        service = surya.SuryaOcrService(manager)
        real = manager.begin_model_use

        def refuse_detector(key: str, unload_callback: Any = None, *, owner: Any = None, hint: Any = None) -> Any:
            if key == service._detector_model_key("cpu"):
                raise RuntimeError("detector admission refused")
            return real(key, unload_callback, owner=owner, hint=hint)

        buffer = io.BytesIO()
        Image.new("RGB", (8, 8), (255, 255, 255)).save(buffer, format="PNG")
        with patch.object(manager, "begin_model_use", refuse_detector), \
                patch.object(surya, "_resolve_selected_backend_device", lambda _fallback: "cpu"):
            with self.assertRaisesRegex(RuntimeError, "detector admission refused"):
                service.recognize_image_bytes(buffer.getvalue(), recognition_dir=str(self.root), detection_dir=str(self.root), task_name=surya.SURYA_TASK_OCR_WITH_BOXES)
        # The foundation load was aborted and its lease released: nothing is left in use or loading.
        self.assertEqual(manager.holdings(), {})


if __name__ == "__main__":
    unittest.main()
