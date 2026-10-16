"""
File: modules/ai_backend/ocr/test_easy.py

Purpose:
Unit tests for the offline loading contract of `EasyOcrService`: the reader
loads from the Rust-provided weight directory with downloads disabled, the
language list is used verbatim (Rust canonicalizes it), a missing directory or
weight file is an explicit "not downloaded" error, and nothing is downloaded.

Main responsibilities:
- `easyocr.Reader` receives `model_storage_directory=<dir>` and
  `download_enabled=False`; `download_and_unzip` is never called;
- an empty / relative / missing directory is refused before the reader is
  built (and a missing one is not created);
- a reader `FileNotFoundError` is reported as "not downloaded" without a CPU
  retry, while other GPU failures still fall back to the CPU;
- guard: Rust's script table (`crates/ms-sysprobe/fixtures/easyocr_scripts.json`)
  names the same file names and md5 sums as the installed `easyocr.config`;
- opt-in real-weight check (`MS_EASYOCR_MODEL_DIR`) with the real package and
  its download function patched to fail.

Notes:
A fake `easyocr` module is injected into `sys.modules`, so the unit tests need
neither the package, nor Torch, nor weights. The guard and the opt-in test skip
cleanly when the fixture, the package or the variable is absent.
"""

from __future__ import annotations

import importlib.util
import io
import json
import os
import sys
import tempfile
import types
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

from modules.ai_backend.ocr import easy as easy_module
from modules.ai_backend.runtime.model_manager import SmallModelManager
from modules.ai_backend.runtime.paths import program_root

IMAGE_RGB = object()
SCRIPTS_FIXTURE = program_root() / "crates" / "ms-sysprobe" / "fixtures" / "easyocr_scripts.json"


class _FakeReader:
    """Stand-in for `easyocr.Reader`: records its construction and answers `readtext`."""

    def __init__(self, langs: list[str], **kwargs: Any) -> None:
        self.langs = langs
        self.kwargs = kwargs

    def readtext(self, _image: Any, **_kwargs: Any) -> list[Any]:
        return [([0, 0], "안녕", 0.9)]


class EasyOcrOfflineTests(unittest.TestCase):
    def setUp(self) -> None:
        self.constructed: list[_FakeReader] = []
        self.reader_error: BaseException | None = None
        self.fail_gpu_only = False
        test_case = self

        def reader_factory(langs: list[str], **kwargs: Any) -> _FakeReader:
            if test_case.reader_error is not None and (not test_case.fail_gpu_only or kwargs.get("gpu") is not False):
                raise test_case.reader_error
            reader = _FakeReader(langs, **kwargs)
            test_case.constructed.append(reader)
            return reader

        def download_and_unzip(*args: object, **kwargs: object) -> None:
            raise AssertionError(f"download_and_unzip must never be called: {args} {kwargs}")

        fake_easyocr = types.ModuleType("easyocr")
        fake_easyocr.Reader = reader_factory
        fake_easyocr.download_and_unzip = download_and_unzip
        modules_patch = patch.dict(sys.modules, {"easyocr": fake_easyocr, "torch": types.ModuleType("torch")})
        modules_patch.start()
        self.addCleanup(modules_patch.stop)

        self.device = "cpu"
        device_patch = patch.object(easy_module, "_resolve_selected_backend_device", lambda _fallback: self.device)
        device_patch.start()
        self.addCleanup(device_patch.stop)
        decode_patch = patch.object(easy_module.EasyOcrService, "_decode_image_rgb", staticmethod(lambda _b: IMAGE_RGB))
        decode_patch.start()
        self.addCleanup(decode_patch.stop)

        tmp = tempfile.TemporaryDirectory(prefix="ms_easyocr_")
        self.addCleanup(tmp.cleanup)
        self.model_dir = tmp.name
        self.service = easy_module.EasyOcrService(SmallModelManager())

    def _recognize(self, *, langs: str = "ko,en", model_dir: str | None = None) -> dict[str, Any]:
        return self.service.recognize_image_bytes(
            b"png", model_dir=self.model_dir if model_dir is None else model_dir, langs=langs
        )

    def test_reader_loads_offline_from_the_request_dir(self) -> None:
        result = self._recognize()

        self.assertEqual(result, {"lines": ["안녕"], "text": "안녕"})
        self.assertEqual(len(self.constructed), 1)
        reader = self.constructed[0]
        self.assertEqual(reader.langs, ["ko", "en"])
        self.assertEqual(
            reader.kwargs, {"model_storage_directory": self.model_dir, "download_enabled": False, "gpu": False}
        )

    def test_langs_are_used_verbatim(self) -> None:
        # No aliasing any more: Rust owns canonicalization ("zh" is not rewritten).
        self._recognize(langs=" ch_sim , en, ch_sim ")
        self.assertEqual(self.constructed[0].langs, ["ch_sim", "en"])
        with self.assertRaises(ValueError):
            self._recognize(langs=" , ")

    def test_missing_dir_is_not_downloaded_and_not_created(self) -> None:
        missing = str(Path(self.model_dir) / "absent")

        with self.assertRaises(FileNotFoundError) as raised:
            self._recognize(model_dir=missing)

        self.assertIn("not downloaded", str(raised.exception))
        self.assertEqual(self.constructed, [])
        self.assertFalse(Path(missing).exists())
        self.assertEqual(self.service.health()["last_error"], str(raised.exception))

    def test_empty_or_relative_dir_is_refused(self) -> None:
        for bad in ("", "relative/EasyOCR"):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                self._recognize(model_dir=bad)
        self.assertEqual(self.constructed, [])

    def test_missing_weight_file_is_not_downloaded_without_cpu_retry(self) -> None:
        self.device = "cuda"
        attempts: list[object] = []
        self.reader_error = FileNotFoundError("Missing /m/korean_g2.pth and downloads disabled")
        original = sys.modules["easyocr"].Reader

        def counting(langs: list[str], **kwargs: Any) -> _FakeReader:
            attempts.append(kwargs.get("gpu"))
            return original(langs, **kwargs)

        sys.modules["easyocr"].Reader = counting
        with self.assertRaises(FileNotFoundError) as raised:
            self._recognize()

        self.assertIn("not downloaded", str(raised.exception))
        self.assertEqual(attempts, ["cuda"], "a missing file must not be retried on the CPU")

    def test_gpu_failure_still_falls_back_to_cpu(self) -> None:
        self.device = "cuda"
        self.reader_error = RuntimeError("CUDA error: no kernel image")
        self.fail_gpu_only = True

        self._recognize()

        self.assertEqual(self.constructed[0].kwargs["gpu"], False)
        self.assertEqual(self.constructed[0].kwargs["download_enabled"], False)

    def test_dir_is_part_of_the_reader_identity(self) -> None:
        self._recognize()
        other = Path(self.model_dir) / "other"
        other.mkdir()

        self._recognize(model_dir=str(other))

        self.assertEqual([r.kwargs["model_storage_directory"] for r in self.constructed], [self.model_dir, str(other)])


def _load_easyocr_config() -> types.ModuleType | None:
    """Load the installed `easyocr/config.py` alone, without the package's torch import.

    `None` when easyocr is not installed. The module sets an environment
    variable at import; it is restored afterwards.
    """
    spec = importlib.util.find_spec("easyocr")
    if spec is None or not spec.submodule_search_locations:
        return None
    config_path = Path(next(iter(spec.submodule_search_locations))) / "config.py"
    config_spec = importlib.util.spec_from_file_location("_ms_easyocr_config_probe", config_path)
    if config_spec is None or config_spec.loader is None:
        return None
    module = importlib.util.module_from_spec(config_spec)
    with patch.dict(os.environ):
        config_spec.loader.exec_module(module)
    return module


def _fixture_file_entries(node: Any) -> list[tuple[str, str]]:
    """Every `(filename, md5)` pair in the fixture, wherever it is nested.

    An entry is any object carrying a file name (`filename` / `file` /
    `member`) and an md5 (`md5` / `md5sum`).
    """
    found: list[tuple[str, str]] = []
    if isinstance(node, dict):
        name = next((node[k] for k in ("filename", "file", "member") if isinstance(node.get(k), str)), None)
        md5 = next((node[k] for k in ("md5", "md5sum") if isinstance(node.get(k), str)), None)
        if name is not None and md5 is not None:
            found.append((name, md5))
        for value in node.values():
            found.extend(_fixture_file_entries(value))
    elif isinstance(node, list):
        for value in node:
            found.extend(_fixture_file_entries(value))
    return found


class EasyOcrScriptTableGuardTests(unittest.TestCase):
    """Rust's pinned EasyOCR table must match the installed easyocr's own table."""

    def test_fixture_matches_installed_easyocr_config(self) -> None:
        if not SCRIPTS_FIXTURE.is_file():
            self.skipTest(f"fixture absent: {SCRIPTS_FIXTURE}")
        config = _load_easyocr_config()
        if config is None:
            self.skipTest("easyocr is not installed")

        expected: dict[str, str] = {
            config.detection_models["craft"]["filename"]: config.detection_models["craft"]["md5sum"]
        }
        for generation in config.recognition_models.values():
            for model in generation.values():
                expected[model["filename"]] = model["md5sum"]

        entries = _fixture_file_entries(json.loads(SCRIPTS_FIXTURE.read_text(encoding="utf-8")))
        self.assertTrue(entries, f"no {{filename, md5}} entries found in {SCRIPTS_FIXTURE}")
        for name, md5 in entries:
            with self.subTest(file=name):
                self.assertIn(name, expected, f"{name} is not an easyocr {getattr(config, '__file__', '')} model")
                self.assertEqual(md5, expected[name])
        self.assertIn("craft_mlt_25k.pth", {name for name, _ in entries}, "the shared detector must be pinned")


@unittest.skipUnless(os.environ.get("MS_EASYOCR_MODEL_DIR"), "set MS_EASYOCR_MODEL_DIR (craft + korean_g2) to run")
class RealWeightsTests(unittest.TestCase):
    """Opt-in: the real package loads the Korean reader offline from the given directory."""

    def test_loads_offline(self) -> None:
        easyocr_pkg = __import__("easyocr.easyocr", fromlist=["download_and_unzip"])
        from PIL import Image

        def no_download(*args: object, **kwargs: object) -> None:
            raise AssertionError(f"download_and_unzip must never be called: {args}")

        image = Image.new("RGB", (64, 32), "white")
        buffer = io.BytesIO()
        image.save(buffer, format="PNG")
        with patch.object(easyocr_pkg, "download_and_unzip", no_download), patch.object(
            easy_module, "_resolve_selected_backend_device", lambda _fallback: "cpu"
        ):
            service = easy_module.EasyOcrService(SmallModelManager())
            result = service.recognize_image_bytes(
                buffer.getvalue(), model_dir=str(Path(os.environ["MS_EASYOCR_MODEL_DIR"]).resolve()), langs="ko,en"
            )
        self.assertEqual(result["lines"], [])


if __name__ == "__main__":
    unittest.main()
