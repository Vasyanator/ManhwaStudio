"""
File: modules/ai_backend/ocr/test_paddle_vl.py

Purpose:
Unit tests for PaddleOCR-VL OCR text post-processing contracts.

Main responsibilities:
- verify line splitting trims whitespace and drops empty lines;
- verify `join_newlines=False` collapses lines with spaces;
- verify `reflect_strings=True` reverses line order for right-to-left columns;
- verify the loader requests the checkpoint dtype and moves the model to the
  device through the ROCm mmap staging helper rather than bare `Module.to`;
- verify unloading drops the model, the processor and the tokenizer-derived
  caches, and reports the eviction to the shared model manager.

Notes:
Real weights need PyTorch/Transformers and a populated Hugging Face cache, so
the loader tests substitute fake `torch`/`transformers` modules and only pin the
call contract of `_ensure_loaded_locked`; the pure text formatter is covered
directly.
"""

from __future__ import annotations

import sys
import types
import unittest
from typing import Any
from unittest import mock

from modules.ai_backend.ocr import paddle_vl as service_module
from modules.ai_backend.ocr.paddle_vl import (
    PaddleVlOcrService,
    _format_recognition_lines,
)
from modules.ai_backend.runtime.model_manager import LoadedModelManager


class FormatRecognitionLinesTests(unittest.TestCase):
    def test_splits_and_trims_lines(self) -> None:
        result = _format_recognition_lines(
            "  first \r\n\n second  \n",
            join_newlines=True,
            reflect_strings=False,
        )
        self.assertEqual(result["lines"], ["first", "second"])
        self.assertEqual(result["text"], "first\nsecond")

    def test_join_newlines_false_uses_spaces(self) -> None:
        result = _format_recognition_lines(
            "a\nb\nc",
            join_newlines=False,
            reflect_strings=False,
        )
        self.assertEqual(result["text"], "a b c")

    def test_reflect_strings_reverses_order(self) -> None:
        result = _format_recognition_lines(
            "top\nmiddle\nbottom",
            join_newlines=True,
            reflect_strings=True,
        )
        self.assertEqual(result["lines"], ["bottom", "middle", "top"])
        self.assertEqual(result["text"], "bottom\nmiddle\ntop")

    def test_empty_text_yields_empty_result(self) -> None:
        result = _format_recognition_lines(
            "",
            join_newlines=True,
            reflect_strings=False,
        )
        self.assertEqual(result["lines"], [])
        self.assertEqual(result["text"], "")


class _FakeModel:
    """Stand-in for the loaded `nn.Module`; fails the test if moved via `.to()`."""

    def __init__(self) -> None:
        self.eval_called = False

    def eval(self) -> "_FakeModel":
        self.eval_called = True
        return self

    def to(self, *_args: Any, **_kwargs: Any) -> "_FakeModel":
        raise AssertionError(
            "PaddleOCR-VL weights are mmap-backed (no dtype cast on load), so the "
            "host->device move must go through "
            "runtime.rocm_mmap_transfer.move_module_to, not Module.to"
        )


def _fake_torch() -> types.ModuleType:
    """A `torch` substitute exposing only what `_resolve_dtype` touches."""
    module = types.ModuleType("torch")
    module.float32 = "float32"
    module.float16 = "float16"
    module.bfloat16 = "bfloat16"
    module.cuda = types.SimpleNamespace(is_bf16_supported=lambda: True)
    return module


def _fake_transformers(model: _FakeModel, calls: dict[str, Any]) -> types.ModuleType:
    """A `transformers` substitute recording the `from_pretrained` keywords."""

    def load_model(model_id: str, **kwargs: Any) -> _FakeModel:
        calls["model"] = (model_id, kwargs)
        return model

    def load_processor(model_id: str, **kwargs: Any) -> object:
        calls["processor"] = (model_id, kwargs)
        return object()

    module = types.ModuleType("transformers")
    module.AutoModelForCausalLM = types.SimpleNamespace(from_pretrained=load_model)
    module.AutoProcessor = types.SimpleNamespace(from_pretrained=load_processor)
    return module


def _load_service(
    device: str,
) -> tuple[PaddleVlOcrService, _FakeModel, dict[str, Any], list[tuple]]:
    """Load a `PaddleVlOcrService` for `device` against fake torch/transformers.

    Returns the service plus the fake model, the recorded `from_pretrained`
    keywords, and the `(module, target, dtype)` tuples the staging helper saw.
    """
    model = _FakeModel()
    calls: dict[str, Any] = {}
    moves: list[tuple] = []

    def spy_move(module: Any, target: Any, dtype: Any = None) -> Any:
        moves.append((module, target, dtype))
        return module

    service = PaddleVlOcrService(LoadedModelManager())
    patched_modules = {
        "torch": _fake_torch(),
        "transformers": _fake_transformers(model, calls),
    }
    with mock.patch.dict(sys.modules, patched_modules), mock.patch.object(
        service_module, "move_module_to", spy_move
    ), mock.patch.object(service_module, "_ensure_transformers_compat"):
        service._ensure_loaded_locked(device)
    return service, model, calls, moves


class EnsureLoadedTransferTests(unittest.TestCase):
    """Pin how `_ensure_loaded_locked` loads and places the model."""

    def _load(self, device: str) -> tuple[_FakeModel, dict[str, Any], list[tuple]]:
        service, model, calls, moves = _load_service(device)
        self.assertIs(service._model, model)
        self.assertTrue(model.eval_called)
        return model, calls, moves

    def test_cuda_load_requests_bf16_and_stages_the_move(self) -> None:
        model, calls, moves = self._load("cuda:0")
        # The requested dtype must match the BF16 checkpoint: a cast would hide
        # the mmap pathology, and the staging helper is what compensates for it.
        self.assertEqual(calls["model"][1]["dtype"], "bfloat16")
        self.assertEqual(moves, [(model, "cuda:0", None)])

    def test_cpu_load_also_goes_through_the_helper(self) -> None:
        model, calls, moves = self._load("cpu")
        self.assertEqual(calls["model"][1]["dtype"], "float32")
        # The helper is a strict no-op for a CPU target; the call must still be
        # the single move path so there is no second, unstaged branch.
        self.assertEqual(moves, [(model, "cpu", None)])


class UnloadTests(unittest.TestCase):
    """Pin what `_unload_model_key` releases and when it refuses."""

    def _unload(self, service: PaddleVlOcrService, model_key: str) -> tuple[bool, Any]:
        # A fake `torch` without `cuda`/`mps` keeps `_clear_torch_cache` from
        # initializing a real accelerator context during the unload.
        with mock.patch.dict(sys.modules, {"torch": _fake_torch()}), mock.patch.object(
            service._model_manager, "mark_unloaded"
        ) as unloaded:
            return service._unload_model_key(model_key), unloaded

    def test_unload_releases_model_processor_and_caches(self) -> None:
        service, _model, _calls, _moves = _load_service("cuda:0")
        # A constraint cache is what a script-restricted request leaves behind;
        # it belongs to the dropped processor's tokenizer.
        service._token_index = object()
        service._constraints = {"korean": object()}
        model_key = PaddleVlOcrService._model_key("cuda:0")

        unloaded_ok, unloaded = self._unload(service, model_key)

        self.assertTrue(unloaded_ok)
        unloaded.assert_called_once_with(model_key)
        self.assertIsNone(service._model)
        self.assertIsNone(service._processor)
        self.assertIsNone(service._device)
        self.assertIsNone(service._token_index)
        self.assertEqual(service._constraints, {})

    def test_unload_of_a_foreign_key_is_refused(self) -> None:
        service, model, _calls, _moves = _load_service("cuda:0")

        unloaded_ok, unloaded = self._unload(service, PaddleVlOcrService._model_key("cpu"))

        self.assertFalse(unloaded_ok)
        unloaded.assert_not_called()
        self.assertIs(service._model, model)

    def test_unload_without_a_loaded_model_is_refused(self) -> None:
        service = PaddleVlOcrService(LoadedModelManager())

        unloaded_ok, unloaded = self._unload(service, PaddleVlOcrService._model_key("cuda:0"))

        self.assertFalse(unloaded_ok)
        unloaded.assert_not_called()


if __name__ == "__main__":
    unittest.main()
