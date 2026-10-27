"""
File: modules/ai_backend/inpaint/flux2_klein/test_runtime_extraction.py

Purpose:
Characterization tests for the FLUX.2 klein names whose implementation lives in
`../../runtime/` (`safetensors_stream`, `memory_probe`, `encoder_truncation`,
`qwen3_gguf`).
They were written against the code BEFORE it moved and pin what FLUX.2 must keep
seeing afterwards: the same refusals, the same log wording on the same loggers,
the same constants, the same names on the same modules, and the same patch
points.

Main responsibilities:
- `hardware.memory_snapshot` reports zeros with the "FLUX.2 klein: ... memory
  unavailable" DEBUG records on the `hardware` logger, and still honours
  `hardware.is_torch_available` as its patch point;
- the constants (`ENCODER_KEEP_LAYERS`, the header ceiling) keep their values;
- every moved name stays reachable on its old module AND on the package.
- the GGUF text-encoder refusals (foreign architecture, foreign tensor,
  missing kept weight) keep their exact FLUX.2 wording, and their error
  records stay in the package's logger tree (`runtime.qwen3_gguf` takes the
  owner name and logger from `large_model`); the four bridge names stay on the
  package. Needs gguf, torch, transformers and accelerate (skips without).

The streaming single-file loader these tests also pinned is gone (WP6: the
large-model manager's loader reads every component tensor by tensor); its
reader and dtype table are `runtime.safetensors_stream`'s own tests.

Notes:
- No torch, no GPU: torch and psutil are made unimportable through a scoped
  `builtins.__import__` patch where the test needs their absence.
"""

from __future__ import annotations

import builtins
import logging
import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import components, hardware, large_model

from ._test_fixtures import load_9b_header_fixture, write_header_only_gguf


def _import_blocking(*blocked: str) -> Any:
    """A `builtins.__import__` replacement that refuses the named top-level modules."""
    real_import = builtins.__import__

    def _guarded(name: str, *args: Any, **kwargs: Any) -> Any:
        if name.split(".", 1)[0] in blocked:
            raise ImportError(f"No module named {name!r} (blocked by test)")
        return real_import(name, *args, **kwargs)

    return _guarded


class MemorySnapshotCharacterizationTests(unittest.TestCase):
    def test_no_psutil_and_no_torch_reports_zeros_and_logs_on_the_hardware_logger(self) -> None:
        with patch.object(hardware, "is_torch_available", lambda: False), patch.object(
            builtins, "__import__", _import_blocking("psutil")
        ), self.assertLogs(svc.log, level=logging.DEBUG) as captured:
            snapshot = hardware.memory_snapshot()
        self.assertEqual(
            snapshot, {"vram_total": 0, "vram_free": 0, "ram_total": 0, "ram_free": 0}
        )
        self.assertEqual(list(snapshot), ["vram_total", "vram_free", "ram_total", "ram_free"])
        self.assertEqual(len(captured.records), 1)
        record = captured.records[0]
        self.assertEqual(record.levelno, logging.DEBUG)
        self.assertEqual(record.name, hardware.__name__)
        self.assertTrue(
            record.getMessage().startswith("FLUX.2 klein: host memory unavailable ("),
            record.getMessage(),
        )

    def test_hardware_is_torch_available_is_still_the_patch_point(self) -> None:
        # Torch "available" by the hardware binding, but unimportable: the probe
        # must try it, fail, and log the device half on the hardware logger.
        with patch.object(hardware, "is_torch_available", lambda: True), patch.object(
            builtins, "__import__", _import_blocking("psutil", "torch")
        ), self.assertLogs(svc.log, level=logging.DEBUG) as captured:
            snapshot = hardware.memory_snapshot("cuda:1")
        self.assertEqual(snapshot["vram_total"], 0)
        self.assertEqual(snapshot["vram_free"], 0)
        messages = [record.getMessage() for record in captured.records]
        self.assertEqual(len(messages), 2, messages)
        self.assertTrue(messages[1].startswith("FLUX.2 klein: device memory unavailable ("))
        self.assertTrue(all(record.name == hardware.__name__ for record in captured.records))

    def test_cuda_device_index_is_unchanged(self) -> None:
        self.assertEqual(hardware._cuda_device_index(" CUDA:2 "), 2)
        self.assertIsNone(hardware._cuda_device_index("cuda:"))


class ConstantsCharacterizationTests(unittest.TestCase):
    def test_encoder_keep_layers(self) -> None:
        self.assertEqual(components.ENCODER_KEEP_LAYERS, 28)
        self.assertEqual(svc.ENCODER_KEEP_LAYERS, 28)

    def test_header_ceiling(self) -> None:
        self.assertEqual(components._MAX_SAFETENSORS_HEADER_BYTES, 100 * 1024 * 1024)


class SurfaceCharacterizationTests(unittest.TestCase):
    #: module -> names that must stay reachable on it AND, identically, on the package.
    _SURFACE = {
        components: (
            "read_safetensors_header",
            "read_safetensors_header_and_data_start",
            "_MAX_SAFETENSORS_HEADER_BYTES",
            "ENCODER_KEEP_LAYERS",
        ),
        hardware: ("memory_snapshot", "_cuda_device_index"),
    }

    def test_every_moved_name_is_reachable_on_its_module_and_the_package(self) -> None:
        for module, names in self._SURFACE.items():
            for name in names:
                with self.subTest(module=module.__name__, name=name):
                    self.assertTrue(hasattr(module, name))
                    self.assertIs(getattr(svc, name), getattr(module, name))

    def test_the_qwen3_gguf_bridge_is_reachable_on_the_package(self) -> None:
        from modules.ai_backend.runtime import qwen3_gguf

        for name in ("encoder_state_key_of_gguf", "gguf_encoder_quant_types", "is_gguf_encoder_name", "require_known_encoder_names"):
            with self.subTest(name=name):
                self.assertIs(getattr(svc, name), getattr(qwen3_gguf, name))

    def test_memory_snapshot_is_defined_by_hardware(self) -> None:
        # A wrapper def owned by `hardware`, not a re-bound import: that is what
        # keeps `hardware.memory_snapshot` the one patch point.
        self.assertEqual(hardware.memory_snapshot.__module__, hardware.__name__)



def _has_gguf_stack() -> bool:
    try:
        import accelerate  # noqa: F401 - availability probe
        import gguf  # noqa: F401 - availability probe
        import torch  # noqa: F401 - availability probe
        import transformers  # noqa: F401 - availability probe
    except ImportError:
        return False
    return True


@unittest.skipUnless(_has_gguf_stack(), "needs gguf, torch, transformers and accelerate")
class Qwen3GgufRefusalCharacterizationTests(unittest.TestCase):
    """The GGUF text-encoder refusals FLUX.2 shows, pinned word for word through `large_model` (the bridge lives in `runtime.qwen3_gguf`)."""

    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.folder = Path(directory.name)
        self.header = load_9b_header_fixture("unc_q8.head")

    def _write(self, name: str, *, metadata: dict[str, object] | None = None, tensors: list[list[object]] | None = None) -> Path:
        merged = {**self.header["metadata"], **(metadata or {})}  # type: ignore[dict-item]
        return write_header_only_gguf(self.folder / name, merged, tensors if tensors is not None else self.header["tensors"])  # type: ignore[arg-type]

    def test_a_foreign_architecture_is_refused_with_the_flux2_wording(self) -> None:
        path = self._write("llama.gguf", metadata={"general.architecture": "llama"})
        with self.assertRaises(ValueError) as caught:
            large_model._require_loadable_gguf_encoder(path, fp8=False)
        self.assertEqual(
            str(caught.exception),
            "Текстовый энкодер llama.gguf: это GGUF модели «llama», а нужен Qwen3 "
            "(general.architecture = «qwen3») — возьмите GGUF текстового энкодера FLUX.2 klein.",
        )

    def test_a_foreign_tensor_is_refused_and_logged_in_the_flux2_tree(self) -> None:
        path = self._write("extra.gguf", tensors=[*self.header["tensors"], ["rope_freqs.weight", [64], 0]])  # type: ignore[list-item]
        with self.assertLogs(svc.log, level=logging.ERROR) as captured, self.assertRaises(ValueError) as caught:
            large_model._require_loadable_gguf_encoder(path, fp8=False)
        self.assertEqual(
            str(caught.exception),
            "Текстовый энкодер extra.gguf: тензоры rope_freqs.weight не относятся к модели Qwen3 — "
            "это не GGUF текстового энкодера FLUX.2 klein (нужен Qwen3 в формате llama.cpp).",
        )
        self.assertEqual([record.getMessage() for record in captured.records], [f"FLUX.2 klein: GGUF текстового энкодера {path} содержит тензоры вне Qwen3: ['rope_freqs.weight']"])

    def test_a_missing_kept_weight_is_refused_and_logged_in_the_flux2_tree(self) -> None:
        tensors = [tensor for tensor in self.header["tensors"] if tensor[0] != "blk.5.ffn_up.weight"]  # type: ignore[union-attr, index]
        path = self._write("short.gguf", tensors=tensors)
        with self.assertLogs(svc.log, level=logging.ERROR) as captured, self.assertRaises(ValueError) as caught:
            large_model._require_loadable_gguf_encoder(path, fp8=False)
        self.assertEqual(
            str(caught.exception),
            "Текстовый энкодер short.gguf: в файле нет весов layers.5.mlp.up_proj.weight — файл неполный "
            "или это не Qwen3. Скачайте файл энкодера заново.",
        )
        self.assertEqual([record.getMessage() for record in captured.records], [f"FLUX.2 klein: в GGUF текстового энкодера {path} нет весов ['layers.5.mlp.up_proj.weight']"])


if __name__ == "__main__":
    unittest.main()
