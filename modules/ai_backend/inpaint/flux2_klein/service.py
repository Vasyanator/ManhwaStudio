"""
File: modules/ai_backend/inpaint/flux2_klein/service.py

Purpose:
`Flux2KleinInpaintService` - the object `server.py` puts on `AppState` and every
`inpaint.flux2_klein.*` IPC handler calls. It owns the service lock, the
in-memory prompt-embedding cache, the tokenizer / scheduler caches and the
request methods; WHERE the weights live during and between runs is decided by
the large-model manager (`runtime.large_models`), which this service drives
through `Session`s of the FLUX.2 specs (`large_model.py`).

Main responsibilities:
- `status` / `health` / `unload` - what the client polls (the residency
  report answers from the manager's ledger);
- `inpaint_image_bytes` - one region edit, in the documented order: the
  pipeline handle (transformer + VAE) is admitted and loaded FIRST, then the
  text encoder handle encodes whatever the prompt cache misses (on the GPU
  when the step pair and the device allow it beside the pipeline, else on the
  CPU from RAM; always under the combined VRAM-axis ceiling planned before
  the pipeline's admission, `_combined_ceilings` / `memory` module notes), then the denoise and the VAE decode run as manager phases (a
  memory failure re-plans along the degrade ladder: tiled decode, lowered tile,
  a lower «Видеопамять» step);
- `component_action` - one per-component residency action, each a manager
  session (load / unload / park in RAM / back to the GPU / warm-up);
- the `prompt_cache_*` methods - the library surface, keyed identically to the
  in-memory cache through `_prompt_cache_key`;
- `large_model_profiles` - the adapter `large_models.profiles` calls: the
  pipeline's step pickers planned with the encoder's admission claimed
  (`large_model_name` / `large_model_inputs` stay the single-handle fallback);
- `set_phase_observer` - the calibration tool's hook (every manager step of
  both handles, plus the FLUX.2 `warmup`);
- `_effective_guidance_scale` - the ONE place a run learns whether
  classifier-free guidance can happen at all.

Notes:
- Sessions are acquired UNDER the service lock, always pipeline before encoder
  (the stated exception of the manager's rule "acquire before the service
  lock", `LargeModelManager.acquire`): FLUX.2 holds no small-pool lease and
  registers no reclaim callback that takes this lock (the large pool's reclaim
  only TRIES a handle's run lock), and one run opens two handles. Acquired
  outside the lock, a run (pipeline run lock, then the encoder's) and an
  encoder «load» action (encoder run lock, then this lock) would take the
  locks in opposite orders; under it the order is total: this lock ->
  pipeline -> encoder. It also lets an action refuse "busy" before any session
  exists. Pinned by `test_service.LockOrderTests`.
- Symbols the test suite monkeypatches are reached through their defining
  module (`hardware._clear_torch_cache`, `prompt_cache.write_prompt_file`,
  `attention.build_text_attention_mask`, ...).
- torch / diffusers / transformers / cv2 are imported lazily.
"""

from __future__ import annotations

import logging
import os
import threading
import time
from collections import OrderedDict
from collections.abc import Callable, Mapping
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    import numpy as np

from ...runtime.health_snapshot import HealthSnapshot
from ...runtime.large_models.manager import LargeModelManager, MemoryLimitsStore, ProfilesReport, Session, SessionObserver
from ...runtime.large_models.forecast import Forecast
from ...runtime.large_models.planner import Planned
from ...runtime.large_models.spec import ExecDevice
from ...runtime.large_models.spec import RequestShape
from ...runtime.large_models.tiers import IDLE_PHASE, RamStep, Steps, VramStep
from ...runtime.model_manager import SmallModelManager
from ...runtime.residency.footprint import normalize_device
from ...runtime.residency.holdings import Pool
from ..region_imaging import (
    composite_over_region,
    decode_image_rgb,
    decode_mask,
    dilate_mask,
    encode_png_rgb,
    match_color_local,
    match_color_outside_mask,
    require_solid_mask,
)
from . import attention, hardware, large_model, memory, pipeline, prompt_cache
from .components import (
    _SCHEDULER_MARKER,
    _SCHEDULER_SUBDIR,
    _TOKENIZER_MARKERS,
    _TOKENIZER_SUBDIR,
    _component_states,
    _first_unavailable_reason,
    _require_component_dir,
    checkpoint_is_distilled,
    component_search_roots,
    require_encoder_transformer_compatible,
    require_text_encoder,
    text_encoder_available,
)
from .params import (
    _lenient_paths,
    _PATH_KEYS,
    effective_steps,
    encoder_on_cpu,
    MIN_REGION_SIDE,
    normalize_flux2_klein_params,
    text_encoder_dtype_name,
    validate_region_size,
)
from .pipeline import (
    _component_actions,
    _component_busy_message,
    _encode_prompt_phase,
    _require_action_name,
    _require_component_name,
    _require_execution_device,
    _warmup_vae_decode,
    ACTIONABLE_COMPONENTS,
    decode_region_latents,
    residency_of,
)
from .progress import (
    _file_progress_reporter,
    _progress_reporter,
    LOAD_PHASE_STEPS,
    LOAD_STEP_ENCODE,
    LOAD_STEP_ENCODER_DONE,
    LOAD_STEP_PLACEMENT,
    LOAD_STEP_PREPARE,
    LOAD_STEP_SCHEDULER,
    LOAD_STEP_TEXT_ENCODER,
    LOAD_STEP_TOKENIZER,
    LOAD_STEP_TRANSFORMER,
    LOAD_STEP_VAE,
    LOAD_STEP_WARMUP,
    ProgressCb,
)
from .prompt_cache import (
    encoder_family_name,
    find_prompt_cache_entry,
    list_prompt_cache_entries,
    list_prompt_cache_families,
    local_encoder_identity,
    prompt_cache_entry_path,
    prompt_cache_family_dir,
    prompt_cache_root,
    prompt_file_metadata,
    PROMPT_EMBED_CACHE_ENTRIES,
    publish_bytes_atomically,
    read_prompt_file_header,
    read_prompt_file_tensor,
    require_free_entry_path,
    require_prompt_file_destination,
    require_prompt_file_source,
    text_encoder_fingerprint,
    validate_prompt_file_metadata,
)

log = logging.getLogger(__name__)

#: The between-runs state of the transformer «to_ram» action (parked in RAM) and
#: of «to_gpu» (kept on the device; `Session.settle` loads it there now).
_TO_RAM_STEPS = Steps(VramStep.PHASE, RamStep.KEEP)
_TO_GPU_STEPS = Steps(VramStep.MAX, RamStep.KEEP)


def _encoder_load_steps(requested: Steps) -> Steps:
    """The steps of a user-pressed encoder «load»: `requested` when its between-runs state keeps the encoder, else (PHASE, KEEP).

    A pair keeps it when the VRAM step is MAX (kept on the card) or the RAM
    step is KEEP or «Авто» (whose walk starts at KEEP). A load that dropped the
    encoder again at once would be a lie.
    """
    if requested.vram is VramStep.MAX or requested.ram in (RamStep.KEEP, RamStep.AUTO):
        return requested
    return _TO_RAM_STEPS


def _reservation_of(admitted: Forecast) -> tuple[int, int]:
    """`(device bytes, host growth)` a plan's admission reserves: its device peak, and its host peak above the baseline RSS.

    The baseline is read back from the idle state (`host_peak` = baseline +
    host weights there), the same figure the manager subtracts.
    """
    idle = admitted.phases[IDLE_PHASE]
    baseline = max(0, idle.host_peak - idle.host_weights)
    return int(admitted.device_peak), max(0, int(admitted.host_peak) - baseline)


def _private_manager(model_manager: SmallModelManager) -> LargeModelManager:
    """A large-model manager of this service alone, over the small pool's arbiter (tests, a standalone service).

    The backend's composition root passes its shared manager instead; this one
    keeps limits and floors in memory only.
    """
    log.info("FLUX.2 klein: no shared large-model manager given; using a private one over the small pool's arbiter")
    return LargeModelManager(model_manager.arbiter, limits_store=MemoryLimitsStore(), tables=memory._tables_for)


# =====================================================================
#  Service
# =====================================================================
class Flux2KleinInpaintService:
    """Lazy-loading FLUX.2 klein region editor for `inpaint.flux2_klein`.

    `model_manager` is the backend's small pool (its arbiter is shared with
    the large pool); `large_models` the backend's large-model manager (default:
    a private one, `_private_manager`). The FLUX.2 specs are registered on it
    here when it does not know them yet.
    """

    def __init__(self, model_manager: SmallModelManager, *, large_models: LargeModelManager | None = None) -> None:
        self._lock = threading.RLock()
        self._model_manager = model_manager
        self._large = large_models if large_models is not None else _private_manager(model_manager)
        large_model.register_specs(self._large)
        #: The device label of the last run (`None` before the first one).
        self._device: str | None = None
        #: Residency key of the last pipeline handle a session used.
        self._active_key: str | None = None
        self._last_error: str | None = None
        # Published copy of `last_error` / `active_key`; `health()` never takes
        # `_lock`, which a load and a generation hold (S-014).
        self._health = HealthSnapshot(ready=False, active_key=None, last_error=None)
        # The Qwen tokenizer and the scheduler, with the directory each was read
        # from: a few MB, needed on every run, read once per directory.
        self._tokenizer: Any = None
        self._tokenizer_dir: str | None = None
        self._scheduler: Any = None
        self._scheduler_dir: str | None = None
        self._last_paths: dict[str, str] = {}
        # Phase-1 results, keyed by everything that changes them
        # (`_prompt_cache_key`). Always on, LRU, host-resident.
        self._prompt_cache: OrderedDict[tuple[Any, ...], Any] = OrderedDict()
        self._guidance_ignored_note: tuple[Any, ...] | None = None
        self._phase_observer: SessionObserver | None = None

    # ---- manager handles ----
    def _device_name(self) -> str:
        """The run device this service resolves to now (`General.ai_device`, never an iGPU)."""
        return hardware._resolve_selected_backend_device("cuda")

    @staticmethod
    def _handle_key(spec_key: Callable[[], str], what: str) -> str | None:
        """The residency key `spec_key` computes, or `None` (logged) when the paths cannot form one: the handle `what` then reads as holding nothing."""
        try:
            return spec_key()
        except (OSError, ValueError) as exc:
            log.debug("FLUX.2 klein: ключ резидентности (%s) не вычислен, компонент считается невыгруженным: %s: %s", what, type(exc).__name__, exc)
            return None

    def _pipeline_key(self, normalized: Mapping[str, Any], device: str) -> str:
        options = large_model.PipelineOptions(normalized["transformer_path"], normalized["vae_path"], normalized["dtype"], 0, 1, False, False, False, 0, None)
        return large_model.pipeline_spec(normalized["dtype"]).key(options, large_model.WEIGHT_FORMAT, normalize_device(device) or str(device))

    def _encoder_key(self, paths: Mapping[str, Any], fp8: bool, device: str, *, on_cpu: bool) -> str:
        options = large_model.EncoderOptions(str(paths.get("text_encoder_path") or ""), bool(fp8), 0, on_cpu=on_cpu)
        return large_model.encoder_spec(fp8).key(options, large_model.WEIGHT_FORMAT, normalize_device(device) or str(device))

    def _parts(self, key: str | None) -> dict[str, tuple[int, int]]:
        """`{component: (device bytes, host bytes)}` the large pool holds for `key` (empty when nothing)."""
        if key is None:
            return {}
        holding = self._model_manager.arbiter.holdings(Pool.LARGE).get(key)
        if holding is None or not holding.resident:
            return {}
        return {part.name: (int(part.device_bytes), int(part.host_bytes)) for part in holding.parts}

    def _holds(self, key: str | None) -> bool:
        """Whether the handle `key` holds any byte anywhere."""
        return any(device or host for device, host in self._parts(key).values())

    def _observer(self, model: str) -> SessionObserver | None:
        observer = self._phase_observer
        if observer is None:
            return None

        def forward(step: str, event: str, info: Mapping[str, Any]) -> None:
            observer(step, event, {**info, "model": model})

        return forward

    def set_phase_observer(self, observer: SessionObserver | None) -> None:
        """Install (or clear, `None`) the per-step observer of every FLUX.2 session (the calibration tool).

        It receives the manager's `(step, event, info)` calls (`load:<component>`,
        transitions, phases, `finish`) of both handles with `info["model"]`
        added, and `("warmup", "begin"/"end", ...)` around the warm-up decode.
        It runs on the run's thread; it must not raise.
        """
        self._phase_observer = observer

    def _acquire(self, model: str, options: Any, shape: Any, steps: Steps, device: str, *, device_cap_bytes: int | None = None) -> Session:
        """Admit one session of `model` with `steps` (call under `self._lock`, pipeline before encoder: module notes).

        `device_cap_bytes`: the per-handle device ceiling (`None` = none; the
        encoder's under the combined VRAM-axis rule, `memory` module notes).
        """
        return self._large.acquire(model, options, shape, device=device, format=large_model.WEIGHT_FORMAT, steps=steps, observer=self._observer(model), device_cap_bytes=device_cap_bytes)

    def _combined_ceilings(self, alone: ProfilesReport, encoder_options: large_model.EncoderOptions, shape: RequestShape, device: str, *, until: VramStep) -> tuple[RamStep, dict[VramStep, int | None]] | None:
        """`(RAM step, memory.combined_ceilings)` of the pipeline report `alone`'s VRAM column, with the manager's live budgets; `None` when there is no VRAM axis to keep monotone.

        Each higher cell is the pipeline's grid row (`alone.vram_rows`) and the
        encoder `encoder_options` admitted beside its reservation under that
        cell's own ceiling, exactly as a run of that cell would admit it. Call
        it BEFORE this run's pipeline holds a reservation (the manager would
        claim it a second time). Read-only.
        """
        if not alone.vram_applicable or not isinstance(alone.selected, Planned):
            return None
        ram = alone.selected.plan.steps.ram
        encoder_model = large_model.encoder_model_name(encoder_options.fp8)

        def combined_at(cell: Steps, ceiling: int | None) -> int | None:
            row = alone.vram_rows.get(cell.vram)
            if not isinstance(row, Planned):
                return None
            claim = _reservation_of(row.forecast)
            beside = self._large.profiles(encoder_model, encoder_options, shape, device=device, steps=cell, claimed_device_bytes=claim[0], claimed_host_bytes=claim[1], device_cap_bytes=memory.encoder_device_cap(ceiling, row.full_forecast))
            if not isinstance(beside.selected, Planned):
                return None
            return memory.combined_device_peak(row.full_forecast, beside.selected.full_forecast)

        return ram, memory.combined_ceilings(ram, combined_at, until=until)

    @staticmethod
    def _encoder_cap(ceilings: tuple[RamStep, dict[VramStep, int | None]] | None, pipeline: Session) -> int | None:
        """The encoder's device ceiling beside the admitted `pipeline` session (`memory.encoder_device_cap`); `None` without ceilings.

        The ceilings were computed for the RAM step the pre-admission plan
        chose; a session admitted at another RAM step (the budgets moved in
        between) gets no ceiling, logged.
        """
        if ceilings is None:
            return None
        ram, table = ceilings
        effective = pipeline.steps_effective
        if effective.ram is not ram:
            log.info("FLUX.2 klein: пайплайн принят на шаге ОЗУ «%s», а потолок энкодера считался для «%s»; энкодер принимается без потолка.", effective.ram.value, ram.value)
            return None
        return memory.encoder_device_cap(table.get(effective.vram), pipeline.forecast)

    def large_model_name(self, params: dict[str, Any]) -> str:
        """The registered PIPELINE model `large_models.profiles` must plan for `params` (`flux2_klein` or `flux2_klein.float16`, by `dtype`).

        The adapter is registered under `flux2_klein` alone, while each compute
        dtype is its own spec (`large_model.pipeline_model_name`).

        # Raises
        `ValueError` for invalid params.
        """
        return large_model.pipeline_model_name(normalize_flux2_klein_params(params)["dtype"])

    def large_model_inputs(self, params: dict[str, Any]) -> tuple[Any, str]:
        """`(options, device)` of the PIPELINE model `large_model_name(params)` for `large_models.profiles`.

        `params` is the `inpaint.flux2_klein` params object.

        # Raises
        `ValueError` for invalid params.
        """
        normalized = normalize_flux2_klein_params(params)
        guidance = self._effective_guidance_scale(normalized) > 1.0
        return large_model.pipeline_options(normalized, guidance=guidance), self._device_name()

    def large_model_profiles(self, params: dict[str, Any], shape: RequestShape, *, format: str | None, steps: Steps) -> dict[str, Any]:
        """`large_models.profiles` for a FLUX.2 run: the PIPELINE's pickers with the text encoder's admission claimed.

        A run admits the pipeline first and the encoder on what the pipeline's
        reservation left. So: (1) the pipeline is planned alone; (2) the
        encoder is planned with that plan's reservation claimed, under the
        same requested `steps`, with the combined VRAM-axis ceiling a run would give it
        (`_combined_ceilings`, `memory` module notes); (3) the pipeline is planned again with the encoder's
        reservation claimed, and that report is the answer
        (`ProfilesReport.to_dict()`; `model` is the dtype's registration). The
        additive key `encoder` is the encoder's `selected` (`null` when the
        run will not encode: the prompt is cached or no encoder is
        installed). An encoder that does not fit makes `selected.fits`
        `false`, with the encoder's `reason` / `shortest_resource`. Read-only.

        # Raises
        `ValueError` for invalid params (or a format the model lacks).
        """
        normalized = normalize_flux2_klein_params(params)
        device = self._device_name()
        guidance = self._effective_guidance_scale(normalized) > 1.0
        model = large_model.pipeline_model_name(normalized["dtype"])
        options = large_model.pipeline_options(normalized, guidance=guidance)
        alone = self._large.profiles(model, options, shape, device=device, format=format, steps=steps)
        if not (text_encoder_available(normalized) and self._prompts_to_encode(normalized)):
            return {**alone.to_dict(), "encoder": None}
        pipeline_claim = _reservation_of(alone.selected.forecast) if isinstance(alone.selected, Planned) else (0, 0)
        encoder_options = large_model.encoder_options(normalized)
        cap = None
        if isinstance(alone.selected, Planned):
            selected_vram = alone.selected.plan.steps.vram
            ceilings = self._combined_ceilings(alone, encoder_options, shape, device, until=selected_vram)
            cap = memory.encoder_device_cap(ceilings[1][selected_vram], alone.selected.full_forecast) if ceilings is not None else None
        encoder = self._large.profiles(large_model.encoder_model_name(encoder_options.fp8), encoder_options, shape, device=device, steps=steps, claimed_device_bytes=pipeline_claim[0], claimed_host_bytes=pipeline_claim[1], device_cap_bytes=cap)
        encoder_selected = encoder.to_dict()["selected"]
        if not isinstance(encoder.selected, Planned):
            answer = alone.to_dict()
            answer["selected"] = {**answer["selected"], "fits": False, "vram": None, "ram": None, "needs_reclaim": None, "reason": encoder_selected["reason"], "shortest_resource": encoder_selected["shortest_resource"]}
            return {**answer, "encoder": encoder_selected}
        encoder_claim = _reservation_of(encoder.selected.forecast)
        combined: ProfilesReport = self._large.profiles(model, options, shape, device=device, format=format, steps=steps, claimed_device_bytes=encoder_claim[0], claimed_host_bytes=encoder_claim[1])
        return {**combined.to_dict(), "encoder": encoder_selected}

    # ---- status / health ----
    def status(self, params: dict[str, Any] | None = None) -> dict[str, Any]:
        """Component availability and residency for the given (or last) paths.

        `params` is read leniently: an absent or empty path is reported as "not
        configured". `prompt_cached` answers whether a READY embedding exists
        for `params` (`_prompt_cached`); `text_encoder_available` whether an
        encoder is present on this machine; `guidance_supported` whether the
        guidance slider can do anything on this checkpoint.

        **Per-component residency** (`residency`, `actions`) comes from the
        large pool's ledger parts and is reported only when the service lock was
        free: a generation holds it for its entire run, so the probe never waits;
        when it is busy the two keys are absent and `components_busy` is true.
        **This call never blocks.** Free memory and forecasts are
        `large_models.profiles`' answer, never this one's.
        """
        paths = _lenient_paths(params) or dict(self._last_paths)
        components = _component_states(paths)
        prompt_cached = self._prompt_cached(params)
        reason = _first_unavailable_reason(components, prompt_cached=prompt_cached)
        device = self._device_label()
        residency, busy = self._components_residency_nowait(paths, params, device)
        if residency is not None:
            for name, state in residency.items():
                components[name].update(state)
        return {
            "available": reason is None,
            "reason": reason,
            "components": components,
            "components_busy": busy,
            "loaded": self._holds(self._active_key),
            "device": device,
            "prompt_cached": prompt_cached,
            "text_encoder_available": text_encoder_available(paths),
            # `is not True` on purpose: an undeclared checkpoint (`None`) is run
            # with guidance live, so it must report the slider as supported.
            "guidance_supported": checkpoint_is_distilled(paths) is not True,
        }

    def _components_residency_nowait(self, paths: Mapping[str, Any], params: dict[str, Any] | None, device: str) -> tuple[dict[str, dict[str, Any]] | None, bool]:
        """`(per-component residency + actions, busy)` without ever waiting for the service lock."""
        if not self._lock.acquire(blocking=False):
            return None, True
        try:
            return self._components_locked(paths, params, device), False
        finally:
            self._lock.release()

    def _components_locked(self, paths: Mapping[str, Any], params: dict[str, Any] | None, device: str | None = None) -> dict[str, dict[str, Any]]:
        """Residency and available actions of the three weight-bearing components (caller holds `self._lock`).

        The pipeline's parts are the handle of `paths` with the request's dtype
        on `device` (default `_device_label`); the encoder's are the handle of
        its path in the request's precision.
        """
        device = device or self._device_label()
        dtype = str((params or {}).get("dtype") or "bfloat16")
        fp8 = bool((params or {}).get("text_encoder_fp8"))
        on_cpu = encoder_on_cpu(params)
        pipeline_parts: dict[str, tuple[int, int]] = {}
        if paths.get("transformer_path") and paths.get("vae_path") and dtype in ("bfloat16", "float16"):
            pipeline_parts = self._parts(self._handle_key(lambda: self._pipeline_key({**paths, "dtype": dtype}, device), "pipeline"))
        encoder_parts: dict[str, tuple[int, int]] = {}
        if paths.get("text_encoder_path"):
            encoder_parts = self._parts(self._handle_key(lambda: self._encoder_key(paths, fp8, device, on_cpu=on_cpu), "text_encoder"))
        pipeline_loaded = any(device_b or host_b for device_b, host_b in pipeline_parts.values())
        held = {
            "text_encoder": encoder_parts.get(large_model.TEXT_ENCODER, (0, 0)),
            "transformer": pipeline_parts.get(large_model.TRANSFORMER, (0, 0)),
            "vae": pipeline_parts.get(large_model.VAE, (0, 0)),
        }
        out: dict[str, dict[str, Any]] = {}
        for name in ACTIONABLE_COMPONENTS:
            residency = residency_of(*held[name])
            out[name] = {"residency": residency, "actions": _component_actions(name, residency, pipeline_loaded=pipeline_loaded)}
        return out

    def _device_label(self) -> str:
        """The device this service runs on: the last run's, else the planned one (`_device_name`). Takes no lock."""
        device = self._device
        if device is not None:
            return str(device)
        return self._device_name()

    def health(self) -> dict[str, Any]:
        """Snapshot for the periodic backend health event (never takes the service lock)."""
        fields = self._health.read()
        return {
            "ready": self._holds(fields["active_key"]),
            "model": "flux2_klein",
            "device": self._device_label(),
            "active_key": fields["active_key"],
            "last_error": fields["last_error"],
        }

    def _publish_health_locked(self) -> None:
        """Republish the state-derived health fields; caller holds `self._lock`."""
        self._health.publish(ready=self._holds(self._active_key), active_key=self._active_key, last_error=self._last_error)

    def unload(self) -> bool:
        """Drop every idle FLUX.2 handle (pipeline and encoder); `False` when nothing was held.

        The prompt cache is KEPT (megabytes of embeddings); the cached tokenizer
        and scheduler are dropped.
        """
        with self._lock:
            dropped = self._unload_models_locked(large_model.pipeline_model_names() + large_model.encoder_model_names())
            self._tokenizer = self._tokenizer_dir = None
            self._scheduler = self._scheduler_dir = None
            self._publish_health_locked()
            return dropped

    def _drop_failed_load_locked(self, key: str) -> None:
        """After a failed pipeline load (its session already finished): drop whatever the handle `key` left, so a failed load registers nothing.

        `Session.finish` only shrinks toward the between-runs state, so a
        transformer read before the VAE failed would otherwise stay held. Only
        that handle is dropped: a warm handle of another checkpoint or device
        under the same model name keeps its bytes.
        """
        if self._large.unload_key(key):
            hardware._clear_torch_cache()
            log.warning("FLUX.2 klein: загрузка пайплайна «%s» не удалась; уже прочитанные компоненты выгружены.", key)

    def _unload_models_locked(self, models: tuple[str, ...]) -> bool:
        """Drop every idle handle of `models`; returns whether any was dropped."""
        dropped = False
        for model in models:
            dropped = bool(self._large.unload(model)) or dropped
        if dropped:
            hardware._clear_torch_cache()
        return dropped

    # ---- main entry ----
    def inpaint_image_bytes(
        self,
        image_bytes: bytes,
        mask_bytes: bytes,
        *,
        reference_bytes: bytes | None = None,
        params: dict[str, Any] | None = None,
        progress_callback: ProgressCb | None = None,
    ) -> dict[str, Any]:
        """Regenerate the masked part of `image_bytes` and composite it back.

        `image_bytes` is the region PNG and `mask_bytes` an L8 mask of the same
        size, where non-zero means "may change"; `reference_bytes`, when given,
        is the user's marks over a copy of the region and must have EXACTLY the
        region's size (it is the pipeline's `image_reference`, never part of the
        composite). The returned `image_png` is byte-identical to the input
        outside the mask. Under `whole_region` the mask must be solid.

        **Order** (a memory contract): the pipeline handle is admitted and
        loaded first; the encoder handle second, planned on what the pipeline's
        reservation left; then the denoise and the decode as manager phases.

        The answer carries `applied` (`{"vae_tiling"}`: whether the decode really
        ran tiled, which the client persists), `oom_recovered`, the requested /
        effective steps (`memory.steps_fields`: `vram_step`, `ram_step`,
        `vram_step_effective`, `ram_step_effective`), `degraded` and
        `degrade_reason`.

        # Raises
        `ValueError` for bad params, region, mask or reference, an empty mask
        with `text_attention_in_mask`, or an encoder that does not match the
        transformer; `FileNotFoundError` when a component is missing;
        `LargeModelRefused` (a `RuntimeError`) when no plan fits;
        `LargeModelRunFailed` when the degrade ladder gives up; whatever the
        pipeline raises.
        """
        normalized = normalize_flux2_klein_params(params)
        region_rgb = decode_image_rgb(image_bytes)
        height, width = region_rgb.shape[:2]
        validate_region_size(width, height)
        mask_u8 = decode_mask(mask_bytes, expected_hw=(height, width))
        if normalized["whole_region"]:
            require_solid_mask(mask_u8)
        if normalized["text_attention_in_mask"]:
            # Before any load: an empty mask would otherwise be refused only
            # after the transformer had been read.
            attention.require_text_attention_target(mask_u8)
        reference_rgb = None
        if reference_bytes is not None:
            reference_rgb = decode_image_rgb(reference_bytes)
            if reference_rgb.shape[:2] != (height, width):
                raise ValueError(
                    f"Референс меток {reference_rgb.shape[1]}x{reference_rgb.shape[0]} не совпадает "
                    f"по размеру с областью {width}x{height}"
                )
        self._last_paths = {key: normalized[key] for key in _PATH_KEYS}
        report = _progress_reporter(progress_callback, "load", LOAD_PHASE_STEPS)
        steps = memory.requested_steps(normalized)

        with self._lock:
            try:
                # The FIRST frame of the run, before any check: the checks read
                # headers and configs, which on a cold page cache take long
                # enough that the user would see nothing happen.
                report(LOAD_STEP_PREPARE, "Подготовка запуска FLUX.2 klein")
                if self._prompts_to_encode(normalized):
                    require_text_encoder(normalized, what="генерация с этим промптом")
                # A 4B transformer beside a 9B encoder would otherwise die as a
                # bare matmul shape error inside the denoise.
                require_encoder_transformer_compatible(normalized)
                large_model.require_loadable(normalized, encoder=bool(self._prompts_to_encode(normalized)))
                result = self._run_locked(normalized, region_rgb, mask_u8, reference_rgb, steps, report, progress_callback)
                self._last_error = None
                self._publish_health_locked()
            except Exception as exc:
                self._last_error = str(exc)
                self._publish_health_locked()
                raise
        return result

    def _run_locked(
        self,
        normalized: dict[str, Any],
        region_rgb: np.ndarray,
        mask_u8: np.ndarray,
        reference_rgb: np.ndarray | None,
        steps: Steps,
        report: Callable[[int, str], None],
        progress_callback: ProgressCb | None,
    ) -> dict[str, Any]:
        """The run under `self._lock`: pipeline session, prompt, denoise, decode, composite."""
        height, width = region_rgb.shape[:2]
        device = self._device_name()
        guidance = self._effective_guidance_scale(normalized) > 1.0
        options = large_model.pipeline_options(normalized, guidance=guidance)
        shape = large_model.request_shape(width, height, with_reference=reference_rgb is not None, max_sequence_length=options.max_sequence_length, steps=options.steps)
        model = large_model.pipeline_model_name(normalized["dtype"])
        cold = not self._holds(self._handle_key(lambda: self._pipeline_key(normalized, device), "pipeline"))
        retries = 0
        # The encoder's combined ceiling is planned before the pipeline holds a
        # reservation (`_combined_ceilings`); only when this run will encode.
        ceilings = None
        if text_encoder_available(normalized) and self._prompts_to_encode(normalized):
            alone = self._large.profiles(model, options, shape, device=device, format=large_model.WEIGHT_FORMAT, steps=steps)
            ceilings = self._combined_ceilings(alone, large_model.encoder_options(normalized), shape, device, until=VramStep.MIN)
        session = self._acquire(model, options, shape, steps, device)
        loaded = False
        try:
            with session:
                self._device = device
                self._active_key = session.key
                self._load_pipeline_locked(session, normalized, report, progress_callback, cold=cold)
                loaded = True
                embeds, encoder_retries = self._prompt_embeds_locked(normalized, report, steps=steps, device=device, shape=shape, device_cap_bytes=self._encoder_cap(ceilings, session))
                retries += encoder_retries
                latent_mask = dilate_mask(mask_u8, normalized["mask_dilate_px"])
                latents_cpu, image_processor = session.run_phase(
                    large_model.PHASE_DENOISE,
                    lambda ctx: self._denoise(ctx, region_rgb, latent_mask, normalized, embeds, progress_callback, reference_rgb),
                )
                decoded, tiled = session.run_phase(
                    large_model.PHASE_DECODE,
                    lambda ctx: decode_region_latents(ctx.module(large_model.VAE), image_processor, latents_cpu, tiling=ctx.options.vae_tiling, lowered=ctx.options.vae_tile_lowered),
                )
                applied = {"vae_tiling": bool(normalized["vae_tiling"]) or bool(tiled)}
                retries += session.retries
                effective = session.steps_effective
                degraded = session.degraded
                degrade_reason = session.degrade_reason
        except BaseException:
            # Only a COLD load is undone: a warm handle held its bytes before
            # this run, and a failure afterwards keeps the between-runs state.
            if cold and not loaded:
                self._drop_failed_load_locked(session.key)
            raise

        import numpy as np

        generated = np.ascontiguousarray(np.asarray(decoded.convert("RGB"), dtype=np.uint8))
        if generated.shape[:2] != (height, width):
            raise RuntimeError(
                f"Пайплайн вернул область {generated.shape[1]}x{generated.shape[0]} вместо "
                f"{width}x{height}; композит невозможен"
            )
        if normalized["color_match"]:
            # Both matches sample outside the LATENT (dilated) mask: that is the
            # area the pipeline was allowed to repaint.
            generated = match_color_outside_mask(generated, region_rgb, latent_mask)
            if normalized["color_match_local_pct"] > 0:
                generated = match_color_local(
                    region_rgb,
                    generated,
                    latent_mask,
                    strength=normalized["color_match_local_pct"] / 100.0,
                    scale_px=int(normalized["color_match_scale_px"]),
                )
        composed = composite_over_region(region_rgb, generated, mask_u8, int(normalized["mask_feather_px"]))
        return {
            "image_png": encode_png_rgb(composed),
            "region_size": [int(width), int(height)],
            "device": self._device_label(),
            "applied": applied,
            "oom_recovered": retries > 0,
            # Deliberately NOT inside `applied` (persisted by the client): the
            # effective value under `whole_region` must not overwrite the user's choice.
            "text_attention_in_mask": bool(normalized["text_attention_in_mask"]),
            **memory.steps_fields(steps, effective),
            "degraded": bool(degraded),
            "degrade_reason": degrade_reason,
        }

    def _load_pipeline_locked(
        self,
        session: Session,
        normalized: dict[str, Any],
        report: Callable[[int, str], None],
        progress_callback: ProgressCb | None,
        *,
        cold: bool,
    ) -> None:
        """Bring the pipeline handle into its denoise state; on a `cold` handle report steps 1-6 and warm the VAE up.

        A cold handle held no byte when the run started: its components are
        read from disk here (the transformer with byte-level progress), placed
        into the denoise state, and the VAE runs one 64x64 decode. A warm
        handle reports nothing (a hot run's load phase is steps 0 and 9 only);
        the manager restores whatever it parked when the denoise phase starts.
        """
        roots = component_search_roots(normalized)
        tokenizer_dir = _require_component_dir(roots, _TOKENIZER_SUBDIR, _TOKENIZER_MARKERS, "токенизатор Qwen")
        scheduler_dir = _require_component_dir(roots, _SCHEDULER_SUBDIR, (_SCHEDULER_MARKER,), "планировщик (scheduler)")
        if not cold:
            self._ensure_tokenizer_locked(tokenizer_dir)
            self._ensure_scheduler_locked(scheduler_dir)
            return
        log.info("FLUX.2 klein: загрузка пайплайна (трансформер + VAE) на %s, шаги «%s» -> «%s».", session.device, session.steps_requested.label, session.steps_effective.label)
        transformer_label = "Загрузка трансформера"
        report(LOAD_STEP_TRANSFORMER, transformer_label)
        # The second, byte-level bar for a single-file checkpoint (the label
        # names the FILE): a diffusers folder reports steps only, as it always did.
        progress = None
        transformer_source = Path(normalized["transformer_path"])
        if transformer_source.is_file():
            byte_progress = _file_progress_reporter(progress_callback, "load", LOAD_PHASE_STEPS, LOAD_STEP_TRANSFORMER, transformer_label, transformer_source.name)
            progress = lambda done, total, _key: byte_progress(done, total)  # noqa: E731 - one-line adapter
        session.load(large_model.TRANSFORMER, progress=progress)
        report(LOAD_STEP_TOKENIZER, "Загрузка токенизатора")
        self._ensure_tokenizer_locked(tokenizer_dir)
        report(LOAD_STEP_VAE, "Загрузка VAE")
        session.load(large_model.VAE)
        report(LOAD_STEP_SCHEDULER, "Загрузка планировщика")
        self._ensure_scheduler_locked(scheduler_dir)
        report(LOAD_STEP_PLACEMENT, "Размещение модели")
        session.transition(None, large_model.PHASE_DENOISE)
        report(LOAD_STEP_WARMUP, "Прогрев модели")
        self._warmup_locked(session)

    def _warmup_locked(self, session: Session) -> bool:
        """One 64x64 decode on the session's VAE (it must be on the device in the current state); returns whether it ran."""
        observer = self._observer(session.model)
        if observer is not None:
            observer("warmup", "begin", {})
        started = time.perf_counter()
        ran = False
        try:
            ran = _warmup_vae_decode(session.module(large_model.VAE), session.device_for(ExecDevice.RUN_DEVICE))
        finally:
            if observer is not None:
                observer("warmup", "end", {"seconds": time.perf_counter() - started, "ran": ran})
        log.info("FLUX.2 klein: прогрев VAE выполнен за %.2f с на %s.", time.perf_counter() - started, session.device)
        return ran

    def _pipeline_for(self, ctx: Any, normalized: dict[str, Any]) -> Any:
        """A `Flux2KleinInpaintPipeline` over the modules the manager placed for this attempt (built per call: a retry may rebuild one)."""
        from diffusers import Flux2KleinInpaintPipeline

        distilled = checkpoint_is_distilled(normalized)
        if distilled is None:
            log.info(
                "FLUX.2 klein: чекпоинт не объявляет is_distilled (нет model_index.json рядом с "
                "трансформером и VAE или в нём нет этого поля) — пайплайн строится с "
                "is_distilled=False, guidance_scale продолжает действовать.",
            )
            distilled = False
        pipe = Flux2KleinInpaintPipeline(
            scheduler=self._scheduler,
            vae=ctx.module(large_model.VAE),
            is_distilled=distilled,
            # The prompt is embedded by the encoder handle; the denoise gets the
            # embeddings and never reaches for an encoder.
            text_encoder=None,
            tokenizer=self._tokenizer,
            transformer=ctx.module(large_model.TRANSFORMER),
        )
        pipe.set_progress_bar_config(disable=True)
        return pipe

    def _denoise(
        self,
        ctx: Any,
        region_rgb: np.ndarray,
        latent_mask: np.ndarray,
        normalized: dict[str, Any],
        embeds: dict[str, Any],
        progress_callback: ProgressCb | None,
        reference_rgb: np.ndarray | None,
    ) -> tuple[Any, Any]:
        """The denoise phase (restartable): returns `(latents on the host, the pipeline's image processor)`.

        The LATENT mask (`latent_mask`, dilated) goes to the pipeline; with
        `text_attention_in_mask` the call carries `attention_kwargs` confining
        the prompt to that mask's tokens, dropped from the pipeline again on
        every exit (diffusers keeps `_attention_kwargs`). Latents come back
        (`output_type="latent"`) and are kept on the host, so a decode retry
        never repeats the denoise.
        """
        import torch
        from PIL import Image

        height, width = region_rgb.shape[:2]
        pipe = self._pipeline_for(ctx, normalized)
        device = ctx.exec_device(large_model.TRANSFORMER)
        seed = normalized["seed"]
        generator = torch.Generator("cpu").manual_seed(int(seed) if seed is not None else int.from_bytes(os.urandom(4), "little"))
        requested_steps = int(normalized["steps"])
        total_steps = effective_steps(requested_steps, float(normalized["strength"]))
        cb = progress_callback

        def _on_step(_pipe: Any, step: int, _t: Any, kwargs: dict[str, Any]) -> dict[str, Any]:
            # A raising callback ABORTS the run (it propagates out of the
            # pipeline call, the session finishes and releases its reservation
            # and lease): the callback is the request's own channel, and what
            # it tolerates (a dead peer) is the caller's policy, not this loop's.
            if cb is not None:
                cb("generate", int(step) + 1, total_steps, "Генерация")
            return kwargs

        if cb is not None:
            cb("generate", 0, total_steps, "Генерация")
        call_kwargs: dict[str, Any] = {
            "image": Image.fromarray(region_rgb, "RGB"),
            "mask_image": Image.fromarray(latent_mask, "L"),
            "height": height,
            "width": width,
            "strength": float(normalized["strength"]),
            "num_inference_steps": requested_steps,
            # Not the raw request: a distilled checkpoint runs no classifier-free guidance.
            "guidance_scale": self._effective_guidance_scale(normalized),
            "max_sequence_length": int(normalized["max_sequence_length"]),
            "generator": generator,
            # Latents only: the VAE decode is its own manager phase.
            "output_type": "latent",
            "callback_on_step_end": _on_step,
            "callback_on_step_end_tensor_inputs": ["latents"],
        }
        if reference_rgb is not None:
            call_kwargs["image_reference"] = Image.fromarray(reference_rgb, "RGB")
        if normalized["text_attention_in_mask"]:
            call_kwargs["attention_kwargs"] = {"attention_mask": self._text_attention_mask_locked(latent_mask, normalized, embeds, reference_rgb, device)}
        _require_execution_device(pipe, device)
        negative = embeds["negative"]
        try:
            result = pipe(
                prompt=None,
                prompt_embeds=embeds["prompt"].to(device=device),
                negative_prompt_embeds=None if negative is None else negative.to(device=device),
                **call_kwargs,
            )
        finally:
            # diffusers keeps `self._attention_kwargs`; dropping it (and the local
            # reference, which the traceback would keep alive) frees the S x S
            # mask before the decode and while idle.
            pipe._attention_kwargs = None
            call_kwargs.pop("attention_kwargs", None)
        return result.images.detach().to("cpu"), pipe.image_processor

    # ---- the prompt (encoder handle) ----
    def _prompt_embeds_locked(
        self,
        normalized: dict[str, Any],
        report: Callable[[int, str], None],
        *,
        steps: Steps,
        device: str,
        shape: Any,
        device_cap_bytes: int | None = None,
    ) -> tuple[dict[str, Any], int]:
        """`({"prompt", "negative"} host embeddings, encoder retries)`; encodes what the cache misses.

        On a full cache hit no encoder is touched (a mask edit, a new seed or a
        repeated prompt goes straight to the denoise). `negative` is `None`
        unless classifier-free guidance is active. `device_cap_bytes`: the
        encoder's device ceiling beside the pipeline (`_encoder_cap`).
        """
        missing = self._prompts_to_encode(normalized)
        retries = 0
        if missing:
            retries = self._encode_prompts_locked(normalized, missing, report, steps=steps, device=device, shape=shape, device_cap_bytes=device_cap_bytes)
        else:
            report(LOAD_STEP_ENCODER_DONE, "Промпт взят из кэша")
        embeds: dict[str, Any] = {"prompt": self._cached_embeds(normalized, normalized["prompt"]), "negative": None}
        if self._effective_guidance_scale(normalized) > 1.0:
            embeds["negative"] = self._cached_embeds(normalized, "")
        return embeds, retries

    def _encode_prompts_locked(
        self,
        normalized: dict[str, Any],
        texts: list[str],
        report: Callable[[int, str], None],
        *,
        steps: Steps,
        device: str,
        shape: Any,
        release: bool = False,
        device_cap_bytes: int | None = None,
    ) -> int:
        """Encode `texts` into the cache through an encoder session; returns its retries.

        THE place a missing encoder becomes fatal (`require_text_encoder`).
        Where the encoder computes is the plan's: the GPU when `steps` and the
        device allow it, else the CPU from RAM; between runs it stays as the
        steps keep it, unless `release` drops it after the encode (the
        prompt-cache build's own release).
        `device_cap_bytes` is the encoder's device ceiling beside an admitted
        pipeline (`None` when no pipeline is involved: the cache build).
        """
        require_text_encoder(normalized, what="кодирование промпта")
        from diffusers import Flux2KleinInpaintPipeline

        roots = component_search_roots(normalized)
        tokenizer = self._ensure_tokenizer_locked(_require_component_dir(roots, _TOKENIZER_SUBDIR, _TOKENIZER_MARKERS, "токенизатор Qwen"))
        options = large_model.encoder_options(normalized)
        model = large_model.encoder_model_name(options.fp8)
        msl = int(normalized["max_sequence_length"])
        report(LOAD_STEP_TEXT_ENCODER, "Загрузка текстового энкодера")
        encoder_session = self._acquire(model, options, shape, steps, device, device_cap_bytes=device_cap_bytes)
        with encoder_session:
            encoder_session.load(large_model.TEXT_ENCODER)
            report(LOAD_STEP_ENCODE, "Кодирование промпта")

            def encode(ctx: Any) -> list[Any]:
                encoder = ctx.module(large_model.TEXT_ENCODER)
                where = ctx.exec_device(large_model.TEXT_ENCODER)
                return [_encode_prompt_phase(Flux2KleinInpaintPipeline, encoder, tokenizer, text, msl, where) for text in texts]

            results = encoder_session.run_phase(large_model.PHASE_ENCODE, encode)
            key = encoder_session.key
            retries = encoder_session.retries
        for text, embeds in zip(texts, results):
            self._store_embeds(self._prompt_cache_key(normalized, text), embeds)
        if release:
            self._unload_models_locked((model,))
        if self._holds(key):
            report(LOAD_STEP_ENCODER_DONE, "Текстовый энкодер оставлен в памяти")
        else:
            report(LOAD_STEP_ENCODER_DONE, "Текстовый энкодер выгружен")
        return retries

    # ---- prompt cache: build ----
    def prompt_cache_build(self, params: dict[str, Any] | None, *, progress_callback: ProgressCb | None = None) -> dict[str, Any]:
        """Encode the prompt into the cache WITHOUT generating anything ("Кэшировать").

        Only the encoder handle is used (no pipeline is admitted or loaded).
        The encoder is released afterwards — that is the point of the button —
        unless its handle already held bytes when the call arrived (that one
        belongs to the previous run's settings). Progress: step 0, then the
        prompt steps 7-9. Returns `{"prompt", "encoded", "prompt_cached", "device"}`.

        # Raises
        `ValueError` for invalid params or an empty prompt; `LargeModelRefused`
        when the encode does not fit; whatever the encoder load raises.
        """
        normalized = normalize_flux2_klein_params(params)
        text = normalized["prompt"]
        if not text:
            raise ValueError("Кэшировать нечего: промпт пуст. Введите текст промпта и повторите.")
        self._last_paths = {key: normalized[key] for key in _PATH_KEYS}
        report = _progress_reporter(progress_callback, "load", LOAD_PHASE_STEPS)
        with self._lock:
            try:
                if self._prompt_cache_key(normalized, text) in self._prompt_cache:
                    report(LOAD_STEP_ENCODER_DONE, "Промпт уже в кэше")
                    encoded = False
                else:
                    require_text_encoder(normalized, what="кэширование промпта")
                    large_model.require_loadable_encoder(normalized)
                    report(LOAD_STEP_PREPARE, "Подготовка кэширования промпта")
                    device = self._device_name()
                    resident = self._holds(self._handle_key(lambda: self._encoder_key(normalized, normalized["text_encoder_fp8"], device, on_cpu=normalized["encoder_on_cpu"]), "text_encoder"))
                    shape = large_model.request_shape(MIN_REGION_SIDE, MIN_REGION_SIDE, with_reference=False, max_sequence_length=int(normalized["max_sequence_length"]), steps=1)
                    self._encode_prompts_locked(normalized, [text], report, steps=memory.requested_steps(normalized), device=device, shape=shape, release=not resident)
                    encoded = True
                self._last_error = None
                self._publish_health_locked()
            except Exception as exc:
                self._last_error = str(exc)
                self._publish_health_locked()
                raise
        log.info("FLUX.2 klein: кэш промпта готов (%s), длина промпта %d символов.", "закодирован" if encoded else "уже был в памяти", len(text))
        return {"prompt": text, "encoded": encoded, "prompt_cached": True, "device": self._device_label()}

    # ---- per-component actions ----
    def component_action(
        self,
        params: dict[str, Any] | None,
        *,
        component: str,
        action: str,
        progress_callback: ProgressCb | None = None,
    ) -> dict[str, Any]:
        """Perform one per-component action and return the snapshot after it.

        `component` / `action` must be in that component's CURRENT `actions`
        (`_component_actions`), re-checked here under the lock. Each action is
        a manager operation: `load` a session that loads the handle and ends in
        its between-runs state; `unload` drops the idle handle; `to_ram` a
        session of (PHASE, KEEP) whose finish parks the pipeline in RAM;
        `to_gpu` a session of (MAX, KEEP) that `settle`s it onto the device;
        `warmup` one 64x64 decode.

        Returns `{"component", "action", "performed", "components",
        "components_busy": False, "device"}`.

        **Refusals are errors**: `ValueError` (unknown or unavailable action,
        invalid params), `RuntimeError` (busy: the lock is taken WITHOUT
        waiting; or the manager refused the plan), `FileNotFoundError`.
        """
        component = _require_component_name(component)
        action = _require_action_name(action)
        normalized = normalize_flux2_klein_params(params)
        self._last_paths = {key: normalized[key] for key in _PATH_KEYS}
        report = _progress_reporter(progress_callback, "load", LOAD_PHASE_STEPS)
        if not self._lock.acquire(blocking=False):
            raise RuntimeError(_component_busy_message(component, action))
        try:
            try:
                self._require_action_available_locked(component, action, normalized)
                performed = self._component_action_locked(normalized, component, action, report, progress_callback)
            except Exception as exc:
                self._last_error = str(exc)
                self._publish_health_locked()
                raise
            self._last_error = None
            self._publish_health_locked()
            components = self._components_locked(self._last_paths, normalized)
        finally:
            self._lock.release()
        log.info(
            "FLUX.2 klein: действие «%s» над компонентом «%s» выполнено (%s).",
            action,
            component,
            "состояние изменено" if performed else "изменений не потребовалось",
        )
        return {
            "component": component,
            "action": action,
            "performed": bool(performed),
            "components": components,
            "components_busy": False,
            "device": self._device_label(),
        }

    def _require_action_available_locked(self, component: str, action: str, normalized: dict[str, Any]) -> None:
        """Refuse an action that is not in `component`'s current `actions` list, naming what is possible.

        # Raises
        `ValueError` naming the component, the refused action and the available ones.
        """
        available = self._components_locked(self._last_paths, normalized)[component]["actions"]
        if action in available:
            return
        offered = ", ".join(f"«{name}»" for name in available) if available else "нет ни одного"
        raise ValueError(
            f"FLUX.2 klein: действие «{action}» сейчас недоступно для компонента "
            f"«{component}». Доступно: {offered}. Обновите состояние компонентов и "
            f"повторите — состояние могло измениться после последнего запроса."
        )

    def _component_action_locked(
        self,
        normalized: dict[str, Any],
        component: str,
        action: str,
        report: Callable[[int, str], None],
        progress_callback: ProgressCb | None,
    ) -> bool:
        """Dispatch one validated action through the manager; `True` when something changed. Caller holds `self._lock`."""
        if action == "unload":
            if component == "text_encoder":
                return self._unload_models_locked(large_model.encoder_model_names())
            return self._unload_models_locked(large_model.pipeline_model_names())
        device = self._device_name()
        shape = large_model.request_shape(MIN_REGION_SIDE, MIN_REGION_SIDE, with_reference=False, max_sequence_length=int(normalized["max_sequence_length"]), steps=1)
        if component == "text_encoder":
            # `load` is the only other encoder action.
            require_text_encoder(normalized, what="загрузка текстового энкодера")
            large_model.require_loadable_encoder(normalized)
            report(LOAD_STEP_PREPARE, "Подготовка загрузки текстового энкодера")
            steps = _encoder_load_steps(memory.requested_steps(normalized))
            options = large_model.encoder_options(normalized)
            report(LOAD_STEP_TEXT_ENCODER, "Загрузка текстового энкодера")
            with self._acquire(large_model.encoder_model_name(options.fp8), options, shape, steps, device) as session:
                session.load(large_model.TEXT_ENCODER)
            report(LOAD_STEP_ENCODER_DONE, "Текстовый энкодер загружен")
            return True
        require_encoder_transformer_compatible(normalized)
        large_model.require_loadable(normalized, encoder=False)
        options = large_model.pipeline_options(normalized, guidance=False)
        model = large_model.pipeline_model_name(normalized["dtype"])
        if action == "load":
            report(LOAD_STEP_PREPARE, "Подготовка загрузки FLUX.2 klein")
            # `None` until admitted: an admission refusal left nothing to drop.
            admitted_key: str | None = None
            try:
                with self._acquire(model, options, shape, memory.requested_steps(normalized), device) as session:
                    admitted_key = session.key
                    self._device = device
                    self._active_key = session.key
                    self._load_pipeline_locked(session, normalized, report, progress_callback, cold=True)
            except BaseException:
                if admitted_key is not None:
                    self._drop_failed_load_locked(admitted_key)
                raise
            return True
        if action in ("to_ram", "to_gpu"):
            # No phase runs. `to_ram`: (PHASE, KEEP) parks the pipeline in RAM
            # between runs, and `finish` (which only shrinks) parks it there.
            # `to_gpu`: (MAX, KEEP) keeps it on the device, and `finish` never
            # moves a unit up, so `settle` loads it there explicitly.
            label = "Выгрузка трансформера в оперативную память" if action == "to_ram" else "Возврат трансформера на устройство"
            report(LOAD_STEP_PLACEMENT, label)
            with self._acquire(model, options, shape, _TO_RAM_STEPS if action == "to_ram" else _TO_GPU_STEPS, device) as session:
                self._active_key = session.key
                if action == "to_gpu":
                    session.settle()
            return True
        # `warmup`: the VAE is its only component.
        with self._acquire(model, options, shape, memory.requested_steps(normalized), device) as session:
            session.load(large_model.VAE)
            report(LOAD_STEP_WARMUP, "Прогрев модели")
            return self._warmup_locked(session)

    def _ensure_scheduler_locked(self, scheduler_dir: Path) -> Any:
        """The `FlowMatchEulerDiscreteScheduler` of `scheduler_dir`, read once per directory (caller holds `self._lock`)."""
        key = str(scheduler_dir)
        if self._scheduler is not None and self._scheduler_dir == key:
            return self._scheduler
        from diffusers import FlowMatchEulerDiscreteScheduler

        self._scheduler = FlowMatchEulerDiscreteScheduler.from_pretrained(key)
        self._scheduler_dir = key
        return self._scheduler

    def _current_family(self, params: dict[str, Any] | None) -> tuple[str, str] | None:
        """`(encoder fingerprint, library family)` for the encoder named in `params`,
        or `None` when no encoder is installed on this machine.

        Reads the encoder path leniently — the library methods that only browse,
        export or import do not need a transformer or a VAE, and requiring them
        would refuse a perfectly meaningful request. `None` is a legitimate
        answer, not a failure: without an encoder there is no current family, and
        the callers say so instead of inventing one.

        # Raises
        `ValueError` when the path exists but cannot be fingerprinted (see
        `local_encoder_identity`).
        """
        return local_encoder_identity(_lenient_paths(params).get("text_encoder_path", ""))

    def _require_current_family(
        self, params: dict[str, Any] | None, *, what: str
    ) -> tuple[str, str]:
        """`_current_family`, but the encoder is mandatory. `what` names the operation.

        Used by the one library operation that cannot fall back on a file's own
        metadata: a SAVE has to record which encoder produced the embedding, and
        an unidentifiable encoder leaves nothing to record. The message therefore
        does NOT offer "load a ready cache" as an alternative the way
        `require_text_encoder` does — here it would not be one.

        # Raises
        `ValueError` when there is no local encoder, or it cannot be fingerprinted.
        """
        paths = _lenient_paths(params)
        encoder_path = paths.get("text_encoder_path", "")
        if not text_encoder_available(paths):
            where = f" (путь не найден: {encoder_path})" if encoder_path else ""
            raise ValueError(
                f"Текстовый энкодер недоступен{where}, а {what} без него невозможно: запись "
                "библиотеки обязана назвать энкодер, которым построена, а опознать его нечем. "
                "Укажите каталог текстового энкодера в настройках."
            )
        encoder_id = text_encoder_fingerprint(encoder_path)
        return encoder_id, encoder_family_name(encoder_path, encoder_id)

    def prompt_cache_list(self, params: dict[str, Any] | None) -> dict[str, Any]:
        """List the library entries of the CURRENT encoder's family — or of all of them.

        Only the encoder path is needed, so the listing works while the
        transformer and the VAE are still unset. A corrupt or foreign file in the
        directory is reported in `skipped` rather than failing the call — see
        `list_prompt_cache_entries`.

        **Without an encoder on this machine there is no current family**, and
        refusing the call there would hide the very entries that make an
        encoder-less machine usable. The listing then spans EVERY family in the
        library, `family` comes back empty (nothing is active) and `directory` is
        the library root. Each entry names its own `family` in both cases, which
        is how the client tells the active listing from a foreign one — no second
        naming scheme is introduced, the family in an entry is the one recorded
        in its file.

        Returns `{"family", "directory", "entries", "skipped",
        "text_encoder_available"}`.

        # Raises
        `ValueError` when an encoder path is present but cannot be fingerprinted.
        """
        identity = self._current_family(params)
        if identity is not None:
            family = identity[1]
            entries, skipped = list_prompt_cache_entries(family)
            return {
                "family": family,
                "directory": str(prompt_cache_family_dir(family)),
                "entries": entries,
                "skipped": skipped,
                "text_encoder_available": True,
            }

        entries: list[dict[str, Any]] = []
        skipped: list[dict[str, str]] = []
        for known in list_prompt_cache_families():
            family_entries, family_skipped = list_prompt_cache_entries(known)
            entries.extend(family_entries)
            # A skipped file is named by its family too, or two corrupt files of
            # the same name in different families would report as one.
            skipped.extend({**item, "family": known} for item in family_skipped)
        entries.sort(key=lambda entry: (str(entry["family"]), str(entry["name"])))
        return {
            "family": "",
            "directory": str(prompt_cache_root()),
            "entries": entries,
            "skipped": skipped,
            "text_encoder_available": False,
        }

    def prompt_cache_save(
        self, params: dict[str, Any] | None, name: str, *, overwrite: bool = False
    ) -> dict[str, Any]:
        """Store the cached embedding of `params["prompt"]` in the library under `name`.

        Saving NEVER encodes: a prompt that is not in the cache is a named error
        pointing at `prompt_cache.build`, because a save that silently spent two
        minutes reading a 16 GB encoder would be a different operation than the
        one the user asked for.

        The entry lands in `prompt_cache/<family>/<name>.msprompt`, where the
        family is the current encoder's (`encoder_family_name`). The file records
        the encoder it was built with, its family, the sequence length, the dtype
        and the fp8 flag, so `prompt_cache_load` can refuse an incompatible one;
        the write itself is atomic (`write_prompt_file`), and an existing name is
        refused unless `overwrite` was asked for (`require_free_entry_path`).

        **Saving needs the encoder itself**, unlike loading: the file records the
        fingerprint of the encoder that produced the embedding, and there is
        nothing to record when none is installed. A cache that arrived as a file
        already has that file, so nothing is lost by refusing here.

        Returns `{"family", "name", "path", "size_bytes", "prompt", "created_at"}`;
        `name` is the SANITIZED name actually written, which may differ from the
        one that was asked for.

        # Raises
        `ValueError` for invalid params, an empty prompt, an unusable name, a
        missing or unidentifiable encoder, an existing entry without `overwrite`,
        or a prompt that is not in the cache.
        """
        normalized = normalize_flux2_klein_params(params)
        text = normalized["prompt"]
        if not text:
            raise ValueError("Сохранять нечего: промпт пуст.")
        encoder_id, family = self._require_current_family(
            params, what="сохранение кэша промпта в библиотеку"
        )
        dest = require_free_entry_path(
            prompt_cache_entry_path(family, name), overwrite=overwrite
        )

        key = self._prompt_cache_key(normalized, text)
        with self._lock:
            embeds = self._prompt_cache.get(key)
            if embeds is not None:
                self._prompt_cache.move_to_end(key)
        if embeds is None:
            raise ValueError(
                "Промпт ещё не закодирован — сохранять нечего. Сначала постройте кэш "
                "(«Кэшировать»/inpaint.flux2_klein.prompt_cache.build) или выполните "
                "генерацию с этим промптом: сохранение ничего не кодирует само."
            )

        # Deliberately outside the lock: the tensor is already ours (a later
        # eviction cannot invalidate the reference), and serialization plus a
        # disk write must not block `status()`, `unload()` or an eviction
        # callback.
        metadata = prompt_file_metadata(normalized, text, encoder_id, family)
        size = prompt_cache.write_prompt_file(dest, embeds, metadata)
        log.info("FLUX.2 klein: кэш промпта сохранён в %s (%d байт).", dest, size)
        return {
            "family": family,
            "name": dest.stem,
            "path": str(dest),
            "size_bytes": int(size),
            "prompt": text,
            "created_at": metadata["created_at"],
        }

    def prompt_cache_load(self, params: dict[str, Any] | None, name: str) -> dict[str, Any]:
        """Load a library entry of the current family into the cache.

        The entry's own prompt text is what the embedding belongs to, so it is
        returned for the client to show; everything else about the file
        (encoder, sequence length, dtype, fp8) must match the current settings or
        the load is refused by name — see `validate_prompt_file_metadata` for why
        a mismatch cannot be tolerated. **An entry of another family is refused
        here even though `prompt_cache_import` accepts one**: importing files a
        cache away for later, while loading puts embeddings into the very cache a
        generation reads from.

        The header is checked BEFORE torch is imported and before a byte of
        tensor is allocated.

        **Without an encoder on this machine** the entry is looked up across
        every family (`find_prompt_cache_entry`) — there is no current one to
        restrict the search to — and the fingerprint is the only check that is
        skipped: the format marker, the version, `max_sequence_length`, the
        dtype, the tensor's own dtype token and the fp8 flag are verified as
        always, because none of them needs an encoder. The answer says which of
        the two happened in `encoder_verified`, so a client never has to guess
        whether the identity was checked or taken on trust.

        Returns `{"family", "name", "path", "prompt", "prompt_cached",
        "max_sequence_length", "dtype", "created_at", "encoder_verified"}`.

        # Raises
        `ValueError` for invalid params, an unusable name, a missing entry, a
        name that exists in several families while no encoder selects one, a
        corrupt file, or one built for other settings.
        """
        normalized = normalize_flux2_klein_params(params)
        identity = self._current_family(params)
        encoder_id = identity[0] if identity is not None else None
        source = find_prompt_cache_entry(name, identity[1] if identity is not None else None)
        metadata, tensor = read_prompt_file_header(source)
        validate_prompt_file_metadata(metadata, tensor, normalized, encoder_id, source)

        text = metadata.get("prompt", "")
        embeds = read_prompt_file_tensor(source)
        with self._lock:
            self._store_embeds(self._prompt_cache_key(normalized, text), embeds)
        log.info(
            "FLUX.2 klein: кэш промпта загружен из %s (создан %s, длина промпта %d символов, "
            "отпечаток энкодера %s).",
            source,
            metadata.get("created_at", "?"),
            len(text),
            "сверен" if encoder_id is not None else "не сверялся — локального энкодера нет",
        )
        return {
            # The family the entry actually came from: with no encoder selected
            # that is the file's own, and the caller must not be told it belongs
            # to a current family that does not exist.
            "family": source.parent.name,
            "name": source.stem,
            "path": str(source),
            "prompt": text,
            "prompt_cached": True,
            "max_sequence_length": int(normalized["max_sequence_length"]),
            # The EMBEDDING's dtype, which is the encoder's and not the request's
            # (`text_encoder_dtype_name`) — it is what the entry was validated
            # against just above.
            "dtype": text_encoder_dtype_name(),
            "created_at": metadata.get("created_at", ""),
            "encoder_verified": encoder_id is not None,
        }

    def prompt_cache_export(
        self, params: dict[str, Any] | None, name: str, path: str
    ) -> dict[str, Any]:
        """Copy a library entry of the current family to an arbitrary `.msprompt` path.

        A byte copy, published atomically: the file already carries everything
        that identifies it, so re-serializing it would only risk changing it.
        Like `prompt_cache_list` this needs the encoder path alone.

        Unlike a LIBRARY entry, an existing destination is overwritten without a
        flag: the path comes from the client's own save dialog, which is where
        the "replace this file?" question belongs, and asking it twice would make
        the second answer meaningless.

        With no encoder installed the entry is resolved across families, exactly
        as `prompt_cache_load` does: copying a file out never feeds a generation,
        so the lookup is the only thing the missing encoder changes.

        Returns `{"family", "name", "path", "size_bytes"}`; `family` is the one
        the entry was taken FROM.

        # Raises
        `ValueError` when the name is unusable, the entry is missing or ambiguous,
        or the destination path is rejected (`require_prompt_file_destination`).
        """
        identity = self._current_family(params)
        source = find_prompt_cache_entry(name, identity[1] if identity is not None else None)
        family = source.parent.name
        dest = require_prompt_file_destination(path)
        try:
            payload = source.read_bytes()
        except OSError as exc:
            raise ValueError(f"Не удалось прочитать кэш промпта {source}: {exc}") from exc
        size = publish_bytes_atomically(dest, payload)
        log.info("FLUX.2 klein: кэш промпта «%s» выгружен в %s (%d байт).", source.stem, dest, size)
        return {"family": family, "name": source.stem, "path": str(dest), "size_bytes": int(size)}

    def prompt_cache_import(
        self,
        params: dict[str, Any] | None,
        path: str,
        *,
        name: str | None = None,
        overwrite: bool = False,
    ) -> dict[str, Any]:
        """Copy an outside `.msprompt` file into the library.

        **The entry lands in the family recorded in the FILE, not in the family
        currently selected.** An embedding belongs to the encoder that produced
        it; filing it under the encoder that happens to be selected right now
        would hide it from the encoder it actually works with, and would put a
        foreign entry into a listing `prompt_cache_load` reads from. When the two
        differ the import still succeeds — the file is a valid cache for
        SOMETHING — and the answer says so (`family_matches: false`) so the
        client can warn instead of pretending the entry is usable now.

        The file is verified as ours (`read_prompt_file_header`) before anything
        is written; the copy is a byte copy, published atomically, and an
        existing name is refused unless `overwrite` was asked for.

        `name` defaults to the source file's stem. Both it and the family are
        sanitized, so a hand-made file cannot place itself outside the library.

        Returns `{"family", "name", "path", "size_bytes", "prompt", "created_at",
        "current_family", "family_matches"}`.

        # Raises
        `ValueError` for a rejected source path, a file that is not a
        `.msprompt`, a file whose metadata names no family, an unusable name, or
        an existing entry without `overwrite`.
        """
        source = require_prompt_file_source(path)
        metadata, _tensor = read_prompt_file_header(source)
        file_family = metadata.get("text_encoder_family", "")
        if not file_family:
            raise ValueError(
                f"Файл кэша промпта {source} не указывает семейство энкодера "
                "(text_encoder_family): неизвестно, куда его положить."
            )
        dest = require_free_entry_path(
            prompt_cache_entry_path(file_family, name if name else source.stem),
            overwrite=overwrite,
        )
        stored_family = dest.parent.name

        # The current family is informational here: an import must work even when
        # no encoder is configured yet, which is exactly the case where a user is
        # setting a machine up from someone else's files. A path that names a
        # broken encoder is caught and treated the same way, for the same reason.
        try:
            identity = self._current_family(params)
        except ValueError:
            identity = None
        current_family = identity[1] if identity is not None else ""

        try:
            payload = source.read_bytes()
        except OSError as exc:
            raise ValueError(f"Не удалось прочитать файл кэша промпта {source}: {exc}") from exc
        size = publish_bytes_atomically(dest, payload)
        family_matches = bool(current_family) and current_family == stored_family
        if not family_matches:
            log.info(
                "FLUX.2 klein: импортированный кэш промпта положен в семейство «%s», а сейчас "
                "выбрано «%s» — с текущим энкодером он не подойдёт.",
                stored_family,
                current_family or "—",
            )
        return {
            "family": stored_family,
            "name": dest.stem,
            "path": str(dest),
            "size_bytes": int(size),
            "prompt": metadata.get("prompt", ""),
            "created_at": metadata.get("created_at", ""),
            "current_family": current_family,
            "family_matches": family_matches,
        }

    def _prompt_cached(self, params: dict[str, Any] | None) -> bool:
        """Whether a READY embedding exists for the combination named by `params`.

        Reported by `status` and read leniently, like the rest of it: an empty
        prompt, a missing path or an unknown enum answers `False` instead of
        raising, because the UI polls this while the user is still typing.

        The answer comes from `_prompt_cache_key` — the same key a generation
        looks up — so it can never claim a hit the run would miss. `whole_region`
        is cleared before normalizing: it cannot change the key, and clearing it
        keeps `_whole_region_overrides`'s log line out of a polling path.

        Takes NO lock: a `dict` membership test with tuple-of-primitives keys is
        one atomic C-level operation under the GIL, and `_store_embeds` is the
        only writer. Waiting for the lock here made a status poll block for the
        whole of a running generation.
        """
        if not isinstance(params, dict):
            return False
        if not str(params.get("prompt", "") or "").strip():
            return False
        probe = dict(params)
        probe["whole_region"] = False
        try:
            normalized = normalize_flux2_klein_params(probe)
        except ValueError:
            return False
        return self._prompt_cache_key(normalized, normalized["prompt"]) in self._prompt_cache

    # ---- phase 2: the prompt ----
    def _prompt_cache_key(self, normalized: dict[str, Any], text: str) -> tuple[Any, ...]:
        """Everything that changes an embedding: encoder, text, length, dtype, fp8.

        Where the encoder ran is deliberately absent — the same encoder computes the same
        embedding whether it ran on the host or the accelerator, and including it
        would evict the cache on a step change for nothing.

        The dtype is the ENCODER's (`text_encoder_dtype_name`) because that is
        what the embedding is made of; the request's `dtype` describes the
        transformer and the VAE and cannot change a single value in this tensor.
        """
        return (
            normalized["text_encoder_path"],
            text,
            int(normalized["max_sequence_length"]),
            text_encoder_dtype_name(),
            bool(normalized["text_encoder_fp8"]),
        )

    def _effective_guidance_scale(self, normalized: dict[str, Any]) -> float:
        """The `guidance_scale` this run will really use — 1.0 on a distilled checkpoint.

        Every decision that depends on the scale must ask THIS, never
        `normalized["guidance_scale"]`, because the two differ exactly when the
        checkpoint declares `is_distilled: true`: diffusers then computes
        `do_classifier_free_guidance = guidance_scale > 1 and not is_distilled`
        as `False`, so a higher scale buys nothing while the code around it would
        still pay for it — a whole extra pass over the 16 GB text encoder for a
        negative prompt the denoise never reads, and a `denoise` step that thinks
        it runs two branches.

        The checkpoint is consulted only when the user actually raised the slider,
        so the default (1.0) never touches the filesystem; when it did, the read
        is one small JSON beside weights measured in gigabytes. The override is
        announced once per `(checkpoint, requested scale)`, because a run asks
        three times.
        """
        requested = float(normalized["guidance_scale"])
        if requested <= 1.0:
            return requested
        if checkpoint_is_distilled(normalized) is not True:
            return requested
        note = (normalized["transformer_path"], normalized["vae_path"], requested)
        if self._guidance_ignored_note != note:
            self._guidance_ignored_note = note
            log.info(
                "FLUX.2 klein: model_index.json чекпоинта объявляет is_distilled: true — "
                "guidance из него дистиллирован, поэтому запрошенный guidance_scale %.3g "
                "игнорируется (используется 1.0), а негативный промпт не кодируется.",
                requested,
            )
        return 1.0

    def _prompts_to_encode(self, normalized: dict[str, Any]) -> list[str]:
        """Prompt texts the phase still has to run; empty when the cache covers the run."""
        wanted = [normalized["prompt"]]
        if self._effective_guidance_scale(normalized) > 1.0:
            # Classifier-free guidance needs the empty prompt too; encoding it now
            # is what keeps the pipeline from reaching for an encoder that is gone.
            # A distilled checkpoint never gets here: `_effective_guidance_scale`
            # has already pinned the scale to 1.0, so the encoder is not read for
            # an embedding the denoise would then ignore.
            wanted.append("")
        return [
            text
            for text in wanted
            if self._prompt_cache_key(normalized, text) not in self._prompt_cache
        ]

    def _cached_embeds(self, normalized: dict[str, Any], text: str) -> Any:
        """Fetch one cached embedding and mark it most-recently used."""
        key = self._prompt_cache_key(normalized, text)
        value = self._prompt_cache[key]
        self._prompt_cache.move_to_end(key)
        return value

    def _store_embeds(self, key: tuple[Any, ...], value: Any) -> None:
        """Insert into the LRU prompt cache, evicting the oldest entry when full."""
        self._prompt_cache[key] = value
        self._prompt_cache.move_to_end(key)
        while len(self._prompt_cache) > PROMPT_EMBED_CACHE_ENTRIES:
            self._prompt_cache.popitem(last=False)

    def _ensure_tokenizer_locked(self, tokenizer_dir: Path) -> Any:
        """Return the Qwen tokenizer for `tokenizer_dir`, reading it once per directory.

        Caller must hold `self._lock`. THE one place `Qwen2TokenizerFast` is
        constructed: the pipeline build takes its `tokenizer` component from here
        and so does every prompt encode, so the vocabulary is read from disk once
        instead of once per encode. On a model that lives on a spinning disk that
        repeated read is seconds of I/O added to an otherwise hot pipeline.

        The cache key is the discovered directory and nothing else, exactly like
        the pipeline cache's own path keys: swapping the files under a path
        without changing the path is not a case this service tracks. The object
        is shared with the pipeline component, which is safe because encoding is
        read-only and the pipeline's copy is never even called (the denoise is
        handed finished `prompt_embeds`).

        # Raises
        Whatever the loader raises when `tokenizer_dir` does not hold a usable
        tokenizer.
        """
        key = str(tokenizer_dir)
        if self._tokenizer is not None and self._tokenizer_dir == key:
            return self._tokenizer

        from transformers import Qwen2TokenizerFast

        tokenizer = Qwen2TokenizerFast.from_pretrained(key)
        self._tokenizer = tokenizer
        self._tokenizer_dir = key
        return tokenizer

    def _text_attention_mask_locked(
        self,
        latent_mask: np.ndarray,
        normalized: dict[str, Any],
        embeds: dict[str, Any],
        reference_rgb: np.ndarray | None,
        device: Any,
    ) -> Any:
        """Build the `text_attention_in_mask` attention mask for this run.

        `latent_mask` is the DILATED mask the pipeline receives as `mask_image`,
        so the prompt reaches every token the latent blend may change. The text
        length is read from the real `prompt_embeds`, never from the request:
        the mask must match the sequence the transformer will actually see.
        A marks reference always has the region's size here (`inpaint_image_bytes`
        refuses any other), so its tokens follow the same inside/outside grid
        (`attention.plan_text_attention`).

        The mask is built on `device` — where the transformer computes and
        `prompt_embeds` go — in the request's `dtype` (the transformer's: SDPA
        requires an additive mask to match the query dtype).

        Caller must hold `self._lock`.

        # Raises
        `ValueError` when a negative embedding (classifier-free guidance) has a
        different text length than the prompt — the same mask goes to both
        passes, so it could only fit one of them — or when the mask has no token
        inside it (`attention.plan_text_attention`).
        """
        text_tokens = int(embeds["prompt"].shape[1])
        negative = embeds["negative"]
        if negative is not None and int(negative.shape[1]) != text_tokens:
            raise ValueError(
                "Внимание текста только внутри маски: длина негативного промпта "
                f"({int(negative.shape[1])} токенов) не совпадает с длиной промпта "
                f"({text_tokens}); одна маска внимания не подходит к обоим проходам."
            )
        layout = attention.plan_text_attention(
            latent_mask,
            text_tokens=text_tokens,
            reference_hw=None if reference_rgb is None else reference_rgb.shape[:2],
        )
        mask = attention.build_text_attention_mask(
            layout, dtype_name=normalized["dtype"], device=device
        )
        log.info(
            "FLUX.2 klein: внимание текста только внутри маски: text_attention_in_mask=True "
            "L=%d N=%d R=%d S=%d inside_tokens=%d reference_follows_mask=%s mask_bytes=%d "
            "dtype=%s device=%s",
            layout.text_tokens,
            layout.image_tokens,
            layout.reference_tokens,
            layout.sequence_length,
            layout.inside_tokens,
            layout.reference_inside is not None,
            layout.mask_bytes(normalized["dtype"]),
            normalized["dtype"],
            device,
        )
        return mask
