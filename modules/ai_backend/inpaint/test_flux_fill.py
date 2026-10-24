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
  into a process-private file, instead of writing over each other;
- verify the download rides the shared retrying primitive: a dropped connection
  is retried and resumed with `Range`, a failure that ends the call parks the
  bytes for the next attempt, and the listing size is the integrity gate;
- verify the download plan refuses instead of guessing: a failed GGUF listing or
  a quant missing from it raises, and every planned size comes from the listing;
- verify `ensure_model` reports a retry as an ordinary `download` frame with a
  retry label, threads `should_cancel` into the transfer, and logs (once) rather
  than swallows a failing progress callback;
- verify the resolved plan is cached next to the model: a complete model runs
  with no listing at all, a missing file refetches the listing, a malformed or
  escaping cache is ignored, and without a cache a failed listing is tolerated
  only when the model is already on disk.

Notes:
- Fake `torch` and `diffusers` modules are injected into `sys.modules`, so the
  tests need neither package, nor the FLUX weights, nor a GPU. The weight
  transfer runs for real against a fake transport installed in the shared
  primitive's `_default_get` / `_default_sleep` seams; no network, no waiting.
- Whether the staging patch does anything on the current host is
  `rocm_mmap_transfer`'s own contract (covered by `test_rocm_mmap_transfer.py`).
  What is pinned here is only how the service drives it.
"""

from __future__ import annotations

import hashlib
import sys
import tempfile
import threading
import time
import types
import unittest
from pathlib import Path
from unittest.mock import patch

from modules.ai_backend.engines import model_download as md
from modules.ai_backend.inpaint import flux_fill as svc
from modules.ai_backend.runtime.model_manager import SmallModelManager


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

        self.service = svc.FluxFillInpaintService(SmallModelManager())

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

        self.sleeps: list[float] = []
        self._install(get)

    def _install(self, get: object) -> None:
        """Route the shared primitive's transport and clock seams to fakes."""
        for name, value in (("_default_get", get), ("_default_sleep", self.sleeps.append)):
            seam = patch.object(md, name, value)
            seam.start()
            self.addCleanup(seam.stop)

    def _fetch(self, expected_size: int = 40960) -> bool:
        return svc._download_file_streaming(
            "https://hf/flux.gguf", str(self.dest), expected_size, lambda _n: None
        )

    def _download(self, errors: list[BaseException]) -> None:
        try:
            self._fetch()
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
        self.dest.write_bytes(b"A" * 40960)

        self.assertFalse(self._fetch())
        self.assertEqual(self.requests_seen, [])

    def test_a_present_file_at_the_wrong_size_is_replaced(self) -> None:
        self.dest.write_bytes(b"truncated")

        self.assertTrue(self._fetch())
        self.assertEqual(self.dest.read_bytes(), self.payloads[0])

    def test_a_dropped_connection_is_retried_and_resumed(self) -> None:
        import requests

        full = bytes(range(256)) * 160
        calls: list[dict[str, str]] = []

        def get(url: str, **kwargs: object) -> _ScriptedFluxResponse:
            headers = dict(kwargs.get("headers") or {})  # type: ignore[arg-type]
            calls.append(headers)
            if "Range" not in headers:
                return _ScriptedFluxResponse(
                    full[:4096], status=200, announce=len(full),
                    raise_after=requests.exceptions.ChunkedEncodingError("reset"),
                )
            start = int(headers["Range"].split("=", 1)[1].rstrip("-"))
            return _ScriptedFluxResponse(full[start:], status=206, start=start, total=len(full))

        self._install(get)

        self.assertTrue(self._fetch(len(full)))
        self.assertEqual(self.dest.read_bytes(), full)
        self.assertEqual([headers.get("Range") for headers in calls], [None, "bytes=4096-"])
        self.assertTrue(self.sleeps)
        self.assertEqual(list(self.root.glob("*.part")), [])

    def test_a_failure_that_ends_the_call_parks_the_bytes(self) -> None:
        """FLUX.1-Fill resumes: a ~12 GB quant must not restart from zero."""

        def get(url: str, **kwargs: object) -> _ScriptedFluxResponse:
            return _ScriptedFluxResponse(b"A" * 4096, status=200, announce=40960, raise_after=OSError("disk gone"))

        self._install(get)

        # `OSError` is not a transport error, so it is permanent: no retry.
        with self.assertRaises(OSError):
            self._fetch()

        self.assertFalse(self.dest.exists())
        self.assertEqual(md.staged_bytes(self.dest), 4096)
        self.assertEqual(self.sleeps, [])

    def test_the_hf_token_is_still_sent_as_a_bearer_header(self) -> None:
        with patch.dict("os.environ", {"HF_TOKEN": "hf_secret"}):
            self.assertTrue(self._fetch())

        self.assertEqual(self.requests_seen[0].get("Authorization"), "Bearer hf_secret")


class _ScriptedFluxResponse:
    """Streaming stand-in with a status, an optional `Content-Range` and a scripted end."""

    def __init__(
        self,
        body: bytes,
        *,
        status: int,
        announce: int | None = None,
        start: int = 0,
        total: int | None = None,
        raise_after: BaseException | None = None,
    ) -> None:
        self._body = body
        self._raise_after = raise_after
        self.status_code = status
        self.headers = {"Content-Length": str(len(body) if announce is None else announce)}
        if status == 206:
            self.headers["Content-Range"] = f"bytes {start}-{start + len(body) - 1}/{total}"

    def __enter__(self) -> "_ScriptedFluxResponse":
        return self

    def __exit__(self, *_exc_info: object) -> bool:
        return False

    def raise_for_status(self) -> None:
        if self.status_code >= 400:
            raise RuntimeError(f"HTTP {self.status_code}")

    def iter_content(self, chunk_size: int = 1 << 20):
        for start in range(0, len(self._body), 1024):
            yield self._body[start : start + 1024]
        if self._raise_after is not None:
            raise self._raise_after


class _Sibling:
    def __init__(self, rfilename: str, size: int | None, sha256: str | None = None) -> None:
        self.rfilename = rfilename
        self.size = size
        # `BlobLfsInfo` surface: LFS files carry their digest, small configs none.
        self.lfs = types.SimpleNamespace(size=size, sha256=sha256, pointer_size=134) if sha256 else None


class _HubInfo:
    def __init__(self, siblings: list[_Sibling]) -> None:
        self.siblings = siblings


class DownloadPlanTests(unittest.TestCase):
    """The plan's sizes are the integrity gate, so the listing is mandatory."""

    VAE_SHA = "ab" * 32
    COMPONENTS = [
        _Sibling("model_index.json", 500),
        _Sibling("vae/diffusion_pytorch_model.safetensors", 9000, VAE_SHA),
    ]

    def _plan(self, gguf_listing: object) -> list[dict[str, object]]:
        components = self.COMPONENTS

        class _Api:
            def __init__(self, token: object = None) -> None:
                del token

            def model_info(self, repo: str, files_metadata: bool = False) -> _HubInfo:
                if repo == svc.GGUF_REPO:
                    if isinstance(gguf_listing, BaseException):
                        raise gguf_listing
                    return gguf_listing  # type: ignore[return-value]
                return _HubInfo(components)

        hub = types.ModuleType("huggingface_hub")
        hub.HfApi = _Api  # type: ignore[attr-defined]
        hub.hf_hub_url = lambda repo, path: f"https://hf/{repo}/{path}"  # type: ignore[attr-defined]
        with patch.dict(sys.modules, {"huggingface_hub": hub}):
            return svc._build_download_plan("Q8_0")

    def test_sizes_come_from_the_listing(self) -> None:
        plan = self._plan(_HubInfo([_Sibling(svc.gguf_filename("Q8_0"), 12_000)]))

        self.assertEqual([item["size"] for item in plan], [12_000, 500, 9000])

    def test_lfs_digests_come_from_the_listing(self) -> None:
        gguf_sha = "CD" * 32
        plan = self._plan(_HubInfo([_Sibling(svc.gguf_filename("Q8_0"), 12_000, gguf_sha)]))

        # Lowercased; a non-LFS config has none and is checked by size alone.
        self.assertEqual([item["sha256"] for item in plan], [gguf_sha.lower(), None, self.VAE_SHA])

    def test_a_failed_gguf_listing_raises_instead_of_planning_size_zero(self) -> None:
        with self.assertRaises(RuntimeError) as caught:
            self._plan(ConnectionError("hub down"))

        self.assertIn(svc.GGUF_REPO, str(caught.exception))

    def test_a_quant_missing_from_the_listing_raises(self) -> None:
        with self.assertRaises(RuntimeError) as caught:
            self._plan(_HubInfo([_Sibling("other.gguf", 1)]))

        self.assertIn(svc.gguf_filename("Q8_0"), str(caught.exception))


class EnsureModelTests(unittest.TestCase):
    """`ensure_model` drives the transfer: retry frames, cancel, callback failures."""

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        self.dest = root / "flux1-fill-dev-Q8_0.gguf"
        self.plan = [
            {
                "url": "https://hf/flux.gguf",
                "dest": str(self.dest),
                "size": 2048,
                "sha256": hashlib.sha256(b"A" * 2048).hexdigest(),
            }
        ]
        for target, value in (
            ("_flux_dir", lambda: str(root)),
            ("_components_dir", lambda: str(root / "components")),
            ("_build_download_plan", lambda quant: self.plan),
        ):
            seam = patch.object(svc, target, value)
            seam.start()
            self.addCleanup(seam.stop)
        self.service = svc.FluxFillInpaintService(SmallModelManager())
        self.calls: list[dict[str, str]] = []
        self.sleeps: list[float] = []

        def get(url: str, **kwargs: object) -> _ScriptedFluxResponse:
            headers = dict(kwargs.get("headers") or {})  # type: ignore[arg-type]
            self.calls.append(headers)
            if len(self.calls) == 1:
                return _ScriptedFluxResponse(b"A" * 1024, status=200, announce=2048)
            return _ScriptedFluxResponse(b"A" * 1024, status=206, start=1024, total=2048)

        for name, value in (("_default_get", get), ("_default_sleep", self.sleeps.append)):
            seam = patch.object(md, name, value)
            seam.start()
            self.addCleanup(seam.stop)

    def test_a_retry_is_a_download_frame_with_a_retry_label(self) -> None:
        frames: list[tuple[str, int, int, str]] = []

        self.service.ensure_model("Q8_0", lambda *frame: frames.append(frame))

        self.assertEqual(self.dest.read_bytes(), b"A" * 2048)
        retry = [frame for frame in frames if frame[3].startswith("Сеть недоступна")]
        self.assertEqual(len(retry), 1)
        self.assertEqual(retry[0][0], "download")
        self.assertIn("1/5", retry[0][3])
        self.assertTrue(retry[0][3].endswith(self.dest.name))
        self.assertEqual(frames[-1][1], 2048)

    def test_a_digest_mismatch_deletes_the_file_and_fails(self) -> None:
        self.plan[0]["sha256"] = "0" * 64

        with self.assertRaises(md.HashMismatchError):
            self.service.ensure_model("Q8_0", None)

        self.assertFalse(self.dest.exists())
        self.assertEqual(md.staged_bytes(self.dest), 0)

    def test_should_cancel_reaches_the_backoff_wait(self) -> None:
        with self.assertRaises(md.DownloadCancelled):
            self.service.ensure_model("Q8_0", None, lambda: bool(self.sleeps))

        self.assertEqual(len(self.calls), 1)
        self.assertFalse(self.dest.exists())
        self.assertEqual(md.staged_bytes(self.dest), 1024)

    def test_a_failing_progress_callback_is_logged_once_and_does_not_abort(self) -> None:
        def broken(*_frame: object) -> None:
            raise ValueError("peer gone")

        with self.assertLogs(svc.log, level="WARNING") as logs:
            self.service.ensure_model("Q8_0", broken)

        self.assertEqual(self.dest.read_bytes(), b"A" * 2048)
        callback_lines = [line for line in logs.output if "progress callback failed" in line]
        self.assertEqual(len(callback_lines), 1)


class CachedPlanTests(unittest.TestCase):
    """The listing is fetched once; a complete model never needs the network again."""

    PAYLOAD = b"A" * 2048

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.dest = self.root / svc.gguf_filename("Q8_0")
        self.listings = 0
        self.listing_error: BaseException | None = None

        def build_plan(quant: str) -> list[dict[str, object]]:
            self.listings += 1
            if self.listing_error is not None:
                raise self.listing_error
            return [{"url": "https://hf/flux.gguf", "dest": str(self.dest), "size": 2048, "sha256": None}]

        for target, value in (
            ("_flux_dir", lambda: str(self.root)),
            ("_components_dir", lambda: str(self.root / "components")),
            ("_build_download_plan", build_plan),
        ):
            seam = patch.object(svc, target, value)
            seam.start()
            self.addCleanup(seam.stop)
        self.gets = 0

        def get(url: str, **kwargs: object) -> _ScriptedFluxResponse:
            self.gets += 1
            return _ScriptedFluxResponse(self.PAYLOAD, status=200, announce=2048)

        for name, value in (("_default_get", get), ("_default_sleep", lambda _s: None)):
            seam = patch.object(md, name, value)
            seam.start()
            self.addCleanup(seam.stop)
        self.service = svc.FluxFillInpaintService(SmallModelManager())

    def test_a_complete_model_runs_without_a_listing(self) -> None:
        self.service.ensure_model("Q8_0", None)
        self.assertEqual((self.listings, self.gets), (1, 1))

        self.listing_error = RuntimeError("offline")
        self.service.ensure_model("Q8_0", None)

        self.assertEqual((self.listings, self.gets), (1, 1))
        self.assertEqual(svc._load_cached_plan("Q8_0"), [{"url": "https://hf/flux.gguf", "dest": str(self.dest), "size": 2048, "sha256": None}])

    def test_a_missing_file_refetches_the_listing(self) -> None:
        self.service.ensure_model("Q8_0", None)
        self.dest.unlink()

        self.service.ensure_model("Q8_0", None)

        self.assertEqual((self.listings, self.gets), (2, 2))
        self.assertEqual(self.dest.read_bytes(), self.PAYLOAD)

    def test_a_malformed_or_escaping_cache_is_ignored(self) -> None:
        cache = Path(svc._plan_cache_path("Q8_0"))
        for content in ("{not json", '{"version": 1, "quant": "Q8_0", "files": [{"url": "u", "rel": "../x", "size": 1, "sha256": null}]}', '{"version": 1, "quant": "Q8_0", "files": []}'):
            with self.subTest(content=content):
                cache.write_text(content, encoding="utf-8")
                self.assertIsNone(svc._load_cached_plan("Q8_0"))

    def test_without_a_cache_a_failed_listing_is_tolerated_only_for_a_present_model(self) -> None:
        self.listing_error = RuntimeError("offline")
        with self.assertRaisesRegex(RuntimeError, "offline"):
            self.service.ensure_model("Q8_0", None)

        self.dest.write_bytes(self.PAYLOAD)
        with patch.object(svc, "_components_present", lambda: True), \
                self.assertLogs(svc.log, level="WARNING") as logs:
            self.service.ensure_model("Q8_0", None)

        self.assertEqual(self.gets, 0)
        self.assertTrue(any("no cached plan" in line for line in logs.output))


class UnloadTests(unittest.TestCase):
    def test_unload_drops_the_pipeline_and_reports_it(self) -> None:
        service = svc.FluxFillInpaintService(SmallModelManager())
        service._pipe = object()
        service._active_key = "flux_fill:Q8_0"

        with patch.object(service._model_manager, "mark_unloaded") as unloaded:
            self.assertTrue(service.unload())

        unloaded.assert_called_once_with("flux_fill:Q8_0")
        self.assertIsNone(service._pipe)
        self.assertIsNone(service._active_key)

    def test_unload_without_a_pipeline_is_a_noop(self) -> None:
        service = svc.FluxFillInpaintService(SmallModelManager())
        self.assertFalse(service.unload())

    def test_unload_key_refuses_a_foreign_key(self) -> None:
        service = svc.FluxFillInpaintService(SmallModelManager())
        service._pipe = object()
        service._active_key = "flux_fill:Q8_0"

        self.assertFalse(service._unload_key("flux_fill:Q4_0"))
        self.assertIsNotNone(service._pipe)


if __name__ == "__main__":
    unittest.main()
