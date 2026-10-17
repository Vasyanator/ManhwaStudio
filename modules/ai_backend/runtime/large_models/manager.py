"""
File: modules/ai_backend/runtime/large_models/manager.py

Purpose:
`LargeModelManager` — the large-model pool of the backend (FLUX.2 / FLUX.3
klein): admits a run with a budget-checked residency plan, drives its phases
through the mechanics (loader, moves, hooks) as a `Session`, recovers from
memory failures along the degrade ladder, keeps the model warm between runs as
the user's two resource axes allow («Видеопамять» `VramStep`, «Оперативная
память» `RamStep`), and is the large pool's `PoolPolicy` under the shared
residency arbiter (the small pool's peer; neither imports the other).

Main responsibilities:
- registration of `LargeModelSpec`s; one handle per residency key
  (`LargeModelSpec.key`): its byte `Ledger`, its loaded components, its
  transport;
- `acquire(...)` -> `Session`: lease (`arbiter.begin_use(LARGE)`), planner
  inputs from fresh probes (own bytes credited, other pools' outstanding
  reservations taken out, reclaimable bytes of idle holdings in, «Авто» limited
  by Q-P1, an optional per-handle `device_cap_bytes` ceiling kept for every
  re-plan), `reserve(ADMISSION, plan peak - own bytes)`;
- `Session.load / transition / run_phase / settle / finish`: every step
  reconciles each unit from its ledger tier to the plan's tier for that step
  (block-wise moves, fresh loads, hooks for juggled components), releases the
  allocator cache at every boundary, updates the run reservation; a memory
  failure at a step is rung 0 (`reserve(IN_RUN, deficit)`: idle small and large
  holdings reclaimed), then `degrade.Ladder`, at most `degrade.MAX_RETRIES`
  retries. `finish` only SHRINKS (parks / drops toward the between-runs state,
  never loads, never moves a unit up); `settle` is the explicit reconcile to
  the between-runs state with loads;
- the large `PoolPolicy`: per idle component a PARK_TO_HOST (class A) and a
  DROP (class C) offer; `execute` parks / drops it block-wise and reports the
  new parts;
- read-only answers for the IPC layer: `profiles` (the two step pickers and
  the selected forecast), `limits` / `set_limits` (partial and atomic:
  `KEEP_LIMIT`), `floors` / `reset_floors`,
  `status`, `placements` (`ComponentPlacement` per component, from the
  ledger), `health`, `unload` / `unload_key`;
- persistence seam `LimitsStore` (+ `JsonLimitsStore`, `MemoryLimitsStore`,
  `limits_store_path`) and the Linux OOM backstop `apply_oom_score_backstop`.

Contracts:
- **No host OOM.** Admission refuses when the planner refuses (shortest
  resource named) or the arbiter cannot reserve; loads and moves run under a
  `HostWatch`; a host shortfall while parking drops the remaining units instead
  (moves), a host shortfall while loading is a ladder failure.
- **Host run device** (`cpu` / `mps`; the residency layer's rule: only
  `cuda*` devices are device-accounted, `footprint.is_device_accounted`):
  planned on the HOST budget (planner module notes: the VRAM axis does not
  apply, the RAM step picks the cell, the device budget is never read).
  Everything the handle holds is reported, reserved and credited on the host
  side (parts, `Need`, own bytes, baseline RSS); loads and restores count the
  device-tier bytes against the `HostWatch`; learned floors are kept under
  `HOST_DEVICE_CLASS`.
- **Host pin** (`LargeModelSpec.host_pin(options)`, e.g. "encoder on the
  CPU"): the planner places the pinned components by the RAM step alone
  (`planner` module notes); the pin is part of the residency key
  (`LargeModelSpec.key`: a pinned request is its own handle) and of the
  learned-floor key (`FloorKey.host_pin`, persisted as `host_pin`; an entry
  stored without it is unpinned).
- **The format is never changed** here; a refusal carries the planner's format
  suggestions. The user's steps are never rewritten: degrades show as
  `Session.steps_effective` / `degraded` / `degrade_reason`, and a learned
  floor (per device class, model, format, host pin; per axis) is recorded only after a
  run that succeeded lower on that axis.
- **Safe points** are step boundaries only (a load, a transition, a whole phase:
  a phase function must be restartable from its inputs). Modules are fetched
  from the `PhaseContext` on every call: a retry or a degrade may rebuild one.
- **Locks.** `_lock` (leaf: specs, handles, limits) is never held across
  probes, the arbiter or mechanics. A handle's `run_lock` serializes sessions of
  one key; it is taken AFTER the lease and released BEFORE it, and the policy's
  `execute` only tries it without blocking, so no reclaim ever waits for a
  running session (an in-use holding is never offered anyway, R1). The owner of
  a large lease defaults to its key: execute never takes a service lock, so the
  arbiter's owner rule needs no shared owner here.
- `acquire` is called BEFORE the service lock (the admission may wait for
  evictions that take other services' locks), unless the service (i) holds no
  small-pool lease and registers no reclaim callback that takes its lock, and
  (ii) opens several handles in one operation: it then acquires every session
  under its lock, in one fixed handle order (FLUX.2: pipeline, then encoder).
  The session's steps run under the service lock either way.

Notes:
- Stdlib only at import; torch is reached lazily through the mechanics, the
  default `TorchTransport` factory and `cost_tables.detect_device_class`.
"""

from __future__ import annotations

import gc
import json
import logging
import sys
import threading
import time
import traceback
from collections.abc import Callable, Collection, Mapping, Sequence
from dataclasses import dataclass, field, replace
from pathlib import Path
from types import MappingProxyType
from typing import Any, Protocol, TypeVar

from ..memory_budget import DeviceBudget, HostWatch, SpillVerdict, detect_sysmem_spill
from ..residency.arbiter import BudgetProbes, MemoryBudgetProbes, ModelUsageLease, Reservation, ResidencyArbiter
from ..residency.footprint import is_device_accounted, normalize_device
from ..residency.holdings import Action, Freed, HoldingsView, Need, Offer, OfferClass, Part, Pool, Requester, Shortfall, Urgency
from ..residency.holdings import Tier as PartTier
from .cost_tables import CostTables, detect_device_class
from .degrade import MAX_RETRIES, FloorEntry, FloorKey, FloorsStore, LearnedFloors, Ladder, Replan, RunFailure, FailureKind, classify_failure, with_requested_steps
from .forecast import Forecast
from .hooks import HookSet, apply_block_residency
from .inventory import ComponentInventory, build_inventory
from .ledger import Holdings, Ledger, UnitRef
from .loader import LoadedComponent, TorchTransport, Transport, initial_tiers, load_component, set_unit_to_meta
from .moves import drop, park, transfer_block_wise
from .planner import (
    REASON_BUDGET_UNKNOWN,
    REASON_DEVICE_SHORT,
    REASON_FORMAT_UNAVAILABLE,
    REASON_HOST_SHORT,
    REASON_STEPS_UNSUPPORTED,
    Budgets,
    Planned,
    PlanRefusal,
    PlanResult,
    Reclaimable,
    axis_rows,
    plan,
    plan_grid,
)
from .spec import ComponentSpec, ExecDevice, FormatInfo, LargeModelSpec, RequestShape
from .tiers import AUTO_STEPS, IDLE_PHASE, VRAM_ORDER, Limits, PhasePlan, RamStep, ResidencyPlan, StepFloor, Steps, Tier, VramStep, more_economical_ram, more_economical_vram, ram_index, vram_index

log = logging.getLogger(__name__)

T = TypeVar("T")

#: `observer(step, event, info)`: `step` is `"load:<component>"`, `"<from>-><to>"`,
#: a phase name or `"finish"`; `event` `"begin"` / `"end"`; the end call adds
#: `seconds` and `failed`.
SessionObserver = Callable[[str, str, Mapping[str, Any]], None]

#: Windows NVIDIA sysmem-fallback detector (`memory_budget.detect_sysmem_spill` shape).
SpillDetector = Callable[[DeviceBudget, DeviceBudget, int], SpillVerdict]

#: `General.<key>` switch of the Linux OOM backstop (default on).
OOM_SCORE_ADJ_CONFIG_KEY = "large_models_oom_score_adj"
#: The value written: the kernel's OOM killer picks the backend first, never the editor.
OOM_SCORE_ADJ_VALUE = 1000

_STORE_VERSION = 1

#: The learned-floor device class of a host run device (`cpu` / `mps`): its
#: floors never mix with a GPU's, whatever cost table it borrows.
HOST_DEVICE_CLASS = "host"


class _Keep:
    """Type of `KEEP_LIMIT`: a `set_limits` cap that was not passed (keep the value in force)."""

    def __repr__(self) -> str:
        return "KEEP_LIMIT"


#: `set_limits` default: keep that cap as it is (`None` clears it instead).
KEEP_LIMIT = _Keep()
_RESOURCE_RU = {"device": "видеопамяти", "host": "оперативной памяти", "slots": "мест для моделей"}


# =====================================================================
#  Errors
# =====================================================================
class LargeModelError(RuntimeError):
    """Base of the large-model manager's user-facing errors (Russian text, the backend's style)."""


class LargeModelRefused(LargeModelError):
    """A run was not admitted: the planner found no fitting plan (`refusal`) or the arbiter could not reserve it (`shortfall`)."""

    def __init__(self, model: str, weight_format: str, *, refusal: PlanRefusal | None = None, shortfall: Shortfall | None = None) -> None:
        self.model = model
        self.weight_format = weight_format
        self.refusal = refusal
        self.shortfall = shortfall
        super().__init__(_refusal_message(model, weight_format, refusal, shortfall))

    @property
    def shortest_resource(self) -> str | None:
        """`"device"`, `"host"`, `"slots"` or `None` (format unavailable / unsupported)."""
        if self.refusal is not None:
            return self.refusal.shortest_resource
        return self.shortfall.resource if self.shortfall is not None else None


class LargeModelRunFailed(LargeModelError):
    """A memory failure the ladder could not recover from (no gentler plan, or the retry cap reached)."""

    def __init__(self, model: str, failure: RunFailure, retries: int) -> None:
        self.model = model
        self.failure = failure
        self.retries = retries
        resource = _RESOURCE_RU["host" if failure.kind is FailureKind.HOST_SHORT else "device"]
        super().__init__(f"Модель «{model}»: не хватило {resource} на шаге «{failure.step}», а более лёгкого плана нет (попыток облегчения: {retries}). Выберите более экономный режим «Видеопамять» или «Оперативная память», более лёгкий формат весов или освободите память.")


def _refusal_message(model: str, weight_format: str, refusal: PlanRefusal | None, shortfall: Shortfall | None) -> str:
    """User-facing text of a refused admission, naming the shortest resource and the suggestions."""
    if refusal is not None:
        if refusal.reason == REASON_FORMAT_UNAVAILABLE:
            head = f"Формат весов «{weight_format}» модели «{model}» недоступен"
        elif refusal.reason == REASON_BUDGET_UNKNOWN:
            head = f"Модель «{model}»: не удалось определить объём свободной {_RESOURCE_RU.get(refusal.shortest_resource or 'device', 'памяти')}, запуск отменён, чтобы не переполнить память"
        elif refusal.reason in (REASON_DEVICE_SHORT, REASON_HOST_SHORT):
            head = f"Модель «{model}» (формат «{weight_format}») не помещается: не хватает {_RESOURCE_RU.get(refusal.shortest_resource or '', 'памяти')} при выбранных режимах «Видеопамять» и «Оперативная память»"
        elif refusal.reason == REASON_STEPS_UNSUPPORTED:
            head = f"Модель «{model}»: выбранные режимы «Видеопамять» и «Оперативная память» не поддерживаются этой моделью"
        else:
            head = f"Модель «{model}»: запуск отклонён ({refusal.reason})"
        formats = [suggestion.payload.get("format", "") for suggestion in refusal.suggestions if suggestion.kind == "format"]
        if formats:
            head += ". Поместится формат: " + ", ".join(f"«{name}»" for name in formats)
        if any(suggestion.kind == "steps" for suggestion in refusal.suggestions):
            head += ". Поместится в более экономном режиме"
        if any(suggestion.kind == "limits" for suggestion in refusal.suggestions):
            head += ". Поместится, если поднять пределы памяти в настройках"
        return head + "."
    if shortfall is not None:
        return f"Модель «{model}»: не удалось зарезервировать память, не хватает {_RESOURCE_RU.get(shortfall.resource, shortfall.resource)}; заняты моделями, которые сейчас используются: {', '.join(shortfall.blocked_by) or 'нет'}."
    return f"Модель «{model}»: запуск отклонён."


# =====================================================================
#  Persistence seam
# =====================================================================
class LimitsStore(FloorsStore, Protocol):
    """Where the user's limits and the learned floors persist (stage 1: `JsonLimitsStore`)."""

    def read_limits(self) -> Limits:
        """The stored limits; an unreadable store answers `Limits()` (and logs)."""
        ...

    def write_limits(self, limits: Limits) -> None:
        """Replace the stored limits; raises `OSError` when the store cannot be written."""
        ...


class MemoryLimitsStore:
    """An in-memory `LimitsStore` (tests, or a backend without a writable data directory)."""

    def __init__(self, limits: Limits | None = None, floors: Sequence[FloorEntry] = ()) -> None:
        self._lock = threading.Lock()
        self._limits = limits if limits is not None else Limits()
        self._floors = list(floors)

    def read_limits(self) -> Limits:
        with self._lock:
            return self._limits

    def write_limits(self, limits: Limits) -> None:
        with self._lock:
            self._limits = limits

    def read_floors(self) -> list[FloorEntry]:
        with self._lock:
            return list(self._floors)

    def write_floors(self, entries: Sequence[FloorEntry]) -> None:
        with self._lock:
            self._floors = list(entries)


def limits_store_path(cache_root: Path) -> Path:
    """`<cache root>/large_models/limits.json`.

    `cache_root` is the backend's writable cache (`ManhwaStudio_AI_Models/.cache`),
    owned by `engines.paddle_onnx.resolve_compiled_cache_root()`; the caller
    (the composition root) resolves it, so `runtime/` does not import upward.
    """
    return cache_root / "large_models" / "limits.json"


def _optional_bytes(raw: Any, name: str, source: Path) -> int | None:
    if raw is None:
        return None
    if isinstance(raw, bool) or not isinstance(raw, int) or raw < 0:
        log.warning("large models: %s: %s must be a non-negative integer or null, got %r; ignored", source, name, raw)
        return None
    return raw


def _stored_host_pin(raw: Any) -> tuple[str, ...]:
    """The `host_pin` of a stored floor entry: absent / `null` = none (entries written before the pin existed).

    # Raises
    `TypeError` for anything but a list of strings (the caller skips the entry).
    """
    if raw is None:
        return ()
    if not isinstance(raw, list) or not all(isinstance(name, str) for name in raw):
        raise TypeError(f"host_pin должен быть списком строк, получено {raw!r}")
    return tuple(sorted(raw))


class JsonLimitsStore:
    """The stage-1 `LimitsStore`: one small JSON document (`{"version", "limits", "floors"}`), written atomically.

    The file is a backend-private cache, not an owned user document: a malformed
    file is moved aside (`<name>.corrupt`) and logged, and the store starts empty.
    """

    def __init__(self, path: Path) -> None:
        self._path = Path(path)
        self._lock = threading.Lock()

    @property
    def path(self) -> Path:
        """The JSON file."""
        return self._path

    def read_limits(self) -> Limits:
        raw = self._read().get("limits")
        if not isinstance(raw, dict):
            return Limits()
        if raw.get("keep_policy") is not None:
            # The between-runs policy is decided by the two step axes now; a
            # value stored by an older build is ignored (this file is a cache).
            log.info("large models: %s: stored keep_policy %r is no longer used (the VRAM / RAM steps decide what stays between runs); ignored", self._path, raw.get("keep_policy"))
        return Limits(vram_cap=_optional_bytes(raw.get("vram_cap_bytes"), "vram_cap_bytes", self._path), ram_cap=_optional_bytes(raw.get("ram_cap_bytes"), "ram_cap_bytes", self._path))

    def write_limits(self, limits: Limits) -> None:
        with self._lock:
            document = self._read_locked()
            document["limits"] = {"vram_cap_bytes": limits.vram_cap, "ram_cap_bytes": limits.ram_cap}
            self._write_locked(document)

    def read_floors(self) -> list[FloorEntry]:
        raw = self._read().get("floors")
        if not isinstance(raw, list):
            return []
        entries: list[FloorEntry] = []
        legacy = 0
        for index, item in enumerate(raw):
            if isinstance(item, dict) and "profile" in item and "vram" not in item and "ram" not in item:
                legacy += 1
                continue
            try:
                key = FloorKey(device_class=str(item["device_class"]), model=str(item["model"]), format=str(item["format"]), host_pin=_stored_host_pin(item.get("host_pin")))
                vram = VramStep(item["vram"]) if item.get("vram") is not None else None
                ram = RamStep(item["ram"]) if item.get("ram") is not None else None
                entries.append(FloorEntry(key=key, vram=vram, ram=ram, reason=str(item.get("reason", "")), step=str(item.get("step", "")), recorded_at=float(item.get("recorded_at", 0.0))))
            except (KeyError, TypeError, ValueError) as exc:
                log.warning("large models: %s: floor entry %d unreadable (%s); ignored", self._path, index, exc)
        if legacy:
            log.info("large models: %s: %d learned floor(s) of the single profile scale skipped (floors are per VRAM / RAM step now; they are learned again)", self._path, legacy)
        return entries

    def write_floors(self, entries: Sequence[FloorEntry]) -> None:
        with self._lock:
            document = self._read_locked()
            document["floors"] = [
                {"device_class": entry.key.device_class, "model": entry.key.model, "format": entry.key.format, "host_pin": list(entry.key.host_pin), "vram": entry.vram.value if entry.vram is not None else None, "ram": entry.ram.value if entry.ram is not None else None, "reason": entry.reason, "step": entry.step, "recorded_at": entry.recorded_at}
                for entry in entries
            ]
            self._write_locked(document)

    def _read(self) -> dict[str, Any]:
        with self._lock:
            return self._read_locked()

    def _read_locked(self) -> dict[str, Any]:
        try:
            text = self._path.read_text(encoding="utf-8")
        except FileNotFoundError:
            return {"version": _STORE_VERSION}
        except OSError as exc:
            log.warning("large models: limits store %s unreadable (%s); using defaults", self._path, exc)
            return {"version": _STORE_VERSION}
        problem: str
        try:
            document = json.loads(text)
        except json.JSONDecodeError as exc:
            problem = f"not JSON: {exc}"
        else:
            if isinstance(document, dict):
                return document
            problem = f"root is {type(document).__name__}, not an object"
        aside = self._path.with_name(self._path.name + ".corrupt")
        try:
            self._path.replace(aside)
            log.warning("large models: limits store %s is malformed (%s); moved to %s, starting empty", self._path, problem, aside)
        except OSError as exc:
            log.warning("large models: limits store %s is malformed (%s) and could not be moved aside (%s); starting empty", self._path, problem, exc)
        return {"version": _STORE_VERSION}

    def _write_locked(self, document: dict[str, Any]) -> None:
        document["version"] = _STORE_VERSION
        self._path.parent.mkdir(parents=True, exist_ok=True)
        staging = self._path.with_name(self._path.name + ".tmp")
        staging.write_text(json.dumps(document, ensure_ascii=False, indent=1), encoding="utf-8")
        staging.replace(self._path)


# =====================================================================
#  Linux OOM backstop (user decision Q6)
# =====================================================================
def apply_oom_score_backstop(general: Mapping[str, Any] | None, *, path: Path = Path("/proc/self/oom_score_adj"), platform: str = sys.platform) -> bool:
    """Make the kernel's OOM killer pick this backend first (`oom_score_adj = 1000`), on Linux only.

    `general` is the `General` section of `user_config` (`None` = defaults);
    the switch `General.large_models_oom_score_adj` defaults to on. Returns
    whether the value was written. Never raises: a failed write is logged at
    WARNING and startup continues.
    """
    if not platform.startswith("linux"):
        return False
    enabled = True
    if general is not None:
        raw = general.get(OOM_SCORE_ADJ_CONFIG_KEY, True)
        if isinstance(raw, bool):
            enabled = raw
        else:
            log.warning("large models: General.%s must be a boolean, got %r; using the default (on)", OOM_SCORE_ADJ_CONFIG_KEY, raw)
    if not enabled:
        log.info("large models: OOM backstop disabled by General.%s", OOM_SCORE_ADJ_CONFIG_KEY)
        return False
    try:
        path.write_text(f"{OOM_SCORE_ADJ_VALUE}\n", encoding="ascii")
    except OSError as exc:
        log.warning("large models: could not write %s = %d (%s); the kernel may pick another process on a host OOM", path, OOM_SCORE_ADJ_VALUE, exc)
        return False
    log.info("large models: %s = %d (the backend is the OOM killer's first choice)", path, OOM_SCORE_ADJ_VALUE)
    return True


# =====================================================================
#  Reports
# =====================================================================
@dataclass(frozen=True)
class FormatRow:
    """One weight format of a model in a `profiles` answer; `recommended` = the steps «Авто» resolves to in it (`None` = nothing fits)."""

    format: str
    info: FormatInfo
    recommended: Steps | None


@dataclass(frozen=True)
class ProfilesReport:
    """The `large_models.profiles` answer for one model, format, request shape and requested steps (wire form: `to_dict()`).

    `selected` is what the requested steps resolve to (Q-P1 and the learned
    floor applied, exactly as an admission would plan); `vram_rows` are the
    VRAM steps planned with the selected RAM step, `ram_rows` the RAM steps
    with the selected VRAM step (cells of one step grid: monotone on their own
    axis). `vram_recommended` / `ram_recommended` are what «Авто» on that axis
    resolves to with the other axis fixed; `recommended` what «Авто» on both
    resolves to. `vram_applicable` is `False` on a host run device (`cpu` /
    `mps`): `vram_rows` is then empty and the RAM rows alone choose.
    """

    model: str
    format: str
    steps_requested: Steps
    selected: PlanResult
    vram_rows: Mapping[VramStep, PlanResult]
    ram_rows: Mapping[RamStep, PlanResult]
    vram_recommended: VramStep | None
    ram_recommended: RamStep | None
    recommended: Steps | None
    floor: FloorEntry | None
    limits: Limits
    host: Mapping[str, int | None]
    device: Mapping[str, int | None]
    formats: tuple[FormatRow, ...]
    vram_applicable: bool = True

    def to_dict(self) -> dict[str, Any]:
        """The wire shape (`selected`, `vram`, `ram`, `learned_floor`, `limits`, `formats`, `suggestions`); forecasts are the full ones (every activation).

        `vram.applicable` / `ram.applicable` say whether that picker applies
        (`vram.applicable` is `false` on a host run device: no steps, no
        recommendation, no floor).
        """
        suggestions: list[dict[str, Any]] = []

        def collect(result: PlanResult) -> None:
            if isinstance(result, PlanRefusal):
                for suggestion in result.suggestions:
                    item = {"kind": suggestion.kind, "text_key": suggestion.text_key, "payload": dict(suggestion.payload)}
                    if item not in suggestions:
                        suggestions.append(item)

        collect(self.selected)
        floor_vram = self.floor.vram if self.floor is not None else None
        floor_ram = self.floor.ram if self.floor is not None else None
        vram_steps = []
        for step, result in self.vram_rows.items():
            collect(result)
            shown = _display_forecast(result)
            vram_steps.append({"id": step.value, "fits": isinstance(result, Planned), "same_as": result.same_as_vram.value if isinstance(result, Planned) and result.same_as_vram is not None else None, "below_floor": floor_vram is not None and vram_index(step) < vram_index(floor_vram), "device_peak": shown.device_peak if shown is not None else None, "total_s": shown.time.total_s if shown is not None else None})
        ram_steps = []
        for ram_step, result in self.ram_rows.items():
            collect(result)
            shown = _display_forecast(result)
            ram_steps.append({"id": ram_step.value, "fits": isinstance(result, Planned), "same_as": result.same_as_ram.value if isinstance(result, Planned) and result.same_as_ram is not None else None, "below_floor": floor_ram is not None and ram_index(ram_step) < ram_index(floor_ram), "host_peak": shown.host_peak if shown is not None else None, "total_s": shown.time.total_s if shown is not None else None})
        return {
            "model": self.model,
            "format": self.format,
            "selected": _selected_dict(self.steps_requested, self.selected),
            "vram": {"applicable": self.vram_applicable, "steps": vram_steps, "recommended": self.vram_recommended.value if self.vram_recommended is not None and self.vram_applicable else None, "floor": floor_vram.value if floor_vram is not None and self.vram_applicable else None},
            "ram": {"applicable": True, "steps": ram_steps, "recommended": self.ram_recommended.value if self.ram_recommended is not None else None, "floor": floor_ram.value if floor_ram is not None else None},
            "learned_floor": _floor_dict(self.floor) if self.floor is not None else None,
            "limits": {**_limits_dict(self.limits), **{f"ram_{key}": value for key, value in self.host.items()}, **{f"vram_{key}": value for key, value in self.device.items()}},
            "formats": [{"id": row.format, "available": row.info.available, "reason": row.info.reason, "dit_bytes": row.info.dit_bytes, "compute_path": row.info.compute_path, "quality_rank": row.info.quality_rank, "fits_best": _steps_dict(row.recommended) if row.recommended is not None else None} for row in self.formats],
            "suggestions": suggestions,
        }


def _display_forecast(result: PlanResult) -> Forecast | None:
    """The forecast a picker row or the selected line shows: the full one of a plan, the closest attempt's of a refusal."""
    return result.full_forecast if isinstance(result, Planned) else result.forecast


def _selected_dict(requested: Steps, result: PlanResult) -> dict[str, Any]:
    shown = _display_forecast(result)
    planned = result if isinstance(result, Planned) else None
    refusal = result if isinstance(result, PlanRefusal) else None
    return {
        "vram_requested": requested.vram.value,
        "ram_requested": requested.ram.value,
        "vram": planned.plan.steps.vram.value if planned is not None else None,
        "ram": planned.plan.steps.ram.value if planned is not None else None,
        "fits": planned is not None,
        "needs_reclaim": {"device_bytes": planned.fit.device_bytes, "host_bytes": planned.fit.host_bytes} if planned is not None else None,
        "reason": refusal.reason if refusal is not None else None,
        "shortest_resource": refusal.shortest_resource if refusal is not None else None,
        "forecast": _forecast_dict(shown) if shown is not None else None,
        "plan_summary": [_plan_line_dict(line) for line in shown.plan_summary] if shown is not None else [],
    }


def _forecast_dict(result: Forecast) -> dict[str, Any]:
    time_ = result.time
    return {"device_peak": result.device_peak, "host_peak": result.host_peak, "peak_phase": result.peak_phase, "time": {"load_cold_s": time_.load_cold_s, "load_warm_s": time_.load_warm_s, "per_step_s": time_.per_step_s, "total_s": time_.total_s}, "calibrated": result.calibrated, "device_calibrated": result.device_calibrated}


def _plan_line_dict(line: Any) -> dict[str, Any]:
    return {"component": line.component, "tier": line.tier.value, "exec_device": line.exec_device.value, "blocks_device": line.blocks_device, "blocks_host": line.blocks_host, "blocks_disk": line.blocks_disk}


def _steps_dict(steps: Steps) -> dict[str, str]:
    return {"vram": steps.vram.value, "ram": steps.ram.value}


def _limits_dict(limits: Limits) -> dict[str, Any]:
    return {"vram_cap": limits.vram_cap, "ram_cap": limits.ram_cap}


def _floor_dict(entry: FloorEntry) -> dict[str, Any]:
    return {"model": entry.key.model, "format": entry.key.format, "device_class": entry.key.device_class, "vram": entry.vram.value if entry.vram is not None else None, "ram": entry.ram.value if entry.ram is not None else None, "reason": entry.reason, "step": entry.step, "recorded_at": entry.recorded_at}


@dataclass(frozen=True)
class ComponentPlacement:
    """Where one component of a handle is right now, from the ledger (no probe).

    `tier`: `DEVICE` when every unit is on the device; `HOST` when any unit is
    parked in RAM (the rest on the device); `DISK` when some units are on the
    device and the rest are read from the file per pass; `None` when nothing of
    the component is held. `exec_device` is where it last ran (`None` = never
    loaded). Block counts cover block units only; the bytes cover every unit.
    """

    component: str
    tier: Tier | None
    exec_device: ExecDevice | None
    blocks_device: int
    blocks_host: int
    blocks_disk: int
    device_bytes: int
    host_bytes: int

    def to_dict(self) -> dict[str, Any]:
        """The wire shape (`status()` / health `components` entries)."""
        return {"tier": self.tier.value if self.tier is not None else None, "exec_device": self.exec_device.value if self.exec_device is not None else None, "blocks_device": self.blocks_device, "blocks_host": self.blocks_host, "blocks_disk": self.blocks_disk, "device_bytes": self.device_bytes, "host_bytes": self.host_bytes}


# =====================================================================
#  Handles
# =====================================================================
@dataclass(eq=False)
class _Handle:
    """One residency key: the loaded components of one model in one format on one device."""

    model: str
    key: str
    spec: LargeModelSpec
    device: str
    weight_format: str
    transport: Transport
    ledger: Ledger
    floor_key: FloorKey
    components: dict[str, LoadedComponent] = field(default_factory=dict)
    exec_devices: dict[str, ExecDevice] = field(default_factory=dict)
    run_lock: threading.Lock = field(default_factory=threading.Lock)
    # The run device's memory is the host's (`cpu` / `mps`): see the module contract.
    host_device: bool = False
    in_session: bool = False
    steps_effective: Steps | None = None
    degraded: bool = False
    degrade_reason: str | None = None


@dataclass(frozen=True)
class _Inputs:
    """Planner inputs of one planning call: the budgets (reclaimable not yet applied) and what other pools could free."""

    budgets: Budgets
    reclaimable: Reclaimable


class _LargePoolPolicy:
    """The large pool's `PoolPolicy` (offers built purely from the arbiter's view; execution on the manager)."""

    def __init__(self, manager: LargeModelManager) -> None:
        self._manager = manager

    @property
    def pool(self) -> Pool:
        return Pool.LARGE

    def slot_capacity(self) -> int | None:
        return None

    def offers(self, view: HoldingsView, need: Need, requester: Requester | None) -> Sequence[Offer]:
        """Per idle component: park its device bytes (class A, LRU order) and drop it (class C, reload cost per byte)."""
        found: list[Offer] = []
        for holding in view.of_pool(Pool.LARGE):
            if not holding.idle:
                continue
            for part in holding.parts:
                if part.device_bytes > 0:
                    # A park keeps the weights: its "reload" is a restore at
                    # copy speed, equal per byte for every park, so class A
                    # orders by LRU only.
                    found.append(Offer(pool=Pool.LARGE, key=holding.key, part=part.name, action=Action.PARK_TO_HOST, device=holding.footprint.device, frees_device=part.device_bytes, frees_host=-part.device_bytes, reload_cost_s=0.0, cls=OfferClass.A, last_used_at=holding.last_used_at))
                if part.device_bytes > 0 or part.host_bytes > 0:
                    found.append(Offer(pool=Pool.LARGE, key=holding.key, part=part.name, action=Action.DROP, device=holding.footprint.device, frees_device=part.device_bytes, frees_host=part.host_bytes, reload_cost_s=part.reload_cost_s, cls=OfferClass.C, last_used_at=holding.last_used_at))
        return found

    def execute(self, offer: Offer) -> Freed:
        return self._manager._execute_offer(offer)


# =====================================================================
#  Session
# =====================================================================
class PhaseContext:
    """What a phase function sees: the modules (fetch them here on every call), the effective options and plan."""

    def __init__(self, session: Session, phase: str, attempt: int) -> None:
        self._session = session
        self.phase = phase
        self.attempt = attempt

    @property
    def options(self) -> Any:
        """The request options with every applied fallback (e.g. tiled decode) switched on."""
        return self._session.options

    @property
    def plan(self) -> ResidencyPlan:
        """The plan in force for this attempt."""
        return self._session.plan

    def module(self, component: str) -> Any:
        """The live module of `component`; `KeyError` when it is not loaded in this phase."""
        return self._session.module(component)

    def exec_device(self, component: str) -> Any:
        """Where `component`'s forward runs in this phase (the run device, or `"cpu"`)."""
        plan_ = self._session.plan.phase(self.phase).components[component]
        return self._session.device_for(plan_.exec_device)


class Session:
    """One admitted run of a large model (see the module contracts). Drive it from ONE thread, under the service lock.

    Use as a context manager (`finish()` on exit), or call `finish()` yourself.
    """

    def __init__(self, manager: LargeModelManager, handle: _Handle, lease: ModelUsageLease, *, owner: str, inventories: Mapping[str, ComponentInventory], all_inventories: Mapping[str, Mapping[str, ComponentInventory]], formats: Mapping[str, FormatInfo], shape: RequestShape, tables: CostTables, planned: Planned, options: Any, reservation: Reservation, baseline_rss: int, observer: SessionObserver | None, device_cap_bytes: int | None = None) -> None:
        self._manager = manager
        self._handle = handle
        self._lease = lease
        self._owner = owner
        self._inventories = inventories
        self._all_inventories = all_inventories
        self._formats = formats
        self._shape = shape
        self._tables = tables
        self._reservation = reservation
        self._baseline_rss = baseline_rss
        self._observer = observer
        # The caller's per-handle device ceiling, kept for every re-plan.
        self._device_cap = device_cap_bytes
        self._requested = planned.plan.steps_requested
        self._watch = HostWatch(manager._probes.host, floor=0)
        self._ladder = Ladder(handle.spec, start=planned, options=options, replan=self._replan, max_retries=manager._max_retries)
        self._current: str | None = None
        self._finished = False
        self._extra_device = 0
        self._extra_host = 0
        self._progress: Any = None
        self.dropped_instead: list[UnitRef] = []
        self.hints: list[str] = []

    # ------------------------------------------------------------- facts
    @property
    def model(self) -> str:
        return self._handle.model

    @property
    def key(self) -> str:
        return self._handle.key

    @property
    def device(self) -> str:
        return self._handle.device

    @property
    def weight_format(self) -> str:
        return self._handle.weight_format

    @property
    def plan(self) -> ResidencyPlan:
        """The plan in force (changes when a rung degrades it)."""
        return self._ladder.plan

    @property
    def forecast(self) -> Forecast:
        """The forecast of the plan in force."""
        return self._ladder.current.forecast

    @property
    def options(self) -> Any:
        """The request options with every applied fallback."""
        return self._ladder.options

    @property
    def steps_requested(self) -> Steps:
        """What the caller asked for (may hold «Авто» on either axis)."""
        return self._requested

    @property
    def steps_effective(self) -> Steps:
        """The concrete steps of the plan in force (Auto resolved, the floor applied, degrades included)."""
        return self._ladder.plan.steps

    @property
    def degraded(self) -> bool:
        """Whether a rung other than a reclaim changed this run."""
        return self._ladder.degraded

    @property
    def degrade_reason(self) -> str | None:
        return self._ladder.reason

    @property
    def retries(self) -> int:
        return self._ladder.retries

    @property
    def history(self) -> list[Any]:
        """The ladder's rungs in order (`degrade.Rung`)."""
        return list(self._ladder.history)

    @property
    def current_phase(self) -> str | None:
        """The phase entered last (`None` before the first, `"idle"` after `finish`)."""
        return self._current

    def module(self, component: str) -> Any:
        """The live module of `component` (fetch it per use: a retry may rebuild it); `KeyError` when not loaded."""
        return self._handle.components[component].module

    def device_for(self, exec_device: ExecDevice) -> Any:
        """The torch device of an `ExecDevice` in this session."""
        return _transport_device(self._handle.transport, self._handle.device) if exec_device is ExecDevice.RUN_DEVICE else "cpu"

    def placements(self) -> Mapping[str, ComponentPlacement]:
        """Where every component of this session's handle is right now (`LargeModelManager.placements`)."""
        return self._manager.placements(self.key)

    # ------------------------------------------------------------- steps
    def load(self, *components: str, progress: Any = None) -> None:
        """Bring `components` (all when none named) to their tiers in the current state (the first phase before any).

        A safe point: a memory failure walks the ladder. Components that the
        current state holds nowhere are not loaded (a later step loads them).
        """
        self._require_open()
        names = components or tuple(component.name for component in self._handle.spec.components)
        unknown = [name for name in names if name not in {component.name for component in self._handle.spec.components}]
        if unknown:
            raise ValueError(f"Модель «{self.model}»: неизвестные компоненты {unknown}")
        for name in names:
            step = f"load:{name}"
            self._progress = progress
            try:
                self._observed(step, lambda name=name: self._guarded(step, None, lambda: self._reconcile(self._state(), only=(name,), needed=(name,))))
            finally:
                self._progress = None
        self._update_reservation()

    def transition(self, from_phase: str | None, to_phase: str) -> None:
        """Move every component from `from_phase`'s placement (`None` = before the first phase) to `to_phase`'s, unit by unit.

        A safe point. `ValueError` when `from_phase` is not the current phase or
        `to_phase` is not a phase of the model.
        """
        self._require_open()
        if from_phase != self._current:
            raise ValueError(f"Модель «{self.model}»: переход из «{from_phase}», а текущая фаза «{self._current}»")
        self.plan.phase(to_phase)
        step = f"{from_phase or IDLE_PHASE}->{to_phase}"
        self._observed(step, lambda: self._guarded(step, to_phase, lambda: self._reconcile(self.plan.phase(to_phase), needed=self._uses(to_phase))))
        self._current = to_phase
        self._update_reservation()

    def run_phase(self, name: str, fn: Callable[[PhaseContext], T]) -> T:
        """Run phase `name` (entering it from the current phase first), retrying it along the ladder on a memory failure.

        `fn(ctx)` must be restartable: after a failure it is called again from
        scratch, possibly on a gentler plan, with rebuilt modules or changed
        options (fetch both from `ctx`). Non-memory exceptions propagate
        unchanged; `LargeModelRunFailed` when the ladder gives up.
        """
        self._require_open()
        if self._current != name:
            self.transition(self._current, name)
        attempt = 0

        def run() -> T:
            nonlocal attempt
            attempt += 1
            self._reconcile(self.plan.phase(name), needed=self._uses(name))
            return self._run_hooked(name, fn, attempt)

        before = self._manager._spill_probe(self.device)
        result = self._observed(name, lambda: self._guarded(name, name, run))
        self._after_phase_spill_check(name, before)
        self._update_reservation()
        return result

    def settle(self) -> None:
        """Bring every component to the between-runs state of the plan in force, LOADING what that state holds.

        The explicit counterpart of `finish` (which only shrinks): e.g. a
        "keep it on the card" action loads the model now. A safe point (a
        memory failure walks the ladder). Afterwards the session stands in the
        between-runs state; a later phase is entered from there.
        """
        self._require_open()
        names = tuple(component.name for component in self._handle.spec.components)
        self._observed("settle", lambda: self._guarded("settle", None, lambda: self._reconcile(self.plan.idle, needed=names)))
        self._current = IDLE_PHASE
        self._update_reservation()

    def finish(self, error: BaseException | None = None) -> None:
        """End the run: components SHRUNK toward the between-runs state, reservation released, run lock and lease returned.

        Idempotent. `finish` never loads a component and never moves a unit
        up (toward the device): it parks or drops toward `plan.idle`; a unit
        whose between-runs tier is more resident than where it is now stays
        where it is (holding less is safe), and an absent component stays
        absent (`settle` is the explicit way to load the between-runs state).
        After a MEMORY failure (`error` is `LargeModelRunFailed` or any failure
        `degrade.classify_failure` recognizes), or when the idle move itself
        fails, every component is dropped instead: the warm state could need
        exactly the memory that just ran out. After any other error the shrunk
        state is kept. A learned floor is recorded per axis only after a
        successful run that ended lower on that axis.
        """
        if self._finished:
            return
        self._finished = True
        handle = self._handle
        try:
            started = time.monotonic()
            self._emit("finish", "begin", self._steps_info())
            memory_failure = error is not None and (isinstance(error, LargeModelRunFailed) or classify_failure(error, step="finish") is not None)
            if memory_failure:
                log.warning("large models %s: run ended by a memory failure (%s); every component is dropped", self.key, type(error).__name__)
                self._manager._drop_all(handle)
            else:
                try:
                    self._reconcile(self.plan.idle, needed=(), allow_fresh=False, allow_incoming=False)
                except Exception as exc:
                    log.warning("large models %s: moving to the between-runs state failed (%s); every component is dropped", self.key, exc, exc_info=True)
                    self._manager._drop_all(handle)
            if error is not None:
                log.info("large models %s: run failed (%s)", self.key, type(error).__name__)
            elif self._ladder.vram_lowered or self._ladder.ram_lowered:
                failure = next((rung.failure for rung in reversed(self._ladder.history) if rung.planned is not None), None)
                steps = self.plan.steps
                self._manager._floors.record(FloorEntry(handle.floor_key, steps.vram if self._ladder.vram_lowered else None, steps.ram if self._ladder.ram_lowered else None, failure.kind.value if failure else "", failure.step if failure else "", self._manager._clock()))
            handle.steps_effective = self.plan.steps
            handle.degraded = self.degraded
            handle.degrade_reason = self.degrade_reason
            self._current = IDLE_PHASE
            self._emit("finish", "end", {"seconds": time.monotonic() - started, "failed": error is not None})
        finally:
            self._manager._end_session(handle, self._lease, self._reservation)

    def __enter__(self) -> Session:
        return self

    def __exit__(self, exc_type: Any, exc: BaseException | None, tb: Any) -> None:
        self.finish(exc)

    # ------------------------------------------------------------- internals
    def _require_open(self) -> None:
        if self._finished:
            raise RuntimeError(f"Сеанс модели «{self.model}» уже завершён")

    def _state(self) -> PhasePlan:
        """The current state: the entered phase, or the first phase before any."""
        if self._current is None or self._current == IDLE_PHASE:
            return self.plan.phases[0]
        return self.plan.phase(self._current)

    def _steps_info(self) -> dict[str, Any]:
        """The observer's step info: the effective VRAM and RAM steps."""
        steps = self.plan.steps
        return {"vram_step": steps.vram.value, "ram_step": steps.ram.value}

    def _emit(self, step: str, event: str, info: Mapping[str, Any]) -> None:
        if self._observer is not None:
            self._observer(step, event, info)

    def _observed(self, step: str, action: Callable[[], T]) -> T:
        self._emit(step, "begin", self._steps_info())
        started = time.monotonic()
        failed = True
        try:
            result = action()
            failed = False
            return result
        finally:
            self._emit(step, "end", {"seconds": time.monotonic() - started, "failed": failed, **self._steps_info()})

    def _guarded(self, step: str, phase: str | None, action: Callable[[], T]) -> T:
        """Run `action`, recovering from memory failures (rung 0, then the ladder) until it succeeds or the ladder gives up."""
        while True:
            try:
                return action()
            except Exception as exc:
                failure = classify_failure(exc, step=step, phase=phase)
                if failure is None:
                    raise
                log.warning("large models %s: %s at %s (deficit ~%d B, plan %s)", self.key, failure.kind.value, step, failure.deficit_bytes, self.plan.steps.label, exc_info=True)
                # Tensors of the failed attempt live in the traceback's frames
                # until the exception is dropped; free them before re-planning.
                traceback.clear_frames(exc.__traceback__)
                gc.collect()
                self._handle.transport.release_cache()
                if not self._recover(failure):
                    raise LargeModelRunFailed(self.model, failure, self._ladder.retries) from exc

    def _recover(self, failure: RunFailure) -> bool:
        """Rung 0, else the next rung; `False` when the run must give up."""
        if not self._ladder.take_retry(failure):
            return False
        if self._reclaim_in_run(failure):
            self._ladder.note_reclaim(failure, failure.deficit_bytes)
            return True
        rung = self._ladder.degrade(failure)
        if rung is None:
            return False
        self._update_reservation()
        return True

    def _reclaim_in_run(self, failure: RunFailure) -> bool:
        """Rung 0: `reserve(IN_RUN, deficit)`; `True` only when other holdings were actually reclaimed."""
        # On a host run device every shortfall is a host one.
        device_side = failure.kind.device_side and not self._handle.host_device
        need = Need(device=self.device, device_bytes=failure.deficit_bytes) if device_side else Need(host_bytes=failure.deficit_bytes)
        before = self._manager._others_held(self.key, self.device if device_side else None)
        requester = Requester(pool=Pool.LARGE, key=self.key, owner=self._owner, urgency=Urgency.IN_RUN, holds_lease=True)
        result = self._manager._arbiter.reserve(requester, need)
        if isinstance(result, Shortfall):
            log.info("large models %s: rung 0 found nothing to reclaim (%s short by %d B, busy: %s)", self.key, result.resource, result.deficit, ", ".join(result.blocked_by) or "-")
            return False
        self._manager._arbiter.release_reservation(result)
        after = self._manager._others_held(self.key, self.device if device_side else None)
        if after >= before:
            log.info("large models %s: rung 0: the deficit already fits, nothing reclaimed; degrading instead", self.key)
            return False
        if device_side:
            self._extra_device += failure.deficit_bytes
        else:
            self._extra_host += failure.deficit_bytes
        self._update_reservation()
        log.info("large models %s: rung 0 reclaimed %d B of other holdings; retrying %s on the same plan", self.key, before - after, failure.step)
        return True

    def _replan(self, request: Replan) -> PlanResult:
        """The ladder's planning callback: fresh budgets, no reclaim (rung 0 already ran), the deficit out of the device room."""
        inputs = self._manager._inputs(self._handle, Requester(pool=Pool.LARGE, key=self.key, owner=self._owner, urgency=Urgency.IN_RUN, holds_lease=True), own_reservation=self._reservation, with_reclaim=False)
        budgets = replace(inputs.budgets, observed_deficit_bytes=request.extra_device_bytes, device_cap_bytes=self._device_cap)
        result = plan(self._handle.spec, self._all_inventories, format=self.weight_format, formats=self._formats, steps=request.steps, limits=self._manager.limits(), budgets=budgets, shape=self._shape, tables=self._tables, options=request.options)
        if isinstance(result, Planned):
            return replace(result, plan=with_requested_steps(result.plan, self._requested))
        return result

    def _run_hooked(self, name: str, fn: Callable[[PhaseContext], T], attempt: int) -> T:
        """`fn` with residency hooks on every juggled component the phase uses; hooks removed and cache released after."""
        phase_spec = next(phase for phase in self._handle.spec.phases if phase.name == name)
        state = self.plan.phase(name)
        hooksets: list[HookSet] = []
        try:
            for component_name in sorted(phase_spec.uses):
                component_plan = state.components[component_name]
                loaded = self._handle.components.get(component_name)
                if loaded is None or component_plan.resident_blocks is None or component_plan.exec_device is not ExecDevice.RUN_DEVICE:
                    continue
                hooksets.append(apply_block_residency(loaded, self._handle.transport, prefetch_depth=self._tables.device.prefetch_depth, plan=component_plan, watch=self._watch))
            return fn(PhaseContext(self, name, attempt))
        finally:
            for hookset in hooksets:
                hookset.remove()
            self._handle.transport.release_cache()

    def _after_phase_spill_check(self, name: str, before: DeviceBudget | None) -> None:
        """Windows NVIDIA sysmem fallback: a spilled phase degrades the REMAINING phases (the result is kept)."""
        detector = self._manager._spill_detector
        if detector is None or before is None:
            return
        after = self._manager._spill_probe(self.device)
        if after is None:
            return
        verdict = detector(before, after, self.forecast.phases[name].device_peak)
        if not verdict.spilled:
            return
        if "nvidia_sysmem_fallback" not in self.hints:
            self.hints.append("nvidia_sysmem_fallback")
        failure = RunFailure(FailureKind.SYSMEM_SPILL, name, name, max(1, verdict.overshoot), f"overshoot {verdict.overshoot} B, over plan {verdict.over_plan} B")
        log.warning("large models %s: phase %s spilled into system RAM (overshoot %d B); the remaining phases are degraded", self.key, name, verdict.overshoot)
        if self._ladder.take_retry(failure) and not self._reclaim_in_run(failure):
            if self._ladder.degrade(failure) is not None:
                self._update_reservation()

    def _remaining_peaks(self) -> tuple[int, int]:
        """(device peak, host growth) over the steps from the current one to the end of the run."""
        forecast_ = self.forecast
        names = [phase.phase for phase in self.plan.phases]
        steps: list[tuple[str, str]] = [("t", f"{IDLE_PHASE}->{names[0]}")]
        for index, name in enumerate(names):
            steps.append(("p", name))
            steps.append(("t", f"{name}->{names[index + 1] if index + 1 < len(names) else IDLE_PHASE}"))
        start = 0
        if self._current is not None and self._current != IDLE_PHASE:
            start = steps.index(("p", self._current))
        device_peak = 0
        host_peak = 0
        for kind, name in steps[start:]:
            entry = forecast_.phases[name] if kind == "p" else forecast_.transitions[name]
            device_peak = max(device_peak, entry.device_peak)
            host_peak = max(host_peak, entry.host_peak)
        return device_peak, max(0, host_peak - self._baseline_rss)

    def _update_reservation(self) -> None:
        """The run reservation = the remaining peak minus what this handle already holds (+ rung-0 extras)."""
        need = self._manager._run_need(self._handle, self._remaining_peaks(), extra_device=self._extra_device, extra_host=self._extra_host)
        self._manager._arbiter.update_reservation(self._reservation, need)

    # ------------------------------------------------------------ reconcile
    def _uses(self, phase: str) -> frozenset[str]:
        """The components phase `phase` needs."""
        return next(spec.uses for spec in self._handle.spec.phases if spec.name == phase)

    def _reconcile(self, state: PhasePlan, *, needed: Collection[str], only: Collection[str] | None = None, allow_fresh: bool = True, allow_incoming: bool = True) -> None:
        """Move every unit (of `only`) from its ledger tier to its tier in `state`; load absent components; release the cache.

        A unit planned in RAM that holds no bytes (dropped by a host shortfall)
        stays where it is unless its component is `needed` by the step: holding
        less than planned is always safe, and re-reading it into RAM only to
        rest there would allocate host memory for nothing. A needed component
        is then re-read whole. `allow_fresh=False` loads nothing (an absent
        component stays absent; a stale one is dropped, not re-read);
        `allow_incoming=False` moves no unit toward the device (it stays where
        it is): together they make a shrink-only reconcile (`finish`).
        """
        handle = self._handle
        transport = handle.transport
        drops: dict[Tier, list[tuple[LoadedComponent, UnitRef]]] = {Tier.DROPPED: [], Tier.DISK: []}
        park_out: list[tuple[LoadedComponent, UnitRef]] = []
        incoming: list[tuple[LoadedComponent, UnitRef]] = []
        fresh: list[tuple[ComponentSpec, Any]] = []
        for component in handle.spec.components:
            name = component.name
            if only is not None and name not in only:
                continue
            component_plan = state.components[name]
            inventory = self._inventories[name]
            targets = initial_tiers(component_plan, inventory)
            holds = any(tier in (Tier.DEVICE, Tier.HOST) for tier in targets.values())
            loaded = handle.components.get(name)
            if loaded is not None:
                stale = loaded.inventory is not inventory or (holds and handle.exec_devices.get(name) is not component_plan.exec_device)
                # A unit that must be in RAM but holds no bytes cannot be
                # brought there by a move: the component is re-read whole.
                unreachable = holds and name in needed and any(targets[ref] is Tier.HOST and loaded.ledger.tier(ref) in (Tier.DISK, Tier.DROPPED) for ref in loaded.units)
                if stale or unreachable:
                    self._manager._discard(handle, name)
                    loaded = None
            if loaded is None:
                if holds and allow_fresh:
                    fresh.append((component, component_plan))
                continue
            ledger = loaded.ledger
            for ref in loaded.units:
                now = ledger.tier(ref)
                target = targets[ref]
                if now is target:
                    continue
                if now is Tier.DEVICE:
                    (park_out if target is Tier.HOST else drops[target]).append((loaded, ref))
                elif target is Tier.DEVICE:
                    if allow_incoming:
                        incoming.append((loaded, ref))
                elif now is Tier.HOST:
                    _release_host_unit(loaded, ref, target, transport)
                elif target is not Tier.HOST:
                    ledger.move_without_copy(ref, target)
        for tier, moves_out in drops.items():
            if moves_out:
                transfer_block_wise(moves_out, [], outgoing_to=tier, transport=transport, owner=f"large models {handle.key}")
        if park_out or incoming:
            report = transfer_block_wise(park_out, incoming, outgoing_to=Tier.HOST, transport=transport, watch=self._watch, owner=f"large models {handle.key}")
            if report.dropped_instead:
                self.dropped_instead.extend(report.dropped_instead)
        for component, component_plan in fresh:
            self._manager._load_fresh(handle, component, component_plan, self._inventories[component.name], options=self.options, watch=self._watch, progress=self._progress)
        transport.release_cache()
        self._manager._publish(handle)


def _release_host_unit(component: LoadedComponent, ref: UnitRef, to: Tier, transport: Transport) -> None:
    """Forget a HOST unit's packed buffer (the unit becomes `to`: DISK or DROPPED)."""
    set_unit_to_meta(component, ref)
    buffer = component.host_buffers.pop(ref, None)
    if buffer is not None:
        transport.free(buffer)
    component.ledger.move_without_copy(ref, to)


def _transport_device(transport: Transport, fallback: str) -> Any:
    """The torch device a transport fills units onto (`TorchTransport.device`; test transports carry one too)."""
    return getattr(transport, "device", fallback)


# =====================================================================
#  Manager
# =====================================================================
class LargeModelManager:
    """The large-model pool (see the module contracts).

    `arbiter` is the backend's shared `ResidencyArbiter` (this manager registers
    the large `PoolPolicy` with it); `limits_store` persists the user limits and
    the learned floors; `tables` is one `CostTables` for every device or a
    callable `device -> CostTables`; `clock` stamps floors. Optional:
    `probes` (pass the SAME `MemoryBudgetProbes` instance the arbiter uses),
    `transport_factory(device) -> Transport` (default `TorchTransport`),
    `device_class(device) -> str` (default `cost_tables.detect_device_class`),
    `spill_detector` (default: `memory_budget.detect_sysmem_spill` on Windows,
    none elsewhere), `max_retries`, `host_device(device) -> bool` (whether a
    run device's memory IS the host's; default: not
    `residency.footprint.is_device_accounted`, i.e. everything but `cuda*`;
    tests that stand a CPU in for a GPU pass `lambda device: False`).
    """

    def __init__(
        self,
        arbiter: ResidencyArbiter,
        *,
        limits_store: LimitsStore,
        tables: CostTables | Callable[[str], CostTables],
        clock: Callable[[], float] = time.time,
        probes: BudgetProbes | None = None,
        transport_factory: Callable[[str], Transport] | None = None,
        device_class: Callable[[str], str] | None = None,
        spill_detector: SpillDetector | None = None,
        max_retries: int = MAX_RETRIES,
        host_device: Callable[[str], bool] | None = None,
    ) -> None:
        self._arbiter = arbiter
        self._store = limits_store
        self._floors = LearnedFloors(limits_store)
        self._limits = limits_store.read_limits()
        self._tables_for: Callable[[str], CostTables] = tables if not isinstance(tables, CostTables) else (lambda device: tables)
        self._clock = clock
        self._probes: BudgetProbes = probes if probes is not None else MemoryBudgetProbes()
        self._transport_factory: Callable[[str], Transport] = transport_factory if transport_factory is not None else TorchTransport
        self._device_class = device_class if device_class is not None else detect_device_class
        # The sysmem fallback exists only in NVIDIA's Windows driver: elsewhere a
        # spill cannot happen (allocations past the card raise), so no detector.
        self._spill_detector: SpillDetector | None = spill_detector if spill_detector is not None else (detect_sysmem_spill if sys.platform == "win32" else None)
        self._max_retries = max_retries
        self._is_host_device: Callable[[str], bool] = host_device if host_device is not None else (lambda name: not is_device_accounted(name))
        self._lock = threading.Lock()
        # Serializes `set_limits` (merge + store write + in-memory update); never
        # taken by readers, never held across anything but that store write.
        self._limits_write_lock = threading.Lock()
        self._specs: dict[str, LargeModelSpec] = {}
        self._handles: dict[str, _Handle] = {}
        self._inventory_cache: dict[tuple[str, str, tuple[tuple[str, tuple[tuple[str, int, int], ...]], ...]], dict[str, ComponentInventory]] = {}
        arbiter.register_pool(_LargePoolPolicy(self))

    # ------------------------------------------------------------ registry
    def register(self, spec: LargeModelSpec, *, model: str | None = None) -> None:
        """Register `spec` under `model` (default `spec.family`).

        # Raises
        `ValueError` when the name is taken or the spec declares no `formats`.
        """
        name = model or spec.family
        if spec.formats is None:
            raise ValueError(f"LargeModelSpec «{spec.family}»: нет formats, менеджер не может её планировать")
        with self._lock:
            if name in self._specs:
                raise ValueError(f"Большая модель «{name}» уже зарегистрирована")
            self._specs[name] = spec

    def models(self) -> list[str]:
        """Registered model names."""
        with self._lock:
            return sorted(self._specs)

    # ------------------------------------------------------------- acquire
    def acquire(self, model: str, options: Any, shape: RequestShape, *, device: str, format: str, steps: Steps = AUTO_STEPS, owner: str | None = None, observer: SessionObserver | None = None, device_cap_bytes: int | None = None) -> Session:
        """Admit one run of `model` in weight format `format` on `device` with the requested `steps`; returns the `Session` the service drives.

        Call it BEFORE the service lock (the admission may wait for evictions
        that take other services' locks). The one exception: a service that (i)
        holds no small-pool lease and registers no reclaim callback that takes
        its lock, and (ii) opens several handles in one operation acquires
        every session UNDER its lock, in one fixed handle order (a run lock is
        taken blocking, so acquiring them outside the lock would invert the
        order with a concurrent single-handle action). `owner` defaults to the
        residency key. `device_cap_bytes` (`None` = none) is a ceiling on the
        device peak THIS handle's plan may forecast (`Budgets.device_cap_bytes`:
        after claims and reclaim, never raising the user's `vram_cap`); it holds
        for the admission and for every re-plan of the degrade ladder.

        # Raises
        `ValueError` for an unregistered model, a negative / non-integer
        `device_cap_bytes` or a host pin the spec cannot honour
        (`LargeModelSpec.host_pin`); `LargeModelRefused` when no plan
        fits (shortest resource and format suggestions named) or the reservation
        cannot be made; `ModelMemoryShortError` is never raised here (large
        leases carry no admission hint).
        """
        _check_device_cap(device_cap_bytes)
        spec = self._spec(model)
        device_name = normalize_device(device) or str(device)
        key = spec.key(options, format, device_name)
        owner_name = owner or key
        lease = self._arbiter.begin_use(Pool.LARGE, key, owner=owner_name)
        handle: _Handle | None = None
        locked = False
        try:
            handle = self._handle_for(model, key, spec, device_name, format, spec.host_pin(options))
            handle.run_lock.acquire()
            locked = True
            session = self._admit(handle, lease, owner_name, options, shape, steps, observer, device_cap_bytes)
        except BaseException:
            if handle is not None and locked:
                handle.in_session = False
                handle.run_lock.release()
            lease.mark_load_failed()
            lease.release()
            raise
        return session

    def _admit(self, handle: _Handle, lease: ModelUsageLease, owner: str, options: Any, shape: RequestShape, steps: Steps, observer: SessionObserver | None, device_cap_bytes: int | None) -> Session:
        spec = handle.spec
        formats = self._formats(spec, options)
        all_inventories = self._inventories(handle.model, spec, options, formats)
        tables = self._tables_for(handle.device)
        floor = self._floors.get(handle.floor_key)
        requester = Requester(pool=Pool.LARGE, key=handle.key, owner=owner, urgency=Urgency.ADMISSION, holds_lease=True)
        shortfall: Shortfall | None = None
        for attempt in range(2):
            inputs = _with_device_cap(self._inputs(handle, requester, own_reservation=None, with_reclaim=True), device_cap_bytes)
            result = self._plan_request(spec, all_inventories, handle.weight_format, formats, steps, inputs, shape, tables, options, floor)
            if isinstance(result, PlanRefusal):
                log.warning("large models %s: admission refused (%s, short on %s)", handle.key, result.reason, result.shortest_resource)
                raise LargeModelRefused(handle.model, handle.weight_format, refusal=result)
            baseline = inputs.budgets.baseline_rss
            peaks = _whole_run_peaks(result, baseline)
            need = self._run_need(handle, peaks)
            reserved = self._arbiter.reserve(requester, need)
            if isinstance(reserved, Reservation):
                break
            shortfall = reserved
            log.warning("large models %s: reservation of the %s plan failed (attempt %d: %s short by %d B); re-planning on fresh budgets", handle.key, result.plan.steps.label, attempt + 1, reserved.resource, reserved.deficit)
        else:
            raise LargeModelRefused(handle.model, handle.weight_format, shortfall=shortfall)
        handle.in_session = True
        log.info("large models %s: admitted %s -> %s (fit %s), device peak %d B, host peak %d B", handle.key, steps.label, result.plan.steps.label, result.fit.kind.value, result.forecast.device_peak, result.forecast.host_peak)
        session = Session(self, handle, lease, owner=owner, inventories=all_inventories[handle.weight_format], all_inventories=all_inventories, formats=formats, shape=shape, tables=tables, planned=result, options=options, reservation=reserved, baseline_rss=baseline, observer=observer, device_cap_bytes=device_cap_bytes)
        # Parts first: `finish_load` keeps a large holding's parts when it has some.
        self._publish(handle)
        lease.mark_loaded()
        return session

    def _plan_request(self, spec: LargeModelSpec, inventories: Mapping[str, Mapping[str, ComponentInventory]], weight_format: str, formats: Mapping[str, FormatInfo], steps: Steps, inputs: _Inputs, shape: RequestShape, tables: CostTables, options: Any, floor: StepFloor | None) -> PlanResult:
        """The planner call of an admission or a `profiles` answer, with the Q-P1 rule for «Авто».

        «Авто» on the VRAM axis may count other pools' idle holdings as free,
        but it never lands more than ONE VRAM step above what fits in free
        memory alone, and (RAM also «Авто») never on a higher RAM step: one
        position up the RAM-major walk. When nothing fits in free memory, a
        reclaim may make only the most economical VRAM step fit. An explicit
        VRAM step may reclaim as much as it needs.
        """
        limits = self.limits()

        def run(budgets: Budgets, chosen_floor: StepFloor | None) -> PlanResult:
            return plan(spec, inventories, format=weight_format, formats=formats, steps=steps, limits=limits, budgets=budgets, shape=shape, tables=tables, options=options, floor=chosen_floor)

        reclaimable = inputs.reclaimable
        ram_auto = steps.ram is RamStep.AUTO
        if steps.vram is not VramStep.AUTO or (reclaimable.device_bytes <= 0 and reclaimable.host_bytes <= 0):
            return run(replace(inputs.budgets, reclaimable=reclaimable), floor)
        free = run(replace(inputs.budgets, reclaimable=Reclaimable()), floor)
        if isinstance(free, Planned):
            fit = free.plan.steps
            if vram_index(fit.vram) == 0:
                return free
            bound = StepFloor(VRAM_ORDER[vram_index(fit.vram) - 1], fit.ram if ram_auto else None)
        else:
            bound = StepFloor(VramStep.MIN, RamStep.MIN if ram_auto else None)
        with_reclaim = run(replace(inputs.budgets, reclaimable=reclaimable), _merge_floors(bound, floor))
        if isinstance(with_reclaim, Planned) or not isinstance(free, Planned):
            return with_reclaim
        return free

    # ------------------------------------------------------- read-only API
    def profiles(self, model: str, options: Any, shape: RequestShape, *, device: str, format: str | None = None, steps: Steps = AUTO_STEPS, claimed_device_bytes: int = 0, claimed_host_bytes: int = 0, device_cap_bytes: int | None = None) -> ProfilesReport:
        """The step pickers for `model` / `format` (default: the best available format), the request `shape` and the requested `steps`.

        `selected` resolves the request exactly as an admission would; the VRAM
        rows are planned with the selected RAM step and the RAM rows with the
        selected VRAM step (the requested one, or «Максимум скорости» /
        «Держать модели в памяти», when the selection is refused on an «Авто»
        axis). Read-only: probes and plans, never loads, reserves or reclaims.
        `claimed_device_bytes` / `claimed_host_bytes` are bytes another handle
        of the same operation will hold (e.g. FLUX.2's encoder admitted beside
        its pipeline), taken out of the room on top of the other pools'
        reservations; on a host run device both are host bytes.
        `device_cap_bytes` is the per-handle device ceiling of `acquire`, applied
        to every row the same way.

        # Raises
        `ValueError` for an unregistered model, a negative / non-integer claim
        or cap, or a host pin the spec cannot honour.
        """
        _check_device_cap(device_cap_bytes)
        for name, value in (("claimed_device_bytes", claimed_device_bytes), ("claimed_host_bytes", claimed_host_bytes)):
            if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                raise ValueError(f"profiles: «{name}» должно быть неотрицательным целым числом байт, получено {value!r}")
        spec = self._spec(model)
        device_name = normalize_device(device) or str(device)
        formats = self._formats(spec, options)
        weight_format = format or _best_format(formats)
        inventories = self._inventories(model, spec, options, formats)
        tables = self._tables_for(device_name)
        key = spec.key(options, weight_format, device_name)
        with self._lock:
            handle = self._handles.get(key)
        floor_key = FloorKey(self._floor_class(device_name), model, weight_format, tuple(sorted(spec.host_pin(options))))
        floor_entry = next((entry for entry in self._floors.entries(model) if entry.key == floor_key), None)
        floor = floor_entry.floor if floor_entry is not None else None
        requester = Requester(pool=Pool.LARGE, key=key, owner=key, urgency=Urgency.ADMISSION, holds_lease=False)
        inputs = self._inputs(handle, requester, own_reservation=None, with_reclaim=True, device=device_name)
        if claimed_device_bytes or claimed_host_bytes:
            base = inputs.budgets
            if base.host_device:
                claimed = replace(base, claimed_host_bytes=base.claimed_host_bytes + claimed_device_bytes + claimed_host_bytes)
            else:
                claimed = replace(base, claimed_device_bytes=base.claimed_device_bytes + claimed_device_bytes, claimed_host_bytes=base.claimed_host_bytes + claimed_host_bytes)
            inputs = replace(inputs, budgets=claimed)
        inputs = _with_device_cap(inputs, device_cap_bytes)
        limits = self.limits()
        budgets = replace(inputs.budgets, reclaimable=inputs.reclaimable)

        def resolve(name: str, wanted: Steps, chosen_floor: StepFloor | None) -> PlanResult:
            return self._plan_request(spec, inventories, name, formats, wanted, inputs, shape, tables, options, chosen_floor)

        selected = resolve(weight_format, steps, floor)
        ram_fixed = selected.plan.steps.ram if isinstance(selected, Planned) else (steps.ram if steps.ram is not RamStep.AUTO else RamStep.KEEP)
        vram_fixed = selected.plan.steps.vram if isinstance(selected, Planned) else (steps.vram if steps.vram is not VramStep.AUTO else VramStep.MAX)
        grid = plan_grid(spec, inventories, format=weight_format, formats=formats, limits=limits, budgets=budgets, shape=shape, tables=tables, options=options)
        vram_auto = resolve(weight_format, Steps(VramStep.AUTO, ram_fixed), floor)
        ram_auto = resolve(weight_format, Steps(vram_fixed, RamStep.AUTO), floor)
        rows: list[FormatRow] = []
        recommended: Steps | None = None
        for name, info in formats.items():
            best: Steps | None = None
            if info.available and name in inventories:
                other_floor = self._floors.get(replace(floor_key, format=name))
                answer = resolve(name, AUTO_STEPS, other_floor)
                best = answer.plan.steps if isinstance(answer, Planned) else None
            if name == weight_format:
                recommended = best
            rows.append(FormatRow(name, info, best))
        host = inputs.budgets.host
        dev = inputs.budgets.device
        vram_applicable = not inputs.budgets.host_device
        return ProfilesReport(
            model=model,
            format=weight_format,
            steps_requested=steps,
            selected=selected,
            vram_rows=MappingProxyType({step: result for step, result in axis_rows(grid, axis="vram", fixed=ram_fixed).items() if isinstance(step, VramStep)} if vram_applicable else {}),
            ram_rows=MappingProxyType({step: result for step, result in axis_rows(grid, axis="ram", fixed=vram_fixed).items() if isinstance(step, RamStep)}),
            vram_recommended=vram_auto.plan.steps.vram if isinstance(vram_auto, Planned) else None,
            ram_recommended=ram_auto.plan.steps.ram if isinstance(ram_auto, Planned) else None,
            recommended=recommended,
            floor=floor_entry,
            limits=limits,
            host={"total": host.total, "free": host.headroom(), "cgroup_limit": host.cgroup_limit, "reserve": host.reserve},
            device={"total": dev.total, "free": dev.headroom(), "reserve": dev.reserve},
            formats=tuple(rows),
            vram_applicable=vram_applicable,
        )

    def limits(self) -> Limits:
        """The user's limits in force."""
        with self._lock:
            return self._limits

    def set_limits(self, *, vram_cap: int | None | _Keep = KEEP_LIMIT, ram_cap: int | None | _Keep = KEEP_LIMIT) -> Limits:
        """Change the user's hard caps (bytes, over every step) and persist them; returns the limits now in force.

        Partial: a cap left at `KEEP_LIMIT` (the default, i.e. not passed)
        keeps its value, `None` clears it (no limit). Merging with the limits
        in force, the store write and the in-memory update are ONE critical
        section (`_limits_write_lock`, serializing writers only): concurrent
        partial sets never lose a cap, and the store and the limits in force
        always end equal. `limits()` readers are never blocked by the write.

        # Raises
        `ValueError` for a negative or non-integer cap (nothing changes);
        `OSError` when the store cannot be written (the limits in force are
        then unchanged).
        """
        for name, value in (("vram_cap", vram_cap), ("ram_cap", ram_cap)):
            if value is KEEP_LIMIT or value is None:
                continue
            if isinstance(value, bool) or not isinstance(value, int) or value < 0:
                raise ValueError(f"Предел «{name}» должен быть неотрицательным целым числом байт или null, получено {value!r}")
        with self._limits_write_lock:
            current = self.limits()
            limits = Limits(
                vram_cap=current.vram_cap if isinstance(vram_cap, _Keep) else vram_cap,
                ram_cap=current.ram_cap if isinstance(ram_cap, _Keep) else ram_cap,
            )
            self._store.write_limits(limits)
            with self._lock:
                self._limits = limits
        log.info("large models: limits set: vram_cap=%s, ram_cap=%s", limits.vram_cap, limits.ram_cap)
        return limits

    def floors(self, model: str | None = None) -> list[FloorEntry]:
        """The learned floors (of `model`, when given)."""
        return self._floors.entries(model)

    def reset_floors(self, model: str | None = None) -> int:
        """Forget the learned floors (of `model`, when given); returns how many were cleared."""
        return self._floors.reset(model)

    def status(self) -> list[dict[str, Any]]:
        """One entry per handle: `{model, key, format, device, host_device, vram_step, ram_step, device_bytes, host_bytes, in_use, degraded, degrade_reason, components}`.

        `vram_step` / `ram_step` are the effective steps of the last finished
        run (`None` before one; on a `host_device` the VRAM step is the one
        `tiers.host_device_steps` derived from the RAM step); `device_bytes` /
        `host_bytes` are the ledger's (a host device's "device" bytes are RAM
        too); `components` maps each component to its
        `ComponentPlacement.to_dict()`. No probe, never waits for a session.
        """
        with self._lock:
            handles = list(self._handles.values())
        entries = []
        for handle in sorted(handles, key=lambda item: item.key):
            held = handle.ledger.holdings()
            entries.append({
                "model": handle.model,
                "key": handle.key,
                "format": handle.weight_format,
                "device": handle.device,
                "host_device": handle.host_device,
                "vram_step": handle.steps_effective.vram.value if handle.steps_effective is not None else None,
                "ram_step": handle.steps_effective.ram.value if handle.steps_effective is not None else None,
                "device_bytes": held.device_bytes,
                "host_bytes": held.host_bytes,
                "in_use": handle.in_session,
                "degraded": handle.degraded,
                "degrade_reason": handle.degrade_reason,
                "components": {name: placement.to_dict() for name, placement in _placements(handle).items()},
            })
        return entries

    def placements(self, key: str) -> Mapping[str, ComponentPlacement]:
        """Where every component of the handle `key` is right now (empty for an unknown key).

        A snapshot of the handle's ledger (each component read under the
        ledger's leaf lock): no probe, and it never waits for a running
        session, so a service may call it from its health path.
        """
        with self._lock:
            handle = self._handles.get(key)
        if handle is None:
            return MappingProxyType({})
        return _placements(handle)

    def health(self) -> dict[str, Any]:
        """The health payload part: `{large_models: status(), limits: {vram_cap, ram_cap}}` (no probe)."""
        return {"large_models": self.status(), "limits": _limits_dict(self.limits())}

    def unload(self, model: str | None = None) -> list[str]:
        """Drop every idle handle (of `model`, when given); returns the keys that were dropped. A handle in a session is skipped."""
        with self._lock:
            handles = [handle for handle in self._handles.values() if model is None or handle.model == model]
        return [handle.key for handle in handles if self._unload_handle(handle)]

    def unload_key(self, key: str) -> bool:
        """Drop exactly the idle handle `key` (every other handle, of the same model too, is untouched).

        Returns whether bytes were dropped: `False` for an unknown key, a
        handle that holds nothing, or one in a session (never waited for).
        """
        with self._lock:
            handle = self._handles.get(key)
        return handle is not None and self._unload_handle(handle)

    def _unload_handle(self, handle: _Handle) -> bool:
        """Drop one idle handle's bytes (try-lock of its run lock: a session is skipped); `True` when something was dropped."""
        if not handle.run_lock.acquire(blocking=False):
            log.info("large models %s: in a session, not unloaded", handle.key)
            return False
        try:
            held = handle.ledger.holdings()
            if held.device_bytes == 0 and held.host_bytes == 0:
                return False
            self._drop_all(handle)
            self._publish(handle)
            self._arbiter.mark_unloaded(Pool.LARGE, handle.key)
            return True
        finally:
            handle.run_lock.release()

    # ----------------------------------------------------------- internals
    def _spec(self, model: str) -> LargeModelSpec:
        with self._lock:
            spec = self._specs.get(model)
        if spec is None:
            raise ValueError(f"Большая модель «{model}» не зарегистрирована")
        return spec

    @staticmethod
    def _formats(spec: LargeModelSpec, options: Any) -> Mapping[str, FormatInfo]:
        assert spec.formats is not None  # checked by register
        return spec.formats(options)

    def _handle_for(self, model: str, key: str, spec: LargeModelSpec, device: str, weight_format: str, host_pin: frozenset[str]) -> _Handle:
        with self._lock:
            handle = self._handles.get(key)
            if handle is not None:
                return handle
        # Created outside the lock: the transport and the device class may touch torch.
        transport = self._transport_factory(device)
        tables = self._tables_for(device)
        floor_key = FloorKey(self._floor_class(device), model, weight_format, tuple(sorted(host_pin)))
        created = _Handle(model=model, key=key, spec=spec, device=device, weight_format=weight_format, transport=transport, ledger=Ledger(max_in_flight=max(2, tables.device.prefetch_depth + 1)), floor_key=floor_key, host_device=self._is_host_device(device))
        with self._lock:
            return self._handles.setdefault(key, created)

    def _floor_class(self, device: str) -> str:
        """The learned-floor device class of `device`: `HOST_DEVICE_CLASS` for a host run device, else the device's calibration class."""
        return HOST_DEVICE_CLASS if self._is_host_device(device) else self._device_class(device)

    def _inventories(self, model: str, spec: LargeModelSpec, options: Any, formats: Mapping[str, FormatInfo]) -> dict[str, dict[str, ComponentInventory]]:
        """Inventories of every available format (cached per file set, size and mtime); a format whose headers fail is left out (logged)."""
        result: dict[str, dict[str, ComponentInventory]] = {}
        for weight_format, info in formats.items():
            if not info.available:
                continue
            try:
                files = {component.name: [Path(path) for path in component.files(options, weight_format)] for component in spec.components}
                signature = tuple((name, tuple((str(path), path.stat().st_size, path.stat().st_mtime_ns) for path in paths)) for name, paths in files.items())
                cache_key = (model, weight_format, signature)
                with self._lock:
                    cached = self._inventory_cache.get(cache_key)
                if cached is None:
                    cached = {component.name: build_inventory(component, files[component.name]) for component in spec.components}
                    with self._lock:
                        self._inventory_cache[cache_key] = cached
                result[weight_format] = cached
            except (OSError, ValueError) as exc:
                log.warning("large models %s: format %s announced available but its files cannot be inventoried (%s); left out", model, weight_format, exc)
        return result

    def _inputs(self, handle: _Handle | None, requester: Requester, *, own_reservation: Reservation | None, with_reclaim: bool, device: str | None = None) -> _Inputs:
        """Fresh planner inputs: probes, own bytes (+ own torch slack) credited, others' reservations claimed, reclaimable bytes.

        A host run device reads no device budget: everything is on the host side (`Budgets.host_device`).
        """
        device_name = device or (handle.device if handle is not None else "")
        own = handle.ledger.holdings() if handle is not None else Holdings(0, 0)
        if self._is_host_device(device_name):
            return self._host_device_inputs(device_name, own, requester, own_reservation=own_reservation, with_reclaim=with_reclaim)
        host = self._probes.host()
        budget = self._probes.device(device_name)
        view = self._arbiter.view(device_name)
        slack = max(0, budget.torch_reserved - budget.torch_allocated)
        claimed_device = view.reserved_device.get(device_name, 0)
        claimed_host = view.reserved_host
        if own_reservation is not None:
            claimed_device -= own_reservation.need.device_bytes if own_reservation.need.device == device_name else 0
            claimed_host -= own_reservation.need.host_bytes
        reclaimable = Reclaimable()
        if with_reclaim:
            small = view.reclaimable(Pool.SMALL, device=device_name, requester=requester)
            large = view.reclaimable(Pool.LARGE, device=device_name, requester=requester)
            reclaimable = Reclaimable(small.device_bytes + large.device_bytes, small.host_bytes + large.host_bytes)
        budgets = Budgets(host=host, device=budget, baseline_rss=max(0, host.rss_private - own.host_bytes), own_device_bytes=own.device_bytes + slack, own_host_bytes=own.host_bytes, claimed_device_bytes=max(0, claimed_device), claimed_host_bytes=max(0, claimed_host))
        return _Inputs(budgets=budgets, reclaimable=reclaimable)

    def _host_device_inputs(self, device_name: str, own: Holdings, requester: Requester, *, own_reservation: Reservation | None, with_reclaim: bool) -> _Inputs:
        """`_inputs` of a host run device: a zero device budget (never probed), every own / claimed / reclaimable byte on the host side."""
        host = self._probes.host()
        view = self._arbiter.view(None)
        own_host = own.device_bytes + own.host_bytes
        claimed_host = view.reserved_host - (own_reservation.need.host_bytes if own_reservation is not None else 0)
        reclaimable = Reclaimable()
        if with_reclaim:
            small = view.reclaimable(Pool.SMALL, device=None, requester=requester)
            large = view.reclaimable(Pool.LARGE, device=None, requester=requester)
            reclaimable = Reclaimable(0, small.host_bytes + large.host_bytes)
        device = DeviceBudget(device=device_name, total=0, free=0, torch_allocated=0, torch_reserved=0, non_torch=0, reserve=0, cap=None)
        budgets = Budgets(host=host, device=device, baseline_rss=max(0, host.rss_private - own_host), own_device_bytes=0, own_host_bytes=own_host, claimed_device_bytes=0, claimed_host_bytes=max(0, claimed_host), host_device=True)
        return _Inputs(budgets=budgets, reclaimable=reclaimable)

    def _run_need(self, handle: _Handle, peaks: tuple[int, int], *, extra_device: int = 0, extra_host: int = 0) -> Need:
        """The reservation for `peaks` (device peak, host growth) minus what the handle already holds (all on the host for a host run device)."""
        own = handle.ledger.holdings()
        device_peak, host_growth = peaks
        if handle.host_device:
            held = own.device_bytes + own.host_bytes
            return Need(device=None, device_bytes=0, host_bytes=max(0, device_peak + host_growth - held) + max(0, extra_device) + max(0, extra_host))
        return Need(device=handle.device, device_bytes=max(0, device_peak - own.device_bytes) + max(0, extra_device), host_bytes=max(0, host_growth - own.host_bytes) + max(0, extra_host))

    def _others_held(self, key: str, device: str | None) -> int:
        """Ledger bytes every OTHER resident holding has on `device` (host bytes when `None`)."""
        total = 0
        for pool in Pool:
            for holding in self._arbiter.holdings(pool).values():
                if not holding.resident or (pool is Pool.LARGE and holding.key == key):
                    continue
                if device is None:
                    total += holding.footprint.host_bytes
                elif holding.footprint.device == device:
                    total += holding.footprint.device_bytes
        return total

    def _spill_probe(self, device: str) -> DeviceBudget | None:
        if self._spill_detector is None or not device.startswith("cuda"):
            return None
        return self._probes.device(device)

    def _publish(self, handle: _Handle) -> None:
        """Report the handle's per-component bytes to the arbiter (`update_parts`); a host run device reports them all as host bytes."""
        constants = self._tables_for(handle.device).device
        parts: list[Part] = []
        for component in handle.spec.components:
            loaded = handle.components.get(component.name)
            held = handle.ledger.holdings(component.name) if loaded is not None else Holdings(0, 0)
            reload_s = loaded.inventory.file_bytes_total / constants.disk_cold_bytes_per_s if loaded is not None else 0.0
            if handle.host_device:
                # The arbiter's convention for a non-accounted device: its bytes are host bytes
                # (and a part with no device bytes is only ever offered as a DROP).
                parts.append(Part(name=component.name, device_bytes=0, host_bytes=held.device_bytes + held.host_bytes, tier=PartTier.HOST, reload_cost_s=reload_s))
            else:
                parts.append(Part(name=component.name, device_bytes=held.device_bytes, host_bytes=held.host_bytes, tier=PartTier.DEVICE if held.device_bytes > 0 else PartTier.HOST, reload_cost_s=reload_s))
        self._arbiter.update_parts(Pool.LARGE, handle.key, parts, device=None if handle.host_device else handle.device)

    def _load_fresh(self, handle: _Handle, component: ComponentSpec, component_plan: Any, inventory: ComponentInventory, *, options: Any, watch: HostWatch, progress: Any) -> None:
        """Load `component` whole per `component_plan` (the one filler), registering its units in the handle's ledger."""
        transport = handle.transport
        loaded = load_component(component, inventory, component_plan, options=options, weight_format=handle.weight_format, device=_transport_device(transport, handle.device), ledger=handle.ledger, watch=watch, progress=progress, owner=f"large models {handle.key}: {component.name}", device_is_host=handle.host_device)
        for ref in loaded.refs(tiers=(Tier.DEVICE,)):
            for entry in loaded.units[ref]:
                transport.track_device(entry.get())
        handle.components[component.name] = loaded
        handle.exec_devices[component.name] = component_plan.exec_device

    def _discard(self, handle: _Handle, name: str) -> None:
        """Drop `name`'s bytes and forget its module and units (it will be loaded fresh)."""
        loaded = handle.components.pop(name, None)
        handle.exec_devices.pop(name, None)
        if loaded is None:
            return
        drop(loaded, transport=handle.transport)
        handle.ledger.forget_component(name)
        del loaded

    def _drop_all(self, handle: _Handle) -> None:
        """Drop every component of `handle` (bytes released, modules forgotten)."""
        for name in list(handle.components):
            self._discard(handle, name)
        handle.transport.release_cache()

    def _end_session(self, handle: _Handle, lease: ModelUsageLease, reservation: Reservation) -> None:
        """Release the run reservation, report the idle footprint, return the run lock, then the lease."""
        try:
            self._arbiter.release_reservation(reservation)
            self._publish(handle)
            held = handle.ledger.holdings()
            if held.device_bytes == 0 and held.host_bytes == 0:
                self._arbiter.mark_unloaded(Pool.LARGE, handle.key)
            log.info("large models %s: idle footprint device %d B, host %d B", handle.key, held.device_bytes, held.host_bytes)
        finally:
            handle.in_session = False
            handle.run_lock.release()
            lease.release()

    def _execute_offer(self, offer: Offer) -> Freed:
        """Park or drop one idle component for another requester (never waits for a session)."""
        with self._lock:
            handle = self._handles.get(offer.key)
        if handle is None or offer.part is None:
            return Freed(0, 0, ok=False)
        if not handle.run_lock.acquire(blocking=False):
            log.info("large models %s: reclaim %s skipped, the handle is in a session", offer.key, offer.action.value)
            return Freed(0, 0, ok=False)
        try:
            component = handle.components.get(offer.part)
            if component is None:
                return Freed(0, 0, ok=False)
            before = handle.ledger.holdings(component.name)
            match offer.action:
                case Action.PARK_TO_HOST:
                    if not component.refs(tiers=(Tier.DEVICE,)):
                        return Freed(0, 0, ok=False)
                    park(component, to=Tier.HOST, transport=handle.transport, watch=HostWatch(self._probes.host, floor=0), owner=f"large models {handle.key} (reclaim)")
                case Action.DROP:
                    self._discard(handle, component.name)
            handle.transport.release_cache()
            after = handle.ledger.holdings(component.name) if component.name in handle.components else Holdings(0, 0)
            self._publish(handle)
            total = handle.ledger.holdings()
            if total.device_bytes == 0 and total.host_bytes == 0:
                self._arbiter.mark_unloaded(Pool.LARGE, handle.key)
            freed = Freed(device_bytes=before.device_bytes - after.device_bytes, host_bytes=before.host_bytes - after.host_bytes, ok=True)
            log.info("large models %s: %s %s for another requester: device -%d B, host %+d B", handle.key, offer.action.value, component.name, freed.device_bytes, -freed.host_bytes)
            return freed
        finally:
            handle.run_lock.release()


def _merge_floors(first: StepFloor, second: StepFloor | None) -> StepFloor:
    """Per axis, the more economical of two floors (an axis without a floor takes the other's)."""
    if second is None:
        return first
    vram = second.vram if first.vram is None else (first.vram if second.vram is None else more_economical_vram(first.vram, second.vram))
    ram = second.ram if first.ram is None else (first.ram if second.ram is None else more_economical_ram(first.ram, second.ram))
    return StepFloor(vram, ram)


def _placements(handle: _Handle) -> Mapping[str, ComponentPlacement]:
    """Every component of `handle` as a `ComponentPlacement`, from its ledger (see `LargeModelManager.placements`)."""
    found: dict[str, ComponentPlacement] = {}
    for component in handle.spec.components:
        name = component.name
        states = handle.ledger.units(name)
        held = handle.ledger.holdings(name)
        counts = {Tier.DEVICE: 0, Tier.HOST: 0, Tier.DISK: 0}
        tiers = {state.tier for state in states}
        for state in states:
            if state.ref.block is not None and state.tier in counts:
                counts[state.tier] += 1
        tier: Tier | None
        if not tiers & {Tier.DEVICE, Tier.HOST}:
            tier = None
        elif tiers == {Tier.DEVICE}:
            tier = Tier.DEVICE
        elif Tier.HOST in tiers:
            tier = Tier.HOST
        else:
            tier = Tier.DISK
        found[name] = ComponentPlacement(name, tier, handle.exec_devices.get(name), counts[Tier.DEVICE], counts[Tier.HOST], counts[Tier.DISK], held.device_bytes, held.host_bytes)
    return MappingProxyType(found)


def _check_device_cap(device_cap_bytes: int | None) -> None:
    """`ValueError` unless `device_cap_bytes` is `None` or a non-negative integer."""
    if device_cap_bytes is None:
        return
    if isinstance(device_cap_bytes, bool) or not isinstance(device_cap_bytes, int) or device_cap_bytes < 0:
        raise ValueError(f"«device_cap_bytes» должно быть неотрицательным целым числом байт или None, получено {device_cap_bytes!r}")


def _with_device_cap(inputs: _Inputs, device_cap_bytes: int | None) -> _Inputs:
    """`inputs` with the per-handle device ceiling set on its budgets (`None` leaves them unchanged)."""
    if device_cap_bytes is None:
        return inputs
    return replace(inputs, budgets=replace(inputs.budgets, device_cap_bytes=device_cap_bytes))


def _whole_run_peaks(planned: Planned, baseline_rss: int) -> tuple[int, int]:
    """(device peak, host growth) of the whole run, from the admitted forecast."""
    return planned.forecast.device_peak, max(0, planned.forecast.host_peak - baseline_rss)


def _best_format(formats: Mapping[str, FormatInfo]) -> str:
    """The available format of the highest quality rank (any format when none is available)."""
    available = [(info.quality_rank, name) for name, info in formats.items() if info.available]
    pool = available or [(info.quality_rank, name) for name, info in formats.items()]
    if not pool:
        raise ValueError("У модели нет ни одного формата весов")
    return max(pool)[1]

