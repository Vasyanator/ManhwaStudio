"""
File: modules/ai_backend/ipc/handlers/test_flux3_klein.py

Purpose:
Tests of the `inpaint.flux3_klein*` handlers (`flux3_klein.py`) over the REAL
service and manager (`inpaint/flux3_klein/_service_test_fixtures.ServiceRig`:
stand-in models on the CPU): the blob split with N references and strict
lengths, the response header (`image_len`, `seed`, `applied` keys, the step
keys) and the streamed frames with their `stage`, a mid-run cancel answered as
`Interrupted` with a follow-up run that succeeds, `.status` / `.unload`,
`large_models.profiles` with `model: "flux3_klein"` (the adapter: the
pipeline handle's answer with the encoder line in `encoder`, `null` for a
served caption; `encoder_on_cpu` absent = on), the four
`.prompt_cache.*` methods (the build's `stage` frames, its cancel before the
start and mid-build, list / export / import), the `MAX_REFERENCES` mirror, the
registration of the seven methods and the absence of the retired
`.download.*` pair (protocol 24: the application's model catalog downloads the
package).

Notes:
CPU only; files go to temporary directories.
"""

from __future__ import annotations

import io
import tempfile
import threading
import unittest
from pathlib import Path
from types import SimpleNamespace
from typing import Any
from unittest.mock import patch

import numpy as np
from PIL import Image

from modules.ai_backend.inpaint.flux3_klein import prompt_cache, wire
from modules.ai_backend.inpaint.flux3_klein._service_test_fixtures import ServiceRig
from modules.ai_backend.inpaint.flux3_klein.config import WeightFormat
from modules.ai_backend.ipc.dispatcher import ProgressEmitter
from modules.ai_backend.ipc.handlers import flux3_klein as handlers
from modules.ai_backend.ipc.protocol import (
    ALL_METHODS,
    METHOD_INPAINT_FLUX3_KLEIN,
    METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_BUILD,
    METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_EXPORT,
    METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_IMPORT,
    METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_LIST,
    METHOD_INPAINT_FLUX3_KLEIN_STATUS,
    METHOD_INPAINT_FLUX3_KLEIN_UNLOAD,
)
from modules.ai_backend.ipc.registry import HandlerContext, Interrupted, get_handler

REQUEST_ID = 7
SIZE = 128


def _png(array: np.ndarray, mode: str) -> bytes:
    with io.BytesIO() as buffer:
        Image.fromarray(array, mode).save(buffer, format="PNG")
        return buffer.getvalue()


REGION = _png(np.full((SIZE, SIZE, 3), 120, dtype=np.uint8), "RGB")
_MASK = np.zeros((SIZE, SIZE), dtype=np.uint8)
_MASK[32:96, 32:96] = 255
MASK = _png(_MASK, "L")
REFERENCE = _png(np.full((64, 64, 3), 9, dtype=np.uint8), "RGB")


class _Dispatcher:
    """Fake dispatcher: records the frames a real `ProgressEmitter` writes."""

    def __init__(self) -> None:
        self.frames: list[dict[str, Any]] = []

    def _write(self, header: dict[str, Any], blob: bytes = b"") -> None:
        self.frames.append(dict(header))


class HandlerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.rig = ServiceRig(self, variants=(WeightFormat.FP8R,))
        self.dispatcher = _Dispatcher()

    def _ctx(self) -> HandlerContext:
        ctx = HandlerContext(state=SimpleNamespace(flux3_klein=self.rig.service), events=None, get_health_snapshot=lambda: {})
        ctx.progress_emitter = ProgressEmitter(self.dispatcher, REQUEST_ID)  # type: ignore[attr-defined]
        return ctx

    def _run(self, references: tuple[bytes, ...] = (), cancel: threading.Event | None = None, **params: Any) -> tuple[dict[str, Any], bytes]:
        header = {"image_len": len(REGION), "mask_len": len(MASK), "reference_lens": [len(item) for item in references], "params": {"source_root": str(self.rig.root), "prompt": "clean", "steps": 2, **params}}
        return get_handler(METHOD_INPAINT_FLUX3_KLEIN)(self._ctx(), header, REGION + MASK + b"".join(references), cancel or threading.Event())

    def test_a_run_answers_the_region_and_streams_stages(self) -> None:
        header, blob = self._run(references=(REFERENCE, REFERENCE), seed=5)
        self.assertEqual(header["image_len"], len(blob))
        with Image.open(io.BytesIO(blob)) as image:
            self.assertEqual(image.size, (SIZE, SIZE))
        self.assertEqual(header["seed"], 5)
        self.assertEqual(set(header["applied"]), {"format", "fp8_compute", "compute_path", "encoder_mode", "ae_dtype"})
        self.assertEqual(set(header) - {"applied"}, {"image_len", "seed", "steps", "width", "height", "text_tokens", "vram_step", "ram_step", "vram_step_effective", "ram_step_effective", "degraded", "degrade_reason"})
        stages = [frame["stage"] for frame in self.dispatcher.frames]
        self.assertEqual(stages[0], "prepare")
        self.assertIn("denoise", stages)
        self.assertEqual({frame["phase"] for frame in self.dispatcher.frames}, {"load", "generate"})

    def test_strict_blob_lengths(self) -> None:
        cases = (
            {"reference_lens": [len(REFERENCE)]},
            {"reference_lens": [0]},
            {"reference_lens": [True]},
            {"reference_lens": "12"},
            {"reference_lens": [1] * 10},
            {"image_len": len(REGION) + 1},
            {"mask_len": None},
        )
        for extra in cases:
            with self.subTest(extra=extra):
                header = {"image_len": len(REGION), "mask_len": len(MASK), "params": {"source_root": str(self.rig.root), "prompt": "clean"}, **extra}
                with self.assertRaises(ValueError):
                    get_handler(METHOD_INPAINT_FLUX3_KLEIN)(self._ctx(), header, REGION + MASK, threading.Event())
        self.assertEqual(self.rig.dits, [])

    def test_a_mid_run_cancel_is_interrupted_and_the_next_run_succeeds(self) -> None:
        cancel = threading.Event()
        original = self.dispatcher._write

        def write(header: dict[str, Any], blob: bytes = b"") -> None:
            original(header, blob)
            if header.get("stage") == "denoise" and header.get("step") == 1:
                cancel.set()

        self.dispatcher._write = write  # type: ignore[method-assign]
        with self.assertRaises(Interrupted):
            self._run(cancel=cancel)
        self.assertEqual(self.rig.large_holdings()[1:], (0, 0))
        self.dispatcher._write = original  # type: ignore[method-assign]
        header, _blob = self._run()
        self.assertEqual(header["width"], SIZE)

    def test_a_cancel_before_start_runs_nothing(self) -> None:
        cancel = threading.Event()
        cancel.set()
        with self.assertRaises(Interrupted):
            self._run(cancel=cancel)
        self.assertEqual(self.rig.dits, [])

    def test_large_models_profiles_serves_flux3(self) -> None:
        from modules.ai_backend.ipc.protocol import METHOD_LARGE_MODELS_PROFILES

        ctx = HandlerContext(state=SimpleNamespace(large_models=self.rig.manager, large_model_adapters={"flux3_klein": self.rig.service}), events=None, get_health_snapshot=lambda: {})
        header = {"model": "flux3_klein", "params": {"source_root": str(self.rig.root), "encoder_mode": "text", "ae_dtype": "float32"}, "shape": {"region_width": SIZE, "region_height": SIZE, "references": [{"w": SIZE, "h": SIZE}]}, "vram_step": "max", "ram_step": "keep"}
        answer, _ = get_handler(METHOD_LARGE_MODELS_PROFILES)(ctx, header, b"", threading.Event())
        # The answer is the PIPELINE handle's (B3); the encoder handle is the additive `encoder`.
        self.assertEqual((answer["model"], answer["format"]), ("flux3_klein.ae_float32", "fp8r"))
        self.assertTrue(answer["selected"]["fits"])
        self.assertNotIn("encoder", {line["component"] for line in answer["selected"]["plan_summary"]})
        [encoder_line] = answer["encoder"]["plan_summary"]
        self.assertEqual((encoder_line["component"], encoder_line["exec_device"]), ("encoder", "cpu"), "absent encoder_on_cpu = on")
        header["params"]["encoder_on_cpu"] = False
        answer, _ = get_handler(METHOD_LARGE_MODELS_PROFILES)(ctx, header, b"", threading.Event())
        self.assertEqual(answer["encoder"]["plan_summary"][0]["exec_device"], "run_device")

    def test_status_and_unload(self) -> None:
        status, _ = get_handler(METHOD_INPAINT_FLUX3_KLEIN_STATUS)(self._ctx(), {"params": {"source_root": str(self.rig.root)}}, b"", threading.Event())
        self.assertTrue(status["available"])
        self.assertEqual(set(status["formats"]), {"bf16", "fp8r", "nvfp4"})
        self.assertEqual(get_handler(METHOD_INPAINT_FLUX3_KLEIN_UNLOAD)(self._ctx(), {}, b"", threading.Event())[0], {"unloaded": False})
        self._run()
        self.assertEqual(get_handler(METHOD_INPAINT_FLUX3_KLEIN_UNLOAD)(self._ctx(), {}, b"", threading.Event())[0], {"unloaded": True})


class PromptCacheHandlerTests(unittest.TestCase):
    """The four `.prompt_cache.*` methods over the real service (an identifiable stand-in encoder, a temporary library root)."""

    def setUp(self) -> None:
        self.rig = ServiceRig(self, variants=(WeightFormat.FP8R,))
        directory = self.rig.root / "text_encoder"
        (directory / "config.json").write_text('{"model_type": "qwen3_vl"}', encoding="utf-8")
        (directory / "chat_template.json").write_text('{"chat_template": "x"}', encoding="utf-8")
        library_root = tempfile.TemporaryDirectory(prefix="flux3_handler_library_")
        self.addCleanup(library_root.cleanup)
        self.program_root = Path(library_root.name)
        patcher = patch.object(prompt_cache, "program_root", lambda: self.program_root)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.dispatcher = _Dispatcher()

    def _ctx(self) -> HandlerContext:
        ctx = HandlerContext(state=SimpleNamespace(flux3_klein=self.rig.service), events=None, get_health_snapshot=lambda: {})
        ctx.progress_emitter = ProgressEmitter(self.dispatcher, REQUEST_ID)  # type: ignore[attr-defined]
        return ctx

    def _params(self, **overrides: Any) -> dict[str, Any]:
        return {"source_root": str(self.rig.root), "prompt": "clean", "prompt_library": "flux3_klein", "prompt_cache_precision_order": ["bf16", "q8_0", "q4_k_m"], **overrides}

    def _call(self, method: str, cancel: threading.Event | None = None, **header: Any) -> dict[str, Any]:
        answer, blob = get_handler(method)(self._ctx(), {"params": self._params(), **header}, b"", cancel or threading.Event())
        self.assertEqual(blob, b"")
        return answer

    def test_build_streams_the_encoder_stages_then_a_run_skips_the_encoder(self) -> None:
        built = self._call(METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_BUILD, name="Чистка")
        self.assertEqual((built["name"], built["encoded"], built["prompt_cached"]), ("Чистка", True, True))
        self.assertEqual([frame["stage"] for frame in self.dispatcher.frames if "file_step" not in frame], ["prepare", "encoder_load", "encode", "encoder_release"])
        reads = [frame for frame in self.dispatcher.frames if "file_step" in frame]
        self.assertTrue(reads)
        self.assertEqual({(frame["stage"], frame["file_component"]) for frame in reads}, {("encoder_load", "text_encoder")})
        self.assertEqual({frame["phase"] for frame in self.dispatcher.frames}, {"load"})
        status, _ = get_handler(METHOD_INPAINT_FLUX3_KLEIN_STATUS)(self._ctx(), {"params": self._params()}, b"", threading.Event())
        self.assertTrue(status["prompt_cached"])
        self.assertIsNotNone(status["prompt_cache_entry"])
        self.dispatcher.frames.clear()
        header = {"image_len": len(REGION), "mask_len": len(MASK), "params": {**self._params(), "steps": 2}}
        get_handler(METHOD_INPAINT_FLUX3_KLEIN)(self._ctx(), header, REGION + MASK, threading.Event())
        self.assertEqual(len(self.rig.encode_calls), 1, "the run's caption was served")

    def test_a_build_cancel_is_interrupted(self) -> None:
        cancel = threading.Event()
        cancel.set()
        with self.assertRaises(Interrupted):
            self._call(METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_BUILD, cancel=cancel)
        self.assertEqual(self.rig.encoders, [])
        cancel = threading.Event()
        original = self.dispatcher._write

        def write(header: dict[str, Any], blob: bytes = b"") -> None:
            original(header, blob)
            if header.get("stage") == "prepare":
                cancel.set()

        self.dispatcher._write = write  # type: ignore[method-assign]
        with self.assertRaises(Interrupted):
            self._call(METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_BUILD, cancel=cancel)
        self.assertEqual(self.rig.encode_calls, [])
        self.assertEqual(self.rig.large_holdings()[1:], (0, 0))

    def test_list_export_import(self) -> None:
        self._call(METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_BUILD)
        listing = self._call(METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_LIST)
        self.assertEqual((listing["library"], listing["text_encoder_available"], listing["legacy"]), ("flux3_klein", True, None))
        file = listing["texts"][0]["selected"]["file"]
        out = self.program_root / "exported.msprompt"
        exported = self._call(METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_EXPORT, file=file, path=str(out))
        self.assertEqual(exported["path"], str(out))
        imported = self._call(METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_IMPORT, path=str(out))
        self.assertEqual((imported["library"], imported["library_matches"], imported["already_present"]), ("flux3_klein", True, True))
        for method, header in ((METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_EXPORT, {"file": file}), (METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_IMPORT, {})):
            with self.subTest(method=method), self.assertRaises(ValueError):
                self._call(method, **header)

    def test_profiles_drop_the_encoder_for_a_served_caption(self) -> None:
        from modules.ai_backend.ipc.protocol import METHOD_LARGE_MODELS_PROFILES

        self._call(METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_BUILD)
        ctx = HandlerContext(state=SimpleNamespace(large_models=self.rig.manager, large_model_adapters={"flux3_klein": self.rig.service}), events=None, get_health_snapshot=lambda: {})
        header = {"model": "flux3_klein", "params": self._params(), "shape": {"region_width": SIZE, "region_height": SIZE}, "vram_step": "phase", "ram_step": "run"}
        answer, _ = get_handler(METHOD_LARGE_MODELS_PROFILES)(ctx, header, b"", threading.Event())
        self.assertIsNone(answer["encoder"])
        self.assertEqual(answer["encoder_formats"], [])


class ContractTests(unittest.TestCase):
    def test_every_method_is_registered(self) -> None:
        for method in (
            METHOD_INPAINT_FLUX3_KLEIN,
            METHOD_INPAINT_FLUX3_KLEIN_STATUS,
            METHOD_INPAINT_FLUX3_KLEIN_UNLOAD,
            METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_BUILD,
            METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_LIST,
            METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_EXPORT,
            METHOD_INPAINT_FLUX3_KLEIN_PROMPT_CACHE_IMPORT,
        ):
            self.assertIsNotNone(get_handler(method), method)
            self.assertIn(method, ALL_METHODS)

    def test_the_download_methods_are_gone(self) -> None:
        # Protocol 24: the application's model catalog downloads the FLUX.3 klein
        # package; the dispatcher answers the old names as unknown methods.
        for method in ("inpaint.flux3_klein.download.check", "inpaint.flux3_klein.download.start"):
            self.assertIsNone(get_handler(method), method)
            self.assertNotIn(method, ALL_METHODS)

    def test_the_reference_cap_mirrors_the_service(self) -> None:
        self.assertEqual(handlers.MAX_REFERENCES, wire.MAX_EXTRA_REFERENCES)


if __name__ == "__main__":
    unittest.main()
