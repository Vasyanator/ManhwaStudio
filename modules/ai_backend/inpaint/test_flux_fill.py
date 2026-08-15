"""
File: modules/ai_backend/inpaint/test_flux_fill.py

Purpose:
Unit tests for the Flux Fill parameter normalization and for the ROCm mmap->GPU
staging seam of the pipeline placement.

Main responsibilities:
- verify mode/quant validation and numeric clamping of the generation params;
- verify the plain placement path moves the pipeline inside the staging patch;
- verify the CPU-offload path re-homes the safetensors components in anonymous
  host memory (staged move to the GPU, then back) before accelerate installs its
  lazy offload hooks, and leaves the GGUF transformer alone;
- verify the re-homing pass skips components that are absent, already on the GPU,
  or no longer file-backed (an fp16 run casts them into anonymous memory), and
  that it frees the allocator cache between components;
- verify a failed move degrades to a warning instead of a failed load;
- verify unloading and swapping quants drop the pipeline and report it;
- verify two concurrent first uses of the same quant stage the download once,
  into a process-private file, instead of writing over each other.

Notes:
- Fake `torch` and `diffusers` modules are injected into `sys.modules`, so the
  tests need neither package, nor the FLUX weights, nor a GPU.
- Whether the staging patch does anything on the current host is
  `rocm_mmap_transfer`'s own contract (covered by `test_rocm_mmap_transfer.py`).
  What is pinned here is only how the service drives it.
"""

from __future__ import annotations

import sys
import tempfile
import threading
import time
import types
import unittest
from pathlib import Path
from unittest.mock import patch

from modules.ai_backend.inpaint import flux_fill as svc
from modules.ai_backend.runtime.model_manager import LoadedModelManager


class NormalizeFluxFillParamsTests(unittest.TestCase):
    def test_unknown_mode_raises(self) -> None:
        with self.assertRaises(ValueError):
            svc.normalize_flux_fill_params({"mode": "bogus"})

    def test_object_removal_gets_default_prompt(self) -> None:
        out = svc.normalize_flux_fill_params({"mode": "object_removal"})
        self.assertEqual(out["prompt"], svc.OBJECT_REMOVAL_PROMPT)

    def test_unknown_quant_falls_back_to_default(self) -> None:
        out = svc.normalize_flux_fill_params({"mode": "inpaint", "quant": "Q9_K_XXL"})
        self.assertEqual(out["quant"], svc.DEFAULT_QUANT)

    def test_numeric_clamping(self) -> None:
        out = svc.normalize_flux_fill_params(
            {"mode": "inpaint", "steps": 9999, "guidance": -5.0, "max_seq": 4096}
        )
        self.assertEqual(out["steps"], 100)
        self.assertAlmostEqual(out["guidance"], 0.0)
        self.assertEqual(out["max_seq"], 512)


class _PatchRecorder:
    """Recording stand-in for `rocm_mmap_transfer.patched_module_to`.

    The instance is both the factory and the context manager, so `depth` can be
    sampled from inside a faked `.to()` to prove the move happened while the
    staging patch was installed.
    """

    def __init__(self) -> None:
        self.depth = 0
        self.enters = 0
        self.exits = 0

    def __call__(self) -> "_PatchRecorder":
        return self

    def __enter__(self) -> "_PatchRecorder":
        self.depth += 1
        self.enters += 1
        return self

    def __exit__(self, *exc_info: object) -> bool:
        self.depth -= 1
        self.exits += 1
        return False


class _FakeDevice:
    """Stand-in for the `torch.device` returned by `_select_discrete_device`."""

    type = "cuda"

    def __str__(self) -> str:
        return "cuda:0"


class _FakeTensor:
    """Minimal parameter stand-in for `_largest_cpu_tensor`'s probe."""

    def __init__(self, ptr: int, nbytes: int, device_type: str = "cpu") -> None:
        self.device = types.SimpleNamespace(type=device_type)
        self._ptr = ptr
        self._nbytes = nbytes

    def numel(self) -> int:
        return self._nbytes

    def element_size(self) -> int:
        return 1

    def data_ptr(self) -> int:
        return self._ptr


def _install_fake_torch(
    recorder: _PatchRecorder, events: list[tuple] | None = None
) -> types.ModuleType:
    """Inject a `torch` module whose `nn.Module` records every `.to()` call.

    The fake deliberately has no `cuda` attribute, so `_clear_torch_cache` cannot
    initialize a real accelerator context. `events`, when given, receives an
    ordered `("to", name, target, patch_depth)` entry per move so a test can
    check the interleaving with other recorded steps.
    """

    class Module:
        def __init__(
            self,
            name: str = "component",
            *,
            ptr: int = 0,
            nbytes: int = 4 << 20,
            device_type: str = "cpu",
        ) -> None:
            # (target, staging patch depth at the time of the move).
            self.moves: list[tuple[object, int]] = []
            self.name = name
            self._tensors = [_FakeTensor(ptr, nbytes, device_type)] if nbytes else []

        def parameters(self) -> list[_FakeTensor]:
            return list(self._tensors)

        def buffers(self) -> list[_FakeTensor]:
            return []

        def to(self, target: object) -> "Module":
            self.moves.append((target, recorder.depth))
            if events is not None:
                events.append(("to", self.name, target, recorder.depth))
            return self

    nn = types.ModuleType("torch.nn")
    nn.Module = Module
    fake_torch = types.ModuleType("torch")
    fake_torch.nn = nn
    fake_torch.bfloat16 = object()
    fake_torch.float16 = object()
    fake_torch.backends = types.SimpleNamespace(cudnn=types.SimpleNamespace(benchmark=True))
    return fake_torch


# Distinct fake addresses so a test can declare exactly which component is still
# backed by the safetensors mapping.
_COMPONENT_PTRS = {"vae": 0x1000, "text_encoder": 0x2000, "text_encoder_2": 0x3000}


class MaterializeComponentsForOffloadTests(unittest.TestCase):
    """Cover the pre-offload re-homing pass in isolation."""

    def setUp(self) -> None:
        self.recorder = _PatchRecorder()
        self.events: list[tuple] = []
        self.torch = _install_fake_torch(self.recorder, self.events)
        modules_patch = patch.dict(sys.modules, {"torch": self.torch, "torch.nn": self.torch.nn})
        modules_patch.start()
        self.addCleanup(modules_patch.stop)

        staging_patch = patch.object(svc, "patched_module_to", self.recorder)
        staging_patch.start()
        self.addCleanup(staging_patch.stop)

        # Every component starts out file-backed; individual tests narrow this.
        self.file_backed_ptrs = set(_COMPONENT_PTRS.values())
        file_backed_patch = patch.object(
            svc, "tensor_needs_staging", lambda tensor: tensor.data_ptr() in self.file_backed_ptrs
        )
        file_backed_patch.start()
        self.addCleanup(file_backed_patch.stop)

        events = self.events
        clear_patch = patch.object(
            svc, "_clear_torch_cache", lambda: events.append(("clear",))
        )
        clear_patch.start()
        self.addCleanup(clear_patch.stop)

        module_cls = self.torch.nn.Module
        self.pipe = types.SimpleNamespace(
            vae=module_cls("vae", ptr=_COMPONENT_PTRS["vae"]),
            text_encoder=module_cls("text_encoder", ptr=_COMPONENT_PTRS["text_encoder"]),
            text_encoder_2=module_cls("text_encoder_2", ptr=_COMPONENT_PTRS["text_encoder_2"]),
            # The GGUF transformer is already in anonymous memory and must not be
            # round-tripped through the GPU.
            transformer=module_cls("transformer", ptr=0x4000),
        )
        self.device = _FakeDevice()

    def test_noop_when_staging_not_required(self) -> None:
        with patch.object(svc, "mmap_staging_required", return_value=False):
            svc._materialize_components_for_offload(self.pipe, self.device)

        for name in ("vae", "text_encoder", "text_encoder_2", "transformer"):
            self.assertEqual(getattr(self.pipe, name).moves, [], name)
        self.assertEqual(self.recorder.enters, 0)
        self.assertEqual(self.events, [])

    def test_components_round_trip_through_the_gpu(self) -> None:
        with patch.object(svc, "mmap_staging_required", return_value=True):
            svc._materialize_components_for_offload(self.pipe, self.device)

        for name in svc._MMAP_BACKED_COMPONENTS:
            with self.subTest(component=name):
                moves = getattr(self.pipe, name).moves
                # Staged move onto the GPU (patch depth 1), then back to the CPU
                # outside the patch, which is what allocates anonymous memory.
                self.assertEqual(moves, [(self.device, 1), ("cpu", 0)])
        self.assertEqual(self.pipe.transformer.moves, [])
        self.assertEqual(self.recorder.depth, 0)
        self.assertEqual(self.recorder.enters, len(svc._MMAP_BACKED_COMPONENTS))

    def test_allocator_cache_is_freed_between_components(self) -> None:
        with patch.object(svc, "mmap_staging_required", return_value=True):
            svc._materialize_components_for_offload(self.pipe, self.device)

        # A ~9 GiB T5 cannot be served out of the small blocks the VAE left
        # behind, so each component's device memory must be released before the
        # next one is staged.
        self.assertEqual(
            self.events,
            [
                ("to", "vae", self.device, 1),
                ("to", "vae", "cpu", 0),
                ("clear",),
                ("to", "text_encoder", self.device, 1),
                ("to", "text_encoder", "cpu", 0),
                ("clear",),
                ("to", "text_encoder_2", self.device, 1),
                ("to", "text_encoder_2", "cpu", 0),
                ("clear",),
                ("clear",),
            ],
        )

    def test_components_that_are_not_file_backed_are_skipped(self) -> None:
        # What an fp16 run looks like: the host-side cast off the BF16 checkpoint
        # already materialized every tensor in anonymous memory.
        self.file_backed_ptrs = set()

        with patch.object(svc, "mmap_staging_required", return_value=True):
            svc._materialize_components_for_offload(self.pipe, self.device)

        for name in svc._MMAP_BACKED_COMPONENTS:
            self.assertEqual(getattr(self.pipe, name).moves, [], name)
        self.assertEqual(self.recorder.enters, 0)
        self.assertEqual(self.events, [("clear",)])

    def test_component_already_on_the_gpu_is_skipped(self) -> None:
        module_cls = self.torch.nn.Module
        self.pipe.vae = module_cls(
            "vae", ptr=_COMPONENT_PTRS["vae"], device_type="cuda"
        )

        with patch.object(svc, "mmap_staging_required", return_value=True):
            svc._materialize_components_for_offload(self.pipe, self.device)

        self.assertEqual(self.pipe.vae.moves, [])
        self.assertEqual(len(self.pipe.text_encoder.moves), 2)

    def test_absent_component_is_skipped(self) -> None:
        del self.pipe.text_encoder

        with patch.object(svc, "mmap_staging_required", return_value=True):
            svc._materialize_components_for_offload(self.pipe, self.device)

        self.assertEqual(len(self.pipe.vae.moves), 2)
        self.assertEqual(len(self.pipe.text_encoder_2.moves), 2)

    def test_failed_move_does_not_fail_the_load(self) -> None:
        def explode(_target: object) -> None:
            raise RuntimeError("HIP out of memory")

        self.pipe.vae.to = explode

        with patch.object(svc, "mmap_staging_required", return_value=True):
            with self.assertLogs(svc.log, level="WARNING") as captured:
                svc._materialize_components_for_offload(self.pipe, self.device)

        self.assertIn("vae", captured.output[0])
        # The pass stops at the first failure; the remaining components keep
        # their file mapping and are moved lazily by accelerate as before.
        self.assertEqual(self.pipe.text_encoder.moves, [])
        self.assertEqual(self.recorder.depth, 0)


class ComponentFileBackingTests(unittest.TestCase):
    """Cover the probe that decides whether a component is worth re-homing."""

    def setUp(self) -> None:
        self.recorder = _PatchRecorder()
        self.torch = _install_fake_torch(self.recorder)
        modules_patch = patch.dict(sys.modules, {"torch": self.torch, "torch.nn": self.torch.nn})
        modules_patch.start()
        self.addCleanup(modules_patch.stop)

    def test_largest_cpu_tensor_ignores_device_tensors(self) -> None:
        module = self.torch.nn.Module("m", ptr=0x10, device_type="cuda")
        self.assertIsNone(svc._largest_cpu_tensor(module))

    def test_module_without_tensors_is_not_file_backed(self) -> None:
        module = self.torch.nn.Module("m", nbytes=0)
        with patch.object(svc, "tensor_needs_staging", side_effect=AssertionError):
            self.assertFalse(svc._component_is_file_backed(module))

    def test_probe_uses_the_largest_cpu_tensor(self) -> None:
        module = self.torch.nn.Module("m")
        module._tensors = [
            _FakeTensor(0x10, 1 << 10),
            _FakeTensor(0x20, 1 << 24),
            _FakeTensor(0x30, 1 << 20),
        ]
        seen: list[int] = []

        def probe(tensor: _FakeTensor) -> bool:
            seen.append(tensor.data_ptr())
            return True

        with patch.object(svc, "tensor_needs_staging", probe):
            self.assertTrue(svc._component_is_file_backed(module))
        self.assertEqual(seen, [0x20])


class PipelinePlacementTests(unittest.TestCase):
    """Cover which placement path `_ensure_pipeline_locked` takes."""

    def setUp(self) -> None:
        self.recorder = _PatchRecorder()
        self.torch = _install_fake_torch(self.recorder)
        recorder = self.recorder
        module_cls = self.torch.nn.Module

        class FakePipe:
            def __init__(self) -> None:
                self.vae = module_cls("vae", ptr=_COMPONENT_PTRS["vae"])
                self.text_encoder = module_cls(
                    "text_encoder", ptr=_COMPONENT_PTRS["text_encoder"]
                )
                self.text_encoder_2 = module_cls(
                    "text_encoder_2", ptr=_COMPONENT_PTRS["text_encoder_2"]
                )
                self.transformer = module_cls("transformer", ptr=0x4000)
                self.moves: list[tuple[object, int]] = []
                self.offload_devices: list[str] = []

            def to(self, device: object) -> "FakePipe":
                self.moves.append((device, recorder.depth))
                return self

            def set_progress_bar_config(self, **_kwargs: object) -> None:
                pass

            def enable_model_cpu_offload(self, device: str) -> None:
                self.offload_devices.append(device)

        self.pipes: list[FakePipe] = []
        pipes = self.pipes

        class FluxFillPipeline:
            @staticmethod
            def from_pretrained(*_args: object, **_kwargs: object) -> FakePipe:
                pipe = FakePipe()
                pipes.append(pipe)
                return pipe

        class FluxTransformer2DModel:
            @staticmethod
            def from_single_file(*_args: object, **_kwargs: object) -> object:
                return object()

        diffusers = types.ModuleType("diffusers")
        diffusers.FluxFillPipeline = FluxFillPipeline
        diffusers.FluxTransformer2DModel = FluxTransformer2DModel
        diffusers.GGUFQuantizationConfig = lambda **_kwargs: object()

        modules_patch = patch.dict(
            sys.modules,
            {"torch": self.torch, "torch.nn": self.torch.nn, "diffusers": diffusers},
        )
        modules_patch.start()
        self.addCleanup(modules_patch.stop)

        for name, replacement in (
            ("patched_module_to", self.recorder),
            ("_is_nonempty_file", lambda _path: True),
            ("_components_present", lambda: True),
            ("_select_discrete_device", _FakeDevice),
            ("_apply_miopen_fast", lambda: None),
            (
                "tensor_needs_staging",
                lambda tensor: tensor.data_ptr() in set(_COMPONENT_PTRS.values()),
            ),
        ):
            attr_patch = patch.object(svc, name, replacement)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)

        self.service = svc.FluxFillInpaintService(LoadedModelManager())

    def _build(self, model_key: str = "flux_fill:test", **overrides: object):
        params: dict[str, object] = {"mode": "inpaint"}
        params.update(overrides)
        normalized = svc.normalize_flux_fill_params(params)
        return self.service._ensure_pipeline_locked(normalized, model_key)

    def test_plain_placement_moves_pipeline_inside_staging_patch(self) -> None:
        pipe = self._build(cpu_offload=False)

        self.assertEqual([depth for _dev, depth in pipe.moves], [1])
        self.assertEqual(pipe.offload_devices, [])
        self.assertEqual(self.recorder.depth, 0)

    def test_offload_placement_rehomes_components_first(self) -> None:
        with patch.object(svc, "mmap_staging_required", return_value=True):
            pipe = self._build(cpu_offload=True)

        # The pipeline itself is never moved wholesale in the offload path.
        self.assertEqual(pipe.moves, [])
        self.assertEqual(pipe.offload_devices, ["cuda:0"])
        self.assertEqual(len(pipe.vae.moves), 2)
        self.assertEqual(pipe.transformer.moves, [])

    def test_cached_pipeline_is_reused(self) -> None:
        first = self._build()
        second = self._build()

        self.assertIs(first, second)
        self.assertEqual(len(self.pipes), 1)

    def test_quant_swap_rebuilds_and_reports_the_previous_key(self) -> None:
        self._build("flux_fill:Q8_0", quant="Q8_0")
        with patch.object(self.service._model_manager, "mark_unloaded") as unloaded:
            second = self._build("flux_fill:Q4_0", quant="Q4_0")

        unloaded.assert_called_once_with("flux_fill:Q8_0")
        self.assertEqual(len(self.pipes), 2)
        self.assertIs(second, self.pipes[1])
        self.assertEqual(self.service._active_key, "flux_fill:Q4_0")


class _SlowFluxResponse:
    """Streaming stand-in whose body arrives slowly, one distinct payload per GET.

    Distinct payloads are the point: if two callers stage into the same file
    their bytes interleave, and the published result matches neither request.
    """

    def __init__(self, payload: bytes) -> None:
        self._payload = payload
        self.headers = {"Content-Length": str(len(payload))}

    def __enter__(self) -> "_SlowFluxResponse":
        return self

    def __exit__(self, *_exc_info: object) -> bool:
        return False

    def raise_for_status(self) -> None:
        return None

    def iter_content(self, chunk_size: int = 1 << 20):
        for start in range(0, len(self._payload), 4096):
            yield self._payload[start : start + 4096]
            time.sleep(0.01)


class WeightDownloadConcurrencyTests(unittest.TestCase):
    """IPC dispatches onto a thread pool, so two first uses of a quant can race.

    FLUX stages ~22 GB per quant, so a staging file shared between requests is a
    very wide corruption window: both writers truncate and rewrite the same path,
    one can hand a half-rewritten file to the reader, and the loser's
    `os.replace` fails outright because the winner already moved it away.
    """

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.dest = self.root / "flux1-fill-dev-Q8_0.gguf"

        self.payloads: list[bytes] = []
        self.requests_seen: list[dict[str, str]] = []
        self.lock = threading.Lock()

        def get(url: str, **kwargs: object) -> _SlowFluxResponse:
            with self.lock:
                index = len(self.requests_seen)
                headers = kwargs.get("headers")
                self.requests_seen.append(dict(headers) if isinstance(headers, dict) else {})
                payload = bytes([65 + index]) * 40960
                self.payloads.append(payload)
            return _SlowFluxResponse(payload)

        module = types.ModuleType("requests")
        module.get = get
        modules_patch = patch.dict(sys.modules, {"requests": module})
        modules_patch.start()
        self.addCleanup(modules_patch.stop)

    def _download(self, errors: list[BaseException]) -> None:
        try:
            svc._download_file_streaming("https://hf/flux.gguf", str(self.dest), lambda _n: None)
        except BaseException as exc:  # noqa: BLE001 - re-raised by the assertion below
            errors.append(exc)

    def test_two_concurrent_downloads_fetch_once_and_do_not_corrupt(self) -> None:
        errors: list[BaseException] = []
        threads = [threading.Thread(target=self._download, args=(errors,)) for _ in range(2)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=60)

        self.assertEqual(errors, [])
        # The loser re-checks under the lock: a ~22 GB refetch is not free.
        self.assertEqual(len(self.requests_seen), 1)
        self.assertEqual(self.dest.read_bytes(), self.payloads[0])
        self.assertEqual(list(self.root.glob("*.part")), [])

    def test_a_present_file_is_not_downloaded_again(self) -> None:
        self.dest.write_bytes(b"already here")

        self.assertFalse(
            svc._download_file_streaming("https://hf/flux.gguf", str(self.dest), lambda _n: None)
        )
        self.assertEqual(self.requests_seen, [])

    def test_the_hf_token_is_still_sent_as_a_bearer_header(self) -> None:
        with patch.dict("os.environ", {"HF_TOKEN": "hf_secret"}):
            self.assertTrue(
                svc._download_file_streaming(
                    "https://hf/flux.gguf", str(self.dest), lambda _n: None
                )
            )

        self.assertEqual(self.requests_seen[0].get("Authorization"), "Bearer hf_secret")


class UnloadTests(unittest.TestCase):
    def test_unload_drops_the_pipeline_and_reports_it(self) -> None:
        service = svc.FluxFillInpaintService(LoadedModelManager())
        service._pipe = object()
        service._active_key = "flux_fill:Q8_0"

        with patch.object(service._model_manager, "mark_unloaded") as unloaded:
            self.assertTrue(service.unload())

        unloaded.assert_called_once_with("flux_fill:Q8_0")
        self.assertIsNone(service._pipe)
        self.assertIsNone(service._active_key)

    def test_unload_without_a_pipeline_is_a_noop(self) -> None:
        service = svc.FluxFillInpaintService(LoadedModelManager())
        self.assertFalse(service.unload())

    def test_unload_key_refuses_a_foreign_key(self) -> None:
        service = svc.FluxFillInpaintService(LoadedModelManager())
        service._pipe = object()
        service._active_key = "flux_fill:Q8_0"

        self.assertFalse(service._unload_key("flux_fill:Q4_0"))
        self.assertIsNotNone(service._pipe)


if __name__ == "__main__":
    unittest.main()
