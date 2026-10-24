"""
File: modules/ai_backend/detection/test_surya_service.py

Purpose:
Unit tests for the offline checkpoint contract of `SuryaTextDetectorService`:
the predictor loads from the request's `model_dir` (as `checkpoint=` and in
Surya's settings), a missing or incomplete directory is a "not downloaded"
error before any load, nothing is downloaded, the directory is part of the
predictor identity, `health()` reports the resident directory instead of
probing a library cache, and a reload on another device (or a failed reload
from another directory) tells the model manager the old predictor is gone.

Notes:
Fake `surya.*` and `torch` modules are injected into `sys.modules` and the
forward pass itself is stubbed (`_forward_with_predictor`, covered by
`test_services.py`), so neither the package nor Torch nor weights are needed.
The opt-in `RealCheckpointTests` load the real detector from
`MS_SURYA_DETECTION_DIR` with `download_directory` patched to fail.
"""

from __future__ import annotations

import os
import sys
import tempfile
import types
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

import numpy as np

from modules.ai_backend.detection import surya as surya_detector
from modules.ai_backend.runtime.model_manager import SmallModelManager

S3_DETECTOR_DEFAULT = "s3://text_detection/2025_05_07"


class SuryaDetectorCheckpointTests(unittest.TestCase):
    def setUp(self) -> None:
        self.constructed: list[dict[str, Any]] = []
        self.fail_construction = False
        test_case = self
        settings = types.SimpleNamespace(DETECTOR_MODEL_CHECKPOINT=S3_DETECTOR_DEFAULT)
        self.settings = settings

        class DetectionPredictor:
            def __init__(self, checkpoint: object = None, device: object = None, **kwargs: object) -> None:
                test_case.constructed.append(
                    {"checkpoint": checkpoint, "device": device, "setting": settings.DETECTOR_MODEL_CHECKPOINT}
                )
                if test_case.fail_construction:
                    raise RuntimeError("simulated detector load failure")

        def download_directory(remote_dir: str, local_dir: str) -> None:
            raise AssertionError(f"download_directory must never be called ({remote_dir} -> {local_dir})")

        tmp = tempfile.TemporaryDirectory(prefix="ms_surya_det_")
        self.addCleanup(tmp.cleanup)
        self.model_dir = str(Path(tmp.name) / "text_detection")
        Path(self.model_dir).mkdir()
        self.ready = {self.model_dir}

        surya = types.ModuleType("surya")
        detection_mod = types.ModuleType("surya.detection")
        detection_mod.DetectionPredictor = DetectionPredictor
        settings_mod = types.ModuleType("surya.settings")
        settings_mod.settings = settings
        common_mod = types.ModuleType("surya.common")
        s3_mod = types.ModuleType("surya.common.s3")
        s3_mod.check_manifest = lambda local_dir: local_dir in test_case.ready
        s3_mod.download_directory = download_directory
        modules_patch = patch.dict(
            sys.modules,
            {
                "surya": surya,
                "surya.detection": detection_mod,
                "surya.settings": settings_mod,
                "surya.common": common_mod,
                "surya.common.s3": s3_mod,
                "torch": types.ModuleType("torch"),
            },
        )
        modules_patch.start()
        self.addCleanup(modules_patch.stop)

        for target, value in (
            ("_resolve_selected_backend_device", lambda _fallback: "cpu"),
            ("_forward_with_predictor", lambda _predictor, tiles: np.zeros((tiles.shape[0], 1, 1, 1), dtype=np.uint8)),
        ):
            attr_patch = patch.object(surya_detector, target, value)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)

        self.service = surya_detector.SuryaTextDetectorService(SmallModelManager())
        self.tiles = np.zeros((1, 4, 4, 3), dtype=np.uint8)

    def test_predictor_loads_the_local_dir(self) -> None:
        self.service.forward_tiles(self.tiles, model_dir=self.model_dir)

        self.assertEqual(
            self.constructed, [{"checkpoint": self.model_dir, "device": "cpu", "setting": self.model_dir}]
        )
        self.assertEqual(
            self.service.health(),
            {"model": "surya_detection", "ready": True, "device": "cpu", "model_dir": self.model_dir, "last_error": None},
        )

    def test_resident_predictor_is_reused_and_a_new_dir_reloads(self) -> None:
        self.service.forward_tiles(self.tiles, model_dir=self.model_dir)
        self.service.forward_tiles(self.tiles, model_dir=self.model_dir)
        self.assertEqual(len(self.constructed), 1)

        other = str(Path(self.model_dir).parent / "other")
        Path(other).mkdir()
        self.ready.add(other)
        self.service.forward_tiles(self.tiles, model_dir=other)

        self.assertEqual([c["checkpoint"] for c in self.constructed], [self.model_dir, other])
        self.assertEqual(self.service.health()["model_dir"], other)

    def test_a_device_change_reports_the_old_predictor_unloaded(self) -> None:
        unloaded: list[str] = []
        with patch.object(self.service._model_manager, "mark_unloaded", unloaded.append):
            with self.service._lock:
                self.service._ensure_predictor_locked("cpu", self.model_dir)
                self.service._ensure_predictor_locked("mps", self.model_dir)

        self.assertEqual(unloaded, [surya_detector.SuryaTextDetectorService._model_key("cpu")])
        self.assertEqual([c["device"] for c in self.constructed], ["cpu", "mps"])

    def test_a_failed_reload_from_another_dir_leaves_nothing_resident(self) -> None:
        self.service.forward_tiles(self.tiles, model_dir=self.model_dir)
        model_key = surya_detector.SuryaTextDetectorService._model_key("cpu")
        self.assertTrue(self.service._model_manager.holdings()[model_key].resident)
        other = str(Path(self.model_dir).parent / "other")
        Path(other).mkdir()
        self.ready.add(other)
        self.fail_construction = True

        with self.assertRaises(RuntimeError):
            self.service.forward_tiles(self.tiles, model_dir=other)

        # The lease found the key resident, so only the service can say it is gone.
        self.assertEqual(self.service._model_manager.holdings(), {})

    def test_incomplete_dir_is_not_downloaded(self) -> None:
        self.ready.clear()

        with self.assertRaises(FileNotFoundError) as raised:
            self.service.forward_tiles(self.tiles, model_dir=self.model_dir)

        self.assertIn("not downloaded", str(raised.exception))
        self.assertEqual(self.constructed, [])
        self.assertEqual(self.settings.DETECTOR_MODEL_CHECKPOINT, S3_DETECTOR_DEFAULT)
        health = self.service.health()
        self.assertFalse(health["ready"])
        self.assertIsNone(health["model_dir"])
        self.assertEqual(health["last_error"], str(raised.exception))

    def test_missing_or_relative_dir_is_refused(self) -> None:
        missing = str(Path(self.model_dir) / "absent")
        with self.assertRaises(FileNotFoundError):
            self.service.forward_tiles(self.tiles, model_dir=missing)
        with self.assertRaises(ValueError):
            self.service.forward_tiles(self.tiles, model_dir="relative/dir")
        self.assertEqual(self.constructed, [])
        self.assertFalse(Path(missing).exists())


@unittest.skipUnless(os.environ.get("MS_SURYA_DETECTION_DIR"), "set MS_SURYA_DETECTION_DIR to run")
class RealCheckpointTests(unittest.TestCase):
    """Opt-in: the real Surya detector loads offline from the given directory."""

    def test_forward_runs_offline(self) -> None:
        import surya.common.s3 as s3

        def no_download(*args: object, **kwargs: object) -> None:
            raise AssertionError(f"download_directory must never be called: {args}")

        model_dir = str(Path(os.environ["MS_SURYA_DETECTION_DIR"]).resolve())
        with patch.object(s3, "download_directory", no_download), patch.object(
            surya_detector, "_resolve_selected_backend_device", lambda _fallback: "cpu"
        ):
            service = surya_detector.SuryaTextDetectorService(SmallModelManager())
            maps = service.forward_tiles(np.full((1, 64, 64, 3), 255, dtype=np.uint8), model_dir=model_dir)
        self.assertEqual(maps.shape, (1, 1, 16, 16))
        self.assertEqual(service.health()["model_dir"], model_dir)


if __name__ == "__main__":
    unittest.main()
