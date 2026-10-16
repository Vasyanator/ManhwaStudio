"""
File: modules/ai_backend/inpaint/flux3_klein/service.py

Purpose:
`Flux3KleinService` - one FLUX.3 Image Klein generation (caption + up to ten
reference images -> image) driven through the large-model manager
(`runtime.large_models.manager.LargeModelManager`), with every phase reported
to an optional `PhaseObserver`.

Main responsibilities:
- `generate` - validation, reference preparation and the fp8 path BEFORE the
  admission, then `manager.acquire` (the two resource steps «Видеопамять» x
  «Оперативная память» pick where every byte lives;
  the weight format is the user's and never changes), the load scope (DiT, AE)
  and the run scope (encode, reference AE encode, flow-map denoise, decode) as
  manager steps (`Session.load` / `transition` / `run_phase`), each bracketed
  by its observer `Phase`;
- `health` / `unload` - the service contract every backend service has
  (`inpaint/MODULE_README.md`, health snapshot); where each component is comes
  from the manager's ledger (`LargeModelManager.placements`), never from the
  service's own bookkeeping;
- `_model_key` - the residency key (`large_model.residency_key`), from the
  options alone.

Key declarations:
- `Flux3KleinService`, `Flux3Result`, `Flux3Applied`
- `_resolve_selected_backend_device`, `_clear_torch_cache` (patch points)

Notes:
- Not wired: no `server.py` construction, no `AppState` field, no IPC method.
  The calibration tool (`tools/large_model_calibration/`, its `flux3` driver) is the only caller for now.
- The service never moves a weight itself: every placement change is a manager
  step; a phase function fetches its module from the `PhaseContext` on every
  call and is restartable (the manager may retry it on a gentler plan).
- torch and the torch-side siblings (`packing`, `sampling`, `text_encoder`,
  `quant`) are imported inside the methods, so importing this module (and the
  package) does not import torch. Tests patch those siblings on their own
  module objects; the service reaches them as `<module>.<name>` at call time.
"""

from __future__ import annotations

import functools
import logging
import threading
import time
import traceback
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, TypeVar

from ...runtime.health_snapshot import HealthSnapshot
from ...runtime.large_models.degrade import classify_failure
from ...runtime.large_models.manager import ComponentPlacement, LargeModelError, LargeModelManager, LargeModelRunFailed, PhaseContext, Session
from ...runtime.large_models.spec import ExecDevice, RequestShape
from ...runtime.large_models.tiers import ComponentPlan, Steps, Tier
from ...runtime.residency.footprint import normalize_device
from ...runtime.torch_device import clear_torch_cache, resolve_discrete_backend_device
from . import large_model
from .config import ModelFiles, PackageConfig, WeightFormat, read_package_config, resolve_model_files
from .large_model import (
    COMPONENT_AE,
    COMPONENT_DIT,
    COMPONENT_ENCODER,
    ENCODER_KEEP_LAYERS,
    PHASE_DECODE,
    PHASE_DENOISE,
    PHASE_ENCODE,
    PHASE_REF_ENCODE,
    Flux3ModelOptions,
)
from .params import AeDtype, EncoderMode, Flux3Options, Flux3Request, Fp8Compute, normalize_request
from .phases import Phase, PhaseObserver, observed

if TYPE_CHECKING:
    from PIL.Image import Image

    from .packing import PreparedReferences

log = logging.getLogger(__name__)

T = TypeVar("T")

#: `General.ai_device` resolution with this service's policy (owner:
#: `runtime.torch_device`; the FLUX.2 klein policy: the user's choice is
#: honoured, never an integrated GPU). Patched by name in the tests.
_resolve_selected_backend_device = resolve_discrete_backend_device

#: Allocator-cache release with this service's policy (owner:
#: `runtime.torch_device`; CUDA cache only). Patched by name in the tests.
_clear_torch_cache = functools.partial(clear_torch_cache, owner="FLUX.3 klein", include_mps=False)

#: Fallback handed to the device resolution (the FLUX.2 klein binding's value).
_DEVICE_FALLBACK = "cuda"

#: A component's residency as the observer actions and `health()` read it:
#: (`DEVICE` / `HOST` / `DROPPED` = not held, where it executes; `None` = never ran).
_Placement = tuple[Tier, ExecDevice | None]

_NOT_HELD: _Placement = (Tier.DROPPED, None)

#: The components `health()` and the actions look at, in report order.
_COMPONENTS = (COMPONENT_DIT, COMPONENT_AE, COMPONENT_ENCODER)


@dataclass(frozen=True)
class Flux3Applied:
    """What a generation actually ran with.

    `fp8_compute` is the concrete fp8 path of an fp8r DiT (`SCALED_MM` or
    `DEQUANT`; `None` for bf16 / nvfp4, which have no fp8 layer);
    `compute_path` how the format's matmuls ran (`native`, `scaled_mm`,
    `dequant`); `steps_requested` the user's pair (may hold «Авто» on either
    axis), `steps_effective` the concrete pair the run ended on (Auto
    resolved, a learned floor applied, a degrade included); `degraded` whether
    the manager had to lower a step (or take another rung) after a memory
    failure. The weight format is never changed by the manager.
    """

    variant: WeightFormat
    fp8_compute: Fp8Compute | None
    encoder_mode: EncoderMode
    ae_dtype: AeDtype
    compute_path: str
    steps_requested: Steps
    steps_effective: Steps
    degraded: bool


@dataclass(frozen=True)
class Flux3Result:
    """One generated image and the facts behind it.

    `width` / `height` are the returned image's size (after the crop of a single
    padded reference); `text_tokens` is the DiT's text sequence length (padded
    to a multiple of 80), `text_real_tokens` the unpadded count.
    """

    image: Image
    seed: int
    width: int
    height: int
    steps: int
    text_tokens: int
    text_real_tokens: int
    applied: Flux3Applied


@dataclass(frozen=True)
class _Plan:
    """Everything decided before the admission: files, config, request, references, size, device, manager inputs."""

    files: ModelFiles
    package: PackageConfig
    caption: str
    seed: int
    steps: int
    references: PreparedReferences
    width: int
    height: int
    device: str
    model: str
    model_options: Flux3ModelOptions
    shape: RequestShape
    info: Mapping[str, Any]


@dataclass
class _Held:
    """The last run's handle as the service knows it: its (finished) session, key and facts; where its components are is the manager's."""

    session: Session
    key: str
    device: str
    variant: WeightFormat
    fp8_applied: Fp8Compute | None
    compute_path: str


def _model_key(options: Flux3Options, device: str) -> str:
    """The residency key of the weights `options` loads on `device` (see `large_model.residency_key`)."""
    return large_model.residency_key(options, options.variant.value, device)


def _release_failed_run_memory(exc: BaseException) -> None:
    """Free what a failed run left only in `exc`'s frames, then release the allocator cache.

    A tensor a failed call created (the activations of a failed denoise) is
    referenced by the locals of the frames in `exc.__traceback__` until the
    caller drops the exception, so a cache release alone would free nothing.
    `traceback.clear_frames` drops those locals (finished frames only; the
    line information stays) before `_clear_torch_cache`. Call it after the
    traceback was logged.
    """
    traceback.clear_frames(exc.__traceback__)
    _clear_torch_cache()


def _placement_of(placement: ComponentPlacement | None) -> _Placement:
    """Where a component is per the manager's ledger, as the actions and `health()` read it.

    Not held (`tier` `None`) -> `DROPPED`; a component with any block on the
    device (whole, or juggled from RAM or the file) counts as on the device; one
    with no block on the device but units parked in RAM counts as in RAM.
    """
    if placement is None or placement.tier is None:
        return _NOT_HELD
    if placement.tier is Tier.HOST and placement.blocks_device == 0:
        return Tier.HOST, placement.exec_device
    return Tier.DEVICE, placement.exec_device


def _target_of(plan: ComponentPlan) -> _Placement:
    """Where a component will be after a step that reaches `plan` (the prediction an action is named by): juggled with k > 0 = on the device."""
    if plan.resident_blocks is not None and plan.resident_blocks > 0:
        return Tier.DEVICE, plan.exec_device
    tier = Tier.DROPPED if plan.tier is Tier.DISK else plan.tier
    return tier, plan.exec_device


def _holds(placement: _Placement) -> bool:
    return placement[0] in (Tier.DEVICE, Tier.HOST)


def _steps_info(steps: Steps, suffix: str = "") -> dict[str, str]:
    """Observer / health keys of a step pair: `vram_step<suffix>`, `ram_step<suffix>`."""
    return {f"vram_step{suffix}": steps.vram.value, f"ram_step{suffix}": steps.ram.value}


#: The forecast entry each observer phase is measured against: a model phase,
#: or the transition the step performs (`"<from>-><to>"`, `idle` = before the
#: first phase). The load scope and DIT_OFFLOAD are parts of the first
#: transition.
_FORECAST_STEP: Mapping[Phase, tuple[str, str]] = {
    Phase.DIT_LOAD: ("transition", f"idle->{PHASE_ENCODE}"),
    Phase.AE_LOAD: ("transition", f"idle->{PHASE_ENCODE}"),
    Phase.DIT_OFFLOAD: ("transition", f"idle->{PHASE_ENCODE}"),
    Phase.ENCODER_LOAD: ("transition", f"idle->{PHASE_ENCODE}"),
    Phase.ENCODE: ("phase", PHASE_ENCODE),
    Phase.ENCODER_RELEASE: ("transition", f"{PHASE_ENCODE}->{PHASE_REF_ENCODE}"),
    Phase.REF_ENCODE: ("phase", PHASE_REF_ENCODE),
    Phase.DIT_RESTORE: ("transition", f"{PHASE_REF_ENCODE}->{PHASE_DENOISE}"),
    Phase.DENOISE: ("phase", PHASE_DENOISE),
    Phase.DIT_OFFLOAD_DECODE: ("transition", f"{PHASE_DENOISE}->{PHASE_DECODE}"),
    Phase.DECODE: ("phase", PHASE_DECODE),
}


def _forecast_info(session: Session, phase: Phase) -> dict[str, Any]:
    """Observer info: the forecast of `phase`'s step and of the whole run, from the plan in force (bytes; host = absolute RSS)."""
    forecast = session.forecast
    kind, name = _FORECAST_STEP[phase]
    entry = (forecast.phases if kind == "phase" else forecast.transitions).get(name)
    return {
        "forecast_device_peak": entry.device_peak if entry is not None else None,
        "forecast_host_peak": entry.host_peak if entry is not None else None,
        "forecast_run_device_peak": forecast.device_peak,
        "forecast_run_host_peak": forecast.host_peak,
        "forecast_calibrated": forecast.calibrated,
    }


class Flux3KleinService:
    """FLUX.3 klein generator over the large-model manager.

    Registers the FLUX.3 models (`large_model.model_specs`, one per encoder
    mode x AE dtype) with `large_models` once. Between runs the weights stay
    wherever the run's step pair leaves them (the card only under «Максимум
    скорости», RAM only under «Держать модели в памяти»), owned by the manager (an idle handle may be parked or dropped there for another
    model). One `RLock` serializes this service's runs; `health()` never takes
    it.
    """

    def __init__(self, large_models: LargeModelManager) -> None:
        self._lock = threading.RLock()
        self._large = large_models
        registered = set(large_models.models())
        for name, spec in large_model.model_specs().items():
            if name not in registered:
                large_models.register(spec, model=name)
        # Text processors (tokenizer + image processor) per encoder directory:
        # small host objects, not weights, so outside the manager.
        self._processors: dict[str, Any] = {}
        # Replaced (never mutated) so `health()` reads it without the lock.
        self._held: _Held | None = None
        self._last_error: str | None = None
        self._health = HealthSnapshot(device=None, model_key=None, variant=None, fp8_compute=None, compute_path=None, vram_step=None, ram_step=None, degraded=False, last_error=None)

    # ---- health / unload ----
    def health(self) -> dict[str, Any]:
        """The health fields; answers while a run holds the lock (never takes it).

        Published at the end of every run / unload: `device`, `model_key`,
        `variant`, `fp8_compute` (applied fp8 path or `None`), `compute_path`,
        `vram_step` / `ram_step` (the effective pair of the last run), `degraded`,
        `last_error`. Live, from the manager's ledger
        (`LargeModelManager.placements`: no probe, never waits for a session;
        the manager may park or drop an idle handle at any time): `loaded` (any
        weights of the last run's handle held), `ready` (DiT + AE held),
        `dit_device` (the run device while any DiT block is there, `cpu` while
        it is parked in RAM, `None` when not held), `encoder_resident`
        (likewise for the text encoder; `cpu` also when it runs on the CPU).
        """
        fields = self._health.read()
        held = self._held
        placements = self._large.placements(held.key) if held is not None else {}
        locations = {name: self._location(held, placements.get(name)) for name in _COMPONENTS}
        fields.update(
            loaded=any(value is not None for value in locations.values()),
            ready=locations[COMPONENT_DIT] is not None and locations[COMPONENT_AE] is not None,
            dit_device=locations[COMPONENT_DIT],
            encoder_resident=locations[COMPONENT_ENCODER],
        )
        return fields

    @staticmethod
    def _location(held: _Held | None, placement: ComponentPlacement | None) -> str | None:
        """`health()` location of a component placed at `placement`: the run device, `cpu`, or `None` when not held."""
        tier, exec_device = _placement_of(placement)
        if held is None or not _holds((tier, exec_device)):
            return None
        if tier is Tier.DEVICE and exec_device is ExecDevice.RUN_DEVICE:
            return held.device
        return "cpu"

    def _publish_health_locked(self) -> None:
        """Republish the published fields from the guarded state; caller holds `self._lock`."""
        held = self._held
        session = held.session if held is not None else None
        self._health.publish(
            device=held.device if held is not None else None,
            model_key=held.key if held is not None else None,
            variant=held.variant.value if held is not None else None,
            fp8_compute=held.fp8_applied.value if held is not None and held.fp8_applied is not None else None,
            compute_path=held.compute_path if held is not None else None,
            vram_step=session.steps_effective.vram.value if session is not None else None,
            ram_step=session.steps_effective.ram.value if session is not None else None,
            degraded=session.degraded if session is not None else False,
            last_error=self._last_error,
        )

    def unload(self) -> bool:
        """Drop every idle FLUX.3 handle (all models, formats, devices) from the manager; `False` when none held weights.

        A handle in another thread's session is skipped by the manager (this
        service's own runs are serialized by its lock).
        """
        with self._lock:
            dropped: list[str] = []
            for name in large_model.model_specs():
                dropped.extend(self._large.unload(name))
            self._processors.clear()
            self._held = None
            self._publish_health_locked()
        if dropped:
            log.info("FLUX.3 klein: выгружено: %s", ", ".join(dropped))
        return bool(dropped)

    # ---- generation ----
    def generate(
        self,
        request: Flux3Request,
        options: Flux3Options,
        *,
        observer: PhaseObserver | None = None,
    ) -> Flux3Result:
        """Generate one image for `request` with the package and run options `options`.

        Before the admission (refusals here cost no load): the package files and
        `config.json`, `normalize_request`, the VL_REFS-needs-references rule,
        the reference preparation (capped by `options.reference_max_pixels`
        when set), the output size, the device and the fp8 path. Then
        `manager.acquire` (refused with the shortest resource and the formats
        that would fit), the load scope (phases DIT_LOAD, AE_LOAD) and the run
        scope (DIT_OFFLOAD, ENCODER_LOAD, ENCODE, ENCODER_RELEASE, REF_ENCODE,
        DIT_RESTORE, DENOISE, DIT_OFFLOAD_DECODE, DECODE), every phase reported
        to `observer` exactly once (the mapping onto manager steps and the info
        keys are in this package's `MODULE_README.md`).

        # Raises
        `FileNotFoundError` for missing package files; `ValueError` for an
        invalid `config.json`, request or reference, VL_REFS without
        references, or a checkpoint that does not match the options;
        `RuntimeError` when an explicit `SCALED_MM` cannot run;
        `LargeModelRefused` when no plan fits; `LargeModelRunFailed` when a
        memory failure could not be recovered; whatever torch raises otherwise;
        whatever `observer` raises.
        """
        plan = self._plan(request, options)
        key = _model_key(options, plan.device)
        started = time.monotonic()
        log.info(
            "FLUX.3 klein: генерация %dx%d, %d шагов, %d референсов, %s/%s, шаги %s, на %s",
            plan.width,
            plan.height,
            plan.steps,
            len(plan.references.images),
            options.variant.value,
            options.encoder_mode.value,
            options.steps.label,
            plan.device,
        )
        # The admission BEFORE the lock: it may wait for evictions that take
        # other services' locks.
        try:
            session = self._large.acquire(plan.model, plan.model_options, plan.shape, device=plan.device, format=options.variant.value, steps=options.steps)
        except LargeModelError as exc:
            with self._lock:
                self._last_error = str(exc)
                self._publish_health_locked()
            log.warning("FLUX.3 klein: запуск не допущен (%s): %s", key, exc)
            raise
        with self._lock:
            held = _Held(
                session=session,
                key=session.key,
                device=plan.device,
                variant=options.variant,
                fp8_applied=plan.model_options.fp8_compute if options.variant is WeightFormat.FP8R else None,
                compute_path=large_model.compute_path(options.variant, plan.model_options.fp8_compute),
            )
            try:
                load_failed = True
                self._load_locked(session, plan, observer)
                load_failed = False
                result = self._generate_locked(session, plan, held, observer)
            except BaseException as exc:
                self._finish_failed_locked(session, plan, held, exc, load_failed=load_failed)
                raise
            session.finish()
            self._held = held
            self._last_error = None
            self._publish_health_locked()
        log.info(
            "FLUX.3 klein: готово %dx%d за %.1f с (текст %d токенов, шаги %s%s)",
            result.width,
            result.height,
            time.monotonic() - started,
            result.text_tokens,
            result.applied.steps_effective.label,
            ", облегчён" if result.applied.degraded else "",
        )
        return result

    def _finish_failed_locked(self, session: Session, plan: _Plan, held: _Held, exc: BaseException, *, load_failed: bool) -> None:
        """End a failed run: the session finishes with `exc`, a failed LOAD scope drops the model, the error is recorded and its memory released.

        A memory failure makes the manager drop every component itself; any
        other failure keeps the between-runs state, except in the load scope:
        a failed load registers nothing (the partial handle is unloaded, with
        any other idle handle of the same model). Caller holds `self._lock`.
        """
        try:
            session.finish(exc)
            memory = isinstance(exc, LargeModelRunFailed) or classify_failure(exc, step="finish") is not None
            if load_failed and not memory:
                dropped = self._large.unload(plan.model)
                log.info("FLUX.3 klein: загрузка не удалась, частично загруженная модель выгружена (%s)", ", ".join(dropped) or "-")
            self._held = held
        finally:
            self._last_error = str(exc)
            self._publish_health_locked()
            if isinstance(exc, Exception):
                log.exception("FLUX.3 klein: генерация не удалась (%s): %s", held.key, exc)
                _release_failed_run_memory(exc)

    @staticmethod
    def _placements(session: Session) -> dict[str, _Placement]:
        """Where every component of `session`'s handle is right now (the ledger, via `Session.placements`)."""
        found = session.placements()
        return {name: _placement_of(found.get(name)) for name in _COMPONENTS}

    def _resolve_package(self, options: Flux3Options) -> tuple[ModelFiles, PackageConfig]:
        """The variant's files and its validated `config.json` (stat calls + one small JSON read).

        # Raises
        `FileNotFoundError` / `ValueError` from `resolve_model_files` /
        `read_package_config` (a missing variant names its missing files).
        """
        files = resolve_model_files(options.model_root, options.variant)
        return files, read_package_config(files.dit_config)

    @staticmethod
    def _fp8_path(options: Flux3Options, device: str) -> Fp8Compute:
        """The concrete fp8r path on `device` (`quant`'s probe, cached per device).

        For an fp8r run this is `resolve_fp8_compute` (an explicit
        `SCALED_MM` that cannot run raises here, before any weight is read);
        for the other formats it only sizes the fp8r row of the forecast, so
        it never raises.
        """
        from . import quant

        if options.variant is WeightFormat.FP8R:
            return quant.resolve_fp8_compute(options.fp8_compute, device)
        if options.fp8_compute is Fp8Compute.DEQUANT:
            return Fp8Compute.DEQUANT
        return Fp8Compute.SCALED_MM if quant.probe_scaled_mm(device).supported else Fp8Compute.DEQUANT

    def _plan(self, request: Flux3Request, options: Flux3Options) -> _Plan:
        """Validate and shape the request; no lock, no admission, no weights.

        # Raises
        See `generate` (everything raised before the admission).
        """
        from . import autoencoder, packing

        files, package = self._resolve_package(options)
        normalized = normalize_request(request, package.sampler)
        if options.encoder_mode is EncoderMode.VL_REFS and not normalized.references:
            raise ValueError(
                "Режим энкодера с референсами (vl_refs) требует хотя бы одно изображение-референс; "
                "для генерации без референсов выберите текстовый режим."
            )
        references = packing.prepare_references(
            normalized.references, normalized.width, normalized.height, max_pixels=options.reference_max_pixels
        )
        default_width, default_height = packing.default_size(references.images, package.sampler.default_size)
        # Reference rule (`generate.py`): each missing side takes the default's.
        width = normalized.width or default_width
        height = normalized.height or default_height
        raw_device = options.device or _resolve_selected_backend_device(_DEVICE_FALLBACK)
        device = normalize_device(raw_device) or str(raw_device)
        fp8 = self._fp8_path(options, device)
        model_options = Flux3ModelOptions(options=options, device=device, fp8_compute=fp8)
        shape = large_model.request_shape(width, height, [image.size for image in references.images], caption=normalized.caption, steps=normalized.steps)
        ratio = autoencoder.DOWNSAMPLING_RATIO
        info = {
            "variant": options.variant.value,
            "encoder_mode": options.encoder_mode.value,
            "fp8_compute": options.fp8_compute.value,
            "ae_dtype": options.ae_dtype.value,
            "device": device,
            "reference_max_pixels": options.reference_max_pixels,
            "n_refs": len(references.images),
            "width": width,
            "height": height,
            "steps": normalized.steps,
            "seed": normalized.seed,
            "image_tokens": (width // ratio) * (height // ratio),
            "ref_tokens": sum((image.width // ratio) * (image.height // ratio) for image in references.images),
            **_steps_info(options.steps),
            "compute_path": large_model.compute_path(options.variant, fp8),
        }
        return _Plan(
            files=files,
            package=package,
            caption=normalized.caption,
            seed=normalized.seed,
            steps=normalized.steps,
            references=references,
            width=width,
            height=height,
            device=device,
            model=large_model.model_name(options.encoder_mode, options.ae_dtype),
            model_options=model_options,
            shape=shape,
            info=info,
        )

    # ---- steps ----
    def _step(self, observer: PhaseObserver | None, phase: Phase, session: Session, info: Mapping[str, Any], run: Callable[[], T], **extra: Any) -> T:
        """Run one manager step as observer `phase`.

        Begin info = `info` + the effective steps (`vram_step_effective`,
        `ram_step_effective`) + the forecast of the plan in force for this step
        (`_forecast_info`) + `extra`; the end event repeats it.
        """
        with observed(observer, phase, **info, **_steps_info(session.steps_effective, "_effective"), **_forecast_info(session, phase), **extra):
            return run()

    def _load_locked(self, session: Session, plan: _Plan, observer: PhaseObserver | None) -> None:
        """The load scope: DIT_LOAD, AE_LOAD.

        A component that holds nothing is read into its place for the first
        phase (`Session.load`, action `loaded`); one already held is left for
        DIT_OFFLOAD (action `resident` = nothing read). A component the plan
        keeps nowhere before its own phase is read by the step that needs it.
        Caller holds `self._lock`.
        """
        first = session.plan.phases[0]
        for phase, name in ((Phase.DIT_LOAD, COMPONENT_DIT), (Phase.AE_LOAD, COMPONENT_AE)):
            reads = not _holds(self._placements(session)[name]) and _holds(_target_of(first.components[name]))
            self._step(observer, phase, session, plan.info, (lambda name=name: session.load(name)) if reads else (lambda: None), action="loaded" if reads else "resident")

    def _transition_locked(self, observer: PhaseObserver | None, phase: Phase, session: Session, info: Mapping[str, Any], from_phase: str | None, to_phase: str, **extra: Any) -> None:
        """Observer `phase` = the manager transition `from_phase` -> `to_phase`."""
        self._step(observer, phase, session, info, lambda: session.transition(from_phase, to_phase), **extra)

    @staticmethod
    def _dit_out_action(before: _Placement, after: _Placement) -> str:
        """DIT_OFFLOAD / DIT_OFFLOAD_DECODE `action`: `offloaded` (leaves the device), `on_host` (stays in RAM), `skipped`."""
        if before[0] is Tier.DEVICE and after[0] is not Tier.DEVICE:
            return "offloaded"
        if before[0] is Tier.HOST and after[0] is Tier.HOST:
            return "on_host"
        return "skipped"

    def _generate_locked(self, session: Session, plan: _Plan, held: _Held, observer: PhaseObserver | None) -> Flux3Result:
        """The run scope, one manager step per observer phase (see `generate`); caller holds `self._lock`."""
        import torch

        from . import autoencoder, packing, sampling, text_encoder

        dit_config = plan.package.dit
        rope_scale = dit_config.image_rope_scale
        options = plan.model_options.options

        # DIT_OFFLOAD: DiT and AE from the between-runs layout to the encode placement.
        # (`Session.load` reconciles only the named components; nothing is read
        # for a component the encode placement keeps nowhere.)
        encode_state = session.plan.phase(PHASE_ENCODE)
        dit_before = self._placements(session)[COMPONENT_DIT]
        dit_after = _target_of(encode_state.components[COMPONENT_DIT]) if _holds(dit_before) else dit_before
        self._step(observer, Phase.DIT_OFFLOAD, session, plan.info, lambda: session.load(COMPONENT_DIT, COMPONENT_AE), action=self._dit_out_action(dit_before, dit_after))

        # ENCODER_LOAD: the transition into the encode phase brings the encoder.
        before = self._placements(session)[COMPONENT_ENCODER]
        target = _target_of(encode_state.components[COMPONENT_ENCODER])
        if not _holds(before) or before[1] is not target[1]:
            encoder_action = "loaded"
        elif before[0] is Tier.HOST and target[0] is Tier.DEVICE:
            encoder_action = "restored"
        else:
            encoder_action = "resident"

        def enter_encode() -> None:
            self._processor_locked(plan)
            session.transition(None, PHASE_ENCODE)

        self._step(observer, Phase.ENCODER_LOAD, session, plan.info, enter_encode, action=encoder_action)

        processor = self._processor_locked(plan)

        def encode(ctx: PhaseContext) -> Any:
            handle = text_encoder.TextEncoderHandle(
                model=ctx.module(COMPONENT_ENCODER),
                processor=processor,
                mode=options.encoder_mode,
                device=torch.device(ctx.exec_device(COMPONENT_ENCODER)),
                keep_layers=ENCODER_KEEP_LAYERS,
            )
            if options.encoder_mode is EncoderMode.VL_REFS:
                return text_encoder.encode_with_references(handle, plan.caption, plan.references.images, vl_ref_max_pixels=options.vl_ref_max_pixels)
            return text_encoder.encode_text(handle, plan.caption)

        encoded = self._step(observer, Phase.ENCODE, session, plan.info, lambda: session.run_phase(PHASE_ENCODE, encode))
        info = {**plan.info, "text_tokens": int(encoded.tokens.shape[0]), "text_real_tokens": int(encoded.real_length)}

        self._transition_locked(observer, Phase.ENCODER_RELEASE, session, info, PHASE_ENCODE, PHASE_REF_ENCODE)

        def ref_encode(ctx: PhaseContext) -> Any:
            ae = ctx.module(COMPONENT_AE)
            device = ctx.exec_device(COMPONENT_AE)
            # Reference ids get the l axis -(1 + i) (`packing.reference_tokens`).
            return packing.reference_tokens([ae.encode(packing.to_tensor(image).to(device)) for image in plan.references.images], rope_scale)

        cond = self._step(observer, Phase.REF_ENCODE, session, info, lambda: session.run_phase(PHASE_REF_ENCODE, ref_encode))

        dit_before = self._placements(session)[COMPONENT_DIT]
        restore = "resident" if dit_before[0] is Tier.DEVICE else "restored"
        self._transition_locked(observer, Phase.DIT_RESTORE, session, info, PHASE_REF_ENCODE, PHASE_DENOISE, action=restore)

        def denoise(ctx: PhaseContext) -> Any:
            device = ctx.exec_device(COMPONENT_DIT)
            latent_h, latent_w = autoencoder.latent_shape(plan.height, plan.width, autoencoder.DOWNSAMPLING_RATIO)
            noise = packing.initial_noise(latent_h, latent_w, plan.seed, dit_config.in_channels["image"]).to(device)
            image_ids = autoencoder.latent_ids(latent_h, latent_w, device)
            text = (encoded.tokens.to(device), encoded.ids.to(device))
            packed = packing.pack([(text, cond, (noise, packing.scaled_ids(image_ids, rope_scale)))], dit_config.in_channels)
            schedule = sampling.flow_map_schedule(plan.steps, plan.package.sampler.alpha)
            tokens = sampling.denoise_flow_map(ctx.module(COMPONENT_DIT), packed, schedule, device_type=torch.device(device).type)
            return tokens, image_ids

        tokens, image_ids = self._step(observer, Phase.DENOISE, session, info, lambda: session.run_phase(PHASE_DENOISE, denoise))

        dit_before = self._placements(session)[COMPONENT_DIT]
        dit_after = _target_of(session.plan.phase(PHASE_DECODE).components[COMPONENT_DIT])
        self._transition_locked(observer, Phase.DIT_OFFLOAD_DECODE, session, info, PHASE_DENOISE, PHASE_DECODE, action=self._dit_out_action(dit_before, dit_after))

        def decode(ctx: PhaseContext) -> Any:
            ae = ctx.module(COMPONENT_AE)
            image = packing.to_pil(ae.decode(tokens.to(ctx.exec_device(COMPONENT_AE)), image_ids.to(ctx.exec_device(COMPONENT_AE))))
            if plan.references.crop_box is not None:
                image = image.crop(plan.references.crop_box)
            return image

        image = self._step(observer, Phase.DECODE, session, info, lambda: session.run_phase(PHASE_DECODE, decode))

        return Flux3Result(
            image=image,
            seed=plan.seed,
            width=image.width,
            height=image.height,
            steps=plan.steps,
            text_tokens=info["text_tokens"],
            text_real_tokens=info["text_real_tokens"],
            applied=Flux3Applied(
                variant=options.variant,
                fp8_compute=held.fp8_applied,
                encoder_mode=options.encoder_mode,
                ae_dtype=options.ae_dtype,
                compute_path=held.compute_path,
                steps_requested=session.steps_requested,
                steps_effective=session.steps_effective,
                degraded=session.degraded,
            ),
        )

    def _processor_locked(self, plan: _Plan) -> Any:
        """The text processor of the package's encoder directory (loaded once, kept until `unload`); caller holds `self._lock`."""
        from . import text_encoder

        key = str(plan.files.text_encoder_dir)
        processor = self._processors.get(key)
        if processor is None:
            processor = text_encoder.load_text_processor(plan.files.text_encoder_dir)
            self._processors[key] = processor
        return processor
