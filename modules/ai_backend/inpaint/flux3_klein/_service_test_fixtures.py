"""
File: modules/ai_backend/inpaint/flux3_klein/_service_test_fixtures.py

Purpose:
Shared harness of the `Flux3KleinService` tests (`test_characterization.py`,
`test_service.py`, `test_large_model.py`): a tiny FLUX.3 package on disk whose
weight files are REAL safetensors of tiny stand-in modules, the real residency
arbiter and the real `LargeModelManager` over fake budget probes, and stubs at
the model seams so the real service, the real manager mechanics and the real
`packing` / `sampling` code run on tiny CPU tensors.

Key declarations:
- `ServiceRig` - package, manager, service, stub registry, failure switches;
- `StubDit`, `StubAe`, `StubTextEncoder`, `StubVlEncoder` - the stand-ins
  (state keys shaped like the real checkpoints, so the spec's block rules and
  key maps apply);
- `FakeProbes` - host / device budgets (huge by default, settable).

Notes:
- Test-only. No weights, no GPU: the run device is `cpu`, the transport the
  real `TorchTransport("cpu")`. Every file goes to a temporary directory the
  rig removes.
- Stubs are installed on the DEFINING modules (`loading.build_dit_meta`,
  `autoencoder.build_autoencoder_meta`, `text_encoder.build_text_encoder_meta`,
  `text_encoder.load_text_processor`, `text_encoder.encode_text`,
  `text_encoder.encode_with_references`, `quant.resolve_fp8_compute`,
  `quant.probe_scaled_mm`) and on the service's patch points.
"""

from __future__ import annotations

import copy
import json
import tempfile
import unittest
from collections.abc import Mapping
from contextlib import ExitStack
from pathlib import Path
from typing import Any
from unittest.mock import patch

import torch
from safetensors.torch import save_file
from torch import nn

from ...runtime.large_models.cost_tables import load_tables
from ...runtime.large_models.manager import LargeModelManager, MemoryLimitsStore
from ...runtime.memory_budget import DeviceBudget, HostBudget
from ...runtime.residency.arbiter import ResidencyArbiter
from . import autoencoder, loading, quant, service, text_encoder
from .config import WeightFormat
from .params import EncoderMode, Fp8Compute
from .phases import Phase
from .test_config import OFFICIAL_CONFIG

#: Tiny token widths so the real packing/sampling code runs instantly.
CHANNELS = {"image": 8, "image_cond": 8, "text": 16}

#: Padded / real text length the stub encoder reports.
TEXT_LEN = 80
TEXT_REAL = 5

HUGE = 1 << 44


class StubDit(nn.Module):
    """Zero velocity; `fail` makes the denoise raise (a failed RUN). Two single blocks + one non-block linear."""

    def __init__(self) -> None:
        super().__init__()
        self.txt_in = nn.Linear(4, 4, bias=False)
        self.single_blocks = nn.ModuleList([nn.Linear(4, 4, bias=False) for _ in range(2)])
        self.fail = False
        self.calls = 0

    def forward(self, packed: Any, timesteps_ref: torch.Tensor) -> torch.Tensor:
        self.calls += 1
        if self.fail:
            raise RuntimeError("denoise exploded")
        if self.txt_in.weight.device.type == "meta":
            raise AssertionError("the DiT ran without its weights")
        return torch.zeros_like(packed.data["image"])


class StubAe(nn.Module):
    """Encodes to zero `image_cond` tokens, decodes to a grey image; one real parameter."""

    def __init__(self) -> None:
        super().__init__()
        self.config = autoencoder.AeConfig()
        self.proj = nn.Linear(2, 2, bias=False)

    def encode(self, image: torch.Tensor) -> tuple[torch.Tensor, torch.Tensor]:
        if self.proj.weight.device.type == "meta":
            raise AssertionError("the AE ran without its weights")
        h, w = image.shape[1] // 16, image.shape[2] // 16
        return torch.zeros(h * w, CHANNELS["image_cond"]), autoencoder.latent_ids(h, w)

    def decode(self, tokens: torch.Tensor, ids: torch.Tensor) -> torch.Tensor:
        if self.proj.weight.device.type == "meta":
            raise AssertionError("the AE ran without its weights")
        h, w = int(ids[:, 1].max()) + 1, int(ids[:, 2].max()) + 1
        return torch.zeros(3, 16 * h, 16 * w)


class _Language(nn.Module):
    def __init__(self) -> None:
        super().__init__()
        self.embed_tokens = nn.Linear(4, 4, bias=False)
        self.layers = nn.ModuleList([nn.Linear(4, 4, bias=False) for _ in range(2)])


class StubTextEncoder(_Language):
    """TEXT-mode stand-in (`Qwen3VLTextModel` key layout: `embed_tokens`, `layers.N`)."""


class StubVlEncoder(nn.Module):
    """VL_REFS stand-in (`Qwen3VLModel` key layout: `language_model.*`, `visual.*`)."""

    def __init__(self) -> None:
        super().__init__()
        self.language_model = _Language()
        self.visual = nn.Linear(4, 4, bias=False)


def encoded() -> text_encoder.EncodedText:
    """What the stub encoders return: `TEXT_LEN` zero tokens, `TEXT_REAL` real."""
    positions = torch.arange(TEXT_LEN)
    zeros = torch.zeros_like(positions)
    return text_encoder.EncodedText(
        tokens=torch.zeros(TEXT_LEN, CHANNELS["text"], dtype=torch.bfloat16),
        ids=torch.stack([zeros, zeros, zeros, positions], dim=-1),
        real_length=TEXT_REAL,
    )


class FakeProbes:
    """Budget probes with settable figures (bytes): the device is as large / free as `device_total`, the host as `host_available`."""

    def __init__(self) -> None:
        self.device_total = HUGE
        self.host_available = HUGE

    def host(self) -> HostBudget:
        return HostBudget(total=HUGE, available=self.host_available, cgroup_limit=None, cgroup_used=None, rss_anon=1 << 20, rss_shmem=0, reserve=0)

    def device(self, device: str) -> DeviceBudget:
        return DeviceBudget(device=device, total=self.device_total, free=self.device_total, torch_allocated=0, torch_reserved=0, non_torch=0, reserve=0, cap=None)


def _save(path: Path, tensors: Mapping[str, torch.Tensor]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    save_file({key: value.contiguous() for key, value in tensors.items()}, str(path))


def write_package(root: Path, variants: tuple[WeightFormat, ...] = (WeightFormat.FP8R,)) -> None:
    """A tiny package: real config.json files (tiny channels), real safetensors of the stand-ins."""
    data = copy.deepcopy(OFFICIAL_CONFIG)
    data["dit"]["in_channels"] = dict(CHANNELS)
    data["sampler"]["size"] = [64, 64]
    dit = {"txt_in.weight": torch.ones(4, 4), "single_blocks.0.weight": torch.ones(4, 4), "single_blocks.1.weight": torch.ones(4, 4)}
    for variant in variants:
        directory = root if variant is WeightFormat.BF16 else root / "variants" / variant.value
        directory.mkdir(parents=True, exist_ok=True)
        config = dict(data) if variant is WeightFormat.BF16 else {**data, "quantization": variant.value}
        (directory / "config.json").write_text(json.dumps(config), encoding="utf-8")
        _save(directory / "model.safetensors", dit)
    _save(root / "ae.safetensors", {"proj.weight": torch.ones(2, 2)})
    encoder_dir = root / "text_encoder"
    shard = "model-00001-of-00001.safetensors"
    tensors = {
        "model.language_model.embed_tokens.weight": torch.ones(4, 4),
        "model.language_model.layers.0.weight": torch.ones(4, 4),
        "model.language_model.layers.1.weight": torch.ones(4, 4),
        "model.visual.weight": torch.ones(4, 4),
    }
    _save(encoder_dir / shard, tensors)
    (encoder_dir / "model.safetensors.index.json").write_text(json.dumps({"weight_map": {key: shard for key in tensors}}), encoding="utf-8")


class ServiceRig:
    """A `Flux3KleinService` over the real arbiter + `LargeModelManager` (fake probes, CPU transport) and stubbed models.

    `dits` / `aes` / `encoders` list every stand-in a meta build created (the
    manager keeps a loaded one between runs, so `dits[-1]` is the live DiT);
    `fail_ae_build` makes the AE build raise (a failed LOAD); `fp8` is what
    the fp8 resolution answers.
    """

    def __init__(self, test: unittest.TestCase, *, variants: tuple[WeightFormat, ...] = (WeightFormat.FP8R,)) -> None:
        stack = ExitStack()
        test.addCleanup(stack.close)
        self.root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="flux3_service_")))
        write_package(self.root, variants)
        self.dits: list[StubDit] = []
        self.aes: list[StubAe] = []
        self.encoders: list[nn.Module] = []
        self.fail_ae_build = False
        self.fp8 = Fp8Compute.DEQUANT
        self.encode_calls: list[tuple[str, Any]] = []
        self.probes = FakeProbes()
        self.arbiter = ResidencyArbiter(self.probes, admission_wait_s=0.0)
        self.store = MemoryLimitsStore()
        # The CPU stands in for a GPU here (fake device probes): plan it as a device, not as the host.
        self.manager = LargeModelManager(self.arbiter, limits_store=self.store, tables=load_tables("generic"), probes=self.probes, device_class=lambda device: "test_class", host_device=lambda device: False)
        self.service = service.Flux3KleinService(self.manager)
        rig = self

        def build_dit_meta(files: Any, config: Any, *, variant: WeightFormat, fp8_compute_applied: Any) -> StubDit:
            with torch.device("meta"):
                dit = StubDit()
            rig.dits.append(dit)
            return dit

        def build_autoencoder_meta(config: Any = None) -> StubAe:
            if rig.fail_ae_build:
                raise ValueError("bad autoencoder")
            with torch.device("meta"):
                ae = StubAe()
            rig.aes.append(ae)
            return ae

        def build_text_encoder_meta(directory: Path, *, mode: EncoderMode, keep_layers: int) -> nn.Module:
            with torch.device("meta"):
                model: nn.Module = StubTextEncoder() if mode is EncoderMode.TEXT else StubVlEncoder()
            rig.encoders.append(model)
            return model

        def encode_text(handle: Any, caption: str) -> Any:
            rig.encode_calls.append(("text", handle.device))
            return encoded()

        def encode_with_references(handle: Any, caption: str, references: Any, *, vl_ref_max_pixels: int) -> Any:
            rig.encode_calls.append(("vl", handle.device))
            return encoded()

        for owner, name, stub in (
            (loading, "build_dit_meta", build_dit_meta),
            (autoencoder, "build_autoencoder_meta", build_autoencoder_meta),
            (text_encoder, "build_text_encoder_meta", build_text_encoder_meta),
            (text_encoder, "load_text_processor", lambda directory: object()),
            (text_encoder, "encode_text", encode_text),
            (text_encoder, "encode_with_references", encode_with_references),
            (quant, "resolve_fp8_compute", lambda requested, device: rig.fp8),
            (quant, "probe_scaled_mm", lambda device: quant.ScaledMmProbe(supported=False, reason="test", rel_error=None)),
            (service, "_clear_torch_cache", lambda: None),
            (service, "_resolve_selected_backend_device", lambda _fallback: "cpu"),
        ):
            stack.enter_context(patch.object(owner, name, stub))

    def large_holdings(self) -> tuple[int, int, int]:
        """(resident, in use, loading) counts of the large pool's holdings."""
        from ...runtime.residency.holdings import Pool

        counters = self.arbiter.pool_counters(Pool.LARGE)
        return counters["resident"], counters["in_use"], counters["loading"]


class Recorder:
    """A `PhaseObserver` that records `(phase, event, info)`."""

    def __init__(self) -> None:
        self.events: list[tuple[Phase, str, dict[str, Any]]] = []

    def __call__(self, phase: Phase, event: str, info: Mapping[str, Any]) -> None:
        self.events.append((phase, event, dict(info)))

    def begin(self, phase: Phase) -> dict[str, Any]:
        """The `begin` info of `phase`."""
        return next(info for p, e, info in self.events if p is phase and e == "begin")
