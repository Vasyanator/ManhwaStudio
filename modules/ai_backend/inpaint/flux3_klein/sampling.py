"""
File: modules/ai_backend/inpaint/flux3_klein/sampling.py

Purpose:
The distilled FLUX.3 klein sampler: the shifted time schedule and the
flow-map Euler loop (one sample, no CFG).

Key declarations:
- `FlowMapDit` - the duck-typed DiT the loop calls
- `StepCallback` - `(done_steps, total_steps) -> None`
- `flow_map_schedule()`, `denoise_flow_map()`

Notes:
- Ported from black-forest-labs/flux-3-image-klein `src/flux3/sampling.py`
  (commit e53f4ea), Copyright 2026 Black Forest Labs, Inc., licensed under the
  Apache License, Version 2.0 (see `LICENSE` in this directory).
- Modified: the CFG sampler (`denoise`) is not ported (the base model is out of
  scope); the DiT is duck-typed (`FlowMapDit`) so this module does not import
  `dit.py`; the autocast device type is a parameter instead of a fixed
  `"cuda"`; a per-step callback was added (progress, and cancellation by
  raising); inputs are validated; `timesteps` is named `flow_map_schedule`.
- Numerics kept from the reference: the schedule is float32 on the CPU, the
  image timesteps are bf16 (`fill_` rounds), `timesteps_ref` is the float32
  next time on the image device, and the update is `x + dt * v` where `x` stays
  float32 (the noise dtype) and `v` is the autocast (bf16) velocity.
"""

from __future__ import annotations

import logging
from collections.abc import Callable
from typing import Protocol

import torch
from torch import Tensor

from .packing import Packed

log = logging.getLogger(__name__)

#: `step_callback(done_steps, total_steps)`, called after every Euler step.
#: An exception it raises propagates and stops the loop (cancellation).
StepCallback = Callable[[int, int], None]


class FlowMapDit(Protocol):
    """A flow-map DiT: `dit(packed, timesteps_ref=t_next)` returns the velocity of
    `packed.data["image"]` (same shape)."""

    def __call__(self, packed: Packed, timesteps_ref: Tensor) -> Tensor: ...


def flow_map_schedule(steps: int, alpha: float) -> Tensor:
    """The `steps + 1` times `t = a*s / (1 + (a-1)*s)` for `s = linspace(1, 0)`, float32 on the CPU.

    It runs from exactly 1 to exactly 0; `alpha > 1` spends more steps at high noise.

    # Raises
    `ValueError` for `steps < 1` or `alpha <= 0`.
    """
    if isinstance(steps, bool) or not isinstance(steps, int) or steps < 1:
        raise ValueError(f"flow_map_schedule: steps={steps!r} должно быть целым >= 1")
    if not alpha > 0:
        raise ValueError(f"flow_map_schedule: alpha={alpha!r} должно быть положительным")
    s = torch.linspace(1.0, 0.0, steps + 1)
    return alpha * s / (1.0 + (alpha - 1.0) * s)


def denoise_flow_map(
    dit: FlowMapDit,
    packed: Packed,
    schedule: Tensor,
    *,
    device_type: str,
    step_callback: StepCallback | None = None,
) -> Tensor:
    """Run the flow-map Euler loop over `schedule` and return the final image tokens.

    Each step sets the image timesteps to `schedule[i]` and moves
    `packed.data["image"]` by `(schedule[i+1] - schedule[i]) * dit(packed,
    timesteps_ref=schedule[i+1])`, under `torch.inference_mode()` and
    `torch.autocast(device_type, dtype=bfloat16)`. `packed` is mutated in place
    (`data["image"]`, `timesteps["image"]`). `device_type` is the torch device
    type of the run (`"cuda"` also on ROCm, `"cpu"`).

    # Raises
    `ValueError` for a schedule that is not 1-D with at least two entries or a
    pack with other than one sample (the flow map samples one image); whatever
    `dit` or `step_callback` raises.
    """
    if schedule.ndim != 1 or schedule.shape[0] < 2:
        raise ValueError(f"denoise_flow_map: расписание должно быть одномерным из >= 2 точек, а не {tuple(schedule.shape)}")
    if packed.num_samples != 1:
        raise ValueError(f"denoise_flow_map: ожидается один образец, а не {packed.num_samples}")
    total = schedule.shape[0] - 1
    image_device = packed.data["image"].device
    log.debug("FLUX.3 klein: денойзинг, %d шагов, autocast %s", total, device_type)
    with torch.inference_mode(), torch.autocast(device_type, dtype=torch.bfloat16):
        for step in range(total):
            t, dt = schedule[step].item(), (schedule[step + 1] - schedule[step]).item()
            packed.timesteps["image"].fill_(t)
            target = schedule[step + 1].reshape(1).to(image_device)
            packed.data["image"] = packed.data["image"] + dt * dit(packed, timesteps_ref=target)
            if step_callback is not None:
                step_callback(step + 1, total)
    return packed.data["image"]
