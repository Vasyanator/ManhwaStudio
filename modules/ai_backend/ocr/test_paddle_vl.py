"""
File: modules/ai_backend/ocr/test_paddle_vl.py

Purpose:
Unit tests for the PaddleOCR-VL OCR service contracts.

Main responsibilities:
- the shared line formatter (`result_format.format_recognition_lines`);
- the offline loader: only the given local directory with
  `local_files_only=True`, the vendored classes, no `trust_remote_code`
  anywhere, the directory's own chat template, the checkpoint dtype and the
  ROCm staging move; a checkpoint with missing/unexpected keys is refused;
- generation passes `use_cache=True`;
- request validation (variant label, absolute directory);
- the lease protocol: key `paddlevlocr:<model>:<device>`, `mark_loaded` before
  the constraint build and inference, a variant or device switch reports the
  old key unloaded (no phantom resident), a failed same-key reload leaves no
  phantom either;
- unloading drops the model, the processor and the tokenizer-derived caches.

Notes:
Real weights are not needed: fake `torch`/`transformers` modules and fake
vendored classes stand in, and `_load_model_and_processor` /
`_generate_text` are stubbed for the lease tests. The opt-in real-weight
parity check lives in `test_paddle_vl_real.py`.
"""

from __future__ import annotations

import io
import sys
import tempfile
import types
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

from modules.ai_backend.ocr import paddle_vl as service_module
from modules.ai_backend.ocr.paddle_vl import PaddleVlOcrService
from modules.ai_backend.ocr.result_format import format_recognition_lines
from modules.ai_backend.runtime.model_manager import LoadedModelManager


class FormatRecognitionLinesTests(unittest.TestCase):
    def test_splits_and_trims_lines(self) -> None:
        result = format_recognition_lines(
            "  first \r\n\n second  \n",
            join_newlines=True,
            reflect_strings=False,
        )
        self.assertEqual(result["lines"], ["first", "second"])
        self.assertEqual(result["text"], "first\nsecond")

    def test_join_newlines_false_uses_spaces(self) -> None:
        result = format_recognition_lines(
            "a\nb\nc",
            join_newlines=False,
            reflect_strings=False,
        )
        self.assertEqual(result["text"], "a b c")

    def test_reflect_strings_reverses_order(self) -> None:
        result = format_recognition_lines(
            "top\nmiddle\nbottom",
            join_newlines=True,
            reflect_strings=True,
        )
        self.assertEqual(result["lines"], ["bottom", "middle", "top"])
        self.assertEqual(result["text"], "bottom\nmiddle\ntop")

    def test_empty_text_yields_empty_result(self) -> None:
        result = format_recognition_lines(
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
    """A `torch` substitute exposing only what the loader and `generate` path touch."""
    module = types.ModuleType("torch")
    module.float32 = "float32"
    module.float16 = "float16"
    module.bfloat16 = "bfloat16"
    module.cuda = types.SimpleNamespace(is_bf16_supported=lambda: True)

    class _NoGrad:
        def __enter__(self) -> None:
            return None

        def __exit__(self, *_exc: Any) -> None:
            return None

    module.no_grad = _NoGrad
    return module


def _assert_offline_kwargs(test: unittest.TestCase, kwargs: dict[str, Any]) -> None:
    test.assertIs(kwargs.get("local_files_only"), True)
    test.assertNotIn("trust_remote_code", kwargs)


class _LoaderFakes:
    """Fake transformers + vendored classes recording every `from_pretrained` call."""

    def __init__(self, loading_info: dict[str, list] | None = None) -> None:
        self.model = _FakeModel()
        self.calls: dict[str, tuple[Any, dict[str, Any]]] = {}
        self.processor_kwargs: dict[str, Any] = {}
        self.loading_info = loading_info or {
            "missing_keys": [],
            "unexpected_keys": [],
            "mismatched_keys": [],
            "error_msgs": [],
        }
        fakes = self

        class _ImageProcessor:
            @staticmethod
            def from_pretrained(source: Any, **kwargs: Any) -> object:
                fakes.calls["image_processor"] = (source, kwargs)
                return "image-processor"

        class _Processor:
            def __init__(self, **kwargs: Any) -> None:
                fakes.processor_kwargs = kwargs

        class _Model:
            @staticmethod
            def from_pretrained(source: Any, **kwargs: Any) -> tuple[_FakeModel, dict[str, list]]:
                fakes.calls["model"] = (source, kwargs)
                return fakes.model, fakes.loading_info

        def load_tokenizer(source: Any, **kwargs: Any) -> object:
            fakes.calls["tokenizer"] = (source, kwargs)
            return "tokenizer"

        self.transformers = types.ModuleType("transformers")
        self.transformers.AutoTokenizer = types.SimpleNamespace(from_pretrained=load_tokenizer)
        self.classes = service_module._VendoredClasses(
            model=_Model, processor=_Processor, image_processor=_ImageProcessor
        )


class LoaderTests(unittest.TestCase):
    """Pin `_load_model_and_processor`: offline, vendored, staged, guarded."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="ms_paddle_vl_loader_")
        self.addCleanup(self._tmp.cleanup)
        self.model_dir = Path(self._tmp.name).resolve()
        (self.model_dir / "chat_template.jinja").write_text("TEMPLATE {{ x }}", encoding="utf-8")

    def _load(self, device: str, fakes: _LoaderFakes) -> tuple[Any, Any, list[tuple]]:
        moves: list[tuple] = []

        def spy_move(module: Any, target: Any, dtype: Any = None) -> Any:
            moves.append((module, target, dtype))
            return module

        patched_modules = {"torch": _fake_torch(), "transformers": fakes.transformers}
        with mock.patch.dict(sys.modules, patched_modules), mock.patch.object(
            service_module, "move_module_to", spy_move
        ), mock.patch.object(service_module, "_ensure_transformers_compat"), mock.patch.object(
            service_module, "_load_vendored_classes", return_value=fakes.classes
        ):
            model, processor = service_module._load_model_and_processor(self.model_dir, device)
        return model, processor, moves

    def test_reads_only_the_local_directory_without_remote_code(self) -> None:
        fakes = _LoaderFakes()
        model, _processor, _moves = self._load("cpu", fakes)
        self.assertIs(model, fakes.model)
        self.assertTrue(model.eval_called)
        for name in ("image_processor", "tokenizer", "model"):
            source, kwargs = fakes.calls[name]
            self.assertEqual(source, str(self.model_dir), name)
            _assert_offline_kwargs(self, kwargs)
        self.assertIs(fakes.calls["model"][1]["output_loading_info"], True)
        self.assertEqual(
            fakes.processor_kwargs,
            {
                "image_processor": "image-processor",
                "tokenizer": "tokenizer",
                "chat_template": "TEMPLATE {{ x }}",
            },
        )

    def test_cuda_load_requests_bf16_and_stages_the_move(self) -> None:
        fakes = _LoaderFakes()
        model, _processor, moves = self._load("cuda:0", fakes)
        # The requested dtype must match the BF16 checkpoint: a cast would hide
        # the mmap pathology, and the staging helper is what compensates for it.
        self.assertEqual(fakes.calls["model"][1]["dtype"], "bfloat16")
        self.assertEqual(moves, [(model, "cuda:0", None)])

    def test_cpu_load_also_goes_through_the_helper(self) -> None:
        fakes = _LoaderFakes()
        model, _processor, moves = self._load("cpu", fakes)
        self.assertEqual(fakes.calls["model"][1]["dtype"], "float32")
        # The helper is a strict no-op for a CPU target; the call must still be
        # the single move path so there is no second, unstaged branch.
        self.assertEqual(moves, [(model, "cpu", None)])

    def test_incomplete_checkpoint_is_refused(self) -> None:
        for key in ("missing_keys", "unexpected_keys", "mismatched_keys"):
            with self.subTest(key=key):
                info = {"missing_keys": [], "unexpected_keys": [], "mismatched_keys": []}
                info[key] = ["model.layers.0.mlp.weight"]
                fakes = _LoaderFakes(info)
                with self.assertRaisesRegex(RuntimeError, key):
                    self._load("cpu", fakes)
                self.assertFalse(fakes.model.eval_called)

    def test_vendored_classes_are_the_vendored_modules(self) -> None:
        # Only the module paths are checked: importing the real vendored code
        # needs torch, which the default suite does not require.
        source = Path(service_module.__file__).read_text(encoding="utf-8")
        self.assertNotIn("trust_remote_code=True", source)
        self.assertNotIn("AutoModelForCausalLM", source)
        self.assertIn("from .paddle_vl_vendor.modeling_paddleocr_vl import", source)


class _FakeInputs(dict):
    def to(self, _device: Any) -> "_FakeInputs":
        return self


class GenerateTests(unittest.TestCase):
    def _run(self, constraint: Any = None) -> dict[str, Any]:
        seen: dict[str, Any] = {}

        class _Tensor(list):
            @property
            def shape(self) -> tuple[int, int]:
                return (1, len(self[0]))

        class _Processor:
            def apply_chat_template(self, *_args: Any, **_kwargs: Any) -> str:
                return "PROMPT"

            def __call__(self, **_kwargs: Any) -> _FakeInputs:
                return _FakeInputs(input_ids=_Tensor([[1, 2, 3]]))

            def batch_decode(self, _ids: Any, **_kwargs: Any) -> list[str]:
                return [" text "]

        processor = _Processor()

        def generate(**kwargs: Any) -> list[list[int]]:
            seen.update(kwargs)
            return [[1, 2, 3, 9]]

        model = types.SimpleNamespace(device="cpu", generate=generate)
        with mock.patch.dict(sys.modules, {"torch": _fake_torch()}):
            text = PaddleVlOcrService._generate_text(model, processor, object(), constraint)
        self.assertEqual(text, "text")
        return seen

    def test_generation_uses_the_kv_cache(self) -> None:
        kwargs = self._run()
        self.assertIs(kwargs["use_cache"], True)
        self.assertEqual(kwargs["max_new_tokens"], service_module.PADDLE_VL_MAX_NEW_TOKENS)

    def test_constrained_generation_keeps_the_cache_and_caps_length(self) -> None:
        constraint = types.SimpleNamespace(prefix_fn=lambda prompt_len: ("fn", prompt_len))
        kwargs = self._run(constraint)
        self.assertIs(kwargs["use_cache"], True)
        self.assertEqual(kwargs["prefix_allowed_tokens_fn"], ("fn", 3))
        self.assertEqual(kwargs["max_new_tokens"], service_module.PADDLE_VL_CONSTRAINED_MAX_NEW_TOKENS)


def _png_bytes() -> bytes:
    from PIL import Image

    buffer = io.BytesIO()
    Image.new("RGB", (40, 20), (255, 255, 255)).save(buffer, format="PNG")
    return buffer.getvalue()


class _FakeProcessor:
    tokenizer = None


class ServiceLeaseTests(unittest.TestCase):
    """Drive `recognize_image_bytes` with a stubbed loader and generator."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="ms_paddle_vl_lease_")
        self.addCleanup(self._tmp.cleanup)
        root = Path(self._tmp.name).resolve()
        self.dir_a = root / "official_1_6"
        self.dir_b = root / "manga_ja"
        self.dir_a.mkdir()
        self.dir_b.mkdir()
        self.manager = LoadedModelManager()
        self.service = PaddleVlOcrService(self.manager)
        self.loads: list[tuple[Path, str]] = []
        self.fail_load = False
        self.on_generate: Any = None
        self.device = "cpu"

        def fake_load(model_dir: Path, device: str) -> tuple[object, _FakeProcessor]:
            if self.fail_load:
                raise RuntimeError("stub load failure")
            self.loads.append((model_dir, device))
            return object(), _FakeProcessor()

        def fake_generate(_model: Any, _processor: Any, _image: Any, _constraint: Any = None) -> str:
            if self.on_generate is not None:
                self.on_generate()
            return "line one\nline two"

        for patcher in (
            mock.patch.object(service_module, "_load_model_and_processor", fake_load),
            mock.patch.object(PaddleVlOcrService, "_generate_text", staticmethod(fake_generate)),
            mock.patch.object(
                service_module, "_resolve_selected_backend_device", lambda _fallback: self.device
            ),
            mock.patch.object(service_module, "_clear_torch_cache"),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    def _recognize(self, model: str = "official_1_6", model_dir: Path | None = None, **kwargs: Any) -> dict[str, Any]:
        return self.service.recognize_image_bytes(
            _png_bytes(), model=model, model_dir=model_dir or self.dir_a, **kwargs
        )

    def _resident_keys(self) -> set[str]:
        return {key for key, entry in self.manager._entries.items() if entry.resident}

    def test_key_format(self) -> None:
        self.assertEqual(PaddleVlOcrService._model_key("manga_ja", "cuda:0"), "paddlevlocr:manga_ja:cuda:0")

    def test_success_reports_model_in_health(self) -> None:
        result = self._recognize()
        self.assertEqual(result, {"lines": ["line one", "line two"], "text": "line one\nline two"})
        self.assertEqual(
            self.service.health(),
            {"ready": True, "device": "cpu", "model": "official_1_6", "last_error": None},
        )
        self.assertEqual(self._resident_keys(), {"paddlevlocr:official_1_6:cpu"})

    def test_invalid_label_and_directory_are_refused_before_any_lease(self) -> None:
        for label in ("Official", "a-b", "", "x" * 65):
            with self.subTest(label=label):
                with self.assertRaisesRegex(RuntimeError, "label"):
                    self._recognize(model=label)
        with self.assertRaisesRegex(RuntimeError, "absolute"):
            self._recognize(model_dir=Path("relative/dir"))
        with self.assertRaisesRegex(RuntimeError, "does not exist"):
            self._recognize(model_dir=self.dir_a / "absent")
        self.assertEqual(self.manager._entries, {})
        self.assertEqual(self.loads, [])

    def test_mark_loaded_precedes_a_failing_inference(self) -> None:
        seen: list[dict[str, int]] = []

        def fail() -> None:
            seen.append(self.manager.health())
            raise RuntimeError("stub generate failure")

        self.on_generate = fail
        with self.assertRaisesRegex(RuntimeError, "stub generate failure"):
            self._recognize()
        self.assertEqual(seen[0]["resident_model_count"], 1)
        self.assertEqual(seen[0]["loading_model_count"], 0)
        self.assertEqual(self._resident_keys(), {"paddlevlocr:official_1_6:cpu"})
        self.assertIn("stub generate failure", self.service.health()["last_error"])

    def test_mark_loaded_precedes_a_failing_constraint_build(self) -> None:
        with mock.patch.object(
            PaddleVlOcrService, "_constraint_for_script_locked", side_effect=RuntimeError("index failure")
        ):
            with self.assertRaisesRegex(RuntimeError, "index failure"):
                self._recognize(script="korean")
        self.assertEqual(self._resident_keys(), {"paddlevlocr:official_1_6:cpu"})
        self.assertTrue(self.service._unload_model_key("paddlevlocr:official_1_6:cpu"))
        self.assertEqual(self.manager._entries, {})

    def test_load_failure_leaves_no_entry(self) -> None:
        self.fail_load = True
        with self.assertRaisesRegex(RuntimeError, "init failed"):
            self._recognize()
        self.assertEqual(self.manager._entries, {})
        self.assertFalse(self.service.health()["ready"])

    def test_variant_switch_unloads_the_old_key(self) -> None:
        self._recognize()
        with mock.patch.object(self.manager, "mark_unloaded", wraps=self.manager.mark_unloaded) as unloaded:
            self._recognize(model="manga_ja", model_dir=self.dir_b)
        unloaded.assert_called_once_with("paddlevlocr:official_1_6:cpu")
        self.assertEqual(self._resident_keys(), {"paddlevlocr:manga_ja:cpu"})
        self.assertEqual(self.loads, [(self.dir_a, "cpu"), (self.dir_b, "cpu")])
        self.assertEqual(self.service.health()["model"], "manga_ja")

    def test_device_switch_leaves_no_phantom_resident(self) -> None:
        self._recognize()
        self.device = "cuda:0"
        self._recognize()
        self.assertEqual(self._resident_keys(), {"paddlevlocr:official_1_6:cuda:0"})
        self.assertEqual(self.manager.health()["resident_model_count"], 1)

    def test_same_identity_is_reused(self) -> None:
        self._recognize()
        self._recognize()
        self.assertEqual(len(self.loads), 1)

    def test_failed_same_key_reload_leaves_no_phantom(self) -> None:
        self._recognize()
        other_dir = Path(self._tmp.name).resolve() / "moved"
        other_dir.mkdir()
        self.fail_load = True
        with self.assertRaises(RuntimeError):
            self._recognize(model_dir=other_dir)
        self.assertEqual(self.manager._entries, {})
        self.assertFalse(self.service.health()["ready"])


class UnloadTests(unittest.TestCase):
    """Pin what `_unload_model_key` releases and when it refuses."""

    def _loaded_service(self) -> PaddleVlOcrService:
        service = PaddleVlOcrService(LoadedModelManager())
        service._model = object()
        service._processor = object()
        service._identity = service_module._ModelIdentity(
            model="official_1_6", model_dir=Path("/models/official_1_6"), device="cuda:0"
        )
        return service

    def _unload(self, service: PaddleVlOcrService, model_key: str) -> tuple[bool, Any]:
        # A fake `torch` without `cuda`/`mps` keeps `_clear_torch_cache` from
        # initializing a real accelerator context during the unload.
        with mock.patch.dict(sys.modules, {"torch": _fake_torch()}), mock.patch.object(
            service._model_manager, "mark_unloaded"
        ) as unloaded:
            return service._unload_model_key(model_key), unloaded

    def test_unload_releases_model_processor_and_caches(self) -> None:
        service = self._loaded_service()
        # A constraint cache is what a script-restricted request leaves behind;
        # it belongs to the dropped processor's tokenizer.
        service._token_index = object()
        service._constraints = {"korean": object()}
        model_key = PaddleVlOcrService._model_key("official_1_6", "cuda:0")

        unloaded_ok, unloaded = self._unload(service, model_key)

        self.assertTrue(unloaded_ok)
        unloaded.assert_called_once_with(model_key)
        self.assertIsNone(service._model)
        self.assertIsNone(service._processor)
        self.assertIsNone(service._identity)
        self.assertIsNone(service._token_index)
        self.assertEqual(service._constraints, {})

    def test_unload_of_a_foreign_key_is_refused(self) -> None:
        for foreign in (
            PaddleVlOcrService._model_key("official_1_6", "cpu"),
            PaddleVlOcrService._model_key("manga_ja", "cuda:0"),
        ):
            with self.subTest(key=foreign):
                service = self._loaded_service()
                model = service._model
                unloaded_ok, unloaded = self._unload(service, foreign)
                self.assertFalse(unloaded_ok)
                unloaded.assert_not_called()
                self.assertIs(service._model, model)

    def test_unload_without_a_loaded_model_is_refused(self) -> None:
        service = PaddleVlOcrService(LoadedModelManager())

        unloaded_ok, unloaded = self._unload(service, PaddleVlOcrService._model_key("official_1_6", "cuda:0"))

        self.assertFalse(unloaded_ok)
        unloaded.assert_not_called()


if __name__ == "__main__":
    unittest.main()
