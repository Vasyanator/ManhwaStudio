"""
File: modules/ai_backend/inpaint/flux2_klein/memory.py

Purpose:
The FLUX.2 klein adapters between the stage-1 wire (the legacy `placement`
flags, `inpaint.flux2_klein.estimate`, the guard's preset advice) and the
large-model manager, which owns every resource decision. Nothing here
forecasts on its own: the numbers come from `runtime.large_models` (the
planner on `large_model.py`'s specs, the one `forecast` arithmetic).

Main responsibilities:
- `requested_steps` - the manager step pair of a request: `params.vram_step` /
  `params.ram_step` when either is sent (the absent axis «Авто»), else the
  LEGACY `params.profile` id (`LEGACY_PROFILE_STEPS`), else the legacy
  `placement` mapping `legacy_steps` (table below);
- `legacy_profile_name` - the stage-1 `profile` / `profile_effective` wire
  value of a step pair (the legacy name of its VRAM step, table below);
- `encoder_cpu_pinned` - whether a request pins the text encoder to the CPU
  (the legacy wire's meaning, below);
- `forecast_memory` - the `estimate` / golden answer for one request: the
  pipeline planned first, the text encoder second on what the pipeline left
  (exactly how a run is admitted), reported in the stage-1 shape
  (`vram_bytes`, `ram_bytes`, `phases`, `resident`, `breakdown`) plus the
  manager's requested / effective steps (`vram_step`, `ram_step`,
  `vram_step_effective`, `ram_step_effective`, and the legacy names
  `profile`, `profile_effective`), `fits` and `calibrated`;
- the COMBINED VRAM-axis rule of the two handles (`combined_device_peak`,
  `combined_ceilings`, `encoder_device_cap`; module notes), shared by
  `forecast_memory`, the run's encoder admission and `large_model_profiles`;
- `_preset_advice` - which concrete step pairs fit the given free memory;
- `is_host_run_device` - whether a run device is planned on the host alone.

Legacy `placement` mapping (also the stage-2 migration of the persisted settings):
| `placement` | flags | steps (VRAM, RAM) |
|---|---|---|
| `full_gpu` | any | (MAX, KEEP) |
| `encoder_cpu` | `unload_transformer_before_vae=False` | (MAX, KEEP) |
| `encoder_cpu` | `unload_transformer_before_vae=True` (its default) | (PHASE, KEEP) |
| `model_cpu_offload` | any | (PHASE, KEEP) |
| `sequential_cpu_offload` | any | (MIN, KEEP) |
| anything else | | (AUTO, AUTO) |
`low_cpu_mem_usage` no longer changes anything (the manager always streams
tensor by tensor) and `vae_slicing` was always inert (KG-008).

The text encoder under the legacy wire (`encoder_cpu_pinned`): every legacy
`placement` encoded on the HOST before the manager, whatever the step pair
above keeps of the pipeline. A request that sends neither `vram_step` nor
`ram_step` therefore pins the encoder to the CPU (the CPU-only encoder spec,
`large_model.encoder_spec(cpu_only=True)`): (MAX, KEEP) for `full_gpu` keeps
the PIPELINE on the card, never the 9B encoder beside it. The one exception is
a legacy `profile` whose RAM step is MIN (`max_economy`): that step offers the
encoder no CPU placement at all (`tiers.template_for`), so it keeps the
unpinned spec (juggled from disk). With `vram_step` / `ram_step` sent the
encoder may compute on the GPU under MAX / PHASE (the two-axis semantics).

Legacy `profile` ids (stage-1 field, no client sends it) -> steps, and back:
| id | steps in | name out (by VRAM step) |
|---|---|---|
| `max_speed` | (MAX, KEEP) | MAX |
| `fast` | (MAX, KEEP) | - (reported as `max_speed`) |
| `balanced` | (PHASE, KEEP) | PHASE |
| `economy` | (HALF, RUN) | HALF |
| `max_economy` | (MIN, MIN) | MIN |
| `auto` | (AUTO, AUTO) | AUTO |
The name ignores the RAM step (a compatibility label); the exact pair is in
the `*_step*` keys.

Notes:
- `hardware.memory_snapshot` (the patch point the suite uses) supplies the
  free figures; `0` = unknown, which plans against unbounded budgets and
  always "fits" (advice only: the manager's admission is the guard).
- A HOST run device (`cpu` / `mps`: any device the manager does not budget
  against a device probe, `runtime.residency.footprint.is_device_accounted`)
  is planned exactly as the manager plans it (`Budgets.host_device`): no
  device budget and no unbounded-VRAM rule, every byte on the host side, the
  RAM step alone picks the plan; the answer's VRAM figures are `0` and `fits`
  is the host plan's.
- Combined VRAM-axis monotonicity (the two handles together): the planner keeps
  each handle monotone on its own, but the encoder is admitted on what the
  pipeline's RESERVATION leaves, and its MAX / PHASE templates fill that room.
  A lower step whose pipeline reserves less (PHASE parks the transformer for
  the decode, MAX keeps it) would hand the encoder more room, and the encode
  moment (the pipeline's denoise-state weights + the encoder) would forecast
  more device memory than the higher step. So a run's encoder is admitted
  with a device ceiling (`Budgets.device_cap_bytes`, `acquire(...,
  device_cap_bytes=)`): the combined device peak of the nearest fitting HIGHER
  VRAM step at the same RAM step (each planned under its own ceiling, the
  chain `combined_ceilings`), minus this step's pipeline weights in the
  denoise state (`encoder_device_cap`). Hence, for the same request and
  budgets, a lower VRAM step never forecasts more device bytes than a higher
  one for the two handles together. A host run device has no VRAM axis: no
  ceiling.
- The planner relaxes only device phases whose cost entry is unmeasured (e.g. a
  borrowed table of an unknown device class): it may admit on their weights alone. The
  displayed peaks are the plan's full forecast (`Planned.full_forecast`, every
  activation estimate included); `calibrated` says whether it is measured.
"""

from __future__ import annotations

import functools
import logging
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from ...runtime.large_models.cost_tables import CostTables, detect_device_class, load_tables
from ...runtime.large_models.forecast import Forecast
from ...runtime.large_models.inventory import ComponentInventory, build_inventory
from ...runtime.large_models.planner import Budgets, Planned, PlanRefusal, plan
from ...runtime.large_models.spec import LargeModelSpec
from ...runtime.large_models.tiers import AUTO_STEPS, STEP_WALK, VRAM_ORDER, Limits, RamStep, Steps, VramStep
from ...runtime.memory_budget import DeviceBudget, HostBudget
from ...runtime.residency.footprint import is_device_accounted
from . import attention, hardware, large_model
from .components import text_encoder_available

log = logging.getLogger(__name__)

#: User-facing names of the concrete steps (the settings' labels), for advice.
VRAM_STEP_LABELS = {
    VramStep.MAX: "Максимум скорости",
    VramStep.PHASE: "Только текущий этап",
    VramStep.HALF: "Не больше половины",
    VramStep.MIN: "Минимум",
}
RAM_STEP_LABELS = {
    RamStep.KEEP: "Держать модели в памяти",
    RamStep.RUN: "Только на время задачи",
    RamStep.MIN: "Минимум, читать с диска",
}

#: The legacy `params.profile` ids onto step pairs (module table).
LEGACY_PROFILE_STEPS: dict[str, Steps] = {
    "auto": AUTO_STEPS,
    "max_speed": Steps(VramStep.MAX, RamStep.KEEP),
    "fast": Steps(VramStep.MAX, RamStep.KEEP),
    "balanced": Steps(VramStep.PHASE, RamStep.KEEP),
    "economy": Steps(VramStep.HALF, RamStep.RUN),
    "max_economy": Steps(VramStep.MIN, RamStep.MIN),
}

#: The legacy wire name of each VRAM step (module table).
_LEGACY_PROFILE_NAMES = {
    VramStep.AUTO: "auto",
    VramStep.MAX: "max_speed",
    VramStep.PHASE: "balanced",
    VramStep.HALF: "economy",
    VramStep.MIN: "max_economy",
}

#: "Unbounded" figure for an unknown budget side (1 PiB): the plan of an
#: unknown machine is the step pair's first choice.
_UNBOUNDED = 1 << 50


def legacy_steps(normalized: dict[str, Any]) -> Steps:
    """The step pair the legacy `placement` flags mean (module table)."""
    placement = normalized["placement"]
    if placement == "full_gpu":
        return Steps(VramStep.MAX, RamStep.KEEP)
    if placement == "encoder_cpu":
        return Steps(VramStep.PHASE if normalized["unload_transformer_before_vae"] else VramStep.MAX, RamStep.KEEP)
    if placement == "model_cpu_offload":
        return Steps(VramStep.PHASE, RamStep.KEEP)
    if placement == "sequential_cpu_offload":
        return Steps(VramStep.MIN, RamStep.KEEP)
    return AUTO_STEPS


def requested_steps(normalized: dict[str, Any]) -> Steps:
    """`params.vram_step` / `params.ram_step` when either is sent (the other «Авто»), else `params.profile`, else `legacy_steps`."""
    vram, ram = normalized.get("vram_step"), normalized.get("ram_step")
    if vram or ram:
        return Steps(VramStep(vram) if vram else VramStep.AUTO, RamStep(ram) if ram else RamStep.AUTO)
    if normalized.get("profile"):
        return LEGACY_PROFILE_STEPS[normalized["profile"]]
    return legacy_steps(normalized)


def encoder_cpu_pinned(normalized: dict[str, Any]) -> bool:
    """Whether the request pins the text encoder to the CPU: a legacy request (no `vram_step` / `ram_step`) whose RAM step is not MIN (module notes)."""
    if normalized.get("vram_step") or normalized.get("ram_step"):
        return False
    return requested_steps(normalized).ram is not RamStep.MIN


def legacy_profile_name(steps: Steps) -> str:
    """The stage-1 `profile` wire value of `steps`: the legacy name of its VRAM step (module table)."""
    return _LEGACY_PROFILE_NAMES[steps.vram]


#: The keys `steps_fields` returns (wire order).
STEP_FIELD_KEYS = ("profile", "profile_effective", "vram_step", "ram_step", "vram_step_effective", "ram_step_effective")


def steps_fields(requested: Steps, effective: Steps | None) -> dict[str, str | None]:
    """The wire keys of a requested / effective step pair (`None` effective = nothing planned).

    `{"profile", "profile_effective", "vram_step", "ram_step",
    "vram_step_effective", "ram_step_effective"}`; `profile*` are the legacy
    names (`legacy_profile_name`).
    """
    return {
        "profile": legacy_profile_name(requested),
        "profile_effective": legacy_profile_name(effective) if effective is not None else None,
        "vram_step": requested.vram.value,
        "ram_step": requested.ram.value,
        "vram_step_effective": effective.vram.value if effective is not None else None,
        "ram_step_effective": effective.ram.value if effective is not None else None,
    }


@functools.lru_cache(maxsize=None)
def _tables_for(device: str) -> CostTables:
    """The cost tables of `device`'s class (patch point of the golden tests)."""
    return load_tables(detect_device_class(device))


_SPECS: dict[tuple[Any, ...], LargeModelSpec] = {}


def _pipeline_spec(dtype: str) -> LargeModelSpec:
    """The pipeline spec of `dtype`, built once per process."""
    key = ("pipeline", dtype)
    if key not in _SPECS:
        _SPECS[key] = large_model.pipeline_spec(dtype)
    return _SPECS[key]


def _encoder_spec(options: large_model.EncoderOptions) -> LargeModelSpec:
    """The encoder spec `options` select (precision, CPU-only), built once per process."""
    key = ("encoder", options.fp8, options.cpu_only)
    if key not in _SPECS:
        _SPECS[key] = large_model.encoder_spec(options.fp8, options.cpu_only)
    return _SPECS[key]


def _inventories(spec: LargeModelSpec, options: Any) -> dict[str, ComponentInventory]:
    """Header inventories of every component of `spec` for `options`, cached per file set, size and mtime."""
    found: dict[str, ComponentInventory] = {}
    for component in spec.components:
        paths = [Path(path) for path in component.files(options, large_model.WEIGHT_FORMAT)]
        key = (id(spec), component.name, tuple((str(path), path.stat().st_size, path.stat().st_mtime_ns) for path in paths))
        cached = _INVENTORY_CACHE.get(key)
        if cached is None:
            cached = build_inventory(component, paths)
            _INVENTORY_CACHE[key] = cached
        found[component.name] = cached
    return found


_INVENTORY_CACHE: dict[tuple[int, str, tuple[tuple[str, int, int], ...]], ComponentInventory] = {}


def is_host_run_device(device: str) -> bool:
    """Whether `device` is a host run device (`cpu` / `mps`): no device budget, everything planned on the host (module notes)."""
    return not is_device_accounted(device)


def _budgets(memory: dict[str, int] | None, *, host_device: bool = False, claimed_device: int = 0, claimed_host: int = 0, device_cap: int | None = None) -> Budgets:
    """Synthetic planner budgets from a `hardware.memory_snapshot` (unknown sides unbounded).

    `host_device`: the run device is the host (module notes): a zero device
    budget that is never read, and every claimed byte on the host side.
    `device_cap`: the per-handle device ceiling (`Budgets.device_cap_bytes`,
    `None` = none; the encoder's under the combined rule, module notes).
    """
    memory = memory or {}
    vram_total = int(memory.get("vram_total") or 0)
    vram_free = int(memory.get("vram_free") or 0)
    ram_total = int(memory.get("ram_total") or 0)
    ram_free = int(memory.get("ram_free") or 0)
    if vram_total <= 0 or vram_free <= 0:
        vram_total = vram_free = _UNBOUNDED
    if ram_free <= 0:
        ram_total = ram_free = _UNBOUNDED
    host = HostBudget(total=max(ram_total, ram_free), available=ram_free, cgroup_limit=None, cgroup_used=None, rss_anon=0, rss_shmem=0, reserve=0)
    if host_device:
        zero = DeviceBudget(device="estimate", total=0, free=0, torch_allocated=0, torch_reserved=0, non_torch=0, reserve=0, cap=None)
        return Budgets(host=host, device=zero, baseline_rss=0, claimed_host_bytes=claimed_device + claimed_host, host_device=True)
    device = DeviceBudget(device="estimate", total=vram_total, free=vram_free, torch_allocated=0, torch_reserved=0, non_torch=max(0, vram_total - vram_free), reserve=0, cap=None)
    return Budgets(host=host, device=device, baseline_rss=0, claimed_device_bytes=claimed_device, claimed_host_bytes=claimed_host, device_cap_bytes=device_cap)


def combined_device_peak(pipeline: Forecast, encoder: Forecast | None) -> int:
    """The device peak of one run of both handles (bytes): the pipeline's denoise / decode peaks, and the encode moment.

    The encode moment is the pipeline's denoise-state device weights (where a
    cold run encodes) plus the encoder's `encode` phase peak; `encoder=None`
    = nothing is encoded. Equal to `forecast_memory`'s `vram_bytes` for the
    same two plans.
    """
    phases = pipeline.phases
    peak = max(int(phases[large_model.PHASE_DENOISE].device_peak), int(phases[large_model.PHASE_DECODE].device_peak))
    if encoder is not None and large_model.PHASE_ENCODE in encoder.phases:
        peak = max(peak, int(phases[large_model.PHASE_DENOISE].device_weights) + int(encoder.phases[large_model.PHASE_ENCODE].device_peak))
    return peak


def encoder_device_cap(ceiling: int | None, pipeline: Forecast) -> int | None:
    """The encoder's device ceiling beside `pipeline`'s plan (bytes; `None` = none): `ceiling` minus the pipeline's denoise-state device weights (module notes)."""
    if ceiling is None:
        return None
    return max(0, ceiling - int(pipeline.phases[large_model.PHASE_DENOISE].device_weights))


def combined_ceilings(ram: RamStep, combined_at: Callable[[Steps, int | None], int | None], *, until: VramStep) -> dict[VramStep, int | None]:
    """The combined ceiling of every concrete VRAM step from MAX down to `until` at the RAM step `ram` (module notes).

    A step's ceiling is the combined device peak of the nearest FITTING higher
    step (`None` for MAX, or when no higher step fits). `combined_at(cell,
    ceiling)` plans `cell` with the encoder capped by `encoder_device_cap(ceiling,
    <its pipeline plan>)` and returns `combined_device_peak` (`None` = the cell
    does not fit); it is called only for the steps ABOVE `until`. The chain
    is monotone by construction: every planned cell stays under its ceiling.
    """
    ceilings: dict[VramStep, int | None] = {}
    ceiling: int | None = None
    for vram in VRAM_ORDER:
        ceilings[vram] = ceiling
        if vram is until:
            break
        peak = combined_at(Steps(vram, ram), ceiling)
        if peak is not None:
            ceiling = peak
    return ceilings


@dataclass(frozen=True)
class _Part:
    """One planned handle of the estimate.

    `steps` is the concrete pair planned (`None` when refused); `guarded` the
    forecast the planner admitted on (what the manager reserves: without the
    activations of unmeasured device phases), or the closest refusal's; `full`
    the same plan with every activation estimate, for display.
    """

    fits: bool
    steps: Steps | None
    guarded: Forecast | None
    full: Forecast | None


def _plan_part(spec: LargeModelSpec, options: Any, shape: Any, steps: Steps, budgets: Budgets, tables: CostTables) -> _Part:
    inventories = _inventories(spec, options)
    result = plan(spec, {large_model.WEIGHT_FORMAT: inventories}, format=large_model.WEIGHT_FORMAT, formats=spec.formats(options), steps=steps, limits=Limits(), budgets=budgets, shape=shape, tables=tables, options=options)
    if isinstance(result, Planned):
        return _Part(True, result.plan.steps, result.forecast, result.full_forecast)
    assert isinstance(result, PlanRefusal)
    return _Part(False, None, result.forecast, result.forecast)


def forecast_memory(
    normalized: dict[str, Any],
    region_width: int,
    region_height: int,
    *,
    with_reference: bool = False,
    memory: dict[str, int] | None = None,
    steps: Steps | None = None,
    device: str | None = None,
) -> dict[str, Any]:
    """The manager's forecast of one run, in the stage-1 `estimate` shape (bytes).

    The pipeline is planned for `steps` (default `requested_steps`) against
    `memory` (a `hardware.memory_snapshot`; `None` / unknown = unbounded; a
    host run `device` plans on the host alone, module notes),
    then the text encoder against what the pipeline's peak left, under the
    combined VRAM-axis ceiling (module notes), as the service admits them. The encoder is pinned to the CPU exactly when a run
    of this request would pin it (`encoder_cpu_pinned`) — and never for an
    explicit `steps` pair (the two-axis semantics the advice names). A side
    that does not fit reports the planner's closest attempt. With no text
    encoder installed both encode phases are 0.

    Returns `{"vram_bytes", "ram_bytes", "phases", "resident", "breakdown",
    "fits", "calibrated"}` plus the `steps_fields` keys (effective = the
    pipeline's plan, `None` when it does not fit); `phases` maps
    `encode`, `encode_standalone`, `denoise`, `decode` to `{"vram_bytes",
    "ram_bytes"}`; `breakdown` keeps the stage-1 key set (+ `attention_mask`
    while that term is non-zero).

    # Raises
    `ValueError` for files whose headers cannot be read (the manager would
    refuse the run the same way).
    """
    chosen = steps or requested_steps(normalized)
    device_name = device or hardware._resolve_selected_backend_device("cuda")
    tables = _tables_for(device_name)
    host_device = is_host_run_device(device_name)
    options = large_model.pipeline_options(normalized, guidance=False)
    shape = large_model.request_shape(region_width, region_height, with_reference=with_reference, max_sequence_length=options.max_sequence_length, steps=options.steps)
    pipeline_spec = _pipeline_spec(normalized["dtype"])
    pipeline = _plan_part(pipeline_spec, options, shape, chosen, _budgets(memory, host_device=host_device), tables)

    encoder_installed = text_encoder_available(normalized)
    encoder_options = large_model.encoder_options(normalized, cpu_only=steps is None and encoder_cpu_pinned(normalized))
    encoder: _Part | None = None
    if encoder_installed:
        encoder_spec = _encoder_spec(encoder_options)

        def encoder_beside(part: _Part, steps_: Steps, cap: int | None) -> _Part:
            # The encoder is admitted on what the pipeline's RESERVATION leaves:
            # the guarded peaks, as the manager reserves them.
            claimed = part.guarded.device_peak if part.guarded is not None else 0
            claimed_host = part.guarded.host_peak if part.guarded is not None else 0
            return _plan_part(encoder_spec, encoder_options, shape, steps_, _budgets(memory, host_device=host_device, claimed_device=claimed, claimed_host=claimed_host, device_cap=cap), tables)

        def combined_at(cell: Steps, ceiling: int | None) -> int | None:
            part = _plan_part(pipeline_spec, options, shape, cell, _budgets(memory), tables)
            if not part.fits or part.full is None:
                return None
            beside = encoder_beside(part, cell, encoder_device_cap(ceiling, part.full))
            return combined_device_peak(part.full, beside.full) if beside.fits else None

        cap = None
        if not host_device and pipeline.steps is not None and pipeline.full is not None:
            ceiling = combined_ceilings(pipeline.steps.ram, combined_at, until=pipeline.steps.vram)[pipeline.steps.vram]
            cap = encoder_device_cap(ceiling, pipeline.full)
        encoder = encoder_beside(pipeline, chosen, cap)

    def phase(full: Forecast | None, name: str) -> tuple[int, int]:
        if full is None or name not in full.phases:
            return 0, 0
        entry = full.phases[name]
        return int(entry.device_peak), int(entry.host_peak)

    pipe_full = pipeline.full
    enc_full = encoder.full if encoder is not None else None
    denoise_vram, denoise_ram = phase(pipe_full, large_model.PHASE_DENOISE)
    decode_vram, decode_ram = phase(pipe_full, large_model.PHASE_DECODE)
    alone_vram, alone_ram = phase(enc_full, large_model.PHASE_ENCODE)
    resting_pipeline = pipe_full.phases[large_model.PHASE_DENOISE].device_weights if pipe_full is not None else 0
    resting_pipeline_host = pipe_full.phases[large_model.PHASE_DENOISE].host_weights if pipe_full is not None else 0
    encoder_idle_host = int(enc_full.phases["idle"].host_weights) if enc_full is not None else 0
    phases = {
        # The encoder runs while the pipeline already sits in its denoise state.
        "encode": {"vram_bytes": int(resting_pipeline + alone_vram) if enc_full else 0, "ram_bytes": int(resting_pipeline_host + alone_ram) if enc_full else 0},
        "encode_standalone": {"vram_bytes": alone_vram, "ram_bytes": alone_ram},
        "denoise": {"vram_bytes": denoise_vram, "ram_bytes": denoise_ram + encoder_idle_host},
        "decode": {"vram_bytes": decode_vram, "ram_bytes": decode_ram + encoder_idle_host},
    }

    pipe_inventories = _inventories(pipeline_spec, options)
    transformer_bytes = pipe_inventories[large_model.TRANSFORMER].bytes_total
    vae_bytes = pipe_inventories[large_model.VAE].bytes_total
    encoder_bytes = 0
    if encoder_installed:
        encoder_bytes = _inventories(_encoder_spec(encoder_options), encoder_options)[large_model.TEXT_ENCODER].bytes_total
    activations = 0
    if pipe_full is not None:
        activations = sum(int(pipe_full.phases[name].device_activation) for name in (large_model.PHASE_DENOISE, large_model.PHASE_DECODE))
    noisy = (int(region_width) // 16) * (int(region_height) // 16)
    reference = noisy if with_reference else 0
    attention_mask_bytes = attention.text_attention_mask_bytes(options.max_sequence_length, noisy, reference, normalized["dtype"]) if normalized["text_attention_in_mask"] else 0
    breakdown = {
        "transformer": int(transformer_bytes),
        "text_encoder": int(encoder_bytes),
        "vae": int(vae_bytes),
        "activations": int(activations),
        "peak_encode": max(phases["encode"]["vram_bytes"], phases["encode"]["ram_bytes"]),
        "peak_denoise": phases["denoise"]["vram_bytes"],
        "peak_decode": phases["decode"]["vram_bytes"],
    }
    if attention_mask_bytes:
        breakdown["attention_mask"] = int(attention_mask_bytes)
    calibrated = all(part.full.calibrated for part in (pipeline, encoder) if part is not None and part.full is not None)
    return {
        "vram_bytes": max(entry["vram_bytes"] for entry in phases.values()),
        "ram_bytes": max(entry["ram_bytes"] for entry in phases.values()),
        "phases": phases,
        "resident": {"pipeline_device": int(resting_pipeline), "text_encoder_host": int(encoder_idle_host)},
        "breakdown": breakdown,
        **steps_fields(chosen, pipeline.steps),
        "fits": pipeline.fits and (encoder is None or encoder.fits),
        "calibrated": calibrated,
    }


def _preset_advice(
    normalized: dict[str, Any],
    region_width: int,
    region_height: int,
    memory: dict[str, int],
    *,
    with_reference: bool = False,
) -> str:
    """One sentence naming the concrete step pairs whose plan fits `memory` right now (computed, never hard-coded), `STEP_WALK` order."""
    fitting = [
        f"«{VRAM_STEP_LABELS[pair.vram]} + {RAM_STEP_LABELS[pair.ram]}»"
        for pair in STEP_WALK
        if forecast_memory(normalized, region_width, region_height, with_reference=with_reference, memory=memory, steps=pair)["fits"]
    ]
    if not fitting:
        return (
            "Ни одно сочетание «Видеопамять» и «Оперативная память» сейчас не помещается: "
            "уменьшите выделенную область или освободите память, закрыв другие программы."
        )
    return "Сейчас помещаются: " + ", ".join(fitting) + "."
