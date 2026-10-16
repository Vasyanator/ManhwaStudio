"""
File: tools/large_model_calibration/common.py

Purpose:
What both measurement drivers (`flux3.py`, `flux2.py`) share: list / size
parsing, the output directory rule, logging setup, device resolution, the
residency stack a measured service runs under, and the device facts taken
before the first device call.

Key functions:
- `parse_list()`, `parse_sizes()` - option parsing (`ValueError` on bad input)
- `resolve_out()` - the output directory (never inside the repository)
- `setup_logging()` - INFO to stderr and `run.log`
- `resolve_device()` - `discrete` / `service` / explicit device
- `build_large_models()` - arbiter + small pool + `LargeModelManager`
  (limits and floors in memory only)
- `open_device()` - baseline, ROCm setup, device, `--vram-cap-gib`, environment

Notes:
- torch is imported inside functions (a dry run imports none).
"""

from __future__ import annotations

import logging
import tempfile
from pathlib import Path
from typing import Any

log = logging.getLogger(__name__)


def parse_list(value: str, allowed: dict[str, Any], what: str) -> list[Any]:
    """Comma list -> values from `allowed`; `ValueError` naming unknown entries."""
    tokens = [token.strip() for token in value.split(",") if token.strip()]
    unknown = [token for token in tokens if token not in allowed]
    if unknown or not tokens:
        raise ValueError(f"{what}: unknown or empty {unknown or value!r}; allowed {list(allowed)}")
    return [allowed[token] for token in tokens]


def parse_sizes(value: str, *, allow_auto: bool = True) -> list[tuple[int, int] | None]:
    """`auto` -> `None` (when allowed), `WxH` -> `(W, H)`; `ValueError` for anything else (the services check the bounds)."""
    sizes: list[tuple[int, int] | None] = []
    for token in (token.strip() for token in value.split(",")):
        if not token:
            continue
        if token == "auto" and allow_auto:
            sizes.append(None)
            continue
        width, sep, height = token.partition("x")
        if not sep or not width.isdigit() or not height.isdigit():
            raise ValueError(f"--sizes: {token!r} is not {'auto or ' if allow_auto else ''}WxH")
        sizes.append((int(width), int(height)))
    if not sizes:
        raise ValueError("--sizes is empty")
    return sizes


def resolve_out(out: Path | None, repo_root: Path, dry_run: bool, prefix: str) -> Path | None:
    """The output directory (created unless `dry_run`; default a fresh `<prefix>*` in the system temp dir).

    # Raises
    `ValueError` for a path inside the repository: measurement output never
    goes into the tree.
    """
    if out is None:
        return None if dry_run else Path(tempfile.mkdtemp(prefix=prefix))
    resolved = out.expanduser().resolve()
    if resolved.is_relative_to(repo_root.resolve()):
        raise ValueError(f"--out {resolved} is inside the repository; choose a path outside it (default: system temp)")
    if not dry_run:
        resolved.mkdir(parents=True, exist_ok=True)
    return resolved


def setup_logging(out_dir: Path | None) -> None:
    """INFO to stderr, plus `run.log` in `out_dir` when there is one."""
    handlers: list[logging.Handler] = [logging.StreamHandler()]
    if out_dir is not None:
        handlers.append(logging.FileHandler(out_dir / "run.log", encoding="utf-8"))
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s", handlers=handlers)


def resolve_device(choice: str) -> tuple[str | None, str]:
    """`(device for the service options, device the measurements use)`.

    `discrete` uses `inpaint.flux_fill._select_discrete_device` (the owner of
    the "never the iGPU" rule); `service` leaves the option `None` and
    measures on what `runtime.torch_device.resolve_selected_backend_device`
    picks (the service's own resolution); anything else is taken as a device
    string.

    # Raises
    `RuntimeError` when the result is not a CUDA/HIP device (nothing to measure).
    """
    import torch

    from modules.ai_backend.inpaint.flux_fill import _select_discrete_device
    from modules.ai_backend.runtime.torch_device import resolve_selected_backend_device

    if choice == "discrete":
        device = str(_select_discrete_device())
        option: str | None = device
    elif choice == "service":
        device = resolve_selected_backend_device("cuda")
        option = None
    else:
        device = choice
        option = choice
    if torch.device(device).type != "cuda":
        raise RuntimeError(f"device {device!r} (from --device {choice}) is not a CUDA/HIP GPU; nothing to measure")
    return option, device


def build_large_models(vram_cap: int | None) -> tuple[Any, Any, Any]:
    """The backend's residency stack for one tool run: `(LargeModelManager, small pool, probes)`.

    Limits and learned floors live in memory only (`MemoryLimitsStore`): the
    tool never writes the backend's `limits.json`. `vram_cap` (bytes, `None`
    = the card) is the manager's whole-device cap. The manager reads the
    tracked calibration table of the device's class, exactly as the backend.
    """
    from modules.ai_backend.runtime.large_models.cost_tables import detect_device_class, load_tables
    from modules.ai_backend.runtime.large_models.manager import LargeModelManager, MemoryLimitsStore
    from modules.ai_backend.runtime.large_models.tiers import Limits
    from modules.ai_backend.runtime.model_manager import SmallModelManager
    from modules.ai_backend.runtime.residency.arbiter import MemoryBudgetProbes, ResidencyArbiter

    probes = MemoryBudgetProbes()
    arbiter = ResidencyArbiter(probes)
    small = SmallModelManager(arbiter=arbiter)
    large = LargeModelManager(arbiter, limits_store=MemoryLimitsStore(Limits(vram_cap=vram_cap)), tables=lambda device: load_tables(detect_device_class(device)), probes=probes)
    return large, small, probes


def open_device(device_choice: str, vram_cap_gib: float | None) -> tuple[str | None, str, Any, dict[str, Any]]:
    """Take the device baseline (before ANY device call of this process), configure ROCm, resolve the
    device, apply `--vram-cap-gib`; returns `(option device, measured device, CapReport, environment)`."""
    from modules.ai_backend.runtime.rocm_runtime import configure_rocm_runtime

    from .device_memory import apply_vram_cap, device_vram_used
    from .report import environment

    baseline = device_vram_used()
    rocm = configure_rocm_runtime()
    option_device, device = resolve_device(device_choice)
    cap = apply_vram_cap(device, vram_cap_gib, baseline)
    env = environment(device, service_device=option_device is None)
    env["rocm_runtime_configured"] = rocm
    log.info("device %s (%s, %s, class %s)", device, env.get("device_name"), env.get("device_arch"), env.get("device_class"))
    return option_device, device, cap, env
