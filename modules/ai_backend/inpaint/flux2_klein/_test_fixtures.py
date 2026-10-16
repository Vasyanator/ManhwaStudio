"""
File: modules/ai_backend/inpaint/flux2_klein/_test_fixtures.py

Purpose:
Shared fixtures for the FLUX.2 klein test modules: the on-disk model tree the
path-based tests need (header-only weights by default, tiny REAL weights on
request) and `_ManagedRunFixture`, a service over a REAL large-model manager on
the CPU.

Key declarations:
- `_make_model_tree` / `_write_header_only` / `_write_real_weights` /
  `_write_safetensors` / `_png_bytes` - on-disk inputs (`_write_safetensors`
  writes a raw header dict, for malformed / fp8 containers).
- `VAE_SAMPLE_SIZE` / `VAE_BLOCK_OUT_CHANNELS` and the thresholds derived from
  them - the fixture VAE's tiling geometry, written into its `config.json` and
  DERIVED exactly as diffusers derives them (never set independently).
- `TINY_TRANSFORMER_CONFIG` / `TINY_VAE_CONFIG` / `TINY_ENCODER_CONFIG` - the tiny
  real models (the encoder has exactly `ENCODER_KEEP_LAYERS` layers and a width
  that matches the transformer, so the variant guard passes).
- `DEFAULT_MODEL_INDEX` - the `model_index.json` the tree carries by default, kept
  faithful to the shipped klein 4B one (`is_distilled: true`). Pass another dict,
  or `None` for no manifest at all, to build an undeclared checkpoint.
- `_TempTreeCase` - a `TestCase` with a complete model tree in a temp directory
  (`real_weights = True` in a subclass for the tiny real models).
- `_FakeEmbeds` - a prompt-embedding stand-in for the library tests.
- `_ManagedRunFixture` (+ `_RoomyProbes`, `_FakeRunPipeline`, `_ImageProcessor`,
  `_flat_tables`) - real manager, arbiter, small pool, torch and model classes;
  fake pipeline call, tokenizer, budget probes and transport device.

Notes:
- The leading underscore keeps pytest from collecting this file as a test module.
- Module attributes are patched on the module that DEFINES them (`hardware`,
  `memory`, ...), never on the `flux2_klein` package: the package's re-export is
  a separate binding, so patching it would not reach the real callers.
"""

from __future__ import annotations

import json
import struct
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch

import numpy as np
from PIL import Image

from modules.ai_backend.inpaint import flux2_klein as svc
from modules.ai_backend.inpaint.flux2_klein import hardware
from modules.ai_backend.runtime.model_manager import SmallModelManager


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


#: Header-only tensors of the default tree (`(dtype, shape)`, no data bytes):
#: valid containers the manager's inventory can read, in each component's
#: real key layout (diffusers transformer, Qwen3 causal-LM encoder, diffusers VAE).
_HEADER_ONLY_TENSORS: dict[str, dict[str, tuple[str, tuple[int, ...]]]] = {
    "transformer": {"x_embedder.weight": ("BF16", (8, 8)), "transformer_blocks.0.attn.to_q.weight": ("BF16", (8, 8)), "single_transformer_blocks.0.attn.to_out.weight": ("BF16", (8, 8))},
    "text_encoder": {"model.embed_tokens.weight": ("BF16", (16, 8)), "model.layers.0.self_attn.q_proj.weight": ("BF16", (8, 8)), "model.norm.weight": ("BF16", (8,)), "lm_head.weight": ("BF16", (16, 8))},
    "vae": {"decoder.conv_in.weight": ("BF16", (4, 4))},
}


def _write_header_only(path: Path, tensors: dict[str, tuple[str, tuple[int, ...]]]) -> None:
    """A valid safetensors HEADER for `tensors` (back to back offsets) with no data bytes."""
    item = {"BF16": 2, "F16": 2, "F32": 4, "I64": 8}
    header: dict[str, object] = {}
    offset = 0
    for name, (dtype, shape) in tensors.items():
        length = item[dtype]
        for side in shape:
            length *= side
        header[name] = {"dtype": dtype, "shape": list(shape), "data_offsets": [offset, offset + length]}
        offset += length
    payload = json.dumps(header).encode("utf-8")
    path.write_bytes(struct.pack("<Q", len(payload)) + payload)


def _make_model_tree(
    root: Path,
    *,
    single_file_transformer: bool = False,
    model_index: dict[str, object] | None = DEFAULT_MODEL_INDEX,
    real_weights: bool = False,
) -> dict[str, str]:
    """Lay out a klein-like checkout and return the three user-supplied paths.

    `model_index` is the manifest written at the checkout root; pass `None` to
    build a tree with no `model_index.json` at all, which is what a
    hand-assembled folder of components looks like. The weights are
    header-only containers by default (enough for every header reader and the
    manager's inventory); `real_weights=True` (needs torch, diffusers and
    transformers) writes tiny REAL models with their configs instead
    (`_write_real_weights`), which the manager can load and run on the CPU.
    """
    if model_index is not None:
        root.mkdir(parents=True, exist_ok=True)
        (root / "model_index.json").write_text(json.dumps(model_index), encoding="utf-8")
    (root / "text_encoder").mkdir(parents=True)
    (root / "text_encoder" / "config.json").write_text("{}", encoding="utf-8")
    _write_header_only(root / "text_encoder" / "model.safetensors", _HEADER_ONLY_TENSORS["text_encoder"])
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
    _write_header_only(root / "vae" / "diffusion_pytorch_model.safetensors", _HEADER_ONLY_TENSORS["vae"])
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
        _write_header_only(transformer / "diffusion_pytorch_model.safetensors", _HEADER_ONLY_TENSORS["transformer"])

    if real_weights:
        if single_file_transformer:
            raise ValueError("real_weights: the tiny real tree is a diffusers folder layout")
        _write_real_weights(root)
    return {
        "text_encoder_path": str(root / "text_encoder"),
        "transformer_path": str(transformer),
        "vae_path": str(root / "vae"),
    }


#: Tiny real configs of `_write_real_weights` (CPU-fast; the VAE keeps the
#: shipped tiling geometry: four `block_out_channels`, `sample_size` 1024).
TINY_TRANSFORMER_CONFIG: dict[str, object] = {
    "_class_name": "Flux2Transformer2DModel",
    "in_channels": 8,
    "num_layers": 1,
    "num_single_layers": 2,
    "attention_head_dim": 16,
    "num_attention_heads": 2,
    "joint_attention_dim": 24,
    "axes_dims_rope": [4, 4, 4, 4],
    "timestep_guidance_channels": 32,
    "guidance_embeds": False,
}
TINY_VAE_CONFIG: dict[str, object] = {
    "_class_name": "AutoencoderKLFlux2",
    "block_out_channels": [8, 8, 8, 8],
    "latent_channels": 4,
    "norm_num_groups": 4,
    "layers_per_block": 1,
    "sample_size": VAE_SAMPLE_SIZE,
}
#: 28 decoder layers: exactly `ENCODER_KEEP_LAYERS`, the minimum the pipeline reads.
TINY_ENCODER_CONFIG: dict[str, object] = {
    "architectures": ["Qwen3ForCausalLM"],
    "model_type": "qwen3",
    # 3 x 8 = the tiny transformer's `joint_attention_dim` (the variant guard).
    "hidden_size": 8,
    "num_attention_heads": 2,
    "num_key_value_heads": 1,
    "head_dim": 4,
    "intermediate_size": 16,
    "num_hidden_layers": 28,
    "vocab_size": 64,
}


def _write_real_weights(root: Path) -> None:
    """Overwrite the tree's three weight files and configs with tiny REAL models (seeded, CPU).

    The transformer is written in the diffusers layout, the encoder as a
    causal-LM checkpoint (`model.` prefix plus an `lm_head`) the way the
    shipped one is, the VAE in its folder.
    """
    import torch
    from diffusers import AutoencoderKLFlux2, Flux2Transformer2DModel
    from safetensors.torch import save_file
    from transformers import Qwen3Config, Qwen3Model

    torch.manual_seed(0)
    transformer = Flux2Transformer2DModel.from_config({key: value for key, value in TINY_TRANSFORMER_CONFIG.items() if key != "_class_name"})
    save_file({key: value.to(torch.bfloat16).contiguous() for key, value in transformer.state_dict().items()}, str(root / "transformer" / "diffusion_pytorch_model.safetensors"))
    (root / "transformer" / "config.json").write_text(json.dumps(TINY_TRANSFORMER_CONFIG), encoding="utf-8")
    vae = AutoencoderKLFlux2.from_config({key: value for key, value in TINY_VAE_CONFIG.items() if key != "_class_name"})
    save_file({key: (value.to(torch.bfloat16) if value.is_floating_point() else value).contiguous() for key, value in vae.state_dict().items()}, str(root / "vae" / "diffusion_pytorch_model.safetensors"))
    (root / "vae" / "config.json").write_text(json.dumps(TINY_VAE_CONFIG), encoding="utf-8")
    encoder = Qwen3Model(Qwen3Config(**{key: value for key, value in TINY_ENCODER_CONFIG.items() if key not in ("architectures", "model_type")}))
    tensors = {f"model.{key}": value.to(torch.float16).contiguous() for key, value in encoder.state_dict().items()}
    tensors["lm_head.weight"] = torch.zeros((int(TINY_ENCODER_CONFIG["vocab_size"]), int(TINY_ENCODER_CONFIG["hidden_size"])), dtype=torch.float16)
    save_file(tensors, str(root / "text_encoder" / "model.safetensors"))
    (root / "text_encoder" / "config.json").write_text(json.dumps(TINY_ENCODER_CONFIG), encoding="utf-8")


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
    #: Tiny REAL weights instead of header-only files (`_write_real_weights`).
    real_weights = False
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
            real_weights=self.real_weights,
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
# Stand-ins
# ---------------------------------------------------------------------------
class _FakeEmbeds:
    """Prompt-embedding stand-in: records every device it was moved to.

    `_encode_prompt_phase` calls `.detach().to("cpu")` positionally and
    `_generate_locked` calls `.to(device=...)` by keyword, so both spellings are
    accepted.
    """

    def __init__(self, text: str = "", text_tokens: int = 512) -> None:
        self.text = text
        #: `(batch, text tokens, width)`: `_text_attention_mask_locked` reads the
        #: text length from `shape[1]`, exactly as from a real tensor.
        self.shape = (1, int(text_tokens), 8)
        self.moves: list[object] = []

    def detach(self) -> "_FakeEmbeds":
        return self

    def to(self, device: object = None, **kwargs: object) -> "_FakeEmbeds":
        self.moves.append(device if device is not None else kwargs.get("device"))
        return self


# ---------------------------------------------------------------------------
# A real run through the large-model manager, on the CPU
# ---------------------------------------------------------------------------
GIB = 1024**3


class _RoomyProbes:
    """Budget probes of an ample machine (64 GiB device, 64 GiB host), so every plan fits."""

    def __init__(self, *, device_total: int = 64 * GIB, host_available: int = 64 * GIB) -> None:
        self.device_total = device_total
        self.host_available = host_available

    def host(self) -> object:
        from modules.ai_backend.runtime.memory_budget import HostBudget

        return HostBudget(total=128 * GIB, available=self.host_available, cgroup_limit=None, cgroup_used=None, rss_anon=1 << 30, rss_shmem=0, reserve=0)

    def device(self, device: str) -> object:
        from modules.ai_backend.runtime.memory_budget import DeviceBudget

        return DeviceBudget(device=device, total=self.device_total, free=self.device_total, torch_allocated=0, torch_reserved=0, non_torch=0, reserve=0, cap=None)


class _FakeRunPipeline:
    """`Flux2KleinInpaintPipeline` stand-in: the diffusers CALL is the only faked part of a managed run.

    It keeps the components it is built with (the manager's real tiny modules),
    returns zero embeddings of `max_sequence_length` tokens from `encode_prompt`
    and, called, zero latents of the region's latent size, which the real tiny
    VAE then decodes (`calls` / `encode_calls` record the kwargs).
    """

    calls: list[dict[str, object]] = []
    encode_calls: list[dict[str, object]] = []

    def __init__(self, **components: object) -> None:
        self.__dict__.update(components)
        self._attention_kwargs = None

    def set_progress_bar_config(self, **_kwargs: object) -> None:
        return None

    def encode_prompt(self, **kwargs: object) -> tuple[object, object]:
        import torch

        type(self).encode_calls.append(kwargs)
        return torch.zeros((1, int(kwargs["max_sequence_length"]), 24)), None

    def __call__(self, **kwargs: object) -> object:
        """Record the call, report every effective step, return zero latents of the region's latent size."""
        import torch

        type(self).calls.append(kwargs)
        self._attention_kwargs = kwargs.get("attention_kwargs")
        steps = svc.effective_steps(int(kwargs["num_inference_steps"]), float(kwargs["strength"]))  # type: ignore[arg-type]
        for step in range(steps):
            kwargs["callback_on_step_end"](self, step, None, {})  # type: ignore[operator]
        height, width = int(kwargs["height"]), int(kwargs["width"])  # type: ignore[arg-type]
        channels = int(TINY_VAE_CONFIG["latent_channels"])  # type: ignore[arg-type]
        return types.SimpleNamespace(images=torch.zeros((1, channels, height // VAE_LATENT_SCALE, width // VAE_LATENT_SCALE)))


class _ImageProcessor:
    """Turns the real VAE's output into a solid PIL image of the decoded size (value `value`)."""

    def __init__(self, value: int = 210) -> None:
        self.value = value

    def postprocess(self, image: object, output_type: str = "pil") -> list[object]:
        assert output_type == "pil"
        height, width = int(image.shape[-2]), int(image.shape[-1])  # type: ignore[attr-defined]
        return [Image.new("RGB", (width, height), (self.value,) * 3)]


class _ManagedRunFixture(_TempTreeCase):
    """A service over a REAL large-model manager on the CPU, with tiny real weights on disk.

    Real: the manager (planner, loader, ledger, moves, hooks), the arbiter, the
    small pool, torch, the diffusers transformer / VAE classes and the
    transformers Qwen3 class. Faked: the diffusers pipeline CALL
    (`_FakeRunPipeline`), the Qwen tokenizer, the budget probes (`_RoomyProbes`)
    and the transport's device (every "cuda:0" unit lives on the CPU). The run
    device label is `cuda:0`.
    """

    real_weights = True

    def setUp(self) -> None:
        try:
            import diffusers  # noqa: F401 - availability probe
            import torch  # noqa: F401 - availability probe
            import transformers  # noqa: F401 - availability probe
        except ImportError as exc:  # pragma: no cover - the backend may run without torch
            self.skipTest(f"needs torch, diffusers and transformers ({exc})")
        super().setUp()
        import diffusers
        import transformers

        from modules.ai_backend.runtime.large_models.loader import TorchTransport
        from modules.ai_backend.runtime.large_models.manager import LargeModelManager, MemoryLimitsStore
        from modules.ai_backend.runtime.residency.arbiter import ResidencyArbiter

        self.pipeline_cls = type("FakeRunPipeline", (_FakeRunPipeline,), {"calls": [], "encode_calls": [], "image_processor": _ImageProcessor()})
        self.tokenizer_loads: list[str] = []
        tokenizer_loads = self.tokenizer_loads

        class _Tokenizer:
            @staticmethod
            def from_pretrained(path: str, **_kwargs: object) -> object:
                tokenizer_loads.append(path)
                return object()

        for module, name, replacement in (
            (diffusers, "Flux2KleinInpaintPipeline", self.pipeline_cls),
            (transformers, "Qwen2TokenizerFast", _Tokenizer),
            (hardware, "_resolve_selected_backend_device", lambda _fallback: "cuda:0"),
            (hardware, "_clear_torch_cache", lambda: None),
        ):
            attr_patch = patch.object(module, name, replacement)
            attr_patch.start()
            self.addCleanup(attr_patch.stop)

        self.probes = _RoomyProbes()
        arbiter = ResidencyArbiter(self.probes, admission_wait_s=0.0)
        self.small = SmallModelManager(arbiter=arbiter)
        self.large = LargeModelManager(
            arbiter,
            limits_store=MemoryLimitsStore(),
            tables=lambda _device: _flat_tables(),
            probes=self.probes,
            transport_factory=lambda _device: TorchTransport("cpu"),
            device_class=lambda _device: "test",
        )
        self.service = svc.Flux2KleinInpaintService(self.small, large_models=self.large)

    def held(self) -> dict[str, tuple[int, int]]:
        """`{handle key: (device bytes, host bytes)}` of every FLUX.2 handle (`LargeModelManager.status()`)."""
        return {entry["key"]: (int(entry["device_bytes"]), int(entry["host_bytes"])) for entry in self.large.status()}


def _flat_tables() -> object:
    """A calibrated round-number table with 1 MiB activations (the tiny models need nothing near it)."""
    from modules.ai_backend.runtime.large_models.cost_tables import parse_tables

    def entry(exec_kind: str) -> dict[str, object]:
        return {"family": "*", "phase": "*", "variant": "*", "format": "*", "compute_path": "*", "exec": exec_kind, "measured": True, "fixed_bytes": 1 << 20, "bytes_per_token": 0, "bytes_per_pixel": 0, "reserved_factor": 1.0, "seconds_fixed": 0.0, "seconds_per_ktoken_pass": 0.0, "seconds_per_ktoken2_pass": 0.0, "seconds_per_mpixel": 0.0}

    rate = 1 << 30
    data = {
        "device_class": "test",
        "calibrated": True,
        "source": "test",
        "device": {"runtime_overhead_bytes": 0, "margin_fraction": 0.0, "margin_cap_bytes": 0, "park_bytes_per_s": rate, "restore_bytes_per_s": rate, "block_h2d_bytes_per_s": rate, "disk_cold_bytes_per_s": rate, "disk_warm_bytes_per_s": rate, "prefetch_depth": 1},
        "phases": [entry("device"), entry("cpu")],
    }
    return parse_tables(data, source="test")
