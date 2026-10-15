"""
File: modules/ai_backend/engines/test_surya_checkpoints.py

Purpose:
Unit tests for the shared Surya checkpoint presence helpers used by
`ocr/surya.py` and `detection/surya.py`.

Main responsibilities:
- verify a complete local checkpoint is accepted and nothing is downloaded;
- verify an empty, relative, missing or incomplete directory raises a distinct,
  explicit error ("not downloaded" for the last two);
- verify an absent Surya package never degrades into an optimistic "ready";
- verify `point_settings_at` assigns exactly the requested settings.

Notes:
A fake `surya.common.s3` module is injected into `sys.modules`, so the tests
need neither the Surya package nor network access. Its `download_directory`
raises, so any download attempt fails the test.
"""

from __future__ import annotations

import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch

from modules.ai_backend.engines import surya_checkpoints as ckpt


def _fake_s3_module(*, ready: set[str]):
    """Build a `surya.common.s3` substitute backed by a mutable `ready` set."""

    def check_manifest(local_dir: str) -> bool:
        return local_dir in ready

    def download_directory(remote_dir: str, local_dir: str) -> None:
        raise AssertionError(f"download_directory must never be called ({remote_dir} -> {local_dir})")

    module = types.ModuleType("surya.common.s3")
    module.check_manifest = check_manifest
    module.download_directory = download_directory
    return module


class SuryaCheckpointsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.ready: set[str] = set()
        surya = types.ModuleType("surya")
        common = types.ModuleType("surya.common")
        modules_patch = patch.dict(
            sys.modules,
            {"surya": surya, "surya.common": common, "surya.common.s3": _fake_s3_module(ready=self.ready)},
        )
        modules_patch.start()
        self.addCleanup(modules_patch.stop)
        tmp = tempfile.TemporaryDirectory(prefix="ms_surya_ckpt_")
        self.addCleanup(tmp.cleanup)
        self.local_dir = str(Path(tmp.name) / "text_recognition" / "2025_09_23")
        Path(self.local_dir).mkdir(parents=True)

    def test_complete_checkpoint_is_accepted(self) -> None:
        self.ready.add(self.local_dir)

        self.assertEqual(ckpt.require_local_checkpoint(self.local_dir, label="Surya OCR recognition"), self.local_dir)
        self.assertTrue(ckpt.checkpoint_ready(self.local_dir))

    def test_incomplete_manifest_is_not_downloaded(self) -> None:
        with self.assertRaises(FileNotFoundError) as raised:
            ckpt.require_local_checkpoint(self.local_dir, label="Surya detector")

        self.assertIn("not downloaded", str(raised.exception))
        self.assertIn("Surya detector", str(raised.exception))

    def test_missing_directory_is_not_downloaded(self) -> None:
        missing = str(Path(self.local_dir) / "absent")

        with self.assertRaises(FileNotFoundError) as raised:
            ckpt.require_local_checkpoint(missing, label="Surya detector")

        self.assertIn("not downloaded", str(raised.exception))
        self.assertIn(missing, str(raised.exception))
        self.assertFalse(Path(missing).exists(), "the check must not create the directory")

    def test_empty_and_relative_paths_are_rejected(self) -> None:
        for bad in ("", "   ", "relative/dir"):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                ckpt.require_local_checkpoint(bad, label="Surya detector")

    def test_point_settings_at_assigns_only_the_given_dirs(self) -> None:
        settings = types.SimpleNamespace(
            DETECTOR_MODEL_CHECKPOINT="s3://text_detection/x",
            FOUNDATION_MODEL_CHECKPOINT="s3://text_recognition/x",
            RECOGNITION_MODEL_CHECKPOINT="s3://text_recognition/x",
        )
        settings_mod = types.ModuleType("surya.settings")
        settings_mod.settings = settings
        with patch.dict(sys.modules, {"surya.settings": settings_mod}):
            ckpt.point_settings_at(detection_dir="/m/det")
            self.assertEqual(settings.DETECTOR_MODEL_CHECKPOINT, "/m/det")
            self.assertEqual(settings.FOUNDATION_MODEL_CHECKPOINT, "s3://text_recognition/x")

            ckpt.point_settings_at(recognition_dir="/m/rec")
            self.assertEqual(settings.FOUNDATION_MODEL_CHECKPOINT, "/m/rec")
            self.assertEqual(settings.RECOGNITION_MODEL_CHECKPOINT, "/m/rec")
            self.assertEqual(settings.DETECTOR_MODEL_CHECKPOINT, "/m/det")


class SuryaAbsentTests(unittest.TestCase):
    """Without the Surya package the helpers must say "unknown", not "ready"."""

    def setUp(self) -> None:
        # An empty `surya` package makes `from surya.common.s3 import ...` fail
        # exactly like an installation without Surya.
        modules_patch = patch.dict(
            sys.modules,
            {
                "surya": types.ModuleType("surya"),
                "surya.common": None,
                "surya.common.s3": None,
                "surya.settings": None,
            },
        )
        modules_patch.start()
        self.addCleanup(modules_patch.stop)
        tmp = tempfile.TemporaryDirectory(prefix="ms_surya_ckpt_")
        self.addCleanup(tmp.cleanup)
        self.local_dir = tmp.name

    def test_checkpoint_is_never_reported_ready(self) -> None:
        self.assertFalse(ckpt.checkpoint_ready(self.local_dir))
        self.assertFalse(ckpt.checkpoint_ready(""))

    def test_require_raises_runtime_error(self) -> None:
        with self.assertRaises(RuntimeError) as raised:
            ckpt.require_local_checkpoint(self.local_dir, label="Surya OCR recognition")
        self.assertIn("Surya OCR recognition", str(raised.exception))

    def test_point_settings_at_raises_runtime_error(self) -> None:
        with self.assertRaises(RuntimeError):
            ckpt.point_settings_at(detection_dir="/m/det")


if __name__ == "__main__":
    unittest.main()
