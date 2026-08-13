"""
File: modules/ai_backend/engines/test_surya_checkpoints.py

Purpose:
Unit tests for the shared Surya checkpoint presence/download helpers used by
`ocr/surya.py` and `detection/surya.py`.

Main responsibilities:
- verify a complete checkpoint costs one manifest check and no download;
- verify an incomplete `s3://` checkpoint is downloaded and re-verified;
- verify an incomplete download and a missing local checkpoint raise distinct,
  explicit errors;
- verify an absent Surya package never degrades into an optimistic "ready".

Notes:
A fake `surya.common.s3` module is injected into `sys.modules`, so the tests
need neither the Surya package nor network access.
"""

from __future__ import annotations

import sys
import types
import unittest
from unittest.mock import patch

from modules.ai_backend.engines import surya_checkpoints as ckpt

CHECKPOINT = "s3://text_recognition/2025_09_23"
LOCAL_DIR = "/cache/text_recognition/2025_09_23"


def _fake_s3_module(*, ready: set[str], downloads: list[tuple[str, str]]):
    """Build a `surya.common.s3` substitute backed by a mutable `ready` set."""

    class S3DownloaderMixin:
        @staticmethod
        def get_local_path(checkpoint: str) -> str:
            return "/cache/" + checkpoint.removeprefix("s3://")

    def check_manifest(local_dir: str) -> bool:
        return local_dir in ready

    def download_directory(remote_dir: str, local_dir: str) -> None:
        downloads.append((remote_dir, local_dir))

    module = types.ModuleType("surya.common.s3")
    module.S3DownloaderMixin = S3DownloaderMixin
    module.check_manifest = check_manifest
    module.download_directory = download_directory
    return module


class SuryaCheckpointsTests(unittest.TestCase):
    def setUp(self) -> None:
        self.ready: set[str] = set()
        self.downloads: list[tuple[str, str]] = []
        s3_module = _fake_s3_module(ready=self.ready, downloads=self.downloads)

        surya = types.ModuleType("surya")
        common = types.ModuleType("surya.common")
        modules_patch = patch.dict(
            sys.modules,
            {"surya": surya, "surya.common": common, "surya.common.s3": s3_module},
        )
        modules_patch.start()
        self.addCleanup(modules_patch.stop)

    def test_local_dir_resolves_through_surya(self) -> None:
        self.assertEqual(ckpt.checkpoint_local_dir(CHECKPOINT), LOCAL_DIR)

    def test_local_dir_passes_through_a_non_s3_path(self) -> None:
        self.assertEqual(ckpt.checkpoint_local_dir("/opt/models/detector"), "/opt/models/detector")

    def test_complete_checkpoint_is_not_downloaded(self) -> None:
        self.ready.add(LOCAL_DIR)

        ckpt.ensure_checkpoint_downloaded(CHECKPOINT, label="Surya OCR foundation")

        self.assertEqual(self.downloads, [])

    def test_missing_checkpoint_is_downloaded_and_reverified(self) -> None:
        downloads = self.downloads
        ready = self.ready

        def complete_on_download(remote_dir: str, local_dir: str) -> None:
            downloads.append((remote_dir, local_dir))
            ready.add(local_dir)

        sys.modules["surya.common.s3"].download_directory = complete_on_download

        ckpt.ensure_checkpoint_downloaded(CHECKPOINT, label="Surya OCR foundation")

        self.assertEqual(self.downloads, [("text_recognition/2025_09_23", LOCAL_DIR)])

    def test_incomplete_download_raises(self) -> None:
        # `download_directory` returns without completing the manifest.
        with self.assertRaises(RuntimeError) as raised:
            ckpt.ensure_checkpoint_downloaded(CHECKPOINT, label="Surya detector")

        self.assertIn("Surya detector", str(raised.exception))
        self.assertEqual(len(self.downloads), 1)

    def test_missing_local_checkpoint_raises_file_not_found(self) -> None:
        with self.assertRaises(FileNotFoundError):
            ckpt.ensure_checkpoint_downloaded("/opt/models/detector", label="Surya detector")
        self.assertEqual(self.downloads, [])


class SuryaAbsentTests(unittest.TestCase):
    """Without the Surya package the helpers must say "unknown", not "ready"."""

    def setUp(self) -> None:
        # An empty `surya` package makes `from surya.common.s3 import ...` fail
        # exactly like an installation without Surya.
        modules_patch = patch.dict(
            sys.modules,
            {"surya": types.ModuleType("surya"), "surya.common": None, "surya.common.s3": None},
        )
        modules_patch.start()
        self.addCleanup(modules_patch.stop)

    def test_local_dir_is_unknown(self) -> None:
        self.assertEqual(ckpt.checkpoint_local_dir(CHECKPOINT), "")

    def test_checkpoint_is_never_reported_ready(self) -> None:
        self.assertFalse(ckpt.checkpoint_ready(LOCAL_DIR))
        self.assertFalse(ckpt.checkpoint_ready(""))

    def test_download_raises_runtime_error(self) -> None:
        with self.assertRaises(RuntimeError) as raised:
            ckpt.ensure_checkpoint_downloaded(CHECKPOINT, label="Surya OCR foundation")
        self.assertIn("Surya OCR foundation", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
