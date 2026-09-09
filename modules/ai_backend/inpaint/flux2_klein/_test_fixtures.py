"""
File: modules/ai_backend/inpaint/flux2_klein/_test_fixtures.py

Purpose:
Shared fixtures for the FLUX.2 klein test modules: the on-disk model tree the
path-based tests need, the fake `torch` / `diffusers` / `transformers` stack that
lets the placement and generation tests run without those packages, the klein
weights or a GPU, and `_PlacementFixture`, the base case that installs all of it.

Key declarations:
- `_make_model_tree` / `_write_safetensors` / `_write_safetensors_with_data` /
  `_png_bytes` - on-disk inputs. The first safetensors helper writes a header-only
  container, the second a real one with tensor bytes at chosen offsets.
- `VAE_SAMPLE_SIZE` / `VAE_BLOCK_OUT_CHANNELS` and the thresholds derived from them -
  the fixture VAE's tiling geometry, written both into its `config.json` and into
  `_make_fake_vae`, so "tiling requested" and "tiling engaged" are different states
  a test can actually assert. They are DERIVED exactly as diffusers derives them and
  must never be set independently of each other.
- `DEFAULT_MODEL_INDEX` - the `model_index.json` the tree carries by default, kept
  faithful to the shipped klein 4B one (`is_distilled: true`). Pass another dict,
  or `None` for no manifest at all, to build an undeclared checkpoint.
- `_TempTreeCase` - a `TestCase` with a complete model tree in a temp directory.
- `_install_fake_torch` and the `_Fake*` doubles - the injected torch stack.
- `_PlacementFixture` - `_TempTreeCase` plus the fake modules and the three
  monkeypatches every pipeline-building test needs.
- `_ResidencyModule` - a module double whose parameters and buffers sit on chosen
  devices, for the per-component residency and action tests.

Notes:
- The leading underscore keeps pytest from collecting this file as a test module.
- Module attributes are patched on the module that DEFINES them (`pipeline`,
  `hardware`, ...), never on the `flux2_klein` package: the package's re-export is
  a separate binding, so patching it would not reach the real callers.
"""

from __future__ import annotations

import json
import struct
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch

import numpy as np
from PIL import Image

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import hardware, pipeline
from modules.ai_backend.runtime.model_manager import LoadedModelManager


# ---------------------------------------------------------------------------
# Fixtures shared by several test classes
# ---------------------------------------------------------------------------
#: `sample_size` and `block_out_channels` written into the fixture VAE config and
#: mirrored by `_make_fake_vae`. They are NOT two independent knobs: diffusers
#: derives `tile_sample_min_size = sample_size` and
#: `tile_latent_min_size = int(sample_size / 2 ** (len(block_out_channels) - 1))`
#: in `AutoencoderKLFlux2.__init__`, so a fixture that sets them separately would
#: model a VAE that cannot exist. These are the shipped klein VAE's values.
VAE_SAMPLE_SIZE = 1024
VAE_BLOCK_OUT_CHANNELS = (128, 256, 512, 512)

#: Pixels per latent cell for that VAE, and the two thresholds it implies.
VAE_LATENT_SCALE = 2 ** (len(VAE_BLOCK_OUT_CHANNELS) - 1)
VAE_TILE_LATENT_MIN_SIZE = int(VAE_SAMPLE_SIZE / VAE_LATENT_SCALE)


#: `model_index.json` written into the fixture tree by default, trimmed from the
#: shipped `side_models/FLUX.2-klein-4B/model_index.json` to the fields this
#: backend reads. The real checkpoint declares `is_distilled: true`, so the
#: default fixture must too — a fixture that quietly declared `false` would let a
#: regression in the guidance skip pass every test.
DEFAULT_MODEL_INDEX: dict[str, object] = {
    "_class_name": "Flux2KleinPipeline",
    "is_distilled": True,
}


def _make_model_tree(
    root: Path,
    *,
    single_file_transformer: bool = False,
    model_index: dict[str, object] | None = DEFAULT_MODEL_INDEX,
) -> dict[str, str]:
    """Lay out a klein-like checkout and return the three user-supplied paths.

    `model_index` is the manifest written at the checkout root; pass `None` to
    build a tree with no `model_index.json` at all, which is what a
    hand-assembled folder of components looks like.
    """
    if model_index is not None:
        root.mkdir(parents=True, exist_ok=True)
        (root / "model_index.json").write_text(json.dumps(model_index), encoding="utf-8")
    (root / "text_encoder").mkdir(parents=True)
    (root / "text_encoder" / "config.json").write_text("{}", encoding="utf-8")
    (root / "text_encoder" / "model.safetensors").write_bytes(b"\x00" * 2048)
    (root / "vae").mkdir()
    # The two fields the tiling threshold is derived from, at the shipped klein
    # VAE's values: `sample_size` 1024 over four `block_out_channels` gives a
    # latent threshold of 128 and therefore a 1024 px side threshold, which is
    # what `components.vae_tile_threshold_pixels` must answer. A `{}` config
    # would send every test down the unreadable-config fallback instead.
    (root / "vae" / "config.json").write_text(
        json.dumps({"sample_size": VAE_SAMPLE_SIZE, "block_out_channels": list(VAE_BLOCK_OUT_CHANNELS)}),
        encoding="utf-8",
    )
    (root / "vae" / "diffusion_pytorch_model.safetensors").write_bytes(b"\x00" * 1024)
    (root / "tokenizer").mkdir()
    (root / "tokenizer" / "tokenizer_config.json").write_text("{}", encoding="utf-8")
    (root / "scheduler").mkdir()
    (root / "scheduler" / "scheduler_config.json").write_text("{}", encoding="utf-8")

    if single_file_transformer:
        # The layout a klein standalone release actually ships: the checkpoint in
        # the repository root and its own `transformer/config.json` beside it.
        # Without that config the load is refused, so it belongs to the fixture.
        transformer = root / "flux2-klein.safetensors"
        _write_safetensors(transformer, {"single_stream_modulation.lin.weight": {"dtype": "BF16"}})
        (root / "transformer").mkdir()
        (root / "transformer" / "config.json").write_text(
            json.dumps({"_class_name": "Flux2Transformer2DModel"}), encoding="utf-8"
        )
    else:
        transformer = root / "transformer"
        transformer.mkdir()
        (transformer / "config.json").write_text("{}", encoding="utf-8")
        (transformer / "diffusion_pytorch_model.safetensors").write_bytes(b"\x00" * 4096)

    return {
        "text_encoder_path": str(root / "text_encoder"),
        "transformer_path": str(transformer),
        "vae_path": str(root / "vae"),
    }


def _write_safetensors(path: Path, header: dict[str, object]) -> None:
    """Write a safetensors container carrying only `header` (no tensor bytes)."""
    payload = json.dumps(header).encode("utf-8")
    path.write_bytes(struct.pack("<Q", len(payload)) + payload)


def _write_safetensors_with_data(
    path: Path,
    tensors: dict[str, tuple[str, tuple[int, ...], bytes]],
    *,
    metadata: dict[str, str] | None = None,
) -> None:
    """Write a REAL safetensors container: header plus the tensor bytes themselves.

    `tensors` maps a tensor name to `(safetensors dtype token, shape, payload)`.
    Data offsets are assigned in INSERTION order, and the JSON header preserves
    that order too, so a caller that inserts anti-alphabetically produces a file
    whose key order and whose byte order genuinely disagree — which is what the
    file-order tests need. No consistency between `shape`, the dtype and
    `len(payload)` is enforced here: a test that wants a corrupt container asks
    for one explicitly.

    The sibling `_write_safetensors` stays the right tool whenever only the
    header matters (dtype refusals, fp8 detection, size accounting).
    """
    header: dict[str, object] = {}
    if metadata is not None:
        header["__metadata__"] = metadata
    offset = 0
    blobs: list[bytes] = []
    for name, (dtype_token, shape, payload) in tensors.items():
        header[name] = {
            "dtype": dtype_token,
            "shape": list(shape),
            "data_offsets": [offset, offset + len(payload)],
        }
        blobs.append(payload)
        offset += len(payload)
    encoded = json.dumps(header).encode("utf-8")
    path.write_bytes(struct.pack("<Q", len(encoded)) + encoded + b"".join(blobs))


def _png_bytes(array: np.ndarray, mode: str) -> bytes:
    import io

    with io.BytesIO() as buffer:
        Image.fromarray(array, mode).save(buffer, format="PNG")
        return buffer.getvalue()


class _TempTreeCase(unittest.TestCase):
    """Base class giving every test a throwaway klein-like model tree."""

    single_file_transformer = False
    #: Manifest written at the tree's root; `None` builds a tree without one.
    #: Override it in a subclass to exercise a checkpoint that declares
    #: `is_distilled: false` or declares nothing.
    model_index: dict[str, object] | None = DEFAULT_MODEL_INDEX

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.paths = _make_model_tree(
            self.root,
            single_file_transformer=self.single_file_transformer,
            model_index=self.model_index,
        )

    def params(self, **overrides: object) -> dict[str, object]:
        merged: dict[str, object] = dict(self.paths)
        merged.update(overrides)
        return merged

    def declare_distilled(self, value: object) -> None:
        """Rewrite the tree's `model_index.json` in place; `None` removes the file.

        `value` is written verbatim as `is_distilled`, so a test can also declare
        a non-boolean and prove it is refused. The manifest is read at call time,
        never cached, so this takes effect for the next request.
        """
        manifest = self.root / "model_index.json"
        if value is None:
            manifest.unlink(missing_ok=True)
            return
        manifest.write_text(
            json.dumps({"_class_name": "Flux2KleinPipeline", "is_distilled": value}),
            encoding="utf-8",
        )


# ---------------------------------------------------------------------------
# Fake torch / diffusers / transformers
# ---------------------------------------------------------------------------
class _PatchRecorder:
    """Recording stand-in for `rocm_mmap_transfer.patched_module_to`.

    The instance is both the factory and the context manager, so `depth` can be
    sampled from inside a faked `.to()` to prove the move happened while the
    staging patch was installed.
    """

    def __init__(self) -> None:
        self.depth = 0
        self.enters = 0

    def __call__(self) -> "_PatchRecorder":
        return self

    def __enter__(self) -> "_PatchRecorder":
        self.depth += 1
        self.enters += 1
        return self

    def __exit__(self, *exc_info: object) -> bool:
        self.depth -= 1
        return False


class _FakeDevice:
    """Stand-in for `torch.device`."""

    def __init__(self, spec: object = "cuda:0") -> None:
        self.spec = str(spec)
        self.type = self.spec.split(":")[0]

    def __str__(self) -> str:
        return self.spec

    def __eq__(self, other: object) -> bool:
        return isinstance(other, _FakeDevice) and other.spec == self.spec

    def __hash__(self) -> int:
        return hash(self.spec)


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


class _FakeOutOfMemoryError(RuntimeError):
    """Stand-in for `torch.OutOfMemoryError`."""


class _FakeEmbeds:
    """Prompt-embedding stand-in: records every device it was moved to.

    `_encode_prompt_phase` calls `.detach().to("cpu")` positionally and
    `_generate_locked` calls `.to(device=...)` by keyword, so both spellings are
    accepted.
    """

    def __init__(self, text: str = "") -> None:
        self.text = text
        self.moves: list[object] = []

    def detach(self) -> "_FakeEmbeds":
        return self

    def to(self, device: object = None, **kwargs: object) -> "_FakeEmbeds":
        self.moves.append(device if device is not None else kwargs.get("device"))
        return self


def _install_fake_torch(recorder: _PatchRecorder) -> types.ModuleType:
    """Inject a `torch` whose `nn.Module` records every `.to()` and its patch depth.

    The fake deliberately has no `cuda` attribute, so `_clear_torch_cache` cannot
    initialize a real accelerator context.
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
            self.device = _FakeDevice(device_type)
            self.dtype = "bfloat16"
            self._tensors = [_FakeTensor(ptr, nbytes, device_type)] if nbytes else []

        def parameters(self) -> list[_FakeTensor]:
            return list(self._tensors)

        def buffers(self) -> list[_FakeTensor]:
            return []

        def to(self, target: object) -> "Module":
            self.moves.append((target, recorder.depth))
            self.device = target if isinstance(target, _FakeDevice) else _FakeDevice(target)
            for tensor in self._tensors:
                tensor.device = types.SimpleNamespace(type=self.device.type)
            return self

    class Generator:
        def __init__(self, _device: str = "cpu") -> None:
            self.seed: int | None = None

        def manual_seed(self, seed: int) -> "Generator":
            self.seed = int(seed)
            return self

    class _NoGrad:
        """Stand-in for `torch.no_grad()`; counts how often it was entered.

        The phase-1 encode and the standalone VAE decode both run OUTSIDE
        `pipeline.__call__`, which is where diffusers puts the decorator, so they
        have to open it themselves — the fake records that they do.
        """

        entered = 0

        def __enter__(self) -> "_NoGrad":
            type(self).entered += 1
            return self

        def __exit__(self, *_exc: object) -> bool:
            return False

    def zeros(shape: tuple[int, ...], **kwargs: object) -> "_FakeWarmupLatents":
        """`torch.zeros` stand-in: only `_warmup_vae_decode` builds a tensor here."""
        return _FakeWarmupLatents(shape, **kwargs)

    nn = types.ModuleType("torch.nn")
    nn.Module = Module
    fake_torch = types.ModuleType("torch")
    fake_torch.nn = nn
    fake_torch.bfloat16 = "bfloat16"
    fake_torch.float16 = "float16"
    fake_torch.device = _FakeDevice
    fake_torch.Generator = Generator
    fake_torch.OutOfMemoryError = _FakeOutOfMemoryError
    fake_torch.no_grad = _NoGrad
    fake_torch.zeros = zeros
    return fake_torch


class _FakeWarmupLatents:
    """The tensor `_warmup_vae_decode` synthesizes, tagged so the fake VAE knows it.

    The distinction matters: a warm-up decode and the run's real decode are the
    same call on the same object, and the tests have to be able to tell an OOM
    armed for one from an OOM hit by the other.
    """

    is_warmup = True

    def __init__(self, shape: tuple[int, ...], **kwargs: object) -> None:
        self.shape = tuple(shape)
        self.kwargs = kwargs


class _FakeLatents:
    """Stand-in for the latent tensor handed back by `output_type="latent"`.

    `shape` is real, because the decode's tiling gate reads it: diffusers enters
    the tiled path only when a LATENT side exceeds `tile_latent_min_size`, so a
    shapeless stand-in cannot express "tiling engages" at all. The default is a
    64x64 latent — a 512 px region on the fixture VAE, below its 128-cell
    threshold — which is the common case the ladder must stop crediting tiling
    for.
    """

    def __init__(self, shape: tuple[int, ...] = (1, 16, 64, 64)) -> None:
        self.shape = tuple(shape)
        self.moves: list[dict[str, object]] = []
        self.detached = 0

    def detach(self) -> "_FakeLatents":
        self.detached += 1
        return self

    def to(self, *args: object, **kwargs: object) -> "_FakeLatents":
        self.moves.append({"args": args, **kwargs})
        return self


def _make_fake_vae(
    module_cls: type,
    decoded: object,
    *,
    oom_times: int = 0,
    device_type: str = "cpu",
    latent_channels: int | None = 16,
    sample_size: int | None = VAE_SAMPLE_SIZE,
) -> object:
    """VAE stand-in: a real subclass of the fake `nn.Module` that also decodes.

    It must genuinely be an `nn.Module` subclass, because
    `_materialize_components_for_offload` skips anything that is not one.
    `device_type` is where the loader is pretending to have put it, and
    `latent_channels` is one of the `config` fields `_warmup_vae_decode` reads —
    `None` models a VAE that does not carry it, which is the degraded warm-up
    path.

    **The two tile thresholds are DERIVED, exactly as `AutoencoderKLFlux2.__init__`
    derives them**, from `sample_size` and `block_out_channels`; they are not
    independent knobs and must not be set apart from each other, because
    `tiled_decode` slices by the latent one and blends by the sample one.
    `sample_size=None` models a VAE that declares neither threshold, which is the
    case where the decode ladder must refuse to lower them rather than corrupt
    the image.

    Warm-up decodes are counted separately from real ones and never consume
    `oom_times`: an OOM armed for the run's decode must not be spent on the
    64x64 warm-up pass.
    """

    class _FakeVae(module_cls):  # type: ignore[misc, valid-type]
        def __init__(self) -> None:
            super().__init__("vae", ptr=0x1000, device_type=device_type)
            self.decoded = decoded
            self.oom_left = oom_times
            self.decode_calls = 0
            self.warmup_calls = 0
            self.tiling = False
            self.slicing = False
            self.thresholds_at_decode: list[tuple[object, object]] = []
            config: dict[str, object] = {}
            if latent_channels is not None:
                config["latent_channels"] = latent_channels
            if sample_size is not None:
                config["sample_size"] = sample_size
                config["block_out_channels"] = list(VAE_BLOCK_OUT_CHANNELS)
                scale = 2 ** (len(VAE_BLOCK_OUT_CHANNELS) - 1)
                self.tile_sample_min_size = sample_size
                self.tile_latent_min_size = int(sample_size / scale)
            if config:
                self.config = types.SimpleNamespace(**config)

        def decode(self, latents: object, return_dict: bool = True) -> list[object]:
            if getattr(latents, "is_warmup", False):
                self.warmup_calls += 1
                return [self.decoded]
            self.decode_calls += 1
            # The thresholds IN FORCE for this attempt. The last rung of the OOM
            # ladder lowers them for one decode and restores them afterwards, so
            # the values a test reads after the call cannot show what ran.
            self.thresholds_at_decode.append(
                (
                    getattr(self, "tile_latent_min_size", None),
                    getattr(self, "tile_sample_min_size", None),
                )
            )
            if self.oom_left > 0:
                self.oom_left -= 1
                raise _FakeOutOfMemoryError("HIP out of memory. Tried to allocate 2.00 GiB")
            return [self.decoded]

        def enable_tiling(self) -> None:
            self.tiling = True

        def disable_tiling(self) -> None:
            self.tiling = False

        def enable_slicing(self) -> None:
            self.slicing = True

        def disable_slicing(self) -> None:
            self.slicing = False

    return _FakeVae()


class _FakeImageProcessor:
    def postprocess(self, image: object, output_type: str = "pil") -> list[object]:
        assert output_type == "pil"
        return [image]


def _fake_pipe(
    module_cls: type,
    decoded: object,
    *,
    oom_times: int = 0,
    sample_size: int | None = VAE_SAMPLE_SIZE,
) -> types.SimpleNamespace:
    """A pipeline stand-in with just the surface the decode step touches."""
    return types.SimpleNamespace(
        vae=_make_fake_vae(module_cls, decoded, oom_times=oom_times, sample_size=sample_size),
        transformer=module_cls("transformer", ptr=0x4000, device_type="cuda"),
        text_encoder=module_cls("text_encoder", ptr=0x2000),
        image_processor=_FakeImageProcessor(),
    )


# ---------------------------------------------------------------------------
# Pipeline placement
# ---------------------------------------------------------------------------
class _PlacementFixture(_TempTreeCase):
    """Fake torch/diffusers/transformers plus a service, shared by the two cases below.

    Carries no tests of its own: the transformer layout (folder vs. single file)
    is what the two subclasses differ in, and each loader shape needs its own
    assertions.
    """

    def setUp(self) -> None:
        super().setUp()
        self.recorder = _PatchRecorder()
        self.torch = _install_fake_torch(self.recorder)
        module_cls = self.torch.nn.Module
        self.load_kwargs: dict[str, dict[str, object]] = {}
        load_kwargs = self.load_kwargs
        #: How often each component loader was entered. `load_kwargs` only keeps
        #: the LAST call, so a cache that must read a component once per
        #: directory rather than once per use is not assertable without a count.
        self.load_calls: dict[str, int] = {}
        load_calls = self.load_calls

        class _Loader:
            """Records the kwargs a component loader was called with.

            The two entry points place the produced module the way the real
            loaders do, because that difference IS the failure this fixture must
            be able to reproduce: `from_pretrained` honours `device_map`, while
            diffusers 0.39's `from_single_file` pops `device_map`, discards it,
            and looks only at its own `device` kwarg — defaulting to the CPU.
            """

            def __init__(self, name: str, factory) -> None:
                self.name = name
                self.factory = factory

            def from_pretrained(self, path: str, **kwargs: object) -> object:
                load_kwargs[self.name] = {"path": path, **kwargs}
                load_calls[self.name] = load_calls.get(self.name, 0) + 1
                device_map = kwargs.get("device_map")
                target = device_map.get("") if isinstance(device_map, dict) else None
                return self.factory(str(target) if target else "cpu")

            def from_single_file(self, path: str, **kwargs: object) -> object:
                load_kwargs[f"{self.name}_single"] = {"path": path, **kwargs}
                load_calls[self.name] = load_calls.get(self.name, 0) + 1
                # `device_map` is deliberately NOT consulted here.
                device = kwargs.get("device")
                return self.factory(str(device) if device else "cpu")

        encode_calls = self.encode_calls = []

        class FakePipeline:
            def __init__(self, **components: object) -> None:
                self.__dict__.update(components)
                self.moves: list[tuple[object, int]] = []
                self.offload_devices: list[str] = []
                self.sequential_offload_devices: list[str] = []
                self.progress_bar_disabled = False

            def to(self, device: object) -> "FakePipeline":
                self.moves.append((device, self.recorder_depth()))
                return self

            @staticmethod
            def recorder_depth() -> int:
                return recorder.depth

            def set_progress_bar_config(self, **_kwargs: object) -> None:
                self.progress_bar_disabled = True

            def encode_prompt(self, **kwargs: object) -> tuple[object, object]:
                """Phase 1 builds an encoder-only instance of this class."""
                encode_calls.append(kwargs)
                return _FakeEmbeds(str(kwargs.get("prompt", ""))), object()

            def enable_model_cpu_offload(self, device: str) -> None:
                self.offload_devices.append(device)

            def enable_sequential_cpu_offload(self, device: str) -> None:
                self.sequential_offload_devices.append(device)

        recorder = self.recorder
        self.pipeline_cls = FakePipeline

        diffusers = types.ModuleType("diffusers")
        diffusers.Flux2Transformer2DModel = _Loader(
            "transformer",
            lambda device: module_cls("transformer", ptr=0x4000, device_type=device),
        )
        diffusers.AutoencoderKLFlux2 = _Loader(
            "vae", lambda device: _make_fake_vae(module_cls, object(), device_type=device)
        )
        diffusers.FlowMatchEulerDiscreteScheduler = _Loader("scheduler", lambda _device: object())
        diffusers.Flux2KleinInpaintPipeline = FakePipeline

        transformers = types.ModuleType("transformers")
        # `Qwen3Model` is what the service loads (the pipeline reads hidden states
        # only). `Qwen3ForCausalLM` stays on the double because the real module
        # still exports it and a test may want to prove we do NOT ask for it.
        transformers.Qwen3Model = _Loader(
            "text_encoder",
            lambda device: module_cls("text_encoder", ptr=0x2000, device_type=device),
        )
        transformers.Qwen3ForCausalLM = _Loader(
            "text_encoder_causal_lm",
            lambda device: module_cls("text_encoder", ptr=0x2000, device_type=device),
        )
        transformers.Qwen2TokenizerFast = _Loader("tokenizer", lambda _device: object())

        modules_patch = patch.dict(
            sys.modules,
            {
                "torch": self.torch,
                "torch.nn": self.torch.nn,
                "diffusers": diffusers,
                "transformers": transformers,
            },
        )
        modules_patch.start()
        self.addCleanup(modules_patch.stop)

        for module, name, replacement in (
            (pipeline, "patched_module_to", self.recorder),
            (hardware, "_resolve_selected_backend_device", lambda _fallback: "cuda:0"),
            (hardware, "_clear_torch_cache", lambda: None),
        ):
            attr_patch = patch.object(module, name, replacement)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)

        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())

    def _build(self, model_key: str = "flux2_klein:test", **overrides: object):
        normalized = svc.normalize_flux2_klein_params(self.params(**overrides))
        return self.service._ensure_pipeline_locked(
            normalized, model_key, lambda _step, _label: None, region_hw=(128, 128)
        )

    def _encode(self, **overrides: object) -> dict[str, object]:
        """Drive phase 1 (`_prompt_embeds_locked`) with a no-op progress reporter."""
        normalized = svc.normalize_flux2_klein_params(self.params(**overrides))
        return self.service._prompt_embeds_locked(normalized, lambda _step, _label: None)

class _ResidencyModule:
    """Weight-bearing module stand-in for `_module_residency`.

    Only the surface the probe reads: `parameters()`, `buffers()` and, when
    `hooked` is set, the `_hf_hook` attribute accelerate leaves behind.
    """

    def __init__(
        self,
        *parameter_devices: str,
        buffer_devices: tuple[str, ...] = (),
        hooked: bool = False,
    ) -> None:
        self._parameters = [_ResidencyModule._tensor(name) for name in parameter_devices]
        self._buffers = [_ResidencyModule._tensor(name) for name in buffer_devices]
        if hooked:
            self._hf_hook = object()

    @staticmethod
    def _tensor(device_type: str) -> types.SimpleNamespace:
        return types.SimpleNamespace(device=types.SimpleNamespace(type=device_type))

    def parameters(self) -> list[types.SimpleNamespace]:
        return list(self._parameters)

    def buffers(self) -> list[types.SimpleNamespace]:
        return list(self._buffers)
