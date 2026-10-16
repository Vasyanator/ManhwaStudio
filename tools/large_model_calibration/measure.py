"""
File: tools/large_model_calibration/measure.py

Purpose:
FLUX.3 klein side of the calibration tool: runs one matrix combo through the
REAL `Flux3KleinService.generate` and records every step through the
service's `PhaseObserver` hook (`recorder.StepRecorder`), annotating each
model phase with the work the forecast charges for it.

Key declarations:
- `EncoderSetting`, `ENCODER_SETTINGS` - the `--encoder-modes` axis
- `Combo` - one matrix point (incl. the two resource steps); `ComboOutcome` -
  what one run produced (incl. the steps it ended on, the manager's full
  forecast of the requested cell before the run)
- `forecast_combo()` - the manager's answer for the combo's requested steps
  (`LargeModelManager.profiles` `selected`: the FULL forecast, every
  activation counted, or the refusal), read before the admission
- `WorkCapture` - per model phase the spec's own `PhaseWork` (the `x` of
  every cost-table fit) plus the cost-table key, captured from the manager
  calls the service makes
- `run_combo()`, `reset_device()`

Notes:
- "Cold start" means no FLUX.3 weights are held anywhere before the combo
  (`service.unload()` drops every handle from the manager + allocator cache
  release); the files stay in the OS page cache after their first read, so
  the first combo of a variant pays the disk read, later ones do not.
- torch is imported inside functions (the dry run must not import it).
"""

from __future__ import annotations

import gc
import logging
import time
import traceback
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any, Literal

from modules.ai_backend.inpaint.flux3_klein import (
    AeDtype,
    EncoderMode,
    Flux3Options,
    Flux3Request,
    Fp8Compute,
    WeightFormat,
)
from modules.ai_backend.runtime.large_models.tiers import RamStep, Steps, VramStep

from .cases import Case
from .recorder import GIB, StepRecord, StepRecorder, rss_private

if TYPE_CHECKING:
    from PIL.Image import Image

    from modules.ai_backend.inpaint.flux3_klein import Flux3KleinService

log = logging.getLogger(__name__)


@dataclass(frozen=True)
class EncoderSetting:
    """One value of the `--encoder-modes` axis: an encoder mode (where the encoder lives is the steps' choice).

    `needs_references` settings are skipped (and logged) for cases without
    references, where the service refuses them.
    """

    label: str
    mode: EncoderMode

    @property
    def needs_references(self) -> bool:
        """True for the VL experiment, which the service refuses without references."""
        return self.mode is EncoderMode.VL_REFS


#: Every `--encoder-modes` value; `text` and `vl_refs` are a like-for-like
#: pair (same seed, caption and steps).
ENCODER_SETTINGS: dict[str, EncoderSetting] = {
    setting.label: setting
    for setting in (
        EncoderSetting("text", EncoderMode.TEXT),
        EncoderSetting("vl_refs", EncoderMode.VL_REFS),
    )
}

#: The `--encoder-modes` default.
DEFAULT_ENCODER_LABELS = ("text",)

#: The two settings whose images are paired side by side in `compare/`.
COMPARE_PAIR = ("text", "vl_refs")


@dataclass(frozen=True)
class Combo:
    """One FLUX.3 matrix point. `size` `None` leaves width / height to the service
    (sampler default without references, else the first reference's size).
    `vram_step` / `ram_step` are `Flux3Options.vram_step` / `ram_step`;
    `ref_max_pixels` is `Flux3Options.reference_max_pixels` (`None`: the
    reference caps)."""

    variant: WeightFormat
    size: tuple[int, int] | None
    case: Case
    encoder: EncoderSetting
    ae_dtype: AeDtype
    vram_step: VramStep = VramStep.AUTO
    ram_step: RamStep = RamStep.AUTO
    ref_max_pixels: int | None = None

    @property
    def steps(self) -> Steps:
        """The requested step pair."""
        return Steps(self.vram_step, self.ram_step)

    @property
    def _cap_suffix(self) -> str:
        """`__ref-<N>px` when a reference cap is set."""
        return "" if self.ref_max_pixels is None else f"__ref-{self.ref_max_pixels}px"

    @property
    def _offload_suffix(self) -> str:
        """`__vram-<step>__ram-<step>`, plus `__ref-<N>px` when a reference cap is set."""
        return f"__vram-{self.vram_step.value}__ram-{self.ram_step.value}{self._cap_suffix}"

    @property
    def size_label(self) -> str:
        """`auto` or `WxH`."""
        return "auto" if self.size is None else f"{self.size[0]}x{self.size[1]}"

    @property
    def combo_id(self) -> str:
        """ASCII file-name-safe identifier (case names are ASCII by construction)."""
        return (
            f"{self.variant.value}__{self.size_label}__{self.case.name}__{self.encoder.label}"
            f"__ae-{self.ae_dtype.value}{self._offload_suffix}"
        )

    @property
    def grid_key(self) -> str:
        """Identifier of the combo minus its two steps: the cells of one VRAM x RAM grid share it."""
        return (
            f"{self.variant.value}__{self.size_label}__{self.case.name}__{self.encoder.label}"
            f"__ae-{self.ae_dtype.value}{self._cap_suffix}"
        )

    @property
    def pair_key(self) -> str:
        """Identifier of the combo minus its encoder setting (the `compare/` grouping)."""
        return f"{self.variant.value}__{self.size_label}__{self.case.name}__ae-{self.ae_dtype.value}{self._offload_suffix}"

    def describe(self) -> dict[str, Any]:
        """The report's descriptive fields of this combo."""
        return {
            "family": "flux3_klein",
            "variant": self.variant.value,
            "size": self.size_label,
            "case": self.case.name,
            "case_kind": self.case.kind.value,
            "references": list(self.case.references),
            "encoder": self.encoder.label,
            "encoder_mode": self.encoder.mode.value,
            "ae_dtype": self.ae_dtype.value,
            "vram_step": self.vram_step.value,
            "ram_step": self.ram_step.value,
            "reference_max_pixels": self.ref_max_pixels,
        }


@dataclass
class ComboOutcome:
    """What one combo produced. `status` is `ok`, `oom` or `error`;
    `failed_phase` is the step that ended with `failed=True` (`None` when the
    failure happened before the first step, e.g. a refused request).
    `combo` is a FLUX.3 `Combo` or a FLUX.2 `flux2.Flux2Combo` (both have
    `combo_id`, `grid_key`, `vram_step`, `ram_step`, `describe()`).
    `first_in_process`: the first combo of the tool's process that ran a step, whose
    host growth also holds the process's one-time initialisation (imports, device
    libraries)."""

    combo: Any
    status: Literal["ok", "oom", "error"]
    phases: list[StepRecord] = field(default_factory=list)
    failed_phase: str | None = None
    error: str | None = None
    traceback: str | None = None
    image_path: Path | None = None
    image_mean: float | None = None
    image_std: float | None = None
    width: int | None = None
    height: int | None = None
    seed: int | None = None
    text_tokens: int | None = None
    text_real_tokens: int | None = None
    fp8_applied: str | None = None
    compute_path: str | None = None
    vram_effective: str | None = None
    ram_effective: str | None = None
    degraded: bool | None = None
    planned: dict[str, Any] = field(default_factory=dict)
    rss_private_before: int | None = None
    wall_s: float = 0.0
    first_in_process: bool = False


def reset_device(service: Any, device: str | None) -> None:
    """Cold start: drop everything the service holds, collect, release the allocator cache."""
    import torch

    dropped = service.unload()
    gc.collect()
    if device is not None and torch.device(device).type == "cuda":
        with torch.cuda.device(torch.device(device)):
            torch.cuda.empty_cache()
    log.debug("cold reset (service held weights: %s)", dropped)


def image_stats(image: Image) -> tuple[float, float]:
    """Mean and standard deviation of all RGB channel values (0..255)."""
    import numpy as np

    array = np.asarray(image.convert("RGB"), dtype=np.float32)
    return float(array.mean()), float(array.std())


def work_dict(spec: Any, phase_name: str, work: Any, *, weight_format: str, compute_path: str, block_bytes: list[int] | None = None) -> dict[str, Any]:
    """The `work` annotation of one model phase: the cost-table key (minus `exec`, read from the
    placements), the compute component and its role, the spec's `PhaseWork` numbers the
    forecast charges, and `block_bytes` (the compute component's block sizes in residency
    order, `None` when unknown: what a juggled phase stages per slot)."""
    phase_spec = next(phase for phase in spec.phases if phase.name == phase_name)
    component = next(component for component in spec.components if component.name == phase_spec.compute)
    return {
        "family": spec.family,
        "phase": phase_name,
        "variant": work.variant,
        "format": weight_format,
        "compute_path": compute_path,
        "compute": phase_spec.compute,
        "role": component.role.value,
        "tokens": int(work.tokens),
        "pixels": int(work.pixels),
        "passes": int(work.passes),
        "extra_device_bytes": int(work.extra_device_bytes),
        "block_bytes": block_bytes,
    }


def _block_bytes(spec: Any, options: Any, weight_format: str) -> dict[str, list[int]]:
    """`{component: block sizes in residency order}` from the safetensors headers (the manager's
    own inventory); empty when a header cannot be read (logged; juggled phases then stay out of
    the byte fits)."""
    from modules.ai_backend.runtime.large_models.inventory import build_inventories

    try:
        inventories = build_inventories(spec, options, weight_format)
    except (OSError, ValueError) as exc:
        log.warning("work capture: inventories of %s unavailable (%s)", spec.family, exc)
        return {}
    return {name: [int(inventory.bytes_by_block[block]) for block in inventory.block_names] for name, inventory in inventories.items()}


class WorkCapture:
    """Records what a measured service asks the manager for, so every phase can be annotated with
    exactly the work the forecast charges: the spec of every model registered through it and the
    `(model, options, shape, format)` of every admission.

    It wraps `register` / `acquire` of ONE manager instance (a measurement-only
    instrument: the manager's behaviour is unchanged). Install it before the
    service registers its specs; `clear()` between combos.
    """

    def __init__(self, large_models: Any) -> None:
        self.specs: dict[str, Any] = {}
        self.acquired: list[tuple[str, Any, Any, str]] = []
        register, acquire = large_models.register, large_models.acquire

        def spy_register(spec: Any, *, model: str | None = None) -> None:
            register(spec, model=model)
            self.specs[model or spec.family] = spec

        def spy_acquire(model: str, options: Any, shape: Any, **kwargs: Any) -> Any:
            session = acquire(model, options, shape, **kwargs)
            self.acquired.append((model, options, shape, kwargs["format"]))
            return session

        large_models.register = spy_register
        large_models.acquire = spy_acquire

    def clear(self) -> None:
        """Forget the admissions recorded so far."""
        self.acquired.clear()

    def works(self) -> dict[tuple[str, str], dict[str, Any]]:
        """`{(model, phase): work_dict + "model"}` of every admission recorded since `clear` (the first one of a model wins)."""
        works: dict[tuple[str, str], dict[str, Any]] = {}
        for model, options, shape, weight_format in self.acquired:
            spec = self.specs.get(model)
            if spec is None:
                log.warning("work capture: model %s was registered before the capture; its phases stay unannotated", model)
                continue
            compute_path = spec.formats(options)[weight_format].compute_path
            blocks = _block_bytes(spec, options, weight_format)
            for phase in spec.phases:
                works.setdefault((model, phase.name), {**work_dict(spec, phase.name, phase.work(shape, options), weight_format=weight_format, compute_path=compute_path, block_bytes=blocks.get(phase.compute)), "model": model})
        return works

    def annotate(self, records: list[StepRecord]) -> None:
        """Set `work` of every model-phase record: by `(record.model, step)`, or by step alone when
        the observer names no model and one model was admitted (FLUX.3)."""
        works = self.works()
        models = {model for model, _phase in works}
        for record in records:
            model = record.model or (next(iter(models)) if len(models) == 1 else "")
            record.work = works.get((model, record.step))


def forecast_combo(service: Flux3KleinService, large_models: Any, request: Flux3Request, options: Flux3Options, rss_before: int | None) -> dict[str, Any]:
    """The manager's answer for `request` / `options` before the admission (read-only: plans, never loads).

    Resolves the request through the service's own pre-admission shaping
    (`Flux3KleinService._plan`: the same model, model options, request shape
    and device the admission uses), then `LargeModelManager.profiles` with the
    requested steps; `selected` is what the admission would plan (floor and
    Q-P1 applied). Keys: `fits`, `vram` / `ram` (resolved steps), `same_as_vram`
    / `same_as_ram`, `reason` / `shortest_resource` (refusal),
    `full_device_peak`, `full_host_peak` (absolute RSS), `host_growth`
    (`full_host_peak` - `rss_before`), `device_calibrated`. A failure here
    (the request itself is invalid) is recorded as `error`; `generate` then
    fails the same way.
    """
    try:
        plan = service._plan(request, options)
        report = large_models.profiles(plan.model, plan.model_options, plan.shape, device=plan.device, format=options.variant.value, steps=options.steps)
    except Exception as exc:  # noqa: BLE001 - recorded beside the combo; the generation reports the same failure
        log.warning("forecast of %s failed: %s", options.steps.label, exc)
        return {"error": f"{type(exc).__name__}: {exc}"}
    return selected_answer(report.selected, rss_before, options.steps.label)


def selected_answer(selected: Any, rss_before: int | None, label: str) -> dict[str, Any]:
    """The JSON answer of one `profiles(...).selected` (`Planned` or `PlanRefusal`), see `forecast_combo`."""
    from modules.ai_backend.runtime.large_models.planner import Planned

    planned = selected if isinstance(selected, Planned) else None
    shown = planned.full_forecast if planned is not None else selected.forecast
    answer: dict[str, Any] = {
        "fits": planned is not None,
        "vram": planned.plan.steps.vram.value if planned is not None else None,
        "ram": planned.plan.steps.ram.value if planned is not None else None,
        "same_as_vram": planned.same_as_vram.value if planned is not None and planned.same_as_vram is not None else None,
        "same_as_ram": planned.same_as_ram.value if planned is not None and planned.same_as_ram is not None else None,
        "reason": None if planned is not None else selected.reason,
        "shortest_resource": None if planned is not None else selected.shortest_resource,
        "full_device_peak": shown.device_peak if shown is not None else None,
        "full_host_peak": shown.host_peak if shown is not None else None,
        "host_growth": shown.host_peak - rss_before if shown is not None and rss_before is not None else None,
        "device_calibrated": shown.device_calibrated if shown is not None else None,
    }
    log.info(
        "forecast %s -> %s: device %s GiB, host +%s GiB%s",
        label,
        f"{answer['vram']}/{answer['ram']}" if planned is not None else f"REFUSED ({answer['reason']}, {answer['shortest_resource']})",
        "n/a" if answer["full_device_peak"] is None else f"{answer['full_device_peak'] / GIB:.2f}",
        "n/a" if answer["host_growth"] is None else f"{answer['host_growth'] / GIB:.2f}",
        "" if answer["same_as_vram"] is None and answer["same_as_ram"] is None else f" (same as vram {answer['same_as_vram']} / ram {answer['same_as_ram']})",
    )
    return answer


def run_combo(
    service: Flux3KleinService,
    large_models: Any,
    capture: WorkCapture,
    combo: Combo,
    references: tuple[Image, ...],
    *,
    model_root: Path,
    device: str | None,
    measure_device: str,
    fp8_compute: Fp8Compute,
    vl_ref_max_pixels: int,
    caption: str,
    seed: int,
    steps: int | None,
    image_path: Path,
    baseline_used: int | None = None,
) -> ComboOutcome:
    """Generate `combo` once through `service.generate` with a `StepRecorder` on `measure_device`.

    First records the manager's full forecast of the requested cell
    (`forecast_combo`); the phases' work comes from `capture` (installed on
    `large_models` before the service). Saves the PNG to
    `image_path` on success. Never raises for a generation failure: an
    out-of-memory error becomes `status="oom"`, any other exception
    `status="error"` (both logged with the traceback, the step recorded). The
    caller unloads after a failure.
    """
    import torch

    options = Flux3Options(
        model_root=model_root,
        variant=combo.variant,
        encoder_mode=combo.encoder.mode,
        fp8_compute=fp8_compute,
        ae_dtype=combo.ae_dtype,
        vl_ref_max_pixels=vl_ref_max_pixels,
        device=device,
        reference_max_pixels=combo.ref_max_pixels,
        vram_step=combo.vram_step,
        ram_step=combo.ram_step,
    )
    width, height = combo.size if combo.size is not None else (None, None)
    request = Flux3Request(caption=caption, references=references, width=width, height=height, seed=seed, steps=steps)
    recorder = StepRecorder(measure_device, baseline_used=baseline_used, status=large_models.status)
    rss_before = rss_private()
    planned = forecast_combo(service, large_models, request, options, rss_before)
    capture.clear()
    started = time.monotonic()
    outcome: ComboOutcome
    try:
        result = service.generate(request, options, observer=recorder)
    except torch.OutOfMemoryError as exc:
        outcome = ComboOutcome(combo=combo, status="oom", error=str(exc), traceback=traceback.format_exc())
        log.error("combo %s: OUT OF MEMORY: %s", combo.combo_id, exc)
    except Exception as exc:  # noqa: BLE001 - recorded in the report and logged with traceback; the run goes on
        outcome = ComboOutcome(combo=combo, status="error", error=f"{type(exc).__name__}: {exc}", traceback=traceback.format_exc())
        log.exception("combo %s failed", combo.combo_id)
    else:
        image_path.parent.mkdir(parents=True, exist_ok=True)
        result.image.save(image_path)
        mean, std = image_stats(result.image)
        outcome = ComboOutcome(
            combo=combo,
            status="ok",
            image_path=image_path,
            image_mean=mean,
            image_std=std,
            width=result.width,
            height=result.height,
            seed=result.seed,
            text_tokens=result.text_tokens,
            text_real_tokens=result.text_real_tokens,
            fp8_applied=result.applied.fp8_compute.value if result.applied.fp8_compute is not None else None,
            compute_path=result.applied.compute_path,
            vram_effective=result.applied.steps_effective.vram.value,
            ram_effective=result.applied.steps_effective.ram.value,
            degraded=result.applied.degraded,
        )
        log.info("combo %s: ok %dx%d, image mean %.1f std %.1f -> %s", combo.combo_id, result.width, result.height, mean, std, image_path)
    finally:
        recorder.close()
    capture.annotate(recorder.records)
    outcome.planned = planned
    outcome.phases = recorder.records
    outcome.rss_private_before = rss_before
    outcome.wall_s = time.monotonic() - started
    failed = [record.step for record in recorder.records if record.failed or not record.ended]
    outcome.failed_phase = failed[-1] if failed else None
    return outcome
