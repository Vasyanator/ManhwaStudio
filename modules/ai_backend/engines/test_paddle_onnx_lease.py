"""
File: modules/ai_backend/engines/test_paddle_onnx_lease.py

Purpose:
Pins how the shared ONNX session cache (`RuntimeFactory`) describes its leases
to the residency arbiter.

Main responsibilities:
- `ProviderSettings.footprint_device`: GPU EPs (CUDA, MIGraphX) map to
  `cuda:<device_id>`, every other EP to `cpu`;
- `RuntimeFactory.acquire_runner` passes no owner (per-key default) and the
  `.onnx` file's size (x1.25, ONNX runtime) on that device;
- `PaddleOnnxRuntime.recognition_sessions` releases the detection lease when
  the recognition acquire raises (a refused admission must not leak a lease),
  and releases both leases when the caller's block raises.

Notes:
- The session build is stubbed (`OnnxSessionRunner`), so onnxruntime, weights
  and a GPU are not needed; the EP is the CPU one, so the hint is
  host-accounted. Scratch files live in a temp directory removed by the test.
"""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import patch

from modules.ai_backend.engines import paddle_onnx
from modules.ai_backend.engines.paddle_onnx import PaddleOnnxRuntime, ProviderSettings, ResolvedModelPaths, RuntimeFactory
from modules.ai_backend.runtime.model_manager import SmallModelManager
from modules.ai_backend.runtime.residency.holdings import FootprintHint, Runtime


class FootprintDeviceTests(unittest.TestCase):
    def test_gpu_eps_map_to_their_cuda_device_and_the_rest_to_cpu(self) -> None:
        self.assertEqual(ProviderSettings("CUDAExecutionProvider", "1").footprint_device(), "cuda:1")
        self.assertEqual(ProviderSettings("MIGraphXExecutionProvider", "0").footprint_device(), "cuda:0")
        self.assertEqual(ProviderSettings("CPUExecutionProvider", "0").footprint_device(), "cpu")
        self.assertEqual(ProviderSettings("DmlExecutionProvider", "0").footprint_device(), "cpu")


class _StubRunner:
    """Stands in for `OnnxSessionRunner`: no onnxruntime session is built."""

    def __init__(self, model_path: Path, settings: ProviderSettings) -> None:
        self.model_path = model_path
        self.settings = settings


class RuntimeFactoryLeaseHintTests(unittest.TestCase):
    def test_the_onnx_file_size_and_owner_reach_the_manager(self) -> None:
        tmp = tempfile.TemporaryDirectory(prefix="ms-onnx-hint-")
        self.addCleanup(tmp.cleanup)
        model = Path(tmp.name) / "rec.onnx"
        model.write_bytes(b"x" * 800)
        manager = SmallModelManager(max_loaded_models=2)
        factory = RuntimeFactory(manager)
        seen: list[tuple[Any, Any]] = []
        real = manager.begin_model_use

        def spy(key: str, unload_callback: Any = None, *, owner: Any = None, hint: Any = None) -> Any:
            seen.append((owner, hint))
            return real(key, unload_callback, owner=owner, hint=hint)

        with patch.object(manager, "begin_model_use", spy), \
                patch.object(paddle_onnx, "OnnxSessionRunner", _StubRunner), \
                patch.object(paddle_onnx, "_configure_onnx_cache_environment", lambda *_args: None), \
                patch.object(paddle_onnx, "resolve_compiled_cache_root", lambda: Path(tmp.name)):
            managed = factory.acquire_runner(model, ProviderSettings("CPUExecutionProvider", "0"))
            managed.release()
        # No owner: each session is its own owner (the key), today's evictability.
        self.assertEqual(seen, [(None, FootprintHint(device="cpu", device_bytes=0, host_bytes=1000, runtime=Runtime.ONNX))])


class _RefusingRecFactory:
    """Duck-typed `RuntimeFactory`: acquires after the `refuse_after`-th raise (`None`: never)."""

    def __init__(self, refuse_after: int | None = 1) -> None:
        self.refuse_after = refuse_after
        self.acquired = 0
        self.released = 0

    def acquire_runner(self, _model_path: Path, _settings: ProviderSettings) -> Any:
        self.acquired += 1
        if self.refuse_after is not None and self.acquired > self.refuse_after:
            raise RuntimeError("rec admission refused")
        return SimpleNamespace(runner=object(), release=self._release)

    def _release(self) -> None:
        self.released += 1


class RecognizeLeaseLeakTests(unittest.TestCase):
    def setUp(self) -> None:
        paths = ResolvedModelPaths(det_model_path=Path("/m/det.onnx"), det_config_path=Path("/m/det.json"), rec_model_path=Path("/m/rec.onnx"), rec_dict_path=Path("/m/dict.txt"))
        for name, value in (("resolve_model_paths", lambda _key: paths), ("parse_det_config", lambda _path: None), ("parse_rec_config", lambda _path: None)):
            patcher = patch.object(paddle_onnx, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_a_failed_rec_acquire_releases_the_det_lease(self) -> None:
        factory = _RefusingRecFactory()
        runtime = PaddleOnnxRuntime(factory)  # type: ignore[arg-type]
        with self.assertRaisesRegex(RuntimeError, "rec admission refused"):
            with runtime.recognition_sessions("ko", ProviderSettings("CPUExecutionProvider", "0")):
                self.fail("the block must not run when an acquire fails")
        self.assertEqual((factory.acquired, factory.released), (2, 1))

    def test_a_raising_block_releases_both_leases(self) -> None:
        factory = _RefusingRecFactory(refuse_after=None)
        runtime = PaddleOnnxRuntime(factory)  # type: ignore[arg-type]
        with self.assertRaisesRegex(ValueError, "inference failed"):
            with runtime.recognition_sessions("ko", ProviderSettings("CPUExecutionProvider", "0")):
                raise ValueError("inference failed")
        self.assertEqual((factory.acquired, factory.released), (2, 2))


if __name__ == "__main__":
    unittest.main()
