"""
File: modules/ai_backend/inpaint/test_lease_hints.py

Purpose:
Pins that every small-pool inpaint service (`lama_v2`, `lama_mpe`, `aot`, `sdxl`,
`flux_fill`) hands the residency arbiter its owner name and a pre-load size
hint built from the very files its load reads.

Main responsibilities:
- spy on `SmallModelManager.begin_model_use` and record `owner=` / `hint=`;
- point each service's checkpoint path(s) at temp files of known size and check
  the hint carries exactly those bytes on the leased device.

Notes:
- Reuses the case builders of `test_lease_protocol.py` (stub load, raising
  inference), so no torch, weights or GPU are needed; only the paths change.
- Every device is `cpu`, so the hint is host-accounted and the arbiter probes
  only the host (a few bytes are requested).
"""

from __future__ import annotations

import tempfile
import unittest
from contextlib import ExitStack
from pathlib import Path
from typing import Any
from unittest.mock import patch

from modules.ai_backend.inpaint import aot, flux_fill, lama, lama_mpe, sdxl
from modules.ai_backend.inpaint.test_lease_protocol import (
    IMAGE_PNG,
    MASK_PNG,
    _case_aot,
    _case_flux_fill,
    _case_lama,
    _case_lama_mpe,
    _case_sdxl,
)
from modules.ai_backend.runtime.model_manager import SmallModelManager, ModelUsageLease, UnloadCallback
from modules.ai_backend.runtime.residency.holdings import FootprintHint, Runtime


class SpyManager(SmallModelManager):
    """A real manager that also records every `begin_model_use` call's `(key, owner, hint)`."""

    def __init__(self) -> None:
        super().__init__(max_loaded_models=4)
        self.calls: list[tuple[str, str | None, FootprintHint | None]] = []

    def begin_model_use(self, model_key: str, unload_callback: UnloadCallback | None = None, *, owner: str | None = None, hint: FootprintHint | None = None) -> ModelUsageLease:
        """Record the call, then take the lease as the real manager does."""
        self.calls.append((model_key, owner, hint))
        return super().begin_model_use(model_key, unload_callback, owner=owner, hint=hint)


def _write(path: Path, size: int) -> Path:
    """Create `path` (and its parents) holding `size` zero bytes."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"\0" * size)
    return path


def _cpu_hint(size: int) -> FootprintHint:
    """The hint `hint_from_files` builds for `size` torch file bytes on the CPU."""
    return FootprintHint(device="cpu", device_bytes=0, host_bytes=size, runtime=Runtime.TORCH)


class InpaintLeaseHintTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="ms-lease-hints-")
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.manager = SpyManager()

    def _run(self, invoke: Any) -> None:
        """Run one request; its stand-in inference fails by design (only the lease call matters here)."""
        with self.assertRaises(Exception):
            invoke()

    def _assert_single_call(self, owner: str, hint: FootprintHint) -> None:
        self.assertEqual(len(self.manager.calls), 1)
        _key, seen_owner, seen_hint = self.manager.calls[0]
        self.assertEqual(seen_owner, owner)
        self.assertEqual(seen_hint, hint)

    def test_lama_hint_is_the_selected_checkpoint_file(self) -> None:
        _write(self.root / "models" / "model.ckpt", 1000)
        with ExitStack() as stack:
            service, invoke = _case_lama(stack, self.manager)
            service._model_dir = self.root
            self._run(invoke)
        self._assert_single_call(lama._LEASE_OWNER, _cpu_hint(1000))

    def test_lama_mpe_hint_is_the_checkpoint_file(self) -> None:
        with ExitStack() as stack:
            service, invoke = _case_lama_mpe(stack, self.manager)
            service._model_path = _write(self.root / "mpe.ckpt", 2000)
            self._run(invoke)
        self._assert_single_call(lama_mpe._LEASE_OWNER, _cpu_hint(2000))

    def test_aot_hint_is_the_checkpoint_file(self) -> None:
        with ExitStack() as stack:
            service, invoke = _case_aot(stack, self.manager)
            service._model_path = _write(self.root / "aot.ckpt", 3000)
            self._run(invoke)
        self._assert_single_call(aot._LEASE_OWNER, _cpu_hint(3000))

    def test_sdxl_hint_is_the_resolved_source(self) -> None:
        folder = self.root / "sdxl"
        _write(folder / "unet" / "w.safetensors", 4000)
        _write(folder / "vae" / "w.safetensors", 500)
        source = sdxl.SdxlModelSource(kind="folder", path=str(folder))
        with ExitStack() as stack:
            _service, invoke = _case_sdxl(stack, self.manager)
            stack.enter_context(patch.object(sdxl, "resolve_model_source", lambda *_a, **_k: source))
            self._run(invoke)
        # Only the SDXL lease: the nested LaMa prefill runs inside the stubbed inference.
        self._assert_single_call(sdxl._LEASE_OWNER, _cpu_hint(4500))

    def test_flux_fill_hint_is_the_gguf_plus_components_on_the_load_device(self) -> None:
        gguf = _write(self.root / "flux.gguf", 6000)
        components = self.root / "components"
        _write(components / "vae" / "w.safetensors", 700)
        with ExitStack() as stack:
            _service, invoke = _case_flux_fill(stack, self.manager)
            stack.enter_context(patch.object(flux_fill, "gguf_path", lambda _quant: str(gguf)))
            stack.enter_context(patch.object(flux_fill, "_components_dir", lambda: str(components)))
            stack.enter_context(patch.object(flux_fill, "resolve_discrete_backend_device", lambda *_a, **_k: "cpu"))
            self._run(invoke)
        self._assert_single_call(flux_fill._LEASE_OWNER, _cpu_hint(6700))

    def test_flux_fill_cpu_offload_is_host_accounted_without_probing_the_gpu(self) -> None:
        gguf = _write(self.root / "flux.gguf", 6000)
        components = self.root / "components"
        components.mkdir()

        def no_probe(*_args: object, **_kwargs: object) -> str:
            raise AssertionError("CPU offload must not probe the discrete GPU for the hint")

        with ExitStack() as stack:
            service, _invoke = _case_flux_fill(stack, self.manager)
            stack.enter_context(patch.object(flux_fill, "gguf_path", lambda _quant: str(gguf)))
            stack.enter_context(patch.object(flux_fill, "_components_dir", lambda: str(components)))
            stack.enter_context(patch.object(flux_fill, "resolve_discrete_backend_device", no_probe))
            self._run(lambda: service.inpaint_image_bytes(IMAGE_PNG, MASK_PNG, params={"cpu_offload": True}))
        self._assert_single_call(flux_fill._LEASE_OWNER, _cpu_hint(6000))


if __name__ == "__main__":
    unittest.main()
