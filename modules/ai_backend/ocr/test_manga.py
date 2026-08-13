"""
File: modules/ai_backend/ocr/test_manga.py

Purpose:
Unit tests for the MangaOCR PyTorch runtime's device placement contract.

Main responsibilities:
- verify the CUDA/ROCm placement goes through
  `runtime.rocm_mmap_transfer.move_module_to`
  instead of `Module.cuda()`, because `MangaOcrModel.from_pretrained` requests no
  dtype and therefore hands out mmap-backed weights;
- verify `force_cpu=True` leaves the model on the CPU;
- verify the MPS branch is untouched by the workaround;
- verify `close()` releases every object the runtime holds and leaves it unusable.

Notes:
`ocr.manga` imports `jaconv` at module scope and the runtime imports
`torch`, `manga_ocr` and `transformers` lazily. The tests skip when `jaconv` is
absent and substitute fake modules for the lazy imports, so no weights, GPU or
heavy packages are required.
"""

from __future__ import annotations

import sys
import types
import unittest
from typing import Any
from unittest import mock


def _import_service_module():
    """Import `modules.ai_backend.ocr.manga`, skipping when `jaconv` is absent."""
    try:
        from modules.ai_backend.ocr import manga
    except ImportError as exc:  # pragma: no cover - environment specific
        raise unittest.SkipTest(
            f"modules.ai_backend.ocr.manga is not importable: {exc}"
        ) from exc
    return manga


class _FakeModel:
    """Stand-in for `MangaOcrModel`; records `.to()` and rejects `.cuda()`."""

    def __init__(self) -> None:
        self.to_calls: list[Any] = []

    def to(self, device: Any) -> "_FakeModel":
        self.to_calls.append(device)
        return self

    def cuda(self, *_args: Any, **_kwargs: Any) -> "_FakeModel":
        raise AssertionError(
            "MangaOCR weights are mmap-backed (from_pretrained requests no dtype), "
            "so the host->device move must go through "
            "rocm_mmap_transfer.move_module_to, not Module.cuda()"
        )


def _fake_modules(model: _FakeModel, *, cuda: bool, mps: bool) -> dict[str, Any]:
    """Build the lazy-import substitutes `_TorchMangaOcrRuntime.__init__` needs."""
    torch_module = types.ModuleType("torch")
    torch_module.cuda = types.SimpleNamespace(is_available=lambda: cuda)
    torch_module.backends = types.SimpleNamespace(
        mps=types.SimpleNamespace(is_available=lambda: mps)
    )
    # The runtime only forwards the result to `move_module_to`, so representing a
    # device by its spec string keeps the assertions readable.
    torch_module.device = str

    manga_ocr_module = types.ModuleType("manga_ocr")
    ocr_module = types.ModuleType("manga_ocr.ocr")
    ocr_module.MangaOcrModel = types.SimpleNamespace(
        from_pretrained=lambda *_a, **_kw: model
    )
    ocr_module.post_process = lambda text: text
    manga_ocr_module.ocr = ocr_module

    transformers_module = types.ModuleType("transformers")
    transformers_module.AutoTokenizer = types.SimpleNamespace(
        from_pretrained=lambda *_a, **_kw: object()
    )
    transformers_module.ViTImageProcessor = types.SimpleNamespace(
        from_pretrained=lambda *_a, **_kw: object()
    )

    return {
        "torch": torch_module,
        "manga_ocr": manga_ocr_module,
        "manga_ocr.ocr": ocr_module,
        "transformers": transformers_module,
    }


class TorchRuntimePlacementTests(unittest.TestCase):
    """Pin where `_TorchMangaOcrRuntime` puts the model and how it gets there."""

    def _build(
        self, *, force_cpu: bool, cuda: bool, mps: bool
    ) -> tuple[Any, _FakeModel, list[tuple]]:
        service_module = _import_service_module()
        model = _FakeModel()
        moves: list[tuple] = []

        def spy_move(module: Any, device: Any, dtype: Any = None) -> Any:
            moves.append((module, device, dtype))
            return module

        with mock.patch.dict(
            sys.modules, _fake_modules(model, cuda=cuda, mps=mps)
        ), mock.patch.object(service_module, "move_module_to", spy_move):
            runtime = service_module._TorchMangaOcrRuntime(force_cpu=force_cpu)
        return runtime, model, moves

    def test_cuda_move_uses_the_staging_helper(self) -> None:
        _runtime, model, moves = self._build(force_cpu=False, cuda=True, mps=False)
        self.assertEqual(moves, [(model, "cuda", None)])
        self.assertEqual(model.to_calls, [])

    def test_force_cpu_leaves_the_model_untouched(self) -> None:
        _runtime, model, moves = self._build(force_cpu=True, cuda=True, mps=True)
        self.assertEqual(moves, [])
        self.assertEqual(model.to_calls, [])

    def test_mps_branch_bypasses_the_helper(self) -> None:
        _runtime, model, moves = self._build(force_cpu=False, cuda=False, mps=True)
        self.assertEqual(moves, [])
        self.assertEqual(model.to_calls, ["mps"])

    def test_close_releases_every_held_object(self) -> None:
        runtime, _model, _moves = self._build(force_cpu=True, cuda=False, mps=False)

        runtime.close()

        # Anything the runtime keeps a reference to pins the checkpoint in
        # memory, so `close` must drop all of it, not only the model.
        for attribute in ("_model", "_processor", "_tokenizer", "_post_process", "_torch"):
            with self.subTest(attribute=attribute):
                self.assertIsNone(getattr(runtime, attribute))

    def test_closed_runtime_refuses_to_recognize(self) -> None:
        runtime, _model, _moves = self._build(force_cpu=True, cuda=False, mps=False)
        runtime.close()

        class _FakeImage:
            def convert(self, _mode: str) -> "_FakeImage":
                return self

            size = (32, 32)

        # `close` makes the runtime unusable; the failure must surface as the
        # service's own RuntimeError, not as a raw TypeError from `None(...)`.
        with self.assertRaises(RuntimeError):
            runtime.recognize(_FakeImage())


if __name__ == "__main__":
    unittest.main()
