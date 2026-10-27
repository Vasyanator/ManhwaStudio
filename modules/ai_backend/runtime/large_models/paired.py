"""
File: modules/ai_backend/runtime/large_models/paired.py

Purpose:
The two-handle memory rule of a model service that runs ONE operation over two
large-model handles: a PIPELINE handle (admitted first) and a TEXT-ENCODER
handle (admitted second, on what the pipeline's reservation left). It owns the
combined VRAM-axis ceiling of the pair, the encoder's device cap beside an
admitted pipeline, the reservation / host-baseline arithmetic and the
three-step `large_models.profiles` composition with its wire answer. Model
packages (FLUX.2 klein, FLUX.3 klein) name their phases through `PairPhases`;
nothing here names a model family.

Main responsibilities:
- `PairPhases` - which pipeline phases bound the run's device peak, the
  pipeline phase whose device weights sit beside the encode (`rest`), and the
  encoder's phase (`encode`);
- `HandleRequest` - one handle of a request (registered model, options, weight
  format); `EncoderNeed` - what a request asks of the encoder handle
  (`counted`: the one rule of whether a forecast counts it);
- the combined VRAM-axis rule (module notes): `encode_moment_device`,
  `combined_device_peak`,
  `encoder_device_cap`, `combined_ceilings`, `live_ceilings` (the ceilings over
  a live manager) and `encoder_cap_beside` (the cap of an admitted pipeline
  session);
- `host_baseline` / `reservation_of` / `with_host_baseline` - what one
  admission reserves and the baseline RSS inside a forecast's host peak;
- `plan_pair` -> `PairedReport` (`to_wire`: the `large_models.profiles` answer
  of a two-handle model; `run_device_peak` / `run_host_peak`: the run's peaks
  as admitted).

Notes:
- Combined VRAM-axis monotonicity (the two handles together): the planner
  keeps each handle monotone on its own, but the encoder is admitted on what
  the pipeline's RESERVATION leaves, and its MAX / PHASE templates fill that
  room. A lower step whose pipeline reserves less would hand the encoder more
  room, and the encode moment (the pipeline's `rest`-state weights + the
  encoder) would forecast more device memory than the higher step. So a run's
  encoder is admitted with a device ceiling (`acquire(...,
  device_cap_bytes=)`): the combined device peak of the nearest fitting HIGHER
  VRAM step at the same RAM step (each planned under its own ceiling, the chain
  `combined_ceilings`), minus this step's pipeline weights in the `rest` state
  (`encoder_device_cap`). Hence, for the same request and budgets, a lower VRAM
  step never forecasts more device bytes than a higher one for the two handles
  together. A host run device has no VRAM axis: no ceiling.
- The ceilings are planned BEFORE the run's pipeline holds a reservation (the
  manager would claim it a second time); `live_ceilings` is read-only.
- Everything here is read-only over the manager (`profiles` only): it never
  acquires, loads or reserves, and never takes a service lock.
"""

from __future__ import annotations

import logging
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from .forecast import Forecast
from .manager import LargeModelManager, ProfilesReport, Session
from .planner import Planned, PlanResult
from .spec import RequestShape
from .tiers import IDLE_PHASE, VRAM_ORDER, RamStep, Steps, VramStep

log = logging.getLogger(__name__)


@dataclass(frozen=True)
class PairPhases:
    """The phase names the two-handle rule reads.

    `pipeline`: the pipeline phases whose device peaks bound the run (the encode
    moment aside); `rest`: the pipeline phase whose device weights sit on the
    card while the encoder runs (where a cold run encodes); `encode`: the
    encoder handle's phase.
    """

    pipeline: tuple[str, ...]
    rest: str
    encode: str


@dataclass(frozen=True)
class HandleRequest:
    """One handle of a request: the registered `model`, its spec `options` and the weight `format` to plan."""

    model: str
    options: Any
    format: str


@dataclass(frozen=True)
class EncoderNeed:
    """What a request asks of the text encoder handle.

    `encodes`: an encoder is installed and some text the run needs is missing
    from the prompt cache — the run will load the encoder and encode.
    `resident`: the encoder handle this request would use holds bytes in RAM or
    VRAM right now.
    """

    encodes: bool
    resident: bool

    @property
    def counted(self) -> bool:
        """Whether the forecast counts the encoder: left out ONLY when it is not resident AND the cache covers every text."""
        return self.encodes or self.resident


def encode_moment_device(pipeline: Forecast, encoder: Forecast | None, phases: PairPhases) -> int:
    """Device bytes while the encoder runs (`0` when nothing is encoded: `encoder=None` or no `phases.encode` phase).

    The pipeline's `phases.rest`-state device weights plus the encoder's
    `phases.encode` device peak (its own runtime overhead included). An encoder
    with nothing on the device (a CPU-pinned one: device peak `0`) adds the
    pipeline's `device_overhead` instead, so the moment still holds the
    device context exactly once.
    """
    if encoder is None or phases.encode not in encoder.phases:
        return 0
    encode = int(encoder.phases[phases.encode].device_peak)
    return int(pipeline.phases[phases.rest].device_weights) + (encode if encode > 0 else int(pipeline.device_overhead))


def combined_device_peak(pipeline: Forecast, encoder: Forecast | None, phases: PairPhases) -> int:
    """The device peak of one run of both handles (bytes): the pipeline's `phases.pipeline` peaks, and the encode moment (`encode_moment_device`).

    `encoder=None` (or an encoder forecast without that phase) = nothing is encoded.
    """
    peak = max(int(pipeline.phases[name].device_peak) for name in phases.pipeline)
    return max(peak, encode_moment_device(pipeline, encoder, phases))


def encoder_device_cap(ceiling: int | None, pipeline: Forecast, phases: PairPhases) -> int | None:
    """The encoder's device ceiling beside `pipeline`'s plan (bytes; `None` = none): `ceiling` minus the pipeline's `phases.rest`-state device weights, at least 0."""
    if ceiling is None:
        return None
    return max(0, ceiling - int(pipeline.phases[phases.rest].device_weights))


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


def host_baseline(forecast: Forecast) -> int:
    """The backend's baseline RSS a forecast's host peaks include, read back from its idle state (`host_peak` = baseline + host weights there).

    The manager's baseline takes EVERY large-pool holding out
    (`Budgets.baseline_rss`), so the pipeline's and the encoder's forecasts of
    one request carry the same figure (up to the RSS moving between the two
    probes) and neither contains the other handle's resident bytes.
    """
    idle = forecast.phases[IDLE_PHASE]
    return max(0, int(idle.host_peak) - int(idle.host_weights))


def reservation_of(admitted: Forecast) -> tuple[int, int]:
    """`(device bytes, host growth)` a plan's admission reserves: its device peak, and its host peak above the baseline RSS (`host_baseline`, the figure the manager subtracts)."""
    return int(admitted.device_peak), max(0, int(admitted.host_peak) - host_baseline(admitted))


def _shown_forecast(result: PlanResult) -> Forecast | None:
    """The forecast a report shows for `result`: a plan's full one, a refusal's closest attempt (`None` when it has none)."""
    return result.full_forecast if isinstance(result, Planned) else result.forecast


def with_host_baseline(selected: dict[str, Any], result: PlanResult) -> dict[str, Any]:
    """`selected` (a report's wire `selected`) with `forecast.host_baseline` added: the baseline RSS inside its `host_peak`.

    Additive (protocol 21): the client sums the pipeline's and the encoder's
    host peaks, and each one already includes the backend process once; it
    subtracts one baseline. The baseline holds no large-pool bytes, so a
    resident encoder is counted once, by its own forecast. `result` is the `PlanResult` `selected` was
    serialized from (its shown forecast: a plan's full one, a refusal's
    closest attempt). A `selected` without a forecast is returned unchanged.
    """
    forecast = _shown_forecast(result)
    if forecast is None or selected.get("forecast") is None:
        return selected
    return {**selected, "forecast": {**selected["forecast"], "host_baseline": host_baseline(forecast)}}


def live_ceilings(manager: LargeModelManager, alone: ProfilesReport, encoder: HandleRequest, shape: RequestShape, device: str, phases: PairPhases, *, until: VramStep) -> tuple[RamStep, dict[VramStep, int | None]] | None:
    """`(RAM step, combined_ceilings)` of the pipeline report `alone`'s VRAM column, with `manager`'s live budgets; `None` when there is no VRAM axis to keep monotone.

    Each higher cell is the pipeline's grid row (`alone.vram_rows`) and the
    `encoder` admitted beside its reservation under that cell's own ceiling,
    exactly as a run of that cell would admit it. Call it BEFORE the run's
    pipeline holds a reservation (the manager would claim it a second time).
    Read-only.
    """
    if not alone.vram_applicable or not isinstance(alone.selected, Planned):
        return None
    ram = alone.selected.plan.steps.ram

    def combined_at(cell: Steps, ceiling: int | None) -> int | None:
        row = alone.vram_rows.get(cell.vram)
        if not isinstance(row, Planned):
            return None
        claim = reservation_of(row.forecast)
        beside = manager.profiles(encoder.model, encoder.options, shape, device=device, format=encoder.format, steps=cell, claimed_device_bytes=claim[0], claimed_host_bytes=claim[1], device_cap_bytes=encoder_device_cap(ceiling, row.full_forecast, phases))
        if not isinstance(beside.selected, Planned):
            return None
        return combined_device_peak(row.full_forecast, beside.selected.full_forecast, phases)

    return ram, combined_ceilings(ram, combined_at, until=until)


def encoder_cap_beside(ceilings: tuple[RamStep, dict[VramStep, int | None]] | None, pipeline: Session, phases: PairPhases, *, owner: str) -> int | None:
    """The encoder's device ceiling beside the admitted `pipeline` session (`encoder_device_cap`); `None` without ceilings.

    The ceilings were computed for the RAM step the pre-admission plan chose
    (`live_ceilings`); a session admitted at another RAM step (the budgets
    moved in between) gets no ceiling, logged under `owner` (the model's
    display name, e.g. "FLUX.2 klein").
    """
    if ceilings is None:
        return None
    ram, table = ceilings
    effective = pipeline.steps_effective
    if effective.ram is not ram:
        log.info("%s: пайплайн принят на шаге ОЗУ «%s», а потолок энкодера считался для «%s»; энкодер принимается без потолка.", owner, effective.ram.value, ram.value)
        return None
    return encoder_device_cap(table.get(effective.vram), pipeline.forecast, phases)


@dataclass(frozen=True)
class PairedReport:
    """The three planning steps of a two-handle `large_models.profiles` answer (`plan_pair`).

    `alone`: the pipeline planned alone (what a run admits first); `encoder`:
    the encoder beside `alone`'s reservation under the combined ceiling
    (`None` = the encoder is not counted); `combined`: the pipeline planned
    again with the encoder's reservation claimed (`None` when the encoder is
    not counted or was refused).
    """

    alone: ProfilesReport
    encoder: ProfilesReport | None
    combined: ProfilesReport | None

    def to_wire(self) -> dict[str, Any]:
        """The wire answer: the pipeline's `ProfilesReport.to_dict()` plus the additive `encoder` and `encoder_formats`.

        `selected` is the pipeline's plan (of `combined` when there is one, else
        of `alone`), so the run's total is `selected` + `encoder`, summed by the
        client; each forecast carries `host_baseline` (`with_host_baseline`).
        `encoder` is the encoder's `selected` (`null` when not counted) and
        `encoder_formats` its `formats` rows (`[]` when not counted). A refused
        encoder makes `selected.fits` false with the encoder's `reason` /
        `shortest_resource` and no step pair.
        """
        if self.encoder is None:
            answer = self.alone.to_dict()
            return {**answer, "selected": with_host_baseline(answer["selected"], self.alone.selected), "encoder": None, "encoder_formats": []}
        encoder_report = self.encoder.to_dict()
        encoder_selected = with_host_baseline(encoder_report["selected"], self.encoder.selected)
        encoder_formats = encoder_report["formats"]
        if self.combined is None:
            answer = self.alone.to_dict()
            answer["selected"] = {**with_host_baseline(answer["selected"], self.alone.selected), "fits": False, "vram": None, "ram": None, "needs_reclaim": None, "reason": encoder_selected["reason"], "shortest_resource": encoder_selected["shortest_resource"]}
            return {**answer, "encoder": encoder_selected, "encoder_formats": encoder_formats}
        answer = self.combined.to_dict()
        return {**answer, "selected": with_host_baseline(answer["selected"], self.combined.selected), "encoder": encoder_selected, "encoder_formats": encoder_formats}

    def run_device_peak(self, phases: PairPhases) -> int | None:
        """The device peak of one run as it is admitted (bytes): `combined_device_peak` of `alone`'s plan and the encoder's beside it.

        `None` when the pipeline or a counted encoder is refused. Consumers:
        the FLUX.3 observer and calibration (`forecast_memory`'s `vram_bytes`
        is the FLUX.2 offline counterpart).
        """
        if not isinstance(self.alone.selected, Planned):
            return None
        encoder: Forecast | None = None
        if self.encoder is not None:
            if not isinstance(self.encoder.selected, Planned):
                return None
            encoder = self.encoder.selected.full_forecast
        return combined_device_peak(self.alone.selected.full_forecast, encoder, phases)

    def run_host_peak(self) -> int | None:
        """The host peak of one run as it is admitted (bytes): `alone`'s plus the encoder's, the baseline RSS counted once.

        The baseline excludes every large-pool holding (`host_baseline`), so
        each handle's resident bytes are counted once, by its own forecast:
        the figure is the backend with only this pair loaded. An upper bound
        only because the two handles' host peaks need not coincide; `None`
        when the pipeline or a counted encoder is refused.
        """
        if not isinstance(self.alone.selected, Planned):
            return None
        pipeline = self.alone.selected.full_forecast
        if self.encoder is None:
            return int(pipeline.host_peak)
        if not isinstance(self.encoder.selected, Planned):
            return None
        encoder = self.encoder.selected.full_forecast
        return int(pipeline.host_peak) + max(0, int(encoder.host_peak) - host_baseline(encoder))


def plan_pair(manager: LargeModelManager, pipeline: HandleRequest, encoder: HandleRequest | None, shape: RequestShape, device: str, steps: Steps, phases: PairPhases) -> PairedReport:
    """The three-step planning of a two-handle request on `manager` (read-only; never waits for a service lock).

    (1) the pipeline alone; (2) when `encoder` is given (`None` = not counted,
    `EncoderNeed.counted`), the encoder with (1)'s reservation claimed, under
    the same requested `steps`, with the combined VRAM-axis ceiling a run would
    give it (`live_ceilings`, module notes); (3) when (2) plans, the pipeline
    again with the encoder's reservation claimed. Every call plans the handle's
    own `format`.

    # Raises
    `ValueError` from `LargeModelManager.profiles` (an unregistered model, a
    format the model lacks, an invalid claim).
    """
    alone = manager.profiles(pipeline.model, pipeline.options, shape, device=device, format=pipeline.format, steps=steps)
    if encoder is None:
        return PairedReport(alone, None, None)
    pipeline_claim = reservation_of(alone.selected.forecast) if isinstance(alone.selected, Planned) else (0, 0)
    cap = None
    if isinstance(alone.selected, Planned):
        selected_vram = alone.selected.plan.steps.vram
        ceilings = live_ceilings(manager, alone, encoder, shape, device, phases, until=selected_vram)
        cap = encoder_device_cap(ceilings[1][selected_vram], alone.selected.full_forecast, phases) if ceilings is not None else None
    beside = manager.profiles(encoder.model, encoder.options, shape, device=device, format=encoder.format, steps=steps, claimed_device_bytes=pipeline_claim[0], claimed_host_bytes=pipeline_claim[1], device_cap_bytes=cap)
    if not isinstance(beside.selected, Planned):
        return PairedReport(alone, beside, None)
    encoder_claim = reservation_of(beside.selected.forecast)
    combined = manager.profiles(pipeline.model, pipeline.options, shape, device=device, format=pipeline.format, steps=steps, claimed_device_bytes=encoder_claim[0], claimed_host_bytes=encoder_claim[1])
    return PairedReport(alone, beside, combined)
