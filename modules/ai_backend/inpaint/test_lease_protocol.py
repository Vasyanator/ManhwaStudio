"""
File: modules/ai_backend/inpaint/test_lease_protocol.py

Purpose:
Pins the `SmallModelManager` lease protocol of the five small-pool inpaint
services (`lama_v2`, `lama_mpe`, `aot`, `sdxl`, `flux_fill`; FLUX.2 klein and
FLUX.3 klein are large-model manager handles and take no small-pool lease)
against two specific ways of getting it wrong: reporting a failed
INFERENCE as a failed LOAD, and not registering a model the service reloaded
under a lease that had found the key resident (STATE S-058).

Main responsibilities:
- verify a model whose load succeeded is registered with `mark_loaded()` even
  when the inference that follows raises;
- verify the manager still counts that model as resident afterwards, so it stays
  visible to the residency cap instead of occupying VRAM off the books;
- verify it is still evictable afterwards, i.e. its entry kept the service's
  unload callback and another model can reclaim the slot;
- verify the lease is released in every case, and that a genuine LOAD failure
  still reports `mark_load_failed()` and leaves nothing resident;
- verify a model dropped between the lease and the service lock (another
  request unloaded or switched it) and reloaded under that now-stale lease is
  registered resident and evictable — `mark_loaded()` is called
  unconditionally after the load (the lease protocol in
  `runtime/model_manager.py`);
- verify a LaMa model switch whose old inpainter's `unload` raises logs the
  failure, still reports the old key unloaded and loads the new model (S-060).

Notes:
- No torch, no diffusers, no weights and no GPU: each service's load step is
  replaced by a stub that installs a stand-in model under the service's own
  model key, and each service's inference step by one that raises. Everything in
  between — the lease calls, their ordering, the `finally` — is the real code.
- The five services deliberately share one table of cases: the protocol is a
  cross-service contract, and a fix applied to only some of them is exactly the
  inconsistency these tests exist to catch.
"""

from __future__ import annotations

import io
import unittest
from contextlib import ExitStack
from typing import Any, Callable
from unittest.mock import patch

from PIL import Image

from modules.ai_backend.inpaint import aot, flux_fill, lama, lama_mpe, sdxl
from modules.ai_backend.runtime.model_manager import SmallModelManager

#: Message raised by every stand-in inference below.
_BOOM = "inference exploded"

_SDXL_MODEL_PATH = "/models/sdxl-inpaint.safetensors"

#: One case: `(service, invoke)`, where `invoke()` runs a single request whose
#: LOAD succeeds and whose INFERENCE raises.
CaseBuilder = Callable[[ExitStack, SmallModelManager], "tuple[Any, Callable[[], None]]"]


def _png_bytes(mode: str, color: object, size: tuple[int, int] = (16, 16)) -> bytes:
    """A tiny solid PNG, decodable by every service's own decoder."""
    with io.BytesIO() as buffer:
        Image.new(mode, size, color).save(buffer, format="PNG")
        return buffer.getvalue()


IMAGE_PNG = _png_bytes("RGB", (128, 128, 128))
MASK_PNG = _png_bytes("L", 255)



class _StubModel:
    """Stand-in for a loaded runtime object.

    It answers `set_refine`/`__call__` for LaMa V2, which drives its inpainter
    directly instead of through an `_inpaint_locked` helper, and deliberately
    carries no `unload`/`to` attribute so every service's `unload()` takes its
    plain path.
    """

    def set_refine(self, *_args: object, **_kwargs: object) -> None:
        return None

    def __call__(self, *_args: object, **_kwargs: object) -> None:
        raise RuntimeError(_BOOM)


def _explode(*_args: object, **_kwargs: object) -> None:
    """Stand-in inference step: always fails, after the load already succeeded."""
    raise RuntimeError(_BOOM)


def _fail_to_load(*_args: object, **_kwargs: object) -> None:
    """Stand-in load step for the failed-load half of the contract."""
    raise FileNotFoundError("checkpoint missing")


def _case_lama(stack: ExitStack, manager: SmallModelManager) -> tuple[Any, Callable[[], None]]:
    service = lama.LamaInpaintService(manager)
    key = service._model_key_for("cpu", "model.ckpt")

    def ensure(device: str, checkpoint_name: str) -> _StubModel:
        service._inpainter = _StubModel()
        service._active_device = device
        service._active_checkpoint_name = checkpoint_name
        service._active_model_key = key
        return service._inpainter

    stack.enter_context(patch.object(lama, "_resolve_selected_backend_device", lambda _f: "cpu"))
    stack.enter_context(patch.object(service, "_resolve_checkpoint_name", lambda _n: "model.ckpt"))
    stack.enter_context(patch.object(service, "_ensure_inpainter_locked", ensure))
    return service, lambda: service.inpaint_image_bytes(IMAGE_PNG, MASK_PNG)


def _case_lama_mpe(stack: ExitStack, manager: SmallModelManager) -> tuple[Any, Callable[[], None]]:
    service = lama_mpe.LamaMpeInpaintService(manager)
    key = service._model_key_for("cpu")

    def ensure(device: str) -> _StubModel:
        service._model = _StubModel()
        service._active_device = device
        service._active_model_key = key
        return service._model

    stack.enter_context(
        patch.object(lama_mpe, "_resolve_selected_backend_device", lambda _f: "cpu")
    )
    stack.enter_context(patch.object(lama_mpe, "_clear_torch_cache", lambda: None))
    stack.enter_context(patch.object(service, "_ensure_model_locked", ensure))
    stack.enter_context(patch.object(service, "_inpaint_locked", _explode))
    return service, lambda: service.inpaint_image_bytes(IMAGE_PNG, MASK_PNG)


def _case_aot(stack: ExitStack, manager: SmallModelManager) -> tuple[Any, Callable[[], None]]:
    service = aot.AotInpaintService(manager)
    key = service._model_key_for("cpu")

    def ensure(device: str) -> _StubModel:
        service._model = _StubModel()
        service._active_device = device
        service._active_model_key = key
        return service._model

    stack.enter_context(patch.object(aot, "_resolve_selected_backend_device", lambda _f: "cpu"))
    stack.enter_context(patch.object(aot, "_clear_torch_cache", lambda: None))
    stack.enter_context(patch.object(service, "_ensure_model_locked", ensure))
    stack.enter_context(patch.object(service, "_inpaint_locked", _explode))
    return service, lambda: service.inpaint_image_bytes(IMAGE_PNG, MASK_PNG)


def _case_sdxl(stack: ExitStack, manager: SmallModelManager) -> tuple[Any, Callable[[], None]]:
    # `server.py` shares ONE `LamaInpaintService` between SDXL and LaMa V2; the
    # prefill that uses it lives inside `_inpaint_locked`, which never runs here.
    service = sdxl.SdxlInpaintService(manager, lama.LamaInpaintService(manager))

    def ensure(*, source: Any, mode: str, device: str, model_key: str) -> _StubModel:
        service._pipe = _StubModel()
        service._active_device = device
        service._active_model_key = model_key
        return service._pipe

    stack.enter_context(patch.object(sdxl, "_resolve_selected_backend_device", lambda _f: "cpu"))
    stack.enter_context(patch.object(sdxl, "_clear_torch_cache", lambda: None))
    # The fictional checkpoint does not exist; resolution is `test_sdxl.py`'s subject.
    source = sdxl.SdxlModelSource(kind="single_file", path=_SDXL_MODEL_PATH, config_dir="/models/sdxl-config")
    stack.enter_context(patch.object(sdxl, "resolve_model_source", lambda *_a, **_k: source))
    stack.enter_context(patch.object(service, "_ensure_pipeline_locked", ensure))
    stack.enter_context(patch.object(service, "_inpaint_locked", _explode))
    params = {"model_path": _SDXL_MODEL_PATH, "mode": "nine_channel"}
    return service, lambda: service.inpaint_image_bytes(
        IMAGE_PNG, MASK_PNG, config_dir="/models/sdxl-config", params=params
    )


def _case_flux_fill(
    stack: ExitStack, manager: SmallModelManager
) -> tuple[Any, Callable[[], None]]:
    service = flux_fill.FluxFillInpaintService(manager)

    def ensure(_normalized: dict[str, Any], model_key: str) -> _StubModel:
        service._pipe = _StubModel()
        service._active_key = model_key
        return service._pipe

    stack.enter_context(patch.object(flux_fill, "_clear_torch_cache", lambda: None))
    stack.enter_context(patch.object(service, "ensure_model", lambda _quant, _cb, _cancel=None: None))
    stack.enter_context(patch.object(service, "_ensure_pipeline_locked", ensure))
    stack.enter_context(patch.object(service, "_generate_locked", _explode))
    return service, lambda: service.inpaint_image_bytes(
        IMAGE_PNG, MASK_PNG, params={"mode": "inpaint"}
    )


#: `(name, builder, load method)` per leased inpaint path.
_CASES: tuple[tuple[str, CaseBuilder, str], ...] = (
    ("lama_v2", _case_lama, "_ensure_inpainter_locked"),
    ("lama_mpe", _case_lama_mpe, "_ensure_model_locked"),
    ("aot", _case_aot, "_ensure_model_locked"),
    ("sdxl", _case_sdxl, "_ensure_pipeline_locked"),
    ("flux_fill", _case_flux_fill, "_ensure_pipeline_locked"),
)


class InferenceFailureLeaseTests(unittest.TestCase):
    """An inference failure must not be reported as a load failure.

    `mark_load_failed()` reaches `SmallModelManager.abort_load`, which clears
    the entry's `resident` flag and drops its unload callback. Calling it after
    `_ensure_*_locked` already succeeded therefore makes the manager under-count
    residency while the weights still sit in VRAM, and makes that model
    permanently non-evictable — the exact shape pinned down here.
    """

    def test_the_model_stays_resident_after_a_failed_inference(self) -> None:
        for name, builder, _load_method in _CASES:
            with self.subTest(service=name):
                manager = SmallModelManager(max_loaded_models=4)
                with ExitStack() as stack:
                    service, invoke = builder(stack, manager)
                    with self.assertRaises(RuntimeError) as caught:
                        invoke()

                    self.assertIn(_BOOM, str(caught.exception))
                    self.assertEqual(service.health()["last_error"], _BOOM)
                    health = manager.health()
                    self.assertEqual(health["resident_model_count"], 1)
                    # The lease was released in the `finally` branch.
                    self.assertEqual(health["active_model_count"], 0)
                    self.assertEqual(health["loading_model_count"], 0)

    def test_the_model_is_still_evictable_after_a_failed_inference(self) -> None:
        for name, builder, _load_method in _CASES:
            with self.subTest(service=name):
                manager = SmallModelManager(max_loaded_models=1)
                with ExitStack() as stack:
                    service, invoke = builder(stack, manager)
                    with self.assertRaises(RuntimeError):
                        invoke()

                    # Another domain now needs the single resident slot, so the
                    # entry must still carry the service's unload callback.
                    lease = manager.begin_model_use("other_domain:model")
                    self.addCleanup(lease.release)

                    self.assertTrue(lease.needs_load)
                    self.assertFalse(
                        service.unload(),
                        "the eviction callback should already have dropped the model",
                    )
                    self.assertEqual(manager.health()["resident_model_count"], 0)

    def test_a_failed_load_still_reports_a_failed_load(self) -> None:
        """The other half of the contract, so the fix cannot be a blanket removal."""
        for name, builder, load_method in _CASES:
            with self.subTest(service=name):
                manager = SmallModelManager(max_loaded_models=4)
                with ExitStack() as stack:
                    service, invoke = builder(stack, manager)
                    stack.enter_context(patch.object(service, load_method, _fail_to_load))
                    with self.assertRaises(FileNotFoundError):
                        invoke()

                    health = manager.health()
                    self.assertEqual(health["resident_model_count"], 0)
                    self.assertEqual(health["active_model_count"], 0)
                    self.assertEqual(health["loading_model_count"], 0)


class StaleLeaseReloadTests(unittest.TestCase):
    """A reload under a lease that found the key resident must still be registered.

    The race: request 2 takes its lease while key A is resident
    (`needs_load=False`), then — before request 2 reaches the service lock —
    another request drops A (a switch to another key or an explicit unload;
    `mark_unloaded` keeps the entry only because request 2 still uses it).
    Request 2 then reloads A. Here the drop is the service's own `unload()`,
    run from inside `begin_model_use`, i.e. exactly between the lease and the
    lock, which makes the race deterministic in one thread.
    """

    def test_a_model_reloaded_under_a_stale_lease_is_registered_and_evictable(self) -> None:
        for name, builder, _load_method in _CASES:
            with self.subTest(service=name):
                manager = SmallModelManager(max_loaded_models=1)
                with ExitStack() as stack:
                    service, invoke = builder(stack, manager)
                    with self.assertRaises(RuntimeError):
                        invoke()
                    self.assertEqual(manager.health()["resident_model_count"], 1)

                    original_begin = manager.begin_model_use
                    seen_needs_load: list[bool] = []

                    def begin_then_drop(*args: Any, **kwargs: Any) -> Any:
                        lease = original_begin(*args, **kwargs)
                        if not seen_needs_load:
                            seen_needs_load.append(lease.needs_load)
                            self.assertTrue(service.unload(), "the model should still be held")
                            self.assertEqual(manager.health()["resident_model_count"], 0)
                        return lease

                    stack.enter_context(patch.object(manager, "begin_model_use", begin_then_drop))
                    with self.assertRaises(RuntimeError) as caught:
                        invoke()
                    self.assertIn(_BOOM, str(caught.exception))
                    # The lease found the key resident, so the manager expected no load.
                    self.assertEqual(seen_needs_load, [False])

                    health = manager.health()
                    self.assertEqual(health["resident_model_count"], 1)
                    self.assertEqual(health["active_model_count"], 0)
                    self.assertEqual(health["loading_model_count"], 0)

                lease = manager.begin_model_use("other_domain:model")
                self.addCleanup(lease.release)
                self.assertTrue(lease.needs_load)
                self.assertFalse(
                    service.unload(),
                    "the eviction callback should already have dropped the reloaded model",
                )


class _RaisingUnloadInpainter:
    """Stand-in inpainter whose explicit unload fails."""

    def unload(self, clear_cache: bool = False) -> None:
        raise RuntimeError("unload exploded")


class _NewInpainter:
    """Stand-in inpainter class: records its constructor arguments."""

    def __init__(self, **kwargs: Any) -> None:
        self.kwargs = kwargs


class LamaSwitchUnloadFailureTests(unittest.TestCase):
    def test_a_failed_unload_is_logged_and_the_switch_still_happens(self) -> None:
        manager = SmallModelManager(max_loaded_models=2)
        service = lama.LamaInpaintService(manager)
        old_key = service._model_key_for("cpu", "old.ckpt")
        service._inpainter = _RaisingUnloadInpainter()
        service._active_device = "cpu"
        service._active_checkpoint_name = "old.ckpt"
        service._active_model_key = old_key
        unloaded: list[str] = []
        with patch.object(manager, "mark_unloaded", unloaded.append), \
                patch.object(service, "_load_inpainter_class_locked", lambda: _NewInpainter), \
                self.assertLogs(lama.log, level="WARNING") as logs:
            with service._lock:
                inpainter = service._ensure_inpainter_locked("cpu", "new.ckpt")
        self.assertIsInstance(inpainter, _NewInpainter)
        self.assertEqual(unloaded, [old_key])
        self.assertEqual(service._active_model_key, service._model_key_for("cpu", "new.ckpt"))
        self.assertTrue(any("unloading the previous inpainter failed" in line for line in logs.output))


if __name__ == "__main__":
    unittest.main()
