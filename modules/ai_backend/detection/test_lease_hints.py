"""
File: modules/ai_backend/detection/test_lease_hints.py

Purpose:
Pins that the torch detector services (CTD, Surya detection) hand the residency
arbiter their owner name and a pre-load size hint from the checkpoint they load.

Main responsibilities:
- spy on `SmallModelManager.begin_model_use` and check `owner=` / `hint=`;
- CTD: the hint is the checkpoint file's size on the leased device;
- Surya: the hint is the checkpoint directory's size; a relative directory (refused
  by the load) is never sized and gets an unknown-size hint.

Notes:
- The load and forward steps are stubbed, so no torch, weights or GPU are needed;
  every device is `cpu` (host-accounted). Scratch files live in a temp directory
  removed by each test.
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

import numpy as np

from modules.ai_backend.detection import ctd, surya
from modules.ai_backend.runtime.model_manager import SmallModelManager
from modules.ai_backend.runtime.residency.holdings import FootprintHint


def _spy(manager: SmallModelManager, seen: list[tuple[Any, Any]]) -> Any:
    """A `begin_model_use` stand-in that records `(owner, hint)` and then takes the real lease."""
    real = manager.begin_model_use

    def spy(key: str, unload_callback: Any = None, *, owner: Any = None, hint: Any = None) -> Any:
        seen.append((owner, hint))
        return real(key, unload_callback, owner=owner, hint=hint)

    return spy


class DetectionLeaseHintTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="ms-detection-hints-")
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.manager = SmallModelManager(max_loaded_models=2)
        self.seen: list[tuple[Any, Any]] = []

    def test_ctd_hint_is_the_checkpoint_file(self) -> None:
        checkpoint = self.root / "ctd.pt"
        checkpoint.write_bytes(b"x" * 900)
        service = ctd.CtdTextDetectorService(self.manager)
        service._model_path = checkpoint
        maps = np.zeros((1, 2, 64, 64), dtype=np.uint8)
        with patch.object(self.manager, "begin_model_use", _spy(self.manager, self.seen)), \
                patch.object(ctd, "_resolve_selected_backend_device", lambda _fallback: "cpu"), \
                patch.object(service, "_ensure_net_locked", lambda *_args: object()), \
                patch.object(service, "_forward_locked", lambda *_args: maps):
            service.forward_tiles(np.zeros((1, 64, 64, 3), dtype=np.uint8))
        self.assertEqual(self.seen, [(ctd._LEASE_OWNER, FootprintHint(device="cpu", device_bytes=0, host_bytes=900))])

    def _surya_forward(self, model_dir: str) -> None:
        service = surya.SuryaTextDetectorService(self.manager)
        maps = np.zeros((1, 1, 1, 1), dtype=np.uint8)
        with patch.object(self.manager, "begin_model_use", _spy(self.manager, self.seen)), \
                patch.object(surya, "_resolve_selected_backend_device", lambda _fallback: "cpu"), \
                patch.object(service, "_ensure_predictor_locked", lambda *_args: object()), \
                patch.object(surya, "_forward_with_predictor", lambda *_args: maps):
            service.forward_tiles(np.zeros((1, 4, 4, 3), dtype=np.uint8), model_dir=model_dir)

    def test_surya_hint_is_the_checkpoint_directory(self) -> None:
        (self.root / "model.safetensors").write_bytes(b"x" * 300)
        (self.root / "config.json").write_bytes(b"x" * 20)
        self._surya_forward(str(self.root))
        self.assertEqual(self.seen, [(surya._LEASE_OWNER, FootprintHint(device="cpu", device_bytes=0, host_bytes=320))])

    def test_surya_relative_directory_is_never_sized(self) -> None:
        self._surya_forward("")
        self.assertEqual(self.seen, [(surya._LEASE_OWNER, FootprintHint(device="cpu"))])


if __name__ == "__main__":
    unittest.main()
