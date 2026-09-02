"""
File: modules/ai_backend/inpaint/test_lease_protocol.py

Purpose:
Pins the `LoadedModelManager` lease protocol of all six inpaint services
(`lama_v2`, `lama_mpe`, `aot`, `sdxl`, `flux_fill`, `flux2_klein`) against one
specific way of getting it wrong: reporting a failed INFERENCE as a failed LOAD.

Main responsibilities:
- verify a model whose load succeeded is registered with `mark_loaded()` even
  when the inference that follows raises;
- verify the manager still counts that model as resident afterwards, so it stays
  visible to the residency cap instead of occupying VRAM off the books;
- verify it is still evictable afterwards, i.e. its entry kept the service's
  unload callback and another model can reclaim the slot;
- verify the lease is released in every case, and that a genuine LOAD failure
  still reports `mark_load_failed()` and leaves nothing resident.

Notes:
- No torch, no diffusers, no weights and no GPU: each service's load step is
  replaced by a stub that installs a stand-in model under the service's own
  model key, and each service's inference step by one that raises. Everything in
  between — the lease calls, their ordering, the `finally` — is the real code.
- The six services deliberately share one table of cases: the protocol is a
  cross-service contract, and a fix applied to only some of them is exactly the
  inconsistency these tests exist to catch.
"""

from __future__ import annotations

import io
import pathlib
import unittest
from contextlib import ExitStack
from typing import Any, Callable
from unittest.mock import patch

from PIL import Image

from modules.ai_backend.inpaint import aot, flux2_klein, flux_fill, lama, lama_mpe, sdxl
from modules.ai_backend.runtime.model_manager import LoadedModelManager

#: Message raised by every stand-in inference below.
_BOOM = "inference exploded"

_SDXL_MODEL_PATH = "/models/sdxl-inpaint.safetensors"

#: FLUX.2 klein validates its three component paths eagerly, so the case builder
#: points them all at a directory that certainly exists.
_FLUX2_KLEIN_PATH = str(pathlib.Path(__file__).resolve().parent)

#: One case: `(service, invoke)`, where `invoke()` runs a single request whose
#: LOAD succeeds and whose INFERENCE raises.
CaseBuilder = Callable[[ExitStack, LoadedModelManager], "tuple[Any, Callable[[], None]]"]


def _png_bytes(mode: str, color: object, size: tuple[int, int] = (16, 16)) -> bytes:
    """A tiny solid PNG, decodable by every service's own decoder."""
    with io.BytesIO() as buffer:
        Image.new(mode, size, color).save(buffer, format="PNG")
        return buffer.getvalue()


IMAGE_PNG = _png_bytes("RGB", (128, 128, 128))
MASK_PNG = _png_bytes("L", 255)

#: FLUX.2 klein refuses a region smaller than 128 px per side (see
#: `flux2_klein.validate_region_size`), so it gets its own pair.
REGION_PNG = _png_bytes("RGB", (128, 128, 128), size=(128, 128))
REGION_MASK_PNG = _png_bytes("L", 255, size=(128, 128))


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


def _case_lama(stack: ExitStack, manager: LoadedModelManager) -> tuple[Any, Callable[[], None]]:
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


def _case_lama_mpe(stack: ExitStack, manager: LoadedModelManager) -> tuple[Any, Callable[[], None]]:
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


def _case_aot(stack: ExitStack, manager: LoadedModelManager) -> tuple[Any, Callable[[], None]]:
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


def _case_sdxl(stack: ExitStack, manager: LoadedModelManager) -> tuple[Any, Callable[[], None]]:
    # `server.py` shares ONE `LamaInpaintService` between SDXL and LaMa V2; the
    # prefill that uses it lives inside `_inpaint_locked`, which never runs here.
    service = sdxl.SdxlInpaintService(manager, lama.LamaInpaintService(manager))

    def ensure(*, model_path: str, mode: str, device: str, model_key: str) -> _StubModel:
        service._pipe = _StubModel()
        service._active_device = device
        service._active_model_key = model_key
        return service._pipe

    stack.enter_context(patch.object(sdxl, "_resolve_selected_backend_device", lambda _f: "cpu"))
    stack.enter_context(patch.object(sdxl, "_clear_torch_cache", lambda: None))
    stack.enter_context(patch.object(service, "_ensure_pipeline_locked", ensure))
    stack.enter_context(patch.object(service, "_inpaint_locked", _explode))
    params = {"model_path": _SDXL_MODEL_PATH, "mode": "nine_channel"}
    return service, lambda: service.inpaint_image_bytes(IMAGE_PNG, MASK_PNG, params=params)


def _case_flux_fill(
    stack: ExitStack, manager: LoadedModelManager
) -> tuple[Any, Callable[[], None]]:
    service = flux_fill.FluxFillInpaintService(manager)

    def ensure(_normalized: dict[str, Any], model_key: str) -> _StubModel:
        service._pipe = _StubModel()
        service._active_key = model_key
        return service._pipe

    stack.enter_context(patch.object(flux_fill, "_clear_torch_cache", lambda: None))
    stack.enter_context(patch.object(service, "ensure_model", lambda _quant, _cb: None))
    stack.enter_context(patch.object(service, "_ensure_pipeline_locked", ensure))
    stack.enter_context(patch.object(service, "_generate_locked", _explode))
    return service, lambda: service.inpaint_image_bytes(
        IMAGE_PNG, MASK_PNG, params={"mode": "inpaint"}
    )


def _case_flux2_klein(
    stack: ExitStack, manager: LoadedModelManager
) -> tuple[Any, Callable[[], None]]:
    service = flux2_klein.Flux2KleinInpaintService(manager)

    def ensure(
        _normalized: dict[str, Any], model_key: str, _report: Any, *, region_hw: tuple[int, int]
    ) -> _StubModel:
        service._pipe = _StubModel()
        service._active_key = model_key
        return service._pipe

    stack.enter_context(patch.object(flux2_klein, "_clear_torch_cache", lambda: None))
    stack.enter_context(patch.object(service, "_ensure_pipeline_locked", ensure))
    # Phase 1 (the prompt encoder) and the pre-load memory guard are separate
    # concerns with their own tests; this case is about the lease protocol only.
    stack.enter_context(
        patch.object(
            service,
            "_prompt_embeds_locked",
            lambda _normalized, _report: {"prompt": object(), "negative": None},
        )
    )
    stack.enter_context(
        patch.object(service, "_require_headroom_locked", lambda *_a, **_k: None)
    )
    stack.enter_context(patch.object(service, "_generate_locked", _explode))
    # The region must survive `validate_region_size`, so this case uses its own
    # 128x128 image instead of the shared 16x16 one.
    params = {
        "text_encoder_path": _FLUX2_KLEIN_PATH,
        "transformer_path": _FLUX2_KLEIN_PATH,
        "vae_path": _FLUX2_KLEIN_PATH,
    }
    return service, lambda: service.inpaint_image_bytes(
        REGION_PNG, REGION_MASK_PNG, params=params
    )


#: `(name, builder, load method)` per inpaint service.
_CASES: tuple[tuple[str, CaseBuilder, str], ...] = (
    ("lama_v2", _case_lama, "_ensure_inpainter_locked"),
    ("lama_mpe", _case_lama_mpe, "_ensure_model_locked"),
    ("aot", _case_aot, "_ensure_model_locked"),
    ("sdxl", _case_sdxl, "_ensure_pipeline_locked"),
    ("flux_fill", _case_flux_fill, "_ensure_pipeline_locked"),
    ("flux2_klein", _case_flux2_klein, "_ensure_pipeline_locked"),
)


class InferenceFailureLeaseTests(unittest.TestCase):
    """An inference failure must not be reported as a load failure.

    `mark_load_failed()` reaches `LoadedModelManager.abort_load`, which clears
    the entry's `resident` flag and drops its unload callback. Calling it after
    `_ensure_*_locked` already succeeded therefore makes the manager under-count
    residency while the weights still sit in VRAM, and makes that model
    permanently non-evictable — the exact shape pinned down here.
    """

    def test_the_model_stays_resident_after_a_failed_inference(self) -> None:
        for name, builder, _load_method in _CASES:
            with self.subTest(service=name):
                manager = LoadedModelManager(max_loaded_models=4)
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
                manager = LoadedModelManager(max_loaded_models=1)
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
                manager = LoadedModelManager(max_loaded_models=4)
                with ExitStack() as stack:
                    service, invoke = builder(stack, manager)
                    stack.enter_context(patch.object(service, load_method, _fail_to_load))
                    with self.assertRaises(FileNotFoundError):
                        invoke()

                    health = manager.health()
                    self.assertEqual(health["resident_model_count"], 0)
                    self.assertEqual(health["active_model_count"], 0)
                    self.assertEqual(health["loading_model_count"], 0)


if __name__ == "__main__":
    unittest.main()
