"""
File: modules/ai_backend/inpaint/test_flux2_klein.py

Purpose:
Unit tests for the FLUX.2 klein region-editing service: parameter and region
validation, tokenizer/scheduler discovery, checkpoint inspection, the four
placement paths, the split denoise/decode step with its out-of-memory recovery,
and the post-processing contract that pixels outside the mask come back
byte-identical.

Main responsibilities:
- verify every enum/path error and every numeric clamp of the wire contract,
  including the placement-dependent default of `unload_transformer_before_vae`;
- verify a region that the pipeline would silently resize or crop is refused
  with concrete numbers instead;
- verify each placement moves exactly the components it promises, inside the
  ROCm staging patch, and that `low_cpu_mem_usage` loads straight to the device;
- verify the transformer is parked off the GPU before the VAE decode and moved
  back afterwards, and that an OOM in the decode is recovered from without
  repeating the denoise;
- verify the composite leaves every pixel outside the mask untouched and the
  color match aligns the generated region to the ring around it;
- verify the memory forecast treats denoise and decode as two peaks;
- verify a single-file transformer is loaded with the local `transformer/
  config.json` and refused with instructions when there is none, so the gated
  `flux-2-dev` config is never fetched;
- verify the reported device is the one that will actually be used, never a
  placeholder `cpu` chosen because nothing is loaded yet;
- verify the prompt-cache library: name sanitization (nothing composes a path
  outside `prompt_cache/`), the encoder fingerprint and the family name it feeds,
  the directory layout of a save, a listing that survives a corrupt file, every
  refusal a `.msprompt` load owes the user (foreign encoder, other sequence
  length, other dtype, other fp8, foreign container, newer version), the name
  collision rule, an import filed under the family recorded IN THE FILE, and a
  `build` that encodes without building a pipeline and lets the encoder go.

Notes:
- Fake `torch`, `diffusers` and `transformers` modules are injected into
  `sys.modules`, so the tests need neither package, nor the klein weights, nor a
  GPU. `numpy` and `Pillow` are real — the post-processing contract is about
  actual pixel values. The one exception is `PromptCacheRoundTripTests`, which
  needs REAL torch because `safetensors.torch` is what writes and reads the
  embedding tensor; it skips itself where torch is absent, and everything else
  about the file (metadata, layout, refusals) is covered without it, because a
  `.msprompt` is validated from its safetensors HEADER alone.
- Whether the staging patch does anything on the current host is
  `rocm_mmap_transfer`'s own contract; what is pinned here is how the service
  drives it.
"""

from __future__ import annotations

import builtins
import importlib.util
import json
import struct
import sys
import tempfile
import types
import unittest
from pathlib import Path
from typing import Any
from unittest.mock import patch

import numpy as np
from PIL import Image

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.runtime.model_manager import LoadedModelManager


# ---------------------------------------------------------------------------
# Fixtures shared by several test classes
# ---------------------------------------------------------------------------
def _make_model_tree(root: Path, *, single_file_transformer: bool = False) -> dict[str, str]:
    """Lay out a klein-like checkout and return the three user-supplied paths."""
    (root / "text_encoder").mkdir(parents=True)
    (root / "text_encoder" / "config.json").write_text("{}", encoding="utf-8")
    (root / "text_encoder" / "model.safetensors").write_bytes(b"\x00" * 2048)
    (root / "vae").mkdir()
    (root / "vae" / "config.json").write_text("{}", encoding="utf-8")
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


def _png_bytes(array: np.ndarray, mode: str) -> bytes:
    import io

    with io.BytesIO() as buffer:
        Image.fromarray(array, mode).save(buffer, format="PNG")
        return buffer.getvalue()


class _TempTreeCase(unittest.TestCase):
    """Base class giving every test a throwaway klein-like model tree."""

    single_file_transformer = False

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.paths = _make_model_tree(
            self.root, single_file_transformer=self.single_file_transformer
        )

    def params(self, **overrides: object) -> dict[str, object]:
        merged: dict[str, object] = dict(self.paths)
        merged.update(overrides)
        return merged


# ---------------------------------------------------------------------------
# Parameter normalization
# ---------------------------------------------------------------------------
class NormalizeParamsTests(_TempTreeCase):
    def test_missing_path_raises(self) -> None:
        params = self.params()
        del params["vae_path"]
        with self.assertRaises(ValueError) as caught:
            svc.normalize_flux2_klein_params(params)
        self.assertIn("vae_path", str(caught.exception))

    def test_absent_path_raises(self) -> None:
        with self.assertRaises(ValueError) as caught:
            svc.normalize_flux2_klein_params(self.params(vae_path=str(self.root / "nope")))
        self.assertIn("не найден", str(caught.exception))

    def test_unknown_placement_raises(self) -> None:
        with self.assertRaises(ValueError):
            svc.normalize_flux2_klein_params(self.params(placement="teleport"))

    def test_unknown_dtype_raises(self) -> None:
        with self.assertRaises(ValueError):
            svc.normalize_flux2_klein_params(self.params(dtype="float64"))

    def test_defaults(self) -> None:
        out = svc.normalize_flux2_klein_params(self.params())
        self.assertEqual(out["steps"], 4)
        self.assertAlmostEqual(out["guidance_scale"], 1.0)
        self.assertAlmostEqual(out["strength"], 1.0)
        self.assertEqual(out["placement"], "full_gpu")
        self.assertEqual(out["dtype"], "bfloat16")
        self.assertEqual(out["mask_dilate_px"], 16)
        self.assertEqual(out["mask_feather_px"], 12)
        self.assertEqual(out["max_sequence_length"], 512)
        self.assertTrue(out["color_match"])
        self.assertFalse(out["whole_region"])
        self.assertFalse(out["unload_text_encoder_after_encode"])
        self.assertFalse(out["text_encoder_fp8"])
        self.assertIsNone(out["seed"])

    def test_every_key_of_the_wire_contract_is_present(self) -> None:
        # `normalize_flux2_klein_params` is what the rest of the module reads, so
        # a key it forgets is an unguarded `KeyError` deep inside a run rather
        # than a request error at the boundary.
        self.assertEqual(
            set(svc.normalize_flux2_klein_params(self.params())),
            {
                "text_encoder_path",
                "transformer_path",
                "vae_path",
                "prompt",
                "steps",
                "guidance_scale",
                "strength",
                "seed",
                "placement",
                "dtype",
                "low_cpu_mem_usage",
                "vae_tiling",
                "vae_slicing",
                "unload_transformer_before_vae",
                "unload_text_encoder_after_encode",
                "text_encoder_fp8",
                "mask_dilate_px",
                "mask_feather_px",
                "color_match",
                "whole_region",
                "max_sequence_length",
            },
        )

    def test_numeric_clamping(self) -> None:
        out = svc.normalize_flux2_klein_params(
            self.params(
                steps=9999,
                guidance_scale=-3.0,
                strength=0.0,
                mask_dilate_px=999,
                mask_feather_px=-4,
                max_sequence_length=4096,
            )
        )
        self.assertEqual(out["steps"], 50)
        self.assertAlmostEqual(out["guidance_scale"], 1.0)
        self.assertAlmostEqual(out["strength"], 0.25)
        self.assertEqual(out["mask_dilate_px"], 64)
        self.assertEqual(out["mask_feather_px"], 0)
        self.assertEqual(out["max_sequence_length"], 512)

    def test_unload_before_vae_default_depends_on_placement(self) -> None:
        # The VAE peak lands on top of the resident transformer everywhere except
        # `full_gpu`, whose whole point is that nothing leaves the GPU.
        self.assertFalse(
            svc.normalize_flux2_klein_params(self.params(placement="full_gpu"))[
                "unload_transformer_before_vae"
            ]
        )
        for placement in ("encoder_cpu", "model_cpu_offload", "sequential_cpu_offload"):
            with self.subTest(placement=placement):
                self.assertTrue(
                    svc.normalize_flux2_klein_params(self.params(placement=placement))[
                        "unload_transformer_before_vae"
                    ]
                )

    def test_explicit_unload_flag_wins_over_the_default(self) -> None:
        out = svc.normalize_flux2_klein_params(
            self.params(placement="full_gpu", unload_transformer_before_vae=True)
        )
        self.assertTrue(out["unload_transformer_before_vae"])

    def test_seed_is_kept_and_null_means_random(self) -> None:
        self.assertEqual(svc.normalize_flux2_klein_params(self.params(seed=7))["seed"], 7)
        self.assertIsNone(svc.normalize_flux2_klein_params(self.params(seed=None))["seed"])


# ---------------------------------------------------------------------------
# Region validation
# ---------------------------------------------------------------------------
class RegionValidationTests(unittest.TestCase):
    def test_accepts_a_valid_region(self) -> None:
        svc.validate_region_size(512, 512)

    def test_rejects_a_non_multiple_of_16(self) -> None:
        with self.assertRaises(ValueError) as caught:
            svc.validate_region_size(500, 512)
        self.assertIn("496", str(caught.exception))

    def test_rejects_a_too_small_side(self) -> None:
        with self.assertRaises(ValueError):
            svc.validate_region_size(112, 512)

    def test_rejects_too_large_an_area(self) -> None:
        with self.assertRaises(ValueError) as caught:
            svc.validate_region_size(1536, 1024)
        self.assertIn("1048576", str(caught.exception))

    def test_rejects_an_extreme_aspect_ratio(self) -> None:
        with self.assertRaises(ValueError) as caught:
            svc.validate_region_size(1280, 128)
        self.assertIn("Соотношение сторон", str(caught.exception))

    def test_accepts_exactly_eight_to_one(self) -> None:
        svc.validate_region_size(1024, 128)


# ---------------------------------------------------------------------------
# Tokenizer / scheduler discovery
# ---------------------------------------------------------------------------
class ComponentDiscoveryTests(_TempTreeCase):
    def test_finds_tokenizer_and_scheduler_next_to_the_encoder(self) -> None:
        roots = svc.component_search_roots(self.paths)
        tokenizer = svc.discover_component_dir(roots, "tokenizer", svc._TOKENIZER_MARKERS)
        scheduler = svc.discover_component_dir(roots, "scheduler", ("scheduler_config.json",))
        self.assertEqual(tokenizer, self.root / "tokenizer")
        self.assertEqual(scheduler, self.root / "scheduler")

    def test_a_transformers_style_encoder_folder_is_its_own_tokenizer(self) -> None:
        (self.root / "tokenizer" / "tokenizer_config.json").unlink()
        (self.root / "tokenizer").rmdir()
        (self.root / "text_encoder" / "tokenizer.json").write_text("{}", encoding="utf-8")
        roots = svc.component_search_roots(self.paths)
        self.assertEqual(
            svc.discover_component_dir(roots, "tokenizer", svc._TOKENIZER_MARKERS),
            self.root / "text_encoder",
        )

    def test_missing_component_raises_with_the_searched_roots(self) -> None:
        (self.root / "scheduler" / "scheduler_config.json").unlink()
        roots = svc.component_search_roots(self.paths)
        with self.assertRaises(FileNotFoundError) as caught:
            svc._require_component_dir(roots, "scheduler", ("scheduler_config.json",), "планировщик")
        message = str(caught.exception)
        self.assertIn("scheduler_config.json", message)
        self.assertIn(str(self.root), message)


# ---------------------------------------------------------------------------
# Checkpoint inspection
# ---------------------------------------------------------------------------
class ComponentDirNormalizationTests(_TempTreeCase):
    """A weights file standing in for its folder, the way users actually pick it.

    `AutoencoderKLFlux2` and the Qwen3 encoder have no single-file loader, so
    selecting `diffusion_pytorch_model.safetensors` used to fail with the
    loader's own class list. The folder holding a file IS the component.
    """

    def test_file_beside_a_config_resolves_to_its_folder(self) -> None:
        vae_dir = Path(self.paths["vae_path"])
        weights = vae_dir / "diffusion_pytorch_model.safetensors"
        weights.write_bytes(b"")
        self.assertEqual(svc.component_dir_for_path(weights), vae_dir)

    def test_file_without_a_sibling_config_is_left_alone(self) -> None:
        lonely = self.root / "loose" / "diffusion_pytorch_model.safetensors"
        lonely.parent.mkdir(parents=True)
        lonely.write_bytes(b"")
        self.assertEqual(svc.component_dir_for_path(lonely), lonely)

    def test_a_folder_is_returned_unchanged(self) -> None:
        vae_dir = Path(self.paths["vae_path"])
        self.assertEqual(svc.component_dir_for_path(vae_dir), vae_dir)

    def test_vae_loader_accepts_a_file_inside_the_folder(self) -> None:
        vae_dir = Path(self.paths["vae_path"])
        weights = vae_dir / "diffusion_pytorch_model.safetensors"
        weights.write_bytes(b"")
        seen: list[str] = []

        class _Vae:
            @staticmethod
            def from_pretrained(path: str, **_kwargs: object) -> str:
                seen.append(path)
                return "vae"

            @staticmethod
            def from_single_file(path: str, **_kwargs: object) -> str:
                raise AssertionError(f"single-file loader must not be reached for {path}")

        loaded = svc._load_vae(
            _Vae, str(weights), dtype=None, device_map=None, low_cpu_mem_usage=True
        )
        self.assertEqual(loaded, "vae")
        self.assertEqual(seen, [str(vae_dir)])

    def test_text_encoder_accepts_a_file_inside_the_folder(self) -> None:
        encoder_dir = Path(self.paths["text_encoder_path"])
        weights = encoder_dir / "model.safetensors"
        weights.write_bytes(b"")
        seen: list[str] = []

        class _Encoder:
            @staticmethod
            def from_pretrained(path: str, **_kwargs: object) -> str:
                seen.append(path)
                return "encoder"

        loaded = svc._load_text_encoder(
            _Encoder, str(weights), dtype=None, device_map=None, low_cpu_mem_usage=True
        )
        self.assertEqual(loaded, "encoder")
        self.assertEqual(seen, [str(encoder_dir)])


class SafetensorsInspectionTests(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)

    def test_reads_the_header(self) -> None:
        path = self.root / "model.safetensors"
        _write_safetensors(path, {"a.weight": {"dtype": "BF16", "shape": [2, 2]}})
        self.assertIn("a.weight", svc.read_safetensors_header(path))

    def test_rejects_a_non_safetensors_file(self) -> None:
        path = self.root / "not.safetensors"
        path.write_bytes(b"nope")
        with self.assertRaises(ValueError):
            svc.read_safetensors_header(path)

    def test_detects_fp8_scaled_by_scale_tensors(self) -> None:
        header = {"blocks.0.weight": {"dtype": "BF16"}, "blocks.0.weight_scale": {"dtype": "F32"}}
        self.assertTrue(svc.is_fp8_scaled_checkpoint(header))

    def test_detects_fp8_scaled_by_dtype(self) -> None:
        self.assertTrue(svc.is_fp8_scaled_checkpoint({"w": {"dtype": "F8_E4M3"}}))

    def test_a_plain_bf16_checkpoint_is_not_fp8(self) -> None:
        self.assertFalse(
            svc.is_fp8_scaled_checkpoint(
                {"__metadata__": {"format": "pt"}, "w": {"dtype": "BF16"}}
            )
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
    """Stand-in for the latent tensor handed back by `output_type="latent"`."""

    def __init__(self) -> None:
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
) -> object:
    """VAE stand-in: a real subclass of the fake `nn.Module` that also decodes.

    It must genuinely be an `nn.Module` subclass, because
    `_materialize_components_for_offload` skips anything that is not one.
    `device_type` is where the loader is pretending to have put it, and
    `latent_channels` is the one `config` field `_warmup_vae_decode` reads —
    `None` models a VAE that does not carry it, which is the degraded warm-up
    path.

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
            if latent_channels is not None:
                self.config = types.SimpleNamespace(latent_channels=latent_channels)

        def decode(self, latents: object, return_dict: bool = True) -> list[object]:
            if getattr(latents, "is_warmup", False):
                self.warmup_calls += 1
                return [self.decoded]
            self.decode_calls += 1
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


def _fake_pipe(module_cls: type, decoded: object, *, oom_times: int = 0) -> types.SimpleNamespace:
    """A pipeline stand-in with just the surface the decode step touches."""
    return types.SimpleNamespace(
        vae=_make_fake_vae(module_cls, decoded, oom_times=oom_times),
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
                device_map = kwargs.get("device_map")
                target = device_map.get("") if isinstance(device_map, dict) else None
                return self.factory(str(target) if target else "cpu")

            def from_single_file(self, path: str, **kwargs: object) -> object:
                load_kwargs[f"{self.name}_single"] = {"path": path, **kwargs}
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
        transformers.Qwen3ForCausalLM = _Loader(
            "text_encoder",
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

        for name, replacement in (
            ("patched_module_to", self.recorder),
            ("_resolve_selected_backend_device", lambda _fallback: "cuda:0"),
            ("_clear_torch_cache", lambda: None),
        ):
            attr_patch = patch.object(svc, name, replacement)
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


class PipelinePlacementTests(_PlacementFixture):
    """Cover which placement path `_ensure_pipeline_locked` takes (transformer folder)."""

    def test_full_gpu_moves_the_pipeline_inside_the_staging_patch(self) -> None:
        pipe = self._build(placement="full_gpu")
        self.assertEqual([depth for _dev, depth in pipe.moves], [1])
        self.assertEqual(pipe.offload_devices, [])
        self.assertEqual(self.recorder.depth, 0)
        self.assertNotIn("device_map", self.load_kwargs["transformer"])

    def test_low_cpu_mem_usage_loads_straight_onto_the_device(self) -> None:
        pipe = self._build(placement="full_gpu", low_cpu_mem_usage=True)
        for component in ("transformer", "vae"):
            with self.subTest(component=component):
                self.assertEqual(self.load_kwargs[component]["device_map"], {"": "cuda:0"})
                self.assertEqual(str(getattr(pipe, component).device), "cuda:0")
        # The placement move still happens, and inside the staging patch: a
        # loader that silently ignores its placement kwarg must not be able to
        # leave a component on the host (see SingleFileTransformerPlacementTests).
        self.assertEqual([depth for _dev, depth in pipe.moves], [1])

    def test_the_pipeline_is_built_without_a_text_encoder(self) -> None:
        # The whole point of the two-phase run: 8B of Qwen3 must never be
        # resident next to the 9B transformer.
        pipe = self._build(placement="full_gpu")
        self.assertIsNone(pipe.text_encoder)
        self.assertNotIn("text_encoder", self.load_kwargs)

    def test_encoder_cpu_moves_only_the_transformer_and_the_vae(self) -> None:
        pipe = self._build(placement="encoder_cpu")
        self.assertEqual(pipe.moves, [])
        self.assertEqual([depth for _dev, depth in pipe.transformer.moves], [1])
        self.assertEqual([depth for _dev, depth in pipe.vae.moves], [1])

    def test_model_cpu_offload_hands_placement_to_accelerate(self) -> None:
        with patch.object(svc, "mmap_staging_required", return_value=False):
            pipe = self._build(placement="model_cpu_offload")
        self.assertEqual(pipe.offload_devices, ["cuda:0"])
        self.assertEqual(pipe.moves, [])

    def test_sequential_cpu_offload_hands_placement_to_accelerate(self) -> None:
        with patch.object(svc, "mmap_staging_required", return_value=False):
            pipe = self._build(placement="sequential_cpu_offload")
        self.assertEqual(pipe.sequential_offload_devices, ["cuda:0"])

    def test_offload_rehomes_the_file_backed_components_first(self) -> None:
        with patch.object(svc, "mmap_staging_required", return_value=True):
            with patch.object(svc, "tensor_needs_staging", return_value=True):
                pipe = self._build(placement="model_cpu_offload")
        # One staged move onto the GPU (patch depth 1) plus the move back that
        # allocates anonymous host memory, per component.
        for component in ("vae", "transformer"):
            with self.subTest(component=component):
                moves = getattr(pipe, component).moves
                self.assertEqual([depth for _dev, depth in moves], [1, 0])
        self.assertEqual(self.recorder.depth, 0)

    def test_the_transformer_is_rehomed_last(self) -> None:
        # It is the largest component, so a failed (OOM) round trip of it must
        # still leave the smaller one re-homed. The text encoder is not in the
        # list at all any more: it is not part of the pipeline.
        self.assertEqual(svc._MMAP_BACKED_COMPONENTS, ("vae", "transformer"))

    def test_vae_options_are_applied_and_reapplied_on_a_cache_hit(self) -> None:
        pipe = self._build(placement="full_gpu", vae_tiling=True, vae_slicing=False)
        self.assertTrue(pipe.vae.tiling)
        self.assertFalse(pipe.vae.slicing)
        same = self._build(placement="full_gpu", vae_tiling=False, vae_slicing=True)
        self.assertIs(same, pipe)
        self.assertFalse(pipe.vae.tiling)
        self.assertTrue(pipe.vae.slicing)

    def test_a_key_change_rebuilds_and_reports_the_previous_key(self) -> None:
        self._build("flux2_klein:a")
        with patch.object(self.service._model_manager, "mark_unloaded") as unloaded:
            self._build("flux2_klein:b")
        unloaded.assert_called_once_with("flux2_klein:a")
        self.assertEqual(self.service._active_key, "flux2_klein:b")

    def test_an_offload_placement_without_a_gpu_is_refused(self) -> None:
        with patch.object(svc, "_resolve_selected_backend_device", lambda _f: "cpu"):
            with self.assertRaises(RuntimeError) as caught:
                self._build(placement="model_cpu_offload")
        self.assertIn("GPU", str(caught.exception))

    def test_a_missing_scheduler_is_a_named_error(self) -> None:
        (self.root / "scheduler" / "scheduler_config.json").unlink()
        with self.assertRaises(FileNotFoundError) as caught:
            self._build()
        self.assertIn("scheduler", str(caught.exception))

    def test_load_progress_is_reported_for_every_component(self) -> None:
        frames: list[tuple[str, int, int, str]] = []
        normalized = svc.normalize_flux2_klein_params(self.params())
        report = svc._progress_reporter(
            lambda *frame: frames.append(frame), "load", svc.LOAD_PHASE_STEPS
        )
        self.service._ensure_pipeline_locked(
            normalized, "flux2_klein:progress", report, region_hw=(128, 128)
        )
        self.assertTrue(all(phase == "load" for phase, _s, _t, _l in frames))
        # The pipeline is now the FIRST phase, so it owns steps 1-5; step 0 is
        # the caller's "preparing", 6 the warm-up and 7-9 the prompt phase.
        self.assertEqual(
            [step for _p, step, _t, _l in frames],
            [
                svc.LOAD_STEP_TRANSFORMER,
                svc.LOAD_STEP_TOKENIZER,
                svc.LOAD_STEP_VAE,
                svc.LOAD_STEP_SCHEDULER,
                svc.LOAD_STEP_PLACEMENT,
            ],
        )
        self.assertEqual([step for _p, step, _t, _l in frames], [1, 2, 3, 4, 5])
        self.assertTrue(all(total == svc.LOAD_PHASE_STEPS for _p, _s, total, _l in frames))

    def test_the_load_steps_are_reported_in_the_order_the_run_uses_them(self) -> None:
        # The step numbers ARE the order the user sees, so they must be strictly
        # increasing along the sequence a run performs: pipeline, warm-up, then
        # the text encoder.
        order = [
            svc.LOAD_STEP_PREPARE,
            svc.LOAD_STEP_TRANSFORMER,
            svc.LOAD_STEP_TOKENIZER,
            svc.LOAD_STEP_VAE,
            svc.LOAD_STEP_SCHEDULER,
            svc.LOAD_STEP_PLACEMENT,
            svc.LOAD_STEP_WARMUP,
            svc.LOAD_STEP_TEXT_ENCODER,
            svc.LOAD_STEP_ENCODE,
            svc.LOAD_STEP_ENCODER_DONE,
        ]
        self.assertEqual(order, sorted(order))
        self.assertEqual(len(set(order)), len(order))
        self.assertEqual(order[-1], svc.LOAD_PHASE_STEPS)
        # The encoder comes AFTER the transformer is placed — that is the whole
        # point of the new order.
        self.assertGreater(svc.LOAD_STEP_TEXT_ENCODER, svc.LOAD_STEP_PLACEMENT)


class PromptEncodingTests(_PlacementFixture):
    """The prompt phase: the encoder is loaded once and used once.

    It now runs AFTER the transformer has been placed on the accelerator, always
    in host memory, and by default stays there for the next prompt. Everything
    below pins that.
    """

    def test_encoding_loads_the_encoder_and_releases_it(self) -> None:
        embeds = self._encode(
            placement="encoder_cpu", prompt="a cat", unload_text_encoder_after_encode=True
        )
        self.assertIn("text_encoder", self.load_kwargs)
        self.assertEqual([call["prompt"] for call in self.encode_calls], ["a cat"])
        # Released when asked: nothing of the encoder survives the phase.
        self.assertIsNone(self.service._text_encoder)
        self.assertIsNotNone(embeds["prompt"])
        self.assertIsNone(embeds["negative"])

    def test_the_encode_runs_under_no_grad(self) -> None:
        # Same reason as the decode: phase 1 does not go through
        # `pipeline.__call__`, and an 8B forward that builds an autograd graph
        # both wastes memory and poisons the cache with grad-carrying tensors.
        before = self.torch.no_grad.entered
        self._encode(placement="encoder_cpu", prompt="a cat")
        self.assertGreater(self.torch.no_grad.entered, before)

    def test_the_encoder_stays_by_default_in_every_placement(self) -> None:
        for placement in svc.VALID_PLACEMENTS:
            with self.subTest(placement=placement):
                self.setUp()
                self._encode(placement=placement, prompt="a cat")
                self.assertIsNotNone(self.service._text_encoder)

    def test_the_encoder_always_encodes_in_host_memory(self) -> None:
        # Under the new load order the transformer is already on the card when
        # this phase runs, so the encoder can no longer join it there — 18.3 GB
        # plus 16.4 GB does not fit on the 34.2 GB reference card. It also never
        # asks the loader for a `device_map`, which is the load-straight-into-VRAM
        # path.
        for placement in svc.VALID_PLACEMENTS:
            with self.subTest(placement=placement):
                self.setUp()
                self._encode(placement=placement, prompt="a cat")
                self.assertEqual(str(self.encode_calls[-1]["device"]), "cpu")
                self.assertNotIn("device_map", self.load_kwargs["text_encoder"])

    def test_a_cached_prompt_does_not_load_the_encoder(self) -> None:
        self._encode(placement="encoder_cpu", prompt="a cat")
        self.load_kwargs.clear()
        self.encode_calls.clear()
        self._encode(placement="encoder_cpu", prompt="a cat", seed=99, mask_dilate_px=3)
        self.assertEqual(self.load_kwargs, {})
        self.assertEqual(self.encode_calls, [])

    def test_a_different_prompt_is_a_miss(self) -> None:
        self._encode(placement="encoder_cpu", prompt="a cat")
        self.encode_calls.clear()
        self._encode(placement="encoder_cpu", prompt="a dog")
        self.assertEqual([call["prompt"] for call in self.encode_calls], ["a dog"])

    def test_guidance_above_one_also_encodes_the_empty_prompt(self) -> None:
        embeds = self._encode(placement="encoder_cpu", prompt="a cat", guidance_scale=2.0)
        self.assertEqual([call["prompt"] for call in self.encode_calls], ["a cat", ""])
        self.assertIsNotNone(embeds["negative"])

    def test_the_cache_evicts_the_least_recently_used_entry(self) -> None:
        prompts = [f"prompt {index}" for index in range(svc.PROMPT_EMBED_CACHE_ENTRIES + 1)]
        for prompt in prompts:
            self._encode(placement="encoder_cpu", prompt=prompt)
        self.assertEqual(len(self.service._prompt_cache), svc.PROMPT_EMBED_CACHE_ENTRIES)
        self.encode_calls.clear()
        # The oldest one is gone and has to be encoded again; the newest is not.
        self._encode(placement="encoder_cpu", prompt=prompts[-1])
        self.assertEqual(self.encode_calls, [])
        self._encode(placement="encoder_cpu", prompt=prompts[0])
        self.assertEqual([call["prompt"] for call in self.encode_calls], [prompts[0]])

    def test_a_hit_refreshes_the_entry_so_it_is_not_evicted_next(self) -> None:
        prompts = [f"prompt {index}" for index in range(svc.PROMPT_EMBED_CACHE_ENTRIES)]
        for prompt in prompts:
            self._encode(placement="encoder_cpu", prompt=prompt)
        self._encode(placement="encoder_cpu", prompt=prompts[0])  # refresh the oldest
        self._encode(placement="encoder_cpu", prompt="one more")  # evicts prompts[1]
        self.encode_calls.clear()
        self._encode(placement="encoder_cpu", prompt=prompts[0])
        self.assertEqual(self.encode_calls, [])

    def test_unload_keeps_the_prompt_cache_and_drops_the_encoder(self) -> None:
        self._encode(placement="full_gpu", prompt="a cat")
        cached = dict(self.service._prompt_cache)
        self.assertTrue(self.service.unload())
        self.assertIsNone(self.service._text_encoder)
        self.assertEqual(dict(self.service._prompt_cache), cached)

    def test_fp8_is_a_separate_cache_key(self) -> None:
        self._encode(placement="encoder_cpu", prompt="a cat")
        self.encode_calls.clear()
        with patch.object(svc, "_quantize_text_encoder_fp8", return_value=0):
            self._encode(placement="encoder_cpu", prompt="a cat", text_encoder_fp8=True)
        self.assertEqual([call["prompt"] for call in self.encode_calls], ["a cat"])

    def test_fp8_quantizes_the_encoder_before_it_is_used(self) -> None:
        with patch.object(svc, "_quantize_text_encoder_fp8", return_value=1) as quantize:
            self._encode(placement="encoder_cpu", prompt="a cat", text_encoder_fp8=True)
        quantize.assert_called_once()

    def test_fp8_is_off_by_default_everywhere(self) -> None:
        for placement in svc.VALID_PLACEMENTS:
            with self.subTest(placement=placement):
                normalized = svc.normalize_flux2_klein_params(self.params(placement=placement))
                self.assertFalse(normalized["text_encoder_fp8"])

    def test_the_unload_default_is_off_in_every_placement(self) -> None:
        # The reorder moved the encoder behind the transformer's departure from
        # host memory, so keeping it costs RAM nothing else in the run wants and
        # buys an instant prompt change. The default is the same everywhere now,
        # placement included.
        for placement in svc.VALID_PLACEMENTS:
            with self.subTest(placement=placement):
                normalized = svc.normalize_flux2_klein_params(self.params(placement=placement))
                self.assertFalse(normalized["unload_text_encoder_after_encode"])

    def test_an_explicit_unload_request_is_honoured(self) -> None:
        normalized = svc.normalize_flux2_klein_params(
            self.params(placement="full_gpu", unload_text_encoder_after_encode=True)
        )
        self.assertTrue(normalized["unload_text_encoder_after_encode"])

    def test_an_explicit_flag_wins_over_the_placement_default(self) -> None:
        normalized = svc.normalize_flux2_klein_params(
            self.params(placement="encoder_cpu", unload_text_encoder_after_encode=False)
        )
        self.assertFalse(normalized["unload_text_encoder_after_encode"])


class MemoryGuardTests(_PlacementFixture):
    """A request is refused BEFORE the first byte when a phase cannot fit.

    A host-side shortfall is not an exception: the kernel OOM killer picks a
    victim among everything running, and on this project's reference host it
    closed the user's editor while the 9B transformer and the 8B encoder were
    being loaded side by side.
    """

    #: klein's real component sizes, so the tests assert on the figures a user
    #: actually sees in the message.
    TRANSFORMER_BYTES = 18_157_185_168
    TEXT_ENCODER_BYTES = 16_381_516_808
    VAE_BYTES = 168_120_878
    GIB = 1024**3

    def setUp(self) -> None:
        super().setUp()
        sizes = {
            self.paths["transformer_path"]: self.TRANSFORMER_BYTES,
            self.paths["text_encoder_path"]: self.TEXT_ENCODER_BYTES,
            self.paths["vae_path"]: self.VAE_BYTES,
        }
        weights_patch = patch.object(svc, "_weight_bytes", lambda path: sizes.get(path, 0))
        weights_patch.start()
        self.addCleanup(weights_patch.stop)

    def _with_memory(self, *, ram_free: float, vram_free: float) -> None:
        snapshot = {
            "ram_free": int(ram_free * self.GIB),
            "ram_total": int(64 * self.GIB),
            "vram_free": int(vram_free * self.GIB),
            "vram_total": int(32 * self.GIB),
        }
        memory_patch = patch.object(svc, "memory_snapshot", lambda _device=None: snapshot)
        memory_patch.start()
        self.addCleanup(memory_patch.stop)

    def _guard(self, **overrides: object) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params(**overrides))
        self.service._require_headroom_locked(normalized, 128, 128, "flux2_klein:test")

    def test_a_phase_short_of_host_memory_is_refused_before_anything_is_read(self) -> None:
        # 16 GiB of encoder on a host with 12 GiB free: phase 1 alone does not fit.
        self._with_memory(ram_free=12.0, vram_free=31.9)
        with self.assertRaises(RuntimeError) as caught:
            self._guard(placement="encoder_cpu", low_cpu_mem_usage=True)
        message = str(caught.exception)
        self.assertIn("оперативной памяти на этап «кодирование промпта»", message)
        self.assertIn("свободно 12.0 ГиБ", message)
        self.assertEqual(self.load_kwargs, {})
        self.assertIsNone(self.service._pipe)

    def test_a_phase_short_of_device_memory_is_refused_and_names_the_card(self) -> None:
        self._with_memory(ram_free=64.0, vram_free=8.0)
        with self.assertRaises(RuntimeError) as caught:
            self._guard(placement="encoder_cpu", low_cpu_mem_usage=True)
        message = str(caught.exception)
        self.assertIn("видеопамяти на cuda:0 на этап «денойз»", message)
        self.assertIn("свободно 8.0 ГиБ", message)
        self.assertEqual(self.load_kwargs, {})

    def test_the_refusal_names_the_settings_that_do_fit(self) -> None:
        # Enough host memory for the encoder, not enough to also copy the 9B
        # transformer back for the decode: the advice must name the one lever
        # that removes that copy rather than declaring the machine hopeless.
        self._with_memory(ram_free=18.0, vram_free=31.9)
        with self.assertRaises(RuntimeError) as caught:
            self._guard(placement="encoder_cpu", low_cpu_mem_usage=True)
        message = str(caught.exception)
        self.assertIn("Сейчас помещаются", message)
        self.assertIn("без выгрузки трансформера перед VAE", message)

    def test_nothing_fitting_says_so_instead_of_naming_a_preset(self) -> None:
        self._with_memory(ram_free=1.0, vram_free=1.0)
        with self.assertRaises(RuntimeError) as caught:
            self._guard(placement="encoder_cpu")
        self.assertIn("Ни один из встроенных профилей", str(caught.exception))

    def test_enough_memory_passes_the_guard(self) -> None:
        self._with_memory(ram_free=27.0, vram_free=31.9)
        # Both host-heavy levers off, so this exercises the guard rather than the
        # shipped defaults: a resident encoder AND a parked transformer put ~34
        # GiB in host memory at once, which is its own case below.
        self._guard(
            placement="encoder_cpu",
            low_cpu_mem_usage=True,
            unload_text_encoder_after_encode=True,
            unload_transformer_before_vae=False,
        )

    def test_a_resident_encoder_and_a_parked_transformer_share_the_host(self) -> None:
        # The one combination the reorder does NOT make cheaper: parking the 9B
        # transformer for the decode copies it back into host memory, where the
        # kept encoder already sits. They genuinely coexist, so this is a sum and
        # the guard must say so rather than hide it behind a maximum.
        normalized = svc.normalize_flux2_klein_params(
            self.params(
                placement="encoder_cpu",
                low_cpu_mem_usage=True,
                unload_text_encoder_after_encode=False,
                unload_transformer_before_vae=True,
            )
        )
        phases = svc.forecast_memory(normalized, 128, 128)["phases"]
        self.assertEqual(
            phases["decode"]["ram_bytes"], self.TEXT_ENCODER_BYTES + self.TRANSFORMER_BYTES
        )

    def test_unknown_free_memory_never_refuses(self) -> None:
        # `memory_snapshot` reports 0 when psutil or an accelerator is missing;
        # 0 means unknown, not empty, and must not gate a run.
        self._with_memory(ram_free=0.0, vram_free=0.0)
        self._guard(placement="full_gpu")

    def test_a_cached_prompt_and_a_resident_pipeline_are_not_gated(self) -> None:
        self._with_memory(ram_free=27.0, vram_free=31.9)
        settings: dict[str, object] = {"placement": "encoder_cpu", "low_cpu_mem_usage": True}
        self._encode(**settings)
        self._build("flux2_klein:test", **settings)
        # Nothing new is allocated on a double cache hit, so the guard must not
        # fire even if the machine has since run out.
        with patch.object(
            svc,
            "memory_snapshot",
            lambda _device=None: {
                "ram_free": 1, "ram_total": 1, "vram_free": 1, "vram_total": 1
            },
        ):
            self._guard(**settings)

    def test_a_new_prompt_is_not_charged_for_memory_the_service_already_holds(self) -> None:
        # Measured on the reference host: after one run the pipeline is on the
        # card and the encoder is in host memory, so the free figures already
        # exclude both — and the guard refused the next prompt for 17.6 GiB of
        # VRAM that the very pipeline it was about to reuse was occupying.
        settings: dict[str, object] = {
            "placement": "encoder_cpu",
            "low_cpu_mem_usage": True,
            "unload_text_encoder_after_encode": False,
        }
        self._with_memory(ram_free=27.0, vram_free=31.9)
        self._encode(**settings)
        self._build("flux2_klein:test", **settings)
        # What the machine looks like now: our own 16 GiB of encoder and 17 GiB
        # of pipeline are gone from the free figures, because we are holding them.
        with patch.object(
            svc,
            "memory_snapshot",
            lambda _device=None: {
                "ram_free": int(13.0 * self.GIB),
                "ram_total": int(64 * self.GIB),
                "vram_free": int(13.5 * self.GIB),
                "vram_total": int(32 * self.GIB),
            },
        ):
            self._guard(prompt="a different prompt", **settings)

    def test_memory_the_service_does_not_hold_is_still_charged(self) -> None:
        # The discount must not become a blanket exemption: with nothing resident
        # the same figures still refuse the run.
        self._with_memory(ram_free=13.0, vram_free=13.5)
        with self.assertRaises(RuntimeError):
            self._guard(
                placement="encoder_cpu",
                low_cpu_mem_usage=True,
                unload_text_encoder_after_encode=False,
            )

    def test_the_peak_is_the_maximum_of_the_phases_not_their_sum(self) -> None:
        normalized = svc.normalize_flux2_klein_params(
            self.params(placement="full_gpu", low_cpu_mem_usage=True)
        )
        forecast = svc.forecast_memory(normalized, 128, 128)
        phases = forecast["phases"]
        for side in ("vram_bytes", "ram_bytes"):
            with self.subTest(side=side):
                self.assertEqual(
                    forecast[side], max(phase[side] for phase in phases.values())
                )
        self.assertLess(
            forecast["vram_bytes"], sum(phase["vram_bytes"] for phase in phases.values())
        )

    def test_the_encoder_is_forecast_in_host_memory_in_every_placement(self) -> None:
        # It encodes on the host now, in every placement, so its cost is a RAM
        # cost everywhere. The encode phase's VRAM is only the already-placed
        # pipeline sitting idle on the card.
        for placement in svc.VALID_PLACEMENTS:
            with self.subTest(placement=placement):
                normalized = svc.normalize_flux2_klein_params(
                    self.params(placement=placement, low_cpu_mem_usage=True)
                )
                phases = svc.forecast_memory(normalized, 128, 128)["phases"]
                encode = phases["encode"]
                self.assertGreaterEqual(
                    encode["ram_bytes"],
                    self.TEXT_ENCODER_BYTES + svc.ENCODE_ACTIVATION_BYTES,
                )
                # Nothing but the already-placed pipeline sitting idle: the same
                # weights the denoise uses, without the denoise activations.
                activations = 64 * svc.ACTIVATION_BYTES_PER_LATENT_TOKEN
                self.assertEqual(
                    encode["vram_bytes"], phases["denoise"]["vram_bytes"] - activations
                )

    def test_the_two_host_peaks_never_overlap_off_the_offload_placements(self) -> None:
        # The reorder's whole point: the transformer's host copy is gone by the
        # time the 16 GiB encoder arrives, so the two are a maximum and not a sum.
        normalized = svc.normalize_flux2_klein_params(
            self.params(
                placement="encoder_cpu",
                low_cpu_mem_usage=False,
                unload_text_encoder_after_encode=False,
                unload_transformer_before_vae=False,
            )
        )
        forecast = svc.forecast_memory(normalized, 128, 128)
        pipeline_bytes = self.TRANSFORMER_BYTES + self.VAE_BYTES
        self.assertEqual(
            forecast["phases"]["encode"]["ram_bytes"],
            self.TEXT_ENCODER_BYTES + svc.ENCODE_ACTIVATION_BYTES,
        )
        self.assertEqual(
            forecast["phases"]["denoise"]["ram_bytes"],
            max(pipeline_bytes, self.TEXT_ENCODER_BYTES),
        )
        self.assertLess(forecast["ram_bytes"], pipeline_bytes + self.TEXT_ENCODER_BYTES)

    def test_an_offload_placement_keeps_the_pipeline_in_the_host_sum(self) -> None:
        # There the pipeline never leaves host memory, so the same maximum has to
        # degenerate into the sum it really is.
        normalized = svc.normalize_flux2_klein_params(
            self.params(
                placement="sequential_cpu_offload",
                unload_text_encoder_after_encode=False,
            )
        )
        phases = svc.forecast_memory(normalized, 128, 128)["phases"]
        pipeline_bytes = self.TRANSFORMER_BYTES + self.VAE_BYTES
        self.assertEqual(
            phases["encode"]["ram_bytes"],
            self.TEXT_ENCODER_BYTES + svc.ENCODE_ACTIVATION_BYTES + pipeline_bytes,
        )
        self.assertEqual(
            phases["denoise"]["ram_bytes"], pipeline_bytes + self.TEXT_ENCODER_BYTES
        )

    def test_the_denoise_phase_no_longer_carries_the_text_encoder(self) -> None:
        normalized = svc.normalize_flux2_klein_params(
            self.params(
                placement="full_gpu",
                low_cpu_mem_usage=True,
                unload_text_encoder_after_encode=True,
            )
        )
        phases = svc.forecast_memory(normalized, 128, 128)["phases"]
        # Exactly transformer + VAE + the per-token activations of a 128x128
        # region (8x8 latent tokens), with nothing of the encoder left in it.
        activations = 64 * svc.ACTIVATION_BYTES_PER_LATENT_TOKEN
        self.assertEqual(
            phases["denoise"]["vram_bytes"],
            self.TRANSFORMER_BYTES + self.VAE_BYTES + activations,
        )

    def test_a_resident_encoder_is_counted_in_the_later_phases(self) -> None:
        kept = svc.forecast_memory(
            svc.normalize_flux2_klein_params(
                self.params(
                    placement="full_gpu",
                    low_cpu_mem_usage=True,
                    unload_text_encoder_after_encode=False,
                )
            ),
            128,
            128,
        )
        released = svc.forecast_memory(
            svc.normalize_flux2_klein_params(
                self.params(
                    placement="full_gpu",
                    low_cpu_mem_usage=True,
                    unload_text_encoder_after_encode=True,
                )
            ),
            128,
            128,
        )
        # It is counted in HOST memory now: the encoder never goes on the card
        # under the new order, whatever the placement.
        self.assertEqual(
            kept["phases"]["denoise"]["ram_bytes"] - released["phases"]["denoise"]["ram_bytes"],
            self.TEXT_ENCODER_BYTES,
        )
        self.assertEqual(
            kept["phases"]["denoise"]["vram_bytes"],
            released["phases"]["denoise"]["vram_bytes"],
        )

    def test_fp8_halves_the_resident_encoder_but_not_the_encode_peak(self) -> None:
        def forecast(fp8: bool) -> dict[str, object]:
            return svc.forecast_memory(
                svc.normalize_flux2_klein_params(
                    self.params(
                        placement="full_gpu",
                        low_cpu_mem_usage=True,
                        unload_text_encoder_after_encode=False,
                        text_encoder_fp8=fp8,
                    )
                ),
                128,
                128,
            )

        plain, quantized = forecast(False), forecast(True)
        # The load peak is unchanged: the bf16 weights have to exist before they
        # can be quantized.
        self.assertEqual(
            plain["phases"]["encode"]["ram_bytes"], quantized["phases"]["encode"]["ram_bytes"]
        )
        self.assertEqual(
            plain["phases"]["denoise"]["ram_bytes"] - quantized["phases"]["denoise"]["ram_bytes"],
            self.TEXT_ENCODER_BYTES - self.TEXT_ENCODER_BYTES // 2,
        )

    def test_the_guard_and_the_forecast_are_the_same_arithmetic(self) -> None:
        # `estimate` is what the UI shows; the guard must refuse exactly what it
        # reports as not fitting, so both go through `forecast_memory`.
        self._with_memory(ram_free=12.0, vram_free=31.9)
        params = self.params(placement="encoder_cpu", low_cpu_mem_usage=True)
        answer = self.service.estimate(params=params, region_width=128, region_height=128)
        self.assertFalse(answer["fits"])
        normalized = svc.normalize_flux2_klein_params(params)
        forecast = svc.forecast_memory(normalized, 128, 128)
        self.assertEqual(answer["ram_bytes"], forecast["ram_bytes"])
        self.assertEqual(answer["vram_bytes"], forecast["vram_bytes"])
        self.assertEqual(answer["breakdown"], forecast["breakdown"])

    def test_parking_the_transformer_is_counted_as_host_memory(self) -> None:
        # It is a full 9B device->host copy, and that peak is what the OOM
        # killer sees.
        def decode_ram(parked: bool) -> int:
            normalized = svc.normalize_flux2_klein_params(
                self.params(
                    placement="encoder_cpu",
                    low_cpu_mem_usage=True,
                    unload_transformer_before_vae=parked,
                    # Isolated from the resident encoder, which has its own test.
                    unload_text_encoder_after_encode=True,
                )
            )
            return svc.forecast_memory(normalized, 128, 128)["phases"]["decode"]["ram_bytes"]

        self.assertEqual(decode_ram(False), 0)
        self.assertEqual(decode_ram(True), self.TRANSFORMER_BYTES)


class SingleFileTransformerPlacementTests(_PlacementFixture):
    """Regression cover for the «Минимум RAM» device mismatch (2026-09-02).

    `encoder_cpu` + `low_cpu_mem_usage` with a single-FILE transformer used to
    build a pipeline whose transformer sat in host memory while the VAE was on
    the accelerator: diffusers' `from_single_file` accepts `device_map` and
    discards it, and `_apply_placement` skipped its own move because a
    `device_map` had been passed. `DiffusionPipeline.device` then answered `cpu`,
    `_execution_device` followed, and the pipeline's VAE encode of the region
    died with "Input type (CPUBFloat16Type) and weight type (CUDABFloat16Type)".
    """

    single_file_transformer = True

    def test_the_single_file_loader_is_given_device_not_device_map(self) -> None:
        pipe = self._build(placement="encoder_cpu", low_cpu_mem_usage=True)
        kwargs = self.load_kwargs["transformer_single"]
        self.assertEqual(kwargs["device"], "cuda:0")
        self.assertNotIn("device_map", kwargs)
        self.assertEqual(str(pipe.transformer.device), "cuda:0")
        # The encoder is not part of this pipeline at all any more.
        self.assertIsNone(pipe.text_encoder)

    def test_a_single_file_load_without_a_device_map_asks_for_no_device(self) -> None:
        self._build(placement="encoder_cpu")
        self.assertNotIn("device", self.load_kwargs["transformer_single"])

    def test_a_transformer_the_loader_left_on_the_host_is_still_placed(self) -> None:
        # The invariant that makes any future loader quirk harmless: placement
        # does not trust the loader kwargs, it moves what is not already there.
        module_cls = self.torch.nn.Module
        pipe = self.pipeline_cls(
            transformer=module_cls("transformer", ptr=0x4000),
            vae=_make_fake_vae(module_cls, object(), device_type="cuda:0"),
            text_encoder=module_cls("text_encoder", ptr=0x2000),
        )
        svc._apply_placement(pipe, "encoder_cpu", _FakeDevice("cuda:0"))
        self.assertEqual(str(pipe.transformer.device), "cuda:0")
        self.assertEqual([depth for _dev, depth in pipe.transformer.moves], [1])
        self.assertEqual(pipe.text_encoder.moves, [])

    def test_the_translation_only_covers_a_whole_model_device_map(self) -> None:
        self.assertEqual(svc._single_file_device({"": "cuda:0"}), "cuda:0")
        self.assertIsNone(svc._single_file_device(None))
        self.assertIsNone(svc._single_file_device({}))
        # A per-submodule map cannot be expressed as one `device` kwarg.
        self.assertIsNone(svc._single_file_device({"blocks.0": "cuda:0", "blocks.1": "cpu"}))


class ExecutionDeviceProbeTests(unittest.TestCase):
    """A run must not start on a device the pipeline was not placed on."""

    def setUp(self) -> None:
        self.recorder = _PatchRecorder()
        self.torch = _install_fake_torch(self.recorder)
        modules_patch = patch.dict(sys.modules, {"torch": self.torch, "torch.nn": self.torch.nn})
        modules_patch.start()
        self.addCleanup(modules_patch.stop)

    def _pipe(self, transformer_device: str) -> object:
        """A pipeline whose `_execution_device` follows diffusers' own rule.

        With no accelerate hooks the probe degrades to `DiffusionPipeline.device`,
        which is the device of the first component in SORTED signature order that
        is still an `nn.Module`. The encoder is `None` here, as it is in the real
        pipeline after the two-phase split, so the transformer decides — and a
        transformer left on the host makes the whole run execute there.
        """
        module_cls = self.torch.nn.Module

        class _Pipe(types.SimpleNamespace):
            @property
            def _execution_device(self) -> object:
                for name in ("text_encoder", "transformer", "vae"):
                    component = getattr(self, name, None)
                    if component is not None:
                        return component.device
                return _FakeDevice("cpu")

        return _Pipe(
            transformer=module_cls("transformer", ptr=0x4000, device_type=transformer_device),
            vae=_make_fake_vae(module_cls, object(), device_type="cuda:0"),
            text_encoder=None,
        )

    def test_a_transformer_left_on_the_host_is_refused(self) -> None:
        with self.assertRaises(RuntimeError) as caught:
            svc._require_execution_device(self._pipe("cpu"), _FakeDevice("cuda:0"))
        message = str(caught.exception)
        self.assertIn("transformer=cpu", message)
        self.assertIn("vae=cuda:0", message)

    def test_a_correctly_placed_pipeline_passes(self) -> None:
        svc._require_execution_device(self._pipe("cuda:0"), _FakeDevice("cuda:0"))

    def test_a_pipeline_without_the_property_is_not_probed(self) -> None:
        # A test double is not a `DiffusionPipeline`; the tripwire must not turn
        # its absence into a failure.
        svc._require_execution_device(types.SimpleNamespace(), _FakeDevice("cuda:0"))


# ---------------------------------------------------------------------------
# Single-file transformer loading
# ---------------------------------------------------------------------------
class SingleFileTransformerTests(_TempTreeCase):
    single_file_transformer = True

    def setUp(self) -> None:
        super().setUp()
        self.calls: list[dict[str, object]] = []
        calls = self.calls

        class _Model:
            @staticmethod
            def from_single_file(path: str, **kwargs: object) -> str:
                calls.append({"path": path, **kwargs})
                return "transformer"

            @staticmethod
            def from_pretrained(path: str, **kwargs: object) -> str:
                calls.append({"path": path, "pretrained": True, **kwargs})
                return "transformer"

        self.model_cls = _Model

    def test_single_file_forces_guidance_embeds_off(self) -> None:
        svc._load_transformer(
            self.model_cls,
            self.paths["transformer_path"],
            dtype="bfloat16",
            device_map=None,
            low_cpu_mem_usage=False,
        )
        # klein has no `guidance_in` block, and diffusers would otherwise use the
        # flux-2-dev config, which does.
        self.assertIs(self.calls[0]["guidance_embeds"], False)

    def test_the_config_beside_the_checkpoint_is_used_and_the_hub_is_blocked(self) -> None:
        # `<checkpoint dir>/transformer/config.json` is the klein standalone
        # layout. Passing it as `config` is what keeps diffusers from resolving
        # the checkpoint to the gated `black-forest-labs/FLUX.2-dev` repo, and
        # `local_files_only` closes the network path a second time.
        svc._load_transformer(
            self.model_cls,
            self.paths["transformer_path"],
            dtype="bfloat16",
            device_map=None,
            low_cpu_mem_usage=False,
        )
        self.assertEqual(self.calls[0]["config"], str(self.root / "transformer"))
        self.assertIs(self.calls[0]["local_files_only"], True)

    def test_a_checkpoint_inside_the_transformer_folder_is_also_covered(self) -> None:
        # The other legitimate layout: the file sits IN the diffusers folder, so
        # the config is in the checkpoint's own directory rather than a subfolder.
        folder = self.root / "transformer"
        checkpoint = folder / "diffusion_pytorch_model.safetensors"
        _write_safetensors(checkpoint, {"single_stream_modulation.lin.weight": {"dtype": "BF16"}})

        svc._load_transformer(
            self.model_cls,
            str(checkpoint),
            dtype="bfloat16",
            device_map=None,
            low_cpu_mem_usage=False,
        )
        self.assertEqual(self.calls[0]["config"], str(folder))

    def test_a_missing_config_is_refused_with_instructions_and_no_hub_call(self) -> None:
        (self.root / "transformer" / "config.json").unlink()
        (self.root / "transformer").rmdir()

        with self.assertRaises(FileNotFoundError) as caught:
            svc._load_transformer(
                self.model_cls,
                self.paths["transformer_path"],
                dtype="bfloat16",
                device_map=None,
                low_cpu_mem_usage=False,
            )

        message = str(caught.exception)
        # What to do, where exactly, and why nothing is guessed instead.
        self.assertIn(str(self.root / "transformer" / "config.json"), message)
        self.assertIn("rope_theta", message)
        self.assertIn("FLUX.2-dev", message)
        # Every probed directory is named, so the user can see where to look.
        for candidate in svc.component_probe_order(
            svc.transformer_config_roots(Path(self.paths["transformer_path"])), "transformer"
        ):
            self.assertIn(str(candidate), message)
        # Refused before the loader ran: nothing could have reached the Hub.
        self.assertEqual(self.calls, [])

    def test_a_config_of_another_component_is_refused(self) -> None:
        # A VAE config found next to the checkpoint would build a different
        # architecture from the same weights instead of failing.
        (self.root / "transformer" / "config.json").write_text(
            json.dumps({"_class_name": "AutoencoderKLFlux2"}), encoding="utf-8"
        )
        with self.assertRaises(ValueError) as caught:
            svc._load_transformer(
                self.model_cls,
                self.paths["transformer_path"],
                dtype="bfloat16",
                device_map=None,
                low_cpu_mem_usage=False,
            )
        self.assertIn("AutoencoderKLFlux2", str(caught.exception))
        self.assertEqual(self.calls, [])

    def test_an_unreadable_config_is_refused(self) -> None:
        (self.root / "transformer" / "config.json").write_text("{not json", encoding="utf-8")
        with self.assertRaises(ValueError):
            svc._load_transformer(
                self.model_cls,
                self.paths["transformer_path"],
                dtype="bfloat16",
                device_map=None,
                low_cpu_mem_usage=False,
            )
        self.assertEqual(self.calls, [])

    def test_an_fp8_scaled_checkpoint_is_refused_with_a_readable_error(self) -> None:
        _write_safetensors(
            Path(self.paths["transformer_path"]),
            {"blocks.0.weight": {"dtype": "F8_E4M3"}, "blocks.0.weight_scale": {"dtype": "F32"}},
        )
        with self.assertRaises(ValueError) as caught:
            svc._load_transformer(
                self.model_cls,
                self.paths["transformer_path"],
                dtype="bfloat16",
                device_map=None,
                low_cpu_mem_usage=False,
            )
        self.assertIn("fp8_scaled", str(caught.exception))
        self.assertEqual(self.calls, [])

    def test_a_loader_failure_is_propagated_unchanged(self) -> None:
        # With a local config in hand there is no remedy left to suggest, so the
        # loader's own error must not be rewrapped into a different type.
        class _Failing:
            @staticmethod
            def from_single_file(_path: str, **_kwargs: object) -> object:
                raise OSError("checkpoint is truncated")

        with self.assertRaises(OSError) as caught:
            svc._load_transformer(
                _Failing,
                self.paths["transformer_path"],
                dtype="bfloat16",
                device_map=None,
                low_cpu_mem_usage=False,
            )
        self.assertIn("truncated", str(caught.exception))


class DirectoryTransformerTests(_TempTreeCase):
    """A diffusers folder must get the same treatment as a single file."""

    single_file_transformer = False

    def setUp(self) -> None:
        super().setUp()
        self.calls: list[dict[str, object]] = []
        calls = self.calls

        class _Model:
            @staticmethod
            def from_pretrained(path: str, **kwargs: object) -> str:
                calls.append({"path": path, **kwargs})
                return "transformer"

            @staticmethod
            def from_single_file(_path: str, **_kwargs: object) -> object:
                raise AssertionError("a directory must not go through from_single_file")

        self.model_cls = _Model

    def _load(self) -> object:
        return svc._load_transformer(
            self.model_cls,
            self.paths["transformer_path"],
            dtype="bfloat16",
            device_map=None,
            low_cpu_mem_usage=False,
        )

    def test_a_directory_also_forces_guidance_embeds_off(self) -> None:
        # klein has no `guidance_in` block either way: a folder carrying the
        # flux-2-dev value of `guidance_embeds: true` must not build that
        # architecture just because it came from a directory.
        self._load()
        self.assertIs(self.calls[0]["guidance_embeds"], False)

    def test_a_plain_bf16_directory_is_loaded(self) -> None:
        _write_safetensors(
            Path(self.paths["transformer_path"]) / "diffusion_pytorch_model.safetensors",
            {"blocks.0.weight": {"dtype": "BF16"}},
        )
        self.assertEqual(self._load(), "transformer")

    def test_an_fp8_scaled_shard_is_refused_before_the_load(self) -> None:
        folder = Path(self.paths["transformer_path"])
        _write_safetensors(
            folder / "diffusion_pytorch_model-00001-of-00002.safetensors",
            {"blocks.0.weight": {"dtype": "BF16"}},
        )
        _write_safetensors(
            folder / "diffusion_pytorch_model-00002-of-00002.safetensors",
            {"blocks.1.weight": {"dtype": "F8_E4M3"}, "blocks.1.weight_scale": {"dtype": "F32"}},
        )
        (folder / "diffusion_pytorch_model.safetensors.index.json").write_text(
            json.dumps(
                {
                    "weight_map": {
                        "blocks.0.weight": "diffusion_pytorch_model-00001-of-00002.safetensors",
                        "blocks.1.weight": "diffusion_pytorch_model-00002-of-00002.safetensors",
                    }
                }
            ),
            encoding="utf-8",
        )
        (folder / "diffusion_pytorch_model.safetensors").unlink()

        with self.assertRaises(ValueError) as caught:
            self._load()
        self.assertIn("fp8_scaled", str(caught.exception))
        # Refused on the header alone: no multi-GiB load was started.
        self.assertEqual(self.calls, [])

    def test_every_shard_is_enumerated_once_index_or_not(self) -> None:
        folder = Path(self.paths["transformer_path"])
        (folder / "diffusion_pytorch_model.safetensors").unlink()
        for index in (1, 2):
            _write_safetensors(
                folder / f"model-0000{index}-of-00002.safetensors",
                {f"blocks.{index}.weight": {"dtype": "BF16"}},
            )
        _write_safetensors(folder / "extra.safetensors", {"lm_head.weight": {"dtype": "BF16"}})
        (folder / "model.safetensors.index.json").write_text(
            json.dumps({"weight_map": {"blocks.1.weight": "model-00001-of-00002.safetensors"}}),
            encoding="utf-8",
        )
        shards = svc.component_safetensors_shards(folder)
        self.assertEqual(len(shards), len(set(shards)))
        self.assertEqual(
            sorted(shard.name for shard in shards),
            [
                "extra.safetensors",
                "model-00001-of-00002.safetensors",
                "model-00002-of-00002.safetensors",
            ],
        )
        # The index is a hint about ordering, not an extra source of shards.
        self.assertEqual(shards[0].name, "model-00001-of-00002.safetensors")

    def test_a_malformed_index_does_not_break_the_scan(self) -> None:
        folder = Path(self.paths["transformer_path"])
        (folder / "model.safetensors.index.json").write_text("{not json", encoding="utf-8")
        self.assertEqual(
            [shard.name for shard in svc.component_safetensors_shards(folder)],
            ["diffusion_pytorch_model.safetensors"],
        )


# ---------------------------------------------------------------------------
# Post-processing
# ---------------------------------------------------------------------------
class PostProcessingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.original = np.full((64, 64, 3), 100, dtype=np.uint8)
        self.generated = np.full((64, 64, 3), 200, dtype=np.uint8)
        self.mask = np.zeros((64, 64), dtype=np.uint8)
        self.mask[20:44, 20:44] = 255

    def test_pixels_outside_the_mask_are_byte_identical(self) -> None:
        composed = svc._composite_over_region(self.original, self.generated, self.mask, 6)
        outside = self.mask == 0
        self.assertTrue(np.array_equal(composed[outside], self.original[outside]))

    def test_the_centre_of_the_mask_takes_the_generated_pixels(self) -> None:
        composed = svc._composite_over_region(self.original, self.generated, self.mask, 4)
        # The inward feather only fades the rim, so the middle is (nearly) all
        # generated content.
        self.assertGreater(int(composed[31, 31, 0]), 190)

    def test_a_zero_feather_is_a_hard_edge(self) -> None:
        composed = svc._composite_over_region(self.original, self.generated, self.mask, 0)
        inside = self.mask > 0
        self.assertTrue(np.array_equal(composed[inside], self.generated[inside]))

    def test_a_mask_thinner_than_the_feather_is_not_blended_away(self) -> None:
        thin = np.zeros((64, 64), dtype=np.uint8)
        thin[32, 20:44] = 255
        composed = svc._composite_over_region(self.original, self.generated, thin, 12)
        self.assertTrue(np.array_equal(composed[32, 30], self.generated[32, 30]))

    def test_the_feather_reaches_full_strength_at_exactly_feather_px(self) -> None:
        # `mask_feather_px` is the ramp WIDTH: zero weight on the contour, full
        # weight `feather_px` pixels in. The predecessor spread a nominal 6 over
        # ~22 px and never reached 1.0 at all for a wide feather.
        for feather in (4, 6, 12):
            with self.subTest(feather=feather):
                alpha = svc._feather_mask_inwards(self.mask, feather)
                self.assertEqual(int(alpha[self.mask == 0].max()), 0)
                # Row 32 crosses the mask at columns 20..43; column 20+k is k+1 px
                # from the outside, so the ramp is complete at 20 + feather - 1.
                self.assertEqual(int(alpha[32, 20 + feather - 1]), 255)
                self.assertLess(int(alpha[32, 20]), 255)

    def test_the_region_border_is_a_contour_the_feather_ramps_from(self) -> None:
        # A mask painted up to the region edge used to meet the untouched page
        # with a hard step, because neither `distanceTransform` nor an erosion
        # sees a contour at the array boundary. The region is a window onto a
        # larger page, so its border is one.
        mask = np.zeros((64, 64), dtype=np.uint8)
        mask[:, :32] = 255  # touches the top, left and bottom borders
        distance = svc._mask_distance_inside(mask)
        self.assertEqual(float(distance[0, 0]), 1.0)
        self.assertEqual(float(distance[63, 0]), 1.0)
        self.assertGreater(float(distance[32, 8]), 1.0)
        alpha = svc._feather_mask_inwards(mask, 8)
        self.assertLess(int(alpha[0, 0]), 32)
        self.assertEqual(int(alpha[32, 8]), 255)
        # Still exactly zero outside the mask: the composite's core contract.
        self.assertEqual(int(alpha[32, 40]), 0)

    def test_blending_a_region_with_itself_is_an_exact_identity(self) -> None:
        # A truncating blend biased every partially blended pixel one level
        # darker, which is a mask-shaped dark patch with a hard contour — part of
        # the seam users reported. Rounding is what makes this hold.
        for feather in (0, 3, 6, 12, 32):
            with self.subTest(feather=feather):
                composed = svc._composite_over_region(
                    self.original, self.original.copy(), self.mask, feather
                )
                self.assertTrue(np.array_equal(composed, self.original))

    def test_the_feather_is_the_same_without_cv2(self) -> None:
        # OpenCV is optional for this backend, so the erosion fallback of
        # `_mask_distance_inside` must produce the same ramp, not a coarser one.
        with_cv2 = svc._feather_mask_inwards(self.mask, 6)
        real_import = builtins.__import__

        def without_cv2(name: str, *args: object, **kwargs: object) -> object:
            if name == "cv2":
                raise ImportError("cv2 is unavailable in this test")
            return real_import(name, *args, **kwargs)

        with patch.object(builtins, "__import__", without_cv2):
            fallback = svc._feather_mask_inwards(self.mask, 6)
        self.assertTrue(np.array_equal(with_cv2, fallback))

    def test_color_match_aligns_the_generated_region_to_the_ring(self) -> None:
        rng = np.random.default_rng(7)
        original = rng.integers(80, 160, size=(64, 64, 3), dtype=np.uint8)
        # The generated window came back uniformly brighter, as a VAE round trip
        # tends to leave it.
        generated = np.clip(original.astype(np.int16) + 30, 0, 255).astype(np.uint8)
        matched = svc._match_color_outside_mask(generated, original, self.mask)
        outside = self.mask == 0
        self.assertLess(
            abs(float(matched[outside].mean()) - float(original[outside].mean())), 1.0
        )

    def test_color_match_is_skipped_when_the_ring_is_too_small(self) -> None:
        full = np.full((64, 64), 255, dtype=np.uint8)
        matched = svc._match_color_outside_mask(self.generated, self.original, full)
        self.assertIs(matched, self.generated)


# ---------------------------------------------------------------------------
# VAE decode: parking and OOM recovery
# ---------------------------------------------------------------------------
class DecodeRecoveryTests(_TempTreeCase):
    def setUp(self) -> None:
        super().setUp()
        self.recorder = _PatchRecorder()
        self.torch = _install_fake_torch(self.recorder)
        modules_patch = patch.dict(
            sys.modules, {"torch": self.torch, "torch.nn": self.torch.nn}
        )
        modules_patch.start()
        self.addCleanup(modules_patch.stop)
        for name, replacement in (
            ("patched_module_to", self.recorder),
            ("_clear_torch_cache", lambda: None),
            (
                "memory_snapshot",
                lambda *_args, **_kwargs: {"vram_total": 1 << 33, "vram_free": 1 << 30, "ram_total": 0, "ram_free": 0},
            ),
        ):
            attr_patch = patch.object(svc, name, replacement)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)

        self.decoded = object()
        self.latents = _FakeLatents()
        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())
        self.service._device = _FakeDevice("cuda:0")

    def _pipe(self, *, oom_times: int = 0) -> types.SimpleNamespace:
        return _fake_pipe(self.torch.nn.Module, self.decoded, oom_times=oom_times)

    def _normalized(self, **overrides: object) -> dict[str, object]:
        return svc.normalize_flux2_klein_params(self.params(**overrides))

    def test_the_decode_runs_under_no_grad(self) -> None:
        # The decode is deliberately OUTSIDE `pipeline.__call__`, which is where
        # diffusers puts the `@torch.no_grad()`; without opening it ourselves the
        # VAE builds an autograd graph and `postprocess` dies on
        # "Can't call numpy() on Tensor that requires grad".
        before = self.torch.no_grad.entered
        svc._decode_once(self._pipe(), _FakeLatents())
        self.assertGreater(self.torch.no_grad.entered, before)

    def test_a_meta_resident_vae_decodes_on_the_execution_device(self) -> None:
        # `enable_sequential_cpu_offload` leaves the VAE's parameters on `meta`
        # between forwards, so `vae.device` is `meta` and moving the latents
        # there fails with "Cannot copy out of meta tensor; no data!".
        pipe = self._pipe()
        pipe.vae.device = _FakeDevice("meta")
        pipe._execution_device = _FakeDevice("cuda:0")
        latents = _FakeLatents()
        svc._decode_once(pipe, latents)
        self.assertEqual(str(latents.moves[-1]["device"]), "cuda:0")

    def test_an_offloaded_vae_decodes_on_the_execution_device(self) -> None:
        # `enable_model_cpu_offload` leaves the VAE on the CPU with a hook, and
        # diffusers' `@apply_forward_hook` calls `pre_forward(self)` WITHOUT the
        # arguments — the weights move to the accelerator, our latents do not.
        pipe = self._pipe()
        pipe.vae._hf_hook = object()
        pipe._execution_device = _FakeDevice("cuda:0")
        latents = _FakeLatents()
        svc._decode_once(pipe, latents)
        self.assertEqual(str(latents.moves[-1]["device"]), "cuda:0")

    def test_a_normally_placed_vae_decodes_on_its_own_device(self) -> None:
        # NOT the execution device: by decode time the transformer may be parked
        # on the host, which would make the probe answer `cpu`.
        pipe = self._pipe()
        pipe.vae.device = _FakeDevice("cuda:1")
        pipe._execution_device = _FakeDevice("cpu")
        latents = _FakeLatents()
        svc._decode_once(pipe, latents)
        self.assertEqual(str(latents.moves[-1]["device"]), "cuda:1")

    def test_the_transformer_is_parked_before_the_decode_and_moved_back(self) -> None:
        pipe = self._pipe()
        normalized = self._normalized(
            placement="encoder_cpu", unload_transformer_before_vae=True
        )
        image, applied, recovered = self.service._decode_locked(pipe, self.latents, normalized)

        self.assertIs(image, self.decoded)
        self.assertFalse(recovered)
        self.assertTrue(applied["unload_transformer_before_vae"])
        # Off the device before the decode, back on it afterwards — and the way
        # back goes through the staging patch.
        self.assertEqual(
            [(str(target), depth) for target, depth in pipe.transformer.moves],
            [("cpu", 0), ("cuda:0", 1)],
        )
        self.assertEqual(pipe.vae.decode_calls, 1)

    def test_full_gpu_keeps_the_transformer_in_place_by_default(self) -> None:
        pipe = self._pipe()
        normalized = self._normalized(placement="full_gpu")
        _image, applied, recovered = self.service._decode_locked(pipe, self.latents, normalized)

        self.assertEqual(pipe.transformer.moves, [])
        self.assertFalse(applied["unload_transformer_before_vae"])
        self.assertFalse(recovered)

    def test_an_oom_parks_the_transformer_and_retries_without_redenoising(self) -> None:
        pipe = self._pipe(oom_times=1)
        normalized = self._normalized(placement="full_gpu")
        image, applied, recovered = self.service._decode_locked(pipe, self.latents, normalized)

        self.assertIs(image, self.decoded)
        self.assertTrue(recovered)
        self.assertTrue(applied["unload_transformer_before_vae"])
        # Exactly two decode attempts, both from the same host copy of the
        # latents: the denoise is never repeated.
        self.assertEqual(pipe.vae.decode_calls, 2)
        self.assertEqual(str(pipe.transformer.moves[0][0]), "cpu")
        self.assertEqual(str(pipe.transformer.moves[-1][0]), "cuda:0")

    def test_a_second_oom_escalates_to_tiling_and_slicing(self) -> None:
        pipe = self._pipe(oom_times=2)
        normalized = self._normalized(
            placement="full_gpu", vae_tiling=False, vae_slicing=False
        )
        image, applied, recovered = self.service._decode_locked(pipe, self.latents, normalized)

        self.assertIs(image, self.decoded)
        self.assertTrue(recovered)
        self.assertTrue(applied["vae_tiling"])
        self.assertTrue(applied["vae_slicing"])
        self.assertTrue(pipe.vae.tiling)
        self.assertTrue(pipe.vae.slicing)
        self.assertEqual(pipe.vae.decode_calls, 3)

    def test_an_unrecoverable_oom_reports_the_free_memory(self) -> None:
        pipe = self._pipe(oom_times=9)
        normalized = self._normalized(
            placement="full_gpu", vae_tiling=False, vae_slicing=False
        )
        with self.assertRaises(RuntimeError) as caught:
            self.service._decode_locked(pipe, self.latents, normalized)
        message = str(caught.exception)
        self.assertIn(str(1 << 30), message)
        # The transformer is still put back even on the failing path.
        self.assertEqual(str(pipe.transformer.moves[-1][0]), "cuda:0")

    def test_a_non_oom_failure_is_not_retried(self) -> None:
        pipe = self._pipe()

        def explode(*_args: object, **_kwargs: object) -> None:
            raise ValueError("bad latents")

        pipe.vae.decode = explode
        with self.assertRaises(ValueError):
            self.service._decode_locked(pipe, self.latents, self._normalized())

    def test_offload_placements_do_not_move_the_transformer_themselves(self) -> None:
        # Accelerate already returned it to host memory after the last forward.
        pipe = self._pipe()
        normalized = self._normalized(placement="model_cpu_offload")
        self.service._decode_locked(pipe, self.latents, normalized)
        self.assertEqual(pipe.transformer.moves, [])

    def test_rocm_style_runtime_errors_count_as_out_of_memory(self) -> None:
        self.assertTrue(_is_oom(svc, RuntimeError("HIP out of memory")))
        self.assertFalse(_is_oom(svc, RuntimeError("shape mismatch")))

    def _arm_failing_restore(self, key: str) -> object:
        """Register `key` as resident and make the transformer restore raise OOM."""
        manager = self.service._model_manager
        lease = manager.begin_model_use(key)
        lease.mark_loaded()
        self.service._active_key = key

        def explode(_pipe: object, _device: object) -> None:
            raise _FakeOutOfMemoryError("HIP out of memory. Tried to allocate 18.00 GiB")

        restore_patch = patch.object(svc, "_restore_transformer_to_device", explode)
        restore_patch.start()
        self.addCleanup(restore_patch.stop)
        return manager

    def test_a_failed_restore_keeps_the_result_and_invalidates_the_cache(self) -> None:
        pipe = self._pipe()
        self.service._pipe = pipe
        manager = self._arm_failing_restore("flux2_klein:test")
        normalized = self._normalized(
            placement="encoder_cpu", unload_transformer_before_vae=True
        )

        image, _applied, _recovered = self.service._decode_locked(
            pipe, self.latents, normalized
        )

        # The decode succeeded, so its result must not be thrown away by a
        # failure that happens after it.
        self.assertIs(image, self.decoded)
        # ...but the cached pipeline no longer matches its key: its transformer
        # is on the host, so the next request must rebuild instead of hitting
        # the cache and failing on a device mismatch.
        self.assertIsNone(self.service._pipe)
        self.assertIsNone(self.service._active_key)
        self.assertTrue(manager.begin_model_use("flux2_klein:test").needs_load)

    def test_a_failed_restore_does_not_mask_a_failed_decode(self) -> None:
        pipe = self._pipe(oom_times=9)
        self.service._pipe = pipe
        manager = self._arm_failing_restore("flux2_klein:test")
        normalized = self._normalized(
            placement="encoder_cpu",
            unload_transformer_before_vae=True,
            vae_tiling=False,
            vae_slicing=False,
        )

        with self.assertRaises(RuntimeError) as caught:
            self.service._decode_locked(pipe, self.latents, normalized)
        # The decode's own diagnosis (with the free-memory figures) survives; the
        # restore failure does not replace it.
        self.assertIn(str(1 << 30), str(caught.exception))
        self.assertIsNone(self.service._pipe)
        self.assertTrue(manager.begin_model_use("flux2_klein:test").needs_load)


def _is_oom(module: object, exc: BaseException) -> bool:
    return module._is_out_of_memory(exc)


# ---------------------------------------------------------------------------
# End-to-end request
# ---------------------------------------------------------------------------
class InpaintRequestTests(_TempTreeCase):
    """Drive `inpaint_image_bytes` with a stubbed pipeline."""

    def setUp(self) -> None:
        super().setUp()
        self.recorder = _PatchRecorder()
        self.torch = _install_fake_torch(self.recorder)
        modules_patch = patch.dict(
            sys.modules, {"torch": self.torch, "torch.nn": self.torch.nn}
        )
        modules_patch.start()
        self.addCleanup(modules_patch.stop)
        for name, replacement in (
            ("patched_module_to", self.recorder),
            ("_clear_torch_cache", lambda: None),
        ):
            attr_patch = patch.object(svc, name, replacement)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)

        self.region = np.full((128, 128, 3), 90, dtype=np.uint8)
        self.mask = np.zeros((128, 128), dtype=np.uint8)
        self.mask[48:80, 48:80] = 255
        self.decoded = Image.fromarray(np.full((128, 128, 3), 210, dtype=np.uint8), "RGB")

        self.latents = _FakeLatents()
        self.pipe_calls: list[dict[str, object]] = []
        pipe_calls = self.pipe_calls
        latents = self.latents

        class _Pipe(types.SimpleNamespace):
            def __call__(self, **kwargs: object) -> object:
                pipe_calls.append(kwargs)
                return types.SimpleNamespace(images=latents)

        self.pipe = _Pipe(
            # Both on the accelerator, as `_apply_placement` would have left them:
            # `_warmup_pipeline_locked` now refuses a component still on the host.
            vae=_make_fake_vae(self.torch.nn.Module, self.decoded, device_type="cuda"),
            transformer=self.torch.nn.Module("transformer", ptr=0x4000, device_type="cuda"),
            # The real pipeline has no text encoder after the two-phase split.
            text_encoder=None,
            image_processor=_FakeImageProcessor(),
        )
        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())
        self.service._device = _FakeDevice("cuda:0")
        self.builds = 0
        self.encodes = 0
        #: Names of the run's phases in the order they were entered, so the load
        #: ORDER itself is assertable and not just its effects.
        self.order: list[str] = []

        ensure_patch = patch.object(
            self.service,
            "_ensure_pipeline_locked",
            lambda normalized, model_key, report, region_hw: self._install(model_key),
        )
        ensure_patch.start()
        self.addCleanup(ensure_patch.stop)

        # Phase 1 is stubbed the same way: this class is about the request flow,
        # not about the encoder. `PromptEncodingTests` covers phase 1 itself.
        def _embeds(normalized: dict[str, object], _report: object) -> dict[str, object]:
            self.encodes += 1
            self.order.append("encode")
            negative = (
                _FakeEmbeds("") if float(normalized["guidance_scale"]) > 1.0 else None
            )
            return {"prompt": _FakeEmbeds(str(normalized["prompt"])), "negative": negative}

        embeds_patch = patch.object(self.service, "_prompt_embeds_locked", _embeds)
        embeds_patch.start()
        self.addCleanup(embeds_patch.stop)

        # The guard has its own tests; here it must not gate a fake tree.
        headroom_patch = patch.object(
            self.service, "_require_headroom_locked", lambda *_args, **_kwargs: None
        )
        headroom_patch.start()
        self.addCleanup(headroom_patch.stop)

    def _install(self, model_key: str) -> object:
        """Stand-in for `_ensure_pipeline_locked`, including its cache-hit branch.

        The cache check is reproduced so that `self.builds` counts real rebuilds:
        an invalidated pipeline must show up here as a second build.
        """
        self.order.append("pipeline")
        if self.service._pipe is not None and self.service._active_key == model_key:
            return self.service._pipe
        self.builds += 1
        # `_apply_placement` puts both GPU-resident components on the device, and
        # the stub has to as well: a rebuild after a failed transformer restore
        # would otherwise hand back a pipeline whose transformer is still parked
        # on the host, which `_warmup_pipeline_locked` correctly refuses.
        self.pipe.transformer.to(self.service._device)
        self.pipe.vae.to(self.service._device)
        self.service._pipe = self.pipe
        self.service._active_key = model_key
        return self.pipe

    def _run(self, **overrides: object) -> dict[str, object]:
        return self.service.inpaint_image_bytes(
            _png_bytes(self.region, "RGB"),
            _png_bytes(self.mask, "L"),
            params=self.params(**overrides),
        )

    def test_the_result_is_the_region_size_and_untouched_outside_the_mask(self) -> None:
        import io

        result = self._run(placement="full_gpu", color_match=False, mask_feather_px=0)
        self.assertEqual(result["region_size"], [128, 128])
        with Image.open(io.BytesIO(result["image_png"])) as image:
            out = np.asarray(image.convert("RGB"), dtype=np.uint8)
        outside = self.mask == 0
        self.assertTrue(np.array_equal(out[outside], self.region[outside]))
        self.assertTrue(np.array_equal(out[63, 63], np.asarray(self.decoded)[63, 63]))

    def test_the_pipeline_is_asked_for_latents_and_a_dilated_mask(self) -> None:
        self._run(placement="full_gpu", mask_dilate_px=8, color_match=False)
        call = self.pipe_calls[0]
        self.assertEqual(call["output_type"], "latent")
        dilated = np.asarray(call["mask_image"], dtype=np.uint8)
        # The latent mask is grown; the composite still uses the original one.
        self.assertGreater(int((dilated > 0).sum()), int((self.mask > 0).sum()))
        self.assertEqual(self.latents.detached, 1)

    def test_the_applied_settings_and_recovery_flag_reach_the_caller(self) -> None:
        result = self._run(placement="encoder_cpu", vae_tiling=True, vae_slicing=False)
        self.assertFalse(result["oom_recovered"])
        self.assertEqual(
            result["applied"],
            {
                "unload_transformer_before_vae": True,
                "vae_tiling": True,
                "vae_slicing": False,
                # The shipped default keeps the encoder now: it arrives after the
                # transformer has left host memory, so there is nothing to free.
                "unload_text_encoder_after_encode": False,
                "text_encoder_fp8": False,
            },
        )

    def test_an_oom_in_the_decode_is_reported_as_recovered(self) -> None:
        self.pipe.vae.oom_left = 1
        result = self._run(placement="full_gpu")
        self.assertTrue(result["oom_recovered"])
        self.assertTrue(result["applied"]["unload_transformer_before_vae"])

    def test_a_mask_of_the_wrong_size_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            self.service.inpaint_image_bytes(
                _png_bytes(self.region, "RGB"),
                _png_bytes(np.zeros((64, 64), dtype=np.uint8), "L"),
                params=self.params(),
            )

    def test_a_region_the_pipeline_would_resize_is_refused(self) -> None:
        odd = np.full((120, 128, 3), 90, dtype=np.uint8)
        with self.assertRaises(ValueError):
            self.service.inpaint_image_bytes(
                _png_bytes(odd, "RGB"),
                _png_bytes(np.zeros((120, 128), dtype=np.uint8), "L"),
                params=self.params(),
            )

    def test_the_pipeline_is_built_before_the_text_encoder_is_read(self) -> None:
        # The load ORDER is the memory contract: the 16 GB encoder may only be
        # read once the transformer's host copy is gone, i.e. after the pipeline
        # has been built AND placed.
        self._run(placement="encoder_cpu")
        self.assertEqual(self.order, ["pipeline", "encode"])

    def test_the_order_holds_on_a_cache_hit_too(self) -> None:
        self._run(placement="encoder_cpu", prompt="a cat")
        self.order.clear()
        self._run(placement="encoder_cpu", prompt="a dog")
        self.assertEqual(self.order, ["pipeline", "encode"])

    def test_the_warmup_runs_once_per_request_and_is_not_a_generation_step(self) -> None:
        frames: list[tuple[str, int, int, str]] = []
        self.service.inpaint_image_bytes(
            _png_bytes(self.region, "RGB"),
            _png_bytes(self.mask, "L"),
            params=self.params(placement="encoder_cpu"),
            progress_callback=lambda *frame: frames.append(frame),
        )
        self.assertEqual(self.pipe.vae.warmup_calls, 1)
        warmups = [frame for frame in frames if frame[3] == "Прогрев модели"]
        self.assertEqual(len(warmups), 1)
        self.assertEqual(warmups[0][0], "load")
        self.assertEqual(warmups[0][1], svc.LOAD_STEP_WARMUP)
        # The generate counter must not have grown by it: `total` there is the
        # number of diffusion steps and nothing else, and no generate frame
        # belongs to the warm-up.
        generate = [frame for frame in frames if frame[0] == "generate"]
        self.assertTrue(generate)
        steps = svc.effective_steps(4, 1.0)
        self.assertTrue(all(total == steps for _p, _s, total, _l in generate))
        self.assertTrue(all(label == "Генерация" for _p, _s, _t, label in generate))

    def test_the_warmup_decode_does_not_consume_the_runs_oom(self) -> None:
        # The warm-up is a 64x64 decode; an OOM belongs to the real one.
        self.pipe.vae.oom_left = 1
        result = self._run(placement="full_gpu")
        self.assertEqual(self.pipe.vae.warmup_calls, 1)
        self.assertTrue(result["oom_recovered"])

    def test_a_second_request_is_a_cache_hit(self) -> None:
        self._run(placement="full_gpu")
        self._run(placement="full_gpu")
        self.assertEqual(self.builds, 1)

    def test_a_failed_restore_makes_the_next_request_rebuild(self) -> None:
        failures = [1]

        def explode(pipe: object, device: object) -> None:
            """Fail the first restore only, so the second request is the clean one."""
            if failures:
                failures.pop()
                raise _FakeOutOfMemoryError("HIP out of memory. Tried to allocate 18.00 GiB")
            pipe.transformer.to(device)

        restore_patch = patch.object(svc, "_restore_transformer_to_device", explode)
        restore_patch.start()
        self.addCleanup(restore_patch.stop)

        # `encoder_cpu` parks the transformer, so the failing restore is reached.
        result = self._run(placement="encoder_cpu")
        self.assertEqual(result["region_size"], [128, 128])

        self._run(placement="encoder_cpu")
        # The invalidated pipeline is rebuilt rather than reused with its
        # transformer left on the host.
        self.assertEqual(self.builds, 2)

    def test_whole_region_regenerates_everything_with_a_solid_mask(self) -> None:
        import io

        solid = np.full((128, 128), 255, dtype=np.uint8)
        result = self.service.inpaint_image_bytes(
            _png_bytes(self.region, "RGB"),
            _png_bytes(solid, "L"),
            params=self.params(placement="full_gpu", whole_region=True, mask_feather_px=0),
        )
        with Image.open(io.BytesIO(result["image_png"])) as image:
            out = np.asarray(image.convert("RGB"), dtype=np.uint8)
        # Every pixel comes from the generated image, not just a painted blob.
        self.assertTrue(np.array_equal(out, np.asarray(self.decoded)))
        # The mask handed to the pipeline is the solid one, undilated.
        latent_mask = np.asarray(self.pipe_calls[0]["mask_image"], dtype=np.uint8)
        self.assertTrue(np.array_equal(latent_mask, solid))

    def test_whole_region_still_feathers_inwards_from_the_region_border(self) -> None:
        import io

        solid = np.full((128, 128), 255, dtype=np.uint8)
        result = self.service.inpaint_image_bytes(
            _png_bytes(self.region, "RGB"),
            _png_bytes(solid, "L"),
            params=self.params(placement="full_gpu", whole_region=True, mask_feather_px=12),
        )
        with Image.open(io.BytesIO(result["image_png"])) as image:
            out = np.asarray(image.convert("RGB"), dtype=np.uint8)
        generated = np.asarray(self.decoded)
        # The centre is fully the edit; the region border is still almost entirely
        # the original page, because the ramp starts at the border and reaches
        # full strength `mask_feather_px` pixels in. That gradient IS the soft
        # join to the rest of the page this mode relies on, and without the zero
        # ring in `_mask_distance_inside` a solid mask would have no contour at
        # all and the feather would be a no-op.
        self.assertTrue(np.array_equal(out[64, 64], generated[64, 64]))
        original, edited = int(self.region[0, 0, 0]), int(generated[0, 0, 0])
        self.assertLess(abs(int(out[0, 0, 0]) - original), abs(edited - original) // 4)
        self.assertNotEqual(int(out[0, 0, 0]), edited)
        # Monotone: the further in, the more of the edit.
        profile = [int(out[row, 64, 0]) for row in range(0, 13)]
        self.assertEqual(profile, sorted(profile))
        self.assertEqual(profile[-1], edited)

    def test_whole_region_refuses_a_mask_that_is_not_solid(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self.service.inpaint_image_bytes(
                _png_bytes(self.region, "RGB"),
                _png_bytes(self.mask, "L"),
                params=self.params(whole_region=True),
            )
        self.assertIn("whole_region", str(caught.exception))
        # Refused before anything is loaded: the flag and the data disagree, and
        # that is a request error, not a smaller edit.
        self.assertEqual(self.builds, 0)
        self.assertEqual(self.encodes, 0)

    def test_an_rgb_mask_is_refused(self) -> None:
        rgb_mask = np.zeros((128, 128, 3), dtype=np.uint8)
        rgb_mask[48:80, 48:80] = 255
        with self.assertRaises(ValueError) as caught:
            self.service.inpaint_image_bytes(
                _png_bytes(self.region, "RGB"), _png_bytes(rgb_mask, "RGB"), params=self.params()
            )
        self.assertIn("RGB", str(caught.exception))

    def test_an_rgba_mask_is_refused(self) -> None:
        rgba_mask = np.zeros((128, 128, 4), dtype=np.uint8)
        rgba_mask[48:80, 48:80] = 255
        with self.assertRaises(ValueError) as caught:
            self.service.inpaint_image_bytes(
                _png_bytes(self.region, "RGB"), _png_bytes(rgba_mask, "RGBA"), params=self.params()
            )
        self.assertIn("RGBA", str(caught.exception))

    def test_the_mask_mode_is_checked_before_anything_is_loaded(self) -> None:
        rgb_mask = np.zeros((128, 128, 3), dtype=np.uint8)
        with self.assertRaises(ValueError):
            self.service.inpaint_image_bytes(
                _png_bytes(self.region, "RGB"), _png_bytes(rgb_mask, "RGB"), params=self.params()
            )
        self.assertEqual(self.builds, 0)


# ---------------------------------------------------------------------------
# Memory forecast and status
# ---------------------------------------------------------------------------
class WarmupTests(_TempTreeCase):
    """`_warmup_pipeline_locked`: proof that the weights really left the host.

    It is the hinge of the new load order — the 16 GB text encoder is read
    immediately after it — so "placed" has to become "materialized" here, with a
    named error when it did not.
    """

    def setUp(self) -> None:
        super().setUp()
        self.recorder = _PatchRecorder()
        self.torch = _install_fake_torch(self.recorder)
        modules_patch = patch.dict(sys.modules, {"torch": self.torch, "torch.nn": self.torch.nn})
        modules_patch.start()
        self.addCleanup(modules_patch.stop)
        cache_patch = patch.object(svc, "_clear_torch_cache", lambda: None)
        cache_patch.start()
        self.addCleanup(cache_patch.stop)

        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())
        self.service._device = _FakeDevice("cuda:0")
        self.frames: list[tuple[int, str]] = []

    def _pipe(self, *, transformer_device: str = "cuda", **vae_kwargs: object) -> object:
        module_cls = self.torch.nn.Module
        return types.SimpleNamespace(
            vae=_make_fake_vae(module_cls, object(), device_type="cuda", **vae_kwargs),
            transformer=module_cls("transformer", ptr=0x4000, device_type=transformer_device),
        )

    def _warmup(self, pipe: object, **overrides: object) -> bool:
        normalized = svc.normalize_flux2_klein_params(self.params(**overrides))
        return self.service._warmup_pipeline_locked(
            pipe, normalized, lambda step, label: self.frames.append((step, label))
        )

    def test_it_runs_one_tiny_decode_and_reports_a_load_step(self) -> None:
        pipe = self._pipe()
        self.assertTrue(self._warmup(pipe, placement="full_gpu"))
        self.assertEqual(pipe.vae.warmup_calls, 1)
        self.assertEqual(pipe.vae.decode_calls, 0)
        self.assertEqual(self.frames, [(svc.LOAD_STEP_WARMUP, "Прогрев модели")])

    def test_the_warmup_latent_matches_the_vae_and_the_contract_size(self) -> None:
        pipe = self._pipe(latent_channels=32)
        decoded: list[object] = []
        original = pipe.vae.decode

        def record(latents: object, return_dict: bool = True) -> object:
            decoded.append(latents)
            return original(latents, return_dict)

        pipe.vae.decode = record
        self._warmup(pipe, placement="full_gpu")
        self.assertEqual(
            decoded[0].shape, (1, 32, svc.WARMUP_LATENT_CELLS, svc.WARMUP_LATENT_CELLS)
        )
        self.assertEqual(decoded[0].kwargs["dtype"], pipe.vae.dtype)

    def test_a_component_left_on_the_host_is_refused_by_name(self) -> None:
        pipe = self._pipe(transformer_device="cpu")
        with self.assertRaises(RuntimeError) as caught:
            self._warmup(pipe, placement="encoder_cpu")
        message = str(caught.exception)
        self.assertIn("transformer", message)
        self.assertIn("cpu", message)
        # Refused BEFORE the decode, so a mis-placed pipeline never runs at all.
        self.assertEqual(pipe.vae.warmup_calls, 0)

    def test_the_offload_placements_are_skipped(self) -> None:
        # There the weights are SUPPOSED to sit in host memory between forwards;
        # a warm-up would drag all 9B onto the card and straight back.
        for placement in ("model_cpu_offload", "sequential_cpu_offload"):
            with self.subTest(placement=placement):
                pipe = self._pipe(transformer_device="cpu")
                self.assertFalse(self._warmup(pipe, placement=placement))
                self.assertEqual(pipe.vae.warmup_calls, 0)
                self.assertEqual(self.frames, [])

    def test_a_vae_without_latent_channels_degrades_to_a_synchronization(self) -> None:
        pipe = self._pipe(latent_channels=None)
        with self.assertLogs(svc.log, level="WARNING") as logs:
            self.assertTrue(self._warmup(pipe, placement="full_gpu"))
        self.assertEqual(pipe.vae.warmup_calls, 0)
        self.assertIn("latent_channels", "\n".join(logs.output))

    def test_a_pipeline_without_a_vae_is_not_warmed_up(self) -> None:
        # The lease-protocol tests install a stand-in with no components at all.
        self.assertFalse(self._warmup(types.SimpleNamespace(), placement="full_gpu"))
        self.assertEqual(self.frames, [])


class WholeRegionTests(_TempTreeCase):
    """The "no mask" mode: `whole_region` edits the entire validated region."""

    def test_it_is_off_by_default(self) -> None:
        self.assertFalse(
            svc.normalize_flux2_klein_params(self.params())["whole_region"]
        )

    def test_it_switches_off_the_dilate_and_the_color_match(self) -> None:
        normalized = svc.normalize_flux2_klein_params(
            self.params(whole_region=True, mask_dilate_px=32, color_match=True)
        )
        # Nothing to grow into, and no unchanged ring to take statistics from.
        self.assertEqual(normalized["mask_dilate_px"], 0)
        self.assertFalse(normalized["color_match"])

    def test_it_leaves_the_feather_alone(self) -> None:
        # The feather is what joins the regenerated region to the rest of the
        # page, so it is the one mask parameter this mode keeps.
        normalized = svc.normalize_flux2_klein_params(
            self.params(whole_region=True, mask_feather_px=20)
        )
        self.assertEqual(normalized["mask_feather_px"], 20)

    def test_a_solid_mask_is_accepted(self) -> None:
        svc._require_solid_mask(np.full((32, 32), 255, dtype=np.uint8))

    def test_a_mask_with_a_hole_is_refused_with_the_count(self) -> None:
        mask = np.full((32, 32), 255, dtype=np.uint8)
        mask[4:8, 4:8] = 0
        with self.assertRaises(ValueError) as caught:
            svc._require_solid_mask(mask)
        message = str(caught.exception)
        self.assertIn("16", message)
        self.assertIn("whole_region", message)

    def test_an_empty_mask_is_refused_too(self) -> None:
        with self.assertRaises(ValueError):
            svc._require_solid_mask(np.zeros((16, 16), dtype=np.uint8))


class EstimateTests(_TempTreeCase):
    def setUp(self) -> None:
        super().setUp()
        memory_patch = patch.object(
            svc,
            "memory_snapshot",
            lambda *_args, **_kwargs: {
                "vram_total": 12 << 30,
                "vram_free": 10 << 30,
                "ram_total": 32 << 30,
                "ram_free": 16 << 30,
            },
        )
        memory_patch.start()
        self.addCleanup(memory_patch.stop)
        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())

    def test_the_breakdown_reports_every_phase_peak(self) -> None:
        out = self.service.estimate(
            params=self.params(placement="full_gpu"), region_width=512, region_height=512
        )
        breakdown = out["breakdown"]
        for key in ("peak_encode", "peak_denoise", "peak_decode"):
            with self.subTest(key=key):
                self.assertIn(key, breakdown)
        # The Rust side renders every breakdown entry it does not recognise as an
        # extra row, so the key set is part of the wire contract, not an internal
        # detail (`Flux2Estimate::breakdown` is a `Vec<(String, u64)>`).
        self.assertEqual(
            set(breakdown),
            {
                "transformer",
                "text_encoder",
                "vae",
                "activations",
                "peak_encode",
                "peak_denoise",
                "peak_decode",
            },
        )
        # The encode phase is the one whose cost is dominated by HOST memory now,
        # so its peak is the larger of the two sides rather than only its VRAM.
        phases = svc.forecast_memory(
            svc.normalize_flux2_klein_params(self.params(placement="full_gpu")), 512, 512
        )["phases"]
        self.assertEqual(
            breakdown["peak_encode"],
            max(phases["encode"]["vram_bytes"], phases["encode"]["ram_bytes"]),
        )
        # A run is a SEQUENCE of phases, so the answer is their maximum.
        self.assertEqual(
            out["vram_bytes"],
            max(phase["vram_bytes"] for phase in svc.forecast_memory(
                svc.normalize_flux2_klein_params(self.params(placement="full_gpu")), 512, 512
            )["phases"].values()),
        )

    def test_parking_the_transformer_lowers_the_decode_peak(self) -> None:
        with_park = self.service.estimate(
            params=self.params(placement="full_gpu", unload_transformer_before_vae=True),
            region_width=512,
            region_height=512,
        )
        without = self.service.estimate(
            params=self.params(placement="full_gpu", unload_transformer_before_vae=False),
            region_width=512,
            region_height=512,
        )
        self.assertLess(
            with_park["breakdown"]["peak_decode"], without["breakdown"]["peak_decode"]
        )

    def test_the_weight_terms_come_from_disk(self) -> None:
        out = self.service.estimate(params=self.params(), region_width=256, region_height=256)
        self.assertEqual(out["breakdown"]["transformer"], 4096)
        self.assertEqual(out["breakdown"]["text_encoder"], 2048)
        self.assertEqual(out["breakdown"]["vae"], 1024)

    def test_without_an_encoder_the_encode_phases_cost_nothing(self) -> None:
        params = self.params()
        params.pop("text_encoder_path")
        forecast = svc.forecast_memory(svc.normalize_flux2_klein_params(params), 512, 512)
        for phase in ("encode", "encode_standalone"):
            with self.subTest(phase=phase):
                self.assertEqual(forecast["phases"][phase]["vram_bytes"], 0)
                self.assertEqual(forecast["phases"][phase]["ram_bytes"], 0)
        self.assertEqual(forecast["breakdown"]["text_encoder"], 0)
        self.assertEqual(forecast["breakdown"]["peak_encode"], 0)
        self.assertEqual(forecast["resident"]["text_encoder_host"], 0)
        # The denoise and the decode are untouched: they never needed the encoder.
        with_encoder = svc.forecast_memory(
            svc.normalize_flux2_klein_params(self.params()), 512, 512
        )
        for phase in ("denoise", "decode"):
            with self.subTest(phase=phase):
                self.assertEqual(
                    forecast["phases"][phase]["vram_bytes"],
                    with_encoder["phases"][phase]["vram_bytes"],
                )

    def test_the_estimate_is_lower_without_an_encoder(self) -> None:
        # One calculation feeds both the UI and the guard, so a machine that
        # cannot run an encode must be forecast — and gated — as one that will not.
        without = dict(self.params())
        without.pop("text_encoder_path")
        cheap = self.service.estimate(params=without, region_width=512, region_height=512)
        full = self.service.estimate(params=self.params(), region_width=512, region_height=512)
        self.assertLess(cheap["ram_bytes"], full["ram_bytes"])
        self.assertEqual(set(cheap["breakdown"]), set(full["breakdown"]))

    def test_an_invalid_region_is_refused_before_any_arithmetic(self) -> None:
        with self.assertRaises(ValueError):
            self.service.estimate(params=self.params(), region_width=100, region_height=256)

    def test_fits_is_false_when_the_forecast_exceeds_the_free_vram(self) -> None:
        with patch.object(
            svc,
            "memory_snapshot",
            lambda *_args, **_kwargs: {
                "vram_total": 1 << 20,
                "vram_free": 1 << 10,
                "ram_total": 1 << 20,
                "ram_free": 1 << 20,
            },
        ):
            out = self.service.estimate(
                params=self.params(), region_width=512, region_height=512
            )
        self.assertFalse(out["fits"])


class StatusTests(_TempTreeCase):
    def setUp(self) -> None:
        super().setUp()
        torch_patch = patch.object(svc, "is_torch_available", lambda: True)
        torch_patch.start()
        self.addCleanup(torch_patch.stop)
        memory_patch = patch.object(
            svc,
            "memory_snapshot",
            lambda *_args, **_kwargs: {"vram_total": 0, "vram_free": 0, "ram_total": 0, "ram_free": 0},
        )
        memory_patch.start()
        self.addCleanup(memory_patch.stop)
        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())

    def test_a_complete_tree_is_available(self) -> None:
        out = self.service.status(self.params())
        self.assertTrue(out["available"])
        self.assertIsNone(out["reason"])
        self.assertTrue(out["components"]["tokenizer"]["found"])
        self.assertTrue(out["components"]["scheduler"]["found"])
        self.assertFalse(out["loaded"])

    def test_an_unconfigured_service_reports_the_missing_component(self) -> None:
        out = self.service.status(None)
        self.assertFalse(out["available"])
        self.assertIn("энкодер", out["reason"])

    def test_a_missing_scheduler_is_named(self) -> None:
        (self.root / "scheduler" / "scheduler_config.json").unlink()
        out = self.service.status(self.params())
        self.assertFalse(out["available"])
        self.assertIn("планировщик", out["reason"])

    def _without_encoder(self, **overrides: object) -> dict[str, object]:
        params = self.params(**overrides)
        params.pop("text_encoder_path")
        return params

    def _cache(self, params: dict[str, object]) -> None:
        """Put a stand-in embedding under the key a run would look up."""
        normalized = svc.normalize_flux2_klein_params(params)
        self.service._prompt_cache[
            self.service._prompt_cache_key(normalized, normalized["prompt"])
        ] = _FakeEmbeds(str(normalized["prompt"]))

    def test_a_missing_encoder_is_reported_as_its_own_flag(self) -> None:
        out = self.service.status(self._without_encoder(prompt="a cat"))
        self.assertFalse(out["text_encoder_available"])
        # A complete tree still reports the flag, so the client can rely on it.
        self.assertTrue(self.service.status(self.params())["text_encoder_available"])

    def test_a_nonexistent_encoder_path_is_not_available_either(self) -> None:
        out = self.service.status(self.params(text_encoder_path=str(self.root / "gone")))
        self.assertFalse(out["text_encoder_available"])
        # The path itself is still reported, so a typo stays diagnosable — and the
        # reason names it AND the way out that needs no encoder.
        self.assertEqual(out["components"]["text_encoder"]["path"], str(self.root / "gone"))
        self.assertFalse(out["components"]["text_encoder"]["exists"])
        self.assertIn(str(self.root / "gone"), out["reason"])
        self.assertIn("кэш", out["reason"])

    def test_a_cached_prompt_keeps_the_service_available_without_an_encoder(self) -> None:
        params = self._without_encoder(prompt="a cat")
        self._cache(params)
        out = self.service.status(params)
        self.assertTrue(out["available"])
        self.assertIsNone(out["reason"])
        self.assertTrue(out["prompt_cached"])
        self.assertFalse(out["text_encoder_available"])

    def test_without_a_cached_prompt_the_reason_offers_the_cache(self) -> None:
        out = self.service.status(self._without_encoder(prompt="a cat"))
        self.assertFalse(out["available"])
        self.assertIn("энкодер", out["reason"])
        self.assertIn("кэш", out["reason"])

    def test_only_the_encoder_is_waived_by_a_cached_prompt(self) -> None:
        # The tokenizer is a real pipeline component (`_ensure_pipeline_locked`
        # builds the pipeline with one), so a cached prompt does NOT waive it —
        # a run without it fails at the load, not at the encode. Same for the
        # scheduler, which the denoise needs.
        import shutil

        params = self._without_encoder(prompt="a cat")
        self._cache(params)
        self.assertTrue(self.service.status(params)["available"])
        shutil.rmtree(self.root / "tokenizer")
        out = self.service.status(params)
        self.assertFalse(out["available"])
        self.assertIn("токенизатор", out["reason"])


class DeviceReportingTests(_TempTreeCase):
    """`cpu` is never a placeholder: it is reported only when it is the answer."""

    def setUp(self) -> None:
        super().setUp()
        self.resolved: list[str] = []

        def _resolve(fallback: str) -> str:
            self.resolved.append(fallback)
            return "cuda:0"

        resolve_patch = patch.object(svc, "_resolve_selected_backend_device", _resolve)
        resolve_patch.start()
        self.addCleanup(resolve_patch.stop)

        self.snapshots: list[object] = []

        def _snapshot(device: object = None) -> dict[str, int]:
            self.snapshots.append(device)
            return {"vram_total": 0, "vram_free": 0, "ram_total": 0, "ram_free": 0}

        memory_patch = patch.object(svc, "memory_snapshot", _snapshot)
        memory_patch.start()
        self.addCleanup(memory_patch.stop)

        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())

    def test_status_before_the_first_load_reports_the_device_that_will_be_used(self) -> None:
        out = self.service.status(self.params())
        # Not "cpu": the run will happen on the configured accelerator, and a
        # user deciding whether to start a tens-of-minutes job must see that.
        self.assertEqual(out["device"], "cuda:0")
        self.assertFalse(out["loaded"])
        self.assertEqual(self.resolved, ["cuda"])

    def test_status_after_a_load_reports_the_actual_device(self) -> None:
        self.service._pipe = object()
        self.service._device = "cuda:1"
        out = self.service.status(self.params())
        self.assertEqual(out["device"], "cuda:1")
        self.assertTrue(out["loaded"])
        # The loaded device is a fact; nothing is resolved on top of it.
        self.assertEqual(self.resolved, [])

    def test_health_follows_the_same_rule(self) -> None:
        self.assertEqual(self.service.health()["device"], "cuda:0")
        self.service._pipe = object()
        self.service._device = "cuda:1"
        self.assertEqual(self.service.health()["device"], "cuda:1")

    def test_the_memory_forecast_is_read_from_the_selected_device(self) -> None:
        # A VRAM forecast compared against another card's free memory is advice
        # about the wrong hardware.
        self.service.estimate(params=self.params(), region_width=512, region_height=512)
        self.assertEqual(self.snapshots, ["cuda:0"])

    def test_a_cpu_selection_is_still_reported_as_cpu(self) -> None:
        with patch.object(svc, "_resolve_selected_backend_device", lambda _fallback: "cpu"):
            self.assertEqual(self.service.status(self.params())["device"], "cpu")


class CudaDeviceIndexTests(unittest.TestCase):
    def test_an_explicit_ordinal_is_parsed(self) -> None:
        self.assertEqual(svc._cuda_device_index("cuda:1"), 1)

    def test_everything_else_means_the_current_device(self) -> None:
        for value in ("cuda", "cpu", "mps", "", None, "cuda:x"):
            self.assertIsNone(svc._cuda_device_index(value))


class UnloadTests(unittest.TestCase):
    def test_unload_drops_the_pipeline_and_reports_it(self) -> None:
        service = svc.Flux2KleinInpaintService(LoadedModelManager())
        service._pipe = object()
        service._active_key = "flux2_klein:a"

        with patch.object(service._model_manager, "mark_unloaded") as unloaded:
            self.assertTrue(service.unload())

        unloaded.assert_called_once_with("flux2_klein:a")
        self.assertIsNone(service._pipe)
        self.assertIsNone(service._active_key)

    def test_unload_without_a_pipeline_is_a_noop(self) -> None:
        self.assertFalse(svc.Flux2KleinInpaintService(LoadedModelManager()).unload())

    def test_unload_key_refuses_a_foreign_key(self) -> None:
        service = svc.Flux2KleinInpaintService(LoadedModelManager())
        service._pipe = object()
        service._active_key = "flux2_klein:a"

        self.assertFalse(service._unload_key("flux2_klein:b"))
        self.assertIsNotNone(service._pipe)


class EffectiveStepsTests(unittest.TestCase):
    def test_full_strength_keeps_every_step(self) -> None:
        self.assertEqual(svc.effective_steps(4, 1.0), 4)

    def test_the_quantization_is_coarse_at_four_steps(self) -> None:
        # 4 - int(4 - 3.2) == 4: strength 0.8 still runs all four steps.
        self.assertEqual(svc.effective_steps(4, 0.8), 4)

    def test_it_never_drops_below_one_step(self) -> None:
        self.assertEqual(svc.effective_steps(4, 0.25), 1)


# ---------------------------------------------------------------------------
# The prompt-cache library
# ---------------------------------------------------------------------------
def _prompt_cache_bytes(metadata: dict[str, str], *, dtype_token: str = "BF16") -> bytes:
    """A syntactically valid `.msprompt` container carrying `metadata`.

    The tensor is a stub: everything the library layer does — listing,
    compatibility checking, filing an import under its own family — reads the
    HEADER only, and building a real tensor would drag torch into tests that are
    about directory layout. The one place that does load a tensor has its own
    torch-gated round-trip test below.
    """
    payload = b"\x00" * 64
    header = {
        "prompt_embeds": {"dtype": dtype_token, "shape": [1, 4, 8], "data_offsets": [0, 64]},
        "__metadata__": {str(key): str(value) for key, value in metadata.items()},
    }
    blob = json.dumps(header).encode("utf-8")
    return struct.pack("<Q", len(blob)) + blob + payload


def _write_prompt_cache_file(path: Path, metadata: dict[str, str], **kwargs: object) -> None:
    """Write a stub `.msprompt` file, creating its family directory."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(_prompt_cache_bytes(metadata, **kwargs))  # type: ignore[arg-type]


class NameSanitizationTests(unittest.TestCase):
    """A family or entry name is untrusted text and must stay one path component."""

    def test_separators_and_traversal_cannot_survive(self) -> None:
        for raw in ("../../etc/passwd", "a/b", "a\\b", "..", ".", "  ..  "):
            with self.subTest(raw=raw):
                try:
                    safe = svc.sanitize_name_component(raw, what="имя")
                except ValueError:
                    continue
                self.assertNotIn("/", safe)
                self.assertNotIn("\\", safe)
                self.assertNotEqual(safe, "..")
                self.assertEqual(Path(safe).name, safe)

    def test_an_empty_or_dots_only_name_is_refused(self) -> None:
        for raw in ("", "   ", "...", "///"):
            with self.subTest(raw=raw):
                with self.assertRaises(ValueError):
                    svc.sanitize_name_component(raw, what="имя кэша")

    def test_ordinary_names_are_kept_verbatim(self) -> None:
        self.assertEqual(
            svc.sanitize_name_component("Удаление текста (v2)", what="имя"),
            "Удаление текста (v2)",
        )

    def test_a_long_name_is_truncated(self) -> None:
        safe = svc.sanitize_name_component("x" * 500, what="имя")
        self.assertEqual(len(safe), svc._MAX_NAME_LENGTH)


class EncoderFingerprintTests(_TempTreeCase):
    """The identity a `.msprompt` file is checked against."""

    def _encoder(self) -> str:
        return self.paths["text_encoder_path"]

    def test_it_is_stable_across_calls(self) -> None:
        self.assertEqual(
            svc.text_encoder_fingerprint(self._encoder()),
            svc.text_encoder_fingerprint(self._encoder()),
        )

    def test_a_changed_config_changes_it(self) -> None:
        before = svc.text_encoder_fingerprint(self._encoder())
        (self.root / "text_encoder" / "config.json").write_text('{"a": 1}', encoding="utf-8")
        self.assertNotEqual(before, svc.text_encoder_fingerprint(self._encoder()))

    def test_a_changed_weight_size_changes_it(self) -> None:
        before = svc.text_encoder_fingerprint(self._encoder())
        (self.root / "text_encoder" / "model.safetensors").write_bytes(b"\x00" * 4096)
        self.assertNotEqual(before, svc.text_encoder_fingerprint(self._encoder()))

    def test_a_new_shard_changes_it(self) -> None:
        before = svc.text_encoder_fingerprint(self._encoder())
        (self.root / "text_encoder" / "model-2.safetensors").write_bytes(b"\x00" * 8)
        self.assertNotEqual(before, svc.text_encoder_fingerprint(self._encoder()))

    def test_a_file_inside_the_folder_identifies_the_folder(self) -> None:
        inside = str(self.root / "text_encoder" / "model.safetensors")
        self.assertEqual(
            svc.text_encoder_fingerprint(inside), svc.text_encoder_fingerprint(self._encoder())
        )

    def test_a_directory_without_a_config_cannot_be_identified(self) -> None:
        (self.root / "text_encoder" / "config.json").unlink()
        with self.assertRaises(ValueError):
            svc.text_encoder_fingerprint(self._encoder())

    def test_the_family_name_is_readable_and_unique(self) -> None:
        encoder_id = svc.text_encoder_fingerprint(self._encoder())
        family = svc.encoder_family_name(self._encoder(), encoder_id)
        self.assertTrue(family.startswith("text_encoder-"))
        self.assertTrue(family.endswith(encoder_id[: svc.PROMPT_CACHE_FAMILY_HASH_CHARS]))

    def test_two_encoders_with_the_same_directory_name_get_different_families(self) -> None:
        # The common case: every checkout calls its encoder folder `text_encoder`.
        other = self.root / "other" / "text_encoder"
        other.mkdir(parents=True)
        (other / "config.json").write_text('{"hidden": 4096}', encoding="utf-8")
        (other / "model.safetensors").write_bytes(b"\x00" * 64)
        mine = svc.encoder_family_name(
            self._encoder(), svc.text_encoder_fingerprint(self._encoder())
        )
        theirs = svc.encoder_family_name(str(other), svc.text_encoder_fingerprint(str(other)))
        self.assertNotEqual(mine, theirs)


class PromptCacheLibraryTests(_TempTreeCase):
    """`prompt_cache/<family>/<name>.msprompt`: layout, listing and the two copies.

    Serialization is stubbed (`_prompt_cache_bytes`) so these stay torch-free;
    what they pin is WHERE files land, which of them a listing accepts, and which
    of them a load refuses. The atomic publish itself is NOT stubbed.
    """

    def setUp(self) -> None:
        super().setUp()
        self.library = self.root / "program"
        self.library.mkdir()
        root_patch = patch.object(svc, "program_root", lambda: self.library)
        root_patch.start()
        self.addCleanup(root_patch.stop)

        def _write(dest: Path, _embeds: object, metadata: dict[str, str]) -> int:
            return svc.publish_bytes_atomically(dest, _prompt_cache_bytes(metadata))

        write_patch = patch.object(svc, "write_prompt_file", _write)
        write_patch.start()
        self.addCleanup(write_patch.stop)

        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())
        self.encoder_id = svc.text_encoder_fingerprint(self.paths["text_encoder_path"])
        self.family = svc.encoder_family_name(self.paths["text_encoder_path"], self.encoder_id)

    def _seed_cache(self, prompt: str = "remove the text", **overrides: object) -> dict[str, Any]:
        """Put a stand-in embedding in the LRU under the real key."""
        normalized = svc.normalize_flux2_klein_params(self.params(prompt=prompt, **overrides))
        self.service._prompt_cache[
            self.service._prompt_cache_key(normalized, normalized["prompt"])
        ] = _FakeEmbeds(prompt)
        return normalized

    def _paths_without_encoder(self, **overrides: object) -> dict[str, object]:
        """`params()` with no text-encoder path: a machine that never downloaded one."""
        params = self.params(**overrides)
        params.pop("text_encoder_path")
        return params

    def _foreign_metadata(self, **overrides: str) -> dict[str, str]:
        """Metadata of a file built by ANOTHER encoder."""
        normalized = svc.normalize_flux2_klein_params(self.params(prompt="foreign"))
        metadata = svc.prompt_file_metadata(normalized, "foreign", "f" * 64, "other-ffffffff")
        metadata.update(overrides)
        return metadata

    # ---- save ----
    def test_save_writes_into_the_family_directory(self) -> None:
        self._seed_cache()
        out = self.service.prompt_cache_save(self.params(prompt="remove the text"), "убрать текст")
        dest = Path(out["path"])
        self.assertTrue(dest.is_file())
        self.assertEqual(dest.parent, self.library / "prompt_cache" / self.family)
        self.assertEqual(out["family"], self.family)
        self.assertEqual(out["name"], "убрать текст")

    def test_save_without_a_cached_prompt_says_what_to_do(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_save(self.params(prompt="never encoded"), "x")
        message = str(caught.exception)
        self.assertIn("не закодирован", message)
        self.assertIn("build", message)
        # Nothing was written: a failed save must not leave a stub behind.
        self.assertFalse((self.library / "prompt_cache").exists())

    def test_save_refuses_an_empty_prompt(self) -> None:
        with self.assertRaises(ValueError):
            self.service.prompt_cache_save(self.params(prompt="   "), "x")

    def test_a_name_collision_is_refused_unless_overwrite_was_asked_for(self) -> None:
        self._seed_cache()
        params = self.params(prompt="remove the text")
        first = self.service.prompt_cache_save(params, "preset")
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_save(params, "preset")
        self.assertIn("уже существует", str(caught.exception))
        again = self.service.prompt_cache_save(params, "preset", overwrite=True)
        self.assertEqual(first["path"], again["path"])

    def test_the_stored_name_is_the_sanitized_one(self) -> None:
        self._seed_cache()
        out = self.service.prompt_cache_save(self.params(prompt="remove the text"), "a/b")
        self.assertEqual(out["name"], "a_b")
        self.assertEqual(Path(out["path"]).parent.name, self.family)

    # ---- list ----
    def test_listing_reports_the_entries_of_this_family_only(self) -> None:
        self._seed_cache()
        self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        _write_prompt_cache_file(
            self.library / "prompt_cache" / "other-ffffffff" / "theirs.msprompt",
            self._foreign_metadata(),
        )
        out = self.service.prompt_cache_list(self.params())
        self.assertEqual(out["family"], self.family)
        self.assertEqual([entry["name"] for entry in out["entries"]], ["mine"])
        self.assertEqual(out["entries"][0]["prompt"], "remove the text")
        self.assertEqual(out["skipped"], [])

    def test_a_corrupt_file_is_skipped_instead_of_failing_the_listing(self) -> None:
        self._seed_cache()
        self.service.prompt_cache_save(self.params(prompt="remove the text"), "good")
        broken = self.library / "prompt_cache" / self.family / "broken.msprompt"
        broken.write_bytes(b"not safetensors at all")
        foreign = self.library / "prompt_cache" / self.family / "foreign.msprompt"
        foreign.write_bytes(_prompt_cache_bytes({"format": "someone.else"}))

        out = self.service.prompt_cache_list(self.params())
        self.assertEqual([entry["name"] for entry in out["entries"]], ["good"])
        self.assertEqual(
            sorted(entry["name"] for entry in out["skipped"]), ["broken", "foreign"]
        )
        self.assertTrue(all(entry["reason"] for entry in out["skipped"]))

    def test_an_empty_library_lists_nothing_rather_than_failing(self) -> None:
        out = self.service.prompt_cache_list(self.params())
        self.assertEqual(out["entries"], [])
        self.assertEqual(out["skipped"], [])

    def test_every_entry_names_the_family_it_sits_in(self) -> None:
        self._seed_cache()
        self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        out = self.service.prompt_cache_list(self.params())
        self.assertEqual([entry["family"] for entry in out["entries"]], [self.family])
        self.assertTrue(out["text_encoder_available"])

    def test_listing_without_an_encoder_spans_every_family(self) -> None:
        # Two families, one of them ours; without an encoder neither is "current",
        # so both must be listed or the entries that make an encoder-less machine
        # usable would be invisible.
        self._seed_cache()
        self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        _write_prompt_cache_file(
            self.library / "prompt_cache" / "other-ffffffff" / "theirs.msprompt",
            self._foreign_metadata(),
        )
        out = self.service.prompt_cache_list({})
        self.assertEqual(
            sorted((entry["family"], entry["name"]) for entry in out["entries"]),
            [("other-ffffffff", "theirs"), (self.family, "mine")],
        )
        # No family is active, and the directory is the library root.
        self.assertEqual(out["family"], "")
        self.assertEqual(out["directory"], str(self.library / "prompt_cache"))
        self.assertFalse(out["text_encoder_available"])

    def test_a_nonexistent_encoder_path_lists_like_an_absent_one(self) -> None:
        # What a settings file carried over from another machine looks like.
        self._seed_cache()
        self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        out = self.service.prompt_cache_list(
            self.params(text_encoder_path=str(self.root / "gone"))
        )
        self.assertEqual(out["family"], "")
        self.assertFalse(out["text_encoder_available"])
        self.assertEqual([entry["name"] for entry in out["entries"]], ["mine"])

    def test_a_corrupt_file_is_named_by_family_in_a_library_wide_listing(self) -> None:
        bad = self.library / "prompt_cache" / "other-ffffffff" / "broken.msprompt"
        bad.parent.mkdir(parents=True)
        bad.write_bytes(b"not safetensors at all")
        out = self.service.prompt_cache_list({})
        self.assertEqual(out["entries"], [])
        self.assertEqual(
            [(item["family"], item["name"]) for item in out["skipped"]],
            [("other-ffffffff", "broken")],
        )

    def test_saving_without_an_encoder_is_refused_with_the_reason(self) -> None:
        self._seed_cache()
        params = self.params(prompt="remove the text")
        params.pop("text_encoder_path")
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_save(params, "mine")
        message = str(caught.exception)
        self.assertIn("энкодер", message)
        # Saving records WHICH encoder built the entry, so a ready cache is no
        # substitute here and must not be offered as one.
        self.assertNotIn("prompt_cache.load", message)
        self.assertFalse((self.library / "prompt_cache").exists())

    def test_export_without_an_encoder_resolves_the_entry_across_families(self) -> None:
        self._seed_cache()
        saved = self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        dest = self.root / "outside" / "shared.msprompt"
        dest.parent.mkdir()
        out = self.service.prompt_cache_export({}, "mine", str(dest))
        self.assertEqual(out["family"], self.family)
        self.assertEqual(dest.read_bytes(), Path(saved["path"]).read_bytes())

    # ---- load without a local encoder ----
    def test_an_ambiguous_name_is_refused_instead_of_guessed(self) -> None:
        for family in (self.family, "other-ffffffff"):
            _write_prompt_cache_file(
                self.library / "prompt_cache" / family / "twin.msprompt",
                self._foreign_metadata(),
            )
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self._paths_without_encoder(prompt="x"), "twin")
        message = str(caught.exception)
        self.assertIn(self.family, message)
        self.assertIn("other-ffffffff", message)

    def test_a_missing_entry_without_an_encoder_names_the_library(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self._paths_without_encoder(), "absent")
        self.assertIn("absent", str(caught.exception))
        self.assertIn(str(self.library / "prompt_cache"), str(caught.exception))

    def test_without_an_encoder_the_settings_are_still_checked(self) -> None:
        # Everything except the fingerprint needs no encoder, so nothing except
        # the fingerprint may be waived: a file of another sequence length, dtype
        # or fp8 setting is still refused, and the tensor is never allocated.
        cases = {
            "max_sequence_length": {"max_sequence_length": "256"},
            "dtype": {"dtype": "float16"},
            "text_encoder_fp8": {"text_encoder_fp8": "true"},
        }
        for field, overrides in cases.items():
            with self.subTest(field=field):
                path = self.library / "prompt_cache" / "other-ffffffff" / f"{field}.msprompt"
                _write_prompt_cache_file(path, self._foreign_metadata(**overrides))
                with self.assertRaises(ValueError):
                    self.service.prompt_cache_load(self._paths_without_encoder(), field)

    def test_a_foreign_container_is_still_refused_without_an_encoder(self) -> None:
        path = self.library / "prompt_cache" / "other-ffffffff" / "alien.msprompt"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(_prompt_cache_bytes({"format": "something.else"}))
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self._paths_without_encoder(), "alien")
        self.assertIn(svc.PROMPT_CACHE_FORMAT, str(caught.exception))

    # ---- load refusals (the successful path needs torch; see the round trip) ----
    def test_loading_a_missing_entry_names_it(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(), "absent")
        self.assertIn("absent", str(caught.exception))

    def test_an_entry_of_another_encoder_is_refused(self) -> None:
        # The file sits in OUR family directory (a user copied it there by hand),
        # so only the recorded fingerprint can tell it apart.
        _write_prompt_cache_file(
            self.library / "prompt_cache" / self.family / "smuggled.msprompt",
            self._foreign_metadata(),
        )
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(), "smuggled")
        self.assertIn("другим текстовым энкодером", str(caught.exception))

    def test_another_sequence_length_is_refused(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params(max_sequence_length=256))
        metadata = svc.prompt_file_metadata(normalized, "p", self.encoder_id, self.family)
        _write_prompt_cache_file(
            self.library / "prompt_cache" / self.family / "short.msprompt", metadata
        )
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(max_sequence_length=512), "short")
        self.assertIn("max_sequence_length", str(caught.exception))

    def test_another_dtype_is_refused(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params(dtype="float16"))
        metadata = svc.prompt_file_metadata(normalized, "p", self.encoder_id, self.family)
        _write_prompt_cache_file(
            self.library / "prompt_cache" / self.family / "half.msprompt",
            metadata,
            dtype_token="F16",
        )
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(dtype="bfloat16"), "half")
        self.assertIn("типа данных", str(caught.exception))

    def test_metadata_that_lies_about_the_tensor_dtype_is_refused(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params(dtype="bfloat16"))
        metadata = svc.prompt_file_metadata(normalized, "p", self.encoder_id, self.family)
        _write_prompt_cache_file(
            self.library / "prompt_cache" / self.family / "lying.msprompt",
            metadata,
            dtype_token="F16",
        )
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(dtype="bfloat16"), "lying")
        self.assertIn("повреждён", str(caught.exception))

    def test_another_fp8_setting_is_refused(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params(text_encoder_fp8=True))
        metadata = svc.prompt_file_metadata(normalized, "p", self.encoder_id, self.family)
        _write_prompt_cache_file(
            self.library / "prompt_cache" / self.family / "quantized.msprompt", metadata
        )
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(text_encoder_fp8=False), "quantized")
        self.assertIn("fp8", str(caught.exception))

    def test_a_file_that_is_not_ours_is_refused_by_its_marker(self) -> None:
        path = self.library / "prompt_cache" / self.family / "alien.msprompt"
        _write_prompt_cache_file(path, {"format": "some.other.tool", "format_version": "1"})
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(), "alien")
        self.assertIn(svc.PROMPT_CACHE_FORMAT, str(caught.exception))

    def test_a_newer_format_version_is_refused_rather_than_guessed(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self.params())
        metadata = svc.prompt_file_metadata(normalized, "p", self.encoder_id, self.family)
        metadata["format_version"] = str(svc.PROMPT_CACHE_VERSION + 1)
        _write_prompt_cache_file(
            self.library / "prompt_cache" / self.family / "future.msprompt", metadata
        )
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_load(self.params(), "future")
        self.assertIn("не поддерживается", str(caught.exception))

    # ---- export / import ----
    def test_export_copies_the_entry_byte_for_byte(self) -> None:
        self._seed_cache()
        saved = self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        dest = self.root / "outside" / "shared.msprompt"
        dest.parent.mkdir()
        out = self.service.prompt_cache_export(self.params(), "mine", str(dest))
        self.assertEqual(out["size_bytes"], Path(saved["path"]).stat().st_size)
        self.assertEqual(dest.read_bytes(), Path(saved["path"]).read_bytes())

    def test_export_refuses_a_foreign_suffix(self) -> None:
        self._seed_cache()
        self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_export(
                self.params(), "mine", str(self.root / "shared.txt")
            )
        self.assertIn(svc.PROMPT_CACHE_SUFFIX, str(caught.exception))

    def test_export_refuses_a_relative_path(self) -> None:
        self._seed_cache()
        self.service.prompt_cache_save(self.params(prompt="remove the text"), "mine")
        with self.assertRaises(ValueError):
            self.service.prompt_cache_export(self.params(), "mine", "shared.msprompt")

    def test_import_files_a_foreign_entry_under_its_own_family(self) -> None:
        outside = self.root / "outside"
        outside.mkdir()
        source = outside / "theirs.msprompt"
        _write_prompt_cache_file(source, self._foreign_metadata())

        out = self.service.prompt_cache_import(self.params(), str(source))
        self.assertEqual(out["family"], "other-ffffffff")
        self.assertFalse(out["family_matches"])
        self.assertEqual(out["current_family"], self.family)
        self.assertEqual(
            Path(out["path"]), self.library / "prompt_cache" / "other-ffffffff" / "theirs.msprompt"
        )
        # It is NOT visible to the current encoder, which is the point of filing
        # it under its own family rather than the selected one.
        self.assertEqual(self.service.prompt_cache_list(self.params())["entries"], [])

    def test_import_of_our_own_family_matches(self) -> None:
        outside = self.root / "outside"
        outside.mkdir()
        normalized = svc.normalize_flux2_klein_params(self.params(prompt="mine"))
        metadata = svc.prompt_file_metadata(normalized, "mine", self.encoder_id, self.family)
        source = outside / "mine.msprompt"
        _write_prompt_cache_file(source, metadata)

        out = self.service.prompt_cache_import(self.params(), str(source), name="взято")
        self.assertTrue(out["family_matches"])
        self.assertEqual(out["name"], "взято")
        self.assertEqual(
            [entry["name"] for entry in self.service.prompt_cache_list(self.params())["entries"]],
            ["взято"],
        )

    def test_import_refuses_a_file_that_is_not_ours(self) -> None:
        outside = self.root / "outside"
        outside.mkdir()
        source = outside / "alien.msprompt"
        _write_prompt_cache_file(source, {"format": "some.other.tool"})
        with self.assertRaises(ValueError):
            self.service.prompt_cache_import(self.params(), str(source))
        self.assertFalse((self.library / "prompt_cache").exists())

    def test_import_refuses_a_file_without_a_family(self) -> None:
        outside = self.root / "outside"
        outside.mkdir()
        metadata = self._foreign_metadata()
        metadata.pop("text_encoder_family")
        source = outside / "orphan.msprompt"
        _write_prompt_cache_file(source, metadata)
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_import(self.params(), str(source))
        self.assertIn("text_encoder_family", str(caught.exception))

    def test_import_works_without_a_configured_encoder(self) -> None:
        # Setting a machine up from someone else's files: nothing is selected yet.
        outside = self.root / "outside"
        outside.mkdir()
        source = outside / "theirs.msprompt"
        _write_prompt_cache_file(source, self._foreign_metadata())
        out = self.service.prompt_cache_import({}, str(source))
        self.assertEqual(out["current_family"], "")
        self.assertFalse(out["family_matches"])

    def test_import_honours_the_collision_rule(self) -> None:
        outside = self.root / "outside"
        outside.mkdir()
        source = outside / "theirs.msprompt"
        _write_prompt_cache_file(source, self._foreign_metadata())
        self.service.prompt_cache_import(self.params(), str(source))
        with self.assertRaises(ValueError):
            self.service.prompt_cache_import(self.params(), str(source))
        self.service.prompt_cache_import(self.params(), str(source), overwrite=True)

    def test_import_refuses_a_missing_source(self) -> None:
        with self.assertRaises(ValueError):
            self.service.prompt_cache_import(self.params(), str(self.root / "nope.msprompt"))

    def test_a_family_name_from_a_file_cannot_escape_the_library(self) -> None:
        outside = self.root / "outside"
        outside.mkdir()
        metadata = self._foreign_metadata(text_encoder_family="../../escaped")
        source = outside / "evil.msprompt"
        _write_prompt_cache_file(source, metadata)
        out = self.service.prompt_cache_import(self.params(), str(source))
        written = Path(out["path"]).resolve()
        self.assertTrue(written.is_relative_to((self.library / "prompt_cache").resolve()))


class PromptCacheRoundTripTests(_TempTreeCase):
    """The real serialization: a torch tensor out to disk and back.

    Gated on torch because `safetensors.torch` is what writes and reads the
    tensor; everything ABOUT the file — its metadata, its layout, its refusals —
    is covered torch-free above.
    """

    def setUp(self) -> None:
        super().setUp()
        if importlib.util.find_spec("torch") is None:  # pragma: no cover - host-dependent
            self.skipTest("torch is not installed")
        self.library = self.root / "program"
        self.library.mkdir()
        root_patch = patch.object(svc, "program_root", lambda: self.library)
        root_patch.start()
        self.addCleanup(root_patch.stop)
        self.service = svc.Flux2KleinInpaintService(LoadedModelManager())

    def test_a_saved_entry_loads_back_into_the_cache_unchanged(self) -> None:
        import torch

        normalized = svc.normalize_flux2_klein_params(self.params(prompt="remove the text"))
        embeds = torch.arange(32, dtype=torch.bfloat16).reshape(1, 4, 8)
        key = self.service._prompt_cache_key(normalized, normalized["prompt"])
        self.service._prompt_cache[key] = embeds

        saved = self.service.prompt_cache_save(
            self.params(prompt="remove the text"), "круглый рейс"
        )
        self.assertGreater(saved["size_bytes"], 0)

        # A fresh service: nothing in memory, everything from the file.
        other = svc.Flux2KleinInpaintService(LoadedModelManager())
        loaded = other.prompt_cache_load(self.params(prompt="ignored"), "круглый рейс")
        self.assertEqual(loaded["prompt"], "remove the text")
        self.assertTrue(loaded["prompt_cached"])
        restored = other._prompt_cache[key]
        self.assertEqual(restored.dtype, torch.bfloat16)
        self.assertTrue(torch.equal(restored, embeds))
        # The load answers for the prompt IN THE FILE, under the shared key.
        self.assertTrue(other._prompt_cached(self.params(prompt="remove the text")))

    def test_the_fingerprint_is_verified_when_the_encoder_is_there(self) -> None:
        import torch

        normalized = svc.normalize_flux2_klein_params(self.params(prompt="p"))
        self.service._prompt_cache[self.service._prompt_cache_key(normalized, "p")] = torch.zeros(
            (1, 2, 4), dtype=torch.bfloat16
        )
        self.service.prompt_cache_save(self.params(prompt="p"), "entry")
        loaded = self.service.prompt_cache_load(self.params(), "entry")
        self.assertTrue(loaded["encoder_verified"])

    def test_an_entry_loads_on_a_machine_that_has_no_encoder(self) -> None:
        # The scenario the format exists for: the `.msprompt` travels, the 16 GB
        # Qwen3 does not. The denoise and the VAE decode never look at the
        # encoder, so the only thing its absence costs is the fingerprint check —
        # and the answer says so instead of pretending it happened.
        import torch

        normalized = svc.normalize_flux2_klein_params(self.params(prompt="remove the text"))
        embeds = torch.arange(32, dtype=torch.bfloat16).reshape(1, 4, 8)
        key = self.service._prompt_cache_key(normalized, "remove the text")
        self.service._prompt_cache[key] = embeds
        saved = self.service.prompt_cache_save(self.params(prompt="remove the text"), "перенос")

        other = svc.Flux2KleinInpaintService(LoadedModelManager())
        params = dict(self.params())
        params.pop("text_encoder_path")
        loaded = other.prompt_cache_load(params, "перенос")

        self.assertFalse(loaded["encoder_verified"])
        self.assertEqual(loaded["family"], Path(saved["path"]).parent.name)
        self.assertEqual(loaded["prompt"], "remove the text")
        # The embedding landed under the key a run on THIS machine looks up, so
        # the generation that follows needs no encoder either.
        self.assertTrue(other._prompt_cached({**params, "prompt": "remove the text"}))
        restored = other._prompt_cache[
            other._prompt_cache_key(
                svc.normalize_flux2_klein_params({**params, "prompt": "remove the text"}),
                "remove the text",
            )
        ]
        self.assertTrue(torch.equal(restored, embeds))

    def test_a_nonexistent_encoder_path_loads_the_same_way(self) -> None:
        import torch

        normalized = svc.normalize_flux2_klein_params(self.params(prompt="p"))
        self.service._prompt_cache[self.service._prompt_cache_key(normalized, "p")] = torch.zeros(
            (1, 2, 4), dtype=torch.bfloat16
        )
        self.service.prompt_cache_save(self.params(prompt="p"), "entry")
        other = svc.Flux2KleinInpaintService(LoadedModelManager())
        loaded = other.prompt_cache_load(
            self.params(text_encoder_path=str(self.root / "no-such-encoder")), "entry"
        )
        self.assertFalse(loaded["encoder_verified"])

    def test_the_written_file_is_a_readable_container(self) -> None:
        import torch

        normalized = svc.normalize_flux2_klein_params(self.params(prompt="p"))
        self.service._prompt_cache[self.service._prompt_cache_key(normalized, "p")] = torch.zeros(
            (1, 2, 4), dtype=torch.bfloat16
        )
        saved = self.service.prompt_cache_save(self.params(prompt="p"), "entry")
        metadata, tensor = svc.read_prompt_file_header(Path(saved["path"]))
        self.assertEqual(metadata["format"], svc.PROMPT_CACHE_FORMAT)
        self.assertEqual(metadata["prompt"], "p")
        self.assertEqual(tensor["dtype"], "BF16")
        self.assertEqual(tensor["shape"], [1, 2, 4])


class PromptCacheBuildTests(_PlacementFixture):
    """`prompt_cache.build`: encode the prompt, load nothing else, let go again."""

    def setUp(self) -> None:
        super().setUp()
        snapshot = {
            "ram_free": 64 * 1024**3,
            "ram_total": 64 * 1024**3,
            "vram_free": 32 * 1024**3,
            "vram_total": 32 * 1024**3,
        }
        memory_patch = patch.object(svc, "memory_snapshot", lambda _device=None: snapshot)
        memory_patch.start()
        self.addCleanup(memory_patch.stop)

    def test_it_encodes_the_prompt_without_building_a_pipeline(self) -> None:
        out = self.service.prompt_cache_build(self.params(prompt="a cat"))
        self.assertTrue(out["encoded"])
        self.assertTrue(out["prompt_cached"])
        self.assertEqual([call["prompt"] for call in self.encode_calls], ["a cat"])
        # The 9B transformer takes no part in a prompt.
        self.assertIsNone(self.service._pipe)
        self.assertNotIn("transformer", self.load_kwargs)
        self.assertNotIn("vae", self.load_kwargs)

    def test_it_releases_the_encoder_it_loaded(self) -> None:
        # The whole point of the button: cache the prompt so the 16 GB encoder
        # does not have to stay.
        self.service.prompt_cache_build(self.params(prompt="a cat"))
        self.assertIsNone(self.service._text_encoder)

    def test_an_encoder_that_was_already_resident_is_left_alone(self) -> None:
        self._encode(prompt="first")  # a normal run keeps the encoder by default
        self.assertIsNotNone(self.service._text_encoder)
        self.service.prompt_cache_build(self.params(prompt="second"))
        self.assertIsNotNone(self.service._text_encoder)

    def test_a_second_build_of_the_same_prompt_reads_nothing(self) -> None:
        self.service.prompt_cache_build(self.params(prompt="a cat"))
        self.load_kwargs.clear()
        out = self.service.prompt_cache_build(self.params(prompt="a cat"))
        self.assertFalse(out["encoded"])
        self.assertNotIn("text_encoder", self.load_kwargs)

    def test_an_empty_prompt_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            self.service.prompt_cache_build(self.params(prompt="   "))

    def test_the_status_flag_follows_the_shared_key(self) -> None:
        with patch.object(svc, "is_torch_available", lambda: True):
            self.service.prompt_cache_build(self.params(prompt="a cat"))
            self.assertTrue(self.service.status(self.params(prompt="a cat"))["prompt_cached"])
            self.assertFalse(self.service.status(self.params(prompt="a dog"))["prompt_cached"])
            # Same prompt, another sequence length: a different embedding.
            self.assertFalse(
                self.service.status(self.params(prompt="a cat", max_sequence_length=256))[
                    "prompt_cached"
                ]
            )
            self.assertFalse(self.service.status(self.params(prompt="  "))["prompt_cached"])
            self.assertFalse(self.service.status(None)["prompt_cached"])

    def test_it_streams_the_prompt_phase_steps(self) -> None:
        frames: list[tuple[str, int, int, str]] = []
        self.service.prompt_cache_build(
            self.params(prompt="a cat"), progress_callback=lambda *frame: frames.append(frame)
        )
        self.assertTrue(all(phase == "load" for phase, _s, _t, _l in frames))
        steps = [step for _p, step, _t, _l in frames]
        self.assertIn(svc.LOAD_STEP_TEXT_ENCODER, steps)
        self.assertIn(svc.LOAD_STEP_ENCODE, steps)
        self.assertEqual(steps, sorted(steps))

    def test_the_memory_gate_charges_only_for_the_encoder(self) -> None:
        # A build must not demand the transformer's 18 GB of VRAM: it never
        # loads one. `encode_standalone` is what the guard checks.
        sizes = {
            self.paths["transformer_path"]: 18_157_185_168,
            self.paths["text_encoder_path"]: 16_381_516_808,
            self.paths["vae_path"]: 168_120_878,
        }
        with patch.object(svc, "_weight_bytes", lambda path: sizes.get(path, 0)):
            forecast = svc.forecast_memory(
                svc.normalize_flux2_klein_params(self.params()), 384, 384
            )
            phase = forecast["phases"]["encode_standalone"]
            self.assertEqual(phase["vram_bytes"], 0)
            self.assertLess(phase["ram_bytes"], forecast["phases"]["encode"]["ram_bytes"] + 1)
            self.assertGreaterEqual(phase["ram_bytes"], sizes[self.paths["text_encoder_path"]])
            # Adding the phase must not have moved the numbers the UI shows.
            self.assertEqual(
                forecast["vram_bytes"],
                max(
                    forecast["phases"][name]["vram_bytes"]
                    for name in ("encode", "denoise", "decode")
                ),
            )

    def test_a_host_short_of_memory_refuses_the_build_before_reading_anything(self) -> None:
        sizes = {self.paths["text_encoder_path"]: 16_381_516_808}
        snapshot = {
            "ram_free": 4 * 1024**3,
            "ram_total": 64 * 1024**3,
            "vram_free": 32 * 1024**3,
            "vram_total": 32 * 1024**3,
        }
        with patch.object(svc, "_weight_bytes", lambda path: sizes.get(path, 0)):
            with patch.object(svc, "memory_snapshot", lambda _device=None: snapshot):
                with self.assertRaises(RuntimeError) as caught:
                    self.service.prompt_cache_build(self.params(prompt="a cat"))
        self.assertIn("кэширование", str(caught.exception))
        self.assertNotIn("text_encoder", self.load_kwargs)


class NoTextEncoderTests(_PlacementFixture):
    """A machine where the 16 GB Qwen3 was never downloaded.

    The contract: a cached prompt generates (the denoise and the VAE decode never
    look at the encoder), and everything that would have to ENCODE is refused
    with a message naming both ways out.
    """

    def setUp(self) -> None:
        super().setUp()
        snapshot = {
            "ram_free": 64 * 1024**3,
            "ram_total": 64 * 1024**3,
            "vram_free": 32 * 1024**3,
            "vram_total": 32 * 1024**3,
        }
        memory_patch = patch.object(svc, "memory_snapshot", lambda _device=None: snapshot)
        memory_patch.start()
        self.addCleanup(memory_patch.stop)

    def _params(self, **overrides: object) -> dict[str, object]:
        params = self.params(**overrides)
        params.pop("text_encoder_path")
        return params

    def _cache(self, params: dict[str, object]) -> dict[str, Any]:
        normalized = svc.normalize_flux2_klein_params(params)
        self.service._prompt_cache[
            self.service._prompt_cache_key(normalized, normalized["prompt"])
        ] = _FakeEmbeds(str(normalized["prompt"]))
        return normalized

    def test_an_absent_encoder_path_normalizes_instead_of_raising(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self._params(prompt="a cat"))
        self.assertEqual(normalized["text_encoder_path"], "")
        # The other two paths are still mandatory: nothing can replace them.
        for key in ("transformer_path", "vae_path"):
            with self.subTest(key=key):
                params = self._params()
                params.pop(key)
                with self.assertRaises(ValueError):
                    svc.normalize_flux2_klein_params(params)

    def test_a_path_that_is_not_on_disk_counts_as_no_encoder(self) -> None:
        params = self.params(text_encoder_path=str(self.root / "gone"))
        self.assertFalse(svc.text_encoder_available(svc.normalize_flux2_klein_params(params)))
        self.assertIsNone(svc.local_encoder_identity(str(self.root / "gone")))
        configured = svc.normalize_flux2_klein_params(self.params())
        self.assertTrue(svc.text_encoder_available(configured))

    def test_an_encoder_that_exists_but_cannot_be_identified_is_still_an_error(self) -> None:
        # Degrading a broken checkout into "no encoder" would hide it behind a
        # silently weaker check; it stays the error it always was.
        (self.root / "text_encoder" / "config.json").unlink()
        with self.assertRaises(ValueError):
            svc.local_encoder_identity(self.paths["text_encoder_path"])

    def test_a_cached_prompt_is_served_without_touching_the_encoder(self) -> None:
        normalized = self._cache(self._params(prompt="a cat"))
        embeds = self.service._prompt_embeds_locked(normalized, lambda _step, _label: None)
        self.assertEqual(embeds["prompt"].text, "a cat")
        self.assertIsNone(embeds["negative"])
        self.assertNotIn("text_encoder", self.load_kwargs)
        self.assertEqual(self.encode_calls, [])

    def test_an_uncached_prompt_is_refused_naming_both_ways_out(self) -> None:
        normalized = svc.normalize_flux2_klein_params(self._params(prompt="a cat"))
        with self.assertRaises(ValueError) as caught:
            self.service._prompt_embeds_locked(normalized, lambda _step, _label: None)
        message = str(caught.exception)
        self.assertIn("энкодер", message)
        self.assertIn("кэш", message)
        self.assertNotIn("text_encoder", self.load_kwargs)

    def test_the_refusal_names_a_configured_path_that_is_missing(self) -> None:
        missing = str(self.root / "gone")
        normalized = svc.normalize_flux2_klein_params(
            self.params(prompt="a cat", text_encoder_path=missing)
        )
        with self.assertRaises(ValueError) as caught:
            self.service._prompt_embeds_locked(normalized, lambda _step, _label: None)
        self.assertIn(missing, str(caught.exception))

    def test_a_run_with_an_uncached_prompt_loads_nothing_at_all(self) -> None:
        # The refusal has to happen BEFORE the 18 GB transformer is read: loading
        # a pipeline for a run that cannot finish is the cost this check avoids.
        region = np.full((128, 128, 3), 90, dtype=np.uint8)
        mask = np.zeros((128, 128), dtype=np.uint8)
        mask[48:80, 48:80] = 255
        with self.assertRaises(ValueError):
            self.service.inpaint_image_bytes(
                _png_bytes(region, "RGB"),
                _png_bytes(mask, "L"),
                params=self._params(prompt="a cat"),
            )
        self.assertEqual(self.load_kwargs, {})
        self.assertIsNone(self.service._pipe)

    def test_a_run_with_a_cached_prompt_gets_past_the_check(self) -> None:
        # Only the encoder gate is under test here, so the run is stopped right
        # after it by a pipeline build that refuses to happen — what matters is
        # WHICH error comes back, and that it is no longer the encoder one.
        self._cache(self._params(prompt="a cat"))
        region = np.full((128, 128, 3), 90, dtype=np.uint8)
        mask = np.zeros((128, 128), dtype=np.uint8)
        mask[48:80, 48:80] = 255
        def _reached(*_args: object, **_kwargs: object) -> object:
            raise RuntimeError("pipeline reached")

        with patch.object(self.service, "_ensure_pipeline_locked", _reached):
            with self.assertRaises(RuntimeError) as caught:
                self.service.inpaint_image_bytes(
                    _png_bytes(region, "RGB"),
                    _png_bytes(mask, "L"),
                    params=self._params(prompt="a cat"),
                )
        self.assertEqual(str(caught.exception), "pipeline reached")

    def test_build_without_an_encoder_is_refused(self) -> None:
        with self.assertRaises(ValueError) as caught:
            self.service.prompt_cache_build(self._params(prompt="a cat"))
        self.assertIn("энкодер", str(caught.exception))
        self.assertNotIn("text_encoder", self.load_kwargs)

    def test_build_of_an_already_cached_prompt_needs_no_encoder(self) -> None:
        # Nothing has to be encoded, so nothing has to be refused.
        self._cache(self._params(prompt="a cat"))
        out = self.service.prompt_cache_build(self._params(prompt="a cat"))
        self.assertFalse(out["encoded"])
        self.assertTrue(out["prompt_cached"])
        self.assertNotIn("text_encoder", self.load_kwargs)


if __name__ == "__main__":
    unittest.main()
