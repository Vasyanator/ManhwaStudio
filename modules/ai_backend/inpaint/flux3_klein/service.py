"""
File: modules/ai_backend/inpaint/flux3_klein/service.py

Purpose:
`Flux3KleinService` - one FLUX.3 Image Klein generation (caption + up to ten
reference images -> image) driven through the large-model manager
(`runtime.large_models.manager.LargeModelManager`) over TWO handles — the
pipeline (DiT + AE) and the text encoder — with every phase reported to an
optional `PhaseObserver`.

Main responsibilities:
- `generate` - validation, reference preparation and the fp8 path BEFORE the
  admissions, then, under the service lock, the caption's prompt plan (TEXT
  mode: memory or the library may serve it, `_prompt_locked`; a served caption
  never opens the encoder handle), and only when the run encodes the
  encoder's combined VRAM-axis ceiling
  (`runtime.large_models.paired.live_ceilings` over `large_model.FLUX3_PAIR`),
  the pipeline admission, the encoder admission beside it (the two resource steps «Видеопамять» x «Оперативная память» pick
  where every byte lives; the weight formats are the user's and never change),
  the load scope (DiT, AE) and the run scope (encode on the encoder handle;
  reference AE encode, flow-map denoise, decode on the pipeline) as manager
  steps (`Session.load` / `transition` / `run_phase` / `finish`), each
  bracketed by its observer `Phase`; an optional disk-read sink
  (`load_progress`) becomes both sessions' `set_load_progress` for the run;
- `health` / `unload` - the service contract every backend service has
  (`inpaint/MODULE_README.md`, health snapshot); where each component is comes
  from the manager's ledger (`LargeModelManager.placements` of both handle
  keys), never from the service's own bookkeeping;
- `large_model_profiles` - the `large_models.profiles` adapter: the two-handle
  composition `paired.plan_pair` (`plan_profiles` / `plan_pair` return its
  `PairedReport`, the calibration tool's run forecast), the encoder counted
  per `_encoder_need`;
- `status`, all over the one wire mapping `wire.normalize_flux3_params`
  (`.status` through `normalize_flux3_status_params`: same rules, a missing
  `source_root` is the unavailable answer, never a skipped validation), with
  the prompt-cache keys `prompt_cached` / `prompt_cache_entry`;
- the prompt cache (TEXT mode only, D11): the in-memory embeddings
  (`runtime.prompt_cache_service.EmbeddingMemory`, count- and byte-capped) and
  the library `prompt_cache/flux3_klein/` through the shared
  `PromptCacheOrchestrator` over `prompt_cache.Flux3PromptCacheAdapter`;
  `prompt_cache_build` («Кэшировать», the encoder handle ALONE) and the
  browse-only `prompt_cache_list` / `_export` / `_import`;
- cancellation: an observer or a step callback raises `Flux3Canceled`; it is
  never a memory failure (no degrade retry), the run ends with the weights
  wherever the manager's `finish` leaves them, and nothing is unloaded for it.

Key declarations:
- `Flux3KleinService`, `Flux3Result`, `Flux3Applied`, `Flux3Canceled`
- `_resolve_selected_backend_device`, `_clear_torch_cache` (patch points)

Notes:
- Callers: `region.py` (the cleaning tab's region editor, IPC
  `inpaint.flux3_klein`, `ipc/handlers/flux3_klein.py`), the
  `large_models.profiles` adapter (`server.py` `large_model_adapters`) and the
  calibration tool (`tools/large_model_calibration/`, its `flux3` driver).
- Lock order (the stated exception of the manager's rule "acquire before the
  service lock", `LargeModelManager.acquire`): both sessions are acquired
  UNDER the service lock, the pipeline before the encoder. FLUX.3 holds no
  small-pool lease and registers no reclaim callback that takes this lock (the
  large pool's reclaim only TRIES a handle's run lock), and one run opens two
  handles; under the lock the order is total: this lock -> pipeline ->
  encoder. Pinned by `test_service.LockOrderTests`.
- `_model_key` / `_encoder_handle_key` - the two residency keys, from the
  options alone (`LargeModelSpec.key`).
- The user's rule for a served caption (TEXT mode, memory or library) while
  the encoder handle holds no byte: the encoder handle is never acquired — its
  files are not read (they may be absent on disk), it is not placed, and the
  forecast does not count it (`_encoder_need`, `large_model_profiles`
  `encoder: null`). A resident encoder over a served caption is counted by the
  forecast but not touched by the run. `vl_refs` never reads or writes the
  library or the memory: the encoder sees the references (D11).
- The caption is served BEFORE the admissions (a library read is a few MB; a
  file that vanished since the plan re-plans once, then encodes or is
  refused), so whether the run opens the encoder handle is decided once.
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
from pathlib import Path
from typing import TYPE_CHECKING, Any, TypeVar

from ...runtime import prompt_library as pl
from ...runtime.health_snapshot import HealthSnapshot
from ...runtime.large_models import paired
from ...runtime.large_models.degrade import classify_failure
from ...runtime.large_models.loader import LoadProgressSink
from ...runtime.large_models.manager import ComponentPlacement, LargeModelError, LargeModelManager, LargeModelRunFailed, PhaseContext, Session
from ...runtime.large_models.paired import EncoderNeed, HandleRequest
from ...runtime.large_models.spec import ExecDevice, RequestShape
from ...runtime.large_models.tiers import ComponentPlan, IDLE_PHASE, Steps, Tier, VramStep
from ...runtime.prompt_cache_service import EmbeddingMemory, PromptCacheOrchestrator, entry_info, require_prompt_to_cache
from ...runtime.residency.footprint import normalize_device
from ...runtime.torch_device import clear_torch_cache, resolve_discrete_backend_device
from . import large_model
from .config import ENCODER_BF16_SOURCE, DitSource, GgufDit, ModelFiles, PackageConfig, WeightFormat, package_text_encoder_dir, read_package_config, resolve_model_files, text_encoder_availability
from .large_model import (
    COMPONENT_AE,
    COMPONENT_DIT,
    COMPONENT_ENCODER,
    ENCODER_KEEP_LAYERS,
    FLUX3_PAIR,
    PHASE_DECODE,
    PHASE_DENOISE,
    PHASE_ENCODE,
    PHASE_REF_ENCODE,
    Flux3ModelOptions,
)
from .params import MIN_SIDE, AeDtype, EncoderMode, Flux3Options, Flux3Request, Fp8Compute, normalize_request
from .phases import Phase, PhaseObserver, observed
from .prompt_cache import (
    MAX_SEQUENCE_LENGTH,
    PROMPT_EMBED_CACHE_BYTES,
    PROMPT_EMBED_CACHE_ENTRIES,
    Flux3CacheQuery,
    Flux3PromptCacheAdapter,
    Flux3PromptCacheRequest,
    encoder_available,
    encoder_provenance,
    text_width,
)
from .wire import normalize_flux3_library_params, normalize_flux3_params, normalize_flux3_status_params

if TYPE_CHECKING:
    from PIL.Image import Image

    from .packing import PreparedReferences

log = logging.getLogger(__name__)

T = TypeVar("T")

#: Display name in log lines and the two-handle rule's log records.
_OWNER = "FLUX.3 klein"

#: `General.ai_device` resolution with this service's policy (owner:
#: `runtime.torch_device`; the FLUX.2 klein policy: the user's choice is
#: honoured, never an integrated GPU). Patched by name in the tests.
_resolve_selected_backend_device = resolve_discrete_backend_device

#: Allocator-cache release with this service's policy (owner:
#: `runtime.torch_device`; CUDA cache only). Patched by name in the tests.
_clear_torch_cache = functools.partial(clear_torch_cache, owner=_OWNER, include_mps=False)

#: Fallback handed to the device resolution (the FLUX.2 klein binding's value).
_DEVICE_FALLBACK = "cuda"

#: A component's residency as the observer actions and `health()` read it:
#: (`DEVICE` / `HOST` / `DROPPED` = not held, where it executes; `None` = never ran).
_Placement = tuple[Tier, ExecDevice | None]

_NOT_HELD: _Placement = (Tier.DROPPED, None)

#: The components of each handle, in report order.
_PIPELINE_COMPONENTS = (COMPONENT_DIT, COMPONENT_AE)
_ENCODER_COMPONENTS = (COMPONENT_ENCODER,)

#: Which handle an observer phase's step belongs to (its effective steps and forecast).
_PIPELINE = "pipeline"
_ENCODER = "encoder"
_ENCODER_PHASES = frozenset({Phase.ENCODER_LOAD, Phase.ENCODE, Phase.ENCODER_RELEASE})


@dataclass(frozen=True)
class Flux3Applied:
    """What a generation actually ran with.

    `variant` is the DiT format that ran (a native `WeightFormat` or a
    `GgufDit`; `.value` its id). `fp8_compute` is the concrete fp8 path of an
    fp8r DiT (`SCALED_MM` or `DEQUANT`; `None` for every other format, which
    has no fp8 layer); `compute_path` how the format's matmuls ran (`native`,
    `scaled_mm`, `dequant`, `gguf`); `steps_requested` the user's pair (may hold «Авто» on either
    axis), `steps_effective` the concrete pair the PIPELINE handle ended on
    (Auto resolved, a learned floor applied, a degrade included; the encoder
    handle is planned with the same request beside it); `degraded` whether
    the manager had to lower a step (or take another rung) after a memory
    failure on either handle, `degrade_reason` the failure kind it degraded
    for (the pipeline's, else the encoder's; `None` when neither did). The
    weight formats are never changed by the manager.
    """

    variant: DitSource
    fp8_compute: Fp8Compute | None
    encoder_mode: EncoderMode
    ae_dtype: AeDtype
    compute_path: str
    steps_requested: Steps
    steps_effective: Steps
    degraded: bool
    degrade_reason: str | None = None


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
    """Everything decided before the admissions: files, config, request, references, size, device, both handles' manager inputs.

    `pipeline` / `encoder` are the two handles' `paired.HandleRequest`s (model,
    `model_options`, format: the DiT format id / the encoder format id).
    """

    files: ModelFiles
    package: PackageConfig
    caption: str
    seed: int
    steps: int
    references: PreparedReferences
    width: int
    height: int
    device: str
    model_options: Flux3ModelOptions
    pipeline: HandleRequest
    encoder: HandleRequest
    shape: RequestShape
    info: Mapping[str, Any]


@dataclass
class _Held:
    """The last run's handles as the service knows them: the (finished) pipeline session, both keys and the run's facts.

    Where the components are is the manager's (`health()` reads the ledger of
    `pipeline_key` and `encoder_key`). `encoder_key` is computed from the
    options, whether or not the run opened the encoder handle.
    """

    session: Session
    pipeline_key: str
    encoder_key: str
    device: str
    variant: DitSource
    fp8_applied: Fp8Compute | None
    compute_path: str


@dataclass
class _Sessions:
    """The open sessions of one run: the pipeline's, and the encoder's (`None` when the run opened none)."""

    pipeline: Session
    encoder: Session | None

    def of(self, phase: Phase) -> Session:
        """The session a step of `phase` belongs to (the pipeline's when the run has no encoder session)."""
        if phase in _ENCODER_PHASES and self.encoder is not None:
            return self.encoder
        return self.pipeline


@dataclass(frozen=True)
class _Prompt:
    """How one run gets its caption's DiT text tokens (decided under the lock, before the admissions).

    `served`: the host tensor `[L, width]` bf16 memory or the library gave
    (`None` = the run encodes); `provenance`: the encoder identity a TEXT
    encode is remembered under (computed before any load; `None` for a served
    caption, in `vl_refs` mode and for an unidentifiable encoder: never
    remembered); `library`: the
    request's library id (`""` = none).
    """

    served: Any | None
    provenance: pl.EncoderProvenance | None
    library: str

    @property
    def encodes(self) -> bool:
        """Whether the run opens the encoder handle and encodes the caption."""
        return self.served is None


@functools.lru_cache(maxsize=None)
def _pipeline_spec_of(ae_dtype: AeDtype) -> Any:
    """The pipeline `LargeModelSpec` of `ae_dtype`, built once (its residency key)."""
    return large_model.pipeline_spec(ae_dtype)


@functools.lru_cache(maxsize=None)
def _encoder_spec_of(encoder_mode: EncoderMode) -> Any:
    """The text-encoder `LargeModelSpec` of `encoder_mode`, built once (its residency key)."""
    return large_model.encoder_spec(encoder_mode)


def _placeholder_options(options: Flux3Options, device: str) -> Flux3ModelOptions:
    """Model options for a key computation: the keys read `options` alone (the requested fp8 path), so the concrete path never changes them."""
    return Flux3ModelOptions(options=options, device=device, fp8_compute=Fp8Compute.DEQUANT)


def _model_key(options: Flux3Options, device: str) -> str:
    """The pipeline handle's residency key of `options` on `device` (`LargeModelSpec.key`: `large_model.pipeline_key`)."""
    return _pipeline_spec_of(options.ae_dtype).key(_placeholder_options(options, device), options.variant.value, device)


def _encoder_handle_key(options: Flux3Options, device: str) -> str:
    """The text-encoder handle's residency key of `options` on `device` (`large_model.encoder_key` + the host pin)."""
    return _encoder_spec_of(options.encoder_mode).key(_placeholder_options(options, device), options.encoder.id, device)


#: A denoise progress sink: `(step, total)` after every flow-map step (1-based).
StepCallback = Callable[[int, int], None]


class Flux3Canceled(Exception):
    """The caller canceled a generation (raised by its observer or step callback).

    Not a memory failure: the manager re-raises it unchanged (no degrade
    rung), `generate` finishes the sessions with it and keeps whatever the
    manager's `finish` left (no unload), and it is not recorded as the
    service's last error.
    """


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


#: The forecast entry each observer phase is measured against: `(handle,
#: kind, name)` = a model phase or the transition the step performs
#: (`"<from>-><to>"`, `idle` = before the first phase) of that handle's
#: forecast; `None` = no forecast entry (ENCODER_RELEASE ends the encoder
#: session, which `finish` performs outside any forecast step). The load scope
#: and DIT_OFFLOAD are parts of the pipeline's first transition (into
#: `ref_encode`, its first phase).
_FORECAST_STEP: Mapping[Phase, tuple[str, str, str] | None] = {
    Phase.DIT_LOAD: (_PIPELINE, "transition", f"{IDLE_PHASE}->{PHASE_REF_ENCODE}"),
    Phase.AE_LOAD: (_PIPELINE, "transition", f"{IDLE_PHASE}->{PHASE_REF_ENCODE}"),
    Phase.DIT_OFFLOAD: (_PIPELINE, "transition", f"{IDLE_PHASE}->{PHASE_REF_ENCODE}"),
    Phase.ENCODER_LOAD: (_ENCODER, "transition", f"{IDLE_PHASE}->{PHASE_ENCODE}"),
    Phase.ENCODE: (_ENCODER, "phase", PHASE_ENCODE),
    Phase.ENCODER_RELEASE: None,
    Phase.REF_ENCODE: (_PIPELINE, "phase", PHASE_REF_ENCODE),
    Phase.DIT_RESTORE: (_PIPELINE, "transition", f"{PHASE_REF_ENCODE}->{PHASE_DENOISE}"),
    Phase.DENOISE: (_PIPELINE, "phase", PHASE_DENOISE),
    Phase.DIT_OFFLOAD_DECODE: (_PIPELINE, "transition", f"{PHASE_DENOISE}->{PHASE_DECODE}"),
    Phase.DECODE: (_PIPELINE, "phase", PHASE_DECODE),
}


def _forecast_info(sessions: _Sessions, phase: Phase) -> dict[str, Any]:
    """Observer info: the forecast of `phase`'s step and of the whole run, from the plans in force (bytes; host = absolute RSS).

    The run's device peak is the two-handle rule's (`paired.combined_device_peak`
    over `FLUX3_PAIR`: the pipeline's phases and the encode moment beside its
    `ref_encode` weights); its host peak is the pipeline's plus the encoder's
    growth above the baseline RSS (counted once). A step without an entry
    (ENCODER_RELEASE, or an encoder step of a run without an encoder session)
    reports `None` for its own two keys.
    """
    pipeline = sessions.pipeline.forecast
    encoder = sessions.encoder.forecast if sessions.encoder is not None else None
    target = _FORECAST_STEP[phase]
    entry = None
    if target is not None:
        handle, kind, name = target
        forecast = pipeline if handle == _PIPELINE else encoder
        if forecast is not None:
            entry = (forecast.phases if kind == "phase" else forecast.transitions).get(name)
    host = int(pipeline.host_peak)
    if encoder is not None:
        host += max(0, int(encoder.host_peak) - paired.host_baseline(encoder))
    return {
        "forecast_device_peak": entry.device_peak if entry is not None else None,
        "forecast_host_peak": entry.host_peak if entry is not None else None,
        "forecast_run_device_peak": paired.combined_device_peak(pipeline, encoder, FLUX3_PAIR),
        "forecast_run_host_peak": host,
        "forecast_calibrated": bool(pipeline.calibrated and (encoder is None or encoder.calibrated)),
    }


class Flux3KleinService:
    """FLUX.3 klein generator over the large-model manager.

    Registers the FLUX.3 models (`large_model.model_specs`: a pipeline per AE
    dtype, a text encoder per encoder mode) with `large_models` once. Between
    runs the weights stay wherever the run's step pair leaves them (the card
    only under «Максимум скорости», RAM only under «Держать модели в памяти»),
    owned by the manager (an idle handle may be parked or dropped there for
    another model). One `RLock` serializes this service's runs and is taken
    BEFORE both admissions (module notes); `health()` never takes it.
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
        # TEXT-mode caption embeddings in memory (host tensors), count- and
        # byte-capped LRU, mutated only under `_lock`; its provenance index is
        # copy-on-write, so the lock-free plan (`.status`, the forecast) reads
        # it during a run. Kept by `unload` (megabytes, not weights).
        self._embeddings = EmbeddingMemory(PROMPT_EMBED_CACHE_ENTRIES, PROMPT_EMBED_CACHE_BYTES)
        self._prompts = PromptCacheOrchestrator(Flux3PromptCacheAdapter(), self._embeddings)
        # Replaced (never mutated) so `health()` reads it without the lock.
        self._held: _Held | None = None
        self._last_error: str | None = None
        self._health = HealthSnapshot(device=None, model_key=None, variant=None, fp8_compute=None, compute_path=None, vram_step=None, ram_step=None, degraded=False, last_error=None)

    # ---- health / unload ----
    def health(self) -> dict[str, Any]:
        """The health fields; answers while a run holds the lock (never takes it).

        Published at the end of every run / unload: `device`, `model_key` (the
        pipeline handle's key), `variant`, `fp8_compute` (applied fp8 path or
        `None`), `compute_path`, `vram_step` / `ram_step` (the pipeline's
        effective pair of the last run), `degraded`, `last_error`. Live, from
        the manager's ledger of BOTH handle keys (`LargeModelManager.placements`:
        no probe, never waits for a session; the manager may park or drop an
        idle handle at any time): `loaded` (any weights of either handle
        held), `ready` (DiT + AE held), `dit_device` (the run device while any
        DiT block is there, `cpu` while it is parked in RAM, `None` when not
        held), `encoder_resident` (likewise for the text encoder handle; `cpu`
        also when it runs on the CPU).
        """
        fields = self._health.read()
        held = self._held
        placements: dict[str, ComponentPlacement] = {}
        if held is not None:
            placements.update(self._large.placements(held.pipeline_key))
            placements.update(self._large.placements(held.encoder_key))
        locations = {name: self._location(held, placements.get(name)) for name in (*_PIPELINE_COMPONENTS, *_ENCODER_COMPONENTS)}
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
            model_key=held.pipeline_key if held is not None else None,
            variant=held.variant.value if held is not None else None,
            fp8_compute=held.fp8_applied.value if held is not None and held.fp8_applied is not None else None,
            compute_path=held.compute_path if held is not None else None,
            vram_step=session.steps_effective.vram.value if session is not None else None,
            ram_step=session.steps_effective.ram.value if session is not None else None,
            degraded=session.degraded if session is not None else False,
            last_error=self._last_error,
        )

    def unload(self) -> bool:
        """Drop every idle FLUX.3 handle (all pipeline and encoder models, formats, devices) from the manager; `False` when none held weights.

        A handle in another thread's session is skipped by the manager (this
        service's own runs are serialized by its lock). The in-memory prompt
        embeddings are KEPT (megabytes); the cached text processors are dropped.
        """
        with self._lock:
            dropped: list[str] = []
            for name in large_model.model_specs():
                dropped.extend(self._large.unload(name))
            self._processors.clear()
            self._held = None
            self._publish_health_locked()
        if dropped:
            log.info("%s: выгружено: %s", _OWNER, ", ".join(dropped))
        return bool(dropped)

    # ---- generation ----
    def generate(
        self,
        request: Flux3Request,
        options: Flux3Options,
        *,
        observer: PhaseObserver | None = None,
        step_callback: StepCallback | None = None,
        load_progress: LoadProgressSink | None = None,
        prompt_cache: Flux3PromptCacheRequest | None = None,
    ) -> Flux3Result:
        """Generate one image for `request` with the package and run options `options`.

        `prompt_cache` names the request's prompt library and precision order
        (`None` = no library: the memory cache alone). In TEXT mode a caption
        served by memory or the library is not encoded and, while the encoder
        handle holds no byte, the encoder handle is never acquired (its weights
        may be absent); the encoder phases are still reported once each,
        ENCODER_LOAD with `action` `cached` and no forecast. A fresh TEXT
        encode is remembered in memory. `vl_refs` always encodes and never
        touches the cache.

        `step_callback(step, total)` is called after every denoise step; it may
        repeat steps when the manager retries the denoise on a gentler plan.
        Exceptions of `observer` and `step_callback` propagate (raise
        `Flux3Canceled` to cancel: see the class).

        `load_progress` becomes the disk-read sink of both sessions
        (`Session.set_load_progress`) for the whole run: every checkpoint read
        — a fresh load of the DiT / AE / encoder, a reload after a degrade
        retry, a DISK / DROPPED re-read (`reread=True`) — arrives as
        `loader.LoadProgress` frames on this thread, inside the observer phase
        whose step performs the read. Rate limiting is the manager's; a sink
        that raises is logged and ignored, so it cannot cancel a read.

        Before the admissions (refusals here cost no load): the package files and
        `config.json`, a GGUF DiT's header (`_require_gguf_dit`), `normalize_request`, the VL_REFS-needs-references rule,
        the reference preparation (capped by `options.reference_max_pixels`
        when set), the output size, the device and the fp8 path. Then, under
        the service lock: the prompt plan (`_prompt_locked`: a served caption,
        else the encoder's weights and a GGUF encoder's header are required),
        and when the run encodes the encoder's combined VRAM-axis ceiling
        (planned before the pipeline holds a reservation); the pipeline
        admission, and when the run encodes the encoder admission beside it
        (each refused with the shortest resource and the formats that would
        fit; an encoder refusal finishes the pipeline session with nothing
        read), the load scope (phases DIT_LOAD,
        AE_LOAD) and the run scope (DIT_OFFLOAD, ENCODER_LOAD, ENCODE,
        ENCODER_RELEASE, REF_ENCODE, DIT_RESTORE, DENOISE, DIT_OFFLOAD_DECODE,
        DECODE), every phase reported to `observer` exactly once (the mapping
        onto manager steps and the info keys are in this package's
        `MODULE_README.md`).

        # Raises
        `FileNotFoundError` for missing package files, or missing encoder
        weights when the run must encode; `ValueError` for an
        invalid `config.json`, request or reference, VL_REFS without
        references, a chosen cache file that vanished while no encoder can
        replace it, or a checkpoint that does not match the options;
        `RuntimeError` when an explicit `SCALED_MM` cannot run;
        `LargeModelRefused` when no plan fits either handle;
        `LargeModelRunFailed` when a memory failure could not be recovered;
        whatever torch raises otherwise; whatever `observer` raises.
        """
        plan = self._plan(request, options)
        started = time.monotonic()
        log.info(
            "%s: генерация %dx%d, %d шагов, %d референсов, %s/%s, шаги %s, на %s",
            _OWNER,
            plan.width,
            plan.height,
            plan.steps,
            len(plan.references.images),
            options.variant.value,
            options.encoder_mode.value,
            options.steps.label,
            plan.device,
        )
        with self._lock:
            prompt = self._prompt_locked(plan, prompt_cache or Flux3PromptCacheRequest())
            sessions = self._admit_locked(plan, encodes=prompt.encodes)
            held = _Held(
                session=sessions.pipeline,
                pipeline_key=sessions.pipeline.key,
                encoder_key=sessions.encoder.key if sessions.encoder is not None else _encoder_handle_key(options, plan.device),
                device=plan.device,
                variant=options.variant,
                fp8_applied=plan.model_options.fp8_compute if options.variant is WeightFormat.FP8R else None,
                compute_path=large_model.compute_path(options.variant, plan.model_options.fp8_compute),
            )
            # Registered before the first step, so the load scope's reads are seen too.
            sessions.pipeline.set_load_progress(load_progress)
            if sessions.encoder is not None:
                sessions.encoder.set_load_progress(load_progress)
            try:
                load_failed = True
                self._load_locked(sessions, plan, observer)
                load_failed = False
                result = self._generate_locked(sessions, plan, held, observer, step_callback, prompt)
            except BaseException as exc:
                self._finish_failed_locked(sessions, held, exc, load_failed=load_failed)
                raise
            sessions.pipeline.finish()
            self._held = held
            self._last_error = None
            self._publish_health_locked()
        log.info(
            "%s: готово %dx%d за %.1f с (текст %d токенов, шаги %s%s)",
            _OWNER,
            result.width,
            result.height,
            time.monotonic() - started,
            result.text_tokens,
            result.applied.steps_effective.label,
            ", облегчён" if result.applied.degraded else "",
        )
        return result

    def _admit_locked(self, plan: _Plan, *, encodes: bool) -> _Sessions:
        """The admissions of one run, pipeline before encoder (module notes); caller holds `self._lock`.

        `encodes` is the prompt decision (`_Prompt.encodes`): only then is the
        encoder handle admitted, its combined VRAM-axis ceiling planned FIRST,
        while this run's pipeline holds no reservation
        (`paired.live_ceilings`). A refusal of either admission records
        `last_error`, touches no weight (a pipeline already admitted is
        finished with nothing read) and is re-raised.

        # Raises
        `LargeModelError` (`LargeModelRefused`) from either admission;
        `ValueError` from the manager for an invalid request.
        """
        try:
            ceilings = None
            if encodes:
                alone = self._large.profiles(plan.pipeline.model, plan.pipeline.options, plan.shape, device=plan.device, format=plan.pipeline.format, steps=plan.model_options.options.steps)
                ceilings = paired.live_ceilings(self._large, alone, plan.encoder, plan.shape, plan.device, FLUX3_PAIR, until=VramStep.MIN)
            pipeline = self._acquire(plan.pipeline, plan)
        except LargeModelError as exc:
            self._refused_locked(plan.pipeline.model, exc)
            raise
        if not encodes:
            return _Sessions(pipeline, None)
        try:
            encoder = self._acquire(plan.encoder, plan, device_cap_bytes=paired.encoder_cap_beside(ceilings, pipeline, FLUX3_PAIR, owner=_OWNER))
        except BaseException as exc:
            # Nothing was read for the pipeline: its finish only releases the
            # reservation, the run lock and the lease.
            pipeline.finish(exc)
            if isinstance(exc, LargeModelError):
                self._refused_locked(plan.encoder.model, exc)
            raise
        return _Sessions(pipeline, encoder)

    def _acquire(self, handle: HandleRequest, plan: _Plan, *, device_cap_bytes: int | None = None) -> Session:
        """Admit one session of `handle` with the request's steps (call under `self._lock`, pipeline before encoder)."""
        return self._large.acquire(handle.model, handle.options, plan.shape, device=plan.device, format=handle.format, steps=plan.model_options.options.steps, device_cap_bytes=device_cap_bytes)

    def _refused_locked(self, model: str, exc: LargeModelError) -> None:
        """Record an admission refusal of `model` as the last error (logged); caller holds `self._lock`."""
        self._last_error = str(exc)
        self._publish_health_locked()
        log.warning("%s: запуск не допущен (%s): %s", _OWNER, model, exc)

    def _finish_failed_locked(self, sessions: _Sessions, held: _Held, exc: BaseException, *, load_failed: bool) -> None:
        """End a failed run: both sessions finish with `exc`, a failed LOAD scope drops the run's pipeline handle, the error is recorded and its memory released.

        A memory failure makes the manager drop every component of the
        session it hit; any other failure keeps the between-runs state, except
        in the load scope: a failed load registers nothing (the partial
        pipeline handle `held.pipeline_key` is unloaded; every other handle is
        left alone). Caller holds `self._lock`.
        """
        canceled = isinstance(exc, Flux3Canceled)
        try:
            # The encoder first: it was admitted last (finish is idempotent; the
            # run may have finished it at ENCODER_RELEASE already).
            if sessions.encoder is not None:
                sessions.encoder.finish(exc)
            sessions.pipeline.finish(exc)
            memory = isinstance(exc, LargeModelRunFailed) or classify_failure(exc, step="finish") is not None
            if load_failed and not memory and not canceled:
                dropped = self._large.unload_key(held.pipeline_key)
                log.info("%s: загрузка не удалась, частично загруженный пайплайн выгружен (%s)", _OWNER, held.pipeline_key if dropped else "-")
            self._held = held
        finally:
            if canceled:
                # A cancel is the user's choice, not a failure of the service.
                self._publish_health_locked()
                log.info("%s: генерация отменена (%s): %s", _OWNER, held.pipeline_key, exc)
                _release_failed_run_memory(exc)
            else:
                self._last_error = str(exc)
                self._publish_health_locked()
                if isinstance(exc, Exception):
                    log.exception("%s: генерация не удалась (%s): %s", _OWNER, held.pipeline_key, exc)
                    _release_failed_run_memory(exc)

    @staticmethod
    def _placements(session: Session, components: tuple[str, ...]) -> dict[str, _Placement]:
        """Where `components` of `session`'s handle are right now (the ledger, via `Session.placements`)."""
        found = session.placements()
        return {name: _placement_of(found.get(name)) for name in components}

    def _holds_key(self, key: str) -> bool:
        """Whether the handle `key` holds any byte anywhere (the ledger; never waits for a session)."""
        return any(_holds(_placement_of(placement)) for placement in self._large.placements(key).values())

    def _encoder_need(self, model_options: Flux3ModelOptions, *, caption: str | None, prompt_cache: Flux3PromptCacheRequest) -> EncoderNeed:
        """THE forecast's decision of what a request with `model_options` and `caption` asks of the text-encoder handle (`paired.EncoderNeed`).

        `encodes`: in `vl_refs` mode always (the encoder sees the references,
        nothing is cached); in TEXT mode when the selected encoder is installed
        AND the caption is not served by memory or the library (the lock-free
        prompt plan; `caption` `None` or empty = unknown, so not served).
        `resident`: the encoder handle these options would use holds bytes in
        RAM or VRAM now. Both read without the service lock (the copy-on-write
        memory index, the library snapshot, the manager's ledger), so a
        forecast is answered during a running generation. Used by
        `plan_profiles` (whether the encoder is counted); the run decides the
        same question with its own served tensor (`_prompt_locked`).
        """
        options = model_options.options
        resident = self._holds_key(_encoder_handle_key(options, model_options.device))
        if options.encoder_mode is EncoderMode.VL_REFS:
            return EncoderNeed(encodes=True, resident=resident)
        available = encoder_available(options.model_root, options.encoder)
        served = False
        if available and caption:
            query = Flux3CacheQuery(caption, prompt_cache, options.model_root, options.encoder, text_width(options.model_root))
            served = not self._prompts.plan(query, blocking_load=True).to_encode
        return EncoderNeed(encodes=available and not served, resident=resident)

    def _resolve_package(self, options: Flux3Options) -> tuple[ModelFiles, PackageConfig]:
        """The variant's files and its validated `config.json` (stat calls + one small JSON read).

        # Raises
        `FileNotFoundError` / `ValueError` from `resolve_model_files` /
        `read_package_config` (a missing variant names its missing files).
        """
        files = resolve_model_files(options.model_root, options.variant)
        return files, read_package_config(files.dit_config)

    @staticmethod
    def _require_gguf_dit(options: Flux3Options, files: ModelFiles, package: PackageConfig) -> None:
        """Refuse a GGUF DiT the run cannot build, before the admission (header + config reads only).

        A native DiT reads nothing here (its header is checked by the meta
        build, as always); a GGUF DiT goes through `loading.require_gguf_dit`,
        whose refusal names the wire field `format`.

        # Raises
        `ValueError` / `FileNotFoundError` from that check.
        """
        if isinstance(options.variant, GgufDit):
            from . import loading

            layout = loading.gguf_dit_layout(files.dit_file, format_id=options.variant.id)
            loading.require_gguf_dit(layout.spans, package, source=files.dit_file, format_id=options.variant.id)

    @staticmethod
    def _require_encoder(options: Flux3Options) -> None:
        """Refuse an encode the selected encoder cannot do, before its admission: its weights must be present, a GGUF encoder's header must match.

        Called only when a run (or a build) really encodes: a served caption
        needs neither the weights nor the header. The bf16 shards' headers are
        checked by the meta build, as always; a GGUF encoder (its shards are
        not needed, but `text_encoder/` still is) goes through
        `text_encoder.require_gguf_text_encoder`, whose refusal names the wire
        field `text_encoder_format`.

        # Raises
        `FileNotFoundError` naming what is missing and the wire field `text_encoder_format`; `ValueError` /
        `FileNotFoundError` from the GGUF header check.
        """
        reason, _size = text_encoder_availability(options.model_root, options.encoder)
        if reason is not None:
            raise FileNotFoundError(f"{_OWNER}: промпт нужно закодировать, а текстовый энкодер «{options.encoder.id}» (поле «text_encoder_format») недоступен: {reason}")
        if options.encoder.path is not None:
            from . import text_encoder

            text_encoder.require_gguf_text_encoder(options.encoder.path, package_text_encoder_dir(options.model_root), format_id=options.encoder.id)

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

    @staticmethod
    def _device_of(options: Flux3Options) -> str:
        """The normalized run device of `options` (`options.device`, else the backend's selected device)."""
        raw_device = options.device or _resolve_selected_backend_device(_DEVICE_FALLBACK)
        return normalize_device(raw_device) or str(raw_device)

    def _model_options(self, options: Flux3Options) -> Flux3ModelOptions:
        """The manager's options of `options`: the normalized run device and the concrete fp8 path (`_fp8_path`).

        # Raises
        `RuntimeError` when an explicit `SCALED_MM` cannot run on the device.
        """
        device = self._device_of(options)
        return Flux3ModelOptions(options=options, device=device, fp8_compute=self._fp8_path(options, device))

    @staticmethod
    def _handles(model_options: Flux3ModelOptions, *, dit_format: str | None = None) -> tuple[HandleRequest, HandleRequest]:
        """The pipeline and text-encoder `HandleRequest`s of `model_options` (the DiT format `dit_format`, default the selected one)."""
        options = model_options.options
        pipeline = HandleRequest(large_model.pipeline_model_name(options.ae_dtype), model_options, dit_format or options.variant.value)
        encoder = HandleRequest(large_model.encoder_model_name(options.encoder_mode), model_options, options.encoder.id)
        return pipeline, encoder

    # ---- the wire: profiles adapter and status ----
    def plan_profiles(self, params: dict[str, Any], shape: RequestShape, *, format: str | None, steps: Steps) -> paired.PairedReport:
        """The two-handle planning of a FLUX.3 run for wire `params` (`paired.plan_pair` over `FLUX3_PAIR`; read-only, never waits for the service lock).

        (1) the pipeline (`flux3_klein.ae_<dtype>`) alone; (2) when the encoder
        is counted (`_encoder_need(...).counted` for the params' `prompt` and
        prompt-library fields: not counted when the caption is served and the
        encoder handle holds no byte), the encoder
        (`flux3_klein_text_encoder.<mode>`, in the selected encoder format)
        beside (1)'s reservation under the combined VRAM-axis ceiling a run
        would give it; (3) the pipeline again with the encoder's reservation
        claimed. `format` is the DiT format to plan (`None` = the request's
        selected one, not the manager's best, so the answer describes the run
        the same params start). The calibration tool reads the run peaks from
        the report (`run_device_peak(FLUX3_PAIR)`, `run_host_peak`).

        # Raises
        `ValueError` for invalid params (`wire.normalize_flux3_params`) or a
        format the model lacks; `RuntimeError` for an explicit `scaled_mm`
        the device cannot run.
        """
        wire = normalize_flux3_params(params, require_prompt=False)
        model_options = self._model_options(wire.options)
        return self.plan_pair(model_options, shape, format=format, steps=steps, caption=wire.prompt, prompt_cache=wire.prompt_cache)

    def plan_pair(
        self,
        model_options: Flux3ModelOptions,
        shape: RequestShape,
        *,
        format: str | None,
        steps: Steps,
        caption: str | None = None,
        prompt_cache: Flux3PromptCacheRequest | None = None,
    ) -> paired.PairedReport:
        """`plan_profiles` over already-resolved manager options (the calibration tool: `_plan(...).model_options` / `.shape`).

        `caption` / `prompt_cache` decide whether the caption is served
        (`_encoder_need`); `caption` `None` = unknown, planned as encoded (the
        calibration tool makes every caption unique).

        # Raises
        `ValueError` from the manager (a format the model lacks).
        """
        pipeline, encoder = self._handles(model_options, dit_format=format)
        need = self._encoder_need(model_options, caption=caption, prompt_cache=prompt_cache or Flux3PromptCacheRequest())
        return paired.plan_pair(self._large, pipeline, encoder if need.counted else None, shape, model_options.device, steps, FLUX3_PAIR)

    def large_model_profiles(self, params: dict[str, Any], shape: RequestShape, *, format: str | None, steps: Steps) -> dict[str, Any]:
        """`large_models.profiles` for a FLUX.3 run: the PIPELINE's pickers with the text encoder's admission claimed (`plan_profiles(...).to_wire()`).

        The answer is FLUX.2's shape (`paired.PairedReport.to_wire`): `selected`
        is the pipeline's plan (the run's total is `selected` + `encoder`,
        summed by the client; each forecast carries `host_baseline` so that sum
        counts the process once); `model` the pipeline's registration; the
        additive `encoder` is the encoder's `selected` (`null` exactly when the
        encoder is not counted, `EncoderNeed.counted`) and `encoder_formats`
        its `formats` rows (one per offered encoder format; `[]` when
        `encoder` is `null`). An encoder that does not fit makes
        `selected.fits` `false` with the encoder's `reason` /
        `shortest_resource`. Read-only.

        # Raises
        See `plan_profiles`.
        """
        return self.plan_profiles(params, shape, format=format, steps=steps).to_wire()

    def status(self, params: dict[str, Any] | None) -> dict[str, Any]:
        """The `inpaint.flux3_klein.status` answer for wire `params` (file checks only; never loads, never probes).

        `available` / `reason`: the selected DiT format's files; `formats`:
        every OFFERED DiT format's `{available, reason, dit_bytes}`, by id
        (`dit_formats`); `text_encoder_formats`: `bf16` and every offered GGUF
        encoder's `{available, reason, bytes}` (`config.text_encoder_availability`:
        the weights plus the `text_encoder/` directory every format needs);
        `ae_present`; `text_encoder_present` = the SELECTED encoder format is
        available; `prompt_cached` = the caption (`params.prompt`) is served by
        memory or the library (TEXT mode; always `false` in `vl_refs` mode,
        for an empty prompt and without a root) and `prompt_cache_entry` the
        `EntryInfo` that would serve it (`null` when none); plus `health()`.
        Without a `source_root` nothing is
        available and `reason` says so (rows for the three native DiT formats
        and `bf16`); every OTHER supplied field is validated before that
        answer (`wire.normalize_flux3_status_params`). Never takes the service
        lock.

        # Raises
        `ValueError` for invalid params other than a missing `source_root`.
        """
        from .config import ENCODER_BF16, package_ae_file

        wire = normalize_flux3_status_params(params)
        if wire is None:
            reason = "Не указан каталог модели FLUX.3 klein"
            return {
                "available": False,
                "reason": reason,
                "formats": {variant.value: {"available": False, "reason": reason, "dit_bytes": 0} for variant in WeightFormat},
                "text_encoder_formats": {ENCODER_BF16: {"available": False, "reason": reason, "bytes": 0}},
                "ae_present": False,
                "text_encoder_present": False,
                "prompt_cached": False,
                "prompt_cache_entry": None,
                **self.health(),
            }
        options = wire.options
        # `formats` needs a concrete fp8 path only for `compute_path`, which
        # status does not report: DEQUANT avoids the device probe.
        found = large_model.formats(_placeholder_options(options, "cpu"))
        selected = found[options.variant.value]
        encoders: dict[str, dict[str, Any]] = {}
        for source in wire.text_encoder_formats:
            encoder_reason, encoder_bytes = text_encoder_availability(options.model_root, source)
            encoders[source.id] = {"available": encoder_reason is None, "reason": encoder_reason, "bytes": encoder_bytes}
        prompt_cached, prompt_entry = False, None
        if options.encoder_mode is EncoderMode.TEXT and wire.prompt:
            query = Flux3CacheQuery(wire.prompt, wire.prompt_cache, options.model_root, options.encoder, text_width(options.model_root))
            prompt_cached, prompt_entry = self._prompts.status(query)
        return {
            "available": selected.available,
            "reason": selected.reason,
            "formats": {name: {"available": info.available, "reason": info.reason, "dit_bytes": info.dit_bytes} for name, info in found.items()},
            "text_encoder_formats": encoders,
            "ae_present": package_ae_file(options.model_root).is_file(),
            "text_encoder_present": encoders[options.encoder.id]["available"],
            "prompt_cached": prompt_cached,
            "prompt_cache_entry": prompt_entry,
            **self.health(),
        }

    def _plan(self, request: Flux3Request, options: Flux3Options) -> _Plan:
        """Validate and shape the request; no lock, no admission, no weights.

        # Raises
        See `generate` (everything raised before the admission).
        """
        from . import autoencoder, packing

        files, package = self._resolve_package(options)
        self._require_gguf_dit(options, files, package)
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
        model_options = self._model_options(options)
        device, fp8 = model_options.device, model_options.fp8_compute
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
            "text_encoder_format": options.encoder.id,
            "image_tokens": (width // ratio) * (height // ratio),
            "ref_tokens": sum((image.width // ratio) * (image.height // ratio) for image in references.images),
            **_steps_info(options.steps),
            "compute_path": large_model.compute_path(options.variant, fp8),
        }
        pipeline, encoder = self._handles(model_options)
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
            model_options=model_options,
            pipeline=pipeline,
            encoder=encoder,
            shape=shape,
            info=info,
        )

    # ---- steps ----
    def _step(self, observer: PhaseObserver | None, phase: Phase, sessions: _Sessions, info: Mapping[str, Any], run: Callable[[], T], **extra: Any) -> T:
        """Run one manager step as observer `phase`.

        Begin info = `info` + the effective steps of the step's handle
        (`vram_step_effective`, `ram_step_effective`) + the forecast of the
        plans in force for this step (`_forecast_info`) + `extra`; the end event
        repeats it.
        """
        session = sessions.of(phase)
        with observed(observer, phase, **info, **_steps_info(session.steps_effective, "_effective"), **_forecast_info(sessions, phase), **extra):
            return run()

    def _load_locked(self, sessions: _Sessions, plan: _Plan, observer: PhaseObserver | None) -> None:
        """The load scope: DIT_LOAD, AE_LOAD on the pipeline handle.

        A component that holds nothing is read into its place for the
        pipeline's first phase (`ref_encode`; `Session.load`, action
        `loaded`); one already held is left for DIT_OFFLOAD (action `resident`
        = nothing read). A component the plan keeps nowhere before its own
        phase is read by the step that needs it. Caller holds `self._lock`.
        """
        session = sessions.pipeline
        first = session.plan.phases[0]
        for phase, name in ((Phase.DIT_LOAD, COMPONENT_DIT), (Phase.AE_LOAD, COMPONENT_AE)):
            reads = not _holds(self._placements(session, _PIPELINE_COMPONENTS)[name]) and _holds(_target_of(first.components[name]))
            self._step(observer, phase, sessions, plan.info, (lambda name=name: session.load(name)) if reads else (lambda: None), action="loaded" if reads else "resident")

    @staticmethod
    def _dit_out_action(before: _Placement, after: _Placement) -> str:
        """DIT_OFFLOAD / DIT_OFFLOAD_DECODE `action`: `offloaded` (leaves the device), `on_host` (stays in RAM), `skipped`."""
        if before[0] is Tier.DEVICE and after[0] is not Tier.DEVICE:
            return "offloaded"
        if before[0] is Tier.HOST and after[0] is Tier.HOST:
            return "on_host"
        return "skipped"

    def _encode_locked(self, sessions: _Sessions, plan: _Plan, observer: PhaseObserver | None, prompt: _Prompt) -> Any:
        """ENCODER_LOAD, ENCODE and ENCODER_RELEASE; returns the `EncodedText` (caller holds `self._lock`).

        A served caption (`prompt.served`) touches no encoder: ENCODER_LOAD
        reports `action` `cached` (nothing done), ENCODE builds the
        `EncodedText` from the served tensor (`_served_text`; nothing under
        `text_encoder/` is read),
        ENCODER_RELEASE does nothing; their forecast keys are `None` (no
        encoder session). Otherwise, on the encoder handle: ENCODER_LOAD =
        `transition(None, encode)` (the text processor is read first); ENCODE
        = `run_phase(encode)`, a TEXT result remembered in memory under
        `prompt.provenance`; ENCODER_RELEASE = `finish()`, which shrinks the
        encoder to its between-runs state now (it is not needed again this
        run) and releases its reservation.
        """
        if prompt.served is not None:
            served = prompt.served
            self._step(observer, Phase.ENCODER_LOAD, sessions, plan.info, lambda: None, action="cached")
            encoded = self._step(observer, Phase.ENCODE, sessions, plan.info, lambda: self._served_text(served))
            info = {**plan.info, "text_tokens": int(encoded.tokens.shape[0]), "text_real_tokens": int(encoded.real_length)}
            self._step(observer, Phase.ENCODER_RELEASE, sessions, info, lambda: None)
            return encoded
        encoder = sessions.encoder
        if encoder is None:
            # `_admit_locked` opens the encoder whenever the prompt decision says the run encodes.
            raise RuntimeError(f"{_OWNER}: кодирование промпта без сеанса текстового энкодера")
        options = plan.model_options.options
        before = self._placements(encoder, _ENCODER_COMPONENTS)[COMPONENT_ENCODER]
        target = _target_of(encoder.plan.phase(PHASE_ENCODE).components[COMPONENT_ENCODER])
        if not _holds(before) or before[1] is not target[1]:
            encoder_action = "loaded"
        elif before[0] is Tier.HOST and target[0] is Tier.DEVICE:
            encoder_action = "restored"
        else:
            encoder_action = "resident"

        def enter_encode() -> None:
            self._processor_locked(plan.files.text_encoder_dir)
            encoder.transition(None, PHASE_ENCODE)

        self._step(observer, Phase.ENCODER_LOAD, sessions, plan.info, enter_encode, action=encoder_action)

        encode = self._encode_phase(self._processor_locked(plan.files.text_encoder_dir), options, plan.caption, plan.references.images)
        encoded = self._step(observer, Phase.ENCODE, sessions, plan.info, lambda: encoder.run_phase(PHASE_ENCODE, encode))
        if prompt.provenance is not None:
            self._prompts.remember_locked(prompt.library, plan.caption, MAX_SEQUENCE_LENGTH, prompt.provenance, encoded.tokens.detach().to("cpu"), pl.utc_now())
        info = {**plan.info, "text_tokens": int(encoded.tokens.shape[0]), "text_real_tokens": int(encoded.real_length)}
        self._step(observer, Phase.ENCODER_RELEASE, sessions, info, encoder.finish)
        return encoded

    @staticmethod
    def _encode_phase(processor: Any, options: Flux3Options, caption: str, references: Any) -> Callable[[PhaseContext], Any]:
        """The restartable `encode` phase function of the encoder handle: `caption` (and, in `vl_refs` mode, `references`) -> `EncodedText` on the encoder's exec device."""
        import torch

        from . import text_encoder

        def encode(ctx: PhaseContext) -> Any:
            handle = text_encoder.TextEncoderHandle(
                model=ctx.module(COMPONENT_ENCODER),
                processor=processor,
                mode=options.encoder_mode,
                device=torch.device(ctx.exec_device(COMPONENT_ENCODER)),
                keep_layers=ENCODER_KEEP_LAYERS,
            )
            if options.encoder_mode is EncoderMode.VL_REFS:
                return text_encoder.encode_with_references(handle, caption, references, vl_ref_max_pixels=options.vl_ref_max_pixels)
            return text_encoder.encode_text(handle, caption)

        return encode

    def _generate_locked(self, sessions: _Sessions, plan: _Plan, held: _Held, observer: PhaseObserver | None, step_callback: StepCallback | None, prompt: _Prompt) -> Flux3Result:
        """The run scope, one manager step per observer phase (see `generate`); caller holds `self._lock`."""
        import torch

        from . import autoencoder, packing, sampling

        dit_config = plan.package.dit
        rope_scale = dit_config.image_rope_scale
        options = plan.model_options.options
        pipeline = sessions.pipeline

        # DIT_OFFLOAD: DiT and AE from the between-runs layout to the pipeline's
        # first-phase (ref_encode) placement, where they sit while the encoder
        # runs. (`Session.load` reconciles only the named components; nothing
        # is read for a component that placement keeps nowhere.)
        first_state = pipeline.plan.phase(PHASE_REF_ENCODE)
        dit_before = self._placements(pipeline, _PIPELINE_COMPONENTS)[COMPONENT_DIT]
        dit_after = _target_of(first_state.components[COMPONENT_DIT]) if _holds(dit_before) else dit_before
        self._step(observer, Phase.DIT_OFFLOAD, sessions, plan.info, lambda: pipeline.load(COMPONENT_DIT, COMPONENT_AE), action=self._dit_out_action(dit_before, dit_after))

        encoded = self._encode_locked(sessions, plan, observer, prompt)
        info = {**plan.info, "text_tokens": int(encoded.tokens.shape[0]), "text_real_tokens": int(encoded.real_length)}

        def ref_encode(ctx: PhaseContext) -> Any:
            ae = ctx.module(COMPONENT_AE)
            device = ctx.exec_device(COMPONENT_AE)
            # Reference ids get the l axis -(1 + i) (`packing.reference_tokens`).
            return packing.reference_tokens([ae.encode(packing.to_tensor(image).to(device)) for image in plan.references.images], rope_scale)

        cond = self._step(observer, Phase.REF_ENCODE, sessions, info, lambda: pipeline.run_phase(PHASE_REF_ENCODE, ref_encode))

        dit_before = self._placements(pipeline, _PIPELINE_COMPONENTS)[COMPONENT_DIT]
        restore = "resident" if dit_before[0] is Tier.DEVICE else "restored"
        self._step(observer, Phase.DIT_RESTORE, sessions, info, lambda: pipeline.transition(PHASE_REF_ENCODE, PHASE_DENOISE), action=restore)

        def denoise(ctx: PhaseContext) -> Any:
            device = ctx.exec_device(COMPONENT_DIT)
            latent_h, latent_w = autoencoder.latent_shape(plan.height, plan.width, autoencoder.DOWNSAMPLING_RATIO)
            noise = packing.initial_noise(latent_h, latent_w, plan.seed, dit_config.in_channels["image"]).to(device)
            image_ids = autoencoder.latent_ids(latent_h, latent_w, device)
            text = (encoded.tokens.to(device), encoded.ids.to(device))
            packed = packing.pack([(text, cond, (noise, packing.scaled_ids(image_ids, rope_scale)))], dit_config.in_channels)
            schedule = sampling.flow_map_schedule(plan.steps, plan.package.sampler.alpha)
            tokens = sampling.denoise_flow_map(ctx.module(COMPONENT_DIT), packed, schedule, device_type=torch.device(device).type, step_callback=step_callback)
            return tokens, image_ids

        tokens, image_ids = self._step(observer, Phase.DENOISE, sessions, info, lambda: pipeline.run_phase(PHASE_DENOISE, denoise))

        dit_before = self._placements(pipeline, _PIPELINE_COMPONENTS)[COMPONENT_DIT]
        dit_after = _target_of(pipeline.plan.phase(PHASE_DECODE).components[COMPONENT_DIT])
        self._step(observer, Phase.DIT_OFFLOAD_DECODE, sessions, info, lambda: pipeline.transition(PHASE_DENOISE, PHASE_DECODE), action=self._dit_out_action(dit_before, dit_after))

        def decode(ctx: PhaseContext) -> Any:
            ae = ctx.module(COMPONENT_AE)
            image = packing.to_pil(ae.decode(tokens.to(ctx.exec_device(COMPONENT_AE)), image_ids.to(ctx.exec_device(COMPONENT_AE))))
            if plan.references.crop_box is not None:
                image = image.crop(plan.references.crop_box)
            return image

        image = self._step(observer, Phase.DECODE, sessions, info, lambda: pipeline.run_phase(PHASE_DECODE, decode))

        encoder = sessions.encoder
        degraded = pipeline.degraded or (encoder is not None and encoder.degraded)
        reason = pipeline.degrade_reason if pipeline.degrade_reason is not None else (encoder.degrade_reason if encoder is not None else None)
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
                steps_requested=pipeline.steps_requested,
                steps_effective=pipeline.steps_effective,
                degraded=degraded,
                degrade_reason=reason,
            ),
        )

    def _processor_locked(self, directory: Path) -> Any:
        """The text processor of the encoder directory `directory` (loaded once, kept until `unload`); caller holds `self._lock`."""
        from . import text_encoder

        key = str(directory)
        processor = self._processors.get(key)
        if processor is None:
            processor = text_encoder.load_text_processor(directory)
            self._processors[key] = processor
        return processor

    # ---- the prompt (memory, library, encoder) ----
    def _prompt_locked(self, plan: _Plan, prompt_cache: Flux3PromptCacheRequest) -> _Prompt:
        """THE run's decision of how the caption becomes DiT text tokens; caller holds `self._lock`, before the admissions.

        `vl_refs`: always encoded, the cache never consulted. TEXT: the
        caption served by memory or the request's library
        (`_serve_text_locked`), else encoded. An encode requires the selected
        encoder (`_require_encoder`) and records its provenance now, before
        anything is loaded; an encoder that cannot be identified
        (`encoder_provenance` raises `ValueError`) still runs, its embedding is
        just not remembered (logged): the cache never fails a run.

        # Raises
        `FileNotFoundError` / `ValueError` from `_require_encoder`, or a chosen
        cache file that vanished while no encoder can replace it.
        """
        options = plan.model_options.options
        library = prompt_cache.library or ""
        if options.encoder_mode is EncoderMode.VL_REFS:
            self._require_encoder(options)
            return _Prompt(served=None, provenance=None, library=library)
        query = Flux3CacheQuery(plan.caption, prompt_cache, options.model_root, options.encoder, plan.package.dit.in_channels["text"])
        served = self._serve_text_locked(query)
        if served is not None:
            log.info("%s: промпт взят из кэша, текстовый энкодер не загружается", _OWNER)
            return _Prompt(served=served, provenance=None, library=library)
        self._require_encoder(options)
        try:
            provenance = encoder_provenance(options.model_root, options.encoder)
        except ValueError as exc:
            log.warning("%s: текстовый энкодер «%s» не опознан, закодированный промпт не будет закэширован: %s", _OWNER, options.encoder.id, exc)
            return _Prompt(served=None, provenance=None, library=library)
        if provenance is None:
            # `_require_encoder` just found the weights: only a race with a deletion gets here.
            raise FileNotFoundError(f"{_OWNER}: текстовый энкодер «{options.encoder.id}» пропал во время подготовки запуска")
        return _Prompt(served=None, provenance=provenance, library=library)

    def _serve_text_locked(self, query: Flux3CacheQuery) -> Any | None:
        """The host tensor that serves `query.prompt` (memory or library, `PromptCacheOrchestrator.plan` + `serve_locked`), `None` when it must be encoded; caller holds `self._lock`.

        A library file that vanished or changed since the plan
        (`LibraryEntryGone`, logged by the orchestrator) re-plans once; a
        caption still unserved then is encoded when the encoder is installed.

        # Raises
        `ValueError` when the chosen file vanished and the encoder is not
        installed (nothing can replace it).
        """
        choice = self._prompts.plan(query, blocking_load=True).served.get(query.prompt)
        if choice is None:
            return None
        tensor = self._prompts.serve_locked(choice)
        if tensor is not None:
            return tensor
        lost = choice.entry.file if isinstance(choice.entry, pl.LibraryEntry) else ""
        replan = self._prompts.plan(query, blocking_load=True).served.get(query.prompt)
        tensor = self._prompts.serve_locked(replan) if replan is not None else None
        if tensor is None and not encoder_available(query.model_root, query.encoder):
            raise ValueError(
                f"Файл кэша промпта пропал или изменился во время запуска ({lost or '—'}), а текстовый энкодер "
                "недоступен — закодировать промпт заново нечем. Скачайте текстовый энкодер или выберите "
                "другой сохранённый промпт."
            )
        return tensor

    @staticmethod
    def _served_text(tokens: Any) -> Any:
        """The `EncodedText` of a served caption: `tokens` as stored, the DiT ids `(0, 0, 0, i)`, `real_length` = the stored length.

        Reads nothing under `text_encoder/` (the user's rule: a served
        caption runs with the encoder directory absent), so the unpadded
        token count is not known: `real_length` (observer `text_real_tokens`,
        `Flux3Result.text_real_tokens`) is the padded length, an upper bound.
        It is informational: the denoise reads the tokens and the ids only.
        """
        import torch

        from . import text_encoder

        length = int(tokens.shape[0])
        # The reference's ids rule lives in `text_encoder` (its `_dit_ids`); the served tensor is on the host.
        return text_encoder.EncodedText(tokens=tokens, ids=text_encoder._dit_ids(length, torch.device("cpu")), real_length=length)

    # ---- prompt cache: build and the library ----
    def prompt_cache_build(
        self,
        params: dict[str, Any] | None,
        *,
        name: str | None = None,
        observer: PhaseObserver | None = None,
        load_progress: LoadProgressSink | None = None,
    ) -> dict[str, Any]:
        """«Кэшировать»: save an entry of the CURRENT encoder for the caption (`params.prompt`) into the library, under the preset name `name`.

        TEXT mode only. A caption that already has an entry of the current
        encoder (`prompt_library.same_encoder`) is not written again; else an
        embedding this process holds in memory for that encoder is reused, else
        the caption is encoded through the encoder handle ALONE (no pipeline is
        admitted; the request's steps, a minimal shape) and the encoder is
        released afterwards unless its handle already held bytes when the call
        arrived. The encode reports the observer phases ENCODER_LOAD, ENCODE and
        ENCODER_RELEASE (no forecast keys); `load_progress` receives the
        encoder read. The name rules are `PromptCacheOrchestrator.build_locked`'s.

        Returns `{"prompt", "name", "encoded", "written": [EntryInfo],
        "renamed", "prompt_cached": true, "device"}`.

        # Raises
        `ValueError` for invalid params, an empty prompt, `encoder_mode`
        `vl_refs`, a request without `prompt_library`, an unidentifiable
        encoder or a failed write; `FileNotFoundError` when the encoder is not
        installed; `LargeModelRefused` when the encode does not fit; whatever
        the encoder load raises or `observer` raises.
        """
        wire = normalize_flux3_params(params, require_prompt=False)
        require_prompt_to_cache(wire.prompt)
        options = wire.options
        if options.encoder_mode is EncoderMode.VL_REFS:
            raise ValueError(
                f"{_OWNER}: поле «encoder_mode» = «{EncoderMode.VL_REFS.value}»: в режиме с референсами промпт кодируется "
                "при каждом запуске и не кэшируется. Выберите текстовый режим, чтобы сохранить промпт."
            )
        query = Flux3CacheQuery(wire.prompt, wire.prompt_cache, options.model_root, options.encoder, text_width(options.model_root))
        library = self._prompts.require_library(query)
        device = self._device_of(options)

        def encode_missing(need: list[str]) -> dict[str, Any]:
            return {text: self._build_encode_locked(options, device, text, query.cache.library or "", observer, load_progress) for text in need}

        def nothing_to_encode(any_to_write: bool) -> None:
            log.info("%s: промпт уже %s — кодирование не нужно", _OWNER, "в памяти" if any_to_write else "в библиотеке")

        with self._lock:
            try:
                current = encoder_provenance(options.model_root, options.encoder)
                if current is None:
                    reason, _size = text_encoder_availability(options.model_root, options.encoder)
                    raise FileNotFoundError(f"{_OWNER}: кэшировать промпт нечем — текстовый энкодер «{options.encoder.id}» недоступен: {reason}")
                extra = {"text_encoder_path": str(options.encoder.path or package_text_encoder_dir(options.model_root))}
                built = self._prompts.build_locked(query, name, encode_missing, current=current, extra=extra, on_nothing_to_encode=nothing_to_encode)
                self._last_error = None
                self._publish_health_locked()
            except Flux3Canceled:
                # A cancel is the user's choice, not a failure of the service (as for a run).
                log.info("%s: кэширование промпта отменено", _OWNER)
                raise
            except Exception as exc:
                self._last_error = str(exc)
                self._publish_health_locked()
                raise
        log.info(
            "%s: промпт «%s» в библиотеке %s: записано %d, %s, %s.",
            _OWNER,
            built.name,
            library.spec.library_id,
            len(built.written),
            "закодирован" if built.encoded else "без кодирования",
            "переименован" if built.renamed else "имя прежнее",
        )
        return {
            "prompt": built.prompt,
            "name": built.name,
            "encoded": built.encoded,
            "written": [entry_info(entry, built.current) for entry in built.written],
            "renamed": built.renamed,
            "prompt_cached": True,
            "device": device,
        }

    def _build_encode_locked(self, options: Flux3Options, device: str, caption: str, library: str, observer: PhaseObserver | None, load_progress: LoadProgressSink | None) -> Any:
        """Encode `caption` (TEXT) through the encoder handle alone for a build; returns the host tokens, remembered in memory under `library` (`""` = none). Caller holds `self._lock`.

        The handle is admitted with the request's steps and a minimal shape
        (no pipeline, no ceiling); a handle that held nothing before is
        unloaded afterwards, also after a failure or a cancel (the build's own
        release). Phases: ENCODER_LOAD
        (`action` as a run reports it), ENCODE, ENCODER_RELEASE.

        # Raises
        `FileNotFoundError` / `ValueError` from `_require_encoder`;
        `LargeModelRefused` when the encode does not fit; whatever the load,
        the encode or `observer` raises.
        """
        self._require_encoder(options)
        provenance = encoder_provenance(options.model_root, options.encoder)
        if provenance is None:
            raise FileNotFoundError(f"{_OWNER}: текстовый энкодер «{options.encoder.id}» пропал во время подготовки кэширования")
        model_options = _placeholder_options(options, device)
        _pipeline, handle = self._handles(model_options)
        key = _encoder_handle_key(options, device)
        resident = self._holds_key(key)
        shape = large_model.request_shape(MIN_SIDE, MIN_SIDE, [], caption=caption, steps=None)
        directory = package_text_encoder_dir(options.model_root)
        info = {"encoder_mode": options.encoder_mode.value, "text_encoder_format": options.encoder.id, "device": device, **_steps_info(options.steps)}
        try:
            session = self._large.acquire(handle.model, handle.options, shape, device=device, format=handle.format, steps=options.steps)
        except LargeModelError as exc:
            log.warning("%s: кэширование промпта не допущено (%s): %s", _OWNER, handle.model, exc)
            raise
        session.set_load_progress(load_progress)
        try:
            before = self._placements(session, _ENCODER_COMPONENTS)[COMPONENT_ENCODER]
            action = "resident" if _holds(before) else "loaded"

            def enter_encode() -> None:
                self._processor_locked(directory)
                session.transition(None, PHASE_ENCODE)

            effective = _steps_info(session.steps_effective, "_effective")
            with observed(observer, Phase.ENCODER_LOAD, **info, **effective, action=action):
                enter_encode()
            encode = self._encode_phase(self._processor_locked(directory), options, caption, ())
            with observed(observer, Phase.ENCODE, **info, **effective):
                encoded = session.run_phase(PHASE_ENCODE, encode)
            tokens = encoded.tokens.detach().to("cpu")
            with observed(observer, Phase.ENCODER_RELEASE, **info, **effective, text_tokens=int(tokens.shape[0]), text_real_tokens=int(encoded.real_length)):
                session.finish()
        except BaseException as exc:
            session.finish(exc)
            raise
        finally:
            # The build's own release, after a failure or a cancel too: a
            # handle that held nothing before the build holds nothing after it.
            if not resident and self._large.unload_key(key):
                log.info("%s: текстовый энкодер выгружен после кэширования промпта", _OWNER)
        self._prompts.remember_locked(library, caption, MAX_SEQUENCE_LENGTH, provenance, tokens, pl.utc_now())
        return tokens

    def _library_query(self, params: dict[str, Any] | None) -> Flux3CacheQuery:
        """The browse-only library methods' query of `params`: lenient about a missing `source_root` (no width, no current encoder), strict about every supplied field.

        # Raises
        `ValueError` for malformed params (`wire.normalize_flux3_library_params`).
        """
        cache, wire = normalize_flux3_library_params(params if isinstance(params, dict) else {})
        if wire is None:
            return Flux3CacheQuery("", cache, None, ENCODER_BF16_SOURCE, None)
        return Flux3CacheQuery(wire.prompt, cache, wire.options.model_root, wire.options.encoder, text_width(wire.options.model_root))

    def prompt_cache_list(self, params: dict[str, Any] | None) -> dict[str, Any]:
        """`.prompt_cache.list`: the presets of the library `params.prompt_library` (`PromptCacheOrchestrator.listing`; `legacy` is always `null`).

        Needs no DiT; without a `source_root` no width filter applies and no
        entry is marked `current_encoder`. `text_encoder_available` is the
        selected encoder's presence.

        # Raises
        `ValueError` for malformed params or a request without `prompt_library`.
        """
        return self._prompts.listing(self._library_query(params))

    def prompt_cache_export(self, params: dict[str, Any] | None, file: str, path: str) -> dict[str, Any]:
        """`.prompt_cache.export`: copy the library entry `file` to the `.msprompt` path `path` (`PromptCacheOrchestrator.export`).

        # Raises
        `ValueError` for malformed params, a request without `prompt_library`,
        an entry that is not in the library, or a rejected destination.
        """
        return self._prompts.export(self._library_query(params), file, path)

    def prompt_cache_import(self, params: dict[str, Any] | None, path: str) -> dict[str, Any]:
        """`.prompt_cache.import`: copy a v2 `.msprompt` file of the FLUX.3 library into it (`PromptCacheOrchestrator.import_file`).

        A file of another model's library, a v1 file or any other container is
        refused.

        # Raises
        `ValueError` for malformed params, a rejected path, a file that is not
        a FLUX.3 prompt cache, or a failed write.
        """
        return self._prompts.import_file(self._library_query(params), path)
