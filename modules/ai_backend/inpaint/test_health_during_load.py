"""
File: modules/ai_backend/inpaint/test_health_during_load.py

Purpose:
Pins S-014 for the inpaint service shape, where ONE instance-lock section spans
the model load AND the inference: `health()` must answer while that section
runs, show `ready=False` during the load, `ready=True` as soon as the load
returned (inference still running), and the error after a failed load.

Notes:
AOT is the representative: its real `_ensure_model_locked` runs, with
`load_aot_model` replaced by a stub that blocks on an Event and
`_inpaint_locked` by one that blocks on a second Event. The checkpoint path
points at a temporary file. No torch, no weights, no GPU.
"""

from __future__ import annotations

import io
import tempfile
import threading
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

import numpy as np
from PIL import Image

from modules.ai_backend.inpaint import aot
from modules.ai_backend.runtime.model_manager import LoadedModelManager


def _png_bytes(mode: str, color: object) -> bytes:
    with io.BytesIO() as buffer:
        Image.new(mode, (16, 16), color).save(buffer, format="PNG")
        return buffer.getvalue()


class AotHealthDuringLoadTests(unittest.TestCase):
    def setUp(self) -> None:
        self.load_started = threading.Event()
        self.load_gate = threading.Event()
        self.inference_started = threading.Event()
        self.inference_gate = threading.Event()
        self.fail_load = False
        # Never leave a worker blocked on a gate if an assertion fails.
        self.addCleanup(self.load_gate.set)
        self.addCleanup(self.inference_gate.set)
        tmp = tempfile.TemporaryDirectory(prefix="ms_aot_health_")
        self.addCleanup(tmp.cleanup)
        checkpoint = Path(tmp.name) / "inpainting.ckpt"
        checkpoint.write_bytes(b"x")

        for target, replacement in (
            ("load_aot_model", self._blocking_load),
            ("_resolve_selected_backend_device", lambda _fallback: "cpu"),
            ("_clear_torch_cache", lambda: None),
        ):
            patcher = mock.patch.object(aot, target, replacement)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.service = aot.AotInpaintService(LoadedModelManager())
        self.service._model_path = checkpoint
        inference = mock.patch.object(self.service, "_inpaint_locked", self._blocking_inference)
        inference.start()
        self.addCleanup(inference.stop)
        self.errors: list[BaseException] = []

    def _blocking_load(self, _path: str, _device: str) -> object:
        self.load_started.set()
        if not self.load_gate.wait(10.0):
            raise RuntimeError("test load gate never opened")
        if self.fail_load:
            raise RuntimeError("stub load failure")
        return object()

    def _blocking_inference(self, _model: object, **_kwargs: Any) -> np.ndarray:
        self.inference_started.set()
        if not self.inference_gate.wait(10.0):
            raise RuntimeError("test inference gate never opened")
        return np.zeros((16, 16, 3), dtype=np.uint8)

    def _start_request(self) -> threading.Thread:
        def run() -> None:
            try:
                self.service.inpaint_image_bytes(_png_bytes("RGB", (128, 128, 128)), _png_bytes("L", 255))
            except BaseException as exc:  # noqa: BLE001 - surfaced by the test
                self.errors.append(exc)

        worker = threading.Thread(target=run, daemon=True)
        worker.start()
        self.assertTrue(self.load_started.wait(5.0), "the load never started")
        return worker

    def _health_within(self, seconds: float) -> dict[str, Any]:
        results: list[dict[str, Any]] = []
        reader = threading.Thread(target=lambda: results.append(self.service.health()), daemon=True)
        reader.start()
        reader.join(seconds)
        self.assertFalse(reader.is_alive(), "health() blocked on the instance lock")
        return results[0]

    def test_health_answers_during_load_and_inference(self) -> None:
        worker = self._start_request()
        during_load = self._health_within(2.0)
        self.assertFalse(during_load["ready"])
        self.assertIsNone(during_load["last_error"])

        self.load_gate.set()
        self.assertTrue(self.inference_started.wait(5.0))
        during_inference = self._health_within(2.0)
        self.assertTrue(during_inference["ready"])
        self.assertEqual(during_inference["device"], "cpu")

        self.inference_gate.set()
        worker.join(5.0)
        self.assertFalse(worker.is_alive())
        self.assertEqual(self.errors, [])
        self.assertTrue(self.service.health()["ready"])

    def test_health_reports_the_error_of_a_failed_load(self) -> None:
        self.fail_load = True
        worker = self._start_request()
        self.assertFalse(self._health_within(2.0)["ready"])
        self.load_gate.set()
        worker.join(5.0)
        self.assertFalse(worker.is_alive())
        self.assertEqual(len(self.errors), 1)
        health = self.service.health()
        self.assertFalse(health["ready"])
        self.assertIn("stub load failure", health["last_error"])


if __name__ == "__main__":
    unittest.main()
