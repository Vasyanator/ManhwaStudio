"""
File: modules/ai_backend/detection/test_lease_hints.py

Purpose:
Pins that the torch detector services (CTD, Surya detection) hand the residency
arbiter their owner name and a pre-load size hint from the checkpoint they load.

Main responsibilities:
- spy on `SmallModelManager.begin_model_use` and check `owner=` / `hint=`;
- CTD: the hint is the checkpoint file's size on the leased device;
- Surya: the hint is the checkpoint directory's size; a relative directory (refused
  by the load) is never sized and gets an unknown-size hint;
- Paddle: the ONNX session is leased BEFORE the instance lock is taken (owner-rule
  premise, S-059) and the forward pass runs under it.

Notes:
- The load and forward steps are stubbed, so no torch, weights or GPU are needed;
  every device is `cpu` (host-accounted). Scratch files live in a temp directory
  removed by each test.
"""

from __future__ import annotations

import tempfile
import threading
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import patch

import numpy as np

from modules.ai_backend.detection import ctd, surya
from modules.ai_backend.detection import paddle as paddle_detector
from modules.ai_backend.engines import paddle_onnx
from modules.ai_backend.engines.paddle_onnx import ProviderSettings
from modules.ai_backend.runtime.model_manager import SmallModelManager
from modules.ai_backend.runtime.residency.holdings import FootprintHint


def _free_for_another_thread(lock: Any) -> bool:
    """True when another thread can take `lock` right now (works for `Lock` and `RLock`)."""
    result: list[bool] = []

    def probe() -> None:
        acquired = lock.acquire(blocking=False)
        if acquired:
            lock.release()
        result.append(acquired)

    thread = threading.Thread(target=probe)
    thread.start()
    thread.join()
    return result[0]


class _LockProbingFactory:
    """Duck-typed `RuntimeFactory`: records, per acquire, whether the service lock was free."""

    def __init__(self, runner: Any) -> None:
        self.runner = runner
        self.service_lock: Any = None
        self.lock_free_at_acquire: list[bool] = []
        self.released = 0

    def acquire_runner(self, _model_path: Path, _settings: ProviderSettings) -> Any:
        self.lock_free_at_acquire.append(_free_for_another_thread(self.service_lock))
        return SimpleNamespace(runner=self.runner, release=self._release)

    def _release(self) -> None:
        self.released += 1


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


class _ProbingDetRunner:
    """Fake detection runner: records whether the service lock was free during `run`."""

    selected_provider = "CPUExecutionProvider"

    def __init__(self) -> None:
        self.service_lock: Any = None
        self.lock_free_at_run: list[bool] = []

    def run(self, det_input: np.ndarray) -> np.ndarray:
        self.lock_free_at_run.append(_free_for_another_thread(self.service_lock))
        n, _channels, height, width = det_input.shape
        return np.zeros((n, 1, height, width), dtype=np.float32)


class PaddleLeaseOrderTests(unittest.TestCase):
    def test_the_session_is_leased_before_the_instance_lock(self) -> None:
        tmp = tempfile.TemporaryDirectory(prefix="ms-detection-order-")
        self.addCleanup(tmp.cleanup)
        runner = _ProbingDetRunner()
        factory = _LockProbingFactory(runner)
        service = paddle_detector.PaddleTextDetectorService(factory)  # type: ignore[arg-type]
        factory.service_lock = service._lock
        runner.service_lock = service._lock
        with patch.object(paddle_onnx, "resolve_det_model_path", lambda: Path(tmp.name) / "det.onnx"), \
                patch.object(paddle_detector, "resolve_provider_settings", lambda _cfg: ProviderSettings("CPUExecutionProvider", "0")):
            maps = service.forward_tiles(np.zeros((1, 32, 32, 3), dtype=np.uint8))
        self.assertEqual(maps.shape, (1, 1, 32, 32))
        self.assertEqual(factory.lock_free_at_acquire, [True])
        self.assertEqual(runner.lock_free_at_run, [False])
        self.assertEqual(factory.released, 1)


if __name__ == "__main__":
    unittest.main()
