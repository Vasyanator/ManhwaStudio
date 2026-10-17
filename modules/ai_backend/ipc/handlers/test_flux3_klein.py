"""
File: modules/ai_backend/ipc/handlers/test_flux3_klein.py

Purpose:
Tests of the `inpaint.flux3_klein*` handlers (`flux3_klein.py`) over the REAL
service and manager (`inpaint/flux3_klein/_service_test_fixtures.ServiceRig`:
stand-in models on the CPU): the blob split with N references and strict
lengths, the response header (`image_len`, `seed`, `applied` keys, the step
keys) and the streamed frames with their `stage`, a mid-run cancel answered as
`Interrupted` with a follow-up run that succeeds, `.status` / `.unload`, the
download methods' format echo and cancel, `large_models.profiles` with
`model: "flux3_klein"` (the adapter; `encoder_on_cpu` absent = on), the
`MAX_REFERENCES` mirror and the
registration of all five methods.

Notes:
CPU only; files go to temporary directories.
"""

from __future__ import annotations

import io
import threading
import unittest
from types import SimpleNamespace
from typing import Any
from unittest.mock import patch

import numpy as np
from PIL import Image

from modules.ai_backend.inpaint.flux3_klein import wire
from modules.ai_backend.inpaint.flux3_klein._service_test_fixtures import ServiceRig
from modules.ai_backend.inpaint.flux3_klein.config import WeightFormat
from modules.ai_backend.ipc.dispatcher import ProgressEmitter
from modules.ai_backend.ipc.handlers import flux3_klein as handlers
from modules.ai_backend.ipc.protocol import (
    METHOD_INPAINT_FLUX3_KLEIN,
    METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_CHECK,
    METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_START,
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
        self.assertEqual((answer["model"], answer["format"]), ("flux3_klein.text.ae_float32", "fp8r"))
        self.assertTrue(answer["selected"]["fits"])
        summary = {line["component"]: line for line in answer["selected"]["plan_summary"]}
        self.assertEqual(summary["encoder"]["exec_device"], "cpu", "absent encoder_on_cpu = on")
        header["params"]["encoder_on_cpu"] = False
        answer, _ = get_handler(METHOD_LARGE_MODELS_PROFILES)(ctx, header, b"", threading.Event())
        summary = {line["component"]: line for line in answer["selected"]["plan_summary"]}
        self.assertEqual(summary["encoder"]["exec_device"], "run_device")

    def test_status_and_unload(self) -> None:
        status, _ = get_handler(METHOD_INPAINT_FLUX3_KLEIN_STATUS)(self._ctx(), {"params": {"source_root": str(self.rig.root)}}, b"", threading.Event())
        self.assertTrue(status["available"])
        self.assertEqual(set(status["formats"]), {"bf16", "fp8r", "nvfp4"})
        self.assertEqual(get_handler(METHOD_INPAINT_FLUX3_KLEIN_UNLOAD)(self._ctx(), {}, b"", threading.Event())[0], {"unloaded": False})
        self._run()
        self.assertEqual(get_handler(METHOD_INPAINT_FLUX3_KLEIN_UNLOAD)(self._ctx(), {}, b"", threading.Event())[0], {"unloaded": True})


class _FakeDownloads:
    """Stand-in for `inpaint/flux3_download.py`."""

    class DownloadCanceled(Exception):
        pass

    def __init__(self) -> None:
        self.checked: list[tuple[str, Any]] = []

    def check_access(self, token: str, *, weight_format: Any = None) -> dict[str, Any]:
        self.checked.append((token, weight_format))
        return {"format": weight_format or "fp8r", "repos": {}, "plan": None, "plan_error": ""}

    def download(self, token: str, *, weight_format: Any = None, progress_callback: Any = None, should_cancel: Any = None) -> dict[str, Any]:
        progress_callback("download", 1, 2, "ae.safetensors", file_step=1, file_total=2, file_label="ae.safetensors")
        if should_cancel():
            raise self.DownloadCanceled("canceled")
        return {"root": "/r", "downloaded_bytes": 2, "skipped_files": 0}


class DownloadHandlerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fake = _FakeDownloads()
        patcher = patch.object(handlers, "_download_module", lambda: self.fake)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.dispatcher = _Dispatcher()
        self.ctx = HandlerContext(state=SimpleNamespace(), events=None, get_health_snapshot=lambda: {})
        self.ctx.progress_emitter = ProgressEmitter(self.dispatcher, REQUEST_ID)  # type: ignore[attr-defined]

    def test_check_forwards_the_token_and_the_format(self) -> None:
        answer, _ = get_handler(METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_CHECK)(self.ctx, {"hf_token": "t", "format": "nvfp4"}, b"", threading.Event())
        self.assertEqual(answer["format"], "nvfp4")
        self.assertEqual(self.fake.checked, [("t", "nvfp4")])
        get_handler(METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_CHECK)(self.ctx, {}, b"", threading.Event())
        self.assertEqual(self.fake.checked[-1], ("", None))

    def test_start_streams_and_a_cancel_is_interrupted(self) -> None:
        answer, _ = get_handler(METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_START)(self.ctx, {"hf_token": "t"}, b"", threading.Event())
        self.assertEqual(answer["root"], "/r")
        self.assertEqual(self.dispatcher.frames[0]["file_label"], "ae.safetensors")
        cancel = threading.Event()

        def set_on_frame(header: dict[str, Any], blob: bytes = b"") -> None:
            cancel.set()

        self.ctx.progress_emitter = ProgressEmitter(SimpleNamespace(_write=set_on_frame), REQUEST_ID)  # type: ignore[attr-defined]
        with self.assertRaises(Interrupted):
            get_handler(METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_START)(self.ctx, {"hf_token": "t"}, b"", cancel)


class ContractTests(unittest.TestCase):
    def test_every_method_is_registered(self) -> None:
        for method in (METHOD_INPAINT_FLUX3_KLEIN, METHOD_INPAINT_FLUX3_KLEIN_STATUS, METHOD_INPAINT_FLUX3_KLEIN_UNLOAD, METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_CHECK, METHOD_INPAINT_FLUX3_KLEIN_DOWNLOAD_START):
            self.assertIsNotNone(get_handler(method), method)

    def test_the_reference_cap_mirrors_the_service(self) -> None:
        self.assertEqual(handlers.MAX_REFERENCES, wire.MAX_EXTRA_REFERENCES)


if __name__ == "__main__":
    unittest.main()
